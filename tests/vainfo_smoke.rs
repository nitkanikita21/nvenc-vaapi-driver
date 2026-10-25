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

    let binary = std::env::var_os("NVENC_TEST_VAINFO").unwrap_or_else(|| "vainfo".into());
    Command::new(binary)
        .env("LIBVA_DRIVERS_PATH", &drivers_path)
        .env("LIBVA_DRIVER_NAME", "nvidia_nvenc")
        .arg("--display")
        .arg("drm")
        .arg("--device")
        .arg(common::render_node())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("VAProfileH264Main")
                .and(predicate::str::contains("VAEntrypointEncSlice")),
        );
}
