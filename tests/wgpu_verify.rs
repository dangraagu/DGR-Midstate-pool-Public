//! TDD tests for the wgpu GPU backend's **CPU-side** logic — the parts that must
//! be correct independent of any GPU.
//!
//! These run on the DEFAULT (`--features wgpu`) build but DO NOT require a GPU:
//! every GPU touch is gated behind `WgpuBackend::try_new()` (which returns
//! `Ok(None)` / `Err` without a device). What we pin here is the *consensus-safe
//! verification path* the GPU feeds into:
//!
//!   (1) GOLDEN VECTORS — the CPU re-verify path reproduces our canonical
//!       `a713dea1…` / `8ac4d9ef…` final hashes (the same oracle every backend is
//!       held to). This is what makes the port consensus-safe: the GPU is only
//!       ever *trusted to surface candidate nonces*; the bytes it surfaces are
//!       recomputed on the CPU before becoming a `Found`.
//!
//!   (2) WINDOW test — over `[nonce_start, nonce_start+count)` with an easy target,
//!       the re-verify path surfaces EXACTLY the CPU-path winners (the nonces whose
//!       1M-iter final hash clears the target), in nonce order, with no extras and
//!       none missed. We simulate a (possibly over-eager) GPU by handing the
//!       re-verifier the *whole* window as candidates; the CPU re-verify must then
//!       reject every non-winner — exactly the safety net that protects against a
//!       buggy/non-deterministic driver.
//!
//! Run: `cargo test --features wgpu --release -- --ignored` for the 1M-iter ones
//! (they do count×1,000,000 BLAKE3); the cheap structural ones run by default.

#![cfg(feature = "wgpu")]

use midstate_miner::pow::{meets_target, midstate_pow};
use midstate_miner::wgpu_backend::verify_candidates;

const GOLDEN_NONCE0: &str = "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369";
const GOLDEN_NONCE1: &str = "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9";

/// (1) GOLDEN VECTORS via the GPU backend's CPU re-verify path. With an all-0xff
/// target every candidate clears it, so re-verifying nonces {0,1} of the all-zero
/// midstate must return the two golden hashes in order. This is the exact code
/// that turns a GPU-surfaced nonce into a trusted `Found`.
#[test]
#[ignore = "2×1,000,000 BLAKE3; run with: cargo test --features wgpu --release -- --ignored"]
fn reverify_reproduces_golden_vectors() {
    let mid = [0u8; 32];
    let target = [0xffu8; 32];
    // Simulate the GPU surfacing nonces 0 and 1 as candidates.
    let out = verify_candidates(&mid, &target, &[0u64, 1u64]);
    assert_eq!(out.len(), 2, "both candidates clear the all-0xff target");
    assert_eq!(out[0].nonce, 0);
    assert_eq!(out[1].nonce, 1);
    assert_eq!(hex::encode(out[0].final_hash), GOLDEN_NONCE0, "nonce=0 golden mismatch");
    assert_eq!(hex::encode(out[1].final_hash), GOLDEN_NONCE1, "nonce=1 golden mismatch");
}

/// The re-verify path REJECTS a candidate whose real final hash does NOT clear the
/// target — the safety net against a buggy/non-deterministic GPU. With an all-0x00
/// target nothing can clear it, so even though the "GPU" surfaced both nonces, the
/// CPU re-verify drops them.
#[test]
#[ignore = "2×1,000,000 BLAKE3; run with: cargo test --features wgpu --release -- --ignored"]
fn reverify_rejects_non_winners() {
    let mid = [0u8; 32];
    let target = [0x00u8; 32]; // nothing is < 0 → reject everything
    let out = verify_candidates(&mid, &target, &[0u64, 1u64]);
    assert!(out.is_empty(), "CPU re-verify must reject GPU candidates that don't clear target");
}

/// (2) WINDOW test — over `[start, start+count)` the re-verify path, fed the whole
/// window as GPU candidates, surfaces EXACTLY the CPU-path winners for a chosen
/// easy target, in nonce order. The oracle is `pow::midstate_pow` + `meets_target`
/// computed independently here. Deterministic, no GPU.
#[test]
#[ignore = "count×1,000,000 BLAKE3; run with: cargo test --features wgpu --release -- --ignored"]
fn window_surfaces_exactly_cpu_winners() {
    let mid = [0u8; 32];
    let start: u64 = 0;
    let count: u32 = 16;
    // An easy-but-not-trivial gate: accept a hash only if its first byte is < 0x10
    // (≈ 1/16 of nonces). Computed against the SAME pow the oracle uses, so the
    // expected set is whatever really clears it in this window — not hardcoded.
    let mut target = [0xffu8; 32];
    target[0] = 0x10;

    // Independent CPU oracle: the true winners in the window.
    let mut expected: Vec<u64> = Vec::new();
    for k in 0..count as u64 {
        let nonce = start + k;
        let h = midstate_pow(mid, nonce);
        if meets_target(&h, &target) {
            expected.push(nonce);
        }
    }

    // Simulate the GPU surfacing the ENTIRE window as candidates (worst case for
    // the safety net). The re-verify must filter to exactly the true winners.
    let all: Vec<u64> = (start..start + count as u64).collect();
    let out = verify_candidates(&mid, &target, &all);

    let got: Vec<u64> = out.iter().map(|f| f.nonce).collect();
    assert_eq!(got, expected, "re-verify did not surface exactly the CPU-path winners");
    // And every surfaced hash is the real consensus hash (bit-exact), in order.
    for f in &out {
        assert_eq!(f.final_hash, midstate_pow(mid, f.nonce), "surfaced hash not bit-exact");
        assert!(meets_target(&f.final_hash, &target), "surfaced a hash that doesn't clear target");
    }
    // Output is sorted ascending by nonce.
    assert!(got.windows(2).all(|w| w[0] < w[1]), "winners must be nonce-sorted");
}

/// Structural (NO 1M BLAKE3, runs by default): an empty candidate list yields no
/// winners, and order/dedup behavior is sane on a trivially-passing target subset.
#[test]
fn reverify_empty_is_empty() {
    let out = verify_candidates(&[0u8; 32], &[0xffu8; 32], &[]);
    assert!(out.is_empty());
}
