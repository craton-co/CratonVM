// SPDX-License-Identifier: Apache-2.0
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

/// Rows of `C` one thread block computes. Must match `BM` in `gemm.cu`.
pub const BM: u32 = 64;

/// Columns of `C` one thread block computes. Must match `BN` in `gemm.cu`.
pub const BN: u32 = 64;

/// Rows of `C` one *thread* computes. Must match `TM` in `gemm.cu`.
pub const TM: u32 = 4;

/// Columns of `C` one *thread* computes. Must match `TN` in `gemm.cu`.
pub const TN: u32 = 4;

/// Threads per block along x and y: `(BN/TN, BM/TM)`, i.e. 16 x 16 = 256.
///
/// Note that the block *dimension* and the block *tile* are no longer the
/// same number, which they were before register blocking. A block is 16x16
/// threads and computes a 64x64 patch of `C`, because each thread owns a
/// `TM x TN` sub-block. Sizing the grid by the thread count instead of the
/// tile — the obvious mistake, and the one the old code's shape made easy
/// — launches 16 times too many blocks and lets them write over each
/// other.
pub const BLOCK_X: u32 = BN / TN;
/// See [`BLOCK_X`].
pub const BLOCK_Y: u32 = BM / TM;

/// Entry points the module is loaded with.
const ENTRY_POINTS: [&str; 4] = [
    "craton_gemm_f32",
    "craton_gemm_f16",
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
    /// The PTX entry point this kind launches.
    pub fn entry_name(self) -> &'static str {
        match self {
            GemmKind::F32 => "craton_gemm_f32",
            GemmKind::F16 => "craton_gemm_f16",
        }
    }
}

/// Load the built-in module against `ctx`.
///
/// Not cached here. `DeviceModule` is bound to the context that loaded
/// it, so the cache belongs with the context — see
/// `OffloadCache::builtin_module`, which holds one per device and hands
/// out a reference.
pub fn load(ctx: &DeviceContext) -> cuda_bridge::Result<DeviceModule> {
    DeviceModule::from_ptx(ctx, GEMM_PTX, &ENTRY_POINTS)
}

/// Launch configuration for a GEMM producing an `m x n` result.
///
/// Grid sized by the BLOCK TILE, block sized by the THREAD COUNT.
///
/// Each block computes a `BM x BN` patch of `C` using `BLOCK_Y x BLOCK_X`
/// threads, so the grid is the output rounded up to the patch and the
/// block is the thread count — two different numbers since register
/// blocking landed. Dividing the grid by the thread count instead would
/// launch 16x too many blocks, all writing over each other.
///
/// Note the axis assignment: `x` indexes columns and `y` rows, matching
/// the kernel's `row0 = blockIdx.y * BM`. Transposing this produces a
/// correct-looking result on a square matrix and garbage on any other.
pub fn gemm_launch_config(m: i32, n: i32) -> LaunchConfig {
    let gx = (n.max(0) as u32).div_ceil(BN).max(1);
    let gy = (m.max(0) as u32).div_ceil(BM).max(1);
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
        Strides { row: 1, col: stored_cols }
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
mod tests {
    use super::*;

    /// The embedded PTX must declare every entry point we resolve.
    ///
    /// This is the cheap half of "the .ptx matches the .cu": if someone
    /// edits `gemm.cu` and forgets to regenerate, a renamed or added
    /// kernel is caught here rather than at `DeviceModule::from_ptx` on a
    /// machine with a GPU — which is to say, rather than never, on the
    /// machines where the suite normally runs.
    #[test]
    fn ptx_declares_every_entry_point() {
        for entry in ENTRY_POINTS {
            let decl = format!(".visible .entry {entry}(");
            assert!(
                GEMM_PTX.contains(&decl),
                "gemm.ptx does not declare `{entry}`. Regenerate it: \
                 nvcc -arch=compute_75 -ptx gemm.cu -o gemm.ptx"
            );
        }
    }

    /// The PTX must be a virtual-arch build, so the driver JITs it for
    /// whatever device is present instead of it being pinned to one.
    /// Both GEMM entry points must take the ten parameters the host
    /// pushes: three pointers, M/N/K, and two stride pairs.
    ///
    /// A mismatch here is not a compile error anywhere — the host pushes
    /// an argument list and the driver reads however many the PTX
    /// declares — so a stale gemm.ptx would silently feed the kernel
    /// garbage strides.
    #[test]
    fn ptx_entry_points_take_the_arguments_the_host_pushes() {
        for entry in ["craton_gemm_f32", "craton_gemm_f16"] {
            let start = GEMM_PTX
                .find(&format!(".visible .entry {entry}("))
                .unwrap_or_else(|| panic!("{entry} not declared"));
            let end = GEMM_PTX[start..]
                .find(')')
                .expect("entry point parameter list is unterminated")
                + start;
            let params = GEMM_PTX[start..end].matches(".param").count();
            assert_eq!(
                params, 10,
                "{entry} declares {params} parameters; the host pushes 10 \
                 (A, B, C, M, N, K, as0, as1, bs0, bs1). Regenerate gemm.ptx."
            );
        }
    }

    #[test]
    fn ptx_targets_a_supported_architecture() {
        assert!(
            GEMM_PTX.contains(".target sm_75"),
            "gemm.ptx should target sm_75 (the CUDA 13 floor); found none"
        );
        assert!(
            GEMM_PTX.contains(".version"),
            "gemm.ptx has no .version directive — is it truncated?"
        );
    }

    /// Every tiling constant must match the CUDA source.
    ///
    /// A mismatch does not fail loudly: the kernel indexes its shared tile
    /// and its accumulators by `threadIdx` against these numbers, so a
    /// launch built from different ones reads the wrong elements and
    /// returns a plausible wrong answer. There are four of them now rather
    /// than one, and they interact — `BLOCK_X` is `BN/TN` — so checking
    /// them individually is the only way to localise a drift.
    #[test]
    fn tiling_constants_match_the_cuda_source() {
        let cu = include_str!("gemm.cu");
        let define = |name: &str| -> u32 {
            let prefix = format!("#define {name} ");
            cu.lines()
                .find(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| panic!("gemm.cu must #define {name}"))
                .trim_start_matches(&prefix)
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        };

        assert_eq!(define("BM"), BM, "block tile rows disagree");
        assert_eq!(define("BN"), BN, "block tile columns disagree");
        assert_eq!(define("TM"), TM, "thread tile rows disagree");
        assert_eq!(define("TN"), TN, "thread tile columns disagree");
    }

    /// The derived thread counts must stay consistent with the tiles.
    #[test]
    fn block_dimensions_follow_from_the_tiles() {
        assert_eq!(BLOCK_X, BN / TN);
        assert_eq!(BLOCK_Y, BM / TM);
        // 256 threads: one warp per 32 of them, and a round number of
        // warps is what keeps the cooperative loads uniform.
        assert_eq!(BLOCK_X * BLOCK_Y, 256);
        assert_eq!((BLOCK_X * BLOCK_Y) % 32, 0, "block must be whole warps");
    }

    /// The cooperative loads assume the tile divides evenly by the thread
    /// count, so every thread does the same number of iterations.
    #[test]
    fn cooperative_loads_divide_evenly() {
        let threads = BLOCK_X * BLOCK_Y;
        assert_eq!((BM * 16) % threads, 0, "A tile must divide by the thread count");
        assert_eq!((16 * BN) % threads, 0, "B tile must divide by the thread count");
    }

    /// Shared memory must fit. `BK*(BM+BN)` floats, and 48 KB is the
    /// per-block default on every architecture this targets.
    #[test]
    fn shared_memory_fits_a_block() {
        let bytes = 16 * (BM + BN) * 4;
        assert!(
            bytes <= 48 * 1024,
            "shared tile is {bytes} bytes, past the 48 KB per-block default"
        );
    }

    /// A row-major operand reads at (cols, 1); its transpose at (1, cols).
    #[test]
    fn strides_describe_both_readings() {
        // A stored 3x4 (row-major): element (i,j) at i*4 + j.
        assert_eq!(Strides::row_major(4), Strides { row: 4, col: 1 });
        // Read transposed as 4x3: element (i,j) is stored (j,i) at j*4 + i,
        // so the row stride is 1 and the column stride is the STORED width.
        assert_eq!(Strides::transposed(4), Strides { row: 1, col: 4 });
    }

    /// The indices the two layouts produce, checked by hand.
    ///
    /// Stride arithmetic is easy to get subtly wrong and impossible to
    /// eyeball once it is inside a kernel, so the mapping is pinned here
    /// against a worked example rather than only end-to-end on a device.
    #[test]
    fn stride_arithmetic_matches_a_worked_example() {
        // Stored 2x3 row-major:  [ 0 1 2 ]
        //                        [ 3 4 5 ]
        let rm = Strides::row_major(3);
        let at = |st: Strides, i: i32, j: i32| i * st.row + j * st.col;

        assert_eq!(at(rm, 0, 0), 0);
        assert_eq!(at(rm, 0, 2), 2);
        assert_eq!(at(rm, 1, 0), 3);
        assert_eq!(at(rm, 1, 2), 5);

        // The same bytes read as a 3x2 transpose:  [ 0 3 ]
        //                                          [ 1 4 ]
        //                                          [ 2 5 ]
        let tr = Strides::transposed(3);
        assert_eq!(at(tr, 0, 0), 0);
        assert_eq!(at(tr, 0, 1), 3);
        assert_eq!(at(tr, 1, 0), 1);
        assert_eq!(at(tr, 2, 1), 5);
    }

    #[test]
    fn of_selects_between_the_two() {
        assert_eq!(Strides::of(false, 7), Strides::row_major(7));
        assert_eq!(Strides::of(true, 7), Strides::transposed(7));
    }

    #[test]
    fn grid_covers_every_output_element() {
        // Exactly one block tile.
        let cfg = gemm_launch_config(64, 64);
        assert_eq!(cfg.grid, (1, 1, 1));
        assert_eq!(cfg.block, (BLOCK_X, BLOCK_Y, 1));

        // One past a tile boundary in each direction rounds up.
        let cfg = gemm_launch_config(65, 129);
        assert_eq!(cfg.grid, (3, 2, 1), "grid is (ceil(N/BN), ceil(M/BM), 1)");

        // A degenerate shape still launches at least one block rather
        // than zero, which the driver rejects.
        let cfg = gemm_launch_config(1, 1);
        assert_eq!(cfg.grid, (1, 1, 1));
    }

    /// The grid divides by the block TILE, not by the thread count.
    ///
    /// These are 64 and 16 respectively, so confusing them launches four
    /// times the blocks in each dimension — sixteen times too many, every
    /// one writing over its neighbours' output.
    #[test]
    fn grid_is_sized_by_the_tile_not_the_thread_count() {
        let cfg = gemm_launch_config(256, 256);
        assert_eq!(cfg.grid, (4, 4, 1), "256/BN = 4, not 256/BLOCK_X = 16");
    }

    #[test]
    fn grid_axes_are_not_transposed() {
        // Deliberately non-square: transposing the axes is invisible on a
        // square shape and wrong everywhere else.
        let cfg = gemm_launch_config(/* m */ 256, /* n */ 64);
        assert_eq!(
            cfg.grid,
            (1, 4, 1),
            "x must index columns (N) and y rows (M), matching the kernel"
        );
    }

    #[test]
    fn shape_validation_rejects_non_positive_dimensions() {
        assert!(gemm_shape(0, 4, 4).is_err());
        assert!(gemm_shape(4, 0, 4).is_err());
        assert!(gemm_shape(4, 4, 0).is_err());
        assert!(gemm_shape(-1, 4, 4).is_err());
    }

    #[test]
    fn shape_validation_reports_element_counts() {
        assert_eq!(gemm_shape(2, 3, 5).unwrap(), (10, 15, 6));
    }

    #[test]
    fn shape_validation_rejects_what_i32_indexing_cannot_address() {
        // 65536 x 65536 is 2^32 elements: past what `row * K + col`
        // computes in i32 inside the kernel.
        let err = gemm_shape(65_536, 65_536, 65_536).unwrap_err();
        assert!(err.contains("i32 indexing"), "unexpected message: {err}");
    }
}
