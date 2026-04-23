//! Verify that deliberately hostile arguments to vtable entries never cross
//! the FFI boundary as an unwinding panic. Every `extern "C"` driver entry
//! is wrapped in `catch_unwind` (see `driver/mod.rs::guard`), so nonsense
//! input must result in a non-zero `VAStatus` rather than tearing down the
//! process.
//!
//! Running this test with `panic = "abort"` in the profile would make the
//! whole suite useless; the root `Cargo.toml` sets `panic = "unwind"` on
//! release precisely so `catch_unwind` works.

mod common;

use std::ffi::{c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr;

use libloading::{Library, Symbol};
use va_sys as va;

type VaDriverInit = unsafe extern "C" fn(*mut va::VADriverContext) -> va::VAStatus;

#[repr(C)]
struct DrmStateStub {
    fd: c_int,
    auth_type: c_int,
    _pad: [u8; 128],
}

struct Loaded {
    // Keep the Library alive for the duration of the test. Function
    // pointers we stored in `vtable` point into this .so; dropping it
    // early would invalidate them.
    #[allow(dead_code)]
    lib: Library,
    ctx: Box<va::VADriverContext>,
    vtable: Box<va::VADriverVTable>,
    _drm: Box<DrmStateStub>,
}

fn load_and_init() -> Loaded {
    let so = common::build_driver();
    let lib = unsafe { Library::new(&so) }.expect("dlopen driver");
    let mut vtable: Box<va::VADriverVTable> =
        Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
    let mut drm = Box::new(DrmStateStub {
        fd: -1,
        auth_type: 0,
        _pad: [0; 128],
    });
    let mut ctx: Box<va::VADriverContext> =
        Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
    ctx.vtable = vtable.as_mut() as *mut _;
    ctx.drm_state = drm.as_mut() as *mut _ as *mut c_void;

    let init: Symbol<VaDriverInit> =
        unsafe { lib.get(b"__vaDriverInit_1_23\0") }.unwrap();
    let s = unsafe { init(ctx.as_mut() as *mut _) };
    assert_eq!(s, 0, "init failed: {s}");

    Loaded { lib, ctx, vtable, _drm: drm }
}

impl Drop for Loaded {
    fn drop(&mut self) {
        if let Some(t) = self.vtable.vaTerminate {
            unsafe { let _ = t(self.ctx.as_mut() as *mut _); }
        }
    }
}

#[test]
fn query_config_profiles_with_null_out_ptrs_returns_error_without_crashing() {
    let mut l = load_and_init();
    let q = l.vtable.vaQueryConfigProfiles.unwrap();
    let status = unsafe { q(l.ctx.as_mut() as *mut _, ptr::null_mut(), ptr::null_mut()) };
    assert_ne!(status, 0, "null out-pointers must produce an error, not success");
}

#[test]
fn destroy_config_with_bogus_id_returns_error() {
    let mut l = load_and_init();
    let destroy = l.vtable.vaDestroyConfig.unwrap();
    // 0xDEADBEEF is very unlikely to match any slot's low-bits id.
    let status = unsafe { destroy(l.ctx.as_mut() as *mut _, 0xDEAD_BEEF as va::VAConfigID) };
    assert_ne!(status, 0, "bogus VAConfigID must produce InvalidConfig error");
}

#[test]
fn create_config_with_null_output_ptr_is_invalid_parameter() {
    let mut l = load_and_init();
    let create = l.vtable.vaCreateConfig.unwrap();
    let status = unsafe {
        create(
            l.ctx.as_mut() as *mut _,
            va::VAProfileH264Main,
            va::VAEntrypointEncSlice,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
        )
    };
    assert_ne!(status, 0);
}

#[test]
fn get_config_attributes_with_zero_count_and_null_list_is_rejected() {
    let mut l = load_and_init();
    let f = l.vtable.vaGetConfigAttributes.unwrap();
    let status = unsafe {
        f(
            l.ctx.as_mut() as *mut _,
            va::VAProfileH264Main,
            va::VAEntrypointEncSlice,
            ptr::null_mut(),
            0,
        )
    };
    assert_ne!(status, 0, "null attrib list must be rejected");
}

#[test]
fn query_entrypoints_for_unknown_profile_does_not_crash() {
    let mut l = load_and_init();
    let q = l.vtable.vaQueryConfigEntrypoints.unwrap();
    let mut buf: [va::VAEntrypoint; 8] = [0; 8];
    let mut n: c_int = 0;
    let s = unsafe {
        q(l.ctx.as_mut() as *mut _, -1 as va::VAProfile, buf.as_mut_ptr(), &mut n)
    };
    assert_ne!(s, 0);
}
