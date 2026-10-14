import type { Box, Opts } from "./box";
export function boot(box: Box): number {
  return box.cb();
}
export function start(opts: Opts): number {
  return opts.go();
}
