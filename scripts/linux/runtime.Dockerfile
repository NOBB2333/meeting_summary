# Runtime verification is separate from the old-glibc compiler sysroot. Real
# desktops supply their graphics drivers; CI exposes both Mesa software backends.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 pulseaudio pulseaudio-utils xvfb x11-xserver-utils \
    dbus-x11 libegl1 libgl1-mesa-dri mesa-vulkan-drivers libvulkan1 \
    libxkbcommon-x11-0 libxcb-shape0 libxcb-xfixes0 fonts-dejavu-core openbox \
    gdb x11-utils xdotool \
    && rm -rf /var/lib/apt/lists/*
ENV LIBGL_ALWAYS_SOFTWARE=1
ENV VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
WORKDIR /workspace
