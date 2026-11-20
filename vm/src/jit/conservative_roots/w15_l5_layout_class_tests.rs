use super::{layout_is_java_local, layout_is_operand_spill};

fn layouts() -> Vec<cratonvm_jit::FrameLayout> {
    let base = cratonvm_jit::FrameLayout {
        java_locals_hi: 64,
        locals_hi: 96,
        spill_lo: 96,
        spill_hi: 192,
        callee_saved_lo: 192,
        callee_saved_hi: 232,
        reg_spill_lo: 232,
        reg_spill_hi: 296,
        ..Default::default()
    };
    vec![
        cratonvm_jit::FrameLayout::default(),
        base,
        // Typed ranges inside the java locals, the tail and the spill.
        cratonvm_jit::FrameLayout {
            ref_hoist_lo: 32,
            ref_hoist_hi: 48,
            arith_lo: 72,
            arith_hi: 88,
            scalar_lo: 112,
            scalar_hi: 128,
            ..base
        },
        // A spill range that starts inside the locals tail.
        cratonvm_jit::FrameLayout {
            spill_lo: 80,
            ..base
        },
        // No java locals, spill only.
        cratonvm_jit::FrameLayout {
            spill_lo: 8,
            spill_hi: 64,
            ..Default::default()
        },
    ]
}

#[test]
fn layout_class_predicates_match_region_name() {
    for layout in layouts() {
        for off in -16..=320 {
            let name = layout.region_name(off);
            assert_eq!(
                layout_is_java_local(&layout, off),
                name == "java-local",
                "java-local off={off} layout={layout:?}"
            );
            assert_eq!(
                layout_is_operand_spill(&layout, off),
                name == "operand-spill",
                "operand-spill off={off} layout={layout:?}"
            );
        }
    }
}

/// gcj j2/pn2: the bounded dead-spill excuse skips only operand-spill
/// words above the cursor; the unbounded one (the default) also skips a
/// typed range and offsets past `spill_hi` that lie above the cursor.
#[test]
fn gcj2pn2_the_bounded_dead_spill_excuse_stays_inside_the_spill_range() {
    use super::band_dead_spill_excuses;
    let layout = cratonvm_jit::FrameLayout {
        java_locals_hi: 32,
        spill_lo: 64,
        spill_hi: 128,
        scalar_lo: 96,
        scalar_hi: 112,
        ..Default::default()
    };
    let hi = 72;
    // An operand-spill word above the cursor: excused either way.
    assert!(band_dead_spill_excuses(&layout, 80, hi, false));
    assert!(band_dead_spill_excuses(&layout, 80, hi, true));
    // Below the cursor: never.
    assert!(!band_dead_spill_excuses(&layout, 64, hi, false));
    assert!(!band_dead_spill_excuses(&layout, 64, hi, true));
    // A scalar-replaced field inside the spill span: only unbounded.
    assert!(band_dead_spill_excuses(&layout, 104, hi, false));
    assert!(!band_dead_spill_excuses(&layout, 104, hi, true));
    // Past `spill_hi`: only unbounded.
    assert!(band_dead_spill_excuses(&layout, 136, hi, false));
    assert!(!band_dead_spill_excuses(&layout, 136, hi, true));
    // No spill range: never.
    let empty = cratonvm_jit::FrameLayout::default();
    assert!(!band_dead_spill_excuses(&empty, 136, 0, false));
}
