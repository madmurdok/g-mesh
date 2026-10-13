function make(): () => number {
  return () => 1;
}
export const handler = make();
class Router {
  handler(): number {
    return 2;
  }
}
