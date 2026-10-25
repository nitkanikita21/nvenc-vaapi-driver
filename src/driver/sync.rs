//! Synchronise output publication and surface lifetime, including finite waits.
use super::{guard, state_from};
use crate::driver::state::{BufferStorage, DriverState, Pools};
use crate::error::{DriverError, VAStatus};
use crate::ids::ContainsKey;
use parking_lot::MutexGuard;
use std::time::{Duration, Instant};
use va_sys as va;

fn lock_pools(
    state: &DriverState,
    timeout_ns: u64,
) -> Result<MutexGuard<'_, Pools>, DriverError> {
    if timeout_ns == u64::MAX {
        Ok(state.pools.lock())
    } else {
        state
            .pools
            .try_lock_for(Duration::from_nanos(timeout_ns))
            .ok_or(DriverError::TimedOut)
    }
}

pub unsafe extern "C" fn sync_surface(
    ctx: va::VADriverContextP,
    target: va::VASurfaceID,
) -> VAStatus {
    // SAFETY: same arguments and lifetime contract as the timed variant.
    unsafe { sync_surface2(ctx, target, u64::MAX) }
}
pub unsafe extern "C" fn sync_surface2(
    ctx: va::VADriverContextP,
    target: va::VASurfaceID,
    timeout_ns: u64,
) -> VAStatus {
    guard(|| {
        let started = Instant::now();
        // SAFETY: libva owns this live driver context.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = lock_pools(state, timeout_ns)?;
        let key = pools
            .surfaces
            .find_by_low_bits(target)
            .ok_or(DriverError::InvalidSurface)?;
        super::completion::surface(
            &pools,
            key,
            super::completion::remaining(timeout_ns, started),
        )
    })
}
pub unsafe extern "C" fn sync_buffer(
    ctx: va::VADriverContextP,
    id: va::VABufferID,
    timeout_ns: u64,
) -> VAStatus {
    guard(|| {
        let started = Instant::now();
        // SAFETY: libva owns this live driver context.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = lock_pools(state, timeout_ns)?;
        let key = pools
            .buffers
            .find_by_low_bits(id)
            .ok_or(DriverError::InvalidBuffer)?;
        super::completion::buffer(
            &pools,
            key,
            super::completion::remaining(timeout_ns, started),
        )?;
        let rec = &pools.buffers[key];
        if matches!(rec.storage, BufferStorage::Coded(_)) && !*rec.coded_ready.lock() {
            return Err(DriverError::OperationFailed(
                "coded buffer has no submitted picture",
            ));
        }
        Ok(())
    })
}
