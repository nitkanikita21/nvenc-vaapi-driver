//! Keep the first backend-owned encoder alive until its device peers close.
//! NVIDIA bug 4340965 leaves an internal timer referring to that first encoder.
use super::api;
use crate::error::DriverError;
use core::ffi::c_void;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

struct Sessions {
    first: usize,
    active: usize,
}
impl Sessions {
    fn retire(&mut self, handle: usize) -> [Option<usize>; 2] {
        self.active -= 1;
        [
            (handle != self.first).then_some(handle),
            (self.active == 0).then_some(self.first),
        ]
    }
}
struct DeviceSessions {
    sessions: Sessions,
    first_cuda: Arc<CudaContext>,
}
pub(super) static CREATE: Mutex<()> = Mutex::new(());

static DEVICES: LazyLock<Mutex<HashMap<usize, DeviceSessions>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) struct EncoderLease {
    pub handle: *mut c_void,
    pub cuda: Arc<CudaContext>,
}
// SAFETY: the lease retains immutable runtime resources. Submission and output
// processing use distinct owned buffers, and teardown waits for all work.
unsafe impl Send for EncoderLease {}
unsafe impl Sync for EncoderLease {}
impl EncoderLease {
    pub fn open(
        cuda: Arc<CudaContext>,
        params: &mut nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    ) -> Result<Self, DriverError> {
        let api = api::get()?;
        // Opening and registering are atomic with respect to peer destruction.
        let mut devices = DEVICES.lock();
        let mut handle = core::ptr::null_mut();
        // SAFETY: current CUDA context, versioned descriptor and writable handle.
        if unsafe { (api.open_encode_session_ex)(params, &mut handle) }
            != nv::NVENCSTATUS::NV_ENC_SUCCESS
            || handle.is_null()
        {
            return Err(DriverError::OperationFailed("NvEncOpenEncodeSessionEx"));
        }
        let group = devices
            .entry(cuda.ordinal())
            .or_insert_with(|| DeviceSessions {
                sessions: Sessions {
                    first: handle as usize,
                    active: 0,
                },
                first_cuda: cuda.clone(),
            });
        group.sessions.active += 1;
        Ok(Self { handle, cuda })
    }
}
impl Drop for EncoderLease {
    fn drop(&mut self) {
        let mut devices = DEVICES.lock();
        let Some(group) = devices.get_mut(&self.cuda.ordinal()) else {
            return;
        };
        let [ordinary, first] = group.sessions.retire(self.handle as usize);
        let first_cuda = group.first_cuda.clone();
        if group.sessions.active == 0 {
            devices.remove(&self.cuda.ordinal());
        }
        // Keep the registry locked through destruction so a new first encoder
        // cannot open while the previous device group is still being retired.
        for (handle, cuda) in [(ordinary, &self.cuda), (first, &first_cuda)] {
            if let Some(handle) = handle {
                if cuda.bind_to_thread().is_ok() {
                    if let Ok(api) = api::get() {
                        // SAFETY: no mappings, output buffers or worker calls remain.
                        unsafe { (api.destroy_encoder)(handle as *mut c_void) };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sessions;

    #[test]
    fn first_encoder_outlives_peers_without_retaining_every_retired_peer() {
        let mut sessions = Sessions {
            first: 1,
            active: 2,
        };
        assert_eq!(sessions.retire(1), [None, None]);
        for handle in 3..100 {
            sessions.active += 1;
            assert_eq!(sessions.retire(handle), [Some(handle), None]);
        }
        assert_eq!(sessions.retire(2), [Some(2), Some(1)]);
        assert_eq!(sessions.active, 0);
    }

    #[test]
    fn normal_reverse_order_and_single_session_release_exactly_once() {
        let mut sessions = Sessions {
            first: 1,
            active: 2,
        };
        assert_eq!(sessions.retire(2), [Some(2), None]);
        assert_eq!(sessions.retire(1), [None, Some(1)]);
        let mut single = Sessions {
            first: 3,
            active: 1,
        };
        assert_eq!(single.retire(3), [None, Some(3)]);
    }
}
