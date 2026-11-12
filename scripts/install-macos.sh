#!/usr/bin/env bash
# Installs (or updates) Meeting Recorder from the latest GitHub release into
# /Applications on a Mac with Apple silicon:
#
#   curl -fsSL https://raw.githubusercontent.com/HryhoriiHaponiukQuitCode/meeting-recorder-lite/main/scripts/install-macos.sh | bash
#
# - The app has an ad-hoc signature, not a Developer ID one. A .dmg saved by
#   a browser is quarantined and macOS then refuses to open the app (only
#   System Settings → Privacy & Security → Open Anyway lets it). A download
#   by curl is not quarantined; the flag is removed anyway in case it is set.
# - A running Meeting Recorder is asked to quit first; while it records it
#   stays open and nothing is installed.
# - Recordings, settings and the speech model live outside the app and are
#   kept.
set -euo pipefail

REPO="HryhoriiHaponiukQuitCode/meeting-recorder-lite"
APP="Meeting Recorder"
DEST="/Applications/${APP}.app"
# The window process (the transcriber's name starts the same way).
running() { pgrep -f 'Contents/MacOS/meeting-recorder$' >/dev/null; }

die() {
  echo "error: $*" >&2
  exit 1
}

[ "$(uname -s)" = "Darwin" ] || die "this installer is for macOS"
[ "$(uname -m)" = "arm64" ] || die "the app is built for Apple silicon (M1 or newer)"

echo "Looking for the latest release of ${REPO}..."
url=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
  grep -o '"browser_download_url": *"[^"]*_aarch64\.dmg"' |
  head -n 1 | sed 's/.*"\(https[^"]*\)"/\1/') || true
[ -n "${url}" ] || die "no macOS installer found in the latest release"

work=$(mktemp -d)
mount="${work}/mount"
cleanup() {
  hdiutil detach "${mount}" -quiet 2>/dev/null || true
  rm -rf "${work}"
}
trap cleanup EXIT

echo "Downloading $(basename "${url}")..."
curl -fL --progress-bar -o "${work}/app.dmg" "${url}"

if running; then
  echo "Quitting ${APP}..."
  osascript -e "tell application \"${APP}\" to quit" >/dev/null 2>&1 || true
  for _ in $(seq 1 15); do
    running || break
    sleep 1
  done
  running &&
    die "${APP} is still running (a recording in progress?). Stop it, quit the app and run this again."
fi

mkdir -p "${mount}"
hdiutil attach "${work}/app.dmg" -nobrowse -readonly -mountpoint "${mount}" -quiet
[ -d "${mount}/${APP}.app" ] || die "the installer does not contain ${APP}.app"

echo "Installing to ${DEST}..."
rm -rf "${DEST}"
ditto "${mount}/${APP}.app" "${DEST}"
xattr -dr com.apple.quarantine "${DEST}" 2>/dev/null || true

open "${DEST}"
echo "Done: ${APP} is installed and open. Settings > Transcription downloads the speech model once."
