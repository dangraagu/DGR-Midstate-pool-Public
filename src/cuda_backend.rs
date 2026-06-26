//! CUDA mining backend (`cudarc`, dynamic-loading) — drives the bit-exact
//! `search` kernel in `src/cuda/midstate.cu`, compiled to the committed PTX
//! `src/cuda/midstate.ptx`. This is the NATIVE path for the rented NVIDIA
//! CUDA-compute containers (5× RTX 3060 etc.) where wgpu/Vulkan is unavailable.
//!
//! ## Toolkit-free at build AND runtime
//! `cudarc` is pulled in with its `dynamic-loading` feature, so it `dlopen`s
//! `libcuda` (the driver) at RUNTIME. The Linux/CI build therefore needs NO CUDA
//! toolkit on the link path — only this committed PTX text and the driver present
//! on the rig. The PTX (`-arch=compute_75`) is JIT-compiled by the driver to the
//! actual SM (sm_86 on a 3060, Blackwell on a 5070 Ti), so one PTX runs on every
//! user GPU.
//!
//! ## Structure mirrors `wgpu_backend` / `opencl_backend`
//! `try_new(gpu_id)` selects the CUDA device by index (multi-GPU: one process per
//! `--gpu-id`), loads the committed PTX, and runs [`CudaBackend::self_test`] — the
//! FULL 1,000,000-iteration chain on fixed nonces, BYTE-COMPARED against
//! `crate::pow::midstate_pow` — refusing to mine (`Err`, fail-closed) on ANY
//! mismatch. `search()` collects the nonces the GPU surfaces and feeds them
//! through [`crate::wgpu_backend::verify_candidates`] (when built) or the local
//! [`verify_candidates`] re-verify, so every GPU-surfaced nonce is recomputed on
//! the CPU before it can become a share. The GPU is NEVER trusted to decide what
//! is a valid share — the same no-clawback net wgpu has.
//!
//! Build with `--features cuda`. On a box with no NVIDIA driver, `try_new`
//! returns `Ok(None)` (auto) so the caller falls back to CPU; an explicit
//! `--gpu-id` that can't be satisfied returns `Err` (loud).

use crate::backend::{Backend, Found};
use crate::pow::{meets_target, midstate_pow};
use anyhow::{anyhow, Result};
use cudarc::driver::{CudaContext, CudaFunction, CudaSlice, CudaStream, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;
use std::sync::Arc;

/// The committed PTX (compiled here with `nvcc -ptx -arch=compute_75`). Embedded
/// at build time so the rig needs no toolkit — the driver JITs it at load.
const KERNEL_PTX: &str = include_str!("cuda/midstate.ptx");

const MAX_RESULTS: u32 = 4096;
const REC: usize = 10; // uints per record: nonce_lo, nonce_hi, then 8 hash words
const SELFTEST_N: u32 = 8;
const BLOCK: u32 = 64; // threads per block

/// Recompute the consensus PoW for each GPU-surfaced candidate nonce on the CPU
/// and keep only those whose real final hash clears `target`. Identical net to
/// `wgpu_backend::verify_candidates`: the GPU is trusted only to *surface*
/// candidates; the bytes that become a [`Found`] are always the CPU's, so a buggy
/// or non-deterministic driver can never produce an invalid share — only waste a
/// little CPU on a false positive. De-duplicated, sorted ascending by nonce. PURE.
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

/// One display line for a CUDA device: `index: name`.
fn device_line(i: usize) -> Result<String> {
    let ctx = CudaContext::new(i).map_err(|e| anyhow!("cuda device {i}: {e:?}"))?;
    let name = ctx.name().unwrap_or_else(|_| "CUDA device".to_string());
    Ok(format!("{i}: {name} (CUDA)"))
}

/// Enumerate every CUDA device and return one `index: name (CUDA)` line. Owns its
/// own contexts so it can be called standalone for `--list-gpus`. Returns an empty
/// Vec when there is no driver / no device (so `--list-gpus` prints a clear note).
pub fn list_devices() -> Vec<String> {
    let count = match CudaContext::device_count() {
        Ok(n) if n > 0 => n as usize,
        _ => return Vec::new(),
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        match device_line(i) {
            Ok(line) => out.push(line),
            Err(_) => out.push(format!("{i}: <unavailable> (CUDA)")),
        }
    }
    out
}

/// CUDA GPU mining backend implementing our [`Backend`] trait.
pub struct CudaBackend {
    name: String,
    stream: Arc<CudaStream>,
    search_fn: CudaFunction,
    mine_fn: CudaFunction,
    // Persistent device buffers (allocated once, reused per launch).
    d_mid: CudaSlice<u8>,
    d_tgt: CudaSlice<u8>,
    d_count: CudaSlice<u32>,
    d_results: CudaSlice<u32>,
}

impl CudaBackend {
    /// Production entry: select the CUDA device by index (or device 0 when
    /// `gpu_id` is `None`), load the committed PTX, and self-test. `Ok(None)` when
    /// there is no driver / no device on AUTO (so the caller falls back to CPU);
    /// `Err` when an EXPLICIT `gpu_id` can't be satisfied or the self-test fails
    /// (fail-closed — never mine on a card we can't prove bit-exact).
    pub fn try_new(gpu_id: Option<usize>) -> Result<Option<Self>> {
        let count = match CudaContext::device_count() {
            Ok(n) => n,
            Err(e) => {
                // No driver / libcuda not loadable. An explicit --gpu-id is a user
                // error to surface; auto falls through to CPU.
                if gpu_id.is_some() {
                    return Err(anyhow!(
                        "--gpu-id requested but no CUDA driver is available: {e:?}. \
                         The rig needs the NVIDIA driver (libcuda) installed."
                    ));
                }
                return Ok(None);
            }
        };
        if count <= 0 {
            if gpu_id.is_some() {
                return Err(anyhow!(
                    "--gpu-id requested but no CUDA device is present (device_count=0)."
                ));
            }
            return Ok(None);
        }

        let idx = gpu_id.unwrap_or(0);
        if idx >= count as usize {
            // Explicit out-of-range index must fail LOUDLY with the device list.
            let listed = list_devices().join("\n");
            return Err(anyhow!(
                "--gpu-id {idx} is out of range: only {count} CUDA device(s) found:\n{listed}"
            ));
        }

        let ctx = CudaContext::new(idx).map_err(|e| anyhow!("cuda context {idx}: {e:?}"))?;
        let dev_name = ctx.name().unwrap_or_else(|_| "CUDA device".to_string());
        let stream = ctx.default_stream();

        // Load the committed PTX. PtxKind::Src → cuModuleLoadData on the PTX text:
        // the DRIVER JITs it (no nvrtc / toolkit needed). A bad PTX surfaces here.
        let module = ctx
            .load_module(Ptx::from_src(KERNEL_PTX))
            .map_err(|e| anyhow!("cuda load PTX module: {e:?}"))?;
        let search_fn = module
            .load_function("search")
            .map_err(|e| anyhow!("cuda load search fn: {e:?}"))?;
        let mine_fn = module
            .load_function("mine_chain")
            .map_err(|e| anyhow!("cuda load mine_chain fn: {e:?}"))?;

        let d_mid = stream
            .alloc_zeros::<u8>(32)
            .map_err(|e| anyhow!("alloc mid: {e:?}"))?;
        let d_tgt = stream
            .alloc_zeros::<u8>(32)
            .map_err(|e| anyhow!("alloc tgt: {e:?}"))?;
        let d_count = stream
            .alloc_zeros::<u32>(1)
            .map_err(|e| anyhow!("alloc count: {e:?}"))?;
        let d_results = stream
            .alloc_zeros::<u32>((MAX_RESULTS as usize) * REC)
            .map_err(|e| anyhow!("alloc results: {e:?}"))?;

        let backend = Self {
            name: format!("cuda:{dev_name}"),
            stream,
            search_fn,
            mine_fn,
            d_mid,
            d_tgt,
            d_count,
            d_results,
        };

        // Refuse to mine unless the GPU reproduces the CPU reference bit-for-bit.
        backend.self_test()?;
        Ok(Some(backend))
    }

    /// Prove the GPU reproduces [`crate::pow::midstate_pow`] bit-for-bit on the
    /// FULL 1,000,000-iteration chain. Runs `mine_chain` over fixed nonces and
    /// byte-compares each 32-byte final hash against the CPU oracle. Errors
    /// (fail-closed) on ANY mismatch so a broken/non-deterministic driver never
    /// mines.
    pub fn self_test(&self) -> Result<()> {
        let midstate = [0xA5u8; 32];
        let base: u64 = 0;

        let d_mid = self
            .stream
            .clone_htod(&midstate)
            .map_err(|e| anyhow!("self_test htod mid: {e:?}"))?;
        let mut d_out = self
            .stream
            .alloc_zeros::<u32>((SELFTEST_N as usize) * 8)
            .map_err(|e| anyhow!("self_test alloc out: {e:?}"))?;

        let iters: u32 = crate::pow::EXTENSION_ITERATIONS as u32;
        let cfg = LaunchConfig {
            grid_dim: (SELFTEST_N.div_ceil(BLOCK), 1, 1),
            block_dim: (BLOCK, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut builder = self.stream.launch_builder(&self.mine_fn);
        builder.arg(&d_mid).arg(&base).arg(&iters).arg(&mut d_out);
        unsafe {
            builder
                .launch(cfg)
                .map_err(|e| anyhow!("self_test launch: {e:?}"))?;
        }
        let words = self
            .stream
            .clone_dtoh(&d_out)
            .map_err(|e| anyhow!("self_test dtoh: {e:?}"))?;

        for gid in 0..SELFTEST_N as usize {
            let expected = midstate_pow(midstate, base + gid as u64);
            let mut got = [0u8; 32];
            for i in 0..8usize {
                got[i * 4..i * 4 + 4].copy_from_slice(&words[gid * 8 + i].to_le_bytes());
            }
            if got != expected {
                return Err(anyhow!(
                    "CUDA self-test FAILED at nonce {gid}: kernel is not consensus-identical \
                     (gpu={} expected={}). Refusing to GPU-mine.",
                    hex::encode(got),
                    hex::encode(expected)
                ));
            }
        }
        Ok(())
    }
}

impl Backend for CudaBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_gpu(&self) -> bool {
        true
    }

    fn suggested_batch(&self) -> u32 {
        1 << 16 // 65536 nonces per launch
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

        // Upload inputs + reset the atomic result counter.
        self.stream
            .memcpy_htod(midstate, &mut self.d_mid)
            .map_err(|e| anyhow!("write mid: {e:?}"))?;
        self.stream
            .memcpy_htod(target, &mut self.d_tgt)
            .map_err(|e| anyhow!("write tgt: {e:?}"))?;
        self.stream
            .memcpy_htod(&[0u32], &mut self.d_count)
            .map_err(|e| anyhow!("reset count: {e:?}"))?;

        let nonce_base: u64 = nonce_start;
        let cnt: u32 = count;
        let maxr: u32 = MAX_RESULTS;
        let cfg = LaunchConfig {
            grid_dim: (count.div_ceil(BLOCK), 1, 1),
            block_dim: (BLOCK, 1, 1),
            shared_mem_bytes: 0,
        };

        let mut builder = self.stream.launch_builder(&self.search_fn);
        builder
            .arg(&self.d_mid)
            .arg(&self.d_tgt)
            .arg(&nonce_base)
            .arg(&cnt)
            .arg(&maxr)
            .arg(&mut self.d_count)
            .arg(&mut self.d_results);
        unsafe {
            builder
                .launch(cfg)
                .map_err(|e| anyhow!("launch search: {e:?}"))?;
        }

        let n = self
            .stream
            .clone_dtoh(&self.d_count)
            .map_err(|e| anyhow!("read count: {e:?}"))?;
        let found_n = (n[0] as usize).min(MAX_RESULTS as usize);
        if found_n == 0 {
            return Ok(Vec::new());
        }

        let recs = self
            .stream
            .clone_dtoh(&self.d_results)
            .map_err(|e| anyhow!("read results: {e:?}"))?;

        // Collect the GPU-surfaced candidate nonces (NOT yet trusted).
        let mut candidates = Vec::with_capacity(found_n);
        for i in 0..found_n {
            let base = i * REC;
            let nonce = (recs[base] as u64) | ((recs[base + 1] as u64) << 32);
            candidates.push(nonce);
        }

        // CONSENSUS SAFETY NET: recompute every surfaced candidate on the CPU and
        // keep only the genuine winners. The GPU never decides what is a share.
        Ok(verify_candidates(midstate, target, &candidates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `verify_candidates` de-duplicates a candidate surfaced twice and never
    /// double-counts it (no GPU; structural with an all-0xff target where every
    /// nonce trivially clears after one chain). Run with:
    /// `cargo test --features cuda --release -- --ignored cuda_verify_dedupes`.
    #[test]
    #[ignore = "1×1,000,000 BLAKE3; run with: cargo test --features cuda --release -- --ignored"]
    fn cuda_verify_dedupes_repeated_candidate() {
        let mid = [0u8; 32];
        let target = [0xffu8; 32];
        let out = verify_candidates(&mid, &target, &[7u64, 7u64, 7u64]);
        assert_eq!(out.len(), 1, "a repeated candidate must be verified once");
        assert_eq!(out[0].nonce, 7);
        assert_eq!(out[0].final_hash, midstate_pow(mid, 7));
    }

    /// Empty candidate list yields no winners (structural, no 1M BLAKE3).
    #[test]
    fn cuda_verify_empty_is_empty() {
        let out = verify_candidates(&[0u8; 32], &[0xffu8; 32], &[]);
        assert!(out.is_empty());
    }
}
