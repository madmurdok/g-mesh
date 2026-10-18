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

// aliasEditFixture builds lib (type ID = string plus the interface) and an
// implementor in another package whose method spells the alias's target
// (string) out, so the interface only reaches it through ID.
func aliasEditFixture(t *testing.T, libTmpl, appMethod string) (string, string) {
	t.Helper()
	const header = "package lib\n\ntype ID = string\n\n"
	lib := header + libTmpl
	app := "package app\n\nimport \"example.com/x/lib\"\n\ntype Mine struct{}\n\n" +
		appMethod + "\n\nvar _ lib.Binder = Mine{}\n"
	root := writeTree(t, map[string]string{
		"go.mod":     "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go": lib,
		"app/app.go": app,
	})
	return root, lib
}

// Retargeting an alias used by an interface method changes the method set
// the implementor must satisfy even though the interface's text for the
// method does not change.
func TestSemanticPassInterfaceEditRetractsOnAnAliasRetarget(t *testing.T) {
	requireGoToolchain(t)
	cases := map[string]struct{ iface, method string }{
		"top level": {
			"type Binder interface{ Bind(id ID) string }\n",
			"func (Mine) Bind(id string) string { return id }",
		},
		"nested in composite types": {
			"type Binder interface {\n\tBind(m map[string][]*ID, xs ...ID) string\n}\n",
			"func (Mine) Bind(m map[string][]*string, xs ...string) string { return \"\" }",
		},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			root, lib := aliasEditFixture(t, c.iface, c.method)
			state, before := carryWholePass(t, root, carryMineToBinder)

			after := ifaceEditPass(t, state, root, "lib/lib.go",
				strings.Replace(lib, "type ID = string", "type ID = int", 1))
			requireRetracted(t, after, carryMineToBinder, before[carryMineToBinder])
			requireRechecked(t, state, "app/app.go")
		})
	}
}

// Renaming a parameter of an interface method leaves the method set intact:
// no implementor is re-checked and the edge is kept.
func TestSemanticPassInterfaceEditParamRenameDoesNotWiden(t *testing.T) {
	requireGoToolchain(t)
	const lib = "package lib\n\ntype Binder interface{ Bind(id string) string }\n"
	root := writeTree(t, map[string]string{
		"go.mod":     "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go": lib,
		"app/app.go": "package app\n\nimport \"example.com/x/lib\"\n\ntype Mine struct{}\n\n" +
			"func (Mine) Bind(id string) string { return id }\n\nvar _ lib.Binder = Mine{}\n",
	})
	state, before := carryWholePass(t, root, carryMineToBinder)

	after := ifaceEditPass(t, state, root, "lib/lib.go", strings.Replace(lib, "id string", "name string", 1))
	if state.semantic.recheckedImplementors != nil {
		t.Errorf("parameter rename re-checked %v", state.semantic.recheckedImplementors)
	}
	if carryDeleted(after, before[carryMineToBinder].ID) {
		t.Errorf("parameter rename retracted %s", carryMineToBinder)
	}
}

// sealedLib embeds x.Sealed, whose only method is unexported; y.Sealed has
// the same text in another package, so it is a different method set.
const sealedLib = "package lib\n\nimport \"example.com/x/x\"\n\ntype Binder interface {\n\tx.Sealed\n\tBind() string\n}\n"

// sealedFixture: app.Mine satisfies lib.Binder through x.Impl's m().
func sealedFixture(t *testing.T) string {
	t.Helper()
	return writeTree(t, map[string]string{
		"go.mod":     "module example.com/x\n\ngo 1.22\n",
		"x/x.go":     "package x\n\ntype Sealed interface{ m() }\n\ntype Impl struct{}\n\nfunc (Impl) m() {}\n",
		"y/y.go":     "package y\n\ntype Sealed interface{ m() }\n",
		"lib/lib.go": sealedLib,
		"app/app.go": "package app\n\nimport \"example.com/x/x\"\n\ntype Mine struct{ x.Impl }\n\nfunc (Mine) Bind() string { return \"\" }\n",
	})
}

// Retargeting an embedded interface from x.Sealed to y.Sealed (both
// `interface{ m() }`) changes the method set: x.m and y.m are different
// names, so the implementor is re-checked and its edge retracted.
func TestSemanticPassInterfaceEditRetractsOnAnUnexportedEmbedRetarget(t *testing.T) {
	requireGoToolchain(t)
	root := sealedFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder)

	retargeted := strings.Replace(strings.Replace(sealedLib, "x/x\"", "x/y\"", 1), "x.Sealed", "y.Sealed", 1)
	after := ifaceEditPass(t, state, root, "lib/lib.go", retargeted)
	requireRetracted(t, after, carryMineToBinder, before[carryMineToBinder])
	requireRechecked(t, state, "app/app.go")
}

// An interface whose method set is unchanged (here one embedding an
// unexported method from another package) does not re-check its
// implementor, over two passes of its file.
func TestSemanticPassInterfaceEditUnchangedUnexportedEmbedDoesNotWiden(t *testing.T) {
	requireGoToolchain(t)
	root := sealedFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder)

	for pass, contents := range []string{sealedLib + "\n// touched\n", sealedLib} {
		after := ifaceEditPass(t, state, root, "lib/lib.go", contents)
		if slices.Contains(state.semantic.recheckedImplementors, "app/app.go") {
			t.Errorf("pass %d of an unchanged lib re-checked app: %v", pass+1, state.semantic.recheckedImplementors)
		}
		if carryDeleted(after, before[carryMineToBinder].ID) {
			t.Errorf("pass %d of an unchanged lib retracted %s", pass+1, carryMineToBinder)
		}
	}
}
