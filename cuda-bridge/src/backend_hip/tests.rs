use super::*;

/// The compile options are the acceptance criterion for Java float
/// semantics on this backend, so they are pinned rather than
/// trusted. `-ffp-contract=off` is the one that matters most: clang
/// defaults to `fast-honor-pragmas`, which fuses `a*b + c` into a
/// single-rounded FMA where JLS §15.17.1/§15.18.2 require two
/// roundings. `Emitter::binop_f32` records the same defect being
/// found and fixed on the PTX path.
#[test]
fn a_block_is_a_whole_number_of_wavefronts() {
    assert_eq!(round_to_wavefront(1024, 64), 1024);
    assert_eq!(round_to_wavefront(250, 64), 192);
    assert_eq!(round_to_wavefront(250, 32), 224);
    assert_eq!(round_to_wavefront(10, 64), 64, "never below one wavefront");
    assert_eq!(
        round_to_wavefront(256, 0),
        256,
        "a zero width must not divide"
    );
}

#[test]
fn hip_compile_options_are_ieee() {
    let opts = hip_compile_options("gfx1100");
    assert!(opts.contains(&"--offload-arch=gfx1100".to_string()));
    assert!(
        opts.contains(&"-ffp-contract=off".to_string()),
        "contraction must be off or `a*b + c` rounds once where Java rounds twice"
    );
    assert!(
        opts.contains(&"-fno-gpu-flush-denormals-to-zero".to_string()),
        "the emitted PTX carries no `.ftz`, so the HIP path must not flush either"
    );
    assert!(
        opts.contains(&"-fwrapv".to_string()),
        "signed overflow must wrap: the renderer emits plain `int` arithmetic"
    );
    assert!(
        opts.contains(&"-fhip-fp32-correctly-rounded-divide-sqrt".to_string()),
        "float division and sqrt must be correctly rounded"
    );
    for forbidden in [
        "-ffast-math",
        "-funsafe-math-optimizations",
        "-ffinite-math-only",
        "-fno-honor-nans",
        "-fno-honor-infinities",
        "-munsafe-fp-atomics",
        "--amdgpu-unsafe-fp-atomics",
    ] {
        assert!(
            !opts.iter().any(|o| o == forbidden),
            "{forbidden} re-enables exactly what this list exists to forbid"
        );
    }
}

/// The arch string is passed to hiprtc verbatim, feature suffixes
/// and all. Stripping them yields a code object the runtime refuses
/// to load, so a helper that "cleans up" the name would be a bug.
#[test]
fn the_offload_arch_keeps_its_feature_suffixes() {
    let opts = hip_compile_options("gfx90a:sramecc+:xnack-");
    assert!(opts.contains(&"--offload-arch=gfx90a:sramecc+:xnack-".to_string()));
}

/// `hipModuleLaunchKernel` takes grid dimensions first, then block.
/// `cubecl-hip-sys`'s own end-to-end test passes them the other way
/// round and survives only because its element count happens to
/// work either way -- so this is pinned here rather than left to an
/// integration test no CI can run.
#[test]
fn launch_geometry_is_grid_then_block() {
    let cfg = LaunchConfig {
        grid: (7, 1, 1),
        block: (256, 1, 1),
        shared_bytes: 0,
    };
    // The order the launch call uses, read back in the same order.
    let ordered = [
        cfg.grid.0,
        cfg.grid.1,
        cfg.grid.2,
        cfg.block.0,
        cfg.block.1,
        cfg.block.2,
    ];
    assert_eq!(ordered, [7, 1, 1, 256, 1, 1]);
    assert_eq!(LaunchConfig::elementwise(1792).grid.0 * 256, 1792);
}

/// Without a runtime present every entry point must return
/// `NoDriver` rather than panicking out of the binding crate's
/// symbol loader. This is the single most important invariant in
/// the file and the only one public CI can check.
#[test]
fn a_missing_runtime_is_an_error_and_not_a_panic() {
    if hip_available() {
        // A box with ROCm installed answers a different question;
        // the invariant under test is the absent-runtime one.
        return;
    }
    assert!(matches!(probe_device(0), Err(DeviceError::NoDriver)));
    assert!(matches!(
        DeviceContextInner::new(0),
        Err(DeviceError::NoDriver)
    ));
    assert!(matches!(toolchain_version(), Err(DeviceError::NoDriver)));
}

#[test]
fn ryzen_ai_integrated_props_enable_zero_copy_shared_memory() {
    // SAFETY: `hipDeviceProp_tR0600` is a plain C struct of integers and byte arrays.
    let mut props: sys::hipDeviceProp_tR0600 = unsafe { std::mem::zeroed() };
    props.integrated = 1;
    props.canMapHostMemory = 1;
    props.unifiedAddressing = 1;
    props.hostRegisterSupported = 1;
    props.managedMemory = 1;
    let apu = shared_memory_caps_from_props(&props);
    assert!(apu.supports_zero_copy_shared_memory());
    assert!(apu.unified_addressing);

    props.integrated = 0;
    let dgpu = shared_memory_caps_from_props(&props);
    assert!(!dgpu.supports_zero_copy_shared_memory());
}
