import { Formatter, format } from "./fmt";
export function both(f: Formatter): string {
  return f.format(1) + format("a");
}
