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
//
// A virtual arch (compute_) rather than a real one (sm_): the driver JITs
// the PTX for whatever device is present, so one artifact covers Turing
// through Blackwell instead of one per generation.
//
// 75 is the floor because it is the oldest CUDA 13 still supports --
// `nvcc --list-gpu-arch` starts at compute_75, Volta having been dropped.
// Anything older needs a CUDA 12 toolkit to regenerate, which is the
// tradeoff: newer toolkit, narrower back-compat. Half-precision
// arithmetic needs sm_53+, so it is not the binding constraint here.
//
// mod.rs fails the build if the two drift apart.
//
// TRANSPOSE IS EXPRESSED AS STRIDES, NOT AS FOUR KERNELS
//
// Rather than NN/NT/TN/TT variants, each input carries a pair of strides:
// element (i, j) of the logical matrix lives at `i*s0 + j*s1`. A
// row-major MxK matrix is (s0, s1) = (K, 1); its transpose -- the same
// bytes read as KxM -- is (1, M). The host computes the pair, so the
// kernel has one code path and no branches in the inner loop.
//
// What this does NOT make free is coalescing. Reading a row-major matrix
// transposed walks a column, so consecutive threads touch addresses one
// row apart instead of adjacent ones, and the loads stop coalescing. The
// tile still amortises it, but a transposed operand is measurably slower
// than a non-transposed one.
//
// ---------------------------------------------------------------------
// REGISTER BLOCKING
// ---------------------------------------------------------------------
//
// The first version of these kernels gave each thread one output element:
// per step of the K loop it read one value of A and one of B from shared
// memory and did a single fused multiply-add. Two shared loads per FMA,
// and shared-memory bandwidth -- not the FMA units -- is what ran out.
// Measured on an RTX 2060 it reached 108 GFLOP/s at 1024^3, about 1.7% of
// the card's ~6.5 TFLOP/s fp32 peak.
//
// Now each thread computes a TM x TN block of C and keeps those TM*TN
// accumulators in registers. One step of the K loop reads TM values of A
// and TN of B into registers, then does TM*TN FMAs against them. At 4x4
// that is 16 FMAs per 8 shared loads instead of 1 per 2 -- a 4x cut in
// shared traffic per flop, paid for in registers, which is where the
// slack was.
//
// The block tile is BM x BN of C, accumulated over the K dimension BK at
// a time, by (BM/TM) x (BN/TN) threads.
//
// Register blocking alone bought less than expected -- +19% fp32 at
// 1024^3 -- because it exposed a second bottleneck rather than removing
// the last one. Reading TN consecutive floats out of Bs as four scalars
// strides the warp by 4 across 32 banks, hitting 8 of them and taking a
// 4-way conflict on every access. The inner loop now does one 128-bit
// load per operand instead, which is why TM and TN are 4: a float4 is
// exactly the vector width, and both tiles are aligned so the load is
// legal.

#include <cuda_fp16.h>

// Block tile: each thread block computes a BM x BN patch of C.
#define BM 64
#define BN 64
// K-depth staged in shared memory per iteration.
#define BK 16
// Thread tile: each thread computes TM x TN of that patch, in registers.
#define TM 4
#define TN 4

// Derived: 16 x 16 = 256 threads per block, each holding 16 accumulators.
//
// Shared memory is BK*(BM + BN) floats = 8 KB, which against a 64 KB
// budget leaves room for several concurrent blocks; the accumulators cost
// 16 registers per thread plus addressing, well inside the 255-register
// limit.
#define THREADS_X (BN / TN)
#define THREADS_Y (BM / TM)
#define THREADS (THREADS_X * THREADS_Y)

// -------------------------------------------------------------------------
// C[M,N] = A[M,K] * B[K,N], row-major, fp32 in and out.
//
// Bounds are checked on the cooperative loads and on the final store, so
// M, N and K need not be multiples of anything. An out-of-range load
// contributes zero, which is the identity for the accumulation -- a
// predicate rather than a separate cleanup kernel.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_gemm_f32(
    const float* __restrict__ A,
    const float* __restrict__ B,
    float* __restrict__ C,
    int M, int N, int K,
    int as0, int as1,      // A: element (i,p) at i*as0 + p*as1
    int bs0, int bs1)      // B: element (p,j) at p*bs0 + j*bs1
{
    // As is stored TRANSPOSED -- As[k][m], not As[m][k] -- so the inner
    // loop's read of TM consecutive rows of A is TM consecutive floats in
    // shared memory. Stored the other way each of those reads would
    // stride by BK and collide on the same bank.
    __shared__ __align__(16) float As[BK][BM];
    __shared__ __align__(16) float Bs[BK][BN];

    const int tx = threadIdx.x;              // [0, THREADS_X)
    const int ty = threadIdx.y;              // [0, THREADS_Y)
    const int tid = ty * THREADS_X + tx;     // [0, THREADS)

    const int row0 = blockIdx.y * BM;        // first row of C this block owns
    const int col0 = blockIdx.x * BN;        // first column

    // This thread's TM x TN patch within the block's BM x BN patch.
    const int threadRow = ty * TM;
    const int threadCol = tx * TN;

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

        // ---- cooperative load of A's tile, transposed into As ----
        //
        // BM*BK = 1024 elements over THREADS threads. The flat index runs
        // kk fastest, so consecutive threads read consecutive elements of
        // a row of A -- coalesced when A is untransposed, which is the
        // common case.
        #pragma unroll
        for (int f = tid; f < BM * BK; f += THREADS) {
            const int m = f / BK;
            const int kk = f % BK;
            const int gRow = row0 + m;
            const int gCol = kBase + kk;
            As[kk][m] = (gRow < M && gCol < K)
                    ? A[gRow * as0 + gCol * as1]
                    : 0.0f;
        }

        // ---- cooperative load of B's tile ----
        //
        // BK*BN = 1024, with n fastest so consecutive threads again read
        // consecutive elements of a row.
        #pragma unroll
        for (int f = tid; f < BK * BN; f += THREADS) {
            const int kk = f / BN;
            const int n = f % BN;
            const int gRow = kBase + kk;
            const int gCol = col0 + n;
            Bs[kk][n] = (gRow < K && gCol < N)
                    ? B[gRow * bs0 + gCol * bs1]
                    : 0.0f;
        }

        __syncthreads();

        // ---- the register-blocked inner product ----
        //
        // TM + TN shared reads feed TM * TN fused multiply-adds. That
        // ratio is the whole point.
        #pragma unroll
        for (int kk = 0; kk < BK; ++kk) {
            // One 128-bit load each instead of four 32-bit ones.
            //
            // The scalar form read Bs[kk][tx*4 + j]: across a warp that is
            // a stride of 4 floats, which over 32 banks lands on only
            // 32/gcd(4,32) = 8 distinct banks and costs a 4-way conflict
            // on every access. A float4 load is a single transaction per
            // thread and consecutive threads cover consecutive 16-byte
            // chunks, so the warp sweeps the banks exactly once.
            //
            // Legal because both tiles are __align__(16) and both
            // threadRow and threadCol are multiples of 4 floats.
            const float4 a4 = *reinterpret_cast<const float4*>(&As[kk][threadRow]);
            const float4 b4 = *reinterpret_cast<const float4*>(&Bs[kk][threadCol]);
            const float aReg[TM] = {a4.x, a4.y, a4.z, a4.w};
            const float bReg[TN] = {b4.x, b4.y, b4.z, b4.w};
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

    // ---- store, guarded ----
    #pragma unroll
    for (int i = 0; i < TM; ++i) {
        const int gRow = row0 + threadRow + i;
        if (gRow >= M) {
            continue;
        }
        #pragma unroll
        for (int j = 0; j < TN; ++j) {
            const int gCol = col0 + threadCol + j;
            if (gCol < N) {
                C[gRow * N + gCol] = acc[i][j];
            }
        }
    }
}

// -------------------------------------------------------------------------
// C[M,N] = A[M,K] * B[K,N] with fp16 inputs and an fp32 accumulator.
//
// Identical structure; the operands are converted to float as they enter
// shared memory, so the inner product is the same fp32 arithmetic. The
// accumulator is not negotiable: summing K terms in half loses accuracy
// fast enough to show in generated tokens at the K a transformer uses.
//
// Deliberately NOT wmma/tensor cores. Those need the operand fragments in
// a specific layout and add a correctness surface that is hard to check
// against a CPU reference. Worth revisiting now that the shared-memory
// bottleneck is gone, but it is a separate change with its own risks.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_gemm_f16(
    const __half* __restrict__ A,
    const __half* __restrict__ B,
    float* __restrict__ C,
    int M, int N, int K,
    int as0, int as1,      // see craton_gemm_f32 for the stride convention
    int bs0, int bs1)
{
    __shared__ __align__(16) float As[BK][BM];
    __shared__ __align__(16) float Bs[BK][BN];

    const int tx = threadIdx.x;
    const int ty = threadIdx.y;
    const int tid = ty * THREADS_X + tx;

    const int row0 = blockIdx.y * BM;
    const int col0 = blockIdx.x * BN;

    const int threadRow = ty * TM;
    const int threadCol = tx * TN;

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

        #pragma unroll
        for (int f = tid; f < BM * BK; f += THREADS) {
            const int m = f / BK;
            const int kk = f % BK;
            const int gRow = row0 + m;
            const int gCol = kBase + kk;
            As[kk][m] = (gRow < M && gCol < K)
                    ? __half2float(A[gRow * as0 + gCol * as1])
                    : 0.0f;
        }

        #pragma unroll
        for (int f = tid; f < BK * BN; f += THREADS) {
            const int kk = f / BN;
            const int n = f % BN;
            const int gRow = kBase + kk;
            const int gCol = col0 + n;
            Bs[kk][n] = (gRow < K && gCol < N)
                    ? __half2float(B[gRow * bs0 + gCol * bs1])
                    : 0.0f;
        }

        __syncthreads();

        #pragma unroll
        for (int kk = 0; kk < BK; ++kk) {
            // One 128-bit load each instead of four 32-bit ones.
            //
            // The scalar form read Bs[kk][tx*4 + j]: across a warp that is
            // a stride of 4 floats, which over 32 banks lands on only
            // 32/gcd(4,32) = 8 distinct banks and costs a 4-way conflict
            // on every access. A float4 load is a single transaction per
            // thread and consecutive threads cover consecutive 16-byte
            // chunks, so the warp sweeps the banks exactly once.
            //
            // Legal because both tiles are __align__(16) and both
            // threadRow and threadCol are multiples of 4 floats.
            const float4 a4 = *reinterpret_cast<const float4*>(&As[kk][threadRow]);
            const float4 b4 = *reinterpret_cast<const float4*>(&Bs[kk][threadCol]);
            const float aReg[TM] = {a4.x, a4.y, a4.z, a4.w};
            const float bReg[TN] = {b4.x, b4.y, b4.z, b4.w};
            #pragma unroll
            for (int i = 0; i < TM; ++i) {
                #pragma unroll
                for (int j = 0; j < TN; ++j) {
                    acc[i][j] = fmaf(aReg[i], bReg[j], acc[i][j]);
                }
            }
        }

        __syncthreads();
    }

    #pragma unroll
    for (int i = 0; i < TM; ++i) {
        const int gRow = row0 + threadRow + i;
        if (gRow >= M) {
            continue;
        }
        #pragma unroll
        for (int j = 0; j < TN; ++j) {
            const int gCol = col0 + threadCol + j;
            if (gCol < N) {
                C[gRow * N + gCol] = acc[i][j];
            }
        }
    }
}

// -------------------------------------------------------------------------
// Bulk fp16 -> fp32 and fp32 -> fp16 conversion.
//
// The host side needs these to move a weight tensor onto the device
// without a per-element round trip, and to read a result back.
// Element-wise and 1-D, so they could in principle be lowered from Java
// bytecode -- but Java has no half type, so a Java kernel would have to
// take short[] and do the bit manipulation by hand, in a lowering that
// has no fp16 support to lower it to.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_h2f(
    const __half* __restrict__ src,
    float* __restrict__ dst,
    int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = __half2float(src[i]);
    }
}

extern "C" __global__ void craton_f2h(
    const float* __restrict__ src,
    __half* __restrict__ dst,
    int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = __float2half(src[i]);
    }
}
