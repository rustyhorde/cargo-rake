//! External tool dependencies for targets.
//!
//! A `Rakefile.toml` may declare a top-level `[tool.<name>]` table of external
//! tools (cargo subcommands and the like) that targets reference by name via
//! their `tools` list. Before a target's commands run, each referenced tool is
//! [`ensure`]d: its `check` command probes for local presence, and if it is
//! missing the `install` command is run. When a tool sets `update = true`, the
//! installed version is compared against the latest reported by its
//! [`SemverCheck`] mode and re-installed if a newer one exists. Each ensure
//! announces a `Checking` line and prints an outcome (`Present`, `Up to date`,
//! `Installing`, or `Updating`), each a right-justified bold-blue status-label
//! prefix in the run's shared column, matching the command status lines.

use std::process::Command as ProcessCommand;

use indexmap::IndexMap;
use semver::Version;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    rakefile::{Target, print_label},
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

/// A single external tool declared in the `[tool.<name>]` table.
#[derive(Debug, Deserialize)]
pub struct Tool {
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
    /// [`Tool::semver_check`] and re-install if a newer one exists. Defaults to
    /// `false` (use whatever is already installed).
    #[serde(default)]
    pub update: bool,
    /// How the `update` check resolves the latest version. Defaults to
    /// [`SemverCheck::CratesIo`].
    #[serde(default)]
    pub semver_check: SemverCheck,
}

/// Validate the tool table and every target's `tools` references.
///
/// Each tool's `check` and `install` must be non-empty, and a tool with
/// `update = true` under the `crates-io` semver check must declare a `crate`.
/// Every `tools` entry on a target must name a tool in the table.
///
/// # Errors
/// Returns [`Error::EmptyToolCommand`], [`Error::ToolUpdateMissingCrate`], or
/// [`Error::UnknownTool`] as appropriate.
pub(crate) fn validate(
    tools: &IndexMap<String, Tool>,
    targets: &IndexMap<String, Target>,
) -> Result<()> {
    for (name, tool) in tools {
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

    for (name, target) in targets {
        for tool in &target.tools {
            if !tools.contains_key(tool) {
                return Err(Error::UnknownTool {
                    target: name.clone(),
                    tool: tool.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Ensure a single tool is available, installing or updating it as needed.
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
pub(crate) fn ensure(name: &str, tool: &Tool) -> Result<()> {
    let Some((program, args)) = tool.check.split_first() else {
        return Err(Error::EmptyToolCommand {
            tool: name.to_string(),
            field: "check",
        });
    };

    eprint_tool("Checking", name, &[]);
    let output = ProcessCommand::new(program).args(args).output();
    let present = output.as_ref().is_ok_and(|o| o.status.success());

    if !present {
        eprint_tool("Installing", name, &tool.install);
        return run_install(name, tool);
    }

    let installed = output
        .ok()
        .and_then(|o| parse_installed_version(&o.stdout, &o.stderr));

    if tool.update {
        update_if_newer(name, tool, installed.as_ref())?;
    } else {
        // Present and not an `update` tool: report what is already installed,
        // including the parsed version when the `check` output carried one.
        let detail = installed.map_or_else(Vec::new, |v| vec![v.to_string()]);
        eprint_tool("Present", name, &detail);
    }
    Ok(())
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
fn update_if_newer(name: &str, tool: &Tool, installed: Option<&Version>) -> Result<()> {
    // Without a parseable installed version there is nothing to compare against,
    // and reinstalling on every run is worse than keeping the current one — so
    // skip the update (and the registry lookup) entirely.
    let Some(installed) = installed else {
        eprint_tool(
            "Warning",
            name,
            &["could not determine installed version; keeping current".to_string()],
        );
        return Ok(());
    };
    let latest = match latest_version(tool) {
        Ok(Some(latest)) => latest,
        Ok(None) => return Ok(()),
        Err(message) => {
            eprint_tool(
                "Warning",
                name,
                &[format!("version check failed: {message}")],
            );
            return Ok(());
        }
    };
    if latest > *installed {
        eprint_tool("Updating", name, &[format!("{installed} -> {latest}")]);
        return run_install(name, tool);
    }
    eprint_tool("Up to date", name, &[installed.to_string()]);
    Ok(())
}

/// Run a tool's `install` command, inheriting stdio so its progress is visible.
fn run_install(name: &str, tool: &Tool) -> Result<()> {
    let Some((program, args)) = tool.install.split_first() else {
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
fn latest_version(tool: &Tool) -> core::result::Result<Option<Version>, String> {
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

/// Query the crates.io registry API for the latest version of `crate_name`.
fn latest_crate_version(crate_name: &str) -> core::result::Result<Option<Version>, String> {
    let url = format!("https://crates.io/api/v1/crates/{crate_name}");
    let mut response = ureq::get(&url)
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

/// Print a tool status line via [`print_label`]: the `label` (e.g. `Checking`,
/// `Installing`) as a right-justified bold-blue prefix in the shared status
/// column, followed by the tool `name` and any `detail`. With an empty `detail`
/// only the name is shown, e.g. `Checking matrix`.
fn eprint_tool(label: &str, name: &str, detail: &[String]) {
    let detail = detail.join(" ");
    let info = if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    };
    print_label(label, &info);
}

#[cfg(test)]
mod tests {
    use super::{
        CratesIoCrate, CratesIoResponse, SemverCheck, Tool, ensure, parse_version_token,
        pick_version, validate,
    };
    use crate::{error::Error, rakefile::Rakefile};
    use indexmap::IndexMap;
    use semver::Version;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SAMPLE: &str = r#"
[tool.matrix]
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
        let toml = "[tool.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
                    update = true\ncrate = \"x\"\nsemver_check = \"crates-io\"\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let entry = rakefile.tools().get("x").ok_or("expected an 'x' tool")?;
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
    fn empty_check_is_rejected() -> TestResult {
        let toml = "[tool.x]\ncheck = []\ninstall = [\"cargo\", \"install\", \"x\"]\n";
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
        let toml = "[tool.x]\ncheck = [\"x\", \"-V\"]\ninstall = []\n";
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
        let toml = "[tool.x]\ncheck = [\"x\", \"-V\"]\ninstall = [\"cargo\", \"install\", \"x\"]\n\
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

    /// Build a one-tool table for the `ensure` tests.
    fn tool(check: &[&str], install: &[&str]) -> Tool {
        Tool {
            crate_name: None,
            check: check.iter().map(|s| (*s).to_string()).collect(),
            install: install.iter().map(|s| (*s).to_string()).collect(),
            update: false,
            semver_check: SemverCheck::CratesIo,
        }
    }

    #[test]
    fn ensure_present_tool_skips_install() -> TestResult {
        // `check` succeeds, so `install` (which would fail) must not run.
        ensure("present", &tool(&["true"], &["false"]))?;
        Ok(())
    }

    #[test]
    fn ensure_absent_tool_installs() -> TestResult {
        // `check` fails (absent) so `install` runs and succeeds.
        ensure("absent", &tool(&["false"], &["true"]))?;
        Ok(())
    }

    #[test]
    fn ensure_update_skips_when_version_unparseable() -> TestResult {
        // Present (`check` = `true`, exit 0) but its stdout has no version, so the
        // installed version is unknown. With `update = true` this must NOT
        // reinstall (a reinstall would run `install` = `false` and surface
        // `ToolInstallFailed`). Runs offline: the `None` arm returns before any
        // registry lookup.
        let mut tool = tool(&["true"], &["false"]);
        tool.update = true;
        tool.crate_name = Some("anything".to_string());
        ensure("present-no-version", &tool)?;
        Ok(())
    }

    #[test]
    fn ensure_install_failure_is_error() -> TestResult {
        match ensure("absent", &tool(&["false"], &["false"])) {
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
        match ensure(
            "absent",
            &tool(&["false"], &["this-program-does-not-exist-cargo-rake"]),
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

    /// `validate` is exercised directly here over a hand-built map to keep its
    /// signature used even though `Rakefile::validate` is the normal entry point.
    #[test]
    fn validate_over_empty_maps_is_ok() -> TestResult {
        let tools: IndexMap<String, Tool> = IndexMap::new();
        let targets = IndexMap::new();
        validate(&tools, &targets)?;
        Ok(())
    }
}
