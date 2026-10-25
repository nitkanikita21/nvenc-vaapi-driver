//! Bounded NVENC output processing. The submitting thread never waits on a
//! bitstream while a free output exists; one sleeping worker collects results.
use super::{api, lifetime::EncoderLease as Encoder};
use crate::{error::DriverError, ids::SurfaceKey};
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    mem::MaybeUninit,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_OUTPUTS: usize = 4;

struct Completion {
    result: Option<Result<(), DriverError>>,
    bytes: Vec<u8>,
}

pub(crate) struct EncodedFrame {
    pub surface: SurfaceKey,
    sequence: u64,
    completion: Mutex<Completion>,
    ready: Condvar,
}
impl EncodedFrame {
    fn new(surface: SurfaceKey, sequence: u64) -> Self {
        Self {
            surface,
            sequence,
            completion: Mutex::new(Completion {
                result: None,
                bytes: Vec::new(),
            }),
            ready: Condvar::new(),
        }
    }
    pub(crate) fn is_ready(&self) -> bool {
        self.completion.lock().result.is_some()
    }
    pub(crate) fn wait(&self, timeout_ns: u64) -> Result<(), DriverError> {
        let started = Instant::now();
        let mut completion = self.completion.lock();
        loop {
            if let Some(result) = &completion.result {
                return result.clone();
            }
            if timeout_ns == u64::MAX {
                self.ready.wait(&mut completion);
            } else {
                let remaining = Duration::from_nanos(timeout_ns).saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(DriverError::TimedOut);
                }
                self.ready.wait_for(&mut completion, remaining);
            }
        }
    }
    pub(crate) fn take_bytes(&self, timeout_ns: u64) -> Result<Vec<u8>, DriverError> {
        self.wait(timeout_ns)?;
        let bytes = std::mem::take(&mut self.completion.lock().bytes);
        if bytes.is_empty() {
            return Err(DriverError::Encoding("output was already consumed"));
        }
        Ok(bytes)
    }
    fn finish(&self, result: Result<(), DriverError>, bytes: Vec<u8>) {
        let mut completion = self.completion.lock();
        completion.bytes = bytes;
        completion.result = Some(result);
        self.ready.notify_all();
    }
}

impl Encoder {
    fn lock_and_copy(
        &self,
        slot_handle: nv::NV_ENC_OUTPUT_PTR,
        sequence: u64,
        data: &mut Vec<u8>,
    ) -> Result<(), DriverError> {
        let api = api::get()?;
        let mut storage = MaybeUninit::<nv::NV_ENC_LOCK_BITSTREAM>::zeroed();
        let lock = storage.as_mut_ptr();
        // SAFETY: keep output-only enums (including nonzero pictureStruct)
        // in raw storage; only read the size and pointer returned on success.
        unsafe {
            core::ptr::addr_of_mut!((*lock).version).write(nv::NV_ENC_LOCK_BITSTREAM_VER);
            core::ptr::addr_of_mut!((*lock).outputBitstream).write(slot_handle);
        }
        // SAFETY: encoder valid, slot_handle from CreateBitstreamBuffer.
        let s = crate::profiling::timed!(Lock, unsafe { (api.lock_bitstream)(self.handle, lock) });
        if s != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::Encoding("NvEncLockBitstream"));
        }
        // SAFETY: successful lock initialises these scalar output fields.
        let (len, pointer) = unsafe {
            (
                core::ptr::addr_of!((*lock).bitstreamSizeInBytes).read() as usize,
                core::ptr::addr_of!((*lock).bitstreamBufferPtr).read(),
            )
        };
        let result = (|| {
            // SAFETY: the locked output descriptor initialises its timestamp.
            if unsafe { core::ptr::addr_of!((*lock).outputTimeStamp).read() } != sequence {
                return Err(DriverError::Encoding("NVENC returned another picture"));
            }
            if len == 0 || pointer.is_null() {
                return Err(DriverError::Encoding("NVENC returned an empty picture"));
            }
            // Reuse the VA coded buffer allocation across frames. Only
            // compressed bytes cross this boundary; raw input stays on CUDA.
            data.clear();
            data.try_reserve(len)
                .map_err(|_| DriverError::AllocFailed)?;
            // SAFETY: NVENC returned a non-null, locked buffer of len bytes;
            // the destination has capacity for exactly those bytes.
            crate::profiling::timed!(BitstreamCopy, unsafe {
                core::ptr::copy_nonoverlapping(pointer.cast::<u8>(), data.as_mut_ptr(), len);
                data.set_len(len);
            });
            Ok(())
        })();
        // SAFETY: paired with a successful lock, also on allocation failure.
        let status = unsafe { (api.unlock_bitstream)(self.handle, slot_handle) };
        result?;
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(DriverError::OperationFailed("NvEncUnlockBitstream"));
        }
        Ok(())
    }
}
pub(super) struct OutputBuffer {
    pub handle: nv::NV_ENC_OUTPUT_PTR,
    encoder: Arc<Encoder>,
}
// SAFETY: ownership moves between the submitter, worker and free-buffer channel.
// The same output buffer is never accessed concurrently.
unsafe impl Send for OutputBuffer {}
impl Drop for OutputBuffer {
    fn drop(&mut self) {
        if self.encoder.cuda.bind_to_thread().is_ok() {
            if let Ok(api) = api::get() {
                // SAFETY: this uniquely owned output is no longer in flight.
                unsafe { (api.destroy_bitstream_buffer)(self.encoder.handle, self.handle) };
            }
        }
    }
}

struct Work {
    output: OutputBuffer,
    input: nv::NV_ENC_INPUT_PTR,
    frame: Arc<EncodedFrame>,
    bytes: Vec<u8>,
}
// SAFETY: input is a live NVENC mapping retained until this worker completes it;
// surface destruction/reuse waits for frame completion before touching memory.
unsafe impl Send for Work {}

pub(super) struct OutputRuntime {
    encoder: Arc<Encoder>,
    submit: Option<SyncSender<Work>>,
    free_tx: Sender<OutputBuffer>,
    free: Receiver<OutputBuffer>,
    worker: Option<JoinHandle<()>>,
    frames: VecDeque<Arc<EncodedFrame>>,
    failed: Arc<AtomicBool>,
}
impl OutputRuntime {
    /// Takes ownership of the initialised encoder, including on spawn failure.
    pub fn new(encoder: Encoder) -> Result<Self, DriverError> {
        let encoder = Arc::new(encoder);
        let (submit, incoming) = mpsc::sync_channel::<Work>(MAX_OUTPUTS);
        let (free_tx, free) = mpsc::channel();
        let returned = free_tx.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let failure_flag = failed.clone();
        // Allocate the complete bounded pool before any output processing can
        // overlap encoder resource creation.
        for _ in 0..MAX_OUTPUTS {
            let api = api::get()?;
            // SAFETY: all descriptor fields admit zero; version is set below.
            let mut params: nv::NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
            params.version = nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            params.memoryHeap = nv::NV_ENC_MEMORY_HEAP::NV_ENC_MEMORY_HEAP_VID;
            // SAFETY: encoder is initialised and has no submitted work.
            if unsafe { (api.create_bitstream_buffer)(encoder.handle, &mut params) }
                != nv::NVENCSTATUS::NV_ENC_SUCCESS
            {
                return Err(DriverError::OperationFailed("NvEncCreateBitstreamBuffer"));
            }
            free_tx
                .send(OutputBuffer {
                    handle: params.bitstreamBuffer,
                    encoder: encoder.clone(),
                })
                .map_err(|_| DriverError::OperationFailed("output pool closed"))?;
        }
        let worker = thread::Builder::new()
            .name("nvenc-output".into())
            .spawn(move || {
                for work in incoming {
                    process(work, &returned, &failure_flag);
                }
            })
            .map_err(|_| DriverError::AllocFailed)?;
        Ok(Self {
            encoder,
            submit: Some(submit),
            free_tx,
            free,
            worker: Some(worker),
            frames: VecDeque::new(),
            failed,
        })
    }
    pub fn acquire(&mut self) -> Result<OutputBuffer, DriverError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(DriverError::Encoding("output worker failed"));
        }
        self.frames.retain(|frame| !frame.is_ready());
        if let Ok(output) = self.free.try_recv() {
            return Ok(output);
        }
        self.free
            .recv()
            .map_err(|_| DriverError::OperationFailed("output worker stopped"))
    }

    pub fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
    pub fn recycle(&self, output: OutputBuffer) {
        let _ = self.free_tx.send(output);
    }
    pub fn enqueue(
        &mut self,
        output: OutputBuffer,
        input: nv::NV_ENC_INPUT_PTR,
        surface: SurfaceKey,
        sequence: u64,
        bytes: Vec<u8>,
    ) -> Result<Arc<EncodedFrame>, DriverError> {
        let frame = Arc::new(EncodedFrame::new(surface, sequence));
        let work = Work {
            output,
            input,
            frame: frame.clone(),
            bytes,
        };
        let submit = self
            .submit
            .as_ref()
            .ok_or(DriverError::OperationFailed("output worker stopped"))?;
        if let Err(error) = submit.send(work) {
            // Finish accepted GPU work even if the receiver stopped unexpectedly.
            process(error.0, &self.free_tx, &self.failed);
            return Err(DriverError::OperationFailed("output worker stopped"));
        }
        self.frames.push_back(frame.clone());
        Ok(frame)
    }
    pub fn wait_surface(&self, surface: SurfaceKey, timeout_ns: u64) -> Result<(), DriverError> {
        let start = Instant::now();
        for frame in &self.frames {
            if frame.surface == surface {
                let left = if timeout_ns == u64::MAX {
                    u64::MAX
                } else {
                    timeout_ns
                        .saturating_sub(start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
                };
                frame.wait(left)?;
            }
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(DriverError::Encoding("output worker failed"));
        }
        Ok(())
    }
    pub fn surface_ready(&self, surface: SurfaceKey) -> bool {
        self.frames
            .iter()
            .filter(|frame| frame.surface == surface)
            .all(|frame| frame.is_ready())
    }
    pub fn wait_all(&self) -> Result<(), DriverError> {
        for frame in &self.frames {
            frame.wait(u64::MAX)?;
        }
        Ok(())
    }
    pub fn drain(&self) {
        for frame in &self.frames {
            let _ = frame.wait(u64::MAX);
        }
    }
}
impl Drop for OutputRuntime {
    fn drop(&mut self) {
        drop(self.submit.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        while self.free.try_recv().is_ok() {}
    }
}

fn process(mut work: Work, returned: &Sender<OutputBuffer>, failed: &AtomicBool) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        work.output
            .encoder
            .cuda
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        work.output
            .encoder
            .lock_and_copy(work.output.handle, work.frame.sequence, &mut work.bytes)
    }))
    .unwrap_or(Err(DriverError::OperationFailed("output worker panicked")));
    // Always pair the successful input map, including output/allocation errors.
    let cleanup = (|| {
        work.output
            .encoder
            .cuda
            .bind_to_thread()
            .map_err(|_| DriverError::OperationFailed("cuda bind_to_thread"))?;
        let api = api::get()?;
        // SAFETY: this accepted work owns the input mapping until completion.
        if crate::profiling::timed!(InputUnmap, unsafe {
            (api.unmap_input_resource)(work.output.encoder.handle, work.input)
        }) != nv::NVENCSTATUS::NV_ENC_SUCCESS
        {
            return Err(DriverError::OperationFailed("NvEncUnmapInputResource"));
        }
        Ok(())
    })();
    let result = result.and(cleanup);
    if result.is_err() {
        failed.store(true, Ordering::Release);
    }
    let _ = returned.send(work.output);
    work.frame.finish(result, work.bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_does_not_consume_a_later_result() {
        let frame = Arc::new(EncodedFrame::new(SurfaceKey::default(), 0));
        assert!(matches!(frame.wait(0), Err(DriverError::TimedOut)));
        assert!(matches!(frame.wait(1_000_000), Err(DriverError::TimedOut)));
        frame.finish(Ok(()), vec![1, 2, 3]);
        assert_eq!(frame.take_bytes(0).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn completion_wakes_waiters_and_preserves_picture_association() {
        let first = Arc::new(EncodedFrame::new(SurfaceKey::default(), 0));
        let second = Arc::new(EncodedFrame::new(SurfaceKey::default(), 0));
        let waiting = first.clone();
        let waiter = std::thread::spawn(move || waiting.take_bytes(u64::MAX).unwrap());
        second.finish(Ok(()), vec![22]);
        first.finish(Ok(()), vec![11]);
        assert_eq!(second.take_bytes(0).unwrap(), [22]);
        assert_eq!(waiter.join().unwrap(), [11]);
    }

    #[test]
    fn worker_errors_remain_errors_for_all_waiters() {
        let frame = EncodedFrame::new(SurfaceKey::default(), 0);
        frame.finish(Err(DriverError::AllocFailed), Vec::new());
        assert!(matches!(frame.wait(0), Err(DriverError::AllocFailed)));
        assert!(matches!(
            frame.take_bytes(u64::MAX),
            Err(DriverError::AllocFailed)
        ));
    }
}
