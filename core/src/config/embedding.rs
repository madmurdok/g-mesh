//! The embedding pipeline's settings: the project's `[embedding]` section,
//! the global config's `[embeddingCache]` and `[rerank]` sections, and the
//! environment variables that override them, resolved into the plain values
//! [`crate::embedding`] takes.
//!
//! Precedence, per part: an environment switch set to `off` wins over the
//! file; the file's `enabled` wins over the default (on); a global config
//! that cannot be read gives the defaults and one line naming the part that
//! needed it. The file is read at most once per resolution, and not at all
//! when both parts are switched off by the environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{read_global_config, EmbeddingConfig, GlobalConfig};
use crate::embedding::cache::{self, CACHE_ENV};
use crate::embedding::model::{default_model_dir_from, InferenceOptions};
use crate::embedding::rerank::{rerank_model_dir_from, RERANK_ENV};
use crate::embedding::{CacheSettings, EmbeddingSettings, ModelSettings, RerankSettings};

/// Everything `EmbeddingPipeline::load` needs for a project configured with
/// `config`, from the process environment and the global config.
pub fn embedding_settings(config: &EmbeddingConfig) -> EmbeddingSettings {
    let switches = resolve(env_lookup, true, read_global_config, |line| crate::log_line!("{line}"));
    let cache = switches.cache_max_size_mb.and_then(|max_size_mb| match cache::default_path() {
        Ok(path) => Some(CacheSettings::new(path, max_size_mb)),
        Err(err) => {
            crate::log_line!("g-mesh: the embedding cache has no location ({err:#}) - embedding without it");
            None
        }
    });
    EmbeddingSettings {
        model: ModelSettings {
            name: config.model.clone(),
            dir_override: model_dir_override(),
            inference: inference_options(),
        },
        cache,
        rerank: RerankSettings { enabled: switches.rerank, model_dir: rerank_model_dir() },
    }
}

/// The rerank's settings alone, as `g-mesh status` reports them: the
/// embedding cache's switch is neither read nor logged.
pub fn rerank_settings() -> RerankSettings {
    let switches = resolve(env_lookup, false, read_global_config, |line| crate::log_line!("{line}"));
    RerankSettings { enabled: switches.rerank, model_dir: rerank_model_dir() }
}

/// Where `model_name`'s weights live: `$G_MESH_MODEL_DIR`, else
/// `~/.g-mesh/models/<model_name>`.
pub fn model_dir(model_name: &str) -> Result<PathBuf> {
    default_model_dir_from(model_dir_override().as_deref(), model_name)
}

/// The full resolution order for a model directory: an explicitly given path,
/// else [`model_dir`].
///
/// The writer (`g-mesh model fetch`, see [`crate::cli::model`]) and the
/// reader (the daemon's pipeline, through [`embedding_settings`]) both
/// resolve through here: their disagreeing about the location is the one
/// failure neither side can detect, since the fetcher would report success
/// and the loader would still report a missing model.
pub fn resolve_model_dir(explicit: Option<&Path>, model_name: &str) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir.to_path_buf()),
        None => model_dir(model_name),
    }
}

/// The cross-encoder's directory: `$G_MESH_RERANK_MODEL_DIR` when set and
/// not empty, else its default beside the embedding model.
pub fn rerank_model_dir() -> Result<PathBuf> {
    let dir_override = crate::knobs::os_non_empty(&crate::knobs::RERANK_MODEL_DIR).map(PathBuf::from);
    rerank_model_dir_from(dir_override.as_deref())
}

/// The `G_MESH_EMBED_*` inference options of the process environment.
pub fn inference_options() -> InferenceOptions {
    InferenceOptions::from_env_with(crate::knobs::lookup)
}

fn model_dir_override() -> Option<PathBuf> {
    crate::knobs::os_raw(&crate::knobs::MODEL_DIR).map(PathBuf::from)
}

fn env_lookup(name: &str) -> Option<OsString> {
    crate::knobs::lookup_os(name)
}

/// Which parts the environment and the global config leave switched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Switches {
    /// The cache's size bound when it is on; `None` when it is off.
    pub(crate) cache_max_size_mb: Option<u64>,
    pub(crate) rerank: bool,
}

/// The precedence rules over any variable lookup and any global config read,
/// so they can be checked without touching the process environment.
/// `wants_cache` false leaves the cache off without reading its switch.
/// `global` runs at most once, and only when a part is not switched off by
/// `lookup`; when it fails, `log` gets the line of each part that needed it.
pub(crate) fn resolve(
    lookup: impl Fn(&str) -> Option<OsString>,
    wants_cache: bool,
    global: impl FnOnce() -> Result<GlobalConfig>,
    mut log: impl FnMut(&str),
) -> Switches {
    let cache_on = wants_cache && !switched_off(lookup(CACHE_ENV));
    let rerank_on = !switched_off(lookup(RERANK_ENV));
    if !cache_on && !rerank_on {
        return Switches { cache_max_size_mb: None, rerank: false };
    }
    let file = global().unwrap_or_else(|err| {
        if cache_on {
            log(&format!(
                "g-mesh: failed to read the global config ({err:#}) - using the embedding cache's defaults"
            ));
        }
        if rerank_on {
            log(&format!(
                "g-mesh: failed to read the global config ({err:#}) - the rerank keeps its default (on)"
            ));
        }
        GlobalConfig::default()
    });
    Switches {
        cache_max_size_mb: (cache_on && file.embedding_cache.enabled)
            .then_some(file.embedding_cache.max_size_mb),
        rerank: rerank_on && file.rerank.enabled,
    }
}

fn switched_off(value: Option<OsString>) -> bool {
    crate::knobs::is_off(value.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `G_MESH_EMBEDDING_CACHE=off` is the kill switch.
    ///
    /// Control: make `switched_off` return false and the `off` rows fail.
    #[test]
    fn the_environment_switch_turns_the_cache_off() {
        assert!(switched_off(Some("off".into())));
        assert!(switched_off(Some("OFF".into())));
        assert!(!switched_off(None));
        assert!(!switched_off(Some("".into())));
        assert!(!switched_off(Some("on".into())));
    }

    #[test]
    fn the_environment_switch_turns_the_rerank_off() {
        assert!(switched_off(Some("off".into())));
        assert!(switched_off(Some("OFF".into())));
        assert!(!switched_off(Some("on".into())));
        assert!(!switched_off(None));
    }

    /// An explicit directory wins over both the env override and the home
    /// default - the `--dir` argument of `g-mesh model fetch` relies on this,
    /// and so does anyone who set `G_MESH_MODEL_DIR` and then asked for a
    /// different directory once.
    #[test]
    fn an_explicit_directory_wins_over_the_env_override_and_the_default() {
        let explicit = Path::new("/somewhere/else");
        assert_eq!(resolve_model_dir(Some(explicit), "some-model").unwrap(), explicit);
        assert_eq!(resolve_model_dir(None, "some-model").unwrap(), model_dir("some-model").unwrap());
    }
}
