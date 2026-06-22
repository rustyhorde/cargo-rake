# cargo-rake

A configuration-driven build tool that runs named targets declared in a
`Rakefile.toml`, in dependency order. It ships as both a `cargo` subcommand
(`cargo rake`) and a standalone `rake` binary.

## Rakefile.toml

A `Rakefile.toml` declares named **targets**. Each target owns an ordered array
of named **commands** plus an optional `depends_on` list:

```toml
[target.build]

[[target.build.command]]
name = "compile"
cmd = ["cargo", "build", "--all-features"]

[target.all]
depends_on = ["build"]

[[target.all.command]]
name = "release"
cmd = ["cargo", "build", "--release"]

[[target.all.command]]
name = "test"
cmd = ["cargo", "test"]
skip_on_error = true
```

- **`cmd`** is a program followed by its arguments. It is spawned directly — no
  shell is involved — so it behaves the same on every platform.
- **`[[target.<t>.command]]`** is a TOML array of tables. Each entry needs a
  `name` (a label used in `--list` output and error messages) and a `cmd`.
  Commands run in **array (declaration) order**. (TOML table headers are
  absolute, so the `target.<t>.command` prefix is required on each entry.)
- **`skip_on_error`** (per command, default `false`): when `true`, a non-zero
  exit from that command is tolerated and the target continues with its
  remaining commands instead of aborting.
- **`depends_on`** lists other targets that must run, in order, before this one.
- **`tools`** lists external tools (by name) the target needs; see below.

### Tools

A target can declare external tools (cargo subcommands and the like) it depends
on. Tools are defined once in a top-level `[tool.<name>]` table and referenced
by name from a target's `tools` list:

```toml
[tool.matrix]
crate   = "cargo-matrix"                       # crates.io name (for the update check)
check   = ["cargo-matrix", "--version"]        # presence probe (zero exit = installed)
install = ["cargo", "install", "cargo-matrix"] # run when missing or out of date
update  = false                                # default; see below

[target.clippy]
tools = ["matrix"]

[[target.clippy.command]]
name = "clippy"
cmd  = ["cargo", "matrix", "clippy", "--all-targets", "--", "-Dwarnings"]
```

Before a target's commands run, each tool it references is ensured:

- The **`check`** command probes whether the tool is already installed: it must
  **exit 0 when the tool is present**. For a cargo subcommand this means
  `["cargo", "<sub>", "--version"]` — the bare `cargo-<sub>` binary rejects
  `--version` and exits non-zero, which would make the tool look perpetually
  absent. When absent, `rake` prints an `Installing` notice and runs
  **`install`**.
- **`update`** (default `false`): when `true`, the installed version (parsed
  from `check`'s output) is compared against the latest reported by the tool's
  **`semver_check`** mode and re-installed if a newer one exists. The only mode
  today is `"crates-io"` (the default), which queries the crates.io API and so
  needs a **`crate`** name; a failed version check — or a `check` whose output
  has no parseable version — is non-fatal and keeps the installed version. With
  `update = false`, the already-installed version is used as-is.
- A failed `install` aborts the run (unlike a tolerated command failure).

Each tool is ensured at most once per run, even when several targets reference
it.

### Execution

A target runs after its transitive dependencies. Within a target, commands run
in array order and execution **stops at the first command that exits non-zero**,
aborting the dependency chain — unless that command sets `skip_on_error = true`,
in which case the failure is tolerated and execution continues. The process
exits with the code of the command that stopped the run (or the last command if
all succeeded).

## Usage

```bash
cargo rake <target>        # run a target (and its dependencies)
cargo rake --list          # list all targets and their commands
cargo rake --file path/to/Rakefile.toml <target>

rake <target>              # the standalone binary, same interface
```

When no target is named, the `default` target runs.
