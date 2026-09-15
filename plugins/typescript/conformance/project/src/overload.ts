/**
 * Formats a value for display - two overload signatures over one
 * implementation. TypeScript collapses an overloaded function's signatures
 * onto a single declaration (`docs/architecture/multi-language-plugins.md`'s
 * "Symbols declared in several files" table: "TS's merged declarations are
 * same-file already"), so `find_callers`/`find_definition` must resolve
 * *both* call shapes below to this one node under the bare name `format` -
 * see expect.toml's `[[callers]]`/`[[definition]]` entries for this symbol.
 */
export function format(value: string): string;
export function format(value: number): string;
export function format(value: string | number): string {
  return String(value);
}
