//! The addresses unresolved relative imports wait at, so that a
//! file's arrival costs core an import scan only when someone may be
//! waiting for it.
//!
//! An unresolved relative specifier (`./missing` in `src/a.ts`) leaves an
//! unlinked placeholder addressed at the path it names (`src/missing`,
//! [`relative_address`](super::resolve::relative_address)). Core links it
//! by itself onto a new file of exactly that path; any other candidate
//! (`src/missing.ts`, `src/missing/index.ts`) needs the importer
//! re-extracted, which only an `Affected` delta naming the address asks for.
//!
//! Two sources of knowledge, because the index core holds was not all
//! extracted by this model:
//! - **exact**: every importer this model extracted itself (its
//!   `fileChanged`) recorded the addresses its relative imports did not
//!   resolve, against the same file set its extraction used;
//! - **mentions**: every other file's rows predate this model (the bulk walk
//!   ran in another process, the daemon or plugin restarted, a hydrated file
//!   was never reported, a reload rebuilt the model). What core holds for
//!   them is unknown here, so a superset stands in: the words written in
//!   those files on disk, read once per model on the first question. A file
//!   that may name an address is assumed to wait at it, resolved or not,
//!   because a target created while no model watched is already on disk.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use g_mesh_plugin_sdk::RelPath;

use crate::project::paths;

/// See the module doc.
#[derive(Debug, Default, Clone)]
pub struct Waiting {
    /// The unresolved addresses of each importer's last extraction by this
    /// model; an importer with none has no entry.
    by_importer: BTreeMap<RelPath, BTreeSet<String>>,
    /// The same, inverted.
    by_address: BTreeMap<String, BTreeSet<RelPath>>,
    /// Built on the first [`Waiting::waited`]; `None` until then.
    mentions: Option<Mentions>,
}

impl Waiting {
    /// `importer` was just extracted with `addresses` unresolved; replaces
    /// what its previous extraction recorded.
    pub fn record(&mut self, importer: &RelPath, addresses: BTreeSet<String>) {
        self.forget(importer);
        for address in &addresses {
            self.by_address.entry(address.clone()).or_default().insert(importer.clone());
        }
        if !addresses.is_empty() {
            self.by_importer.insert(importer.clone(), addresses);
        }
    }

    /// `importer` is gone: nothing of it waits any more.
    pub fn forget(&mut self, importer: &RelPath) {
        let Some(addresses) = self.by_importer.remove(importer) else { return };
        for address in addresses {
            if let Some(importers) = self.by_address.get_mut(&address) {
                importers.remove(importer);
                if importers.is_empty() {
                    self.by_address.remove(&address);
                }
            }
        }
    }

    /// The `addresses` a newly present file answers that an import may be
    /// waiting at, in their order. The exact waiters of each one returned are
    /// dropped: the file now exists as a candidate of it, so their re-extract
    /// resolves it, and one that core re-extracts records afresh anyway.
    ///
    /// The first call reads every file of `existence` under `root` for
    /// [`Mentions`]; a file it cannot read makes every address count.
    pub fn waited(
        &mut self,
        root: &Path,
        existence: &BTreeSet<RelPath>,
        addresses: &[String],
    ) -> Vec<String> {
        if addresses.is_empty() {
            return Vec::new();
        }
        let mentions = self.mentions.get_or_insert_with(|| Mentions::scan(root, existence));
        let waited: Vec<String> = addresses
            .iter()
            .filter(|address| self.by_address.contains_key(*address) || mentions.may_name(address))
            .cloned()
            .collect();
        for address in &waited {
            for importer in self.by_address.remove(address).unwrap_or_default() {
                if let Some(addresses) = self.by_importer.get_mut(&importer) {
                    addresses.remove(address);
                    if addresses.is_empty() {
                        self.by_importer.remove(&importer);
                    }
                }
            }
        }
        waited
    }
}

/// What the files this model did not extract may name, as a superset: the
/// words written in them, and the directories of those that may write a
/// specifier of dots alone (`'..'`, `'./'`) or climb through `/..`.
///
/// Sound for every specifier the extractor folds (a literal, a template of
/// string constants, `path.join` of literals), since each of its words is
/// written whole in some literal of the file; a name split across two
/// literals (`` `./${'mis'}sing` ``) is the one shape it misses.
#[derive(Debug, Default, Clone)]
pub struct Mentions {
    words: HashSet<String>,
    dot_dirs: BTreeSet<String>,
    /// A file could not be read: anything may be waited on.
    opaque: bool,
}

impl Mentions {
    fn scan(root: &Path, files: &BTreeSet<RelPath>) -> Self {
        let mut mentions = Self::default();
        for path in files {
            match std::fs::read(path.to_absolute(root)) {
                Ok(bytes) => mentions.add(path, &String::from_utf8_lossy(&bytes)),
                Err(_) => {
                    mentions.opaque = true;
                    break;
                }
            }
        }
        mentions
    }

    fn add(&mut self, path: &RelPath, source: &str) {
        for word in words(source) {
            if !self.words.contains(word) {
                self.words.insert(word.to_string());
            }
        }
        if may_write_dots_alone(source) {
            self.dot_dirs.insert(paths::dirname(path.as_str()).to_string());
        }
    }

    /// Whether one of these files may name `address`: every word of its
    /// last segment is written in one of them; or it is
    /// a directory at or above one that may climb to it by dots alone.
    fn may_name(&self, address: &str) -> bool {
        if self.opaque {
            return true;
        }
        if words(paths::basename(address)).all(|word| self.words.contains(word)) {
            return true;
        }
        let below = format!("{address}/");
        self.dot_dirs.iter().any(|dir| dir == address || dir.starts_with(&below))
    }
}

/// The identifier-like runs of `text`.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).filter(|word| !word.is_empty())
}

/// Whether `source` holds a quoted run of dots and slashes alone (`'..'`,
/// `"./"`, `` `.` ``) or a `/..` anywhere: what a specifier naming one of
/// the importer's own ancestor directories is folded from.
fn may_write_dots_alone(source: &str) -> bool {
    if source.contains("/..") {
        return true;
    }
    let bytes = source.as_bytes();
    bytes.iter().enumerate().any(|(at, &quote)| {
        matches!(quote, b'\'' | b'"' | b'`') && {
            let run = bytes[at + 1..].iter().take_while(|&&byte| byte == b'.' || byte == b'/').count();
            run > 0 && bytes.get(at + 1 + run) == Some(&quote)
        }
    })
}
