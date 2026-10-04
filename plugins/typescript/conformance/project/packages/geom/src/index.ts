// the package's source entry. `package.json`'s `main` and
// `exports` both name `./dist/prod/index.js`, which is never built here - the
// excalidraw shape - so an import of `@fx/geom` must still land on this file.
export * from "./point";
