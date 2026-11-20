// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2026 Craton Software Company
//
// Self-contained OpenCL C translation of `gemm.cu`.

#define BK 16
#define VEC 4

static inline float craton_load_f32(float v) {
    return v;
}

static inline float craton_load_f16(ushort h) {
    uint sign = ((uint)(h & (ushort)0x8000u)) << 16;
    uint exp = (uint)((h >> 10) & (ushort)0x1Fu);
    uint mant = (uint)(h & (ushort)0x03FFu);
    uint out;
    if (exp == 0u) {
        if (mant == 0u) {
            out = sign;
        } else {
            uint e = 113u;
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
    return as_float(out);
}

static inline ushort craton_store_f16(float f) {
    uint bits = as_uint(f);
    uint sign = (bits >> 16) & 0x8000u;
    uint rest = bits & 0x7FFFFFFFu;
    if (rest >= 0x47800000u) {
        if (rest > 0x7F800000u) {
            return (ushort)(sign | 0x7E00u | ((rest >> 13) & 0x03FFu));
        }
        return (ushort)(sign | 0x7C00u);
    }
    if (rest >= 0x38800000u) {
        uint mant_odd = (rest >> 13) & 1u;
        uint rounded = (rest - 0x38000000u) + 0x0FFFu + mant_odd;
        uint h = rounded >> 13;
        if (h >= 0x7C00u) {
            h = 0x7C00u;
        }
        return (ushort)(sign | h);
    }
    if (rest < 0x33000000u) {
        return (ushort)sign;
    }
    int exp = (int)(rest >> 23);
    uint mant = (rest & 0x007FFFFFu) | 0x00800000u;
    uint shift = (uint)(126 - exp);
    uint half = mant >> shift;
    uint rem = mant & ((1u << shift) - 1u);
    uint halfway = 1u << (shift - 1u);
    if (rem > halfway || (rem == halfway && (half & 1u) != 0u)) {
        half++;
    }
    return (ushort)(sign | half);
}

#define DEFINE_CRATON_GEMM(NAME, BM, BN, TM, TN, T, LOAD_FN)                     \
__kernel void NAME(                                                              \
    __global const T* restrict A,                                                \
    __global const T* restrict B,                                                \
    __global float* restrict C,                                                  \
    int M, int N, int K,                                                         \
    int as0, int as1, int bs0, int bs1)                                          \
{                                                                                \
    const int THREADS_X = (BN) / (TN);                                           \
    const int THREADS_Y = (BM) / (TM);                                           \
    const int THREADS = THREADS_X * THREADS_Y;                                   \
    const int A_GROUPS = (TM) / VEC;                                             \
    const int B_GROUPS = (TN) / VEC;                                             \
                                                                                 \
    __local float As[BK][BM];                                                    \
    __local float Bs[BK][BN];                                                    \
                                                                                 \
    const int tx = (int)get_local_id(0);                                         \
    const int ty = (int)get_local_id(1);                                         \
    const int tid = ty * THREADS_X + tx;                                         \
                                                                                 \
    const int row0 = (int)get_group_id(1) * (BM);                                \
    const int col0 = (int)get_group_id(0) * (BN);                                \
                                                                                 \
    float acc[TM][TN];                                                           \
    for (int i = 0; i < (TM); ++i) {                                             \
        for (int j = 0; j < (TN); ++j) {                                         \
            acc[i][j] = 0.0f;                                                    \
        }                                                                        \
    }                                                                            \
                                                                                 \
    const int tiles = (K + BK - 1) / BK;                                         \
    for (int t = 0; t < tiles; ++t) {                                            \
        const int kBase = t * BK;                                                \
                                                                                 \
        for (int f = tid; f < (BM) * BK; f += THREADS) {                         \
            const int m = f / BK;                                                \
            const int kk = f % BK;                                               \
            const int gRow = row0 + m;                                           \
            const int gCol = kBase + kk;                                         \
            As[kk][m] = (gRow < M && gCol < K)                                   \
                    ? LOAD_FN(A[gRow * as0 + gCol * as1])                        \
                    : 0.0f;                                                      \
        }                                                                        \
        for (int f = tid; f < BK * (BN); f += THREADS) {                         \
            const int kk = f / (BN);                                             \
            const int n = f % (BN);                                              \
            const int gRow = kBase + kk;                                         \
            const int gCol = col0 + n;                                           \
            Bs[kk][n] = (gRow < K && gCol < N)                                   \
                    ? LOAD_FN(B[gRow * bs0 + gCol * bs1])                        \
                    : 0.0f;                                                      \
        }                                                                        \
                                                                                 \
        barrier(CLK_LOCAL_MEM_FENCE);                                            \
                                                                                 \
        for (int kk = 0; kk < BK; ++kk) {                                        \
            float aReg[TM];                                                      \
            float bReg[TN];                                                      \
            for (int g = 0; g < A_GROUPS; ++g) {                                 \
                const float4 v = vload4(0, &As[kk][ty * VEC + g * ((BM) / A_GROUPS)]); \
                aReg[g * VEC + 0] = v.x;                                         \
                aReg[g * VEC + 1] = v.y;                                         \
                aReg[g * VEC + 2] = v.z;                                         \
                aReg[g * VEC + 3] = v.w;                                         \
            }                                                                    \
            for (int g = 0; g < B_GROUPS; ++g) {                                 \
                const float4 v = vload4(0, &Bs[kk][tx * VEC + g * ((BN) / B_GROUPS)]); \
                bReg[g * VEC + 0] = v.x;                                         \
                bReg[g * VEC + 1] = v.y;                                         \
                bReg[g * VEC + 2] = v.z;                                         \
                bReg[g * VEC + 3] = v.w;                                         \
            }                                                                    \
            for (int i = 0; i < (TM); ++i) {                                     \
                for (int j = 0; j < (TN); ++j) {                                 \
                    acc[i][j] = fma(aReg[i], bReg[j], acc[i][j]);                \
                }                                                                \
            }                                                                    \
        }                                                                        \
                                                                                 \
        barrier(CLK_LOCAL_MEM_FENCE);                                            \
    }                                                                            \
                                                                                 \
    for (int i = 0; i < (TM); ++i) {                                             \
        const int localRow = ty * VEC + (i / VEC) * ((BM) / A_GROUPS) + (i % VEC); \
        const int gRow = row0 + localRow;                                        \
        if (gRow >= M) {                                                         \
            continue;                                                            \
        }                                                                        \
        for (int j = 0; j < (TN); ++j) {                                         \
            const int localCol = tx * VEC + (j / VEC) * ((BN) / B_GROUPS) + (j % VEC); \
            const int gCol = col0 + localCol;                                    \
            if (gCol < N) {                                                      \
                C[gRow * N + gCol] = acc[i][j];                                  \
            }                                                                    \
        }                                                                        \
    }                                                                            \
}

DEFINE_CRATON_GEMM(craton_gemm_f32,    64,  64, 4, 4, float,  craton_load_f32)
DEFINE_CRATON_GEMM(craton_gemm_f32_lg, 128, 128, 8, 8, float,  craton_load_f32)
DEFINE_CRATON_GEMM(craton_gemm_f16,    64,  64, 4, 4, ushort, craton_load_f16)
DEFINE_CRATON_GEMM(craton_gemm_f16_lg, 128, 128, 8, 8, ushort, craton_load_f16)

__kernel void craton_h2f(
    __global const ushort* restrict src, __global float* restrict dst, int n)
{
    const int i = (int)get_global_id(0);
    if (i < n) {
        dst[i] = craton_load_f16(src[i]);
    }
}

__kernel void craton_f2h(
    __global const float* restrict src, __global ushort* restrict dst, int n)
{
    const int i = (int)get_global_id(0);
    if (i < n) {
        dst[i] = craton_store_f16(src[i]);
    }
}
