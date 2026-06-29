//! Bounded GPU-backend benchmark. Builds whichever GPU backend the active cargo
//! feature enables (cuda / wgpu / opencl), then grinds an IMPOSSIBLE all-0x00
//! target so NO nonce ever matches (zero CPU re-verify), timing total nonces to
//! report nonces/s. Pool-free, bounded.
//!
//! Run (in WSL):
//!   cargo run --release --features cuda   --example bench_backend
//!   cargo run --release --features wgpu   --example bench_backend
//!   cargo run --release --features opencl --example bench_backend

use midstate_miner::Backend;
use std::time::Instant;

fn build() -> Option<Box<dyn Backend>> {
    #[cfg(feature = "cuda")]
    {
        match midstate_miner::cuda_backend::CudaBackend::try_new(Some(0), None) {
            Ok(Some(b)) => return Some(Box::new(b)),
            Ok(None) => {
                eprintln!("cuda: no device");
                return None;
            }
            Err(e) => {
                eprintln!("cuda init failed: {e:?}");
                return None;
            }
        }
    }
    #[cfg(feature = "wgpu")]
    {
        match midstate_miner::wgpu_backend::WgpuBackend::try_new(Some(0)) {
            Ok(Some(b)) => return Some(Box::new(b)),
            Ok(None) => {
                eprintln!("wgpu: no adapter");
                return None;
            }
            Err(e) => {
                eprintln!("wgpu init failed: {e:?}");
                return None;
            }
        }
    }
    #[cfg(feature = "opencl")]
    {
        match midstate_miner::opencl_backend::OpenClBackend::try_new() {
            Ok(Some(b)) => return Some(Box::new(b)),
            Ok(None) => {
                eprintln!("opencl: no device");
                return None;
            }
            Err(e) => {
                eprintln!("opencl init failed: {e:?}");
                return None;
            }
        }
    }
    #[allow(unreachable_code)]
    {
        eprintln!("no GPU feature compiled in");
        None
    }
}

fn main() {
    let mut b = match build() {
        Some(b) => b,
        None => {
            eprintln!("BENCH_RESULT backend=NONE nonces_per_s=0 (backend failed to build/run)");
            std::process::exit(2);
        }
    };
    println!("backend = {} (is_gpu={})", b.name(), b.is_gpu());
    let batch = b.suggested_batch().max(4096);
    println!("suggested_batch = {} (using window={})", b.suggested_batch(), batch);

    let mid = [0x11u8; 32];
    let target = [0x00u8; 32]; // impossible: no nonce ever clears it -> no CPU re-verify

    // Warm-up window (kernel JIT / first-launch cost not counted).
    let _ = b.search(&mid, &target, 0, batch).expect("warmup search");

    // Timed grind: ~8 seconds of wall-clock windows.
    let run_secs = 8.0_f64;
    let start = Instant::now();
    let mut total_nonces: u128 = 0;
    let mut nonce_cursor: u64 = batch as u64;
    while start.elapsed().as_secs_f64() < run_secs {
        let found = b.search(&mid, &target, nonce_cursor, batch).expect("search");
        assert!(found.is_empty(), "impossible target produced a match?!");
        total_nonces += batch as u128;
        nonce_cursor = nonce_cursor.wrapping_add(batch as u64);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let nps = total_nonces as f64 / elapsed;
    println!(
        "BENCH_RESULT backend={} nonces={} secs={:.3} nonces_per_s={:.1}",
        b.name(),
        total_nonces,
        elapsed,
        nps
    );
}
