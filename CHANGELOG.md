# Changelog

All notable changes to **midstate-pool-miner** are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.4] - 2026-06-26

### Added

- **wgpu GPU backend (`--features wgpu`)** — a cross-platform GPU backend on
  Vulkan / DX12 / Metal (no CUDA or OpenCL toolkit required). Its WGSL kernel is
  **bit-exact** to the CPU reference: every GPU-surfaced candidate nonce is
  **re-verified on the CPU** before it can become a share, and the backend runs a
  full 1,000,000-iteration self-test at boot that **fail-closes to CPU** on any
  mismatch — a buggy or non-deterministic driver can only cost throughput, never
  produce an invalid share. The dispatch loop is checkpointed (~2,000 iters per
  dispatch) so it does not trip the OS GPU watchdog (TDR).
- **Multi-GPU support** — `--gpu-id <index>` pins a process to one GPU and
  `--list-gpus` prints the visible adapters (index, name, type, backend). An
  explicit `--gpu-id` fails LOUDLY on a bad/unusable index instead of silently
  falling back to CPU. The new `mine-multi-gpu.sh` launcher runs one process per
  GPU (first = hybrid CPU+GPU, the rest GPU-only) with per-GPU logs and a
  liveness/backoff restart loop.

### Changed

- **`-gpu` release binaries are now built with `--features wgpu`** (Linux and
  macOS) instead of `--features opencl` — wgpu is cross-platform and needs no
  OpenCL toolkit, and the checkpointed dispatch avoids the desktop GPU watchdog.
  The CPU builds are unchanged.

## [0.1.1] - 2026-06-25

First public release — prebuilt, SHA-256-verified binaries for Windows, Linux,
and macOS via GitHub Releases, plus the one-click installer and self-updating
launcher.

### Added

- **Run modes** — `--mode cpu|gpu|hybrid|auto` (default `auto`). `auto` uses a
  GPU when one is present and **degrades gracefully to CPU** when it isn't, so the
  GPU binary never crashes on a GPU-less box.
- **OpenCL GPU backend + hybrid mode** — an OpenCL GPU backend (covers NVIDIA,
  AMD, and Intel) and a hybrid backend that runs CPU and GPU concurrently in one
  process. Built with `--features opencl`; shipped as the per-OS `…-gpu` release
  binary.
- **Working CPU Stratum miner** — the CPU backend mines to the pool over Stratum,
  computing each nonce's real 1,000,000-iteration BLAKE3 VDF and gating shares
  locally against `--share-bits`. CPU is genuinely competitive because the PoW is
  a sequential chain.
- **Compiled-in endpoint lock** — the pool endpoint is baked into the binary
  (XOR-obfuscated, `src/endpoint.rs`); there is no `--pool`/`--url`/`--host`
  override. Pool-only by design; you provide only your payout address.
- **Brick-safe self-updating launchers** — `install-midstate-miner.{bat,sh}` and
  `mine-auto.{bat,sh}`:
  - download the prebuilt release binary for the host OS and **verify it against
    the release `SHA256SUMS`** using the OS hashing tool (`sha256sum`/`shasum`,
    or `Get-FileHash` on Windows);
  - **fail closed** — a missing checksums file, an unlisted asset, no available
    verifier, or a hash mismatch refuses the download and keeps the working
    binary; a fresh install aborts before anything runs;
  - update via a **temp-path download → verify → atomic swap**, never writing an
    unverified binary onto the live path, and restart the miner — a failed swap
    brings the existing binary back up rather than stranding the rig idle;
  - resolve "latest" from a CDN-served `latest-version.txt` (not the rate-limited
    GitHub API, which would freeze whole farms behind one public IP);
  - can refresh **themselves** the same way (download → SHA-verify → staged atomic
    swap applied on the next start), so launcher-side fixes reach the fleet.

### Notes

- Licensed under the **PolyForm Perimeter License 1.0.0** (pool-only; no competing
  use or redistribution). See `LICENSE`, `NOTICE`, and `TRADEMARK.md`.
- Early-development software — treat mining as experimental. GPU bit-exactness has
  been validated against the golden vectors on POCL; if you hit a GPU-specific
  reject pattern, fall back to `--mode cpu` and report it.

[0.1.4]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.4
[0.1.1]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.1
