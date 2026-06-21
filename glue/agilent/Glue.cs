// AgilentGlue.Exports — reflection-only bridge to Agilent MHDAC, exposed to Rust as a set of
// [UnmanagedCallersOnly] C-ABI functions.
//
// WINDOWS-RUNTIME-ONLY and UNTESTED. This file compiles on any platform with a .NET 8 SDK because
// it never references the MHDAC types at compile time — every MHDAC call goes through
// System.Reflection. At run time it requires the Agilent MHDAC DLLs (sourced from a ProteoWizard
// install's `vendor_api/Agilent` directory; see README.md) and an x64 Windows process.
//
// ABI mirror of src/agilent.rs:
//   long Open(char* pathUtf16, char* mhdacDirUtf16)            -> handle >=0, or negative error
//   long SpectrumCount(long handle)                            -> count >=0, or negative error
//   int  GetSpectrum(long handle, long index, SpectrumOut* out)-> 0 ok, non-zero error
//   void FreeSpectrum(SpectrumOut* out)                        -> frees the two HGlobal buffers
//   void Close(long handle)                                    -> idempotent
//   int  LastError(ushort* bufUtf16, int cap)                  -> FULL length (UTF-16 code units);
//                                                                 fills up to cap, not NUL-terminated
//                                                                 unless room. cap<=0/null => length.
//
// All strings are UTF-16 NUL-terminated (matches .NET native `char`).

using System.Collections.Concurrent;
using System.Reflection;
using System.Runtime.InteropServices;

namespace AgilentGlue;

/// <summary>
/// Layout-compatible mirror of the Rust <c>SpectrumOut</c> struct. Sequential layout + matching
/// field order/sizes keeps it ABI-identical across the boundary. <c>MzPtr</c>/<c>IntensityPtr</c>
/// are <see cref="Marshal.AllocHGlobal(int)"/> buffers of <c>NPoints</c> doubles each.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct SpectrumOut
{
    public IntPtr MzPtr;        // double*  (n_points)
    public IntPtr IntensityPtr; // double*  (n_points), intensity widened to double
    public long NPoints;
    public double RtMinutes;
    public int MsLevel;         // 1 = MS1, 2 = MS2
    public int Polarity;        // 1 = +, -1 = -, 0 = unknown
    public int IsCentroid;      // 1 = centroid/peak, 0 = profile
    public int ScanId;
}

public static class Exports
{
    // ------------------------------------------------------------------
    // Handle registry: keeps live MHDAC reader wrappers alive across calls.
    // ------------------------------------------------------------------
    private static readonly ConcurrentDictionary<long, ReaderState> Readers = new();
    private static long _nextHandle = 0;

    // Per-thread last error for diagnostics surfaced back to Rust.
    [ThreadStatic] private static string? _lastError;

    // ABI drift guard. The Rust side asserts size_of::<SpectrumOut>() == 48 / align == 8 at
    // compile time (src/agilent.rs). Validate the C# layout matches at module load so a field
    // edit on one side that the other doesn't mirror fails loudly here instead of silently
    // corrupting memory across the boundary. Layout: 2×IntPtr(8) + long(8) + double(8) + 4×int(4).
    private const int ExpectedSpectrumOutSize = 48;

    static Exports()
    {
        int actual = Marshal.SizeOf<SpectrumOut>();
        if (actual != ExpectedSpectrumOutSize)
        {
            throw new InvalidOperationException(
                $"ABI mismatch: SpectrumOut is {actual} bytes, expected {ExpectedSpectrumOutSize}. " +
                "The C# struct and the Rust #[repr(C)] SpectrumOut have drifted — keep them in sync.");
        }
    }

    // The MHDAC assembly + resolved reflection members, loaded once from the pwiz dir.
    private static Assembly? _mhdac;
    private static readonly object _loadLock = new();

    /// <summary>State for one open .d: the reflected MassSpecDataReader instance + cached members.</summary>
    private sealed class ReaderState
    {
        public required object Reader;            // MHDAC MassSpecDataReader instance
        public required MhdacApi Api;             // cached reflected members
        public int Count;
    }

    /// <summary>
    /// Cached reflected MethodInfo/PropertyInfo for the MHDAC types we touch. Resolving these once
    /// avoids repeated reflection lookups per spectrum.
    /// </summary>
    private sealed class MhdacApi
    {
        public required MethodInfo OpenDataFile;          // bool MassSpecDataReader.OpenDataFile(string)
        public required MethodInfo CloseDataFile;         // void MassSpecDataReader.CloseDataFile()
        public required PropertyInfo MsScanFileInfo;      // IMsdrDataReader.MSScanFileInformation
        public required PropertyInfo TotalScansPresent;   // IBDAMSScanFileInformation.TotalScansPresent
        public required MethodInfo GetSpectrumByRow;      // IBDASpecData[] GetSpectrum(int,IMsdrPeakFilter,IMsdrPeakFilter,DesiredMSStorageType)
        public required MethodInfo GetScanRecord;         // IMSScanRecord GetScanRecord(int)

        // IBDASpecData members
        public required PropertyInfo SpecXArray;          // double[] XArray
        public required PropertyInfo SpecYArray;          // float[]  YArray
        public required PropertyInfo SpecTotalDataPoints; // int TotalDataPoints
        public required PropertyInfo SpecMsLevelInfo;     // MSLevel MSLevelInfo
        public required PropertyInfo SpecIonPolarity;     // IonPolarity IonPolarity
        public required PropertyInfo SpecMsStorageMode;   // MSStorageMode MSStorageMode
        public required PropertyInfo SpecScanId;          // int ScanId

        // IMSScanRecord members
        public required PropertyInfo RecRetentionTime;    // double RetentionTime

        // DesiredMSStorageType.ProfileElsePeak boxed enum value for the GetSpectrum call.
        public required object DesiredProfileElsePeak;
    }

    // ==================================================================
    // Exports
    // ==================================================================

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static unsafe long Open(ushort* pathUtf16, ushort* mhdacDirUtf16)
    {
        // NOTE: `ushort*` (not `char*`) — `char` is non-blittable under [UnmanagedCallersOnly], which
        // would make the runtime reject/mis-marshal the export. Rust passes `*const u16` (UTF-16).
        try
        {
            string path = Marshal.PtrToStringUni((IntPtr)pathUtf16) ?? "";
            string mhdacDir = Marshal.PtrToStringUni((IntPtr)mhdacDirUtf16) ?? "";

            EnsureMhdacLoaded(mhdacDir);
            MhdacApi api = ResolveApi(_mhdac!);

            // new MassSpecDataReader()
            Type readerType = _mhdac!.GetType("Agilent.MassSpectrometry.DataAnalysis.MassSpecDataReader", throwOnError: true)!;
            object reader = Activator.CreateInstance(readerType)!;

            // Dispose the freshly-created reader on any failure between here and successful
            // registration, otherwise a failed Open leaks the native MHDAC handle it holds.
            try
            {
                bool ok = (bool)api.OpenDataFile.Invoke(reader, new object[] { path })!;
                if (!ok)
                {
                    _lastError = $"MassSpecDataReader.OpenDataFile returned false for '{path}'";
                    DisposeReader(api, reader);
                    return -2;
                }

                // TotalScansPresent via MSScanFileInformation.
                object scanFileInfo = api.MsScanFileInfo.GetValue(reader)!;
                int count = Convert.ToInt32(api.TotalScansPresent.GetValue(scanFileInfo));

                long handle = Interlocked.Increment(ref _nextHandle);
                Readers[handle] = new ReaderState { Reader = reader, Api = api, Count = count };
                return handle;
            }
            catch
            {
                DisposeReader(api, reader);
                throw; // surfaced by the outer catch as -1 with _lastError set
            }
        }
        catch (Exception ex)
        {
            _lastError = Flatten(ex);
            return -1;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "SpectrumCount")]
    public static long SpectrumCount(long handle)
    {
        try
        {
            if (!Readers.TryGetValue(handle, out ReaderState? st))
            {
                _lastError = $"unknown handle {handle}";
                return -1;
            }
            return st.Count;
        }
        catch (Exception ex)
        {
            _lastError = Flatten(ex);
            return -1;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "GetSpectrum")]
    public static unsafe int GetSpectrum(long handle, long index, SpectrumOut* outPtr)
    {
        // Zero the out struct so a failure leaves nothing for Rust to free.
        if (outPtr != null) *outPtr = default;
        try
        {
            if (!Readers.TryGetValue(handle, out ReaderState? st))
            {
                _lastError = $"unknown handle {handle}";
                return 1;
            }
            if (outPtr == null)
            {
                _lastError = "null out pointer";
                return 2;
            }
            if (index < 0 || index >= st.Count)
            {
                _lastError = $"index {index} out of range (count {st.Count})";
                return 3;
            }

            MhdacApi api = st.Api;

            // GetSpectrum(rowNumber, peakFilter=null, peakFilter2=null, DesiredMSStorageType.ProfileElsePeak)
            // Returns IBDASpecData[]; we use element 0 (single device, MS data).
            object specArrObj = api.GetSpectrumByRow.Invoke(
                st.Reader,
                new object?[] { (int)index, null, null, api.DesiredProfileElsePeak })!;

            object? spec = FirstElement(specArrObj);
            if (spec == null)
            {
                _lastError = $"GetSpectrum({index}) returned no spectrum";
                return 4;
            }

            // Arrays. Use LongLength / long throughout: NPoints is a long across the ABI, and an
            // `int` count would cap at ~2G points and could overflow vs the long field.
            double[] x = (double[])api.SpecXArray.GetValue(spec)!;
            // YArray is float[] in MHDAC; widen to double for the ABI.
            Array yRaw = (Array)api.SpecYArray.GetValue(spec)!;
            long n = x.LongLength;
            if (yRaw.LongLength < n) n = yRaw.LongLength; // defensive: use the shorter length

            // Scan record for retention time (minutes). The ABI field is RtMinutes (Rust:
            // rt_minutes) — Agilent reports RT natively in minutes and mzdata's ScanEvent.start_time
            // is also minutes, so the value crosses the boundary unconverted. Keep this contract in
            // sync with the comment on SpectrumOut.RtMinutes and src/agilent.rs.
            double rtMinutes = 0.0;
            try
            {
                object rec = api.GetScanRecord.Invoke(st.Reader, new object[] { (int)index })!;
                rtMinutes = Convert.ToDouble(api.RecRetentionTime.GetValue(rec));
            }
            catch
            {
                // RT is best-effort; leave 0 if the record is unavailable.
            }

            // Allocate both unmanaged buffers under one try/finally so a throw from the second
            // AllocHGlobal (or the fill loop) cannot leak the first. On success we null out the
            // locals so finally does not free buffers now owned by *outPtr; on any failure finally
            // frees whatever was allocated. (checked() turns an n*8 overflow into an exception that
            // the outer catch maps to an error code rather than a short allocation.)
            IntPtr mzBuf = IntPtr.Zero;
            IntPtr intBuf = IntPtr.Zero;
            try
            {
                mzBuf = Marshal.AllocHGlobal(checked((nint)(n * sizeof(double))));
                intBuf = Marshal.AllocHGlobal(checked((nint)(n * sizeof(double))));
                unsafe
                {
                    double* mzDst = (double*)mzBuf;
                    double* intDst = (double*)intBuf;
                    for (long i = 0; i < n; i++)
                    {
                        mzDst[i] = x[i];
                        // Guard non-finite intensity (NaN/Inf): Rust narrows this to f32 and feeds
                        // it into a numeric column, so coerce garbage to 0 at the source.
                        double yi = Convert.ToDouble(yRaw.GetValue(i));
                        intDst[i] = double.IsFinite(yi) ? yi : 0.0;
                    }
                }

                SpectrumOut o = default;
                o.MzPtr = mzBuf;
                o.IntensityPtr = intBuf;
                o.NPoints = n;
                o.RtMinutes = rtMinutes;
                o.MsLevel = MapMsLevel(api.SpecMsLevelInfo.GetValue(spec));
                o.Polarity = MapPolarity(api.SpecIonPolarity.GetValue(spec));
                o.IsCentroid = MapStorageMode(api.SpecMsStorageMode.GetValue(spec));
                o.ScanId = Convert.ToInt32(api.SpecScanId.GetValue(spec));
                *outPtr = o;

                // Ownership transferred to *outPtr; prevent finally from freeing them.
                mzBuf = IntPtr.Zero;
                intBuf = IntPtr.Zero;
                return 0;
            }
            finally
            {
                if (mzBuf != IntPtr.Zero) Marshal.FreeHGlobal(mzBuf);
                if (intBuf != IntPtr.Zero) Marshal.FreeHGlobal(intBuf);
            }
        }
        catch (Exception ex)
        {
            _lastError = Flatten(ex);
            return 100;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "FreeSpectrum")]
    public static unsafe void FreeSpectrum(SpectrumOut* outPtr)
    {
        // Must never let an exception escape the unmanaged boundary — that would kill the process.
        try
        {
            if (outPtr == null) return;
            if (outPtr->MzPtr != IntPtr.Zero) { Marshal.FreeHGlobal(outPtr->MzPtr); outPtr->MzPtr = IntPtr.Zero; }
            if (outPtr->IntensityPtr != IntPtr.Zero) { Marshal.FreeHGlobal(outPtr->IntensityPtr); outPtr->IntensityPtr = IntPtr.Zero; }
            outPtr->NPoints = 0;
        }
        catch (Exception ex)
        {
            _lastError = Flatten(ex);
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "Close")]
    public static void Close(long handle)
    {
        try
        {
            if (Readers.TryRemove(handle, out ReaderState? st))
            {
                DisposeReader(st.Api, st.Reader);
            }
        }
        catch (Exception ex)
        {
            _lastError = Flatten(ex);
        }
    }

    /// <summary>
    /// Release a MHDAC reader: CloseDataFile (best effort) then Dispose. Used both by Close and by
    /// Open's failure path so a failed Open does not leak the native handle. Never throws.
    /// </summary>
    private static void DisposeReader(MhdacApi api, object reader)
    {
        try { api.CloseDataFile.Invoke(reader, Array.Empty<object>()); }
        catch { /* best effort */ }
        try { (reader as IDisposable)?.Dispose(); }
        catch { /* best effort */ }
    }

    [UnmanagedCallersOnly(EntryPoint = "LastError")]
    public static unsafe int LastError(ushort* buf, int cap)
    {
        // Fill-buffer contract (matches src/agilent.rs LastErrorFn): copy the stashed last-error
        // message (UTF-16) into `buf` for up to `cap` code units (NOT NUL-terminated unless room),
        // and ALWAYS return the FULL length in UTF-16 code units so the caller can detect
        // truncation. A null `buf` or `cap <= 0` just reports the needed length. Wrapped in
        // try/catch so it can never throw across the unmanaged boundary (returns 0 on failure).
        try
        {
            string msg = _lastError ?? string.Empty;
            int full = msg.Length; // UTF-16 code units
            if (buf == null || cap <= 0)
            {
                return full;
            }
            int toCopy = Math.Min(full, cap);
            for (int i = 0; i < toCopy; i++) buf[i] = msg[i];
            // NUL-terminate only if there is room beyond the copied units (the caller relies on the
            // returned length, not a terminator, but a terminator is friendly when it fits).
            if (toCopy < cap) buf[toCopy] = (ushort)'\0';
            return full;
        }
        catch
        {
            // Diagnostics-only export: on failure report "nothing written" rather than crash.
            return 0;
        }
    }

    // ==================================================================
    // MHDAC loading + reflection resolution
    // ==================================================================

    /// <summary>
    /// Load MHDAC from <paramref name="mhdacDir"/> (the pwiz <c>vendor_api/Agilent</c> directory)
    /// once, and register an AssemblyResolve handler so MHDAC's own dependencies (BaseCommon.dll,
    /// BaseDataAccess.dll, agtsampleinforw.dll, …) resolve from the same directory.
    /// </summary>
    private static void EnsureMhdacLoaded(string mhdacDir)
    {
        if (_mhdac != null) return;
        lock (_loadLock)
        {
            if (_mhdac != null) return;

            if (!Directory.Exists(mhdacDir))
                throw new DirectoryNotFoundException($"MHDAC directory not found: '{mhdacDir}'");

            // Resolve sibling vendor dependencies on demand.
            AppDomain.CurrentDomain.AssemblyResolve += (_, args) =>
            {
                string simpleName = new AssemblyName(args.Name).Name + ".dll";
                string candidate = Path.Combine(mhdacDir, simpleName);
                return File.Exists(candidate) ? Assembly.LoadFrom(candidate) : null;
            };

            string main = Path.Combine(mhdacDir, "MassSpecDataReader.dll");
            if (!File.Exists(main))
                throw new FileNotFoundException($"MassSpecDataReader.dll not found in '{mhdacDir}'", main);

            _mhdac = Assembly.LoadFrom(main);
        }
    }

    private static MhdacApi ResolveApi(Assembly mhdac)
    {
        const string ns = "Agilent.MassSpectrometry.DataAnalysis.";
        Type readerType = mhdac.GetType(ns + "MassSpecDataReader", throwOnError: true)!;
        Type specType = mhdac.GetType(ns + "IBDASpecData", throwOnError: true)!;
        Type scanRecType = mhdac.GetType(ns + "IMSScanRecord", throwOnError: true)!;
        Type scanFileInfoType = mhdac.GetType(ns + "IBDAMSScanFileInformation", throwOnError: true)!;
        Type desiredStorageType = mhdac.GetType(ns + "DesiredMSStorageType", throwOnError: true)!;

        // MassSpecDataReader implements IMsdrDataReader; resolve members from the concrete type so
        // both interface-mapped and direct members are found.
        MethodInfo openDataFile = FindMethod(readerType, "OpenDataFile", new[] { typeof(string) });
        MethodInfo? closeDataFile = readerType.GetMethod("CloseDataFile", Type.EmptyTypes);
        PropertyInfo msScanFileInfo = FindProp(readerType, "MSScanFileInformation");
        PropertyInfo totalScans = FindProp(scanFileInfoType, "TotalScansPresent");

        // GetSpectrum(int, IMsdrPeakFilter, IMsdrPeakFilter, DesiredMSStorageType) — match by name
        // + first parameter being int (overload disambiguation across the several GetSpectrum forms).
        MethodInfo getSpectrumByRow = readerType.GetMethods()
            .First(m => m.Name == "GetSpectrum"
                        && m.GetParameters().Length == 4
                        && m.GetParameters()[0].ParameterType == typeof(int));
        MethodInfo getScanRecord = FindMethod(readerType, "GetScanRecord", new[] { typeof(int) });

        object desiredProfileElsePeak = Enum.Parse(desiredStorageType, "ProfileElsePeak");

        return new MhdacApi
        {
            OpenDataFile = openDataFile,
            CloseDataFile = closeDataFile ?? FindMethod(readerType, "CloseDataFile", Type.EmptyTypes),
            MsScanFileInfo = msScanFileInfo,
            TotalScansPresent = totalScans,
            GetSpectrumByRow = getSpectrumByRow,
            GetScanRecord = getScanRecord,

            SpecXArray = FindProp(specType, "XArray"),
            SpecYArray = FindProp(specType, "YArray"),
            SpecTotalDataPoints = FindProp(specType, "TotalDataPoints"),
            SpecMsLevelInfo = FindProp(specType, "MSLevelInfo"),
            SpecIonPolarity = FindProp(specType, "IonPolarity"),
            SpecMsStorageMode = FindProp(specType, "MSStorageMode"),
            SpecScanId = FindProp(specType, "ScanId"),

            RecRetentionTime = FindProp(scanRecType, "RetentionTime"),

            DesiredProfileElsePeak = desiredProfileElsePeak,
        };
    }

    // ==================================================================
    // Enum mapping (by name, since the enum types are reflected)
    // ==================================================================

    // MHDAC MSLevel: MS=1, MSMS=2 (also "All"). We map MSMS->2, everything else->1.
    private static int MapMsLevel(object? v)
    {
        string s = v?.ToString() ?? "MS";
        return s.Equals("MSMS", StringComparison.OrdinalIgnoreCase) ? 2 : 1;
    }

    // MHDAC IonPolarity: Positive / Negative / Mixed / Unassigned.
    private static int MapPolarity(object? v)
    {
        string s = v?.ToString() ?? "";
        if (s.StartsWith("Pos", StringComparison.OrdinalIgnoreCase)) return 1;
        if (s.StartsWith("Neg", StringComparison.OrdinalIgnoreCase)) return -1;
        return 0;
    }

    // MHDAC MSStorageMode: ProfileSpectrum / PeakDetectedSpectrum / Mixed / Unspecified.
    // Treat "Peak..." as centroid (1), everything else as profile (0).
    private static int MapStorageMode(object? v)
    {
        string s = v?.ToString() ?? "";
        return s.IndexOf("Peak", StringComparison.OrdinalIgnoreCase) >= 0 ? 1 : 0;
    }

    // ==================================================================
    // Small reflection helpers
    // ==================================================================

    private static MethodInfo FindMethod(Type t, string name, Type[] args)
    {
        MethodInfo? m = t.GetMethod(name, args);
        if (m != null) return m;
        // Fall back to interface maps for explicitly-implemented members.
        foreach (Type iface in t.GetInterfaces())
        {
            m = iface.GetMethod(name, args);
            if (m != null) return m;
        }
        throw new MissingMethodException(t.FullName, name);
    }

    private static PropertyInfo FindProp(Type t, string name)
    {
        PropertyInfo? p = t.GetProperty(name);
        if (p != null) return p;
        foreach (Type iface in t.GetInterfaces())
        {
            p = iface.GetProperty(name);
            if (p != null) return p;
        }
        throw new MissingMemberException(t.FullName, name);
    }

    private static object? FirstElement(object arrayObj)
    {
        if (arrayObj is Array a)
            return a.Length > 0 ? a.GetValue(0) : null;
        return arrayObj; // single object fallback
    }

    private static string Flatten(Exception ex)
    {
        // Reflection wraps the real fault in TargetInvocationException; unwrap for a useful message.
        Exception e = ex is TargetInvocationException tie && tie.InnerException != null ? tie.InnerException : ex;
        return $"{e.GetType().Name}: {e.Message}";
    }
}
