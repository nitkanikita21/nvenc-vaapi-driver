//! VAAPI picture vtable entries: `vaBeginPicture`, `vaRenderPicture`, `vaEndPicture`.
//!
//! These three functions define the encode frame lifecycle:
//!
//! 1. `vaBeginPicture` — binds a render target surface to the context and
//!    clears the accumulated frame parameter state.
//! 2. `vaRenderPicture` — accepts a list of parameter buffers and classifies
//!    them into `PendingFrame` fields (SPS, PPS, slice params, packed headers,
//!    coded output buffer handle).
//! 3. `vaEndPicture` — the point where a real implementation would parse the
//!    accumulated parameters, invoke `NvEncEncodePicture`, wait for the
//!    bitstream via `NvEncLockBitstream`, and write a `VACodedBufferSegment`
//!    chain into the coded buffer. Currently this is a stub that marks the
//!    coded buffer as "ready but empty".

use super::{guard, state_from};
use crate::driver::state::{BufferStorage, SurfaceKind};
use crate::error::{DriverError, VAStatus};
use crate::h264;
use crate::ids::{ContainsKey, SurfaceKey};
use crate::nvenc::h264_config::{EncoderConfig, H264Profile};
use core::ffi::c_int;
use core::ffi::c_void;
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
            .find_by_low_bits(context as u32)
            .ok_or(DriverError::InvalidContext)?;
        let sk = pools
            .surfaces
            .find_by_low_bits(render_target as u32)
            .ok_or(DriverError::InvalidSurface)?;
        let c = &pools.contexts[ck];
        *c.current_target.lock() = Some(sk);
        c.pending.lock().packed_headers.clear();
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
            .find_by_low_bits(context as u32)
            .ok_or(DriverError::InvalidContext)?;
        let cctx = &pools.contexts[ck];
        let mut pending = cctx.pending.lock();

        // SAFETY: caller-provided array of VABufferID of length num_buffers.
        let ids = unsafe { core::slice::from_raw_parts(buffers, num_buffers as usize) };
        for bid in ids {
            let bk = pools
                .buffers
                .find_by_low_bits(*bid as u32)
                .ok_or(DriverError::InvalidBuffer)?;
            let brec = &pools.buffers[bk];
            let data = brec.storage.bytes().lock().clone();
            match brec.buf_type {
                va::VAEncSequenceParameterBufferType => pending.seq_param = Some(data),
                va::VAEncPictureParameterBufferType => pending.pic_param = Some(data),
                va::VAEncSliceParameterBufferType => pending.slice_param = Some(data),
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
                    pending.packed_headers.push((brec.buf_type, data));
                }
                va::VAEncPackedHeaderDataBufferType => {
                    if let Some(kind) = pending.pending_packed_kind.take() {
                        pending
                            .packed_headers_typed
                            .push(crate::driver::state::PackedHeader {
                                kind,
                                bytes: data.clone(),
                            });
                    }
                    // Legacy form.
                    pending.packed_headers.push((brec.buf_type, data));
                }
                va::VAEncCodedBufferType => pending.coded_buf = Some(bk),
                _ => {
                    // Accept unknown parameter buffers silently – many clients
                    // pass misc buffers (framerate, HRD, etc.) that we do not
                    // need to model until we enable the real NVENC path.
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
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ck = pools
            .contexts
            .find_by_low_bits(context as u32)
            .ok_or(DriverError::InvalidContext)?;
        let cctx = &pools.contexts[ck];

        // Snapshot pending state, release the per-frame accumulator early.
        let mut pending = cctx.pending.lock();
        let pic_bytes = pending.pic_param.take();
        let seq_bytes = pending.seq_param.take();
        let coded_buf_key = pending.coded_buf.take();
        let packed_headers: Vec<crate::driver::state::PackedHeader> =
            core::mem::take(&mut pending.packed_headers_typed);
        pending.slice_param = None;
        pending.packed_headers.clear();
        pending.pending_packed_kind = None;
        drop(pending);

        let target_key: SurfaceKey = cctx
            .current_target
            .lock()
            .ok_or(DriverError::InvalidSurface)?;

        // Parse PPS so we know the force_idr flag and coded_buf ID.
        let pic_params = pic_bytes
            .as_deref()
            .and_then(h264::parse_pps)
            .unwrap_or_default();

        // If the client supplied a fresh SPS, map it to an EncoderConfig and
        // reconfigure the encoder whenever the config actually changes.
        let mut session_guard = cctx.session.lock();
        let session = session_guard
            .as_mut()
            .ok_or(DriverError::OperationFailed("no NVENC session — GPU init failed"))?;

        if let Some(sps_bytes) = seq_bytes.as_deref() {
            if let Some(sps_view) = h264::parse_sps(sps_bytes) {
                let profile = h264_profile_from_config(&pools, cctx.config)
                    .unwrap_or(H264Profile::Main);
                let new_cfg = build_cfg_from_sps(&sps_view, profile, session.config());
                if new_cfg != *session.config() {
                    // Best-effort; failure is logged and we carry on with old config.
                    if session.reconfigure(new_cfg).is_err() {
                        crate::logging::info(
                            ctx,
                            "end_picture: NvEncReconfigureEncoder failed; keeping previous config",
                        );
                    }
                }
            }
        }

        // Lazy-register the render target if this is the first time we see
        // it. ffmpeg's h264_vaapi often creates surfaces on-the-fly and
        // passes them at vaBeginPicture rather than listing them up-front in
        // vaCreateContext's render_targets argument.
        if let Some(srec) = pools.surfaces.get(target_key) {
            match &srec.kind {
                SurfaceKind::Internal { cu_ptr, pitch, width, height } => {
                    if let Err(e) = session.register_internal_surface(
                        target_key, *cu_ptr, *pitch, *width, *height,
                    ) {
                        crate::logging::error(ctx, "end_picture: register_internal_surface failed");
                        return Err(e);
                    }
                }
                SurfaceKind::ExternalDmaBuf { image, registered_nvenc } => {
                    // First encode against this surface: register the
                    // imported CUarray with NVENC and cache the pointer.
                    // Subsequent calls short-circuit inside
                    // register_external_surface itself.
                    if registered_nvenc.get().is_none() {
                        match session.register_external_surface(
                            target_key,
                            image.plane0,
                            image.width,
                            image.height,
                        ) {
                            Ok(ptr) => {
                                let _ = registered_nvenc.set(ptr as usize);
                            }
                            Err(e) => {
                                crate::logging::error(
                                    ctx,
                                    "end_picture: register_external_surface failed",
                                );
                                return Err(e);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Always let NVENC emit its own SPS/PPS. An earlier iteration tried
        // to passthrough the client's VAEncPackedHeader* bytes on frames
        // where the client supplied Sequence/Picture NALs, toggling
        // `repeat_sps_pps` to suppress NVENC's own. That backfired: Chromium
        // WebRTC only reliably sends Slice packed headers (never Sequence),
        // so on every other frame we'd flip between "client SPS" and "NVENC
        // SPS". The two SPS NALs are byte-different (different sps_id,
        // different VUI), so every flip invalidated the receiver's
        // parameter-set tracking — the stream would die after ~4 frames,
        // which is exactly the stall we saw in RTC debug.
        //
        // So: NVENC drives SPS/PPS always. Client packed headers are still
        // consumed (for SPS parsing → encoder reconfigure) but NOT injected
        // into the coded buffer. This keeps every IDR's SPS byte-identical
        // across the stream, matching what ffmpeg h264_vaapi and OBS do.
        let client_drives_headers = false;
        let _ = &packed_headers; // intentionally unused below

        let bitstream = session.encode_frame(target_key, pic_params.force_idr)?;
        drop(session_guard);


        // Resolve the coded buffer. Prefer coded_buf from PPS (if non-zero),
        // otherwise fall back to the coded_buf seen on the vaRenderPicture
        // chain.
        let cb_key = if pic_params.coded_buf != 0 {
            pools
                .buffers
                .find_by_low_bits(pic_params.coded_buf)
                .or(coded_buf_key)
        } else {
            coded_buf_key
        };

        if let Some(cb) = cb_key {
            if let Some(buf) = pools.buffers.get(cb) {
                if let BufferStorage::Coded(slot) = &buf.storage {
                    if bitstream.is_pending {
                        // Pending frame: do not hand anything to the client
                        // this time around. Next successful encode will flush.
                        *buf.coded_ready.lock() = false;
                    } else {
                        let mut backing = slot.backing.lock();
                        backing.clear();
                        // When the client provided its own SPS/PPS via
                        // PackedHeader buffers, prepend them on IDR frames
                        // before the NVENC-produced slice NALs. NVENC is
                        // configured with repeatSPSPPS=0 in this path, so
                        // `bitstream.data` is slice-only.
                        let prepend_len: usize = if client_drives_headers && bitstream.is_idr {
                            packed_headers
                                .iter()
                                .filter(|h| matches!(
                                    h.kind,
                                    crate::driver::state::PackedHeaderKind::Sequence
                                        | crate::driver::state::PackedHeaderKind::Picture
                                ))
                                .map(|h| h.bytes.len())
                                .sum()
                        } else {
                            0
                        };
                        let _ = backing
                            .try_reserve_exact(prepend_len + bitstream.data.len());
                        if client_drives_headers && bitstream.is_idr {
                            for h in &packed_headers {
                                if matches!(
                                    h.kind,
                                    crate::driver::state::PackedHeaderKind::Sequence
                                        | crate::driver::state::PackedHeaderKind::Picture
                                ) {
                                    backing.extend_from_slice(&h.bytes);
                                }
                            }
                        }
                        backing.extend_from_slice(&bitstream.data);
                        // libva headers shipped in this tree do not expose
                        // VA_CODED_BUF_STATUS_PICTURE_TYPE_I — upstream
                        // encoders typically set only size/buf and leave
                        // the status word at 0 for a normal frame.
                        let _ = bitstream.is_idr;
                        let seg = Box::new(va::VACodedBufferSegment {
                            size: u32::try_from(backing.len())
                                .map_err(|_| DriverError::InvalidParameter)?,
                            bit_offset: 0,
                            status: 0,
                            reserved: 0,
                            buf: backing.as_mut_ptr() as *mut c_void,
                            next: core::ptr::null_mut(),
                            va_reserved: [0; 4],
                        });
                        *slot.segment.lock() = Some(seg);
                        *buf.coded_ready.lock() = true;
                    }
                }
            }
        }
        *cctx.last_coded.lock() = cb_key;
        Ok(())
    })
}

// -- helpers --------------------------------------------------------------

fn h264_profile_from_config(
    pools: &crate::driver::state::Pools,
    config_key: crate::ids::ConfigKey,
) -> Option<H264Profile> {
    let cfg = pools.configs.get(config_key)?;
    crate::nvenc::session::h264_profile_from_va(cfg.profile)
}

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
    }
    if sps.fps > 0 {
        out.fps_num = sps.fps;
        out.fps_den = 1;
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
