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
