//! The embedding pipeline's settings: the project's `[embedding]` section,
//! the global config's `[embeddingCache]` and `[rerank]` sections, and the
//! environment variables that override them, resolved into the plain values
//! [`crate::embedding`] takes.
//!
//! Precedence, per part: an environment switch set to `off` wins over the
//! file; the file's `enabled` wins over the default (on); a global config
//! that cannot be read gives the defaults and one line naming the part that
//! needed it. The file is read at most once per resolution, and not at all
//! when both parts are switched off by the environment. The rerank variant
//! is `G_MESH_RERANK_MODEL`, else `[rerank] model`, else the default; a value
//! naming no variant gives the default and one line.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{read_global_config, EmbeddingConfig, GlobalConfig};
use crate::embedding::cache::{self, CACHE_ENV};
use crate::embedding::model::{default_model_dir_from, InferenceOptions};
use crate::embedding::rerank::{rerank_model_dir_from, RERANK_ENV, RERANK_MODEL_ENV};
use crate::embedding::{CacheSettings, EmbeddingSettings, ModelSettings, RerankSettings, RerankVariant};

/// Everything `EmbeddingPipeline::load` needs for a project configured with
/// `config`, from the process environment and the global config.
pub fn embedding_settings(config: &EmbeddingConfig) -> EmbeddingSettings {
    let (switches, file) = resolve(env_lookup, true, read_global_config, |line| crate::log_line!("{line}"));
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
        rerank: rerank_settings_for(switches.rerank, file.as_ref()),
    }
}

/// The rerank's settings alone, as `g-mesh status` reports them: the
/// embedding cache's switch is neither read nor logged.
pub fn rerank_settings() -> RerankSettings {
    let (switches, file) = resolve(env_lookup, false, read_global_config, |line| crate::log_line!("{line}"));
    rerank_settings_for(switches.rerank, file.as_ref())
}

/// The rerank's settings for `g-mesh model`: the variant is resolved from
/// the global config even when the environment switches the rerank off,
/// since `model fetch` fills the selected variant's directory either way.
pub fn rerank_model_settings() -> RerankSettings {
    let file = read_global_config()
        .map_err(|err| {
            crate::log_line!(
                "g-mesh: failed to read the global config ({err:#}) - the rerank model keeps its default ({})",
                RerankVariant::DEFAULT
            );
        })
        .ok();
    let enabled =
        !switched_off(env_lookup(RERANK_ENV)) && file.as_ref().is_none_or(|file| file.rerank.enabled);
    rerank_settings_for(enabled, file.as_ref())
}

/// [`RerankSettings`] for the variant [`select_variant`] picks from the
/// environment and `file`.
fn rerank_settings_for(enabled: bool, file: Option<&GlobalConfig>) -> RerankSettings {
    let (variant, explicit) = select_variant(env_lookup, file, |line| crate::log_line!("{line}"));
    let dir_override = rerank_dir_override();
    let fallback_dir = if explicit || dir_override.is_some() {
        None
    } else {
        variant.fallback().and_then(|fallback| rerank_model_dir_from(None, fallback).ok())
    };
    RerankSettings {
        enabled,
        variant,
        model_dir: rerank_model_dir_from(dir_override.as_deref(), variant),
        fallback_dir,
        beta_override: rerank_beta_override(|line| crate::log_line!("{line}")),
    }
}

/// The rerank variant: `G_MESH_RERANK_MODEL` when set and not empty, else
/// `[rerank] model` of `file`, else the default. The flag is true when a
/// setting named the variant; a value naming none logs one line and gives
/// the default, not explicit.
pub(crate) fn select_variant(
    lookup: impl Fn(&str) -> Option<OsString>,
    file: Option<&GlobalConfig>,
    mut log: impl FnMut(&str),
) -> (RerankVariant, bool) {
    let env = lookup(RERANK_MODEL_ENV).filter(|value| !value.is_empty());
    let (value, source) = match (&env, file.and_then(|file| file.rerank.model.as_deref())) {
        (Some(value), _) => (value.to_string_lossy().into_owned(), RERANK_MODEL_ENV),
        (None, Some(value)) if !value.is_empty() => (value.to_string(), "[rerank] model"),
        _ => return (RerankVariant::DEFAULT, false),
    };
    match RerankVariant::parse(&value) {
        Some(variant) => (variant, true),
        None => {
            let accepted: Vec<&str> = RerankVariant::ALL.iter().map(|variant| variant.name()).collect();
            log(&format!(
                "g-mesh: {source} = {value:?} names no rerank model (accepted: {}) - using {}",
                accepted.join(", "),
                RerankVariant::DEFAULT
            ));
            (RerankVariant::DEFAULT, false)
        }
    }
}

/// `$G_MESH_RERANK_BETA` when it is a finite number >= 0; any other set
/// value logs one line and is ignored.
fn rerank_beta_override(mut log: impl FnMut(&str)) -> Option<f64> {
    let raw = crate::knobs::string_non_empty(&crate::knobs::RERANK_BETA)?;
    match raw.trim().parse::<f64>() {
        Ok(beta) if beta.is_finite() && beta >= 0.0 => Some(beta),
        _ => {
            log(&format!(
                "g-mesh: {} = {raw:?} is not a finite number >= 0 - ignored",
                crate::knobs::RERANK_BETA.name
            ));
            None
        }
    }
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

/// `variant`'s directory: `$G_MESH_RERANK_MODEL_DIR` when set and not
/// empty, else its default beside the embedding model.
pub fn rerank_model_dir(variant: RerankVariant) -> Result<PathBuf> {
    rerank_model_dir_from(rerank_dir_override().as_deref(), variant)
}

fn rerank_dir_override() -> Option<PathBuf> {
    crate::knobs::os_non_empty(&crate::knobs::RERANK_MODEL_DIR).map(PathBuf::from)
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
/// The file is returned when it was read.
pub(crate) fn resolve(
    lookup: impl Fn(&str) -> Option<OsString>,
    wants_cache: bool,
    global: impl FnOnce() -> Result<GlobalConfig>,
    mut log: impl FnMut(&str),
) -> (Switches, Option<GlobalConfig>) {
    let cache_on = wants_cache && !switched_off(lookup(CACHE_ENV));
    let rerank_on = !switched_off(lookup(RERANK_ENV));
    if !cache_on && !rerank_on {
        return (Switches { cache_max_size_mb: None, rerank: false }, None);
    }
    let mut read = true;
    let file = global().unwrap_or_else(|err| {
        read = false;
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
    let switches = Switches {
        cache_max_size_mb: (cache_on && file.embedding_cache.enabled)
            .then_some(file.embedding_cache.max_size_mb),
        rerank: rerank_on && file.rerank.enabled,
    };
    (switches, read.then_some(file))
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

    const CACHE_LINE: &str = "- using the embedding cache's defaults";
    const RERANK_LINE: &str = "- the rerank keeps its default (on)";

    /// `resolve` over `env` (name, value) pairs and `global`, returning the
    /// switches, the lines logged and how many times `global` ran.
    fn run(
        env: &[(&str, &str)],
        wants_cache: bool,
        global: Result<GlobalConfig>,
    ) -> (Switches, Vec<String>, usize) {
        let lookup =
            |name: &str| env.iter().find(|(key, _)| *key == name).map(|(_, value)| OsString::from(value));
        let reads = std::cell::Cell::new(0);
        let mut lines = Vec::new();
        let (switches, _) = resolve(
            lookup,
            wants_cache,
            || {
                reads.set(reads.get() + 1);
                global
            },
            |line| lines.push(line.to_string()),
        );
        (switches, lines, reads.get())
    }

    fn file(cache_enabled: bool, max_size_mb: u64, rerank_enabled: bool) -> Result<GlobalConfig> {
        Ok(GlobalConfig {
            embedding_cache: super::super::EmbeddingCacheConfig { enabled: cache_enabled, max_size_mb },
            rerank: super::super::RerankConfig { enabled: rerank_enabled, model: None },
            ..GlobalConfig::default()
        })
    }

    fn broken() -> Result<GlobalConfig> {
        Err(anyhow::anyhow!("expected `=` at line 1"))
    }

    #[test]
    fn with_no_switch_the_file_decides_each_part() {
        let (switches, lines, reads) = run(&[], true, file(true, 64, true));
        assert_eq!(switches, Switches { cache_max_size_mb: Some(64), rerank: true });
        assert_eq!((lines.len(), reads), (0, 1));

        let (switches, _, _) = run(&[], true, file(false, 64, true));
        assert_eq!(switches, Switches { cache_max_size_mb: None, rerank: true }, "file turns the cache off");

        let (switches, _, _) = run(&[], true, file(true, 64, false));
        assert_eq!(
            switches,
            Switches { cache_max_size_mb: Some(64), rerank: false },
            "file turns the rerank off"
        );
    }

    #[test]
    fn an_environment_off_beats_a_file_that_enables_the_part() {
        let (switches, _, _) = run(&[(CACHE_ENV, "off")], true, file(true, 64, true));
        assert_eq!(switches, Switches { cache_max_size_mb: None, rerank: true });

        let (switches, _, _) = run(&[(RERANK_ENV, "OFF")], true, file(true, 64, true));
        assert_eq!(switches, Switches { cache_max_size_mb: Some(64), rerank: false });
    }

    #[test]
    fn both_parts_switched_off_by_the_environment_never_read_the_file() {
        let (switches, lines, reads) = run(&[(CACHE_ENV, "off"), (RERANK_ENV, "off")], true, broken());
        assert_eq!(switches, Switches { cache_max_size_mb: None, rerank: false });
        assert_eq!((lines.len(), reads), (0, 0));
    }

    #[test]
    fn a_broken_file_gives_the_defaults_and_one_line_per_part_that_needed_it() {
        let (switches, lines, reads) = run(&[], true, broken());
        let defaults = GlobalConfig::default();
        assert_eq!(
            switches,
            Switches { cache_max_size_mb: Some(defaults.embedding_cache.max_size_mb), rerank: true }
        );
        assert_eq!(reads, 1);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("g-mesh: failed to read the global config (")
                && lines[0].ends_with(CACHE_LINE)
        );
        assert!(
            lines[1].starts_with("g-mesh: failed to read the global config (")
                && lines[1].ends_with(RERANK_LINE)
        );
        assert!(lines.iter().all(|line| line.contains("expected `=` at line 1")), "{lines:?}");

        let (_, lines, _) = run(&[(CACHE_ENV, "off")], true, broken());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with(RERANK_LINE));

        let (_, lines, _) = run(&[(RERANK_ENV, "off")], true, broken());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with(CACHE_LINE));
    }

    /// `g-mesh status` resolves the rerank alone: no cache, no cache line,
    /// whatever the cache's switch says.
    #[test]
    fn without_the_cache_only_the_rerank_is_resolved_and_logged() {
        let (switches, lines, _) = run(&[], false, broken());
        assert_eq!(switches, Switches { cache_max_size_mb: None, rerank: true });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with(RERANK_LINE));

        let (switches, _, _) = run(&[], false, file(true, 64, true));
        assert_eq!(switches, Switches { cache_max_size_mb: None, rerank: true });
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
