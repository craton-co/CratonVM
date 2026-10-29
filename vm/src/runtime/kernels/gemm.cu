// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company
//
// Built-in GEMM kernels for craton.gpu.
//
// WHY THESE ARE HAND-WRITTEN AND NOT LOWERED FROM BYTECODE
//
// jit-cuda lowers a Java method to PTX when its body is a single counted
// loop, or a two-level rectangular nest that it flattens. A matrix
// multiply is three nested loops -- two parallel, one a reduction -- and
// a fast one additionally needs a shared-memory tile, a __syncthreads()
// barrier, and a 2-D block. None of those exist in the lowering's model,
// and a naive three-loop Java kernel is rejected rather than
// mis-lowered. So the kernels an inference workload actually spends its
// time in cannot come from the bytecode path at all; they ship here.
//
// THE SOURCE OF TRUTH IS THIS FILE, NOT THE .ptx BESIDE IT
//
// gemm.ptx is generated, committed, and embedded with include_str! so a
// cratonvm build needs no CUDA toolkit. Regenerate after any edit here:
//
//   nvcc -arch=compute_75 -ptx gemm.cu -o gemm.ptx
//   sed -i 's/^\.version .*/.version 6.3/' gemm.ptx      # see below
//
// A virtual arch (compute_) rather than a real one (sm_): the driver JITs
// the PTX for whatever device is present, so one artifact covers Turing
// through Blackwell instead of one per generation. 75 is the floor
// because it is the oldest CUDA 13 supports.
//
// THE SECOND COMMAND IS NOT OPTIONAL
//
// nvcc stamps the .version directive with whichever TOOLKIT ran, not with
// what the code needs. Regenerating on CUDA 13.3 wrote `.version 9.3`,
// and a driver older than the CUDA 13 line cannot parse that -- so on an
// r550-era host the module failed to load and every matmul fell back to
// the CPU, on hardware that would have run the sm_75 code perfectly.
// The .target was carefully chosen for breadth and the .version silently
// undid it.
//
// 6.3 is the floor for sm_75 (CUDA 10.0, driver r410), and it is honest:
// every mnemonic below -- ld.global.nc, ld.shared.v4.f32, bar.sync,
// fma.rn.f32, mul.wide.s32, cvta.to.global, .pragma "nounroll" -- predates
// CUDA 10. Verified by assembling the rewritten file with ptxas at 6.3,
// 7.0, 7.5 and 8.0; all four succeed. It matches what
// jit_cuda::target::min_isa_for_target(7, 5) independently reports for the
// bytecode-lowered kernels, which is not a coincidence: it is the same
// question.
//
// mod.rs fails the build if the two drift apart, and it asserts the
// .version literally so a raw nvcc regeneration cannot quietly raise the
// driver floor again.
//
// TRANSPOSE IS EXPRESSED AS STRIDES, NOT AS FOUR KERNELS
//
// Rather than NN/NT/TN/TT variants, each input carries a pair of strides:
// element (i, j) of the logical matrix lives at `i*s0 + j*s1`. A
// row-major MxK matrix is (s0, s1) = (K, 1); its transpose -- the same
// bytes read as KxM -- is (1, M). One code path, no inner-loop branch.
//
// It does not make transposition free: reading a row-major matrix
// transposed walks a column, so the loads stop coalescing. The tile
// amortises that but does not remove it.
//
// ---------------------------------------------------------------------
// TWO TILE SIZES, SELECTED BY SHAPE
// ---------------------------------------------------------------------
//
// The body below is a template so the same code emits two kernels:
//
//   craton_gemm_f32     64x64 block,  4x4 per thread   (SMALL)
//   craton_gemm_f32_lg  128x128 block, 8x8 per thread  (LARGE)
//
// Neither wins everywhere, which is why both exist. Measured on an
// RTX 2060, kernel time only, GFLOP/s:
//
//     size     4x4     8x8
//       64     7.1     6.0
//      128      58      43
//      256     437     261
//      512    1470    1296
//     1024    1973    3112
//
// The large tile is 58% faster at 1024 and progressively worse below it,
// and the reason is block count rather than anything about the inner
// loop. A 128x128 tile covers a 256x256 problem in four blocks, which
// leaves most of a 30-SM GPU idle; the 64x64 tile makes sixteen. Past
// about 512 there is enough work to fill the machine either way and the
// larger tile's arithmetic intensity -- 64 FMAs per 16 shared loads
// against 16 per 8 -- takes over.
//
// The host picks by output size; see `gemm_entry` in mod.rs.
//
// REGISTER BLOCKING
//
// Each thread computes a TM x TN block of C with those TM*TN
// accumulators in registers, so one K step reads TM values of A and TN
// of B and does TM*TN FMAs against them. The alternative -- one output
// per thread -- is two shared loads per FMA, and shared bandwidth, not
// the FMA units, is what that runs out of.
//
// HALF-TILE SPLIT
//
// A thread's rows and columns are not contiguous. They are TM/4 groups
// of four, spread evenly across the block tile:
//
//     row of group g = ty*4 + g*(BM/(TM/4))
//
// A 128-bit shared load is serviced in phases of eight threads, and
// eight threads times sixteen bytes is 128 bytes -- exactly the 32
// banks. Consecutive threads therefore have to be sixteen bytes apart
// for a phase to sweep the banks once. Eight contiguous columns at tx*8
// would put them thirty-two apart, spanning two bank rows, and every
// load would take a 2-way conflict. At tx*4 the spacing is right and a
// thread simply issues TM/4 loads instead of one.

#include <cuda_fp16.h>

// K-depth staged in shared memory per iteration. Shared by both shapes.
#define BK 16

// Vector width of a shared-memory load, in floats. A float4 is 128 bits,
// the widest shared load, and TM/TN must be multiples of it.
#define VEC 4

// -------------------------------------------------------------------------
// The kernel body, parameterised by tile shape.
//
// `T` is the operand element type and `Load` converts one to float, which
// is what lets the fp32 and fp16 kernels share every line below: the
// operands are widened entering shared memory, so the inner product is
// the same fp32 arithmetic either way. The accumulator is fp32 in both
// -- summing K terms in half loses accuracy fast enough to show in
// generated tokens at the K a transformer uses.
// -------------------------------------------------------------------------
template <int BM, int BN, int TM, int TN, typename T, typename Load>
__device__ __forceinline__ void gemm_body(
    const T* __restrict__ A,
    const T* __restrict__ B,
    float* __restrict__ C,
    int M, int N, int K,
    int as0, int as1,
    int bs0, int bs1,
    Load load)
{
    constexpr int THREADS_X = BN / TN;
    constexpr int THREADS_Y = BM / TM;
    constexpr int THREADS = THREADS_X * THREADS_Y;
    constexpr int A_GROUPS = TM / VEC;      // 128-bit loads per thread, A
    constexpr int B_GROUPS = TN / VEC;      // ... and B

    // As is stored TRANSPOSED -- As[k][m], not As[m][k] -- so the inner
    // loop's read of VEC consecutive rows of A is VEC consecutive floats.
    // Stored the other way each read would stride by BK and collide.
    __shared__ __align__(16) float As[BK][BM];
    __shared__ __align__(16) float Bs[BK][BN];

    const int tx = threadIdx.x;
    const int ty = threadIdx.y;
    const int tid = ty * THREADS_X + tx;

    const int row0 = blockIdx.y * BM;
    const int col0 = blockIdx.x * BN;

    float acc[TM][TN];
    #pragma unroll
    for (int i = 0; i < TM; ++i) {
        #pragma unroll
        for (int j = 0; j < TN; ++j) {
            acc[i][j] = 0.0f;
        }
    }

    const int tiles = (K + BK - 1) / BK;

    for (int t = 0; t < tiles; ++t) {
        const int kBase = t * BK;

        // Cooperative loads. The flat index runs the contiguous dimension
        // fastest, so consecutive threads read consecutive elements of a
        // row -- coalesced when the operand is untransposed. Out of range
        // contributes zero, the identity for the accumulation, so ragged
        // M/N/K cost a predicate rather than a cleanup kernel.
        #pragma unroll
        for (int f = tid; f < BM * BK; f += THREADS) {
            const int m = f / BK;
            const int kk = f % BK;
            const int gRow = row0 + m;
            const int gCol = kBase + kk;
            As[kk][m] = (gRow < M && gCol < K)
                    ? load(A[gRow * as0 + gCol * as1])
                    : 0.0f;
        }
        #pragma unroll
        for (int f = tid; f < BK * BN; f += THREADS) {
            const int kk = f / BN;
            const int n = f % BN;
            const int gRow = kBase + kk;
            const int gCol = col0 + n;
            Bs[kk][n] = (gRow < K && gCol < N)
                    ? load(B[gRow * bs0 + gCol * bs1])
                    : 0.0f;
        }

        __syncthreads();

        #pragma unroll
        for (int kk = 0; kk < BK; ++kk) {
            float aReg[TM];
            float bReg[TN];
            // One 128-bit load per group; the groups are spread across
            // the tile so each load's phase sweeps the banks once.
            #pragma unroll
            for (int g = 0; g < A_GROUPS; ++g) {
                const float4 v = *reinterpret_cast<const float4*>(
                        &As[kk][ty * VEC + g * (BM / A_GROUPS)]);
                aReg[g * VEC + 0] = v.x;
                aReg[g * VEC + 1] = v.y;
                aReg[g * VEC + 2] = v.z;
                aReg[g * VEC + 3] = v.w;
            }
            #pragma unroll
            for (int g = 0; g < B_GROUPS; ++g) {
                const float4 v = *reinterpret_cast<const float4*>(
                        &Bs[kk][tx * VEC + g * (BN / B_GROUPS)]);
                bReg[g * VEC + 0] = v.x;
                bReg[g * VEC + 1] = v.y;
                bReg[g * VEC + 2] = v.z;
                bReg[g * VEC + 3] = v.w;
            }
            #pragma unroll
            for (int i = 0; i < TM; ++i) {
                #pragma unroll
                for (int j = 0; j < TN; ++j) {
                    acc[i][j] = fmaf(aReg[i], bReg[j], acc[i][j]);
                }
            }
        }

        // Nobody may overwrite the tile until every thread has finished
        // reading it. Dropping this second barrier is the classic tiled
        // GEMM race: invisible on small inputs and wrong at scale.
        __syncthreads();
    }

    #pragma unroll
    for (int i = 0; i < TM; ++i) {
        // Accumulator i belongs to group i/VEC, which sits
        // g*(BM/A_GROUPS) rows into the tile.
        const int localRow = ty * VEC + (i / VEC) * (BM / A_GROUPS) + (i % VEC);
        const int gRow = row0 + localRow;
        if (gRow >= M) {
            continue;
        }
        #pragma unroll
        for (int j = 0; j < TN; ++j) {
            const int localCol = tx * VEC + (j / VEC) * (BN / B_GROUPS) + (j % VEC);
            const int gCol = col0 + localCol;
            if (gCol < N) {
                C[gRow * N + gCol] = acc[i][j];
            }
        }
    }
}

// Widening functors. Passing these rather than branching keeps the fp16
// conversion out of the inner product entirely.
struct LoadF32 {
    __device__ __forceinline__ float operator()(float v) const { return v; }
};
struct LoadF16 {
    __device__ __forceinline__ float operator()(__half v) const {
        return __half2float(v);
    }
};

// -------------------------------------------------------------------------
// The four GEMM entry points. Small tile for shapes that would not make
// enough 128x128 blocks to fill the device; large tile above that.
// -------------------------------------------------------------------------

extern "C" __global__ void craton_gemm_f32(
    const float* __restrict__ A, const float* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<64, 64, 4, 4>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF32{});
}

extern "C" __global__ void craton_gemm_f32_lg(
    const float* __restrict__ A, const float* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<128, 128, 8, 8>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF32{});
}

extern "C" __global__ void craton_gemm_f16(
    const __half* __restrict__ A, const __half* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<64, 64, 4, 4>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF16{});
}

extern "C" __global__ void craton_gemm_f16_lg(
    const __half* __restrict__ A, const __half* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<128, 128, 8, 8>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF16{});
}

// -------------------------------------------------------------------------
// Bulk fp16 <-> fp32 conversion.
//
// Element-wise and 1-D, so these could in principle be lowered from Java
// bytecode -- but Java has no half type, so a Java kernel would have to
// take short[] and do the bit manipulation by hand, in a lowering that
// has no fp16 support to lower it to.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_h2f(
    const __half* __restrict__ src, float* __restrict__ dst, int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = __half2float(src[i]);
    }
}

extern "C" __global__ void craton_f2h(
    const float* __restrict__ src, __half* __restrict__ dst, int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = __float2half(src[i]);
    }
}
