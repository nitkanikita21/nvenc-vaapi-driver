//! VAAPI synchronisation vtable entries: `vaSyncSurface`, `vaSyncSurface2`,
//! `vaSyncBuffer`.
//!
//! NVENC encode is synchronous from the driver's perspective: by the time
//! `vaEndPicture` returns, the encode call and `NvEncLockBitstream` will have
//! completed (once the real backend is wired in). Therefore these sync entry
//! points are permanent no-ops that return `VA_STATUS_SUCCESS` immediately.

use super::guard;
use crate::error::VAStatus;
use va_sys as va;

pub unsafe extern "C" fn sync_surface(
    _ctx: va::VADriverContextP,
    _render_target: va::VASurfaceID,
) -> VAStatus {
    guard(|| Ok(()))
}

pub unsafe extern "C" fn sync_surface2(
    _ctx: va::VADriverContextP,
    _render_target: va::VASurfaceID,
    _timeout_ns: u64,
) -> VAStatus {
    guard(|| Ok(()))
}

pub unsafe extern "C" fn sync_buffer(
    _ctx: va::VADriverContextP,
    _buf_id: va::VABufferID,
    _timeout_ns: u64,
) -> VAStatus {
    guard(|| Ok(()))
}
