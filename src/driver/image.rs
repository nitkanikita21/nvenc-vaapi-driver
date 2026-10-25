//! NV12 VA images backed by a host shadow of internal CUDA surfaces.
//!
//! Read mappings download current surface pixels. Write mappings upload on
//! unmap; write-only mappings avoid readback. Standalone images use explicit
//! vaGetImage/vaPutImage transfers. Unsupported presentation/subpicture slots
//! remain populated because libva validates the vtable during driver loading.

use super::{guard, state_from};
use crate::driver::state::{BufferRec, BufferStorage, DriverState, ImageRec, Pools, SurfaceKind};
use crate::error::{DriverError, VA_STATUS_SUCCESS, VAStatus};
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
        let (image_out, _bk, _ik) = alloc_host_image(state, None, w, h, pitch, fourcc)?;
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
                .find_by_low_bits(surface)
                .ok_or(DriverError::InvalidSurface)?;
            let srec = &pools.surfaces[sk];
            if srec.format != va::VA_RT_FORMAT_YUV420 {
                return Err(DriverError::UnsupportedRtFormat);
            }
            let pitch = match &srec.kind {
                SurfaceKind::Internal { pitch, .. } => *pitch,
                _ => return Err(DriverError::OperationFailed("surface cannot be CPU mapped")),
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
            .find_by_low_bits(image_id)
            .ok_or(DriverError::InvalidImage)?;
        let bk = pools.images[ik].buffer_key;
        if pools
            .buffers
            .get(bk)
            .is_some_and(|buf| buf.mapped.lock().is_some())
        {
            return Err(DriverError::InvalidParameter);
        }
        let rec = pools.images.remove(ik).ok_or(DriverError::InvalidImage)?;
        // Best-effort: also drop the backing data buffer. If the client
        // already destroyed it separately this is a no-op.
        if let Some(buffer) = pools.buffers.remove(rec.buffer_key) {
            recycle_image_buffer(&mut pools, buffer);
        }
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
    ctx: va::VADriverContextP,
    surface: va::VASurfaceID,
    x: c_int,
    y: c_int,
    width: c_uint,
    height: c_uint,
    image: va::VAImageID,
) -> VAStatus {
    guard(|| {
        // SAFETY: libva owns this live driver context.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ik = pools
            .images
            .find_by_low_bits(image)
            .ok_or(DriverError::InvalidImage)?;
        let sk = pools
            .surfaces
            .find_by_low_bits(surface)
            .ok_or(DriverError::InvalidSurface)?;
        let image = &pools.images[ik];
        if image.surface_key == Some(sk) {
            return Err(DriverError::SurfaceBusy);
        }
        if width != image.width || height != image.height || x < 0 || y < 0 {
            return Err(DriverError::InvalidParameter);
        }
        download_image_from_surface(&pools, image, sk, x as u32, y as u32)?;
        if let Some(target) = image.surface_key {
            upload_image_to_surface(&pools, image, target)?;
        }
        Ok(())
    })
}

pub unsafe extern "C" fn put_image(
    ctx: va::VADriverContextP,
    surface: va::VASurfaceID,
    image: va::VAImageID,
    sx: c_int,
    sy: c_int,
    sw: c_uint,
    sh: c_uint,
    dx: c_int,
    dy: c_int,
    dw: c_uint,
    dh: c_uint,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let ik = pools
            .images
            .find_by_low_bits(image)
            .ok_or(DriverError::InvalidImage)?;
        let sk = pools
            .surfaces
            .find_by_low_bits(surface)
            .ok_or(DriverError::InvalidSurface)?;
        let irec = &pools.images[ik];
        if irec.surface_key == Some(sk) {
            return Err(DriverError::SurfaceBusy);
        }
        // Only accept full-frame, non-cropped puts for now. Partial puts
        // would require extra bookkeeping we do not yet need.
        if sx != 0
            || sy != 0
            || dx != 0
            || dy != 0
            || sw != irec.width
            || sh != irec.height
            || dw != irec.width
            || dh != irec.height
        {
            crate::logging::info(ctx, "put_image: partial/cropped puts not supported");
            return Err(DriverError::Unimplemented);
        }
        if let Some(source) = irec.surface_key {
            download_image_from_surface(&pools, irec, source, 0, 0)?;
        }
        upload_image_to_surface(&pools, irec, sk)
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
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn destroy_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn set_subpicture_image(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _image: va::VAImageID,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn set_subpicture_chromakey(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _min: c_uint,
    _max: c_uint,
    _mask: c_uint,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn set_subpicture_global_alpha(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _global_alpha: f32,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn associate_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _target_surfaces: *mut va::VASurfaceID,
    _num_surfaces: c_int,
    _sx: i16,
    _sy: i16,
    _sw: u16,
    _sh: u16,
    _dx: i16,
    _dy: i16,
    _dw: u16,
    _dh: u16,
    _flags: c_uint,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn deassociate_subpicture(
    _ctx: va::VADriverContextP,
    _subpicture: va::VASubpictureID,
    _target_surfaces: *mut va::VASurfaceID,
    _num_surfaces: c_int,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

// libva also requires a vaPutSurface pointer (window-system rendering).
pub unsafe extern "C" fn put_surface(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _draw: *mut core::ffi::c_void,
    _srcx: i16,
    _srcy: i16,
    _srcw: u16,
    _srch: u16,
    _destx: i16,
    _desty: i16,
    _destw: u16,
    _desth: u16,
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
    _ls: *mut c_uint,
    _cus: *mut c_uint,
    _cvs: *mut c_uint,
    _lo: *mut c_uint,
    _cuo: *mut c_uint,
    _cvo: *mut c_uint,
    _buffer_name: *mut c_uint,
    _buffer: *mut *mut core::ffi::c_void,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn unlock_surface(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

pub unsafe extern "C" fn query_surface_error(
    _ctx: va::VADriverContextP,
    _surface: va::VASurfaceID,
    _error_status: VAStatus,
    _error_info: *mut *mut core::ffi::c_void,
) -> VAStatus {
    guard(|| Err(DriverError::Unimplemented))
}

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
    #[cfg(feature = "profiling")]
    let _span = crate::profiling::Span::new(crate::profiling::Phase::HostImage);
    debug_assert_eq!(fourcc, va::VA_FOURCC_NV12);
    let image_width = u16::try_from(width).map_err(|_| DriverError::InvalidParameter)?;
    let image_height = u16::try_from(height).map_err(|_| DriverError::InvalidParameter)?;
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 || pitch < width {
        return Err(DriverError::InvalidParameter);
    }
    let y_size = (pitch as usize)
        .checked_mul(height as usize)
        .ok_or(DriverError::InvalidParameter)?;
    let uv_size = (pitch as usize)
        .checked_mul((height as usize) / 2)
        .ok_or(DriverError::InvalidParameter)?;
    let total = y_size
        .checked_add(uv_size)
        .ok_or(DriverError::InvalidParameter)?;

    let total_u32 = u32::try_from(total).map_err(|_| DriverError::InvalidParameter)?;
    let mut bytes = core::mem::take(&mut state.pools.lock().image_bytes_cache);
    // Standalone images start clear. Derived images refresh visible pixels
    // on read maps; write-only maps discard them, so cached initialized bytes
    // can be reused without clearing a full frame. New bytes are always zeroed.
    if surface_key.is_none() {
        bytes.clear();
    }
    bytes
        .try_reserve_exact(total.saturating_sub(bytes.len()))
        .map_err(|_| DriverError::AllocFailed)?;
    bytes.resize(total, 0);

    let mut pools = state.pools.lock();
    let bk: BufferKey = pools.buffers.insert(BufferRec {
        buf_type: va::VAImageBufferType,
        element_size: total_u32,
        num_elements: 1,
        allocated_elements: 1,
        storage: BufferStorage::Generic(Mutex::new(bytes)),
        coded_ready: Mutex::new(false),
        pending_output: Mutex::new(None),
        mapped: Mutex::new(None),
    })?;
    let pitches = [pitch, pitch, 0];
    let offsets = [0u32, y_size as u32, 0];
    let inserted = pools.images.insert(ImageRec {
        surface_key,
        fourcc,
        width,
        height,
        pitches,
        offsets,
        data_size: total_u32,
        buffer_key: bk,
    });
    let ik: ImageKey = match inserted {
        Ok(key) => key,
        Err(error) => {
            pools.buffers.remove(bk);
            return Err(error);
        }
    };

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
        width: image_width,
        height: image_height,
        data_size: total_u32,
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

/// Retain at most one unmapped image allocation per display, up to 16 MiB.
/// Coded/parameter buffers and oversized images keep their ordinary lifetime.
pub(crate) fn recycle_image_buffer(pools: &mut Pools, buffer: BufferRec) {
    if buffer.buf_type != va::VAImageBufferType {
        return;
    }
    if let BufferStorage::Generic(bytes) = buffer.storage {
        let bytes = bytes.into_inner();
        if bytes.capacity() <= 16 * 1024 * 1024
            && bytes.capacity() > pools.image_bytes_cache.capacity()
        {
            pools.image_bytes_cache = bytes;
        }
    }
}

/// Trigger `cuMemcpy2D_v2` host → device for all planes of an NV12 image
/// into an internal CUDA surface. Other memory types require a separate path.
pub(crate) fn upload_image_to_surface(
    pools: &Pools,
    image: &ImageRec,
    surface_key: SurfaceKey,
) -> Result<(), DriverError> {
    // Buffer bytes.
    let brec = pools
        .buffers
        .get(image.buffer_key)
        .ok_or(DriverError::InvalidBuffer)?;
    if brec.mapped.lock().is_some() {
        return Err(DriverError::InvalidParameter);
    }
    let srec = pools
        .surfaces
        .get(surface_key)
        .ok_or(DriverError::InvalidSurface)?;
    let (cu_ptr, surf_pitch, surf_w, surf_h) = match &srec.kind {
        SurfaceKind::Internal {
            cu_ptr,
            pitch,
            width,
            height,
        } => (*cu_ptr, *pitch, *width, *height),
        _ => return Err(DriverError::UnsupportedMemory),
    };
    if image.width != surf_w || image.height != surf_h {
        return Err(DriverError::InvalidParameter);
    }
    super::completion::surface(pools, surface_key, u64::MAX)?;
    let cuda_ctx = srec
        .cuda_ctx
        .as_ref()
        .ok_or(DriverError::OperationFailed("surface has no CUDA context"))?;

    let bytes_mtx = brec.storage.bytes();
    let bytes = bytes_mtx.lock();
    if bytes.len() < image.data_size as usize {
        return Err(DriverError::InvalidParameter);
    }

    cuda_ctx
        .bind_to_thread()
        .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;

    use cudarc::driver::sys::{
        CUDA_MEMCPY2D_st, CUdeviceptr, CUmemorytype, CUresult, cuMemcpy2D_v2,
    };

    let rows = nv12_upload_rows(image)?;
    if surf_pitch < surf_w {
        return Err(DriverError::InvalidParameter);
    }
    // Both planes use the same pitch and are adjacent in driver-owned images
    // and surfaces. One 2D transfer avoids a second synchronous CUDA call.
    let copy = CUDA_MEMCPY2D_st {
        srcXInBytes: 0,
        srcY: 0,
        srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
        srcHost: bytes.as_ptr().cast(),
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
        Height: rows,
    };
    // SAFETY: validated contiguous NV12 rows fit in image.data_size and the
    // live host buffer. The CUDA surface owns pitch * (height + height/2)
    // bytes; its context is bound and input/output pitches cover the width.
    if crate::profiling::timed!(Upload, unsafe { cuMemcpy2D_v2(&copy) }) != CUresult::CUDA_SUCCESS {
        return Err(DriverError::OperationFailed("cuMemcpy2D_v2 NV12 image"));
    }
    Ok(())
}

/// Copy a chroma-aligned surface rectangle into a driver-owned NV12 image.
/// The caller holds the pools lock; mapping is checked before locking bytes.
pub(crate) fn download_image_from_surface(
    pools: &Pools,
    image: &ImageRec,
    surface_key: SurfaceKey,
    x: u32,
    y: u32,
) -> Result<(), DriverError> {
    nv12_upload_rows(image)?;
    let buffer = pools
        .buffers
        .get(image.buffer_key)
        .ok_or(DriverError::InvalidBuffer)?;
    if buffer.mapped.lock().is_some() {
        return Err(DriverError::InvalidParameter);
    }
    let surface = pools
        .surfaces
        .get(surface_key)
        .ok_or(DriverError::InvalidSurface)?;
    validate_nv12_region(
        x,
        y,
        image.width,
        image.height,
        surface.width,
        surface.height,
    )?;
    let (cu_ptr, pitch) = match &surface.kind {
        SurfaceKind::Internal { cu_ptr, pitch, .. } => (*cu_ptr, *pitch),
        _ => return Err(DriverError::UnsupportedMemory),
    };
    if pitch < surface.width {
        return Err(DriverError::InvalidParameter);
    }
    super::completion::surface(pools, surface_key, u64::MAX)?;
    let cuda = surface
        .cuda_ctx
        .as_ref()
        .ok_or(DriverError::OperationFailed("surface has no CUDA context"))?;
    let mut bytes = buffer.storage.bytes().lock();
    if bytes.len() < image.data_size as usize {
        return Err(DriverError::InvalidParameter);
    }
    cuda.bind_to_thread()
        .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
    use cudarc::driver::sys::{CUDA_MEMCPY2D_st, CUmemorytype, CUresult, cuMemcpy2D_v2};
    let combined = y == 0 && image.height == surface.height;
    for plane in 0..if combined { 1 } else { 2 } {
        let divisor = if plane == 0 { 1 } else { 2 };
        let device_offset = if plane == 0 {
            0
        } else {
            u64::from(pitch) * u64::from(surface.height)
        };
        let copy = CUDA_MEMCPY2D_st {
            srcXInBytes: x as usize,
            srcY: (y / divisor) as usize,
            srcMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
            srcDevice: cu_ptr + device_offset,
            srcPitch: pitch as usize,
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
            // SAFETY: validated plane layout and live buffer cover both planes.
            dstHost: unsafe { bytes.as_mut_ptr().add(image.offsets[plane] as usize) }.cast(),
            dstPitch: image.pitches[plane] as usize,
            WidthInBytes: image.width as usize,
            Height: if combined {
                image.height as usize + image.height as usize / 2
            } else {
                (image.height / divisor) as usize
            },
            srcHost: core::ptr::null(),
            srcArray: core::ptr::null_mut(),
            dstXInBytes: 0,
            dstY: 0,
            dstDevice: 0,
            dstArray: core::ptr::null_mut(),
        };
        // SAFETY: source region fits the live CUDA allocation; host layout and
        // lengths are validated above, and its CUDA context is current.
        if crate::profiling::timed!(Readback, unsafe { cuMemcpy2D_v2(&copy) })
            != CUresult::CUDA_SUCCESS
        {
            return Err(DriverError::OperationFailed("cuMemcpy2D_v2 NV12 readback"));
        }
    }
    Ok(())
}

fn validate_nv12_region(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    surface_width: u32,
    surface_height: u32,
) -> Result<(), DriverError> {
    if x % 2 != 0
        || y % 2 != 0
        || width == 0
        || height == 0
        || width % 2 != 0
        || height % 2 != 0
        || x.checked_add(width).is_none_or(|end| end > surface_width)
        || y.checked_add(height).is_none_or(|end| end > surface_height)
    {
        return Err(DriverError::InvalidParameter);
    }
    Ok(())
}

/// Validate that a single pitched transfer spans both NV12 planes safely.
fn nv12_upload_rows(image: &ImageRec) -> Result<usize, DriverError> {
    let pitch = u64::from(image.pitches[0]);
    let height = u64::from(image.height);
    let rows = height + height / 2;
    let y_size = pitch
        .checked_mul(height)
        .ok_or(DriverError::InvalidParameter)?;
    let total = pitch
        .checked_mul(rows)
        .ok_or(DriverError::InvalidParameter)?;
    if image.fourcc != va::VA_FOURCC_NV12
        || image.width == 0
        || image.height == 0
        || image.width % 2 != 0
        || image.height % 2 != 0
        || pitch < u64::from(image.width)
        || image.pitches[1] != image.pitches[0]
        || image.offsets[0] != 0
        || u64::from(image.offsets[1]) != y_size
        || total > u64::from(image.data_size)
    {
        return Err(DriverError::InvalidParameter);
    }
    usize::try_from(rows).map_err(|_| DriverError::InvalidParameter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pitched_image() -> ImageRec {
        ImageRec {
            surface_key: None,
            buffer_key: Default::default(),
            fourcc: va::VA_FOURCC_NV12,
            width: 854,
            height: 480,
            pitches: [864, 864, 0],
            offsets: [0, 864 * 480, 0],
            data_size: 864 * 720,
        }
    }

    #[test]
    fn nv12_regions_reject_misalignment_bounds_and_overflow() {
        assert!(validate_nv12_region(8, 6, 22, 12, 66, 34).is_ok());
        for (x, y, w, h) in [
            (1, 0, 2, 2),
            (0, 1, 2, 2),
            (0, 0, 3, 2),
            (0, 0, 2, 3),
            (0, 0, 0, 2),
            (66, 0, 2, 2),
            (0, 34, 2, 2),
            (u32::MAX - 1, 0, 4, 2),
        ] {
            assert!(validate_nv12_region(x, y, w, h, 66, 34).is_err());
        }
    }

    #[test]
    fn combined_upload_includes_chroma_rows_with_padded_pitch() {
        assert_eq!(nv12_upload_rows(&pitched_image()).unwrap(), 720);
    }

    #[test]
    fn combined_upload_rejects_noncontiguous_or_out_of_bounds_planes() {
        let base = pitched_image();
        for (pitch, offset, size, height) in [
            (848, base.offsets[1], base.data_size, 480),
            (864, base.offsets[1] + 1, base.data_size, 480),
            (864, base.offsets[1], base.data_size - 1, 480),
            (864, base.offsets[1], base.data_size, 479),
        ] {
            let mut image = pitched_image();
            image.pitches[1] = pitch;
            image.offsets[1] = offset;
            image.data_size = size;
            image.height = height;
            assert!(nv12_upload_rows(&image).is_err());
        }
    }

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
