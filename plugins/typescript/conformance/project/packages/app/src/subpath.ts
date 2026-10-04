// a package subpath whose `exports` target does not exist, so it
// falls back into the package's `src`.
import { pointOf } from "@fx/geom/point";

export function viaSubpath() {
  return pointOf(3, 4);
}
