package server

import "testing"

// TestNothing exists only so `packages.Load(..., Tests: true)` type-checks
// this package's `[server.test]` variant separately from its production
// build - an *internal* test file (same package, not `server_test`) is what
// makes the loader do that at all. It deliberately calls nothing this
// fixture already tracks (no Server, Conn, Logged or Closer reference), so
// it cannot perturb any other expectation in this file - it exists only to
// give the `server` package a second, separately type-checked variant.
//
// GM-362: without that second variant, `Closer` (conn.go) never gets a
// second, distinct `*types.TypeName` object for the same declaration, and
// the semantic pass's implicit-implementation defect this file's
// `[[implementations]]` entry now also pins - an interface coming back as
// its own implementor - has nothing to reproduce against. With it, the
// pre-fix pass adds `Closer` to its own implementors list, which the
// existing entry's exact `expect` set already catches as an extra row.
func TestNothing(t *testing.T) {}
