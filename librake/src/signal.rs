//! Ctrl+C / kill-signal cancellation.
//!
//! Installs a process-wide handler (once, via [`install`]) that reacts to
//! `SIGINT`/`SIGTERM`/`SIGQUIT` on Unix (deliberately excluding `SIGHUP`) or
//! any console control event on Windows: it emits a final
//! [`LifecycleEvent::Cancelled`] event, prints a matching `Cancelled` status
//! line, and arranges for the in-flight child command (if any) to be killed
//! before the process exits — see [`crate::rakefile`]'s `spawn_resolved`,
//! which polls [`cancel_requested`] and kills its own child directly rather
//! than this module reaching across a thread boundary to do it.
//!
//! Only `spawn_resolved`'s target-command path gets this clean poll-and-kill
//! treatment. A signal arriving while blocked in some other shell-out this
//! feature doesn't poll (a toolchain/tool install, a `git` call) still emits
//! the event/line and ends the process via the safety-net thread below, but
//! that one specific child won't be explicitly killed.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::lifecycle::{Emitter, LifecycleEvent};

/// The active run's context, needed to send a correlated `Cancelled` event
/// if a signal arrives. `None` when no run is currently in progress.
struct RunState {
    run_id: String,
    addresses: Vec<SocketAddr>,
    start: Instant,
}

static RUN_STATE: OnceLock<Mutex<Option<RunState>>> = OnceLock::new();
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);
static HANDLING: AtomicBool = AtomicBool::new(false);
static INSTALL: Once = Once::new();

/// How long the safety-net thread waits before force-exiting the process,
/// giving the normal poll-loop path (typically well under this) a chance to
/// win the race and unwind cleanly first.
const SAFETY_NET_DELAY: Duration = Duration::from_millis(300);

fn run_state() -> &'static Mutex<Option<RunState>> {
    RUN_STATE.get_or_init(|| Mutex::new(None))
}

/// Install the process-wide signal/console-event handler. Idempotent — safe
/// to call at the start of every run; only the first call has any effect.
pub(crate) fn install() {
    INSTALL.call_once(|| {
        #[cfg(unix)]
        install_unix();
        #[cfg(windows)]
        install_windows();
    });
}

#[cfg(unix)]
fn install_unix() {
    use signal_hook::consts::signal::{SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGQUIT]) else {
        return;
    };
    let _join = std::thread::Builder::new()
        .name("rake-signal".to_string())
        .spawn(move || {
            for signal in signals.forever() {
                match signal {
                    SIGINT => handle("SIGINT", 130),
                    SIGTERM => handle("SIGTERM", 143),
                    SIGQUIT => handle("SIGQUIT", 131),
                    _ => {}
                }
            }
        });
}

#[cfg(windows)]
fn install_windows() {
    // ctrlc's Windows handler ignores the console-control-event code
    // entirely, so every one of Ctrl+C/Break/close/logoff/shutdown reports
    // the same generic name here. Default features are sufficient: the
    // `termination` feature only affects ctrlc's Unix build (see
    // librake/Cargo.toml).
    let _ = ctrlc::set_handler(|| handle("console-event", 130));
}

/// The shared reaction to a caught signal/console event: emit the
/// `Cancelled` lifecycle event (if a run is active), print the matching
/// status line, flag the cancellation for pollers, and arrange a bounded
/// fallback exit. Runs at most once per process even if further
/// signals/events arrive while this is in progress.
fn handle(name: &str, code: i32) {
    if HANDLING.swap(true, Ordering::SeqCst) {
        return;
    }
    EXIT_CODE.store(code, Ordering::SeqCst);

    let active = run_state().lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .map(|s| (s.run_id.clone(), s.addresses.clone(), s.start.elapsed()))
    });
    if let Some((run_id, addresses, elapsed)) = active {
        let emitter = Emitter::new_with_run_id(addresses, run_id.clone());
        emitter.emit(&LifecycleEvent::Cancelled {
            run_id,
            ts: Utc::now(),
            signal: name.to_string(),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        });
    }
    crate::rakefile::print_cancelled(name);
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);

    let _join = std::thread::spawn(move || {
        std::thread::sleep(SAFETY_NET_DELAY);
        std::process::exit(code);
    });
}

/// Mark a run as active, so a signal arriving during it can send a
/// correlated `Cancelled` event. `addresses` should be the same (possibly
/// empty) address list the run's real [`Emitter`] was built from.
pub(crate) fn begin_run(run_id: String, addresses: Vec<SocketAddr>, start: Instant) {
    if let Ok(mut guard) = run_state().lock() {
        *guard = Some(RunState {
            run_id,
            addresses,
            start,
        });
    }
}

/// Clear the active run, so a signal arriving after it finishes doesn't try
/// to send a stale `Cancelled` event for it.
pub(crate) fn end_run() {
    if let Ok(mut guard) = run_state().lock() {
        *guard = None;
    }
}

/// Whether a signal/console event has been caught and the run should stop.
pub(crate) fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

/// The process exit code matching whichever signal/event was caught. Only
/// meaningful after [`cancel_requested`] returns `true`.
pub(crate) fn resolved_exit_code() -> i32 {
    EXIT_CODE.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{CANCEL_REQUESTED, EXIT_CODE, Ordering};

    /// Force `cancel_requested()` to report `true` without going through a
    /// real signal, so `spawn_resolved`'s poll/kill loop can be exercised
    /// in-process. Restore with [`reset`] once the test is done, since this
    /// flag is process-global.
    pub(crate) fn force_cancelled(code: i32) {
        EXIT_CODE.store(code, Ordering::SeqCst);
        CANCEL_REQUESTED.store(true, Ordering::SeqCst);
    }

    /// Undo [`force_cancelled`].
    pub(crate) fn reset() {
        CANCEL_REQUESTED.store(false, Ordering::SeqCst);
        EXIT_CODE.store(0, Ordering::SeqCst);
    }
}
