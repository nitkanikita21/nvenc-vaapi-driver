//! Raw libva backend ABI bindings, згенеровані з `va_backend.h`.
//!
//! # Навіщо окремий sys-крейт
//!
//! Існуючі libva-binding крейти (`libva-sys`, `cros-libva`, `fev`) — це
//! **клієнтські** обгортки: вони лінкуються з `libva.so.2` і викликають її
//! функції. Ми ж **реалізуємо** той самий ABI — ті самі типи, але з іншого
//! боку `dlopen`-межі.
//!
//! Тому `va-sys`:
//! - Виконує `bindgen` на системний `/usr/include/va/va_backend.h`
//!   (разом з `va.h`, `va_enc_h264.h`, `va_drmcommon.h` транзитивно).
//! - **Не** має `cargo:rustc-link-lib=va` — ми не лінкуємося з libva,
//!   ми завантажуємося нею.
//! - Реекспортує тільки типи: `VADriverVTable`, `VADriverContext`,
//!   `VAStatus`, enum-и, `VAEnc*H264` structs, `VADRMPRIMESurfaceDescriptor`.
//!
//! # Поле `links`
//!
//! `Cargo.toml` оголошує `links = "va_backend_headers"` — marker-значення,
//! яке запобігає підключенню двох примірників `va-sys` в одному графі
//! залежностей. Це не викликає реального system-link.
//!
//! # Suppress lint
//!
//! `bindgen`-згенерований код містить `non_camel_case_types` та інші
//! порушення Rust naming conventions, тому вони всі вимкнені через
//! `#![allow(...)]` у цьому файлі.
#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unsafe_op_in_unsafe_fn,
    unused_unsafe,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/va_bindings.rs"));
