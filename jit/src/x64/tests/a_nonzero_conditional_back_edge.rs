#[allow(unused_imports)]
use super::*;

/// Interpreter round i1 wave 13, lane L4, made exact in wave 14
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`): a CONDITIONAL back
/// edge — javac's `do { } while`, a loop rotated by ecj or kotlinc, a count
/// down tested against zero, a backward switch arm — polls with its operands
/// live, before the branch. An OSR-tier body leaves there through a map
/// recorded at the branch's own bci (the fused `sipush; if_icmp` at the
/// constant's bci), carrying the operands, so the interpreter re-executes the
/// branch. A zero verdict, or a method-entry compile given the
/// innermost-frame bit alone, runs the loop to completion (since wave 15 the
/// method-entry compile records the map too, and leaves on the every-frame
/// bit).
///
/// Checked for EXACTNESS, not plausibility: the helper answers non-zero from
/// a chosen call on, so the body must leave at that back-edge poll, with the
/// counter that poll saw, and poll no more. Wave 13 asserted `1..1000` and
/// read 0 on Linux: the ROTATED shape's bottom test runs once before the first
/// body iteration, so 0 is exactly what its first poll sees
/// (`interpreter-L0-conditional-back-edge-exit-reconstructs-a-stale-counter-RETIRED-20260925.md`).
#[test]
pub(in crate::x64::tests) fn a_nonzero_conditional_back_edge_verdict_leaves_an_osr_body_at_its_branch() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    static FLAG: u8 = 1;
    // Slow-path calls made so far, and the 0-based call from which the helper
    // answers "leave" (`i64::MAX`: never).
    static CALLS: AtomicI64 = AtomicI64::new(0);
    static LEAVE_FROM: AtomicI64 = AtomicI64::new(i64::MAX);
    extern "C" fn verdict_poll(_flag_addr: i64) -> i64 {
        let n = CALLS.fetch_add(1, Ordering::SeqCst);
        i64::from(n >= LEAVE_FROM.load(Ordering::SeqCst))
    }
    // int i = 0; do { i++; } while (i < 1000); return i;
    #[rustfmt::skip]
    let do_while: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x84, 0x00, 0x01, // 2:  iinc 0, 1           <- loop header
        0x1a,             // 5:  iload_0
        0x11, 0x03, 0xe8, // 6:  sipush 1000         (fused with the branch)
        0xa1, 0xff, 0xf9, // 9:  if_icmplt -7 -> 2
        0x1a,             // 12: iload_0
        0xac,             // 13: ireturn
        0, 0,
    ];
    // int i = 0, n = 1000; goto COND; BODY: i++; COND: if (i < n) goto BODY;
    // return i;  -- the rotated shape, compared against a local.
    #[rustfmt::skip]
    let rotated: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x11, 0x03, 0xe8, // 2:  sipush 1000
        0x3c,             // 5:  istore_1
        0xa7, 0x00, 0x06, // 6:  goto +6 -> 12
        0x84, 0x00, 0x01, // 9:  iinc 0, 1           <- loop header
        0x1a,             // 12: iload_0
        0x1b,             // 13: iload_1
        0xa1, 0xff, 0xfb, // 14: if_icmplt -5 -> 9
        0x1a,             // 17: iload_0
        0xac,             // 18: ireturn
        0, 0,
    ];
    // int i = 1000; do { i--; } while (i != 0); return i;  -- one operand,
    // compared against zero.
    #[rustfmt::skip]
    let count_down: Vec<u8> = vec![
        0x11, 0x03, 0xe8, // 0:  sipush 1000
        0x3b,             // 3:  istore_0
        0x84, 0x00, 0xff, // 4:  iinc 0, -1          <- loop header
        0x1a,             // 7:  iload_0
        0x9a, 0xff, 0xfc, // 8:  ifne -4 -> 4
        0x1a,             // 11: iload_0
        0xac,             // 12: ireturn
        0, 0,
    ];
    // int i = 0; L: i++; switch (i) { case 1000: return i; default: goto L; }
    // -- the backward edge is a switch arm; the key is the one operand.
    #[rustfmt::skip]
    let switch_arm: Vec<u8> = vec![
        0x03,                   // 0:  iconst_0
        0x3b,                   // 1:  istore_0
        0x84, 0x00, 0x01,       // 2:  iinc 0, 1     <- loop header
        0x1a,                   // 5:  iload_0
        0xab, 0x00,             // 6:  lookupswitch (+1 pad)
        0xff, 0xff, 0xff, 0xfc, // 8:  default -> 2
        0x00, 0x00, 0x00, 0x01, // 12: npairs = 1
        0x00, 0x00, 0x03, 0xe8, // 16: key 1000
        0x00, 0x00, 0x00, 0x12, // 20:   -> 24
        0x1a,                   // 24: iload_0
        0xac,                   // 25: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = verdict_poll as *const () as usize; // Cast: helper address
    let build = |code: &[u8], code_len: usize, max_locals: usize, mode_exit_polls: bool| {
        compile_with_request(
            BackendRequest {
                mode_exit_polls,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            code,
            code_len,
            0,
            max_locals,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("test JIT compile")
    };
    struct Shape<'a> {
        name: &'a str,
        code: &'a [u8],
        code_len: usize,
        max_locals: usize,
        /// What a full run returns.
        result: i64,
        /// How many times a full run executes the polling branch.
        back_edge_polls: i64,
        /// The bcis the exit may name, with the operand count there: the
        /// fused `sipush; if_icmplt` exits at the constant's bci with one
        /// operand; unfused, at the branch with two.
        exits: &'a [(u32, usize)],
        /// The counter the `k`-th (1-based) back-edge poll sees.
        counter_at_poll: fn(i64) -> i64,
    }
    let shapes = [
        Shape {
            name: "do-while",
            code: &do_while,
            code_len: 14,
            max_locals: 1,
            result: 1000,
            back_edge_polls: 1000,
            exits: &[(6, 1), (9, 2)],
            counter_at_poll: |k| k,
        },
        // The bottom test runs once before the body does: the first poll
        // sees 0, which is the whole of what wave 13 read as "stale".
        Shape {
            name: "rotated",
            code: &rotated,
            code_len: 19,
            max_locals: 2,
            result: 1000,
            back_edge_polls: 1001,
            exits: &[(14, 2)],
            counter_at_poll: |k| k - 1,
        },
        Shape {
            name: "count-down",
            code: &count_down,
            code_len: 13,
            max_locals: 1,
            result: 0,
            back_edge_polls: 1000,
            exits: &[(8, 1)],
            counter_at_poll: |k| 1000 - k,
        },
        Shape {
            name: "switch-arm",
            code: &switch_arm,
            code_len: 26,
            max_locals: 1,
            result: 1000,
            back_edge_polls: 1000,
            exits: &[(6, 1)],
            counter_at_poll: |k| k,
        },
    ];
    for shape in &shapes {
        let name = shape.name;
        let osr_tier = build(shape.code, shape.code_len, shape.max_locals, true);
        let method_entry = build(shape.code, shape.code_len, shape.max_locals, false);
        assert!(
            shape
                .exits
                .iter()
                .any(|&(bci, _)| osr_tier.osr_exit_points.contains(&(bci as usize))),
            "{name}: the OSR-tier compile records a map at the branch: {:?}",
            osr_tier.osr_exit_points
        );
        // Wave 15: a method-entry compile records the branch map too; its
        // poll leaves only on the every-frame bit (see
        // `a_method_entry_body_leaves_at_its_nth_back_edge_poll_on_the_every_frame_verdict`).
        assert!(
            shape
                .exits
                .iter()
                .any(|&(bci, _)| method_entry.osr_exit_points.contains(&(bci as usize))),
            "{name}: a method-entry compile records the branch map: {:?}",
            method_entry.osr_exit_points
        );
        let _ = crate::deopt::take_last_deopt_with_point();

        // Answering 0 throughout: every poll runs its slow path, the loop
        // completes, and the call count says how many polls precede the loop
        // (the method-entry poll, when there is one).
        LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
        CALLS.store(0, Ordering::SeqCst);
        // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
        let full = unsafe { osr_tier.try_call(&[]) };
        assert_eq!(full, Ok(shape.result), "{name}: a zero verdict completes");
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        let before_loop = CALLS.load(Ordering::SeqCst) - shape.back_edge_polls;
        assert!(
            before_loop >= 0,
            "{name}: {} slow-path calls for {} back-edge polls",
            CALLS.load(Ordering::SeqCst),
            shape.back_edge_polls
        );

        // Answering 1 throughout, in a body not compiled for the OSR tier:
        // ignored.
        LEAVE_FROM.store(0, Ordering::SeqCst);
        CALLS.store(0, Ordering::SeqCst);
        // SAFETY: as above.
        let ignored = unsafe { method_entry.try_call(&[]) };
        LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
        assert_eq!(
            ignored,
            Ok(shape.result),
            "{name}: a method-entry body ignores it"
        );
        assert!(crate::deopt::take_last_deopt_with_point().is_none());

        // Answering 1 from the (`skipped` + 1)-th back-edge poll on: the
        // OSR-tier body leaves at exactly that poll, with exactly the state
        // that poll saw, and polls no more.
        for skipped in [0i64, 5] {
            let poll = skipped + 1;
            LEAVE_FROM.store(before_loop + skipped, Ordering::SeqCst);
            CALLS.store(0, Ordering::SeqCst);
            // SAFETY: as above.
            let raw = unsafe { osr_tier.try_call(&[]) };
            let calls = CALLS.load(Ordering::SeqCst);
            LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
            assert_eq!(
                raw,
                Ok(i64::MIN),
                "{name}/{poll}: the exit returns the deopt sentinel"
            );
            assert_eq!(
                calls,
                before_loop + poll,
                "{name}/{poll}: the body leaves at the first non-zero verdict"
            );
            let (frame, cause, point) = crate::deopt::take_last_deopt_with_point()
                .unwrap_or_else(|| panic!("{name}/{poll}: the exit stashes a frame"));
            assert_eq!(
                cause.map(|c| c.reason),
                Some(DeoptReason::OsrExit),
                "{name}/{poll}"
            );
            assert!(
                osr_tier.owns_deopt_point(point),
                "{name}/{poll}: fired by one of this artifact's own points"
            );
            let depth = shape
                .exits
                .iter()
                .find(|&&(bci, _)| bci == frame.bci)
                .map(|&(_, depth)| depth)
                .unwrap_or_else(|| {
                    panic!(
                        "{name}/{poll}: resumed at bci {}, not at the branch",
                        frame.bci
                    )
                });
            assert_eq!(
                frame.stack.len(),
                depth,
                "{name}/{poll}: the branch's operands, live"
            );
            let counter = (shape.counter_at_poll)(poll);
            assert_eq!(
                frame.locals.first(),
                Some(&FrameValue::Int(counter)),
                "{name}/{poll}: local 0 is the counter that poll saw"
            );
            assert_eq!(
                frame.stack.first(),
                Some(&FrameValue::Int(counter)),
                "{name}/{poll}: value1 is the counter: {:?}",
                frame.stack
            );
            if depth == 2 {
                assert_eq!(
                    frame.stack.get(1),
                    Some(&FrameValue::Int(1000)),
                    "{name}/{poll}: value2 is the bound: {:?}",
                    frame.stack
                );
            }
            if shape.max_locals == 2 {
                assert_eq!(
                    frame.locals.get(1),
                    Some(&FrameValue::Int(1000)),
                    "{name}/{poll}: local 1 is the bound"
                );
            }
        }
    }
}

/// Interpreter round i1 wave 15, lane L3
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, stage 3): a
/// METHOD-ENTRY body's back-edge polls leave for the interpreter too — the
/// `goto` poll through its loop header's map, the conditional and switch
/// polls through a map at the branch — but only on
/// `SAFEPOINT_VERDICT_POLLING_BODY` (wave 15's `SAFEPOINT_VERDICT_EVERY_FRAME`).
/// The innermost-frame bit alone is about the thread's innermost interpreter
/// frame, which is not this body's, and is ignored.
///
/// Exact, as the OSR-tier test above: the helper answers from a chosen call
/// on, so the body must leave at that back-edge poll, poll no more, and carry
/// the state that poll saw.
///
/// Wave 17, lane L1 (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`):
/// every slow-path call, the entry poll's included, names the polling body in
/// its second argument — the artifact's own `compile_id` (0 when none was
/// reserved) — which is what lets the VM answer the polling-body bit about
/// this body's method.
#[test]
pub(in crate::x64::tests) fn a_method_entry_body_leaves_at_its_nth_back_edge_poll_on_the_every_frame_verdict() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    static FLAG: u8 = 1;
    // Slow-path calls made so far, the 0-based call from which the helper
    // answers `VERDICT` (`i64::MAX`: never), and that answer.
    static CALLS: AtomicI64 = AtomicI64::new(0);
    static LEAVE_FROM: AtomicI64 = AtomicI64::new(i64::MAX);
    static VERDICT: AtomicI64 = AtomicI64::new(0);
    // The body id the current body must pass; `i64::MIN` (sticky) once a
    // call passed another.
    static BODY_ID: AtomicI64 = AtomicI64::new(0);
    extern "C" fn verdict_poll(_flag_addr: i64, body_id: i64) -> i64 {
        if body_id != BODY_ID.load(Ordering::SeqCst) {
            BODY_ID.store(i64::MIN, Ordering::SeqCst);
        }
        let n = CALLS.fetch_add(1, Ordering::SeqCst);
        if n >= LEAVE_FROM.load(Ordering::SeqCst) {
            VERDICT.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    let every = crate::SAFEPOINT_VERDICT_INNERMOST_FRAME | crate::SAFEPOINT_VERDICT_POLLING_BODY;
    // int i = 0; while (i < 1000) { i++; } return i;  -- javac's `goto` shape.
    #[rustfmt::skip]
    let goto_loop: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
        0x84, 0x00, 0x01, // 9:  iinc 0, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1a,             // 15: iload_0
        0xac,             // 16: ireturn
        0, 0,
    ];
    // int i = 0; do { i++; } while (i < 1000); return i;
    #[rustfmt::skip]
    let do_while: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x84, 0x00, 0x01, // 2:  iinc 0, 1           <- loop header
        0x1a,             // 5:  iload_0
        0x11, 0x03, 0xe8, // 6:  sipush 1000         (fused with the branch)
        0xa1, 0xff, 0xf9, // 9:  if_icmplt -7 -> 2
        0x1a,             // 12: iload_0
        0xac,             // 13: ireturn
        0, 0,
    ];
    // int i = 0, n = 1000; goto COND; BODY: i++; COND: if (i < n) goto BODY;
    // return i;
    #[rustfmt::skip]
    let rotated: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x11, 0x03, 0xe8, // 2:  sipush 1000
        0x3c,             // 5:  istore_1
        0xa7, 0x00, 0x06, // 6:  goto +6 -> 12
        0x84, 0x00, 0x01, // 9:  iinc 0, 1           <- loop header
        0x1a,             // 12: iload_0
        0x1b,             // 13: iload_1
        0xa1, 0xff, 0xfb, // 14: if_icmplt -5 -> 9
        0x1a,             // 17: iload_0
        0xac,             // 18: ireturn
        0, 0,
    ];
    // int i = 0; L: i++; switch (i) { case 1000: return i; default: goto L; }
    #[rustfmt::skip]
    let switch_arm: Vec<u8> = vec![
        0x03,                   // 0:  iconst_0
        0x3b,                   // 1:  istore_0
        0x84, 0x00, 0x01,       // 2:  iinc 0, 1     <- loop header
        0x1a,                   // 5:  iload_0
        0xab, 0x00,             // 6:  lookupswitch (+1 pad)
        0xff, 0xff, 0xff, 0xfc, // 8:  default -> 2
        0x00, 0x00, 0x00, 0x01, // 12: npairs = 1
        0x00, 0x00, 0x03, 0xe8, // 16: key 1000
        0x00, 0x00, 0x00, 0x12, // 20:   -> 24
        0x1a,                   // 24: iload_0
        0xac,                   // 25: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = verdict_poll as *const () as usize; // Cast: helper address
    let build = |code: &[u8], code_len: usize, max_locals: usize| {
        compile_with_request(
            // A method-entry compile. Baseline mode, so the counted loops are
            // not unrolled: an unrolled body polls once per unrolled group
            // (~251 slow-path calls for the `goto` shape's 1000 iterations),
            // and this test pins the counter at the k-th single-iteration poll.
            BackendRequest {
                baseline_mode: true,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            code,
            code_len,
            0,
            max_locals,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("test JIT compile")
    };
    struct Shape<'a> {
        name: &'a str,
        code: &'a [u8],
        code_len: usize,
        max_locals: usize,
        result: i64,
        /// How many times a full run executes the polling back edge.
        back_edge_polls: i64,
        /// The bcis the exit may name, with the operand count there.
        exits: &'a [(u32, usize)],
        /// The counter the `k`-th (1-based) back-edge poll sees.
        counter_at_poll: fn(i64) -> i64,
    }
    let shapes = [
        Shape {
            name: "goto",
            code: &goto_loop,
            code_len: 17,
            max_locals: 1,
            result: 1000,
            back_edge_polls: 1000,
            exits: &[(2, 0)],
            counter_at_poll: |k| k,
        },
        Shape {
            name: "do-while",
            code: &do_while,
            code_len: 14,
            max_locals: 1,
            result: 1000,
            back_edge_polls: 1000,
            exits: &[(6, 1), (9, 2)],
            counter_at_poll: |k| k,
        },
        Shape {
            name: "rotated",
            code: &rotated,
            code_len: 19,
            max_locals: 2,
            result: 1000,
            back_edge_polls: 1001,
            exits: &[(14, 2)],
            counter_at_poll: |k| k - 1,
        },
        Shape {
            name: "switch-arm",
            code: &switch_arm,
            code_len: 26,
            max_locals: 1,
            result: 1000,
            back_edge_polls: 1000,
            exits: &[(6, 1)],
            counter_at_poll: |k| k,
        },
    ];
    for shape in &shapes {
        let name = shape.name;
        let body = build(shape.code, shape.code_len, shape.max_locals);
        assert!(
            shape
                .exits
                .iter()
                .any(|&(bci, _)| body.osr_exit_points.contains(&(bci as usize))),
            "{name}: the method-entry compile records an exit map: {:?}",
            body.osr_exit_points
        );
        let _ = crate::deopt::take_last_deopt_with_point();
        BODY_ID.store(i64::from(body.compile_id), Ordering::SeqCst);

        // A zero verdict throughout: the loop completes, and the call count
        // says how many polls precede the loop (the method-entry poll).
        LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
        VERDICT.store(0, Ordering::SeqCst);
        CALLS.store(0, Ordering::SeqCst);
        // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
        let full = unsafe { body.try_call(&[]) };
        assert_eq!(full, Ok(shape.result), "{name}: a zero verdict completes");
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        let before_loop = CALLS.load(Ordering::SeqCst) - shape.back_edge_polls;
        assert!(
            before_loop >= 0,
            "{name}: {before_loop} polls before the loop"
        );

        // The innermost-frame bit alone, from the first call on: ignored.
        LEAVE_FROM.store(0, Ordering::SeqCst);
        VERDICT.store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
        CALLS.store(0, Ordering::SeqCst);
        // SAFETY: as above.
        let ignored = unsafe { body.try_call(&[]) };
        LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
        assert_eq!(
            ignored,
            Ok(shape.result),
            "{name}: the innermost-frame bit is not this body's"
        );
        assert!(crate::deopt::take_last_deopt_with_point().is_none());

        // The every-frame verdict from the (`skipped` + 1)-th back-edge poll on.
        VERDICT.store(every, Ordering::SeqCst);
        for skipped in [0i64, 5] {
            let poll = skipped + 1;
            LEAVE_FROM.store(before_loop + skipped, Ordering::SeqCst);
            CALLS.store(0, Ordering::SeqCst);
            // SAFETY: as above.
            let raw = unsafe { body.try_call(&[]) };
            let calls = CALLS.load(Ordering::SeqCst);
            LEAVE_FROM.store(i64::MAX, Ordering::SeqCst);
            assert_eq!(
                raw,
                Ok(i64::MIN),
                "{name}/{poll}: the exit returns the deopt sentinel"
            );
            assert_eq!(
                calls,
                before_loop + poll,
                "{name}/{poll}: the body leaves at the first every-frame verdict"
            );
            let (frame, cause, point) = crate::deopt::take_last_deopt_with_point()
                .unwrap_or_else(|| panic!("{name}/{poll}: the exit stashes a frame"));
            assert_eq!(
                cause.map(|c| c.reason),
                Some(DeoptReason::OsrExit),
                "{name}/{poll}"
            );
            assert!(
                body.owns_deopt_point(point),
                "{name}/{poll}: fired by one of this artifact's own points"
            );
            let depth = shape
                .exits
                .iter()
                .find(|&&(bci, _)| bci == frame.bci)
                .map(|&(_, depth)| depth)
                .unwrap_or_else(|| {
                    panic!(
                        "{name}/{poll}: resumed at bci {}, not at the exit",
                        frame.bci
                    )
                });
            assert_eq!(
                frame.stack.len(),
                depth,
                "{name}/{poll}: the operands, live"
            );
            let counter = (shape.counter_at_poll)(poll);
            assert_eq!(
                frame.locals.first(),
                Some(&FrameValue::Int(counter)),
                "{name}/{poll}: local 0 is the counter that poll saw"
            );
            if depth >= 1 {
                assert_eq!(
                    frame.stack.first(),
                    Some(&FrameValue::Int(counter)),
                    "{name}/{poll}: value1 is the counter: {:?}",
                    frame.stack
                );
            }
            if depth == 2 {
                assert_eq!(
                    frame.stack.get(1),
                    Some(&FrameValue::Int(1000)),
                    "{name}/{poll}: value2 is the bound: {:?}",
                    frame.stack
                );
            }
            if shape.max_locals == 2 {
                assert_eq!(
                    frame.locals.get(1),
                    Some(&FrameValue::Int(1000)),
                    "{name}/{poll}: local 1 is the bound"
                );
            }
        }
        VERDICT.store(0, Ordering::SeqCst);
        assert_eq!(
            BODY_ID.load(Ordering::SeqCst),
            i64::from(body.compile_id),
            "{name}: every slow-path call names this body"
        );
    }
}

/// Interpreter round i1 wave 18, lane L3
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, the single-pass
/// admission list): a `goto` back edge with a LIVE operand stack leaves
/// through a map at the `goto`'s own bci carrying that stack, in a
/// method-entry compile (on the polling-body bit) and in an OSR-tier compile
/// (on the innermost-frame bit), at exactly the poll that answered and with
/// the state that poll saw. Before, `mode_exit_target` refused any non-empty
/// stack and the body finished compiled.
#[test]
pub(in crate::x64::tests) fn a_goto_back_edge_with_a_live_operand_stack_leaves_through_a_map_at_the_goto() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // The poll's flag byte, set: a promoted constant, so every poll takes its
    // slow path.
    let flag: &'static u8 = &1;
    // One static for the helper's state (the statics ratchet counts
    // declarations): the slow-path calls made so far, the 0-based call from
    // which the helper answers the verdict (`i64::MAX`: never), and that
    // verdict.
    static POLL: [AtomicI64; 3] = [
        AtomicI64::new(0),
        AtomicI64::new(i64::MAX),
        AtomicI64::new(0),
    ];
    let (calls_made, leave_from, verdict) = (&POLL[0], &POLL[1], &POLL[2]);
    extern "C" fn verdict_poll(_flag_addr: i64, _body_id: i64) -> i64 {
        let n = POLL[0].fetch_add(1, Ordering::SeqCst);
        if n >= POLL[1].load(Ordering::SeqCst) {
            POLL[2].load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // int i = 0; push 7; L: i++; if (i >= 1000) goto X; goto L; X: pop; return i;
    // -- the 7 stays on the operand stack across the loop, so the back-edge
    // `goto` at 14 polls with one live operand.
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x10, 0x07,       // 2:  bipush 7
        0x84, 0x00, 0x01, // 4:  iinc 0, 1           <- loop header, stack [7]
        0x1a,             // 7:  iload_0
        0x11, 0x03, 0xe8, // 8:  sipush 1000
        0xa2, 0x00, 0x06, // 11: if_icmpge +6 -> 17
        0xa7, 0xff, 0xf6, // 14: goto -10 -> 4       (back edge, stack [7])
        0x57,             // 17: pop
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    const GOTO_BCI: u32 = 14;
    const BACK_EDGE_POLLS: i64 = 999;
    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = flag as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = verdict_poll as *const () as usize; // Cast: helper address
    let build = |mode_exit_polls: bool| {
        compile_with_request(
            // Baseline, so the loop is not unrolled (one poll per iteration).
            BackendRequest {
                baseline_mode: true,
                mode_exit_polls,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            &code,
            20,
            0,
            1,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("test JIT compile")
    };
    // (tier, the verdict bit that tier's poll leaves on)
    for (tier, mode_exit_polls, bit) in [
        ("method-entry", false, crate::SAFEPOINT_VERDICT_POLLING_BODY),
        ("osr-tier", true, crate::SAFEPOINT_VERDICT_INNERMOST_FRAME),
    ] {
        let body = build(mode_exit_polls);
        assert!(
            body.osr_exit_points.contains(&(GOTO_BCI as usize)),
            "{tier}: a map is recorded at the goto: {:?}",
            body.osr_exit_points
        );
        let _ = crate::deopt::take_last_deopt_with_point();

        // A zero verdict: the loop completes; count the polls before it.
        leave_from.store(i64::MAX, Ordering::SeqCst);
        verdict.store(0, Ordering::SeqCst);
        calls_made.store(0, Ordering::SeqCst);
        // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
        let full = unsafe { body.try_call(&[]) };
        assert_eq!(full, Ok(1000), "{tier}: a zero verdict completes");
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        let before_loop = calls_made.load(Ordering::SeqCst) - BACK_EDGE_POLLS;
        assert!(
            before_loop >= 0,
            "{tier}: {before_loop} polls before the loop"
        );

        verdict.store(bit, Ordering::SeqCst);
        for skipped in [0i64, 5] {
            let poll = skipped + 1;
            leave_from.store(before_loop + skipped, Ordering::SeqCst);
            calls_made.store(0, Ordering::SeqCst);
            // SAFETY: as above.
            let raw = unsafe { body.try_call(&[]) };
            let calls = calls_made.load(Ordering::SeqCst);
            leave_from.store(i64::MAX, Ordering::SeqCst);
            assert_eq!(raw, Ok(i64::MIN), "{tier}/{poll}: the deopt sentinel");
            assert_eq!(
                calls,
                before_loop + poll,
                "{tier}/{poll}: the body leaves at the first answering poll"
            );
            let (frame, cause, point) = crate::deopt::take_last_deopt_with_point()
                .unwrap_or_else(|| panic!("{tier}/{poll}: the exit stashes a frame"));
            assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
            assert!(body.owns_deopt_point(point), "{tier}/{poll}: its own point");
            assert_eq!(frame.bci, GOTO_BCI, "{tier}/{poll}: resumed at the goto");
            assert_eq!(
                frame.stack,
                vec![FrameValue::Int(7)],
                "{tier}/{poll}: the live operand"
            );
            assert_eq!(
                frame.locals.first(),
                Some(&FrameValue::Int(poll)),
                "{tier}/{poll}: local 0 is the counter that poll saw"
            );
        }
        verdict.store(0, Ordering::SeqCst);
    }
}

// -----------------------------------------------------------------------
// G1-2 — the inline reference-store fast paths must not elide the
// collector's post-write barrier on a backend that publishes no region
// bounds. `g1-audit.md` §8.1: under G1 a young region held out of
// the collection set by a JNI pin is reachable ONLY through its remembered
// set, so an inline store that skips `post_write_barrier_rset` loses the
// edge and the next pause frees a live referent.
// -----------------------------------------------------------------------

/// Does the emitted code contain `value` as a little-endian 8-byte
/// immediate? `emit_mov_imm64` bakes the bounds-table address this way for
/// the region-containment guard, so its presence/absence distinguishes the
/// full guard from the bare trusted-oop null test.
pub(in crate::x64::tests) fn code_contains_u64(compiled: &CompiledMethod, value: usize) -> bool {
    // Cast: address → the exact 8-byte immediate the encoder emitted
    let needle = (value as u64).to_le_bytes();
    compiled.code_bytes().windows(8).any(|w| *w == needle)
}

/// Number of CALLs to `target` in the emitted code. `emit_call_absolute`
/// emits `E8 rel32` when the target is within ±2GB of the call site and a
/// `MOV RAX, imm64` + `CALL RAX` form beyond it, so both encodings are
/// counted. The scan is not instruction-aligned, which can only ever
/// over-count — good enough to assert "the barrier call IS emitted".
pub(in crate::x64::tests) fn calls_to(compiled: &CompiledMethod, target: usize) -> usize {
    let code = compiled.code_bytes();
    // Cast: buffer base address for resolving rel32 displacements
    let base = code.as_ptr() as usize;
    let mut n = 0usize;
    for i in 0..code.len().saturating_sub(4) {
        if code[i] != 0xE8 {
            continue;
        }
        let disp = i32::from_le_bytes([code[i + 1], code[i + 2], code[i + 3], code[i + 4]]);
        let next = base.wrapping_add(i).wrapping_add(5);
        // Cast: sign-extend the rel32 for wrapping address arithmetic
        if next.wrapping_add(disp as isize as usize) == target {
            n += 1;
        }
    }
    if code_contains_u64(compiled, target) {
        n += 1;
    }
    n
}

/// A fake, 8-byte-aligned "compact young object" that every inline
/// reference-`putfield` header test accepts: `GC_FLAG_COMPACT` set,
/// `GC_FLAG_OLD_GEN` clear, and a NULL reference cell at the first compact
/// field (displacement `COMPACT_HEADER_SIZE`). A compact header is the 8-byte
/// header word alone -- there is no field-count word to write.
///
/// Deliberately NOT a `GenerationalHeap` allocation: these tests need the
/// receiver's header bits and the published region bounds to vary
/// independently, and a real heap couples them.
pub(in crate::x64::tests) fn fake_compact_young_object() -> Box<[u64; 8]> {
    let mut o = Box::new([0u64; 8]);
    // SAFETY: `o` is 64 bytes and 8-byte aligned (a `[u64; 8]`); the write
    // lands inside it (`GC_FLAGS_BYTE_OFFSET` is 7).
    unsafe {
        let p = o.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
    }
    o
}

/// Read the 8-byte compact reference cell (body offset 0) of a fake object.
pub(in crate::x64::tests) fn fake_object_ref_cell(o: &[u64; 8]) -> usize {
    // Derived, never restated: this said "HEADER_SIZE == 32 == 4 * 8, so the
    // cell is word 4" and silently read the WRONG WORD when the header shrank
    // to 24 — the failure mode a hard-coded layout constant always has, an
    // assertion comparing two unrelated words rather than a compile error.
    o[cratonvm_types::COMPACT_HEADER_SIZE / 8] as usize // Cast: raw stored pointer word
}

// `region_bounds_are_live` must read the TABLE, not its address.
//
// This is finding 2 of the audit in executable form: `region_bounds_addr`
// is the address of a process-global static and is therefore always
// non-zero, so the `!= 0` test the emitters used to key on is a constant
// true and not a backend gate at all.
// -----------------------------------------------------------------------
// F-08 — the inline G1 post-write barrier
// -----------------------------------------------------------------------

/// The barrier table is read by CONTENT, never by address — the same lesson
/// `region_bounds_are_live_reads_the_table_not_its_address` pins one table
/// over, and the reason that test exists is that the `!= 0` form had shipped.
///
/// A published table with a zero base or a zero region mask must read as NOT
/// live: those are the two words the emitted sequence subtracts and masks with,
/// and a zero mask would make every pair of addresses look like the same region
/// — i.e. it would silently disable the barrier rather than fail loudly.
#[test]
pub(in crate::x64::tests) fn the_g1_barrier_table_is_read_by_content_not_by_address() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let addr = TABLE.as_ptr() as usize;

    assert_ne!(addr, 0, "a static's address is never zero");
    assert!(
        !g1_barrier_table_live(addr),
        "an all-zero table is the no-G1-collector shape and must not read as live"
    );
    assert!(!g1_barrier_table_live(0), "an unwired field is not live");

    // arena_len alone is not enough: the base and the mask are what the
    // sequence computes with.
    TABLE[1].store(0x10000, Ordering::Release);
    assert!(!g1_barrier_table_live(addr), "no base, no mask");
    TABLE[0].store(0x4000_0000, Ordering::Release);
    assert!(!g1_barrier_table_live(addr), "no mask");
    TABLE[2].store(!(0x100000usize - 1), Ordering::Release);
    assert!(
        g1_barrier_table_live(addr),
        "a fully published table is live"
    );

    // `G1Collector::drop` clears the length first.
    TABLE[1].store(0, Ordering::Release);
    assert!(
        !g1_barrier_table_live(addr),
        "a torn-down collector must read as not-live again"
    );
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// The inline barrier's two-instruction filter, EXECUTED.
///
/// This is the test that matters. `emit_g1_barrier_filter` introduces three
/// instruction encodings this backend had no other user for (`SUB r64, m64`,
/// `AND r64, m64`, `XOR r64, r64`) and an address-arithmetic argument that is
/// wrong in a silent, use-after-free direction if it is wrong at all. Reading
/// the emitted bytes would only re-assert the encoding I believe I wrote; this
/// runs them on a real CPU and asks what the flags did.
///
/// The shape under test:
///
/// ```text
///   mov  rax, arg0            ; obj
///   mov  rdx, arg1            ; val
///   <filter>                  ; jumps to `nothing` when there is nothing to do
///   mov  eax, 1 ; ret         ; "would have called the barrier"
/// nothing:
///   xor  eax, eax ; ret
/// ```
///
/// The four cases are exactly the four the filter claims to separate, and the
/// last two are the ones that would be indistinguishable if the arena base
/// were not subtracted: G1's arena is only malloc-aligned, so an aligned
/// `region_size` block of the address space is NOT a region, and a raw
/// `(obj ^ val) & mask` would call two addresses straddling the real region
/// boundary "same region", skip the barrier and lose the edge.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::x64::tests) fn the_inline_g1_barrier_filter_separates_the_four_cases_when_executed() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A synthetic arena at an UNALIGNED base, which is the real shape: G1's
    // arena is a `Vec<u8>`, so its base is malloc-aligned and nothing more.
    const REGION_SIZE: usize = 0x10_0000; // 1 MiB, as G1's default is
    const ARENA_BASE: usize = 0x4000_0000 + 0x30; // deliberately not region-aligned
    const ARENA_LEN: usize = 8 * REGION_SIZE;

    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(ARENA_BASE, Ordering::Release);
    TABLE[2].store(!(REGION_SIZE - 1), Ordering::Release);
    TABLE[1].store(ARENA_LEN, Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = TABLE.as_ptr() as usize;

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "g1-barrier-filter-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        // dev added `array_len_hoist_info` as argument 13 while this branch
        // was open; these tests hoist no array length.
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // `Compiler::new` has already emitted a method prologue into the buffer;
    // this snippet is a self-contained leaf that must be entered AFTER it, so
    // record where it starts and call there rather than at the buffer base.
    // (Entering at the base runs a prologue whose epilogue this snippet's `ret`
    // never reaches — an access violation, which is how this was found.)
    let entry_off = compiler.buf.pos();
    // Move the two C arguments into the registers the filter operates on. No
    // prologue of its own: the sequence touches only RAX/RDX/RCX and returns.
    compiler.emit_mov_r64_r64(RAX, ARG_REGS[0]);
    compiler.emit_mov_r64_r64(RDX, ARG_REGS[1]);
    let nothing = compiler.emit_g1_barrier_filter(RAX, RDX, RCX);
    compiler.emit_mov_imm64(RAX, 1);
    compiler.emit_ret();
    for patch in nothing {
        compiler.patch_rel32_to_here(patch);
    }
    compiler.emit_xor_reg_self(RAX);
    compiler.emit_ret();

    assert!(
        !compiler.buf.overflowed(),
        "the test buffer must hold the snippet"
    );
    // W^X: `ExecutableBuffer::new` maps RW, and the compile driver flips the
    // page to RX when it finalises a method. This snippet bypasses the driver,
    // so it has to do the flip itself or the first instruction faults.
    crate::platform::make_executable(compiler.buf.as_ptr() as *mut u8, compiler.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { compiler.buf.as_ptr().add(entry_off) };
    // SAFETY: the emitted code takes two i64 arguments in the platform's first
    // two argument registers, clobbers only RAX/RCX/RDX (all caller-saved on
    // both the SysV and Win64 ABIs), dereferences nothing but `TABLE`, and
    // returns an i64 in RAX. The buffer is RWX for its whole lifetime, which
    // outlives this call because `compiler` is still alive.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };

    let r0 = ARENA_BASE + 8; // region 0
    let r0_high = ARENA_BASE + REGION_SIZE - 8; // still region 0
    let r1 = ARENA_BASE + REGION_SIZE + 8; // region 1

    assert_eq!(
        f(r0 as i64, 0),
        0,
        "a null store records nothing — post_write_barrier_rset returns on it"
    );
    assert_eq!(
        f(r0 as i64, (r0 + 64) as i64),
        0,
        "a same-region store records nothing"
    );
    assert_eq!(
        f(r0 as i64, r1 as i64),
        1,
        "a cross-region store must reach the collector's barrier"
    );

    let _ = r0_high;

    // The two ALIGNMENT cases, which exist only because G1's arena base is not
    // a multiple of the region size. Region 0 is
    // `[ARENA_BASE, ARENA_BASE + REGION_SIZE)`, and the aligned 1 MiB block
    // boundary at `0x4010_0000` falls INSIDE it.
    //
    // (a) Same region, different aligned blocks. A filter that masked the raw
    //     addresses would call this cross-region: a redundant call, harmless.
    let same_region_lo = ARENA_BASE + REGION_SIZE - 0x38;
    let same_region_hi = same_region_lo + 0x10;
    assert_eq!(
        (same_region_lo - ARENA_BASE) / REGION_SIZE,
        (same_region_hi - ARENA_BASE) / REGION_SIZE,
        "test setup: both addresses are in one region"
    );
    assert_ne!(
        same_region_lo >> 20,
        same_region_hi >> 20,
        "test setup: they are in DIFFERENT aligned 1 MiB blocks"
    );
    assert_eq!(
        f(same_region_lo as i64, same_region_hi as i64),
        0,
        "both are in region 0, so there is nothing to remember"
    );

    // (b) Different regions, SAME aligned block. This is the pair a base-free
    //     `(obj ^ val) & mask` silently dismisses, and dismissing it loses a
    //     live cross-region edge -- the use-after-free the subtraction prevents.
    let last_of_r0 = ARENA_BASE + REGION_SIZE - 8;
    let first_of_r1 = ARENA_BASE + REGION_SIZE + 8;
    assert_ne!(
        (last_of_r0 - ARENA_BASE) / REGION_SIZE,
        (first_of_r1 - ARENA_BASE) / REGION_SIZE,
        "test setup: the pair really is cross-region"
    );
    assert_eq!(
        last_of_r0 >> 20,
        first_of_r1 >> 20,
        "test setup: and it shares one aligned 1 MiB block"
    );
    assert_eq!(
        f(last_of_r0 as i64, first_of_r1 as i64),
        1,
        "a cross-region pair inside one aligned block must still reach the barrier"
    );

    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Availability is the conjunction of three independent facts, and none of the
/// three is redundant: the flag (default OFF), a published table, and a wired
/// helper. Dropping any one of them either emits a barrier nobody asked for,
/// loads through a null table, or CALLs address zero.
#[test]
pub(in crate::x64::tests) fn the_inline_g1_barrier_needs_the_flag_the_table_and_the_helper() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(0x4000_0000, Ordering::Release);
    TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    TABLE[1].store(0x80_0000, Ordering::Release);

    let build = |barrier_addr: usize, helper: usize| {
        let mut helpers = test_helpers();
        helpers.g1_barrier_addr = barrier_addr;
        helpers.g1_post_write_barrier = helper;
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        Compiler::new(
            "g1-barrier-availability-test".to_string(),
            ExecutableBuffer::new(256).expect("test executable buffer"),
            0,
            0,
            8,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            // dev added `array_len_hoist_info` as argument 13 while this branch
            // was open; these tests hoist no array length.
            Vec::new(),
            alloc_result,
            false,
            helpers,
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        )
    };

    let table = TABLE.as_ptr() as usize;
    let helper = 0x1234_5678usize;

    // `CRATONVM_G1_INLINE_BARRIER=0` still suppresses the arm outright. This
    // used to read "the flag is OFF by default, so even a fully wired backend
    // emits nothing"; the default flipped on 2026-09-04, and the assertion that
    // survives the flip is the one about the OFF setting, not about the
    // default.
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(false)));
    assert!(
        !build(table, helper).g1_inline_barrier_available(),
        "`=0` must suppress the inline arm even on a fully wired backend — it is \
         the revert lever for a barrier whose failure mode is a lost \
         remembered-set edge"
    );

    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(true)));
    assert!(
        build(table, helper).g1_inline_barrier_available(),
        "flag + table + helper is the combination that emits"
    );
    assert!(
        !build(0, helper).g1_inline_barrier_available(),
        "no table address: the sequence would load through null"
    );
    assert!(
        !build(table, 0).g1_inline_barrier_available(),
        "no helper: the slow arm would CALL address zero"
    );

    // And with the override OUT of the way, the wired backend emits — which is
    // what "default ON" means and is the half a forced-on assertion cannot show.
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(None));
    assert!(
        build(table, helper).g1_inline_barrier_available(),
        "the arm is default ON since 2026-09-04: a wired backend emits it with no \
         flag set at all"
    );

    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Defect G1-2's gate is untouched by F-08.
///
/// The G1 arm reads a DIFFERENT table, and `region_bounds_are_live` — the
/// predicate that decides whether a barrier-free inline store is legal — must
/// still answer NO for a backend that publishes only the G1 barrier table.
/// If this ever passes, a G1 receiver has become eligible for the generational
/// arm's barrier-free store, which is the use-after-free G1-2 named.
#[test]
pub(in crate::x64::tests) fn publishing_the_g1_barrier_table_does_not_make_region_bounds_live() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static G1_TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static STORE_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    G1_TABLE[0].store(0x4000_0000, Ordering::Release);
    G1_TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    G1_TABLE[1].store(0x80_0000, Ordering::Release);

    assert!(g1_barrier_table_live(G1_TABLE.as_ptr() as usize));
    assert!(
        !region_bounds_are_live(STORE_BOUNDS.as_ptr() as usize),
        "G1 publishes no store-side region bounds, and closing G1-2 depends on it"
    );
    for w in G1_TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// The generational inline card mark stays disabled. F-08 is a different
/// mechanism against a different table and must not be read as re-enabling it:
/// `inline_card_mark_available` needs its own opt-in
/// (`CRATONVM_JIT_INLINE_CARD_MARK`, gen r4w4/cards4) AND a generational card
/// view in `jit_card_table_addr`, which a G1 helper table never carries.
#[test]
pub(in crate::x64::tests) fn the_generational_inline_card_mark_stays_disabled_under_f08() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(0x4000_0000, Ordering::Release);
    TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    TABLE[1].store(0x80_0000, Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = TABLE.as_ptr() as usize;
    helpers.g1_post_write_barrier = 0x1234_5678;
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let compiler = Compiler::new(
        "g1-card-mark-separation-test".to_string(),
        ExecutableBuffer::new(256).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        // dev added `array_len_hoist_info` as argument 13 while this branch
        // was open; these tests hoist no array length.
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(true)));
    assert!(compiler.g1_inline_barrier_available());
    assert!(
        !compiler.inline_card_mark_available(),
        "F-08 must not re-enable the generational inline card mark"
    );
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(None));
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

#[test]
pub(in crate::x64::tests) fn region_bounds_are_live_reads_the_table_not_its_address() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let addr = BOUNDS.as_ptr() as usize; // Cast: static address → helpers-table field

    // The G1/ZGC shape: the static exists (non-zero ADDRESS — exactly what
    // `region_bounds_addr != 0` tested) but nothing was ever published.
    assert_ne!(addr, 0, "a static's address is never zero");
    assert!(
        !region_bounds_are_live(addr),
        "an all-zero table is the G1/ZGC shape and must NOT read as live"
    );

    // Unwired table: JIT unit tests, and any embedding that never set it.
    assert!(!region_bounds_are_live(0));

    // A degenerate empty range is not a live region.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x1000, Ordering::Release);
    assert!(!region_bounds_are_live(addr));

    // A real published range — the Generational shape.
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(addr));

    // Any ONE of the three pairs is enough (here: old gen only).
    BOUNDS[0].store(0, Ordering::Release);
    BOUNDS[1].store(0, Ordering::Release);
    BOUNDS[4].store(0x8000, Ordering::Release);
    BOUNDS[5].store(0x9000, Ordering::Release);
    assert!(region_bounds_are_live(addr));

    // `GenerationalHeap::drop` re-zeroes the table.
    BOUNDS[4].store(0, Ordering::Release);
    BOUNDS[5].store(0, Ordering::Release);
    assert!(
        !region_bounds_are_live(addr),
        "a torn-down heap must read as not-live again"
    );
}


#[test]
pub(in crate::x64::tests) fn inline_ref_putfield_fast_path_is_gated_on_published_region_bounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Marker barrier helper: records that the full-barrier path ran and
    /// deliberately does NOT perform the store, so a fast-path store and a
    /// helper store are trivially distinguishable.
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // void setRef(Object this, Object v) { this.f = v; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1 (reference)
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'L')];
    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → helpers slot
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address

    // Compact layout for the site: pc 2, body offset 0, reference field.
    let compiled = compile_with_request(
        BackendRequest::default(),
        vec![(2, 8, true)],
        &code,
        code_len,
        2,
        2,
        true, // needs_heap — the reference putfield helper takes it
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("reference putfield must compile");

    // Shape: the full-barrier call is emitted exactly once, as the bail
    // target every guarded arm branches to.
    assert_eq!(
        calls_to(&compiled, helpers.putfield_object),
        1,
        "the inline arm must emit exactly one CALL to the barrier helper"
    );

    let mut obj = fake_compact_young_object();
    let val = Box::new([0u64; 8]);
    let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    // 1. G1/ZGC shape — table all zero. Young + compact + null old value,
    //    and the fast path must STILL not be taken.
    assert!(!region_bounds_are_live(helpers.region_bounds_addr));
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap;
    // the receiver is a live 64-byte aligned buffer shaped like an object
    // header and the marker helper performs no store.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "unpublished bounds (the G1/ZGC shape) must route the store to the \
         full-barrier helper even for a young receiver — G1-2"
    );
    assert_eq!(
        fake_object_ref_cell(&obj),
        0,
        "the inline store must not have run"
    );

    // 2. Generational shape — publish a range covering the receiver.
    let page = obj_addr & !0xFFF;
    BOUNDS[0].store(page, Ordering::Release);
    BOUNDS[1].store(page + 0x10000, Ordering::Release);
    assert!(region_bounds_are_live(helpers.region_bounds_addr));

    // 3. Null old value ⇒ nothing for SATB to log ⇒ the elision is correct
    //    and must be preserved: the barrier helper is NOT called.
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "a young, compact, in-bounds receiver with a NULL old value keeps \
         the barrier-free fast path"
    );
    assert_eq!(
        fake_object_ref_cell(&obj),
        val_addr,
        "the inline store must have written the reference cell"
    );

    // 4. Non-null OLD value ⇒ SATB has something to log ⇒ helper.
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a non-null old value must take the SATB pre-barrier helper"
    );

    // 5. Old-generation receiver ⇒ helper (card / RSet), even with a null
    //    old value and live bounds.
    obj[4] = 0;
    // SAFETY: `obj` is the 64-byte buffer built above; byte 7 is its
    // `gc_flags` header byte.
    unsafe {
        let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_OLD_GEN;
    }
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "an old-generation receiver must take the full-barrier helper"
    );
}

/// The trusted-oop receiver substitution replaces the region-containment
/// guard with a bare null test — and the containment guard is precisely
/// what routes every receiver to the full-barrier helper on a backend that
/// publishes no bounds. So the substitution is legal only when bounds are
/// actually live (audit finding 1).
///
/// Proven structurally: with an unpublished table the emitter must still
/// bake the bounds-table address as the guard's `MOV RDX, imm64` operand;
/// with a published one it may drop the guard (and the method gets shorter).
#[test]
pub(in crate::x64::tests) fn trusted_oop_receiver_substitution_requires_live_bounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let mut helpers = test_helpers();
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address
    let bounds_addr = helpers.region_bounds_addr;

    // void setRef(Object this, Object v) { this.f = v; }  — `aload_0`
    // marks the receiver as a proven oop, and a non-empty `method_key` is
    // the other half of `receiver_is_trusted_oop`, so the substitution is
    // eligible at this site. The legacy `compile` wrapper passes `""` and
    // could never reach it, hence `compile_with_param_slots` here.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1 (reference)
        0xb1, // 5: return
        0, 0,
    ];
    let compile_it = |helpers: &JitRuntimeHelpers| {
        compile_with_param_slots(
            &crate::compile_gate::CompileAdmission::for_backend_test(),
            &code,
            6,
            2,
            2,
            true,
            Vec::new(),
            vec![(2usize, 0usize, b'L')],
            Vec::new(), // typecheck_info
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),         // mic_slots
            Vec::new(),         // pic_slots
            Vec::new(),         // ldc_info
            Vec::new(),         // ldc_string_info
            Vec::new(),         // ldc_class_info
            Vec::new(),         // ldc2w_info
            Default::default(), // ldc_fp_pcs
            HashMap::new(),
            HashMap::new(),
            helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            HashMap::new(), // inline_guard_variants (PGO-02)
            None,           // string_layout
            &[],
            0,
            0b11, // param_oop_mask: both parameters are references
            vec![(2usize, 0u32, true)],
            "T.setRef:(Ljava/lang/Object;)V", // non-empty ⇒ trusted-oop eligible
            None,                             // despec: no VM
            Vec::new(),
            None, // elidable_init_pcs: no constant pool, so nothing is proven empty,
            crate::x64::BackendRequest::default(),
        )
        .expect("reference putfield must compile")
    };

    // G1/ZGC shape: no bounds published at emission time.
    assert!(!region_bounds_are_live(bounds_addr));
    let g1 = compile_it(&helpers);

    // Generational shape: bounds live at emission time.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(bounds_addr));
    let generational = compile_it(&helpers);

    // The containment guard bakes the table address as a 64-bit immediate
    // (`emit_mov_imm64` only uses the imm64 form above `i32::MAX`, which is
    // where any real static lands), so its presence is an exact readout of
    // which receiver check was emitted.
    if bounds_addr > i32::MAX as usize {
        assert!(
            code_contains_u64(&g1, bounds_addr),
            "unpublished bounds must keep the full containment guard — G1-2. \
             A failure here means the trusted-oop substitution is being taken \
             on a backend that publishes nothing, which loses G1's RSet edge."
        );
        assert!(
            !code_contains_u64(&generational, bounds_addr),
            "live bounds must still allow the cheap trusted-oop null test. A \
             failure here means either the gate is now unconditionally strict \
             (a throughput regression on Generational) or this site stopped \
             qualifying as `receiver_is_trusted_oop` — check `method_key`, \
             `stack_oop_marks_exact` and the aload_0 oop mark before assuming \
             the barrier gate regressed."
        );
    }

    assert!(
        generational.code_bytes().len() < g1.code_bytes().len(),
        "with live bounds the trusted-oop null test replaces the six-compare \
         containment guard, so the method must be strictly shorter \
         (live={}, unpublished={})",
        generational.code_bytes().len(),
        g1.code_bytes().len()
    );

    // Both shapes still keep the full-barrier helper as the bail target.
    assert_eq!(calls_to(&g1, helpers.putfield_object), 1);
    assert_eq!(calls_to(&generational, helpers.putfield_object), 1);
}

/// `emit_inline_fresh_ctor_compact_ref_putfield` had NO receiver guard at
/// all, so on a non-publishing backend it wrote the reference inline and
/// dropped the collector's post-write barrier entirely. It must now take
/// the helper whenever bounds are not live, and keep the fast path when
/// they are.
#[test]
pub(in crate::x64::tests) fn fresh_ctor_ref_putfield_takes_the_full_barrier_when_bounds_are_not_live() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // Caller: void f(Object o, Object v) { o.<init>(v); }
    let caller: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb7, 0x00, 0x01, // 2: invokespecial #1 → inline site
        0xb1, // 5: return
        0, 0,
    ];
    // Callee: <init>(Object v) { this.f = v; } — the fresh-ctor first store.
    let make_site = || {
        let mut site = make_inline_site(
            &[0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1],
            2,     // callee_max_locals: this + v
            2,     // callee_num_args: this + v
            false, // instance method
            b'V',
        );
        site.method_name = "<init>".to_string();
        site.descriptor = "(Ljava/lang/Object;)V".to_string();
        site.field_info = vec![(2usize, 0usize, b'L')];
        site.compact_field_info = vec![(2usize, 8u32, true)];
        site.needs_heap = true;
        let mut sites = HashMap::new();
        sites.insert(2usize, site);
        sites
    };

    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address

    let compile_it = |helpers: &JitRuntimeHelpers| {
        compile(
            &caller,
            6,
            2,
            2,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            helpers,
            std::collections::HashSet::new(),
            make_site(),
            None, // string_layout
        )
        .expect("inlined <init> reference putfield must compile")
    };

    // 1. G1/ZGC shape at emission time → the emitter routes to the helper.
    assert!(!region_bounds_are_live(helpers.region_bounds_addr));
    let g1 = compile_it(&helpers);

    // 2. Generational shape at emission time → the fast path is emitted.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(helpers.region_bounds_addr));
    let generational = compile_it(&helpers);

    assert_eq!(
        calls_to(&g1, helpers.putfield_object),
        1,
        "the barrier call must be emitted"
    );
    assert!(
        g1.code_bytes().len() < generational.code_bytes().len(),
        "the unpublished-bounds compile is the bare helper call, with no \
         inline store at all (unpublished={}, live={})",
        g1.code_bytes().len(),
        generational.code_bytes().len()
    );

    // The fresh-ctor emitter has no containment guard, so its runtime
    // behaviour does not depend on the table's state — only on which shape
    // was emitted. Run both against identical receivers.
    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    let mut obj_g1 = fake_compact_young_object();
    let obj_g1_addr = obj_g1.as_mut_ptr() as usize; // Cast: receiver address
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap;
    // the receiver is a live 64-byte aligned object-shaped buffer and the
    // marker helper performs no store.
    unsafe {
        g1.call_with_heap(0, &[obj_g1_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a fresh-ctor reference store must take the collector's barrier when \
         the backend publishes no region bounds — G1-2"
    );
    assert_eq!(
        fake_object_ref_cell(&obj_g1),
        0,
        "the inline store must not have run"
    );

    let mut obj_gen = fake_compact_young_object();
    let obj_gen_addr = obj_gen.as_mut_ptr() as usize; // Cast: receiver address
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        generational.call_with_heap(0, &[obj_gen_addr as i64, val_addr as i64]);
        // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "with live bounds a young compact fresh-ctor store keeps its \
         barrier-free inline path"
    );
    assert_eq!(
        fake_object_ref_cell(&obj_gen),
        val_addr,
        "the inline store must have written the reference cell"
    );
}

/// Round 16 lane alloc: the fresh-constructor arm keys on the FIRST SYNTACTIC
/// store to a slot index, which proves nothing about the field's old value
/// unless `inline_ctor_store_starts_from_null` also holds (here it cannot:
/// the hand-built compiler has no method key). A non-null old value must
/// take `jit_putfield_object`, whose SATB pre-barrier logs it; a null one
/// keeps the inline store. No process-global state: the bounds table is
/// leaked and the marker records into the receiver's last word.
#[test]
pub(in crate::x64::tests) fn fresh_ctor_ref_putfield_with_a_non_null_old_value_takes_the_helper() {
    use std::sync::atomic::AtomicUsize;
    const MARK_WORD: usize = 7;
    // SAFETY: `obj` is the live 64-byte fake object the test passes.
    unsafe extern "C" fn marker_putfield_object(_vm: i64, obj: i64, _idx: i64, val: i64) {
        *(obj as *mut u64).add(MARK_WORD) = val as u64; // Cast: raw stored word
    }
    let bounds: &'static [AtomicUsize; 6] = Box::leak(Box::new([
        AtomicUsize::new(0x1000),
        AtomicUsize::new(0x2000),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]));
    // Caller: void f(Object o, Object v) { o.<init>(v); }
    let caller: Vec<u8> = vec![0x2a, 0x2b, 0xb7, 0x00, 0x01, 0xb1, 0, 0];
    let mut site = make_inline_site(&[0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1], 2, 2, false, b'V');
    site.method_name = "<init>".to_string();
    site.descriptor = "(Ljava/lang/Object;)V".to_string();
    site.field_info = vec![(2usize, 0usize, b'L')];
    site.compact_field_info = vec![(2usize, 8u32, true)];
    site.needs_heap = true;
    let mut sites = HashMap::new();
    sites.insert(2usize, site);

    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
    helpers.region_bounds_addr = bounds.as_ptr() as usize; // Cast: leaked table address
    if !region_bounds_are_live(helpers.region_bounds_addr)
        || !crate::x64::objects::fresh_ctor_old_value_test_enabled()
    {
        return; // switched off by the environment
    }
    let compiled = compile(
        &caller,
        6,
        2,
        2,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        sites,
        None, // string_layout
    )
    .expect("inlined <init> reference putfield must compile");

    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize; // Cast: stored reference
    let old = Box::new([0u64; 8]);
    let old_addr = old.as_ptr() as usize; // Cast: the field's previous referent

    let mut obj = fake_compact_young_object();
    obj[cratonvm_types::COMPACT_HEADER_SIZE / 8] = old_addr as u64; // Cast: raw cell word
    let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
                                              // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
                                              // receiver is a live 64-byte object-shaped buffer and the marker writes
                                              // only its last word.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        obj[MARK_WORD] as usize, val_addr,
        "a non-null old value takes the helper"
    );
    assert_eq!(
        fake_object_ref_cell(&obj),
        old_addr,
        "no inline store over it"
    );

    let mut fresh = fake_compact_young_object();
    let fresh_addr = fresh.as_mut_ptr() as usize; // Cast: receiver address
                                                  // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[fresh_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(fresh[MARK_WORD], 0, "a null old value stays inline");
    assert_eq!(fake_object_ref_cell(&fresh), val_addr);
    drop((val, old));
}

#[test]
pub(in crate::x64::tests) fn test_getfield_long() {
    // Method: long getY(Object this) { return this.y; }
    // y is at field_index=1
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x02, // 1: getfield #2
        0xad, // 4: lreturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 1usize, b'J')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);
    heap.set_field(obj, 1, Value::Long(9_999_999_999i64));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 9_999_999_999i64);
}

#[test]
pub(in crate::x64::tests) fn test_getfield_float() {
    // Return float field as bits
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xae, // 4: freturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'F')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Float(3.5f32));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let result_f = f32::from_bits(result as u32); // Cast: JIT ABI convention
    assert!((result_f - 3.5f32).abs() < 0.001);
}

#[test]
pub(in crate::x64::tests) fn test_putfield_int() {
    // Method: void setX(Object this, int val) { this.x = val; }
    // Bytecode: aload_0, iload_1, putfield #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(0));

    // Call: setX(obj, 99)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 99])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention

    // Verify the field was updated
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(99));

    // Test with negative value
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, -42])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(-42));
}

#[test]
pub(in crate::x64::tests) fn test_putfield_long() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1f, // 1: lload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'J')];
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 123_456_789_012i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Long(123_456_789_012i64));
}

#[test]
pub(in crate::x64::tests) fn test_putfield_object_with_write_barrier() {
    // Method: void setRef(Object this, Object ref) { this.ref = ref; }
    // Object putfield needs heap pointer for write barrier
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'L')];
    // needs_heap = true for Object putfield
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        true,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let ref_obj = heap.alloc_object(ClassId::new(0), 1);

    let heap_ptr = &heap as *const GenerationalHeap as i64; // Cast: address arithmetic

    // Call with heap pointer as hidden first arg
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe {
        compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, ref_obj.as_ptr() as i64])
        // Cast: JIT ABI convention
    };

    // Verify the field was updated
    let val = heap.get_field(obj, 0);
    match val {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), ref_obj.as_ptr()),
        other => unreachable!("expected Object(Some), got {other:?}"),
    }

    // Test setting to null
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe { compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, 0]) }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Object(None));
}

#[test]
pub(in crate::x64::tests) fn test_getfield_putfield_roundtrip() {
    // Method: int inc(Object this) { this.x = this.x + 1; return this.x; }
    // aload_0, getfield #1, iconst_1, iadd, aload_0, swap, putfield #1, aload_0, getfield #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1 → x
        0x04, // 4: iconst_1
        0x60, // 5: iadd → x+1
        0x2a, // 6: aload_0
        0x5f, // 7: swap → [obj, x+1]
        0xb5, 0x00, 0x01, // 8: putfield #1 → this.x = x+1
        0x2a, // 11: aload_0
        0xb4, 0x00, 0x01, // 12: getfield #1
        0xac, // 15: ireturn
        0, 0,
    ];
    let code_len = 16;
    let field_info = vec![
        (1usize, 0usize, b'I'),  // getfield at pc=1
        (8usize, 0usize, b'I'),  // putfield at pc=8
        (12usize, 0usize, b'I'), // getfield at pc=12
    ];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(10));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 11);

    // Verify the field is now 11
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(11));

    // Call again — should return 12
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 12);
}

#[test]
pub(in crate::x64::tests) fn test_getfield_double() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xaf, // 4: dreturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'D')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Double(2.719));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let result_d = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert!((result_d - 2.719).abs() < 0.0001);
}

#[test]
pub(in crate::x64::tests) fn test_getfield_object_ref() {
    // getfield that returns an Object reference (areturn)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xb0, // 4: areturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'L')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let ref_obj = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(obj, 0, Value::Object(Some(ref_obj)));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, ref_obj.as_ptr() as i64); // Cast: JIT ABI convention

    // Test null reference
    heap.set_field(obj, 0, Value::Object(None));
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 0);
}

#[test]
pub(in crate::x64::tests) fn test_putfield_float_double() {
    // Test putfield for float
    // Method: void setF(Object this, float f) { this.f = f; }
    let fcode: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x23, // 1: fload_1 (load float param from local 1)
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 1usize, b'F')];
    let compiled = compile(
        &fcode,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);

    let float_bits = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                              // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                              // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, float_bits])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 1);
    assert_eq!(val, Value::Float(1.5f32));
}

#[test]
pub(in crate::x64::tests) fn test_getfield_second_field() {
    // Access field_index=2 (third field)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x03, // 1: getfield #3
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 2usize, b'I')]; // field_index=2
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 4);
    heap.set_field(obj, 0, Value::Int(100));
    heap.set_field(obj, 1, Value::Int(200));
    heap.set_field(obj, 2, Value::Int(300));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 300); // reads field at index 2
}

// ── Inline getfield codegen tests (Value-cell direct MOV) ───────────
//
// These exercise the inline `getfield` path that emits a raw MOV
// against the 16-byte `Value` field cell instead of `CALL jit_getfield`.
// The cases cover int/byte/ref payloads and the null-receiver guard
// and confirm bit-identical results vs the helper.

/// Compile `aload_0; getfield #1; <ret>` with the given field metadata.
pub(in crate::x64::tests) fn compile_single_getfield(ret_op: u8, field_index: usize, type_tag: u8) -> CompiledMethod {
    let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, ret_op, 0, 0];
    compile(
        &code,
        5,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        vec![(1usize, field_index, type_tag)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap()
}

#[test]
pub(in crate::x64::tests) fn test_getfield_inline_byte_field() {
    // A `byte` field is stored as `Value::Int` (sign-extended) — the
    // inline path uses MOVSXD so a negative byte round-trips.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xac /* ireturn */, 0, b'B');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    // Byte fields live in the cell as Value::Int(sign-extended).
    heap.set_field(obj, 0, Value::Int(-7));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r, -7,
        "inline byte getfield must sign-extend like the helper"
    );
    heap.set_field(obj, 0, Value::Int(127));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(r, 127);
}

#[test]
pub(in crate::x64::tests) fn test_getfield_inline_ref_field() {
    // A reference field: the inline path MOVs the 8-byte payload word,
    // which is exactly the raw object pointer (0 for Object(None)).
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xb0 /* areturn */, 0, b'L');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    let target = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(obj, 0, Value::Object(Some(target)));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r,
        // Cast: object/array pointer to i64 for the JIT calling convention
        target.as_ptr() as i64,
        "inline ref getfield must return the raw object pointer"
    );
    // Null reference field → 0.
    heap.set_field(obj, 0, Value::Object(None));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    // Cast: numeric conversion
    assert_eq!(r, 0, "Object(None) field must read as 0");
}

#[test]
pub(in crate::x64::tests) fn test_getfield_inline_long_field() {
    // 8-byte payload load for a `long` field, including a value whose
    // high bit is set (no truncation / sign issues).
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xad /* lreturn */, 1, b'J');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let v = 0x7EDC_BA98_7654_3210_i64;
    heap.set_field(obj, 1, Value::Long(v));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r, v,
        "inline long getfield must load the full 64-bit payload"
    );
}

#[test]
pub(in crate::x64::tests) fn test_getfield_inline_null_receiver_returns_zero() {
    // A null receiver must NOT fault: the inline null check skips the
    // load and yields 0, bit-identical to `jit_getfield`'s
    // `if obj_ptr == 0 { return 0 }` guard. Covered for both an int
    // field (32-bit payload path) and a ref field (64-bit path).
    let compiled_int = compile_single_getfield(0xac /* ireturn */, 0, b'I');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe {
        compiled_int
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("test JIT call")
    };
    assert_eq!(r, 0, "null-receiver int getfield must return 0, not fault");

    let compiled_ref = compile_single_getfield(0xb0 /* areturn */, 0, b'L');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe {
        compiled_ref
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("test JIT call")
    };
    assert_eq!(r, 0, "null-receiver ref getfield must return 0, not fault");

    let compiled_long = compile_single_getfield(0xad /* lreturn */, 0, b'J');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe {
        compiled_long
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("test JIT call")
    };
    assert_eq!(r, 0, "null-receiver long getfield must return 0, not fault");
}

#[test]
pub(in crate::x64::tests) fn test_getfield_inline_matches_helper_differential() {
    // Differential: for a spread of int field values, the inline path
    // result must equal what `stub_getfield` (the helper) would return.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xac /* ireturn */, 0, b'I');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    for &v in &[0i32, 1, -1, i32::MAX, i32::MIN, 0x5A5A_5A5A, -0x0102_0304] {
        heap.set_field(obj, 0, Value::Int(v));
        // Helper reference result.
        // SAFETY: obj is a live heap object with field index 0 in bounds.
        let helper = unsafe { stub_getfield(0, obj.as_ptr() as i64, 0) };
        // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
        let inline = unsafe {
            compiled
                .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
                .expect("test JIT call")
        };
        assert_eq!(
            inline, helper,
            "inline getfield diverged from helper for value {v}"
        );
    }
}

// ===================================================================
// LICM (Loop-Invariant Code Motion) tests
// ===================================================================

#[test]
pub(in crate::x64::tests) fn test_detect_loops() {
    // Simple loop: for (k=0; k<10; k++) { ... }
    // 0: iconst_0       k = 0
    // 1: istore_1
    // 2: iload_1        ← loop header
    // 3: bipush 10
    // 5: if_icmpge +8   → exit at 13
    // 8: iinc 1, 1      k++
    // 11: goto -9        → back to 2
    // 13: iload_0        after loop
    // 14: ireturn
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1 (header)
        0x10, 0x0a, // 3: bipush 10
        0xa2, 0x00, 0x08, // 5: if_icmpge +8 → 13
        0x84, 0x01, 0x01, // 8: iinc 1, 1
        0xa7, 0xff, 0xf7, // 11: goto -9 → 2
        0x1a, // 14: iload_0
        0xac, // 15: ireturn
        0, 0,
    ];
    let loops = detect_loops(&code, 16);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (2, 11)); // header=2, back_edge=11
}

/// Tier proposal R12-3: javac's `while (n > 0) { if (m > 0) { n--; continue; }
/// n -= 2; }` has two backward `goto`s to one header. The interpreter's
/// loop-exit recorder charges the `if`'s taken branch (to pc 14, right after
/// the `continue`'s `goto` at 11) to key 11, so the planner must read a trip
/// hint only under the header's last back edge, 17.
#[test]
pub(in crate::x64::tests) fn a_trip_hint_is_keyed_by_the_headers_last_back_edge() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (header)
        0x9e, 0x00, 0x13, // 1: ifle +19 → 20 (the loop's exit)
        0x1b, // 4: iload_1
        0x9e, 0x00, 0x09, // 5: ifle +9 → 14 (the `else`)
        0x84, 0x00, 0xff, // 8: iinc 0, -1
        0xa7, 0xff, 0xf5, // 11: goto -11 → 0 (the `continue`)
        0x84, 0x00, 0xfe, // 14: iinc 0, -2
        0xa7, 0xff, 0xef, // 17: goto -17 → 0 (the back edge)
        0x1a, // 20: iload_0
        0xac, // 21: ireturn
        0, 0,
    ];
    let mut loops = detect_loops(&code, 22);
    loops.sort_unstable();
    assert_eq!(loops, vec![(0, 11), (0, 17)]);
    let last = last_back_edge_per_header(&loops);
    assert_eq!(last.get(&0), Some(&17));
    assert_eq!(last.len(), 1);
    // Order-independent.
    loops.reverse();
    assert_eq!(last_back_edge_per_header(&loops).get(&0), Some(&17));
}

#[test]
pub(in crate::x64::tests) fn detects_only_canonical_inclusive_zero_byte_fill_loop() {
    // for (int i = from; i <= bound; i++) array[i] = 0;
    let code = [
        0x1b, // 0: iload_1 (i)
        0x1c, // 1: iload_2 (bound)
        0xa3, 0x00, 0x0d, // 2: if_icmpgt -> 15
        0x2a, // 5: aload_0 (array)
        0x1b, // 6: iload_1 (i)
        0x03, // 7: iconst_0
        0x54, // 8: bastore
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -> 0
        0xb1, // 15: return
    ];
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops, vec![(0, 12)]);
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&code, code.len(), 0, 12),
        Some(BulkZeroByteFillLoop {
            header_pc: 0,
            array_local: 0,
            iv_local: 1,
            bound_local: 2,
        })
    );

    let mut nonzero = code;
    nonzero[7] = 0x04; // iconst_1
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&nonzero, nonzero.len(), 0, 12),
        None,
        "non-zero fills retain scalar code"
    );

    let mut mismatched_iv = code;
    mismatched_iv[6] = 0x1d; // store index comes from local 3
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&mismatched_iv, mismatched_iv.len(), 0, 12),
        None,
        "the store index must be the loop induction variable"
    );
}

#[test]
pub(in crate::x64::tests) fn bulk_zero_byte_fill_executes_range_and_skips_empty_null_range() {
    // int f(byte[] a, int i, int bound) {
    //   for (; i <= bound; i++) a[i] = 0;
    //   return i;
    // }
    let code = [
        0x1b, // 0: iload_1
        0x1c, // 1: iload_2
        0xa3, 0x00, 0x0d, // 2: if_icmpgt -> 15
        0x2a, // 5: aload_0
        0x1b, // 6: iload_1
        0x03, // 7: iconst_0
        0x54, // 8: bastore
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -> 0
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
        0x00, 0x00,
    ];
    let compiled = compile(
        &code,
        17,
        3,
        3,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("bulk-zero loop should compile");

    let byte_len = 8usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and has room for the complete VM header
    // plus eight data bytes.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
    }

    // SAFETY: the compiled method receives a correctly laid out live array.
    let result = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 2, 5])
            .expect("bulk-zero JIT call")
    };
    assert_eq!(result, 6);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[7, 7, 0, 0, 0, 0, 7, 7]);

    // An empty range does not dereference the array, even when it is null.
    // SAFETY: the bytecode exits at its condition before any array access.
    let empty = unsafe { compiled.try_call(&[0, 6, 5]).expect("empty-range JIT call") };
    assert_eq!(empty, 6);
}

#[test]
pub(in crate::x64::tests) fn detects_and_executes_canonical_strided_byte_set_loop() {
    // int f(byte[] a, int j, int bound, int step) {
    //   for (; j <= bound; j += step) a[j] = 1;
    //   return j;
    // }
    let code = [
        0x1b, // 0: iload_1
        0x1c, // 1: iload_2
        0xa3, 0x00, 0x10, // 2: if_icmpgt -> 18
        0x2a, // 5: aload_0
        0x1b, // 6: iload_1
        0x04, // 7: iconst_1
        0x54, // 8: bastore
        0x1b, // 9: iload_1
        0x1d, // 10: iload_3
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0xa7, 0xff, 0xf3, // 13: goto -> 0
        0x00, 0x00, // 16: padding
        0x1b, // 18: iload_1
        0xac, // 19: ireturn
        0x00, 0x00,
    ];
    let loops = detect_loops(&code, 20);
    assert_eq!(loops, vec![(0, 13)]);
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&code, 20, 0, 13),
        Some(BulkSetByteStrideLoop {
            header_pc: 0,
            array_local: 0,
            iv_local: 1,
            bound_local: 2,
            step_local: 3,
        })
    );
    let mut wrong_value = code;
    wrong_value[7] = 0x03; // iconst_0
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&wrong_value, 20, 0, 13),
        None,
        "only the canonical unit store is specialized"
    );
    let mut changing_step = code;
    changing_step[10] = 0x1b; // iload_1: the IV cannot also be its step
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&changing_step, 20, 0, 13),
        None,
        "the cached step must be loop-invariant"
    );

    let compiled = compile(
        &code,
        20,
        4,
        4,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("strided byte-set loop should compile");
    let byte_len = 8usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and contains the VM header plus data.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
    }
    // SAFETY: the compiled method receives a correctly laid out live array.
    let result = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 1, 7, 2])
            .expect("strided byte-set JIT call")
    };
    assert_eq!(result, 9);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[0, 1, 0, 1, 0, 1, 0, 1]);

    // A bound beyond the array conservatively takes the scalar path. This
    // particular stride never reaches the invalid index, so it still
    // completes normally and demonstrates that guard failure preserves
    // the bytecode's exact store sequence.
    // SAFETY: `ARRAY_DATA_OFFSET + byte_len` bytes lie inside `words`, which is live and sized for the array header plus data.
    unsafe {
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 0, byte_len);
    }
    // SAFETY: the array is live and every reached strided index is valid.
    let conservative = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 1, 8, 2])
            .expect("conservative scalar fallback call")
    };
    assert_eq!(conservative, 9);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[0, 1, 0, 1, 0, 1, 0, 1]);

    // Empty ranges retain Java's condition-before-array-access behavior.
    // SAFETY: the loop exits before dereferencing the null array.
    let empty = unsafe {
        compiled
            .try_call(&[0, 8, 7, 2])
            .expect("empty-range strided JIT call")
    };
    assert_eq!(empty, 8);
}

#[test]
pub(in crate::x64::tests) fn detects_and_executes_canonical_byte_sieve_loop_nest() {
    // Exact javac shape of CratonBench.sieve(boolean[], int).
    let code = [
        0x03, 0x3d, // 0: int i/count = 0
        0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: clear-loop header -> 17
        0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
        0x03, 0x3d, 0x05, 0x3e, // 17: count=0; outer i=2
        0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 21: outer header -> 66
        0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 26: if (a[i]) -> 60
        0x84, 0x02, 0x01, // 32: count++
        0x1d, 0x1d, 0x60, 0x36, 0x04, // 35: j=i+i
        0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 40: inner header -> 60
        0x2a, 0x15, 0x04, 0x04, 0x54, // 46: a[j] = 1
        0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 51: j += i
        0xa7, 0xff, 0xef, // 57: goto 40
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xd6, // 60: i++; goto 21
        0x1c, 0xac, // 66: return count
        0x00, 0x00,
    ];
    let loops = detect_loops(&code, 68);
    assert!(loops.contains(&(21, 63)), "outer loop missing: {loops:?}");
    assert_eq!(
        detect_byte_sieve_loop(&code, 68, 21, 63),
        Some(ByteSieveLoop {
            header_pc: 21,
            array_local: 0,
            outer_iv_local: 3,
            bound_local: 1,
            count_local: 2,
            inner_iv_local: 4,
        })
    );

    let compiled = compile(
        &code,
        68,
        2,
        5,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("canonical byte sieve should compile");

    let byte_len = 32usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and contains the VM header plus data.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
    }
    // SAFETY: the compiled method receives a correctly laid out live array.
    let count = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 31])
            .expect("canonical byte-sieve JIT call")
    };
    assert_eq!(count, 11);
    // SAFETY: the 32-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    for prime in [2usize, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31] {
        assert_eq!(data[prime], 0, "{prime} must remain prime");
    }
    for composite in [4usize, 6, 8, 9, 10, 12, 15, 21, 25, 27, 30] {
        assert_eq!(data[composite], 1, "{composite} must be marked");
    }

    // Both counted loops are zero-trip, so a null array is not touched.
    // SAFETY: the original bytecode exits both headers before array access.
    let empty = unsafe {
        compiled
            .try_call(&[0, -1])
            .expect("empty canonical byte-sieve call")
    };
    assert_eq!(empty, 0);
}

/// Round 11 wave 11 (lane x64): the sieve nest's qword scan jumps straight to
/// the first zero byte of a word that holds one (`emit_byte_sieve_nest`,
/// `.has_zero`). Every byte and the prime count must still equal a plain
/// Rust sieve, for limits whose tails end at every residue of the 8-byte scan.
#[test]
pub(in crate::x64::tests) fn byte_sieve_nest_qword_skip_matches_a_scalar_oracle() {
    let code = [
        0x03, 0x3d, 0x1c, 0x1b, 0xa3, 0x00, 0x0d, 0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xf4, 0x03, 0x3d, 0x05, 0x3e, 0x1d, 0x1b, 0xa3, 0x00, 0x2b, 0x2a, 0x1d, 0x33, 0x9a,
        0x00, 0x1f, 0x84, 0x02, 0x01, 0x1d, 0x1d, 0x60, 0x36, 0x04, 0x15, 0x04, 0x1b, 0xa3, 0x00,
        0x11, 0x2a, 0x15, 0x04, 0x04, 0x54, 0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, 0xa7, 0xff, 0xef,
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xd6, 0x1c, 0xac, 0x00, 0x00,
    ];
    let compiled = compile(
        &code,
        68,
        2,
        5,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("canonical byte sieve should compile");

    for limit in [9usize, 16, 17, 63, 64, 65, 66, 67, 68, 69, 70, 1000, 4999] {
        let byte_len = limit + 1;
        let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
        let array_ptr = words.as_mut_ptr() as *mut u8;
        // SAFETY: `words` is aligned and contains the VM header plus data.
        unsafe {
            (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
            std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
        }
        // SAFETY: the compiled method receives a correctly laid out live array.
        let count = unsafe {
            compiled
                .try_call(&[array_ptr as i64, limit as i64])
                .expect("byte-sieve JIT call")
        };

        let mut oracle = vec![0u8; byte_len];
        let mut oracle_count = 0i64;
        for i in 2..=limit {
            if oracle[i] == 0 {
                oracle_count += 1;
                let mut j = i + i;
                while j <= limit {
                    oracle[j] = 1;
                    j += i;
                }
            }
        }
        assert_eq!(count, oracle_count, "prime count for limit {limit}");
        // SAFETY: the data region is within `words`.
        let data =
            unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
        assert_eq!(data, &oracle[..], "marks for limit {limit}");
    }
}

#[test]
pub(in crate::x64::tests) fn test_find_modified_locals() {
    // Loop body: iinc 1,1; istore_2; aload_0; iload_3; aaload
    let code: Vec<u8> = vec![
        0x84, 0x01, 0x01, // 0: iinc 1, 1
        0x3d, // 3: istore_2
        0x2a, // 4: aload_0
        0x1d, // 5: iload_3
        0x32, // 6: aaload
    ];
    let modified = find_modified_locals(&code, 0, 7);
    assert!(modified & (1 << 1) != 0); // local 1 modified by iinc
    assert!(modified & (1 << 2) != 0); // local 2 modified by istore_2
    assert!(modified & (1 << 0) == 0); // local 0 NOT modified
    assert!(modified & (1 << 3) == 0); // local 3 NOT modified
}

/// A `long`/`double` store writes TWO local slots, and the mask has to say so.
///
/// `test_find_modified_locals_sees_every_wide_form` covers the `wide` prefix
/// (`7af844829`). This is the adjacent hole that audit turned up: every arm of
/// this scan recorded only the LOWER slot of a category-2 store, so a consumer
/// asking about the upper one was told the loop does not write it.
///
/// It was pinned the wrong way round in this very file until 2026-09-05 —
/// `p87_fp_loop_hoist_detection` and `p87_fp_hoist_double_and_float` both
/// asserted that a `dload_1` is hoistable out of a loop whose body contains
/// `dstore_0`, one of them with the comment "this modifies 0, but doesn't
/// affect 1". It affects 1.
#[test]
pub(in crate::x64::tests) fn find_modified_locals_marks_the_high_half_of_a_category_2_store() {
    // dstore_1 (narrow, implicit index) covers locals 1 and 2.
    let dstore_1: Vec<u8> = vec![0x48, 0xb1];
    assert_eq!(
        find_modified_locals(&dstore_1, 0, dstore_1.len()),
        (1u64 << 1) | (1u64 << 2),
        "`dstore_1` writes locals 1 and 2"
    );

    // lstore 5 (1-byte index operand) covers 5 and 6.
    let lstore_5: Vec<u8> = vec![0x37, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&lstore_5, 0, lstore_5.len()),
        (1u64 << 5) | (1u64 << 6),
        "`lstore 5` writes locals 5 and 6"
    );

    // wide lstore 5 (2-byte index operand) covers the same pair.
    let wide_lstore: Vec<u8> = vec![0xc4, 0x37, 0x00, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&wide_lstore, 0, wide_lstore.len()),
        (1u64 << 5) | (1u64 << 6),
        "`wide lstore 5` writes locals 5 and 6"
    );

    // A category-1 store keeps its single slot: over-marking is safe here, but
    // marking MORE than the store writes is still a lost hoist, so pin it.
    let istore_5: Vec<u8> = vec![0x36, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&istore_5, 0, istore_5.len()),
        1u64 << 5,
        "`istore 5` writes local 5 only"
    );
}

/// `for (i = 0; i < a.length; i++) if (a[i] == 'a') c++;` — the exact inner
/// loop of `probes/CharAtCostCurve.java::scanArr`, taken from `javap -c -p -l`.
///
/// This is the shape the whole hoist exists for. javac re-evaluates `a.length`
/// at the top of every iteration, and the single-pass emitter took that
/// literally: a receiver move, a `TEST`/`JZ` null check that the loop's own
/// previous iteration had already discharged, and a header dereference — four
/// instructions of a 21-instruction body whose useful work is one `MOVZX`.
///
/// Locals: 0=a (char[]), 2=c, 4=i.
#[test]
pub(in crate::x64::tests) fn find_array_len_hoists_matches_the_canonical_counted_loop() {
    let code: Vec<u8> = vec![
        0x03, // 0:  iconst_0
        0x3d, // 1:  istore_2        (c = 0)
        0x03, // 2:  iconst_0
        0x36, 0x04, // 3:  istore 4        (i = 0)
        0x00, 0x00, 0x00, 0x00, 0x00, // 5..9: nop padding to bci 10
        0x00, 0x00, // 10..11: nop
        0x15, 0x04, // 12: iload 4         (header)
        0x2a, // 14: aload_0
        0xbe, // 15: arraylength
        0xa2, 0x00, 0x16, // 16: if_icmpge +22 -> 38
        0x2a, // 19: aload_0
        0x15, 0x04, // 20: iload 4
        0x34, // 22: caload
        0x10, 0x61, // 23: bipush 97
        0xa0, 0x00, 0x06, // 25: if_icmpne +6 -> 31
        0x84, 0x02, 0x01, // 28: iinc 2, 1
        0x84, 0x04, 0x01, // 31: iinc 4, 1
        0xa7, 0xff, 0xea, // 34: goto -22 -> 12
        0x1c, // 37: iload_2
        0xac, // 38: ireturn
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(
        loops[0],
        (12, 34),
        "back edge 34 -> header 12, got {loops:?}"
    );

    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "one invariant arraylength, got {hoists:?}");
    assert_eq!(hoists[0].loop_header, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].loop_end, 37, "one past the 3-byte goto at 34");
    assert_eq!(
        hoists[0].sites,
        vec![(14, 16)],
        "the aload_0 at 14 through the arraylength at 15"
    );
}

/// Two reads of the same length in one body share ONE slot and ONE pre-header
/// computation — the pre-header must not grow a load per site.
#[test]
pub(in crate::x64::tests) fn find_array_len_hoists_shares_one_slot_across_sites() {
    // Locals: 0=a, 1=i.
    //  0: iload_1 ; 1: aload_0 ; 2: arraylength ; 3: if_icmpge -> 16 (header at 0)
    //  6: aload_0 ; 7: arraylength ; 8: pop
    //  9: iinc 1,1 ; 12: goto -> 0 ; 15: return
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0c, // 3:  if_icmpge +12 -> 15
        0x2a, // 6:  aload_0
        0xbe, // 7:  arraylength
        0x57, // 8:  pop
        0x84, 0x01, 0x01, // 9:  iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 -> 0
        0xb1, // 15: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "one record, not one per site");
    assert_eq!(hoists[0].sites, vec![(1, 3), (6, 8)]);
}

/// An array local reassigned inside the body is not invariant, and its length
/// may genuinely differ per iteration. Caching it would make the loop read a
/// stale bound — and, where BCE trusted that bound, an unchecked access.
#[test]
pub(in crate::x64::tests) fn find_array_len_hoists_refuses_a_reassigned_array_local() {
    // Locals: 0=a, 1=i, 2=other.
    //  0: iload_1 ; 1: aload_0 ; 2: arraylength ; 3: if_icmpge -> 16
    //  6: aload_2 ; 7: astore_0        <- a = other, inside the body
    //  8: iinc 1,1 ; 11: goto -> 0 ; 14: return
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0b, // 3:  if_icmpge +11 -> 14
        0x2c, // 6:  aload_2
        0x4b, // 7:  astore_0
        0x84, 0x01, 0x01, // 8:  iinc 1, 1
        0xa7, 0xff, 0xf5, // 11: goto -11 -> 0
        0xb1, // 14: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "astore_0 in the body makes local 0 variant, got {hoists:?}"
    );
}

/// The body scan refuses any body containing a `wide` prefix outright.
///
/// It was written when `find_modified_locals` genuinely could not decode one:
/// a `wide astore 0` fell into that function's catch-all arm and left local 0
/// looking invariant. The mask decodes `wide` now -- it had to, because the
/// arith, `aaload` and FP hoists share it and had no guard of their own, and
/// the arith one miscompiled a strided loop as a result. So this refusal is
/// belt-and-braces today. Kept anyway: the same scan also refuses `jsr`/`ret`,
/// which `detect_loops` still does not model, and refusing costs only a hoist.
///
/// The second assertion is the one that notices the mask regressing.
#[test]
pub(in crate::x64::tests) fn find_array_len_hoists_refuses_a_wide_prefixed_body() {
    // Same as the refusal above, but the store is `wide astore 0`
    // (c4 3a 00 00) — four bytes, which `find_modified_locals` skips whole.
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0e, // 3:  if_icmpge +14 -> 17
        0x2c, // 6:  aload_2
        0xc4, 0x3a, 0x00, 0x00, // 7:  wide astore 0
        0x84, 0x01, 0x01, // 11: iinc 1, 1
        0xa7, 0xff, 0xf2, // 14: goto -14 -> 0
        0xb1, // 17: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "a wide prefix in the body is not modelled, got {hoists:?}"
    );
    assert_eq!(
        find_modified_locals(&code, 0, code_len) & 1,
        1,
        "and the mask itself must see the `wide astore 0`"
    );
}

/// The cooperative safepoint poll is one RIP-relative instruction, and the
/// displacement it bakes must resolve to the flag byte itself.
///
/// A displacement measured from the wrong reference point reads a byte NEAR
/// the flag. That is not a fault and not a crash: the poll simply stops seeing
/// stop-the-world requests, or sees phantom ones, on the back edge of every
/// compiled loop in the VM. Nothing else in the suite would notice, so decode
/// the bytes and check the arithmetic.
#[test]
pub(in crate::x64::tests) fn rip_relative_safepoint_poll_addresses_the_flag_byte() {
    static FLAG: u8 = 0;

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = test_helpers();
    let flag_addr = &FLAG as *const u8 as usize;
    helpers.safepoint_flag_addr = flag_addr;
    let mut c = Compiler::new(
        "rip-poll-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    let start = c.buf.pos();
    let emitted = c.emit_test_mem8_abs_imm8(flag_addr, 0xFF);
    if !emitted {
        // The buffer landed more than 2GB from this test binary's data
        // segment. The fallback is the pre-2026-09-02 sequence and is checked
        // by the executing poll tests; nothing to decode here.
        assert_eq!(c.buf.pos(), start, "a refused encoding emits nothing");
        return;
    }
    let bytes: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert_eq!(bytes.len(), 7, "F6 05 <disp32> <imm8>");
    assert_eq!(&bytes[..2], &[0xF6, 0x05], "TEST r/m8, imm8 via [rip+d32]");
    assert_eq!(bytes[6], 0xFF, "the imm8 tests every bit of the flag byte");

    let disp = i32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    // RIP is the address of the NEXT instruction — past the imm8, not past the
    // displacement. `+ 7`, not `+ 6`: the whole point of the test.
    let insn_end = c.buf.as_ptr() as usize + start + 7;
    let resolved = (insn_end as i64).wrapping_add(disp as i64) as usize;
    assert_eq!(
        resolved, flag_addr,
        "the RIP-relative displacement must land exactly on the flag byte"
    );
}

/// Round 11 wave 2 (lane x64gen): the volatile-store StoreLoad barrier is
/// `LOCK ADD DWORD [RSP], 0`, and the OSR placeholder-floor refresh is inert
/// without a floor slot and, with one, compares the slot against `-1` before
/// fetching the real floor.
#[test]
pub(in crate::x64::tests) fn volatile_store_fence_and_osr_floor_refresh_bytes() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut c = Compiler::new(
        "r11-x64gen-fence-floor-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    let start = c.buf.pos();
    c.emit_volatile_store_fence();
    assert_eq!(
        &c.buf.as_slice()[start..c.buf.pos()],
        &[0xF0, 0x83, 0x04, 0x24, 0x00],
        "LOCK ADD DWORD [RSP], 0 -- not MFENCE (0F AE F0)"
    );

    c.stack_floor_slot_off = 0;
    let before = c.buf.pos();
    c.emit_osr_placeholder_floor_refresh();
    assert_eq!(c.buf.pos(), before, "no floor slot: nothing emitted");

    c.stack_floor_slot_off = 0x40;
    c.helpers.native_stack_floor_fn = 0x1150;
    let start = c.buf.pos();
    c.emit_osr_placeholder_floor_refresh();
    let bytes: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert_eq!(
        &bytes[..5],
        &[0x48, 0x83, 0x7D, 0xC0, 0xFF],
        "CMP qword [rbp-0x40], -1"
    );
    assert_eq!(
        &bytes[5..7],
        &[0x0F, 0x85],
        "JNE past the refresh when the floor is real"
    );
    let n = bytes.len();
    // ...MOV [rbp-0x40], RAX closes the refresh, and the JNE lands after it.
    assert_eq!(
        &bytes[n - 4..],
        &[0x48, 0x89, 0x45, 0xC0],
        "MOV [rbp-0x40], RAX"
    );
    let jne_rel = i32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
    assert_eq!(
        11 + jne_rel as usize,
        n,
        "JNE lands right after the refresh"
    );
}

/// The layout-epoch guard's RIP-relative compare must name the counter, and
/// must register the trail the unroll duplicator needs.
///
/// Two failures this catches, both silent:
///
/// * a displacement measured from the wrong reference point — `+ 6` instead of
///   `+ 10`, forgetting the trailing `imm32` — compares a word four bytes from
///   the epoch against the baked value. That is not a fault. It is a guard
///   that answers about an unrelated global forever, which means an inline
///   field access that keeps its baked offset across a layout REPLACEMENT:
///   the "confirmed heap corruption" the allocation emitters' own guard
///   comment names.
/// * a `rip_abs_disp32_patches` entry whose trail is not 4. The unroller
///   copies body bytes verbatim and re-resolves each entry against the copy's
///   own PC; a trail that under-counts by 3 (the poll's 1, transcribed) makes
///   every UNROLLED copy of the guard read three bytes off while the original
///   stays correct — so the first iteration is right and the rest are not.
#[test]
pub(in crate::x64::tests) fn rip_relative_epoch_guard_addresses_the_counter_and_declares_its_trail() {
    static EPOCH: u32 = 0;

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut c = Compiler::new(
        "rip-epoch-guard-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    let epoch_addr = &EPOCH as *const u32 as usize;
    let start = c.buf.pos();
    let patches_before = c.rip_abs_disp32_patches.len();
    if !c.emit_cmp_mem32_abs_imm32(epoch_addr, 0x1234_5678) {
        // The buffer landed more than 2GB from this test binary's data
        // segment — which is exactly the shape a `static` counter has, and
        // exactly why the epoch lives on the heap. The fallback is the
        // materialize-the-address form.
        assert_eq!(c.buf.pos(), start, "a refused encoding emits nothing");
        assert_eq!(
            c.rip_abs_disp32_patches.len(),
            patches_before,
            "a refused encoding registers no patch site either",
        );
        return;
    }
    let bytes: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert_eq!(bytes.len(), 10, "81 3D <disp32> <imm32>");
    assert_eq!(&bytes[..2], &[0x81, 0x3D], "CMP r/m32, imm32 via [rip+d32]");
    assert_eq!(
        i32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
        0x1234_5678,
        "the baked epoch is the trailing imm32",
    );

    let disp = i32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    // RIP is the address of the NEXT instruction — past the imm32, not past
    // the displacement. `+ 10`, not `+ 6`.
    let insn_end = c.buf.as_ptr() as usize + start + 10;
    let resolved = (insn_end as i64).wrapping_add(disp as i64) as usize;
    assert_eq!(
        resolved, epoch_addr,
        "the RIP-relative displacement must land exactly on the epoch counter"
    );

    let (patch_off, trail) = *c
        .rip_abs_disp32_patches
        .last()
        .expect("the guard registers its displacement for the unroll duplicator");
    assert_eq!(
        patch_off,
        start + 2,
        "the disp32 follows the two opcode bytes"
    );
    assert_eq!(
        trail, 4,
        "the imm32 is part of the instruction the CPU measures from"
    );
}

#[test]
pub(in crate::x64::tests) fn test_find_loop_hoists_conditional_body_still_detected() {
    // The detector deliberately hoists a sequence that sits BEHIND a
    // conditional inside the body (`for(..){ if (m != null) m[j]... }`) —
    // and the loop may also be zero-trip at runtime. That is only sound
    // because the EMISSION now guards the hoisted load with null+bounds
    // checks routed to the reason-2 deopt stub (see the preheader "LICM:
    // Emit hoisted aaload" block and test_classes/LicmHoistRepro.java);
    // before those guards this shape crashed the VM on m == null. This
    // test pins the detector contract so an emission-side reader knows
    // conditional/zero-trip shapes DO reach the guarded preheader.
    //
    // Locals: 0=m (Object[]), 1=j, 2=n, 3=i.
    //   0: iconst_0 ; 1: istore_3                    (i = 0)
    //   2: iload_3 ; 3: iload_2 ; 4: if_icmpge → 21  (header)
    //   7: aload_0 ; 8: ifnull → 15                  (skip if m == null)
    //  11: aload_0 ; 12: iload_1 ; 13: aaload ; 14: pop
    //  15: iinc 3, 1 ; 18: goto → 2 ; 21: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3
        0x1d, // 2: iload_3 (header)
        0x1c, // 3: iload_2
        0xa2, 0x00, 0x11, // 4: if_icmpge +17 → 21
        0x2a, // 7: aload_0
        0xc6, 0x00, 0x07, // 8: ifnull +7 → 15
        0x2a, // 11: aload_0
        0x1b, // 12: iload_1
        0x32, // 13: aaload
        0x57, // 14: pop
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf0, // 18: goto -16 → 2
        0xb1, // 21: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (2, 18), "loop should be (2, 18), got {loops:?}");

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "conditional-body aaload is hoisted");
    assert_eq!(hoists[0].loop_header, 2);
    assert_eq!(hoists[0].seq_start, 11);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 1);
}

#[test]
pub(in crate::x64::tests) fn test_match_invariant_aaload() {
    // aload_0; iload_3; aaload
    let code: Vec<u8> = vec![0x2a, 0x1d, 0x32];
    let modified = 0u64; // nothing modified
    let result = match_invariant_aaload(&code, 0, modified, 3);
    assert_eq!(result, Some((0, 3, 3))); // array=0, index=3, seq_end=3

    // Same but local 0 is modified → should NOT match
    let modified2 = 1u64 << 0;
    let result2 = match_invariant_aaload(&code, 0, modified2, 3);
    assert_eq!(result2, None);

    // aload_1; iload (wide) 4; aaload
    let code2: Vec<u8> = vec![0x2b, 0x15, 0x04, 0x32];
    let result3 = match_invariant_aaload(&code2, 0, 0, 4);
    assert_eq!(result3, Some((1, 4, 4)));
}

#[test]
pub(in crate::x64::tests) fn test_find_loop_hoists_matmul_pattern() {
    // Simulate matmul inner loop pattern:
    // header=2: iload 7; iload 2; if_icmpge exit;
    //           aload_0; iload 4; aaload;     ← INVARIANT (a[i])
    //           iload 7; iaload;              sum += a[i][k]
    //           iinc 7,1; goto header
    //
    // Modified: local 7 (k) via iinc
    // Invariant: local 0 (a) and local 4 (i)
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (k=0)
        0x36, 0x07, // 1: istore 7
        // header at 3:
        0x15, 0x07, // 3: iload 7
        0x1c, // 5: iload_2 (n)
        0xa2, 0x00, 0x0e, // 6: if_icmpge +14 → 20
        0x2a, // 9: aload_0 (a) ← SEQ START
        0x15, 0x04, // 10: iload 4 (i)
        0x32, // 12: aaload  ← SEQ END (13)
        0x15, 0x07, // 13: iload 7 (k)
        0x2e, // 15: iaload (a[i][k])
        0x84, 0x07, 0x01, // 16: iinc 7, 1
        0xa7, 0xff, 0xf0, // 19: goto -16 → 3
        0x1a, // 22: iload_0
        0xac, // 23: ireturn
        0, 0,
    ];
    let loops = detect_loops(&code, 24);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (3, 19)); // header=3, back=19

    let hoists = find_loop_hoists(&code, 24, &loops);
    assert_eq!(hoists.len(), 1);
    assert_eq!(hoists[0].loop_header, 3);
    assert_eq!(hoists[0].seq_start, 9);
    assert_eq!(hoists[0].seq_end, 13);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 4);
}

#[test]
pub(in crate::x64::tests) fn detects_exact_guarded_matrix_dot_product_loop() {
    // CratonBench.matmul's javac bytecode at bci 31..62.
    let mut code = vec![0u8; 31];
    code.extend_from_slice(&[
        0x15, 0x07, // 31: iload 7 (k)
        0x1c, // 33: iload_2 (n)
        0xa2, 0x00, 0x1d, // 34: if_icmpge 63
        0x15, 0x06, // 37: iload 6 (sum)
        0x2a, // 39: aload_0 (a)
        0x15, 0x04, // 40: iload 4 (row)
        0x32, // 42: aaload
        0x15, 0x07, // 43: iload 7 (k)
        0x2e, // 45: iaload
        0x2b, // 46: aload_1 (b)
        0x15, 0x07, // 47: iload 7 (k)
        0x32, // 49: aaload
        0x15, 0x05, // 50: iload 5 (column)
        0x2e, // 52: iaload
        0x68, // 53: imul
        0x60, // 54: iadd
        0x36, 0x06, // 55: istore 6
        0x84, 0x07, 0x01, // 57: iinc 7,1
        0xa7, 0xff, 0xe3, // 60: goto 31
    ]);

    let dot = detect_matrix_dot_loop(&code, 31, 60, 7).expect("matrix dot loop");
    assert_eq!(
        dot,
        MatrixDotLoop {
            header_pc: 31,
            back_edge_pc: 60,
            iv_local: 7,
            bound_local: 2,
            acc_local: 6,
            a_outer_local: 0,
            a_row_local: 4,
            b_outer_local: 1,
            b_column_local: 5,
        }
    );

    // Any side effect or different arithmetic body must stay on the exact
    // scalar emitter; fallback restart would not be valid for it.
    code[53] = 0x4f; // iastore instead of imul
    assert!(detect_matrix_dot_loop(&code, 31, 60, 7).is_none());
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_licm_loop_sum_with_aaload() {
    // Test that LICM correctly hoists an aaload out of a loop.
    //
    // Java equivalent:
    //   int sum(int[][] a, int i, int n) {
    //       int sum = 0;
    //       for (int k = 0; k < n; k++) {
    //           sum += a[i][k];  // a[i] is loop-invariant
    //       }
    //       return sum;
    //   }
    //
    // Params: a(local 0), i(local 1), n(local 2)
    // Locals: sum(local 3), k(local 4)
    // goto at PC 23 → target PC 5: offset = 5 - 23 = -18 = 0xFFEE
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (sum=0)
        0x3e, // 1: istore_3
        0x03, // 2: iconst_0 (k=0)
        0x36, 0x04, // 3: istore 4
        // header at 5:
        0x15, 0x04, // 5: iload 4 (k)
        0x1c, // 7: iload_2 (n)
        0xa2, 0x00, 0x12, // 8: if_icmpge +18 → 26
        0x1d, // 11: iload_3 (sum)
        0x2a, // 12: aload_0 (a) ← HOIST
        0x1b, // 13: iload_1 (i)
        0x32, // 14: aaload (a[i])
        0x15, 0x04, // 15: iload 4 (k)
        0x2e, // 17: iaload (a[i][k])
        0x60, // 18: iadd
        0x3e, // 19: istore_3
        0x84, 0x04, 0x01, // 20: iinc 4, 1
        0xa7, 0xff, 0xee, // 23: goto -18 → 5
        0x1d, // 26: iload_3
        0xac, // 27: ireturn
        0, 0,
    ];
    let code_len = 28;

    // Verify LICM detection
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (5, 23));

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1);
    assert_eq!(hoists[0].loop_header, 5);
    assert_eq!(hoists[0].seq_start, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 1);

    // Compile and verify correctness
    assert!(is_jit_compatible(&code, code_len, "([[III)I"));

    // Create arrays: a = int[3][4], a[1] = {10, 20, 30, 40}
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

    let heap = GenerationalHeap::new();

    // Create int[] row = {10, 20, 30, 40}
    let row = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
    let _ = heap.set_array_element(row, 0, Value::Int(10));
    let _ = heap.set_array_element(row, 1, Value::Int(20));
    let _ = heap.set_array_element(row, 2, Value::Int(30));
    let _ = heap.set_array_element(row, 3, Value::Int(40));

    // Create Object[] (int[][]) outer array with 3 rows
    let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = heap.set_array_element(outer, 1, Value::Object(Some(row)));

    // Compile with heap (needs_heap for arrays)
    let compiled = compile(
        &code,
        code_len,
        3,
        5,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // Call: sum(a=outer, i=1, n=4) → should sum row[0..4] = 10+20+30+40 = 100
    let heap_ptr = &heap as *const _ as i64; // Cast: function pointer for JIT call target
                                             // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
                                             // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 4]) }; // Cast: JIT ABI convention
    assert_eq!(result, 100);

    // Call with n=2 → sum first 2 elements: 10+20 = 30
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result2 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 2]) }; // Cast: JIT ABI convention
    assert_eq!(result2, 30);

    // Call with n=0 → sum nothing = 0
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result3 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 0]) }; // Cast: JIT ABI convention
    assert_eq!(result3, 0);
}

#[test]
pub(in crate::x64::tests) fn test_licm_no_hoist_when_modified() {
    // When the index local is modified inside the loop, aaload should NOT be hoisted.
    // loop body: aload_0; iload_1; aaload; ...; iinc 1, 1
    // local 1 is modified → no hoist
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (header)
        0x1b, // 1: iload_1
        0x32, // 2: aaload
        0x57, // 3: pop
        0x84, 0x01, 0x01, // 4: iinc 1, 1
        0xa7, 0xff, 0xf9, // 7: goto -7 → 0
        0, 0,
    ];
    let loops = detect_loops(&code, 10);
    assert_eq!(loops.len(), 1);
    let hoists = find_loop_hoists(&code, 10, &loops);
    assert_eq!(hoists.len(), 0); // no hoist because local 1 is modified
}

#[test]
pub(in crate::x64::tests) fn test_licm_no_hoist_with_aastore() {
    // When the loop contains aastore (0x53), aaload should NOT be hoisted
    // (conservative safety: aastore could modify the array being read)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (header)
        0x1b, // 1: iload_1
        0x32, // 2: aaload
        0x57, // 3: pop
        0x53, // 4: aastore (tests analysis only)
        0xa7, 0xff, 0xfa, // 5: goto -6 → 0
        0, 0,
    ];
    // Test the LICM analysis directly (aastore isn't JIT-compilable, but
    // find_loop_hoists analyzes bytecode patterns independently)
    let loops = vec![(0usize, 5usize)]; // header=0, back=5
    let hoists = find_loop_hoists(&code, 8, &loops);
    assert_eq!(hoists.len(), 0); // no hoist because aastore present
}

#[test]
pub(in crate::x64::tests) fn test_licm_nested_loops() {
    // Nested loops: outer (j) and inner (k).
    // aload_0; iload 4; aaload is invariant in BOTH loops.
    // Hoisted to the INNER loop (header=6): an outer-loop hoist would make the inner header OSR-ineligible (round 9 wave 8, licm::span_holds_inner_osr_entry).
    //
    // Modified in outer: local 5 (j) via iinc, local 7 (k) via iinc
    // Modified in inner: local 7 (k) via iinc
    // Invariant in both: local 0 (a) and local 4 (i)
    //
    // Layout:
    //   PC 0: outer header
    //   PC 6: inner header
    //   PC 12-15: aload_0; iload 4; aaload (invariant)
    //   PC 20: inner back-edge (goto PC 6, offset = 6-20 = -14 = 0xFFF2)
    //   PC 23: outer iinc j
    //   PC 26: outer back-edge (goto PC 0, offset = 0-26 = -26 = 0xFFE6)
    let code: Vec<u8> = vec![
        0x15, 0x05, // 0: iload 5 (j)
        0x1c, // 2: iload_2 (n)
        0xa2, 0x00, 0x17, // 3: if_icmpge +23 → 26
        0x15, 0x07, // 6: iload 7 (k)
        0x1c, // 8: iload_2 (n)
        0xa2, 0x00, 0x0a, // 9: if_icmpge +10 → 19
        0x2a, // 12: aload_0    ← INVARIANT
        0x15, 0x04, // 13: iload 4 (i)
        0x32, // 15: aaload
        0x57, // 16: pop
        0x84, 0x07, 0x01, // 17: iinc 7, 1
        0xa7, 0xff, 0xf2, // 20: goto -14 → 6
        0x84, 0x05, 0x01, // 23: iinc 5, 1
        0xa7, 0xff, 0xe6, // 26: goto -26 → 0
        0, 0,
    ];
    let code_len = 29;

    // Use detect_loops to find both loops
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 2);

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1);
    // Hoisted to the INNER loop (header=6), not the outer one (header=0).
    assert_eq!(hoists[0].loop_header, 6);
    assert_eq!(hoists[0].seq_start, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 4);
}

#[test]
pub(in crate::x64::tests) fn test_arith_licm_invariant_expr() {
    // Counted loop where `base*3 + 11` (base = local 0, never stored in
    // the loop) is loop-invariant and should be hoisted.
    //
    //   PC 0: iload 4 (i)           — loop header / cond
    //   PC 2: iload_1 (n)
    //   PC 3: if_icmpge +N          — loop exit
    //   PC 6: iload_0 (base)        ← invariant run START
    //   PC 7: iconst_3
    //   PC 8: imul
    //   PC 9: bipush 11
    //   PC 11: iadd                 ← invariant run END (seq_end = 12)
    //   PC 12: istore 5
    //   PC 14: iinc 4, 1
    //   PC 17: goto -17 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x11, // 3: if_icmpge +17 → 20
        0x1a, // 6: iload_0 (base)   ← invariant
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x10, 0x0b, // 9: bipush 11
        0x60, // 11: iadd
        0x36, 0x05, // 12: istore 5
        0x84, 0x04, 0x01, // 14: iinc 4, 1
        0xa7, 0xff, 0xef, // 17: goto -17 → 0
        0, 0,
    ];
    let code_len = 20;

    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);

    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "base*3+11 must be hoisted");
    assert_eq!(hoists[0].loop_header, 0);
    assert_eq!(hoists[0].seq_start, 6);
    assert_eq!(hoists[0].seq_end, 12);
    // Expression: iload_0, iconst_3, imul, bipush 11, iadd → 5 steps.
    assert_eq!(hoists[0].steps.len(), 5);
    assert!(matches!(hoists[0].steps[0], ArithStep::PushLocal(0)));
    assert!(matches!(hoists[0].steps[1], ArithStep::PushConst(3)));
    assert!(matches!(hoists[0].steps[2], ArithStep::BinOp(0x68)));
    assert!(matches!(hoists[0].steps[3], ArithStep::PushConst(11)));
    assert!(matches!(hoists[0].steps[4], ArithStep::BinOp(0x60)));
    assert_eq!(arith_expr_max_depth(&hoists[0].steps), 2);
}

#[test]
pub(in crate::x64::tests) fn test_arith_licm_no_hoist_across_invoke_loop() {
    // The arithmetic itself is invariant, but a loop that invokes Java is
    // re-entrant: its synthetic LICM frame value must not be kept across
    // that call boundary. This is the shape exercised by Lucene's
    // recursive Sorter.mergeInPlace loop.
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x14, // 3: if_icmpge -> 23
        0x1a, // 6: iload_0 (invariant)
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x10, 0x0b, // 9: bipush 11
        0x60, // 11: iadd
        0x36, 0x05, // 12: istore 5
        0xb8, 0x00, 0x01, // 14: invokestatic #1
        0x84, 0x04, 0x01, // 17: iinc 4, 1
        0xa7, 0xff, 0xec, // 20: goto -> 0
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);
    assert!(
        find_arith_loop_hoists(&code, code_len, &loops).is_empty(),
        "a loop containing invoke* must keep arithmetic in bytecode order"
    );
}

#[test]
pub(in crate::x64::tests) fn test_arith_licm_no_hoist_when_operand_modified() {
    // Same shape, but `base` (local 0) IS stored inside the loop, so the
    // expression is NOT loop-invariant and must not be hoisted.
    //
    //   PC 0: iload 4              — header
    //   PC 2: iload_1
    //   PC 3: if_icmpge +N
    //   PC 6: iload_0 (base)
    //   PC 7: iconst_3
    //   PC 8: imul
    //   PC 9: istore_0 (base)      ← base modified → not invariant
    //   PC 10: iinc 4, 1
    //   PC 13: goto -13 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x0d, // 3: if_icmpge +13 → 16
        0x1a, // 6: iload_0
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x3b, // 9: istore_0  ← modifies base
        0x84, 0x04, 0x01, // 10: iinc 4, 1
        0xa7, 0xff, 0xf3, // 13: goto -13 → 0
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "expr on a loop-modified local must not hoist"
    );
}

#[test]
pub(in crate::x64::tests) fn test_arith_licm_no_hoist_when_operand_modified_by_wide_iinc() {
    // The same refusal, through the `wide` prefix.
    //
    // AUDIT 2026-09-05. `find_modified_locals` had an arm for `iinc`
    // (0x84) and none for `wide` (0xc4), so `wide iinc` fell to the
    // length-only default: the walk stayed aligned and the local was
    // never marked modified. javac emits `wide iinc` whenever the delta
    // does not fit in a signed byte -- `i += 1024` -- so the induction
    // variable of every such loop read as loop-INVARIANT and
    // `i + <invariant>` got hoisted into the pre-header. Stride 1 was
    // correct and stride 1024 was not.
    //
    //   PC 0: iload 4              — header
    //   PC 2: iload_1
    //   PC 3: if_icmpge +N
    //   PC 6: iload 4  (the INDUCTION VARIABLE, not a constant)
    //   PC 8: iload_0
    //   PC 9: iadd                 ← i + base, must NOT hoist
    //   PC 10: istore_2
    //   PC 11: wide iinc 4, 1024   ← modifies local 4
    //   PC 17: goto -17 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x11, // 3: if_icmpge +17 → 20
        0x15, 0x04, // 6: iload 4   ← the induction variable
        0x1a, // 8: iload_0
        0x60, // 9: iadd
        0x3d, // 10: istore_2
        0xc4, 0x84, 0x00, 0x04, 0x04, 0x00, // 11: wide iinc 4, 1024
        0xa7, 0xff, 0xef, // 17: goto -17 → 0
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "an expression over a local incremented by WIDE iinc is not \
         loop-invariant and must not hoist; got {} hoist(s)",
        hoists.len()
    );
}

/// `wide` stores mark their local too, not just `wide iinc`.
#[test]
pub(in crate::x64::tests) fn test_find_modified_locals_sees_every_wide_form() {
    use crate::x64::escape_analysis::find_modified_locals;
    // wide istore 4      c4 36 00 04
    let m = find_modified_locals(&[0xc4, 0x36, 0x00, 0x04], 0, 4);
    assert_eq!(m, 1 << 4, "wide istore must mark its local");
    // wide astore 7      c4 3a 00 07
    let m = find_modified_locals(&[0xc4, 0x3a, 0x00, 0x07], 0, 4);
    assert_eq!(m, 1 << 7, "wide astore must mark its local");
    // wide iinc 4, 1024  c4 84 00 04 04 00
    let m = find_modified_locals(&[0xc4, 0x84, 0x00, 0x04, 0x04, 0x00], 0, 6);
    assert_eq!(m, 1 << 4, "wide iinc must mark its local");
    // wide iload 4 writes nothing.
    let m = find_modified_locals(&[0xc4, 0x15, 0x00, 0x04], 0, 4);
    assert_eq!(m, 0, "a wide LOAD must not mark anything");
}

#[test]
pub(in crate::x64::tests) fn test_arith_licm_excludes_idiv() {
    // idiv (0x6c) can throw ArithmeticException — it must never be part
    // of a hoisted run. `base / 3` here should yield no hoist.
    //   PC 0..5: header + cond (as above)
    //   PC 6: iload_0; PC 7: iconst_3; PC 8: idiv
    //   PC 9: istore 5 ...
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x0e, // 3: if_icmpge → 17
        0x1a, // 6: iload_0
        0x06, // 7: iconst_3
        0x6c, // 8: idiv  ← faulting, not hoistable
        0x36, 0x05, // 9: istore 5
        0x84, 0x04, 0x01, // 11: iinc 4, 1
        0xa7, 0xff, 0xf2, // 14: goto -14 → 0
        0, 0,
    ];
    let code_len = 17;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(hoists.is_empty(), "idiv must not be hoisted (can fault)");
}

#[test]
pub(in crate::x64::tests) fn test_aconst_null() {
    // Method: long f() { return null; }  (aconst_null, areturn)
    let code: Vec<u8> = vec![0x01, 0xb0, 0, 0];
    let code_len = 2;
    assert!(is_jit_compatible(&code, code_len, "()Ljava/lang/Object;"));
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
pub(in crate::x64::tests) fn test_ifnull_taken() {
    // Method: int f(Object a) { if (a == null) return 1; return 0; }
    // aload_0, ifnull +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc6, 0x00, 0x05, // 1: ifnull +5 → PC 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0,
    ];
    let code_len = 8;
    assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // null input → branch taken → returns 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") };
    assert_eq!(result, 1);
    // non-null input → branch not taken → returns 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
pub(in crate::x64::tests) fn test_ifnonnull_taken() {
    // Method: int f(Object a) { if (a != null) return 1; return 0; }
    // aload_0, ifnonnull +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc7, 0x00, 0x05, // 1: ifnonnull +5 → PC 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0,
    ];
    let code_len = 8;
    assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // non-null input → branch taken → returns 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(result, 1);
    // null input → branch not taken → returns 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
pub(in crate::x64::tests) fn test_if_acmpeq() {
    // Method: int f(Object a, Object b) { if (a == b) return 1; return 0; }
    // aload_0, aload_1, if_acmpeq +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xa5, 0x00, 0x05, // 2: if_acmpeq +5 → PC 7
        0x03, // 5: iconst_0
        0xac, // 6: ireturn
        0x04, // 7: iconst_1
        0xac, // 8: ireturn
        0, 0,
    ];
    let code_len = 9;
    assert!(is_jit_compatible(
        &code,
        code_len,
        "(Ljava/lang/Object;Ljava/lang/Object;)I"
    ));
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Same ref → branch taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 100]).expect("test JIT call") };
    assert_eq!(result, 1);
    // Different refs → not taken → 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 200]).expect("test JIT call") };
    assert_eq!(result, 0);
    // Both null → taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0, 0]).expect("test JIT call") };
    assert_eq!(result, 1);
}

