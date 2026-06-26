// midstate.cu — Midstate PoW kernel in CUDA C.
//
// BLAKE3 single-block compression + the 1,000,000-iteration sequential chain.
// CONSENSUS-CRITICAL: must be bit-exact with src/pow.rs and the golden vectors
// (a713dea1…/8ac4d9ef…). Ported line-for-line from src/opencl/midstate.cl; the
// math is identical integer ops. Compiled to a committed PTX
// (src/cuda/midstate.ptx) so the rigs JIT it at runtime via the driver — no
// CUDA toolkit needed at runtime, only the committed PTX + the driver.
//
//   nvcc -ptx -arch=compute_75 -o src/cuda/midstate.ptx src/cuda/midstate.cu
//
// compute_75 = virtual arch; the driver JITs it forward to sm_86 (RTX 3060) /
// Blackwell (RTX 5070 Ti), so one PTX runs on every user GPU.
//
// PTX-ISA PIN (forward-compat across DRIVER versions): after compiling, the
// committed PTX's `.version` directive is pinned DOWN to `7.8` (CUDA 11.8). A
// newer nvcc (13.3 here) emits `.version 9.3`, which an older rig driver rejects
// with CUDA_ERROR_UNSUPPORTED_PTX_VERSION. This kernel uses only long-stable PTX
// ops (add/xor/shift/atom.add), so a 7.8 header is valid and JIT-loads on the
// widest range of rig drivers. Regenerate with `src/cuda/build_ptx.sh`, which
// runs nvcc then re-pins `.version` — do NOT commit the raw `.version 9.3`.

typedef unsigned int  u32;
typedef unsigned long long u64;
typedef unsigned char u8;

__constant__ u32 IVc[8] = {
    0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
    0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u
};

__device__ __forceinline__ u32 rotr32(u32 x, int n) {
    return (x >> n) | (x << (32 - n));
}

__device__ __forceinline__ void G(u32 *v, int a, int b, int c, int d, u32 mx, u32 my) {
    v[a] = v[a] + v[b] + mx; v[d] = rotr32(v[d] ^ v[a], 16);
    v[c] = v[c] + v[d];      v[b] = rotr32(v[b] ^ v[c], 12);
    v[a] = v[a] + v[b] + my; v[d] = rotr32(v[d] ^ v[a], 8);
    v[c] = v[c] + v[d];      v[b] = rotr32(v[b] ^ v[c], 7);
}

// Single-block BLAKE3 compression, counter t=0. out[8] = 32-byte digest words.
__device__ void compress(const u32 *cv, const u32 *msg, u32 block_len, u32 flags, u32 *out) {
    u32 v[16];
    for (int i = 0; i < 8; i++) v[i] = cv[i];
    v[8] = IVc[0]; v[9] = IVc[1]; v[10] = IVc[2]; v[11] = IVc[3];
    v[12] = 0u; v[13] = 0u; v[14] = block_len; v[15] = flags;

    u32 m[16];
    for (int i = 0; i < 16; i++) m[i] = msg[i];

    const int PERM[16] = {2,6,3,10,7,0,4,13,1,11,12,5,9,14,15,8};
    for (int r = 0; r < 7; r++) {
        G(v, 0, 4, 8,  12, m[0],  m[1]);
        G(v, 1, 5, 9,  13, m[2],  m[3]);
        G(v, 2, 6, 10, 14, m[4],  m[5]);
        G(v, 3, 7, 11, 15, m[6],  m[7]);
        G(v, 0, 5, 10, 15, m[8],  m[9]);
        G(v, 1, 6, 11, 12, m[10], m[11]);
        G(v, 2, 7, 8,  13, m[12], m[13]);
        G(v, 3, 4, 9,  14, m[14], m[15]);
        if (r < 6) {
            u32 pm[16];
            for (int i = 0; i < 16; i++) pm[i] = m[PERM[i]];
            for (int i = 0; i < 16; i++) m[i] = pm[i];
        }
    }
    for (int i = 0; i < 8; i++) out[i] = v[i] ^ v[i + 8];
}

#define FLAGS_ROOTBLOCK 0x0Bu

// x = final hash after `iters` chain rounds. seed = BLAKE3(midstate32 || nonce_le8).
__device__ void chain_for_nonce(const u8 *midstate, u64 nonce, u32 iters, u32 *x) {
    u32 cv[8];
    for (int i = 0; i < 8; i++) cv[i] = IVc[i];

    u32 m[16];
    for (int i = 0; i < 16; i++) m[i] = 0u;
    for (int i = 0; i < 8; i++) {
        m[i] = (u32)midstate[4*i]
             | ((u32)midstate[4*i+1] << 8)
             | ((u32)midstate[4*i+2] << 16)
             | ((u32)midstate[4*i+3] << 24);
    }
    m[8] = (u32)(nonce & 0xFFFFFFFFu);
    m[9] = (u32)(nonce >> 32);

    compress(cv, m, 40u, FLAGS_ROOTBLOCK, x);          // seed (block_len = 40)
    for (u32 k = 0; k < iters; k++) {
        u32 mm[16];
        for (int i = 0; i < 16; i++) mm[i] = 0u;
        for (int i = 0; i < 8; i++) mm[i] = x[i];      // 32-byte input
        compress(cv, mm, 32u, FLAGS_ROOTBLOCK, x);
    }
}

// Selftest entry: write the final 8 words per thread (nonce = nonce_base + gid).
extern "C" __global__ void mine_chain(const u8 *midstate, u64 nonce_base, u32 iters,
                                      u32 *out_words) {
    u32 tid = blockIdx.x * blockDim.x + threadIdx.x;
    u32 x[8];
    chain_for_nonce(midstate, nonce_base + (u64)tid, iters, x);
    for (int i = 0; i < 8; i++) out_words[tid*8 + i] = x[i];
}

// final hash (bytes = little-endian per word, == blake3 as_bytes) < target (big-endian)?
__device__ int meets_target(const u32 *x, const u8 *target) {
    for (int i = 0; i < 32; i++) {
        u8 hb = (u8)((x[i >> 2] >> (8 * (i & 3))) & 0xffu);
        u8 tb = target[i];
        if (hb < tb) return 1;
        if (hb > tb) return 0;
    }
    return 0; // equal => not strictly less
}

// Production search: scan `count` nonces from nonce_base; for each whose 1M-iter
// final hash < target, append {nonce_lo, nonce_hi, 8 hash words} to `results`
// via an atomic counter. Caller pre-zeros result_count. Record stride = 10 uints.
extern "C" __global__ void search(const u8 *midstate, const u8 *target,
                                  u64 nonce_base, u32 count, u32 max_results,
                                  u32 *result_count, u32 *results) {
    u32 tid = blockIdx.x * blockDim.x + threadIdx.x;
    if (tid >= count) return;
    u64 nonce = nonce_base + (u64)tid;
    u32 x[8];
    chain_for_nonce(midstate, nonce, 1000000u, x);
    if (meets_target(x, target)) {
        u32 idx = atomicAdd(result_count, 1u);
        if (idx < max_results) {
            u32 *rec = results + idx * 10;
            rec[0] = (u32)(nonce & 0xFFFFFFFFu);
            rec[1] = (u32)(nonce >> 32);
            for (int i = 0; i < 8; i++) rec[2 + i] = x[i];
        }
    }
}
