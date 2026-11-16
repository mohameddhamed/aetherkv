use parking_lot::{Condvar, Mutex};
use std::time::Duration;

pub const L0_SLOWDOWN_TRIGGER: usize = 4;
pub const L0_STOP_TRIGGER: usize = 12;

/// Explicit backpressure controller preventing multi-second write stalls.
pub struct BackpressureController {
    lock: Mutex<bool>,
    condvar: Condvar,
}

impl Default for BackpressureController {
    fn default() -> Self {
        Self::new()
    }
}

impl BackpressureController {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    /// Calculate latency delay in microseconds as a function of L0 file count.
    #[inline(always)]
    pub fn compute_delay_micros(l0_count: usize) -> u64 {
        if l0_count <= L0_SLOWDOWN_TRIGGER {
            0
        } else if l0_count < L0_STOP_TRIGGER {
            // Exponential latency injection: 2^((count - 4) / 2)
            let exponent = (l0_count - L0_SLOWDOWN_TRIGGER) as f64 / 2.0;
            2.0f64.powf(exponent).round() as u64
        } else {
            u64::MAX // Signals hard write stall
        }
    }

    /// Throttle incoming write thread according to current L0 accumulation.
    pub fn throttle(&self, l0_count: usize) {
        if l0_count <= L0_SLOWDOWN_TRIGGER {
            return;
        }

        if l0_count < L0_STOP_TRIGGER {
            let delay_micros = Self::compute_delay_micros(l0_count);
            if delay_micros > 0 {
                std::thread::sleep(Duration::from_micros(delay_micros));
            }
        } else {
            // Hard stall: wait until compaction frees L0 space
            let mut stalled = self.lock.lock();
            while *stalled {
                self.condvar.wait(&mut stalled);
            }
        }
    }

    /// Signal that compaction has finished merging L0 files.
    pub fn notify_compaction_complete(&self) {
        let mut stalled = self.lock.lock();
        *stalled = false;
        self.condvar.notify_all();
    }

    /// Set hard stall state when L0 reaches or exceeds L0_STOP_TRIGGER.
    pub fn trigger_hard_stall(&self) {
        let mut stalled = self.lock.lock();
        *stalled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backpressure_delay_computation() {
        assert_eq!(BackpressureController::compute_delay_micros(0), 0);
        assert_eq!(BackpressureController::compute_delay_micros(4), 0);
        assert_eq!(BackpressureController::compute_delay_micros(6), 2);
        assert_eq!(BackpressureController::compute_delay_micros(8), 4);
        assert_eq!(BackpressureController::compute_delay_micros(10), 8);
        assert_eq!(BackpressureController::compute_delay_micros(12), u64::MAX);
    }
}
