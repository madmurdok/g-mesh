package app

import "strings"

// helper is unexported, so its visibility is this package's container and
// nothing outside `github.com/example/app` can link to it. The server
// package declares a function with exactly this name (server/conn.go), which
// is what makes the caller expectations in expect.toml a real test: a
// file-scoped or project-wide address would confuse the two.
func helper() string {
	return strings.TrimSpace(" app ")
}

func init() {
	// An `init` is a real declaration with a real body. Without a node for
	// it, this call would have no caller to hang off and would degrade to a
	// reference from the file.
	_ = helper()
}
