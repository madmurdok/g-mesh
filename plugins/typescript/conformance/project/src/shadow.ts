// `outer`'s parameter shadows the module function `helper`.
export function helper(): void {}

export function inner(): void {
  helper();
}

export function outer(helper: () => void): void {
  helper();
}
