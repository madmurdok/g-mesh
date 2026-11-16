//! Control-plane operations on a project's state: stopping its daemon and plugins,
//! deleting orphaned state, and the shim's startup sweep. Shared by the CLI commands,
//! the daemon's lifecycle and the shim.

pub mod lock;
pub mod purge;
pub mod stop;
pub mod sweep;

#[cfg(test)]
pub(crate) mod fixture_tests;
