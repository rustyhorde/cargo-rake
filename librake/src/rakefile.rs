//! The `Rakefile.toml` model: parsing, validation, and target execution.

use std::{
    collections::HashSet,
    io::{IsTerminal, Write, stderr},
    path::Path,
    process::Command as ProcessCommand,
    process::ExitStatus,
    time::{Duration, Instant},
};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    graph, tool,
    tool::Tool,
};

/// A single named command within a target.
#[derive(Debug, Deserialize)]
pub struct Command {
    /// A label for this command, used in `--list` output and error messages.
    pub name: String,
    /// The command to run, as a program followed by its arguments. Spawned
    /// directly (no shell), so it behaves identically on every platform.
    pub cmd: Vec<String>,
    /// When `true`, a non-zero exit from this command is tolerated: the target
    /// continues with its remaining commands instead of aborting the
    /// dependency chain. Defaults to `false`.
    #[serde(default)]
    pub skip_on_error: bool,
}

/// A single named target from the `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Target {
    /// The commands to run, in array (declaration) order.
    #[serde(rename = "command", default)]
    pub commands: Vec<Command>,
    /// Other targets that must run, in order, before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Names of `[tool.<name>]` entries this target needs; each is ensured
    /// (installed if missing) before the target's commands run.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A parsed `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Rakefile {
    #[serde(rename = "target", default)]
    targets: IndexMap<String, Target>,
    #[serde(rename = "tool", default)]
    tools: IndexMap<String, Tool>,
    /// The Rust toolchain channel the project requires (`stable`, `beta`,
    /// `nightly`, or any valid rustup toolchain such as `1.89.0`). Optional:
    /// when present, both binaries verify/install and pin the run to it; when
    /// omitted (`None`) the active toolchain is used as-is.
    #[serde(default)]
    toolchain: Option<String>,
}

/// The outcome of running a target: the exit status of the last command that
/// ran, plus the total wall-clock time spent running the chain.
#[derive(Debug, Clone, Copy)]
pub struct RunReport {
    /// The [`ExitStatus`] of the last command to run, or `None` when no command
    /// ran at all (a target chain defined purely by `depends_on`). Callers
    /// should treat `None` as success.
    pub status: Option<ExitStatus>,
    /// Total wall-clock time spent running the target and its transitive
    /// dependencies.
    pub elapsed: Duration,
}

impl Rakefile {
    /// Load and validate a `Rakefile.toml` from `path`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if `path` cannot be read, or any error from
    /// [`Rakefile::from_toml_str`] if the contents are invalid.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_toml_str(&contents)
    }

    /// Parse and validate a `Rakefile.toml` from a string.
    ///
    /// # Errors
    /// Returns [`Error::Parse`] if `s` is not valid TOML, or
    /// [`Error::EmptyTarget`] (a target with neither commands nor dependencies)
    /// / [`Error::EmptyCmd`] / [`Error::DuplicateCommand`] (two commands in one
    /// target sharing a `name`) / [`Error::UnknownDependency`] /
    /// [`Error::CircularDependency`] if validation fails.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let rakefile: Rakefile = toml::from_str(s)?;
        rakefile.validate()?;
        Ok(rakefile)
    }

    /// Every target must define at least one command or dependency, each
    /// command's `cmd` must be non-empty, the `toolchain` must be a single
    /// non-empty token, and the dependency graph must be valid (no unknown
    /// dependencies, no cycles).
    fn validate(&self) -> Result<()> {
        // When declared, the channel must be a single clean token, so it can be
        // passed safely to the rustup installer as a `--default-toolchain` arg.
        if let Some(toolchain) = &self.toolchain
            && (toolchain.is_empty() || toolchain.chars().any(char::is_whitespace))
        {
            return Err(Error::InvalidToolchain {
                value: toolchain.clone(),
            });
        }
        for (name, target) in &self.targets {
            if target.commands.is_empty() && target.depends_on.is_empty() {
                return Err(Error::EmptyTarget {
                    target: name.clone(),
                });
            }
            let mut seen: HashSet<&str> = HashSet::new();
            for command in &target.commands {
                if command.cmd.is_empty() {
                    return Err(Error::EmptyCmd {
                        target: name.clone(),
                        command: command.name.clone(),
                    });
                }
                if !seen.insert(command.name.as_str()) {
                    return Err(Error::DuplicateCommand {
                        target: name.clone(),
                        command: command.name.clone(),
                    });
                }
            }
        }
        graph::validate(&self.targets)?;
        tool::validate(&self.tools, &self.targets)
    }

    /// The targets, in declaration order.
    #[must_use]
    pub fn targets(&self) -> &IndexMap<String, Target> {
        &self.targets
    }

    /// The declared tools, in declaration order.
    #[must_use]
    pub fn tools(&self) -> &IndexMap<String, Tool> {
        &self.tools
    }

    /// The Rust toolchain channel this Rakefile targets, if one is declared.
    #[must_use]
    pub fn toolchain(&self) -> Option<&str> {
        self.toolchain.as_deref()
    }

    /// Look up a single target by name.
    #[must_use]
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    /// Run `names` (root targets) after their transitive dependencies.
    ///
    /// Each root's dependency graph runs in full, in the order the roots are
    /// given: targets run in dependency order (each at most once within a single
    /// root's graph, but once per root when shared across roots), and within a
    /// target its commands run in array order. Tools, however, are ensured at
    /// most once for the whole run even when shared across roots.
    /// Execution stops at the first command that exits non-zero, returning that
    /// [`ExitStatus`]; otherwise the final command's status is returned. A
    /// command that sets `skip_on_error` is the exception: a non-zero exit there
    /// is tolerated and execution continues with the target's remaining commands
    /// and its dependents. A command that runs but fails is not an [`Error`] —
    /// the caller decides what to do with the exit code.
    ///
    /// The returned [`RunReport`] carries the last command's status (its
    /// `status` is `None` when no command runs at all — targets, and their
    /// transitive dependencies, defined purely by `depends_on`; callers should
    /// treat that as success) and the total wall-clock time spent.
    ///
    /// # Errors
    /// Returns [`Error::UnknownTarget`] if any entry in `names` is not defined,
    /// or [`Error::Spawn`] if a command's program cannot be launched.
    pub fn run(&self, names: &[&str]) -> Result<RunReport> {
        let order = graph::execution_order(&self.targets, names)?;
        let start = Instant::now();
        let mut status = None;
        let mut ensured: HashSet<&str> = HashSet::new();
        for step in order {
            let Some(target) = self.targets.get(step) else {
                continue;
            };
            // Ensure each referenced tool is available, at most once per run.
            for name in &target.tools {
                if ensured.insert(name.as_str())
                    && let Some(t) = self.tools.get(name)
                {
                    tool::ensure(name, t)?;
                }
            }
            let (current, stop) = run_one(step, target)?;
            if current.is_some() {
                status = current;
            }
            if stop {
                break;
            }
        }
        Ok(RunReport {
            status,
            elapsed: start.elapsed(),
        })
    }
}

/// Run a target's commands in array order. Returns the last status run (or
/// `None` when the target is a dependency-only aggregator with no commands) and
/// whether execution should stop (a command failed without `skip_on_error`).
fn run_one(name: &str, target: &Target) -> Result<(Option<ExitStatus>, bool)> {
    let mut last = None;
    for command in &target.commands {
        let start = Instant::now();
        let status = spawn_command(name, command)?;
        print_runtime("Cmd Runtime", start.elapsed());
        last = Some(status);
        if !status.success() && !command.skip_on_error {
            return Ok((Some(status), true));
        }
    }
    Ok((last, false))
}

/// SGR escape for bold cyan (command and tool status-line prefixes), matching
/// the color cargo paints its own status gutter.
pub(crate) const BOLD_CYAN: &str = "\x1b[1;36m";
/// SGR escape for bold green (runtime labels).
pub(crate) const BOLD_GREEN: &str = "\x1b[1;32m";
/// SGR escape for bold yellow — the value of the final total `Runtime` line.
pub(crate) const BOLD_YELLOW: &str = "\x1b[1;33m";
/// SGR escape for bold magenta — the [`RAKE_TAG`] marker prefixing every status
/// line's info, matching nextest's package-name color so rake's own output reads
/// apart from the subprocesses it spawns.
pub(crate) const BOLD_MAGENTA: &str = "\x1b[1;35m";
/// SGR escape for green (non-bold) — the command name (`[ <name> ]`) in a
/// `Running` status line.
pub(crate) const GREEN: &str = "\x1b[32m";
/// SGR escape resetting all attributes.
pub(crate) const RESET: &str = "\x1b[0m";

/// The marker prefixing each status line's info to identify rake's own output.
pub(crate) const RAKE_TAG: &str = "[ rake ]";

/// The complete set of status-label prefixes: the command (`Running`) and
/// runtime labels plus the tool-ensure verbs. The longest of these sets the
/// shared right-justification column width ([`LABEL_WIDTH`]); command names are
/// not involved (they live in the line's info, after `Running`).
const STATUS_LABELS: &[&str] = &[
    "Running",
    "Cmd Runtime",
    "Runtime",
    "Checking",
    "Installing",
    "Present",
    "Up to date",
    "Updating",
    "Warning",
];

/// The longest [`STATUS_LABELS`] entry, in bytes (all ASCII, so == chars).
const fn max_label_width() -> usize {
    let mut max = 0;
    let mut i = 0;
    while i < STATUS_LABELS.len() {
        let len = STATUS_LABELS[i].len();
        if len > max {
            max = len;
        }
        i += 1;
    }
    max
}

/// Cargo right-justifies its own status labels into a 12-column gutter; we match
/// it so rake's lines align with cargo's output.
const CARGO_LABEL_WIDTH: usize = 12;

/// The shared column every status-label prefix is right-justified into: the
/// wider of cargo's [`CARGO_LABEL_WIDTH`] gutter and the longest
/// [`STATUS_LABELS`] entry, so the output lines up with cargo and a longer label
/// still widens the column automatically.
const LABEL_WIDTH: usize = {
    let derived = max_label_width();
    if derived > CARGO_LABEL_WIDTH {
        derived
    } else {
        CARGO_LABEL_WIDTH
    }
};

/// The uncolored `label (rake) info` status line, with `label` right-justified
/// into a `width`-char column and the [`RAKE_TAG`] marker prefixing the info.
/// With an empty `info` only the justified label is emitted (no tag, no trailing
/// space), e.g. `label_line("Checking", "", 13)`.
fn label_line(label: &str, info: &str, width: usize) -> String {
    if info.is_empty() {
        format!("{label:>width$}")
    } else {
        format!("{label:>width$} {RAKE_TAG} {info}")
    }
}

/// Whether stderr output should be colored: it is a TTY and `NO_COLOR` is unset.
pub(crate) fn color_stderr() -> bool {
    stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Print a status line to stderr: `label` right-justified into the shared
/// [`LABEL_WIDTH`] column and painted `color` (bold), then the bold-magenta
/// [`RAKE_TAG`] marker and `info` — `info` painted `value_color` when one is
/// given, else the default terminal color. All coloring applies only when stderr
/// is a TTY and `NO_COLOR` is unset. Write errors are ignored — best-effort.
fn print_justified(color: &str, label: &str, info: &str, value_color: Option<&str>) {
    let width = LABEL_WIDTH;
    let mut err = stderr();
    let result = if color_stderr() {
        if info.is_empty() {
            writeln!(err, "{color}{label:>width$}{RESET}")
        } else {
            let value = match value_color {
                Some(vc) => format!("{vc}{info}{RESET}"),
                None => info.to_string(),
            };
            writeln!(
                err,
                "{color}{label:>width$}{RESET} {BOLD_MAGENTA}{RAKE_TAG}{RESET} {value}"
            )
        }
    } else {
        writeln!(err, "{}", label_line(label, info, width))
    };
    // Best-effort; drop any write error (`.ok()` first so the discarded value
    // carries no destructor, satisfying `let_underscore_drop`).
    let _ = result.ok();
}

/// Print a bold-cyan-prefixed status line (commands and tools), right-justified
/// into the shared [`LABEL_WIDTH`] column.
pub(crate) fn print_label(label: &str, info: &str) {
    print_justified(BOLD_CYAN, label, info, None);
}

/// Spawn a single named command, inheriting the parent's stdio. A blank line and
/// the command's status line (`Running [ <name> ] <program args>`, the name
/// green on a TTY) are printed first.
fn spawn_command(target: &str, command: &Command) -> Result<ExitStatus> {
    let (program, args) = command.cmd.split_first().ok_or_else(|| Error::EmptyCmd {
        target: target.to_string(),
        command: command.name.clone(),
    })?;
    // A blank line separates each command block from the previous output.
    let _ = writeln!(stderr()).ok();
    let name = if color_stderr() {
        format!("{GREEN}[ {} ]{RESET}", command.name)
    } else {
        format!("[ {} ]", command.name)
    };
    print_label("Running", &format!("{name} {}", command.cmd.join(" ")));
    ProcessCommand::new(program)
        .args(args)
        .status()
        .map_err(|source| Error::Spawn {
            target: target.to_string(),
            command: command.name.clone(),
            program: program.clone(),
            source,
        })
}

/// Microseconds in a millisecond.
const US_PER_MS: u128 = 1_000;
/// Microseconds in a second.
const US_PER_S: u128 = 1_000_000;
/// Microseconds in a minute.
const US_PER_MIN: u128 = 60_000_000;

/// Minimum width of the integer part before the decimal point, space-padded so
/// runtimes line up on the decimal point across `Cmd Runtime`/`Runtime` lines.
const INT_WIDTH: usize = 4;
/// Number of fractional digits rendered after the decimal point.
const FRAC_DIGITS: usize = 5;
/// `10^FRAC_DIGITS`, used to scale a sub-unit remainder to [`FRAC_DIGITS`] places.
const FRAC_SCALE: u128 = 100_000;

/// Render `value` microseconds as a decimal count of `unit`-microsecond units:
/// the integer part (space-padded to [`INT_WIDTH`] so values line up on the
/// decimal point), then exactly [`FRAC_DIGITS`] fractional digits, zero-padded
/// (and truncated to that precision), e.g. `decimal(1_010, 1_000)` is
/// `"   1.01000"`.
fn decimal(value: u128, unit: u128) -> String {
    let int = value / unit;
    let frac = (value % unit) * FRAC_SCALE / unit;
    format!("{int:>INT_WIDTH$}.{frac:0FRAC_DIGITS$}")
}

/// Format `elapsed` with microsecond precision, promoting the unit as the value
/// grows: `µs`, then `ms`, then `s`, then composite `min`/`s` at the top tier.
/// Every tier carries exactly [`FRAC_DIGITS`] digits after the decimal,
/// zero-padded, with the integer part space-padded to [`INT_WIDTH`], e.g.
/// ` 523.00000 µs`, `   1.01000 ms`, `   1.50100 s`, `1 min   30.50000 s`.
#[must_use]
pub fn format_duration(elapsed: Duration) -> String {
    let us = elapsed.as_micros();
    if us < US_PER_MS {
        format!("{} µs", decimal(us, 1))
    } else if us < US_PER_S {
        format!("{} ms", decimal(us, US_PER_MS))
    } else if us < US_PER_MIN {
        format!("{} s", decimal(us, US_PER_S))
    } else {
        let mins = us / US_PER_MIN;
        let rem = us % US_PER_MIN;
        format!("{mins} min {} s", decimal(rem, US_PER_S))
    }
}

/// Print `label` (right-justified into the shared [`LABEL_WIDTH`] column,
/// bold-green when stderr is a TTY and `NO_COLOR` is unset) followed by the
/// [`format_duration`] rendering of `elapsed`. Justifying to the shared width
/// lines the times up with the command/tool status lines and across the
/// per-command `Cmd Runtime` and final `Runtime` lines. Write errors are
/// ignored — this output is best-effort.
pub fn print_runtime(label: &str, elapsed: Duration) {
    print_justified(BOLD_GREEN, label, &format_duration(elapsed), None);
}

/// Like [`print_runtime`] for the binaries' final total: a bold-green `Runtime`
/// label, but the [`format_duration`] value painted bold yellow to set the
/// overall total apart from the per-command times.
pub fn print_total_runtime(elapsed: Duration) {
    print_justified(
        BOLD_GREEN,
        "Runtime",
        &format_duration(elapsed),
        Some(BOLD_YELLOW),
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Rakefile, format_duration};
    use crate::error::Error;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn format_duration_promotes_units() {
        let cases: &[(u64, &str)] = &[
            (0, "   0.00000 µs"),
            (100, " 100.00000 µs"),
            (999, " 999.00000 µs"),
            (1_000, "   1.00000 ms"),
            (1_010, "   1.01000 ms"),
            (1_000_000, "   1.00000 s"),
            (1_501_000, "   1.50100 s"),
            (60_000_000, "1 min    0.00000 s"),
            (90_500_000, "1 min   30.50000 s"),
            (3_661_500_000, "61 min    1.50000 s"),
        ];
        for &(us, expected) in cases {
            assert_eq!(format_duration(Duration::from_micros(us)), expected);
        }
    }

    const SAMPLE: &str = r#"
[[target.build.command]]
name = "compile"
cmd = ["cargo", "build", "--all-features"]

[[target.test.command]]
name = "run"
cmd = ["cargo", "test"]

[target.all]
depends_on = ["build", "test"]

[[target.all.command]]
name = "release"
cmd = ["cargo", "build", "--release"]

[[target.all.command]]
name = "doc"
cmd = ["cargo", "doc"]

[[target.'a fancy target name'.command]]
name = "doc"
cmd = ["cargo", "doc"]
"#;

    #[test]
    fn label_line_right_justifies_prefix() {
        use super::LABEL_WIDTH;
        // Commands print a fixed "Running" prefix (7 chars) justified into the
        // 12-char column: 5 leading spaces, then the "[ rake ]" tag before the info.
        assert_eq!(
            super::label_line(
                "Running",
                "[compile] -> cargo build --all-features",
                LABEL_WIDTH
            ),
            "     Running [ rake ] [compile] -> cargo build --all-features"
        );
        // Empty info emits only the justified label, with no trailing space
        // ("Checking" is 8 chars -> 4 leading spaces).
        assert_eq!(
            super::label_line("Checking", "", LABEL_WIDTH),
            "    Checking"
        );
    }

    #[test]
    fn parses_targets_in_declaration_order() -> TestResult {
        let rakefile = Rakefile::from_toml_str(SAMPLE)?;
        let names: Vec<&str> = rakefile.targets().keys().map(String::as_str).collect();
        assert_eq!(names, vec!["build", "test", "all", "a fancy target name"]);

        let all = rakefile.target("all").ok_or("expected an 'all' target")?;
        assert_eq!(
            all.depends_on,
            vec!["build".to_string(), "test".to_string()]
        );
        // Commands within a target keep their array (declaration) order.
        let commands: Vec<&str> = all.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, vec!["release", "doc"]);
        Ok(())
    }

    #[test]
    fn toolchain_absent_is_none() -> TestResult {
        // SAMPLE omits `toolchain`, so the key is absent.
        assert_eq!(Rakefile::from_toml_str(SAMPLE)?.toolchain(), None);
        Ok(())
    }

    #[test]
    fn toolchain_round_trips_explicit_value() -> TestResult {
        let src = format!("toolchain = \"nightly\"\n{SAMPLE}");
        assert_eq!(Rakefile::from_toml_str(&src)?.toolchain(), Some("nightly"));
        Ok(())
    }

    #[test]
    fn invalid_toolchain_is_rejected() -> TestResult {
        for value in ["", "night ly", " stable", "a\tb"] {
            let src = format!("toolchain = \"{value}\"\n{SAMPLE}");
            match Rakefile::from_toml_str(&src) {
                Err(Error::InvalidToolchain { value: got }) => assert_eq!(got, value),
                other => {
                    return Err(
                        format!("expected InvalidToolchain for {value:?}, got {other:?}").into(),
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn empty_file_has_no_targets() -> TestResult {
        let rakefile = Rakefile::from_toml_str("")?;
        assert!(rakefile.targets().is_empty());
        Ok(())
    }

    #[test]
    fn target_with_neither_commands_nor_deps_is_rejected() -> TestResult {
        let toml = "[target.build]\ndepends_on = []\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyTarget { target }) => {
                assert_eq!(target, "build");
                Ok(())
            }
            other => Err(format!("expected EmptyTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn depends_only_target_is_accepted() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"compile\"\ncmd = [\"true\"]\n\
                    [target.all]\ndepends_on = [\"build\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let all = rakefile.target("all").ok_or("expected an 'all' target")?;
        assert!(all.commands.is_empty());
        assert_eq!(all.depends_on, vec!["build".to_string()]);
        Ok(())
    }

    #[test]
    fn missing_command_name_is_a_parse_error() -> TestResult {
        let toml = "[[target.build.command]]\ncmd = [\"cargo\", \"build\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::Parse(_)) => Ok(()),
            other => Err(format!("expected Parse error, got {other:?}").into()),
        }
    }

    #[test]
    fn empty_cmd_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"compile\"\ncmd = []\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyCmd { target, command }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "compile");
                Ok(())
            }
            other => Err(format!("expected EmptyCmd, got {other:?}").into()),
        }
    }

    #[test]
    fn duplicate_command_name_within_target_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::DuplicateCommand { target, command }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                Ok(())
            }
            other => Err(format!("expected DuplicateCommand, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_dependency_is_rejected() -> TestResult {
        let toml = "[target.build]\ndepends_on = [\"nope\"]\n\
                    [[target.build.command]]\nname = \"compile\"\ncmd = [\"cargo\", \"build\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::UnknownDependency { target, dependency }) => {
                assert_eq!(target, "build");
                assert_eq!(dependency, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn cycle_is_rejected() -> TestResult {
        let toml = "[target.a]\ndepends_on = [\"b\"]\n\
                    [[target.a.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.b]\ndepends_on = [\"a\"]\n\
                    [[target.b.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::CircularDependency { .. }) => Ok(()),
            other => Err(format!("expected CircularDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn run_unknown_target_errors() -> TestResult {
        let rakefile = Rakefile::from_toml_str(SAMPLE)?;
        match rakefile.run(&["does-not-exist"]) {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "does-not-exist");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn run_missing_program_is_spawn_error() -> TestResult {
        let toml = "[[target.go.command]]\nname = \"ghost\"\n\
                    cmd = [\"this-program-does-not-exist-cargo-rake\", \"--version\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.run(&["go"]) {
            Err(Error::Spawn {
                target,
                command,
                program,
                ..
            }) => {
                assert_eq!(target, "go");
                assert_eq!(command, "ghost");
                assert_eq!(program, "this-program-does-not-exist-cargo-rake");
                Ok(())
            }
            other => Err(format!("expected Spawn error, got {other:?}").into()),
        }
    }

    #[test]
    fn run_portable_command_succeeds() -> TestResult {
        let toml = "[[target.version.command]]\nname = \"ver\"\ncmd = [\"cargo\", \"--version\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["version"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_defaults_to_false() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"compile\"\ncmd = [\"cargo\", \"build\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let build = rakefile
            .target("build")
            .ok_or("expected a 'build' target")?;
        let command = build.commands.first().ok_or("expected a command")?;
        assert!(!command.skip_on_error);
        Ok(())
    }

    #[test]
    fn commands_run_in_array_order_stop_at_failure() -> TestResult {
        let toml = "[[target.demo.command]]\nname = \"boom\"\ncmd = [\"false\"]\n\
                    [[target.demo.command]]\nname = \"after\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // The first command fails without `skip_on_error`, so execution stops
        // there and `after` never runs: the returned status is the failure.
        let status = rakefile.run(&["demo"])?.status.ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_continues_remaining_commands() -> TestResult {
        let toml = "[[target.demo.command]]\nname = \"boom\"\ncmd = [\"false\"]\nskip_on_error = true\n\
                    [[target.demo.command]]\nname = \"after\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `boom` fails but opts into skipping, so `after` still runs and its
        // success is the status returned.
        let status = rakefile.run(&["demo"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_continues_chain() -> TestResult {
        let toml = "[[target.flaky.command]]\nname = \"boom\"\ncmd = [\"false\"]\nskip_on_error = true\n\
                    [target.all]\ndepends_on = [\"flaky\"]\n\
                    [[target.all.command]]\nname = \"ok\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `flaky` exits non-zero but opts into skipping, so `all` still runs and
        // its success is the status returned for the whole chain.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn failing_dependency_without_skip_aborts() -> TestResult {
        let toml = "[[target.flaky.command]]\nname = \"boom\"\ncmd = [\"false\"]\n\
                    [target.all]\ndepends_on = [\"flaky\"]\n\
                    [[target.all.command]]\nname = \"ok\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `flaky` fails and does not skip, so the chain stops there: `all` never
        // runs and the returned status reflects the failure.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn depends_only_target_runs_dependencies() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"compile\"\ncmd = [\"true\"]\n\
                    [[target.test.command]]\nname = \"check\"\ncmd = [\"true\"]\n\
                    [target.all]\ndepends_on = [\"build\", \"test\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `all` has no command of its own; its status is that of the last
        // dependency to run.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn multiple_root_targets_run_in_one_call() -> TestResult {
        // Two independent roots given together both run; the run succeeds.
        let toml = "[[target.one.command]]\nname = \"a\"\ncmd = [\"true\"]\n\
                    [[target.two.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["one", "two"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn shared_dependency_of_two_roots_stops_run_once() -> TestResult {
        // Both roots depend on `flaky`, which fails without `skip_on_error`.
        // The shared dep runs once and aborts the whole merged chain.
        let toml = "[[target.flaky.command]]\nname = \"boom\"\ncmd = [\"false\"]\n\
                    [target.one]\ndepends_on = [\"flaky\"]\n\
                    [[target.one.command]]\nname = \"a\"\ncmd = [\"true\"]\n\
                    [target.two]\ndepends_on = [\"flaky\"]\n\
                    [[target.two.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["one", "two"])?
            .status
            .ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn run_with_present_tool_succeeds() -> TestResult {
        // The tool's `check` (`true`) reports it present, so `install` (`false`,
        // which would fail) never runs and the target's command still runs.
        let toml = "[tool.t]\ncheck = [\"true\"]\ninstall = [\"false\"]\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn run_with_failing_tool_install_aborts() -> TestResult {
        // The tool is absent (`check` is `false`) and its `install` fails, so the
        // run errors before the target's command runs.
        let toml = "[tool.t]\ncheck = [\"false\"]\ninstall = [\"false\"]\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.run(&["build"]) {
            Err(Error::ToolInstallFailed { tool, .. }) => {
                assert_eq!(tool, "t");
                Ok(())
            }
            other => Err(format!("expected ToolInstallFailed, got {other:?}").into()),
        }
    }

    #[test]
    fn shared_tool_is_ensured_once_per_run() -> TestResult {
        // Both `build` and its dependency `dep` reference the same present tool;
        // ensuring is deduped, so the run completes (and `install`, `false`,
        // never runs even though two targets reference the tool).
        let toml = "[tool.t]\ncheck = [\"true\"]\ninstall = [\"false\"]\n\
                    [target.dep]\ntools = [\"t\"]\n\
                    [[target.dep.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.build]\ntools = [\"t\"]\ndepends_on = [\"dep\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }
}
