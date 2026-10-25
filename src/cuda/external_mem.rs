//! DMA-BUF → CUDA external memory import (RAII wrapper).
//!
//! Only tightly packed, single-object, linear NV12 can be mapped as the
//! packed CUDA array used by this backend. Reject other plane layouts and
//! modifiers rather than silently interpreting tiled/padded pixels as linear.
//! Successful OPAQUE_FD import transfers the duplicate fd to CUDA. The
//! retained CUDA context outlives both the mapped array and external memory.

use crate::cuda::CudaCtx;
use crate::error::DriverError;
use cudarc::driver::sys as cu;
use va_sys as va;

/// A DMA-BUF imported into CUDA as a mipmapped array, plus the level-0
/// `CUarray` handles NVENC expects for registration.
///
/// # Invariants
/// * `ext_mem` is a live `CUexternalMemory` from `cuImportExternalMemory`.
/// * `mip` is a live `CUmipmappedArray` produced from `ext_mem`.
/// * `plane0` is the level-0 `CUarray` extracted from `mip`. It aliases
///   memory owned by `mip` and therefore MUST NOT be destroyed directly;
///   it dies with `mip` in `Drop`.
/// * CUDA owns the imported duplicate fd; only the failed-import path closes it.
///
/// # Thread-safety
/// Not `Sync`. The raw CUDA handles are thread-safe from CUDA's side, but
/// sharing a `&mut` of this struct across threads would race `Drop`.
pub struct ExternalDmaBufImage {
    ext_mem: cu::CUexternalMemory,
    mip: cu::CUmipmappedArray,
    /// Level-0 array for plane 0 (Y for NV12; full buffer for packed NV12).
    pub plane0: cu::CUarray,
    /// Level-0 array for plane 1 (UV for NV12 at half-height). Currently
    /// always `None` — packed NV12 is registered as a single array that
    /// spans both planes (see module docs).
    pub plane1: Option<cu::CUarray>,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    cuda_ctx: std::sync::Arc<cudarc::driver::CudaContext>,
}

// SAFETY: the CUDA runtime serialises all external-memory operations; the
// handles carry no per-thread state. We only ever hand out `&mut self` from
// `Drop`, so Send is sound.
unsafe impl Send for ExternalDmaBufImage {}

impl ExternalDmaBufImage {
    /// Import a DMA-BUF fd described by `desc` into CUDA.
    ///
    /// # Preconditions
    /// * `desc.objects[0].fd` is a valid, currently-open DMA-BUF file
    ///   descriptor. The caller may close their fd immediately on return
    ///   because we dup it internally.
    /// * `desc.fourcc` is `VA_FOURCC_NV12` (other formats return
    ///   `DriverError::UnsupportedRtFormat`).
    /// * `desc` passes `validate_descriptor`.
    ///
    /// # Postconditions on success
    /// `plane0` points at a level-0 mipmap slice whose lifetime is tied to
    /// the returned value. On `Drop` the mipmap, external memory and the
    /// dup'd fd are released in the correct order.
    ///
    /// # Errors
    /// * `InvalidParameter` — zero dimensions, `num_objects == 0`,
    ///   `num_layers == 0`, negative fd or zero size.
    /// * `UnsupportedRtFormat` — `fourcc` not NV12.
    /// * `AllocFailed` — any CUDA call failed. Chromium maps this to a
    ///   soft surface-allocation failure and retries without DMA-BUF,
    ///   instead of deciding the driver has no DRM_PRIME_2 support at all
    ///   (which it would infer from `UnsupportedMemory`).
    pub fn import(
        cuda: &CudaCtx,
        desc: &va::VADRMPRIMESurfaceDescriptor,
    ) -> Result<Self, DriverError> {
        validate_descriptor(desc)?;
        let obj = &desc.objects[0];

        // 2. Ensure CUDA context exists and is current on this thread.
        let cuda_ctx = cuda.get_or_init()?;
        cuda_ctx
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;

        // 3. dup the fd so we own a copy regardless of what the caller
        //    does with theirs.
        // SAFETY: obj.fd is a valid open fd per the descriptor contract.
        let dup_fd = unsafe { libc::fcntl(obj.fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup_fd < 0 {
            return Err(DriverError::OperationFailed("fcntl(F_DUPFD_CLOEXEC)"));
        }

        // 4. Build the external memory handle descriptor.
        //    SAFETY: the whole struct is POD; bindgen produces a union we
        //    zero-initialise, then overwrite `fd` variant.
        let mut handle_desc: cu::CUDA_EXTERNAL_MEMORY_HANDLE_DESC = unsafe { core::mem::zeroed() };
        handle_desc.type_ =
            cu::CUexternalMemoryHandleType::CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD;
        handle_desc.handle.fd = dup_fd;
        handle_desc.size = obj.size as u64;
        handle_desc.flags = 0;

        // 5. Import.
        let mut ext_mem: cu::CUexternalMemory = core::ptr::null_mut();
        // SAFETY: `ext_mem` is a writable local; `handle_desc` lives through
        // the call; dup_fd is open and owned by us.
        let r = unsafe { cu::cuImportExternalMemory(&mut ext_mem, &handle_desc) };
        if r != cu::CUresult::CUDA_SUCCESS || ext_mem.is_null() {
            // SAFETY: we own dup_fd; no one else has observed it yet.
            unsafe { libc::close(dup_fd) };
            return Err(DriverError::AllocFailed);
        }

        // 6. Map a single mipmapped array spanning the packed NV12 buffer.
        //    For packed NV12 the combined size is width × (height * 3 / 2).
        //    We expose it as a single-channel UINT8 2D array — NVENC does
        //    not consult the declared channel count for CUDAARRAY input;
        //    it reads `width`/`height`/`pitch` on register-resource.
        let y_rows = desc.height as usize;
        let packed_rows = y_rows + y_rows / 2;
        // SAFETY: POD descriptor zero-init, all fields then explicitly set.
        let mut mip_desc: cu::CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC =
            unsafe { core::mem::zeroed() };
        mip_desc.offset = u64::from(desc.layers[0].offset[0]);
        mip_desc.arrayDesc.Width = desc.width as usize;
        mip_desc.arrayDesc.Height = packed_rows;
        mip_desc.arrayDesc.Depth = 0;
        mip_desc.arrayDesc.Format = cu::CUarray_format::CU_AD_FORMAT_UNSIGNED_INT8;
        mip_desc.arrayDesc.NumChannels = 1;
        mip_desc.arrayDesc.Flags = 0;
        mip_desc.numLevels = 1;

        let mut mip: cu::CUmipmappedArray = core::ptr::null_mut();
        // SAFETY: ext_mem is live; mip_desc is fully initialised; mip is a
        // writable local.
        let r =
            unsafe { cu::cuExternalMemoryGetMappedMipmappedArray(&mut mip, ext_mem, &mip_desc) };
        if r != cu::CUresult::CUDA_SUCCESS || mip.is_null() {
            // SAFETY: ext_mem came from cuImportExternalMemory above.
            unsafe { cu::cuDestroyExternalMemory(ext_mem) };
            return Err(DriverError::AllocFailed);
        }

        // 7. Fetch level 0 — NVENC wants a plain CUarray handle, not the
        //    mipmap.
        let mut plane0: cu::CUarray = core::ptr::null_mut();
        // SAFETY: mip is live; plane0 is a writable local.
        let r = unsafe { cu::cuMipmappedArrayGetLevel(&mut plane0, mip, 0) };
        if r != cu::CUresult::CUDA_SUCCESS || plane0.is_null() {
            // SAFETY: mip from cuExternalMemoryGetMappedMipmappedArray.
            unsafe { cu::cuMipmappedArrayDestroy(mip) };
            // SAFETY: ext_mem from cuImportExternalMemory.
            unsafe { cu::cuDestroyExternalMemory(ext_mem) };
            return Err(DriverError::AllocFailed);
        }

        Ok(Self {
            ext_mem,
            mip,
            plane0,
            plane1: None,
            width: desc.width,
            height: desc.height,
            fourcc: desc.fourcc,
            cuda_ctx,
        })
    }
}

impl Drop for ExternalDmaBufImage {
    fn drop(&mut self) {
        if self.cuda_ctx.bind_to_thread().is_err() {
            return;
        }
        // Teardown order is load-bearing: the mipmapped array references
        // the external memory, which references the fd. Reverse destroy.
        // Never panic from Drop — log-only is the caller's concern.
        // SAFETY: mip produced by cuExternalMemoryGetMappedMipmappedArray.
        unsafe {
            let _ = cu::cuMipmappedArrayDestroy(self.mip);
        }
        // SAFETY: ext_mem produced by cuImportExternalMemory.
        unsafe {
            let _ = cu::cuDestroyExternalMemory(self.ext_mem);
        }
        // A successful OPAQUE_FD import transfers fd ownership to CUDA.
        // Closing that numeric descriptor again could close an unrelated fd
        // that another thread has opened since the import.
    }
}

/// Pure-logic validation mirror of `ExternalDmaBufImage::import`.
///
/// Returns the same `DriverError` a full `import` call would return for
/// parameter-level validation failures (no CUDA calls). Used by unit tests
/// to exercise every branch without a GPU.
pub fn validate_descriptor(desc: &va::VADRMPRIMESurfaceDescriptor) -> Result<(), DriverError> {
    if desc.num_objects == 0 || desc.num_layers == 0 || desc.num_layers as usize > desc.layers.len()
    {
        return Err(DriverError::InvalidParameter);
    }
    if desc.width == 0 || desc.height == 0 || desc.width & 1 != 0 || desc.height & 1 != 0 {
        return Err(DriverError::InvalidParameter);
    }
    if desc.fourcc != va::VA_FOURCC_NV12 {
        return Err(DriverError::UnsupportedRtFormat);
    }
    if desc.num_objects != 1 {
        return Err(DriverError::UnsupportedMemory);
    }
    let obj = &desc.objects[0];
    if obj.fd < 0 || obj.size == 0 {
        return Err(DriverError::InvalidParameter);
    }
    if desc.num_layers != 1 || obj.drm_format_modifier != 0 {
        return Err(DriverError::UnsupportedMemory);
    }
    let layer = &desc.layers[0];
    if layer.num_planes != 2
        || layer.drm_format != va::VA_FOURCC_NV12
        || layer.object_index[0] != 0
        || layer.object_index[1] != 0
        || layer.pitch[0] != desc.width
        || layer.pitch[1] != desc.width
    {
        return Err(DriverError::UnsupportedMemory);
    }
    let y_size = u64::from(desc.width) * u64::from(desc.height);
    let uv_offset = u64::from(layer.offset[0]) + y_size;
    let end = uv_offset + y_size / 2;
    if u64::from(layer.offset[1]) != uv_offset || end > u64::from(obj.size) {
        return Err(DriverError::InvalidParameter);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_desc() -> va::VADRMPRIMESurfaceDescriptor {
        // SAFETY: POD layout — all-zero is a valid bit pattern for every
        // field (u32/i32/u64). We mutate required fields below.
        let mut d: va::VADRMPRIMESurfaceDescriptor = unsafe { core::mem::zeroed() };
        d.fourcc = va::VA_FOURCC_NV12;
        d.width = 1280;
        d.height = 720;
        d.num_objects = 1;
        d.num_layers = 1;
        d.objects[0].fd = 100; // fake but non-negative
        d.objects[0].size = 1280 * 720 * 3 / 2;
        d.objects[0].drm_format_modifier = 0;
        d.layers[0].drm_format = va::VA_FOURCC_NV12;
        d.layers[0].num_planes = 2;
        d.layers[0].pitch[0] = d.width;
        d.layers[0].pitch[1] = d.width;
        d.layers[0].offset[1] = d.width * d.height;
        d
    }

    #[test]
    fn accepts_valid_single_plane_nv12() {
        assert!(validate_descriptor(&fresh_desc()).is_ok());
    }

    #[test]
    fn rejects_non_nv12() {
        let mut d = fresh_desc();
        d.fourcc = u32::from_le_bytes(*b"RGBA");
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::UnsupportedRtFormat)
        ));
    }

    #[test]
    fn rejects_zero_objects() {
        let mut d = fresh_desc();
        d.num_objects = 0;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::InvalidParameter)
        ));
    }

    #[test]
    fn rejects_zero_layers() {
        let mut d = fresh_desc();
        d.num_layers = 0;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::InvalidParameter)
        ));
    }

    #[test]
    fn rejects_zero_dimensions() {
        let mut d = fresh_desc();
        d.width = 0;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::InvalidParameter)
        ));
    }

    #[test]
    fn rejects_zero_size_object() {
        let mut d = fresh_desc();
        d.objects[0].size = 0;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::InvalidParameter)
        ));
    }

    #[test]
    fn rejects_negative_fd() {
        let mut d = fresh_desc();
        d.objects[0].fd = -1;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::InvalidParameter)
        ));
    }

    #[test]
    fn rejects_tiling_padding_and_out_of_bounds_planes() {
        let mut d = fresh_desc();
        d.objects[0].drm_format_modifier = 1;
        assert!(validate_descriptor(&d).is_err());
        let mut d = fresh_desc();
        d.layers[0].pitch[0] += 128;
        assert!(validate_descriptor(&d).is_err());
        let mut d = fresh_desc();
        d.layers[0].offset[1] += 1;
        assert!(validate_descriptor(&d).is_err());
        let mut d = fresh_desc();
        d.objects[0].size -= 1;
        assert!(validate_descriptor(&d).is_err());
        let mut d = fresh_desc();
        d.num_layers = u32::MAX;
        assert!(validate_descriptor(&d).is_err());
    }

    #[test]
    fn rejects_multi_object_for_now() {
        let mut d = fresh_desc();
        d.num_objects = 2;
        assert!(matches!(
            validate_descriptor(&d),
            Err(DriverError::UnsupportedMemory)
        ));
    }
}
