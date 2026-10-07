// a module function and a same-named instance method, plus a
// static member called through its class.
export function pick(): number {
  return 1;
}

export class Store {
  // The bare `pick()` here is the module function, not this method.
  pick(): number {
    return pick();
  }

  static drop(): void {}
}

export function clear(): void {
  Store.drop();
}
