//! `debug-embed-eval churn`: how many embedded texts a synthetic edit
//! invalidates, per variant (`docs/architecture/gm-455-structural-context.md`,
//! "Churn"). Every instance of every edit is applied to an in-memory copy
//! of the snapshot's nodes, and all texts are rebuilt with
//! `context::embed_texts`, the builder `run` embeds. The count is the
//! production cache's misses: distinct text hashes after the edit that were
//! not among the hashes before it (`embedding/cache.rs` keys by
//! `sha256(text)`).
//!
//! No builder input reads edges, so an added call (E1) is modelled only as
//! the body growing by one line. Nodes that are neither embedded, a
//! Function nor a Type are dropped before the edits: the builder reads no
//! other node, and no edit targets one (E5 still lists every file).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::config::{ContextForm, CorporaFile, TextForm, VariantsFile};
use super::context::{self, ParentRef};
use super::{load_nodes, snapshot_path, verified_snapshot, EvalDir, Node};
use crate::embedding::text::full_text;

#[derive(Debug, Args)]
pub struct ChurnArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// Variants to count (their `text` and `context`); repeat for several.
    #[arg(long, required = true)]
    pub variant: Vec<String>,
    #[arg(long, default_value = "g-mesh")]
    pub corpus: String,
    /// Read this index instead of the corpus's verified snapshot, e.g. a
    /// scratch re-index for the real-edit control.
    #[arg(long)]
    pub index_db: Option<PathBuf>,
    /// Only this edit.
    #[arg(long)]
    pub edit: Option<Edit>,
    /// Only the instances whose target has this qualifiedName (e1-e4) or
    /// file path (e5); each instance's count is printed.
    #[arg(long)]
    pub target: Option<String>,
    /// Write `<dir>/<variant>.tsv`: `id<TAB>sha256(text)` of every embedded
    /// text of the unedited index, by id.
    #[arg(long)]
    pub dump: Option<PathBuf>,
    /// Also write the distributions as JSON here.
    #[arg(long)]
    pub json: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Edit {
    /// A body edit that adds one call, on every Function.
    E1,
    /// Rename a function, on every Function.
    E2,
    /// Add a method to a type, on every Type with members.
    E3,
    /// Rename a type, on every Type with members.
    E4,
    /// Rename a file, on every file.
    E5,
}

impl Edit {
    const ALL: [Edit; 5] = [Edit::E1, Edit::E2, Edit::E3, Edit::E4, Edit::E5];

    fn label(self) -> &'static str {
        match self {
            Edit::E1 => "E1 body edit adding a call",
            Edit::E2 => "E2 rename a function",
            Edit::E3 => "E3 add a method to a type",
            Edit::E4 => "E4 rename a type",
            Edit::E5 => "E5 rename a file",
        }
    }
}

/// What an edit instance applies to.
#[derive(Debug, Clone)]
pub enum Target {
    Node(usize),
    /// A type and the separator its members' qualifiedNames use after it.
    Type(usize, String),
    File(String),
}

/// Every instance of every edit in one snapshot.
pub struct Plan {
    functions: Vec<usize>,
    types: Vec<(usize, String)>,
    files: Vec<String>,
}

impl Plan {
    pub fn new(nodes: &[Node], files: BTreeSet<String>) -> Self {
        let functions = (0..nodes.len()).filter(|&i| nodes[i].kind == "Function").collect();
        let index = context::type_index(nodes);
        let mut types: BTreeMap<usize, String> = BTreeMap::new();
        for i in 0..nodes.len() {
            if let Some(ParentRef::Type(t)) = context::parent_ref(nodes, &index, i) {
                let (m, owner) = (&nodes[i], &nodes[t].qualified_name);
                let sep = &m.qualified_name[owner.len()..m.qualified_name.len() - m.name.len()];
                types.entry(t).or_insert_with(|| sep.to_string());
            }
        }
        Plan { functions, types: types.into_iter().collect(), files: files.into_iter().collect() }
    }

    pub fn targets(&self, edit: Edit) -> Vec<Target> {
        match edit {
            Edit::E1 | Edit::E2 => self.functions.iter().map(|&i| Target::Node(i)).collect(),
            Edit::E3 | Edit::E4 => self.types.iter().map(|(t, sep)| Target::Type(*t, sep.clone())).collect(),
            Edit::E5 => self.files.iter().cloned().map(Target::File).collect(),
        }
    }
}

fn target_name<'a>(nodes: &'a [Node], target: &'a Target) -> &'a str {
    match target {
        Target::Node(i) | Target::Type(i, _) => &nodes[*i].qualified_name,
        Target::File(f) => f,
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `text` with every whole-identifier occurrence of `old` replaced.
pub fn replace_ident(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(pos) = rest.find(old) {
        let before = rest[..pos].chars().next_back().or(prev);
        let after = rest[pos + old.len()..].chars().next();
        out.push_str(&rest[..pos]);
        if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
            out.push_str(old);
        } else {
            out.push_str(new);
        }
        prev = old.chars().next_back();
        rest = &rest[pos + old.len()..];
    }
    out.push_str(rest);
    out
}

fn set_signature(n: &mut Node, signature: Option<String>) {
    n.text = full_text(n.doc.as_deref(), signature.as_deref());
    n.signature = signature;
}

/// Lines after `line` in `file` move down by one; nodes spanning it grow.
fn insert_line(nodes: &mut [Node], file: &str, line: i64) {
    for n in nodes.iter_mut().filter(|n| n.file_path == file) {
        if n.start_line > line {
            n.start_line += 1;
        }
        if n.end_line >= line {
            n.end_line += 1;
        }
    }
}

/// `qualified_name` with its `name` suffix replaced by `new`.
fn renamed_qn(qualified_name: &str, name: &str, new: &str) -> String {
    match qualified_name.strip_suffix(name) {
        Some(owner) => format!("{owner}{new}"),
        None => qualified_name.to_string(),
    }
}

/// `qn` re-rooted from `old` to `new` when it is a member path under `old`.
fn reroot(qn: &str, old: &str, new: &str) -> Option<String> {
    let rest = qn.strip_prefix(old)?;
    ["::", ".", "#"].iter().any(|sep| rest.starts_with(sep)).then(|| format!("{new}{rest}"))
}

fn added_signature(language: &str, type_name: &str) -> String {
    match language {
        "rust" => "fn added_by_churn(&self)".to_string(),
        "python" => "def added_by_churn(self)".to_string(),
        "go" => format!("func (r *{type_name}) added_by_churn()"),
        _ => "added_by_churn(): void".to_string(),
    }
}

/// The snapshot's nodes after one edit instance.
pub fn apply(nodes: &[Node], edit: Edit, target: &Target) -> Vec<Node> {
    let mut out = nodes.to_vec();
    match (edit, target) {
        (Edit::E1, Target::Node(f)) => {
            let f = &nodes[*f];
            insert_line(&mut out, &f.file_path, f.end_line);
        }
        (Edit::E2, Target::Node(f)) => {
            let f = &nodes[*f];
            let new = format!("{}_renamed", f.name);
            let new_qn = renamed_qn(&f.qualified_name, &f.name, &new);
            for n in out.iter_mut() {
                if n.id == f.id {
                    n.name = new.clone();
                    n.qualified_name = new_qn.clone();
                    let sig = n.signature.as_deref().map(|s| replace_ident(s, &f.name, &new));
                    set_signature(n, sig);
                } else if n.file_path == f.file_path && n.language == f.language {
                    if let Some(qn) = reroot(&n.qualified_name, &f.qualified_name, &new_qn) {
                        n.qualified_name = qn;
                    }
                }
            }
        }
        (Edit::E3, Target::Type(t, sep)) => {
            let t = &nodes[*t];
            let name = "added_by_churn".to_string();
            let signature = added_signature(&t.language, &t.name);
            insert_line(&mut out, &t.file_path, t.end_line);
            out.push(Node {
                id: format!("{}-added-by-churn", t.id),
                kind: "Function".into(),
                qualified_name: format!("{}{sep}{name}", t.qualified_name),
                name,
                file_path: t.file_path.clone(),
                language: t.language.clone(),
                native_kind: Some("method".into()),
                container: t.container.clone(),
                start_line: t.end_line + 1,
                end_line: t.end_line + 1,
                text: full_text(None, Some(&signature)),
                doc: None,
                signature: Some(signature),
            });
        }
        (Edit::E4, Target::Type(t, _)) => {
            // Members are re-rooted in the type's file, or across its
            // module when it has one (Go methods live in other files of the
            // package). Signatures naming the type are rewritten across the
            // language: without reference resolution a same-named type
            // elsewhere is renamed too.
            let t = &nodes[*t];
            let (old, new) = (t.name.as_str(), format!("{}Renamed", t.name));
            let new_qn = renamed_qn(&t.qualified_name, old, &new);
            let same_scope = |n: &Node| {
                n.file_path == t.file_path
                    || t.container
                        .as_deref()
                        .is_some_and(|c| !c.is_empty() && n.container.as_deref() == Some(c))
            };
            let impl_patterns =
                [(format!("<{old} as "), format!("<{new} as ")), (format!("<{old}<"), format!("<{new}<"))];
            for n in out.iter_mut().filter(|n| n.language == t.language) {
                if n.id == t.id {
                    n.name = new.clone();
                    n.qualified_name = new_qn.clone();
                } else if same_scope(n) {
                    if let Some(qn) = reroot(&n.qualified_name, &t.qualified_name, &new_qn) {
                        n.qualified_name = qn;
                    }
                }
                if n.language == "rust" {
                    for (from, to) in &impl_patterns {
                        n.qualified_name = n.qualified_name.replace(from.as_str(), to);
                        n.name = n.name.replace(from.as_str(), to);
                    }
                }
                if let Some(sig) = n.signature.as_deref() {
                    let replaced = replace_ident(sig, old, &new);
                    if replaced != sig {
                        set_signature(n, Some(replaced));
                    }
                }
            }
        }
        (Edit::E5, Target::File(file)) => {
            let (dir, base) = file.rsplit_once('/').map_or(("", file.as_str()), |(d, b)| (d, b));
            let (stem, ext) = base.rsplit_once('.').map_or((base, ""), |(s, e)| (s, e));
            let new_stem = format!("{stem}_renamed");
            let new_base = if ext.is_empty() { new_stem.clone() } else { format!("{new_stem}.{ext}") };
            let new_path = if dir.is_empty() { new_base } else { format!("{dir}/{new_base}") };
            // A Rust qualifiedName starts with its file's module; a module
            // path (container) ends with it.
            for n in out.iter_mut().filter(|n| &n.file_path == file) {
                n.file_path = new_path.clone();
                if n.language == "rust" {
                    if n.qualified_name == stem {
                        n.qualified_name = new_stem.clone();
                    } else if let Some(rest) = n.qualified_name.strip_prefix(&format!("{stem}::")) {
                        n.qualified_name = format!("{new_stem}::{rest}");
                    }
                }
                if let Some(c) = n.container.as_mut() {
                    for sep in ["::", "."] {
                        if let Some(parent) = c.strip_suffix(&format!("{sep}{stem}")) {
                            *c = format!("{parent}{sep}{new_stem}");
                            break;
                        }
                    }
                }
            }
        }
        (edit, target) => unreachable!("{edit:?} does not apply to {target:?}"),
    }
    out
}

fn hashes(texts: &[Option<String>]) -> HashSet<[u8; 32]> {
    texts.iter().flatten().map(|t| Sha256::digest(t.as_bytes()).into()).collect()
}

/// Cache misses: distinct text hashes in `after` absent from `before`.
/// `base` is the unedited texts, whose hashes are all in `before`: an
/// edited text equal to the one at its index is not hashed.
pub fn misses(before: &HashSet<[u8; 32]>, base: &[Option<String>], after: &[Option<String>]) -> usize {
    let changed: HashSet<[u8; 32]> = after
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.as_ref().filter(|t| base.get(i).and_then(Option::as_ref) != Some(*t)))
        .map(|t| Sha256::digest(t.as_bytes()).into())
        .collect();
    changed.difference(before).count()
}

/// One instance's churn under one variant's form and context.
#[allow(clippy::too_many_arguments)]
pub fn instance_churn(
    nodes: &[Node],
    before: &HashSet<[u8; 32]>,
    base: &[Option<String>],
    form: TextForm,
    context: ContextForm,
    arm_seed: u64,
    edit: Edit,
    target: &Target,
) -> usize {
    let edited = apply(nodes, edit, target);
    misses(before, base, &context::embed_texts(&edited, form, context, arm_seed))
}

/// `f` over `items` on up to half the machine's cores, in order.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> =
            items.chunks(chunk).map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<_>>())).collect();
        handles.into_iter().flat_map(|h| h.join().expect("churn worker panicked")).collect()
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub instances: usize,
    pub mean: f64,
    pub p50: usize,
    pub p90: usize,
    pub max: usize,
}

/// Nearest-rank percentiles.
pub fn stats(counts: &[usize]) -> Stats {
    let mut v = counts.to_vec();
    v.sort_unstable();
    let n = v.len();
    let rank = |q: f64| if n == 0 { 0 } else { v[((q * n as f64).ceil() as usize).clamp(1, n) - 1] };
    Stats {
        instances: n,
        mean: if n == 0 { 0.0 } else { v.iter().sum::<usize>() as f64 / n as f64 },
        p50: rank(0.5),
        p90: rank(0.9),
        max: v.last().copied().unwrap_or(0),
    }
}

pub fn run(args: &ChurnArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let variants = VariantsFile::load(eval_dir)?;
    let db = match &args.index_db {
        Some(db) => db.clone(),
        None => {
            verified_snapshot(eval_dir, &CorporaFile::load(eval_dir)?, &args.corpus)?;
            snapshot_path(eval_dir, &args.corpus)
        }
    };
    let all = load_nodes(&db)?;
    let files: BTreeSet<String> = all.iter().map(|n| n.file_path.clone()).collect();
    let nodes: Vec<Node> =
        all.into_iter().filter(|n| n.text.is_some() || n.kind == "Function" || n.kind == "Type").collect();
    let seed = variants.settings.arm_seed;
    let plan = Plan::new(&nodes, files);
    let edits: Vec<Edit> = args.edit.map_or(Edit::ALL.to_vec(), |e| vec![e]);

    let mut rows: BTreeMap<Edit, Vec<Stats>> = BTreeMap::new();
    let mut json_out = serde_json::Map::new();
    for name in &args.variant {
        let variant = variants.get(name)?;
        let started = Instant::now();
        let texts = context::embed_texts(&nodes, variant.text, variant.context, seed);
        if let Some(dir) = &args.dump {
            std::fs::create_dir_all(dir)?;
            let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join(format!("{name}.tsv")))?);
            for (n, t) in nodes.iter().zip(&texts) {
                if let Some(t) = t {
                    writeln!(out, "{}\t{}", n.id, super::hex(&Sha256::digest(t.as_bytes())))?;
                }
            }
            out.flush()?;
        }
        if variant.context.shuffled() {
            let real = context::headers(&nodes, &texts, ContextForm::PathParent, seed);
            let shuffled = context::headers(&nodes, &texts, variant.context, seed);
            let with = real.iter().filter(|h| h.len() > 1).count();
            let kept =
                real.iter().zip(&shuffled).filter(|(r, s)| r.len() > 1 && r.get(1) == s.get(1)).count();
            println!("{name}: {kept} of {with} parent lines equal the node's own after the derangement");
        }
        let before = hashes(&texts);
        let mut per_edit = serde_json::Map::new();
        for &edit in &edits {
            let targets: Vec<Target> = plan
                .targets(edit)
                .into_iter()
                .filter(|t| args.target.as_deref().is_none_or(|want| target_name(&nodes, t) == want))
                .collect();
            let counts = par_map(&targets, |t| {
                instance_churn(&nodes, &before, &texts, variant.text, variant.context, seed, edit, t)
            });
            if args.target.is_some() {
                for (t, c) in targets.iter().zip(&counts) {
                    println!("{name} {edit:?} {}: {c}", target_name(&nodes, t));
                }
            }
            let s = stats(&counts);
            per_edit.insert(
                format!("{edit:?}"),
                json!({"instances": s.instances, "mean": s.mean, "p50": s.p50, "p90": s.p90, "max": s.max, "counts": counts}),
            );
            rows.entry(edit).or_default().push(s);
        }
        json_out.insert(name.clone(), serde_json::Value::Object(per_edit));
        eprintln!("{name}: churn over {} nodes in {:.1} s", nodes.len(), started.elapsed().as_secs_f64());
    }

    println!("\ncells: mean / p50 / p90 / max cache misses per instance\n");
    println!("| edit | instances | {} |", args.variant.join(" | "));
    println!("|---|---|{}", "---|".repeat(args.variant.len()));
    for (edit, stats) in &rows {
        let cells: Vec<String> =
            stats.iter().map(|s| format!("{:.2} / {} / {} / {}", s.mean, s.p50, s.p90, s.max)).collect();
        println!("| {} | {} | {} |", edit.label(), stats[0].instances, cells.join(" | "));
    }
    if let Some(path) = &args.json {
        super::write_json(path, &serde_json::Value::Object(json_out))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::embed_eval::context::test_node as node;

    fn fixture() -> Vec<Node> {
        vec![
            node("1", "Type", "struct", "a::Foo", "src/a.rs", "rust", Some("pub struct Foo")),
            node("2", "Function", "method", "a::Foo::new", "src/a.rs", "rust", Some("fn new() -> Foo")),
            node(
                "3",
                "Function",
                "trait_impl_method",
                "a::<Foo as Default>::default",
                "src/a.rs",
                "rust",
                Some("fn default() -> Self"),
            ),
            node("4", "Function", "function", "a::bar", "src/a.rs", "rust", Some("fn bar(f: &Foo)")),
            node("5", "Function", "function", "b::baz", "src/b.rs", "rust", Some("fn baz(x: FooBar)")),
        ]
    }

    fn churn(context: ContextForm, edit: Edit, name: &str) -> usize {
        let nodes = fixture();
        let plan = Plan::new(&nodes, nodes.iter().map(|n| n.file_path.clone()).collect());
        let target = plan.targets(edit).into_iter().find(|t| target_name(&nodes, t) == name).unwrap();
        let base = context::embed_texts(&nodes, TextForm::FirstParagraph, context, 1);
        instance_churn(&nodes, &hashes(&base), &base, TextForm::FirstParagraph, context, 1, edit, &target)
    }

    /// Per edit and arm on a five-node fixture. Controls: making `misses`
    /// return 0 fails every non-zero row; dropping E4's `<X as T>` rewrite
    /// leaves `default`'s parent line unchanged (parent E4 3, not 4);
    /// dropping E5's `file_path` rewrite gives path E5 0, not 4;
    /// `replace_ident` matching substrings renames `FooBar` (E4 none 4).
    #[test]
    fn churn_per_edit_on_a_tiny_fixture() {
        use ContextForm::{None as Plain, Parent, Path};
        for ctx in [Plain, Path, Parent] {
            for f in ["a::Foo::new", "a::bar", "b::baz", "a::<Foo as Default>::default"] {
                assert_eq!(churn(ctx, Edit::E1, f), 0, "{ctx:?} E1 {f}");
            }
            assert_eq!(churn(ctx, Edit::E2, "a::bar"), 1, "{ctx:?} E2");
            assert_eq!(churn(ctx, Edit::E3, "a::Foo"), 1, "{ctx:?} E3");
        }
        // Foo's own signature, `new` and `bar` name it; `default` says Self.
        assert_eq!(churn(Plain, Edit::E4, "a::Foo"), 3);
        assert_eq!(churn(Path, Edit::E4, "a::Foo"), 3);
        assert_eq!(churn(Parent, Edit::E4, "a::Foo"), 4, "+ default's impl line");
        assert_eq!(churn(Plain, Edit::E5, "src/a.rs"), 0);
        assert_eq!(churn(Parent, Edit::E5, "src/a.rs"), 0);
        assert_eq!(churn(Path, Edit::E5, "src/a.rs"), 4);
    }

    #[test]
    fn only_whole_identifiers_are_renamed() {
        assert_eq!(replace_ident("fn f(a: Foo, b: FooBar) -> Foo", "Foo", "X"), "fn f(a: X, b: FooBar) -> X");
        assert_eq!(replace_ident("Foo<Foo>", "Foo", "X"), "X<X>");
        assert_eq!(replace_ident("_Foo", "Foo", "X"), "_Foo");
    }

    /// Control: `rank` flooring instead of `ceil` gives p50 3 and p90 6.
    #[test]
    fn nearest_rank_percentiles() {
        let s = stats(&(1..=7).collect::<Vec<_>>());
        assert_eq!((s.p50, s.p90, s.max), (4, 7, 7));
        assert!((s.mean - 4.0).abs() < 1e-12);
    }
}
