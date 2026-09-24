package main

// The one-shot `--bulk-index <root>` cold-start walk: streams NDJSON nodes
// (and, once GM-280 exists, edges) to stdout, one file at a time, then lets
// the process exit. Mirrors plugins/typescript/src/bulkIndex.ts's
// bulkIndexProject - its own doc comment explains why this is a *separate*
// process rather than a control-plane method (an open-ended stream has no
// place in a framed request/response protocol), which is exactly why
// main.go dispatches `--bulk-index` before ever touching the control loop.

import (
	"bufio"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
)

type bulkIndexSummary struct {
	filesProcessed int
	nodesEmitted   int
	edgesEmitted   int
}

// runBulkIndex walks root (walkProjectFiles - gitignore-aware, symlink-
// guarded, honoring exclude_dirs) and writes each matched file's graph to
// out as compact NDJSON, one line per node or edge. A file this plugin can
// no longer read (vanished between the walk and the read) is skipped, not
// fatal - matching bulkIndexProject's own "a corrupt/vanished file must not
// abort the whole bulk index" rule.
//
// # Line order is the contract, not a formatting choice
//
// All of a file's nodes are written before any of its edges, and a file's
// nodes and edges are never interleaved with another file's. That is what
// the kit's `stream-order` check enforces and what lets
// `daemon::bulk_index` cut the stream at any line: an edge is a foreign key
// onto two nodes, so it may only lean on what an earlier line already
// delivered, and extractFile guarantees every edge of a file points at two
// nodes of that same file.
//
// The workspace is loaded once for the whole walk rather than per file: it
// is the same answer for every file in the tree, and reading every go.mod in
// the project once per file would turn a linear walk into a quadratic one.
func runBulkIndex(root string, out io.Writer) (bulkIndexSummary, error) {
	var summary bulkIndexSummary

	ws := loadWorkspace(root)
	// Test-only (GM-397): parks the walk after the workspace load and before
	// the first write - the silent stretch in which a killed core goes
	// unnoticed. See hold.go.
	holdPoint("bulk")
	w := bufio.NewWriter(out)
	write := func(value interface{}) error {
		line, err := json.Marshal(value)
		if err != nil {
			// Unreachable outside a bug in wire.go's marshalling - every
			// field is a plain string/int/bool or a small struct of them.
			return nil
		}
		if _, err := w.Write(line); err != nil {
			return err
		}
		return w.WriteByte('\n')
	}

	for _, relPath := range walkProjectFiles(root) {
		abs := filepath.Join(root, filepath.FromSlash(relPath))
		content, err := os.ReadFile(abs)
		if err != nil {
			continue
		}

		graph := extractFile(ws, relPath, content)
		for _, node := range graph.nodes {
			if err := write(node); err != nil {
				return summary, err
			}
			summary.nodesEmitted++
		}
		for _, edge := range graph.edges {
			if err := write(edge); err != nil {
				return summary, err
			}
			summary.edgesEmitted++
		}
		summary.filesProcessed++
	}

	return summary, w.Flush()
}
