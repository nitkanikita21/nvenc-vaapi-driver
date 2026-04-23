//! Реалізації слотів `VADriverVTable`, розкладені по субмодулях.
//!
//! Кожна `extern "C"` функція — тонка обгортка, що:
//! 1. Отримує `&DriverState` через [`state_from`] з `ctx->pDriverData`.
//! 2. Обгортає тіло у [`guard`] (`catch_unwind` + `DriverError → VAStatus`).
//! 3. Повертає `VAStatus` назад у libva.
//!
//! # Розкладка субмодулів
//!
//! | Файл | Vtable-слоти |
//! |---|---|
//! | `config.rs` | `vaCreateConfig`, `vaQueryConfigProfiles`, `vaGetConfigAttributes` |
//! | `surface.rs` | `vaCreateSurfaces`, `vaCreateSurfaces2`, `vaDestroySurfaces` |
//! | `context.rs` | `vaCreateContext`, `vaDestroyContext` |
//! | `buffer.rs` | `vaCreateBuffer`, `vaMapBuffer`, `vaUnmapBuffer`, `vaDestroyBuffer` |
//! | `picture.rs` | `vaBeginPicture`, `vaRenderPicture`, `vaEndPicture` |
//! | `sync.rs` | `vaSyncSurface`, `vaSyncSurface2`, `vaSyncBuffer` |
//! | `image.rs` | стаби image/subpicture (вимагає libva validator) |
//! | `export.rs` | `vaExportSurfaceHandle` (stub, post-MVP) |
//! | `display_attr.rs` | `vaQueryDisplayAttributes` та інші |
//! | `state.rs` | `DriverState`, `Pools`, типи записів |
//!
//! Поля записів, що залежать від NVENC/CUDA-бекенду, наразі заглушені.
//! Дивіться TODO у відповідних модулях.
#![allow(dead_code)]

pub mod state;
pub mod config;
pub mod surface;
pub mod context;
pub mod buffer;
pub mod picture;
pub mod sync;
pub mod image;
pub mod export;
pub mod display_attr;

use crate::error::{DriverError, VAStatus, VA_STATUS_SUCCESS, VA_STATUS_UNKNOWN};
use std::panic::{AssertUnwindSafe, catch_unwind};
use va_sys as va;

pub use state::DriverState;

/// Retrieve the `&DriverState` from the driver context. Returns `None` if
/// the context or its driver data pointer is null.
///
/// # Safety
/// Caller asserts that `ctx` points to a valid `VADriverContext` whose
/// `pDriverData` (if non-null) was produced by `Box::into_raw::<DriverState>`.
#[inline]
pub(crate) unsafe fn state_from<'a>(ctx: va::VADriverContextP) -> Option<&'a DriverState> {
    if ctx.is_null() { return None; }
    // SAFETY: caller guarantees ctx is valid for reads.
    let p = unsafe { (*ctx).pDriverData } as *const DriverState;
    if p.is_null() { return None; }
    // SAFETY: pDriverData was Box::into_raw'd in __vaDriverInit, still live
    // until vaTerminate consumes it.
    Some(unsafe { &*p })
}

/// Guard that converts any panic or `DriverError` into a `VAStatus`.
#[inline]
pub(crate) fn guard<F>(f: F) -> VAStatus
where
    F: FnOnce() -> Result<(), DriverError>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => VA_STATUS_SUCCESS,
        Ok(Err(e)) => e.to_status(),
        Err(_) => VA_STATUS_UNKNOWN,
    }
}
