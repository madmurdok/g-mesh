package main

// The extractor. GM-279's own scope: a stub that emits exactly one node per
// file - a File node spanning the whole text - and no edges at all. Real Go
// declarations (functions, types, methods, their DEFINES/EXPORTS edges,
// containers) are GM-280; go/types semantics are GM-281. This file's whole
// job until then is to let `g-mesh plugin check` exercise the shape,
// stream-order, id-stability and ownership rules end to end with a minimal,
// honest extraction - not to parse Go source at all yet.

import (
	"bytes"
	"path"
)

// computeFileNode builds the sole node this scaffold's extractor ever
// emits for relPath.
//
// visibility is deliberately "file", not "public": this repo's one
// concrete example of a Go plugin's File node
// (core/tests/fixtures/valid_v2.ndjson) uses "file", matching the TS
// plugin's own convention (extract.ts's addNode: a node is "file"-visible
// unless something marks it exported, and nothing ever marks the File node
// itself exported - only what it *defines* can be public). See this
// repo's docs/architecture/multi-language-plugins.md, Go plugin section,
// "Implementation notes (GM-279)" for why this was chosen over the literal
// wording in GM-279's own task description.
func computeFileNode(relPath string, content []byte) wireNode {
	endLine, endCol := textEndPosition(content)
	return wireNode{
		ID:            nodeIDFor(relPath, "File", relPath, ""),
		Kind:          "File",
		Name:          path.Base(relPath),
		QualifiedName: relPath,
		FilePath:      relPath,
		Range: wireRange{
			Start: wirePosition{Line: 0, Col: 0},
			End:   wirePosition{Line: endLine, Col: endCol},
		},
		Visibility:      fileVisibility(),
		Language:        languageName,
		HasSyntaxErrors: false,
	}
}

// textEndPosition is this scaffold's substitute for a real parser's
// root-node end position (go/parser lands in GM-280) - and it must agree
// with what core/src/cli/plugin_check/session.rs's whitespace_edit
// actually does for that check to pass: endLine is the number of '\n'
// bytes in the file, endCol is the byte length of whatever text follows
// the last one (0 when the file ends in a newline). This is exactly
// tree-sitter's own root-node end position for the TS plugin - see that
// doc comment's own measurement, an 8-line file ending at (8, 0) - so a
// whitespace-only edit before the file's *last* newline (never after it)
// changes neither the newline count nor what follows the final one, and
// this function's answer is unchanged by it, which is what lets
// control.go's handleFileChanged answer that edit with an empty diff.
func textEndPosition(content []byte) (line, col int) {
	line = bytes.Count(content, []byte{'\n'})
	lastNewline := bytes.LastIndexByte(content, '\n')
	col = len(content) - (lastNewline + 1)
	return line, col
}
