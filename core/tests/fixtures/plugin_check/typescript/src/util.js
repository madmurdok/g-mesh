// GM-275: a plain .js file in the fixture, so `g-mesh plugins check` exercises
// this plugin's ownership.language conformance rule against an extension the
// manifest claims but whose grammar isn't TypeScript's own - see extract.ts's
// `WIRE_LANGUAGE` doc comment for why this file's nodes still report
// `language: "typescript"`, matching the manifest's identity, not "javascript".
export function shout(word) {
  return word.toUpperCase() + "!";
}
