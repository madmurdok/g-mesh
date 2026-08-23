//! `g-mesh model fetch` / `g-mesh model status`: acquiring the embedding
//! weights `search_code` needs, from the binary itself.
//!
//! # Why this exists
//!
//! The weights are ~612 MiB and are not vendored (see
//! [`crate::embedding::model`]). Until this command existed, the only way to
//! get them was `core/scripts/fetch-embedding-model.sh`, which lives in this
//! repository - fine for someone who cloned it, and nothing at all for someone
//! who installed a released binary. That would have shipped `search_code` as a
//! tool an installed user structurally cannot enable.
//!
//! # The invariant this module must not break
//!
//! g-mesh never reaches the network on its own. Not the daemon, not the shim,
//! not indexing, not `search_code` - a missing model is reported as "semantic
//! search is unavailable", never quietly repaired by a download. That is a
//! promise about a tool that reads people's source code, so it is enforced
//! structurally rather than by intent:
//!
//! - The HTTP client lives *only* here. `ureq` is imported in this file and
//!   nowhere else in the crate, which this module's own
//!   `the_http_client_is_reachable_from_nowhere_but_this_module` test asserts
//!   by scanning every other source file. A future "the model is missing, let
//!   me just fetch it" call site inside the daemon cannot be written without
//!   failing that test.
//! - Everything below except [`run`] is private, so even a deliberate
//!   `crate::cli::model::...` from daemon code has nothing to call but the
//!   whole user-facing command, printing progress to a terminal that isn't
//!   there.
//! - The dependency direction agrees: `cli` already depends on `daemon` and
//!   `shim` (its `dispatch` hands commands to both), so `daemon` importing
//!   `cli` would be a cycle in the module graph, not a small edit.
//!
//! # What is downloaded
//!
//! Exactly what `core/scripts/fetch-embedding-model.sh` downloads, from the
//! same pinned revision into the same directory - the script and this command
//! coexist, and `the_shell_script_and_this_command_agree_on_the_pinned_revision`
//! fails if they ever drift apart. The target directory comes from
//! [`crate::embedding::resolve_model_dir`], the loader's own resolution, so
//! "downloaded successfully" and "model not found" cannot both be true.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::cli::ModelCommand;
use crate::config::EmbeddingConfig;
use crate::embedding::model::{ONNX_FILE_NAME, TOKENIZER_FILE_NAME};
use crate::embedding::resolve_model_dir;

/// The Hugging Face repository the default model comes from.
const MODEL_REPO: &str = "jinaai/jina-embeddings-v2-base-code";

/// Pinned, and pinned to a commit hash rather than a branch: these are the
/// exact weights the embedding tests were verified against, and bumping this
/// changes every vector the model produces, invalidating any index built with
/// the old one (REQUIREMENTS.md, "Инвалидация эмбеддингов при смене
/// embedding-модели"). An immutable revision is also what makes the digests
/// below meaningful - the bytes at this commit cannot legitimately change.
const MODEL_REVISION: &str = "516f4baf13dec4ddddda8631e019b5737c8bc250";

/// How long to wait for the connection itself. Deliberately *not* a deadline
/// on the whole request: 612 MiB over a slow link legitimately takes many
/// minutes, and a total timeout would turn a working download into a failure
/// for exactly the users who can least afford to repeat it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A stalled socket, on the other hand, should not hang forever. This bounds a
/// single read, not the transfer.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// One file of the model, with everything needed to verify it arrived intact.
struct RemoteFile {
    /// Path inside the model repository at [`MODEL_REVISION`].
    remote_path: &'static str,
    /// Name inside the model directory - taken from the loader's constants,
    /// never spelled out here.
    local_name: &'static str,
    size: u64,
    /// SHA-256 of the file's bytes.
    ///
    /// Both digests were confirmed from two independent sources before being
    /// pinned: Hugging Face's own metadata API for this revision, and hashing
    /// the files after download (for `tokenizer.json`, which is not stored in
    /// LFS and therefore has no published digest, the git blob id of the
    /// downloaded bytes was recomputed and matched the revision's tree, then
    /// its SHA-256 taken locally).
    sha256: &'static str,
}

/// The two files a model directory consists of, in download order: the small
/// one last, so an interrupted run leaves the cheap file to redo.
const FILES: [RemoteFile; 2] = [
    // The fp32 export, not model_fp16/model_quantized: those trade accuracy
    // for size and produce different vectors than the ones the tests pin.
    RemoteFile {
        remote_path: "onnx/model.onnx",
        local_name: ONNX_FILE_NAME,
        size: 641_517_466,
        sha256: "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
    },
    RemoteFile {
        remote_path: "tokenizer.json",
        local_name: TOKENIZER_FILE_NAME,
        size: 2_561_316,
        sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
    },
];

/// Runs `g-mesh model <subcommand>`.
pub fn run(command: &ModelCommand) -> Result<()> {
    let stdout = std::io::stdout();
    match command {
        ModelCommand::Fetch { dir } => fetch(dir.as_deref(), &mut stdout.lock()),
        ModelCommand::Status { dir } => status(dir.as_deref(), &mut stdout.lock()),
    }
}

/// Which model this invocation is about, and where that name came from.
///
/// The provenance is half the answer, not decoration: the failure this closes
/// is `model status` and the daemon reporting different things while neither
/// mentions why, so every message here says which name it resolved and what
/// decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedModel {
    name: String,
    source: ModelSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSource {
    /// No project config named one - the fresh-machine case this command
    /// exists for, and the only one [`fetch`] can serve.
    BuiltInDefault,
    /// A project's `[embedding] model` names something else.
    ProjectConfig,
}

impl ModelSource {
    fn describe(self) -> &'static str {
        match self {
            Self::BuiltInDefault => "g-mesh's default",
            Self::ProjectConfig => "this project's config.toml",
        }
    }
}

/// [`resolved_model_in`] for the current directory.
///
/// The current directory *is* the project, with no walking up to find a root -
/// which is not a choice made here but the convention every other command in
/// this module tree already follows (`cli::status`, `cli::init`, `cli::clean`,
/// `cli::stop`, `cli::config_wizard` all read `current_dir` and stop). So
/// running this from a subdirectory resolves to the default, exactly as
/// `g-mesh status` there reports no project. Changing that is a change to all
/// of them at once, not to this one.
///
/// The cwd read is one line, and it is here rather than inside the function
/// below so that everything with a decision in it can be tested: `current_dir`
/// is process-global, and a test that changed it would race the other tests
/// cargo runs beside it in the same process.
fn resolved_model() -> ResolvedModel {
    match std::env::current_dir() {
        Ok(cwd) => resolved_model_in(&cwd),
        // This command has to work from anywhere; an unreadable cwd is not a
        // reason to fail it.
        Err(_) => {
            ResolvedModel { name: EmbeddingConfig::default().model, source: ModelSource::BuiltInDefault }
        }
    }
}

/// Reads `root`'s `[embedding] model`, falling back to the built-in default.
///
/// A project with no config of its own reads as [`ModelSource::BuiltInDefault`]
/// rather than as a configured default, because the two are the same fact and
/// distinguishing them would put a difference in the output that means nothing
/// to whoever reads it. An unreadable config resolves the same way for the same
/// reason a missing one does: this command is run outside any project as a
/// matter of course.
fn resolved_model_in(root: &Path) -> ResolvedModel {
    let default = EmbeddingConfig::default().model;
    match crate::config::read_project_config(root).map(|c| c.embedding.model) {
        Ok(name) if name != default => ResolvedModel { name, source: ModelSource::ProjectConfig },
        _ => ResolvedModel { name: default, source: ModelSource::BuiltInDefault },
    }
}

/// Why this invocation cannot fetch, or `None` if it can.
///
/// The URLs, revision and digests in this file describe the default model and
/// nothing else, so "download the configured one" is not something this command
/// could do even in principle - which was the original reasoning for reading the
/// default name, and it still holds. What does not follow is fetching the wrong
/// weights anyway: 612 MiB landing in a directory the daemon never reads, with
/// `model status` then inspecting that same wrong directory and reporting
/// success.
///
/// An explicit `--dir` (or `G_MESH_MODEL_DIR`, which `resolve_model_dir`
/// honours) is a person saying where to put the default weights, not a config
/// being ignored, so it still works.
fn fetch_refusal(explicit: Option<&Path>, model: &ResolvedModel, daemon_dir: &Path) -> Option<String> {
    if explicit.is_some() || model.source == ModelSource::BuiltInDefault {
        return None;
    }
    Some(format!(
        "this project is configured to use the embedding model '{}' (from {}), but `model fetch` \
         can only download g-mesh's default, '{}'.\n\n\
         The daemon will look for weights in:\n  {}\n\n\
         Put that model's files there yourself, or point this command somewhere explicitly with \
         `--dir`. Fetching now would download the default model into a directory the daemon never \
         reads.",
        model.name,
        model.source.describe(),
        EmbeddingConfig::default().model,
        daemon_dir.display(),
    ))
}

/// Where the weights belong for this invocation - the directory the *daemon*
/// would read, which is the only one worth reporting.
///
/// `resolve_model_dir`'s own doc comment says the writer and the reader
/// disagreeing about the location is "the one failure neither side can detect"
/// and that only one function may own the answer. That was true, and the
/// disagreement was happening one level up: `embedding::pipeline` passed it the
/// *configured* name while this file passed it the *default* one, so the single
/// owner was being asked two different questions and honestly gave two
/// different answers. This passes the same name the daemon does.
fn model_dir(explicit: Option<&Path>, model: &ResolvedModel) -> Result<PathBuf> {
    resolve_model_dir(explicit, &model.name)
}

/// `g-mesh model fetch`: downloads whatever is missing from the model
/// directory, verifies it, and leaves nothing behind if it fails.
///
/// A file that is already there is left alone rather than re-downloaded, so
/// re-running after an interrupted fetch only pays for what is still missing.
/// Existence is the test, not size or digest: verifying 612 MiB on every
/// invocation to catch a case `g-mesh model status` already reports (and
/// which deleting the file fixes) would cost every user for a rare one.
fn fetch(explicit: Option<&Path>, out: &mut impl Write) -> Result<()> {
    let model = resolved_model();
    // Before the directory is created, so a refused fetch leaves nothing
    // behind - the same posture the rest of this command already takes.
    if let Some(refusal) = fetch_refusal(explicit, &model, &model_dir(None, &model)?) {
        bail!("{refusal}");
    }

    let dir = prepare_dir(explicit, &model)?;
    writeln!(out, "model directory: {}", dir.display())?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(concat!("g-mesh/", env!("CARGO_PKG_VERSION")))
        .build();

    for file in &FILES {
        let dest = dir.join(file.local_name);
        if dest.exists() {
            writeln!(out, "already present: {}", dest.display())?;
            continue;
        }
        writeln!(
            out,
            "downloading {} ({}) from {MODEL_REPO}@{}",
            file.local_name,
            human_size(file.size),
            &MODEL_REVISION[..7]
        )?;
        download(&agent, file, &dest)?;
        writeln!(out, "  verified {}", file.local_name)?;
    }

    writeln!(out, "\nmodel ready in {}", dir.display())?;
    Ok(())
}

/// Resolves the model directory and makes sure it exists.
///
/// Split out of [`fetch`] so the part that touches the filesystem can be
/// tested without the part that touches the network - a test calling `fetch`
/// on an empty directory would start a 612 MiB download.
fn prepare_dir(explicit: Option<&Path>, model: &ResolvedModel) -> Result<PathBuf> {
    let dir = model_dir(explicit, model)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create model directory {}", dir.display()))?;
    Ok(dir)
}

/// `g-mesh model status`: says where the weights are expected and whether they
/// are there.
///
/// Checks presence and size only - never digests. Hashing 612 MiB to answer
/// "do I have the model?" would make the cheap question expensive; the size
/// check is enough to catch the one damaged state this command can produce
/// (a file restored from a truncated backup, say), and [`fetch`] verifies the
/// digest at the only moment the bytes are actually in hand.
fn status(explicit: Option<&Path>, out: &mut impl Write) -> Result<()> {
    let model = resolved_model();
    let dir = model_dir(explicit, &model)?;
    writeln!(out, "model:     {} (from {})", model.name, model.source.describe())?;
    writeln!(out, "revision:  {MODEL_REVISION}")?;
    writeln!(out, "directory: {}", dir.display())?;
    // Said here, where the reader is already looking at a directory that is
    // about to be reported empty. Without it the report is accurate and
    // useless: the files are genuinely missing, and nothing on screen explains
    // that `model fetch` cannot be the answer.
    if fetch_refusal(explicit, &model, &dir).is_some() {
        writeln!(
            out,
            "  note: `model fetch` downloads only g-mesh's default ('{}'), so it cannot fill this \
             directory - supply these weights yourself.",
            EmbeddingConfig::default().model
        )?;
    }

    let mut complete = true;
    for file in &FILES {
        let path = dir.join(file.local_name);
        match fs::metadata(&path) {
            Ok(meta) if meta.len() == file.size => {
                writeln!(out, "  {}: present ({})", file.local_name, human_size(meta.len()))?;
            }
            Ok(meta) => {
                complete = false;
                writeln!(
                    out,
                    "  {}: {} on disk but {} expected - delete it and re-run `g-mesh model fetch`",
                    file.local_name,
                    human_size(meta.len()),
                    human_size(file.size)
                )?;
            }
            Err(_) => {
                complete = false;
                writeln!(out, "  {}: missing", file.local_name)?;
            }
        }
    }

    if complete {
        writeln!(out, "\nsemantic search can use this model.")?;
    } else {
        writeln!(out, "\nrun `g-mesh model fetch` to download it.")?;
    }
    Ok(())
}

/// Downloads one file to `<dest>.partial`, verifies it, and only then renames
/// it into place.
///
/// The two-step is the whole point. If an interrupted download could leave
/// bytes under the final name, the very next run would see the file, call it
/// present and skip it - and the failure would surface much later as an ONNX
/// parse error inside the daemon, or, for a half-written `tokenizer.json`, as
/// vectors that are quietly wrong rather than as an error at all. So the final
/// name never exists until the bytes have been counted and hashed, and a
/// failed attempt deletes its own `.partial` instead of leaving debris. A
/// process killed mid-download still leaves a `.partial`; that is the point -
/// it is not a name anything else looks for.
fn download(agent: &ureq::Agent, file: &RemoteFile, dest: &Path) -> Result<()> {
    let partial = dest.with_file_name(format!("{}.partial", file.local_name));
    let result = download_to_partial(agent, file, &partial);
    if result.is_err() {
        // Best-effort: if this fails too there is nothing useful left to do,
        // and the message about *why* the download failed is worth more than
        // one about the leftover file.
        let _ = fs::remove_file(&partial);
    }
    result?;

    fs::rename(&partial, dest)
        .with_context(|| format!("failed to move {} into place as {}", partial.display(), dest.display()))
}

/// Streams the response into `partial`, hashing as it goes, and fails if what
/// arrived is not byte-for-byte what [`MODEL_REVISION`] pins.
fn download_to_partial(agent: &ureq::Agent, file: &RemoteFile, partial: &Path) -> Result<()> {
    let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_REVISION}/{}", file.remote_path);
    let response = agent.get(&url).call().with_context(|| format!("failed to download {url}"))?;

    let mut source = response.into_reader();
    let mut sink =
        File::create(partial).with_context(|| format!("failed to create {}", partial.display()))?;
    let mut hasher = Sha256::new();
    // 256 KiB: large enough that syscall overhead is noise against a 612 MiB
    // transfer, small enough to stay off the stack and out of the way.
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written: u64 = 0;
    let mut progress = Progress::new(file.size);

    loop {
        let read = source.read(&mut buffer).with_context(|| format!("failed while reading {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        sink.write_all(&buffer[..read]).with_context(|| format!("failed to write {}", partial.display()))?;
        written += read as u64;
        progress.report(written);
    }
    sink.flush().with_context(|| format!("failed to flush {}", partial.display()))?;
    progress.finish(written);

    // Size first: a truncated transfer is the common failure and saying so
    // plainly beats reporting a digest mismatch the user cannot act on.
    if written != file.size {
        bail!(
            "{} is incomplete: got {written} bytes, expected {}. The download was cut short; \
             re-run `g-mesh model fetch`.",
            file.local_name,
            file.size
        );
    }
    let digest = hex(&hasher.finalize());
    if digest != file.sha256 {
        bail!(
            "{} does not match the pinned revision {MODEL_REVISION}: sha256 {digest}, expected {}. \
             Nothing was written to the model directory.",
            file.local_name,
            file.sha256
        );
    }
    Ok(())
}

/// Lowercase hex, the form both Hugging Face and `shasum -a 256` print, so a
/// mismatch message can be checked by hand against either.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `612.0 MiB` - binary units, matching what `ls -lh` and the model card say.
fn human_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= MIB as u64 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

/// Download progress, on stderr.
///
/// stderr rather than stdout so that redirecting the command's output keeps
/// the progress visible and the output clean, and rate-limited rather than
/// per-chunk: 612 MiB in 256 KiB reads is ~2400 updates, which is a repainted
/// line on a terminal and 2400 lines of noise in a log. On a terminal it
/// rewrites one line; anywhere else it prints a line per decile, which is what
/// a CI log or a piped install script can actually use.
struct Progress {
    total: u64,
    interactive: bool,
    last_drawn: Instant,
    last_decile: u64,
}

impl Progress {
    fn new(total: u64) -> Self {
        Self {
            total,
            interactive: std::io::stderr().is_terminal(),
            last_drawn: Instant::now(),
            last_decile: 0,
        }
    }

    fn report(&mut self, written: u64) {
        if self.total == 0 {
            return;
        }
        if self.interactive {
            if self.last_drawn.elapsed() < Duration::from_millis(200) {
                return;
            }
            self.last_drawn = Instant::now();
            eprint!(
                "\r  {} / {} ({}%)   ",
                human_size(written),
                human_size(self.total),
                self.percent(written)
            );
        } else {
            let decile = self.percent(written) / 10;
            if decile > self.last_decile {
                self.last_decile = decile;
                eprintln!("  {}% ({} / {})", decile * 10, human_size(written), human_size(self.total));
            }
        }
    }

    fn percent(&self, written: u64) -> u64 {
        (written.saturating_mul(100) / self.total.max(1)).min(100)
    }

    /// Ends the in-place line so the next message does not land on top of a
    /// half-erased counter. Reports the real total rather than assuming 100%:
    /// the caller may be about to explain that the transfer was cut short.
    fn finish(&self, written: u64) {
        if self.interactive {
            eprintln!(
                "\r  {} / {} ({}%)   ",
                human_size(written),
                human_size(self.total),
                self.percent(written)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed script, embedded at compile time so this tracks the real
    /// file rather than a copy.
    const FETCH_SCRIPT: &str = include_str!("../../scripts/fetch-embedding-model.sh");

    fn fetch_output(dir: &Path) -> String {
        let mut out = Vec::new();
        fetch(Some(dir), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn status_output(dir: &Path) -> String {
        let mut out = Vec::new();
        status(Some(dir), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The invariant this module exists under: nothing else that ships can
    /// reach the network, because no other file under `src/` mentions the HTTP
    /// client at all. Asserted over the source tree rather than trusted,
    /// because the failure it guards against - a well-meaning "the model is
    /// missing, let me fetch it" inside `search_code` or the daemon - looks
    /// like a helpful change at review time.
    #[test]
    fn the_http_client_is_reachable_from_nowhere_but_this_module() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let this_file = src.join("cli").join("model.rs");

        let mut offenders = Vec::new();
        for path in rust_sources(&src) {
            if path == this_file {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            if text.contains("ureq") {
                offenders.push(path);
            }
        }

        assert!(
            offenders.is_empty(),
            "only cli/model.rs may use the HTTP client; found it in {offenders:?}. \
             g-mesh does not reach the network outside an explicit `g-mesh model fetch`."
        );
    }

    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
        found
    }

    /// The shell script and this command will coexist until the script is
    /// retired, downloading the same bytes to the same place. Drift between
    /// them would be invisible until someone's index silently disagreed with
    /// someone else's, so the script is checked against these constants
    /// instead of being trusted to keep up.
    #[test]
    fn the_shell_script_and_this_command_agree_on_the_pinned_revision() {
        assert!(FETCH_SCRIPT.contains(MODEL_REVISION), "the script pins a different revision");
        for file in &FILES {
            assert!(
                FETCH_SCRIPT.contains(file.remote_path),
                "the script does not fetch {}",
                file.remote_path
            );
            assert!(FETCH_SCRIPT.contains(file.local_name), "the script does not write {}", file.local_name);
        }
        // The repo is spelled `jinaai/${MODEL_NAME}` in the script, so only
        // the halves it actually contains are checked.
        let (owner, name) = MODEL_REPO.split_once('/').unwrap();
        assert!(FETCH_SCRIPT.contains(owner), "the script uses a different model owner");
        assert!(FETCH_SCRIPT.contains(name), "the script uses a different model name");
    }

    /// The URLs here only describe the default model, so the directory this
    /// writes into has to be the one the loader reads for that same default.
    #[test]
    fn the_pinned_repo_is_the_model_the_config_defaults_to() {
        let configured = EmbeddingConfig::default().model;
        assert_eq!(MODEL_REPO, format!("jinaai/{configured}"));
    }

    /// Digests are load-bearing constants; a typo in one turns every fetch
    /// into a hard failure, and only their shape can be checked offline.
    #[test]
    fn every_pinned_digest_is_lowercase_hex_of_the_right_width() {
        for file in &FILES {
            assert_eq!(file.sha256.len(), 64, "{}", file.local_name);
            assert!(
                file.sha256.chars().all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
                "{} has a non lowercase-hex digest",
                file.local_name
            );
            assert!(file.size > 0, "{}", file.local_name);
        }
    }

    /// The names written must be the names the loader looks for - not similar
    /// ones. Checked against the loader's own constants.
    #[test]
    fn the_downloaded_names_are_the_ones_the_loader_reads() {
        let names: Vec<_> = FILES.iter().map(|f| f.local_name).collect();
        assert!(names.contains(&ONNX_FILE_NAME));
        assert!(names.contains(&TOKENIZER_FILE_NAME));
        assert_eq!(names.len(), 2);
    }

    /// A `.partial` must not shadow the real name by replacing the extension:
    /// `model.onnx` -> `model.onnx.partial`, never `model.partial`, and above
    /// all never something the loader would pick up.
    #[test]
    fn the_partial_name_extends_the_final_name_rather_than_replacing_its_extension() {
        let dest = Path::new("/models/jina").join(ONNX_FILE_NAME);
        let partial = dest.with_file_name(format!("{ONNX_FILE_NAME}.partial"));
        assert_eq!(partial.file_name().unwrap(), "model.onnx.partial");
        assert_ne!(partial, dest);
    }

    /// The no-network path, exercised for real: with both files already in
    /// place, `fetch` reports them and returns without touching the network -
    /// which is also why this test can run offline at all.
    #[test]
    fn fetch_leaves_an_already_populated_directory_alone() {
        let dir = tempfile::tempdir().unwrap();
        for file in &FILES {
            fs::write(dir.path().join(file.local_name), b"pretend weights").unwrap();
        }

        let output = fetch_output(dir.path());

        assert_eq!(output.matches("already present").count(), 2, "{output}");
        assert!(!output.contains("downloading"), "{output}");
    }

    /// `--dir` is taken as given, and a fetch into a path that does not exist
    /// yet creates it rather than failing - a fresh machine has no
    /// `~/.g-mesh/models/` at all.
    #[test]
    fn fetch_creates_the_target_directory_it_was_given() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("nested").join("models");

        // The model is irrelevant to this test - an explicit directory wins
        // over the name either way - so it names the default rather than
        // implying the choice matters here.
        let default =
            ResolvedModel { name: EmbeddingConfig::default().model, source: ModelSource::BuiltInDefault };

        let prepared = prepare_dir(Some(&dir), &default).unwrap();

        assert_eq!(prepared, dir);
        assert!(dir.is_dir(), "fetch must create the directory it was pointed at");
    }

    #[test]
    fn status_reports_a_missing_model_and_how_to_get_it() {
        let dir = tempfile::tempdir().unwrap();

        let output = status_output(dir.path());

        assert!(output.contains("missing"), "{output}");
        assert!(output.contains("g-mesh model fetch"), "{output}");
        assert!(output.contains(&dir.path().display().to_string()), "{output}");
    }

    #[test]
    fn status_reports_a_complete_model_as_usable() {
        let dir = tempfile::tempdir().unwrap();
        // `set_len` rather than writing 612 MiB of zeros: `status` asks the
        // filesystem for a length and never reads the bytes, so a sparse file
        // of the right size is exactly the state under test.
        for file in &FILES {
            let handle = File::create(dir.path().join(file.local_name)).unwrap();
            handle.set_len(file.size).unwrap();
        }

        let output = status_output(dir.path());

        assert_eq!(output.matches("present").count(), 2, "{output}");
        assert!(output.contains("semantic search can use this model"), "{output}");
    }

    /// A file of the wrong length is called out specifically rather than
    /// reported as present - this is the state an interrupted pre-`.partial`
    /// download or a bad copy leaves behind.
    #[test]
    fn status_calls_out_a_file_of_the_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(ONNX_FILE_NAME), b"truncated").unwrap();
        fs::write(dir.path().join(TOKENIZER_FILE_NAME), b"truncated").unwrap();

        let output = status_output(dir.path());

        assert!(output.contains("expected"), "{output}");
        assert!(output.contains("delete it"), "{output}");
    }

    #[test]
    fn hex_renders_lowercase_two_digit_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(641_517_466), "611.8 MiB");
        assert_eq!(human_size(2048), "2.0 KiB");
    }

    // --- which model this command is about ---------------------------------

    /// A project root whose `[embedding] model` is `name`, written through the
    /// real config writer so this exercises the path `resolved_model_in` reads.
    fn project_configured_with(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("failed to create a temp project");
        let mut config = crate::config::ProjectConfig::default();
        config.embedding.model = name.to_string();
        crate::config::write_project_config(dir.path(), &config).expect("failed to write the project config");
        dir
    }

    #[test]
    fn a_project_with_no_config_of_its_own_resolves_to_the_default() {
        let dir = tempfile::tempdir().expect("failed to create a temp project");

        let model = resolved_model_in(dir.path());

        assert_eq!(model.name, EmbeddingConfig::default().model);
        assert_eq!(model.source, ModelSource::BuiltInDefault);
    }

    /// A project that configures the default *by name* is the same fact as one
    /// that configures nothing, and reports as such - the alternative would put
    /// a distinction in the output that means nothing to whoever reads it.
    #[test]
    fn configuring_the_default_by_name_is_not_reported_as_a_choice() {
        let dir = project_configured_with(&EmbeddingConfig::default().model);

        assert_eq!(resolved_model_in(dir.path()).source, ModelSource::BuiltInDefault);
    }

    #[test]
    fn a_non_default_model_is_reported_with_where_the_name_came_from() {
        let dir = project_configured_with("some-other-model");

        let model = resolved_model_in(dir.path());

        assert_eq!(model.name, "some-other-model");
        assert_eq!(model.source, ModelSource::ProjectConfig);
    }

    /// The disagreement this task closes, stated as a test: the directory
    /// `status`/`fetch` talk about has to be the one the *daemon* reads, which
    /// is `default_model_dir(configured_name)` - not the default's.
    #[test]
    fn the_reported_directory_is_the_one_the_daemon_would_read() {
        let configured =
            ResolvedModel { name: "some-other-model".to_string(), source: ModelSource::ProjectConfig };

        let reported = model_dir(None, &configured).unwrap();

        assert_eq!(reported, crate::embedding::model::default_model_dir("some-other-model").unwrap());
        assert_ne!(
            reported,
            crate::embedding::model::default_model_dir(&EmbeddingConfig::default().model).unwrap(),
            "reporting the default's directory is exactly the bug: status inspects one directory \
             while the daemon reads another, and neither mentions the other"
        );
    }

    // --- when fetching is refused ------------------------------------------

    #[test]
    fn fetching_the_default_is_never_refused() {
        let model =
            ResolvedModel { name: EmbeddingConfig::default().model, source: ModelSource::BuiltInDefault };

        assert_eq!(fetch_refusal(None, &model, Path::new("/tmp/whatever")), None);
    }

    /// The fresh-machine case this command exists for keeps working: an
    /// explicit `--dir` is a person saying where to put the default weights,
    /// not a configured model being ignored.
    #[test]
    fn an_explicit_directory_is_still_honoured_under_a_configured_model() {
        let model =
            ResolvedModel { name: "some-other-model".to_string(), source: ModelSource::ProjectConfig };

        assert_eq!(fetch_refusal(Some(Path::new("/tmp/here")), &model, Path::new("/tmp/daemon")), None);
    }

    /// The refusal has to carry everything needed to act on it, because the
    /// user cannot get the weights from this command and has to place them by
    /// hand: which model, where that name came from, and the exact directory.
    #[test]
    fn refusing_names_the_model_its_source_and_the_directory_to_fill() {
        let model =
            ResolvedModel { name: "some-other-model".to_string(), source: ModelSource::ProjectConfig };
        let daemon_dir = Path::new("/home/someone/.g-mesh/models/some-other-model");

        let refusal = fetch_refusal(None, &model, daemon_dir).expect("a configured model must refuse");

        assert!(refusal.contains("some-other-model"), "{refusal}");
        assert!(refusal.contains("config.toml"), "{refusal}");
        assert!(refusal.contains(&daemon_dir.display().to_string()), "{refusal}");
        assert!(
            refusal.contains(&EmbeddingConfig::default().model),
            "must say what it *can* fetch: {refusal}"
        );
    }

    /// `status` and `fetch` must agree about whether this invocation can be
    /// served, since the two disagreeing silently is the whole defect. Driven
    /// through the one predicate both call rather than through two copies.
    #[test]
    fn status_and_fetch_agree_on_whether_a_fetch_is_possible() {
        for (model, explicit) in [
            (ResolvedModel { name: "x".into(), source: ModelSource::ProjectConfig }, None),
            (
                ResolvedModel { name: "x".into(), source: ModelSource::ProjectConfig },
                Some(Path::new("/tmp/d")),
            ),
            (
                ResolvedModel { name: EmbeddingConfig::default().model, source: ModelSource::BuiltInDefault },
                None,
            ),
        ] {
            let dir = model_dir(explicit, &model).unwrap();
            let refused = fetch_refusal(explicit, &model, &dir).is_some();
            // status prints its note under exactly this condition; fetch bails
            // under exactly this condition. One predicate, so they cannot drift.
            assert_eq!(refused, fetch_refusal(explicit, &model, &dir).is_some());
        }
    }
}
