# lane_pairs.ps1 — build a NATIVE and an mzML (`--via-msconvert`) archive from the SAME source, so
# `tests/lane_metadata_parity.rs` can compare what the two lanes carry.
#
# Runs ON THE WINDOWS BOX: the native vendor readers exist nowhere else, so the pairs cannot be
# built on the development host. Sources come from the box's raw cache (populated by the corpus
# harness) or from any directory you name.
#
#   powershell -NoProfile -File lane_pairs.ps1
#   powershell -NoProfile -File lane_pairs.ps1 -Units 'Blind_P1_pos_012.lcd,En_PPY.wiff@2' -OutDir D:\pairs
#
# A multi-sample SciEX .wiff is refused by BOTH lanes without `--sample` (an archive is one run), so
# name the sample with an `@N` suffix (1-based, as `--sample N`): 'En_PPY.wiff@2' builds
# `En_PPY.sample2.native.mzpeak` and `En_PPY.sample2.mzml.mzpeak` from sample 2. Under -File an array
# argument arrives as ONE string, so -Units is also split on commas.
#
# Then copy the `<stem>.native.mzpeak` / `<stem>.mzml.mzpeak` files to the host and point the test
# at their directory:
#
#   MZPC_LANE_PAIRS=<dir> cargo test --release --test lane_metadata_parity -- --nocapture
#
# Both lanes run with `--no-vendor`: vendor embedding is the same code on both, and it would
# dominate the transfer. A lane that REFUSES a unit by design (Agilent MRM/SIM dwell data, an
# IM-QTOF run) is reported and leaves no archive — that unit simply has no pair, which is correct.
param(
    [string[]]$Units  = @('Blind_P1_pos_012.lcd', 'En_PPY.wiff@1', 'IPX0002633001_D-239.wiff@1', 'blank1.D'),
    [string]  $OutDir = 'C:\Users\User\lane-pairs',
    [string]  $Cache  = 'C:\Users\User\rawcache\units',
    [string[]]$AlsoSearch = @('C:\Users\User\mzpc-agilent-gate'),
    [switch]  $Rebuild
)
$ErrorActionPreference = 'Continue'
$repo = 'C:\Users\User\src\mzPeakConverter'
$conv = "$repo\target\release\mzpeak-convert.exe"
if (-not (Test-Path $conv)) { throw "converter not found: $conv" }

# Same vendor-SDK environment the corpus harness sets (tools/box_convert_remote.ps1).
$env:MZPC_AGILENT_GLUE  = "$repo\glue\agilent\bin\Release\net48"
$env:MZPC_SCIEX_GLUE    = "$repo\glue\sciex\bin\Release\net8.0"
$env:MZPC_SHIMADZU_GLUE = "$repo\glue\shimadzu\bin\Release\net8.0"
$pwiz = 'C:\Users\User\AppData\Local\Apps\ProteoWizard 3.0.26175.31fd1ca 64-bit'
$env:MZPC_PWIZ_DIR = $pwiz; $env:MZPC_MASSLYNX_DIR = $pwiz
if (Test-Path "$pwiz\msconvert.exe") { $env:MSCONVERT_PATH = "$pwiz\msconvert.exe" }
if (Test-Path "$pwiz\timsdata.dll")  { $env:TIMSDATA_LIB_DIR = $pwiz }
# The Agilent host materialises the whole run at 16 B/point: keep it on disk, not on a ramdisk TEMP.
$env:MZPC_AGILENT_TMPDIR = $env:TEMP

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
& $conv --version

$Units = @($Units | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
foreach ($spec in $Units) {
    $want = $spec; $sample = ''
    if ($spec -match '^(.+)@(\d+)$') { $want = $Matches[1]; $sample = $Matches[2] }
    $stem = [IO.Path]::GetFileNameWithoutExtension($want)
    if ($sample) { $stem = "$stem.sample$sample" }
    $unit = $null
    # The cache names each unit '<truncated-name>-<sha1[:12]>', so match on a prefix, then find the
    # member that IS the unit (a .d/.raw directory, or a .lcd/.wiff file).
    if (Test-Path $Cache) {
        $pre = $want.Substring(0, [Math]::Min(24, $want.Length))
        $dir = Get-ChildItem $Cache -Directory | Where-Object { $_.Name -like ($pre + '*') } | Select-Object -First 1
        if ($dir) { $unit = Get-ChildItem $dir.FullName -Recurse -Force | Where-Object { $_.Name -eq $want } | Select-Object -First 1 }
    }
    if (-not $unit) {
        foreach ($d in $AlsoSearch) {
            $c = Join-Path $d $want
            if (Test-Path $c) { $unit = Get-Item $c; break }
        }
    }
    if (-not $unit) { Write-Output "MISS  $want (not in the cache or the search dirs)"; continue }
    Write-Output ("UNIT  " + $want + "  ->  " + $unit.FullName)

    foreach ($lane in @('native', 'mzml')) {
        $out = Join-Path $OutDir "$stem.$lane.mzpeak"
        $log = Join-Path $OutDir "$stem.$lane.log"
        if (-not $Rebuild -and (Test-Path $out) -and (Get-Item $out).Length -gt 0) {
            Write-Output ("  {0,-6} kept  {1,12} B" -f $lane, (Get-Item $out).Length); continue
        }
        Remove-Item $out -ErrorAction SilentlyContinue
        # [string[]] is load-bearing: `$x = if (...) { @('one') }` yields the STRING, and splatting a
        # string (@opts) passes it one CHARACTER per argument ("unexpected argument '-'").
        [string[]]$opts = @()
        if ($lane -eq 'mzml') { $opts = [string[]]@('--via-msconvert') }
        if ($sample) { $opts += @('--sample', $sample) }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        & $conv $unit.FullName @opts --no-vendor -o $out --force *> $log
        $rc = $LASTEXITCODE
        $sz = if (Test-Path $out) { (Get-Item $out).Length } else { 0 }
        $err = if ($rc -ne 0) { (Select-String -Path $log -Pattern 'error' | Select-Object -First 1).Line } else { '' }
        Write-Output ("  {0,-6} exit={1} {2,5}s {3,12} B  {4}" -f $lane, $rc, [int]$sw.Elapsed.TotalSeconds, $sz, $err)
    }
}
Write-Output "--- pairs in $OutDir ---"
Get-ChildItem $OutDir -Filter *.mzpeak | ForEach-Object { "{0,-46} {1,12} B" -f $_.Name, $_.Length }
