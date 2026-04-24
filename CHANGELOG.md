# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- VAAPI backend ABI scaffold: `__vaDriverInit_1_0` through `__vaDriverInit_1_23`
  exported as aliases of a single `driver_init` function, making the `.so`
  loadable by any libva 1.x minor version.
- Full `VADriverVTable` wiring: config, surface, context, buffer, picture, sync,
  image (stubs), display attributes, and `vaExportSurfaceHandle` (stub).
- `DriverState` lifecycle via `Box::into_raw` / `Box::from_raw` across the FFI
  boundary; `vaTerminate` correctly deallocates driver state.
- `src/ids.rs`: generational ID pools using `slotmap::DenseSlotMap` with typed
  keys (`ConfigKey`, `SurfaceKey`, `ContextKey`, `BufferKey`, `ImageKey`).
  Stale client IDs fail lookup rather than aliasing recycled slots.
- `src/error.rs`: `DriverError` enum mapping every driver error to a canonical
  `VA_STATUS_ERROR_*` code.
- `src/nvenc/preset.rs`: bitrate/fps/RC-mode → NVENC preset + tuning + RC
  selection logic with unit tests (runs without CUDA hardware).
- `src/cuda/mod.rs`: `CudaCtx` placeholder that dup's the DRM fd from
  `ctx->drm_state`; correct `Drop` implementation closes the dup'd fd.
- `src/nvenc/session.rs`: `NvencSession` placeholder with correct public shape
  (`new`, `width`, `height`) so downstream code compiles without NVENC linked.
- `src/h264/mod.rs`: reserved module surface for the H.264 parameter parser.
- `src/logging.rs`: forwarding to libva `info_callback` / `error_callback`.
- `crates/va-sys`: `bindgen 0.72`-based FFI bindings to `va_backend.h`,
  `va.h`, `va_enc_h264.h`, `va_drmcommon.h`. Does not link libva.
- `tools/install-driver.sh`: copies release `.so` to `~/.local/lib/dri/`
  with the correct `nvidia_nvenc_drv_video.so` name (without `lib` prefix).
- `tools/run-vainfo.sh`: runs `vainfo` directly from the build directory via a
  temporary symlink, no install step required.
- `panic = "unwind"` in release profile + `catch_unwind` at every `extern "C"`
  boundary to prevent undefined behaviour from Rust panics crossing FFI.
- `vainfo` smoke test passes: reports `VAProfileH264ConstrainedBaseline` and
  `VAProfileH264Main` with `VAEntrypointEncSlice`.
- Scaffolding для NVENC encode: `EncoderConfig`, `apply_config(&mut NV_ENC_CONFIG)`,
  `SurfaceKind::Internal`, `CodedBufferSlot`. Actual encoding not wired yet
  (next slice).
- `create_surfaces2` тепер явно відкидає `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`
  з `VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE` (замість мовчазного fallback).
- Real NVENC H.264 encode pipeline: `NvencSession::new` /
  `register_internal_surface` / `encode_frame` / `reconfigure` over
  `nvidia_video_codec_sdk::sys`. INTERNAL NV12 surfaces via raw
  `cuMemAllocPitch_v2`. Wiring in `context.rs` / `picture.rs` / `buffer.rs`
  complete; live GPU test deferred to c.3.
- VA image path (slice c.4): `vaCreateImage`, `vaDeriveImage`,
  `vaDestroyImage`, `vaPutImage`, plus `vaMapBuffer` / `vaUnmapBuffer`
  for `VAImageBufferType`. Unmapping a derived image bound to a CUDA
  surface triggers a host→device `cuMemcpy2D_v2` for the NV12 Y and UV
  planes, unblocking ffmpeg `h264_vaapi` + `hwupload` and any other
  CPU-origin VAAPI client. NV12 is the only supported fourcc.
- `end_picture` now lazily calls `register_internal_surface` on the
  current render target if it was not listed at `vaCreateContext` time,
  so ffmpeg-style late-bound render targets encode without workarounds.
- DMA-BUF zero-copy surface import via `cuImportExternalMemory` +
  `cuExternalMemoryGetMappedMipmappedArray`. Advertises DRM_PRIME_2 in
  `VASurfaceAttribMemoryType` and `DRM_FORMAT_MOD_INVALID` in
  `VASurfaceAttribDRMFormatModifiers` so Chromium/Vesktop stop falling
  back to software OpenH264 for Wayland/PipeWire screen share. NVENC
  registration uses `NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY` and is done
  lazily at first `vaEndPicture`. Currently single-object NV12 only;
  multi-object layouts and BGRA via CUDA colour conversion deferred.

### Not Yet Implemented

- Real NVENC encode session (`NvEncInitializeEncoder`, `NvEncEncodePicture`,
  `NvEncLockBitstream`).
- H.264 parameter buffer parsing (`VAEncSequenceParameterBufferH264` →
  `NV_ENC_CONFIG_H264`).
- `vaExportSurfaceHandle` (post-MVP).
- `VAProfileH264High`.

[Unreleased]: https://github.com/yourusername/libvaapi-rust-nvenc/compare/HEAD
