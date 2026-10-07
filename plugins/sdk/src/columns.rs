//! Byte columns in, character columns out.
//!
//! The wire counts columns in Unicode scalar values (characters); a
//! tree-sitter parser, and any other parser that reads a byte slice, reports
//! byte offsets. On an ASCII line the two agree, which is why getting the
//! conversion wrong is silent until the first line with a `é` on it.
//! [`CharColumns`] is that conversion, computed against the file's own text,
//! for any extractor that holds a `(row, byte column)` pair.
//!
//! It takes plain numbers rather than a parser's point type so that the SDK
//! does not depend on a parser.

use g_mesh_wire::{Position, Range};

/// One file's text, indexed by line, for converting byte columns to
/// character columns.
pub struct CharColumns<'s> {
    source: &'s str,
    /// The byte offset each line starts at, indexed by line number.
    line_starts: Vec<usize>,
}

impl<'s> CharColumns<'s> {
    /// Indexes `source`'s line starts. Lines are split on `\n` only, as a
    /// parser's row counter splits them; a `\r` before it is an ordinary
    /// character of the line.
    pub fn new(source: &'s str) -> Self {
        let mut line_starts = vec![0usize];
        line_starts.extend(source.match_indices('\n').map(|(at, _)| at + 1));
        Self { source, line_starts }
    }

    /// The text this was built over.
    pub fn source(&self) -> &'s str {
        self.source
    }

    /// The wire position of byte column `byte_column` on line `line`.
    ///
    /// A line past the end, or a byte column that does not fall on a
    /// character boundary, returns the byte column unchanged rather than
    /// guessing.
    pub fn at(&self, line: usize, byte_column: usize) -> Position {
        let col = self
            .line_starts
            .get(line)
            .and_then(|start| self.source.get(*start..start + byte_column))
            .map_or(byte_column, |prefix| prefix.chars().count());
        Position { line: line as u32, col: col as u32 }
    }

    /// A range from two `(line, byte column)` pairs.
    pub fn range(&self, start: (usize, usize), end: (usize, usize)) -> Range {
        Range { start: self.at(start.0, start.1), end: self.at(end.0, end.1) }
    }

    /// The whole file's range, ending where its content ends: trailing
    /// whitespace (`' '`, `\t`, `\r`, `\n`, `\v`, `\f`) is trimmed, and the
    /// end is `(number of newlines in the rest, length of its last line)`.
    ///
    /// So the end line is the file's last real line (`"a\nbc\n"` ends at
    /// `(1, 2)`, never on the empty line after the final newline), and a
    /// space inserted before the file's last newline does not move it (see
    /// [`FileGraphBuilder::file_node`](crate::FileGraphBuilder::file_node)).
    /// An empty or whitespace-only file is `(0, 0)`.
    pub fn file_range(&self) -> Range {
        Range { start: Position { line: 0, col: 0 }, end: content_end(self.source) }
    }
}

/// The trailing whitespace a whole-file range ends before.
const TRAILING_WHITESPACE: [char; 6] = [' ', '\t', '\r', '\n', '\x0b', '\x0c'];

/// The end of `text` with its trailing whitespace removed, in characters:
/// `(number of '\n' in the content, characters after the last one)`.
fn content_end(text: &str) -> Position {
    let content = text.trim_end_matches(TRAILING_WHITESPACE);
    let (line, last) = match content.rfind('\n') {
        Some(at) => (content.matches('\n').count(), &content[at + 1..]),
        None => (0, content),
    };
    Position { line: line as u32, col: last.chars().count() as u32 }
}
