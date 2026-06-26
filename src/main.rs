//! `midstate-miner` — pool-only CLI. The endpoint is compiled in (no `--pool`).

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use midstate_miner::client::{run, ClientConfig};
use midstate_miner::mode::{select_mode, Mode, Resolved};
use midstate_miner::{
    cpu_only_thread_budget, cpu_thread_budget, pool_endpoint, Backend, CpuBackend, HybridBackend,
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
    #[arg(long)]
    address: String,
    /// Mining mode: cpu | gpu | hybrid | auto. `auto` (default) discovers the
    /// hardware and runs hybrid (CPU+GPU) if a usable OpenCL GPU is present, else
    /// cpu. `gpu`/`hybrid` error clearly if no GPU is available in this build.
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
    let mut backend = select_backend(requested, physical, logical, cli.cpu_threads)?;

    let cfg = ClientConfig {
        host,
        port,
        address: cli.address,
        share_bits: cli.share_bits,
        reconnect_backoff: Duration::from_secs(5),
        read_timeout: Duration::from_secs(120),
    };
    let dur = (cli.duration != 0).then(|| Duration::from_secs(cli.duration));
    run(cfg, backend.as_mut(), dur)
}

/// Was the `opencl` feature compiled into THIS binary?
const OPENCL_BUILT: bool = cfg!(feature = "opencl");

/// Try to construct an OpenCL GPU backend. Returns `Ok(Some(b))` on a usable
/// device, `Ok(None)` if no device/ICD, `Err` only on a hard build error. Always
/// `Ok(None)` when the feature isn't compiled in (so a CPU-only binary degrades
/// gracefully instead of failing to build).
#[cfg(feature = "opencl")]
fn try_gpu_backend() -> Result<Option<Box<dyn Backend>>> {
    match midstate_miner::opencl_backend::OpenClBackend::try_new() {
        Ok(Some(b)) => Ok(Some(Box::new(b))),
        Ok(None) => Ok(None),
        Err(e) => {
            // A missing ICD loader / driver surfaces here on some boxes. Treat it
            // as "no GPU" so a CPU-only machine running the GPU binary still mines.
            eprintln!("OpenCL init failed: {e}; treating as no-GPU");
            Ok(None)
        }
    }
}
#[cfg(not(feature = "opencl"))]
fn try_gpu_backend() -> Result<Option<Box<dyn Backend>>> {
    Ok(None)
}

/// Auto-discover hardware, resolve the requested [`Mode`] against it, and build
/// the concrete backend(s). Prints what was discovered + the chosen mode.
///
/// Mode handling (see [`select_mode`]):
/// - `cpu`    → CPU backend (all cores; no GPU touched).
/// - `gpu`    → OpenCL backend alone; a clear error if no usable GPU / not built.
/// - `hybrid` → CPU + GPU concurrently; error if no usable GPU / not built.
/// - `auto`   → hybrid if a usable GPU exists, else cpu (never errors).
fn select_backend(
    requested: Mode,
    physical: usize,
    logical: usize,
    cpu_threads: Option<usize>,
) -> Result<Box<dyn Backend>> {
    // --- AUTO-DISCOVER ------------------------------------------------------
    // Probe for a GPU only when the request could USE one (cpu mode never probes,
    // so a forced-CPU run on a GPU-less box does zero OpenCL work). We construct
    // the device once and reuse it for hybrid/gpu so we don't init twice.
    let mut gpu_backend: Option<Box<dyn Backend>> = None;
    if matches!(requested, Mode::Gpu | Mode::Hybrid | Mode::Auto) {
        gpu_backend = try_gpu_backend()?;
    }
    let gpu_present = gpu_backend.is_some();
    let gpu_label = gpu_backend.as_deref().map(|b| b.name().to_string());

    println!(
        "discover: opencl_built={} gpu_present={}{}",
        OPENCL_BUILT,
        gpu_present,
        gpu_label
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default()
    );

    let resolved = select_mode(requested, OPENCL_BUILT, gpu_present);
    match resolved {
        Resolved::Error(msg) => bail!("{msg}"),
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
            Ok(Box::new(b))
        }
        Resolved::Gpu => {
            let gpu = gpu_backend.expect("select_mode returned Gpu without a device");
            println!("mode=gpu | backend: {}", gpu.name());
            Ok(gpu)
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
                return Ok(gpu);
            }
            let cpu = Box::new(CpuBackend::new(threads));
            let h = HybridBackend::new(cpu, gpu, physical);
            println!(
                "mode=hybrid | backend: {} (cpu {} threads, leave-2-free)",
                h.name(),
                threads
            );
            Ok(Box::new(h))
        }
    }
}

fn parse_endpoint(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("malformed endpoint: {s}"))?;
    Ok((h.to_string(), p.parse()?))
}
