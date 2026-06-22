//! Error types for `librake`.

use std::io;

/// Errors that can occur while loading, validating, or running a `Rakefile.toml`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `Rakefile.toml` could not be read from disk.
    #[error("unable to read Rakefile")]
    Io(#[from] io::Error),
    /// The `Rakefile.toml` contents could not be parsed (covers a missing `cmd`).
    #[error("unable to parse Rakefile")]
    Parse(#[from] toml::de::Error),
    /// A target declared a `cmd` with no program to run.
    #[error("target '{target}' has an empty 'cmd' (it must contain at least the program to run)")]
    EmptyCmd {
        /// The offending target name.
        target: String,
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
    #[error("failed to run target '{target}': could not launch '{program}'")]
    Spawn {
        /// The target whose command failed to launch.
        target: String,
        /// The program that could not be launched.
        program: String,
        /// The underlying I/O error.
        source: io::Error,
    },
}

/// A `Result` alias using this crate's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
