//! Shared clap command-line definition for the `rake` and `cargo-rake`
//! binaries.
//!
//! Both front-ends parse the same arguments; keeping the definition here means
//! they cannot drift apart and lets `xtask` reuse the exact [`clap::Command`]
//! to generate the man page and shell completions.

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

/// A configuration-driven build tool.
///
/// Targets are declared in a `Rakefile.toml`. With no subcommand, the `default`
/// target is run.
#[derive(Debug, Parser)]
#[command(name = "rake")]
pub struct Cli {
    /// Path to the Rakefile.
    #[arg(short, long, global = true, default_value = "Rakefile.toml")]
    pub file: PathBuf,
    /// Print the targets and commands that would run without executing anything.
    /// Tool checks and toolchain operations are also skipped. Configuration
    /// errors (unknown targets, missing shell variants) are still reported.
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// What to do (defaults to running the `default` target).
    #[command(subcommand)]
    pub action: Option<Action>,
}

/// The action to perform: a built-in subcommand (`list`/`syntax`) or, by
/// default, running the named targets.
///
/// `list` and `syntax` are reserved words; a target sharing one of those names
/// cannot be run by name (run it via a parent target instead).
#[derive(Debug, Subcommand)]
pub enum Action {
    /// List the available targets and their commands.
    List,
    /// Parse and validate the Rakefile, reporting any errors.
    Syntax,
    /// Run the named targets (the default action). Runs the union of their
    /// dependency graphs, each target at most once. Prefix a target with `^`
    /// (e.g. `^clean`) to skip it: that target, and any dependency reachable
    /// only through it, is pruned from the run — allowed only when no other
    /// target that still runs depends on it.
    #[command(external_subcommand)]
    Run(Vec<String>),
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
