import { Registry } from "./reg";
export function make(): Registry {
  return Registry.create();
}
