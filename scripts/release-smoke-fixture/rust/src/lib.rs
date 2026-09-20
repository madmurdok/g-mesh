//! scripts/release-smoke.sh's Rust input - see
//! scripts/release-smoke-fixture/README.md for why this exists and why it is
//! shaped this way.

/// add and double mirror
/// plugins/typescript/conformance/project/src/math.ts: two functions in one
/// file, the second calling the first, so a real `g-mesh reindex` produces
/// both a node and a same-file CALLS edge for this language without needing
/// cross-file or cross-crate resolution.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn double(n: i32) -> i32 {
    add(n, n)
}
