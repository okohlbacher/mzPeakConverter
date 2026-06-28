# box_url_convert.ps1 — box-side: download a raw (+ vendor sidecars) from PUBLIC source URLs into a
# LOCAL CACHE, convert to .mzpeak, NO S3. The mac scp's the result back. Cached raws are kept so a
# re-convert skips the (large) download.
#   powershell -NoProfile -ExecutionPolicy Bypass -File box_url_convert.ps1 \
#       -Urls "<url1>,<url2>,..." -RawName <file-to-convert> -OutPath <box out.mzpeak> -CacheKey <subdir> [-Names "n1,n2"]
# -Names: explicit cache filenames parallel to -Urls (needed when the filename is in the query string,
#   not the URL path — e.g. MassIVE ProteoSAFe DownloadResultFile endpoints).
param(
  [Parameter(Mandatory=$true)][string]$Urls,
  [Parameter(Mandatory=$true)][string]$RawName,
  [Parameter(Mandatory=$true)][string]$OutPath,
  [Parameter(Mandatory=$true)][string]$CacheKey,
  [string]$Names = ""
)
$ErrorActionPreference = 'Continue'
$ProgressPreference    = 'SilentlyContinue'
$cache = Join-Path 'C:\Users\User\rawcache' $CacheKey
New-Item -ItemType Directory -Force -Path $cache | Out-Null
# Do NOT filter empties — -Names is index-parallel to -Urls; a blank slot means "derive from URL".
$urlArr  = @($Urls  -split ',')
$nameArr = @($Names -split ',')
for ($i = 0; $i -lt $urlArr.Count; $i++) {
  $u = $urlArr[$i].Trim()
  if (-not $u) { continue }
  if ($i -lt $nameArr.Count -and $nameArr[$i].Trim()) { $name = $nameArr[$i].Trim() }
  else {
    $name = [IO.Path]::GetFileName(([Uri]$u).AbsolutePath)
    if (-not $name) { "DL_FAIL=no filename for url (pass -Names): $u"; exit 1 }   # guard empty basename
  }
  $dst  = Join-Path $cache $name
  $part = "$dst.part"
  if ((Test-Path $dst) -and ((Get-Item $dst).Length -gt 0)) { "CACHED=$name"; continue }   # cache hit
  Remove-Item -LiteralPath $part -ErrorAction SilentlyContinue                  # drop any stale partial
  # download to .part, publish on success only — a killed/interrupted curl must never be cached as
  # complete (which would later be CACHED= and converted as a truncated raw).
  & curl.exe -fSL --retry 3 --retry-delay 5 -o $part $u
  if ($LASTEXITCODE -ne 0) { Remove-Item -LiteralPath $part -ErrorAction SilentlyContinue; "DL_FAIL=$name (curl $LASTEXITCODE)"; exit 1 }
  Move-Item -LiteralPath $part -Destination $dst -Force
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
