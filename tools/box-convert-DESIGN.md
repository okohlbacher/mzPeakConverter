# box-convert — URL-driven, S3-relayed, parallel box conversions

## Goal
Convert a raw file that lives at a **URL** on the Windows flash-workstation ("the box"), without
host↔box file copies, so conversions are **isolated** (each in its own temp dir, no shared state) and
**parallelizable** (N independent jobs). The converted `.mzpeak` is relayed through S3
(StackIT, S3-compatible): the box uploads via a **presigned PUT** (no credentials on the box), the
host downloads it and deletes the S3 object.

## Data flow (one job)
```
host: mint S3 key + presigned PUT url ─┐
host: ssh ──(job json via stdin)──▶ box: download raw from URL
                                       box: mzpeak-convert <raw> <opts> -o out.mzpeak
                                       box: curl PUT out.mzpeak ──▶ S3 (presigned)
host ◀──(result json: exit, log, md5, size)── box: print + rm -rf workdir
host: s3 GET key ──▶ local out.mzpeak ; verify size+md5
host: s3 DELETE key            (always — even on failure: no orphan objects)
```
Raw stays on the box (pulled from its URL). Only the `.mzpeak` crosses S3. Nothing but the result
JSON crosses the SSH channel.

## Components
1. **`tools/s3_relay.py`** — boto3 over the StackIT profile (no embedded secrets). Subcommands:
   `presign-put <key> [--expires S]` → URL on stdout; `get <key> <dest>`; `delete <key>`;
   `head <key>` (size for verify). Config overridable by env (`S3_BUCKET/S3_ENDPOINT/S3_REGION/AWS_PROFILE`).
2. **`tools/box_convert_remote.ps1`** — runs on the box. Reads ONE job as JSON from **stdin**
   `{raw_url, put_url, opts, archive, converter}`. Per-job temp dir. Downloads raw (curl); if
   `archive` (or URL ends .zip/.tgz/.tar.gz) extracts and locates the unit
   (first `*.wiff|*.d|*.raw|*.RAW|*.mzML|*.imzML`). Runs the converter, captures `$LASTEXITCODE` and
   the log. If `out.mzpeak` exists: md5 + size, then `curl -fS -X PUT --upload-file` to `put_url`.
   Emits exactly one result JSON between `<<<BOXRESULT` / `BOXRESULT>>>` markers. `finally`: `rm -rf`
   the temp dir (raw + mzpeak gone — isolation + disk hygiene).
3. **`tools/box_convert.sh`** — host orchestrator. One job:
   `box_convert.sh <raw-url> <out.mzpeak> [-- <converter opts…>]`. Many/parallel:
   `box_convert.sh --manifest jobs.tsv [--jobs N]` (`raw_url <TAB> out_path <TAB> opts`). Per job:
   uuid key under `$S3_PREFIX`, presign PUT, ssh (job JSON piped to **stdin** of the staged remote
   script), extract result between markers, on success `s3_relay get`+verify, **always**
   `s3_relay delete` (trap). `xargs -P`/bounded fan-out for `--jobs`.

## Config (no secrets, no institutional names committed)
Sourced from env or a gitignored `tools/box.env` if present:
`BOX_SSH` (user@host), `BOX_JUMP` (jump user@host), `BOX_SSH_KEY`, `BOX_CONVERTER` (exe path on box),
`BOX_WORKROOT`; `S3_BUCKET/S3_PREFIX/S3_ENDPOINT/S3_REGION/AWS_PROFILE`.

## Hardening (after adversarial review)
- **Secrets never in argv.** Host→box: job JSON on stdin. On the box: the presigned PUT and raw URLs
  are written to per-user-temp `curl -K` config files, not curl's command line (no exposure in the
  box process table). Temp dir (raw+mzpeak+configs) is always `rm -rf`'d.
- **Always delete the S3 key.** Minted keys are registered in a temp dir; an `EXIT/INT/TERM` trap
  deletes any still-registered key (idempotent), so Ctrl-C / crash mid-job can't orphan an object.
  Per-job delete failures are surfaced (WARN) and retried by the trap.
- **Deliver only a clean success.** The box uploads *only* when the converter exits 0 **and** wrote
  output; the host delivers *only* when `uploaded==true && exit==0`. A nonzero/partial run is never
  named into place.
- **End-to-end integrity.** Box reports size+md5 over the (trusted) SSH channel; host verifies the S3
  download against both before the atomic `mv` of a per-job-unique `.part`. Marker-collision-proof:
  the result is base64'd between the markers (a log containing the marker string can't corrupt it).

## Deliberate non-goals / limits
- **Single-PUT ≤ 5 GB.** A presigned single PUT caps at 5 GB; the host refuses (clear error) if the box
  reports a larger `.mzpeak`, pointing at scp/multipart. Most corpus outputs are < 3 GB.
- Assumes the converter is **already built** on the box (the wrapper does not git-pull/build).
- Multi-file vendor formats (`.d`, `.wiff`+`.scan`, `.imzML`+`.ibd`) require the URL to be an **archive**.
