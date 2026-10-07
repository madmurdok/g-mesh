/**
 * A thing that can produce a greeting - implemented by `Greeter` below, for
 * `find_implementations`'s (and `find_references`'s own `SUPERTYPE_OF` row)
 * conformance case in expect.toml.
 */
export interface Greetable {
  greet(): string;
}

export class Greeter implements Greetable {
  greet(): string {
    return "hi";
  }
}

/**
 * A receiver call (`g.greet()`) on a parameter annotated with the interface.
 *
 * The structural tier has no type for `g` and leaves the call open; the
 * semantic tier (vtsls) lands it on `Greetable#greet`, the declaration the
 * receiver's static type names - as Go, Rust and Python do for theirs.
 * `conformance/expect.toml`'s `[[callers]] Greetable#greet` asserts it.
 */
export function viaGreetable(g: Greetable): string {
  return g.greet();
}
