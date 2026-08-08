//! Local, CPU-only text embedding.
//!
//! This module is the "text in, vector out" half of the future `search_code`
//! tool: it turns a string (a symbol's signature plus its docstring, or a
//! developer's natural-language query) into a fixed-size `Vec<f32>` that can
//! be compared to other vectors by cosine similarity. It knows nothing about
//! where those vectors are stored - persistence is `storage`'s job - and
//! nothing about what text is worth embedding, which is the indexer's.
//!
//! Everything runs in-process on the CPU against a model on local disk. No
//! request leaves the machine at embed time, which is what REQUIREMENTS.md's
//! "содержимое проекта не покидает машину" guarantee needs from this layer.
//! The one moment g-mesh touches the network for embeddings is *acquiring* the
//! model's weights, which is explicitly allowed ("скачивание весов
//! embedding-модели при установке" - that is g-mesh pulling data in, not
//! project code going out) and is deliberately not done by this module: see
//! [`model`] on where weights are expected to already be.

pub mod model;
pub mod pipeline;

pub use model::{cosine_similarity, default_model_dir, EmbeddingModel, EMBEDDING_DIM};
pub use pipeline::EmbeddingPipeline;
