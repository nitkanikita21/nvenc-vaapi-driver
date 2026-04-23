#!/usr/bin/env bash
# Copy the built cdylib to the per-user libva drivers directory.
set -euo pipefail

# libva resolves LIBVA_DRIVER_NAME=<name> as <path>/<name>_drv_video.so
# (without the "lib" prefix that Cargo adds to cdylibs), so we copy
# libnvidia_nvenc_drv_video.so -> nvidia_nvenc_drv_video.so on install.
SRC="$(dirname "$0")/../target/release/libnvidia_nvenc_drv_video.so"
DEST_DIR="${HOME}/.local/lib/dri"
DEST="${DEST_DIR}/nvidia_nvenc_drv_video.so"

if [[ ! -f "${SRC}" ]]; then
  echo "error: ${SRC} not found. run 'cargo build --release' first." >&2
  exit 1
fi

mkdir -p "${DEST_DIR}"
cp -v "${SRC}" "${DEST}"
echo "installed -> ${DEST}"
echo
echo "use with:"
echo "  LIBVA_DRIVERS_PATH=${DEST_DIR} LIBVA_DRIVER_NAME=nvidia_nvenc vainfo"
