//! `g_mesh_plugin_sdk::log_line!` writes each line in one `write(2)`, so a
//! plugin and the daemon sharing the daemon log never split each other's
//! lines. The tests are core's (`core/tests/log_line/shared.rs`), run here
//! against the SDK's own copy of the macro.
#![cfg(unix)]

use g_mesh_plugin_sdk::log_line;

include!("../../../core/tests/log_line/shared.rs");
