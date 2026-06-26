//! Dependency-graph validation and ordering over the targets in a `Rakefile`.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::{
    error::{Error, Result},
    rakefile::Target,
};

/// Validate the whole dependency graph.
///
/// Every `depends_on` entry must name an existing target
/// ([`Error::UnknownDependency`]) and the graph must be acyclic
/// ([`Error::CircularDependency`]). Dependencies present in `excluded`
/// (targets that exist only as platform-specific variants not matching the
/// current host) are silently accepted here — they are automatically pruned
/// during execution by [`build_order`] (which simply does not visit missing
/// targets). [`Error::TargetNotAvailableOnPlatform`] is only raised when a
/// user *explicitly requests* an excluded target as a root (see
/// [`execution_order_with_skips`]).
pub(crate) fn validate(
    targets: &IndexMap<String, Target>,
    excluded: &IndexMap<String, Vec<String>>,
) -> Result<()> {
    for (name, target) in targets {
        for dependency in &target.depends_on {
            if !targets.contains_key(dependency) {
                if excluded.contains_key(dependency.as_str()) {
                    continue;
                }
                return Err(Error::UnknownDependency {
                    target: name.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        for skip in &target.skip_deps {
            if !targets.contains_key(skip.as_str()) {
                if excluded.contains_key(skip.as_str()) {
                    continue;
                }
                return Err(Error::UnknownDependency {
                    target: name.clone(),
                    dependency: skip.clone(),
                });
            }
            if target.depends_on.contains(skip) {
                return Err(Error::ConflictingDependency {
                    target: name.clone(),
                    name: skip.clone(),
                });
            }
        }
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut path: Vec<&str> = Vec::new();
    let mut in_path: HashSet<&str> = HashSet::new();
    for name in targets.keys() {
        detect_cycle(
            targets,
            name.as_str(),
            &mut visited,
            &mut path,
            &mut in_path,
        )?;
    }
    Ok(())
}

/// Depth-first search that reports the first cycle it encounters.
///
/// `path`/`in_path` track the current recursion stack; `visited` records nodes
/// that have been fully explored so they are not revisited.
fn detect_cycle<'a>(
    targets: &'a IndexMap<String, Target>,
    node: &str,
    visited: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
    in_path: &mut HashSet<&'a str>,
) -> Result<()> {
    let Some((key, target)) = targets.get_key_value(node) else {
        return Ok(());
    };
    let key: &'a str = key.as_str();
    if visited.contains(key) {
        return Ok(());
    }

    path.push(key);
    let _ = in_path.insert(key);
    for dependency in &target.depends_on {
        let dependency = dependency.as_str();
        if in_path.contains(dependency) {
            // Walk the current path from the first occurrence of `dependency`
            // to its end, then close the loop by repeating it.
            let cycle: Vec<String> = path
                .iter()
                .skip_while(|step| **step != dependency)
                .map(|step| (*step).to_string())
                .chain(std::iter::once(dependency.to_string()))
                .collect();
            return Err(Error::CircularDependency { cycle });
        }
        detect_cycle(targets, dependency, visited, path, in_path)?;
    }
    let _ = path.pop();
    let _ = in_path.remove(key);
    let _ = visited.insert(key);
    Ok(())
}

/// Compute the execution order for `roots`: each root's transitive dependency
/// graph in full, concatenated in the order the roots are given. Within a single
/// root's graph a target appears at most once (dependencies before dependents,
/// declaration order as the tie-break, root last); across roots there is no
/// deduplication, so a target shared by several roots runs once per root — `puc`
/// then `most` runs all of `puc`'s graph, then all of `most`'s.
///
/// Returns [`Error::UnknownTarget`] if any entry in `roots` is not present;
/// every root is checked before any ordering happens, so an unknown target
/// fails fast.
#[cfg(test)]
pub(crate) fn execution_order<'a>(
    targets: &'a IndexMap<String, Target>,
    roots: &[&str],
) -> Result<Vec<&'a str>> {
    for root in roots {
        if !targets.contains_key(*root) {
            return Err(Error::UnknownTarget {
                name: (*root).to_string(),
            });
        }
    }
    Ok(build_order(targets, roots, &HashSet::new()))
}

/// A single step in an [`ExecutionPlan`]: either run a target or announce that
/// it was skipped at the position it would have run.
#[derive(Debug)]
pub(crate) enum Step<'a> {
    Run(&'a str),
    Skip(&'a str),
}

/// The outcome of [`execution_order_with_skips`]: an interleaved sequence of
/// run and skip steps in the natural execution order, so callers can announce
/// each skip at the exact point where the target would have run.
#[derive(Debug)]
pub(crate) struct ExecutionPlan<'a> {
    pub steps: Vec<Step<'a>>,
}

/// Like [`execution_order`] but prunes each target named in `skips` (and any
/// dependency reachable *only* through a skipped target) from the run.
///
/// A skip is honored only when no *other* target that will still run depends on
/// it; the named `roots` are exempt (skipping a root's own direct dependency is
/// the point). If a non-root target that remains in the run lists a skipped
/// target in its `depends_on`, this returns [`Error::SkipNotAllowed`] rather
/// than silently running that target without its prerequisite.
///
/// Every `root` and every `skip` must name an existing target, else
/// [`Error::UnknownTarget`]; both are checked before any ordering happens. If a
/// name is absent from `targets` but present in `excluded` (platform-specific
/// variants that don't match the current host), [`Error::TargetNotAvailableOnPlatform`]
/// is returned instead.
pub(crate) fn execution_order_with_skips<'a>(
    targets: &'a IndexMap<String, Target>,
    excluded: &IndexMap<String, Vec<String>>,
    roots: &[&str],
    skips: &[&str],
) -> Result<ExecutionPlan<'a>> {
    for name in roots.iter().chain(skips) {
        if !targets.contains_key(*name) {
            if let Some(available_on) = excluded.get(*name) {
                return Err(Error::TargetNotAvailableOnPlatform {
                    name: (*name).to_string(),
                    available_on: available_on.clone(),
                });
            }
            return Err(Error::UnknownTarget {
                name: (*name).to_string(),
            });
        }
    }

    // The full (un-skipped) reachable set: used to find which skips had an
    // effect and to collect embedded skip_deps from targets in the graph.
    let full_order = build_order(targets, roots, &HashSet::new());

    // Augment the CLI-provided skip set with embedded skip_deps declared in
    // each target's depends_on (as ^-prefixed entries). Explicit roots are
    // exempt: if the user named a target as a root, it wins over any embedded
    // skip of the same name.
    let root_set: HashSet<&str> = roots.iter().copied().collect();
    let mut cli_skip_set: HashSet<&str> = skips.iter().copied().collect();
    for step in &full_order {
        if let Some(target) = targets.get(*step) {
            for dep_skip in &target.skip_deps {
                if let Some((key, _)) = targets.get_key_value(dep_skip.as_str()) {
                    let key: &'a str = key.as_str();
                    if !root_set.contains(key) {
                        let _ = cli_skip_set.insert(key);
                    }
                }
            }
        }
    }

    let order = build_order(targets, roots, &cli_skip_set);

    // Eligibility: a CLI-requested skip may not be a dependency of any non-root
    // target that still runs. Embedded skip_deps are intentional and are always
    // honored without this check.
    for skip in skips {
        let dependents: Vec<&str> = order
            .iter()
            .filter(|step| !root_set.contains(*step))
            .filter(|step| {
                targets
                    .get(**step)
                    .is_some_and(|target| target.depends_on.iter().any(|dep| dep == skip))
            })
            .copied()
            .collect();
        if !dependents.is_empty() {
            return Err(Error::SkipNotAllowed {
                target: (*skip).to_string(),
                dependents: dependents.join(", "),
            });
        }
    }

    // Build interleaved steps from the full (un-skipped) order:
    // - CLI/embedded skip targets become Skip steps at their natural position
    // - targets still in the kept order become Run steps
    // - orphan dependencies (reachable only through a skipped target) are omitted
    let order_set: HashSet<&str> = order.iter().copied().collect();
    let steps = full_order
        .into_iter()
        .filter_map(|t| {
            if cli_skip_set.contains(t) {
                Some(Step::Skip(t))
            } else if order_set.contains(t) {
                Some(Step::Run(t))
            } else {
                None
            }
        })
        .collect();

    Ok(ExecutionPlan { steps })
}

/// Build the concatenated per-root execution order, never visiting a target in
/// `skips`. Roots are assumed to exist (callers validate first).
fn build_order<'a>(
    targets: &'a IndexMap<String, Target>,
    roots: &[&str],
    skips: &HashSet<&str>,
) -> Vec<&'a str> {
    let mut order: Vec<&'a str> = Vec::new();
    for root in roots {
        // Fresh visited/in_progress per root so each root's whole graph runs;
        // deduplication is scoped to a single root, not the whole run.
        let mut visited: HashSet<&'a str> = HashSet::new();
        let mut in_progress: HashSet<&'a str> = HashSet::new();
        order_visit(
            targets,
            root,
            skips,
            &mut visited,
            &mut in_progress,
            &mut order,
        );
    }
    order
}

/// Post-order DFS used by [`build_order`]. A node in `skips` (and so everything
/// reachable only through it) is never visited. The `in_progress` guard makes
/// this safe even on an unvalidated, cyclic graph (it simply stops descending).
fn order_visit<'a>(
    targets: &'a IndexMap<String, Target>,
    node: &str,
    skips: &HashSet<&str>,
    visited: &mut HashSet<&'a str>,
    in_progress: &mut HashSet<&'a str>,
    order: &mut Vec<&'a str>,
) {
    if skips.contains(node) {
        return;
    }
    let Some((key, target)) = targets.get_key_value(node) else {
        return;
    };
    let key: &'a str = key.as_str();
    if visited.contains(key) || in_progress.contains(key) {
        return;
    }

    let _ = in_progress.insert(key);
    for dependency in &target.depends_on {
        order_visit(targets, dependency, skips, visited, in_progress, order);
    }
    let _ = in_progress.remove(key);
    let _ = visited.insert(key);
    order.push(key);
}

#[cfg(test)]
impl<'a> ExecutionPlan<'a> {
    fn order(&self) -> Vec<&'a str> {
        self.steps
            .iter()
            .filter_map(|s| if let Step::Run(t) = s { Some(*t) } else { None })
            .collect()
    }

    fn skipped(&self) -> Vec<&'a str> {
        self.steps
            .iter()
            .filter_map(|s| {
                if let Step::Skip(t) = s {
                    Some(*t)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{execution_order, execution_order_with_skips, validate};
    use crate::{
        error::Error,
        rakefile::{Command, Target},
    };
    use indexmap::IndexMap;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn target(depends_on: &[&str]) -> Target {
        target_full(depends_on, &[])
    }

    fn target_full(depends_on: &[&str], skip_deps: &[&str]) -> Target {
        Target {
            commands: vec![Command {
                name: "run".to_string(),
                cmd: Some(vec!["true".to_string()]),
                sh: None,
                fish: None,
                ps: None,
                skip_on_error: false,
                platform: None,
                arch: None,
                tools: vec![],
            }],
            depends_on: depends_on.iter().map(|d| (*d).to_string()).collect(),
            skip_deps: skip_deps.iter().map(|s| (*s).to_string()).collect(),
            tools: Vec::new(),
        }
    }

    fn graph(entries: &[(&str, &[&str])]) -> IndexMap<String, Target> {
        let mut map = IndexMap::new();
        for (name, deps) in entries {
            let _old = map.insert((*name).to_string(), target(deps));
        }
        map
    }

    #[test]
    fn diamond_runs_each_prerequisite_once() -> TestResult {
        // a -> {b, c}, b -> d, c -> d
        let targets = graph(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])]);
        validate(&targets, &IndexMap::new())?;
        let order = execution_order(&targets, &["a"])?;
        assert_eq!(order, vec!["d", "b", "c", "a"]);
        Ok(())
    }

    #[test]
    fn each_root_runs_its_full_graph() -> TestResult {
        // x -> shared, y -> shared. Each root runs its whole graph in turn, so
        // `shared` runs once per root (no cross-root deduplication).
        let targets = graph(&[
            ("x", &["shared"]),
            ("y", &["shared"]),
            ("shared", &[]),
            ("other", &[]),
        ]);
        validate(&targets, &IndexMap::new())?;
        let order = execution_order(&targets, &["x", "y"])?;
        assert_eq!(order, vec!["shared", "x", "shared", "y"]);
        Ok(())
    }

    #[test]
    fn duplicate_roots_run_each_time() -> TestResult {
        let targets = graph(&[("a", &[])]);
        validate(&targets, &IndexMap::new())?;
        let order = execution_order(&targets, &["a", "a"])?;
        assert_eq!(order, vec!["a", "a"]);
        Ok(())
    }

    #[test]
    fn unknown_root_among_several_is_rejected() -> TestResult {
        let targets = graph(&[("a", &[]), ("b", &[])]);
        match execution_order(&targets, &["a", "nope", "b"]) {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_dependency_is_rejected() -> TestResult {
        let targets = graph(&[("a", &["missing"])]);
        match validate(&targets, &IndexMap::new()) {
            Err(Error::UnknownDependency { target, dependency }) => {
                assert_eq!(target, "a");
                assert_eq!(dependency, "missing");
                Ok(())
            }
            other => Err(format!("expected UnknownDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn excluded_dependency_is_silently_accepted_at_parse_time() -> TestResult {
        // `a` depends on `mac`, which is in the excluded map (macos-only).
        // validate() must accept this silently: excluded deps are pruned during
        // execution by build_order, not rejected at parse time.
        let mut targets = graph(&[("a", &[])]);
        let excluded: IndexMap<String, Vec<String>> =
            [("mac".to_string(), vec!["macos".to_string()])]
                .into_iter()
                .collect();
        targets.get_mut("a").unwrap().depends_on = vec!["mac".to_string()];
        validate(&targets, &excluded)?;
        Ok(())
    }

    #[test]
    fn cycle_is_reported_with_path() -> TestResult {
        let targets = graph(&[("a", &["b"]), ("b", &["a"])]);
        match validate(&targets, &IndexMap::new()) {
            Err(Error::CircularDependency { cycle }) => {
                assert_eq!(
                    cycle.first().map(String::as_str),
                    cycle.last().map(String::as_str)
                );
                assert!(cycle.contains(&"a".to_string()));
                assert!(cycle.contains(&"b".to_string()));
                Ok(())
            }
            other => Err(format!("expected CircularDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_root_is_rejected() -> TestResult {
        let targets = graph(&[("a", &[])]);
        match execution_order(&targets, &["nope"]) {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn excluded_root_gives_platform_error() -> TestResult {
        let targets = graph(&[("build", &[])]);
        let excluded: IndexMap<String, Vec<String>> =
            [("mac".to_string(), vec!["macos".to_string()])]
                .into_iter()
                .collect();
        match execution_order_with_skips(&targets, &excluded, &["mac"], &[]) {
            Err(Error::TargetNotAvailableOnPlatform { name, available_on }) => {
                assert_eq!(name, "mac");
                assert_eq!(available_on, vec!["macos".to_string()]);
                Ok(())
            }
            other => Err(format!("expected TargetNotAvailableOnPlatform, got {other:?}").into()),
        }
    }

    #[test]
    fn skip_excludes_direct_only_dependency() -> TestResult {
        // a -> {b, c}; skip c (nothing else needs it).
        let targets = graph(&[("a", &["b", "c"]), ("b", &[]), ("c", &[])]);
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["a"], &["c"])?;
        assert_eq!(plan.order(), vec!["b", "a"]);
        assert_eq!(plan.skipped(), vec!["c"]);
        Ok(())
    }

    #[test]
    fn skip_prunes_orphaned_transitive_dependency() -> TestResult {
        // a -> {clean, build}, clean -> wipe. Skipping clean also prunes wipe,
        // which was reachable only through clean.
        let targets = graph(&[
            ("a", &["clean", "build"]),
            ("clean", &["wipe"]),
            ("build", &[]),
            ("wipe", &[]),
        ]);
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["a"], &["clean"])?;
        assert_eq!(plan.order(), vec!["build", "a"]);
        assert!(!plan.order().contains(&"wipe"));
        assert_eq!(plan.skipped(), vec!["clean"]);
        Ok(())
    }

    #[test]
    fn skip_blocked_by_nonroot_dependent() -> TestResult {
        // a -> build -> clean. `build` (not a root) needs clean, so clean cannot
        // be skipped.
        let targets = graph(&[("a", &["build"]), ("build", &["clean"]), ("clean", &[])]);
        validate(&targets, &IndexMap::new())?;
        match execution_order_with_skips(&targets, &IndexMap::new(), &["a"], &["clean"]) {
            Err(Error::SkipNotAllowed { target, dependents }) => {
                assert_eq!(target, "clean");
                assert_eq!(dependents, "build");
                Ok(())
            }
            other => Err(format!("expected SkipNotAllowed, got {other:?}").into()),
        }
    }

    #[test]
    fn skip_shared_dependency_allowed_when_only_roots_depend() -> TestResult {
        // x -> shared, y -> shared. Both dependents are named roots, which are
        // exempt, so skipping shared is allowed.
        let targets = graph(&[("x", &["shared"]), ("y", &["shared"]), ("shared", &[])]);
        validate(&targets, &IndexMap::new())?;
        let plan =
            execution_order_with_skips(&targets, &IndexMap::new(), &["x", "y"], &["shared"])?;
        assert_eq!(plan.order(), vec!["x", "y"]);
        // Each root's graph runs independently, so the skip appears once per
        // root at the position where shared would have run.
        assert_eq!(plan.skipped(), vec!["shared", "shared"]);
        Ok(())
    }

    #[test]
    fn unknown_skip_target_is_rejected() -> TestResult {
        let targets = graph(&[("a", &[])]);
        match execution_order_with_skips(&targets, &IndexMap::new(), &["a"], &["nope"]) {
            Err(Error::UnknownTarget { name }) => {
                assert_eq!(name, "nope");
                Ok(())
            }
            other => Err(format!("expected UnknownTarget, got {other:?}").into()),
        }
    }

    #[test]
    fn skip_target_not_in_run_graph_is_noop() -> TestResult {
        // `b` exists but is not part of `a`'s graph: nothing to prune.
        let targets = graph(&[("a", &[]), ("b", &[])]);
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["a"], &["b"])?;
        assert_eq!(plan.order(), vec!["a"]);
        assert!(plan.skipped().is_empty());
        Ok(())
    }

    #[test]
    fn validate_rejects_unknown_skip_dep() -> TestResult {
        let mut targets = graph(&[("a", &[])]);
        targets.get_mut("a").unwrap().skip_deps = vec!["ghost".to_string()];
        match validate(&targets, &IndexMap::new()) {
            Err(Error::UnknownDependency { target, dependency }) => {
                assert_eq!(target, "a");
                assert_eq!(dependency, "ghost");
                Ok(())
            }
            other => Err(format!("expected UnknownDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn validate_rejects_conflicting_dep_and_skip_dep() -> TestResult {
        let mut targets = graph(&[("a", &["b"]), ("b", &[])]);
        targets.get_mut("a").unwrap().skip_deps = vec!["b".to_string()];
        match validate(&targets, &IndexMap::new()) {
            Err(Error::ConflictingDependency { target, name }) => {
                assert_eq!(target, "a");
                assert_eq!(name, "b");
                Ok(())
            }
            other => Err(format!("expected ConflictingDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn embedded_skip_dep_prunes_from_execution() -> TestResult {
        // `ci` depends on `all`, and declares `clean` as a skip_dep.
        // `all` depends on `clean` and `build`. Running `rake ci` should prune
        // `clean` from the run without the user passing `^clean` on the CLI.
        let mut targets = IndexMap::new();
        drop(targets.insert("build".to_string(), target(&[])));
        drop(targets.insert("clean".to_string(), target(&[])));
        drop(targets.insert("all".to_string(), target(&["build", "clean"])));
        drop(targets.insert("ci".to_string(), target_full(&["all"], &["clean"])));
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["ci"], &[])?;
        assert_eq!(plan.order(), vec!["build", "all", "ci"]);
        assert!(!plan.order().contains(&"clean"));
        assert_eq!(plan.skipped(), vec!["clean"]);
        Ok(())
    }

    #[test]
    fn embedded_skip_dep_in_transitive_dependency_applies() -> TestResult {
        // `deploy` depends on `ci`, which declares `clean` as a skip_dep.
        // Running `rake deploy` should still prune `clean`.
        let mut targets = IndexMap::new();
        drop(targets.insert("build".to_string(), target(&[])));
        drop(targets.insert("clean".to_string(), target(&[])));
        drop(targets.insert("all".to_string(), target(&["build", "clean"])));
        drop(targets.insert("ci".to_string(), target_full(&["all"], &["clean"])));
        drop(targets.insert("deploy".to_string(), target(&["ci"])));
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["deploy"], &[])?;
        assert_eq!(plan.order(), vec!["build", "all", "ci", "deploy"]);
        assert!(!plan.order().contains(&"clean"));
        assert_eq!(plan.skipped(), vec!["clean"]);
        Ok(())
    }

    #[test]
    fn root_wins_over_embedded_skip_dep() -> TestResult {
        // `ci` declares `clean` as a skip_dep, but the user explicitly names
        // `clean` as a root. The root takes precedence — `clean` runs.
        let mut targets = IndexMap::new();
        drop(targets.insert("build".to_string(), target(&[])));
        drop(targets.insert("clean".to_string(), target(&[])));
        drop(targets.insert("all".to_string(), target(&["build", "clean"])));
        drop(targets.insert("ci".to_string(), target_full(&["all"], &["clean"])));
        validate(&targets, &IndexMap::new())?;
        let plan = execution_order_with_skips(&targets, &IndexMap::new(), &["ci", "clean"], &[])?;
        // `clean` is a root, so it must appear in the order
        assert!(plan.order().contains(&"clean"));
        Ok(())
    }
}
