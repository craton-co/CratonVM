use super::*;

const ALL: [u16; 16] = [
    SLG_FUSED_LDC_BOUND,
    SLG_LDC_HOISTED_PC,
    SLG_LDC_INLINE_HIT,
    SLG_FUSED_GETSTATIC_BOUND,
    SLG_LDC2W_INLINE_HIT,
    SLG_GETSTATIC_INLINE,
    SLG_GETSTATIC_REMAP_TRACE,
    SLG_PUTSTATIC_INLINE,
    SLG_FUSED_GETSTATIC_IFZERO,
    SLG_FUSED_STATIC_INCREMENT,
    SLG_FUSED_INCREMENT_LDC2W,
    SLG_FUSED_WIDE_LOCAL_GETFIELD,
    SLG_FUSED_ARRAY_ACCUMULATE_LONG,
    SLG_FUSED_LOAD_PAIR,
    SLG_FUSED_ICONST_GOTO,
    SLG_FUSED_ICONST_ARITH,
];

#[test]
fn the_gate_bits_are_distinct_single_bits() {
    let mut seen = 0u16;
    for bit in ALL {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(seen & bit, 0);
        seen |= bit;
    }
}

#[test]
fn each_gate_reads_back_as_its_bool() {
    for (i, &bit) in ALL.iter().enumerate() {
        let mut gates = ALL.map(|b| (false, b));
        gates[i].0 = true;
        let word = static_ldc_gate_word(gates);
        for &other in &ALL {
            assert_eq!(word & other != 0, other == bit);
        }
    }
    assert_eq!(static_ldc_gate_word(ALL.map(|b| (false, b))), 0);
    assert_eq!(
        static_ldc_gate_word(ALL.map(|b| (true, b))),
        ALL.iter().fold(0, |w, b| w | b)
    );
}
