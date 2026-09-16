package app

// A tiny fixture for `g-mesh plugins check plugins/go --fixture
// plugins/go/conformance/project`. Deliberately small: every file in it
// exists to exercise one shape of the Go plugin's contract, and the kit
// rewrites whichever file has the most nodes on its way through, so a bigger
// fixture buys nothing but a slower run.

// Placeholder is this package's exported entry point, and the one symbol the
// external test package reaches through `app.Placeholder`.
func Placeholder() string {
	// A call into a *sibling file of the same package*, which is the whole
	// point of a container-scoped placeholder: nothing in this file declares
	// `helper`, so the plugin addresses it at this package's container and
	// core links it against helper.go's declaration.
	return helper()
}
