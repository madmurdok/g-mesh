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
	"sync"
	"time"
)

// bulkIndexFlag selects one-shot bulk-index mode instead of the
// control-plane loop; must stay in sync with core's
// daemon::bulk_index::BULK_INDEX_FLAG (core/src/daemon/bulk_index.rs) and
// plugins/typescript/src/index.ts's own copy of the same constant.
const bulkIndexFlag = "--bulk-index"

// bulkStdinLifelineEnv is set to "1" by core on every bulk spawn: stdin is
// then a pipe core holds open and never writes, and its EOF means core is
// gone (GM-397). Must stay in sync with core's
// daemon::bulk_index::BULK_STDIN_LIFELINE_ENV. Without it the bulk walk
// leaves stdin alone, so an older core or a hand run with `< /dev/null` is
// not read as "exit before walking".
const bulkStdinLifelineEnv = "G_MESH_BULK_STDIN_LIFELINE"

// lifelineGrace is how long the control-plane reader gives the main loop to
// take its graceful path after core closed stdin, before it ends the process
// itself - see runControlLoop.
const lifelineGrace = time.Second

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
		if os.Getenv(bulkStdinLifelineEnv) == "1" {
			go watchBulkLifeline(os.Stdin)
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

	runControlLoop(projectRoot, os.Stdin, os.Stdout, exitAfterLifelineGrace)
}

// watchBulkLifeline is the bulk walk's lifeline watcher (GM-397): it reads
// and discards stdin and ends the process the moment it reaches EOF. The
// walk never touches stdin, and it can go a long time without writing -
// loadWorkspace, walkProjectFiles, any stretch that fits in the 4 KiB
// output buffer - so without this a killed core goes unnoticed until the
// next write, or never, if the walk finishes first. A read error counts as
// EOF: with the variable set core promised a pipe, and one that cannot be
// read is not one core still holds. Exits 1, not 0: whatever is left to
// read this stream did not get a complete one.
func watchBulkLifeline(in io.Reader) {
	_, _ = io.Copy(io.Discard, in)
	logf("core closed the bulk stream's lifeline - exiting")
	os.Exit(1)
}

// exitAfterLifelineGrace is the control plane's last resort once core has
// closed stdin (GM-397): an idle loop returns on the EOF by itself well
// within lifelineGrace, so a process still alive after it is one stuck in a
// request. `go list` children of a semantic pass are not killed here; they
// finish, or die of SIGPIPE on their next write to this dead process.
func exitAfterLifelineGrace() {
	time.Sleep(lifelineGrace)
	logf("core closed the control stream mid-request - exiting")
	os.Exit(1)
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
//
// Frames are read on their own goroutine (readControlStream) rather than
// between requests, so EOF is noticed while a request is still being
// handled (GM-397): onClosed, when non-nil, runs on that goroutine once
// stdin has ended - main passes exitAfterLifelineGrace, tests pass nil.
func runControlLoop(projectRoot string, in io.Reader, out io.Writer, onClosed func()) {
	sendHandshake(out)

	frames := readControlStream(in, onClosed)
	state := newPluginState(projectRoot)
	for {
		body, err := frames.pop()
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

// frameResult is one readFrame outcome, in stream order.
type frameResult struct {
	body []byte
	err  error
}

// frameQueue is an unbounded FIFO between the reader goroutine and the
// control loop. Unbounded on purpose: a bounded channel would block the
// reader behind a busy loop, and a blocked reader cannot see EOF - the very
// thing it exists to notice.
type frameQueue struct {
	mu    sync.Mutex
	items []frameResult
	wake  chan struct{}
}

func (q *frameQueue) push(r frameResult) {
	q.mu.Lock()
	q.items = append(q.items, r)
	q.mu.Unlock()
	select {
	case q.wake <- struct{}{}:
	default:
	}
}

// pop blocks until a result is queued. The one-slot wake channel cannot lose
// a push that lands between the empty check and the receive: the token stays
// buffered until this receive takes it.
func (q *frameQueue) pop() ([]byte, error) {
	for {
		q.mu.Lock()
		if len(q.items) > 0 {
			r := q.items[0]
			q.items = q.items[1:]
			q.mu.Unlock()
			return r.body, r.err
		}
		q.mu.Unlock()
		<-q.wake
	}
}

// readControlStream reads frames from in on its own goroutine until EOF or
// a framing error, queueing each outcome in order. After a framing error it
// keeps draining in to EOF: the stream is desynchronized, but the loop may
// still be mid-request, and EOF is still the signal that core is gone. Once
// in has ended it calls onClosed, if set.
func readControlStream(in io.Reader, onClosed func()) *frameQueue {
	q := &frameQueue{wake: make(chan struct{}, 1)}
	go func() {
		reader := bufio.NewReader(in)
		for {
			body, err := readFrame(reader)
			q.push(frameResult{body: body, err: err})
			if err == nil {
				continue
			}
			if err != io.EOF {
				_, _ = io.Copy(io.Discard, reader)
			}
			if onClosed != nil {
				onClosed()
			}
			return
		}
	}()
	return q
}
