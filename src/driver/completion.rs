//! Publish each worker result into its original coded buffer and synchronise
//! surface access. The worker never takes the pools lock held by these callers.
use super::state::{BufferStorage, Pools};
use crate::{
    error::DriverError,
    ids::{BufferKey, SurfaceKey},
};
use core::ffi::c_void;
use std::time::Instant;
use va_sys as va;

pub(crate) fn remaining(timeout_ns: u64, started: Instant) -> u64 {
    if timeout_ns == u64::MAX {
        u64::MAX
    } else {
        timeout_ns.saturating_sub(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
    }
}

pub(crate) fn buffer(pools: &Pools, key: BufferKey, timeout_ns: u64) -> Result<(), DriverError> {
    let buffer = pools.buffers.get(key).ok_or(DriverError::InvalidBuffer)?;
    let pending = buffer.pending_output.lock().clone();
    let Some(frame) = pending else {
        return Ok(());
    };
    if buffer.mapped.lock().is_some() {
        return Err(DriverError::InvalidParameter);
    }
    let BufferStorage::Coded(slot) = &buffer.storage else {
        return Err(DriverError::InvalidBuffer);
    };
    let bytes = match frame.take_bytes(timeout_ns) {
        Ok(bytes) => bytes,
        Err(DriverError::TimedOut) => return Err(DriverError::TimedOut),
        Err(error) => {
            buffer.pending_output.lock().take();
            *buffer.coded_ready.lock() = false;
            return Err(error);
        }
    };
    let mut backing = slot.backing.lock();
    *backing = bytes;
    let segment_value = va::VACodedBufferSegment {
        size: u32::try_from(backing.len()).map_err(|_| DriverError::InvalidParameter)?,
        bit_offset: 0,
        status: 0,
        reserved: 0,
        buf: backing.as_mut_ptr() as *mut c_void,
        next: core::ptr::null_mut(),
        va_reserved: [0; 4],
    };
    let mut segment = slot.segment.lock();
    if let Some(existing) = segment.as_mut() {
        **existing = segment_value;
    } else {
        *segment = Some(Box::new(segment_value));
    }
    buffer.pending_output.lock().take();
    *buffer.coded_ready.lock() = true;
    Ok(())
}

pub(crate) fn surface(pools: &Pools, key: SurfaceKey, timeout_ns: u64) -> Result<(), DriverError> {
    let started = Instant::now();
    for (_, context) in pools.contexts.iter() {
        if let Some(session) = context.session.lock().as_ref() {
            session.wait_surface(key, remaining(timeout_ns, started))?;
        }
    }
    Ok(())
}

pub(crate) fn surface_ready(pools: &Pools, key: SurfaceKey) -> bool {
    pools.contexts.iter().all(|(_, context)| {
        context
            .session
            .lock()
            .as_ref()
            .is_none_or(|session| session.surface_ready(key))
    })
}
