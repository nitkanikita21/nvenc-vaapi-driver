//! Shared helpers for integration tests.
//!
//! The driver ships as a `cdylib`, so integration tests cannot link against
//! its Rust API — they exercise it *the same way libva does*: via `dlopen`
//! on the built `.so`. This module builds the .so once per `cargo test`
//! process (cached behind `OnceCell`) and returns its absolute path.

#![allow(dead_code)]

use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::process::Command;

static DRIVER_PATH: OnceCell<PathBuf> = OnceCell::new();

/// Build the cdylib in release mode (matches CI) and return the path to
/// `libnvidia_nvenc_drv_video.so`. Panics (fails the test) if the build
/// fails or the artifact is missing.
///
/// Cargo itself parallelises our calls safely — if we're already inside
/// `cargo test`, re-invoking `cargo build` from a test reuses the same
/// target dir without re-linking unnecessary rebuilds.
pub fn build_driver() -> PathBuf {
    DRIVER_PATH
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

            let status = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--release",
                    "--lib",
                    "-p",
                    "libvaapi-rust-nvenc",
                ])
                .current_dir(&manifest_dir)
                .status()
                .expect("failed to spawn `cargo build` for driver");
            assert!(status.success(), "cargo build --release failed");

            let so = manifest_dir
                .join("target")
                .join("release")
                .join("libnvidia_nvenc_drv_video.so");
            assert!(
                so.exists(),
                "expected driver .so at {}, but it does not exist",
                so.display()
            );
            so
        })
        .clone()
}

/// Directory containing the built driver — useful as `LIBVA_DRIVERS_PATH`.
pub fn drivers_path() -> PathBuf {
    let mut p = build_driver();
    p.pop();
    p
}

/// Ensure the `nvidia_nvenc_drv_video.so` symlink (the name libva resolves
/// from `LIBVA_DRIVER_NAME=nvidia_nvenc`) exists next to the built artifact.
/// Idempotent; tolerates a pre-existing symlink or file.
pub fn ensure_libva_symlink() -> PathBuf {
    let real = build_driver();
    let dir = drivers_path();
    let link = dir.join("nvidia_nvenc_drv_video.so");
    if link.exists() {
        return link;
    }
    // best-effort: create a relative symlink
    let _ = std::os::unix::fs::symlink(
        real.file_name().expect("driver file has no name"),
        &link,
    );
    link
}
