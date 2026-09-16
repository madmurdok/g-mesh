//! The one spelling of a file path that crosses the wire.
//!
//! A node's id is derived from its `filePath` verbatim (`ids`' module doc),
//! so the same file addressed two ways produces two disjoint sets of nodes -
//! and nothing downstream can tell that it happened, because both sets look
//! perfectly well-formed. The spelling is therefore not a detail to be
//! careful about at each call site but an invariant worth a type: project
//! relative, forward slashes, no leading `./`.
//!
//! This is the same convention core's `fileChanged` carries and the TS plugin
//! follows (`toPosixPath` plus `path.relative`); [`RelPath`] is the place it
//! is *stated* rather than repeated.

use std::fmt;
use std::path::Path;

/// A project-relative POSIX path, as it appears in every id and on the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    /// Normalizes `path` into the wire spelling: backslashes become forward
    /// slashes (Windows), and a leading `./` is dropped.
    ///
    /// Deliberately not fallible. An absolute path, or one climbing out of
    /// the project with `..`, is not rejected here - core would simply never
    /// match it against a file it knows, which is a visible, diagnosable
    /// failure, whereas a plugin that cannot build a `RelPath` at all has
    /// nothing useful to do about it in the middle of a walk.
    pub fn new(path: impl AsRef<str>) -> Self {
        let path = path.as_ref().replace('\\', "/");
        Self(path.strip_prefix("./").unwrap_or(&path).to_string())
    }

    /// `path` made relative to `root`, in the wire spelling. `None` when
    /// `path` is not under `root` at all.
    pub fn relative_to(root: &Path, path: &Path) -> Option<Self> {
        let relative = path.strip_prefix(root).ok()?;
        Some(Self::new(relative.to_string_lossy()))
    }

    /// The path as core spells it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The lowercased extension *with* its leading dot (`.rs`), as a
    /// manifest's `extensions` list spells it. `None` for a path with no
    /// extension.
    pub fn extension(&self) -> Option<String> {
        let name = self.0.rsplit('/').next()?;
        let (_, extension) = name.rsplit_once('.')?;
        (!extension.is_empty()).then(|| format!(".{}", extension.to_lowercase()))
    }

    /// This path resolved against the project root, for reading the file.
    pub fn to_absolute(&self, root: &Path) -> std::path::PathBuf {
        root.join(&self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RelPath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<String> for RelPath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_the_wire_spelling() {
        assert_eq!(RelPath::new("src\\a.rs").as_str(), "src/a.rs");
        assert_eq!(RelPath::new("./src/a.rs").as_str(), "src/a.rs");
        assert_eq!(RelPath::new("src/a.rs").as_str(), "src/a.rs");
    }

    #[test]
    fn extensions_are_lowercased_and_keep_their_dot() {
        assert_eq!(RelPath::new("src/A.TS").extension().as_deref(), Some(".ts"));
        assert_eq!(RelPath::new("src/a.tar.gz").extension().as_deref(), Some(".gz"));
        assert_eq!(RelPath::new("Makefile").extension(), None);
        assert_eq!(RelPath::new("src/.gitignore").extension().as_deref(), Some(".gitignore"));
        assert_eq!(RelPath::new("src/a.").extension(), None);
    }

    #[test]
    fn relative_to_a_root_it_is_not_under_is_none() {
        assert_eq!(RelPath::relative_to(Path::new("/a"), Path::new("/b/c.rs")), None);
        assert_eq!(
            RelPath::relative_to(Path::new("/a"), Path::new("/a/b/c.rs")).map(|p| p.as_str().to_string()),
            Some("b/c.rs".to_string())
        );
    }
}
