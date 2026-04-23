//! CUDA Driver API adapter (via `cudarc` with `dynamic-loading`).
//!
//! The driver picks a CUDA device based on the PCI bus id resolved from the
//! libva-provided DRM fd. If anything in that chain fails (no DRM fd, sysfs
//! read error, `cuDeviceGetByPCIBusId` miss), we fall back to device 0 —
//! which on single-GPU NVIDIA systems is almost always correct.
//!
//! The full `cudarc::driver::CudaContext` is built lazily. Touching
//! `libcuda.so.1` (even via `dynamic-loading`) on a machine without the
//! NVIDIA driver would abort here; keeping `CudaCtx` always-constructible
//! means `vaInitialize` (and hence `vainfo`) can still succeed up to the
//! first context-requiring call.

#![allow(dead_code)]

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
            ((*ctx).display_type, (*ctx).drm_state as *const va::drm_state)
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
        let ordinal = self.pick_device_ordinal();
        let c = cudarc::driver::CudaContext::new(ordinal)
            .map_err(|_| DriverError::OperationFailed("cuCtxCreate failed"))?;
        let _ = self.ctx.set(c.clone());
        Ok(c)
    }

    /// Resolve a CUDA device ordinal from the DRM fd via sysfs.
    ///
    /// Follows `/sys/dev/char/<major>:<minor>/device` to the PCI bus id
    /// (`DDDD:BB:DD.F`) and calls `cuDeviceGetByPCIBusId`. Any failure
    /// falls back to ordinal 0.
    fn pick_device_ordinal(&self) -> usize {
        let Some(fd) = self.drm_fd else { return 0 };
        // SAFETY: fstat writes into an owned local; fd is open.
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return 0;
        }
        let (major, minor) = (libc::major(st.st_rdev), libc::minor(st.st_rdev));
        let link = format!("/sys/dev/char/{}:{}/device", major, minor);
        let Ok(resolved) = std::fs::read_link(&link) else { return 0 };
        let Some(bus_id) = resolved.file_name().and_then(|s| s.to_str()) else {
            return 0;
        };
        Self::cuda_ordinal_for_pci(bus_id).unwrap_or(0)
    }

    fn cuda_ordinal_for_pci(bus_id: &str) -> Result<usize, DriverError> {
        use cudarc::driver::sys::{CUdevice, cuDeviceGetByPCIBusId, cuInit};
        use std::ffi::CString;
        // SAFETY: cuInit is idempotent; dynamic-loading resolves on first call.
        let r = unsafe { cuInit(0) };
        if r != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DriverError::OperationFailed("cuInit"));
        }
        let c = CString::new(bus_id)
            .map_err(|_| DriverError::OperationFailed("bad pci id"))?;
        let mut dev: CUdevice = 0;
        // SAFETY: FFI call, dev is valid for writes.
        let r = unsafe { cuDeviceGetByPCIBusId(&mut dev, c.as_ptr()) };
        if r != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DriverError::OperationFailed("cuDeviceGetByPCIBusId"));
        }
        Ok(dev as usize)
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
