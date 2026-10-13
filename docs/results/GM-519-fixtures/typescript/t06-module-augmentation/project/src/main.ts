import { Widget } from "./base";
import "./plugin";
export function show(w: Widget): string {
  return w.glow() + w.draw();
}
