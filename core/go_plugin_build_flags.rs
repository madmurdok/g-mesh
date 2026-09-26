// Included (with `include!`) by `core/build.rs` and by
// `core/tests/go_plugin_fingerprint.rs`, so the test builds the Go plugin
// with exactly the flags the build script does. `scripts/bundle-go-plugin.sh`
// passes the same flags to its own `go build`; that test checks it does.

/// Extra flags every `go build` of the Go plugin passes before `-o`.
///
/// `-buildvcs=false`: by default Go stamps the enclosing repository's
/// revision, commit time and "tree is dirty" bit into the binary. The index's
/// generation (`daemon::registry::indexer_version`) digests the plugin
/// directory's bytes, binary included, so a stamped binary changes with any
/// edit or commit anywhere in the checkout, and each rebuild then wipes indexes
/// an unchanged extractor built. Nothing in the plugin reads that stamp.
const GO_PLUGIN_BUILD_FLAGS: &[&str] = &["-buildvcs=false"];
