//! VAAPI display-attribute vtable entries: `vaQueryDisplayAttributes`,
//! `vaGetDisplayAttributes`, `vaSetDisplayAttributes`.
//!
//! This is an encode-only driver; no display attributes are supported.
//! `vaQueryDisplayAttributes` reports a count of zero. The get/set entries
//! return `VA_STATUS_SUCCESS` without touching the attribute list, which is
//! the correct response per the libva spec when no attributes are defined.

use super::guard;
use crate::error::VAStatus;
use core::ffi::c_int;
use va_sys as va;

pub unsafe extern "C" fn query_display_attributes(
    _ctx: va::VADriverContextP,
    _attr_list: *mut va::VADisplayAttribute,
    num_attributes: *mut c_int,
) -> VAStatus {
    guard(|| {
        if !num_attributes.is_null() {
            // SAFETY: non-null checked.
            unsafe { *num_attributes = 0 };
        }
        Ok(())
    })
}

pub unsafe extern "C" fn get_display_attributes(
    _ctx: va::VADriverContextP,
    _attr_list: *mut va::VADisplayAttribute,
    _num: c_int,
) -> VAStatus {
    crate::error::VA_STATUS_SUCCESS
}

pub unsafe extern "C" fn set_display_attributes(
    _ctx: va::VADriverContextP,
    _attr_list: *mut va::VADisplayAttribute,
    _num: c_int,
) -> VAStatus {
    crate::error::VA_STATUS_SUCCESS
}
