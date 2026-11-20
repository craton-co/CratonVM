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
        "gemm.ptx must declare `.version 6.3`, the floor for sm_75. \
         nvcc stamps the toolkit's own version instead — rerun the \
         rewrite from gemm.cu's header. Found: {:?}",
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
            "gemm.cu must instantiate {inst}; the host sizes its grid \
             by exactly these tiles"
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
        assert_eq!(
            (bm * 16) % threads,
            0,
            "A tile {bm} must divide by {threads}"
        );
        assert_eq!(
            (16 * bn) % threads,
            0,
            "B tile {bn} must divide by {threads}"
        );
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
    for (m, n) in [
        (64, 64),
        (512, 512),
        (1024, 1024),
        (2048, 2048),
        (4096, 64),
        (64, 4096),
        (1023, 1024),
        (1024, 1023),
    ] {
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
    assert!(!use_large_tile(
        LARGE_TILE_MIN_EDGE - 1,
        LARGE_TILE_MIN_EDGE
    ));
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

#[test]
fn hip_and_opencl_builtin_sources_declare_every_entry_point() {
    for src in [GEMM_HIP_SRC, GEMM_OPENCL_SRC] {
        assert!(!src.lines().any(|l| l.trim_start().starts_with("#include")), "built-in kernel must be self-contained");
        assert!(src.contains("BM") && src.contains("BN") && src.contains("#define BK 16"));
        for entry in ENTRY_POINTS {
            assert!(
                src.contains(entry),
                "built-in kernel source missing entry point `{entry}`"
            );
        }
    }
}
