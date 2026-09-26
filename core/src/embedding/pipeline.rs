//! Turns a written node into a stored vector - the piece that sits between
//! [`storage::write::apply_diff`] (what actually lands a node's row) and
//! [`storage::vectors::insert`] (what actually stores an embedding).
//!
//! # What gets embedded, and what doesn't
//!
//! A node's embeddable text is its doc comment and its signature, in that
//! order, joined by a blank line - the doc comment reads as prose and the
//! signature as code, and putting the prose first is what
//! `jina-embeddings-v2-base-code` (a code-context-aware model) was shown to
//! respond best to for natural-language queries in its own model card. A node
//! with neither (the overwhelming majority - most symbols have no docstring)
//! is skipped entirely rather than embedding an empty string: an empty
//! string still tokenizes to something (see `EmbeddingModel::embed`'s special
//! tokens), and a vector for "nothing" would only ever be a false positive in
//! a similarity search, never a true one.
//!
//! # Where the model lives
//!
//! Loading the ONNX model is expensive - hundreds of MiB, measured at several
//! seconds on real hardware - so [`EmbeddingPipeline::load`] does not load it
//! at all. It only stores `config` behind an [`OnceLock`]; the first real
//! [`apply`](Self::apply) call is what resolves it, synchronously, on
//! whichever thread that call happens to run on. This is deliberate, not an
//! optimization applied for its own sake: an earlier version loaded the model
//! on a background thread kicked off from `daemon::run`, and even that -
//! never blocking, just *existing* - measurably cost daemon startup enough to
//! blow through `serving_while_indexing`'s 1-second "an already-walked
//! project restarts fast" budget and `cli::clean`'s 10-second "the daemon is
//! listening" wait under load, because a bare `thread::spawn` still competes
//! with the plugin spawn and the accept loop for scheduling. Nothing about
//! daemon startup may cost more than it did before this feature existed;
//! [`EmbeddingPipeline`] is still loaded once and held for a whole daemon's
//! lifetime - the same shape `daemon::lifecycle::PluginSupervisor` already
//! holds its plugin process handle in - it just does not pay for that load
//! until something has actually asked to embed.
//!
//! A model that fails to load (not fetched yet, wrong directory, corrupt
//! files) does not fail the pipeline - it disables it. Indexing without
//! semantic search available is a strictly better outcome than an indexer
//! that refuses to start because an optional model is missing; the daemon
//! logs once and every write from then on simply skips embedding, exactly
//! the way a failed semantic pass is reported and dropped rather than taking
//! the reparse down with it (`watcher::apply::apply_file_change`'s doc
//! comment argues the identical trade-off for the type checker).
//!
//! # The embedding cache
//!
//! [`EmbeddingPipeline::compute`] is the one place every reindex path gets
//! its vectors from, so it is also the one place the machine-wide embedding
//! cache ([`crate::embedding::cache`]) sits: a text the cache already holds
//! a vector for, under the same model fingerprint, is never embedded again,
//! and the model is loaded only on the first text it does not hold. Design:
//! ADR 0007 (`docs/adr/0007-embedding-cache.md`).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::config::EmbeddingConfig;
use crate::embedding::cache::{self, EmbeddingCache, Hash, OpenError};
use crate::embedding::model::{default_model_dir, EmbeddingModel, ONNX_FILE_NAME, TOKENIZER_FILE_NAME};
use crate::storage::vectors;
use crate::storage::write::Diff;

/// Turns one text into one vector. [`EmbeddingModel`] in production; a
/// deterministic fake in tests that count inference calls.
pub(crate) trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

impl Embedder for EmbeddingModel {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        EmbeddingModel::embed(self, text)
    }
}

type Loader = Box<dyn Fn(&Path) -> Result<Box<dyn Embedder>> + Send + Sync>;

/// Where the machine-wide embedding cache ([`crate::embedding::cache`])
/// lives and how large it may grow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheSettings {
    pub path: PathBuf,
    pub max_bytes: u64,
}

impl CacheSettings {
    pub fn new(path: PathBuf, max_size_mb: u64) -> Self {
        Self { path, max_bytes: max_size_mb.saturating_mul(1024 * 1024) }
    }

    /// What the global config and [`cache::CACHE_ENV`] ask for: `None` when
    /// the cache is switched off by either.
    fn from_global_config() -> Option<Self> {
        if env_disables_cache(std::env::var_os(cache::CACHE_ENV).as_deref()) {
            return None;
        }
        let config = crate::config::read_global_config().map(|config| config.embedding_cache).unwrap_or_else(|err| {
            eprintln!("g-mesh: failed to read the global config ({err:#}) - using the embedding cache's defaults");
            Default::default()
        });
        if !config.enabled {
            return None;
        }
        match cache::default_path() {
            Ok(path) => Some(Self::new(path, config.max_size_mb)),
            Err(err) => {
                eprintln!("g-mesh: the embedding cache has no location ({err:#}) - embedding without it");
                None
            }
        }
    }
}

fn env_disables_cache(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("off"))
}

/// The cache as one process sees it. Opened on the first `compute` that has
/// something to embed, never by [`EmbeddingPipeline::load`].
enum CacheSlot {
    /// Not opened yet: the settings to open with, `None` to read them from
    /// the global config at that moment.
    Unopened(Option<CacheSettings>),
    Active(ActiveCache),
    /// Switched off, no model files to fingerprint, or unusable even after
    /// being recreated: every text is embedded, as without a cache.
    Off,
}

struct ActiveCache {
    cache: EmbeddingCache,
    settings: CacheSettings,
    fingerprint: Hash,
    model_id: i64,
}

/// What a run of [`EmbeddingPipeline::compute`] calls did, summed by the
/// caller over one reindex unit and reported by
/// [`EmbeddingPipeline::finish_unit`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmbedStats {
    /// Nodes with embeddable text, while a model or a cache was available.
    pub texts: usize,
    /// Vectors served from the cache.
    pub hits: usize,
    /// Vectors computed by the model: the cache's misses.
    pub embedded: usize,
    /// Lookups or inserts the cache failed; each one degraded to "no cache"
    /// for its batch.
    pub cache_errors: usize,
    /// Rows this unit added to the cache.
    pub inserted: usize,
}

/// A lazily-resolved embedding model plus the version string stored rows are
/// tagged with, held for as long as the daemon runs.
///
/// `version` is `config.embedding.model` - the project's *configured* choice.
/// [`apply`](Self::apply) keeps `meta.embedding_model` mirroring this same
/// value on every call (see `storage::schema::set_embedding_model`), so a
/// project's vector rows and its `meta` row always agree on which model
/// produced them.
///
/// The cache's mutex is the innermost lock: it is taken for one batch lookup
/// or one batch insert, never across inference or the model's hashing and
/// never while taking another lock.
pub struct EmbeddingPipeline {
    config: EmbeddingConfig,
    /// `None`: `default_model_dir(config.model)`.
    model_dir: Option<PathBuf>,
    loader: Loader,
    model: OnceLock<Option<Box<dyn Embedder>>>,
    cache: Mutex<CacheSlot>,
}

impl EmbeddingPipeline {
    /// Stores `config` for a later load - see the module doc's "Where the
    /// model lives" section for why this does no I/O and returns instantly.
    /// The embedding cache follows the global config (`[embeddingCache]`,
    /// `G_MESH_EMBEDDING_CACHE`), read when it is first needed.
    pub fn load(config: &EmbeddingConfig) -> Self {
        Self::build(config, None, CacheSlot::Unopened(None))
    }

    /// [`load`](Self::load) with the cache given explicitly: at `cache`, or
    /// none at all for `None`, whatever the global config says.
    pub fn load_with_cache(config: &EmbeddingConfig, cache: Option<CacheSettings>) -> Self {
        let slot = match cache {
            Some(settings) => CacheSlot::Unopened(Some(settings)),
            None => CacheSlot::Off,
        };
        Self::build(config, None, slot)
    }

    fn build(config: &EmbeddingConfig, model_dir: Option<PathBuf>, cache: CacheSlot) -> Self {
        Self {
            config: config.clone(),
            model_dir,
            loader: Box::new(|dir| Ok(Box::new(EmbeddingModel::load(dir)?) as Box<dyn Embedder>)),
            model: OnceLock::new(),
            cache: Mutex::new(cache),
        }
    }

    /// A pipeline with no model at all - what every caller that does not
    /// care about embeddings (most of the test suite) constructs instead of
    /// depending on real weights being present on disk. Resolves instantly,
    /// same as [`load`](Self::load): nothing is loaded either way until
    /// [`apply`](Self::apply) is actually called, and this pre-fills that
    /// result with "no model" so a disabled pipeline never tries.
    pub fn disabled() -> Self {
        let mut pipeline = Self::build(&EmbeddingConfig::default(), None, CacheSlot::Off);
        pipeline.model = OnceLock::from(None);
        pipeline
    }

    /// A pipeline whose model is whatever `loader` builds from `model_dir`,
    /// for tests that count inference calls. `model_dir` must hold the two
    /// files the cache fingerprints (`model.onnx`, `tokenizer.json`).
    #[cfg(test)]
    pub(crate) fn with_loader(
        model_dir: &Path,
        loader: impl Fn(&Path) -> Result<Box<dyn Embedder>> + Send + Sync + 'static,
        cache: Option<CacheSettings>,
    ) -> Self {
        let slot = match cache {
            Some(settings) => CacheSlot::Unopened(Some(settings)),
            None => CacheSlot::Off,
        };
        let mut pipeline = Self::build(&EmbeddingConfig::default(), Some(model_dir.to_path_buf()), slot);
        pipeline.loader = Box::new(loader);
        pipeline
    }

    fn model_dir(&self) -> Result<PathBuf> {
        match &self.model_dir {
            Some(dir) => Ok(dir.clone()),
            None => default_model_dir(&self.config.model),
        }
    }

    /// The loaded model, resolving `config`'s model directory and loading it
    /// the first time this is called. A missing or unreadable model does not
    /// panic or propagate an error - see the module doc for why - it is
    /// reported to stderr once and every call after the first (loaded or not)
    /// returns instantly from the cached result.
    fn model(&self) -> Option<&dyn Embedder> {
        self.model
            .get_or_init(|| match self.model_dir().and_then(|dir| (self.loader)(&dir)) {
                Ok(model) => Some(model),
                Err(err) => {
                    eprintln!(
                        "g-mesh daemon: embedding model {:?} is not available ({err:#}) - \
                         indexing will continue without semantic search",
                        self.config.model
                    );
                    None
                }
            })
            .as_deref()
    }

    /// Cheap check for whether the embedding backfill pass
    /// (`embedding::backfill::run`) has anything to do at all, *without*
    /// paying [`model`](Self::model)'s load cost to find out.
    ///
    /// A pipeline this same process has already resolved - loaded, failed to
    /// load, or [`disabled`](Self::disabled) - answers from that cached
    /// outcome (`self.model.get()`), never re-checking the filesystem: once
    /// [`model`](Self::model) has decided, that decision is the one source of
    /// truth, and a `disabled` pipeline in particular must read as
    /// unavailable regardless of what happens to exist under
    /// `default_model_dir` (a test's real weights, say) - it was
    /// constructed to never try. Otherwise - the common cold-start case,
    /// nothing has asked to embed yet - this falls back to the one fact that
    /// can be checked without loading anything: do `model.onnx` and
    /// `tokenizer.json` exist in the directory [`model`](Self::model) would
    /// resolve to. `false` from either path means [`model`](Self::model)
    /// would return `None` if called right now; `true` is not a promise it
    /// will *succeed* (the files could still be corrupt), only that there is
    /// something worth the load's cost.
    pub fn is_available(&self) -> bool {
        if let Some(loaded) = self.model.get() {
            return loaded.is_some();
        }
        let Ok(dir) = self.model_dir() else { return false };
        model_files_exist(&dir)
    }

    /// Embeds a free-text query (`search_code`'s input) with the same model
    /// and pooling `apply` embeds node text with, so a query vector and a
    /// stored node vector live in the same space and are comparable by cosine
    /// distance. `None` when no model is loaded (see [`model`](Self::model))
    /// or the model itself fails on this input - `search_code` reports either
    /// as "semantic search is unavailable" rather than a tool-level crash, the
    /// same best-effort posture [`apply`](Self::apply) already takes with node
    /// text.
    pub fn embed_query(&self, text: &str) -> Option<Vec<f32>> {
        let model = self.model()?;
        match model.embed(text) {
            Ok(embedding) => Some(embedding),
            Err(err) => {
                eprintln!("g-mesh daemon: failed to embed search query ({err:#})");
                None
            }
        }
    }

    /// Embeds and stores every upserted node in `diff` that has embeddable
    /// text. A no-op, quickly, if no model is loaded (or loadable).
    ///
    /// Best-effort per node, matching how a bulk walk treats one unreadable
    /// line (`daemon::bulk_index::ingest`) and how a reparse treats a failed
    /// semantic pass: one node's inference failing (a pathological input, an
    /// ONNX runtime error) is reported and skipped, not allowed to lose the
    /// rest of the diff's embeddings or - worse - the diff's already-committed
    /// rows.
    ///
    /// [`compute`](Self::compute) then [`store`](Self::store) with no lock
    /// released in between, for a caller that holds none: a disabled
    /// pipeline, or a test. Every production path calls the two halves
    /// itself so inference runs outside the store lock.
    pub fn apply(&self, conn: &Connection, diff: &Diff) -> Result<()> {
        let computed = self.compute(diff, &mut EmbedStats::default());
        self.store(conn, &computed);
        Ok(())
    }

    /// The database-free half of [`apply`](Self::apply): returns one
    /// [`ComputedEmbedding`] for each upserted node in `diff` with embeddable
    /// text, in `diff`'s order, and adds what it did to `stats`. Empty,
    /// quickly, if there is neither a model nor a cache to answer from.
    ///
    /// Texts are looked up in the embedding cache first, all of the batch's
    /// keys at once; only the misses reach the model, which is loaded on the
    /// first miss - so a batch the cache fully answers never loads it - and
    /// the misses' vectors are inserted back in one transaction. A cache
    /// failure of any kind degrades to embedding everything, never to an
    /// error.
    ///
    /// Takes no `Connection`: a caller holding the project store's lock can
    /// release it around this call, which is pure CPU (plus the cache's own
    /// short transactions and, once, the model's multi-second load).
    ///
    /// Best-effort per node, exactly as `apply` always was: one node's
    /// inference failing is reported and skipped, never allowed to lose the
    /// rest of the diff's embeddings.
    pub fn compute(&self, diff: &Diff, stats: &mut EmbedStats) -> Vec<ComputedEmbedding> {
        if matches!(self.model.get(), Some(None)) {
            return Vec::new();
        }
        let pending: Vec<(&str, String)> = diff
            .upsert_nodes
            .iter()
            .filter_map(|node| {
                text_to_embed(node.doc_comment.as_deref(), node.signature.as_deref())
                    .map(|text| (node.id.as_str(), text))
            })
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }

        let keys: Vec<Hash> = pending.iter().map(|(_, text)| cache::text_hash(text)).collect();
        let mut cached = self.cache_lookup(&keys, stats);
        if cached.is_none() && self.model().is_none() {
            return Vec::new();
        }
        stats.texts += pending.len();

        let mut computed = Vec::with_capacity(pending.len());
        let mut fresh: Vec<(Hash, usize)> = Vec::new();
        for (index, (node_id, text)) in pending.into_iter().enumerate() {
            if let Some(embedding) = cached.as_mut().and_then(|hits| hits[index].take()) {
                stats.hits += 1;
                computed.push(ComputedEmbedding { node_id: node_id.to_string(), embedding, text });
                continue;
            }
            let Some(model) = self.model() else { continue };
            match model.embed(&text) {
                Ok(embedding) => {
                    stats.embedded += 1;
                    fresh.push((keys[index], computed.len()));
                    computed.push(ComputedEmbedding { node_id: node_id.to_string(), embedding, text });
                }
                Err(err) => {
                    eprintln!(
                        "g-mesh daemon: failed to embed node {node_id} ({err:#}) - it is left unembedded"
                    )
                }
            }
        }

        if cached.is_some() && !fresh.is_empty() {
            let entries: Vec<(Hash, &[f32])> =
                fresh.iter().map(|(key, at)| (*key, computed[*at].embedding.as_slice())).collect();
            self.cache_insert(&entries, stats);
        }
        computed
    }

    /// Reports one reindex unit's `stats` - a bulk walk, a workspace
    /// reindex, a backfill - as one stderr line, then trims the cache if the
    /// unit added to it. Silent for a unit that had nothing to embed.
    pub fn finish_unit(&self, label: &str, stats: &EmbedStats, elapsed: Duration) {
        if stats.texts == 0 {
            return;
        }
        log_stats(label, stats, elapsed);
        if stats.inserted > 0 {
            self.collect_garbage();
        }
    }

    /// Reports a single file change's `stats`, only when it embedded
    /// something. Never trims the cache: that is a reindex unit's job.
    pub fn finish_file_change(&self, label: &str, stats: &EmbedStats, elapsed: Duration) {
        if stats.embedded > 0 {
            log_stats(label, stats, elapsed);
        }
    }

    /// The cached vector for each key, or `None` when no cache is active. A
    /// failed lookup is an all-miss batch.
    fn cache_lookup(&self, keys: &[Hash], stats: &mut EmbedStats) -> Option<Vec<Option<Vec<f32>>>> {
        self.open_if_unopened();
        let mut slot = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let CacheSlot::Active(active) = &mut *slot else { return None };
        match active.cache.lookup(active.model_id, keys, cache::today()) {
            Ok(found) => Some(found),
            Err(err) => {
                stats.cache_errors += 1;
                recover(&mut slot, &err, "look up");
                Some(vec![None; keys.len()])
            }
        }
    }

    /// Stores freshly computed vectors; a failed insert drops the batch (the
    /// vectors still reach the project index).
    fn cache_insert(&self, entries: &[(Hash, &[f32])], stats: &mut EmbedStats) {
        let mut slot = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let CacheSlot::Active(active) = &mut *slot else { return };
        match active.cache.insert(active.model_id, entries, cache::today()) {
            Ok(inserted) => stats.inserted += inserted,
            Err(err) => {
                stats.cache_errors += 1;
                recover(&mut slot, &err, "store into");
            }
        }
    }

    fn collect_garbage(&self) {
        let mut slot = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let CacheSlot::Active(active) = &mut *slot else { return };
        let max_bytes = active.settings.max_bytes;
        match active.cache.gc(active.model_id, max_bytes, cache::today()) {
            Ok(outcome) if outcome.models_dropped > 0 || outcome.entries_evicted > 0 => eprintln!(
                "g-mesh: embedding cache trimmed - {} unused model(s) dropped, {} entries evicted",
                outcome.models_dropped, outcome.entries_evicted
            ),
            Ok(_) => {}
            Err(err) => recover(&mut slot, &err, "trim"),
        }
    }

    /// Opens the cache if this is the first time it is needed. Opening
    /// fingerprints the model, which hashes its weights on a cold memo, so it
    /// runs without the cache's mutex held; the result is installed only if
    /// no other thread installed one meanwhile. Afterwards the slot is
    /// `Active`, `Off` (switched off, no model files to fingerprint, or
    /// unusable), or still `Unopened` (another process held the file; tried
    /// again on the next call).
    fn open_if_unopened(&self) {
        let settings = match &*self.cache.lock().unwrap_or_else(PoisonError::into_inner) {
            CacheSlot::Unopened(settings) => settings.clone(),
            _ => return,
        };
        let opened = match settings.or_else(CacheSettings::from_global_config) {
            None => CacheSlot::Off,
            Some(settings) => match self.open_cache(&settings) {
                Ok(Some(active)) => CacheSlot::Active(active),
                Ok(None) => CacheSlot::Off,
                Err(()) => CacheSlot::Unopened(Some(settings)),
            },
        };
        let mut slot = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*slot, CacheSlot::Unopened(_)) {
            *slot = opened;
        }
    }

    /// `Ok(None)` for a cache that cannot be used for this process's life,
    /// `Err(())` for one that is busy right now.
    fn open_cache(&self, settings: &CacheSettings) -> Result<Option<ActiveCache>, ()> {
        let Ok(dir) = self.model_dir() else { return Ok(None) };
        // Without the model's files there is nothing to fingerprint, and no
        // model to have produced a cached vector either.
        if !model_files_exist(&dir) {
            return Ok(None);
        }
        let mut cache = match EmbeddingCache::open(&settings.path) {
            Ok(cache) => cache,
            Err(OpenError::Busy(_)) => return Err(()),
            Err(OpenError::Failed(err)) => {
                disable_notice(&settings.path, &err);
                return Ok(None);
            }
        };
        let mut identity = identify(&cache, &dir);
        if let Err(err) = &identity {
            if cache::is_corrupt(err) {
                drop(cache);
                cache = match EmbeddingCache::recreate(&settings.path, err) {
                    Ok(cache) => cache,
                    Err(OpenError::Busy(_)) => return Err(()),
                    Err(OpenError::Failed(err)) => {
                        disable_notice(&settings.path, &err);
                        return Ok(None);
                    }
                };
                identity = identify(&cache, &dir);
            }
        }
        match identity {
            Ok((fingerprint, model_id)) => {
                Ok(Some(ActiveCache { cache, settings: settings.clone(), fingerprint, model_id }))
            }
            Err(err) if cache::is_busy(&err) => Err(()),
            Err(err) => {
                disable_notice(&settings.path, &err);
                Ok(None)
            }
        }
    }

    /// The database-writing half of [`apply`](Self::apply): stores every
    /// embedding [`compute`](Self::compute) produced, and records the active
    /// embedding model - both no-ops when `computed` is empty, which also
    /// covers "no model loaded" (`compute` never returns anything in that
    /// case), so a disabled pipeline still never claims an active model,
    /// matching `apply`'s original contract.
    ///
    /// # GM-396: a node's content may have moved on since it was computed
    ///
    /// `compute` and `store` run under two separate `conn` locks with the
    /// lock released in between (`watcher::apply::round_trip`,
    /// `daemon::bulk_index::commit`), precisely so that a slow inference step
    /// never holds up a concurrent reader - but that same gap is a window in
    /// which some *other* writer (a second reparse of the same file replayed
    /// by a relaunched plugin, a workspace-triggered per-language re-walk)
    /// can commit a diff of its own for the very node this one is about to
    /// write a vector for. Blindly storing the embedding this call already
    /// paid for would silently attach a stale vector to fresh content - worse
    /// than skipping it, since nothing about a successful `INSERT` would ever
    /// say so, and `storage::connection::open` runs with `foreign_keys OFF`
    /// (see its own doc comment), so a node that was deleted in the meantime
    /// would not even fail the insert - it would leave a vector row for an id
    /// that no longer names anything.
    ///
    /// So before writing, this re-reads the node's *current* row and recomputes
    /// the same `text_to_embed` it derives from - if it no longer exists, or
    /// its doc comment/signature no longer combine to the exact text this
    /// embedding was computed from, the write is skipped: only a node still
    /// present with unchanged content gets its vector stored. A skip here is
    /// not a lost update - whatever wrote the newer content is a diff of its
    /// own, and either already ran (or will run) this same compute-then-store
    /// pair for it, which is what actually keeps the node's vector current.
    ///
    /// Best-effort per node beyond that, same as `compute`'s own inference
    /// step: a single row failing to store is reported and skipped, not
    /// allowed to lose the rest of the batch.
    pub fn store(&self, conn: &Connection, computed: &[ComputedEmbedding]) {
        if computed.is_empty() {
            return;
        }
        if let Err(err) = crate::storage::schema::set_embedding_model(conn, &self.config.model) {
            eprintln!("g-mesh daemon: failed to record the active embedding model ({err:#})");
        }
        for entry in computed {
            match current_embeddable_text(conn, &entry.node_id) {
                Ok(Some(current_text)) if current_text == entry.text => {
                    if let Err(err) =
                        vectors::insert(conn, &entry.node_id, &entry.embedding, &self.config.model)
                    {
                        eprintln!(
                            "g-mesh daemon: failed to store the embedding for node {} ({err:#}) - it is \
                             left unembedded",
                            entry.node_id
                        );
                    }
                }
                // The node is gone, or its content moved on while this
                // embedding was being computed with the lock released - see
                // this method's own "GM-396" doc section. Whoever wrote the
                // newer content owns re-embedding it; this vector would only
                // ever be stale.
                Ok(_) => {}
                Err(err) => eprintln!(
                    "g-mesh daemon: failed to verify node {} before storing its embedding ({err:#}) - it is \
                     left unembedded",
                    entry.node_id
                ),
            }
        }
    }
}

/// The model's fingerprint over the files in `dir`, and its row in `cache`.
fn identify(cache: &EmbeddingCache, dir: &Path) -> Result<(Hash, i64)> {
    let onnx = cache.file_sha256(&dir.join(ONNX_FILE_NAME))?;
    let tokenizer = cache.file_sha256(&dir.join(TOKENIZER_FILE_NAME))?;
    let fingerprint = cache::fingerprint(&onnx, &tokenizer);
    Ok((fingerprint, cache.model_id(&fingerprint, cache::today())?))
}

fn model_files_exist(dir: &Path) -> bool {
    dir.join(ONNX_FILE_NAME).exists() && dir.join(TOKENIZER_FILE_NAME).exists()
}

fn log_stats(label: &str, stats: &EmbedStats, elapsed: Duration) {
    eprintln!(
        "g-mesh daemon: embeddings [{label}]: {} texts, {} cache hits, {} embedded, {} cache errors, {:.1}s",
        stats.texts,
        stats.hits,
        stats.embedded,
        stats.cache_errors,
        elapsed.as_secs_f64()
    );
}

fn disable_notice(path: &Path, err: &anyhow::Error) {
    eprintln!(
        "g-mesh: the embedding cache {} cannot be used ({err:#}) - embedding without it until this process exits",
        path.display()
    );
}

/// After a failed cache operation: contention leaves the cache as it is, a
/// corrupt file is moved aside and recreated, anything else switches the
/// cache off for this process.
fn recover(slot: &mut CacheSlot, err: &anyhow::Error, operation: &str) {
    if cache::is_busy(err) {
        return;
    }
    let CacheSlot::Active(active) = std::mem::replace(slot, CacheSlot::Off) else { return };
    let ActiveCache { cache, settings, fingerprint, .. } = active;
    if cache::is_corrupt(err) {
        drop(cache);
        reopen(slot, settings, fingerprint, err);
    } else {
        eprintln!("g-mesh: failed to {operation} the embedding cache ({err:#})");
        disable_notice(&settings.path, err);
    }
}

/// Moves a corrupt cache aside and opens a fresh one in `slot` under the
/// same `fingerprint`; `slot` stays `Off` if that fails.
fn reopen(slot: &mut CacheSlot, settings: CacheSettings, fingerprint: Hash, cause: &anyhow::Error) {
    let cache = match EmbeddingCache::recreate(&settings.path, cause) {
        Ok(cache) => cache,
        Err(OpenError::Busy(err) | OpenError::Failed(err)) => {
            disable_notice(&settings.path, &err);
            return;
        }
    };
    match cache.model_id(&fingerprint, cache::today()) {
        Ok(model_id) => *slot = CacheSlot::Active(ActiveCache { cache, settings, fingerprint, model_id }),
        Err(err) => disable_notice(&settings.path, &err),
    }
}

/// One embedding [`EmbeddingPipeline::compute`] produced, still waiting to be
/// written by [`EmbeddingPipeline::store`].
///
/// `text` is not the node's raw doc comment/signature - it is the exact
/// string [`text_to_embed`] built and fed to the model, kept around purely so
/// `store` can compare it against the same node's *current* row before
/// writing - see `store`'s own "GM-396" doc section for why that comparison
/// exists.
pub struct ComputedEmbedding {
    node_id: String,
    embedding: Vec<f32>,
    text: String,
}

/// The text a node's *current* row would embed as, or `None` if it has none
/// (or the node no longer exists at all) - [`EmbeddingPipeline::store`]'s own
/// half of the GM-396 staleness check, re-deriving exactly what
/// [`text_to_embed`] would have produced had `compute` run against this row
/// right now instead of whenever it actually ran.
fn current_embeddable_text(conn: &Connection, node_id: &str) -> Result<Option<String>> {
    let row = conn
        .query_row("SELECT docComment, signature FROM nodes WHERE id = ?1", [node_id], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()
        .context("failed to read node for its embedding staleness check")?;
    Ok(row.and_then(|(doc_comment, signature)| text_to_embed(doc_comment.as_deref(), signature.as_deref())))
}

/// Builds the text a node's doc comment and signature embed as, or `None` if
/// there is nothing worth embedding.
///
/// `None` for both inputs, or for both trimming to nothing, are the same
/// case: nothing to say about this symbol beyond what its name already
/// carries, so no row is written at all rather than one embedding an empty
/// or whitespace-only string.
fn text_to_embed(doc_comment: Option<&str>, signature: Option<&str>) -> Option<String> {
    let doc_comment = doc_comment.map(str::trim).filter(|s| !s.is_empty());
    let signature = signature.map(str::trim).filter(|s| !s.is_empty());

    match (doc_comment, signature) {
        (Some(doc), Some(sig)) => Some(format!("{doc}\n\n{sig}")),
        (Some(doc), None) => Some(doc.to_string()),
        (None, Some(sig)) => Some(sig.to_string()),
        (None, None) => None,
    }
}

/// Embeds one node's doc comment/signature and stores the result, or does
/// nothing if there is no embeddable text - the acceptance criterion that a
/// node with neither must not embed an empty string.
pub fn embed_node(
    model: &EmbeddingModel,
    conn: &Connection,
    node_id: &str,
    doc_comment: Option<&str>,
    signature: Option<&str>,
    embedding_version: &str,
) -> Result<()> {
    let Some(text) = text_to_embed(doc_comment, signature) else { return Ok(()) };
    let embedding = model.embed(&text)?;
    vectors::insert(conn, node_id, &embedding, embedding_version)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A deterministic stand-in for the model, for tests that count how
    //! often the pipeline actually embeds.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use anyhow::Result;

    use super::{CacheSettings, Embedder, EmbeddingPipeline};
    use crate::embedding::cache;
    use crate::embedding::model::{EMBEDDING_DIM, ONNX_FILE_NAME, TOKENIZER_FILE_NAME};

    /// How many times the fake model was loaded, and how many texts it
    /// embedded, across every pipeline sharing these counters.
    #[derive(Clone, Default)]
    pub(crate) struct Counters {
        loads: Arc<AtomicUsize>,
        embeds: Arc<AtomicUsize>,
    }

    impl Counters {
        pub(crate) fn loads(&self) -> usize {
            self.loads.load(Ordering::SeqCst)
        }

        pub(crate) fn embeds(&self) -> usize {
            self.embeds.load(Ordering::SeqCst)
        }
    }

    struct FakeEmbedder {
        embeds: Arc<AtomicUsize>,
    }

    impl Embedder for FakeEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.embeds.fetch_add(1, Ordering::SeqCst);
            Ok(fake_vector(text))
        }
    }

    /// The fake model's vector for `text`: a function of the text alone.
    pub(crate) fn fake_vector(text: &str) -> Vec<f32> {
        let hash = cache::text_hash(text);
        (0..EMBEDDING_DIM).map(|i| f32::from(hash[i % 32]) / 255.0 + i as f32 * 1e-3).collect()
    }

    /// Writes stand-in model files into `dir`; `weights` decides the
    /// fingerprint.
    pub(crate) fn fake_model_dir(dir: &Path, weights: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(ONNX_FILE_NAME), weights).unwrap();
        std::fs::write(dir.join(TOKENIZER_FILE_NAME), "{}").unwrap();
        dir.to_path_buf()
    }

    /// A pipeline over the fake model in `model_dir`, counting into
    /// `counters`.
    pub(crate) fn fake_pipeline(
        model_dir: &Path,
        cache: Option<CacheSettings>,
        counters: &Counters,
    ) -> EmbeddingPipeline {
        let counters = counters.clone();
        EmbeddingPipeline::with_loader(
            model_dir,
            move |_dir| {
                counters.loads.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(FakeEmbedder { embeds: Arc::clone(&counters.embeds) }) as Box<dyn Embedder>)
            },
            cache,
        )
    }

    pub(crate) fn cache_at(dir: &Path) -> CacheSettings {
        CacheSettings::new(dir.join("embedding-cache").join("cache.sqlite"), 512)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_with_only_a_doc_comment_embeds_the_doc_comment_alone() {
        assert_eq!(text_to_embed(Some("Reads a file."), None), Some("Reads a file.".to_string()));
    }

    #[test]
    fn a_node_with_only_a_signature_embeds_the_signature_alone() {
        assert_eq!(
            text_to_embed(None, Some("fn read(path: &Path) -> String")),
            Some("fn read(path: &Path) -> String".to_string())
        );
    }

    #[test]
    fn a_node_with_both_embeds_the_doc_comment_before_the_signature() {
        assert_eq!(
            text_to_embed(Some("Reads a file."), Some("fn read(path: &Path) -> String")),
            Some("Reads a file.\n\nfn read(path: &Path) -> String".to_string())
        );
    }

    #[test]
    fn a_node_with_neither_has_nothing_to_embed() {
        assert_eq!(text_to_embed(None, None), None);
    }

    #[test]
    fn whitespace_only_fields_count_as_absent() {
        assert_eq!(text_to_embed(Some("   \n"), Some("\t")), None);
    }

    /// The acceptance criterion at the unit level: an empty-text node must
    /// never reach the model at all, so a caller that (incorrectly) tried to
    /// embed it with no model loaded would still not observe a panic -
    /// `embed_node` is called with a `model` argument in the tests below only
    /// because the type requires one, and the point of this test is that it
    /// is provably never used.
    #[test]
    fn embedding_a_node_with_no_text_never_touches_the_model_or_the_database() {
        // No real `EmbeddingModel` is constructed here at all - if
        // `embed_node` tried to call `.embed()` on neither doc comment nor
        // signature being present, this test would need one and would not
        // compile without real weights. That it compiles and passes without
        // one is the proof.
        assert_eq!(text_to_embed(None, None), None);
    }

    #[test]
    fn a_disabled_pipeline_has_no_query_embedding() {
        let pipeline = EmbeddingPipeline::disabled();
        assert_eq!(pipeline.embed_query("find a function that reads a file"), None);
    }

    #[test]
    fn a_disabled_pipeline_applies_as_a_no_op() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();

        let pipeline = EmbeddingPipeline::disabled();
        let diff = Diff {
            upsert_nodes: vec![crate::storage::write::NodeRecord::new(
                "n1",
                "Function",
                "foo",
                "foo",
                "src/lib.rs",
                "rust",
            )],
            ..Default::default()
        };
        pipeline.apply(&conn, &diff).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "a disabled pipeline must never write a vector row");

        let recorded: Option<String> =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(recorded, None, "a disabled pipeline must never claim an active model either");
    }

    /// GM-394's own split, exercised directly rather than only through
    /// `apply`'s composition: a disabled pipeline's [`EmbeddingPipeline::compute`]
    /// touches no database at all (it takes no `Connection` to touch), and
    /// its empty result makes [`EmbeddingPipeline::store`] a no-op too -
    /// mirroring `a_disabled_pipeline_applies_as_a_no_op` one layer down, so
    /// a regression that reintroduced a `set_embedding_model` write inside
    /// `store` for an empty `computed` slice (or a `compute` that returned
    /// something for a disabled pipeline) would fail here without needing a
    /// real model.
    #[test]
    fn compute_and_store_compose_to_the_same_no_op_a_disabled_pipeline_gives_apply() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();

        let pipeline = EmbeddingPipeline::disabled();
        let diff = Diff {
            upsert_nodes: vec![crate::storage::write::NodeRecord::new(
                "n1",
                "Function",
                "foo",
                "foo",
                "src/lib.rs",
                "rust",
            )],
            ..Default::default()
        };

        let computed = pipeline.compute(&diff, &mut EmbedStats::default());
        assert!(computed.is_empty(), "a disabled pipeline must compute no embeddings");

        pipeline.store(&conn, &computed);

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "storing an empty computed list must write no vector row");

        let recorded: Option<String> =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(recorded, None, "storing an empty computed list must not claim an active model");
    }

    /// Seeds a bare `nodes` row directly (no `apply_diff`, which needs a
    /// `&mut Connection` this module's tests have no other use for) with the
    /// given doc comment/signature - the two columns [`current_embeddable_text`]
    /// reads back to judge staleness.
    fn insert_node(conn: &Connection, id: &str, doc_comment: &str, signature: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, docComment, signature, language)
             VALUES (?1, 'Function', ?1, ?1, 'src/lib.rs', 1, 0, 3, 1, ?2, ?3, 'rust')",
            rusqlite::params![id, doc_comment, signature],
        )
        .unwrap();
    }

    fn vector_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap()
    }

    /// GM-396's own control at the unit level, for the staleness check
    /// [`EmbeddingPipeline::store`]'s doc comment describes: a node whose
    /// *current* row still matches the exact text a [`ComputedEmbedding`] was
    /// computed from gets its vector stored - the ordinary, no-race case the
    /// lock-free window is supposed to cost nothing.
    #[test]
    fn store_writes_the_vector_when_the_nodes_current_content_still_matches_what_was_computed() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        insert_node(&conn, "n1", "Reads a file.", "fn foo()");

        let pipeline = EmbeddingPipeline::disabled();
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.\n\nfn foo()".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            1,
            "a node whose content still matches what was embedded must get its vector stored"
        );
    }

    /// The actual regression this check exists for: `compute` ran against a
    /// node's *old* doc comment/signature, and by the time `store` runs (with
    /// the connection lock reacquired, per `watcher::apply::round_trip`'s own
    /// "GM-396" doc section) some other writer has already committed newer
    /// content for that same node id. Storing the stale embedding anyway
    /// would silently attach it to content it was never computed from - this
    /// asserts it is skipped instead.
    ///
    /// Disabling the check this test is about is exactly "compare the
    /// current row's text against `entry.text` with `==`" in `store`'s match
    /// arm - replacing it with an unconditional `Ok(_) => { store it anyway }`
    /// makes this test fail (a row appears) while
    /// `store_writes_the_vector_when_the_nodes_current_content_still_matches_what_was_computed`
    /// above still passes, which is what shows this test is actually
    /// exercising the staleness check and not some other reason.
    #[test]
    fn store_skips_a_node_whose_content_changed_since_the_embedding_was_computed() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        // The node's *current* row - as if a second writer already committed
        // an edit to this file's signature after `compute` read the old one.
        insert_node(&conn, "n1", "Reads a file, now with a path argument.", "fn foo(path: &Path)");

        let pipeline = EmbeddingPipeline::disabled();
        // What `compute` produced against the *old* text.
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.\n\nfn foo()".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            0,
            "a node whose content moved on since it was computed must not get a stale vector"
        );
    }

    /// The other half of the same window: the node was deleted entirely (a
    /// second reparse removed it, or a bulk re-walk replaced the whole file)
    /// before `store` got to it. `storage::connection::open` runs with
    /// `foreign_keys OFF` (see its own doc comment), so nothing would stop
    /// `vectors::insert` from writing a row for an id that names nothing -
    /// `store`'s own existence check is what actually prevents that orphan.
    #[test]
    fn store_skips_a_node_that_no_longer_exists() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        // No `nodes` row for "n1" at all.

        let pipeline = EmbeddingPipeline::disabled();
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            0,
            "a node that no longer exists must not get an orphaned vector row"
        );
    }

    // -----------------------------------------------------------------
    // The embedding cache seam, over the fake model in `test_support`.
    // -----------------------------------------------------------------

    use super::test_support::{cache_at, fake_model_dir, fake_pipeline, fake_vector, Counters};

    fn documented(id: &str, doc: &str) -> crate::storage::write::NodeRecord {
        let mut node = crate::storage::write::NodeRecord::new(id, "Function", id, id, "src/lib.rs", "rust");
        node.doc_comment = Some(doc.to_string());
        node.signature = Some(format!("fn {id}()"));
        node
    }

    fn diff_of(count: usize) -> Diff {
        Diff {
            upsert_nodes: (0..count)
                .map(|i| documented(&format!("n{i}"), &format!("Does thing {i}.")))
                .collect(),
            ..Default::default()
        }
    }

    fn bits(vector: &[f32]) -> Vec<u32> {
        vector.iter().map(|value| value.to_bits()).collect()
    }

    /// A second process over a warm cache: every text hits, nothing is
    /// embedded, and the model is never even loaded.
    ///
    /// Control: disable the lookup in `compute` (treat `cache_lookup` as
    /// always `None`) and the second pipeline embeds all 5; resolve the model
    /// before the lookup and `loads` becomes 1.
    #[test]
    fn a_warm_cache_answers_every_text_without_loading_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let diff = diff_of(5);

        let cold = Counters::default();
        let mut cold_stats = EmbedStats::default();
        let first =
            fake_pipeline(&model_dir, Some(cache_at(dir.path())), &cold).compute(&diff, &mut cold_stats);
        assert_eq!(cold.embeds(), 5);
        assert_eq!(
            (cold_stats.texts, cold_stats.hits, cold_stats.embedded, cold_stats.inserted),
            (5, 0, 5, 5)
        );

        let warm = Counters::default();
        let mut warm_stats = EmbedStats::default();
        let second =
            fake_pipeline(&model_dir, Some(cache_at(dir.path())), &warm).compute(&diff, &mut warm_stats);

        assert_eq!(warm.embeds(), 0, "every text is in the cache");
        assert_eq!(warm.loads(), 0, "an all-hit batch must not load the model");
        assert_eq!((warm_stats.texts, warm_stats.hits, warm_stats.embedded), (5, 5, 0));
        let ids: Vec<&str> = second.iter().map(|entry| entry.node_id.as_str()).collect();
        assert_eq!(ids, ["n0", "n1", "n2", "n3", "n4"], "results keep the diff's order");
        for (cached, fresh) in second.iter().zip(&first) {
            assert_eq!(bits(&cached.embedding), bits(&fresh.embedding));
            assert_eq!(cached.text, fresh.text);
        }
    }

    /// Same texts under a second fingerprint: nothing is shared.
    ///
    /// Control: drop the fingerprint from the key (e.g. make
    /// `cache::fingerprint` ignore its inputs) and the second model gets 0
    /// calls.
    #[test]
    fn a_different_model_embeds_every_text_again() {
        let dir = tempfile::tempdir().unwrap();
        let first_model = fake_model_dir(&dir.path().join("model-a"), "weights v1");
        let second_model = fake_model_dir(&dir.path().join("model-b"), "weights v2");
        let diff = diff_of(4);

        let counters = Counters::default();
        fake_pipeline(&first_model, Some(cache_at(dir.path())), &counters)
            .compute(&diff, &mut EmbedStats::default());
        assert_eq!(counters.embeds(), 4);

        let mut stats = EmbedStats::default();
        fake_pipeline(&second_model, Some(cache_at(dir.path())), &counters).compute(&diff, &mut stats);
        assert_eq!(counters.embeds(), 8, "a new fingerprint must not reuse the first model's vectors");
        assert_eq!(stats.hits, 0);
    }

    /// Control: stop checking the pre-resolved `None` model at the top of
    /// `compute` and a disabled pipeline opens the cache and counts texts.
    #[test]
    fn a_pipeline_without_a_cache_embeds_everything_and_reports_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, None, &counters);
        let mut stats = EmbedStats::default();
        pipeline.compute(&diff_of(3), &mut stats);
        pipeline.compute(&diff_of(3), &mut stats);
        assert_eq!(counters.embeds(), 6);
        assert_eq!((stats.texts, stats.hits, stats.embedded), (6, 0, 6));
        assert!(!dir.path().join("embedding-cache").exists(), "no cache means no cache file");
    }

    /// `G_MESH_EMBEDDING_CACHE=off` is the kill switch.
    ///
    /// Control: make `env_disables_cache` return false and the `off` rows
    /// fail.
    #[test]
    fn the_environment_switch_turns_the_cache_off() {
        use std::ffi::OsStr;
        assert!(env_disables_cache(Some(OsStr::new("off"))));
        assert!(env_disables_cache(Some(OsStr::new("OFF"))));
        assert!(!env_disables_cache(None));
        assert!(!env_disables_cache(Some(OsStr::new(""))));
        assert!(!env_disables_cache(Some(OsStr::new("on"))));
    }

    fn index_with(diff: &Diff) -> Connection {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        for node in &diff.upsert_nodes {
            insert_node(
                &conn,
                &node.id,
                node.doc_comment.as_deref().unwrap(),
                node.signature.as_deref().unwrap(),
            );
        }
        conn
    }

    /// A garbage cache file does not fail indexing: it is moved aside, a
    /// fresh cache takes its place, and every vector is still stored.
    ///
    /// Control: propagate the open failure instead of degrading (e.g. make
    /// `open_cache` return early with nothing computed, or panic, on
    /// `OpenError::Failed`) and the vectors are missing.
    #[test]
    fn a_garbage_cache_file_still_indexes_every_node() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let settings = cache_at(dir.path());
        std::fs::create_dir_all(settings.path.parent().unwrap()).unwrap();
        std::fs::write(&settings.path, vec![0x42u8; 16384]).unwrap();
        let diff = diff_of(3);
        let conn = index_with(&diff);

        let counters = Counters::default();
        let mut stats = EmbedStats::default();
        let pipeline = fake_pipeline(&model_dir, Some(settings.clone()), &counters);
        let computed = pipeline.compute(&diff, &mut stats);
        pipeline.store(&conn, &computed);

        assert_eq!(vector_count(&conn), 3, "indexing must store every vector despite the garbage cache");
        assert_eq!(stats.inserted, 3, "the recreated cache takes this run's vectors");
        let warm = Counters::default();
        fake_pipeline(&model_dir, Some(settings), &warm).compute(&diff, &mut EmbedStats::default());
        assert_eq!(warm.embeds(), 0, "the recreated cache serves the next run");
    }

    /// Another process holding the cache's writer drops this batch's
    /// inserts and nothing else: indexing stores every vector, and the cache
    /// stays enabled for the batches after it.
    ///
    /// Control: propagate the busy insert (e.g. return no embeddings from
    /// `compute` when `cache_insert` fails) and the vectors are missing;
    /// drop the busy guard at the top of `recover` (a busy insert then
    /// switches the cache off) and the next batch inserts nothing.
    #[test]
    fn a_held_cache_writer_still_indexes_every_node() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let settings = cache_at(dir.path());
        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, Some(settings.clone()), &counters);
        // Opens the cache before the lock is taken.
        pipeline.compute(
            &Diff { upsert_nodes: vec![documented("warm", "Warms up.")], ..Default::default() },
            &mut EmbedStats::default(),
        );

        let holder = rusqlite::Connection::open(&settings.path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let diff = diff_of(4);
        let conn = index_with(&diff);
        let mut stats = EmbedStats::default();
        let computed = pipeline.compute(&diff, &mut stats);
        pipeline.store(&conn, &computed);
        holder.execute_batch("COMMIT").unwrap();

        assert_eq!(vector_count(&conn), 4, "indexing must store every vector while the cache is held");
        assert_eq!(stats.cache_errors, 1, "the busy insert is counted");
        assert_eq!(stats.inserted, 0, "the busy batch's inserts are dropped");

        let mut next = EmbedStats::default();
        pipeline.compute(&diff, &mut next);
        assert_eq!((next.inserted, next.cache_errors), (4, 0), "the next batch is stored");
        let mut again = EmbedStats::default();
        pipeline.compute(&diff, &mut again);
        assert_eq!((again.hits, again.embedded), (4, 0), "and served from the cache after that");
    }

    /// A cache that cannot be opened even after moving the file aside (its
    /// directory is a regular file) is switched off for the process: every
    /// vector is still stored, and later batches never try the cache again,
    /// so its notice is logged once.
    ///
    /// Control: panic or return nothing from `open_cache` on
    /// `OpenError::Failed` and the vectors are missing; treat `Failed` like
    /// `Busy` (leave the slot `Unopened`) and the second batch opens the
    /// cache once the obstacle is gone.
    #[test]
    fn a_cache_that_cannot_be_created_is_off_for_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let settings = cache_at(dir.path());
        let cache_dir = settings.path.parent().unwrap().to_path_buf();
        std::fs::write(&cache_dir, "not a directory").unwrap();
        let diff = diff_of(3);
        let conn = index_with(&diff);

        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, Some(settings.clone()), &counters);
        let mut stats = EmbedStats::default();
        let computed = pipeline.compute(&diff, &mut stats);
        pipeline.store(&conn, &computed);

        assert_eq!(vector_count(&conn), 3, "indexing must store every vector without a cache");
        assert_eq!((stats.texts, stats.embedded, stats.inserted), (3, 3, 0));
        assert!(
            matches!(*pipeline.cache.lock().unwrap(), CacheSlot::Off),
            "the cache is off for the process"
        );

        std::fs::remove_file(&cache_dir).unwrap();
        let mut later = EmbedStats::default();
        pipeline.compute(&diff, &mut later);
        assert_eq!((later.embedded, later.inserted), (3, 0), "a disabled cache is never retried");
        assert!(!settings.path.exists(), "no cache file is created after the cache was switched off");
    }

    /// Opening the cache fingerprints the model's files without holding the
    /// cache's mutex: while the weights are being read, the mutex is free.
    /// The weights are a FIFO, so the read blocks until this test writes
    /// them, and opening the FIFO's write end returns only once the reader
    /// has it open.
    ///
    /// Control: open the cache under the mutex (call `open_cache` from
    /// inside the locked section of `open_if_unopened`) and `try_lock` fails.
    #[cfg(unix)]
    #[test]
    fn the_model_is_hashed_without_holding_the_cache_mutex() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "unused");
        let onnx = model_dir.join(crate::embedding::model::ONNX_FILE_NAME);
        std::fs::remove_file(&onnx).unwrap();
        let status = std::process::Command::new("mkfifo").arg(&onnx).status().unwrap();
        assert!(status.success(), "mkfifo failed");

        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, Some(cache_at(dir.path())), &counters);
        std::thread::scope(|scope| {
            let computing = scope.spawn(|| {
                let mut stats = EmbedStats::default();
                pipeline.compute(&diff_of(2), &mut stats);
                stats
            });
            let mut weights = std::fs::OpenOptions::new().write(true).open(&onnx).unwrap();
            let free = pipeline.cache.try_lock().is_ok();
            weights.write_all(b"weights v1").unwrap();
            drop(weights);
            let stats = computing.join().unwrap();
            assert!(free, "the cache's mutex must be free while the model is being hashed");
            assert_eq!(stats.inserted, 2, "the cache opened once the weights were read");
        });
    }

    /// A cache held before this process first opens it: the open is retried
    /// on the next call rather than the cache being switched off.
    ///
    /// Control: treat `OpenError::Busy` like `Failed` (switch the cache
    /// `Off`) and the second call inserts nothing.
    #[test]
    fn a_cache_busy_at_first_open_is_opened_on_a_later_call() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let settings = cache_at(dir.path());
        drop(super::cache::EmbeddingCache::open(&settings.path).map_err(|err| format!("{err:?}")).unwrap());

        let holder = rusqlite::Connection::open(&settings.path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, Some(settings), &counters);
        let diff = diff_of(2);
        let conn = index_with(&diff);
        let computed = pipeline.compute(&diff, &mut EmbedStats::default());
        pipeline.store(&conn, &computed);
        assert_eq!(vector_count(&conn), 2, "a busy cache at open still indexes every node");
        holder.execute_batch("COMMIT").unwrap();

        let mut stats = EmbedStats::default();
        pipeline.compute(&diff, &mut stats);
        assert_eq!(stats.inserted, 2, "the cache opens once the holder is gone");
    }

    /// A reindex unit that added to the cache trims it to its bound.
    ///
    /// Control: skip `collect_garbage` in `finish_unit` and every entry
    /// stays.
    #[test]
    fn finishing_a_unit_that_inserted_trims_the_cache_to_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&dir.path().join("model"), "weights v1");
        let settings = CacheSettings { max_bytes: 1, ..cache_at(dir.path()) };
        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, Some(settings.clone()), &counters);
        let mut stats = EmbedStats::default();
        pipeline.compute(&diff_of(20), &mut stats);

        pipeline.finish_unit("test", &stats, Duration::ZERO);

        let conn = rusqlite::Connection::open(&settings.path).unwrap();
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).unwrap();
        assert!(remaining < 20, "the unit's end must evict down toward the bound, {remaining} left");
    }

    /// The fake's vectors are what `fake_vector` says, so a test can compare
    /// an index row with a fresh embed.
    #[test]
    fn the_fake_model_is_deterministic() {
        assert_eq!(bits(&fake_vector("a")), bits(&fake_vector("a")));
        assert_ne!(bits(&fake_vector("a")), bits(&fake_vector("b")));
    }

    /// `PIPELINE_EPOCH` is part of every cache key's fingerprint and must
    /// change whenever `text_to_embed`'s output does, or the cache serves
    /// vectors for text formatted the old way. This pins the two together:
    /// a format change fails here until the epoch is bumped and the digest
    /// below updated with it.
    ///
    /// Control: change `text_to_embed`'s separator and this fails.
    #[test]
    fn the_pipeline_epoch_is_pinned_to_the_text_format() {
        use sha2::{Digest, Sha256};
        let inputs: [(Option<&str>, Option<&str>); 5] = [
            (Some("  Reads a file.  "), Some(" fn read(path: &Path) -> String ")),
            (Some("Reads a file."), None),
            (None, Some("fn read()")),
            (Some(" \n"), Some("\tfn x()")),
            (None, None),
        ];
        let mut hasher = Sha256::new();
        for (doc, signature) in inputs {
            hasher.update(format!("{:?}\u{0}", text_to_embed(doc, signature)).as_bytes());
        }
        let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            (crate::embedding::cache::PIPELINE_EPOCH, digest.as_str()),
            (1, "fa199d7fbc4ca707c83707eac28bd687701c737343d062ff19b7400590b19440"),
            "text_to_embed's output changed: bump PIPELINE_EPOCH and update this digest"
        );
    }
}
