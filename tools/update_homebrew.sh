#!/usr/bin/env bash
# Point the Homebrew tap at a published release.
#
# Downloads each macOS asset of the given tag from GitHub, hashes it, and rewrites
# `Casks/mzpeak-convert.rb` with that version and those checksums — so the cask can
# only ever claim a digest that a real, published archive has. Run
# after a release's macOS assets exist (the release workflow calls this itself):
#
#     tools/update_homebrew.sh v0.11.4
#
# MZPC_CASK_REPO overrides the repository (default okohlbacher/mzPeakConverter).
set -euo pipefail
# Written for bash 3.2: that is what /bin/bash is on macOS and on GitHub's macOS
# runners, so no associative arrays or other bash-4 features here.

version="${1:-}"
[ -n "$version" ] || { echo "usage: $0 <version>   (e.g. v0.11.4 or 0.11.4)" >&2; exit 2; }
version="${version#v}"
repo="${MZPC_CASK_REPO:-okohlbacher/mzPeakConverter}"
here="$(cd "$(dirname "$0")/.." && pwd)"
cask="$here/Casks/mzpeak-convert.rb"
[ -f "$cask" ] || { echo "no cask at $cask" >&2; exit 1; }

sha256_of() {  # macOS ships shasum, Linux runners ship sha256sum
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else sha256sum "$1" | awk '{print $1}'; fi
}

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
digest_aarch64=""
digest_x86_64=""
for target in aarch64 x86_64; do
  asset="mzpeak-convert-${version}-${target}-apple-darwin.tar.gz"
  url="https://github.com/${repo}/releases/download/v${version}/${asset}"
  echo "fetching $asset"
  curl -fsSL --retry 3 --retry-delay 2 -o "$tmp/$asset" "$url" \
    || { echo "no such asset published: $url" >&2; exit 1; }
  d="$(sha256_of "$tmp/$asset")"
  printf -v "digest_$target" '%s' "$d"
  echo "  sha256 $d"
done

python3 - "$cask" "$version" "$digest_aarch64" "$digest_x86_64" <<'PY'
import pathlib, re, sys
cask, version, arm, intel = sys.argv[1:5]

s = pathlib.Path(cask).read_text()
s, n_v = re.subn(r'^(  version ")[^"]+(")$', rf'\g<1>{version}\g<2>', s, count=1, flags=re.M)
s, n_a = re.subn(r'^(  sha256 arm:\s+")[0-9a-f]{64}(",)$', rf'\g<1>{arm}\g<2>', s, count=1, flags=re.M)
s, n_i = re.subn(r'^(\s+intel: ")[0-9a-f]{64}(")$', rf'\g<1>{intel}\g<2>', s, count=1, flags=re.M)
if not (n_v and n_a and n_i):
    sys.exit(f"{cask} did not match the expected shape (version={n_v} arm={n_a} intel={n_i}); fix it by hand")
pathlib.Path(cask).write_text(s)
print(f"{cask}: version {version}, both checksums updated")
PY

if command -v ruby >/dev/null 2>&1; then
  ruby -c "$cask" >/dev/null
  echo "cask parses"
fi
git -C "$here" --no-pager diff --stat -- Casks/mzpeak-convert.rb || true
