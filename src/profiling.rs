//! Opt-in process-wide timing counters; compiled out of ordinary builds.
#[cfg(feature = "profiling")]
use std::{
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[cfg(feature = "profiling")]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Phase {
    EndPicture,
    HostImage,
    Upload,
    Readback,
    InputMap,
    Submit,
    Lock,
    BitstreamCopy,
    InputUnmap,
}

#[cfg(feature = "profiling")]
const PHASES: [Phase; 9] = [
    Phase::EndPicture,
    Phase::HostImage,
    Phase::Upload,
    Phase::Readback,
    Phase::InputMap,
    Phase::Submit,
    Phase::Lock,
    Phase::BitstreamCopy,
    Phase::InputUnmap,
];
#[cfg(feature = "profiling")]
static CALLS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
#[cfg(feature = "profiling")]
static NANOS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

#[cfg(feature = "profiling")]
pub(crate) struct Span {
    phase: Phase,
    start: Instant,
}
#[cfg(feature = "profiling")]
impl Span {
    pub(crate) fn new(phase: Phase) -> Self {
        Self {
            phase,
            start: Instant::now(),
        }
    }
}
#[cfg(feature = "profiling")]
impl Drop for Span {
    fn drop(&mut self) {
        let nanos = self.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        NANOS[self.phase as usize].fetch_add(nanos, Ordering::Relaxed);
        CALLS[self.phase as usize].fetch_add(1, Ordering::Relaxed);
    }
}
#[cfg(feature = "profiling")]
pub(crate) fn report() {
    let mut stderr = std::io::stderr().lock();
    for phase in PHASES {
        let calls = CALLS[phase as usize].load(Ordering::Relaxed);
        if calls > 0 {
            let nanos = NANOS[phase as usize].load(Ordering::Relaxed);
            let _ = writeln!(
                stderr,
                "nvenc-vaapi-profile phase={phase:?} calls={calls} total_ns={nanos}"
            );
        }
    }
}

macro_rules! timed {
    ($phase:ident, $body:expr) => {{
        #[cfg(feature = "profiling")]
        let _span = $crate::profiling::Span::new($crate::profiling::Phase::$phase);
        $body
    }};
}
pub(crate) use timed;

#[cfg(test)]
mod tests {
    #[test]
    fn timing_preserves_errors_and_evaluates_the_operation_once() {
        let mut calls = 0;
        let result: Result<(), &str> = super::timed!(Submit, {
            calls += 1;
            Err("encode failed")
        });
        assert_eq!(calls, 1);
        assert_eq!(result, Err("encode failed"));
    }
}
