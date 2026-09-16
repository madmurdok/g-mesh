//! Reading `pyproject.toml`'s own root-redirection hints - never by running
//! `setup.py`, invoking `pip`, or asking a build backend anything.
//!
//! # What is read, and why only this much
//!
//! The task this module implements is explicit: "any `[tool.setuptools]` /
//! `[tool.poetry]` / `[project]` packages hints in `pyproject.toml`, parsed
//! with the `toml` crate; never execute project code." Two fields change
//! where this plugin looks for code at all - which is the only thing a
//! *root* is, in `project`'s own vocabulary - and both are read:
//!
//! - **`[tool.poetry] packages`**: a list of `{ include = "...", from =
//!   "..." }` tables. `from` (default: the project root) names a directory
//!   that becomes a root. `include` is deliberately **not** read: it names
//!   *which* package under that root is published, not where the root is,
//!   and this plugin indexes everything reachable from a root regardless of
//!   what a build would publish (the same reasoning `plugins/rust/src/project`
//!   gives for not modeling `src/bin/*.rs` autodiscovery in reverse - a
//!   narrower *publishing* rule is not a narrower *indexing* rule).
//! - **`[tool.setuptools] package-dir`**: a table whose `""` key is
//!   setuptools' own way of saying "packages live under this directory
//!   instead of the project root" - the exact same fact `[tool.poetry]
//!   packages[].from` states for poetry. Every other key remaps one
//!   *specific* package's own directory (`{"foo": "some/other/place"}`),
//!   which is a narrower, per-package redirection this module does not
//!   model - see `project`'s module doc, "What is not modeled", for why that
//!   omission cannot produce a wrong container key (only a missed root, the
//!   same "missing beats wrong" trade every decision in this plugin makes).
//! - **`[tool.setuptools] packages`** (an explicit list) and PEP 621's own
//!   `[project]` table are not read at all: neither one names a *directory*
//!   a build backend doesn't already know from `package-dir`/`src`
//!   convention, so neither can teach this module a root it would not
//!   otherwise find.
//!
//! # Never fatal
//!
//! A missing `pyproject.toml` is not even worth a note - most Python
//! projects this plugin indexes will not have packaging hints to offer, and
//! that is the ordinary case, not a degraded one (`project::ProjectContext::load`
//! falls back to layout-only root detection). A malformed one is noted and
//! ignored, the same "validation is not this plugin's job, and a bad
//! manifest must not take the whole project model down with it" rule
//! `plugins/rust/src/project/cargo_manifest.rs` documents for `Cargo.toml`.

use std::path::Path;

use g_mesh_plugin_sdk::RelPath;
use serde::Deserialize;

/// The handful of `pyproject.toml` fields this plugin's root detection
/// needs. Every other table (`[project]`'s own metadata, `[tool.black]`,
/// `[build-system]`, ...) is parsed into nothing, by the absence of a field
/// for it - no `deny_unknown_fields` anywhere in this module, the same rule
/// `plugins/rust/src/project/cargo_manifest.rs`'s own doc states for
/// `Cargo.toml`: a `pyproject.toml` gaining a table this plugin does not
/// understand must never stop the project model from loading.
#[derive(Debug, Default, Deserialize)]
struct RawPyproject {
    tool: Option<RawTool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTool {
    poetry: Option<RawPoetry>,
    setuptools: Option<RawSetuptools>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPoetry {
    #[serde(default)]
    packages: Vec<RawPoetryPackage>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPoetryPackage {
    #[serde(default)]
    from: Option<String>,
    // `include` is intentionally absent - see this module's doc.
}

#[derive(Debug, Default, Deserialize)]
struct RawSetuptools {
    #[serde(default, rename = "package-dir")]
    package_dir: std::collections::BTreeMap<String, String>,
}

/// The root directories `<root>/pyproject.toml` names, project-relative, in
/// the order this module found them (`[tool.poetry] packages` before
/// `[tool.setuptools] package-dir`, declaration order within each) - not yet
/// deduplicated or merged with the layout-derived roots, which is
/// `ProjectContext::load`'s job. Empty when there is no `pyproject.toml`, it
/// cannot be read, it does not parse, or it declares neither hint.
pub(crate) fn root_hints(root: &Path, notes: &mut Vec<String>) -> Vec<RelPath> {
    let path = root.join("pyproject.toml");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        // No `pyproject.toml` at all is the ordinary case, not a note-worthy
        // one - see this module's doc.
        Err(_) => return Vec::new(),
    };
    let parsed: RawPyproject = match toml::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            notes.push(format!(
                "{}: {err:#} - ignored for root detection; falling back to layout-only roots",
                path.display()
            ));
            return Vec::new();
        }
    };

    let mut hints = Vec::new();
    let Some(tool) = parsed.tool else { return hints };
    if let Some(poetry) = tool.poetry {
        for package in poetry.packages {
            hints.push(RelPath::new(package.from.unwrap_or_default()));
        }
    }
    if let Some(setuptools) = tool.setuptools {
        if let Some(dir) = setuptools.package_dir.get("") {
            hints.push(RelPath::new(dir.clone()));
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let root = std::env::temp_dir()
                .join(format!("g-mesh-plugin-python-pyproject-{}-{name}-{nanos}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, contents: &str) {
            std::fs::write(self.0.join("pyproject.toml"), contents).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn no_pyproject_toml_yields_no_hints_and_no_note() {
        let tree = Tree::new("missing");
        let mut notes = Vec::new();
        assert!(root_hints(&tree.0, &mut notes).is_empty());
        assert!(notes.is_empty());
    }

    #[test]
    fn a_poetry_packages_from_names_a_root() {
        let tree = Tree::new("poetry");
        tree.write("[tool.poetry]\npackages = [{ include = \"pkg\", from = \"src\" }]\n");
        let mut notes = Vec::new();
        let hints = root_hints(&tree.0, &mut notes);
        assert_eq!(hints, vec![RelPath::new("src")]);
        assert!(notes.is_empty());
    }

    #[test]
    fn a_poetry_package_with_no_from_names_the_project_root() {
        let tree = Tree::new("poetry-no-from");
        tree.write("[tool.poetry]\npackages = [{ include = \"pkg\" }]\n");
        let mut notes = Vec::new();
        assert_eq!(root_hints(&tree.0, &mut notes), vec![RelPath::new("")]);
    }

    #[test]
    fn a_setuptools_package_dir_root_key_names_a_root() {
        let tree = Tree::new("setuptools");
        tree.write("[tool.setuptools]\npackage-dir = { \"\" = \"lib\" }\n");
        let mut notes = Vec::new();
        assert_eq!(root_hints(&tree.0, &mut notes), vec![RelPath::new("lib")]);
    }

    /// A per-package remap (`{"foo": "elsewhere"}`, no `""` key) is not a
    /// root hint this module reads - see the module doc's "What is not
    /// modeled".
    #[test]
    fn a_setuptools_package_dir_without_the_root_key_yields_no_hint() {
        let tree = Tree::new("setuptools-no-root-key");
        tree.write("[tool.setuptools]\npackage-dir = { \"foo\" = \"elsewhere\" }\n");
        let mut notes = Vec::new();
        assert!(root_hints(&tree.0, &mut notes).is_empty());
    }

    #[test]
    fn a_malformed_pyproject_toml_is_noted_and_ignored() {
        let tree = Tree::new("malformed");
        tree.write("this is not toml {{{");
        let mut notes = Vec::new();
        assert!(root_hints(&tree.0, &mut notes).is_empty());
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    /// A `pyproject.toml` with content this module does not read at all
    /// (plain PEP 621 `[project]` metadata, `[build-system]`, ...) yields no
    /// hints and no note - it is not malformed, it simply says nothing about
    /// roots.
    #[test]
    fn a_pyproject_toml_with_no_recognized_hints_is_silent() {
        let tree = Tree::new("no-hints");
        tree.write("[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[build-system]\nrequires = [\"setuptools\"]\n");
        let mut notes = Vec::new();
        assert!(root_hints(&tree.0, &mut notes).is_empty());
        assert!(notes.is_empty());
    }
}
