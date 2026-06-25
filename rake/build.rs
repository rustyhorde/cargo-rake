// Copyright (c) 2025 barto developers
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use anyhow::Result;
use vergen_gix::{Build, Cargo, Emitter, Gix, Rustc, Sysinfo};

pub fn main() -> Result<()> {
    println!("cargo:rustc-check-cfg=cfg(coverage_nightly)");
    nightly();
    let gix = Gix::builder()
        .maybe_dirty(None)
        .branch(true)
        .commit_author_email(true)
        .commit_author_name(true)
        .commit_timestamp(true)
        .describe(true, false, None)
        .sha(false)
        .build();
    Emitter::default()
        .add_instructions(&Build::all().build_date(false).build())?
        .add_instructions(&Cargo::all_cargo())?
        .add_instructions(&gix)?
        .add_instructions(&Rustc::all().llvm_version(false).build())?
        .add_instructions(&Sysinfo::builder().name(true).os_version(true).build())?
        .emit()
}

#[rustversion::nightly]
fn nightly() {
    println!("cargo:rustc-check-cfg=cfg(nightly)");
    println!("cargo:rustc-cfg=nightly");
}

#[rustversion::not(nightly)]
fn nightly() {
    println!("cargo:rustc-check-cfg=cfg(nightly)");
}
