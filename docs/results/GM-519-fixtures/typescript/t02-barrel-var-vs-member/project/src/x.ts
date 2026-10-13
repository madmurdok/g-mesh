function make(): () => number {
  return () => 1;
}
class Local {
  a(): number {
    return 1;
  }
  b(): number {
    return 2;
  }
}
export const handler = make();
