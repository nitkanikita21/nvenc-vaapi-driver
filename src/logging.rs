//! Пересилання внутрішніх повідомлень драйвера до libva info/error callbacks.
//!
//! libva встановлює два необов'язкові callback-и у `VADriverContext`:
//! `info_callback` і `error_callback`. Обидва мають сигнатуру
//! `fn(VADriverContextP, *const c_char)` і можуть бути `NULL`, якщо libva
//! не встановила їх (наприклад при `LIBVA_MESSAGING_LEVEL=0`).
//!
//! Цей модуль надає [`info`] і [`error`] — тонкі обгортки, які:
//! 1. Перевіряють, що `ctx` та callback ненульові.
//! 2. Конвертують `&str` у `CString` (додають NUL-термінатор).
//! 3. Викликають відповідний callback.
//!
//! Повідомлення з внутрішнім NUL-байтом мовчки ігноруються
//! (`CString::new` повертає `Err`), що безпечно для FFI-контексту.

use core::ffi::c_char;
use std::ffi::CString;
use va_sys as va;

#[inline]
fn send(cb: Option<unsafe extern "C" fn(va::VADriverContextP, *const c_char)>,
        ctx: va::VADriverContextP, msg: &str) {
    let Some(cb) = cb else { return };
    let Ok(cstr) = CString::new(msg) else { return };
    // SAFETY: cb is non-null (we just checked), ctx is the driver context
    // libva passed us, the string is NUL-terminated and lives for the call.
    unsafe { cb(ctx, cstr.as_ptr()) };
}

/// Надіслати інформаційне повідомлення через `VADriverContext::info_callback`.
///
/// Викликається з `src/lib.rs` та vtable-функцій для логування стану драйвера.
/// Якщо `ctx` нульовий або callback не встановлено — нічого не відбувається.
pub fn info(ctx: va::VADriverContextP, msg: &str) {
    if ctx.is_null() { return; }
    // SAFETY: ctx is non-null per check; libva guarantees the struct is valid.
    let cb = unsafe { (*ctx).info_callback };
    send(cb, ctx, msg);
}

/// Надіслати повідомлення про помилку через `VADriverContext::error_callback`.
///
/// Використовується при відмові ініціалізації або невідновних помилках encode.
/// Якщо `ctx` нульовий або callback не встановлено — нічого не відбувається.
pub fn error(ctx: va::VADriverContextP, msg: &str) {
    if ctx.is_null() { return; }
    // SAFETY: see `info`.
    let cb = unsafe { (*ctx).error_callback };
    send(cb, ctx, msg);
}
