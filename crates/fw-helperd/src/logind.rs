//! logind integration: suspend and resume.
//!
//! Two jobs, and the second one is safety-critical.
//!
//! 1. The energy counter cannot be trusted across suspend — it may reset while
//!    wall-clock advances, so any delta spanning the gap is meaningless (ADR 0009).
//! 2. **The fan must be back under EC control before the machine suspends**
//!    (ADR 0006 point 2). Manual control is a lease held by a running process, and a
//!    suspended process is not minding anything: the watchdog thread is frozen too, so
//!    for the whole duration of the sleep there is nothing between the fan and
//!    whatever duty it was left at.
//!
//! The signal alone is not enough for job 2. `PrepareForSleep(true)` is a
//! notification, not a request for permission — logind does not wait for handlers. So
//! we hold a **delay inhibitor lock**, which is what actually buys the time to write
//! `pwm1_enable=2` before the machine goes down. The lock is a file descriptor: it is
//! held by holding it, and released by dropping it, which is why the drop below is
//! deliberately the last thing that happens.

use crate::fan::FanLease;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Login1Manager {
    /// `true` when about to sleep, `false` on resume.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;

    /// Take an inhibitor lock. The returned descriptor *is* the lock; dropping it
    /// releases. `mode` is `delay` (hold suspend open briefly) or `block` (refuse it
    /// outright, which would be a wildly disproportionate thing for a fan daemon to do).
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// Take the delay lock that keeps suspend open long enough to release the fan.
///
/// Failure is not fatal. Without the lock the release still runs, it simply races the
/// suspend instead of being guaranteed to precede it — and the startup reclaim catches
/// a fan left manual across a sleep. Log it and carry on rather than refusing to run.
async fn take_delay_lock(proxy: &Login1ManagerProxy<'static>) -> Option<zbus::zvariant::OwnedFd> {
    match proxy
        .inhibit(
            "sleep",
            "fw-helper",
            "return the fan to firmware control before suspending",
            "delay",
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            eprintln!(
                "could not take a sleep inhibitor ({e}); the fan release will race \
                 suspend rather than precede it"
            );
            None
        }
    }
}

/// Watch for suspend and resume: release the fan on the way down, flag the resume.
///
/// Failure to reach logind is not fatal for the energy counter — the sampler's max-gap
/// check already discards deltas spanning a long stall. It is more serious for the
/// fan, so it is logged as such, and the startup reclaim remains the backstop.
pub async fn watch_sleep(
    conn: &zbus::Connection,
    flag: Arc<AtomicBool>,
    lease: Arc<FanLease>,
    battery_log: Arc<crate::battery_log::BatteryLog>,
) {
    let proxy = match Login1ManagerProxy::new(conn).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("logind unavailable ({e}); relying on sampler max-gap instead");
            return;
        }
    };
    let mut stream = match proxy.receive_prepare_for_sleep().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot subscribe to PrepareForSleep ({e}); relying on max-gap");
            return;
        }
    };

    let mut lock = take_delay_lock(&proxy).await;

    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };

            if args.start {
                // Going down. Everything here happens while the delay lock is held.
                match lease.release_for_sleep() {
                    Some(duty) => eprintln!(
                        "suspending: released manual fan control (was duty {duty}/255); \
                         it will be restored on resume"
                    ),
                    None => {
                        // Still worth asserting EC control: the flag can be wrong, and
                        // a fan left manual across a suspend has nothing watching it.
                        lease.release_now();
                    }
                }
                // While the delay lock still holds suspend open, so the line lands
                // before the freeze. After the fan, which is the part that matters.
                battery_log.event_now(fw_helper_core::battery_log::Event::Suspend);
                // Last, and deliberately so: this is what is holding suspend open.
                drop(lock.take());
            } else {
                // Resuming. The next energy delta would span the sleep, and the fan
                // may need taking back — but not from here: the poll loop does it on
                // the next tick, with fresh telemetry, so the floor and ceiling are
                // computed from temperatures read after the wake rather than before
                // the sleep.
                flag.store(true, Ordering::SeqCst);
                lock = take_delay_lock(&proxy).await;
            }
        }
    });
}
