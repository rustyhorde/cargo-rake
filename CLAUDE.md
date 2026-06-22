# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A configuration-driven build tool that runs named targets declared in a `Rakefile.toml`, in dependency order. Targets have a `cmd` (program + args, spawned directly — no shell), an optional `depends_on` list, and an optional `skip_on_error` flag (default `false`) that lets a non-zero exit be tolerated so the dependency chain continues.

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

Three crates, sharing dependency versions via `[workspace.dependencies]`:

- **`librake`** — all the real logic; the two binaries are thin shells over it.
  - `rakefile.rs` — `Rakefile`/`Target` model (incl. the optional `skip_on_error` flag), TOML parsing, validation, and execution (`Rakefile::run` runs a target after its transitive deps and stops at the first non-zero exit unless that target sets `skip_on_error`; `run_one` spawns each command inheriting stdio).
  - `graph.rs` — dependency-graph logic over an `IndexMap<String, Target>`: `validate` (rejects unknown deps and cycles via DFS) and `execution_order` (post-order DFS; deps before dependents, declaration order as tie-break, root last).
  - `error.rs` — `Error` enum (`thiserror`) and the crate `Result` alias.
  - `lib.rs` — public re-exports, `DEFAULT_TARGET` const, `exit_code`, `list_targets`.
- **`cargo-rake`** — the `cargo rake` subcommand binary.
- **`rake`** — the standalone `rake` binary.

`cargo-rake` and `rake` are near-identical clap front-ends. The only behavioral difference: `cargo-rake::main` strips a leading `rake` argument, because Cargo invokes subcommands as `cargo-rake rake ...`. Keep their CLI definitions and `run` logic in sync when changing one.

## Conventions

- **No `unwrap`/`expect` and no panicking operations anywhere, including tests.** Use `match`/`ok_or`/`?` and return `Result` instead. Tests use `Result<(), Box<dyn Error>>` and convert failures with `.into()`/`Err(...)`. This is enforced by intent, not just lints — see the deliberately-spelled-out `exit_code` in `lib.rs`.
- A command that *runs but exits non-zero* is not an `Error`; its `ExitStatus` propagates so the caller chooses the process exit code (`exit_code` maps a signal-killed child to `1`). Reserve `Error` for load/parse/validation/spawn failures.
- Target order is significant and preserved with `IndexMap` (`toml` uses `preserve_order`); don't swap in a `HashMap`.
- Each crate's `build.rs` uses `vergen-gix` to emit build/git/rustc/system info, rendered by `vergen-pretty` as the binaries' `--version` long output.
- Edition 2024, MSRV 1.91.1.
