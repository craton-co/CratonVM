//! The monotone-induction rule (nio21 wave 3, lane L5) on the shape of
//! `UTF_8$Decoder.decodeArrayLoop`: a non-counted `while (sp < sl)` whose
//! index advances by one in one arm and by `step` in another.
use super::*;
use crate::ir::IrType;
use crate::ir_schedule;

#[derive(Clone, Copy)]
struct Shape {
    /// `Guard(sl <= sa.length)` in the pre-header (the upper half).
    length_guard: bool,
    /// `Guard(0 <= init)` in the pre-header.
    init_guard: bool,
    /// The multi-byte arm's `x - sp < k` bail test, if any.
    room: Option<i64>,
    /// The arm's `x` is a second bound `dl` instead of `sl`.
    room_on_other: bool,
    /// `Guard(0 <= dl)` in the pre-header.
    other_guard: bool,
    /// The multi-byte arm's step.
    step: i64,
    /// nio21 wave 4: an `sa[sp + j]` load on the multi arm past its room
    /// test (the decode loop's `sa[sp + 1..3]`).
    offset: Option<i64>,
}

const BASE: Shape = Shape {
    length_guard: true,
    init_guard: true,
    room: Some(2),
    room_on_other: false,
    other_guard: false,
    step: 2,
    offset: None,
};

/// ```java
/// while (sp < sl) {
///     int b = sa[sp];
///     if (b >= 0) { sp++; }
///     else { if (x - sp < k) return; sp += step; }
/// }
/// ```
/// Returns `(graph, schedule, the sa[sp] load, the sa[sp + j] load)`.
fn decode_like(s: Shape) -> (Graph, Schedule, NodeId, Option<NodeId>) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let sa = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let init = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let sl = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let dl = g.add(Op::Param(3), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    if s.length_guard {
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, sa], None);
        let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![sl, len], None);
        g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
    }
    if s.init_guard {
        let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, init], None);
        g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
    }
    if s.other_guard {
        let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, dl], None);
        g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, le], Some(0));
    }
    let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let sp = g.add(Op::Phi, IrType::Int, vec![region, init], None);
    // `if (sp >= sl) <exit>`, the builder's polarity: Proj(0) is the
    // branch target, the exit.
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![sp, sl], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], None);
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let load = g.add(
        Op::ArrayLoad(MemKind::Byte),
        IrType::Int,
        vec![stay, m0, sa, sp],
        Some(1),
    );
    let neg = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![load, zero], None);
    let arm = g.add(Op::If, IrType::Control, vec![stay, neg], None);
    let multi = g.add(Op::Proj(0), IrType::Control, vec![arm], None);
    let ascii = g.add(Op::Proj(1), IrType::Control, vec![arm], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let sp1 = g.add(Op::Add, IrType::Int, vec![sp, one], None);
    let step = g.add(Op::Const(s.step), IrType::Int, vec![], None);
    let spk = g.add(Op::Add, IrType::Int, vec![sp, step], None);
    let (multi_ok, bail) = match s.room {
        Some(k) => {
            let kk = g.add(Op::Const(k), IrType::Int, vec![], None);
            let x = if s.room_on_other { dl } else { sl };
            let rem = g.add(Op::Sub, IrType::Int, vec![x, sp], None);
            let short = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![rem, kk], None);
            let t = g.add(Op::If, IrType::Control, vec![multi, short], None);
            let bail = g.add(Op::Proj(0), IrType::Control, vec![t], None);
            let ok = g.add(Op::Proj(1), IrType::Control, vec![t], None);
            (ok, Some(bail))
        }
        None => (multi, None),
    };
    let off_load = s.offset.map(|j| {
        let jj = g.add(Op::Const(j), IrType::Int, vec![], None);
        let at = g.add(Op::Add, IrType::Int, vec![sp, jj], None);
        g.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![multi_ok, m0, sa, at],
            Some(3),
        )
    });
    let latch = g.add(Op::Merge, IrType::Control, vec![ascii, multi_ok], None);
    let next = g.add(Op::Phi, IrType::Int, vec![latch, sp1, spk], None);
    g.nodes[region as usize].inputs.push(latch);
    g.nodes[sp as usize].inputs.push(next);
    let out = match bail {
        Some(b) => g.add(Op::Merge, IrType::Control, vec![exit, b], None),
        None => exit,
    };
    g.exit = g.add(Op::Return, IrType::Void, vec![out], Some(2));
    let sched = ir_schedule::schedule(&g);
    (g, sched, load, off_load)
}

fn proved(s: Shape) -> bool {
    let (g, sched, load, _) = decode_like(s);
    let e = analyze(&g, &sched);
    e.bounds_elided(load) && e.bounds_range_proved(load)
}

/// nio21 wave 4: is the `sa[sp + j]` load proven?
fn offset_proved(s: Shape) -> bool {
    let (g, sched, _, off) = decode_like(s);
    let off = off.expect("the shape has an offset load");
    let e = analyze(&g, &sched);
    e.bounds_elided(off) && e.bounds_range_proved(off)
}

#[test]
fn nio21w4_an_offset_inside_the_room_test_is_proven() {
    // `sl - sp < 2` false: `sa[sp + 1]`.
    assert!(offset_proved(Shape {
        offset: Some(1),
        ..BASE
    }));
    // `sl - sp < 4` false: `sa[sp + 3]`.
    assert!(offset_proved(Shape {
        room: Some(4),
        step: 4,
        offset: Some(3),
        ..BASE
    }));
}

#[test]
fn nio21w4_an_offset_at_the_room_keeps_its_check() {
    // `sl - sp >= 2` admits `sp + 2 == sl`.
    assert!(!offset_proved(Shape {
        offset: Some(2),
        ..BASE
    }));
}

#[test]
fn nio21w4_an_offset_needs_the_length_guard() {
    assert!(!offset_proved(Shape {
        offset: Some(1),
        length_guard: false,
        ..BASE
    }));
}

#[test]
fn nio21w4_an_offset_under_another_bound_needs_it_non_negative() {
    // `dl - sp >= 2` with no `0 <= dl` guard and no `dl <= sa.length`:
    // the length guard is on `sl`, so even `0 <= dl` does not prove it.
    let other = Shape {
        offset: Some(1),
        room_on_other: true,
        ..BASE
    };
    assert!(!offset_proved(other));
    assert!(!offset_proved(Shape {
        other_guard: true,
        ..other
    }));
}


#[test]
fn a_decode_loop_index_is_proven_under_its_entry_guards() {
    assert!(proved(BASE));
}

#[test]
fn a_wider_step_under_a_wide_enough_room_test_is_proven() {
    assert!(proved(Shape {
        room: Some(4),
        step: 4,
        ..BASE
    }));
}

#[test]
fn an_unknown_start_keeps_the_check() {
    assert!(!proved(Shape {
        init_guard: false,
        ..BASE
    }));
}

#[test]
fn a_multi_step_with_no_room_test_keeps_the_check() {
    assert!(!proved(Shape { room: None, ..BASE }));
}

#[test]
fn a_step_wider_than_the_room_test_keeps_the_check() {
    assert!(!proved(Shape { step: 3, ..BASE }));
}

#[test]
fn no_length_guard_keeps_the_check() {
    assert!(!proved(Shape {
        length_guard: false,
        ..BASE
    }));
}

#[test]
fn a_room_test_on_another_bound_needs_that_bound_non_negative() {
    let other = Shape {
        room_on_other: true,
        ..BASE
    };
    assert!(!proved(other));
    assert!(proved(Shape {
        other_guard: true,
        ..other
    }));
}


#[test]
fn constant_step_reads_a_chain_of_non_negative_constants() {
    let mut g = Graph::empty(0);
    let p = g.add(Op::Param(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let minus = g.add(Op::Const(-1), IrType::Int, vec![], None);
    let a = g.add(Op::Add, IrType::Int, vec![p, one], None);
    let b = g.add(Op::Add, IrType::Int, vec![one, a], None);
    let c = g.add(Op::Add, IrType::Int, vec![p, minus], None);
    assert_eq!(constant_step(&g, b), Some((p, 2)));
    assert_eq!(constant_step(&g, c), None);
    assert_eq!(constant_step(&g, p), None);
}
