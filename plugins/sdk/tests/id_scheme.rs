//! The id scheme is a cross-plugin contract, and this is where it is held to
//! one.
//!
//! `ids`' module doc specifies the scheme in bytes. That specification is
//! worth nothing unless the two implementations that exist agree on it, so
//! every case below pins this crate's answer against the id
//! `plugins/typescript`'s own `nodeIdFor`/`edgeIdFor`
//! (`plugins/typescript/src/extract.ts`) produced for the same tuple, on
//! 2026-09-16, by calling those functions directly on the built plugin:
//!
//! ```text
//! node -e 'const e = require("./plugins/typescript/dist/src/extract.js");
//!          console.log(e.nodeIdFor("src/overloads.ts", "Function", "parse", "function"))'
//! ```
//!
//! The values are *recorded*, not recomputed by this test. Recomputing would
//! make the test pass whenever both implementations change together, which is
//! exactly the failure it exists to catch - and would make the Rust test
//! suite depend on a Node toolchain and a built TS plugin.
//!
//! The first case is independently corroborated: it is the very id in
//! `wire`'s own `OVERLOADED_NODE_LINE` constant, a line copied out of the TS
//! plugin's real `--bulk-index` output long before this crate existed.
//!
//! **`plugins/go` must produce these same ids.** The Go plugin is being built
//! in parallel and is not on this branch; when it lands, this table is the
//! one it has to reproduce - `crypto/sha256` over the same NUL-joined fields,
//! hex, first 32 characters.

use g_mesh_plugin_sdk::ids::{edge_id, node_id};
use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};

/// `(filePath, kind, qualifiedName, nativeKind) -> id`, as the TS plugin
/// computes it.
const NODE_CASES: &[(&str, NodeKind, &str, Option<&str>, &str)] = &[
    // An overloaded function - the case `wire`'s own OVERLOADED_NODE_LINE
    // carries, so this id has two independent provenances.
    ("src/overloads.ts", NodeKind::Function, "parse", Some("function"), "5ff9a3373000bb2f00e38ba616f6cd46"),
    // A File node: its qualifiedName is its own path, which is the one place
    // the same string appears in two fields.
    ("src/a.ts", NodeKind::File, "src/a.ts", Some("file"), "9f60f2ec6d4d54f6552208bf273574f6"),
    // The two placeholder shapes, whose nativeKind is what keeps them apart
    // from an ordinary Module node with the same qualifiedName.
    (
        "src/a.ts",
        NodeKind::Module,
        "src/b.ts#helper",
        Some("pending_symbol"),
        "5314e55c6fd3d950a54751b0bcabebc4",
    ),
    ("src/a.ts", NodeKind::Module, "./b", Some("resolved_module"), "a8170b6e3a96d182421067296ebcf7a8"),
    // Non-ASCII in both the path and the name: the scheme hashes UTF-8 bytes,
    // so this only agrees if both sides encode the same way.
    ("pkg/naïve.ts", NodeKind::Type, "Naïve", Some("interface"), "9744a9154b9c5d68cacf97ef72034846"),
    // No nativeKind at all - the `?? ""` / `unwrap_or("")` case.
    ("p", NodeKind::File, "q", None, "d0b5b9f1750a7aae60a7d88b4b17bd3c"),
    // Every string field empty: proves the separators are still written, and
    // that neither side collapses an empty field away.
    ("", NodeKind::Variable, "", None, "d61c3c4fe145ae4017786a96a2eadc2d"),
];

/// `(fromId, kind, toId, toDeclaration) -> id`.
const EDGE_CASES: &[(&str, EdgeKind, &str, Option<u32>, &str)] = &[
    ("F", EdgeKind::Calls, "T", None, "c1b2532a8760a618eadf8d35abdb11c2"),
    // Ordinal 0 is a binding, not an absence - and must differ from both the
    // unbound edge above and any other ordinal.
    ("F", EdgeKind::Calls, "T", Some(0), "aac4e5520b0f9e700249fcd5274f802c"),
    ("F", EdgeKind::Calls, "T", Some(2), "e35a265880a84bfd8417897086c90b3a"),
    // The kinds whose wire spelling is not their Rust variant name.
    ("aaa", EdgeKind::Defines, "bbb", None, "48cf092ed45b7763e8a0547309a60e3b"),
    ("aaa", EdgeKind::SupertypeOf, "bbb", None, "a1890793893b0d7ce5efbce251ffae06"),
    ("aaa", EdgeKind::References, "bbb", None, "43b1a6790755e2df8b5286a8b743bbc3"),
];

#[test]
fn node_ids_match_the_typescript_plugins() {
    for (file_path, kind, qualified_name, native_kind, expected) in NODE_CASES {
        assert_eq!(
            &node_id(file_path, *kind, qualified_name, *native_kind),
            expected,
            "node id for ({file_path:?}, {kind:?}, {qualified_name:?}, {native_kind:?}) diverged from \
             the TypeScript plugin's - the id scheme is a cross-plugin contract, see `ids`' module doc"
        );
    }
}

#[test]
fn edge_ids_match_the_typescript_plugins() {
    for (from_id, kind, to_id, to_declaration, expected) in EDGE_CASES {
        assert_eq!(
            &edge_id(from_id, *kind, to_id, *to_declaration),
            expected,
            "edge id for ({from_id:?}, {kind:?}, {to_id:?}, {to_declaration:?}) diverged from the \
             TypeScript plugin's - the id scheme is a cross-plugin contract, see `ids`' module doc"
        );
    }
}
