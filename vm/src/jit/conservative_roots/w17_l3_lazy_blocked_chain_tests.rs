use super::*;

fn compiled_method_with_frame(frame_size: i32) -> cratonvm_jit::CompiledMethod {
    let mut cm = cratonvm_jit::CompiledMethod::new(
        cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests"),
    );
    cm.osr_frame_size = frame_size;
    cm
}

fn info_for(
    cm: &cratonvm_jit::CompiledMethod,
    exact_rbp: usize,
    frame_base: usize,
) -> PreciseFrameInfo {
    PreciseFrameInfo {
        compiled_method: cm as *const cratonvm_jit::CompiledMethod,
        frame_base,
        entry_ptr: cm.entry_ptr(),
        exact_rbp,
        exact_cm_id: 0,
        not_entrant_ret_pc: 0,
    }
}

/// One compiled frame whose caller is not compiled code (its saved return
/// address resolves to no code range): the band walk covers it with its
/// layout alone, so the deposit may defer it.
#[test]
fn a_single_frame_with_a_usable_layout_is_admitted() {
    let band = [0usize; 64];
    let lo = band.as_ptr() as usize;
    let rbp = lo + 32 * 8;
    let frame_base = lo + 64 * 8;
    let cm = compiled_method_with_frame(16 * 8);
    assert!(precise_band_walk_is_complete(
        info_for(&cm, rbp, frame_base),
        lo
    ));
    std::hint::black_box(&band);
}

/// Every case `scan_one_frame_precise` / `scan_precise_frames_and_bands`
/// would answer with a layout-free sweep of `[scanner_sp, ..)` -- memory
/// the blocked thread keeps running in -- is refused.
#[test]
fn a_walk_that_would_sweep_below_the_frames_is_refused() {
    let band = [0usize; 64];
    let lo = band.as_ptr() as usize;
    let rbp = lo + 32 * 8;
    let frame_base = lo + 64 * 8;
    let usable = compiled_method_with_frame(16 * 8);
    // No exact RBP published.
    assert!(!precise_band_walk_is_complete(
        info_for(&usable, 0, frame_base),
        lo
    ));
    // A misaligned one.
    assert!(!precise_band_walk_is_complete(
        info_for(&usable, rbp + 4, frame_base),
        lo
    ));
    // Below the scanner, or at / above the entry.
    assert!(!precise_band_walk_is_complete(
        info_for(&usable, rbp, frame_base),
        rbp + 8
    ));
    assert!(!precise_band_walk_is_complete(
        info_for(&usable, frame_base, frame_base),
        lo
    ));
    // No recorded frame size, or an implausible one.
    let sizeless = compiled_method_with_frame(0);
    assert!(!precise_band_walk_is_complete(
        info_for(&sizeless, rbp, frame_base),
        lo
    ));
    let huge = compiled_method_with_frame(2 * 1024 * 1024);
    assert!(!precise_band_walk_is_complete(
        info_for(&huge, rbp, frame_base),
        lo
    ));
    std::hint::black_box(&band);
}

/// The layout test is `scan_one_compiled_frame_with_layout`'s.
#[test]
fn the_layout_test_matches_the_band_scan() {
    let at = |size: i32, rbp: usize| {
        compiled_frame_layout_scannable(rbp, &compiled_method_with_frame(size))
    };
    assert!(!at(0, 4096));
    assert!(!at(-8, 4096));
    assert!(at(4096, 4096));
    assert!(!at(4104, 4096));
    assert!(at(1024 * 1024, 2 * 1024 * 1024));
    assert!(!at(1024 * 1024 + 8, 4 * 1024 * 1024));
}

/// A thread with no chain defers nothing (the deposit then scans, which
/// with no chain is nothing either).
#[test]
fn an_empty_chain_defers_nothing() {
    std::thread::spawn(|| {
        assert!(defer_chain_if_frozen(current_stack_pointer()).is_none());
    })
    .join()
    .expect("the probe thread finishes");
}
