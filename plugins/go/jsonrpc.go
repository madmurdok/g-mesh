package main

// LSP-style Content-Length framing, wire-compatible with core's
// core/src/protocol/jsonrpc.rs and plugins/typescript/src/jsonrpc.ts - see
// both for the exact contract this mirrors. Header lines are newline
// terminated (a trailing '\r' is tolerated), a blank line ends the header
// block, and the body is exactly the announced number of bytes, with no
// trailing newline of its own.

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io"
	"strconv"
	"strings"
)

const contentLengthHeader = "content-length"

// writeFrame writes one Content-Length-framed body, matching
// jsonrpc.rs's write_frame byte-for-byte (a single header line, a blank
// line, then the body - no trailing newline). One buffered write rather
// than two separate ones, so a frame can never be interleaved with another
// goroutine's write to the same stream - moot for this single-threaded
// plugin today, but it costs nothing and matches the Rust/TS
// implementations' own single-write shape.
func writeFrame(w io.Writer, body []byte) error {
	var buf bytes.Buffer
	buf.WriteString("Content-Length: ")
	buf.WriteString(strconv.Itoa(len(body)))
	buf.WriteString("\r\n\r\n")
	buf.Write(body)
	_, err := w.Write(buf.Bytes())
	return err
}

// readFrame reads exactly one frame, returning `io.EOF` (unwrapped, so a
// caller checks with `errors.Is(err, io.EOF)` or `err == io.EOF`) when the
// peer closed the stream cleanly at a frame boundary - matching core's
// read_frame's `Ok(None)`. Any other error leaves the stream
// desynchronized: the caller must not call readFrame again on the same
// reader.
func readFrame(r *bufio.Reader) ([]byte, error) {
	contentLength := -1
	started := false

	for {
		line, err := r.ReadString('\n')
		if len(line) == 0 {
			if errors.Is(err, io.EOF) {
				if !started {
					return nil, io.EOF
				}
				return nil, errors.New("unexpected EOF inside frame header")
			}
			return nil, err
		}
		started = true

		trimmed := strings.TrimSuffix(strings.TrimSuffix(line, "\n"), "\r")
		if trimmed == "" {
			// A blank "line" only reaches here when ReadString actually
			// found the '\n' delimiter (err == nil): that is the header
			// block's real terminator. If instead the stream ended right
			// after a bare '\r' or nothing at all with no delimiter ever
			// found, err is io.EOF here and this is a truncated frame, not
			// a legitimate terminator.
			if errors.Is(err, io.EOF) {
				return nil, errors.New("unexpected EOF inside frame header")
			}
			break
		}

		colon := strings.IndexByte(trimmed, ':')
		if colon < 0 {
			return nil, fmt.Errorf("malformed frame header line: %q", trimmed)
		}
		name := strings.TrimSpace(trimmed[:colon])
		value := strings.TrimSpace(trimmed[colon+1:])
		if strings.EqualFold(name, contentLengthHeader) {
			n, convErr := strconv.Atoi(value)
			if convErr != nil || n < 0 {
				return nil, fmt.Errorf("invalid Content-Length value: %q", value)
			}
			contentLength = n
		}

		if errors.Is(err, io.EOF) {
			// This header line's own bytes were real and already parsed
			// above, but the stream ended without ever reaching a blank
			// terminator line - a truncated frame.
			return nil, errors.New("unexpected EOF inside frame header")
		}
	}

	if contentLength < 0 {
		return nil, errors.New("frame header is missing Content-Length")
	}

	body := make([]byte, contentLength)
	if _, err := io.ReadFull(r, body); err != nil {
		return nil, fmt.Errorf("failed to read %d-byte frame body: %w", contentLength, err)
	}
	return body, nil
}
