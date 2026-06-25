//! Hybrid CPU+GPU backend: grind ONE nonce window on the CPU and the GPU
//! **concurrently**, over DISJOINT sub-ranges that together cover the whole
//! window exactly once (no nonce mined twice, none skipped). The split ratio,
//! the CPU thread budget, and the GPU batch all self-tune from measured
//! throughput — no user input.
//!
//! ## Why this is bit-exact
//! Both sides call the SAME backends (`CpuBackend` / `OpenClBackend`), each of
//! which is already validated against the golden vectors. Hybrid adds NO new hash
//! math: it only *partitions the nonce space* and *runs the two existing backends
//! in parallel*. Correctness therefore reduces to one property — the partition is
//! a disjoint cover of `[nonce_start, nonce_start + count)` — which is what
//! [`split_count`] guarantees and the tests pin down. The PoW (`pow.rs`) and the
//! golden vectors are untouched.
//!
//! ## Concurrency
//! `search()` spawns the GPU side on a scoped thread and runs the CPU side on the
//! calling thread (so the heavyweight CPU `thread::scope` fan-out stays on the
//! caller), joins both, concatenates the `Found`s, and returns. The CPU sub-range
//! uses `cpu_thread_budget(physical, gpu_active = true, …)` — the inviolable
//! "leave 2 free" rule — so a flat-out CPU half never starves the GPU feeder/OS.

use crate::backend::{Backend, Found};
use crate::threads::cpu_thread_budget;
use anyhow::Result;
use std::time::Instant;

/// Clamp band for the GPU's share of each window. We never let either side's
/// fraction collapse to ~0 (which would serialize the window onto one device and
/// waste the other) nor to ~1 (starving the other side). On this chain a GPU is
/// ~10–30× a CPU, so the GPU naturally sits high; the floor/ceiling are just guards.
pub const MIN_GPU_FRACTION: f64 = 0.05;
pub const MAX_GPU_FRACTION: f64 = 0.95;

/// Split `count` nonces into `(cpu_count, gpu_count)` for a given GPU fraction.
///
/// PURE + the correctness core. Guarantees, for any `count` and any finite
/// `gpu_fraction`:
/// - `cpu_count + gpu_count == count` (TOTAL COVER — no nonce skipped),
/// - the CPU takes the low sub-range `[start, start+cpu_count)` and the GPU the
///   high sub-range `[start+cpu_count, start+count)` (DISJOINT — no overlap),
/// - `gpu_fraction` is clamped into `[MIN_GPU_FRACTION, MAX_GPU_FRACTION]` first,
///   so neither side is ever handed the whole window when both should run; the
///   sole exception is a window so small the clamp can't give each ≥1 nonce, in
///   which case the larger side keeps it (still a disjoint cover).
///
/// `gpu_count` is `round(count * clamped_fraction)`; `cpu_count` is the remainder.
pub fn split_count(count: u32, gpu_fraction: f64) -> (u32, u32) {
    if count == 0 {
        return (0, 0);
    }
    // NaN-safe clamp into the band.
    let frac = if gpu_fraction.is_nan() {
        0.5
    } else {
        gpu_fraction.clamp(MIN_GPU_FRACTION, MAX_GPU_FRACTION)
    };
    // Round to nearest; the remainder is exact, so the cover is exact regardless.
    let mut gpu = (count as f64 * frac).round() as i64;
    if gpu < 0 {
        gpu = 0;
    }
    if gpu > count as i64 {
        gpu = count as i64;
    }
    let gpu_count = gpu as u32;
    let cpu_count = count - gpu_count; // exact remainder → total cover, no skip
    (cpu_count, gpu_count)
}

/// Self-tuning split ratio + per-side sizing.
///
/// Holds the current `gpu_fraction` and adapts it after each window from the
/// measured throughput (nonces ÷ seconds) of each side, moving toward the
/// throughput-implied fraction with damping (an EMA-style step) so a single noisy
/// batch can't swing the split wildly. Also caches the live CPU thread budget and
/// the GPU's suggested batch so `search()` sizes both sides without re-deriving.
#[derive(Clone, Debug)]
pub struct SplitTuner {
    gpu_fraction: f64,
    /// Damping factor in (0,1]: fraction of the way to move toward the new
    /// observation each update. 0.3 ≈ smooth but responsive over a few batches.
    alpha: f64,
}

impl Default for SplitTuner {
    fn default() -> Self {
        // Start ~50/50; the first couple of windows calibrate it toward the real
        // (usually GPU-heavy) ratio quickly via `observe`.
        SplitTuner {
            gpu_fraction: 0.5,
            alpha: 0.3,
        }
    }
}

impl SplitTuner {
    /// Start at an explicit fraction (clamped to the band) — used by tests and by
    /// a future calibration warm-start.
    pub fn with_fraction(gpu_fraction: f64) -> Self {
        SplitTuner {
            gpu_fraction: gpu_fraction.clamp(MIN_GPU_FRACTION, MAX_GPU_FRACTION),
            alpha: 0.3,
        }
    }

    /// The current GPU fraction (already within the band).
    pub fn gpu_fraction(&self) -> f64 {
        self.gpu_fraction
    }

    /// Split the next window using the current fraction.
    pub fn split(&self, count: u32) -> (u32, u32) {
        split_count(count, self.gpu_fraction)
    }

    /// Update the fraction from one window's measured per-side throughput.
    ///
    /// `cpu_nps` / `gpu_nps` are nonces-per-second for each side (nonces ÷ wall
    /// seconds for that side's sub-search). The *ideal* steady-state GPU fraction
    /// is the GPU's share of total throughput: `gpu_nps / (cpu_nps + gpu_nps)` —
    /// at that split both sides finish a window at the same time (max overlap, min
    /// wait). We step the current fraction a fraction `alpha` of the way toward
    /// that target, then clamp to the band. Degenerate inputs (a side reporting 0
    /// or non-finite throughput, e.g. its sub-range was empty this batch) are
    /// ignored so the ratio holds rather than lurching.
    pub fn observe(&mut self, cpu_nps: f64, gpu_nps: f64) {
        if !cpu_nps.is_finite() || !gpu_nps.is_finite() {
            return;
        }
        if cpu_nps <= 0.0 || gpu_nps <= 0.0 {
            return; // not enough signal (an empty side this batch) — hold.
        }
        let target = gpu_nps / (cpu_nps + gpu_nps);
        let next = self.gpu_fraction + self.alpha * (target - self.gpu_fraction);
        self.gpu_fraction = next.clamp(MIN_GPU_FRACTION, MAX_GPU_FRACTION);
    }
}

/// Compute throughput (nonces per second) defensively. A zero/negative/near-zero
/// elapsed (a window that returned instantly) yields `0.0`, which `observe`
/// treats as "no signal" and ignores — never a divide-by-zero or an absurd rate.
pub fn throughput(nonces: u32, secs: f64) -> f64 {
    if !secs.is_finite() || secs <= 1e-9 {
        return 0.0;
    }
    nonces as f64 / secs
}

/// CPU + GPU concurrent backend.
///
/// Owns both child backends and the [`SplitTuner`]. `physical` is the physical
/// core count, threaded through so the CPU side always re-derives its budget with
/// `gpu_active = true` (leave-2-free), even if the box's reported cores change.
pub struct HybridBackend {
    name: String,
    cpu: Box<dyn Backend>,
    gpu: Box<dyn Backend>,
    tuner: SplitTuner,
    physical: usize,
}

impl HybridBackend {
    /// Build from a CPU backend and a GPU backend. The CPU backend SHOULD already
    /// have been constructed with `cpu_thread_budget(physical, true, …)` threads
    /// (the caller in `main` does this); `physical` is kept for telemetry/asserts.
    pub fn new(cpu: Box<dyn Backend>, gpu: Box<dyn Backend>, physical: usize) -> Self {
        let name = format!("hybrid[{} + {}]", cpu.name(), gpu.name());
        HybridBackend {
            name,
            cpu,
            gpu,
            tuner: SplitTuner::default(),
            physical,
        }
    }

    /// Current GPU fraction (for tests / telemetry).
    pub fn gpu_fraction(&self) -> f64 {
        self.tuner.gpu_fraction()
    }

    /// The CPU thread budget this hybrid would use (always `gpu_active = true`).
    pub fn cpu_budget(&self) -> usize {
        cpu_thread_budget(self.physical, true, None)
    }
}

impl Backend for HybridBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_gpu(&self) -> bool {
        // A hybrid IS partly a GPU miner; the thread-budget "2-free" rule applies.
        true
    }

    fn suggested_batch(&self) -> u32 {
        // Drive responsiveness off the GPU (the larger, coarser side). The window
        // is then split CPU/GPU internally; a bigger window amortizes the GPU
        // launch overhead while the CPU half stays plenty granular.
        self.gpu.suggested_batch()
    }

    fn search(
        &mut self,
        midstate: &[u8; 32],
        target: &[u8; 32],
        nonce_start: u64,
        count: u32,
    ) -> Result<Vec<Found>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let (cpu_count, gpu_count) = self.tuner.split(count);
        // DISJOINT cover: CPU does the low sub-range, GPU the high sub-range.
        let cpu_start = nonce_start;
        let gpu_start = nonce_start.wrapping_add(cpu_count as u64);

        let cpu = &mut self.cpu;
        let gpu = &mut self.gpu;
        let mid = *midstate;
        let tgt = *target;

        // Run the GPU side on a scoped thread; run the CPU side on this thread.
        // Each side times itself so the tuner can adapt the ratio next window.
        let mut cpu_found: Vec<Found> = Vec::new();
        let mut gpu_found: Vec<Found> = Vec::new();
        let mut cpu_secs = 0.0f64;
        let mut gpu_secs = 0.0f64;
        let mut err: Option<anyhow::Error> = None;

        std::thread::scope(|s| {
            let gpu_handle = s.spawn(|| {
                let t = Instant::now();
                let r = if gpu_count > 0 {
                    gpu.search(&mid, &tgt, gpu_start, gpu_count)
                } else {
                    Ok(Vec::new())
                };
                (r, t.elapsed().as_secs_f64())
            });

            // CPU side on the calling thread.
            let t = Instant::now();
            let cpu_res = if cpu_count > 0 {
                cpu.search(&mid, &tgt, cpu_start, cpu_count)
            } else {
                Ok(Vec::new())
            };
            cpu_secs = t.elapsed().as_secs_f64();

            match cpu_res {
                Ok(v) => cpu_found = v,
                Err(e) => err = Some(e),
            }

            let (gpu_res, g_secs) = gpu_handle.join().unwrap_or_else(|_| {
                (
                    Err(anyhow::anyhow!("hybrid: GPU worker thread panicked")),
                    0.0,
                )
            });
            gpu_secs = g_secs;
            match gpu_res {
                Ok(v) => gpu_found = v,
                Err(e) => {
                    if err.is_none() {
                        err = Some(e);
                    }
                }
            }
        });

        if let Some(e) = err {
            return Err(e);
        }

        // Adapt the split from this window's measured per-side throughput.
        let cpu_nps = throughput(cpu_count, cpu_secs);
        let gpu_nps = throughput(gpu_count, gpu_secs);
        self.tuner.observe(cpu_nps, gpu_nps);

        let mut found = cpu_found;
        found.extend(gpu_found);
        found.sort_by_key(|f| f.nonce);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Found;

    // ───────────────────────── split_count: disjoint + total cover ──────────────

    /// The fundamental invariant for an arbitrary count and fraction:
    /// cpu_count + gpu_count == count (no nonce skipped or double-counted).
    #[test]
    fn split_is_total_cover() {
        for &count in &[0u32, 1, 2, 3, 7, 64, 65535, 65536, 1_000_000, u32::MAX] {
            for &frac in &[0.0, 0.01, 0.25, 0.5, 0.5001, 0.75, 0.95, 1.0, 1.5, -3.0] {
                let (c, g) = split_count(count, frac);
                assert_eq!(
                    c as u64 + g as u64,
                    count as u64,
                    "cover broken for count={count} frac={frac}: cpu={c} gpu={g}"
                );
            }
        }
    }

    /// The two sub-ranges, laid out as [start, start+cpu) and [start+cpu, start+count),
    /// are DISJOINT and together hit every nonce in the window exactly once.
    #[test]
    fn split_sub_ranges_are_disjoint_and_exhaustive() {
        let start: u64 = 1_000;
        let count: u32 = 200;
        for &frac in &[0.0, 0.1, 0.37, 0.5, 0.63, 0.9, 1.0] {
            let (cpu_count, gpu_count) = split_count(count, frac);
            let cpu_start = start;
            let gpu_start = start + cpu_count as u64;

            // Materialize both sub-ranges and confirm: no overlap, full cover.
            let mut seen = vec![false; count as usize];
            for k in 0..cpu_count as u64 {
                let idx = (cpu_start + k - start) as usize;
                assert!(!seen[idx], "CPU revisited nonce idx {idx} (frac={frac})");
                seen[idx] = true;
            }
            for k in 0..gpu_count as u64 {
                let idx = (gpu_start + k - start) as usize;
                assert!(
                    !seen[idx],
                    "GPU overlapped CPU at nonce idx {idx} (frac={frac})"
                );
                seen[idx] = true;
            }
            assert!(
                seen.iter().all(|&b| b),
                "some nonce was skipped (frac={frac}): {:?}",
                seen.iter().position(|&b| !b)
            );
        }
    }

    /// The clamp keeps both sides ≥1 nonce for any reasonable window, even at
    /// extreme fractions, so neither device ever idles on a non-tiny window.
    #[test]
    fn split_keeps_both_sides_busy_on_reasonable_windows() {
        for &count in &[100u32, 1000, 65536] {
            let (c0, g0) = split_count(count, 0.0); // clamps up to MIN_GPU_FRACTION
            assert!(g0 >= 1, "gpu starved at frac 0 for count={count}");
            assert!(c0 >= 1);
            let (c1, g1) = split_count(count, 1.0); // clamps down to MAX_GPU_FRACTION
            assert!(c1 >= 1, "cpu starved at frac 1 for count={count}");
            assert!(g1 >= 1);
        }
    }

    #[test]
    fn split_nan_is_treated_as_half() {
        let (c, g) = split_count(100, f64::NAN);
        assert_eq!(c + g, 100);
        assert_eq!(g, 50);
    }

    // ───────────────────────── SplitTuner: ratio converges ──────────────────────

    /// With the GPU ~10× the CPU, the tuner should converge the GPU fraction
    /// toward ~10/11 ≈ 0.909 (clamped to the 0.95 ceiling). Monotone-ish climb
    /// from 0.5, and within a few % of the target after enough batches.
    #[test]
    fn tuner_converges_toward_throughput_ratio_gpu_heavy() {
        let mut t = SplitTuner::default();
        assert!((t.gpu_fraction() - 0.5).abs() < 1e-9);
        // GPU 10M nonces/s, CPU 1M nonces/s → ideal gpu fraction ≈ 0.909.
        for _ in 0..50 {
            t.observe(1_000_000.0, 10_000_000.0);
        }
        let f = t.gpu_fraction();
        assert!(
            (0.88..=0.95).contains(&f),
            "expected gpu fraction to converge near 10/11≈0.909 (capped 0.95), got {f}"
        );
    }

    /// Symmetric case: if the CPU is actually faster (tiny GPU), the fraction
    /// should fall toward the CPU, i.e. the GPU fraction drops well below 0.5.
    #[test]
    fn tuner_moves_toward_cpu_when_cpu_faster() {
        let mut t = SplitTuner::with_fraction(0.5);
        for _ in 0..50 {
            t.observe(8_000_000.0, 2_000_000.0); // ideal gpu frac = 0.2
        }
        let f = t.gpu_fraction();
        assert!((0.18..=0.25).contains(&f), "expected ~0.2, got {f}");
    }

    /// Balanced throughput → fraction settles near 0.5 and stays in band.
    #[test]
    fn tuner_balanced_settles_near_half() {
        let mut t = SplitTuner::with_fraction(0.5);
        for _ in 0..30 {
            t.observe(5_000_000.0, 5_000_000.0);
        }
        let f = t.gpu_fraction();
        assert!((0.45..=0.55).contains(&f), "expected ~0.5, got {f}");
    }

    /// Convergence is damped: one wild outlier batch can't swing the fraction more
    /// than ~alpha of the way (no lurch from a single noisy measurement).
    #[test]
    fn tuner_single_step_is_damped() {
        let mut t = SplitTuner::with_fraction(0.5);
        // Target would be ~0.95 here; a single step moves only ~alpha*(0.95-0.5).
        t.observe(100.0, 1_000_000.0);
        let f = t.gpu_fraction();
        assert!(
            f > 0.5 && f < 0.72,
            "single update should be damped (≈0.5+0.3*Δ), got {f}"
        );
    }

    /// Degenerate per-side throughput (a side reported 0, e.g. empty sub-range)
    /// is ignored — the fraction holds rather than lurching to a boundary.
    #[test]
    fn tuner_ignores_zero_or_nonfinite_signal() {
        let mut t = SplitTuner::with_fraction(0.7);
        t.observe(0.0, 5_000_000.0);
        assert!((t.gpu_fraction() - 0.7).abs() < 1e-12, "zero cpu held");
        t.observe(5_000_000.0, 0.0);
        assert!((t.gpu_fraction() - 0.7).abs() < 1e-12, "zero gpu held");
        t.observe(f64::NAN, 1.0);
        assert!((t.gpu_fraction() - 0.7).abs() < 1e-12, "nan held");
        t.observe(f64::INFINITY, 1.0);
        assert!((t.gpu_fraction() - 0.7).abs() < 1e-12, "inf held");
    }

    /// The fraction is ALWAYS inside the band after any sequence of observations.
    #[test]
    fn tuner_stays_in_band() {
        let mut t = SplitTuner::default();
        for i in 0..200 {
            // Alternate extreme signals; must never leave [MIN, MAX].
            if i % 2 == 0 {
                t.observe(1.0, 1e12);
            } else {
                t.observe(1e12, 1.0);
            }
            let f = t.gpu_fraction();
            assert!(
                (MIN_GPU_FRACTION..=MAX_GPU_FRACTION).contains(&f),
                "out of band: {f}"
            );
        }
    }

    // ───────────────────────── throughput helper ────────────────────────────────

    #[test]
    fn throughput_is_defensive() {
        assert_eq!(throughput(1000, 0.0), 0.0);
        assert_eq!(throughput(1000, -1.0), 0.0);
        assert_eq!(throughput(1000, f64::NAN), 0.0);
        assert!((throughput(1000, 0.5) - 2000.0).abs() < 1e-6);
    }

    // ───────────────────────── HybridBackend with fake child backends ───────────

    /// A deterministic fake backend: records the EXACT nonce sub-range it was
    /// asked to search, and "finds" every nonce in it (target ignored). Lets us
    /// assert the hybrid split is a disjoint cover end-to-end WITHOUT doing 1M-iter
    /// BLAKE3 — the real backends are golden-tested separately.
    struct RecordingBackend {
        name: String,
        gpu: bool,
        batch: u32,
        seen: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    }
    impl RecordingBackend {
        fn new(name: &str, gpu: bool, batch: u32) -> Self {
            RecordingBackend {
                name: name.to_string(),
                gpu,
                batch,
                seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }
    impl Backend for RecordingBackend {
        fn name(&self) -> &str {
            &self.name
        }
        fn is_gpu(&self) -> bool {
            self.gpu
        }
        fn suggested_batch(&self) -> u32 {
            self.batch
        }
        fn search(
            &mut self,
            _midstate: &[u8; 32],
            _target: &[u8; 32],
            nonce_start: u64,
            count: u32,
        ) -> Result<Vec<Found>> {
            let mut out = Vec::with_capacity(count as usize);
            let mut guard = self.seen.lock().unwrap();
            for k in 0..count as u64 {
                let nonce = nonce_start + k;
                guard.push(nonce);
                out.push(Found {
                    nonce,
                    final_hash: [0u8; 32],
                });
            }
            Ok(out)
        }
    }

    /// End-to-end: HybridBackend.search must return EVERY nonce in the window
    /// exactly once (the union of the two child sub-ranges is a disjoint cover),
    /// with NO nonce searched by both children.
    #[test]
    fn hybrid_search_is_disjoint_cover_end_to_end() {
        let cpu_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let gpu_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut cpu = RecordingBackend::new("cpu:fake", false, 256);
        let mut gpu = RecordingBackend::new("gpu:fake", true, 65536);
        cpu.seen = cpu_seen.clone();
        gpu.seen = gpu_seen.clone();

        let mut h = HybridBackend::new(Box::new(cpu), Box::new(gpu), 12);
        let start = 5_000_000u64;
        let count = 10_000u32;
        let out = h.search(&[0u8; 32], &[0xffu8; 32], start, count).unwrap();

        // Every nonce in [start, start+count) returned exactly once.
        assert_eq!(
            out.len(),
            count as usize,
            "hybrid lost or duplicated nonces"
        );
        let mut nonces: Vec<u64> = out.iter().map(|f| f.nonce).collect();
        nonces.sort_unstable();
        nonces.dedup();
        assert_eq!(nonces.len(), count as usize, "duplicate nonce in output");
        assert_eq!(*nonces.first().unwrap(), start);
        assert_eq!(*nonces.last().unwrap(), start + count as u64 - 1);

        // The two children's searched sets are DISJOINT and partition the window.
        let c = cpu_seen.lock().unwrap().clone();
        let g = gpu_seen.lock().unwrap().clone();
        assert_eq!(
            c.len() + g.len(),
            count as usize,
            "children did not partition"
        );
        let cset: std::collections::HashSet<u64> = c.into_iter().collect();
        let gset: std::collections::HashSet<u64> = g.into_iter().collect();
        assert!(
            cset.is_disjoint(&gset),
            "CPU and GPU searched the same nonce"
        );
    }

    /// is_gpu() is true (so the client/thread-budget treat hybrid as GPU-active),
    /// and the CPU budget honors leave-2-free.
    #[test]
    fn hybrid_is_gpu_and_budgets_leave_two_free() {
        let cpu = RecordingBackend::new("cpu:fake", false, 256);
        let gpu = RecordingBackend::new("gpu:fake", true, 65536);
        let h = HybridBackend::new(Box::new(cpu), Box::new(gpu), 12);
        assert!(
            h.is_gpu(),
            "hybrid must report is_gpu()=true for the 2-free rule"
        );
        assert_eq!(h.cpu_budget(), 10, "12 physical, gpu active → 10 (leave 2)");
        assert_eq!(
            h.suggested_batch(),
            65536,
            "hybrid batch follows the GPU side"
        );
    }

    /// A zero-count window returns nothing and never touches a child.
    #[test]
    fn hybrid_zero_count_is_empty() {
        let cpu = RecordingBackend::new("cpu:fake", false, 256);
        let gpu = RecordingBackend::new("gpu:fake", true, 65536);
        let mut h = HybridBackend::new(Box::new(cpu), Box::new(gpu), 12);
        let out = h.search(&[0u8; 32], &[0xffu8; 32], 42, 0).unwrap();
        assert!(out.is_empty());
    }

    /// If a child backend errors, the hybrid surfaces the error (never silently
    /// drops a half of the window).
    #[test]
    fn hybrid_propagates_child_error() {
        struct ErrBackend;
        impl Backend for ErrBackend {
            fn name(&self) -> &str {
                "gpu:err"
            }
            fn is_gpu(&self) -> bool {
                true
            }
            fn suggested_batch(&self) -> u32 {
                65536
            }
            fn search(
                &mut self,
                _m: &[u8; 32],
                _t: &[u8; 32],
                _s: u64,
                _c: u32,
            ) -> Result<Vec<Found>> {
                Err(anyhow::anyhow!("device fell off the bus"))
            }
        }
        let cpu = RecordingBackend::new("cpu:fake", false, 256);
        let mut h = HybridBackend::new(Box::new(cpu), Box::new(ErrBackend), 12);
        let res = h.search(&[0u8; 32], &[0xffu8; 32], 0, 1000);
        assert!(
            res.is_err(),
            "a child error must propagate, not be swallowed"
        );
    }

    /// REAL backends, bit-exact end-to-end: build a HybridBackend over the REAL
    /// `CpuBackend` and the REAL `OpenClBackend`, grind a tiny window with an
    /// all-0xff target (every nonce passes), and assert it returns EXACTLY the
    /// golden hashes for nonce 0 and nonce 1 — i.e. concurrent CPU+GPU mining over
    /// the disjoint split is bit-identical to the reference PoW. Needs an OpenCL
    /// device + the `opencl` feature. With a 50/50 default split over 2 nonces the
    /// CPU takes nonce 0 and the GPU nonce 1, so this also proves BOTH paths feed
    /// the same `Found` set with no overlap/skip.
    /// Run: `cargo test --release --features opencl -- --ignored hybrid_real_golden`.
    #[cfg(feature = "opencl")]
    #[test]
    #[ignore = "needs an OpenCL device + the opencl feature; does 2x1M BLAKE3"]
    fn hybrid_real_backends_are_bit_exact() {
        use crate::backend::CpuBackend;
        use crate::opencl_backend::OpenClBackend;
        use opencl3::device::CL_DEVICE_TYPE_ALL;

        let gpu = OpenClBackend::try_new_with_type(CL_DEVICE_TYPE_ALL)
            .unwrap()
            .expect("an OpenCL device");
        let cpu = CpuBackend::new(2);
        let mut h = HybridBackend::new(Box::new(cpu), Box::new(gpu), 4);

        let target = [0xffu8; 32];
        let out = h.search(&[0u8; 32], &target, 0, 2).unwrap();
        assert_eq!(out.len(), 2, "hybrid must return both nonces exactly once");
        assert_eq!(out[0].nonce, 0);
        assert_eq!(out[1].nonce, 1);
        assert_eq!(
            hex::encode(out[0].final_hash),
            "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369",
            "hybrid nonce=0 must match the CPU/GPU golden vector bit-for-bit"
        );
        assert_eq!(
            hex::encode(out[1].final_hash),
            "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9",
            "hybrid nonce=1 must match the golden vector bit-for-bit"
        );
    }
}
