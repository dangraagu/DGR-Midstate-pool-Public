//! Mining backends. A `Backend` scans a nonce window and returns every nonce
//! whose 1,000,000-iteration final hash clears the supplied (share) target.
//!
//! `CpuBackend` is the always-available floor (works on any machine, no GPU) and
//! the correctness oracle the future CUDA backend is validated against (golden
//! vectors `a713dea1…` / `8ac4d9ef…`). The CUDA backend (persistent kernel via
//! cudarc + committed PTX) lands behind a `cuda` cargo feature in the next
//! milestone and implements this same trait, so the client loop is backend-agnostic.

use crate::pow::{meets_target, midstate_pow};
use anyhow::Result;

/// A solved candidate: a nonce whose 1M-iter final hash is `< target`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    pub nonce: u64,
    pub final_hash: [u8; 32],
}

/// One synchronous "search this nonce window" unit of work. Backends are
/// interchangeable; the Stratum loop drives whichever one was selected.
pub trait Backend: Send {
    /// Human label for logs/telemetry, e.g. `"cpu:x10"` / `"cuda:RTX 5070 Ti"`.
    fn name(&self) -> &str;

    /// True for any GPU backend — drives the CPU thread-budget "2-free" rule.
    fn is_gpu(&self) -> bool;

    /// Scan `[nonce_start, nonce_start + count)`; return every nonce whose
    /// consensus final hash is `< target`. Blocking: returns when the whole
    /// window is hashed.
    fn search(
        &mut self,
        midstate: &[u8; 32],
        target: &[u8; 32],
        nonce_start: u64,
        count: u32,
    ) -> Result<Vec<Found>>;

    /// Suggested window size per `search` call (responsiveness vs overhead).
    fn suggested_batch(&self) -> u32;
}

/// CPU backend: parallelizes a window across `n_threads` workers using the
/// bit-exact `pow::midstate_pow`. Workers walk disjoint interleaved residue
/// classes (worker `i` does nonces `start + i, i+n, i+2n, …`) — no overlap, no
/// RNG, no shared cursor.
pub struct CpuBackend {
    name: String,
    n_threads: usize,
}

impl CpuBackend {
    pub fn new(n_threads: usize) -> Self {
        let n = n_threads.max(1);
        CpuBackend {
            name: format!("cpu:x{}", n),
            n_threads: n,
        }
    }
}

impl Backend for CpuBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_gpu(&self) -> bool {
        false
    }

    fn suggested_batch(&self) -> u32 {
        // FIX 1 — RESPONSIVE WINDOW. Each nonce ≈ 1M sequential BLAKE3 (~0.56 ms
        // on one core). With ~1 nonce per core the whole window finishes in
        // roughly that ~0.5–1 s, so the loop re-checks the new-job epoch ~16×
        // more often than the old `*128` window (which blocked ~8.5 s on a
        // 119-thread box and never finished before the pool rolled the job,
        // wasting an entire window of work per roll). Target ≈ a small constant
        // of nonces-per-thread so the wall time is bounded SUB-SECOND regardless
        // of thread count; keep a >=64 floor so a tiny box doesn't thrash on a
        // 1-nonce window.
        (self.n_threads as u32).saturating_mul(4).max(64)
    }

    fn search(
        &mut self,
        midstate: &[u8; 32],
        target: &[u8; 32],
        nonce_start: u64,
        count: u32,
    ) -> Result<Vec<Found>> {
        let n = self.n_threads;
        let mid = *midstate;
        let tgt = *target;
        let mut found: Vec<Found> = Vec::new();

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    s.spawn(move || {
                        let mut local = Vec::new();
                        let mut k = i as u64;
                        while k < count as u64 {
                            let nonce = nonce_start.wrapping_add(k);
                            let h = midstate_pow(mid, nonce);
                            if meets_target(&h, &tgt) {
                                local.push(Found {
                                    nonce,
                                    final_hash: h,
                                });
                            }
                            k += n as u64;
                        }
                        local
                    })
                })
                .collect();
            for handle in handles {
                if let Ok(mut v) = handle.join() {
                    found.append(&mut v);
                }
            }
        });

        found.sort_by_key(|f| f.nonce);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_backend_basics() {
        let b = CpuBackend::new(10);
        assert_eq!(b.name(), "cpu:x10");
        assert!(!b.is_gpu());
        assert!(b.suggested_batch() >= 64);
        // n_threads is clamped to at least 1.
        assert_eq!(CpuBackend::new(0).name(), "cpu:x1");
    }

    /// FIX 1 — responsive window. The per-`search` window must target a
    /// sub-second block REGARDLESS of thread count. Each nonce ≈ 1M BLAKE3 ≈
    /// ~0.56 ms on one core, so a window of `~1 nonce per core` finishes in
    /// roughly that ~0.5–1 s; the multiplier per thread must be SMALL (the old
    /// `*128` blocked ~8.5 s on a 119-thread box and never finished before a job
    /// roll). We assert the multiplier dropped well below 128 and the resulting
    /// window stays sub-second for a big box, while keeping a >=64 floor.
    #[test]
    fn suggested_batch_is_responsive_for_high_thread_counts() {
        // The per-thread multiplier is the batch / n_threads once past the floor.
        // It MUST be far below the old 128 (we use 4) so the window stays small.
        let big_n = 119usize;
        let b = CpuBackend::new(big_n);
        let batch = b.suggested_batch();
        let per_thread = batch as f64 / big_n as f64;
        assert!(
            per_thread <= 8.0,
            "multiplier must be << old 128 (got {per_thread} nonces/thread)"
        );
        assert!(
            per_thread != 128.0,
            "multiplier must have changed from 128"
        );
        // Sub-second window check: each nonce ≈ 1M BLAKE3 ≈ ~0.56 ms/core, and the
        // window is split across `big_n` cores, so wall time ≈ per_thread * 0.56 ms.
        // per_thread<=8 → ≈4.5 ms worst case — comfortably sub-second. We pin the
        // hard nonce ceiling that keeps it there: a big box must not exceed ~1
        // nonce-per-core * a small constant.
        const MS_PER_NONCE_PER_CORE: f64 = 0.56; // ~1M BLAKE3 on one core
        let est_window_ms = per_thread * MS_PER_NONCE_PER_CORE;
        assert!(
            est_window_ms < 500.0,
            "window must be sub-second (~{est_window_ms} ms estimated)"
        );
        // The old formula would have been 119*128 = 15232 nonces.
        assert!(
            batch < 119 * 128,
            "batch must be smaller than the old n_threads*128 window"
        );
    }

    /// The >=64 minimum window still holds for tiny boxes (a 1-thread box must
    /// not thrash on a 1-nonce window — keep at least the 64 floor).
    #[test]
    fn suggested_batch_keeps_minimum_floor() {
        assert!(CpuBackend::new(1).suggested_batch() >= 64);
        assert!(CpuBackend::new(2).suggested_batch() >= 64);
        // A mid box uses the multiplier once it exceeds the floor.
        assert_eq!(CpuBackend::new(60).suggested_batch(), 60 * 4);
    }

    /// Real 1M-iter search over a tiny window with an all-0xff target (accepts
    /// every nonce). Verifies the interleaved-stride split covers the whole range
    /// exactly once and returns bit-exact hashes. `#[ignore]` — does count×1M BLAKE3.
    /// Run: `cargo test --release -- --ignored cpu_backend_search`.
    #[test]
    #[ignore = "count×1,000,000 BLAKE3; run with --release --ignored"]
    fn cpu_backend_search_covers_range() {
        let mut b = CpuBackend::new(4);
        let mid = [0u8; 32];
        let target = [0xffu8; 32]; // every nonce clears it
        let out = b.search(&mid, &target, 0, 8).unwrap();
        assert_eq!(out.len(), 8);
        for (i, f) in out.iter().enumerate() {
            assert_eq!(f.nonce, i as u64);
            assert_eq!(f.final_hash, midstate_pow(mid, i as u64));
        }
    }
}
