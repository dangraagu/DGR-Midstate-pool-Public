//! The Midstate proof-of-work — pure Rust, bit-exact, consensus-critical.
//!
//! This is the reference implementation every backend (CPU AVX2, CUDA) MUST
//! reproduce bit-for-bit. It is the consensus contract made executable.
//!
//! Consensus contract:
//!   seed  x = BLAKE3( midstate[0..32] ++ nonce.to_le_bytes()[0..8] )  // 40-byte input
//!   chain for _ in 0..1_000_000 { x = BLAKE3(x) }                     // 32 -> 32
//!   final 32-byte x is compared big-endian against target / share_target.
//!
//! BLAKE3 is UNKEYED standard mode for every call (seed + all iters).
//!
//! Mirrors upstream `midstate/src/core/extension.rs` `create_extension` and
//! `EXTENSION_ITERATIONS = 1_000_000` (`types.rs`). Upstream's `fast-mining`
//! cargo feature swaps the count to 100 for tests — that produces INVALID
//! mainnet work. We NEVER do that here.

/// The mainnet sequential-hash iteration count. Consensus-critical: do not change.
pub const EXTENSION_ITERATIONS: u64 = 1_000_000;

/// Midstate PoW with an explicit iteration count.
///
/// Computes the seed hash from `midstate` + `nonce`, then applies `iters`
/// additional rounds of `x = BLAKE3(x)`. With `iters == 0` this returns the
/// bare seed hash; with `iters == EXTENSION_ITERATIONS` it returns the
/// consensus final hash.
#[inline]
pub fn midstate_pow_n(midstate: [u8; 32], nonce: u64, iters: u64) -> [u8; 32] {
    let mut data = [0u8; 40];
    data[..32].copy_from_slice(&midstate);
    data[32..].copy_from_slice(&nonce.to_le_bytes());
    let mut x = *blake3::hash(&data).as_bytes();
    for _ in 0..iters {
        x = *blake3::hash(&x).as_bytes();
    }
    x
}

/// Midstate PoW at the consensus iteration count (1,000,000).
///
/// This is the function whose output must match every accelerated backend for a
/// block to be valid on the network.
#[inline]
pub fn midstate_pow(midstate: [u8; 32], nonce: u64) -> [u8; 32] {
    midstate_pow_n(midstate, nonce, EXTENSION_ITERATIONS)
}

/// True if `final_hash` (big-endian) is strictly less than `target`.
/// `[u8; 32]` Ord is big-endian lexicographic, matching the consensus compare.
#[inline]
pub fn meets_target(final_hash: &[u8; 32], target: &[u8; 32]) -> bool {
    final_hash < target
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms the `blake3` crate is in UNKEYED standard mode (not keyed / KDF).
    /// The empty-input digest is a fixed, well-known vector.
    #[test]
    fn blake3_unkeyed_sanity() {
        let h = blake3::hash(b"");
        assert!(
            hex::encode(h.as_bytes()).starts_with("af1349b9"),
            "blake3 empty-input digest must start af1349b9 (unkeyed mode); got {}",
            hex::encode(h.as_bytes())
        );
    }

    /// Hand check at iters=1: the result must equal blake3(blake3(seed)) where the
    /// seed is the 40-byte (midstate ++ nonce_le) block, computed independently.
    #[test]
    fn small_iters_hand_check() {
        let midstate = [7u8; 32];
        let nonce: u64 = 42;

        let mut seed_input = [0u8; 40];
        seed_input[..32].copy_from_slice(&midstate);
        seed_input[32..].copy_from_slice(&nonce.to_le_bytes());
        let seed = *blake3::hash(&seed_input).as_bytes();
        let expected = *blake3::hash(&seed).as_bytes();

        assert_eq!(midstate_pow_n(midstate, nonce, 1), expected);
        assert_eq!(midstate_pow_n(midstate, nonce, 0), seed);
    }

    #[test]
    fn target_compare_is_big_endian() {
        let small = [0u8; 32];
        let mut big = [0u8; 32];
        big[0] = 1;
        assert!(meets_target(&small, &big));
        assert!(!meets_target(&big, &small));
        assert!(!meets_target(&big, &big)); // strict <
    }

    /// GOLDEN VECTORS — full 1,000,000-iteration consensus hashes for the all-zero
    /// midstate at nonce 0 and nonce 1. These exact bytes were captured by running
    /// the reference in release. Every accelerated backend must reproduce these.
    ///
    /// `#[ignore]` keeps the default `cargo test` fast (this does ~2M BLAKE3 calls).
    /// Run: `cargo test --release -- --ignored golden`.
    #[test]
    #[ignore = "1M-iter golden vector; run with: cargo test --release -- --ignored"]
    fn golden() {
        let g0 = midstate_pow([0u8; 32], 0);
        let g1 = midstate_pow([0u8; 32], 1);

        const GOLDEN_NONCE0: &str =
            "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369";
        const GOLDEN_NONCE1: &str =
            "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9";

        assert_eq!(hex::encode(g0), GOLDEN_NONCE0, "nonce=0 golden mismatch");
        assert_eq!(hex::encode(g1), GOLDEN_NONCE1, "nonce=1 golden mismatch");
    }
}
