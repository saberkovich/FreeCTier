//! Pacing for the engine loop.
//!
//! Windows rounds a short `Sleep` up to the next clock tick — 15.6 ms by
//! default, and a process only gets finer granularity if it asks for it. A
//! packet path paced by `thread::sleep(1ms)` therefore pays up to 15 ms per
//! hop, which is most of a LAN game's latency budget and the single largest
//! avoidable delay in this client.
//!
//! Two mechanisms replace it. A waitable timer created with
//! `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803+) gives
//! sub-millisecond pacing without calling `timeBeginPeriod`, which would
//! raise the timer resolution for the whole system and cost battery on
//! laptops. And an event, signalled by the pipe reader the moment a packet is
//! queued, means an inbound packet wakes the loop right away instead of
//! waiting out the rest of the period.

use std::sync::OnceLock;
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    CreateEventW, CreateWaitableTimerExW, GetCurrentThread, SetEvent, SetThreadPriority,
    SetWaitableTimer, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, THREAD_PRIORITY_ABOVE_NORMAL, TIMER_ALL_ACCESS,
};

/// Put the calling thread one step above normal priority. Packet pumps are
/// latency-critical but barely use any CPU, so they should preempt ordinary
/// work — the game itself included — instead of waiting a scheduler quantum
/// behind it on a loaded machine. One step, not time-critical: a bug in a
/// thread this cheap must never be able to starve the desktop.
pub fn prioritize() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Raw handle of the engine's wake event. Stored as `isize` because `HANDLE`
/// is not `Send` and this is reachable from the pipe reader thread; the only
/// use is `SetEvent`, which is thread-safe.
static SIGNAL: OnceLock<isize> = OnceLock::new();

/// Wake the engine loop now. A no-op before the loop starts, or if the event
/// could not be created (the loop still runs on its timer).
pub fn signal() {
    if let Some(&raw) = SIGNAL.get() {
        unsafe {
            let _ = SetEvent(HANDLE(raw as *mut _));
        }
    }
}

/// Owns the timer and the wake event for the engine thread. Both handles live
/// as long as the process does (the worker ends in `TerminateProcess`), so
/// they are deliberately never closed: `SIGNAL` must not dangle while other
/// threads can still call [`signal`].
pub struct Waker {
    timer: Option<HANDLE>,
    signal: Option<HANDLE>,
}

impl Waker {
    pub fn new() -> Self {
        // Auto-reset: one wake-up per batch of packets, not one per packet.
        let signal = unsafe { CreateEventW(None, false, false, None) }.ok();
        if let Some(event) = signal {
            let _ = SIGNAL.set(event.0 as isize);
        }
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok();
        Self { timer, signal }
    }

    /// Block until `period` has passed or a packet arrives, whichever is
    /// first. Falls back to progressively coarser waits if the high-resolution
    /// timer is unavailable (pre-1803 Windows), which costs latency but never
    /// correctness.
    pub fn wait(&mut self, period: Duration) {
        let timeout = period.as_millis().max(1) as u32;
        if let Some(timer) = self.timer {
            // Negative due time means "relative", in 100 ns units; period 0
            // makes it one-shot, re-armed on every wait.
            let due = -((period.as_nanos() / 100) as i64);
            if unsafe { SetWaitableTimer(timer, &due, 0, None, None, false) }.is_ok() {
                let mut handles = [timer, timer];
                let count = match self.signal {
                    // Duplicate handles would make the wait fail outright.
                    Some(event) => {
                        handles[1] = event;
                        2
                    }
                    None => 1,
                };
                // The timeout is a backstop only; the timer fires first.
                unsafe { WaitForMultipleObjects(&handles[..count], false, timeout + 50) };
                return;
            }
        }
        if let Some(event) = self.signal {
            unsafe { WaitForSingleObject(event, timeout) };
            return;
        }
        std::thread::sleep(period);
    }
}
