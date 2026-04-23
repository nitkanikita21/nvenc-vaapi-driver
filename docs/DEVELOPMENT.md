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

## Building

```bash
cargo build --release
```

The artifact is `target/release/libnvidia_nvenc_drv_video.so`. The `lib` prefix
is added by Cargo's `cdylib` target; libva expects the file without it — see the
next section.

## Running vainfo Without Installing

```bash
./tools/run-vainfo.sh
```

The script symlinks `nvidia_nvenc_drv_video.so -> libnvidia_nvenc_drv_video.so`
in the build output directory and sets the required environment variables.

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
the wrong bindgen version is being used — check `cargo tree -p va-sys`.

## Running Tests

```bash
# All tests (unit + integration)
cargo test

# Preset logic only (no hardware needed)
cargo test -p libvaapi-rust-nvenc nvenc::preset

# With output shown
cargo test -- --nocapture
```

The unit tests in `src/nvenc/preset.rs`, `src/ids.rs`, and `src/error.rs` run
without any GPU or CUDA installation.

## Code Quality

```bash
cargo fmt --check      # check formatting
cargo fmt              # apply formatting
cargo clippy -- -D warnings
```

All three must pass cleanly before a PR is merged.

## Workspace Layout

```
libvaapi-rust-nvenc/
  Cargo.toml           root package + workspace manifest
  src/
    lib.rs             crate root: ABI exports, vtable installation
    error.rs           DriverError <-> VAStatus translation
    ids.rs             generational ID pools (key_to_id / id_to_key)
    logging.rs         libva info/error callbacks
    driver/
      mod.rs           state_from(), guard() helpers
      state.rs         DriverState, Pools, all record types
      config.rs        vaCreateConfig and friends
      surface.rs       vaCreateSurfaces / DMA-BUF import
      context.rs       vaCreateContext / vaDestroyContext
      buffer.rs        vaCreateBuffer / vaMapBuffer / vaUnmapBuffer
      picture.rs       vaBeginPicture / vaRenderPicture / vaEndPicture
      sync.rs          vaSyncSurface (no-op)
      image.rs         VAImage / VASubpicture stubs
      export.rs        vaExportSurfaceHandle stub
      display_attr.rs  vaQueryDisplayAttributes
    nvenc/
      mod.rs           module entry, NvencSession re-export
      session.rs       NvencSession placeholder
      preset.rs        bitrate/fps -> preset/tuning/RC mapping
    cuda/
      mod.rs           CudaCtx placeholder (DRM fd dup)
    h264/
      mod.rs           H.264 parameter buffer parser (placeholder)
  crates/
    va-sys/
      Cargo.toml
      build.rs         bindgen invocation
      wrapper.h        #include directives for va/va_backend.h etc.
      src/lib.rs       re-exports generated bindings
  tools/
    install-driver.sh  copy .so to ~/.local/lib/dri/ with correct name
    run-vainfo.sh      run vainfo directly from build dir
  docs/
    ARCHITECTURE.md    this codebase's design document
    DEVELOPMENT.md     this file
```
