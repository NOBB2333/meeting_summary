#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

task="${1:-run}"
if [[ $# -gt 0 ]]; then
  shift
fi

case "$task" in
  build) command=(cargo build --workspace --locked) ;;
  run) command=(cargo run -p opencap --locked) ;;
  test) command=(cargo test --workspace --locked) ;;
  check) command=(cargo check --workspace --locked) ;;
  bench) command=(cargo run -p opencap-bench --locked --) ;;
  *)
    echo "Usage: scripts/dev.sh {build|run|test|check|bench} [arguments...]" >&2
    exit 2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust/Cargo is required. Run scripts/setup.sh first." >&2
  exit 1
fi

exec "${command[@]}" "$@"
