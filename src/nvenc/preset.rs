//! Mapping from VA-level encode parameters to NVENC presets, tunings, and RC modes.
//!
//! This module is pure data logic: it does not depend on any NVENC or CUDA
//! types and can be compiled and tested on any machine. The actual preset
//! values are fed to `NvEncInitializeEncoder` once the NVENC backend is wired
//! in (`src/nvenc/session.rs`).
//!
//! # Preset selection table
//!
//! | Condition | Preset | Tuning | RC | AQ |
//! |-----------|--------|--------|----|----|
//! | `rc_wants_cqp` (any bitrate / fps) | P4 | LowLatency | ConstQP | no |
//! | bitrate ≤ 2.5 Mbps **and** fps ≤ 30 | P3 | UltraLowLatency | CBR | no |
//! | 2.5 Mbps < bitrate ≤ 6 Mbps **or** fps ≤ 60 | P4 | LowLatency | CBR | yes |
//! | bitrate > 6 Mbps **or** fps > 60 | P5 | LowLatency | CBR | yes |
//!
//! The P3/ULL/CBR row targets Discord's "720p @ 30 fps, 2 Mbps" screenshare
//! preset, where NVENC's ultra-low-latency tuning minimises encode-to-decode
//! delay at the expense of slightly lower quality. AQ (Adaptive Quantization)
//! is disabled there to reduce the per-frame decision overhead, which matters
//! at 30 fps where the encoder has ≈33 ms to finish before the next frame
//! arrives.
//!
//! NVENC presets P3–P7 are available from driver 520.x onwards
//! (NVENC SDK 12.0). See the
//! [NVENC Programming Guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/12.0/nvenc-programming-guide/)
//! for the full preset speed/quality tradeoff table.

/// NVENC encode preset (speed vs. quality tradeoff).
///
/// Maps to `NV_ENC_PRESET_P3_GUID` / `P4` / `P5` in the NVENC SDK. Only the
/// three presets used by the selection table are represented here; additional
/// variants can be added when higher-quality encode paths are needed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NvencPreset {
    /// Fast preset, optimised for real-time low-bitrate screenshare (≤ 2.5 Mbps).
    P3,
    /// Balanced preset for mid-range bitrates (2.5–6 Mbps) and up to 60 fps.
    P4,
    /// Higher-quality preset for source-quality or high-framerate capture.
    P5,
}

/// NVENC tuning hint, controls latency vs. quality tradeoffs inside the preset.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tuning {
    /// Minimises encode-side latency at the cost of slightly lower quality.
    /// Maps to `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
    UltraLowLatency,
    /// Balanced latency tuning suitable for interactive screenshare at 60 fps.
    /// Maps to `NV_ENC_TUNING_INFO_LOW_LATENCY`.
    LowLatency,
}

/// Rate control mode to pass to NVENC.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RcMode {
    /// Constant bitrate. Primary mode for Discord screenshare.
    Cbr,
    /// CBR with lookahead (higher quality, slightly more latency).
    CbrLookahead,
    /// Constant QP — quality-driven, ignores bitrate target.
    ConstQp,
    /// Variable bitrate.
    Vbr,
}

/// The complete set of NVENC parameters chosen for one encode session.
#[derive(Copy, Clone, Debug)]
pub struct PresetChoice {
    pub preset: NvencPreset,
    pub tuning: Tuning,
    pub rc: RcMode,
    /// Whether to enable Adaptive Quantization. Reduces blocking at the cost
    /// of per-macroblock overhead; disabled at very low bitrates / fps.
    pub enable_aq: bool,
}

/// Select the NVENC preset, tuning, and RC mode for the given encode parameters.
///
/// `bitrate_bps` is the target bitrate in bits per second as negotiated via
/// `VAConfigAttribRateControl` / `VAEncMiscParameterRateControl`. `fps` is
/// the frame rate (integer; fractional rates round down). `rc_wants_cqp` is
/// true when the client selected `VA_RC_CQP`.
///
/// See the [module-level table](self) for the full decision matrix.
pub fn choose(bitrate_bps: u64, fps: u32, rc_wants_cqp: bool) -> PresetChoice {
    if rc_wants_cqp {
        return PresetChoice {
            preset: NvencPreset::P4,
            tuning: Tuning::LowLatency,
            rc: RcMode::ConstQp,
            enable_aq: false,
        };
    }
    if bitrate_bps <= 2_500_000 && fps <= 30 {
        PresetChoice {
            preset: NvencPreset::P3,
            tuning: Tuning::UltraLowLatency,
            rc: RcMode::Cbr,
            enable_aq: false,
        }
    } else if bitrate_bps <= 6_000_000 && fps <= 60 {
        PresetChoice {
            preset: NvencPreset::P4,
            tuning: Tuning::LowLatency,
            rc: RcMode::Cbr,
            enable_aq: true,
        }
    } else {
        PresetChoice {
            preset: NvencPreset::P5,
            tuning: Tuning::LowLatency,
            rc: RcMode::Cbr,
            enable_aq: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_720p30_smoother() {
        let c = choose(2_000_000, 30, false);
        assert_eq!(c.preset, NvencPreset::P3);
        assert_eq!(c.tuning, Tuning::UltraLowLatency);
        assert_eq!(c.rc, RcMode::Cbr);
    }

    #[test]
    fn discord_1080p60_source() {
        let c = choose(8_000_000, 60, false);
        assert_eq!(c.preset, NvencPreset::P5);
        assert!(c.enable_aq);
    }

    #[test]
    fn cqp_override() {
        let c = choose(5_000_000, 60, true);
        assert_eq!(c.rc, RcMode::ConstQp);
    }

    // ---------- Exhaustive table tests from the plan ----------
    //
    // Plan table:
    //   bitrate ≤ 2.5 Mbps, fps ≤ 30 -> P3 + ULL + CBR
    //   2.5–6 Mbps, fps ≤ 60         -> P4 + LL  + CBR + AQ
    //   > 6 Mbps or "Source"         -> P5 + LL  + CBR + AQ
    //   client wants CQP             -> P4 + LL  + ConstQp
    //
    // The "2.5 Mbps boundary" is inclusive of the P3 row per the current
    // impl (`<= 2_500_000`), and the "6 Mbps boundary" is inclusive of the
    // P4 row (`<= 6_000_000`). Boundary cases are pinned here so any future
    // refactor has to acknowledge where the break points are.

    struct Row {
        name: &'static str,
        bitrate_bps: u64,
        fps: u32,
        cqp: bool,
        expect_preset: NvencPreset,
        expect_tuning: Tuning,
        expect_rc: RcMode,
        expect_aq: bool,
    }

    fn assert_row(r: &Row) {
        let got = choose(r.bitrate_bps, r.fps, r.cqp);
        assert_eq!(got.preset, r.expect_preset, "row={}: preset", r.name);
        assert_eq!(got.tuning, r.expect_tuning, "row={}: tuning", r.name);
        assert_eq!(got.rc, r.expect_rc, "row={}: rc", r.name);
        assert_eq!(got.enable_aq, r.expect_aq, "row={}: enable_aq", r.name);
    }

    #[test]
    fn plan_table_1mbps_30fps() {
        assert_row(&Row {
            name: "1 Mbps @ 30 fps",
            bitrate_bps: 1_000_000, fps: 30, cqp: false,
            expect_preset: NvencPreset::P3,
            expect_tuning: Tuning::UltraLowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: false,
        });
    }

    #[test]
    fn plan_table_5mbps_60fps() {
        assert_row(&Row {
            name: "5 Mbps @ 60 fps",
            bitrate_bps: 5_000_000, fps: 60, cqp: false,
            expect_preset: NvencPreset::P4,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: true,
        });
    }

    #[test]
    fn plan_table_10mbps_60fps() {
        assert_row(&Row {
            name: "10 Mbps @ 60 fps",
            bitrate_bps: 10_000_000, fps: 60, cqp: false,
            expect_preset: NvencPreset::P5,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: true,
        });
    }

    #[test]
    fn plan_table_cqp_overrides_to_p4_ll_constqp() {
        assert_row(&Row {
            name: "CQP override",
            bitrate_bps: 4_000_000, fps: 60, cqp: true,
            expect_preset: NvencPreset::P4,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::ConstQp,
            expect_aq: false,
        });
    }

    // ---- Boundary cases (pin the <= semantics) ----

    #[test]
    fn boundary_exactly_2_5mbps_at_30fps_is_p3() {
        // The plan writes "≤ 2.5 Mbps" so exactly 2.5M must still land on P3+ULL.
        assert_row(&Row {
            name: "2.5 Mbps @ 30 fps (lower-row boundary)",
            bitrate_bps: 2_500_000, fps: 30, cqp: false,
            expect_preset: NvencPreset::P3,
            expect_tuning: Tuning::UltraLowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: false,
        });
    }

    #[test]
    fn boundary_just_over_2_5mbps_is_p4() {
        assert_row(&Row {
            name: "2.500001 Mbps @ 30 fps",
            bitrate_bps: 2_500_001, fps: 30, cqp: false,
            expect_preset: NvencPreset::P4,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: true,
        });
    }

    #[test]
    fn boundary_exactly_6mbps_at_60fps_is_p4() {
        // The plan writes "2.5–6 Mbps" — exactly 6M still on P4 per impl.
        assert_row(&Row {
            name: "6 Mbps @ 60 fps (upper P4 boundary)",
            bitrate_bps: 6_000_000, fps: 60, cqp: false,
            expect_preset: NvencPreset::P4,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: true,
        });
    }

    #[test]
    fn boundary_just_over_6mbps_is_p5() {
        assert_row(&Row {
            name: "6.000001 Mbps @ 60 fps",
            bitrate_bps: 6_000_001, fps: 60, cqp: false,
            expect_preset: NvencPreset::P5,
            expect_tuning: Tuning::LowLatency,
            expect_rc: RcMode::Cbr,
            expect_aq: true,
        });
    }

    #[test]
    fn low_bitrate_but_fps_over_30_escalates_to_p4() {
        // fps > 30 knocks us out of the P3 row even at tiny bitrate.
        let c = choose(500_000, 60, false);
        assert_eq!(c.preset, NvencPreset::P4);
    }

    #[test]
    fn cqp_wins_even_when_bitrate_is_source_tier() {
        // CQP override must short-circuit regardless of bitrate band.
        let c = choose(50_000_000, 120, true);
        assert_eq!(c.preset, NvencPreset::P4);
        assert_eq!(c.tuning, Tuning::LowLatency);
        assert_eq!(c.rc, RcMode::ConstQp);
        assert!(!c.enable_aq);
    }

    #[test]
    fn zero_bitrate_and_zero_fps_still_returns_a_choice() {
        // Defensive: degenerate inputs must not panic, and must land in the
        // P3 branch (both conditions of the lowest row are satisfied).
        let c = choose(0, 0, false);
        assert_eq!(c.preset, NvencPreset::P3);
        assert_eq!(c.rc, RcMode::Cbr);
    }
}
