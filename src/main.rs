//! `midstate-miner` — pool-only CLI. The endpoint is compiled in (no `--pool`).

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use midstate_miner::client::{capped_duration, run, ClientConfig};
use midstate_miner::mode::{adjust_auto_pinned, select_mode, Mode, Resolved};
use midstate_miner::{
    cpu_fallback_thread_budget, cpu_only_thread_budget, cpu_thread_budget, pool_endpoint, Backend,
    CpuBackend, HybridBackend,
};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "midstate-miner",
    version,
    about = "Open-source pool miner for Midstate (post-quantum BLAKE3 chain). Pool-only."
)]
struct Cli {
    /// Your Midstate payout address (hex) — get it from your Midstate node/wallet.
    /// Required to MINE; optional for pure queries like `--list-gpus`.
    #[arg(long)]
    address: Option<String>,
    /// Mining mode: cpu | gpu | hybrid | auto. `auto` (default) discovers the
    /// hardware and runs hybrid (CPU+GPU) if a usable GPU is present, else cpu.
    /// NEVER-DARK: if `gpu`/`hybrid` can't get a usable GPU, the miner warns
    /// loudly and falls back to a reduced-thread CPU miner so the rig stays
    /// visible to the pool (pass --strict-gpu to make it a fatal error instead).
    #[arg(long, default_value = "auto")]
    mode: Mode,
    /// CPU worker threads (default: physical cores; minus 2 if a GPU also mines).
    #[arg(long)]
    cpu_threads: Option<usize>,
    /// Force the CPU backend even if an OpenCL GPU is present.
    /// Deprecated alias for `--mode cpu` (kept for older launchers/scripts).
    #[arg(long, default_value_t = false)]
    cpu: bool,
    /// Share-difficulty bits to gate at (must match the pool; default 14).
    #[arg(long, default_value_t = 14)]
    share_bits: u32,
    /// Stop after N seconds (0 = run forever).
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Pin mining to a specific GPU by its index (see `--list-gpus`). When set, an
    /// out-of-range or unusable index warns LOUDLY and falls back to a reduced
    /// CPU miner (never-dark) so the rig stays visible to the pool; pass
    /// --strict-gpu to make it exit with an error instead. Used for multi-GPU
    /// rigs (one process per `--gpu-id`). Omit for auto-select.
    #[arg(long)]
    gpu_id: Option<usize>,
    /// List the GPU adapters this binary can see (index, name, type, backend) and
    /// exit. Use the printed index with `--gpu-id`. Requires a `wgpu` build.
    #[arg(long, default_value_t = false)]
    list_gpus: bool,
    /// CUDA only: nonces per `search` window (the GPU "batch"). `search` pipelines
    /// this window across streams to keep the card pegged. Bigger = the GPU stays
    /// busier but the job-epoch is re-checked less often; the default (262144) fills
    /// a 4090-class card. Omit to auto-size. No effect on non-CUDA backends.
    #[arg(long)]
    gpu_batch: Option<u32>,
    /// STRICT GPU: restore the pre-v0.1.9 contract — exit with an error when
    /// `--mode gpu`/`hybrid` or an explicit `--gpu-id` cannot initialize its GPU,
    /// instead of the default loud reduced-thread CPU fallback (never-dark).
    /// Only affects failed GPU requests: plain `--mode auto` (without --gpu-id)
    /// and `--mode cpu` mine on CPU by design regardless of this flag.
    #[arg(long, default_value_t = false)]
    strict_gpu: bool,
}

/// v0.1.9 review fix #1 — never-dark fallback runs are TIME-CAPPED so a
/// transient GPU failure cannot latch the rig into the CPU trickle forever:
/// after this many seconds the process exits cleanly, the launcher restarts it
/// within seconds (its normal liveness loop), and the GPU is re-probed. A
/// permanent failure just cycles visible-fallback → re-probe → visible-fallback.
const FALLBACK_RETRY_SECS: u64 = 1800;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // `--list-gpus` is a pure query: enumerate adapters and exit BEFORE any pool
    // connection or backend construction.
    if cli.list_gpus {
        return list_gpus();
    }

    // From here on we are MINING, which requires a payout address. (It is optional
    // on the CLI only so pure queries like `--list-gpus` can run without one.)
    let address = cli
        .address
        .ok_or_else(|| anyhow!("--address <hex> is required to mine (omit only for --list-gpus)"))?;

    let endpoint = pool_endpoint(); // compiled-in, e.g. midstate.yamaduo.no:3666
    let (host, port) = parse_endpoint(&endpoint)?;
    let physical = num_cpus::get_physical().max(1);
    // FIX 3 — logical (vCPU) count. The CPU-only path budgets off this so a rented
    // box (physical ≈ logical/2) runs all its vCPUs instead of ~half.
    let logical = num_cpus::get().max(physical);

    // The legacy `--cpu` flag is an alias for `--mode cpu` (force CPU). If both are
    // given, `--cpu` wins (it's the more conservative, never-touch-the-GPU choice).
    let requested = if cli.cpu { Mode::Cpu } else { cli.mode };

    println!(
        "midstate-miner | endpoint={endpoint} | logical_cores={logical} physical_cores={physical}"
    );
    let (mut backend, fell_back) = select_backend(
        requested,
        physical,
        logical,
        cli.cpu_threads,
        cli.gpu_id,
        cli.gpu_batch,
        cli.strict_gpu,
    )?;

    let cfg = ClientConfig {
        host,
        port,
        address,
        share_bits: cli.share_bits,
        reconnect_backoff: Duration::from_secs(5),
        read_timeout: Duration::from_secs(120),
    };
    let mut dur = (cli.duration != 0).then(|| Duration::from_secs(cli.duration));
    if fell_back {
        // Review fix #1: cap the fallback run so it cannot latch (see the const).
        dur = capped_duration(dur, Duration::from_secs(FALLBACK_RETRY_SECS));
        println!(
            "never-dark fallback: this process exits after {FALLBACK_RETRY_SECS}s so the \
             launcher restarts it and RE-PROBES the GPU — transient failures self-heal."
        );
    }
    run(cfg, backend.as_mut(), dur)
}

/// Was a GPU backend (`cuda`, `wgpu`, or `opencl`) compiled into THIS binary?
/// Drives mode resolution: `--mode gpu/hybrid` need a GPU build to be satisfiable.
const GPU_BUILT: bool =
    cfg!(feature = "cuda") || cfg!(feature = "wgpu") || cfg!(feature = "opencl");

/// Whether THIS binary can enumerate GPU devices for `--list-gpus` (cuda lists
/// CUDA devices; wgpu lists adapters). An OpenCL-only / CPU-only build cannot.
const LIST_GPUS_BUILT: bool = cfg!(feature = "cuda") || cfg!(feature = "wgpu");

/// `--list-gpus`: enumerate every GPU this binary can see and print one
/// `index: name …` line. CUDA devices first (the native path for these rigs),
/// then wgpu adapters. The index printed for each backend is exactly what
/// `--gpu-id` expects for a binary built with that backend. Prints a clear note
/// if none / not a GPU-enumerable build.
fn list_gpus() -> Result<()> {
    // `mut` is used only when a cuda/wgpu feature is compiled in (those blocks set
    // it); in a CPU-only / opencl-only build both blocks vanish and it stays false.
    #[allow(unused_mut)]
    let mut printed = false;

    #[cfg(feature = "cuda")]
    {
        for line in midstate_miner::cuda_backend::list_devices() {
            println!("{line}");
            printed = true;
        }
    }
    #[cfg(feature = "wgpu")]
    {
        for line in midstate_miner::wgpu_backend::list_adapters() {
            println!("{line}");
            printed = true;
        }
    }

    if !printed {
        if LIST_GPUS_BUILT {
            println!(
                "no GPU devices found. CUDA needs the NVIDIA driver (libcuda); wgpu uses \
                 Vulkan/DX12/Metal/GL — verify with `nvidia-smi` / `vulkaninfo --summary`."
            );
        } else {
            println!(
                "--list-gpus requires a cuda or wgpu build. This binary was built without \
                 either, so it has no GPU-device enumerator."
            );
        }
    }
    Ok(())
}

/// Try to construct a GPU backend. Returns `Ok(Some(b))` on a usable device,
/// `Ok(None)` if no device/driver, `Err` only when `strict` demands a hard fail.
/// Always `Ok(None)` when no GPU feature is compiled in (so a CPU-only binary
/// degrades gracefully instead of failing to build).
///
/// PREFERENCE: when multiple GPU features are compiled in, `cuda` (the native path
/// for these NVIDIA rigs) is tried first, then `wgpu` (Vulkan/DX12/Metal/GL,
/// checkpointed dispatch — no TDR), then `opencl`. Every failure is fail-closed
/// for GPU MINING (never mine on a GPU we couldn't prove bit-exact) — but
/// NEVER-DARK for the process: an init/self-test failure on an explicit
/// `--gpu-id` propagates `Err` (exit) ONLY under `--strict-gpu`; by default it
/// warns loudly and falls through, so `select_mode` lands on the reduced CPU
/// fallback and the rig stays connected + visible to the pool. (Pre-v0.1.9 this
/// exited before ever opening a socket — the launcher then crash-looped an
/// INVISIBLE rig, which is how a broken CUDA build could silently dark a fleet.)
fn try_gpu_backend(
    gpu_id: Option<usize>,
    gpu_batch: Option<u32>,
    strict: bool,
) -> Result<Option<Box<dyn Backend>>> {
    // `gpu_id`/`strict` are consumed only by the cuda/wgpu arms; in a CPU-only /
    // opencl-only build they are unused — acknowledge them (no unused warnings).
    #[cfg(not(any(feature = "cuda", feature = "wgpu")))]
    let _ = (gpu_id, strict);
    // `gpu_batch` is consumed only by the cuda arm; ack it in non-cuda builds.
    #[cfg(not(feature = "cuda"))]
    let _ = gpu_batch;
    #[cfg(feature = "cuda")]
    {
        match midstate_miner::cuda_backend::CudaBackend::try_new(gpu_id, gpu_batch) {
            Ok(Some(b)) => return Ok(Some(Box::new(b))),
            Ok(None) => {} // no usable CUDA device — try wgpu (if built) / opencl / CPU
            Err(e) => {
                // --strict-gpu: an explicit --gpu-id that can't init is fatal, as
                // pre-v0.1.9 (a user error to surface, not paper over).
                if gpu_id.is_some() && strict {
                    return Err(e);
                }
                // NEVER-DARK default: warn loudly and fall through. GPU mining is
                // still fail-closed (we never mine on an unproven GPU) — but the
                // process survives to reach the CPU fallback and stay visible.
                if gpu_id.is_some() {
                    eprintln!("cuda init/self-test FAILED on explicit --gpu-id: {e}");
                    eprintln!(
                        "never-dark: NOT exiting — falling back so this rig stays visible \
                         to the pool (pass --strict-gpu to make this fatal)"
                    );
                } else {
                    // Auto-select: treat as no-GPU, fall through.
                    eprintln!("cuda init/self-test failed: {e}; treating as no-cuda-GPU");
                }
            }
        }
    }
    #[cfg(feature = "wgpu")]
    {
        match midstate_miner::wgpu_backend::WgpuBackend::try_new(gpu_id) {
            Ok(Some(b)) => return Ok(Some(Box::new(b))),
            Ok(None) => {} // no usable wgpu adapter — try opencl (if built) / CPU
            Err(e) => {
                // Same never-dark contract as the cuda arm above.
                if gpu_id.is_some() && strict {
                    return Err(e);
                }
                if gpu_id.is_some() {
                    eprintln!("wgpu init/self-test FAILED on explicit --gpu-id: {e}");
                    eprintln!(
                        "never-dark: NOT exiting — falling back so this rig stays visible \
                         to the pool (pass --strict-gpu to make this fatal)"
                    );
                } else {
                    eprintln!("wgpu init/self-test failed: {e}; treating as no-wgpu-GPU");
                }
            }
        }
    }
    #[cfg(feature = "opencl")]
    {
        match midstate_miner::opencl_backend::OpenClBackend::try_new() {
            Ok(Some(b)) => return Ok(Some(Box::new(b))),
            Ok(None) => {}
            Err(e) => {
                // A missing ICD loader / driver surfaces here on some boxes. Treat
                // it as "no GPU" so a CPU-only machine running the GPU binary mines.
                eprintln!("OpenCL init failed: {e}; treating as no-GPU");
            }
        }
    }
    Ok(None)
}

/// Auto-discover hardware, resolve the requested [`Mode`] against it, and build
/// the concrete backend(s). Prints what was discovered + the chosen mode.
///
/// Mode handling (see [`select_mode`]):
/// - `cpu`    → CPU backend (all cores; no GPU touched).
/// - `gpu`    → GPU backend alone; no usable GPU → LOUD reduced-thread CPU
///   fallback (never-dark), or a clear error under `--strict-gpu`.
/// - `hybrid` → CPU + GPU concurrently; same never-dark fallback rule.
/// - `auto`   → hybrid if a usable GPU exists, else cpu (never errors).
fn select_backend(
    requested: Mode,
    physical: usize,
    logical: usize,
    cpu_threads: Option<usize>,
    gpu_id: Option<usize>,
    gpu_batch: Option<u32>,
    strict_gpu: bool,
) -> Result<(Box<dyn Backend>, bool)> {
    // Returns (backend, fell_back): `fell_back=true` marks the never-dark CPU
    // fallback so main() can time-cap the run (review fix #1 — no latching).
    // --- AUTO-DISCOVER ------------------------------------------------------
    // Probe for a GPU only when the request could USE one (cpu mode never probes,
    // so a forced-CPU run on a GPU-less box does zero GPU work). We construct
    // the device once and reuse it for hybrid/gpu so we don't init twice.
    let mut gpu_backend: Option<Box<dyn Backend>> = None;
    if matches!(requested, Mode::Gpu | Mode::Hybrid | Mode::Auto) {
        gpu_backend = try_gpu_backend(gpu_id, gpu_batch, strict_gpu)?;
    }
    let gpu_present = gpu_backend.is_some();
    let gpu_label = gpu_backend.as_deref().map(|b| b.name().to_string());

    println!(
        "discover: gpu_built={} gpu_present={}{}",
        GPU_BUILT,
        gpu_present,
        gpu_label
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default()
    );

    // Review fix #2: an explicit --gpu-id under default `auto` is still
    // GPU-intent — if the pinned GPU failed, plain auto would resolve to
    // FULL-WIDTH Cpu (all logical cores × one process per card on a broken
    // multi-GPU rig). Demote exactly that cell to the reduced fallback
    // (or Error under --strict-gpu). Healthy pinned paths are untouched.
    let resolved = adjust_auto_pinned(
        select_mode(requested, GPU_BUILT, gpu_present, strict_gpu),
        requested,
        gpu_id.is_some(),
        strict_gpu,
    );
    match resolved {
        Resolved::Error(msg) => bail!("{msg}"),
        Resolved::CpuFallback(reason) => {
            // NEVER-DARK (v0.1.9): a gpu/hybrid request without a usable GPU used
            // to exit BEFORE connecting — the launcher then crash-looped an
            // INVISIBLE rig (indistinguishable from powered-off; how a broken
            // CUDA build could silently dark a whole fleet). Instead: warn LOUDLY
            // on both streams (launcher logs capture stdout; humans tail stderr)
            // and mine on CPU with a REDUCED budget. Reduced because a multi-GPU
            // rig runs one process per card — N full-width CPU miners would fight
            // for the box; min(2, logical) per process keeps every worker
            // connected + submitting without converting the rig to CPU mining.
            eprintln!("WARNING: {reason}");
            eprintln!(
                "WARNING: never-dark fallback — mining on CPU (reduced threads) so this rig \
                 stays VISIBLE to the pool. Pass --strict-gpu to make this a fatal error, \
                 or --mode cpu for a full-width CPU miner."
            );
            println!("WARNING: {reason} → never-dark CPU fallback (reduced threads)");
            if cpu_threads == Some(0) {
                // Review fix: never-dark is absolute — an explicit 0 is floored
                // to 1 by the budget below (a 0-thread fallback would exit before
                // connecting = the invisible crash-loop again).
                eprintln!(
                    "WARNING: explicit --cpu-threads 0 floored to 1 under the never-dark \
                     fallback (use --strict-gpu to forbid fallback CPU mining entirely)"
                );
            }
            let threads = cpu_fallback_thread_budget(logical, cpu_threads);
            if threads == 0 {
                bail!("0 CPU threads after fallback budget — nothing to mine");
            }
            let b = CpuBackend::new(threads);
            println!(
                "mode=cpu-FALLBACK | backend: {} ({} threads of {} logical; reduced \
                 never-dark budget)",
                b.name(),
                threads,
                logical
            );
            Ok((Box::new(b), true))
        }
        Resolved::Cpu => {
            // FIX 3 — No GPU mining → budget off LOGICAL cores (all vCPUs), and
            // honor a --cpu-threads override UP TO logical (it may exceed physical).
            let threads = cpu_only_thread_budget(logical, cpu_threads);
            if threads == 0 {
                bail!("0 CPU threads after budget — nothing to mine");
            }
            let b = CpuBackend::new(threads);
            println!(
                "mode=cpu | backend: {} ({} threads of {} logical / {} physical)",
                b.name(),
                threads,
                logical,
                physical
            );
            Ok((Box::new(b), false))
        }
        Resolved::Gpu => {
            let gpu = gpu_backend.expect("select_mode returned Gpu without a device");
            println!("mode=gpu | backend: {}", gpu.name());
            Ok((gpu, false))
        }
        Resolved::Hybrid => {
            let gpu = gpu_backend.expect("select_mode returned Hybrid without a device");
            // CPU half runs with gpu_active=true → the inviolable "leave 2 free".
            let threads = cpu_thread_budget(physical, true, cpu_threads);
            if threads == 0 {
                // Too few cores to both leave 2 free AND mine on CPU → run GPU-only
                // rather than refusing (the GPU is ~all of the hashrate anyway).
                println!(
                    "mode=hybrid requested but only {physical} physical core(s): \
                     can't leave 2 free AND mine on CPU → running GPU-only."
                );
                println!("backend: {}", gpu.name());
                return Ok((gpu, false));
            }
            let cpu = Box::new(CpuBackend::new(threads));
            let h = HybridBackend::new(cpu, gpu, physical);
            println!(
                "mode=hybrid | backend: {} (cpu {} threads, leave-2-free)",
                h.name(),
                threads
            );
            Ok((Box::new(h), false))
        }
    }
}

fn parse_endpoint(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("malformed endpoint: {s}"))?;
    Ok((h.to_string(), p.parse()?))
}
