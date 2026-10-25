//! VAAPI context vtable entries: `vaCreateContext`, `vaDestroyContext`.
//!
//! A context retains the render geometry and config. NVENC is initialised on
//! the first picture, once the client has supplied its actual rate control.

use super::{guard, state_from};
use crate::driver::state::{ContextRec, PendingFrame};
use crate::error::{DriverError, VAStatus};
use crate::ids::{ContainsKey, ContextKey, SurfaceKey, key_to_id};
use core::ffi::c_int;
use parking_lot::Mutex;
use va_sys as va;

pub unsafe extern "C" fn create_context(
    ctx: va::VADriverContextP,
    config_id: va::VAConfigID,
    picture_width: c_int,
    picture_height: c_int,
    flag: c_int,
    render_targets: *mut va::VASurfaceID,
    num_render_targets: c_int,
    context_out: *mut va::VAContextID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if context_out.is_null()
            || picture_width < 0
            || picture_height < 0
            || num_render_targets < 0
            || (num_render_targets > 0 && render_targets.is_null())
        {
            return Err(DriverError::InvalidParameter);
        }
        let mut pools = state.pools.lock();
        let cfg_key = pools
            .configs
            .find_by_low_bits(config_id)
            .ok_or(DriverError::InvalidConfig)?;

        let mut targets: Vec<SurfaceKey> = Vec::new();
        if !render_targets.is_null() && num_render_targets > 0 {
            // SAFETY: caller-provided array.
            let ids =
                unsafe { core::slice::from_raw_parts(render_targets, num_render_targets as usize) };
            targets.reserve(ids.len());
            for id in ids {
                let k = pools
                    .surfaces
                    .find_by_low_bits(*id)
                    .ok_or(DriverError::InvalidSurface)?;
                targets.push(k);
            }
        }

        let mut width = u32::try_from(picture_width).unwrap_or(0);
        let mut height = u32::try_from(picture_height).unwrap_or(0);
        // Chromium's WebRTC VaapiVideoEncodeAccelerator calls vaCreateContext
        // with picture_width/height = 0 (libva treats 0 as "derive from render
        // targets"). Without this fallback NVENC gets initialised at 0×0, the
        // H.264 SPS it emits has zero picture dimensions, and Chromium's
        // `VideoFrame::coded_size()` parses as (0,0) → the WebRTC debug overlay
        // shows "undefined × undefined" and the encoder gets cycled out after
        // ~4 trial frames. Ground truth: the first render-target surface.
        if (width == 0 || height == 0) && !targets.is_empty() {
            if let Some(first) = pools.surfaces.get(targets[0]) {
                if width == 0 {
                    width = first.width;
                }
                if height == 0 {
                    height = first.height;
                }
            }
        }
        if width == 0 || height == 0 {
            return Err(DriverError::InvalidParameter);
        }
        let rec = ContextRec {
            config: pools.configs[cfg_key],
            width,
            height,
            flag,
            render_targets: targets,
            current_target: Mutex::new(None),
            session: Mutex::new(None),
            pending: Mutex::new(PendingFrame::default()),
            last_coded: Mutex::new(None),
        };
        let k: ContextKey = pools.contexts.insert(rec)?;
        // SAFETY: non-null checked.
        unsafe { *context_out = key_to_id(k) as va::VAContextID };
        Ok(())
    })
}

pub unsafe extern "C" fn destroy_context(
    ctx: va::VADriverContextP,
    context_id: va::VAContextID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let mut pools = state.pools.lock();
        let k = pools
            .contexts
            .find_by_low_bits(context_id)
            .ok_or(DriverError::InvalidContext)?;
        pools.contexts.remove(k);
        Ok(())
    })
}
