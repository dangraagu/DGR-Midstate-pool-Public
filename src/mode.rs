//! Mining-mode selection — the pure decision of which backend(s) to run.
//!
//! The CLI exposes `--mode <cpu|gpu|hybrid|auto>` (default `auto`). This module
//! holds the *pure* logic that turns a requested [`Mode`] plus two runtime facts —
//! "was the `opencl` feature compiled in?" and "is a usable OpenCL GPU present?" —
//! into a concrete [`Resolved`] decision. Keeping it pure (no clap, no OpenCL
//! calls) is what lets the mode matrix be unit-tested exhaustively without a GPU.
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
    /// Run the CPU backend alone.
    Cpu,
    /// Run the GPU (OpenCL) backend alone.
    Gpu,
    /// Run CPU + GPU concurrently.
    Hybrid,
    /// The request cannot be satisfied; `&'static str` explains why (so the
    /// caller can bail with a clear, mode-specific message).
    Error(&'static str),
}

/// Resolve a requested [`Mode`] into a concrete [`Resolved`] decision.
///
/// Pure: the two booleans are the ONLY runtime inputs.
/// - `opencl_built`: was the crate compiled `--features opencl`?
/// - `gpu_present`: did `OpenClBackend::try_new()` find a usable device?
///   (Always `false` when `!opencl_built`, but we don't rely on the caller for
///   that — we treat "no feature" as "no GPU possible" defensively.)
///
/// Matrix:
/// - `cpu`    → always [`Resolved::Cpu`].
/// - `gpu`    → [`Resolved::Gpu`] iff built+present, else a specific [`Resolved::Error`].
/// - `hybrid` → [`Resolved::Hybrid`] iff built+present, else [`Resolved::Error`].
/// - `auto`   → [`Resolved::Hybrid`] if built+present, else [`Resolved::Cpu`]
///   (auto NEVER errors — it always has the CPU floor to fall back to).
pub fn select_mode(requested: Mode, opencl_built: bool, gpu_present: bool) -> Resolved {
    let gpu_usable = opencl_built && gpu_present;
    match requested {
        Mode::Cpu => Resolved::Cpu,
        Mode::Gpu => {
            if gpu_usable {
                Resolved::Gpu
            } else if !opencl_built {
                Resolved::Error(
                    "--mode gpu requires a GPU build (this is the CPU-only binary; \
                     run the -gpu binary or build --features opencl)",
                )
            } else {
                Resolved::Error(
                    "--mode gpu: no usable OpenCL GPU found (no device / missing ICD driver)",
                )
            }
        }
        Mode::Hybrid => {
            if gpu_usable {
                Resolved::Hybrid
            } else if !opencl_built {
                Resolved::Error(
                    "--mode hybrid requires a GPU build (this is the CPU-only binary; \
                     run the -gpu binary or build --features opencl). Use --mode cpu here.",
                )
            } else {
                Resolved::Error(
                    "--mode hybrid: no usable OpenCL GPU found (no device / missing ICD driver). \
                     Use --mode cpu here.",
                )
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

    // ---- AUTO: picks hybrid when a usable GPU exists, else CPU; never errors ----
    #[test]
    fn auto_picks_hybrid_when_gpu_usable() {
        assert_eq!(
            select_mode(Mode::Auto, true, true),
            Resolved::Hybrid,
            "auto + (opencl built, gpu present) must be hybrid"
        );
    }

    #[test]
    fn auto_falls_back_to_cpu_without_gpu() {
        // No feature at all.
        assert_eq!(select_mode(Mode::Auto, false, false), Resolved::Cpu);
        // Feature built but no device (missing ICD / no card).
        assert_eq!(select_mode(Mode::Auto, true, false), Resolved::Cpu);
        // Defensive: gpu_present=true but feature not built is still no-GPU.
        assert_eq!(select_mode(Mode::Auto, false, true), Resolved::Cpu);
    }

    // ---- CPU: always CPU regardless of hardware ----
    #[test]
    fn cpu_is_always_cpu() {
        assert_eq!(select_mode(Mode::Cpu, true, true), Resolved::Cpu);
        assert_eq!(select_mode(Mode::Cpu, false, false), Resolved::Cpu);
    }

    // ---- GPU: only when usable; otherwise a specific error (degrade gracefully) ----
    #[test]
    fn gpu_requires_usable_device() {
        assert_eq!(select_mode(Mode::Gpu, true, true), Resolved::Gpu);
        // Built but no device → error mentioning the missing device/ICD.
        match select_mode(Mode::Gpu, true, false) {
            Resolved::Error(m) => assert!(m.contains("no usable OpenCL GPU")),
            other => panic!("expected Error, got {other:?}"),
        }
        // Not built → error telling the user to use the -gpu binary.
        match select_mode(Mode::Gpu, false, false) {
            Resolved::Error(m) => assert!(m.contains("GPU build")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---- HYBRID: only when usable; otherwise error that points at --mode cpu ----
    #[test]
    fn hybrid_requires_usable_device() {
        assert_eq!(select_mode(Mode::Hybrid, true, true), Resolved::Hybrid);
        match select_mode(Mode::Hybrid, true, false) {
            Resolved::Error(m) => assert!(m.contains("no usable OpenCL GPU") && m.contains("cpu")),
            other => panic!("expected Error, got {other:?}"),
        }
        match select_mode(Mode::Hybrid, false, false) {
            Resolved::Error(m) => assert!(m.contains("GPU build")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Exhaustive 4×2×2 matrix sanity: auto + cpu NEVER error; the only Error
    /// outcomes are gpu/hybrid without a usable GPU.
    #[test]
    fn full_matrix_error_only_for_gpu_hybrid_without_device() {
        for &built in &[false, true] {
            for &present in &[false, true] {
                assert_ne!(select_mode(Mode::Cpu, built, present), Resolved::Error(""));
                assert_ne!(select_mode(Mode::Auto, built, present), Resolved::Error(""));
                let usable = built && present;
                let gpu = select_mode(Mode::Gpu, built, present);
                let hyb = select_mode(Mode::Hybrid, built, present);
                if usable {
                    assert_eq!(gpu, Resolved::Gpu);
                    assert_eq!(hyb, Resolved::Hybrid);
                } else {
                    assert!(matches!(gpu, Resolved::Error(_)));
                    assert!(matches!(hyb, Resolved::Error(_)));
                }
            }
        }
    }
}
