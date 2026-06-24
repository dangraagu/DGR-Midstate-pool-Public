/* OpenCL bit-exact selftest for midstate.cl.
 *
 * Runs the 1,000,000-iteration chain for nonce 0 and 1 over an all-zero midstate
 * on whatever OpenCL device is present, and checks the two golden vectors. A
 * kernel that passes here on ANY conformant OpenCL runtime is bit-exact on all
 * of them (integer math is deterministic). Build + run (e.g. under POCL):
 *   gcc selftest.c -o ocl_selftest -lOpenCL && ./ocl_selftest midstate.cl
 */
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *read_file(const char *path, size_t *len) {
    FILE *f = fopen(path, "rb");
    if (!f) { perror("fopen"); exit(2); }
    fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
    char *buf = (char *)malloc(n + 1);
    if (fread(buf, 1, n, f) != (size_t)n) { perror("fread"); exit(2); }
    buf[n] = 0; *len = (size_t)n; fclose(f);
    return buf;
}

#define CK(x) do { cl_int _e = (x); if (_e != CL_SUCCESS) { \
    fprintf(stderr, "CL error %d at %s:%d\n", _e, __FILE__, __LINE__); exit(3); } } while (0)

int main(int argc, char **argv) {
    const char *clpath = argc > 1 ? argv[1] : "midstate.cl";
    size_t srclen; char *src = read_file(clpath, &srclen);

    cl_platform_id plat; CK(clGetPlatformIDs(1, &plat, NULL));
    cl_device_id dev; CK(clGetDeviceIDs(plat, CL_DEVICE_TYPE_ALL, 1, &dev, NULL));
    char dname[256] = {0}; clGetDeviceInfo(dev, CL_DEVICE_NAME, sizeof(dname), dname, NULL);
    printf("device: %s\n", dname);

    cl_int err;
    cl_context ctx = clCreateContext(NULL, 1, &dev, NULL, NULL, &err); CK(err);
    cl_command_queue q = clCreateCommandQueueWithProperties(ctx, dev, NULL, &err); CK(err);
    cl_program prog = clCreateProgramWithSource(ctx, 1, (const char **)&src, &srclen, &err); CK(err);
    err = clBuildProgram(prog, 1, &dev, "", NULL, NULL);
    if (err != CL_SUCCESS) {
        char log[16384] = {0};
        clGetProgramBuildInfo(prog, dev, CL_PROGRAM_BUILD_LOG, sizeof(log), log, NULL);
        fprintf(stderr, "BUILD FAILED:\n%s\n", log); exit(4);
    }
    cl_kernel k = clCreateKernel(prog, "mine_chain", &err); CK(err);

    unsigned char midstate[32]; memset(midstate, 0, sizeof(midstate));
    cl_mem d_mid = clCreateBuffer(ctx, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR, 32, midstate, &err); CK(err);
    cl_mem d_out = clCreateBuffer(ctx, CL_MEM_WRITE_ONLY, 64, NULL, &err); CK(err);

    cl_ulong nonce_base = 0; cl_uint iters = 1000000u;
    CK(clSetKernelArg(k, 0, sizeof(cl_mem), &d_mid));
    CK(clSetKernelArg(k, 1, sizeof(cl_ulong), &nonce_base));
    CK(clSetKernelArg(k, 2, sizeof(cl_uint), &iters));
    CK(clSetKernelArg(k, 3, sizeof(cl_mem), &d_out));

    size_t global = 2; // nonce 0 and 1
    CK(clEnqueueNDRangeKernel(q, k, 1, NULL, &global, NULL, 0, NULL, NULL));
    CK(clFinish(q));

    cl_uint out[16];
    CK(clEnqueueReadBuffer(q, d_out, CL_TRUE, 0, 64, out, 0, NULL, NULL));

    char hex[2][65];
    for (int j = 0; j < 2; j++) {
        int p = 0;
        for (int i = 0; i < 8; i++)
            for (int b = 0; b < 4; b++) {
                unsigned char byte = (out[j * 8 + i] >> (8 * b)) & 0xff; // little-endian per word
                sprintf(hex[j] + p, "%02x", byte); p += 2;
            }
        hex[j][64] = 0;
    }
    const char *g0 = "a713dea125b1e8bf085776df4a201a2021745a72a324f3984a7d16083d691369";
    const char *g1 = "8ac4d9effd5052aa95505848f8ce6995b5f6a708bdab3b0994a161d58ee419b9";
    printf("nonce0 = %s\ngolden = %s\n", hex[0], g0);
    printf("nonce1 = %s\ngolden = %s\n", hex[1], g1);
    int ok = (strcmp(hex[0], g0) == 0) && (strcmp(hex[1], g1) == 0);
    printf("%s\n", ok ? "OPENCL_GOLDEN_PASS" : "OPENCL_GOLDEN_FAIL");
    return ok ? 0 : 1;
}
