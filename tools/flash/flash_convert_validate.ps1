<#
  flash_convert_validate.ps1 — Windows-side worker for the flash-workstation harness.

  Runs ON the Windows box (the only machine with the native vendor DLLs). Discovers every
  dataset under -DataRoot, converts each to mzPeak with the native reader + locked per-format
  strategy, validates the archive, and writes a single results.tsv + per-dataset logs to -OutRoot.

  This is the native lane (mzpeak-convert.exe + glue), the counterpart of the msconvert lane in
  tools/convert_vendor_ci.sh. Keep the strategy table below in sync with that script.

  Driven over SSH by tools/flash/flash_run.sh, but runnable standalone:
    pwsh -File flash_convert_validate.ps1 -DataRoot C:\data -OutRoot C:\out -PwizDir 'C:\ProteoWizard'
#>
param(
  [string]$DataRoot = $(if ($env:MZPC_DATA) { $env:MZPC_DATA } else { 'C:\data' }),
  [string]$OutRoot  = 'C:\out',
  [string]$Bin      = 'C:\mzpc\target\release\mzpeak-convert.exe',
  [string]$PwizDir  = $env:MZPC_PWIZ_DIR,        # ProteoWizard install (vendor DLLs)
  [switch]$ViaMsconvert,                          # force the msconvert lane for sciex/waters
  [switch]$PurgeInputs,                           # delete each raw after measuring (small-disk runners)
  [switch]$NoValidate
)
$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Bin)) { throw "mzpeak-convert.exe not found at $Bin — build it on the box first (cargo build --release)." }
if (-not $PwizDir)         { throw "Set -PwizDir / `$env:MZPC_PWIZ_DIR to a ProteoWizard install (the vendor DLLs)." }

# Runtime env the native readers need (mirrors HANDOFF.md gotchas + build-and-test.ps1).
$env:MZPC_PWIZ_DIR              = $PwizDir
if (-not $env:MZPC_SCIEX_GLUE)   { $env:MZPC_SCIEX_GLUE   = 'C:\mzpc\glue\sciex\bin\Release\net8.0' }
if (-not $env:MZPC_AGILENT_GLUE) { $env:MZPC_AGILENT_GLUE = 'C:\mzpc\glue\agilent\bin\Release\net8.0' }
if (-not $env:DOTNET_ROLL_FORWARD) { $env:DOTNET_ROLL_FORWARD = 'LatestMajor' }      # Thermo .NET
$env:MZDATA_IGNORE_UNKNOWN_INSTRUMENT = 'ignore'

New-Item -ItemType Directory -Force -Path $OutRoot, "$OutRoot\archives", "$OutRoot\logs" | Out-Null
$tsv = "$OutRoot\results.tsv"
"id`tformat`tstatus`traw_bytes`tmzpeak_bytes`tratio`tsecs`tvalidate" | Set-Content -Encoding utf8 $tsv

$validate = if ($NoValidate) { $null } else { Get-Command mzpeak-validate -ErrorAction SilentlyContinue }

# Locked per-format strategy (2026-06-24), aligned with tools/convert_vendor_ci.sh. -ViaMsconvert
# overrides sciex/waters to the ProteoWizard lane. mzML/imzML/thermo/bruker use native defaults
# (Bruker TDF is ims-compact by default; pass --no-ims-compact to disable).
function Get-Flags($fmt) {
  switch ($fmt) {
    'agilent-d'  { return @('--agilent-grid','--no-vendor') }
    'sciex-wiff' { return $(if ($ViaMsconvert) { @('--via-msconvert','--tof-grid','auto','--no-vendor') } else { @('--tof-grid','auto','--no-vendor') }) }
    'waters-raw' { return $(if ($ViaMsconvert) { @('--via-msconvert','--no-vendor') } else { @('--no-vendor') }) }
    default      { return @() }    # thermo-raw, bruker-d, mzml, imzml
  }
}

function Size-Of($path) { (Get-ChildItem -LiteralPath $path -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum }
function Slug($path)    { ($path -replace [regex]::Escape($DataRoot),'' -replace '\.[^.\\/]+$','' -replace '[^A-Za-z0-9._-]','_' -replace '_+','_').Trim('_') }

function Convert-One($input, $fmt) {
  $id  = Slug $input
  $arc = "$OutRoot\archives\$id.mzpeak"
  $log = "$OutRoot\logs\$id.log"
  $raw = Size-Of $input
  Write-Host "==> [$fmt] $id"
  $sw  = [Diagnostics.Stopwatch]::StartNew()
  $status='OK'; $vstat='-'; $mp=''; $ratio=''
  try {
    & $Bin $input @(Get-Flags $fmt) -o $arc --force *> $log
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $arc)) { throw "convert exit $LASTEXITCODE" }
    $mp = Size-Of $arc
    if ($raw -gt 0) { $ratio = '{0:N4}' -f ($mp / $raw) }
    if ($validate) {
      & $validate.Source $arc *>> $log
      $vstat = $(if ($LASTEXITCODE -eq 0) { 'PASS' } else { 'FAIL' })
      if ($vstat -eq 'FAIL') { $status = 'VALIDATE-FAIL' }
    }
  } catch {
    $status = 'CONV-ERR'; "ERROR: $_" | Add-Content $log
  }
  $sw.Stop()
  "$id`t$fmt`t$status`t$raw`t$mp`t$ratio`t$([int]$sw.Elapsed.TotalSeconds)`t$vstat" | Add-Content -Encoding utf8 $tsv
  if ($PurgeInputs) { Remove-Item -LiteralPath $input -Recurse -Force -ErrorAction SilentlyContinue }
}

# --- discovery (order matters: classify .d / .raw dirs before falling through) ---------------
# Bruker .d : has analysis.tdf / analysis.tsf / analysis.baf
Get-ChildItem $DataRoot -Recurse -Directory -Filter '*.d' -ErrorAction SilentlyContinue | Where-Object {
  Test-Path (Join-Path $_.FullName 'analysis.tdf') -or (Test-Path (Join-Path $_.FullName 'analysis.tsf')) -or (Test-Path (Join-Path $_.FullName 'analysis.baf'))
} | ForEach-Object { Convert-One $_.FullName 'bruker-d' }

# Agilent .d : has an AcqData subdir (discover by the marker, not the name — " - Copy" suffixes exist)
Get-ChildItem $DataRoot -Recurse -Directory -Filter 'AcqData' -ErrorAction SilentlyContinue |
  ForEach-Object { $_.Parent.FullName } | Sort-Object -Unique | Where-Object {
    -not (Test-Path (Join-Path $_ 'analysis.tdf')) -and -not (Test-Path (Join-Path $_ 'analysis.tsf'))
  } | ForEach-Object { Convert-One $_ 'agilent-d' }

# Waters .raw : a DIRECTORY (Thermo .raw is a FILE, handled below)
Get-ChildItem $DataRoot -Recurse -Directory -Filter '*.raw' -ErrorAction SilentlyContinue |
  ForEach-Object { Convert-One $_.FullName 'waters-raw' }

# Thermo .raw : a FILE
Get-ChildItem $DataRoot -Recurse -File -Filter '*.raw' -ErrorAction SilentlyContinue |
  ForEach-Object { Convert-One $_.FullName 'thermo-raw' }

# SciEX .wiff (the .wiff.scan / .wiff2 sidecars travel with it; the reader picks them up)
Get-ChildItem $DataRoot -Recurse -File -Filter '*.wiff' -ErrorAction SilentlyContinue |
  ForEach-Object { Convert-One $_.FullName 'sciex-wiff' }

# imzML (before mzML so we don't double-count) and plain mzML / mzML.gz
Get-ChildItem $DataRoot -Recurse -File -Filter '*.imzML' -ErrorAction SilentlyContinue |
  ForEach-Object { Convert-One $_.FullName 'imzml' }
Get-ChildItem $DataRoot -Recurse -File -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '\.mzML(\.gz)?$' } |
  ForEach-Object { Convert-One $_.FullName 'mzml' }

Write-Host "=== done -> $tsv ==="
Get-Content $tsv
$fails = (Get-Content $tsv | Select-Object -Skip 1 | Where-Object { $_ -notmatch "`tOK`t" }).Count
exit $(if ($fails -eq 0) { 0 } else { 1 })
