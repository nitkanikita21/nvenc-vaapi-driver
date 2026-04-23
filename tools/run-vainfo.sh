#!/usr/bin/env bash
# Load the driver directly from the release build dir (no install needed).
set -euo pipefail
DIR="$(cd "$(dirname "$0")/.." && pwd)/target/release"
if [[ ! -f "${DIR}/libnvidia_nvenc_drv_video.so" ]]; then
  echo "error: ${DIR}/libnvidia_nvenc_drv_video.so not found" >&2
  exit 1
fi
# Create/refresh the libva-expected name (without 'lib' prefix).
ln -sf libnvidia_nvenc_drv_video.so "${DIR}/nvidia_nvenc_drv_video.so"
LIBVA_MESSAGING_LEVEL=2 \
LIBVA_DRIVERS_PATH="${DIR}" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
vainfo "$@"
