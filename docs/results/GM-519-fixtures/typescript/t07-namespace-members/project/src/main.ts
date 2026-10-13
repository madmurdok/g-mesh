import { Geo } from "./geo";
export function measure(): number {
  const c = new Geo.Circle();
  return Geo.area(c.radius());
}
