//! `scripts/new-task-worktree.sh` creates a task worktree and clones the main
//! checkout's `target/debug` into it only when asked and only when no build is
//! writing there. It runs for real against a scratch repository; `uname`,
//! `cp` and `pgrep` are stand-ins on `PATH`, so the clone path runs on any
//! Unix and a busy or quiet machine is a switch. Git only, so Unix only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BRANCH: &str = "task/probe";
const SUFFIX: &str = "probe";

/// `uname` says Darwin; `cp` drops `-c` (the APFS clone flag) and fails when
/// `$FAKE_CP_FAIL` is set; `pgrep` records its arguments in `$FAKE_PGREP_LOG`
/// and prints `$FAKE_PGREP_OUT`.
const STUBS: [(&str, &str); 3] = [
    ("uname", "#!/bin/sh\necho Darwin\n"),
    (
        "cp",
        "#!/bin/sh\n[ -z \"$FAKE_CP_FAIL\" ] || exit 1\nn=$#\ni=0\nwhile [ $i -lt $n ]; do\n  a=\"$1\"; shift\n  \
         [ \"$a\" = -c ] || set -- \"$@\" \"$a\"\n  i=$((i+1))\ndone\nexec /bin/cp \"$@\"\n",
    ),
    ("pgrep", "#!/bin/sh\necho \"$*\" >> \"$FAKE_PGREP_LOG\"\n[ -z \"$FAKE_PGREP_OUT\" ] || echo \"$FAKE_PGREP_OUT\"\n"),
];

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(output.status.success(), "git {args:?}\n{}", describe(&output));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A main checkout `<parent>/proj` holding the script and a built
/// `target/debug`, and a directory of stand-in tools.
struct Fixture {
    _dir: tempfile::TempDir,
    main: PathBuf,
    wt: PathBuf,
    bin: PathBuf,
    logs: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let parent = fs::canonicalize(dir.path()).unwrap();
        let main = parent.join("proj");
        fs::create_dir_all(main.join("scripts")).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/new-task-worktree.sh");
        fs::copy(script, main.join("scripts/new-task-worktree.sh")).unwrap();
        fs::write(main.join("README"), "x\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "base"]);
        fs::create_dir_all(main.join("target/debug/deps")).unwrap();
        fs::write(main.join("target/debug/deps/artifact"), "built\n").unwrap();

        let bin = parent.join("stubs");
        fs::create_dir_all(&bin).unwrap();
        for (name, body) in STUBS {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::create_dir_all(parent.join("logs")).unwrap();
        let wt = parent.join(format!("proj-wt-{SUFFIX}"));
        Fixture { logs: parent.join("logs"), _dir: dir, main, wt, bin }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = Command::new("bash");
        cmd.arg(self.main.join("scripts/new-task-worktree.sh"))
            .args(args)
            .current_dir(&self.main)
            .env("PATH", path)
            .env("FAKE_PGREP_LOG", self.logs.join("pgrep"))
            .env_remove("FAKE_PGREP_OUT")
            .env_remove("FAKE_CP_FAIL");
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().expect("failed to run bash")
    }

    fn create(&self, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let mut args = vec![SUFFIX, BRANCH];
        args.extend(extra);
        self.run(&args, env)
    }

    fn pgrep_calls(&self) -> String {
        fs::read_to_string(self.logs.join("pgrep")).unwrap_or_default()
    }

    fn branches(&self) -> String {
        git(&self.main, &["branch", "--list", BRANCH])
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn without_seed_the_worktree_is_created_and_no_target_is_cloned() {
    let fx = Fixture::new();
    let output = fx.create(&[], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(git(&fx.wt, &["branch", "--show-current"]), BRANCH);
    assert!(!fx.wt.join("target").exists(), "a target was created without --seed");
    assert!(stdout(&output).contains("not seeded (pass --seed"), "{}", describe(&output));
    assert_eq!(fx.pgrep_calls(), "", "the busy check ran without --seed");
}

#[test]
fn with_seed_and_no_build_running_target_debug_is_cloned_from_the_main_checkout() {
    let fx = Fixture::new();
    let output = fx.create(&["--seed"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fs::read_to_string(fx.wt.join("target/debug/deps/artifact")).unwrap(), "built\n");
    assert!(fx.main.join("target/debug/deps/artifact").is_file(), "the main target was touched");
    let calls = fx.pgrep_calls();
    assert!(
        calls.contains(&format!("--out-dir {}/target", fx.main.display())),
        "pgrep must look for builds writing into the main target, got {calls:?}"
    );
}

#[test]
fn with_seed_and_a_build_writing_into_the_main_target_nothing_is_cloned() {
    let fx = Fixture::new();
    let busy = format!("4242 rustc --out-dir {}/target/debug/deps", fx.main.display());
    let output = fx.create(&["--seed"], &[("FAKE_PGREP_OUT", &busy)]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(!fx.wt.join("target").exists(), "a target was cloned while a build was writing");
    assert!(stdout(&output).contains("not seeded: a build is writing"), "{}", describe(&output));
    assert_eq!(git(&fx.wt, &["branch", "--show-current"]), BRANCH);
}

#[test]
fn an_existing_directory_is_refused_before_anything_is_created() {
    let fx = Fixture::new();
    fs::create_dir_all(&fx.wt).unwrap();
    fs::write(fx.wt.join("keep"), "mine\n").unwrap();
    let output = fx.create(&["--seed"], &[]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(stderr(&output).contains("already exists"), "{}", describe(&output));
    assert_eq!(fs::read_to_string(fx.wt.join("keep")).unwrap(), "mine\n");
    assert_eq!(fx.branches(), "", "a branch was created for a refused directory");
    assert!(!stderr(&output).contains("wt-cleanup"), "nothing was created, so nothing to clean");
}

#[test]
fn the_old_no_seed_flag_is_an_unknown_option() {
    let fx = Fixture::new();
    let output = fx.create(&["--no-seed"], &[]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(stderr(&output).contains("usage:"), "{}", describe(&output));
    assert!(!fx.wt.exists(), "a worktree was created for a refused call");
}

/// A step after `git worktree add` fails: the script says how to remove what
/// it created, keeps its exit code, and deletes nothing itself.
#[test]
fn a_failure_after_the_worktree_is_added_prints_the_cleanup_command_and_keeps_the_worktree() {
    let fx = Fixture::new();
    let output = fx.create(&["--seed"], &[("FAKE_CP_FAIL", "1")]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    let err = stderr(&output);
    assert!(err.contains("wt-cleanup.sh"), "{}", describe(&output));
    assert!(err.contains(&format!("proj-wt-{SUFFIX}")), "{}", describe(&output));
    assert_eq!(git(&fx.wt, &["branch", "--show-current"]), BRANCH, "the worktree must be left in place");
}
