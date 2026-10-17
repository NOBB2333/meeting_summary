#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$(uname -s)" in
  Darwin) exec python3 "$root/scripts/package-macos.py" "$@" ;;
  Linux) exec python3 "$root/scripts/package-linux.py" "$@" ;;
  *) echo "Use package.ps1 on Windows." >&2; exit 1 ;;
esac
