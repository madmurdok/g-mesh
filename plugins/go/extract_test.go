package main

import "testing"

func TestTextEndPositionCountsNewlinesAndTrailingBytes(t *testing.T) {
	cases := []struct {
		name     string
		content  string
		wantLine int
		wantCol  int
	}{
		{"empty file", "", 0, 0},
		{"ends with newline", "package main\n", 1, 0},
		{"no trailing newline", "package main", 0, len("package main")},
		{"multi-line, trailing newline", "a\nb\nc\n", 3, 0},
		{"multi-line, no trailing newline", "a\nb\nc", 2, 1},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			line, col := textEndPosition([]byte(c.content))
			if line != c.wantLine || col != c.wantCol {
				t.Fatalf("textEndPosition(%q) = (%d, %d), want (%d, %d)", c.content, line, col, c.wantLine, c.wantCol)
			}
		})
	}
}

// The design doc's own worked example (docs/architecture/
// multi-language-plugins.md and core/tests/fixtures/valid_v2.ndjson): a
// 10-line main.go ends its File node's range at (10, 0).
func TestComputeFileNodeMatchesTheDesignDocsWorkedExample(t *testing.T) {
	content := []byte("package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hi\")\n}\n\n// trailer\n\n")
	node := computeFileNode("main.go", content)

	if node.Kind != "File" {
		t.Fatalf("kind = %q, want File", node.Kind)
	}
	if node.Name != "main.go" {
		t.Fatalf("name = %q, want main.go", node.Name)
	}
	if node.QualifiedName != "main.go" {
		t.Fatalf("qualifiedName = %q, want main.go", node.QualifiedName)
	}
	if node.FilePath != "main.go" {
		t.Fatalf("filePath = %q, want main.go", node.FilePath)
	}
	if node.Language != "go" {
		t.Fatalf("language = %q, want go", node.Language)
	}
	if node.HasSyntaxErrors {
		t.Fatal("hasSyntaxErrors must be false for this scaffold")
	}
	if node.Range.Start != (wirePosition{0, 0}) {
		t.Fatalf("range.start = %+v, want (0, 0)", node.Range.Start)
	}
	wantEndLine := 10
	if node.Range.End.Line != wantEndLine || node.Range.End.Col != 0 {
		t.Fatalf("range.end = %+v, want (%d, 0)", node.Range.End, wantEndLine)
	}

	// The design doc's own reference JSON serialization for a Go File
	// node (core/tests/fixtures/valid_v2.ndjson) uses `"visibility":"file"`.
	body, err := node.Visibility.MarshalJSON()
	if err != nil {
		t.Fatalf("MarshalJSON failed: %v", err)
	}
	if string(body) != `"file"` {
		t.Fatalf("visibility JSON = %s, want %q", body, `"file"`)
	}
}

// computeFileNode's id must depend only on the path (nodeIDFor's own
// contract - see ids.go), so two calls for the same relPath with
// different content must agree on the id, and only the range may differ.
func TestComputeFileNodeIDIsContentIndependent(t *testing.T) {
	a := computeFileNode("x.go", []byte("package x\n"))
	b := computeFileNode("x.go", []byte("package x\n\nfunc F() {}\n"))
	if a.ID != b.ID {
		t.Fatalf("ids differ across an edit: %q != %q", a.ID, b.ID)
	}
	if a.Range == b.Range {
		t.Fatal("ranges should differ - the file content is not the same length")
	}
}
