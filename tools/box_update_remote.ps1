# box_update_remote.ps1 — runs ON the flash-workstation. Brings the box's mzpeak-convert to the
# wanted version and reports what it did. Reads ONE job as JSON from stdin:
#   {"repo":"C:\\Users\\User\\src\\mzPeakConverter","want":"latest"|"vX.Y.Z",
#    "bin_dir":"C:\\Users\\User\\bin","build":true}
# Prints base64(result-json) between <<<BOXSYNC / BOXSYNC>>> markers (same shape as
# box_convert_remote.ps1, so the host decodes it with the same helper).
#
# "latest" = the newest TAG after a fetch, version-sorted. Never origin/main: Cargo.toml is bumped
# only by the `chore: release` commit, so a main-built binary reports the PREVIOUS tag's version and
# would corrupt the corpus currency model (archives stamped with a version that is not what built
# them). Never the host's binary either — a stale one on the host would DOWNGRADE the box.
#
# The built exe is installed as bin_dir\mzpeak-convert-<ver>.exe and returned in `exe`; the host
# passes it per job as the `converter` field. Versioned paths mean a concurrent conversion holds a
# DIFFERENT file, so Windows' running-exe lock can never fail the link step.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$job = [Console]::In.ReadToEnd() | ConvertFrom-Json
$res = [ordered]@{ ok=$false; action='failed'; have=''; want=''; latest=''; exe=''; error=''; log='' }
$lock = Join-Path $env:TEMP 'mzpc-boxupdate.lock'
$lockHeld = $false

function Probe([string]$exe) {
    if (-not (Test-Path $exe)) { return '' }
    try { return (((& $exe --version 2>&1) | Select-Object -First 1) -split '\s+')[-1] } catch { return '' }
}

try {
    $repo = $job.repo; $binDir = $job.bin_dir
    if (-not (Test-Path $repo)) { throw "repo not found: $repo" }

    # 1. BUSY: a conversion in flight holds the exe open; cargo's link step would fail with
    #    "Access is denied (os error 5)". Skipping is correct — the job proceeds on the installed exe.
    if (Get-Process mzpeak-convert -ErrorAction SilentlyContinue) {
        $res.action = 'skipped-busy'; $res.ok = $true
        $res.have = Probe (Join-Path $repo 'target\release\mzpeak-convert.exe')
        return
    }

    # 2. LOCK: create-if-absent (no -Force) is atomic, so two hosts cannot build at once.
    try {
        New-Item -ItemType File -Path $lock -ErrorAction Stop | Out-Null
        $lockHeld = $true
    } catch {
        $age = (Get-Date) - (Get-Item $lock).LastWriteTime
        if ($age.TotalSeconds -gt 7200) {          # stale: a previous run died before its finally
            Remove-Item $lock -Force -ErrorAction SilentlyContinue
            New-Item -ItemType File -Path $lock -ErrorAction Stop | Out-Null
            $lockHeld = $true
        } else {
            $res.action = 'skipped-locked'; $res.ok = $true; return
        }
    }

    $relExe = Join-Path $repo 'target\release\mzpeak-convert.exe'
    $res.have = Probe $relExe

    # 3. Refuse to clobber uncommitted work — a dirty tree means someone is testing on the box.
    Set-Location $repo
    $dirty = (git status --porcelain 2>&1 | Measure-Object -Line).Lines
    if ($LASTEXITCODE -ne 0) { throw "git status failed" }
    if ($dirty -gt 0) {
        # Cargo.lock churn is normal after a build and must not block an update.
        $real = (git status --porcelain -- . ':(exclude)Cargo.lock' 2>$null | Measure-Object -Line).Lines
        if ($real -gt 0) { $res.action = 'refused-dirty'; throw "$real uncommitted change(s) in $repo" }
    }

    git fetch origin --tags --force *>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "git fetch failed (network/auth?)" }

    # -v:refname, not -creatordate: same-day releases sort wrongly by date, and plain lexicographic
    # ranks v0.7.9 above v0.7.10.
    $latest = (git tag --sort=-v:refname | Select-Object -First 1)
    $res.latest = $latest
    $want = if ($job.want -and $job.want -ne 'latest') { $job.want } else { $latest }
    $res.want = ($want -replace '^v','')
    if (-not $want) { throw "no tags in $repo" }

    if ($res.have -and ($res.have -eq $res.want)) {
        $installed = Join-Path $binDir ("mzpeak-convert-" + $res.have + ".exe")
        $res.exe = if (Test-Path $installed) { $installed } else { $relExe }
        $res.action = 'current'; $res.ok = $true; return
    }
    if (-not $job.build) { $res.action = 'behind'; $res.ok = $true; return }

    git checkout -f $want *>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "git checkout $want failed" }

    $buildLog = Join-Path $env:TEMP 'mzpc-boxupdate-build.log'
    cargo build --release *> $buildLog
    if ($LASTEXITCODE -ne 0) {
        $res.log = (Get-Content $buildLog -Tail 40 -ErrorAction SilentlyContinue) -join "`n"
        throw "cargo build failed"
    }

    $now = Probe $relExe
    if (-not $now) { throw "built, but the exe does not report a version" }
    if ($now -ne $res.want) { throw "version mismatch after build: got $now, wanted $($res.want)" }

    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    $installed = Join-Path $binDir ("mzpeak-convert-" + $now + ".exe")
    Copy-Item $relExe $installed -Force
    $res.exe = $installed; $res.have = $now; $res.action = 'updated'; $res.ok = $true
}
catch {
    if ($res.action -ne 'refused-dirty') { $res.action = 'failed' }
    $res.error = $_.Exception.Message
}
finally {
    if ($lockHeld) { Remove-Item $lock -Force -ErrorAction SilentlyContinue }
    if ($res.log.Length -gt 8192) { $res.log = $res.log.Substring($res.log.Length - 8192) }
    $json = $res | ConvertTo-Json -Compress -Depth 4
    Write-Output "<<<BOXSYNC"
    Write-Output ([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json)))
    Write-Output "BOXSYNC>>>"
}
