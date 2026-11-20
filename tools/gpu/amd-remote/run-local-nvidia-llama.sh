#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# GPULlama3 decode on a LOCAL NVIDIA card: CratonVM (CUDA) vs TornadoVM, no HotSpot.
#
#   TVHOME=<TornadoVM SDK dir> TV_FLAG=--ptx    TV_JDK=<the SDK's JDK> TV_DESC="4.0.1 PTX" \
#     bash tools/gpu/amd-remote/run-local-nvidia-llama.sh
#   TVHOME=<5.2.0 OpenCL SDK>  TV_FLAG=--opencl                       TV_DESC="5.2.0 OpenCL" \
#     bash tools/gpu/amd-remote/run-local-nvidia-llama.sh
#
# Only the 1B F16 model: the 8B F16 is 16 GB and does not fit a 12 GB card.
# CUDA supports graph capture, so unlike the HIP run the app keeps its default (graph ON).
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
ROOT="$(cd "$HERE/../../.." && { pwd -W 2>/dev/null || pwd; })"
: "${TVHOME:?set TVHOME to a TornadoVM SDK directory}"
export TVHOME TV_FLAG="${TV_FLAG:---ptx}" TV_JDK="${TV_JDK:-C:/craton/tornadovm/jdk-25.0.3}"
export APP="${APP:-$ROOT/../../../apps/GPULlama3.java}"
export CV="${CV:-$ROOT/target-2060/release/cratonvm.exe}"
export JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
export GPU_JAR="${GPU_JAR:-$APP/craton-gpu.jar}"
export MODELS="${MODELS:-C:/craton/models/Llama-3.2-1B-Instruct-F16.gguf}"
export ARMS="${ARMS:-tv cv}"
export XMX="${XMX:-16g}" TV_GPU_MEM="${TV_GPU_MEM:-6GB}" TV_HEAP_MIN="${TV_HEAP_MIN:-4g}"
export CRATON_EXTRA="${CRATON_EXTRA:- }" CV_DESC="CUDA" TV_DESC="${TV_DESC:-$(basename "$TVHOME")}"
[ -d /c/craton/tornadovm/py3shim ] && export PATH="/c/craton/tornadovm/py3shim:$PATH"
exec bash "$HERE/bench-llama.sh" "$@"
