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
  run) command=(cargo run -p opencap --features dev-profile --locked) ;;
  test) command=(cargo test --workspace --locked) ;;
  check) command=(cargo check --workspace --locked) ;;
  lint) command=(cargo clippy --workspace --all-targets --all-features --locked) ;;
  bench) command=(cargo run -p opencap-bench --features dev-profile --locked --) ;;
  *)
    echo "Usage: scripts/dev.sh {build|run|test|check|lint|bench} [arguments...]" >&2
    exit 2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust/Cargo is required. Run scripts/setup.sh first." >&2
  exit 1
fi

# Reuse the ordinary debug dependency cache; release developer artifacts remain
# isolated so they cannot replace the production release binary.
if [[ "$task" == run && " $* " != *" --release "* ]]; then
  export CARGO_TARGET_DIR="$root/target"
elif [[ "$task" == run || "$task" == bench ]]; then
  export CARGO_TARGET_DIR="$root/target/opencap-dev"
fi

# A Windows FFmpeg SDK must never shadow Homebrew's native libraries.
if [[ "$(uname -s)" == Darwin ]]; then
  # Rust 1.97's debuginfo stripping can produce a __LINKEDIT layout rejected by
  # older Apple tools. Release already has debug=0; preserve the Mach-O layout.
  export CARGO_PROFILE_RELEASE_STRIP="${CARGO_PROFILE_RELEASE_STRIP:-none}"
  if [[ -n "${FFMPEG_DIR:-}" && ! -f "$FFMPEG_DIR/lib/libavcodec.dylib" ]]; then
    unset FFMPEG_DIR
  fi
  if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists libavcodec libavformat libavfilter libavutil libswscale libswresample; then
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
  echo "OpenCap Dev: building $run_profile and launching the isolated app bundle."
  cargo build -p opencap --features dev-profile --locked "${build_arguments[@]}"
  python3 "$root/scripts/package-macos.py" --dev --profile "$run_profile" --no-build \
    --binary "$CARGO_TARGET_DIR/$run_profile/opencap" --require-default
  exec open "$root/release/OpenCap Dev.app" --args "${app_arguments[@]}"
fi

if [[ "$task" == run ]]; then
  if [[ " $* " == *" --release "* ]]; then
    echo "OpenCap: running the optimized Release build."
  else
    echo "OpenCap: running Debug (unoptimized). For performance testing: scripts/dev.sh run --release"
  fi
fi
exec "${command[@]}" "$@"
