# Third-Party Notices

mzPeakConverter is distributed under the [MIT License](LICENSE). It builds on
third-party components, acknowledged here; every release archive carries this file. A
machine-readable inventory of every resolved dependency (version, license, source) is
attached to each [release](https://github.com/okohlbacher/mzPeakConverter/releases) as
`mzpeak-convert-<version>.cdx.json` (CycloneDX 1.5), generated from `Cargo.lock` by
`tools/gen_sbom.py` when the release is built.

## Vendored source — requires attention

| Component | Location | License |
|---|---|---|
| **`mzpeak_prototyping`** | `vendor/mzpeak_prototyping/` | **not declared upstream** |

`mzpeak_prototyping` is the HUPO-PSI reference mzPeak writer by Joshua Klein
(<https://github.com/mobiusklein>). A snapshot is **vendored** into this
repository because the converter extends it (the native integer-TOF seam, see
[`NATIVE-TOF-DESIGN.md`](NATIVE-TOF-DESIGN.md)) and pins it exactly to the
arrow/parquet/mzdata version graph.

> ⚠️ The upstream crate does not currently ship an explicit license file. It is
> redistributed here in good faith as the public PSI reference implementation;
> its terms remain those of the upstream author. If you redistribute
> mzPeakConverter, confirm the upstream license, or replace the vendored path
> dependency with an upstream git/crates.io dependency.

## Embedded .NET assemblies — requires attention

`thermorawfilereader` reads Thermo `.raw` files through a .NET bundle that its
`dotnetrawfilereader-sys` dependency embeds in the `mzpeak-convert` binary and unpacks at
runtime: `librawfilereader`, Thermo Fisher Scientific's RawFileReader assemblies
(`ThermoFisher.CommonCore.*`) and `OpenMcdf`. Both crates declare Apache-2.0 for their own
sources. The Thermo Fisher Scientific and OpenMcdf assemblies carry their own terms, which
neither crate states. If you redistribute mzPeakConverter, confirm those terms.

## Key runtime dependencies

| Crate | Role | License | Source |
|---|---|---|---|
| [`mzdata`](https://github.com/mobiusklein/mzdata) | format readers (mzML/imzML/Thermo/TDF) | Apache-2.0 | **git fork** [`okohlbacher/mzdata@1d53971`](https://github.com/okohlbacher/mzdata/commit/1d539717ecc7e9cec7b7d81cb7cbcfdf363b45db), pinned through `[patch.crates-io]`: 0.66.6 plus the isolation-window reader fix of [mobiusklein/mzdata#58](https://github.com/mobiusklein/mzdata/pull/58), merged upstream and not yet released |
| [`mzpeaks`](https://github.com/mobiusklein/mzpeaks) | peak/centroid models | Apache-2.0 | crates.io |
| [`arrow`](https://github.com/apache/arrow-rs), [`parquet`](https://github.com/apache/arrow-rs) | columnar storage | Apache-2.0 | crates.io |
| [`timsrust`](https://github.com/MannLabs/timsrust) | native Bruker TDF integer-TOF | Apache-2.0 | crates.io |
| [`thermorawfilereader`](https://github.com/mobiusklein/thermorawfilereader.rs) | Thermo `.raw` via .NET | Apache-2.0 | crates.io |
| [`rusqlite`](https://github.com/rusqlite/rusqlite) / [`zstd`](https://github.com/gyscos/zstd-rs) | Bruker TSF reader | MIT | crates.io |
| [`zip`](https://github.com/zip-rs/zip2), [`flate2`](https://github.com/rust-lang/flate2-rs) | archive + vendor-blob embedding | MIT; MIT OR Apache-2.0 | crates.io |
| [`clap`](https://github.com/clap-rs/clap), [`anyhow`](https://github.com/dtolnay/anyhow), [`serde`](https://github.com/serde-rs/serde) | CLI / errors / config | MIT OR Apache-2.0 | crates.io |

## Dependency licenses

Every other resolved Cargo dependency declares a permissive license (MIT, Apache-2.0 with or
without the LLVM exception, BSD-2-Clause, BSD-3-Clause, BSD-3-Clause-Clear, ISC, Zlib,
Unicode-3.0, 0BSD, CC0-1.0, MIT-0, Unlicense, BSL-1.0, bzip2-1.0.6 or CDLA-Permissive-2.0)
or a dual/triple combination of them; the release SBOM lists each crate's. None declares a
copyleft license (GPL/AGPL). `r-efi` (two versions resolved), the only crate offering an
optional `LGPL-2.1-or-later` alternative, is also offered under `MIT OR Apache-2.0`, which
this project takes. `mzpeak_prototyping` declares no license (above).

## Vendor instrument SDKs (not bundled)

The native readers for Bruker BAF and the timsdata SDK, Agilent (MHDAC), SciEX
(Clearcore2), Shimadzu (LabSolutions.IO) and Waters (MassLynx) call proprietary vendor
libraries that are **not** included in this repository or in any release archive. They are
loaded at runtime from a licensed vendor install (e.g. ProteoWizard), and their licenses are
governed by the respective vendors. The Windows release archive ships only this project's own
reflection-only .NET glue under `glue\`, which contains no vendor code.

## Apache License 2.0

The Apache-2.0 components above, among them `mzdata`, `mzpeaks`, `mzsignal`, `arrow`,
`parquet`, `timsrust` and `thermorawfilereader`, are used under the Apache License,
Version 2.0, reproduced at the end of this file. The `arrow` and `parquet` crates ship the
following NOTICE, which section 4(d) of that license requires redistributions to carry:

```text
Apache Arrow
Copyright 2016-2026 The Apache Software Foundation

This product includes software developed at
The Apache Software Foundation (http://www.apache.org/).

This product includes software from the chronoutil crate (MIT)
 * Copyright (c) 2020-2022 Oliver Margetts
 * https://github.com/olliemath/chronoutil

This product includes software from the compact-thrift project (Apache 2.0)
 * Copyright Jörn Horstmann
 * https://github.com/jhorstmann/compact-thrift
```

```text
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
```
