// one dynamic import whose template folds to a constant path, and
// one that cannot be folded.
const DIR = "./plugins";

export async function loadAlpha(): Promise<unknown> {
  return import(`${DIR}/alpha`);
}

export async function loadLocale(code: string): Promise<unknown> {
  return import(`./locales/${code}.json`);
}
