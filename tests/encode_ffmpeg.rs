//! c.3 smoke encode test — runs real ffmpeg h264_vaapi encode through our
//! driver end-to-end.
//!
//! Instead of reimplementing a VAAPI client in Rust, we use `ffmpeg` as a
//! subprocess: set `LIBVA_DRIVERS_PATH` + `LIBVA_DRIVER_NAME=nvidia_nvenc` so
//! libva dlopens our built driver, then `ffmpeg -c:v h264_vaapi` drives the
//! whole encode cycle (Config/Surfaces/Context/Buffers/Begin/Render/End/Sync)
//! and writes a valid H.264 file. A second subprocess — `ffprobe` — parses
//! metadata and confirms codec/profile/dimensions/frame-count.
//!
//! Ignored by default (requires live NVIDIA GPU + ffmpeg h264_vaapi encoder).
//! Run via:
//!   cargo test --test encode_ffmpeg --release -- --ignored --nocapture

mod common;

use serde_json::Value;
use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires live NVIDIA GPU and ffmpeg h264_vaapi"]
fn smoke_encode_h264_720p60() {
    if !prereqs_ok() {
        return;
    }

    // Ensure `nvidia_nvenc_drv_video.so` is present (symlink next to the real
    // artifact so libva can resolve `LIBVA_DRIVER_NAME=nvidia_nvenc`).
    let _link = common::ensure_libva_symlink();
    let drivers_path = common::drivers_path();

    let out_h264 = std::env::temp_dir()
        .join(format!("nvenc_smoke_720p60_{}.h264", std::process::id()));
    let _ = std::fs::remove_file(&out_h264);

    let output = Command::new("ffmpeg")
        .env("LIBVA_DRIVERS_PATH", &drivers_path)
        .env("LIBVA_DRIVER_NAME", "nvidia_nvenc")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=60:duration=1", // 60 frames
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-profile:v",
            "main",
            "-b:v",
            "5M",
            "-g",
            "30",
            "-f",
            "h264",
            out_h264.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg spawn failed");

    assert!(
        output.status.success(),
        "ffmpeg failed (exit={:?}):\n--- stderr ---\n{}\n--- stdout ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let size = std::fs::metadata(&out_h264)
        .expect("out.h264 missing")
        .len();
    assert!(size > 1024, "output suspiciously small: {} bytes", size);

    assert_h264_stream(&out_h264, 1280, 720, 50);

    let _ = std::fs::remove_file(&out_h264);
}

#[test]
#[ignore = "requires live NVIDIA GPU and ffmpeg h264_vaapi"]
fn smoke_encode_h264_baseline_480p30() {
    if !prereqs_ok() {
        return;
    }

    let _link = common::ensure_libva_symlink();
    let drivers_path = common::drivers_path();

    let out_h264 = std::env::temp_dir()
        .join(format!("nvenc_smoke_480p30_{}.h264", std::process::id()));
    let _ = std::fs::remove_file(&out_h264);

    let output = Command::new("ffmpeg")
        .env("LIBVA_DRIVERS_PATH", &drivers_path)
        .env("LIBVA_DRIVER_NAME", "nvidia_nvenc")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=854x480:rate=30:duration=1", // 30 frames
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-profile:v",
            "constrained_baseline",
            "-b:v",
            "2M",
            "-g",
            "30",
            "-f",
            "h264",
            out_h264.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg spawn failed");

    assert!(
        output.status.success(),
        "ffmpeg failed (exit={:?}):\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    let size = std::fs::metadata(&out_h264)
        .expect("out.h264 missing")
        .len();
    assert!(size > 256, "output suspiciously small: {} bytes", size);

    assert_h264_stream(&out_h264, 854, 480, 25);

    let _ = std::fs::remove_file(&out_h264);
}

/// Shared ffprobe assertion: runs `ffprobe -of json` and validates the video
/// stream metadata. `min_frames` is a permissive floor (ffprobe's
/// `nb_read_frames` may drop a couple of frames on trailing I/O).
fn assert_h264_stream(path: &Path, expected_w: u64, expected_h: u64, min_frames: u64) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile,coded_width,coded_height,pix_fmt,nb_read_frames",
            "-of",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe spawn failed");

    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("invalid ffprobe JSON");
    let stream = &json["streams"][0];

    assert_eq!(stream["codec_name"], "h264", "wrong codec: {}", stream);

    let profile = stream["profile"].as_str().unwrap_or("");
    assert!(
        matches!(profile, "Main" | "Constrained Baseline" | "Baseline" | "High"),
        "unexpected profile: {profile:?} (full stream: {stream})"
    );

    // `coded_width`/`coded_height` are ints in ffprobe JSON.
    assert_eq!(
        stream["coded_width"].as_u64(),
        Some(expected_w),
        "wrong width (stream: {stream})"
    );
    assert_eq!(
        stream["coded_height"].as_u64(),
        Some(expected_h),
        "wrong height (stream: {stream})"
    );

    // `nb_read_frames` is a string in ffprobe JSON.
    let nb_frames: u64 = stream["nb_read_frames"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        nb_frames >= min_frames,
        "too few frames decoded: {nb_frames} (min={min_frames})"
    );

    // NV12 input → yuv420p after decode; accept both just to be safe across
    // ffmpeg versions.
    let pix = stream["pix_fmt"].as_str().unwrap_or("");
    assert!(
        matches!(pix, "yuv420p" | "nv12"),
        "unexpected pix_fmt: {pix:?}"
    );
}

// ---------------------------------------------------------------------------
// Prerequisite probes (soft-skip with eprintln! — *not* panic — so that CI
// machines without a GPU don't fail when someone forgets `--ignored` gate).
// ---------------------------------------------------------------------------

fn prereqs_ok() -> bool {
    if !has_nvidia_gpu() {
        eprintln!("skip: nvidia-smi missing or no NVIDIA GPU detected");
        return false;
    }
    if !has_ffmpeg_vaapi() {
        eprintln!("skip: ffmpeg h264_vaapi encoder not available");
        return false;
    }
    if !has_render_node() {
        eprintln!("skip: /dev/dri/renderD128 missing");
        return false;
    }
    if !has_ffprobe() {
        eprintln!("skip: ffprobe not on PATH");
        return false;
    }
    true
}

fn has_nvidia_gpu() -> bool {
    Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

fn has_ffmpeg_vaapi() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("h264_vaapi")
        })
        .unwrap_or(false)
}

fn has_ffprobe() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_render_node() -> bool {
    Path::new("/dev/dri/renderD128").exists()
}
