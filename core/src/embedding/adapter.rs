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

#[cfg(test)]
mod tests {
    use super::*;

    fn le_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Writes `bytes` as `w.f32` in `dir` and returns its path and sha256.
    fn write_w(dir: &Path, bytes: &[u8]) -> (std::path::PathBuf, String) {
        let path = dir.join("w.f32");
        std::fs::write(&path, bytes).unwrap();
        (path, sha256_hex(bytes))
    }

    /// q' = W q with row `i` of the file = output `i`. W = [[1, 2], [3, 4]]:
    /// e0 -> (1, 3), e1 -> (2, 4). Control: multiplying by Wᵀ (indexing
    /// `w[j * dim + i]`) gives (1, 2) and fails; returning `q` fails.
    #[test]
    fn apply_multiplies_the_row_major_matrix_by_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha) = write_w(dir.path(), &le_bytes(&[1.0, 2.0, 3.0, 4.0]));
        let adapter = QueryAdapter::load(&path, &sha).unwrap();
        assert_eq!(adapter.dim, 2);
        assert_eq!(adapter.apply(&[1.0, 0.0]), vec![1.0, 3.0]);
        assert_eq!(adapter.apply(&[0.0, 1.0]), vec![2.0, 4.0]);
        assert_eq!(adapter.apply(&[0.5, -1.0]), vec![-1.5, -2.5]);
    }

    /// An identity W returns the query unchanged in value, the premise of the
    /// identity-adapter control (C0).
    #[test]
    fn an_identity_adapter_returns_the_query_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha) = write_w(dir.path(), &le_bytes(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]));
        let adapter = QueryAdapter::load(&path, &sha).unwrap();
        let q = [0.123_456_7_f32, -3.5e-3, 42.0];
        assert_eq!(adapter.apply(&q), q.to_vec());
    }

    /// A sha256 other than the file's fails and names the file; the check is
    /// case-insensitive. Control: dropping the sha256 comparison in `load`
    /// accepts the wrong hash.
    #[test]
    fn load_refuses_a_wrong_sha256_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha) = write_w(dir.path(), &le_bytes(&[1.0, 0.0, 0.0, 1.0]));
        let err = format!("{:#}", QueryAdapter::load(&path, &"0".repeat(64)).unwrap_err());
        assert!(err.contains(&path.display().to_string()) && err.contains("sha256"), "{err}");
        assert!(QueryAdapter::load(&path, &sha.to_uppercase()).is_ok());
    }

    /// A file that is not a square d x d f32 matrix fails naming the file,
    /// even when its sha256 is the one pinned.
    #[test]
    fn load_refuses_a_file_that_is_not_a_square_f32_matrix() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [Vec::new(), le_bytes(&[1.0, 0.0, 0.0]), vec![0u8; 17]] {
            let (path, sha) = write_w(dir.path(), &bytes);
            let err = format!("{:#}", QueryAdapter::load(&path, &sha).unwrap_err());
            assert!(err.contains(&path.display().to_string()), "{err}");
            assert!(err.contains("not a square d x d f32 matrix"), "{} bytes: {err}", bytes.len());
        }
        let missing = dir.path().join("absent.f32");
        let err = format!("{:#}", QueryAdapter::load(&missing, "00").unwrap_err());
        assert!(err.contains("absent.f32"), "{err}");
    }

    /// Python export -> Rust load (behaviour 7). The fixture was written by
    /// `eval/training/gm436/export_adapter.py` (`write_w`, `apply`), and
    /// `test_gm436_export.py` checks it is still what that code writes.
    /// Rust's W q must match Python's within 1e-6 on all 3 queries. Control:
    /// a column-major read (Wᵀ) in `load` or `apply` fails.
    #[test]
    fn a_python_exported_adapter_round_trips_through_load_and_apply() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../eval/training/gm436/fixtures/w_roundtrip");
        let read = |name: &str| -> Vec<f32> {
            std::fs::read(fixture.join(name))
                .unwrap()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        let sha = std::fs::read_to_string(fixture.join("W.f32.sha256")).unwrap();
        let sha = sha.split_whitespace().next().unwrap();
        let adapter = QueryAdapter::load(&fixture.join("W.f32"), sha).unwrap();
        assert_eq!(adapter.dim, 3);
        let queries = read("q.f32");
        let expected = read("expected.f32");
        assert_eq!(queries.len(), 9);
        for (q, want) in queries.chunks_exact(3).zip(expected.chunks_exact(3)) {
            let got = adapter.apply(q);
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() <= 1e-6, "q {q:?}: rust {got:?}, python {want:?}");
            }
        }
    }
}
