//! VAAPI picture vtable entries: `vaBeginPicture`, `vaRenderPicture`, `vaEndPicture`.
//!
//! These three functions define the encode frame lifecycle:
//!
//! 1. `vaBeginPicture` — binds a render target surface to the context and
//!    clears the accumulated frame parameter state.
//! 2. `vaRenderPicture` — accepts a list of parameter buffers and classifies
//!    them into `PendingFrame` fields (SPS, PPS, slice params, packed headers,
//!    coded output buffer handle).
//! 3. `vaEndPicture` applies rate control, encodes synchronously through NVENC
//!    and publishes the matching coded buffer.

use super::{guard, state_from};
use crate::driver::state::{BufferStorage, SurfaceKind};
use crate::error::{DriverError, VAStatus};
use crate::h264;
use crate::ids::{ContainsKey, SurfaceKey};
use crate::nvenc::h264_config::{EncoderConfig, H264Profile, HrdBuffer};
use core::ffi::c_int;
use va_sys as va;

pub unsafe extern "C" fn begin_picture(
    ctx: va::VADriverContextP,
    context: va::VAContextID,
    render_target: va::VASurfaceID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ck = pools
            .contexts
            .find_by_low_bits(context)
            .ok_or(DriverError::InvalidContext)?;
        let sk = pools
            .surfaces
            .find_by_low_bits(render_target)
            .ok_or(DriverError::InvalidSurface)?;
        let c = &pools.contexts[ck];
        let mut target = c.current_target.lock();
        if target.is_some() {
            return Err(DriverError::InvalidParameter);
        }
        *c.pending.lock() = Default::default();
        *target = Some(sk);
        Ok(())
    })
}

pub unsafe extern "C" fn render_picture(
    ctx: va::VADriverContextP,
    context: va::VAContextID,
    buffers: *mut va::VABufferID,
    num_buffers: c_int,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if buffers.is_null() || num_buffers <= 0 {
            return Err(DriverError::InvalidParameter);
        }
        let pools = state.pools.lock();
        let ck = pools
            .contexts
            .find_by_low_bits(context)
            .ok_or(DriverError::InvalidContext)?;
        let cctx = &pools.contexts[ck];
        if cctx.current_target.lock().is_none() {
            return Err(DriverError::InvalidParameter);
        }
        let mut pending = cctx.pending.lock();

        // SAFETY: caller-provided array of VABufferID of length num_buffers.
        let ids = unsafe { core::slice::from_raw_parts(buffers, num_buffers as usize) };
        for bid in ids {
            let bk = pools
                .buffers
                .find_by_low_bits(*bid)
                .ok_or(DriverError::InvalidBuffer)?;
            let brec = &pools.buffers[bk];
            // Map retains the generic storage mutex until unmap. Checking
            // first avoids deadlocking the display on a malformed submission.
            if brec.mapped.lock().is_some() {
                return Err(DriverError::InvalidParameter);
            }
            if brec.buf_type == va::VAEncCodedBufferType {
                pending.coded_buf = Some(bk);
                continue;
            }
            let len = (brec.element_size as usize)
                .checked_mul(brec.num_elements as usize)
                .ok_or(DriverError::InvalidParameter)?;
            let storage = brec.storage.bytes().lock();
            let data = storage.get(..len).ok_or(DriverError::InvalidParameter)?;
            match brec.buf_type {
                va::VAEncSequenceParameterBufferType => pending.seq_param = Some(data.to_vec()),
                va::VAEncPictureParameterBufferType => pending.pic_param = Some(data.to_vec()),
                va::VAEncSliceParameterBufferType => pending.slice_param = Some(data.to_vec()),
                va::VAEncPackedHeaderParameterBufferType => {
                    // Pair marker: the very next PackedHeaderDataBuffer
                    // carries the bytes whose NAL kind this param announces.
                    // Layout of `VAEncPackedHeaderParameterBuffer` is
                    // `{ type: u32, bit_length: u32, has_emulation_bytes: u8, ... }`
                    // — we only read the first u32 (type).
                    if data.len() >= core::mem::size_of::<u32>() {
                        let mut raw = [0u8; 4];
                        raw.copy_from_slice(&data[..4]);
                        let va_type = u32::from_ne_bytes(raw);
                        pending.pending_packed_kind =
                            crate::driver::state::PackedHeaderKind::from_va_type(va_type);
                    }
                    // Legacy tuple form, kept until all read sites migrate.
                    pending.packed_headers.push((brec.buf_type, data.to_vec()));
                }
                va::VAEncPackedHeaderDataBufferType => {
                    if let Some(kind) = pending.pending_packed_kind.take() {
                        pending
                            .packed_headers_typed
                            .push(crate::driver::state::PackedHeader {
                                kind,
                                bytes: data.to_vec(),
                            });
                    }
                    // Legacy form.
                    pending.packed_headers.push((brec.buf_type, data.to_vec()));
                }
                va::VAEncCodedBufferType => pending.coded_buf = Some(bk),
                va::VAEncMiscParameterBufferType => {
                    // Borrow transient controls; only retained parameters need a copy.
                    if data.len() < core::mem::size_of::<u32>() {
                        continue;
                    }
                    let mut misc_type_bytes = [0u8; 4];
                    misc_type_bytes.copy_from_slice(&data[..4]);
                    let misc_type = u32::from_ne_bytes(misc_type_bytes);
                    match misc_type {
                        // VAEncMiscParameterTypeFrameRate = 0: next u32
                        // packs framerate as `num | (den << 16)` (libva
                        // convention when the 16 low bits are non-zero;
                        // older libvas treat the whole word as numerator
                        // with den=1).
                        0 => {
                            if data.len() >= 8 {
                                let mut b = [0u8; 4];
                                b.copy_from_slice(&data[4..8]);
                                let raw = u32::from_ne_bytes(b);
                                let num = raw & 0xFFFF;
                                let den = (raw >> 16).max(1);
                                if num > 0 {
                                    pending.misc_fps = Some((num, den));
                                }
                            }
                        }
                        va::VAEncMiscParameterTypeRateControl => {
                            pending.misc_rate_control = Some(
                                h264::parse_rate_control(&data[4..])
                                    .ok_or(DriverError::InvalidParameter)?,
                            );
                        }
                        va::VAEncMiscParameterTypeHRD => {
                            pending.misc_hrd = Some(
                                h264::parse_hrd(&data[4..]).ok_or(DriverError::InvalidParameter)?,
                            );
                        }
                        _ => {
                            // MaxSliceSize, AIR, MaxFrameSize, etc.
                            // Not yet modeled — NVENC will operate on
                            // the current rate-control state.
                        }
                    }
                }
                _ => {
                    // Unknown buffer type — accept silently.
                }
            }
        }
        Ok(())
    })
}

pub unsafe extern "C" fn end_picture(
    ctx: va::VADriverContextP,
    context: va::VAContextID,
) -> VAStatus {
    #[cfg(feature = "profiling")]
    let _span = crate::profiling::Span::new(crate::profiling::Phase::EndPicture);
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ck = pools
            .contexts
            .find_by_low_bits(context)
            .ok_or(DriverError::InvalidContext)?;
        let cctx = &pools.contexts[ck];

        // Snapshot pending state, release the per-frame accumulator early.
        let mut pending = cctx.pending.lock();
        let pic_bytes = pending.pic_param.take();
        let seq_bytes = pending.seq_param.take();
        let coded_buf_key = pending.coded_buf.take();
        pending.packed_headers_typed.clear();
        let misc_rate_control = pending.misc_rate_control.take();
        let misc_hrd = pending.misc_hrd.take();
        let misc_fps = pending.misc_fps.take();
        pending.slice_param = None;
        pending.packed_headers.clear();
        pending.pending_packed_kind = None;
        drop(pending);

        let target_key: SurfaceKey = cctx
            .current_target
            .lock()
            .take()
            .ok_or(DriverError::InvalidSurface)?;

        let pic_params = pic_bytes
            .as_deref()
            .and_then(h264::parse_pps)
            .ok_or(DriverError::InvalidParameter)?;

        // Validate the destination before CUDA/NVENC work. A mapped coded
        // buffer is borrowed by the client and cannot be replaced in place.
        let cb_key = if pic_params.coded_buf != 0 {
            pools.buffers.find_by_low_bits(pic_params.coded_buf)
        } else {
            coded_buf_key
        }
        .ok_or(DriverError::InvalidBuffer)?;
        let buf = pools
            .buffers
            .get(cb_key)
            .ok_or(DriverError::InvalidBuffer)?;
        if buf.mapped.lock().is_some() {
            return Err(DriverError::InvalidParameter);
        }
        let BufferStorage::Coded(slot) = &buf.storage else {
            return Err(DriverError::InvalidBuffer);
        };
        super::completion::buffer(&pools, cb_key, u64::MAX)?;
        *buf.coded_ready.lock() = false;
        let mut backing = slot.backing.lock();
        backing.clear();

        let mut session_guard = cctx.session.lock();
        let profile = crate::nvenc::session::h264_profile_from_va(cctx.config.profile)
            .ok_or(DriverError::UnsupportedProfile)?;
        let mut new_cfg = session_guard
            .as_ref()
            .map(|s| *s.config())
            .unwrap_or_else(|| EncoderConfig::default_for(cctx.width, cctx.height, profile));
        new_cfg.rc_mode = match cctx.config.rc_mode {
            va::VA_RC_CBR => crate::nvenc::preset::RcMode::Cbr,
            va::VA_RC_VBR => crate::nvenc::preset::RcMode::Vbr,
            va::VA_RC_CQP => crate::nvenc::preset::RcMode::ConstQp,
            _ => return Err(DriverError::AttrNotSupported),
        };
        if let Some(bytes) = seq_bytes.as_deref() {
            let sps = h264::parse_sps(bytes).ok_or(DriverError::InvalidParameter)?;
            new_cfg = build_cfg_from_sps(&sps, profile, &new_cfg);
        }
        // Misc parameters override the sequence's initial rate-control values.
        if let Some(rc) = misc_rate_control {
            if rc.bits_per_second > 0 {
                new_cfg.bitrate_bps = rc.bits_per_second;
                new_cfg.max_bitrate_bps = rc.bits_per_second;
            }
            if matches!(new_cfg.rc_mode, crate::nvenc::preset::RcMode::Vbr) {
                if rc.target_percentage == 0 || rc.target_percentage > 100 {
                    return Err(DriverError::InvalidParameter);
                }
                new_cfg.bitrate_bps = ((u64::from(new_cfg.max_bitrate_bps)
                    * u64::from(rc.target_percentage))
                    / 100) as u32;
            }
            new_cfg.initial_qp = rc.initial_qp;
            new_cfg.min_qp = rc.min_qp;
            new_cfg.max_qp = rc.max_qp;
            new_cfg.window_size_ms = rc.window_size_ms;
        }
        if let Some((size_bits, initial_fullness_bits)) = misc_hrd {
            new_cfg.hrd = if size_bits == 0 && initial_fullness_bits == 0 {
                None
            } else {
                Some(HrdBuffer {
                    size_bits,
                    initial_fullness_bits,
                })
            };
        }
        if let Some((num, den)) = misc_fps {
            new_cfg.fps_num = num;
            new_cfg.fps_den = den;
        }
        if matches!(new_cfg.rc_mode, crate::nvenc::preset::RcMode::ConstQp) {
            if pic_params.pic_init_qp > 51 {
                return Err(DriverError::InvalidParameter);
            }
            new_cfg.qp = pic_params.pic_init_qp;
        }
        new_cfg.validate_rate_control()?;
        if let Some(session) = session_guard.as_mut() {
            session.reconfigure(new_cfg, misc_rate_control.is_some_and(|rc| rc.reset))?;
        } else {
            // Wait for the client's actual SPS/RC before opening NVENC. This
            // avoids an expensive reconfigure immediately after initialisation.
            *session_guard = Some(crate::nvenc::NvencSession::new(
                state.cuda.get_or_init()?,
                new_cfg,
            )?);
        }
        let session = session_guard
            .as_mut()
            .ok_or(DriverError::OperationFailed("NVENC init"))?;

        // Lazy-register the render target if this is the first time we see
        // it. ffmpeg's h264_vaapi often creates surfaces on-the-fly and
        // passes them at vaBeginPicture rather than listing them up-front in
        // vaCreateContext's render_targets argument.
        if let Some(srec) = pools.surfaces.get(target_key) {
            match &srec.kind {
                SurfaceKind::Internal {
                    cu_ptr,
                    pitch,
                    width,
                    height,
                } => {
                    if let Err(e) = session
                        .register_internal_surface(target_key, *cu_ptr, *pitch, *width, *height)
                    {
                        crate::logging::error(ctx, "end_picture: register_internal_surface failed");
                        return Err(e);
                    }
                }
                SurfaceKind::ExternalDmaBuf { image, .. } => {
                    // Registrations belong to an encoder session, not a surface:
                    // another context may encode the same imported frame.
                    session.register_external_surface(
                        target_key,
                        image.plane0,
                        image.width,
                        image.height,
                    )?;
                }
                _ => {}
            }
        }

        let bytes = core::mem::take(&mut *backing);
        let pending = session.encode_frame(target_key, pic_params.force_idr, bytes)?;
        *buf.pending_output.lock() = Some(pending);
        *cctx.last_coded.lock() = Some(cb_key);
        Ok(())
    })
}

// -- helpers --------------------------------------------------------------

/// Merge an SPS view into the session's current NVENC config, keeping
/// existing preset/tuning choices when the SPS does not override a value.
fn build_cfg_from_sps(
    sps: &h264::EncoderConfig,
    profile: H264Profile,
    current: &EncoderConfig,
) -> EncoderConfig {
    let mut out = *current;
    out.profile = profile;
    if sps.bits_per_second > 0 {
        out.bitrate_bps = sps.bits_per_second;
        out.max_bitrate_bps = sps.bits_per_second;
    }
    if sps.fps_num > 0 && sps.fps_den > 0 {
        out.fps_num = sps.fps_num;
        out.fps_den = sps.fps_den;
    }
    if sps.idr_period > 0 {
        out.gop_length = sps.idr_period;
    }
    if sps.width > 0 && sps.height > 0 {
        out.width = sps.width;
        out.height = sps.height;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::state::{DriverState, SurfaceRec};
    use crate::driver::{buffer, config, context};
    use crate::ids::key_to_id;
    use parking_lot::Mutex;
    use std::ptr;

    struct Fixture {
        ctx: Box<va::VADriverContext>,
        state: Box<DriverState>,
        context: va::VAContextID,
        surface: va::VASurfaceID,
        config: va::VAConfigID,
    }

    impl Fixture {
        fn new() -> Self {
            // SAFETY: zero is valid for the POD backend context; CUDA is lazy.
            let mut ctx: Box<va::VADriverContext> = Box::new(unsafe { core::mem::zeroed() });
            let mut state = Box::new(DriverState::new(&mut *ctx).unwrap());
            ctx.pDriverData = (&mut *state as *mut DriverState).cast();
            let key = state
                .pools
                .lock()
                .surfaces
                .insert(SurfaceRec {
                    width: 64,
                    height: 64,
                    format: va::VA_RT_FORMAT_YUV420,
                    kind: SurfaceKind::Stub,
                    registered: Mutex::new(None),
                    cuda_ctx: None,
                })
                .unwrap();
            let mut config_id = va::VA_INVALID_ID;
            let mut context_id = va::VA_INVALID_ID;
            // SAFETY: fixture owns all input and output objects.
            unsafe {
                assert_eq!(
                    config::create_config(
                        &mut *ctx,
                        va::VAProfileH264Main,
                        va::VAEntrypointEncSlice,
                        ptr::null_mut(),
                        0,
                        &mut config_id
                    ),
                    0
                );
                assert_eq!(
                    context::create_context(
                        &mut *ctx,
                        config_id,
                        64,
                        64,
                        0,
                        ptr::null_mut(),
                        0,
                        &mut context_id
                    ),
                    0
                );
            }
            Self {
                ctx,
                state,
                context: context_id,
                surface: key_to_id(key),
                config: config_id,
            }
        }

        fn begin(&mut self) -> VAStatus {
            // SAFETY: live fixture objects.
            unsafe { begin_picture(&mut *self.ctx, self.context, self.surface) }
        }

        fn buffer(&mut self, ty: va::VABufferType, data: &[u8]) -> va::VABufferID {
            let mut id = va::VA_INVALID_ID;
            // SAFETY: data remains readable during creation.
            unsafe {
                assert_eq!(
                    buffer::create_buffer(
                        &mut *self.ctx,
                        self.context,
                        ty,
                        data.len() as u32,
                        1,
                        data.as_ptr().cast_mut().cast(),
                        &mut id
                    ),
                    0
                );
            }
            id
        }

        fn render(&mut self, ids: &mut [va::VABufferID]) -> VAStatus {
            // SAFETY: live array of identifiers.
            unsafe {
                render_picture(
                    &mut *self.ctx,
                    self.context,
                    ids.as_mut_ptr(),
                    ids.len() as c_int,
                )
            }
        }

        fn pps(&mut self, output: va::VABufferID) -> va::VABufferID {
            // SAFETY: bindgen H.264 parameters are POD, including their padding.
            let mut pps: va::VAEncPictureParameterBufferH264 = unsafe { core::mem::zeroed() };
            pps.coded_buf = output;
            // SAFETY: view spans one live initialised parameter struct.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pps as *const va::VAEncPictureParameterBufferH264).cast::<u8>(),
                    core::mem::size_of_val(&pps),
                )
            };
            self.buffer(va::VAEncPictureParameterBufferType, bytes)
        }

        fn misc<T>(&mut self, kind: u32, params: &T) -> va::VABufferID {
            let mut bytes = kind.to_ne_bytes().to_vec();
            // SAFETY: callers use fully initialised VA integer-POD controls.
            bytes.extend_from_slice(unsafe {
                core::slice::from_raw_parts((params as *const T).cast(), core::mem::size_of::<T>())
            });
            self.buffer(va::VAEncMiscParameterBufferType, &bytes)
        }
    }

    #[test]
    fn frame_lifecycle_rejects_missing_or_nested_begin_and_consumes_failed_end() {
        let mut f = Fixture::new();
        let param = f.buffer(va::VAEncMiscParameterBufferType, &[0; 8]);
        assert_ne!(f.render(&mut [param]), 0);
        assert_eq!(f.begin(), 0);
        assert_ne!(f.begin(), 0);
        assert_eq!(f.render(&mut [param]), 0);
        // No PPS: failure closes this picture, allowing a clean retry.
        assert_ne!(unsafe { end_picture(&mut *f.ctx, f.context) }, 0);
        assert_ne!(f.render(&mut [param]), 0);
        assert_eq!(f.begin(), 0);
        let pools = f.state.pools.lock();
        let key = pools.contexts.find_by_low_bits(f.context).unwrap();
        assert!(pools.contexts[key].pending.lock().misc_fps.is_none());
    }

    #[test]
    fn render_rejects_mapped_parameters_before_locking_their_storage() {
        let mut f = Fixture::new();
        let id = f.buffer(va::VAEncMiscParameterBufferType, &[0; 8]);
        assert_eq!(f.begin(), 0);
        let mut mapped = ptr::null_mut();
        assert_eq!(
            unsafe { buffer::map_buffer(&mut *f.ctx, id, &mut mapped) },
            0
        );
        assert_ne!(f.render(&mut [id]), 0);
        assert_eq!(unsafe { buffer::unmap_buffer(&mut *f.ctx, id) }, 0);
        assert_eq!(f.render(&mut [id]), 0);
    }

    #[test]
    fn complete_rate_controls_are_retained_and_truncated_payloads_rejected() {
        let mut f = Fixture::new();
        // SAFETY: these VA control structs contain only integer POD.
        let mut rc: va::VAEncMiscParameterRateControl = unsafe { core::mem::zeroed() };
        let mut hrd: va::VAEncMiscParameterHRD = unsafe { core::mem::zeroed() };
        rc.bits_per_second = 3_000_000;
        rc.initial_qp = 26;
        rc.min_qp = 10;
        rc.max_qp = 42;
        rc.window_size = 1500;
        hrd.buffer_size = 4_500_000;
        hrd.initial_buffer_fullness = 2_250_000;
        let rc_id = f.misc(va::VAEncMiscParameterTypeRateControl, &rc);
        let hrd_id = f.misc(va::VAEncMiscParameterTypeHRD, &hrd);
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [rc_id, hrd_id]), 0);
        {
            let pools = f.state.pools.lock();
            let key = pools.contexts.find_by_low_bits(f.context).unwrap();
            let pending = pools.contexts[key].pending.lock();
            let rc = pending.misc_rate_control.unwrap();
            assert_eq!(
                (rc.initial_qp, rc.min_qp, rc.max_qp, rc.window_size_ms),
                (26, 10, 42, 1500)
            );
            assert_eq!(pending.misc_hrd, Some((4_500_000, 2_250_000)));
        }
        let short_rc = f.buffer(va::VAEncMiscParameterBufferType, &[1, 0, 0, 0, 0, 0, 0, 0]);
        let short_hrd = f.buffer(va::VAEncMiscParameterBufferType, &[5, 0, 0, 0]);
        assert_eq!(
            f.render(&mut [short_rc]),
            va::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus
        );
        assert_eq!(
            f.render(&mut [short_hrd]),
            va::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus
        );
    }

    #[test]
    fn invalid_qp_limits_fail_before_creating_a_cuda_session() {
        let mut f = Fixture::new();
        let coded = f.buffer(va::VAEncCodedBufferType, &[0; 64]);
        let pps = f.pps(coded);
        // SAFETY: VA rate control is integer POD.
        let mut rc: va::VAEncMiscParameterRateControl = unsafe { core::mem::zeroed() };
        rc.min_qp = 52;
        let rc_id = f.misc(va::VAEncMiscParameterTypeRateControl, &rc);
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [pps, rc_id]), 0);
        assert_eq!(
            unsafe { end_picture(&mut *f.ctx, f.context) },
            va::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus
        );
        let pools = f.state.pools.lock();
        let key = pools.contexts.find_by_low_bits(f.context).unwrap();
        assert!(pools.contexts[key].session.lock().is_none());
    }

    #[test]
    fn invalid_output_is_rejected_before_cuda_and_cannot_fall_back_to_another_buffer() {
        let mut f = Fixture::new();
        let coded = f.buffer(va::VAEncCodedBufferType, &[0; 64]);
        let pps = f.pps(va::VA_INVALID_ID);
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [pps, coded]), 0);
        assert_eq!(
            unsafe { end_picture(&mut *f.ctx, f.context) },
            va::VA_STATUS_ERROR_INVALID_BUFFER as VAStatus
        );
        let pools = f.state.pools.lock();
        let key = pools.contexts.find_by_low_bits(f.context).unwrap();
        assert!(pools.contexts[key].session.lock().is_none());
    }

    #[test]
    fn end_cannot_replace_a_coded_segment_borrowed_by_the_client() {
        let mut f = Fixture::new();
        let coded = f.buffer(va::VAEncCodedBufferType, &[0; 64]);
        let pps = f.pps(coded);
        {
            let pools = f.state.pools.lock();
            let key = pools.buffers.find_by_low_bits(coded).unwrap();
            let BufferStorage::Coded(slot) = &pools.buffers[key].storage else {
                panic!()
            };
            // SAFETY: POD descriptor; a sentinel tests its stable address/value.
            let mut segment: Box<va::VACodedBufferSegment> =
                Box::new(unsafe { core::mem::zeroed() });
            segment.size = 17;
            *slot.segment.lock() = Some(segment);
            *pools.buffers[key].coded_ready.lock() = true;
        }
        let mut mapped = ptr::null_mut();
        assert_eq!(
            unsafe { buffer::map_buffer(&mut *f.ctx, coded, &mut mapped) },
            0
        );
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [pps]), 0);
        assert_eq!(
            unsafe { end_picture(&mut *f.ctx, f.context) },
            va::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus
        );
        assert_eq!(
            unsafe { (*mapped.cast::<va::VACodedBufferSegment>()).size },
            17
        );
        assert_eq!(unsafe { buffer::unmap_buffer(&mut *f.ctx, coded) }, 0);
    }

    #[test]
    fn context_owns_configuration_after_public_config_is_destroyed() {
        let mut f = Fixture::new();
        assert_eq!(unsafe { config::destroy_config(&mut *f.ctx, f.config) }, 0);
        let pools = f.state.pools.lock();
        let key = pools.contexts.find_by_low_bits(f.context).unwrap();
        assert_eq!(pools.contexts[key].config.profile, va::VAProfileH264Main);
        assert_eq!(pools.contexts[key].config.rc_mode, va::VA_RC_CBR);
    }

    #[test]
    fn failed_reuse_never_maps_the_previous_coded_picture() {
        let mut f = Fixture::new();
        let coded = f.buffer(va::VAEncCodedBufferType, &[0; 64]);
        let pps = f.pps(coded);
        let malformed_sps = f.buffer(va::VAEncSequenceParameterBufferType, &[0; 4]);
        {
            let pools = f.state.pools.lock();
            let key = pools.buffers.find_by_low_bits(coded).unwrap();
            let BufferStorage::Coded(slot) = &pools.buffers[key].storage else {
                panic!()
            };
            // SAFETY: descriptor fields have valid zero representations.
            *slot.segment.lock() = Some(Box::new(unsafe { core::mem::zeroed() }));
            *pools.buffers[key].coded_ready.lock() = true;
        }
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [pps, malformed_sps]), 0);
        assert_ne!(unsafe { end_picture(&mut *f.ctx, f.context) }, 0);
        let mut mapped = ptr::null_mut();
        assert_ne!(
            unsafe { buffer::map_buffer(&mut *f.ctx, coded, &mut mapped) },
            0
        );
        assert!(mapped.is_null());
    }

    #[test]
    fn zero_timeout_sync_never_waits_for_the_pool_mutex() {
        let mut f = Fixture::new();
        let pools = f.state.pools.lock();
        assert_eq!(
            unsafe { crate::driver::sync::sync_surface2(&mut *f.ctx, f.surface, 0) },
            va::VA_STATUS_ERROR_TIMEDOUT as i32
        );
        drop(pools);
        assert_eq!(
            unsafe { crate::driver::sync::sync_surface2(&mut *f.ctx, f.surface, 0) },
            0
        );
        assert_eq!(
            unsafe { crate::driver::sync::sync_surface2(&mut *f.ctx, va::VA_INVALID_SURFACE, 0) },
            va::VA_STATUS_ERROR_INVALID_SURFACE as i32
        );
    }

    #[test]
    fn image_storage_reuse_is_bounded_and_clears_previous_pixels() {
        let mut f = Fixture::new();
        let mut format: va::VAImageFormat = unsafe { core::mem::zeroed() };
        format.fourcc = va::VA_FOURCC_NV12;
        let mut image: va::VAImage = unsafe { core::mem::zeroed() };
        let mut ptr = ptr::null_mut();
        // SAFETY: image structs and mapped storage remain live until unmap.
        unsafe {
            assert_eq!(
                crate::driver::image::create_image(
                    &mut *f.ctx,
                    &mut format,
                    1024,
                    1024,
                    &mut image
                ),
                0
            );
            assert_eq!(buffer::map_buffer(&mut *f.ctx, image.buf, &mut ptr), 0);
            let previous_address = ptr as usize;
            core::slice::from_raw_parts_mut(ptr.cast::<u8>(), image.data_size as usize).fill(0xa5);
            assert_eq!(buffer::unmap_buffer(&mut *f.ctx, image.buf), 0);
            assert_eq!(
                crate::driver::image::destroy_image(&mut *f.ctx, image.image_id),
                0
            );
            assert_eq!(
                crate::driver::image::create_image(&mut *f.ctx, &mut format, 64, 64, &mut image),
                0
            );
            assert_eq!(buffer::map_buffer(&mut *f.ctx, image.buf, &mut ptr), 0);
            assert_eq!(ptr as usize, previous_address);
            assert!(
                core::slice::from_raw_parts(ptr.cast::<u8>(), image.data_size as usize)
                    .iter()
                    .all(|&b| b == 0)
            );
            assert_eq!(buffer::unmap_buffer(&mut *f.ctx, image.buf), 0);
            // Clients may destroy an image buffer directly before its image.
            assert_eq!(buffer::destroy_buffer(&mut *f.ctx, image.buf), 0);
            assert_eq!(
                crate::driver::image::destroy_image(&mut *f.ctx, image.image_id),
                0
            );
            // An oversized image must not grow the retained cache.
            assert_eq!(
                crate::driver::image::create_image(
                    &mut *f.ctx,
                    &mut format,
                    4096,
                    4096,
                    &mut image
                ),
                0
            );
            assert_eq!(
                crate::driver::image::destroy_image(&mut *f.ctx, image.image_id),
                0
            );
        }
        assert!(f.state.pools.lock().image_bytes_cache.capacity() <= 16 * 1024 * 1024);
    }

    #[test]
    fn derive_rejects_surfaces_without_cpu_access() {
        let mut f = Fixture::new();
        let mut image: va::VAImage = unsafe { core::mem::zeroed() };
        assert_eq!(
            unsafe { crate::driver::image::derive_image(&mut *f.ctx, f.surface, &mut image) },
            va::VA_STATUS_ERROR_OPERATION_FAILED as i32
        );
        assert_eq!(f.state.pools.lock().images.iter().count(), 0);
    }

    #[test]
    fn map_access_flags_reject_unknown_bits_without_poisoning_mapping() {
        let mut f = Fixture::new();
        let id = f.buffer(va::VAImageBufferType, &[7; 64]);
        let mut mapped = ptr::null_mut();
        for flags in [4, u32::MAX] {
            assert_eq!(
                unsafe { buffer::map_buffer2(&mut *f.ctx, id, &mut mapped, flags) },
                va::VA_STATUS_ERROR_INVALID_PARAMETER as i32
            );
            assert!(mapped.is_null());
        }
        for flags in [
            va::VA_MAPBUFFER_FLAG_DEFAULT,
            va::VA_MAPBUFFER_FLAG_READ,
            va::VA_MAPBUFFER_FLAG_WRITE,
            va::VA_MAPBUFFER_FLAG_READ | va::VA_MAPBUFFER_FLAG_WRITE,
        ] {
            assert_eq!(
                unsafe { buffer::map_buffer2(&mut *f.ctx, id, &mut mapped, flags) },
                0
            );
            assert_eq!(unsafe { *mapped.cast::<u8>() }, 7);
            assert_eq!(
                unsafe { buffer::destroy_buffer(&mut *f.ctx, id) },
                va::VA_STATUS_ERROR_INVALID_PARAMETER as i32
            );
            assert_eq!(unsafe { buffer::unmap_buffer(&mut *f.ctx, id) }, 0);
        }
        assert_eq!(unsafe { buffer::destroy_buffer(&mut *f.ctx, id) }, 0);
    }

    #[test]
    fn derived_image_unmap_releases_mapping_guard_before_upload_validation() {
        let mut f = Fixture::new();
        let id = f.buffer(va::VAImageBufferType, &[0; 64]);
        {
            let mut pools = f.state.pools.lock();
            let buffer_key = pools.buffers.find_by_low_bits(id).unwrap();
            let surface_key = pools.surfaces.find_by_low_bits(f.surface).unwrap();
            pools
                .images
                .insert(crate::driver::state::ImageRec {
                    surface_key: Some(surface_key),
                    buffer_key,
                    fourcc: va::VA_FOURCC_NV12,
                    width: 64,
                    height: 64,
                    pitches: [64, 64, 0],
                    offsets: [0, 4096, 0],
                    data_size: 6144,
                })
                .unwrap();
        }
        let mut mapped = ptr::null_mut();
        assert_eq!(
            unsafe {
                buffer::map_buffer2(&mut *f.ctx, id, &mut mapped, va::VA_MAPBUFFER_FLAG_WRITE)
            },
            0
        );
        assert_eq!(
            unsafe { buffer::unmap_buffer(&mut *f.ctx, id) },
            va::VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as i32
        );
        assert_eq!(unsafe { buffer::destroy_buffer(&mut *f.ctx, id) }, 0);
    }

    #[test]
    fn render_respects_active_element_count_instead_of_copying_allocation_tail() {
        let mut f = Fixture::new();
        let id = f.buffer(va::VAEncSequenceParameterBufferType, &[9; 64]);
        assert_eq!(
            unsafe { buffer::buffer_set_num_elements(&mut *f.ctx, id, 0) },
            0
        );
        assert_eq!(f.begin(), 0);
        assert_eq!(f.render(&mut [id]), 0);
        let pools = f.state.pools.lock();
        let key = pools.contexts.find_by_low_bits(f.context).unwrap();
        assert_eq!(
            pools.contexts[key].pending.lock().seq_param.as_deref(),
            Some(&[][..])
        );
    }
}
