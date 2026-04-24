# libvaapi-rust-nvenc — CLAUDE notes

Проєктно-специфічна пам'ять для Claude Code. Ключові знахідки та
контекстні факти, які інакше довелось би реверс-інжинірити заново у
наступних сесіях.

---

## Що це за проєкт

VAAPI backend driver на Rust, який реалізує libva ABI з боку сервера
(не клієнта) і мостить VA encode-запити до NVIDIA NVENC через
`nvidia-video-codec-sdk` + `cudarc`. Ціль — дозволити Linux-клієнтам
які ходять через `libva.so.2` (Chromium/Electron для WebRTC, OBS
`ffmpeg_vaapi`, `ffmpeg h264_vaapi`, gst-vaapi, тощо) використати NVENC
для H.264 encode.

Цільова платформа розробки: Arch Linux, Hyprland/Wayland, RTX 4060,
NVIDIA driver 595.58.03, libva 2.22 (1.23 ABI).

## Hardware-verified working setups

1. **ffmpeg h264_vaapi** — 5.5× realtime на 1080p60 testsrc2,
   `enc=~68%` у `nvidia-smi dmon`. Smoke-тест `tests/encode_ffmpeg.rs`.
2. **OBS Studio** → Advanced → Video Encoder = `FFmpeg VAAPI H.264` —
   пише валідні MP4 через наш драйвер. OBS native `obs-nvenc` тут не
   задіяний; ми переходимо через libva-шлях.
3. **Vesktop (Electron-based Discord) screen share через WebRTC** —
   `Encoder: VaapiVideoEncodeAccelerator`, `Power Efficient: Yes`,
   `Average Encode Time: 8ms`. Перевірено у live Discord voice-каналі.

## Магічна комбінація що зламала Chromium WebRTC gate (КРИТИЧНО)

Chromium на Linux має **три runtime feature-gate-и** у
`media/gpu/gpu_video_encode_accelerator_factory.cc::CreateVaapiVEA`,
які всі `FEATURE_DISABLED_BY_DEFAULT`. Без усіх трьох одночасно WebRTC
пайплайн падає ще до того як торкнеться нашого драйвера:

- `AcceleratedVideoEncoder` — основний gate (feature string без
  суфіксу `Linux` попри те що C++ symbol `kAcceleratedVideoEncodeLinux`)
- `VaapiOnNvidiaGPUs` — анти-NVIDIA гілка; Chromium-коментар:
  *"NVIDIA VA-API drivers do not support Chromium and can sometimes
  cause crashes, disable VA-API on NVIDIA GPUs by default"*
- `VaapiIgnoreDriverChecks` — пропуск валідації vendor string
  (інакше наш `nvidia_nvenc-rs` відхиляється)

Плюс:
- `--ignore-gpu-blocklist` (blocklist від `software_rendering_list.json`)
- `--disable-gpu-sandbox` + `--no-sandbox` — інакше GPU-процес не може
  відкрити `/dev/dri/renderD128` і/або не успадкує `LIBVA_*` env-vars
- `--ozone-platform=x11` — Wayland+VAAPI у Electron має окрему проблему
  з `vaGetDisplay` який повертає invalid display у GPU-процесі

Повна робоча команда для Vesktop:

```bash
LIBVA_DRIVERS_PATH=$HOME/CODING/libvaapi-rust-nvenc/target/release \
LIBVA_DRIVER_NAME=nvidia_nvenc \
vesktop \
  --enable-features=AcceleratedVideoEncoder,VaapiVideoEncoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,WebRtcPipeWireCapturer \
  --ignore-gpu-blocklist \
  --disable-gpu-driver-bug-workarounds \
  --disable-gpu-sandbox \
  --no-sandbox \
  --ozone-platform=x11
```

Vesktop settings (`~/.config/vesktop/settings.json`) мусить містити:
```json
{ "hardwareVideoAcceleration": true }
```

Бо Vesktop `src/main/index.ts` видаляє user-supplied
`--enable-features` / `--disable-features` і перебудовує Set, додаючи
`AcceleratedVideoEncoder` тільки коли `hardwareVideoAcceleration=true`.

## Stock Google Chrome Linux не працює — чому

Google Chrome Linux stable build збудовано без
`enable_hardware_h264_encoding_on_linux=true`. Жоден CLI flag чи
chrome://flags setting цей compile-time gate не обходить. Навіть з
повним набором flag-ів вище `chrome://gpu` показує
`Video Encode: Software only` і `Problems Detected: video_encode`
у disabled features. Arch'ний `chromium` — така сама історія (перевірено).

Electron 40 (той що у Vesktop) — ЗБУДОВАНО з VAAPI encoding enabled.
Саме тому Vesktop працює, а Chrome/Chromium ні.

## Ключові файли драйвера

- `src/lib.rs` — `__vaDriverInit_1_0..1_23` експорти, vtable install.
  Жодних `info_callback` / `error_callback` викликів під час init —
  деякі клієнти (OBS qsv11, obs-nvenc subprocess hand-off) дають нам
  stale callback-и з попередньо вивантажених модулів.
- `src/cuda/mod.rs::from_va_context` — читає `drm_state.fd` ТІЛЬКИ коли
  `display_type == VA_DISPLAY_DRM` або `VA_DISPLAY_DRM_RENDERNODES`.
  Довільний deref `drm_state` на non-DRM display-type = SIGSEGV (OBS-qsv11
  виклик вважав до фіксу). Регресійний тест відсутній — fix обґрунтовано
  у коментарі.
- `src/cuda/external_mem.rs` — `cuImportExternalMemory(OPAQUE_FD)` →
  packed NV12 mipmapped array. Single-object, single-layer DMA-BUF
  (Chromium/PipeWire completing path). Multi-object → UnsupportedMemory.
- `src/driver/config.rs::query_config_attributes` — критично повертає
  реальні 6 attribs (RTFormat=YUV420, RC=CBR|VBR|CQP, PackedHeaders=0x7,
  MaxRefFrames=1, MaxSlices=1, SliceStructure=PowerOfTwo). Повернення 0
  attribs давало Chromium `FillProfileInfo_Locked failed` бо він шукає
  RTFormat & YUV420 бітмаску.
- `src/driver/surface.rs::query_surface_attributes` — 7 attribs з
  `VASurfaceAttribMemoryType = VA | DRM_PRIME_2 (0x40000001)` і
  `DRMFormatModifiers = INVALID (0xffffffffffffffff)`. Без цього
  Chromium не пробує DMA-BUF шлях.
- `src/nvenc/session.rs` — 6-slot bitstream pool з Free/Pending state
  machine, `NV_ENC_ERR_NEED_MORE_INPUT` обробляється як Pending без
  помилки клієнту.
- `src/driver/picture.rs::end_picture` — lazy NVENC register для
  external surfaces через `OnceLock` (session може ще не існувати під
  час create_surfaces2).

## Обмеження і відкладене

- **Multi-object DMA-BUF** (окремі Y і UV fds) — рідко у Chromium,
  відкладено. Повертаємо UnsupportedMemory.
- **BGRA/ARGB DMA-BUF** — потребує PTX-kernel для кольорової
  конверсії. Відкладено. UnsupportedRtFormat.
- **`vaExportSurfaceHandle`** — inverse path (NVENC surface → DMA-BUF
  fd для клієнта). OBS пробує його для tex-path оптимізації; не
  реалізовано, OBS fallback'ається на non-tex (CPU-upload) що працює.
- Google Chrome / stock Arch chromium — build-time gate; не наш баг.
- `VA_CODED_BUF_STATUS_PICTURE_TYPE_I` у libva 2.22 відсутня; `is_idr`
  у `CodedBitstream` лог-only. Не блокер.

## Стек тестів

- `tests/panic_safety.rs` — ABI-межа не падає на null/bogus input.
- `tests/smoke_driver_init.rs` — vtable, __vaDriverInit_* символи.
- `tests/vainfo_smoke.rs` (#[ignore]) — системний vainfo бачить наш
  драйвер.
- `tests/encode_ffmpeg.rs` (#[ignore]) — 720p60 живий encode через
  ffmpeg h264_vaapi → ffprobe валідація. Другий тест (480p30 baseline)
  має incorrect expectation на `coded_width` — NVENC вирівнює до 864.

Всього 68 tests passed, 0 failed, 2 ignored.

## Git

- `613bbb3` — Initial (скелет + encode + c.1..c.4)
- `b485313` — Slice (d) DMA-BUF

Remote не налаштовано. Commit'ити тільки за явним запитом.

## Стиль роботи

- ABI-межа: кожна `extern "C"` — під `catch_unwind`, паніка → VA_STATUS_ERROR_UNKNOWN.
- `unsafe` лише з `// SAFETY:` коментарем що обґрунтовує інваріант.
- Edition 2024, Rust stable 1.85+.
- `.claude/` у gitignore (per-developer workspace), не комітиться.
- Файли `tests/*` — regression guard, не чіпати без потреби.
- `README.md`, `docs/ARCHITECTURE.md`, `docs/DEVELOPMENT.md` — чіпати
  тільки коли зміна справді того вартує.
