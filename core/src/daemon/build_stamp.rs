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
//! # Why the executable is not enough on its own
//!
//! Half the pipeline is not in the executable. The JS/TS plugin is a separate
//! process built by `npm`, and *everything* in the index is computed by it -
//! so `cd plugins/typescript && npm run build` after an extractor change leaves a
//! running daemon holding a plugin whose logic no longer exists anywhere on
//! disk, with an untouched core binary vouching for it. Task 116 measured
//! exactly that: task 115 changed how same-file edges resolve, and the daemon
//! went on serving the old resolution with `g-mesh status` reporting "this
//! build", because that was true and beside the point.
//!
//! So the stamp carries a second fact, `daemon::plugin::fingerprint` - a
//! digest of the plugin's compiled output, derived from the artifact for the
//! same reason the exe's mtime is. The two are read together by [`vintage`]
//! and each catches what the other cannot.
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
//!
//! The plugin fingerprint has no ordering to fall back on - two digests are
//! equal or they are not - so it is consulted only when the exe comparison
//! found no difference to act on, i.e. when the incumbent's executable is the
//! same age as this one's and so, to every practical purpose, is it. That
//! keeps the whole mechanism monotonic: a shim never retires a daemon from a
//! core build ahead of its own, whatever plugin either of them is holding, and
//! two builds of different ages go on settling it by age alone. The case the
//! fingerprint is for needs nothing more, because a plugin-only rebuild leaves
//! the core binary untouched by construction.
//!
//! (Two *different* executables that happen to share an mtime to the
//! millisecond, shipping different plugins, would retire each other - the same
//! thrash the ordering rule exists to prevent, reachable only by a collision
//! this comparison has always been willing to live with: it already reads such
//! a pair as one build.)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::daemon::plugin;
use crate::watcher::staleness::mtime_millis;

/// Keys written into the stamp file. Spelled out rather than derived so the
/// file stays a stable, greppable contract between two processes that may be
/// different builds of g-mesh.
const EXE_KEY: &str = "exe";
const EXE_MTIME_KEY: &str = "exe_mtime_millis";
const PLUGIN_KEY: &str = "plugin_fingerprint";

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
    /// The JS/TS plugin build this process would run, per
    /// `daemon::plugin::fingerprint` - the half of the pipeline the
    /// executable says nothing about.
    pub plugin: String,
}

/// How a running daemon's build compares with the build asking about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vintage {
    /// The daemon started from this same executable and the same plugin
    /// build, or from an executable built after this one. Either way it is
    /// not holding an upgrade back.
    Current,
    /// The daemon started from an executable built before this one: it
    /// predates whatever was installed since, index invalidation included.
    Outdated,
    /// Same executable, different JS/TS plugin: the daemon holds a plugin
    /// build that has since been replaced, so every node and edge it is
    /// serving was computed by extraction logic no longer on disk. Acted on
    /// exactly like `Outdated` - kept apart so nothing tells a human their
    /// core binary is old when it is the one they just built.
    PluginChanged,
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
    // Infallible on purpose, unlike the two above: a plugin that cannot be
    // read at all resolves to `FINGERPRINT_UNAVAILABLE`, which compares equal
    // to the same answer from anyone else, so an install with no readable
    // plugin behaves exactly as it did before this field existed instead of
    // making every process look like a change.
    let plugin = plugin::bundled_fingerprint().to_string();
    Ok(BuildStamp { exe, exe_mtime_millis, plugin })
}

/// Publishes `stamp` at `path`, replacing whatever was there.
pub fn write(path: &Path, stamp: &BuildStamp) -> Result<()> {
    let body = format!(
        "{EXE_KEY}={}\n{EXE_MTIME_KEY}={}\n{PLUGIN_KEY}={}\n",
        stamp.exe.display(),
        stamp.exe_mtime_millis,
        stamp.plugin
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
///
/// A stamp from a build that predates the plugin fingerprint is malformed by
/// this definition, and deliberately so: it is a partial account of a build,
/// and every daemon that published one is older than the build that started
/// requiring it. Reading it as `Unknown` retires those daemons once, which is
/// the same one-off cost this file's introduction charged.
pub fn read(path: &Path) -> Option<BuildStamp> {
    let body = fs::read_to_string(path).ok()?;

    let mut exe = None;
    let mut exe_mtime_millis = None;
    let mut plugin = None;
    for line in body.lines() {
        // Split on the first `=` only: an executable path may well contain
        // one, and it is the value, not the key, that gets to be arbitrary.
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            EXE_KEY => exe = Some(PathBuf::from(value)),
            EXE_MTIME_KEY => exe_mtime_millis = value.parse().ok(),
            PLUGIN_KEY => plugin = Some(value.to_string()),
            // Ignored rather than rejected, so a future build may add a field
            // without every older shim reading the whole stamp as garbage.
            _ => {}
        }
    }

    Some(BuildStamp { exe: exe?, exe_mtime_millis: exe_mtime_millis?, plugin: plugin? })
}

/// Compares the stamp a running daemon published (`None` if it published
/// none) against the build asking.
///
/// The executable is judged first and by ordering, the plugin second and by
/// equality - see this module's docs for why the second comparison is reached
/// only when the first found the two executables to be the same one.
pub fn vintage(incumbent: Option<&BuildStamp>, ours: &BuildStamp) -> Vintage {
    let Some(incumbent) = incumbent else {
        return Vintage::Unknown;
    };
    if incumbent.exe_mtime_millis < ours.exe_mtime_millis {
        return Vintage::Outdated;
    }
    if incumbent.exe_mtime_millis == ours.exe_mtime_millis && incumbent.plugin != ours.plugin {
        return Vintage::PluginChanged;
    }
    Vintage::Current
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
        Vintage::PluginChanged => "is holding a JS/TS plugin build that has since been rebuilt",
        Vintage::Unknown => "published no build stamp, so it predates this check",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(exe: &str, mtime: i64) -> BuildStamp {
        stamp_with_plugin(exe, mtime, "0123456789abcdef")
    }

    fn stamp_with_plugin(exe: &str, mtime: i64, plugin: &str) -> BuildStamp {
        BuildStamp {
            exe: PathBuf::from(exe),
            exe_mtime_millis: mtime,
            plugin: plugin.to_string(),
        }
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
        fs::write(
            &path,
            "exe=/bin/g-mesh\nexe_mtime_millis=42\nplugin_fingerprint=0123456789abcdef\ncommit=deadbeef\n",
        )
        .unwrap();

        assert_eq!(read(&path), Some(stamp("/bin/g-mesh", 42)));
    }

    #[test]
    fn an_absent_incomplete_or_unparseable_stamp_reads_as_nothing_on_record() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(read(&dir.path().join("absent")), None);

        for (name, body) in [
            ("empty", ""),
            ("no-mtime", "exe=/bin/g-mesh\nplugin_fingerprint=ab\n"),
            ("no-exe", "exe_mtime_millis=42\nplugin_fingerprint=ab\n"),
            (
                "mtime-not-a-number",
                "exe=/bin/g-mesh\nexe_mtime_millis=soon\nplugin_fingerprint=ab\n",
            ),
            ("not-key-values", "g-mesh 0.5.0\n"),
            // The shape every daemon started before task 116 published: a
            // complete account of the executable and none at all of the
            // plugin, which is half the pipeline.
            ("no-plugin-fingerprint", "exe=/bin/g-mesh\nexe_mtime_millis=42\n"),
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

    /// Task 116's headline case: `npm run build` in the plugin, nothing else.
    /// The core binary is untouched - which is exactly why nothing noticed
    /// before - so the only evidence there is, is the fingerprint.
    #[test]
    fn a_daemon_holding_a_plugin_that_has_since_been_rebuilt_is_not_current() {
        let ours = stamp_with_plugin("/bin/g-mesh", 2_000, "after0000000000");
        let incumbent = stamp_with_plugin("/bin/g-mesh", 2_000, "before000000000");

        assert_eq!(vintage(Some(&incumbent), &ours), Vintage::PluginChanged);
    }

    /// The other half of "content, not mtime": a rebuild that emitted the
    /// same bytes changed nothing anyone can observe, and must not cost a
    /// project a re-walk.
    #[test]
    fn a_rebuilt_but_identical_plugin_leaves_the_daemon_alone() {
        let ours = stamp_with_plugin("/bin/g-mesh", 2_000, "same000000000000");
        let incumbent = stamp_with_plugin("/bin/g-mesh", 2_000, "same000000000000");

        assert_eq!(vintage(Some(&incumbent), &ours), Vintage::Current);
    }

    /// Monotonicity, which the plugin comparison must not break: an installed
    /// build and a dev build of the same tree ship different plugins, and if
    /// mere inequality were enough they would retire each other's daemon on
    /// every call forever. The exe ordering still decides, and only a tie on
    /// it - the same executable, so genuinely a plugin-only rebuild - lets the
    /// fingerprint speak.
    #[test]
    fn a_daemon_from_a_later_build_is_left_alone_however_its_plugin_compares() {
        let ours = stamp_with_plugin("/bin/g-mesh", 2_000, "ours000000000000");
        let newer = stamp_with_plugin("/somewhere/else/g-mesh", 9_000, "theirs0000000000");

        assert_eq!(vintage(Some(&newer), &ours), Vintage::Current);
    }

    /// An older executable is reported as the older executable it is, not as
    /// a plugin change that happens to come with it - the two verdicts are
    /// told apart so nothing tells a human the wrong thing to go and look at.
    #[test]
    fn an_older_executable_is_named_as_such_even_when_the_plugin_differs_too() {
        let ours = stamp_with_plugin("/bin/g-mesh", 2_000, "ours000000000000");
        let older = stamp_with_plugin("/bin/g-mesh", 1_000, "theirs0000000000");

        assert_eq!(vintage(Some(&older), &ours), Vintage::Outdated);
    }

    /// Every verdict says something different, or `describe` is telling
    /// someone to go and look at the wrong thing.
    #[test]
    fn each_verdict_is_described_in_its_own_words() {
        let described = [Vintage::Current, Vintage::Outdated, Vintage::PluginChanged, Vintage::Unknown]
            .map(describe);
        let unique: std::collections::HashSet<_> = described.iter().collect();
        assert_eq!(unique.len(), described.len(), "{described:?}");
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
