//! VAAPI surface vtable entries: `vaCreateSurfaces`, `vaCreateSurfaces2`,
//! `vaDestroySurfaces`, `vaQuerySurfaceStatus`, `vaQuerySurfaceAttributes`.
//!
//! Surfaces are stored as [`SurfaceRec`] entries in the `Pools::surfaces` slotmap.
//! The `kind` field tracks whether backing memory is a CUDA device allocation,
//! an imported DMA-BUF fd, or a placeholder stub.
//!
//! The DMA-BUF zero-copy path (parsing `VASurfaceAttribExternalBuffers` with
//! `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2` and calling `cuImportExternalMemory`)
//! is not yet implemented; `create_surfaces2` currently allocates `Stub` entries.

use super::{guard, state_from};
use crate::driver::state::{SurfaceKind, SurfaceRec};
use crate::error::{DriverError, VAStatus};
use crate::ids::{ContainsKey, SurfaceKey, key_to_id};
use core::ffi::c_int;
use parking_lot::Mutex;
use va_sys as va;

pub unsafe extern "C" fn create_surfaces(
    ctx: va::VADriverContextP,
    width: c_int,
    height: c_int,
    format: c_int,
    num_surfaces: c_int,
    surfaces: *mut va::VASurfaceID,
) -> VAStatus {
    // Delegate to create_surfaces2 with no attributes.
    // SAFETY: arg pass-through.
    unsafe {
        create_surfaces2(
            ctx,
            format as u32,
            width as u32,
            height as u32,
            surfaces,
            num_surfaces as u32,
            core::ptr::null_mut(),
            0,
        )
    }
}

pub unsafe extern "C" fn create_surfaces2(
    ctx: va::VADriverContextP,
    format: u32,
    width: u32,
    height: u32,
    surfaces: *mut va::VASurfaceID,
    num_surfaces: u32,
    attrib_list: *mut va::VASurfaceAttrib,
    num_attribs: u32,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx validity delegated.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if surfaces.is_null() || num_surfaces == 0 {
            return Err(DriverError::InvalidParameter);
        }
        if format != va::VA_RT_FORMAT_YUV420 {
            return Err(DriverError::UnsupportedRtFormat);
        }
        // DRM_PRIME_2 import — slice d. Явно відкидаємо із UNSUPPORTED_MEMORY_TYPE,
        // щоб клієнт зміг fallback-нутись на INTERNAL allocation.
        if !attrib_list.is_null() && num_attribs > 0 {
            // SAFETY: client-provided array of `num_attribs` VASurfaceAttrib.
            let attribs = unsafe {
                core::slice::from_raw_parts(attrib_list, num_attribs as usize)
            };
            for a in attribs {
                if a.type_ == va::VASurfaceAttribMemoryType {
                    // SAFETY: VAGenericValue.value — union; `i` валідний
                    // варіант (MemoryType завжди int).
                    let mem_type = unsafe { *a.value.value.i.as_ref() } as u32;
                    if mem_type == va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 {
                        crate::logging::info(
                            ctx,
                            "create_surfaces2: DRM_PRIME_2 import not yet supported (slice d)",
                        );
                        return Err(DriverError::UnsupportedMemory);
                    }
                }
            }
        }
        // Try to allocate CUDA-resident NV12 storage for each surface. If
        // CUDA init fails (no GPU on this host), fall back to `Stub` so the
        // library still loads for libva probe callers — the real encode
        // path will fail later with OPERATION_FAILED, not a crash.
        let cuda_ctx_opt = state.cuda.get_or_init().ok();
        let mut pools = state.pools.lock();
        for i in 0..num_surfaces {
            let (kind, ctx_for_rec) = match &cuda_ctx_opt {
                Some(cuda_arc) => match allocate_nv12(cuda_arc, width, height) {
                    Ok((cu_ptr, pitch)) => (
                        SurfaceKind::Internal { cu_ptr, pitch, width, height },
                        Some(cuda_arc.clone()),
                    ),
                    Err(e) => {
                        crate::logging::error(
                            ctx,
                            "create_surfaces2: cuMemAllocPitch_v2 failed",
                        );
                        return Err(e);
                    }
                },
                None => (SurfaceKind::Stub, None),
            };
            let k: SurfaceKey = pools.surfaces.insert(SurfaceRec {
                width,
                height,
                format,
                kind,
                registered: Mutex::new(None),
                cuda_ctx: ctx_for_rec,
            });
            // SAFETY: caller provided a `num_surfaces`-sized array.
            unsafe { *surfaces.add(i as usize) = key_to_id(k) as va::VASurfaceID };
        }
        Ok(())
    })
}

/// Allocate one NV12 surface via `cuMemAllocPitch_v2`.
///
/// NV12 layout: Y plane (width × height) + interleaved UV plane
/// (width × height/2). We allocate those 1.5 × height rows as a single
/// pitched block so the Y pitch and UV pitch are identical, which NVENC
/// requires when registering through `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`.
fn allocate_nv12(
    cuda: &std::sync::Arc<cudarc::driver::CudaContext>,
    width: u32,
    height: u32,
) -> Result<(u64, u32), DriverError> {
    cuda.bind_to_thread()
        .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
    let width_bytes = width as usize;
    let rows = (height as usize) * 3 / 2;
    let mut dptr: cudarc::driver::sys::CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: FFI: dptr/pitch are writable locals, a live CUDA context is
    // bound to this thread, and element_size=16 meets the alignment
    // requirement for any NV12 texture fetch.
    let r = unsafe {
        cudarc::driver::sys::cuMemAllocPitch_v2(
            &mut dptr,
            &mut pitch,
            width_bytes,
            rows,
            16,
        )
    };
    if r != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
        return Err(DriverError::AllocFailed);
    }
    let pitch_u32 = u32::try_from(pitch).map_err(|_| DriverError::InvalidParameter)?;
    Ok((dptr as u64, pitch_u32))
}

pub unsafe extern "C" fn destroy_surfaces(
    ctx: va::VADriverContextP,
    surface_list: *mut va::VASurfaceID,
    num_surfaces: c_int,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if surface_list.is_null() || num_surfaces < 0 {
            return Err(DriverError::InvalidParameter);
        }
        let mut pools = state.pools.lock();
        // SAFETY: caller-provided array of `num_surfaces` u32 IDs.
        let ids = unsafe {
            core::slice::from_raw_parts(surface_list, num_surfaces as usize)
        };
        for id in ids {
            if let Some(k) = pools.surfaces.find_by_low_bits(*id as u32) {
                pools.surfaces.remove(k);
            }
        }
        Ok(())
    })
}

pub unsafe extern "C" fn query_surface_status(
    _ctx: va::VADriverContextP,
    _render_target: va::VASurfaceID,
    status: *mut va::VASurfaceStatus,
) -> VAStatus {
    guard(|| {
        if status.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        // Always ready – we are synchronous until NVENC is wired in.
        // SAFETY: non-null checked.
        unsafe { *status = va::VASurfaceReady };
        Ok(())
    })
}

/// Build the fixed list of surface attributes supported by this driver.
///
/// NVENC H.264 on RTX 40-series caps out at 4096×4096 inputs; we also advertise
/// NV12 as the only pixel format (single-plane register path in c.2) and the VA
/// internal memory type (raw DMA-BUF import — `DRM_PRIME_2` — is deferred to a
/// later slice and deliberately *not* advertised here so that Chromium does not
/// try to hand us external buffers it knows we cannot consume).
fn supported_attribs() -> [va::VASurfaceAttrib; 6] {
    fn int_attr(ty: va::VASurfaceAttribType, flags: u32, v: i32) -> va::VASurfaceAttrib {
        // SAFETY: `VAGenericValue` contains a bindgen-generated union of
        // `i/f/p/fn_` variants — all POD-safe. Zero-initialising the union and
        // then writing the `i` slot via `bindgen_union_field` matches the layout
        // bindgen emits for the `union { int i; float f; void* p; VAGenericFunc fn; }`
        // C declaration, and pairs with `type_ = VAGenericValueTypeInteger` so
        // consumers read only the integer variant.
        let mut value: va::VAGenericValue = unsafe { core::mem::zeroed() };
        value.type_ = va::VAGenericValueTypeInteger;
        value.value.bindgen_union_field = v as u32 as u64;
        va::VASurfaceAttrib {
            type_: ty,
            flags,
            value,
        }
    }
    [
        int_attr(
            va::VASurfaceAttribPixelFormat,
            va::VA_SURFACE_ATTRIB_GETTABLE | va::VA_SURFACE_ATTRIB_SETTABLE,
            va::VA_FOURCC_NV12 as i32,
        ),
        int_attr(
            va::VASurfaceAttribMinWidth,
            va::VA_SURFACE_ATTRIB_GETTABLE,
            16,
        ),
        int_attr(
            va::VASurfaceAttribMinHeight,
            va::VA_SURFACE_ATTRIB_GETTABLE,
            16,
        ),
        int_attr(
            va::VASurfaceAttribMaxWidth,
            va::VA_SURFACE_ATTRIB_GETTABLE,
            4096,
        ),
        int_attr(
            va::VASurfaceAttribMaxHeight,
            va::VA_SURFACE_ATTRIB_GETTABLE,
            4096,
        ),
        int_attr(
            va::VASurfaceAttribMemoryType,
            va::VA_SURFACE_ATTRIB_GETTABLE | va::VA_SURFACE_ATTRIB_SETTABLE,
            va::VA_SURFACE_ATTRIB_MEM_TYPE_VA as i32,
        ),
    ]
}

pub unsafe extern "C" fn query_surface_attributes(
    _ctx: va::VADriverContextP,
    _config: va::VAConfigID,
    attrib_list: *mut va::VASurfaceAttrib,
    num_attribs: *mut u32,
) -> VAStatus {
    guard(|| {
        if num_attribs.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        let attribs = supported_attribs();
        let total = attribs.len() as u32;
        // SAFETY: non-null checked above.
        let capacity = unsafe { *num_attribs };
        // SAFETY: non-null checked above.
        unsafe { *num_attribs = total };

        // Two-call pattern: first call passes `attrib_list = NULL` to query the
        // required size, second call fills an allocation of that size.
        if attrib_list.is_null() {
            return Ok(());
        }
        if capacity < total {
            // Caller's buffer is too small; `*num_attribs` was updated above so
            // the caller can retry with a correctly-sized allocation.
            return Err(DriverError::InvalidParameter);
        }
        // SAFETY: caller guaranteed `attrib_list` has at least `capacity >= total`
        // VASurfaceAttrib slots; both structs are `repr(C)` and trivially copyable.
        unsafe {
            core::ptr::copy_nonoverlapping(attribs.as_ptr(), attrib_list, attribs.len());
        }
        Ok(())
    })
}
