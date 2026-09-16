package app

// A tiny fixture for `g-mesh plugins check plugins/go --fixture
// plugins/go/conformance/project` (GM-279). This scaffold's extractor
// emits File nodes only - no symbols, no edges - so this fixture exists to
// exercise the shape/stream-order/id-stability/ownership checks over a
// small multi-file, multi-directory tree, not to demonstrate real Go
// cross-file linking (that needs GM-280's real declarations and edges, and
// its own `expect.toml` - see this directory's own absence of one, and
// GM-279's report, for why).

func Placeholder() string {
	return "app"
}
