// SciexGlue/Glue.cs
//
// Thin C# shim that reads SciEX .wiff/.wiff2 files via the vendor Clearcore2 .NET API and
// exposes a tiny C ABI (matching src/sciex.rs) of [UnmanagedCallersOnly] static methods to
// the Rust host (which boots CoreCLR via netcorehost).
//
// ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. This compiles on any platform (no compile-time
//    reference to Clearcore2 — everything vendor-specific is reached through reflection at
//    runtime), but it only *runs* where the Clearcore2 DLLs (sourced from a ProteoWizard
//    install's vendor_api/ABI directory) and a compatible .NET 8 runtime are present.
//
// WHY REFLECTION: the Clearcore2 assemblies are proprietary, redistributed only inside
// ProteoWizard, and not available on the build host. A compile-time <Reference> would make
// the build fail without them. Instead we Assembly.LoadFrom() them at Open() time from the
// caller-supplied pwiz directory and invoke members late-bound. This mirrors how
// ProteoWizard's own C++/CLI bridge (pwiz_aux/.../vendor_api/ABI/WiffFile.cpp) drives the
// same classes — see the method/property names referenced below.
//
// CLEARCORE2 API SHAPE (from WiffFile.cpp), all reached by reflection here:
//   Clearcore2.Data.DataAccess.SampleData.AnalystDataProviderFactory
//       static IAnalystDataProvider CreateDataProvider(string, bool)
//       static Batch CreateBatch(string wiffPath, IAnalystDataProvider provider)
//   Batch:
//       string[] GetSampleNames()
//       Sample  GetSample(int sampleIndex0)
//   Sample:
//       MassSpectrometerSample MassSpectrometerSample { get; }
//   MassSpectrometerSample:
//       int ExperimentCount { get; }
//       MSExperiment GetMSExperiment(int experimentIndex0)
//   MSExperiment:
//       ExperimentDetails Details { get; }   // .NumberOfScans, .Polarity, .ExperimentType
//       MassSpectrum GetMassSpectrum(int cycleIndex0)
//       MassSpectrumInfo GetMassSpectrumInfo(int cycleIndex0)   // .MSLevel, .StartRT (varies by version)
//       double GetRTFromExperimentScanIndex(int cycleIndex0)
//   MassSpectrumInfo (the precursor, as WiffFile.cpp's SpectrumImpl ctor reads it):
//       bool IsProductSpectrum, double ParentMZ, int ParentChargeState
//   ExperimentDetails, on a Product / Precursor experiment (WiffFile.cpp getIsolationInfo):
//       MassRangeInfo[0]      // a FragmentBasedScanMassRange: double IsolationWindow (full width)
//       Parameters["CE"]      // .Start / .Stop in eV, negative on a negative-polarity method
//   MassSpectrum:
//       double[] GetActualXValues()   // m/z
//       double[] GetActualYValues()   // intensity
//       int NumDataPoints { get; }
//
// Because exact member names/casing drift between Clearcore2 releases, every lookup below is
// tolerant: it tries a list of candidate names and falls back gracefully. This is best-effort
// glue, not a hardened production reader.

using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;

namespace SciexGlue;

/// <summary>
/// The flattened address of one spectrum: (sample, experiment, cycle), all 1-based, plus the
/// resolved metadata we surface to Rust.
/// </summary>
internal readonly struct SpectrumAddress
{
    public readonly int Sample;       // 1-based
    public readonly int Experiment;   // 1-based
    public readonly int Cycle;        // 1-based

    public SpectrumAddress(int sample, int experiment, int cycle)
    {
        Sample = sample;
        Experiment = experiment;
        Cycle = cycle;
    }
}

/// <summary>
/// Per-spectrum scalar metadata. Layout MUST match the Rust <c>SciexSpectrumMeta</c> #[repr(C)]
/// struct in src/sciex.rs (field order + types).
///
/// ABI CONTRACT: 6 × int32 (4B each) + 1 × double (8B). With Sequential layout and natural
/// alignment the double sits at offset 24 (already 8-aligned), so the struct is exactly 32
/// bytes with 8-byte alignment — matching the Rust <c>#[repr(C)]</c> mirror. The static check
/// in <see cref="Exports"/> fails loudly if a future edit drifts the layout.
///
/// RT UNIT CONTRACT: <c>RetentionTimeSeconds</c> carries retention time in SECONDS across the
/// ABI. The managed side multiplies Clearcore2's minutes by 60 to produce it; the Rust side
/// divides by 60 to recover minutes for mzdata. Both halves are intentionally symmetric — do
/// not change one without the other.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct SciexSpectrumMeta
{
    public int Sample;
    public int Experiment;
    public int Cycle;
    public int MsLevel;            // 1-based
    public int Polarity;           // 0 = positive, 1 = negative, other = unknown
    public int SignalContinuity;   // 0 = profile, 1 = centroid
    public double RetentionTimeSeconds; // seconds at the ABI (see RT UNIT CONTRACT above)
}

/// <summary>
/// V2 metadata: <see cref="SciexSpectrumMeta"/> VERBATIM as a prefix, plus what Clearcore2 states about
/// the precursor, read where ProteoWizard's WiffFile.cpp reads it. Zero means "not stated"; the
/// precursor is DECIDED on the Rust side (src/sciex_run.rs <c>precursor</c>). Never reorder or resize
/// the prefix: a binary that predates the handshake still reaches V1 through <c>SpectrumMeta</c>.
///
/// ABI CONTRACT: the 32 B prefix + 2 × int32 + 4 × double = 72 bytes, 8-byte aligned, matching
/// <c>SciexSpectrumMetaV2</c> in src/sciex.rs (asserted in <see cref="Exports"/>'s static ctor).
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct SciexSpectrumMetaV2
{
    // --- V1 prefix, byte-for-byte (32 B) ---
    public int Sample;
    public int Experiment;
    public int Cycle;
    public int MsLevel;
    public int Polarity;
    public int SignalContinuity;
    public double RetentionTimeSeconds;
    // --- V2 additions (40 B) ---
    public int ExperimentType;          // Details.ExperimentType value: MS 0, Product 1, Precursor 2, NeutralGainOrLoss 3, SIM 4, MRM 5; -1 unreadable
    public int PrecursorCharge;         // MassSpectrumInfo.ParentChargeState of a product spectrum; 0 = not stated
    public double ParentMz;             // MassSpectrumInfo.ParentMZ when IsProductSpectrum; 0 = not stated
    public double IsolationWidth;       // MassRangeInfo[0].IsolationWindow, full width; 0 = not stated
    public double CollisionEnergyStart; // Details.Parameters["CE"].Start, eV as stored; 0 = not stated
    public double CollisionEnergyStop;  // Details.Parameters["CE"].Stop, eV as stored; 0 = not stated
}

/// <summary>
/// What <c>SpectrumDataV2</c> changed in one spectrum's arrays on their way out: intensity points
/// mapped from NaN to 0, points clamped to ±float.MaxValue (±Inf included), and points dropped by
/// cutting an m/z / intensity pair of unequal length to the shorter one. Layout MUST match
/// <c>SciexValueChanges</c> in src/sciex.rs: 3 × int64 = 24 bytes.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct SciexValueChanges
{
    public long NanToZero;
    public long ClampedToF32;
    public long TruncatedPoints;
}

/// <summary>
/// All managed state for one opened WIFF: the reflected provider/batch plus a flattened index
/// of every (sample, experiment, cycle) spectrum so Rust can address them by a single i64.
/// </summary>
internal sealed class WiffSession : IDisposable
{
    public object Provider = null!;          // IAnalystDataProvider
    public object Batch = null!;             // Batch
    public List<SpectrumAddress> Index = new();

    // Run-level facts gathered while the index is built (RunInfo / RunString exports).
    public int SampleCount;
    public int UnreadableSamples;
    public int DwellExperiments;             // ExperimentType MRM / SIM: transition dwells, not spectra
    public int ScanExperiments;
    public int TotalExperiments;
    public string ExperimentTypes = string.Empty;   // distinct type names, ';'-joined
    public string InstrumentName = string.Empty;
    public string InstrumentSerial = string.Empty;
    public string SoftwareVersion = string.Empty;
    public string AcquisitionDateTime = string.Empty; // ISO 8601 round-trip form + "|" + DateTimeKind
    public string SampleNames = string.Empty;         // '\u001F'-joined, in sample order
    public string Resolved = string.Empty;            // which Details members answered (diagnostic)

    public void Dispose()
    {
        // Clearcore2 providers implement IDisposable; release best-effort.
        TryDispose(Batch);
        TryDispose(Provider);
        Batch = null!;
        Provider = null!;
        Index.Clear();
    }

    private static void TryDispose(object? o)
    {
        if (o is IDisposable d)
        {
            try { d.Dispose(); } catch { /* best-effort */ }
        }
    }
}

/// <summary>
/// Reflection-based loader + accessor for the Clearcore2 assemblies. Loaded once per process
/// from the caller-supplied pwiz directory; member lookups are cached.
/// </summary>
internal sealed class Clearcore2Api
{
    private static Clearcore2Api? _instance;
    private static readonly object _gate = new();

    private readonly Type _factoryType;
    private readonly Type _providerType;
    private readonly MethodInfo _createBatch;

    private Clearcore2Api(Type factoryType, Type providerType, MethodInfo createBatch)
    {
        _factoryType = factoryType;
        _providerType = providerType;
        _createBatch = createBatch;
    }

    /// <summary>
    /// Load (once) all Clearcore2*.dll out of <paramref name="pwizDir"/> and resolve the
    /// factory entry points. Registers an AssemblyResolve handler so transitive Clearcore2
    /// dependencies resolve out of the same directory.
    /// </summary>
    public static Clearcore2Api Load(string pwizDir)
    {
        lock (_gate)
        {
            if (_instance != null)
            {
                return _instance;
            }

            if (!Directory.Exists(pwizDir))
            {
                throw new DirectoryNotFoundException(
                    $"Clearcore2 directory not found: {pwizDir}");
            }

            // Resolve any not-yet-loaded Clearcore2 dependency out of pwizDir.
            AppDomain.CurrentDomain.AssemblyResolve += (_, args) =>
            {
                var simpleName = new AssemblyName(args.Name).Name;
                if (simpleName == null)
                {
                    return null;
                }
                var candidate = Path.Combine(pwizDir, simpleName + ".dll");
                return File.Exists(candidate) ? Assembly.LoadFrom(candidate) : null;
            };

            // Eagerly load every Clearcore2*.dll so reflection can see all types.
            foreach (var dll in Directory.GetFiles(pwizDir, "Clearcore2*.dll"))
            {
                try { Assembly.LoadFrom(dll); } catch { /* skip unloadable */ }
            }

            // Real Clearcore2 (Analyst/SCIEX) API (verified by reflection against the pwiz Clearcore2
            // build): the factory lives in `Clearcore2.Data.AnalystDataProvider` (NOT the assumed
            // `...DataAccess.SampleData`), there is NO `CreateDataProvider`, and the WIFF provider is
            // constructed directly: `new AnalystWiffDataProvider()` → `CreateBatch(path, provider)`.
            var factoryType =
                FindType("Clearcore2.Data.AnalystDataProvider.AnalystDataProviderFactory")
                ?? throw new TypeLoadException(
                    "AnalystDataProviderFactory not found among the loaded Clearcore2 assemblies");

            var providerType =
                FindType("Clearcore2.Data.AnalystDataProvider.AnalystWiffDataProvider")
                ?? throw new TypeLoadException(
                    "AnalystWiffDataProvider not found among the loaded Clearcore2 assemblies");

            var createBatch =
                FindStaticMethod(factoryType, "CreateBatch")
                ?? throw new MissingMethodException(
                    "AnalystDataProviderFactory.CreateBatch not found");

            _instance = new Clearcore2Api(factoryType, providerType, createBatch);
            return _instance;
        }
    }

    /// <summary>Open a WIFF and build the flattened spectrum index.</summary>
    public WiffSession Open(string wiffPath)
    {
        // provider = new AnalystWiffDataProvider()  (parameterless ctor; the real Clearcore2 API has
        // no factory CreateDataProvider — you construct the provider and pass it to CreateBatch).
        var provider = Activator.CreateInstance(_providerType)
            ?? throw new InvalidOperationException("AnalystWiffDataProvider construction returned null");
        // batch = CreateBatch(wiffPath, provider)
        var batch = _createBatch.Invoke(null, new object?[] { wiffPath, provider })
            ?? throw new InvalidOperationException("CreateBatch returned null");

        var session = new WiffSession { Provider = provider, Batch = batch };
        BuildIndex(session);
        return session;
    }

    /// <summary>
    /// Walk every sample → experiment → cycle and record a flattened address per spectrum.
    /// </summary>
    private void BuildIndex(WiffSession session)
    {
        var sampleNames = (Array?)Invoke(session.Batch, "GetSampleNames");
        int sampleCount = sampleNames?.Length ?? 0;
        session.SampleCount = sampleCount;
        var names = new List<string>();
        if (sampleNames != null)
        {
            foreach (var n in sampleNames) { names.Add(n?.ToString() ?? string.Empty); }
        }
        session.SampleNames = string.Join("\u001F", names);
        var types = new SortedSet<string>(StringComparer.Ordinal);
        bool runStringsDone = false;

        for (int s = 0; s < sampleCount; s++)
        {
            object? sample;
            try
            {
                sample = Invoke(session.Batch, "GetSample", s);
            }
            catch
            {
                // An unreadable sample is COUNTED, never silently dropped: a partial archive must be
                // visible to the caller (RunInfo), which refuses rather than publishing a subset.
                session.UnreadableSamples++;
                continue;
            }
            if (sample == null)
            {
                session.UnreadableSamples++;
                continue;
            }

            var msSample = GetProperty(sample, "MassSpectrometerSample");
            if (msSample == null)
            {
                session.UnreadableSamples++;
                continue;
            }

            if (!runStringsDone)
            {
                ReadRunStrings(session, sample, msSample);
                runStringsDone = true;
            }

            int experimentCount = ToInt(GetProperty(msSample, "ExperimentCount"));
            for (int e = 0; e < experimentCount; e++)
            {
                object? experiment;
                try
                {
                    experiment = Invoke(msSample, "GetMSExperiment", e);
                }
                catch
                {
                    session.UnreadableSamples++; // an unreadable experiment is a partial sample
                    continue;
                }
                if (experiment == null)
                {
                    continue;
                }

                var details = GetProperty(experiment, "Details");
                session.TotalExperiments++;
                // ExperimentType is an enum (MS, Product, Precursor, NeutralGainOrLoss, SIM, MRM …):
                // classify by NAME, with the pwiz ordinal (SIM 4, MRM 5) only as a fallback for a
                // value that renders as a bare number.
                string typeName = GetProperty(details, "ExperimentType")?.ToString() ?? string.Empty;
                if (!string.IsNullOrEmpty(typeName)) { types.Add(typeName); }
                bool dwell;
                var upper = typeName.ToUpperInvariant();
                if (upper.Contains("MRM") || upper.Contains("SIM"))
                {
                    dwell = true;
                }
                else if (int.TryParse(typeName, out int ordinal))
                {
                    dwell = ordinal == 4 || ordinal == 5;
                }
                else
                {
                    dwell = false;
                }
                if (dwell) { session.DwellExperiments++; } else { session.ScanExperiments++; }

                int cycleCount = ToInt(GetProperty(details, "NumberOfScans"));
                for (int c = 0; c < cycleCount; c++)
                {
                    // store 1-based addresses (ProteoWizard id convention)
                    session.Index.Add(new SpectrumAddress(s + 1, e + 1, c + 1));
                }
            }
        }
        session.ExperimentTypes = string.Join(";", types);
    }

    /// <summary>
    /// Run identity from the first readable sample: instrument, serial, acquisition software
    /// version and acquisition time live on <c>Sample.Details</c> (ProteoWizard's WiffFile.cpp reads
    /// them there). Every read is best-effort by reflection; which member answered is recorded in
    /// <c>Resolved</c> so the first Windows run tells us the real member names.
    /// </summary>
    private static void ReadRunStrings(WiffSession session, object sample, object msSample)
    {
        var resolved = new List<string>();
        string? First(object? target, string tag, params string[] candidates)
        {
            if (target == null) { return null; }
            foreach (var name in candidates)
            {
                object? v;
                try { v = GetProperty(target, name); } catch { continue; }
                if (v == null) { continue; }
                string text = v is DateTime dt
                    ? dt.ToString("o", System.Globalization.CultureInfo.InvariantCulture) + "|" + dt.Kind
                    : v.ToString() ?? string.Empty;
                if (text.Length == 0) { continue; }
                resolved.Add(tag + "=" + name);
                return text;
            }
            return null;
        }
        var details = GetProperty(sample, "Details");
        var msDetails = GetProperty(msSample, "Details");
        session.InstrumentName = First(details, "instrument", "InstrumentName") ?? First(msDetails, "instrument", "InstrumentName") ?? string.Empty;
        session.InstrumentSerial = First(details, "serial", "InstrumentSerialNumber", "SerialNumber") ?? First(msDetails, "serial", "InstrumentSerialNumber", "SerialNumber") ?? string.Empty;
        session.SoftwareVersion = First(details, "software", "SoftwareVersion") ?? First(msDetails, "software", "SoftwareVersion") ?? string.Empty;
        session.AcquisitionDateTime = First(details, "time", "AcquisitionDateTime") ?? First(msDetails, "time", "AcquisitionDateTime") ?? string.Empty;
        session.Resolved = string.Join(";", resolved);
    }

    /// <summary>Resolve scalar metadata for one flattened spectrum, its precursor facts included.</summary>
    public SciexSpectrumMetaV2 GetMeta(WiffSession session, SpectrumAddress addr)
    {
        var (experiment, _) = GetExperimentAndSpectrum(session, addr, fetchSpectrum: false);

        var details = GetProperty(experiment, "Details");

        int msLevel = 1;
        double rtSeconds = 0.0;
        int polarityCode = -1;
        int continuity = 0; // default profile

        // MS level + RT: prefer per-cycle MassSpectrumInfo; fall back to experiment helpers.
        var info = TryInvoke(experiment, "GetMassSpectrumInfo", addr.Cycle - 1);
        if (info != null)
        {
            msLevel = ToInt(GetProperty(info, "MSLevel"), 1);
            rtSeconds = ToDouble(GetProperty(info, "StartRT")) * 60.0; // StartRT is minutes in Clearcore2
            int centroided = ToInt(GetProperty(info, "CentroidMode"), -1);
            if (centroided >= 0)
            {
                continuity = centroided == 0 ? 0 : 1;
            }
        }

        // RT fallback via experiment scan-index → minutes.
        if (rtSeconds == 0.0)
        {
            var rt = TryInvoke(experiment, "GetRTFromExperimentScanIndex", addr.Cycle - 1);
            if (rt != null)
            {
                rtSeconds = ToDouble(rt) * 60.0;
            }
        }

        // Polarity off the experiment details (an enum; map by name/underlying value).
        var polarity = GetProperty(details, "Polarity");
        polarityCode = MapPolarity(polarity);

        // Precursor facts, read where ProteoWizard's WiffFile.cpp reads them and left UNDECIDED (the
        // Rust side builds the precursor): the parent of a product spectrum from its MassSpectrumInfo;
        // the isolation width and collision energy from the experiment, only on a Product or Precursor
        // experiment with a mass range (pwiz's getHasIsolationInfo). ExperimentType travels as the
        // enum's integer value, which pwiz casts straight to its own MS 0 … MRM 5.
        int experimentType = ToInt(GetProperty(details, "ExperimentType"), -1);
        int parentCharge = 0;
        double parentMz = 0.0, isolationWidth = 0.0, ceStart = 0.0, ceStop = 0.0;
        if (info != null && GetProperty(info, "IsProductSpectrum") is bool isProduct && isProduct)
        {
            parentMz = ToDouble(GetProperty(info, "ParentMZ"));
            parentCharge = ToInt(GetProperty(info, "ParentChargeState"));
        }
        if ((experimentType == 1 || experimentType == 2)
            && GetProperty(details, "MassRangeInfo") is Array ranges && ranges.Length > 0)
        {
            isolationWidth = ToDouble(GetProperty(ranges.GetValue(0), "IsolationWindow"));
            var ce = ParameterNamed(GetProperty(details, "Parameters"), "CE");
            ceStart = ToDouble(GetProperty(ce, "Start"));
            ceStop = ToDouble(GetProperty(ce, "Stop"));
        }

        return new SciexSpectrumMetaV2
        {
            Sample = addr.Sample,
            Experiment = addr.Experiment,
            Cycle = addr.Cycle,
            MsLevel = msLevel < 1 ? 1 : msLevel,
            Polarity = polarityCode,
            SignalContinuity = continuity,
            RetentionTimeSeconds = rtSeconds,
            ExperimentType = experimentType,
            PrecursorCharge = parentCharge,
            ParentMz = parentMz,
            IsolationWidth = isolationWidth,
            CollisionEnergyStart = ceStart,
            CollisionEnergyStop = ceStop,
        };
    }

    /// <summary>Fetch the (m/z, intensity) double arrays for one flattened spectrum, and how many
    /// points were dropped to make their lengths agree.</summary>
    public (double[] mz, double[] intensity, long truncated) GetData(WiffSession session, SpectrumAddress addr)
    {
        var (_, spectrum) = GetExperimentAndSpectrum(session, addr, fetchSpectrum: true);
        if (spectrum == null)
        {
            return (Array.Empty<double>(), Array.Empty<double>(), 0);
        }

        var mz = (double[]?)Invoke(spectrum, "GetActualXValues") ?? Array.Empty<double>();
        var intensity = (double[]?)Invoke(spectrum, "GetActualYValues") ?? Array.Empty<double>();

        // Defensive: clamp to the shorter length so we never read past either array. The points cut
        // off are counted, and the archive declares the cut (`sciex:truncate-unequal-arrays`).
        int n = Math.Min(mz.Length, intensity.Length);
        long truncated = Math.Abs((long)mz.Length - intensity.Length);
        if (mz.Length != n) { Array.Resize(ref mz, n); }
        if (intensity.Length != n) { Array.Resize(ref intensity, n); }
        return (mz, intensity, truncated);
    }

    private (object experiment, object? spectrum) GetExperimentAndSpectrum(
        WiffSession session, SpectrumAddress addr, bool fetchSpectrum)
    {
        var sample = Invoke(session.Batch, "GetSample", addr.Sample - 1)
            ?? throw new InvalidOperationException($"GetSample({addr.Sample - 1}) returned null");
        var msSample = GetProperty(sample, "MassSpectrometerSample")
            ?? throw new InvalidOperationException("MassSpectrometerSample is null");
        var experiment = Invoke(msSample, "GetMSExperiment", addr.Experiment - 1)
            ?? throw new InvalidOperationException(
                $"GetMSExperiment({addr.Experiment - 1}) returned null");
        object? spectrum = fetchSpectrum
            ? Invoke(experiment, "GetMassSpectrum", addr.Cycle - 1)
            : null;
        return (experiment, spectrum);
    }

    // --- reflection helpers ------------------------------------------------

    private static Type? FindType(string fullName)
    {
        foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
        {
            var t = asm.GetType(fullName, throwOnError: false);
            if (t != null)
            {
                return t;
            }
        }
        // Fallback: match by simple name across all loaded types (handles namespace drift).
        var simple = fullName.Substring(fullName.LastIndexOf('.') + 1);
        foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
        {
            Type[] types;
            try { types = asm.GetTypes(); } catch { continue; }
            foreach (var t in types)
            {
                if (t.Name == simple)
                {
                    return t;
                }
            }
        }
        return null;
    }

    private static MethodInfo? FindStaticMethod(Type type, string name)
    {
        return type.GetMethod(
            name,
            BindingFlags.Public | BindingFlags.Static | BindingFlags.FlattenHierarchy);
    }

    // Resolve an instance method by name AND arity/argument types. Clearcore2 overloads several
    // members (e.g. GetMassSpectrum has Int32/(Int32,Int32)/Double/(Double,Double) variants), so a
    // bare GetMethod(name) throws AmbiguousMatchException — pick the overload whose parameters accept
    // the supplied args.
    private static MethodInfo? ResolveMethod(Type t, string method, object?[] args)
    {
        MethodInfo? fallback = null;
        foreach (var m in t.GetMethods(
                     BindingFlags.Public | BindingFlags.Instance | BindingFlags.FlattenHierarchy))
        {
            if (m.Name != method)
            {
                continue;
            }
            var ps = m.GetParameters();
            if (ps.Length != args.Length)
            {
                continue;
            }
            bool ok = true;
            for (int i = 0; i < ps.Length; i++)
            {
                if (args[i] != null && !ps[i].ParameterType.IsAssignableFrom(args[i]!.GetType()))
                {
                    ok = false;
                    break;
                }
            }
            if (ok)
            {
                return m;
            }
            fallback ??= m; // same name + arity but a type mismatch — last resort
        }
        return fallback;
    }

    private static object? Invoke(object target, string method, params object?[] args)
    {
        var t = target.GetType();
        var m = ResolveMethod(t, method, args)
            ?? throw new MissingMethodException(t.FullName, method);
        return m.Invoke(target, args.Length == 0 ? null : args);
    }

    private static object? TryInvoke(object target, string method, params object?[] args)
    {
        try
        {
            var m = ResolveMethod(target.GetType(), method, args);
            return m?.Invoke(target, args.Length == 0 ? null : args);
        }
        catch
        {
            return null;
        }
    }

    private static object? GetProperty(object? target, string name)
    {
        if (target == null)
        {
            return null;
        }
        var p = target.GetType().GetProperty(
            name, BindingFlags.Public | BindingFlags.Instance | BindingFlags.FlattenHierarchy);
        return p?.GetValue(target);
    }

    /// <summary><c>parameters[key]</c> on an experiment's <c>Details.Parameters</c> (pwiz: <c>ContainsKey</c>,
    /// then the indexer), or null when the key or the collection is absent.</summary>
    private static object? ParameterNamed(object? parameters, string key)
    {
        if (parameters == null)
        {
            return null;
        }
        if (parameters is System.Collections.IDictionary dictionary)
        {
            return dictionary.Contains(key) ? dictionary[key] : null;
        }
        var type = parameters.GetType();
        var args = new object?[] { key };
        if (ResolveMethod(type, "ContainsKey", args)?.Invoke(parameters, args) is not true)
        {
            return null;
        }
        return type.GetProperty("Item", new[] { typeof(string) })?.GetValue(parameters, args);
    }

    private static int ToInt(object? v, int fallback = 0)
    {
        try { return v == null ? fallback : Convert.ToInt32(v); }
        catch { return fallback; }
    }

    private static double ToDouble(object? v, double fallback = 0.0)
    {
        try { return v == null ? fallback : Convert.ToDouble(v); }
        catch { return fallback; }
    }

    /// <summary>Map a Clearcore2 Polarity enum (by name, then underlying int) to 0/1/-1.</summary>
    private static int MapPolarity(object? polarity)
    {
        if (polarity == null)
        {
            return -1;
        }
        var name = polarity.ToString() ?? "";
        if (name.IndexOf("Positive", StringComparison.OrdinalIgnoreCase) >= 0) { return 0; }
        if (name.IndexOf("Negative", StringComparison.OrdinalIgnoreCase) >= 0) { return 1; }
        // Some versions: Positive=0, Negative=1 as the underlying value.
        try
        {
            int code = Convert.ToInt32(polarity);
            return code == 0 ? 0 : (code == 1 ? 1 : -1);
        }
        catch
        {
            return -1;
        }
    }
}

/// <summary>
/// The C ABI surface. Each method is [UnmanagedCallersOnly] and is resolved by the Rust host
/// through netcorehost's get_function_with_unmanaged_callers_only. Signatures MUST match the
/// extern "system" fn types in src/sciex.rs.
///
/// Marshalling contracts:
///   * Strings cross as NUL-terminated UTF-16 (char* == ushort*); we read them with
///     Marshal.PtrToStringUni.
///   * Spectrum data uses the pointer+len+free pattern: SpectrumData pins managed arrays via
///     GCHandle and returns raw pointers; the caller copies and then calls DataFree, which
///     releases the matching pins.
/// </summary>
public static unsafe class Exports
{
    private static readonly object _gate = new();
    private static long _nextHandle = 1;
    private static readonly Dictionary<long, WiffSession> _sessions = new();

    // Pins handed out by SpectrumData. Keyed by (handle, mzPtr, intPtr) so a stale or mismatched
    // (mz, intensity) pair can never be freed by accident — DataFree must present the exact triple
    // that SpectrumData returned. See finding #4.
    private readonly record struct PinKey(long Handle, IntPtr MzPtr, IntPtr IntPtr);

    private static readonly Dictionary<PinKey, (GCHandle mz, GCHandle intensity)> _pins = new();

    // Last error message, for a best-effort diagnostic channel surfaced to Rust via the LastError
    // export below. Every UnmanagedCallersOnly boundary stores the exception text here (via
    // RecordError) rather than letting it escape (which would kill the host process), so a failed
    // Open/SpectrumCount/SpectrumMeta/SpectrumData carries a detail string the Rust side can fetch.
    // See finding #1.
    private static string _lastError = string.Empty;

    private static void RecordError(Exception ex)
    {
        try
        {
            lock (_gate)
            {
                _lastError = ex.ToString();
            }
        }
        catch { /* never let diagnostics throw across the boundary */ }
    }

    // ABI layout assertions: each struct's size, and the V2 prefix at V1's offsets. Drift here would
    // silently corrupt memory on the Rust side, so fail loudly at type init instead. See finding #11.
    // They check THIS build's own layout only; agreement with src/sciex.rs is SciexAbiVersion plus
    // tests/sciex_abi_pin.rs.
    static Exports()
    {
        if (Marshal.SizeOf<SciexSpectrumMeta>() != 32)
        {
            throw new InvalidOperationException(
                $"SciexSpectrumMeta marshals to {Marshal.SizeOf<SciexSpectrumMeta>()} bytes; src/sciex.rs expects 32. " +
                "Field order/types drifted — fix both sides in lockstep.");
        }
        if (Marshal.SizeOf<SciexSpectrumMetaV2>() != 72)
        {
            throw new InvalidOperationException(
                $"SciexSpectrumMetaV2 marshals to {Marshal.SizeOf<SciexSpectrumMetaV2>()} bytes; src/sciex.rs expects 72. " +
                "Field order/types drifted — fix both sides in lockstep.");
        }
        if (Marshal.SizeOf<SciexValueChanges>() != 24)
        {
            throw new InvalidOperationException(
                $"SciexValueChanges marshals to {Marshal.SizeOf<SciexValueChanges>()} bytes; src/sciex.rs expects 24.");
        }
        foreach (var name in new[] { "Sample", "Experiment", "Cycle", "MsLevel", "Polarity", "SignalContinuity", "RetentionTimeSeconds" })
        {
            var v1 = Marshal.OffsetOf<SciexSpectrumMeta>(name);
            var v2 = Marshal.OffsetOf<SciexSpectrumMetaV2>(name);
            if (v1 != v2)
            {
                throw new InvalidOperationException($"SciexSpectrumMetaV2.{name} is at {v2}, SciexSpectrumMeta has it at {v1}");
            }
        }
    }

    // CALLING-CONVENTION CONTRACT (finding #10): these [UnmanagedCallersOnly] exports omit an
    // explicit CallConvs list, so the runtime uses the platform-default convention. The Rust side
    // declares the matching function pointers as `extern "system"`, which resolves to that same
    // default. This project ships x64-only (the Clearcore2 vendor DLLs are x64), where the
    // platform default and Cdecl are ABI-identical, so the two sides are provably consistent.
    // Do NOT add `CallConvs = new[] { typeof(CallConvCdecl) }` here without switching the Rust
    // pointers to `extern "cdecl"` in lockstep.

    // ---- open / close ----

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static long Open(ushort* pathUtf16, ushort* pwizDirUtf16)
    {
        WiffSession? session = null;
        try
        {
            string? wiffPath = Marshal.PtrToStringUni((IntPtr)pathUtf16);
            string? pwizDir = Marshal.PtrToStringUni((IntPtr)pwizDirUtf16);
            if (string.IsNullOrEmpty(wiffPath) || string.IsNullOrEmpty(pwizDir))
            {
                return 0;
            }

            var api = Clearcore2Api.Load(pwizDir);
            session = api.Open(wiffPath);

            lock (_gate)
            {
                long handle = _nextHandle++;
                _sessions[handle] = session;
                // Ownership transferred to _sessions; clear the local so the catch below won't
                // dispose a live session.
                session = null;
                // We keep the api as a static singleton inside Load, so a per-handle reference is
                // unnecessary; re-resolving is a cached no-op.
                return handle;
            }
        }
        catch (Exception ex)
        {
            // Stash the detail for the Rust side to fetch via LastError; return a generic 0 handle.
            // Dispose any partially-built provider/batch state so a failed Open doesn't leak the
            // native Clearcore2 resources it managed to allocate. See finding #9.
            RecordError(ex);
            try { session?.Dispose(); } catch { /* best-effort */ }
            return 0;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "Close")]
    public static void Close(long handle)
    {
        try
        {
            WiffSession? session = null;
            List<(GCHandle mz, GCHandle intensity)> orphanedPins = new();
            lock (_gate)
            {
                if (_sessions.TryGetValue(handle, out session))
                {
                    _sessions.Remove(handle);
                }

                // Release any pins still outstanding for this handle. A well-behaved caller frees
                // each pair via DataFree, but a panic/early-exit on the Rust side could leave some
                // pinned; closing the handle is the backstop. See finding #5.
                var stale = new List<PinKey>();
                foreach (var kv in _pins)
                {
                    if (kv.Key.Handle == handle)
                    {
                        stale.Add(kv.Key);
                        orphanedPins.Add(kv.Value);
                    }
                }
                foreach (var key in stale)
                {
                    _pins.Remove(key);
                }
            }

            foreach (var p in orphanedPins)
            {
                if (p.mz.IsAllocated) { p.mz.Free(); }
                if (p.intensity.IsAllocated) { p.intensity.Free(); }
            }

            session?.Dispose();
        }
        catch (Exception ex)
        {
            // Close has no return code; swallow so nothing escapes the boundary. See finding #1.
            RecordError(ex);
        }
    }

    // ---- counts / metadata ----

    [UnmanagedCallersOnly(EntryPoint = "SpectrumCount")]
    public static long SpectrumCount(long handle)
    {
        try
        {
            var session = Get(handle);
            return session == null ? -1 : session.Index.Count;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return -1;
        }
    }

    // The V1 prefix of SpectrumMetaV2, kept so a binary that predates the handshake still works.
    [UnmanagedCallersOnly(EntryPoint = "SpectrumMeta")]
    public static int SpectrumMeta(long handle, long index, SciexSpectrumMeta* outMeta)
    {
        if (outMeta == null)
        {
            return 1;
        }
        int rc = FillMeta(handle, index, out var m);
        if (rc == 0)
        {
            *outMeta = new SciexSpectrumMeta
            {
                Sample = m.Sample,
                Experiment = m.Experiment,
                Cycle = m.Cycle,
                MsLevel = m.MsLevel,
                Polarity = m.Polarity,
                SignalContinuity = m.SignalContinuity,
                RetentionTimeSeconds = m.RetentionTimeSeconds,
            };
        }
        return rc;
    }

    [UnmanagedCallersOnly(EntryPoint = "SpectrumMetaV2")]
    public static int SpectrumMetaV2(long handle, long index, SciexSpectrumMetaV2* outMeta)
    {
        if (outMeta == null)
        {
            return 1;
        }
        int rc = FillMeta(handle, index, out var m);
        if (rc == 0)
        {
            *outMeta = m;
        }
        return rc;
    }

    // ---- spectrum data (pointer + len + free) ----

    // The V1 export, kept so a binary that predates the handshake still works; it reports no counts.
    [UnmanagedCallersOnly(EntryPoint = "SpectrumData")]
    public static int SpectrumData(
        long handle,
        long index,
        double** outMzPtr,
        float** outIntPtr,
        long* outLen)
    {
        return FillData(handle, index, outMzPtr, outIntPtr, outLen, null);
    }

    // SpectrumData plus what the glue changed in the arrays (SciexValueChanges), so the archive can
    // declare it.
    [UnmanagedCallersOnly(EntryPoint = "SpectrumDataV2")]
    public static int SpectrumDataV2(
        long handle,
        long index,
        double** outMzPtr,
        float** outIntPtr,
        long* outLen,
        SciexValueChanges* outChanges)
    {
        if (outChanges == null)
        {
            return 1;
        }
        return FillData(handle, index, outMzPtr, outIntPtr, outLen, outChanges);
    }

    // Shared by SpectrumData and SpectrumDataV2: an [UnmanagedCallersOnly] method cannot be called from
    // managed code. outChanges is null for the V1 export.
    private static int FillData(
        long handle,
        long index,
        double** outMzPtr,
        float** outIntPtr,
        long* outLen,
        SciexValueChanges* outChanges)
    {
        // Track pins locally so we can free them if anything throws after allocation but before
        // ownership is transferred to _pins. See findings #1 and #2.
        GCHandle mzPin = default;
        GCHandle intPin = default;
        bool ownershipTransferred = false;
        try
        {
            if (outMzPtr == null || outIntPtr == null || outLen == null)
            {
                return 1;
            }
            *outMzPtr = null;
            *outIntPtr = null;
            *outLen = 0;
            if (outChanges != null)
            {
                *outChanges = default;
            }

            var session = Get(handle);
            if (session == null)
            {
                return 2;
            }
            if (index < 0 || index >= session.Index.Count)
            {
                return 3;
            }

            var addr = session.Index[(int)index];
            var api = ApiOrThrow();
            var (mz, intensityDouble, truncated) = api.GetData(session, addr);
            int n = mz.Length;
            var changes = new SciexValueChanges { TruncatedPoints = truncated };

            if (n == 0)
            {
                if (outChanges != null)
                {
                    *outChanges = changes;
                }
                return 0; // empty spectrum: null pointers, zero length
            }

            // Clearcore2 hands us intensities as double; the mzPeak schema wants f32. Narrow
            // here so the pinned buffer we expose is already f32 (matches the Rust ABI type).
            // Guard non-finite / out-of-f32-range values so a corrupt double can't become a NaN
            // or Inf in the output stream: clamp magnitudes beyond float.MaxValue and map any
            // NaN to 0. See finding #6. Each such change is counted so the archive can declare it.
            var intensity = new float[n];
            for (int i = 0; i < n; i++)
            {
                double v = intensityDouble[i];
                if (double.IsNaN(v))
                {
                    intensity[i] = 0f;
                    changes.NanToZero++;
                }
                else if (v > float.MaxValue)
                {
                    intensity[i] = float.MaxValue;
                    changes.ClampedToF32++;
                }
                else if (v < -float.MaxValue)
                {
                    intensity[i] = -float.MaxValue;
                    changes.ClampedToF32++;
                }
                else
                {
                    intensity[i] = (float)v;
                }
            }

            // Pin both arrays so the GC cannot move them while Rust copies. Released in DataFree.
            mzPin = GCHandle.Alloc(mz, GCHandleType.Pinned);
            intPin = GCHandle.Alloc(intensity, GCHandleType.Pinned);
            IntPtr mzAddr = mzPin.AddrOfPinnedObject();
            IntPtr intAddr = intPin.AddrOfPinnedObject();

            lock (_gate)
            {
                // Keyed by the full (handle, mzPtr, intPtr) triple so DataFree must present the
                // exact pair to release it. See finding #4.
                _pins[new PinKey(handle, mzAddr, intAddr)] = (mzPin, intPin);
            }
            ownershipTransferred = true;

            if (outChanges != null)
            {
                *outChanges = changes;
            }
            *outMzPtr = (double*)mzAddr;
            *outIntPtr = (float*)intAddr;
            *outLen = n;
            return 0;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return 4;
        }
        finally
        {
            // If we allocated pins but never handed ownership to _pins (an exception between
            // GCHandle.Alloc and the dictionary insert), free them here so they don't leak.
            if (!ownershipTransferred)
            {
                if (mzPin.IsAllocated) { mzPin.Free(); }
                if (intPin.IsAllocated) { intPin.Free(); }
            }
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "DataFree")]
    public static void DataFree(long handle, double* mzPtr, float* intPtr)
    {
        try
        {
            // A null m/z pointer (empty spectrum) is a no-op. The pin is identified by the exact
            // (handle, mzPtr, intPtr) triple SpectrumData returned, so a stale/mismatched pair
            // cannot free the wrong pins. See finding #4.
            if (mzPtr == null)
            {
                return;
            }
            var key = new PinKey(handle, (IntPtr)mzPtr, (IntPtr)intPtr);
            (GCHandle mz, GCHandle intensity) pins;
            lock (_gate)
            {
                if (!_pins.TryGetValue(key, out pins))
                {
                    return;
                }
                _pins.Remove(key);
            }
            if (pins.mz.IsAllocated) { pins.mz.Free(); }
            if (pins.intensity.IsAllocated) { pins.intensity.Free(); }
        }
        catch (Exception ex)
        {
            // DataFree has no return code; never let an exception escape the boundary. See finding #1.
            RecordError(ex);
        }
    }

    // ---- diagnostics ----

    /// <summary>ABI generation of this glue build. src/sciex.rs (REQUIRED_ABI_VERSION) resolves it
    /// OPTIONALLY — a DLL too old to export it counts as version 1 — and refuses any other version with
    /// a message naming both, because nothing else makes a mismatch visible: exports resolve by name
    /// and each side asserts only its own struct sizes. A layout change therefore gets a new versioned
    /// entry point and a bump here, never a wider struct behind an old name.</summary>
    [UnmanagedCallersOnly(EntryPoint = "SciexAbiVersion")]
    public static int SciexAbiVersion() => 3;   // 3: + SpectrumDataV2 (value changes); 2: + SpectrumMetaV2; 1: no handshake

    /// <summary>
    /// Run-level counts: [sampleCount, unreadableSamples, dwellExperiments, scanExperiments,
    /// totalExperiments]. Returns 0 on success (see RunString for the text fields).
    /// </summary>
    [UnmanagedCallersOnly(EntryPoint = "RunInfo")]
    public static int RunInfo(long handle, int* out5)
    {
        try
        {
            var session = Get(handle);
            if (session == null || out5 == null)
            {
                return 1;
            }
            out5[0] = session.SampleCount;
            out5[1] = session.UnreadableSamples;
            out5[2] = session.DwellExperiments;
            out5[3] = session.ScanExperiments;
            out5[4] = session.TotalExperiments;
            return 0;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return 3;
        }
    }

    /// <summary>
    /// Run-level text by index: 0 experiment types, 1 instrument name, 2 serial, 3 software
    /// version, 4 acquisition time ("o" form + "|" + DateTimeKind), 5 sample names
    /// (U+001F-joined), 6 which Details members resolved. Fill-buffer contract as LastError:
    /// copies up to <c>cap</c> UTF-16 units and ALWAYS returns the full length; -1 on error.
    /// </summary>
    [UnmanagedCallersOnly(EntryPoint = "RunString")]
    public static int RunString(long handle, int which, ushort* buf, int cap)
    {
        try
        {
            var session = Get(handle);
            if (session == null)
            {
                return -1;
            }
            string msg = which switch
            {
                0 => session.ExperimentTypes,
                1 => session.InstrumentName,
                2 => session.InstrumentSerial,
                3 => session.SoftwareVersion,
                4 => session.AcquisitionDateTime,
                5 => session.SampleNames,
                6 => session.Resolved,
                _ => string.Empty,
            };
            int full = msg.Length;
            if (buf == null || cap <= 0)
            {
                return full;
            }
            int toCopy = Math.Min(full, cap);
            for (int i = 0; i < toCopy; i++)
            {
                buf[i] = msg[i];
            }
            if (toCopy < cap)
            {
                buf[toCopy] = 0;
            }
            return full;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return -1;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "LastError")]
    public static int LastError(ushort* buf, int cap)
    {
        // Fill-buffer contract (matches src/sciex.rs SciexLastError): copy the stashed last-error
        // message (UTF-16) into `buf` for up to `cap` code units (NOT NUL-terminated unless room),
        // and ALWAYS return the FULL length in UTF-16 code units so the caller can detect
        // truncation. A null `buf` or `cap <= 0` just reports the needed length. Wrapped in
        // try/catch so it can never throw across the unmanaged boundary (returns 0 on failure).
        try
        {
            string msg;
            lock (_gate)
            {
                msg = _lastError ?? string.Empty;
            }
            int full = msg.Length; // UTF-16 code units
            if (buf == null || cap <= 0)
            {
                return full;
            }
            int toCopy = Math.Min(full, cap);
            for (int i = 0; i < toCopy; i++)
            {
                buf[i] = msg[i];
            }
            // NUL-terminate only if there is room beyond the copied units (caller relies on the
            // returned length, not a terminator, but a terminator is friendly when it fits).
            if (toCopy < cap)
            {
                buf[toCopy] = (ushort)'\0';
            }
            return full;
        }
        catch
        {
            return 0;
        }
    }

    // ---- internals ----

    // Shared by SpectrumMeta and SpectrumMetaV2: an [UnmanagedCallersOnly] method cannot be called from
    // managed code. 0 on success; 1 unknown handle, 2 index out of range, 3 exception (see LastError).
    private static int FillMeta(long handle, long index, out SciexSpectrumMetaV2 meta)
    {
        meta = default;
        try
        {
            var session = Get(handle);
            if (session == null)
            {
                return 1;
            }
            if (index < 0 || index >= session.Index.Count)
            {
                return 2;
            }
            meta = ApiOrThrow().GetMeta(session, session.Index[(int)index]);
            return 0;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return 3;
        }
    }

    private static WiffSession? Get(long handle)
    {
        lock (_gate)
        {
            return _sessions.TryGetValue(handle, out var s) ? s : null;
        }
    }

    private static Clearcore2Api ApiOrThrow()
    {
        // Load() is idempotent + cached once a session exists; we re-fetch the singleton.
        // The pwiz dir was supplied at Open(); after the first successful Open the singleton
        // is set, so this returns the cached instance.
        return Clearcore2Api.Load(SessionPwizDir());
    }

    // We don't retain the pwiz dir per call; the singleton is already initialized after Open,
    // so Load() short-circuits and the argument is only used on the very first call. Returning
    // an empty string here is safe because by the time SpectrumMeta/SpectrumData run, Load()
    // has a cached instance and never touches the directory again.
    private static string SessionPwizDir() => string.Empty;
}
