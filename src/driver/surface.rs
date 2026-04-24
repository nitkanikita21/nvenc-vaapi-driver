//! VAAPI surface vtable entries: `vaCreateSurfaces`, `vaCreateSurfaces2`,
//! `vaDestroySurfaces`, `vaQuerySurfaceStatus`, `vaQuerySurfaceAttributes`.
//!
//! Surfaces are stored as [`SurfaceRec`] entries in the `Pools::surfaces` slotmap.
//! The `kind` field tracks whether backing memory is a CUDA device allocation,
//! an imported DMA-BUF fd, or a placeholder stub.
//!
//! The DMA-BUF zero-copy path parses `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`
//! from the attribute list, reads the paired `VASurfaceAttribExternalBufferDescriptor`,
//! and imports the fd into CUDA through [`crate::cuda::external_mem`].
//! Registration with NVENC is deferred to first encode (see
//! `driver::picture::end_picture`).

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
        // Parse attribute list: look for DRM_PRIME_2 memory type + the
        // paired VASurfaceAttribExternalBufferDescriptor that actually
        // carries the DMA-BUF fds.
        let attribs: &[va::VASurfaceAttrib] = if !attrib_list.is_null() && num_attribs > 0 {
            // SAFETY: client-provided array of `num_attribs` VASurfaceAttrib.
            unsafe { core::slice::from_raw_parts(attrib_list, num_attribs as usize) }
        } else {
            &[]
        };
        let (want_drm_prime_2, prime_desc_ptr) = parse_drm_prime_attribs(attribs);
        if want_drm_prime_2 {
            let _ = (width, height); // dimensions come from the descriptor
            return create_surfaces2_dma_buf(
                ctx,
                state,
                format,
                surfaces,
                num_surfaces,
                prime_desc_ptr,
            );
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

/// Scan the client's `VASurfaceAttrib` list for the DMA-BUF import pair:
/// `VASurfaceAttribMemoryType == DRM_PRIME_2` plus the external-buffer
/// descriptor pointer.
///
/// Returned flag is true iff the client *asked* for DRM_PRIME_2. The
/// pointer is `None` when the attribute list did not contain the paired
/// external-buffer descriptor (invalid — `create_surfaces2_dma_buf` will
/// reject it).
fn parse_drm_prime_attribs(
    attribs: &[va::VASurfaceAttrib],
) -> (bool, Option<*const va::VADRMPRIMESurfaceDescriptor>) {
    let mut want = false;
    let mut desc: Option<*const va::VADRMPRIMESurfaceDescriptor> = None;
    for a in attribs {
        if a.type_ == va::VASurfaceAttribMemoryType {
            // SAFETY: VAGenericValue is a bindgen union; MemoryType
            // attributes always carry the `i` integer variant.
            let mem_type = unsafe { *a.value.value.i.as_ref() } as u32;
            if mem_type & va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 != 0 {
                want = true;
            }
        } else if a.type_ == va::VASurfaceAttribExternalBufferDescriptor {
            // SAFETY: the external buffer descriptor attribute carries
            // the `p` pointer variant per libva contract.
            let p = unsafe { *a.value.value.p.as_ref() };
            if !p.is_null() {
                desc = Some(p as *const va::VADRMPRIMESurfaceDescriptor);
            }
        }
    }
    (want, desc)
}

/// DRM_PRIME_2 path: import each (already-dup'd) DMA-BUF from the caller's
/// descriptor into CUDA via `cuImportExternalMemory`, wrap it in a
/// `SurfaceKind::ExternalDmaBuf`, and hand back surface IDs.
///
/// On import failure we return `AllocFailed` rather than `UnsupportedMemory`
/// so Chromium treats it as a retriable soft failure (fall back to its own
/// buffer allocator) instead of concluding the driver lacks DRM_PRIME_2
/// altogether and never offering zero-copy again.
fn create_surfaces2_dma_buf(
    ctx: va::VADriverContextP,
    state: &crate::driver::state::DriverState,
    format: u32,
    surfaces: *mut va::VASurfaceID,
    num_surfaces: u32,
    prime_desc_ptr: Option<*const va::VADRMPRIMESurfaceDescriptor>,
) -> Result<(), DriverError> {
    let Some(desc_ptr) = prime_desc_ptr else {
        crate::logging::error(
            ctx,
            "create_surfaces2: DRM_PRIME_2 requested without ExternalBufferDescriptor",
        );
        return Err(DriverError::InvalidParameter);
    };
    if num_surfaces != 1 {
        // Chromium/PipeWire one-descriptor-per-surface in practice. Batch
        // imports in one descriptor are not wired up in this cut.
        crate::logging::info(
            ctx,
            "create_surfaces2: DRM_PRIME_2 with num_surfaces != 1 not supported",
        );
        return Err(DriverError::AllocFailed);
    }
    // SAFETY: caller asserts the pointer is valid for the duration of
    // this call; bindgen struct is POD-layout.
    let desc: &va::VADRMPRIMESurfaceDescriptor = unsafe { &*desc_ptr };

    let image = match crate::cuda::external_mem::ExternalDmaBufImage::import(
        &state.cuda, desc,
    ) {
        Ok(img) => img,
        Err(e) => {
            crate::logging::error(
                ctx,
                "create_surfaces2: cuImportExternalMemory failed, Chromium will fall back",
            );
            // Normalise anything beyond the two retriable errors into
            // AllocFailed so the client treats it uniformly.
            return Err(match e {
                DriverError::InvalidParameter
                | DriverError::UnsupportedRtFormat
                | DriverError::UnsupportedMemory => e,
                _ => DriverError::AllocFailed,
            });
        }
    };

    let mut pools = state.pools.lock();
    let k: SurfaceKey = pools.surfaces.insert(SurfaceRec {
        width: image.width,
        height: image.height,
        format,
        kind: SurfaceKind::ExternalDmaBuf {
            image: Box::new(image),
            registered_nvenc: std::sync::OnceLock::new(),
        },
        registered: Mutex::new(None),
        cuda_ctx: None,
    });
    // SAFETY: caller provided a writable slot.
    unsafe { *surfaces = key_to_id(k) as va::VASurfaceID };
    Ok(())
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
                // For DMA-BUF-backed surfaces we must unregister from
                // NVENC first — otherwise the session still holds a
                // registered_ptr pointing at a CUarray whose backing
                // store is about to disappear.
                if let Some(rec) = pools.surfaces.get(k) {
                    if matches!(rec.kind, SurfaceKind::ExternalDmaBuf { .. }) {
                        for (_, cctx) in pools.contexts.iter() {
                            if let Some(sess) = cctx.session.lock().as_mut() {
                                sess.unregister_surface(k);
                            }
                        }
                    }
                }
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

/// DRM modifier sentinel meaning "any layout is acceptable". Defined by
/// `drm_fourcc.h` as `DRM_FORMAT_MOD_INVALID = ((1ULL << 56) - 1)`; we
/// inline it here because va-sys does not re-export DRM headers.
const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;

/// Build the fixed list of surface attributes supported by this driver.
///
/// NVENC H.264 on RTX 40-series caps out at 4096×4096 inputs; we advertise
/// NV12 as the only pixel format. `VASurfaceAttribMemoryType` now carries
/// both `VA` and `DRM_PRIME_2` bits (slice d) so Chromium/Vesktop use the
/// zero-copy DMA-BUF path for Wayland/PipeWire screen share. We also
/// advertise `VASurfaceAttribDRMFormatModifiers = DRM_FORMAT_MOD_INVALID`
/// which means "accept any layout" — the safest value until we exercise
/// the import path against real PipeWire output and can list the concrete
/// modifiers Nouveau/NVIDIA produce.
fn supported_attribs() -> [va::VASurfaceAttrib; 7] {
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
            (va::VA_SURFACE_ATTRIB_MEM_TYPE_VA
                | va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2) as i32,
        ),
        // DRMFormatModifiers: DRM_FORMAT_MOD_INVALID => accept any.
        // The 64-bit value fits into `bindgen_union_field` directly.
        {
            let mut value: va::VAGenericValue = unsafe { core::mem::zeroed() };
            value.type_ = va::VAGenericValueTypeInteger;
            value.value.bindgen_union_field = DRM_FORMAT_MOD_INVALID;
            va::VASurfaceAttrib {
                type_: va::VASurfaceAttribDRMFormatModifiers,
                flags: va::VA_SURFACE_ATTRIB_GETTABLE,
                value,
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_mem_type_attr(bits: u32) -> va::VASurfaceAttrib {
        // SAFETY: POD union zero-init, then write `i` variant.
        let mut value: va::VAGenericValue = unsafe { core::mem::zeroed() };
        value.type_ = va::VAGenericValueTypeInteger;
        value.value.bindgen_union_field = bits as u64;
        va::VASurfaceAttrib {
            type_: va::VASurfaceAttribMemoryType,
            flags: va::VA_SURFACE_ATTRIB_SETTABLE,
            value,
        }
    }

    fn mk_ext_buf_attr(p: *mut core::ffi::c_void) -> va::VASurfaceAttrib {
        // SAFETY: POD union zero-init, then write `p` pointer variant.
        let mut value: va::VAGenericValue = unsafe { core::mem::zeroed() };
        value.type_ = va::VAGenericValueTypePointer;
        value.value.bindgen_union_field = p as usize as u64;
        va::VASurfaceAttrib {
            type_: va::VASurfaceAttribExternalBufferDescriptor,
            flags: va::VA_SURFACE_ATTRIB_SETTABLE,
            value,
        }
    }

    #[test]
    fn parse_detects_drm_prime_2_bit() {
        let attribs = [mk_mem_type_attr(va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2)];
        let (want, desc) = parse_drm_prime_attribs(&attribs);
        assert!(want);
        assert!(desc.is_none());
    }

    #[test]
    fn parse_ignores_va_only_memtype() {
        let attribs = [mk_mem_type_attr(va::VA_SURFACE_ATTRIB_MEM_TYPE_VA)];
        let (want, _) = parse_drm_prime_attribs(&attribs);
        assert!(!want);
    }

    #[test]
    fn parse_extracts_external_buffer_descriptor_pointer() {
        // Allocate a zero-filled VADRMPRIMESurfaceDescriptor on the heap
        // (Box) so the pointer is stable for the duration of the test.
        let desc = Box::new(unsafe {
            core::mem::zeroed::<va::VADRMPRIMESurfaceDescriptor>()
        });
        let raw = Box::into_raw(desc);
        let attribs = [
            mk_mem_type_attr(va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2),
            mk_ext_buf_attr(raw as *mut core::ffi::c_void),
        ];
        let (want, parsed) = parse_drm_prime_attribs(&attribs);
        assert!(want);
        assert_eq!(parsed, Some(raw as *const va::VADRMPRIMESurfaceDescriptor));
        // SAFETY: reclaim and drop the Box we leaked via into_raw.
        let _ = unsafe { Box::from_raw(raw) };
    }

    #[test]
    fn parse_empty_attribs_returns_false() {
        let (want, desc) = parse_drm_prime_attribs(&[]);
        assert!(!want);
        assert!(desc.is_none());
    }

    #[test]
    fn supported_attribs_advertises_drm_prime_2_and_modifiers() {
        let attribs = supported_attribs();
        let mem_type = attribs
            .iter()
            .find(|a| a.type_ == va::VASurfaceAttribMemoryType)
            .expect("MemoryType advertised");
        let bits = unsafe { mem_type.value.value.bindgen_union_field } as u32;
        assert_eq!(
            bits & va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            "DRM_PRIME_2 bit missing"
        );
        assert_eq!(
            bits & va::VA_SURFACE_ATTRIB_MEM_TYPE_VA,
            va::VA_SURFACE_ATTRIB_MEM_TYPE_VA,
            "VA bit missing"
        );
        let modifiers = attribs
            .iter()
            .find(|a| a.type_ == va::VASurfaceAttribDRMFormatModifiers)
            .expect("DRMFormatModifiers advertised");
        let mod_val = unsafe { modifiers.value.value.bindgen_union_field };
        assert_eq!(mod_val, DRM_FORMAT_MOD_INVALID);
    }
}
