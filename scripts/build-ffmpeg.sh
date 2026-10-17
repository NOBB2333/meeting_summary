#!/usr/bin/env bash
set -euo pipefail
prefix="${1:-/opt/ffmpeg}"
version=9.0.2
digest=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
work="${OPENCAP_FFMPEG_WORK:-/tmp/opencap-ffmpeg}"
mkdir -p "$work"
cd "$work"
curl -fL --retry 4 "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz" -o ffmpeg.tar.xz
echo "$digest  ffmpeg.tar.xz" | sha256sum --check
tar -xJf ffmpeg.tar.xz
cd "ffmpeg-$version"
./configure --prefix="$prefix" --enable-shared --disable-static --disable-doc --disable-debug \
  --disable-autodetect --enable-gpl --enable-libx264 --enable-libpulse \
  --disable-encoders --enable-encoder=aac,pcm_f32le,pcm_s16le,libx264,mpeg4,png,bmp \
  --disable-ffplay
make -j"${OPENCAP_BUILD_JOBS:-4}"
make install
mkdir -p "$prefix/share/opencap-source"
cp ../ffmpeg.tar.xz "$prefix/share/opencap-source/ffmpeg-$version.tar.xz"
cp COPYING.GPLv2 COPYING.GPLv3 LICENSE.md "$prefix/share/opencap-source/"
cp ffbuild/config.mak "$prefix/share/opencap-source/ffmpeg-config.mak"
