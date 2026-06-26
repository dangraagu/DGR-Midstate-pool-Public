#!/usr/bin/env bash
set -euo pipefail

# --- What this is -------------------------------------------------------
# Multi-GPU Midstate miner launcher: runs ONE midstate-miner process PER GPU,
# each pinned to its own card with --gpu-id, mining to YOUR payout address.
# Not silent or hidden; does not install or run itself on anyone else's
# machine. Standard pool miner for the public Midstate proof-of-work chain.
#
# Why one process per GPU (not one process driving N GPUs):
#   * Isolation — a driver hiccup or TDR on one card restarts only that card.
#   * Simplicity — each process owns a single wgpu device + its own nonce
#     window, no cross-GPU scheduling.
#   * The FIRST process additionally mines on the CPU (--mode hybrid) so spare
#     cores aren't idle; the rest are GPU-only (--mode gpu).
#
# The miner kernel is bit-exact and CPU-re-verified: every GPU-surfaced nonce
# is recomputed on the CPU before it becomes a share, and each process runs a
# full self-test at boot and fail-closes to CPU if the GPU isn't bit-exact.
# ------------------------------------------------------------------------

# ── Config (override via env) ───────────────────────────────────────────
#   MINER       path to the midstate-miner binary (default: ./midstate-miner
#               or ./midstate-miner.exe next to this script, else PATH)
#   ADDRESS     your Midstate payout address (REQUIRED; or pass as $1)
#   SHARE_BITS  share-difficulty bits (must match the pool; default 14)
#   GPU_IDS     force the exact adapter indices to mine (skip autodetect),
#               space- or comma-separated, e.g. GPU_IDS="0 2" or GPU_IDS=0,2.
#               For power users who want a specific (or non-Vulkan) adapter set.
#   CPU_THREADS CPU worker threads for the first (hybrid) process
#               (default: physical-cores - 2, floored at 1)
#   LOG_DIR     where per-GPU logs go (default: ./logs-multi-gpu)
#   LIVE_SEC    liveness poll cadence in seconds (default: 10)
#   MAX_BACKOFF cap for the restart backoff in seconds (default: 60)
# ------------------------------------------------------------------------

HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

# Resolve the miner binary: prefer one sitting next to this script, else PATH.
pick_miner() {
  if [ -n "${MINER:-}" ]; then printf '%s\n' "$MINER"; return; fi
  for cand in "$HERE/midstate-miner" "$HERE/midstate-miner.exe"; do
    if [ -x "$cand" ]; then printf '%s\n' "$cand"; return; fi
  done
  if command -v midstate-miner >/dev/null 2>&1; then
    printf '%s\n' "midstate-miner"; return
  fi
  echo "ERROR: midstate-miner binary not found (set MINER=/path/to/midstate-miner)" >&2
  exit 1
}
MINER="$(pick_miner)"

ADDRESS="${ADDRESS:-${1:-}}"
if [ -z "$ADDRESS" ]; then
  echo "ERROR: no payout address. Usage: ADDRESS=<addr> $0   (or: $0 <addr>)" >&2
  exit 1
fi

SHARE_BITS="${SHARE_BITS:-14}"
LOG_DIR="${LOG_DIR:-$HERE/logs-multi-gpu}"
LIVE_SEC="${LIVE_SEC:-10}"
MAX_BACKOFF="${MAX_BACKOFF:-60}"
mkdir -p "$LOG_DIR"

# ── Detect the adapter INDICES to mine (one per PHYSICAL GPU) ────────────
# CRITICAL: `--list-gpus` prints ONE line per (adapter × backend), so a single
# physical card shows up as multiple lines — e.g. one RTX 5070 Ti appears as
# Vulkan + Dx12 + Cpu + Gl = 4 lines. Spawning one process per LINE would run
# 4 miners on ONE card. Instead we pick exactly ONE line per physical card:
# the `(Vulkan)` adapter that is NOT a software `[Cpu]` adapter. Each physical
# NVIDIA card exposes exactly one Vulkan adapter, so the count of such lines ==
# the physical GPU count, and each line's index is the `--gpu-id` to pin to.
#
# Line format (src/wgpu_backend.rs::adapter_line):  `INDEX: name [Type] (Backend)`
#
# Emits the chosen adapter indices, one per line. Resolution order:
#   1. GPU_IDS env override (power users) — exact indices, space/comma sep.
#   2. --list-gpus Vulkan-and-not-[Cpu] lines — one real index per physical GPU.
#   3. nvidia-smi -L count N → indices 0..N-1.
#   4. fallback: single index 0.
detect_gpu_ids() {
  # 1. Explicit override.
  if [ -n "${GPU_IDS:-}" ]; then
    printf '%s\n' "$GPU_IDS" | tr ',' ' ' | tr ' ' '\n' | grep -E '^[0-9]+$' || true
    return
  fi

  # 2. Parse --list-gpus: keep lines that are (Vulkan) AND not [Cpu], take their
  #    leading index. One such line per physical NVIDIA card.
  local listing ids
  listing="$("$MINER" --list-gpus 2>/dev/null || true)"
  ids="$(printf '%s\n' "$listing" \
    | grep -E '^[0-9]+:' \
    | grep -F '(Vulkan)' \
    | grep -vF '[Cpu]' \
    | sed -E 's/^([0-9]+):.*/\1/' \
    | grep -E '^[0-9]+$' || true)"
  if [ -n "$ids" ]; then printf '%s\n' "$ids"; return; fi

  # 3. No Vulkan line (non-wgpu binary, or ICD missing): fall back to nvidia-smi
  #    physical count → indices 0..N-1.
  if command -v nvidia-smi >/dev/null 2>&1; then
    local n
    n="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
    if [ "${n:-0}" -gt 0 ] 2>/dev/null; then
      local i=0
      while [ "$i" -lt "$n" ]; do printf '%s\n' "$i"; i=$(( i + 1 )); done
      return
    fi
  fi

  # 4. Last resort: a single adapter at index 0.
  printf '%s\n' "0"
}

# Collect the chosen indices into an array.
declare -a GPU_ID_LIST=()
while IFS= read -r _gid; do
  [ -n "$_gid" ] && GPU_ID_LIST+=("$_gid")
done < <(detect_gpu_ids)
# Guarantee at least one target.
if [ "${#GPU_ID_LIST[@]}" -eq 0 ]; then GPU_ID_LIST=(0); fi
NGPU="${#GPU_ID_LIST[@]}"

# CPU threads for the hybrid (first) process: physical cores - 2, floored at 1.
default_cpu_threads() {
  local cores=1
  if command -v nproc >/dev/null 2>&1; then cores="$(nproc)"; fi
  local t=$(( cores - 2 ))
  if [ "$t" -lt 1 ]; then t=1; fi
  printf '%s\n' "$t"
}
CPU_THREADS="${CPU_THREADS:-$(default_cpu_threads)}"

echo "midstate multi-GPU launcher | miner=$MINER | gpus=$NGPU (adapter ids: ${GPU_ID_LIST[*]}) | share_bits=$SHARE_BITS"
echo "  first GPU runs hybrid (CPU+GPU, cpu_threads=$CPU_THREADS); the rest run gpu-only"
echo "  per-GPU logs in: $LOG_DIR"

# ── Per-GPU supervisor loop with escalating backoff ─────────────────────
# Each GPU gets its own background subshell that (re)launches its miner and
# backs off on rapid exits so a wedged card doesn't hot-loop.
declare -a PIDS=()

run_one_gpu() {
  local slot="$1"   # position in GPU_ID_LIST (0 = first → hybrid)
  local gid="$2"    # the REAL adapter index passed to --gpu-id
  local log="$LOG_DIR/gpu-$gid.log"
  local backoff=5
  # The FIRST process (slot 0) also mines on the CPU (hybrid); the rest are
  # GPU-only. (slot, not gid: the first physical card may have any adapter index.)
  local mode_args
  if [ "$slot" -eq 0 ]; then
    mode_args=(--mode hybrid --cpu-threads "$CPU_THREADS")
  else
    mode_args=(--mode gpu)
  fi
  while true; do
    local started ended
    started="$(date +%s)"
    echo "[gpu $gid] starting: $MINER --address <addr> --gpu-id $gid --share-bits $SHARE_BITS ${mode_args[*]}" \
      | tee -a "$log"
    "$MINER" \
      --address "$ADDRESS" \
      --gpu-id "$gid" \
      --share-bits "$SHARE_BITS" \
      "${mode_args[@]}" \
      >>"$log" 2>&1 || true
    ended="$(date +%s)"
    # If it ran a healthy while, reset the backoff; else escalate (cap MAX_BACKOFF).
    if [ $(( ended - started )) -ge 60 ]; then
      backoff=5
    fi
    echo "[gpu $gid] exited; restarting in ${backoff}s" | tee -a "$log"
    sleep "$backoff"
    backoff=$(( backoff * 3 ))
    if [ "$backoff" -gt "$MAX_BACKOFF" ]; then backoff="$MAX_BACKOFF"; fi
  done
}

stop_all() {
  echo
  echo "stopping all GPU miners…"
  for pid in "${PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  # Also reap any lingering miner children.
  pkill -P $$ 2>/dev/null || true
  wait 2>/dev/null || true
  echo "stopped."
}
trap stop_all INT TERM EXIT

# Launch one supervisor per PHYSICAL GPU (slot = position, gid = real adapter index).
slot=0
for gid in "${GPU_ID_LIST[@]}"; do
  run_one_gpu "$slot" "$gid" &
  PIDS+=("$!")
  slot=$(( slot + 1 ))
done

echo "launched $NGPU GPU miner supervisor(s) on adapter id(s): ${GPU_ID_LIST[*]}. Ctrl+C to stop all."

# ── Liveness watch: if any supervisor dies (it shouldn't — it self-restarts),
# log it. The supervisors own per-GPU restart; this is the top-level heartbeat.
while true; do
  alive=0
  for pid in "${PIDS[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then alive=$(( alive + 1 )); fi
  done
  if [ "$alive" -eq 0 ]; then
    echo "all GPU supervisors have exited; shutting down."
    break
  fi
  sleep "$LIVE_SEC"
done
