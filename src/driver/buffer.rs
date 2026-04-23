//! VAAPI buffer vtable entries: `vaCreateBuffer`, `vaMapBuffer`, `vaMapBuffer2`,
//! `vaUnmapBuffer`, `vaDestroyBuffer`, `vaBufferSetNumElements`, `vaBufferInfo`.
//!
//! All buffer types (encode parameter buffers, coded output buffers) are stored
//! as heap-allocated `Vec<u8>` inside a `Mutex`. `vaMapBuffer` hands the caller
//! a raw pointer into the locked storage and intentionally holds the lock
//! until `vaUnmapBuffer` is called, matching the libva contract that the client
//! must not call other VA functions on the same buffer between Map and Unmap.

use super::{guard, state_from};
use crate::driver::state::{BufferRec, BufferStorage, CodedBufferSlot};
use crate::error::{DriverError, VAStatus};
use crate::ids::{BufferKey, ContainsKey, key_to_id};
use core::ffi::c_void;
use parking_lot::Mutex;
use va_sys as va;

pub unsafe extern "C" fn create_buffer(
    ctx: va::VADriverContextP,
    _context: va::VAContextID,
    buf_type: va::VABufferType,
    size: u32,
    num_elements: u32,
    data: *mut c_void,
    buf_id: *mut va::VABufferID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if buf_id.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        let total = (size as usize).checked_mul(num_elements as usize)
            .ok_or(DriverError::InvalidParameter)?;
        let mut bytes: Vec<u8> = Vec::new();
        // Try_reserve to surface OOM as VA_STATUS_ERROR_ALLOCATION_FAILED.
        bytes.try_reserve_exact(total).map_err(|_| DriverError::AllocFailed)?;
        bytes.resize(total, 0);
        if !data.is_null() && total > 0 {
            // SAFETY: caller promises `data` points to `total` readable bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data as *const u8,
                    bytes.as_mut_ptr(),
                    total,
                );
            }
        }
        // Coded-output буфери отримують окремий slot із pinned
        // VACodedBufferSegment (map-path у slice c.2).
        let storage = if buf_type == va::VAEncCodedBufferType {
            BufferStorage::Coded(CodedBufferSlot {
                backing: Mutex::new(bytes),
                segment: Mutex::new(None),
            })
        } else {
            BufferStorage::Generic(Mutex::new(bytes))
        };
        let rec = BufferRec {
            buf_type,
            element_size: size,
            num_elements,
            storage,
            coded_ready: Mutex::new(false),
        };
        let mut pools = state.pools.lock();
        let k: BufferKey = pools.buffers.insert(rec);
        // SAFETY: non-null checked.
        unsafe { *buf_id = key_to_id(k) as va::VABufferID };
        Ok(())
    })
}

pub unsafe extern "C" fn map_buffer(
    ctx: va::VADriverContextP,
    buf_id: va::VABufferID,
    pbuf: *mut *mut c_void,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if pbuf.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        let pools = state.pools.lock();
        let k = pools
            .buffers
            .find_by_low_bits(buf_id as u32)
            .ok_or(DriverError::InvalidBuffer)?;
        let rec = &pools.buffers[k];
        let ptr: *mut c_void = match &rec.storage {
            BufferStorage::Generic(m) => {
                // We hand back a raw pointer into the locked-Vec's storage.
                // UB-adjacent if the client retains it across destroy, but
                // matches the libva contract (Unmap releases access). We do
                // NOT release the lock here – that happens in UnmapBuffer.
                let mut g = m.lock();
                let p = g.as_mut_ptr();
                // Forget the guard so the lock stays held; Unmap reclaims it.
                core::mem::forget(g);
                p as *mut c_void
            }
            BufferStorage::Coded(slot) => {
                // Real bitstream pipeline is wired in slice c.2; until then
                // there is no segment to hand out.
                let seg = slot.segment.lock();
                match seg.as_ref() {
                    Some(boxed) => {
                        let raw = (&**boxed) as *const va::VACodedBufferSegment
                            as *mut va::VACodedBufferSegment;
                        raw as *mut c_void
                    }
                    None => return Err(DriverError::OperationFailed("coded buffer not ready — did you call vaEndPicture?")),
                }
            }
        };
        // SAFETY: non-null checked.
        unsafe { *pbuf = ptr };
        Ok(())
    })
}

pub unsafe extern "C" fn map_buffer2(
    ctx: va::VADriverContextP,
    buf_id: va::VABufferID,
    pbuf: *mut *mut c_void,
    _flags: u32,
) -> VAStatus {
    // SAFETY: pass-through.
    unsafe { map_buffer(ctx, buf_id, pbuf) }
}

pub unsafe extern "C" fn unmap_buffer(
    ctx: va::VADriverContextP,
    buf_id: va::VABufferID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let k = pools
            .buffers
            .find_by_low_bits(buf_id as u32)
            .ok_or(DriverError::InvalidBuffer)?;
        let rec = &pools.buffers[k];
        match &rec.storage {
            BufferStorage::Generic(m) => {
                // SAFETY: we forgot the guard in map_buffer, meaning the
                // mutex is logically held. Force-unlock to re-balance.
                unsafe { m.force_unlock() };
            }
            BufferStorage::Coded(_) => {
                // Coded map-path doesn't hold a lock across map/unmap yet;
                // the pinned VACodedBufferSegment is a stable borrow. Noop.
            }
        }
        // If this buffer is the backing store of a derived VAImage bound to
        // a CUDA-resident surface, trigger the host→device upload now. This
        // is what unblocks ffmpeg's hwupload path on CPU-origin clients.
        if rec.buf_type == va::VAImageBufferType {
            if let Some((image_ref, sk)) = pools
                .images
                .iter()
                .find_map(|(_ik, ir)| {
                    if ir.buffer_key == k { Some((ir, ir.surface_key?)) } else { None }
                })
            {
                crate::driver::image::upload_image_to_surface(ctx, &pools, image_ref, sk)?;
            }
        }
        Ok(())
    })
}

pub unsafe extern "C" fn destroy_buffer(
    ctx: va::VADriverContextP,
    buffer_id: va::VABufferID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let mut pools = state.pools.lock();
        let k = pools
            .buffers
            .find_by_low_bits(buffer_id as u32)
            .ok_or(DriverError::InvalidBuffer)?;
        pools.buffers.remove(k);
        Ok(())
    })
}

pub unsafe extern "C" fn buffer_set_num_elements(
    _ctx: va::VADriverContextP,
    _buf_id: va::VABufferID,
    _num_elements: u32,
) -> VAStatus {
    // Rarely used by encoders; accept as no-op.
    crate::error::VA_STATUS_SUCCESS
}

pub unsafe extern "C" fn buffer_info(
    ctx: va::VADriverContextP,
    buf_id: va::VABufferID,
    typ: *mut va::VABufferType,
    size: *mut u32,
    num_elements: *mut u32,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let k = pools
            .buffers
            .find_by_low_bits(buf_id as u32)
            .ok_or(DriverError::InvalidBuffer)?;
        let rec = &pools.buffers[k];
        // SAFETY: out pointers owned by caller; null-guarded.
        unsafe {
            if !typ.is_null() { *typ = rec.buf_type; }
            if !size.is_null() { *size = rec.element_size; }
            if !num_elements.is_null() { *num_elements = rec.num_elements; }
        }
        Ok(())
    })
}
