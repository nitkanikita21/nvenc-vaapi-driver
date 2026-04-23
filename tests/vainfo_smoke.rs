//! End-to-end smoke test: run the system `vainfo` against our freshly-built
//! driver and check that H.264 Main + EncSlice show up in its output.
//!
//! This test is `#[ignore]` by default. Run explicitly on machines with:
//!   * a working libva runtime (`vainfo` binary),
//!   * a NVIDIA GPU + kernel module + /dev/dri/renderD128,
//!   * the LIBVA_DRIVER_NAME -> libnvidia_nvenc_drv_video.so symlink in place.
//!
//! Command to invoke:
//!   cargo test --release --test vainfo_smoke -- --ignored --nocapture

mod common;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
#[ignore = "requires system vainfo + NVIDIA DRM render node"]
fn vainfo_lists_h264_main_encslice() {
    // Build the .so and make sure the `nvidia_nvenc_drv_video.so` symlink
    // exists next to it so libva's `LIBVA_DRIVER_NAME=nvidia_nvenc` path
    // resolves.
    let _link = common::ensure_libva_symlink();
    let drivers_path = common::drivers_path();

    // vainfo may not be present on CI — fail soft.
    if !has_vainfo() {
        eprintln!("vainfo not found on PATH; skipping end-to-end smoke");
        return;
    }

    Command::new("vainfo")
        .env("LIBVA_DRIVERS_PATH", &drivers_path)
        .env("LIBVA_DRIVER_NAME", "nvidia_nvenc")
        .arg("--display")
        .arg("drm")
        .arg("--device")
        .arg("/dev/dri/renderD128")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("VAProfileH264Main")
                .and(predicate::str::contains("VAEntrypointEncSlice")),
        );
}

fn has_vainfo() -> bool {
    let Ok(path) = std::env::var("PATH") else { return false; };
    for p in path.split(':') {
        let candidate = std::path::Path::new(p).join("vainfo");
        if candidate.is_file() {
            return true;
        }
    }
    false
}
