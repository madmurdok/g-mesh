//! Front mode (D11 step 1 in `docs/architecture/lazy-indexing.md`, GM-399).
//!
//! What a daemon becomes when its root is a folder of projects
//! (`candidates::Mode::Multi`): it serves the candidate list and
//! `select_project` (`mcp::front`), and nothing else. No plugin discovery, no
//! `index.db`, no watcher, no activation - the whole of `daemon::run` after
//! the mode decision is skipped.
//!
//! To everything outside it, a front looks like any other daemon: it
//! publishes the same files in the same order (endpoint, pid file, phase
//! file, serving owner), so the shim's bootstrap, `g-mesh status`, `stop` and
//! `clean` need no special case to find, stop or sweep it. Its `index.phase`
//! reads `front`, which is the one thing that tells it apart.
//!
//! The session switch that follows a selection lives in the shim (slice 5):
//! the front only names the project, in the `select_project` result's
//! `_meta`.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::candidates::Detection;
use super::lifecycle::{self, CoreActivity, IdleTimeouts};
use super::manifest::DiscoveredPlugins;
use super::registry::PluginRegistry;
use super::{
    endpoint_in, phase_path_in, record_serving_owner, write_pid_file, write_state_file_atomic, PID_FILE,
};
use crate::embedding::EmbeddingPipeline;
use crate::ipc;
use crate::mcp;

/// What a front writes to `index.phase`.
pub const FRONT_PHASE: &str = "front";

/// How long a front with no connection stays up. Fixed rather than read from
/// `config.toml`: a front holds no index and no plugins, so there is nothing
/// worth keeping it warm for, and a folder has no project config of its own.
pub const FRONT_CORE_IDLE: Duration = Duration::from_secs(60);

/// Overrides [`FRONT_CORE_IDLE`] for the test suite, in milliseconds; `0`
/// turns the timer off. Same parsing as `lifecycle::CORE_IDLE_ENV`.
pub const FRONT_IDLE_ENV: &str = "G_MESH_FRONT_IDLE_MS";

fn front_core_idle() -> Option<Duration> {
    lifecycle::parse_timeout(std::env::var(FRONT_IDLE_ENV).ok().as_deref(), FRONT_CORE_IDLE, FRONT_IDLE_ENV)
}

/// Serves `root` as a front until it idles out or is orphaned. Called by
/// `daemon::run` right after the mode decision, holding the singleton lock.
pub(super) fn run(root: &Path, dir: &Path, singleton: File, detection: Detection) -> Result<()> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;

    // The normal daemon's publish sequence, in its order (`daemon::run`).
    let endpoint = endpoint_in(dir)
        .with_context(|| format!("failed to derive the daemon endpoint from {}", dir.display()))?;
    endpoint.clear_stale();
    let listener = ipc::Listener::bind(&endpoint)
        .with_context(|| format!("failed to bind the daemon endpoint at {endpoint}"))?;
    write_pid_file(&dir.join(PID_FILE), std::process::id());
    write_state_file_atomic(&phase_path_in(dir), &format!("{FRONT_PHASE}\n"), "phase file");
    record_serving_owner(dir);

    eprintln!(
        "g-mesh daemon: {} is a folder of {} projects - serving the front (no index)",
        canonical_root.display(),
        detection.candidates.len()
    );

    // `supervise` wants a registry; an empty one spawns nothing, and building
    // it from discovery would bring back the manifest dependency front mode
    // exists to avoid.
    let registry = PluginRegistry::new(
        &canonical_root,
        dir.to_path_buf(),
        DiscoveredPlugins::default(),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    );
    let core_activity = CoreActivity::new();
    let front = Arc::new(mcp::front::Front::new(canonical_root.clone(), &detection));

    let (accept_result, accept_loop) = mpsc::channel();
    {
        let core_activity = Arc::clone(&core_activity);
        thread::spawn(move || {
            let _ = accept_result.send(serve_forever(listener, front, core_activity));
        });
    }

    let timeouts = IdleTimeouts { plugin: None, core: front_core_idle() };
    let outcome =
        lifecycle::supervise(&canonical_root, dir, &registry, &core_activity, timeouts, accept_loop);

    drop(singleton);
    outcome
}

/// `daemon::serve_forever`'s shape, serving `mcp::front` instead.
fn serve_forever(
    listener: ipc::Listener,
    front: Arc<mcp::front::Front>,
    core_activity: Arc<CoreActivity>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to build the front's async runtime")?;

    runtime.block_on(async move {
        let mut listener =
            listener.into_async().context("failed to register the daemon endpoint with the async runtime")?;
        loop {
            let stream = listener.accept().await.context("failed to accept a daemon connection")?;
            let front = Arc::clone(&front);
            let attached = core_activity.connection_opened();
            let core_activity = Arc::clone(&core_activity);
            tokio::spawn(async move {
                let _attached = attached;
                if let Err(err) = mcp::front::serve_connection(stream, front, core_activity).await {
                    eprintln!("g-mesh daemon: front connection ended: {err:#}");
                }
            });
        }
    })
}
