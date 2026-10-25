#!/usr/bin/env bash
# Install without modifying the inode mapped by existing clients.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${ROOT}/target/release/libnvidia_nvenc_drv_video.so"
DEST_DIR="${HOME}/.local/lib/dri"
SESSION=none
usage() {
  echo 'Usage: install-driver.sh [--library FILE] [--dest-dir DIR] [--enable-session | --disable-session]'
}
while (($#)); do
  case "$1" in
    --library|--dest-dir)
      if (($# < 2)) || [[ -z "$2" ]]; then usage >&2; exit 2; fi
      if [[ "$1" == --library ]]; then SRC="$2"; else DEST_DIR="$2"; fi
      shift 2 ;;
    --enable-session|--disable-session)
      if [[ "$SESSION" != none ]]; then usage >&2; exit 2; fi
      SESSION="${1#--}"; shift ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
ENV_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/environment.d"
ENV_FILE="${ENV_DIR}/90-nvenc-vaapi.conf"
MARKER='# Managed by nvenc-vaapi-driver install-driver.sh'
if [[ "$SESSION" != none && -e "$ENV_FILE" ]]; then
  IFS= read -r FIRST_LINE < "$ENV_FILE" || true
  if [[ "${FIRST_LINE:-}" != "$MARKER" ]]; then
    echo "error: refusing to change unmanaged file: ${ENV_FILE}" >&2
    exit 1
  fi
fi
if [[ "$SESSION" == disable-session ]]; then
  rm -f -- "$ENV_FILE"
  echo 'Removed session selection. Log out and back in to refresh application environments.'
  exit 0
fi
if [[ ! -f "$SRC" || ! -s "$SRC" ]]; then
  echo "error: nonempty driver library not found: ${SRC}; run cargo build --release" >&2
  exit 1
fi
DEST_DIR="$(realpath -m -- "$DEST_DIR")"
# environment.d performs variable expansion. Refuse ambiguous values rather
# than writing a persistent driver path that resolves differently at login.
if [[ "$SESSION" == enable-session && "$DEST_DIR" == *[\$\`\"\\$'\n'$'\r']* ]]; then
  echo 'error: session driver directory contains unsupported quoting/expansion characters' >&2
  exit 2
fi
mkdir -p -- "$DEST_DIR"
DEST_DIR="$(cd -- "$DEST_DIR" && pwd)"
TEMP=''
cleanup() { if [[ -n "$TEMP" ]]; then rm -f -- "$TEMP"; fi; }
trap cleanup EXIT
TEMP="$(mktemp "${DEST_DIR}/.nvidia_nvenc.XXXXXXXX")"
install -m 755 -- "$SRC" "$TEMP"
mv -fT -- "$TEMP" "${DEST_DIR}/nvidia_nvenc_drv_video.so"
TEMP=''
echo "Installed: ${DEST_DIR}/nvidia_nvenc_drv_video.so"
if [[ "$SESSION" == enable-session ]]; then
  mkdir -p -- "$ENV_DIR"
  TEMP="$(mktemp "${ENV_DIR}/.nvenc-vaapi.XXXXXXXX")"
  {
    printf '%s\n' "$MARKER"
    printf 'LIBVA_DRIVER_NAME=nvidia_nvenc\n'
    printf 'LIBVA_DRIVERS_PATH="%s"\n' "$DEST_DIR"
  } > "$TEMP"
  chmod 644 -- "$TEMP"
  mv -fT -- "$TEMP" "$ENV_FILE"
  TEMP=''
  echo 'Session selection enabled for future logins (encode-only backend; no VA-API decoding).'
else
  echo 'Select with LIBVA_DRIVER_NAME=nvidia_nvenc and LIBVA_DRIVERS_PATH pointing to this directory.'
fi
