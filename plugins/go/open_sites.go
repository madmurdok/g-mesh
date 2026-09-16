package main

// The two lists the structural tier hands to the semantic one (semantic.go):
// the use sites it deliberately answers with *nothing* (open sites), and the
// calls it answers with an address that may name a type rather than a
// function (placeholder calls). Neither ever reaches core.
//
// # Why a receiver call gets no edge
//
// `x.M()` is the overwhelmingly common shape of a Go method call, and the
// name `M` alone is worthless for finding the declaration: `Close` exists on
// dozens of types in any real repository, and `Do`, `Run`, `Get` and `Len`
// are worse. A structural tier that guessed - by picking the only `M` in the
// package, say - would be right often enough to look like it worked and
// wrong often enough to poison `find_callers` for exactly the symbols people
// query most. So nothing is emitted, the gap is declared in plugin.toml
// (`receiver_calls = "unresolved"`) and stated in the MCP instructions, and
// the site is kept here for the pass that can settle it.
//
// # Why these never reach core
//
// An open site is not a graph fact. It is a *question* - a position in a
// file plus what was written there - and core has no table for one; the
// design doc puts `open_sites` on the SDK's `FileGraph` with "Kept by the
// SDK in memory and handed to the semantic bridge. Never sent to core."
// This plugin keeps them the same way: `fileGraph.openSites` is filled by
// every extraction, and `pluginState` (control.go) keeps the current set for
// each file it has parsed, replacing a file's sites wholesale whenever that
// file is re-extracted, so the collection can never drift from the code it
// describes. During a one-shot `--bulk-index` they are computed and dropped
// with the process, which is correct: that process answers no semanticPass.

import "go/ast"

// openSiteKind says *why* a site is open, which is also what tells the
// semantic pass which of `go/types`' two answer tables to ask.
type openSiteKind int

const (
	// openSiteSelection is `x.M()` / `x.field` - a selection through a
	// value. `types.Info.Selections` is keyed by the whole `*ast.Selector
	// Expr` and answers it exactly, embedding promotion and interface
	// dispatch included.
	openSiteSelection openSiteKind = iota
	// openSiteBareName is a bare identifier in a file that carries a dot
	// import. The structural tier emits nothing for one (extract.go's
	// collectImports has why: the set of names the dot-imported package
	// publishes is exactly what a per-file parser cannot see, so *every*
	// bare name in such a file is ambiguous between two packages). The
	// answer is in `types.Info.Uses`, keyed by the identifier itself.
	openSiteBareName
)

// openSite is one use site this structural tier deliberately answered with
// nothing, recorded for the semantic pass.
type openSite struct {
	// FilePath is project-relative, as everything on the wire is - even
	// though this never crosses it, so a site is still identifiable once
	// several files' sites sit in one collection.
	FilePath string
	// Line and Col are 0-based, the same convention the wire uses, and point
	// at the *selected name* (`M` in `x.M()`) - or at the bare identifier
	// itself for [openSiteBareName] - rather than at the receiver: that is
	// the position `types.Info.Selections`/`Uses` are keyed by and the one
	// an LSP `textDocument/definition` would be asked at.
	Line int
	Col  int
	// Name is the selected identifier - what a resolution has to find a
	// declaration for.
	Name string
	// Kind says which table answers this site; see [openSiteKind].
	Kind openSiteKind
	// IsCall distinguishes `x.M()` from `x.M` / `x.field`: the first should
	// become a CALLS edge once resolved, the second a REFERENCES one.
	IsCall bool
	// EnclosingSymbolID is the node the resolved edge will be written from,
	// so the semantic pass does not have to re-derive the enclosing
	// declaration from the position.
	EnclosingSymbolID string
	// EnclosingCallerID is the same for a CALLS edge, and is empty where
	// there is no enclosing symbol to call from (a top-level initializer of
	// a blank-named var) - in which case the resolved edge degrades to
	// REFERENCES exactly as the structural one would have.
	EnclosingCallerID string
}

// placeholderCall is the opposite of an open site: a call this tier *did*
// answer, with an address that may turn out to name something a `CALLS` edge
// can never land on.
//
// `pkg.T(x)` and `T(x)` are conversions, not calls, and they are written
// exactly like a call - so the structural tier, which cannot tell a type
// from a function across a file boundary, emits `CALLS` onto a name-keyed
// placeholder for both. Core's kind filter then refuses to land that edge on
// a `Type` (core/src/graph/symbol_links.rs, step 3), so the edge survives as
// a permanently unresolved claim about a call that never happens. Recording
// the site lets the semantic pass retract exactly those edges - it knows
// whether the name is a func or a type - and re-state them as the
// `REFERENCES` edge a conversion actually is. A site whose name really is a
// function is left completely alone: the structural edge was right, and core
// has already linked it.
//
// Only calls onto a *placeholder* are recorded. A same-file callee is a node
// this file already emitted, so `emitUse` reads its kind directly and never
// gets this wrong in the first place.
type placeholderCall struct {
	FilePath string
	Line     int
	Col      int
	Name     string
	// EdgeID is the CALLS edge the structural tier emitted, so the semantic
	// pass can retract it by id without re-deriving anything.
	EdgeID string
	// EnclosingSymbolID is the `from` of the REFERENCES edge that replaces
	// it. (The `from` of the retracted CALLS edge was the caller id, which is
	// the same node whenever a CALLS edge was emitted at all.)
	EnclosingSymbolID string
}

func (e *extractor) recordOpenSite(sel *ast.SelectorExpr, ctx useContext, isCall bool) {
	at := e.position(sel.Sel.Pos())
	e.openSites = append(e.openSites, openSite{
		FilePath:          e.relPath,
		Line:              at.Line,
		Col:               at.Col,
		Name:              sel.Sel.Name,
		Kind:              openSiteSelection,
		IsCall:            isCall,
		EnclosingSymbolID: ctx.symbolID,
		EnclosingCallerID: ctx.callerID,
	})
}

// recordBareNameSite records a bare identifier a dot import made ambiguous -
// see [openSiteBareName] and extract.go's collectImports.
func (e *extractor) recordBareNameSite(ident *ast.Ident, ctx useContext, isCall bool) {
	at := e.position(ident.Pos())
	e.openSites = append(e.openSites, openSite{
		FilePath:          e.relPath,
		Line:              at.Line,
		Col:               at.Col,
		Name:              ident.Name,
		Kind:              openSiteBareName,
		IsCall:            isCall,
		EnclosingSymbolID: ctx.symbolID,
		EnclosingCallerID: ctx.callerID,
	})
}

func (e *extractor) recordPlaceholderCall(at *ast.Ident, ctx useContext, edgeID string) {
	if at == nil || edgeID == "" {
		return
	}
	pos := e.position(at.Pos())
	e.placeholderCalls = append(e.placeholderCalls, placeholderCall{
		FilePath:          e.relPath,
		Line:              pos.Line,
		Col:               pos.Col,
		Name:              at.Name,
		EdgeID:            edgeID,
		EnclosingSymbolID: ctx.symbolID,
	})
}
