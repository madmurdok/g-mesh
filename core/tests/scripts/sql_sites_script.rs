//! `scripts/sql-sites.py`: the ratchet on SQL string literals written outside
//! the index's own modules (`storage/`, `graph/`, `link/`, `model/`). The real
//! tree passes its allow list; a new literal, a count above its allowance and
//! a lowered count left unrecorded each fail; only literals that open with an
//! uppercase SQL keyword on a live (non-test, non-comment) line count.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn real_allow() -> PathBuf {
    repo_root().join("scripts/sql-sites.allow")
}

fn sql_sites(src: &Path, check: Option<&Path>) -> Output {
    let mut command = Command::new("python3");
    command.arg(repo_root().join("scripts/sql-sites.py")).arg("--src").arg(src);
    if let Some(allow) = check {
        command.arg("--check").arg(allow);
    }
    command.current_dir(repo_root()).output().expect("python3 must be on PATH")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The `count path` lines of a run, and its `total N` line.
fn counts(output: &Output) -> (BTreeMap<String, usize>, usize) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = BTreeMap::new();
    let mut total = None;
    for line in stdout.lines() {
        let (first, rest) = line.split_once(' ').unwrap_or_else(|| panic!("unexpected line {line:?}"));
        if first == "total" {
            total = Some(rest.parse().unwrap());
        } else {
            files.insert(rest.to_string(), first.parse().unwrap());
        }
    }
    (files, total.unwrap_or_else(|| panic!("no total line\n{}", describe(output))))
}

/// The real allow list's `path -> allowance`, comments and blanks skipped.
fn allowances() -> BTreeMap<String, usize> {
    fs::read_to_string(real_allow())
        .unwrap()
        .lines()
        .filter_map(|line| {
            let text = line.split('#').next().unwrap().trim();
            let (path, count) = text.split_once(char::is_whitespace)?;
            Some((path.to_string(), count.trim().parse().unwrap()))
        })
        .collect()
}

/// A scratch `src` holding `files` (path relative to `src`, contents).
fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        write(dir.path(), path, contents);
    }
    dir
}

fn write(src: &Path, path: &str, contents: &str) {
    let full = src.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, contents).unwrap();
}

/// A scratch `src` holding a copy of every file the real allow list names:
/// every other real file has no site, so this tree passes the real list
/// exactly as `core/src` does.
fn copy_of_the_allowed_files() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for path in allowances().keys() {
        let contents = fs::read_to_string(repo_root().join("core/src").join(path)).unwrap();
        write(dir.path(), path, &contents);
    }
    dir
}

fn assert_fails_with(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", describe(output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&format!("sql-sites: {message}")), "expected {message:?}\n{}", describe(output));
}

#[test]
fn the_real_tree_passes_its_allow_list_and_counts_exactly_the_allowances() {
    let output = sql_sites(&repo_root().join("core/src"), Some(&real_allow()));

    assert!(output.status.success(), "{}", describe(&output));
    let (files, total) = counts(&output);
    let allowed = allowances();
    assert_eq!(files, allowed, "{}", describe(&output));
    assert_eq!(total, allowed.values().sum::<usize>(), "{}", describe(&output));
}

#[test]
fn a_new_sql_literal_fails_the_check_in_an_unlisted_file_and_in_a_listed_one() {
    let src = copy_of_the_allowed_files();
    let passes = sql_sites(src.path(), Some(&real_allow()));
    assert!(passes.status.success(), "the copied tree must pass first\n{}", describe(&passes));

    write(
        src.path(),
        "mcp/new_query.rs",
        "fn purge(conn: &Connection) {\n    conn.execute(\"DELETE FROM nodes\", []);\n}\n",
    );
    let output = sql_sites(src.path(), Some(&real_allow()));
    assert_fails_with(&output, "mcp/new_query.rs: 1 SQL site(s), not in the allow list");

    fs::remove_file(src.path().join("mcp/new_query.rs")).unwrap();
    let (listed, allowed) = allowances().into_iter().next().unwrap();
    let full = src.path().join(&listed);
    let mut contents = fs::read_to_string(&full).unwrap();
    contents.push_str("\nfn one_more() -> &'static str {\n    \"SELECT 1\"\n}\n");
    fs::write(&full, contents).unwrap();
    let output = sql_sites(src.path(), Some(&real_allow()));
    assert_fails_with(&output, &format!("{listed}: {} SQL site(s), allowed {allowed}", allowed + 1));
}

#[test]
fn a_lowered_count_fails_until_its_allowance_is_lowered_too() {
    let src = copy_of_the_allowed_files();
    let (listed, allowed) = allowances().into_iter().next().unwrap();
    let full = src.path().join(&listed);
    let contents = fs::read_to_string(&full).unwrap();
    // Spoil the file's first site: its keyword lowercased no longer counts.
    let keyword = ["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "PRAGMA", "WITH"]
        .iter()
        .filter_map(|keyword| contents.find(&format!("\"{keyword}")).map(|at| (at, *keyword)))
        .min()
        .unwrap_or_else(|| panic!("{listed} has no same-line SQL literal to spoil"));
    let spoiled = format!(
        "{}{}{}",
        &contents[..keyword.0 + 1],
        keyword.1.to_lowercase(),
        &contents[keyword.0 + 1 + keyword.1.len()..]
    );
    fs::write(&full, spoiled).unwrap();

    let output = sql_sites(src.path(), Some(&real_allow()));
    assert_fails_with(
        &output,
        &format!(
            "{listed}: {} SQL site(s), allowed {allowed} (lower the allowance to {})",
            allowed - 1,
            allowed - 1
        ),
    );

    let lowered = src.path().join("lowered.allow");
    let list: String = allowances()
        .into_iter()
        .map(|(path, count)| format!("{path} {}\n", if path == listed { count - 1 } else { count }))
        .collect();
    fs::write(&lowered, list).unwrap();
    let output = sql_sites(src.path(), Some(&lowered));
    assert!(output.status.success(), "a recorded lower count passes\n{}", describe(&output));

    fs::remove_file(&full).unwrap();
    let output = sql_sites(src.path(), Some(&lowered));
    assert_fails_with(
        &output,
        &format!("{listed}: 0 SQL site(s), allowed {} (lower the allowance to 0)", allowed - 1),
    );
}

#[test]
fn each_way_an_sql_literal_can_open_counts_as_one_site() {
    let src = tree(&[
        ("mcp/same_line.rs", "fn q() -> &'static str { \"SELECT id FROM nodes\" }\n"),
        ("mcp/same_line_raw.rs", "fn q() -> &'static str { r#\"SELECT \"id\" FROM nodes\"# }\n"),
        ("mcp/formatted.rs", "fn q(t: &str) -> String { format!(\"DELETE FROM {t}\") }\n"),
        ("mcp/newline_plain.rs", "fn q() -> &'static str {\n    \"\n    SELECT 1\"\n}\n"),
        ("mcp/newline_raw.rs", "fn q() -> &'static str {\n    r#\"\n    SELECT 1\"#\n}\n"),
        ("mcp/continued.rs", "fn q() -> &'static str {\n    \"\\\n     SELECT 1\"\n}\n"),
        ("mcp/two_on_a_line.rs", "fn q() -> [&'static str; 2] { [\"BEGIN\", \"RELEASE sp\"] }\n"),
    ]);

    let output = sql_sites(src.path(), None);

    assert!(output.status.success(), "{}", describe(&output));
    let (files, total) = counts(&output);
    let expected: BTreeMap<String, usize> = [
        ("mcp/same_line.rs", 1),
        ("mcp/same_line_raw.rs", 1),
        ("mcp/formatted.rs", 1),
        ("mcp/newline_plain.rs", 1),
        ("mcp/newline_raw.rs", 1),
        ("mcp/continued.rs", 1),
        ("mcp/two_on_a_line.rs", 2),
    ]
    .into_iter()
    .map(|(path, count)| (path.to_string(), count))
    .collect();
    assert_eq!(files, expected, "{}", describe(&output));
    assert_eq!(total, 8);
}

#[test]
fn lowercase_prose_and_sql_that_does_not_open_the_literal_do_not_count() {
    let src = tree(&[(
        "mcp/prose.rs",
        "fn p() -> [&'static str; 5] {\n    [\"select 1\", \"update the index\", \"n.id IN (SELECT id FROM x)\", \
         \"SELECTED files\", \"Delete it\"]\n}\n",
    )]);

    let output = sql_sites(src.path(), None);

    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(counts(&output), (BTreeMap::new(), 0), "{}", describe(&output));
}

#[test]
fn literals_in_comments_test_code_and_the_owner_modules_do_not_count() {
    let live = "fn live() -> &'static str { \"SELECT 1\" }\n";
    let commented = format!(
        "{live}// conn.execute(\"DELETE FROM nodes\")\n/* \"SELECT 2\"\n   \"UPDATE x\" */\n/// \"INSERT INTO y\"\n"
    );
    let cfg_test = format!(
        "{live}#[cfg(test)]\nmod tests {{\n    fn q() -> &'static str {{ \"SELECT 3\" }}\n    \
         fn r() -> &'static str {{\n        \"\n        DELETE FROM z\"\n    }}\n}}\n"
    );
    let src = tree(&[
        ("mcp/commented.rs", commented.as_str()),
        ("mcp/cfg_gated.rs", cfg_test.as_str()),
        ("mcp/tests.rs", live),
        ("mcp/query_tests.rs", live),
        ("mcp/tests/fixture.rs", live),
        ("storage/x.rs", live),
        ("graph/x.rs", live),
        ("link/x.rs", live),
        ("model/x.rs", live),
        ("storage.rs", live),
    ]);

    let output = sql_sites(src.path(), None);

    assert!(output.status.success(), "{}", describe(&output));
    let expected: BTreeMap<String, usize> =
        [("mcp/commented.rs".to_string(), 1), ("mcp/cfg_gated.rs".to_string(), 1)].into_iter().collect();
    assert_eq!(counts(&output), (expected, 2), "{}", describe(&output));
}

#[test]
fn a_malformed_or_repeated_allow_line_is_rejected() {
    let src = tree(&[("mcp/a.rs", "fn q() -> &'static str { \"SELECT 1\" }\n")]);
    for (name, list, message) in [
        ("malformed.allow", "mcp/a.rs one  # not a number\n", "expected `path count  # reason`"),
        ("repeated.allow", "mcp/a.rs 1\nmcp/a.rs 1\n", "mcp/a.rs is listed twice"),
    ] {
        let allow = src.path().join(name);
        fs::write(&allow, list).unwrap();

        let output = sql_sites(src.path(), Some(&allow));

        assert!(!output.status.success(), "{name}\n{}", describe(&output));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message), "{name}\n{}", describe(&output));
    }

    let allow = src.path().join("exact.allow");
    fs::write(&allow, "# header\n\nmcp/a.rs 1  # reason\n").unwrap();
    let output = sql_sites(src.path(), Some(&allow));
    assert!(output.status.success(), "{}", describe(&output));
}
