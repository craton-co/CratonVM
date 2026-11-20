use super::super::{detect_loops, find_bypassable_loop_headers};
use super::*;

/// How a fixture run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Return(i32),
    Void,
    /// Exception kind, and the ORIGINAL bci that threw.
    Throw(&'static str, usize),
    StepLimit,
}

/// One reference run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Trace {
    /// `(original bci, locals)` before every executed instruction.
    steps: Vec<(usize, Vec<i32>)>,
    /// Execution PC of each step, in the same order. Only a VERSIONED run
    /// needs it: a guard's bytes carry the header's bci, so `steps` alone
    /// cannot tell a guard from the header, and filtering the guard out by
    /// bci would delete the header's own steps too.
    step_pcs: Vec<usize>,
    outcome: Outcome,
    heap: Vec<i32>,
}

/// An exception-handler body is not reachable along ordinary control flow,
/// so nothing inside it is — including the merge point of a branch the
/// handler body makes to itself. This is the whole content of the
/// handler-body-merge refusal: the emitter's branch-target map said `true`
/// for that merge, and "some instruction branches here" is not
/// "control can reach here".
#[test]
fn a_handler_bodys_own_branch_targets_are_not_reachable() {
    //  0: iload_0
    //  1: ireturn
    //  2: iload_0           <- handler body (exception-table target)
    //  3: aload_1
    //  4: ifnonnull 11
    //  7: iconst_0
    //  8: goto 12
    // 11: iconst_1
    // 12: iadd
    // 13: ireturn
    let code = [
        0x1a, 0xac, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0xac,
    ];
    let r = bytecode_analysis::reachable_pcs(&code, code.len(), &[])
        .expect("statically known control flow");
    assert_eq!(&r[..2], &[true, true], "the live prefix is reachable");
    assert!(
        r[2..code.len()].iter().all(|&b| !b),
        "nothing after the `ireturn` is reachable: {r:?}"
    );

    // The emitter's own question answers differently for the two merges,
    // which is exactly why it could not be used for this.
    let targets = bytecode_analysis::branch_target_map(&code, code.len());
    assert!(targets[11] && targets[12]);
}

/// Both edges of a conditional, and the fall-through of everything that
/// has one, are reachable; a `goto` has no fall-through.
#[test]
fn reachability_follows_both_edges_and_stops_at_a_goto() {
    //  0: iload_0
    //  1: ifeq 7
    //  4: goto 8
    //  7: iconst_1          (reached only by the `ifeq`)
    //  8: ireturn
    let code = [0x1a, 0x99, 0x00, 0x06, 0xa7, 0x00, 0x04, 0x04, 0xac];
    let r = bytecode_analysis::reachable_pcs(&code, code.len(), &[])
        .expect("statically known control flow");
    assert_eq!(
        &r[..code.len()],
        &[true, true, false, false, true, false, false, true, true],
        "only instruction boundaries on a real path are marked"
    );
}

/// Every arm of a switch is an edge, and the switch itself has no
/// fall-through.
#[test]
fn reachability_follows_every_switch_arm() {
    //  0: iconst_0
    //  1: nop; nop          (align the tableswitch operands to 4)
    //  3: tableswitch { 0: +21 (24), 1: +23 (26), default: +25 (28) }
    // 24: iconst_1; ireturn
    // 26: iconst_2; ireturn
    // 28: iconst_3; ireturn
    let mut code: Vec<u8> = vec![0x03, 0x00, 0x00, 0xaa];
    code.extend_from_slice(&25i32.to_be_bytes()); // default -> 28
    code.extend_from_slice(&0i32.to_be_bytes()); // low
    code.extend_from_slice(&1i32.to_be_bytes()); // high
    code.extend_from_slice(&21i32.to_be_bytes()); // case 0 -> 24
    code.extend_from_slice(&23i32.to_be_bytes()); // case 1 -> 26
    code.extend_from_slice(&[0x04, 0xac, 0x05, 0xac, 0x06, 0xac]);
    assert_eq!(code.len(), 30);
    let r = bytecode_analysis::reachable_pcs(&code, code.len(), &[])
        .expect("statically known control flow");
    for pc in [0usize, 1, 2, 3, 24, 25, 26, 27, 28, 29] {
        assert!(r[pc], "pc {pc} is on a real path");
    }
    for pc in 4..24usize {
        assert!(!r[pc], "pc {pc} is switch payload, not an instruction");
    }
}

/// `jsr`/`ret` have no statically known successor set. The analysis refuses
/// rather than under-approximating reachability, because an
/// under-approximation here deletes live code.
#[test]
fn opaque_control_flow_refuses_rather_than_guessing() {
    let jsr = [0xa8, 0x00, 0x03, 0xac]; // jsr +3; ireturn
    assert!(bytecode_analysis::reachable_pcs(&jsr, jsr.len(), &[]).is_none());
    let ret = [0xa9, 0x01]; // ret 1
    assert!(bytecode_analysis::reachable_pcs(&ret, ret.len(), &[]).is_none());
}

/// Target PC of the 2-byte-offset branch at `pc`, or its fall-through.
fn branch_to(code: &[u8], pc: usize, taken: bool) -> usize {
    if taken {
        let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
        (pc as isize + off) as usize
    } else {
        pc + 3
    }
}

/// Reference interpreter for the bytecode subset the fixtures use.
///
/// `bci_of` maps an execution PC to the ORIGINAL bci (identity for an
/// untransformed run) so two runs can be compared step for step. Local 0
/// doubles as the fixtures' array reference: `aload_0` pushes a handle for
/// the single `heap` array.
fn interp(
    code: &[u8],
    code_len: usize,
    bci_of: Option<&[u32]>,
    locals_in: &[i32],
    heap_in: &[i32],
) -> Trace {
    let mut locals = locals_in.to_vec();
    let mut heap = heap_in.to_vec();
    let mut stack: Vec<i32> = Vec::new();
    let mut steps: Vec<(usize, Vec<i32>)> = Vec::new();
    let mut step_pcs: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    let mut budget = 200_000usize;
    loop {
        if budget == 0 {
            return Trace {
                steps,
                step_pcs,
                outcome: Outcome::StepLimit,
                heap,
            };
        }
        budget -= 1;
        assert!(pc < code_len, "control ran off the end at pc {pc}");
        let bci = match bci_of {
            Some(m) => m[pc] as usize,
            None => pc,
        };
        steps.push((bci, locals.clone()));
        step_pcs.push(pc);
        let op = code[pc];
        match op {
            // nop
            0x00 => pc += 1,
            // iconst_m1 .. iconst_5
            0x02..=0x08 => {
                stack.push(op as i32 - 3);
                pc += 1;
            }
            // bipush
            0x10 => {
                stack.push(code[pc + 1] as i8 as i32);
                pc += 2;
            }
            // sipush
            0x11 => {
                stack.push(i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32);
                pc += 3;
            }
            // iload
            0x15 => {
                stack.push(locals[code[pc + 1] as usize]);
                pc += 2;
            }
            // iload_0 .. iload_3
            0x1a..=0x1d => {
                stack.push(locals[(op - 0x1a) as usize]);
                pc += 1;
            }
            // aload_0 — the fixtures' single array reference
            0x2a => {
                stack.push(0);
                pc += 1;
            }
            // iaload
            0x2e => {
                let idx = stack.pop().expect("iaload index");
                let _aref = stack.pop().expect("iaload arrayref");
                if idx < 0 || idx as usize >= heap.len() {
                    return Trace {
                        steps,
                        step_pcs,
                        outcome: Outcome::Throw("ArrayIndexOutOfBounds", bci),
                        heap,
                    };
                }
                stack.push(heap[idx as usize]);
                pc += 1;
            }
            // istore
            0x36 => {
                let v = stack.pop().expect("istore value");
                locals[code[pc + 1] as usize] = v;
                pc += 2;
            }
            // istore_0 .. istore_3
            0x3b..=0x3e => {
                let v = stack.pop().expect("istore_n value");
                locals[(op - 0x3b) as usize] = v;
                pc += 1;
            }
            // iastore
            0x4f => {
                let v = stack.pop().expect("iastore value");
                let idx = stack.pop().expect("iastore index");
                let _aref = stack.pop().expect("iastore arrayref");
                if idx < 0 || idx as usize >= heap.len() {
                    return Trace {
                        steps,
                        step_pcs,
                        outcome: Outcome::Throw("ArrayIndexOutOfBounds", bci),
                        heap,
                    };
                }
                heap[idx as usize] = v;
                pc += 1;
            }
            // iadd / isub / imul
            0x60 | 0x64 | 0x68 => {
                let b = stack.pop().expect("binop rhs");
                let a = stack.pop().expect("binop lhs");
                stack.push(match op {
                    0x60 => a.wrapping_add(b),
                    0x64 => a.wrapping_sub(b),
                    _ => a.wrapping_mul(b),
                });
                pc += 1;
            }
            // idiv
            0x6c => {
                let b = stack.pop().expect("idiv rhs");
                let a = stack.pop().expect("idiv lhs");
                if b == 0 {
                    return Trace {
                        steps,
                        step_pcs,
                        outcome: Outcome::Throw("ArithmeticException", bci),
                        heap,
                    };
                }
                stack.push(a.wrapping_div(b));
                pc += 1;
            }
            // iinc
            0x84 => {
                let l = code[pc + 1] as usize;
                locals[l] = locals[l].wrapping_add(code[pc + 2] as i8 as i32);
                pc += 3;
            }
            // ifeq .. ifle
            0x99..=0x9e => {
                let v = stack.pop().expect("if<cond> operand");
                let taken = match op {
                    0x99 => v == 0,
                    0x9a => v != 0,
                    0x9b => v < 0,
                    0x9c => v >= 0,
                    0x9d => v > 0,
                    _ => v <= 0,
                };
                pc = branch_to(code, pc, taken);
            }
            // if_icmpeq .. if_icmple
            0x9f..=0xa4 => {
                let b = stack.pop().expect("if_icmp rhs");
                let a = stack.pop().expect("if_icmp lhs");
                let taken = match op {
                    0x9f => a == b,
                    0xa0 => a != b,
                    0xa1 => a < b,
                    0xa2 => a >= b,
                    0xa3 => a > b,
                    _ => a <= b,
                };
                pc = branch_to(code, pc, taken);
            }
            // goto
            0xa7 => pc = branch_to(code, pc, true),
            // ireturn
            0xac => {
                let v = stack.pop().expect("ireturn value");
                return Trace {
                    steps,
                    step_pcs,
                    outcome: Outcome::Return(v),
                    heap,
                };
            }
            // return
            0xb1 => {
                return Trace {
                    steps,
                    step_pcs,
                    outcome: Outcome::Void,
                    heap,
                }
            }
            _ => panic!("fixture uses an opcode the reference interpreter lacks: {op:#04x}"),
        }
    }
}

/// The step sequence with the back-edge `goto` removed (see the section
/// comment: it has no data effect and cannot throw).
fn steps_without_back_edge(t: &Trace, back_edge: usize) -> Vec<(usize, Vec<i32>)> {
    steps_without_back_edge_or_guard(t, back_edge, None)
}

/// …and with a versioning guard's synthetic bytes removed as well.
///
/// The guard is filtered by output PC, not by bci: it carries the header's
/// bci so that provenance stays total, so filtering by bci would delete the
/// header's own steps. Removing it is sound for the same reason removing
/// the back edge is — it writes no local, cannot throw, and its only
/// observable effect is which version runs, which the caller asserts
/// separately.
fn steps_without_back_edge_or_guard(
    t: &Trace,
    back_edge: usize,
    guard: Option<(usize, usize)>,
) -> Vec<(usize, Vec<i32>)> {
    t.steps
        .iter()
        .zip(t.step_pcs.iter())
        .filter(|(s, pc)| {
            s.0 != back_edge && !matches!(guard, Some((from, to)) if **pc >= from && **pc < to)
        })
        .map(|(s, _)| s.clone())
        .collect()
}

/// How many times the back edge — hence the safepoint poll — executed.
fn poll_count(t: &Trace, back_edge: usize) -> usize {
    t.steps.iter().filter(|(b, _)| *b == back_edge).count()
}

/// Every PC whose instruction has a backward branch target.
fn backward_branch_pcs(code: &[u8], code_len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        targets.clear();
        assert!(
            bytecode_analysis::explicit_targets(code, code_len, pc, &mut targets),
            "opaque control flow at {pc}"
        );
        if targets.iter().any(|&t| t <= pc) {
            out.push(pc);
        }
        pc += bytecode_analysis::step(code, pc);
    }
    out
}

/// `int i = 0, sum = 0; while (i < n) { sum += i * 2; i++; } return sum;`
///
/// locals: 0 = n, 1 = i, 2 = sum. Header 4, back edge 18, length 23.
/// This is javac's condition-at-top `while` shape.
fn shape_a() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1          i = 0
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2          sum = 0
        0x1b, // 4: iload_1           <- header
        0x1a, // 5: iload_0
        0xa2, 0x00, 0x0f, // 6: if_icmpge 21
        0x1c, // 9: iload_2
        0x1b, // 10: iload_1
        0x05, // 11: iconst_2
        0x68, // 12: imul
        0x60, // 13: iadd
        0x3d, // 14: istore_2
        0x84, 0x01, 0x01, // 15: iinc 1, 1
        0xa7, 0xff, 0xf2, // 18: goto 4      <- back edge
        0x1c, // 21: iload_2
        0xac, // 22: ireturn
    ]
}

/// The pre-header-bypass shape: a branch from *before* the loop straight
/// into the header. This is `AttributesImpl.ensureCapacity`'s `goto` into
/// the `while` header — the witness the LICM pre-header bypass fix was
/// written for. Same loop as [`shape_a`], header 11, back edge 25,
/// length 30.
fn shape_b() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1          i = 0
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2          sum = 0
        0x1a, // 4: iload_0
        0x9a, 0x00, 0x06, // 5: ifne 11      <- external edge INTO the header
        0x03, // 8: iconst_0
        0x3d, // 9: istore_2
        0x00, // 10: nop
        0x1b, // 11: iload_1          <- header
        0x1a, // 12: iload_0
        0xa2, 0x00, 0x0f, // 13: if_icmpge 28
        0x1c, // 16: iload_2
        0x1b, // 17: iload_1
        0x05, // 18: iconst_2
        0x68, // 19: imul
        0x60, // 20: iadd
        0x3d, // 21: istore_2
        0x84, 0x01, 0x01, // 22: iinc 1, 1
        0xa7, 0xff, 0xf2, // 25: goto 11     <- back edge
        0x1c, // 28: iload_2
        0xac, // 29: ireturn
    ]
}

/// `for (i = 0; i < n; i++) a[i] = 100 / (i - 3);`
///
/// Throws ArithmeticException at `i == 3`, after three stores, and
/// ArrayIndexOutOfBounds past the array end — two different exceptions at
/// two different bcis, so a transform that reorders effects is caught.
/// locals: 0 = array, 1 = n, 2 = i. Header 2, back edge 19, length 23.
fn shape_throws() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2          i = 0
        0x1c, // 2: iload_2           <- header
        0x1b, // 3: iload_1
        0xa2, 0x00, 0x12, // 4: if_icmpge 22
        0x2a, // 7: aload_0
        0x1c, // 8: iload_2
        0x10, 0x64, // 9: bipush 100
        0x1c, // 11: iload_2
        0x06, // 12: iconst_3
        0x64, // 13: isub
        0x6c, // 14: idiv
        0x4f, // 15: iastore
        0x84, 0x02, 0x01, // 16: iinc 2, 1
        0xa7, 0xff, 0xef, // 19: goto 2       <- back edge
        0xb1, // 22: return
    ]
}

/// `do { i++; } while (i < n);` — the back edge is the exit test itself.
/// Header 2, back edge 7, length 12.
fn shape_do_while() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x84, 0x01, 0x01, // 2: iinc 1, 1     <- header
        0x1b, // 5: iload_1
        0x1a, // 6: iload_0
        0xa1, 0xff, 0xfb, // 7: if_icmplt 2   <- conditional back edge
        0x1b, // 10: iload_1
        0xac, // 11: ireturn
    ]
}

/// An irreducible loop: the cycle `L1 → L2 → L1` is entered at BOTH
/// blocks. Its back edge is a plain `goto`, so the transform gets past
/// every shape check and has to refuse on the entry/domination test.
/// Header 7, back edge 17, length 21.
fn shape_irreducible() -> Vec<u8> {
    vec![
        0x1a, // 0: iload_0
        0x99, 0x00, 0x0c, // 1: ifeq 13       -> enters the cycle at L2
        0xa7, 0x00, 0x03, // 4: goto 7        -> enters the cycle at L1
        0x84, 0x01, 0x01, // 7: L1: iinc 1, 1
        0xa7, 0x00, 0x03, // 10: goto 13
        0x1b, // 13: L2: iload_1
        0x99, 0x00, 0x06, // 14: ifeq 20
        0xa7, 0xff, 0xf6, // 17: goto 7       <- back edge
        0xb1, // 20: return
    ]
}

/// A reducible outer loop whose BODY contains an irreducible two-entry
/// cycle (`A → B → A`, entered at both). The outer header dominates its
/// whole region and there is no external entry, so only the inner
/// reducibility test can catch this. Header 2, back edge 30, length 35.
fn shape_irreducible_inner() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1            <- outer header
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x1d, // 4: if_icmpge 33
        0x1b, // 7: iload_1
        0x99, 0x00, 0x0c, // 8: ifeq 20        -> enters the inner cycle at B
        0xa7, 0x00, 0x03, // 11: goto 14       -> enters the inner cycle at A
        0x84, 0x01, 0x01, // 14: A: iinc 1, 1
        0xa7, 0x00, 0x03, // 17: goto 20
        0x1b, // 20: B: iload_1
        0x99, 0x00, 0x06, // 21: ifeq 27
        0xa7, 0xff, 0xf6, // 24: goto 14       <- inner back edge
        0x84, 0x01, 0x01, // 27: iinc 1, 1
        0xa7, 0xff, 0xe4, // 30: goto 2        <- outer back edge
        0x1b, // 33: iload_1
        0xac, // 34: ireturn
    ]
}

/// A one-instruction loop whose header is reached by a `goto_w`, which
/// the rewriter refuses as input. Header 0, back edge 5.
fn shape_with_goto_w() -> Vec<u8> {
    vec![
        0xc8, 0x00, 0x00, 0x00, 0x05, // 0: goto_w 5
        0xa7, 0xff, 0xfb, // 5: goto 0    <- back edge
    ]
}

#[test]
fn fixtures_have_the_loops_the_tests_assume() {
    assert_eq!(detect_loops(&shape_a(), 23), vec![(4, 18)]);
    assert_eq!(detect_loops(&shape_b(), 30), vec![(11, 25)]);
    assert_eq!(detect_loops(&shape_throws(), 23), vec![(2, 19)]);
    assert_eq!(detect_loops(&shape_do_while(), 12), vec![(2, 7)]);
    assert_eq!(detect_loops(&shape_irreducible(), 21), vec![(7, 17)]);
    assert_eq!(
        detect_loops(&shape_irreducible_inner(), 35),
        vec![(14, 24), (2, 30)]
    );
}

#[test]
fn a_transformed_loop_computes_the_same_result_on_every_trip_count() {
    for (code, header, back_edge) in [(shape_a(), 4usize, 18usize), (shape_b(), 11, 25)] {
        let len = code.len();
        for k in 1..=3usize {
            let peel = plan_loop_peel(&code, len, header, back_edge, k, &[])
                .expect("peel should be admitted");
            let unroll = plan_loop_unroll(&code, len, header, back_edge, k, &[])
                .expect("unroll should be admitted");
            // Trip counts 0 and 1 are the interesting ends: with 0 the
            // first copy's exit test fires before any body effect, and
            // with 1 the loop leaves from the middle of the copy group.
            for n in 0..=6i32 {
                let locals = [n, 0, 0];
                let base = interp(&code, len, None, &locals, &[]);
                assert_eq!(
                    base.outcome,
                    Outcome::Return(n * (n - 1)),
                    "fixture itself is wrong for n = {n}"
                );
                for x in [&peel, &unroll] {
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                    assert_eq!(t.outcome, base.outcome, "{:?} k = {k}, n = {n}", x.kind);
                    assert_eq!(
                        steps_without_back_edge(&t, back_edge),
                        steps_without_back_edge(&base, back_edge),
                        "{:?} k = {k}, n = {n}: executed sequence diverged",
                        x.kind
                    );
                }
            }
        }
    }
}

#[test]
fn every_transform_preserves_the_backedge_poll() {
    let code = shape_a();
    let len = code.len();
    // The emitter polls at a backward branch and nowhere else, and the
    // original loop's only backward branch is its back edge.
    assert!(emits_safepoint_poll_at(&code, 18, len));
    assert!(!emits_safepoint_poll_at(&code, 6, len));
    assert!(all_backward_edges_are_polled(&code, len));
    assert_eq!(backward_branch_pcs(&code, len), vec![18]);

    for k in 1..=3usize {
        for x in [
            plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
            plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
        ] {
            // Static half: every backward branch in the OUTPUT is at an
            // opcode the emitter polls, and there is exactly one — the
            // loop's own back edge. Since every cycle in a linear bytecode
            // CFG contains a backward branch, every cycle is polled.
            assert!(all_backward_edges_are_polled(&x.code, x.code_len));
            assert_eq!(
                backward_branch_pcs(&x.code, x.code_len),
                vec![x.back_edge_pc()],
                "{:?} k = {k}",
                x.kind
            );
            assert!(emits_safepoint_poll_at(
                &x.code,
                x.back_edge_pc(),
                x.code_len
            ));

            // Time-to-safepoint: the polled cycle is one body long for
            // peel (unchanged), k + 1 bodies for unroll (bounded).
            let cycle = x.back_edge_pc() - x.steady_state_base();
            match x.kind {
                LoopXformKind::Peel => assert_eq!(cycle, 14),
                LoopXformKind::Unroll => assert_eq!(cycle, 14 * (k + 1)),
            }
            assert_eq!(x.poll_free_bytes, 14 * (k + 1));
            assert!(x.poll_free_bytes <= LOOP_XFORM_MAX_POLL_FREE_BYTES);

            // Dynamic half: count the polls actually executed. Peeling
            // moves k iterations out of the loop, so it drops exactly k
            // polls, once, and the steady state still polls per iteration.
            // Unrolling polls once per group, so the poll count falls by
            // at most the factor — never to zero for a long-running loop,
            // which is what "time to safepoint stays bounded" means.
            for n in 0..=9i32 {
                let locals = [n, 0, 0];
                let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                let polls = poll_count(&t, 18);
                // Widening: loop trip count is non-negative here
                let trips = n as usize;
                match x.kind {
                    LoopXformKind::Peel => assert_eq!(polls, trips.saturating_sub(k)),
                    LoopXformKind::Unroll => assert_eq!(polls, trips / (k + 1)),
                }
                assert!(
                    trips <= polls * (k + 1) + 2 * (k + 1),
                    "{:?} k = {k}, n = {n}: {trips} iterations for {polls} polls",
                    x.kind
                );
            }
        }
    }
}

#[test]
fn a_backward_goto_w_is_polled_but_still_refused_by_the_rewriter() {
    // Since round 9 `jit_scan` admits `goto_w` and `walk_control` polls a
    // backward one, so the model counts it as polled.
    let spin = vec![0xc8, 0x00, 0x00, 0x00, 0x00, 0xb1];
    assert!(all_backward_edges_are_polled(&spin, spin.len()));
    assert!(poll_bearing_opcode(0xc8));
    // The model is still not vacuous: a backward `jsr_w` (never polled)
    // is refused.
    let jsr_w_spin = vec![0xc9, 0x00, 0x00, 0x00, 0x00, 0xb1];
    assert!(!all_backward_edges_are_polled(
        &jsr_w_spin,
        jsr_w_spin.len()
    ));
    assert!(!poll_bearing_opcode(0xc9));

    // A method containing one is refused outright rather than duplicated.
    let code = shape_with_goto_w();
    assert_eq!(
        plan_loop_unroll(&code, code.len(), 0, 5, 1, &[]),
        Err(LoopXformRefusal::OpaqueControlFlow)
    );
}

#[test]
fn an_irreducible_loop_is_refused() {
    let code = shape_irreducible();
    let len = code.len();
    // The second entry into the cycle IS the branch that breaks the
    // header's domination of its own region, so the single-entry scan
    // names it first. Both are refusals; neither transforms.
    assert_eq!(
        plan_loop_unroll(&code, len, 7, 17, 1, &[]),
        Err(LoopXformRefusal::ExternalEntry)
    );
    assert_eq!(
        plan_loop_peel(&code, len, 7, 17, 1, &[]),
        Err(LoopXformRefusal::ExternalEntry)
    );
    // …and the dominator machinery agrees on why: the header does not
    // dominate the other entry block.
    let cfg = bytecode_analysis::InsnCfg::build(&code, len).expect("cfg builds");
    let h = cfg.node_of(7).expect("header node");
    let l2 = cfg.node_of(13).expect("L2 node");
    assert!(!cfg.dominates(h, l2));
    // A reducible loop's header does dominate its region.
    let a = shape_a();
    let acfg = bytecode_analysis::InsnCfg::build(&a, a.len()).expect("cfg builds");
    let ah = acfg.node_of(4).expect("header node");
    for &pc in acfg.nodes() {
        if (4..21).contains(&pc) {
            let n = acfg.node_of(pc).expect("region node");
            assert!(acfg.dominates(ah, n), "header should dominate {pc}");
        }
    }
}

#[test]
fn an_irreducible_inner_cycle_is_refused() {
    let code = shape_irreducible_inner();
    let len = code.len();
    // The outer loop is single-entry and its header dominates the whole
    // region, so only the inner reducibility test can reject this.
    let cfg = bytecode_analysis::InsnCfg::build(&code, len).expect("cfg builds");
    let h = cfg.node_of(2).expect("outer header");
    for &pc in cfg.nodes() {
        if (2..33).contains(&pc) {
            let n = cfg.node_of(pc).expect("region node");
            assert!(cfg.dominates(h, n), "outer header should dominate {pc}");
        }
    }
    assert_eq!(
        plan_loop_unroll(&code, len, 2, 30, 1, &[]),
        Err(LoopXformRefusal::IrreducibleInnerLoop)
    );
    assert_eq!(
        plan_loop_peel(&code, len, 2, 30, 2, &[]),
        Err(LoopXformRefusal::IrreducibleInnerLoop)
    );
}

#[test]
fn exception_order_is_unchanged() {
    let code = shape_throws();
    let len = code.len();
    let heap = vec![0i32; 6];
    // The fixture throws where we think it does, after the stores that
    // precede it.
    let base = interp(&code, len, None, &[0, 6, 0], &heap);
    assert_eq!(base.outcome, Outcome::Throw("ArithmeticException", 14));
    assert_eq!(base.heap, vec![-33, -50, -100, 0, 0, 0]);

    for k in 1..=3usize {
        for x in [
            plan_loop_peel(&code, len, 2, 19, k, &[]).expect("peel"),
            plan_loop_unroll(&code, len, 2, 19, k, &[]).expect("unroll"),
        ] {
            // n = 0..3 completes; n >= 4 throws ArithmeticException at
            // i == 3; n > 6 would throw ArrayIndexOutOfBounds first if the
            // divide were reordered past the store.
            for n in 0..=9i32 {
                let locals = [0, n, 0];
                let base = interp(&code, len, None, &locals, &heap);
                let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &heap);
                assert_eq!(t.outcome, base.outcome, "{:?} k = {k}, n = {n}", x.kind);
                assert_eq!(t.heap, base.heap, "{:?} k = {k}, n = {n}", x.kind);
                assert_eq!(
                    steps_without_back_edge(&t, 19),
                    steps_without_back_edge(&base, 19),
                    "{:?} k = {k}, n = {n}",
                    x.kind
                );
            }
        }
    }
}

#[test]
fn deopt_into_a_transformed_loop_resolves_its_locals() {
    let code = shape_a();
    let len = code.len();
    let orig_starts = bytecode_analysis::instruction_starts(&code, len);
    for k in 1..=3usize {
        for x in [
            plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
            plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
        ] {
            // Provenance is TOTAL: every output byte names an original
            // bci, so no output PC can deopt into a hole.
            assert!(x.provenance_is_total());
            let out_starts = bytecode_analysis::instruction_starts(&x.code, x.code_len);
            for (p, &is_start) in out_starts.iter().enumerate() {
                if !is_start {
                    continue;
                }
                let bci = x.bci_at(p).expect("provenance for an instruction start");
                assert!(
                    orig_starts[bci],
                    "{:?} k = {k}: output pc {p} maps to {bci}, not an instruction start",
                    x.kind
                );
            }
            // …and the frame at that bci is the frame the interpreter
            // would have had: the whole (bci, locals) sequence matches.
            for n in 0..=5i32 {
                let locals = [n, 0, 0];
                let base = interp(&code, len, None, &locals, &[]);
                let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                assert_eq!(
                    steps_without_back_edge(&t, 18),
                    steps_without_back_edge(&base, 18),
                    "{:?} k = {k}, n = {n}",
                    x.kind
                );
            }
        }
    }
}

#[test]
fn osr_entry_targets_the_steady_state_copy() {
    let code = shape_a();
    let len = code.len();
    let peel = plan_loop_peel(&code, len, 4, 18, 2, &[]).expect("peel");
    // A bci inside the body has k + 1 images. Entering a PEELED copy from
    // OSR would re-run the peeled iterations, so the answer must be the
    // last copy — the one the back edge re-enters.
    assert_eq!(peel.steady_state_base(), 4 + 2 * 14);
    assert_eq!(peel.osr_entry_pc(4), Some(4 + 2 * 14));
    assert_eq!(peel.osr_entry_pc(9), Some(9 + 2 * 14));
    assert_eq!(peel.osr_entry_pc(0), Some(0));
    assert_eq!(peel.osr_entry_pc(21), Some(21 + 2 * 14));
    assert_eq!(peel.osr_entry_pc(23), None);

    // For unroll every copy is a full iteration, so the first one — the
    // back edge's target — is the right entry.
    let unroll = plan_loop_unroll(&code, len, 4, 18, 2, &[]).expect("unroll");
    assert_eq!(unroll.steady_state_base(), 4);
    assert_eq!(unroll.osr_entry_pc(4), Some(4));
    assert_eq!(unroll.osr_entry_pc(9), Some(9));
    assert_eq!(unroll.osr_entry_pc(21), Some(21 + 2 * 14));

    // Round trip: the OSR entry for a bci carries that bci.
    for x in [&peel, &unroll] {
        for bci in [0usize, 4, 9, 15, 21] {
            let p = x.osr_entry_pc(bci).expect("osr entry");
            assert_eq!(x.bci_at(p), Some(bci), "{:?} bci {bci}", x.kind);
        }
    }
}

#[test]
fn peeling_removes_the_preheader_bypass_from_the_steady_state_loop() {
    let code = shape_b();
    let len = code.len();
    // The original is exactly the shape the pre-header bypass guard
    // rejects: a branch from outside lands on the header, after the
    // pre-header the emitter would have placed there.
    let loops = detect_loops(&code, len);
    assert!(find_bypassable_loop_headers(&code, len, &loops, &[]).contains(&11));

    // Peeling moves the external entry onto the PEELED copy, so the
    // steady-state loop has no entry but its own back edge and the
    // fall-through: a hoist the guard had to drop is legal again. At every
    // factor the planner can ask for, not just one.
    for k in 1..=3usize {
        let peel = plan_loop_peel(&code, len, 11, 25, k, &[]).expect("peel");
        let ploops = detect_loops(&peel.code, peel.code_len);
        assert_eq!(
            ploops,
            vec![(peel.steady_state_base(), peel.back_edge_pc())],
            "k={k}"
        );
        assert_eq!(peel.steady_state_base(), 11 + k * 14, "k={k}");
        assert!(
            find_bypassable_loop_headers(&peel.code, peel.code_len, &ploops, &[]).is_empty(),
            "k={k}"
        );
    }

    // Unrolling does NOT: its first copy IS the header, so the external
    // edge still lands on the loop. Stated so nobody assumes otherwise.
    let unroll = plan_loop_unroll(&code, len, 11, 25, 1, &[]).expect("unroll");
    let uloops = detect_loops(&unroll.code, unroll.code_len);
    assert!(
        find_bypassable_loop_headers(&unroll.code, unroll.code_len, &uloops, &[]).contains(&11)
    );
}

#[test]
fn exception_ranges_are_refused_or_widened() {
    let code = shape_a();
    let len = code.len();
    // A handler inside the region would need duplicating with it.
    assert_eq!(
        plan_loop_peel(&code, len, 4, 18, 1, &[(9, 15, 12)]),
        Err(LoopXformRefusal::HandlerInRegion)
    );
    // A protected range that covers only part of the loop cannot be
    // mapped onto the copies.
    assert_eq!(
        plan_loop_peel(&code, len, 4, 18, 1, &[(0, 10, 22)]),
        Err(LoopXformRefusal::HandlerRangeStraddlesRegion)
    );
    // A range that encloses the loop is widened to cover every copy.
    let x = plan_loop_peel(&code, len, 4, 18, 1, &[(0, 21, 21)]).expect("enclosing range");
    assert_eq!(x.exception_ranges, vec![(0, 35, 35)]);
    // A range entirely before the loop does not move.
    let y = plan_loop_peel(&code, len, 4, 18, 1, &[(0, 4, 2)]).expect("range before the loop");
    assert_eq!(y.exception_ranges, vec![(0, 4, 2)]);
}

#[test]
fn shapes_the_rewriter_refuses_to_guess_at() {
    let code = shape_a();
    let len = code.len();
    // A `do { } while` back edge is the exit test: duplicating the body
    // without it would run the body k + 1 times per test.
    let dw = shape_do_while();
    assert_eq!(
        plan_loop_unroll(&dw, dw.len(), 2, 7, 1, &[]),
        Err(LoopXformRefusal::ConditionalBackEdge)
    );
    // Copy-count bounds are what keeps time-to-safepoint bounded.
    assert_eq!(
        plan_loop_unroll(&code, len, 4, 18, 0, &[]),
        Err(LoopXformRefusal::TooManyCopies)
    );
    assert_eq!(
        plan_loop_unroll(&code, len, 4, 18, LOOP_XFORM_MAX_COPIES + 1, &[]),
        Err(LoopXformRefusal::TooManyCopies)
    );
    // The pair must actually be a loop.
    assert_eq!(
        plan_loop_peel(&code, len, 4, 6, 1, &[]),
        Err(LoopXformRefusal::ConditionalBackEdge)
    );
    assert_eq!(
        plan_loop_peel(&code, len, 9, 18, 1, &[]),
        Err(LoopXformRefusal::NotABackEdge)
    );
}

#[test]
fn the_rewrite_is_layout_consistent() {
    let code = shape_a();
    let len = code.len();
    for k in 1..=LOOP_XFORM_MAX_COPIES {
        for x in [
            plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
            plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
        ] {
            assert_eq!(x.code_len, len + k * 14);
            assert_eq!(x.code.len(), x.code_len);
            assert_eq!(x.back_edge_pc(), 4 + (k + 1) * 14);
            assert_eq!(x.code[x.back_edge_pc()], 0xa7);
            // The prefix never moves, so the header PC is stable and the
            // copies are byte-identical apart from their branch offsets.
            assert_eq!(&x.code[..4], &code[..4]);
            for ci in 0..=k {
                let base = 4 + ci * 14;
                assert_eq!(x.code[base], 0x1b, "copy {ci} must start at the header");
                assert_eq!(x.bci_at(base), Some(4));
            }
            // Every copy's exit branch leaves to the (shifted) exit.
            for ci in 0..=k {
                let at = 4 + ci * 14 + 2;
                let off = i16::from_be_bytes([x.code[at + 1], x.code[at + 2]]) as isize;
                assert_eq!(at as isize + off, (21 + k * 14) as isize);
            }
        }
    }
}

// ── Switch fixtures ──────────────────────────────────────────────
//
// Four positions for one `lookupswitch` (zero pairs, so it is 8 operand
// bytes plus its padding), chosen to separate the two PC-relative facts
// precondition 6 is about: the padding, which depends on the switch's own
// PC, and the 4-byte jump offsets, which this rewriter never re-encodes.
// The reference interpreter does not implement `lookupswitch`, so these
// are used by the STATIC tests only — the step-sequence equivalence tests
// keep running on the switch-free shapes.

/// A `lookupswitch` before the loop whose only target is the loop header.
/// Neither the switch nor its target moves, so the rewrite leaves it
/// byte-identical and it is admitted. Header 12, back edge 26, length 31 —
/// the same 14-byte body as [`shape_a`].
fn shape_switch_before_loop() -> Vec<u8> {
    vec![
        0xab, // 0: lookupswitch
        0x00, 0x00, 0x00, // 1: padding to the 4-byte boundary
        0x00, 0x00, 0x00, 0x0c, // 4: default = +12 -> 12 (the header)
        0x00, 0x00, 0x00, 0x00, // 8: npairs = 0
        0x1b, // 12: iload_1          <- header
        0x1a, // 13: iload_0
        0xa2, 0x00, 0x0f, // 14: if_icmpge 29
        0x1c, // 17: iload_2
        0x1b, // 18: iload_1
        0x05, // 19: iconst_2
        0x68, // 20: imul
        0x60, // 21: iadd
        0x3d, // 22: istore_2
        0x84, 0x01, 0x01, // 23: iinc 1, 1
        0xa7, 0xff, 0xf2, // 26: goto 12     <- back edge
        0x1c, // 29: iload_2
        0xac, // 30: ireturn
    ]
}

/// A `lookupswitch` AFTER the loop, at a PC that is already 4-byte aligned
/// (`switch_pad(19) == 0`). It shifts by `delta = k * 14`, so its padding
/// survives only when `k` is even — the case the old "no switch anywhere"
/// rule could not distinguish. Header 2, back edge 16, length 29.
fn shape_switch_after_loop() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1           <- header
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x0f, // 4: if_icmpge 19
        0x84, 0x01, 0x01, // 7: iinc 1, 1
        0x84, 0x01, 0x01, // 10: iinc 1, 1
        0x00, // 13: nop
        0x00, // 14: nop
        0x00, // 15: nop
        0xa7, 0xff, 0xf2, // 16: goto 2      <- back edge
        0xab, // 19: lookupswitch (no padding — 20 is already aligned)
        0x00, 0x00, 0x00, 0x09, // 20: default = +9 -> 28
        0x00, 0x00, 0x00, 0x00, // 24: npairs = 0
        0xb1, // 28: return
    ]
}

/// A `lookupswitch` INSIDE the loop body. Every copy would sit at a
/// different PC, so the copies do not agree on their padding, and each
/// copy's targets would have to be relocated in a 4-byte field the
/// rewriter does not touch. Header 2, back edge 19, length 23.
fn shape_switch_in_body() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1           <- header
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x12, // 4: if_icmpge 22
        0xab, // 7: lookupswitch (no padding — 8 is already aligned)
        0x00, 0x00, 0x00, 0x09, // 8: default = +9 -> 16
        0x00, 0x00, 0x00, 0x00, // 12: npairs = 0
        0x84, 0x01, 0x01, // 16: iinc 1, 1
        0xa7, 0xff, 0xef, // 19: goto 2       <- back edge
        0xb1, // 22: return
    ]
}

/// A `lookupswitch` before the loop that branches ACROSS it. The switch
/// does not move but its target does, so its un-rewritten 4-byte offset
/// would name the wrong instruction. Header 16, back edge 27, length 32.
/// Everything else about this method is admissible, which is what makes it
/// a test of the displacement rule specifically.
fn shape_switch_over_the_loop() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1a, // 2: iload_0
        0x99, 0x00, 0x0d, // 3: ifeq 16      -> the header
        0xab, // 6: lookupswitch
        0x00, // 7: padding to the 4-byte boundary
        0x00, 0x00, 0x00, 0x18, // 8: default = +24 -> 30 (past the loop)
        0x00, 0x00, 0x00, 0x00, // 12: npairs = 0
        0x1b, // 16: iload_1          <- header
        0x1a, // 17: iload_0
        0xa2, 0x00, 0x0c, // 18: if_icmpge 30
        0x84, 0x01, 0x01, // 21: iinc 1, 1
        0x00, // 24: nop
        0x00, // 25: nop
        0x00, // 26: nop
        0xa7, 0xff, 0xf5, // 27: goto 16     <- back edge
        0x1b, // 30: iload_1
        0xac, // 31: ireturn
    ]
}

/// Byte offset of the low byte of `shape_switch_over_the_loop`'s default
/// offset, so a test can retarget it without re-spelling the fixture.
const SWITCH_OVER_DEFAULT_LOW_BYTE: usize = 11;

/// Every fixture the rewriter is expected to admit, as
/// `(name, code, header, back_edge)`.
fn admissible_fixtures() -> Vec<(&'static str, Vec<u8>, usize, usize)> {
    vec![
        ("shape_a", shape_a(), 4, 18),
        ("shape_b", shape_b(), 11, 25),
        ("shape_throws", shape_throws(), 2, 19),
        (
            "shape_switch_before_loop",
            shape_switch_before_loop(),
            12,
            26,
        ),
        // Admitted at even `k` only — see the fixture's doc comment.
        ("shape_switch_after_loop", shape_switch_after_loop(), 2, 16),
    ]
}

/// The hand-assembled switch fixtures decode the way every test below
/// assumes: the walk lands exactly on the end, the switch is where and as
/// long as claimed, its targets are the claimed ones, and the method is a
/// buildable CFG with one loop and every cycle polled. Without this, a
/// mis-typed padding byte would silently turn a refusal test into a test
/// of a malformed method.
#[test]
fn the_switch_fixtures_decode_the_way_these_tests_assume() {
    for (name, code, header, back_edge, sw, sw_len, sw_targets) in [
        (
            "before",
            shape_switch_before_loop(),
            12usize,
            26usize,
            0usize,
            12usize,
            vec![12usize],
        ),
        ("after", shape_switch_after_loop(), 2, 16, 19, 9, vec![28]),
        ("in body", shape_switch_in_body(), 2, 19, 7, 9, vec![16]),
        (
            "over",
            shape_switch_over_the_loop(),
            16,
            27,
            6,
            10,
            vec![30],
        ),
    ] {
        let len = code.len();
        let starts = bytecode_analysis::instruction_starts(&code, len);
        let mut pc = 0usize;
        while pc < len {
            assert!(starts[pc], "{name}: {pc} is not an instruction start");
            let l = bytecode_analysis::step(&code, pc);
            assert!(l > 0, "{name}: zero-length instruction at {pc}");
            pc += l;
        }
        assert_eq!(pc, len, "{name}: the walk overran the fixture");

        assert!(matches!(code[sw], 0xaa | 0xab), "{name}: no switch at {sw}");
        assert_eq!(
            bytecode_analysis::step(&code, sw),
            sw_len,
            "{name}: switch length"
        );
        let mut t: Vec<usize> = Vec::new();
        assert!(
            bytecode_analysis::explicit_targets(&code, len, sw, &mut t),
            "{name}: switch does not decode"
        );
        assert_eq!(t, sw_targets, "{name}: switch targets");

        assert_eq!(
            detect_loops(&code, len),
            vec![(header, back_edge)],
            "{name}"
        );
        assert_eq!(code[back_edge], 0xa7, "{name}: back edge is not a goto");
        assert!(
            bytecode_analysis::InsnCfg::build(&code, len).is_some(),
            "{name}: cfg builds"
        );
        assert!(all_backward_edges_are_polled(&code, len), "{name}");
    }
}

/// `bci_at` is TOTAL, and what it answers is a real instruction of the
/// original method.
///
/// This is the safety property the whole rewrite rests on. Every deopt
/// bci, every oop-map `bytecode_pc` and every exception range recorded by
/// a compiled method has to translate through this map; a pc it cannot
/// answer for, or answers with a byte that is not an instruction start,
/// is a frame the interpreter cannot resume and an oop-map the GC cannot
/// match. Proved here over every admissible fixture, both kinds and every
/// legal factor, not just `shape_a` at `k <= 3`.
#[test]
fn bci_at_is_total_over_every_fixture_and_factor() {
    let mut admitted_total = 0usize;
    for (name, code, header, back_edge) in admissible_fixtures() {
        let len = code.len();
        let orig_starts = bytecode_analysis::instruction_starts(&code, len);
        let mut admitted_here = 0usize;
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for planned in [
                plan_loop_peel(&code, len, header, back_edge, k, &[]),
                plan_loop_unroll(&code, len, header, back_edge, k, &[]),
            ] {
                // A refusal publishes no map, so there is nothing for the
                // property to hold over. `shape_switch_after_loop` refuses
                // at odd `k`; see
                // `a_switch_outside_the_rewritten_region_no_longer_refuses`.
                let x = match planned {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                admitted_here += 1;
                admitted_total += 1;
                let what = format!("{name} {:?} k={k}", x.kind);

                // 1. The map covers the output byte for byte.
                assert!(x.provenance_is_total(), "{what}");
                assert_eq!(x.code.len(), x.code_len, "{what}");
                assert_eq!(x.bci_of.len(), x.code_len, "{what}");

                // 2. THE PROPERTY: every output pc that begins an
                //    instruction resolves, and resolves to a bci that
                //    begins an instruction in the ORIGINAL.
                let out_starts = bytecode_analysis::instruction_starts(&x.code, x.code_len);
                for (p, &is_start) in out_starts.iter().enumerate() {
                    if !is_start {
                        continue;
                    }
                    assert!(
                        x.bci_at(p).is_some(),
                        "{what}: no provenance for instruction start {p}"
                    );
                    let bci = x.bci_at(p).unwrap_or(usize::MAX);
                    assert!(bci < len, "{what}: pc {p} maps to {bci}, past the original");
                    assert!(
                        orig_starts[bci],
                        "{what}: pc {p} maps to {bci}, which is mid-instruction"
                    );
                }

                // 3. …and the instruction found there is the SAME
                //    instruction: same opcode, same length, and every
                //    interior byte maps to the matching interior byte. So
                //    a pc that is NOT an instruction start cannot resolve
                //    to a plausible-looking bci of some other instruction.
                let mut pc = 0usize;
                while pc < x.code_len {
                    let bci = x.bci_at(pc).unwrap_or(usize::MAX);
                    assert!(bci < len, "{what}: pc {pc} has no provenance");
                    assert_eq!(x.code[pc], code[bci], "{what}: pc {pc} vs bci {bci}");
                    let l = bytecode_analysis::step(&x.code, pc);
                    assert!(l > 0, "{what}: zero-length instruction at {pc}");
                    assert_eq!(
                        l,
                        bytecode_analysis::step(&code, bci),
                        "{what}: pc {pc} and bci {bci} disagree on length"
                    );
                    for d in 0..l {
                        assert_eq!(x.bci_at(pc + d), Some(bci + d), "{what}: pc {pc} + {d}");
                    }
                    pc += l;
                }
                assert_eq!(pc, x.code_len, "{what}: the output walk overran");

                // 4. The poll proof, re-checked on this output too — every
                //    fixture and factor, not just `shape_a`.
                assert!(all_backward_edges_are_polled(&x.code, x.code_len), "{what}");
                assert!(
                    emits_safepoint_poll_at(&x.code, x.back_edge_pc(), x.code_len),
                    "{what}: the back edge lost its poll"
                );

                // 5. `osr_entry_pc` is a partial INVERSE of `bci_at`
                //    wherever it answers at all — including at bytes that
                //    are not instruction starts.
                for bci in 0..len {
                    if let Some(entry) = x.osr_entry_pc(bci) {
                        assert!(
                            entry < x.code_len,
                            "{what}: osr {bci} -> {entry}, past the end"
                        );
                        assert_eq!(x.bci_at(entry), Some(bci), "{what}: osr {bci} -> {entry}");
                    }
                }
            }
        }
        assert!(
            admitted_here > 0,
            "{name}: never admitted, so the property held vacuously"
        );
    }
    // Three fixtures are admitted for both kinds at every factor, so the
    // property was exercised, not skipped.
    assert!(
        admitted_total >= 6 * LOOP_XFORM_MAX_COPIES,
        "only {admitted_total} plans checked"
    );
}

/// OSR must REFUSE the unrolled back-edge gap, not answer it.
///
/// The back-edge `goto` is emitted in the last copy only, and unroll's
/// steady state is copy 0, which stops just before it. The old answer,
/// `Some(bci)`, named output pc `header + body_len` — the first byte of
/// copy 1, i.e. the header — so an OSR entry there resumed a "back edge
/// next" frame at the top of a fresh body and ran an extra iteration.
/// `outputs_for_bci` is an exact inverse of `bci_at`, and one-to-many
/// inside the rewritten region.
///
/// This is the property a side-table replication pass rests on. If an
/// output PC were missing from its bci's image list, that bci's entry in
/// one of the 21 pc-keyed tables would not be replicated into that copy,
/// and the copy would silently lose a field resolution or an inline cache.
#[test]
fn outputs_for_bci_is_the_exact_inverse_of_bci_at() {
    for (name, code, header, back_edge) in admissible_fixtures() {
        let len = code.len();
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for planned in [
                plan_loop_peel(&code, len, header, back_edge, k, &[]),
                plan_loop_unroll(&code, len, header, back_edge, k, &[]),
            ] {
                let Ok(x) = planned else { continue };
                // Forward then back.
                for pc in 0..x.code.len() {
                    let Some(bci) = x.bci_at(pc) else { continue };
                    assert!(
                        x.outputs_for_bci(bci).contains(&pc),
                        "{name} k={k}: output {pc} maps to bci {bci} but is missing \
                         from its image list"
                    );
                }
                // Back then forward.
                for bci in 0..len {
                    for pc in x.outputs_for_bci(bci) {
                        assert_eq!(x.bci_at(pc), Some(bci), "{name} k={k}");
                    }
                }
                // The header survives, so it has at least one image.
                assert!(
                    !x.outputs_for_bci(header).is_empty(),
                    "{name} k={k}: the header must have an image"
                );
            }
        }
    }
}
/// Every entry of a replicated table lands on a PC that maps back to the
/// entry's original bci, and a body entry appears once per copy.
///
/// The count property is the one that matters: a copy missing its entry
/// is silent — it loses a field resolution or an inline cache and takes a
/// different lowering path than its siblings, with nothing to fail on.
#[test]
fn a_replicated_side_table_covers_every_copy() {
    for (name, code, header, back_edge) in admissible_fixtures() {
        let len = code.len();
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for planned in [
                plan_loop_peel(&code, len, header, back_edge, k, &[]),
                plan_loop_unroll(&code, len, header, back_edge, k, &[]),
            ] {
                let Ok(x) = planned else { continue };
                // One entry per original instruction start, payload = its bci.
                let table: Vec<(usize, usize)> = (0..len)
                    .filter(|&pc| x.bci_at(pc).is_some() || pc < len)
                    .map(|pc| (pc, pc))
                    .collect();
                let rep = x.replicate_pc_keyed(&table);

                // Sorted, as every consumer assumes.
                assert!(
                    rep.windows(2).all(|w| w[0].0 <= w[1].0),
                    "{name} k={k}: replicated table is not sorted by pc"
                );

                // Every replicated entry sits on a PC that maps back to it.
                for (pc, orig) in &rep {
                    assert_eq!(
                        x.bci_at(*pc),
                        Some(*orig),
                        "{name} k={k}: entry moved to {pc}, which is not an image of {orig}"
                    );
                }

                // A body entry appears exactly once per image — the count
                // property a missed replication would break.
                for (pc, _) in &table {
                    let images = x.outputs_for_bci(*pc).len();
                    let landed = rep.iter().filter(|(_, o)| o == pc).count();
                    assert_eq!(
                        landed, images,
                        "{name} k={k}: bci {pc} has {images} images but {landed} entries"
                    );
                }
            }
        }
    }
}

/// The one-pass inverse index `replicate_pc_keyed` now builds answers
/// EXACTLY what the per-entry `outputs_for_bci` scan it replaced answered:
/// the same images, in the same order, for every bci including ones past
/// the method's end (which have none).
#[test]
fn the_inverse_index_is_outputs_for_bci_for_every_bci() {
    for (name, code, header, back_edge) in admissible_fixtures() {
        let len = code.len();
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for planned in [
                plan_loop_peel(&code, len, header, back_edge, k, &[]),
                plan_loop_unroll(&code, len, header, back_edge, k, &[]),
            ] {
                let Ok(x) = planned else { continue };
                let index = x.images_by_bci();
                for bci in 0..len + 3 {
                    let want = x.outputs_for_bci(bci);
                    let got = index.get(bci).cloned().unwrap_or_default();
                    assert_eq!(got, want, "{name} k={k}: bci {bci}");
                }
            }
        }
    }
}
/// The rebuilt `pc_to_native` sends every original bci to a native offset
/// belonging to the image OSR would enter, and marks the unrolled
/// back-edge gap unmapped.
///
/// Picking any other image would re-run iterations, which is the exact bug
/// `osr_entry_pc` was fixed for; this asserts the rebuild inherits that
/// choice rather than making its own.
#[test]
fn rebuilt_pc_to_native_follows_the_osr_entry_image() {
    for (name, code, header, back_edge) in admissible_fixtures() {
        let len = code.len();
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for planned in [
                plan_loop_peel(&code, len, header, back_edge, k, &[]),
                plan_loop_unroll(&code, len, header, back_edge, k, &[]),
            ] {
                let Ok(x) = planned else { continue };
                // Synthetic: native offset = output pc * 4, so a wrong
                // image is visible as a wrong number rather than a crash.
                let out: Vec<i32> = (0..x.code.len()).map(|pc| (pc as i32) * 4).collect();
                let rebuilt = x.rebuild_pc_to_native(&out, len);
                assert_eq!(rebuilt.len(), len + 1, "{name} k={k}: length");
                for bci in 0..=len {
                    match x.osr_entry_pc(bci) {
                        Some(image) if image < out.len() => assert_eq!(
                            rebuilt[bci], out[image],
                            "{name} k={k}: bci {bci} must use image {image}"
                        ),
                        _ => assert_eq!(
                            rebuilt[bci], -1,
                            "{name} k={k}: bci {bci} has no valid entry image and \
                             must stay unmapped"
                        ),
                    }
                }
                // The unrolled back-edge gap is unmapped, not mapped to
                // the header — entering there would re-run iterations.
                if matches!(x.kind, LoopXformKind::Unroll) {
                    assert_eq!(
                        rebuilt[x.orig_back_edge_pc()],
                        -1,
                        "{name} k={k}: the unrolled back-edge gap must be unmapped"
                    );
                }
            }
        }
    }
}
#[test]
fn osr_refuses_the_unrolled_back_edge_gap_and_answers_the_steady_state_elsewhere() {
    let code = shape_a();
    let len = code.len();
    let (header, back_edge, body_len) = (4usize, 18usize, 14usize);
    let back_edge_end = back_edge + 3;
    for k in 1..=LOOP_XFORM_MAX_COPIES {
        let peel = plan_loop_peel(&code, len, header, back_edge, k, &[]).expect("peel");
        let unroll = plan_loop_unroll(&code, len, header, back_edge, k, &[]).expect("unroll");
        assert_eq!(unroll.orig_back_edge_pc(), back_edge, "k={k}");
        assert_eq!(peel.orig_back_edge_pc(), back_edge, "k={k}");

        for bci in back_edge..back_edge_end {
            assert_eq!(unroll.osr_entry_pc(bci), None, "k={k} bci={bci}");
            // Why: the pc the old code answered carries a DIFFERENT
            // instruction's provenance.
            assert_ne!(unroll.bci_at(bci), Some(bci), "k={k} bci={bci}");
            // Peel's steady-state copy carries the back edge, so it keeps
            // an entry there and that entry round-trips.
            let p = peel.osr_entry_pc(bci).expect("peel has no gap");
            assert_eq!(p, bci + k * body_len, "k={k} bci={bci}");
            assert_eq!(peel.bci_at(p), Some(bci), "k={k} bci={bci}");
        }
        assert_eq!(unroll.bci_at(back_edge), Some(header), "k={k}");
        assert_eq!(peel.bci_at(peel.back_edge_pc()), Some(back_edge), "k={k}");

        // The gap is the ONLY refusal inside the method …
        for bci in 0..len {
            if unroll.osr_entry_pc(bci).is_none() {
                assert!(
                    (back_edge..back_edge_end).contains(&bci),
                    "k={k}: bci {bci} refused outside the back-edge gap"
                );
            }
            assert!(
                peel.osr_entry_pc(bci).is_some(),
                "k={k}: peel refused {bci}"
            );
        }
        // … and everywhere else the answer is the steady-state copy,
        // exactly as before this fix.
        for bci in header..back_edge {
            assert_eq!(unroll.osr_entry_pc(bci), Some(bci), "k={k} bci={bci}");
            assert_eq!(
                peel.osr_entry_pc(bci),
                Some(bci + k * body_len),
                "k={k} bci={bci}"
            );
            let entry = unroll.osr_entry_pc(bci).unwrap_or(usize::MAX);
            assert!(
                entry >= unroll.steady_state_base()
                    && entry < unroll.steady_state_base() + body_len,
                "k={k} bci={bci}: {entry} is outside unroll's steady-state copy"
            );
        }
        assert_eq!(unroll.osr_entry_pc(0), Some(0), "k={k}");
        assert_eq!(peel.osr_entry_pc(0), Some(0), "k={k}");
        assert_eq!(unroll.osr_entry_pc(21), Some(21 + k * body_len), "k={k}");
        assert_eq!(unroll.osr_entry_pc(len), None, "k={k}");
    }
}

/// `SwitchInMethod` is now a per-switch question, not a per-method one: a
/// switch the rewrite does not disturb is admitted. See precondition 6.
#[test]
fn a_switch_outside_the_rewritten_region_no_longer_refuses() {
    // (a) Before the loop, branching to the header. Neither the switch nor
    // its target moves, so its bytes are still a faithful encoding.
    let before = shape_switch_before_loop();
    let blen = before.len();
    assert_eq!(switch_pad(0), 3);
    for k in 1..=3usize {
        for x in [
            plan_loop_peel(&before, blen, 12, 26, k, &[]).expect("switch before the loop"),
            plan_loop_unroll(&before, blen, 12, 26, k, &[]).expect("switch before the loop"),
        ] {
            assert_eq!(&x.code[..12], &before[..12], "{:?} k={k}", x.kind);
            assert_eq!(
                bytecode_analysis::step(&x.code, 0),
                12,
                "{:?} k={k}",
                x.kind
            );
            let mut t: Vec<usize> = Vec::new();
            assert!(bytecode_analysis::explicit_targets(
                &x.code, x.code_len, 0, &mut t
            ));
            assert_eq!(
                t,
                vec![12],
                "{:?} k={k}: the switch must still enter the header",
                x.kind
            );
            assert_eq!(x.bci_at(12), Some(12), "{:?} k={k}", x.kind);
            assert!(all_backward_edges_are_polled(&x.code, x.code_len));
        }
    }

    // (b) After the loop it moves by `delta = 14k`, so it survives exactly
    // when the padding RECOMPUTED at the shifted pc is unchanged — which
    // for this fixture means even `k`. This is the case a "does it move at
    // all" rule cannot decide and the reason the check recomputes padding.
    let after = shape_switch_after_loop();
    let alen = after.len();
    assert_eq!(switch_pad(19), 0);
    for k in 1..=4usize {
        let planned = plan_loop_unroll(&after, alen, 2, 16, k, &[]);
        if k % 2 == 0 {
            let x = planned.expect("even k preserves the padding");
            let sw = 19 + k * 14;
            assert_eq!(switch_pad(sw), switch_pad(19), "k={k}");
            assert_eq!(x.code[sw], 0xab, "k={k}");
            assert_eq!(bytecode_analysis::step(&x.code, sw), 9, "k={k}");
            assert_eq!(x.bci_at(sw), Some(19), "k={k}");
            let mut t: Vec<usize> = Vec::new();
            assert!(bytecode_analysis::explicit_targets(
                &x.code, x.code_len, sw, &mut t
            ));
            assert_eq!(
                t,
                vec![28 + k * 14],
                "k={k}: the default must follow the shift"
            );
            // The same verdict for peel — the rule is kind-independent.
            assert!(plan_loop_peel(&after, alen, 2, 16, k, &[]).is_ok(), "k={k}");
        } else {
            assert_eq!(
                planned.unwrap_err(),
                LoopXformRefusal::SwitchInMethod,
                "k={k}: shifting by {} changes the padding, hence the length",
                14 * k
            );
            assert_eq!(
                plan_loop_peel(&after, alen, 2, 16, k, &[]).unwrap_err(),
                LoopXformRefusal::SwitchInMethod,
                "k={k}"
            );
        }
    }

    // (c) Inside the region it is duplicated: the copies do not agree on
    // their padding and their targets would need relocating in a field
    // this rewriter does not touch. Still refused.
    let inside = shape_switch_in_body();
    let ilen = inside.len();
    for k in 1..=3usize {
        assert_eq!(
            plan_loop_peel(&inside, ilen, 2, 19, k, &[]).unwrap_err(),
            LoopXformRefusal::SwitchInMethod,
            "k={k}"
        );
        assert_eq!(
            plan_loop_unroll(&inside, ilen, 2, 19, k, &[]).unwrap_err(),
            LoopXformRefusal::SwitchInMethod,
            "k={k}"
        );
    }

    // (d) Outside the region but branching ACROSS it: the switch does not
    // move and its target does, so its 4-byte offset would name the wrong
    // instruction. Refused — and the SAME fixture with the default
    // retargeted at the header (which does not move) is admitted, so the
    // displacement is demonstrably the only thing being refused.
    let over = shape_switch_over_the_loop();
    let olen = over.len();
    for k in 1..=3usize {
        assert_eq!(
            plan_loop_unroll(&over, olen, 16, 27, k, &[]).unwrap_err(),
            LoopXformRefusal::SwitchInMethod,
            "k={k}"
        );
    }
    let mut retargeted = over.clone();
    retargeted[SWITCH_OVER_DEFAULT_LOW_BYTE] = 0x0a; // +24 (past the loop) -> +10 (the header)
    let mut t: Vec<usize> = Vec::new();
    assert!(bytecode_analysis::explicit_targets(
        &retargeted,
        olen,
        6,
        &mut t
    ));
    assert_eq!(t, vec![16], "the retarget must name the header");
    for k in 1..=3usize {
        let x = plan_loop_unroll(&retargeted, olen, 16, 27, k, &[])
            .expect("a switch that branches to the header is admitted");
        assert_eq!(&x.code[..16], &retargeted[..16], "k={k}");
        assert!(all_backward_edges_are_polled(&x.code, x.code_len), "k={k}");
    }
}

// ── Guarded versioning ───────────────────────────────────────────────
//
// The transform is described in `LoopVersioning`. These tests carry its
// four load-bearing claims: the two versions compute the same thing, the
// guard decides which one runs and nothing else, the guard is inert enough
// for its provenance to be the header's, and both loops still poll.

use crate::scev::SymBound;

/// `trip >= minimum` on [`shape_a`]'s runtime limit (`n`, local 0) — the
/// exact shape `CountedLoop::prove_trip_count_at_least` mints for
/// `for (i = 0; i < n; i++)`, which is the commonest loop in Java and the
/// one whose compile-time `trip.min` is zero.
fn trip_guard(minimum: u64) -> PreheaderGuard {
    PreheaderGuard::TripCountAtLeast {
        term: SymBound {
            base: BoundTerm::Bound(BoundSource::Local(0)),
            addend: 0,
        },
        minimum,
    }
}

#[test]
fn a_versioned_loop_runs_the_same_steps_whichever_version_it_takes() {
    let code = shape_a();
    let (len, header, back_edge, body_len) = (23usize, 4usize, 18usize, 14usize);
    for k in 1..=3usize {
        for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
            // The minimum the planner asks for: enough trips to reach every
            // copy the transform makes.
            let guard = trip_guard(k as u64 + 1);
            let x = plan_loop_version(&code, len, header, back_edge, k, &[], kind, &guard)
                .expect("versioning is admitted");
            let v = x.versioning.as_ref().expect("versioned");
            // Layout: prefix, guard, k + 1 copies, the untouched fallback,
            // suffix.
            assert_eq!(v.guard_pc, header);
            assert_eq!(v.guard_len, 5, "iload_0; iconst_<k+1>; if_icmplt");
            assert_eq!(x.fast_base(), header + 5);
            assert_eq!(v.fallback_base, header + 5 + (k + 1) * body_len + 3);
            assert_eq!(x.code_len, len + 5 + k * body_len + body_len + 3);
            assert!(x.provenance_is_total());

            // Trip counts either side of the guard's minimum, so both
            // versions are exercised by this comparison.
            for n in 0..=8i32 {
                let locals = [n, 0, 0];
                let base = interp(&code, len, None, &locals, &[]);
                assert_eq!(base.outcome, Outcome::Return(n * (n - 1)), "fixture, n={n}");
                let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                assert_eq!(t.outcome, base.outcome, "{kind:?} k={k} n={n}");
                assert_eq!(
                    steps_without_back_edge_or_guard(&t, back_edge, x.guard_span()),
                    steps_without_back_edge(&base, back_edge),
                    "{kind:?} k={k} n={n}: executed sequence diverged"
                );
            }
        }
    }
}

#[test]
fn the_guard_takes_the_fast_version_exactly_when_it_holds() {
    let code = shape_a();
    let (len, header, back_edge) = (23usize, 4usize, 18usize);
    let k = 3usize;
    let x = plan_loop_version(
        &code,
        len,
        header,
        back_edge,
        k,
        &[],
        LoopXformKind::Unroll,
        &trip_guard(4),
    )
    .expect("versioning is admitted");
    let fallback = x.versioning.as_ref().expect("versioned").fallback_base;
    let fallback_end = fallback + x.body_len + 3;
    for n in 0..=8i32 {
        let t = interp(&x.code, x.code_len, Some(&x.bci_of), &[n, 0, 0], &[]);
        let ran_fast = t
            .step_pcs
            .iter()
            .any(|&pc| pc >= x.fast_base() && pc < fallback);
        let ran_fallback = t
            .step_pcs
            .iter()
            .any(|&pc| pc >= fallback && pc < fallback_end);
        // `n >= 4` IS the guard, and it is the only thing that decides
        // which body runs. Without this the test above would pass just as
        // happily if the guard were never emitted at all.
        assert_eq!(ran_fast, n >= 4, "n={n}: wrong version for the guard");
        assert_eq!(ran_fallback, n < 4, "n={n}: wrong version for the guard");
        assert!(ran_fast != ran_fallback, "n={n}: both versions ran");
    }
}

#[test]
fn a_versioning_guard_writes_nothing_and_balances_the_stack() {
    // This is the property that makes the guard's provenance — the header's
    // bci — sound. Nothing it emits can throw, allocate, call, poll or
    // write a local, so no deopt point, exception-check stub, bounds-check
    // stub or `athrow` can name a guard PC, and the operand stack at the
    // guard's first byte is the stack the interpreter has at the header.
    for guard in [
        trip_guard(4),
        PreheaderGuard::NonNegative(SymBound {
            base: BoundTerm::IvEntry(1),
            addend: 0,
        }),
        PreheaderGuard::AtMost {
            term: SymBound {
                base: BoundTerm::Bound(BoundSource::Local(2)),
                addend: -1,
            },
            limit: 1000,
        },
        PreheaderGuard::AtLeast {
            term: SymBound {
                base: BoundTerm::Bound(BoundSource::Local(9)),
                addend: 7,
            },
            limit: -50,
        },
    ] {
        let bytes = encode_preheader_guard(&guard).expect("encodable");
        // Ends in exactly one conditional branch, with the placeholder
        // offset `rewrite_loop_copies` patches.
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0], "{guard:?}");
        let branch = bytes[bytes.len() - 3];
        assert!(
            matches!(branch, 0xa1 | 0xa3),
            "{guard:?}: branch opcode {branch:#04x}"
        );
        let mut pc = 0usize;
        let mut depth = 0i32;
        let mut branches = 0usize;
        while pc < bytes.len() {
            let op = bytes[pc];
            assert!(
                matches!(op, 0x02..=0x08 | 0x10 | 0x11 | 0x15 | 0x1a..=0x1d | 0xa1 | 0xa3),
                "{guard:?}: emitted {op:#04x}, which is not in the inert set"
            );
            depth += match op {
                0xa1 | 0xa3 => {
                    branches += 1;
                    -2
                }
                _ => 1,
            };
            pc += bytecode_analysis::step(&bytes, pc);
        }
        assert_eq!(
            pc,
            bytes.len(),
            "{guard:?}: the guard does not walk exactly"
        );
        assert_eq!(depth, 0, "{guard:?}: the guard is not stack-balanced");
        assert_eq!(branches, 1, "{guard:?}: one fallback edge, no more");
        // No backward branch, so no poll is owed and none is dropped.
        assert!(all_backward_edges_are_polled(
            &bytes[..bytes.len() - 3],
            pc - 3
        ));
    }
}

#[test]
fn the_guard_encoder_refuses_everything_it_cannot_prove_inert() {
    use LoopXformRefusal as R;
    let local = |l: usize| SymBound {
        base: BoundTerm::Bound(BoundSource::Local(l)),
        addend: 0,
    };
    let code = shape_a();
    for (guard, want, why) in [
        // Would need `aload; arraylength` — a NullPointerException at a PC
        // whose provenance is the loop header, i.e. a throw the original
        // method does not have at that bci.
        (
            PreheaderGuard::LengthAtLeast(local(0)),
            R::GuardNotEncodable,
            "length guard",
        ),
        (
            PreheaderGuard::NonNegative(SymBound {
                base: BoundTerm::Bound(BoundSource::ArrayLength(0)),
                addend: 0,
            }),
            R::GuardNotEncodable,
            "arraylength term",
        ),
        // A field read adds resolution and class initialisation too.
        (
            PreheaderGuard::NonNegative(SymBound {
                base: BoundTerm::Bound(BoundSource::Field {
                    cp_index: 3,
                    receiver_local: None,                    }),
                addend: 0,
            }),
            R::GuardNotEncodable,
            "field term",
        ),
        // Two comparisons would need two fallback edges.
        (
            PreheaderGuard::StrideInRange {
                local: 1,
                headroom: local(0),
            },
            R::GuardNotEncodable,
            "stride range",
        ),
        // `wide iload`.
        (
            PreheaderGuard::NonNegative(local(256)),
            R::GuardNotEncodable,
            "wide local index",
        ),
        // Needs `ldc`, hence a constant-pool entry this rewriter cannot
        // mint: it rewrites bytes, it does not own the class.
        (
            PreheaderGuard::AtLeast {
                term: local(0),
                limit: 100_000,
            },
            R::GuardNotEncodable,
            "threshold past sipush",
        ),
        // Compile-time verdicts, in both directions.
        (
            PreheaderGuard::NonNegative(SymBound {
                base: BoundTerm::Const(7),
                addend: 0,
            }),
            R::GuardIsConstant,
            "constant term",
        ),
        (
            PreheaderGuard::AtLeast {
                term: local(0),
                limit: i32::MIN,
            },
            R::GuardIsConstant,
            "no int can fail it",
        ),
        (
            PreheaderGuard::TripCountAtLeast {
                term: local(0),
                minimum: u32::MAX as u64,
            },
            R::GuardIsConstant,
            "no int can pass it",
        ),
    ] {
        assert_eq!(encode_preheader_guard(&guard), Err(want), "{why}");
        // …and the transform refuses with the same reason rather than
        // emitting a fast path nothing guards.
        assert_eq!(
            plan_loop_version(&code, 23, 4, 18, 1, &[], LoopXformKind::Unroll, &guard),
            Err(want),
            "{why}"
        );
    }
    // Positive control: the same call site IS admitted with a guard the
    // encoder can emit, so none of the refusals above is vacuous.
    assert!(plan_loop_version(
        &code,
        23,
        4,
        18,
        1,
        &[],
        LoopXformKind::Unroll,
        &trip_guard(2)
    )
    .is_ok());
}

#[test]
fn a_side_table_entry_is_never_replicated_onto_the_guard() {
    let code = shape_a();
    let x = plan_loop_version(
        &code,
        23,
        4,
        18,
        1,
        &[],
        LoopXformKind::Unroll,
        &trip_guard(2),
    )
    .expect("versioned");
    let (gfrom, gto) = x.guard_span().expect("versioned");
    let fallback = x.versioning.as_ref().expect("versioned").fallback_base;
    // The header's images are the two fast copies and the fallback — three
    // real instructions — and NOT the five synthetic guard bytes.
    let images = x.outputs_for_bci(4);
    assert_eq!(images, vec![x.fast_base(), x.fast_base() + 14, fallback]);
    assert!(images.iter().all(|&pc| pc < gfrom || pc >= gto));
    assert!(
        images.iter().all(|&pc| x.code[pc] == code[4]),
        "every image must really be the header's opcode"
    );
    // The guard's bytes still resolve to a bci — `Compiler::orig_bci` has
    // to be able to answer for every output PC — they are simply not
    // images.
    for pc in gfrom..gto {
        assert_eq!(x.bci_at(pc), Some(4));
    }
    assert!(x.provenance_is_total());
    // A pc-keyed table therefore lands one entry per real copy: two fast,
    // one fallback, plus the untouched suffix site.
    let lifted = x.replicate_pc_keyed(&[(4usize, 0xAAu8), (22, 0xBBu8)]);
    assert_eq!(lifted.len(), 4);
    assert!(lifted[..3].iter().all(|&(_, p)| p == 0xAA));
    assert_eq!(lifted[3], (22 + x.suffix_shift, 0xBB));
}

#[test]
fn osr_into_a_versioned_loop_lands_in_the_fallback_and_never_the_guard() {
    let code = shape_a();
    let (len, header, back_edge) = (23usize, 4usize, 18usize);
    for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
        for k in 1..=3usize {
            let x = plan_loop_version(
                &code,
                len,
                header,
                back_edge,
                k,
                &[],
                kind,
                &trip_guard(k as u64 + 1),
            )
            .expect("versioned");
            let v = x.versioning.clone().expect("versioned");
            // The header enters the FALLBACK, not the guard. Entering the
            // guard is correct bytecode and wrong machine code: the guard
            // is not a loop header, so it is not a pc the OSR trampoline
            // can reconstruct a compiled state for. This assertion is the
            // regression test for the null receiver that produced.
            assert_eq!(x.osr_entry_pc(header), Some(v.fallback_base));
            assert_ne!(x.osr_entry_pc(header), Some(v.guard_pc));
            // …and so does every other bci in the region: entering a
            // transformed copy would skip the guard, which is the whole
            // point of having one.
            let mut pc = header + bytecode_analysis::step(&code, header);
            while pc < back_edge + 3 {
                let entry = x.osr_entry_pc(pc).expect("bci is in range");
                assert_eq!(
                    entry,
                    v.fallback_base + (pc - header),
                    "{kind:?} k={k} bci={pc}"
                );
                assert_eq!(x.bci_at(entry), Some(pc));
                assert!(entry >= x.steady_state_base() && entry <= x.back_edge_pc());
                assert!(
                    entry >= v.fallback_base,
                    "{kind:?} k={k} bci={pc}: OSR entered a guarded copy"
                );
                pc += bytecode_analysis::step(&code, pc);
            }
            // No OSR entry anywhere in the method resolves into the guard.
            let (gfrom, gto) = x.guard_span().expect("versioned");
            for bci in 0..len {
                if let Some(entry) = x.osr_entry_pc(bci) {
                    assert!(
                        entry < gfrom || entry >= gto,
                        "{kind:?} k={k} bci={bci}: OSR entry {entry} is inside the guard"
                    );
                }
            }
            // Versioning has no back-edge gap whatever the fast side is:
            // the fallback is a full image of the region, back edge
            // included. Unroll on its own answers `None` here.
            assert_eq!(x.osr_entry_pc(back_edge), Some(x.back_edge_pc()));
            // Outside the region, unchanged apart from the shift.
            assert_eq!(x.osr_entry_pc(0), Some(0));
            assert_eq!(x.osr_entry_pc(22), Some(22 + x.suffix_shift));
            assert_eq!(x.osr_entry_pc(len), None);
            // The published vector is one slot per interpreter bci, with no
            // refusal anywhere in the region.
            let synthetic: Vec<i32> = (0..=x.code_len as i32).collect();
            let rebuilt = x.rebuild_pc_to_native(&synthetic, len);
            assert_eq!(rebuilt.len(), len + 1);
            assert!(
                rebuilt[header..=back_edge].iter().all(|&n| n >= 0),
                "{kind:?} k={k}"
            );
        }
    }
}

#[test]
fn both_versions_keep_a_back_edge_poll() {
    let code = shape_a();
    let (len, body_len) = (23usize, 14usize);
    for k in 1..=3usize {
        for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
            let x =
                plan_loop_version(&code, len, 4, 18, k, &[], kind, &trip_guard(k as u64 + 1))
                    .expect("versioned");
            let v = x.versioning.as_ref().expect("versioned");
            assert!(all_backward_edges_are_polled(&x.code, x.code_len));
            // There are TWO loops now, and both are polled: the guarded one
            // and the fallback the failing edge reaches. Checking only the
            // first would leave the copy OSR enters unproven.
            assert_eq!(
                backward_branch_pcs(&x.code, x.code_len),
                vec![x.fast_back_edge_pc(), x.back_edge_pc()],
                "{kind:?} k={k}"
            );
            assert!(emits_safepoint_poll_at(
                &x.code,
                x.fast_back_edge_pc(),
                x.code_len
            ));
            assert!(emits_safepoint_poll_at(
                &x.code,
                x.back_edge_pc(),
                x.code_len
            ));
            assert_eq!(x.back_edge_pc(), v.fallback_base + body_len);
            assert!(x.poll_free_bytes <= LOOP_XFORM_MAX_POLL_FREE_BYTES);
            // Below the guard's minimum the fallback runs, and it polls
            // once per trip exactly like the original loop.
            for n in 0..=(k as i32) {
                let t = interp(&x.code, x.code_len, Some(&x.bci_of), &[n, 0, 0], &[]);
                // Widening: a non-negative trip count to usize
                assert_eq!(poll_count(&t, 18), n as usize, "{kind:?} k={k} n={n}");
            }
        }
    }
}

#[test]
fn an_enclosing_handler_range_covers_both_versions() {
    let code = shape_a();
    // A `try` that lexically encloses the loop, with its handler after it.
    let ranges = [(2usize, 21usize, 21usize)];
    let x = plan_loop_version(
        &code,
        23,
        4,
        18,
        1,
        &ranges,
        LoopXformKind::Unroll,
        &trip_guard(2),
    )
    .expect("versioned");
    let v = x.versioning.as_ref().expect("versioned");
    assert_eq!(x.exception_ranges.len(), 1);
    let (s, e, h) = x.exception_ranges[0];
    assert_eq!(s, 2, "the range still starts where it did");
    assert_eq!(e, 21 + x.suffix_shift);
    assert_eq!(h, 21 + x.suffix_shift);
    assert!(
        s <= v.guard_pc && e >= v.fallback_base + x.body_len + 3,
        "the widened range must cover the guard, every copy and the fallback"
    );
}

/// **Inline reference emission and an armed ZGC barrier are mutually
/// exclusive by construction**, and that is why there is no inline barrier
/// sequence in this backend.
///
/// `zgc-jit-load-barrier.md` scopes stage (a) as "colored slots +
/// interpreter/native barrier + x64 barrier emission (~6-7 instructions on
/// the fast path)". The first two landed on 2026-08-13. The third was
/// **closed by decision, not deferred**, and this test is the reason
/// written as an assertion:
///
/// `narrow_oops_block_inline_fields` — the predicate every inline
/// compact-field arm is gated on — returns `true` whenever the barrier is
/// armed. So the inline arm is not emitted at all in the only state where
/// a barrier would have anything to do. An inline sequence would therefore
/// execute exclusively with the barrier disarmed, where it is required to
/// be the identity transform (bad mask 0, address mask all-ones, heap base
/// 0) — six instructions of provable no-op on the hottest path in the VM.
///
/// Making it worthwhile means first REMOVING the helper routing, i.e.
/// trading a mechanism that is correct today for one that is not yet
/// validated. That is a throughput change and it needs a suite
/// measurement, which is the same gate every other default-on decision in
/// the maturity plan carries.
///
/// If someone lifts the ZGC clause out of
/// `narrow_oops_block_inline_fields`, this test fails — and at that moment
/// inline emission stops being dead code and starts being mandatory,
/// because `zgc_codegen_honours_read_barrier` would be lying.
#[test]
fn an_armed_zgc_barrier_and_inline_reference_emission_cannot_coexist() {
    // zt t22/x18: the flip below is process-wide.
    let _flip = super::zgc_read_gate_flip_test_lock();
    // Disarmed: inline emission is available (unless narrow oops, which is
    // the pre-existing clause and is not what this test is about).
    cratonvm_types::set_zgc_read_barrier_armed(false);
    let inline_blocked_when_disarmed = zgc_read_barrier_blocks_inline_fields();

    cratonvm_types::set_zgc_read_barrier_armed(true);
    let inline_blocked_when_armed = zgc_read_barrier_blocks_inline_fields();
    let gate_when_armed = narrow_oops_block_inline_fields();
    cratonvm_types::set_zgc_read_barrier_armed(false);

    assert!(
        !inline_blocked_when_disarmed,
        "a disarmed barrier must not cost the inline arms anything"
    );
    assert!(
        inline_blocked_when_armed,
        "an armed barrier must block inline reference emission"
    );
    assert!(
        gate_when_armed,
        "...and it must do so THROUGH the gate the inline arms actually \
         consult, or the block is decorative"
    );
    assert!(
        zgc_codegen_honours_read_barrier(),
        "the capability `zgc_relocation_permitted` trusts rests on exactly \
         the property asserted above"
    );
}
