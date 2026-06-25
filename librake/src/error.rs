//! Error types for `librake`.

use std::io;
use std::process::ExitStatus;

/// Errors that can occur while loading, validating, or running a `Rakefile.toml`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `Rakefile.toml` could not be read from disk.
    #[error("unable to read Rakefile")]
    Io(#[from] io::Error),
    /// The `Rakefile.toml` contents could not be parsed (covers a command
    /// missing its `name` or `cmd`).
    #[error("unable to parse Rakefile")]
    Parse(#[from] toml::de::Error),
    /// A target defined neither commands nor dependencies.
    #[error(
        "target '{target}' defines neither commands nor dependencies (add at least one [[target.{target}.command]] or a depends_on entry)"
    )]
    EmptyTarget {
        /// The offending target name.
        target: String,
    },
    /// A target's command declared a `cmd` with no program to run.
    #[error(
        "target '{target}' command '{command}' has an empty 'cmd' (it must contain at least the program to run)"
    )]
    EmptyCmd {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
    },
    /// A target's command declared no body at all — none of `cmd`, `sh`, `fish`,
    /// or `ps`.
    #[error(
        "target '{target}' command '{command}' declares none of 'cmd', 'sh', 'fish', or 'ps' (it must set at least one)"
    )]
    MissingCommandBody {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
    },
    /// A target's command declared both a `cmd` array and a shell variant; the
    /// two kinds are mutually exclusive.
    #[error(
        "target '{target}' command '{command}' declares both 'cmd' and a shell variant (sh/fish/ps); they are mutually exclusive"
    )]
    AmbiguousCommandBody {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
    },
    /// A target's command declared a shell variant that is empty or only
    /// whitespace.
    #[error(
        "target '{target}' command '{command}' has an empty '{variant}' (it must contain a command line to run)"
    )]
    EmptyShell {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
        /// Which shell variant was blank: `sh`, `fish`, or `ps`.
        variant: &'static str,
    },
    /// A command was selected to run but declares no variant for the detected
    /// shell, so there is nothing to run under it.
    #[error(
        "target '{target}' command '{command}' has no '{shell}' variant for the current shell (add a '{shell}' command line, or run from a shell whose variant is defined)"
    )]
    MissingShellVariant {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
        /// The detected shell's variant key: `sh`, `fish`, or `ps`.
        shell: &'static str,
    },
    /// A command's `platform` list named an unrecognized token (likely a typo).
    #[error(
        "target '{target}' command '{command}' has invalid platform '{token}' (valid: an OS like linux, macos, windows, freebsd, … or a family like unix, windows)"
    )]
    InvalidPlatform {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
        /// The unrecognized platform token.
        token: String,
    },
    /// A command's `arch` list named an unrecognized token (likely a typo).
    #[error(
        "target '{target}' command '{command}' has invalid arch '{token}' (valid: x86_64, aarch64, arm, x86, riscv64, …)"
    )]
    InvalidArch {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
        /// The unrecognized architecture token.
        token: String,
    },
    /// A command declared an empty `platform` or `arch` list, which would gate
    /// the command out on every host.
    #[error(
        "target '{target}' command '{command}' has an empty '{key}' list (omit it to run everywhere, or list at least one token)"
    )]
    EmptyPlatformList {
        /// The target that owns the offending command.
        target: String,
        /// The offending command's name.
        command: String,
        /// Which gating key was empty: `platform` or `arch`.
        key: &'static str,
    },
    /// A target declared two commands with the same `name`.
    #[error(
        "target '{target}' declares duplicate command name '{command}' (each [[target.{target}.command]] must have a unique name)"
    )]
    DuplicateCommand {
        /// The target that owns the duplicated command name.
        target: String,
        /// The command name declared more than once.
        command: String,
    },
    /// A target was requested that does not exist in the `Rakefile.toml`.
    #[error("unknown target '{name}'")]
    UnknownTarget {
        /// The requested target name.
        name: String,
    },
    /// A target's `depends_on` referenced a target that does not exist.
    #[error("target '{target}' depends on unknown target '{dependency}'")]
    UnknownDependency {
        /// The target declaring the dependency.
        target: String,
        /// The missing dependency name.
        dependency: String,
    },
    /// A target's `depends_on` lists the same name both as a regular dependency
    /// and as a skip entry (with a `^` prefix), which is contradictory.
    #[error(
        "target '{target}' lists '{name}' in depends_on both as a dependency and a skip (^{name}); remove one"
    )]
    ConflictingDependency {
        /// The target with the conflicting `depends_on` entry.
        target: String,
        /// The name that appears as both a dep and a skip.
        name: String,
    },
    /// The dependency graph contains a cycle.
    #[error("circular dependency detected: {}", .cycle.join(" -> "))]
    CircularDependency {
        /// The cycle path, e.g. `["a", "b", "c", "a"]`.
        cycle: Vec<String>,
    },
    /// A target requested to be skipped (e.g. `^clean`) is depended on by
    /// another target that still runs in this invocation, so skipping it would
    /// run that target without its prerequisite.
    #[error("target '{target}' cannot be skipped: required by {dependents}")]
    SkipNotAllowed {
        /// The target that was requested to be skipped.
        target: String,
        /// The non-root targets that depend on it, comma-joined.
        dependents: String,
    },
    /// A target's program could not be launched.
    #[error("failed to run target '{target}' command '{command}': could not launch '{program}'")]
    Spawn {
        /// The target whose command failed to launch.
        target: String,
        /// The name of the command that failed to launch.
        command: String,
        /// The program that could not be launched.
        program: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A target's `tools` listed a name with no matching `[tool.cargo.<name>]`
    /// or `[tool.os.<name>]` entry.
    #[error("target '{target}' needs unknown tool '{tool}'")]
    UnknownTool {
        /// The target declaring the tool dependency.
        target: String,
        /// The missing tool name.
        tool: String,
    },
    /// A tool's `check` or `install` command was declared empty.
    #[error(
        "tool '{tool}' has an empty '{field}' command (it must contain at least the program to run)"
    )]
    EmptyToolCommand {
        /// The offending tool name.
        tool: String,
        /// Which command was empty: `check` or `install`.
        field: &'static str,
    },
    /// A tool set `update = true` under the `crates-io` semver check but did not
    /// declare the `crate` needed to query crates.io.
    #[error(
        "tool '{tool}' sets update = true with the crates-io semver check but declares no 'crate' to look up"
    )]
    ToolUpdateMissingCrate {
        /// The offending tool name.
        tool: String,
    },
    /// A tool's `install` program could not be launched.
    #[error("failed to install tool '{tool}': could not launch '{program}'")]
    ToolInstallSpawn {
        /// The tool whose install command failed to launch.
        tool: String,
        /// The program that could not be launched.
        program: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A tool's `install` command ran but exited non-zero.
    #[error("failed to install tool '{tool}': install command exited with {status}")]
    ToolInstallFailed {
        /// The tool that failed to install.
        tool: String,
        /// The non-zero exit status of the install command.
        status: ExitStatus,
    },
    /// A required OS tool is not installed and declares no `install` command for
    /// `rake` to run, so the run is aborted with the requirement (and any `hint`).
    #[error(
        "{}",
        match .hint {
            Some(hint) => format!(
                "the '{tool}' tool is required but not installed.\n{hint}"
            ),
            None => format!(
                "the '{tool}' tool is required but not installed.\nInstall it and try again."
            ),
        }
    )]
    RequiredToolMissing {
        /// The missing OS tool's name.
        tool: String,
        /// An optional, tool-supplied hint describing how to install it.
        hint: Option<String>,
    },
    /// A tool name was declared in more than one of the `[tool.cargo]`,
    /// `[tool.os]`, or `[tool.fish]` categories; tool reference names share
    /// one flat namespace.
    #[error(
        "tool '{tool}' is declared in more than one category (tool names must be unique across [tool.cargo], [tool.os], and [tool.fish])"
    )]
    DuplicateTool {
        /// The name declared in both categories.
        tool: String,
    },
    /// A Rust toolchain (cargo) is required to run targets but none is available
    /// — it was not found and installation was declined, impossible (no
    /// interactive terminal), or did not make cargo available.
    #[error(
        "a Rust toolchain is required to run targets, but 'cargo' was not found.\nInstall Rust from https://rustup.rs and try again."
    )]
    RustToolchainMissing,
    /// The rustup installer could not be launched.
    #[error("failed to install Rust: could not launch '{program}'")]
    RustInstallSpawn {
        /// The program that could not be launched (the installer shell).
        program: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The rustup installer ran but exited non-zero.
    #[error("failed to install Rust: the rustup installer exited with {status}")]
    RustInstallFailed {
        /// The non-zero exit status of the installer.
        status: ExitStatus,
    },
    /// The requested Rust toolchain channel is not installed and installing it
    /// was declined or impossible (no interactive terminal).
    #[error(
        "the '{toolchain}' Rust toolchain is required but not installed.\nInstall it with `rustup toolchain install {toolchain}` and try again."
    )]
    RustChannelMissing {
        /// The requested toolchain channel that is not installed.
        toolchain: String,
    },
    /// The top-level `toolchain` value is empty or contains whitespace.
    #[error(
        "invalid toolchain '{value}': it must be a single non-empty token (e.g. stable, nightly, 1.89.0)"
    )]
    InvalidToolchain {
        /// The offending `toolchain` value.
        value: String,
    },
}

/// A `Result` alias using this crate's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
