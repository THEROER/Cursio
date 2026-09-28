# The Linux build for machines on Ubuntu 24.04, made from a newer host.
#
# A Linux build links the system's FFmpeg by soname, and 24.04 ships FFmpeg 6
# (libavutil.so.58) where a 25.10 host has FFmpeg 7 (.so.59): a binary built on
# the host does not start there. Built in here, against 24.04's own libraries,
# it does. `cargo xtask stage ubuntu24` and `publish` use this image; the
# source and the cargo registry are mounted, nothing is copied in.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config clang libclang-dev cmake curl ca-certificates git nasm \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev \
    libavfilter-dev libavdevice-dev \
    libgtk-4-dev libadwaita-1-dev libpipewire-0.3-dev libspa-0.2-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
    libfuse3-dev libdbus-1-dev libxkbcommon-dev libudev-dev libssl-dev libopus-dev \
    libx11-dev libxcb1-dev libxi-dev libxtst-dev libevdev-dev libusb-1.0-0-dev \
    libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev libayatana-appindicator3-dev \
    librsvg2-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
