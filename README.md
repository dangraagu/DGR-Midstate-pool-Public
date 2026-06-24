# midstate-pool-miner

An open-source **pool miner** for the **Midstate** network — a post-quantum,
proof-of-work blockchain whose PoW is a sequential **BLAKE3 VDF** (each nonce is
a 1,000,000-iteration hash chain). Point it at your Midstate payout address and
it mines to the pool.

It connects to the pool **by default**: there is no server/pool flag to set — the
endpoint is compiled in. The only thing you provide is your payout address.

> **Status: early development.** The consensus core is done and validated — the
> bit-exact PoW (matches the network's golden vectors) and the compiled-in
> pool-lock both pass their tests. The Stratum client, the CPU/CUDA backends, and
> the one-click plug-and-play launcher are **in progress**. There is **no
> prebuilt binary yet** — build from source (below). No release has been cut.

## What this is — and what it isn't

This is **opt-in mining software you run on your own hardware.** Midstate is a
public proof-of-work blockchain; this repository is a pool client for it.

- You choose to download and run it. It mines **only when you start it**, on
  **your own machine**, to **your own payout address** — nothing happens until
  you do.
- It is **not** silent, hidden, or self-spreading. There is no mechanism here to
  install or run it on anyone else's computer, and nothing in this repo accesses
  systems you don't control.
- It is standard cryptocurrency-mining infrastructure (a Stratum pool client) —
  the same category as the miner for any public proof-of-work coin.
- The source is open and auditable.

## Why a pool — and why a CPU is worth it here

Midstate's PoW is a **sequential** BLAKE3 chain, which is deliberately
GPU-resistant: a GPU's massive parallelism can't speed up a single nonce's chain,
so a good CPU is only a few× behind a GPU (not 50× like a normal coin). That makes
**CPU mining genuinely viable** — so this client targets both CPU and NVIDIA GPU,
and a pool lets many independent miners combine hashrate and smooth out variance.

## Build from source

CPU path (no GPU toolchain needed):

```sh
cargo build --release
cargo test                                   # fast unit tests
cargo test --release -- --ignored golden     # bit-exact PoW vs the network golden vectors
```

A GPU (`cuda`) backend and the plug-and-play installer/launcher land in the next
milestones.

## Design (pool-only, by the book)

- **Endpoint-locked.** One pool is compiled in (XOR-obfuscated, `src/endpoint.rs`);
  there is no `--pool`/`--url`/`--host` flag. The miner cannot be repurposed to
  point elsewhere without rebuilding from source.
- **Bit-exact PoW.** `src/pow.rs` is the pure-Rust consensus reference; every
  accelerated backend must reproduce its golden vectors exactly.
- **Brick-safe auto-update (planned).** Launchers will verify each download's
  SHA-256 against the release `SHA256SUMS` and atomically swap, **failing closed**
  (keep the working binary) on any error — never strand a rig.

## License & trademark

Licensed under the **PolyForm Perimeter License 1.0.0** (see [`LICENSE`](LICENSE)
and [`NOTICE`](NOTICE)): the source is public to read and audit, but the license
does **not** permit using it to build or operate a competing product or pool, nor
redistributing or reselling it. Forks must rename and remove the maintainer's
marks — see [`TRADEMARK.md`](TRADEMARK.md).
