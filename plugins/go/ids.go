package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
)

// hashID mirrors plugins/typescript/src/extract.ts's `hash`: sha256 of the
// UTF-8 input, hex-encoded, truncated to the first 32 hex characters (the
// first 16 bytes of the 32-byte digest). See ids_test.go for expected
// values computed by actually running that TS code (`node -e '...'`, using
// the real Node crypto module) against a handful of tuples, not recalled
// from memory - this task's own report has the transcript.
func hashID(input string) string {
	sum := sha256.Sum256([]byte(input))
	return hex.EncodeToString(sum[:])[:32]
}

// nodeIDFor mirrors extract.ts's nodeIdFor field-for-field: the same
// four-part space-joined string, in the same order. Ids are
// content-position-independent (path, kind, qualifiedName, nativeKind
// only, never a source range) so they survive an edit elsewhere in the
// file - the property this plugin's own id stability (control.go's
// handleFileChanged) and the conformance kit's id-stability.* checks both
// depend on. `nativeKind` participates even when empty, the same way
// extract.ts's `nativeKind ?? ""` does, so an absent nativeKind is a real
// empty-string field in the hashed string, not an omitted one - callers
// pass "" for it today (this scaffold's only node kind, File, has no
// nativeKind), and GM-280 will pass the real thing once nodes have one.
func nodeIDFor(filePath, kind, qualifiedName, nativeKind string) string {
	return hashID(fmt.Sprintf("node %s %s %s %s", filePath, kind, qualifiedName, nativeKind))
}

// edgeIDFor mirrors extract.ts's edgeIdFor: identity is the (from, kind,
// to) triple, plus - only when set - the declaration ordinal a semantic
// pass bound the edge to. Go has no overload sets, so nothing this plugin
// emits ever binds one and every caller here passes nil; the parameter
// exists because the id scheme is shared with the TS plugin and must stay
// field-for-field identical to it. Unlike nodeIDFor's nativeKind, an absent
// toDeclaration contributes *nothing* to the hashed string, not an empty
// field - see extract.ts's own doc comment on edgeIdFor for why: every edge
// a structural pass emits (which never binds a declaration ordinal) must
// keep exactly the id it would have without this parameter existing at all.

func edgeIDFor(fromID, kind, toID string, toDeclaration *int) string {
	binding := ""
	if toDeclaration != nil {
		binding = fmt.Sprintf(" %d", *toDeclaration)
	}
	return hashID(fmt.Sprintf("edge %s %s %s%s", fromID, kind, toID, binding))
}
