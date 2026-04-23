//! Smoke test for the VAAPI driver-init ABI boundary.
//!
//! We `dlopen` the built `libnvidia_nvenc_drv_video.so` exactly the way
//! libva does, look up `__vaDriverInit_1_23`, and drive it with a
//! minimally-populated `VADriverContext`. Then we poke the vtable the same
//! way `vainfo` / Chromium would — query profiles, query entrypoints,
//! create+destroy a config, call `vaTerminate`.
//!
//! The goal is not to exercise NVENC at all: we verify that the Rust driver
//! crate correctly populates the libva ABI surface and handles lifecycle
//! without leaking or panicking across FFI.

mod common;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr;

use libloading::{Library, Symbol};
use va_sys as va;

type VaDriverInit =
    unsafe extern "C" fn(ctx: *mut va::VADriverContext) -> va::VAStatus;

// libva's `struct drm_state` starts with `int fd`; the rest is irrelevant
// for our CudaCtx stub (it just reads the first int and dups it).
#[repr(C)]
struct DrmStateStub {
    fd: c_int,
    auth_type: c_int,
    // pad so we don't accidentally alias into someone else's memory if the
    // driver decides to read further. The real struct is ~40 bytes.
    _pad: [u8; 128],
}

/// Guard that owns a loaded driver and a live `VADriverContext`. Calls
/// `vaTerminate` on drop so the test cannot leak `DriverState`.
struct DriverFixture {
    lib: Library,
    ctx: Box<va::VADriverContext>,
    vtable: Box<va::VADriverVTable>,
    _drm: Box<DrmStateStub>,
}

impl DriverFixture {
    fn new() -> Self {
        let so = common::build_driver();
        // SAFETY: loading our own just-built cdylib; the symbols are
        // `extern "C"` with known signatures.
        let lib = unsafe { Library::new(&so) }.expect("dlopen driver .so");

        // libva zero-initialises the vtable with calloc(1, sizeof(...)).
        // We must mirror that so optional fn pointers default to None.
        let vtable: Box<va::VADriverVTable> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });

        let drm = Box::new(DrmStateStub {
            fd: -1, // CudaCtx::from_va_context handles fd < 0 as "no dup".
            auth_type: 0,
            _pad: [0; 128],
        });

        // Most VADriverContext pointer fields are optional; zeroing is fine.
        let ctx: Box<va::VADriverContext> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
        let mut f = DriverFixture { lib, ctx, vtable, _drm: drm };
        f.ctx.vtable = f.vtable.as_mut() as *mut va::VADriverVTable;
        f.ctx.drm_state = f._drm.as_mut() as *mut _ as *mut c_void;
        f.ctx.info_callback = Some(info_cb);
        f.ctx.error_callback = Some(error_cb);
        f
    }

    fn init(&mut self) -> va::VAStatus {
        let sym: Symbol<VaDriverInit> = unsafe {
            self.lib
                .get(b"__vaDriverInit_1_23\0")
                .expect("__vaDriverInit_1_23 missing from driver")
        };
        unsafe { sym(self.ctx.as_mut() as *mut _) }
    }
}

impl Drop for DriverFixture {
    fn drop(&mut self) {
        // Call vaTerminate if it was installed — guarantees DriverState is
        // freed even if a test assertion fires mid-way.
        if let Some(term) = self.vtable.vaTerminate {
            unsafe {
                let _ = term(self.ctx.as_mut() as *mut _);
            }
        }
    }
}

unsafe extern "C" fn info_cb(_ctx: *mut va::VADriverContext, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(msg) };
    eprintln!("[driver info] {}", s.to_string_lossy());
}

unsafe extern "C" fn error_cb(_ctx: *mut va::VADriverContext, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(msg) };
    eprintln!("[driver error] {}", s.to_string_lossy());
}

#[test]
fn driver_init_returns_success_and_populates_vtable() {
    let mut f = DriverFixture::new();
    let status = f.init();
    assert_eq!(status, 0, "__vaDriverInit_1_23 should return VA_STATUS_SUCCESS");

    // After init, pDriverData must be non-null (DriverState box was installed).
    assert!(
        !f.ctx.pDriverData.is_null(),
        "pDriverData must be set after driver_init"
    );

    // Vendor string was set and is a valid C string.
    assert!(!f.ctx.str_vendor.is_null(), "str_vendor must be set");
    let vendor = unsafe { CStr::from_ptr(f.ctx.str_vendor) }
        .to_string_lossy()
        .into_owned();
    assert!(
        vendor.contains("nvidia_nvenc"),
        "vendor string should identify us, got: {:?}",
        vendor
    );

    // Version fields reflect the libva version we were built against.
    assert_eq!(f.ctx.version_major as u32, va::VA_MAJOR_VERSION);
    assert_eq!(f.ctx.version_minor as u32, va::VA_MINOR_VERSION);

    // A handful of load-bearing vtable slots must be populated.
    assert!(f.vtable.vaTerminate.is_some(), "vaTerminate missing");
    assert!(
        f.vtable.vaQueryConfigProfiles.is_some(),
        "vaQueryConfigProfiles missing"
    );
    assert!(
        f.vtable.vaQueryConfigEntrypoints.is_some(),
        "vaQueryConfigEntrypoints missing"
    );
    assert!(f.vtable.vaCreateConfig.is_some(), "vaCreateConfig missing");
    assert!(f.vtable.vaDestroyConfig.is_some(), "vaDestroyConfig missing");
    assert!(
        f.vtable.vaGetConfigAttributes.is_some(),
        "vaGetConfigAttributes missing"
    );
    assert!(
        f.vtable.vaCreateSurfaces2.is_some(),
        "vaCreateSurfaces2 missing"
    );
}

#[test]
fn query_config_profiles_returns_h264_baseline_and_main() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);

    let mut profiles: [va::VAProfile; 16] = [0; 16];
    let mut n: c_int = 0;
    let fn_ptr = f
        .vtable
        .vaQueryConfigProfiles
        .expect("vaQueryConfigProfiles not installed");
    let status = unsafe {
        fn_ptr(f.ctx.as_mut() as *mut _, profiles.as_mut_ptr(), &mut n)
    };
    assert_eq!(status, 0, "vaQueryConfigProfiles must succeed");
    assert_eq!(n, 3, "driver advertises three H.264 profiles");
    let set: Vec<va::VAProfile> = profiles[..n as usize].to_vec();
    assert!(
        set.contains(&va::VAProfileH264ConstrainedBaseline),
        "must include H264ConstrainedBaseline, got {:?}",
        set
    );
    assert!(
        set.contains(&va::VAProfileH264High),
        "must include H264High, got {:?}",
        set
    );
    assert!(
        set.contains(&va::VAProfileH264Main),
        "must include H264Main, got {:?}",
        set
    );
}

#[test]
fn query_config_entrypoints_h264_main_returns_encslice() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);

    let mut eps: [va::VAEntrypoint; 8] = [0; 8];
    let mut n: c_int = 0;
    let fn_ptr = f
        .vtable
        .vaQueryConfigEntrypoints
        .expect("vaQueryConfigEntrypoints not installed");
    let status = unsafe {
        fn_ptr(
            f.ctx.as_mut() as *mut _,
            va::VAProfileH264Main,
            eps.as_mut_ptr(),
            &mut n,
        )
    };
    assert_eq!(status, 0, "entrypoint query should succeed");
    assert_eq!(n, 1, "exactly one entrypoint is advertised");
    assert_eq!(eps[0], va::VAEntrypointEncSlice);
}

#[test]
fn query_entrypoints_for_unsupported_profile_fails_cleanly() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);

    let mut eps: [va::VAEntrypoint; 8] = [0; 8];
    let mut n: c_int = 0;
    let fn_ptr = f.vtable.vaQueryConfigEntrypoints.unwrap();
    // VAProfileNone / unrelated profile value.
    let bogus: va::VAProfile = 9999;
    let status = unsafe {
        fn_ptr(f.ctx.as_mut() as *mut _, bogus, eps.as_mut_ptr(), &mut n)
    };
    assert_ne!(status, 0, "unsupported profile must produce an error status");
    // Don't assert the exact code — the driver is free to map it to
    // UNSUPPORTED_PROFILE or INVALID_PARAMETER; we just want a non-success
    // with no crash.
}

#[test]
fn create_then_destroy_config_roundtrips() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);

    let create = f.vtable.vaCreateConfig.unwrap();
    let destroy = f.vtable.vaDestroyConfig.unwrap();

    // Minimal attrib list: RT format YUV420.
    let mut attribs = [va::VAConfigAttrib {
        type_: va::VAConfigAttribRTFormat,
        value: va::VA_RT_FORMAT_YUV420,
    }];
    let mut config_id: va::VAConfigID = 0;
    let s = unsafe {
        create(
            f.ctx.as_mut() as *mut _,
            va::VAProfileH264Main,
            va::VAEntrypointEncSlice,
            attribs.as_mut_ptr(),
            attribs.len() as c_int,
            &mut config_id,
        )
    };
    assert_eq!(s, 0, "vaCreateConfig should succeed");
    assert_ne!(config_id, 0, "a valid VAConfigID must be non-zero");

    let s = unsafe { destroy(f.ctx.as_mut() as *mut _, config_id) };
    assert_eq!(s, 0, "vaDestroyConfig should succeed");

    // Destroying the same ID again must fail (InvalidConfig).
    let s = unsafe { destroy(f.ctx.as_mut() as *mut _, config_id) };
    assert_ne!(s, 0, "second destroy should fail");
}

#[test]
fn create_config_rejects_bogus_profile() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);

    let create = f.vtable.vaCreateConfig.unwrap();
    let mut config_id: va::VAConfigID = 0;
    let s = unsafe {
        create(
            f.ctx.as_mut() as *mut _,
            99999 as va::VAProfile,
            va::VAEntrypointEncSlice,
            ptr::null_mut(),
            0,
            &mut config_id,
        )
    };
    assert_ne!(s, 0, "bogus profile must be rejected");
}

#[test]
fn terminate_clears_driver_data() {
    let mut f = DriverFixture::new();
    assert_eq!(f.init(), 0);
    assert!(!f.ctx.pDriverData.is_null());

    let term = f.vtable.vaTerminate.unwrap();
    let s = unsafe { term(f.ctx.as_mut() as *mut _) };
    assert_eq!(s, 0, "vaTerminate should succeed");
    assert!(
        f.ctx.pDriverData.is_null(),
        "vaTerminate must null out pDriverData"
    );

    // Drop will call vaTerminate again. It must not crash on an already-
    // cleared context (idempotent).
}

#[test]
fn driver_init_rejects_null_ctx() {
    let so = common::build_driver();
    let lib = unsafe { Library::new(&so) }.expect("dlopen driver");
    let sym: Symbol<VaDriverInit> =
        unsafe { lib.get(b"__vaDriverInit_1_23\0") }.expect("symbol present");
    let status = unsafe { sym(ptr::null_mut()) };
    assert_ne!(
        status, 0,
        "driver_init(NULL) must not return success — should be a VA error"
    );
}

#[test]
fn all_fallback_init_symbols_are_exported() {
    // libva tries `__vaDriverInit_1_<minor>` newest-first; missing a minor
    // that matches the runtime libva results in a "no driver" error from
    // libva's loader. We export the full 0..=23 range — verify they resolve.
    let so = common::build_driver();
    let lib = unsafe { Library::new(&so) }.expect("dlopen driver");
    for minor in 0..=23u32 {
        let name = format!("__vaDriverInit_1_{minor}\0");
        let _sym: Symbol<VaDriverInit> = unsafe { lib.get(name.as_bytes()) }
            .unwrap_or_else(|e| panic!("symbol {} missing: {e}", &name[..name.len() - 1]));
    }
}
