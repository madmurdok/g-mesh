package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"testing"
)

func TestRunBulkIndexEmitsOneNodePerGoFile(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "main.go", "package main\n\nfunc main() {}\n")
	writeFile(t, root, "internal/util.go", "package internal\n")
	writeFile(t, root, "vendor/dep/dep.go", "package dep\n")

	var out bytes.Buffer
	summary, err := runBulkIndex(root, &out)
	if err != nil {
		t.Fatalf("runBulkIndex failed: %v", err)
	}
	if summary.filesProcessed != 2 || summary.nodesEmitted != 2 {
		t.Fatalf("summary = %+v, want 2 files and 2 nodes (vendor/ excluded)", summary)
	}

	scanner := bufio.NewScanner(&out)
	seenPaths := map[string]bool{}
	for scanner.Scan() {
		var node wireNode
		if err := json.Unmarshal(scanner.Bytes(), &node); err != nil {
			t.Fatalf("emitted line did not parse as a wireNode: %v\nline: %s", err, scanner.Text())
		}
		if node.Kind != "File" {
			t.Errorf("kind = %q, want File", node.Kind)
		}
		if node.ID == "" {
			t.Error("node has no id")
		}
		seenPaths[node.FilePath] = true
	}
	if err := scanner.Err(); err != nil {
		t.Fatalf("scanner error: %v", err)
	}
	if !seenPaths["main.go"] || !seenPaths["internal/util.go"] {
		t.Fatalf("seenPaths = %v, want main.go and internal/util.go", seenPaths)
	}
}

// Two bulk runs over the same unmodified tree must emit exactly the same
// node ids - the plugin-side half of core's id-stability.bulk-repeat
// check (core/src/cli/plugin_check/checks.rs).
func TestRunBulkIndexIsIDStableAcrossRepeatedRuns(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n")
	writeFile(t, root, "b/b.go", "package b\n\nfunc F() {}\n")

	ids := func() []string {
		var out bytes.Buffer
		if _, err := runBulkIndex(root, &out); err != nil {
			t.Fatalf("runBulkIndex failed: %v", err)
		}
		var ids []string
		scanner := bufio.NewScanner(&out)
		for scanner.Scan() {
			var node wireNode
			if err := json.Unmarshal(scanner.Bytes(), &node); err != nil {
				t.Fatalf("bad line: %v", err)
			}
			ids = append(ids, node.ID)
		}
		return ids
	}

	first := ids()
	second := ids()
	if len(first) != 2 || len(second) != 2 {
		t.Fatalf("expected 2 nodes per run, got %d and %d", len(first), len(second))
	}
	for i := range first {
		if first[i] != second[i] {
			t.Fatalf("run 1 id %q != run 2 id %q at index %d", first[i], second[i], i)
		}
	}
}

// An empty project must not error and must emit nothing.
func TestRunBulkIndexOverAnEmptyProjectEmitsNothing(t *testing.T) {
	root := t.TempDir()
	var out bytes.Buffer
	summary, err := runBulkIndex(root, &out)
	if err != nil {
		t.Fatalf("runBulkIndex failed: %v", err)
	}
	if summary.filesProcessed != 0 || out.Len() != 0 {
		t.Fatalf("summary = %+v, out.Len() = %d, want an empty walk", summary, out.Len())
	}
}
