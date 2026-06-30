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

/// The action to perform: a built-in subcommand (`list`/`syntax`/`license`/`basic`)
/// or, by default, running the named targets.
///
/// `list`, `syntax`, `license`, and `basic` are reserved words; a target
/// sharing one of those names cannot be run by name (run it via a parent
/// target instead).
#[derive(Debug, Subcommand)]
pub enum Action {
    /// List the available targets and their commands.
    List,
    /// Parse and validate the Rakefile, reporting any errors.
    Syntax,
    /// Activate a license key and persist it for future runs, or remove a
    /// stored key with `--remove`.
    ///
    /// The key is written to the platform config directory
    /// (`~/.config/rake/license` on Linux/macOS, `%APPDATA%\rake\license` on
    /// Windows). Omit `KEY` to read the key from stdin instead (useful for
    /// piping or interactive pasting).
    #[command(name = "license")]
    License {
        /// The license key string. Omit to read from stdin.
        #[arg(conflicts_with = "remove")]
        key: Option<String>,
        /// Remove the stored license key (prompts for confirmation).
        #[arg(long, conflicts_with = "key")]
        remove: bool,
    },
    /// Show whether the `basic` licensed feature is unlocked.
    Basic,
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
