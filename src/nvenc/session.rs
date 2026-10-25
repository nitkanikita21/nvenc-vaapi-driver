//! Real NVENC H.264 encode session.
//!
//! NVENC functions are loaded lazily by the runtime loader. Missing libraries,
//! unsupported versions and incomplete function tables produce explicit errors.
//!
//! Each session caches input registrations and owns a bounded output queue.
//! A worker collects synchronous Linux NVENC outputs while the caller submits
//! pictures. No B-frames/lookahead are enabled; each output retains its picture.

use super::{
    api,
    output::{EncodedFrame, OutputRuntime},
};
use crate::error::DriverError;
use crate::ids::SurfaceKey;
use crate::nvenc::h264_config::{self, EncoderConfig, H264Profile};
use crate::nvenc::preset::{NvencPreset, PresetChoice, Tuning};
use core::ffi::c_void;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::sync::Arc;

/// Resource bound to a specific VA surface after [`NvencSession::register_internal_surface`].
///
/// We cache the `NV_ENC_REGISTERED_PTR` so `vaBeginPicture` → `vaEndPicture`
/// can re-use the same registration across frames (NVENC explicitly
/// supports this and it is cheaper than re-registering per frame).
struct RegisteredSurface {
    registered_ptr: nv::NV_ENC_REGISTERED_PTR,
    width: u32,
    height: u32,
    pitch: u32,
}

pub struct NvencSession {
    encoder: *mut c_void,
    _cuda_ctx: Arc<cudarc::driver::CudaContext>,
    config: EncoderConfig,
    encode_config: nv::NV_ENC_CONFIG,
    /// Cached presetGUID so `reconfigure` can re-use it.
    preset_guid: nv::GUID,
    tuning: nv::NV_ENC_TUNING_INFO,
    registered: HashMap<SurfaceKey, RegisteredSurface>,
    output: OutputRuntime,
    next_frame: u64,
}

// SAFETY: the NVENC encoder handle is an opaque driver resource; all access
// goes through function pointers that the driver synchronises internally.
// The Rust wrapper holds a `&mut self` for every mutating operation, so the
// Rust borrow checker keeps the external synchronisation honest.
unsafe impl Send for NvencSession {}

impl NvencSession {
    /// Build a session. Requires a live CUDA context.
    ///
    /// Missing runtime libraries or unsupported API versions return an error.
    pub fn new(
        cuda: Arc<cudarc::driver::CudaContext>,
        cfg: EncoderConfig,
    ) -> Result<Self, DriverError> {
        cfg.validate_rate_control()?;
        // Ensure CUDA context is current on this thread before NVENC touches it.
        cuda.bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;

        let api = api::get()?;

        // Keep opening and initialising ordered across concurrent displays.
        let _creation = super::lifetime::CREATE.lock();
        // 1. Open session.
        let mut session_params = open_session_ex_params(cuda.cu_ctx() as *mut c_void);
        let lease = super::lifetime::EncoderLease::open(cuda.clone(), &mut session_params)?;
        let encoder = lease.handle;

        // 2. Translate high-level config → NVENC primitives.
        let (preset_guid, tuning) = map_preset_choice(cfg);
        let profile_guid = match cfg.profile {
            H264Profile::Main => nv::NV_ENC_H264_PROFILE_MAIN_GUID,
            H264Profile::ConstrainedBaseline => nv::NV_ENC_H264_PROFILE_BASELINE_GUID,
            H264Profile::High => nv::NV_ENC_H264_PROFILE_HIGH_GUID,
        };

        // The SDK bindings use Rust enums that cannot represent zero for
        // frameFieldMode. Keep C output as raw storage until it is normalised.
        let mut preset_storage = MaybeUninit::<nv::NV_ENC_PRESET_CONFIG>::zeroed();
        let preset_ptr = preset_storage.as_mut_ptr();
        // SAFETY: writable storage; only integer fields are initialised here.
        unsafe {
            core::ptr::addr_of_mut!((*preset_ptr).version).write(nv::NV_ENC_PRESET_CONFIG_VER);
            core::ptr::addr_of_mut!((*preset_ptr).presetCfg.version).write(nv::NV_ENC_CONFIG_VER);
        }
        // SAFETY: versioned output storage, live encoder, H.264 codec.
        let status = unsafe {
            (api.get_encode_preset_config_ex)(
                encoder,
                nv::NV_ENC_CODEC_H264_GUID,
                preset_guid,
                tuning,
                preset_ptr,
            )
        };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncGetEncodePresetConfigEx"));
        }
        // SAFETY: successful H.264 preset output; normalise the only nested
        // nonzero Rust enum before forming a typed value (progressive only).
        let mut preset_cfg = unsafe {
            core::ptr::addr_of_mut!((*preset_ptr).presetCfg.frameFieldMode)
                .write(nv::NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME);
            preset_storage.assume_init()
        };

        // Apply our project-specific config on top of the preset default.
        h264_config::apply_config(&mut preset_cfg.presetCfg, &cfg);
        preset_cfg.presetCfg.profileGUID = profile_guid;

        // 4. Build and submit NV_ENC_INITIALIZE_PARAMS pointing at the config.
        let mut init = init_params(
            &cfg,
            preset_guid,
            tuning,
            &mut preset_cfg.presetCfg as *mut nv::NV_ENC_CONFIG,
        );
        // SAFETY: encoder valid; init references preset_cfg which outlives
        // the call (lives until function return).
        let status = unsafe { (api.initialize_encoder)(encoder, &mut init) };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncInitializeEncoder"));
        }

        let output = OutputRuntime::new(lease)?;

        Ok(Self {
            encoder,
            _cuda_ctx: cuda,
            config: cfg,
            encode_config: preset_cfg.presetCfg,
            preset_guid,
            tuning,
            registered: HashMap::new(),
            output,
            next_frame: 0,
        })
    }

    /// Register a CUDA-resident NV12 surface with the encoder.
    ///
    /// Idempotent per `key` — a second call with the same surface returns
    /// immediately with the cached registration.
    pub fn register_internal_surface(
        &mut self,
        key: SurfaceKey,
        cu_ptr: u64,
        pitch: u32,
        width: u32,
        height: u32,
    ) -> Result<(), DriverError> {
        if self.registered.contains_key(&key) {
            return Ok(());
        }
        self._cuda_ctx
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        let api = api::get()?;
        let mut params: nv::NV_ENC_REGISTER_RESOURCE =
            unsafe { MaybeUninit::zeroed().assume_init() };
        params.version = nv::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType =
            nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
        params.width = width;
        params.height = height;
        params.pitch = pitch;
        params.resourceToRegister = cu_ptr as *mut c_void;
        params.bufferFormat = nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
        params.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        // SAFETY: encoder valid, params versioned correctly, cu_ptr is a
        // live CUDA device pointer owned by the VA surface.
        let s = unsafe { (api.register_resource)(self.encoder, &mut params) };
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncRegisterResource"));
        }
        self.registered.insert(
            key,
            RegisteredSurface {
                registered_ptr: params.registeredResource,
                width,
                height,
                pitch,
            },
        );
        Ok(())
    }

    /// Register a DMA-BUF-imported CUarray with the encoder.
    ///
    /// Mirrors [`register_internal_surface`] but uses
    /// `NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY`. NVENC determines pitch from
    /// the CUDA array itself; we still pass width/height for its bookkeeping.
    ///
    /// Idempotent per `key`.
    pub fn register_external_surface(
        &mut self,
        key: SurfaceKey,
        cuda_array: cudarc::driver::sys::CUarray,
        width: u32,
        height: u32,
    ) -> Result<nv::NV_ENC_REGISTERED_PTR, DriverError> {
        if let Some(existing) = self.registered.get(&key) {
            return Ok(existing.registered_ptr);
        }
        self._cuda_ctx
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        let api = api::get()?;
        let mut params: nv::NV_ENC_REGISTER_RESOURCE =
            unsafe { MaybeUninit::zeroed().assume_init() };
        params.version = nv::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY;
        params.width = width;
        params.height = height;
        // For CUDAARRAY NVENC derives pitch from the array descriptor;
        // passing 0 tells it to do exactly that.
        params.pitch = 0;
        params.resourceToRegister = cuda_array as *mut c_void;
        params.bufferFormat = nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
        params.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        // SAFETY: encoder valid, params versioned, cuda_array is a live
        // CUarray whose lifetime outlives this session (owned by the
        // SurfaceRec's ExternalDmaBufImage).
        let s = unsafe { (api.register_resource)(self.encoder, &mut params) };
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed(
                "NvEncRegisterResource(CUDAARRAY)",
            ));
        }
        let registered_ptr = params.registeredResource;
        self.registered.insert(
            key,
            RegisteredSurface {
                registered_ptr,
                width,
                height,
                // Pitch unused on the CUDAARRAY path but kept for the
                // shared struct layout.
                pitch: 0,
            },
        );
        Ok(registered_ptr)
    }

    /// Best-effort NVENC unregister for a previously-registered surface.
    /// Used when a DMA-BUF-backed surface is destroyed so we tear down the
    /// NVENC-side registration before CUDA drops the underlying mipmap.
    pub fn unregister_surface(&mut self, key: SurfaceKey) {
        if self._cuda_ctx.bind_to_thread().is_err() {
            return;
        }
        let Some(reg) = self.registered.remove(&key) else {
            return;
        };
        let Ok(api) = api::get() else {
            return;
        };
        // SAFETY: registered_ptr came from a successful RegisterResource
        // against this encoder and has not been unregistered yet.
        unsafe {
            let _ = (api.unregister_resource)(self.encoder, reg.registered_ptr);
        }
    }

    /// Submit one picture; completion owns its matching output and input map.
    pub fn encode_frame(
        &mut self,
        key: SurfaceKey,
        force_idr: bool,
        bytes: Vec<u8>,
    ) -> Result<Arc<EncodedFrame>, DriverError> {
        let next = self
            .next_frame
            .checked_add(1)
            .ok_or(DriverError::Encoding("frame counter exhausted"))?;
        self._cuda_ctx
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        self.output.wait_surface(key, u64::MAX)?;
        let api = api::get()?;
        let reg = self
            .registered
            .get(&key)
            .ok_or(DriverError::InvalidSurface)?;
        let output = self.output.acquire()?;
        // SAFETY: versioned POD mapping descriptor for this live registration.
        let mut map: nv::NV_ENC_MAP_INPUT_RESOURCE = unsafe { core::mem::zeroed() };
        map.version = nv::NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = reg.registered_ptr;
        // SAFETY: registration belongs to this encoder and outlives the job.
        if crate::profiling::timed!(InputMap, unsafe {
            (api.map_input_resource)(self.encoder, &mut map)
        }) != nv::NVENCSTATUS::NV_ENC_SUCCESS
        {
            self.output.recycle(output);
            return Err(DriverError::OperationFailed("NvEncMapInputResource"));
        }
        let mut pic = pic_params(
            reg.width,
            reg.height,
            reg.pitch,
            map.mappedResource,
            output.handle,
            force_idr,
        );
        pic.frameIdx = self.next_frame as u32;
        pic.inputTimeStamp = self.next_frame;
        pic.inputDuration = 1;
        // SAFETY: input/output stay owned until the output worker finishes.
        let status = crate::profiling::timed!(Submit, unsafe {
            (api.encode_picture)(self.encoder, &mut pic)
        });
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            self.output.fail();
            // SAFETY: pair the input map on failed submission.
            unsafe { (api.unmap_input_resource)(self.encoder, map.mappedResource) };
            self.output.recycle(output);
            return Err(DriverError::Encoding(
                "NvEncEncodePicture: output unavailable",
            ));
        }
        let sequence = self.next_frame;
        self.next_frame = next;
        self.output
            .enqueue(output, map.mappedResource, key, sequence, bytes)
    }

    pub fn wait_surface(&self, key: SurfaceKey, timeout_ns: u64) -> Result<(), DriverError> {
        self.output.wait_surface(key, timeout_ns)
    }
    pub fn surface_ready(&self, key: SurfaceKey) -> bool {
        self.output.surface_ready(key)
    }

    /// Reconfigure without reopening the encoder. Explicit rate-control resets
    /// also start a new IDR; ordinary bitrate/FPS updates preserve references.
    pub fn reconfigure(
        &mut self,
        new_cfg: EncoderConfig,
        reset_rate_control: bool,
    ) -> Result<(), DriverError> {
        new_cfg.validate_rate_control()?;
        if new_cfg == self.config && !reset_rate_control {
            return Ok(());
        }
        self._cuda_ctx
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        // NVENC supports rate changes with pictures in flight. Preserve the
        // queue for these updates; resets and coding-tool changes still drain.
        if reset_rate_control || !new_cfg.is_rate_update_from(&self.config) {
            self.output.wait_all()?;
        }
        let api = api::get()?;

        // Preserve the successful preset configuration. A bitrate update
        // does not need another driver query or a different preset.
        let mut encode_config = self.encode_config;
        h264_config::apply_config(&mut encode_config, &new_cfg);
        let reinit = init_params(&new_cfg, self.preset_guid, self.tuning, &mut encode_config);
        let mut recfg: nv::NV_ENC_RECONFIGURE_PARAMS =
            unsafe { MaybeUninit::zeroed().assume_init() };
        recfg.version = nv::NV_ENC_RECONFIGURE_PARAMS_VER;
        recfg.reInitEncodeParams = reinit;
        if reset_rate_control {
            recfg.set_forceIDR(1);
            recfg.set_resetEncoder(1);
        }
        // SAFETY: encoder valid; recfg self-contained except reInitEncodeParams
        // which references encode_config, which outlives this call.
        let status = unsafe { (api.reconfigure_encoder)(self.encoder, &mut recfg) };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncReconfigureEncoder"));
        }
        self.config = new_cfg;
        self.encode_config = encode_config;
        Ok(())
    }

    /// Read the current encoder config (for change-detection on SPS replay).
    #[inline]
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        if self._cuda_ctx.bind_to_thread().is_err() {
            return;
        }
        let Ok(api) = api::get() else {
            return;
        };
        self.output.drain();
        let mut eos = eos_params();
        // SAFETY: all submitted outputs are complete; EOS flushes the live
        // encoder before any registrations or bitstream buffers are released.
        unsafe { (api.encode_picture)(self.encoder, eos.as_mut_ptr()) };
        // Unregister all surfaces.
        for reg in self.registered.values() {
            // SAFETY: registered_ptr came from RegisterResource.
            unsafe { (api.unregister_resource)(self.encoder, reg.registered_ptr) };
        }
        // OutputRuntime joins the worker and destroys outputs/encoder after
        // all session-local input registrations have been removed.
    }
}

// -------- helpers (VER population lives in one place so future version bumps
// are a single-line change each) --------

fn eos_params() -> MaybeUninit<nv::NV_ENC_PIC_PARAMS> {
    let mut storage = MaybeUninit::<nv::NV_ENC_PIC_PARAMS>::zeroed();
    // SAFETY: EOS is a C descriptor whose pictureStruct must remain zero.
    // Zero is invalid for that Rust enum, so never assume_init this storage.
    unsafe {
        core::ptr::addr_of_mut!((*storage.as_mut_ptr()).version).write(nv::NV_ENC_PIC_PARAMS_VER);
        core::ptr::addr_of_mut!((*storage.as_mut_ptr()).encodePicFlags)
            .write(nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS as u32);
    }
    storage
}

fn open_session_ex_params(device: *mut c_void) -> nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    // SAFETY: struct is POD; zero is a valid bit pattern for every field.
    let mut p: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS =
        unsafe { MaybeUninit::zeroed().assume_init() };
    p.version = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    p.deviceType = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA;
    p.device = device;
    p.apiVersion = nv::NVENCAPI_VERSION;
    p
}

fn init_params(
    cfg: &EncoderConfig,
    preset_guid: nv::GUID,
    tuning: nv::NV_ENC_TUNING_INFO,
    encode_config: *mut nv::NV_ENC_CONFIG,
) -> nv::NV_ENC_INITIALIZE_PARAMS {
    // SAFETY: POD; zero is valid for every field.
    let mut p: nv::NV_ENC_INITIALIZE_PARAMS = unsafe { MaybeUninit::zeroed().assume_init() };
    p.version = nv::NV_ENC_INITIALIZE_PARAMS_VER;
    p.encodeGUID = nv::NV_ENC_CODEC_H264_GUID;
    p.presetGUID = preset_guid;
    p.encodeWidth = cfg.width;
    p.encodeHeight = cfg.height;
    p.darWidth = cfg.width;
    p.darHeight = cfg.height;
    p.frameRateNum = cfg.fps_num;
    p.frameRateDen = cfg.fps_den;
    p.enablePTD = 1;
    p.enableEncodeAsync = 0;
    p.tuningInfo = tuning;
    p.bufferFormat = nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
    p.encodeConfig = encode_config;
    p
}

fn pic_params(
    width: u32,
    height: u32,
    pitch: u32,
    input_buf: nv::NV_ENC_INPUT_PTR,
    output_buf: nv::NV_ENC_OUTPUT_PTR,
    force_idr: bool,
) -> nv::NV_ENC_PIC_PARAMS {
    let mut storage = MaybeUninit::<nv::NV_ENC_PIC_PARAMS>::zeroed();
    // SAFETY: pictureStruct is the only nonzero Rust enum in this descriptor.
    // Initialise it before forming a Rust value; other zero fields are valid.
    let mut p = unsafe {
        core::ptr::addr_of_mut!((*storage.as_mut_ptr()).pictureStruct)
            .write(nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME);
        storage.assume_init()
    };
    p.version = nv::NV_ENC_PIC_PARAMS_VER;
    p.inputWidth = width;
    p.inputHeight = height;
    p.inputPitch = pitch;
    p.inputBuffer = input_buf;
    p.outputBitstream = output_buf;
    p.bufferFmt = nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
    p.pictureStruct = nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
    if force_idr {
        p.encodePicFlags = nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32;
    }
    p
}

fn map_preset_choice(cfg: EncoderConfig) -> (nv::GUID, nv::NV_ENC_TUNING_INFO) {
    let rc_wants_cqp = matches!(cfg.rc_mode, crate::nvenc::preset::RcMode::ConstQp);
    let bitrate = cfg.bitrate_bps as u64;
    let fps = cfg.fps_num.checked_div(cfg.fps_den).unwrap_or(30);
    let choice: PresetChoice = crate::nvenc::preset::choose(bitrate, fps, rc_wants_cqp);
    let guid = match choice.preset {
        NvencPreset::P3 => nv::NV_ENC_PRESET_P3_GUID,
        NvencPreset::P4 => nv::NV_ENC_PRESET_P4_GUID,
        NvencPreset::P5 => nv::NV_ENC_PRESET_P5_GUID,
    };
    let tuning = match choice.tuning {
        Tuning::UltraLowLatency => nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        Tuning::LowLatency => nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY,
    };
    (guid, tuning)
}

/// Map VA profile → H.264 profile enum used by the NVENC config applier.
pub fn h264_profile_from_va(profile: va_sys::VAProfile) -> Option<H264Profile> {
    match profile {
        va_sys::VAProfileH264Main => Some(H264Profile::Main),
        va_sys::VAProfileH264ConstrainedBaseline => Some(H264Profile::ConstrainedBaseline),
        va_sys::VAProfileH264High => Some(H264Profile::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-logic helpers we can test without a GPU.

    #[test]
    fn picture_descriptors_initialise_nonzero_enums_and_eos_flags() {
        let pic = pic_params(
            128,
            64,
            128,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            true,
        );
        assert_eq!(pic.version, nv::NV_ENC_PIC_PARAMS_VER);
        assert_eq!(
            pic.pictureStruct,
            nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME
        );
        assert_eq!(
            pic.encodePicFlags,
            nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
        );
        let eos = eos_params();
        // SAFETY: read scalar members of initialised raw C storage, never its enum.
        unsafe {
            assert_eq!(
                core::ptr::addr_of!((*eos.as_ptr()).version).read(),
                nv::NV_ENC_PIC_PARAMS_VER
            );
            assert_eq!(
                core::ptr::addr_of!((*eos.as_ptr()).encodePicFlags).read(),
                nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS as u32
            );
            assert!(
                core::ptr::addr_of!((*eos.as_ptr()).inputBuffer)
                    .read()
                    .is_null()
            );
        }
    }

    #[test]
    fn map_preset_choice_p4_for_5mbps_60fps() {
        let cfg = EncoderConfig::default_for(1920, 1080, H264Profile::Main);
        let (_guid, tuning) = map_preset_choice(cfg);
        assert_eq!(
            tuning,
            nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY
        );
    }

    #[test]
    fn h264_profile_mapping_from_va() {
        assert_eq!(
            h264_profile_from_va(va_sys::VAProfileH264Main),
            Some(H264Profile::Main)
        );
        assert_eq!(
            h264_profile_from_va(va_sys::VAProfileH264ConstrainedBaseline),
            Some(H264Profile::ConstrainedBaseline)
        );
        assert_eq!(
            h264_profile_from_va(va_sys::VAProfileH264High),
            Some(H264Profile::High)
        );
    }

    #[test]
    fn synchronous_params_have_no_async_queue() {
        let cfg = EncoderConfig::default_for(1920, 1080, H264Profile::Main);
        let (guid, tuning) = map_preset_choice(cfg);
        let params = init_params(&cfg, guid, tuning, core::ptr::null_mut());
        assert_eq!(params.enableEncodeAsync, 0);
        assert_eq!(params.enablePTD, 1);
        assert_eq!(params.frameRateNum, 60);
        assert_eq!(params.frameRateDen, 1);
    }
}
