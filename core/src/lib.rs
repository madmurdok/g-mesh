pub mod cli;
pub mod config;
pub mod control;
pub mod daemon;
pub mod embedding;
pub mod fsutil;
pub mod gc;
pub mod graph;
pub mod index_health;
pub mod indexing_status;
pub mod ingest;
pub mod ipc;
pub mod knobs;
pub mod languages;
pub mod link;
pub mod log;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod plugin_manifest;
pub mod process;
pub mod project_detect;
pub mod project_walk;
pub mod protocol;
pub mod shim;
pub mod storage;
/// A fake, protocol-speaking plugin for unit tests that need two languages.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub mod test_plugin;
pub mod watcher;
