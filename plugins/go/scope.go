package main

// Lexical scope tracking, and Go's universe block.
//
// # Why this exists, and why go/ast's own scopes are not used
//
// A bare identifier that is neither declared in this file nor a builtin gets
// a package-scoped placeholder (extract.go), on the theory that it is
// declared in a sibling file of the same package. That theory is false for
// every *local* name - a parameter, a `:=` variable, a named result, a
// receiver, a type parameter, a local `var`/`const`/`type` - and emitting a
// placeholder for one produces an edge onto a package-level symbol the call
// site cannot even see. That is the single largest source of wrong edges a
// type-checker-free Go extractor can have, and this file is what rules it
// out: a name the scope chain binds is dropped, always, without ever asking
// what it is bound *to*.
//
// go/ast has `ast.Object`/`ast.Scope` fields that look like they answer
// this, and `parser.ParseFile` even fills some of them in. They are not used
// here, for two reasons:
//
//   - They are deprecated (go/ast's own documentation says so) and were
//     never correct: the parser resolves names with no type information, so
//     what it records is a best effort that the go/types pass was always
//     meant to replace.
//   - A file parsed with `AllErrors` is a *partial* AST. Whatever resolution
//     the parser managed is partial in the same way, and there is no flag
//     saying which parts are trustworthy. Deriving "this name is local" from
//     it would mean trusting exactly the thing the project's standing rule -
//     a missing edge beats a wrong one - says to distrust.
//
// So the chain below is built by the walk itself, from the declarations it
// actually sees, which works identically on a complete and on a partial AST.
//
// # Where this deliberately over- and under-approximates
//
// Go's scoping is honoured statement by statement (uses.go walks each block
// in source order and declares a name at the point it is declared, so
// `x := x` reads the outer `x` on the right), with three deliberate
// simplifications, each of which can only ever *lose* an edge:
//
//   - **Labels are not tracked at all, they are skipped.** A label lives in
//     its own namespace (`break L` can never mean a package-level `L`), so
//     uses.go never treats a `LabeledStmt`'s label or a `BranchStmt`'s label
//     as an identifier use. Nothing has to be bound for that to be exact.
//   - **Type parameters share the chain with values.** Go keeps type and
//     value namespaces distinct only for the universe block; within a
//     function a type parameter `T` and a variable `T` cannot coexist
//     anyway, so one chain is enough.
//   - **A name bound anywhere in an enclosing block shadows for the rest of
//     that block, never before it.** This is exactly Go's rule, and the one
//     place it is loosened is the receiver/parameter/result set of a
//     function, which is bound before the body is walked - which is also
//     exactly Go's rule.

import "go/ast"

// scope is one link of the lexical chain: the names a block binds, and the
// block enclosing it. Bindings are added as the walk meets them, so a scope
// is mutated in place while its own statements are walked.
type scope struct {
	names  map[string]bool
	parent *scope
}

func newScope(parent *scope) *scope {
	return &scope{names: map[string]bool{}, parent: parent}
}

// child opens a nested block.
func (s *scope) child() *scope { return newScope(s) }

// declare binds a name from this point on. The blank identifier binds
// nothing (it is not a name), and neither does an empty one.
func (s *scope) declare(name string) {
	if name == "" || name == "_" {
		return
	}
	s.names[name] = true
}

// declareIdent binds an identifier, ignoring a nil one (a partial AST can
// produce those).
func (s *scope) declareIdent(ident *ast.Ident) {
	if ident != nil {
		s.declare(ident.Name)
	}
}

// declareFieldNames binds the names of a parameter, result or receiver list.
// Only the names: a field's *type* is a use and is walked separately.
func (s *scope) declareFieldNames(list *ast.FieldList) {
	if list == nil {
		return
	}
	for _, field := range list.List {
		for _, name := range field.Names {
			s.declareIdent(name)
		}
	}
}

// bound reports whether the name is bound by this scope or any enclosing
// one. Knowing that is enough to drop a use; what the name is bound to never
// needs answering.
func (s *scope) bound(name string) bool {
	for current := s; current != nil; current = current.parent {
		if current.names[name] {
			return true
		}
	}
	return false
}

// universeNames is Go's universe block, as of Go 1.21+ (`min`, `max` and
// `clear` were added there; `any` and `comparable` in 1.18). A use of one of
// these is not a package symbol, so it gets no edge and no placeholder.
//
// Listing them rather than emitting a placeholder that would simply never
// resolve is not only tidiness: a package *may* declare a name that shadows
// a universe one (`func len(...)` at package level is legal Go), and
// extract.go checks the file's own declarations *before* this map for
// exactly that reason - so a package that shadows a builtin still links,
// while one that does not emits nothing rather than an address no container
// will ever answer.
var universeNames = map[string]bool{
	// Types.
	"any": true, "bool": true, "byte": true, "comparable": true,
	"complex64": true, "complex128": true, "error": true, "float32": true,
	"float64": true, "int": true, "int8": true, "int16": true, "int32": true,
	"int64": true, "rune": true, "string": true, "uint": true, "uint8": true,
	"uint16": true, "uint32": true, "uint64": true, "uintptr": true,
	// Constants and the zero value.
	"true": true, "false": true, "iota": true, "nil": true,
	// Functions.
	"append": true, "cap": true, "clear": true, "close": true,
	"complex": true, "copy": true, "delete": true, "imag": true,
	"len": true, "make": true, "max": true, "min": true, "new": true,
	"panic": true, "print": true, "println": true, "real": true,
	"recover": true,
}
