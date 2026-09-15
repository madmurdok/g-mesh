import { add } from "./math";
import { twice } from "./index";
import * as m from "./math";

export function run(): number {
  return add(1, 2) + twice(3) + m.double(4);
}
