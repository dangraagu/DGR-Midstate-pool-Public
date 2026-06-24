//! OpenCL mining backend (`opencl3`) — drives the bit-exact `search` kernel in
//! `src/opencl/midstate.cl`. Single queue, synchronous `search()` matching the
//! `Backend` trait. The kernel is validated bit-exact vs the golden vectors
//! (see `src/opencl/selftest.c`); this module is the host-side glue (set args,
//! enqueue, read the results buffer → `Vec<Found>`).
//!
//! Build with `--features opencl` (links the system OpenCL ICD loader). On a box
//! with no OpenCL GPU, `try_new()` returns `Ok(None)` so the caller falls back
//! to the CPU backend.

use crate::backend::{Backend, Found};
use anyhow::{anyhow, Result};
use opencl3::command_queue::CommandQueue;
use opencl3::context::Context;
use opencl3::device::{cl_device_type, get_all_devices, Device, CL_DEVICE_TYPE_GPU};
use opencl3::kernel::{ExecuteKernel, Kernel};
use opencl3::memory::{Buffer, CL_MEM_READ_ONLY, CL_MEM_READ_WRITE, CL_MEM_WRITE_ONLY};
use opencl3::program::Program;
use opencl3::types::{cl_uchar, cl_uint, cl_ulong, CL_BLOCKING};
use std::ptr;

const KERNEL_SRC: &str = include_str!("opencl/midstate.cl");
const MAX_RESULTS: u32 = 4096;
const REC: usize = 10; // uints per record: nonce_lo, nonce_hi, then 8 hash words

pub struct OpenClBackend {
    name: String,
    #[allow(dead_code)]
    context: Context, // kept alive for the lifetime of the buffers/queue
    queue: CommandQueue,
    kernel: Kernel,
    d_mid: Buffer<cl_uchar>,
    d_tgt: Buffer<cl_uchar>,
    d_count: Buffer<cl_uint>,
    d_results: Buffer<cl_uint>,
    local: usize,
}

impl OpenClBackend {
    /// Production entry: first OpenCL **GPU**. `Ok(None)` if none present.
    pub fn try_new() -> Result<Option<Self>> {
        Self::try_new_with_type(CL_DEVICE_TYPE_GPU)
    }

    /// Build on the first device of `dtype`. (Tests pass `CL_DEVICE_TYPE_ALL` so
    /// they can run on a CPU OpenCL runtime like POCL.)
    pub fn try_new_with_type(dtype: cl_device_type) -> Result<Option<Self>> {
        let devices =
            get_all_devices(dtype).map_err(|e| anyhow!("opencl get_all_devices: {e:?}"))?;
        if devices.is_empty() {
            return Ok(None);
        }
        let device = Device::new(devices[0]);
        let dev_name = device.name().unwrap_or_else(|_| "OpenCL".to_string());
        let context =
            Context::from_device(&device).map_err(|e| anyhow!("opencl context: {e:?}"))?;
        #[allow(deprecated)]
        let queue = CommandQueue::create_default(&context, 0)
            .map_err(|e| anyhow!("opencl queue: {e:?}"))?;
        let program = Program::create_and_build_from_source(&context, KERNEL_SRC, "")
            .map_err(|e| anyhow!("opencl build failed: {e}"))?;
        let kernel =
            Kernel::create(&program, "search").map_err(|e| anyhow!("opencl kernel: {e:?}"))?;

        let (d_mid, d_tgt, d_count, d_results) = unsafe {
            (
                Buffer::<cl_uchar>::create(&context, CL_MEM_READ_ONLY, 32, ptr::null_mut())
                    .map_err(|e| anyhow!("mid buf: {e:?}"))?,
                Buffer::<cl_uchar>::create(&context, CL_MEM_READ_ONLY, 32, ptr::null_mut())
                    .map_err(|e| anyhow!("tgt buf: {e:?}"))?,
                Buffer::<cl_uint>::create(&context, CL_MEM_READ_WRITE, 1, ptr::null_mut())
                    .map_err(|e| anyhow!("count buf: {e:?}"))?,
                Buffer::<cl_uint>::create(
                    &context,
                    CL_MEM_WRITE_ONLY,
                    (MAX_RESULTS as usize) * REC,
                    ptr::null_mut(),
                )
                .map_err(|e| anyhow!("results buf: {e:?}"))?,
            )
        };

        Ok(Some(Self {
            name: format!("opencl:{dev_name}"),
            context,
            queue,
            kernel,
            d_mid,
            d_tgt,
            d_count,
            d_results,
            local: 64,
        }))
    }
}

impl Backend for OpenClBackend {
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
        let mid: [cl_uchar; 32] = *midstate;
        let tgt: [cl_uchar; 32] = *target;

        unsafe {
            self.queue
                .enqueue_write_buffer(&mut self.d_mid, CL_BLOCKING, 0, &mid, &[])
                .map_err(|e| anyhow!("write mid: {e:?}"))?;
            self.queue
                .enqueue_write_buffer(&mut self.d_tgt, CL_BLOCKING, 0, &tgt, &[])
                .map_err(|e| anyhow!("write tgt: {e:?}"))?;
            self.queue
                .enqueue_write_buffer(&mut self.d_count, CL_BLOCKING, 0, &[0u32], &[])
                .map_err(|e| anyhow!("reset count: {e:?}"))?;
        }

        let nonce_base: cl_ulong = nonce_start;
        let cnt: cl_uint = count;
        let maxr: cl_uint = MAX_RESULTS;
        let global = ((count as usize).div_ceil(self.local)) * self.local;

        unsafe {
            ExecuteKernel::new(&self.kernel)
                .set_arg(&self.d_mid)
                .set_arg(&self.d_tgt)
                .set_arg(&nonce_base)
                .set_arg(&cnt)
                .set_arg(&maxr)
                .set_arg(&self.d_count)
                .set_arg(&self.d_results)
                .set_global_work_size(global)
                .set_local_work_size(self.local)
                .enqueue_nd_range(&self.queue)
                .map_err(|e| anyhow!("launch: {e:?}"))?;
        }
        self.queue.finish().map_err(|e| anyhow!("finish: {e:?}"))?;

        let mut n = [0u32; 1];
        unsafe {
            self.queue
                .enqueue_read_buffer(&self.d_count, CL_BLOCKING, 0, &mut n, &[])
                .map_err(|e| anyhow!("read count: {e:?}"))?;
        }
        let found_n = (n[0] as usize).min(MAX_RESULTS as usize);
        if found_n == 0 {
            return Ok(Vec::new());
        }

        let mut recs = vec![0u32; found_n * REC];
        unsafe {
            self.queue
                .enqueue_read_buffer(&self.d_results, CL_BLOCKING, 0, &mut recs, &[])
                .map_err(|e| anyhow!("read results: {e:?}"))?;
        }

        let mut out = Vec::with_capacity(found_n);
        for i in 0..found_n {
            let base = i * REC;
            let nonce = (recs[base] as u64) | ((recs[base + 1] as u64) << 32);
            let mut h = [0u8; 32];
            for w in 0..8 {
                h[w * 4..w * 4 + 4].copy_from_slice(&recs[base + 2 + w].to_le_bytes());
            }
            out.push(Found {
                nonce,
                final_hash: h,
            });
        }
        out.sort_by_key(|f| f.nonce);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencl3::device::CL_DEVICE_TYPE_ALL;

    /// Bit-exact golden via the `search` kernel with an all-0xff target (every
    /// nonce passes). Needs an OpenCL device + the `opencl` feature.
    /// Run: `cargo test --features opencl -- --ignored opencl_golden`.
    #[test]
    #[ignore = "needs an OpenCL device + the opencl feature"]
    fn opencl_golden() {
        let mut b = OpenClBackend::try_new_with_type(CL_DEVICE_TYPE_ALL)
            .unwrap()
            .expect("an OpenCL device");
        let target = [0xffu8; 32];
        let found = b.search(&[0u8; 32], &target, 0, 2).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].nonce, 0);
        assert_eq!(found[1].nonce, 1);
        assert_eq!(
            hex::encode(found[0].final_hash),
            "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369"
        );
        assert_eq!(
            hex::encode(found[1].final_hash),
            "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9"
        );
    }
}
