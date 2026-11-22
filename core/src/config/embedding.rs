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
    let (selected, explicit) = select_variant(env_lookup, file, |line| crate::log_line!("{line}"));
    settle_rerank(
        enabled,
        selected,
        explicit,
        rerank_dir_override().as_deref(),
        rerank_beta_override(|line| crate::log_line!("{line}")),
    )
}

/// The settings for `selected`. An unpublished variant
/// ([`RerankVariant::published`]) is kept only when both `dir_override`
/// and `beta_override` are set; otherwise its fallback stands in, from its
/// own default directory, at its own beta (ADR 0044). `dir_override` and
/// `beta_override` apply only to the variant selected and kept: a
/// directory set for the unpublished variant may hold its only copy, which
/// the stand-in must neither load nor advise fetching over.
pub(crate) fn settle_rerank(
    enabled: bool,
    selected: RerankVariant,
    explicit: bool,
    dir_override: Option<&Path>,
    beta_override: Option<f64>,
) -> RerankSettings {
    let measuring = dir_override.is_some() && beta_override.is_some();
    let (variant, unpublished, beta_override) = match selected.fallback() {
        Some(stand_in) if !selected.published() && !measuring => (stand_in, Some(selected), None),
        _ => (selected, None, beta_override),
    };
    let fallback_dir = if explicit || dir_override.is_some() || unpublished.is_some() {
        None
    } else {
        variant.fallback().and_then(|fallback| rerank_model_dir_from(None, fallback).ok())
    };
    let dir_override = if unpublished.is_some() { None } else { dir_override };
    RerankSettings {
        enabled,
        variant,
        model_dir: rerank_model_dir_from(dir_override, variant),
        fallback_dir,
        beta_override,
        unpublished,
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

    // --- the rerank variant -------------------------------------------------

    use crate::embedding::rerank::pretend_published;

    const QAT: RerankVariant = RerankVariant::Qat;
    const L6: RerankVariant = RerankVariant::L6;

    /// `select_variant` over `env` (name, value) pairs and a global config
    /// whose `[rerank] model` is `file_model`, with the lines logged.
    fn selected(env: &[(&str, &str)], file_model: Option<&str>) -> ((RerankVariant, bool), Vec<String>) {
        let lookup =
            |name: &str| env.iter().find(|(key, _)| *key == name).map(|(_, value)| OsString::from(value));
        let file = GlobalConfig {
            rerank: super::super::RerankConfig { enabled: true, model: file_model.map(str::to_string) },
            ..GlobalConfig::default()
        };
        let mut lines = Vec::new();
        let picked = select_variant(lookup, Some(&file), |line| lines.push(line.to_string()));
        (picked, lines)
    }

    fn default_dir(dir_name: &str) -> PathBuf {
        dirs::home_dir().unwrap().join(".g-mesh").join("models").join(dir_name)
    }

    /// The settings' parts other than `model_dir`, which is compared apart.
    fn parts(
        settings: &RerankSettings,
    ) -> (bool, RerankVariant, Option<PathBuf>, Option<f64>, Option<RerankVariant>) {
        (
            settings.enabled,
            settings.variant,
            settings.fallback_dir.clone(),
            settings.beta_override,
            settings.unpublished,
        )
    }

    /// With no setting the rerank is today's: `l6` from
    /// `~/.g-mesh/models/ms-marco-MiniLM-L6-v2` at beta 40, no fallback, no
    /// stand-in note.
    ///
    /// *Control:* `RerankVariant::DEFAULT = Self::Qat` -> the default
    /// selects `qat`.
    #[test]
    fn with_no_setting_the_rerank_is_l6_as_before() {
        assert_eq!(selected(&[], None), ((L6, false), vec![]));
        assert_eq!(selected(&[(RERANK_MODEL_ENV, "")], Some("")), ((L6, false), vec![]));
        let settings = settle_rerank(true, L6, false, None, None);
        assert_eq!(parts(&settings), (true, L6, None, None, None));
        assert_eq!(settings.model_dir.unwrap(), default_dir("ms-marco-MiniLM-L6-v2"));
        assert_eq!(L6.beta().unwrap(), 40.0);
    }

    /// `G_MESH_RERANK_MODEL` beats `[rerank] model`, which beats the
    /// default; either names the variant explicitly, in any case.
    ///
    /// *Control:* read `[rerank] model` before the environment in
    /// `select_variant` -> the env-over-file rows give the file's variant.
    #[test]
    fn the_environment_beats_the_file_and_the_file_beats_the_default() {
        assert_eq!(selected(&[(RERANK_MODEL_ENV, "qat")], Some("l6")), ((QAT, true), vec![]));
        assert_eq!(selected(&[(RERANK_MODEL_ENV, "l6")], Some("qat")), ((L6, true), vec![]));
        assert_eq!(selected(&[(RERANK_MODEL_ENV, "")], Some("qat")), ((QAT, true), vec![]));
        assert_eq!(selected(&[], Some(" QAT ")), ((QAT, true), vec![]));
        assert_eq!(selected(&[], Some("l6")), ((L6, true), vec![]));
    }

    /// A value naming no variant gives the default, not explicit, and one
    /// line naming where it came from and what is accepted.
    #[test]
    fn a_value_naming_no_variant_logs_one_line_and_gives_the_default() {
        let (picked, lines) = selected(&[(RERANK_MODEL_ENV, "l12")], Some("qat"));
        assert_eq!(picked, (L6, false));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains(&format!("{RERANK_MODEL_ENV} = \"l12\"")), "{lines:?}");
        assert!(lines[0].contains("accepted: l6, qat"), "{lines:?}");

        let (picked, lines) = selected(&[], Some("bogus"));
        assert_eq!(picked, (L6, false));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[rerank] model = \"bogus\""), "{lines:?}");
    }

    /// `qat` is not published: asked for, explicitly or not, with no
    /// directory or no beta set, `l6` stands in - always in `l6`'s own
    /// directory (a directory set for `qat` may hold its only copy), at
    /// `l6`'s own beta (a beta set for `qat` is dropped), with no fallback.
    ///
    /// *Control (directory):* pass `dir_override` to the stand-in in
    /// `settle_rerank` -> the directory-only row loads `l6` from it.
    ///
    /// *Control:* drop the `!selected.published()` arm of `settle_rerank`
    /// -> `qat` is kept.
    #[test]
    fn an_unpublished_qat_gets_l6_in_its_place() {
        assert!(!QAT.published() && L6.published());
        let elsewhere = Path::new("/models/elsewhere");
        for explicit in [true, false] {
            let plain = settle_rerank(true, QAT, explicit, None, None);
            assert_eq!(parts(&plain), (true, L6, None, None, Some(QAT)));
            assert_eq!(plain.model_dir.unwrap(), default_dir("ms-marco-MiniLM-L6-v2"));

            let dir_only = settle_rerank(true, QAT, explicit, Some(elsewhere), None);
            assert_eq!(parts(&dir_only), (true, L6, None, None, Some(QAT)));
            assert_eq!(dir_only.model_dir.unwrap(), default_dir("ms-marco-MiniLM-L6-v2"));
            assert_ne!(default_dir("ms-marco-MiniLM-L6-v2"), elsewhere);

            let beta_only = settle_rerank(true, QAT, explicit, None, Some(7.5));
            assert_eq!(parts(&beta_only), (true, L6, None, None, Some(QAT)), "the beta is qat's, dropped");
            assert_eq!(beta_only.model_dir.unwrap(), default_dir("ms-marco-MiniLM-L6-v2"));
        }
        let off = settle_rerank(false, QAT, true, None, None);
        assert_eq!(parts(&off), (false, L6, None, None, Some(QAT)));
    }

    /// The measurement setup: `qat` itself loads only with both a directory
    /// and a beta set, from that directory at that beta.
    ///
    /// *Control:* `measuring = dir_override.is_some()` in `settle_rerank`
    /// -> the directory-only row keeps `qat`.
    #[test]
    fn qat_loads_only_with_both_a_directory_and_a_beta() {
        let dir = Path::new("/models/qat");
        let measured = settle_rerank(true, QAT, true, Some(dir), Some(7.5));
        assert_eq!(parts(&measured), (true, QAT, None, Some(7.5), None));
        assert_eq!(measured.model_dir.unwrap(), dir);
        assert_eq!(settle_rerank(true, QAT, true, Some(dir), None).variant, L6);
        assert_eq!(settle_rerank(true, QAT, true, None, Some(7.5)).variant, L6);
    }

    /// `G_MESH_RERANK_BETA` applies to the variant loaded: kept for `l6`
    /// selected, dropped for `l6` standing in for `qat`.
    ///
    /// *Control:* keep `beta_override` in the stand-in arm of
    /// `settle_rerank` -> the stand-in carries 7.5.
    #[test]
    fn the_beta_override_applies_only_to_the_variant_selected_and_loaded() {
        assert_eq!(settle_rerank(true, L6, true, None, Some(7.5)).beta_override, Some(7.5));
        assert_eq!(settle_rerank(true, L6, false, None, Some(7.5)).beta_override, Some(7.5));
        assert_eq!(settle_rerank(true, QAT, true, None, Some(7.5)).beta_override, None);
        assert_eq!(settle_rerank(true, QAT, false, Some(Path::new("/d")), None).beta_override, None);
    }

    /// `G_MESH_RERANK_MODEL_DIR` with no variant set keeps today's setup:
    /// `l6` in that directory, no fallback, no note.
    #[test]
    fn a_rerank_directory_with_no_variant_set_loads_l6_from_it() {
        let dir = Path::new("/models/my-l6");
        let settings = settle_rerank(true, L6, false, Some(dir), None);
        assert_eq!(parts(&settings), (true, L6, None, None, None));
        assert_eq!(settings.model_dir.unwrap(), dir);
    }

    /// Once `qat` is published, it is loaded itself: from its own directory
    /// at the beta set; as an unchosen default its missing directory falls
    /// back to `l6`'s, and an explicit choice or a set directory never does.
    ///
    /// *Control:* make the stand-in arm of `settle_rerank` ignore
    /// `published()` (`Some(stand_in) if !measuring`) -> `l6` stands in.
    #[test]
    fn once_qat_is_published_it_loads_itself() {
        pretend_published(|| {
            let explicit = settle_rerank(true, QAT, true, None, Some(3.0));
            assert_eq!(parts(&explicit), (true, QAT, None, Some(3.0), None));
            assert_eq!(explicit.model_dir.unwrap(), default_dir("g-mesh-reranker-l6-qat"));

            let unchosen = settle_rerank(true, QAT, false, None, None);
            assert_eq!(parts(&unchosen), (true, QAT, Some(default_dir("ms-marco-MiniLM-L6-v2")), None, None));

            let in_dir = settle_rerank(true, QAT, false, Some(Path::new("/models/q")), None);
            assert_eq!(parts(&in_dir), (true, QAT, None, None, None));
        });
    }
}
