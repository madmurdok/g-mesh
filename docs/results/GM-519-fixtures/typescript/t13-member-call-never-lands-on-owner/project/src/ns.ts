import type * as B from "./box";
export function open(bag: B.Bag): number {
  return bag.cb();
}
export function load(cfg: B.Cfg): number {
  return cfg.go();
}
