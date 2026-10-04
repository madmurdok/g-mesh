// a const arrow, extracted as a function, reached three ways from
// packages/app (bare package root, package subpath, tsconfig `paths` alias).
export const pointOf = (x: number, y: number) => ({ x, y });
