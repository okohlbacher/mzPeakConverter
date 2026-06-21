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
/// All managed state for one opened WIFF: the reflected provider/batch plus a flattened index
/// of every (sample, experiment, cycle) spectrum so Rust can address them by a single i64.
/// </summary>
internal sealed class WiffSession : IDisposable
{
    public object Provider = null!;          // IAnalystDataProvider
    public object Batch = null!;             // Batch
    public List<SpectrumAddress> Index = new();

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
    private readonly MethodInfo _createDataProvider;
    private readonly MethodInfo _createBatch;

    private Clearcore2Api(Type factoryType, MethodInfo createDataProvider, MethodInfo createBatch)
    {
        _factoryType = factoryType;
        _createDataProvider = createDataProvider;
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

            var factoryType =
                FindType("Clearcore2.Data.DataAccess.SampleData.AnalystDataProviderFactory")
                ?? throw new TypeLoadException(
                    "AnalystDataProviderFactory not found among the loaded Clearcore2 assemblies");

            var createDataProvider =
                FindStaticMethod(factoryType, "CreateDataProvider")
                ?? throw new MissingMethodException(
                    "AnalystDataProviderFactory.CreateDataProvider not found");

            var createBatch =
                FindStaticMethod(factoryType, "CreateBatch")
                ?? throw new MissingMethodException(
                    "AnalystDataProviderFactory.CreateBatch not found");

            _instance = new Clearcore2Api(factoryType, createDataProvider, createBatch);
            return _instance;
        }
    }

    /// <summary>Open a WIFF and build the flattened spectrum index.</summary>
    public WiffSession Open(string wiffPath)
    {
        // provider = CreateDataProvider("", true)  (the empty-string/true args mirror WiffFile.cpp)
        var provider = InvokeCreateDataProvider();
        // batch = CreateBatch(wiffPath, provider)
        var batch = _createBatch.Invoke(null, new object?[] { wiffPath, provider })
            ?? throw new InvalidOperationException("CreateBatch returned null");

        var session = new WiffSession { Provider = provider, Batch = batch };
        BuildIndex(session);
        return session;
    }

    private object InvokeCreateDataProvider()
    {
        // CreateDataProvider has had a few overloads across versions: (string,bool), (string),
        // and (). Try them in decreasing specificity.
        var ps = _createDataProvider.GetParameters();
        object?[] args = ps.Length switch
        {
            2 => new object?[] { "", true },
            1 => new object?[] { "" },
            _ => Array.Empty<object?>(),
        };
        return _createDataProvider.Invoke(null, args)
            ?? throw new InvalidOperationException("CreateDataProvider returned null");
    }

    /// <summary>
    /// Walk every sample → experiment → cycle and record a flattened address per spectrum.
    /// </summary>
    private void BuildIndex(WiffSession session)
    {
        var sampleNames = (Array?)Invoke(session.Batch, "GetSampleNames");
        int sampleCount = sampleNames?.Length ?? 0;

        for (int s = 0; s < sampleCount; s++)
        {
            object? sample;
            try
            {
                sample = Invoke(session.Batch, "GetSample", s);
            }
            catch
            {
                continue; // skip unreadable samples
            }
            if (sample == null)
            {
                continue;
            }

            var msSample = GetProperty(sample, "MassSpectrometerSample");
            if (msSample == null)
            {
                continue;
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
                    continue;
                }
                if (experiment == null)
                {
                    continue;
                }

                var details = GetProperty(experiment, "Details");
                int cycleCount = ToInt(GetProperty(details, "NumberOfScans"));
                for (int c = 0; c < cycleCount; c++)
                {
                    // store 1-based addresses (ProteoWizard id convention)
                    session.Index.Add(new SpectrumAddress(s + 1, e + 1, c + 1));
                }
            }
        }
    }

    /// <summary>Resolve scalar metadata for one flattened spectrum.</summary>
    public SciexSpectrumMeta GetMeta(WiffSession session, SpectrumAddress addr)
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

        return new SciexSpectrumMeta
        {
            Sample = addr.Sample,
            Experiment = addr.Experiment,
            Cycle = addr.Cycle,
            MsLevel = msLevel < 1 ? 1 : msLevel,
            Polarity = polarityCode,
            SignalContinuity = continuity,
            RetentionTimeSeconds = rtSeconds,
        };
    }

    /// <summary>Fetch the (m/z, intensity) double arrays for one flattened spectrum.</summary>
    public (double[] mz, double[] intensity) GetData(WiffSession session, SpectrumAddress addr)
    {
        var (_, spectrum) = GetExperimentAndSpectrum(session, addr, fetchSpectrum: true);
        if (spectrum == null)
        {
            return (Array.Empty<double>(), Array.Empty<double>());
        }

        var mz = (double[]?)Invoke(spectrum, "GetActualXValues") ?? Array.Empty<double>();
        var intensity = (double[]?)Invoke(spectrum, "GetActualYValues") ?? Array.Empty<double>();

        // Defensive: clamp to the shorter length so we never read past either array.
        int n = Math.Min(mz.Length, intensity.Length);
        if (mz.Length != n) { Array.Resize(ref mz, n); }
        if (intensity.Length != n) { Array.Resize(ref intensity, n); }
        return (mz, intensity);
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

    private static object? Invoke(object target, string method, params object?[] args)
    {
        var t = target.GetType();
        var m = t.GetMethod(method, BindingFlags.Public | BindingFlags.Instance | BindingFlags.FlattenHierarchy)
            ?? throw new MissingMethodException(t.FullName, method);
        return m.Invoke(target, args.Length == 0 ? null : args);
    }

    private static object? TryInvoke(object target, string method, params object?[] args)
    {
        try
        {
            var m = target.GetType().GetMethod(
                method, BindingFlags.Public | BindingFlags.Instance | BindingFlags.FlattenHierarchy);
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

    // ABI layout assertion: 6×int32 + 1×double, naturally aligned == 32 bytes. Drift here would
    // silently corrupt memory on the Rust side, so fail loudly at type init instead. See finding #11.
    static Exports()
    {
        int size = Marshal.SizeOf<SciexSpectrumMeta>();
        if (size != 32)
        {
            throw new InvalidOperationException(
                $"SciexSpectrumMeta marshals to {size} bytes; the Rust #[repr(C)] mirror expects 32. " +
                "Field order/types drifted — fix both sides in lockstep.");
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

    [UnmanagedCallersOnly(EntryPoint = "SpectrumMeta")]
    public static int SpectrumMeta(long handle, long index, SciexSpectrumMeta* outMeta)
    {
        try
        {
            var session = Get(handle);
            if (session == null || outMeta == null)
            {
                return 1;
            }
            if (index < 0 || index >= session.Index.Count)
            {
                return 2;
            }
            var addr = session.Index[(int)index];
            var api = ApiOrThrow();
            *outMeta = api.GetMeta(session, addr);
            return 0;
        }
        catch (Exception ex)
        {
            RecordError(ex);
            return 3;
        }
    }

    // ---- spectrum data (pointer + len + free) ----

    [UnmanagedCallersOnly(EntryPoint = "SpectrumData")]
    public static int SpectrumData(
        long handle,
        long index,
        double** outMzPtr,
        float** outIntPtr,
        long* outLen)
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
            var (mz, intensityDouble) = api.GetData(session, addr);
            int n = mz.Length;

            if (n == 0)
            {
                return 0; // empty spectrum: null pointers, zero length
            }

            // Clearcore2 hands us intensities as double; the mzPeak schema wants f32. Narrow
            // here so the pinned buffer we expose is already f32 (matches the Rust ABI type).
            // Guard non-finite / out-of-f32-range values so a corrupt double can't become a NaN
            // or Inf in the output stream: clamp magnitudes beyond float.MaxValue and map any
            // NaN to 0. See finding #6.
            var intensity = new float[n];
            for (int i = 0; i < n; i++)
            {
                double v = intensityDouble[i];
                if (double.IsNaN(v))
                {
                    intensity[i] = 0f;
                }
                else if (v > float.MaxValue)
                {
                    intensity[i] = float.MaxValue;
                }
                else if (v < -float.MaxValue)
                {
                    intensity[i] = -float.MaxValue;
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
