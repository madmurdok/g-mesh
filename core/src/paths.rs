//! Where g-mesh keeps what it owns on the machine it runs on.
//!
//! One function, because two of them would eventually disagree: an index
//! written under one root and a config read from another is a project that
//! cannot see its own settings.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Points g-mesh's state somewhere other than `~/.g-mesh`.
///
/// The same shape as [`crate::embedding::model::MODEL_DIR_ENV`], and for the
/// same reason: the test suite has to be able to run without writing into the
/// developer's own state. Before this existed, a `cargo test` run left ~350
/// project directories a day under the real `~/.g-mesh/projects/`, and one
/// `cli_init` test failed against state an unrelated benchmark run was
/// mutating at the same time.
pub const HOME_ENV: &str = "G_MESH_HOME";

/// `~/.g-mesh`, or `$G_MESH_HOME` if set.
///
/// Covers the *state* g-mesh writes: `projects/<hash>/` (index, config,
/// socket, pid files) and the global `config.toml`. Two things under
/// `~/.g-mesh` deliberately do not move with it:
///
/// - `models/` - a ~612 MiB per-machine cache of immutable weights, not
///   per-run state. Pointing it at a test root would mean re-downloading it
///   rather than reusing what is already on disk, which is why it keeps its
///   own, older `G_MESH_MODEL_DIR` override
///   ([`crate::embedding::model::default_model_dir`]).
/// - `bin/` - written by `scripts/install.sh`, which this binary never reads.
///   `G_MESH_HOME` moves where g-mesh *works*, not where it is installed.
pub fn g_mesh_home() -> Result<PathBuf> {
    home_from(std::env::var_os(HOME_ENV))
}

/// The resolution itself, with the override passed in rather than read.
///
/// So that the tests below can exercise both branches without calling
/// `set_var` on a variable every other test in the same binary is
/// simultaneously reading - which is the exact class of shared-mutable-state
/// bug this module exists to end.
fn home_from(override_dir: Option<OsString>) -> Result<PathBuf> {
    // An empty value reads as "not set", not as "the empty path". Nothing ever
    // means to put the state root at a relative nowhere, and the failure it
    // produces is quiet: every project directory would resolve under the
    // current working directory, differently for each process that asked.
    //
    // Not hypothetical - a CI expression that yields `''` on the platforms it
    // does not apply to would set exactly this, and cargo's `force = false`
    // treats an empty-but-present variable as already set, so `.cargo/config
    // .toml`'s own value would not fill the gap either.
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".g-mesh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_home_is_dot_g_mesh_in_the_users_home_directory() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(home_from(None).unwrap(), home.join(".g-mesh"));
    }

    /// An empty override is the same as none: a state root at the empty path
    /// would resolve every project directory relative to the current working
    /// directory, which differs per process and fails silently.
    #[test]
    fn an_empty_override_falls_back_to_the_default_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(home_from(Some(OsString::new())).unwrap(), home.join(".g-mesh"));
    }

    /// Whatever the override names is taken verbatim - no `.g-mesh` appended,
    /// so a test root reads as the directory it was pointed at.
    #[test]
    fn the_override_is_used_exactly_as_given() {
        let dir = OsString::from("/tmp/g-mesh-home-override");
        assert_eq!(home_from(Some(dir)).unwrap(), PathBuf::from("/tmp/g-mesh-home-override"));
    }

    /// The override is what the process was started with, not something a
    /// caller may pass alongside it: `g_mesh_home` and the private resolution
    /// it delegates to must not drift apart.
    #[test]
    fn the_public_entry_point_resolves_the_environment_it_was_given() {
        assert_eq!(g_mesh_home().unwrap(), home_from(std::env::var_os(HOME_ENV)).unwrap());
    }
}
