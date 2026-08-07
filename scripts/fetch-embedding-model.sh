#!/usr/bin/env bash
#
# Refresh the vendored embedding model under vendor/hf-cache/.
#
# CIH embeds code with the all-MiniLM-L6-v2 ONNX model via `fastembed`, which
# normally downloads it from huggingface.co on first use. We vendor the model in
# this repo so hosts whose network blocks huggingface.co work offline (the model
# is committed to git, baked into the Docker image, and bind-mounted in compose).
# See the "Offline / air-gapped use" section of README.md.
#
# Run this ONLY on a machine that CAN reach huggingface.co (e.g. to pick up a new
# model revision), then commit the updated vendor/hf-cache/ directory.
#
# Requires: bash, curl. No CIH build and no database needed.
#
# Env overrides:
#   MODEL_REPO   HuggingFace repo (default: Qdrant/all-MiniLM-L6-v2-onnx)
#   MODEL_REV    revision/branch  (default: main)
#   HF_ENDPOINT  mirror endpoint  (default: https://huggingface.co)
set -euo pipefail

REPO="${MODEL_REPO:-Qdrant/all-MiniLM-L6-v2-onnx}"
REV="${MODEL_REV:-main}"
ENDPOINT="${HF_ENDPOINT:-https://huggingface.co}"

# Exact files fastembed requests for this model: the ONNX weights plus the
# tokenizer/config set (see crates/cih-embed + fastembed's model table).
FILES=(model.onnx tokenizer.json tokenizer_config.json special_tokens_map.json config.json)

# hf-hub cache dir name: "models--<org>--<name>"
CACHE_NAME="models--${REPO//\//--}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/vendor/hf-cache/$CACHE_NAME"

echo "Refreshing $REPO@$REV from $ENDPOINT"

# Resolve the commit sha for REV the same way hf-hub names the snapshot dir.
# A GET on a small (non-LFS) file returns the X-Repo-Commit response header.
SHA="$(curl -fsSL -o /dev/null -D - "$ENDPOINT/$REPO/resolve/$REV/config.json" \
        | tr -d '\r' | awk -F': ' 'tolower($1)=="x-repo-commit"{print $2}' | tail -n1)"
if [ -z "${SHA:-}" ]; then
  echo "ERROR: could not resolve commit sha for $REPO@$REV" >&2
  exit 1
fi
echo "Resolved commit: $SHA"

SNAP="$DEST/snapshots/$SHA"
mkdir -p "$SNAP" "$DEST/refs"
printf '%s' "$SHA" > "$DEST/refs/$REV"

for f in "${FILES[@]}"; do
  echo "  → $f"
  curl -fSL "$ENDPOINT/$REPO/resolve/$REV/$f" -o "$SNAP/$f"
done

echo
echo "Vendored into: $DEST"
echo "Layout:"
echo "  refs/$REV -> $SHA"
echo "  snapshots/$SHA/{${FILES[*]// /, }}"
echo
echo "Next: git add vendor/hf-cache && git commit -m 'chore: refresh vendored embedding model'"
