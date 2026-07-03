// ShimadzuGlue/Glue.cs
//
// Thin C# shim that reads Shimadzu LabSolutions .lcd files via the vendor
// Shimadzu.LabSolutions.IO managed API and exposes a tiny C ABI (matching src/shimadzu.rs) of
// [UnmanagedCallersOnly] static methods to the Rust host (which boots CoreCLR via netcorehost).
//
// ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. This compiles anywhere (no compile-time reference to the
//    Shimadzu DLLs — everything vendor-specific is reached through reflection at runtime), but only
//    *runs* where Shimadzu.LabSolutions.IO.IoModule.dll (sourced from a ProteoWizard install, flat
//    in pwiz-bin) and a compatible .NET 8 runtime are present. It also carries the restrictive
//    Shimadzu EULA — see README.md.
//
// SHIMADZU.LABSOLUTIONS.IO API SHAPE (from ProteoWizard ShimadzuReader.cpp), all reached by
// reflection here:
//   Shimadzu.LabSolutions.IO.Data.DataObject          (root; new DataObject())
//       .IO   : IDataIO            -> LoadData(string) : <status enum>, Close(), SystemName()
//       .MS   : IMassSpectrometry  -> .Spectrum, .Chromatogram, .Parameters
//   .MS.Parameters:
//       GetAnalysisTime(out int start, out int end, int segment)
//   .MS.Chromatogram:
//       int SegmentCount { get; }, int EventCount(int seg), short GetEventNo(int seg, int idx)
//   .MS.Spectrum:
//       RetTimeToScan(out uint scan, int retTime, short eventNo) : <status>
//       GetMSSpectrumInfo(int scan, out int retTime, out int msLevel, out int precursorMass,
//                         out int precursorScan, out Polarities polarity, out int segment,
//                         out short event) : <status>
//       GetMSSpectrumByScan(out MassSpectrumObject spectrum, int scan, bool profileDesired) : <status>
//   MassSpectrumObject:
//       IList ProfileList { get; }   // elements: .Mass (int), .Intensity (double)
//       IList CentroidList { get; }
//       double RetentionTime, Polarities Polarity, IList PrecursorMzList, ...
//   Unit scaling (ShimadzuReader.cpp): m/z = Mass * (1.0 / MASSNUMBER_UNIT); precursor m/z stored
//   ×1e9; retention time in ms (×0.001). MASSNUMBER_UNIT is a managed constant — reflected below.
//
// Member names/casing can drift between LabSolutions releases, so lookups are tolerant (try a list
// of candidate names, fall back gracefully). Best-effort glue, not a hardened reader.

using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Runtime.InteropServices;

namespace ShimadzuGlue;

/// <summary>Marshalled per-spectrum scalar metadata. Layout MUST match `ShimadzuSpectrumMeta` in
/// src/shimadzu.rs (repr(C)): i64 + 4×i32 + 2×f64 + i64 = 48 bytes, 8-byte aligned.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct ShimadzuSpectrumMeta
{
    public long ScanNumber;
    public int MsLevel;
    public int Polarity;            // 0 = positive, 1 = negative, 2 = unknown
    public int SignalContinuity;    // 0 = profile, 1 = centroid
    public int PrecursorCharge;     // 0 = unknown
    public double RetentionTimeSeconds;
    public double PrecursorMz;      // 0 = none
    public long NPoints;
}

/// <summary>One opened .lcd reader: the managed DataObject tree + resolved reflection handles.</summary>
internal sealed class ShimadzuData
{
    public required object DataObject;
    public required object IoObj;       // .IO
    public required object SpectrumObj; // .MS.Spectrum
    public required int ScanCount;
    public required double MassMultiplier;  // 1 / MASSNUMBER_UNIT
    public const double PrecursorMzMultiplier = 1.0 / 1e9;
    public const double TimeMultiplier = 0.001; // ms -> s

    // Reflection method handles (resolved once at open).
    public required MethodInfo GetSpectrumInfo;
    public required MethodInfo GetSpectrumByScan;
}

internal static class Reflect
{
    public static PropertyInfo? Prop(object o, params string[] names)
    {
        var t = o.GetType();
        foreach (var n in names)
        {
            var p = t.GetProperty(n, BindingFlags.Public | BindingFlags.Instance | BindingFlags.IgnoreCase);
            if (p != null) return p;
        }
        return null;
    }

    public static object? GetProp(object o, params string[] names)
        => Prop(o, names)?.GetValue(o);

    public static MethodInfo? Method(Type t, string name, int argCount)
        => t.GetMethods(BindingFlags.Public | BindingFlags.Instance | BindingFlags.IgnoreCase)
            .FirstOrDefault(m => string.Equals(m.Name, name, StringComparison.OrdinalIgnoreCase)
                                 && m.GetParameters().Length == argCount);

    /// <summary>A status enum (or int) is "success" iff its integer value is 0 or its name is a
    /// known OK synonym. Shimadzu's IDataIO/ISpectrum methods return such a status.</summary>
    public static bool Ok(object? status)
    {
        if (status == null) return false;
        if (status is int i) return i == 0;
        var name = status.ToString() ?? "";
        try
        {
            var v = Convert.ToInt64(status, CultureInfo.InvariantCulture);
            if (v == 0) return true;
        }
        catch { /* non-numeric enum */ }
        return name.Equals("Success", StringComparison.OrdinalIgnoreCase)
            || name.Equals("OK", StringComparison.OrdinalIgnoreCase)
            || name.Equals("NoError", StringComparison.OrdinalIgnoreCase)
            || name.Equals("None", StringComparison.OrdinalIgnoreCase);
    }
}

public static class Api
{
    // --- vendor assembly loading -------------------------------------------------------------

    private static string? _pwizDir;

    /// <summary>Resolve Shimadzu.LabSolutions.* dependencies from the pwiz directory.</summary>
    private static Assembly? OnResolve(object? sender, ResolveEventArgs args)
    {
        if (_pwizDir == null) return null;
        var simple = new AssemblyName(args.Name).Name;
        if (simple == null) return null;
        var dll = Path.Combine(_pwizDir, simple + ".dll");
        return File.Exists(dll) ? Assembly.LoadFrom(dll) : null;
    }

    private static Assembly LoadIoModule(string pwizDir)
    {
        _pwizDir = pwizDir;
        AppDomain.CurrentDomain.AssemblyResolve -= OnResolve;
        AppDomain.CurrentDomain.AssemblyResolve += OnResolve;
        var dll = Path.Combine(pwizDir, "Shimadzu.LabSolutions.IO.IoModule.dll");
        if (!File.Exists(dll))
            throw new FileNotFoundException($"Shimadzu.LabSolutions.IO.IoModule.dll not found in {pwizDir}");
        return Assembly.LoadFrom(dll);
    }

    /// <summary>Reflect ShimadzuUtil/Tool.MASSNUMBER_UNIT from the loaded Shimadzu assemblies; the
    /// int-mass values are m/z * MASSNUMBER_UNIT. Falls back to 20 (the observed .qgd-family scale)
    /// if the constant can't be found — CALIBRATE against a reference conversion if the fallback is
    /// hit.</summary>
    private static double ResolveMassNumberUnit()
    {
        foreach (var asm in AppDomain.CurrentDomain.GetAssemblies()
                     .Where(a => (a.GetName().Name ?? "").StartsWith("Shimadzu", StringComparison.OrdinalIgnoreCase)))
        {
            Type[] types;
            try { types = asm.GetTypes(); } catch { continue; }
            foreach (var t in types)
            {
                var f = t.GetField("MASSNUMBER_UNIT", BindingFlags.Public | BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.IgnoreCase);
                if (f != null)
                {
                    try { return Convert.ToDouble(f.GetValue(null), CultureInfo.InvariantCulture); } catch { }
                }
                var p = t.GetProperty("MASSNUMBER_UNIT", BindingFlags.Public | BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.IgnoreCase);
                if (p != null)
                {
                    try { return Convert.ToDouble(p.GetValue(null), CultureInfo.InvariantCulture); } catch { }
                }
            }
        }
        return 20.0; // fallback hypothesis
    }

    // --- open / scan-count -------------------------------------------------------------------

    internal static ShimadzuData Open(string path, string pwizDir)
    {
        var asm = LoadIoModule(pwizDir);
        var dataType = asm.GetType("Shimadzu.LabSolutions.IO.Data.DataObject")
            ?? asm.GetTypes().FirstOrDefault(t => t.Name == "DataObject")
            ?? throw new Exception("type Shimadzu.LabSolutions.IO.Data.DataObject not found");
        var data = Activator.CreateInstance(dataType)
            ?? throw new Exception("could not construct DataObject");

        var io = Reflect.GetProp(data, "IO") ?? throw new Exception("DataObject.IO missing");
        var loadData = Reflect.Method(io.GetType(), "LoadData", 1)
            ?? throw new Exception("IDataIO.LoadData(string) missing");
        var status = loadData.Invoke(io, new object[] { path });
        if (!Reflect.Ok(status))
            throw new Exception($"LoadData error: {status}"); // may be E_UNSUPPORTEDFILE (IT-TOF/legacy)

        var ms = Reflect.GetProp(data, "MS") ?? throw new Exception("DataObject.MS missing");
        var spectrum = Reflect.GetProp(ms, "Spectrum") ?? throw new Exception("MS.Spectrum missing");
        var parameters = Reflect.GetProp(ms, "Parameters");
        var chromatogram = Reflect.GetProp(ms, "Chromatogram");

        var massMul = 1.0 / ResolveMassNumberUnit();
        int scanCount = ComputeScanCount(spectrum, parameters, chromatogram);

        var getInfo = Reflect.Method(spectrum.GetType(), "GetMSSpectrumInfo", 8)
            ?? throw new Exception("ISpectrum.GetMSSpectrumInfo(8 args) missing");
        var getByScan = Reflect.Method(spectrum.GetType(), "GetMSSpectrumByScan", 3)
            ?? throw new Exception("ISpectrum.GetMSSpectrumByScan(3 args) missing");

        return new ShimadzuData
        {
            DataObject = data,
            IoObj = io,
            SpectrumObj = spectrum,
            ScanCount = scanCount,
            MassMultiplier = massMul,
            GetSpectrumInfo = getInfo,
            GetSpectrumByScan = getByScan,
        };
    }

    /// <summary>Max scan number across all (segment, event) pairs, via RetTimeToScan(endTime).
    /// Mirrors ShimadzuReader.cpp getScanCount. Falls back to probing if the segment/event tree
    /// isn't reachable.</summary>
    private static int ComputeScanCount(object spectrum, object? parameters, object? chromatogram)
    {
        int lastScan = 0;
        try
        {
            // endTime from Parameters.GetAnalysisTime(out start, out end, 0)
            int endTime = 0;
            if (parameters != null)
            {
                var gat = Reflect.Method(parameters.GetType(), "GetAnalysisTime", 3);
                if (gat != null)
                {
                    var args = new object?[] { 0, 0, 0 };
                    gat.Invoke(parameters, args);
                    endTime = Convert.ToInt32(args[1] ?? 0);
                }
            }
            var retTimeToScan = Reflect.Method(spectrum.GetType(), "RetTimeToScan", 3);

            if (chromatogram != null && retTimeToScan != null && endTime > 0)
            {
                int segCount = Convert.ToInt32(Reflect.GetProp(chromatogram, "SegmentCount") ?? 0);
                var eventCountM = Reflect.Method(chromatogram.GetType(), "EventCount", 1);
                var getEventNoM = Reflect.Method(chromatogram.GetType(), "GetEventNo", 2);
                for (int seg = 1; seg <= segCount; seg++)
                {
                    int evCount = eventCountM != null ? Convert.ToInt32(eventCountM.Invoke(chromatogram, new object[] { seg })) : 1;
                    for (int ei = 1; ei <= evCount; ei++)
                    {
                        short eventNo = getEventNoM != null
                            ? Convert.ToInt16(getEventNoM.Invoke(chromatogram, new object[] { seg, ei }))
                            : (short)ei;
                        var args = new object?[] { (uint)0, endTime, eventNo };
                        var st = retTimeToScan.Invoke(spectrum, args);
                        if (Reflect.Ok(st))
                        {
                            int last = Convert.ToInt32(args[0] ?? 0);
                            if (last > lastScan) lastScan = last;
                        }
                    }
                }
            }
        }
        catch { lastScan = 0; }

        if (lastScan > 0) return lastScan;

        // Fallback: probe upward with GetMSSpectrumInfo until failure (bounded).
        try
        {
            var getInfo = Reflect.Method(spectrum.GetType(), "GetMSSpectrumInfo", 8);
            if (getInfo != null)
            {
                const int Cap = 5_000_000;
                int scan = 1;
                for (; scan <= Cap; scan++)
                {
                    var args = new object?[] { scan, 0, 0, 0, 0, null, 0, (short)0 };
                    object? st;
                    try { st = getInfo.Invoke(spectrum, args); } catch { break; }
                    if (!Reflect.Ok(st)) break;
                }
                lastScan = scan - 1;
            }
        }
        catch { }
        return lastScan;
    }

    // --- per-spectrum metadata + data --------------------------------------------------------

    internal static ShimadzuSpectrumMeta Meta(ShimadzuData d, int scan)
    {
        var m = new ShimadzuSpectrumMeta { ScanNumber = scan, MsLevel = 1, Polarity = 2, SignalContinuity = 0 };
        var args = new object?[] { scan, 0, 0, 0, 0, null, 0, (short)0 };
        var st = d.GetSpectrumInfo.Invoke(d.SpectrumObj, args);
        if (Reflect.Ok(st))
        {
            int retTime = Convert.ToInt32(args[1] ?? 0);
            m.MsLevel = Math.Max(1, Convert.ToInt32(args[2] ?? 1));
            int precursorMassInt = Convert.ToInt32(args[3] ?? 0);
            m.RetentionTimeSeconds = retTime * ShimadzuData.TimeMultiplier;
            m.PrecursorMz = precursorMassInt > 0 ? precursorMassInt * ShimadzuData.PrecursorMzMultiplier : 0.0;
            m.Polarity = PolarityCode(args[5]);
        }
        return m;
    }

    private static int PolarityCode(object? polarity)
    {
        if (polarity == null) return 2;
        var name = polarity.ToString() ?? "";
        if (name.IndexOf("Pos", StringComparison.OrdinalIgnoreCase) >= 0) return 0;
        if (name.IndexOf("Neg", StringComparison.OrdinalIgnoreCase) >= 0) return 1;
        try { var v = Convert.ToInt64(polarity, CultureInfo.InvariantCulture); return v == 0 ? 0 : (v == 1 ? 1 : 2); }
        catch { return 2; }
    }

    /// <summary>Return (mz[], intensity[]) for a scan. Prefers profile; falls back to centroid when
    /// the profile list is empty (SpectrumList_Shimadzu does the same).</summary>
    internal static (double[] mz, float[] intensity, bool centroid) Data(ShimadzuData d, int scan)
    {
        object? specObj = null;
        var args = new object?[] { null, scan, true }; // out spectrum, scan, profileDesired
        var st = d.GetSpectrumByScan.Invoke(d.SpectrumObj, args);
        if (Reflect.Ok(st)) specObj = args[0];
        if (specObj == null) throw new Exception($"GetMSSpectrumByScan failed for scan {scan}: {st}");

        var profile = Reflect.GetProp(specObj, "ProfileList") as IList;
        bool centroid = false;
        IList? list = profile;
        if (list == null || list.Count == 0)
        {
            list = Reflect.GetProp(specObj, "CentroidList") as IList;
            centroid = true;
        }
        if (list == null) return (Array.Empty<double>(), Array.Empty<float>(), centroid);

        int n = list.Count;
        var mz = new double[n];
        var inten = new float[n];
        for (int i = 0; i < n; i++)
        {
            var pt = list[i]!;
            long massInt = Convert.ToInt64(Reflect.GetProp(pt, "Mass") ?? 0L);
            double intensity = Convert.ToDouble(Reflect.GetProp(pt, "Intensity") ?? 0.0);
            mz[i] = massInt * d.MassMultiplier;
            inten[i] = (float)intensity;
        }
        return (mz, inten, centroid);
    }

    // --- C ABI (matches src/shimadzu.rs) -----------------------------------------------------

    private static readonly object Gate = new();
    private static long _nextHandle = 1;
    private static readonly Dictionary<long, ShimadzuData> Readers = new();
    // Pins for data arrays we've handed to Rust, keyed by (handle, mzPtr) so DataFree can release.
    private static readonly Dictionary<(long, IntPtr), (GCHandle mz, GCHandle inten)> Pins = new();
    [ThreadStatic] private static string? _lastError;

    static Api()
    {
        // Fail fast on ABI drift.
        if (Marshal.SizeOf<ShimadzuSpectrumMeta>() != 48)
            throw new Exception($"ShimadzuSpectrumMeta must be 48 bytes, is {Marshal.SizeOf<ShimadzuSpectrumMeta>()}");
    }

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static unsafe long Open(ushort* pathUtf16, ushort* pwizDirUtf16)
    {
        try
        {
            string path = new string((char*)pathUtf16);
            string pwiz = new string((char*)pwizDirUtf16);
            var d = Open(path, pwiz);
            lock (Gate)
            {
                long h = _nextHandle++;
                Readers[h] = d;
                return h;
            }
        }
        catch (Exception e) { _lastError = e.ToString(); return 0; }
    }

    [UnmanagedCallersOnly(EntryPoint = "Close")]
    public static void Close(long handle)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); Readers.Remove(handle); }
            if (d != null)
            {
                var close = Reflect.Method(d.IoObj.GetType(), "Close", 0);
                close?.Invoke(d.IoObj, null);
            }
        }
        catch (Exception e) { _lastError = e.ToString(); }
    }

    [UnmanagedCallersOnly(EntryPoint = "SpectrumCount")]
    public static long SpectrumCount(long handle)
    {
        try { lock (Gate) { return Readers.TryGetValue(handle, out var d) ? d.ScanCount : -1; } }
        catch (Exception e) { _lastError = e.ToString(); return -1; }
    }

    [UnmanagedCallersOnly(EntryPoint = "SpectrumMeta")]
    public static unsafe int SpectrumMeta(long handle, long index, ShimadzuSpectrumMeta* outMeta)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 1; }
            int scan = (int)index + 1; // reader index 0-based -> 1-based scan
            var m = Meta(d, scan);
            // n_points requires a data fetch; fill lazily as 0 (Rust doesn't rely on it).
            *outMeta = m;
            return 0;
        }
        catch (Exception e) { _lastError = e.ToString(); return 1; }
    }

    [UnmanagedCallersOnly(EntryPoint = "SpectrumData")]
    public static unsafe int SpectrumData(long handle, long index, double** mzOut, float** intOut, long* nOut)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 1; }
            int scan = (int)index + 1;
            var (mz, inten, _) = Data(d, scan);
            var mzH = GCHandle.Alloc(mz, GCHandleType.Pinned);
            var inH = GCHandle.Alloc(inten, GCHandleType.Pinned);
            var mzP = mz.Length > 0 ? (double*)mzH.AddrOfPinnedObject() : null;
            var inP = inten.Length > 0 ? (float*)inH.AddrOfPinnedObject() : null;
            lock (Gate) { Pins[(handle, (IntPtr)mzP)] = (mzH, inH); }
            *mzOut = mzP;
            *intOut = inP;
            *nOut = mz.Length;
            return 0;
        }
        catch (Exception e) { _lastError = e.ToString(); return 1; }
    }

    [UnmanagedCallersOnly(EntryPoint = "DataFree")]
    public static unsafe void DataFree(long handle, double* mzPtr, float* intPtr)
    {
        try
        {
            lock (Gate)
            {
                if (Pins.TryGetValue((handle, (IntPtr)mzPtr), out var pins))
                {
                    if (pins.mz.IsAllocated) pins.mz.Free();
                    if (pins.inten.IsAllocated) pins.inten.Free();
                    Pins.Remove((handle, (IntPtr)mzPtr));
                }
            }
        }
        catch (Exception e) { _lastError = e.ToString(); }
    }

    [UnmanagedCallersOnly(EntryPoint = "LastError")]
    public static unsafe int LastError(ushort* buf, int cap)
    {
        var msg = _lastError ?? "";
        if (msg.Length == 0) return 0;
        if (buf != null && cap > 0)
        {
            int n = Math.Min(cap, msg.Length);
            for (int i = 0; i < n; i++) buf[i] = msg[i];
        }
        return msg.Length;
    }
}
