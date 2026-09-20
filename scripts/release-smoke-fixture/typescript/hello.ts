/**
 * scripts/release-smoke.sh's TypeScript input - see
 * scripts/release-smoke-fixture/README.md for why this exists and why it is
 * shaped this way. Mirrors
 * plugins/typescript/conformance/project/src/math.ts exactly: two functions
 * in one file, the second calling the first.
 */
export function add(a: number, b: number): number {
  return a + b;
}

export function double(n: number): number {
  return add(n, n);
}
