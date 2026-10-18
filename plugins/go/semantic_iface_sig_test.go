package main

import (
	"go/ast"
	"go/importer"
	"go/parser"
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

// sigImporter type-checks in-memory packages on demand, so a test can
// declare several packages without a module on disk.
type sigImporter struct {
	t    *testing.T
	fset *token.FileSet
	srcs map[string]string
	pkgs map[string]*types.Package
}

func (im *sigImporter) Import(path string) (*types.Package, error) {
	if pkg, ok := im.pkgs[path]; ok {
		return pkg, nil
	}
	src, ok := im.srcs[path]
	if !ok {
		return importer.Default().Import(path)
	}
	f, err := parser.ParseFile(im.fset, path+"/src.go", src, 0)
	if err != nil {
		im.t.Fatalf("parse %s: %v", path, err)
	}
	pkg, err := (&types.Config{Importer: im}).Check(path, im.fset, []*ast.File{f}, nil)
	if err != nil {
		im.t.Fatalf("type-check %s: %v", path, err)
	}
	im.pkgs[path] = pkg
	return pkg, nil
}

// checkSigPackages type-checks srcs (import path -> one file's source) and
// returns the packages by path.
func checkSigPackages(t *testing.T, srcs map[string]string) map[string]*types.Package {
	t.Helper()
	im := &sigImporter{t: t, fset: token.NewFileSet(), srcs: srcs, pkgs: map[string]*types.Package{}}
	for path := range srcs {
		if _, err := im.Import(path); err != nil {
			t.Fatal(err)
		}
	}
	return im.pkgs
}

func sigOf(t *testing.T, pkg *types.Package, name string) string {
	t.Helper()
	obj := pkg.Scope().Lookup(name)
	if obj == nil {
		t.Fatalf("%s has no %s", pkg.Path(), name)
	}
	iface, ok := obj.Type().Underlying().(*types.Interface)
	if !ok {
		t.Fatalf("%s.%s is not an interface", pkg.Path(), name)
	}
	return interfaceSignature(iface)
}

func canonicalOf(typ types.Type) string {
	var b strings.Builder
	writeCanonicalType(&b, typ)
	return b.String()
}

// Unexported names from different packages are always different (the spec),
// so the same interface text declared in two packages is two method sets
// when a name is unexported, and one when every name is exported.
func TestInterfaceSignatureQualifiesUnexportedNamesByPackage(t *testing.T) {
	const src = `package %s

type J interface{ m() }

type F interface{ M(struct{ a int }) }

type E interface {
	M(struct{ A int })
	N() int
}
`
	pkgs := checkSigPackages(t, map[string]string{
		"example.com/b": strings.Replace(src, "%s", "b", 1),
		"example.com/c": strings.Replace(src, "%s", "c", 1),
	})
	b, c := pkgs["example.com/b"], pkgs["example.com/c"]
	for _, name := range []string{"J", "F"} {
		if sb, sc := sigOf(t, b, name), sigOf(t, c, name); sb == sc {
			t.Errorf("b.%s and c.%s render the same: %q", name, name, sb)
		}
	}
	sb, sc := sigOf(t, b, "E"), sigOf(t, c, "E")
	if sb != sc {
		t.Errorf("exported-only E renders per package: b %q, c %q", sb, sc)
	}
	if strings.Contains(sb, "example.com/b") {
		t.Errorf("exported names are qualified: %q", sb)
	}
}

// Two function-local types of one name in one package are different types
// and render differently; a package-level type renders without an ordinal.
func TestCanonicalTypeTellsFunctionLocalTypesOfOneNameApart(t *testing.T) {
	fset := token.NewFileSet()
	const src = `package p

type L struct{}

func A() any { type L struct{}; return L{} }

func B() any {
	{
		type L struct{}
		return L{}
	}
}
`
	f, err := parser.ParseFile(fset, "p.go", src, 0)
	if err != nil {
		t.Fatal(err)
	}
	info := &types.Info{Defs: map[*ast.Ident]types.Object{}}
	pkg, err := (&types.Config{}).Check("example.com/p", fset, []*ast.File{f}, info)
	if err != nil {
		t.Fatal(err)
	}
	var locals []string
	for ident, obj := range info.Defs {
		if tn, ok := obj.(*types.TypeName); ok && tn.Name() == "L" && tn.Parent() != pkg.Scope() {
			locals = append(locals, canonicalOf(tn.Type())+"@"+fset.Position(ident.Pos()).String())
		}
	}
	if len(locals) != 2 {
		t.Fatalf("want 2 local L, got %v", locals)
	}
	r0, r1 := strings.Split(locals[0], "@")[0], strings.Split(locals[1], "@")[0]
	if r0 == r1 {
		t.Errorf("two local L render the same: %v", locals)
	}
	top := canonicalOf(pkg.Scope().Lookup("L").Type())
	if top != "example.com/p.L" {
		t.Errorf("package-level L renders %q, want %q", top, "example.com/p.L")
	}
	if r0 == top || r1 == top {
		t.Errorf("a local L renders like the package-level one: %v vs %q", locals, top)
	}
}

// A type parameter's constraint is part of a generic interface's method set,
// and a constraint that mentions its own parameter still renders.
func TestInterfaceSignatureCarriesTypeParameterConstraints(t *testing.T) {
	pkgs := checkSigPackages(t, map[string]string{"example.com/p": `package p

type H[T any] interface{ Get() T }

type H2[T comparable] interface{ Get() T }

type Self[T interface{ M() T }] interface{ Get() T }
`})
	p := pkgs["example.com/p"]
	if h, h2 := sigOf(t, p, "H"), sigOf(t, p, "H2"); h == h2 {
		t.Errorf("constraints any and comparable render the same: %q", h)
	}
	if s := sigOf(t, p, "Self"); !strings.Contains(s, "$0[") {
		t.Errorf("self-referencing constraint not rendered: %q", s)
	}
}

// Embedding an alias `A = T` names the field A, so struct{ A } and
// struct{ T } are different types and render differently.
func TestCanonicalTypeNamesAnEmbeddedAliasField(t *testing.T) {
	pkgs := checkSigPackages(t, map[string]string{"example.com/p": `package p

type T struct{}

type A = T

type I1 interface{ M(struct{ A }) }

type I2 interface{ M(struct{ T }) }
`})
	p := pkgs["example.com/p"]
	if s1, s2 := sigOf(t, p, "I1"), sigOf(t, p, "I2"); s1 == s2 {
		t.Errorf("struct{ A } and struct{ T } render the same: %q", s1)
	}
}
