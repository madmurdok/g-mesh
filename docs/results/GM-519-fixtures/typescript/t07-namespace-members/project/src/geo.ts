export namespace Geo {
  export function area(r: number): number {
    return r * r;
  }
  export class Circle {
    radius(): number {
      return 1;
    }
  }
}
