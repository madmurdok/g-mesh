import { Widget } from "./base";
declare module "./base" {
  interface Widget {
    glow(): string;
  }
}
Widget.prototype.glow = function (): string {
  return "glow";
};
