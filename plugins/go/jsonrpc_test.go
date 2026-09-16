package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"testing"
)

// coreFixture reads one of core's own golden framing/NDJSON fixtures
// (core/tests/fixtures/*.rpc) - the ground truth this plugin's framing
// must round-trip against, generated independently of this package so a
// bug shared between "how this test builds a frame" and "how this package
// reads one" cannot hide here. plugins/go's module root is two directories
// below the repo root (<repo>/plugins/go), so ../../core/tests/fixtures is
// <repo>/core/tests/fixtures.
func coreFixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("..", "..", "core", "tests", "fixtures", name))
	if err != nil {
		t.Fatalf("failed to read core's golden fixture %s: %v", name, err)
	}
	return data
}

// valid_control.rpc is core's own golden framing of a `reindex` request:
// `Content-Length: 80\r\n\r\n{"jsonrpc":"2.0","id":1,"method":"reindex","params":{"filePath":"src/index.ts"}}`
// (core/src/protocol/jsonrpc.rs's own tests build the equivalent by hand;
// this is the literal bytes core's own test suite ships).
func TestReadFrameParsesCoresGoldenValidControlFixture(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader(coreFixture(t, "valid_control.rpc")))
	body, err := readFrame(reader)
	if err != nil {
		t.Fatalf("readFrame failed on a golden fixture: %v", err)
	}

	var env controlEnvelope
	if err := json.Unmarshal(body, &env); err != nil {
		t.Fatalf("the frame body did not parse as a controlEnvelope: %v", err)
	}
	if env.Method != "reindex" {
		t.Fatalf("method = %q, want %q", env.Method, "reindex")
	}
	var params filePathParams
	if err := json.Unmarshal(env.Params, &params); err != nil {
		t.Fatalf("params did not parse: %v", err)
	}
	if params.FilePath != "src/index.ts" {
		t.Fatalf("filePath = %q, want %q", params.FilePath, "src/index.ts")
	}

	// A clean EOF right after the one frame must come back as io.EOF, not
	// an error - matching core's own read_frame's `Ok(None)`.
	if _, err := readFrame(reader); err != io.EOF {
		t.Fatalf("expected io.EOF at the end of a single-frame stream, got %v", err)
	}
}

// semantic_pass_request.rpc: a `semanticPass` request with a one-entry
// filePaths list - the per-file shape, as against the whole-project empty
// list.
func TestReadFrameParsesCoresGoldenSemanticPassRequestFixture(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader(coreFixture(t, "semantic_pass_request.rpc")))
	body, err := readFrame(reader)
	if err != nil {
		t.Fatalf("readFrame failed: %v", err)
	}

	var env controlEnvelope
	if err := json.Unmarshal(body, &env); err != nil {
		t.Fatalf("did not parse as a controlEnvelope: %v", err)
	}
	if env.Method != "semanticPass" {
		t.Fatalf("method = %q, want %q", env.Method, "semanticPass")
	}
	var params filePathsParams
	if err := json.Unmarshal(env.Params, &params); err != nil {
		t.Fatalf("params did not parse: %v", err)
	}
	if len(params.FilePaths) != 1 || params.FilePaths[0] != "src/a.ts" {
		t.Fatalf("filePaths = %v, want [src/a.ts]", params.FilePaths)
	}
}

// semantic_pass_upgrade.rpc: core's golden shape of a *response* this
// plugin would send back to a semanticPass request - read it back as a
// fileChangeResponse to confirm this package's own response struct
// deserializes core's own wire bytes, not just the other way around.
func TestReadFrameParsesCoresGoldenSemanticPassUpgradeFixture(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader(coreFixture(t, "semantic_pass_upgrade.rpc")))
	body, err := readFrame(reader)
	if err != nil {
		t.Fatalf("readFrame failed: %v", err)
	}

	var resp fileChangeResponse
	if err := json.Unmarshal(body, &resp); err != nil {
		t.Fatalf("did not parse as a fileChangeResponse: %v", err)
	}
	if len(resp.Result.UpsertEdges) != 1 {
		t.Fatalf("upsertEdges = %v, want exactly one edge", resp.Result.UpsertEdges)
	}
	edge := resp.Result.UpsertEdges[0]
	if edge.ID != "e1" || edge.FromID != "n1" || edge.ToID != "n2" || edge.Kind != "CALLS" ||
		edge.Source != "semantic" || edge.Engine != "ts-compiler" || !edge.Resolved {
		t.Fatalf("unexpected edge: %+v", edge)
	}
}

// broken_framing.rpc: `Content-Length nope\r\n\r\n{}` - a header line with
// no colon at all. Must be reported as an error, never a panic and never a
// silently-accepted frame.
func TestReadFrameRejectsCoresGoldenBrokenFramingFixture(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader(coreFixture(t, "broken_framing.rpc")))
	if _, err := readFrame(reader); err == nil {
		t.Fatal("expected an error reading a malformed frame header, got nil")
	}
}

// Matches jsonrpc.rs's own written_frame_uses_lsp_wire_format test
// byte-for-byte, so the two implementations are checked against the same
// expectation rather than each against its own idea of the format.
func TestWriteFrameUsesLSPWireFormat(t *testing.T) {
	var buf bytes.Buffer
	if err := writeFrame(&buf, []byte(`{"ok":true}`)); err != nil {
		t.Fatalf("writeFrame failed: %v", err)
	}
	want := "Content-Length: 11\r\n\r\n{\"ok\":true}"
	if buf.String() != want {
		t.Fatalf("writeFrame output = %q, want %q", buf.String(), want)
	}
}

// A round trip through this package's own writeFrame/readFrame must
// reproduce the original body exactly, including for a body containing
// multi-byte UTF-8 (Content-Length counts bytes, not runes).
func TestWriteFrameThenReadFrameRoundTrips(t *testing.T) {
	body := []byte(`{"jsonrpc":"2.0","id":7,"method":"fileChanged","params":{"filePath":"δ/文件.go"}}`)
	var buf bytes.Buffer
	if err := writeFrame(&buf, body); err != nil {
		t.Fatalf("writeFrame failed: %v", err)
	}

	got, err := readFrame(bufio.NewReader(&buf))
	if err != nil {
		t.Fatalf("readFrame failed: %v", err)
	}
	if !bytes.Equal(got, body) {
		t.Fatalf("round-tripped body = %q, want %q", got, body)
	}
}

// A frame split across many small reads must still reassemble correctly -
// mirroring jsonrpc.rs's frame_split_across_reads_is_reassembled test,
// since a real os.Stdin pipe delivers bytes in whatever chunks the OS
// happens to hand over, not necessarily one frame at a time.
func TestReadFrameReassemblesAChunkedStream(t *testing.T) {
	var full bytes.Buffer
	if err := writeFrame(&full, []byte(`{"a":1}`)); err != nil {
		t.Fatalf("writeFrame failed: %v", err)
	}

	reader := bufio.NewReaderSize(&chunkedReader{data: full.Bytes(), chunk: 3}, 8)
	got, err := readFrame(reader)
	if err != nil {
		t.Fatalf("readFrame failed on a chunked stream: %v", err)
	}
	if string(got) != `{"a":1}` {
		t.Fatalf("got %q, want %q", got, `{"a":1}`)
	}
}

// chunkedReader hands back at most `chunk` bytes per Read call, forcing
// bufio.Reader to buffer across calls instead of seeing a whole frame at
// once - the Go analog of jsonrpc.rs's own test-only ChunkedReader.
type chunkedReader struct {
	data  []byte
	pos   int
	chunk int
}

func (c *chunkedReader) Read(p []byte) (int, error) {
	if c.pos >= len(c.data) {
		return 0, io.EOF
	}
	remaining := c.data[c.pos:]
	n := len(remaining)
	if n > len(p) {
		n = len(p)
	}
	if n > c.chunk {
		n = c.chunk
	}
	copy(p, remaining[:n])
	c.pos += n
	return n, nil
}

// A stream with nothing at all (immediate EOF, no bytes ever) must be
// io.EOF, not an error - the "peer closed between messages" case, distinct
// from EOF mid-frame.
func TestReadFrameCleanEOFAtBoundaryIsNotAnError(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader(nil))
	if _, err := readFrame(reader); err != io.EOF {
		t.Fatalf("expected io.EOF, got %v", err)
	}
}

// EOF *inside* a frame header (a Content-Length line with no terminating
// blank line, then nothing) must be a real error, not io.EOF - core's own
// read_frame draws this same distinction (`bail!("unexpected EOF inside
// frame header")` vs. `Ok(None)`).
func TestReadFrameEOFInsideHeaderIsAnError(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader([]byte("Content-Length: 2\r\n")))
	_, err := readFrame(reader)
	if err == nil || err == io.EOF {
		t.Fatalf("expected a non-EOF error for a truncated header, got %v", err)
	}
}

// A body shorter than its announced Content-Length must be an error.
func TestReadFrameShortBodyIsAnError(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader([]byte("Content-Length: 64\r\n\r\n{}")))
	if _, err := readFrame(reader); err == nil {
		t.Fatal("expected an error for a body shorter than announced, got nil")
	}
}

// A header block with no Content-Length at all must be an error.
func TestReadFrameMissingContentLengthIsAnError(t *testing.T) {
	reader := bufio.NewReader(bytes.NewReader([]byte("\r\n{}")))
	if _, err := readFrame(reader); err == nil {
		t.Fatal("expected an error for a missing Content-Length, got nil")
	}
}

// A header other than Content-Length must be tolerated, not just ignored
// silently by accident - matches jsonrpc.rs's
// headers_other_than_content_length_are_ignored test.
func TestReadFrameIgnoresOtherHeaders(t *testing.T) {
	raw := "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}"
	reader := bufio.NewReader(bytes.NewReader([]byte(raw)))
	body, err := readFrame(reader)
	if err != nil {
		t.Fatalf("readFrame failed: %v", err)
	}
	if string(body) != "{}" {
		t.Fatalf("body = %q, want %q", body, "{}")
	}
}
