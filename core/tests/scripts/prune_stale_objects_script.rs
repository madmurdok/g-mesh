//! `scripts/prune-stale-objects.sh` deletes the `*.rcgu.o` in
//! `<target>/debug/deps` that no current executable's debug map (N_OSO)
//! references (GM-538), and `scripts/test-sections.sh run` calls it before
//! cargo.
//!
//! Most tests run the script on a fake target dir whose "binaries" are tiny
//! Mach-O files written here, so the debug map is exactly what the test says.
//! A `uname` stub on `PATH` makes the script take its macOS path on any Unix
//! (the parsing is plain python3), or its off-macOS path. One test builds a
//! real scratch crate twice and checks the prune against `nm`'s reading of the
//! binary and against a backtrace; it needs the macOS toolchain, so it is
//! macOS-only. Every test points `CARGO_TARGET_DIR` at a temp dir: the script
//! must never see this checkout's own `target`.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `PATH` whose `uname` prints `system`, ahead of the real one.
fn uname_path(dir: &Path, system: &str) -> String {
    let bin = dir.join("uname-bin");
    fs::create_dir_all(&bin).unwrap();
    write_exec(&bin.join("uname"), &format!("#!/bin/sh\necho {system}\n"));
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

fn prune(path_env: &str, target: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/prune-stale-objects.sh"))
        .args(args)
        .env("PATH", path_env)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("failed to run bash")
}

const N_OSO: u8 = 0x66;
const N_SECT_EXT: u8 = 0x0f;

/// A thin 64-bit little-endian Mach-O file holding only a symbol table: one
/// (n_type, string) symbol per entry. An LC_UUID precedes LC_SYMTAB so the
/// reader has to walk the load commands.
fn macho(symbols: &[(u8, &str)]) -> Vec<u8> {
    let mut strtab = vec![0u8];
    let mut nlist = Vec::new();
    for (n_type, name) in symbols {
        nlist.extend((strtab.len() as u32).to_le_bytes());
        nlist.push(*n_type);
        nlist.push(0); // n_sect
        nlist.extend(0u16.to_le_bytes()); // n_desc
        nlist.extend(0u64.to_le_bytes()); // n_value
        strtab.extend(name.as_bytes());
        strtab.push(0);
    }
    let symoff = 32 + 24 + 24;
    let stroff = symoff + nlist.len();
    let mut out = Vec::new();
    out.extend([0xcf, 0xfa, 0xed, 0xfe]); // MH_MAGIC_64
    out.extend(0x0100_0007u32.to_le_bytes()); // CPU_TYPE_X86_64
    out.extend(3u32.to_le_bytes()); // cpusubtype
    out.extend(2u32.to_le_bytes()); // MH_EXECUTE
    out.extend(2u32.to_le_bytes()); // ncmds
    out.extend(48u32.to_le_bytes()); // sizeofcmds
    out.extend(0u32.to_le_bytes()); // flags
    out.extend(0u32.to_le_bytes()); // reserved
    out.extend(0x1bu32.to_le_bytes()); // LC_UUID
    out.extend(24u32.to_le_bytes());
    out.extend([0x5a; 16]);
    out.extend(2u32.to_le_bytes()); // LC_SYMTAB
    out.extend(24u32.to_le_bytes());
    out.extend((symoff as u32).to_le_bytes());
    out.extend((symbols.len() as u32).to_le_bytes());
    out.extend((stroff as u32).to_le_bytes());
    out.extend((strtab.len() as u32).to_le_bytes());
    assert_eq!(out.len(), symoff);
    out.extend(nlist);
    out.extend(strtab);
    out
}

fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect()
}

fn touch(path: &Path) {
    fs::write(path, b"not an object, only a name").unwrap();
}

/// A fake `<target>/debug` laid out as cargo leaves it. Objects named in a
/// current binary's debug map: `kept-a` (by an absolute path into another
/// target dir, as after copying the target), `kept-b` (by the example only),
/// `kept-c` (by the hardlinked binary). Everything else that ends in
/// `.rcgu.o` in deps is stale, including objects named only by a
/// non-debug-map symbol, an rlib member, or a path whose parent is not
/// `deps`. Returns the target dir.
fn fake_target(root: &Path) -> PathBuf {
    let target = root.join("target");
    let debug = target.join("debug");
    let deps = debug.join("deps");
    let examples = debug.join("examples");
    fs::create_dir_all(&deps).unwrap();
    fs::create_dir_all(&examples).unwrap();

    for name in [
        "kept-a.cgu0.rcgu.o",
        "kept-b.cgu0.rcgu.o",
        "kept-c.cgu0.rcgu.o",
        "stale-old.cgu0.rcgu.o",
        "stale-symbol.cgu0.rcgu.o",
        "stale-member.cgu0.rcgu.o",
        "stale-notdeps.cgu0.rcgu.o",
        // Not `.rcgu.o`: never touched.
        "app-1111.d",
        "libdep-2222.rlib",
        "libdep-2222.rmeta",
        "plain.o",
    ] {
        touch(&deps.join(name));
    }
    // A `.rcgu.o` outside deps is not the script's business.
    touch(&debug.join("outside.cgu0.rcgu.o"));

    let app = macho(&[
        (N_OSO, "/elsewhere/target/debug/deps/kept-a.cgu0.rcgu.o"),
        (N_SECT_EXT, "/x/target/debug/deps/stale-symbol.cgu0.rcgu.o"),
        (N_OSO, "/x/lib/libstd-9.rlib(stale-member.cgu0.rcgu.o)"),
        (N_OSO, "/x/target/debug/build/stale-notdeps.cgu0.rcgu.o"),
    ]);
    fs::write(deps.join("app-1111"), app).unwrap();
    // cargo hardlinks debug/<bin> to deps/<bin>-<hash>: one file, scanned once.
    fs::hard_link(deps.join("app-1111"), debug.join("app")).unwrap();
    fs::write(deps.join("tool-3333"), macho(&[(N_OSO, "/x/target/debug/deps/kept-c.cgu0.rcgu.o")])).unwrap();
    fs::write(examples.join("ex"), macho(&[(N_OSO, "/x/target/debug/deps/kept-b.cgu0.rcgu.o")])).unwrap();
    target
}

const ALL_DEPS: [&str; 13] = [
    "app-1111",
    "app-1111.d",
    "kept-a.cgu0.rcgu.o",
    "kept-b.cgu0.rcgu.o",
    "kept-c.cgu0.rcgu.o",
    "libdep-2222.rlib",
    "libdep-2222.rmeta",
    "plain.o",
    "stale-member.cgu0.rcgu.o",
    "stale-notdeps.cgu0.rcgu.o",
    "stale-old.cgu0.rcgu.o",
    "stale-symbol.cgu0.rcgu.o",
    "tool-3333",
];

fn all_deps() -> BTreeSet<String> {
    ALL_DEPS.iter().map(|n| n.to_string()).collect()
}

fn summary(target: &Path, scanned: usize, kept: usize, stale: usize, deleted: usize, dry: bool) -> String {
    let deps = target.join("debug/deps");
    format!(
        "prune-stale-objects: {scanned} Mach-O files scanned, {} .rcgu.o in {}: \
         {kept} referenced, {stale} stale, {deleted} deleted{}",
        kept + stale,
        deps.display(),
        if dry { " (dry run)" } else { "" }
    )
}

#[test]
fn prune_deletes_exactly_the_objects_no_current_binary_names() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    let output = prune(&uname_path(dir.path(), "Darwin"), &target, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));

    let mut expected = all_deps();
    for stale in [
        "stale-old.cgu0.rcgu.o",
        "stale-symbol.cgu0.rcgu.o",
        "stale-member.cgu0.rcgu.o",
        "stale-notdeps.cgu0.rcgu.o",
    ] {
        expected.remove(stale);
    }
    assert_eq!(names(&target.join("debug/deps")), expected, "{}", describe(&output));
    assert!(target.join("debug/outside.cgu0.rcgu.o").exists(), "a .rcgu.o outside deps was deleted");
    // Three distinct Mach-O files: app (twice, by hardlink), tool, ex.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        summary(&target, 3, 3, 4, 4, false),
        "{}",
        describe(&output)
    );

    // Nothing is left to prune: a second run deletes nothing.
    let again = prune(&uname_path(dir.path(), "Darwin"), &target, &[]);
    assert_eq!(String::from_utf8_lossy(&again.stdout).trim(), summary(&target, 3, 3, 0, 0, false));
}

#[test]
fn dry_run_deletes_nothing_and_prints_the_counts() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    let output = prune(&uname_path(dir.path(), "Darwin"), &target, &["--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(names(&target.join("debug/deps")), all_deps(), "{}", describe(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        summary(&target, 3, 3, 4, 0, true),
        "{}",
        describe(&output)
    );
}

#[test]
fn cargo_target_dir_selects_the_target_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let chosen = fake_target(&dir.path().join("chosen"));
    let other = fake_target(&dir.path().join("other"));
    let output = prune(&uname_path(dir.path(), "Darwin"), &chosen, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(!chosen.join("debug/deps/stale-old.cgu0.rcgu.o").exists(), "{}", describe(&output));
    assert_eq!(names(&other.join("debug/deps")), all_deps(), "another target dir was touched");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&chosen.join("debug/deps").display().to_string()),
        "{}",
        describe(&output)
    );

    // A CARGO_TARGET_DIR without debug/deps is a no-op, not an error.
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let output = prune(&uname_path(dir.path(), "Darwin"), &empty, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing to do"), "{}", describe(&output));
}

#[test]
fn off_macos_the_prune_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    let output = prune(&uname_path(dir.path(), "Linux"), &target, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not macOS"), "{}", describe(&output));
    assert_eq!(names(&target.join("debug/deps")), all_deps(), "{}", describe(&output));
}

#[test]
fn a_universal_binary_or_an_unknown_argument_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    let path_env = uname_path(dir.path(), "Darwin");

    let output = prune(&path_env, &target, &["--force"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"), "{}", describe(&output));
    assert_eq!(names(&target.join("debug/deps")), all_deps());

    // FAT_MAGIC, big-endian: a universal binary whose slices are not read.
    fs::write(target.join("debug/fat"), [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0]).unwrap();
    let output = prune(&path_env, &target, &[]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert_eq!(names(&target.join("debug/deps")), all_deps(), "{}", describe(&output));
}

// ---------------------------------------------------------------------------
// scripts/test-sections.sh run

/// A `PATH` with the `uname` stub and a `cargo` that records each call and
/// whether `probe` existed at that moment, in `<dir>/cargo-called`.
fn probing_cargo(dir: &Path, probe: &Path) -> String {
    let path_env = uname_path(dir, "Darwin");
    let bin = dir.join("cargo-bin");
    fs::create_dir_all(&bin).unwrap();
    write_exec(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\nif [ -e '{probe}' ]; then s=present; else s=absent; fi\necho \"$s $*\" >> '{log}'\nexit 0\n",
            probe = probe.display(),
            log = dir.join("cargo-called").display()
        ),
    );
    format!("{}:{path_env}", bin.display())
}

fn sections_run(path_env: &str, target: &Path) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/test-sections.sh"))
        .args(["run", "wire"])
        .env("PATH", path_env)
        .env("CARGO_TARGET_DIR", target)
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .expect("failed to run bash")
}

#[test]
fn sections_run_prunes_before_the_first_cargo_nextest_run() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    let stale = target.join("debug/deps/stale-old.cgu0.rcgu.o");
    let output = sections_run(&probing_cargo(dir.path(), &stale), &target);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));

    let log = fs::read_to_string(dir.path().join("cargo-called")).unwrap();
    let runs: Vec<&str> = log.lines().filter(|l| l.contains("nextest run")).collect();
    assert_eq!(runs.len(), 1, "{log}");
    assert!(runs[0].starts_with("absent "), "cargo nextest run saw the stale object:\n{log}");
    assert!(target.join("debug/deps/kept-a.cgu0.rcgu.o").exists(), "{log}");
}

#[test]
fn sections_run_stops_before_cargo_when_the_prune_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = fake_target(dir.path());
    fs::write(target.join("debug/fat"), [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0]).unwrap();
    let stale = target.join("debug/deps/stale-old.cgu0.rcgu.o");
    let output = sections_run(&probing_cargo(dir.path(), &stale), &target);
    assert!(!output.status.success(), "{}", describe(&output));
    let log = fs::read_to_string(dir.path().join("cargo-called")).unwrap_or_default();
    assert!(!log.contains("nextest run"), "cargo ran after a failed prune:\n{log}");
}

// ---------------------------------------------------------------------------
// A real crate, built twice

#[cfg(target_os = "macos")]
mod real_crate {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn cargo_build(krate: &Path, target: &Path) {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let output = Command::new(cargo)
            .args(["build", "--quiet", "--offline"])
            .current_dir(krate)
            .env("CARGO_TARGET_DIR", target)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", describe(&output));
    }

    fn rcgu_objects(deps: &Path) -> BTreeSet<String> {
        names(deps).into_iter().filter(|n| n.ends_with(".rcgu.o")).collect()
    }

    /// The loose objects `nm` reads from the binary's debug map: an oracle
    /// independent of the script's own Mach-O reader.
    fn nm_oso(binary: &Path) -> BTreeSet<String> {
        let output = Command::new("nm").arg("-ap").arg(binary).output().unwrap();
        assert!(output.status.success(), "{}", describe(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.split_once(" OSO ").map(|(_, p)| p.to_owned()))
            .filter(|p| !p.contains('('))
            .map(|p| Path::new(&p).file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn panic_backtrace(binary: &Path) -> String {
        let output = Command::new(binary).arg("panic").env("RUST_BACKTRACE", "1").output().unwrap();
        assert!(!output.status.success(), "{}", describe(&output));
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    #[test]
    fn a_rebuild_leaves_the_old_objects_and_the_prune_removes_only_them() {
        let dir = tempfile::tempdir().unwrap();
        let krate = dir.path().join("tiny");
        let target = dir.path().join("target");
        fs::create_dir_all(krate.join("src")).unwrap();
        fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        let main = krate.join("src/main.rs");
        fs::write(
            &main,
            "fn main() {\n    if std::env::args().count() > 1 {\n        explode();\n    }\n}\n\n\
             #[inline(never)]\nfn explode() {\n    panic!(\"boom\");\n}\n",
        )
        .unwrap();
        let deps = target.join("debug/deps");
        let binary = target.join("debug/tiny");

        cargo_build(&krate, &target);
        let first = rcgu_objects(&deps);
        // Touch: a later mtime, so cargo recompiles the same source.
        let later = SystemTime::now() + Duration::from_secs(5);
        fs::File::options().write(true).open(&main).unwrap().set_modified(later).unwrap();
        cargo_build(&krate, &target);
        let both = rcgu_objects(&deps);
        let referenced = nm_oso(&binary);

        assert!(!referenced.is_empty(), "the binary names no loose objects");
        assert!(referenced.is_subset(&both), "nm names objects that do not exist");
        assert!(
            referenced.is_disjoint(&first),
            "the rebuild reused a first-build object name; the fixture no longer shows growth"
        );
        let second: BTreeSet<String> = both.difference(&first).cloned().collect();
        assert_eq!(second, referenced, "the rebuild wrote objects the binary does not name");

        let output = prune(&std::env::var("PATH").unwrap_or_default(), &target, &[]);
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        assert_eq!(rcgu_objects(&deps), referenced, "{}", describe(&output));
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!(
                "{} referenced, {} stale, {} deleted",
                referenced.len(),
                first.len(),
                first.len()
            )),
            "{}",
            describe(&output)
        );

        // The objects the debug map names are where file:line comes from.
        let trace = panic_backtrace(&binary);
        assert!(trace.contains("src/main.rs:9:"), "the backtrace lost file:line after the prune:\n{trace}");
    }
}
