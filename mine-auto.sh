#!/usr/bin/env bash
set -euo pipefail

# --- What this is -------------------------------------------------------
# Opt-in Midstate miner launcher: mines on THIS machine, to YOUR own
# payout address, only while you choose to run it. Not silent or hidden,
# and does not install or run itself on anyone else's computer. Standard
# pool miner for the public Midstate proof-of-work chain. See README.
# ------------------------------------------------------------------------

# ============================================================
#  Self-updating launcher. Leave this running.
#   * Runs the miner to your address. Midstate's PoW is a
#     SEQUENTIAL BLAKE3 chain (GPU-resistant), so a good CPU is
#     only a few× behind a GPU — both builds are worth running.
#     The gpu build runs OpenCL (and CPU+GPU together in hybrid/auto) in
#     ONE process; the binary's --mode picks cpu/gpu/hybrid/auto.
#   * Checks GitHub for the latest release every CHECK_MIN
#     minutes. A new version is gated through THREE checks before
#     it ever runs (brick-safe hardening):
#       1. version gate: plain string compare — update only when
#          the published version differs from the installed one,
#       2. download to a TEMP path (never onto the live binary),
#       3. SHA-256 verify against the release SHA256SUMS (OS
#          sha256sum) BEFORE the atomic swap.
#     A failed verify discards the temp and keeps the running
#     binary; the rig never executes an unverified download.
#   * Liveness is checked on a SHORT cadence (LIVE_SEC), decoupled
#     from the slow update poll, with ESCALATING BACKOFF so a
#     crash-looping rig doesn't hammer (5s,15s,60s capped). After
#     MAX_RESTARTS rapid restarts it backs off and (optionally)
#     runs your MIDSTATE_ON_CRASH hook.
#  MODE (cpu|gpu|hybrid|auto, default auto) picks which build to download AND
#  which --mode to run. `auto` auto-detects a GPU: if one is present it fetches
#  the GPU build (which then runs hybrid CPU+GPU), else the CPU build. You can
#  also force a build by passing it as the first arg (cpu | gpu):
#     MODE=hybrid ./mine-auto.sh        # GPU build, hybrid CPU+GPU
#     ./mine-auto.sh cpu                # force the CPU build
#  Stop everything: Ctrl+C (this also stops the miners).
#
#  Env knobs (all optional):
#     MODE          cpu | gpu | hybrid | auto             (default auto)
#     CHECK_MIN     update-poll period in minutes         (default 15)
#     LIVE_SEC      liveness-check period in seconds       (default 30)
#     MAX_RESTARTS  rapid restarts before backing off      (default 5)
#     MIDSTATE_ON_CRASH  path to a script run once when the
#                   restart cap is hit (driver reset, etc.)
# ============================================================

REPO="dangraagu/DGR-Midstate-pool-Public"

# MODE selects what we run (passed to the binary as --mode) AND which build to
# fetch. Resolve it from $MODE (env), defaulting to auto. An explicit first-arg
# build (cpu|gpu) overrides the download choice but MODE still drives --mode.
# The first positional arg (cpu|gpu) selects the build AND — unless MODE is set
# explicitly via the env — the run --mode too, so `mine-auto.sh cpu` runs
# `--mode cpu` (not the default `auto`). An explicit `MODE=...` env still wins.
if [ -z "${MODE:-}" ]; then
  case "${1:-}" in
    cpu)        MODE=cpu ;;
    gpu|nvidia) MODE=hybrid ;;
  esac
fi
MODE="${MODE:-auto}"
case "$MODE" in
  cpu|gpu|hybrid|auto) ;;
  *) echo "[X] Unknown MODE '$MODE'. Use one of: cpu | gpu | hybrid | auto" >&2; exit 1 ;;
esac

# Which BUILD to download: the CPU-only binary, or the GPU/hybrid (OpenCL) binary.
#   - first arg, if given, wins (cpu|gpu);
#   - else derive from MODE: cpu => cpu build; gpu|hybrid => gpu build;
#   - else (auto) auto-detect a GPU and pick the gpu build if one is present.
# The gpu build also runs fine CPU-only at runtime (it degrades gracefully if no
# OpenCL device), and the updater falls back to the cpu asset if the gpu asset is
# missing — so this choice is never a brick.
gpu_detected() {
  # OpenCL-agnostic "is there likely a GPU?" probe. nvidia-smi covers NVIDIA;
  # the presence of an ICD / clinfo covers AMD/Intel. Best-effort; a false
  # negative just means we fetch the cpu build (still mines).
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then return 0; fi
  if command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -qi 'Device Name'; then return 0; fi
  # Linux ICD vendor files present => an OpenCL platform is installed.
  if ls /etc/OpenCL/vendors/*.icd >/dev/null 2>&1; then return 0; fi
  return 1
}

VARIANT="${1:-}"
if [ -z "$VARIANT" ]; then
  case "$MODE" in
    cpu) VARIANT="cpu" ;;
    gpu|hybrid) VARIANT="gpu" ;;
    # auto: NVIDIA -> the CUDA build (fastest); any other GPU -> the OpenCL/wgpu
    # gpu build; no GPU -> cpu. Mirrors install-midstate-miner.sh + mine-auto.bat
    # so `auto` (the default) is CUDA-default on NVIDIA. The gpu-cuda -> gpu -> cpu
    # fail-closed fallback below covers a missing cuda asset or a driver that
    # cannot init, so auto-preferring cuda never bricks.
    auto)
      if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
        VARIANT="gpu-cuda"
      elif gpu_detected; then
        VARIANT="gpu"
      else
        VARIANT="cpu"
      fi ;;
  esac
fi
case "$VARIANT" in
  gpu|gpu-cuda|cpu) ;;
  # Convenience: 'cuda' => the CUDA build; 'nvidia' (old callers) => CUDA too now.
  cuda|nvidia) VARIANT="gpu-cuda" ;;
  *) echo "[X] Unknown build '$VARIANT'. Use one of: gpu-cuda | gpu | cpu" >&2; exit 1 ;;
esac

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/midstate-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/midstate-miner"
CFG="$CFG_DIR/address.txt"

# Pick the OS asset-name segment so a macOS rig fetches the macOS binary, not a
# Linux ELF (this same bash launcher runs on both). The release publishes
# midstate-miner-linux and midstate-miner-macos; PLATFORM selects which.
case "$(uname -s 2>/dev/null)" in
  Darwin) PLATFORM="macos" ;;
  *)      PLATFORM="linux" ;;
esac
# The CUDA asset (…-gpu-cuda) is built for Linux ONLY (release.yml). On macOS
# downgrade gpu-cuda to the cross-platform gpu (wgpu) build before mapping names.
if [ "$VARIANT" = "gpu-cuda" ] && [ "$PLATFORM" != "linux" ]; then
  echo "[!] CUDA build is Linux-only; using the gpu (wgpu) build on $PLATFORM."
  VARIANT="gpu"
fi
# Asset basename. cpu => midstate-miner-<platform>; gpu => midstate-miner-<platform>-gpu
# (the OpenCL/wgpu/hybrid build); gpu-cuda => midstate-miner-<platform>-gpu-cuda
# (the native CUDA build, Linux). The name MUST equal the release asset + its
# SHA256SUMS key (see release.yml ASSET-NAME CONTRACT).
case "$VARIANT" in
  cpu)      BIN_NAME="midstate-miner-$PLATFORM" ;;
  gpu)      BIN_NAME="midstate-miner-$PLATFORM-gpu" ;;
  gpu-cuda) BIN_NAME="midstate-miner-$PLATFORM-gpu-cuda" ;;
esac
BIN="$DATA_DIR/$BIN_NAME"
CHECK_MIN="${CHECK_MIN:-15}"
LIVE_SEC="${LIVE_SEC:-30}"
MAX_RESTARTS="${MAX_RESTARTS:-5}"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === Midstate Pool Miner - auto-update (build: $VARIANT) ==="
echo

# Download $1 -> $2 atomically: fetch to a temp file and only move it into
# place on success, so a failed/partial download never leaves a 0-byte binary
# that later gets chmod+x'd and exec'd. Returns non-zero on failure.
download() {
  local url="$1" out="$2" tmp
  tmp="$out.tmp"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    echo "[X] Neither 'curl' nor 'wget' is installed." >&2
    return 1
  fi
}

# Resolve the latest published version (empty string on any failure).
#
# Fetch it from the releases/latest/download/ CDN asset latest-version.txt
# — NOT api.github.com/.../releases/latest. The unauthenticated API is capped at
# 60 req/hr/IP, so ~20 rigs behind ONE public IP (a farm) get HTTP 403, an empty
# tag, and the whole farm SILENTLY stops updating. The CDN download path has no
# such per-IP limit. Output is the bare version (e.g. "0.1.10", no leading 'v').
# On offline/404 we return empty and the caller cleanly no-ops (keeps mining) —
# we deliberately do NOT fall back to the rate-limited API.
latest_tag() {
  local url="https://github.com/$REPO/releases/latest/download/latest-version.txt" out
  if command -v curl >/dev/null 2>&1; then
    out="$(curl -fsSL -H 'User-Agent: midstate-miner' "$url" 2>/dev/null)" || return 0
  elif command -v wget >/dev/null 2>&1; then
    out="$(wget -qO- --header='User-Agent: midstate-miner' "$url" 2>/dev/null)" || return 0
  else
    return 0
  fi
  # First non-empty line, whitespace-stripped, with any leading 'v' removed.
  out="$(printf '%s\n' "$out" | sed -e 's/[[:space:]]//g' -e '/^$/d' | head -n1)"
  printf '%s' "${out#v}"
}

# Decide whether to update: plain string inequality between $INSTALLED and
# $LATEST. Both are release-tag strings from the same source (latest-version.txt
# / the value recorded after the last swap; "none" before the first install), so
# "differs" means "a different release is published". The binary has no
# subcommands (flat clap parser) — there is no semver compare to call.
# Returns 0 (update) / non-zero (skip).
should_update() {
  local installed="$1" latest="$2"
  [ "$installed" != "$latest" ]
}

# Fetch SHA256SUMS for the latest release and echo the expected hex digest for
# $1 (the asset basename). Empty output => no checksums published (older
# release) OR the asset isn't listed; the caller treats empty as "cannot
# verify". The SHA256SUMS line format is `<hex>  <filename>` (sha256sum style).
expected_sha() {
  local asset="$1" sums
  sums="$DATA_DIR/SHA256SUMS.tmp"
  if download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" 2>/dev/null; then
    # Match the exact basename in the second field; print the first field (hex).
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
  fi
}

# Download the latest $VARIANT build, VERIFY it, and only then atomically swap
# it into $BIN. Never writes an unverified binary onto the live path:
#   1. download to a staging path "$BIN.new",
#   2. look up the expected SHA-256 from the release SHA256SUMS,
#   3. SHA-256 the staging copy with the OS sha256sum; if the digest matches,
#      mv it into place; if it does NOT match, discard the staging copy and
#      keep the running binary (return non-zero),
#   4. if no SHA256SUMS is published, FAIL CLOSED — refuse the swap and keep the
#      running binary (never accept an unverified download).
# If a non-cpu variant's asset is missing (404), fall back to the cpu build
# (always published), updating VARIANT/BIN_NAME/BIN so the loop tracks cpu.
# Returns non-zero only if no usable, verified binary could be staged.
download_verify_swap() {
  local staged="$BIN.new" want
  if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged"; then
    # gpu-cuda missing → try the plain gpu (OpenCL/wgpu) asset before dropping to
    # cpu, so an NVIDIA rig keeps a GPU build (slower path) rather than cpu-only.
    if [ "$VARIANT" = "gpu-cuda" ]; then
      echo "[!] 'gpu-cuda' build unavailable. Falling back to the gpu (OpenCL/wgpu) build." >&2
      VARIANT="gpu"
      BIN_NAME="midstate-miner-$PLATFORM-gpu"
      BIN="$DATA_DIR/$BIN_NAME"
      staged="$BIN.new"
      MULTI_GPU=0  # per-card fan-out is a CUDA-only path; the gpu build is single-process
    fi
    if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged"; then
      if [ "$VARIANT" != "cpu" ]; then
        echo "[!] '$VARIANT' build unavailable (download failed / 404). Falling back to the cpu build." >&2
        VARIANT="cpu"
        BIN_NAME="midstate-miner-$PLATFORM"
        BIN="$DATA_DIR/$BIN_NAME"
        staged="$BIN.new"
        MULTI_GPU=0
        # The CPU binary rejects --mode gpu/hybrid; downgrade an explicit GPU mode to
        # auto so the fallback rig mines on CPU instead of erroring out on every start.
        case "$MODE" in gpu|hybrid) MODE="auto" ;; esac
        download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged" || return 1
      else
        return 1
      fi
    fi
  fi

  want="$(expected_sha "$BIN_NAME")"
  if [ -z "$want" ]; then
    # FAIL CLOSED. Every live release publishes SHA256SUMS, so a missing
    # SHA256SUMS (or our asset not being listed in it) is anomalous, not routine —
    # refuse the update and keep running the EXISTING binary rather than swapping
    # in an unverified download.
    echo "[X] refusing unverified update: no SHA256SUMS published (or '$BIN_NAME' not listed in it). Keeping the running binary." >&2
    rm -f "$staged"
    return 1
  else
    # Verify with a TRUSTED tool ONLY: the OS sha256sum. NEVER let the
    # just-downloaded $staged verify itself — a malicious download would simply
    # pass its own check. And if we have a digest but NO trusted verifier, FAIL
    # CLOSED (refuse the swap) rather than run an unverified binary.
    if command -v sha256sum >/dev/null 2>&1; then
      local got
      got="$(sha256sum "$staged" | awk '{print $1}')"
      if [ "$got" != "$want" ]; then
        echo "[X] SHA-256 verify FAILED for the downloaded $BIN_NAME (got $got, want $want) - discarding it." >&2
        rm -f "$staged"
        return 1
      fi
    else
      echo "[X] have a SHA256SUMS digest but no trusted verifier (no sha256sum) - refusing the update." >&2
      rm -f "$staged"
      return 1
    fi
  fi

  chmod +x "$staged"
  mv "$staged" "$BIN"   # atomic swap onto the live path, only after verify
}

# Absolute path of THIS launcher script, captured before any cd, so the launcher
# self-update writes back to the right file even if $0 was relative. mine-auto.sh
# never cd's, but we resolve defensively.
SELF_PATH="$0"
case "$SELF_PATH" in
  /*) : ;;                                  # already absolute
  *)  SELF_PATH="$(pwd)/$SELF_PATH" ;;      # make relative invocation absolute
esac
SELF_NAME="mine-auto.sh"   # the release-asset basename + the SHA256SUMS key

# Update the LAUNCHER ITSELF (this mine-auto.sh) in place, fail-closed + no-brick.
#
# WHY: download_verify_swap above swaps only the miner BINARY ($BIN); a fix to
# THIS script would otherwise never reach a rig that only ever runs the old
# on-disk launcher. So after a verified binary swap we also refresh the launcher —
# but with the SAME three-gate discipline and two extra hard safety rules, because
# a bad launcher is a brick, not just a stale miner:
#
#   SAFETY (a) FAIL-CLOSED: a download failure, a missing/!listed SHA256SUMS
#     entry for "$SELF_NAME", a SHA mismatch, or no trusted verifier ALL discard
#     the temp and leave the on-disk launcher byte-for-byte untouched. The rig
#     keeps running the known-good launcher. We never write an unverified script
#     over ourselves.
#   SAFETY (b) NO-BRICK / NO RE-EXEC: we DO NOT exec the new launcher mid-run.
#     A re-exec of a subtly-broken new script could crash-loop the rig with no
#     human present (no clawback). Instead we replace the file ATOMICALLY on disk
#     (write temp → mv) and let it take effect on the NEXT operator start/restart
#     of mine-auto.sh. The currently-running process keeps using the already-
#     loaded (old) script text, so this launch can never be bricked by the swap.
#     We also keep "$SELF_PATH.bak" (the prior launcher) as a manual fallback.
#
# Verifier trust: we verify with the OS sha256sum — NEVER by letting the
# downloaded script check itself. Returns non-zero on any skip/failure; the
# caller treats this as purely best-effort and ignores the result (a
# launcher-update failure must never disturb mining).
update_launcher_self() {
  local staged want got cur
  staged="$SELF_PATH.new.$$"

  # 1. Download the candidate launcher to a per-pid temp (never onto $SELF_PATH).
  if ! download "https://github.com/$REPO/releases/latest/download/$SELF_NAME" "$staged"; then
    echo "[$(date '+%H:%M:%S')] launcher self-update: download failed; keeping the on-disk launcher." >&2
    rm -f "$staged"
    return 1
  fi

  # 2. Expected SHA-256 from the SAME release SHA256SUMS, keyed by basename.
  want="$(expected_sha "$SELF_NAME")"
  if [ -z "$want" ]; then
    # FAIL CLOSED: no published launcher checksum (or this release doesn't ship
    # the launcher as an asset) → refuse, keep the on-disk launcher untouched.
    echo "[$(date '+%H:%M:%S')] launcher self-update: no SHA256SUMS entry for '$SELF_NAME' — refusing (keeping on-disk launcher)." >&2
    rm -f "$staged"
    return 1
  fi

  # 3. Verify the temp with a TRUSTED verifier (never the downloaded script
  #    itself): the OS sha256sum; if it is unavailable, refuse.
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$staged" | awk '{print $1}')"
    if [ "$got" != "$want" ]; then
      echo "[$(date '+%H:%M:%S')] launcher self-update: SHA-256 verify FAILED for $SELF_NAME (got $got, want $want) — discarding." >&2
      rm -f "$staged"
      return 1
    fi
  else
    echo "[$(date '+%H:%M:%S')] launcher self-update: have a digest but no trusted verifier — refusing." >&2
    rm -f "$staged"
    return 1
  fi

  # 4. Skip the write if the on-disk launcher is already byte-identical (same
  #    SHA) — avoids needless churn + a pointless .bak rewrite every poll.
  if command -v sha256sum >/dev/null 2>&1; then
    cur="$(sha256sum "$SELF_PATH" 2>/dev/null | awk '{print $1}')"
    if [ -n "$cur" ] && [ "$cur" = "$want" ]; then
      rm -f "$staged"
      return 0
    fi
  fi

  # 5. NO-BRICK atomic on-disk swap. Keep the prior launcher as .bak, preserve the
  #    exec bit, mv into place. We DO NOT exec it — it takes effect next start.
  chmod +x "$staged" 2>/dev/null || true
  cp -p "$SELF_PATH" "$SELF_PATH.bak" 2>/dev/null || true
  if mv "$staged" "$SELF_PATH"; then
    echo "[$(date '+%H:%M:%S')] launcher self-update: refreshed $SELF_NAME on disk (takes effect on the next start; prior kept at $SELF_PATH.bak)."
    return 0
  fi
  echo "[$(date '+%H:%M:%S')] launcher self-update: on-disk swap failed — keeping current launcher." >&2
  rm -f "$staged"
  return 1
}

# Test hook: when MIDSTATE_SOURCE_ONLY=1, stop here so a test can `source` this
# script to exercise the functions above (download / expected_sha /
# download_verify_swap / update_launcher_self) WITHOUT running the address prompt,
# GPU detection, or the mining loop. Has ZERO effect on a normal
# `./mine-auto.sh <variant>` run (the var is unset there). A sourced script uses
# `return`; on the (unexpected) case of being executed directly with the var set,
# `exit` is the fallback. The `# shellcheck disable` covers SC2317's false
# "unreachable" on the fallback.
if [ "${MIDSTATE_SOURCE_ONLY:-0}" = "1" ]; then
  # shellcheck disable=SC2317
  { return 0 2>/dev/null; exit 0; }
fi

# --- payout address (reuse the saved one, else prompt) ---------------------
# Midstate uses a long hex MSS payout address. We accept whatever hex the user
# provides and do NOT validate a fixed length (it is NOT a 40-hex addr).
ADDR=""
if [ -f "$CFG" ]; then
  ADDR="$(tr -d '[:space:]' < "$CFG")"
fi
if [ -z "$ADDR" ]; then
  printf 'Enter your Midstate payout address (hex): '
  read -r ADDR
  ADDR="$(printf '%s' "$ADDR" | tr -d '[:space:]')"
fi
if [ -z "$ADDR" ]; then
  echo "[X] No address entered." >&2
  exit 1
fi
printf '%s\n' "$ADDR" > "$CFG"

echo "Mining to $ADDR (mode=$MODE, build=$VARIANT)."
echo "Auto-checking GitHub for updates every $CHECK_MIN min (liveness every ${LIVE_SEC}s). Keep this running."
echo

PIDS=()

# How many physical NVIDIA GPUs does this rig have? Used to decide whether to
# fan out one miner process PER GPU (the high-throughput default on a multi-GPU
# NVIDIA box) or run the single in-process path. Counts `nvidia-smi -L` lines;
# 0 when there is no NVIDIA driver / not an NVIDIA rig. Best-effort — any failure
# yields 0, which falls back to the (unchanged) single-process launch.
nvidia_gpu_count() {
  local n=0
  if command -v nvidia-smi >/dev/null 2>&1; then
    n="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
  fi
  [ "${n:-0}" -ge 0 ] 2>/dev/null || n=0
  printf '%s' "${n:-0}"
}

# Spawn one miner process PER NVIDIA GPU only when it actually pays off: the
# build can use CUDA (the cuda asset) AND we are running a GPU/hybrid/auto mode
# AND there is more than one card. A single-GPU rig, a forced cpu mode, or a
# non-cuda build keeps the original single-process path untouched.
MULTI_GPU=0
NGPU=1
if [ "$VARIANT" = "gpu-cuda" ] && [ "$MODE" != "cpu" ]; then
  NGPU="$(nvidia_gpu_count)"
  [ "${NGPU:-1}" -gt 1 ] 2>/dev/null && MULTI_GPU=1
fi
if [ "$MULTI_GPU" -eq 1 ]; then
  echo "Detected $NGPU NVIDIA GPUs -> spawning one worker per GPU (rig-g0..rig-g$(( NGPU - 1 )), CUDA_VISIBLE_DEVICES pinning). g0 = hybrid (CPU+GPU), rest gpu-only."
  echo
fi

stop_miners() {
  if [ "${#PIDS[@]}" -gt 0 ]; then
    kill "${PIDS[@]}" 2>/dev/null || true
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  # Belt and braces: kill any stragglers by binary name.
  pkill -f "$BIN_NAME" 2>/dev/null || true
  PIDS=()
}

# Start the miner(s).
#
# SINGLE-PROCESS (default for 0/1 GPU, cpu mode, or a non-cuda build): one
# process per rig. The binary handles all hardware itself — --mode picks
# cpu / gpu / hybrid / auto, and the GPU backend drives the device while the
# hybrid backend runs CPU + GPU concurrently in-process. We log to a per-build
# file under $DATA_DIR.
#
# MULTI-GPU (CUDA build + GPU/hybrid/auto mode + >1 NVIDIA card): fan out ONE
# process per physical GPU, each pinned to a single card via CUDA_VISIBLE_DEVICES
# (so every worker sees exactly one device at index 0 and the CUDA backend can
# never collide on a card). This is the fix for a multi-4090 rig getting a
# fraction of a native miner's rate: the embarrassingly-parallel nonce search now
# runs N independent saturating processes instead of one process time-slicing the
# cards. The FIRST worker (g0) runs --mode hybrid so spare CPU cores aren't idle;
# the rest run --mode gpu. Each worker gets its own log (rig-g<i>.log). The
# fail-safe invariant is unchanged: each process still boot-self-tests GPU vs CPU
# midstate_pow and CPU-re-verifies every candidate before it becomes a share.
start_miners() {
  PIDS=()
  local logdir="$DATA_DIR/${VARIANT}-log"
  mkdir -p "$logdir"

  if [ "$MULTI_GPU" -ne 1 ]; then
    "$BIN" --address "$ADDR" --mode "$MODE" \
      > "$logdir/stdout.log" 2>&1 &
    PIDS+=("$!")
    return
  fi

  # One worker per GPU. CUDA_VISIBLE_DEVICES=i masks all but card i, so --gpu-id 0
  # (or omitted) targets exactly that card. Worker g0 = hybrid (CPU+GPU), rest gpu.
  local i wmode
  for i in $(seq 0 $(( NGPU - 1 ))); do
    if [ "$i" -eq 0 ]; then wmode="hybrid"; else wmode="gpu"; fi
    # hybrid only makes sense if the operator didn't force a plain gpu run.
    [ "$MODE" = "gpu" ] && wmode="gpu"
    CUDA_VISIBLE_DEVICES="$i" "$BIN" --address "$ADDR" --mode "$wmode" --gpu-id 0 \
      > "$logdir/rig-g$i.log" 2>&1 &
    PIDS+=("$!")
  done
}

# Are ALL of our launched miners still alive?
# v0.1.9 deploy-check fix (F2): this was ANY-alive, which on a multi-GPU rig
# meant a single dead worker (crash, or the never-dark fallback's 1800s
# re-probe exit) was NEVER respawned while a sibling lived — that worker's
# card sat dark until the next release bounced the set. Now any dead worker
# fails liveness -> the supervisor bounces the WHOLE set (healthy siblings
# lose seconds per cycle, and every card gets re-probed — which is exactly
# what the never-dark TTL exit is for). An empty set still reports not-running.
miners_running() {
  local p any=0
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] || continue
    any=1
    kill -0 "$p" 2>/dev/null || return 1
  done
  [ "$any" -eq 1 ]
}

# Run the optional operator crash hook (driver reset, reboot, etc.) once.
run_crash_hook() {
  if [ -n "${MIDSTATE_ON_CRASH:-}" ]; then
    if [ -x "$MIDSTATE_ON_CRASH" ]; then
      echo "[$(date '+%H:%M:%S')] running MIDSTATE_ON_CRASH hook: $MIDSTATE_ON_CRASH"
      "$MIDSTATE_ON_CRASH" || echo "[$(date '+%H:%M:%S')] MIDSTATE_ON_CRASH hook exited non-zero (continuing)."
    else
      echo "[$(date '+%H:%M:%S')] MIDSTATE_ON_CRASH set but '$MIDSTATE_ON_CRASH' is not executable - skipping." >&2
    fi
  fi
}

# Clean shutdown on Ctrl+C / TERM.
cleanup() {
  echo
  echo "Stopping miners ..."
  stop_miners
  exit 0
}
trap cleanup INT TERM

INSTALLED="none"
RESTARTS=0          # rapid restarts since the last sustained-healthy window
BACKOFF=0           # current crash-loop backoff in seconds (0 when healthy)
HOOK_FIRED=0        # so the crash hook runs once per crash-loop, not every tick
LAST_UPDATE_CHECK=0 # epoch seconds of the last update poll

# One-shot: pull the latest release before the first launch so we start current.
do_update_check() {
  local latest
  latest="$(latest_tag || true)"
  if [ -n "$latest" ] && should_update "$INSTALLED" "$latest"; then
    echo "[$(date '+%H:%M:%S')] update: $INSTALLED -> $latest  (verify, then swap + restart)"
    stop_miners
    if download_verify_swap; then
      INSTALLED="$latest"
      start_miners
      RESTARTS=0; BACKOFF=0; HOOK_FIRED=0
      echo "[$(date '+%H:%M:%S')] now mining $latest (build: $VARIANT)."
      # Best-effort: also refresh THIS launcher (so a launcher-side fix reaches
      # the rig). Runs AFTER mining is back up so it can never delay the restart;
      # fail-closed + no-brick (replaces on disk, no re-exec); result ignored.
      update_launcher_self || true
    else
      echo "[$(date '+%H:%M:%S')] update not applied (download/verify failed); keeping current, will retry."
      # If we had a running set, bring it back so a failed update doesn't leave
      # the rig idle.
      [ "$INSTALLED" != "none" ] && start_miners
    fi
  fi
  LAST_UPDATE_CHECK="$(date +%s)"
}

do_update_check

while true; do
  now="$(date +%s)"

  # Slow path: poll for a new release every CHECK_MIN minutes.
  if [ $((now - LAST_UPDATE_CHECK)) -ge $((CHECK_MIN * 60)) ]; then
    do_update_check
  fi

  # Fast path: keep the miners alive with escalating backoff. A miner set that
  # dies is restarted quickly; but if it keeps dying (>= MAX_RESTARTS in this
  # window) we back off (5s,15s,60s capped) and fire the crash hook ONCE, so a
  # flapping rig (driver/hardware fault) is not hammered and the pool is not
  # spammed. A sustained-healthy LIVE_SEC tick resets the counter.
  if [ "$INSTALLED" != "none" ]; then
    if ! miners_running; then
      if [ "$RESTARTS" -ge "$MAX_RESTARTS" ]; then
        if [ "$BACKOFF" -eq 0 ]; then BACKOFF=5; else BACKOFF=$((BACKOFF * 3)); fi
        [ "$BACKOFF" -gt 60 ] && BACKOFF=60
        echo "[$(date '+%H:%M:%S')] miners crash-looping ($RESTARTS restarts) - backing off ${BACKOFF}s before retry." >&2
        if [ "$HOOK_FIRED" -eq 0 ]; then run_crash_hook; HOOK_FIRED=1; fi
        sleep "$BACKOFF"
      fi
      echo "[$(date '+%H:%M:%S')] miners not running - restarting"
      start_miners
      RESTARTS=$((RESTARTS + 1))
    else
      # Healthy this tick: decay the crash-loop state so a later isolated crash
      # gets a fast restart again.
      if [ "$RESTARTS" -gt 0 ]; then RESTARTS=0; BACKOFF=0; HOOK_FIRED=0; fi
    fi
  fi

  sleep "$LIVE_SEC"
done
