# Changelog

All notable changes to **midstate-pool-miner** are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.9] - 2026-07-01

### Fixed

- **NEVER-DARK: a broken GPU backend can no longer make a rig invisible.**
  Previously, `--mode gpu`/`--mode hybrid` or an explicit `--gpu-id` whose GPU
  failed to initialize (driver rejects the PTX, CUDA init error, failed bit-exact
  self-test) made the process exit **before ever connecting to the pool** — the
  launcher then crash-looped it forever, and the rig was indistinguishable from
  powered-off. This is exactly the launch configuration `mine-auto.sh` /
  `mine-multi-gpu.sh` use on multi-GPU rigs (one process per card with
  `--gpu-id`), so one bad update could silently dark a whole fleet. Now the miner
  warns LOUDLY on both stdout and stderr and falls back to a reduced CPU miner
  (`min(2, logical)` threads per process — a visibility trickle, not a CPU
  takeover; an explicit `--cpu-threads` overrides it), so every worker stays
  connected, submitting, and visible in the pool's per-address stats. GPU mining
  itself remains fail-closed: a GPU that cannot prove itself bit-exact never
  mines. The new `--strict-gpu` flag restores the old exit-with-error contract
  for rigs where CPU mining must never happen.
- **Wider CUDA driver compatibility.** The committed kernel PTX `.version` pin
  dropped 7.8 → 6.5 (CUDA 10.2 ISA), lowering the minimum NVIDIA driver from
  ~r520 to ~r440 — covering older-driver GPU containers that would previously
  fail with `CUDA_ERROR_UNSUPPORTED_PTX_VERSION` (and, pre-never-dark, go
  invisible). `.target sm_75` is unchanged; the kernel uses only long-stable
  integer ops. Validated bit-exact on real hardware (RTX 5070 Ti, 5/5 golden
  vectors, two runs).

- **Never-dark details (adversarial-review hardening).** (a) Fallback runs are
  TIME-CAPPED at 30 minutes: the process exits cleanly, the launcher restarts
  it when it notices the worker down (its normal liveness loop), and the GPU is
  re-probed — so a *transient* GPU failure at boot cannot latch a rig into the
  CPU trickle forever (a permanent failure just cycles visible-fallback →
  re-probe). A shorter user `--duration` still wins. To make this true on
  multi-GPU rigs, `mine-auto.sh`'s liveness check is now ALL-workers-alive
  (was ANY-alive): one dead worker — a crash or the TTL re-probe exit —
  bounces the whole set, so a single broken card can no longer leave its
  worker permanently un-respawned while siblings mine.
  (b) An explicit `--gpu-id` under the default `--mode auto` is treated as
  GPU-intent: if the pinned GPU fails, the rig gets the *reduced* fallback (or
  an error under `--strict-gpu`), never a full-width all-cores CPU miner — one
  process per card on a broken multi-GPU rig stays a bounded trickle. Healthy
  pinned rigs are bit-identical to v0.1.8. (c) `--cpu-threads 0` is floored to
  1 inside the fallback (a 0-thread fallback would exit before connecting —
  the invisible crash-loop again); `--strict-gpu` is the supported way to
  forbid fallback CPU mining entirely. Note: an explicit `--cpu-threads N`
  (e.g. `mine-multi-gpu.sh` worker g0) still overrides the trickle by design.

### Added

- **Heartbeat hashrate.** The 30-second `[miner] hb:` line now reports
  `backend=<name> hs=<H/s>` (windowed rate since the previous heartbeat), and
  the session `FINAL:` line reports `hs_avg=`. While a job is being mined, a
  degraded rig shows `hs=0` (or a CPU-fallback `backend=`) right in its own
  launcher log; a rig with no job prints no heartbeat at all — heartbeat
  *absence* is the waiting/hung signal.
- **Keepalive — no more idle-disconnect flapping.** The miner now sends a
  `mining.subscribe` keepalive every 30 s (the pool acks it and, crucially, its
  120 s idle read-timeout is reset). Slow submitters — CPU rigs (a share every
  4+ minutes), the never-dark reduced fallback, or a miner waiting while the
  pool gates jobs during a node re-sync — previously flapped
  connect→120s→drop→reconnect forever. Proven live: a 150 s zero-submit run
  held one unbroken connection. (A single GPU search window longer than 120 s
  can still outlive the timer mid-window; fully addressed by the streamed
  search planned for v0.1.10.)
- **`stale_dropped=` counter** in the heartbeat and `FINAL:` lines: found
  shares discarded because the job rolled before they could be submitted
  (whole-window drops + mid-submit remainders). This makes the stale-window
  share leak measurable per rig — the prerequisite for validating the v0.1.10
  streamed-search fix and interim `--gpu-batch` tuning on slow cards.
- **`TCP_NODELAY` on the miner socket** (best-effort): submits are tiny
  one-line writes; Nagle coalescing added RTT-scale latency exactly when a
  share — or a block-winning share — should be on the wire immediately.

## [0.1.8] - 2026-06-29

### Fixed

- **mine-auto.bat fork bomb (P0).** The Windows launcher's liveness check used
  `tasklist`'s default TABLE format, which truncates the Image Name column to 25
  chars — so the 27-char `midstate-miner-gpu-cuda.exe` (the NVIDIA/CUDA default)
  displayed as `midstate-miner-gpu-cuda.e` and `find` for the full name never
  matched. The launcher concluded the miner was dead every tick and spawned
  another, accumulating processes without bound. Fixed by using `/FO CSV` (full,
  un-truncated name) for the liveness check, plus an idempotent `taskkill /IM
  %EXE%` before each spawn so a misfiring check can never accumulate processes.
  Only the CUDA variant (27-char name) was affected; CPU/OpenCL names fit. The
  Linux/HiveOS `mine-auto.sh` tracks PIDs directly and was never affected.

## [0.1.7] - 2026-06-29

### Added

- **`midstate-dashboard.sh` / `midstate-dashboard.bat`** — a read-only terminal
  dashboard that polls the public pool per-address API (works with the v0.1.6
  fleet; opt-in). No miner/consensus change.

## [0.1.6] - 2026-06-29

### Added

- **Windows CUDA build** (`midstate-miner-gpu-cuda.exe`) so Windows NVIDIA rigs
  use the native CUDA backend instead of the slower OpenCL/wgpu fallback. The
  launchers detect an NVIDIA card and default to CUDA on Windows the same way they
  already do on Linux.
- **CUDA-default-on-NVIDIA + one-process-per-GPU auto-spawn** in
  `mine-auto.sh`/`mine-auto.bat` — on a multi-GPU NVIDIA box the launcher spawns
  one CUDA process per GPU (`--gpu-id`) automatically.

### Changed

- **2-stream pipeline plumbing refactor** in the CUDA backend — the search loop now
  carries its sub-waves across two CUDA streams with ping-pong count/result buffers
  so host readback + CPU re-verify of one wave overlaps the next wave's kernel,
  freeing a host core. This is a **non-regression** pipeline change, **not** a
  hashrate increase: the existing CUDA kernel was already GPU-bound, so per-GPU
  hashrate is unchanged. **No consensus / PoW / kernel change** — the committed
  `midstate.cu` / `midstate.ptx` are byte-identical and the boot self-test still
  byte-compares against the CPU reference, fail-closed.

## [0.1.5] - 2026-06-26

### Added

- **CUDA GPU backend** (`--features cuda`) for NVIDIA CUDA compute-container rigs where Vulkan/OpenCL are unavailable (driver-only). Ported from the OpenCL kernel; bit-exact (golden vectors validated on real hardware), boot self-test fail-closes to CPU, every nonce CPU-re-verified. cudarc dynamic-loading + a committed version-pinned (PTX 7.8) kernel = no CUDA toolkit to build or run. New asset `midstate-miner-linux-gpu-cuda`. Backend preference: cuda > wgpu > opencl > cpu, each fail-closed.

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

## [0.1.3] - 2026-06-26

### Fixed

- **Launcher `--mode`** — the first build arg now also drives the run mode, so
  `mine-auto.sh cpu` runs `--mode cpu` (previously it left `--mode` at the default
  `auto`; with the CPU build that still mined CPU-only, but the mode string was
  unclear). `gpu` maps to `--mode hybrid`. An explicit `MODE=…` env still wins.

## [0.1.2] - 2026-06-26

CPU throughput fix — multi-core and multi-rig fleets now land their full hashrate
instead of colliding and wasting work.

### Fixed

- **Fleet nonce collision** — every rig previously started (and reset on each new
  job) its nonce search at 0, so a whole fleet ground identical nonces and all but
  the first submitter got "Duplicate share." Each rig now seeds a per-instance
  random nonce base (OS entropy) and advances continuously, never resetting to 0.
- **Job-roll waste** — the per-search window was `threads × 128` nonces, which at
  high thread counts blocked for seconds and never finished before the pool rolled
  the job (restarting at the low nonces). The window is now `threads × 4`
  (sub-second), so the loop stays responsive to new jobs.
- **CPU thread cap** — CPU-only mining was clamped to *physical* cores, so a rented
  vCPU box ran ~half its threads. CPU-only now uses *logical* cores and honors a
  `--cpu-threads` override above physical. GPU/hybrid keep the physical-minus-2 rule.

### Changed

- **Default `--share-bits` is now 14** (was 20) to match the pool's share
  difficulty, so the launcher and bare runs gate correctly without an explicit flag.

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

[0.1.6]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.6
[0.1.5]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.5
[0.1.4]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.4
[0.1.3]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.3
[0.1.2]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.2
[0.1.1]: https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/tag/v0.1.1
