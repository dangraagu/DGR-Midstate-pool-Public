//! `midstate-miner` — pool-only CLI. The endpoint is compiled in (no `--pool`).

use anyhow::{anyhow, Result};
use clap::Parser;
use midstate_miner::client::{run, ClientConfig};
use midstate_miner::{cpu_thread_budget, pool_endpoint, CpuBackend};
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
    // v1 is CPU-only; the CUDA backend (gpu_active=true → the 2-free rule) lands next.
    let gpu_active = false;
    let threads = cpu_thread_budget(physical, gpu_active, cli.cpu_threads);
    if threads == 0 {
        return Err(anyhow!("0 CPU threads after budget — nothing to mine"));
    }
    let mut backend = CpuBackend::new(threads);

    println!(
        "midstate-miner | endpoint={endpoint} | physical_cores={physical} | cpu_threads={threads}"
    );

    let cfg = ClientConfig {
        host,
        port,
        address: cli.address,
        share_bits: cli.share_bits,
        reconnect_backoff: Duration::from_secs(5),
        read_timeout: Duration::from_secs(120),
    };
    let dur = (cli.duration != 0).then(|| Duration::from_secs(cli.duration));
    run(cfg, &mut backend, dur)
}

fn parse_endpoint(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("malformed endpoint: {s}"))?;
    Ok((h.to_string(), p.parse()?))
}
