// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2026 Craton Software Company
//
// Self-contained HIP translation of `gemm.cu` for hiprtc compilation
// (no external `#include` headers required).

#define BK 16
#define VEC 4

static __device__ __forceinline__ float craton_bits_h2f(unsigned short h) {
    unsigned int sign = ((unsigned int)(h & 0x8000u)) << 16;
    unsigned int exp = (unsigned int)((h >> 10) & 0x1Fu);
    unsigned int mant = (unsigned int)(h & 0x03FFu);
    unsigned int out;
    if (exp == 0u) {
        if (mant == 0u) {
            out = sign;
        } else {
            unsigned int e = 113u;
            while ((mant & 0x0400u) == 0u) {
                mant <<= 1;
                e--;
            }
            mant &= 0x03FFu;
            out = sign | (e << 23) | (mant << 13);
        }
    } else if (exp == 31u) {
        out = sign | 0x7F800000u | (mant << 13);
        if (mant != 0u) {
            out |= 0x00400000u;
        }
    } else {
        out = sign | ((exp + 112u) << 23) | (mant << 13);
    }
    return __uint_as_float(out);
}

static __device__ __forceinline__ unsigned short craton_bits_f2h(float f) {
    unsigned int bits = __float_as_uint(f);
    unsigned int sign = (bits >> 16) & 0x8000u;
    unsigned int rest = bits & 0x7FFFFFFFu;
    if (rest >= 0x47800000u) {
        if (rest > 0x7F800000u) {
            return (unsigned short)(sign | 0x7E00u | ((rest >> 13) & 0x03FFu));
        }
        return (unsigned short)(sign | 0x7C00u);
    }
    if (rest >= 0x38800000u) {
        unsigned int mant_odd = (rest >> 13) & 1u;
        unsigned int rounded = (rest - 0x38000000u) + 0x0FFFu + mant_odd;
        unsigned int h = rounded >> 13;
        if (h >= 0x7C00u) {
            h = 0x7C00u;
        }
        return (unsigned short)(sign | h);
    }
    if (rest < 0x33000000u) {
        return (unsigned short)sign;
    }
    int exp = (int)(rest >> 23);
    unsigned int mant = (rest & 0x007FFFFFu) | 0x00800000u;
    unsigned int shift = (unsigned int)(126 - exp);
    unsigned int half = mant >> shift;
    unsigned int rem = mant & ((1u << shift) - 1u);
    unsigned int halfway = 1u << (shift - 1u);
    if (rem > halfway || (rem == halfway && (half & 1u) != 0u)) {
        half++;
    }
    return (unsigned short)(sign | half);
}

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
    constexpr int A_GROUPS = TM / VEC;
    constexpr int B_GROUPS = TN / VEC;

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

        __syncthreads();
    }

    #pragma unroll
    for (int i = 0; i < TM; ++i) {
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

struct LoadF32 {
    __device__ __forceinline__ float operator()(float v) const { return v; }
};
struct LoadF16 {
    __device__ __forceinline__ float operator()(unsigned short v) const {
        return craton_bits_h2f(v);
    }
};

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
    const unsigned short* __restrict__ A, const unsigned short* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<64, 64, 4, 4>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF16{});
}

extern "C" __global__ void craton_gemm_f16_lg(
    const unsigned short* __restrict__ A, const unsigned short* __restrict__ B,
    float* __restrict__ C, int M, int N, int K,
    int as0, int as1, int bs0, int bs1)
{
    gemm_body<128, 128, 8, 8>(A, B, C, M, N, K, as0, as1, bs0, bs1, LoadF16{});
}

extern "C" __global__ void craton_h2f(
    const unsigned short* __restrict__ src, float* __restrict__ dst, int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = craton_bits_h2f(src[i]);
    }
}

extern "C" __global__ void craton_f2h(
    const float* __restrict__ src, unsigned short* __restrict__ dst, int n)
{
    const int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        dst[i] = craton_bits_f2h(src[i]);
    }
}
