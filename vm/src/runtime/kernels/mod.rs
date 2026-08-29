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

/// Tile edge, and therefore the block dimension. Must match `TILE` in
/// `gemm.cu` — the kernel indexes its shared tile by `threadIdx`, so a
/// launch with a different block shape reads the wrong elements rather
/// than failing.
pub const TILE: u32 = 16;

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
/// One thread per output element, `TILE x TILE` per block, so the grid is
/// the output shape rounded up. Note the axis assignment: `x` indexes
/// columns and `y` rows, matching the kernel's
/// `row = blockIdx.y * TILE + ty`. Transposing this is the kind of
/// mistake that produces a correct-looking result on a square matrix and
/// garbage on any other.
pub fn gemm_launch_config(m: i32, n: i32) -> LaunchConfig {
    let gx = (n.max(0) as u32).div_ceil(TILE).max(1);
    let gy = (m.max(0) as u32).div_ceil(TILE).max(1);
    LaunchConfig {
        grid: (gx, gy, 1),
        block: (TILE, TILE, 1),
        shared_bytes: 0,
    }
}

/// Build the argument list for a GEMM launch.
///
/// The order matches `gemm.cu`: `A, B, C, M, N, K`.
pub fn gemm_args<A, B>(
    a: &cuda_bridge::DeviceBuffer<A>,
    b: &cuda_bridge::DeviceBuffer<B>,
    c: &cuda_bridge::DeviceBuffer<f32>,
    m: i32,
    n: i32,
    k: i32,
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

    /// The block shape the host launches with must match the tile the
    /// kernel indexes by. A mismatch does not fail: it reads the wrong
    /// shared-memory elements and returns a plausible wrong answer.
    #[test]
    fn tile_matches_the_cuda_source() {
        let cu = include_str!("gemm.cu");
        let line = cu
            .lines()
            .find(|l| l.starts_with("#define TILE "))
            .expect("gemm.cu must #define TILE");
        let declared: u32 = line
            .trim_start_matches("#define TILE ")
            .trim()
            .parse()
            .expect("TILE must be an integer");
        assert_eq!(
            declared, TILE,
            "gemm.cu's TILE and this module's TILE disagree; the launch \
             block shape would not match the kernel's shared tile"
        );
    }

    #[test]
    fn grid_covers_every_output_element() {
        // Exactly one tile.
        let cfg = gemm_launch_config(16, 16);
        assert_eq!(cfg.grid, (1, 1, 1));
        assert_eq!(cfg.block, (TILE, TILE, 1));

        // One past a tile boundary in each direction rounds up.
        let cfg = gemm_launch_config(17, 33);
        assert_eq!(cfg.grid, (3, 2, 1), "grid is (ceil(N/16), ceil(M/16), 1)");

        // A degenerate shape still launches at least one block rather
        // than zero, which the driver rejects.
        let cfg = gemm_launch_config(1, 1);
        assert_eq!(cfg.grid, (1, 1, 1));
    }

    #[test]
    fn grid_axes_are_not_transposed() {
        // Deliberately non-square: transposing the axes is invisible on a
        // square shape and wrong everywhere else.
        let cfg = gemm_launch_config(/* m */ 64, /* n */ 16);
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
