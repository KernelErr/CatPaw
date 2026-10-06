//! The page clock behind `Date`, `performance.now()` and timers.
//!
//! In real mode it follows the system's monotonic clock. In virtual mode time
//! only moves when the event loop decides it should (it jumps to the next
//! timer once the page is otherwise idle), which makes page loads fast and
//! repeatable.

use std::cell::Cell;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How far a read of the virtual clock moves it, in milliseconds. Without
/// this, a script that spins until some time has passed would never finish.
const VIRTUAL_READ_TICK_MS: f64 = 0.005;

pub struct Clock {
    virtual_time: bool,
    start: Instant,
    /// Unix time of the time origin, in milliseconds.
    origin_unix_ms: f64,
    /// Virtual mode: milliseconds since the time origin.
    virtual_now: Cell<f64>,
}

impl Clock {
    pub fn new(virtual_time: bool) -> Self {
        let origin_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
            .floor();
        Self::with_origin(virtual_time, origin_unix_ms)
    }

    /// A clock whose time origin is a fixed Unix time (for repeatable runs).
    pub fn with_origin(virtual_time: bool, origin_unix_ms: f64) -> Self {
        Self {
            virtual_time,
            start: Instant::now(),
            origin_unix_ms,
            virtual_now: Cell::new(0.0),
        }
    }

    pub fn is_virtual(&self) -> bool {
        self.virtual_time
    }

    /// Milliseconds since the time origin, as observed by script: reading a
    /// virtual clock advances it by a tick.
    pub fn now(&self) -> f64 {
        if self.virtual_time {
            let t = self.virtual_now.get() + VIRTUAL_READ_TICK_MS;
            self.virtual_now.set(t);
            t
        } else {
            self.start.elapsed().as_secs_f64() * 1000.0
        }
    }

    /// Milliseconds since the time origin, without the read tick (for the
    /// event loop's own bookkeeping).
    pub fn peek(&self) -> f64 {
        if self.virtual_time {
            self.virtual_now.get()
        } else {
            self.start.elapsed().as_secs_f64() * 1000.0
        }
    }

    /// Unix time of the time origin, in milliseconds.
    pub fn time_origin(&self) -> f64 {
        self.origin_unix_ms
    }

    /// Current Unix time in milliseconds, as observed by script.
    pub fn unix_ms(&self) -> f64 {
        self.origin_unix_ms + self.now()
    }

    /// Moves a virtual clock forward to `t` (milliseconds since the time
    /// origin). Does nothing in real mode or if `t` is in the past.
    pub fn advance_to(&self, t: f64) {
        if self.virtual_time && t > self.virtual_now.get() {
            self.virtual_now.set(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_only_moves_when_told_or_read() {
        let clock = Clock::with_origin(true, 1_000.0);
        assert_eq!(clock.peek(), 0.0);
        let a = clock.now();
        let b = clock.now();
        assert!(b > a && b < 1.0, "reads tick forward slightly");
        clock.advance_to(250.0);
        assert_eq!(clock.peek(), 250.0);
        clock.advance_to(100.0);
        assert_eq!(clock.peek(), 250.0, "never goes backwards");
        assert!(clock.unix_ms() > 1_250.0);
    }
}
