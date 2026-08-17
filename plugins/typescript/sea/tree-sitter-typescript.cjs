// esbuild `--alias:tree-sitter-typescript` target for the single-executable build.
// See native-require.cjs for why these three packages are loaded from disk
// instead of being bundled.
module.exports = require("./native-require.cjs")("tree-sitter-typescript");
