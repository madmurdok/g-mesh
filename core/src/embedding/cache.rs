//! The machine-wide embedding cache: one SQLite file under
//! `$G_MESH_HOME/embedding-cache/`, shared by every daemon and CLI process on
//! the machine, mapping `sha256(text)` under a model fingerprint to the
//! vector that model produced for that text. The design, and what was
//! rejected, is ADR 0007 (`docs/adr/0007-embedding-cache.md`).
//!
//! Invariants this module relies on:
//!
//! - A vector is a pure function of the fingerprint and the text, so the
//!   same key always carries the same bytes. That is what makes
//!   `INSERT OR IGNORE` between racing writers harmless, and what makes a
//!   cached vector interchangeable with a fresh one. [`PIPELINE_EPOCH`] and
//!   [`ORT_VERSION`] are part of the fingerprint for exactly that reason.
//! - Every error is returned to the caller, never panicked on: the pipeline
//!   treats a failed lookup as an all-miss batch and a failed insert as a
//!   dropped batch, so the cache can never fail indexing.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, params_from_iter, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::embedding::model::{DEFAULT_MAX_SEQUENCE_LENGTH, EMBEDDING_DIM};
use crate::storage::vectors;

/// Set to `off` to bypass the cache entirely, whatever the global config
/// says: every text is embedded, nothing is read or written.
pub const CACHE_ENV: &str = "G_MESH_EMBEDDING_CACHE";

/// Version of everything between the model's raw output and the stored
/// vector that this crate owns: `text_to_embed`'s format, mean pooling and
/// L2 normalization. Part of the fingerprint, so bumping it makes every
/// cached vector unreachable. It must be bumped whenever that code changes;
/// `pipeline`'s tests pin it to `text_to_embed`'s output so a format change
/// cannot land without touching it.
pub const PIPELINE_EPOCH: u32 = 1;

/// The `ort` crate version the vectors were computed with - an ONNX Runtime
/// upgrade may change floating-point results. Must equal the exact version
/// `core/Cargo.toml` pins; a test reads the manifest to keep them in step.
pub const ORT_VERSION: &str = "2.0.0-rc.9";

const DIR_NAME: &str = "embedding-cache";
const FILE_NAME: &str = "cache.sqlite";

/// Bumped with any change to the tables below. A file carrying another
/// version is treated as corrupt: moved aside and recreated.
const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for another process's write transaction.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// A model whose `last_used` is older than this is dropped with its entries.
const MODEL_RETENTION_DAYS: i64 = 30;

/// Eviction trims the file down to this fraction of the bound, so the next
/// few reindexes do not each have to evict again.
const EVICT_TO_FRACTION: f64 = 0.8;

/// Keys per `SELECT ... IN (...)`; well under SQLite's bound-parameter limit.
const LOOKUP_CHUNK: usize = 500;

const SCHEMA: &str = "
    CREATE TABLE models (
        id INTEGER PRIMARY KEY,
        fingerprint BLOB UNIQUE,
        last_used INTEGER
    );
    CREATE TABLE model_files (
        path TEXT,
        size INTEGER,
        mtime_ns INTEGER,
        sha256 BLOB,
        PRIMARY KEY (path, size, mtime_ns)
    );
    CREATE TABLE entries (
        model_id INTEGER,
        text_hash BLOB,
        vector BLOB,
        last_used INTEGER,
        PRIMARY KEY (model_id, text_hash)
    ) WITHOUT ROWID;
";

/// A SHA-256 digest: a text key, a file hash or a model fingerprint.
pub(crate) type Hash = [u8; 32];

/// `$G_MESH_HOME/embedding-cache/cache.sqlite`.
pub fn default_path() -> Result<PathBuf> {
    Ok(crate::paths::g_mesh_home()?.join(DIR_NAME).join(FILE_NAME))
}

/// The key of one text: SHA-256 of its UTF-8 bytes, exactly as passed to
/// the model (before tokenizer truncation).
pub(crate) fn text_hash(text: &str) -> Hash {
    Sha256::digest(text.as_bytes()).into()
}

/// The model fingerprint: SHA-256 over a canonical record of every input a
/// vector depends on besides the text itself.
pub(crate) fn fingerprint(onnx_sha256: &Hash, tokenizer_sha256: &Hash) -> Hash {
    let record = format!(
        "g-mesh embedding fingerprint\nmodel.onnx sha256={}\ntokenizer.json sha256={}\n\
         max_sequence_length={DEFAULT_MAX_SEQUENCE_LENGTH}\nembedding_dim={EMBEDDING_DIM}\n\
         pipeline_epoch={PIPELINE_EPOCH}\nort={ORT_VERSION}\n",
        hex(onnx_sha256),
        hex(tokenizer_sha256),
    );
    Sha256::digest(record.as_bytes()).into()
}

fn hex(hash: &Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Today as a UTC day number, the unit of every `last_used` column.
pub(crate) fn today() -> i64 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    (secs / 86_400) as i64
}

/// Whether `err` is another process holding the database: transient, so the
/// file is left alone and the operation is simply skipped.
pub(crate) fn is_busy(err: &anyhow::Error) -> bool {
    matches!(sqlite_code(err), Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

/// Whether `err` says the file is not a usable database.
pub(crate) fn is_corrupt(err: &anyhow::Error) -> bool {
    matches!(sqlite_code(err), Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
}

fn sqlite_code(err: &anyhow::Error) -> Option<ErrorCode> {
    err.chain().find_map(|cause| match cause.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(failure, _)) => Some(failure.code),
        _ => None,
    })
}

/// Why [`EmbeddingCache::open`] failed.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// Another process holds the file; try again later, leave it alone.
    Busy(anyhow::Error),
    /// The file could not be opened even after moving it aside.
    Failed(anyhow::Error),
}

impl OpenError {
    fn classify(err: anyhow::Error) -> Self {
        if is_busy(&err) {
            Self::Busy(err)
        } else {
            Self::Failed(err)
        }
    }
}

/// What one [`EmbeddingCache::gc`] round did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GcOutcome {
    /// Another process held the writer; nothing was done this round.
    pub(crate) skipped: bool,
    pub(crate) models_dropped: usize,
    pub(crate) entries_evicted: usize,
}

/// One open connection to the cache file.
pub(crate) struct EmbeddingCache {
    conn: Connection,
}

impl EmbeddingCache {
    /// Opens the cache at `path`, creating it if missing. A file that is not
    /// a cache of this schema (garbage, another schema version, a failed
    /// open for any reason but contention) is moved aside to
    /// `cache.sqlite.corrupt-<unix>` and a fresh one is created in its place.
    pub(crate) fn open(path: &Path) -> Result<Self, OpenError> {
        match Self::open_in_place(path) {
            Ok(cache) => Ok(cache),
            Err(err) if is_busy(&err) => Err(OpenError::Busy(err)),
            Err(err) => Self::recreate_after(path, &err),
        }
    }

    /// Moves the file at `path` aside and creates a fresh cache there - for
    /// a connection that reported corruption after it was opened.
    pub(crate) fn recreate(path: &Path, cause: &anyhow::Error) -> Result<Self, OpenError> {
        Self::recreate_after(path, cause)
    }

    fn recreate_after(path: &Path, cause: &anyhow::Error) -> Result<Self, OpenError> {
        let moved_to = move_aside(path).map_err(OpenError::Failed)?;
        eprintln!(
            "g-mesh: the embedding cache {} is unusable ({cause:#}) - moved it to {} and started an empty one",
            path.display(),
            moved_to.display()
        );
        Self::open_in_place(path).map_err(OpenError::classify)
    }

    fn open_in_place(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| {
                format!("failed to create the embedding cache directory {}", dir.display())
            })?;
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("failed to open the embedding cache {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Only takes effect on a file with no tables yet, which is the only
        // time it is needed: before the schema below is created.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            other => bail!("unknown embedding cache schema version {other}"),
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// SHA-256 of the file at `path`, memoized by its canonical path, size
    /// and modification time: hashing the model's weights takes seconds, a
    /// memoized read one row.
    pub(crate) fn file_sha256(&self, path: &Path) -> Result<Hash> {
        let canonical =
            fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))?;
        let key = canonical.to_string_lossy().into_owned();
        let (size, mtime_ns) = size_and_mtime(&canonical)?;

        let memoized: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT sha256 FROM model_files WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3",
                params![key, size, mtime_ns],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(hash) = memoized.and_then(|blob| Hash::try_from(blob.as_slice()).ok()) {
            return Ok(hash);
        }

        let hash = hash_file(&canonical)?;
        // A file rewritten while it was being hashed is not memoized under
        // either version's metadata.
        if size_and_mtime(&canonical)? == (size, mtime_ns) {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute("DELETE FROM model_files WHERE path = ?1", [&key])?;
            tx.execute(
                "INSERT INTO model_files (path, size, mtime_ns, sha256) VALUES (?1, ?2, ?3, ?4)",
                params![key, size, mtime_ns, hash.as_slice()],
            )?;
            tx.commit()?;
        }
        Ok(hash)
    }

    /// The `models` row id for `fingerprint`, created on first use, with its
    /// `last_used` set to `today`.
    pub(crate) fn model_id(&self, fingerprint: &Hash, today: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO models (fingerprint, last_used) VALUES (?1, ?2)",
            params![fingerprint.as_slice(), today],
        )?;
        self.conn.execute(
            "UPDATE models SET last_used = ?2 WHERE fingerprint = ?1 AND last_used <> ?2",
            params![fingerprint.as_slice(), today],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM models WHERE fingerprint = ?1",
            [fingerprint.as_slice()],
            |row| row.get(0),
        )?)
    }

    /// The cached vector for each of `keys`, in order; `None` for a miss.
    ///
    /// A hit's `last_used` is moved to `today` only when it differs, so a
    /// warm reindex does not rewrite every row. That touch is best-effort:
    /// failing it (another writer holds the file) only ages the entry.
    pub(crate) fn lookup(&self, model_id: i64, keys: &[Hash], today: i64) -> Result<Vec<Option<Vec<f32>>>> {
        let mut found: std::collections::HashMap<Hash, (Vec<u8>, i64)> = std::collections::HashMap::new();
        for chunk in keys.chunks(LOOKUP_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT text_hash, vector, last_used FROM entries WHERE model_id = ? AND text_hash IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let args = std::iter::once(rusqlite::types::Value::Integer(model_id))
                .chain(chunk.iter().map(|key| rusqlite::types::Value::Blob(key.to_vec())));
            let rows = stmt.query_map(params_from_iter(args), |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, i64>(2)?))
            })?;
            for row in rows {
                let (text_hash, vector, last_used) = row?;
                if let Ok(key) = Hash::try_from(text_hash.as_slice()) {
                    found.insert(key, (vector, last_used));
                }
            }
        }

        let stale: Vec<&Hash> = keys
            .iter()
            .filter(|key| found.get(*key).is_some_and(|(_, last_used)| *last_used != today))
            .collect();
        if !stale.is_empty() {
            let _ = self.touch(model_id, &stale, today);
        }

        Ok(keys.iter().map(|key| found.get(key).and_then(|(blob, _)| decode(blob))).collect())
    }

    fn touch(&self, model_id: i64, keys: &[&Hash], today: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE entries SET last_used = ?3 WHERE model_id = ?1 AND text_hash = ?2")?;
            for key in keys {
                stmt.execute(params![model_id, key.as_slice(), today])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores `entries` under `model_id` in one transaction, returning how
    /// many were new. A key already present is left as it is: it carries
    /// the same bytes. A vector of the wrong width is not stored.
    pub(crate) fn insert(&mut self, model_id: i64, entries: &[(Hash, &[f32])], today: i64) -> Result<usize> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO entries (model_id, text_hash, vector, last_used) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (key, vector) in entries.iter().filter(|(_, vector)| vector.len() == EMBEDDING_DIM) {
                inserted += stmt.execute(params![model_id, key.as_slice(), vectors::pack(vector), today])?;
            }
        }
        // Keeps a long-running process's model from ageing out under it.
        tx.execute(
            "UPDATE models SET last_used = ?2 WHERE id = ?1 AND last_used <> ?2",
            params![model_id, today],
        )?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Drops models unused for [`MODEL_RETENTION_DAYS`] (never
    /// `current_model`) with their entries, then, if the live pages exceed
    /// `max_bytes`, evicts the least recently used entries down to
    /// [`EVICT_TO_FRACTION`] of it and returns the freed pages to the file
    /// system.
    ///
    /// Takes the writer without waiting: when another process holds it, this
    /// round is skipped rather than stalling a reindex.
    pub(crate) fn gc(&mut self, current_model: i64, max_bytes: u64, today: i64) -> Result<GcOutcome> {
        self.conn.busy_timeout(Duration::ZERO)?;
        let outcome = self.gc_now(current_model, max_bytes, today);
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        match outcome {
            Err(err) if is_busy(&err) => Ok(GcOutcome { skipped: true, ..GcOutcome::default() }),
            other => other,
        }
    }

    fn gc_now(&mut self, current_model: i64, max_bytes: u64, today: i64) -> Result<GcOutcome> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let models_dropped = tx.execute(
            "DELETE FROM models WHERE last_used < ?1 AND id <> ?2",
            params![today - MODEL_RETENTION_DAYS, current_model],
        )?;
        if models_dropped > 0 {
            tx.execute("DELETE FROM entries WHERE model_id NOT IN (SELECT id FROM models)", [])?;
        }

        let used = live_bytes(&tx)?;
        let mut entries_evicted = 0;
        if used > max_bytes {
            let target = (max_bytes as f64) * EVICT_TO_FRACTION;
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))?;
            let fraction = (used as f64 - target) / used as f64;
            let evict = ((count as f64) * fraction).ceil() as i64;
            entries_evicted = tx.execute(
                "DELETE FROM entries WHERE (model_id, text_hash) IN
                     (SELECT model_id, text_hash FROM entries ORDER BY last_used ASC LIMIT ?1)",
                [evict],
            )?;
        }
        tx.commit()?;

        if models_dropped > 0 || entries_evicted > 0 {
            self.conn.execute_batch("PRAGMA incremental_vacuum")?;
        }
        Ok(GcOutcome { skipped: false, models_dropped, entries_evicted })
    }

    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> Result<u64> {
        live_bytes(&self.conn)
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Bytes of the file's pages in use: `page_count * page_size`, less the
/// free pages a previous delete left that `incremental_vacuum` has not yet
/// returned.
fn live_bytes(conn: &Connection) -> Result<u64> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(((page_count - freelist).max(0) as u64) * page_size as u64)
}

/// A stored vector back to floats, bit for bit; `None` for a blob of any
/// other length than one [`EMBEDDING_DIM`]-wide vector.
fn decode(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() != EMBEDDING_DIM * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect(),
    )
}

fn size_and_mtime(path: &Path) -> Result<(i64, i64)> {
    let meta = fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let mtime = meta.modified().with_context(|| format!("no modification time for {}", path.display()))?;
    let mtime_ns = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    Ok((meta.len() as i64, mtime_ns))
}

fn hash_file(path: &Path) -> Result<Hash> {
    let mut file = fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// Renames `path` (and its `-wal`/`-shm` companions) to
/// `<name>.corrupt-<unix seconds>`, returning the new main path.
fn move_aside(path: &Path) -> Result<PathBuf> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = path.file_name().unwrap_or_else(|| OsStr::new(FILE_NAME)).to_string_lossy().into_owned();
    let target = path.with_file_name(format!("{name}.corrupt-{secs}"));
    if path.exists() {
        fs::rename(path, &target)
            .with_context(|| format!("failed to move {} aside to {}", path.display(), target.display()))?;
    }
    for suffix in ["-wal", "-shm"] {
        let companion = path.with_file_name(format!("{name}{suffix}"));
        if companion.exists() {
            let _ = fs::rename(&companion, path.with_file_name(format!("{name}.corrupt-{secs}{suffix}")));
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(seed: u8) -> Vec<f32> {
        (0..EMBEDDING_DIM).map(|i| f32::from_bits(0x3f00_0000 | (u32::from(seed) << 12) | i as u32)).collect()
    }

    fn open_temp() -> (tempfile::TempDir, EmbeddingCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = match EmbeddingCache::open(&dir.path().join(DIR_NAME).join(FILE_NAME)) {
            Ok(cache) => cache,
            Err(err) => panic!("failed to open a fresh cache: {err:?}"),
        };
        (dir, cache)
    }

    /// Control: decoding with a different byte order, or any change to the
    /// encoding, makes the round trip unequal.
    #[test]
    fn a_vector_round_trips_bit_for_bit() {
        let (_dir, mut cache) = open_temp();
        let model = cache.model_id(&[1; 32], 100).unwrap();
        let original = vector(7);
        cache.insert(model, &[(text_hash("fn a()"), &original)], 100).unwrap();

        let found = cache.lookup(model, &[text_hash("fn a()"), text_hash("fn b()")], 100).unwrap();
        let stored = found[0].as_ref().expect("the inserted text must hit");
        assert!(stored.iter().zip(&original).all(|(a, b)| a.to_bits() == b.to_bits()));
        assert!(found[1].is_none(), "an absent text must miss");
    }

    /// Control: key entries on the text alone (drop `model_id` from the
    /// lookup) and the second model hits the first model's vector.
    #[test]
    fn two_fingerprints_never_share_a_vector() {
        let (_dir, mut cache) = open_temp();
        let first = cache.model_id(&[1; 32], 100).unwrap();
        let second = cache.model_id(&[2; 32], 100).unwrap();
        assert_ne!(first, second);
        cache.insert(first, &[(text_hash("fn a()"), &vector(1))], 100).unwrap();

        assert!(cache.lookup(second, &[text_hash("fn a()")], 100).unwrap()[0].is_none());
        assert_eq!(cache.model_id(&[1; 32], 101).unwrap(), first, "a fingerprint keeps its row");
    }

    /// Control: remove the length check in `decode` and a truncated blob is
    /// returned as a short vector instead of a miss.
    #[test]
    fn a_blob_of_the_wrong_length_is_a_miss() {
        let (_dir, cache) = open_temp();
        let model = cache.model_id(&[1; 32], 100).unwrap();
        cache
            .connection()
            .execute(
                "INSERT INTO entries (model_id, text_hash, vector, last_used) VALUES (?1, ?2, ?3, 100)",
                params![model, text_hash("fn a()").as_slice(), vec![0u8; 12]],
            )
            .unwrap();
        assert!(cache.lookup(model, &[text_hash("fn a()")], 100).unwrap()[0].is_none());
    }

    /// Control: drop the `last_used <> today` touch and the hit keeps its old
    /// day, so the next eviction would take a vector just used.
    #[test]
    fn a_hit_moves_its_entry_to_today() {
        let (_dir, mut cache) = open_temp();
        let model = cache.model_id(&[1; 32], 100).unwrap();
        cache.insert(model, &[(text_hash("fn a()"), &vector(1))], 100).unwrap();
        cache.lookup(model, &[text_hash("fn a()")], 105).unwrap();
        let last_used: i64 =
            cache.connection().query_row("SELECT last_used FROM entries", [], |row| row.get(0)).unwrap();
        assert_eq!(last_used, 105);
    }

    /// Control: memoize by path alone (ignore size/mtime) and the rewritten
    /// file keeps its old hash.
    #[test]
    fn a_file_hash_is_memoized_until_the_file_changes() {
        let (dir, cache) = open_temp();
        let file = dir.path().join("model.onnx");
        fs::write(&file, b"weights v1").unwrap();
        let first = cache.file_sha256(&file).unwrap();
        assert_eq!(first, <Hash>::from(Sha256::digest(b"weights v1")));
        let memo_rows: i64 =
            cache.connection().query_row("SELECT COUNT(*) FROM model_files", [], |row| row.get(0)).unwrap();
        assert_eq!(memo_rows, 1);
        assert_eq!(cache.file_sha256(&file).unwrap(), first);

        fs::write(&file, b"weights, version two").unwrap();
        assert_eq!(cache.file_sha256(&file).unwrap(), <Hash>::from(Sha256::digest(b"weights, version two")));
    }

    #[test]
    fn the_fingerprint_depends_on_both_files() {
        let a = fingerprint(&[1; 32], &[2; 32]);
        assert_ne!(a, fingerprint(&[3; 32], &[2; 32]));
        assert_ne!(a, fingerprint(&[1; 32], &[3; 32]));
        assert_eq!(a, fingerprint(&[1; 32], &[2; 32]));
    }

    /// The fingerprint names the `ort` version the vectors came from; it has
    /// to be the one the manifest actually pins.
    #[test]
    fn the_fingerprints_ort_version_is_the_one_the_manifest_pins() {
        let manifest = include_str!("../../Cargo.toml");
        let pinned = manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("ort = \"="))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("core/Cargo.toml must pin `ort` to an exact version");
        assert_eq!(ORT_VERSION, pinned, "bump ORT_VERSION with the ort dependency");
    }

    #[test]
    fn a_new_cache_uses_wal_and_incremental_auto_vacuum() {
        let (_dir, cache) = open_temp();
        let mode: String = cache.connection().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        let vacuum: i64 = cache.connection().query_row("PRAGMA auto_vacuum", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(vacuum, 2, "auto_vacuum must be INCREMENTAL");
    }

    /// Control: open without moving the garbage aside and the open fails.
    #[test]
    fn a_garbage_file_is_moved_aside_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, vec![0x5a; 8192]).unwrap();

        let cache = EmbeddingCache::open(&path).expect("a garbage file must be replaced, not fail the open");
        assert!(cache.model_id(&[1; 32], 100).is_ok());
        let moved: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("cache.sqlite.corrupt-"))
            .collect();
        assert_eq!(moved.len(), 1, "the garbage must be kept aside for inspection: {moved:?}");
    }

    /// Control: accept any `user_version` and the foreign schema is used as
    /// if it were this one.
    #[test]
    fn an_unknown_schema_version_is_treated_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        let cache = EmbeddingCache::open(&path).expect("an unknown version must be replaced");
        let version: i64 = cache.connection().query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// Control: skip the eviction `DELETE` and the file stays over its bound
    /// with the oldest entry still present.
    #[test]
    fn gc_evicts_the_oldest_entries_down_under_the_bound() {
        let (_dir, mut cache) = open_temp();
        let model = cache.model_id(&[1; 32], 200).unwrap();
        for day in 0..40i64 {
            let keys: Vec<(Hash, Vec<f32>)> =
                (0..10).map(|i| (text_hash(&format!("day {day} text {i}")), vector(i as u8))).collect();
            let entries: Vec<(Hash, &[f32])> = keys.iter().map(|(key, v)| (*key, v.as_slice())).collect();
            cache.insert(model, &entries, 100 + day).unwrap();
        }
        let before = cache.used_bytes().unwrap();
        let bound = before / 2;

        let outcome = cache.gc(model, bound, 200).unwrap();

        assert!(!outcome.skipped);
        assert!(outcome.entries_evicted > 0);
        assert!(cache.used_bytes().unwrap() <= bound, "{} > {bound}", cache.used_bytes().unwrap());
        let oldest = cache.lookup(model, &[text_hash("day 0 text 0")], 200).unwrap();
        assert!(oldest[0].is_none(), "the least recently used entry goes first");
        let newest = cache.lookup(model, &[text_hash("day 39 text 0")], 200).unwrap();
        assert!(newest[0].is_some(), "the most recently used entry stays");
    }

    /// Control: drop the `models` retention `DELETE` and the stale model's
    /// entries survive.
    #[test]
    fn gc_drops_a_model_unused_for_thirty_days_but_never_the_current_one() {
        let (_dir, mut cache) = open_temp();
        let stale = cache.model_id(&[1; 32], 100).unwrap();
        let current = cache.model_id(&[2; 32], 100).unwrap();
        cache.insert(stale, &[(text_hash("a"), &vector(1))], 100).unwrap();
        cache.insert(current, &[(text_hash("a"), &vector(2))], 100).unwrap();

        let outcome = cache.gc(current, u64::MAX, 100 + MODEL_RETENTION_DAYS + 1).unwrap();

        assert_eq!(outcome.models_dropped, 1);
        let remaining: i64 =
            cache.connection().query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 1, "only the current model's entry survives");
    }

    /// Control: return the busy error from `gc` instead of mapping it to a
    /// skipped round, and this fails.
    #[test]
    fn gc_skips_its_round_when_another_writer_holds_the_file() {
        let (dir, mut cache) = open_temp();
        let model = cache.model_id(&[1; 32], 100).unwrap();
        let other = Connection::open(dir.path().join(DIR_NAME).join(FILE_NAME)).unwrap();
        other.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let outcome = cache.gc(model, 0, 100).unwrap();

        assert!(outcome.skipped);
        other.execute_batch("COMMIT").unwrap();
    }

    /// Set in a child process to make [`concurrent_writer_child`] act as one
    /// of [`concurrent_writers_share_one_cache`]'s writers:
    /// `<cache path>|<go file>|<writer index>`.
    const WRITER_ENV: &str = "G_MESH_TEST_CACHE_WRITER";
    const WRITERS: u64 = 4;
    const KEYS_PER_WRITER: u64 = 150;
    const WRITER_STRIDE: u64 = 50;

    fn writer_key(k: u64) -> Hash {
        text_hash(&format!("shared text {k}"))
    }

    fn writer_vector(k: u64) -> Vec<f32> {
        (0..EMBEDDING_DIM).map(|i| (k * 1000 + i as u64) as f32 / 7.0).collect()
    }

    /// One writer: waits for the go file, then inserts its overlapping key
    /// range in small batches, panicking (a non-zero exit) on any error. A
    /// no-op when not spawned by the test below.
    #[test]
    fn concurrent_writer_child() {
        let Ok(spec) = std::env::var(WRITER_ENV) else { return };
        let mut parts = spec.split('|');
        let path = PathBuf::from(parts.next().unwrap());
        let go = PathBuf::from(parts.next().unwrap());
        let index: u64 = parts.next().unwrap().parse().unwrap();
        while !go.exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
        // A busy open is retried by the pipeline on its next batch; this
        // mirrors that.
        let mut cache = None;
        for _ in 0..40 {
            match EmbeddingCache::open(&path) {
                Ok(opened) => {
                    cache = Some(opened);
                    break;
                }
                Err(OpenError::Busy(_)) => std::thread::sleep(Duration::from_millis(25)),
                Err(OpenError::Failed(err)) => panic!("writer {index} could not open the cache: {err:#}"),
            }
        }
        let mut cache = cache.expect("the cache stayed busy for a whole second");
        let model = cache.model_id(&[9; 32], 100).expect("model_id failed");
        let first = index * WRITER_STRIDE;
        let keys: Vec<u64> = (first..first + KEYS_PER_WRITER).collect();
        for batch in keys.chunks(10) {
            let vectors: Vec<(Hash, Vec<f32>)> =
                batch.iter().map(|k| (writer_key(*k), writer_vector(*k))).collect();
            let entries: Vec<(Hash, &[f32])> = vectors.iter().map(|(key, v)| (*key, v.as_slice())).collect();
            cache
                .insert(model, &entries, 100)
                .unwrap_or_else(|err| panic!("writer {index} insert failed: {err:#}"));
            cache.lookup(model, &[writer_key(first)], 100).expect("lookup failed");
        }
    }

    /// Four processes writing overlapping keys into one cache at once: none
    /// errors, every key is present exactly once, with its own bytes.
    ///
    /// Control: drop `busy_timeout` in `open_in_place` (writers then fail
    /// with SQLITE_BUSY) or `OR IGNORE` in `insert` (overlapping keys then
    /// fail with a constraint error) and a writer exits non-zero.
    #[test]
    fn concurrent_writers_share_one_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DIR_NAME).join(FILE_NAME);
        let go = dir.path().join("go");
        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..WRITERS)
            .map(|index| {
                std::process::Command::new(&exe)
                    .args(["--exact", "embedding::cache::tests::concurrent_writer_child", "--nocapture"])
                    .env(WRITER_ENV, format!("{}|{}|{index}", path.display(), go.display()))
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        fs::write(&go, "").unwrap();
        for (index, child) in children.into_iter().enumerate() {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "writer {index} failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let cache = EmbeddingCache::open(&path).map_err(|err| format!("{err:?}")).unwrap();
        let model = cache.model_id(&[9; 32], 100).unwrap();
        let total = (WRITERS - 1) * WRITER_STRIDE + KEYS_PER_WRITER;
        let rows: i64 =
            cache.connection().query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).unwrap();
        assert_eq!(rows as u64, total, "every key exactly once");
        let keys: Vec<Hash> = (0..total).map(writer_key).collect();
        let found = cache.lookup(model, &keys, 100).unwrap();
        for (k, vector) in found.iter().enumerate() {
            let vector = vector.as_ref().unwrap_or_else(|| panic!("key {k} is missing"));
            assert_eq!(bits(vector), bits(&writer_vector(k as u64)), "key {k} carries another key's bytes");
        }
    }

    fn bits(vector: &[f32]) -> Vec<u32> {
        vector.iter().map(|value| value.to_bits()).collect()
    }
}
