package main

// A per-file semantic pass keeps SUPERTYPE_OF edges onto interfaces outside
// the edited file's package: imported project interfaces are evaluated
// exactly, and an edge onto an interface the pass cannot see is carried
// forward while the subtype still has every interface method.

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const (
	carryMineToBinder  = "app/app.go:Mine SUPERTYPE_OF container:example.com/x/lib#Binder"
	carryMineToBinder2 = "app/app.go:Mine SUPERTYPE_OF container:example.com/x/other#Binder2"
	carryMineToGone    = "app/app.go:Mine SUPERTYPE_OF container:example.com/x/lib#Gone"
)

const carryAppSource = `package app

import "example.com/x/lib"

type Mine struct{}

func (Mine) Bind() string { return "mine" }

func Use(b lib.Binder) string { return b.Bind() }
`

const carryLibSource = `package lib

type Binder interface{ Bind() string }

type Gone interface{ Bind() string }

type impl struct{}

func (impl) Bind() string { return "impl" }

func New() Binder { return impl{} }
`

// carryFixture: app imports lib (Binder, Gone) and does not import other
// (Binder2); app.Mine satisfies all three.
func carryFixture(t *testing.T) string {
	t.Helper()
	return writeTree(t, map[string]string{
		"go.mod":         "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go":     carryLibSource,
		"other/other.go": "package other\n\ntype Binder2 interface{ Bind() string }\n",
		"app/app.go":     carryAppSource,
	})
}

// supertypeEdgesByLabel renders every upserted edge of diff and returns the
// edge per rendered label.
func supertypeEdgesByLabel(t *testing.T, root string, diff fileChangeDiff) map[string]wireEdge {
	t.Helper()
	out := map[string]wireEdge{}
	for _, edge := range diff.UpsertEdges {
		single := fileChangeDiff{UpsertNodes: diff.UpsertNodes, UpsertEdges: []wireEdge{edge}}
		rendered, _ := renderTwinEdges(t, root, single)
		if len(rendered) == 1 {
			out[rendered[0]] = edge
		}
	}
	return out
}

func carryLabels(edges map[string]wireEdge) []string {
	out := make([]string, 0, len(edges))
	for label := range edges {
		out = append(out, label)
	}
	return out
}

func carryDeleted(diff fileChangeDiff, id string) bool {
	for _, deleted := range diff.DeleteEdgeIds {
		if deleted == id {
			return true
		}
	}
	return false
}

func carryUpserted(diff fileChangeDiff, id string) bool {
	for _, edge := range diff.UpsertEdges {
		if edge.ID == id {
			return true
		}
	}
	return false
}

func carryHasNode(diff fileChangeDiff, id string) bool {
	for _, node := range diff.UpsertNodes {
		if node.ID == id {
			return true
		}
	}
	return false
}

// carryWholePass runs the whole-project pass on a fresh state and returns the
// state and the SUPERTYPE_OF edges it emitted, requiring the given labels.
func carryWholePass(t *testing.T, root string, want ...string) (*pluginState, map[string]wireEdge) {
	t.Helper()
	state := newPluginState(root)
	diff, reason, _ := state.handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("whole-project pass answered incomplete: %s", reason)
	}
	edges := supertypeEdgesByLabel(t, root, diff)
	for _, label := range want {
		if _, ok := edges[label]; !ok {
			t.Fatalf("whole-project pass did not emit %s; edges: %v", label, carryLabels(edges))
		}
	}
	return state, edges
}

func carryRewrite(t *testing.T, root, relPath, contents string) {
	t.Helper()
	writeFixtureFile(t, canonicalizeProjectRoot(root), relPath, contents)
}

// An unchanged implementor's file keeps, over two per-file passes, its edges
// onto an imported interface (re-evaluated) and onto a non-imported one
// (carried, with its placeholder).
func TestSemanticPassPerFileKeepsCrossPackageSupertypes(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder, carryMineToBinder2)

	for pass := 1; pass <= 2; pass++ {
		after, _, _ := state.handleSemanticPass([]string{"app/app.go"})
		for _, label := range []string{carryMineToBinder, carryMineToBinder2} {
			edge := before[label]
			if carryDeleted(after, edge.ID) {
				t.Errorf("per-file pass %d retracted %s", pass, label)
			}
			if !carryUpserted(after, edge.ID) {
				t.Errorf("per-file pass %d did not re-upsert %s", pass, label)
			}
			if !carryHasNode(after, edge.ToID) {
				t.Errorf("per-file pass %d upserted %s without its placeholder node %s",
					pass, label, edge.ToID)
			}
		}
	}
}

// A carried edge goes once the subtype no longer has a method name the
// interface recorded.
func TestSemanticPassPerFileRetractsACarriedSupertypeWhoseMethodIsGone(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder2)

	carryRewrite(t, root, "app/app.go",
		strings.Replace(carryAppSource, "func (Mine) Bind() string", "func (Mine) Attach() string", 1))
	after, _, _ := state.handleSemanticPass([]string{"app/app.go"})
	id := before[carryMineToBinder2].ID
	if !carryDeleted(after, id) || carryUpserted(after, id) {
		t.Fatalf("Mine lost Bind but %s was not retracted (deleted=%v upserted=%v)",
			carryMineToBinder2, carryDeleted(after, id), carryUpserted(after, id))
	}
}

// A carried edge is not resurrected out of a subtype that is gone (renamed).
func TestSemanticPassPerFileRetractsACarriedSupertypeWhoseSubtypeIsGone(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder2)

	carryRewrite(t, root, "app/app.go", strings.ReplaceAll(carryAppSource, "Mine", "Yours"))
	after, _, _ := state.handleSemanticPass([]string{"app/app.go"})
	old := before[carryMineToBinder2]
	if !carryDeleted(after, old.ID) {
		t.Errorf("Mine is gone but %s was not retracted", carryMineToBinder2)
	}
	for _, edge := range after.UpsertEdges {
		if edge.FromID == old.FromID {
			t.Errorf("per-file pass upserted %s out of the removed Mine node", edge.ID)
		}
	}
}

// An imported interface is evaluated exactly, so a signature change that
// keeps the method name still retracts.
func TestSemanticPassPerFileRetractsAnImportedSupertypeOnASignatureChange(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder)

	carryRewrite(t, root, "app/app.go", strings.Replace(carryAppSource,
		`func (Mine) Bind() string { return "mine" }`, `func (Mine) Bind() int { return 1 }`, 1))
	after, _, _ := state.handleSemanticPass([]string{"app/app.go"})
	id := before[carryMineToBinder].ID
	if !carryDeleted(after, id) || carryUpserted(after, id) {
		t.Fatalf("Mine.Bind no longer satisfies lib.Binder but %s was not retracted (deleted=%v upserted=%v)",
			carryMineToBinder, carryDeleted(after, id), carryUpserted(after, id))
	}
}

// An exported interface deleted from an imported package is known to be gone
// (export data would carry it), so its edge is retracted.
func TestSemanticPassPerFileRetractsASupertypeOntoADeletedImportedInterface(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToGone, carryMineToBinder)

	carryRewrite(t, root, "lib/lib.go",
		strings.Replace(carryLibSource, "type Gone interface{ Bind() string }\n", "", 1))
	after, _, _ := state.handleSemanticPass([]string{"app/app.go"})
	id := before[carryMineToGone].ID
	if !carryDeleted(after, id) || carryUpserted(after, id) {
		t.Fatalf("lib.Gone was deleted but %s was not retracted (deleted=%v upserted=%v)",
			carryMineToGone, carryDeleted(after, id), carryUpserted(after, id))
	}
	if carryDeleted(after, before[carryMineToBinder].ID) {
		t.Errorf("deleting lib.Gone also retracted %s", carryMineToBinder)
	}
}

// A whole-project pass in which every module loaded evaluated every
// interface: an edge onto a deleted package is retracted, not carried.
func TestSemanticPassExhaustiveRetractsASupertypeOntoADeletedPackage(t *testing.T) {
	requireGoToolchain(t)
	root := carryFixture(t)
	state, before := carryWholePass(t, root, carryMineToBinder2)

	if err := os.RemoveAll(filepath.Join(canonicalizeProjectRoot(root), "other")); err != nil {
		t.Fatalf("remove other/: %v", err)
	}
	after, reason, _ := state.handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("second whole-project pass answered incomplete: %s", reason)
	}
	id := before[carryMineToBinder2].ID
	if !carryDeleted(after, id) || carryUpserted(after, id) {
		t.Fatalf("package other is gone but %s was not retracted (deleted=%v upserted=%v)",
			carryMineToBinder2, carryDeleted(after, id), carryUpserted(after, id))
	}
}

// A whole-project pass with a failed module keeps the edges onto that
// module's interfaces: it could not evaluate them.
func TestSemanticPassKeepsSupertypesOntoAFailedModulesInterfaces(t *testing.T) {
	requireGoToolchain(t)
	const label = "app/app.go:Mine SUPERTYPE_OF container:example.com/broken#Binder2"
	root := writeTree(t, map[string]string{
		"go.mod":            "module example.com/x\n\ngo 1.22\n",
		"app/app.go":        "package app\n\ntype Mine struct{}\n\nfunc (Mine) Bind() string { return \"mine\" }\n",
		"broken/go.mod":     healthyBrokenGoMod,
		"broken/binder2.go": "package broken\n\ntype Binder2 interface{ Bind() string }\n",
	})
	state, before := carryWholePass(t, root, label)

	writeFixtureFile(t, root, "broken/go.mod", brokenGoMod)
	after, reason, _ := state.handleSemanticPass(nil)
	if !strings.Contains(reason, "broken") {
		t.Fatalf("second pass reason = %q, want it to name the broken module", reason)
	}
	edge := before[label]
	if carryDeleted(after, edge.ID) {
		t.Fatalf("the pass retracted %s although module broken failed to load", label)
	}
	if !carryUpserted(after, edge.ID) || !carryHasNode(after, edge.ToID) {
		t.Errorf("the pass did not re-upsert %s with its placeholder", label)
	}
}
