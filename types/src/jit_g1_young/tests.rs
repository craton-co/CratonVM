use super::*;

/// The only test in this crate that touches [`JIT_G1_YOUNG`]: the reader
/// answers only a live, plausible table, and the run-time words sit at the
/// byte offsets the emitted sequence hard-codes (0, 24, 32).
#[test]
fn the_geometry_reader_refuses_unpublished_poisoned_and_implausible_tables() {
    let w = &JIT_G1_YOUNG.words;
    assert_eq!(std::mem::size_of::<JitG1YoungTable>(), 8 * 8);
    let base = jit_g1_young_addr();
    assert_eq!(&w[W_REGIONS] as *const AtomicUsize as usize - base, 0);
    assert_eq!(&w[W_SHIFT] as *const AtomicUsize as usize - base, 24);
    assert_eq!(&w[W_COUNT] as *const AtomicUsize as usize - base, 32);
    assert_eq!(jit_g1_young_geometry(), None, "unpublished");
    w[W_STRIDE].store(96, Ordering::SeqCst);
    w[W_TYPE_OFFSET].store(40, Ordering::SeqCst);
    w[W_YOUNG_MAX].store(1, Ordering::SeqCst);
    w[W_SHIFT].store(20, Ordering::SeqCst);
    assert_eq!(
        jit_g1_young_geometry(),
        Some(JitG1YoungGeometry {
            table: base,
            shift: 20,
            stride: 96,
            type_offset: 40,
            young_max: 1,
        })
    );
    assert_eq!(jit_g1_young_shift(), 20);
    w[W_TYPE_OFFSET].store(96, Ordering::SeqCst);
    assert_eq!(jit_g1_young_geometry(), None, "type offset outside the stride");
    w[W_TYPE_OFFSET].store(40, Ordering::SeqCst);
    w[W_STRIDE].store(0, Ordering::SeqCst);
    assert_eq!(jit_g1_young_geometry(), None, "zero stride");
    w[W_STRIDE].store(96, Ordering::SeqCst);
    w[W_SHIFT].store(64, Ordering::SeqCst);
    assert_eq!(jit_g1_young_geometry(), None, "over-wide shift");
    w[W_SHIFT].store(0, Ordering::SeqCst);
    assert_eq!(jit_g1_young_geometry(), None, "poisoned");
    for word in w.iter() {
        word.store(0, Ordering::SeqCst);
    }
}
