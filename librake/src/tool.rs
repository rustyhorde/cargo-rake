//! External tool dependencies for targets.
//!
//! A `Rakefile.toml` may declare a top-level `[tool]` table split into three
//! categories: **`[tool.cargo.<name>]`** for cargo-installable tools (cargo
//! subcommands and the like), **`[tool.os.<name>]`** for OS-level dependencies
//! (`docker`, `pkg-config`, …) that `rake` cannot `cargo install`, and
//! **`[tool.fish.<name>]`** for fish shell functions that must exist before a
//! target runs. All three are modelled by [`ToolTable`]; targets reference any
//! kind by name via their `tools` list (the three categories share one flat
//! reference namespace, so names must be unique across all of them).
//!
//! Before a target's commands run, each referenced tool is
//! [`ensure`](ToolTable::ensure)d. A **cargo** tool's `check` command probes for
//! local presence, and if it is missing the `install` command is run; when it
//! sets `update = true`, the installed version is compared against the latest
//! reported by its [`SemverCheck`] mode and re-installed if a newer one exists.
//! An **os** tool is checked the same way, but if it is absent and declares an
//! `install` that runs; otherwise the run aborts with the requirement (and any
//! `hint`). OS tools have no update support. Each ensure announces a `Checking`
//! line and prints an outcome (`Present`, `Up to date`, `Installing`, or
//! `Updating`), each a right-justified bold-cyan status-label prefix in the run's
//! shared column, matching the command status lines.

use std::io::{Write, stderr};
use std::process::Command as ProcessCommand;
use std::sync::OnceLock;

use indexmap::IndexMap;
use semver::Version;
use serde::Deserialize;
use ureq::tls::{RootCerts, TlsConfig};

use crate::{
    error::{Error, Result},
    rakefile::{GREEN, RESET, Target, color_stderr, print_label},
};

/// How a tool's `update = true` check resolves the latest available version.
///
/// Selected per tool via `semver_check`, defaulting to [`SemverCheck::CratesIo`].
/// This is the extension point for future version sources (a git tag, a custom
/// command, …); only `crates-io` is supported today.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemverCheck {
    /// Query the crates.io registry API for the latest published version.
    #[default]
    CratesIo,
}

/// The status-line bracket tag used by cargo and OS tools: `[ check ]`.
pub const CHECK_TAG: &str = "check";
/// The status-line bracket tag used by fish function tools: `[ fish ]`.
pub(crate) const FISH_TAG: &str = "fish";

/// A tool install or version update that occurred during a run.
///
/// Collected by the run machinery and returned in [`crate::RunReport::updates`];
/// the binaries print a consolidated summary after the final `Runtime` line.
#[derive(Debug, Clone)]
pub struct UpdateRecord {
    /// The tool's name (e.g. `"cargo-nextest"` or `"cargo-rake"` for the
    /// self-update).
    pub name: String,
    /// The version that was present before the update, or `None` when the tool
    /// was not previously installed.
    pub from: Option<String>,
    /// The version now installed, or `None` when the post-install version was
    /// not determined (fresh installs via `cargo install` do not re-probe).
    pub to: Option<String>,
}

/// The top-level `[tool]` table, split into the three tool categories.
///
/// `[tool.cargo.<name>]` entries deserialize into [`cargo`](ToolTable::cargo),
/// `[tool.os.<name>]` entries into [`os`](ToolTable::os), and
/// `[tool.fish.<name>]` entries into [`fish`](ToolTable::fish). All three
/// categories share one flat reference namespace (a target's `tools` names
/// resolve against all of them), so a name must not appear in more than one —
/// `validate` rejects that.
#[derive(Debug, Default, Deserialize)]
pub struct ToolTable {
    /// Cargo-installable tools, declared under `[tool.cargo.<name>]`.
    #[serde(default)]
    pub cargo: IndexMap<String, CargoTool>,
    /// OS-level tool dependencies, declared under `[tool.os.<name>]`.
    #[serde(default)]
    pub os: IndexMap<String, OsTool>,
    /// Fish shell function dependencies, declared under `[tool.fish.<name>]`.
    /// The key is the fish function name to probe; no `check`/`install` fields
    /// are needed because the name itself is the function to look up.
    #[serde(default)]
    pub fish: IndexMap<String, FishTool>,
}

/// A single fish shell function dependency declared in `[tool.fish.<name>]`.
///
/// The TOML key (`<name>`) is the fish function name to check. Unlike
/// [`CargoTool`] and [`OsTool`], there is no `check` or `install` command —
/// the presence check is always `fish -c "functions --query <name>"`, which
/// covers user-defined functions, autoloaded functions, and builtins. When the
/// function is absent the run aborts with the requirement and any `hint`.
#[derive(Debug, Default, Deserialize)]
pub struct FishTool {
    /// An optional message describing how to define the function, shown when it
    /// is absent (e.g. `"define my_fn in ~/.config/fish/functions/my_fn.fish"`).
    #[serde(default)]
    pub hint: Option<String>,
}

/// A single cargo-installable tool declared in `[tool.cargo.<name>]`.
#[derive(Debug, Deserialize)]
pub struct CargoTool {
    /// The crates.io crate name, used by the [`SemverCheck::CratesIo`] update
    /// check. Required only when `update = true` under that mode.
    #[serde(rename = "crate", default)]
    pub crate_name: Option<String>,
    /// The command that probes whether the tool is already installed: a zero
    /// exit means present. Its stdout is also parsed for the installed version
    /// when `update = true`.
    #[serde(default)]
    pub check: Vec<String>,
    /// The command that installs (or upgrades) the tool.
    #[serde(default)]
    pub install: Vec<String>,
    /// When `true`, compare the installed version against the latest reported by
    /// [`CargoTool::semver_check`] and re-install if a newer one exists. Defaults
    /// to `false` (use whatever is already installed).
    #[serde(default)]
    pub update: bool,
    /// How the `update` check resolves the latest version. Defaults to
    /// [`SemverCheck::CratesIo`].
    #[serde(default)]
    pub semver_check: SemverCheck,
}

/// A single OS-level tool dependency declared in `[tool.os.<name>]`.
///
/// Unlike a [`CargoTool`], an OS tool has no update support. Its `check` probes
/// for presence; when absent, a declared `install` is run, otherwise the run
/// aborts with the requirement and any `hint`.
#[derive(Debug, Deserialize)]
pub struct OsTool {
    /// The command that probes whether the tool is already installed: a zero
    /// exit means present. Required (validated non-empty).
    #[serde(default)]
    pub check: Vec<String>,
    /// An optional command to install the tool when it is absent. When empty, an
    /// absent tool aborts the run with the requirement instead.
    #[serde(default)]
    pub install: Vec<String>,
    /// An optional message describing how to install the tool, shown when it is
    /// absent and no `install` is declared.
    #[serde(default)]
    pub hint: Option<String>,
}

/// Validate the tool table and every target's `tools` references.
///
/// Each cargo tool's `check` and `install` must be non-empty, and one with
/// `update = true` under the `crates-io` semver check must declare a `crate`.
/// Each os tool's `check` must be non-empty. Fish tools require no additional
/// validation beyond name uniqueness. A tool name must not appear in more than
/// one category, and every `tools` entry on a target must name a tool in one of
/// the three categories.
///
/// # Errors
/// Returns [`Error::EmptyToolCommand`], [`Error::ToolUpdateMissingCrate`],
/// [`Error::DuplicateTool`], or [`Error::UnknownTool`] as appropriate.
pub(crate) fn validate(tools: &ToolTable, targets: &IndexMap<String, Target>) -> Result<()> {
    for (name, tool) in &tools.cargo {
        if tool.check.is_empty() {
            return Err(Error::EmptyToolCommand {
                tool: name.clone(),
                field: "check",
            });
        }
        if tool.install.is_empty() {
            return Err(Error::EmptyToolCommand {
                tool: name.clone(),
                field: "install",
            });
        }
        if tool.update {
            match tool.semver_check {
                SemverCheck::CratesIo => {
                    if tool.crate_name.is_none() {
                        return Err(Error::ToolUpdateMissingCrate { tool: name.clone() });
                    }
                }
            }
        }
    }

    for (name, tool) in &tools.os {
        if tool.check.is_empty() {
            return Err(Error::EmptyToolCommand {
                tool: name.clone(),
                field: "check",
            });
        }
        // All three categories share one flat reference namespace.
        if tools.cargo.contains_key(name) || tools.fish.contains_key(name) {
            return Err(Error::DuplicateTool { tool: name.clone() });
        }
    }

    for (name, _tool) in &tools.fish {
        // Fish tools have no check/install to validate — just name uniqueness.
        if tools.cargo.contains_key(name) || tools.os.contains_key(name) {
            return Err(Error::DuplicateTool { tool: name.clone() });
        }
    }

    for (name, target) in targets {
        for tool in &target.tools {
            if !tools.cargo.contains_key(tool)
                && !tools.os.contains_key(tool)
                && !tools.fish.contains_key(tool)
            {
                return Err(Error::UnknownTool {
                    target: name.clone(),
                    tool: tool.clone(),
                });
            }
        }
    }

    // Validate command-level tool references: each must resolve against the
    // ToolTable, and must not duplicate a tool already declared at the target level
    // (declare it at one level only).
    for (tname, target) in targets {
        let target_tool_set: std::collections::HashSet<&str> =
            target.tools.iter().map(String::as_str).collect();
        for command in &target.commands {
            for tool in &command.tools {
                if !tools.cargo.contains_key(tool)
                    && !tools.os.contains_key(tool)
                    && !tools.fish.contains_key(tool)
                {
                    return Err(Error::UnknownCommandTool {
                        target: tname.clone(),
                        command: command.name.clone(),
                        tool: tool.clone(),
                    });
                }
                if target_tool_set.contains(tool.as_str()) {
                    return Err(Error::ToolDeclaredAtBothLevels {
                        target: tname.clone(),
                        command: command.name.clone(),
                        tool: tool.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

impl ToolTable {
    /// The bracket tag string (`"check"` or `"fish"`) for the tool named `name`,
    /// or `None` when the name does not appear in any category.
    pub(crate) fn tag_for(&self, name: &str) -> Option<&'static str> {
        if self.fish.contains_key(name) {
            Some(FISH_TAG)
        } else if self.cargo.contains_key(name) || self.os.contains_key(name) {
            Some(CHECK_TAG)
        } else {
            None
        }
    }

    /// The maximum `[ <tag> ]` bracket width across all tools in this table.
    /// Returns `0` when the table is empty.
    #[must_use]
    pub fn max_tag_width(&self) -> usize {
        let has_check = !self.cargo.is_empty() || !self.os.is_empty();
        let has_fish = !self.fish.is_empty();
        match (has_check, has_fish) {
            (false, false) => 0,
            (true, false) => CHECK_TAG.len(),
            (false, true) => FISH_TAG.len(),
            (true, true) => CHECK_TAG.len().max(FISH_TAG.len()),
        }
    }

    /// Ensure the tool referenced by `name` is available.
    ///
    /// Dispatches by category: a name in [`cargo`](ToolTable::cargo) is handled
    /// by [`ensure_cargo`], one in [`os`](ToolTable::os) by [`ensure_os`]. A name
    /// in neither is a silent no-op (references are validated up front by
    /// [`validate`], so this cannot happen for a validated `Rakefile`).
    ///
    /// Returns `Some(`[`UpdateRecord`]`)` when a tool was newly installed or
    /// updated to a newer version; `None` when it was already present and
    /// current (or when the check was skipped in dry-run mode).
    ///
    /// # Errors
    /// Propagates the install/requirement errors of the dispatched ensure.
    pub(crate) fn ensure(
        &self,
        name: &str,
        dry_run: bool,
        name_width: usize,
    ) -> Result<Option<UpdateRecord>> {
        if dry_run {
            // In dry-run mode, announce the dependency but skip the check and
            // any install — no processes are spawned. Use the same tag as the
            // live path so the output is consistent.
            let Some(tag) = self.tag_for(name) else {
                return Ok(None);
            };
            eprint_tool("Checking", tag, name, &[], name_width);
            return Ok(None);
        }
        if let Some(tool) = self.cargo.get(name) {
            ensure_cargo(name, tool, name_width)
        } else if let Some(tool) = self.os.get(name) {
            ensure_os(name, tool, name_width)
        } else if let Some(tool) = self.fish.get(name) {
            ensure_fish(name, tool, name_width)
        } else {
            Ok(None)
        }
    }
}

/// Check whether a newer version of `cargo-rake` is published on crates.io and
/// install it via `cargo install cargo-rake` if one is found.
///
/// `current_version` is the running binary's semver string — pass
/// `env!("CARGO_PKG_VERSION")` from the call site. Version-check and network
/// failures are non-fatal (a `Warning` line is printed and the run continues),
/// consistent with how cargo tool update failures are handled.
///
/// Returns `Some(`[`UpdateRecord`]`)` when a new version was installed (the
/// caller should re-exec the updated binary), or `None` when already up to
/// date or when the version check could not be performed.
///
/// # Errors
/// Returns [`Error::SelfUpdatePrepare`] if the running binary cannot be renamed
/// on Windows before installation, or [`Error::ToolInstallSpawn`] /
/// [`Error::ToolInstallFailed`] if `cargo install cargo-rake` cannot be
/// launched or exits non-zero.
///
/// # Examples
///
/// ```no_run
/// // Pass the running binary's version string; `Some(_)` means a new version
/// // was installed and the caller should relaunch the updated binary.
/// if librake::ensure_self_update(env!("CARGO_PKG_VERSION"), 0)?.is_some() {
///     // relaunch the updated binary with the original arguments
/// }
/// # Ok::<(), librake::Error>(())
/// ```
pub fn ensure_self_update(
    current_version: &str,
    name_width: usize,
) -> Result<Option<UpdateRecord>> {
    const NAME: &str = "cargo-rake";
    eprint_tool("Checking", CHECK_TAG, NAME, &[], name_width);

    let Some(installed) = parse_version_token(current_version) else {
        eprint_tool(
            "Warning",
            CHECK_TAG,
            NAME,
            &["could not determine installed version; keeping current".to_string()],
            name_width,
        );
        return Ok(None);
    };

    let latest = match latest_crate_version(NAME) {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(None),
        Err(message) => {
            eprint_tool(
                "Warning",
                CHECK_TAG,
                NAME,
                &[format!("version check failed: {message}")],
                name_width,
            );
            return Ok(None);
        }
    };

    if latest <= installed {
        eprint_tool(
            "Up to date",
            CHECK_TAG,
            NAME,
            &[installed.to_string()],
            name_width,
        );
        return Ok(None);
    }

    eprint_tool(
        "Updating",
        CHECK_TAG,
        NAME,
        &[format!("{installed} -> {latest}")],
        name_width,
    );

    // Windows: the OS locks running executables, so `cargo install` cannot
    // overwrite the current binary in place. Rename it away first; the
    // running process continues from the old (now renamed) file, and after
    // install the original path holds the new binary.
    #[cfg(windows)]
    rename_for_self_update()?;

    let install = vec!["cargo".to_string(), "install".to_string(), NAME.to_string()];
    run_install(NAME, &install)?;
    Ok(Some(UpdateRecord {
        name: NAME.to_string(),
        from: Some(installed.to_string()),
        to: Some(latest.to_string()),
    }))
}

/// Rename the running executable to `<exe>.bak` so `cargo install` can place
/// the updated binary at the original path without a sharing-violation error.
#[cfg(windows)]
fn rename_for_self_update() -> Result<()> {
    use std::path::PathBuf;
    let exe = std::env::current_exe().map_err(Error::SelfUpdatePrepare)?;
    let bak: PathBuf = {
        let mut s = exe.clone().into_os_string();
        s.push(".bak");
        s.into()
    };
    std::fs::rename(&exe, &bak).map_err(Error::SelfUpdatePrepare)?;
    Ok(())
}

/// Ensure a single cargo tool is available, installing or updating it as needed.
///
/// Announces the check (a `Checking` line), then runs the tool's `check`
/// command (capturing its output) to detect presence, and prints an outcome
/// line. When absent, prints an `Installing` notice and runs `install`. When
/// present and `update = false`, prints a `Present` line (with the installed
/// version when the `check` output carried one). When present and
/// `update = true`, resolves the latest version via [`latest_version`] and
/// either re-installs (`Updating`) if it is newer than the installed version
/// parsed from the `check` output or prints `Up to date`; a version-check
/// failure here is non-fatal (a warning is printed and the installed version is
/// kept).
///
/// # Errors
/// Returns [`Error::EmptyToolCommand`] if `check`/`install` are empty (normally
/// caught by [`validate`]), or [`Error::ToolInstallSpawn`] /
/// [`Error::ToolInstallFailed`] if the install command cannot be launched or
/// exits non-zero.
fn ensure_cargo(name: &str, tool: &CargoTool, name_width: usize) -> Result<Option<UpdateRecord>> {
    let Some((program, args)) = tool.check.split_first() else {
        return Err(Error::EmptyToolCommand {
            tool: name.to_string(),
            field: "check",
        });
    };

    eprint_tool("Checking", CHECK_TAG, name, &[], name_width);
    let output = ProcessCommand::new(program).args(args).output();
    let present = output.as_ref().is_ok_and(|o| o.status.success());

    if !present {
        eprint_tool("Installing", CHECK_TAG, name, &tool.install, name_width);
        run_install(name, &tool.install)?;
        return Ok(Some(UpdateRecord {
            name: name.to_string(),
            from: None,
            to: None,
        }));
    }

    let installed = output
        .ok()
        .and_then(|o| parse_installed_version(&o.stdout, &o.stderr));

    if tool.update {
        update_if_newer(name, tool, installed.as_ref(), name_width)
    } else {
        // Present and not an `update` tool: report what is already installed,
        // including the parsed version when the `check` output carried one.
        let detail = installed.map_or_else(Vec::new, |v| vec![v.to_string()]);
        eprint_tool("Present", CHECK_TAG, name, &detail, name_width);
        Ok(None)
    }
}

/// Ensure a single OS tool is available.
///
/// Announces the check (a `Checking` line) and runs `check` to detect presence.
/// When present, prints a `Present` line. When absent: if the tool declares an
/// `install`, prints an `Installing` notice and runs it (like a cargo tool);
/// otherwise the run aborts with [`Error::RequiredToolMissing`], stating the
/// requirement and any `hint`.
///
/// # Errors
/// Returns [`Error::EmptyToolCommand`] if `check` is empty (normally caught by
/// [`validate`]), [`Error::RequiredToolMissing`] when the tool is absent and has
/// no `install`, or [`Error::ToolInstallSpawn`] / [`Error::ToolInstallFailed`]
/// if a declared install cannot be launched or exits non-zero.
fn ensure_os(name: &str, tool: &OsTool, name_width: usize) -> Result<Option<UpdateRecord>> {
    let Some((program, args)) = tool.check.split_first() else {
        return Err(Error::EmptyToolCommand {
            tool: name.to_string(),
            field: "check",
        });
    };

    eprint_tool("Checking", CHECK_TAG, name, &[], name_width);
    let output = ProcessCommand::new(program).args(args).output();
    let present = output.as_ref().is_ok_and(|o| o.status.success());

    if present {
        eprint_tool("Present", CHECK_TAG, name, &[], name_width);
        return Ok(None);
    }

    if tool.install.is_empty() {
        return Err(Error::RequiredToolMissing {
            tool: name.to_string(),
            hint: tool.hint.clone(),
        });
    }
    eprint_tool("Installing", CHECK_TAG, name, &tool.install, name_width);
    run_install(name, &tool.install)?;
    Ok(Some(UpdateRecord {
        name: name.to_string(),
        from: None,
        to: None,
    }))
}

/// Ensure a fish shell function is available.
///
/// Announces the check (a `Checking` line) and runs
/// `fish -c "functions --query <name>"` to detect presence. When present,
/// prints a `Present` line. When absent, aborts with
/// [`Error::RequiredToolMissing`], stating the requirement and any `hint`.
///
/// The check covers user-defined functions, autoloaded functions, and builtins,
/// matching what `fish -c "functions --query <name>"` reports.
fn ensure_fish(name: &str, tool: &FishTool, name_width: usize) -> Result<Option<UpdateRecord>> {
    eprint_tool("Checking", FISH_TAG, name, &[], name_width);
    let present = ProcessCommand::new("fish")
        .args(["-c", &format!("functions --query {name}")])
        .status()
        .is_ok_and(|s| s.success());
    if present {
        eprint_tool("Present", FISH_TAG, name, &[], name_width);
        Ok(None)
    } else {
        Err(Error::RequiredToolMissing {
            tool: name.to_string(),
            hint: tool.hint.clone(),
        })
    }
}

/// Parse the installed version from a `check` command's captured output. Both
/// stdout and stderr are searched, since `--version` lands on stderr for some
/// tools (notably cargo subcommands invoked as `cargo <sub> --version`).
fn parse_installed_version(stdout: &[u8], stderr: &[u8]) -> Option<Version> {
    parse_version_token(&String::from_utf8_lossy(stdout))
        .or_else(|| parse_version_token(&String::from_utf8_lossy(stderr)))
}

/// When `update = true`, re-install the tool if [`latest_version`] reports a
/// version newer than `installed`. Version-check failures are non-fatal.
///
/// Returns `Some(`[`UpdateRecord`]`)` when the tool was updated, `None` when
/// already up to date or when the version check could not be performed.
fn update_if_newer(
    name: &str,
    tool: &CargoTool,
    installed: Option<&Version>,
    name_width: usize,
) -> Result<Option<UpdateRecord>> {
    // Without a parseable installed version there is nothing to compare against,
    // and reinstalling on every run is worse than keeping the current one — so
    // skip the update (and the registry lookup) entirely.
    let Some(installed) = installed else {
        eprint_tool(
            "Warning",
            CHECK_TAG,
            name,
            &["could not determine installed version; keeping current".to_string()],
            name_width,
        );
        return Ok(None);
    };
    let latest = match latest_version(tool) {
        Ok(Some(latest)) => latest,
        Ok(None) => return Ok(None),
        Err(message) => {
            eprint_tool(
                "Warning",
                CHECK_TAG,
                name,
                &[format!("version check failed: {message}")],
                name_width,
            );
            return Ok(None);
        }
    };
    if latest > *installed {
        eprint_tool(
            "Updating",
            CHECK_TAG,
            name,
            &[format!("{installed} -> {latest}")],
            name_width,
        );
        run_install(name, &tool.install)?;
        return Ok(Some(UpdateRecord {
            name: name.to_string(),
            from: Some(installed.to_string()),
            to: Some(latest.to_string()),
        }));
    }
    eprint_tool(
        "Up to date",
        CHECK_TAG,
        name,
        &[installed.to_string()],
        name_width,
    );
    Ok(None)
}

/// Run a tool's `install` command, inheriting stdio so its progress is visible.
/// Shared by cargo and os tools, so it takes the `install` slice directly.
fn run_install(name: &str, install: &[String]) -> Result<()> {
    let Some((program, args)) = install.split_first() else {
        return Err(Error::EmptyToolCommand {
            tool: name.to_string(),
            field: "install",
        });
    };
    let status = ProcessCommand::new(program)
        .args(args)
        .status()
        .map_err(|source| Error::ToolInstallSpawn {
            tool: name.to_string(),
            program: program.clone(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::ToolInstallFailed {
            tool: name.to_string(),
            status,
        })
    }
}

/// Resolve the latest available version of `tool` via its [`SemverCheck`] mode.
///
/// Returns `Ok(None)` when no version could be determined (e.g. the registry
/// reports none). The `Err` carries a human-readable message for a non-fatal
/// warning — a failed lookup must not abort a run.
fn latest_version(tool: &CargoTool) -> core::result::Result<Option<Version>, String> {
    match tool.semver_check {
        SemverCheck::CratesIo => {
            let crate_name = tool
                .crate_name
                .as_deref()
                .ok_or("no 'crate' declared for the crates-io semver check")?;
            latest_crate_version(crate_name)
        }
    }
}

/// The crates.io registry response shape we care about.
#[derive(Debug, Deserialize)]
struct CratesIoResponse {
    #[serde(rename = "crate")]
    krate: CratesIoCrate,
}

/// The `crate` object within a crates.io registry response.
#[derive(Debug, Deserialize)]
struct CratesIoCrate {
    /// The highest stable (non-prerelease, non-yanked) version, when any exists.
    #[serde(default)]
    max_stable_version: Option<String>,
    /// The newest published version, used as a fallback when there is no stable
    /// release.
    #[serde(default)]
    newest_version: Option<String>,
}

/// Pick the version to compare against from a crates.io `crate` object,
/// preferring the highest stable release over the newest published one.
fn pick_version(krate: &CratesIoCrate) -> Option<Version> {
    krate
        .max_stable_version
        .as_deref()
        .or(krate.newest_version.as_deref())
        .and_then(|v| Version::parse(v).ok())
}

/// Returns a shared `ureq::Agent` configured to verify TLS certificates using the
/// OS platform verifier rather than a bundled Mozilla root list. This allows the
/// agent to trust corporate CA certificates installed in the system keystore, which
/// is necessary when running behind a firewall that performs TLS inspection.
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    })
}

/// Query the crates.io registry API for the latest version of `crate_name`.
fn latest_crate_version(crate_name: &str) -> core::result::Result<Option<Version>, String> {
    let url = format!("https://crates.io/api/v1/crates/{crate_name}");
    let mut response = http_agent()
        .get(&url)
        .header(
            "User-Agent",
            "cargo-rake (https://github.com/rustyhorde/cargo-rake)",
        )
        .call()
        .map_err(|source| source.to_string())?;
    let body: CratesIoResponse = response
        .body_mut()
        .read_json()
        .map_err(|source| source.to_string())?;
    Ok(pick_version(&body.krate))
}

/// Parse the first whitespace-delimited token of `stdout` that looks like a
/// semantic version (a leading `v` is tolerated), e.g. `"cargo-matrix 0.3.1"`
/// yields `0.3.1`. Returns `None` when no token parses.
fn parse_version_token(stdout: &str) -> Option<Version> {
    stdout
        .split_whitespace()
        .find_map(|token| Version::parse(token.strip_prefix('v').unwrap_or(token)).ok())
}

/// Print a blank separator followed by one `Updated` or `Installed` status
/// line per [`UpdateRecord`], using `print_label` so the labels align in the
/// shared status column. Does nothing when `updates` is empty.
pub fn print_update_summary(updates: &[UpdateRecord]) {
    if updates.is_empty() {
        return;
    }
    let _ = writeln!(stderr()).ok();
    for record in updates {
        let (label, info) = match (&record.from, &record.to) {
            (Some(from), Some(to)) => ("Updated", format!("{} {from} → {to}", record.name)),
            (Some(from), None) => ("Updated", format!("{} {from} → ?", record.name)),
            (None, Some(to)) => ("Installed", format!("{} {to}", record.name)),
            (None, None) => ("Installed", record.name.clone()),
        };
        print_label(label, &info);
    }
}

/// Print a tool status line via [`print_label`]: the `label` (e.g. `Checking`,
/// `Installing`) as a right-justified bold-cyan prefix in the shared status
/// column, followed by a green `[ <tag> ]` tag right-aligned to `name_width`
/// (matching the command-name column), then the tool `name` and any `detail`.
/// `tag` is `"check"` for cargo/os tools and `"fish"` for fish function tools.
fn eprint_tool(label: &str, tag: &str, name: &str, detail: &[String], name_width: usize) {
    let tag_str = if color_stderr() {
        format!("{GREEN}[ {tag:>name_width$} ]{RESET}")
    } else {
        format!("[ {tag:>name_width$} ]")
    };
    let detail = detail.join(" ");
    let info = if detail.is_empty() {
        format!("{tag_str} {name}")
    } else {
        format!("{tag_str} {name} {detail}")
    };
    print_label(label, &info);
}

#[cfg(test)]
mod tests {
    use super::{
        CargoTool, CratesIoCrate, CratesIoResponse, OsTool, SemverCheck, ToolTable, UpdateRecord,
        ensure_cargo, ensure_os, parse_version_token, pick_version, validate,
    };
    use crate::{error::Error, rakefile::Rakefile};
    use semver::Version;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SAMPLE: &str = r#"
[tool.cargo.matrix]
crate = "cargo-matrix"
check = ["cargo-matrix", "--version"]
install = ["cargo", "install", "cargo-matrix"]

[target.build]
tools = ["matrix"]
[[target.build.command]]
name = "build"
cmd = ["cargo", "matrix", "build"]
"#;

    #[test]
    fn parses_tool_table_with_defaults() -> TestResult {
        let rakefile = Rakefile::from_toml_str(SAMPLE)?;
        let tool = rakefile
            .tools()
            .cargo
            .get("matrix")
            .ok_or("expected a 'matrix' tool")?;
        assert_eq!(tool.crate_name.as_deref(), Some("cargo-matrix"));
        assert_eq!(tool.check, vec!["cargo-matrix", "--version"]);
        assert!(!tool.update);
        assert!(matches!(tool.semver_check, SemverCheck::CratesIo));
        let build = rakefile
            .target("build")
            .ok_or("expected a 'build' target")?;
        assert_eq!(build.tools, vec!["matrix".to_string()]);
        Ok(())
    }

    #[test]
    fn explicit_crates_io_semver_check_parses() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
                    update = true\ncrate = \"x\"\nsemver_check = \"crates-io\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let entry = rakefile
            .tools()
            .cargo
            .get("x")
            .ok_or("expected an 'x' tool")?;
        assert!(entry.update);
        assert!(matches!(entry.semver_check, SemverCheck::CratesIo));
        Ok(())
    }

    #[test]
    fn unknown_tool_reference_is_rejected() -> TestResult {
        let toml = "[target.build]\ntools = [\"nope\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::UnknownTool { target, tool }) => {
                assert_eq!(target, "build");
                assert_eq!(tool, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownTool, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_command_tool_reference_is_rejected() -> TestResult {
        let toml = "[[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\ntools = [\"nope\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::UnknownCommandTool {
                target,
                command,
                tool,
            }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                assert_eq!(tool, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownCommandTool, got {other:?}").into()),
        }
    }

    #[test]
    fn tool_declared_at_both_levels_is_rejected() -> TestResult {
        let toml = "[tool.cargo.t]\ncheck = [\"true\"]\ninstall = [\"true\"]\n\
                    [target.build]\ntools = [\"t\"]\n\
                    [[target.build.command]]\nname = \"c\"\ncmd = [\"true\"]\ntools = [\"t\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::ToolDeclaredAtBothLevels {
                target,
                command,
                tool,
            }) => {
                assert_eq!(target, "build");
                assert_eq!(command, "c");
                assert_eq!(tool, "t");
                Ok(())
            }
            other => Err(format!("expected ToolDeclaredAtBothLevels, got {other:?}").into()),
        }
    }

    #[test]
    fn empty_check_is_rejected() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = []\ninstall = [\"cargo\", \"install\", \"x\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyToolCommand { tool, field }) => {
                assert_eq!(tool, "x");
                assert_eq!(field, "check");
                Ok(())
            }
            other => Err(format!("expected EmptyToolCommand, got {other:?}").into()),
        }
    }

    #[test]
    fn empty_install_is_rejected() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = [\"x\", \"-V\"]\ninstall = []\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyToolCommand { tool, field }) => {
                assert_eq!(tool, "x");
                assert_eq!(field, "install");
                Ok(())
            }
            other => Err(format!("expected EmptyToolCommand, got {other:?}").into()),
        }
    }

    #[test]
    fn update_without_crate_is_rejected() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
                    update = true\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::ToolUpdateMissingCrate { tool }) => {
                assert_eq!(tool, "x");
                Ok(())
            }
            other => Err(format!("expected ToolUpdateMissingCrate, got {other:?}").into()),
        }
    }

    #[test]
    fn well_formed_tool_validates() -> TestResult {
        // Parsing succeeds, which runs validation.
        let _rakefile = Rakefile::from_toml_str(SAMPLE)?;
        Ok(())
    }

    #[test]
    fn parse_version_token_cases() {
        let cases: &[(&str, Option<&str>)] = &[
            ("cargo-matrix 0.3.1", Some("0.3.1")),
            ("tool v1.2.3", Some("1.2.3")),
            ("1.0.0", Some("1.0.0")),
            ("no version here", None),
            ("", None),
        ];
        for &(input, expected) in cases {
            let got = parse_version_token(input);
            let expected = expected.and_then(|v| Version::parse(v).ok());
            assert_eq!(got, expected, "input: {input:?}");
        }
    }

    #[test]
    fn parse_installed_version_reads_either_stream() {
        // cargo subcommands print `--version` to stderr.
        assert_eq!(
            super::parse_installed_version(b"", b"cargo-matrix 0.4.5"),
            Version::parse("0.4.5").ok()
        );
        // stdout is preferred when both carry a version.
        assert_eq!(
            super::parse_installed_version(b"tool 1.2.3", b"stale 0.0.1"),
            Version::parse("1.2.3").ok()
        );
        assert_eq!(super::parse_installed_version(b"", b"no version"), None);
    }

    // Platform-portable exit-0 / exit-1 command slices for tool check/install fields.
    #[cfg(windows)]
    const EXIT0: &[&str] = &["cmd", "/c", "exit", "0"];
    #[cfg(not(windows))]
    const EXIT0: &[&str] = &["true"];

    #[cfg(windows)]
    const EXIT1: &[&str] = &["cmd", "/c", "exit", "1"];
    #[cfg(not(windows))]
    const EXIT1: &[&str] = &["false"];

    /// Build a one-tool [`CargoTool`] for the `ensure_cargo` tests.
    fn tool(check: &[&str], install: &[&str]) -> CargoTool {
        CargoTool {
            crate_name: None,
            check: check.iter().map(|s| (*s).to_string()).collect(),
            install: install.iter().map(|s| (*s).to_string()).collect(),
            update: false,
            semver_check: SemverCheck::CratesIo,
        }
    }

    /// Build a one-tool [`OsTool`] for the `ensure_os` tests.
    fn os_tool(check: &[&str], install: &[&str], hint: Option<&str>) -> OsTool {
        OsTool {
            check: check.iter().map(|s| (*s).to_string()).collect(),
            install: install.iter().map(|s| (*s).to_string()).collect(),
            hint: hint.map(str::to_string),
        }
    }

    #[test]
    fn ensure_present_tool_skips_install() -> TestResult {
        // `check` succeeds, so `install` (which would fail) must not run.
        let record = ensure_cargo("present", &tool(EXIT0, EXIT1), 0)?;
        assert!(record.is_none(), "present tool must return None");
        Ok(())
    }

    #[test]
    fn ensure_absent_tool_installs() -> TestResult {
        // `check` fails (absent) so `install` runs and succeeds; fresh installs
        // have no from/to version since we don't re-probe after install.
        let record = ensure_cargo("absent", &tool(EXIT1, EXIT0), 0)?;
        let r = record.ok_or("expected Some(UpdateRecord) for fresh install")?;
        assert_eq!(r.name, "absent");
        assert!(r.from.is_none(), "fresh install has no prior version");
        assert!(r.to.is_none(), "fresh install has no post version");
        Ok(())
    }

    #[test]
    fn ensure_os_present_tool_is_ok() -> TestResult {
        // `check` succeeds, so a missing `install` is fine and no error fires.
        let record = ensure_os("present", &os_tool(EXIT0, &[], None), 0)?;
        assert!(record.is_none(), "present os tool must return None");
        Ok(())
    }

    #[test]
    fn ensure_os_absent_with_install_runs_it() -> TestResult {
        // Absent (`check` fails), but a declared `install` (exit 0) runs and succeeds.
        let record = ensure_os("absent", &os_tool(EXIT1, EXIT0, None), 0)?;
        let r = record.ok_or("expected Some(UpdateRecord) for os tool install")?;
        assert_eq!(r.name, "absent");
        assert!(r.from.is_none());
        assert!(r.to.is_none());
        Ok(())
    }

    #[test]
    fn ensure_os_absent_without_install_is_required_error() -> TestResult {
        // Absent and no `install`: abort with the requirement, carrying the hint.
        match ensure_os(
            "docker",
            &os_tool(&["false"], &[], Some("install Docker")),
            0,
        ) {
            Err(Error::RequiredToolMissing { tool, hint }) => {
                assert_eq!(tool, "docker");
                assert_eq!(hint.as_deref(), Some("install Docker"));
                Ok(())
            }
            other => Err(format!("expected RequiredToolMissing, got {other:?}").into()),
        }
    }

    #[test]
    fn ensure_os_absent_install_failure_is_error() -> TestResult {
        // Absent with a declared `install` that fails aborts the run.
        match ensure_os("absent", &os_tool(EXIT1, EXIT1, None), 0) {
            Err(Error::ToolInstallFailed { tool, status }) => {
                assert_eq!(tool, "absent");
                assert!(!status.success());
                Ok(())
            }
            other => Err(format!("expected ToolInstallFailed, got {other:?}").into()),
        }
    }

    #[test]
    fn ensure_update_skips_when_version_unparseable() -> TestResult {
        // Present (`check` = `true`, exit 0) but its stdout has no version, so the
        // installed version is unknown. With `update = true` this must NOT
        // reinstall (a reinstall would run `install` = `false` and surface
        // `ToolInstallFailed`). Runs offline: the `None` arm returns before any
        // registry lookup.
        let mut tool = tool(EXIT0, EXIT1);
        tool.update = true;
        tool.crate_name = Some("anything".to_string());
        drop(ensure_cargo("present-no-version", &tool, 0)?);
        Ok(())
    }

    #[test]
    fn ensure_install_failure_is_error() -> TestResult {
        match ensure_cargo("absent", &tool(EXIT1, EXIT1), 0) {
            Err(Error::ToolInstallFailed { tool, status }) => {
                assert_eq!(tool, "absent");
                assert!(!status.success());
                Ok(())
            }
            other => Err(format!("expected ToolInstallFailed, got {other:?}").into()),
        }
    }

    #[test]
    fn ensure_install_spawn_failure_is_error() -> TestResult {
        match ensure_cargo(
            "absent",
            &tool(&["false"], &["this-program-does-not-exist-cargo-rake"]),
            0,
        ) {
            Err(Error::ToolInstallSpawn { tool, program, .. }) => {
                assert_eq!(tool, "absent");
                assert_eq!(program, "this-program-does-not-exist-cargo-rake");
                Ok(())
            }
            other => Err(format!("expected ToolInstallSpawn, got {other:?}").into()),
        }
    }

    #[test]
    fn validate_accepts_well_formed_table() -> TestResult {
        let rakefile = Rakefile::from_toml_str(SAMPLE)?;
        validate(rakefile.tools(), rakefile.targets())?;
        Ok(())
    }

    #[test]
    fn pick_version_prefers_max_stable() {
        let krate = CratesIoCrate {
            max_stable_version: Some("0.3.1".to_string()),
            newest_version: Some("0.4.0-rc.1".to_string()),
        };
        assert_eq!(pick_version(&krate), Version::parse("0.3.1").ok());
    }

    #[test]
    fn pick_version_falls_back_to_newest() {
        let krate = CratesIoCrate {
            max_stable_version: None,
            newest_version: Some("0.4.0-rc.1".to_string()),
        };
        assert_eq!(pick_version(&krate), Version::parse("0.4.0-rc.1").ok());
    }

    #[test]
    fn crates_io_response_deserializes() -> TestResult {
        let json = r#"{"crate":{"max_stable_version":"0.3.1","newest_version":"0.4.0"}}"#;
        let response: CratesIoResponse = serde_json::from_str(json)?;
        assert_eq!(pick_version(&response.krate), Version::parse("0.3.1").ok());
        Ok(())
    }

    /// Live crates.io query — network-gated, run with `--ignored`.
    #[test]
    #[ignore = "network: hits the crates.io API"]
    fn latest_crate_version_live() -> TestResult {
        let version = super::latest_crate_version("serde")?;
        assert!(version.is_some(), "expected a version for serde");
        Ok(())
    }

    /// `validate` is exercised directly here over a hand-built table to keep its
    /// signature used even though `Rakefile::validate` is the normal entry point.
    #[test]
    fn validate_over_empty_maps_is_ok() -> TestResult {
        let tools = ToolTable::default();
        let targets = indexmap::IndexMap::new();
        validate(&tools, &targets)?;
        Ok(())
    }

    #[test]
    fn os_tool_parses_and_validates() -> TestResult {
        let toml = "[tool.os.docker]\ncheck = [\"docker\", \"--version\"]\n\
                    hint = \"install Docker\"\n\
                    [target.build]\ntools = [\"docker\"]\n\
                    [[target.build.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let docker = rakefile
            .tools()
            .os
            .get("docker")
            .ok_or("expected a 'docker' os tool")?;
        assert_eq!(docker.check, vec!["docker", "--version"]);
        assert_eq!(docker.hint.as_deref(), Some("install Docker"));
        assert!(docker.install.is_empty());
        Ok(())
    }

    #[test]
    fn os_tool_empty_check_is_rejected() -> TestResult {
        let toml = "[tool.os.docker]\ncheck = []\n\
                    [target.build]\ntools = [\"docker\"]\n\
                    [[target.build.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyToolCommand { tool, field }) => {
                assert_eq!(tool, "docker");
                assert_eq!(field, "check");
                Ok(())
            }
            other => Err(format!("expected EmptyToolCommand, got {other:?}").into()),
        }
    }

    #[test]
    fn name_in_both_categories_is_rejected() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
                    [tool.os.x]\ncheck = [\"x\", \"-V\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::DuplicateTool { tool }) => {
                assert_eq!(tool, "x");
                Ok(())
            }
            other => Err(format!("expected DuplicateTool, got {other:?}").into()),
        }
    }

    // --- fish tool tests ---

    #[test]
    fn parses_fish_tool_table() -> TestResult {
        let toml = "[tool.fish.my_fn]\nhint = \"define my_fn in fish functions\"\n\
                    [target.build]\ntools = [\"my_fn\"]\n\
                    [[target.build.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let entry = rakefile
            .tools()
            .fish
            .get("my_fn")
            .ok_or("expected a 'my_fn' fish tool")?;
        assert_eq!(
            entry.hint.as_deref(),
            Some("define my_fn in fish functions")
        );
        let build = rakefile
            .target("build")
            .ok_or("expected a 'build' target")?;
        assert_eq!(build.tools, vec!["my_fn".to_string()]);
        Ok(())
    }

    #[test]
    fn parses_fish_tool_without_hint() -> TestResult {
        let toml = "[tool.fish.cd]\n\
                    [target.build]\ntools = [\"cd\"]\n\
                    [[target.build.command]]\nname = \"b\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let entry = rakefile
            .tools()
            .fish
            .get("cd")
            .ok_or("expected a 'cd' fish tool")?;
        assert!(entry.hint.is_none());
        Ok(())
    }

    #[test]
    fn fish_tool_name_in_cargo_and_fish_is_rejected() -> TestResult {
        let toml = "[tool.cargo.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
                    [tool.fish.x]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::DuplicateTool { tool }) => {
                assert_eq!(tool, "x");
                Ok(())
            }
            other => Err(format!("expected DuplicateTool, got {other:?}").into()),
        }
    }

    #[test]
    fn fish_tool_name_in_os_and_fish_is_rejected() -> TestResult {
        let toml = "[tool.os.x]\ncheck = [\"x\", \"-V\"]\n\
                    [tool.fish.x]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::DuplicateTool { tool }) => {
                assert_eq!(tool, "x");
                Ok(())
            }
            other => Err(format!("expected DuplicateTool, got {other:?}").into()),
        }
    }

    #[test]
    fn ensure_fish_present() -> TestResult {
        // Skip when fish is not installed in the current environment.
        if std::process::Command::new("fish")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return Ok(());
        }
        // `cd` is a fish builtin always available.
        drop(super::ensure_fish(
            "cd",
            &super::FishTool { hint: None },
            0,
        )?);
        Ok(())
    }

    #[test]
    fn ensure_fish_absent() -> TestResult {
        match super::ensure_fish(
            "__rake_test_nonexistent_fn_xyz",
            &super::FishTool { hint: None },
            0,
        ) {
            Err(Error::RequiredToolMissing { tool, hint }) => {
                assert_eq!(tool, "__rake_test_nonexistent_fn_xyz");
                assert!(hint.is_none());
                Ok(())
            }
            other => Err(format!("expected RequiredToolMissing, got {other:?}").into()),
        }
    }

    #[test]
    fn ensure_self_update_unparseable_version_is_nonfatal() -> TestResult {
        // An unparseable version prints a Warning and returns Ok(None) with no
        // registry lookup or install attempt.
        let updated = super::ensure_self_update("not-a-version", 0)?;
        assert!(
            updated.is_none(),
            "unparseable version should not trigger an update"
        );
        Ok(())
    }

    #[test]
    fn ensure_self_update_already_up_to_date() -> TestResult {
        // With network: latest_crate_version("cargo-rake") returns the real
        // published version (e.g. 0.4.2), which is <= 99999.0.0, so the
        // "Up to date" branch is taken and Ok(None) is returned.
        // Without network: the version check errors out and the Warning branch
        // is taken instead — also Ok(None).  Either way no install is
        // triggered and the assertion holds.
        let updated = super::ensure_self_update("99999.0.0", 0)?;
        assert!(
            updated.is_none(),
            "impossibly high version should never trigger an install"
        );
        Ok(())
    }

    #[test]
    fn ensure_fish_absent_with_hint() -> TestResult {
        match super::ensure_fish(
            "__rake_test_nonexistent_fn_xyz",
            &super::FishTool {
                hint: Some("add the function".to_string()),
            },
            0,
        ) {
            Err(Error::RequiredToolMissing { tool, hint }) => {
                assert_eq!(tool, "__rake_test_nonexistent_fn_xyz");
                assert_eq!(hint.as_deref(), Some("add the function"));
                Ok(())
            }
            other => Err(format!("expected RequiredToolMissing, got {other:?}").into()),
        }
    }

    // --- tag_for tests ---

    #[test]
    fn tag_for_fish_tool_returns_fish_tag() -> TestResult {
        let toml = "[tool.fish.my_fn]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().tag_for("my_fn"), Some(super::FISH_TAG));
        Ok(())
    }

    #[test]
    fn tag_for_cargo_tool_returns_check_tag() -> TestResult {
        let toml = "[tool.cargo.matrix]\ncheck = [\"matrix\", \"--version\"]\n\
                    install = [\"cargo\", \"install\", \"matrix\"]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().tag_for("matrix"), Some(super::CHECK_TAG));
        Ok(())
    }

    #[test]
    fn tag_for_os_tool_returns_check_tag() -> TestResult {
        let toml = "[tool.os.docker]\ncheck = [\"docker\", \"--version\"]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().tag_for("docker"), Some(super::CHECK_TAG));
        Ok(())
    }

    #[test]
    fn tag_for_unknown_returns_none() -> TestResult {
        let toml = "[[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().tag_for("nonexistent"), None);
        Ok(())
    }

    // --- max_tag_width tests ---

    #[test]
    fn max_tag_width_empty_is_zero() {
        assert_eq!(ToolTable::default().max_tag_width(), 0);
    }

    #[test]
    fn max_tag_width_cargo_only() -> TestResult {
        let toml = "[tool.cargo.matrix]\ncheck = [\"matrix\", \"--version\"]\n\
                    install = [\"cargo\", \"install\", \"matrix\"]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().max_tag_width(), super::CHECK_TAG.len());
        Ok(())
    }

    #[test]
    fn max_tag_width_fish_only() -> TestResult {
        let toml = "[tool.fish.my_fn]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        assert_eq!(rakefile.tools().max_tag_width(), super::FISH_TAG.len());
        Ok(())
    }

    #[test]
    fn max_tag_width_cargo_and_fish() -> TestResult {
        let toml = "[tool.cargo.matrix]\ncheck = [\"matrix\", \"--version\"]\n\
                    install = [\"cargo\", \"install\", \"matrix\"]\n\
                    [tool.fish.my_fn]\n\
                    [[target.default.command]]\nname = \"c\"\ncmd = [\"true\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let expected = super::CHECK_TAG.len().max(super::FISH_TAG.len());
        assert_eq!(rakefile.tools().max_tag_width(), expected);
        Ok(())
    }

    // --- ensure dry-run path tests ---

    #[test]
    fn ensure_dry_run_cargo_tool_announces_without_executing() -> TestResult {
        // check = ["false"] would fail in live mode; dry_run must not spawn it.
        let mut table = ToolTable::default();
        drop(table.cargo.insert("x".to_string(), tool(EXIT1, EXIT1)));
        drop(table.ensure("x", true, 0)?);
        Ok(())
    }

    #[test]
    fn ensure_dry_run_fish_tool_announces_without_executing() -> TestResult {
        // fish function does not exist, but dry_run must not run the check.
        let mut table = ToolTable::default();
        drop(table.fish.insert(
            "__nonexistent_fn".to_string(),
            super::FishTool { hint: None },
        ));
        drop(table.ensure("__nonexistent_fn", true, 0)?);
        Ok(())
    }

    // --- print_update_summary tests ---

    #[test]
    fn print_update_summary_empty_is_noop() {
        // Empty slice: early return, nothing written to stderr.
        super::print_update_summary(&[]);
    }

    #[test]
    fn print_update_summary_updated_both_versions() {
        // (Some(from), Some(to)) → "Updated" label with "name from → to".
        super::print_update_summary(&[UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: Some("0.9.0".to_string()),
            to: Some("0.9.90".to_string()),
        }]);
    }

    #[test]
    fn print_update_summary_updated_from_only() {
        // (Some(from), None) → "Updated" label with "name from → ?".
        super::print_update_summary(&[UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: Some("0.9.0".to_string()),
            to: None,
        }]);
    }

    #[test]
    fn print_update_summary_installed_with_version() {
        // (None, Some(to)) → "Installed" label with "name version".
        super::print_update_summary(&[UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: None,
            to: Some("0.9.90".to_string()),
        }]);
    }

    #[test]
    fn print_update_summary_installed_no_versions() {
        // (None, None) → "Installed" label with bare name.
        super::print_update_summary(&[UpdateRecord {
            name: "cargo-nextest".to_string(),
            from: None,
            to: None,
        }]);
    }
}
