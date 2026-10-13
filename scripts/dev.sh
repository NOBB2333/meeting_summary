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
  lint) command=(cargo clippy --workspace --all-targets --all-features --locked) ;;
  bench) command=(cargo run -p opencap-bench --locked --) ;;
  *)
    echo "Usage: scripts/dev.sh {build|run|test|check|lint|bench} [arguments...]" >&2
    exit 2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust/Cargo is required. Run scripts/setup.sh first." >&2
  exit 1
fi

# A Windows FFmpeg SDK must never shadow Homebrew's native libraries.
if [[ "$(uname -s)" == Darwin ]]; then
  if [[ -n "${FFMPEG_DIR:-}" && ! -f "$FFMPEG_DIR/lib/libavcodec.dylib" ]]; then
    unset FFMPEG_DIR
  fi
  if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists libavcodec libavformat libavutil libswscale libswresample; then
    echo "Run scripts/setup.sh first (brew install ffmpeg pkgconf)." >&2
    exit 1
  fi
fi

if [[ "$task" == run && "$(uname -s)" == Darwin ]]; then
  build_arguments=()
  app_arguments=()
  app_arguments_started=false
  run_profile=debug
  for argument in "$@"; do
    if [[ "$argument" == -- && "$app_arguments_started" == false ]]; then
      app_arguments_started=true
    elif [[ "$app_arguments_started" == true ]]; then
      app_arguments+=("$argument")
    else
      build_arguments+=("$argument")
      if [[ "$argument" == --release ]]; then run_profile=release; fi
    fi
  done
  echo "OpenCap: building $run_profile and launching release/OpenCap.app (macOS permissions belong to the app bundle)."
  cargo build -p opencap --locked "${build_arguments[@]}"
  python3 "$root/scripts/package-macos.py" --profile "$run_profile" --no-build
  exec open "$root/release/OpenCap.app" --args "${app_arguments[@]}"
fi

if [[ "$task" == run ]]; then
  if [[ " $* " == *" --release "* ]]; then
    echo "OpenCap: running the optimized Release build."
  else
    echo "OpenCap: running Debug (unoptimized). For performance testing: scripts/dev.sh run --release"
  fi
fi
exec "${command[@]}" "$@"
