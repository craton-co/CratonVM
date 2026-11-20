use super::*;

/// `ByteBuffer.wrap([BII)` as javac emits it: `try { return new
/// HeapByteBuffer(a, off, len, null); } catch (IllegalArgumentException x)
/// { throw new IndexOutOfBoundsException(); }`. The handler is the tail of
/// the body: nothing follows its `athrow`.
const WRAP: [u8; 21] = [
    0xbb, 0x00, 0x31, // 0: new #49 HeapByteBuffer
    0x59, // 3: dup
    0x2a, // 4: aload_0
    0x1b, // 5: iload_1
    0x1c, // 6: iload_2
    0x01, // 7: aconst_null
    0xb7, 0x00, 0x36, // 8: invokespecial #54 <init>
    0xb0, // 11: areturn
    0x4e, // 12: astore_3 (handler)
    0xbb, 0x00, 0x3b, // 13: new #59 IndexOutOfBoundsException
    0x59, // 16: dup
    0xb7, 0x00, 0x3d, // 17: invokespecial #61 <init>()V
    0xbf, // 20: athrow
];
const ROWS: [(usize, usize, usize); 1] = [(0, 11, 12)];
const SWITCH: &str = concat!("CRATON", "VM_JIT_IR_SPLICE_TAIL_HANDLER");

fn plan_with(value: Option<&str>) -> Option<SpliceCalleeHandlerPlan> {
    cratonvm_types::flags::with_thread_overrides(&[(SWITCH, value)], || {
        splice_callee_handler_plan_with(&WRAP, WRAP.len(), &ROWS, true)
    })
}

#[test]
fn a_handler_that_ends_the_body_is_admitted_and_masked_to_the_end() {
    let plan = plan_with(Some("1")).expect("=1: the wrap shape is admitted");
    assert_eq!(plan.runs, vec![(12, 21)]);
    assert_eq!(
        &plan.code[12..],
        &[0; 9],
        "the handler is masked up to the end"
    );
    assert_eq!(
        &plan.code[..12],
        &WRAP[..12],
        "the try range is the method's own"
    );
}

#[test]
fn the_default_admits_and_the_kill_switch_restores_the_refusal() {
    assert!(plan_with(None).is_some());
}

#[test]
fn the_masked_tail_handler_follows_the_last_return() {
    let masked = plan_with(Some("1")).expect("admitted").code;
    let ask = |code: &[u8], handlers: &[usize], value: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, value)], || {
            splice_tail_handler_follows_return(code, code.len(), 11, handlers.iter().copied())
        })
    };
    assert!(
        ask(&masked, &[12], Some("1")),
        "the resolver and builder admit it"
    );
    assert!(ask(&masked, &[12], None), "default on");
    assert!(
        !ask(&masked, &[13], Some("1")),
        "no handler starts after the return"
    );
    assert!(
        !ask(&WRAP, &[12], Some("1")),
        "an unmasked tail is real code"
    );
}

#[test]
fn a_tail_run_without_its_athrow_is_still_refused() {
    // 20: athrow -> nop: the run falls off the end of the body.
    let mut code = WRAP;
    code[20] = 0x00;
    let plan = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
        splice_callee_handler_plan_with(&code, code.len(), &ROWS, true)
    });
    assert!(plan.is_none());
}
