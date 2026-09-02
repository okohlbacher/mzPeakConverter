// ShimadzuGlue/Glue.cs
//
// Thin C# shim that reads Shimadzu LabSolutions .lcd files via the vendor
// Shimadzu.LabSolutions.IO managed API and exposes a tiny C ABI (matching src/shimadzu.rs) of
// [UnmanagedCallersOnly] static methods to the Rust host (which boots CoreCLR via netcorehost).
//
// ⚠️ WINDOWS-RUNTIME-ONLY. This compiles anywhere (no compile-time reference to the Shimadzu DLLs
//    — everything vendor-specific is reached through reflection at runtime), but only *runs* where
//    Shimadzu.LabSolutions.IO.IoModule.dll (sourced from a ProteoWizard install, flat in pwiz-bin)
//    and a compatible .NET 8 runtime are present. It also carries the restrictive Shimadzu EULA —
//    see README.md.
//
//    Verified against real data on 2026-08-20 (LCMS-9030 QTOF). Set MZPC_SHIMADZU_DEBUG=1 to trace
//    scan-count discovery and the mass-unit resolution on stderr; both fail silently otherwise.
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

/// <summary>V2 metadata: the V1 layout VERBATIM as a prefix, plus the precursor/acquisition fields
/// the native lane was missing against the mzML lane. Never reorder or resize the prefix — old
/// callers reach V1 through the unchanged `SpectrumMeta` export and must keep working.
///
/// Sources on `MassSpectrumObject` (confirmed by reflective dump of DIA_Hela_20ng scan 2):
/// `AcqModeMz` 4525000 -> isolation target m/z 452.5; `QTransmissionWidthMz` 170 -> 17.0 Th window;
/// `CollisionEnergy` 18.4; `PrecursorScanNo`; `PrecursorChargeState`; `SegmentNo`; `EventNo`.</summary>
public struct ShimadzuSpectrumMetaV2
{
    // --- V1 prefix, byte-for-byte (48 B) ---
    public long ScanNumber;
    public int MsLevel;
    public int Polarity;
    public int SignalContinuity;
    public int PrecursorCharge;
    public double RetentionTimeSeconds;
    public double PrecursorMz;
    public long NPoints;
    // --- V2 additions (40 B) ---
    public double IsolationTargetMz;   // 0 = none
    public double IsolationWidthMz;    // 0 = unknown; full width, so the offsets are half each side
    public double CollisionEnergy;     // 0 = none
    public long PrecursorScanNumber;   // 0 = none
    public int SegmentNo;
    public int EventNo;
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
    public object? ParametersObj;   // .MS.Parameters — GetMassRawRange(seg, event) is the scan window
    public object? SampleInfoObj;   // .SampleInfo  — AnalysisDate is the run start time
    /// Multiplier for the point's `MassHigh` (Int64) field, or 0 to read the coarse `Mass` (Int32)
    /// instead. Decided ONCE per file at open (see `Api.DecideMassScale`); never mixed within a file.
    public double HighMassMul;
    // (segment, event) -> m/z range, memoised: an event's range is fixed for the whole run.
    public readonly Dictionary<(int, int), (double lo, double hi)> RangeCache = new();

    // One-entry memo for Api.Data: the caller fetches profile then centroid for the same scan.
    public int CachedScan = -1;
    public ((double[] mz, float[] intensity) profile, (double[] mz, float[] intensity) centroid)? CachedData;

    // One-entry memo for the decoded MassSpectrumObject. SpectrumMetaV2 needs the same object
    // Data() decodes (the precursor/acquisition scalars live on it), and the caller asks for meta
    // and data on the same scan back to back — without this each spectrum costs two full decodes.
    public int CachedSpecScan = -1;
    public object? CachedSpecObj;
}

/// <summary>Diagnostics for the scan-count discovery, which otherwise swallows every failure and
/// reports 0 spectra with no clue why. Set MZPC_SHIMADZU_DEBUG=1 to trace it on stderr.</summary>
internal static class Dbg
{
    // `internal`, not private: callers must be able to GUARD the interpolation, not just the write.
    // A `Dbg.Say($"...{list.Count}...")` builds its string — and so calls the vendor Count getters —
    // on every scan even with debug off, which is exactly the kind of extra vendor interaction the
    // rotation investigation has to hold constant.
    internal static readonly bool On =
        Environment.GetEnvironmentVariable("MZPC_SHIMADZU_DEBUG") is string v && v != "0" && v != "";

    internal static void Say(string msg)
    {
        if (On) Console.Error.WriteLine("[shimadzu-glue] " + msg);
    }
}

/// <summary>Stage-B experiment levers for the centroid intensity rotation.
///
/// The defect: on `.lcd` files that store NO profile signal, the vendor's centroid list comes back
/// with intensities rotated against the m/z axis (`[s alien values] + truth[0:n-s]`, s in 1..7, last
/// peak dropped). Files that DO store profile are bit-exact, so the leading theory is that the
/// centroid intensity view is mis-based when the decode struct carries no profile arrays. These
/// levers exist to test that; they are read once, so a run cannot change mode mid-flight.</summary>
internal static class Exp
{
    internal static readonly bool ProfileDesired =
        Environment.GetEnvironmentVariable("MZPC_SHIMADZU_PROFILE_DESIRED") != "0";

    /// legacy (default) · centroid-first · centroid-only · split
    internal static readonly string Fetch =
        (Environment.GetEnvironmentVariable("MZPC_SHIMADZU_FETCH") ?? "legacy").Trim().ToLowerInvariant();

    /// 1-based vendor scan number to dump once, or -1.
    internal static readonly int DumpScan =
        int.TryParse(Environment.GetEnvironmentVariable("MZPC_SHIMADZU_DUMP"), out var s) ? s : -1;

    /// Any lever off its default makes the one-entry memo a liability: an experiment that re-reads
    /// the same scan would be served the cache instead of the vendor, and would report "unchanged"
    /// no matter what the vendor does.
    internal static bool Active => !ProfileDesired || Fetch != "legacy" || DumpScan >= 0;

    private static bool _dumped;

    /// <summary>One-shot reflective dump of the spectrum object and the first point of each list.
    /// Call only AFTER the arrays have been copied out — these getters may themselves mutate vendor
    /// state, which would contaminate the very reading being investigated.</summary>
    internal static void Dump(int scan, object? specObj, IList? profile, IList? centroid)
    {
        if (_dumped || scan != DumpScan) return;
        _dumped = true;
        DumpObject($"scan {scan} MassSpectrumObject", specObj);
        DumpObject($"scan {scan} ProfileList[0]", profile != null && profile.Count > 0 ? profile[0] : null);
        DumpObject($"scan {scan} CentroidList[0]", centroid != null && centroid.Count > 0 ? centroid[0] : null);
        // The precursor list is the vendor's own answer for "what was selected"; the scalar that
        // GetSpectrumInfo hands back has no documented scale and lands outside the instrument's
        // m/z range under every obvious one, so read this instead of guessing a divisor.
        if (Reflect.GetProp(specObj!, "PrecursorMzList") is IList pl)
        {
            Console.Error.WriteLine($"[shimadzu-dump] scan {scan} PrecursorMzList: {pl.Count} entries");
            for (int i = 0; i < Math.Min(pl.Count, 3); i++)
                DumpObject($"scan {scan} PrecursorMzList[{i}]", pl[i]);
        }
    }

    /// <summary>Reflective map of the reader-level object graph (DataObject → MS → Parameters …),
    /// for finding where scan windows and instrument identity live. `MZPC_SHIMADZU_DUMP_READER=1`.
    /// Lists properties WITH scalar values, and method signatures, two levels deep.</summary>
    internal static readonly bool DumpReader =
        Environment.GetEnvironmentVariable("MZPC_SHIMADZU_DUMP_READER") is string r && r != "0" && r != "";

    internal static void DumpGraph(string label, object? o, int depth)
    {
        if (o == null) { Console.Error.WriteLine($"[shimadzu-graph] {label}: null"); return; }
        var t = o.GetType();
        Console.Error.WriteLine($"[shimadzu-graph] {label}: {t.FullName}");
        foreach (var m in t.GetMethods(BindingFlags.Public | BindingFlags.Instance | BindingFlags.DeclaredOnly))
        {
            if (m.IsSpecialName) continue; // property accessors
            var ps = string.Join(", ", m.GetParameters().Select(x => (x.IsOut ? "out " : "") + x.ParameterType.Name + " " + x.Name));
            Console.Error.WriteLine($"[shimadzu-graph]   {m.ReturnType.Name} {m.Name}({ps})");
        }
        foreach (var p in t.GetProperties(BindingFlags.Public | BindingFlags.Instance))
        {
            if (p.GetIndexParameters().Length > 0) continue;
            object? v;
            try { v = p.GetValue(o); } catch (Exception e) { v = "<throws " + e.GetType().Name + ">"; }
            bool scalar = v == null || v is string || v.GetType().IsPrimitive || v.GetType().IsEnum || v is decimal;
            if (v is IList list)
            {
                Console.Error.WriteLine($"[shimadzu-graph]   .{p.Name} : IList[{list.Count}] of {p.PropertyType.Name}");
                if (depth > 0 && list.Count > 0) DumpGraph($"{label}.{p.Name}[0]", list[0], depth - 1);
            }
            else if (scalar)
                Console.Error.WriteLine($"[shimadzu-graph]   .{p.Name} ({p.PropertyType.Name}) = {v}");
            else
            {
                Console.Error.WriteLine($"[shimadzu-graph]   .{p.Name} : {p.PropertyType.Name}");
                if (depth > 0) DumpGraph($"{label}.{p.Name}", v, depth - 1);
            }
        }
    }

    private static void DumpObject(string label, object? o)
    {
        if (o == null) { Console.Error.WriteLine($"[shimadzu-dump] {label}: null"); return; }
        var t = o.GetType();
        // A primitive has no properties to walk — print the value, which is the whole point when
        // the list element IS the datum (PrecursorMzList is a List<Int32>).
        if (t.IsPrimitive || o is string || o is decimal)
        {
            Console.Error.WriteLine($"[shimadzu-dump] {label}: {t.Name} = {o}");
            return;
        }
        Console.Error.WriteLine($"[shimadzu-dump] {label}: type {t.FullName}");
        foreach (var p in t.GetProperties(BindingFlags.Public | BindingFlags.Instance))
        {
            if (p.GetIndexParameters().Length > 0) continue; // skip indexers
            object? v;
            try { v = p.GetValue(o); } catch (Exception e) { v = "<throws " + e.GetType().Name + ">"; }
            if (v is IList list) v = $"IList[{list.Count}]";
            Console.Error.WriteLine($"[shimadzu-dump]   {p.Name} ({p.PropertyType.Name}) = {v}");
        }
        foreach (var f in t.GetFields(BindingFlags.Public | BindingFlags.Instance))
        {
            object? v;
            try { v = f.GetValue(o); } catch (Exception e) { v = "<throws " + e.GetType().Name + ">"; }
            if (v is IList list) v = $"IList[{list.Count}]";
            Console.Error.WriteLine($"[shimadzu-dump]   .{f.Name} ({f.FieldType.Name}) = {v}");
        }
    }
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

    /// <summary>Invoke `m` after coercing each argument to the parameter type the method actually
    /// declares.</summary>
    ///
    /// <remarks>
    /// Reflection `Invoke` demands EXACT value types: a boxed `int` will not bind to a `short`,
    /// `uint` or enum parameter, it throws ArgumentException. The Shimadzu signatures use a mix
    /// (`GetMSSpectrumInfo(uint scan, ...)`, `GetEventNo(short, short)`), and the widths drift
    /// between LabSolutions releases, so hardcoding them here would break on the next one. Both the
    /// segment/event scan-count path and its probe fallback were failing on exactly this and being
    /// swallowed by a bare `catch`, so every `.lcd` reported 0 spectra whatever the file contained.
    /// `out`/`ref` parameters are coerced through their element type and written back by the caller.
    /// </remarks>
    public static object? InvokeCoerced(MethodInfo m, object target, object?[] args)
    {
        var ps = m.GetParameters();
        for (int i = 0; i < ps.Length && i < args.Length; i++)
        {
            var want = ps[i].ParameterType;
            if (want.IsByRef) want = want.GetElementType()!;
            args[i] = Coerce(args[i], want);
        }
        return m.Invoke(target, args);
    }

    /// <summary>Best-effort conversion of a boxed value to `want` (enums and numeric widths).</summary>
    public static object? Coerce(object? value, Type want)
    {
        if (value == null) return null;
        if (want.IsInstanceOfType(value)) return value;
        try
        {
            if (want.IsEnum) return Enum.ToObject(want, Convert.ToInt64(value, CultureInfo.InvariantCulture));
            if (want.IsPrimitive || want == typeof(decimal))
                return Convert.ChangeType(value, want, CultureInfo.InvariantCulture);
        }
        catch { /* fall through: hand the original back and let Invoke report it */ }
        return value;
    }

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
        // Nothing found. Dump the candidates so the constant can be identified rather than guessed:
        // getting this wrong does not fail, it silently writes a WRONG m/z axis.
        if (true)
        {
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies()
                         .Where(a => (a.GetName().Name ?? "").StartsWith("Shimadzu", StringComparison.OrdinalIgnoreCase)))
            {
                Type[] types;
                try { types = asm.GetTypes(); } catch { continue; }
                foreach (var t in types)
                    foreach (var f in t.GetFields(BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static))
                    {
                        var n = f.Name.ToUpperInvariant();
                        if (!n.Contains("UNIT") && !n.Contains("MASS") && !n.Contains("SCALE")) continue;
                        object? v = null;
                        try { v = f.GetValue(null); } catch { }
                        Dbg.Say($"candidate constant {t.FullName}.{f.Name} = {v}");
                    }
            }
        }
        // The vendor assembly exposes NO such constant (verified by dumping every static field whose
        // name mentions UNIT/MASS/SCALE on an LCMS-9030 install: none). ProteoWizard carries it as a
        // C++ constant in ShimadzuReader.cpp, not in the managed DLL, so it cannot be reflected and
        // must be pinned here.
        //
        // 10000 = masses stored as integers with 4 decimal places. Established against a known-good
        // msconvert conversion of the same file (MTBLS5861 HEK_PosOAD1.lcd, LCMS-9030 QTOF):
        // msconvert reports m/z 70.0-1250.0, the raw integers are 700000-12500000, ratio 10000
        // exactly on both bounds. The previous fallback of 20 was a guess and was wrong by 500x.
        Dbg.Say("MASSNUMBER_UNIT not exposed by the vendor assembly; using the pinned 10000");
        return 10000.0;
    }

    // --- open / scan-count -------------------------------------------------------------------

    // NOT named `Open`: the exported [UnmanagedCallersOnly] `Open(ushort*, ushort*)` lives in this
    // same class, and the Rust host resolves exports by NAME through reflection. An overload made
    // that lookup ambiguous, so every native `.lcd` conversion died at startup with
    // AmbiguousMatchException (HRESULT 0x8000211D) -- before touching the file, which is why it
    // looked like an unsupported .lcd variant. Keep exported entry-point names unique in `Api`.
    internal static ShimadzuData OpenData(string path, string pwizDir)
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

        if (Exp.DumpReader)
        {
            Exp.DumpGraph("DataObject", data, 1);
            Exp.DumpGraph("DataObject.MS", ms, 1);
            Exp.DumpGraph("DataObject.MS.Parameters", parameters, 2);
        }

        var unit = ResolveMassNumberUnit();
        var massMul = 1.0 / unit;
        Dbg.Say($"MASSNUMBER_UNIT resolved to {unit} (massMul={massMul})");
        int scanCount = ComputeScanCount(spectrum, parameters, chromatogram);

        var getInfo = Reflect.Method(spectrum.GetType(), "GetMSSpectrumInfo", 8)
            ?? throw new Exception("ISpectrum.GetMSSpectrumInfo(8 args) missing");
        var getByScan = Reflect.Method(spectrum.GetType(), "GetMSSpectrumByScan", 3)
            ?? throw new Exception("ISpectrum.GetMSSpectrumByScan(3 args) missing");

        var reader = new ShimadzuData
        {
            DataObject = data,
            IoObj = io,
            SpectrumObj = spectrum,
            ScanCount = scanCount,
            MassMultiplier = massMul,
            GetSpectrumInfo = getInfo,
            GetSpectrumByScan = getByScan,
            ParametersObj = parameters,
            SampleInfoObj = Reflect.GetProp(data, "SampleInfo"),
        };
        DecideMassScale(reader);
        return reader;
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
                    Reflect.InvokeCoerced(gat, parameters, args);
                    endTime = Convert.ToInt32(args[1] ?? 0);
                }
            }
            var retTimeToScan = Reflect.Method(spectrum.GetType(), "RetTimeToScan", 3);
            Dbg.Say($"endTime={endTime} chromatogram={(chromatogram != null)} retTimeToScan={(retTimeToScan != null)} parameters={(parameters != null)}");

            if (chromatogram != null && retTimeToScan != null && endTime > 0)
            {
                int segCount = Convert.ToInt32(Reflect.GetProp(chromatogram, "SegmentCount") ?? 0);
                var eventCountM = Reflect.Method(chromatogram.GetType(), "EventCount", 1);
                var getEventNoM = Reflect.Method(chromatogram.GetType(), "GetEventNo", 2);
                for (int seg = 1; seg <= segCount; seg++)
                {
                    int evCount = eventCountM != null ? Convert.ToInt32(Reflect.InvokeCoerced(eventCountM, chromatogram, new object?[] { seg })) : 1;
                    for (int ei = 1; ei <= evCount; ei++)
                    {
                        short eventNo = getEventNoM != null
                            ? Convert.ToInt16(Reflect.InvokeCoerced(getEventNoM, chromatogram, new object?[] { seg, ei }))
                            : (short)ei;
                        var args = new object?[] { (uint)0, endTime, eventNo };
                        var st = Reflect.InvokeCoerced(retTimeToScan, spectrum, args);
                        if (Reflect.Ok(st))
                        {
                            int last = Convert.ToInt32(args[0] ?? 0);
                            if (last > lastScan) lastScan = last;
                        }
                    }
                }
            }
        }
        catch (Exception e) { Dbg.Say($"segment/event scan-count path threw: {e.GetType().Name}: {e.Message}"); lastScan = 0; }

        Dbg.Say($"segment/event scan-count path -> lastScan={lastScan}");
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
                    try { st = Reflect.InvokeCoerced(getInfo, spectrum, args); }
                    catch (Exception e)
                    {
                        Dbg.Say($"probe GetMSSpectrumInfo(scan={scan}) threw: " +
                                $"{(e.InnerException ?? e).GetType().Name}: {(e.InnerException ?? e).Message}");
                        break;
                    }
                    if (!Reflect.Ok(st)) { Dbg.Say($"probe GetMSSpectrumInfo(scan={scan}) status={st}"); break; }
                }
                lastScan = scan - 1;
                Dbg.Say($"probe fallback -> lastScan={lastScan}");
            }
            else Dbg.Say("probe fallback: GetMSSpectrumInfo(8 args) not found");
        }
        catch (Exception e) { Dbg.Say($"probe fallback threw: {e.GetType().Name}: {e.Message}"); }
        return lastScan;
    }

    // --- per-spectrum metadata + data --------------------------------------------------------

    internal static ShimadzuSpectrumMeta Meta(ShimadzuData d, int scan)
    {
        var m = new ShimadzuSpectrumMeta { ScanNumber = scan, MsLevel = 1, Polarity = 2, SignalContinuity = 0 };
        var args = new object?[] { scan, 0, 0, 0, 0, null, 0, (short)0 };
        var st = Reflect.InvokeCoerced(d.GetSpectrumInfo, d.SpectrumObj, args);
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

    /// <summary>V1 metadata plus the precursor/acquisition scalars, which live on the decoded
    /// `MassSpectrumObject` rather than on `GetSpectrumInfo`'s out-params. Vendor units: `AcqModeMz`
    /// and `QTransmissionWidthMz` are fixed-point (1e4 and 1e1 respectively), matching what the
    /// LabSolutions mzML records as isolation target 452.5 and a 17.0 Th window.</summary>
    internal static ShimadzuSpectrumMetaV2 MetaV2(ShimadzuData d, int scan)
    {
        var v1 = Meta(d, scan);
        var m = new ShimadzuSpectrumMetaV2
        {
            ScanNumber = v1.ScanNumber,
            MsLevel = v1.MsLevel,
            Polarity = v1.Polarity,
            SignalContinuity = v1.SignalContinuity,
            PrecursorCharge = v1.PrecursorCharge,
            RetentionTimeSeconds = v1.RetentionTimeSeconds,
            PrecursorMz = v1.PrecursorMz,
            NPoints = v1.NPoints,
        };
        object spec;
        try { spec = SpecFor(d, scan, Exp.ProfileDesired); }
        catch (Exception e) { Dbg.Say($"MetaV2 scan {scan}: no spectrum object ({e.Message})"); return m; }

        double AsDouble(string name, double scale)
        {
            var v = Reflect.GetProp(spec, name);
            if (v == null) return 0.0;
            try { return Convert.ToDouble(v, CultureInfo.InvariantCulture) * scale; }
            catch { return 0.0; }
        }
        long AsLong(string name)
        {
            var v = Reflect.GetProp(spec, name);
            if (v == null) return 0L;
            try { return Convert.ToInt64(v, CultureInfo.InvariantCulture); }
            catch { return 0L; }
        }

        m.IsolationTargetMz = AsDouble("AcqModeMz", d.MassMultiplier);
        m.IsolationWidthMz = AsDouble("QTransmissionWidthMz", 1e-1);
        m.CollisionEnergy = AsDouble("CollisionEnergy", 1.0);
        // The precursor m/z, from the vendor's own selection record and on the SAME fixed-point
        // scale as the m/z axis (`Mass` 1002162 -> 100.2162), which is proven bit-exact against the
        // LabSolutions export. Do NOT use the scalar `GetSpectrumInfo` returns in `args[3]`: on
        // HEK_PosOAD1 scan 18 that is 2241279, which is exactly the spectrum's own `BPMass` (base
        // peak), not its precursor — msconvert reads the same field and publishes it as an m/z of
        // 2.24e07. `AcqModeMz` is preferred where the vendor sets it because DIA validates it
        // directly (4525000 -> 452.5, matching the mzML lane's isolation target); OAD/DDA runs
        // leave it 0 and carry the selection in `PrecursorMzList` instead.
        m.PrecursorMz = 0.0; // discard the V1 value unconditionally — see above, it is BPMass
        if (m.IsolationTargetMz > 0.0)
        {
            m.PrecursorMz = m.IsolationTargetMz;
        }
        else if (Reflect.GetProp(spec, "PrecursorMzList") is IList pl && pl.Count > 0)
        {
            try
            {
                double first = Convert.ToDouble(pl[0], CultureInfo.InvariantCulture) * d.MassMultiplier;
                if (first > 0.0) m.PrecursorMz = first;
            }
            catch { /* leave 0: better no precursor than an invented one */ }
        }
        else
        {
            m.PrecursorMz = 0.0;
        }
        m.PrecursorScanNumber = AsLong("PrecursorScanNo");
        m.SegmentNo = (int)AsLong("SegmentNo");
        m.EventNo = (int)AsLong("EventNo");
        if (m.PrecursorCharge == 0) m.PrecursorCharge = (int)AsLong("PrecursorChargeState");
        // MS1 spectra carry an AcqModeMz too (the acquisition range centre); an isolation target is
        // only meaningful for MSn, and emitting one for MS1 would invent a precursor.
        if (m.MsLevel < 2)
        {
            m.IsolationTargetMz = 0.0;
            m.IsolationWidthMz = 0.0;
            m.CollisionEnergy = 0.0;
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

    /// <summary>Return BOTH representations for a scan: (profile, centroid), either possibly empty.
    ///
    /// It used to return one or the other -- profile preferred, centroid as fallback -- and drop a
    /// `bool centroid` on the floor at the ABI boundary while Meta() hardcoded "profile". So centroid
    /// data was written into the profile facet and labelled profile. mzPeak carries both
    /// representations for one spectrum by design (spectra_data + spectra_peaks), so return both and
    /// let the caller decide.
    /// <para>Original note: prefers profile; falls back to centroid when
    /// the profile list is empty (SpectrumList_Shimadzu does the same).</summary>
    internal static ((double[] mz, float[] intensity) profile, (double[] mz, float[] intensity) centroid) Data(ShimadzuData d, int scan)
    {
        // The Rust side asks for `which = 0` then `which = 1` on the SAME scan, so without a memo
        // every spectrum costs two GetMSSpectrumByScan round-trips into the vendor assembly. A
        // one-entry cache halves that. Keyed by scan; the reader is single-threaded per handle
        // (`_not_thread_safe` in src/shimadzu.rs) so no locking is needed.
        // Bypassed whenever a Stage-B lever is set: an experiment that re-reads the same scan must
        // reach the vendor, not this cache, or it reports "unchanged" whatever the vendor did.
        if (!Exp.Active && d.CachedScan == scan && d.CachedData != null) return d.CachedData.Value;

        var empty = (new double[0], new float[0]);
        ((double[] mz, float[] intensity) profile, (double[] mz, float[] intensity) centroid) both;

        if (Exp.Fetch == "split")
        {
            // Two INDEPENDENT vendor calls: profile from a profileDesired=true decode, centroid from
            // a profileDesired=false one. Tests whether the centroid product depends on the flag.
            var pSpec = FetchSpectrum(d, scan, true);
            var pList = Reflect.GetProp(pSpec, "ProfileList") as IList;
            var pArrays = ToArrays(pList, d);
            var cSpec = FetchSpectrum(d, scan, false);
            var cList = Reflect.GetProp(cSpec, "CentroidList") as IList;
            both = (pArrays, ToArrays(cList, d));
            Exp.Dump(scan, cSpec, pList, cList);
        }
        else
        {
            var specObj = SpecFor(d, scan, Exp.ProfileDesired);
            IList? profile = null, centroidList = null;
            switch (Exp.Fetch)
            {
                case "centroid-only":
                    // Never touch ProfileList at all — isolates "was the centroid list disturbed by
                    // materialising the profile one first?" from the flag itself.
                    centroidList = Reflect.GetProp(specObj, "CentroidList") as IList;
                    both = (empty, ToArrays(centroidList, d));
                    break;
                case "centroid-first":
                    // Same single decode as legacy, opposite order, and the centroid array is COPIED
                    // OUT before ProfileList is even looked up.
                    centroidList = Reflect.GetProp(specObj, "CentroidList") as IList;
                    var centroidArrays = ToArrays(centroidList, d);
                    profile = Reflect.GetProp(specObj, "ProfileList") as IList;
                    both = (ToArrays(profile, d), centroidArrays);
                    break;
                default: // legacy
                    profile = Reflect.GetProp(specObj, "ProfileList") as IList;
                    centroidList = Reflect.GetProp(specObj, "CentroidList") as IList;
                    if (Dbg.On)
                    {
                        // GUARDED: this interpolation calls the vendor `Count` getters, so building it
                        // unconditionally added a vendor interaction to every scan of every run.
                        Dbg.Say($"scan {scan}: ProfileList={(profile == null ? "null" : profile.Count.ToString())} " +
                                $"CentroidList={(centroidList == null ? "null" : centroidList.Count.ToString())}");
                    }
                    both = (ToArrays(profile, d), ToArrays(centroidList, d));
                    break;
            }
            // AFTER the arrays are copied: these getters may mutate vendor state themselves.
            Exp.Dump(scan, specObj, profile, centroidList);
        }

        d.CachedScan = scan;
        d.CachedData = both;
        return both;
    }

    /// <summary>The decoded spectrum object for `scan`, memoised so that a meta call and a data
    /// call on the same scan share one decode. Bypassed whenever a Stage-B lever is set.</summary>
    internal static object SpecFor(ShimadzuData d, int scan, bool profileDesired)
    {
        if (!Exp.Active && d.CachedSpecScan == scan && d.CachedSpecObj != null) return d.CachedSpecObj;
        var o = FetchSpectrum(d, scan, profileDesired);
        if (!Exp.Active) { d.CachedSpecScan = scan; d.CachedSpecObj = o; }
        return o;
    }

    /// <summary>One `GetMSSpectrumByScan(out spectrum, scan, profileDesired)` round trip.</summary>
    private static object FetchSpectrum(ShimadzuData d, int scan, bool profileDesired)
    {
        var args = new object?[] { null, scan, profileDesired }; // out spectrum, scan, profileDesired
        var st = Reflect.InvokeCoerced(d.GetSpectrumByScan, d.SpectrumObj, args);
        var specObj = Reflect.Ok(st) ? args[0] : null;
        if (specObj == null)
            throw new Exception($"GetMSSpectrumByScan failed for scan {scan} (profileDesired={profileDesired}): {st}");
        return specObj;
    }

    /// Property handles for one vendor point type, resolved once. `ToArrays` used to walk
    /// `GetProperty` twice per point on the hottest loop of the conversion.
    private sealed record PointProps(PropertyInfo Mass, PropertyInfo Intensity, PropertyInfo? MassHigh);
    private static readonly Dictionary<Type, PointProps> PointPropCache = new();

    private static PointProps PropsFor(object pt)
    {
        var t = pt.GetType();
        lock (PointPropCache)
        {
            if (PointPropCache.TryGetValue(t, out var cached)) return cached;
            var pp = new PointProps(
                Reflect.Prop(pt, "Mass") ?? throw new Exception($"{t.FullName} has no Mass"),
                Reflect.Prop(pt, "Intensity") ?? throw new Exception($"{t.FullName} has no Intensity"),
                Reflect.Prop(pt, "MassHigh"));
            PointPropCache[t] = pp;
            return pp;
        }
    }

    /// <summary>Vendor point list -> (m/z, intensity). Empty arrays when the list is absent.
    ///
    /// m/z comes from `MassHigh` (Int64, ~1e-9 Da) when `d.HighMassMul` was established for this
    /// file, else from the coarse `Mass` (Int32, 1e-4 Da lattice — what ProteoWizard reads).
    /// `MassHigh` is what LabSolutions' own mzML exporter writes (verified bit-for-bit on
    /// Blind_P1_pos_012), so it is the vendor's stated coordinate; the converter stores it
    /// rather than the truncation. A whole SPECTRUM falls back to `Mass` if any of its points lacks
    /// a usable `MassHigh`, so precision is never mixed inside one array.</summary>
    private static (double[] mz, float[] intensity) ToArrays(IList? list, ShimadzuData d)
    {
        int n = list?.Count ?? 0;
        var mz = new double[n];
        var inten = new float[n];
        if (n == 0) return (mz, inten);
        var pp = PropsFor(list![0]!);
        bool high = d.HighMassMul > 0 && pp.MassHigh != null;
        for (int i = 0; i < n; i++)
        {
            var pt = list[i]!;
            long massInt = System.Convert.ToInt64(pp.Mass.GetValue(pt) ?? 0L);
            double intensity = System.Convert.ToDouble(pp.Intensity.GetValue(pt) ?? 0.0);
            inten[i] = (float)intensity;
            if (high)
            {
                long mh = System.Convert.ToInt64(pp.MassHigh!.GetValue(pt) ?? 0L);
                if (mh <= 0 && massInt > 0) { high = false; i = -1; continue; } // restart on Mass
                mz[i] = mh * d.HighMassMul;
            }
            else
            {
                mz[i] = massInt * d.MassMultiplier;
            }
        }
        return (mz, inten);
    }

    /// <summary>Decide, once per file, whether `MassHigh` can be trusted and at what scale.
    ///
    /// Guards (from the size-analysis review): the scale is a FILE-level constant, never a per-point
    /// ratio (that divides by zero on Mass = 0 and carries a truncation bias); it is snapped to a
    /// power of ten; every sampled point must satisfy |MassHigh − Mass×R| ≤ R/2 (i.e. Mass is the
    /// rounding of MassHigh); ≥ 1000 positive points across the first scans, or the whole file stays
    /// on `Mass`. Measured on three LCMS-9030 runs: R = 100000 exactly, profile points exactly on
    /// the grid, centroids carrying sub-lattice digits (an interpolated apex should).</summary>
    internal static void DecideMassScale(ShimadzuData d)
    {
        d.HighMassMul = 0;
        if (Environment.GetEnvironmentVariable("MZPC_SHIMADZU_COARSE_MZ") == "1") { Dbg.Say("MassHigh disabled by env"); return; }
        var ratios = new List<double>();
        var pairs = new List<(long mass, long high)>();
        int scan = 1;
        try
        {
            while (pairs.Count < 1000 && scan <= 8 && scan <= d.ScanCount)
            {
                var spec = SpecFor(d, scan, Exp.ProfileDesired);
                foreach (var name in new[] { "ProfileList", "CentroidList" })
                {
                    if (Reflect.GetProp(spec, name) is not IList list || list.Count == 0) continue;
                    var pp = PropsFor(list[0]!);
                    if (pp.MassHigh == null) { Dbg.Say($"{list[0]!.GetType().Name} has no MassHigh; using Mass"); return; }
                    long prevHigh = long.MinValue;
                    foreach (var pt in list)
                    {
                        long m = System.Convert.ToInt64(pp.Mass.GetValue(pt) ?? 0L);
                        long h = System.Convert.ToInt64(pp.MassHigh.GetValue(pt) ?? 0L);
                        if (h < prevHigh) { Dbg.Say("MassHigh not non-decreasing; using Mass"); return; }
                        prevHigh = h;
                        if (m > 0 && h > 0) { pairs.Add((m, h)); ratios.Add((double)h / m); }
                    }
                }
                scan++;
            }
        }
        catch (Exception e) { Dbg.Say($"MassHigh probe failed ({e.Message}); using Mass"); return; }
        if (pairs.Count < 1000) { Dbg.Say($"only {pairs.Count} points to establish the MassHigh scale; using Mass"); return; }
        ratios.Sort();
        double median = ratios[ratios.Count / 2];
        double r = Math.Pow(10, Math.Round(Math.Log10(median)));
        if (r < 1e3 || r > 1e9 || Math.Abs(median / r - 1.0) > 1e-3) { Dbg.Say($"MassHigh/Mass median {median} is not a power of ten; using Mass"); return; }
        long R = (long)r;
        int bad = pairs.Count(p => Math.Abs(p.high - p.mass * R) > R / 2);
        if (bad > 0) { Dbg.Say($"{bad}/{pairs.Count} points violate |MassHigh - Mass*R| <= R/2; using Mass"); return; }
        d.HighMassMul = d.MassMultiplier / R;
        Dbg.Say($"MassHigh scale R={R} from {pairs.Count} points; m/z = MassHigh * {d.HighMassMul:G}");
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
        // Fail fast on ABI drift. These check THIS build's own layout only — cross-binary safety
        // comes from `ShimadzuAbiVersion` plus the versioned entry points.
        if (Marshal.SizeOf<ShimadzuSpectrumMeta>() != 48)
            throw new Exception($"ShimadzuSpectrumMeta must be 48 bytes, is {Marshal.SizeOf<ShimadzuSpectrumMeta>()}");
        if (Marshal.SizeOf<ShimadzuSpectrumMetaV2>() != 88)
            throw new Exception($"ShimadzuSpectrumMetaV2 must be 88 bytes, is {Marshal.SizeOf<ShimadzuSpectrumMetaV2>()}");
        // The V1 prefix must stay byte-identical, or `SpectrumMeta` and `SpectrumMetaV2` would
        // disagree about where the shared fields live.
        foreach (var name in new[] { "ScanNumber", "MsLevel", "Polarity", "SignalContinuity",
                                     "PrecursorCharge", "RetentionTimeSeconds", "PrecursorMz", "NPoints" })
        {
            var a = Marshal.OffsetOf<ShimadzuSpectrumMeta>(name);
            var b = Marshal.OffsetOf<ShimadzuSpectrumMetaV2>(name);
            if (a != b) throw new Exception($"ShimadzuSpectrumMetaV2.{name} is at {b}, V1 has it at {a}");
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static unsafe long Open(ushort* pathUtf16, ushort* pwizDirUtf16)
    {
        try
        {
            string path = new string((char*)pathUtf16);
            string pwiz = new string((char*)pwizDirUtf16);
            var d = OpenData(path, pwiz);
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

    /// <summary>ABI generation of this glue build. Resolved OPTIONALLY by the Rust side: a DLL too
    /// old to export it is treated as version 1.
    ///
    /// This exists because nothing else made a version mismatch detectable. Both sides assert their
    /// OWN struct size (48 here, a const assert there) and exports resolve by name, so neither
    /// learns anything about the other: a new binary against a stale DLL would have read
    /// uninitialised tail bytes as metadata, and a stale binary against a new DLL would have taken
    /// a 40-byte out-param overrun. Layout changes never fail on their own — only ADDED exports do,
    /// which is why every layout change gets a new versioned entry point rather than a wider
    /// struct behind the old name.</summary>
    [UnmanagedCallersOnly(EntryPoint = "ShimadzuAbiVersion")]
    public static int ShimadzuAbiVersion() => 3;   // 3: + MassRange, InstrumentInfo

    /// <summary>Scan window for one (segment, event): `MS.Parameters.GetMassRawRange`, in raw mass
    /// units (× MassMultiplier, like point masses). Every scan of an event shares it, so the answer
    /// is memoised per pair. Returns 1 with both outputs 0 when the vendor has no range.</summary>
    [UnmanagedCallersOnly(EntryPoint = "MassRange")]
    public static unsafe int MassRange(long handle, int segmentNo, int eventNo, double* loOut, double* hiOut)
    {
        *loOut = 0; *hiOut = 0;
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 1; }
            if (d.RangeCache.TryGetValue((segmentNo, eventNo), out var hit)) { *loOut = hit.lo; *hiOut = hit.hi; return 0; }
            if (d.ParametersObj == null) { _lastError = "MS.Parameters unavailable"; return 1; }
            var m = Reflect.Method(d.ParametersObj.GetType(), "GetMassRawRange", 4);
            if (m == null) { _lastError = "GetMassRawRange(4 args) missing"; return 1; }
            var args = new object?[] { 0, 0, (short)segmentNo, (short)eventNo }; // ref start, ref end, seg, event
            var st = Reflect.InvokeCoerced(m, d.ParametersObj, args);
            if (!Reflect.Ok(st)) { _lastError = $"GetMassRawRange({segmentNo},{eventNo}): {st}"; return 1; }
            double lo = Convert.ToInt64(args[0] ?? 0L) * d.MassMultiplier;
            double hi = Convert.ToInt64(args[1] ?? 0L) * d.MassMultiplier;
            d.RangeCache[(segmentNo, eventNo)] = (lo, hi);
            *loOut = lo; *hiOut = hi;
            return 0;
        }
        catch (Exception e) { _lastError = e.ToString(); return 1; }
    }

    /// <summary>Instrument identity + run start, as one UTF-16 string of '\u001F'-separated fields:
    /// `systemName ␟ deviceId ␟ analysisDateIso8601 ␟ ionization`. Same buffer convention as
    /// `LastError` (returns the length needed; fills up to `cap`). Fields the vendor does not expose
    /// are empty, never invented — the Rust side asserts only what is present.</summary>
    [UnmanagedCallersOnly(EntryPoint = "InstrumentInfo")]
    public static unsafe int InstrumentInfo(long handle, ushort* buf, int cap)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 0; }

            string system = "";
            try
            {
                var sn = Reflect.Method(d.IoObj.GetType(), "SystemName", 0);
                system = (sn?.Invoke(d.IoObj, null) as string ?? "").Trim();
            }
            catch (Exception e) { Dbg.Say($"SystemName: {e.Message}"); }

            string device = "";
            try { device = Reflect.GetProp(d.ParametersObj ?? new object(), "DeviceID")?.ToString() ?? ""; }
            catch (Exception e) { Dbg.Say($"DeviceID: {e.Message}"); }

            string date = "";
            try
            {
                var v = d.SampleInfoObj == null ? null : Reflect.GetProp(d.SampleInfoObj, "AnalysisDate");
                if (v is DateTime dt && dt.Year > 1900)
                    date = dt.ToString("yyyy-MM-dd'T'HH:mm:ss", CultureInfo.InvariantCulture);
            }
            catch (Exception e) { Dbg.Say($"AnalysisDate: {e.Message}"); }

            string ionization = "";
            try
            {
                // On the decoded spectrum object (IfKind); one memoised decode of scan 1.
                var spec = SpecFor(d, 1, Exp.ProfileDesired);
                ionization = Reflect.GetProp(spec, "IfKind")?.ToString() ?? "";
            }
            catch (Exception e) { Dbg.Say($"IfKind: {e.Message}"); }

            var msg = string.Join("\u001F", new[] { system, device, date, ionization });
            if (buf != null && cap > 0)
            {
                int n = Math.Min(cap, msg.Length);
                for (int i = 0; i < n; i++) buf[i] = msg[i];
            }
            return msg.Length;
        }
        catch (Exception e) { _lastError = e.ToString(); return 0; }
    }

    /// <summary>V2 metadata. A separate entry point from `SpectrumMeta` ON PURPOSE: an old binary
    /// resolving `SpectrumMeta` still gets exactly 48 bytes written into its 48-byte buffer, and a
    /// new binary against an old DLL fails to resolve THIS name and errors at load.</summary>
    [UnmanagedCallersOnly(EntryPoint = "SpectrumMetaV2")]
    public static unsafe int SpectrumMetaV2(long handle, long index, ShimadzuSpectrumMetaV2* outMeta)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 1; }
            *outMeta = MetaV2(d, (int)index + 1); // reader index 0-based -> 1-based scan
            return 0;
        }
        catch (Exception e) { _lastError = e.ToString(); return 1; }
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

    /// <summary>`which`: 0 = profile, 1 = centroid. Both are available for the same scan; the caller
    /// asks for each separately so an archive can carry both facets.</summary>
    [UnmanagedCallersOnly(EntryPoint = "SpectrumData")]
    public static unsafe int SpectrumData(long handle, long index, int which, double** mzOut, float** intOut, long* nOut)
    {
        try
        {
            ShimadzuData? d;
            lock (Gate) { Readers.TryGetValue(handle, out d); }
            if (d == null) { _lastError = "unknown handle"; return 1; }
            int scan = (int)index + 1;
            var both = Data(d, scan);
            var (mz, inten) = which == 1 ? both.centroid : both.profile;
            // Boundary contract: Rust reads both arrays with ONE length. A mismatch here would make
            // it walk off the end of the shorter one, so refuse rather than hand over the pair.
            if (mz.Length != inten.Length)
            {
                _lastError = $"scan {scan} which={which}: m/z has {mz.Length} values, intensity has {inten.Length}";
                return 1;
            }
            // Nothing to hand over, and nothing to pin. Pinning empty arrays anyway would key every
            // such fetch on (handle, null) — the second one overwrote the first entry in `Pins`, so
            // its GCHandles could never be freed.
            if (mz.Length == 0)
            {
                *mzOut = null;
                *intOut = null;
                *nOut = 0;
                return 0;
            }
            var mzH = GCHandle.Alloc(mz, GCHandleType.Pinned);
            var inH = GCHandle.Alloc(inten, GCHandleType.Pinned);
            var mzP = (double*)mzH.AddrOfPinnedObject();
            var inP = (float*)inH.AddrOfPinnedObject();
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
