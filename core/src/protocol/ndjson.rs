use std::io::BufRead;

use anyhow::{anyhow, Context, Result};

use crate::protocol::types::{WireEdge, WireNode};

#[derive(Debug, Clone, PartialEq)]
pub enum BulkItem {
    Node(WireNode),
    Edge(WireEdge),
}

impl BulkItem {
    /// WireNode and WireEdge have disjoint required fields (name/qualifiedName/
    /// filePath/... vs fromId/toId/...), so trying one then the other
    /// distinguishes them without a wire-level discriminator field.
    pub fn parse(line: &str) -> Result<Self> {
        if let Ok(node) = serde_json::from_str::<WireNode>(line) {
            return Ok(BulkItem::Node(node));
        }
        serde_json::from_str::<WireEdge>(line)
            .map(BulkItem::Edge)
            .with_context(|| format!("malformed NDJSON line (neither a valid node nor edge): {line:?}"))
    }
}

/// Streams NDJSON bulk-transfer lines one at a time, never buffering more
/// than the current line - `read_until` grows its buffer as needed, so
/// there's no fixed line-length cap. A malformed line yields `Some(Err(_))`
/// without stopping the stream; the caller decides whether to keep
/// consuming afterward. Trailing `\r` (Windows-originated plugin output) is
/// stripped before parsing.
pub struct NdjsonReader<R> {
    reader: R,
}

impl<R: BufRead> NdjsonReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }
}

impl<R: BufRead> Iterator for NdjsonReader<R> {
    type Item = Result<BulkItem>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut line = Vec::new();
            match self.reader.read_until(b'\n', &mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    let text = match std::str::from_utf8(&line) {
                        Ok(t) => t,
                        Err(e) => return Some(Err(anyhow!("NDJSON line is not valid UTF-8: {e}"))),
                    };
                    return Some(BulkItem::parse(text));
                }
                Err(e) => return Some(Err(anyhow::Error::new(e).context("failed to read NDJSON line"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{EdgeKind, EdgeSource, NodeKind, Position, Range};
    use std::io::{BufReader, Cursor, Read};

    fn node_json(id: &str) -> String {
        serde_json::to_string(&WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: id.to_string(),
            qualified_name: format!("m::{id}"),
            file_path: "src/lib.rs".to_string(),
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 2, col: 0 } },
            signature: None,
            exported: false,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
        })
        .unwrap()
    }

    fn edge_json(id: &str, from: &str, to: &str) -> String {
        serde_json::to_string(&WireEdge {
            id: id.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: EdgeKind::Calls,
            source: EdgeSource::TreeSitter,
            resolved: false,
            to_declaration: None,
        })
        .unwrap()
    }

    /// Hands back at most `chunk` bytes per `read` call, forcing `read_until`
    /// to loop across multiple partial reads instead of seeing a whole line
    /// (or even a whole stream) at once.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len()).min(self.chunk);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn parses_mixed_node_and_edge_lines_including_crlf() {
        let stream = format!(
            "{}\r\n{}\n{}\n",
            node_json("n1"),
            edge_json("e1", "n1", "n2"),
            node_json("n2")
        );
        let reader = NdjsonReader::new(Cursor::new(stream.into_bytes()));
        let items: Vec<BulkItem> = reader.map(|r| r.unwrap()).collect();

        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], BulkItem::Node(n) if n.id == "n1"));
        assert!(matches!(&items[1], BulkItem::Edge(e) if e.id == "e1"));
        assert!(matches!(&items[2], BulkItem::Node(n) if n.id == "n2"));
    }

    #[test]
    fn malformed_line_is_reported_without_losing_the_rest_of_the_batch() {
        let stream = format!("{}\nnot valid json at all\n{}\n", node_json("n1"), node_json("n2"));
        let reader = NdjsonReader::new(Cursor::new(stream.into_bytes()));
        let results: Vec<Result<BulkItem>> = reader.collect();

        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok(), "a bad line must not abort the lines after it");
        assert!(matches!(results[0].as_ref().unwrap(), BulkItem::Node(n) if n.id == "n1"));
        assert!(matches!(results[2].as_ref().unwrap(), BulkItem::Node(n) if n.id == "n2"));
    }

    #[test]
    fn lines_split_across_partial_reads_are_reassembled() {
        let stream = format!("{}\n{}\n", node_json("n1"), edge_json("e1", "n1", "n2"));
        let inner = ChunkedReader { data: stream.into_bytes(), pos: 0, chunk: 5 };
        let reader = NdjsonReader::new(BufReader::with_capacity(8, inner));
        let items: Vec<BulkItem> = reader.map(|r| r.unwrap()).collect();

        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], BulkItem::Node(n) if n.id == "n1"));
        assert!(matches!(&items[1], BulkItem::Edge(e) if e.id == "e1"));
    }
}
