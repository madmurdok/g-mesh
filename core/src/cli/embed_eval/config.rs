//! `eval/embedding/corpora.toml` and `eval/embedding/variants.toml`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::embedding::model::{EncoderSpec, Pooling};

#[derive(Debug, Clone, Deserialize)]
pub struct CorporaFile {
    #[serde(rename = "corpus")]
    pub corpora: Vec<Corpus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Corpus {
    pub id: String,
    pub language: String,
    /// Where the pinned revision comes from: a git URL or a local repository.
    pub source: String,
    pub revision: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// The checkout the snapshot is indexed from, relative to the eval dir.
    pub checkout: String,
}

impl CorporaFile {
    pub fn load(eval_dir: &Path) -> Result<Self> {
        let path = eval_dir.join("corpora.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Result<&Corpus> {
        self.corpora.iter().find(|c| c.id == id).with_context(|| format!("no corpus {id} in corpora.toml"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VariantsFile {
    pub settings: Settings,
    #[serde(rename = "variant")]
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    /// The variant every other one is compared against (D9's R).
    pub reference: String,
    pub bootstrap_seed: u64,
    pub bootstrap_resamples: usize,
    /// Seed of the random, shuffled and words-shuffled arms.
    pub arm_seed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arm {
    /// A real encoder run through `EmbeddingModel`.
    Model,
    /// i.i.d. Gaussian node and query vectors (D7).
    Random,
    /// The reference's vectors, reassigned to nodes by a derangement (D7).
    Shuffled,
    /// Each node text's words shuffled, re-embedded with the reference (D7).
    WordsShuffled,
    /// Lexical ranking over the same texts (D7).
    Bm25,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Reference,
    /// Smaller or faster: D9's cost-candidate rule.
    Cost,
    /// Not cheaper: D9's quality-candidate rule.
    Quality,
    /// A broken arm gated by D7.
    Control,
    /// Reported, never gated.
    Informational,
}

/// Which text of a node a model arm embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextForm {
    /// `text_to_embed`'s output, what production embeds.
    #[default]
    Full,
    /// The doc comment cut at its first blank line, then the signature, in
    /// `text_to_embed`'s layout.
    FirstParagraph,
    /// The doc comment trimmed to its prose outline by
    /// `structured::structured_doc` (summary, headings, short paragraphs; no
    /// code, parameter lists or link lines), then the signature, in
    /// `text_to_embed`'s layout.
    Structured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolingName {
    Mean,
    Cls,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    pub name: String,
    pub arm: Arm,
    pub role: Role,
    /// Hugging Face repository and the commit its files are pinned to.
    #[serde(default)]
    pub hf_repo: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    /// The ONNX file inside the repository, fetched as `model.onnx`.
    #[serde(default)]
    pub onnx_file: Option<String>,
    /// Directory holding `model.onnx` and `tokenizer.json`; `~/` expands to
    /// the home directory, a relative path is relative to the eval dir.
    /// Defaults to `work/models/<name>`.
    #[serde(default)]
    pub model_dir: Option<String>,
    #[serde(default)]
    pub pooling: Option<PoolingName>,
    #[serde(default)]
    pub dimension: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub token_type_ids: bool,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    /// The model variant a shuffled or words-shuffled arm is derived from.
    #[serde(default)]
    pub reference: Option<String>,
    /// Kept last: `embed_eval::fingerprint` drops it when it is the default,
    /// so a default variant's fingerprint is the one it had without the field.
    #[serde(default)]
    pub text: TextForm,
}

impl VariantsFile {
    pub fn load(eval_dir: &Path) -> Result<Self> {
        let path = eval_dir.join("variants.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text)?;
        file.validate()?;
        Ok(file)
    }

    fn validate(&self) -> Result<()> {
        let reference = self.get(&self.settings.reference)?;
        if reference.arm != Arm::Model || reference.role != Role::Reference {
            bail!("the reference variant {} must be a model arm with role = \"reference\"", reference.name);
        }
        for v in &self.variants {
            if self.variants.iter().filter(|w| w.name == v.name).count() > 1 {
                bail!("variant {} is declared twice", v.name);
            }
            match v.arm {
                Arm::Model => {
                    v.encoder_spec()?;
                }
                Arm::Shuffled | Arm::WordsShuffled => {
                    let base = v
                        .reference
                        .as_deref()
                        .with_context(|| format!("variant {} needs `reference`", v.name))?;
                    if self.get(base)?.arm != Arm::Model {
                        bail!("variant {}'s reference {base} is not a model arm", v.name);
                    }
                }
                Arm::Random => {
                    if v.dimension.is_none() {
                        bail!("random variant {} needs `dimension`", v.name);
                    }
                }
                Arm::Bm25 => {}
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&Variant> {
        self.variants
            .iter()
            .find(|v| v.name == name)
            .with_context(|| format!("no variant {name} in variants.toml"))
    }

    /// The model variant whose encoder a variant runs: itself for a model
    /// arm, its `reference` for words-shuffled.
    pub fn encoder_of<'a>(&'a self, variant: &'a Variant) -> Result<&'a Variant> {
        match variant.arm {
            Arm::Model => Ok(variant),
            Arm::WordsShuffled | Arm::Shuffled => self.get(variant.reference.as_deref().unwrap_or_default()),
            Arm::Random | Arm::Bm25 => bail!("variant {} runs no encoder", variant.name),
        }
    }
}

impl Variant {
    pub fn encoder_spec(&self) -> Result<EncoderSpec> {
        let pooling = match self.pooling.with_context(|| format!("variant {} needs `pooling`", self.name))? {
            PoolingName::Mean => Pooling::Mean,
            PoolingName::Cls => Pooling::Cls,
        };
        let dimension = self.dimension.with_context(|| format!("variant {} needs `dimension`", self.name))?;
        let max_sequence_length =
            self.max_tokens.with_context(|| format!("variant {} needs `max_tokens`", self.name))?;
        Ok(EncoderSpec { pooling, dimension, max_sequence_length, token_type_ids: self.token_type_ids })
    }

    pub fn model_dir(&self, eval_dir: &Path) -> Result<PathBuf> {
        match self.model_dir.as_deref() {
            Some(dir) if dir.starts_with("~/") => {
                Ok(dirs::home_dir().context("could not resolve home directory")?.join(&dir[2..]))
            }
            Some(dir) if Path::new(dir).is_absolute() => Ok(PathBuf::from(dir)),
            Some(dir) => Ok(eval_dir.join(dir)),
            None => Ok(eval_dir.join("work").join("models").join(&self.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARIANTS: &str = include_str!("../../../../eval/embedding/variants.toml");

    /// D4: the reference row runs exactly what production runs - the same
    /// `EncoderSpec` `EmbeddingModel::load` uses, and no prefixes. Control:
    /// changing the jina row's pooling, dimension, max_tokens or
    /// token_type_ids in variants.toml, or giving it a prefix, fails this.
    #[test]
    fn the_reference_variant_is_todays_production_behaviour() {
        let file = VariantsFile::parse(VARIANTS).unwrap();
        let reference = file.get(&file.settings.reference).unwrap();
        assert_eq!(reference.encoder_spec().unwrap(), EncoderSpec::production());
        assert_eq!(reference.query_prefix, "");
        assert_eq!(reference.document_prefix, "");
        assert_eq!(reference.revision.as_deref(), Some(crate::cli::model::MODEL_REVISION));
    }

    #[test]
    fn every_broken_arm_of_d7_is_declared() {
        let file = VariantsFile::parse(VARIANTS).unwrap();
        for arm in [Arm::Random, Arm::Shuffled, Arm::WordsShuffled, Arm::Bm25] {
            assert!(file.variants.iter().any(|v| v.arm == arm), "{arm:?} missing");
        }
    }

    #[test]
    fn a_shuffled_arm_must_name_a_model_reference() {
        let text = r#"
            [settings]
            reference = "r"
            bootstrap_seed = 1
            bootstrap_resamples = 10
            arm_seed = 1

            [[variant]]
            name = "r"
            arm = "model"
            role = "reference"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024

            [[variant]]
            name = "s"
            arm = "shuffled"
            role = "control"
        "#;
        assert!(VariantsFile::parse(text).is_err());
        assert!(VariantsFile::parse(
            &text.replace("role = \"control\"", "role = \"control\"\nreference = \"r\"")
        )
        .is_ok());
    }
}
