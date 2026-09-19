//! Columns and file names: the two things the bridge and a language server
//! both have and spell differently.
//!
//! # Decision 3: a column is not a column
//!
//! Three units are in play and only one of them is g-mesh's.
//!
//! - **The wire** counts **Unicode scalar values** - characters. Every range
//!   on it comes from an extractor that converted to them deliberately
//!   (`plugins/rust/src/extractor/emit.rs`'s `Positions`, whose whole reason
//!   to exist is that tree-sitter counts bytes), and [`SdkIndex::node_at`]
//!   compares against them.
//! - **LSP** counts **UTF-16 code units** by default (base protocol,
//!   "Position": "the offset is based on a UTF-16 string representation"),
//!   and since 3.17 a client may negotiate `utf-8` or `utf-32` through
//!   `general.positionEncodings`.
//! - **A byte offset** is neither, and is what a server that ignores the
//!   negotiation and answers in UTF-8 gives.
//!
//! On a line of ASCII all three agree, which is exactly what makes getting
//! this wrong so quiet: every test written against `fn main()` passes, and the
//! first line with a `é` in it (or a `🦀`, where UTF-16 and UTF-32 differ too)
//! maps a definition onto the wrong node, or onto no node at all. There is no
//! error at any layer; a caller list is simply missing an entry.
//!
//! So the bridge negotiates ([`PositionEncoding`], sent in `initialize` and
//! read back from the server's `capabilities.positionEncoding`), and converts
//! in both directions against the file's own text - the text the SDK's index
//! holds, which is the same text the server was sent in `didOpen`, which is
//! the only reason a conversion computed here is true over there.
//!
//! `utf-32` is offered first because it *is* the wire's unit and makes both
//! conversions the identity; `utf-16` is offered because the specification
//! requires every client to support it, and is what almost every server will
//! pick.
//!
//! # File names
//!
//! LSP addresses documents by `file:` URI, the index by project-relative
//! path. The conversion is the usual one with the usual two traps: a path may
//! contain characters a URI must percent-encode, and a Windows path has a
//! drive letter that produces the third slash in `file:///C:/…`. Both
//! directions are here so they cannot disagree, and both are tested.

use std::path::{Path, PathBuf};

/// How a server counts a column - LSP 3.17's `positionEncoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PositionEncoding {
    /// Bytes.
    Utf8,
    /// UTF-16 code units: the specification's default, and the one value
    /// every client must support.
    Utf16,
    /// Unicode scalar values - what the g-mesh wire already counts, so both
    /// conversions become the identity.
    Utf32,
}

impl PositionEncoding {
    /// The encodings the bridge offers, in the order it prefers them.
    pub(crate) const OFFERED: [&'static str; 3] = ["utf-32", "utf-16", "utf-8"];

    /// What a server's `capabilities.positionEncoding` named, or the
    /// specification's default for a server that named nothing (or named
    /// something no version of this protocol defines - a server that answers
    /// outside the negotiation is a server whose answer cannot be trusted to
    /// mean anything else either, and UTF-16 is the reading every LSP client
    /// would give it).
    pub(crate) fn from_capability(named: Option<&str>) -> Self {
        match named {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    /// A wire column (characters) as this encoding counts it, within `line`.
    ///
    /// A column past the end of the line is clamped to the line's own length
    /// rather than extrapolated: the alternative is a number no encoding
    /// agrees on, and a request at the end of a line is a question a server
    /// can still answer.
    #[allow(clippy::wrong_self_convention)] // `self` is the encoding, not the value being converted
    pub(crate) fn from_wire_column(self, line: &str, column: u32) -> u32 {
        let prefix: String = line.chars().take(column as usize).collect();
        match self {
            Self::Utf8 => prefix.len() as u32,
            Self::Utf16 => prefix.encode_utf16().count() as u32,
            Self::Utf32 => prefix.chars().count() as u32,
        }
    }

    /// A column this encoding counted, as the wire counts it (characters).
    ///
    /// A column that lands inside a character - which only a server
    /// disagreeing with its own negotiated encoding can produce - rounds down
    /// to the character it is inside, which is the position a human would
    /// point at.
    pub(crate) fn to_wire_column(self, line: &str, column: u32) -> u32 {
        let column = column as usize;
        let mut offset = 0usize;
        for (index, character) in line.chars().enumerate() {
            let width = match self {
                Self::Utf8 => character.len_utf8(),
                Self::Utf16 => character.len_utf16(),
                Self::Utf32 => 1,
            };
            // The character this column starts at, or falls inside - the
            // second half of a surrogate pair, or the third byte of a `🦀`,
            // belongs to the character it is part of and not to the next one.
            if column < offset + width {
                return index as u32;
            }
            offset += width;
        }
        line.chars().count() as u32
    }
}

/// The text of one line of `source`, zero-based, or `""` for a line past its
/// end.
///
/// `""` rather than `None` because every caller's answer for a missing line
/// is the same as for an empty one - column zero - and a `None` here would
/// only be unwrapped back into it.
pub(crate) fn line_text(source: &str, line: u32) -> &str {
    source.split('\n').nth(line as usize).map(|text| text.trim_end_matches('\r')).unwrap_or("")
}

/// A `file:` URI for an absolute path.
fn encode(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Whether `text` begins with a Windows drive letter and its colon.
///
/// A plain `&str` test rather than `Path::components`, because every caller
/// here holds a path that came off the wire: on a non-Windows host `Path`
/// has no notion of a prefix at all, so asking it would answer "no" on the
/// very platform the question is being asked *about*.
fn starts_with_drive(text: &str) -> bool {
    let mut chars = text.chars();
    matches!((chars.next(), chars.next()), (Some(letter), Some(':')) if letter.is_ascii_alphabetic())
}

/// A Windows extended-length (`\\?\`) path in its ordinary spelling, or
/// `path` unchanged when it is not one.
///
/// `std::fs::canonicalize` returns that spelling on Windows and only there,
/// and it is not interchangeable with the ordinary one anywhere it matters
/// to this module: `Path::strip_prefix` compares a `Prefix::VerbatimDisk`
/// against a `Prefix::Disk` and finds them different, so a root kept in the
/// verbatim form matches nothing a language server ever reports - every
/// server on Windows says `file:///C:/…`. A URI built from one is worse
/// still: `file:////?/C:/…` names no file on any host.
///
/// A pure function over the text rather than a `#[cfg(windows)]` branch, so
/// that its Windows arm is exercisable from a Unix host - the same reason
/// `daemon::manifest::exe_suffixed` takes its suffix as an argument instead
/// of reading `EXE_SUFFIX` itself. On Unix nothing produces such a path, so
/// this is the identity there in practice as well as in principle.
pub(crate) fn without_verbatim_prefix(path: &Path) -> PathBuf {
    // A path this side cannot read as UTF-8 cannot carry the ASCII prefix
    // either, so leaving it alone is the whole of the right answer.
    let Some(text) = path.to_str() else { return path.to_path_buf() };
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{share}"));
    }
    match text.strip_prefix(r"\\?\") {
        // A drive is the only verbatim path with an ordinary spelling to fall
        // back to. `\\?\Volume{…}` and the rest of the device namespace have
        // none, and are left exactly as they are rather than mangled into
        // something that looks like a path and is not.
        Some(rest) if starts_with_drive(rest) => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// The `file:` URI naming `path`, which must be absolute.
///
/// Windows paths are spelled with forward slashes and keep their drive
/// letter's colon, which is why `:` is in the unreserved set above: a server
/// handed `file:///C%3A/x` would look for a drive named `C%3A`. An
/// extended-length path loses its `\\?\` first - see
/// [`without_verbatim_prefix`].
pub(crate) fn file_uri(path: &Path) -> String {
    let path = without_verbatim_prefix(path);
    let text = path.to_string_lossy().replace('\\', "/");
    let text = encode(&text);
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        // A Windows path (`C:/x`) has no leading slash of its own, and the
        // URI needs three.
        format!("file:///{text}")
    }
}

/// The path a `file:` URI names, or `None` for a URI naming something else -
/// `untitled:`, a server's synthetic `rust-analyzer:` document, anything with
/// an authority this side cannot reach.
pub(crate) fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path` is a share this process cannot read as a local path,
    // and `file:///path` leaves an empty authority, which is the normal form.
    let rest = rest.strip_prefix('/')?;
    let decoded = decode(rest)?;
    // `/home/x` came back as `home/x` when the leading slash was stripped;
    // `C:/x` is already whole. A drive letter is the only case where the
    // stripped slash was structural rather than part of the path.
    Some(if starts_with_drive(&decoded) {
        PathBuf::from(decoded)
    } else {
        PathBuf::from(format!("/{decoded}"))
    })
}

/// Percent-decoding, or `None` for an escape that is not one.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line that makes this module necessary: the three encodings
    /// disagree on it, and one of them is the wire's.
    const MIXED: &str = "let héllo = 🦀.field();";

    #[test]
    fn the_three_encodings_disagree_exactly_where_they_should() {
        // "let h" is 5 of everything; after `é` they part company.
        let field = MIXED.chars().position(|c| c == 'f').unwrap() as u32;
        assert_eq!(PositionEncoding::Utf32.from_wire_column(MIXED, field), field);
        assert_eq!(
            PositionEncoding::Utf16.from_wire_column(MIXED, field),
            field + 1,
            "the crab is two UTF-16 code units"
        );
        assert_eq!(
            PositionEncoding::Utf8.from_wire_column(MIXED, field),
            field + 4,
            "é is two bytes and the crab is four"
        );
    }

    #[test]
    fn every_encoding_round_trips_every_column_of_a_non_ascii_line() {
        for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16, PositionEncoding::Utf32] {
            for column in 0..=MIXED.chars().count() as u32 {
                let there = encoding.from_wire_column(MIXED, column);
                let back = encoding.to_wire_column(MIXED, there);
                assert_eq!(back, column, "{encoding:?} did not round-trip column {column}");
            }
        }
    }

    #[test]
    fn a_column_past_the_end_of_a_line_is_clamped_rather_than_extrapolated() {
        let end = MIXED.chars().count() as u32;
        assert_eq!(PositionEncoding::Utf16.from_wire_column(MIXED, 999), MIXED.encode_utf16().count() as u32);
        assert_eq!(PositionEncoding::Utf16.to_wire_column(MIXED, 999), end);
    }

    /// A server that answers a column inside a character (it disagrees with
    /// the encoding it negotiated) still maps onto that character.
    #[test]
    fn a_column_inside_a_character_rounds_down_to_it() {
        let crab = MIXED.chars().position(|c| c == '🦀').unwrap() as u32;
        let at_crab = PositionEncoding::Utf16.from_wire_column(MIXED, crab);
        assert_eq!(PositionEncoding::Utf16.to_wire_column(MIXED, at_crab + 1), crab);
    }

    #[test]
    fn a_server_that_names_no_encoding_is_read_as_utf16() {
        assert_eq!(PositionEncoding::from_capability(None), PositionEncoding::Utf16);
        assert_eq!(PositionEncoding::from_capability(Some("utf-16")), PositionEncoding::Utf16);
        assert_eq!(PositionEncoding::from_capability(Some("utf-8")), PositionEncoding::Utf8);
        assert_eq!(PositionEncoding::from_capability(Some("utf-32")), PositionEncoding::Utf32);
        assert_eq!(PositionEncoding::from_capability(Some("ebcdic")), PositionEncoding::Utf16);
    }

    #[test]
    fn lines_are_addressed_by_number_and_a_missing_one_is_empty() {
        let source = "one\r\ntwo\nthree";
        assert_eq!(line_text(source, 0), "one", "a CRLF line keeps neither");
        assert_eq!(line_text(source, 1), "two");
        assert_eq!(line_text(source, 2), "three");
        assert_eq!(line_text(source, 9), "");
    }

    #[test]
    fn a_path_round_trips_through_its_uri() {
        for path in [
            "/projects/thing/src/lib.rs",
            "/projects/a b/src/main.rs",
            "/projects/héllo/src/🦀.rs",
            "/projects/percent%20literal/x.rs",
        ] {
            let uri = file_uri(Path::new(path));
            assert!(!uri.contains(' '), "a URI never carries a raw space: {uri}");
            assert_eq!(path_from_uri(&uri).as_deref(), Some(Path::new(path)), "{uri}");
        }
    }

    #[test]
    fn a_windows_path_keeps_its_drive_letter() {
        let uri = file_uri(Path::new("C:\\projects\\thing\\src\\lib.rs"));
        assert_eq!(uri, "file:///C:/projects/thing/src/lib.rs");
        assert_eq!(path_from_uri(&uri).as_deref(), Some(Path::new("C:/projects/thing/src/lib.rs")));
    }

    /// The spelling `std::fs::canonicalize` hands back on Windows, and the
    /// only one no other layer here accepts - see
    /// [`without_verbatim_prefix`]. Written as literals so that the Windows
    /// arm of this decision is checked on every host, not only the one that
    /// can produce such a path.
    #[test]
    fn an_extended_length_windows_path_is_spelled_the_ordinary_way() {
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\C:\projects\thing")),
            PathBuf::from(r"C:\projects\thing")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\server\share\x.rs")),
            PathBuf::from(r"\\server\share\x.rs"),
            "a verbatim UNC path's ordinary spelling is the share, not a drive"
        );
        // Nothing to fall back to: the device namespace has no ordinary
        // spelling, and half-stripping it would invent one.
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\Volume{9f3b}\x.rs")),
            PathBuf::from(r"\\?\Volume{9f3b}\x.rs")
        );
        // The two spellings this function must never touch.
        assert_eq!(without_verbatim_prefix(Path::new(r"C:\a\b")), PathBuf::from(r"C:\a\b"));
        assert_eq!(without_verbatim_prefix(Path::new("/private/var/a")), PathBuf::from("/private/var/a"));
    }

    /// A URI built from a canonicalized Windows root is the one every server
    /// speaks - `file:///C:/…`, never `file:////?/C:/…`.
    #[test]
    fn an_extended_length_windows_path_gets_an_ordinary_file_uri() {
        let uri = file_uri(Path::new(r"\\?\C:\projects\thing\src\lib.rs"));
        assert_eq!(uri, "file:///C:/projects/thing/src/lib.rs");
        assert_eq!(path_from_uri(&uri).as_deref(), Some(Path::new("C:/projects/thing/src/lib.rs")));
    }

    #[test]
    fn a_uri_that_is_not_a_local_file_is_refused_rather_than_guessed() {
        assert_eq!(path_from_uri("untitled:Untitled-1"), None);
        // The scheme a server invents for a document that is not on disk -
        // every real one has one, and the bridge refuses them all alike.
        assert_eq!(path_from_uri("analyzer-synthetic://inlay-hints/x.toy"), None);
        assert_eq!(path_from_uri("file://server/share/x.rs"), None, "a UNC share is not a local path");
        assert_eq!(path_from_uri("file:///bad%2escape%ZZ"), None);
    }
}
