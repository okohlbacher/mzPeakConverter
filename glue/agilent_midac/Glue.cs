// AgilentMidacGlue.Exports — reflection-only bridge to the Agilent MIDAC (ion-mobility) SDK, exposed
// to Rust as [UnmanagedCallersOnly] C-ABI functions. Sibling of glue/agilent (which wraps MHDAC).
//
// WINDOWS-RUNTIME-ONLY and UNTESTED SCAFFOLD. Compiles on any platform with a .NET 8 SDK because it
// never references MIDAC types at compile time — every MIDAC call goes through System.Reflection. At
// run time it requires the Agilent MIDAC DLLs (from a ProteoWizard install's vendor_api/Agilent dir)
// and an x64 Windows process. The MIDAC member names below are best-effort (per the ProteoWizard
// reference); a runtime member-not-found is reported via LastError rather than crashing.
//
// ABI mirror of src/agilent_midac.rs (FrameOut = 3×IntPtr + long + double + 4×int = 56 bytes):
//   long Open(ushort* pathUtf16, ushort* midacDirUtf16)  -> handle >=0, or negative error
//   long FrameCount(long handle)                         -> count >=0, or negative error
//   int  GetFrame(long handle, long index, FrameOut* out)-> 0 ok, non-zero error
//   void FreeFrame(FrameOut* out)                        -> frees the three HGlobal buffers
//   void Close(long handle)                              -> idempotent
//   int  LastError(ushort* bufUtf16, int cap)            -> FULL UTF-16 length; fills up to cap
//   int  HasImsData(ushort* pathUtf16, ushort* midacDirUtf16) -> 1 has IM, 0 no, <0 error

using System.Collections.Concurrent;
using System.Reflection;
using System.Runtime.InteropServices;

namespace AgilentMidacGlue;

/// <summary>Layout-compatible mirror of the Rust <c>FrameOut</c>. Three parallel double[] arrays.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct FrameOut
{
    public IntPtr MzPtr;        // double* (n_points)
    public IntPtr IntensityPtr; // double* (n_points), widened to double on the wire
    public IntPtr MobilityPtr;  // double* (n_points), inverse reduced ion mobility per point
    public long NPoints;
    public double RtMinutes;
    public int MsLevel;         // 1 = MS1, 2 = MS2
    public int Polarity;        // 1 = +, -1 = -, 0 = unknown
    public int FrameId;
    public int Pad;             // explicit trailing pad → 56 bytes both sides
}

public static class Exports
{
    private static readonly ConcurrentDictionary<long, ReaderState> Readers = new();
    private static long _nextHandle = 0;
    [ThreadStatic] private static string? _lastError;

    private const int ExpectedFrameOutSize = 56;

    static Exports()
    {
        int actual = Marshal.SizeOf<FrameOut>();
        if (actual != ExpectedFrameOutSize)
        {
            throw new InvalidOperationException(
                $"ABI mismatch: FrameOut is {actual} bytes, expected {ExpectedFrameOutSize}. " +
                "The C# struct and the Rust #[repr(C)] FrameOut have drifted — keep them in sync.");
        }
    }

    private static Assembly? _midac;
    private static readonly object _loadLock = new();

    private sealed class ReaderState
    {
        public required object Reader;   // MIDAC IMidacImsReader instance
        public int Count;
    }

    // ------------------------------------------------------------------
    // MIDAC loading (reflection): resolve + load the assembly from the midac dir on first use, with
    // an AssemblyResolve handler so its dependencies load from the same directory.
    // ------------------------------------------------------------------
    private static Assembly LoadMidac(string midacDir)
    {
        lock (_loadLock)
        {
            if (_midac != null) return _midac;
            if (!Directory.Exists(midacDir))
                throw new DirectoryNotFoundException($"MIDAC dir not found: {midacDir}");

            AppDomain.CurrentDomain.AssemblyResolve += (_, args) =>
            {
                string name = new AssemblyName(args.Name).Name + ".dll";
                string candidate = Path.Combine(midacDir, name);
                return File.Exists(candidate) ? Assembly.LoadFrom(candidate) : null;
            };

            // The MIDAC entry assembly. Name per Agilent's SDK; adjust if the install differs.
            string dll = Path.Combine(midacDir, "MIDAC.dll");
            if (!File.Exists(dll))
            {
                // Some installs name it with the full namespace.
                string alt = Path.Combine(midacDir, "Agilent.MassSpectrometry.MIDAC.dll");
                dll = File.Exists(alt) ? alt : dll;
            }
            _midac = Assembly.LoadFrom(dll);
            return _midac;
        }
    }

    private static Type MidacType(string name) =>
        _midac!.GetType("Agilent.MassSpectrometry.MIDAC." + name, throwOnError: true)!;

    // ------------------------------------------------------------------
    // Exports
    // ------------------------------------------------------------------

    [UnmanagedCallersOnly(EntryPoint = "HasImsData")]
    public static unsafe int HasImsData(ushort* pathUtf16, ushort* midacDirUtf16)
    {
        try
        {
            string path = Marshal.PtrToStringUni((IntPtr)pathUtf16) ?? "";
            string midacDir = Marshal.PtrToStringUni((IntPtr)midacDirUtf16) ?? "";
            LoadMidac(midacDir);
            // MidacFileAccess.FileHasImsData(string) -> bool  (per ProteoWizard MassHunterData.cpp)
            Type fileAccess = MidacType("MidacFileAccess");
            MethodInfo? m = fileAccess.GetMethod("FileHasImsData", BindingFlags.Public | BindingFlags.Static);
            if (m == null) { _lastError = "MidacFileAccess.FileHasImsData not found"; return -1; }
            bool has = (bool)m.Invoke(null, new object[] { path })!;
            return has ? 1 : 0;
        }
        catch (Exception e) { _lastError = e.ToString(); return -2; }
    }

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static unsafe long Open(ushort* pathUtf16, ushort* midacDirUtf16)
    {
        try
        {
            string path = Marshal.PtrToStringUni((IntPtr)pathUtf16) ?? "";
            string midacDir = Marshal.PtrToStringUni((IntPtr)midacDirUtf16) ?? "";
            LoadMidac(midacDir);

            // Open an ImsDataReader on the .d. The exact factory/ctor varies by SDK build; try the
            // common shapes (MidacFileAccess.ImsDataReader(path) -> IMidacImsReader, else a ctor).
            Type fileAccess = MidacType("MidacFileAccess");
            object? reader = null;
            MethodInfo? factory = fileAccess.GetMethod("ImsDataReader", BindingFlags.Public | BindingFlags.Static);
            if (factory != null)
            {
                reader = factory.Invoke(null, new object[] { path });
            }
            else
            {
                Type readerType = MidacType("MidacImsReader");
                reader = Activator.CreateInstance(readerType);
                MethodInfo? open = readerType.GetMethod("OpenDataFile") ?? readerType.GetMethod("Open");
                open?.Invoke(reader, new object[] { path });
            }
            if (reader == null) { _lastError = "could not construct a MIDAC reader"; return -2; }

            int count = FrameCountOf(reader);
            long handle = Interlocked.Increment(ref _nextHandle);
            Readers[handle] = new ReaderState { Reader = reader, Count = count };
            return handle;
        }
        catch (Exception e) { _lastError = e.ToString(); return -1; }
    }

    private static int FrameCountOf(object reader)
    {
        // IMidacImsReader exposes the frame count via a property (NumFrames / FrameCount / FramesMs).
        Type t = reader.GetType();
        foreach (string name in new[] { "NumFrames", "FrameCount", "FramesMs", "TotalFrames" })
        {
            PropertyInfo? p = t.GetProperty(name);
            if (p != null) return Convert.ToInt32(p.GetValue(reader));
            MethodInfo? m = t.GetMethod(name);
            if (m != null && m.GetParameters().Length == 0) return Convert.ToInt32(m.Invoke(reader, null));
        }
        throw new MissingMemberException("MIDAC frame count property not found (NumFrames/FrameCount/...)");
    }

    [UnmanagedCallersOnly(EntryPoint = "FrameCount")]
    public static long FrameCount(long handle)
    {
        try
        {
            return Readers.TryGetValue(handle, out var st) ? st.Count : -1;
        }
        catch (Exception e) { _lastError = e.ToString(); return -2; }
    }

    [UnmanagedCallersOnly(EntryPoint = "GetFrame")]
    public static unsafe int GetFrame(long handle, long index, FrameOut* outPtr)
    {
        if (outPtr == null) return -1;
        *outPtr = default;
        try
        {
            if (!Readers.TryGetValue(handle, out var st)) { _lastError = "bad handle"; return -2; }
            object reader = st.Reader;
            Type t = reader.GetType();

            // Read one IM frame's profile/peak data. The MIDAC member shapes vary; we look up a
            // frame accessor returning an object exposing MzValues/YValues/DriftValues (or similar).
            object frame = InvokeFrame(reader, t, (int)index);
            double[] mz = AsDoubleArray(GetMember(frame, "MzArray", "MzValues", "XArray", "Mz"));
            double[] inten = AsDoubleArray(GetMember(frame, "YArray", "Intensities", "Abundance", "YValues"));
            double[] mob = AsDoubleArray(GetMember(frame, "DriftTimeArray", "DriftValues", "Mobilities", "OneOverK0"));

            int n = mz.Length;
            if (inten.Length != n || (mob.Length != 0 && mob.Length != n))
            {
                _lastError = $"frame {index} array length mismatch (mz {n}, int {inten.Length}, mob {mob.Length})";
                return -3;
            }
            if (mob.Length == 0) mob = new double[n]; // mobility optional → zero-fill so the column exists

            outPtr->MzPtr = AllocDoubles(mz);
            outPtr->IntensityPtr = AllocDoubles(inten);
            outPtr->MobilityPtr = AllocDoubles(mob);
            outPtr->NPoints = n;
            outPtr->RtMinutes = TryDouble(GetMember(frame, "RetentionTime", "AcqTimeRange", "TimeInMinutes"), 0.0);
            outPtr->MsLevel = (int)TryDouble(GetMember(frame, "MsLevel", "MSLevel"), 1.0);
            outPtr->Polarity = PolarityOf(frame);
            outPtr->FrameId = (int)index;
            return 0;
        }
        catch (Exception e)
        {
            _lastError = e.ToString();
            FreeFrameInternal(outPtr);
            return -9;
        }
    }

    private static object InvokeFrame(object reader, Type t, int index)
    {
        // Try the common frame accessors: Frame(i) / ReadFrame(i) / FrameMs(i) / this[i].
        foreach (string name in new[] { "Frame", "ReadFrame", "FrameMs", "GetFrame" })
        {
            MethodInfo? m = t.GetMethod(name, new[] { typeof(int) });
            if (m != null) return m.Invoke(reader, new object[] { index })!;
        }
        PropertyInfo? indexer = t.GetProperty("Item", new[] { typeof(int) });
        if (indexer != null) return indexer.GetValue(reader, new object[] { index })!;
        throw new MissingMemberException("MIDAC frame accessor not found (Frame/ReadFrame/FrameMs/[i])");
    }

    private static int PolarityOf(object frame)
    {
        object? p = GetMember(frame, "IonPolarity", "Polarity");
        if (p == null) return 0;
        string s = p.ToString() ?? "";
        if (s.Contains("Positive", StringComparison.OrdinalIgnoreCase)) return 1;
        if (s.Contains("Negative", StringComparison.OrdinalIgnoreCase)) return -1;
        return 0;
    }

    private static object? GetMember(object obj, params string[] names)
    {
        Type t = obj.GetType();
        foreach (string name in names)
        {
            PropertyInfo? p = t.GetProperty(name);
            if (p != null) return p.GetValue(obj);
            MethodInfo? m = t.GetMethod(name, Type.EmptyTypes);
            if (m != null) return m.Invoke(obj, null);
        }
        return null;
    }

    private static double[] AsDoubleArray(object? value)
    {
        switch (value)
        {
            case null: return Array.Empty<double>();
            case double[] d: return d;
            case float[] f: { var o = new double[f.Length]; for (int i = 0; i < f.Length; i++) o[i] = f[i]; return o; }
            case int[] ii: { var o = new double[ii.Length]; for (int i = 0; i < ii.Length; i++) o[i] = ii[i]; return o; }
            default: return Array.Empty<double>();
        }
    }

    private static double TryDouble(object? value, double fallback) =>
        value == null ? fallback : Convert.ToDouble(value);

    private static unsafe IntPtr AllocDoubles(double[] src)
    {
        IntPtr p = Marshal.AllocHGlobal(checked(src.Length * sizeof(double)));
        Marshal.Copy(src, 0, p, src.Length);
        return p;
    }

    [UnmanagedCallersOnly(EntryPoint = "FreeFrame")]
    public static unsafe void FreeFrame(FrameOut* outPtr) => FreeFrameInternal(outPtr);

    private static unsafe void FreeFrameInternal(FrameOut* outPtr)
    {
        if (outPtr == null) return;
        if (outPtr->MzPtr != IntPtr.Zero) { Marshal.FreeHGlobal(outPtr->MzPtr); outPtr->MzPtr = IntPtr.Zero; }
        if (outPtr->IntensityPtr != IntPtr.Zero) { Marshal.FreeHGlobal(outPtr->IntensityPtr); outPtr->IntensityPtr = IntPtr.Zero; }
        if (outPtr->MobilityPtr != IntPtr.Zero) { Marshal.FreeHGlobal(outPtr->MobilityPtr); outPtr->MobilityPtr = IntPtr.Zero; }
        outPtr->NPoints = 0;
    }

    [UnmanagedCallersOnly(EntryPoint = "Close")]
    public static void Close(long handle)
    {
        try
        {
            if (Readers.TryRemove(handle, out var st))
            {
                MethodInfo? close = st.Reader.GetType().GetMethod("Close") ?? st.Reader.GetType().GetMethod("Dispose");
                close?.Invoke(st.Reader, null);
            }
        }
        catch (Exception e) { _lastError = e.ToString(); }
    }

    [UnmanagedCallersOnly(EntryPoint = "LastError")]
    public static unsafe int LastError(ushort* buf, int cap)
    {
        string msg = _lastError ?? "";
        int len = msg.Length;
        if (buf == null || cap <= 0) return len;
        int n = Math.Min(cap, len);
        for (int i = 0; i < n; i++) buf[i] = msg[i];
        return len;
    }
}
