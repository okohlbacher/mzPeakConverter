# box_url_convert.ps1 — box-side: download a raw (+ vendor sidecars) from PUBLIC source URLs into a
# LOCAL CACHE, convert to .mzpeak, NO S3. The mac scp's the result back. Cached raws are kept so a
# re-convert skips the (large) download.
#   powershell -NoProfile -ExecutionPolicy Bypass -File box_url_convert.ps1 \
#       -Urls "<url1>,<url2>,..." -RawName <file-to-convert> -OutPath <box out.mzpeak> -CacheKey <subdir>
param(
  [Parameter(Mandatory=$true)][string]$Urls,
  [Parameter(Mandatory=$true)][string]$RawName,
  [Parameter(Mandatory=$true)][string]$OutPath,
  [Parameter(Mandatory=$true)][string]$CacheKey
)
$ErrorActionPreference = 'Continue'
$ProgressPreference    = 'SilentlyContinue'
$cache = Join-Path 'C:\Users\User\rawcache' $CacheKey
New-Item -ItemType Directory -Force -Path $cache | Out-Null
foreach ($u in ($Urls -split ',')) {
  if (-not $u.Trim()) { continue }
  $name = [IO.Path]::GetFileName(([Uri]$u).AbsolutePath)
  $dst  = Join-Path $cache $name
  if ((Test-Path $dst) -and ((Get-Item $dst).Length -gt 0)) { "CACHED=$name"; continue }   # cache hit
  & curl.exe -fSL --retry 3 --retry-delay 5 -o $dst $u
  if ($LASTEXITCODE -ne 0) { "DL_FAIL=$name (curl $LASTEXITCODE)"; exit 1 }
  "DOWNLOADED=$name"
}
# vendor SDK env (mirrors box_convert_remote.ps1 / box_local_convert.ps1)
$env:DOTNET_ROOT = 'C:\Users\User\dotnet8'; $env:DOTNET_ROLL_FORWARD = 'LatestMajor'
$cvtRoot = 'C:\Users\User\src\mzPeakConverter'
$pwiz = 'C:/Program Files (x86)/FLASHApp-1.0.0/FLASHApp-1.0.0/share/OpenMS/THIRDPARTY/pwiz-bin'
if (Test-Path "$cvtRoot\glue\sciex\bin\Release\net8.0")  { $env:MZPC_SCIEX_GLUE  = (Resolve-Path "$cvtRoot\glue\sciex\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\waters\bin\Release\net8.0") { $env:MZPC_WATERS_GLUE = (Resolve-Path "$cvtRoot\glue\waters\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\agilent\bin\Release\net48") { $env:MZPC_AGILENT_GLUE = (Resolve-Path "$cvtRoot\glue\agilent\bin\Release\net48").Path }
$env:MZPC_PWIZ_DIR = $pwiz; $env:MZPC_MASSLYNX_DIR = $pwiz
if (Test-Path 'C:\Users\User\box_convert_env.ps1') { . 'C:\Users\User\box_convert_env.ps1' }
$conv = "$cvtRoot\target\release\mzpeak-convert.exe"
$in   = Join-Path $cache $RawName
$log  = "$OutPath.log"
& $conv $in --no-vendor -o $OutPath --force *> $log
"EXIT=$LASTEXITCODE"
if (Test-Path $OutPath) { "SIZE=" + (Get-Item $OutPath).Length } else { "NO_OUTPUT" }
