//! Ensuring the required Rust toolchain is available before running targets.
//!
//! The standalone `rake` binary may be installed (via AUR/DEB/RPM/Homebrew)
//! on a machine without Rust, yet nearly every target shells out to `cargo`.
//! [`ensure_rust_toolchain`] checks for `cargo`, offering to install Rust via
//! the official rustup installer when it is absent, and — when a different
//! toolchain is already installed — verifies (and offers to install) the
//! channel the `Rakefile.toml` asks for, pinning the run to it.

use std::{
    ffi::OsStr,
    io::{IsTerminal, Write, stderr, stdin},
    path::PathBuf,
    process::Command as ProcessCommand,
};

use crate::{
    error::{Error, Result},
    rakefile::print_label,
};

/// The rustup bootstrap script, run via `sh -c`. The desired toolchain is
/// passed as the positional argument `$1` (quoted) rather than interpolated
/// into the script text, so a (validated) toolchain token cannot break out of
/// the `--default-toolchain` argument.
const RUSTUP_INSTALL_SCRIPT: &str = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
     | sh -s -- -y --default-toolchain \"$1\"";

/// Ensure the required Rust toolchain is available, then pin this run to it.
///
/// With `toolchain` `None` (no channel declared) this is a quiet no-op when
/// `cargo` is present; it only bootstraps Rust (defaulting to `stable`) when
/// `cargo` is entirely absent, so the standalone `rake` binary still works on a
/// bare machine.
///
/// With `toolchain` `Some(channel)`: when `cargo` is absent and stdin is an
/// interactive terminal, the user is prompted to install Rust via rustup
/// (defaulting to `channel`). When `cargo` is present but `channel` is not an
/// installed toolchain, the user is prompted to install that channel via rustup.
/// Once available, the process is pinned to `channel` (via `RUSTUP_TOOLCHAIN`)
/// so the commands it spawns use the requested channel. When rustup is not
/// managing the toolchain (e.g. a distro-packaged cargo), the existing `cargo`
/// is accepted as-is.
///
/// # Errors
/// Returns [`Error::RustToolchainMissing`] when `cargo` is absent and install is
/// declined or impossible (no interactive terminal);
/// [`Error::RustChannelMissing`] when the requested channel is absent and
/// installing it is declined or impossible; [`Error::RustInstallSpawn`] /
/// [`Error::RustInstallFailed`] when an installer cannot be launched or exits
/// non-zero; or [`Error::Io`] if reading a prompt response fails.
pub fn ensure_rust_toolchain(toolchain: Option<&str>) -> Result<()> {
    match toolchain {
        // No declared channel: don't verify or pin anything, and stay quiet —
        // commands use the active toolchain. Only guarantee *some* cargo exists,
        // so the standalone `rake` binary can still bootstrap on a bare machine.
        None => {
            if cargo_version().is_none() {
                bootstrap_rust("stable")?;
            }
            Ok(())
        }
        Some(channel) => {
            print_label("Checking", &format!("rust toolchain ({channel})"));
            if cargo_version().is_none() {
                bootstrap_rust(channel)?;
            }
            ensure_channel(channel)
        }
    }
}

/// Install Rust from scratch via the rustup installer (with confirmation),
/// defaulting to `toolchain`. Returns `Ok` only once `cargo` is available.
fn bootstrap_rust(toolchain: &str) -> Result<()> {
    // Without an interactive terminal there is no one to prompt; refuse rather
    // than hang or install software unattended.
    if !stdin().is_terminal() {
        return Err(Error::RustToolchainMissing);
    }
    if !prompt(&format!(
        "No Rust toolchain found. Install Rust (toolchain '{toolchain}') now via rustup? [y/N]: "
    ))? {
        return Err(Error::RustToolchainMissing);
    }

    print_label("Installing", &format!("rustup (toolchain {toolchain})"));
    install_rustup(toolchain)?;
    prepend_cargo_bin_to_path();

    if cargo_version().is_none() {
        return Err(Error::RustToolchainMissing);
    }
    Ok(())
}

/// With `cargo` already present, verify the requested `toolchain` channel is
/// installed (offering to install it when missing) and pin the run to it.
fn ensure_channel(toolchain: &str) -> Result<()> {
    match toolchain_status(toolchain) {
        // No rustup to manage channels (e.g. a distro-packaged cargo): trust the
        // `cargo` already on `PATH` rather than failing.
        ChannelStatus::Unmanaged => {
            present(toolchain, false);
            Ok(())
        }
        ChannelStatus::Installed => {
            pin_toolchain(toolchain);
            present(toolchain, true);
            Ok(())
        }
        ChannelStatus::Missing => {
            if !stdin().is_terminal() {
                return Err(Error::RustChannelMissing {
                    toolchain: toolchain.to_string(),
                });
            }
            if !prompt(&format!(
                "The '{toolchain}' Rust toolchain is not installed. Install it now via rustup? [y/N]: "
            ))? {
                return Err(Error::RustChannelMissing {
                    toolchain: toolchain.to_string(),
                });
            }
            print_label("Installing", &format!("rustup toolchain {toolchain}"));
            install_channel(toolchain)?;
            pin_toolchain(toolchain);
            present(toolchain, true);
            Ok(())
        }
    }
}

/// Print a `Present` status line. With a rustup-managed `toolchain`, the channel
/// is named; otherwise the detected `cargo` version (when readable) is shown.
fn present(toolchain: &str, managed: bool) {
    if managed {
        print_label("Present", &format!("toolchain {toolchain}"));
    } else {
        match cargo_version() {
            Some(version) => print_label("Present", &format!("cargo {version}")),
            None => print_label("Present", "cargo"),
        }
    }
}

/// Whether the requested toolchain channel is installed under rustup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelStatus {
    /// rustup is unavailable, so channels cannot be managed.
    Unmanaged,
    /// The requested channel is installed.
    Installed,
    /// rustup is available but the requested channel is not installed.
    Missing,
}

/// Classify the requested `toolchain` against `rustup toolchain list`.
fn toolchain_status(toolchain: &str) -> ChannelStatus {
    let Ok(output) = ProcessCommand::new("rustup")
        .args(["toolchain", "list"])
        .output()
    else {
        return ChannelStatus::Unmanaged;
    };
    if !output.status.success() {
        return ChannelStatus::Unmanaged;
    }
    if toolchain_listed(&String::from_utf8_lossy(&output.stdout), toolchain) {
        ChannelStatus::Installed
    } else {
        ChannelStatus::Missing
    }
}

/// Whether `list` (rustup's `toolchain list` output) contains `toolchain`.
/// Each line begins with a full toolchain name like
/// `stable-x86_64-unknown-linux-gnu`; a requested channel matches when it equals
/// that name or is its leading segment (the trailing `-` guards against partial
/// matches like `1.9` against `1.95.0`).
fn toolchain_listed(list: &str, toolchain: &str) -> bool {
    let prefix = format!("{toolchain}-");
    list.lines()
        .any(|line| match line.split_whitespace().next() {
            Some(name) => name == toolchain || name.starts_with(&prefix),
            None => false,
        })
}

/// Probe for `cargo` by running `cargo --version`, returning the parsed version
/// token (e.g. `1.89.0` from `cargo 1.89.0 (...)`) when present and successful.
fn cargo_version() -> Option<String> {
    let output = ProcessCommand::new("cargo")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // `cargo --version` prints `cargo <version> (<hash> <date>)`; take the
    // version token, falling back to the trimmed line if the shape differs.
    text.split_whitespace()
        .nth(1)
        .map_or_else(|| Some(text.trim().to_string()), |v| Some(v.to_string()))
}

/// Prompt with `message` on stderr, returning whether the response was
/// affirmative.
fn prompt(message: &str) -> Result<bool> {
    let mut err = stderr();
    let _ = write!(err, "\n{message}").ok();
    let _ = err.flush().ok();
    let mut line = String::new();
    let _read = stdin().read_line(&mut line).map_err(Error::Io)?;
    Ok(affirmative(&line))
}

/// Whether `input` is an affirmative response (`y`/`yes`, case-insensitive).
/// Empty or anything else is treated as a no.
fn affirmative(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Run the rustup installer for `toolchain`, inheriting stdio so its progress
/// is visible.
fn install_rustup(toolchain: &str) -> Result<()> {
    let status = ProcessCommand::new("sh")
        .args(["-c", RUSTUP_INSTALL_SCRIPT, "sh", toolchain])
        .status()
        .map_err(|source| Error::RustInstallSpawn {
            program: "sh".to_string(),
            source,
        })?;
    finish_install(status)
}

/// Install the `toolchain` channel through an existing rustup, inheriting stdio.
fn install_channel(toolchain: &str) -> Result<()> {
    let status = ProcessCommand::new("rustup")
        .args(["toolchain", "install", toolchain])
        .status()
        .map_err(|source| Error::RustInstallSpawn {
            program: "rustup".to_string(),
            source,
        })?;
    finish_install(status)
}

/// Map an installer's exit `status` to success or [`Error::RustInstallFailed`].
fn finish_install(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::RustInstallFailed { status })
    }
}

/// Prepend `~/.cargo/bin` to this process's `PATH` so a freshly installed
/// `cargo` (and the tools it later spawns) are found without restarting the
/// shell. A best-effort no-op when `HOME` is unset or `PATH` cannot be rebuilt.
fn prepend_cargo_bin_to_path() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let mut cargo_bin = PathBuf::from(home);
    cargo_bin.push(".cargo");
    cargo_bin.push("bin");

    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries = vec![cargo_bin];
    entries.extend(std::env::split_paths(&existing));
    let Ok(joined) = std::env::join_paths(entries) else {
        return;
    };
    set_env("PATH", &joined);
}

/// Pin this process — and the commands it spawns — to `toolchain` via
/// `RUSTUP_TOOLCHAIN`, so rustup's `cargo`/`rustc` proxies use the requested
/// channel regardless of the configured default.
fn pin_toolchain(toolchain: &str) {
    set_env("RUSTUP_TOOLCHAIN", toolchain);
}

/// Set the environment variable `key` to `value` for this process.
#[allow(unsafe_code)]
fn set_env(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: this runs early in `main`, before any threads are spawned, so no
    // other thread can be reading or writing the environment concurrently.
    unsafe {
        std::env::set_var(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::{affirmative, ensure_rust_toolchain, toolchain_listed};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SAMPLE_LIST: &str = "stable-x86_64-unknown-linux-gnu\n\
         nightly-x86_64-unknown-linux-gnu (active, default)\n\
         1.95.0-x86_64-unknown-linux-gnu\n";

    #[test]
    fn affirmative_accepts_yes_variants() {
        for input in ["y", "Y", "yes", "Yes", "YES", " y \n", "yes\r\n"] {
            assert!(affirmative(input), "expected '{input}' to be affirmative");
        }
    }

    #[test]
    fn affirmative_rejects_everything_else() {
        for input in ["", "n", "N", "no", "nope", "\n", "y e s", "yep", "1"] {
            assert!(!affirmative(input), "expected '{input}' to be negative");
        }
    }

    #[test]
    fn toolchain_listed_matches_channels_and_versions() {
        for channel in ["stable", "nightly", "1.95.0"] {
            assert!(
                toolchain_listed(SAMPLE_LIST, channel),
                "expected '{channel}' to be listed"
            );
        }
    }

    #[test]
    fn toolchain_listed_rejects_partial_and_absent() {
        for channel in ["beta", "1.9", "night", "1.95"] {
            assert!(
                !toolchain_listed(SAMPLE_LIST, channel),
                "expected '{channel}' not to be listed"
            );
        }
    }

    #[test]
    fn ensure_succeeds_for_installed_toolchain() -> TestResult {
        // Use a toolchain that is guaranteed available so this neither prompts
        // nor installs: the first one rustup reports, or any channel when rustup
        // is absent (then the existing cargo is accepted best-effort).
        let toolchain = first_installed_toolchain().unwrap_or_else(|| "stable".to_string());
        ensure_rust_toolchain(Some(&toolchain))?;
        Ok(())
    }

    #[test]
    fn ensure_none_is_ok_when_cargo_present() -> TestResult {
        // No declared channel: a silent no-op since cargo is present in the test
        // environment (no prompt, no install, no pin).
        ensure_rust_toolchain(None)?;
        Ok(())
    }

    /// The first toolchain name reported by `rustup toolchain list`, or `None`
    /// when rustup is unavailable or reports nothing usable.
    fn first_installed_toolchain() -> Option<String> {
        let output = std::process::Command::new("rustup")
            .args(["toolchain", "list"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            // The "no installed toolchains" message has no host-triple `-`.
            .filter(|name| name.contains('-'))
            .map(str::to_string)
    }
}
