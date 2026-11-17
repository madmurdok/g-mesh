//! Row shapes and identities shared by storage, graph, linking and every
//! reader: no queries, no dependency on the modules that read or write them.

pub mod kinds;
pub mod qualified_path;
pub mod records;

use sha2::{Digest, Sha256};

pub use records::{DeclarationRecord, Diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

/// The id of the container node for `key` in `language`.
///
/// Same family as every plugin-computed id (`node_id` in
/// plugins/sdk/src/ids.rs): sha256 over a type-marked string,
/// lowercase hex, truncated to 32 characters. The preimage is
/// `"container " + language + "\0" + key`:
///
///  - The `container ` marker is what keeps it disjoint from plugin ids, whose
///    preimages start `node ` (`nodeIdFor`) or `edge ` (`edgeIdFor`). Two ids
///    can then only coincide by a collision in 128 bits of sha256, not by any
///    choice of path, name or key.
///  - NUL, not a space, between language and key, because a key is free text
///    and may contain spaces (a C++ namespace never does, but nothing here
///    should rely on which languages exist). `language` is a plugin directory
///    name (`plugin_manifest` requires the two to match) and no filesystem
///    allows NUL in one, so the split is unambiguous.
///
/// A plugin that ever needs to address a container node directly computes the
/// same string; the design doc's "plugins and core compute the same id" is this
/// function.
pub fn container_id(language: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("container {language}\0{key}").as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}
