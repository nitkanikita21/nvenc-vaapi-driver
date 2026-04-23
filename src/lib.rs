//! Userspace VAAPI backend driver that routes H.264 hardware encoding through
//! NVIDIA NVENC.
//!
//! This crate is compiled as a `cdylib` and loaded by `libva` via `dlopen`.
//! It implements the [`VADriverVTable`] interface defined in `<va/va_backend.h>`
//! and is selected by setting:
//!
//! ```text
//! LIBVA_DRIVER_NAME=nvidia_nvenc
//! LIBVA_DRIVERS_PATH=/path/to/dir/containing/nvidia_nvenc_drv_video.so
//! ```
//!
//! See the [project README](https://github.com/yourusername/libvaapi-rust-nvenc)
//! for installation and usage instructions.
//!
//! # Architecture
//!
//! libva calls `__vaDriverInit_1_<minor>` on load. We export all 24 aliases
//! (`_1_0` through `_1_23`) so this driver loads against any libva 1.x minor
//! version. All aliases resolve to the same [`driver_init`] function.
//!
//! `driver_init` allocates a [`DriverState`] on the heap and stores it in
//! `ctx->pDriverData` via `Box::into_raw`. The corresponding `vaTerminate`
//! handler reclaims it with `Box::from_raw`. All other vtable functions reach
//! driver state through [`driver::state_from`].
//!
//! All resource pools (configs, surfaces, contexts, buffers, images) live
//! inside `DriverState` behind a single `parking_lot::Mutex<Pools>`.
//!
//! # Safety
//!
//! Every public `extern "C"` symbol in this crate crosses an FFI boundary.
//! The following invariants are relied upon from the libva side:
//!
//! - `VADriverContextP` passed to any vtable function is the same pointer
//!   returned to libva during `__vaDriverInit_1_*`, and remains valid until
//!   `vaTerminate` returns.
//! - `ctx->pDriverData` is written exclusively by `driver_init` and consumed
//!   exclusively by `terminate`; no other code reassigns it.
//! - Array pointer + length pairs (e.g. `profile_list` / `num_profiles`)
//!   satisfy the bounds stated in `<va/va_backend.h>`.
//!
//! Panics inside any `extern "C"` function are caught by `std::panic::catch_unwind`
//! at the outermost call site. Unwinding across an FFI boundary is undefined
//! behaviour in Rust; the `catch_unwind` barrier converts any panic into
//! `VA_STATUS_ERROR_UNKNOWN`, which libva propagates to the caller as an
//! error return rather than crashing the process. The `Cargo.toml` profile
//! sets `panic = "unwind"` (not `abort`) to make this viable.
//!
//! All `unsafe` blocks are annotated with a `// SAFETY:` comment explaining
//! the invariant that makes the operation sound.

#![deny(improper_ctypes_definitions)]

mod cuda;
mod driver;
mod error;
mod h264;
mod ids;
mod logging;
mod nvenc;

use crate::driver::DriverState;
use crate::error::{VAStatus, VA_STATUS_SUCCESS, VA_STATUS_UNKNOWN};
use std::panic::{AssertUnwindSafe, catch_unwind};
use va_sys as va;

/// Single real entry-point. All `__vaDriverInit_1_*` symbols alias this.
///
/// # Safety
/// `ctx` must point to a live `VADriverContext` allocated by libva. We write
/// its `pDriverData` and `vtable` fields and read `drm_state`.
unsafe extern "C" fn driver_init(ctx: va::VADriverContextP) -> VAStatus {
    match catch_unwind(AssertUnwindSafe(|| unsafe { driver_init_inner(ctx) })) {
        Ok(s) => s,
        Err(_) => VA_STATUS_UNKNOWN,
    }
}

unsafe fn driver_init_inner(ctx: va::VADriverContextP) -> VAStatus {
    if ctx.is_null() {
        return error::DriverError::InvalidParameter.to_status();
    }
    // SAFETY: caller guarantees ctx is a valid VADriverContext.
    let ctx_ref = unsafe { &mut *ctx };

    // 1. Allocate driver state.
    let state = match DriverState::new(ctx) {
        Ok(s) => s,
        Err(e) => {
            // Deliberately do NOT route this through logging::error: some
            // clients (OBS's obs-nvenc/obs-qsv11 after a prior module load)
            // leave `info_callback` / `error_callback` pointing at a stale
            // function that segfaults when called. An init-time failure log
            // is not worth a crash.
            return e.to_status();
        }
    };
    let state_box = Box::new(state);
    ctx_ref.pDriverData = Box::into_raw(state_box).cast();

    // 2. Declare capability counts so libva allocates the query arrays big
    //    enough.
    ctx_ref.version_major = va::VA_MAJOR_VERSION as _;
    ctx_ref.version_minor = va::VA_MINOR_VERSION as _;
    ctx_ref.max_profiles = 8;
    ctx_ref.max_entrypoints = 4;
    ctx_ref.max_attributes = 32;
    ctx_ref.max_image_formats = 4;
    ctx_ref.max_subpic_formats = 1; // libva's init validator requires > 0
    ctx_ref.max_display_attributes = 1;
    ctx_ref.str_vendor = VENDOR_STR.as_ptr().cast();

    // 3. Fill the vtable. libva allocates it with calloc() before calling us;
    //    we only need to populate the fields we implement.
    let vt = ctx_ref.vtable;
    if vt.is_null() {
        return VA_STATUS_UNKNOWN;
    }
    // SAFETY: libva-owned vtable, zero-initialised.
    let vt = unsafe { &mut *vt };
    install_vtable(vt);

    // NOTE: no info_callback invocation here. See OBS crash history:
    // repeated vaInitialize from obs-qsv11/obs-nvenc module load paths hands
    // us a context whose `info_callback` points into a since-unloaded module,
    // so calling it crashes with SIGSEGV even though the Option is `Some(..)`.
    VA_STATUS_SUCCESS
}

// NUL-terminated vendor string (kept in .rodata). `ctx->str_vendor` is a
// `*const c_char` that must outlive the driver; static is the simplest way.
static VENDOR_STR: &[u8] = b"nvidia_nvenc-rs (Rust NVENC VAAPI backend)\0";

fn install_vtable(vt: &mut va::VADriverVTable) {
    use crate::driver::{
        buffer, config, context, display_attr, export, image, picture, surface, sync,
    };

    vt.vaTerminate = Some(terminate);

    vt.vaQueryConfigProfiles = Some(config::query_config_profiles);
    vt.vaQueryConfigEntrypoints = Some(config::query_config_entrypoints);
    vt.vaGetConfigAttributes = Some(config::get_config_attributes);
    vt.vaCreateConfig = Some(config::create_config);
    vt.vaDestroyConfig = Some(config::destroy_config);
    vt.vaQueryConfigAttributes = Some(config::query_config_attributes);

    vt.vaCreateSurfaces = Some(surface::create_surfaces);
    vt.vaCreateSurfaces2 = Some(surface::create_surfaces2);
    vt.vaDestroySurfaces = Some(surface::destroy_surfaces);
    vt.vaQuerySurfaceStatus = Some(surface::query_surface_status);
    vt.vaQuerySurfaceAttributes = Some(surface::query_surface_attributes);

    vt.vaCreateContext = Some(context::create_context);
    vt.vaDestroyContext = Some(context::destroy_context);

    vt.vaCreateBuffer = Some(buffer::create_buffer);
    vt.vaMapBuffer = Some(buffer::map_buffer);
    vt.vaMapBuffer2 = Some(buffer::map_buffer2);
    vt.vaUnmapBuffer = Some(buffer::unmap_buffer);
    vt.vaDestroyBuffer = Some(buffer::destroy_buffer);
    vt.vaBufferSetNumElements = Some(buffer::buffer_set_num_elements);
    vt.vaBufferInfo = Some(buffer::buffer_info);

    vt.vaBeginPicture = Some(picture::begin_picture);
    vt.vaRenderPicture = Some(picture::render_picture);
    vt.vaEndPicture = Some(picture::end_picture);

    vt.vaSyncSurface = Some(sync::sync_surface);
    vt.vaSyncSurface2 = Some(sync::sync_surface2);
    vt.vaSyncBuffer = Some(sync::sync_buffer);

    vt.vaQueryImageFormats = Some(image::query_image_formats);
    vt.vaCreateImage = Some(image::create_image);
    vt.vaDeriveImage = Some(image::derive_image);
    vt.vaDestroyImage = Some(image::destroy_image);
    vt.vaSetImagePalette = Some(image::set_image_palette);
    vt.vaGetImage = Some(image::get_image);
    vt.vaPutImage = Some(image::put_image);
    vt.vaQuerySubpictureFormats = Some(image::query_subpicture_formats);
    vt.vaCreateSubpicture = Some(image::create_subpicture);
    vt.vaDestroySubpicture = Some(image::destroy_subpicture);
    vt.vaSetSubpictureImage = Some(image::set_subpicture_image);
    vt.vaSetSubpictureChromakey = Some(image::set_subpicture_chromakey);
    vt.vaSetSubpictureGlobalAlpha = Some(image::set_subpicture_global_alpha);
    vt.vaAssociateSubpicture = Some(image::associate_subpicture);
    vt.vaDeassociateSubpicture = Some(image::deassociate_subpicture);
    vt.vaPutSurface = Some(image::put_surface);
    vt.vaLockSurface = Some(image::lock_surface);
    vt.vaUnlockSurface = Some(image::unlock_surface);
    vt.vaQuerySurfaceError = Some(image::query_surface_error);

    vt.vaExportSurfaceHandle = Some(export::export_surface_handle);

    vt.vaQueryDisplayAttributes = Some(display_attr::query_display_attributes);
    vt.vaGetDisplayAttributes = Some(display_attr::get_display_attributes);
    vt.vaSetDisplayAttributes = Some(display_attr::set_display_attributes);
}

unsafe extern "C" fn terminate(ctx: va::VADriverContextP) -> VAStatus {
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        if ctx.is_null() { return VA_STATUS_SUCCESS; }
        let p = (*ctx).pDriverData as *mut DriverState;
        if !p.is_null() {
            // SAFETY: pDriverData came from Box::into_raw in driver_init.
            drop(Box::from_raw(p));
            (*ctx).pDriverData = core::ptr::null_mut();
        }
        VA_STATUS_SUCCESS
    })) {
        Ok(s) => s,
        Err(_) => VA_STATUS_UNKNOWN,
    }
}

// -- __vaDriverInit_1_{0..=23} exports ---------------------------------------
// libva tries the exact-minor symbol first, then falls back. We alias all
// 24 minors to the same function via #[export_name], which in edition 2024
// requires the `unsafe(...)` attribute form.

macro_rules! va_init_export {
    ($name:literal, $fn:ident) => {
        #[unsafe(export_name = $name)]
        pub unsafe extern "C" fn $fn(ctx: va::VADriverContextP) -> VAStatus {
            // SAFETY: libva passes a valid driver context.
            unsafe { driver_init(ctx) }
        }
    };
}

va_init_export!("__vaDriverInit_1_0",  __va_init_1_0);
va_init_export!("__vaDriverInit_1_1",  __va_init_1_1);
va_init_export!("__vaDriverInit_1_2",  __va_init_1_2);
va_init_export!("__vaDriverInit_1_3",  __va_init_1_3);
va_init_export!("__vaDriverInit_1_4",  __va_init_1_4);
va_init_export!("__vaDriverInit_1_5",  __va_init_1_5);
va_init_export!("__vaDriverInit_1_6",  __va_init_1_6);
va_init_export!("__vaDriverInit_1_7",  __va_init_1_7);
va_init_export!("__vaDriverInit_1_8",  __va_init_1_8);
va_init_export!("__vaDriverInit_1_9",  __va_init_1_9);
va_init_export!("__vaDriverInit_1_10", __va_init_1_10);
va_init_export!("__vaDriverInit_1_11", __va_init_1_11);
va_init_export!("__vaDriverInit_1_12", __va_init_1_12);
va_init_export!("__vaDriverInit_1_13", __va_init_1_13);
va_init_export!("__vaDriverInit_1_14", __va_init_1_14);
va_init_export!("__vaDriverInit_1_15", __va_init_1_15);
va_init_export!("__vaDriverInit_1_16", __va_init_1_16);
va_init_export!("__vaDriverInit_1_17", __va_init_1_17);
va_init_export!("__vaDriverInit_1_18", __va_init_1_18);
va_init_export!("__vaDriverInit_1_19", __va_init_1_19);
va_init_export!("__vaDriverInit_1_20", __va_init_1_20);
va_init_export!("__vaDriverInit_1_21", __va_init_1_21);
va_init_export!("__vaDriverInit_1_22", __va_init_1_22);
va_init_export!("__vaDriverInit_1_23", __va_init_1_23);

