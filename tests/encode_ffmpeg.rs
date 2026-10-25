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
    require_prereqs();

    // Ensure `nvidia_nvenc_drv_video.so` is present (symlink next to the real
    // artifact so libva can resolve `LIBVA_DRIVER_NAME=nvidia_nvenc`).
    let _link = common::ensure_libva_symlink();
    let drivers_path = common::drivers_path();

    let out_h264 =
        std::env::temp_dir().join(format!("nvenc_smoke_720p60_{}.h264", std::process::id()));
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
            common::render_node().to_str().unwrap(),
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

    assert_h264_stream(&out_h264, 1280, 720, 60, "Main");

    let _ = std::fs::remove_file(&out_h264);
}

#[test]
#[ignore = "requires live NVIDIA GPU and ffmpeg h264_vaapi"]
fn smoke_encode_h264_baseline_480p30() {
    require_prereqs();

    let _link = common::ensure_libva_symlink();
    let drivers_path = common::drivers_path();

    let out_h264 =
        std::env::temp_dir().join(format!("nvenc_smoke_480p30_{}.h264", std::process::id()));
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
            common::render_node().to_str().unwrap(),
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

    assert_h264_stream(&out_h264, 854, 480, 30, "Constrained Baseline");

    let _ = std::fs::remove_file(&out_h264);
}

/// Shared ffprobe assertion: runs `ffprobe -of json` and validates the video
/// stream metadata and fully decodes the output. Every submitted frame must survive.
fn assert_h264_stream(
    path: &Path,
    expected_w: u64,
    expected_h: u64,
    expected_frames: u64,
    expected_profile: &str,
) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile,width,height,pix_fmt,nb_read_frames",
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

    let json: Value = serde_json::from_slice(&output.stdout).expect("invalid ffprobe JSON");
    let stream = &json["streams"][0];

    assert_eq!(stream["codec_name"], "h264", "wrong codec: {}", stream);

    assert_eq!(
        stream["profile"], expected_profile,
        "wrong negotiated profile"
    );

    assert!(
        output.stderr.is_empty(),
        "decoder errors: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stream["width"].as_u64(),
        Some(expected_w),
        "wrong width (stream: {stream})"
    );
    assert_eq!(
        stream["height"].as_u64(),
        Some(expected_h),
        "wrong height (stream: {stream})"
    );

    // `nb_read_frames` is a string in ffprobe JSON.
    let nb_frames: u64 = stream["nb_read_frames"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(nb_frames, expected_frames, "lost or duplicated frames");

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

fn require_prereqs() {
    assert!(
        has_nvidia_gpu(),
        "requires nvidia-smi and a live NVIDIA GPU"
    );
    assert!(has_ffmpeg_vaapi(), "requires ffmpeg h264_vaapi");
    assert!(has_ffprobe(), "requires ffprobe");
    assert!(common::render_node().exists(), "requires a render node");
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
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("h264_vaapi"))
        .unwrap_or(false)
}

fn has_ffprobe() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires live NVIDIA GPU and ffmpeg h264_vaapi"]
fn encode_profiles_resolutions_and_rate_control() {
    require_prereqs();
    common::ensure_libva_symlink();
    let cases = [
        ("1080p60", 1920, 1080, "60", 120, "high", "CBR", "5M", "28"),
        (
            "fractional",
            1280,
            720,
            "30000/1001",
            90,
            "main",
            "VBR",
            "2M",
            "28",
        ),
        (
            "cqp",
            854,
            480,
            "30",
            60,
            "constrained_baseline",
            "CQP",
            "2M",
            "20",
        ),
        (
            "low_bitrate",
            1280,
            720,
            "60",
            180,
            "main",
            "CBR",
            "244k",
            "28",
        ),
    ];
    for (name, width, height, fps, frames, profile, rc, bitrate, qp) in cases {
        let path = std::env::temp_dir().join(format!("nvenc_{name}_{}.h264", std::process::id()));
        let output = Command::new("timeout")
            .args(["45s", "ffmpeg"])
            .env("LIBVA_DRIVERS_PATH", common::drivers_path())
            .env("LIBVA_DRIVER_NAME", "nvidia_nvenc")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-vaapi_device"])
            .arg(common::render_node())
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size={width}x{height}:rate={fps}"),
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-profile:v",
                profile,
                "-rc_mode",
                rc,
                "-b:v",
                bitrate,
                "-qp",
                qp,
                "-bf",
                "0",
                "-g",
                "60",
                "-frames:v",
                &frames.to_string(),
                "-f",
                "h264",
            ])
            .arg(&path)
            .output()
            .expect("ffmpeg");
        assert!(
            output.status.success(),
            "case {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected_profile = match profile {
            "high" => "High",
            "main" => "Main",
            _ => "Constrained Baseline",
        };
        assert_h264_stream(&path, width, height, frames, expected_profile);
        if name == "fractional" {
            // Raw H.264 ffprobe r_frame_rate can report the field rate even
            // for native h264_nvenc. Check the actual SPS rational instead.
            let trace = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "info", "-i"])
                .arg(&path)
                .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(trace.status.success());
            let text = String::from_utf8_lossy(&trace.stderr);
            let field = |name: &str| -> u64 {
                text.lines()
                    .find(|line| line.contains(name))
                    .and_then(|line| line.rsplit_once(" = "))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .expect("SPS timing field")
            };
            let ticks = field("num_units_in_tick");
            let scale = field("time_scale");
            assert!(ticks > 0);
            assert_eq!(scale * 1001, ticks * 2 * 30000);
        }
        std::fs::remove_file(path).unwrap();
    }
}
