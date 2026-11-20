use super::*;
use crate::ir::{CmpOp, IrBuilder, SafepointSnapshot};

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the tests below reach no helper.
    unsafe { std::mem::zeroed() }
}

fn compile_one_int_arg(code: &[u8]) -> CompiledMethod {
    let graph = IrBuilder::new(1, 1)
        .build(code, code.len())
        .expect("IR build");
    let schedule = crate::ir_schedule::schedule(&graph);
    lower(&graph, &schedule, 1, 1, &no_helpers()).expect("lower")
}

/// `Guard(Cmp(cc, a, b))` with the compare's only use the guard: the guard
/// branches to its pad on the compare's own flags (no `SETcc` boolean
/// anywhere in the body), and still passes / deopts on exactly the pairs
/// the materialised form did, for a condition, its negation's polarity and
/// an equality.
#[test]
fn r11_a_guard_branches_on_its_single_use_compare() {
    use crate::deopt::take_last_deopt;
    let cases: [(CmpOp, [(i64, i64, bool); 4]); 3] = [
        (
            CmpOp::Lt,
            [(1, 2, true), (2, 2, false), (-5, 3, true), (3, -5, false)],
        ),
        (
            CmpOp::Ne,
            [(1, 2, true), (2, 2, false), (0, 0, false), (-1, 0, true)],
        ),
        (
            CmpOp::Ge,
            [(1, 2, false), (2, 2, true), (-5, 3, false), (3, -5, true)],
        ),
    ];
    for (cc, pairs) in cases {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let cond = graph.add(Op::Cmp(cc), IrType::Int, vec![a, b], Some(5));
        let _guard = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, cond], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, b], None);
        graph
            .safepoints
            .push(SafepointSnapshot::new(5, vec![a, b], vec![], Vec::new()));
        let schedule = crate::ir_schedule::schedule(&graph);
        let method = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower");
        let setcc = method
            .code_bytes()
            .windows(3)
            .any(|w| w[0] == 0x0F && (0x90..=0x9F).contains(&w[1]) && w[2] == 0xC0);
        assert!(
            !setcc,
            "{cc:?}: the guard's compare was materialised (SETcc AL) instead of fused"
        );
        for (x, y, passes) in pairs {
            let _ = take_last_deopt();
            // SAFETY: an `int f(int, int)` body compiled above with the no-helper table; the only non-local transfer is the deopt stub, which this test expects.
            let got = unsafe { method.try_call(&[x, y]) }.expect("call");
            if passes {
                assert_eq!(got, y, "{cc:?} {x},{y}: the guard must pass");
                assert!(take_last_deopt().is_none(), "{cc:?} {x},{y}: no deopt");
            } else {
                assert_eq!(got, i64::MIN, "{cc:?} {x},{y}: the guard must deopt");
                let frame = take_last_deopt().expect("the deopt reconstructed a frame");
                assert_eq!(frame.bci, 5, "{cc:?} {x},{y}: resumes at the guard's bci");
            }
        }
    }
}

/// `dneg` is a sign-bit flip for every input, `-0.0` included: `1 / -x`
/// at `x == 0` is `-inf` (so `d2l` answers `Long.MIN_VALUE`); a
/// subtraction from `0.0` would have produced `+inf`. And the plain values
/// negate. (The arm is `BTC RAX, 63` since round 11.)
#[test]
fn r11_dneg_flips_the_sign_bit_including_zero() {
    // dconst_1 iload_0 i2d dneg ddiv d2l lreturn
    let recip = compile_one_int_arg(&[0x0f, 0x1a, 0x87, 0x77, 0x6f, 0x8f, 0xad]);
    // SAFETY: a one-int-argument body with no helper calls.
    let at_zero = unsafe { recip.try_call(&[0]) }.expect("call");
    assert_eq!(
        at_zero,
        i64::MIN,
        "1 / -(0.0) must be -inf, i.e. -0.0 was produced"
    );
    // iload_0 i2d dneg d2l lreturn
    let neg = compile_one_int_arg(&[0x1a, 0x87, 0x77, 0x8f, 0xad]);
    for x in [0i32, 1, -1, 7, -123_456, i32::MAX, i32::MIN] {
        // SAFETY: as above.
        let got = unsafe { neg.try_call(&[i64::from(x)]) }.expect("call");
        assert_eq!(got, -i64::from(x), "-(double){x}");
    }
}

/// The integer-to-FP conversions still convert after the `XORPS`
/// dependency break: `i2f`, `i2d`, `l2f` and `l2d`, round-tripped back.
#[test]
fn r11_int_to_fp_conversions_survive_the_dependency_break() {
    let shapes: [(&[u8], &str); 4] = [
        // iload_0 i2d d2l lreturn
        (&[0x1a, 0x87, 0x8f, 0xad], "i2d"),
        // iload_0 i2f f2l lreturn
        (&[0x1a, 0x86, 0x8c, 0xad], "i2f"),
        // iload_0 i2l l2d d2l lreturn
        (&[0x1a, 0x85, 0x8a, 0x8f, 0xad], "l2d"),
        // iload_0 i2l l2f f2l lreturn
        (&[0x1a, 0x85, 0x89, 0x8c, 0xad], "l2f"),
    ];
    for (code, what) in shapes {
        let cm = compile_one_int_arg(code);
        // Every value here is exact in a `float` too (|x| <= 2^24).
        for x in [0i32, 1, -1, 1000, -16_777_216, 16_777_216] {
            // SAFETY: a one-int-argument body with no helper calls.
            let got = unsafe { cm.try_call(&[i64::from(x)]) }.expect("call");
            assert_eq!(got, i64::from(x), "{what} round trip of {x}");
        }
    }
}
