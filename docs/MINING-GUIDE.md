# Midstate Pool Mining — How-To Guide

A friendly, copy-pasteable walkthrough for mining on the Midstate pool with the
open-source `midstate-miner`.

> **Honest status (early development):** There is **no prebuilt binary and no
> published release yet.** The one-click installer and the auto-updating
> launcher exist in the repo but are **inert today** — they try to download a
> release that doesn't exist. The **only working way to mine right now is to
> build from source** (`cargo build --release`) and run the binary with your
> payout address. This guide tells you exactly how, and is careful not to
> promise anything that isn't actually wired up yet.

---

## 1. TL;DR — start mining today

You need the Rust toolchain (stable) and a Midstate payout address. Then:

```sh
# 1. Get the source (clone the repo, or use your existing checkout)
cd DGR-Midstate-pool-Public

# 2. Build the CPU miner (release mode — no GPU toolchain needed)
cargo build --release

# 3. Run it against your payout address
#    Windows:
target\release\midstate-miner.exe --address YOUR_MIDSTATE_ADDRESS
#    Linux/macOS:
./target/release/midstate-miner --address YOUR_MIDSTATE_ADDRESS
```

That's it. `--address` is the only required flag. The pool endpoint is compiled
into the binary — there is nothing else to configure.

A healthy start looks like this:

```
midstate-miner | endpoint=midstate.yamaduo.no:3666 | physical_cores=8
backend: cpu:x8 (8 threads)
[miner] pool=midstate.yamaduo.no:3666 backend=cpu:x8 addr=abcdef0123456789... share_bits=20
[miner] connected to midstate.yamaduo.no:3666
[miner] authorize: true
[miner] job 1a2b3c midstate=00112233aabb…
[miner] hb: submitted=3 accepted=3 rejected=0
```

If you see `authorize: true` and the `accepted=` counter climbing, you're mining.

---

## 2. Requirements

| What | Why |
|---|---|
| **Rust toolchain (stable)** | To build from source. Install via [rustup](https://rustup.rs). Edition 2021; no pinned minimum version — current stable is fine. |
| **An OS that Rust supports** | Windows, Linux, or macOS. The default build is CPU-only and needs **no GPU toolkit**. |
| **A Midstate payout address** | Where your rewards go. See section 3 — the miner does **not** create one for you. |
| **Internet access to the pool** | The miner makes an **outbound** TCP connection to the compiled-in pool endpoint (`midstate.yamaduo.no:3666`). |
| **Internet to fetch crates (build time)** | Cargo downloads dependencies the first time you build. |

You do **not** need: a GPU, a CUDA toolkit, or any pool/server configuration —
the endpoint is baked into the binary.

---

## 3. Get a Midstate payout address

**The miner cannot generate an address, and never will.** Midstate addresses
are post-quantum, derived from a **stateful** Merkle-Signature-Scheme (MSS/WOTS)
keypair — each signing leaf may be used only once, and reuse is catastrophic. So
the key material must live in the **wallet that spends it**, never on a mining
rig. The miner only takes an address *string* and sends shares to the pool under
it.

A Midstate address is a long hex value (an MSS address — **not** a 40-hex /
`0x…` Ethereum-style address). The miner does **not** validate its length or
format; if you paste the wrong thing, the only symptom is the pool rejecting
your authorization (see Troubleshooting).

**How you're meant to get one:**

1. Run your Midstate node/wallet.
2. Ask it for a new receiving address (e.g. the wallet's "new address" /
   `getnewaddress` command).
3. Copy the resulting hex string **exactly** and use it as `--address`.

> **Honest gap:** The Midstate node/wallet is **not part of this repository**,
> and there is currently **no in-repo `WALLET.md` or wallet documentation**
> (the config example references a `WALLET.md` that does not exist here yet).
> That means this repo alone does not give a brand-new user a working,
> step-by-step way to obtain an address. You need access to a Midstate
> node/wallet from elsewhere. Wallet and key generation are **intentionally out
> of scope** for the miner — by design, so your single-use signing keys never
> touch a mining rig. If you don't yet have a node/wallet, ask in the community
> (section 12) before you start.

---

## 4. Build (there's no prebuilt binary yet)

There is no published release and nothing to download. Build it yourself:

```sh
# CPU-only release build (the normal path — no GPU toolchain required)
cargo build --release
```

The binary lands at:

- **Windows:** `target\release\midstate-miner.exe`
- **Linux/macOS:** `target/release/midstate-miner`

Optional sanity checks (recommended, not required):

```sh
cargo test                                   # fast unit tests
cargo test --release -- --ignored golden     # bit-exact PoW vs network golden vectors
```

> The package is `publish = false`, so you **cannot** `cargo install` it from
> crates.io — building from this source tree is the only way.

---

## 5. Run

Minimal command (only `--address` is required):

```sh
# Windows
target\release\midstate-miner.exe --address YOUR_MIDSTATE_ADDRESS

# Linux/macOS
./target/release/midstate-miner --address YOUR_MIDSTATE_ADDRESS
```

From source without building first:

```sh
cargo run --release -- --address YOUR_MIDSTATE_ADDRESS
```

### Flags

These are **all** the flags the current binary accepts. Every flag is
`--long` form (there are no short aliases except clap's built-in `-h`/`-V`).

| Flag | Default | What it does |
|---|---|---|
| `--address <ADDRESS>` | *(required — no default)* | Your Midstate payout address (hex). Get it from your Midstate node/wallet. The miner does not generate or validate it. |
| `--cpu-threads <N>` | *(unset → all physical cores)* | Number of CPU worker threads. Clamped to a budget ceiling: you can request **fewer**, never **more**, than the available cores. |
| `--cpu` | `false` | Force the CPU backend even if an OpenCL GPU is present. Only meaningful if you built with `--features opencl` (see section 7); does nothing on a default CPU build. |
| `--share-bits <N>` | `20` | Share-difficulty bits to gate at locally. **Must match the pool** — the pool never sends the target over the wire, so the miner computes it from this value. Leave at the default unless told otherwise. |
| `--duration <SECS>` | `0` | Stop after N seconds. `0` = run forever. |
| `--help` / `-h` | *(built-in)* | Print help. |
| `--version` / `-V` | *(built-in)* | Print the version. |

> **Note on the "minus 2" rule:** the help text for `--cpu-threads` mentions
> "minus 2 if a GPU also mines." That reservation is implemented and tested, but
> in **today's** binary it does **not** fire — a default CPU run uses **all**
> physical cores. The minus-2 behaviour only activates in the future GPU+CPU
> hybrid (not wired up yet). See section 7.

### Copy-paste example

```sh
# Run forever on all cores, default share difficulty (Windows)
target\release\midstate-miner.exe --address abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789

# Linux: use 6 threads and stop after 1 hour (for a quick test)
./target/release/midstate-miner --address abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789 --cpu-threads 6 --duration 3600
```

*(The address above is a placeholder — replace it with your real one.)*

---

## 6. "Is it working?"

The miner prints plain-text status lines prefixed with `[miner]`. Here are the
**verbatim** lines you'll see and what each means.

### Lines you'll see

```
midstate-miner | endpoint=midstate.yamaduo.no:3666 | physical_cores=<N>
backend: cpu:x<N> (<N> threads)
[miner] pool={host}:{port} backend={name} addr={first 16 chars of your address}... share_bits={bits}
[miner] connected to {host}:{port}
[miner] authorize: true
[miner] job {job_id} midstate={first 6 bytes hex}…
[miner] hb: submitted={n} accepted={n} rejected={n}
[miner] FINAL: submitted={n} accepted={n} rejected={n}
[miner] duration reached, stopping
```

And, in problem situations, these (mostly on stderr):

```
[miner] authorize REJECTED — check your address
[miner] submit rejected: {error}
[miner] read timeout — link stalled, dropping
[miner] session ended: {error}; reconnecting in 5s
no OpenCL GPU found; using CPU      (only on an --features opencl build)
```

### What a healthy run looks like

1. `connected to …` — TCP link to the pool is up.
2. `authorize: true` — the pool accepted your address.
3. A stream of `job … midstate=…` lines — the pool is feeding you work.
4. Every 30 seconds, a heartbeat: `hb: submitted=… accepted=… rejected=…`.
   **`accepted` climbing = the pool is taking your shares. That's success.**

### Working vs idle vs broken

- **Working:** `connected` → `authorize: true` → `job …` lines → heartbeat with
  `accepted=` rising.
- **Idle / waiting for work:** After `authorize: true`, if the pool has no job
  for you yet, the miner prints **nothing new** — the heartbeat only ticks while
  a job is actually being mined. **A connected-but-no-jobs miner going quiet is
  normal, not a hang.** New `job …` lines will appear once the pool has work.
- **Broken:** `authorize REJECTED` (bad address), `read timeout — link stalled`
  (pool went silent >120s), or a repeating `session ended … reconnecting in 5s`
  loop (can't connect/handshake). See Troubleshooting.

When the miner stops (via `--duration` or Ctrl+C), it prints a final tally:
`[miner] FINAL: submitted=… accepted=… rejected=…`.

---

## 7. CPU and GPU

### Why a CPU is viable here

Midstate's proof-of-work is a **sequential BLAKE3 VDF** — a chain of 1,000,000
BLAKE3 iterations that must be computed in order, one after another. Because the
work is inherently sequential, a GPU's massive parallelism gives a smaller
advantage than on a classic parallel hash. A CPU is "only a few times" behind a
GPU here, so **CPU mining is genuinely worthwhile**, not a token gesture.

The miner only submits a nonce after computing its **real** 1,000,000-iteration
final hash and checking it locally against the share target derived from
`--share-bits`. If your `--share-bits` doesn't match the pool, your valid-looking
submits get silently dropped — so leave it at the default unless the pool tells
you otherwise.

### Thread budget

By default the miner uses **all** your physical cores. `--cpu-threads N` lets you
dial that **down** (it's clamped to the core count — you can ask for fewer, never
more). The "leave 2 cores free when a GPU is also mining" rule exists in the code
and is unit-tested, but it is **not active in today's binary** (the live path
always runs with no GPU, so it uses every core). It will matter only once the
GPU+CPU hybrid ships.

### GPU — opt-in and in progress (not ready today)

Be clear-eyed about GPU status:

- **The default build is CPU-only.** GPU support is **opt-in at build time**, and
  there is **no released GPU binary of any kind.**
- **OpenCL** is the only GPU backend that has code today. To try it you must
  build it yourself with `cargo build --release --features opencl` **and** have a
  working OpenCL driver/ICD installed. If present, the miner auto-selects the GPU;
  `--cpu` forces CPU instead. Note: OpenCL bit-exactness has so far been verified
  only on POCL (a CPU OpenCL implementation), not validated on real GPU hardware,
  so treat it as experimental.
- **CUDA / NVIDIA** is **not available.** There is no `cuda` feature in the build
  config, the CUDA backend is unimplemented, and the project describes it as a
  "next milestone." Anything you see referring to NVIDIA builds is forward-looking.

**Bottom line: today, mine on CPU.** GPU is coming later.

---

## 8. Network & firewall

- The miner makes a single **outbound** TCP connection to the compiled-in pool
  endpoint: **`midstate.yamaduo.no:3666`**. No inbound ports are opened; you do
  not need to forward anything.
- If your firewall blocks outbound connections, allow `midstate-miner` to reach
  that host/port. A blocked or unreachable pool shows up as a repeating
  `session ended … reconnecting in 5s` loop.
- **Pool-only by design.** There is **no** `--pool` / `--url` / `--host` flag.
  The endpoint is compiled into the binary (stored XOR-obfuscated and decoded at
  runtime — that's not a secret, just a tamper-deterrent). You cannot point this
  miner at a different pool without editing `src/endpoint.rs` and rebuilding from
  source — and the license (section 11) forbids repurposing it to a competing
  pool anyway.

---

## 9. The one-click launcher (`mine-auto`) — accurate status

The repo ships `install-midstate-miner.bat` / `.sh` (installers) and
`mine-auto.bat` / `.sh` (a self-updating supervisor that restarts dead miners and
checks for new versions with SHA-256 verification). They're designed to be the
eventual plug-and-play path.

**They do not work yet, and you should not rely on them today.** Here's why,
plainly:

- Every download they perform points at this repo's GitHub **Releases**
  (`releases/latest/download/…`). **No release has been published** — no tags
  exist — so those URLs 404.
- The installer will print **`[X] Download failed. Either no release is
  published yet…`** and stop. The auto-updater finds no version file and simply
  no-ops (it keeps quiet rather than breaking anything).
- The launcher scripts are also written **ahead of the binary**: they invoke
  flags and subcommands (`--device`, `--gpu-id`, `--log-dir`, `check-update`,
  `verify-file`) that the **current** binary does not have. Running today's
  freshly-built binary through them would fail on unknown arguments.

**So: skip the launcher for now and use the build-from-source + `--address`
path from sections 4–5.** Once a signed release is published, the installer and
auto-updater become the easy path (download → verify SHA-256 → run → keep
itself updated). Until then, they're inert by design.

---

## 10. Troubleshooting

| Symptom (the line you see) | Likely cause | Fix |
|---|---|---|
| `[miner] authorize: false` + `[miner] authorize REJECTED — check your address` | Bad, garbled, or wrong-format payout address. | Re-copy your address from your Midstate node/wallet **exactly** and pass it to `--address`. The session ends on rejection, so fix and restart. |
| `[miner] read timeout — link stalled, dropping` | The pool went silent for more than 120 seconds (network blip or pool-side pause). | Usually self-heals — the miner drops the session and reconnects automatically. If it persists, check your connection to `midstate.yamaduo.no:3666`. |
| `[miner] session ended: … ; reconnecting in 5s` (repeating) | Can't connect or complete the handshake — firewall, DNS, pool down, or no internet. | Verify outbound access to `midstate.yamaduo.no:3666` (section 8). The miner retries forever every 5s, so it'll recover once the path is back. |
| `[miner] submit rejected: …` (occasional) | A share arrived stale (the job rolled) or didn't meet the target. | Occasional rejects are normal. A **flood** of rejects usually means `--share-bits` doesn't match the pool — leave it at the default `20` unless told otherwise. |
| Quiet after `authorize: true` (no jobs, no heartbeat) | The pool simply has no work for you yet. | **Not a bug.** The heartbeat only ticks while mining a job. Wait — `job …` lines appear when the pool sends work. |
| `0 CPU threads after budget — nothing to mine` | You passed `--cpu-threads 0` (or the budget resolved to zero). | Give it at least 1 thread, or omit `--cpu-threads` to use all cores. |
| "Slow hashrate" / feels slow | It's a sequential 1,000,000-iteration VDF on CPU — this is expected (section 7). | Use more threads (default already uses all cores), or wait for the GPU backend. Don't expect GPU-class throughput from a CPU. |
| Unknown-argument errors when starting | You ran the binary through `mine-auto` or copied launcher flags. | Use the bare `--address` invocation (section 5). The launcher's `--device`/`--gpu-id`/`--log-dir` flags don't exist in today's binary. |

---

## 11. FAQ + trust note

**Is this open source?**
The source is public to read and audit, under the **PolyForm Perimeter License
1.0.0** (plus trademark restrictions). That means: you may read and audit it, but
you may **not** use it to build or operate a competing product/pool, and you may
**not** redistribute or resell it. Forks must rename and remove maintainer marks.
It is **not** an MIT-style "do anything" license.

**Does it run on its own / phone home?**
No. It runs **only when you start it**, on **your** machine, mining to **your**
address. Its single network connection is the outbound link to the pool.

**Can it point at another pool or steal my coins?**
No. There's no pool/host override — the endpoint is compiled in (section 8). The
miner never holds your keys: it only takes an address string and submits shares.
Your single-use MSS signing keys stay in your wallet, never on the rig
(section 3).

**Will I earn X / what's it worth?**
This guide makes **no** earnings or price claims. Midstate is in early
development; treat mining as experimental.

**Why can a CPU compete?**
The PoW is a sequential VDF, which limits the advantage of parallel hardware
(section 7).

**Where's the GPU/NVIDIA version?**
Not ready. CPU is the path today (sections 7 and 9).

---

## 12. More info & community

- **Project README:** [`../README.md`](../README.md) — status, design notes, and
  build instructions from the maintainers.
- **License:** [`../LICENSE`](../LICENSE) and [`../TRADEMARK.md`](../TRADEMARK.md).
- **Roadmap:** [`docs/TODO.md`](./TODO.md).
- **Community / Discord:** a community invite link will be published in the
  project README once it's available — check there to ask questions, get help
  finding a wallet/node, and hear about releases.

---

*Early-development software. Build from source, mine on CPU, and watch the
`accepted=` counter climb. Have fun, and be honest with yourself about what's
ready and what isn't.*
