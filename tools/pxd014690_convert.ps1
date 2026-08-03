# Box-side: fetch one PXD014690 run from PRIDE, convert .wiff -> mzML (msconvert, the only
# reader for SCIEX Clearcore2) -> .mzpeak (mzpeak-convert). Cached raws are kept so a
# re-run skips the download.
param([Parameter(Mandatory=$true)][string]$Base)
$ErrorActionPreference = 'Continue'
$ProgressPreference    = 'SilentlyContinue'
$pwizDir = 'C:\Program Files\OpenMS-3.5.0-pre-FVdeploy-2026-01-29\share\OpenMS\THIRDPARTY\pwiz-bin'
$conv    = 'C:\Users\User\src\mzPeakConverter\target\release\mzpeak-convert.exe'
$ftp     = 'ftp://ftp.pride.ebi.ac.uk/pride/data/archive/2019/11/PXD014690'
$dir     = 'C:\Users\User\pxd014690'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

foreach ($ext in @('.wiff', '.wiff.scan')) {
  $dst = Join-Path $dir ($Base + $ext)
  if ((Test-Path $dst) -and (Get-Item $dst).Length -gt 0) { Write-Output "CACHED $ext"; continue }
  try {
    Invoke-WebRequest -Uri "$ftp/$Base$ext" -OutFile "$dst.part" -ErrorAction Stop
    Move-Item -Force "$dst.part" $dst
    Write-Output ("DL_OK $ext " + (Get-Item $dst).Length)
  } catch { Write-Output ("DL_ERR $ext " + $_.Exception.Message); exit 1 }
}

$wiff = Join-Path $dir ($Base + '.wiff')
$mzml = Join-Path $dir ($Base + '.mzML')
if (-not (Test-Path $mzml)) {
  & (Join-Path $pwizDir 'msconvert.exe') $wiff --mzML --zlib --64 -o $dir 2>&1 | Select-Object -Last 2
}
if (-not (Test-Path $mzml)) { Write-Output 'MZML_FAIL'; exit 2 }
Write-Output ("MZML_OK " + (Get-Item $mzml).Length)

$mzp = Join-Path $dir ($Base + '.mzpeak')
if (-not (Test-Path $mzp)) { & $conv $mzml -o $mzp 2>&1 | Select-Object -Last 2 }
if (-not (Test-Path $mzp)) { Write-Output 'MZPEAK_FAIL'; exit 3 }
Write-Output ("MZPEAK_OK " + (Get-Item $mzp).Length)
