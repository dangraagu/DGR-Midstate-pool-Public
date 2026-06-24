//! `midstate_miner` — open-source pool miner for the Midstate chain.
//!
//! **Pool-only, endpoint-locked.** This library houses the two pieces that must
//! be correct before anything else: the bit-exact consensus PoW ([`pow`]) and
//! the compiled-in pool [`endpoint`] lock. The Stratum client, the CPU/CUDA
//! backends, and the CLI binary build on top of these (next milestone).
//!
//! Midstate is a post-quantum chain whose PoW is a sequential BLAKE3 VDF
//! (each nonce = a 1,000,000-iteration hash chain). See [`pow`] for the exact
//! consensus contract.

pub mod endpoint;
pub mod pow;

pub use endpoint::{pool_endpoint, EndpointList};
pub use pow::{meets_target, midstate_pow, midstate_pow_n, EXTENSION_ITERATIONS};
