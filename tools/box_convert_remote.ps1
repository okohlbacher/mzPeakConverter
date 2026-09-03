# box_convert_remote.ps1 — runs ON the flash-workstation. Reads ONE job as JSON from stdin:
#   {"raw_url": "...", "put_url": "...", "opts": "--no-vendor", "archive": false, "converter": "..."}
# Downloads the raw from its URL, converts in an isolated temp dir, uploads the .mzpeak via the
# presigned PUT url, and prints base64(result-json) between <<<BOXRESULT / BOXRESULT>>> markers.
#
# Secret URLs (presigned PUT, and a possibly-authenticated raw URL) are written to curl -K config
# files inside the per-user temp dir, NEVER passed on curl's command line — so they don't appear in
# the box process table. The temp dir (raw + mzpeak + configs) is always removed (isolation+hygiene).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # keep CLIXML progress out of stdout

# ---- persistent raw cache helpers -------------------------------------------------------------
# A corpus rebuild re-downloaded all 16.9 GB of raw input every time (the per-job $work dir is
# deleted in `finally`). Measured: the box phase moved ~27 GB in ~40 min at ~63 Mbit/s while the
# converter itself ran for ~2 % of that wall clock — the job is transfer-bound, and 62 % of the
# bytes are raws that never change. So members are kept under a persistent cache root and re-fetched
# only when the host says they differ.
#
# The cache is a STAGING AREA, never the thing the converter opens: vendor libraries open the raw
# READ-WRITE (a Shimadzu .lcd getter was observed rewriting its own OLE2 container), so every job
# COPIES cache -> $work and converts the copy. No hardlinks, no in-place conversion.
#
# OPERATING THE CACHE. It is additive and never evicts, so a renamed or reorganised unit mints a new
# unit_key and strands the old directory. To force a refill of one unit, delete its <root>\<unit_key>
# directory; to reclaim everything, delete the whole root. Both are safe at any time a rebuild is not
# running -- a missing entry is simply re-downloaded. The default root is a DEDICATED subdirectory
# (...\rawcache\units) because C:\Users\User\rawcache already held ~20 GB written by an unrelated
# June 2026 session; nothing here ever deletes outside its own root. Per-unit lock files live in
# <root>\.locks\ so they cannot be mistaken for unit directories.

function Get-CacheUnixTime {   # epoch seconds (int64) -> DateTime in UTC
    param([int64]$Epoch)
    return ([DateTimeOffset]::FromUnixTimeSeconds($Epoch)).UtcDateTime
}

function Test-SafeRel {        # a member `rel` that can only ever land INSIDE the root it is joined to
    # `rel` is s3_relay's key-minus-parent, i.e. bucket-controlled text. Before the cache it was only
    # ever joined onto $work, which `finally` deletes; it is now also joined onto a PERSISTENT root,
    # where a '..' segment or a drive letter would write somewhere nothing ever cleans up. Checked on
    # the string itself so it holds for every root, and applied to all members (the $work joins had
    # the same hole, just a bounded one).
    param([string]$Rel)
    if ([string]::IsNullOrWhiteSpace($Rel)) { return $false }
    if ($Rel -match '^[A-Za-z]:' -or $Rel[0] -eq '\' -or $Rel[0] -eq '/') { return $false }   # rooted
    foreach ($seg in ($Rel -split '[\\/]')) { if ($seg -eq '..') { return $false } }
    return $true
}

function Test-CachedMember {   # cached copy matches the declared length, mtime and (if known) content?
    param($Root, $Member, [bool]$VerifyEtag)
    if ($null -eq $Member.size -or $null -eq $Member.mtime) { return $false }   # host declared nothing
    $p = Join-Path $Root $Member.rel
    if (-not (Test-Path -LiteralPath $p -PathType Leaf)) { return $false }
    try { $fi = Get-Item -LiteralPath $p } catch { return $false }
    if ($fi.Length -ne [int64]$Member.size) { return $false }
    # 2 s slack: we set this stamp ourselves from whole epoch seconds, so this only absorbs
    # filesystem timestamp granularity. A false miss costs one re-download, never a wrong result.
    if ([math]::Abs(($fi.LastWriteTimeUtc - (Get-CacheUnixTime ([int64]$Member.mtime))).TotalSeconds) -ge 2) { return $false }
    # CONTENT check. Size+mtime alone would make this the first published-corpus input the pipeline
    # never verifies: before the cache the raw was re-fetched from S3 every run, so S3 was implicitly
    # authoritative; now it can be a file that has sat on a shared C: for months. A single-part PUT's
    # ETag IS the body md5 (the same identity the archive side already trusts at box_convert.sh's
    # `head --etag` gate), so hashing a HIT closes the gap. ~16.9 GB at Get-FileHash speed is well
    # under a minute against the ~25 min the cache saves. A mismatch is a MISS, never a throw: the
    # member is simply re-downloaded, so a corrupted or foreign-written entry self-corrects.
    # Multipart ETags ("<md5>-<n>") are not body hashes; the host omits those and we fall through.
    if ($VerifyEtag -and $Member.etag) {
        try { $h = (Get-FileHash -LiteralPath $p -Algorithm MD5).Hash.ToLower() } catch { return $false }
        if ($h -ne ([string]$Member.etag).ToLower()) { return $false }
    }
    return $true
}

function Open-CacheLock {      # exclusive per-unit lock; $null on timeout (caller degrades, never waits forever)
    param($Path, [int]$TimeoutSec)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        try {
            return [System.IO.File]::Open($Path, [System.IO.FileMode]::OpenOrCreate,
                                          [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        } catch {
            if ((Get-Date) -ge $deadline) { return $null }
            Start-Sleep -Seconds 2
        }
    }
}

# Vendor SDK environment the converter needs for .wiff/.raw/.d (box-specific paths). Override any of
# these by creating C:\Users\User\box_convert_env.ps1 (dot-sourced last if present).
$env:DOTNET_ROOT = 'C:\Users\User\dotnet8'; $env:DOTNET_ROLL_FORWARD = 'LatestMajor'
$cvtRoot = 'C:\Users\User\src\mzPeakConverter'
# ProteoWizard install that supplies every vendor reader. Switched 2026-09-03 from the FLASHApp
# bundle to a STANDALONE pwiz: the bundle carries Shimadzu.LabSolutions.IO 3.8.4.6016, which returns
# centroid intensities MISALIGNED against their m/z on profile-less .lcd files (shifted 1-7
# positions, final peak dropped) — the defect this project spent a week documenting as unfixable.
# The standalone 3.0.26151 ships 5.0.0.0 of the same library, which reads them CORRECTLY: our native
# lane against it reproduces the LabSolutions export exactly on DIA_Hela_20ng (612/14,300/13,558/
# 11,361 peaks, max |dintensity| = 0, m/z to 2e-13). It also matches the bundle on every other
# vendor library we use (Clearcore/SCIEX, MassLynx, baf2sql, timsdata, Thermo, MHDAC); the only
# thing it lacks is unifi_protobuf_net.dll, and every UNIFI corpus unit is an mzML input.
# NOTE the two builds lay Agilent out differently (bundle: vendor_api\Agilent; installer: flattened)
# — the converter probes both since `agilent_dll_dir` (src/agilent.rs).
$pwiz = 'C:/Users/User/AppData/Local/Apps/ProteoWizard 3.0.26151.e7e989c 64-bit'
if (Test-Path "$cvtRoot\glue\sciex\bin\Release\net8.0")  { $env:MZPC_SCIEX_GLUE  = (Resolve-Path "$cvtRoot\glue\sciex\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\waters\bin\Release\net8.0") { $env:MZPC_WATERS_GLUE = (Resolve-Path "$cvtRoot\glue\waters\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\agilent\bin\Release\net48") { $env:MZPC_AGILENT_GLUE = (Resolve-Path "$cvtRoot\glue\agilent\bin\Release\net48").Path }  # net48 AgilentGlueHost.exe (MHDAC needs .NET FW)
if (Test-Path "$cvtRoot\glue\shimadzu\bin\Release\net8.0") { $env:MZPC_SHIMADZU_GLUE = (Resolve-Path "$cvtRoot\glue\shimadzu\bin\Release\net8.0").Path }  # native Shimadzu .lcd (LabSolutions.IO)
$env:MZPC_PWIZ_DIR = $pwiz; $env:MZPC_MASSLYNX_DIR = $pwiz   # MHDAC for Agilent loads from $pwiz/vendor_api/Agilent
# --via-msconvert resolves msconvert via $MSCONVERT_PATH; pin it to THIS pwiz so it has the bundled
# vendor readers (vendor_api/Agilent etc.) — else it grabs a msconvert on PATH that lacks them ([ReaderFail]).
if (Test-Path "$pwiz/msconvert.exe") { $env:MSCONVERT_PATH = (Join-Path $pwiz 'msconvert.exe') }
if (Test-Path "$pwiz/timsdata.dll") { $env:TIMSDATA_LIB_DIR = $pwiz }   # --bruker-sdk loads timsdata.dll from pwiz-bin
if (Test-Path 'C:\Users\User\box_convert_env.ps1') { . 'C:\Users\User\box_convert_env.ps1' }

$job = [Console]::In.ReadToEnd() | ConvertFrom-Json
$work = Join-Path $env:TEMP ("bxc-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$res = [ordered]@{ stage='init'; exit=1; uploaded=$false; size=0; md5=''; log=''; error=''; note='';
                   dl_s=0; msconv_s=0; conv_s=0; up_s=0; raw_bytes=0 }
$cacheLock = $null   # released as soon as the unit is copied out; the `finally` is only a backstop

try {
    $converter = if ($job.converter) { $job.converter } else { 'C:\Users\User\src\mzPeakConverter\target\release\mzpeak-convert.exe' }
    if (-not (Test-Path $converter)) { throw "converter not found: $converter" }

    # 1. download the raw — urls go in a -K config file, never argv (they are presigned secrets)
    $res.stage = 'download'
    $dlcfg = Join-Path $work 'dl.cfg'
    $unitBytes = 0
    $swdl = [Diagnostics.Stopwatch]::StartNew()
    if ($job.raw_urls) {
        # S3-FIRST, multi-object unit. The members are ALREADY in the corpus bucket, so the host
        # sends a member list of presigned GETs and the box reassembles the unit itself. This exists
        # because the alternative -- host tars the local copy and re-uploads it to the same bucket --
        # ran at the host's ~0.5 MB/s uplink and was 86% of box-job wall clock. `rel` is relative to
        # the unit's PARENT, so a .d unit lands as unit\<name>.d\... and a .wiff lands beside its
        # .wiff.scan; `primary` is what the converter is handed.
        $unitRoot = Join-Path $work 'unit'
        New-Item -ItemType Directory -Force -Path $unitRoot | Out-Null

        # Bucket-controlled `rel` must not be able to name a path outside the root it is joined to.
        # Checked BEFORE anything is created, for every member, so a bad unit dies here rather than
        # half-written into a persistent cache.
        foreach ($m in $job.raw_urls) {
            if (-not (Test-SafeRel $m.rel)) { throw "unsafe unit member path: $($m.rel)" }
        }

        # PERSISTENT RAW CACHE (see the helpers at the top of this file).
        #   default                  -> C:\Users\User\rawcache\units
        #   value 'off'              -> no cache at all; download straight into $work, exactly as
        #                               before this existed, and $work is still deleted in `finally`.
        # SETTABLE FROM THE HOST. These arrive in the JOB JSON, because nothing forwards environment
        # over the ssh invocation (no SendEnv, and Windows sshd does not accept env by default) — a
        # host-side `MZPC_BOX_RAW_CACHE=off box_convert.sh ...` would otherwise be silently ignored,
        # leaving the one kill-switch for this feature reachable only by hand-authoring a file on the
        # box. The $env: forms remain as a box-LOCAL override for an interactive session.
        # $job.unit_key is minted HOST-side from the corpus-relative key (s3_relay presign-unit) —
        # never derived here, because everything the box sees of a member is a presigned URL whose
        # signature is different on every run. What actually guarantees the key cannot escape the
        # cache root is that s3_relay always appends '-<sha1[:12]>', so it can never BE '.' or '..';
        # the pattern below additionally requires a leading alphanumeric and bans path separators.
        $cacheRoot = if ($job.raw_cache) { [string]$job.raw_cache }
                     elseif ($env:MZPC_BOX_RAW_CACHE) { $env:MZPC_BOX_RAW_CACHE }
                     else { 'C:\Users\User\rawcache\units' }
        $ukey      = if ($job.unit_key) { [string]$job.unit_key } else { '' }
        $useCache  = ($cacheRoot -ne 'off') -and ($ukey -match '^[A-Za-z0-9][A-Za-z0-9._-]{0,119}$')
        $dlRoot    = $unitRoot
        $unitCache = $null
        $wanted    = @($job.raw_urls)
        if ($useCache) {
            # Content-verify a cache HIT unless explicitly switched off (see Test-CachedMember).
            $cacheVerify = $true
            $cvRaw = if ($null -ne $job.raw_cache_verify) { [string]$job.raw_cache_verify } else { [string]$env:MZPC_BOX_CACHE_VERIFY }
            if ($cvRaw -and ($cvRaw -eq '0' -or $cvRaw -eq 'off')) { $cacheVerify = $false }
            $unitCache = Join-Path $cacheRoot $ukey
            New-Item -ItemType Directory -Force -Path $unitCache | Out-Null
            # One writer per unit. Jobs never share a unit today, but a manifest that repeats one
            # would otherwise have two curls writing the same member. A timeout DEGRADES to the
            # uncached path rather than blocking the pool slot indefinitely. Locks live in a
            # subdirectory so the root holds nothing but unit dirs (a stale 0-byte lock is harmless;
            # deleting one on release would race a process that has just opened it).
            $lockWait = 1800
            $lwRaw = if ($job.raw_cache_lock_wait) { [string]$job.raw_cache_lock_wait } else { [string]$env:MZPC_BOX_CACHE_LOCK_WAIT }
            if ($lwRaw -match '^\d+$') { $lockWait = [int]$lwRaw }
            $lockDir = Join-Path $cacheRoot '.locks'
            New-Item -ItemType Directory -Force -Path $lockDir | Out-Null
            $cacheLock = Open-CacheLock (Join-Path $lockDir ($ukey + '.lock')) $lockWait
            if ($cacheLock) {
                $dlRoot = $unitCache
                $wanted = @($job.raw_urls | Where-Object { -not (Test-CachedMember $unitCache $_ $cacheVerify) })
            } else {
                $useCache = $false
                $unitCache = $null
                $res.note = ((@($res.note, 'rawcache=lock-timeout') | Where-Object { $_ }) -join ' ')
            }
        }

        if ($wanted.Count -gt 0) {
            $lines = New-Object System.Collections.Generic.List[string]
            foreach ($m in $wanted) {
                # forward slashes: curl's -K parser treats a backslash as an escape inside quotes
                $dest = ((Join-Path $dlRoot $m.rel) -replace '\\', '/')
                $lines.Add('url = "' + $m.url + '"')
                $lines.Add('output = "' + $dest + '"')
            }
            Set-Content -LiteralPath $dlcfg -Value $lines -Encoding ASCII
            & curl.exe -fsS --retry 3 --retry-delay 5 --create-dirs --parallel --parallel-max 8 -K $dlcfg
            if ($LASTEXITCODE -ne 0) {
                & curl.exe -fsS --retry 3 --retry-delay 5 --create-dirs -K $dlcfg   # no --parallel (curl < 7.66)
                if ($LASTEXITCODE -ne 0) { throw "unit download failed (curl exit $LASTEXITCODE)" }
            }
        }

        if ($useCache) {
            # Stamp the declared mtime ONLY after curl reported success, so a truncated member keeps
            # its own timestamp, fails Test-CachedMember next run, and is re-fetched.
            foreach ($m in $wanted) {
                $p = Join-Path $unitCache $m.rel
                if ((Test-Path -LiteralPath $p -PathType Leaf) -and $null -ne $m.mtime) {
                    (Get-Item -LiteralPath $p).LastWriteTimeUtc = (Get-CacheUnixTime ([int64]$m.mtime))
                }
            }
            foreach ($m in $job.raw_urls) {   # the cache now holds a COMPLETE unit, or we do not proceed
                $p = Join-Path $unitCache $m.rel
                if (-not (Test-Path -LiteralPath $p -PathType Leaf)) { throw "cache member missing after download: $($m.rel)" }
                if ($null -ne $m.size -and (Get-Item -LiteralPath $p).Length -ne [int64]$m.size) {
                    throw "cache member size mismatch: $($m.rel)"
                }
            }
            # COPY into the per-job temp dir. Deliberately not a hardlink and never converted in
            # place: the converter's vendor backends open the raw read-write. Local copy runs at
            # ~1 GB/s (~3 s for the largest unit) against ~60 s to re-download it.
            foreach ($m in $job.raw_urls) {
                $dst = Join-Path $unitRoot $m.rel
                $dstDir = Split-Path -Parent $dst
                if ($dstDir -and -not (Test-Path -LiteralPath $dstDir)) {
                    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
                }
                Copy-Item -LiteralPath (Join-Path $unitCache $m.rel) -Destination $dst -Force
            }
            $res.note = ((@($res.note, "rawcache=$($job.raw_urls.Count - $wanted.Count)/$($job.raw_urls.Count) cached") | Where-Object { $_ }) -join ' ')
            $cacheLock.Dispose(); $cacheLock = $null   # convert without holding the unit's lock
        }
        $rawName = $job.primary
        $rawPath = Join-Path $unitRoot $rawName
        if (-not (Test-Path -LiteralPath $rawPath)) { throw "unit primary missing after download: $rawName" }
        $got = @(Get-ChildItem -LiteralPath $unitRoot -Recurse -File)
        if ($got.Count -lt $job.raw_urls.Count) {
            throw "unit incomplete: got $($got.Count) of $($job.raw_urls.Count) member(s)"
        }
        $unitBytes = ($got | Measure-Object Length -Sum).Sum
    } else {
        $rawName = [IO.Path]::GetFileName(([Uri]$job.raw_url).AbsolutePath)
        if (-not $rawName) { $rawName = 'input.bin' }
        $rawPath = Join-Path $work $rawName
        Set-Content -LiteralPath $dlcfg -Value ('url = "' + $job.raw_url + '"') -Encoding ASCII
        & curl.exe -fSL --retry 3 --retry-delay 5 -K $dlcfg -o $rawPath
        if ($LASTEXITCODE -ne 0) { throw "raw download failed (curl exit $LASTEXITCODE)" }
    }
    $res.dl_s = [math]::Round($swdl.Elapsed.TotalSeconds, 1)

    # 2. archive (multi-file vendor formats) -> extract + pick the unit deterministically
    $isArchive = $job.archive -or ($rawName -match '\.(zip|tgz|tar\.gz)$')
    if ($isArchive) {
        $res.stage = 'extract'
        $ex = Join-Path $work 'unpacked'; New-Item -ItemType Directory -Force -Path $ex | Out-Null
        & tar.exe -xf $rawPath -C $ex   # bsdtar on Win10+ handles .zip and .tar.gz
        if ($LASTEXITCODE -ne 0) { throw "extract failed (tar exit $LASTEXITCODE)" }
        $order = @('.d','.wiff','.raw','.imzml','.mzml')   # preference order
        $units = @()
        # vendor dirs: Bruker/Agilent .d AND Waters .raw (a directory, not a file).
        # A vendor dir counts as a unit ONLY if it holds the vendor payload. Some archives nest the
        # real unit inside a WRAPPER dir that also ends in .d (e.g. MSV000101607:
        # `Blank_Try.d/Blank(1) Try_Slot1-1_1_8270.d/analysis.tdf`). Without this check the wrapper
        # wins the FullName sort below (it is a path prefix of the nested unit) and the converter is
        # handed a directory with no vendor data -> native read fails, msconvert fails, job lost.
        $hasPayload = {
            param($dir)
            $markers = @('analysis.tdf','analysis.tsf','analysis.baf','_HEADER.TXT','MSScan.bin','msprofile.bin')
            foreach ($mk in $markers) { if (Test-Path -LiteralPath (Join-Path $dir.FullName $mk)) { return $true } }
            # Agilent keeps its payload under AcqData\; Waters uses _FUNC*.DAT
            if (Test-Path -LiteralPath (Join-Path $dir.FullName 'AcqData')) { return $true }
            if (Get-ChildItem -LiteralPath $dir.FullName -Filter '_FUNC*.DAT' -File -ea SilentlyContinue | Select-Object -First 1) { return $true }
            return $false
        }
        $vendorDirs = @(Get-ChildItem -Path $ex -Recurse -Directory | Where-Object { @('.d','.raw') -contains $_.Extension.ToLower() })
        $withPayload = @($vendorDirs | Where-Object { & $hasPayload $_ })
        # Prefer dirs that actually hold vendor data; fall back to the old behaviour if none matched
        # (unknown vendor layout) so this can never make a previously-working archive unconvertible.
        if ($withPayload.Count -gt 0) {
            if ($withPayload.Count -lt $vendorDirs.Count) {
                $res.note = ((@($res.note, "skipped $($vendorDirs.Count - $withPayload.Count) wrapper dir(s) w/o vendor payload") | Where-Object { $_ }) -join ' ')
            }
            $units += $withPayload
        } else {
            $units += $vendorDirs
        }
        $units += Get-ChildItem -Path $ex -Recurse -File | Where-Object { $order -contains $_.Extension.ToLower() }
        $units = $units | Where-Object { $_.Name -notlike '._*' }   # drop macOS AppleDouble junk
        if (-not $units -or $units.Count -eq 0) { throw "no convertible unit (.d/.wiff/.raw/.imzML/.mzML) in archive" }
        $unit = $units | Sort-Object @{ Expression = { $order.IndexOf($_.Extension.ToLower()) } }, FullName | Select-Object -First 1
        if ($units.Count -gt 1) { $res.note = "archive held $($units.Count) units; picked $($unit.Name)" }
        $inputPath = $unit.FullName
    } else {
        $inputPath = $rawPath
    }

    # 3. convert (capture every stream + the converter's own exit code)
    # 2b. BENCH: measure raw footprint + msconvert->mzML size (reported via note; does not affect the mzpeak upload)
    try {
        $ri = Get-Item -LiteralPath $inputPath
        $rawSize = if ($unitBytes) { $unitBytes } elseif ($ri.PSIsContainer) { (Get-ChildItem -LiteralPath $inputPath -Recurse -File | Measure-Object Length -Sum).Sum } else { $ri.Length }
        $res.raw_bytes = $rawSize
        $mzmlSize = 0
        # OPT-IN size-bench only: running msconvert here purely to MEASURE the mzML size doubles the
        # work of every --via-msconvert job (a full multi-GB mzML write, then discarded). Gate it so
        # production conversions never pay for it. Set MZPC_BENCH_MZML=1 for compression-bench runs.
        if ($env:MZPC_BENCH_MZML -eq '1' -and $env:MSCONVERT_PATH -and (Test-Path $env:MSCONVERT_PATH)) {
            $mzmlDir = Join-Path $work 'mzml'; New-Item -ItemType Directory -Force -Path $mzmlDir | Out-Null
            $swmz = [Diagnostics.Stopwatch]::StartNew()
            & $env:MSCONVERT_PATH $inputPath --mzML -o $mzmlDir *> (Join-Path $work 'msconvert.log')
            $res.msconv_s = [math]::Round($swmz.Elapsed.TotalSeconds, 1)
            $mz = Get-ChildItem -Path $mzmlDir -Filter *.mzML -ErrorAction SilentlyContinue | Sort-Object Length -Descending | Select-Object -First 1
            if ($mz) { $mzmlSize = $mz.Length }
        }
        $res.note = ((@($res.note, "raw=$rawSize", "mzml=$mzmlSize", "msconv_s=$($res.msconv_s)") | Where-Object { $_ }) -join ' ')
    } catch { $res.note = ((@($res.note, "benchmeas-fail") | Where-Object { $_ }) -join ' ') }

    $res.stage = 'convert'
    $out = Join-Path $work 'out.mzpeak'
    $log = Join-Path $work 'convert.log'
    $optList = @()
    if ($job.opts -and $job.opts.Trim()) { $optList = @([regex]::Split($job.opts.Trim(), '\s+') | Where-Object { $_ -ne '' }) }
    # --byte-plane-intensity is a converter ENV toggle (MZPC_BYTE_PLANE_INTENSITY), not a CLI flag —
    # lift it out of the opts so it can be driven per-job through box_convert.
    if ($optList -contains '--byte-plane-intensity') {
        $env:MZPC_BYTE_PLANE_INTENSITY = '1'
        $optList = @($optList | Where-Object { $_ -ne '--byte-plane-intensity' })
    }
    # mzML intermediate -> RAMDISK when configured. The converter writes any msconvert mzML to
    # $env:TEMP (Rust std::env::temp_dir), so pointing TEMP/TMP at a RAM-backed volume keeps the giant
    # intermediate off disk. Set MZPC_MZML_TMPDIR to a ramdisk in C:\Users\User\box_convert_env.ps1.
    $mzmlTmp = $null
    if ($env:MZPC_MZML_TMPDIR -and (Test-Path $env:MZPC_MZML_TMPDIR)) {
        $env:TMP = $env:MZPC_MZML_TMPDIR; $env:TEMP = $env:MZPC_MZML_TMPDIR
        $mzmlTmp = $env:MZPC_MZML_TMPDIR
        $res.note = ((@($res.note, 'mzmltmp=ram') | Where-Object { $_ }) -join ' ')
    }
    # Continue around the native call: the converter logs INFO to stderr on SUCCESS, which would
    # otherwise raise a NativeCommandError under 'Stop' and mask a clean run. Exit is read explicitly.
    $prevEAP = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    # msconvert is FALLBACK-ONLY, never the primary path. Windows has a native reader for every vendor
    # (Thermo/.NET, Bruker TDF+BAF, Agilent MHDAC/grid, SciEX Clearcore2, Waters MassLynx, Shimadzu
    # LabSolutions) and native is both faster and smaller (measured: Waters 946->163 MB, SciEX SWATH
    # 1242->495 s and 2891->2040 MB; msconvert inflates the mzML round-trip). So the PRIMARY attempt is
    # ALWAYS native: strip the mzML-lane flags (--via-msconvert, --tof-grid <mode>) so even a job that
    # still asks for --via-msconvert is tried native first. msconvert runs ONLY if the native read fails.
    $nativeOpts = @(); $skipNext = $false
    foreach ($o in $optList) {
        if ($skipNext) { $skipNext = $false; continue }
        if ($o -eq '--via-msconvert') { continue }
        if ($o -eq '--tof-grid') { $skipNext = $true; continue }   # drop the flag AND its mode arg
        $nativeOpts += $o
    }
    $swcv = [Diagnostics.Stopwatch]::StartNew()
    & $converter $inputPath @nativeOpts -o $out --force *> $log
    $res.exit = $LASTEXITCODE
    if ($res.exit -eq 0) {
        $res.note = ((@($res.note, 'path=native') | Where-Object { $_ }) -join ' ')
    } else {
        # native failed on this file/model -> msconvert FALLBACK (the ONLY place msconvert ever runs)
        $res.note = ((@($res.note, 'path=native-fail->msconvert') | Where-Object { $_ }) -join ' ')
        # RAMDISK GUARD. box_convert.sh's job cap is justified against C: free space, but when
        # MZPC_MZML_TMPDIR points TEMP at a RAM-backed volume the mzML intermediate lands in the
        # box's 63 GB of RAM instead — and this fallback is the one path that writes a multi-tens-of-GB
        # mzML. Several concurrent fallbacks would fill it and stall mid-write, which looks nothing
        # like "too much concurrency". Fail loudly instead. 4x raw is a floor, not a guarantee:
        # msconvert's mzML is typically several times the vendor raw for profile data.
        if ($mzmlTmp) {
            $need = 4 * [int64]$res.raw_bytes
            $free = $null
            try { $free = (Get-PSDrive -Name ((Split-Path -Qualifier $mzmlTmp).TrimEnd(':')) -ErrorAction Stop).Free } catch { $free = $null }
            if ($null -ne $free -and $need -gt 0 -and $free -lt $need) {
                throw ("msconvert fallback needs ~$([math]::Round($need/1GB,1)) GB on the mzML tmpdir " +
                       "$mzmlTmp but only $([math]::Round($free/1GB,1)) GB is free. That volume is a " +
                       "ramdisk (MZPC_MZML_TMPDIR); size the concurrency to it (MZPC_BOX_JOBS_CAP=1) " +
                       "or unset MZPC_MZML_TMPDIR so the intermediate goes to disk.")
            }
        }
        $native_only = @('--agilent-grid', '--bruker-sdk')   # conflict with / ignored by the mzML lane
        $fbOpts = @($nativeOpts | Where-Object { $native_only -notcontains $_ }) + @('--via-msconvert', '--tof-grid', 'auto')
        & $converter $inputPath @fbOpts -o $out --force *>> $log
        $res.exit = $LASTEXITCODE
    }
    $res.conv_s = [math]::Round($swcv.Elapsed.TotalSeconds, 1)
    $ErrorActionPreference = $prevEAP
    if (Test-Path $log) { $res.log = (Get-Content $log -Raw) }

    # upload ONLY a clean success (exit 0 AND output present) — never ship a failed/partial archive
    if ($res.exit -eq 0 -and (Test-Path $out)) {
        $res.size = (Get-Item $out).Length
        $res.md5 = (Get-FileHash $out -Algorithm MD5).Hash.ToLower()
        if ($res.size -gt 5GB) { $res.stage = 'too-big'; throw "mzpeak $($res.size) B exceeds the 5 GB single-PUT limit" }
        $res.stage = 'upload'
        $upcfg = Join-Path $work 'up.cfg'
        Set-Content -LiteralPath $upcfg -Value ('url = "' + $job.put_url + '"') -Encoding ASCII
        $swup = [Diagnostics.Stopwatch]::StartNew()
        & curl.exe -fS -X PUT --upload-file $out -K $upcfg
        if ($LASTEXITCODE -ne 0) { throw "upload failed (curl exit $LASTEXITCODE)" }
        $res.up_s = [math]::Round($swup.Elapsed.TotalSeconds, 1)
        $res.uploaded = $true
        $res.stage = 'done'
    } elseif ($res.exit -eq 0) {
        $res.stage = 'no-output'   # exit 0 but nothing written — treat as failure, don't ship
    } else {
        $res.stage = 'convert-failed'
    }
}
catch {
    $res.error = $_.Exception.Message
}
finally {
    # Normal path already released this right after the cache->$work copy; this only fires when the
    # job threw between acquiring it and copying out. Never leave a unit locked against later runs.
    if ($cacheLock) { try { $cacheLock.Dispose() } catch { } ; $cacheLock = $null }
    # $work only. The raw cache is deliberately PERSISTENT and additive: 16.9 GB against 2,944 GB
    # free needs no eviction, and a stale entry self-corrects through the size+mtime+ETag check.
    # It never evicts, though, so a reorganised corpus strands old unit dirs — see the operating
    # note at the top of this file for how to purge one unit or the whole root.
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    if ($res.log.Length -gt 16384) { $res.log = $res.log.Substring($res.log.Length - 16384) }  # tail only
    $json = $res | ConvertTo-Json -Compress -Depth 4
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))   # marker-collision-proof
    Write-Output "<<<BOXRESULT"
    Write-Output $b64
    Write-Output "BOXRESULT>>>"
}
