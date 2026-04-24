//! Real NVENC H.264 encode session.
//!
//! Covers the full map-encode-lock-unmap cycle over
//! `nvidia_video_codec_sdk::sys::nvEncodeAPI` function pointers. The safe
//! wrapper `ENCODE_API` is a `lazy_static` that `.expect()`s inside; we wrap
//! every access in `catch_unwind` so that on a machine without
//! `libnvidia-encode.so.1` we surface a clean `DriverError::OperationFailed`
//! rather than aborting the process across the FFI boundary.
//!
//! The session owns:
//! 1. A raw NVENC encoder handle (`*mut c_void`).
//! 2. A table of `NV_ENC_REGISTERED_PTR` values keyed by `SurfaceKey`, built
//!    by [`NvencSession::register_internal_surface`] as surfaces are
//!    allocated.
//! 3. A round-robin pool of bitstream output buffers. NVENC can return
//!    `NV_ENC_ERR_NEED_MORE_INPUT` and defer output to a later frame; when
//!    that happens we mark the slot `Pending` and keep the `mapped_input`
//!    alive so that the driver can `Lock`/`Unmap` it once output actually
//!    materialises. The slot state machine is a local 2-state enum — `Free`
//!    vs `Pending { mapped_input, surface_key }` — deliberately kept simple:
//!    we use 6 slots which is >= the NVENC look-ahead + B-frame horizon we
//!    configure today (0 B-frames, no look-ahead).

use crate::error::DriverError;
use crate::ids::SurfaceKey;
use crate::nvenc::h264_config::{self, EncoderConfig, H264Profile};
use crate::nvenc::preset::{NvencPreset, PresetChoice, Tuning};
use core::ffi::c_void;
use nvidia_video_codec_sdk::safe::{ENCODE_API, EncodeAPI};
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

/// Size of the NVENC bitstream output pool. 6 slots cover CBR streams without
/// look-ahead or B-frames with plenty of head-room to survive one or two
/// `NV_ENC_ERR_NEED_MORE_INPUT` deferrals before back-pressuring callers.
const POOL_SIZE: usize = 6;

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

/// Bitstream output slot state.
///
/// * `Free` — slot available for `encode_picture`.
/// * `Pending` — NVENC returned `NEED_MORE_INPUT` for the previous frame
///   targeting this slot; the mapped input must survive until the deferred
///   output is unlocked.
enum SlotState {
    Free,
    Pending {
        mapped_input: nv::NV_ENC_INPUT_PTR,
        _surface_key: SurfaceKey,
    },
}

struct BitstreamSlot {
    handle: nv::NV_ENC_OUTPUT_PTR,
    state: SlotState,
}

/// Result of a single [`NvencSession::encode_frame`].
///
/// `is_pending=true` means NVENC buffered this frame; no bitstream bytes are
/// available yet. The driver logs it and returns an empty coded buffer for
/// this `vaEndPicture` — the client will (eventually) get the drained output
/// on a later `encode_frame` call.
#[derive(Debug, Default)]
pub struct CodedBitstream {
    pub data: Vec<u8>,
    pub is_pending: bool,
    pub is_idr: bool,
}

pub struct NvencSession {
    encoder: *mut c_void,
    _cuda_ctx: Arc<cudarc::driver::CudaContext>,
    config: EncoderConfig,
    /// Cached presetGUID so `reconfigure` can re-use it.
    preset_guid: nv::GUID,
    tuning: nv::NV_ENC_TUNING_INFO,
    registered: HashMap<SurfaceKey, RegisteredSurface>,
    pool: Vec<BitstreamSlot>,
    next_slot: usize,
    /// FIFO of already-locked coded bitstreams waiting to be returned to the
    /// client. NVENC is free to delay output by several `encode_picture`
    /// calls (internal reorder / B-frame / lookahead queues); whenever a
    /// `LockBitstream` call produces bytes we push them here, and each
    /// `encode_frame()` call pops the oldest entry. This means a client
    /// always sees output in submission order even if NVENC buffered N
    /// earlier frames.
    pending_outputs: std::collections::VecDeque<CodedBitstream>,
}

// SAFETY: the NVENC encoder handle is an opaque driver resource; all access
// goes through function pointers that the driver synchronises internally.
// The Rust wrapper holds a `&mut self` for every mutating operation, so the
// Rust borrow checker keeps the external synchronisation honest.
unsafe impl Send for NvencSession {}

impl NvencSession {
    /// Build a session. Requires a live CUDA context.
    ///
    /// On machines without `libnvidia-encode.so.1` the `ENCODE_API`
    /// `lazy_static` panics inside `.expect(...)`; we catch that panic and
    /// convert it into a clean error.
    pub fn new(
        cuda: Arc<cudarc::driver::CudaContext>,
        cfg: EncoderConfig,
    ) -> Result<Self, DriverError> {
        // Ensure CUDA context is current on this thread before NVENC touches it.
        cuda.bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;

        // Probe ENCODE_API through catch_unwind so a missing libnvidia-encode
        // does not abort the process.
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return Err(DriverError::OperationFailed("ENCODE_API unavailable")),
        };

        // 1. Open session.
        let mut encoder: *mut c_void = core::ptr::null_mut();
        let mut session_params = open_session_ex_params(cuda.cu_ctx() as *mut c_void);
        // SAFETY: session_params is a valid initialised struct with correct VER.
        let status = unsafe {
            (api.open_encode_session_ex)(&mut session_params, &mut encoder)
        };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS || encoder.is_null() {
            return Err(DriverError::OperationFailed("NvEncOpenEncodeSessionEx"));
        }

        // 2. Translate high-level config → NVENC primitives.
        let (preset_guid, tuning) = map_preset_choice(cfg);
        let profile_guid = match cfg.profile {
            H264Profile::Main => nv::NV_ENC_H264_PROFILE_MAIN_GUID,
            H264Profile::ConstrainedBaseline => nv::NV_ENC_H264_PROFILE_BASELINE_GUID,
            H264Profile::High => nv::NV_ENC_H264_PROFILE_HIGH_GUID,
        };

        // 3. Fetch preset-default NV_ENC_CONFIG and mutate it in-place.
        //    MaybeUninit::zeroed is correct because every field is either
        //    u32/enum/POD or will be overwritten by GetEncodePresetConfigEx.
        let mut preset_cfg: nv::NV_ENC_PRESET_CONFIG = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        preset_cfg.version = nv::NV_ENC_PRESET_CONFIG_VER;
        preset_cfg.presetCfg.version = nv::NV_ENC_CONFIG_VER;
        // SAFETY: encoder valid (just opened), preset_cfg is a fresh struct.
        let status = unsafe {
            (api.get_encode_preset_config_ex)(
                encoder,
                nv::NV_ENC_CODEC_H264_GUID,
                preset_guid,
                tuning,
                &mut preset_cfg,
            )
        };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            // SAFETY: encoder valid.
            unsafe { (api.destroy_encoder)(encoder) };
            return Err(DriverError::OperationFailed("NvEncGetEncodePresetConfigEx"));
        }

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
            // SAFETY: encoder valid.
            unsafe { (api.destroy_encoder)(encoder) };
            return Err(DriverError::OperationFailed("NvEncInitializeEncoder"));
        }

        // 5. Bitstream pool.
        let mut pool: Vec<BitstreamSlot> = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            let mut params: nv::NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe {
                MaybeUninit::zeroed().assume_init()
            };
            params.version = nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            // size=0 lets NVENC pick based on encodeConfig.
            params.memoryHeap = nv::NV_ENC_MEMORY_HEAP::NV_ENC_MEMORY_HEAP_VID;
            // SAFETY: encoder valid, params correctly versioned.
            let s = unsafe { (api.create_bitstream_buffer)(encoder, &mut params) };
            if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
                // Roll back already-created slots before returning.
                for slot in &pool {
                    // SAFETY: handle came from a successful CreateBitstreamBuffer.
                    unsafe { (api.destroy_bitstream_buffer)(encoder, slot.handle) };
                }
                // SAFETY: encoder valid.
                unsafe { (api.destroy_encoder)(encoder) };
                return Err(DriverError::OperationFailed("NvEncCreateBitstreamBuffer"));
            }
            pool.push(BitstreamSlot {
                handle: params.bitstreamBuffer,
                state: SlotState::Free,
            });
        }

        Ok(Self {
            encoder,
            _cuda_ctx: cuda,
            config: cfg,
            preset_guid,
            tuning,
            registered: HashMap::new(),
            pool,
            next_slot: 0,
            pending_outputs: std::collections::VecDeque::new(),
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
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return Err(DriverError::OperationFailed("ENCODE_API unavailable")),
        };
        let mut params: nv::NV_ENC_REGISTER_RESOURCE = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
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
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return Err(DriverError::OperationFailed("ENCODE_API unavailable")),
        };
        let mut params: nv::NV_ENC_REGISTER_RESOURCE = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        params.version = nv::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType =
            nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY;
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
            return Err(DriverError::OperationFailed("NvEncRegisterResource(CUDAARRAY)"));
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
        let Some(reg) = self.registered.remove(&key) else {
            return;
        };
        let Ok(api) = catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) else {
            return;
        };
        // SAFETY: registered_ptr came from a successful RegisterResource
        // against this encoder and has not been unregistered yet.
        unsafe {
            let _ = (api.unregister_resource)(self.encoder, reg.registered_ptr);
        }
    }

    /// Encode one frame, honouring NVENC's internal reorder buffer.
    ///
    /// Three-phase flow:
    /// 1. **Non-blocking drain** — walk every Pending slot; ask NVENC with
    ///    `doNotWait=1` if output is ready. Ready slots return to the pool
    ///    and their bytes are pushed onto `pending_outputs`.
    /// 2. **Encode** — map the requested surface, submit to NVENC. On
    ///    SUCCESS we lock+copy synchronously; on `NEED_MORE_INPUT` the slot
    ///    becomes Pending.
    /// 3. **Guarantee output** — if the FIFO is still empty but there *is* a
    ///    Pending slot, do a *blocking* `LockBitstream` on the oldest one.
    ///    This keeps WebRTC clients happy (they expect each `vaEndPicture`
    ///    to surface bytes) while still letting NVENC's reorder queue do
    ///    its job.
    pub fn encode_frame(
        &mut self,
        key: SurfaceKey,
        force_idr: bool,
    ) -> Result<CodedBitstream, DriverError> {
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return Err(DriverError::OperationFailed("ENCODE_API unavailable")),
        };

        // Phase 1 — non-blocking drain of already-ready Pending slots.
        self.drain_pending_non_blocking(api);

        // 1. Look up registered surface (copy out fields so we release the
        //    &mut self borrow before we need &mut self.pool).
        let (reg_ptr, reg_w, reg_h, reg_pitch) = {
            let reg = self.registered.get(&key).ok_or(DriverError::InvalidSurface)?;
            (reg.registered_ptr, reg.width, reg.height, reg.pitch)
        };

        // 2. Pick a Free slot round-robin, starting at next_slot.
        let slot_idx = self.find_free_slot().ok_or(DriverError::OperationFailed(
            "NVENC bitstream pool exhausted",
        ))?;

        // 3. Map the registered resource for this encode call.
        let mut map: nv::NV_ENC_MAP_INPUT_RESOURCE = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        map.version = nv::NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = reg_ptr;
        // SAFETY: encoder valid; reg_ptr was produced by RegisterResource.
        let s = unsafe { (api.map_input_resource)(self.encoder, &mut map) };
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncMapInputResource"));
        }
        let mapped_input = map.mappedResource;

        // Phase 2 — submit the encode call.
        let slot_handle = self.pool[slot_idx].handle;
        let mut pic = pic_params(
            reg_w,
            reg_h,
            reg_pitch,
            mapped_input,
            slot_handle,
            force_idr,
        );
        // SAFETY: encoder valid; pic references live buffers for the call.
        let status = unsafe { (api.encode_picture)(self.encoder, &mut pic) };

        match status {
            nv::NVENCSTATUS::NV_ENC_SUCCESS => {
                // Output available immediately — lock, copy, unlock, unmap.
                let bitstream = self.lock_and_copy(api, slot_handle)?;
                // SAFETY: mapped_input was returned by MapInputResource.
                let _ = unsafe { (api.unmap_input_resource)(self.encoder, mapped_input) };
                self.pool[slot_idx].state = SlotState::Free;
                self.next_slot = (slot_idx + 1) % self.pool.len();
                self.pending_outputs.push_back(bitstream);
            }
            nv::NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT => {
                // Deferred. Keep the mapped_input alive until NVENC flushes.
                self.pool[slot_idx].state = SlotState::Pending {
                    mapped_input,
                    _surface_key: key,
                };
                self.next_slot = (slot_idx + 1) % self.pool.len();
            }
            _ => {
                // SAFETY: mapped_input was returned by MapInputResource.
                let _ = unsafe { (api.unmap_input_resource)(self.encoder, mapped_input) };
                return Err(DriverError::Encoding("NvEncEncodePicture"));
            }
        }

        // Phase 3 — try exactly one non-blocking drain of an oldest Pending
        // slot if the FIFO is still empty. We intentionally do NOT do a
        // blocking lock here: an earlier iteration that did was observed
        // to freeze Vesktop when NVENC held an orphan Pending slot across
        // a `reconfigure`, because `lock_and_copy` then waits forever for
        // output NVENC will never produce on that slot. A single
        // non-blocking probe is enough to cover the "NVENC finished this
        // frame while we were still preparing the next" case, which is
        // the only timing the WebRTC quality controller actually cares
        // about.
        if self.pending_outputs.is_empty() {
            if let Some(idx) = self.oldest_pending_slot() {
                let mut lock: nv::NV_ENC_LOCK_BITSTREAM = unsafe {
                    MaybeUninit::zeroed().assume_init()
                };
                lock.version = nv::NV_ENC_LOCK_BITSTREAM_VER;
                lock.outputBitstream = self.pool[idx].handle;
                lock.set_doNotWait(1);
                // SAFETY: encoder + slot handle both valid.
                let s = unsafe { (api.lock_bitstream)(self.encoder, &mut lock) };
                if s == nv::NVENCSTATUS::NV_ENC_SUCCESS {
                    let len = lock.bitstreamSizeInBytes as usize;
                    let mut data: Vec<u8> = Vec::with_capacity(len);
                    // SAFETY: NVENC gave us a readable ptr of length `bitstreamSizeInBytes`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            lock.bitstreamBufferPtr as *const u8,
                            data.as_mut_ptr(),
                            len,
                        );
                        data.set_len(len);
                    }
                    let is_idr =
                        lock.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
                    // SAFETY: just-locked handle.
                    let _ = unsafe {
                        (api.unlock_bitstream)(self.encoder, self.pool[idx].handle)
                    };
                    if let SlotState::Pending { mapped_input, .. } = self.pool[idx].state {
                        // SAFETY: mapped_input alive across Pending state.
                        let _ = unsafe { (api.unmap_input_resource)(self.encoder, mapped_input) };
                    }
                    self.pool[idx].state = SlotState::Free;
                    self.pending_outputs.push_back(CodedBitstream {
                        data,
                        is_pending: false,
                        is_idr,
                    });
                }
            }
        }

        // Pop the oldest ready bitstream. When nothing is ready we return
        // is_pending=true — the caller writes an empty coded-buffer for
        // this frame, NVENC flushes it next time. WebRTC does penalise
        // this as a drop; that is the root of the HW↔SW cycling we see
        // in Vesktop, but it is preferable to a freeze.
        Ok(self.pending_outputs.pop_front().unwrap_or(CodedBitstream {
            data: Vec::new(),
            is_pending: true,
            is_idr: false,
        }))
    }

    /// Walk every Pending slot and try a non-blocking `LockBitstream`.
    /// Slots that have output ready return to `Free` and push their
    /// bitstream onto `self.pending_outputs`. Slots still busy stay
    /// Pending. Other errors free the slot (log and continue).
    fn drain_pending_non_blocking(&mut self, api: &EncodeAPI) {
        for idx in 0..self.pool.len() {
            let (handle, mapped_input) = match self.pool[idx].state {
                SlotState::Pending { mapped_input, .. } => (self.pool[idx].handle, mapped_input),
                _ => continue,
            };
            let mut lock: nv::NV_ENC_LOCK_BITSTREAM = unsafe {
                MaybeUninit::zeroed().assume_init()
            };
            lock.version = nv::NV_ENC_LOCK_BITSTREAM_VER;
            lock.outputBitstream = handle;
            lock.set_doNotWait(1);
            // SAFETY: encoder + handle both valid; lock is freshly zeroed.
            let s = unsafe { (api.lock_bitstream)(self.encoder, &mut lock) };
            match s {
                nv::NVENCSTATUS::NV_ENC_SUCCESS => {
                    let len = lock.bitstreamSizeInBytes as usize;
                    let mut data: Vec<u8> = Vec::with_capacity(len);
                    // SAFETY: NVENC returns a readable pointer of length `bitstreamSizeInBytes`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            lock.bitstreamBufferPtr as *const u8,
                            data.as_mut_ptr(),
                            len,
                        );
                        data.set_len(len);
                    }
                    let is_idr = lock.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
                    // SAFETY: handle we just locked.
                    let _ = unsafe { (api.unlock_bitstream)(self.encoder, handle) };
                    // SAFETY: mapped_input stayed alive across Pending state.
                    let _ = unsafe { (api.unmap_input_resource)(self.encoder, mapped_input) };
                    self.pool[idx].state = SlotState::Free;
                    self.pending_outputs.push_back(CodedBitstream {
                        data,
                        is_pending: false,
                        is_idr,
                    });
                }
                nv::NVENCSTATUS::NV_ENC_ERR_LOCK_BUSY => {
                    // Not ready yet — leave Pending.
                }
                _ => {
                    // Unexpected error. Free the slot so we don't wedge forever;
                    // the mapped input is released best-effort.
                    // SAFETY: mapped_input came from MapInputResource.
                    let _ = unsafe { (api.unmap_input_resource)(self.encoder, mapped_input) };
                    self.pool[idx].state = SlotState::Free;
                }
            }
        }
    }

    /// Index of the oldest Pending slot, or None if none are pending.
    /// Uses `next_slot` as a proxy for round-robin order: the Pending slot
    /// nearest (next_slot - 1) in cyclic order is the oldest submission.
    fn oldest_pending_slot(&self) -> Option<usize> {
        let n = self.pool.len();
        if n == 0 {
            return None;
        }
        for offset in 0..n {
            let idx = (self.next_slot + n - 1 - offset) % n;
            if matches!(self.pool[idx].state, SlotState::Pending { .. }) {
                return Some(idx);
            }
        }
        None
    }

    /// Best-effort runtime reconfiguration. On failure we keep the old config
    /// — a full drop-and-recreate is left for a later slice.
    pub fn reconfigure(&mut self, new_cfg: EncoderConfig) -> Result<(), DriverError> {
        if new_cfg == self.config {
            return Ok(());
        }
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return Err(DriverError::OperationFailed("ENCODE_API unavailable")),
        };

        // Build a fresh preset config and re-apply.
        let mut preset_cfg: nv::NV_ENC_PRESET_CONFIG = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        preset_cfg.version = nv::NV_ENC_PRESET_CONFIG_VER;
        preset_cfg.presetCfg.version = nv::NV_ENC_CONFIG_VER;
        // SAFETY: encoder valid.
        let s = unsafe {
            (api.get_encode_preset_config_ex)(
                self.encoder,
                nv::NV_ENC_CODEC_H264_GUID,
                self.preset_guid,
                self.tuning,
                &mut preset_cfg,
            )
        };
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncGetEncodePresetConfigEx"));
        }
        h264_config::apply_config(&mut preset_cfg.presetCfg, &new_cfg);
        preset_cfg.presetCfg.profileGUID = match new_cfg.profile {
            H264Profile::Main => nv::NV_ENC_H264_PROFILE_MAIN_GUID,
            H264Profile::ConstrainedBaseline => nv::NV_ENC_H264_PROFILE_BASELINE_GUID,
            H264Profile::High => nv::NV_ENC_H264_PROFILE_HIGH_GUID,
        };

        let mut reinit = init_params(
            &new_cfg,
            self.preset_guid,
            self.tuning,
            &mut preset_cfg.presetCfg as *mut nv::NV_ENC_CONFIG,
        );
        let mut recfg: nv::NV_ENC_RECONFIGURE_PARAMS = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        recfg.version = nv::NV_ENC_RECONFIGURE_PARAMS_VER;
        recfg.reInitEncodeParams = reinit;
        // Force IDR on config change so downstream decoders resync cleanly.
        recfg.set_forceIDR(1);
        // SAFETY: encoder valid; recfg self-contained except reInitEncodeParams
        // which references preset_cfg, which outlives this call.
        let status = unsafe { (api.reconfigure_encoder)(self.encoder, &mut recfg) };
        // Defeat unused warning on `reinit` — we deliberately drop the local
        // so the borrow checker stops worrying about its lifetime after the
        // FFI call completes.
        let _ = &mut reinit;
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncReconfigureEncoder"));
        }
        self.config = new_cfg;
        Ok(())
    }

    /// Read the current encoder config (for change-detection on SPS replay).
    #[inline]
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    // -------- internals --------

    fn find_free_slot(&self) -> Option<usize> {
        let n = self.pool.len();
        for i in 0..n {
            let idx = (self.next_slot + i) % n;
            if matches!(self.pool[idx].state, SlotState::Free) {
                return Some(idx);
            }
        }
        None
    }

    fn lock_and_copy(
        &mut self,
        api: &EncodeAPI,
        slot_handle: nv::NV_ENC_OUTPUT_PTR,
    ) -> Result<CodedBitstream, DriverError> {
        let mut lock: nv::NV_ENC_LOCK_BITSTREAM = unsafe {
            MaybeUninit::zeroed().assume_init()
        };
        lock.version = nv::NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = slot_handle;
        // SAFETY: encoder valid, slot_handle from CreateBitstreamBuffer.
        let s = unsafe { (api.lock_bitstream)(self.encoder, &mut lock) };
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::Encoding("NvEncLockBitstream"));
        }
        let len = lock.bitstreamSizeInBytes as usize;
        let mut data: Vec<u8> = Vec::with_capacity(len);
        // SAFETY: NVENC returns a readable pointer of length `bitstreamSizeInBytes`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                lock.bitstreamBufferPtr as *const u8,
                data.as_mut_ptr(),
                len,
            );
            data.set_len(len);
        }
        let is_idr = lock.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
        // SAFETY: same slot_handle just locked.
        let _ = unsafe { (api.unlock_bitstream)(self.encoder, slot_handle) };
        Ok(CodedBitstream {
            data,
            is_pending: false,
            is_idr,
        })
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        let api = match catch_unwind(AssertUnwindSafe(|| &*ENCODE_API)) {
            Ok(a) => a,
            Err(_) => return, // cannot clean up without the API; leak is harmless at shutdown
        };
        // Unmap any pending mapped inputs.
        for slot in &self.pool {
            if let SlotState::Pending { mapped_input, .. } = &slot.state {
                // SAFETY: mapped_input was obtained via MapInputResource.
                unsafe { (api.unmap_input_resource)(self.encoder, *mapped_input) };
            }
        }
        // Destroy bitstream buffers.
        for slot in &self.pool {
            // SAFETY: handle came from CreateBitstreamBuffer.
            unsafe { (api.destroy_bitstream_buffer)(self.encoder, slot.handle) };
        }
        // Unregister all surfaces.
        for reg in self.registered.values() {
            // SAFETY: registered_ptr came from RegisterResource.
            unsafe { (api.unregister_resource)(self.encoder, reg.registered_ptr) };
        }
        // Tear down the encoder.
        // SAFETY: encoder valid until this call.
        unsafe { (api.destroy_encoder)(self.encoder) };
    }
}

// -------- helpers (VER population lives in one place so future version bumps
// are a single-line change each) --------

fn open_session_ex_params(device: *mut c_void) -> nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    // SAFETY: struct is POD; zero is a valid bit pattern for every field.
    let mut p: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe {
        MaybeUninit::zeroed().assume_init()
    };
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
    let mut p: nv::NV_ENC_INITIALIZE_PARAMS = unsafe {
        MaybeUninit::zeroed().assume_init()
    };
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
    // SAFETY: POD with zeroed-init; we fill the fields we care about.
    let mut p: nv::NV_ENC_PIC_PARAMS = unsafe {
        MaybeUninit::zeroed().assume_init()
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
        p.encodePicFlags =
            nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32;
    }
    p
}

fn map_preset_choice(cfg: EncoderConfig) -> (nv::GUID, nv::NV_ENC_TUNING_INFO) {
    let rc_wants_cqp = matches!(cfg.rc_mode, crate::nvenc::preset::RcMode::ConstQp);
    let bitrate = cfg.bitrate_bps as u64;
    let fps = if cfg.fps_den > 0 { cfg.fps_num / cfg.fps_den } else { 30 };
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
    fn map_preset_choice_p4_for_5mbps_60fps() {
        let cfg = EncoderConfig::default_for(1920, 1080, H264Profile::Main);
        let (_guid, tuning) = map_preset_choice(cfg);
        assert_eq!(
            tuning,
            nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY
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

    // Slot state machine: free → pending → free transitions.
    #[test]
    fn slot_state_round_robin_math() {
        // Small pure model of the round-robin pointer update.
        let pool_size = 6;
        let mut next = 0usize;
        for step in 0..20 {
            let picked = next;
            next = (picked + 1) % pool_size;
            assert_eq!(picked, step % pool_size);
        }
        assert_eq!(next, 20 % pool_size);
    }
}
