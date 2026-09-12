#!/usr/bin/env bash
# Submit one artefact to Apple for notarization and, if it is rejected, say WHY.
#
#   .github/notarize.sh <a .zip, .dmg or .pkg>
#
# needs MACOS_APPLE_ID, MACOS_TEAM_ID and MACOS_NOTARY_PASSWORD in the env.
#
# `notarytool submit --wait` does exit non-zero when Apple returns Invalid, but all
# it prints is a submission id: the reasons — the unsigned binary, the missing
# hardened runtime, the bad entitlement — live behind `notarytool log <id>`, which
# is a second round trip nobody is at the keyboard to make. Fetching it here is the
# difference between "notarization failed" and a named cause, on a step that takes
# tens of minutes to reach a second time.
#
# Shared with okohlbacher/DIALibraryGenerator, which is where it was written.
set -uo pipefail

: "${MACOS_APPLE_ID:?not set}" "${MACOS_TEAM_ID:?not set}" "${MACOS_NOTARY_PASSWORD:?not set}"
ART=${1:?no artefact given}
[ -s "$ART" ] || { echo "::error::$ART does not exist or is empty"; exit 1; }

CREDS=(--apple-id "$MACOS_APPLE_ID" --team-id "$MACOS_TEAM_ID" --password "$MACOS_NOTARY_PASSWORD")

out=$(xcrun notarytool submit "$ART" "${CREDS[@]}" --wait 2>&1)
rc=$?
printf '%s\n' "$out"
[ "$rc" -eq 0 ] && exit 0

# The id is printed on submission, so it is in `out` even when the wait failed.
id=$(printf '%s\n' "$out" | awk '/^ *id: /{print $2; exit}')
if [ -n "$id" ]; then
  echo "--- notarytool log $id ---"
  # || true: the log itself failing must not replace the real error with its own.
  xcrun notarytool log "$id" "${CREDS[@]}" || true
fi
echo "::error::Apple rejected $(basename "$ART") — the issues are listed above"
exit 1
