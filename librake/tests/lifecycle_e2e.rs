//! End-to-end test for license-gated lifecycle events: binds a real loopback
//! UDP socket, points a `[lifecycle]`-configured Rakefile at it, runs the
//! Rakefile via [`librake::Rakefile::run_licensed`] with a manually built
//! (unsigned — signature verification is `license.rs`'s concern, not
//! `run_licensed`'s) [`librake::LicensePayload`] granting the `events`
//! feature, and asserts the received datagrams decode to the expected event
//! sequence.

use std::error::Error;
use std::net::UdpSocket;
use std::time::Duration;

use chrono::Utc;
use librake::{Features, Host, LicensePayload, Rakefile};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn licensed_run_emits_lifecycle_events_over_udp() -> TestResult {
    let listener = UdpSocket::bind("127.0.0.1:0")?;
    listener.set_read_timeout(Some(Duration::from_secs(2)))?;
    let addr = listener.local_addr()?;

    // Gate the "winonly" command on whichever platform this test is *not*
    // running on, so the command is reliably skipped regardless of the CI
    // runner's actual OS.
    let host = Host::detect();
    let other_platform = if host.os == "windows" {
        "linux"
    } else {
        "windows"
    };

    let toml = format!(
        r#"
update = false

[[target.extra.command]]
name = "extra-cmd"
cmd = ["cargo", "--version"]

[target.build]
depends_on = ["extra"]

[[target.build.command]]
name = "compile"
cmd = ["cargo", "--version"]

[[target.build.command]]
name = "winonly"
platform = ["{other_platform}"]
cmd = ["cargo", "--version"]

[lifecycle]
address = "{addr}"
"#
    );

    let rakefile = Rakefile::from_toml_str(&toml)?;
    let license = LicensePayload {
        licensee: "test@example.com".to_string(),
        features: Features {
            basic: false,
            events: true,
        },
        expires_at: None,
        issued_at: Utc::now(),
    };
    let report = rakefile.run_licensed(&["build", "^extra"], Some(&license))?;
    assert!(report.status.is_some_and(|s| s.success()));

    let mut events = Vec::new();
    loop {
        let mut buf = [0u8; 4096];
        match listener.recv(&mut buf) {
            Ok(n) => {
                let value: serde_json::Value = serde_json::from_slice(&buf[..n])?;
                events.push(value);
            }
            Err(_) => break,
        }
    }

    let tags: Vec<&str> = events.iter().filter_map(|v| v["event"].as_str()).collect();

    assert_eq!(tags.first(), Some(&"before_all"));
    assert_eq!(tags.last(), Some(&"after_all"));

    let before_all = events.first().ok_or("expected a before_all event")?;
    // The test process always has a valid cwd; git presence/branch/sha
    // depend on the sandbox (e.g. a tarball build without `.git`), so only
    // assert the keys are present, not that they're non-null.
    assert!(
        before_all["cwd"].is_string(),
        "expected cwd in {before_all}"
    );
    assert!(
        before_all.get("git_branch").is_some(),
        "expected git_branch key in {before_all}"
    );
    assert!(
        before_all.get("git_sha").is_some(),
        "expected git_sha key in {before_all}"
    );
    assert!(tags.contains(&"target_skipped"), "tags: {tags:?}");
    assert!(tags.contains(&"command_skipped"), "tags: {tags:?}");
    assert!(tags.contains(&"before_target"));
    assert!(tags.contains(&"after_target"));
    assert!(tags.contains(&"before_command"));
    assert!(tags.contains(&"after_command"));

    let skipped_target = events
        .iter()
        .find(|v| v["event"] == "target_skipped")
        .ok_or("expected a target_skipped event")?;
    assert_eq!(skipped_target["target"], "extra");

    let skipped_command = events
        .iter()
        .find(|v| v["event"] == "command_skipped")
        .ok_or("expected a command_skipped event")?;
    assert_eq!(skipped_command["command"], "winonly");
    assert!(
        skipped_command["reason"]
            .as_str()
            .is_some_and(|r| r.contains(other_platform))
    );

    Ok(())
}

#[test]
fn unlicensed_run_emits_no_events() -> TestResult {
    let listener = UdpSocket::bind("127.0.0.1:0")?;
    listener.set_read_timeout(Some(Duration::from_millis(300)))?;
    let addr = listener.local_addr()?;

    let toml = format!(
        r#"
update = false

[[target.build.command]]
name = "compile"
cmd = ["cargo", "--version"]

[lifecycle]
address = "{addr}"
"#
    );

    let rakefile = Rakefile::from_toml_str(&toml)?;
    // `license: None` behaves exactly like the unlicensed `run` path — no
    // warning either, since there's nothing to be "not licensed for" without
    // a license at all.
    let report = rakefile.run_licensed(&["build"], None)?;
    assert!(report.status.is_some_and(|s| s.success()));

    let mut buf = [0u8; 64];
    let result = listener.recv(&mut buf);
    assert!(result.is_err(), "expected a read timeout, got {result:?}");

    Ok(())
}
