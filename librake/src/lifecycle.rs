//! License-gated lifecycle events: a fire-and-forget, best-effort JSON event
//! stream sent over a loopback UDP socket so external tooling can observe a
//! run as it happens.
//!
//! Every [`Emitter::emit`] call is unconditional at the call site — a
//! disabled emitter (unlicensed, or no `[lifecycle]` table configured) is a
//! [`NullSink`] that silently drops every event, so `run_impl`/`run_steps`/
//! `run_one` never need to branch on whether events are enabled.

use std::net::{SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::tool::UpdateRecord;

/// A single lifecycle event, externally tagged so a JSON consumer can match
/// on the `"event"` field without a schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum LifecycleEvent {
    /// The whole run is about to start; the execution plan is already known.
    BeforeAll {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The root targets requested on the command line.
        roots: Vec<String>,
        /// The number of steps (targets to run or skip) in the resolved plan.
        target_count: usize,
    },
    /// The whole run has finished (successfully, or via a stopped chain).
    AfterAll {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// Total wall-clock time spent on the run.
        elapsed_ms: u64,
        /// The last command's exit code, or `None` when no command ran.
        exit_code: Option<i32>,
        /// `true` when no command failed the chain.
        success: bool,
        /// The number of tool installs/updates that occurred.
        tool_updates: usize,
    },
    /// A target is about to run (its dependencies have already run).
    BeforeTarget {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The target's name.
        target: String,
    },
    /// A target has finished running all of its commands (or stopped early).
    AfterTarget {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The target's name.
        target: String,
        /// The last command's exit code, or `None` when no command ran.
        exit_code: Option<i32>,
        /// `true` when no command failed the chain.
        success: bool,
        /// `true` when a command failure here stops the rest of the run.
        chain_stopped: bool,
        /// Wall-clock time spent running this target's commands.
        elapsed_ms: u64,
    },
    /// A `^`-skipped target was pruned from the run instead of executing.
    TargetSkipped {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The target's name.
        target: String,
        /// Why the target was skipped.
        reason: String,
    },
    /// A command is about to be spawned.
    BeforeCommand {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The owning target's name.
        target: String,
        /// The command's name.
        command: String,
    },
    /// A command finished (or failed to launch).
    AfterCommand {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The owning target's name.
        target: String,
        /// The command's name.
        command: String,
        /// The command's exit code, or `None` when it never launched.
        exit_code: Option<i32>,
        /// `true` when the command launched and exited zero.
        success: bool,
        /// The command's own `skip_on_error` setting.
        skip_on_error: bool,
        /// `true` when this outcome stops the rest of the run.
        chain_stopped: bool,
        /// Wall-clock time spent on this command.
        duration_ms: u64,
    },
    /// A command was excluded by its `platform`/`arch` gate.
    CommandSkipped {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The owning target's name.
        target: String,
        /// The command's name.
        command: String,
        /// The unmet `platform`/`arch` requirement.
        reason: String,
    },
    /// A tool is about to be checked/ensured.
    BeforeTool {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The owning target's name.
        target: String,
        /// The owning command's name, or `None` for a target-level tool.
        command: Option<String>,
        /// The tool's name.
        tool: String,
    },
    /// A tool check/install/update has finished.
    AfterTool {
        /// Identifier shared by every event emitted during this run.
        run_id: String,
        /// Wall-clock time the event was emitted.
        ts: DateTime<Utc>,
        /// The owning target's name.
        target: String,
        /// The owning command's name, or `None` for a target-level tool.
        command: Option<String>,
        /// The tool's name.
        tool: String,
        /// Whether the tool was already present, freshly installed, or updated.
        outcome: ToolOutcome,
        /// The previously installed version, when known.
        from: Option<String>,
        /// The now-installed version, when known.
        to: Option<String>,
        /// Wall-clock time spent ensuring this tool.
        duration_ms: u64,
    },
}

/// Whether a tool ensure found the tool already present, installed it fresh,
/// or updated an older version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolOutcome {
    /// The tool was already present and current.
    Present,
    /// The tool was absent and has just been installed.
    Installed,
    /// The tool was present but outdated and has just been updated.
    Updated,
}

impl ToolOutcome {
    /// Derive the outcome and `from`/`to` versions from `ToolTable::ensure`'s
    /// return value, without any changes to `tool.rs` itself: `None` means
    /// the tool was already present, `Some` with no `from` means a fresh
    /// install, and `Some` with a `from` means an update.
    pub(crate) fn from_update(
        record: Option<&UpdateRecord>,
    ) -> (Self, Option<String>, Option<String>) {
        match record {
            None => (Self::Present, None, None),
            Some(r) if r.from.is_none() => (Self::Installed, r.from.clone(), r.to.clone()),
            Some(r) => (Self::Updated, r.from.clone(), r.to.clone()),
        }
    }
}

/// Where a serialized [`LifecycleEvent`] is sent. Abstracted so tests can
/// inject a recording double instead of a real socket.
pub(crate) trait Sink {
    /// Send the already-encoded event. Best-effort: implementations must not
    /// panic or block waiting for a peer.
    fn send(&self, bytes: &[u8]);
}

/// Sends events over a loopback UDP socket. `send_to` does not block waiting
/// for a listener, and a datagram sent to an address with nobody listening is
/// silently dropped by the OS — exactly the desired fire-and-forget contract.
struct UdpSink {
    socket: UdpSocket,
    target: SocketAddr,
}

impl Sink for UdpSink {
    fn send(&self, bytes: &[u8]) {
        let _ = self.socket.send_to(bytes, self.target).ok();
    }
}

/// The disabled sink: every event is silently dropped. Used when lifecycle
/// events are unlicensed, unconfigured, or the socket could not be bound.
struct NullSink;

impl Sink for NullSink {
    fn send(&self, _bytes: &[u8]) {}
}

/// Emits lifecycle events for one run. Always safe to call `emit` on,
/// regardless of whether events are actually enabled.
pub(crate) struct Emitter {
    sink: Box<dyn Sink>,
    run_id: String,
}

impl Emitter {
    /// A disabled emitter: every [`emit`](Self::emit) call is a no-op.
    pub(crate) fn disabled() -> Self {
        Self {
            sink: Box::new(NullSink),
            run_id: generate_run_id(),
        }
    }

    /// Build a live emitter bound to an ephemeral local port, sending to
    /// `target`. A bind failure (e.g. no loopback interface available) falls
    /// back to [`disabled`](Self::disabled) rather than failing the run —
    /// lifecycle events are an observability side channel, never a required
    /// one.
    pub(crate) fn new(target: SocketAddr) -> Self {
        let bind_addr: SocketAddr = if target.is_ipv6() {
            (std::net::Ipv6Addr::LOCALHOST, 0).into()
        } else {
            (std::net::Ipv4Addr::LOCALHOST, 0).into()
        };
        match UdpSocket::bind(bind_addr) {
            Ok(socket) => Self {
                sink: Box::new(UdpSink { socket, target }),
                run_id: generate_run_id(),
            },
            Err(_) => Self::disabled(),
        }
    }

    /// Build an emitter around an arbitrary [`Sink`], for tests.
    #[cfg(test)]
    pub(crate) fn from_sink(sink: Box<dyn Sink>) -> Self {
        Self {
            sink,
            run_id: generate_run_id(),
        }
    }

    /// The identifier shared by every event this emitter sends.
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Serialize and send `event`. Both encoding and send failures are
    /// swallowed — emission is always best-effort and never surfaces an
    /// [`Error`](crate::Error) or aborts the run.
    pub(crate) fn emit(&self, event: &LifecycleEvent) {
        if let Ok(bytes) = serde_json::to_vec(event) {
            self.sink.send(&bytes);
        }
    }
}

/// A cheap, dependency-free per-run identifier: the process id plus
/// nanoseconds since the Unix epoch, hex-joined. Uniqueness only needs to
/// hold within one machine's lifetime of listeners correlating events from
/// concurrent runs, not cryptographic guarantees.
fn generate_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}-{:x}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::sync::{Arc, Mutex};

    use super::{Emitter, LifecycleEvent, Sink, ToolOutcome};
    use crate::tool::UpdateRecord;

    /// Records every sent payload into a shared buffer, so a test can hold a
    /// clone of the `Arc` after the sink itself has been moved into a boxed
    /// `Emitter`.
    struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Sink for RecordingSink {
        fn send(&self, bytes: &[u8]) {
            // A poisoned lock here would only follow an earlier panic on this
            // same value within a single test; ignore rather than propagate.
            if let Ok(mut guard) = self.0.lock() {
                guard.push(bytes.to_vec());
            }
        }
    }

    #[test]
    fn disabled_emitter_sends_nothing() {
        let emitter = Emitter::disabled();
        // No observable effect beyond "does not panic" — disabled() uses a
        // NullSink with no recording, so this just exercises the no-op path.
        emitter.emit(&LifecycleEvent::BeforeAll {
            run_id: emitter.run_id().to_string(),
            ts: chrono::Utc::now(),
            roots: vec!["default".to_string()],
            target_count: 1,
        });
    }

    #[test]
    fn emit_serializes_and_sends_tagged_json() -> Result<(), Box<dyn Error>> {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let emitter = Emitter::from_sink(Box::new(RecordingSink(buffer.clone())));
        emitter.emit(&LifecycleEvent::BeforeTarget {
            run_id: "abc-123".to_string(),
            ts: chrono::Utc::now(),
            target: "build".to_string(),
        });
        let sent = buffer.lock().map_err(|_| "poisoned lock")?;
        let value: serde_json::Value = serde_json::from_slice(&sent[0])?;
        assert_eq!(value["event"], "before_target");
        assert_eq!(value["target"], "build");
        Ok(())
    }

    #[test]
    fn run_id_is_stable_per_emitter() {
        let emitter = Emitter::disabled();
        assert_eq!(emitter.run_id(), emitter.run_id());
        assert!(!emitter.run_id().is_empty());
    }

    #[test]
    fn tool_outcome_present_when_no_update_record() {
        let (outcome, from, to) = ToolOutcome::from_update(None);
        assert_eq!(outcome, ToolOutcome::Present);
        assert_eq!(from, None);
        assert_eq!(to, None);
    }

    #[test]
    fn tool_outcome_installed_when_from_is_none() {
        let record = Some(UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: None,
            to: Some("0.9.0".to_string()),
        });
        let (outcome, from, to) = ToolOutcome::from_update(record.as_ref());
        assert_eq!(outcome, ToolOutcome::Installed);
        assert_eq!(from, None);
        assert_eq!(to, Some("0.9.0".to_string()));
    }

    #[test]
    fn tool_outcome_updated_when_from_is_some() {
        let record = Some(UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: Some("0.8.0".to_string()),
            to: Some("0.9.0".to_string()),
        });
        let (outcome, from, to) = ToolOutcome::from_update(record.as_ref());
        assert_eq!(outcome, ToolOutcome::Updated);
        assert_eq!(from, Some("0.8.0".to_string()));
        assert_eq!(to, Some("0.9.0".to_string()));
    }
}
