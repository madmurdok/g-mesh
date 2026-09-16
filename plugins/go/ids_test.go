package main

import "testing"

// The expected values below were computed by running the TS plugin's own
// id-hashing code verbatim, not recalled from memory:
//
//	node -e '
//	const {createHash} = require("crypto");
//	function hash(input){return createHash("sha256").update(input,"utf8").digest("hex").slice(0,32);}
//	function nodeIdFor(filePath, kind, qualifiedName, nativeKind){
//	  return hash(`node ${filePath} ${kind} ${qualifiedName} ${nativeKind ?? ""}`);
//	}
//	function edgeIdFor(fromId, kind, toId, toDeclaration){
//	  const binding = toDeclaration === undefined ? "" : ` ${toDeclaration}`;
//	  return hash(`edge ${fromId} ${kind} ${toId}${binding}`);
//	}
//	console.log(nodeIdFor("main.go","File","main.go",undefined));
//	console.log(edgeIdFor("n1","DEFINES","n2",undefined));
//	console.log(edgeIdFor("n1","CALLS","n2",2));
//	'
//
// which printed, in order:
//
//	1453099e1c11aef90ca118eb6d140fe1
//	6be2b51147fd32ef62ebf506f184329a
//	1deb655be169c9fcfe9e081b775c7117
//
// (Node v20.6.1, plugins/typescript/src/extract.ts's `hash`/`nodeIdFor`/
// `edgeIdFor` copied verbatim into the -e script above.) Asserting against
// these fixed strings, rather than only against Go-computed values, is
// the whole point: a bug that made this file's own hash function and its
// own callers agree with each other while disagreeing with the TS scheme
// would pass a self-consistency check and fail this one.
const (
	tsReferenceNodeIDMainGoFile   = "1453099e1c11aef90ca118eb6d140fe1"
	tsReferenceEdgeIDUnbound      = "6be2b51147fd32ef62ebf506f184329a"
	tsReferenceEdgeIDBoundOrdinal = "1deb655be169c9fcfe9e081b775c7117"
)

func TestNodeIDForMatchesTheTSScheme(t *testing.T) {
	got := nodeIDFor("main.go", "File", "main.go", "")
	if got != tsReferenceNodeIDMainGoFile {
		t.Fatalf("nodeIDFor(main.go, File, main.go, \"\") = %q, want %q (the TS-computed reference)",
			got, tsReferenceNodeIDMainGoFile)
	}
}

func TestEdgeIDForMatchesTheTSScheme(t *testing.T) {
	got := edgeIDFor("n1", "DEFINES", "n2", nil)
	if got != tsReferenceEdgeIDUnbound {
		t.Fatalf("edgeIDFor(n1, DEFINES, n2, nil) = %q, want %q", got, tsReferenceEdgeIDUnbound)
	}

	ordinal := 2
	got = edgeIDFor("n1", "CALLS", "n2", &ordinal)
	if got != tsReferenceEdgeIDBoundOrdinal {
		t.Fatalf("edgeIDFor(n1, CALLS, n2, &2) = %q, want %q", got, tsReferenceEdgeIDBoundOrdinal)
	}
}

// nodeIDFor must be a pure function of (filePath, kind, qualifiedName,
// nativeKind) - never of anything content- or position-derived - since
// id-stability.whitespace-edit and id-stability.declaration-edit-applies
// both depend on an id surviving an edit elsewhere in (or anywhere in) the
// file untouched.
func TestNodeIDForIsStableAcrossRepeatedCalls(t *testing.T) {
	first := nodeIDFor("a/b.go", "File", "a/b.go", "")
	second := nodeIDFor("a/b.go", "File", "a/b.go", "")
	if first != second {
		t.Fatalf("nodeIDFor is not pure: %q != %q", first, second)
	}
}

// An absent nativeKind ("") and a present-but-different one must not
// collide - this is what keeps two nodes with the same qualifiedName but
// different native kinds (a getter and a setter, say - not a shape this
// scaffold produces yet, but nodeIDFor's own contract) distinct.
func TestNodeIDForDependsOnNativeKind(t *testing.T) {
	withEmpty := nodeIDFor("a.go", "Function", "F", "")
	withKind := nodeIDFor("a.go", "Function", "F", "method")
	if withEmpty == withKind {
		t.Fatalf("nodeIDFor(..., \"\") and nodeIDFor(..., \"method\") must not collide")
	}
}

// edgeIDFor's toDeclaration must actually change the id when present, and
// two different ordinals must not collide with each other - the whole
// reason toDeclaration exists (extract.ts's own doc comment on
// edgeIdFor): a caller binding two different overloads of one target must
// get two different edges, not one overwriting the other.
func TestEdgeIDForDistinguishesOrdinals(t *testing.T) {
	unbound := edgeIDFor("n1", "CALLS", "n2", nil)
	zero := 0
	one := 1
	boundZero := edgeIDFor("n1", "CALLS", "n2", &zero)
	boundOne := edgeIDFor("n1", "CALLS", "n2", &one)

	if unbound == boundZero {
		t.Fatalf("an edge with toDeclaration 0 must not collide with an unbound edge")
	}
	if boundZero == boundOne {
		t.Fatalf("toDeclaration 0 and 1 must not collide")
	}
}
