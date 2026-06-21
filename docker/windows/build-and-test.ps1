# Native Agilent/SciEX runtime verification (run inside the Windows container).
# Expects, mounted at runtime:
#   $env:MZPC_PWIZ_DIR  -> a ProteoWizard install (MHDAC in vendor_api\Agilent, Clearcore2 in vendor_api\ABI)
#   C:\data             -> the downloaded vendor corpus (data/vendor-agilent-sciex from the repo)
$ErrorActionPreference = 'Stop'

$bin   = 'C:\mzpc\target\release\mzpeak-convert.exe'
$data  = if ($env:MZPC_DATA) { $env:MZPC_DATA } else { 'C:\data' }
$out   = 'C:\out'; New-Item -ItemType Directory -Force -Path $out | Out-Null

if (-not $env:MZPC_PWIZ_DIR) { throw "Set MZPC_PWIZ_DIR to a ProteoWizard install (the vendor DLLs)." }
# The glues live next to their built dll; point each at its Release output.
$env:MZPC_SCIEX_GLUE   = 'C:\mzpc\glue\sciex\bin\Release\net8.0'
$env:MZPC_AGILENT_GLUE = 'C:\mzpc\glue\agilent\bin\Release\net8.0'

# Resolve the pyarrow>=14 validator if present (optional); else skip validation.
$validate = Get-Command mzpeak-validate -ErrorAction SilentlyContinue

function Convert-And-Check($input, $tag) {
  Write-Host "==> $tag : $input"
  & $bin "$input" -o "$out\$tag.mzpeak" --force
  if ($LASTEXITCODE -ne 0) { Write-Host "  CONVERT FAILED ($LASTEXITCODE)"; return $false }
  if ($validate) {
    & $validate.Source "$out\$tag.mzpeak"
    if ($LASTEXITCODE -ne 0) { Write-Host "  VALIDATE FAILED"; return $false }
  }
  Write-Host "  OK"; return $true
}

$ok = $true
# Native SciEX (.wiff) + Agilent (.d) discovered under the mounted corpus.
Get-ChildItem -Path "$data\sciex"   -Filter *.wiff -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
  $ok = (Convert-And-Check $_.FullName ("sciex_"   + $_.BaseName)) -and $ok }
Get-ChildItem -Path "$data\agilent" -Filter *.d    -Recurse -Directory -ErrorAction SilentlyContinue | ForEach-Object {
  $ok = (Convert-And-Check $_.FullName ("agilent_" + $_.Name)) -and $ok }

if ($ok) { Write-Host "ALL NATIVE CONVERSIONS OK"; exit 0 } else { Write-Host "SOME FAILED"; exit 1 }
