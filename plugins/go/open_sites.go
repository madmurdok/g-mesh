package main

// Open sites: the use sites this structural tier deliberately answers with
// nothing, recorded so GM-281's go/types pass can answer them exactly.
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

// openSite is one unresolved selection.
type openSite struct {
	// FilePath is project-relative, as everything on the wire is - even
	// though this never crosses it, so a site is still identifiable once
	// several files' sites sit in one collection.
	FilePath string
	// Line and Col are 0-based, the same convention the wire uses, and point
	// at the *selected name* (`M` in `x.M()`) rather than at the receiver:
	// that is the position `types.Info.Selections` is keyed by and the one
	// an LSP `textDocument/definition` would be asked at.
	Line int
	Col  int
	// Name is the selected identifier - what a resolution has to find a
	// declaration for.
	Name string
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

func (e *extractor) recordOpenSite(sel *ast.SelectorExpr, ctx useContext, isCall bool) {
	at := e.position(sel.Sel.Pos())
	e.openSites = append(e.openSites, openSite{
		FilePath:          e.relPath,
		Line:              at.Line,
		Col:               at.Col,
		Name:              sel.Sel.Name,
		IsCall:            isCall,
		EnclosingSymbolID: ctx.symbolID,
		EnclosingCallerID: ctx.callerID,
	})
}
