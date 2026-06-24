// midstate.cl — Midstate PoW kernel in OpenCL C.
//
// BLAKE3 single-block compression + the 1,000,000-iteration sequential chain.
// CONSENSUS-CRITICAL: must be bit-exact with src/pow.rs and the golden vectors
// (a713dea1…/8ac4d9ef…). Ported line-for-line from the CUDA reference
// (gpu-miner/kernel/midstate_blake3.cu); the math is identical integer ops.

__constant uint IVc[8] = {
    0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
    0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u
};

static uint rotr32(uint x, int n) { return (x >> n) | (x << (32 - n)); }

static void G(uint *v, int a, int b, int c, int d, uint mx, uint my) {
    v[a] = v[a] + v[b] + mx; v[d] = rotr32(v[d] ^ v[a], 16);
    v[c] = v[c] + v[d];      v[b] = rotr32(v[b] ^ v[c], 12);
    v[a] = v[a] + v[b] + my; v[d] = rotr32(v[d] ^ v[a], 8);
    v[c] = v[c] + v[d];      v[b] = rotr32(v[b] ^ v[c], 7);
}

// Single-block BLAKE3 compression, counter t=0. out[8] = 32-byte digest words.
static void compress(const uint *cv, const uint *msg, uint block_len, uint flags, uint *out) {
    uint v[16];
    for (int i = 0; i < 8; i++) v[i] = cv[i];
    v[8] = IVc[0]; v[9] = IVc[1]; v[10] = IVc[2]; v[11] = IVc[3];
    v[12] = 0u; v[13] = 0u; v[14] = block_len; v[15] = flags;

    uint m[16];
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
            uint pm[16];
            for (int i = 0; i < 16; i++) pm[i] = m[PERM[i]];
            for (int i = 0; i < 16; i++) m[i] = pm[i];
        }
    }
    for (int i = 0; i < 8; i++) out[i] = v[i] ^ v[i + 8];
}

#define FLAGS_ROOTBLOCK 0x0Bu

// x = final hash after `iters` chain rounds. seed = BLAKE3(midstate32 || nonce_le8).
static void chain_for_nonce(__global const uchar *midstate, ulong nonce, uint iters, uint *x) {
    uint cv[8];
    for (int i = 0; i < 8; i++) cv[i] = IVc[i];

    uint m[16];
    for (int i = 0; i < 16; i++) m[i] = 0u;
    for (int i = 0; i < 8; i++) {
        m[i] = (uint)midstate[4*i]
             | ((uint)midstate[4*i+1] << 8)
             | ((uint)midstate[4*i+2] << 16)
             | ((uint)midstate[4*i+3] << 24);
    }
    m[8] = (uint)(nonce & 0xFFFFFFFFu);
    m[9] = (uint)(nonce >> 32);

    compress(cv, m, 40u, FLAGS_ROOTBLOCK, x);          // seed (block_len = 40)
    for (uint k = 0; k < iters; k++) {
        uint mm[16];
        for (int i = 0; i < 16; i++) mm[i] = 0u;
        for (int i = 0; i < 8; i++) mm[i] = x[i];      // 32-byte input
        compress(cv, mm, 32u, FLAGS_ROOTBLOCK, x);
    }
}

// Selftest entry: write the final 8 words per work-item (nonce = nonce_base + gid).
__kernel void mine_chain(__global const uchar *midstate, ulong nonce_base, uint iters,
                         __global uint *out_words) {
    uint tid = get_global_id(0);
    uint x[8];
    chain_for_nonce(midstate, nonce_base + (ulong)tid, iters, x);
    for (int i = 0; i < 8; i++) out_words[tid*8 + i] = x[i];
}

// final hash (bytes = little-endian per word, == blake3 as_bytes) < target (big-endian)?
static int meets_target(const uint *x, __global const uchar *target) {
    for (int i = 0; i < 32; i++) {
        uchar hb = (uchar)((x[i >> 2] >> (8 * (i & 3))) & 0xffu);
        uchar tb = target[i];
        if (hb < tb) return 1;
        if (hb > tb) return 0;
    }
    return 0; // equal => not strictly less
}

// Production search: scan `count` nonces from nonce_base; for each whose 1M-iter
// final hash < target, append {nonce_lo, nonce_hi, 8 hash words} to `results`
// via an atomic counter. Caller pre-zeros result_count. Record stride = 10 uints.
__kernel void search(__global const uchar *midstate, __global const uchar *target,
                     ulong nonce_base, uint count, uint max_results,
                     __global uint *result_count, __global uint *results) {
    uint tid = get_global_id(0);
    if (tid >= count) return;
    ulong nonce = nonce_base + (ulong)tid;
    uint x[8];
    chain_for_nonce(midstate, nonce, 1000000u, x);
    if (meets_target(x, target)) {
        uint idx = atomic_add(result_count, 1u);
        if (idx < max_results) {
            __global uint *rec = results + idx * 10;
            rec[0] = (uint)(nonce & 0xFFFFFFFFu);
            rec[1] = (uint)(nonce >> 32);
            for (int i = 0; i < 8; i++) rec[2 + i] = x[i];
        }
    }
}
