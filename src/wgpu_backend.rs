//! wgpu / WGSL GPU mining backend — ported from upstream `ciphernom/midstate`
//! (`src/core/gpu_mining.rs`) and adapted to our [`Backend`] contract.
//!
//! ## What we lifted verbatim (consensus-identical)
//! The WGSL [`SHADER`] is copied byte-for-byte from upstream. Its PoW is
//! bit-identical to our reference [`crate::pow::midstate_pow`]:
//!   - `first_compress`: `blake3(midstate ++ nonce_le8)` over a **40-byte** block,
//!   - `iterate`: `blake3(x)` over a **32-byte** block, FLAGS = `0x0b`
//!     (`CHUNK_START | CHUNK_END | ROOT`),
//!   - repeated `EXTENSION_ITERATIONS` (1,000,000) times,
//!   - big-endian compare of the final 32 bytes against the target.
//!
//! The host [`pick_adapter`], the **checkpointed** dispatch loop (`k_init` → many
//! `k_step` of ~2000 iters each → `k_test`), the readback, and the boot
//! [`WgpuBackend::self_test`] are all ported from upstream.
//!
//! ## What we ADAPTED to our contract
//! Upstream's `mine_gpu` owns its own nonce cursor: it picks a **random** base,
//! loops **forever** until it finds a hit, supports a *second* `pool_target` tier,
//! and **stashes** surplus winners. Our [`crate::client`] owns the cursor and
//! re-calls `search(midstate, target, nonce_start, count)` itself, so we DROP all
//! of that. This backend searches **exactly** `[nonce_start, nonce_start + count)`,
//! has a **single** target tier, and returns **every** [`Found`] winner in that
//! window (no random base, no infinite loop, no stash). Larger-than-buffer windows
//! are chunked internally.
//!
//! ## Why this is safe (the same net upstream has)
//! The kernel is trusted only to *surface candidate nonces*. Every candidate is
//! recomputed on the CPU with [`crate::pow::midstate_pow`] and re-checked against
//! the target ([`verify_candidates`]) before becoming a `Found`, so a buggy or
//! non-deterministic driver can never produce an invalid share — only cost
//! throughput. And [`WgpuBackend::self_test`] runs the **full 1,000,000-iter**
//! chain on fixed nonces at startup and refuses to mine unless the GPU output
//! equals the CPU reference bit-for-bit.
//!
//! Build with `--features wgpu`. The default build excludes this module entirely
//! (toolkit-free, Defender-safe fleet rule). On a box with no Vulkan/DX12/Metal
//! GPU, [`WgpuBackend::try_new`] returns `Ok(None)` so the caller falls back to CPU.

use crate::backend::{Backend, Found};
use crate::pow::{meets_target, midstate_pow};
use anyhow::{anyhow, bail, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Our consensus iteration count. Sourced from `pow` so there is one definition.
use crate::pow::EXTENSION_ITERATIONS;

// ── Tunables ──────────────────────────────────────────────────────────────────
//
// The number of nonces ground per GPU launch. This sizes the on-device `state`
// buffer (BATCH_NONCES × 32 bytes) allocated once at construction. The client's
// `suggested_batch()` returns this so windows fit in one launch; larger windows
// are chunked. State buffer at the default = 8192 × 32 B = 256 KiB.
const BATCH_NONCES: u32 = 1 << 13; // 8,192

/// Chain steps applied per GPU dispatch. Higher = fewer host↔GPU round-trips but
/// each dispatch holds the GPU longer (OS watchdog / TDR exposure). 2000 ×
/// (one 32-byte BLAKE3) is well under the ~2s desktop watchdog on any real GPU.
const ITERS_PER_DISPATCH: u32 = 2_000;

const MAX_WINNERS: u32 = 256;
const WINNERS_BYTES: u64 = 16 + (MAX_WINNERS as u64) * 4 * 3;
const SELFTEST_N: u32 = 8;

// ─────────────────────────────────────────────────────────────────────────────
//  CONSENSUS-CRITICAL WGSL — lifted VERBATIM from upstream gpu_mining.rs.
//  Do not edit. Any change must keep `self_test` + golden vectors bit-exact.
//  (Our PoW uses no `pool_target`, so we always pass `has_pool = 0`; the `pool`
//  branch in `k_test` is simply never taken.)
// ─────────────────────────────────────────────────────────────────────────────
const SHADER: &str = r#"// BLAKE3 mining kernel for the PoW extension chain.
//
// CONSENSUS-CRITICAL: every value below mirrors `create_extension` /
// `simd_mining::compress_4way` exactly. The per-nonce result MUST be
// bit-identical to the scalar reference or the miner produces rejected
// blocks/shares. The compression body is machine-generated from the same
// MSG_SCHEDULE the CPU path uses.
//
//   per nonce:  h = blake3_40(midstate || nonce_le8)        // block_len = 40
//               repeat EXTENSION_ITERATIONS:  h = blake3_32(h)  // block_len = 32
//   the chaining value fed into every compression is the IV (each step is a
//   fresh BLAKE3 of a 32-byte input, NOT a running hash).

const IV0: u32 = 0x6A09E667u;
const IV1: u32 = 0xBB67AE85u;
const IV2: u32 = 0x3C6EF372u;
const IV3: u32 = 0xA54FF53Au;
const IV4: u32 = 0x510E527Fu;
const IV5: u32 = 0x9B05688Cu;
const IV6: u32 = 0x1F83D9ABu;
const IV7: u32 = 0x5BE0CD19u;
const FLAGS: u32 = 11u; // CHUNK_START | CHUNK_END | ROOT  (1 | 2 | 8)

struct Params {
    midstate: array<u32, 8>,  // u32::from_le_bytes of each 4-byte group of the 32-byte midstate
    tgt:      array<u32, 8>,  // from_be_bytes of each 4-byte group of the 32-byte target ('target' is a WGSL reserved word)
    pool:     array<u32, 8>,  // same encoding as target; used only when has_pool != 0
    base_lo:  u32,
    base_hi:  u32,
    n_nonces: u32,
    iters:    u32,            // k_step: how many 32-byte iterations to apply this dispatch
    has_pool: u32,
    pad0: u32, pad1: u32, pad2: u32,
};

struct Winners {
    count: atomic<u32>,
    cap: u32,
    pad0: u32, pad1: u32,
    nonce_lo: array<u32, 256>,
    nonce_hi: array<u32, 256>,
    kind:     array<u32, 256>,   // 0 = block, 1 = share
};

@group(0) @binding(0) var<storage, read>       P:     Params;
@group(0) @binding(1) var<storage, read_write> state: array<u32>;   // n_nonces * 8 chaining words
@group(0) @binding(2) var<storage, read_write> out:   Winners;

fn rotr(x: u32, n: u32) -> u32 {
    return (x >> n) | (x << (32u - n));
}

// Reverse the 4 bytes of a word: turns a little-endian hash word into the
// big-endian key whose numeric order matches the [u8;32] lexicographic order.
fn bswap(x: u32) -> u32 {
    return ((x & 0xFFu) << 24u) | ((x & 0xFF00u) << 8u) | ((x >> 8u) & 0xFF00u) | ((x >> 24u) & 0xFFu);
}

fn compress(m: array<u32,16>, block_len: u32) -> array<u32,8> {
    var v0 = IV0; var v1 = IV1; var v2 = IV2; var v3 = IV3;
    var v4 = IV4; var v5 = IV5; var v6 = IV6; var v7 = IV7;
    var v8 = IV0; var v9 = IV1; var v10 = IV2; var v11 = IV3;
    var v12 = 0u; var v13 = 0u; var v14 = block_len; var v15 = FLAGS;
  // round 0
  v0 = v0 + v4 + m[0]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[1]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[2]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[3]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[4]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[5]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[6]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[7]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[8]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[9]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[10]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[11]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[12]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[13]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[14]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[15]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 1
  v0 = v0 + v4 + m[2]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[6]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[3]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[10]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[7]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[0]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[4]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[13]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[1]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[11]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[12]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[5]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[9]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[14]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[15]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[8]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 2
  v0 = v0 + v4 + m[3]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[4]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[10]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[12]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[13]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[2]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[7]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[14]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[6]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[5]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[9]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[0]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[11]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[15]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[8]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[1]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 3
  v0 = v0 + v4 + m[10]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[7]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[12]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[9]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[14]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[3]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[13]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[15]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[4]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[0]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[11]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[2]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[5]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[8]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[1]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[6]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 4
  v0 = v0 + v4 + m[12]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[13]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[9]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[11]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[15]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[10]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[14]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[8]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[7]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[2]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[5]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[3]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[0]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[1]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[6]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[4]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 5
  v0 = v0 + v4 + m[9]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[14]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[11]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[5]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[8]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[12]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[15]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[1]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[13]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[3]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[0]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[10]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[2]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[6]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[4]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[7]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
  // round 6
  v0 = v0 + v4 + m[11]; v12 = rotr(v12 ^ v0, 16u); v8 = v8 + v12; v4 = rotr(v4 ^ v8, 12u);
  v0 = v0 + v4 + m[15]; v12 = rotr(v12 ^ v0, 8u);  v8 = v8 + v12; v4 = rotr(v4 ^ v8, 7u);
  v1 = v1 + v5 + m[5]; v13 = rotr(v13 ^ v1, 16u); v9 = v9 + v13; v5 = rotr(v5 ^ v9, 12u);
  v1 = v1 + v5 + m[0]; v13 = rotr(v13 ^ v1, 8u);  v9 = v9 + v13; v5 = rotr(v5 ^ v9, 7u);
  v2 = v2 + v6 + m[1]; v14 = rotr(v14 ^ v2, 16u); v10 = v10 + v14; v6 = rotr(v6 ^ v10, 12u);
  v2 = v2 + v6 + m[9]; v14 = rotr(v14 ^ v2, 8u);  v10 = v10 + v14; v6 = rotr(v6 ^ v10, 7u);
  v3 = v3 + v7 + m[8]; v15 = rotr(v15 ^ v3, 16u); v11 = v11 + v15; v7 = rotr(v7 ^ v11, 12u);
  v3 = v3 + v7 + m[6]; v15 = rotr(v15 ^ v3, 8u);  v11 = v11 + v15; v7 = rotr(v7 ^ v11, 7u);
  v0 = v0 + v5 + m[14]; v15 = rotr(v15 ^ v0, 16u); v10 = v10 + v15; v5 = rotr(v5 ^ v10, 12u);
  v0 = v0 + v5 + m[10]; v15 = rotr(v15 ^ v0, 8u);  v10 = v10 + v15; v5 = rotr(v5 ^ v10, 7u);
  v1 = v1 + v6 + m[2]; v12 = rotr(v12 ^ v1, 16u); v11 = v11 + v12; v6 = rotr(v6 ^ v11, 12u);
  v1 = v1 + v6 + m[12]; v12 = rotr(v12 ^ v1, 8u);  v11 = v11 + v12; v6 = rotr(v6 ^ v11, 7u);
  v2 = v2 + v7 + m[3]; v13 = rotr(v13 ^ v2, 16u); v8 = v8 + v13; v7 = rotr(v7 ^ v8, 12u);
  v2 = v2 + v7 + m[4]; v13 = rotr(v13 ^ v2, 8u);  v8 = v8 + v13; v7 = rotr(v7 ^ v8, 7u);
  v3 = v3 + v4 + m[7]; v14 = rotr(v14 ^ v3, 16u); v9 = v9 + v14; v4 = rotr(v4 ^ v9, 12u);
  v3 = v3 + v4 + m[13]; v14 = rotr(v14 ^ v3, 8u);  v9 = v9 + v14; v4 = rotr(v4 ^ v9, 7u);
    return array<u32,8>(v0 ^ v8, v1 ^ v9, v2 ^ v10, v3 ^ v11, v4 ^ v12, v5 ^ v13, v6 ^ v14, v7 ^ v15);
}

fn nonce_for(gid: u32) -> vec2<u32> {
    let lo = P.base_lo + gid;          // gid < n_nonces <= 2^18, so at most one carry
    var carry = 0u;
    if (lo < P.base_lo) { carry = 1u; }
    let hi = P.base_hi + carry;
    return vec2<u32>(lo, hi);
}

fn first_compress(gid: u32) -> array<u32,8> {
    var m: array<u32,16>;
    m[0] = P.midstate[0]; m[1] = P.midstate[1]; m[2] = P.midstate[2]; m[3] = P.midstate[3];
    m[4] = P.midstate[4]; m[5] = P.midstate[5]; m[6] = P.midstate[6]; m[7] = P.midstate[7];
    let n = nonce_for(gid);
    m[8] = n.x; m[9] = n.y;
    m[10] = 0u; m[11] = 0u; m[12] = 0u; m[13] = 0u; m[14] = 0u; m[15] = 0u;
    return compress(m, 40u);
}

fn iterate(h: array<u32,8>) -> array<u32,8> {
    var m: array<u32,16>;
    m[0] = h[0]; m[1] = h[1]; m[2] = h[2]; m[3] = h[3];
    m[4] = h[4]; m[5] = h[5]; m[6] = h[6]; m[7] = h[7];
    m[8] = 0u; m[9] = 0u; m[10] = 0u; m[11] = 0u; m[12] = 0u; m[13] = 0u; m[14] = 0u; m[15] = 0u;
    return compress(m, 32u);
}

// final_hash[u8;32] < ref ?  (byte 0 most significant), unrolled to avoid
// dynamic indexing into value arrays.
fn lt8(h: array<u32,8>, r: array<u32,8>) -> bool {
    var k: u32;
    k = bswap(h[0]); if (k < r[0]) { return true; } if (k > r[0]) { return false; }
    k = bswap(h[1]); if (k < r[1]) { return true; } if (k > r[1]) { return false; }
    k = bswap(h[2]); if (k < r[2]) { return true; } if (k > r[2]) { return false; }
    k = bswap(h[3]); if (k < r[3]) { return true; } if (k > r[3]) { return false; }
    k = bswap(h[4]); if (k < r[4]) { return true; } if (k > r[4]) { return false; }
    k = bswap(h[5]); if (k < r[5]) { return true; } if (k > r[5]) { return false; }
    k = bswap(h[6]); if (k < r[6]) { return true; } if (k > r[6]) { return false; }
    k = bswap(h[7]); if (k < r[7]) { return true; } if (k > r[7]) { return false; }
    return false;
}

fn load_state(gid: u32) -> array<u32,8> {
    let b = gid * 8u;
    var h: array<u32,8>;
    h[0] = state[b + 0u]; h[1] = state[b + 1u]; h[2] = state[b + 2u]; h[3] = state[b + 3u];
    h[4] = state[b + 4u]; h[5] = state[b + 5u]; h[6] = state[b + 6u]; h[7] = state[b + 7u];
    return h;
}

fn store_state(gid: u32, h: array<u32,8>) {
    let b = gid * 8u;
    state[b + 0u] = h[0]; state[b + 1u] = h[1]; state[b + 2u] = h[2]; state[b + 3u] = h[3];
    state[b + 4u] = h[4]; state[b + 5u] = h[5]; state[b + 6u] = h[6]; state[b + 7u] = h[7];
}

@compute @workgroup_size(64)
fn k_init(@builtin(global_invocation_id) gid3: vec3<u32>) {
    let gid = gid3.x;
    if (gid >= P.n_nonces) { return; }
    store_state(gid, first_compress(gid));
}

@compute @workgroup_size(64)
fn k_step(@builtin(global_invocation_id) gid3: vec3<u32>) {
    let gid = gid3.x;
    if (gid >= P.n_nonces) { return; }
    var h = load_state(gid);
    for (var i = 0u; i < P.iters; i = i + 1u) {
        h = iterate(h);
    }
    store_state(gid, h);
}

@compute @workgroup_size(64)
fn k_test(@builtin(global_invocation_id) gid3: vec3<u32>) {
    let gid = gid3.x;
    if (gid >= P.n_nonces) { return; }
    let h = load_state(gid);
    var kind = 0xFFFFFFFFu;
    if (lt8(h, P.tgt)) {
        kind = 0u;
    } else if (P.has_pool != 0u && lt8(h, P.pool)) {
        kind = 1u;
    }
    if (kind != 0xFFFFFFFFu) {
        let idx = atomicAdd(&out.count, 1u);
        if (idx < out.cap) {
            let n = nonce_for(gid);
            out.nonce_lo[idx] = n.x;
            out.nonce_hi[idx] = n.y;
            out.kind[idx] = kind;
        }
    }
}
"#;

// ── Param block mirrored 1:1 by the WGSL `Params` struct (std430, 128 bytes) ──

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    midstate: [u32; 8],
    target: [u32; 8],
    pool: [u32; 8],
    base_lo: u32,
    base_hi: u32,
    n_nonces: u32,
    iters: u32,
    has_pool: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}
const ITERS_FIELD_OFFSET: u64 = 96 + 3 * 4; // byte offset of `iters` within Params

fn words_le(b: &[u8; 32]) -> [u32; 8] {
    let mut w = [0u32; 8];
    for i in 0..8 {
        w[i] = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    }
    w
}

fn words_be(b: &[u8; 32]) -> [u32; 8] {
    let mut w = [0u32; 8];
    for i in 0..8 {
        w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    }
    w
}

// ─────────────────────────────────────────────────────────────────────────────
//  CONSENSUS SAFETY NET — the CPU re-verify path (GPU-free, fully unit-tested).
// ─────────────────────────────────────────────────────────────────────────────

/// Recompute the consensus PoW for each GPU-surfaced candidate nonce on the CPU
/// and keep only those whose real final hash clears `target`. This is the SAME
/// safety net upstream's `mine_gpu` has: the GPU is trusted only to *surface*
/// candidates; the bytes that become a [`Found`] are always the CPU's, so a buggy
/// or non-deterministic driver can never produce an invalid share — only waste a
/// little CPU on a false positive. Output is sorted ascending by nonce and
/// de-duplicated. PURE — no GPU, no I/O — which is why it can be golden-tested.
pub fn verify_candidates(midstate: &[u8; 32], target: &[u8; 32], candidates: &[u64]) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for &nonce in candidates {
        if !seen.insert(nonce) {
            continue; // a candidate surfaced twice — verify once
        }
        let final_hash = midstate_pow(*midstate, nonce);
        if meets_target(&final_hash, target) {
            out.push(Found { nonce, final_hash });
        }
    }
    out.sort_by_key(|f| f.nonce);
    out
}

// ── Adapter selection (ported from upstream `pick_adapter`) ───────────────────

/// One display line for an adapter: `index: name [device_type] (backend)`.
fn adapter_line(i: usize, a: &wgpu::Adapter) -> String {
    let info = a.get_info();
    format!("{i}: {} [{:?}] ({:?})", info.name, info.device_type, info.backend)
}

/// Format the full adapter list as `index: name [device_type] (backend)` lines,
/// one per line. Used for the error message when an explicit `--gpu-id` is out of
/// range.
fn adapter_list_string(adapters: &[wgpu::Adapter]) -> String {
    adapters
        .iter()
        .enumerate()
        .map(|(i, a)| adapter_line(i, a))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Enumerate every GPU adapter across all backends and return one display line per
/// adapter (`index: name [device_type] (backend)`). Owns its own `Instance` so it
/// can be called standalone for `--list-gpus` without constructing a backend.
/// Returns an empty Vec if no adapters are present.
pub fn list_adapters() -> Vec<String> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    adapters
        .iter()
        .enumerate()
        .map(|(i, a)| adapter_line(i, a))
        .collect()
}

/// Choose the GPU adapter to mine on. Enumerates all adapters across all backends.
///
/// When `gpu_id` is `Some(idx)` the caller has EXPLICITLY pinned a device: select
/// `adapters[idx]` LITERALLY (no software-skip, no ranking — the boot `self_test`
/// still gates whatever is picked), and bail with the full adapter list if `idx`
/// is out of range so a typo fails loudly instead of silently mining the wrong card.
///
/// When `gpu_id` is `None` (auto), honor a `WGPU_ADAPTER_NAME` case-insensitive
/// substring override, otherwise drop pure-software adapters and rank
/// discrete > integrated > virtual, preferring Vulkan > Dx12 > Metal > GL within a
/// tier. Errors (→ CPU fallback) if nothing usable is found.
async fn pick_adapter(instance: &wgpu::Instance, gpu_id: Option<usize>) -> Result<wgpu::Adapter> {
    let mut adapters = instance.enumerate_adapters(wgpu::Backends::all()).await;
    if adapters.is_empty() {
        bail!(
            "no GPU adapters found. wgpu uses Vulkan/DX12/Metal/GL, not CUDA — an NVIDIA \
             card needs its Vulkan ICD installed (verify with `vulkaninfo --summary`)."
        );
    }

    // Explicit --gpu-id: literal index into the enumerated list (no skipping).
    if let Some(idx) = gpu_id {
        if idx >= adapters.len() {
            bail!(
                "--gpu-id {idx} is out of range: only {} GPU adapter(s) found:\n{}",
                adapters.len(),
                adapter_list_string(&adapters)
            );
        }
        return Ok(adapters.swap_remove(idx));
    }

    let name_pref = std::env::var("WGPU_ADAPTER_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if let Some(want) = name_pref {
        let want_lc = want.to_lowercase();
        if let Some(pos) = adapters
            .iter()
            .position(|a| a.get_info().name.to_lowercase().contains(&want_lc))
        {
            return Ok(adapters.swap_remove(pos));
        }
    }

    // Drop software adapters — the real CPU miner beats llvmpipe etc.
    adapters.retain(|a| a.get_info().device_type != wgpu::DeviceType::Cpu);
    if adapters.is_empty() {
        bail!("only software (CPU) GPU adapters available; using the CPU miner instead");
    }

    adapters.sort_by_key(|a| {
        let i = a.get_info();
        let type_rank = match i.device_type {
            wgpu::DeviceType::DiscreteGpu => 0u8,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            _ => 3,
        };
        let backend_rank = match i.backend {
            wgpu::Backend::Vulkan => 0u8,
            wgpu::Backend::Dx12 => 1,
            wgpu::Backend::Metal => 2,
            wgpu::Backend::Gl => 3,
            _ => 4,
        };
        (type_rank, backend_rank)
    });
    Ok(adapters.into_iter().next().unwrap())
}

// ── The GPU backend ───────────────────────────────────────────────────────────

/// wgpu/WGSL GPU mining backend implementing our [`Backend`] trait.
pub struct WgpuBackend {
    name: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipe_init: wgpu::ComputePipeline,
    pipe_step: wgpu::ComputePipeline,
    pipe_test: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params_buf: wgpu::Buffer,
    state_buf: wgpu::Buffer,
    winners_buf: wgpu::Buffer,
    readback_buf: wgpu::Buffer,
}

impl WgpuBackend {
    /// Production entry: build + self-test the GPU. `Ok(None)` if there is no
    /// usable (non-software) adapter, `Err` only on a hard device/shader failure
    /// (the caller treats both as "no GPU" and falls back to CPU). On success the
    /// returned backend has ALREADY passed [`self_test`] — it is consensus-safe to
    /// mine with or this returns `Err`.
    pub fn try_new(gpu_id: Option<usize>) -> Result<Option<Self>> {
        match pollster::block_on(Self::new_async(gpu_id)) {
            Ok(b) => {
                b.self_test()?; // refuse to mine unless bit-exact vs the CPU reference
                Ok(Some(b))
            }
            Err(e) => {
                // "no adapters" / "only software" are the expected no-GPU paths.
                let msg = e.to_string();
                if msg.contains("no GPU adapters") || msg.contains("only software") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn new_async(gpu_id: Option<usize>) -> Result<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());

        let adapter = pick_adapter(&instance, gpu_id).await?;
        let info = adapter.get_info();
        let name = format!("wgpu:{} [{:?} via {:?}]", info.name, info.device_type, info.backend);

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("pow-gpu"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| anyhow!("request_device failed: {e:?}"))?;

        // Capture shader-validation errors instead of aborting the process.
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pow-blake3"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        if let Some(e) = scope.pop().await {
            return Err(anyhow!("shader validation failed: {e}"));
        }

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pow-bgl"),
            entries: &[
                storage_entry(0, true),
                storage_entry(1, false),
                storage_entry(2, false),
            ],
        });

        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pow-pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let make_pipe = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let pipe_init = make_pipe("k_init");
        let pipe_step = make_pipe("k_step");
        let pipe_test = make_pipe("k_test");

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let state_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("state"),
            size: (BATCH_NONCES as u64) * 8 * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let winners_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("winners"),
            size: WINNERS_BYTES,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: WINNERS_BYTES, // also big enough for the self-test state copy
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pow-bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: state_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: winners_buf.as_entire_binding() },
            ],
        });

        Ok(Self {
            name,
            device,
            queue,
            pipe_init,
            pipe_step,
            pipe_test,
            bind_group,
            params_buf,
            state_buf,
            winners_buf,
            readback_buf,
        })
    }

    fn dispatch(&self, pipe: &wgpu::ComputePipeline, groups: u32) {
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            cp.set_pipeline(pipe);
            cp.set_bind_group(0, &self.bind_group, &[]);
            cp.dispatch_workgroups(groups, 1, 1);
        }
        self.queue.submit([enc.finish()]);
    }

    fn wait(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }

    fn map_readback(&self, len: u64) -> Vec<u8> {
        let slice = self.readback_buf.slice(0..len);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.wait();
        let _ = rx.recv();
        let data = slice.get_mapped_range();
        let out = data.to_vec();
        drop(data);
        self.readback_buf.unmap();
        out
    }

    fn groups(n: u32) -> u32 {
        n.div_ceil(64)
    }

    /// Run ONE batch of `n_nonces` (≤ [`BATCH_NONCES`]) starting at `base`,
    /// applying the full [`EXTENSION_ITERATIONS`] chain via the CHECKPOINTED
    /// `k_step` loop (≈ [`ITERS_PER_DISPATCH`] iters/dispatch, the watchdog/TDR
    /// guard the OpenCL backend lacks), then `k_test`. Returns the candidate nonces
    /// the GPU surfaced (NOT yet CPU-verified). When `collect_winners` is false
    /// (self-test) it skips `k_test` and the caller reads the raw state instead.
    fn run_batch(
        &self,
        params: &mut Params,
        base: u64,
        n_nonces: u32,
        cancel: &AtomicBool,
        hash_counter: &AtomicU64,
        collect_winners: bool,
    ) -> Option<Vec<u64>> {
        params.base_lo = base as u32;
        params.base_hi = (base >> 32) as u32;
        params.n_nonces = n_nonces;
        params.iters = 0;
        self.queue
            .write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&*params));
        self.queue
            .write_buffer(&self.winners_buf, 0, bytemuck::cast_slice(&[0u32, MAX_WINNERS]));

        let groups = Self::groups(n_nonces);
        self.dispatch(&self.pipe_init, groups);
        self.wait();

        let total = EXTENSION_ITERATIONS;
        let chunk = ITERS_PER_DISPATCH;
        let mut remaining = total;
        while remaining > 0 {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let k = remaining.min(chunk as u64) as u32;
            self.queue
                .write_buffer(&self.params_buf, ITERS_FIELD_OFFSET, &k.to_le_bytes());
            self.dispatch(&self.pipe_step, groups);
            self.wait(); // bound watchdog exposure + keep cancel responsive
            remaining -= k as u64;
            // Count nonces (smoothed across the batch's dispatches), matching the
            // CPU counter semantics: ≈ n_nonces per full chain.
            let add = (n_nonces as u64).saturating_mul(k as u64) / total;
            hash_counter.fetch_add(add, Ordering::Relaxed);
        }

        if !collect_winners {
            return Some(Vec::new());
        }

        self.dispatch(&self.pipe_test, groups);
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_buffer_to_buffer(&self.winners_buf, 0, &self.readback_buf, 0, WINNERS_BYTES);
        self.queue.submit([enc.finish()]);

        let bytes = self.map_readback(WINNERS_BYTES);
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).min(MAX_WINNERS);
        let lo_off = 16usize;
        let hi_off = lo_off + (MAX_WINNERS as usize) * 4;
        let mut candidates = Vec::with_capacity(count as usize);
        for j in 0..count as usize {
            let lo =
                u32::from_le_bytes(bytes[lo_off + j * 4..lo_off + j * 4 + 4].try_into().unwrap());
            let hi =
                u32::from_le_bytes(bytes[hi_off + j * 4..hi_off + j * 4 + 4].try_into().unwrap());
            candidates.push((lo as u64) | ((hi as u64) << 32));
        }
        Some(candidates)
    }

    /// Prove the GPU reproduces [`crate::pow::midstate_pow`] bit-for-bit on the
    /// FULL 1,000,000-iteration chain. Runs a tiny batch and reads back the raw
    /// chaining state (which equals the final hash). Errors on any mismatch so a
    /// broken driver never mines.
    pub fn self_test(&self) -> Result<()> {
        let midstate = [0xA5u8; 32];
        let never = AtomicBool::new(false);
        let sink = AtomicU64::new(0);
        let base: u64 = 0;

        let mut params = Params {
            midstate: words_le(&midstate),
            target: [0u32; 8],
            pool: [0u32; 8],
            base_lo: 0,
            base_hi: 0,
            n_nonces: SELFTEST_N,
            iters: 0,
            has_pool: 0,
            pad0: 0,
            pad1: 0,
            pad2: 0,
        };
        self.run_batch(&mut params, base, SELFTEST_N, &never, &sink, false)
            .ok_or_else(|| anyhow!("self-test batch was unexpectedly cancelled"))?;

        let state_bytes_len = (SELFTEST_N as u64) * 8 * 4;
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_buffer_to_buffer(&self.state_buf, 0, &self.readback_buf, 0, state_bytes_len);
        self.queue.submit([enc.finish()]);
        let bytes = self.map_readback(state_bytes_len);

        for gid in 0..SELFTEST_N as u64 {
            let expected = midstate_pow(midstate, base + gid);
            let mut got = [0u8; 32];
            for i in 0..8usize {
                let off = (gid as usize) * 32 + i * 4;
                let w = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
                got[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            if got != expected {
                return Err(anyhow!(
                    "GPU self-test FAILED at nonce {gid}: kernel is not consensus-identical \
                     (gpu={} expected={}). Refusing to GPU-mine.",
                    hex::encode(got),
                    hex::encode(expected)
                ));
            }
        }
        Ok(())
    }
}

impl Backend for WgpuBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_gpu(&self) -> bool {
        true
    }

    fn suggested_batch(&self) -> u32 {
        BATCH_NONCES
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
        // Single target tier — our PoW has no separate pool_target, so has_pool=0.
        let mut params = Params {
            midstate: words_le(midstate),
            target: words_be(target),
            pool: [0u32; 8],
            base_lo: 0,
            base_hi: 0,
            n_nonces: BATCH_NONCES,
            iters: 0,
            has_pool: 0,
            pad0: 0,
            pad1: 0,
            pad2: 0,
        };
        let cancel = AtomicBool::new(false);
        let hash_counter = AtomicU64::new(0);

        // Search EXACTLY [nonce_start, nonce_start+count). The on-device buffer
        // holds BATCH_NONCES; chunk a larger window into back-to-back batches
        // (the client's suggested_batch() == BATCH_NONCES, so this is normally one
        // batch). We collect GPU-surfaced candidate nonces, then CPU-re-verify.
        let mut candidates: Vec<u64> = Vec::new();
        let mut done: u64 = 0;
        let total = count as u64;
        while done < total {
            let this = (total - done).min(BATCH_NONCES as u64) as u32;
            let base = nonce_start.wrapping_add(done);
            match self.run_batch(&mut params, base, this, &cancel, &hash_counter, true) {
                Some(mut c) => candidates.append(&mut c),
                None => break, // cancelled (not used by our synchronous caller)
            }
            done += this as u64;
        }

        // CONSENSUS SAFETY NET: recompute every surfaced candidate on the CPU and
        // keep only the genuine winners. The GPU never decides what is a share.
        Ok(verify_candidates(midstate, target, &candidates))
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `verify_candidates` de-duplicates a candidate surfaced twice and never
    /// double-counts it (no GPU, no 1M BLAKE3 — structural, runs by default with
    /// an all-0xff target where every nonce trivially "clears" after one chain).
    /// We keep it tiny: a single nonce, checked against an all-0xff target.
    #[test]
    #[ignore = "1×1,000,000 BLAKE3; run with: cargo test --features wgpu --release -- --ignored"]
    fn verify_dedupes_repeated_candidate() {
        let mid = [0u8; 32];
        let target = [0xffu8; 32];
        let out = verify_candidates(&mid, &target, &[7u64, 7u64, 7u64]);
        assert_eq!(out.len(), 1, "a repeated candidate must be verified once");
        assert_eq!(out[0].nonce, 7);
        assert_eq!(out[0].final_hash, midstate_pow(mid, 7));
    }

    /// The WGSL `Params` layout offset our host writes `iters` to must match the
    /// std430 struct (8+8+8 = 24 u32 words = 96 bytes, then base_lo, base_hi,
    /// n_nonces, then iters → 96 + 12 = 108).
    #[test]
    fn iters_field_offset_is_correct() {
        assert_eq!(ITERS_FIELD_OFFSET, 108);
        // Params is exactly 32 u32 words (128 bytes) — std430, no tail padding.
        assert_eq!(std::mem::size_of::<Params>(), 128);
    }

    /// Byte-encoding helpers round-trip the way the kernel expects: midstate is
    /// little-endian per 4-byte group, target is big-endian per group.
    #[test]
    fn word_encoders_match_kernel_expectations() {
        let mut b = [0u8; 32];
        b[0] = 0x01;
        b[3] = 0x04;
        assert_eq!(words_le(&b)[0], u32::from_le_bytes([0x01, 0, 0, 0x04]));
        assert_eq!(words_be(&b)[0], u32::from_be_bytes([0x01, 0, 0, 0x04]));
    }
}
