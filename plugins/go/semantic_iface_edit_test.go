package main

// A per-file pass of an interface's file re-checks the SUPERTYPE_OF
// edges other files hold onto that interface when the interface's method set
// changed or the interface is gone, and only then. Every test runs a
// whole-project pass, rewrites the interface's file and runs a per-file pass
// of that file alone on the same long-lived pluginState.

import (
	"slices"
	"strings"
	"testing"
)

// carryLibWithClose is carryLibSource with Close() added to Binder (impl
// keeps satisfying it, so lib itself still type-checks).
const carryLibWithClose = `package lib

type Binder interface {
	Bind() string
	Close()
}

type Gone interface{ Bind() string }

type impl struct{}

func (impl) Bind() string { return "impl" }

func (impl) Close() {}

func New() Binder { return impl{} }
`

// ifaceEditPass rewrites relPath and runs a per-file pass of it alone.
func ifaceEditPass(t *testing.T, state *pluginState, root, relPath, contents string) fileChangeDiff {
	t.Helper()
	carryRewrite(t, root, relPath, contents)
	diff, reason, _ := state.handleSemanticPass([]string{relPath})
	if reason != "" {
		t.Fatalf("per-file pass of %s answered incomplete: %s", relPath, reason)
	}
	return diff
}

// requireRetracted fails unless the edge's id is deleted and not upserted.
func requireRetracted(t *testing.T, diff fileChangeDiff, label string, edge wireEdge) {
	t.Helper()
	if !carryDeleted(diff, edge.ID) || carryUpserted(diff, edge.ID) {
		t.Errorf("%s no longer holds but was not retracted (deleted=%v upserted=%v)",
			label, carryDeleted(diff, edge.ID), carryUpserted(diff, edge.ID))
	}
}

// requireRechecked fails unless the last pass re-checked relPath. A lib edit
// may also re-check other files (other.Binder2's interface-to-interface edge
// onto lib.Binder), so this asserts membership, not the whole list.
func requireRechecked(t *testing.T, state *pluginState, relPath string) {
	t.Helper()
	if !slices.Contains(state.semantic.recheckedImplementors, relPath) {
		t.Errorf("pass did not re-check %s; rechecked: %v", relPath, state.semantic.recheckedImplementors)
	}
}

// Behaviour 1: a method added to the interface that the implementor lacks
// retracts the implementor's edge from a pass of the interface's file.
func TestSemanticPassInterfaceEditRetractsOnAnAddedMethod(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder)

	after := ifaceEditPass(t, state, root, "lib/lib.go", carryLibWithClose)
	requireRetracted(t, after, carryMineToBinder, before[carryMineToBinder])
	requireRechecked(t, state, "app/app.go")
}

// Behaviour 2: a signature change that keeps the method name retracts.
func TestSemanticPassInterfaceEditRetractsOnASignatureChange(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder, carryMineToGone)

	lib := strings.NewReplacer(
		"type Binder interface{ Bind() string }", "type Binder interface{ Bind() int }",
		`func (impl) Bind() string { return "impl" }`, `func (impl) Bind() int { return 1 }`,
	).Replace(carryLibSource)
	after := ifaceEditPass(t, state, root, "lib/lib.go", lib)
	requireRetracted(t, after, carryMineToBinder, before[carryMineToBinder])
	if carryDeleted(after, before[carryMineToGone].ID) {
		t.Errorf("changing lib.Binder also retracted %s", carryMineToGone)
	}
	requireRechecked(t, state, "app/app.go")
}

// Behaviour 3: deleting the interface retracts the edge onto it, and only it.
func TestSemanticPassInterfaceEditRetractsOntoADeletedInterface(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToGone, carryMineToBinder)

	after := ifaceEditPass(t, state, root, "lib/lib.go",
		strings.Replace(carryLibSource, "type Gone interface{ Bind() string }\n", "", 1))
	requireRetracted(t, after, carryMineToGone, before[carryMineToGone])
	if carryDeleted(after, before[carryMineToBinder].ID) {
		t.Errorf("deleting lib.Gone also retracted %s", carryMineToBinder)
	}
	requireRechecked(t, state, "app/app.go")
}

// Behaviour 4: an interface change the implementor still satisfies keeps the
// edge (upserted with its target node, not deleted), and the refreshed record
// makes a later body-only edit of the same file cost no second load.
func TestSemanticPassInterfaceEditKeepsAStillSatisfiedEdge(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	carryRewrite(t, root, "app/app.go", carryAppSource+"\nfunc (Mine) Close() {}\n")
	state, before := carryWholePass(t, root, carryMineToBinder)

	after := ifaceEditPass(t, state, root, "lib/lib.go", carryLibWithClose)
	edge := before[carryMineToBinder]
	if carryDeleted(after, edge.ID) {
		t.Errorf("Mine has Close but %s was retracted", carryMineToBinder)
	}
	if !carryUpserted(after, edge.ID) {
		t.Errorf("Mine still satisfies lib.Binder but %s was not upserted", carryMineToBinder)
	}
	if !carryHasNode(after, edge.ToID) {
		t.Errorf("%s upserted without its target node %s", carryMineToBinder, edge.ToID)
	}
	requireRechecked(t, state, "app/app.go")

	again := ifaceEditPass(t, state, root, "lib/lib.go",
		strings.Replace(carryLibWithClose, `return "impl"`, `return "impl, edited"`, 1))
	if state.semantic.recheckedImplementors != nil {
		t.Errorf("body edit after the interface change re-checked %v", state.semantic.recheckedImplementors)
	}
	if carryDeleted(again, edge.ID) {
		t.Errorf("body edit retracted %s", carryMineToBinder)
	}
}

// Behaviour 5: an implementor in a package that does not import the
// interface's package is re-checked too.
func TestSemanticPassInterfaceEditRetractsANonImportingImplementor(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder2)

	after := ifaceEditPass(t, state, root, "other/other.go",
		"package other\n\ntype Binder2 interface {\n\tBind() string\n\tClose()\n}\n")
	requireRetracted(t, after, carryMineToBinder2, before[carryMineToBinder2])
	requireRechecked(t, state, "app/app.go")
}

// Behaviour 6: an edit that keeps every interface's method set (a body and a
// comment) needs no second load and leaves the implementors' edges alone.
func TestSemanticPassInterfaceEditBodyOnlyDoesNotWiden(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder, carryMineToGone)

	lib := strings.NewReplacer(
		`return "impl"`, `return "impl, edited"`,
		"type Binder interface", "// Binder binds.\ntype Binder interface",
	).Replace(carryLibSource)
	after := ifaceEditPass(t, state, root, "lib/lib.go", lib)
	if state.semantic.recheckedImplementors != nil {
		t.Errorf("body-only edit re-checked %v", state.semantic.recheckedImplementors)
	}
	mine := before[carryMineToBinder].FromID
	for _, edge := range after.UpsertEdges {
		if edge.FromID == mine {
			t.Errorf("body-only edit of lib/lib.go upserted %s out of app/app.go's Mine", edge.ID)
		}
	}
	for _, label := range []string{carryMineToBinder, carryMineToGone} {
		if carryDeleted(after, before[label].ID) {
			t.Errorf("body-only edit retracted %s", label)
		}
	}
}

// An edit of an interface that another interface embeds changes the
// embedding interface's method set. Base is an alias, so no edge points at
// it and only Binder's flattened method set can reveal the change.
func TestSemanticPassInterfaceEditDetectsAnEmbeddedInterfaceChange(t *testing.T) {
	requireGoToolchain(t)
	const lib = "package lib\n\ntype Base = interface{ Bind() string }\n\ntype Binder interface{ Base }\n"
	root := writeTree(t, map[string]string{
		"go.mod":     "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go": lib,
		"app/app.go": carryAppSource,
	})
	state, before := carryWholePass(t, root, carryMineToBinder)

	after := ifaceEditPass(t, state, root, "lib/lib.go", strings.Replace(lib,
		"interface{ Bind() string }", "interface {\n\tBind() string\n\tClose()\n}", 1))
	requireRetracted(t, after, carryMineToBinder, before[carryMineToBinder])
	requireRechecked(t, state, "app/app.go")
}
