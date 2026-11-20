use super::*;

const DIAG: [u16; 14] = [
    DG_FIELD_PHASES,
    DG_ACMP_IDENTITY_TRACE,
    DG_TRACE_SB_FILTER,
    DG_PAIR_CENSUS,
    DG_INVOKE_PHASES,
    DG_RETURN_DIAGNOSTICS,
    DG_BACKEDGE_POLL_GATE_OFF,
    DG_LOOP_WORK,
    DG_THREAD_IS_VIRTUAL,
    DG_OBSERVERS_HOISTED,
    DG_OBSERVER_REREAD,
    DG_STACK_DUMP_AT_POLL,
    DG_METHOD_ENTRY_FRAME_CONTROL,
    DG_FINALIZER_INIT_RETURN,
];
// Interpreter round i6 wave 115 (lane L8): a `u16` word, with the two
// branch fusions' bits.
const DOOR: [u16; 16] = [
    DOOR_INVOKE_FAST,
    DOOR_NONVIRTUAL_FAST,
    DOOR_SIG_POLY_SITE,
    DOOR_SIG_POLY_SITE_FIRST,
    DOOR_FUSED_LONG_ARITH,
    DOOR_FUSED_DOUBLE_ARITH,
    DOOR_FUSED_LONG_INT_OPERAND,
    DOOR_FUSED_I2L_LONG_TAIL,
    DOOR_FUSED_ZERO_BRANCH,
    DOOR_FUSED_NULL_BRANCH,
    DOOR_FUSED_GETFIELD_CHAIN,
    DOOR_FUSED_NULL_BRANCH_WIDE,
    DOOR_FAST_SWITCH,
    DOOR_FUSED_LOOP_STEP_TEST,
    DOOR_FUSED_STORE_CHAIN,
    DOOR_FUSED_STORE_CHAIN_WIDE,
];
// Wave 84 (lane L7): `TG_PENDING` and `TG_SLOT_SIGNALS` join the byte.
const TOP: [u8; 5] = [
    TG_SINGLE_STEP,
    TG_TOP_HOOKS,
    TG_STACK_DUMP,
    TG_PENDING,
    TG_SLOT_SIGNALS,
];

/// Wave 84 (lane L7, `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST`): the loop
/// top's one test covers exactly the parked-signal bit and the stack-dump
/// hook, never the mode bit (which would send every bytecode of a
/// switched-off loop into the cold block) nor the per-bytecode hooks.
#[test]
fn the_top_signal_mask_is_pending_and_stack_dump_only() {
    assert_eq!(TG_TOP_SIGNALS, TG_PENDING | TG_STACK_DUMP);
    assert_eq!(TG_TOP_SIGNALS & TG_SLOT_SIGNALS, 0);
    assert_eq!(TG_TOP_SIGNALS & (TG_SINGLE_STEP | TG_TOP_HOOKS), 0);
}

#[test]
fn the_gate_bits_are_distinct_single_bits() {
    let mut seen = 0u16;
    for bit in DIAG {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(seen & bit, 0);
        seen |= bit;
    }
    let mut seen = 0u16;
    for bit in DOOR {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(seen & bit, 0);
        seen |= bit;
    }
    let mut seen = 0u8;
    for bit in TOP {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(seen & bit, 0);
        seen |= bit;
    }
}

#[test]
fn the_field_phase_bit_is_bit_zero() {
    // The `getfield` arms pass `diag_gates & DG_FIELD_PHASES != 0` by
    // value; at bit 0 that is the word's low bit itself.
    assert_eq!(DG_FIELD_PHASES, 1);
}

/// Wave 83 (lane L7): the loop builds its words with `gate_word!`; it
/// answers as the array fold it replaced.
#[test]
fn the_gate_word_macro_answers_as_the_fold() {
    for mask in 0u16..(1 << 4) {
        let on = |i: u16| mask & (1 << i) != 0;
        let fold = static_ldc_gate_word([
            (on(0), DG_FIELD_PHASES),
            (on(1), DG_PAIR_CENSUS),
            (on(2), DG_OBSERVER_REREAD),
            (on(3), DG_STACK_DUMP_AT_POLL),
        ]);
        let word = gate_word!(u16;
            (on(0), DG_FIELD_PHASES),
            (on(1), DG_PAIR_CENSUS),
            (
                on(2),
                DG_OBSERVER_REREAD,
            ),
            (on(3), DG_STACK_DUMP_AT_POLL),
        );
        assert_eq!(word, fold, "{mask:#x}");
        let byte = gate_word!(u8; (on(0), TG_SINGLE_STEP), (on(1), TG_STACK_DUMP));
        assert_eq!(
            byte,
            gate_byte([(on(0), TG_SINGLE_STEP), (on(1), TG_STACK_DUMP)])
        );
    }
}

#[test]
fn each_gate_reads_back_as_its_bool() {
    for (i, &bit) in DIAG.iter().enumerate() {
        let mut gates = DIAG.map(|b| (false, b));
        gates[i].0 = true;
        let word = static_ldc_gate_word(gates);
        for &other in &DIAG {
            assert_eq!(word & other != 0, other == bit);
        }
    }
    for &bit in &TOP {
        let mut word = 0u8;
        for &other in &TOP {
            word = with_gate_bit(word, other, other == bit);
        }
        assert_eq!(word, bit);
    }
    // The door word is a `u16` since wave 115 (lane L8): packed by the
    // same fold as `static_ldc_gates`.
    for (i, &bit) in DOOR.iter().enumerate() {
        let mut gates = DOOR.map(|b| (false, b));
        gates[i].0 = true;
        let word = static_ldc_gate_word(gates);
        for &other in &DOOR {
            assert_eq!(word & other != 0, other == bit);
        }
    }
    assert_eq!(static_ldc_gate_word(DOOR.map(|b| (false, b))), 0);
    assert_eq!(
        static_ldc_gate_word(DOOR.map(|b| (true, b))),
        DOOR.iter().fold(0, |w, b| w | b)
    );
    assert_eq!(
        gate_byte([(true, TG_SINGLE_STEP), (false, TG_TOP_HOOKS)]),
        TG_SINGLE_STEP
    );
}

#[test]
fn a_bit_update_leaves_the_other_bits_alone() {
    let all = TOP.iter().fold(0u8, |w, b| w | b);
    for &bit in &TOP {
        assert_eq!(with_gate_bit(all, bit, false), all & !bit);
        assert_eq!(with_gate_bit(all & !bit, bit, true), all);
        assert_eq!(with_gate_bit(all, bit, true), all);
    }
}
