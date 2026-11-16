package main

import "testing"

// The expected values were computed from the byte string plugins/sdk/src/ids.rs
// hashes (tag, then each field preceded by a NUL byte), not from Go code:
//
//	printf 'node\0main.go\0File\0main.go\0' | shasum -a 256 | cut -c1-32
//	printf 'edge\0n1\0DEFINES\0n2' | shasum -a 256 | cut -c1-32
//	printf 'edge\0n1\0CALLS\0n2\0002' | shasum -a 256 | cut -c1-32
//
// (the same recipe reproduces the SDK-pinned id in plugins/sdk/tests/id_scheme.rs).
// Asserting against fixed strings, rather than only against Go-computed
// values, catches a bug that keeps this file's hash function and its callers
// consistent with each other while disagreeing with the SDK scheme.
const (
	sdkReferenceNodeIDMainGoFile   = "1c06532b9ba18d30da653a043b3ed0e1"
	sdkReferenceEdgeIDUnbound      = "e993bb1f768d7f2f97b6b5a84a5a7a7c"
	sdkReferenceEdgeIDBoundOrdinal = "c25c95159195db66a611a5b42f125c09"
)

func TestNodeIDForMatchesTheSDKScheme(t *testing.T) {
	got := nodeIDFor("main.go", "File", "main.go", "")
	if got != sdkReferenceNodeIDMainGoFile {
		t.Fatalf("nodeIDFor(main.go, File, main.go, \"\") = %q, want %q (the SDK reference)",
			got, sdkReferenceNodeIDMainGoFile)
	}
}

func TestEdgeIDForMatchesTheSDKScheme(t *testing.T) {
	got := edgeIDFor("n1", "DEFINES", "n2", nil)
	if got != sdkReferenceEdgeIDUnbound {
		t.Fatalf("edgeIDFor(n1, DEFINES, n2, nil) = %q, want %q", got, sdkReferenceEdgeIDUnbound)
	}

	ordinal := 2
	got = edgeIDFor("n1", "CALLS", "n2", &ordinal)
	if got != sdkReferenceEdgeIDBoundOrdinal {
		t.Fatalf("edgeIDFor(n1, CALLS, n2, &2) = %q, want %q", got, sdkReferenceEdgeIDBoundOrdinal)
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
