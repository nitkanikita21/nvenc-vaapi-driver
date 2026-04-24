# Development Guide

## Prerequisites

```bash
# Arch Linux
sudo pacman -S rust libva libva-utils clang

# Ubuntu / Debian
sudo apt install rustup libva-dev libva-utils clang libclang-dev
```

`clang` and `libclang-dev` are required by `bindgen` to parse the libva headers
at build time. If `clang` is absent, `cargo build` will fail with a message from
`bindgen` about a missing `libclang`.

NVIDIA Video Codec SDK headers are located automatically. If they are not in
standard system paths, set:

```bash
export NVIDIA_VIDEO_CODEC_SDK_PATH=/opt/nvidia-video-codec-sdk
```

---

## Building

```bash
cargo build --release
```

The artifact is `target/release/libnvidia_nvenc_drv_video.so`. The `lib` prefix
is added by Cargo's `cdylib` target; libva expects the file without it. The
install script and the `run-vainfo.sh` helper handle this automatically.

Check that all expected ABI symbols are present:

```bash
nm -D target/release/libnvidia_nvenc_drv_video.so | grep __vaDriverInit
```

You should see entries from `__vaDriverInit_1_0` through `__vaDriverInit_1_23`.

---

## Running vainfo Without Installing

```bash
./tools/run-vainfo.sh
```

The script creates `target/release/nvidia_nvenc_drv_video.so` as a symlink to
`libnvidia_nvenc_drv_video.so` (the name libva expects), sets the required
environment variables, and runs `vainfo`.

---

## Installing for Manual Testing

```bash
bash tools/install-driver.sh
```

This copies the built `.so` to `~/.local/lib/dri/nvidia_nvenc_drv_video.so`.

Set the two environment variables needed by every client application:

```bash
export LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri"
export LIBVA_DRIVER_NAME="nvidia_nvenc"
```

---

## Iteration Loop (pointing a client at a freshly built driver)

The typical inner loop when iterating on encode behaviour:

```bash
# 1. Build
cargo build --release

# 2. Reinstall (one-liner)
bash tools/install-driver.sh

# 3. Test with ffmpeg (no OBS / browser restart needed)
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
ffmpeg \
  -f lavfi -i "testsrc2=size=1280x720:rate=60:duration=2" \
  -vaapi_device /dev/dri/renderD128 \
  -vf "format=nv12,hwupload" \
  -c:v h264_vaapi -profile:v main -b:v 5M \
  /tmp/test.mp4 && \
ffprobe -v error -show_streams /tmp/test.mp4 | grep codec_name
```

For OBS or Vesktop you need to relaunch the application after reinstalling; the
`.so` is loaded at startup via `dlopen` and is not hot-reloaded.

---

## Running Tests

### Unit tests (no GPU or CUDA required)

```bash
cargo test
```

The unit tests in `src/nvenc/preset.rs`, `src/nvenc/h264_config.rs`,
`src/driver/state.rs`, `src/ids.rs`, and `src/error.rs` run without any GPU or
CUDA installation.

### All tests including integration tests

```bash
cargo test --all
```

### Ignored smoke tests (require NVIDIA GPU)

The following tests are marked `#[ignore]` and need a live NVIDIA GPU, `ffmpeg`
with `h264_vaapi`, `ffprobe`, and the driver installed:

```bash
# Full ffmpeg encode + ffprobe validation (720p60 and 480p30)
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --test encode_ffmpeg --release -- --ignored --nocapture

# vainfo smoke (checks the .so loads and advertises the right profiles)
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --release -- --ignored vainfo --nocapture
```

To run a single ignored test by name:

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME=nvidia_nvenc \
cargo test --test encode_ffmpeg --release -- --ignored smoke_encode_h264_720p60 --nocapture
```

### Test output

Pass `--nocapture` to see `println!` / `eprintln!` output from tests and the
ffmpeg/ffprobe stderr in real time.

---

## Code Quality

All three checks must pass cleanly before a PR is merged:

```bash
cargo fmt --check      # verify formatting
cargo fmt              # apply formatting
cargo clippy -- -D warnings
```

---

## Regenerating Bindings After a libva Header Update

The `va-sys` crate generates bindings at build time via `bindgen`. If you upgrade
the system libva headers, the bindings regenerate automatically on the next
`cargo build`. To force a regeneration:

```bash
touch crates/va-sys/wrapper.h
cargo build
```

**Important:** `bindgen` must be version 0.72 or later. Version 0.71 produces
opaque stubs for `VADriverContext` due to a bug with forward-declared structs.
The version is pinned in `crates/va-sys/Cargo.toml`.

If you see compile errors like `no field 'pDriverData' on type 'VADriverContext'`,
the wrong bindgen version is in use — check with:

```bash
cargo tree -p va-sys
```

---

## Workspace Layout

```
nvenc-vaapi-driver/
  Cargo.toml           root package + workspace manifest (cdylib name: nvidia_nvenc_drv_video)
  CLAUDE.md            developer-internal notes (Ukrainian); source of truth for context
  CHANGELOG.md         keep-a-changelog formatted change log
  src/
    lib.rs             crate root: ABI exports (__vaDriverInit_1_0..1_23), vtable install
    error.rs           DriverError <-> VAStatus translation
    ids.rs             generational ID pools (key_to_id / id_to_key)
    logging.rs         libva info/error callbacks
    h264/
      mod.rs           H.264 parameter buffer parser (parse_sps, parse_pps)
    driver/
      mod.rs           state_from(), guard() helpers
      state.rs         DriverState, Pools, PendingFrame, all record types
      config.rs        vaCreateConfig and friends
      surface.rs       vaCreateSurfaces / DMA-BUF import
      context.rs       vaCreateContext / vaDestroyContext
      buffer.rs        vaCreateBuffer / vaMapBuffer / vaUnmapBuffer
      picture.rs       vaBeginPicture / vaRenderPicture / vaEndPicture
      sync.rs          vaSyncSurface / vaSyncBuffer
      image.rs         VAImage / VASubpicture (stubs satisfy libva validator)
      export.rs        vaExportSurfaceHandle (stub)
      display_attr.rs  vaQueryDisplayAttributes
    nvenc/
      mod.rs           module entry, NvencSession re-export
      session.rs       NvencSession: open-init-register-encode-lock-unmap-destroy
      preset.rs        bitrate/fps -> preset/tuning/RC mapping (GPU-free, unit-tested)
      h264_config.rs   apply_config(): mutates NV_ENC_CONFIG in place
    cuda/
      mod.rs           CudaCtx: DRM fd -> CUDA context
      external_mem.rs  cuImportExternalMemory for DMA-BUF surfaces
  crates/
    va-sys/
      Cargo.toml
      build.rs         bindgen invocation
      wrapper.h        #include directives for va/va_backend.h etc.
      src/lib.rs       re-exports generated bindings
  tools/
    install-driver.sh  copy .so to ~/.local/lib/dri/ with correct name
    run-vainfo.sh      run vainfo directly from build dir
  tests/
    panic_safety.rs    ABI boundary does not crash on null/bogus input
    smoke_driver_init.rs  vtable, __vaDriverInit_* symbols
    vainfo_smoke.rs    (#[ignore]) system vainfo sees our driver
    encode_ffmpeg.rs   (#[ignore]) 720p60 live encode via ffmpeg h264_vaapi
  docs/
    ARCHITECTURE.md    internal design document
    DEVELOPMENT.md     this file
```

---

## Key Invariants to Preserve

These invariants are easy to break accidentally and have caused bugs in earlier
iterations:

1. **Never call `info_callback` or `error_callback` during `__vaDriverInit_1_*`.**
   Some clients (OBS after obs-qsv11 module unload) leave stale callback
   pointers that segfault when called. Log init failures via return code only.

2. **Always read `drm_state->fd` only when `display_type == VA_DISPLAY_DRM` or
   `VA_DISPLAY_DRM_RENDERNODES`.** Dereferencing `drm_state` on a non-DRM
   display type is a SIGSEGV.

3. **`NvEncUnregisterResource` must precede CUDA mipmap/external-memory
   destruction for DMA-BUF surfaces.** The NVIDIA driver crashes if the CUDA
   array is destroyed while NVENC still holds a registration against it.

4. **NVENC's SPS/PPS always wins.** Do not mix client-supplied packed headers
   with NVENC-generated SPS/PPS in the same stream. Earlier iterations that
   toggled `repeatSPSPPS` per-frame caused streams to die after ~4 frames
   because receivers saw alternating byte-different SPS NALs.

5. **VBV buffer = 1 second of bitrate, not 1 frame.** Per-frame VBV sizing
   starves NVENC at low WebRTC bitrates and causes it to produce malformed
   bitstream segments on IDR frames.
