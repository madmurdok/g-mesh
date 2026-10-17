//! A file reaches the tree-sitter grammar its extension names, through core's
//! routing and the real TypeScript plugin: `.d.ts` and an uppercase `.TS`
//! parse as TypeScript, an uppercase `.TSX` as TSX.
//!
//! The parsed symbols tell the grammars apart. A type assertion
//! (`<number>value`) is TypeScript-only: TSX reads it as an unclosed JSX
//! element and loses every declaration after it. A JSX expression is
//! TSX-only: TypeScript loses the declaration after it.
//!
//! Controls: drop the extension's lowercasing in `RelPath::extension`
//! (`plugins/sdk`) -> the uppercase files parse with no grammar and their
//! symbols are missing; route `.ts` to `Grammar::Tsx` in
//! `extractor::grammar::GRAMMARS` -> the `.d.ts` and `.TS` rows lose their
//! symbols after the type assertion.

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

/// Ends with a function TSX cannot reach.
const TYPE_ASSERTION: &str =
    "declare const value: unknown;\nconst n = <number>value;\n\nexport function after_cast(): number {\n  return n;\n}\n";

/// Ends with a function TypeScript cannot reach.
const JSX: &str =
    "export const View = () => <div>{1}</div>;\n\nexport function after_jsx(): number {\n  return 1;\n}\n";

/// Declarations, then the type assertion. An initializer is not valid in a
/// declaration file; it is here only to tell the grammars apart.
const DECLARATIONS: &str = "export declare function declared_only(): number;\n\
     export interface Shape {\n  side: number;\n}\n\
     declare const value: unknown;\nexport const cast = <number>value;\n\
     export declare function after_cast(): number;\n";

/// The names of every non-file node in `file_path`, sorted.
fn symbols(harness: &Harness, file_path: &str) -> Vec<String> {
    harness.conn.with(|c| {
        let mut statement =
            c.prepare("SELECT name FROM nodes WHERE filePath = ?1 AND kind != 'File' ORDER BY name").unwrap();
        statement
            .query_map([file_path], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
}

fn routed(files: &[(&str, &str)]) -> Harness {
    let harness = Harness::new();
    for (path, contents) in files {
        harness.write(path, contents);
        harness.route(path);
    }
    harness
}

#[test]
fn a_declaration_file_parses_as_typescript() {
    let harness = routed(&[("types.d.ts", DECLARATIONS)]);
    assert_eq!(symbols(&harness, "types.d.ts"), ["Shape", "after_cast", "cast", "declared_only", "value"]);
}

#[test]
fn an_uppercase_ts_extension_parses_as_typescript() {
    let harness = routed(&[("Cast.TS", TYPE_ASSERTION)]);
    assert_eq!(symbols(&harness, "Cast.TS"), ["after_cast", "n", "value"]);
}

#[test]
fn an_uppercase_tsx_extension_parses_as_tsx() {
    let harness = routed(&[("View.TSX", JSX)]);
    assert_eq!(symbols(&harness, "View.TSX"), ["View", "after_jsx"]);
}
