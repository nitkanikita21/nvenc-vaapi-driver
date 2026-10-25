//! Pure mutator для `NV_ENC_CONFIG` під наші encoder-параметри.
//!
//! Це **не builder** з нуля. Правильний шлях для NVENC — отримати
//! preset-default через `nvEncGetEncodePresetConfigEx`, а потім
//! **модифікувати** потрібні поля. Саме це робить [`apply_config`]: приймає
//! уже-ініціалізований preset-config і вибірково перезаписує профіль,
//! GOP, RC-параметри та VUI. Поля, не згадані тут (тюнінг-hint-и, резерв,
//! matrix-коефіцієнти окрім VUI), лишаються такими, як виставив preset.
//!
//! Tests use a synthetic config with valid Rust enum discriminants. Runtime
//! sessions obtain preset defaults from NVENC before applying these overrides.

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
    pub max_bitrate_bps: u32,
    pub gop_length: u32,
    pub profile: H264Profile,
    pub rc_mode: preset::RcMode,
    pub qp: u32,
    /// Zero leaves the corresponding rate-control hint unspecified.
    pub initial_qp: u32,
    pub min_qp: u32,
    pub max_qp: u32,
    pub window_size_ms: u32,
    /// Explicit HRD buffering takes precedence over the rate-control window.
    pub hrd: Option<HrdBuffer>,
    /// Коли `true`, NVENC сам вставляє SPS+PPS перед кожним IDR
    /// (`repeatSPSPPS=1`). Це історичний default для ffmpeg/gst-клієнтів,
    /// які не шлють `VAEncPackedHeader*`. Коли `false`, клієнт сам веде
    /// SPS/PPS (WebRTC/Chromium), і драйвер конкатенує їх у coded-buffer
    /// перед slice-байтами. `end_picture` перемикає цей прапорець на льоту
    /// залежно від того, чи прилетіли packed-headers.
    pub repeat_sps_pps: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrdBuffer {
    pub size_bits: u32,
    pub initial_fullness_bits: u32,
}

impl EncoderConfig {
    /// Rate updates leave queued picture layouts and coding tools unchanged.
    pub fn is_rate_update_from(&self, previous: &Self) -> bool {
        *self
            == Self {
                bitrate_bps: self.bitrate_bps,
                max_bitrate_bps: self.max_bitrate_bps,
                fps_num: self.fps_num,
                fps_den: self.fps_den,
                window_size_ms: self.window_size_ms,
                hrd: self.hrd,
                ..*previous
            }
    }

    pub fn validate_rate_control(&self) -> Result<(), crate::error::DriverError> {
        if self.qp > 51
            || self.initial_qp > 51
            || self.min_qp > 51
            || self.max_qp > 51
            || (self.min_qp != 0 && self.max_qp != 0 && self.min_qp > self.max_qp)
            || self
                .hrd
                .is_some_and(|hrd| hrd.size_bits == 0 || hrd.initial_fullness_bits > hrd.size_bits)
            || (self.hrd.is_none()
                && u64::from(self.max_bitrate_bps.max(self.bitrate_bps))
                    * u64::from(self.window_size_ms)
                    / 1000
                    > u64::from(u32::MAX))
        {
            return Err(crate::error::DriverError::InvalidParameter);
        }
        Ok(())
    }

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
            max_bitrate_bps: 5_000_000,
            gop_length: 120,
            profile,
            rc_mode: preset::RcMode::Cbr,
            qp: 28,
            initial_qp: 0,
            min_qp: 0,
            max_qp: 0,
            window_size_ms: 0,
            hrd: None,
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
    let per = (bitrate_bps as u64).saturating_mul(fps_den as u64) / (fps_num as u64);
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
            rc.maxBitRate = cfg.max_bitrate_bps.max(cfg.bitrate_bps);
            rc.vbvBufferSize = 0;
            rc.vbvInitialDelay = 0;
        }
        preset::RcMode::ConstQp => {
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
            // Розумний default — QP 28 (Discord-типовий сценарій);
            // точний QP прийде у c.2 з VAEncMiscParameterQP.
            rc.constQP = nv::NV_ENC_QP {
                qpInterP: cfg.qp,
                qpInterB: cfg.qp,
                qpIntra: cfg.qp,
            };
        }
    }

    if !matches!(cfg.rc_mode, preset::RcMode::ConstQp) {
        let qp = |value| nv::NV_ENC_QP {
            qpInterP: value,
            qpInterB: value,
            qpIntra: value,
        };
        rc.set_enableMinQP(u32::from(cfg.min_qp != 0));
        rc.set_enableMaxQP(u32::from(cfg.max_qp != 0));
        rc.set_enableInitialRCQP(u32::from(cfg.initial_qp != 0));
        rc.minQP = qp(cfg.min_qp);
        rc.maxQP = qp(cfg.max_qp);
        rc.initialRCQP = qp(cfg.initial_qp);
        let buffer = cfg.hrd.or_else(|| {
            (cfg.window_size_ms != 0).then(|| {
                let peak = rc.maxBitRate.max(rc.averageBitRate);
                // The caller validates this product before configuring NVENC.
                let size_bits = (u64::from(peak) * u64::from(cfg.window_size_ms) / 1000) as u32;
                HrdBuffer {
                    size_bits,
                    initial_fullness_bits: size_bits / 2,
                }
            })
        });
        if let Some(buffer) = buffer {
            rc.vbvBufferSize = buffer.size_bits;
            rc.vbvInitialDelay = buffer.initial_fullness_bits;
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
        H264Profile::Main | H264Profile::High => {
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC
        }
        H264Profile::ConstrainedBaseline => {
            nv::NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC
        }
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
    /// Initialise the nonzero frame-field enum before forming a Rust config.
    fn zeroed_preset() -> nv::NV_ENC_CONFIG {
        let mut storage = MaybeUninit::<nv::NV_ENC_CONFIG>::zeroed();
        // SAFETY: initialise the only nonzero Rust enum before assuming init.
        unsafe {
            core::ptr::addr_of_mut!((*storage.as_mut_ptr()).frameFieldMode)
                .write(nv::NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME);
            storage.assume_init()
        }
    }

    fn default_1080p60_cbr() -> EncoderConfig {
        EncoderConfig::default_for(1920, 1080, H264Profile::Main)
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
            max_bitrate_bps: 500_000,
            gop_length: 1,
            profile: H264Profile::Main,
            rc_mode: preset::RcMode::Cbr,
            qp: 28,
            repeat_sps_pps: true,
            ..default_1080p60_cbr()
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
    #[test]
    fn vbr_retains_separate_average_and_peak_and_cqp_honours_client_qp() {
        let mut config = zeroed_preset();
        let mut request = default_1080p60_cbr();
        request.rc_mode = preset::RcMode::Vbr;
        request.bitrate_bps = 2_000_000;
        request.max_bitrate_bps = 4_000_000;
        apply_config(&mut config, &request);
        assert_eq!(config.rcParams.averageBitRate, 2_000_000);
        assert_eq!(config.rcParams.maxBitRate, 4_000_000);
        request.rc_mode = preset::RcMode::ConstQp;
        request.qp = 20;
        apply_config(&mut config, &request);
        assert_eq!(config.rcParams.constQP.qpIntra, 20);
        assert_eq!(config.rcParams.constQP.qpInterP, 20);
    }

    #[test]
    fn client_qp_limits_and_hrd_override_defaults_in_bitrate_modes() {
        for mode in [preset::RcMode::Cbr, preset::RcMode::Vbr] {
            let mut config = zeroed_preset();
            let mut request = default_1080p60_cbr();
            request.rc_mode = mode;
            request.initial_qp = 26;
            request.min_qp = 10;
            request.max_qp = 42;
            request.window_size_ms = 1500;
            request.hrd = Some(HrdBuffer {
                size_bits: 6_000_000,
                initial_fullness_bits: 2_000_000,
            });
            apply_config(&mut config, &request);
            assert_eq!(config.rcParams.enableMinQP(), 1);
            assert_eq!(config.rcParams.enableMaxQP(), 1);
            assert_eq!(config.rcParams.enableInitialRCQP(), 1);
            assert_eq!(config.rcParams.minQP.qpInterP, 10);
            assert_eq!(config.rcParams.maxQP.qpIntra, 42);
            assert_eq!(config.rcParams.initialRCQP.qpInterP, 26);
            assert_eq!(config.rcParams.vbvBufferSize, 6_000_000);
            assert_eq!(config.rcParams.vbvInitialDelay, 2_000_000);
        }
    }

    #[test]
    fn rate_window_tracks_peak_bitrate_when_hrd_is_absent() {
        let mut config = zeroed_preset();
        let mut request = default_1080p60_cbr();
        request.rc_mode = preset::RcMode::Vbr;
        request.bitrate_bps = 2_000_000;
        request.max_bitrate_bps = 4_000_000;
        request.window_size_ms = 1500;
        apply_config(&mut config, &request);
        assert_eq!(config.rcParams.vbvBufferSize, 6_000_000);
        assert_eq!(config.rcParams.vbvInitialDelay, 3_000_000);
    }

    #[test]
    fn clearing_client_controls_removes_stale_qp_limits_and_buffering() {
        let mut config = zeroed_preset();
        let mut request = default_1080p60_cbr();
        request.min_qp = 10;
        request.max_qp = 42;
        request.initial_qp = 26;
        request.hrd = Some(HrdBuffer {
            size_bits: 8_000_000,
            initial_fullness_bits: 4_000_000,
        });
        apply_config(&mut config, &request);
        request = default_1080p60_cbr();
        apply_config(&mut config, &request);
        assert_eq!(config.rcParams.enableMinQP(), 0);
        assert_eq!(config.rcParams.enableMaxQP(), 0);
        assert_eq!(config.rcParams.enableInitialRCQP(), 0);
        assert_eq!(config.rcParams.vbvBufferSize, 5_000_000);
        request.rc_mode = preset::RcMode::Vbr;
        apply_config(&mut config, &request);
        assert_eq!(config.rcParams.vbvBufferSize, 0);
        assert_eq!(config.rcParams.vbvInitialDelay, 0);
    }

    #[test]
    fn invalid_rate_controls_are_rejected_without_clamping() {
        let baseline = default_1080p60_cbr();
        for bad in [
            EncoderConfig {
                min_qp: 52,
                ..baseline
            },
            EncoderConfig {
                max_qp: 52,
                ..baseline
            },
            EncoderConfig {
                initial_qp: 52,
                ..baseline
            },
            EncoderConfig {
                min_qp: 42,
                max_qp: 10,
                ..baseline
            },
            EncoderConfig {
                hrd: Some(HrdBuffer {
                    size_bits: 10,
                    initial_fullness_bits: 11,
                }),
                ..baseline
            },
            EncoderConfig {
                window_size_ms: u32::MAX,
                ..baseline
            },
        ] {
            assert!(bad.validate_rate_control().is_err(), "{bad:?}");
        }
        assert!(baseline.validate_rate_control().is_ok());
        let exact = EncoderConfig {
            hrd: Some(HrdBuffer {
                size_bits: u32::MAX,
                initial_fullness_bits: u32::MAX,
            }),
            ..baseline
        };
        assert!(exact.validate_rate_control().is_ok());
    }

    #[test]
    fn queued_rate_updates_exclude_coding_tool_and_geometry_changes() {
        let before = default_1080p60_cbr();
        let rates = EncoderConfig {
            bitrate_bps: 2_000_000,
            max_bitrate_bps: 3_000_000,
            fps_num: 30_000,
            fps_den: 1001,
            window_size_ms: 1500,
            hrd: Some(HrdBuffer {
                size_bits: 4_500_000,
                initial_fullness_bits: 2_250_000,
            }),
            ..before
        };
        assert!(rates.is_rate_update_from(&before));
        for changed in [
            EncoderConfig {
                width: 1280,
                ..rates
            },
            EncoderConfig {
                height: 720,
                ..rates
            },
            EncoderConfig {
                gop_length: 60,
                ..rates
            },
            EncoderConfig {
                profile: H264Profile::High,
                ..rates
            },
            EncoderConfig {
                rc_mode: preset::RcMode::Vbr,
                ..rates
            },
            EncoderConfig { qp: 20, ..rates },
            EncoderConfig {
                initial_qp: 26,
                ..rates
            },
            EncoderConfig {
                min_qp: 10,
                ..rates
            },
            EncoderConfig {
                max_qp: 42,
                ..rates
            },
            EncoderConfig {
                repeat_sps_pps: false,
                ..rates
            },
        ] {
            assert!(!changed.is_rate_update_from(&before), "{changed:?}");
        }
    }
}
