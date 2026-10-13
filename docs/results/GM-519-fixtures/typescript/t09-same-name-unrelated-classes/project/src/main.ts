import { FileHandle } from "./files";
export function shut(f: FileHandle): void {
  f.close();
}
