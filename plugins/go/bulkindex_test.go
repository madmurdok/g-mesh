package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"testing"
)

func TestRunBulkIndexEmitsEveryGoFilesGraph(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "main.go", "package main\n\nfunc main() {}\n")
	writeFile(t, root, "internal/util.go", "package internal\n")
	writeFile(t, root, "vendor/dep/dep.go", "package dep\n")

	var out bytes.Buffer
	summary, err := runBulkIndex(root, &out)
	if err != nil {
		t.Fatalf("runBulkIndex failed: %v", err)
	}
	// main.go: a File node plus `main`, and the DEFINES edge between them
	// (`main` is not capitalized, so there is no EXPORTS edge).
	// internal/util.go: a File node and nothing else. vendor/ is excluded.
	if summary.filesProcessed != 2 || summary.nodesEmitted != 3 || summary.edgesEmitted != 1 {
		t.Fatalf("summary = %+v, want 2 files, 3 nodes, 1 edge (vendor/ excluded)", summary)
	}

	scanner := bufio.NewScanner(&out)
	seenPaths := map[string]bool{}
	kinds := map[string]int{}
	for scanner.Scan() {
		var record map[string]json.RawMessage
		if err := json.Unmarshal(scanner.Bytes(), &record); err != nil {
			t.Fatalf("emitted line did not parse as JSON: %v\nline: %s", err, scanner.Text())
		}
		if _, isEdge := record["fromId"]; isEdge {
			var edge wireEdge
			if err := json.Unmarshal(scanner.Bytes(), &edge); err != nil {
				t.Fatalf("edge line did not parse as a wireEdge: %v", err)
			}
			kinds["edge:"+edge.Kind]++
			continue
		}
		var node wireNode
		if err := json.Unmarshal(scanner.Bytes(), &node); err != nil {
			t.Fatalf("node line did not parse as a wireNode: %v\nline: %s", err, scanner.Text())
		}
		if node.ID == "" {
			t.Error("node has no id")
		}
		kinds["node:"+node.Kind]++
		seenPaths[node.FilePath] = true
	}
	if err := scanner.Err(); err != nil {
		t.Fatalf("scanner error: %v", err)
	}
	if !seenPaths["main.go"] || !seenPaths["internal/util.go"] {
		t.Fatalf("seenPaths = %v, want main.go and internal/util.go", seenPaths)
	}
	if kinds["node:File"] != 2 || kinds["node:Function"] != 1 || kinds["edge:DEFINES"] != 1 {
		t.Fatalf("kinds = %v, want 2 File nodes, 1 Function node and 1 DEFINES edge", kinds)
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
			// Every record, node and edge alike, carries an `id` - and both
			// have to be stable, since core keys upserts and deletes on
			// them either way.
			var record struct {
				ID string `json:"id"`
			}
			if err := json.Unmarshal(scanner.Bytes(), &record); err != nil {
				t.Fatalf("bad line: %v", err)
			}
			ids = append(ids, record.ID)
		}
		return ids
	}

	first := ids()
	second := ids()
	// a.go: one File node. b/b.go: a File node, `F`, and the DEFINES and
	// EXPORTS edges from the one to the other - five lines in all.
	if len(first) != 5 || len(second) != 5 {
		t.Fatalf("expected 5 records per run, got %d and %d", len(first), len(second))
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
