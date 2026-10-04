// calls inside a const arrow, a callback, a class-field arrow, and
// at module top level.
export function leaf(): number {
  return 1;
}

export const arrowCaller = () => leaf();

export function viaCallback(xs: number[]): number[] {
  return xs.map(() => leaf());
}

export class Holder {
  fire = () => leaf();
}

leaf();
