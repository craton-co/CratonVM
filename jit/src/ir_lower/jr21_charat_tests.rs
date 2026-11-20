use super::{lower, CompiledMethod, JitRuntimeHelpers};
use crate::ir::{CmpOp, Graph, IrType, Op, SafepointSnapshot};
use crate::ir_schedule;

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; these bodies reach no helper.
    unsafe { std::mem::zeroed() }
}

/// `(value, index, coder) -> Op::StrCharDecode`, and nothing else.
fn decode_body() -> CompiledMethod {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let value = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let index = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let coder = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let ch = g.add(
        Op::StrCharDecode,
        IrType::Int,
        vec![ctrl, mem, value, index, coder],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, ch], Some(1));
    let schedule = ir_schedule::schedule(&g);
    lower(&g, &schedule, 3, 3, &no_helpers()).expect("the decode lowers")
}

/// LATIN1 (`coder == 0`) reads the byte zero-extended -- `0xE9` is 233,
/// never -23 -- and UTF16 (`coder != 0`) the little-endian unit at byte
/// `2 * index`, the two answers the branchless combine gave.
#[test]
#[cfg(target_arch = "x86_64")]
fn jr21_charat_the_decode_reads_the_latin1_byte_or_the_utf16_unit() {
    let cm = decode_body();
    let call = |a: &[u8], index: usize, coder: i64| -> i32 {
        // Cast: a test array's address, and a small index.
        let args = [a.as_ptr() as i64, index as i64, coder];
        // SAFETY: `a` is a synthetic byte array (length word and elements at the layout's offsets) that outlives the call; the body reads one element of it inside its length and calls nothing.
        let r = unsafe { cm.try_call(&args) }.expect("the decode runs");
        r as i32 // Cast: an `int` result in the low half
    };
    let latin1 = [0x48u8, 0xE9, 0xFF, 0x00, 0x7A];
    let elems: Vec<i64> = latin1.iter().map(|&b| i64::from(b)).collect();
    let a = crate::tests::synthetic_array(1, &elems);
    for (k, &b) in latin1.iter().enumerate() {
        assert_eq!(call(&a, k, 0), i32::from(b), "LATIN1 char {k}");
    }
    let units = [0x0041u16, 0x20AC, 0xFFFF, 0xD800, 0x00E9];
    let le: Vec<i64> = units
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .map(i64::from)
        .collect();
    let u = crate::tests::synthetic_array(1, &le);
    for (k, &c) in units.iter().enumerate() {
        assert_eq!(call(&u, k, 1), i32::from(c), "UTF16 char {k}");
    }
}

/// `Guard(recv != null)` at bci 2 and again at bci 5 in one block: the
/// second is proven by the first (`ir_check_elim::guard_proven`) and emits
/// no deopt point; `CRATONVM_JIT_IR_GUARD_FACT_ELIDE=0` emits both.
fn twice_guarded() -> Graph {
    let mut g = Graph::empty(12);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null], None);
    g.add(Op::Guard { bci: 2 }, IrType::Void, vec![ctrl, nn], Some(2));
    g.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, nn], Some(5));
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, one], Some(8));
    for bci in [2, 5] {
        g.safepoints.push(SafepointSnapshot::new(
            bci,
            vec![recv],
            Vec::new(),
            Vec::new(),
        ));
    }
    g
}


