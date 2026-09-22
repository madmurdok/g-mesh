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
 * This is the shape that separates this plugin from the other three bundled
 * ones, and until GM-385 nothing in this fixture contained one - so
 * `plugin.toml`'s `receiver_calls = "unresolved"` /
 * `receiver_calls_structural = "unresolved"` was a claim no check could
 * fail. Go, Rust and Python all resolve a call like this, to the declaration
 * their receiver's *static* type names; this plugin emits no edge for it in
 * either tier, so `Greetable#greet` and `Greeter#greet` both answer
 * `find_callers` with nothing at all. `conformance/expect.toml` asserts that
 * emptiness.
 */
export function viaGreetable(g: Greetable): string {
  return g.greet();
}
