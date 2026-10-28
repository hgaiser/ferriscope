#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if ! command -v glslc &>/dev/null; then
    echo "Error: glslc not found. Install the Vulkan SDK or glslc package." >&2
    exit 1
fi

for name in sdr_on_hdr; do
    echo "Compiling $name.comp → $name.spv"
    glslc -x glsl --target-env=vulkan1.3 -O "$SCRIPT_DIR/$name.comp" -o "$SCRIPT_DIR/$name.spv"
    FILE_SIZE=$(stat -c%s "$SCRIPT_DIR/$name.spv")
    echo "Written $FILE_SIZE bytes to $SCRIPT_DIR/$name.spv"
done
