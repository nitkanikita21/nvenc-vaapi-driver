# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Upload contiguous NV12 image planes with one pitched CUDA transfer instead
  of two, validating plane offsets, stride and allocation bounds first.
- Allow the benchmark tool to select a driver directory for build comparisons.

- Initialise NVENC lazily from actual H.264 parameters and apply dynamic rate
  control without forcing IDR for ordinary bitrate/framerate changes.
- Load NVENC at runtime with explicit version/function validation; remove
  link-time dependencies on NVIDIA encoding and decoding libraries.
- Reuse coded-output storage across frames and cache input registrations per
  session. Encode completion publishes the corresponding picture synchronously.
- Install the library by atomic replacement. Optional session selection has a
  matching removal command and does not overwrite unmanaged configuration.

### Fixed

- Download current pixels when mapping derived NV12 images for reading;
  honour read/write mapping flags and reject unknown access bits.
- Reject unsupported CPU image access instead of silently skipping uploads,
  and validate image dimensions before allocating objects.

- Preserve rational frame rates and visible dimensions after H.264 cropping.
- Keep VBR average and peak bitrate separate and honour constant-QP input.
- Never reuse public object IDs; reject stale handles after destruction.
- Reject conflicting mapped-buffer operations before locking or replacing
  storage; validate coded output before submitting GPU work.
- Retain context configuration after its public config object is destroyed.
- Unregister inputs before releasing surface memory, including termination.
- Validate external NV12 descriptor bounds and transferred FD ownership.
- Stop advertising packed-header passthrough that is not implemented.

### Added

- NV12 image readback with chroma-aligned cropping and regression coverage
  for mapping access, copies between surfaces and stale surface handles.

- Regression coverage for ABI lifecycle, dynamic rate control, shared inputs,
  full-stream decoding, installation and runtime API loading.
- Comparative benchmark tool with output validation and separate pacing modes.

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

[Unreleased]: https://github.com/nitkanikita21/nvenc-vaapi-driver/compare/HEAD
