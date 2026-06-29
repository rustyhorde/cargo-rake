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
#[cfg(not(windows))]
const SAMPLE: &str = r#"
update = false

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

#[cfg(windows)]
const SAMPLE: &str = r#"
update = false

[[target.hello.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "Hello from rake!"]

[target.default]
depends_on = ["hello"]
[[target.default.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "Running default target"]

[[target.boom.command]]
name = "fail"
cmd = ["cmd", "/c", "exit", "3"]

[[target.skip.command]]
name = "flaky"
cmd = ["cmd", "/c", "exit", "1"]
skip_on_error = true

[target.after_skip]
depends_on = ["skip"]
[[target.after_skip.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "ran after skip"]
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
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"))
        .stdout(predicate::str::contains(if cfg!(windows) {
            "say: cmd /c echo Hello from rake!"
        } else {
            "say: echo Hello from rake!"
        }))
        .stdout(predicate::str::contains("depends_on: hello"))
        .stdout(predicate::str::contains(if cfg!(windows) {
            "flaky: cmd /c exit 1 (skip_on_error)"
        } else {
            "flaky: sh -c exit 1 (skip_on_error)"
        }));
    Ok(())
}

#[test]
fn syntax_confirms_valid_rakefile() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    cargo_rake(&dir)?
        .arg("syntax")
        .assert()
        .success()
        .stdout(predicate::str::contains("syntax OK"));
    Ok(())
}

#[test]
fn syntax_reports_invalid_rakefile() -> TestResult {
    // A command with an empty `cmd` is a validation error, surfaced by the
    // load that `syntax` performs.
    let dir = rakefile_dir(
        r#"
[[target.broken.command]]
name = "x"
cmd = []
"#,
    )?;
    cargo_rake(&dir)?
        .arg("syntax")
        .assert()
        .failure()
        .stderr(predicate::str::contains("empty 'cmd'"));
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
        .stderr(predicate::str::contains(if cfg!(windows) {
            "     Running [ rake ] [ say ] cmd /c echo Hello from rake!"
        } else {
            "     Running [ rake ] [ say ] echo Hello from rake!"
        }))
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
fn skips_target_with_caret_prefix() -> TestResult {
    // `all` depends on `clean` and `build`; nothing else needs `clean`, so
    // `^clean` prunes it. `clean`'s output is absent and a `Skipped` line shows.
    let (clean_cmd, build_cmd) = if cfg!(windows) {
        (
            r#"["cmd", "/c", "echo", "CLEANING"]"#,
            r#"["cmd", "/c", "echo", "BUILDING"]"#,
        )
    } else {
        (r#"["echo", "CLEANING"]"#, r#"["echo", "BUILDING"]"#)
    };
    let toml = format!(
        "update = false\n\
         [[target.clean.command]]\nname = \"wipe\"\ncmd = {clean_cmd}\n\
         [[target.build.command]]\nname = \"compile\"\ncmd = {build_cmd}\n\
         [target.all]\ndepends_on = [\"clean\", \"build\"]\n"
    );
    let dir = rakefile_dir(&toml)?;
    cargo_rake(&dir)?
        .arg("all")
        .arg("^clean")
        .assert()
        .success()
        .stdout(predicate::str::contains("BUILDING"))
        .stdout(predicate::str::contains("CLEANING").not())
        .stderr(predicate::str::contains(
            "Skipped [ rake ] [   clean ] skip requested",
        ));
    Ok(())
}

#[test]
fn skip_required_by_other_target_fails_fast() -> TestResult {
    // `build` (not a root) depends on `clean`, so `^clean` is rejected before
    // anything runs (no commands execute, so the echo programs never need to
    // exist on the host).
    let dir = rakefile_dir(
        "update = false\n\
         [[target.clean.command]]\nname = \"wipe\"\ncmd = [\"echo\", \"CLEANING\"]\n\
         [target.build]\ndepends_on = [\"clean\"]\n\
         [[target.build.command]]\nname = \"compile\"\ncmd = [\"echo\", \"BUILDING\"]\n\
         [target.all]\ndepends_on = [\"build\"]\n",
    )?;
    cargo_rake(&dir)?
        .arg("all")
        .arg("^clean")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "target 'clean' cannot be skipped: required by build",
        ));
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

/// A target whose command names a program that cannot be launched aborts the run
/// with a spawn error. Even on that error path the run still prints the failed
/// command's `Cmd Runtime` and the total `Runtime` before the error message.
#[test]
fn spawn_failure_still_prints_runtimes() -> TestResult {
    let toml = "update = false\n\
                [[target.ghost.command]]\nname = \"missing\"\n\
                cmd = [\"this-program-does-not-exist-cargo-rake\"]\n";
    let dir = rakefile_dir(toml)?;
    cargo_rake(&dir)?
        .arg("ghost")
        .assert()
        .failure()
        // The command was attempted, so its per-command runtime prints...
        .stderr(predicate::str::contains(" Cmd Runtime "))
        // ...and the total runtime prints even though the run aborts...
        .stderr(predicate::str::contains("     Runtime "))
        // ...with the spawn error surfaced afterwards.
        .stderr(predicate::str::contains("could not launch"));
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
#[cfg(not(windows))]
const AGGREGATOR: &str = r#"
update = false

[[target.one.command]]
name = "say"
cmd = ["echo", "ran one"]

[[target.two.command]]
name = "say"
cmd = ["echo", "ran two"]

[target.all]
depends_on = ["one", "two"]
"#;

#[cfg(windows)]
const AGGREGATOR: &str = r#"
update = false

[[target.one.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "ran one"]

[[target.two.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "ran two"]

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

/// A target whose tool is reported absent (`check` is `false`/`cmd /c exit 1`)
/// and whose `install` is a portable no-op (`true`/`cmd /c exit 0`), so the
/// run installs then proceeds.
#[cfg(not(windows))]
const NEEDS_TOOL: &str = r#"
update = false

[tool.cargo.widget]
check = ["false"]
install = ["true"]

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["echo", "built with widget"]
"#;

#[cfg(windows)]
const NEEDS_TOOL: &str = r#"
update = false

[tool.cargo.widget]
check = ["cmd", "/c", "exit", "1"]
install = ["cmd", "/c", "exit", "0"]

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "built with widget"]
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
        // "Installing" prefix followed by the "[ rake ]" tag, the "[ check ]"
        // name tag, and the tool name.
        .stderr(predicate::str::contains(
            "Installing [ rake ] [ check ] widget",
        ));
    Ok(())
}

/// An os tool reported absent (`check` is `false`/`cmd /c exit 1`) with no
/// `install`, so the run aborts before the command with the requirement message
/// and the `hint`.
#[cfg(not(windows))]
const NEEDS_OS_TOOL: &str = r#"
update = false

[tool.os.widget]
check = ["false"]
hint = "install widget from your package manager"

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["echo", "should not run"]
"#;

#[cfg(windows)]
const NEEDS_OS_TOOL: &str = r#"
update = false

[tool.os.widget]
check = ["cmd", "/c", "exit", "1"]
hint = "install widget from your package manager"

[target.build]
tools = ["widget"]
[[target.build.command]]
name = "say"
cmd = ["cmd", "/c", "echo", "should not run"]
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
    // `cargo-rake rake list` — the leading `rake` is dropped so the rest
    // parses like the standalone binary.
    let dir = rakefile_dir(SAMPLE)?;
    Command::cargo_bin("cargo-rake")?
        .arg("rake")
        .arg("list")
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
    // parse, since the first arg here is `list`, not `rake`.
    let dir = rakefile_dir(SAMPLE)?;
    Command::cargo_bin("cargo-rake")?
        .arg("list")
        .arg("-f")
        .arg(dir.path().join("Rakefile.toml"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn pre_update_env_var_appears_in_summary() -> TestResult {
    // When the RAKE_SELF_UPDATED env var is set (written by the pre-relaunch
    // binary), the freshly started binary reads it via read_self_update_env()
    // and includes it in the end-of-run print_update_summary() output.
    let dir = rakefile_dir(
        "update = false\n\
         [[target.default.command]]\nname = \"ok\"\ncmd = [\"true\"]\n",
    )?;
    cargo_rake(&dir)?
        .env("RAKE_SELF_UPDATED", "0.4.0|0.5.0")
        .assert()
        .success()
        .stderr(predicate::str::contains("Updated"))
        .stderr(predicate::str::contains("cargo-rake"))
        .stderr(predicate::str::contains("0.4.0"));
    Ok(())
}
