//! Mining-mode selection — the pure decision of which backend(s) to run.
//!
//! The CLI exposes `--mode <cpu|gpu|hybrid|auto>` (default `auto`). This module
//! holds the *pure* logic that turns a requested [`Mode`] plus three runtime
//! facts — "was any GPU feature (cuda/wgpu/opencl) compiled in?", "is a usable
//! GPU present?", and "did the user pass `--strict-gpu`?" — into a concrete
//! [`Resolved`] decision. Keeping it pure (no clap, no driver calls) is what
//! lets the mode matrix be unit-tested exhaustively without a GPU.
//!
//! NEVER-DARK (v0.1.9): by default an unsatisfiable `gpu`/`hybrid` request
//! resolves to a LOUD CPU fallback instead of an error. Rationale: on the fleet,
//! a rig whose GPU backend breaks (driver rejects the PTX, self-test fails, bad
//! `--gpu-id`) used to exit BEFORE ever connecting to the pool — the launcher
//! then crash-looped it forever and the rig was INVISIBLE, indistinguishable
//! from powered-off. A visible, degraded CPU miner is strictly better than an
//! invisible dead one. `--strict-gpu` restores the old exit-with-error contract
//! for operators who never want CPU mining.
//!
//! The pool endpoint lock is unaffected: a mode only chooses *which hardware*
//! grinds the SAME compiled-in pool. There is no `--pool`/`--host` flag.

use std::str::FromStr;

/// What the user asked for on the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// CPU backend only.
    Cpu,
    /// GPU (OpenCL) backend only — an error if unavailable.
    Gpu,
    /// CPU + GPU concurrently (the [`HybridBackend`](crate::hybrid::HybridBackend)).
    Hybrid,
    /// Discover hardware and pick: hybrid if a usable GPU exists, else cpu.
    #[default]
    Auto,
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cpu" => Ok(Mode::Cpu),
            "gpu" => Ok(Mode::Gpu),
            "hybrid" => Ok(Mode::Hybrid),
            "auto" => Ok(Mode::Auto),
            other => Err(format!(
                "unknown mode '{other}' (expected one of: cpu, gpu, hybrid, auto)"
            )),
        }
    }
}

/// The concrete backend decision after applying the runtime facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Run the CPU backend alone (the normal cpu/auto path).
    Cpu,
    /// Run the GPU backend alone.
    Gpu,
    /// Run CPU + GPU concurrently.
    Hybrid,
    /// NEVER-DARK: the gpu/hybrid request cannot be satisfied, but instead of
    /// dying (and leaving the rig invisible to the pool while the launcher
    /// crash-loops it) we mine on CPU with a REDUCED thread budget and a loud
    /// warning. `&'static str` is the reason to surface. Default for
    /// unsatisfiable gpu/hybrid; suppressed by `--strict-gpu`.
    CpuFallback(&'static str),
    /// The request cannot be satisfied and `--strict-gpu` forbids the CPU
    /// fallback; `&'static str` explains why (the caller bails with it).
    Error(&'static str),
}

/// Resolve a requested [`Mode`] into a concrete [`Resolved`] decision.
///
/// Pure: the three booleans are the ONLY runtime inputs.
/// - `gpu_built`: was ANY GPU feature (cuda/wgpu/opencl) compiled in?
/// - `gpu_present`: did a GPU backend construct + pass its self-test?
///   (Always `false` when `!gpu_built`, but we don't rely on the caller for
///   that — we treat "no feature" as "no GPU possible" defensively.)
/// - `strict`: `--strict-gpu` — forbid the never-dark CPU fallback.
///
/// Matrix:
/// - `cpu`    → always [`Resolved::Cpu`].
/// - `auto`   → [`Resolved::Hybrid`] if built+present, else [`Resolved::Cpu`]
///   (auto NEVER errors — the CPU floor is its normal path, no warning needed).
/// - `gpu`    → [`Resolved::Gpu`] iff built+present; else
///   [`Resolved::CpuFallback`] (default) / [`Resolved::Error`] (strict).
/// - `hybrid` → [`Resolved::Hybrid`] iff built+present; else
///   [`Resolved::CpuFallback`] (default) / [`Resolved::Error`] (strict).
pub fn select_mode(requested: Mode, gpu_built: bool, gpu_present: bool, strict: bool) -> Resolved {
    let gpu_usable = gpu_built && gpu_present;
    // One reason string per unsatisfiable case, shared by both outcomes so the
    // strict error and the fallback warning always tell the same story.
    const GPU_NOT_BUILT: &str = "--mode gpu requires a GPU build (this is the CPU-only binary; \
         run the -gpu/-gpu-cuda binary)";
    const GPU_NO_DEVICE: &str = "--mode gpu: no usable GPU found \
         (no device / missing driver / failed init or self-test)";
    const HYB_NOT_BUILT: &str = "--mode hybrid requires a GPU build (this is the CPU-only binary; \
         run the -gpu/-gpu-cuda binary). Use --mode cpu here.";
    const HYB_NO_DEVICE: &str = "--mode hybrid: no usable GPU found \
         (no device / missing driver / failed init or self-test). Use --mode cpu here.";

    match requested {
        Mode::Cpu => Resolved::Cpu,
        Mode::Gpu => {
            if gpu_usable {
                Resolved::Gpu
            } else {
                let reason = if gpu_built { GPU_NO_DEVICE } else { GPU_NOT_BUILT };
                if strict {
                    Resolved::Error(reason)
                } else {
                    Resolved::CpuFallback(reason)
                }
            }
        }
        Mode::Hybrid => {
            if gpu_usable {
                Resolved::Hybrid
            } else {
                let reason = if gpu_built { HYB_NO_DEVICE } else { HYB_NOT_BUILT };
                if strict {
                    Resolved::Error(reason)
                } else {
                    Resolved::CpuFallback(reason)
                }
            }
        }
        Mode::Auto => {
            if gpu_usable {
                Resolved::Hybrid
            } else {
                Resolved::Cpu
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes_case_insensitive() {
        assert_eq!("cpu".parse::<Mode>().unwrap(), Mode::Cpu);
        assert_eq!("GPU".parse::<Mode>().unwrap(), Mode::Gpu);
        assert_eq!(" Hybrid ".parse::<Mode>().unwrap(), Mode::Hybrid);
        assert_eq!("auto".parse::<Mode>().unwrap(), Mode::Auto);
        assert!("nonsense".parse::<Mode>().is_err());
        assert_eq!(Mode::default(), Mode::Auto);
    }

    // ---- AUTO: picks hybrid when a usable GPU exists, else CPU; never errors,
    //      never falls back "loudly" (its CPU floor is the normal path) ---------
    #[test]
    fn auto_picks_hybrid_when_gpu_usable() {
        for &strict in &[false, true] {
            assert_eq!(
                select_mode(Mode::Auto, true, true, strict),
                Resolved::Hybrid,
                "auto + (gpu built, gpu present) must be hybrid (strict={strict})"
            );
        }
    }

    #[test]
    fn auto_falls_back_to_cpu_without_gpu() {
        for &strict in &[false, true] {
            // No feature at all.
            assert_eq!(select_mode(Mode::Auto, false, false, strict), Resolved::Cpu);
            // Feature built but no device (missing driver / no card).
            assert_eq!(select_mode(Mode::Auto, true, false, strict), Resolved::Cpu);
            // Defensive: gpu_present=true but feature not built is still no-GPU.
            assert_eq!(select_mode(Mode::Auto, false, true, strict), Resolved::Cpu);
        }
    }

    // ---- CPU: always CPU regardless of hardware or strictness ----
    #[test]
    fn cpu_is_always_cpu() {
        for &strict in &[false, true] {
            assert_eq!(select_mode(Mode::Cpu, true, true, strict), Resolved::Cpu);
            assert_eq!(select_mode(Mode::Cpu, false, false, strict), Resolved::Cpu);
        }
    }

    // ---- GPU: usable → Gpu. Unusable → NEVER-DARK default is a LOUD CPU
    //      fallback (the rig keeps mining + stays visible to the pool);
    //      --strict-gpu restores the old exit-with-error behavior. -------------
    #[test]
    fn gpu_usable_is_gpu_regardless_of_strict() {
        for &strict in &[false, true] {
            assert_eq!(select_mode(Mode::Gpu, true, true, strict), Resolved::Gpu);
        }
    }

    #[test]
    fn gpu_unusable_default_falls_back_to_cpu_loudly() {
        // Built but no device → CpuFallback carrying the device reason.
        match select_mode(Mode::Gpu, true, false, false) {
            Resolved::CpuFallback(m) => assert!(m.contains("no usable GPU")),
            other => panic!("expected CpuFallback, got {other:?}"),
        }
        // Not built → CpuFallback telling the user this is not a GPU build.
        match select_mode(Mode::Gpu, false, false, false) {
            Resolved::CpuFallback(m) => assert!(m.contains("GPU build")),
            other => panic!("expected CpuFallback, got {other:?}"),
        }
    }

    #[test]
    fn gpu_unusable_strict_errors() {
        match select_mode(Mode::Gpu, true, false, true) {
            Resolved::Error(m) => assert!(m.contains("no usable GPU")),
            other => panic!("expected Error, got {other:?}"),
        }
        match select_mode(Mode::Gpu, false, false, true) {
            Resolved::Error(m) => assert!(m.contains("GPU build")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---- HYBRID: same never-dark rule as GPU ----
    #[test]
    fn hybrid_usable_is_hybrid_regardless_of_strict() {
        for &strict in &[false, true] {
            assert_eq!(select_mode(Mode::Hybrid, true, true, strict), Resolved::Hybrid);
        }
    }

    #[test]
    fn hybrid_unusable_default_falls_back_to_cpu_loudly() {
        match select_mode(Mode::Hybrid, true, false, false) {
            Resolved::CpuFallback(m) => assert!(m.contains("no usable GPU")),
            other => panic!("expected CpuFallback, got {other:?}"),
        }
        match select_mode(Mode::Hybrid, false, false, false) {
            Resolved::CpuFallback(m) => assert!(m.contains("GPU build")),
            other => panic!("expected CpuFallback, got {other:?}"),
        }
    }

    #[test]
    fn hybrid_unusable_strict_errors() {
        match select_mode(Mode::Hybrid, true, false, true) {
            Resolved::Error(m) => assert!(m.contains("no usable GPU")),
            other => panic!("expected Error, got {other:?}"),
        }
        match select_mode(Mode::Hybrid, false, false, true) {
            Resolved::Error(m) => assert!(m.contains("GPU build")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Exhaustive 4(mode)×2(built)×2(present)×2(strict) matrix — every cell
    /// asserted exactly. THE never-dark invariant: with strict=false NO cell is
    /// `Error` (the process always has something to mine on), and the ONLY
    /// `Error` cells are gpu/hybrid-without-usable-GPU under strict=true.
    #[test]
    fn full_matrix_never_dark_unless_strict() {
        for &built in &[false, true] {
            for &present in &[false, true] {
                for &strict in &[false, true] {
                    let usable = built && present;

                    // cpu + auto: identical across strictness, never Error/Fallback.
                    assert_eq!(select_mode(Mode::Cpu, built, present, strict), Resolved::Cpu);
                    assert_eq!(
                        select_mode(Mode::Auto, built, present, strict),
                        if usable { Resolved::Hybrid } else { Resolved::Cpu }
                    );

                    let gpu = select_mode(Mode::Gpu, built, present, strict);
                    let hyb = select_mode(Mode::Hybrid, built, present, strict);
                    if usable {
                        assert_eq!(gpu, Resolved::Gpu);
                        assert_eq!(hyb, Resolved::Hybrid);
                    } else if strict {
                        assert!(matches!(gpu, Resolved::Error(_)), "strict gpu must Error");
                        assert!(matches!(hyb, Resolved::Error(_)), "strict hybrid must Error");
                    } else {
                        assert!(
                            matches!(gpu, Resolved::CpuFallback(_)),
                            "never-dark: default gpu w/o device must CpuFallback, got {gpu:?}"
                        );
                        assert!(
                            matches!(hyb, Resolved::CpuFallback(_)),
                            "never-dark: default hybrid w/o device must CpuFallback, got {hyb:?}"
                        );
                    }

                    // The absolute invariant: strict=false NEVER yields Error.
                    if !strict {
                        for m in [Mode::Cpu, Mode::Gpu, Mode::Hybrid, Mode::Auto] {
                            assert!(
                                !matches!(select_mode(m, built, present, false), Resolved::Error(_)),
                                "never-dark violated: {m:?} built={built} present={present} errored"
                            );
                        }
                    }
                }
            }
        }
    }
}
