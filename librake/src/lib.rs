//! Core library for `cargo-rake` / `rake`: a configuration-driven build tool.
//!
//! A `Rakefile.toml` declares named targets, each with an ordered array of
//! named commands, an optional `depends_on` list, and an optional `tools` list.
//! A command body is either a `cmd` (program + args, spawned directly) or one
//! or more shell variants (`sh`/`fish`/`ps`); the current shell is
//! auto-detected at run time. Prefix a `depends_on` entry with `^` to embed a
//! skip for that dependency (equivalent to the CLI `^target` syntax). Tools
//! live in a top-level `[tool]` table split into three categories:
//! `[tool.cargo.<name>]`, `[tool.os.<name>]`, and `[tool.fish.<name>]` (see
//! [`ToolTable`]). Targets are parsed and validated by [`Rakefile`] and run in
//! dependency order via [`Rakefile::run`] (or previewed without execution via
//! [`Rakefile::run_dry`]); commands run in array order after referenced tools
//! have been ensured. The optional top-level `update` key (default `true`)
//! controls whether `cargo-rake` checks crates.io for a newer version of itself
//! on startup and installs it automatically; see [`ensure_self_update`] and
//! [`Rakefile::update`]. An optional top-level `[lifecycle]` table (an
//! `address` of the form `host:port`) enables before/after lifecycle events
//! — sent fire-and-forget as JSON over a loopback UDP socket for the whole
//! run, each target, each command, and each tool check/install/update — but
//! only when the run is also licensed for the `events` feature (see
//! [`Rakefile::run_licensed`]); absent or unlicensed is a quiet no-op.

//! librake

// rustc lints
#![cfg_attr(
    all(feature = "unstable", nightly),
    feature(
        multiple_supertrait_upcastable,
        must_not_suspend,
        non_exhaustive_omitted_patterns_lint,
        strict_provenance_lints,
        unqualified_local_imports,
    )
)]
#![cfg_attr(nightly, allow(single_use_lifetimes))]
#![cfg_attr(
    nightly,
    deny(
        aarch64_softfloat_neon,
        absolute_paths_not_starting_with_crate,
        ambiguous_derive_helpers,
        ambiguous_glob_imported_traits,
        ambiguous_glob_reexports,
        ambiguous_import_visibilities,
        ambiguous_negative_literals,
        ambiguous_panic_imports,
        ambiguous_wide_pointer_comparisons,
        anonymous_parameters,
        array_into_iter,
        asm_sub_register,
        async_fn_in_trait,
        bad_asm_style,
        bare_trait_objects,
        boxed_slice_into_iter,
        break_with_label_and_loop,
        clashing_extern_declarations,
        closure_returning_async_block,
        coherence_leak_check,
        confusable_idents,
        const_evaluatable_unchecked,
        const_item_interior_mutations,
        const_item_mutation,
        dangling_pointers_from_locals,
        dangling_pointers_from_temporaries,
        dead_code,
        deprecated,
        deprecated_in_future,
        deprecated_safe_2024,
        deprecated_where_clause_location,
        deref_into_dyn_supertrait,
        double_negations,
        drop_bounds,
        dropping_copy_types,
        dropping_references,
        duplicate_macro_attributes,
        dyn_drop,
        edition_2024_expr_fragment_specifier,
        elided_lifetimes_in_paths,
        ellipsis_inclusive_range_patterns,
        explicit_outlives_requirements,
        exported_private_dependencies,
        ffi_unwind_calls,
        float_literal_f32_fallback,
        forbidden_lint_groups,
        forgetting_copy_types,
        forgetting_references,
        for_loops_over_fallibles,
        function_casts_as_integer,
        function_item_references,
        hidden_glob_reexports,
        if_let_rescope,
        impl_trait_overcaptures,
        impl_trait_redundant_captures,
        improper_ctypes,
        improper_ctypes_definitions,
        improper_gpu_kernel_arg,
        inline_no_sanitize,
        integer_to_ptr_transmutes,
        internal_eq_trait_method_impls,
        internal_features,
        invalid_doc_attributes,
        invalid_from_utf8,
        invalid_nan_comparisons,
        invalid_value,
        irrefutable_let_patterns,
        keyword_idents_2018,
        keyword_idents_2024,
        large_assignments,
        late_bound_lifetime_arguments,
        let_underscore_drop,
        macro_use_extern_crate,
        malformed_diagnostic_attributes,
        malformed_diagnostic_format_literals,
        map_unit_fn,
        meta_variable_misuse,
        mismatched_lifetime_syntaxes,
        misplaced_diagnostic_attributes,
        missing_abi,
        missing_copy_implementations,
        missing_debug_implementations,
        missing_docs,
        missing_gpu_kernel_export_name,
        missing_unsafe_on_extern,
        mixed_script_confusables,
        named_arguments_used_positionally,
        no_mangle_generic_items,
        non_ascii_idents,
        non_camel_case_types,
        non_contiguous_range_endpoints,
        non_fmt_panics,
        non_local_definitions,
        non_shorthand_field_patterns,
        non_snake_case,
        non_upper_case_globals,
        noop_method_call,
        opaque_hidden_inferred_bound,
        overlapping_range_endpoints,
        path_statements,
        private_bounds,
        private_interfaces,
        ptr_to_integer_transmute_in_consts,
        redundant_imports,
        redundant_lifetimes,
        redundant_semicolons,
        refining_impl_trait_internal,
        refining_impl_trait_reachable,
        renamed_and_removed_lints,
        repr_c_enums_larger_than_int,
        rtsan_nonblocking_async,
        rust_2021_incompatible_closure_captures,
        rust_2021_incompatible_or_patterns,
        rust_2021_prefixes_incompatible_syntax,
        rust_2021_prelude_collisions,
        rust_2024_guarded_string_incompatible_syntax,
        rust_2024_incompatible_pat,
        rust_2024_prelude_collisions,
        self_constructor_from_outer_item,
        single_use_lifetimes,
        special_module_name,
        stable_features,
        static_mut_refs,
        suspicious_double_ref_op,
        tail_expr_drop_order,
        trivial_bounds,
        trivial_casts,
        trivial_numeric_casts,
        type_alias_bounds,
        tyvar_behind_raw_pointer,
        uncommon_codepoints,
        unconditional_recursion,
        uncovered_param_in_projection,
        unexpected_cfgs,
        unfulfilled_lint_expectations,
        ungated_async_fn_track_caller,
        unit_bindings,
        unknown_diagnostic_attributes,
        unnameable_test_items,
        unnameable_types,
        unnecessary_transmutes,
        unpredictable_function_pointer_comparisons,
        unreachable_cfg_select_predicates,
        unreachable_code,
        unreachable_patterns,
        unreachable_pub,
        unsafe_attr_outside_unsafe,
        unsafe_code,
        unsafe_op_in_unsafe_fn,
        unstable_name_collisions,
        unstable_syntax_pre_expansion,
        unsupported_calling_conventions,
        unused_allocation,
        unused_assignments,
        unused_associated_type_bounds,
        unused_attributes,
        unused_braces,
        unused_comparisons,
        unused_crate_dependencies,
        unused_doc_comments,
        unused_extern_crates,
        unused_features,
        unused_import_braces,
        unused_imports,
        unused_labels,
        unused_lifetimes,
        unused_macro_rules,
        unused_macros,
        unused_must_use,
        unused_mut,
        unused_parens,
        unused_qualifications,
        unused_results,
        unused_unsafe,
        unused_variables,
        unused_visibilities,
        useless_ptr_null_checks,
        uses_power_alignment,
        variant_size_differences,
        while_true,
    )
)]
// If nightly and unstable, allow `incomplete_features` and `unstable_features`
#![cfg_attr(
    all(feature = "unstable", nightly),
    allow(incomplete_features, unstable_features)
)]
// If nightly and not unstable, deny `incomplete_features` and `unstable_features`
#![cfg_attr(
    all(not(feature = "unstable"), nightly),
    deny(incomplete_features, unstable_features)
)]
// The unstable lints
#![cfg_attr(
    all(feature = "unstable", nightly),
    deny(
        implicit_provenance_casts,
        multiple_supertrait_upcastable,
        must_not_suspend,
        non_exhaustive_omitted_patterns,
        unqualified_local_imports,
    )
)]
// clippy lints
#![cfg_attr(nightly, deny(clippy::all, clippy::pedantic))]
// rustdoc lints
#![cfg_attr(
    nightly,
    deny(
        rustdoc::bare_urls,
        rustdoc::broken_intra_doc_links,
        rustdoc::invalid_codeblock_attributes,
        rustdoc::invalid_html_tags,
        rustdoc::invalid_rust_codeblocks,
        rustdoc::missing_crate_level_docs,
        rustdoc::private_doc_tests,
        rustdoc::private_intra_doc_links,
        rustdoc::redundant_explicit_links,
        rustdoc::unescaped_backticks,
    )
)]
#![cfg_attr(all(docsrs), feature(doc_cfg))]
// #![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod cli;
mod error;
mod graph;
mod license;
mod lifecycle;
mod rakefile;
mod tool;
mod toolchain;

use std::fmt::Write as _;
use std::process::ExitStatus;

pub use crate::{
    error::{Error, Result},
    license::{
        Features, LicensePayload, activate_license, basic_feature_status, license_info_status,
        load_license, remove_license,
    },
    lifecycle::{LifecycleEvent, ProjectInfo, ToolOutcome},
    rakefile::{
        Command, Host, Rakefile, RunReport, ShellFamily, Target, detect_shell_family,
        format_duration, print_runtime, print_total_runtime,
    },
    tool::{
        CHECK_TAG, CargoTool, FishTool, OsTool, SemverCheck, ToolTable, UpdateRecord,
        ensure_self_update, print_update_summary,
    },
    toolchain::ensure_rust_toolchain,
};

/// The target run when none is named on the command line.
pub const DEFAULT_TARGET: &str = "default";

/// Derive a process exit code from a finished command's [`ExitStatus`].
///
/// Falls back to `1` when the child was terminated by a signal and so has no
/// exit code of its own.
#[must_use]
// Deliberately spelled out rather than `unwrap_or(1)`: this project forbids the
// `unwrap`/`expect` family even where a panic-free variant exists.
#[allow(clippy::manual_unwrap_or)]
pub fn exit_code(status: ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => 1,
    }
}

/// Render the targets of `rakefile` for display, in declaration order.
///
/// Platform-specific variants are resolved at parse time, so only the matched
/// variant's commands appear. For each target the output shows the target name;
/// `depends_on` and `tools` summaries; and each command's name and body (cmd or
/// shell variants) with per-command `(platform: …)`/`(arch: …)`/`(tools: …)`/`(skip_on_error)` markers.
///
/// # Examples
///
/// ```
/// use librake::{Rakefile, list_targets};
///
/// let toml = "[[target.build.command]]\nname=\"compile\"\ncmd=[\"cargo\",\"build\"]";
/// let rakefile = Rakefile::from_toml_str(toml)?;
/// let out = list_targets(&rakefile);
/// assert!(out.contains("build"));
/// assert!(out.contains("compile"));
/// assert!(out.contains("cargo build"));
/// # Ok::<(), librake::Error>(())
/// ```
///
/// Platform-specific variants are resolved at parse time; only the matching
/// variant (or base) appears in the output:
///
/// ```
/// use librake::{Rakefile, list_targets};
///
/// let toml = r#"
/// [[target.sign.command]]
/// name = "notarize"
/// cmd  = ["xcrun", "notarytool", "submit"]
/// "#;
///
/// let rakefile = Rakefile::from_toml_str(toml)?;
/// let out = list_targets(&rakefile);
/// assert!(out.contains("sign"), "got: {out}");
/// assert!(out.contains("notarize"), "got: {out}");
/// # Ok::<(), librake::Error>(())
/// ```
///
/// A command with command-level tools shows a `(tools: …)` marker:
///
/// ```
/// use librake::{Rakefile, list_targets};
///
/// let toml = r#"
/// [tool.os.docker]
/// check = ["docker", "--version"]
///
/// [[target.build.command]]
/// name  = "package"
/// cmd   = ["docker", "build", "."]
/// tools = ["docker"]
/// "#;
///
/// let rakefile = Rakefile::from_toml_str(toml)?;
/// let out = list_targets(&rakefile);
/// assert!(out.contains("(tools: docker)"), "got: {out}");
/// # Ok::<(), librake::Error>(())
/// ```
#[must_use]
pub fn list_targets(rakefile: &Rakefile) -> String {
    let mut out = String::new();
    if rakefile.targets().is_empty() {
        out.push_str("No targets defined.\n");
        return out;
    }

    for (name, target) in rakefile.targets() {
        let _ = writeln!(out, "{name}");
        if !target.depends_on.is_empty() || !target.skip_deps.is_empty() {
            let all: Vec<String> = target
                .depends_on
                .iter()
                .cloned()
                .chain(target.skip_deps.iter().map(|s| format!("^{s}")))
                .collect();
            let _ = writeln!(out, "    depends_on: {}", all.join(", "));
        }
        if !target.tools.is_empty() {
            let _ = writeln!(out, "    tools: {}", target.tools.join(", "));
        }
        for command in &target.commands {
            let mut marker = String::new();
            if let Some(platforms) = &command.platform {
                let _ = write!(marker, " (platform: {})", platforms.join(", "));
            }
            if let Some(arches) = &command.arch {
                let _ = write!(marker, " (arch: {})", arches.join(", "));
            }
            if !command.tools.is_empty() {
                let _ = write!(marker, " (tools: {})", command.tools.join(", "));
            }
            if command.skip_on_error {
                marker.push_str(" (skip_on_error)");
            }
            let _ = writeln!(out, "    {}: {}{marker}", command.name, command.display());
        }
    }
    out
}
