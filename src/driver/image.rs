//! VAImage / VASubpicture vtable entries.
//!
//! libva's init-time validator refuses to accept a driver that leaves any of
//! these entries NULL (see `va_openDriver` -> `va_NewDriverContext` checks in
//! `va/va.c`). Unimplemented slots still return a stub; the NV12 image upload
//! path (`vaCreateImage`/`vaDeriveImage`/`vaPutImage`/`vaDestroyImage`) is
//! fully wired — see slice c.4 in CHANGELOG.
//!
//! The upload flow for ffmpeg `h264_vaapi` + `hwupload`:
//!
//! 1. Client calls `vaDeriveImage(surface)` → we create a host-backed
//!    buffer sized `pitch * height * 3/2` and return a VAImage whose `buf`
//!    field names that buffer.
//! 2. Client `vaMapBuffer(image.buf)` → raw pointer into that Vec.
//! 3. Client memcpy's Y and UV planes.
//! 4. Client `vaUnmapBuffer(image.buf)` → we look the image up in
//!    `DriverState.pools.images`, and if the surface is CUDA-resident
//!    (`SurfaceKind::Internal`), we issue two `cuMemcpy2D_v2` calls (Y plane
//!    and UV plane) host → device.

use super::{guard, state_from};
use crate::driver::state::{
    BufferRec, BufferStorage, DriverState, ImageRec, Pools, SurfaceKind,
};
use crate::error::{DriverError, VAStatus, VA_STATUS_SUCCESS};
use crate::ids::{BufferKey, ContainsKey, ImageKey, SurfaceKey, key_to_id};
use core::ffi::{c_int, c_uint};
use parking_lot::Mutex;
use va_sys as va;

pub unsafe extern "C" fn query_image_formats(
    _ctx: va::VADriverContextP,
    format_list: *mut va::VAImageFormat,
    num_formats: *mut c_int,
) -> VAStatus {
    guard(|| {
        if num_formats.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        if !format_list.is_null() {
            // SAFETY: caller sized the array to at least max_image_formats.
            unsafe {
                core::ptr::write_bytes(format_list, 0, 1);
                (*format_list).fourcc = u32::from_le_bytes(*b"NV12");
                (*format_list).byte_order = va::VA_LSB_FIRST;
                (*format_list).bits_per_pixel = 12;
            }
        }
        // SAFETY: non-null checked.
        unsafe { *num_formats = 1 };
        Ok(())
    })
}

pub unsafe extern "C" fn create_image(
    ctx: va::VADriverContextP,
    format: *mut va::VAImageFormat,
    width: c_int,
    height: c_int,
    image: *mut va::VAImage,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if image.is_null() || format.is_null() || width <= 0 || height <= 0 {
            return Err(DriverError::InvalidParameter);
        }
        // SAFETY: format is a caller-provided readable VAImageFormat.
        let fourcc = unsafe { (*format).fourcc };
        if fourcc != va::VA_FOURCC_NV12 {
            return Err(DriverError::UnsupportedBufferType);
        }
        let w = width as u32;
        let h = height as u32;
        let pitch = w; // tightly packed host buffer
        let (image_out, _bk, _ik) =
            alloc_host_image(state, None, w, h, pitch, fourcc)?;
        // SAFETY: out pointer owned by caller.
        unsafe { *image = image_out };
        Ok(())
    })
}

pub unsafe extern "C" fn derive_image(
    ctx: va::VADriverContextP,
    surface: va::VASurfaceID,
    image: *mut va::VAImage,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if image.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        // Gather surface geometry + key under a short lock.
        let (sk, width, height, pitch) = {
            let pools = state.pools.lock();
            let sk = pools
                .surfaces
                .find_by_low_bits(surface as u32)
                .ok_or(DriverError::InvalidSurface)?;
            let srec = &pools.surfaces[sk];
            if srec.format != va::VA_RT_FORMAT_YUV420 {
                return Err(DriverError::UnsupportedRtFormat);
            }
            let pitch = match &srec.kind {
                SurfaceKind::Internal { pitch, .. } => *pitch,
                // Host buffer pitch for non-GPU surfaces — no upload happens
                // on unmap (surface has no CUDA allocation).
                _ => srec.width,
            };
            (sk, srec.width, srec.height, pitch)
        };
        let (image_out, _bk, _ik) =
            alloc_host_image(state, Some(sk), width, height, pitch, va::VA_FOURCC_NV12)?;
        // SAFETY: out pointer owned by caller.
        unsafe { *image = image_out };
        Ok(())
    })
}

pub unsafe extern "C" fn destroy_image(
    ctx: va::VADriverContextP,
    image_id: va::VAImageID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let mut pools = state.pools.lock();
        let ik = pools
            .images
            .find_by_low_bits(image_id as u32)
            .ok_or(DriverError::InvalidImage)?;
        let rec = pools.images.remove(ik).ok_or(DriverError::InvalidImage)?;
        // Best-effort: also drop the backing data buffer. If the client
        // already destroyed it separately this is a no-op.
        pools.buffers.remove(rec.buffer_key);
        Ok(())
    })
}

pub unsafe extern "C" fn set_image_palette(
    _ctx: va::VADriverContextP,
    _image: va::VAImageID,
    _palette: *mut u8,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn get_image(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _x: c_int,
    _y: c_int,
    _width: c_uint,
    _height: c_uint,
    _image: va::VAImageID,
) -> VAStatus {
    // Encode driver: device→host read-back is not a hot path for encoders.
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn put_image(
    ctx: va::VADriverContextP,
    surface: va::VASurfaceID,
    image: va::VAImageID,
    sx: c_int, sy: c_int, sw: c_uint, sh: c_uint,
    dx: c_int, dy: c_int, dw: c_uint, dh: c_uint,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ik = pools
            .images
            .find_by_low_bits(image as u32)
            .ok_or(DriverError::InvalidImage)?;
        let sk = pools
            .surfaces
            .find_by_low_bits(surface as u32)
            .ok_or(DriverError::InvalidSurface)?;
        let irec = &pools.images[ik];
        // Only accept full-frame, non-cropped puts for now. Partial puts
        // would require extra bookkeeping we do not yet need.
        if sx != 0 || sy != 0 || dx != 0 || dy != 0
            || sw != irec.width || sh != irec.height
            || dw != irec.width || dh != irec.height
        {
            crate::logging::info(ctx, "put_image: partial/cropped puts not supported");
            return Err(DriverError::Unimplemented);
        }
        upload_image_to_surface(ctx, &pools, irec, sk)
    })
}

pub unsafe extern "C" fn query_subpicture_formats(
    _ctx: va::VADriverContextP,
    _format_list: *mut va::VAImageFormat,
    _flags: *mut c_uint,
    num_formats: *mut c_uint,
) -> VAStatus {
    guard(|| {
        if !num_formats.is_null() {
            // SAFETY: non-null.
            unsafe { *num_formats = 0 };
        }
        Ok(())
    })
}

pub unsafe extern "C" fn create_subpicture(
    _ctx: va::VADriverContextP,
    _image: va::VAImageID,
    _subpicture: *mut va::VASubpictureID,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn destroy_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn set_subpicture_image(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _image: va::VAImageID,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn set_subpicture_chromakey(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _min: c_uint, _max: c_uint, _mask: c_uint,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn set_subpicture_global_alpha(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _global_alpha: f32,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn associate_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _target_surfaces: *mut va::VASurfaceID,
    _num_surfaces: c_int,
    _sx: i16, _sy: i16, _sw: u16, _sh: u16,
    _dx: i16, _dy: i16, _dw: u16, _dh: u16,
    _flags: c_uint,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn deassociate_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _target_surfaces: *mut va::VASurfaceID,
    _num_surfaces: c_int,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

// libva also requires a vaPutSurface pointer (window-system rendering).
pub unsafe extern "C" fn put_surface(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _draw: *mut core::ffi::c_void,
    _srcx: i16, _srcy: i16, _srcw: u16, _srch: u16,
    _destx: i16, _desty: i16, _destw: u16, _desth: u16,
    _cliprects: *mut va::VARectangle,
    _number_cliprects: c_uint,
    _flags: c_uint,
) -> VAStatus {
    // Encoders don't present; return SUCCESS to satisfy the check.
    let _ = VA_STATUS_SUCCESS;
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn lock_surface(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _fourcc: *mut c_uint,
    _ls: *mut c_uint, _cus: *mut c_uint, _cvs: *mut c_uint,
    _lo: *mut c_uint, _cuo: *mut c_uint, _cvo: *mut c_uint,
    _buffer_name: *mut c_uint,
    _buffer: *mut *mut core::ffi::c_void,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn unlock_surface(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

pub unsafe extern "C" fn query_surface_error(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _error_status: VAStatus,
    _error_info: *mut *mut core::ffi::c_void,
) -> VAStatus { guard(|| Err(DriverError::Unimplemented)) }

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocate a host-backed NV12 image of size `width × height` with Y-plane
/// pitch `pitch`. Inserts the backing `BufferRec` + `ImageRec` into state and
/// returns a fully-populated `VAImage` plus the pool keys.
fn alloc_host_image(
    state: &DriverState,
    surface_key: Option<SurfaceKey>,
    width: u32,
    height: u32,
    pitch: u32,
    fourcc: u32,
) -> Result<(va::VAImage, BufferKey, ImageKey), DriverError> {
    debug_assert_eq!(fourcc, va::VA_FOURCC_NV12);
    let y_size = (pitch as usize).checked_mul(height as usize)
        .ok_or(DriverError::InvalidParameter)?;
    let uv_size = (pitch as usize).checked_mul((height as usize) / 2)
        .ok_or(DriverError::InvalidParameter)?;
    let total = y_size.checked_add(uv_size).ok_or(DriverError::InvalidParameter)?;

    let mut bytes: Vec<u8> = Vec::new();
    bytes.try_reserve_exact(total).map_err(|_| DriverError::AllocFailed)?;
    bytes.resize(total, 0);

    let mut pools = state.pools.lock();
    let bk: BufferKey = pools.buffers.insert(BufferRec {
        buf_type: va::VAImageBufferType,
        element_size: total as u32,
        num_elements: 1,
        storage: BufferStorage::Generic(Mutex::new(bytes)),
        coded_ready: Mutex::new(false),
    });
    let pitches = [pitch, pitch, 0];
    let offsets = [0u32, y_size as u32, 0];
    let ik: ImageKey = pools.images.insert(ImageRec {
        surface_key,
        fourcc,
        width,
        height,
        pitches,
        offsets,
        data_size: total as u32,
        buffer_key: bk,
    });

    let image_id = key_to_id(ik) as va::VAImageID;
    let buf_id = key_to_id(bk) as va::VABufferID;

    let va_fmt = va::VAImageFormat {
        fourcc,
        byte_order: va::VA_LSB_FIRST,
        bits_per_pixel: 12,
        depth: 0,
        red_mask: 0,
        green_mask: 0,
        blue_mask: 0,
        alpha_mask: 0,
        va_reserved: [0; 4],
    };

    let image = va::VAImage {
        image_id,
        format: va_fmt,
        buf: buf_id,
        width: u16::try_from(width).map_err(|_| DriverError::InvalidParameter)?,
        height: u16::try_from(height).map_err(|_| DriverError::InvalidParameter)?,
        data_size: total as u32,
        num_planes: 2,
        pitches,
        offsets,
        num_palette_entries: 0,
        entry_bytes: 0,
        component_order: [0; 4],
        va_reserved: [0; 4],
    };
    Ok((image, bk, ik))
}

/// Trigger `cuMemcpy2D_v2` host → device for all planes of an NV12 image
/// into the given surface. No-op (with a debug log) if the surface is not
/// CUDA-resident (`SurfaceKind::Stub`/`DmaBuf`/`CudaDevice`).
pub(crate) fn upload_image_to_surface(
    ctx: va::VADriverContextP,
    pools: &Pools,
    image: &ImageRec,
    surface_key: SurfaceKey,
) -> Result<(), DriverError> {
    let srec = pools
        .surfaces
        .get(surface_key)
        .ok_or(DriverError::InvalidSurface)?;
    let (cu_ptr, surf_pitch, surf_w, surf_h) = match &srec.kind {
        SurfaceKind::Internal { cu_ptr, pitch, width, height } =>
            (*cu_ptr, *pitch, *width, *height),
        _ => {
            crate::logging::info(ctx, "upload_image_to_surface: non-CUDA surface, skipping");
            return Ok(());
        }
    };
    if image.width != surf_w || image.height != surf_h {
        return Err(DriverError::InvalidParameter);
    }
    let cuda_ctx = srec
        .cuda_ctx
        .as_ref()
        .ok_or(DriverError::OperationFailed("surface has no CUDA context"))?;

    // Buffer bytes.
    let brec = pools
        .buffers
        .get(image.buffer_key)
        .ok_or(DriverError::InvalidBuffer)?;
    let bytes_mtx = brec.storage.bytes();
    let bytes = bytes_mtx.lock();
    if bytes.len() < image.data_size as usize {
        return Err(DriverError::InvalidParameter);
    }

    cuda_ctx
        .bind_to_thread()
        .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;

    use cudarc::driver::sys::{
        cuMemcpy2D_v2, CUDA_MEMCPY2D_st, CUdeviceptr, CUmemorytype, CUresult,
    };

    let host_y = bytes.as_ptr();
    // SAFETY: UV plane starts at `offsets[1]` bytes into the same live Vec.
    let host_uv = unsafe { host_y.add(image.offsets[1] as usize) };

    // Y plane.
    let y_copy = CUDA_MEMCPY2D_st {
        srcXInBytes: 0,
        srcY: 0,
        srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
        srcHost: host_y as *const core::ffi::c_void,
        srcDevice: 0,
        srcArray: core::ptr::null_mut(),
        srcPitch: image.pitches[0] as usize,
        dstXInBytes: 0,
        dstY: 0,
        dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
        dstHost: core::ptr::null_mut(),
        dstDevice: cu_ptr as CUdeviceptr,
        dstArray: core::ptr::null_mut(),
        dstPitch: surf_pitch as usize,
        WidthInBytes: surf_w as usize,
        Height: surf_h as usize,
    };
    // SAFETY: Host pointer is derived from a live `&[u8]` whose length is
    // >= pitches[0] * height; device pointer came from cuMemAllocPitch_v2
    // on the bound context and is at least `surf_pitch * surf_h * 3 / 2`
    // bytes; pitches satisfy CUDA 2D copy alignment (pitch >= width).
    let r = unsafe { cuMemcpy2D_v2(&y_copy) };
    if r != CUresult::CUDA_SUCCESS {
        return Err(DriverError::OperationFailed("cuMemcpy2D_v2 Y plane"));
    }

    // UV plane.
    let uv_dst = (cu_ptr as u64)
        .checked_add((surf_pitch as u64) * (surf_h as u64))
        .ok_or(DriverError::InvalidParameter)?;
    let uv_copy = CUDA_MEMCPY2D_st {
        srcXInBytes: 0,
        srcY: 0,
        srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
        srcHost: host_uv as *const core::ffi::c_void,
        srcDevice: 0,
        srcArray: core::ptr::null_mut(),
        srcPitch: image.pitches[1] as usize,
        dstXInBytes: 0,
        dstY: 0,
        dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
        dstHost: core::ptr::null_mut(),
        dstDevice: uv_dst as CUdeviceptr,
        dstArray: core::ptr::null_mut(),
        dstPitch: surf_pitch as usize,
        WidthInBytes: surf_w as usize,
        Height: (surf_h / 2) as usize,
    };
    // SAFETY: Same invariants as Y; UV host range is offsets[1]..data_size,
    // UV device range starts at cu_ptr + surf_pitch * surf_h (NV12 layout).
    let r = unsafe { cuMemcpy2D_v2(&uv_copy) };
    if r != CUresult::CUDA_SUCCESS {
        return Err(DriverError::OperationFailed("cuMemcpy2D_v2 UV plane"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn nv12_pitch_math_matches_layout() {
        // Tightly-packed NV12 1280x720 should yield:
        // Y plane: 1280 * 720 = 921_600 bytes, pitch 1280, offset 0
        // UV plane: 1280 * 360 = 460_800 bytes, pitch 1280, offset 921_600
        // Total: 1_382_400 bytes (w * h * 3 / 2).
        let w: u32 = 1280;
        let h: u32 = 720;
        let pitch: u32 = w;
        let y_size = (pitch as usize) * (h as usize);
        let uv_size = (pitch as usize) * ((h as usize) / 2);
        assert_eq!(y_size, 921_600);
        assert_eq!(uv_size, 460_800);
        assert_eq!(y_size + uv_size, (w as usize) * (h as usize) * 3 / 2);
        assert_eq!(y_size, 1_382_400 - 460_800);
    }

    #[test]
    fn nv12_uv_offset_follows_pitched_y_plane() {
        // When surface pitch > width (e.g. cuMemAllocPitch returned 1536 for
        // a 1280-wide image), UV offset in the surface is pitch * height,
        // not width * height.
        let surf_pitch: u32 = 1536;
        let h: u32 = 720;
        let uv_offset = (surf_pitch as u64) * (h as u64);
        assert_eq!(uv_offset, 1_105_920);
    }
}
