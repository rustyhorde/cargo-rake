//! End-to-end CLI tests for the `rake` binary.
//!
//! Each test writes a `Rakefile.toml` into a temporary directory and runs the
//! built binary against it with `-f <path>`, asserting on stdout, stderr, and
//! the process exit code. Per project convention these use
//! `Result<(), Box<dyn Error>>` and the `?` operator rather than
//! `unwrap`/`expect`.

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

/// `rake -f <SAMPLE>` ready for further args.
fn rake(dir: &TempDir) -> Result<Command, Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("rake")?;
    let _ = cmd.arg("-f").arg(dir.path().join("Rakefile.toml"));
    Ok(cmd)
}

#[test]
fn list_prints_targets() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    rake(&dir)?
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
    rake(&dir)?
        .arg("syntax")
        .assert()
        .success()
        .stdout(predicate::str::contains("syntax OK"));
    Ok(())
}

#[test]
fn syntax_reports_invalid_rakefile() -> TestResult {
    let dir = rakefile_dir(
        r#"
[[target.broken.command]]
name = "x"
cmd = []
"#,
    )?;
    rake(&dir)?
        .arg("syntax")
        .assert()
        .failure()
        .stderr(predicate::str::contains("empty 'cmd'"));
    Ok(())
}

#[test]
fn version_flag_prints_semver() -> TestResult {
    Command::cargo_bin("rake")?
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::starts_with(format!(
            "rake {}",
            env!("CARGO_PKG_VERSION")
        )));
    Ok(())
}

#[test]
fn help_flag_succeeds() -> TestResult {
    Command::cargo_bin("rake")?
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("rake"));
    Ok(())
}

#[test]
fn runs_named_target() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    rake(&dir)?
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
    rake(&dir)?
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
    rake(&dir)?
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
    rake(&dir)?
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
    rake(&dir)?
        .assert()
        .success()
        // `default` depends on `hello`, so both run, deps first.
        .stdout(predicate::str::contains("Hello from rake!"))
        .stdout(predicate::str::contains("Running default target"));
    Ok(())
}

#[test]
fn missing_rakefile_errors() -> TestResult {
    Command::cargo_bin("rake")?
        .arg("-f")
        .arg("does-not-exist.toml")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unable to read Rakefile"));
    Ok(())
}

#[test]
fn invalid_toml_errors() -> TestResult {
    let dir = rakefile_dir("oops")?;
    rake(&dir)?
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unable to parse Rakefile"));
    Ok(())
}

#[test]
fn unknown_target_errors() -> TestResult {
    let dir = rakefile_dir(SAMPLE)?;
    rake(&dir)?
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
    rake(&dir)?.arg("boom").assert().code(3);
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
    rake(&dir)?
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
    rake(&dir)?
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
    rake(&dir)?
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
    rake(&dir)?
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

#[test]
fn basic_subcommand_prints_status() -> TestResult {
    // `rake basic` does not need a Rakefile — it exits before loading one.
    // Prints "locked" when no license is configured, or "enabled for <licensee>"
    // when a valid license is present on this machine.
    Command::cargo_bin("rake")?
        .arg("basic")
        .assert()
        .success()
        .stderr(predicate::str::contains("locked").or(predicate::str::contains("enabled for")));
    Ok(())
}

#[test]
fn license_subcommand_rejects_malformed_key() -> TestResult {
    // A key with no '.' separator is malformed; the run must fail with exit 1.
    Command::cargo_bin("rake")?
        .arg("license")
        .arg("badkey")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("malformed"));
    Ok(())
}

#[test]
fn license_remove_no_license_or_no_terminal() -> TestResult {
    // When no license file is stored: success + "no license key stored".
    // When a file exists but stdin is not a TTY (always in tests): exit 1 + "not a terminal".
    let output = Command::cargo_bin("rake")?
        .arg("license")
        .arg("remove")
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let no_file = output.status.success() && stderr.contains("no license key stored");
    let no_tty = !output.status.success() && stderr.contains("not a terminal");
    assert!(
        no_file || no_tty,
        "unexpected remove_license output (status={}, stderr={stderr})",
        output.status
    );
    Ok(())
}

#[test]
fn license_info_prints_status() -> TestResult {
    // `license info` does not need a Rakefile — it exits before loading one.
    Command::cargo_bin("rake")?
        .args(["license", "info"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("no license active").or(predicate::str::contains("Features")),
        );
    Ok(())
}
