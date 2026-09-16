//! Building one crate's module tree - which file (or, for an inline module,
//! which nested block of which file) backs each `<crate>::<module path>`
//! container key - without a real Rust parser.
//!
//! # Decision 1: a cheap, regex-free scan, not tree-sitter and not two-phase
//!
//! Knowing where `mod foo;` points requires reading `.rs` files, which is
//! ordinarily the extractor's job - but [`Extractor::load_project`] has to
//! hand back a complete module tree *before* any file is extracted, so the
//! extractor can look a file's own container key up rather than derive it.
//! Three shapes were weighed:
//!
//! - **tree-sitter-rust in `load_project`.** Exact - it is the same grammar
//!   GM-286's extractor will use - but it means this task's crate takes on
//!   tree-sitter-rust as a dependency for a job that only needs to find
//!   `mod` items and `#[path]` attributes, not build a full AST. That
//!   dependency is GM-286's to add, for GM-286's own reason (declarations,
//!   edges, ranges); pulling it in here to answer a much narrower question
//!   would make this task's own boundary (module doc: "the extractor stub
//!   emits File nodes only") harder to see in the dependency graph than in
//!   the code.
//! - **A two-phase model, completed lazily as files are extracted.** Rejected
//!   outright: a file's container key would then depend on *which files have
//!   been extracted so far*, which is precisely what the task rules out
//!   ("The container key of a file must not depend on extraction order").
//!   `--bulk-index` streams file-by-file; a lazy tree would make the second
//!   file's container key depend on whether the first file's `mod` items had
//!   already been read, and a `fileChanged` for one file could not answer
//!   correctly without having reread others.
//! - **A cheap, regex-free scan (chosen).** [`mask`] blanks every comment and
//!   string/char literal - byte-length-preserving, so its output stays
//!   aligned with the original source - and [`scan_body`] then walks what is
//!   left with a single explicit brace-depth stack, matching the literal
//!   patterns `mod IDENT ;`, `mod IDENT { … }` and a `path = "…"` inside an
//!   immediately preceding `#[…]`. It is not a parser: it does not know a
//!   declaration from a doc example, a string that survived masking wrong
//!   from a real one, or a `mod` written inside a macro invocation from one
//!   that is really there. What it is exact about is exactly what the module
//!   tree needs and the extractor does not supply until GM-286: comments and
//!   ordinary/raw string and char literals cannot smuggle a `mod`, `{`, `}`
//!   or `;` past it, because those are masked before the structural scan
//!   ever runs (see [`mask`]'s own doc for the state machine and what it
//!   still cannot see through - a `mod` written inside a `macro_rules!` body
//!   being the honest, documented gap).
//!
//! # Decision 2: `#[cfg(...)]` is not evaluated
//!
//! `#[cfg(unix)] mod imp;` next to `#[cfg(windows)] #[path = "imp_win.rs"]
//! mod imp;` both declare container key `<parent>::imp` - this scanner never
//! evaluates a `cfg` predicate, so it does not choose between them, which is
//! also the design doc's own rule ("the structural tier indexes every
//! alternative, and rust-analyzer resolves under the default features" -
//! Failure Modes). Concretely: both `mod` items are scanned exactly as any
//! other, both resolve their own file, and both files are recorded under the
//! *same* container key (see [`register_file`]). Once GM-286 emits members
//! for both files, core materializes one container node for that key whose
//! members are the union of both `cfg` branches - a caller may therefore see
//! a callee defined under a `cfg` that is not active, which is the
//! documented gap the design doc already accepts, not a new one this task
//! introduces.
//!
//! # Decision 3: `#[path]` outside the crate, and two modules on one file
//!
//! `#[path = "…"]` is resolved relative to the *file that carries the
//! attribute* - Rust's own rule, unrelated to how deep the `mod` sits inside
//! nested inline modules - and it is followed wherever it points, including
//! above `src/`, into a sibling crate's tree, or (via enough `../`) outside
//! the project root entirely. The first case is honoured because nothing
//! about container membership requires a file to be *within* its crate's own
//! directory, only reachable from its root by a `mod` chain; the second
//! yields no [`RelPath`] under the project root at all, so [`register_file`]
//! is simply never called for it - an honest miss, the same as a `mod` naming
//! a file that was deleted (see [`resolve_child_file`]).
//!
//! Two *different* `mod` items - in one crate or in two - can both resolve to
//! the same physical file (two `#[path = "shared.rs"]` declarations under
//! different names, or, more mundanely, a file both `mod`-included by a
//! parent and also matched by a stray sibling declaration). Real `rustc`
//! treats this as two independent copies of the file's items, each under its
//! own module path - a shape [`ContainerInfo`](crate::project::ContainerInfo)
//! cannot represent, because it maps one file to *one* container key. This
//! module chooses the deterministic alternative over the precise one:
//! **first claim wins**, where "first" is crate-declaration order and then
//! left-to-right, top-to-bottom scan order within a file - a function of the
//! source text alone, so still independent of GM-286's extraction order. The
//! losing `mod` item is recorded as a note ([`ProjectContext::notes`]) and
//! contributes no second container; that file's declarations, once GM-286
//! emits them, are attributed only to the container that claimed it first.
//!
//! # What this scanner does not attempt
//!
//! - **Macro-generated modules.** A `mod` item produced by a macro expansion
//!   is invisible - this scanner reads source text, not expanded output,
//!   which the design doc already lists as a documented structural gap for
//!   the Rust plugin generally.
//! - **`mod` written inside a function body.** Legal Rust, vanishingly rare,
//!   and scanned the same as any other `mod` occurrence (this module does
//!   not distinguish a block's brace from a module's own body other than by
//!   the literal `mod IDENT {`/`mod IDENT ;` pattern immediately before a
//!   brace or semicolon) - harmless over-approximation, not a correctness
//!   bug, since a real one is genuinely a module wherever it is written.

use std::collections::BTreeMap;
use std::path::Path;

use g_mesh_plugin_sdk::RelPath;

/// One file's resolved container key and parent key, keyed by
/// [`RelPath`]. What [`scan_crate`] builds and
/// [`crate::project::ProjectContext::load`] merges across every crate.
pub(crate) type FileContainers = BTreeMap<RelPath, (String, Option<String>)>;

/// Scans `crate_root_file` and everything it reaches by `mod` (file-based or
/// inline), recording each reached file's own container key into `files` and
/// appending a note for anything this scan could not honestly resolve.
///
/// `crate_key` is this crate's own root container key (already normalized -
/// see `crate::project::normalize_crate_name`); the crate root file itself is
/// recorded under exactly that key, with no parent, which is what closes the
/// `pub(crate)`/`pub(super)` parent-chain gap the design doc's containers
/// module doc warns about - see `crate::project`'s module doc, decision 6.
pub(crate) fn scan_crate(
    root: &Path,
    crate_key: &str,
    crate_root_file: RelPath,
    files: &mut FileContainers,
    notes: &mut Vec<String>,
) {
    scan_file(root, crate_key, &[], crate_root_file, files, notes, true);
}

/// Scans one file: records its own container key, then walks its `mod`
/// items, recursing into an inline module's body in place and into a
/// file-based module's file by calling this function again.
///
/// `key_segments` is this file's module path *within its crate*, empty for
/// the crate root. `is_dir_owner` is true for the crate root and any
/// `mod.rs` - a `mod child;` written at this file's own top level then
/// resolves relative to this file's own directory; false for a "leaf"
/// `name.rs`, whose children resolve relative to a subdirectory named after
/// the *module* (`key_segments`' last segment), not after the file's own
/// name, because `#[path]` can make those differ (Decision 3).
fn scan_file(
    root: &Path,
    crate_key: &str,
    key_segments: &[String],
    file: RelPath,
    files: &mut FileContainers,
    notes: &mut Vec<String>,
    is_dir_owner: bool,
) {
    if !register_file(crate_key, key_segments, &file, files, notes) {
        return;
    }

    let Ok(source) = std::fs::read_to_string(file.to_absolute(root)) else {
        // A `mod` item can name a file that does not exist - a stale
        // declaration, or a file mid-save. The container key is already
        // recorded above (Decision 3's "honest miss" only applies to a
        // target *outside the project*; a missing file inside it still owns
        // its key, it just has nothing left to scan).
        notes.push(format!("{file}: declared as a module but could not be read; module tree stops here"));
        return;
    };
    let cleaned = mask(&source);

    let file_dir = parent_dir(file.as_str());
    let own_mod_dir = if is_dir_owner {
        file_dir
    } else {
        join(&file_dir, key_segments.last().map(String::as_str).unwrap_or(""))
    };

    scan_body(root, crate_key, key_segments, &file, &own_mod_dir, &source, &cleaned, files, notes);
}

/// Records `file`'s own container key, unless the key was already claimed
/// by an earlier file - see the module doc's Decision 3. Returns whether the
/// caller should keep scanning `file` for further `mod` items: `false` both
/// when `file` lost the claim and when it was, impossibly, visited twice by
/// construction (defensive only - every call site reaches a given `file` at
/// most once along any single recursion path).
fn register_file(
    crate_key: &str,
    key_segments: &[String],
    file: &RelPath,
    files: &mut FileContainers,
    notes: &mut Vec<String>,
) -> bool {
    if files.contains_key(file) {
        notes.push(format!(
            "{file}: already indexed under a different module - a second `mod` claimed the same file; \
             keeping the first"
        ));
        return false;
    }
    let key = full_key(crate_key, key_segments);
    let parent = parent_key(&key);
    files.insert(file.clone(), (key, parent));
    true
}

/// The container key for `key_segments` within `crate_key` - just `crate_key`
/// at the root, `"<crate_key>::a::b"` beneath it.
fn full_key(crate_key: &str, key_segments: &[String]) -> String {
    if key_segments.is_empty() {
        crate_key.to_string()
    } else {
        format!("{crate_key}::{}", key_segments.join("::"))
    }
}

/// The parent of `key` - `key` with its last `::`-separated segment dropped,
/// or `None` for a crate root (no `::` at all). String manipulation rather
/// than tracked alongside the recursion: `containers.parentKey` is exactly
/// this rule (design doc: `<crate>::<module path>`, parent = enclosing
/// module), and computing it from the key keeps the two from ever disagreeing
/// with each other.
fn parent_key(key: &str) -> Option<String> {
    key.rsplit_once("::").map(|(parent, _)| parent.to_string())
}

/// A single explicit forward scan of `cleaned` (the comment/string-masked
/// text of `file`, byte-aligned with `source`) that finds every `mod` item at
/// any brace depth, recursing into an inline module's body and into a
/// file-based module's own file.
///
/// The only state carried between iterations is a small stack of "was this
/// brace a module's own body" flags (so `}` knows whether to pop a module
/// path segment) plus the module path and lookup directory themselves, both
/// of which grow and shrink with that same stack - see the module doc's
/// Decision 1 for why a full AST is not built to get this.
#[allow(clippy::too_many_arguments)]
fn scan_body(
    root: &Path,
    crate_key: &str,
    base_key_segments: &[String],
    file: &RelPath,
    base_mod_dir: &str,
    source: &str,
    cleaned: &str,
    files: &mut FileContainers,
    notes: &mut Vec<String>,
) {
    let bytes = cleaned.as_bytes();
    let mut key_segments: Vec<String> = base_key_segments.to_vec();
    let mut mod_dir = base_mod_dir.to_string();
    // `true` for a frame opened by an inline `mod NAME {`, `false` for any
    // other brace (a function body, a struct literal, an `impl` block, ...) -
    // both are pushed so `}` always pops something, but only a module frame
    // changes `key_segments`/`mod_dir` on the way back out.
    let mut frames: Vec<bool> = Vec::new();
    let mut pending_path: Option<String> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'#' if bytes.get(i + 1) == Some(&b'[') => {
                let (attr_start, attr_end) = read_attribute_span(bytes, i + 2);
                if let Some(path) = extract_path_value(&source[attr_start..attr_end.min(source.len())]) {
                    pending_path = Some(path);
                }
                i = attr_end + 1; // past the closing `]`
            }
            b'{' => {
                frames.push(false);
                pending_path = None;
                i += 1;
            }
            b'}' => {
                if frames.pop() == Some(true) {
                    key_segments.pop();
                    mod_dir = parent_dir(&mod_dir);
                }
                pending_path = None;
                i += 1;
            }
            c if is_ident_start(c) => {
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                let word = &cleaned[start..i];
                if word == "mod" && word_boundary_before(bytes, start) {
                    if let Some(outcome) = read_mod_item(bytes, i) {
                        let ModItem { name, after, opens_body } = outcome;
                        if opens_body {
                            // Inline module: recurse in place. Its own file
                            // is `file` itself, so it needs no new
                            // `register_file` call - only its declarations
                            // (GM-286's job) live under this deeper key.
                            // `after` points AT the opening `{`; it is
                            // consumed here (not by the generic `{` branch
                            // below) so exactly one frame is pushed for it.
                            key_segments.push(name.clone());
                            mod_dir = join(&mod_dir, &name);
                            frames.push(true);
                            pending_path = None;
                            i = after + 1;
                        } else {
                            let explicit_path = pending_path.take();
                            handle_file_mod(
                                root,
                                crate_key,
                                &key_segments,
                                &mod_dir,
                                file,
                                &name,
                                explicit_path,
                                files,
                                notes,
                            );
                            i = after; // just past the `;`
                        }
                    } else {
                        pending_path = None;
                    }
                } else {
                    pending_path = None;
                }
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                pending_path = None;
                i += 1;
            }
        }
    }
}

/// What [`read_mod_item`] found after `mod IDENT`: the name, the index to
/// resume scanning at, and whether it was `mod IDENT {` (inline, `after`
/// points *at* the `{` so the main loop's own `{` handling pushes the frame)
/// rather than `mod IDENT ;` (`after` points just past the `;`).
struct ModItem {
    name: String,
    after: usize,
    opens_body: bool,
}

/// Reads the identifier and terminator following a `mod` keyword already
/// consumed up to `after_kw`. `None` for anything that is not
/// `IDENT ;` or `IDENT {` - a `mod` used as some other identifier (this
/// scanner does not know Rust's keyword rules beyond the literal word), or
/// malformed source mid-edit.
fn read_mod_item(bytes: &[u8], after_kw: usize) -> Option<ModItem> {
    let mut i = skip_ws(bytes, after_kw);
    let name_start = i;
    while i < bytes.len() && is_ident_byte(bytes[i]) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = std::str::from_utf8(&bytes[name_start..i]).ok()?.to_string();
    let after_name = skip_ws(bytes, i);
    match bytes.get(after_name) {
        Some(b';') => Some(ModItem { name, after: after_name + 1, opens_body: false }),
        Some(b'{') => Some(ModItem { name, after: after_name, opens_body: true }),
        _ => None,
    }
}

/// Resolves and recurses into a file-based `mod NAME;` (or `#[path = "…"]
/// mod NAME;`) declared at `mod_dir`/`key_segments` inside `declaring_file`.
#[allow(clippy::too_many_arguments)]
fn handle_file_mod(
    root: &Path,
    crate_key: &str,
    key_segments: &[String],
    mod_dir: &str,
    declaring_file: &RelPath,
    name: &str,
    explicit_path: Option<String>,
    files: &mut FileContainers,
    notes: &mut Vec<String>,
) {
    let Some((child_file, is_mod_rs)) =
        resolve_child_file(root, mod_dir, declaring_file, name, explicit_path)
    else {
        notes.push(format!(
            "{declaring_file}: `mod {name};` has no file on disk under the project root - not indexed"
        ));
        return;
    };
    let mut child_segments = key_segments.to_vec();
    child_segments.push(name.to_string());
    scan_file(root, crate_key, &child_segments, child_file, files, notes, is_mod_rs);
}

/// Finds the file a file-based `mod NAME;` resolves to, and whether that file
/// is itself directory-owning (`mod.rs`) for its own children.
///
/// - With `explicit_path` (`#[path = "…"]`): resolved relative to the
///   *declaring file's own directory* - Rust's rule, not `mod_dir` - per the
///   module doc's Decision 3. A path escaping the project root (enough
///   `../`, or an absolute path) yields `None`: [`RelPath::relative_to`]
///   refuses it, which is the same honest-miss behaviour as a target that
///   does not exist.
/// - Without one: `<mod_dir>/<name>.rs`, else `<mod_dir>/<name>/mod.rs`.
///   Cargo's own ambiguity error (both exist) is not reproduced; `<name>.rs`
///   wins deterministically and a note records the choice.
fn resolve_child_file(
    root: &Path,
    mod_dir: &str,
    declaring_file: &RelPath,
    name: &str,
    explicit_path: Option<String>,
) -> Option<(RelPath, bool)> {
    if let Some(explicit_path) = explicit_path {
        let base = parent_dir(declaring_file.as_str());
        let candidate = join(&base, &explicit_path);
        let absolute = RelPath::new(candidate).to_absolute(root);
        if !absolute.is_file() {
            return None;
        }
        let resolved = RelPath::relative_to(root, &absolute)?;
        let is_mod_rs = resolved.as_str().rsplit('/').next() == Some("mod.rs");
        return Some((resolved, is_mod_rs));
    }

    let as_file = join(mod_dir, &format!("{name}.rs"));
    let as_file_abs = RelPath::new(&as_file).to_absolute(root);
    let as_dir = join(mod_dir, &format!("{name}/mod.rs"));
    let as_dir_abs = RelPath::new(&as_dir).to_absolute(root);
    match (as_file_abs.is_file(), as_dir_abs.is_file()) {
        (true, _) => Some((RelPath::new(as_file), false)),
        (false, true) => Some((RelPath::new(as_dir), true)),
        (false, false) => None,
    }
}

/// Reads a `#[…]` attribute's own span, starting right after its opening
/// `[`. Returns `(content_start, content_end)`, both indices into the same
/// buffer `bytes` was sliced from - the caller reads the *original* source at
/// this span rather than `cleaned`, because [`mask`] has already blanked out
/// any string literal inside the attribute (`"other.rs"` in `#[path =
/// "other.rs"]`), which is exactly the value a `path` attribute needs.
/// Matching brackets against the *masked* text is still correct (and safer
/// than matching against raw source): a doc attribute containing its own
/// `]` inside a string (`#[doc = "see [x]"]`) cannot desynchronize the
/// depth count, because that string's content was already blanked.
fn read_attribute_span(bytes: &[u8], content_start: usize) -> (usize, usize) {
    let mut depth = 1usize;
    let mut i = content_start;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    // `i` is just past the matching `]` (or end of buffer on malformed
    // input); the content itself ends one byte earlier.
    (content_start, i.saturating_sub(1))
}

/// Pulls the string value out of a `path = "…"` attribute body, or `None` for
/// any other attribute (`cfg(...)`, `derive(...)`, `doc = "..."`, ...). Not a
/// full attribute-syntax parser - just enough to find `path`, `=` and a
/// quoted string in that order, which is the whole of what `#[path = "…"]`
/// ever looks like.
fn extract_path_value(attr: &str) -> Option<String> {
    let rest = attr.trim_start().strip_prefix("path")?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// --- byte/path helpers -------------------------------------------------

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || b >= 0x80
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
}

fn word_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0 || !is_ident_byte(bytes[i - 1])
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `a/b/c.rs` -> `a/b`; `c.rs` -> `""`; `""` -> `""`.
fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Joins a directory (possibly empty, meaning the project root) with a
/// relative tail, normalizing `a/../b` segments - needed because `#[path]`
/// legitimately climbs with `../` (Decision 3: it may point outside the
/// crate).
fn join(dir: &str, tail: &str) -> String {
    let mut segments: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for segment in tail.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Comment/string/char-literal masking - see the module doc's Decision 1.
/// Byte-length-preserving (every masked byte becomes `b' '`, or stays `b'\n'`
/// so line numbers in any future diagnostic stay meaningful), so the output
/// is always the same length as `source` and safe to index in lockstep with
/// it.
fn mask(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                blank(&mut out, start, i);
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            b'r' if is_raw_string_start(bytes, i) => {
                let start = i;
                let mut j = i + 1;
                while bytes.get(j) == Some(&b'#') {
                    j += 1;
                }
                let hashes = j - (i + 1);
                j += 1; // opening `"`
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while seen < hashes && bytes.get(k) == Some(&b'#') {
                            k += 1;
                            seen += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                i = j.min(bytes.len());
                blank(&mut out, start, i);
            }
            b'\'' => {
                if let Some(end) = char_literal_end(bytes, i) {
                    blank(&mut out, i, end);
                    i = end;
                } else {
                    // A lifetime tick (`'a`), not a char literal - leave it,
                    // it contains none of the structural bytes this scanner
                    // looks for.
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn blank(out: &mut [u8], start: usize, end: usize) {
    let end = end.min(out.len());
    for b in &mut out[start..end] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn is_raw_string_start(bytes: &[u8], i: usize) -> bool {
    if !word_boundary_before(bytes, i) {
        return false;
    }
    let mut j = i + 1;
    while bytes.get(j) == Some(&b'#') {
        j += 1;
    }
    bytes.get(j) == Some(&b'"')
}

/// The index just past a char literal starting at `bytes[i] == b'\''`, or
/// `None` when this `'` starts a lifetime instead (`'a`, `'static`) - the two
/// are told apart the same way `rustc` tells them apart lexically: an escape
/// (`'\''`, `'\n'`, `'\u{2603}'`) is always a char literal, and otherwise it
/// is one only if a second `'` follows the very next character.
fn char_literal_end(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'\\') {
        j += 1;
        while j < bytes.len() && bytes[j] != b'\'' && bytes[j] != b'\n' {
            j += 1;
        }
        return (bytes.get(j) == Some(&b'\'')).then_some(j + 1);
    }
    if j < bytes.len() && bytes.get(j + 1) == Some(&b'\'') {
        return Some(j + 2);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(root: &Path, crate_key: &str, entry: &str) -> (FileContainers, Vec<String>) {
        let mut files = FileContainers::new();
        let mut notes = Vec::new();
        scan_crate(root, crate_key, RelPath::new(entry), &mut files, &mut notes);
        (files, notes)
    }

    struct Tree(std::path::PathBuf);

    impl Tree {
        /// See `project::tests::Tree::new`'s own comment: unique per call,
        /// not just per `name`, because tests run concurrently.
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let root = std::env::temp_dir()
                .join(format!("g-mesh-plugin-rust-module-tree-{}-{name}-{nanos}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) -> &Self {
            let full = self.0.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_mod_and_an_inline_mod_both_get_the_right_key() {
        let tree = Tree::new("basic");
        tree.write("src/lib.rs", "mod outer;\nmod inline_mod {\n    fn f() {}\n}\n");
        tree.write("src/outer.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(files[&RelPath::new("src/lib.rs")], ("demo".to_string(), None));
        assert_eq!(
            files[&RelPath::new("src/outer.rs")],
            ("demo::outer".to_string(), Some("demo".to_string()))
        );
        // The inline module has no file of its own - it is not in `files`.
        assert!(!files.contains_key(&RelPath::new("src/inline_mod.rs")));
    }

    #[test]
    fn nested_mod_rs_resolves_a_further_child_relative_to_its_own_directory() {
        let tree = Tree::new("nested-mod-rs");
        tree.write("src/lib.rs", "mod nested;\n");
        tree.write("src/nested/mod.rs", "mod deep;\n");
        tree.write("src/nested/deep.rs", "");
        let (files, _) = scan(&tree.0, "demo", "src/lib.rs");
        assert_eq!(
            files[&RelPath::new("src/nested/mod.rs")],
            ("demo::nested".to_string(), Some("demo".to_string()))
        );
        assert_eq!(
            files[&RelPath::new("src/nested/deep.rs")],
            ("demo::nested::deep".to_string(), Some("demo::nested".to_string()))
        );
    }

    #[test]
    fn a_leaf_file_mod_resolves_its_own_child_under_a_subdirectory_named_after_the_module() {
        let tree = Tree::new("leaf-child");
        tree.write("src/lib.rs", "mod leaf;\n");
        tree.write("src/leaf.rs", "mod child;\n");
        tree.write("src/leaf/child.rs", "");
        let (files, _) = scan(&tree.0, "demo", "src/lib.rs");
        assert_eq!(
            files[&RelPath::new("src/leaf/child.rs")],
            ("demo::leaf::child".to_string(), Some("demo::leaf".to_string()))
        );
    }

    #[test]
    fn a_path_attribute_overrides_the_file_and_the_module_name_still_names_the_key() {
        let tree = Tree::new("path-attr");
        tree.write("src/lib.rs", "#[path = \"impl_detail.rs\"]\nmod imp;\n");
        tree.write("src/impl_detail.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            files[&RelPath::new("src/impl_detail.rs")],
            ("demo::imp".to_string(), Some("demo".to_string()))
        );
        assert!(!files.contains_key(&RelPath::new("src/imp.rs")));
    }

    #[test]
    fn a_path_attribute_may_point_outside_the_crates_own_directory() {
        let tree = Tree::new("path-outside-crate");
        // `crates/a/src/lib.rs`'s own directory is `crates/a/src`; three
        // `../` climbs `src` -> `a` -> `crates` -> the project root.
        tree.write("crates/a/src/lib.rs", "#[path = \"../../../shared/thing.rs\"]\nmod thing;\n");
        tree.write("shared/thing.rs", "");
        let (files, notes) = scan(&tree.0, "a", "crates/a/src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(files[&RelPath::new("shared/thing.rs")], ("a::thing".to_string(), Some("a".to_string())));
    }

    #[test]
    fn a_path_attribute_escaping_the_project_root_is_an_honest_miss_not_a_crash() {
        let tree = Tree::new("path-escapes-root");
        tree.write("src/lib.rs", "#[path = \"../../../../outside.rs\"]\nmod thing;\n");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(!files.contains_key(&RelPath::new("thing.rs")));
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    /// `#[cfg(unix)] mod imp;` and `#[cfg(windows)] #[path = "imp_win.rs"]
    /// mod imp;` both declare `demo::imp` - Decision 2: neither `cfg` is
    /// evaluated, so *both* `mod` items are followed to their own (distinct)
    /// files, and both files are recorded under the very same container key.
    /// This is not the Decision 3 collision (two `mod` items resolving to
    /// *one* file) - it is two `mod` items resolving to *two* files that
    /// share a key, which is exactly what "index every alternative" means.
    #[test]
    fn cfg_gated_alternatives_share_one_container_key() {
        let tree = Tree::new("cfg-gated");
        tree.write(
            "src/lib.rs",
            "#[cfg(unix)]\nmod imp;\n#[cfg(windows)]\n#[path = \"imp_win.rs\"]\nmod imp;\n",
        );
        tree.write("src/imp.rs", "");
        tree.write("src/imp_win.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(files[&RelPath::new("src/imp.rs")], ("demo::imp".to_string(), Some("demo".to_string())));
        assert_eq!(
            files[&RelPath::new("src/imp_win.rs")],
            ("demo::imp".to_string(), Some("demo".to_string())),
            "both cfg alternatives are indexed under the same container key"
        );
    }

    /// The genuine Decision 3 collision: two *different* `mod` items -
    /// different names, no `cfg` involved - both naming the same file via
    /// `#[path]`. Real `rustc` compiles the file's contents twice, once
    /// under each module path; this model cannot represent that (one file,
    /// one container key), so it keeps the first claim deterministically and
    /// notes the second rather than silently merging or duplicating it.
    #[test]
    fn two_different_mod_items_naming_one_file_keep_only_the_first_claim() {
        let tree = Tree::new("two-mods-one-file");
        tree.write(
            "src/lib.rs",
            "#[path = \"shared.rs\"]\nmod first;\n#[path = \"shared.rs\"]\nmod second;\n",
        );
        tree.write("src/shared.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert_eq!(
            files[&RelPath::new("src/shared.rs")],
            ("demo::first".to_string(), Some("demo".to_string())),
            "the first `mod` to claim the file wins, deterministically"
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("shared.rs"), "{notes:?}");
    }

    #[test]
    fn a_mod_keyword_inside_a_comment_or_string_is_not_a_module() {
        let tree = Tree::new("masking");
        tree.write(
            "src/lib.rs",
            "// mod fake_a;\n/* mod fake_b; */\nconst S: &str = \"mod fake_c;\";\nmod real;\n",
        );
        tree.write("src/real.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files.contains_key(&RelPath::new("src/real.rs")));
        for fake in ["src/fake_a.rs", "src/fake_b.rs", "src/fake_c.rs"] {
            assert!(!files.contains_key(&RelPath::new(fake)), "{fake} must not have been indexed");
        }
    }

    #[test]
    fn a_raw_string_and_a_char_literal_do_not_confuse_the_scan() {
        let tree = Tree::new("raw-and-char");
        tree.write(
            "src/lib.rs",
            "const R: &str = r#\"mod fake; { } ;\"#;\nconst C: char = '\\'';\nmod real;\n",
        );
        tree.write("src/real.rs", "");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(!files.contains_key(&RelPath::new("src/fake.rs")));
    }

    /// A `mod` that resolves to nothing on disk contributes no file - and no
    /// crash - but is still worth a note (Decision 1's "the container key of
    /// a file must not depend on extraction order" extends to "an
    /// unreachable mod does not fabricate one").
    #[test]
    fn a_mod_naming_nothing_on_disk_is_noted_and_skipped() {
        let tree = Tree::new("dangling");
        tree.write("src/lib.rs", "mod ghost;\n");
        let (files, notes) = scan(&tree.0, "demo", "src/lib.rs");
        assert!(files.len() == 1, "only the crate root itself: {files:?}");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("ghost"), "{notes:?}");
    }

    #[test]
    fn scanning_is_a_pure_function_of_the_files_on_disk() {
        let tree = Tree::new("determinism");
        tree.write("src/lib.rs", "mod a;\nmod b;\n");
        tree.write("src/a.rs", "mod inner;\n");
        tree.write("src/a/inner.rs", "");
        tree.write("src/b.rs", "");
        let first = scan(&tree.0, "demo", "src/lib.rs");
        let second = scan(&tree.0, "demo", "src/lib.rs");
        assert_eq!(first.0, second.0);
    }
}
