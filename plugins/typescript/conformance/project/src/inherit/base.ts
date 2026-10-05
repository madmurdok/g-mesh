/**
 * A base class whose method is reached only from another file, through
 * `this` and `super` - `[[callers]] Base#hello` in expect.toml. Neither call
 * names a member the calling file declares, so the structural tier leaves
 * both to the semantic tier.
 */
export class Base {
  hello(): string {
    return "hello";
  }
}
