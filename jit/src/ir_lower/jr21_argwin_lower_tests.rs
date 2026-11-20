use super::*;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn position(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn reference_phi_home_checks_the_phi_and_only_its_value_inputs() {
    assert_eq!(
        Lowerer::owned_window_named_values(17, true, &[4, 23, 29]),
        vec![17, 23, 29],
        "exclude the phi control input but inspect each incoming value"
    );
    assert_eq!(
        Lowerer::owned_window_named_values(17, false, &[4, 23]),
        vec![17],
        "non-phi arguments retain the original single-node check"
    );
}

/// The site rule takes only a statically bound call at a bci no
/// exception-table row covers, outside a splice, with no armed exit, in a
/// method-entry compile of a body that does not lock itself, whose caller
/// replay is exact -- and asks the bytecode scan last.
#[test]
fn the_owned_args_site_rule_admits_only_a_bound_unprotected_replay_exact_site() {
    let yes = || true;
    let site = |op: Option<u8>, kind: u8| {
        ir_direct_call_site_admits_owned_args(op, kind, 4, &[], false, false, false, false, yes)
    };
    assert!(site(Some(0xb8), 3), "invokestatic");
    assert!(site(Some(0xb7), 1), "invokespecial");
    assert!(site(Some(0xb6), 1), "an invokevirtual pinned to its owner");
    assert!(
        !site(Some(0xb6), 0),
        "a virtual dispatch reads its receiver"
    );
    assert!(
        !site(Some(0xb9), 2),
        "an interface dispatch reads its receiver"
    );
    assert!(
        !site(Some(0xb8), 4),
        "the self-recursive marker has its own route"
    );
    assert!(!site(None, 3), "no opcode at the bci");
    let rows = [(0usize, 8usize, 20usize)];
    assert!(
        !ir_direct_call_site_admits_owned_args(
            Some(0xb8),
            3,
            4,
            &rows,
            false,
            false,
            false,
            false,
            yes
        ),
        "a protected bci"
    );
    assert!(
        ir_direct_call_site_admits_owned_args(
            Some(0xb8),
            3,
            8,
            &rows,
            false,
            false,
            false,
            false,
            yes
        ),
        "the row's end is exclusive"
    );
    for (osr, locks, splice, armed) in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        assert!(!ir_direct_call_site_admits_owned_args(
            Some(0xb8),
            3,
            4,
            &[],
            osr,
            locks,
            splice,
            armed,
            yes
        ));
    }
    assert!(
        !ir_direct_call_site_admits_owned_args(
            Some(0xb8),
            3,
            4,
            &[],
            false,
            false,
            false,
            false,
            || false
        ),
        "a caller whose prefix commits"
    );
    let asked = std::cell::Cell::new(false);
    assert!(!ir_direct_call_site_admits_owned_args(
        Some(0xb6),
        0,
        4,
        &[],
        false,
        false,
        false,
        false,
        || {
            asked.set(true);
            true
        }
    ));
    assert!(!asked.get(), "the bytecode scan is asked last");
}


#[test]
fn cycle_snapshot_is_fresh_only_after_every_path_executes_the_array() {
    let successors = |pc| match pc {
        40 => vec![10], // call returns to the loop header
        10 => vec![20],
        20 => vec![30], // array allocation
        30 => vec![40],
        _ => vec![],
    };
    assert!(!ir_owned_cycle_bytecode_path_without_array(
        40,
        30,
        20,
        successors,
        &[]
    ));
    // The loop header and the allocation itself precede the fresh value.
    assert!(ir_owned_cycle_bytecode_path_without_array(
        40,
        10,
        20,
        successors,
        &[]
    ));
    assert!(ir_owned_cycle_bytecode_path_without_array(
        40,
        20,
        20,
        successors,
        &[]
    ));
}

#[test]
fn array_allocation_exception_path_does_not_prove_freshness() {
    let successors = |pc| match pc {
        10 => vec![20],
        20 => vec![30],
        30 => vec![],
        40 => vec![],
        _ => vec![],
    };
    assert!(ir_owned_cycle_bytecode_path_without_array(
        10,
        40,
        20,
        successors,
        &[(20, 21, 40)]
    ));
}

#[test]
fn cycle_reentry_exempts_only_a_fresh_reference_array() {
    let array = Op::NewArray {
        element_type: 0,
        component_class_id: 1,
    };
    assert!(ir_owned_cycle_arg_is_fresh_array(IrType::Ref, &array, true));
    assert!(!ir_owned_cycle_arg_is_fresh_array(IrType::Ref, &array, false));
    assert!(!ir_owned_cycle_arg_is_fresh_array(IrType::Int, &array, true));
    assert!(!ir_owned_cycle_arg_is_fresh_array(
        IrType::Ref,
        &Op::New {
            class_id: 1,
            num_fields: 0,
        },
        true
    ));
}

#[test]
fn cycle_reentry_must_pass_through_the_cross_block_allocation() {
    // Call -> allocation -> call: every repeated call gets a fresh array.
    assert!(ir_owned_cycle_reentry_crosses_definition(
        1,
        2,
        &[vec![1], vec![2], vec![1]]
    ));
    // A bypass edge can repeat the call with the previous array.
    assert!(!ir_owned_cycle_reentry_crosses_definition(
        1,
        2,
        &[vec![1], vec![2], vec![1, 3], vec![2]]
    ));
    // No call cycle means the bytecode-level cycle alone proves nothing.
    assert!(!ir_owned_cycle_reentry_crosses_definition(
        1,
        2,
        &[vec![1], vec![3], vec![], vec![]]
    ));
}

/// Owned-argument and window switches are default ON; splice replay stays opt-in.
/// The latter remains off until the complete retention gate passes.


/// The window zeroes the home, then the home's word of the site's push
/// under the push's own guards (no thread; a tagged base), and is consumed.
/// The frame-block form (no per-call push) zeroes the home alone.
#[test]
fn the_window_zeroes_the_home_and_its_shadow_word_under_the_push_guards() {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.shadow_thread_slot_off = 16;
    lo.shadow_savebase_slot_off = 24;
    lo.pending_shadow = vec![40, 48];
    lo.direct_call_arg_window = vec![48];
    let at = lo.buf.pos();
    lo.emit_owned_call_arg_window_clears();
    let code = lo.buf.as_slice()[at..].to_vec();
    assert!(
        code.starts_with(&[0x48, 0xC7, 0x85, 0xD0, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]),
        "the home [rbp - 48]: {code:02x?}"
    );
    assert!(
        contains(
            &code,
            &[0x4C, 0x8B, 0x9D, 0xF0, 0xFF, 0xFF, 0xFF, 0x4D, 0x85, 0xDB, 0x0F, 0x84]
        ),
        "no thread, no push: skipped"
    );
    assert!(
        contains(
            &code,
            &[
                0x4C, 0x8B, 0x9D, 0xE8, 0xFF, 0xFF, 0xFF, 0x49, 0xF7, 0xC3, 1, 0, 0, 0, 0x0F,
                0x85
            ]
        ),
        "a tagged base (the push overflowed): skipped"
    );
    assert!(
        code.ends_with(&[0x49, 0xC7, 0x83, 8, 0, 0, 0, 0, 0, 0, 0]),
        "the push's word 1 (the home is the second published word)"
    );
    assert!(lo.direct_call_arg_window.is_empty(), "consumed");

    lo.pending_shadow.clear();
    lo.direct_call_arg_window = vec![40];
    let at = lo.buf.pos();
    lo.emit_owned_call_arg_window_clears();
    assert_eq!(
        &lo.buf.as_slice()[at..],
        &[0x48, 0xC7, 0x85, 0xD8, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0],
        "frame block: the home alone"
    );

    // Nothing planned: nothing emitted.
    let at = lo.buf.pos();
    lo.emit_owned_call_arg_window_clears();
    assert_eq!(lo.buf.pos(), at);
}

/// The service's cold side at an owned site hands over the owned word
/// (the lean encoding when lean) and a null buffer; elsewhere, the count.
#[test]
fn an_owned_site_hands_the_service_the_owned_word_and_no_buffer() {
    const SERVICE: u64 = 0x7fff_0000_0000_5200;
    let reg = CALL_ARG_REGS[2];
    let null_arg2 = [
        0x48 | if reg >= 8 { 0x01 } else { 0 },
        0xB8 + (reg & 7),
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    let emit = |owned: bool, lean: bool| -> Vec<u8> {
        let mut lo = super::tests::lowerer_with_resident_xmm(4096, None);
        lo.service_callee_deopt = SERVICE as usize;
        lo.direct_call_args_owned = owned;
        lo.service_lean_bit_pending = lean;
        let at = lo.buf.pos();
        lo.emit_callee_deopt_service_cold(0x1234, 0, &[], true, &mut None);
        lo.buf.as_slice()[at..].to_vec()
    };
    // Cast: the service word's bits.
    let owned_word = (crate::service_args_word(None, false) as u64).to_le_bytes();
    let owned_lean = (crate::service_args_word(None, true) as u64).to_le_bytes();
    let code = emit(true, false);
    assert!(
        contains(&code, &SERVICE.to_le_bytes()),
        "the service is called"
    );
    assert!(contains(&code, &owned_word), "the owned word");
    assert!(contains(&code, &null_arg2), "no buffer");
    let code = emit(true, true);
    assert!(contains(&code, &owned_lean), "the lean owned word");
    assert!(!contains(&code, &owned_word));
    let code = emit(false, false);
    assert!(!contains(&code, &owned_word), "a copying site: the count");
    assert!(!contains(&code, &null_arg2), "a copying site: its buffer");
}

// The whole route: the window's stores sit after the marshal and before
// `MOV RAX, entry; CALL RAX`, and the site's service call (inline, or
// the deferred cold tail) carries the owned word.
