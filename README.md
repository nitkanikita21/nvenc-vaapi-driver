# libvaapi-rust-nvenc

Drop-in VAAPI encode backend для Linux, що надає H.264-кодування через NVIDIA NVENC застосункам, які використовують стандартний VAAPI-шлях (Discord, Chromium, OBS).

---

## Проблема

NVIDIA не постачає VAAPI encode driver для Linux. Офіційний `nvidia_drv_video.so` реалізує лише **decode** — він прокидає виклики через VDPAU і не має жодного encode entrypoint. Коли Chromium/Electron (Discord) запускається з прапорцями `--enable-features=AcceleratedVideoEncodeVaapi,VaapiVideoEncoder`, libva не знаходить `VAEntrypointEncSlice` і без попередження переходить на software encode. Результат: високе CPU-навантаження під час screenshare і обмежена якість.

## Рішення

`libvaapi-rust-nvenc` реалізує VAAPI backend з боку *сервера* (backend driver, а не клієнт). libva завантажує `.so` через стандартний `dlopen`-механізм і делегує encode-виклики напряму до NVIDIA Video Codec SDK (NVENC) через CUDA Driver API. З точки зору Chromium це звичайний VAAPI-драйвер — жодних змін у коді застосунку не потрібно.

## Статус

**Alpha / MVP.** Реалізовано:

- H.264 encode: `VAProfileH264ConstrainedBaseline` та `VAProfileH264Main`, entrypoint `VAEntrypointEncSlice`
- ABI-сумісність з libva 1.0–1.23 (усі 24 аліаси `__vaDriverInit_1_*`)
- Повна `VADriverVTable` (config, surface, context, buffer, picture, sync, image stubs, export stub, display attrs)
- Пресет-менеджер (bitrate/fps → NVENC P3/P4/P5 + CBR/CQP), реалізований та покритий тестами
- NV12 DMA-BUF zero-copy вхід через `cuImportExternalMemory` (заглушка, підключення у наступній ітерації)

**Non-goals першої ітерації:** decode, HEVC, AV1. Цей драйвер не замінює та не конфліктує з офіційним NVIDIA VAAPI backend.

---

## Вимоги

| Компонент | Мінімальна версія |
|---|---|
| Linux | будь-яке ядро з DRM/KMS |
| NVIDIA driver | 525.x (NVENC SDK 12.0+, presets P3–P7) |
| libva | 1.18 (runtime + headers, `va_drmcommon.h`) |
| Rust (stable) | 1.85+ (edition 2024) |
| clang / libclang-dev | будь-яка (потрібен bindgen для `va_backend.h`) |

Перевірено на Arch Linux: NVIDIA driver 595.x, libva 2.22, RTX 4060.

NVIDIA Video Codec SDK headers шукаються автоматично. Якщо вони не у стандартних системних шляхах:

```bash
export NVIDIA_VIDEO_CODEC_SDK_PATH=/opt/nvidia-video-codec-sdk
```

---

## Збірка

```bash
# Arch Linux
sudo pacman -S libva libva-utils clang

cargo build --release
```

Артефакт: `target/release/libnvidia_nvenc_drv_video.so`

Перевірити наявність export-символів:

```bash
nm -D target/release/libnvidia_nvenc_drv_video.so | grep __vaDriverInit
```

Очікуваний результат — рядки від `__vaDriverInit_1_0` до `__vaDriverInit_1_23`.

> Rust `cdylib` завжди додає префікс `lib`. libva очікує файл без нього:
> `libnvidia_nvenc_drv_video.so` → встановлюється як `nvidia_nvenc_drv_video.so`.

---

## Інсталяція

### Без root (рекомендовано)

```bash
mkdir -p ~/.local/lib/dri
cp target/release/libnvidia_nvenc_drv_video.so \
   ~/.local/lib/dri/nvidia_nvenc_drv_video.so
```

### Системна (root)

```bash
sudo cp target/release/libnvidia_nvenc_drv_video.so \
        /usr/lib/dri/nvidia_nvenc_drv_video.so
```

Задайте змінні оточення (можна додати до `~/.bash_profile`, `~/.zprofile` або `~/.config/environment.d/vaapi.conf`):

```bash
export LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri"
export LIBVA_DRIVER_NAME="nvidia_nvenc"
```

libva формує ім'я файлу за правилом: `lib${LIBVA_DRIVER_NAME}_drv_video.so`.

---

## Використання з Discord

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME="nvidia_nvenc" \
discord \
  --enable-features=AcceleratedVideoEncodeVaapi,VaapiVideoEncoder \
  --disable-features=UseChromeOSDirectVideoDecoder \
  --ozone-platform-hint=auto
```

Для Flatpak-версії Discord:

```bash
flatpak override --user \
  --env=LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
  --env=LIBVA_DRIVER_NAME="nvidia_nvenc" \
  com.discordapp.Discord
```

---

## Використання з Chromium / Chrome

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME="nvidia_nvenc" \
chromium \
  --enable-features=AcceleratedVideoEncodeVaapi,VaapiVideoEncoder \
  --ozone-platform-hint=auto
```

Після запуску відкрийте `chrome://gpu` і знайдіть рядок:

```
Video Encode: Hardware accelerated
```

У розділі "Video Acceleration Information" має бути рядок `Encode h264`.

---

## Верифікація

### vainfo

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME="nvidia_nvenc" \
vainfo
```

Очікуваний вивід (скорочено):

```
libva info: VA-API version 1.23.0
libva info: Trying to open /home/<user>/.local/lib/dri/nvidia_nvenc_drv_video.so
libva info: Found init function __vaDriverInit_1_23
libva info: va_openDriver() returns 0
vainfo: VA-API version: 1.23 (libva 2.x.y)
vainfo: Driver version: nvidia_nvenc-rs (Rust NVENC VAAPI backend)
vainfo: Supported profile and entrypoints
      VAProfileH264ConstrainedBaseline: VAEntrypointEncSlice
      VAProfileH264Main               : VAEntrypointEncSlice
```

Якщо рядків `VAEntrypointEncSlice` немає — драйвер не завантажився. Дивіться розділ Troubleshooting.

### Навантаження NVENC під час encode

```bash
nvidia-smi dmon -s u
```

Стовпець `enc` повинен бути ненульовим під час активного screenshare. Якщо він дорівнює 0 — Chromium використовує software fallback.

---

## Архітектура (огляд)

```
Chromium / Discord (Electron)
        |  VAAPI client calls
        v
   libva.so.2
        |  dlopen("nvidia_nvenc_drv_video.so")
        |  dlsym("__vaDriverInit_1_23")
        v
+-----------------------------------------------+
|       libnvidia_nvenc_drv_video.so             |
|                                                |
|   VA frontend          NVENC backend           |
|   (vtable impl,        (NVIDIA Video Codec     |
|    ID pools/slotmap,   SDK + CUDA Driver API,  |
|    stream builder)     DMA-BUF import)         |
+-----------------------------------------------+
        |
        v
  libnvidia-encode.so  +  libcuda.so
```

Два шляхи передачі вхідного кадру:

**Внутрішні NV12 surfaces** — `cuMemAllocPitch` → `NvEncRegisterResource(CUDADEVICEPTR)` → `NvEncEncodePicture`.

**DMA-BUF zero-copy** — compositor передає DRM PRIME fd → `cuImportExternalMemory(OPAQUE_FD)` → `NvEncRegisterResource(CUDAARRAY)` → encode без CPU-копіювання.

Детальніша документація для контрибьюторів: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## Тестування

Запустити всі unit-тести (не потребують GPU):

```bash
cargo test --workspace --release
```

End-to-end тести (потрібен GPU + встановлений драйвер):

```bash
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME="nvidia_nvenc" \
cargo test --workspace --release -- --include-ignored vainfo
```

Smoke-encode тест (записує `out.h264`, перевіряє через `ffprobe`):

```bash
cargo test --release encode_nv12
```

---

## Troubleshooting

### Неправильний драйвер або драйвер не завантажується

Увімкніть трасування libva:

```bash
LIBVA_TRACE=/tmp/va \
LIBVA_DRIVERS_PATH="$HOME/.local/lib/dri" \
LIBVA_DRIVER_NAME="nvidia_nvenc" \
vainfo
```

Перевірте `/tmp/va.log` — видно, який `.so` фактично відкрито і чи знайдено символ `__vaDriverInit`.

Переконайтесь, що файл встановлено **без** префікса `lib`:

```bash
ls -la ~/.local/lib/dri/nvidia_nvenc_drv_video.so
```

### Permission denied на `/dev/nvidia*`

```bash
ls -la /dev/nvidia*
# Додати користувача до групи video:
sudo usermod -aG video $USER
```

### Wayland + PipeWire: format mismatch

Деякі compositors надсилають BGRA DMA-BUF замість NV12. Драйвер виконує конвертацію BGRA→NV12 через вбудований CUDA PTX kernel. Якщо виникає помилка під час `vaCreateSurfaces2`, переконайтесь, що compositor підтримує `GBM_FORMAT_NV12` export.

### Chromium повертається до software encode

1. Переконайтесь, що `vainfo` успішно виводить `VAEntrypointEncSlice`.
2. Перевірте `chrome://version` → Command Line — обидва прапорці `AcceleratedVideoEncodeVaapi` і `VaapiVideoEncoder` мають бути присутні.
3. Запустіть з `--vmodule=vaapi*=3` для детального логування VAAPI у stderr.

---

## Ліцензія

TBD — see LICENSE.
