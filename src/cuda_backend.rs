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
const BLOCK: u32 = 256; // threads per block — a multiple of the warp size (32) that
                        // keeps a big card's SMs saturated. The kernel uses a heap
                        // of registers (the BLAKE3 working state), so 256 is a
                        // balanced occupancy point across Ampere/Ada/Blackwell.

/// Default per-`search` nonce window for a CUDA card. THROUGHPUT FIX 2 — each nonce
/// is one ~1M-iter BLAKE3 chain (one resident thread), and a 4090 has ~16k+
/// resident threads; the window must be several times that so every SM stays full
/// for the whole launch instead of draining at the tail. 1<<18 = 262 144 nonces is
/// a few× the resident capacity of the biggest consumer cards. Tunable via
/// `--gpu-batch`; the loop also splits it into pipelined sub-waves (see `search`).
const DEFAULT_BATCH: u32 = 1 << 18; // 262_144

/// Number of pipelined sub-waves the window is carved into. With 2+ streams +
/// ping-pong buffers we launch wave N+1 while copying back + CPU-scanning wave N,
/// so the GPU is never idle during readback/host work. 4 sub-waves keeps each wave
/// large enough to saturate the SMs while giving enough in-flight overlap to hide
/// the dtoh + scan latency behind the next launch.
const PIPELINE_WAVES: u32 = 4;

/// Lower bound on a single sub-wave so a tiny window (or a tiny `--gpu-batch`)
/// doesn't degenerate into launches with too few threads to fill even one SM.
const MIN_WAVE: u32 = 1 << 14; // 16_384

/// One pipeline slot: an independent CUDA stream plus its OWN device count/result
/// buffers, so wave N (being copied back on slot A) never races wave N+1 (launching
/// on slot B). Inputs (midstate/target) are shared read-only across slots.
struct Slot {
    stream: Arc<CudaStream>,
    d_count: CudaSlice<u32>,
    d_results: CudaSlice<u32>,
}

/// Split `[0, count)` into contiguous, DISJOINT sub-waves of at most `wave` nonces
/// each (the last one is the remainder). Returns `(offset, len)` pairs whose union
/// is exactly `[0, count)` with no gaps or overlap — the partition the pipeline
/// hands to successive launches. PURE (no GPU): unit-tested. `wave` is clamped to
/// `>= 1` so a degenerate `wave == 0` can't loop forever.
pub fn partition_waves(count: u32, wave: u32) -> Vec<(u32, u32)> {
    let w = wave.max(1);
    let mut out = Vec::new();
    let mut off = 0u32;
    while off < count {
        let len = w.min(count - off);
        out.push((off, len));
        off += len;
    }
    out
}

/// Choose the per-sub-wave nonce count for a given total window: aim for
/// [`PIPELINE_WAVES`] waves but never below [`MIN_WAVE`] (so each launch still
/// fills the card), and never above `count` itself. PURE: unit-tested.
pub fn wave_size(count: u32, pipeline_waves: u32, min_wave: u32) -> u32 {
    let waves = pipeline_waves.max(1);
    let target = count.div_ceil(waves); // ceil so `waves` launches cover `count`
    target.max(min_wave).min(count.max(1))
}

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
    /// The primary stream — owns the shared input buffers + drives the self-test.
    stream: Arc<CudaStream>,
    search_fn: CudaFunction,
    mine_fn: CudaFunction,
    // Shared, read-only-per-search input buffers (allocated once, reused).
    d_mid: CudaSlice<u8>,
    d_tgt: CudaSlice<u8>,
    /// Pipeline slots: independent streams + their own count/result buffers, so
    /// successive sub-waves overlap (launch N+1 while reading back N). Always >= 2.
    slots: Vec<Slot>,
    /// Per-`search` nonce window (from `--gpu-batch`, else [`DEFAULT_BATCH`]).
    batch: u32,
}

impl CudaBackend {
    /// Production entry: select the CUDA device by index (or device 0 when
    /// `gpu_id` is `None`), load the committed PTX, and self-test. `Ok(None)` when
    /// there is no driver / no device on AUTO (so the caller falls back to CPU);
    /// `Err` when an EXPLICIT `gpu_id` can't be satisfied or the self-test fails
    /// (fail-closed — never mine on a card we can't prove bit-exact).
    pub fn try_new(gpu_id: Option<usize>, batch: Option<u32>) -> Result<Option<Self>> {
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

        // THROUGHPUT FIX 1 — switch the context from the default spin-wait
        // synchronize (CU_CTX_SCHED_AUTO → busy-spins a CPU core, esp. on
        // Windows) to BLOCKING sync. Each `clone_dtoh` below blocks on the
        // launching stream; with the pipeline draining one slot while another
        // launches, a spin-wait would pin a whole physical core in a kernel-mode
        // poll loop. Blocking-sync trades a few µs of wake latency for a freed
        // core — the GPU is the hashrate, the host just feeds it.
        ctx.set_blocking_synchronize()
            .map_err(|e| anyhow!("cuda set_blocking_synchronize: {e:?}"))?;

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

        // Shared, read-only-per-search input buffers on the primary stream.
        let d_mid = stream
            .alloc_zeros::<u8>(32)
            .map_err(|e| anyhow!("alloc mid: {e:?}"))?;
        let d_tgt = stream
            .alloc_zeros::<u8>(32)
            .map_err(|e| anyhow!("alloc tgt: {e:?}"))?;

        // Build the pipeline slots ONCE: each gets its OWN stream + count/result
        // buffers so wave N (being drained on slot A) never races wave N+1
        // (launching on slot B). Two slots is the minimum for launch/drain
        // overlap; more just adds in-flight depth at the cost of memory.
        const N_SLOTS: usize = 2;
        let mut slots = Vec::with_capacity(N_SLOTS);
        for s in 0..N_SLOTS {
            let sstream = ctx
                .new_stream()
                .map_err(|e| anyhow!("alloc slot {s} stream: {e:?}"))?;
            let d_count = sstream
                .alloc_zeros::<u32>(1)
                .map_err(|e| anyhow!("alloc slot {s} count: {e:?}"))?;
            let d_results = sstream
                .alloc_zeros::<u32>((MAX_RESULTS as usize) * REC)
                .map_err(|e| anyhow!("alloc slot {s} results: {e:?}"))?;
            slots.push(Slot {
                stream: sstream,
                d_count,
                d_results,
            });
        }

        // Window per `search` call: explicit `--gpu-batch` (clamped to >= MIN_WAVE
        // so it can still fill the card) else DEFAULT_BATCH.
        let batch = batch.map(|b| b.max(MIN_WAVE)).unwrap_or(DEFAULT_BATCH);

        let backend = Self {
            name: format!("cuda:{dev_name}"),
            stream,
            search_fn,
            mine_fn,
            d_mid,
            d_tgt,
            slots,
            batch,
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
        // Launch EXACTLY SELFTEST_N threads (one block of SELFTEST_N). `mine_chain`
        // has NO `tid >= n` guard and `d_out` is only SELFTEST_N*8 u32, so launching
        // a full BLOCK (256) would spawn threads tid 8..255 that write out of bounds.
        // One block of SELFTEST_N threads ⇒ tid ∈ [0, SELFTEST_N) ⇒ no OOB write.
        let cfg = LaunchConfig {
            grid_dim: (1, 1, 1),
            block_dim: (SELFTEST_N, 1, 1),
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
        // The whole `--gpu-batch` window: `search` internally pipelines it across
        // slots, so a big window keeps the card pegged (no per-call idle gap) while
        // the client still re-checks the job epoch once per window.
        self.batch
    }

    /// PIPELINED search. The `count` window is carved into DISJOINT sub-waves
    /// ([`partition_waves`] / [`wave_size`]); we keep both slots in flight,
    /// launching wave N+1 on the free slot while [`clone_dtoh`]-draining +
    /// CPU-re-verifying wave N on the other — so the GPU never idles between
    /// launches. Every surfaced candidate is still CPU-recomputed by
    /// [`verify_candidates`] before it can become a share (the no-clawback net).
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

        // Upload shared inputs ONCE on the primary stream and wait for them to
        // land before any slot kernel reads them (the slots run on other streams).
        self.stream
            .memcpy_htod(midstate, &mut self.d_mid)
            .map_err(|e| anyhow!("write mid: {e:?}"))?;
        self.stream
            .memcpy_htod(target, &mut self.d_tgt)
            .map_err(|e| anyhow!("write tgt: {e:?}"))?;
        self.stream
            .synchronize()
            .map_err(|e| anyhow!("sync inputs: {e:?}"))?;

        let wave = wave_size(count, PIPELINE_WAVES, MIN_WAVE);
        let waves = partition_waves(count, wave);
        let n_slots = self.slots.len();
        let maxr: u32 = MAX_RESULTS;

        // Per-slot record of the in-flight wave so we can drain + map its nonces
        // back to the right offset. `None` == slot idle.
        let mut inflight: Vec<Option<(u64, u32)>> = vec![None; n_slots]; // (nonce_base, len)
        let mut candidates: Vec<u64> = Vec::new();

        // Reset every slot's atomic counter before its first launch this call.
        for slot in self.slots.iter_mut() {
            slot.stream
                .memcpy_htod(&[0u32], &mut slot.d_count)
                .map_err(|e| anyhow!("reset count: {e:?}"))?;
        }

        // Drive `waves` through the slots round-robin. At step `i` we first DRAIN
        // the slot we are about to reuse (if it still holds wave i - n_slots), then
        // LAUNCH wave i on it. This overlaps drain(i-n_slots) with the kernels of
        // waves already running on the OTHER slot(s).
        let drain_slot = |slot: &Slot, base: u64, len: u32, out: &mut Vec<u64>| -> Result<()> {
            let n = slot
                .stream
                .clone_dtoh(&slot.d_count)
                .map_err(|e| anyhow!("read count: {e:?}"))?;
            let found_n = (n[0] as usize).min(MAX_RESULTS as usize);
            if found_n == 0 {
                return Ok(());
            }
            let recs = slot
                .stream
                .clone_dtoh(&slot.d_results)
                .map_err(|e| anyhow!("read results: {e:?}"))?;
            for i in 0..found_n {
                let b = i * REC;
                let nonce = (recs[b] as u64) | ((recs[b + 1] as u64) << 32);
                // Defensive: only keep nonces this wave actually covered.
                if nonce >= base && nonce < base + len as u64 {
                    out.push(nonce);
                }
            }
            Ok(())
        };

        for (i, &(off, len)) in waves.iter().enumerate() {
            let s = i % n_slots;

            // Drain the wave currently occupying this slot before reusing it.
            if let Some((base, plen)) = inflight[s].take() {
                drain_slot(&self.slots[s], base, plen, &mut candidates)?;
                // Re-zero the counter for the upcoming launch on this slot.
                let slot = &mut self.slots[s];
                slot.stream
                    .memcpy_htod(&[0u32], &mut slot.d_count)
                    .map_err(|e| anyhow!("reset count: {e:?}"))?;
            }

            let nonce_base: u64 = nonce_start + off as u64;
            let cnt: u32 = len;
            let cfg = LaunchConfig {
                grid_dim: (len.div_ceil(BLOCK), 1, 1),
                block_dim: (BLOCK, 1, 1),
                shared_mem_bytes: 0,
            };
            {
                let slot = &mut self.slots[s];
                let mut builder = slot.stream.launch_builder(&self.search_fn);
                builder
                    .arg(&self.d_mid)
                    .arg(&self.d_tgt)
                    .arg(&nonce_base)
                    .arg(&cnt)
                    .arg(&maxr)
                    .arg(&mut slot.d_count)
                    .arg(&mut slot.d_results);
                unsafe {
                    builder
                        .launch(cfg)
                        .map_err(|e| anyhow!("launch search: {e:?}"))?;
                }
            }
            inflight[s] = Some((nonce_base, len));
        }

        // Drain whatever is still in flight on each slot (the last n_slots waves).
        for (s, slot_inflight) in inflight.iter_mut().enumerate() {
            if let Some((base, len)) = slot_inflight.take() {
                drain_slot(&self.slots[s], base, len, &mut candidates)?;
            }
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

    /// `partition_waves` covers `[0, count)` exactly: contiguous, disjoint, union
    /// == the whole window, last wave is the remainder. This is the property the
    /// pipeline relies on for DISJOINT nonce coverage (no nonce searched twice, no
    /// gap left unsearched).
    #[test]
    fn partition_waves_is_a_disjoint_cover() {
        for &(count, wave) in &[(10u32, 3u32), (262_144, 65_536), (1, 1), (100, 100), (100, 7)] {
            let parts = partition_waves(count, wave);
            // contiguous + disjoint
            let mut expect_off = 0u32;
            for &(off, len) in &parts {
                assert_eq!(off, expect_off, "gap/overlap at count={count} wave={wave}");
                assert!(len > 0 && len <= wave, "bad len at count={count} wave={wave}");
                expect_off += len;
            }
            // union is exactly [0, count)
            assert_eq!(expect_off, count, "cover != count at count={count} wave={wave}");
        }
    }

    /// A `count` of 0 produces no waves (the pipeline early-returns before this,
    /// but the partition must still be empty, never an infinite loop).
    #[test]
    fn partition_waves_zero_count_is_empty() {
        assert!(partition_waves(0, 1024).is_empty());
    }

    /// A degenerate `wave == 0` is clamped to 1 (no infinite loop).
    #[test]
    fn partition_waves_zero_wave_is_clamped() {
        let parts = partition_waves(3, 0);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts, vec![(0, 1), (1, 1), (2, 1)]);
    }

    /// `wave_size` aims for `pipeline_waves` waves, floored at `min_wave`, capped
    /// at `count`. A small window collapses to one wave of the whole window.
    #[test]
    fn wave_size_floors_caps_and_targets() {
        // Big window: count/waves, above the floor.
        assert_eq!(wave_size(262_144, 4, 1 << 14), 65_536);
        // Window smaller than the floor: a single wave of the whole window.
        assert_eq!(wave_size(1000, 4, 1 << 14), 1000);
        // count/waves below the floor → the floor wins.
        assert_eq!(wave_size(40_000, 4, 1 << 14), 1 << 14);
        // pipeline_waves == 0 is clamped to 1 (one wave covers count).
        assert_eq!(wave_size(50_000, 0, 1 << 14), 50_000);
        // count == 0 never yields 0 (min .min(count.max(1))).
        assert_eq!(wave_size(0, 4, 1 << 14), 1);
    }
}
