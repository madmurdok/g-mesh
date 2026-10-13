function wrap(f: () => number): () => number {
  return f;
}
export class Box {
  cb = wrap(() => 1);
}
export interface Opts {
  go: () => number;
}
