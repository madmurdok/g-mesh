package app_test

import (
	"testing"

	"github.com/example/app"
)

// TestPlaceholder lives in the *external* test package, whose container is
// `github.com/example/app_test` - a different container from the package it
// tests. So `app.Placeholder` here is an ordinary cross-package reference
// through an import, exactly like cmd/main.go's, and it links only because
// `Placeholder` is exported.
func TestPlaceholder(t *testing.T) {
	if app.Placeholder() != "app" {
		t.Fatal("unexpected greeting")
	}
}
