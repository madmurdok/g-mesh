// a bodiless overload set, and a function inside a dotted
// namespace name.
export declare function ambient(x: string): string;
export declare function ambient(x: number): number;

export namespace Outer.Inner {
  export function deepFn(): number {
    return 1;
  }
}
