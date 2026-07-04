//! The `Rakefile.toml` model: parsing, validation, and target execution.

use std::{
    collections::HashSet,
    io::{IsTerminal, Write, stderr},
    net::SocketAddr,
    path::Path,
    process::Command as ProcessCommand,
    process::ExitStatus,
    time::{Duration, Instant},
};

use chrono::Utc;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    graph::{self, Step},
    license::LicensePayload,
    lifecycle::{Emitter, LifecycleEvent, ProjectInfo, ToolOutcome},
    tool,
    tool::{ToolTable, UpdateRecord},
};

/// A single named command within a target.
///
/// A command carries exactly one *kind* of body: either a [`cmd`](Self::cmd)
/// array (spawned directly, no shell) or one or more shell variants
/// ([`sh`](Self::sh)/[`fish`](Self::fish)/[`ps`](Self::ps)), each a command line
/// run through that shell so it expands `$(...)`/`~`/`$VAR`/globs. At run time
/// the variant matching the detected shell (see [`ShellFamily`]) is selected.
/// Validation requires at least one body and rejects mixing `cmd` with any shell
/// variant.
#[derive(Debug, Deserialize)]
pub struct Command {
    /// A label for this command, used in `list` output and error messages.
    pub name: String,
    /// The command to run, as a program followed by its arguments. Spawned
    /// directly (no shell), so it behaves identically on every platform.
    /// Mutually exclusive with `sh`/`fish`/`ps`.
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    /// A command line run through POSIX `sh -c` (selected when the detected
    /// shell is a POSIX shell). Mutually exclusive with `cmd`.
    #[serde(default)]
    pub sh: Option<String>,
    /// A command line run through `fish -c` (selected when the detected shell is
    /// fish). Mutually exclusive with `cmd`.
    #[serde(default)]
    pub fish: Option<String>,
    /// A command line run through PowerShell `-Command` (selected when the
    /// detected shell is PowerShell). Mutually exclusive with `cmd`.
    #[serde(default)]
    pub ps: Option<String>,
    /// When `true`, a non-zero exit from this command is tolerated: the target
    /// continues with its remaining commands instead of aborting the
    /// dependency chain. Defaults to `false`.
    #[serde(default)]
    pub skip_on_error: bool,
    /// The platforms this command applies to, as OS names (`linux`/`macos`/
    /// `windows`/…) or family aliases (`unix`/`windows`). When set, the command
    /// runs only if the host's OS *or* family matches one of these tokens;
    /// otherwise it is silently skipped. `None` (the default) means every
    /// platform. Orthogonal to the body kind and to [`arch`](Self::arch).
    #[serde(default)]
    pub platform: Option<Vec<String>>,
    /// The architectures this command applies to (`x86_64`/`aarch64`/…). When
    /// set, the command runs only if the host's architecture matches one of
    /// these tokens; otherwise it is silently skipped. `None` (the default)
    /// means every architecture. Combined with [`platform`](Self::platform) as a
    /// logical AND.
    #[serde(default)]
    pub arch: Option<Vec<String>>,
    /// Names of `[tool.cargo.<name>]`/`[tool.os.<name>]`/`[tool.fish.<name>]`
    /// entries this command needs; each is ensured (installed if missing)
    /// immediately before this command runs — and only when the command is not
    /// skipped by its [`platform`](Self::platform)/[`arch`](Self::arch) gates.
    /// An empty list (the default) means no command-level tool requirements.
    /// A name that also appears in the owning target's `tools` list is a
    /// validation error ([`Error::ToolDeclaredAtBothLevels`]).
    #[serde(default)]
    pub tools: Vec<String>,
}

impl Command {
    /// Why this command should be skipped on `host`, or `None` when it runs. The
    /// returned string is the unmet requirement, shown in the `Skipped` status
    /// line (e.g. `platform: linux, macos`). Each declared dimension
    /// (`platform`, then `arch`) must have a token matching the host; a
    /// `platform` token matches the host's OS *or* family, an `arch` token the
    /// host's architecture. Dimensions left unset always match.
    fn skip_reason(&self, host: &Host) -> Option<String> {
        if let Some(platforms) = &self.platform
            && !platforms.iter().any(|p| p == host.os || p == host.family)
        {
            return Some(format!("platform: {}", platforms.join(", ")));
        }
        if let Some(arches) = &self.arch
            && !arches.iter().any(|a| a == host.arch)
        {
            return Some(format!("arch: {}", arches.join(", ")));
        }
        None
    }
}

/// The host platform rake detects for the current run: the running operating
/// system, OS family, and architecture, used to gate commands by their
/// `platform`/`arch` lists. The fields mirror [`std::env::consts`]
/// (`OS`/`FAMILY`/`ARCH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// The operating system, e.g. `linux`, `macos`, `windows`
    /// ([`std::env::consts::OS`]).
    pub os: &'static str,
    /// The OS family, e.g. `unix`, `windows` ([`std::env::consts::FAMILY`]).
    pub family: &'static str,
    /// The CPU architecture, e.g. `x86_64`, `aarch64`
    /// ([`std::env::consts::ARCH`]).
    pub arch: &'static str,
}

impl Host {
    /// Detect the host platform from the running binary's compile-time targets.
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Host;
    ///
    /// let host = Host::detect();
    /// assert!(!host.os.is_empty());
    /// assert!(!host.family.is_empty());
    /// assert!(!host.arch.is_empty());
    /// ```
    #[must_use]
    pub fn detect() -> Host {
        Host {
            os: std::env::consts::OS,
            family: std::env::consts::FAMILY,
            arch: std::env::consts::ARCH,
        }
    }
}

/// Recognized operating-system tokens for a command's `platform` list (the
/// stable `target_os` values [`std::env::consts::OS`] can report). Used to
/// reject typos at validation time.
const KNOWN_OS: &[&str] = &[
    "linux",
    "macos",
    "windows",
    "freebsd",
    "netbsd",
    "openbsd",
    "dragonfly",
    "solaris",
    "illumos",
    "android",
    "ios",
    "tvos",
    "watchos",
    "visionos",
    "fuchsia",
    "redox",
    "haiku",
    "hurd",
    "aix",
    "nto",
    "emscripten",
    "wasi",
];

/// Recognized OS-family tokens for a command's `platform` list (the values
/// [`std::env::consts::FAMILY`] can report). A `platform` token may name either
/// an OS (see [`KNOWN_OS`]) or one of these families.
const KNOWN_FAMILY: &[&str] = &["unix", "windows", "wasm"];

/// Keys that may appear directly in a `[target.X]` table as base fields.
/// Any other key must be a recognized platform/family token (a variant sub-table)
/// or it is rejected with [`Error::InvalidPlatformVariant`].
const KNOWN_TARGET_BASE_KEYS: &[&str] =
    &["command", "depends_on", "tools", "events", "time_tracking"];

/// Recognized architecture tokens for a command's `arch` list (the stable
/// `target_arch` values [`std::env::consts::ARCH`] can report).
const KNOWN_ARCH: &[&str] = &[
    "x86",
    "x86_64",
    "arm",
    "aarch64",
    "loongarch64",
    "m68k",
    "csky",
    "mips",
    "mips32r6",
    "mips64",
    "mips64r6",
    "powerpc",
    "powerpc64",
    "riscv32",
    "riscv64",
    "s390x",
    "sparc",
    "sparc64",
    "wasm32",
    "wasm64",
];

/// The shell family rake detects for the current run, naming which command
/// variant (`sh`/`fish`/`ps`) is selected and how it is invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    /// A POSIX shell (`sh`/`bash`/`zsh`/`dash`/…); runs the `sh` variant.
    Posix,
    /// The fish shell; runs the `fish` variant.
    Fish,
    /// PowerShell (`pwsh`/`powershell`); runs the `ps` variant.
    Ps,
}

impl ShellFamily {
    /// The Rakefile key naming this family's command variant.
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::ShellFamily;
    ///
    /// assert_eq!(ShellFamily::Posix.key(), "sh");
    /// assert_eq!(ShellFamily::Fish.key(),  "fish");
    /// assert_eq!(ShellFamily::Ps.key(),    "ps");
    /// ```
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            ShellFamily::Posix => "sh",
            ShellFamily::Fish => "fish",
            ShellFamily::Ps => "ps",
        }
    }

    /// This family's command line on `command`, if the variant is defined.
    fn variant(self, command: &Command) -> Option<&String> {
        match self {
            ShellFamily::Posix => command.sh.as_ref(),
            ShellFamily::Fish => command.fish.as_ref(),
            ShellFamily::Ps => command.ps.as_ref(),
        }
    }

    /// The interpreter program and its command flag for this family. PowerShell
    /// prefers `pwsh` (cross-platform) and falls back to `powershell` when
    /// `pwsh` is not on `PATH`.
    fn interpreter(self) -> (String, &'static str) {
        match self {
            ShellFamily::Posix => ("sh".to_string(), "-c"),
            ShellFamily::Fish => ("fish".to_string(), "-c"),
            ShellFamily::Ps => {
                let program = if program_on_path("pwsh") {
                    "pwsh"
                } else {
                    "powershell"
                };
                (program.to_string(), "-Command")
            }
        }
    }
}

/// Whether `name` resolves to an executable on `PATH` (used to prefer `pwsh`
/// over `powershell`). On Windows, `name.exe` is also checked.
fn program_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        cfg!(windows) && dir.join(format!("{name}.exe")).is_file()
    })
}

/// Classify a shell program's basename into a [`ShellFamily`]: `fish` → fish,
/// `pwsh`/`powershell` → PowerShell, anything else → POSIX.
fn classify_shell(name: &str) -> ShellFamily {
    let name = name.to_ascii_lowercase();
    if name.contains("fish") {
        ShellFamily::Fish
    } else if name == "pwsh" || name == "powershell" {
        ShellFamily::Ps
    } else {
        ShellFamily::Posix
    }
}

/// Resolve the [`ShellFamily`] from the relevant environment signals, in
/// priority order (first match wins): a PowerShell env channel (any OS), then
/// `PSModulePath` on non-Windows (pwsh on Linux/macOS), then `FISH_VERSION`
/// (exported by fish to all child processes), then `$SHELL`'s basename, then
/// the platform default (`Ps` on Windows, else `Posix`). PowerShell is checked
/// first because it does not set `$SHELL`; `FISH_VERSION` is checked before
/// `$SHELL` because `$SHELL` reflects the login shell, not the running shell.
fn shell_family_from_env(
    has_ps_signal: bool,
    fish_version: bool,
    shell: Option<&str>,
    is_windows: bool,
) -> ShellFamily {
    if has_ps_signal {
        return ShellFamily::Ps;
    }
    if fish_version {
        return ShellFamily::Fish;
    }
    match shell.filter(|s| !s.is_empty()) {
        Some(shell) => {
            let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
            classify_shell(base)
        }
        None if is_windows => ShellFamily::Ps,
        None => ShellFamily::Posix,
    }
}

/// Detect the current shell family from the process environment.
///
/// The detection follows a fixed priority order (first match wins):
///
/// 0. `RAKE_SHELL` env variable — set to any shell name (`fish`, `sh`, `bash`,
///    `pwsh`, …) to pin the family, skipping all other detection.
/// 1. `POWERSHELL_DISTRIBUTION_CHANNEL` or `PSModulePath` (non-Windows only) —
///    checked before `$SHELL` because PowerShell does not set `$SHELL`.
/// 2. `FISH_VERSION` env variable — exported by fish to all child processes;
///    checked before `$SHELL` because `$SHELL` reflects the login shell, not
///    the running shell.
/// 3. `$SHELL`'s basename — classified into a [`ShellFamily`].
/// 4. Platform default — [`ShellFamily::Ps`] on Windows, [`ShellFamily::Posix`]
///    otherwise.
///
/// # Examples
///
/// ```no_run
/// use librake::{ShellFamily, detect_shell_family};
///
/// let family = detect_shell_family();
/// // key() returns "sh", "fish", or "ps" depending on the environment.
/// println!("shell: {}", family.key());
/// ```
#[must_use]
pub fn detect_shell_family() -> ShellFamily {
    if let Some(val) = std::env::var_os("RAKE_SHELL")
        && let Some(s) = val.to_str().filter(|s| !s.is_empty())
    {
        return classify_shell(s);
    }
    let has_ps_signal = std::env::var_os("POWERSHELL_DISTRIBUTION_CHANNEL").is_some()
        || (!cfg!(windows) && std::env::var_os("PSModulePath").is_some());
    let fish_version = std::env::var_os("FISH_VERSION").is_some();
    let shell = std::env::var_os("SHELL");
    shell_family_from_env(
        has_ps_signal,
        fish_version,
        shell.as_deref().and_then(|s| s.to_str()),
        cfg!(windows),
    )
}

impl Command {
    /// The program and arguments to spawn for this command under `family`. A
    /// `cmd` body splits into its program + args; a shell body runs the family's
    /// interpreter with its flag followed by the command line. Returns `None`
    /// when no body applies — an empty `cmd`, or the `family` variant is absent
    /// (callers turn that into a [`Error::MissingShellVariant`]).
    fn invocation(&self, family: ShellFamily) -> Option<(String, Vec<String>)> {
        if let Some(cmd) = &self.cmd {
            let (program, args) = cmd.split_first()?;
            Some((program.clone(), args.to_vec()))
        } else {
            let line = family.variant(self)?;
            let (program, flag) = family.interpreter();
            Some((program, vec![flag.to_string(), line.clone()]))
        }
    }

    /// How this command renders in `list` output (shell-agnostic): a `cmd` body joins
    /// its program + args; a shell command joins each defined variant as
    /// `"{key}: {line}"` (e.g. `sh: $(pwd) … | fish: (pwd) …`).
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Command;
    ///
    /// let cmd = Command {
    ///     name: "compile".to_string(),
    ///     cmd: Some(vec!["cargo".to_string(), "build".to_string()]),
    ///     sh: None, fish: None, ps: None,
    ///     skip_on_error: false, platform: None, arch: None, tools: vec![],
    /// };
    /// assert_eq!(cmd.display(), "cargo build");
    ///
    /// let shell = Command {
    ///     name: "archive".to_string(),
    ///     cmd: None,
    ///     sh:   Some("tar czf out.tgz .".to_string()),
    ///     fish: Some("tar czf out.tgz .".to_string()),
    ///     ps: None,
    ///     skip_on_error: false, platform: None, arch: None, tools: vec![],
    /// };
    /// assert_eq!(shell.display(), "sh: tar czf out.tgz . | fish: tar czf out.tgz .");
    /// ```
    #[must_use]
    pub fn display(&self) -> String {
        if let Some(cmd) = &self.cmd {
            return cmd.join(" ");
        }
        let mut parts = Vec::new();
        for (key, line) in [("sh", &self.sh), ("fish", &self.fish), ("ps", &self.ps)] {
            if let Some(line) = line {
                parts.push(format!("{key}: {line}"));
            }
        }
        parts.join(" | ")
    }
}

/// A single named target from the `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Target {
    /// The commands to run, in array (declaration) order.
    #[serde(rename = "command", default)]
    pub commands: Vec<Command>,
    /// Other targets that must run, in order, before this one. After
    /// [`Rakefile::from_toml_str`] normalizes the raw TOML value, this holds
    /// only the non-`^`-prefixed entries; `^`-prefixed entries are moved to
    /// [`skip_deps`](Self::skip_deps).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Targets to skip from the execution graph when this target is in the
    /// run, derived from `^`-prefixed entries in the TOML `depends_on` list.
    /// Not a TOML key — populated by [`Rakefile::from_toml_str`] after
    /// parsing.
    #[serde(skip)]
    pub skip_deps: Vec<String>,
    /// Names of `[tool.cargo.<name>]`/`[tool.os.<name>]` entries this target
    /// needs; each is ensured (installed if missing) before the target's
    /// commands run.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Whether this target participates in lifecycle events (see
    /// [`crate::lifecycle`]). Defaults to `true` (current behavior): events
    /// still require a top-level `[lifecycle]` table and the `events`
    /// license feature. Set `false` to have this target — and its
    /// commands' and tools' before/after/skip events — never fire,
    /// regardless of that gating.
    #[serde(default = "default_true")]
    pub events: bool,
    /// Whether this target participates in time tracking. Defaults to
    /// `true`. Set `false` to have a `no_time_tracking` lifecycle event
    /// fire immediately after this target's `before_target` event (still
    /// subject to the same `events`/license/`[lifecycle]` gating as every
    /// other event).
    #[serde(default = "default_true")]
    pub time_tracking: bool,
}

fn default_true() -> bool {
    true
}

/// A parsed `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Rakefile {
    #[serde(rename = "target", default)]
    targets: IndexMap<String, Target>,
    #[serde(rename = "tool", default)]
    tools: ToolTable,
    /// The Rust toolchain channel the project requires (`stable`, `beta`,
    /// `nightly`, or any valid rustup toolchain such as `1.89.0`). Optional:
    /// when present, both binaries verify/install and pin the run to it; when
    /// omitted (`None`) the active toolchain is used as-is.
    #[serde(default)]
    toolchain: Option<String>,
    /// When `true` (the default), `cargo-rake` checks crates.io for a newer
    /// version of itself on startup and installs it if found. Set `false` to
    /// disable the check entirely.
    #[serde(default = "default_true")]
    update: bool,
    /// Targets that exist only as platform-specific variants
    /// (`[target.X.linux]`, etc.) and were excluded because no variant matched
    /// the current host. Not a TOML field — populated by
    /// [`from_toml_str_with_host`](Self::from_toml_str_with_host).
    /// Carried through to graph validation to produce
    /// [`Error::TargetNotAvailableOnPlatform`] instead of
    /// [`Error::UnknownDependency`] when a required target is excluded.
    #[serde(skip)]
    excluded_targets: IndexMap<String, Vec<String>>,
    /// The optional `[lifecycle]` table, enabling before/after lifecycle
    /// events (see [`crate::lifecycle`]) when also licensed for the `events`
    /// feature. Absent (the default) is a quiet no-op, matching `toolchain`.
    #[serde(default)]
    lifecycle: Option<LifecycleConfig>,
}

/// Configuration for the optional `[lifecycle]` table: where before/after
/// lifecycle events are sent when the run is licensed for the `events`
/// feature. See the [`crate`]-level docs and [`Rakefile::run_licensed`].
#[derive(Debug, Deserialize)]
pub(crate) struct LifecycleConfig {
    /// A `host:port` UDP socket address, e.g. `"127.0.0.1:9999"`, that events
    /// are sent to (fire-and-forget; nobody needing to listen is fine).
    address: String,
    /// `address` parsed once at validation time, so `run_impl` never
    /// re-parses (or re-fails on) a string it has already validated.
    #[serde(skip)]
    resolved_address: Option<SocketAddr>,
}

/// The outcome of running a target: the exit status of the last command that
/// ran, plus the total wall-clock time spent running the chain.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The [`ExitStatus`] of the last command to run, or `None` when no command
    /// ran at all (a target chain defined purely by `depends_on`). Callers
    /// should treat `None` as success.
    pub status: Option<ExitStatus>,
    /// Total wall-clock time spent running the target and its transitive
    /// dependencies.
    pub elapsed: Duration,
    /// Tool installs and version updates that occurred while running the target
    /// chain. Empty when no tools were installed or updated (all already
    /// present and current), or in dry-run mode (no real ensures executed).
    pub updates: Vec<UpdateRecord>,
}

impl Rakefile {
    /// Load and validate a `Rakefile.toml` from `path`.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if `path` cannot be read, or any error from
    /// [`Rakefile::from_toml_str`] if the contents are invalid.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_path_with_host(path, &Host::detect())
    }

    /// Like [`from_path`](Self::from_path) but with an explicit [`Host`],
    /// so callers and tests can pin which platform variant is selected at
    /// parse time.
    ///
    /// # Errors
    /// As [`from_path`](Self::from_path).
    pub fn from_path_with_host(path: impl AsRef<Path>, host: &Host) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_toml_str_with_host(&contents, host)
    }

    /// Parse and validate a `Rakefile.toml` from a string using the current
    /// host's platform to resolve any platform-specific target variants.
    ///
    /// # Errors
    /// Returns [`Error::Parse`] if `s` is not valid TOML, or any of the
    /// following if validation fails:
    /// - [`Error::EmptyTarget`] — a target with neither commands nor dependencies
    /// - [`Error::EmptyCmd`] — a `cmd` array with no program
    /// - [`Error::DuplicateCommand`] — two commands in one target share a `name`
    /// - [`Error::UnknownDependency`] / [`Error::CircularDependency`]
    /// - [`Error::ConflictingDependency`] — a `depends_on` entry appears both
    ///   with and without a `^` prefix
    /// - [`Error::InvalidPlatformVariant`] — an unknown key in a target table
    ///   (including the old `platform = [...]` field at the target level)
    /// - [`Error::InvalidPlatform`] / [`Error::InvalidArch`] /
    ///   [`Error::EmptyPlatformList`] — command-level platform/arch list errors
    /// - [`Error::UnknownCommandTool`] — a command's `tools` entry has no
    ///   matching tool definition
    /// - [`Error::ToolDeclaredAtBothLevels`] — a tool name appears in both the
    ///   target's and a command's `tools` list
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Rakefile;
    ///
    /// let toml = r#"
    /// [[target.build.command]]
    /// name = "compile"
    /// cmd  = ["cargo", "build"]
    /// "#;
    ///
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// assert!(rakefile.target("build").is_some());
    /// # Ok::<(), librake::Error>(())
    /// ```
    ///
    /// Platform variant syntax:
    ///
    /// ```
    /// use librake::{Error, Host, Rakefile};
    ///
    /// // An unknown key in a target table is rejected at parse time.
    /// let err = Rakefile::from_toml_str(r#"
    /// [target.sign]
    /// platform = ["macos"]
    /// [[target.sign.command]]
    /// name = "n"
    /// cmd  = ["true"]
    /// "#);
    /// assert!(matches!(err, Err(Error::InvalidPlatformVariant { .. })));
    ///
    /// // Command-level tools are parsed and validated at parse time.
    /// let toml = r#"
    /// [tool.os.pkg-config]
    /// check = ["pkg-config", "--version"]
    ///
    /// [[target.build.command]]
    /// name  = "compile"
    /// cmd   = ["cargo", "build"]
    /// tools = ["pkg-config"]
    /// "#;
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// assert_eq!(
    ///     rakefile.target("build")
    ///         .and_then(|t| t.commands.first())
    ///         .map(|c| c.tools.clone()),
    ///     Some(vec!["pkg-config".to_string()])
    /// );
    /// # Ok::<(), librake::Error>(())
    /// ```
    pub fn from_toml_str(s: &str) -> Result<Self> {
        Self::from_toml_str_with_host(s, &Host::detect())
    }

    /// Like [`from_toml_str`](Self::from_toml_str) but with an explicit
    /// [`Host`], so callers and tests can pin which platform variant is
    /// selected at parse time. Particularly useful for tests that need to
    /// simulate a specific host platform.
    ///
    /// # Errors
    /// As [`from_toml_str`](Self::from_toml_str).
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::{Host, Rakefile};
    ///
    /// let toml = r#"
    /// [target.sign.macos]
    /// [[target.sign.macos.command]]
    /// name = "notarize"
    /// cmd  = ["xcrun", "notarytool", "submit"]
    ///
    /// [[target.sign.command]]
    /// name = "noop"
    /// cmd  = ["true"]
    /// "#;
    ///
    /// let macos_host = Host { os: "macos", family: "unix", arch: "aarch64" };
    /// let rakefile = Rakefile::from_toml_str_with_host(toml, &macos_host)?;
    /// // On macOS the macos variant is selected.
    /// assert_eq!(
    ///     rakefile.target("sign").and_then(|t| t.commands.first()).map(|c| c.name.as_str()),
    ///     Some("notarize")
    /// );
    ///
    /// let linux_host = Host { os: "linux", family: "unix", arch: "x86_64" };
    /// let rakefile = Rakefile::from_toml_str_with_host(toml, &linux_host)?;
    /// // On Linux the base variant is selected.
    /// assert_eq!(
    ///     rakefile.target("sign").and_then(|t| t.commands.first()).map(|c| c.name.as_str()),
    ///     Some("noop")
    /// );
    /// # Ok::<(), librake::Error>(())
    /// ```
    ///
    /// A target that exists only as a platform-specific variant is excluded (not
    /// an error) when no variant matches the current host — it simply does not
    /// appear in [`targets()`](Self::targets):
    ///
    /// ```
    /// use librake::{Host, Rakefile};
    ///
    /// let toml = r#"
    /// [target.notarize.macos]
    /// [[target.notarize.macos.command]]
    /// name = "submit"
    /// cmd  = ["xcrun", "notarytool", "submit"]
    /// "#;
    ///
    /// let linux_host = Host { os: "linux", family: "unix", arch: "x86_64" };
    /// let rakefile = Rakefile::from_toml_str_with_host(toml, &linux_host)?;
    /// // The macOS-only target is excluded on Linux — not an error, just absent.
    /// assert!(rakefile.target("notarize").is_none());
    /// # Ok::<(), librake::Error>(())
    /// ```
    pub fn from_toml_str_with_host(s: &str, host: &Host) -> Result<Self> {
        let raw: toml::Value = toml::from_str(s)?;
        let (resolved, excluded) = resolve_platform_variants(raw, host)?;
        let mut rakefile: Rakefile = Deserialize::deserialize(resolved)?;
        rakefile.excluded_targets = excluded;
        rakefile.normalize_depends_on();
        rakefile.validate()?;
        Ok(rakefile)
    }

    /// Split each target's raw `depends_on` list into actual dependencies
    /// (stored back in `depends_on`) and skip targets (stored in `skip_deps`).
    /// A `^`-prefixed entry names a skip; a bare `^` with no name is dropped,
    /// matching the CLI's behavior for a bare `^` token.
    fn normalize_depends_on(&mut self) {
        for target in self.targets.values_mut() {
            let raw = std::mem::take(&mut target.depends_on);
            for dep in raw {
                match dep.strip_prefix('^') {
                    Some(name) if !name.is_empty() => target.skip_deps.push(name.to_string()),
                    Some(_) => {}
                    None => target.depends_on.push(dep),
                }
            }
        }
    }

    /// Every target must define at least one command or dependency, each
    /// command's `cmd` must be non-empty, the `toolchain` must be a single
    /// non-empty token, the `[lifecycle]` table's `address` (if present) must
    /// parse as a socket address, and the dependency graph must be valid (no
    /// unknown dependencies, no cycles).
    fn validate(&mut self) -> Result<()> {
        // When declared, the channel must be a single clean token, so it can be
        // passed safely to the rustup installer as a `--default-toolchain` arg.
        if let Some(toolchain) = &self.toolchain
            && (toolchain.is_empty() || toolchain.chars().any(char::is_whitespace))
        {
            return Err(Error::InvalidToolchain {
                value: toolchain.clone(),
            });
        }
        if let Some(lifecycle) = &mut self.lifecycle {
            let addr = lifecycle.address.parse::<SocketAddr>().map_err(|_| {
                Error::InvalidLifecycleAddress {
                    value: lifecycle.address.clone(),
                }
            })?;
            lifecycle.resolved_address = Some(addr);
        }
        for (name, target) in &self.targets {
            if target.commands.is_empty() && target.depends_on.is_empty() {
                return Err(Error::EmptyTarget {
                    target: name.clone(),
                });
            }
            let mut seen: HashSet<&str> = HashSet::new();
            for command in &target.commands {
                validate_command(name, command)?;
                if !seen.insert(command.name.as_str()) {
                    return Err(Error::DuplicateCommand {
                        target: name.clone(),
                        command: command.name.clone(),
                    });
                }
            }
        }
        graph::validate(&self.targets, &self.excluded_targets)?;
        tool::validate(&self.tools, &self.targets)
    }

    /// The targets, in declaration order.
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Rakefile;
    ///
    /// let toml = concat!(
    ///     "[[target.build.command]]\nname=\"c\"\ncmd=[\"cargo\",\"build\"]\n",
    ///     "[[target.test.command]]\nname=\"t\"\ncmd=[\"cargo\",\"test\"]",
    /// );
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// let names: Vec<&str> = rakefile.targets().keys().map(String::as_str).collect();
    /// assert_eq!(names, ["build", "test"]);
    /// # Ok::<(), librake::Error>(())
    /// ```
    #[must_use]
    pub fn targets(&self) -> &IndexMap<String, Target> {
        &self.targets
    }

    /// The declared tools, split into the cargo and os categories.
    #[must_use]
    pub fn tools(&self) -> &ToolTable {
        &self.tools
    }

    /// The Rust toolchain channel this Rakefile targets, if one is declared.
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Rakefile;
    ///
    /// let without = Rakefile::from_toml_str(
    ///     "[[target.b.command]]\nname=\"c\"\ncmd=[\"true\"]"
    /// )?;
    /// assert_eq!(without.toolchain(), None);
    ///
    /// let with_chain = Rakefile::from_toml_str(
    ///     "toolchain = \"stable\"\n[[target.b.command]]\nname=\"c\"\ncmd=[\"true\"]"
    /// )?;
    /// assert_eq!(with_chain.toolchain(), Some("stable"));
    /// # Ok::<(), librake::Error>(())
    /// ```
    #[must_use]
    pub fn toolchain(&self) -> Option<&str> {
        self.toolchain.as_deref()
    }

    /// Whether `cargo-rake` should check for and apply self-updates on startup.
    ///
    /// Defaults to `true` when the key is absent from `Rakefile.toml`.
    ///
    /// # Examples
    ///
    /// ```
    /// use librake::Rakefile;
    ///
    /// let without = Rakefile::from_toml_str(
    ///     "[[target.b.command]]\nname=\"c\"\ncmd=[\"true\"]"
    /// )?;
    /// assert!(without.update()); // defaults to true
    ///
    /// let opt_out = Rakefile::from_toml_str(
    ///     "update = false\n[[target.b.command]]\nname=\"c\"\ncmd=[\"true\"]"
    /// )?;
    /// assert!(!opt_out.update());
    /// # Ok::<(), librake::Error>(())
    /// ```
    #[must_use]
    pub fn update(&self) -> bool {
        self.update
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
    /// [`Error::TargetNotAvailableOnPlatform`] if a root or skip target exists
    /// only as platform-specific variants that don't match the current host,
    /// [`Error::MissingShellVariant`] if a selected command has no variant for
    /// the detected shell, or [`Error::Spawn`] if a command's program cannot be
    /// launched.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use librake::Rakefile;
    ///
    /// # fn main() -> librake::Result<()> {
    /// let toml = "[[target.build.command]]\nname=\"compile\"\ncmd=[\"cargo\",\"build\"]";
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// let report = rakefile.run(&["build"])?;
    /// println!("status: {:?}", report.status);
    /// # Ok(())
    /// # }
    /// ```
    pub fn run(&self, names: &[&str]) -> Result<RunReport> {
        self.run_impl(names, detect_shell_family(), Host::detect(), false, None)
    }

    /// Like [`run`](Self::run) but license-aware: when this Rakefile's
    /// `[lifecycle]` table is present *and* `license` grants the `events`
    /// feature, before/after lifecycle events are sent (fire-and-forget, as
    /// JSON over a loopback UDP socket) for the whole run, each target, each
    /// command, and each tool check/install/update. Pass `None` for an
    /// unlicensed run — identical to calling [`run`](Self::run). When
    /// `[lifecycle]` is configured but `license` does not grant `events`, a
    /// one-line warning is printed and the run proceeds exactly as
    /// unlicensed (lifecycle events are an observability side channel, never
    /// a reason to fail a build).
    ///
    /// # Errors
    /// As [`run`](Self::run).
    pub fn run_licensed(
        &self,
        names: &[&str],
        license: Option<&LicensePayload>,
    ) -> Result<RunReport> {
        self.run_impl(names, detect_shell_family(), Host::detect(), false, license)
    }

    /// Like [`run`](Self::run) but without executing any commands: prints the
    /// same status lines (`Running`, `Checking`, `Skipped`, …) but skips every
    /// spawn and every tool check/install. Useful for previewing what a run would
    /// do. Configuration errors (unknown targets, missing shell variants) are
    /// still reported. Returns `status: None` (treated as success by the
    /// binaries) when no command ran — which is always the case in dry-run.
    ///
    /// # Errors
    /// Returns [`Error::UnknownTarget`] for an unknown root or skip target, or
    /// [`Error::MissingShellVariant`] when a command has no variant for the
    /// detected shell.
    pub fn run_dry(&self, names: &[&str]) -> Result<RunReport> {
        self.run_impl(names, detect_shell_family(), Host::detect(), true, None)
    }

    /// Like [`run`](Self::run) but with an explicit [`ShellFamily`] rather than
    /// detecting it from the environment, so callers (and tests) can pin which
    /// shell variant is selected. The host platform is still detected from the
    /// environment; pin it too with [`run_with`](Self::run_with).
    ///
    /// # Errors
    /// As [`run`](Self::run).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use librake::{Rakefile, ShellFamily};
    ///
    /// # fn main() -> librake::Result<()> {
    /// let toml = "[[target.build.command]]\nname=\"compile\"\ncmd=[\"cargo\",\"build\"]";
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// let report = rakefile.run_with_family(&["build"], ShellFamily::Posix)?;
    /// println!("status: {:?}", report.status);
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_with_family(&self, names: &[&str], family: ShellFamily) -> Result<RunReport> {
        self.run_impl(names, family, Host::detect(), false, None)
    }

    /// Like [`run_dry`](Self::run_dry) but with an explicit [`ShellFamily`]
    /// pinned rather than detected.
    ///
    /// # Errors
    /// As [`run_dry`](Self::run_dry).
    pub fn run_dry_with_family(&self, names: &[&str], family: ShellFamily) -> Result<RunReport> {
        self.run_impl(names, family, Host::detect(), true, None)
    }

    /// Like [`run`](Self::run) but with both the [`ShellFamily`] and the [`Host`]
    /// platform pinned rather than detected, so callers (and tests) can select
    /// which shell variant runs and which commands their `platform`/`arch` lists
    /// gate in or out.
    ///
    /// # Errors
    /// As [`run`](Self::run).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use librake::{Rakefile, ShellFamily, Host};
    ///
    /// # fn main() -> librake::Result<()> {
    /// let toml = "[[target.build.command]]\nname=\"compile\"\ncmd=[\"cargo\",\"build\"]";
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// let report = rakefile.run_with(&["build"], ShellFamily::Posix, Host::detect())?;
    /// println!("status: {:?}", report.status);
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_with(&self, names: &[&str], family: ShellFamily, host: Host) -> Result<RunReport> {
        self.run_impl(names, family, host, false, None)
    }

    /// Like [`run_dry`](Self::run_dry) but with both [`ShellFamily`] and [`Host`]
    /// pinned — the test seam for dry-run unit tests.
    ///
    /// # Errors
    /// Returns [`Error::UnknownTarget`] for an unknown root or skip target,
    /// [`Error::TargetNotAvailableOnPlatform`] when a root or skip target exists
    /// only as platform-specific variants that don't match the pinned host, or
    /// [`Error::MissingShellVariant`] when a command has no variant for the
    /// pinned shell.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use librake::{Rakefile, ShellFamily, Host};
    ///
    /// # fn main() -> librake::Result<()> {
    /// let toml = "[[target.build.command]]\nname=\"compile\"\ncmd=[\"cargo\",\"build\"]";
    /// let rakefile = Rakefile::from_toml_str(toml)?;
    /// let report = rakefile.run_dry_with(&["build"], ShellFamily::Posix, Host::detect())?;
    /// assert!(report.status.is_none());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Requesting a platform-only target on a non-matching host returns
    /// [`Error::TargetNotAvailableOnPlatform`]:
    ///
    /// ```
    /// use librake::{Error, Host, Rakefile, ShellFamily};
    ///
    /// let toml = r#"
    /// [target.notarize.macos]
    /// [[target.notarize.macos.command]]
    /// name = "submit"
    /// cmd  = ["xcrun", "notarytool", "submit"]
    /// "#;
    ///
    /// let linux_host = Host { os: "linux", family: "unix", arch: "x86_64" };
    /// let rakefile = Rakefile::from_toml_str_with_host(toml, &linux_host)?;
    /// let err = rakefile.run_dry_with(&["notarize"], ShellFamily::Posix, linux_host);
    /// assert!(matches!(err, Err(Error::TargetNotAvailableOnPlatform { .. })));
    /// # Ok::<(), librake::Error>(())
    /// ```
    pub fn run_dry_with(
        &self,
        names: &[&str],
        family: ShellFamily,
        host: Host,
    ) -> Result<RunReport> {
        self.run_impl(names, family, host, true, None)
    }

    /// Compute the tag-column width for running `names` without executing.
    ///
    /// Builds the same execution plan that [`run`](Self::run) would use and
    /// returns the `name_width` value: the maximum of all command names, skipped
    /// target names, and tool-tag strings in that plan. Callers that need to
    /// print aligned output *before* the run (e.g. a self-update check) can use
    /// this to match the column width that the run itself will use.
    ///
    /// # Errors
    /// Propagates any planning error (`UnknownTarget`, `SkipNotAllowed`, etc.).
    pub fn plan_name_width(&self, names: &[&str]) -> Result<usize> {
        let (mut roots, skips) = split_skip_targets(names);
        if roots.is_empty() {
            roots.push(crate::DEFAULT_TARGET);
        }
        let plan = graph::execution_order_with_skips(
            &self.targets,
            &self.excluded_targets,
            &roots,
            &skips,
        )?;
        Ok(self.name_width_for(&plan))
    }

    fn name_width_for(&self, plan: &graph::ExecutionPlan<'_>) -> usize {
        let cmd_name_width = plan
            .steps
            .iter()
            .filter_map(|step| {
                if let Step::Run(name) = step {
                    self.targets.get(*name)
                } else {
                    None
                }
            })
            .flat_map(|t| t.commands.iter())
            .map(|c| c.name.len())
            .max()
            .unwrap_or(0);
        let skip_name_width = plan
            .steps
            .iter()
            .filter_map(|step| {
                if let Step::Skip(name) = step {
                    Some(name.len())
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);
        let tool_tag_width = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Run(name) => self.targets.get(*name),
                Step::Skip(_) => None,
            })
            .flat_map(|t| {
                t.tools.iter().map(String::as_str).chain(
                    t.commands
                        .iter()
                        .flat_map(|c| c.tools.iter().map(String::as_str)),
                )
            })
            .filter_map(|tool_name| self.tools.tag_for(tool_name))
            .map(str::len)
            .max()
            .unwrap_or(0);
        cmd_name_width.max(skip_name_width).max(tool_tag_width)
    }

    fn run_impl(
        &self,
        names: &[&str],
        family: ShellFamily,
        host: Host,
        dry_run: bool,
        license: Option<&LicensePayload>,
    ) -> Result<RunReport> {
        // A `^name` token marks a target to skip rather than a root to run; the
        // rest are roots. With only skips named, fall back to the default target.
        let (mut roots, skips) = split_skip_targets(names);
        if roots.is_empty() {
            roots.push(crate::DEFAULT_TARGET);
        }
        // Resolve the order before the timer: a pre-execution error (an unknown
        // target, or a skip nothing else can do without) aborts without printing
        // a total `Runtime` line.
        let plan = graph::execution_order_with_skips(
            &self.targets,
            &self.excluded_targets,
            &roots,
            &skips,
        )?;
        let name_width = self.name_width_for(&plan);
        // Lifecycle events are a fire-and-forget observability side channel:
        // dry-run never builds a live emitter (its data would be synthetic),
        // and a `[lifecycle]` table with no license for `events` warns once
        // and behaves exactly like an absent one — never a hard error.
        let emitter = match (&self.lifecycle, license) {
            (Some(cfg), Some(payload)) if !dry_run && payload.features.events => cfg
                .resolved_address
                .map_or_else(Emitter::disabled, Emitter::new),
            (Some(_), _) if !dry_run => {
                print_label(
                    "Warning",
                    "lifecycle events configured but not licensed — run `rake license <key>` \
                     to activate; continuing without event emission",
                );
                Emitter::disabled()
            }
            _ => Emitter::disabled(),
        };
        emitter.emit(&LifecycleEvent::BeforeAll {
            run_id: emitter.run_id().to_string(),
            ts: Utc::now(),
            roots: roots.iter().map(|s| (*s).to_string()).collect(),
            target_count: plan.steps.len(),
            project: ProjectInfo::detect(),
        });
        let start = Instant::now();
        let mut updates: Vec<UpdateRecord> = Vec::new();
        let result = self.run_steps(
            &plan.steps,
            family,
            &host,
            dry_run,
            name_width,
            &mut updates,
            &emitter,
        );
        let elapsed = start.elapsed();
        // Print the total runtime on every path that started executing. In dry-run
        // mode there is no real work to time, so the line is skipped.
        if !dry_run {
            print_total_runtime(elapsed);
        }
        emitter.emit(&LifecycleEvent::AfterAll {
            run_id: emitter.run_id().to_string(),
            ts: Utc::now(),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            exit_code: result
                .as_ref()
                .ok()
                .and_then(|s| s.and_then(|st| st.code())),
            success: !matches!(&result, Ok(Some(st)) if !st.success()),
            tool_updates: updates.len(),
        });
        result.map(|status| RunReport {
            status,
            elapsed,
            updates,
        })
    }

    #[cfg_attr(nightly, allow(clippy::too_many_arguments))]
    fn run_steps<'rf>(
        &'rf self,
        steps: &[Step<'_>],
        family: ShellFamily,
        host: &Host,
        dry_run: bool,
        name_width: usize,
        updates: &mut Vec<UpdateRecord>,
        emitter: &Emitter,
    ) -> Result<Option<ExitStatus>> {
        let mut status = None;
        let mut ensured: HashSet<&'rf str> = HashSet::new();
        for step in steps {
            match step {
                Step::Skip(name) => {
                    print_target_skipped(name, name_width);
                    if self.targets.get(*name).is_none_or(|t| t.events) {
                        emitter.emit(&LifecycleEvent::TargetSkipped {
                            run_id: emitter.run_id().to_string(),
                            ts: Utc::now(),
                            target: (*name).to_string(),
                            reason: "skip requested".to_string(),
                        });
                    }
                }
                Step::Run(name) => {
                    let Some(target) = self.targets.get(*name) else {
                        continue;
                    };
                    // A target with `events = false` suppresses its own
                    // before/after events plus every nested command/tool
                    // event for it, by routing through a disabled emitter —
                    // `run_one`/`ensure_tool_with_events` never branch on
                    // whether the emitter they're handed is real.
                    let disabled_emitter;
                    let target_emitter: &Emitter = if target.events {
                        emitter
                    } else {
                        disabled_emitter = Emitter::disabled();
                        &disabled_emitter
                    };
                    // Ensure each target-level tool before any command runs.
                    for tool in &target.tools {
                        if ensured.insert(tool.as_str()) {
                            ensure_tool_with_events(
                                &self.tools,
                                tool,
                                dry_run,
                                name_width,
                                name,
                                None,
                                updates,
                                target_emitter,
                            )?;
                        }
                    }
                    target_emitter.emit(&LifecycleEvent::BeforeTarget {
                        run_id: target_emitter.run_id().to_string(),
                        ts: Utc::now(),
                        target: (*name).to_string(),
                    });
                    if !target.time_tracking {
                        target_emitter.emit(&LifecycleEvent::NoTimeTracking {
                            run_id: target_emitter.run_id().to_string(),
                            ts: Utc::now(),
                            target: (*name).to_string(),
                        });
                    }
                    let target_start = Instant::now();
                    let (current, stop) = run_one(
                        name,
                        target,
                        family,
                        host,
                        dry_run,
                        name_width,
                        &self.tools,
                        &mut ensured,
                        updates,
                        target_emitter,
                    )?;
                    target_emitter.emit(&LifecycleEvent::AfterTarget {
                        run_id: target_emitter.run_id().to_string(),
                        ts: Utc::now(),
                        target: (*name).to_string(),
                        exit_code: current.and_then(|st| st.code()),
                        success: !matches!(current, Some(st) if !st.success()),
                        chain_stopped: stop,
                        elapsed_ms: u64::try_from(target_start.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    });
                    if current.is_some() {
                        status = current;
                    }
                    if stop {
                        break;
                    }
                }
            }
        }
        Ok(status)
    }
}

/// Partition the requested target names into `(roots, skips)`: a token with a
/// leading `^` names a target to skip (the `^` stripped), every other token a
/// root to run. A bare `^` carries no name and is dropped.
fn split_skip_targets<'a>(names: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut roots = Vec::new();
    let mut skips = Vec::new();
    for name in names {
        match name.strip_prefix('^') {
            Some(skip) if !skip.is_empty() => skips.push(skip),
            Some(_) => {}
            None => roots.push(*name),
        }
    }
    (roots, skips)
}

/// Validate a single command: it must carry exactly one *kind* of body — a
/// non-empty `cmd` array, or one or more non-blank shell variants
/// (`sh`/`fish`/`ps`) — and must not mix `cmd` with a shell variant.
fn validate_command(target: &str, command: &Command) -> Result<()> {
    // Platform/arch gating is orthogonal to the body kind, so validate it first —
    // the body match below has early returns for a valid `cmd`.
    validate_platform_gates(target, command)?;
    let shells = [
        ("sh", &command.sh),
        ("fish", &command.fish),
        ("ps", &command.ps),
    ];
    let has_shell = shells.iter().any(|(_, body)| body.is_some());
    match &command.cmd {
        Some(_) if has_shell => {
            return Err(Error::AmbiguousCommandBody {
                target: target.to_string(),
                command: command.name.clone(),
            });
        }
        Some(cmd) if cmd.is_empty() => {
            return Err(Error::EmptyCmd {
                target: target.to_string(),
                command: command.name.clone(),
            });
        }
        Some(_) => return Ok(()),
        None if !has_shell => {
            return Err(Error::MissingCommandBody {
                target: target.to_string(),
                command: command.name.clone(),
            });
        }
        None => {}
    }
    // No `cmd`, at least one shell variant: each declared variant must be
    // non-blank.
    for (variant, body) in shells {
        if let Some(line) = body
            && line.trim().is_empty()
        {
            return Err(Error::EmptyShell {
                target: target.to_string(),
                command: command.name.clone(),
                variant,
            });
        }
    }
    Ok(())
}

/// Validate a command's `platform`/`arch` gating lists: a declared list must be
/// non-empty, and every token must be recognized. A `platform` token may name an
/// OS ([`KNOWN_OS`]) or a family ([`KNOWN_FAMILY`]); an `arch` token must be a
/// known architecture ([`KNOWN_ARCH`]). Unset lists are always valid.
fn validate_platform_gates(target: &str, command: &Command) -> Result<()> {
    if let Some(platforms) = &command.platform {
        if platforms.is_empty() {
            return Err(Error::EmptyPlatformList {
                target: target.to_string(),
                command: command.name.clone(),
                key: "platform",
            });
        }
        for token in platforms {
            if !KNOWN_OS.contains(&token.as_str()) && !KNOWN_FAMILY.contains(&token.as_str()) {
                return Err(Error::InvalidPlatform {
                    target: target.to_string(),
                    command: command.name.clone(),
                    token: token.clone(),
                });
            }
        }
    }
    if let Some(arches) = &command.arch {
        if arches.is_empty() {
            return Err(Error::EmptyPlatformList {
                target: target.to_string(),
                command: command.name.clone(),
                key: "arch",
            });
        }
        for token in arches {
            if !KNOWN_ARCH.contains(&token.as_str()) {
                return Err(Error::InvalidArch {
                    target: target.to_string(),
                    command: command.name.clone(),
                    token: token.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Outcome of resolving a single `[target.X]` table during platform variant
/// processing. Defined at module level to satisfy `clippy::items_after_statements`.
enum Resolution {
    UseVariant(toml::Value),
    UseBase(toml::map::Map<String, toml::Value>),
    Exclude,
}

/// Walk the `[target]` table in a parsed TOML value, resolve platform-specific
/// sub-table variants for `host`, and return the modified value plus a map of
/// targets that were excluded on this host (only defined for other platforms).
///
/// For each `[target.X]` entry the function distinguishes three kinds of keys:
/// - **base keys** (`command`, `depends_on`, `tools`, `events`) — remain in the base target
/// - **variant keys** (any token from [`KNOWN_OS`] or [`KNOWN_FAMILY`]) — declare
///   a platform-specific override for that OS/family
/// - **anything else** — rejected with [`Error::InvalidPlatformVariant`]
///
/// Resolution selects the most specific matching variant (OS before family);
/// when no variant matches the base keys form the resolved target; when no base
/// keys exist either the target is added to `excluded` and removed from the map.
fn resolve_platform_variants(
    mut raw: toml::Value,
    host: &Host,
) -> Result<(toml::Value, IndexMap<String, Vec<String>>)> {
    let mut excluded: IndexMap<String, Vec<String>> = IndexMap::new();

    let Some(target_map) = raw.get_mut("target").and_then(|v| v.as_table_mut()) else {
        return Ok((raw, excluded));
    };

    let names: Vec<String> = target_map.keys().cloned().collect();

    for name in &names {
        let Some(value) = target_map.get(name) else {
            continue;
        };
        let Some(table) = value.as_table() else {
            continue;
        };

        // Validate and collect variant keys (KNOWN_OS ∪ KNOWN_FAMILY).
        let mut variant_keys: Vec<String> = Vec::new();
        for key in table.keys() {
            if KNOWN_TARGET_BASE_KEYS.contains(&key.as_str()) {
                // base field — fine
            } else if KNOWN_OS.contains(&key.as_str()) || KNOWN_FAMILY.contains(&key.as_str()) {
                variant_keys.push(key.clone());
            } else {
                return Err(Error::InvalidPlatformVariant {
                    target: name.clone(),
                    key: key.clone(),
                });
            }
        }

        if variant_keys.is_empty() {
            continue; // plain base target — nothing to resolve
        }

        // OS token beats family token (more specific wins).
        let matched = variant_keys
            .iter()
            .find(|k| k.as_str() == host.os)
            .or_else(|| variant_keys.iter().find(|k| k.as_str() == host.family));

        // Collect the resolution decision while the table borrow is live,
        // then apply it after releasing the borrow.
        let resolution = if let Some(variant_key) = matched {
            Resolution::UseVariant(table[variant_key.as_str()].clone())
        } else {
            let base: toml::map::Map<String, toml::Value> = table
                .iter()
                .filter(|(k, _)| KNOWN_TARGET_BASE_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if base.is_empty() {
                Resolution::Exclude
            } else {
                Resolution::UseBase(base)
            }
        };

        match resolution {
            Resolution::UseVariant(v) => {
                drop(target_map.insert(name.clone(), v));
            }
            Resolution::UseBase(base) => {
                drop(target_map.insert(name.clone(), toml::Value::Table(base)));
            }
            Resolution::Exclude => {
                drop(excluded.insert(name.clone(), variant_keys));
                drop(target_map.remove(name.as_str()));
            }
        }
    }

    Ok((raw, excluded))
}

/// Ensure `tool`, emitting `BeforeTool`/`AfterTool` lifecycle events around it
/// and recording any install/update in `updates`. Shared by the target-level
/// tool loop in `run_steps` and the command-level tool loop in `run_one`;
/// `command` is `None` for a target-level tool, `Some` for a command-level one.
#[cfg_attr(nightly, allow(clippy::too_many_arguments))]
fn ensure_tool_with_events(
    tools: &ToolTable,
    tool: &str,
    dry_run: bool,
    name_width: usize,
    target: &str,
    command: Option<&str>,
    updates: &mut Vec<UpdateRecord>,
    emitter: &Emitter,
) -> Result<()> {
    emitter.emit(&LifecycleEvent::BeforeTool {
        run_id: emitter.run_id().to_string(),
        ts: Utc::now(),
        target: target.to_string(),
        command: command.map(str::to_string),
        tool: tool.to_string(),
    });
    let start = Instant::now();
    let record = tools.ensure(tool, dry_run, name_width)?;
    let (outcome, from, to) = ToolOutcome::from_update(record.as_ref());
    emitter.emit(&LifecycleEvent::AfterTool {
        run_id: emitter.run_id().to_string(),
        ts: Utc::now(),
        target: target.to_string(),
        command: command.map(str::to_string),
        tool: tool.to_string(),
        outcome,
        from,
        to,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
    if let Some(record) = record {
        updates.push(record);
    }
    Ok(())
}

/// Run a target's commands in array order under shell `family`. Returns the last
/// status run (or `None` when the target is a dependency-only aggregator with no
/// commands) and whether execution should stop (a command failed without
/// `skip_on_error`).
///
/// Command-level tools are ensured immediately before the command that requires
/// them, after the platform/arch skip check — so a skipped command never triggers
/// its tool checks. The `ensured` set deduplicates ensures across the whole run.
// Ten parameters keeps related context together without introducing a struct;
// `run_one` is private so there is no public-API concern here.
#[cfg_attr(nightly, allow(clippy::too_many_arguments))]
fn run_one<'rf>(
    name: &str,
    target: &'rf Target,
    family: ShellFamily,
    host: &Host,
    dry_run: bool,
    name_width: usize,
    tools: &ToolTable,
    ensured: &mut HashSet<&'rf str>,
    updates: &mut Vec<UpdateRecord>,
    emitter: &Emitter,
) -> Result<(Option<ExitStatus>, bool)> {
    let mut last = None;
    for command in &target.commands {
        // A command whose platform/arch excludes the host is silently skipped
        // (a no-op, not a failure): the target's remaining commands and the
        // dependency chain continue. Reported with a `Skipped` status line.
        if let Some(reason) = command.skip_reason(host) {
            print_skipped(&command.name, &reason, name_width);
            emitter.emit(&LifecycleEvent::CommandSkipped {
                run_id: emitter.run_id().to_string(),
                ts: Utc::now(),
                target: name.to_string(),
                command: command.name.clone(),
                reason,
            });
            continue;
        }
        // Ensure each command-level tool before this command runs, at most once
        // per run. Skipped commands (above) never reach this point, so their
        // tools are not ensured when the platform/arch excludes them.
        for tool in &command.tools {
            if ensured.insert(tool.as_str()) {
                ensure_tool_with_events(
                    tools,
                    tool,
                    dry_run,
                    name_width,
                    name,
                    Some(command.name.as_str()),
                    updates,
                    emitter,
                )?;
            }
        }
        // Resolve the invocation before the timer: a command with no variant for
        // the detected shell never starts, so it gets no `Cmd Runtime` line.
        let (program, args) =
            command
                .invocation(family)
                .ok_or_else(|| Error::MissingShellVariant {
                    target: name.to_string(),
                    command: command.name.clone(),
                    shell: family.key(),
                })?;
        if dry_run {
            // Print the "Running" line (same output as a real run) but skip the
            // spawn entirely. No `Cmd Runtime` is printed — there is no real
            // time to report, and (per `run_impl`) the emitter is always
            // disabled in dry-run, so no before/after command event fires.
            let _ = writeln!(stderr()).ok();
            let cmd_name = if color_stderr() {
                format!("{GREEN}[ {:>name_width$} ]{RESET}", command.name)
            } else {
                format!("[ {:>name_width$} ]", command.name)
            };
            let invocation = std::iter::once(program.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            print_label("Running", &format!("{cmd_name} {invocation}"));
            continue;
        }
        emitter.emit(&LifecycleEvent::BeforeCommand {
            run_id: emitter.run_id().to_string(),
            ts: Utc::now(),
            target: name.to_string(),
            command: command.name.clone(),
        });
        let start = Instant::now();
        let result = spawn_resolved(name, command, &program, &args, name_width);
        let elapsed = start.elapsed();
        // Print the per-command runtime even when the spawn fails, so a command
        // that was attempted but could not launch still reports its time.
        print_runtime("Cmd Runtime", elapsed);
        // Emit `AfterCommand` before propagating a launch failure via `?`, so a
        // command that was attempted but never ran still produces an event.
        let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok(status) => emitter.emit(&LifecycleEvent::AfterCommand {
                run_id: emitter.run_id().to_string(),
                ts: Utc::now(),
                target: name.to_string(),
                command: command.name.clone(),
                exit_code: status.code(),
                success: status.success(),
                skip_on_error: command.skip_on_error,
                chain_stopped: !status.success() && !command.skip_on_error,
                duration_ms,
            }),
            Err(_) => emitter.emit(&LifecycleEvent::AfterCommand {
                run_id: emitter.run_id().to_string(),
                ts: Utc::now(),
                target: name.to_string(),
                command: command.name.clone(),
                exit_code: None,
                success: false,
                skip_on_error: command.skip_on_error,
                chain_stopped: true,
                duration_ms,
            }),
        }
        let status = result?;
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
    "Skipped",
    "Cmd Runtime",
    "Runtime",
    "Checking",
    "Installing",
    "Present",
    "Up to date",
    "Updating",
    "Warning",
    "Updated",
    "Installed",
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

/// Print a `Skipped` status line for a command excluded by its `platform`/`arch`
/// list, in the same shape as a `Running` line: a blank separator line, then the
/// label with the command's name (green on a TTY) and the unmet `reason` (e.g.
/// `platform: linux, macos`).
fn print_skipped(command: &str, reason: &str, name_width: usize) {
    let _ = writeln!(stderr()).ok();
    let name = if color_stderr() {
        format!("{GREEN}[ {command:>name_width$} ]{RESET}")
    } else {
        format!("[ {command:>name_width$} ]")
    };
    print_label("Skipped", &format!("{name} {reason}"));
}

/// Print a `Skipped` status line for a whole target pruned from the run by a
/// `^name` skip request, in the same shape as [`print_skipped`]: a blank
/// separator line, then the label with the target's name (green on a TTY) and a
/// `skip requested` reason.
fn print_target_skipped(target: &str, name_width: usize) {
    print_skipped(target, "skip requested", name_width);
}

/// Spawn a single named command from its already-resolved `program`/`args`,
/// inheriting the parent's stdio. A blank line and the command's status line
/// (`Running [ <name> ] <program args>`, the name green on a TTY) are printed
/// first. Resolution (and the [`Error::MissingShellVariant`] it can raise) is the
/// caller's responsibility, so this only fails with [`Error::Spawn`].
fn spawn_resolved(
    target: &str,
    command: &Command,
    program: &str,
    args: &[String],
    name_width: usize,
) -> Result<ExitStatus> {
    // A blank line separates each command block from the previous output.
    let _ = writeln!(stderr()).ok();
    let name = if color_stderr() {
        format!("{GREEN}[ {:>name_width$} ]{RESET}", command.name)
    } else {
        format!("[ {:>name_width$} ]", command.name)
    };
    // Show the resolved invocation (`sh -c <line>`, `pwsh -Command <line>`, or
    // the direct program + args) so the line reflects what actually runs.
    let invocation = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    print_label("Running", &format!("{name} {invocation}"));
    ProcessCommand::new(program)
        .args(args)
        .status()
        .map_err(|source| Error::Spawn {
            target: target.to_string(),
            command: command.name.clone(),
            program: program.to_string(),
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
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use librake::format_duration;
///
/// assert_eq!(format_duration(Duration::from_micros(100)),      " 100.00000 µs");
/// assert_eq!(format_duration(Duration::from_micros(1_000)),    "   1.00000 ms");
/// assert_eq!(format_duration(Duration::from_secs(1)),          "   1.00000 s");
/// assert_eq!(format_duration(Duration::from_micros(90_500_000)), "1 min   30.50000 s");
/// ```
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

    use super::{Host, Rakefile, format_duration};
    use crate::error::Error;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    // Platform-portable `cmd = [...]` fragments for embedding in TOML strings.
    #[cfg(windows)]
    const CMD_EXIT0: &str = r#"cmd = ["cmd", "/c", "exit", "0"]"#;
    #[cfg(not(windows))]
    const CMD_EXIT0: &str = r#"cmd = ["true"]"#;

    #[cfg(windows)]
    const CMD_EXIT1: &str = r#"cmd = ["cmd", "/c", "exit", "1"]"#;
    #[cfg(not(windows))]
    const CMD_EXIT1: &str = r#"cmd = ["false"]"#;

    // Portable TOML array values for tool `check`/`install` fields.
    #[cfg(windows)]
    const TOML_EXIT0: &str = r#"["cmd", "/c", "exit", "0"]"#;
    #[cfg(not(windows))]
    const TOML_EXIT0: &str = r#"["true"]"#;

    #[cfg(windows)]
    const TOML_EXIT1: &str = r#"["cmd", "/c", "exit", "1"]"#;
    #[cfg(not(windows))]
    const TOML_EXIT1: &str = r#"["false"]"#;

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
    fn update_defaults_to_true() -> TestResult {
        assert!(Rakefile::from_toml_str(SAMPLE)?.update());
        Ok(())
    }

    #[test]
    fn update_false_is_respected() -> TestResult {
        let src = format!("update = false\n{SAMPLE}");
        assert!(!Rakefile::from_toml_str(&src)?.update());
        Ok(())
    }

    #[test]
    fn update_true_explicit_round_trips() -> TestResult {
        let src = format!("update = true\n{SAMPLE}");
        assert!(Rakefile::from_toml_str(&src)?.update());
        Ok(())
    }

    #[test]
    fn target_events_defaults_to_true() -> TestResult {
        let build = Rakefile::from_toml_str(SAMPLE)?
            .target("build")
            .ok_or("expected a build target")?
            .events;
        assert!(build);
        Ok(())
    }

    #[test]
    fn target_events_false_is_respected() -> TestResult {
        let src = format!("[target.build]\nevents = false\n{SAMPLE}");
        let events = Rakefile::from_toml_str(&src)?
            .target("build")
            .ok_or("expected a build target")?
            .events;
        assert!(!events);
        Ok(())
    }

    #[test]
    fn target_events_true_explicit_round_trips() -> TestResult {
        let src = format!("[target.build]\nevents = true\n{SAMPLE}");
        let events = Rakefile::from_toml_str(&src)?
            .target("build")
            .ok_or("expected a build target")?
            .events;
        assert!(events);
        Ok(())
    }

    #[test]
    fn target_time_tracking_defaults_to_true() -> TestResult {
        let build = Rakefile::from_toml_str(SAMPLE)?
            .target("build")
            .ok_or("expected a build target")?
            .time_tracking;
        assert!(build);
        Ok(())
    }

    #[test]
    fn target_time_tracking_false_is_respected() -> TestResult {
        let src = format!("[target.build]\ntime_tracking = false\n{SAMPLE}");
        let time_tracking = Rakefile::from_toml_str(&src)?
            .target("build")
            .ok_or("expected a build target")?
            .time_tracking;
        assert!(!time_tracking);
        Ok(())
    }

    #[test]
    fn target_time_tracking_true_explicit_round_trips() -> TestResult {
        let src = format!("[target.build]\ntime_tracking = true\n{SAMPLE}");
        let time_tracking = Rakefile::from_toml_str(&src)?
            .target("build")
            .ok_or("expected a build target")?
            .time_tracking;
        assert!(time_tracking);
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
        let toml = format!(
            "[[target.demo.command]]\nname = \"boom\"\n{CMD_EXIT1}\n\
                    [[target.demo.command]]\nname = \"after\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        // The first command fails without `skip_on_error`, so execution stops
        // there and `after` never runs: the returned status is the failure.
        let status = rakefile.run(&["demo"])?.status.ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_continues_remaining_commands() -> TestResult {
        let toml = format!(
            "[[target.demo.command]]\nname = \"boom\"\n{CMD_EXIT1}\nskip_on_error = true\n\
                    [[target.demo.command]]\nname = \"after\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        // `boom` fails but opts into skipping, so `after` still runs and its
        // success is the status returned.
        let status = rakefile.run(&["demo"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_continues_chain() -> TestResult {
        let toml = format!(
            "[[target.flaky.command]]\nname = \"boom\"\n{CMD_EXIT1}\nskip_on_error = true\n\
                    [target.all]\ndepends_on = [\"flaky\"]\n\
                    [[target.all.command]]\nname = \"ok\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        // `flaky` exits non-zero but opts into skipping, so `all` still runs and
        // its success is the status returned for the whole chain.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn failing_dependency_without_skip_aborts() -> TestResult {
        let toml = format!(
            "[[target.flaky.command]]\nname = \"boom\"\n{CMD_EXIT1}\n\
                    [target.all]\ndepends_on = [\"flaky\"]\n\
                    [[target.all.command]]\nname = \"ok\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        // `flaky` fails and does not skip, so the chain stops there: `all` never
        // runs and the returned status reflects the failure.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn depends_only_target_runs_dependencies() -> TestResult {
        let toml = format!(
            "[[target.build.command]]\nname = \"compile\"\n{CMD_EXIT0}\n\
                    [[target.test.command]]\nname = \"check\"\n{CMD_EXIT0}\n\
                    [target.all]\ndepends_on = [\"build\", \"test\"]\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        // `all` has no command of its own; its status is that of the last
        // dependency to run.
        let status = rakefile.run(&["all"])?.status.ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn multiple_root_targets_run_in_one_call() -> TestResult {
        // Two independent roots given together both run; the run succeeds.
        let toml = format!(
            "[[target.one.command]]\nname = \"a\"\n{CMD_EXIT0}\n\
                    [[target.two.command]]\nname = \"b\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
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
        let toml = format!(
            "[[target.flaky.command]]\nname = \"boom\"\n{CMD_EXIT1}\n\
                    [target.one]\ndepends_on = [\"flaky\"]\n\
                    [[target.one.command]]\nname = \"a\"\n{CMD_EXIT0}\n\
                    [target.two]\ndepends_on = [\"flaky\"]\n\
                    [[target.two.command]]\nname = \"b\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
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
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT0}\ninstall = {TOML_EXIT1}\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn run_with_failing_tool_install_aborts() -> TestResult {
        // The tool is absent (`check` fails) and its `install` fails, so the
        // run errors before the target's command runs.
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT1}\ninstall = {TOML_EXIT1}\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        match rakefile.run(&["build"]) {
            Err(Error::ToolInstallFailed { tool, .. }) => {
                assert_eq!(tool, "t");
                Ok(())
            }
            other => Err(format!("expected ToolInstallFailed, got {other:?}").into()),
        }
    }

    #[test]
    fn run_with_absent_tool_install_populates_updates() -> TestResult {
        // Tool is absent (check=false) and installs (install=true); the
        // resulting UpdateRecord must appear in RunReport.updates.
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT1}\ninstall = {TOML_EXIT0}\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let report = rakefile.run(&["build"])?;
        assert_eq!(report.updates.len(), 1, "expected one update record");
        assert_eq!(report.updates[0].name, "t");
        assert!(report.updates[0].from.is_none());
        assert!(report.updates[0].to.is_none());
        Ok(())
    }

    #[test]
    fn command_tool_absent_install_populates_updates() -> TestResult {
        // Command-level tool is absent and installs; its UpdateRecord must appear
        // in RunReport.updates (covers the updates.push path in run_one).
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT1}\ninstall = {TOML_EXIT0}\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\ntools = [\"t\"]\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let report = rakefile.run(&["build"])?;
        assert_eq!(report.updates.len(), 1, "expected one update record");
        assert_eq!(report.updates[0].name, "t");
        assert!(report.updates[0].from.is_none());
        assert!(report.updates[0].to.is_none());
        Ok(())
    }

    #[test]
    fn shared_tool_is_ensured_once_per_run() -> TestResult {
        // Both `build` and its dependency `dep` reference the same present tool;
        // ensuring is deduped, so the run completes (and `install`, `false`,
        // never runs even though two targets reference the tool).
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT0}\ninstall = {TOML_EXIT1}\n\
                    [target.dep]\ntools = [\"t\"]\n\
                    [[target.dep.command]]\nname = \"c\"\n{CMD_EXIT0}\n\
                    [target.build]\ntools = [\"t\"]\ndepends_on = [\"dep\"]\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn command_tool_is_ensured_when_command_runs() -> TestResult {
        // The command-level tool's `check` (`true`) reports it present, so
        // `install` (`false`, which would fail) never runs.
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT0}\ninstall = {TOML_EXIT1}\n\
                    [[target.build.command]]\nname = \"c\"\n{CMD_EXIT0}\ntools = [\"t\"]\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn command_tool_not_ensured_when_command_platform_skipped() -> TestResult {
        // The command is gated to windows; on the test host (not windows) it is
        // platform-skipped before tool ensuring, so `install = ["false"]` never
        // runs and the run succeeds (second command keeps the chain going).
        let toml = "[tool.cargo.t]\ncheck = [\"false\"]\ninstall = [\"false\"]\n\
                    [[target.build.command]]\nname = \"win\"\n\
                    platform = [\"windows\"]\ncmd = [\"true\"]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"always\"\ncmd = [\"true\"]\n";
        // Only execute this test on non-Windows hosts.
        if cfg!(windows) {
            return Ok(());
        }
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn command_tool_is_ensured_once_across_commands() -> TestResult {
        // Two commands in the same target both reference the same present tool;
        // the dedup set ensures it is checked at most once (`install = ["false"]`
        // must not fire).
        let toml = format!(
            "[tool.cargo.t]\ncheck = {TOML_EXIT0}\ninstall = {TOML_EXIT1}\n\
                    [[target.build.command]]\nname = \"a\"\n{CMD_EXIT0}\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"b\"\n{CMD_EXIT0}\ntools = [\"t\"]\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let status = rakefile
            .run(&["build"])?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn classify_shell_maps_families() {
        use super::{ShellFamily, classify_shell};
        assert_eq!(classify_shell("fish"), ShellFamily::Fish);
        assert_eq!(classify_shell("FISH"), ShellFamily::Fish);
        assert_eq!(classify_shell("pwsh"), ShellFamily::Ps);
        assert_eq!(classify_shell("powershell"), ShellFamily::Ps);
        for posix in ["sh", "bash", "zsh", "dash", "ksh", "tcsh"] {
            assert_eq!(classify_shell(posix), ShellFamily::Posix);
        }
    }

    #[test]
    fn shell_family_from_env_precedence() {
        use super::{ShellFamily, shell_family_from_env};
        // A PowerShell env signal wins over everything (PowerShell never sets $SHELL).
        assert_eq!(
            shell_family_from_env(true, false, Some("/usr/bin/fish"), false),
            ShellFamily::Ps
        );
        // PS wins even when FISH_VERSION is also set.
        assert_eq!(
            shell_family_from_env(true, true, Some("/bin/bash"), false),
            ShellFamily::Ps
        );
        // FISH_VERSION wins over $SHELL — the Omarchy scenario: login=bash, running=fish.
        assert_eq!(
            shell_family_from_env(false, true, Some("/usr/bin/bash"), false),
            ShellFamily::Fish
        );
        // FISH_VERSION wins even when $SHELL is unset.
        assert_eq!(
            shell_family_from_env(false, true, None, false),
            ShellFamily::Fish
        );
        // Without FISH_VERSION, $SHELL basename classifies the family.
        assert_eq!(
            shell_family_from_env(false, false, Some("/usr/bin/fish"), false),
            ShellFamily::Fish
        );
        assert_eq!(
            shell_family_from_env(false, false, Some("/bin/zsh"), false),
            ShellFamily::Posix
        );
        // Unset $SHELL falls back to the platform default.
        assert_eq!(
            shell_family_from_env(false, false, None, true),
            ShellFamily::Ps
        );
        assert_eq!(
            shell_family_from_env(false, false, None, false),
            ShellFamily::Posix
        );
        // An empty $SHELL is treated as unset.
        assert_eq!(
            shell_family_from_env(false, false, Some(""), false),
            ShellFamily::Posix
        );
    }

    #[test]
    fn command_with_no_body_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::MissingCommandBody { target, command }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                Ok(())
            }
            other => Err(format!("expected MissingCommandBody, got {other:?}").into()),
        }
    }

    #[test]
    fn command_mixing_cmd_and_shell_variant_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\nfish = \"true\"\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::AmbiguousCommandBody { target, command }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                Ok(())
            }
            other => Err(format!("expected AmbiguousCommandBody, got {other:?}").into()),
        }
    }

    #[test]
    fn blank_shell_variant_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\nfish = \"   \"\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyShell {
                target,
                command,
                variant,
            }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                assert_eq!(variant, "fish");
                Ok(())
            }
            other => Err(format!("expected EmptyShell, got {other:?}").into()),
        }
    }

    #[test]
    fn coexisting_shell_variants_are_accepted() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\n\
                    sh = \"true\"\nfish = \"true\"\nps = \"$true\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let build = rakefile.target("build").ok_or("expected 'build'")?;
        let command = build.commands.first().ok_or("expected a command")?;
        // All three variants render in `list` output.
        assert_eq!(command.display(), "sh: true | fish: true | ps: $true");
        Ok(())
    }

    #[test]
    fn missing_variant_for_detected_shell_errors() -> TestResult {
        use super::ShellFamily;
        // The command defines only a `fish` variant, but a POSIX shell is
        // selected, so there is no `sh` variant to run.
        let toml = "[[target.go.command]]\nname = \"only fish\"\nfish = \"echo hi\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.run_with_family(&["go"], ShellFamily::Posix) {
            Err(Error::MissingShellVariant {
                target,
                command,
                shell,
            }) => {
                assert_eq!(target, "go");
                assert_eq!(command, "only fish");
                assert_eq!(shell, "sh");
                Ok(())
            }
            other => Err(format!("expected MissingShellVariant, got {other:?}").into()),
        }
    }

    #[test]
    fn sh_variant_expands_and_runs() -> TestResult {
        use super::ShellFamily;
        if cfg!(windows) {
            return Ok(());
        }
        // `$(...)` only expands through a shell; `sh -c` runs this and `test`
        // exits 0 when the substitution worked.
        let toml =
            "[[target.go.command]]\nname = \"expand\"\nsh = \"test \\\"$(echo ok)\\\" = ok\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run_with_family(&["go"], ShellFamily::Posix)?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn sh_variant_failure_propagates() -> TestResult {
        use super::ShellFamily;
        if cfg!(windows) {
            return Ok(());
        }
        let toml = "[[target.go.command]]\nname = \"boom\"\nsh = \"exit 3\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile
            .run_with_family(&["go"], ShellFamily::Posix)?
            .status
            .ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn run_with_missing_os_tool_aborts() -> TestResult {
        // The os tool is absent (`check` = `false`) and declares no `install`, so
        // the run aborts with the requirement before the command runs.
        let toml = "[tool.os.docker]\ncheck = [\"false\"]\nhint = \"install Docker\"\n\
                    [target.build]\ntools = [\"docker\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.run(&["build"]) {
            Err(Error::RequiredToolMissing { tool, hint }) => {
                assert_eq!(tool, "docker");
                assert_eq!(hint.as_deref(), Some("install Docker"));
                Ok(())
            }
            other => Err(format!("expected RequiredToolMissing, got {other:?}").into()),
        }
    }

    /// A `Command` with the given `platform`/`arch` gating and a trivial `cmd`
    /// body, for exercising `skip_reason` directly.
    fn gated_command(platform: Option<Vec<&str>>, arch: Option<Vec<&str>>) -> super::Command {
        let to_vec = |v: Option<Vec<&str>>| v.map(|l| l.iter().map(|s| (*s).to_string()).collect());
        super::Command {
            name: "c".to_string(),
            cmd: Some(vec!["true".to_string()]),
            sh: None,
            fish: None,
            ps: None,
            skip_on_error: false,
            platform: to_vec(platform),
            arch: to_vec(arch),
            tools: vec![],
        }
    }

    #[test]
    fn skip_reason_gates_on_platform_and_arch() {
        use super::Host;
        let linux = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        // No gating runs everywhere.
        assert!(gated_command(None, None).skip_reason(&linux).is_none());
        // OS match / mismatch.
        assert!(
            gated_command(Some(vec!["linux", "macos"]), None)
                .skip_reason(&linux)
                .is_none()
        );
        assert_eq!(
            gated_command(Some(vec!["windows"]), None).skip_reason(&linux),
            Some("platform: windows".to_string())
        );
        // Family alias matches the host family even when no OS token does.
        assert!(
            gated_command(Some(vec!["unix"]), None)
                .skip_reason(&linux)
                .is_none()
        );
        // Arch match / mismatch.
        assert!(
            gated_command(None, Some(vec!["x86_64"]))
                .skip_reason(&linux)
                .is_none()
        );
        assert_eq!(
            gated_command(None, Some(vec!["aarch64"])).skip_reason(&linux),
            Some("arch: aarch64".to_string())
        );
        // AND across dimensions: a matching platform but mismatched arch skips.
        assert_eq!(
            gated_command(Some(vec!["linux"]), Some(vec!["aarch64"])).skip_reason(&linux),
            Some("arch: aarch64".to_string())
        );
    }

    #[test]
    fn dry_run_does_not_execute_failing_command() -> TestResult {
        use super::{Host, ShellFamily};
        // A command that would fail if spawned — in dry-run mode the spawn is
        // skipped entirely, so the report carries no status (treated as success).
        let toml = "[[target.go.command]]\nname = \"boom\"\ncmd = [\"false\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let report = rakefile.run_dry_with(&["go"], ShellFamily::Posix, host)?;
        assert!(report.status.is_none());
        Ok(())
    }

    #[test]
    fn dry_run_resolves_dependency_order() -> TestResult {
        use super::{Host, ShellFamily};
        // A chain with all-failing commands — dry-run never spawns them, so the
        // entire dependency graph is processed and the run still succeeds.
        let toml = "[[target.build.command]]\nname = \"b\"\ncmd = [\"false\"]\n\
                    [target.all]\ndepends_on = [\"build\"]\n\
                    [[target.all.command]]\nname = \"a\"\ncmd = [\"false\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let report = rakefile.run_dry_with(&["all"], ShellFamily::Posix, host)?;
        assert!(report.status.is_none());
        Ok(())
    }

    #[test]
    fn dry_run_missing_shell_variant_still_errors() -> TestResult {
        use super::{Host, ShellFamily};
        // Even in dry-run, a command with no variant for the detected shell is a
        // configuration error — the Rakefile is invalid regardless of execution.
        let toml = "[[target.go.command]]\nname = \"fish only\"\nfish = \"echo hi\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        match rakefile.run_dry_with(&["go"], ShellFamily::Posix, host) {
            Err(Error::MissingShellVariant {
                target,
                command,
                shell,
            }) => {
                assert_eq!(target, "go");
                assert_eq!(command, "fish only");
                assert_eq!(shell, "sh");
                Ok(())
            }
            other => Err(format!("expected MissingShellVariant, got {other:?}").into()),
        }
    }

    #[test]
    fn excluded_command_is_skipped_and_chain_continues() -> TestResult {
        use super::{Host, ShellFamily};
        // The first command is gated to windows (skipped on this linux host); the
        // second still runs, so the run reports its success.
        let toml = format!(
            "[[target.go.command]]\nname = \"win only\"\nplatform = [\"windows\"]\n{CMD_EXIT1}\n\
                    [[target.go.command]]\nname = \"always\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let status = rakefile
            .run_with(&["go"], ShellFamily::Posix, host)?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn skipped_target_command_does_not_run() -> TestResult {
        use super::{Host, ShellFamily};
        // `clean` would fail if it ran; skipping it via `^clean` lets `all`
        // (which still runs `build`) succeed.
        let toml = format!(
            "[[target.clean.command]]\nname = \"boom\"\n{CMD_EXIT1}\n\
                    [[target.build.command]]\nname = \"ok\"\n{CMD_EXIT0}\n\
                    [target.all]\ndepends_on = [\"clean\", \"build\"]\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let status = rakefile
            .run_with(&["all", "^clean"], ShellFamily::Posix, host)?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn blocked_skip_surfaces_error() -> TestResult {
        use super::{Host, ShellFamily};
        // `build` (not a root) depends on `clean`, so skipping clean is rejected.
        let toml = "[[target.clean.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.build]\ndepends_on = [\"clean\"]\n\
                    [[target.build.command]]\nname = \"b\"\ncmd = [\"true\"]\n\
                    [target.all]\ndepends_on = [\"build\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        match rakefile.run_with(&["all", "^clean"], ShellFamily::Posix, host) {
            Err(Error::SkipNotAllowed { target, dependents }) => {
                assert_eq!(target, "clean");
                assert_eq!(dependents, "build");
                Ok(())
            }
            other => Err(format!("expected SkipNotAllowed, got {other:?}").into()),
        }
    }

    #[test]
    fn matching_command_runs() -> TestResult {
        use super::{Host, ShellFamily};
        if cfg!(windows) {
            return Ok(());
        }
        // Gated to the host's platform, so it runs and its non-zero exit shows.
        let toml =
            "[[target.go.command]]\nname = \"boom\"\nplatform = [\"linux\"]\nsh = \"exit 3\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let status = rakefile
            .run_with(&["go"], ShellFamily::Posix, host)?
            .status
            .ok_or("expected a status")?;
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn invalid_platform_token_rejected() -> TestResult {
        let toml = "[[target.go.command]]\nname = \"c\"\nplatform = [\"linx\"]\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::InvalidPlatform { command, token, .. }) => {
                assert_eq!(command, "c");
                assert_eq!(token, "linx");
                Ok(())
            }
            other => Err(format!("expected InvalidPlatform, got {other:?}").into()),
        }
    }

    #[test]
    fn invalid_arch_token_rejected() -> TestResult {
        let toml = "[[target.go.command]]\nname = \"c\"\narch = [\"x86_65\"]\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::InvalidArch { command, token, .. }) => {
                assert_eq!(command, "c");
                assert_eq!(token, "x86_65");
                Ok(())
            }
            other => Err(format!("expected InvalidArch, got {other:?}").into()),
        }
    }

    #[test]
    fn empty_platform_list_rejected() -> TestResult {
        let toml = "[[target.go.command]]\nname = \"c\"\nplatform = []\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyPlatformList { command, key, .. }) => {
                assert_eq!(command, "c");
                assert_eq!(key, "platform");
                Ok(())
            }
            other => Err(format!("expected EmptyPlatformList, got {other:?}").into()),
        }
    }

    #[test]
    fn caret_in_depends_on_becomes_skip_dep() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [[target.clean.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.ci]\ndepends_on = [\"build\", \"^clean\"]\n\
                    [[target.ci.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let ci = rakefile.target("ci").ok_or("expected 'ci'")?;
        assert_eq!(ci.depends_on, vec!["build".to_string()]);
        assert_eq!(ci.skip_deps, vec!["clean".to_string()]);
        Ok(())
    }

    #[test]
    fn caret_skip_dep_prunes_from_run() -> TestResult {
        use super::{Host, ShellFamily};
        // `clean` would fail if it ran; `ci`'s depends_on `^clean` prunes it
        // automatically without a CLI `^clean` token.
        let toml = format!(
            "[[target.clean.command]]\nname = \"boom\"\n{CMD_EXIT1}\n\
             [[target.build.command]]\nname = \"ok\"\n{CMD_EXIT0}\n\
             [target.all]\ndepends_on = [\"clean\", \"build\"]\n\
             [[target.all.command]]\nname = \"c\"\n{CMD_EXIT0}\n\
             [target.ci]\ndepends_on = [\"all\", \"^clean\"]\n\
             [[target.ci.command]]\nname = \"c\"\n{CMD_EXIT0}\n"
        );
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let host = Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        };
        let status = rakefile
            .run_with(&["ci"], ShellFamily::Posix, host)?
            .status
            .ok_or("expected a status")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn unknown_caret_dep_in_depends_on_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.ci]\ndepends_on = [\"build\", \"^ghost\"]\n\
                    [[target.ci.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::UnknownDependency { target, dependency }) => {
                assert_eq!(target, "ci");
                assert_eq!(dependency, "ghost");
                Ok(())
            }
            other => Err(format!("expected UnknownDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn conflicting_dep_and_skip_dep_is_rejected() -> TestResult {
        let toml = "[[target.clean.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.ci]\ndepends_on = [\"clean\", \"^clean\"]\n\
                    [[target.ci.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::ConflictingDependency { target, name }) => {
                assert_eq!(target, "ci");
                assert_eq!(name, "clean");
                Ok(())
            }
            other => Err(format!("expected ConflictingDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn bare_caret_in_depends_on_is_dropped_silently() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.ci]\ndepends_on = [\"build\", \"^\"]\n\
                    [[target.ci.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let ci = rakefile.target("ci").ok_or("expected 'ci'")?;
        assert_eq!(ci.depends_on, vec!["build".to_string()]);
        assert!(ci.skip_deps.is_empty());
        Ok(())
    }

    // ── Target-level platform variant tests ─────────────────────────────────

    fn linux_host() -> Host {
        Host {
            os: "linux",
            family: "unix",
            arch: "x86_64",
        }
    }

    fn macos_host() -> Host {
        Host {
            os: "macos",
            family: "unix",
            arch: "aarch64",
        }
    }

    #[test]
    fn platform_variant_os_resolves_on_matching_host() -> TestResult {
        // [target.foo.linux] is selected on a linux host.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"echo\",\"default\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &linux_host())?;
        let cmds: Vec<&str> = rf
            .target("foo")
            .ok_or("expected 'foo'")?
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cmds, vec!["l"]);
        Ok(())
    }

    #[test]
    fn platform_variant_base_used_when_no_match() -> TestResult {
        // No macos variant exists; the base is used on macOS.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"echo\",\"default\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        let cmds: Vec<&str> = rf
            .target("foo")
            .ok_or("expected 'foo'")?
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cmds, vec!["d"]);
        Ok(())
    }

    #[test]
    fn platform_variant_family_token_matches_host() -> TestResult {
        // [target.foo.unix] matches a linux host (family = "unix").
        let toml = "[[target.foo.unix.command]]\nname = \"u\"\ncmd = [\"echo\",\"unix\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"echo\",\"default\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &linux_host())?;
        let cmds: Vec<&str> = rf
            .target("foo")
            .ok_or("expected 'foo'")?
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cmds, vec!["u"]);
        Ok(())
    }

    #[test]
    fn platform_variant_os_beats_family() -> TestResult {
        // Both linux and unix variants exist; linux is more specific and wins.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [[target.foo.unix.command]]\nname = \"u\"\ncmd = [\"echo\",\"unix\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"echo\",\"default\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &linux_host())?;
        let cmds: Vec<&str> = rf
            .target("foo")
            .ok_or("expected 'foo'")?
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cmds, vec!["l"]);
        Ok(())
    }

    #[test]
    fn platform_variant_only_excluded_absent_from_targets() -> TestResult {
        // foo has only a linux variant; on macOS it is absent from the resolved map.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [[target.bar.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        assert!(rf.target("foo").is_none(), "foo should be absent on macOS");
        assert!(rf.target("bar").is_some());
        Ok(())
    }

    #[test]
    fn platform_variant_excluded_dependency_is_silently_pruned() -> TestResult {
        // bar depends on foo, which only has a linux variant.  On macOS parsing
        // succeeds — excluded deps are accepted at parse time and simply absent
        // from the resolved target map; build_order prunes them during execution.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [target.bar]\ndepends_on = [\"foo\"]\n\
                    [[target.bar.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        assert!(rf.target("foo").is_none(), "foo should be absent on macOS");
        assert!(rf.target("bar").is_some(), "bar should still be present");
        Ok(())
    }

    #[test]
    fn platform_variant_running_excluded_target_directly_errors() -> TestResult {
        use super::ShellFamily;
        // Requesting an excluded target as a root gives a clear error.
        let toml = "[[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"echo\",\"linux\"]\n\
                    [[target.bar.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        match rf.run_with(&["foo"], ShellFamily::Posix, macos_host()) {
            Err(Error::TargetNotAvailableOnPlatform { name, .. }) => {
                assert_eq!(name, "foo");
                Ok(())
            }
            other => Err(format!("expected TargetNotAvailableOnPlatform, got {other:?}").into()),
        }
    }

    #[test]
    fn platform_variant_unknown_key_rejected() -> TestResult {
        // The old `platform = [...]` field is now invalid; it's caught as an unknown key.
        let toml = "[target.build]\nplatform = [\"linux\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::InvalidPlatformVariant { target, key }) => {
                assert_eq!(target, "build");
                assert_eq!(key, "platform");
                Ok(())
            }
            other => Err(format!("expected InvalidPlatformVariant, got {other:?}").into()),
        }
    }

    #[test]
    fn platform_variant_typo_key_rejected() -> TestResult {
        // A typo in the platform variant key (linuX) is rejected.
        let toml = "[[target.foo.linuX.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::InvalidPlatformVariant { target, key }) => {
                assert_eq!(target, "foo");
                assert_eq!(key, "linuX");
                Ok(())
            }
            other => Err(format!("expected InvalidPlatformVariant, got {other:?}").into()),
        }
    }

    #[test]
    fn platform_variant_events_key_allowed() -> TestResult {
        // `events` is a recognized target base key: combining it with a
        // platform-variant sub-table must not trip `InvalidPlatformVariant`,
        // and (like `depends_on`) it round-trips through the base-key path
        // when no variant matches the host.
        let toml = "[target.foo]\nevents = false\n\
                    [[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"true\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        let foo = rf.target("foo").ok_or("expected 'foo'")?;
        assert!(!foo.events);
        Ok(())
    }

    #[test]
    fn platform_variant_time_tracking_key_allowed() -> TestResult {
        // `time_tracking` is a recognized target base key: combining it with
        // a platform-variant sub-table must not trip `InvalidPlatformVariant`,
        // and (like `events`) it round-trips through the base-key path when
        // no variant matches the host.
        let toml = "[target.foo]\ntime_tracking = false\n\
                    [[target.foo.linux.command]]\nname = \"l\"\ncmd = [\"true\"]\n\
                    [[target.foo.command]]\nname = \"d\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        let foo = rf.target("foo").ok_or("expected 'foo'")?;
        assert!(!foo.time_tracking);
        Ok(())
    }

    #[test]
    fn platform_variant_base_without_variants_unchanged() -> TestResult {
        // A plain [target.foo] with no variant sub-tables is unaffected.
        let toml = "[[target.foo.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &linux_host())?;
        assert!(rf.target("foo").is_some());
        Ok(())
    }

    #[test]
    fn platform_variant_explicit_empty_header_excluded_on_non_matching() -> TestResult {
        // Reproduces the Rakefile.toml linux-only pattern: an explicit empty
        // [target.X.linux] header (no key-value pairs) followed by a
        // [[target.X.linux.command]] array. On a non-linux host the target must
        // be excluded (not appear as an EmptyTarget in self.targets).
        let toml = "[target.linux-only.linux]\n\n\
                    [[target.linux-only.linux.command]]\n\
                    name = \"uname\"\n\
                    cmd  = [\"uname\", \"-r\"]\n";
        let windows_host = Host {
            os: "windows",
            family: "windows",
            arch: "x86_64",
        };
        let rf = Rakefile::from_toml_str_with_host(toml, &windows_host)?;
        assert!(
            rf.target("linux-only").is_none(),
            "linux-only must be absent on windows"
        );
        Ok(())
    }

    #[test]
    fn platform_variant_depends_on_resolved_correctly() -> TestResult {
        // The linux variant has different depends_on than the base.
        let toml = "[target.foo.linux]\ndepends_on = [\"dep-linux\"]\n\
                    [[target.foo.linux.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [target.foo]\ndepends_on = [\"dep-base\"]\n\
                    [[target.foo.command]]\nname = \"c\"\ncmd = [\"true\"]\n\
                    [[target.dep-linux.command]]\nname = \"d\"\ncmd = [\"true\"]\n\
                    [[target.dep-base.command]]\nname = \"d\"\ncmd = [\"true\"]\n";
        let rf = Rakefile::from_toml_str_with_host(toml, &linux_host())?;
        let foo = rf.target("foo").ok_or("expected 'foo'")?;
        assert_eq!(foo.depends_on, vec!["dep-linux".to_string()]);
        let rf2 = Rakefile::from_toml_str_with_host(toml, &macos_host())?;
        let foo2 = rf2.target("foo").ok_or("expected 'foo'")?;
        assert_eq!(foo2.depends_on, vec!["dep-base".to_string()]);
        Ok(())
    }

    // --- plan_name_width tests ---

    #[test]
    fn plan_name_width_unknown_target_is_error() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.plan_name_width(&["ghost"]) {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "ghost");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn plan_name_width_returns_command_name_length() -> TestResult {
        let toml =
            "[[target.default.command]]\nname = \"my-build\"\n".to_string() + CMD_EXIT0 + "\n";
        let rakefile = Rakefile::from_toml_str(&toml)?;
        let width = rakefile.plan_name_width(&["default"])?;
        assert_eq!(width, "my-build".len());
        Ok(())
    }
}
