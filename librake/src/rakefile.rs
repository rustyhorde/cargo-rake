//! The `Rakefile.toml` model: parsing, validation, and target execution.

use std::{path::Path, process::Command, process::ExitStatus};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    graph,
};

/// A single named target from the `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Target {
    /// The command to run, as a program followed by its arguments. Spawned
    /// directly (no shell), so it behaves identically on every platform.
    pub cmd: Vec<String>,
    /// Other targets that must run, in order, before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// When `true`, a non-zero exit from this target's command is tolerated:
    /// execution continues with the remaining steps instead of aborting the
    /// dependency chain. Defaults to `false`.
    #[serde(default)]
    pub skip_on_error: bool,
}

/// A parsed `Rakefile.toml`.
#[derive(Debug, Deserialize)]
pub struct Rakefile {
    #[serde(rename = "target", default)]
    targets: IndexMap<String, Target>,
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
    /// Returns [`Error::Parse`] if `s` is not valid TOML or a target is missing
    /// its `cmd`, or [`Error::EmptyCmd`] / [`Error::UnknownDependency`] /
    /// [`Error::CircularDependency`] if validation fails.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let rakefile: Rakefile = toml::from_str(s)?;
        rakefile.validate()?;
        Ok(rakefile)
    }

    /// Every target's `cmd` must be non-empty and the dependency graph must be
    /// valid (no unknown dependencies, no cycles).
    fn validate(&self) -> Result<()> {
        for (name, target) in &self.targets {
            if target.cmd.is_empty() {
                return Err(Error::EmptyCmd {
                    target: name.clone(),
                });
            }
        }
        graph::validate(&self.targets)
    }

    /// The targets, in declaration order.
    #[must_use]
    pub fn targets(&self) -> &IndexMap<String, Target> {
        &self.targets
    }

    /// Look up a single target by name.
    #[must_use]
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    /// Run `name` after its transitive dependencies.
    ///
    /// Steps run in dependency order, each at most once. Execution stops at the
    /// first step that exits non-zero, returning that [`ExitStatus`]; otherwise
    /// the final step's status is returned. A step whose target sets
    /// `skip_on_error` is the exception: a non-zero exit there is tolerated and
    /// execution continues with the remaining steps. A command that runs but
    /// fails is not an [`Error`] — the caller decides what to do with the exit
    /// code.
    ///
    /// # Errors
    /// Returns [`Error::UnknownTarget`] if `name` is not defined, or
    /// [`Error::Spawn`] if a step's program cannot be launched.
    pub fn run(&self, name: &str) -> Result<ExitStatus> {
        let order = graph::execution_order(&self.targets, name)?;
        let mut status = None;
        for step in order {
            let Some(target) = self.targets.get(step) else {
                continue;
            };
            let current = run_one(step, target)?;
            let success = current.success();
            status = Some(current);
            if !success && !target.skip_on_error {
                break;
            }
        }
        // `order` always contains `name`, so `status` is always set here.
        status.ok_or_else(|| Error::UnknownTarget {
            name: name.to_string(),
        })
    }
}

/// Spawn a single target's command, inheriting the parent's stdio.
fn run_one(name: &str, target: &Target) -> Result<ExitStatus> {
    let (program, args) = target.cmd.split_first().ok_or_else(|| Error::EmptyCmd {
        target: name.to_string(),
    })?;
    Command::new(program)
        .args(args)
        .status()
        .map_err(|source| Error::Spawn {
            target: name.to_string(),
            program: program.clone(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::Rakefile;
    use crate::error::Error;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SAMPLE: &str = r#"
[target.build]
cmd = ["cargo", "build", "--all-features"]

[target.test]
cmd = ["cargo", "test"]

[target.all]
cmd = ["cargo", "build", "--release"]
depends_on = ["build", "test"]

[target.'a fancy target name']
cmd = ["cargo", "doc"]
"#;

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
        Ok(())
    }

    #[test]
    fn empty_file_has_no_targets() -> TestResult {
        let rakefile = Rakefile::from_toml_str("")?;
        assert!(rakefile.targets().is_empty());
        Ok(())
    }

    #[test]
    fn missing_cmd_is_a_parse_error() -> TestResult {
        let toml = "[target.build]\ndepends_on = []\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::Parse(_)) => Ok(()),
            other => Err(format!("expected Parse error, got {other:?}").into()),
        }
    }

    #[test]
    fn empty_cmd_is_rejected() -> TestResult {
        let toml = "[target.build]\ncmd = []\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::EmptyCmd { target }) => {
                assert_eq!(target, "build");
                Ok(())
            }
            other => Err(format!("expected EmptyCmd, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_dependency_is_rejected() -> TestResult {
        let toml = "[target.build]\ncmd = [\"cargo\", \"build\"]\ndepends_on = [\"nope\"]\n";
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
        let toml = "[target.a]\ncmd = [\"true\"]\ndepends_on = [\"b\"]\n\
                    [target.b]\ncmd = [\"true\"]\ndepends_on = [\"a\"]\n";
        match Rakefile::from_toml_str(toml) {
            Err(Error::CircularDependency { .. }) => Ok(()),
            other => Err(format!("expected CircularDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn run_unknown_target_errors() -> TestResult {
        let rakefile = Rakefile::from_toml_str(SAMPLE)?;
        match rakefile.run("does-not-exist") {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "does-not-exist");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn run_missing_program_is_spawn_error() -> TestResult {
        let toml =
            "[target.go]\ncmd = [\"this-program-does-not-exist-cargo-rake\", \"--version\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        match rakefile.run("go") {
            Err(Error::Spawn {
                target, program, ..
            }) => {
                assert_eq!(target, "go");
                assert_eq!(program, "this-program-does-not-exist-cargo-rake");
                Ok(())
            }
            other => Err(format!("expected Spawn error, got {other:?}").into()),
        }
    }

    #[test]
    fn run_portable_command_succeeds() -> TestResult {
        let toml = "[target.version]\ncmd = [\"cargo\", \"--version\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let status = rakefile.run("version")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn skip_on_error_defaults_to_false() -> TestResult {
        let toml = "[target.build]\ncmd = [\"cargo\", \"build\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        let build = rakefile
            .target("build")
            .ok_or("expected a 'build' target")?;
        assert!(!build.skip_on_error);
        Ok(())
    }

    #[test]
    fn skip_on_error_continues_chain() -> TestResult {
        let toml = "[target.flaky]\ncmd = [\"false\"]\nskip_on_error = true\n\
                    [target.all]\ncmd = [\"true\"]\ndepends_on = [\"flaky\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `flaky` exits non-zero but opts into skipping, so `all` still runs and
        // its success is the status returned for the whole chain.
        let status = rakefile.run("all")?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn failing_dependency_without_skip_aborts() -> TestResult {
        let toml = "[target.flaky]\ncmd = [\"false\"]\n\
                    [target.all]\ncmd = [\"true\"]\ndepends_on = [\"flaky\"]\n";
        let rakefile = Rakefile::from_toml_str(toml)?;
        // `flaky` fails and does not skip, so the chain stops there: `all` never
        // runs and the returned status reflects the failure.
        let status = rakefile.run("all")?;
        assert!(!status.success());
        Ok(())
    }
}
