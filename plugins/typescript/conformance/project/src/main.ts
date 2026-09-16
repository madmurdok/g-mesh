import { add } from "./math";
import { add as addViaBarrel } from "./index";
import { twice } from "./index";
import * as m from "./math";
import { shout } from "./util.js";
import { format } from "./overload";

// same-file call (math.ts's `double` calling `add`), cross-file named import
// call (`add`), and a call through a named barrel re-export (`twice`, from
// index.ts's `export { double as twice } from "./math"`) - see expect.toml's
// `[[callers]] symbol = "add"` and `symbol = "double"` entries.
export function run(): number {
  return add(1, 2) + twice(3) + shout("hi").length;
}

// A call through the *other* barrel re-export form, index.ts's
// `export * from "./math"` - `addViaBarrel` is `add` re-exported under a
// different local name, so this resolves to the very same node `run()`'s
// `add(1, 2)` does.
export function useBarrelStar(): number {
  return addViaBarrel(5, 6);
}

// Calls `double` only through the namespace import `m` - the one call shape
// the structural pass cannot see at all: no bare name appears at the use
// site, only the receiver `m` and a property access
// (`core/tests/namespace_import_resolution.rs`'s own module doc has why).
// Kept in a function of its own, never mixed into `run()`'s callers, so
// disabling the semantic pass removes exactly this caller and nothing else -
// see expect.toml's discrimination note on `[[callers]] symbol = "double"`.
export function useNamespaceImport(): number {
  return m.double(4);
}

// Calls the overloaded `format` under both of its call shapes - both must
// resolve to the single merged declaration in overload.ts.
export function useOverloads(): string {
  return format("a") + format(1);
}
