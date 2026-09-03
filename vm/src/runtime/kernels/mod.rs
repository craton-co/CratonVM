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
pub fn load(ctx: &DeviceContext) -> cuda_bridge::Result<DeviceModule> {
    DeviceModule::from_ptx(ctx, GEMM_PTX, &ENTRY_POINTS)
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

    /// The `.target` was chosen for breadth. The `.version` has to be
    /// checked with the same care, because nvcc picks it for you.
    ///
    /// AUDIT 2026-09-02: the committed artifact carried `.version 9.3` —
    /// whatever toolkit last regenerated it (CUDA 13.3), not what the
    /// code needs. A driver older than the CUDA 13 line cannot parse ISA
    /// 9.3, so the module failed to load and every matmul fell back to
    /// the CPU on a host whose GPU would have run the sm_75 body
    /// perfectly. The `.target` bought Turing-through-Blackwell breadth
    /// and the `.version` silently spent it.
    ///
    /// Asserted as a literal, not as "some version exists", so a
    /// regeneration on a newer toolkit fails this test instead of
    /// quietly raising the driver floor. See `gemm.cu`'s header for the
    /// one-line rewrite that belongs beside the nvcc invocation, and for
    /// why 6.3 is honest rather than merely low.
    #[test]
    fn ptx_targets_a_supported_architecture() {
        assert!(
            GEMM_PTX.contains(".target sm_75"),
            "gemm.ptx should target sm_75 (the CUDA 13 floor); found none"
        );
        assert!(
            GEMM_PTX.contains(".version 6.3"),
            "gemm.ptx must declare `.version 6.3`, the floor for sm_75.              nvcc stamps the toolkit's own version instead — rerun the              rewrite from gemm.cu's header. Found: {:?}",
            GEMM_PTX
                .lines()
                .find(|l| l.trim_start().starts_with(".version"))
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

        // The tile shapes are template arguments now, not #defines, so the
        // check is that the instantiations the host assumes are present.
        for inst in ["gemm_body<64, 64, 4, 4>", "gemm_body<128, 128, 8, 8>"] {
            assert!(
                cu.contains(inst),
                "gemm.cu must instantiate {inst}; the host sizes its grid                  by exactly these tiles"
            );
        }
        assert_eq!(define("BK"), 16, "K-depth disagrees");
        assert_eq!(define("VEC"), 4, "shared-load vector width disagrees");
    }

    /// Both tile shapes must yield the same 16x16 thread block.
    ///
    /// They do because the tile and the per-thread block grow together —
    /// 64/4 and 128/8 are both 16 — which is what lets one `BLOCK_X`
    /// serve both kernels. If a future shape broke that, the launch would
    /// use the wrong block dimension for one of them.
    #[test]
    fn both_tiles_yield_the_same_thread_block() {
        assert_eq!(BLOCK_X, BN_SMALL / 4);
        assert_eq!(BLOCK_Y, BM_SMALL / 4);
        assert_eq!(BLOCK_X, BN_LARGE / 8);
        assert_eq!(BLOCK_Y, BM_LARGE / 8);
        assert_eq!(BLOCK_X * BLOCK_Y, 256);
        assert_eq!((BLOCK_X * BLOCK_Y) % 32, 0, "block must be whole warps");
    }

    /// The cooperative loads assume each tile divides evenly by the thread
    /// count, so every thread does the same number of iterations.
    #[test]
    fn cooperative_loads_divide_evenly() {
        let threads = BLOCK_X * BLOCK_Y;
        for (bm, bn) in [(BM_SMALL, BN_SMALL), (BM_LARGE, BN_LARGE)] {
            assert_eq!((bm * 16) % threads, 0, "A tile {bm} must divide by {threads}");
            assert_eq!((16 * bn) % threads, 0, "B tile {bn} must divide by {threads}");
        }
    }

    /// Shared memory must fit, for both shapes: `BK*(BM+BN)` floats
    /// against the 48 KB per-block default.
    ///
    /// Worth asserting rather than commenting because exceeding it does
    /// not fail at build time — the PTX is fine and the *launch* fails on
    /// whatever device is present, which is the worst place to find out.
    #[test]
    fn shared_memory_fits_a_block() {
        let small = 16 * (BM_SMALL + BN_SMALL) * 4;
        let large = 16 * (BM_LARGE + BN_LARGE) * 4;
        assert_eq!(small, 8 * 1024);
        assert_eq!(large, 16 * 1024);
        for bytes in [small, large] {
            assert!(
                bytes <= 48 * 1024,
                "shared tiles are {bytes} bytes, past the 48 KB default"
            );
        }
    }

    /// The tile choice must be the same on both sides of the launch.
    ///
    /// `gemm_launch_config` sizes the grid from one tile and
    /// `GemmKind::entry_name` picks the kernel that indexes for one tile.
    /// If they ever disagreed, the grid would cover the output for one
    /// shape while the kernel strode by the other — every block writing
    /// the wrong elements, with no error anywhere.
    #[test]
    fn grid_and_entry_point_agree_on_the_tile() {
        for (m, n) in [(64, 64), (512, 512), (1024, 1024), (2048, 2048),
                       (4096, 64), (64, 4096), (1023, 1024), (1024, 1023)] {
            let large = use_large_tile(m, n);
            let tile = if large { BN_LARGE } else { BN_SMALL };
            let cfg = gemm_launch_config(m, n);
            let expected_gx = (n as u32).div_ceil(tile).max(1);
            assert_eq!(
                cfg.grid.0, expected_gx,
                "{m}x{n}: grid sized for the other tile than entry_name picks"
            );
            let name = GemmKind::F32.entry_name(large);
            assert_eq!(name.ends_with("_lg"), large, "{m}x{n}: wrong entry point");
        }
    }

    /// The large tile needs BOTH edges to be large.
    #[test]
    fn a_thin_output_stays_on_the_small_tile() {
        // Plenty of rows, half a tile of columns: the large shape would
        // waste most of every block it launched.
        assert!(!use_large_tile(4096, 64));
        assert!(!use_large_tile(64, 4096));
        assert!(use_large_tile(1024, 1024));
        // Exactly at the threshold counts as large.
        assert!(use_large_tile(LARGE_TILE_MIN_EDGE, LARGE_TILE_MIN_EDGE));
        assert!(!use_large_tile(LARGE_TILE_MIN_EDGE - 1, LARGE_TILE_MIN_EDGE));
    }

    #[test]
    fn grid_covers_every_output_element() {
        // Exactly one small tile.
        let cfg = gemm_launch_config(64, 64);
        assert_eq!(cfg.grid, (1, 1, 1));
        assert_eq!(cfg.block, (BLOCK_X, BLOCK_Y, 1));

        // One past a small-tile boundary in each direction rounds up.
        let cfg = gemm_launch_config(65, 129);
        assert_eq!(cfg.grid, (3, 2, 1), "grid is (ceil(N/BN), ceil(M/BM), 1)");

        // Above the threshold the large tile is used, so the same output
        // needs a quarter as many blocks per axis.
        let cfg = gemm_launch_config(2048, 2048);
        assert_eq!(cfg.grid, (16, 16, 1), "2048/128 = 16");

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
        // 512 is below the large-tile threshold, so this is 512/64.
        let cfg = gemm_launch_config(512, 512);
        assert_eq!(cfg.grid, (8, 8, 1), "512/64 = 8, not 512/BLOCK_X = 32");
    }

    #[test]
    fn grid_axes_are_not_transposed() {
        // Deliberately non-square: transposing the axes is invisible on a
        // square shape and wrong everywhere else.
        let cfg = gemm_launch_config(/* m */ 512, /* n */ 64);
        assert_eq!(
            cfg.grid,
            (1, 8, 1),
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
