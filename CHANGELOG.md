# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- feat(rate-control): parse `VAEncMiscParameterBufferType` subtypes 0
  (`VAEncMiscParameterTypeFrameRate`) and 1 (`VAEncMiscParameterTypeRateControl`)
  in `render_picture`. The parsed values are stashed as `PendingFrame::misc_fps`
  and `PendingFrame::misc_bitrate_bps` respectively (`src/driver/state.rs`).
  In `end_picture`, if either field is present, `NvencSession::reconfigure` is
  called with the updated `EncoderConfig` before encoding the frame, so the new
  bitrate or framerate takes effect on the very next frame with a forced IDR.
  This is required for Chromium WebRTC `RateController` compatibility:
  Chromium sends a rate-control update on virtually every frame and penalises
  encoders that ignore it by cycling them out in favour of software OpenH264.

### Fixed

- fix(vbv): VBV buffer (`vbvBufferSize`) is now sized at 1 second of bitrate
  (i.e. equal to `bitrate_bps`) with an initial delay of 0.5 seconds
  (`bitrate_bps / 2`). The prior implementation sized it at one frame worth of
  bits (`bitrate / fps`), which at low WebRTC targets such as 244 Kbps/60 fps
  yielded a ~500-byte VBV buffer. NVENC then had insufficient headroom to
  accommodate IDR frames (which can be 10–20 KB), causing it to either drop
  frames or produce malformed bitstream segments. Chromium's encoder quality
  controller interpreted the resulting gaps as encoder failures and switched back
  to software OpenH264 after its 8-frame trial window.

---

### Added (earlier)

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
- `src/nvenc/preset.rs`: bitrate/fps/RC-mode to NVENC preset + tuning + RC
  selection logic with unit tests (runs without CUDA hardware).
- `src/cuda/mod.rs`: `CudaCtx` placeholder that dup's the DRM fd from
  `ctx->drm_state`; correct `Drop` implementation closes the dup'd fd.
- `src/nvenc/session.rs`: `NvencSession` with real encode pipeline
  (`NvEncOpenEncodeSessionEx`, `NvEncRegisterResource`, `NvEncEncodePicture`,
  `NvEncLockBitstream`). 6-slot bitstream output pool with round-robin
  scheduling and `NV_ENC_ERR_NEED_MORE_INPUT` deferral handling.
- `src/h264/mod.rs`: H.264 parameter buffer parser (`parse_sps`, `parse_pps`).
- `src/logging.rs`: forwarding to libva `info_callback` / `error_callback`.
- `crates/va-sys`: `bindgen 0.72`-based FFI bindings to `va_backend.h`,
  `va.h`, `va_enc_h264.h`, `va_drmcommon.h`. Does not link libva.
- `tools/install-driver.sh`: copies release `.so` to `~/.local/lib/dri/`
  with the correct `nvidia_nvenc_drv_video.so` name (without `lib` prefix).
- `tools/run-vainfo.sh`: runs `vainfo` directly from the build directory via a
  temporary symlink; no install step required.
- `panic = "unwind"` in release profile + `catch_unwind` at every `extern "C"`
  boundary to prevent undefined behaviour from Rust panics crossing FFI.
- `vainfo` smoke test passes: reports `VAProfileH264ConstrainedBaseline` and
  `VAProfileH264Main` with `VAEntrypointEncSlice`.
- VA image path: `vaCreateImage`, `vaDeriveImage`, `vaDestroyImage`,
  `vaPutImage`, plus `vaMapBuffer` / `vaUnmapBuffer` for `VAImageBufferType`.
  Unmapping a derived image bound to a CUDA surface triggers a host-to-device
  `cuMemcpy2D_v2`, unblocking `ffmpeg h264_vaapi` + `hwupload`.
- `end_picture` lazily calls `register_internal_surface` on the current render
  target if it was not listed at `vaCreateContext` time, so ffmpeg-style
  late-bound render targets encode without workarounds.
- DMA-BUF zero-copy surface import via `cuImportExternalMemory` +
  `cuExternalMemoryGetMappedMipmappedArray`. Advertises `DRM_PRIME_2` in
  `VASurfaceAttribMemoryType` and `DRM_FORMAT_MOD_INVALID` in
  `VASurfaceAttribDRMFormatModifiers` so Chromium/Vesktop uses the zero-copy
  path for Wayland/PipeWire screen share. NVENC registration uses
  `NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY` and is deferred to the first
  `vaEndPicture`. Currently single-object NV12 only; multi-object layouts and
  BGRA colour conversion are deferred.

[Unreleased]: https://github.com/yourusername/libvaapi-rust-nvenc/compare/HEAD
