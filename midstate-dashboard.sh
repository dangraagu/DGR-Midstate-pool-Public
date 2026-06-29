#!/usr/bin/env bash
# midstate-dashboard.sh — live terminal dashboard for the Midstate pool miner.
#
# Licensed under PolyForm Perimeter 1.0.0 (see LICENSE). Part of midstate-pool-miner.
#
# A READ-ONLY remote viewer: it does nothing but GET the PUBLIC pool API
# (<base>/api/miner/<address> + <base>/api/history) once per refresh and draw it.
# It never writes config, never touches the miner binary or the share/submit
# path, and never opens a socket to anything but the public pool over HTTPS.
# Worst case it prints "endpoint unreachable / set your address" — it cannot
# stop, slow, or corrupt mining. It works against the public pool regardless of
# which fleet version you mine with (it reads the pool's stats, not the miner's).
#
# Usage:
#   midstate-dashboard.sh [--address ADDR] [--refresh N] [--once] [--no-color] [--update] [-h]
# Address precedence: --address > $MIDSTATE_ADDR > ~/.config/midstate-miner/address.txt
# Pool API base:      $MIDSTATE_POOL_API (default https://midstate.yamaduo.no)
set -u

SELF_NAME="midstate-dashboard.sh"
REPO_DL="https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/latest/download"

POOL_API="${MIDSTATE_POOL_API:-https://midstate.yamaduo.no}"
ADDRESS=""
REFRESH="${MIDSTATE_REFRESH:-5}"
ONCE=0
NO_COLOR_FLAG=0
DO_UPDATE=0

usage() {
  cat <<EOF
midstate-dashboard.sh — live Midstate pool miner dashboard (read-only viewer)

  --address ADDR  your Midstate payout address (default: \$MIDSTATE_ADDR or
                  the address saved by the miner at ~/.config/midstate-miner/address.txt)
  --refresh N     seconds between refreshes (default: 5)
  --once          print one frame and exit (good for pipes / cron / HiveOS)
  --no-color      disable ANSI color
  --update        self-update this script from the latest release (fail-closed)
  -h, --help      this help

Pool API base: \$MIDSTATE_POOL_API overrides the default (https://midstate.yamaduo.no).
Reads only the PUBLIC pool stats API — never the miner binary or share path.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --address) ADDRESS="${2:-}"; shift 2 || { echo "missing value for --address" >&2; exit 2; } ;;
    --refresh) REFRESH="${2:-}"; shift 2 || { echo "missing value for --refresh" >&2; exit 2; } ;;
    --once) ONCE=1; shift ;;
    --no-color) NO_COLOR_FLAG=1; shift ;;
    --update) DO_UPDATE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# integer-only refresh (busybox sleep takes no fractions)
case "$REFRESH" in (*[!0-9]*|"") REFRESH=5 ;; esac
[ "$REFRESH" -lt 1 ] 2>/dev/null && REFRESH=1

# strip a trailing slash off the API base so "<base>/api/..." never doubles up
case "$POOL_API" in (*/) POOL_API="${POOL_API%/}" ;; esac

# ---- resolve payout address: --address > $MIDSTATE_ADDR > saved file ---------
if [ -z "$ADDRESS" ]; then ADDRESS="${MIDSTATE_ADDR:-}"; fi
if [ -z "$ADDRESS" ]; then
  CFG="${XDG_CONFIG_HOME:-$HOME/.config}/midstate-miner/address.txt"
  if [ -f "$CFG" ]; then ADDRESS="$(tr -d '[:space:]' < "$CFG" 2>/dev/null)"; fi
fi
# (an empty ADDRESS is NOT fatal — render() shows a "set your address" banner)

# ---- one HTTP GET (curl, fall back to wget) ---------------------------------
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 6 "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --timeout=6 "$1"
  else
    return 99
  fi
}

# ---- self-update (fail-closed, best-effort) ---------------------------------
self_update() {
  local self dir tmp want got verifier
  self="$0"; case "$self" in (/*) : ;; (*) self="$(pwd)/$self" ;; esac
  dir="$(dirname "$self")"; tmp="$dir/.$SELF_NAME.new.$$"
  for verifier in sha256sum "shasum -a 256"; do
    command -v "${verifier%% *}" >/dev/null 2>&1 && break || verifier=""
  done
  [ -z "$verifier" ] && { echo "no sha256 verifier; refusing to self-update" >&2; return 1; }
  fetch "$REPO_DL/SHA256SUMS" > "$tmp.sums" 2>/dev/null || { echo "SHA256SUMS fetch failed" >&2; rm -f "$tmp.sums"; return 1; }
  want="$(awk -v f="$SELF_NAME" '$2==f || $2=="*"f {print $1}' "$tmp.sums" | head -n1)"
  rm -f "$tmp.sums"
  [ -z "$want" ] && { echo "$SELF_NAME not in SHA256SUMS; refusing" >&2; return 1; }
  fetch "$REPO_DL/$SELF_NAME" > "$tmp" 2>/dev/null || { echo "download failed" >&2; rm -f "$tmp"; return 1; }
  got="$($verifier "$tmp" | awk '{print $1}')"
  if [ "$got" != "$want" ]; then echo "checksum mismatch; refusing (kept current)" >&2; rm -f "$tmp"; return 1; fi
  chmod +x "$tmp" 2>/dev/null
  cp "$self" "$self.bak" 2>/dev/null
  mv "$tmp" "$self" && { echo "updated $SELF_NAME (prior at $SELF_NAME.bak)"; return 0; }
  rm -f "$tmp"; echo "install failed; kept current" >&2; return 1
}

if [ "$DO_UPDATE" = 1 ]; then self_update; exit $?; fi

# ---- color resolution (once) ------------------------------------------------
C_R=""; C_G=""; C_Y=""; C_C=""; C_D=""; C_B=""; C_0=""
color_off=0
[ "$NO_COLOR_FLAG" = 1 ] && color_off=1
[ -n "${NO_COLOR:-}" ] && color_off=1
[ ! -t 1 ] && [ "$ONCE" = 1 ] && color_off=1
[ "${TERM:-dumb}" = dumb ] && color_off=1
if [ "$color_off" = 0 ]; then
  C_R='\033[31m'; C_G='\033[32m'; C_Y='\033[33m'; C_C='\033[36m'; C_D='\033[90m'; C_B='\033[1m'; C_0='\033[0m'
fi

# ---- helpers ----------------------------------------------------------------
# humanize H/s -> "1.21 GH/s" (busybox-awk safe; handles sci notation)
hr() {
  awk -v v="${1:-0}" 'BEGIN{
    v=v+0; u="H/s";
    if (v>=1e12){v/=1e12;u="TH/s"} else if (v>=1e9){v/=1e9;u="GH/s"}
    else if (v>=1e6){v/=1e6;u="MH/s"} else if (v>=1e3){v/=1e3;u="kH/s"}
    printf "%.2f %s", v, u
  }'
}
# seconds -> "3h 12m 40s"
uptime_fmt() {
  awk -v s="${1:-0}" 'BEGIN{ s=int(s+0); h=int(s/3600); m=int((s%3600)/60); x=s%60;
    if(h>0) printf "%dh %dm %ds",h,m,x; else if(m>0) printf "%dm %ds",m,x; else printf "%ds",x }'
}
# mid-truncate a long address -> "abc123…9f3c"
trunc_addr() {
  awk -v a="${1:-}" 'BEGIN{ if(length(a)>14) printf "%s…%s", substr(a,1,6), substr(a,length(a)-3); else printf "%s", a }'
}
# pure-sed scalar extractors (used when jq is absent). Tolerant of optional
# whitespace after the colon (serde pretty uses ": ", compact uses ":") and
# assume the input has already had newlines stripped (see no-jq branch).
js_str()  { printf '%s' "$1" | sed -n "s/.*\"$2\"[ ]*:[ ]*\"\([^\"]*\)\".*/\1/p" | head -n1; }
js_num()  { printf '%s' "$1" | sed -n "s/.*\"$2\"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p" | head -n1; }

HAS_JQ=0; command -v jq >/dev/null 2>&1 && HAS_JQ=1

# pull the LAST {"ts":..,"pool_hs":..,"net_hs":..} sample out of /api/history.
# With jq: read the .samples[-1]. Without jq: collapse to one line, grab the
# last pool_hs / net_hs occurrence (the samples are time-ordered, last = now).
hist_last() {
  local h pool net
  h="$(fetch "$POOL_API/api/history" 2>/dev/null)"
  [ -z "$h" ] && { printf '0|0'; return; }
  if [ "$HAS_JQ" = 1 ]; then
    printf '%s' "$h" | jq -r '(.samples // [] | last) as $s | "\(($s.pool_hs // 0))|\(($s.net_hs // 0))"' 2>/dev/null
  else
    h="$(printf '%s' "$h" | tr -d '\n\r')"
    pool="$(printf '%s' "$h" | sed -n 's/.*"pool_hs"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p' | head -n1)"
    net="$(printf '%s' "$h" | sed -n 's/.*"net_hs"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p' | head -n1)"
    : "${pool:=0}"; : "${net:=0}"
    printf '%s|%s' "$pool" "$net"
  fi
}

# previous-sample state for the accepted-share/min rate
PREV_GOOD=""; PREV_TS=""

# ---- terminal setup / teardown ----------------------------------------------
restore() { [ "$ONCE" = 1 ] || { [ -t 1 ] && printf '\033[?25h\033[0m\n'; }; }
trap 'restore; exit 0' INT TERM
trap 'restore' EXIT
if [ "$ONCE" = 0 ] && [ -t 1 ]; then printf '\033[2J\033[H\033[?25l'; fi

# ---- render one frame -------------------------------------------------------
render() {
  local body rc _tty
  local addr h5 h1 h6 acc rej lastdiff workers contrib eph eps sess pend owed life
  _tty=0; [ "$ONCE" = 0 ] && [ -t 1 ] && _tty=1
  [ "$_tty" = 1 ] && printf '\033[H'

  # no address configured at all — remediation banner, never blank
  if [ -z "$ADDRESS" ]; then
    printf "${C_B}${C_C}  Midstate Pool Miner${C_0}\n\n"
    printf "  ${C_Y}● set your payout address${C_0}\n\n"
    printf "  Pass ${C_B}--address <ADDR>${C_0}, set ${C_B}MIDSTATE_ADDR${C_0}, or run the miner once\n"
    printf "  (it saves your address to ${C_D}~/.config/midstate-miner/address.txt${C_0}).\n"
    [ "$ONCE" = 0 ] && printf "\n  ${C_D}retrying every %ss · q / Ctrl-C to quit${C_0}\n" "$REFRESH"
    [ "$_tty" = 1 ] && printf '\033[J'
    return
  fi

  body="$(fetch "$POOL_API/api/miner/$ADDRESS")"; rc=$?

  if [ $rc -ne 0 ] || [ -z "$body" ]; then
    # endpoint down / unreachable / unknown address — remediation, never blank
    printf "${C_B}${C_C}  Midstate Pool Miner${C_0}${C_D}   %s${C_0}\n\n" "$(trunc_addr "$ADDRESS")"
    printf "  ${C_R}● pool API unreachable${C_0}\n"
    printf "  ${C_D}%s${C_0}\n\n" "$POOL_API/api/miner/$ADDRESS"
    printf "  Is ${C_B}%s${C_0} reachable, and is the address correct?\n" "$POOL_API"
    printf "  Override the base with ${C_B}MIDSTATE_POOL_API${C_0}, the address with ${C_B}--address${C_0}.\n"
    printf "  ${C_D}(A brand-new miner has no stats until its first accepted share.)${C_0}\n"
    [ "$ONCE" = 0 ] && printf "\n  ${C_D}retrying every %ss · q / Ctrl-C to quit${C_0}\n" "$REFRESH"
    [ "$_tty" = 1 ] && printf '\033[J'
    return
  fi

  if [ "$HAS_JQ" = 1 ]; then
    # one jq pass, fields joined on '|'. A non-whitespace separator is REQUIRED:
    # with a tab/whitespace IFS, `read` collapses empty fields, so a missing
    # field would shift every later field left. '|' is safe: no value
    # (hex address, ints, floats, CSD-amount strings) contains it.
    IFS='|' read -r addr h5 h1 h6 acc rej lastdiff workers contrib eph eps sess pend owed life <<EOF
$(printf '%s' "$body" | jq -r '[(.address//""),((.hr5m_hs//0)|tostring),((.hr1h_hs//0)|tostring),((.hr6h_hs//0)|tostring),((.shares_accepted//0)|tostring),((.shares_rejected//0)|tostring),((.last_difficulty//0)|tostring),((.connected_workers//0)|tostring),((.contribution_pct//0)|tostring),((.est_csd_per_hour//0)|tostring),((.est_csd_this_session//0)|tostring),((.session_secs//0)|tostring),(.pending_csd//"0"),(.owed_csd//"0"),(.lifetime_paid_csd//"0")]|join("|")' 2>/dev/null)
EOF
  else
    # collapse to one line so the sed extractors work on pretty OR compact JSON
    local b1; b1="$(printf '%s' "$body" | tr -d '\n\r')"
    addr="$(js_str "$b1" address)"
    h5="$(js_num "$b1" hr5m_hs)"; h1="$(js_num "$b1" hr1h_hs)"; h6="$(js_num "$b1" hr6h_hs)"
    acc="$(js_num "$b1" shares_accepted)"; rej="$(js_num "$b1" shares_rejected)"
    lastdiff="$(js_num "$b1" last_difficulty)"
    workers="$(js_num "$b1" connected_workers)"; contrib="$(js_num "$b1" contribution_pct)"
    eph="$(js_num "$b1" est_csd_per_hour)"; eps="$(js_num "$b1" est_csd_this_session)"
    sess="$(js_num "$b1" session_secs)"
    pend="$(js_str "$b1" pending_csd)"; owed="$(js_str "$b1" owed_csd)"; life="$(js_str "$b1" lifetime_paid_csd)"
  fi

  : "${addr:=$ADDRESS}"
  : "${h5:=0}"; : "${h1:=0}"; : "${h6:=0}"
  : "${acc:=0}"; : "${rej:=0}"; : "${lastdiff:=0}"
  : "${workers:=0}"; : "${contrib:=0}"
  : "${eph:=0}"; : "${eps:=0}"; : "${sess:=0}"
  : "${pend:=0}"; : "${owed:=0}"; : "${life:=0}"

  # pool vs network hashrate + dominance, from /api/history
  local hist pool_hs net_hs dom
  hist="$(hist_last)"
  pool_hs="${hist%%|*}"; net_hs="${hist##*|}"
  : "${pool_hs:=0}"; : "${net_hs:=0}"
  dom="$(awk -v p="$pool_hs" -v n="$net_hs" 'BEGIN{ n=n+0; if(n<=0){print "0.00"} else printf "%.2f",(p+0)*100/n }')"

  # computed: reject% and an accepted-share/min rate (needs a 2nd sample)
  local total rejpct rate now
  total="$(awk -v a="$acc" -v r="$rej" 'BEGIN{printf "%d",(a+0)+(r+0)}')"
  rejpct="$(awk -v r="$rej" -v t="$total" 'BEGIN{ t=t+0; if(t<=0){print "0.00"} else printf "%.2f",(r+0)*100/t }')"
  now="$(date +%s 2>/dev/null || echo 0)"
  if [ -n "$PREV_GOOD" ] && [ -n "$PREV_TS" ] && [ "$now" -gt "$PREV_TS" ] 2>/dev/null; then
    rate="$(awk -v g="$acc" -v pg="$PREV_GOOD" -v dt="$((now-PREV_TS))" 'BEGIN{ d=g-pg; if(d<0)d=0; printf "%.1f", d*60/dt }')/min"
  else
    rate="—"
  fi
  PREV_GOOD="$acc"; PREV_TS="$now"

  # reject-rate color. Explicit if/elif — a chained `awk && red || awk && yellow`
  # mis-fires (the trailing && also runs after the red branch), painting a
  # critical >5% reject rate yellow instead of red.
  local rej_col="$C_G"
  if awk "BEGIN{exit !($rejpct>5)}"; then rej_col="$C_R"
  elif awk "BEGIN{exit !($rejpct>1)}"; then rej_col="$C_Y"
  fi

  # formatted scalars
  local contribf lastdifff
  contribf="$(awk -v c="$contrib" 'BEGIN{printf "%.2f%%",c+0}')"
  lastdifff="$(awk -v d="$lastdiff" 'BEGIN{printf "%g",d+0}')"

  printf "${C_B}${C_C}  Midstate Pool Miner${C_0}${C_D}   %s${C_0}\n" "$(trunc_addr "$addr")"
  printf "  ${C_D}Pool${C_0}    %-30s ${C_D}Uptime${C_0}  %s\n" "$POOL_API" "$(uptime_fmt "$sess")"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}HASHRATE${C_0}   ${C_D}5m${C_0} %-12s ${C_D}1h${C_0} %-12s ${C_D}6h${C_0} %s\n" "$(hr "$h5")" "$(hr "$h1")" "$(hr "$h6")"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}SHARES${C_0}     ${C_G}acc${C_0} %-9s ${rej_col}rej${C_0} %-7s ${rej_col}reject%%${C_0} %s\n" "$acc" "$rej" "${rejpct}%"
  printf "             ${C_D}rate${C_0} %s\n" "$rate"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}POOL${C_0}       ${C_D}workers${C_0} %-6s ${C_D}contrib${C_0} %-9s ${C_D}diff${C_0} %s\n" "$workers" "$contribf" "$lastdifff"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}EARNINGS${C_0}   ${C_D}est/h${C_0} %-10s ${C_D}est/session${C_0} %s\n" "$eph" "$eps"
  printf "             ${C_D}pending${C_0} %-10s ${C_D}owed${C_0} %-10s ${C_D}paid${C_0} %s\n" "$pend" "$owed" "$life"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}NETWORK${C_0}    ${C_D}pool${C_0} %-12s ${C_D}net${C_0} %-12s ${C_D}dom${C_0} %s\n" "$(hr "$pool_hs")" "$(hr "$net_hs")" "${dom}%"
  [ "$ONCE" = 0 ] && printf "  ${C_D}refresh %ss · q / Ctrl-C quit${C_0}\n" "$REFRESH"
  [ "$_tty" = 1 ] && printf '\033[J'
}

# ---- main loop --------------------------------------------------------------
if [ "$ONCE" = 1 ]; then
  render
  exit 0
fi

while :; do
  render
  # read doubles as the delay AND a 'q' quit listener on a TTY; else plain sleep
  if [ -t 0 ]; then
    if read -t "$REFRESH" -n1 key 2>/dev/null; then
      case "$key" in (q|Q) break ;; esac
    fi
  else
    sleep "$REFRESH"
  fi
done
