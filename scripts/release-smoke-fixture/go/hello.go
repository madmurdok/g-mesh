// Package fixture is scripts/release-smoke.sh's Go input - see
// scripts/release-smoke-fixture/README.md for why this exists and why it is
// shaped this way.
package fixture

// add and double mirror plugins/typescript/conformance/project/src/math.ts:
// two functions in one file, the second calling the first, so a real
// `g-mesh reindex` produces both a node and a same-file CALLS edge for this
// language without needing cross-file or cross-package resolution.
func add(a, b int) int {
	return a + b
}

func double(n int) int {
	return add(n, n)
}
