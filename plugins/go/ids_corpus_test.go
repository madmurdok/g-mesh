package main

import (
	"encoding/json"
	"os"
	"strconv"
	"testing"
)

// testdata/ids.json is written by plugins/sdk/tests/go_id_corpus.rs from
// sdk::ids, which also asserts the file is current; this test holds the Go
// side to the same ids, so the two schemes cannot drift apart unnoticed.
type idCorpus struct {
	Nodes []struct {
		FilePath      string `json:"filePath"`
		Kind          string `json:"kind"`
		QualifiedName string `json:"qualifiedName"`
		NativeKind    string `json:"nativeKind"`
		ID            string `json:"id"`
	} `json:"nodes"`
	Edges []struct {
		FromID        string `json:"fromId"`
		Kind          string `json:"kind"`
		ToID          string `json:"toId"`
		ToDeclaration *int   `json:"toDeclaration"`
		ID            string `json:"id"`
	} `json:"edges"`
}

func loadIDCorpus(t *testing.T) idCorpus {
	t.Helper()
	data, err := os.ReadFile("testdata/ids.json")
	if err != nil {
		t.Fatalf("read corpus: %v", err)
	}
	var corpus idCorpus
	if err := json.Unmarshal(data, &corpus); err != nil {
		t.Fatalf("parse corpus: %v", err)
	}
	if len(corpus.Nodes) == 0 || len(corpus.Edges) == 0 {
		t.Fatalf("corpus has %d nodes and %d edges; both must be non-empty", len(corpus.Nodes), len(corpus.Edges))
	}
	return corpus
}

func TestNodeIDForMatchesTheSDKCorpus(t *testing.T) {
	for _, c := range loadIDCorpus(t).Nodes {
		if got := nodeIDFor(c.FilePath, c.Kind, c.QualifiedName, c.NativeKind); got != c.ID {
			t.Errorf("nodeIDFor(%q, %q, %q, %q) = %q, sdk::ids says %q",
				c.FilePath, c.Kind, c.QualifiedName, c.NativeKind, got, c.ID)
		}
	}
}

func TestEdgeIDForMatchesTheSDKCorpus(t *testing.T) {
	for _, c := range loadIDCorpus(t).Edges {
		ordinal := "nil"
		if c.ToDeclaration != nil {
			ordinal = strconv.Itoa(*c.ToDeclaration)
		}
		if got := edgeIDFor(c.FromID, c.Kind, c.ToID, c.ToDeclaration); got != c.ID {
			t.Errorf("edgeIDFor(%q, %q, %q, %s) = %q, sdk::ids says %q",
				c.FromID, c.Kind, c.ToID, ordinal, got, c.ID)
		}
	}
}

// Each pair joins to the same string under a space separator; a NUL
// separator keeps their ids apart.
func TestIDsOfSpaceJoinCollidingTuplesDiffer(t *testing.T) {
	if nodeIDFor("p", "Function", "x y", "z") == nodeIDFor("p", "Function", "x", "y z") {
		t.Errorf("node ids collide for qualifiedName/nativeKind split \"x y\"|\"z\" vs \"x\"|\"y z\"")
	}
	if nodeIDFor("a b", "c", "", "") == nodeIDFor("a", "b c", "", "") {
		t.Errorf("node ids collide for filePath/kind split \"a b\"|\"c\" vs \"a\"|\"b c\"")
	}
	one := 1
	if edgeIDFor("f", "CALLS", "t 1", nil) == edgeIDFor("f", "CALLS", "t", &one) {
		t.Errorf("edge ids collide for toId \"t 1\" unbound vs toId \"t\" bound to 1")
	}
}
