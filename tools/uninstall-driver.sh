#!/usr/bin/env bash
# Standalone nvenc-vaapi-driver recovery: works from a text console, no sudo.
set -euo pipefail

DEST_DIR="${HOME}/.local/lib/dri"
while (($#)); do
  case "$1" in
    --dest-dir)
      if (($# < 2)) || [[ -z "$2" ]]; then echo 'error: --dest-dir requires a directory' >&2; exit 2; fi
      DEST_DIR="$2"; shift 2 ;;
    --help|-h)
      echo 'Usage: uninstall-driver.sh [--dest-dir DIR]'
      echo 'Remove the per-user NVENC VA-API backend and its managed selection files.'
      exit 0 ;;
    *) echo "error: unknown option: $1" >&2; exit 2 ;;
  esac
done

remove_managed_file() {
  local path="$1" marker="$2" first_line=''
  if [[ ! -e "$path" && ! -L "$path" ]]; then return; fi
  IFS= read -r first_line < "$path" || true
  if [[ "$first_line" == "$marker" ]]; then
    rm -f -- "$path"
  else
    echo "Kept unmanaged file: $path" >&2
  fi
}

remove_managed_file "${XDG_CONFIG_HOME:-${HOME}/.config}/environment.d/90-nvenc-vaapi.conf" \
  '# Managed by nvenc-vaapi-driver install-driver.sh'
remove_managed_file "${XDG_DATA_HOME:-${HOME}/.local/share}/applications/vesktop-nvenc-vaapi.desktop" \
  '# Managed by nvenc-vaapi-driver'
# Unlinking does not modify the inode mapped by running applications.
rm -f -- "${DEST_DIR}/nvidia_nvenc_drv_video.so"

# A TTY may lack the graphical session's bus variables. Use the current user's
# standard runtime directory when present; never contact another user's bus.
USER_RUNTIME="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
if [[ -S "${USER_RUNTIME}/bus" && -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  export DBUS_SESSION_BUS_ADDRESS="unix:path=${USER_RUNTIME}/bus"
fi
if command -v systemctl >/dev/null 2>&1; then
  if timeout 5s systemctl --user show-environment 2>/dev/null | \
       grep -x 'LIBVA_DRIVER_NAME=nvidia_nvenc' >/dev/null; then
    if ! timeout 5s systemctl --user unset-environment \
      LIBVA_DRIVER_NAME LIBVA_DRIVERS_PATH NVENC_VAAPI_DEVICE; then
      echo 'Could not refresh user manager environment; log out and back in.' >&2
    fi
  fi
fi
echo 'NVENC VA-API backend removed. Restart affected applications or log out and back in.'
echo 'The NVIDIA graphics driver and other VA-API backends were not changed.'
