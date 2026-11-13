//! A linear query adapter: a d x d matrix `W` applied to a query vector
//! before it is scored against unchanged node vectors (q' = W q).
//!
//! File format: `d * d` little-endian `f32`, row-major (row `i` holds the
//! weights of output component `i`), no header. The file's sha256 is pinned
//! by whoever names the file, so a swapped or truncated matrix is refused.

use std::path::Path;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq)]
pub struct QueryAdapter {
    pub dim: usize,
    /// Row-major `dim x dim`.
    pub w: Vec<f32>,
}

impl QueryAdapter {
    /// Reads the matrix at `path`, checking that its size is a square `d x d`
    /// of `f32` and that its sha256 is `expected_sha256` (hex, any case).
    /// Every error names the file.
    pub fn load(path: &Path, expected_sha256: &str) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read adapter {}", path.display()))?;
        let count = bytes.len() / 4;
        let dim = count.isqrt();
        if bytes.is_empty() || !bytes.len().is_multiple_of(4) || dim * dim != count {
            bail!("adapter {} holds {} bytes, not a square d x d f32 matrix", path.display(), bytes.len());
        }
        let actual: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            bail!("adapter {} has sha256 {actual}, expected {expected_sha256}", path.display());
        }
        let w = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        Ok(Self { dim, w })
    }

    /// q' = W q. Each component is summed in `f32`, column order, so an
    /// identity `W` returns `q` unchanged in value.
    pub fn apply(&self, q: &[f32]) -> Vec<f32> {
        assert_eq!(
            q.len(),
            self.dim,
            "query has {} components, the adapter is {}x{}",
            q.len(),
            self.dim,
            self.dim
        );
        self.w
            .chunks_exact(self.dim)
            .map(|row| row.iter().zip(q).fold(0.0f32, |acc, (w, x)| acc + w * x))
            .collect()
    }
}
