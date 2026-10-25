#!/usr/bin/env python3
"""Compare paced H.264 VA-API/NVENC/CPU encoding; validate every output frame."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def render_node() -> Path:
    override = os.environ.get("NVENC_TEST_RENDER_NODE")
    if override:
        return Path(override)
    for node in sorted(Path("/sys/class/drm").glob("renderD*")):
        if (node / "device/vendor").read_text().strip() == "0x10de":
            return Path("/dev/dri") / node.name
    raise RuntimeError("No NVIDIA render node; set NVENC_TEST_RENDER_NODE")


def benchmark(mode: str, seconds: int, work: Path, pacing: str, *, drivers_path: Path | None = None) -> dict:
    output = work / f"{mode}.h264"
    env = os.environ.copy()
    node = render_node()
    pci = (Path("/sys/class/drm") / node.resolve().name / "device").resolve().name
    gpu_index = subprocess.check_output(["nvidia-smi", f"--id={pci}", "--query-gpu=index", "--format=csv,noheader"], text=True).strip()
    env["NVENC_VAAPI_DEVICE"] = pci
    args = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
    if mode == "vaapi":
        drivers = drivers_path or Path(__file__).resolve().parent.parent / "target/release"
        if not (drivers / "nvidia_nvenc_drv_video.so").exists():
            raise RuntimeError("Build release driver and run tools/run-vainfo.sh first")
        env.update(LIBVA_DRIVER_NAME="nvidia_nvenc", LIBVA_DRIVERS_PATH=str(drivers))
        args += ["-vaapi_device", str(render_node())]
    if pacing == "real-time":
        args += ["-re"]
    args += ["-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60",
             "-frames:v", str(seconds * 60)]
    encoders = {
        "vaapi": ["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-rc_mode", "CBR"],
        "nvenc": ["-vf", "format=nv12", "-c:v", "h264_nvenc", "-gpu", gpu_index, "-preset", "p4", "-tune", "ull", "-rc", "cbr"],
        "cpu": ["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency"],
    }
    # Fix initial VBV fullness too; encoder defaults differ across backends.
    args += encoders[mode] + ["-b:v", "5M", "-maxrate", "5M", "-bufsize", "5M",
                             "-rc_init_occupancy", "2500000",
                             "-bf", "0", "-g", "120", "-f", "h264", str(output)]
    samples = work / f"{mode}.gpu.csv"
    with samples.open("w") as stream:
        monitor = subprocess.Popen(["nvidia-smi", f"--id={pci}", "--query-gpu=power.draw,utilization.encoder,memory.used",
                                    "--format=csv,noheader,nounits", "--loop-ms=200"],
                                   stdout=stream, stderr=subprocess.PIPE)
        try:
            started = time.monotonic()
            child = subprocess.Popen(["timeout", str(seconds * 4 + 30), *args], env=env)
            _, status, usage = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            elapsed = time.monotonic() - started
            if child.returncode:
                raise subprocess.CalledProcessError(child.returncode, args)
        finally:
            monitor.terminate()
            monitor.communicate(timeout=10)
    probe = subprocess.run(["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
                            "-show_entries", "stream=width,height,nb_read_frames", "-of", "json", str(output)],
                           check=True, capture_output=True, text=True, timeout=30)
    if probe.stderr:
        raise RuntimeError(probe.stderr)
    video = json.loads(probe.stdout)["streams"][0]
    if (video["width"], video["height"], int(video["nb_read_frames"])) != (1920, 1080, seconds * 60):
        raise RuntimeError(f"Invalid output: {video}")
    user, system, rss = usage.ru_utime, usage.ru_stime, usage.ru_maxrss
    readings = []
    for line in samples.read_text().splitlines():
        try:
            readings.append([float(v.strip()) for v in line.split(",")])
        except ValueError:  # e.g. power not supported on this GPU
            continue
    return {"mode": mode, "pacing": pacing, "fps": seconds * 60 / elapsed, "elapsed_s": elapsed, "cpu_s": user + system,
            "cpu_percent_one_core": round((user + system) / elapsed * 100, 2),
            "max_rss_mib": rss / 1024, "frames": seconds * 60,
            "mean_gpu_power_w": statistics.mean(r[0] for r in readings) if readings else None,
            "max_encoder_util_percent": max((r[1] for r in readings), default=None),
            "output_bytes": output.stat().st_size,
            "output_sha256": hashlib.sha256(output.read_bytes()).hexdigest()}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=15)
    parser.add_argument("--pacing", choices=["real-time", "unlimited"], default="real-time")
    parser.add_argument("--mode", choices=["vaapi", "nvenc", "cpu", "all"], default="all")
    parser.add_argument("--drivers-path", type=Path, help="VA-API driver directory for controlled build comparisons")
    options = parser.parse_args()
    if not 1 <= options.seconds <= 120:
        parser.error("--seconds must be between 1 and 120")
    modes = ["vaapi", "nvenc", "cpu"] if options.mode == "all" else [options.mode]
    with tempfile.TemporaryDirectory(prefix="nvenc-benchmark-") as folder:
        for mode in modes:
            print(json.dumps(benchmark(mode, options.seconds, Path(folder), options.pacing, drivers_path=options.drivers_path)), flush=True)


if __name__ == "__main__":
    main()
