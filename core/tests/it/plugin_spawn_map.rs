//! `scripts/test-plugin-spawns.txt` stays in step with the tests it describes.
//!
//! `scripts/test-select.sh --narrow` runs, for a diff under `plugins/<p>/`,
//! only the core tests that map says start plugin `<p>`. A test that starts a
//! plugin but is not listed would never run locally for that plugin's diffs.
//! This guard fails, naming the module, when:
//!
//! (a) an `it` module has no row (or two), or a row names a module that does
//!     not exist;
//! (b) an `it` module's source, or a `core/tests/fixtures/<dir>` it names,
//!     holds a marker of plugin `<p>` and the row does not list `<p>`. A
//!     marker is a string literal that ends in one of `<p>`'s extensions (read
//!     from `plugins/<p>/plugin.toml`), names `plugins/<p>` or
//!     `g-mesh-plugin-<p>`; or, for a fixture dir, a file with such an
//!     extension (for typescript, also `tsconfig.json` and `package.json`);
//! (c) a core lib test module reads `../plugins` or uses `typescript_manifest`,
//!     `bundled_roots` or `PluginProcess::spawn`, and has no `lib:` row (an
//!     explicit `none` row says it only reads `plugin.toml`).
//!
//! Plain file reading: it runs in milliseconds, in `core-it`, so in CI and in
//! every narrowed run that touches `core/tests/it/`. A literal such as
//! `"main.rs"` is a rust marker, so the rows of this file and of `layout.rs`
//! list rust though neither starts a plugin.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const PLUGINS: [&str; 4] = ["typescript", "rust", "python", "go"];

const MAP: &str = "scripts/test-plugin-spawns.txt";

/// Row key (`it:<module>`, `lib:<module>`, `scripts:<module>`) to the plugins
/// it lists; `none` is the empty set.
type Rows = BTreeMap<String, BTreeSet<String>>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The `.rs` files under `dir`, sorted; a missing `dir` has none.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read_dir) = fs::read_dir(dir) else { return out };
    let mut entries: Vec<PathBuf> = read_dir.map(|e| e.expect("directory entry").path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read_dir) = fs::read_dir(dir) else { return out };
    let mut entries: Vec<PathBuf> = read_dir.map(|e| e.expect("directory entry").path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(all_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn parse_rows(text: &str, problems: &mut Vec<String>) -> Rows {
    let mut rows = Rows::new();
    for (index, line) in text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        let mut words = code.split_whitespace();
        let Some(key) = words.next() else { continue };
        let at = format!("{MAP}:{}", index + 1);
        match key.split_once(':') {
            Some(("it" | "lib" | "scripts", module)) if !module.is_empty() => {}
            _ => {
                problems.push(format!("{at}: `{key}` is not it:<module>, lib:<module> or scripts:<module>"));
                continue;
            }
        }
        let listed: Vec<&str> = words.collect();
        let mut plugins = BTreeSet::new();
        for word in &listed {
            if *word != "none" && !PLUGINS.contains(word) {
                problems.push(format!("{at}: `{word}` is not a plugin ({PLUGINS:?}) or `none`"));
            } else if *word != "none" {
                plugins.insert((*word).to_string());
            }
        }
        if listed.is_empty() || (listed.contains(&"none") && listed.len() > 1) {
            problems.push(format!("{at}: `{key}` must list plugins, or only `none`"));
        }
        if rows.insert(key.to_string(), plugins).is_some() {
            problems.push(format!("{at}: more than one row for `{key}`"));
        }
    }
    rows
}

/// The module names `main.rs` declares with `mod x;` / `pub mod x;`.
fn declared_modules(main_rs: &Path) -> BTreeSet<String> {
    read(main_rs)
        .lines()
        .filter_map(|line| {
            let code = line.split("//").next().unwrap_or("").trim();
            let code = code.strip_prefix("pub ").unwrap_or(code);
            Some(code.strip_prefix("mod ")?.strip_suffix(';')?.trim().to_string())
        })
        .collect()
}

/// The extensions `plugins/<p>/plugin.toml` declares, empty when the plugin or
/// the key is absent.
fn extensions(root: &Path, plugin: &str) -> Vec<String> {
    let path = root.join("plugins").join(plugin).join("plugin.toml");
    let Ok(text) = fs::read_to_string(&path) else { return Vec::new() };
    let config: toml::Value = text.parse().unwrap_or_else(|e| panic!("{} is not TOML: {e}", path.display()));
    config["plugin"]["languages"]["extensions"]
        .as_array()
        .map(|list| list.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The double quote, spelled so that this scanner's own source has balanced quotes.
const QUOTE: char = '\u{22}';

/// Every double-quoted string literal in `text`, comments included.
fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != QUOTE {
            continue;
        }
        let mut lit = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    lit.push(c);
                    lit.extend(chars.next());
                }
                QUOTE => {
                    closed = true;
                    break;
                }
                _ => lit.push(c),
            }
        }
        if closed {
            out.push(lit);
        }
    }
    out
}

/// The plugins whose marker a source text holds.
fn text_markers(text: &str, exts: &BTreeMap<&str, Vec<String>>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for lit in literals(text) {
        for plugin in PLUGINS {
            let by_extension = exts[plugin].iter().any(|e| lit.ends_with(e.as_str()));
            if by_extension
                || lit.contains(&format!("plugins/{plugin}"))
                || lit.contains(&format!("g-mesh-plugin-{plugin}"))
            {
                found.insert(plugin.to_string());
            }
        }
    }
    found
}

/// The plugins whose files a fixture directory holds.
fn fixture_markers(dir: &Path, exts: &BTreeMap<&str, Vec<String>>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for file in all_files(dir) {
        let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
        for plugin in PLUGINS {
            let project_file = plugin == "typescript" && (name == "tsconfig.json" || name == "package.json");
            if project_file || exts[plugin].iter().any(|e| name.ends_with(e.as_str())) {
                found.insert(plugin.to_string());
            }
        }
    }
    found
}

/// The source of `it` module `name`: `<name>.rs`, or every `.rs` under `<name>/`.
fn module_source(it_dir: &Path, name: &str) -> String {
    let file = it_dir.join(format!("{name}.rs"));
    if file.is_file() {
        return read(&file);
    }
    rust_files(&it_dir.join(name)).iter().map(|f| read(f)).collect()
}

/// `text` with whole-line `//` comments removed.
fn without_comments(text: &str) -> String {
    text.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
}

/// A lib test file: its module path (`daemon::registry::tests`) and the text
/// that is test code. The file's module comes from its path, or from the
/// `#[path = "x"]` attribute and `mod <name>;` that declare it.
struct LibTest {
    module: String,
    text: String,
}

fn module_of(src: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(src).ok()?.with_extension("");
    let mut parts: Vec<String> =
        rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.last().is_some_and(|p| p == "mod") {
        parts.pop();
    }
    (!parts.is_empty() && parts != ["lib"] && parts != ["main"]).then(|| parts.join("::"))
}

fn lib_tests(src: &Path) -> Vec<LibTest> {
    let files = rust_files(src);
    let mut declared_by_path: BTreeMap<PathBuf, String> = BTreeMap::new();
    for file in &files {
        let Some(parent_module) = module_of(src, file) else { continue };
        let text = read(file);
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(target) = line.trim().strip_prefix("#[path = \"").and_then(|r| r.strip_suffix("\"]"))
            else {
                continue;
            };
            let name = lines[i + 1..].iter().find_map(|l| {
                let l = l.trim().strip_prefix("pub ").unwrap_or(l.trim());
                Some(l.strip_prefix("mod ")?.trim_end_matches(';').trim().to_string())
            });
            if let Some(name) = name {
                declared_by_path.insert(
                    file.parent().expect("a file has a parent").join(target),
                    format!("{parent_module}::{name}"),
                );
            }
        }
    }
    let mut out = Vec::new();
    for file in files {
        let Some(own_module) = module_of(src, &file) else { continue };
        let module = declared_by_path.get(&file).cloned().unwrap_or(own_module);
        let stem = file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let in_tests_dir =
            file.strip_prefix(src).map(|r| r.components().any(|c| c.as_os_str() == "tests")).unwrap_or(false);
        let text = read(&file);
        let test_code = if stem == "tests" || stem.ends_with("_tests") || in_tests_dir {
            text
        } else if let Some(at) =
            text.find("#[cfg(test)]\nmod ").or_else(|| text.find("#[cfg(test)]\npub mod "))
        {
            text[at..].to_string()
        } else {
            continue;
        };
        out.push(LibTest { module, text: without_comments(&test_code) });
    }
    out
}

fn is_lib_spawner(text: &str) -> bool {
    literals(text).iter().any(|l| l.contains("../plugins"))
        || ["typescript_manifest", "bundled_roots", "PluginProcess::spawn"].iter().any(|n| text.contains(n))
}

/// Every way the map at `root` disagrees with the tests under `root`.
fn violations(root: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let rows = parse_rows(&read(&root.join(MAP)), &mut problems);
    let it_dir = root.join("core/tests/it");
    let exts: BTreeMap<&str, Vec<String>> = PLUGINS.iter().map(|p| (*p, extensions(root, p))).collect();

    // (a) one row per it module, and no row without a module.
    let mut modules = declared_modules(&it_dir.join("main.rs"));
    modules.remove("common");
    for module in &modules {
        if !rows.contains_key(&format!("it:{module}")) {
            problems.push(format!("it:{module} has no row in {MAP}"));
        }
    }
    for key in rows.keys().filter_map(|k| k.strip_prefix("it:")) {
        if !modules.contains(key) {
            problems.push(format!("{MAP} has a row for it:{key}, which is not a module of core/tests/it"));
        }
    }
    let scripts_modules = declared_modules(&root.join("core/tests/scripts/main.rs"));
    for key in rows.keys().filter_map(|k| k.strip_prefix("scripts:")) {
        if !scripts_modules.contains(key) {
            problems.push(format!(
                "{MAP} has a row for scripts:{key}, which is not a module of core/tests/scripts"
            ));
        }
    }

    // (b) a marker needs its plugin in the row.
    let fixtures = root.join("core/tests/fixtures");
    let fixture_dirs: Vec<String> = fs::read_dir(&fixtures)
        .map(|rd| {
            rd.map(|e| e.expect("directory entry").path())
                .filter(|p| p.is_dir())
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    for module in &modules {
        let Some(listed) = rows.get(&format!("it:{module}")) else { continue };
        let source = module_source(&it_dir, module);
        let mut needed: BTreeMap<String, String> = BTreeMap::new();
        for plugin in text_markers(&source, &exts) {
            needed.insert(plugin, "its source".to_string());
        }
        for dir in &fixture_dirs {
            let named = source.contains(&format!("fixtures/{dir}")) || literals(&source).contains(dir);
            if named {
                for plugin in fixture_markers(&fixtures.join(dir), &exts) {
                    needed.entry(plugin).or_insert_with(|| format!("fixtures/{dir}"));
                }
            }
        }
        for (plugin, from) in needed {
            if !listed.contains(&plugin) {
                problems.push(format!(
                    "it:{module}: {from} holds a {plugin} marker, but its row in {MAP} lists {}",
                    if listed.is_empty() {
                        "none".to_string()
                    } else {
                        listed.iter().cloned().collect::<Vec<_>>().join(" ")
                    }
                ));
            }
        }
    }

    // (c) lib test modules that read plugins/ or start a plugin need a lib row.
    let tests = lib_tests(&root.join("core/src"));
    let mut known: BTreeSet<String> = BTreeSet::new();
    for test in &tests {
        let mut path = String::new();
        for part in test.module.split("::") {
            path = if path.is_empty() { part.to_string() } else { format!("{path}::{part}") };
            known.insert(path.clone());
        }
    }
    for test in &tests {
        if !is_lib_spawner(&test.text) {
            continue;
        }
        let covered = rows.keys().filter_map(|k| k.strip_prefix("lib:")).any(|row| {
            test.module == row || test.module.strip_prefix(row).is_some_and(|rest| rest.starts_with("::"))
        });
        if !covered {
            problems.push(format!(
                "lib:{} reads ../plugins or starts a plugin, but {MAP} has no `lib:` row for it \
                 (`none` if it only reads plugin.toml)",
                test.module
            ));
        }
    }
    for key in rows.keys().filter_map(|k| k.strip_prefix("lib:")) {
        if !known.contains(key) {
            problems.push(format!("{MAP} has a row for lib:{key}, which is not a core module with tests"));
        }
    }
    problems
}

#[test]
fn the_spawn_map_matches_the_core_tests() {
    let problems = violations(&repo_root());
    assert!(
        problems.is_empty(),
        "{MAP} is out of step with the tests; fix the row (or add one):\n{}",
        problems.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The guard on scratch trees: each case changes one thing from a tree with no
// violations, so a case failing for another reason would show in `a_clean_tree`.

const FAKE_TS: &str = ".fakets";
const FAKE_RS: &str = ".fakers";

struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    /// Two plugins with made-up extensions, an `it` binary with one module
    /// `quiet` (row `none`), and a lib module `foo::tests` that reads nothing.
    fn new() -> Self {
        let tree = Tree { dir: tempfile::tempdir().unwrap() };
        for (plugin, ext) in [("typescript", FAKE_TS), ("rust", FAKE_RS)] {
            tree.put(
                &format!("plugins/{plugin}/plugin.toml"),
                &format!("[plugin.languages]\nextensions = [\"{ext}\"]\n"),
            );
        }
        tree.put("core/tests/it/main.rs", "mod common;\nmod quiet;\n");
        tree.put("core/tests/it/quiet.rs", "#[test]\nfn nothing() {}\n");
        tree.put("core/tests/scripts/main.rs", "mod a_script;\n");
        tree.put("core/tests/fixtures/.keep", "");
        tree.put("core/src/lib.rs", "mod foo;\n");
        tree.put("core/src/foo/mod.rs", "mod tests;\n");
        tree.put("core/src/foo/tests.rs", "fn t() {}\n");
        tree.map("it:quiet none\n");
        tree
    }

    fn put(&self, relative: &str, body: &str) {
        let path = self.dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn map(&self, rows: &str) {
        self.put(MAP, rows);
    }

    fn problems(&self) -> Vec<String> {
        violations(self.dir.path())
    }

    fn add_it_module(&self, name: &str, body: &str) {
        let main = self.dir.path().join("core/tests/it/main.rs");
        fs::write(&main, format!("{}mod {name};\n", read(&main))).unwrap();
        self.put(&format!("core/tests/it/{name}.rs"), body);
    }
}

fn mentions(problems: &[String], parts: &[&str]) -> bool {
    problems.iter().any(|p| parts.iter().all(|part| p.contains(part)))
}

#[test]
fn a_clean_tree_has_no_violations() {
    assert_eq!(Tree::new().problems(), Vec::<String>::new());
}

#[test]
fn an_it_module_without_a_row_is_named() {
    let tree = Tree::new();
    tree.add_it_module("probe", "fn f() {}\n");
    let problems = tree.problems();
    assert!(mentions(&problems, &["it:probe", "no row"]), "{problems:?}");
    tree.map("it:quiet none\nit:probe none\n");
    assert_eq!(tree.problems(), Vec::<String>::new());
}

#[test]
fn a_row_of_a_module_that_is_gone_or_a_second_row_is_named() {
    let tree = Tree::new();
    tree.map("it:quiet none\nit:gone none\nscripts:gone none\n");
    let problems = tree.problems();
    assert!(mentions(&problems, &["it:gone", "not a module"]), "{problems:?}");
    assert!(mentions(&problems, &["scripts:gone", "not a module"]), "{problems:?}");

    tree.map("it:quiet none\nit:quiet typescript\n");
    assert!(mentions(&tree.problems(), &["more than one row", "it:quiet"]), "{:?}", tree.problems());
}

#[test]
fn a_malformed_row_is_named() {
    let tree = Tree::new();
    for (rows, needle) in [
        ("it:quiet none\nbogus:x none\n", "bogus:x"),
        ("it:quiet none\nit:x cobol\n", "cobol"),
        ("it:quiet none typescript\n", "only `none`"),
        ("it:quiet\n", "must list plugins"),
    ] {
        tree.map(rows);
        assert!(mentions(&tree.problems(), &[needle]), "{rows:?} -> {:?}", tree.problems());
    }
}

#[test]
fn a_marker_in_a_modules_source_needs_its_plugin_in_the_row() {
    let tree = Tree::new();
    let cases = [
        ("typescript", format!("fn f() {{ let _ = \"a{FAKE_TS}\"; }}\n")),
        ("rust", format!("fn f() {{ let _ = \"{}\"; }}\n", "plugins/rust")),
        ("rust", format!("fn f() {{ let _ = \"g-mesh-plugin-{}\"; }}\n", "rust")),
    ];
    for (plugin, body) in cases {
        tree.add_it_module("probe", &body);
        tree.map("it:quiet none\nit:probe none\n");
        let problems = tree.problems();
        assert!(mentions(&problems, &["it:probe", plugin]), "{body} -> {problems:?}");
        tree.map(&format!("it:quiet none\nit:probe {plugin}\n"));
        assert_eq!(tree.problems(), Vec::<String>::new(), "{body}");
        // Back to the clean tree for the next case.
        let main = tree.dir.path().join("core/tests/it/main.rs");
        fs::write(main, "mod common;\nmod quiet;\n").unwrap();
    }
}

#[test]
fn a_marker_in_a_directory_module_or_a_named_fixture_dir_counts() {
    let tree = Tree::new();
    tree.put("core/tests/it/main.rs", "mod common;\nmod quiet;\nmod nested;\n");
    tree.put("core/tests/it/nested/mod.rs", "mod inner;\n");
    tree.put("core/tests/it/nested/inner.rs", &format!("fn f() {{ let _ = \"x{FAKE_RS}\"; }}\n"));
    tree.map("it:quiet none\nit:nested none\n");
    assert!(mentions(&tree.problems(), &["it:nested", "rust"]), "{:?}", tree.problems());

    // A fixture dir counts only when the module names it.
    tree.put("core/tests/it/nested/inner.rs", "fn f() { let _ = \"fixtures/probe_dir\"; }\n");
    tree.put("core/tests/fixtures/probe_dir/sub/file.fakets", "");
    let problems = tree.problems();
    assert!(mentions(&problems, &["it:nested", "fixtures/probe_dir", "typescript"]), "{problems:?}");
    tree.put("core/tests/it/nested/inner.rs", "fn f() {}\n");
    assert_eq!(tree.problems(), Vec::<String>::new());
}

#[test]
fn a_lib_test_that_reads_plugins_needs_a_lib_row_and_none_is_enough() {
    let tree = Tree::new();
    tree.put("core/src/foo/tests.rs", "fn t() { let _ = std::path::Path::new(\"../plugins\"); }\n");
    let problems = tree.problems();
    assert!(mentions(&problems, &["lib:foo::tests", "no `lib:` row"]), "{problems:?}");

    tree.map("it:quiet none\nlib:foo::tests none\n");
    assert_eq!(tree.problems(), Vec::<String>::new());
    // A row for an ancestor module covers it.
    tree.map("it:quiet none\nlib:foo typescript\n");
    assert_eq!(tree.problems(), Vec::<String>::new());
}

#[test]
fn a_lib_test_that_starts_a_plugin_by_name_needs_a_row_but_production_code_does_not() {
    let tree = Tree::new();
    tree.put("core/src/foo/tests.rs", "fn t() { PluginProcess::spawn(); }\n");
    assert!(mentions(&tree.problems(), &["lib:foo::tests"]), "{:?}", tree.problems());

    // Production code before the test module, and a comment inside it, are not tests.
    tree.put("core/src/foo/tests.rs", "// bundled_roots in a comment\nfn t() {}\n");
    tree.put("core/src/lib.rs", "mod foo;\nmod prod;\n");
    tree.put(
        "core/src/prod.rs",
        "fn p() { bundled_roots(); }\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
    );
    assert_eq!(tree.problems(), Vec::<String>::new());
    tree.put(
        "core/src/prod.rs",
        "fn p() {}\n#[cfg(test)]\nmod tests {\n    fn t() { bundled_roots(); }\n}\n",
    );
    assert!(mentions(&tree.problems(), &["lib:prod"]), "{:?}", tree.problems());
}

#[test]
fn a_tests_file_declared_by_a_path_attribute_is_the_modules_tests() {
    let tree = Tree::new();
    tree.put("core/src/lib.rs", "mod foo;\nmod widget;\n");
    tree.put("core/src/widget.rs", "#[cfg(test)]\n#[path = \"widget_tests.rs\"]\nmod tests;\n");
    tree.put("core/src/widget_tests.rs", "fn t() { typescript_manifest(); }\n");
    assert!(mentions(&tree.problems(), &["lib:widget::tests"]), "{:?}", tree.problems());
    tree.map("it:quiet none\nlib:widget::tests typescript\n");
    assert_eq!(tree.problems(), Vec::<String>::new());
}

#[test]
fn a_lib_row_of_a_module_that_is_gone_is_named() {
    let tree = Tree::new();
    tree.map("it:quiet none\nlib:foo::vanished none\n");
    assert!(mentions(&tree.problems(), &["lib:foo::vanished", "not a core module"]), "{:?}", tree.problems());
}
