// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company
//
// Built-in GEMM kernels for craton.gpu.
//
// WHY THESE ARE HAND-WRITTEN AND NOT LOWERED FROM BYTECODE
//
// jit-cuda lowers a Java method to PTX when its body is a single counted
// loop, or a two-level rectangular nest that it flattens. A matrix
// multiply is three nested loops -- two parallel, one reduction -- and a
// fast one additionally needs a shared-memory tile, a __syncthreads()
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
// gemm_ptx_matches_source() in mod.rs fails the build if the two drift.

#include <cuda_fp16.h>

// Tile edge. 16x16 = 256 threads, one per output element of the tile.
//
// 16 rather than 32: a 32x32 tile of floats is two 4 KB shared arrays,
// which on a 64 KB-per-SM budget caps occupancy at 8 blocks; at 16 the
// same budget holds 32. It is also the largest tile for which the
// K-remainder guard below stays a single predicated load rather than a
// loop.
#define TILE 16

// -------------------------------------------------------------------------
// C[M,N] = A[M,K] * B[K,N], row-major, fp32 in and out.
//
// Every matrix is row-major and unpadded, matching a Java float[] holding
// a flattened 2-D array -- the layout craton.gpu.GpuArray hands over. No
// transpose variants: a caller that needs one transposes on the host,
// which is honest about the cost rather than hiding a second kernel
// behind the same name.
//
// Bounds are checked on both loads and the store, so M, N and K need not
// be multiples of TILE. The guard costs a predicate per load and buys the
// ability to run a 4096x11008 projection without padding it first.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_gemm_f32(
    const float* __restrict__ A,
    const float* __restrict__ B,
    float* __restrict__ C,
    int M, int N, int K)
{
    __shared__ float As[TILE][TILE];
    __shared__ float Bs[TILE][TILE];

    const int tx = threadIdx.x;
    const int ty = threadIdx.y;
    const int row = blockIdx.y * TILE + ty;
    const int col = blockIdx.x * TILE + tx;

    // Accumulate in a register, not in C: one global write per output
    // element instead of one per K-tile.
    float acc = 0.0f;

    const int tiles = (K + TILE - 1) / TILE;
    for (int t = 0; t < tiles; ++t) {
        const int aCol = t * TILE + tx;
        const int bRow = t * TILE + ty;

        As[ty][tx] = (row < M && aCol < K) ? A[row * K + aCol] : 0.0f;
        Bs[ty][tx] = (bRow < K && col < N) ? B[bRow * N + col] : 0.0f;

        // Both halves of the tile must be resident before anyone reads it.
        __syncthreads();

        #pragma unroll
        for (int i = 0; i < TILE; ++i) {
            acc = fmaf(As[ty][i], Bs[i][tx], acc);
        }

        // And nobody may overwrite the tile until every thread is done
        // reading it. Dropping this second barrier is the classic tiled-GEMM
        // race: it is invisible at TILE=16 on small inputs and wrong at scale.
        __syncthreads();
    }

    if (row < M && col < N) {
        C[row * N + col] = acc;
    }
}

// -------------------------------------------------------------------------
// C[M,N] = A[M,K] * B[K,N] with fp16 inputs and an fp32 accumulator.
//
// This is the shape that matters for inference. Weights dominate the
// memory traffic of a decode step and fp16 halves it; the accumulator
// stays fp32 because summing K terms in fp16 loses accuracy fast -- at
// K=4096 the relative error of an fp16 accumulator is percent-scale,
// which is visible in generated tokens.
//
// Deliberately NOT wmma/tensor cores. Those need the operand fragments
// laid out in a specific way, add a correctness surface that is hard to
// test against a CPU reference, and buy throughput this library cannot
// yet feed -- a dispatch costs ~23 us and these kernels run in tens of
// microseconds, so the launch path is the bound, not the math. Halving
// the bytes moved is the win available today.
//
// C is fp32: the caller usually wants the result at full precision (a
// logits vector, or an activation about to be normalised), and writing
// fp16 would round twice for nothing.
// -------------------------------------------------------------------------
extern "C" __global__ void craton_gemm_f16(
    const __half* __restrict__ A,
    const __half* __restrict__ B,
    float* __restrict__ C,
    int M, int N, int K)
{
    // Tiles are staged as fp32. Shared memory is not the constraint at
    // TILE=16 (2 KB total), and converting once on load beats converting
    // inside the inner product, where each value is read TILE times.
    __shared__ float As[TILE][TILE];
    __shared__ float Bs[TILE][TILE];

    const int tx = threadIdx.x;
    const int ty = threadIdx.y;
    const int row = blockIdx.y * TILE + ty;
    const int col = blockIdx.x * TILE + tx;

    float acc = 0.0f;

    const int tiles = (K + TILE - 1) / TILE;
    for (int t = 0; t < tiles; ++t) {
        const int aCol = t * TILE + tx;
        const int bRow = t * TILE + ty;

        As[ty][tx] = (row < M && aCol < K) ? __half2float(A[row * K + aCol]) : 0.0f;
        Bs[ty][tx] = (bRow < K && col < N) ? __half2float(B[bRow * N + col]) : 0.0f;

        __syncthreads();

        #pragma unroll
        for (int i = 0; i < TILE; ++i) {
            acc = fmaf(As[ty][i], Bs[i][tx], acc);
        }

        __syncthreads();
    }

    if (row < M && col < N) {
        C[row * N + col] = acc;
    }
}

// -------------------------------------------------------------------------
// Bulk fp16 -> fp32 and fp32 -> fp16 conversion.
//
// The host side needs these to move a weight tensor onto the device
// without a per-element JNI-ish round trip, and to read a result back.
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
