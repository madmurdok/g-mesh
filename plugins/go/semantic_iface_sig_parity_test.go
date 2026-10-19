package main

import (
	"go/token"
	"go/types"
	"os"
	"testing"

	"golang.org/x/tools/go/gcexportdata"
	"golang.org/x/tools/go/packages"
)

// sigParityLib exercises what the signature writes beyond exported names:
// an embedded interface with an unexported method from another package, an
// unexported struct field, a func alias from another package, an embedded
// alias field and a generic interface's constraint.
const sigParityLib = `package p

import "example.com/x/q"

type T struct{}

type A = T

type I interface {
	q.S
	m(func() q.Fn)
	Fields(struct{ a, B int }, struct{ A })
}

type H[U comparable] interface{ Get() U }
`

// An unchanged interface renders the same from a source load and from the
// package's export data, so the GM-620 gate does not fire on every pass.
func TestInterfaceSignatureSourceAndExportDataAgree(t *testing.T) {
	requireGoToolchain(t)
	root := writeTree(t, map[string]string{
		"go.mod": "module example.com/x\n\ngo 1.22\n",
		"p/p.go": sigParityLib,
		"q/q.go": "package q\n\ntype S interface {\n\tm()\n\tExp()\n}\n\ntype Fn = func(int) string\n",
	})
	load := func(mode packages.LoadMode) *packages.Package {
		t.Helper()
		pkgs, err := packages.Load(&packages.Config{Mode: mode, Dir: root}, "./p")
		if err != nil || len(pkgs) != 1 || len(pkgs[0].Errors) > 0 {
			if len(pkgs) == 1 {
				t.Fatalf("load ./p: %v %v", err, pkgs[0].Errors)
			}
			t.Fatalf("load ./p: %v %v", err, pkgs)
		}
		return pkgs[0]
	}
	src := load(packages.NeedName | packages.NeedTypes | packages.NeedSyntax | packages.NeedTypesInfo | packages.NeedImports | packages.NeedDeps)
	exp := load(packages.NeedName | packages.NeedExportFile)
	f, err := os.Open(exp.ExportFile)
	if err != nil {
		t.Fatalf("open export data: %v", err)
	}
	defer f.Close()
	r, err := gcexportdata.NewReader(f)
	if err != nil {
		t.Fatalf("export data reader: %v", err)
	}
	expPkg, err := gcexportdata.Read(r, token.NewFileSet(), map[string]*types.Package{}, exp.PkgPath)
	if err != nil {
		t.Fatalf("read export data: %v", err)
	}
	for _, name := range []string{"I", "H"} {
		s, e := sigOf(t, src.Types, name), sigOf(t, expPkg, name)
		if s != e {
			t.Errorf("%s: source %q, export data %q", name, s, e)
		}
	}
}
