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

# Vendor SDK environment the converter needs for .wiff/.raw/.d (box-specific paths). Override any of
# these by creating C:\Users\User\box_convert_env.ps1 (dot-sourced last if present).
$env:DOTNET_ROOT = 'C:\Users\User\dotnet8'; $env:DOTNET_ROLL_FORWARD = 'LatestMajor'
$cvtRoot = 'C:\Users\User\src\mzPeakConverter'
$pwiz = 'C:/Program Files (x86)/FLASHApp-1.0.0/FLASHApp-1.0.0/share/OpenMS/THIRDPARTY/pwiz-bin'
if (Test-Path "$cvtRoot\glue\sciex\bin\Release\net8.0")  { $env:MZPC_SCIEX_GLUE  = (Resolve-Path "$cvtRoot\glue\sciex\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\waters\bin\Release\net8.0") { $env:MZPC_WATERS_GLUE = (Resolve-Path "$cvtRoot\glue\waters\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\agilent\bin\Release\net48") { $env:MZPC_AGILENT_GLUE = (Resolve-Path "$cvtRoot\glue\agilent\bin\Release\net48").Path }  # net48 AgilentGlueHost.exe (MHDAC needs .NET FW)
$env:MZPC_PWIZ_DIR = $pwiz; $env:MZPC_MASSLYNX_DIR = $pwiz   # MHDAC for Agilent loads from $pwiz/vendor_api/Agilent
if (Test-Path 'C:\Users\User\box_convert_env.ps1') { . 'C:\Users\User\box_convert_env.ps1' }

$job = [Console]::In.ReadToEnd() | ConvertFrom-Json
$work = Join-Path $env:TEMP ("bxc-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$res = [ordered]@{ stage='init'; exit=1; uploaded=$false; size=0; md5=''; log=''; error=''; note='' }

try {
    $converter = if ($job.converter) { $job.converter } else { 'C:\Users\User\src\mzPeakConverter\target\release\mzpeak-convert.exe' }
    if (-not (Test-Path $converter)) { throw "converter not found: $converter" }

    # 1. download the raw from its URL — url goes in a -K config file, not argv
    $res.stage = 'download'
    $rawName = [IO.Path]::GetFileName(([Uri]$job.raw_url).AbsolutePath)
    if (-not $rawName) { $rawName = 'input.bin' }
    $rawPath = Join-Path $work $rawName
    $dlcfg = Join-Path $work 'dl.cfg'
    Set-Content -LiteralPath $dlcfg -Value ('url = "' + $job.raw_url + '"') -Encoding ASCII
    & curl.exe -fSL --retry 3 --retry-delay 5 -K $dlcfg -o $rawPath
    if ($LASTEXITCODE -ne 0) { throw "raw download failed (curl exit $LASTEXITCODE)" }

    # 2. archive (multi-file vendor formats) -> extract + pick the unit deterministically
    $isArchive = $job.archive -or ($rawName -match '\.(zip|tgz|tar\.gz)$')
    if ($isArchive) {
        $res.stage = 'extract'
        $ex = Join-Path $work 'unpacked'; New-Item -ItemType Directory -Force -Path $ex | Out-Null
        & tar.exe -xf $rawPath -C $ex   # bsdtar on Win10+ handles .zip and .tar.gz
        if ($LASTEXITCODE -ne 0) { throw "extract failed (tar exit $LASTEXITCODE)" }
        $order = @('.d','.wiff','.raw','.imzml','.mzml')   # preference order
        $units = @()
        # vendor dirs: Bruker/Agilent .d AND Waters .raw (a directory, not a file)
        $units += Get-ChildItem -Path $ex -Recurse -Directory | Where-Object { @('.d','.raw') -contains $_.Extension.ToLower() }
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
    $res.stage = 'convert'
    $out = Join-Path $work 'out.mzpeak'
    $log = Join-Path $work 'convert.log'
    $optList = @()
    if ($job.opts -and $job.opts.Trim()) { $optList = @([regex]::Split($job.opts.Trim(), '\s+') | Where-Object { $_ -ne '' }) }
    # Continue around the native call: the converter logs INFO to stderr on SUCCESS, which would
    # otherwise raise a NativeCommandError under 'Stop' and mask a clean run. Exit is read explicitly.
    $prevEAP = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    & $converter $inputPath @optList -o $out --force *> $log
    $res.exit = $LASTEXITCODE
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
        & curl.exe -fS -X PUT --upload-file $out -K $upcfg
        if ($LASTEXITCODE -ne 0) { throw "upload failed (curl exit $LASTEXITCODE)" }
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
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    if ($res.log.Length -gt 16384) { $res.log = $res.log.Substring($res.log.Length - 16384) }  # tail only
    $json = $res | ConvertTo-Json -Compress -Depth 4
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))   # marker-collision-proof
    Write-Output "<<<BOXRESULT"
    Write-Output $b64
    Write-Output "BOXRESULT>>>"
}
