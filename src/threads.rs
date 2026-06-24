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
}
