//! Optional `.rake/config.toml` local override layer.
//!
//! Resolved relative to the current working directory, independent of
//! wherever the base `Rakefile.toml` itself lives (see
//! [`crate::Rakefile::from_path_with_host`]). When present, its contents are
//! merged onto the base `Rakefile.toml` using JSON-Merge-Patch (RFC 7386)
//! semantics before the merged result is parsed and validated.

use std::io;
use std::path::Path;

use crate::error::Result;

/// Path of the optional personal override layer, resolved relative to a
/// caller-supplied base directory.
const LOCAL_CONFIG_RELATIVE_PATH: &str = ".rake/config.toml";

/// Read and parse the optional `.rake/config.toml` override relative to
/// `cwd`. `Ok(None)` means the file does not exist — not an error, since the
/// override layer is entirely optional.
pub(crate) fn load(cwd: &Path) -> Result<Option<toml::Value>> {
    let path = cwd.join(LOCAL_CONFIG_RELATIVE_PATH);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(toml::from_str(&contents)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Merge `over` onto `base` using JSON-Merge-Patch semantics: recurse
/// key-by-key only where **both** sides are tables; any other pairing
/// (scalar, array — including arrays of tables — or a table/non-table type
/// mismatch) replaces `base`'s value at that key wholesale. A key present
/// only in `base` is untouched; a key present only in `over` is added.
/// Key order: existing `base` keys keep their original position; keys
/// introduced by `over` are appended, in `over`'s own order.
pub(crate) fn merge(mut base: toml::Value, over: toml::Value) -> toml::Value {
    merge_in_place(&mut base, over);
    base
}

fn merge_in_place(base: &mut toml::Value, over: toml::Value) {
    match (base.as_table_mut(), over) {
        (Some(base_table), toml::Value::Table(over_table)) => {
            for (key, over_value) in over_table {
                match base_table.entry(key) {
                    toml::map::Entry::Occupied(mut occupied) => {
                        merge_in_place(occupied.get_mut(), over_value);
                    }
                    toml::map::Entry::Vacant(vacant) => {
                        let _ = vacant.insert(over_value);
                    }
                }
            }
        }
        (_, over_value) => *base = over_value,
    }
}

#[cfg(test)]
mod tests {
    use super::{load, merge};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse(s: &str) -> Result<toml::Value, Box<dyn std::error::Error>> {
        Ok(toml::from_str(s)?)
    }

    #[test]
    fn scalar_override_replaces_base_value() -> TestResult {
        let base = parse(r#"toolchain = "stable""#)?;
        let over = parse(r#"toolchain = "beta""#)?;
        let merged = merge(base, over);
        assert_eq!(
            merged.get("toolchain").and_then(toml::Value::as_str),
            Some("beta")
        );
        Ok(())
    }

    #[test]
    fn nested_table_merges_recursively() -> TestResult {
        let base = parse(
            r#"
            [tool.cargo.widget]
            check = ["cargo", "widget", "--version"]
            install = ["cargo", "install", "widget"]
            "#,
        )?;
        let over = parse(
            r#"
            [tool.cargo.widget]
            install = ["cargo", "install", "widget", "--locked"]
            "#,
        )?;
        let merged = merge(base, over);
        let widget = &merged["tool"]["cargo"]["widget"];
        assert_eq!(
            widget
                .get("check")
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(3)
        );
        assert_eq!(
            widget
                .get("install")
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(4)
        );
        Ok(())
    }

    #[test]
    fn array_of_tables_replaced_wholesale() -> TestResult {
        let base = parse(
            r#"
            [[target.build.command]]
            name = "compile"
            cmd = ["cargo", "build"]

            [[target.build.command]]
            name = "lint"
            cmd = ["cargo", "clippy"]
            "#,
        )?;
        let over = parse(
            r#"
            [[target.build.command]]
            name = "compile"
            cmd = ["cargo", "build", "--offline"]
            "#,
        )?;
        let merged = merge(base, over);
        let commands = merged["target"]["build"]["command"]
            .as_array()
            .ok_or("expected command array")?;
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].get("name").and_then(toml::Value::as_str),
            Some("compile")
        );
        Ok(())
    }

    #[test]
    fn override_only_target_is_added() -> TestResult {
        let base = parse(
            r#"
            [[target.build.command]]
            name = "compile"
            cmd = ["cargo", "build"]
            "#,
        )?;
        let over = parse(
            r#"
            [[target.lint.command]]
            name = "clippy"
            cmd = ["cargo", "clippy"]
            "#,
        )?;
        let merged = merge(base, over);
        let targets = merged["target"].as_table().ok_or("expected target table")?;
        let names: Vec<&str> = targets.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["build", "lint"]);
        Ok(())
    }

    #[test]
    fn key_absent_from_base_is_inserted() -> TestResult {
        let base = parse("[tool]\n")?;
        let over = parse(
            r#"
            [tool.os.docker]
            check = ["docker", "--version"]
            "#,
        )?;
        let merged = merge(base, over);
        assert!(merged["tool"]["os"].get("docker").is_some());
        Ok(())
    }

    #[test]
    fn table_value_replaces_scalar() -> TestResult {
        let base = parse(r#"toolchain = "stable""#)?;
        let over = parse("[toolchain]\nchannel = \"x\"\n")?;
        let merged = merge(base, over);
        assert!(merged["toolchain"].is_table());
        Ok(())
    }

    #[test]
    fn scalar_replaces_table_value() -> TestResult {
        let base = parse("[x]\ny = 1\n")?;
        let over = parse(r#"x = "z""#)?;
        let merged = merge(base, over);
        assert_eq!(merged.get("x").and_then(toml::Value::as_str), Some("z"));
        Ok(())
    }

    #[test]
    fn key_order_preserved_base_first_then_override_appends() -> TestResult {
        let base = parse("a = 1\nb = 2\nc = 3\n")?;
        let over = parse("b = 20\nd = 4\ne = 5\n")?;
        let merged = merge(base, over);
        let table = merged.as_table().ok_or("expected top-level table")?;
        let keys: Vec<&str> = table.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["a", "b", "c", "d", "e"]);
        Ok(())
    }

    #[test]
    fn load_returns_none_when_file_absent() -> TestResult {
        let dir = tempfile::TempDir::new()?;
        assert!(load(dir.path())?.is_none());
        Ok(())
    }

    #[test]
    fn load_parses_present_file() -> TestResult {
        let dir = tempfile::TempDir::new()?;
        std::fs::create_dir_all(dir.path().join(".rake"))?;
        std::fs::write(
            dir.path().join(".rake").join("config.toml"),
            r#"toolchain = "beta""#,
        )?;
        let loaded = load(dir.path())?.ok_or("expected Some")?;
        assert_eq!(
            loaded.get("toolchain").and_then(toml::Value::as_str),
            Some("beta")
        );
        Ok(())
    }
}
