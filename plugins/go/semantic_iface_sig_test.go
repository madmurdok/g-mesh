package main

import (
	"go/token"
	"go/types"
	"strings"
	"testing"
)

// An embedded field and a field named "embed" of the same type are different
// structs, so their canonical renderings must differ.
func TestCanonicalTypeTellsAnEmbeddedFieldFromAFieldNamedEmbed(t *testing.T) {
	pkg := types.NewPackage("example.com/p", "p")
	named := types.NewNamed(types.NewTypeName(token.NoPos, pkg, "T", nil), types.NewStruct(nil, nil), nil)
	embedded := types.NewStruct([]*types.Var{types.NewField(token.NoPos, pkg, "T", named, true)}, nil)
	field := types.NewStruct([]*types.Var{types.NewField(token.NoPos, pkg, "embed", named, false)}, nil)

	var a, b strings.Builder
	writeCanonicalType(&a, embedded)
	writeCanonicalType(&b, field)
	if a.String() == b.String() {
		t.Fatalf("embedded and named field render the same: %q", a.String())
	}
}
