//! CUDA Driver API adapter (via `cudarc` with `dynamic-loading`).
//!
//! Prefer the explicit NVENC_VAAPI_DEVICE PCI address, then match the DRM
//! device. An unmatched display can use the sole CUDA device; ambiguous
//! multi-NVIDIA configurations require an explicit choice.
//!
//! The full `cudarc::driver::CudaContext` is built lazily. Touching
//! `libcuda.so.1` (even via `dynamic-loading`) on a machine without the
//! NVIDIA driver would abort here; keeping `CudaCtx` always-constructible
//! means `vaInitialize` (and hence `vainfo`) can still succeed up to the
//! first context-requiring call.

#![allow(dead_code)]

pub mod external_mem;

use crate::error::DriverError;
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};
use va_sys as va;

pub struct CudaCtx {
    /// DRM fd from `ctx->drm_state` (dup'd so we outlive the caller's use).
    pub drm_fd: Option<libc::c_int>,
    /// Lazily-initialised CUDA driver context. `OnceLock` gives us
    /// thread-safe one-shot init without blocking the common `.get()` path.
    ctx: OnceLock<Arc<cudarc::driver::CudaContext>>,
    /// Serialises the (rare) racing initialisation attempts so we do not
    /// leak a second `cuCtxCreate` result.
    init_lock: Mutex<()>,
}

impl CudaCtx {
    pub fn from_va_context(ctx: va::VADriverContextP) -> Result<Self, DriverError> {
        if ctx.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        // SAFETY: ctx non-null per check above.
        let (display_type, drm_state_p) = unsafe {
            (
                (*ctx).display_type,
                (*ctx).drm_state as *const va::drm_state,
            )
        };
        // `drm_state` is only a valid `struct drm_state` pointer when libva
        // was opened via `vaGetDisplayDRM`. For X11/Wayland/Android/other
        // display backends the field is either NULL or points at an opaque
        // struct with a completely different layout (e.g. OBS's QSV plugin
        // probes every VAAPI driver on startup with its own opaque state).
        // Blindly dereferencing those as `drm_state.fd` reads garbage and in
        // the worst case crashes with SIGSEGV on unmapped addresses — exactly
        // what happened when OBS's `obs-qsv11` module triggered `vaInitialize`
        // through our driver. Gate the read on `display_type`.
        let is_drm = display_type == va::VA_DISPLAY_DRM as libc::c_ulong
            || display_type == va::VA_DISPLAY_DRM_RENDERNODES as libc::c_ulong;
        let drm_fd = if !is_drm || drm_state_p.is_null() {
            None
        } else {
            // SAFETY: `display_type` promises this is a `struct drm_state`.
            let fd = unsafe { (*drm_state_p).fd };
            if fd < 0 {
                None
            } else {
                // SAFETY: valid open fd.
                let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                if dup < 0 { None } else { Some(dup) }
            }
        };
        Ok(Self {
            drm_fd,
            ctx: OnceLock::new(),
            init_lock: Mutex::new(()),
        })
    }

    /// Return the lazily-initialised CUDA context, creating it on first use.
    pub fn get_or_init(&self) -> Result<Arc<cudarc::driver::CudaContext>, DriverError> {
        if let Some(c) = self.ctx.get() {
            return Ok(c.clone());
        }
        let _g = self.init_lock.lock();
        if let Some(c) = self.ctx.get() {
            return Ok(c.clone());
        }
        let ordinal = self.pick_device_ordinal()?;
        let c = cudarc::driver::CudaContext::new(ordinal)
            .map_err(|_| DriverError::OperationFailed("cuCtxCreate failed"))?;
        let _ = self.ctx.set(c.clone());
        Ok(c)
    }

    /// Resolve a CUDA device ordinal from the DRM fd via sysfs.
    ///
    /// Follows `/sys/dev/char/<major>:<minor>/device` to the PCI bus id
    /// (`DDDD:BB:DD.F`) and calls `cuDeviceGetByPCIBusId`.
    fn pick_device_ordinal(&self) -> Result<usize, DriverError> {
        // A PCI address remains stable when DRM/CUDA enumeration changes.
        if let Some(bus_id) = std::env::var_os("NVENC_VAAPI_DEVICE") {
            let bus_id = bus_id.to_str().ok_or(DriverError::InvalidParameter)?;
            return Self::cuda_ordinal_for_pci(bus_id);
        }
        if let Some(bus_id) = self.drm_pci_address() {
            if let Ok(ordinal) = Self::cuda_ordinal_for_pci(&bus_id) {
                return Ok(ordinal);
            }
        }
        use cudarc::driver::sys::{CUresult, cuDeviceGetCount, cuInit};
        let mut count = 0;
        // SAFETY: idempotent initialisation; count is a writable local.
        if unsafe { cuInit(0) } != CUresult::CUDA_SUCCESS
            || unsafe { cuDeviceGetCount(&mut count) } != CUresult::CUDA_SUCCESS
        {
            return Err(DriverError::OperationFailed("CUDA device enumeration"));
        }
        select_single_device(count)
    }

    fn drm_pci_address(&self) -> Option<String> {
        let fd = self.drm_fd?;
        // SAFETY: fstat writes into an owned local; fd is open.
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return None;
        }
        let (major, minor) = (libc::major(st.st_rdev), libc::minor(st.st_rdev));
        let link = format!("/sys/dev/char/{}:{}/device", major, minor);
        let resolved = std::fs::read_link(&link).ok()?;
        Some(resolved.file_name()?.to_str()?.to_owned())
    }

    fn cuda_ordinal_for_pci(bus_id: &str) -> Result<usize, DriverError> {
        use cudarc::driver::sys::{CUdevice, cuDeviceGetByPCIBusId, cuInit};
        use std::ffi::CString;
        // SAFETY: cuInit is idempotent; dynamic-loading resolves on first call.
        let r = unsafe { cuInit(0) };
        if r != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DriverError::OperationFailed("cuInit"));
        }
        let c = CString::new(bus_id).map_err(|_| DriverError::OperationFailed("bad pci id"))?;
        let mut dev: CUdevice = 0;
        // SAFETY: FFI call, dev is valid for writes.
        let r = unsafe { cuDeviceGetByPCIBusId(&mut dev, c.as_ptr()) };
        if r != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DriverError::OperationFailed("cuDeviceGetByPCIBusId"));
        }
        Ok(dev as usize)
    }
}

fn select_single_device(count: i32) -> Result<usize, DriverError> {
    if count == 1 {
        Ok(0)
    } else {
        Err(DriverError::OperationFailed(
            "no unambiguous NVIDIA device; set NVENC_VAAPI_DEVICE to a PCI address",
        ))
    }
}

impl Drop for CudaCtx {
    fn drop(&mut self) {
        if let Some(fd) = self.drm_fd.take() {
            // SAFETY: we own the dup'd fd.
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_requires_exactly_one_cuda_device() {
        assert_eq!(select_single_device(1).unwrap(), 0);
        for count in [-1, 0, 2, 8] {
            assert!(select_single_device(count).is_err());
        }
    }
}
