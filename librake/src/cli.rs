//! Shared clap command-line definition for the `rake` and `cargo-rake`
//! binaries.
//!
//! Both front-ends parse the same arguments; keeping the definition here means
//! they cannot drift apart and lets `xtask` reuse the exact [`clap::Command`]
//! to generate the man page and shell completions.

use std::path::PathBuf;

use clap::{CommandFactory, Parser};

/// A configuration-driven build tool.
///
/// Targets are declared in a `Rakefile.toml`. With no target, the `default`
/// target is run.
#[derive(Debug, Parser)]
#[command(name = "rake")]
pub struct Cli {
    /// Path to the Rakefile.
    #[arg(short, long, default_value = "Rakefile.toml")]
    pub file: PathBuf,
    /// List the available targets instead of running one.
    #[arg(short, long)]
    pub list: bool,
    /// The targets to run (defaults to "default"). Runs the union of their
    /// dependency graphs, each target at most once. Prefix a target with `^`
    /// (e.g. `^clean`) to skip it: that target, and any dependency reachable
    /// only through it, is pruned from the run — allowed only when no other
    /// target that still runs depends on it.
    pub targets: Vec<String>,
}

/// Build the [`clap::Command`] for the CLI, labelled with the given `name` and
/// `bin_name`.
///
/// The binaries layer their own `version`/`long_version` on top; `xtask` uses
/// it as-is to render the man page and shell completions.
#[must_use]
pub fn command(name: &'static str, bin_name: &'static str) -> clap::Command {
    Cli::command().name(name).bin_name(bin_name)
}
