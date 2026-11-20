// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2026 Craton Software Company

//! Built-in kernels that ship with the VM rather than being lowered from
//! Java bytecode.
//!
//! # Why a second kernel source at all
//!
//! `jit-cuda` turns a `@GpuKernel` method into PTX when its body is a
//! single counted loop, or a two-level rectangular nest it can flatten.
//! That covers element-wise work and it does not cover a matrix multiply:
//! three nested loops, two parallel and one a reduction, wanting a
//! shared-memory tile, a `__syncthreads()` barrier and a 2-D block. The
//! analyzer rejects such a method rather than mis-lowering it, so the
//! kernels an inference workload actually spends its time in cannot come
//! from the bytecode path.
//!
//! They come from [`gemm.cu`](../../../../vm/src/runtime/kernels/gemm.cu)
//! instead, compiled to PTX ahead of time and embedded here. A cratonvm
//! build therefore needs no CUDA toolkit; only regenerating the PTX does.
//!
//! # Scope
//!
//! Deliberately not a general "load arbitrary PTX" entry point. That
//! would mean inventing a marshalling protocol for untyped argument
//! lists, and handing Java the ability to run arbitrary device code
//! through a `String`. These are a fixed, named, reviewable set with
//! typed signatures — the same shape a BLAS is.

use cuda_bridge::{DeviceContext, DeviceModule, KernelArgs, LaunchConfig};

/// The compiled kernels, generated from `gemm.cu`.
///
/// Committed rather than built, so `cargo build` does not require nvcc.
/// See `gemm.cu`'s header for the regeneration command; the test at the
/// bottom of this file fails if the two drift apart in the ways it can
/// cheaply detect.
const GEMM_PTX: &str = include_str!("gemm.ptx");
const GEMM_HIP_SRC: &str = include_str!("gemm.hip");
const GEMM_OPENCL_SRC: &str = include_str!("gemm.cl");

/// Rows and columns of `C` one thread block computes, small tile.
/// Must match the `gemm_body<64, 64, ...>` instantiations in `gemm.cu`.
pub const BM_SMALL: u32 = 64;
/// See [`BM_SMALL`].
pub const BN_SMALL: u32 = 64;

/// The same for the large tile: `gemm_body<128, 128, ...>`.
pub const BM_LARGE: u32 = 128;
/// See [`BM_LARGE`].
pub const BN_LARGE: u32 = 128;

/// Threads per block, both shapes: `(BN/TN, BM/TM)` is 16 x 16 either
/// way, because the tile and the per-thread block grow together.
pub const BLOCK_X: u32 = 16;
/// See [`BLOCK_X`].
pub const BLOCK_Y: u32 = 16;

/// Output edge at or above which the large tile is used.
///
/// Chosen from measurement rather than theory. Kernel-only throughput on
/// an RTX 2060, GFLOP/s:
///
/// | edge | 64x64 tile | 128x128 tile |
/// |------|-----------:|-------------:|
/// | 256  |        437 |          261 |
/// | 512  |       1470 |         1296 |
/// | 1024 |       1973 |         3112 |
///
/// The large tile does more arithmetic per shared-memory load and is
/// therefore faster once there is enough work to fill the device — but a
/// 128x128 tile covers a 256x256 problem in four blocks, which leaves
/// most of a 30-SM GPU idle. 1024 is where the crossover sits here; 512
/// still favours the small tile.
///
/// Deliberately a single threshold on the smaller output edge rather
/// than an occupancy model. The real quantity is "are there enough
/// blocks to fill this device", which depends on the SM count and so
/// cannot be a constant — but a constant that is right on the hardware
/// this was measured on beats a model that is wrong everywhere.
pub const LARGE_TILE_MIN_EDGE: i32 = 1024;

/// Entry points the module is loaded with.
const ENTRY_POINTS: [&str; 6] = [
    "craton_gemm_f32",
    "craton_gemm_f32_lg",
    "craton_gemm_f16",
    "craton_gemm_f16_lg",
    "craton_h2f",
    "craton_f2h",
];

/// Which GEMM to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GemmKind {
    /// `f32 x f32 -> f32`.
    F32,
    /// `f16 x f16 -> f32`, accumulating in `f32`.
    ///
    /// The accumulator is not negotiable: summing K terms in half
    /// precision loses accuracy fast enough to be visible in generated
    /// tokens at the K a transformer uses.
    F16,
}

impl GemmKind {
    /// The PTX entry point for this kind at the tile `use_large` selects.
    ///
    /// Four kernels rather than two: the same templated body instantiated
    /// at both tile sizes for both element types. Which one to launch is
    /// [`use_large_tile`]'s decision.
    pub fn entry_name(self, use_large: bool) -> &'static str {
        match (self, use_large) {
            (GemmKind::F32, false) => "craton_gemm_f32",
            (GemmKind::F32, true) => "craton_gemm_f32_lg",
            (GemmKind::F16, false) => "craton_gemm_f16",
            (GemmKind::F16, true) => "craton_gemm_f16_lg",
        }
    }
}

/// Whether an `m x n` output is big enough for the large tile.
///
/// Both dimensions must clear the threshold. A 4096x64 output has plenty
/// of rows and only half a tile of columns, so the large shape would
/// waste most of every block it launched.
#[must_use]
pub fn use_large_tile(m: i32, n: i32) -> bool {
    m >= LARGE_TILE_MIN_EDGE && n >= LARGE_TILE_MIN_EDGE
}

/// Load the built-in module against `ctx`.
///
/// Not cached here. `DeviceModule` is bound to the context that loaded
/// it, so the cache belongs with the context — see
/// `OffloadCache::builtin_module`, which holds one per device and hands
/// out a reference.
/// Load the built-in module for `ctx` targeting an optional explicit HIP `offload_arch`.
pub fn load_for_target(
    ctx: &DeviceContext,
    hip_offload_arch: Option<&str>,
) -> cuda_bridge::Result<DeviceModule> {
    match cuda_bridge::backend_kind() {
        cuda_bridge::BackendKind::Hip => {
            let owned;
            let offload_arch = match hip_offload_arch {
                Some(arch) => arch,
                None => {
                    let caps = cuda_bridge::probe_device(0)?;
                    match caps.arch {
                        cuda_bridge::DeviceArch::Hip(h) => {
                            owned = h.gcn_arch_name;
                            owned.as_str()
                        }
                        _ => return Err(cuda_bridge::DeviceError::NoDriver),
                    }
                }
            };
            DeviceModule::from_kernel_image(
                ctx,
                &cuda_bridge::KernelImage::HipSource {
                    source: GEMM_HIP_SRC,
                    offload_arch,
                },
                &ENTRY_POINTS,
            )
        }
        cuda_bridge::BackendKind::OpenCl => DeviceModule::from_kernel_image(
            ctx,
            &cuda_bridge::KernelImage::OpenClSource(GEMM_OPENCL_SRC),
            &ENTRY_POINTS,
        ),
        _ => DeviceModule::from_ptx(ctx, GEMM_PTX, &ENTRY_POINTS),
    }
}

pub fn load(ctx: &DeviceContext) -> cuda_bridge::Result<DeviceModule> {
    load_for_target(ctx, None)
}

/// Launch configuration for a GEMM producing an `m x n` result.
///
/// Grid sized by the BLOCK TILE, block sized by the THREAD COUNT.
///
/// Each block computes a tile of `C` using `BLOCK_Y x BLOCK_X` threads,
/// so the grid is the output rounded up to the TILE and the block is the
/// THREAD COUNT — two different numbers since register blocking landed,
/// and now the tile itself depends on the shape. Dividing the grid by
/// the thread count instead would launch far too many blocks, all
/// writing over each other.
///
/// Note the axis assignment: `x` indexes columns and `y` rows, matching
/// the kernel's `row0 = blockIdx.y * BM`. Transposing this produces a
/// correct-looking result on a square matrix and garbage on any other.
pub fn gemm_launch_config(m: i32, n: i32) -> LaunchConfig {
    let large = use_large_tile(m, n);
    let (bm, bn) = if large {
        (BM_LARGE, BN_LARGE)
    } else {
        (BM_SMALL, BN_SMALL)
    };
    let gx = (n.max(0) as u32).div_ceil(bn).max(1);
    let gy = (m.max(0) as u32).div_ceil(bm).max(1);
    LaunchConfig {
        grid: (gx, gy, 1),
        block: (BLOCK_X, BLOCK_Y, 1),
        shared_bytes: 0,
    }
}

/// How a logical matrix is laid out in the buffer behind it.
///
/// Element `(i, j)` lives at `i * row * j * col` — that is, at
/// `i*row + j*col`. A row-major `R x C` matrix is `(C, 1)`; reading the
/// same bytes transposed is `(1, R)`.
///
/// Expressing transposition this way rather than as separate NN/NT/TN/TT
/// kernels keeps one code path with no inner-loop branches, at the cost
/// of one multiply per tile load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strides {
    /// Distance between consecutive rows.
    pub row: i32,
    /// Distance between consecutive columns.
    pub col: i32,
}

impl Strides {
    /// Strides for a row-major `rows x cols` matrix read as itself.
    #[must_use]
    pub fn row_major(cols: i32) -> Self {
        Strides { row: cols, col: 1 }
    }

    /// Strides for a row-major matrix read transposed.
    ///
    /// `stored_cols` is the column count of the matrix **as stored**, not
    /// of the logical (transposed) view — getting that backwards produces
    /// a correct-looking result on a square matrix and garbage otherwise.
    #[must_use]
    pub fn transposed(stored_cols: i32) -> Self {
        let _ = stored_cols;
        // For a stored `R x C` matrix, element (i, j) of its transpose is
        // stored element (j, i) at `j*C + i`, so the transpose's row
        // stride is 1 and its column stride is C.
        Strides {
            row: 1,
            col: stored_cols,
        }
    }

    /// Pick the layout for an operand.
    ///
    /// `stored_cols` is the operand's column count as stored in memory.
    #[must_use]
    pub fn of(transposed: bool, stored_cols: i32) -> Self {
        if transposed {
            Self::transposed(stored_cols)
        } else {
            Self::row_major(stored_cols)
        }
    }
}

/// Build the argument list for a GEMM launch.
///
/// The order matches `gemm.cu`: `A, B, C, M, N, K, as0, as1, bs0, bs1`.
#[allow(clippy::too_many_arguments)]
pub fn gemm_args<A, B>(
    a: &cuda_bridge::DeviceBuffer<A>,
    b: &cuda_bridge::DeviceBuffer<B>,
    c: &cuda_bridge::DeviceBuffer<f32>,
    m: i32,
    n: i32,
    k: i32,
    a_strides: Strides,
    b_strides: Strides,
) -> KernelArgs
where
    A: cuda_bridge::DeviceElem,
    B: cuda_bridge::DeviceElem,
{
    KernelArgs::new()
        .push_device_ptr(a)
        .push_device_ptr(b)
        .push_device_ptr(c)
        .push_i32(m)
        .push_i32(n)
        .push_i32(k)
        .push_i32(a_strides.row)
        .push_i32(a_strides.col)
        .push_i32(b_strides.row)
        .push_i32(b_strides.col)
}

/// Validate a GEMM's shape before anything touches the device.
///
/// Returns the required element counts for `A`, `B` and `C`.
///
/// Worth doing on the host: an out-of-range shape here becomes an
/// out-of-bounds global read on the device, which on current hardware is
/// either silent garbage or a context-killing fault several launches
/// later. A clear error now is strictly better than either.
pub fn gemm_shape(m: i32, n: i32, k: i32) -> Result<(usize, usize, usize), String> {
    if m <= 0 || n <= 0 || k <= 0 {
        return Err(format!(
            "gemm: M, N and K must all be positive (got M={m}, N={n}, K={k})"
        ));
    }
    // i32 multiplication is what the kernel indexes with, so overflow
    // here is exactly the overflow it would suffer.
    let a = (m as i64) * (k as i64);
    let b = (k as i64) * (n as i64);
    let c = (m as i64) * (n as i64);
    for (name, elems) in [("A (MxK)", a), ("B (KxN)", b), ("C (MxN)", c)] {
        if elems > i32::MAX as i64 {
            return Err(format!(
                "gemm: {name} needs {elems} elements, past the i32 indexing \
                 the kernel does (M={m}, N={n}, K={k})"
            ));
        }
    }
    Ok((a as usize, b as usize, c as usize))
}

/// Launch config for the bulk fp16 conversion kernels.
pub fn convert_launch_config(n: i32) -> LaunchConfig {
    LaunchConfig::elementwise_with_block(n.max(0) as u32, 256)
}

/// Entry point name for `f16 -> f32`.
pub const H2F_ENTRY: &str = "craton_h2f";
/// Entry point name for `f32 -> f16`.
pub const F2H_ENTRY: &str = "craton_f2h";

#[cfg(test)]
mod tests;
