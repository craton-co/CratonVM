use super::{
    note_osr_optimizing_refusal, osr_optimizing_cache_disabled, osr_optimizing_memo_disabled,
    osr_optimizing_refusal_key, osr_reentry_memo_defers_to_optimizing,
    osr_reentry_memo_optimizing_pending, osr_reentry_memo_put,
    osr_reentry_memo_set_optimizing_pending, osr_reentry_stamp, osr_single_pass_preferred,
    remember_osr_optimizing_artifact, BytecodeLoopScan, OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

const METHOD: &str = "run";
const DESC: &str = "()J";

// lload_0; lconst_1; lcmp; ifeq 12; invokestatic #1; goto 0; return
const LCMP_LOOP_WITH_CALL: &[u8] = &[
    0x1e, 0x0a, 0x94, 0x99, 0x00, 0x09, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xf7, 0xb1,
];
// lload_0; lconst_1; lcmp; ifeq 9; goto 0; invokestatic #1; return
const LCMP_LOOP_CALL_AFTER: &[u8] = &[
    0x1e, 0x0a, 0x94, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfa, 0xb8, 0x00, 0x01, 0xb1,
];
// The timing-harness shape (round 11 wave 12): a call-free `lcmp` loop
// inside a loop that calls.
// 0: invokestatic #1; 3: lload_0; 4: lconst_1; 5: lcmp; 6: ifne 3;
// 9: invokestatic #1; 12: goto 0; 15: return
const LCMP_LOOP_INSIDE_A_CALL_LOOP: &[u8] = &[
    0xb8, 0x00, 0x01, 0x1e, 0x0a, 0x94, 0x9a, 0xff, 0xfd, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xf4,
    0xb1,
];

fn bytecode_loop_contains_call(code: &[u8]) -> bool {
    BytecodeLoopScan::of(code).a_loop_calls()
}

#[test]
fn a_call_counts_only_inside_a_back_edge_range() {
    assert!(bytecode_loop_contains_call(LCMP_LOOP_WITH_CALL));
    assert!(!bytecode_loop_contains_call(LCMP_LOOP_CALL_AFTER));
    // invokestatic #1; goto_w -3 (back to 0)
    assert!(bytecode_loop_contains_call(&[
        0xb8, 0x00, 0x01, 0xc8, 0xff, 0xff, 0xff, 0xfd
    ]));
    // invokestatic #1; goto_w +5 (forward); return
    assert!(!bytecode_loop_contains_call(&[
        0xb8, 0x00, 0x01, 0xc8, 0x00, 0x00, 0x00, 0x05, 0xb1
    ]));
    // invokedynamic #1; goto 0: an uncommon trap in the IR tier, not a call.
    assert!(!bytecode_loop_contains_call(&[
        0xba, 0x00, 0x01, 0x00, 0x00, 0xa7, 0xff, 0xfb
    ]));
    // Truncated branches are not back edges, and do not panic.
    assert!(!bytecode_loop_contains_call(&[0xa7]));
    assert!(!bytecode_loop_contains_call(&[
        0xb8, 0x00, 0x01, 0xc8, 0xff
    ]));
    assert!(!bytecode_loop_contains_call(&[]));
}

#[test]
fn the_single_pass_preference_yields_to_a_call_loop_only_with_exits_admitted() {
    // Exits not admitted: exactly `osr_optimizing_not_known_better`.
    assert!(osr_single_pass_preferred(LCMP_LOOP_WITH_CALL, false));
    assert!(osr_single_pass_preferred(LCMP_LOOP_CALL_AFTER, false));
    // Exits admitted: a call inside the loop is outside the measurement.
    assert!(!osr_single_pass_preferred(LCMP_LOOP_WITH_CALL, true));
    // A call after the loop leaves the measured shape alone.
    assert!(osr_single_pass_preferred(LCMP_LOOP_CALL_AFTER, true));
    // No gated opcode: never preferred.
    // iload_0; ifeq 10; invokestatic #1; goto 0; return
    let int_loop = [
        0x1a, 0x99, 0x00, 0x09, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xf9, 0xb1,
    ];
    assert!(bytecode_loop_contains_call(&int_loop));
    assert!(!osr_single_pass_preferred(&int_loop, false));
    assert!(!osr_single_pass_preferred(&int_loop, true));
}

/// Round 11 wave 12: a call in a loop that only ENCLOSES the call-free
/// `lcmp` loop does not lift the preference. Wave 11's rule (any call in
/// any loop) did, and with exits default-ON that was the default.
#[test]
fn an_enclosing_call_loop_does_not_lift_the_preference_for_a_call_free_gated_loop() {
    assert!(bytecode_loop_contains_call(LCMP_LOOP_INSIDE_A_CALL_LOOP));
    assert!(osr_single_pass_preferred(
        LCMP_LOOP_INSIDE_A_CALL_LOOP,
        true
    ));
    assert!(osr_single_pass_preferred(
        LCMP_LOOP_INSIDE_A_CALL_LOOP,
        false
    ));
    let scan = BytecodeLoopScan::of(LCMP_LOOP_INSIDE_A_CALL_LOOP);
    assert_eq!(scan.loops, vec![(3, 6), (0, 12)]);
    assert!(!scan.every_gated_loop_calls());
    // Every gated loop calls: lifted.
    assert!(BytecodeLoopScan::of(LCMP_LOOP_WITH_CALL).every_gated_loop_calls());
    // The gated opcode outside every loop, and an `int` loop that calls:
    // no loop of the measured shape, so lifted as in wave 11.
    // lload_0; lconst_1; lcmp; pop; iload_2; ifeq 14; invokestatic #1; goto 5; return
    let gated_outside = [
        0x1e, 0x0a, 0x94, 0x57, 0x1c, 0x99, 0x00, 0x09, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xfa,
        0xb1,
    ];
    assert!(BytecodeLoopScan::of(&gated_outside).every_gated_loop_calls());
    assert!(!osr_single_pass_preferred(&gated_outside, true));
    assert!(osr_single_pass_preferred(&gated_outside, false));
}

/// A re-entry memo entry put while the worker still owed its optimizing
/// verdict yields once that body is cached, and becomes exact again once
/// the verdict is a refusal.
#[test]
fn a_pending_reentry_memo_entry_yields_to_the_cached_optimizing_body() {
    if osr_optimizing_cache_disabled() || osr_optimizing_memo_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let Some(stamp) = osr_reentry_stamp(&shared) else {
        return; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
    };
    let cid = ClassId::new(4343);
    let method: Arc<str> = Arc::from(METHOD);
    let desc: Arc<str> = Arc::from(DESC);
    let code: Arc<[u8]> = Arc::from(LCMP_LOOP_WITH_CALL);
    // Cast: the bytecode's address is its identity, as in `try_osr`.
    let code_identity = Arc::as_ptr(&code) as *const u8 as usize;
    let single_pass = {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.compiled_via_osr = true;
        cm.osr_pc_to_native = Some(vec![0; 32]);
        Arc::new(cm)
    };
    let optimizing = {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.ir_osr_entries = [0u32].iter().map(|&b| (b, 0, 0)).collect();
        cm.ir_osr_sentinel_free = true;
        Arc::new(cm)
    };
    let key_at =
        |pc: usize| osr_optimizing_refusal_key(shared.vm_identity, cid, METHOD, DESC, pc);
    let names = ("r11w11/Loop", METHOD, DESC);
    let defers = |pc: usize| {
        osr_reentry_memo_defers_to_optimizing(&shared, cid, names, key_at(pc), pc, &code)
    };
    let (id, sp) = (code_identity, &single_pass);
    let put = |pc: usize| osr_reentry_memo_put(&shared, stamp, id, cid, &method, &desc, pc, sp);

    // An exact entry never yields, even with a body cached at its pc.
    put(0);
    assert!(!osr_reentry_memo_optimizing_pending());
    let pins: Arc<[cratonvm_jit::RetainedCode]> =
        Vec::<cratonvm_jit::RetainedCode>::new().into();
    let stamps = OsrBuildStamps::read(&shared, cid);
    remember_osr_optimizing_artifact(
        &shared,
        key_at(0),
        &optimizing,
        &pins,
        stamps,
        &code,
        false,
    );
    assert!(
        !defers(0),
        "not pending: the memo's own exactness argument holds"
    );

    // Pending, verdict still owed: the single-pass hit stands.
    put(9);
    osr_reentry_memo_set_optimizing_pending(true);
    assert!(!defers(9), "nothing cached or refused at pc 9 yet");
    assert!(osr_reentry_memo_optimizing_pending());

    // The worker's verdict is a refusal: exact again, the flag clears.
    note_osr_optimizing_refusal(&shared, key_at(9), shared.jit.jit_cache.redefine_epoch());
    assert!(!defers(9));
    assert!(!osr_reentry_memo_optimizing_pending());

    // Pending, and the worker cached an enterable body here: yield.
    put(0);
    osr_reentry_memo_set_optimizing_pending(true);
    assert!(
        defers(0),
        "the cached optimizing body must win the re-entry"
    );
}

/// Tier proposal 29's arm marks the memo entry this call puts. A text pin,
/// because `try_osr` needs a live frame, a loaded class and a backend.
#[test]
fn try_osr_marks_a_memo_entry_put_while_the_worker_owes_its_verdict() {
    let whole = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let src = whole.as_str();
    let body = &src[src
        .find("\npub(in crate::runtime::interpreter) fn try_osr(")
        .expect("try_osr exists")..];
    let body = &body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)];
    // Whitespace stripped, so the pin survives any wrapping `cargo fmt`
    // chooses.
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let filter = flat
        .find("letmemo_hit=memo_hit.filter(")
        .expect("a memo hit is asked whether it defers");
    let arm = flat
        .find("optimizing_verdict_pending=true;None}else{")
        .expect("the background arm marks the verdict pending");
    let mark = flat
        .find("ifoptimizing_verdict_pending{osr_reentry_memo_set_optimizing_pending(true);}")
        .expect("the memo entry put after the arm is marked");
    assert!(filter < arm && arm < mark);
}
