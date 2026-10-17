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
    Darwin) echo "Install with: brew install ffmpeg pkgconf" >&2 ;;
    Linux) echo "Install your distribution's ffmpeg development and pkg-config packages." >&2 ;;
  esac
  missing=1
fi

case "$platform" in
  Darwin)
    if ! xcrun --find swiftc >/dev/null 2>&1; then
      echo "Install Xcode Command Line Tools: xcode-select --install" >&2
      missing=1
    fi
    if ! xcrun --find metal >/dev/null 2>&1; then
      echo "GPUI requires Xcode's Metal compiler. Run: xcodebuild -downloadComponent MetalToolchain" >&2
      missing=1
    fi
    echo "macOS backend: ScreenCaptureKit + CoreAudio + VideoToolbox (macOS 13+)."
    ;;
  Linux)
    echo "Linux backend: X11 + PulseAudio / PipeWire-Pulse. Wayland: audio and documents only."
    echo "For UOS-compatible builds use scripts/linux/Dockerfile (glibc 2.28); see docs/build-and-linux.md."
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
