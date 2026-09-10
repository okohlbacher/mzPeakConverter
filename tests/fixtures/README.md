# Test fixtures

Small slices of public acquisitions, committed so that the tests which need them run on every
platform — CI included — without the reference corpus. Each keeps only the members its test reads;
binary members a test merely lists and digests are empty stand-ins.

| Fixture | Source | Kept | Used by |
|---|---|---|---|
| `swath.api-sample-centroid.mzML.gz` | ProteoWizard `vendor_readers` test data: its reference mzML of `swath.api.wiff2` (SCIEX X500R QTOF SWATH), gzipped | the whole file | `tests/gridded_spectrum_summaries.rs`, `tests::tof_grid_subpath_embeds_sdrf` |
| `lee_maire_s25_acqdata/` | MetaboLights MTBLS14741, `240319-LL-LeeMaire_c18-isoflavone-pos-S25.d/AcqData` | `MSScan.bin`, `MSScan.xsd`, `MSTS.xml` | `agilent_profile::tests::agilent_scan_records_carry_the_vendor_polarity` |
| `20181203_Capan2_1.raw/` | MetaboLights MTBLS812 | `_HEADER.TXT`, `_extern.inf`; empty `_FUNC001.DAT` | `waters_meta::tests::capan2_states_model_time_and_members` |
| `blind_file_property.bin` | MetaboLights MTBLS13204: the root `File Property` stream of `Blind_P1_pos_012.lcd` | that stream (8 KB of 55 MB) | `shimadzu_meta::tests::blind_states_a_zoned_start_sample_and_labsolutions_version` |
| `blank1.D/` | MetaboLights MTBLS11742, `blank1.D/AcqData` | `Devices.xml`, `Contents.xml`; empty `MSScan.bin`; plus an empty AppleDouble `._MSScan.bin`, added for the test (not in the source), that the member walk must skip | `agilent_meta::tests::blank1_gc_ms_states_model_serial_time_and_members` |
| `mtbls243.d/` | MetaboLights MTBLS243, `03_D24062013T1259_1399CBU_01QC_A3.d/AcqData` | `Devices.xml` | `agilent_meta::tests::numeric_serial_stays_a_string` |

Each fixture stays under its source's terms: ProteoWizard's repository is Apache-2.0, and the MetaboLights
slices come from public studies under EMBL-EBI's terms of use for MetaboLights.

The tests that still need the corpus are `#[ignore]`d, so CI reports them as not run rather than as
passed. Run them with `MZPEAK_CORPUS=<corpus data root> cargo test --release -- --include-ignored`,
and add `MZPC_REQUIRE_CORPUS=1` to make a missing fixture fail instead of skip (`tests/common/corpus.rs`).
