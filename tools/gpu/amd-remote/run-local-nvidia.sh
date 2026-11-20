#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# CratonVM (CUDA, built from this tree) vs TornadoVM on a LOCAL NVIDIA card. No HotSpot.
#
#   TVHOME=<TornadoVM SDK dir> bash tools/gpu/amd-remote/run-local-nvidia.sh
#
# Build CratonVM first (no-LTO is enough for GPU rows and avoids fat-LTO's memory use):
#
#   CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_DEBUG=0 \
#   CARGO_TARGET_DIR=target-2060 cargo build --release -j 5 -p cratonvm-cli --bin cratonvm --features gpu-driver
#
# HotSpot is not run: the longest chains take it minutes per call. Correctness is checked against the
# HotSpot checksums recorded on the MI300X (Java arithmetic is deterministic, so they hold here).
# Run it once per TornadoVM SDK: the OpenCL SDK is the like-for-like comparison with the AMD run; on
# NVIDIA the PTX SDK is TornadoVM's own best backend and is the fairer opponent.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
ROOT="$(cd "$HERE/../../.." && { pwd -W 2>/dev/null || pwd; })"
: "${TVHOME:?set TVHOME to a TornadoVM SDK directory}"
export TVHOME
export CV="${CV:-$ROOT/target-2060/release/cratonvm.exe}"
export JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
export GPU_JAR="${GPU_JAR:-$ROOT/../../../apps/GPULlama3.java/craton-gpu.jar}"
export HS_RUN=0 REF_SUMS="${REF_SUMS:-$ROOT/tools/bench/bench-gpu/results/amd-mi300x-20261109/reference-checksums.txt}"
# the nine rows of the MI300X table, minus the 1024 chain (it has no HotSpot reference)
export SWEEP=1 SWEEP_INT="${SWEEP_INT:-192 768}" SWEEP_DBL="${SWEEP_DBL:-256}"
export TV_DESC="${TV_DESC:-$(basename "$TVHOME")}"
# python3 for the generators and for TornadoVM's launcher (Windows has only `python`)
[ -d /c/craton/tornadovm/py3shim ] && export PATH="/c/craton/tornadovm/py3shim:$PATH"
exec bash "$HERE/bench-rows.sh" "$@"
