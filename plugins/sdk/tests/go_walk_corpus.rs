//! `plugins/go/testdata/walk.json` holds project trees (files, `.gitignore`
//! layers, symlinks) with the file list `sdk::walk_project` returns for each.
//! The Go plugin's `walk_corpus_test.go` builds the same trees and expects the
//! same list from its own walker, so the two walkers cannot drift apart
//! unnoticed. This test holds the file equal to what the SDK walker answers.
//!
//! Paths starting with `../` are relative to the project root and land beside
//! it, which is how a case puts a file outside the project.
//!
//! Regenerate the file with `G_MESH_BLESS=1 cargo test -p g-mesh-plugin-sdk
//! --test go_walk_corpus`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use g_mesh_plugin_sdk::walk_project;
use serde_json::{json, Value};

struct Case {
    name: &'static str,
    files: &'static [&'static str],
    /// `(directory, text)`; the directory is project-relative, `""` for the root.
    gitignores: &'static [(&'static str, &'static str)],
    /// `(link path, symlink text)`.
    links: &'static [(&'static str, &'static str)],
}

const CASES: &[Case] = &[
    Case {
        name: "root gitignore literal and wildcard",
        files: &["main.go", "generated.go", "api_gen.go", "x1.go", "xy.go", "README.md"],
        gitignores: &[("", "generated.go\n*_gen.go\nx?.go\n")],
        links: &[],
    },
    Case {
        name: "comments and blank lines are not patterns",
        files: &["a.go", "b.go"],
        gitignores: &[("", "# a.go\n\nb.go\n")],
        links: &[],
    },
    Case {
        name: "nested gitignore applies below its directory only",
        files: &["a.go", "sub/a.go", "sub/b.go", "other/b.go"],
        gitignores: &[("sub", "b.go\n")],
        links: &[],
    },
    Case {
        name: "negation re-includes an earlier match",
        files: &["a.go", "keep.go", "drop.go"],
        gitignores: &[("", "*.go\n!keep.go\n")],
        links: &[],
    },
    Case {
        name: "deeper layer overrides an ancestor",
        files: &["a.go", "sub/a.go", "sub/b.go", "sub/deep/a.go"],
        gitignores: &[("", "*.go\n"), ("sub", "!a.go\n")],
        links: &[],
    },
    Case {
        name: "double star matches any depth",
        files: &["main.go", "a/gen/x.go", "a/b/gen/y.go", "a/b/c.go", "logs/one.go", "logs/two/three.go"],
        gitignores: &[("", "**/gen/*.go\nlogs/**\n")],
        links: &[],
    },
    Case {
        name: "double star between segments",
        files: &["a/b.go", "a/x/b.go", "a/x/y/b.go", "c/b.go"],
        gitignores: &[("", "a/**/b.go\n")],
        links: &[],
    },
    Case {
        name: "trailing slash matches directories only",
        files: &["build/out.go", "src/build", "main.go", "pkg/build/inner.go"],
        gitignores: &[("", "build/\n")],
        links: &[],
    },
    Case {
        name: "leading slash anchors to the layer directory",
        files: &["x.go", "sub/x.go", "sub/deep/x.go"],
        gitignores: &[("", "/x.go\n")],
        links: &[],
    },
    Case {
        name: "embedded slash anchors",
        files: &["a/b.go", "c/a/b.go"],
        gitignores: &[("", "a/b.go\n")],
        links: &[],
    },
    Case {
        name: "hard-excluded directories at any depth",
        files: &[
            "main.go",
            "vendor/dep/dep.go",
            "testdata/fixture.go",
            "pkg/vendor/v.go",
            "pkg/testdata/t.go",
            ".git/hooks/h.go",
            ".claude/worktrees/w.go",
        ],
        gitignores: &[],
        links: &[],
    },
    Case {
        name: "extension match is case-insensitive",
        files: &["lower.go", "UPPER.GO", "Mixed.Go", "notgo.gox", "dir/x.GO"],
        gitignores: &[],
        links: &[],
    },
    Case {
        name: "symlinked directory is followed once",
        files: &["real/a.go", "main.go"],
        gitignores: &[],
        links: &[("alias", "real")],
    },
    Case {
        name: "symlinked file to a walked file is listed once",
        files: &["real.go"],
        gitignores: &[],
        links: &[("link.go", "real.go")],
    },
    Case {
        name: "gitignored target reached only through a link is listed under the link",
        files: &["hidden/a.go", "main.go"],
        gitignores: &[("", "hidden/\n")],
        links: &[("view", "hidden")],
    },
    Case {
        name: "link to an ancestor terminates",
        files: &["a/b/c.go", "main.go"],
        gitignores: &[],
        links: &[("a/b/up", "../..")],
    },
    Case {
        name: "dangling link is skipped",
        files: &["main.go"],
        gitignores: &[],
        links: &[("broken", "does-not-exist"), ("broken.go", "missing.go")],
    },
    Case {
        name: "link outside the root is refused",
        files: &["main.go", "../outside/o.go"],
        gitignores: &[],
        links: &[("out", "../outside"), ("out.go", "../outside/o.go")],
    },
    Case {
        name: "link into an excluded directory is refused",
        files: &["main.go", "vendor/dep/dep.go"],
        gitignores: &[],
        links: &[("dep", "vendor/dep")],
    },
    Case {
        name: "nested links into one target list each file once",
        files: &["target/a.go", "target/sub/b.go"],
        gitignores: &[("", "target/\n")],
        links: &[("one", "target"), ("two", "target")],
    },
];

fn exclude_dirs() -> Vec<String> {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../go/plugin.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&manifest).unwrap();
    manifest["plugin"]["workspace"]["exclude_dirs"]
        .as_array()
        .expect("plugin.workspace.exclude_dirs in plugins/go/plugin.toml")
        .iter()
        .map(|dir| dir.as_str().unwrap().to_string())
        .collect()
}

fn materialise(root: &Path, case: &Case) {
    for file in case.files {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "package p\n").unwrap();
    }
    for (dir, text) in case.gitignores {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".gitignore"), text).unwrap();
    }
    for (link, target) in case.links {
        let path = root.join(link);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
    }
}

fn corpus() -> String {
    let excluded = exclude_dirs();
    let extensions = vec![".go".to_string()];
    let cases: Vec<Value> = CASES
        .iter()
        .map(|case| {
            let scratch = tempfile::tempdir().unwrap();
            let root = scratch.path().join("root");
            fs::create_dir_all(&root).unwrap();
            materialise(&root, case);
            let expect: Vec<String> =
                walk_project(&root, &extensions, &excluded).iter().map(|path| path.as_str().to_string()).collect();
            json!({
                "name": case.name,
                "files": case.files,
                "gitignores": case.gitignores.iter().map(|(dir, text)| json!({ "dir": dir, "text": text })).collect::<Vec<_>>(),
                "links": case.links.iter().map(|(link, target)| json!({ "link": link, "target": target })).collect::<Vec<_>>(),
                "expect": expect,
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({ "cases": cases })).unwrap();
    text.push('\n');
    text
}

#[test]
fn the_go_walk_corpus_matches_the_sdk_walker() {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../go/testdata/walk.json"));
    let expected = corpus();
    if std::env::var_os("G_MESH_BLESS").is_some() {
        fs::write(path, &expected).unwrap();
    }
    let on_disk =
        fs::read_to_string(path).expect("plugins/go/testdata/walk.json is missing; run with G_MESH_BLESS=1");
    assert_eq!(
        on_disk, expected,
        "plugins/go/testdata/walk.json is stale; regenerate it with G_MESH_BLESS=1"
    );
}
