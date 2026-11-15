package main

import (
	"crypto/sha256"
	"encoding/hex"
	"strconv"
)

// idFieldSeparator is the byte between hashed fields, as in
// plugins/sdk/src/ids.rs: a NUL, which no path or name can contain.
const idFieldSeparator = "\x00"

// hashID is the digest half of the id scheme in plugins/sdk/src/ids.rs:
// sha256 over the tag and the fields, each field preceded by a NUL byte,
// hex-encoded and truncated to the first 32 hex characters (the first 16
// bytes of the digest).
func hashID(tag string, fields ...string) string {
	h := sha256.New()
	h.Write([]byte(tag))
	for _, field := range fields {
		h.Write([]byte(idFieldSeparator))
		h.Write([]byte(field))
	}
	return hex.EncodeToString(h.Sum(nil))[:32]
}

// nodeIDFor mirrors `node_id` in plugins/sdk/src/ids.rs field for field:
// tag "node", then path, kind, qualifiedName and nativeKind. Ids are
// independent of source position (path, kind, qualifiedName, nativeKind
// only, never a range), so they survive an edit elsewhere in the file - the
// property control.go's handleFileChanged and the conformance kit's
// id-stability.* checks depend on. nativeKind participates even when empty:
// an absent nativeKind is an empty field, not an omitted one. Callers pass
// "" for it today.
func nodeIDFor(filePath, kind, qualifiedName, nativeKind string) string {
	return hashID("node", filePath, kind, qualifiedName, nativeKind)
}

// edgeIDFor mirrors `edge_id` in plugins/sdk/src/ids.rs: identity is the
// (from, kind, to) triple, plus - only when set - the declaration ordinal a
// semantic pass bound the edge to. Go has no overload sets, so every caller
// here passes nil. Unlike nodeIDFor's nativeKind, an absent toDeclaration
// contributes no field and no separator, so an edge a structural pass emits
// keeps the id it would have without the parameter.
func edgeIDFor(fromID, kind, toID string, toDeclaration *int) string {
	if toDeclaration == nil {
		return hashID("edge", fromID, kind, toID)
	}
	return hashID("edge", fromID, kind, toID, strconv.Itoa(*toDeclaration))
}
