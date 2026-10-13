#!/usr/bin/env bash
# Linux/macOS helper. The Windows installer itself is built on Windows
# (scripts/build-windows.ps1) or by the GitHub Actions workflow.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "Running launcher-core tests..."
cargo test -p launcher-core

if [[ "$(uname -s)" == "MINGW"* || "$(uname -s)" == "Windows_NT" ]]; then
  powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build-windows.ps1
  exit $?
fi

echo
echo "This machine is not Windows. Tests above are the cross-platform patch suite."
echo "To produce the installer:"
echo "  - On Windows:  powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1"
echo "  - Or push the repo and download the NSIS artifact from the Windows build workflow."
