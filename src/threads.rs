//! CPU thread-budget allocator.
//!
//! User requirement (2026-06-24): when a GPU is ALSO mining, the CPU miner must
//! ALWAYS leave **2 threads free** — one for the GPU feeder, one for the OS/IO —
//! so a flat-out CPU miner never starves the GPU (which is ~96% of combined
//! hashrate on this chain) or the system. With no GPU, the CPU may use all
//! (physical) cores.
//!
//! AVX2 BLAKE3 is execution-port-bound, so hyperthreading buys ~nothing; the
//! budget is in PHYSICAL cores, and workers should be pinned one-per-physical-core.

/// Number of CPU worker threads to run.
///
/// - `physical_cores`: physical core count (NOT logical / hyperthreads).
/// - `gpu_active`: true if a GPU backend is mining alongside the CPU.
/// - `user_override`: an explicit `--cpu-threads N`, if the user set one.
///
/// Rule: with a GPU active the ceiling is `physical_cores - 2` (the inviolable
/// "leave 2 free" rule); otherwise the ceiling is `physical_cores`. A user
/// override is honored but **clamped to that ceiling** — it can ask for *fewer*,
/// never more, so the 2-free guarantee holds even against an explicit override.
/// Saturates to 0 on tiny boxes (e.g. a 2-core box with a GPU → 0 CPU threads;
/// the caller should log that CPU mining is disabled there).
pub fn cpu_thread_budget(
    physical_cores: usize,
    gpu_active: bool,
    user_override: Option<usize>,
) -> usize {
    let ceiling = if gpu_active {
        physical_cores.saturating_sub(2)
    } else {
        physical_cores
    };
    match user_override {
        Some(n) => n.min(ceiling),
        None => ceiling,
    }
}

/// FIX 3 — CPU-ONLY thread budget, based on **LOGICAL** cores.
///
/// On a rented box the reported physical-core count is often ~half the logical
/// (vCPU) count, and the GPU-path budget above (which uses `physical_cores`)
/// would silently run a CPU-only miner at ~half throughput. When there is NO GPU
/// in play, AVX2 BLAKE3's port-bound argument doesn't apply the same way to a
/// rented vCPU box — the operator paid for all the vCPUs and wants them grinding
/// — so the CPU-only path uses ALL logical cores by default.
///
/// - `logical_cores`: logical / vCPU count (`num_cpus::get()`), the ceiling.
/// - `user_override`: an explicit `--cpu-threads N`. Honored UP TO `logical_cores`
///   — crucially it MAY exceed the physical count (that's the whole point); it is
///   only clamped at the logical ceiling so we never oversubscribe far beyond the
///   hardware. Asking for fewer is honored.
///
/// This path NEVER applies the "leave 2 free" rule — that guard exists only to
/// protect a co-resident GPU feeder/OS, and there is no GPU here.
pub fn cpu_only_thread_budget(logical_cores: usize, user_override: Option<usize>) -> usize {
    match user_override {
        Some(n) => n.min(logical_cores),
        None => logical_cores,
    }
}

/// v0.1.9 NEVER-DARK — the reduced CPU budget for the gpu/hybrid → CPU fallback.
///
/// When an explicit `--mode gpu`/`hybrid`/`--gpu-id` request can't get its GPU
/// (driver rejects the PTX, self-test fails, bad index), the miner now falls
/// back to CPU instead of exiting invisible. But the fallback is a VISIBILITY
/// trickle, not a CPU takeover: a multi-GPU rig runs one process per card, and
/// if the GPU backend breaks on all of them, N full-width CPU miners would
/// oversubscribe the box. So: **min(2, logical)** threads per process — enough
/// to stay connected and submit occasional shares, cheap enough that N of them
/// coexist harmlessly. An explicit `--cpu-threads N` overrides the trickle
/// (clamped to `logical_cores`, same contract as [`cpu_only_thread_budget`]).
// v0.1.9 review fix: an explicit `--cpu-threads 0` is FLOORED TO 1 here —
// never-dark is absolute: a 0-thread fallback would bail before connecting,
// recreating the invisible crash-loop this budget exists to kill. Use
// `--strict-gpu` to forbid fallback CPU mining entirely. (On an absurd
// 0-logical-core report the result is still 0 and the caller bails.)
pub fn cpu_fallback_thread_budget(logical_cores: usize, user_override: Option<usize>) -> usize {
    match user_override {
        Some(n) => n.max(1).min(logical_cores),
        None => 2.min(logical_cores),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_two_free_when_gpu_active() {
        // i7-12700K: 12 physical cores.
        assert_eq!(cpu_thread_budget(12, true, None), 10); // GPU on → leave 2
        assert_eq!(cpu_thread_budget(12, false, None), 12); // no GPU → all cores
    }

    #[test]
    fn small_boxes_saturate_to_zero_with_gpu() {
        assert_eq!(cpu_thread_budget(4, true, None), 2);
        assert_eq!(cpu_thread_budget(2, true, None), 0); // can't leave 2 AND mine
        assert_eq!(cpu_thread_budget(1, true, None), 0);
        assert_eq!(cpu_thread_budget(0, true, None), 0);
        // ...but with no GPU a tiny box still mines on what it has.
        assert_eq!(cpu_thread_budget(2, false, None), 2);
    }

    #[test]
    fn override_is_clamped_to_ceiling_never_exceeds() {
        // The "leave 2 free" rule is inviolable: even asking for 20 caps at 10.
        assert_eq!(cpu_thread_budget(12, true, Some(20)), 10);
        // No GPU: clamp to physical, not beyond.
        assert_eq!(cpu_thread_budget(12, false, Some(20)), 12);
    }

    #[test]
    fn override_can_request_fewer() {
        assert_eq!(cpu_thread_budget(12, true, Some(4)), 4);
        assert_eq!(cpu_thread_budget(12, false, Some(8)), 8);
        assert_eq!(cpu_thread_budget(12, true, Some(0)), 0); // user disables CPU
    }

    // ─────────────────────── FIX 3 — CPU-ONLY uses LOGICAL cores ────────────────

    /// CPU-only path: the budget is based on LOGICAL cores, not physical. On a
    /// rented box reporting 60 physical / 120 logical, a CPU-only miner must use
    /// all 120 logical cores (the old code clamped to ~60 physical → ran ~half).
    #[test]
    fn cpu_only_budget_uses_logical_cores() {
        // 120 logical cores, no override → use all 120 (NOT the ~60 physical).
        assert_eq!(cpu_only_thread_budget(120, None), 120);
        assert_eq!(cpu_only_thread_budget(6, None), 6);
        assert_eq!(cpu_only_thread_budget(1, None), 1);
        // Saturates to 0 on a 0-core report (caller logs nothing-to-mine).
        assert_eq!(cpu_only_thread_budget(0, None), 0);
    }

    /// CPU-only path: an explicit `--cpu-threads N` is honored UP TO logical —
    /// crucially it may ask for MORE than physical (the whole point of the fix),
    /// and is only clamped at the logical ceiling.
    #[test]
    fn cpu_only_override_honored_above_physical_up_to_logical() {
        // Box: 60 physical, 120 logical. Operator asks for 100 (> physical) → 100.
        assert_eq!(cpu_only_thread_budget(120, Some(100)), 100);
        // Asking for exactly logical is fine.
        assert_eq!(cpu_only_thread_budget(120, Some(120)), 120);
        // Asking ABOVE logical clamps to logical (don't oversubscribe wildly).
        assert_eq!(cpu_only_thread_budget(120, Some(500)), 120);
        // Asking for fewer is honored.
        assert_eq!(cpu_only_thread_budget(120, Some(8)), 8);
    }

    /// The GPU/hybrid path is UNCHANGED: still physical-minus-2, override clamped
    /// to that ceiling. (Same as `cpu_thread_budget(.., true, ..)`.)
    #[test]
    fn gpu_path_still_physical_minus_two() {
        // 60 physical, GPU active → 58, even if logical is 120.
        assert_eq!(cpu_thread_budget(60, true, None), 58);
        // Override above the physical-2 ceiling is still clamped down.
        assert_eq!(cpu_thread_budget(60, true, Some(120)), 58);
        // hybrid.rs:185 path: cpu_thread_budget(physical, true, None).
        assert_eq!(cpu_thread_budget(12, true, None), 10);
    }

    // ───────────── v0.1.9 NEVER-DARK — reduced CPU-fallback budget ─────────────

    /// The never-dark fallback budget is a small VISIBILITY trickle, not a CPU
    /// takeover: with no override it is min(2, logical). Rationale: a multi-GPU
    /// rig runs one process per card; if the GPU backend breaks on ALL of them,
    /// N full-width CPU miners would fight each other for the box. 2 threads per
    /// process keeps every worker connected + submitting without converting a
    /// rented GPU box into an oversubscribed CPU miner.
    #[test]
    fn fallback_budget_is_two_thread_trickle_by_default() {
        assert_eq!(cpu_fallback_thread_budget(128, None), 2);
        assert_eq!(cpu_fallback_thread_budget(8, None), 2);
        assert_eq!(cpu_fallback_thread_budget(2, None), 2);
        // Tiny boxes: never exceed logical.
        assert_eq!(cpu_fallback_thread_budget(1, None), 1);
        assert_eq!(cpu_fallback_thread_budget(0, None), 0);
    }

    /// An explicit `--cpu-threads N` overrides the trickle (the operator chose),
    /// clamped to the logical ceiling exactly like the CPU-only path — EXCEPT
    /// that never-dark is absolute: an explicit 0 is floored to 1 (v0.1.9 review
    /// fix — a 0-thread fallback would bail before connecting, recreating the
    /// invisible crash-loop this release exists to kill; `--strict-gpu` is the
    /// supported way to forbid CPU mining entirely).
    #[test]
    fn fallback_budget_honors_explicit_override_up_to_logical() {
        assert_eq!(cpu_fallback_thread_budget(128, Some(64)), 64);
        assert_eq!(cpu_fallback_thread_budget(128, Some(500)), 128); // clamp
        assert_eq!(cpu_fallback_thread_budget(128, Some(1)), 1); // fewer ok
        assert_eq!(cpu_fallback_thread_budget(128, Some(0)), 1); // floored: never dark
        assert_eq!(cpu_fallback_thread_budget(0, Some(0)), 0); // absurd 0-core box only
    }
}
