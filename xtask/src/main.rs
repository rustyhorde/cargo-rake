//! Workspace automation tasks, invoked as `cargo xtask <command>`.
//!
//! Currently provides `cargo xtask dist rake`, which generates the
//! distribution sidecar files bundled with the AUR / DEB / RPM / Homebrew
//! packages: the man page, shell completions, the license files, and an
//! example `Rakefile.toml`. Output lands in `dist/<bin>/` at the workspace
//! root.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use clap_complete::{Shell, generate_to};
use clap_mangen::Man;

/// Version stamped into the generated man page (the whole workspace shares one
/// version).
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("dist") => {
            let bin = args
                .next()
                .ok_or_else(|| anyhow!("usage: cargo xtask dist <bin>"))?;
            dist(&bin)
        }
        Some(other) => bail!("unknown xtask command: {other} (expected `dist`)"),
        None => bail!("usage: cargo xtask dist <bin>"),
    }
}

/// Generate the `dist/<bin>/` sidecar bundle for `bin`.
fn dist(bin: &str) -> Result<()> {
    if bin != "rake" {
        bail!("unknown dist target: {bin} (only `rake` is supported)");
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .ok_or_else(|| anyhow!("could not locate the workspace root"))?;
    let out = root.join("dist").join(bin);
    fs::create_dir_all(&out)?;

    // Man page → dist/rake/rake.1
    let mut man_buf = Vec::new();
    Man::new(librake::cli::command("rake", "rake").version(VERSION)).render(&mut man_buf)?;
    fs::write(out.join("rake.1"), man_buf)?;

    // Shell completions → rake.bash, _rake, rake.fish
    let mut cmd = librake::cli::command("rake", "rake").version(VERSION);
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        let _path = generate_to(shell, &mut cmd, "rake", &out)?;
    }

    // Sidecar files: licenses and an example Rakefile.
    copy(root.join("LICENSE-MIT"), out.join("LICENSE-MIT"))?;
    copy(root.join("LICENSE-APACHE"), out.join("LICENSE-APACHE"))?;
    copy(
        root.join("Rakefile.toml"),
        out.join("Rakefile.toml.example"),
    )?;

    println!("dist artifacts written to {}", out.display());
    Ok(())
}

/// `fs::copy` with the source path in the error context.
fn copy(from: PathBuf, to: impl AsRef<Path>) -> Result<()> {
    let _bytes = fs::copy(&from, to).map_err(|e| anyhow!("copying {}: {e}", from.display()))?;
    Ok(())
}
