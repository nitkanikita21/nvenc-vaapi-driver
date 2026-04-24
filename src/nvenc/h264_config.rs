//! Pure mutator для `NV_ENC_CONFIG` під наші encoder-параметри.
//!
//! Це **не builder** з нуля. Правильний шлях для NVENC — отримати
//! preset-default через `nvEncGetEncodePresetConfigEx`, а потім
//! **модифікувати** потрібні поля. Саме це робить [`apply_config`]: приймає
//! уже-ініціалізований preset-config і вибірково перезаписує профіль,
//! GOP, RC-параметри та VUI. Поля, не згадані тут (тюнінг-hint-и, резерв,
//! matrix-коефіцієнти окрім VUI), лишаються такими, як виставив preset.
//!
//! Тести у цьому файлі не викликають жодного FFI — preset-default
//! емулюється через `MaybeUninit::zeroed()`, що безпечно для POD-структур
//! NVENC. Реальний FFI-шлях (`nvEncGetEncodePresetConfigEx` → `apply_config`
//! → `NvEncInitializeEncoder`) буде доданий у наступному slice (c.2).

use crate::nvenc::preset;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// H.264-профіль, який ми пропагуємо у `profileGUID` та `entropyCodingMode`.
///
/// `ConstrainedBaseline` → CAVLC, `Main`/`High` → CABAC.
///
/// High profile additionally enables 8×8 transform / intra prediction, which
/// NVENC supports on all modern GPUs. Clients like OBS's `ffmpeg_vaapi` encoder
/// default to High (profile_idc = 100); refusing it forces them to software.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// Constrained Baseline: CAVLC, сумісно з усіма декодерами.
    ConstrainedBaseline,
    /// Main: CABAC, кращий RD.
    Main,
    /// High (profile_idc = 100): CABAC + 8×8 transforms. Superset of Main.
    High,
}

/// Високорівнева конфігурація кодеку, незалежна від NVENC-типів.
///
/// Створюється у `driver::picture::end_picture` з `VAEncSequence/PictureParameterBufferH264`
/// і передається в [`apply_config`] разом з preset-default NVENC-конфігом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// `fps_num / fps_den` — точна дробова частота кадрів (23.976 → 24000/1001).
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    pub gop_length: u32,
    pub profile: H264Profile,
    pub rc_mode: preset::RcMode,
    /// Коли `true`, NVENC сам вставляє SPS+PPS перед кожним IDR
    /// (`repeatSPSPPS=1`). Це історичний default для ffmpeg/gst-клієнтів,
    /// які не шлють `VAEncPackedHeader*`. Коли `false`, клієнт сам веде
    /// SPS/PPS (WebRTC/Chromium), і драйвер конкатенує їх у coded-buffer
    /// перед slice-байтами. `end_picture` перемикає цей прапорець на льоту
    /// залежно від того, чи прилетіли packed-headers.
    pub repeat_sps_pps: bool,
}

impl EncoderConfig {
    /// Дефолт: 60 fps, 5 Mbps CBR, GOP = 120 кадрів (2 с), repeat_sps_pps=true.
    /// Клієнти, що драйвлять свій SPS через PackedHeaderData (Chromium),
    /// отримують `repeat_sps_pps=false` через `reconfigure` у `end_picture`.
    pub fn default_for(width: u32, height: u32, profile: H264Profile) -> Self {
        Self {
            width,
            height,
            fps_num: 60,
            fps_den: 1,
            bitrate_bps: 5_000_000,
            gop_length: 120,
            profile,
            rc_mode: preset::RcMode::Cbr,
            repeat_sps_pps: true,
        }
    }
}

/// Per-frame VBV-буфер у бітах: `bitrate / fps`. Округлення — floor
/// (ніколи не overshoot-имо buffer — це безпечно для CBR pacing-у).
#[inline]
fn per_frame_bits(bitrate_bps: u32, fps_num: u32, fps_den: u32) -> u32 {
    if fps_num == 0 {
        return bitrate_bps;
    }
    // ((bitrate * fps_den) / fps_num) у u64, щоб не переповнити u32 на
    // високих bitrate-ах (10 Mbps * 1001 = ~10 G, поміщається в u64).
    let per = (bitrate_bps as u64)
        .saturating_mul(fps_den as u64)
        / (fps_num as u64);
    per.min(u32::MAX as u64) as u32
}

/// Модифікує preset-default `NV_ENC_CONFIG` під `cfg`.
///
/// Поля, що перезаписуються:
/// * `profileGUID` (Main / ConstrainedBaseline)
/// * `gopLength`, `frameIntervalP = 1` (no B-frames)
/// * `rcParams.rateControlMode`, `averageBitRate`, `maxBitRate`, `vbv*`, `constQP`
/// * `encodeCodecConfig.h264Config`:
///   `idrPeriod`, `sliceMode=3`, `sliceModeData=1`, `chromaFormatIDC=1`,
///   `entropyCodingMode`, `repeatSPSPPS=1` bit, `outputAUD=0` bit,
///   `h264VUIParameters` (BT.709 limited range).
///
/// Ніколи не чіпається: `version` (лишається від preset), `tuningInfo`
/// (індиректно — це поле `NV_ENC_INITIALIZE_PARAMS`, не тут), будь-які
/// поля h264Config окрім перелічених (включно з `separateColourPlaneFlag`).
pub fn apply_config(preset_cfg: &mut nv::NV_ENC_CONFIG, cfg: &EncoderConfig) {
    // --- профіль ---
    preset_cfg.profileGUID = match cfg.profile {
        H264Profile::Main => nv::NV_ENC_H264_PROFILE_MAIN_GUID,
        H264Profile::ConstrainedBaseline => nv::NV_ENC_H264_PROFILE_BASELINE_GUID,
        H264Profile::High => nv::NV_ENC_H264_PROFILE_HIGH_GUID,
    };

    // --- GOP ---
    preset_cfg.gopLength = cfg.gop_length;
    preset_cfg.frameIntervalP = 1; // IPPP...

    // --- rate control ---
    let rc = &mut preset_cfg.rcParams;
    match cfg.rc_mode {
        preset::RcMode::Cbr | preset::RcMode::CbrLookahead => {
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = cfg.bitrate_bps;
            rc.maxBitRate = cfg.bitrate_bps;
            // VBV = 1 second of bitrate. An earlier iteration used
            // `per_frame_bits` (1 frame = ~500 bytes at 244 Kbps/60 fps),
            // which starved NVENC: an IDR at 1080p needs ~20 KB and NVENC
            // then either dropped frames or produced fragments — which
            // WebRTC diagnosed as a failing encoder and switched back to
            // OpenH264 after its 8-frame trial window. 1 second is the
            // standard low-latency real-time buffer used by x264/libvpx
            // (`bitrate == buffer-size`).
            rc.vbvBufferSize = cfg.bitrate_bps;
            // Initial delay = half buffer — starts half-full, so the first
            // IDR can borrow up to half-buffer worth of bits without
            // underflowing the decoder.
            rc.vbvInitialDelay = cfg.bitrate_bps / 2;
        }
        preset::RcMode::Vbr => {
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
            rc.averageBitRate = cfg.bitrate_bps;
            // maxBitRate лишаємо як preset поставив, або дублюємо.
            if rc.maxBitRate < cfg.bitrate_bps {
                rc.maxBitRate = cfg.bitrate_bps;
            }
        }
        preset::RcMode::ConstQp => {
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
            // Розумний default — QP 28 (Discord-типовий сценарій);
            // точний QP прийде у c.2 з VAEncMiscParameterQP.
            rc.constQP = nv::NV_ENC_QP {
                qpInterP: 28,
                qpInterB: 28,
                qpIntra: 28,
            };
        }
    }

    // --- Low-latency realtime hints ---
    // Без reorder-delay NVENC віддає кожен закодований кадр одразу, без
    // внутрішнього framebuffer-у. Це критично для WebRTC-пайплайну: без
    // цього прапорця NVENC повертає `NEED_MORE_INPUT` для перших кількох
    // кадрів, Chromium отримує пусті bitstream-и і вимикає encoder.
    rc.set_zeroReorderDelay(1);
    // Lookahead тримаємо вимкненим — realtime screen share вимагає нульової
    // латентності; lookahead додав би кілька кадрів затримки.
    rc.set_enableLookahead(0);

    // --- H.264 кодек-specific ---
    // SAFETY: `encodeCodecConfig` — C-union; ми трактуємо його як
    // `h264Config`, що коректно, бо preset-config отримано з
    // `nvEncGetEncodePresetConfigEx(..., H264 codec GUID, ...)` і тому активний
    // варіант — саме h264Config. У юніт-тестах preset-config — `zeroed()`,
    // що є валідним бітовим паттерном для всіх варіантів union-а.
    let h264 = unsafe { &mut preset_cfg.encodeCodecConfig.h264Config };
    h264.idrPeriod = cfg.gop_length;
    h264.sliceMode = 3; // fixed number of slices
    h264.sliceModeData = 1; // 1 slice per frame
    h264.chromaFormatIDC = 1; // 4:2:0
    h264.entropyCodingMode = match cfg.profile {
        H264Profile::Main | H264Profile::High =>
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC,
        H264Profile::ConstrainedBaseline =>
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC,
    };
    // `repeatSPSPPS` керується з `EncoderConfig`: ffmpeg-like клієнти
    // хочуть SPS+PPS на кожному IDR від енкодера, WebRTC/Chromium — ні
    // (Chromium пакує свій SPS у `VAEncPackedHeader*` і чекає що ми
    // емітимо саме його байти).
    h264.set_repeatSPSPPS(if cfg.repeat_sps_pps { 1 } else { 0 });
    h264.set_outputAUD(0); // AUD не вставляємо (libva client сам напакує)

    // --- VUI: BT.709 limited range ---
    let vui = &mut h264.h264VUIParameters;
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = 0;
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    vui.transferCharacteristics =
        nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::MaybeUninit;

    /// Preset-default емуляція: zero-filled NV_ENC_CONFIG.
    ///
    /// # Safety
    /// NVENC-структури `#[repr(C)]`, всі поля — POD (u32 / enum з варіантом 0 /
    /// вкладений union зі spare-варіантом). Zero — валідний біт-паттерн.
    fn zeroed_preset() -> nv::NV_ENC_CONFIG {
        unsafe { MaybeUninit::<nv::NV_ENC_CONFIG>::zeroed().assume_init() }
    }

    fn default_1080p60_cbr() -> EncoderConfig {
        EncoderConfig {
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_bps: 5_000_000,
            gop_length: 120,
            profile: H264Profile::Main,
            rc_mode: preset::RcMode::Cbr,
            repeat_sps_pps: true,
        }
    }

    #[test]
    fn apply_config_cbr_1080p60_main() {
        let mut cfg = zeroed_preset();
        apply_config(&mut cfg, &default_1080p60_cbr());
        assert_eq!(cfg.profileGUID, nv::NV_ENC_H264_PROFILE_MAIN_GUID);
        assert_eq!(cfg.gopLength, 120);
        assert_eq!(cfg.frameIntervalP, 1);
        assert_eq!(
            cfg.rcParams.rateControlMode,
            nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR
        );
        assert_eq!(cfg.rcParams.averageBitRate, 5_000_000);
        assert_eq!(cfg.rcParams.maxBitRate, 5_000_000);
        // VBV = 1 second of bitrate (low-latency real-time convention);
        // initial delay = half that. Earlier iteration used per-frame
        // buffer which starved NVENC at low Kbps WebRTC targets.
        assert_eq!(cfg.rcParams.vbvBufferSize, 5_000_000);
        assert_eq!(cfg.rcParams.vbvInitialDelay, 2_500_000);
        // SAFETY: union set to h264 by apply_config.
        let h264 = unsafe { &cfg.encodeCodecConfig.h264Config };
        assert_eq!(h264.sliceMode, 3);
        assert_eq!(h264.sliceModeData, 1);
        assert_eq!(h264.idrPeriod, 120);
        assert_eq!(
            h264.entropyCodingMode,
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC
        );
        assert_eq!(h264.chromaFormatIDC, 1);
        assert_eq!(h264.repeatSPSPPS(), 1);
        assert_eq!(h264.outputAUD(), 0);
        assert_eq!(
            h264.h264VUIParameters.colourMatrix,
            nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709
        );
        assert_eq!(h264.h264VUIParameters.videoFullRangeFlag, 0);
    }

    #[test]
    fn apply_config_cqp_fills_const_qp() {
        let mut cfg = zeroed_preset();
        let mut ec = default_1080p60_cbr();
        ec.rc_mode = preset::RcMode::ConstQp;
        apply_config(&mut cfg, &ec);
        assert_eq!(
            cfg.rcParams.rateControlMode,
            nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP
        );
        assert_eq!(cfg.rcParams.constQP.qpIntra, 28);
        assert_eq!(cfg.rcParams.constQP.qpInterP, 28);
        assert_eq!(cfg.rcParams.constQP.qpInterB, 28);
        // У CQP не виставляємо bitrate/vbv (лишаються preset-default = 0 тут).
        assert_eq!(cfg.rcParams.averageBitRate, 0);
        assert_eq!(cfg.rcParams.vbvBufferSize, 0);
    }

    #[test]
    fn apply_config_baseline_uses_cavlc_and_baseline_guid() {
        let mut cfg = zeroed_preset();
        let mut ec = default_1080p60_cbr();
        ec.profile = H264Profile::ConstrainedBaseline;
        apply_config(&mut cfg, &ec);
        assert_eq!(cfg.profileGUID, nv::NV_ENC_H264_PROFILE_BASELINE_GUID);
        // SAFETY: union resolved by apply_config.
        let h264 = unsafe { &cfg.encodeCodecConfig.h264Config };
        assert_eq!(
            h264.entropyCodingMode,
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC
        );
    }

    #[test]
    fn default_for_720p_main_has_sane_values() {
        let ec = EncoderConfig::default_for(1280, 720, H264Profile::Main);
        assert_eq!(ec.width, 1280);
        assert_eq!(ec.height, 720);
        assert_eq!(ec.fps_num, 60);
        assert_eq!(ec.fps_den, 1);
        assert_eq!(ec.bitrate_bps, 5_000_000);
        assert_eq!(ec.gop_length, 120);
        assert_eq!(ec.profile, H264Profile::Main);
        assert_eq!(ec.rc_mode, preset::RcMode::Cbr);
    }

    #[test]
    fn edge_cases_small_dims_23_976_fps_gop1() {
        let mut cfg = zeroed_preset();
        let ec = EncoderConfig {
            width: 320,
            height: 240,
            fps_num: 24_000,
            fps_den: 1001, // 23.976
            bitrate_bps: 500_000,
            gop_length: 1,
            profile: H264Profile::Main,
            rc_mode: preset::RcMode::Cbr,
            repeat_sps_pps: true,
        };
        apply_config(&mut cfg, &ec);
        assert_eq!(cfg.gopLength, 1);
        // VBV = 1 s of bitrate (500 Kbps).
        assert_eq!(cfg.rcParams.vbvBufferSize, 500_000);
        // SAFETY: union resolved.
        let h264 = unsafe { &cfg.encodeCodecConfig.h264Config };
        assert_eq!(h264.idrPeriod, 1);
    }

    #[test]
    fn apply_config_does_not_touch_tuning_fields() {
        let mut cfg = zeroed_preset();
        // Емуляція preset-набитих полів, які ми не маємо перезаписувати:
        cfg.monoChromeEncoding = 42;
        cfg.mvPrecision = nv::NV_ENC_MV_PRECISION::NV_ENC_MV_PRECISION_QUARTER_PEL;
        cfg.rcParams.targetQuality = 25;
        cfg.rcParams.lookaheadDepth = 8;
        apply_config(&mut cfg, &default_1080p60_cbr());
        assert_eq!(cfg.monoChromeEncoding, 42);
        assert_eq!(
            cfg.mvPrecision,
            nv::NV_ENC_MV_PRECISION::NV_ENC_MV_PRECISION_QUARTER_PEL
        );
        assert_eq!(cfg.rcParams.targetQuality, 25);
        assert_eq!(cfg.rcParams.lookaheadDepth, 8);
    }

    #[test]
    fn apply_config_preserves_untouched_h264_fields() {
        let mut cfg = zeroed_preset();
        // Сетимо «preset default» для полів, що ми не чіпаємо.
        // SAFETY: union — ми записуємо через h264Config; далі apply_config
        // все одно активує той самий варіант.
        unsafe {
            let h264 = &mut cfg.encodeCodecConfig.h264Config;
            h264.separateColourPlaneFlag = 7;
            h264.maxNumRefFrames = 4;
            h264.level = 42;
        }
        apply_config(&mut cfg, &default_1080p60_cbr());
        // SAFETY: union resolved.
        let h264 = unsafe { &cfg.encodeCodecConfig.h264Config };
        assert_eq!(h264.separateColourPlaneFlag, 7);
        assert_eq!(h264.maxNumRefFrames, 4);
        assert_eq!(h264.level, 42);
    }

    #[test]
    fn apply_config_bitrate_round_trip_10mbps() {
        let mut cfg = zeroed_preset();
        let mut ec = default_1080p60_cbr();
        ec.bitrate_bps = 10_000_000;
        apply_config(&mut cfg, &ec);
        assert_eq!(cfg.rcParams.averageBitRate, 10_000_000);
        assert_eq!(cfg.rcParams.maxBitRate, 10_000_000);
        // VBV = 1 s of bitrate; initial delay = 500 ms.
        assert_eq!(cfg.rcParams.vbvBufferSize, 10_000_000);
        assert_eq!(cfg.rcParams.vbvInitialDelay, 5_000_000);
    }
}
