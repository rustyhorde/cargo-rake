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

    let mut order: Vec<&'a str> = Vec::new();
    for root in roots {
        // Fresh visited/in_progress per root so each root's whole graph runs;
        // deduplication is scoped to a single root, not the whole run.
        let mut visited: HashSet<&'a str> = HashSet::new();
        let mut in_progress: HashSet<&'a str> = HashSet::new();
        order_visit(targets, root, &mut visited, &mut in_progress, &mut order);
    }
    Ok(order)
}

/// Post-order DFS used by [`execution_order`]. The `in_progress` guard makes
/// this safe even on an unvalidated, cyclic graph (it simply stops descending).
fn order_visit<'a>(
    targets: &'a IndexMap<String, Target>,
    node: &str,
    visited: &mut HashSet<&'a str>,
    in_progress: &mut HashSet<&'a str>,
    order: &mut Vec<&'a str>,
) {
    let Some((key, target)) = targets.get_key_value(node) else {
        return;
    };
    let key: &'a str = key.as_str();
    if visited.contains(key) || in_progress.contains(key) {
        return;
    }

    let _ = in_progress.insert(key);
    for dependency in &target.depends_on {
        order_visit(targets, dependency, visited, in_progress, order);
    }
    let _ = in_progress.remove(key);
    let _ = visited.insert(key);
    order.push(key);
}

#[cfg(test)]
mod tests {
    use super::{execution_order, validate};
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
}
