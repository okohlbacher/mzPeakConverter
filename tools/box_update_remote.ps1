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

function Invoke-Native([scriptblock]$Cmd) {
    # PowerShell 5.1 turns a native command's stderr into ErrorRecords when that stderr is
    # redirected, so under $ErrorActionPreference='Stop' a command that SUCCEEDED still aborts the
    # script. git reports progress on stderr: `git fetch` prints "From <url>" and "* [new tag] ..."
    # -- 0 bytes when it has nothing to do, 234 on the day a release lands -- and `git checkout`
    # prints "Previous HEAD position was ...". That is why the updater worked every ordinary day and
    # failed only on release day, reporting `failed: From https://github.com/...`, git's own notice,
    # as the error; twice it left the box on a new tag carrying the OLD exe. Exit codes decide
    # success here, never stderr.
    #
    # $script:, not a plain assignment: the scriptblocks are written at script scope, and only the
    # script-scoped variable is certain to be the one their native command reads.
    # A scriptblock (not a name + args) keeps `-f`/`--release` from being bound as PowerShell
    # parameters. The helper returns the exit code ONLY; every call site redirects its own output.
    $prev = $script:ErrorActionPreference
    $script:ErrorActionPreference = 'Continue'
    try { & $Cmd } finally { $script:ErrorActionPreference = $prev }
    return $LASTEXITCODE
}

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
        if ($age.TotalSeconds -gt 1800) {          # stale: a previous run died before its finally
            # 1800s, not 7200s: a cargo build takes minutes, so a lock older than half an hour is a
            # corpse. At 7200 a host killed mid-update blocked every later run for two hours.
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

    # The box clone is SHALLOW and single-branch (fetch = +refs/heads/main:refs/remotes/origin/main),
    # which does NOT keep release tags out: `--tags` fetches refs/tags/* in addition to whatever the
    # configured refspec would fetch. Measured against a clone provisioned the same way -- a tag
    # pushed after the clone arrives as `* [new tag]`, and `git checkout -f <tag>` succeeds even for
    # a tag older than the depth-1 window, the repository staying shallow throughout. So neither a
    # widened refspec nor `--unshallow` is needed here, and neither would have fixed the two missed
    # releases; the stderr trap above did.
    if ((Invoke-Native { git fetch origin --tags --force *>&1 | Out-Null }) -ne 0) {
        throw "git fetch failed (network/auth?)"
    }

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

    if ((Invoke-Native { git checkout -f $want *>&1 | Out-Null }) -ne 0) { throw "git checkout $want failed" }

    $buildLog = Join-Path $env:TEMP 'mzpc-boxupdate-build.log'
    if ((Invoke-Native { cargo build --release *> $buildLog }) -ne 0) {
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
