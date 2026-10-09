package main

import (
	"reflect"
	"testing"
)

// assertPathRules checks core's qualifiedPath rules on one node: present
// exactly on declarations (Function, Type, Variable), joining back to
// QualifiedName, ending in Name, `sep` absent on the first segment and "."
// on every later one.
func assertPathRules(t *testing.T, node wireNode) {
	t.Helper()
	declaration := node.Kind != nodeKindFile && node.Kind != nodeKindModule
	if !declaration {
		if node.QualifiedPath != nil {
			t.Fatalf("%s %q carries a path %+v; only declarations do", node.Kind, node.QualifiedName, node.QualifiedPath)
		}
		return
	}
	path := node.QualifiedPath
	if len(path) == 0 {
		t.Fatalf("declaration %q has no qualifiedPath", node.QualifiedName)
	}
	if got := joinPath(path); got != node.QualifiedName {
		t.Fatalf("path %+v joins to %q, want %q", path, got, node.QualifiedName)
	}
	if last := path[len(path)-1].Name; last != node.Name {
		t.Fatalf("path %+v ends in %q, want the node's name %q", path, last, node.Name)
	}
	for i, segment := range path {
		if segment.Name == "" || (i == 0) != (segment.Sep == "") || (i > 0 && segment.Sep != ".") {
			t.Fatalf("path %+v: segment %d breaks the element rules", path, i)
		}
	}
}

func TestEveryDeclarationCarriesAPathThatJoinsBack(t *testing.T) {
	graph := extract(t, "a.go", `package app

import "fmt"

type Server struct{}

type Stack[T any] struct{}

type Closer interface {
	Close() error
}

const Limit = 3

var defaultServer Server

func (s *Server) Close() error { return nil }

func (s *Stack[T]) Push(v T) {}

func New() *Server { fmt.Println(Limit); return nil }

func init() {}
`)
	for _, node := range graph.nodes {
		assertPathRules(t, node)
	}

	for qualifiedName, want := range map[string][]pathSegment{
		"Server.Close":  {{Name: "Server"}, {Sep: ".", Name: "Close"}},
		"Stack.Push":    {{Name: "Stack"}, {Sep: ".", Name: "Push"}},
		"Closer.Close":  {{Name: "Closer"}, {Sep: ".", Name: "Close"}},
		"New":           {{Name: "New"}},
		"Limit":         {{Name: "Limit"}},
		"defaultServer": {{Name: "defaultServer"}},
	} {
		if got := nodeByQName(t, graph, qualifiedName).QualifiedPath; !reflect.DeepEqual(got, want) {
			t.Fatalf("%s: path = %+v, want %+v", qualifiedName, got, want)
		}
	}
}

// A semantic placeholder's qualifiedName key carries the same path the
// declaration it addresses was given, as keyPath.
func TestSemanticPlaceholderKeysCarryAKeyPathThatJoinsBack(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	state := newPluginState(root)
	diff, reason, _ := state.handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("pass answered incomplete: %s", reason)
	}

	methodKeys := 0
	for _, node := range diff.UpsertNodes {
		assertPathRules(t, node)
		if node.Target == nil || node.Target.Key.QualifiedName == "" {
			continue
		}
		key, path := node.Target.Key.QualifiedName, node.Target.KeyPath
		if got := joinPath(path); got != key {
			t.Fatalf("keyPath %+v joins to %q, want the key %q", path, got, key)
		}
		if len(path) == 2 {
			methodKeys++
		}
	}
	if methodKeys == 0 {
		t.Fatal("no two-segment `T.M` keyPath in the probe project's semantic diff")
	}
	if got := nodeByQNameInDiff(t, diff, "example.com/probe.Handle.Name").Target.KeyPath; !reflect.DeepEqual(got,
		[]pathSegment{{Name: "Handle"}, {Sep: ".", Name: "Name"}}) {
		t.Fatalf("Handle.Name keyPath = %+v", got)
	}
}

func nodeByQNameInDiff(t *testing.T, diff fileChangeDiff, qualifiedName string) wireNode {
	t.Helper()
	for _, node := range diff.UpsertNodes {
		if node.QualifiedName == qualifiedName {
			return node
		}
	}
	t.Fatalf("no node with qualifiedName %q in the semantic diff", qualifiedName)
	return wireNode{}
}
