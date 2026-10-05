//! Which tree-sitter grammar reads which extension: `typescript` for
//! `.ts/.mts/.cts`, `tsx` for `.tsx`, `javascript` for `.js/.jsx/.mjs/.cjs`
//! (the routing is section 1.2 of
//! `docs/architecture/gm-324-typescript-rust-port.md`). Matching is
//! case-insensitive. The wire `language` is `typescript` for every
//! extension, whichever grammar parsed it.

use std::cell::RefCell;

use g_mesh_plugin_sdk::RelPath;

/// One of the three grammars this plugin links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grammar {
    TypeScript,
    Tsx,
    JavaScript,
}

impl Grammar {
    /// The grammar's name, as `plugin.toml`'s `[plugin.grammars]` table keys it.
    pub fn name(self) -> &'static str {
        match self {
            Grammar::TypeScript => "typescript",
            Grammar::Tsx => "tsx",
            Grammar::JavaScript => "javascript",
        }
    }

    /// The tree-sitter language this grammar parses with.
    pub fn language(self) -> tree_sitter::Language {
        match self {
            Grammar::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Grammar::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Grammar::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    fn index(self) -> usize {
        match self {
            Grammar::TypeScript => 0,
            Grammar::Tsx => 1,
            Grammar::JavaScript => 2,
        }
    }
}

/// Every grammar and the extensions it owns, lowercased with their dot. Each
/// extension appears exactly once.
pub const GRAMMARS: [(Grammar, &[&str]); 3] = [
    (Grammar::TypeScript, &[".ts", ".mts", ".cts"]),
    (Grammar::Tsx, &[".tsx"]),
    (Grammar::JavaScript, &[".js", ".jsx", ".mjs", ".cjs"]),
];

/// Every extension this plugin claims, in `plugin.toml`'s
/// `[plugin.languages] extensions` order. Must equal the union of
/// [`GRAMMARS`].
pub const EXTENSIONS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

/// The grammar that parses `path`, or `None` for an extension this plugin
/// does not own.
pub fn grammar_for(path: &RelPath) -> Option<Grammar> {
    let extension = path.extension()?;
    GRAMMARS
        .iter()
        .find(|(_, extensions)| extensions.contains(&extension.as_str()))
        .map(|(grammar, _)| *grammar)
}

thread_local! {
    /// One parser per grammar per thread, each built on first use.
    /// `thread_local` because `Extractor::extract` takes `&self` and the
    /// trait requires `Sync`.
    static PARSERS: RefCell<[Option<tree_sitter::Parser>; 3]> = const { RefCell::new([None, None, None]) };
}

/// The grammars reject a raw NUL even inside a literal, where tsc accepts it
/// (a common field separator), and the parse around it degrades into errors
/// that lose the declarations and uses after it. U+0001 stands in for it:
/// one byte like NUL, so every position stays put; inside a literal the
/// grammars accept it, and outside one it is an error exactly as NUL is. The
/// tree is parsed from the stand-in text but every name is read from the real
/// source, so names keep their NULs.
const NUL: char = '\0';
const NUL_STAND_IN: &str = "\u{1}";

/// Parses `source` with `grammar`. `None` only when tree-sitter gives up,
/// which needs a cancellation flag or a timeout this plugin never sets.
pub fn parse(grammar: Grammar, source: &str) -> Option<tree_sitter::Tree> {
    if source.contains(NUL) {
        return parse_text(grammar, &source.replace(NUL, NUL_STAND_IN));
    }
    parse_text(grammar, source)
}

fn parse_text(grammar: Grammar, source: &str) -> Option<tree_sitter::Tree> {
    PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let parser = parsers[grammar.index()].get_or_insert_with(|| {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&grammar.language())
                .expect("a bundled grammar must match the bundled tree-sitter ABI");
            parser
        });
        parser.parse(source, None)
    })
}
