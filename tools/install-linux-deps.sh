#!/usr/bin/env bash
set -euo pipefail

# GPUI Kit 0.6.1 / gpui-pre 0.3.6, Ubuntu 24.04.
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential clang cmake pkg-config \
  libfontconfig-dev libwayland-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libssl-dev libzstd-dev libvulkan1 fonts-dejavu-core
