// a generic interface and the type argument a heritage clause
// passes to it. `T` is deliberately unused so `Item` is referenced only by
// that clause.
export interface Item {
  id: string;
}

export interface Container<T> {
  size(): number;
}
