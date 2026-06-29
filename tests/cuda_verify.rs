//! TDD tests for the CUDA GPU backend.
//!
//! Two layers, matching `tests/wgpu_verify.rs`:
//!
//!   (A) CPU-side re-verify path (NO GPU needed): the consensus-safe net the CUDA
//!       kernel feeds into. The GPU is only ever *trusted to surface candidate
//!       nonces*; the bytes that become a `Found` are recomputed on the CPU first.
//!       These reproduce the canonical golden vectors and reject non-winners.
//!
//!   (B) End-to-end via the REAL CUDA backend (needs an NVIDIA driver + device):
//!       reproduce the golden vectors through `CudaBackend::search` with an
//!       all-0xff target (every nonce surfaces), and confirm a non-winner target
//!       (all-0x00) yields nothing. These are `#[ignore]` (heavy + need a GPU);
//!       run with `cargo test --features cuda --release -- --ignored`.

#![cfg(feature = "cuda")]

use midstate_miner::cuda_backend::{verify_candidates, CudaBackend};
use midstate_miner::pow::{meets_target, midstate_pow};
use midstate_miner::Backend; // brings `search` into scope for the end-to-end tests
use std::sync::Mutex;

/// Serialize the GPU-touching end-to-end tests. `cargo test` runs tests in
/// parallel by default; several concurrent CUDA contexts each launching the FULL
/// 1,000,000-iteration chain on ONE physical GPU contend for the device and can
/// corrupt each other's results (a harness artifact — the kernel itself is
/// bit-exact, proven by these same tests when run serially). Holding this lock for
/// the whole GPU section makes the suite pass at any `--test-threads`.
static GPU_LOCK: Mutex<()> = Mutex::new(());

const GOLDEN_NONCE0: &str = "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369";
const GOLDEN_NONCE1: &str = "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9";

// ── (A) CPU re-verify path — no GPU ────────────────────────────────────────────

/// GOLDEN VECTORS via the CUDA backend's CPU re-verify path. With an all-0xff
/// target every candidate clears it, so re-verifying nonces {0,1} of the all-zero
/// midstate returns the two golden hashes in order — the exact code that turns a
/// GPU-surfaced nonce into a trusted `Found`.
#[test]
#[ignore = "2×1,000,000 BLAKE3; run with: cargo test --features cuda --release -- --ignored"]
fn reverify_reproduces_golden_vectors() {
    let mid = [0u8; 32];
    let target = [0xffu8; 32];
    let out = verify_candidates(&mid, &target, &[0u64, 1u64]);
    assert_eq!(out.len(), 2, "both candidates clear the all-0xff target");
    assert_eq!(out[0].nonce, 0);
    assert_eq!(out[1].nonce, 1);
    assert_eq!(hex::encode(out[0].final_hash), GOLDEN_NONCE0, "nonce=0 golden mismatch");
    assert_eq!(hex::encode(out[1].final_hash), GOLDEN_NONCE1, "nonce=1 golden mismatch");
}

/// The re-verify path REJECTS a candidate whose real final hash does NOT clear the
/// target — the safety net against a buggy/non-deterministic GPU.
#[test]
#[ignore = "2×1,000,000 BLAKE3; run with: cargo test --features cuda --release -- --ignored"]
fn reverify_rejects_non_winners() {
    let mid = [0u8; 32];
    let target = [0x00u8; 32]; // nothing is < 0 → reject everything
    let out = verify_candidates(&mid, &target, &[0u64, 1u64]);
    assert!(out.is_empty(), "CPU re-verify must reject candidates that don't clear target");
}

/// Structural (NO 1M BLAKE3, runs by default): empty candidate list → no winners.
#[test]
fn reverify_empty_is_empty() {
    let out = verify_candidates(&[0u8; 32], &[0xffu8; 32], &[]);
    assert!(out.is_empty());
}

// ── (B) End-to-end via the real CUDA device ─────────────────────────────────────

/// GOLDEN VECTORS reproduced by the REAL CUDA kernel. `search` over the all-zero
/// midstate with an all-0xff target surfaces every nonce; CPU re-verify keeps them
/// and the bytes equal the canonical golden hashes. Needs an NVIDIA driver+device.
#[test]
#[ignore = "needs an NVIDIA CUDA device + the cuda feature; run with --release --ignored"]
fn cuda_search_reproduces_golden_vectors() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = CudaBackend::try_new(Some(0), None)
        .expect("cuda try_new")
        .expect("a CUDA device");
    let target = [0xffu8; 32]; // every nonce clears it
    let found = b.search(&[0u8; 32], &target, 0, 2).unwrap();
    assert_eq!(found.len(), 2, "both nonces in the window must surface");
    assert_eq!(found[0].nonce, 0);
    assert_eq!(found[1].nonce, 1);
    assert_eq!(hex::encode(found[0].final_hash), GOLDEN_NONCE0, "nonce=0 golden mismatch");
    assert_eq!(hex::encode(found[1].final_hash), GOLDEN_NONCE1, "nonce=1 golden mismatch");
}

/// The CUDA backend surfaces NOTHING for an impossible (all-0x00) target — both
/// the kernel's `meets_target` and the CPU re-verify reject every non-winner.
#[test]
#[ignore = "needs an NVIDIA CUDA device + the cuda feature; run with --release --ignored"]
fn cuda_search_rejects_non_winners() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = CudaBackend::try_new(Some(0), None)
        .expect("cuda try_new")
        .expect("a CUDA device");
    let target = [0x00u8; 32]; // nothing is < 0
    let found = b.search(&[0u8; 32], &target, 0, 16).unwrap();
    assert!(found.is_empty(), "no nonce can clear an all-0x00 target");
}

/// WINDOW test on the real device — over `[0,count)` with an easy-but-not-trivial
/// target, the backend surfaces EXACTLY the CPU-oracle winners, in nonce order,
/// every surfaced hash bit-exact. This exercises the kernel's atomic result
/// collection + the CPU re-verify together.
#[test]
#[ignore = "needs an NVIDIA CUDA device + the cuda feature; run with --release --ignored"]
fn cuda_window_surfaces_exactly_cpu_winners() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = CudaBackend::try_new(Some(0), None)
        .expect("cuda try_new")
        .expect("a CUDA device");
    let mid = [0u8; 32];
    let count: u32 = 32;
    // Accept a hash only if its first byte is < 0x10 (≈ 1/16 of nonces).
    let mut target = [0xffu8; 32];
    target[0] = 0x10;

    // Independent CPU oracle.
    let mut expected: Vec<u64> = Vec::new();
    for k in 0..count as u64 {
        let h = midstate_pow(mid, k);
        if meets_target(&h, &target) {
            expected.push(k);
        }
    }

    let found = b.search(&mid, &target, 0, count).unwrap();
    let got: Vec<u64> = found.iter().map(|f| f.nonce).collect();
    assert_eq!(got, expected, "CUDA did not surface exactly the CPU-path winners");
    for f in &found {
        assert_eq!(f.final_hash, midstate_pow(mid, f.nonce), "surfaced hash not bit-exact");
        assert!(meets_target(&f.final_hash, &target), "surfaced a hash that doesn't clear target");
    }
    assert!(got.windows(2).all(|w| w[0] < w[1]), "winners must be nonce-sorted");
}
