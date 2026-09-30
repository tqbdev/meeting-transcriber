#!/usr/bin/env bash
# Builds "Meeting Transcriber.app" so macOS attributes the microphone and
# System Audio Recording permissions to this app instead of your terminal.
#
# Usage: scripts/bundle.sh [--run]
#   SIGN_IDENTITY  codesign identity. Defaults to your first "Apple Development"
#                  certificate, or ad-hoc ("-") if there is none. With ad-hoc
#                  signing macOS asks for permission again after every rebuild.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p meeting-transcriber

app="target/release/Meeting Transcriber.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/meeting-transcriber "$app/Contents/MacOS/"
cp crates/app/Info.plist "$app/Contents/Info.plist"

identity="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | awk -F'"' '/Apple Development/ { print $2; exit }')}"
identity="${identity:--}"
codesign --force --sign "$identity" "$app"
echo "Built $app (signed with: $identity)"

if [[ "${1:-}" == "--run" ]]; then
  open "$app"
fi
