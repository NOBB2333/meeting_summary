#!/usr/bin/env bash
set -euo pipefail

platform="$(uname -s)"
missing=0
for tool in cargo pkg-config clang; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    missing=1
  fi
done

if ! pkg-config --exists libavcodec libavformat libavutil libswscale libswresample 2>/dev/null; then
  echo "Missing FFmpeg development libraries visible through pkg-config." >&2
  case "$platform" in
    Darwin) echo "Install with: brew install ffmpeg pkg-config llvm" >&2 ;;
    Linux) echo "Install your distribution's ffmpeg development and pkg-config packages." >&2 ;;
  esac
  missing=1
fi

case "$platform" in
  Darwin)
    echo "macOS toolchain detected. ScreenCaptureKit/CoreAudio backend is the next platform implementation step."
    ;;
  Linux)
    echo "Linux toolchain detected. PipeWire backend is the next platform implementation step."
    ;;
  *)
    echo "Use scripts/setup.ps1 on Windows." >&2
    missing=1
    ;;
esac

if [[ "$missing" -ne 0 ]]; then
  exit 1
fi

echo "Native build prerequisites are available. Run scripts/dev.sh check."
