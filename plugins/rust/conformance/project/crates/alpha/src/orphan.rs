//! Deliberately unreferenced by any `mod` item in this crate - the fixture's
//! orphan file. It is still a real, walked `.rs` file (the SDK's own
//! `walk_project` finds it by extension, not by module reachability), so it
//! still gets a `File` node; `ProjectContext::container_for` answers
//! `Orphan { key: "orphan:crates/alpha/src/orphan.rs" }` for it rather than a
//! crate/module key.

pub fn unreachable_fn() {}
