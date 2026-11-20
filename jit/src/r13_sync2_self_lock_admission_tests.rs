use super::{self_lock_bytecode_admitted, SelfLockBytecodeRules};

const INSTANCE: SelfLockBytecodeRules = SelfLockBytecodeRules {
    loops: false,
    array_loops: false,
    local0_is_monitor: true,
    indy: false,
    blocks: false,
};

/// JIT round 18 wave 9 (lane syncdoor6, SD17-3): `blocks` admits javac's
/// `synchronized` block (one enter, its normal and its handler's exit) and
/// lifts nothing else; an enter without an exit is refused even then.
#[test]
fn r18_blocks_admit_only_balanced_monitor_bytecode() {
    let with_blocks = SelfLockBytecodeRules {
        blocks: true,
        ..INSTANCE
    };
    let admitted_blocks =
        |code: &[u8]| self_lock_bytecode_admitted(code, code.len(), with_blocks);
    // `synchronized int blocked(int x) { synchronized (other) {
    // o += helper2(x); } return o; }` as javac emits it.
    let blocked = [
        0x2a, 0xb4, 0x00, 0x02, 0x59, 0x4d, 0xc2, 0x2a, 0x59, 0xb4, 0x00, 0x03, 0x2a, 0x1b,
        0xb6, 0x00, 0x04, 0x60, 0xb5, 0x00, 0x03, 0x2c, 0xc3, 0xa7, 0x00, 0x08, 0x4e, 0x2c,
        0xc3, 0x2d, 0xbf, 0x2a, 0xb4, 0x00, 0x03, 0xac,
    ];
    assert!(admitted_blocks(&blocked));
    assert!(
        !admitted(&blocked),
        "the switch off refuses monitor bytecode"
    );
    // An enter with no exit, an exit with no enter.
    assert!(!admitted_blocks(&[0x2a, 0xc2, 0xb1]));
    assert!(!admitted_blocks(&[0x2a, 0xc3, 0xb1]));
    // A store to local 0 beside a block is still refused.
    let mut stores_this = blocked;
    stores_this[29] = 0x4b; // astore_0 in place of aload_3
    assert!(!admitted_blocks(&stores_this));
}

/// Round 15 wave 5 (lane sync4): `indy` admits an `invokedynamic` and
/// lifts nothing else; without it (`CRATONVM_JIT_SELF_LOCKING_SYNC_INDY=0`)
/// every indy is refused as before.
#[test]
fn indy_admits_only_the_invokedynamic_term() {
    let with_indy = SelfLockBytecodeRules {
        indy: true,
        ..INSTANCE
    };
    let admitted_indy = |code: &[u8]| self_lock_bytecode_admitted(code, code.len(), with_indy);
    // aload_0; getfield #2; invokedynamic #3 (a `"v=" + x` concat);
    // areturn.
    let concat = [0x2a, 0xb4, 0x00, 0x02, 0xba, 0x00, 0x03, 0x00, 0x00, 0xb0];
    assert!(admitted_indy(&concat));
    assert!(!admitted(&concat), "the switch off refuses every indy");
    // Monitor ops, a store to local 0 and a loop (loops off) are still
    // refused beside an indy.
    assert!(!admitted_indy(&[
        0xba, 0x00, 0x03, 0x00, 0x00, 0x57, 0x2a, 0xc2, 0xb1
    ]));
    assert!(!admitted_indy(&[0xba, 0x00, 0x03, 0x00, 0x00, 0x4b, 0xb1]));
    // invokedynamic; pop; goto 0.
    assert!(!admitted_indy(&[
        0xba, 0x00, 0x03, 0x00, 0x00, 0x57, 0xa7, 0xff, 0xfa
    ]));
    // A truncated indy is undecodable.
    assert!(!admitted_indy(&[0x2a, 0xba, 0x00, 0x03]));
}

/// The optimizing candidate's own question (`ir_self_lock_candidate`).
#[test]
fn the_invokedynamic_scan_fails_closed() {
    use super::self_lock_code_has_invokedynamic as has_indy;
    let concat = [0x2a, 0xb4, 0x00, 0x02, 0xba, 0x00, 0x03, 0x00, 0x00, 0xb0];
    assert!(has_indy(&concat, concat.len()));
    assert!(!has_indy(&[0x2a, 0xb4, 0x00, 0x02, 0xb0], 5));
    assert!(
        has_indy(&[0x2a, 0xb4, 0x00], 3),
        "undecodable code answers yes"
    );
}

/// With loops refused, the wave-4 admission
/// (`CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS=0`).
fn admitted(code: &[u8]) -> bool {
    self_lock_bytecode_admitted(code, code.len(), INSTANCE)
}

fn admitted_with_loops(code: &[u8]) -> bool {
    self_lock_bytecode_admitted(
        code,
        code.len(),
        SelfLockBytecodeRules {
            loops: true,
            ..INSTANCE
        },
    )
}

/// Round 13 wave 10 (lane sync6): a `static synchronized` method's monitor
/// is its class mirror, so a store to local 0 is an ordinary store; every
/// other refusal is the instance method's.
#[test]
fn a_static_method_may_store_local_zero_and_nothing_else_changes() {
    let static_rules = SelfLockBytecodeRules {
        local0_is_monitor: false,
        ..INSTANCE
    };
    let admitted_static =
        |code: &[u8]| self_lock_bytecode_admitted(code, code.len(), static_rules);
    // iload_0; iconst_1; iadd; istore_0; iload_0; ireturn
    let bump_param = [0x1a, 0x04, 0x60, 0x3b, 0x1a, 0xac];
    assert!(admitted_static(&bump_param));
    assert!(
        !admitted(&bump_param),
        "an instance method's local 0 is its monitor"
    );
    // iinc 0 1; iload_0; ireturn -- and the wide form.
    assert!(admitted_static(&[0x84, 0x00, 0x01, 0x1a, 0xac]));
    assert!(admitted_static(&[
        0xc4, 0x84, 0x00, 0x00, 0x00, 0x01, 0x1a, 0xac
    ]));
    // Monitor ops, indy and a loop (loops off) are still refused.
    assert!(!admitted_static(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1]));
    assert!(!admitted_static(&[0xba, 0x00, 0x01, 0x00, 0x00, 0xb1]));
    assert!(!admitted_static(&[0x00, 0xa7, 0xff, 0xff]));
}

/// Round 13 wave 10 (lane sync6, proposal CH4-2): `array_loops` admits a
/// loop that indexes an array, and nothing else.
#[test]
fn array_loops_admit_only_the_array_term() {
    // The `this.a[i]` loop of `loops_are_admitted_unless_they_index_an_array`.
    let array_loop = [
        0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x10, 0x2a, 0xb4, 0x00, 0x02, 0x1b, 0x2e,
        0x57, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf0, 0xb1,
    ];
    let with_arrays = SelfLockBytecodeRules {
        loops: true,
        array_loops: true,
        ..INSTANCE
    };
    assert!(self_lock_bytecode_admitted(
        &array_loop,
        array_loop.len(),
        with_arrays
    ));
    assert!(!admitted_with_loops(&array_loop));
    // `array_loops` does not lift the loop switch itself.
    let arrays_without_loops = SelfLockBytecodeRules {
        array_loops: true,
        ..INSTANCE
    };
    assert!(!self_lock_bytecode_admitted(
        &array_loop,
        array_loop.len(),
        arrays_without_loops
    ));
    // Nor any other refusal: a store to local 0 inside such a loop.
    let store_in_loop = [0x01, 0x4b, 0xa7, 0xff, 0xfe];
    assert!(!self_lock_bytecode_admitted(
        &store_in_loop,
        store_in_loop.len(),
        with_arrays
    ));
}

/// Round 13 wave 6 (lane sync3): a loop is admitted unless its range
/// indexes an array; everything else is refused exactly as without loops.
#[test]
fn loops_are_admitted_unless_they_index_an_array() {
    // iconst_0; istore_1; L: iload_1; bipush 10; if_icmpge END;
    // aload_0; dup; getfield #2; iconst_1; iadd; putfield #2;
    // iinc 1 1; goto L; END: return
    let field_loop = [
        0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x13, 0x2a, 0x59, 0xb4, 0x00, 0x02, 0x04,
        0x60, 0xb5, 0x00, 0x02, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xed, 0xb1,
    ];
    assert!(admitted_with_loops(&field_loop));
    assert!(!admitted(&field_loop), "the switch off refuses every loop");
    // The same loop reading `this.a[i]`: aload_0; getfield #2; iload_1;
    // iaload; pop.
    let array_loop = [
        0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x10, 0x2a, 0xb4, 0x00, 0x02, 0x1b, 0x2e,
        0x57, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf0, 0xb1,
    ];
    assert!(!admitted_with_loops(&array_loop));
    // An array read BEFORE the loop is outside every loop range:
    // aload_0; getfield #2; iconst_0; iaload; istore_2; iconst_0;
    // istore_1; L: iload_1; bipush 10; if_icmpge END; iinc 1 1; goto L;
    // END: iload_2; ireturn
    let array_before_loop = [
        0x2a, 0xb4, 0x00, 0x02, 0x03, 0x2e, 0x3d, 0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00,
        0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7, 0x1c, 0xac,
    ];
    assert!(admitted_with_loops(&array_before_loop));
    // A loop does not lift any other refusal: the backward goto after a
    // monitorenter, and a store to local 0 inside a loop.
    assert!(!admitted_with_loops(&[0x2a, 0xc2, 0x00, 0xa7, 0xff, 0xff]));
    assert!(!admitted_with_loops(&[0x01, 0x4b, 0xa7, 0xff, 0xfe]));
}

#[test]
fn a_field_bump_that_calls_is_admitted() {
    // aload_0; dup; getfield #2; iconst_1; iadd; putfield #2;
    // aload_0; invokevirtual #3; ireturn
    let code = [
        0x2a, 0x59, 0xb4, 0x00, 0x02, 0x04, 0x60, 0xb5, 0x00, 0x02, 0x2a, 0xb6, 0x00, 0x03,
        0xac,
    ];
    assert!(admitted(&code));
    // A store to ANOTHER local is fine: iconst_0; istore_1; iload_1; ireturn.
    assert!(admitted(&[0x03, 0x3c, 0x1b, 0xac]));
    assert!(admitted(&[0x03, 0x36, 0x01, 0x1b, 0xac]));
}

#[test]
fn a_store_to_local_zero_is_refused_in_every_encoding() {
    // aconst_null; astore_0; return
    assert!(!admitted(&[0x01, 0x4b, 0xb1]));
    // aconst_null; astore 0; return
    assert!(!admitted(&[0x01, 0x3a, 0x00, 0xb1]));
    // iconst_0; istore_0; return
    assert!(!admitted(&[0x03, 0x3b, 0xb1]));
    // iinc 0 1; return
    assert!(!admitted(&[0x84, 0x00, 0x01, 0xb1]));
    // aconst_null; wide astore 0; return
    assert!(!admitted(&[0x01, 0xc4, 0x3a, 0x00, 0x00, 0xb1]));
    // wide iinc 0 1; return
    assert!(!admitted(&[0xc4, 0x84, 0x00, 0x00, 0x00, 0x01, 0xb1]));
    // lstore_0 / dstore_0 / fstore_0.
    assert!(!admitted(&[0x09, 0x3f, 0xb1]));
    assert!(!admitted(&[0x0e, 0x47, 0xb1]));
    assert!(!admitted(&[0x0b, 0x43, 0xb1]));
}

#[test]
fn monitors_loops_and_undecodable_code_are_refused() {
    // aload_0; monitorenter; aload_0; monitorexit; return
    assert!(!admitted(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1]));
    // A backward goto: nop; goto -1.
    assert!(!admitted(&[0x00, 0xa7, 0xff, 0xff]));
    // Truncated: getfield with one operand byte.
    assert!(!admitted(&[0x2a, 0xb4, 0x00]));
}
