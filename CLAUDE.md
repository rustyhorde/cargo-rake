# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A configuration-driven build tool that runs named targets declared in a `Rakefile.toml`, in dependency order. Each target owns an ordered array of named commands (`[[target.<t>.command]]`, each with a `name`, a `cmd` of program + args spawned directly — no shell, and an optional `skip_on_error` flag), an optional `depends_on` list, and an optional `tools` list. A target's commands run in array order; `skip_on_error` (default `false`) lets a command's non-zero exit be tolerated so the target's remaining commands and the dependency chain continue. The dependency graph is at the target level.

A target's `tools` names entries in a top-level `[tool.<name>]` table of external tools (cargo subcommands, etc.). Before a target's commands run, each referenced tool is ensured: its `check` command probes for presence (zero exit = installed; for a cargo subcommand use `["cargo", "<sub>", "--version"]`, since the bare `cargo-<sub>` binary rejects `--version`) and, if absent, its `install` command runs (printing an `Installing` notice). When a tool sets `update = true`, the installed version (parsed from `check`'s stdout) is compared against the latest from its `semver_check` mode (only `crates-io` today, the default, which queries the crates.io API and needs a `crate` name) and re-installed if newer; a failed version check — or an unparseable installed version — skips the update (warns) and keeps the current version. A failed `install` aborts the run. Each tool is ensured at most once per run. Every ensure announces a `Checking` line and prints an outcome (`Present`/`Up to date`/`Installing`/`Updating`), each printed in the same cargo-style status-line format as commands: a right-justified, bold-cyan label prefix in a shared column followed by the tool name and detail.

## Commands

```bash
cargo build                       # build the whole workspace
cargo test                        # run all tests
cargo test -p librake             # test a single crate
cargo test parses_targets         # run one test by name substring
cargo run -p rake -- --list       # run the standalone binary
cargo run -p cargo-rake -- rake --list   # run the cargo-subcommand binary
```

Most lints (the large `deny(...)` blocks, `clippy::pedantic`, rustdoc lints) are gated behind the `nightly` cfg, which `build.rs` sets only on a nightly toolchain. To surface them locally use a nightly toolchain, e.g. `cargo +nightly clippy --all-targets`. The `unstable` feature additionally enables nightly-only `feature(...)` gates; it is a no-op on stable.

## Workspace layout

Four crates, sharing dependency versions via `[workspace.dependencies]`:

- **`librake`** — all the real logic; the two binaries are thin shells over it.
  - `rakefile.rs` — `Rakefile`/`Target`/`Command` model (a target holds a `Vec<Command>`; each `Command` has `name`/`cmd`/`skip_on_error`), TOML parsing, validation (every target has ≥1 command **or** ≥1 dependency, each `cmd` non-empty), and execution (`Rakefile::run` runs a target after its transitive deps, returning a `RunReport` whose `status` is `Option<ExitStatus>` — `None` when no command ran, i.e. a depends-only target chain — plus the total wall-clock `elapsed`; `run_one` runs the target's commands in order, timing and printing each command's right-justified `Cmd Runtime`, and stops at the first non-zero exit unless that command sets `skip_on_error`, returning an `(Option<status>, stop)` pair; `spawn_command` prints a blank line then the command's cargo-style status line (`Running [ <name> ] <program args>`, the command name green via `GREEN` on a TTY, no `====` banners) and spawns it inheriting stdio). Output is cargo-style: every status line is a right-justified label prefix in one shared column followed by plain info; command/tool prefixes are bold cyan (matching cargo's status gutter), `Cmd Runtime`/`Runtime` bold green, and every line's info is prefixed with a `[ rake ]` tag (`RAKE_TAG`, color `BOLD_MAGENTA` to match nextest's package-name color) so rake's own output reads apart from the subprocesses it spawns. The column is a compile-time `LABEL_WIDTH` const (currently 12) — the wider of cargo's `CARGO_LABEL_WIDTH` (12, matching cargo's own status gutter) and the longest fixed `STATUS_LABELS` entry (`Running`/runtime labels/tool verbs), so a longer label still widens it automatically; command names live in the info after `Running`, so the width never depends on them. `print_label`/`print_justified` write these lines; `format_duration` renders a `Duration` with tiered µs/ms/s/min units (its integer part space-padded to `INT_WIDTH` = 4 so values line up on the decimal point) and `print_runtime(label, elapsed)` prints a right-justified, bold-green runtime line to stderr (used for the per-command `Cmd Runtime`), while `print_total_runtime(elapsed)` prints the binaries' final `Runtime` line with a bold-green label but a bold-yellow value (`BOLD_YELLOW`) to set the overall total apart. Note `std::process::Command` is imported as `ProcessCommand` to avoid clashing with the model `Command`.
  - `graph.rs` — dependency-graph logic over an `IndexMap<String, Target>`: `validate` (rejects unknown deps and cycles via DFS) and `execution_order` (post-order DFS; deps before dependents, declaration order as tie-break, root last).
  - `tool.rs` — the `[tool.<name>]` table: `Tool` (`crate`/`check`/`install`/`update`/`semver_check`) and the `SemverCheck` enum (only `CratesIo` today), `validate` (non-empty `check`/`install`, `crate` required when `update` under crates-io, every target `tools` reference resolves), and `ensure` (announce a `Checking` line; run `check` capturing output for presence + version; install missing tools inheriting stdio; print a `Present` outcome when present and not updating; `update_if_newer` re-installs (`Updating`) when `latest_version` reports a newer release or prints `Up to date` — an unparseable installed version or a version-check failure skips the update with a non-fatal warning rather than reinstalling). `eprint_tool` builds each tool status line's info (`{name}` plus any `{detail}`) and delegates to `rakefile::print_label` for the right-justified bold-cyan prefix. `latest_crate_version` queries crates.io via `ureq`; `parse_version_token` pulls a semver out of `check`'s stdout. The `print_label` helper is reused from `rakefile.rs` (made `pub(crate)`).
  - `error.rs` — `Error` enum (`thiserror`) and the crate `Result` alias. Tool failures add `UnknownTool`, `EmptyToolCommand`, `ToolUpdateMissingCrate`, `ToolInstallSpawn`, `ToolInstallFailed`.
  - `cli.rs` — the shared clap `Cli` derive (`file`/`list`/`targets`) and `command(name, bin_name)` helper (`pub mod cli`). Both binaries and `xtask` build their `clap::Command` from here so the CLI cannot drift.
  - `lib.rs` — public re-exports (incl. `Tool`/`SemverCheck`), the `pub mod cli`, `DEFAULT_TARGET` const, `exit_code`, `list_targets` (shows `tools:` lines), `format_duration`, `print_runtime`.
- **`cargo-rake`** — the `cargo rake` subcommand binary.
- **`rake`** — the standalone `rake` binary. Not published to crates.io (the `rake` name is taken by an unrelated crate); `publish = false`. Distributed via AUR/DEB/RPM/Homebrew instead.
- **`xtask`** — workspace automation (`publish = false`). `cargo xtask dist rake` generates the packaging sidecars (man page via `clap_mangen`, bash/zsh/fish completions via `clap_complete`, license files, example `Rakefile.toml`) into `dist/rake/`, reusing `librake::cli::command`.

`cargo-rake` and `rake` are near-identical clap front-ends, both parsing the shared `librake::cli::Cli` via `librake::cli::command(...)` (layering their own `version`/`long_version`). The only behavioral difference: `cargo-rake::main` strips a leading `rake` argument, because Cargo invokes subcommands as `cargo-rake rake ...`. Keep their `run` logic in sync when changing one; the argument definitions now live only in `librake::cli`.

Packaging/distribution lives in `packaging/` (AUR PKGBUILDs under `arch/`, nfpm DEB/RPM under `nfpm/`, Homebrew template under `homebrew/`) and the `release.yml` / `test-aur-publish.yml` workflows. `cargo-rake` ships via crates.io + `cargo binstall` (`[package.metadata.binstall]`); `rake` ships as four AUR packages (`rake`, `rake-unstable`, `rake-bin`, `rake-unstable-bin`), DEB/RPM, and Homebrew. The `-unstable` variants build with `--features unstable` on the stable toolchain — that feature is a functional no-op (it only toggles nightly-only lint gates), so no nightly toolchain is involved.

## Conventions

- **No `unwrap`/`expect` and no panicking operations anywhere, including tests.** Use `match`/`ok_or`/`?` and return `Result` instead. Tests use `Result<(), Box<dyn Error>>` and convert failures with `.into()`/`Err(...)`. This is enforced by intent, not just lints — see the deliberately-spelled-out `exit_code` in `lib.rs`.
- A command that *runs but exits non-zero* is not an `Error`; its `ExitStatus` propagates so the caller chooses the process exit code (`exit_code` maps a signal-killed child to `1`). Reserve `Error` for load/parse/validation/spawn failures.
- Target order is significant and preserved with `IndexMap` (`toml` uses `preserve_order`); don't swap in a `HashMap`.
- Each crate's `build.rs` uses `vergen-gix` to emit build/git/rustc/system info, rendered by `vergen-pretty` as the binaries' `--version` long output.
- Edition 2024, MSRV 1.95.0.
