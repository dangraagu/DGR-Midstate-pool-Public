#!/usr/bin/env bash
# Regenerate the committed CUDA PTX (src/cuda/midstate.ptx) from midstate.cu.
#
# nvcc emits a PTX `.version` matching the toolkit (e.g. CUDA 13.3 → 9.3), which
# OLDER rig drivers reject (CUDA_ERROR_UNSUPPORTED_PTX_VERSION). The kernel uses
# only long-stable PTX ops, so we PIN `.version` down for the widest driver
# compatibility. v0.1.9: pinned 7.8 → 6.5 (CUDA 10.2 ISA) after a rented fleet
# went dark — 6.5 drops the driver floor from ~r520 to ~r440, covering the
# older-driver vast/runpod GPU containers. `.target sm_75` (needs ~r418+) is now
# the arch floor; the `.version` line is no longer the binding constraint.
# Validate after regen: the bit-exact golden vectors on real hardware
# (`cargo test --features cuda -- --ignored`) — the driver JIT-load is the proof.
# Run from the repo root.
#
#   bash src/cuda/build_ptx.sh
#
# Requires nvcc on PATH (and, on Windows, cl.exe — run from a VS dev shell or with
# the MSVC Hostx64\x64 bin on PATH).
set -euo pipefail

CU="src/cuda/midstate.cu"
PTX="src/cuda/midstate.ptx"
PTX_VERSION_PIN="6.5"   # CUDA 10.2 ISA — JIT-loads on drivers back to ~r440

nvcc -ptx -arch=compute_75 -o "$PTX" "$CU"

# Pin the .version directive down for forward driver-compat.
sed -i -E "s/^\.version [0-9]+\.[0-9]+/.version ${PTX_VERSION_PIN}/" "$PTX"

echo "wrote $PTX ($(wc -c < "$PTX") bytes), .version pinned to ${PTX_VERSION_PIN}:"
grep -E "^\.version|^\.target" "$PTX"
