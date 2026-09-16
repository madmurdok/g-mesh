package main

// The use-site walk: the half of extraction that turns identifiers into
// edges. extract.go's doc comment has the four-way classification this
// implements; scope.go has why locals are dropped and how the chain is
// built. This file is the mechanism.
//
// # The walk is hand-written, not `ast.Inspect`
//
// `ast.Inspect` visits every node and knows nothing about scopes, and the
// three things this walk has to get right are all about *which* child is
// visited in *which* scope:
//
//   - a statement list declares names as it goes, so `x := x` must read the
//     right-hand `x` in the scope *before* the left-hand one is bound;
//   - a parameter's *type* resolves in the enclosing scope while its *name*
//     binds in the body (Go's own rule: "the scope of a function parameter
//     is the function body"), so types are walked before names are declared;
//   - several children must not be visited at all - a label, a struct field
//     name, a selector's `.Sel`, a bare key in a composite literal - because
//     each of them lives in a namespace where a package-level symbol of the
//     same spelling can never be what is meant.
//
// An `Inspect`-based walk would have to undo all three after the fact.

import (
	"go/ast"
	"go/token"
	"strconv"
)

// useContext is who an edge is written *from* at a given point in the walk.
type useContext struct {
	// symbolID is the `from` of REFERENCES edges: the nearest enclosing
	// declared symbol, falling back to the File node at top level.
	symbolID string
	// callerID is the `from` of CALLS edges: the nearest enclosing declared
	// symbol, or "" where there is none (a call in a top-level initializer
	// of a blank-named `var`), in which case the call degrades to a
	// REFERENCES edge from symbolID rather than claiming a caller that does
	// not exist.
	callerID string
}

// useAll walks every declaration's body and type expressions, after
// declareAll has created the nodes they may point at.
func (e *extractor) useAll(file *ast.File) {
	fileScope := newScope(nil)
	for _, decl := range file.Decls {
		switch d := decl.(type) {
		case *ast.FuncDecl:
			e.useFuncDecl(d, fileScope)
		case *ast.GenDecl:
			e.useGenDecl(d, fileScope)
		}
	}
}

// contextFor builds the `from` pair for a declaration whose node id may not
// exist (a blank-named var, a receiver this tier could not read).
func (e *extractor) contextFor(id string) useContext {
	if id == "" {
		return useContext{symbolID: e.fileNodeID}
	}
	return useContext{symbolID: id, callerID: id}
}

func (e *extractor) useFuncDecl(d *ast.FuncDecl, fileScope *scope) {
	if d.Type == nil {
		return
	}
	ctx := e.contextFor(e.declaredFuncID(d))

	body := e.openSignatureScope(fileScope, ctx, d.Type.TypeParams, d.Recv, d.Type.Params, d.Type.Results)
	e.walkBlock(d.Body, body, ctx)
}

// declaredFuncID recomputes the node id declareFunc gave this declaration,
// or "" if it declared none. Recomputing rather than threading a map keeps
// the two passes independent: nothing here can be out of step with
// declareFunc except by disagreeing about the id, which is the one thing
// nodeIDFor makes a pure function of the declaration.
func (e *extractor) declaredFuncID(d *ast.FuncDecl) string {
	if d.Name == nil || d.Name.Name == "" || d.Name.Name == "_" {
		return ""
	}
	if d.Recv != nil && len(d.Recv.List) > 0 {
		receiver, ok := receiverTypeName(d.Recv.List[0].Type)
		if !ok {
			return ""
		}
		return e.knownID(nodeIDFor(e.relPath, nodeKindFunction, receiver+"."+d.Name.Name, "method"))
	}
	if d.Name.Name == "init" {
		// Matched by position rather than recomputed: declareFunc numbers a
		// file's inits in source order, and this pass meets them in the same
		// order, so the first node whose id is still unclaimed is this one.
		return e.nextInitID()
	}
	return e.knownID(nodeIDFor(e.relPath, nodeKindFunction, d.Name.Name, "function"))
}

func (e *extractor) knownID(id string) string {
	if e.nodeSeen[id] {
		return id
	}
	return ""
}

// nextInitID hands out this file's init node ids in the order declareFunc
// created them.
func (e *extractor) nextInitID() string {
	nativeKind := "init"
	if e.usedInits > 0 {
		nativeKind = "init#" + strconv.Itoa(e.usedInits)
	}
	e.usedInits++
	return e.knownID(nodeIDFor(e.relPath, nodeKindFunction, "init", nativeKind))
}

// openSignatureScope walks a function's signature and returns the scope its
// body runs in.
//
// The ordering is Go's own and matters: type parameters are in scope for the
// whole signature (`func f[T any](x T)`), while receiver, parameter and
// result *names* are in scope only for the body - so their types are walked
// first, in a scope that does not yet bind them, and the names are declared
// afterwards.
func (e *extractor) openSignatureScope(
	outer *scope,
	ctx useContext,
	typeParams, recv, params, results *ast.FieldList,
) *scope {
	inner := outer.child()

	if typeParams != nil {
		// Declared before their constraints are walked: a type parameter
		// list may refer to itself (`[S ~[]E, E any]`).
		inner.declareFieldNames(typeParams)
		e.walkFieldTypes(typeParams, inner, ctx)
	}

	e.walkFieldTypes(recv, inner, ctx)
	e.walkFieldTypes(params, inner, ctx)
	e.walkFieldTypes(results, inner, ctx)

	inner.declareFieldNames(recv)
	inner.declareFieldNames(params)
	inner.declareFieldNames(results)
	return inner
}

// walkFieldTypes walks the *types* of a field list and never its names. A
// field name - a parameter, a result, a struct field, an interface method -
// is a declaration, not a use.
func (e *extractor) walkFieldTypes(list *ast.FieldList, sc *scope, ctx useContext) {
	if list == nil {
		return
	}
	for _, field := range list.List {
		e.walkExpr(field.Type, sc, ctx)
	}
}

func (e *extractor) useGenDecl(d *ast.GenDecl, fileScope *scope) {
	switch d.Tok {
	case token.TYPE:
		for _, spec := range d.Specs {
			typeSpec, ok := spec.(*ast.TypeSpec)
			if !ok || typeSpec.Name == nil {
				continue
			}
			e.useTypeSpec(typeSpec, fileScope)
		}
	case token.VAR, token.CONST:
		for _, spec := range d.Specs {
			if valueSpec, ok := spec.(*ast.ValueSpec); ok {
				e.useValueSpec(d, valueSpec, fileScope)
			}
		}
	}
}

func (e *extractor) useTypeSpec(spec *ast.TypeSpec, fileScope *scope) {
	ctx := e.contextFor(e.knownID(nodeIDFor(e.relPath, nodeKindType, spec.Name.Name, typeNativeKind(spec))))

	inner := fileScope.child()
	if spec.TypeParams != nil {
		inner.declareFieldNames(spec.TypeParams)
		e.walkFieldTypes(spec.TypeParams, inner, ctx)
	}

	// An interface's methods each get their own `from`, so a reference to a
	// type in `M(ctx context.Context) error` is attributed to `I.M` rather
	// than to `I`. Everything else - a struct's fields, a defined type's
	// underlying type, an embedded interface - is attributed to the type.
	if iface, ok := spec.Type.(*ast.InterfaceType); ok {
		e.useInterface(spec.Name.Name, iface, inner, ctx)
		return
	}
	e.walkExpr(spec.Type, inner, ctx)
}

func (e *extractor) useInterface(interfaceName string, iface *ast.InterfaceType, sc *scope, ifaceCtx useContext) {
	if iface.Methods == nil {
		return
	}
	for _, field := range iface.Methods.List {
		funcType, ok := field.Type.(*ast.FuncType)
		if !ok {
			// An embedded interface (`io.Reader`) or a type-set element
			// (`~int | ~string`): an ordinary type reference from the
			// interface itself. Embedding promotes the embedded methods into
			// this interface's method set, which is a *method set* question
			// and therefore GM-281's (go/types); nothing here claims it.
			e.walkExpr(field.Type, sc, ifaceCtx)
			continue
		}
		ctx := ifaceCtx
		if len(field.Names) > 0 && field.Names[0] != nil {
			qualifiedName := interfaceName + "." + field.Names[0].Name
			if id := e.knownID(nodeIDFor(e.relPath, nodeKindFunction, qualifiedName, "interface_method")); id != "" {
				ctx = e.contextFor(id)
			}
		}
		e.openSignatureScope(sc, ctx, funcType.TypeParams, nil, funcType.Params, funcType.Results)
	}
}

func (e *extractor) useValueSpec(gen *ast.GenDecl, spec *ast.ValueSpec, fileScope *scope) {
	nativeKind := "var"
	if gen.Tok == token.CONST {
		nativeKind = "const"
	}
	// Attributed to the spec's first named variable; a spec that names only
	// `_` has no node, and its initializer's uses hang off the File node.
	id := ""
	for _, ident := range spec.Names {
		if ident != nil && ident.Name != "" && ident.Name != "_" {
			id = e.knownID(nodeIDFor(e.relPath, nodeKindVariable, ident.Name, nativeKind))
			break
		}
	}
	ctx := e.contextFor(id)

	e.walkExpr(spec.Type, fileScope, ctx)
	for _, value := range spec.Values {
		e.walkExpr(value, fileScope, ctx)
	}
}

// --- statements ---------------------------------------------------------

// walkBlock opens a block scope and walks its statements in order, so a name
// declared partway through shadows only from that point on.
func (e *extractor) walkBlock(block *ast.BlockStmt, sc *scope, ctx useContext) {
	if block == nil {
		return
	}
	inner := sc.child()
	for _, stmt := range block.List {
		e.walkStmt(stmt, inner, ctx)
	}
}

func (e *extractor) walkStmt(stmt ast.Stmt, sc *scope, ctx useContext) {
	switch n := stmt.(type) {
	case nil:
		return

	case *ast.EmptyStmt, *ast.BadStmt:
		return

	case *ast.BranchStmt:
		// `break L`, `continue L`, `goto L`, `fallthrough`. A label lives in
		// its own namespace and is function-scoped, so it can never denote a
		// package symbol - which is why labels need no scope tracking at
		// all: not visiting them is exact, not an approximation.
		return

	case *ast.LabeledStmt:
		// The label is skipped for the same reason; its statement is not.
		e.walkStmt(n.Stmt, sc, ctx)

	case *ast.DeclStmt:
		e.walkLocalDecl(n.Decl, sc, ctx)

	case *ast.ExprStmt:
		e.walkExpr(n.X, sc, ctx)

	case *ast.SendStmt:
		e.walkExpr(n.Chan, sc, ctx)
		e.walkExpr(n.Value, sc, ctx)

	case *ast.IncDecStmt:
		e.walkExpr(n.X, sc, ctx)

	case *ast.AssignStmt:
		// The right-hand side is always evaluated in the scope *before* a
		// `:=` binds anything, which is what makes `x := x` read the outer
		// `x` and `err := f()` not shadow whatever `f` is.
		for _, rhs := range n.Rhs {
			e.walkExpr(rhs, sc, ctx)
		}
		if n.Tok == token.DEFINE {
			for _, lhs := range n.Lhs {
				if ident, ok := lhs.(*ast.Ident); ok {
					sc.declareIdent(ident)
					continue
				}
				e.walkExpr(lhs, sc, ctx)
			}
			return
		}
		for _, lhs := range n.Lhs {
			e.walkExpr(lhs, sc, ctx)
		}

	case *ast.GoStmt:
		e.walkExpr(n.Call, sc, ctx)

	case *ast.DeferStmt:
		e.walkExpr(n.Call, sc, ctx)

	case *ast.ReturnStmt:
		for _, result := range n.Results {
			e.walkExpr(result, sc, ctx)
		}

	case *ast.BlockStmt:
		e.walkBlock(n, sc, ctx)

	case *ast.IfStmt:
		// `if x := f(); cond {} else {}` - the init statement's bindings
		// cover the condition, the body and the else branch, and nothing
		// after the statement.
		inner := sc.child()
		e.walkStmt(n.Init, inner, ctx)
		e.walkExpr(n.Cond, inner, ctx)
		e.walkBlock(n.Body, inner, ctx)
		e.walkStmt(n.Else, inner, ctx)

	case *ast.SwitchStmt:
		inner := sc.child()
		e.walkStmt(n.Init, inner, ctx)
		e.walkExpr(n.Tag, inner, ctx)
		e.walkClauses(n.Body, inner, ctx)

	case *ast.TypeSwitchStmt:
		// `switch v := x.(type)` binds `v` once, in a scope covering every
		// clause (Go gives each clause its own `v`, but they share the name,
		// and one binding is all this walk needs to know).
		inner := sc.child()
		e.walkStmt(n.Init, inner, ctx)
		e.walkStmt(n.Assign, inner, ctx)
		e.walkClauses(n.Body, inner, ctx)

	case *ast.SelectStmt:
		if n.Body == nil {
			return
		}
		for _, clause := range n.Body.List {
			comm, ok := clause.(*ast.CommClause)
			if !ok {
				e.walkStmt(clause, sc, ctx)
				continue
			}
			inner := sc.child()
			e.walkStmt(comm.Comm, inner, ctx)
			for _, stmt := range comm.Body {
				e.walkStmt(stmt, inner, ctx)
			}
		}

	case *ast.ForStmt:
		inner := sc.child()
		e.walkStmt(n.Init, inner, ctx)
		e.walkExpr(n.Cond, inner, ctx)
		e.walkStmt(n.Post, inner, ctx)
		e.walkBlock(n.Body, inner, ctx)

	case *ast.RangeStmt:
		// The ranged expression is evaluated before the loop variables
		// exist, so it is walked in a scope that does not bind them.
		inner := sc.child()
		e.walkExpr(n.X, inner, ctx)
		if n.Tok == token.DEFINE {
			for _, target := range []ast.Expr{n.Key, n.Value} {
				if ident, ok := target.(*ast.Ident); ok {
					inner.declareIdent(ident)
				}
			}
		} else {
			e.walkExpr(n.Key, inner, ctx)
			e.walkExpr(n.Value, inner, ctx)
		}
		e.walkBlock(n.Body, inner, ctx)

	case *ast.CaseClause:
		// Reachable only from a malformed AST, where a clause turns up
		// outside a switch; handled rather than dropped.
		inner := sc.child()
		for _, expr := range n.List {
			e.walkExpr(expr, inner, ctx)
		}
		for _, stmt := range n.Body {
			e.walkStmt(stmt, inner, ctx)
		}

	case *ast.CommClause:
		inner := sc.child()
		e.walkStmt(n.Comm, inner, ctx)
		for _, stmt := range n.Body {
			e.walkStmt(stmt, inner, ctx)
		}
	}
}

func (e *extractor) walkClauses(body *ast.BlockStmt, sc *scope, ctx useContext) {
	if body == nil {
		return
	}
	for _, stmt := range body.List {
		clause, ok := stmt.(*ast.CaseClause)
		if !ok {
			e.walkStmt(stmt, sc, ctx)
			continue
		}
		// Each clause is its own block: a name declared in one is invisible
		// in the next.
		inner := sc.child()
		for _, expr := range clause.List {
			e.walkExpr(expr, inner, ctx)
		}
		for _, stmt := range clause.Body {
			e.walkStmt(stmt, inner, ctx)
		}
	}
}

// walkLocalDecl handles a `var`, `const` or `type` inside a function body.
//
// None of them becomes a graph node: a local is not addressable from
// anywhere else, so a node for it would be a symbol nothing can ever
// reference. What matters here is that its *name* is bound - otherwise every
// later use of it would look like a package symbol - and that its type and
// initializer are still walked, since those are real uses.
func (e *extractor) walkLocalDecl(decl ast.Decl, sc *scope, ctx useContext) {
	gen, ok := decl.(*ast.GenDecl)
	if !ok {
		return
	}
	for _, spec := range gen.Specs {
		switch s := spec.(type) {
		case *ast.TypeSpec:
			// Declared before its own definition is walked: a local type may
			// be recursive (`type node struct { next *node }`).
			sc.declareIdent(s.Name)
			inner := sc.child()
			if s.TypeParams != nil {
				inner.declareFieldNames(s.TypeParams)
				e.walkFieldTypes(s.TypeParams, inner, ctx)
			}
			e.walkExpr(s.Type, inner, ctx)
		case *ast.ValueSpec:
			e.walkExpr(s.Type, sc, ctx)
			for _, value := range s.Values {
				e.walkExpr(value, sc, ctx)
			}
			for _, name := range s.Names {
				sc.declareIdent(name)
			}
		}
	}
}

// --- expressions --------------------------------------------------------

func (e *extractor) walkExpr(expr ast.Expr, sc *scope, ctx useContext) {
	switch n := expr.(type) {
	case nil:
		return

	case *ast.BadExpr, *ast.BasicLit:
		return

	case *ast.Ident:
		e.useIdent(n, sc, ctx, false)

	case *ast.SelectorExpr:
		e.useSelector(n, sc, ctx, false)

	case *ast.CallExpr:
		e.walkCallee(n.Fun, sc, ctx)
		for _, arg := range n.Args {
			e.walkExpr(arg, sc, ctx)
		}

	case *ast.ParenExpr:
		e.walkExpr(n.X, sc, ctx)

	case *ast.StarExpr:
		e.walkExpr(n.X, sc, ctx)

	case *ast.UnaryExpr:
		e.walkExpr(n.X, sc, ctx)

	case *ast.BinaryExpr:
		e.walkExpr(n.X, sc, ctx)
		e.walkExpr(n.Y, sc, ctx)

	case *ast.IndexExpr:
		e.walkExpr(n.X, sc, ctx)
		e.walkExpr(n.Index, sc, ctx)

	case *ast.IndexListExpr:
		e.walkExpr(n.X, sc, ctx)
		for _, index := range n.Indices {
			e.walkExpr(index, sc, ctx)
		}

	case *ast.SliceExpr:
		e.walkExpr(n.X, sc, ctx)
		e.walkExpr(n.Low, sc, ctx)
		e.walkExpr(n.High, sc, ctx)
		e.walkExpr(n.Max, sc, ctx)

	case *ast.TypeAssertExpr:
		e.walkExpr(n.X, sc, ctx)
		e.walkExpr(n.Type, sc, ctx) // nil in `x.(type)`, handled by the nil case

	case *ast.KeyValueExpr:
		// Outside a composite literal (which handles its own elements) both
		// halves are ordinary expressions.
		e.walkExpr(n.Key, sc, ctx)
		e.walkExpr(n.Value, sc, ctx)

	case *ast.CompositeLit:
		e.walkCompositeLit(n, sc, ctx)

	case *ast.FuncLit:
		// A closure's calls keep the enclosing declared symbol as their
		// caller: a function literal is not a symbol of its own, and
		// attributing its calls to the File node instead would lose the
		// caller for every callback in the codebase.
		body := e.openSignatureScope(sc, ctx, n.Type.TypeParams, nil, n.Type.Params, n.Type.Results)
		e.walkBlock(n.Body, body, ctx)

	case *ast.Ellipsis:
		e.walkExpr(n.Elt, sc, ctx)

	case *ast.ArrayType:
		e.walkExpr(n.Len, sc, ctx)
		e.walkExpr(n.Elt, sc, ctx)

	case *ast.MapType:
		e.walkExpr(n.Key, sc, ctx)
		e.walkExpr(n.Value, sc, ctx)

	case *ast.ChanType:
		e.walkExpr(n.Value, sc, ctx)

	case *ast.StructType:
		// Field *types* only. A field name is a declaration in the struct's
		// own namespace, never a use of a package symbol - and an embedded
		// field, whose "name" is its type, is reached through Type anyway.
		e.walkFieldTypes(n.Fields, sc, ctx)

	case *ast.InterfaceType:
		// An anonymous interface (`interface{ Close() error }`) in a type
		// position: its methods declare nothing addressable, but their
		// signatures still reference types.
		if n.Methods == nil {
			return
		}
		for _, field := range n.Methods.List {
			if funcType, ok := field.Type.(*ast.FuncType); ok {
				e.openSignatureScope(sc, ctx, funcType.TypeParams, nil, funcType.Params, funcType.Results)
				continue
			}
			e.walkExpr(field.Type, sc, ctx)
		}

	case *ast.FuncType:
		e.openSignatureScope(sc, ctx, n.TypeParams, nil, n.Params, n.Results)
	}
}

// walkCompositeLit handles the one place where a bare identifier is
// genuinely ambiguous to a type-checker-free tier.
//
// `T{Field: v}` and `map[K]V{Key: v}` are the same syntax, and which one a
// bare `Key:` is depends entirely on what the literal's type turns out to
// be. A struct field name is not a use of anything, so emitting an edge for
// it would be wrong; a map key that happens to be a package-level constant
// is a real use, so dropping it loses an edge. The two cannot both be
// served, and the project's standing rule decides it: a bare identifier key
// is skipped.
//
// A key that is *not* a bare identifier is unambiguous and is walked: a
// struct field name can only ever be a single identifier, so `pkg.Const:` or
// `someExpr():` is necessarily a map or array key.
func (e *extractor) walkCompositeLit(lit *ast.CompositeLit, sc *scope, ctx useContext) {
	// Nil for an elided inner literal (`[]T{{...}}`), which walkExpr's nil
	// case handles.
	e.walkExpr(lit.Type, sc, ctx)

	for _, element := range lit.Elts {
		keyValue, ok := element.(*ast.KeyValueExpr)
		if !ok {
			e.walkExpr(element, sc, ctx)
			continue
		}
		if _, bare := keyValue.Key.(*ast.Ident); !bare {
			e.walkExpr(keyValue.Key, sc, ctx)
		}
		e.walkExpr(keyValue.Value, sc, ctx)
	}
}

// walkCallee walks the function position of a call, where a resolvable
// target becomes a CALLS edge rather than a REFERENCES one.
func (e *extractor) walkCallee(fun ast.Expr, sc *scope, ctx useContext) {
	switch n := fun.(type) {
	case nil:
		return
	case *ast.Ident:
		e.useIdent(n, sc, ctx, true)
	case *ast.SelectorExpr:
		e.useSelector(n, sc, ctx, true)
	case *ast.ParenExpr:
		e.walkCallee(n.X, sc, ctx)
	case *ast.IndexExpr:
		// An explicitly instantiated generic call, `F[int](x)`.
		e.walkCallee(n.X, sc, ctx)
		e.walkExpr(n.Index, sc, ctx)
	case *ast.IndexListExpr:
		e.walkCallee(n.X, sc, ctx)
		for _, index := range n.Indices {
			e.walkExpr(index, sc, ctx)
		}
	default:
		// A conversion (`[]byte(s)`, `(*T)(p)`), an immediately-invoked
		// function literal, a call through a returned function value: not a
		// named callee, so the expression is walked for its own uses and no
		// CALLS edge is claimed.
		e.walkExpr(fun, sc, ctx)
	}
}

// useIdent is the four-way classification extract.go's doc comment
// describes, in the order the classification has to happen.
func (e *extractor) useIdent(ident *ast.Ident, sc *scope, ctx useContext, isCall bool) {
	if ident == nil {
		return
	}
	name := ident.Name
	if name == "" || name == "_" {
		return
	}

	// 1. A local. Dropped without ever asking what it is bound to.
	if sc.bound(name) {
		return
	}

	// A package name used bare rather than as a selector base. Not a symbol
	// of any container, so nothing is emitted - and emitting an own-container
	// placeholder for it would be an address no package will ever answer.
	if _, isImport := e.imports[name]; isImport {
		return
	}

	// 2. Declared in this very file: a direct, confirmed edge. Checked
	// before the universe block, because a package may legally declare a
	// name that shadows a builtin.
	if id, ok := e.fileDecls[name]; ok {
		e.emitUse(ctx, isCall, id, e.nodeKind[id] == nodeKindFunction)
		return
	}

	// 3. A builtin.
	if universeNames[name] {
		return
	}

	// A dot import makes every remaining bare name ambiguous between this
	// package and the dot-imported one, and nothing structural can tell them
	// apart - see collectImports.
	if e.hasDotImport {
		return
	}

	// 4. A sibling file of this package, as far as this tier can tell.
	e.emitUse(ctx, isCall, e.placeholderFor(e.container, name, ident), true)
}

// useSelector splits `a.b` into the one case this tier can answer exactly
// and the one it refuses to answer at all.
func (e *extractor) useSelector(sel *ast.SelectorExpr, sc *scope, ctx useContext, isCall bool) {
	if sel.Sel == nil {
		e.walkExpr(sel.X, sc, ctx)
		return
	}

	// `pkg.F()` / `pkg.T{}` / `pkg.Var`: the base is an import binding this
	// file made, and nothing local shadows it. The address is exact - the
	// container is named by the import path itself - so this is the one
	// cross-package edge a structural tier can honestly emit.
	if base, ok := unparen(sel.X).(*ast.Ident); ok && !sc.bound(base.Name) {
		if importPath, isImport := e.imports[base.Name]; isImport {
			e.emitUse(ctx, isCall, e.placeholderFor(importPath, sel.Sel.Name, sel.Sel), true)
			return
		}
	}

	// Everything else is a selection through a *value*: `x.M()`, `s.field`,
	// `pkg.New().Close()`, `c.inner.Do()`. Which type the receiver has is
	// exactly what a type checker answers and a parser cannot, and the name
	// alone is worthless - `Close` exists on many types, and picking one by
	// name is how a structural tier produces confidently wrong edges. So
	// this emits no edge at all and records an open site for GM-281's
	// go/types pass instead.
	//
	// The receiver expression itself is still walked: `logger.Printf()`
	// where `logger` is a package-level variable is a real, resolvable use
	// of `logger`, even though the `.Printf` half is not.
	e.recordOpenSite(sel, ctx, isCall)
	e.walkExpr(sel.X, sc, ctx)
}

// emitUse writes the edge a resolved use produces.
//
// A CALLS edge is only written when there is an enclosing symbol to write it
// from *and* the target can be a function. Core's linker lands CALLS only on
// a `Function` node, so a CALLS edge onto a type conversion's target would
// be an edge core drops on arrival; REFERENCES says the true thing instead.
// For a placeholder the target's kind is unknown here, and "can be a
// function" is the right claim to make: core applies the kind filter to the
// candidates it finds, and refuses rather than mislands.
func (e *extractor) emitUse(ctx useContext, isCall bool, toID string, targetMayBeFunction bool) {
	if isCall && targetMayBeFunction && ctx.callerID != "" {
		e.addEdge(ctx.callerID, edgeKindCalls, toID)
		return
	}
	e.addEdge(ctx.symbolID, edgeKindReferences, toID)
}

// placeholderFor returns the node that addresses `name` inside `container`,
// creating it on first use.
//
// One node per (container, name) per file, however many times the file uses
// it: the address is the identity, and the node id is a pure function of it,
// which is what makes the id survive an edit that moves every use site. The
// node's *range* is the first use the walk met - a placeholder is not a
// declaration and has no range of its own, so the first mention is the
// honest answer and the deterministic one.
func (e *extractor) placeholderFor(container, name string, at *ast.Ident) string {
	qualifiedName := container + "." + name
	id := nodeIDFor(e.relPath, nodeKindModule, qualifiedName, pendingSymbolNativeKind)
	if e.nodeSeen[id] {
		return id
	}
	e.placeholder[id] = true
	return e.addNode(wireNode{
		ID:            id,
		Kind:          nodeKindModule,
		Name:          name,
		QualifiedName: qualifiedName,
		FilePath:      e.relPath,
		Range:         e.rangeOf(at.Pos(), at.End()),
		Visibility:    fileVisibility(),
		Language:      languageName,
		NativeKind:    pendingSymbolNativeKind,
		Target: &placeholderTarget{
			Scope: targetScope{Container: container},
			Key:   targetKey{Name: name},
			// Who is asking, for core's visibility check: an unexported
			// symbol of another package is `container(that package)` and
			// this file's container is not it, so core refuses the link -
			// which is exactly Go's own rule.
			FromContainer: e.container,
		},
	})
}

func unparen(expr ast.Expr) ast.Expr {
	for {
		paren, ok := expr.(*ast.ParenExpr)
		if !ok {
			return expr
		}
		expr = paren.X
	}
}
