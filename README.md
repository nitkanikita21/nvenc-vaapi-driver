# nvenc-vaapi-driver

A VAAPI backend driver for Linux that bridges the standard libva encode path to
NVIDIA NVENC. Applications using `libva.so.2` — Chromium/Electron for WebRTC,
OBS ffmpeg\_vaapi, `ffmpeg h264_vaapi`, gst-vaapi — get real GPU-accelerated
H.264 encoding without any code changes, because NVIDIA ships no VAAPI encode
driver of their own for Linux.

> **Status: Alpha / Work-in-Progress — not production ready.**
> Core encode works and has been validated in real screenshare sessions, but the
> driver has not been hardened against unusual pixel formats, multi-GPU setups, or
> clients other than those listed below.

---

## Hardware-Verified Working Setups

| Application | Encode path | Measured result |
|---|---|---|
| `ffmpeg h264_vaapi` | NV12 CPU upload via `hwupload` | 5.5x realtime on 1080p60 `testsrc2` |
| OBS Studio (Advanced, FFmpeg VAAPI H.264) | ffmpeg VAAPI | Live encode to MP4; `enc` column in `nvidia-smi dmon` non-zero |
| Vesktop (Electron Discord) WebRTC screen share | DMA-BUF zero-copy | `Encoder: VaapiVideoEncodeAccelerator`, `Power Efficient: Yes`, average encode time ~8 ms |

All three were tested on Arch Linux, Hyprland/Wayland, RTX 4060, NVIDIA driver
595.58.03, libva 2.22.

---

## Requirements

| Component | Minimum version |
|---|---|
| Linux | Any kernel with DRM/KMS |
| NVIDIA GPU | Any with NVENC (Kepler or newer) |
| NVIDIA proprietary driver | 555.x or later recommended (NVENC SDK 12.2+) |
| libva | 2.22 (runtime + headers) |
| libva-utils | any (for `vainfo`) |
| Rust stable | 1.85+ (edition 2024) |
| clang / libclang | Any version (needed by `bindgen` at build time) |

---

## Build

Install build dependencies:

```bash
# Arch Linux
sudo pacman -S libva libva-utils clang rust

# Ubuntu / Debian
sudo apt install rustup libva-dev libva-utils clang libclang-dev
```

Build the driver:

```bash
cargo build --release
```

The output is `target/release/libnvidia_nvenc_drv_video.so`.

Cargo's `cdylib` target adds a `lib` prefix; libva expects the file without it.
The install step below handles this automatically.

Verify the ABI exports are present:

```bash
nm -D target/release/libnvidia_nvenc_drv_video.so | grep __vaDriverInit
```

You should see entries from `__vaDriverInit_1_0` through `__vaDriverInit_1_23`.
All 24 aliases resolve to the same init function, making this driver loadable by
any libva 1.x minor version.

---

## Install and Configure

### Without root (recommended)

```bash
bash tools/install-driver.sh
```

This copies the built `.so` to `~/.local/lib/dri/nvidia_nvenc_drv_video.so`
(stripping the `lib` prefix that libva does not expect).

Then set the two environment variables. Add them to `~/.bash_profile`,
`~/.zprofile`, or `~/.config/environment.d/vaapi.conf` to make them permanent:

```bash
export LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri"
export LIBVA_DRIVER_NAME="nvidia_nvenc"
```

libva constructs the filename as `${LIBVA_DRIVER_NAME}_drv_video.so`, so the
above pair resolves to `~/.local/lib/dri/nvidia_nvenc_drv_video.so`.

### System-wide (requires root)

```bash
sudo cp target/release/libnvidia_nvenc_drv_video.so \
        /usr/lib/dri/nvidia_nvenc_drv_video.so
```

Set `LIBVA_DRIVERS_PATH=/usr/lib/dri` and `LIBVA_DRIVER_NAME=nvidia_nvenc` as
above.

---

## Verify the Driver Loaded

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
vainfo
```

Expected output (abbreviated):

```
libva info: VA-API version 1.23.0
libva info: Trying to open /home/<user>/.local/lib/dri/nvidia_nvenc_drv_video.so
libva info: Found init function __vaDriverInit_1_23
libva info: va_openDriver() returns 0
vainfo: VA-API version: 1.23 (libva 2.x.y)
vainfo: Driver version: nvidia_nvenc-rs (Rust NVENC VAAPI backend)
vainfo: Supported profile and entrypoints
      VAProfileH264ConstrainedBaseline: VAEntrypointEncSlice
      VAProfileH264Main               : VAEntrypointEncSlice
```

If `VAEntrypointEncSlice` is absent the driver did not load — check the
Troubleshooting section below.

---

## What You Can Test Right Now

### (a) ffmpeg smoke encode

Build and install the driver, then run:

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
ffmpeg \
  -f lavfi -i "testsrc2=size=1920x1080:rate=60:duration=5" \
  -vaapi_device /dev/dri/renderD128 \
  -vf "format=nv12,hwupload" \
  -c:v h264_vaapi -profile:v main -b:v 5M \
  /tmp/nvenc_test.mp4
```

Verify the result:

```bash
ffprobe -v error -show_streams /tmp/nvenc_test.mp4
```

Look for `codec_name: h264` and a non-zero `nb_frames`. While encoding, check
GPU utilisation:

```bash
nvidia-smi dmon -s u
```

The `enc` column should read 60% or higher for 1080p60 input.

To run the project's own automated smoke test (requires `ffmpeg` and
`ffprobe` on `PATH` and an NVIDIA GPU at `/dev/dri/renderD128`):

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --test encode_ffmpeg --release -- --ignored --nocapture
```

### (b) OBS Studio

1. Set the environment variables before launching OBS:

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
obs
```

2. In OBS: **Settings > Output > Output Mode: Advanced > Encoder: FFmpeg VAAPI
   H.264**.
3. Start a recording or stream. In `nvidia-smi dmon -s u` the `enc` column
   should be non-zero.

OBS will fall back to CPU-side upload (`vaPutImage` path) if the DMA-BUF path
is unavailable, so it works without PipeWire capture as well.

### (c) Vesktop (Electron Discord) WebRTC screen share

Chromium-based Electron apps have three runtime feature gates that must all be
enabled simultaneously for the VAAPI encode path to activate. They are all
disabled by default in upstream Chromium. Vesktop (which is built with VAAPI
encoding enabled at compile time) can be unlocked with:

```bash
LIBVA_DRIVERS_PATH="$HOME/CODING/libvaapi-rust-nvenc/target/release" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
vesktop \
  --enable-features=AcceleratedVideoEncoder,VaapiVideoEncoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,WebRtcPipeWireCapturer \
  --ignore-gpu-blocklist \
  --disable-gpu-driver-bug-workarounds \
  --disable-gpu-sandbox \
  --no-sandbox \
  --ozone-platform=x11
```

Also set `hardwareVideoAcceleration: true` in
`~/.config/vesktop/settings.json`, because Vesktop rebuilds the feature flag set
at startup and only includes `AcceleratedVideoEncoder` when that setting is on:

```json
{ "hardwareVideoAcceleration": true }
```

Once in a voice channel with screen share active, open Vesktop's debug overlay.
You should see:

```
Encoder: VaapiVideoEncodeAccelerator
Power Efficient: Yes
Average Encode Time: ~8ms
```

The three Chromium gates that must be enabled are:

- `AcceleratedVideoEncoder` — the main hardware encode gate (the C++ symbol is
  `kAcceleratedVideoEncodeLinux` but the feature string has no `Linux` suffix).
- `VaapiOnNvidiaGPUs` — bypasses Chromium's default NVIDIA block (the upstream
  comment reads: "NVIDIA VA-API drivers do not support Chromium and can sometimes
  cause crashes, disable VA-API on NVIDIA GPUs by default").
- `VaapiIgnoreDriverChecks` — skips vendor string validation that would
  otherwise reject our `nvidia_nvenc-rs` driver name.

`--disable-gpu-sandbox` and `--no-sandbox` are required because the GPU sandbox
prevents the GPU process from opening `/dev/dri/renderD128` and from inheriting
`LIBVA_*` environment variables. `--ozone-platform=x11` avoids a separate
Wayland/Electron issue where `vaGetDisplay` returns an invalid display inside
the GPU subprocess.

---

## Why Stock Chromium and Google Chrome on Linux Do Not Work

Google Chrome and the Arch Linux `chromium` package are built without
`enable_hardware_h264_encoding_on_linux=true`. This is a **compile-time** flag,
not a runtime one. No `--enable-features` flag, no `chrome://flags` entry, and
no environment variable can override it. Even with the full set of Vesktop flags
above, `chrome://gpu` will show `Video Encode: Software only` and `Problems
Detected: video_encode` in disabled features.

Electron 40 (the version bundled in Vesktop) is built with the flag on, which is
why Vesktop works and stock Chrome/Chromium do not. This is an upstream build
decision, not a deficiency in this driver.

---

## Known Limitations

- **Multi-object DMA-BUF** (separate file descriptors for the Y and UV planes)
  is not supported. The driver returns `VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE`.
  This layout is rare in practice; Chromium/PipeWire uses single-object NV12.
- **BGRA/ARGB DMA-BUF** input is not supported. Converting to NV12 on the GPU
  would require a CUDA PTX colour-conversion kernel. Deferred.
- **`vaExportSurfaceHandle`** (the inverse path — exporting an NVENC surface as
  a DMA-BUF for the client) is not implemented. OBS attempts it for a
  texture-sharing optimisation, then falls back to CPU upload via `vaPutImage`,
  which works correctly.
- **Decode, HEVC, AV1** are non-goals of this project. The driver advertises
  only `VAEntrypointEncSlice`.
- **Multi-object DMA-BUF** and colour conversion to NV12 are deferred.
- **Stock Google Chrome / Arch chromium** are incompatible by upstream build
  choice (`enable_hardware_h264_encoding_on_linux=false`), not a driver bug.

---

## Architecture Overview

```
Chromium / Discord (Electron) / OBS / ffmpeg
        |   VAAPI client API (vaInitialize, vaCreateConfig, vaBeginPicture, ...)
        v
   libva.so.2  (system)
        |   dlopen("$LIBVA_DRIVERS_PATH/nvidia_nvenc_drv_video.so")
        |   dlsym("__vaDriverInit_1_23")
        v
+------------------------------------------------------------------+
|              libnvidia_nvenc_drv_video.so                        |
|                                                                  |
|   src/lib.rs                                                     |
|   __vaDriverInit_1_{0..23}  (24 aliases, all -> driver_init)    |
|                                                                  |
|   driver/config.rs     vaCreateConfig / vaQueryConfigProfiles    |
|   driver/surface.rs    vaCreateSurfaces / DMA-BUF import        |
|   driver/context.rs    vaCreateContext                           |
|   driver/buffer.rs     vaCreateBuffer / vaMapBuffer             |
|   driver/picture.rs    vaBeginPicture / vaRenderPicture /        |
|                        vaEndPicture (rate-control reconfigure)   |
|   driver/sync.rs       vaSyncSurface / vaSyncBuffer             |
|                                                                  |
|   nvenc/session.rs     NvencSession: map-encode-lock-unmap cycle |
|   nvenc/preset.rs      bitrate/fps -> NVENC preset/tuning/RC    |
|   nvenc/h264_config.rs apply_config() mutates NV_ENC_CONFIG     |
|   cuda/external_mem.rs cuImportExternalMemory (DMA-BUF path)    |
+------------------------------------------------------------------+
        |
        v
   libnvidia-encode.so  +  libcuda.so
        |
        v
   NVIDIA GPU (NVENC engine)
```

Two frame input paths:

- **Internal NV12 surfaces** — `cuMemAllocPitch` allocates an NV12 buffer in
  CUDA device memory. The client uploads pixels via `vaPutImage`
  (`cuMemcpy2D_v2`), then `NvEncRegisterResource(CUDADEVICEPTR)` hands it to
  NVENC. Used by ffmpeg `hwupload` and OBS.
- **DMA-BUF zero-copy** — the compositor passes a DRM PRIME file descriptor.
  `cuImportExternalMemory(OPAQUE_FD)` maps it into the CUDA address space as a
  mipmapped array. `NvEncRegisterResource(CUDAARRAY)` registers it with NVENC
  without any CPU copy. Used by Chromium/PipeWire screen share.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for a detailed design document.

---

## Project Status and Roadmap

### Working

- H.264 encode: `VAProfileH264ConstrainedBaseline` and `VAProfileH264Main`
- ABI compatibility with libva 1.0 through 1.23 (all 24 `__vaDriverInit_1_*`
  aliases)
- Internal NV12 surfaces via `cuMemAllocPitch` + `NvEncRegisterResource`
- DMA-BUF zero-copy surface import via `cuImportExternalMemory`
- Dynamic rate-control and framerate reconfiguration per-frame via
  `VAEncMiscParameterBufferType` (Chromium WebRTC `RateController` path)
- 6-slot bitstream output pool with `NV_ENC_ERR_NEED_MORE_INPUT` handling
- `catch_unwind` at every `extern "C"` boundary (panics return
  `VA_STATUS_ERROR_UNKNOWN` instead of crossing the FFI boundary)

### Deferred

- Multi-object DMA-BUF (separate Y and UV file descriptors)
- BGRA/ARGB DMA-BUF input (needs PTX colour-conversion kernel)
- `vaExportSurfaceHandle` inverse path
- `VAProfileH264High` (struct is wired; session config needs testing)
- Decode, HEVC, AV1

---

## Testing

Run all unit tests (no GPU required):

```bash
cargo test
```

Run the ignored integration tests (requires NVIDIA GPU, `ffmpeg h264_vaapi`,
`ffprobe`):

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --test encode_ffmpeg --release -- --ignored --nocapture
```

Run the `vainfo` smoke test (requires the driver installed and `vainfo` on PATH):

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --release -- --ignored vainfo
```

Or without installing, directly from the build directory:

```bash
./tools/run-vainfo.sh
```

---

## Troubleshooting

**Driver not loading / vainfo shows no entrypoints**

Enable libva trace logging:

```bash
LIBVA_MESSAGING_LEVEL=2 \
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
vainfo
```

The output will show which `.so` was opened and whether the `__vaDriverInit`
symbol was found. Also confirm the file exists without the `lib` prefix:

```bash
ls -la ~/.local/lib/dri/nvidia_nvenc_drv_video.so
```

**Permission denied on `/dev/dri/renderD128`**

```bash
ls -la /dev/dri/renderD128
# Add yourself to the render and/or video group:
sudo usermod -aG render,video "$USER"
# Then log out and back in.
```

**Chromium / Vesktop falls back to software encode**

1. Confirm `vainfo` shows `VAEntrypointEncSlice`.
2. Check that all three feature flags (`AcceleratedVideoEncoder`,
   `VaapiOnNvidiaGPUs`, `VaapiIgnoreDriverChecks`) appear in the command line
   (`chrome://version` > Command Line).
3. Confirm `~/.config/vesktop/settings.json` contains
   `"hardwareVideoAcceleration": true`.
4. Add `--vmodule=vaapi*=3` to the launch command for detailed VAAPI logs in
   stderr.

**`nvidia-smi dmon` enc column stays at 0**

The client is using software encode. Revisit the driver load steps above.

---

## Contributing

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for build instructions, test
commands, code quality requirements, and workspace layout.

---

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).
