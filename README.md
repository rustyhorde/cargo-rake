# cargo-rake

A configuration-driven build tool that runs named targets declared in a
`Rakefile.toml`, in dependency order. It ships as both a `cargo` subcommand
(`cargo rake`) and a standalone `rake` binary.

## Installation

The two binaries are distributed through different channels:

- **`cargo-rake`** (the `cargo rake` subcommand) — through cargo.
- **`rake`** (the standalone binary) — through system package managers.

### cargo (`cargo-rake`)

```bash
cargo binstall cargo-rake   # download a pre-built binary, no compile
cargo install cargo-rake    # build and install from crates.io
```

### Arch Linux (`rake`)

Available from the AUR in four mutually-exclusive flavors — pre-built or
built-from-source, each in a stable and a nightly `unstable`-feature variant:

```bash
paru -S rake-bin            # pre-built binary (recommended)
paru -S rake-unstable-bin   # pre-built, nightly unstable build
paru -S rake                # build from source
paru -S rake-unstable       # build from source, nightly unstable build
```

### Debian/Ubuntu (`rake`)

#### From the apt repository (recommended)

The signed apt repository at <https://rustyhorde.github.io/cargo-rake-packages/>
tracks every release, so `apt upgrade` keeps `rake` current. Packages are
available for `amd64` and `arm64`:

```bash
# Add the repository signing key
sudo install -d /etc/apt/keyrings
curl -fsSL https://rustyhorde.github.io/cargo-rake-packages/gpg.key \
    | sudo gpg --dearmor -o /etc/apt/keyrings/rake.gpg

# Add the apt source
echo "deb [arch=amd64,arm64 signed-by=/etc/apt/keyrings/rake.gpg] \
  https://rustyhorde.github.io/cargo-rake-packages/apt stable main" \
    | sudo tee /etc/apt/sources.list.d/rake.list

# Install
sudo apt update
sudo apt install rake
```

The same repository also carries the `rake-unstable` build (the `unstable`
feature is a functional no-op — it only toggles nightly-only lint gates).
`rake-unstable` conflicts with `rake`, so only one can be installed at a time.

#### From a downloaded `.deb`

Pre-built `.deb` packages are also attached to each
[GitHub release](https://github.com/rustyhorde/cargo-rake/releases) if you prefer
not to add the repository. `dpkg -i` runs as root and works from any location:

```bash
sudo dpkg -i rake_*_amd64.deb
```

### Fedora/RHEL (`rake`)

#### From the dnf repository (recommended)

Pre-built `.rpm` packages for `x86_64` and `aarch64` are served from the signed
dnf repository at <https://rustyhorde.github.io/cargo-rake-packages/>, so
`dnf upgrade` keeps `rake` current:

```bash
# Add the repository (imports the signing key on first install)
sudo dnf config-manager \
    --add-repo https://rustyhorde.github.io/cargo-rake-packages/rpm/rake.repo

# Install
sudo dnf install rake
```

> On older releases the subcommand is `sudo dnf config-manager addrepo
> --from-repofile=…`, and on dnf 4 you may need `sudo dnf install
> dnf-plugins-core` first.

The `rake-unstable` build is available from the same repository by name; it
conflicts with `rake`, so only one can be installed at a time.

#### From a downloaded `.rpm`

`.rpm` files are also attached to each
[GitHub release](https://github.com/rustyhorde/cargo-rake/releases) for direct
installation:

```bash
sudo dnf install ./rake-*.x86_64.rpm   # or: sudo rpm -i rake-*.x86_64.rpm
```

### macOS (`rake`)

Pre-compiled binaries for **Apple Silicon (aarch64)** are published to the
[`rustyhorde/rake`](https://github.com/rustyhorde/homebrew-rake) Homebrew tap.
Intel Macs are not currently supported.

```bash
brew tap rustyhorde/rake
brew install rake
```

## Rakefile.toml

A `Rakefile.toml` declares named **targets**. Each target owns an ordered array
of named **commands** plus an optional `depends_on` list:

```toml
# Optional: the Rust toolchain this project targets. When set, both `rake` and
# `cargo rake` verify/install the channel (via rustup) and pin the run to it.
# Omit it to use whatever toolchain is already active.
# toolchain = "stable"            # default: unset (active toolchain)

[target.build]

[[target.build.command]]
name = "compile"                  # label shown in --list and errors (required)
cmd  = ["cargo", "build", "--all-features"]   # program + args, spawned directly (required)
# skip_on_error = false           # default: a non-zero exit aborts the target

[target.all]
depends_on = ["build"]            # default: [] — run these targets first, in order
# tools     = []                  # default: [] — external tools to ensure (see below)

[[target.all.command]]
name = "release"
cmd  = ["cargo", "build", "--release"]

[[target.all.command]]
name = "test"
cmd  = ["cargo", "test"]
skip_on_error = true              # tolerate a failure and keep going
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

A target can declare external tools it depends on. Tools are defined once in a
top-level `[tool]` table, split into two categories — **`[tool.cargo.<name>]`**
for cargo-installable tools (cargo subcommands and the like) and
**`[tool.os.<name>]`** for OS-level dependencies (`docker`, `pkg-config`, …)
that `rake` cannot `cargo install`. A target references either kind by name from
its `tools` list (the two categories share one flat reference namespace, so a
name must be unique across them):

```toml
[tool.cargo.matrix]
crate   = "cargo-matrix"                       # crates.io name (for the update check)
check   = ["cargo-matrix", "--version"]        # presence probe (zero exit = installed)
install = ["cargo", "install", "cargo-matrix"] # run when missing or out of date
update  = false                                # default; see below

[tool.os.docker]
check   = ["docker", "--version"]              # presence probe (zero exit = installed)
hint    = "install Docker: https://docs.docker.com/get-docker/"  # shown if absent

[target.clippy]
tools = ["matrix"]

[[target.clippy.command]]
name = "clippy"
cmd  = ["cargo", "matrix", "clippy", "--all-targets", "--", "-Dwarnings"]
```

Tools are ensured lazily: a tool's `check` only runs when a target that
references it is actually run (never at parse time or for unrelated targets), and
at most once per run. Before a target's commands run, each tool it references is
ensured:

- The **`check`** command probes whether the tool is already installed: it must
  **exit 0 when the tool is present**. For a cargo subcommand this means
  `["cargo", "<sub>", "--version"]` — the bare `cargo-<sub>` binary rejects
  `--version` and exits non-zero, which would make the tool look perpetually
  absent.

**Cargo tools** (`[tool.cargo.<name>]`):

- When absent, `rake` prints an `Installing` notice and runs **`install`** (both
  `check` and `install` are required).
- **`update`** (default `false`): when `true`, the installed version (parsed
  from `check`'s output) is compared against the latest reported by the tool's
  **`semver_check`** mode and re-installed if a newer one exists. The only mode
  today is `"crates-io"` (the default), which queries the crates.io API and so
  needs a **`crate`** name; a failed version check — or a `check` whose output
  has no parseable version — is non-fatal and keeps the installed version. With
  `update = false`, the already-installed version is used as-is.
- A failed `install` aborts the run (unlike a tolerated command failure).

**OS tools** (`[tool.os.<name>]`): only `check` is required; there is no update
support.

- When absent and an **`install`** is declared, `rake` runs it (like a cargo
  tool). When absent and no `install` is declared, the run **aborts** with a
  message stating the requirement, plus any **`hint`** — `rake` does not try to
  install OS dependencies itself.

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
