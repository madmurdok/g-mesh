//! Which build a running daemon came from, published next to its socket so
//! anything else on the machine can tell whether that daemon is still the
//! current one.
//!
//! # The failure this closes
//!
//! `storage::schema::ensure_current` throws away an index a previous
//! generation of the pipeline built - but it is called exactly once, in
//! `daemon::run`, at startup. Daemons are long-lived and the shim connects to
//! whatever incumbent is listening, so installing a new g-mesh changes nothing
//! until that process happens to die. Task 96 fixed the index never catching
//! up; this fixes the process never catching up, which is the same shape one
//! level higher: the check is correct, nothing schedules it.
//!
//! # Why the executable, and not a version constant
//!
//! The obvious candidate was `CURRENT_INDEXER_VERSION` - the daemon could
//! publish the generation it was serving and a shim could compare. It is the
//! right granularity for *the index* and deliberately so (see that constant's
//! own doc comment: it moves only when extraction or linking would produce a
//! different graph). That makes it the wrong granularity for *the process*.
//! A release can rewrite how results are paginated, truncated or described
//! without changing a single node or edge, and correctly leave the indexer
//! generation alone - and a daemon left running across that release goes on
//! serving the old query code with every stamp still matching. Keying off it
//! would rebuild the failure this module exists to end, only rarer and so
//! harder to spot.
//!
//! `env!("CARGO_PKG_VERSION")` fails the other way: dev builds between
//! releases all carry the same string, and most fixes ship without a version
//! bump of their own.
//!
//! So the stamp is about the executable itself - the one thing that is always
//! different when the code is different, needs no discipline to maintain, and
//! cannot silently agree when it should not. Its mtime is the comparison;
//! its path is recorded only so a human reading `g-mesh status` can see which
//! binary they are actually talking to.
//!
//! # Newer wins, never older
//!
//! [`vintage`] reports `Outdated` only when the incumbent's executable is
//! strictly *older* than the asking one, not merely different. Plain
//! inequality would let two builds used against the same project - an
//! installed `g-mesh` and a `cargo build` of the same tree, say - retire each
//! other's daemon on every single call, turning a one-time upgrade cost into a
//! permanent thrash. Ordering makes the mechanism monotonic: a project's
//! daemon only ever moves forward, and a shim from an older build recognizes
//! that it, not the daemon, is the stale one. Deliberately rolling back to an
//! older binary is therefore not detected - `g-mesh stop` is the answer there,
//! and `g-mesh status` says which build is running.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::watcher::staleness::mtime_millis;

/// Keys written into the stamp file. Spelled out rather than derived so the
/// file stays a stable, greppable contract between two processes that may be
/// different builds of g-mesh.
const EXE_KEY: &str = "exe";
const EXE_MTIME_KEY: &str = "exe_mtime_millis";

/// The build a daemon is running, as it describes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStamp {
    /// Where the executable was when the daemon started. Diagnostic only -
    /// see this module's docs for why the path takes no part in the
    /// comparison.
    pub exe: PathBuf,
    /// The executable's mtime, in the same milliseconds-since-epoch form
    /// `indexed_files.mtimeMillis` uses, so there is one mtime convention in
    /// this codebase rather than two.
    pub exe_mtime_millis: i64,
}

/// How a running daemon's build compares with the build asking about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vintage {
    /// The daemon started from this same executable, or from one built after
    /// it. Either way it is not holding an upgrade back.
    Current,
    /// The daemon started from an executable built before this one: it
    /// predates whatever was installed since, index invalidation included.
    Outdated,
    /// The daemon published no readable stamp. Every daemon running before
    /// this file existed reads this way, and every one of them is by
    /// construction older than the build that introduced it - so callers
    /// treat it exactly like `Outdated`. It is kept distinct only so
    /// `g-mesh status` can say "cannot be compared" instead of inventing a
    /// comparison it did not make.
    Unknown,
}

/// Describes the build of the currently running process.
///
/// Fallible on purpose: a caller that cannot describe its own executable has
/// no evidence about anything and must fall back to the behavior it had
/// before this check existed, rather than guess.
pub fn of_running_process() -> Result<BuildStamp> {
    let exe = std::env::current_exe().context("failed to resolve this process's executable")?;
    let metadata = fs::metadata(&exe)
        .with_context(|| format!("failed to stat the executable at {}", exe.display()))?;
    let exe_mtime_millis = mtime_millis(&metadata)
        .with_context(|| format!("failed to read the mtime of {}", exe.display()))?;
    Ok(BuildStamp { exe, exe_mtime_millis })
}

/// Publishes `stamp` at `path`, replacing whatever was there.
pub fn write(path: &Path, stamp: &BuildStamp) -> Result<()> {
    let body = format!(
        "{EXE_KEY}={}\n{EXE_MTIME_KEY}={}\n",
        stamp.exe.display(),
        stamp.exe_mtime_millis
    );
    fs::write(path, body)
        .with_context(|| format!("failed to write the daemon build stamp at {}", path.display()))
}

/// Reads a published stamp back.
///
/// `None` covers absent, unreadable and malformed alike, because all three
/// mean the same thing to every caller - nothing trustworthy is on record -
/// and all three resolve to [`Vintage::Unknown`], which fails towards
/// replacing the daemon rather than towards trusting it.
pub fn read(path: &Path) -> Option<BuildStamp> {
    let body = fs::read_to_string(path).ok()?;

    let mut exe = None;
    let mut exe_mtime_millis = None;
    for line in body.lines() {
        // Split on the first `=` only: an executable path may well contain
        // one, and it is the value, not the key, that gets to be arbitrary.
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            EXE_KEY => exe = Some(PathBuf::from(value)),
            EXE_MTIME_KEY => exe_mtime_millis = value.parse().ok(),
            // Ignored rather than rejected, so a future build may add a field
            // without every older shim reading the whole stamp as garbage.
            _ => {}
        }
    }

    Some(BuildStamp { exe: exe?, exe_mtime_millis: exe_mtime_millis? })
}

/// Compares the stamp a running daemon published (`None` if it published
/// none) against the build asking.
pub fn vintage(incumbent: Option<&BuildStamp>, ours: &BuildStamp) -> Vintage {
    let Some(incumbent) = incumbent else {
        return Vintage::Unknown;
    };
    if incumbent.exe_mtime_millis < ours.exe_mtime_millis {
        Vintage::Outdated
    } else {
        Vintage::Current
    }
}

/// One clause naming a verdict, for the warning `shim::retire_outdated_daemon`
/// prints before it acts on one. Total rather than defined only over the two
/// vintages that get acted on, so a caller cannot be surprised by a variant it
/// forgot. (`cli::status` deliberately words its own line differently: it is
/// telling someone what to *do*, not narrating something already under way.)
pub fn describe(vintage: Vintage) -> &'static str {
    match vintage {
        Vintage::Current => "started from this build",
        Vintage::Outdated => "started from an older build of g-mesh",
        Vintage::Unknown => "published no build stamp, so it predates this check",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(exe: &str, mtime: i64) -> BuildStamp {
        BuildStamp { exe: PathBuf::from(exe), exe_mtime_millis: mtime }
    }

    #[test]
    fn a_published_stamp_reads_back_exactly_as_it_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.build");
        let written = stamp("/usr/local/bin/g-mesh", 1_753_900_000_000);

        write(&path, &written).unwrap();

        assert_eq!(read(&path), Some(written));
    }

    /// The value side of a line is a path, and paths are allowed to contain
    /// the separator this format uses.
    #[test]
    fn an_executable_path_containing_an_equals_sign_survives_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.build");
        let written = stamp("/opt/build=2/g-mesh", 7);

        write(&path, &written).unwrap();

        assert_eq!(read(&path), Some(written));
    }

    #[test]
    fn a_stamp_gaining_a_field_is_still_readable_by_a_build_that_does_not_know_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.build");
        fs::write(&path, "exe=/bin/g-mesh\nexe_mtime_millis=42\ncommit=deadbeef\n").unwrap();

        assert_eq!(read(&path), Some(stamp("/bin/g-mesh", 42)));
    }

    #[test]
    fn an_absent_incomplete_or_unparseable_stamp_reads_as_nothing_on_record() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(read(&dir.path().join("absent")), None);

        for (name, body) in [
            ("empty", ""),
            ("no-mtime", "exe=/bin/g-mesh\n"),
            ("no-exe", "exe_mtime_millis=42\n"),
            ("mtime-not-a-number", "exe=/bin/g-mesh\nexe_mtime_millis=soon\n"),
            ("not-key-values", "g-mesh 0.5.0\n"),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, body).unwrap();
            assert_eq!(read(&path), None, "{name} must not read as a usable stamp");
        }
    }

    #[test]
    fn a_daemon_from_an_earlier_build_is_outdated() {
        let ours = stamp("/bin/g-mesh", 2_000);
        assert_eq!(vintage(Some(&stamp("/bin/g-mesh", 1_000)), &ours), Vintage::Outdated);
    }

    #[test]
    fn a_daemon_from_this_very_executable_is_current() {
        let ours = stamp("/bin/g-mesh", 2_000);
        assert_eq!(vintage(Some(&ours.clone()), &ours), Vintage::Current);
    }

    /// The rule that stops two builds used against one project from retiring
    /// each other's daemon forever: a shim never downgrades what is running.
    #[test]
    fn a_daemon_from_a_later_build_is_current_rather_than_something_to_replace() {
        let ours = stamp("/bin/g-mesh", 2_000);
        let newer = stamp("/somewhere/else/g-mesh", 9_000);
        assert_eq!(vintage(Some(&newer), &ours), Vintage::Current);
    }

    /// Two installs of the same age are not evidence of anything, and the
    /// path deliberately takes no part in the comparison.
    #[test]
    fn two_executables_of_the_same_age_are_not_treated_as_an_upgrade() {
        let ours = stamp("/usr/local/bin/g-mesh", 2_000);
        let other = stamp("/home/u/.cargo/bin/g-mesh", 2_000);
        assert_eq!(vintage(Some(&other), &ours), Vintage::Current);
    }

    #[test]
    fn a_daemon_that_published_nothing_cannot_be_vouched_for() {
        assert_eq!(vintage(None, &stamp("/bin/g-mesh", 2_000)), Vintage::Unknown);
    }

    /// The stamp a live process writes about itself has to satisfy its own
    /// check, or every shim would retire every daemon on every call.
    #[test]
    fn the_running_process_describes_itself_as_current() {
        let ours = of_running_process().expect("a running process can always be described");
        assert_eq!(vintage(Some(&ours), &ours), Vintage::Current);
        assert!(ours.exe.exists(), "the recorded executable must be the one running");
    }
}
