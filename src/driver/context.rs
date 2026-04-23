//! VAAPI context vtable entries: `vaCreateContext`, `vaDestroyContext`.
//!
//! A `VAContext` ties together a config, a picture size, and a set of render
//! targets. One `NvencSession` is lazily created per context on the first
//! `vaEndPicture` call (not yet implemented; the session field is `None` until
//! the NVENC backend is wired in).

use super::{guard, state_from};
use crate::driver::state::{ContextRec, PendingFrame, SurfaceKind};
use crate::error::{DriverError, VAStatus};
use crate::ids::{ContainsKey, ContextKey, SurfaceKey, key_to_id};
use crate::nvenc::NvencSession;
use crate::nvenc::h264_config::{EncoderConfig, H264Profile};
use crate::nvenc::session::h264_profile_from_va;
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
        if context_out.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        let mut pools = state.pools.lock();
        let cfg_key = pools
            .configs
            .find_by_low_bits(config_id as u32)
            .ok_or(DriverError::InvalidConfig)?;

        let mut targets: Vec<SurfaceKey> = Vec::new();
        if !render_targets.is_null() && num_render_targets > 0 {
            // SAFETY: caller-provided array.
            let ids = unsafe {
                core::slice::from_raw_parts(render_targets, num_render_targets as usize)
            };
            targets.reserve(ids.len());
            for id in ids {
                let k = pools
                    .surfaces
                    .find_by_low_bits(*id as u32)
                    .ok_or(DriverError::InvalidSurface)?;
                targets.push(k);
            }
        }

        // Resolve the VA profile from the config so we can build an
        // initial EncoderConfig for NVENC. Unsupported profiles are gated
        // at vaCreateConfig, so the lookup below should always succeed.
        let profile = pools.configs[cfg_key].profile;
        let h264_profile = h264_profile_from_va(profile).unwrap_or(H264Profile::Main);
        let width = u32::try_from(picture_width).map_err(|_| DriverError::InvalidParameter)?;
        let height = u32::try_from(picture_height).map_err(|_| DriverError::InvalidParameter)?;
        let draft_cfg = EncoderConfig::default_for(width, height, h264_profile);

        // Eagerly bring up a NVENC session. On hosts without a GPU we log
        // and leave session=None — vaEndPicture will then surface a clean
        // OPERATION_FAILED to the client.
        let session_opt = match state.cuda.get_or_init() {
            Ok(cuda) => match NvencSession::new(cuda, draft_cfg) {
                Ok(mut s) => {
                    // Register every Internal render target with the new session.
                    for k in &targets {
                        let Some(rec) = pools.surfaces.get(*k) else { continue };
                        let SurfaceKind::Internal { cu_ptr, pitch, width, height } = rec.kind
                        else { continue };
                        if let Err(e) = s.register_internal_surface(
                            *k, cu_ptr, pitch, width, height,
                        ) {
                            crate::logging::error(
                                ctx,
                                "create_context: register_internal_surface failed",
                            );
                            return Err(e);
                        }
                    }
                    Some(s)
                }
                Err(_) => {
                    crate::logging::info(
                        ctx,
                        "create_context: NVENC session init failed; encode will return OPERATION_FAILED",
                    );
                    None
                }
            },
            Err(_) => {
                crate::logging::info(
                    ctx,
                    "create_context: CUDA unavailable; encode will return OPERATION_FAILED",
                );
                None
            }
        };

        let rec = ContextRec {
            config: cfg_key,
            width,
            height,
            flag,
            render_targets: targets,
            current_target: Mutex::new(None),
            session: Mutex::new(session_opt),
            pending: Mutex::new(PendingFrame::default()),
            last_coded: Mutex::new(None),
        };
        let k: ContextKey = pools.contexts.insert(rec);
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
            .find_by_low_bits(context_id as u32)
            .ok_or(DriverError::InvalidContext)?;
        pools.contexts.remove(k);
        Ok(())
    })
}
