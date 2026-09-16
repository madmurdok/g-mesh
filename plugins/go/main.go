// Command g-mesh-plugin-go is g-mesh's bundled Go language plugin: a
// Content-Length-framed JSON-RPC control loop plus a one-shot `--bulk-index`
// NDJSON walk (GM-279), speaking wire protocol v2
// (core/src/protocol/types.rs), over a `go/parser` structural extractor
// (GM-280) that produces real declarations, containers, visibility and
// edges. The semantic tier - `go/types` through
// golang.org/x/tools/go/packages, which is what answers the receiver calls
// this one records as open sites - is GM-281, and until it lands every
// `semanticPass` is answered honestly with an empty diff.
//
// See docs/architecture/multi-language-plugins.md's "Go plugin" section,
// and its "Implementation notes (GM-279)" and "(GM-280)" subsections, for
// the design this implements and the decisions each task had to settle
// rather than infer.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"os"
)

// bulkIndexFlag selects one-shot bulk-index mode instead of the
// control-plane loop; must stay in sync with core's
// daemon::bulk_index::BULK_INDEX_FLAG (core/src/daemon/bulk_index.rs) and
// plugins/typescript/src/index.ts's own copy of the same constant.
const bulkIndexFlag = "--bulk-index"

func logf(format string, args ...interface{}) {
	fmt.Fprintf(os.Stderr, "[g-mesh-go] "+format+"\n", args...)
}

func main() {
	args := os.Args[1:]

	if len(args) > 0 && args[0] == bulkIndexFlag {
		root := "."
		if len(args) > 1 {
			root = args[1]
		}
		summary, err := runBulkIndex(root, os.Stdout)
		if err != nil {
			logf("bulk index failed: %v", err)
			os.Exit(1)
		}
		logf("bulk index complete: %d file(s), %d node(s), %d edge(s)",
			summary.filesProcessed, summary.nodesEmitted, summary.edgesEmitted)
		return
	}

	// Nothing else core sends carries a project root (control messages
	// only ever carry a file path), so the plugin has to learn it at
	// startup - core passes it as this process's one positional argument
	// (daemon::plugin::PluginProcess::spawn), mirrored here exactly the
	// way index.ts reads it, falling back to the working directory so a
	// bare manual run still works.
	projectRoot := "."
	if len(args) > 0 {
		projectRoot = args[0]
	}

	runControlLoop(projectRoot, os.Stdin, os.Stdout)
}

func sendHandshake(out io.Writer) {
	body, err := json.Marshal(handshakeMessage{
		ProtocolVersion: protocolVersion,
		Language:        languageName,
		PluginVersion:   pluginVersion,
	})
	if err != nil {
		logf("failed to encode handshake: %v", err)
		return
	}
	if err := writeFrame(out, body); err != nil {
		logf("failed to write handshake: %v", err)
	}
}

// runControlLoop is the plugin's long-lived half: handshake, then one
// framed JSON-RPC request or notification at a time until stdin reaches
// EOF - core's own signal to exit (daemon::plugin::PluginProcess::shutdown
// closes the plugin's stdin rather than sending a message), mirrored here
// exactly the way plugins/typescript/src/index.ts's
// `process.stdin.on("end", ...)` does. A framing error desynchronizes the
// stream (jsonrpc.go's readFrame's own doc comment) and ends the loop; a
// malformed envelope or an unrecognized method does not - see
// handleEnvelope.
func runControlLoop(projectRoot string, in io.Reader, out io.Writer) {
	sendHandshake(out)

	reader := bufio.NewReader(in)
	state := newPluginState(projectRoot)
	for {
		body, err := readFrame(reader)
		if err != nil {
			if err == io.EOF {
				return // clean shutdown - core closed our stdin
			}
			logf("framing error: %v", err)
			return
		}

		var env controlEnvelope
		if err := json.Unmarshal(body, &env); err != nil {
			logf("malformed control message JSON: %v", err)
			continue
		}
		handleEnvelope(state, env, out)
	}
}
