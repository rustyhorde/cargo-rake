//! End-to-end CLI tests for the `cargo-rake` binary (the `cargo rake`
//! subcommand).
//!
//! Cargo invokes the subcommand as `cargo rake ...`, so argv arrives as
//! `[cargo-rake, rake, ...]`; the binary drops that leading `rake`. These tests
//! exercise the same behavior as the standalone `rake` binary, invoked with the
//! leading `rake` arg, plus the arg-stripping that is unique to this binary.
//! Per project convention they use `Result<(), Box<dyn Error>>` and `?` rather
//! than `unwrap`/`expect`.

use std::error::Error;
use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

/// A Rakefile exercising plain targets, a `depends_on` chain, a failing target,
/// and a `skip_on_error` dependency feeding a dependent.
const SAMPLE: &str = r#"
[[target.hello.command]]
name = "say"
cmd = ["echo", "Hello from rake!"]

[target.default]
depends_on = ["hello"]
[[target.default.command]]
name = "say"
cmd = ["echo", "Running default target"]

[[target.boom.command]]
name = "fail"
cmd = ["sh", "-c", "exit 3"]

[[target.skip.command]]
name = "flaky"
cmd = ["sh", "-c", "exit 1"]
skip_on_error = true

[target.after_skip]
depends_on = ["skip"]
[[target.after_skip.command]]
name = "say"
cmd = ["echo", "ran after skip"]
"#;

/// Write `contents` to a `Rakefile.toml` in a fresh temp dir, returning the dir
/// (kept alive so it isn't deleted) for the caller to derive the path from.
fn rakefile_dir(contents: &str) -> Result<TempDir, Box<dyn Error>> {
    let dir = TempDir::new()?;
    fs::write(dir.path().join("Rakefile.toml"), contents)?;
    Ok(dir)
}

/// `cargo-rake rake -f <SAMPLE>`, simulating a `cargo rake ...` invocation,
/// ready for further args.
fn cargo_rake(dir: &TempDir) -> Result<Command, Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("cargo-rake")?;
    let _ = cmd
        .arg("rake")
        .arg("-f")
        .arg(dir.path().join("Rakefile.toml"));
    Ok(cmd)
}

#[test]
fn list_prints_targets() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .arg("--list")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"))
        .stdout(predicate::str::contains("say: echo Hello from rake!"))
        .stdout(predicate::str::contains("depends_on: hello"))
        .stdout(predicate::str::contains(
            "flaky: sh -c exit 1 (skip_on_error)",
        ));
    Ok(())
}

#[test]
fn version_flag_prints_semver() -> TestResult {
    Command::cargo_bin("cargo-rake")?
        .args(["rake", "-V"])
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")))
        .stdout(predicate::str::contains("rake"));
    Ok(())
}

#[test]
fn help_shows_cargo_rake_bin_name() -> TestResult {
    Command::cargo_bin("cargo-rake")?
        .args(["rake", "--help"])
        .assert()
        .success()
        // clap's configured `bin_name = "cargo rake"` shows in the usage line.
        .stdout(predicate::str::contains("cargo rake"));
    Ok(())
}

#[test]
fn runs_named_target() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello from rake!"))
        // The per-command status and runtime lines are printed to stderr; under
        // assert_cmd stderr is not a TTY, so they appear uncolored. Commands use
        // a fixed "Running" prefix (5 leading spaces in the 12-char column)
        // followed by the "[ rake ]" tag and `[ name ] program args`.
        .stderr(predicate::str::contains(
            "     Running [ rake ] [ say ] echo Hello from rake!",
        ))
        // Labels share that column: per-command "Cmd Runtime" gets 1 leading
        // space, the final "Runtime" gets 5.
        .stderr(predicate::str::contains(" Cmd Runtime "))
        .stderr(predicate::str::contains("     Runtime "));
    Ok(())
}

#[test]
fn runs_multiple_named_targets() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    // Two roots given together: `hello` and `after_skip` (which runs `skip`
    // first). Both their command outputs appear in one run.
    cargo_rake(&dir)?
        .arg("hello")
        .arg("after_skip")
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello from rake!"))
        .stdout(predicate::str::contains("ran after skip"));
    Ok(())
}

#[test]
fn runs_default_target_when_none_given() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .assert()
        .success()
        // `default` depends on `hello`, so both run, deps first.
        .stdout(predicate::str::contains("Hello from rake!"))
        .stdout(predicate::str::contains("Running default target"));
    Ok(())
}

#[test]
fn missing_rakefile_errors() -> TestResult {
    Command::cargo_bin("cargo-rake")?
        .args(["rake", "-f", "does-not-exist.toml"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unable to read Rakefile"));
    Ok(())
}

#[test]
fn invalid_toml_errors() -> TestResult {
    let dir = rakefile_dir("oops")?;
    cargo_rake(&dir)?
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unable to parse Rakefile"));
    Ok(())
}

#[test]
fn unknown_target_errors() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .arg("nope")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown target 'nope'"));
    Ok(())
}

#[test]
fn failing_target_propagates_exit_code() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?.arg("boom").assert().code(3);
    Ok(())
}

#[test]
fn skip_on_error_continues_chain() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .arg("after_skip")
        .assert()
        // `skip` fails but is tolerated, so the chain reaches `after_skip`.
        .success()
        .stdout(predicate::str::contains("ran after skip"));
    Ok(())
}

/// A target defined purely by `depends_on` (no commands of its own) is valid: it
/// runs its dependencies in order and exits 0.
const AGGREGATOR: &str = r#"
[[target.one.command]]
name = "say"
cmd = ["echo", "ran one"]

[[target.two.command]]
name = "say"
cmd = ["echo", "ran two"]

[target.all]
depends_on = ["one", "two"]
"#;

#[test]
fn depends_only_target_runs_dependencies() -> TestResult {
    let dir = rakefile_dir(AGGREGATOR)?;
    cargo_rake(&dir)?
        .arg("all")
        .assert()
        .success()
        .stdout(predicate::str::contains("ran one"))
        .stdout(predicate::str::contains("ran two"));
    Ok(())
}

/// A target whose tool is reported absent (`check` is `false`) and whose
/// `install` is a portable no-op (`true`), so the run installs then proceeds.
const NEEDS_TOOL: &str = r#"
[tool.cargo.widget]
check = ["false"]
install = ["true"]

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["echo", "built with widget"]
"#;

#[test]
fn missing_tool_is_installed_before_target() -> TestResult {
    let dir = rakefile_dir(NEEDS_TOOL)?;
    cargo_rake(&dir)?
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("built with widget"))
        // The install notice is printed to stderr: the right-justified
        // "Installing" prefix followed by the "[ rake ]" tag and the tool name.
        .stderr(predicate::str::contains("Installing [ rake ] widget"));
    Ok(())
}

/// An os tool reported absent (`check` is `false`) with no `install`, so the run
/// aborts before the command with the requirement message and the `hint`.
const NEEDS_OS_TOOL: &str = r#"
[tool.os.widget]
check = ["false"]
hint = "install widget from your package manager"

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["echo", "should not run"]
"#;

#[test]
fn missing_required_os_tool_aborts() -> TestResult {
    let dir = rakefile_dir(NEEDS_OS_TOOL)?;
    cargo_rake(&dir)?
        .arg("build")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("should not run").not())
        .stderr(predicate::str::contains(
            "the 'widget' tool is required but not installed",
        ))
        .stderr(predicate::str::contains(
            "install widget from your package manager",
        ));
    Ok(())
}

#[test]
fn strips_leading_rake_arg() -> TestResult {
    // `cargo-rake rake --list` — the leading `rake` is dropped so the rest
    // parses like the standalone binary.
    let dir = rakefile_dir(SAMPLE)?;
    Command::cargo_bin("cargo-rake")?
        .arg("rake")
        .arg("--list")
        .arg("-f")
        .arg(dir.path().join("Rakefile.toml"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    Ok(())
}

#[test]
fn works_without_rake_prefix() -> TestResult {
    // Stripping is conditional on argv[1] == "rake"; without it the args still
    // parse, since the first arg here is `--list`, not `rake`.
    let dir = rakefile_dir(SAMPLE)?;
    Command::cargo_bin("cargo-rake")?
        .arg("--list")
        .arg("-f")
        .arg(dir.path().join("Rakefile.toml"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    Ok(())
}
