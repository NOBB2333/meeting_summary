#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$(uname -s)" != Darwin ]]; then
  echo "This packager supports macOS. Use package.ps1 on Windows." >&2
  exit 1
fi
exec python3 "$root/scripts/package-macos.py" "$@"
