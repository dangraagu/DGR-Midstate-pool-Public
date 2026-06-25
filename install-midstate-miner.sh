#!/usr/bin/env bash
set -euo pipefail

# ============================================================
#  Midstate Pool Miner - all-in-one installer for Ubuntu / Linux.
#  Run it. It will:
#    1. Detect your GPU (NVIDIA) or fall back to CPU. Midstate's PoW
#       is a SEQUENTIAL BLAKE3 chain (GPU-resistant), so the CPU build
#       is genuinely competitive - only a few× behind a GPU.
#    2. Download the matching prebuilt miner from GitHub Releases.
#    3. Ask for your Midstate payout address once (and remember it).
#    4. Start mining to the pool.
#  Override detection:  ./install-midstate-miner.sh gpu|cpu
#  Set MODE=cpu|gpu|hybrid|auto to choose the run mode (default auto).
#  GPU DRIVERS ARE NOT INSTALLED HERE - the gpu build needs your GPU's
#  OpenCL runtime/driver already present; otherwise use the cpu build.
#
#  Running via  curl ... | bash  (no terminal)? There is no TTY to
#  prompt on, so pass your address in the environment:
#     curl -fsSL <url> | MIDSTATE_ADDR=<address> bash
#  or as the second argument:  ... | bash -s -- <variant> <address>
# ============================================================

REPO="dangraagu/DGR-Midstate-pool-Public"

# XDG dirs: binary lives under data, address under config.
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/midstate-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/midstate-miner"
CFG="$CFG_DIR/address.txt"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === Midstate Pool Miner installer (Linux) ==="
echo

# --- helpers ---------------------------------------------------------------

# Download $1 -> $2 atomically using curl (preferred) or wget. We fetch into a
# temp file and only move it into place on success, so a failed/partial
# download can never leave a 0-byte file that later gets chmod+x'd and exec'd.
# Returns non-zero on failure.
download() {
  local url="$1" out="$2" tmp
  tmp="$out.tmp"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    echo "[X] Neither 'curl' nor 'wget' is installed. Install one and re-run." >&2
    echo "    Ubuntu/Debian:  sudo apt-get install -y curl" >&2
    return 1
  fi
}

# --- 1. Pick the build variant (arg overrides auto-detect) -----------------
# A GPU (NVIDIA, or any vendor with an OpenCL ICD) selects the gpu build
# (OpenCL/hybrid). The gpu build still runs CPU-only at runtime if no device is
# found. MODE (default auto) is passed through to mine-auto.sh at the end.
MODE="${MODE:-auto}"
case "$MODE" in
  cpu|gpu|hybrid|auto) ;;
  *) echo "[X] Unknown MODE '$MODE'. Use one of: cpu | gpu | hybrid | auto" >&2; exit 1 ;;
esac

VARIANT="${1:-}"
if [ -z "$VARIANT" ]; then
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    VARIANT="gpu"
  elif command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -qi 'Device Name'; then
    VARIANT="gpu"
  elif ls /etc/OpenCL/vendors/*.icd >/dev/null 2>&1; then
    VARIANT="gpu"
  else
    VARIANT="cpu"
  fi
fi

case "$VARIANT" in
  gpu|cpu) ;;
  nvidia) VARIANT="gpu" ;;  # back-compat alias
  *)
    echo "[X] Unknown build '$VARIANT'. Use one of: gpu | cpu" >&2
    exit 1
    ;;
esac
echo "Selected build: $VARIANT  (mode=$MODE)"

# Print the relevant prerequisite hint.
case "$VARIANT" in
  gpu)
    echo "  -> GPU build: needs an OpenCL runtime/ICD for your GPU (NVIDIA driver,"
    echo "     AMD/Intel OpenCL, etc.). Runs CPU-only if no device is found."
    ;;
  cpu)
    echo "  -> CPU build: no GPU or driver required. Midstate's sequential BLAKE3"
    echo "     PoW makes CPU mining genuinely competitive."
    ;;
esac

# Pick the OS asset-name segment so a macOS rig fetches the macOS binary, not a
# Linux ELF (this installer runs on both). Matches mine-auto.sh / release.yml.
case "$(uname -s 2>/dev/null)" in
  Darwin) PLATFORM="macos" ;;
  *)      PLATFORM="linux" ;;
esac
if [ "$VARIANT" = "cpu" ]; then
  BIN_NAME="midstate-miner-$PLATFORM"
else
  BIN_NAME="midstate-miner-$PLATFORM-gpu"
fi
BIN="$DATA_DIR/$BIN_NAME"
URL="https://github.com/$REPO/releases/latest/download/$BIN_NAME"

# --- 2. Download the matching miner ----------------------------------------
echo
echo "Downloading $BIN_NAME ..."
if ! download "$URL" "$BIN"; then
  echo
  echo "[X] Download failed. Either no release is published yet, the"
  echo "    '$VARIANT' build isn't in the latest release, or no network."
  echo "    Releases: https://github.com/$REPO/releases/latest"
  echo "    Tip: try another build, e.g.  ./install-midstate-miner.sh cpu"
  echo
  exit 1
fi

# --- 2a. Verify the downloaded binary against the release SHA256SUMS --------
# Defence in depth on the FIRST fetch (TLS already authenticates the GitHub CDN,
# but this also catches a truncated download / a tampered asset). FAIL CLOSED:
# any missing checksums file, missing entry, missing verifier, or hash mismatch
# removes the unverified binary and aborts the install. Nothing is running yet,
# so aborting can never brick a rig. The SHA256SUMS line format is
# `<hex>  <filename>` (sha256sum style); we match $BIN_NAME exactly.
echo "Verifying $BIN_NAME against the release SHA256SUMS ..."
SUMS_TMP="$DATA_DIR/SHA256SUMS.install"
if ! download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$SUMS_TMP" 2>/dev/null; then
  rm -f "$BIN"
  echo "[X] Could not fetch SHA256SUMS - refusing to install an unverified binary." >&2
  echo "    Releases: https://github.com/$REPO/releases/latest" >&2
  exit 1
fi
WANT="$(awk -v a="$BIN_NAME" '$2==a || $2=="*"a {print $1; exit}' "$SUMS_TMP")"
rm -f "$SUMS_TMP"
if [ -z "$WANT" ]; then
  rm -f "$BIN"
  echo "[X] '$BIN_NAME' is not listed in SHA256SUMS - refusing the install (fail-closed)." >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  GOT="$(sha256sum "$BIN" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  # macOS ships `shasum` (not `sha256sum`).
  GOT="$(shasum -a 256 "$BIN" | awk '{print $1}')"
else
  rm -f "$BIN"
  echo "[X] No sha256sum/shasum available to verify the download - refusing (fail-closed)." >&2
  exit 1
fi
if [ "$GOT" != "$WANT" ]; then
  rm -f "$BIN"
  echo "[X] SHA-256 verify FAILED for $BIN_NAME (got $GOT, want $WANT) - aborting install." >&2
  exit 1
fi
echo "  -> verified ($WANT)."
chmod +x "$BIN"

# --- 2b. Also fetch the auto-update launcher next to this file -------------
# Fetch mine-auto.sh from the RELEASE ASSET, NOT raw.githubusercontent main. The
# release asset is the SHA-covered artifact (listed in SHA256SUMS) that
# mine-auto.sh's own launcher self-update pulls, so a fresh install and a later
# self-update converge on identical, verified bytes — instead of installing the
# unverified raw-main blob on the first fetch. Fail-closed: download() returns
# non-zero on any failure and we only chmod on success (a failed fetch leaves no
# launcher; the hand-off below then falls back to running the verified binary).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "Fetching the auto-update launcher ..."
for f in mine-auto.sh; do
  if download "https://github.com/$REPO/releases/latest/download/$f" "$SCRIPT_DIR/$f" 2>/dev/null; then
    chmod +x "$SCRIPT_DIR/$f" 2>/dev/null || true
  fi
done
echo "  - mine-auto.sh      = auto-update launcher (recommended for 24/7)"

# --- 3. Midstate payout address: prompt once, remember thereafter ----------
# Midstate uses a long hex MSS payout address. We accept whatever hex the user
# provides and do NOT validate a fixed length (it is NOT a 40-hex addr).
ADDR=""
if [ -f "$CFG" ]; then
  ADDR="$(tr -d '[:space:]' < "$CFG")"
fi

if [ -z "$ADDR" ]; then
  # Second positional arg, then $MIDSTATE_ADDR, are accepted in any mode and are
  # the ONLY way to supply an address when there is no terminal (e.g. curl | bash,
  # where stdin is the pipe, not a TTY, so `read` would get script bytes/EOF).
  ADDR="${2:-${MIDSTATE_ADDR:-}}"
  if [ -z "$ADDR" ]; then
    if [ -t 0 ]; then
      echo
      echo "Enter YOUR Midstate payout address (hex) - where the pool sends"
      echo "your mining rewards:"
      printf '> '
      read -r ADDR
    else
      echo "[X] No saved address and not a TTY - cannot prompt." >&2
      echo "    Re-run in a terminal, or pass the address non-interactively:" >&2
      echo "      curl -fsSL <url> | MIDSTATE_ADDR=<address> bash" >&2
      echo "      ... | bash -s -- $VARIANT <address>" >&2
      exit 1
    fi
  fi
  ADDR="$(printf '%s' "$ADDR" | tr -d '[:space:]')"
fi

if [ -z "$ADDR" ]; then
  echo "[X] No address entered." >&2
  exit 1
fi

# Persist the address for next time (no length validation - it is a long hex MSS
# address, not a fixed 40-hex addr).
printf '%s\n' "$ADDR" > "$CFG"

# --- 4. Mine (hand off to the self-updating launcher) ----------------------
# IMPORTANT: we do NOT exec the raw binary here. Stranding a rig on an old
# version is the whole problem this fleet must avoid, so the one-click install
# ends by handing off to mine-auto.sh — which keeps polling GitHub and swaps in
# newer VERIFIED builds for as long as it runs. mine-auto.sh reuses the address
# we just saved to $CFG (no re-prompt).
echo
echo "Starting $VARIANT miner via the self-updating launcher (mine-auto.sh)."
echo "Payout address: $ADDR   (change it later by deleting: $CFG)"
echo "It auto-checks GitHub for updates and verifies each download before swapping it in."
echo "Press Ctrl+C to stop."
echo

MINE_AUTO="$SCRIPT_DIR/mine-auto.sh"
# Export MODE so it survives the exec into the launcher (which reads $MODE).
export MODE
if [ -x "$MINE_AUTO" ] || [ -f "$MINE_AUTO" ]; then
  # FAIL-SAFE: if the self-updating launcher can't start for any reason, fall
  # back to running the binary we just installed+verified so the rig still mines.
  exec bash "$MINE_AUTO" "$VARIANT" || exec "$BIN" --address "$ADDR" --mode "$MODE"
else
  echo "[!] mine-auto.sh not found next to the installer; running the installed"
  echo "    binary directly (no auto-update). Re-download mine-auto.sh for 24/7 rigs."
  exec "$BIN" --address "$ADDR"
fi
