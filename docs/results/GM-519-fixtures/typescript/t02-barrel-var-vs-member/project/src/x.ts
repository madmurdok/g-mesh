function make(): () => number {
  return () => 1;
}
export const handler = make();
export class Router {
  handler(): number {
    return 2;
  }
}
