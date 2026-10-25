# nvenc-vaapi-driver

A Linux VA-API backend that encodes H.264 using NVIDIA NVENC. Applications use
libva's encode API; the driver translates those requests to CUDA and NVENC.
There is no background daemon and no application-specific routing.

## Supported operation

- H.264 Constrained Baseline, Main and High; progressive NV12 input.
- CBR, VBR and constant QP, dynamic bitrate and rational frame rate.
- Client-supplied initial/minimum/maximum QP, rate-control windows and HRD
  buffer size/fullness for bitrate-based encoding.
- CUDA-resident input allocation and CPU upload through VA images.
- NV12 image readback, including chroma-aligned crops, and explicit read/write
  mapping access for internal CUDA surfaces.
- Per-session input registration and synchronous, low-latency output without
  B-frames or lookahead.

The backend is encode-only. It does not provide decoding, video processing,
HEVC/AV1, arbitrary packed-header passthrough or surface export. PRIME import
accepts a restricted linear NV12 layout; end-to-end compositor DMA-BUF
interoperability needs separate validation. General Wayland/Electron sandbox
compatibility and a combined decode/encode dispatcher remain unfinished.

Rate controls apply to any VA-API client. Explicit HRD buffering takes
precedence over the rate-control window; omitted controls use backend defaults.
A maximum QP limits compression and can cause bitrate overshoot on complex
scenes, as permitted by VA-API. It cannot guarantee sharp output within every
bitrate budget. Ordinary bitrate/FPS updates preserve queued output; explicit
rate-control resets drain the queue and request an IDR.

Applications must actually select VA-API H.264 encoding. A libva backend cannot
redirect software encoders that never call libva. Application build options,
codec negotiation and capture pipelines can prevent hardware encoding even
when driver discovery succeeds.

## Build

Requires Rust (edition 2024), a C/C++ toolchain, Clang/libclang, libva development
headers. CUDA/NVENC are loaded at runtime; the CUDA toolkit is not required
for compilation. Runtime
encoding requires NVIDIA's CUDA and NVENC userspace libraries, an accessible
GPU with H.264 encoding support, and a compatible NVIDIA driver.

```sh
cargo build --release --locked
bash tools/install-driver.sh
```

The installer places `nvidia_nvenc_drv_video.so` in `~/.local/lib/dri` using atomic
replacement. Existing processes retain their loaded version; restart clients
to use the update. `--dest-dir DIR` supports packaging and staged installation.

## Uninstall and recovery

```sh
bash tools/uninstall-driver.sh
```

This standalone script works from a TTY without sudo, network access or the
repository. It removes the per-user backend, managed session selection and
managed test launcher, and clears this backend's user-manager environment.
Other graphics drivers, VA-API backends and unmanaged configuration are kept.
Copy the script somewhere accessible before testing session-wide selection.
For a custom installation directory, pass `--dest-dir DIR` to both scripts.
Restart affected applications or log out and back in after removal.

## Select the backend

```sh
export LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri"
export LIBVA_DRIVER_NAME=nvidia_nvenc
vainfo --display drm --device /dev/dri/by-path/PCI-DEVICE-render
```

Replace the device path with the NVIDIA render node from `/dev/dri/by-path`.
Driver selection follows the display's DRM PCI device. For an explicit CUDA
selection, set `NVENC_VAAPI_DEVICE` to its PCI address (domain:bus:device.function).
Ambiguous GPU selection fails instead of silently choosing another device.

libva loads the shared library automatically when the application initialises
VA-API. No driver launcher or boot service is required. To persist selection
for applications launched through the systemd user manager, `bash tools/install-driver.sh --enable-session`
writes a user `environment.d` file; log out and back in. The desktop session
must inherit the user manager environment for this to affect its applications. This selects an
**encode-only** backend for inheriting VA-API clients and removes access to
other backends' decode capabilities in those processes. Use scoped environment
variables when clients also need VA-API decoding. `--disable-session` removes
only the installer's environment file.

Do not disable application sandboxes as a general installation step. Sandboxed
packages must be given access to the driver and GPU through their supported
packaging mechanisms.

## Validation

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

GPU tests are opt-in. They exercise real libva/NVENC encoding and fully decode
the output rather than treating successful process exit as proof of validity.
See [validation](docs/VALIDATION.md) for hardware tests and performance
measurement, and [architecture](docs/ARCHITECTURE.md) for resource ownership.

This remains a development backend. Unit tests and FFmpeg integration do not
establish production readiness for every client, compositor or GPU generation.
