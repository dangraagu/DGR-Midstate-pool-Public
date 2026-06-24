//! `midstate-miner` — pool-only CLI. The endpoint is compiled in (no `--pool`).

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use midstate_miner::client::{run, ClientConfig};
use midstate_miner::{cpu_thread_budget, pool_endpoint, Backend, CpuBackend};
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
    /// CPU worker threads (default: physical cores; minus 2 if a GPU also mines).
    #[arg(long)]
    cpu_threads: Option<usize>,
    /// Force the CPU backend even if an OpenCL GPU is present.
    #[arg(long, default_value_t = false)]
    cpu: bool,
    /// Share-difficulty bits to gate at (must match the pool; default 20).
    #[arg(long, default_value_t = 20)]
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

    println!("midstate-miner | endpoint={endpoint} | physical_cores={physical}");
    let mut backend = select_backend(&cli, physical)?;

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

/// Pick a backend: an OpenCL GPU if present + the `opencl` feature is built and
/// `--cpu` was not passed, otherwise the CPU backend.
fn select_backend(cli: &Cli, physical: usize) -> Result<Box<dyn Backend>> {
    if !cli.cpu {
        #[cfg(feature = "opencl")]
        {
            match midstate_miner::opencl_backend::OpenClBackend::try_new() {
                Ok(Some(b)) => {
                    println!("backend: {}", b.name());
                    return Ok(Box::new(b));
                }
                Ok(None) => println!("no OpenCL GPU found; using CPU"),
                Err(e) => eprintln!("OpenCL init failed: {e}; using CPU"),
            }
        }
    }
    // Single CPU backend → gpu_active=false (uses all cores). The "leave 2 free"
    // rule activates in the future GPU+CPU hybrid (see cpu_thread_budget tests).
    let threads = cpu_thread_budget(physical, false, cli.cpu_threads);
    if threads == 0 {
        bail!("0 CPU threads after budget — nothing to mine");
    }
    let b = CpuBackend::new(threads);
    println!("backend: {} ({} threads)", b.name(), threads);
    Ok(Box::new(b))
}

fn parse_endpoint(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("malformed endpoint: {s}"))?;
    Ok((h.to_string(), p.parse()?))
}
