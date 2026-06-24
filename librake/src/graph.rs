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
/// ([`Error::CircularDependency`]).
pub(crate) fn validate(targets: &IndexMap<String, Target>) -> Result<()> {
    for (name, target) in targets {
        for dependency in &target.depends_on {
            if !targets.contains_key(dependency) {
                return Err(Error::UnknownDependency {
                    target: name.clone(),
                    dependency: dependency.clone(),
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

/// The outcome of [`execution_order_with_skips`]: the kept targets to run, in
/// order, plus the named skips that were actually part of the run graph (so the
/// caller can announce only the skips that had an effect).
#[derive(Debug)]
pub(crate) struct ExecutionPlan<'a> {
    /// The targets to run, in execution order, with skipped targets (and any
    /// dependency reachable only through one) pruned.
    pub order: Vec<&'a str>,
    /// The requested skips that were present in the un-skipped run graph, i.e.
    /// the ones that actually pruned something.
    pub skipped: Vec<&'a str>,
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
/// [`Error::UnknownTarget`]; both are checked before any ordering happens.
pub(crate) fn execution_order_with_skips<'a>(
    targets: &'a IndexMap<String, Target>,
    roots: &[&str],
    skips: &[&str],
) -> Result<ExecutionPlan<'a>> {
    for name in roots.iter().chain(skips) {
        if !targets.contains_key(*name) {
            return Err(Error::UnknownTarget {
                name: (*name).to_string(),
            });
        }
    }

    // The full (un-skipped) reachable set tells us which requested skips are
    // actually part of this run, so we announce only the ones that had an effect.
    let full: HashSet<&str> = build_order(targets, roots, &HashSet::new())
        .into_iter()
        .collect();
    let skipped: Vec<&'a str> = skips
        .iter()
        .filter_map(|s| targets.get_key_value(*s).map(|(k, _)| k.as_str()))
        .filter(|s| full.contains(s))
        .collect();

    let skip_set: HashSet<&str> = skips.iter().copied().collect();
    let order = build_order(targets, roots, &skip_set);

    // Eligibility: a skipped target may not be a dependency of any non-root
    // target that still runs. Iterate skips in order for a deterministic message.
    let root_set: HashSet<&str> = roots.iter().copied().collect();
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

    Ok(ExecutionPlan { order, skipped })
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
mod tests {
    use super::{execution_order, execution_order_with_skips, validate};
    use crate::{
        error::Error,
        rakefile::{Command, Target},
    };
    use indexmap::IndexMap;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn target(depends_on: &[&str]) -> Target {
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
            }],
            depends_on: depends_on.iter().map(|d| (*d).to_string()).collect(),
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
        validate(&targets)?;
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
        validate(&targets)?;
        let order = execution_order(&targets, &["x", "y"])?;
        assert_eq!(order, vec!["shared", "x", "shared", "y"]);
        Ok(())
    }

    #[test]
    fn duplicate_roots_run_each_time() -> TestResult {
        let targets = graph(&[("a", &[])]);
        validate(&targets)?;
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
        match validate(&targets) {
            Err(Error::UnknownDependency { target, dependency }) => {
                assert_eq!(target, "a");
                assert_eq!(dependency, "missing");
                Ok(())
            }
            other => Err(format!("expected UnknownDependency, got {other:?}").into()),
        }
    }

    #[test]
    fn cycle_is_reported_with_path() -> TestResult {
        let targets = graph(&[("a", &["b"]), ("b", &["a"])]);
        match validate(&targets) {
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
    fn skip_excludes_direct_only_dependency() -> TestResult {
        // a -> {b, c}; skip c (nothing else needs it).
        let targets = graph(&[("a", &["b", "c"]), ("b", &[]), ("c", &[])]);
        validate(&targets)?;
        let plan = execution_order_with_skips(&targets, &["a"], &["c"])?;
        assert_eq!(plan.order, vec!["b", "a"]);
        assert_eq!(plan.skipped, vec!["c"]);
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
        validate(&targets)?;
        let plan = execution_order_with_skips(&targets, &["a"], &["clean"])?;
        assert_eq!(plan.order, vec!["build", "a"]);
        assert!(!plan.order.contains(&"wipe"));
        assert_eq!(plan.skipped, vec!["clean"]);
        Ok(())
    }

    #[test]
    fn skip_blocked_by_nonroot_dependent() -> TestResult {
        // a -> build -> clean. `build` (not a root) needs clean, so clean cannot
        // be skipped.
        let targets = graph(&[("a", &["build"]), ("build", &["clean"]), ("clean", &[])]);
        validate(&targets)?;
        match execution_order_with_skips(&targets, &["a"], &["clean"]) {
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
        validate(&targets)?;
        let plan = execution_order_with_skips(&targets, &["x", "y"], &["shared"])?;
        assert_eq!(plan.order, vec!["x", "y"]);
        assert_eq!(plan.skipped, vec!["shared"]);
        Ok(())
    }

    #[test]
    fn unknown_skip_target_is_rejected() -> TestResult {
        let targets = graph(&[("a", &[])]);
        match execution_order_with_skips(&targets, &["a"], &["nope"]) {
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
        validate(&targets)?;
        let plan = execution_order_with_skips(&targets, &["a"], &["b"])?;
        assert_eq!(plan.order, vec!["a"]);
        assert!(plan.skipped.is_empty());
        Ok(())
    }
}
