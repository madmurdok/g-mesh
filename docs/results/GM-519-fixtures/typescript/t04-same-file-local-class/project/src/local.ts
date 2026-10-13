class Counter {
  bump(): number {
    return 1;
  }
}
export function tally(): number {
  const c = new Counter();
  return c.bump();
}
