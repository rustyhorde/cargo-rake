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
    /// The dependency graph contains a cycle.
    #[error("circular dependency detected: {}", .cycle.join(" -> "))]
    CircularDependency {
        /// The cycle path, e.g. `["a", "b", "c", "a"]`.
        cycle: Vec<String>,
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
    /// A target's `tools` listed a name with no matching `[tool.<name>]` entry.
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
}

/// A `Result` alias using this crate's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
