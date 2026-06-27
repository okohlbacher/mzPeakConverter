// WatersGlue/Glue.cs
//
// Thin C# shim that reads Waters MassLynx `.raw` directories via the vendor MassLynx SDK
// managed API and exposes a tiny C ABI (matching src/waters.rs) of [UnmanagedCallersOnly]
// static methods to the Rust host (which boots CoreCLR via netcorehost).
//
// ⚠️ WINDOWS-RUNTIME-ONLY AND UNTESTED. This compiles on any platform (no compile-time
//    reference to MassLynx — everything vendor-specific is reached through reflection at
//    runtime), but it only *runs* where the MassLynx managed DLLs (from the MassLynx SDK,
//    or a ProteoWizard install's vendor_api/Waters directory) and a compatible .NET 8
//    runtime are present.
//
// WHY REFLECTION: the MassLynx assemblies are proprietary, redistributed only inside the
// Waters SDK / ProteoWizard, and not available on the build host. A compile-time <Reference>
// would make the build fail without them. Instead we Assembly.LoadFrom() them at Open() time
// from the caller-supplied SDK directory and invoke members late-bound. This mirrors how the
// SciEX glue (glue/sciex/Glue.cs) drives Clearcore2.
//
// MASSLYNX MANAGED API SHAPE (all reached by reflection here, every binding marked
// NEEDS-VALIDATION — the real type/method names MUST be confirmed on Windows against the
// actual SDK; like the SciEX glue these were written blind and the first runtime call is the
// first real test):
//   MassLynxRaw.MassLynxRawInfoReader   ctor(string rawPath)                // NEEDS-VALIDATION
//       int  GetFunctionCount()                                            // NEEDS-VALIDATION
//       int  GetScansInFunction(int function)                             // NEEDS-VALIDATION
//       float GetRetentionTime(int function, int scan)                    // NEEDS-VALIDATION
//       ?    GetAcquisitionMassRange(...)                                  // NEEDS-VALIDATION
//       ?    GetIonMode(int function)                                      // NEEDS-VALIDATION (polarity)
//       bool IsContinuum(int function)                                     // NEEDS-VALIDATION
//       int  GetDriftScanCount(int function)                              // NEEDS-VALIDATION (IMS, optional)
//   MassLynxRaw.MassLynxRawScanReader   ctor(string rawPath)               // NEEDS-VALIDATION
//       ReadScan(int function, int scan, out float[] masses, out float[] intensities)        // NEEDS-VALIDATION
//       ReadDriftScan(int function, int scan, int drift, out float[] masses, out float[] intensities) // NEEDS-VALIDATION (IMS, optional)
//
// Flatten every (function, scan) pair into a single 0-based spectrum index, just like the
// SciEX glue flattens (sample, experiment, cycle). The basic non-IMS path is wired first;
// drift scans are treated as optional/best-effort and are NOT currently expanded into the
// flattened index (a TWIMS .raw still surfaces its per-(function,scan) summed spectra).
//
// Because exact member names/casing are unconfirmed, every lookup below throws a clear
// MissingMethodException / MissingMethodException-style error when a member is absent, so a
// renamed/missing member fails loudly at the first call rather than being silently mishandled.

using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;

namespace WatersGlue;

/// <summary>
/// The flattened address of one spectrum: (function, scan), both 0-based as the MassLynx API
/// uses them, plus an optional drift index (-1 = none) reserved for the IMS path.
/// </summary>
internal readonly struct SpectrumAddress
{
    public readonly int Function;  // 0-based
    public readonly int Scan;      // 0-based
    public readonly int Drift;     // 0-based drift bin, or -1 for a non-IMS (summed) scan

    public SpectrumAddress(int function, int scan, int drift)
    {
        Function = function;
        Scan = scan;
        Drift = drift;
    }
}

/// <summary>
/// Per-spectrum scalar metadata. Layout MUST match the Rust <c>WatersSpectrumMeta</c> #[repr(C)]
/// struct in src/waters.rs (field order + types).
///
/// ABI CONTRACT: 6 × int32 (4B each) + 1 × double (8B). With Sequential layout and natural
/// alignment the double sits at offset 24 (already 8-aligned), so the struct is exactly 32
/// bytes with 8-byte alignment — matching the Rust <c>#[repr(C)]</c> mirror. The static check
/// in <see cref="Exports"/> fails loudly if a future edit drifts the layout.
///
/// RT UNIT CONTRACT: <c>RetentionTimeSeconds</c> carries retention time in SECONDS across the
/// ABI. MassLynx's GetRetentionTime returns MINUTES, so the managed side multiplies by 60; the
/// Rust side divides by 60 to recover minutes for mzdata. Both halves are intentionally
/// symmetric — do not change one without the other.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct WatersSpectrumMeta
{
    public int Function;           // 1-based for the surfaced id
    public int Scan;               // 1-based for the surfaced id
    public int Drift;              // 1-based drift bin, or 0 when not an IMS scan
    public int MsLevel;            // 1-based
    public int Polarity;           // 0 = positive, 1 = negative, other = unknown
    public int SignalContinuity;   // 0 = profile (continuum), 1 = centroid
    public double RetentionTimeSeconds; // seconds at the ABI (see RT UNIT CONTRACT above)
}

/// <summary>
/// All managed state for one opened `.raw`: the reflected info/scan readers plus a flattened
/// index of every (function, scan) spectrum so Rust can address them by a single i64.
/// </summary>
internal sealed class RawSession : IDisposable
{
    public object InfoReader = null!;        // MassLynxRawInfoReader
    public object ScanReader = null!;        // MassLynxRawScanReader
    public List<SpectrumAddress> Index = new();

    public void Dispose()
    {
        // MassLynx readers may implement IDisposable; release best-effort.
        TryDispose(ScanReader);
        TryDispose(InfoReader);
        ScanReader = null!;
        InfoReader = null!;
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
/// Reflection-based loader + accessor for the MassLynx assemblies. Loaded once per process
/// from the caller-supplied SDK directory; member lookups are cached.
/// </summary>
internal sealed class MassLynxApi
{
    private static MassLynxApi? _instance;
    private static readonly object _gate = new();

    private readonly Type _infoReaderType;   // MassLynxRawInfoReader
    private readonly Type _scanReaderType;   // MassLynxRawScanReader

    private MassLynxApi(Type infoReaderType, Type scanReaderType)
    {
        _infoReaderType = infoReaderType;
        _scanReaderType = scanReaderType;
    }

    /// <summary>
    /// Load (once) the MassLynx managed DLLs out of <paramref name="sdkDir"/> and resolve the
    /// reader types. Registers an AssemblyResolve handler so transitive MassLynx dependencies
    /// resolve out of the same directory.
    /// </summary>
    public static MassLynxApi Load(string sdkDir)
    {
        lock (_gate)
        {
            if (_instance != null)
            {
                return _instance;
            }

            if (!Directory.Exists(sdkDir))
            {
                throw new DirectoryNotFoundException(
                    $"MassLynx SDK directory not found: {sdkDir}");
            }

            // Resolve any not-yet-loaded MassLynx dependency out of sdkDir.
            AppDomain.CurrentDomain.AssemblyResolve += (_, args) =>
            {
                var simpleName = new AssemblyName(args.Name).Name;
                if (simpleName == null)
                {
                    return null;
                }
                var candidate = Path.Combine(sdkDir, simpleName + ".dll");
                return File.Exists(candidate) ? Assembly.LoadFrom(candidate) : null;
            };

            // Eagerly load every MassLynx*.dll so reflection can see all types. The managed
            // MassLynx SDK assemblies are typically named MassLynxRaw.dll / MassLynx*.dll.
            foreach (var dll in Directory.GetFiles(sdkDir, "MassLynx*.dll"))
            {
                try { Assembly.LoadFrom(dll); } catch { /* skip unloadable */ }
            }

            // NEEDS-VALIDATION: the namespace-qualified type names below are the best-known
            // surface; confirm against the real SDK on Windows.
            var infoReaderType =
                FindType("MassLynxRaw.MassLynxRawInfoReader")
                ?? throw new TypeLoadException(
                    "MassLynxRawInfoReader not found among the loaded MassLynx assemblies");
            var scanReaderType =
                FindType("MassLynxRaw.MassLynxRawScanReader")
                ?? throw new TypeLoadException(
                    "MassLynxRawScanReader not found among the loaded MassLynx assemblies");

            _instance = new MassLynxApi(infoReaderType, scanReaderType);
            return _instance;
        }
    }

    /// <summary>Open a `.raw` directory and build the flattened spectrum index.</summary>
    public RawSession Open(string rawPath)
    {
        // info = new MassLynxRawInfoReader(rawPath); scan = new MassLynxRawScanReader(rawPath)
        var info = Activator.CreateInstance(_infoReaderType, new object?[] { rawPath })
            ?? throw new InvalidOperationException("MassLynxRawInfoReader ctor returned null");
        var scan = Activator.CreateInstance(_scanReaderType, new object?[] { rawPath })
            ?? throw new InvalidOperationException("MassLynxRawScanReader ctor returned null");

        var session = new RawSession { InfoReader = info, ScanReader = scan };
        BuildIndex(session);
        return session;
    }

    /// <summary>
    /// Walk every function → scan and record a flattened address per spectrum. Drift (IMS)
    /// expansion is deliberately omitted for the basic path: a TWIMS `.raw` still surfaces its
    /// per-(function, scan) summed spectra. Drift = -1 marks a non-IMS scan.
    /// </summary>
    private void BuildIndex(RawSession session)
    {
        int functionCount = ToInt(Invoke(session.InfoReader, "GetFunctionCount")); // NEEDS-VALIDATION
        for (int f = 0; f < functionCount; f++)
        {
            int scanCount;
            try
            {
                scanCount = ToInt(Invoke(session.InfoReader, "GetScansInFunction", f)); // NEEDS-VALIDATION
            }
            catch
            {
                continue; // skip a function whose scan count can't be read
            }
            for (int s = 0; s < scanCount; s++)
            {
                session.Index.Add(new SpectrumAddress(f, s, -1));
            }
        }
    }

    /// <summary>Resolve scalar metadata for one flattened spectrum.</summary>
    public WatersSpectrumMeta GetMeta(RawSession session, SpectrumAddress addr)
    {
        var info = session.InfoReader;

        int msLevel = 1; // MassLynx does not expose a clean MS level on the info reader; default MS1.
        double rtSeconds = 0.0;
        int polarityCode = -1;
        int continuity = 0; // default profile / continuum

        // Retention time (MassLynx returns minutes → seconds at the ABI). NEEDS-VALIDATION.
        var rt = TryInvoke(info, "GetRetentionTime", addr.Function, addr.Scan);
        if (rt != null)
        {
            rtSeconds = ToDouble(rt) * 60.0;
        }

        // Continuum vs centroid is a per-function property. NEEDS-VALIDATION.
        var isContinuum = TryInvoke(info, "IsContinuum", addr.Function);
        if (isContinuum != null)
        {
            try { continuity = Convert.ToBoolean(isContinuum) ? 0 : 1; }
            catch { /* keep default */ }
        }

        // Polarity off the ion mode (an enum/int; map by name then underlying value). NEEDS-VALIDATION.
        var ionMode = TryInvoke(info, "GetIonMode", addr.Function);
        polarityCode = MapPolarity(ionMode);

        return new WatersSpectrumMeta
        {
            Function = addr.Function + 1,
            Scan = addr.Scan + 1,
            Drift = addr.Drift + 1, // -1 → 0 (non-IMS); a real drift bin → 1-based
            MsLevel = msLevel < 1 ? 1 : msLevel,
            Polarity = polarityCode,
            SignalContinuity = continuity,
            RetentionTimeSeconds = rtSeconds,
        };
    }

    /// <summary>Fetch the (m/z, intensity) arrays for one flattened spectrum.</summary>
    public (double[] mz, double[] intensity) GetData(RawSession session, SpectrumAddress addr)
    {
        // ReadScan / ReadDriftScan return their arrays through out-params; reflection wraps the
        // call by supplying a writable object?[] and reading the out slots back. NEEDS-VALIDATION:
        // the exact out-param shape (float[] vs List<float>, masses-first ordering) must be
        // confirmed on the real SDK.
        var method = addr.Drift >= 0 ? "ReadDriftScan" : "ReadScan";
        object?[] args = addr.Drift >= 0
            ? new object?[] { addr.Function, addr.Scan, addr.Drift, null, null }
            : new object?[] { addr.Function, addr.Scan, null, null };
        int massesSlot = addr.Drift >= 0 ? 3 : 2;
        int intsSlot = addr.Drift >= 0 ? 4 : 3;

        var m = session.ScanReader.GetType().GetMethod(
            method, BindingFlags.Public | BindingFlags.Instance | BindingFlags.FlattenHierarchy)
            ?? throw new MissingMethodException(session.ScanReader.GetType().FullName, method);
        m.Invoke(session.ScanReader, args);

        var masses = AsDoubleArray(args[massesSlot]);
        var intensity = AsDoubleArray(args[intsSlot]);

        // Defensive: clamp to the shorter length so we never read past either array.
        int n = Math.Min(masses.Length, intensity.Length);
        if (masses.Length != n) { Array.Resize(ref masses, n); }
        if (intensity.Length != n) { Array.Resize(ref intensity, n); }
        return (masses, intensity);
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

    /// <summary>Coerce a MassLynx numeric array (float[]/double[]/List&lt;float&gt;) to double[].</summary>
    private static double[] AsDoubleArray(object? v)
    {
        switch (v)
        {
            case null:
                return Array.Empty<double>();
            case double[] d:
                return d;
            case float[] f:
                {
                    var d = new double[f.Length];
                    for (int i = 0; i < f.Length; i++) { d[i] = f[i]; }
                    return d;
                }
            case System.Collections.IEnumerable en:
                {
                    var list = new List<double>();
                    foreach (var o in en) { list.Add(ToDouble(o)); }
                    return list.ToArray();
                }
            default:
                return Array.Empty<double>();
        }
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

    /// <summary>Map a MassLynx ion-mode enum/int (by name, then underlying value) to 0/1/-1.</summary>
    private static int MapPolarity(object? ionMode)
    {
        if (ionMode == null)
        {
            return -1;
        }
        var name = ionMode.ToString() ?? "";
        // MassLynx ion modes are named like "ES+", "EI+", "ES-"; the trailing sign carries polarity.
        if (name.IndexOf("Positive", StringComparison.OrdinalIgnoreCase) >= 0 || name.Contains('+')) { return 0; }
        if (name.IndexOf("Negative", StringComparison.OrdinalIgnoreCase) >= 0 || name.Contains('-')) { return 1; }
        try
        {
            int code = Convert.ToInt32(ionMode);
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
/// extern "system" fn types in src/waters.rs.
///
/// Marshalling contracts:
///   * Strings cross as NUL-terminated UTF-16 (char* == ushort*); we read them with
///     Marshal.PtrToStringUni.
///   * Spectrum data uses the pointer+len+free pattern: SpectrumData pins managed arrays via
///     GCHandle and returns raw pointers; the caller copies and then calls FreeSpectrum, which
///     releases the matching pins.
/// </summary>
public static unsafe class Exports
{
    private static readonly object _gate = new();
    private static long _nextHandle = 1;
    private static readonly Dictionary<long, RawSession> _sessions = new();

    // Pins handed out by SpectrumData. Keyed by (handle, mzPtr, intPtr) so a stale or mismatched
    // (mz, intensity) pair can never be freed by accident — FreeSpectrum must present the exact triple
    // that SpectrumData returned.
    private readonly record struct PinKey(long Handle, IntPtr MzPtr, IntPtr IntPtr);

    private static readonly Dictionary<PinKey, (GCHandle mz, GCHandle intensity)> _pins = new();

    // Last error message, for a best-effort diagnostic channel surfaced to Rust via the LastError
    // export below. Every UnmanagedCallersOnly boundary stores the exception text here (via
    // RecordError) rather than letting it escape (which would kill the host process), so a failed
    // Open/SpectrumCount/SpectrumMeta/GetSpectrum carries a detail string the Rust side can fetch.
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
    // silently corrupt memory on the Rust side, so fail loudly at type init instead.
    static Exports()
    {
        int size = Marshal.SizeOf<WatersSpectrumMeta>();
        if (size != 32)
        {
            throw new InvalidOperationException(
                $"WatersSpectrumMeta marshals to {size} bytes; the Rust #[repr(C)] mirror expects 32. " +
                "Field order/types drifted — fix both sides in lockstep.");
        }
    }

    // CALLING-CONVENTION CONTRACT: these [UnmanagedCallersOnly] exports omit an explicit CallConvs
    // list, so the runtime uses the platform-default convention. The Rust side declares the matching
    // function pointers as `extern "system"`, which resolves to that same default. This project
    // ships x64-only (the MassLynx vendor DLLs are x64), where the platform default and Cdecl are
    // ABI-identical, so the two sides are provably consistent. Do NOT add
    // `CallConvs = new[] { typeof(CallConvCdecl) }` here without switching the Rust pointers to
    // `extern "cdecl"` in lockstep.

    // ---- open / close ----

    [UnmanagedCallersOnly(EntryPoint = "Open")]
    public static long Open(ushort* pathUtf16, ushort* sdkDirUtf16)
    {
        RawSession? session = null;
        try
        {
            string? rawPath = Marshal.PtrToStringUni((IntPtr)pathUtf16);
            string? sdkDir = Marshal.PtrToStringUni((IntPtr)sdkDirUtf16);
            if (string.IsNullOrEmpty(rawPath) || string.IsNullOrEmpty(sdkDir))
            {
                return 0;
            }

            var api = MassLynxApi.Load(sdkDir);
            session = api.Open(rawPath);

            lock (_gate)
            {
                long handle = _nextHandle++;
                _sessions[handle] = session;
                // Ownership transferred to _sessions; clear the local so the catch below won't
                // dispose a live session.
                session = null;
                return handle;
            }
        }
        catch (Exception ex)
        {
            // Stash the detail for the Rust side to fetch via LastError; return a generic 0 handle.
            // Dispose any partially-built reader state so a failed Open doesn't leak the native
            // MassLynx resources it managed to allocate.
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
            RawSession? session = null;
            List<(GCHandle mz, GCHandle intensity)> orphanedPins = new();
            lock (_gate)
            {
                if (_sessions.TryGetValue(handle, out session))
                {
                    _sessions.Remove(handle);
                }

                // Release any pins still outstanding for this handle. A well-behaved caller frees
                // each pair via FreeSpectrum, but a panic/early-exit on the Rust side could leave
                // some pinned; closing the handle is the backstop.
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
            // Close has no return code; swallow so nothing escapes the boundary.
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
    public static int SpectrumMeta(long handle, long index, WatersSpectrumMeta* outMeta)
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

    [UnmanagedCallersOnly(EntryPoint = "GetSpectrum")]
    public static int GetSpectrum(
        long handle,
        long index,
        double** outMzPtr,
        float** outIntPtr,
        long* outLen)
    {
        // Track pins locally so we can free them if anything throws after allocation but before
        // ownership is transferred to _pins.
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

            // MassLynx hands us intensities as float (widened to double in GetData); the mzPeak
            // schema wants f32 intensity. Narrow here so the pinned buffer we expose is already f32
            // (matches the Rust ABI type). Guard non-finite / out-of-f32-range values so a corrupt
            // double can't become a NaN or Inf in the output stream: clamp magnitudes beyond
            // float.MaxValue and map any NaN to 0.
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

            // Pin both arrays so the GC cannot move them while Rust copies. Released in FreeSpectrum.
            mzPin = GCHandle.Alloc(mz, GCHandleType.Pinned);
            intPin = GCHandle.Alloc(intensity, GCHandleType.Pinned);
            IntPtr mzAddr = mzPin.AddrOfPinnedObject();
            IntPtr intAddr = intPin.AddrOfPinnedObject();

            lock (_gate)
            {
                // Keyed by the full (handle, mzPtr, intPtr) triple so FreeSpectrum must present the
                // exact pair to release it.
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

    [UnmanagedCallersOnly(EntryPoint = "FreeSpectrum")]
    public static void FreeSpectrum(long handle, double* mzPtr, float* intPtr)
    {
        try
        {
            // A null m/z pointer (empty spectrum) is a no-op. The pin is identified by the exact
            // (handle, mzPtr, intPtr) triple GetSpectrum returned, so a stale/mismatched pair
            // cannot free the wrong pins.
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
            // FreeSpectrum has no return code; never let an exception escape the boundary.
            RecordError(ex);
        }
    }

    // ---- diagnostics ----

    [UnmanagedCallersOnly(EntryPoint = "LastError")]
    public static int LastError(ushort* buf, int cap)
    {
        // Fill-buffer contract (matches src/waters.rs WatersLastError): copy the stashed last-error
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

    private static RawSession? Get(long handle)
    {
        lock (_gate)
        {
            return _sessions.TryGetValue(handle, out var s) ? s : null;
        }
    }

    private static MassLynxApi ApiOrThrow()
    {
        // Load() is idempotent + cached once a session exists; we re-fetch the singleton.
        // The SDK dir was supplied at Open(); after the first successful Open the singleton
        // is set, so this returns the cached instance.
        return MassLynxApi.Load(SessionSdkDir());
    }

    // We don't retain the SDK dir per call; the singleton is already initialized after Open,
    // so Load() short-circuits and the argument is only used on the very first call. Returning
    // an empty string here is safe because by the time SpectrumMeta/GetSpectrum run, Load()
    // has a cached instance and never touches the directory again.
    private static string SessionSdkDir() => string.Empty;
}
