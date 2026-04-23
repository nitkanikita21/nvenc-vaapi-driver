//! Root driver state, owned by `VADriverContext::pDriverData` via `Box`.

use crate::cuda::CudaCtx;
use crate::nvenc::NvencSession;
use parking_lot::Mutex;
use slotmap::DenseSlotMap;
use va_sys as va;

use crate::ids::{BufferKey, ConfigKey, ContextKey, ImageKey, SurfaceKey};

// Configuration record (one per vaCreateConfig).
pub struct ConfigRec {
    pub profile: va::VAProfile,
    pub entrypoint: va::VAEntrypoint,
    pub rt_format: u32,
    pub rc_mode: u32,
    pub packed_headers: u32,
}

// Surface record – either an internally-allocated CUDA resident buffer or an
// imported DMA-BUF (zero-copy path).
pub struct SurfaceRec {
    pub width: u32,
    pub height: u32,
    pub format: u32, // VA_RT_FORMAT_*
    pub kind: SurfaceKind,
    /// Cached NVENC-registered resource handle; populated on first use.
    pub registered: Mutex<Option<usize>>,
    /// CUDA context that owns the `Internal` allocation, retained so Drop
    /// can bind it before calling `cuMemFree_v2`. `None` for `Stub` /
    /// `DmaBuf` kinds where there is nothing CUDA-resident to free.
    pub cuda_ctx: Option<std::sync::Arc<cudarc::driver::CudaContext>>,
}

pub enum SurfaceKind {
    /// Owned CUDA device pitched allocation.
    CudaDevice { device_ptr: u64, pitch: u32 },
    /// Internally-allocated (cuMemAllocPitch) NV12 surface. Populated in
    /// slice c.2; зараз тримається тут лише як структурна заготовка, щоб
    /// потоки коду, які мають звертатися до pitched-буфера, вже могли
    /// pattern-match-ити варіант.
    ///
    /// `cu_ptr` зберігається як `u64` (CUdeviceptr), щоб не тягнути cudarc-типи
    /// у цей модуль.
    Internal { cu_ptr: u64, pitch: u32, width: u32, height: u32 },
    /// Imported DMA-BUF (dup'd fd kept alive here).
    DmaBuf { fd: libc::c_int },
    /// Placeholder until the backend is wired up.
    Stub,
}

impl Drop for SurfaceRec {
    fn drop(&mut self) {
        // Release backing storage. We never panic from Drop.
        match self.kind {
            SurfaceKind::DmaBuf { fd } => {
                // SAFETY: we own the dup'd fd.
                unsafe { libc::close(fd) };
            }
            SurfaceKind::Internal { cu_ptr, .. } => {
                if let Some(ctx) = &self.cuda_ctx {
                    // Best-effort: bind the owning context so the free
                    // targets the right address space. Any error is logged
                    // by cuMemFree's own return value and ignored here —
                    // Drop must not unwind.
                    let _ = ctx.bind_to_thread();
                    // SAFETY: cu_ptr came from a successful cuMemAllocPitch_v2
                    // against this same CUDA context.
                    unsafe {
                        let _ = cudarc::driver::sys::cuMemFree_v2(
                            cu_ptr as cudarc::driver::sys::CUdeviceptr,
                        );
                    }
                }
            }
            SurfaceKind::CudaDevice { .. } | SurfaceKind::Stub => {}
        }
    }
}

pub struct ContextRec {
    pub config: ConfigKey,
    pub width: u32,
    pub height: u32,
    pub flag: i32,
    pub render_targets: Vec<SurfaceKey>,
    pub current_target: Mutex<Option<SurfaceKey>>,
    pub session: Mutex<Option<NvencSession>>,
    /// Per-frame parameter accumulator between vaBeginPicture/vaEndPicture.
    pub pending: Mutex<PendingFrame>,
    /// Default coded-output slot, populated in vaEndPicture.
    pub last_coded: Mutex<Option<BufferKey>>,
}

#[derive(Default)]
pub struct PendingFrame {
    pub seq_param: Option<Vec<u8>>,
    pub pic_param: Option<Vec<u8>>,
    pub slice_param: Option<Vec<u8>>,
    pub packed_headers: Vec<(u32, Vec<u8>)>,
    pub coded_buf: Option<BufferKey>,
}

/// Coded-bitstream output slot.
///
/// Реальний bitstream пишеться у `backing` під час `vaEndPicture` (slice c.2).
/// `segment` — pinned box із `VACodedBufferSegment`, який мапиться клієнту
/// при `vaMapBuffer`: поле `buf` у ньому має вказувати на стабільну
/// адресу перших байтів `backing`. Boxing-ом ми гарантуємо, що сам
/// `VACodedBufferSegment` не переїжджає між Map-ами.
pub struct CodedBufferSlot {
    pub backing: Mutex<Vec<u8>>,
    pub segment: Mutex<Option<Box<va::VACodedBufferSegment>>>,
}

/// Backing storage for a buffer. Parameter-buffers (SPS/PPS/slice/misc)
/// живуть у `Generic`, coded-output — у `Coded`.
pub enum BufferStorage {
    Generic(Mutex<Vec<u8>>),
    Coded(CodedBufferSlot),
}

impl BufferStorage {
    /// Зручний доступ до байтів для параметричних буферів та legacy
    /// code path-ів. Для `Coded` повертає backing-vec (map-path у
    /// `driver::buffer` обробляється окремо).
    #[inline]
    pub fn bytes(&self) -> &Mutex<Vec<u8>> {
        match self {
            BufferStorage::Generic(m) => m,
            BufferStorage::Coded(slot) => &slot.backing,
        }
    }
}

/// Generic buffer handed to the client (holds bytes for parameter buffers,
/// or a pre-sized Vec<u8> for CodedBuffer output).
pub struct BufferRec {
    pub buf_type: va::VABufferType,
    pub element_size: u32,
    pub num_elements: u32,
    pub storage: BufferStorage,
    /// Coded-segment header scratch; for EncCodedBufferType we hand back a
    /// VACodedBufferSegment chain written at the head of `storage`.
    pub coded_ready: Mutex<bool>,
}

/// VA image record. Either a standalone `vaCreateImage` allocation
/// (`surface_key = None`) or a derived image bound to a specific surface
/// (`surface_key = Some`). In both cases the pixel bytes live in a generic
/// buffer slot referenced by `buffer_key`; on `vaUnmapBuffer` of a derived
/// image we trigger a host→device `cuMemcpy2D_v2` into the owning surface.
pub struct ImageRec {
    pub surface_key: Option<SurfaceKey>,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub data_size: u32,
    pub buffer_key: BufferKey,
}

pub struct Pools {
    pub configs: DenseSlotMap<ConfigKey, ConfigRec>,
    pub surfaces: DenseSlotMap<SurfaceKey, SurfaceRec>,
    pub contexts: DenseSlotMap<ContextKey, ContextRec>,
    pub buffers: DenseSlotMap<BufferKey, BufferRec>,
    pub images: DenseSlotMap<ImageKey, ImageRec>,
}

impl Pools {
    fn new() -> Self {
        Self {
            configs: DenseSlotMap::with_key(),
            surfaces: DenseSlotMap::with_key(),
            contexts: DenseSlotMap::with_key(),
            buffers: DenseSlotMap::with_key(),
            images: DenseSlotMap::with_key(),
        }
    }
}

pub struct DriverState {
    pub cuda: CudaCtx,
    pub pools: Mutex<Pools>,
}

impl DriverState {
    pub fn new(ctx: va::VADriverContextP) -> Result<Self, crate::error::DriverError> {
        Ok(Self {
            cuda: CudaCtx::from_va_context(ctx)?,
            pools: Mutex::new(Pools::new()),
        })
    }
}
