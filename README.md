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

## Self-Update

`cargo-rake` (the `cargo rake` subcommand) checks crates.io for a newer version of itself on
every startup and installs it automatically via `cargo install cargo-rake` if one is found. After
a successful update, the binary relaunches itself with the original arguments so the newly
installed version handles the actual work.

`rake` (the standalone binary) is distributed through system package managers and does not
self-update via this mechanism — use your package manager to update it.

**Opt out** per-project by adding `update = false` to your `Rakefile.toml`:

```toml
update = false
```

When the key is absent the check is **enabled** (default `true`). The check is skipped entirely
in `--dry-run` mode. Network and version-check failures are non-fatal: a `Warning` line is
printed and the run continues normally.

## Rakefile.toml

A `Rakefile.toml` declares named **targets**. Each target owns an ordered array
of named **commands**, an optional `depends_on` list, and an optional `tools`
list. Tools are defined once in a top-level `[tool]` table. A complete example:

```toml
# toolchain = "stable"   # optional; pins both binaries to a specific Rust channel via rustup
# update    = false      # optional; set false to disable cargo-rake's self-update check

# Cargo-installable tools (cargo subcommands, etc.)
[tool.cargo.nextest]
crate   = "cargo-nextest"                         # crates.io name (required for update checks)
check   = ["cargo", "nextest", "--version"]       # probe: zero exit = installed
install = ["cargo", "install", "cargo-nextest", "--force", "--locked"]
update  = true                                    # re-install when a newer crates.io version exists

# OS-level tools that rake cannot install (docker, pkg-config, …)
[tool.os.docker]
check = ["docker", "--version"]
hint  = "install Docker: https://docs.docker.com/get-docker/"  # shown when absent and no install

# Fish shell function dependencies — probed with `fish -c "functions --query <name>"`
[tool.fish.puc]
hint = "define puc in ~/.config/fish/functions/puc.fish"

# ── Targets ──────────────────────────────────────────────────────────────────

[[target.build.command]]
name = "compile"           # label shown in `list` output and error messages (required)
cmd  = ["cargo", "build"]  # program + args, spawned directly — no shell involved

[target.test]
depends_on = ["build"]     # run these targets first, in order
tools      = ["nextest"]   # ensure these tools before the target's commands run

[[target.test.command]]
name = "run"
cmd  = ["cargo", "nextest", "run"]

[[target.check.command]]
name          = "fmt"
cmd           = ["cargo", "fmt", "--check"]
skip_on_error = true       # tolerate a non-zero exit and keep going

[target.package]
tools = ["docker"]

[[target.package.command]]
name     = "archive"
platform = ["linux", "macos"]               # skipped silently on other platforms
sh       = "tar czf dist.tgz \"$(pwd)\"/target/release/*"
fish     = "tar czf dist.tgz (pwd)/target/release/*"

[[target.package.command]]
name     = "zip"
platform = ["windows"]
ps       = "Compress-Archive target/release/* dist.zip"

[target.puc]
tools = ["puc"]

[[target.puc.command]]
name = "puc"
fish = "puc"

# macOS-specific variant — active only on macOS.  A base [target.notarize]
# is defined below as a cross-platform no-op so that `all` can depend on it
# without TargetNotAvailableOnPlatform on other hosts.
[target.notarize.macos]

[[target.notarize.macos.command]]
name = "submit"
cmd  = ["xcrun", "notarytool", "submit", "dist.pkg"]

[target.notarize]            # base: no-op on non-macOS hosts
[[target.notarize.command]]
name = "skip"
sh   = "true"
ps   = "exit 0"

[target.all]
depends_on = ["build", "test", "check", "package", "notarize"]

[target.default]
depends_on = ["test"]
```

- A command sets **one kind** of body: either `cmd`, or one or more shell
  variants (`sh` / `fish` / `ps`). `cmd` is mutually exclusive with the shell
  variants; the shell variants may coexist.
  - **`cmd`** is a program followed by its arguments, spawned directly — no
    shell is involved — so it behaves the same on every platform.
  - **`sh` / `fish` / `ps`** are each a single command line run through that
    shell, so shell features (`$(...)` substitution, `~`/`$VAR` expansion, globs,
    pipes) apply: `sh -c`, `fish -c`, and PowerShell `-Command` (`pwsh` if on
    `PATH`, else `powershell`) respectively. rake resolves the current shell in
    priority order: (1) a PowerShell environment variable
    (`POWERSHELL_DISTRIBUTION_CHANNEL` or `PSModulePath` on non-Windows) —
    checked first because PowerShell does not set `$SHELL`; (2) `$SHELL`'s
    basename; (3) the platform default (`ps` on Windows, Posix otherwise).
    Selection is **strict**: if the detected shell has no matching variant, the
    run aborts with an error — so define a variant for every shell a command must
    run under. A `platform`/`arch` mismatch, by contrast, is a silent skip, not
    an error.
- **`[[target.<t>.command]]`** is a TOML array of tables. Each entry needs a
  `name` (a label used in `list` output and error messages) and a body (`cmd`
  or one or more of `sh`/`fish`/`ps`). Commands run in **array (declaration)
  order**. (TOML table headers are absolute, so the `target.<t>.command` prefix
  is required on each entry.)
- **`skip_on_error`** (per command, default `false`): when `true`, a non-zero
  exit from that command is tolerated and the target continues with its
  remaining commands instead of aborting.
- **`depends_on`** lists other targets that must run, in order, before this one. Prefix an
  entry with `^` (e.g. `depends_on = ["all", "^install"]`) to embed a skip for that
  dependency — the skipped target (and any dep reachable only through it) is pruned
  whenever this target is in the run. This complements the CLI `^target` syntax.
- **`tools`** — tools can be declared at **two levels**: on `[target.<name>]`
  (ensured before every command in that target) or on
  `[[target.<t>.command]]` (ensured immediately before that specific command,
  and only when it is not platform/arch-skipped). A name declared at both
  levels for the same target is a parse-time error. See [Tools](#tools).
- **Platform-specific targets** — add a platform token as a sub-table key to
  declare a platform-specific variant of a target (e.g. `[target.sign.macos]`,
  `[target.sign.linux]`). The most specific match wins: an OS token (e.g.
  `linux`) beats a family token (e.g. `unix`) when both variants exist for the
  current host. If no variant matches and no base `[target.sign]` exists, any
  target that `depends_on` it fails with `TargetNotAvailableOnPlatform` — add a
  base variant as a cross-platform fallback (or no-op) when needed. The
  `notarize` target above shows this pattern.

  Valid **OS tokens** (match `std::env::consts::OS`):
  `linux`, `macos`, `windows`, `freebsd`, `netbsd`, `openbsd`, `dragonfly`,
  `solaris`, `illumos`, `android`, `ios`, `tvos`, `watchos`, `visionos`,
  `fuchsia`, `redox`, `haiku`, `hurd`, `aix`, `nto`, `emscripten`, `wasi`

  Valid **family tokens** (match `std::env::consts::FAMILY`): `unix`, `windows`, `wasm`

  An unknown token, or any key other than `depends_on`/`tools`/`command` and the
  platform tokens above, is a hard parse-time error.
- **`platform`** (optional list, per command) names OS or family tokens
  (`linux`/`macos`/`windows`/`unix`/…). **`arch`** (optional list, per command
  only) names architecture tokens (`x86_64`/`aarch64`/…). Both lists are
  validated at parse time; an empty list or an unknown token is a hard error. A
  command runs only when every declared dimension matches (AND across
  `platform`/`arch`, OR within each list). A non-matching command is **silently
  skipped** — a `Skipped` status line is printed and the target's remaining
  commands continue.
- **Skipping targets**: prefix a target name with `^` to exclude it from the run
  (e.g. `rake all ^clean`). The skipped target and any dependency reachable only
  through it are pruned. A skip is not allowed if another non-root target that
  still runs `depends_on` the skipped target — the run fails fast. With only
  skip tokens (e.g. `rake ^clean`), the `default` target runs.

### Tools

Tools are defined in a top-level `[tool]` table, split into three categories:
**`[tool.cargo.<name>]`** for cargo-installable tools (cargo subcommands and
the like), **`[tool.os.<name>]`** for OS-level dependencies (`docker`,
`pkg-config`, …) that `rake` cannot `cargo install`, and **`[tool.fish.<name>]`**
for fish shell function dependencies. The three categories share one flat
reference namespace, so a name must be unique across all of them.

Tools can be referenced at **two levels**:

- **Target-level** (`tools = [...]` on `[target.<name>]`): the named tools are
  ensured before any of the target's commands run.
- **Command-level** (`tools = [...]` on `[[target.<t>.command]]`): the named
  tools are ensured immediately before *that specific command* runs — and only
  when the command is not already skipped by its `platform`/`arch` gates. This
  lets platform-specific commands declare their own tool dependencies without
  requiring the same tools on every platform.

A tool name may not appear in both a target's `tools` list and a command's
`tools` list within the same target — that is a parse-time error. Use
target-level `tools` when a dependency is shared by all commands in the target,
and command-level `tools` when only a specific (often platform-gated) command
needs it:

```toml
[tool.os.gpg]
check = ["gpg", "--version"]

[tool.os.xcrun]
check = ["xcrun", "--version"]

[target.sign]
# No target-level tools here — each platform needs a different signing tool.

[[target.sign.command]]
name     = "gpg-sign"
platform = ["linux"]
sh       = "gpg --detach-sign dist.tgz"
tools    = ["gpg"]                  # only ensured on linux

[[target.sign.command]]
name     = "notarize"
platform = ["macos"]
cmd      = ["xcrun", "notarytool", "submit", "dist.pkg"]
tools    = ["xcrun"]                # only ensured on macOS
```

Tools are ensured lazily: a tool's `check` only runs when a target (or command)
that references it is actually run (never at parse time or for unrelated targets),
and at most once per run across both levels. Before a target's commands run, each
tool it references is ensured:

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

**Fish tools** (`[tool.fish.<name>]`): the TOML key is the fish function name to
check. No `check` or `install` fields are needed — `rake` always probes with
`fish -c "functions --query <name>"`, covering user-defined, autoloaded, and
builtin functions. When absent the run aborts with the requirement and any
**`hint`**. Fish tools have no update support.

### Execution

A target runs after its transitive dependencies. Within a target, commands run
in array order and execution **stops at the first command that exits non-zero**,
aborting the dependency chain — unless that command sets `skip_on_error = true`,
in which case the failure is tolerated and execution continues. The process
exits with the code of the command that stopped the run (or the last command if
all succeeded).

When multiple roots are given (e.g. `rake build test`), each root's dependency
graph runs in full in the order given. A target shared by two roots runs once per
root (no cross-root deduplication). Tools, however, are ensured at most once for
the whole run regardless of how many roots reference them.

## Usage

```bash
cargo rake <target>              # run a target (and its dependencies)
cargo rake all ^clean            # skip 'clean' (and any dep reachable only through it)
cargo rake --dry-run <target>    # preview what would run without executing anything
cargo rake list                  # list all targets and their commands
cargo rake syntax                # parse + validate the Rakefile, reporting any errors
cargo rake --file path/to/Rakefile.toml <target>

rake <target>                    # the standalone binary, same interface
```

`list` and `syntax` are reserved subcommand names: a target sharing one of
those names cannot be run by name (run it via a parent target instead).

When no target is named, the `default` target runs.
