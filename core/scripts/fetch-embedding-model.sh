#!/usr/bin/env bash
#
# Downloads the weights g-mesh's embedding model needs (see
# core/src/embedding/model.rs). This is the one network step in the embedding
# path and it is deliberately explicit: nothing in the daemon downloads
# anything by itself.
#
# `g-mesh model fetch` (core/src/cli/model.rs) is the supported way to do this
# and the only one an installed binary has - it downloads the same two files,
# from the same pinned revision, into the same directory, and additionally
# verifies them against pinned SHA-256 digests, which this script does not.
#
# This script is kept anyway, for the one case the binary cannot serve: a fresh
# checkout with nothing built yet, where fetching the weights first lets
# `cargo test embedding -- --ignored` run on the same pass as the build. It is
# a contributor convenience, not a second source of truth - a unit test in
# cli::model fails the build if the revision, the remote paths or the file
# names below ever drift from the binary's.
#
# Usage:
#   core/scripts/fetch-embedding-model.sh [target-dir]
#
# Default target dir is ~/.g-mesh/models/jina-embeddings-v2-base-code, which is
# where embedding::model::default_model_dir looks. Existing files are left
# alone, so re-running after an interrupted download is cheap for the file that
# already finished but does NOT resume a partial one - delete it and re-run.
#
# The revision below is pinned: these are the exact weights the embedding tests
# were verified against. Bumping it changes every vector the model produces and
# therefore invalidates any index built with the old one (see
# REQUIREMENTS.md, "Инвалидация эмбеддингов при смене embedding-модели").

set -euo pipefail

MODEL_NAME="jina-embeddings-v2-base-code"
REPO="jinaai/${MODEL_NAME}"
REVISION="516f4baf13dec4ddddda8631e019b5737c8bc250"

# Same resolution order as embedding::model::default_model_dir, so the script
# and the loader can never disagree about where the weights belong: an explicit
# argument, else $G_MESH_MODEL_DIR, else the per-user model directory.
TARGET_DIR="${1:-${G_MESH_MODEL_DIR:-${HOME}/.g-mesh/models/${MODEL_NAME}}}"

# Where the weights come from, tried in order - the same rule and the same
# variable as `g-mesh model fetch` (core/src/cli/model.rs). An override is
# tried first and upstream still follows it, so an unreachable mirror costs a
# slower download rather than a failed one.
#
# The two are addressed differently on purpose. Upstream is addressed the way
# Hugging Face addresses itself; an override serves the two files side by side
# under one prefix, which is the shape of both a corporate mirror and a GitHub
# release's assets.
UPSTREAM_URL="https://huggingface.co/${REPO}/resolve/${REVISION}"

mkdir -p "${TARGET_DIR}"

fetch() {
  local remote_path="$1"
  local local_name="$2"
  local dest="${TARGET_DIR}/${local_name}"

  if [ -f "${dest}" ]; then
    echo "already present: ${dest}"
    return
  fi

  local -a urls=()
  if [ -n "${G_MESH_MODEL_BASE_URL:-}" ]; then
    urls+=("${G_MESH_MODEL_BASE_URL%/}/${local_name}")
  fi
  urls+=("${UPSTREAM_URL}/${remote_path}")

  local url
  for url in "${urls[@]}"; do
    echo "downloading ${local_name} from ${url} ..."
    if curl --fail --location --progress-bar --output "${dest}.partial" "${url}"; then
      mv "${dest}.partial" "${dest}"
      return
    fi
    # Every attempt is reported rather than only the last, so "the download
    # failed" never hides which sources were tried.
    echo "  ${url} failed" >&2
    rm -f "${dest}.partial"
  done

  echo "could not download ${local_name} from any source" >&2
  exit 1
}

# The fp32 export, not model_fp16/model_quantized: those trade accuracy for
# size, and the vectors they produce differ from the ones the tests pin.
fetch "onnx/model.onnx" "model.onnx"
fetch "tokenizer.json" "tokenizer.json"

echo
echo "model ready in ${TARGET_DIR}"
echo "(no checksum was verified - 'g-mesh model fetch' does that)"
echo "run the full embedding tests with:"
echo "  cd core && cargo test embedding -- --ignored --test-threads=1"
