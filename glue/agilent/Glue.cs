// AgilentGlueHost — out-of-process .NET Framework 4.8 host for Agilent MHDAC.
//
// WHY .NET FRAMEWORK (not the .NET 8 in-process glue): MHDAC (MassHunter Data Access Component) was
// built for .NET Framework 4.x and, inside MassSpecDataReader.OpenDataFile, calls the legacy
// Delegate.BeginInvoke pattern (DataFileMgr.ReadNonMSInfoDelegate.BeginInvoke). BeginInvoke/EndInvoke
// are permanently unsupported on .NET Core / .NET 5+ (they throw PlatformNotSupportedException, with
// no opt-in flag). So MHDAC cannot run in a .NET 8 runtime — this is a standalone net48 EXE the Rust
// converter (src/agilent.rs) spawns once per .d.
//
// PROTOCOL (one-shot, argv in / file out):  AgilentGlueHost <in.d> <mhdacDir> <out.bin>
// Reads every MS scan via MHDAC and writes this little-endian binary file (the Rust twin of this
// layout, with tests, is src/agl.rs):
//     magic "AGL2" (4 bytes) | count u64 |
//     scanTypes: len u32 + UTF-8 (MHDAC MSScanFileInformation.ScanTypes.ToString(), e.g.
//                "Scan" or "MultipleReaction, SelectedIon"; empty when unreadable) |
//     device:    len u32 + UTF-8 ("<DeviceType>" + U+001F + "<device name>" + U+001F + "<serial>",
//                parts empty when unreadable) |
//     offset[count] u64 (abs file offset of each record)
//   then, per record:
//     rt f64 | msLevel i32 | polarity i32 | isCentroid i32 | scanId i32 |
//     nPoints u64 | mz[nPoints] f64 | intensity[nPoints] f64
// AGL1 (0.9.x) had no strings; the converter refuses it and asks for a rebuilt host. ScanTypes is
// what lets the converter keep MRM/SIM dwell runs (chromatograms, not spectra) out of the native
// lane: MHDAC hands those over as one one-point "MS2 spectrum" per dwell.
// Exit 0 on success; non-zero with one diagnostic line on stderr on failure (and out.bin removed).
// stdout is left clean (reserved). Little-endian is assumed (x64 Windows) for the bulk array copies.
//
// MHDAC access is REFLECTION-ONLY (no compile-time reference), so this builds with just a .NET SDK +
// the net48 reference assemblies — no Agilent DLLs at build time. The DLLs are needed only at runtime
// and are loaded from <mhdacDir> (a ProteoWizard install's vendor_api/Agilent; see README.md).

using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;

namespace AgilentGlue
{
    internal static class Program
    {
        private static int Main(string[] args)
        {
            if (args.Length != 3)
            {
                Console.Error.WriteLine("usage: AgilentGlueHost <in.d> <mhdacDir> <out.bin>");
                return 2;
            }
            string dPath = args[0], mhdacDir = args[1], outPath = args[2];
            var reader = new MhdacReader();
            try
            {
                int count = reader.Open(dPath, mhdacDir);
                // Write to a .part file and atomically publish only after a fully-successful write +
                // close. The offset table is back-filled at the end, so a host killed/crashed mid-write
                // (e.g. a native MHDAC AccessViolation that bypasses catch/finally) would otherwise
                // leave an out.bin with a zeroed offset table that the Rust reader treats as valid.
                string partPath = outPath + ".part";
                using (var fs = new FileStream(partPath, FileMode.Create, FileAccess.ReadWrite))
                using (var bw = new BinaryWriter(fs))
                {
                    bw.Write((byte)'A'); bw.Write((byte)'G'); bw.Write((byte)'L'); bw.Write((byte)'2');
                    bw.Write((ulong)count);
                    WriteString(bw, reader.ScanTypes);
                    WriteString(bw, reader.Device);
                    long tablePos = fs.Position;
                    var offsets = new long[count];
                    for (int i = 0; i < count; i++) bw.Write((ulong)0);   // reserve the offset table

                    for (int i = 0; i < count; i++)
                    {
                        offsets[i] = fs.Position;
                        Spec s = reader.ReadSpectrum(i);
                        bw.Write(s.RtMinutes);
                        bw.Write(s.MsLevel);
                        bw.Write(s.Polarity);
                        bw.Write(s.IsCentroid);
                        bw.Write(s.ScanId);
                        int n = s.Mz.Length;
                        bw.Write((ulong)n);
                        WriteDoubles(bw, s.Mz, n);
                        WriteDoubles(bw, s.Intensity, n);
                    }
                    bw.Flush();
                    fs.Position = tablePos;                                // backfill real offsets
                    for (int i = 0; i < count; i++) bw.Write((ulong)offsets[i]);
                    bw.Flush();
                }
                if (File.Exists(outPath)) File.Delete(outPath);
                File.Move(partPath, outPath);                              // atomic publish
                // A rewrite the archive would otherwise not know about: say so (the converter logs
                // a non-empty stderr of a successful host at warn level).
                if (reader.NonFiniteIntensities > 0)
                    Console.Error.WriteLine("AgilentGlueHost: " + reader.NonFiniteIntensities + " intensity value(s) were NaN/Inf and were stored as 0");
                return 0;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine("AgilentGlueHost error: " + MhdacReader.Flatten(ex));
                try { if (File.Exists(outPath)) File.Delete(outPath); } catch { /* best effort */ }
                try { if (File.Exists(outPath + ".part")) File.Delete(outPath + ".part"); } catch { }
                return 1;
            }
            finally
            {
                reader.Close();
            }
        }

        // len u32 + UTF-8 bytes (no BinaryWriter 7-bit length prefix: the Rust side reads a plain u32).
        private static void WriteString(BinaryWriter bw, string s)
        {
            byte[] b = System.Text.Encoding.UTF8.GetBytes(s ?? "");
            bw.Write((uint)b.Length);
            bw.Write(b);
        }

        // Bulk little-endian write of the first n doubles. Buffer.BlockCopy of a double[] is the raw
        // IEEE-754 bytes in platform order — little-endian on x64 Windows, matching the Rust reader.
        private static void WriteDoubles(BinaryWriter bw, double[] a, int n)
        {
            if (n == 0) return;
            byte[] buf = new byte[checked(n * 8)];
            Buffer.BlockCopy(a, 0, buf, 0, buf.Length);
            bw.Write(buf);
        }
    }

    /// <summary>One MS scan, as handed to the binary writer.</summary>
    internal struct Spec
    {
        public double RtMinutes;
        public int MsLevel, Polarity, IsCentroid, ScanId;
        public double[] Mz;
        public double[] Intensity;
    }

    /// <summary>
    /// Reflection-only bridge to one open MHDAC <c>MassSpecDataReader</c>. Resolves the (version-
    /// specific) MHDAC API once, then yields per-scan arrays + metadata. Non-IM MS only (MS1/MS2,
    /// profile or centroid); Agilent ion mobility needs the separate MIDAC SDK (out of scope).
    /// </summary>
    internal sealed class MhdacReader
    {
        private object _reader;     // MHDAC MassSpecDataReader instance
        private MhdacApi _api;      // cached reflected members
        private int _count;

        /// <summary>Points whose MHDAC intensity was NaN/Inf and were stored as 0 — reported on stderr at exit.</summary>
        public long NonFiniteIntensities { get; private set; }
        /// <summary>MHDAC <c>MSScanFileInformation.ScanTypes</c> as its flags string; "" when unreadable.</summary>
        public string ScanTypes { get; private set; } = "";
        /// <summary>DeviceType, device name and serial joined by U+001F; each part "" when unreadable.</summary>
        public string Device { get; private set; } = "\u001F\u001F";

        // MHDAC assembly set, loaded once from the mhdac dir.
        private static Assembly _mhdac;
        private static string _mhdacDir;
        private static List<Assembly> _mhdacAssemblies;
        private static readonly object _loadLock = new object();

        /// <summary>Open the <c>.d</c>; returns the MS scan count (<c>TotalScansPresent</c>).</summary>
        public int Open(string dPath, string mhdacDir)
        {
            EnsureMhdacLoaded(mhdacDir);
            _api = ResolveApi();
            Type readerType = _mhdac.GetType("Agilent.MassSpectrometry.DataAnalysis.MassSpecDataReader", true);
            _reader = Activator.CreateInstance(readerType);
            try
            {
                bool ok = (bool)_api.OpenDataFile.Invoke(_reader, new object[] { dPath });
                if (!ok) throw new Exception("MassSpecDataReader.OpenDataFile returned false for '" + dPath + "'");
                object scanFileInfo = _api.MsScanFileInfo.GetValue(_reader);
                _count = Convert.ToInt32(_api.TotalScansPresent.GetValue(scanFileInfo));
                ReadRunInfo(scanFileInfo);   // best effort: never fails the open
                return _count;
            }
            catch
            {
                Close();   // don't leak the native handle a failed open may hold
                throw;
            }
        }

        // ScanTypes (the MRM/SIM guard) and the instrument identity, every step best-effort: a
        // reflection miss on one MHDAC version must cost a metadata field, never the conversion.
        private void ReadRunInfo(object scanFileInfo)
        {
            // Like TotalScansPresent: these are EXPLICIT interface implementations on the concrete
            // MHDAC object, invisible to GetType().GetProperty — FindProp walks the interfaces.
            try
            {
                object st = FindProp(scanFileInfo.GetType(), "ScanTypes").GetValue(scanFileInfo);
                if (st != null) ScanTypes = st.ToString();
            }
            catch { }
            string deviceType = "", deviceName = "", serial = "";
            object dt = null;
            try
            {
                dt = FindProp(scanFileInfo.GetType(), "DeviceType").GetValue(scanFileInfo);
                if (dt != null) deviceType = dt.ToString();
            }
            catch { }
            try
            {
                // IMsdrDataReader.FileInformation (IBDAFileInformation): GetDeviceName(DeviceType) and
                // the instrument serial. Explicit-interface members, hence the interface walk.
                object fileInfo = FindProp(_reader.GetType(), "FileInformation").GetValue(_reader);
                if (fileInfo != null)
                {
                    MethodInfo getName = fileInfo.GetType().GetMethod("GetDeviceName");
                    if (getName == null)
                        foreach (Type iface in fileInfo.GetType().GetInterfaces())
                        { getName = iface.GetMethod("GetDeviceName"); if (getName != null) break; }
                    if (getName != null && dt != null)
                    {
                        try { object n = getName.Invoke(fileInfo, new object[] { dt }); if (n != null) deviceName = n.ToString(); }
                        catch { }
                    }
                    foreach (string p in new[] { "InstrumentSerialNumber", "SerialNumber" })
                    {
                        try
                        {
                            object v = FindProp(fileInfo.GetType(), p).GetValue(fileInfo);
                            if (v != null && v.ToString().Length > 0) { serial = v.ToString(); break; }
                        }
                        catch { }
                    }
                }
            }
            catch { }
            Device = deviceType + "\u001F" + deviceName + "\u001F" + serial;
        }

        /// <summary>Read scan <paramref name="index"/> (0-based) into a <see cref="Spec"/>.</summary>
        public Spec ReadSpectrum(int index)
        {
            if (index < 0 || index >= _count)
                throw new ArgumentOutOfRangeException(nameof(index), index, "count " + _count);

            // GetSpectrum(rowNumber, peakFilter=null, peakFilter2=null, DesiredMSStorageType.ProfileElsePeak).
            object specArrObj = _api.GetSpectrumByRow.Invoke(
                _reader, new object[] { index, null, null, _api.DesiredProfileElsePeak });
            object spec = FirstElement(specArrObj);
            if (spec == null) throw new Exception("GetSpectrum(" + index + ") returned no spectrum");

            double[] x = (double[])_api.SpecXArray.GetValue(spec);
            Array yRaw = (Array)_api.SpecYArray.GetValue(spec);   // float[] in MHDAC; widen to double
            int n = x.Length;
            if (yRaw.Length < n) n = yRaw.Length;                 // defensive: use the shorter length

            var s = new Spec { Mz = new double[n], Intensity = new double[n] };
            for (int i = 0; i < n; i++)
            {
                s.Mz[i] = x[i];
                double yi = Convert.ToDouble(yRaw.GetValue(i));
                if (double.IsNaN(yi) || double.IsInfinity(yi)) { yi = 0.0; NonFiniteIntensities++; }
                s.Intensity[i] = yi;
            }

            // Retention time (minutes) is best-effort. NaN, not 0.0, when the scan record is
            // unavailable: 0.0 is a legitimate time and the reader must be able to tell the two apart.
            s.RtMinutes = double.NaN;
            try
            {
                object rec = _api.GetScanRecord.Invoke(_reader, new object[] { index });
                s.RtMinutes = Convert.ToDouble(_api.RecRetentionTime.GetValue(rec));
            }
            catch { /* best effort */ }

            s.MsLevel = MapMsLevel(_api.SpecMsLevelInfo.GetValue(spec));
            s.Polarity = MapPolarity(_api.SpecIonPolarity.GetValue(spec));
            s.IsCentroid = MapStorageMode(_api.SpecMsStorageMode.GetValue(spec));
            // ScanId is i32 on the wire; MHDAC may type it wider. Don't let a >int.MaxValue id abort
            // the whole .d — fall back to the row index (it only labels the spectrum id string).
            try { s.ScanId = Convert.ToInt32(_api.SpecScanId.GetValue(spec)); }
            catch (OverflowException) { s.ScanId = index; }
            return s;
        }

        /// <summary>CloseDataFile + Dispose (best effort, never throws). Idempotent.</summary>
        public void Close()
        {
            if (_reader == null) return;
            try { _api?.CloseDataFile.Invoke(_reader, new object[0]); } catch { }
            try { (_reader as IDisposable)?.Dispose(); } catch { }
            _reader = null;
        }

        // ==================================================================
        // MHDAC loading + reflection resolution
        // ==================================================================

        private static void EnsureMhdacLoaded(string mhdacDir)
        {
            if (_mhdac != null) return;
            lock (_loadLock)
            {
                if (_mhdac != null) return;
                if (!Directory.Exists(mhdacDir))
                    throw new DirectoryNotFoundException("MHDAC directory not found: '" + mhdacDir + "'");

                // Resolve MHDAC's own sibling dependencies on demand from the same directory.
                AppDomain.CurrentDomain.AssemblyResolve += (sender, e) =>
                {
                    string simpleName = new AssemblyName(e.Name).Name + ".dll";
                    string candidate = Path.Combine(mhdacDir, simpleName);
                    return File.Exists(candidate) ? Assembly.LoadFrom(candidate) : null;
                };

                string main = Path.Combine(mhdacDir, "MassSpecDataReader.dll");
                if (!File.Exists(main))
                    throw new FileNotFoundException("MassSpecDataReader.dll not found in '" + mhdacDir + "'", main);
                _mhdacDir = mhdacDir;
                _mhdac = Assembly.LoadFrom(main);
            }
        }

        // The Agilent MHDAC assemblies in the load dir: the entry assembly + its Base*/agt*/MIDAC*
        // siblings, loaded once. MHDAC spreads its types across several assemblies and which one owns
        // a given interface varies by version, so we search all of them rather than hard-coding it.
        private static IEnumerable<Assembly> MhdacAssemblies()
        {
            if (_mhdacAssemblies != null) return _mhdacAssemblies;
            var list = new List<Assembly> { _mhdac };
            foreach (string dll in Directory.EnumerateFiles(_mhdacDir, "*.dll"))
            {
                string nm = Path.GetFileName(dll);
                bool isBase = nm.StartsWith("Base", StringComparison.OrdinalIgnoreCase)
                           || nm.StartsWith("agt", StringComparison.OrdinalIgnoreCase)
                           || nm.StartsWith("MIDAC", StringComparison.OrdinalIgnoreCase)
                           || nm.StartsWith("MassSpecDataReader", StringComparison.OrdinalIgnoreCase);
                if (!isBase) continue;
                try { list.Add(Assembly.LoadFrom(dll)); } catch { /* native/unloadable sibling — skip */ }
            }
            _mhdacAssemblies = list;
            return list;
        }

        private static Type ResolveMhdacType(string fullName)
        {
            foreach (Assembly asm in MhdacAssemblies())
            {
                Type t = asm.GetType(fullName);
                if (t != null) return t;
            }
            throw new TypeLoadException("MHDAC type '" + fullName + "' not found in MassSpecDataReader.dll " +
                "or its sibling Agilent assemblies under '" + _mhdacDir + "'. MHDAC version mismatch?");
        }

        private static MhdacApi ResolveApi()
        {
            const string ns = "Agilent.MassSpectrometry.DataAnalysis.";
            Type specType = ResolveMhdacType(ns + "IBDASpecData");
            Type scanRecType = ResolveMhdacType(ns + "IMSScanRecord");
            Type scanFileInfoType = ResolveMhdacType(ns + "IBDAMSScanFileInformation");
            Type desiredStorageType = ResolveMhdacType(ns + "DesiredMSStorageType");

            // MHDAC exposes the reader API through IMsdrDataReader as EXPLICIT interface implementations,
            // so the methods/properties are not visible on the concrete MassSpecDataReader via public
            // reflection. Resolve them from the interface; reflection-invoking an interface MethodInfo
            // on the concrete reader instance dispatches correctly.
            Type readerIface = ResolveMhdacType(ns + "IMsdrDataReader");

            var api = new MhdacApi();
            api.OpenDataFile = FindMethod(readerIface, "OpenDataFile", new[] { typeof(string) });
            api.CloseDataFile = readerIface.GetMethod("CloseDataFile", Type.EmptyTypes)
                                ?? FindMethod(readerIface, "CloseDataFile", Type.EmptyTypes);
            api.MsScanFileInfo = FindProp(readerIface, "MSScanFileInformation");
            api.TotalScansPresent = FindProp(scanFileInfoType, "TotalScansPresent");

            // The 4-arg int overload: GetSpectrum(int, IMsdrPeakFilter, IMsdrPeakFilter, DesiredMSStorageType).
            // Constrain the FULL signature (first param int, last param the storage-type enum) so we
            // can't bind a different 4-arg overload, and raise a descriptive error if it's gone.
            api.GetSpectrumByRow = readerIface.GetMethods().FirstOrDefault(m =>
                m.Name == "GetSpectrum" && m.GetParameters().Length == 4
                && m.GetParameters()[0].ParameterType == typeof(int)
                && m.GetParameters()[3].ParameterType == desiredStorageType)
                ?? throw new MissingMethodException("IMsdrDataReader",
                    "GetSpectrum(int, IMsdrPeakFilter, IMsdrPeakFilter, DesiredMSStorageType)");
            api.GetScanRecord = FindMethod(readerIface, "GetScanRecord", new[] { typeof(int) });

            api.SpecXArray = FindProp(specType, "XArray");
            api.SpecYArray = FindProp(specType, "YArray");
            api.SpecMsLevelInfo = FindProp(specType, "MSLevelInfo");
            api.SpecIonPolarity = FindProp(specType, "IonPolarity");
            api.SpecMsStorageMode = FindProp(specType, "MSStorageMode");
            api.SpecScanId = FindProp(specType, "ScanId");
            api.RecRetentionTime = FindProp(scanRecType, "RetentionTime");
            api.DesiredProfileElsePeak = Enum.Parse(desiredStorageType, "ProfileElsePeak");
            return api;
        }

        private sealed class MhdacApi
        {
            public MethodInfo OpenDataFile, CloseDataFile, GetSpectrumByRow, GetScanRecord;
            public PropertyInfo MsScanFileInfo, TotalScansPresent;
            public PropertyInfo SpecXArray, SpecYArray, SpecMsLevelInfo, SpecIonPolarity, SpecMsStorageMode, SpecScanId;
            public PropertyInfo RecRetentionTime;
            public object DesiredProfileElsePeak;
        }

        // ==================================================================
        // Enum mapping (by name, since the enum types are reflected)
        // ==================================================================

        private static int MapMsLevel(object v)   // MHDAC MSLevel: MS=1, MSMS=2
        {
            string s = v == null ? "MS" : v.ToString();
            return s.Equals("MSMS", StringComparison.OrdinalIgnoreCase) ? 2 : 1;
        }

        private static int MapPolarity(object v)  // Positive / Negative / Mixed / Unassigned
        {
            string s = v == null ? "" : v.ToString();
            if (s.StartsWith("Pos", StringComparison.OrdinalIgnoreCase)) return 1;
            if (s.StartsWith("Neg", StringComparison.OrdinalIgnoreCase)) return -1;
            return 0;
        }

        private static int MapStorageMode(object v)  // "Peak..." => centroid (1), else profile (0)
        {
            string s = v == null ? "" : v.ToString();
            return s.IndexOf("Peak", StringComparison.OrdinalIgnoreCase) >= 0 ? 1 : 0;
        }

        // ==================================================================
        // Small reflection helpers
        // ==================================================================

        private static MethodInfo FindMethod(Type t, string name, Type[] args)
        {
            MethodInfo m = t.GetMethod(name, args);
            if (m != null) return m;
            foreach (Type iface in t.GetInterfaces())   // explicitly-implemented members
            {
                m = iface.GetMethod(name, args);
                if (m != null) return m;
            }
            throw new MissingMethodException(t.FullName, name);
        }

        private static PropertyInfo FindProp(Type t, string name)
        {
            PropertyInfo p = t.GetProperty(name);
            if (p != null) return p;
            foreach (Type iface in t.GetInterfaces())
            {
                p = iface.GetProperty(name);
                if (p != null) return p;
            }
            throw new MissingMemberException(t.FullName, name);
        }

        private static object FirstElement(object arrayObj)
        {
            if (arrayObj is Array a) return a.Length > 0 ? a.GetValue(0) : null;
            return arrayObj;   // single object fallback
        }

        /// <summary>Flatten an exception (unwrap reflection's TargetInvocationException) to one line.</summary>
        public static string Flatten(Exception ex)
        {
            Exception e = (ex is TargetInvocationException && ex.InnerException != null) ? ex.InnerException : ex;
            var sb = new System.Text.StringBuilder(e.GetType().Name + ": " + e.Message);
            for (Exception inner = e.InnerException; inner != null; inner = inner.InnerException)
                sb.Append(" <- ").Append(inner.GetType().Name).Append(": ").Append(inner.Message);
            if (!string.IsNullOrEmpty(e.StackTrace))
            {
                var frames = e.StackTrace.Split('\n').Take(4).Select(f => f.Trim());
                sb.Append(" | at ").Append(string.Join(" | ", frames));
            }
            return sb.ToString();
        }
    }
}
