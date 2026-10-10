//! A Rust module file added mid-session with its `mod` line gets its
//! container without a `Cargo.toml` save, through the real Rust plugin and
//! the registry's routing (GM-507,
//! docs/architecture/gm-507-rust-module-tree-refresh.md, section 5).
//!
//! How a re-extract is seen, as in `config_reindex_rust.rs`: once the index
//! is built, every `.rs` file is rewritten on disk to declare `mark_v2`
//! instead of `mark_v1`, and none of those rewrites is routed. A file core
//! re-extracts shows `mark_v2`; a file left alone still shows `mark_v1`; a
//! whole-language reindex would show `mark_v2` everywhere. The crate has
//! enough modules that one moved file stays under core's
//! `FALLBACK_SHARE_PERCENT`. The manifest is the shipped one with
//! `semantic_pass` off, so no language server is involved.

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

fn harness() -> Harness {
    Harness::with_manifest("rust", |text| {
        assert!(text.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
        text.replace("semantic_pass = true", "semantic_pass = false")
    })
}

const MODULES: &[&str] = &["a", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8"];

fn source(items: &str, mark: &str) -> String {
    format!("{items}pub fn {mark}() -> u32 {{\n    1\n}}\n")
}

/// `lib.rs`'s items: every module of [`MODULES`], plus `extra`.
fn lib_items(extra: &[&str]) -> String {
    MODULES.iter().chain(extra).map(|name| format!("pub mod {name};\n")).collect()
}

/// Package `alpha` of `lib.rs` and the [`MODULES`], each declaring
/// `mark_v1`, indexed by the first `Cargo.toml` save.
fn indexed() -> Harness {
    let harness = harness();
    harness.write("Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    harness.write("src/lib.rs", &source(&lib_items(&[]), "mark_v1"));
    for name in MODULES {
        harness.write(&format!("src/{name}.rs"), &source("", "mark_v1"));
    }
    harness.route("Cargo.toml");
    assert_eq!(marked("mark_v1", &harness).len(), MODULES.len() + 1, "the first save indexed every file");
    harness
}

/// Rewrites every indexed file to `mark_v2` without routing it.
fn rewrite_unrouted(harness: &Harness, lib_extra: &[&str]) {
    harness.write("src/lib.rs", &source(&lib_items(lib_extra), "mark_v2"));
    for name in MODULES {
        harness.write(&format!("src/{name}.rs"), &source("", "mark_v2"));
    }
}

/// The files whose indexed `Function` nodes include `mark`, sorted.
fn marked(mark: &str, harness: &Harness) -> Vec<String> {
    harness.conn.with(|c| {
        let mut statement = c
            .prepare("SELECT DISTINCT filePath FROM nodes WHERE name = ?1 AND kind = 'Function' ORDER BY 1")
            .unwrap();
        statement
            .query_map([mark], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
}

/// The container of the function `name`, as indexed.
fn container_of(name: &str, harness: &Harness) -> Option<String> {
    harness.conn.with(|c| {
        c.query_row("SELECT container FROM nodes WHERE name = ?1 AND kind = 'Function'", [name], |row| {
            row.get(0)
        })
        .unwrap()
    })
}

/// Acceptance, child first: `child.rs` is created and indexed as an orphan;
/// `lib.rs` then gains `pub mod child;`, and that save alone gives the
/// child its container, re-extracting only `child.rs` beside `lib.rs`
/// (no reindex). Removing the `mod` line orphans it again.
///
/// Controls: answer `None` from `RustExtractor::source_changed` (the child
/// stays an orphan); drop the `affected` handling in
/// `watcher::apply::apply_file_change_in` (likewise).
#[test]
fn a_child_file_then_its_mod_line_gets_its_container_without_a_cargo_toml_save() {
    let harness = indexed();
    harness.write("src/child.rs", &source("pub fn child_fn() {}\n", "mark_v1"));
    harness.route("src/child.rs");
    assert_eq!(container_of("child_fn", &harness).as_deref(), Some("orphan:src/child.rs"));

    rewrite_unrouted(&harness, &["child"]);
    harness.write("src/child.rs", &source("pub fn child_fn() {}\n", "mark_v2"));
    harness.route("src/lib.rs");

    assert_eq!(container_of("child_fn", &harness).as_deref(), Some("alpha::child"));
    assert_eq!(
        marked("mark_v2", &harness),
        vec!["src/child.rs", "src/lib.rs"],
        "only the saved file and the moved one were extracted"
    );

    harness.write("src/lib.rs", &source(&lib_items(&[]), "mark_v2"));
    harness.route("src/lib.rs");
    assert_eq!(container_of("child_fn", &harness).as_deref(), Some("orphan:src/child.rs"), "orphaned again");
}

/// Acceptance, `mod` line first: `lib.rs` names `later` before it exists;
/// the file's own save places it. Removing the `mod` line orphans it.
///
/// Control: answer an empty `forced` set in the `(None, Some(_))` arm of
/// `ProjectContext::source_changed` (the new file extracts as an orphan).
#[test]
fn a_mod_line_then_its_child_file_gets_its_container() {
    let harness = indexed();
    harness.write("src/lib.rs", &source(&lib_items(&["later"]), "mark_v1"));
    harness.route("src/lib.rs");

    harness.write("src/later.rs", "pub fn later_fn() {}\n");
    harness.route("src/later.rs");
    assert_eq!(container_of("later_fn", &harness).as_deref(), Some("alpha::later"));

    harness.write("src/lib.rs", &source(&lib_items(&[]), "mark_v1"));
    harness.route("src/lib.rs");
    assert_eq!(container_of("later_fn", &harness).as_deref(), Some("orphan:src/later.rs"));
}

/// Behaviour 8 end to end: in a crate of three files the one moved file is
/// over `FALLBACK_SHARE_PERCENT`, so the save reindexes the language whole
/// (every rewritten file shows `mark_v2`), and the child still ends placed.
///
/// Control: drop `self.run_owed_reindex(..)` from
/// `PluginRegistry::file_changed` (nothing is reindexed: `a.rs` keeps
/// `mark_v1` and the child stays an orphan).
#[test]
fn a_selection_over_the_threshold_reindexes_the_language() {
    let harness = harness();
    harness.write("Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    harness.write("src/lib.rs", &source("pub mod a;\n", "mark_v1"));
    harness.write("src/a.rs", &source("", "mark_v1"));
    harness.write("src/child.rs", &source("pub fn child_fn() {}\n", "mark_v1"));
    harness.route("Cargo.toml");
    assert_eq!(container_of("child_fn", &harness).as_deref(), Some("orphan:src/child.rs"));

    harness.write("src/a.rs", &source("", "mark_v2"));
    harness.write("src/lib.rs", &source("pub mod a;\npub mod child;\n", "mark_v2"));
    harness.route("src/lib.rs");

    assert_eq!(
        marked("mark_v2", &harness),
        vec!["src/a.rs", "src/lib.rs"],
        "the whole language was re-walked"
    );
    assert_eq!(container_of("child_fn", &harness).as_deref(), Some("alpha::child"));
}

/// Must-confirm 4 of the note: `ensure_fresh` latency on a stale file whose
/// save moves one module (the selective path), against the same save in a
/// crate small enough to fall back to the whole-language reindex, and
/// against a save that keeps its `mod` items. Run by hand
/// (`--run-ignored only`); prints `GM507-MEASURE` lines.
#[test]
#[ignore = "measurement"]
fn measure_ensure_fresh_on_the_select_and_fallback_paths() {
    const ROUNDS: usize = 6;
    let selective = indexed();
    selective.write("src/child.rs", "pub fn child_fn() {}\n");
    selective.route("src/child.rs");
    let small = harness();
    small.write("Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    small.write("src/lib.rs", "pub mod a;\n");
    small.write("src/a.rs", "");
    small.write("src/child.rs", "pub fn child_fn() {}\n");
    small.route("Cargo.toml");

    let mut select = Vec::new();
    let mut fallback = Vec::new();
    let mut plain = Vec::new();
    for round in 0..ROUNDS {
        let with_child = round % 2 == 0;
        let mark = format!("round_{round}");
        let extra: &[&str] = if with_child { &["child"] } else { &[] };
        select.push(stale_query(&selective, &source(&lib_items(extra), &mark)));
        assert_eq!(
            container_of("child_fn", &selective).as_deref(),
            Some(if with_child { "alpha::child" } else { "orphan:src/child.rs" })
        );
        plain.push(stale_query(&selective, &source(&lib_items(extra), &format!("plain_{round}"))));

        small.write("src/a.rs", &source("", &mark));
        let items = if with_child { "pub mod a;\npub mod child;\n" } else { "pub mod a;\n" };
        fallback.push(stale_query(&small, &source(items, &mark)));
        assert_eq!(marked(&mark, &small), vec!["src/a.rs", "src/lib.rs"], "round {round} reindexed whole");
    }
    for (name, mut samples) in [("select", select), ("fallback", fallback), ("plain", plain)] {
        samples.sort();
        println!(
            "GM507-MEASURE ensure_fresh {name}: min {:?} median {:?} max {:?} over {ROUNDS}",
            samples[0],
            samples[ROUNDS / 2],
            samples[ROUNDS - 1]
        );
    }
}

/// Writes `src/lib.rs` without routing it, moves its mtime past the
/// recorded one, and times the query-time `ensure_fresh` that indexes it.
fn stale_query(harness: &Harness, text: &str) -> std::time::Duration {
    static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let step = STEP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    harness.write("src/lib.rs", text);
    let path = harness.root().join("src/lib.rs");
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(10 * step);
    file.set_modified(ahead).unwrap();
    drop(file);
    let started = std::time::Instant::now();
    let outcome = harness.registry.ensure_fresh(&harness.conn, "src/lib.rs").unwrap();
    let elapsed = started.elapsed();
    use g_mesh::watcher::staleness::StalenessOutcome::{ReindexedNoPriorRecord, ReindexedViaHashMismatch};
    assert!(matches!(outcome, Some(ReindexedViaHashMismatch | ReindexedNoPriorRecord)), "{outcome:?}");
    elapsed
}

/// Every node as `filePath|kind|name|container`, and every edge as
/// `from|kind|to|resolved` over those node labels, sorted: the index
/// content, free of row ids.
fn index_rows(harness: &Harness) -> (Vec<String>, Vec<String>) {
    harness.conn.with(|c| {
        let rows = |sql: &str| -> Vec<String> {
            let mut statement = c.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let label = |n: &str| {
            ["filePath", "kind", "name", "container"]
                .map(|column| format!("COALESCE({n}.{column}, '')"))
                .join(" || '|' || ")
        };
        let nodes = rows(&format!("SELECT {} FROM nodes n ORDER BY 1", label("n")));
        let edges = rows(&format!(
            "SELECT {} || ' -' || e.kind || '-> ' || {} || ' resolved=' || COALESCE(e.resolved, '') \
             FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId ORDER BY 1",
            label("f"),
            label("t")
        ));
        (nodes, edges)
    })
}

/// A fresh harness over `files`, indexed by its first `Cargo.toml` save.
fn cold(files: &[(&str, &str)]) -> Harness {
    let harness = harness();
    for (path, text) in files {
        harness.write(path, text);
    }
    harness.route("Cargo.toml");
    harness
}

/// The index equals a cold index of the same files.
fn assert_equals_cold(harness: &Harness, files: &[(&str, &str)]) {
    let warm = index_rows(harness);
    let cold = index_rows(&cold(files));
    assert!(!warm.0.is_empty(), "the warm index holds nodes");
    assert_eq!(warm.0, cold.0, "nodes differ from a cold index");
    assert_eq!(warm.1, cold.1, "edges differ from a cold index");
}

fn has_function(name: &str, harness: &Harness) -> bool {
    harness.conn.with(|c| {
        c.query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1 AND kind = 'Function'", [name], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
            > 0
    })
}

const MAIN_ONLY: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n"),
    ("src/main.rs", "mod cli;\nfn main() {}\n"),
    ("src/cli.rs", "pub fn cli_fn() {}\n"),
    ("src/core.rs", "pub fn core_fn() {}\n"),
];

const LIB_RS: (&str, &str) = ("src/lib.rs", "pub mod core;\npub fn lib_fn() {}\n");

/// Acceptance, `src/lib.rs`: in a package of `src/main.rs` alone, creating
/// `lib.rs` and routing only it places the lib's module without a
/// `Cargo.toml` save; the lib takes the package's crate key, so the bin's
/// module is orphaned. Deleting `lib.rs` restores the bin. Both states equal
/// a cold index of the same files.
///
/// Control: drop the crate-root check at the top of
/// `ProjectContext::source_changed` (`core.rs` stays an orphan).
#[test]
fn creating_and_deleting_src_lib_rs_moves_the_crate_without_a_cargo_toml_save() {
    let harness = cold(MAIN_ONLY);
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("alpha::cli"));
    assert_eq!(container_of("core_fn", &harness).as_deref(), Some("orphan:src/core.rs"));

    harness.write(LIB_RS.0, LIB_RS.1);
    harness.route("src/lib.rs");
    assert_eq!(container_of("core_fn", &harness).as_deref(), Some("alpha::core"));
    assert_eq!(container_of("lib_fn", &harness).as_deref(), Some("alpha"));
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("orphan:src/cli.rs"));
    let with_lib: Vec<(&str, &str)> = MAIN_ONLY.iter().copied().chain([LIB_RS]).collect();
    assert_equals_cold(&harness, &with_lib);

    std::fs::remove_file(harness.root().join("src/lib.rs")).unwrap();
    harness.route("src/lib.rs");
    assert!(!has_function("lib_fn", &harness), "the deleted file's nodes are gone");
    assert_eq!(container_of("core_fn", &harness).as_deref(), Some("orphan:src/core.rs"));
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("alpha::cli"));
    assert_equals_cold(&harness, MAIN_ONLY);
}

const LIB_ONLY: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"alphalib\"\n"),
    ("src/lib.rs", "pub fn lib_fn() {}\n"),
    ("src/cli.rs", "pub fn cli_fn() {}\n"),
];

const MAIN_RS: (&str, &str) = ("src/main.rs", "mod cli;\nfn main() {}\n");

/// Acceptance, `src/main.rs`: a package whose lib is named apart gains the
/// bin crate `alpha` when `main.rs` is created and routed, and its module
/// is placed; deleting `main.rs` drops the crate. Both states equal a cold
/// index of the same files.
///
/// Controls: drop the crate-root check at the top of
/// `ProjectContext::source_changed`; answer `None` from
/// `ProjectContext::reload_for` after the reload (`cli.rs` is never
/// re-extracted and stays an orphan).
#[test]
fn creating_and_deleting_src_main_rs_adds_and_drops_the_bin_crate() {
    let harness = cold(LIB_ONLY);
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("orphan:src/cli.rs"));

    harness.write(MAIN_RS.0, MAIN_RS.1);
    harness.route("src/main.rs");
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("alpha::cli"));
    assert_eq!(container_of("main", &harness).as_deref(), Some("alpha"));
    let with_main: Vec<(&str, &str)> = LIB_ONLY.iter().copied().chain([MAIN_RS]).collect();
    assert_equals_cold(&harness, &with_main);

    std::fs::remove_file(harness.root().join("src/main.rs")).unwrap();
    harness.route("src/main.rs");
    assert!(!has_function("main", &harness), "the deleted file's nodes are gone");
    assert_eq!(container_of("cli_fn", &harness).as_deref(), Some("orphan:src/cli.rs"));
    assert_equals_cold(&harness, LIB_ONLY);
}
