#!/usr/bin/env bash
# Cross-builds the Windows app from macOS and zips it into dist/.
#
# One-time setup:
#   brew install rustup mingw-w64
#   /opt/homebrew/opt/rustup/bin/rustup toolchain install stable --profile minimal --target x86_64-pc-windows-gnu
#
# Usage: scripts/build-windows.sh
#   TOOLCHAIN  rustup toolchain to use (default: stable)
set -euo pipefail
cd "$(dirname "$0")/.."

toolchain="${TOOLCHAIN:-stable}"
target=x86_64-pc-windows-gnu
# rustup is keg-only in Homebrew; put it first without touching the shell profile.
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"

cargo "+$toolchain" build --release -p meeting-transcriber --target "$target"

exe="target/$target/release/meeting-transcriber.exe"
# Fail loudly if MinGW runtime DLLs sneak in; they don't exist on a stock Windows PC.
if x86_64-w64-mingw32-objdump -p "$exe" | grep -qiE "DLL Name: (libgcc|libstdc|libwinpthread)"; then
  echo "error: $exe depends on MinGW runtime DLLs" >&2
  exit 1
fi

mkdir -p dist
zip="dist/meeting-transcriber-windows-x64.zip"
rm -f "$zip"
( cd "$(dirname "$exe")" && zip -q -9 "$OLDPWD/$zip" meeting-transcriber.exe )
echo "Built $zip"
