#!/usr/bin/env bash
# Download a ggml Whisper model into yapper's data directory.
#   ./scripts/fetch-model.sh [model]   (default: base.en)
# Sizes: tiny.en 75M | base.en 148M | small.en 488M | medium.en 1.5G | large-v3 3.1G
set -euo pipefail

model="${1:-base.en}"
dest="${XDG_DATA_HOME:-$HOME/.local/share}/yapper/models"
file="ggml-${model}.bin"
url="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/${file}"

mkdir -p "$dest"
if [[ -f "$dest/$file" ]]; then
  echo "already present: $dest/$file"
  exit 0
fi

echo "downloading $file -> $dest"
curl -fL --progress-bar -o "$dest/$file.part" "$url"
mv "$dest/$file.part" "$dest/$file"
echo "done: $dest/$file"
