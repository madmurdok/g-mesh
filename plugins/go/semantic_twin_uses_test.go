package main

// The semantic pass re-answers a structural use of a build-tag twin.
// The structural tier emits a name-keyed edge onto a placeholder for every
// `pkg.Name` and sibling-file bare name; when the name is declared in two
// build-tag twins core sees two candidates and refuses the edge. The
// semantic pass retracts that edge and emits one of the same kind onto the
// twin the host build compiles - and touches nothing else.
//
// The fixture is twinFiles plus:
//
//	lib/vars.go       //go:build !tagx  twinned var JSON, func validate
//	lib/vars_tagx.go  //go:build tagx   the excluded twins
//	lib/single.go                       non-twinned var Single, func check
//	lib/check.go                        bare sibling-file uses of all four
//	app/use.go                          lib.JSON, lib.Binder, lib.Single, fmt
//	app/use_tagx.go   //go:build tagx   lib.JSON in a file the host skips

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

var twinUseFiles = map[string]string{
	"lib/vars.go": `//go:build !tagx

package lib

var JSON = "json"

func validate(v string) bool { return v != "" }
`,
	"lib/vars_tagx.go": `//go:build tagx

package lib

var JSON = "msgpack"

func validate(v string) bool { return len(v) > 0 }
`,
	"lib/single.go": `package lib

var Single = 1

func check(v string) bool { return v != "-" }
`,
	"lib/check.go": `package lib

func Validate(v string) bool {
	return validate(v) && check(v) && JSON != "" && Single > 0
}
`,
	"app/use.go": `package app

import (
	"fmt"

	"example.com/twins/lib"
)

func UseJSON() string { return lib.JSON }

func UseBinderType(b lib.Binder) string { return "" }

func UseSingle() int { return lib.Single }

func UseFmt() string { return fmt.Sprint(1) }
`,
	"app/use_tagx.go": `//go:build tagx

package app

import "example.com/twins/lib"

func UseJSONTagx() string { return lib.JSON }
`,
}

func twinUseFixture(t *testing.T) string {
	t.Helper()
	outside := filepath.ToSlash(filepath.Join(t.TempDir(), "outside-project-root", "outside.go"))
	files := make(map[string]string, len(twinFiles)+len(twinUseFiles))
	for name, contents := range twinFiles {
		files[name] = strings.ReplaceAll(contents, "{{OUTSIDE}}", outside)
	}
	for name, contents := range twinUseFiles {
		files[name] = contents
	}
	return writeTree(t, files)
}

// structuralUses is what the structural tier recorded for the fixture: every
// placeholderUse by "file:From KIND Name" (e.g. "app/use.go:UseJSON
// REFERENCES JSON"), and the id of every structural edge it emitted.
type structuralUses struct {
	byLabel map[string]placeholderUse
	edges   map[string]bool
}

func extractStructuralUses(t *testing.T, root string) structuralUses {
	t.Helper()
	ws := loadWorkspace(root)
	out := structuralUses{byLabel: map[string]placeholderUse{}, edges: map[string]bool{}}
	for _, relPath := range walkProjectFiles(root) {
		content, err := os.ReadFile(filepath.Join(canonicalizeProjectRoot(root), filepath.FromSlash(relPath)))
		if err != nil {
			t.Fatalf("read %s: %v", relPath, err)
		}
		graph := extractFile(ws, relPath, content)
		names := map[string]string{}
		for _, node := range graph.nodes {
			names[node.ID] = node.QualifiedName
		}
		for _, edge := range graph.edges {
			out.edges[edge.ID] = true
		}
		for _, use := range graph.placeholderUses {
			label := relPath + ":" + names[use.FromID] + " " + use.Kind + " " + use.Name
			out.byLabel[label] = use
		}
	}
	return out
}

func (s structuralUses) require(t *testing.T, label string) placeholderUse {
	t.Helper()
	use, ok := s.byLabel[label]
	if !ok {
		labels := make([]string, 0, len(s.byLabel))
		for l := range s.byLabel {
			labels = append(labels, l)
		}
		sort.Strings(labels)
		t.Fatalf("sanity: the structural tier recorded no use %q; recorded:\n  %s", label, strings.Join(labels, "\n  "))
	}
	return use
}

func TestSemanticPassReanswersStructuralUsesOfTwins(t *testing.T) {
	requireGoToolchain(t)
	root := twinUseFixture(t)
	uses := extractStructuralUses(t, root)

	diff, reason, _ := newPluginState(root).handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("a whole-project pass answered incomplete: %s", reason)
	}
	got, _ := renderTwinEdges(t, root, diff)
	retracted := map[string]bool{}
	for _, id := range diff.DeleteEdgeIds {
		retracted[id] = true
	}

	requireRetracted := func(t *testing.T, label string) {
		t.Helper()
		if use := uses.require(t, label); !retracted[use.EdgeID] {
			t.Fatalf("the structural edge of %q was not retracted; deletes were %v", label, diff.DeleteEdgeIds)
		}
	}
	requireKept := func(t *testing.T, label string) {
		t.Helper()
		if use := uses.require(t, label); retracted[use.EdgeID] {
			t.Fatalf("the structural edge of %q was retracted", label)
		}
	}
	requireEdge := func(t *testing.T, want string) {
		t.Helper()
		if !containsString(got, want) {
			t.Fatalf("the semantic pass did not emit %s; got\n  %s", want, strings.Join(got, "\n  "))
		}
	}
	requireNoEdgeFrom := func(t *testing.T, from string) {
		t.Helper()
		for _, edge := range got {
			if strings.HasPrefix(edge, from+" ") {
				t.Fatalf("the semantic pass emitted %s; uses from %s must be left alone", edge, from)
			}
		}
	}

	// B1: lib.JSON as a value, and lib.Binder as a type: a structural
	// REFERENCES onto a twinned name is retracted and re-stated onto the
	// host-built twin.
	t.Run("ReferencesOfATwinIsReanswered", func(t *testing.T) {
		requireRetracted(t, "app/use.go:UseJSON REFERENCES JSON")
		requireEdge(t, "app/use.go:UseJSON REFERENCES file:lib/vars.go#JSON")
		requireRetracted(t, "app/use.go:UseBinderType REFERENCES Binder")
		requireEdge(t, "app/use.go:UseBinderType REFERENCES file:lib/binding.go#Binder")
	})

	// B2: lib.New(), a placeholder CALLS onto a twinned func.
	t.Run("CallOfATwinIsReanswered", func(t *testing.T) {
		requireRetracted(t, "app/app.go:UseBinder CALLS New")
		requireEdge(t, "app/app.go:UseBinder CALLS file:lib/binding.go#New")
	})

	// B3: bare sibling-file names (useIdent step 4), a call and a value.
	t.Run("BareSiblingUsesOfATwinAreReanswered", func(t *testing.T) {
		requireRetracted(t, "lib/check.go:Validate CALLS validate")
		requireEdge(t, "lib/check.go:Validate CALLS file:lib/vars.go#validate")
		requireRetracted(t, "lib/check.go:Validate REFERENCES JSON")
		requireEdge(t, "lib/check.go:Validate REFERENCES file:lib/vars.go#JSON")
	})

	// B4: non-twinned names keep their structural edge and get no semantic
	// one, by selector and by bare name. (Validate's own semantic edges are
	// all twinned, so it is checked by target, not by from.)
	t.Run("NonTwinnedUsesAreLeftAlone", func(t *testing.T) {
		requireKept(t, "app/use.go:UseSingle REFERENCES Single")
		requireNoEdgeFrom(t, "app/use.go:UseSingle")
		requireKept(t, "lib/check.go:Validate CALLS check")
		requireKept(t, "lib/check.go:Validate REFERENCES Single")
		for _, edge := range got {
			if strings.HasSuffix(edge, "#check") || strings.HasSuffix(edge, "#Single") {
				t.Fatalf("the semantic pass re-answered a non-twinned use: %s", edge)
			}
		}
	})

	// B5 / brief 6: a use go/types has no answer for (a file the host does
	// not build) or resolves outside the project (fmt) is left alone.
	t.Run("UnresolvedAndOutOfProjectUsesAreLeftAlone", func(t *testing.T) {
		requireKept(t, "app/use_tagx.go:UseJSONTagx REFERENCES JSON")
		requireNoEdgeFrom(t, "app/use_tagx.go:UseJSONTagx")
		requireKept(t, "app/use.go:UseFmt CALLS Sprint")
		requireNoEdgeFrom(t, "app/use.go:UseFmt")
	})

	// B6: lib.Legacy is twinned, but its host-built declaration sits behind
	// a `//line` directive naming a file the host does not compile, so
	// targetScopeOf keeps the container scope and the use is not touched.
	t.Run("TwinNotDeclaredInAHostBuiltFileIsLeftAlone", func(t *testing.T) {
		for _, label := range []string{
			"app/app.go:UseLegacy REFERENCES Legacy",
			"app/app.go:UseOutside REFERENCES Outside",
			"app/app.go:UsePhantom REFERENCES Phantom",
		} {
			requireKept(t, label)
		}
		for _, edge := range got {
			if strings.HasSuffix(edge, "#Legacy") || strings.HasSuffix(edge, "#Outside") || strings.HasSuffix(edge, "#Phantom") {
				t.Fatalf("the semantic pass re-answered a twin the host does not build: %s", edge)
			}
		}
	})

	// B7: every recorded EdgeID names an edge the structural tier emitted,
	// for both kinds - otherwise a retraction misses its edge.
	t.Run("RecordedEdgeIDsAreStructuralEdges", func(t *testing.T) {
		kinds := map[string]bool{}
		for label, use := range uses.byLabel {
			if !uses.edges[use.EdgeID] {
				t.Fatalf("the recorded edge id of %q is not an edge the structural tier emitted", label)
			}
			kinds[use.Kind] = true
		}
		if !kinds[edgeKindCalls] || !kinds[edgeKindReferences] {
			t.Fatalf("sanity: expected both CALLS and REFERENCES uses, got kinds %v", kinds)
		}
	})

	// B8: a conversion of a twinned type is still a CALLS retracted and
	// re-stated as REFERENCES, not a CALLS onto the file-scoped twin.
	t.Run("ConversionOfATwinStaysAReference", func(t *testing.T) {
		requireRetracted(t, "app/app.go:Convert CALLS Kelvin")
		requireEdge(t, "app/app.go:Convert REFERENCES file:lib/binding.go#Kelvin")
		if containsString(got, "app/app.go:Convert CALLS file:lib/binding.go#Kelvin") {
			t.Fatalf("a conversion of a twinned type was re-answered as a CALLS")
		}
	})
}

// B9: a per-file pass of the referencing file re-answers a REFERENCES use
// of an imported package's twin the same way (loadReferencedTwins).
func TestSemanticPassPerFileReanswersReferencesOfAnImportedTwin(t *testing.T) {
	requireGoToolchain(t)
	root := twinUseFixture(t)
	uses := extractStructuralUses(t, root)

	diff, reason, _ := newPluginState(root).handleSemanticPass([]string{"app/use.go"})
	if reason != "" {
		t.Fatalf("a per-file pass answered incomplete: %s", reason)
	}
	got, _ := renderTwinEdges(t, root, diff)
	want := "app/use.go:UseJSON REFERENCES file:lib/vars.go#JSON"
	if !containsString(got, want) {
		t.Fatalf("per-file pass of app/use.go did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
	}
	use := uses.require(t, "app/use.go:UseJSON REFERENCES JSON")
	if !containsString(diff.DeleteEdgeIds, use.EdgeID) {
		t.Fatalf("per-file pass did not retract the structural lib.JSON edge; deletes were %v", diff.DeleteEdgeIds)
	}
}
