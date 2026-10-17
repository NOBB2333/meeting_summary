FROM debian:buster-slim
ENV DEBIAN_FRONTEND=noninteractive
ARG TARGETARCH
# Bootstrap HTTPS for the archived base image; APT still verifies Debian signatures.
ADD --chmod=644 https://curl.se/ca/cacert.pem /etc/ssl/certs/ca-certificates.crt
# Build against glibc 2.28 for UOS 20 / Debian 10. Use signed archived repositories.
RUN printf 'deb https://archive.debian.org/debian buster main\ndeb https://archive.debian.org/debian-security buster/updates main\n' > /etc/apt/sources.list \
    && printf 'Acquire::Check-Valid-Until "false";\nAcquire::Retries "4";\nAcquire::https::Pipeline-Depth "0";\n' > /etc/apt/apt.conf.d/99archive \
    && rm /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,id=opencap-buster-apt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
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
RUN apt-get update && apt-get install -y --no-install-recommends x11-xserver-utils \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/linux/cxx-runtime.sh /opt/cxx-runtime.sh
RUN bash /opt/cxx-runtime.sh
ENV PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig LD_LIBRARY_PATH=/opt/compat/lib:/opt/ffmpeg/lib
ENV LIBRARY_PATH=/opt/compat/lib
ENV CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0
WORKDIR /workspace
