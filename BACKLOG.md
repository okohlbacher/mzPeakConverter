# Backlog

**Since 2026-09-04 the issue list lives in the review ledger**, not here. This file is a pointer plus
the handful of items the ledger does not track. Decided by the owner in the 2026-09-04 interview:
*"ledger is truth; BACKLOG.md becomes a short pointer."*

- **Current issues, ranked, with evidence and status:** the *mzPeakConverter Review Ledger*
  (claude.ai artifact, §8 "Measures" carries a Status column: *done* / *open*). Its source is kept
  at `scratchpad/review2/review-ledger.html` in the maintainer's session; ask for the link.
- **What is open** (2026-09-11): precursors on the BAF and Agilent lanes (neither has an MSn
  acquisition to verify against, here or in the corpus); device chromatograms beyond Bruker; the
  "no isolation" marker, which waits on mzdata 0.66.7; a shared `.NET` host for the four glue lanes
  (`dotnet_host.rs`), which only the Windows box can verify and which the SciEX `OnceLock` already
  removed the live defect from. Landed with the fixes, their Windows or box half still unconfirmed:
  SciEX precursors and that `OnceLock`, the MIDAC scaffold's deletion, the BAF lane's member
  digests, the M35 route label (`conversion_route`), and the box harness stamping the *effective*
  recipe.
- **Measured 2026-09-11, corpus-wide (report in `~/Claude/mzPeak/data/open-issues-fixes-2026-09-10/`):**
  156 mzML round trips over 227,979 spectra carry the **total intensity exactly** — every m/z
  difference is inside the `numpress-linear` bound the archive declares, ids/order/MS level/
  representation all preserved. Two gaps came out of it; both are fixed in 0.12.3: the archive →
  mzML export dropped the wavelength spectra (`--to mzml` from an mzML source wrote them all along),
  and a non-indexed mzML's chromatograms were never read. Decided 2026-09-13: the order of mass and
  wavelength spectra in an export does not matter (time order stays), and filtering into an archive
  takes the export's wavelength rules. Two adversarial reviews of those fixes left these open:
  - **Device-trace units are relabelled detector counts.** A pressure (psi), temperature (°C), flow
    (µL/min) or percent chromatogram is stored under the intensity column's single unit and written
    back to mzML the same way, on every lane; indexed sources too.
  - **SRM product windows (Q3) have no place in the archive.** mzML → mzPeak keeps a chromatogram's
    precursor and drops its `<product>`, so the export writes none.
  - **mzdata's writer wraps a chromatogram's precursor and product in list elements** the mzML 1.1
    schema rejects (`<precursorList>` inside `<chromatogram>`): 143 xmllint errors on the Shimadzu MRM
    file's mzML-lane output.
  - **Import recomputes a wavelength spectrum's TIC, base peak and lambda max** over what the source
    stated (ProteoWizard's Waters PDA spectra state them; the archive keeps only computed values).
  - **On an MS2-only mzML the mzML lane replaces the source TIC/BPC** with the writer's pair summed
    over the MS2 spectra, and loses a second source BPC (`BPC,±MS` on a timsTOF file), where the
    archive keeps them.
  - **The UV byte sink cannot tell an invented term from a stated one**: a wavelength spectrum that
    states `positive scan` or an `ion injection time` of 0 loses it (none in the corpus).
  - **A repeated chromatogram id** is stored twice, but an mzML written from it indexes the id once
    (mzdata's `OffsetIndex` is a map), so a re-read keeps one. None in the corpus.
  - **Chromatogram recovery is keyed on the `.mzML` extension**, as the declared-count check always
    was; mzML content under another name is not scanned. And a literal `</spectrumList>` after the
    `<chromatogramList` tag (in a comment) hides the list from the backwards search.
  - **An exported mzML states no file content, software or data processing**, and so no
    `defaultDataProcessingRef`, which the schema requires on both lists.
  - **`--drop-aux` on the archive → mzML lane** is neither honoured, refused nor reported inert.
  - **A gzip mzML export can end truncated with exit 0** if the encoder's final write fails at close:
    flate2 discards that error in `Drop`, and `mzml_sink` hands out no handle to finish it.
  - ponytail: the export reads each wavelength spectrum's arrays with a per-spectrum facet lookup,
    O(N²) in their number; fine at 520, slow at 20,000.
  - Not a defect, worth knowing: a native SciEX archive stores spectra **experiment-major**, so
    retention time is not monotonic in spectrum index there, while ProteoWizard writes acquisition
    order. Same 168,412 spectra, same ids.
- **M17 and M28 are closed** (2026-09-11, CHANGELOG Unreleased): the six archive-epilogue copies,
  the four mzML epilogues, the two `msconvert` invocations, the two TOF axis fields, the four
  hand-written `codec: "tof-grid"` blocks and the four ims-compact array triples are each one
  definition now, with no archive or exported-mzML byte changing. Two judgements inside them are
  deliberate and should not be re-litigated without new evidence: the four mzML PROLOGUES, the
  three ims-compact spectrum BUILDERS and the seven probe/observe loops stay separate because the
  copies differ (the reasons are in the code beside each); and `tests/contract_strings.rs` stays a
  source pin rather than becoming shared constants, because the two lanes whose strings it guards
  are `#[cfg(windows)]` — a readback test would assert nothing on every host that runs CI. What
  M28 asked for, a spelling that cannot drift between lanes, is the `tof_grid_block` builder.
- **Run metadata on the native lanes — landed after 0.11.2** (`src/run_metadata.rs`,
  `agilent_meta.rs`, `waters_meta.rs`, Bruker `GlobalMetadata`, SciEX `RunInfo`; CHANGELOG
  Unreleased). Measured against fresh lane pairs: blank1 now carries the vendor serial, sample and
  MassHunter version; Capan2 the model, sample and MassLynx version; SWATH/Sample002/MRM-HR WIFFs
  model, serial, `SCIEX OS`/`Analyst TF` versions, sample and both digested members. Acquisition
  clocks are tracked in their own item below. **Still open, by lane:**
  - **Orphan MS2 (precursors):** Waters landed 2026-09-09 (scan items through the MassLynx parameters object:
    SET_MASS / COLLISION_ENERGY; Capan2's 682 high-energy MSe scans carry a precursor stating the activation, DDA set
    masses a selected ion with a target-only window); SciEX landed 2026-09-10 (the
    `SpectrumMetaV2` glue export behind an ABI handshake, S-P2), not yet run on a WIFF: the box has
    to compare Sample002 and MRM_03 with their ProteoWizard twins, and the seven corpus archives
    (663k MS2 rows) gain precursors on rebuild; Agilent MHDAC (`MSScan.bin` precursor decode; no DDA/QQQ
    `.d` on host or in the corpus to verify against); BAF (SQL `Steps`/`Variables`; builds and runs
    on Linux; needs an MSn BAF acquisition).
    Bruker TDF/TSF and Shimadzu carry theirs.
  - **Waters ion mobility — landed 2026-09-09:** HDMSe/HDDDA functions are read bin by bin and written as
    frames (one spectrum per MassLynx scan, per-point `raw_ion_mobility` in ms, sorted by m/z); RT,
    polarity, scan window and function-type MS levels come from the SDK (W-P2 closed for those fields).
    Verified bin-for-bin against pwiz on Capan2 frames. Followed on 2026-09-09: precursors from the scan
    items, lock-mass detection (`getLockMassFunction`, else the method text), MS levels by function-type
    code, SONAR detection (written summed with a warning, not mislabelled as drift), the collapsed retention-time functions skipped, the
    zero-run mask off for frames (plus the writer bug that masked every profile spectrum regardless),
    acquisition-time order. Remaining on this lane: CCS per peak (`getCollisionalCrossSection` needs a
    charge); a SONAR file to decide how quadrupole-position bins should be stored; mixed IMS/non-IMS
    runs beyond the sFtsk_2 probe; the id convention — pwiz's `function=F process=0 scan=S` names a
    frame here and a drift bin there (pwiz's own combined mode uses `merged=I function=F block=B`), a
    spec question. From the 2026-09-09 adversarial review (78 findings, synthesis in the data dir's
    `waters/review/SYNTHESIS.md`; every wrong-data path closed and re-verified at HEAD), the items that need a
    DECISION rather than code: (a) RESOLVED 2026-09-09 (research + DLL probe round 23: no window is stated anywhere for MSe — the
    lane now writes the acquisition range as the window with a provenance parameter, as pwiz does); (a2) the PSI DIA recommendation v1.0 (§3.4) marks an MSe/HDMSe window with MS:1003159
    "no isolation" (= "isolation window full range") on the isolation window itself; mzdata's `IsolationWindow`
    carries no parameter list and the vendored writer appends an empty one, so the term has no home yet —
    decided 2026-09-10 (D8), after mzdata 0.66.7: the lane sets `NoIsolation` and the writer writes MS:1003159
    beside the numbers (see the isolation-window item below);
    likewise `file_description.contents` could state MS:1003226 (HDMSe) / MS:1003227 (MSe). (a3) a `_dda.inf`
    sidecar (Waters post-acquisition tooling; none in the corpus) makes the DLL's DDA processor return real
    quad-isolation offsets (keys 1900/1901) — wire them, with provenance, if such a run ever arrives; the
    probe lever `MZPC_WATERS_PROBE_QUAD=2` reads them today. (a4) record the tune-page quad profile
    (`MS Profile Type`, `MSProfileMass1..3`, `LM/HM Resolution`) as run-level parameters so a reader can
    bound the real RF-only passband (Waters: low cut ≈ 0.8 × set mass under a Manual profile).
    (b) MS:1000045 on MSe rows is the scan
    item's 4 eV trap energy (pwiz writes the same); the transfer ramp is on MS:1002013/1002014; (c) the
    synthesized TIC keeps the lock-mass function's frames, as pwiz's does; (e) mzPeakViewer cannot see a Waters frame at all
    (keys on `ims_calibration` and the 1/K0 array name; needs MS:1003007 + `waters_drift`); (f) non-ASCII
    `.raw` paths go through the narrow-char `createRawReaderFromPath` — untested. Coverage still owed:
    a centroid IMS run, an all-empty frame, a non-IMS `.raw`, a SONAR file, a full uncapped HDDDA
    conversion, `validate_everything.py` on the frame archives. Viewer: mzPeakViewer keys its mobility panel on the
    `ims_calibration` block and the 1/K0 array name; it needs to recognise `raw_ion_mobility` (ms)
    and the `waters_drift` block.
  - **Device chromatograms** (UV, pressure, temperature; B-P3): the mzML lane gets 620 traces on
    8 Agilent archives and more from pwiz's Waters/SciEX/Thermo readers. The Bruker lanes now read
    HyStar's `chromatography-data.sqlite` themselves (`src/bruker_traces.rs`; verified on TDF and
    TSF — the corpus BAF run has no such file); the Agilent, SciEX, Waters and Shimadzu native
    lanes still write TIC/BPC only. Agilent `.cg`/`.cd` layouts unknown.
    Converting a Bruker archive's mzML export back stores every device array as an auxiliary array
    under its name (`schema_sample_chromatogram` samples time and intensity only, since
    2026-09-11); a non-standard column had read back nameless and came back from a second round
    trip as a column named ''. The vendored point reader still hands a non-standard `chromatograms_data`
    column back without its name, which an archive built before that (or by another writer) shows.
    Consumers (2026-09-11): mzPeakViewer plots a stored chromatogram's `time array` against its
    `intensity array` only (`packages/core/src/reader/explorer/browse.ts`, `getStoredChromatogram`),
    so a device trace, whose values are auxiliary arrays over a null `intensity` column, draws as a
    flat line at zero (25 of 27 chromatograms on a 2485.d archive); and it takes a chromatogram's
    type from a promoted `MS_1000626_chromatogram_type` column or six fixed accessions
    (`engine/chrom.ts`), so pressure (MS:1003019) and flow-rate (MS:1003020) traces get no label
    although the flat `chromatogram_type` column carries them. Both are viewer changes (take the
    first auxiliary array, with its name and unit, when `intensity` is absent or all null; read the
    flat column); USER_MANUAL §7 says where the values are.
  - **Instrument components on non-Bruker lanes:** pwiz asserts hand-tabled sources and detectors
    per model; the native lanes state only what the file says (do-not-guess) — a decision, not a gap.
- **Acquisition clocks — every open point in one place (2026-09-09).** `run.start_time` is an RFC 3339
  instant and RFC 3339 cannot say "zone unknown", so the converter's rule (branch
  `feat/native-run-metadata`, `src/run_metadata.rs`) is: a vendor time that STATES its UTC offset is
  written verbatim; a wall clock WITHOUT a zone leaves `run.start_time` null and is preserved verbatim in
  `metadata.acquisition_time = {wall_clock, zone: "unstated", source, note}`. Nothing is shifted by the
  converting host's zone. Measured per vendor on the six lane pairs (native vs ProteoWizard 3.0.26175):
  | vendor (pair) | what the file states | native lane | ProteoWizard writes |
  |---|---|---|---|
  | Agilent `.d` (blank1) | `Contents.xml` `AcquiredTime` `2022-11-01T13:11:27.729717400-04:00` (= 17:11:27Z) | `run.start_time` verbatim | `2022-11-01T18:11:27Z` — off by +1 h from the stated instant; mechanism not established (not the box's offset either: CET would give 16:11Z or 12:11Z) |
  | Bruker TDF/TSF | `GlobalMetadata.AcquisitionDateTime` with offset | verbatim | verbatim (agrees) |
  | Waters `.raw` (Capan2) | `_HEADER.TXT` `Acquired Date/Time` `03-Dec-2018 22:39:33`, no zone | null + block `22:39:33` | `2018-12-03T22:39:33Z` — labels the wall clock UTC |
  | SciEX `.wiff` (SWATH, Sample002, MRM_03) | Clearcore2 `AcquisitionDateTime`, `Kind = Unspecified` | null + block (`09:52:14`, `19:58:37`, `02:56:30`) | wall clock − 2 h on all three (`07:52:14Z`, `17:58:37Z`, `00:56:30Z`) although the runs are from November, February and August — the box's CURRENT offset, not the acquisition date's |
  | Shimadzu `.lcd` (Blind) | `File Property` stream: `SampleInfo.DateTime` = UTC FILETIME `2024-02-15T10:47:18.756Z` beside `szLocGMTDiffGenDateTime = +01'00'` (measured: OLE2 directory FILETIMEs and the MS-CAB local stamps in the same file agree) | `2024-02-15T11:47:18.756+01:00` — read from the file on any host (`src/shimadzu_meta.rs`; the DLL's `SampleInfo` is empty in the .NET 8 host) | `2024-02-15T08:47:18Z` — the DLL's UTC value minus the box's CURRENT offset (+2 h in September): `ShimadzuReader.cpp::getAnalysisDate` adds `universal_time() − local_time()` evaluated at conversion time (`adjustUnknownTimeZonesToHostTimeZone`) |
  | Thermo `.raw` | mzdata's reader, zoned | verbatim | verbatim (agrees) |
  The SciEX and Shimadzu shifts are the same ProteoWizard mechanism — the host's offset at CONVERSION
  time applied to a value that already is UTC (Shimadzu) or that the reader treats as unzoned (SciEX);
  the Agilent +1 h is not yet pinned to it. Open, in order: (1) **Shimadzu — resolved 2026-09-09** by
  reading the file (option c); the DLL-side emptiness is still being analysed (Codex's leading
  hypothesis: the 1252 code page has no decoder in .NET 8 unless the provider is registered — the glue
  now registers it; on the next Blind run the debug dump that fires on an empty date no longer fired). (2) **Validator
  rule:** the validator should flag a null `run.start_time` WITHOUT the block on a vendor-derived
  archive, and never flag the block itself (handoff to mzPeakValidator pending). The reader guidance,
  fall back to `acquisition_time.wall_clock` when `run.start_time` is null, is in USER_MANUAL §8. (3)
  **Decision recorded, revisit only on request:** we do NOT assert a zone for an unzoned wall clock for
  consumer compatibility — pwiz's `Z`/host-offset labels are exactly the false precision the rule avoids.
  (4) **Upstream:** the Agilent +1 h and the SciEX current-offset shifts are ProteoWizard behaviours worth
  a report once the mechanism is pinned (pwiz's `Reader_Agilent` / `Reader_ABI` date handling; not
  reproduced by us). (5) Agilent `Contents.xml` without `AcquiredTime` and Waters headers without
  `Acquired Date` are handled (nothing recorded); a vendor time that fails to parse is logged at WARN
  with the raw text and dropped — pinned by `run_metadata::tests::parse_vendor_time`.
- **SciEX MRM/SIM dwell runs** are refused natively (`refuse_if_unsupported`) and republished via
  msconvert; MRM-HR scan runs convert natively. Open: reading the transitions natively (Q1/Q3,
  compound, CE, RT window — S-P4); the published `En_PPY.mzpeak` carries 1 of its 117 samples. Since
  PR #18 the msconvert lanes refuse a multi-sample WIFF without `--sample`, so the box fallback, which
  passes none, now fails on `En_PPY.wiff`; which of its samples to republish, each as its own
  archive, is D4 and still open.
- **Surfaced by the 0.11.3 corpus rebuild (2026-09-09).** (1) The box relay returns archives through
  one presigned S3 PUT, capped at 5 GB: PXD077098's Waters TWIMS run (15.4 GB `.raw`) now writes a
  9.04 GB frame archive (it was 2.1 GB as drift-summed scans) and was delivered by hand (direct scp
  with a checksum, then stamped); 2 of the 201 corpus archives are over the ceiling (that one and
  PXD076703 at 9.8 GB). `tools/s3_relay.py` can now presign the parts of a multipart upload for a
  hand-driven delivery, but the automatic path needs three more things, none of them wired: the box
  worker must slice and upload the parts instead of refusing at `stage=too-big`
  (`tools/box_convert_remote.ps1`), the deferred integrity gate must stop comparing the ETag against
  the body md5 (a multipart object's ETag is md5-of-part-md5s + `-N` — verify the composite from
  per-part md5s the box reports, or the size), and `publish` must stop using `copy_object`, which S3
  caps at 5 GB, so even a completed multipart object cannot reach its durable corpus key today. An
  scp fallback avoids all three, and `box_convert.sh` now takes it for a local target (the default of
  `corpus_reconvert.py --box`): the box holds the archive and the host pulls it by scp with a size and
  md5 check; only an `s3://` target still stops at `stage=too-big`. (2) The frame representation's size cost on big HDMSe runs: Capan2 166 → 531 MB,
  PXD077098 2.1 → 9.0 GB (58 % of the vendor `.raw`; every point of every bin is kept, zero flanks
  included, 44–46 % of the points) — **accepted 2026-09-10 (D7)**: a per-bin mask that keeps peak
  boundaries saves nothing (MassLynx returns only flank zeros plus two sentinels per bin), and
  dropping every zero (about −26 %) could not be undone. (3) ~~The harness's box updater could not fetch the release tag (twice)~~
  — FIXED, and the shallow-clone diagnosis recorded here was wrong: `--tags` fetches `refs/tags/*`
  whatever the configured refspec says, measured on a clone provisioned exactly like the box's
  (`--depth 1 --single-branch`), where a later tag arrives and checks out with the repository staying
  shallow. The real cause was PowerShell 5.1 turning git's redirected stderr into a terminating
  error, so the updater threw on the one day `git fetch` had something to report and reported git's
  own "From https://github.com/..." notice as the failure; `Invoke-Native` in
  `tools/box_update_remote.ps1` relaxes `$ErrorActionPreference` around each native call. Still open:
  make the harness assert the box's version before, not after, the jobs. (4) A long box conversion driven from an interactive SSH session
  is dropped by the gateway (`Connection closed by remote host` after ~25 min of silence) even with
  ServerAlive keepalives — run long box jobs detached and poll a log.
- **Isolation windows and the MS:1003159 marker — decided 2026-09-10 (D8), blocked on mzdata
  0.66.7.** The mzPeak spec's prose (`docs/schemas/spectra.md`) says an `isolation_window` group MUST
  carry at least one MS:1000792 child; its schema rules (`schema/table_rules.json`
  `precursor_isolationwindow_may`) and the validator say MAY. That is the spec's call, and every
  corpus row conforms under either reading. PSI's DIA recommendation v1.0 (§3.4) marks full-range
  acquisitions (MSe/HDMSe, AIF, bbCID, MSall) with MS:1003159 "no isolation". Decision: write the
  marker beside the acquisition-range numbers the lanes already write. mzdata 0.66.7 (linked since
  2026-09-23) has `IsolationWindowState::NoIsolation`, so no side channel is needed. Still to do: the
  vendored writer's `NoIsolation` arm currently writes nulls (upstream's behaviour, a placeholder — no
  lane sets the flag yet); the designed arm keeps target and offsets and appends MS:1003159, the
  reader maps the term back, and the Waters lane sets the flag. Order matters on an mzML round trip:
  upstream's reader drops offsets that follow the marker.
- ~~**The mzdata git fork (`[patch.crates-io]`).**~~ Done 2026-09-23: mzdata `=0.66.7` from
  crates.io, patch block deleted, `tests/isolation_window_offset_order.rs` kept and green against the
  released crate. (The zero-lower-offset sweep had already found 0 collapsed offsets in 2,669,071
  corpus precursor rows.)
- **Not in the ledger — the Thermo target-only isolation-window guard is interim (2026-09-10).**
  When a scan has no `MS<n> Isolation Width` trailer, thermorawfilereader's `Lib.cs` builds its window
  from the scan filter's width, halved twice, and inverted when the filter reports a negative width.
  `src/thermo_isolation.rs` writes such windows target-only and declares
  `thermo:target-only-isolation-window`. An upstream issue (code path, plus a minimal fix: drop the
  second halving, treat a non-positive width as unknown) is drafted but not filed. Once a
  thermorawfilereader release with the fix reaches mzdata, delete the guard and its transformation
  entry. Rebuild `ec04479_qy_4cell_SanJose_A1`, `2013_30_Amrutha_050713_1`, `SZB8102938` and
  `LD401_001fmol_r1` now and again then. Even with the upstream fix, the LTQ XL and LCQ windows would
  come out at half the method's 2.00, because their filter reports 1.0; msconvert reads the method.
- **Not in the ledger — found by the 2026-09-11 harmonization reviews, not fixed there:**
  - SciEX glue reopen check in its own process: `GlueApi::shared` (the `OnceLock`) has landed, but
    windows.yml runs the reopen test for Shimadzu only. A twin
    (`sciex::tests::a_reader_opens_again_after_one_was_dropped`, `MZPC_SCIEX_GLUE` and
    `MZPC_PWIZ_DIR=${{ runner.temp }}`, the same `1 passed` guard) needs glue/sciex `Glue.cs`
    `Open` to catch and return 0 as Shimadzu's does; confirm that first.
  - An archive → mzML export drops the archive's stored TIC/BPC and writes a TIC and BIC the
    vendored exporter derives from the spectra in spectrum order, so a run whose spectra are not in
    time order gets an unsorted time array (`tiny.pwiz.1.1`: 5.8905, 5.9905, 0.0, 0.7008). Export the
    stored pair, or sort.
  - The archive route loses `tiny.pwiz.1.1`'s two target-only precursors on its `cycle=22`
    spectrum (no `spectrumRef`): the archive holds one precursor row, and its export shows a window
    of target 0, while `--to mzml` keeps 456.7 and 678.9. Same on 0.11.5; a precursor-storage item.
  - A filtered archive → mzML export (`-o x.mzML --rt`/`--ms-level`) keeps a precursor's
    `spectrumRef` to a spectrum it filtered out, which the rewrite route nulls (small.RAW
    `scan=9`); it also parses the archive index twice (duplicated reader warnings).
  - `transcode_to_utf8` names its temp copy `.mzpc-utf8-<pid>-<stem>`, so two conversions of files
    with one stem in one process collide ("writing transcoded …: Invalid argument"). The CLI converts
    one input per process; in-process tests converting `tiny.pwiz.1.1.mzML` in parallel flaked on it
    once (`convert_file_writes_the_route_it_is_handed`).
  - The HUPO python reader sizes its spectrum iterator by row count and indexes by spectrum index,
    so a `--rt`/`--ms-level` rewrite, whose survivors keep their sparse original indices, raises
    KeyError (also on 0.11.5). File it with the footer-count definition, or renumber survivors.
- **Not in the ledger — native-lane metadata parity** (`tests/lane_metadata_parity.rs`). Sample,
  acquisition time, serial, model, member SHA-1s, software and contents landed in 0.11.3. Still open:
  the non-MS device chromatograms on the Agilent, SciEX, Waters and Shimadzu native lanes (the Bruker
  lanes write HyStar's since 2026-09-10); the SciEX `run.id` (ProteoWizard names a WIFF run after its
  sample, the native lane after the file stem). The BAF lane records its member SHA-1s since
  2026-09-10, and the instrument series, serial, software and acquisition time the baf2sql
  Properties table states; CI builds that read on Linux and Windows, but it has never run against a
  real baf2sql cache, and FM_1-1_01_20254 and NreB_PAS_DECONV gain it only on rebuild.
- **Not in the ledger — Agilent native lane follow-ups (0.11.0, 2026-09-06):** `--tof-grid` on the
  MHDAC lane (the Q-TOF profile points sit on the flight-time lattice — the msconvert+`--tof-grid`
  build of the same run is 200 MB against 245 MB numpress-chunked f64; a SciEX-style per-run fit
  would close that); MRM/SIM transition chromatograms through MHDAC (today refused → msconvert);
  the temp-file materialisation (16 B/point, whole run)
  could stream, and the inspect path (no `-o`) pays it in full just to print a scan count (a host
  `--count` mode would fix both); no kill-on-parent-death for the host process (a killed converter
  orphans it, still writing its temp file: a kill-on-close Job Object needs windows-sys's
  `Win32_System_JobObjects` feature, a dependency change); a Ctrl+C still leaves the host's temp
  file behind (the deadline and the panic-hook sweep do not cover a console interrupt, which ends
  both processes); per-record scan types in the protocol so a mixed Scan+MRM method can drop the dwell rows
  instead of storing them as one-point spectra (today: a warning).
- **Not in the ledger — surfaced by the 0.10.2 corpus rebuild (2026-09-06):** a chunk-capable
  integer axis. M6 put gridded profile spectra into `spectra_data`, which therefore has to be point
  layout, so a native SCIEX run's off-lattice profile minority is now stored as exact f64 points
  instead of numpress chunks: 9.2 % of the points on MSV000093587 Sample002 (+27 % archive), 3.2 %
  on PXD011326 (+12 %), 1–3 % on three more SWATH runs. A `tof_index` list column beside the chunk
  encoding (or a per-facet mixed layout) would recover it. **Accepted 2026-09-10 (D6):** that size
  stays; no chunk-capable axis or mixed layout is planned.
- **Not in the ledger — spec and CV items, all deferred by decision:**
  - PSI-MS term request for the per-window ion-mobility band (currently `MZP:1000006/1000007`).
    *Decision: keep MZP, do not file for now.*
  - Spec proposal: a per-spectrum precursor ordinal column in both precursor facets, so the join
    key is unique for multi-precursor spectra. *Decision: propose it; keep reader-side positional
    pairing meanwhile.*
  - From the speXtract S30 handoff, spec asks not converter work: a TIMS scan→1/K0 grid term
    (signal-data.md §grid transforms); a quantified request for per-(frame, scan) chunking (ruled
    out by chunked-layout.md:62–63; would give ≈ −6 % vs the vendor file); whether the mixed
    layout-family deviation (conformance.md item 4) should be raised with PSI.
  - Upstream mzdata: graceful decode in the sourceFile handler (old #8).
- **.NET 8 end of support, 2026-11-10** (the same day as .NET 9; .NET 10 runs to 2028-11-14). The
  SciEX and Shimadzu glues target net8.0. **Decided 2026-09-10 (D9): stay on net8 for now.**
  After that date those lanes need an out-of-support runtime, and a host with only .NET 9 or 10 fails
  framework resolution. A later retarget goes to net10.0: Shimadzu would then lean on Microsoft's
  unsupported BinaryFormatter compatibility package inside a hostfxr component (unverified), or move
  out of process on net48 as the Agilent host did.
- **Decided 2026-09-10 — D1–D15 of the open-issue review.** Each line is the option the fixes take.
  Only D4's choice of samples stays with the owner.
  - D1: a data facet's `<entity>_count` is max(index)+1 in that file, 0 when empty (`spectra_peaks` of centroid-only runs too).
  - D2: the secondary facets (`*_scans`, `*_precursors`, `*_selected_ions`) carry no `spectrum_count`, `chromatogram_count` or `wavelength_spectrum_count`, from the writer or from the filter lane's rewrite.
  - D3: a Thermo window whose scan states no positive `MSn Isolation Width`, or that comes out empty or inverted, is written target-only, declared and warned once, until thermorawfilereader is fixed upstream.
  - D4: a multi-sample WIFF becomes one archive per chosen sample, with `--sample N` kept through the box fallback; which of `En_PPY.wiff`'s 117 samples to publish stays with the owner.
  - D5: `corpus_reconvert.py` writes the durable v09 keys only behind an opt-in flag; by default a box archive comes back to the host.
  - D6: the native SciEX archive size is accepted; no chunk-capable integer axis.
  - D7: the Waters HDMSe frame size is accepted (Capan2 166 → 531 MB).
  - D8: full-range isolation windows carry MS:1003159 beside their numbers once mzdata 0.66.7 is out; MUST vs MAY stays with the spec.
  - D9: the in-process glues stay on net8 for now.
  - D10: the MIDAC scaffold is deleted; IM-QTOF runs stay on `--via-msconvert`.
  - D11: the unwired `glue/waters` is deleted.
  - D12: the SBOM is generated for and attached to each release instead of tracked; every release archive ships `THIRD-PARTY-NOTICES.md` with the Apache-2.0 text; the `mzpeak_prototyping` license stays the owner's to settle.
  - D13: M35 gets a route label (`conversion_route`) only; fallback rows are not re-merged into frames.
  - D14: ProteoWizard's `_xHHHH_` escapes in `run.id` and the software ids are decoded when the mzML and imzML lanes copy metadata (47 corpus archives); the shared fixup is left alone, so an exported mzML id stays an XML name.
  - D15: `transformations` lists what a conversion applied, counted by the writer, not what it was configured to do (135 corpus archives change on rebuild).
  - Harmonization (2026-09-11): every chromatogram time is stored in minutes, on every lane; a time recorded in seconds or milliseconds (ProteoWizard's mzML chromatograms, HyStar's device traces) is divided into minutes before the schema is sampled and declared as `chromatogram-time-to-minutes`. `--rt` keeps reading a column's declared unit, for the seconds columns of mzML-lane archives built by 0.11.5 and earlier (none published).
  - Harmonization (2026-09-11): a default archive embeds no raw signal file of a BAF, Agilent MassHunter or Waters MassLynx directory (`analysis.baf*`, `*.ami`, `ser`/`fid`; `MSProfile.bin`, `MSPeak.bin`, `IMSFrame.bin`; `_FUNC*.DAT/.IDX`, `_func*.cdt/.ind`), matched by file name in any case; kept by decision: `MSScan.bin`, `MSMassCal.bin`, `*.mcf`, `_FUNC*.STS`, `_CHRO*`, `_mob/`, and the timsTOF `*_bin` on the f64 TDF/TSF lanes.

## Vendoring exit — `vendor/mzpeak_prototyping` (opened 2026-09-23)

Owner decision: *"we will move away from vendored libraries asap."* **Guiding principle (owner, 2026-09-23):
converge on the upstream code — mzdata and mzpeak_prototyping — to the extent possible.** When a choice
exists, take upstream's layout, cutter, encodings and API; a deviation must be measured, minimal, and named
as such (a candidate for an upstream proposal, not a permanent fork). Evidence and the full catalogue live
in `~/Claude/mzPeak/output/vendor-audit-2026-09-23/` (`VENDOR-AUDIT-mzpeak_prototyping.md`,
`upstream-grid-writer-assessment.md`, `delta-categorization.md`). State: base `589d6e3`, 18 commits behind
upstream `eb08ba0`, ~4,830 lines / 268 hunks of local delta in 23 of 30 files; 61 distinct changes
(31 bug fixes, 14 upstreamable features, 15 converter-specific, 9 already superseded, 4 churn groups).
**Merging upstream HEAD is not viable** — the residual would be 516 hunks, larger than the delta — so the
route is: adopt a clean `eb08ba0` (needs mzdata 0.67.1 + `cv`), re-apply only what is not superseded or
churn, in this order. Each item below is explored and decided one at a time.

1. **Point-layout grid → upstream chunk-layout grid** (~1,150 lines; the hard residual). SciEX and
   Agilent-via-msconvert sqrt grids, the Shimadzu Int64 lattice and the 0.12.x timsTOF path write an
   integer point column tagged `MS:1003824/5` with `mzpeak:transform_params` (+ per-spectrum
   `tof_c0/c1`). Upstream's grid is chunk-layout only; a spec-conformant reader misreads ours. Writing,
   m/z-less summaries and above all range queries cannot live outside the library.
   **Tested 2026-09-23** (`point-to-chunk-grid-test-2026-09-23.md` + `chunk_grid_proto.py` in the audit
   directory; Shimadzu Blind/HEK, Agilent FM_01_Pos, Shimadzu DIA 20 ng). Outcome: the **sqrt/TOF grids
   migrate** — bit-exact, decoded by the vendored reader (Blind/HEK round trips 100 % identical), size
   −7.5 … +6 % with one chunk per spectrum (+2.5 % on the 1.1 GB Agilent file) but +13 … +30 % with
   upstream's 50-Th default, so the chunk width must be per lane; negative indices from a run-wide fit
   need per-chunk re-anchoring (≤ 7e-10 ppm) and bounds must be the model at the stored indices. The
   **1e-9 lattice centroids cannot move as-is**: 2³² steps = 4.29 Th caps every chunk, +18.7 % on DIA,
   +126 … +132 % on sparse Blind; needs 64-bit `indices` upstream (ask Joshua), or accept the cost, or drop
   the lattice (+64 … +91 %). Also found: a chunk facet without a Parquet page index reads back as EMPTY
   spectra with exit 0 (fix the reader regardless); ungriddable spectra need `Basic` rows, not delta.
   Closeness to upstream (measured): the exact sqrt chunk grid is literally mzdata's own `MS:1003825
   [intercept, slope]` Param convention per spectrum, and upstream's writer recovers our indices 100 %
   from the values + that Param (Blind/HEK); a run-wide fit loses its negative indices to `clamp_u32`
   (Agilent: 1.15 M points → 0, silently), and the lattice saturates (0.4–5 % of points survive) because
   upstream holds one model per spectrum. Upstream's own fit (F, ≤ 1e-6 Da, lossy 0.004 ppm) is −10.6 %
   on DIA / −6.4 % HEK / +56 % Blind. **Decided (owner, 2026-09-23): go with upstream.** The sqrt/TOF
   lanes (Shimadzu profile, Agilent-via-msconvert, SciEX) move onto upstream's `ChunkingStrategy::Grid`
   with the exact per-spectrum model attached as an `MS:1003825` Param (mzdata's convention); the 1e-9
   lattice is dropped — Shimadzu centroids take upstream's grid fit (F) / default. Chunk width per lane,
   spectrum-sized for profile/TOF. Requires the re-vendor to `eb08ba0` first (see the plan below).
   `src/shimadzu_grid.rs`, `src/mz_lattice.rs`, `src/tof_grid.rs`, the `tof_index` schemas and the MZP
   `tof_c0/c1` columns retire with it; `tof_calibration`/`mz_calibration` index blocks go.
   **DONE (2026-09-23, unreleased → 0.14.0).** All four point-grid lanes write upstream's chunk grid:
   mzML `--tof-grid`, native SCIEX and the Shimadzu profile facet attach the exact per-spectrum
   `MS:1003825 [c0, c1, 1]` model to the m/z array (`sqrt_grid_arrays`; negative run-wide indices
   re-anchored per spectrum; one chunk per spectrum), `--agilent-grid` the same with the vendor's
   polynomial rows moved to an `agilent_calibration` block; lattice centroids (mzML lane when
   detected, Shimadzu native) take upstream's fitted `MS:1003824` grid (50-Th chunks, ≤ 1e-6 Da,
   declared `grid-fit:1e-6Da`). Deleted: `tof_index_field`/`tof_index_peak_schema`/`tof_grid_block`/
   `MzReconstruction`, the lattice half of `mz_lattice.rs` + `shimadzu_grid.rs`, `VendorHints`'
   point-layout fields, `write_spectrum_with_peak_arrays` in the vendored writer,
   `tests/shimadzu_lattice_peaks.rs`. KEPT on purpose: `tof_grid.rs` / `shimadzu_grid.rs`'s fits —
   they recover the VENDOR lattice (small indices, −7.5 … +6 %), where upstream's slot fit gives
   +52 … +94 %; converter-side model providers, not library deviations. The vendored reader keeps the
   0.9–0.13 point-grid decode path (`reconstruct_grid_mz`, `LinearMz`/`SqrtMzFromTof`) for one
   release so existing archives read; retire it once the corpus is rebuilt. Verified: SWATH
   `--tof-grid` 201/201 spectra, worst 6.4e-10 ppm vs 0.13.0; the Shimadzu/SCIEX Windows lanes
   compile-checked only until the box build.

**Plan (owner, 2026-09-23):** (a) fix the empty-facet reader bug (item below); (b) re-vendor
`vendor/mzpeak_prototyping` to a clean `eb08ba0` (mzdata 0.67.1 + `cv`), re-applying only the A/B/C
hunks of `delta-categorization.md` (D+E vanish), corpus as the semantic oracle; (c) integrate item 1 on
top; (d) commit, push, tag — an output-changing release (Shimadzu/Agilent/SciEX archives change layout,
SHA512 file-index checksums and term markers arrive with upstream) → corpus rebuild after.

- **TDF MzCalibration ModelType 2 (2026-09-26).** Converter side fixed for 0.14.0 (quadratic +
  declared bound; mzdata lanes on the chord). Open: (a) upstream mzdata `MzCalibrationModel2::try_from`
  accepts ModelType 2 and reads C3/C4 as cubic/shift — report with the formula and the OpenTIMS golden
  (item in the Joshua message); (b) a grid model that carries the calibrant polynomial (new CURIE,
  Joshua's call) so ModelType-2 runs can be exact; (c) a validator rule: decoded m/z outside
  `MzAcqRangeLower/Upper` (or the scan window) is an error — it would have caught 0.13.0's SBA415 at once;
  (d) SDK-verify SBA415 itself (`MZPC_TDF_SDK_GOLDEN` on the box) — only the OpenTIMS file is SDK-pinned.
- **Message to Joshua Klein — due 2026-09-23.** Draft: `~/Claude/mzPeak/output/vendor-audit-2026-09-23/
  MESSAGE-to-Joshua-2026-09-23.md`. Six findings, all verified in code or data: (1) `clamp_u32`
  saturates silently — a run-wide sqrt fit's negative indices (Agilent FM_01_Pos: 1.15 M points below
  `c0²`) become index 0, a 1e-9 lattice beyond 2³² becomes `u32::MAX−1`, no error; (2) is
  `indices: uint64` acceptable? (the exact-lattice case his grid cannot hold); (3) single-point chunks
  get `chunk_end = 0.0` (rows 131/294 of his own `diaPASEF.grid.mzpeak`; invisible to his range
  predicate); (4) grid-row bounds should be the model at the stored index, not the input value; (5) the
  Python reader (`grid.py:136-152`) still has the pre-#34 parameter order; (6) mzdata 0.67.1
  `convert_f6_wide` has a sign-flipped discriminant (scalar path unaffected). Plus the one proposal: a
  `WriterProperties` hook for BSS with dictionary off (measured −9.6 % vs his default on 2485; his
  DELTA intent on index lists would be +21.4 %).
2. **0.12.x ims-chunked TOF-boundary layout** (~500 lines). Still the intermediate the TDF lane writes
   before `src/tdf_grid.rs` rewrites it, and `--no-ims-grid`'s final form. Retire by emitting upstream's
   `ChunkingStrategy::Grid` natively with the exact `MzCalibration`/`TimsCalibration` models attached as
   array params — kills this and the rewrite pass together.
   **DONE (2026-09-24, unreleased → 0.14.0).** `bruker_native::ims_grid_arrays` builds each frame's
   m/z / 1/K0 through upstream's `TimsTofMzGrid2` / `TimsTofTimsLinearGrid2` models (Params on the
   arrays; per-frame `TdfMzCalibrationRow::grid_parameters(t1, t2)`, `TimsMobilityCalibration::
   grid_parameters()`); `write_ims_compact_archive_impl` uses `ChunkingStrategy::Grid` + tolerance-0
   policies on both facets, the schema sampled from the first frame. `src/tdf_grid.rs`, the TOF
   layout, the flat layout, `--no-ims-grid`/`--ims-grid`/`--grid-encoding` deleted; the SDK lane takes
   the same path (compile-checked only until the box/CI build). Inversion `to_index(from_index(k))`
   verified on every bin of 2485 and on the C2 ≠ 0 goldens. Remaining vendored deviations this leaves
   unused: `TofMzBoundary`/`add_raw_mz_boundary` (C7) and the chunk-facet `tof_chunk_*` decode path
   — reader side stays one release for 0.12.x archives, writer side can go (item 6).
3. **MZP provisional CV** (~150 lines). mzdata's `CURIE` cannot carry a foreign prefix and its `Display`
   panics on `Unknown`; the serialisers are inside the crate. Retire MZP: `tof_c0/c1` are unnecessary
   under the grid layout, SciEX/Agilent can write them as name-only userParams, request PSI terms for
   the two mobility-limit params (MZP:1000006/7). The honest upstream fix is
   `ControlledVocabulary::Other` in mzdata.
4. **Row-group and flush sizing.** The 16 MB row-group cap on every layout and the flush thresholds shape
   every point-layout archive's bytes; needs `WriteBatchConfig` knobs upstream. Until then a switch
   changes every archive's row groups.
5. **One-setter hooks upstream:** install a custom peak schema, a `spectra()` accessor, a peaks-facet
   stream sampler, one `pub use` (`EntryMetadataDerivedFromData`). Trivial PRs.
6. **Byte-comparability across the switch.** 25 output-affecting bug fixes / features of ours are in the
   published corpus; each must land upstream or archives change on the switch. Largest movers: the
   BSS/dictionary-off encoding policy (measured: upstream's DELTA intent would be +21 % on the grid
   facet) and the row-group sizing (item 4). Also to decide once: parquet 57 (upstream) vs 59 (ours).

Also found on the way, to report upstream: single-point chunks get `chunk_end = 0.0` (in Joshua's own file;
invisible to his range predicate); the Python reader still has the pre-#34 parameter order; mzdata 0.67.1's
`convert_f6_wide` has a sign-flipped discriminant; two panics on malformed grids; upstream's `mini_peak.rs`
chunked path repeats our old `size = chunks.len()` point-count bug.

## History

Everything this file used to contain — 23 numbered items with their analyses, measurements and
resolutions (grid CV terms, the generic grid facet, timsTOF mobility grids, the `tof` column
encoding, timsrust 5.1.x decompression, the performance section, the Agilent hosting mismatch) —
is preserved in git: `git show v0.9.12:BACKLOG.md`. Of those, #1–#3, #5–#7, #13–#14, #16–#21 were
done; #9 was regressed by merge `5a62b90` and restored in 0.11.0; #4, #8, #10–#12, #15, #22
are the deferred spec/research items listed here or superseded by the ledger.
