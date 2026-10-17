FROM debian:bullseye-slim
ENV DEBIAN_FRONTEND=noninteractive
# Bullseye security updates moved to the signed archive after LTS ended.
RUN sed -i 's|http://deb.debian.org/debian-security|http://archive.debian.org/debian-security|' /etc/apt/sources.list
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl xz-utils build-essential clang libclang-dev cmake pkg-config \
    nasm python3 git patchelf binutils \
    libx264-dev libpulse-dev pulseaudio pulseaudio-utils \
    libfontconfig1-dev libfreetype6-dev libxcb1-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libxcb-xkb-dev libxkbcommon-x11-dev libxrandr-dev libxfixes-dev libxi-dev \
    libxinerama-dev libxcursor-dev libxrender-dev libxcomposite-dev libxdamage-dev \
    libwayland-dev libegl1-mesa-dev libvulkan-dev libssl-dev libdbus-1-dev libsecret-1-dev \
    fonts-noto-cjk fonts-dejavu-core xvfb xauth dbus-x11 mesa-utils \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/build-ffmpeg.sh /opt/build-ffmpeg.sh
RUN bash /opt/build-ffmpeg.sh /opt/ffmpeg && rm -rf /tmp/opencap-ffmpeg
ENV CARGO_HOME=/opt/rust/cargo RUSTUP_HOME=/opt/rust/rustup
ENV PATH=/opt/rust/cargo/bin:/opt/ffmpeg/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.97.1 --component rustfmt,clippy \
    && rm /tmp/rustup.sh
ENV PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig LD_LIBRARY_PATH=/opt/ffmpeg/lib
ENV CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0
WORKDIR /workspace
