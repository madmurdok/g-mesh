// Package tools is the second module named by this fixture's go.work.
package tools

// Generate calls into a sibling file of its own package, the same shape
// main.go does - but in a package whose container key comes from
// tools/go.mod (`github.com/example/tools`) rather than from the root
// module, which would have made it `github.com/example/app/tools`.
func Generate() string {
	return local()
}
