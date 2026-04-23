//! VAAPI surface export vtable entry: `vaExportSurfaceHandle`.
//!
//! This entry point allows a consumer (e.g. a compositor or display pipeline)
//! to receive a DRM PRIME fd for a surface that was produced by the encoder.
//! It is a post-MVP feature and currently returns
//! `VA_STATUS_ERROR_UNIMPLEMENTED`. The implementation will use
//! `cuExternalMemoryGetMappedBuffer` + `drmPrimeHandleToFD` once the CUDA
//! backend is active.

use super::guard;
use crate::error::{DriverError, VAStatus};
use core::ffi::c_void;
use va_sys as va;

pub unsafe extern "C" fn export_surface_handle(
    _ctx: va::VADriverContextP,
    _surface_id: va::VASurfaceID,
    _mem_type: u32,
    _flags: u32,
    _descriptor: *mut c_void,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}
