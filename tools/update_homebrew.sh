#!/usr/bin/env bash
# Point the Homebrew tap at a published release.
#
# Downloads each macOS asset of the given tag from GitHub, hashes it, and rewrites
# both `Casks/mzpeak-convert.rb` and `Formula/mzpeak-convert.rb` with that version and
# those checksums — so neither can claim a digest that no published archive has. Run
# after a release's macOS assets exist (the release workflow calls this itself):
#
#     tools/update_homebrew.sh v0.11.4
#
# MZPC_CASK_REPO overrides the repository (default okohlbacher/mzPeakConverter).
set -euo pipefail

version="${1:-}"
[ -n "$version" ] || { echo "usage: $0 <version>   (e.g. v0.11.4 or 0.11.4)" >&2; exit 2; }
version="${version#v}"
repo="${MZPC_CASK_REPO:-okohlbacher/mzPeakConverter}"
here="$(cd "$(dirname "$0")/.." && pwd)"
cask="$here/Casks/mzpeak-convert.rb"
formula="$here/Formula/mzpeak-convert.rb"
for f in "$cask" "$formula"; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

sha256_of() {  # macOS ships shasum, Linux runners ship sha256sum
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else sha256sum "$1" | awk '{print $1}'; fi
}

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
declare -A digest
for target in aarch64 x86_64; do
  asset="mzpeak-convert-${version}-${target}-apple-darwin.tar.gz"
  url="https://github.com/${repo}/releases/download/v${version}/${asset}"
  echo "fetching $asset"
  curl -fsSL --retry 3 --retry-delay 2 -o "$tmp/$asset" "$url" \
    || { echo "no such asset published: $url" >&2; exit 1; }
  digest[$target]="$(sha256_of "$tmp/$asset")"
  echo "  sha256 ${digest[$target]}"
done

python3 - "$cask" "$formula" "$version" "${digest[aarch64]}" "${digest[x86_64]}" <<'PY'
import pathlib, re, sys
cask, formula, version, arm, intel = sys.argv[1:6]

def bump_version(text, path):
    text, n = re.subn(r'^(  version ")[^"]+(")$', rf'\g<1>{version}\g<2>', text, count=1, flags=re.M)
    if n != 1:
        sys.exit(f"{path}: expected exactly one `version \"…\"` line, found {n}")
    return text

# Cask: one `sha256 arm:` / `intel:` pair.
s = bump_version(pathlib.Path(cask).read_text(), cask)
s, n_a = re.subn(r'^(  sha256 arm:\s+")[0-9a-f]{64}(",)$', rf'\g<1>{arm}\g<2>', s, count=1, flags=re.M)
s, n_i = re.subn(r'^(\s+intel: ")[0-9a-f]{64}(")$', rf'\g<1>{intel}\g<2>', s, count=1, flags=re.M)
if not (n_a and n_i):
    sys.exit(f"{cask}: checksum lines did not match (arm={n_a} intel={n_i}); fix it by hand")
pathlib.Path(cask).write_text(s)
print(f"{cask}: version {version}, both checksums updated")

# Formula: the sha256 inside each on_arm / on_intel block.
s = bump_version(pathlib.Path(formula).read_text(), formula)
for block, digest in (("on_arm", arm), ("on_intel", intel)):
    pattern = re.compile(rf'(    {block} do\n(?:.*?\n)*?      sha256 ")[0-9a-f]{{64}}(")')
    s, n = pattern.subn(rf'\g<1>{digest}\g<2>', s, count=1)
    if n != 1:
        sys.exit(f"{formula}: could not find the sha256 inside `{block} do` ({n} matches); fix it by hand")
pathlib.Path(formula).write_text(s)
print(f"{formula}: version {version}, both checksums updated")
PY

if command -v ruby >/dev/null 2>&1; then
  ruby -c "$cask" >/dev/null && ruby -c "$formula" >/dev/null && echo "cask and formula parse"
fi
git -C "$here" --no-pager diff --stat -- Casks/mzpeak-convert.rb Formula/mzpeak-convert.rb || true
