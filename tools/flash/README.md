# flash-workstation conversion harness

Convert **all** datasets on the Windows **flash-workstation** — the only machine with the native
vendor DLLs — then validate and pull the results back to the Mac.

## Why the Windows box

The Mac (Apple Silicon) is blocked on every native vendor read: Agilent `.d`, SciEX `.wiff`,
Waters `.raw`, Bruker BAF, and the msconvert fallback all need Windows vendor DLLs; Thermo `.raw`
needs .NET. The flash-workstation has **ProteoWizard** (the vendor DLLs), **.NET**, and the built
C# glue, so it converts every format natively. (This is why the Mac sweep showed rows like
`BLOCKED_APPLE_SILICON_needs_msconvert`.)

## Pieces

| File | Runs on | Role |
|---|---|---|
| `flash_run.sh` | Mac | orchestrator: `preflight \| push \| fetch \| convert \| pull \| all` over SSH |
| `flash_convert_validate.ps1` | Windows | worker: discover → convert (native, locked per-format flags) → `mzpeak-validate` → `results.tsv` |

The worker is the **native** counterpart of `tools/convert_vendor_ci.sh` (the msconvert lane); keep
the per-format strategy tables in sync.

## Connection

An ssh alias in `~/.ssh/config` bakes in the jump + user — the script just uses `ssh flash`:

```
Host flash
    HostName 192.214.178.124
    User user
    ProxyJump hive            # hive.cs.uni-tuebingen.de, User kohlbach (uni key/agent)
    StrictHostKeyChecking accept-new
```

The **jump hop** (hive) uses your uni key/agent — same as the existing `spock`/`data` hosts. Only
the **final Windows hop** needs the password, fed by `sshpass` from `FLASH_PW` (defaults to the
`PW=` line in `../../flash-workstation.txt`). Install once: `brew install hudochenkov/sshpass/sshpass`.

Better: drop the password — `ssh-copy-id flash`, then `export FLASH_PW=''` for key auth, and the
plaintext `flash-workstation.txt` can go.

### One-time box prerequisites

- **OpenSSH Server** (if not already): in an elevated PowerShell —
  `Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0; Start-Service sshd; Set-Service sshd -StartupType Automatic`
- The built binary at `C:\mzpc\target\release\mzpeak-convert.exe` (with the `sciex`/`agilent`/
  `bruker_sdk` features) and this repo's `tools/flash/` synced to `C:\mzpc\tools\flash\`.
- **ProteoWizard** at `C:\ProteoWizard` (override with `FLASH_PWIZ`) — supplies the vendor DLLs.
- `mzpeak-validate` on PATH (optional; the worker skips validation if absent).

## Run

```sh
cd mzPeakConverter
tools/flash/flash_run.sh preflight                 # verify ssh + exe + pwiz + validator

# Get data onto the box (pick per dataset):
tools/flash/flash_run.sh fetch                     # public sets: curl ON the box from tools/vendor_ci_manifest.tsv
tools/flash/flash_run.sh push \                    # Mac-only sets (private fixtures, the MTBLS18 BAF .d)
  ~/Claude/mzPeak/data/vendor-bruker-baf/MTBLS18

tools/flash/flash_run.sh convert                   # native convert + validate, writes C:\out\results.tsv
tools/flash/flash_run.sh pull                      # archives + results.tsv + logs -> out/flash/

# or the whole public pipeline at once:
tools/flash/flash_run.sh all
```

### Knobs (env or flags)

| Var / flag | Default | Meaning |
|---|---|---|
| `FLASH_SSH` | `flash` | ssh host alias |
| `FLASH_PW` | from `flash-workstation.txt` | Windows-hop password (set `''` for key auth) |
| `FLASH_DATA` / `FLASH_OUT` | `C:\data` / `C:\out` | box-side input / output roots |
| `FLASH_PWIZ` | `C:\ProteoWizard` | vendor-DLL source |
| `convert --via-msconvert` | off | force the ProteoWizard lane for SciEX/Waters instead of native |
| `convert --purge` | off | delete each raw after measuring (small-disk runners) |

## Transfer strategy

The Mac corpus is ~302 GB — don't push it all. **fetch-on-box** pulls public datasets straight to
the box from `tools/vendor_ci_manifest.tsv` (+ the `v09` object-store mirror); **push** is only for
Mac-only data; **pull** brings back just the `.mzpeak` archives + `results.tsv` + logs (tiny vs. raw).

## Output

`C:\out\results.tsv` (pulled to `out/flash/`): `id, format, status, raw_bytes, mzpeak_bytes, ratio,
secs, validate`. `status` is `OK` / `CONV-ERR` / `VALIDATE-FAIL`; `validate` is `PASS` / `FAIL` / `-`.
Per-dataset stdout+validator output in `C:\out\logs\<id>.log`. Worker exit 0 ⇔ every row `OK`.
