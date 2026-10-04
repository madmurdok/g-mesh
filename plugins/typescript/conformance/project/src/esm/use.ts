// a `.js` specifier naming a `.ts` file, and a directory import.
import { fromLib } from "./lib.js";
import { fromDir } from "./dir";

export function viaJsSpecifier(): number {
  return fromLib();
}

export function viaDirIndex(): number {
  return fromDir();
}
