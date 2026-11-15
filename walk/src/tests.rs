use super::*;
use std::fs;
use std::path::PathBuf;

/// A fresh directory under the system temp dir, removed on drop. Named by
/// test and process so parallel tests and runs never share one.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("g-mesh-walk-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn file(&self, relative: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A project with source beside, and nested under, the baseline directories.
fn project(test: &str) -> Scratch {
    let root = Scratch::new(test);
    root.file("main.rs");
    root.file("src/lib.rs");
    root.file(".git/HEAD");
    root.file(".git/objects/ab/cdef.rs");
    root.file(".claude/worktrees/agent/main.rs");
    root.file("src/.claude/x.rs");
    root.file("src/.git/y.rs");
    root
}

fn relatives(entries: &[WalkedEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.relative.as_str()).collect()
}

#[test]
fn walk_prunes_the_baseline_dirs_with_nothing_excluded() {
    let root = project("walk");
    let walked = walk(&root.0, &[]);
    assert_eq!(relatives(&walked.files), ["main.rs", "src/lib.rs"]);
}

#[test]
fn walk_filtered_prunes_the_baseline_dirs_with_nothing_excluded() {
    let root = project("walk-filtered");
    let walked = walk_filtered(&root.0, &[], |_| true);
    assert_eq!(relatives(&walked.files), ["main.rs", "src/lib.rs"]);
}

#[test]
fn walk_dirs_prunes_the_baseline_dirs_with_nothing_excluded() {
    let root = project("walk-dirs");
    let walked = walk_dirs(&root.0, &[]);
    assert_eq!(relatives(&walked.dirs), ["", "src"]);
}
