// a `paths` alias inherited through `extends` from
// ../tsconfig.base.json, whose target is relative to that config (no baseUrl).
import { pointOf } from "~geom/point";

export function viaAlias() {
  return pointOf(5, 6);
}
