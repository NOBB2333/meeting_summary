#!/usr/bin/env bash
set -euo pipefail
prefix="${1:-/opt/ffmpeg}"
version=9.0.2
digest=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
work="${OPENCAP_FFMPEG_WORK:-/tmp/opencap-ffmpeg}"
mkdir -p "$work"
cd "$work"
if [[ "$(uname -m)" == x86_64 ]]; then
  # Debian 10's NASM is too old for FFmpeg 9's SIMD assembly. Keep SIMD enabled
  # by building a pinned assembler instead of disabling those optimizations.
  curl -fL --retry 4 https://deb.debian.org/debian/pool/main/n/nasm/nasm_2.16.03.orig.tar.xz -o nasm.tar.xz
  echo '1412a1c760bbd05db026b6c0d1657affd6631cd0a63cddb6f73cc6d4aa616148  nasm.tar.xz' | sha256sum --check
  tar -xJf nasm.tar.xz
  (
    cd nasm-2.16.03
    ./configure --prefix=/opt/nasm
    make -j"${OPENCAP_BUILD_JOBS:-4}"
    make install
  )
  export PATH="/opt/nasm/bin:$PATH"
fi
curl -fL --retry 4 "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz" -o ffmpeg.tar.xz
echo "$digest  ffmpeg.tar.xz" | sha256sum --check
tar -xJf ffmpeg.tar.xz
cd "ffmpeg-$version"
./configure --prefix="$prefix" --enable-shared --disable-static --disable-doc --disable-debug \
  --disable-autodetect --enable-gpl --enable-libx264 --enable-libpulse --enable-zlib \
  --disable-encoders --enable-encoder=aac,pcm_f32le,pcm_s16le,libx264,mpeg4,png,bmp \
  --disable-ffplay
make -j"${OPENCAP_BUILD_JOBS:-4}"
make install
mkdir -p "$prefix/share/opencap-source"
cp ../ffmpeg.tar.xz "$prefix/share/opencap-source/ffmpeg-$version.tar.xz"
cp COPYING.GPLv2 COPYING.GPLv3 LICENSE.md "$prefix/share/opencap-source/"
cp ffbuild/config.mak "$prefix/share/opencap-source/ffmpeg-config.mak"
