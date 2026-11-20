use super::*;

// NOTE: nothing in this module may call `enable` / `disable_for_test`.
//
// Those publish the PROCESS-GLOBAL geometry, and a `cargo test` binary runs
// its tests on parallel threads — so a test that flips the config here is
// retyping every reference slot in every OTHER test in this binary, including
// `heap_types::tests::element_byte_size_reference`, which asserts the wide
// (8-byte) element size. The tests that genuinely need a published geometry
// live alone in `types/tests/narrow_oop_global_config.rs`; see the warning on
// [`disable_for_test`] for what this cost in `cratonvm-gc`.
//
// Everything below is the pure arithmetic, exercised through `encode_with` /
// `decode_with`.

/// zt t20/zb3: the colored-slot decode is an identity on every plain word,
/// and on every word when no base is published; a well-formed colored word
/// becomes `base + offset`; bit-63 garbage with a reserved bit set is
/// returned verbatim. Pure arithmetic: nothing is published.
#[test]
fn zt20zb3_the_colored_slot_decode_is_an_identity_on_plain_words() {
    let base = 0x7f12_3400_0000u64;
    let remapped = 1u64 << 44;
    for plain in [
        0u64,
        8,
        0x7f12_3400_0040,
        0x0000_7fff_ffff_fff8,
        i64::MAX as u64,
    ] {
        assert_eq!(
            decode_colored_slot_word_with(base, plain),
            plain,
            "{plain:#x}"
        );
        assert_eq!(decode_colored_slot_word_with(0, plain), plain, "{plain:#x}");
        // The flagless form never reads the base for a plain word.
        assert_eq!(decode_colored_slot_word(plain), plain, "{plain:#x}");
    }
    let colored = COLORED_SLOT_TAG | remapped | 0x40;
    assert_eq!(decode_colored_slot_word_with(base, colored), base + 0x40);
    assert_eq!(
        decode_colored_slot_word_with(0, colored),
        colored,
        "no base published: verbatim"
    );
    let max = COLORED_SLOT_TAG | COLORED_SLOT_METADATA_MASK | COLORED_SLOT_OFFSET_MASK;
    assert_eq!(
        decode_colored_slot_word_with(base, max),
        base + COLORED_SLOT_OFFSET_MASK
    );
    for garbage in [
        0x8D8D_8D8D_8D8D_8D8Du64,
        u64::MAX,
        COLORED_SLOT_TAG | (1 << 46),
        // zt t21/x17: tagged but no color bit -- `i64::MIN` (the JIT
        // helpers' sentinel) and a `long` just above it.
        i64::MIN as u64,
        COLORED_SLOT_TAG | 0x40,
    ] {
        assert_eq!(
            decode_colored_slot_word_with(base, garbage),
            garbage,
            "{garbage:#x}"
        );
    }
}

/// zt t22/zb5: once the color flips (`CRATONVM_ZGC_COLOR_FLIP`), a slot
/// can hold any of the four colors; the GC readers' decode answers the
/// same address for each (relocation is stop-the-world, so a stale
/// color's offset is current). Pure arithmetic: nothing is published.
#[test]
fn zt22zb5_the_gc_decode_answers_every_color() {
    let base = 0x7f12_3400_0000u64;
    for bit in 42..=45u32 {
        for off in [0u64, 0x40, COLORED_SLOT_OFFSET_MASK] {
            let word = COLORED_SLOT_TAG | (1u64 << bit) | off;
            assert_eq!(
                decode_colored_slot_word_with(base, word),
                base + off,
                "color bit {bit}, offset {off:#x}"
            );
        }
    }
}

#[test]
fn roundtrip_shift3() {
    let base = 0x7f00_0000_0000u64;
    for delta in [8u64, 16, 4096, 1 << 20, (u32::MAX as u64) << 3] {
        let addr = base + delta;
        let n = encode_with(base, 3, addr);
        assert_eq!(decode_with(base, 3, n), addr, "delta {delta:#x}");
    }
    assert_eq!(encode_with(base, 3, 0), 0);
    assert_eq!(decode_with(base, 3, 0), 0);
}

#[test]
fn roundtrip_shift0() {
    let base = 0x1000u64;
    for delta in [1u64, 7, 8, 1 << 20, u32::MAX as u64] {
        let addr = base + delta;
        assert_eq!(decode_with(base, 0, encode_with(base, 0, addr)), addr);
    }
}

#[test]
fn the_geometry_is_a_parameter_not_a_global() {
    // The same address encodes differently under two geometries, and each
    // decodes back through its own — the property that lets a test exercise
    // the codec without publishing anything.
    let addr = 0x7f00_0000_1000u64;
    let a = encode_with(0x7f00_0000_0000, 3, addr);
    let b = encode_with(0x7f00_0000_0800, 3, addr);
    assert_ne!(a, b);
    assert_eq!(decode_with(0x7f00_0000_0000, 3, a), addr);
    assert_eq!(decode_with(0x7f00_0000_0800, 3, b), addr);
    assert!(
        !narrow_oops_enabled(),
        "a unit test must not leave the process-global geometry published"
    );
}
