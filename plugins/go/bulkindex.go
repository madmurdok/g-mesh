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
}

// runBulkIndex walks root (walkProjectFiles - gitignore-aware, symlink-
// guarded, honoring exclude_dirs) and writes each matched file's node(s) to
// out as compact NDJSON, one line per node. A file this plugin can no
// longer read (vanished between the walk and the read) is skipped, not
// fatal - matching bulkIndexProject's own "a corrupt/vanished file must not
// abort the whole bulk index" rule.
func runBulkIndex(root string, out io.Writer) (bulkIndexSummary, error) {
	var summary bulkIndexSummary

	w := bufio.NewWriter(out)
	for _, relPath := range walkProjectFiles(root) {
		abs := filepath.Join(root, filepath.FromSlash(relPath))
		content, err := os.ReadFile(abs)
		if err != nil {
			continue
		}

		node := computeFileNode(relPath, content)
		line, err := json.Marshal(node)
		if err != nil {
			// Unreachable outside a bug in wire.go's MarshalJSON - every
			// field here is a plain string/int/bool. Skipping rather than
			// aborting the walk keeps this consistent with the read-error
			// case above.
			continue
		}
		if _, err := w.Write(line); err != nil {
			return summary, err
		}
		if err := w.WriteByte('\n'); err != nil {
			return summary, err
		}

		summary.filesProcessed++
		summary.nodesEmitted++
	}

	return summary, w.Flush()
}
