//! NVENC backend adapter. Placeholder: the MVP compiles without the real
//! NVENC dependency so we can verify the VAAPI ABI boundary on any machine.
//!
//! The plan is to wire `nvidia-video-codec-sdk = "0.4"` (safe NVENC) +
//! `cudarc = "0.19"` here. The public surface of this module is intentionally
//! narrow so that swapping the stub for the real implementation is a local
//! change:
//!
//!   * `NvencSession::new(cuda, params) -> Result<Self>`
//!   * `session.register_surface(...)` – returns a cached NV_ENC_REGISTERED_PTR
//!   * `session.encode_frame(pic_params, registered) -> BitstreamView`
//!
//! See `preset.rs` for the bitrate/fps -> preset+tuning+RC mapping that does
//! *not* depend on NVENC types and is therefore implemented properly today.

#![allow(dead_code)]

pub mod h264_config;
pub mod preset;
pub mod session;

pub use session::NvencSession;
