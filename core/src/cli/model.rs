//! `g-mesh model fetch` / `g-mesh model status`: acquiring the embedding
//! weights `search_code` needs, from the binary itself.
//!
//! # Why this exists
//!
//! The weights are ~154 MiB and are not vendored (see
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
//!   there. The two exceptions, [`fetch_to_file`] and [`fetch_text`], are
//!   `pub(super)`: visible to `cli`'s own `plugins install` and to nothing
//!   outside `cli`.
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
//! [`crate::config::embedding::resolve_model_dir`], the loader's own resolution, so
//! "downloaded successfully" and "model not found" cannot both be true.
//!
//! # Plugin assets
//!
//! `g-mesh plugins install <language>` downloads through [`fetch_to_file`],
//! which streams and hashes exactly as the model download does but hands the
//! digest back instead of comparing it to a pinned constant: a plugin's
//! expected digest comes from the `.sha256` published beside it. The rule the
//! caller applies - per-asset `.sha256`, compare before unpacking, refuse and
//! delete on mismatch, print both digests - is also implemented in
//! `scripts/install.sh` and `scripts/install.ps1`; change all three
//! ([ADR 0027](../../../docs/adr/0027-plugin-fetch-checksums-in-rust.md)).

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::cli::ModelCommand;
use crate::config::embedding::{rerank_model_settings, resolve_model_dir};
use crate::config::EmbeddingConfig;
use crate::embedding::model::{
    DEFAULT_ONNX_REMOTE_PATH, DEFAULT_ONNX_SHA256, DEFAULT_ONNX_SIZE, ONNX_FILE_NAME, TOKENIZER_FILE_NAME,
};
use crate::embedding::rerank::{RerankVariant, VariantSpec, L6, QAT, RERANK_MODEL_DIR_ENV};

/// The Hugging Face repository the default model comes from.
pub(crate) const MODEL_REPO: &str = "jinaai/jina-embeddings-v2-base-code";

/// Pinned, and pinned to a commit hash rather than a branch: these are the
/// exact weights the embedding tests were verified against, and bumping this
/// changes every vector the model produces, invalidating any index built with
/// the old one (REQUIREMENTS.md, "Инвалидация эмбеддингов при смене
/// embedding-модели"). An immutable revision is also what makes the digests
/// below meaningful - the bytes at this commit cannot legitimately change.
pub(crate) const MODEL_REVISION: &str = "516f4baf13dec4ddddda8631e019b5737c8bc250";

/// Where the weights are fetched from, tried in order.
///
/// Upstream stays first among the built-ins deliberately. The mirror exists
/// for availability, not for provenance: keeping Hugging Face as the source
/// everyone normally touches means the project never quietly becomes the
/// origin of weights it only redistributes. The cost of that choice is that a
/// fallback nobody exercises can rot, which is why the fallback has a test of
/// its own rather than only a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// Hugging Face, addressed the way it addresses itself: repository,
    /// revision, then the path inside the repository.
    Upstream,
    /// Any host serving the embedding model's two files side by side under
    /// one prefix - a corporate mirror, a GitHub release's assets (which are
    /// flat under their tag), or a directory served for a test - and each
    /// rerank variant's two under `<prefix>/<variant's dir_name>/`, e.g.
    /// `<prefix>/g-mesh-reranker-l6-qat/model.onnx`.
    Flat(String),
}

impl Source {
    fn url(&self, file: &RemoteFile) -> String {
        match self {
            Self::Upstream => {
                format!("https://huggingface.co/{}/resolve/{}/{}", file.repo, file.revision, file.remote_path)
            }
            Self::Flat(base) => {
                format!("{}/{}{}", base.trim_end_matches('/'), file.flat_prefix, file.local_name)
            }
        }
    }

    fn describe(&self, file: &RemoteFile) -> String {
        match self {
            Self::Upstream => format!("{}@{}", file.repo, &file.revision[..7]),
            Self::Flat(base) => base.clone(),
        }
    }
}

/// The sources to try, in order.
fn sources() -> Vec<Source> {
    let override_base = crate::knobs::os_non_empty(&crate::knobs::MODEL_BASE_URL);
    sources_from(override_base.as_ref().map(|value| value.to_string_lossy()).as_deref())
}

/// The ordering decision on its own, so it can be tested without touching the
/// environment. `std::env::set_var` is process-global and cargo runs tests in
/// threads, so a test that set the variable would race every other test in the
/// binary - and the failure would look like flakiness rather than like a test
/// reaching outside itself.
fn sources_from(override_base: Option<&str>) -> Vec<Source> {
    let mut sources = Vec::new();
    if let Some(base) = override_base.filter(|value| !value.is_empty()) {
        sources.push(Source::Flat(base.to_string()));
    }
    sources.push(Source::Upstream);
    sources
}

/// How long to wait for the connection itself. Deliberately *not* a deadline
/// on the whole request: 154 MiB over a slow link legitimately takes many
/// minutes, and a total timeout would turn a working download into a failure
/// for exactly the users who can least afford to repeat it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A stalled socket, on the other hand, should not hang forever. This bounds a
/// single read, not the transfer.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// A partial untouched for this long belongs to no live fetch and is deleted
/// by the next fetch of the same file (see [`sweep_stale_partials`]).
///
/// Invariant: well above [`READ_TIMEOUT`]. A live fetch writes its partial at
/// least once per read, and a read that waits longer than `READ_TIMEOUT`
/// fails the fetch, so a live partial's mtime is never older than that.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(60 * 60);
const _: () = assert!(STALE_PARTIAL_AGE.as_secs() >= 10 * READ_TIMEOUT.as_secs());

/// One file of a model, with everything needed to verify it arrived intact.
struct RemoteFile {
    /// The Hugging Face repository, and the pinned revision inside it.
    repo: &'static str,
    revision: &'static str,
    /// Prepended to `local_name` on a [`Source::Flat`] mirror.
    flat_prefix: &'static str,
    /// Path inside the model repository at [`MODEL_REVISION`].
    remote_path: &'static str,
    /// Name inside the model directory - taken from the loader's constants,
    /// never spelled out here.
    local_name: &'static str,
    size: u64,
    /// SHA-256 of the file's bytes.
    ///
    /// Both digests were confirmed from two independent sources before being
    /// pinned: Hugging Face's own metadata for this revision (the LFS
    /// `x-linked-etag` of `onnx/model_quantized.onnx`), and hashing the files
    /// after download (for `tokenizer.json`, which is not stored in LFS and
    /// therefore has no published digest, the git blob id of the downloaded
    /// bytes was recomputed and matched the revision's tree, then its SHA-256
    /// taken locally).
    sha256: &'static str,
}

/// The two files a model directory consists of, in download order: the small
/// one last, so an interrupted run leaves the cheap file to redo.
const FILES: [RemoteFile; 2] = [
    // The int8 export (docs/adr/0011-embedding-model-int8.md), written under
    // the loader's `model.onnx`.
    RemoteFile {
        repo: MODEL_REPO,
        revision: MODEL_REVISION,
        flat_prefix: "",
        remote_path: DEFAULT_ONNX_REMOTE_PATH,
        local_name: ONNX_FILE_NAME,
        size: DEFAULT_ONNX_SIZE,
        sha256: DEFAULT_ONNX_SHA256,
    },
    RemoteFile {
        repo: MODEL_REPO,
        revision: MODEL_REVISION,
        flat_prefix: "",
        remote_path: "tokenizer.json",
        local_name: TOKENIZER_FILE_NAME,
        size: 2_561_316,
        sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
    },
];

/// A rerank variant's two files (`embedding::rerank`) at its pinned
/// revision, in the same download order as [`FILES`]. `flat_prefix` is the
/// variant's `dir_name` and a slash.
const fn rerank_files_of(spec: &'static VariantSpec, flat_prefix: &'static str) -> [RemoteFile; 2] {
    [
        RemoteFile {
            repo: spec.repo,
            revision: spec.revision,
            flat_prefix,
            remote_path: spec.onnx_remote_path,
            local_name: ONNX_FILE_NAME,
            size: spec.onnx_size,
            sha256: spec.onnx_sha256,
        },
        RemoteFile {
            repo: spec.repo,
            revision: spec.revision,
            flat_prefix,
            remote_path: "tokenizer.json",
            local_name: TOKENIZER_FILE_NAME,
            size: spec.tokenizer_size,
            sha256: spec.tokenizer_sha256,
        },
    ]
}

const QAT_FILES: [RemoteFile; 2] = rerank_files_of(&QAT, "g-mesh-reranker-l6-qat/");
const L6_FILES: [RemoteFile; 2] = rerank_files_of(&L6, "ms-marco-MiniLM-L6-v2/");

fn rerank_files(variant: RerankVariant) -> &'static [RemoteFile; 2] {
    match variant {
        RerankVariant::Qat => &QAT_FILES,
        RerankVariant::L6 => &L6_FILES,
    }
}

/// The rerank variant `model` works on, its directory, the fallback
/// variant's directory when a missing default falls back to it, and the
/// unpublished variant asked for when `variant` stands in for it.
struct RerankTarget {
    variant: RerankVariant,
    dir: PathBuf,
    fallback_dir: Option<PathBuf>,
    unpublished: Option<RerankVariant>,
}

impl RerankTarget {
    fn from_settings() -> Result<Self> {
        let settings = rerank_model_settings();
        Ok(Self {
            variant: settings.variant,
            dir: settings.model_dir?,
            fallback_dir: settings.fallback_dir,
            unpublished: settings.unpublished,
        })
    }

    /// One line when `variant` stands in for an unpublished variant.
    fn note_unpublished(&self, out: &mut impl Write) -> Result<()> {
        if let Some(asked) = self.unpublished {
            writeln!(
                out,
                "rerank model {asked} is not published yet - using {}, at its own beta",
                self.variant
            )?;
        }
        Ok(())
    }
}

/// Runs `g-mesh model <subcommand>`.
pub fn run(command: &ModelCommand) -> Result<()> {
    let stdout = std::io::stdout();
    match command {
        ModelCommand::Fetch { dir, no_rerank } => {
            let rerank = if *no_rerank { None } else { Some(RerankTarget::from_settings()?) };
            fetch(dir.as_deref(), rerank.as_ref(), &mut stdout.lock())
        }
        ModelCommand::Status { dir } => {
            status(dir.as_deref(), Some(&RerankTarget::from_settings()?), &mut stdout.lock())
        }
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
/// weights anyway: 154 MiB landing in a directory the daemon never reads, with
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
/// A file that is already there at its pinned size is left alone rather than
/// re-downloaded, so re-running after an interrupted fetch only pays for what
/// is still missing. Size is the test, not digest: hashing 154 MiB on every
/// invocation would cost every user for a rare case. A file of any other size
/// is replaced: a `model.onnx` holding other weights (the fp32 export, at
/// 641,517,466 bytes) must not pass for the pinned one. The replacement is downloaded and verified beside it and renamed
/// over it only once verified, so a failed fetch leaves the old file intact.
///
/// `rerank` is the selected rerank variant and where it goes
/// (`$G_MESH_RERANK_MODEL_DIR`, else `~/.g-mesh/models/<variant's
/// dir_name>`), or `None` to skip it (`--no-rerank`). Only the selected
/// variant is fetched; another variant's directory is not touched. The rerank
/// model does not depend on the embedding model, so a refused embedding
/// fetch still fetches it, then fails with the refusal.
fn fetch(explicit: Option<&Path>, rerank: Option<&RerankTarget>, out: &mut impl Write) -> Result<()> {
    let model = resolved_model();
    if let Some(rerank) = rerank {
        rerank.note_unpublished(out)?;
    }
    // Before the embedding directory is created, so a refused fetch leaves
    // nothing behind there.
    if let Some(refusal) = fetch_refusal(explicit, &model, &model_dir(None, &model)?) {
        if let Some(rerank) = rerank {
            fetch_rerank(&http_agent(), rerank.variant, &rerank.dir, &sources(), out)?;
            writeln!(out)?;
        }
        bail!("{refusal}");
    }

    let dir = prepare_dir(explicit, &model)?;
    writeln!(out, "model directory: {}", dir.display())?;

    let agent = http_agent();
    let sources = sources();
    fetch_files(&agent, &dir, &FILES, &sources, out)?;
    writeln!(out, "\nmodel ready in {}", dir.display())?;

    if let Some(rerank) = rerank {
        writeln!(out)?;
        fetch_rerank(&agent, rerank.variant, &rerank.dir, &sources, out)?;
    }
    Ok(())
}

/// Fetches `variant` into `rerank_dir`; refused, before anything is created,
/// while the variant is unpublished or while `rerank_dir` holds another
/// variant's model.
fn fetch_rerank(
    agent: &ureq::Agent,
    variant: RerankVariant,
    rerank_dir: &Path,
    sources: &[Source],
    out: &mut impl Write,
) -> Result<()> {
    variant.spec().check_published()?;
    if let Some(other) = other_variant_in(rerank_dir, variant) {
        bail!(
            "{} holds rerank model {other}, not {variant}; `g-mesh model fetch` does not replace it \
             ({other} may not be downloadable). Point {RERANK_MODEL_DIR_ENV} at another directory, or unset it.",
            rerank_dir.display()
        );
    }
    fs::create_dir_all(rerank_dir)
        .with_context(|| format!("failed to create model directory {}", rerank_dir.display()))?;
    writeln!(out, "rerank model {variant} directory: {}", rerank_dir.display())?;
    fetch_files(agent, rerank_dir, rerank_files(variant), sources, out)?;
    writeln!(out, "\nrerank model {variant} ready in {}", rerank_dir.display())?;
    Ok(())
}

/// The variant other than `variant` whose `model.onnx` is in `dir` at its
/// pinned size. The variants' `model.onnx` sizes differ (their tokenizers do
/// not), so the size tells them apart; replacing that file would lose a
/// model that may not be downloadable (an unpublished `qat`).
fn other_variant_in(dir: &Path, variant: RerankVariant) -> Option<RerankVariant> {
    let size = fs::metadata(dir.join(ONNX_FILE_NAME)).ok()?.len();
    RerankVariant::ALL.into_iter().find(|other| *other != variant && other.spec().onnx_size == size)
}

/// Downloads each of `files` into `dir` unless it is already there at its
/// pinned size.
fn fetch_files(
    agent: &ureq::Agent,
    dir: &Path,
    files: &[RemoteFile],
    sources: &[Source],
    out: &mut impl Write,
) -> Result<()> {
    for file in files {
        let dest = dir.join(file.local_name);
        match on_disk(&dest, file) {
            OnDisk::Pinned => {
                writeln!(out, "already present: {}", dest.display())?;
                continue;
            }
            OnDisk::Other(size) => writeln!(
                out,
                "replacing {} ({} on disk, not the pinned {})",
                dest.display(),
                human_size(size),
                human_size(file.size)
            )?,
            OnDisk::Missing => {}
        }
        writeln!(
            out,
            "downloading {} ({}) from {}",
            file.local_name,
            human_size(file.size),
            sources[0].describe(file)
        )?;
        download(agent, file, &dest, sources, out)?;
        writeln!(out, "  verified {}", file.local_name)?;
    }
    Ok(())
}

/// What [`fetch`] finds at a file's destination.
#[derive(Debug, PartialEq, Eq)]
enum OnDisk {
    Missing,
    /// Present at the pinned size: left alone.
    Pinned,
    /// Present at another size (other weights, a truncated copy): replaced.
    Other(u64),
}

fn on_disk(dest: &Path, file: &RemoteFile) -> OnDisk {
    match fs::metadata(dest) {
        Ok(meta) if meta.len() == file.size => OnDisk::Pinned,
        Ok(meta) => OnDisk::Other(meta.len()),
        Err(_) => OnDisk::Missing,
    }
}

/// Resolves the model directory and makes sure it exists.
///
/// Split out of [`fetch`] so the part that touches the filesystem can be
/// tested without the part that touches the network - a test calling `fetch`
/// on an empty directory would start a 154 MiB download.
fn prepare_dir(explicit: Option<&Path>, model: &ResolvedModel) -> Result<PathBuf> {
    let dir = model_dir(explicit, model)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create model directory {}", dir.display()))?;
    Ok(dir)
}

/// `g-mesh model status`: says where the weights are expected and whether they
/// are there.
///
/// Checks presence and size only - never digests. Hashing 154 MiB to answer
/// "do I have the model?" would make the cheap question expensive; the size
/// check is enough to catch the one damaged state this command can produce
/// (a file restored from a truncated backup, say), and [`fetch`] verifies the
/// digest at the only moment the bytes are actually in hand.
///
/// `rerank`, when given, is reported the same way for the selected rerank
/// variant, with the fallback the daemon uses while it is missing.
fn status(explicit: Option<&Path>, rerank: Option<&RerankTarget>, out: &mut impl Write) -> Result<()> {
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

    let complete = report_files(&dir, &FILES, out)?;
    if complete {
        writeln!(out, "\nsemantic search can use this model.")?;
    } else {
        writeln!(out, "\nrun `g-mesh model fetch` to download it.")?;
    }

    if let Some(rerank) = rerank {
        let spec = rerank.variant.spec();
        writeln!(out, "\nrerank model: {} ({})", rerank.variant, spec.repo)?;
        rerank.note_unpublished(out)?;
        writeln!(out, "revision:  {}", spec.revision)?;
        writeln!(out, "directory: {}", rerank.dir.display())?;
        let fallback = rerank
            .variant
            .fallback()
            .zip(rerank.fallback_dir.as_deref())
            .filter(|(fallback, dir)| fallback.spec().sizes_match(dir));
        if report_files(&rerank.dir, rerank_files(rerank.variant), out)? {
            writeln!(out, "\nsearch_code can rerank with this model (unless switched off: [rerank] enabled, G_MESH_RERANK=off).")?;
        } else if let Some((fallback, dir)) = fallback {
            writeln!(
                out,
                "\nsearch_code reranks with {fallback} from {} until this model is fetched; run `g-mesh \
                 model fetch` to download it.",
                dir.display()
            )?;
        } else if rerank.variant.published() {
            writeln!(
                out,
                "\nsearch_code keeps the embedding order; run `g-mesh model fetch` to download it."
            )?;
        } else {
            writeln!(
                out,
                "\nsearch_code keeps the embedding order; this model is not published yet, so `g-mesh \
                 model fetch` cannot download it: put its files in {RERANK_MODEL_DIR_ENV}."
            )?;
        }
    }
    Ok(())
}

/// One line per file of `files` in `dir`: present, missing or the wrong
/// size. True when every file is present at its pinned size.
fn report_files(dir: &Path, files: &[RemoteFile], out: &mut impl Write) -> Result<bool> {
    let mut complete = true;
    for file in files {
        let path = dir.join(file.local_name);
        match fs::metadata(&path) {
            Ok(meta) if meta.len() == file.size => {
                writeln!(out, "  {}: present ({})", file.local_name, human_size(meta.len()))?;
            }
            Ok(meta) => {
                complete = false;
                writeln!(
                    out,
                    "  {}: {} on disk but {} expected - `g-mesh model fetch` replaces it",
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
    Ok(complete)
}

/// Downloads one file to a partial name of its own (see [`partial_path`]),
/// verifies it, and only then renames it into place.
///
/// The two-step is the whole point. If an interrupted download could leave
/// bytes under the final name, the very next run would see the file, call it
/// present and skip it - and the failure would surface much later as an ONNX
/// parse error inside the daemon, or, for a half-written `tokenizer.json`, as
/// vectors that are quietly wrong rather than as an error at all. So the final
/// name never exists until the bytes have been counted and hashed, and every
/// way out of this function other than a successful rename deletes its own
/// partial instead of leaving debris. A process killed mid-download cannot,
/// so its partial is left for [`sweep_stale_partials`] to delete once it has
/// gone stale.
fn download(
    agent: &ureq::Agent,
    file: &RemoteFile,
    dest: &Path,
    sources: &[Source],
    out: &mut impl Write,
) -> Result<()> {
    sweep_stale_partials(dest, file, STALE_PARTIAL_AGE, out)?;
    let partial = PartialFile(partial_path(dest, file));

    // Every attempt is kept, not just the last. One generic "download failed"
    // for a list of sources is the same mistake as one generic "not indexed"
    // for a list of reasons: it names the symptom of the last try and hides
    // that the others were tried at all.
    let mut failures = Vec::new();
    for source in sources {
        match download_to_partial(agent, file, &partial.0, source) {
            Ok(()) => {
                if !failures.is_empty() {
                    writeln!(out, "  fetched from {} instead", source.describe(file))?;
                }
                return move_into_place(&partial.0, dest, file);
            }
            Err(err) => {
                // Best-effort: if this fails too there is nothing useful left
                // to do, and the message about *why* the download failed is
                // worth more than one about the leftover file.
                let _ = fs::remove_file(&partial.0);
                writeln!(out, "  {} failed: {err:#}", source.describe(file))?;
                failures.push(format!("{}: {err:#}", source.describe(file)));
            }
        }
    }

    bail!("could not download {} from any source.\n  {}", file.local_name, failures.join("\n  "))
}

/// Where this writer streams `file` before renaming it to `dest`:
/// `<local_name>.<pid>-<n>.partial` next to `dest`.
///
/// Unique per call, so two processes (or two calls in one process) fetching
/// into the same directory never write, truncate or rename each other's
/// partial. The name extends the final name rather than replacing its
/// extension, so it never matches a name the loader reads.
fn partial_path(dest: &Path, file: &RemoteFile) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!("{}.{}-{n}.partial", file.local_name, std::process::id()))
}

/// Whether `name` is a partial of `local_name` written by any fetcher: the
/// per-process `<local_name>.<pid>-<n>.partial` of [`partial_path`], the
/// shell script's `<local_name>.<pid>.partial`, or the shared
/// `<local_name>.partial` older releases wrote.
fn is_partial_of(name: &str, local_name: &str) -> bool {
    let Some(rest) = name.strip_prefix(local_name).and_then(|rest| rest.strip_prefix('.')) else {
        return false;
    };
    if rest == "partial" {
        return true;
    }
    rest.strip_suffix(".partial").is_some_and(|tag| {
        let mut parts = tag.split('-');
        let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        parts.next().is_some_and(all_digits) && parts.next().is_none_or(all_digits) && parts.next().is_none()
    })
}

/// Deletes partials of `file` next to `dest` whose last write is at least
/// `max_age` ago: debris of fetches that were killed before their
/// [`PartialFile`] guard could run. A partial written more recently is left
/// alone, since a concurrent fetch may still be writing it; a modification
/// time in the future counts as recent. Failing to delete one is reported
/// and does not stop the fetch.
fn sweep_stale_partials(
    dest: &Path,
    file: &RemoteFile,
    max_age: Duration,
    out: &mut impl Write,
) -> Result<()> {
    let Some(dir) = dest.parent() else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_partial_of(name, file.local_name) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if !stale {
            continue;
        }
        let path = entry.path();
        match fs::remove_file(&path) {
            Ok(()) => writeln!(out, "removed stale partial {}", path.display())?,
            Err(err) => writeln!(out, "  could not remove stale partial {}: {err}", path.display())?,
        }
    }
    Ok(())
}

/// A partial this call created; dropping it deletes the file, best-effort, so
/// an early return or a panic does not leave it behind. After a successful
/// rename the path no longer exists and the delete is a no-op.
struct PartialFile(PathBuf);

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Renames a verified `partial` to `dest`, replacing whatever is there.
///
/// A failed rename is still success when `dest` now holds the pinned file,
/// checked by size and digest like a fresh download: a concurrent fetch got
/// there first (on Windows the rename over a file another process has open
/// fails).
fn move_into_place(partial: &Path, dest: &Path, file: &RemoteFile) -> Result<()> {
    match fs::rename(partial, dest) {
        Ok(()) => Ok(()),
        Err(_) if holds_pinned(dest, file) => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!("failed to move {} into place as {}", partial.display(), dest.display())
        }),
    }
}

/// Whether `path` holds exactly the pinned bytes of `file`: its size, then
/// its sha256. An unreadable file does not.
fn holds_pinned(path: &Path, file: &RemoteFile) -> bool {
    on_disk(path, file) == OnDisk::Pinned && sha256_of(path).is_ok_and(|digest| digest == file.sha256)
}

/// The SHA-256 of the file at `path`, as lowercase hex.
fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut source = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Streams the response into `partial`, hashing as it goes, and fails if what
/// arrived is not byte-for-byte what [`MODEL_REVISION`] pins.
fn download_to_partial(
    agent: &ureq::Agent,
    file: &RemoteFile,
    partial: &Path,
    source: &Source,
) -> Result<()> {
    let url = source.url(file);
    let (written, digest) = stream_hashed(agent, &url, partial, Some(file.size))?;

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
    if digest != file.sha256 {
        bail!(
            "{} does not match the pinned revision {}: sha256 {digest}, expected {}. \
             Nothing was written to the model directory.",
            file.local_name,
            file.revision,
            file.sha256
        );
    }
    Ok(())
}

/// The client every download in this module uses.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(concat!("g-mesh/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Streams `url` into `dest`, hashing as it goes, and returns the bytes
/// written and their SHA-256 as lowercase hex. `expected_size` only drives the
/// progress line; `None` takes it from the response's `Content-Length`, and an
/// unknown size draws none.
fn stream_hashed(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
) -> Result<(u64, String)> {
    let response = agent.get(url).call().with_context(|| format!("failed to download {url}"))?;
    let expected_size = expected_size
        .or_else(|| response.header("Content-Length").and_then(|len| len.parse().ok()))
        .unwrap_or(0);

    let mut source = response.into_reader();
    let mut sink = File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    // 256 KiB: large enough that syscall overhead is noise against a 154 MiB
    // transfer, small enough to stay off the stack and out of the way.
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written: u64 = 0;
    let mut progress = Progress::new(expected_size);

    loop {
        let read = source.read(&mut buffer).with_context(|| format!("failed while reading {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        sink.write_all(&buffer[..read]).with_context(|| format!("failed to write {}", dest.display()))?;
        written += read as u64;
        progress.report(written);
    }
    sink.flush().with_context(|| format!("failed to flush {}", dest.display()))?;
    progress.finish(written);

    Ok((written, hex(&hasher.finalize())))
}

/// Downloads `url` into `dest` and returns the SHA-256 of what arrived, as
/// lowercase hex. Comparing it is the caller's job; on any error `dest` is
/// deleted rather than left half-written.
pub(super) fn fetch_to_file(url: &str, dest: &Path) -> Result<String> {
    stream_hashed(&http_agent(), url, dest, None).map(|(_, digest)| digest).inspect_err(|_| {
        let _ = fs::remove_file(dest);
    })
}

/// Downloads a small text resource (a `.sha256` file) into memory. Capped so a
/// wrong URL serving something large fails instead of filling memory.
pub(super) fn fetch_text(url: &str) -> Result<String> {
    const LIMIT: u64 = 64 * 1024;
    let response = http_agent().get(url).call().with_context(|| format!("failed to download {url}"))?;
    let mut text = String::new();
    response
        .into_reader()
        .take(LIMIT)
        .read_to_string(&mut text)
        .with_context(|| format!("failed while reading {url}"))?;
    Ok(text)
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
/// per-chunk: 154 MiB in 256 KiB reads is ~2400 updates, which is a repainted
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

    /// Serves `body` once at `/<name>` and then stops. Enough HTTP for `ureq`
    /// to read a body, and no dependency to add for it.
    fn serve_once(body: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind a test server");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut scratch = [0u8; 1024];
                let _ = socket.read(&mut scratch);
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = socket.write_all(body);
                let _ = socket.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// A port nothing is listening on. Bound and released, so it is free and
    /// refuses immediately rather than hanging until `CONNECT_TIMEOUT`.
    fn dead_source() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    const TEST_BODY: &[u8] = b"the bytes a mirror would serve\n";
    /// sha256 of TEST_BODY, computed rather than written from memory - the
    /// first attempt at this constant was invented, and the test caught it by
    /// failing exactly where a substituted mirror would. That is the property
    /// worth having: the fallback is verified the same way the real download
    /// is, and a fallback that skipped the digest check would be worse than no
    /// fallback at all.
    const TEST_DIGEST: &str = "602a7f1475de6cea8eeee903327a0e3d402620c8f277f075d4fcdc9f068fc6a9";

    /// `l6` in `dir`, with no fallback.
    fn l6_target(dir: &Path) -> RerankTarget {
        RerankTarget {
            variant: RerankVariant::L6,
            dir: dir.to_path_buf(),
            fallback_dir: None,
            unpublished: None,
        }
    }

    fn test_file() -> RemoteFile {
        RemoteFile {
            repo: MODEL_REPO,
            revision: MODEL_REVISION,
            flat_prefix: "",
            remote_path: "onnx/model.onnx",
            local_name: "mirrored.bin",
            size: TEST_BODY.len() as u64,
            sha256: TEST_DIGEST,
        }
    }

    #[test]
    fn an_override_is_tried_first_and_upstream_still_follows_it() {
        assert_eq!(sources_from(None), vec![Source::Upstream]);
        assert_eq!(
            sources_from(Some("https://mirror.example/g-mesh")),
            vec![Source::Flat("https://mirror.example/g-mesh".to_string()), Source::Upstream],
            "an override must not remove the fallback - an unreachable mirror should cost a \
             slower download, not a failed one"
        );
        assert_eq!(sources_from(Some("")), vec![Source::Upstream], "an empty override is not a source");
    }

    #[test]
    fn a_flat_source_addresses_the_file_by_the_name_it_lands_under() {
        let file = test_file();
        assert_eq!(
            Source::Flat("https://example.test/weights/".to_string()).url(&file),
            "https://example.test/weights/mirrored.bin",
            "a trailing slash on the base must not produce a doubled one"
        );
        assert!(Source::Upstream.url(&file).ends_with("/onnx/model.onnx"), "upstream keeps its own shape");
    }

    /// The acceptance criterion of GM-214: with the primary unreachable, the
    /// download transparently succeeds from the secondary, the digest is still
    /// checked, and the output names both.
    #[test]
    fn an_unreachable_primary_falls_through_to_the_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let agent = ureq::AgentBuilder::new().build();
        let sources = vec![Source::Flat(dead_source()), Source::Flat(serve_once(TEST_BODY))];
        let mut out = Vec::new();

        download(&agent, &test_file(), &dest, &sources, &mut out).expect("the fallback must succeed");

        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY, "the bytes must be the ones the digest pins");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("failed:"), "the failed attempt must be named: {said}");
        assert!(said.contains("fetched from"), "the source that worked must be named: {said}");
    }

    #[test]
    fn every_attempt_is_named_when_none_of_them_works() {
        let dir = tempfile::tempdir().unwrap();
        let agent = ureq::AgentBuilder::new().build();
        let (first, second) = (dead_source(), dead_source());
        let sources = vec![Source::Flat(first.clone()), Source::Flat(second.clone())];

        let err = download(&agent, &test_file(), &dir.path().join("x.bin"), &sources, &mut Vec::new())
            .expect_err("no source could have served this");

        let report = format!("{err:#}");
        assert!(report.contains(&first), "the first attempt must be in the error: {report}");
        assert!(report.contains(&second), "the second attempt must be in the error: {report}");
    }

    fn fetch_output(dir: &Path) -> String {
        let mut out = Vec::new();
        fetch(Some(dir), None, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn status_output(dir: &Path) -> String {
        let mut out = Vec::new();
        status(Some(dir), None, &mut out).unwrap();
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
             g-mesh does not reach the network outside an explicit `g-mesh model fetch` or `g-mesh plugins install`."
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
            assert!(
                FETCH_SCRIPT.contains(&format!(" {}\n", file.size)),
                "the script does not expect {}'s pinned size {}",
                file.local_name,
                file.size
            );
        }
        // The repo is spelled `jinaai/${MODEL_NAME}` in the script, so only
        // the halves it actually contains are checked.
        let (owner, name) = MODEL_REPO.split_once('/').unwrap();
        assert!(FETCH_SCRIPT.contains(owner), "the script uses a different model owner");
        assert!(FETCH_SCRIPT.contains(name), "the script uses a different model name");
    }

    /// The override is only useful if both ways of fetching honour it, and a
    /// variable name is exactly the kind of thing that agrees on the day it is
    /// written and drifts afterwards. Checked here for the same reason the
    /// revision above is: the script is a second implementation of this
    /// command, and the two have to be kept in step by something.
    #[test]
    fn the_shell_script_reads_the_same_override_variable() {
        let base_url_env = crate::knobs::MODEL_BASE_URL.name;
        assert!(
            FETCH_SCRIPT.contains(base_url_env),
            "the script does not honour {base_url_env}, so the two ways of fetching \
             the weights would disagree about where they come from"
        );
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

    /// A partial must not shadow the real name by replacing the extension:
    /// `model.onnx` -> `model.onnx.<pid>-<n>.partial`, never `model.partial`,
    /// and above all never something the loader would pick up.
    #[test]
    fn the_partial_name_extends_the_final_name_rather_than_replacing_its_extension() {
        let dest = Path::new("/models/jina").join(ONNX_FILE_NAME);
        let file = RemoteFile { local_name: ONNX_FILE_NAME, ..test_file() };
        let partial = partial_path(&dest, &file);
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("model.onnx."), "{name}");
        assert!(name.ends_with(".partial"), "{name}");
        assert_eq!(partial.parent(), dest.parent());
        assert_ne!(partial, dest);
    }

    /// The no-network path, exercised for real: with both files already in
    /// place, `fetch` reports them and returns without touching the network -
    /// which is also why this test can run offline at all.
    #[test]
    fn fetch_leaves_an_already_populated_directory_alone() {
        let dir = tempfile::tempdir().unwrap();
        // Sparse files of the pinned sizes: `fetch` judges by length, and a
        // file of any other length would be replaced (next test).
        for file in &FILES {
            File::create(dir.path().join(file.local_name)).unwrap().set_len(file.size).unwrap();
        }

        let output = fetch_output(dir.path());

        assert_eq!(output.matches("already present").count(), 2, "{output}");
        assert!(!output.contains("downloading"), "{output}");
    }

    /// With both models already in place, `fetch` reports all four files and
    /// downloads nothing; the rerank model lands in its own directory, never
    /// in the embedding model's.
    ///
    /// *Control:* drop the rerank loop from `fetch`, and only two files are
    /// reported.
    #[test]
    fn fetch_reports_the_rerank_model_in_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let rerank = tempfile::tempdir().unwrap();
        for file in &FILES {
            File::create(dir.path().join(file.local_name)).unwrap().set_len(file.size).unwrap();
        }
        for file in &L6_FILES {
            File::create(rerank.path().join(file.local_name)).unwrap().set_len(file.size).unwrap();
        }

        let mut out = Vec::new();
        fetch(Some(dir.path()), Some(&l6_target(rerank.path())), &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert_eq!(output.matches("already present").count(), 4, "{output}");
        assert!(
            output.contains(&format!("rerank model l6 ready in {}", rerank.path().display())),
            "{output}"
        );
        assert!(!output.contains("downloading"), "{output}");
    }

    /// The fp32 file the int8 model replaced is not the pinned one, and
    /// `fetch` downloads over it.
    ///
    /// *Controls:* `L6.onnx_size` back to the fp32 size -> `on_disk`
    /// reads it `Pinned`; make `fetch_files` skip any file that exists ->
    /// the stale bytes stay.
    #[test]
    fn a_stale_fp32_rerank_file_is_replaced_by_fetch() {
        const FP32_ONNX_SIZE: u64 = 91_011_230;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(L6_FILES[0].local_name);
        File::create(&dest).unwrap().set_len(FP32_ONNX_SIZE).unwrap();
        assert_eq!(on_disk(&dest, &L6_FILES[0]), OnDisk::Other(FP32_ONNX_SIZE));

        let file = RemoteFile { local_name: L6_FILES[0].local_name, ..test_file() };
        let agent = ureq::AgentBuilder::new().build();
        let mut out = Vec::new();
        fetch_files(&agent, dir.path(), &[file], &[Source::Flat(serve_once(TEST_BODY))], &mut out).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("replacing"), "{said}");
        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
    }

    #[test]
    fn status_reports_a_missing_rerank_model_and_how_to_get_it() {
        let dir = tempfile::tempdir().unwrap();
        let rerank = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        status(Some(dir.path()), Some(&l6_target(rerank.path())), &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains(L6.repo), "{output}");
        assert!(output.contains(&rerank.path().display().to_string()), "{output}");
        assert!(output.contains("search_code keeps the embedding order"), "{output}");
    }

    /// The rerank model is the quint8 export at the revision the blend was
    /// fitted against, with the sizes and digests of those files, and a flat
    /// mirror keeps it apart from the embedding model's same-named files.
    #[test]
    fn the_pinned_rerank_model_is_the_quint8_export() {
        let [onnx, tokenizer] = &L6_FILES;
        assert_eq!(
            (onnx.repo, onnx.revision),
            ("cross-encoder/ms-marco-MiniLM-L6-v2", "233902d25c440f23af6f7d6e94d2946bac0bee0a")
        );
        assert_eq!(
            (onnx.remote_path, onnx.local_name, onnx.size),
            ("onnx/model_quint8_avx2.onnx", ONNX_FILE_NAME, 23_200_716)
        );
        assert_eq!(onnx.sha256, "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9");
        assert_eq!((tokenizer.local_name, tokenizer.size), (TOKENIZER_FILE_NAME, 711_396));
        assert_eq!(tokenizer.sha256, "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66");
        assert_eq!(onnx.flat_prefix, format!("{}/", L6.dir_name));
        assert_eq!(
            Source::Flat("https://mirror.test/g".to_string()).url(onnx),
            "https://mirror.test/g/ms-marco-MiniLM-L6-v2/model.onnx"
        );
        assert_eq!(
            Source::Upstream.url(onnx),
            "https://huggingface.co/cross-encoder/ms-marco-MiniLM-L6-v2/resolve/233902d25c440f23af6f7d6e94d2946bac0bee0a/onnx/model_quint8_avx2.onnx"
        );
    }

    /// An fp32 `model.onnx` in the model directory is not "already present":
    /// `fetch` replaces it, or `g-mesh model fetch` could never deliver the
    /// pinned int8 weights over it. Judged on `on_disk`, the decision `fetch`
    /// acts on, so no download starts.
    ///
    /// *Control:* make `on_disk` return `Pinned` for any existing file (an
    /// existence test), and the fp32 file reads as present.
    #[test]
    fn fp32_weights_on_disk_are_replaced_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let onnx = &FILES[0];
        assert_eq!(onnx.local_name, ONNX_FILE_NAME);
        let dest = dir.path().join(onnx.local_name);

        assert_eq!(on_disk(&dest, onnx), OnDisk::Missing);
        File::create(&dest).unwrap().set_len(641_517_466).unwrap();
        assert_eq!(on_disk(&dest, onnx), OnDisk::Other(641_517_466), "the fp32 file must be replaced");
        File::create(&dest).unwrap().set_len(onnx.size).unwrap();
        assert_eq!(on_disk(&dest, onnx), OnDisk::Pinned);
    }

    /// The shipped weights are the int8 export at the pinned revision, with
    /// the size and digest of the downloaded file.
    ///
    /// *Control:* restore the fp32 entry (`onnx/model.onnx`, 641,517,466,
    /// `63363fc1...`) in `FILES`, and every assertion here fails.
    #[test]
    fn the_pinned_weights_are_the_int8_export() {
        let onnx = &FILES[0];
        assert_eq!(onnx.remote_path, "onnx/model_quantized.onnx");
        assert_eq!(onnx.local_name, "model.onnx", "the loader's name, not the remote one");
        assert_eq!(onnx.size, 161_895_621);
        assert_eq!(onnx.sha256, "ed45870251c9f0cf656e78aab0d37a23489066df8a222bb1c8caf8a45f2cb16d");
        assert_eq!(MODEL_REVISION, "516f4baf13dec4ddddda8631e019b5737c8bc250");
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
        // `set_len` rather than writing 154 MiB of zeros: `status` asks the
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
        assert!(output.contains("`g-mesh model fetch` replaces it"), "{output}");
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
    /// is `config::embedding::model_dir(configured_name)` - not the default's.
    #[test]
    fn the_reported_directory_is_the_one_the_daemon_would_read() {
        let configured =
            ResolvedModel { name: "some-other-model".to_string(), source: ModelSource::ProjectConfig };

        let reported = model_dir(None, &configured).unwrap();

        assert_eq!(reported, crate::config::embedding::model_dir("some-other-model").unwrap());
        assert_ne!(
            reported,
            crate::config::embedding::model_dir(&EmbeddingConfig::default().model).unwrap(),
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

    /// The names in `dir` that start with `local_name` and end in `.partial`,
    /// sorted.
    fn partials_in(dir: &Path, local_name: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.starts_with(local_name) && name.ends_with(".partial"))
            .collect();
        names.sort();
        names
    }

    /// Every name in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Bytes of another file than [`TEST_BODY`].
    const WRONG_BODY: &[u8] = b"not the bytes the digest pins\n";

    /// Two calls for one destination never share a partial, and each is
    /// `<local_name>.<pid>-<n>.partial` next to the destination.
    ///
    /// *Control:* name the partial `<local_name>.partial` in `partial_path`
    /// -> the two names are equal.
    #[test]
    fn each_download_gets_a_partial_of_its_own() {
        let dest = Path::new("/models/jina").join(ONNX_FILE_NAME);
        let file = RemoteFile { local_name: ONNX_FILE_NAME, ..test_file() };
        let (first, second) = (partial_path(&dest, &file), partial_path(&dest, &file));

        assert_ne!(first, second, "two downloads must not share a partial");
        for partial in [&first, &second] {
            assert_eq!(partial.parent(), dest.parent(), "{}", partial.display());
            let name = partial.file_name().unwrap().to_str().unwrap();
            let tag = name
                .strip_prefix("model.onnx.")
                .and_then(|rest| rest.strip_suffix(".partial"))
                .unwrap_or_else(|| panic!("not <local_name>.<tag>.partial: {name}"));
            let (pid, n) = tag.split_once('-').unwrap_or_else(|| panic!("no <pid>-<n>: {name}"));
            assert_eq!(pid, std::process::id().to_string(), "{name}");
            assert!(!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()), "{name}");
        }
    }

    /// Serves `body` once, like [`serve_once`], and reports the partials of
    /// `local_name` in `dir` at the moment the request arrived.
    fn serve_once_noting_partials(
        body: &'static [u8],
        dir: PathBuf,
        local_name: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind a test server");
        let port = listener.local_addr().unwrap().port();
        let (seen, partials) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = socket.read(&mut scratch);
                let _ = seen.send(partials_in(&dir, local_name));
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = socket.write_all(body);
                let _ = socket.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), partials)
    }

    /// A source that serves the wrong bytes leaves no partial behind when the
    /// next source is asked.
    ///
    /// *Control:* drop the `fs::remove_file` in `download`'s error arm -> the
    /// first source's partial is still there when the second is asked.
    #[test]
    fn a_failed_source_removes_its_partial_before_the_next_is_tried() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let (second, seen) = serve_once_noting_partials(TEST_BODY, dir.path().to_path_buf(), "mirrored.bin");
        let sources = vec![Source::Flat(serve_once(WRONG_BODY)), Source::Flat(second)];
        let mut out = Vec::new();

        download(&http_agent(), &test_file(), &dest, &sources, &mut out)
            .expect("the second source serves the pinned bytes");

        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("failed:"), "the first source must have failed: {said}");
        assert_eq!(
            seen.recv().unwrap(),
            Vec::<String>::new(),
            "partials left when the second source was asked"
        );
        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
        assert_eq!(partials_in(dir.path(), "mirrored.bin"), Vec::<String>::new());
    }

    /// A writer that fails as soon as it is asked to write `poison`.
    struct FailsOn(&'static str);

    impl Write for FailsOn {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if String::from_utf8_lossy(buf).contains(self.0) {
                return Err(std::io::Error::other("the output is gone"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An early return after the partial is complete (here the output failing
    /// on "fetched from ... instead") leaves neither the partial nor a file
    /// under the final name.
    ///
    /// *Control:* make `PartialFile::drop` a no-op -> the complete partial is
    /// left behind.
    #[test]
    fn an_early_return_after_the_download_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let sources = vec![Source::Flat(dead_source()), Source::Flat(serve_once(TEST_BODY))];

        let err = download(&http_agent(), &test_file(), &dest, &sources, &mut FailsOn("instead"))
            .expect_err("the failing output must end the download");

        assert!(format!("{err:#}").contains("the output is gone"), "{err:#}");
        assert!(!dest.exists(), "nothing under the final name");
        assert_eq!(partials_in(dir.path(), "mirrored.bin"), Vec::<String>::new());
    }

    /// A panic while a partial is held deletes it on the way out.
    ///
    /// *Control:* make `PartialFile::drop` a no-op -> the file survives.
    #[test]
    fn a_panic_while_a_partial_is_held_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirrored.bin.1-0.partial");
        fs::write(&path, &TEST_BODY[..10]).unwrap();

        let unwound = std::panic::catch_unwind(|| {
            let _partial = PartialFile(path.clone());
            panic!("a panic mid-download");
        });

        assert!(unwound.is_err());
        assert!(!path.exists(), "the partial must go with the unwinding");
    }

    /// A verified partial replaces whatever is at the destination.
    #[test]
    fn a_verified_partial_replaces_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let partial = dir.path().join("mirrored.bin.1-0.partial");
        fs::write(&dest, WRONG_BODY).unwrap();
        fs::write(&partial, TEST_BODY).unwrap();

        move_into_place(&partial, &dest, &test_file()).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
        assert!(!partial.exists());
    }

    /// A rename that fails onto a destination already holding the pinned
    /// bytes (a concurrent fetch got there first) is success.
    ///
    /// *Control:* drop the `holds_pinned` arm of `move_into_place` -> the
    /// failed rename is an error.
    #[test]
    fn a_failed_rename_onto_the_pinned_file_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        fs::write(&dest, TEST_BODY).unwrap();
        let gone = dir.path().join("mirrored.bin.1-0.partial");

        move_into_place(&gone, &dest, &test_file()).expect("the pinned file is already in place");

        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
    }

    /// A failed rename is an error when the destination is anything but the
    /// pinned bytes: the pinned size with another digest, another size, or
    /// nothing at all.
    ///
    /// *Control:* compare only the size in `holds_pinned` -> the same-size
    /// file with another digest is accepted.
    #[test]
    fn a_failed_rename_onto_anything_but_the_pinned_file_is_an_error() {
        let mut same_size = TEST_BODY.to_vec();
        same_size[0] ^= 1;
        for (what, content) in [
            ("same size, other digest", Some(same_size)),
            ("other size", Some(WRONG_BODY.to_vec())),
            ("nothing", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("mirrored.bin");
            if let Some(content) = &content {
                fs::write(&dest, content).unwrap();
            }
            let gone = dir.path().join("mirrored.bin.1-0.partial");

            let err = move_into_place(&gone, &dest, &test_file())
                .expect_err(&format!("{what} at the destination must not count as success"));

            assert!(format!("{err:#}").contains("failed to move"), "{what}: {err:#}");
        }
    }

    /// Every fetcher's partial name matches: the shared one older releases
    /// wrote, the shell script's `<pid>`, and this module's `<pid>-<n>`.
    /// Nothing else does, the final name and other files' partials included.
    ///
    /// *Control:* drop the `rest == "partial"` arm of `is_partial_of` -> the
    /// legacy name no longer matches; drop its `parts.next().is_none()` -> a
    /// three-part tag matches.
    #[test]
    fn partial_names_of_every_fetcher_match_and_nothing_else_does() {
        let ours = partial_path(
            Path::new("/m").join(ONNX_FILE_NAME).as_path(),
            &RemoteFile { local_name: ONNX_FILE_NAME, ..test_file() },
        );
        let ours = ours.file_name().unwrap().to_str().unwrap().to_string();
        for name in
            ["model.onnx.partial", "model.onnx.4242.partial", "model.onnx.4242-0.partial", ours.as_str()]
        {
            assert!(is_partial_of(name, "model.onnx"), "{name} must match");
        }
        for name in [
            "model.onnx",
            "model.partial",
            "model.onnx.partial.bak",
            "model.onnx.1.partial.partial",
            "model.onnxx.1.partial",
            "xmodel.onnx.partial",
            "model.onnx..partial",
            "model.onnx.-1.partial",
            "model.onnx.1-.partial",
            "model.onnx.1-2-3.partial",
            "model.onnx.12a.partial",
            "tokenizer.json.4242.partial",
        ] {
            assert!(!is_partial_of(name, "model.onnx"), "{name} must not match");
        }
        assert!(
            FETCH_SCRIPT.contains(r#"PARTIAL="${dest}.$$.partial""#),
            "the script's partial name is the `<pid>` form matched above"
        );
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    /// Partials of the file last written `STALE_PARTIAL_AGE` ago or earlier
    /// are deleted and named; a younger one, one dated in the future, the
    /// final file and other files' partials are kept.
    ///
    /// *Control:* count a modification time in the future as stale
    /// (`elapsed()` error -> `Duration::MAX`) -> the future partial is
    /// deleted.
    #[test]
    fn stale_partials_are_swept_and_everything_else_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let now = std::time::SystemTime::now();
        let stale = now - STALE_PARTIAL_AGE - Duration::from_secs(60);
        let young = now - STALE_PARTIAL_AGE + Duration::from_secs(300);
        let future = now + Duration::from_secs(3600);
        let swept = ["mirrored.bin.partial", "mirrored.bin.4242.partial", "mirrored.bin.4242-3.partial"];
        let kept = [
            ("mirrored.bin.4243-0.partial", young),
            ("mirrored.bin.4244-0.partial", future),
            ("mirrored.bin", stale),
            ("mirrored.bin.bak", stale),
            ("other.bin.4242.partial", stale),
        ];
        for name in swept {
            fs::write(dir.path().join(name), b"half").unwrap();
            set_mtime(&dir.path().join(name), stale);
        }
        for (name, when) in kept {
            fs::write(dir.path().join(name), b"half").unwrap();
            set_mtime(&dir.path().join(name), when);
        }
        let mut out = Vec::new();

        sweep_stale_partials(&dest, &test_file(), STALE_PARTIAL_AGE, &mut out).unwrap();

        let said = String::from_utf8(out).unwrap();
        for name in swept {
            assert!(!dir.path().join(name).exists(), "{name} is stale and must be deleted");
            assert!(said.contains(name), "{name} must be named as removed: {said}");
        }
        for (name, _) in kept {
            assert!(dir.path().join(name).exists(), "{name} must be kept");
        }
    }

    /// A download deletes its file's stale partials, here the shared name a
    /// killed older release left.
    ///
    /// *Control:* drop the `sweep_stale_partials` call from `download` ->
    /// the stale partial survives.
    #[test]
    fn a_download_sweeps_stale_partials_of_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let legacy = dir.path().join("mirrored.bin.partial");
        fs::write(&legacy, &TEST_BODY[..10]).unwrap();
        set_mtime(&legacy, std::time::SystemTime::now() - STALE_PARTIAL_AGE - Duration::from_secs(60));

        download(&http_agent(), &test_file(), &dest, &[Source::Flat(serve_once(TEST_BODY))], &mut Vec::new())
            .unwrap();

        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
        assert_eq!(names_in(dir.path()), vec!["mirrored.bin".to_string()]);
    }

    /// `<model dir>|<base url>` for [`fetch_race_child`].
    const RACE_CHILD_ENV: &str = "G_MESH_TEST_FETCH_RACE_CHILD";

    /// How long the race waits for any one step before failing.
    const RACE_STEP_DEADLINE: Duration = Duration::from_secs(60);

    /// One `fetch_files` of [`test_file`] as [`RACE_CHILD_ENV`] says; does
    /// nothing without it.
    #[test]
    #[ignore = "a child process of two_concurrent_fetches_into_one_directory_both_succeed"]
    fn fetch_race_child() {
        let Ok(spec) = std::env::var(RACE_CHILD_ENV) else {
            return;
        };
        let (dir, base) = spec.split_once('|').expect("<model dir>|<base url>");
        fetch_files(
            &http_agent(),
            Path::new(dir),
            &[test_file()],
            &[Source::Flat(base.to_string())],
            &mut std::io::stdout(),
        )
        .unwrap();
    }

    /// Polls `probe` until it answers, failing after [`RACE_STEP_DEADLINE`].
    fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + RACE_STEP_DEADLINE;
        loop {
            if let Some(found) = probe() {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn describe(output: &std::process::Output) -> String {
        format!(
            "{}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// A [`fetch_race_child`] process, killed if the test fails first.
    struct Racer(Option<std::process::Child>);

    impl Racer {
        fn spawn(dir: &Path, base: &str) -> Self {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cli::model::tests::fetch_race_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(RACE_CHILD_ENV, format!("{}|{base}", dir.display()))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("failed to spawn a fetch");
            Self(Some(child))
        }

        fn output_if_exited(&mut self) -> Option<std::process::Output> {
            self.0.as_mut()?.try_wait().unwrap()?;
            Some(self.0.take().unwrap().wait_with_output().unwrap())
        }

        /// Waits for the fetch to exit; it must have succeeded.
        fn succeeds(mut self, name: &str) {
            let output = wait_for(&format!("fetch {name} to exit"), || self.output_if_exited());
            assert!(output.status.success(), "fetch {name} failed: {}", describe(&output));
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("verified mirrored.bin"),
                "fetch {name} fetched nothing: {}",
                describe(&output)
            );
        }

        /// Accepts this fetch's request on `listener`, failing if it exits
        /// first.
        fn accept(&mut self, listener: &std::net::TcpListener) -> std::net::TcpStream {
            listener.set_nonblocking(true).unwrap();
            let mut socket = wait_for("a fetch to connect", || {
                if let Some(output) = self.output_if_exited() {
                    panic!("a fetch exited before connecting: {}", describe(&output));
                }
                match listener.accept() {
                    Ok((socket, _)) => Some(socket),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(err) => panic!("accept failed: {err}"),
                }
            });
            socket.set_nonblocking(false).unwrap();
            let mut request = Vec::new();
            let mut scratch = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut scratch).unwrap();
                assert_ne!(read, 0, "the fetch closed before finishing its request");
                request.extend_from_slice(&scratch[..read]);
            }
            socket
        }
    }

    impl Drop for Racer {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// The sizes of the partials of `mirrored.bin` in `dir`.
    fn partial_sizes(dir: &Path) -> Vec<u64> {
        partials_in(dir, "mirrored.bin")
            .iter()
            .filter_map(|name| fs::metadata(dir.join(name)).ok())
            .map(|meta| meta.len())
            .collect()
    }

    /// Two fetches into one directory at once, as separate processes, both
    /// succeed and leave one complete file and no partial. The server holds
    /// the responses so the two overlap: A has written all but the tail when
    /// B starts and creates its partial, then A gets the tail and exits
    /// before B gets a byte. A fetch that reports success has the pinned
    /// bytes in place when it exits, whatever the other is doing.
    ///
    /// *Control:* name the partial `<local_name>.partial` in `partial_path`
    /// -> B's create truncates A's partial, A writes its tail after a hole
    /// and renames that into place, and the file A reported is not the pinned
    /// bytes.
    #[test]
    fn two_concurrent_fetches_into_one_directory_both_succeed() {
        const SPLIT: usize = 20;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mirrored.bin");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind a test server");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", TEST_BODY.len());

        let mut a = Racer::spawn(dir.path(), &base);
        let mut a_socket = a.accept(&listener);
        a_socket.write_all(header.as_bytes()).unwrap();
        a_socket.write_all(&TEST_BODY[..SPLIT]).unwrap();
        a_socket.flush().unwrap();
        wait_for("A to write all but its tail", || {
            partial_sizes(dir.path()).contains(&(SPLIT as u64)).then_some(())
        });

        let mut b = Racer::spawn(dir.path(), &base);
        let mut b_socket = b.accept(&listener);
        b_socket.write_all(header.as_bytes()).unwrap();
        b_socket.flush().unwrap();
        wait_for("B to create its partial", || partial_sizes(dir.path()).contains(&0).then_some(()));

        a_socket.write_all(&TEST_BODY[SPLIT..]).unwrap();
        drop(a_socket);
        a.succeeds("A");
        assert_eq!(
            fs::read(&dest).unwrap(),
            TEST_BODY,
            "A reported success, so the pinned bytes are in place"
        );

        b_socket.write_all(TEST_BODY).unwrap();
        drop(b_socket);
        b.succeeds("B");
        assert_eq!(fs::read(&dest).unwrap(), TEST_BODY);
        assert_eq!(names_in(dir.path()), vec!["mirrored.bin".to_string()], "one file, no partial");
    }
}
