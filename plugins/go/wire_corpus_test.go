package main

import (
	"encoding/json"
	"os"
	"reflect"
	"testing"
)

// testdata/wire.json is written by wire/tests/go_corpus.rs from the wire
// crate's types, which also asserts the file is current. Each frame is decoded
// into this plugin's own type and encoded again; the result must equal the
// frame once the keys in goIgnores are removed from it, so a field renamed or
// added on either side fails here.
//
// goIgnores lists the keys this plugin never emits or reads. Every other key
// of a Rust frame has to survive the round trip.
var goIgnores = map[string]bool{
	"declarations":  true,
	"aliasPaths":    true,
	"untypedCalls":  true,
	"alsoScopes":    true,
	"toDeclaration": true,
	"linkedEdges":   true,
	"budgetMs":      true,
	"watchPaths":    true,
	"affected":      true,
}

// goIgnoredMethods are control methods this plugin does not declare.
var goIgnoredMethods = map[string]bool{"prepareSemanticPass": true}

type wireCorpusEntry struct {
	Name  string          `json:"name"`
	Frame json.RawMessage `json:"frame"`
}

type wireCorpus struct {
	Nodes     []wireCorpusEntry `json:"nodes"`
	Edges     []wireCorpusEntry `json:"edges"`
	Trailers  []wireCorpusEntry `json:"trailers"`
	Requests  []wireCorpusEntry `json:"requests"`
	Responses []wireCorpusEntry `json:"responses"`
	Errors    []wireCorpusEntry `json:"errors"`
}

func loadWireCorpus(t *testing.T) wireCorpus {
	t.Helper()
	data, err := os.ReadFile("testdata/wire.json")
	if err != nil {
		t.Fatalf("read corpus: %v", err)
	}
	var corpus wireCorpus
	if err := json.Unmarshal(data, &corpus); err != nil {
		t.Fatalf("parse corpus: %v", err)
	}
	for name, entries := range map[string][]wireCorpusEntry{
		"nodes": corpus.Nodes, "edges": corpus.Edges, "trailers": corpus.Trailers,
		"requests": corpus.Requests, "responses": corpus.Responses, "errors": corpus.Errors,
	} {
		if len(entries) == 0 {
			t.Fatalf("corpus section %q is empty", name)
		}
	}
	return corpus
}

func withoutGoIgnores(value any) any {
	switch v := value.(type) {
	case map[string]any:
		out := make(map[string]any, len(v))
		for key, inner := range v {
			if !goIgnores[key] {
				out[key] = withoutGoIgnores(inner)
			}
		}
		return out
	case []any:
		out := make([]any, len(v))
		for i, inner := range v {
			out[i] = withoutGoIgnores(inner)
		}
		return out
	default:
		return v
	}
}

func asJSONValue(t *testing.T, data []byte) any {
	t.Helper()
	var value any
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatalf("not JSON: %v: %s", err, data)
	}
	return value
}

// roundTrip decodes frame into target, encodes target back and compares it
// with the frame minus goIgnores.
func roundTrip(t *testing.T, name string, frame json.RawMessage, target any) {
	t.Helper()
	if err := json.Unmarshal(frame, target); err != nil {
		t.Errorf("%s: decode into %T: %v", name, target, err)
		return
	}
	encoded, err := json.Marshal(target)
	if err != nil {
		t.Errorf("%s: encode %T: %v", name, target, err)
		return
	}
	want := withoutGoIgnores(asJSONValue(t, frame))
	got := asJSONValue(t, encoded)
	if !reflect.DeepEqual(got, want) {
		t.Errorf("%s: %T round trip differs\n  got:  %s\n  want: %s", name, target, encoded, mustMarshal(t, want))
	}
}

func mustMarshal(t *testing.T, value any) []byte {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func TestWireNodesRoundTripThroughTheCorpus(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Nodes {
		roundTrip(t, "node "+entry.Name, entry.Frame, &wireNode{})
	}
}

func TestWireEdgesRoundTripThroughTheCorpus(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Edges {
		roundTrip(t, "edge "+entry.Name, entry.Frame, &wireEdge{})
	}
}

func TestBulkTrailerRoundTripsThroughTheCorpus(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Trailers {
		roundTrip(t, "trailer "+entry.Name, entry.Frame, &bulkFactsLine{})
	}
}

func TestControlRequestsRoundTripThroughTheCorpus(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Requests {
		var envelope controlEnvelope
		if err := json.Unmarshal(entry.Frame, &envelope); err != nil {
			t.Errorf("request %s: decode envelope: %v", entry.Name, err)
			continue
		}
		if goIgnoredMethods[envelope.Method] {
			continue
		}
		var params any
		switch envelope.Method {
		case "reindex", "fileChanged", "workspaceChanged":
			params = &filePathParams{}
		case "semanticPass", "filesCreated":
			params = &filePathsParams{}
		case "resolutionChanged":
			params = &resolutionChangedParams{}
		case "status":
		default:
			t.Errorf("request %s: method %q is neither handled by the Go plugin nor in goIgnoredMethods", entry.Name, envelope.Method)
			continue
		}
		if params != nil {
			if err := json.Unmarshal(envelope.Params, params); err != nil {
				t.Errorf("request %s: decode params into %T: %v", entry.Name, params, err)
				continue
			}
			envelope.Params = mustMarshal(t, params)
		}
		encoded := mustMarshal(t, envelope)
		want := withoutGoIgnores(asJSONValue(t, entry.Frame))
		if got := asJSONValue(t, encoded); !reflect.DeepEqual(got, want) {
			t.Errorf("request %s: round trip differs\n  got:  %s\n  want: %s", entry.Name, encoded, mustMarshal(t, want))
		}
	}
}

func TestControlResponsesRoundTripThroughTheCorpus(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Responses {
		var shape struct {
			Result map[string]json.RawMessage `json:"result"`
		}
		if err := json.Unmarshal(entry.Frame, &shape); err != nil {
			t.Errorf("response %s: %v", entry.Name, err)
			continue
		}
		var target any
		switch {
		case shape.Result["acknowledged"] != nil:
			target = &ackResponse{}
		case shape.Result["delta"] != nil:
			target = &resolutionChangedResponse{}
		default:
			target = &fileChangeResponse{}
		}
		roundTrip(t, "response "+entry.Name, entry.Frame, target)
	}
}

// The Go plugin never answers with a JSON-RPC error, so there is no Go type to
// round-trip through; the frames still have to keep the error code and the id
// a Go reader would match them by.
func TestControlErrorFramesKeepTheirCodeAndId(t *testing.T) {
	for _, entry := range loadWireCorpus(t).Errors {
		var frame struct {
			JSONRPC string          `json:"jsonrpc"`
			ID      json.RawMessage `json:"id"`
			Error   struct {
				Code    int    `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}
		if err := json.Unmarshal(entry.Frame, &frame); err != nil {
			t.Errorf("error %s: %v", entry.Name, err)
			continue
		}
		if frame.JSONRPC != jsonrpcVersion || len(frame.ID) == 0 || frame.Error.Code != -32602 || frame.Error.Message == "" {
			t.Errorf("error %s: unexpected frame %s", entry.Name, entry.Frame)
		}
	}
}
