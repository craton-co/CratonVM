use super::{
    ir_osr_guard_exit_entries_enabled, lower_inner_with_mode_exit_polls, IrPollModeExits,
    PollExitBytecode,
};
use crate::deopt::{DeoptReason, FrameValue};
use crate::ir::{IrBuilder, IrInlineFrameSites};
use crate::{CompiledMethod, JitRuntimeHelpers};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

/// The stop-the-world byte, always raised: every poll takes its slow path.
static FLAG: u8 = 1;

thread_local! {
    /// The `body_id` of every slow-path call made on this thread since
    /// the last `run`; its length is the number of calls (wave 18).
    static CALLS: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    /// `(from, verdict)`: from the 0-based call `from` on, the helper
    /// answers `verdict` — the innermost-frame bit unless a test sets
    /// another (wave 18: the polling-body bit a method-entry poll tests).
    static LEAVE_FROM: Cell<(i64, i64)> =
        const { Cell::new((i64::MAX, crate::SAFEPOINT_VERDICT_INNERMOST_FRAME)) };
}

/// `HelperFnSafepointSlowPath`: `(flag_addr, body_id) -> verdict`.
extern "C" fn verdict_poll(_flag_addr: i64, body_id: i64) -> i64 {
    let n = CALLS.with(|ids| {
        let mut ids = ids.borrow_mut();
        ids.push(body_id);
        ids.len() - 1
    });
    let (from, verdict) = LEAVE_FROM.with(Cell::get);
    // Cast: a test's call count is far below i64::MAX.
    if n as i64 >= from {
        verdict
    } else {
        0
    }
}

/// Answer `verdict` (from `run`'s chosen call on) until changed.
fn set_verdict(verdict: i64) {
    LEAVE_FROM.with(|c| c.set((c.get().0, verdict)));
}

/// Did every slow-path call since the last `run` pass `body_id`?
fn every_call_named(body_id: i64) -> bool {
    CALLS.with(|ids| ids.borrow().iter().all(|&id| id == body_id))
}

/// `static int f() { int i = 0; while (i < 1000) i++; return i; }`, javac's
/// `goto` shape: header at 2, the back edge at 12.
const COUNT: [u8; 19] = [
    0x03, // 0:  iconst_0
    0x3b, // 1:  istore_0
    0x1a, // 2:  iload_0             <- loop header
    0x11, 0x03, 0xe8, // 3:  sipush 1000
    0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
    0x84, 0x00, 0x01, // 9:  iinc 0, 1
    0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
    0x1a, // 15: iload_0
    0xac, // 16: ireturn
    0, 0,
];

/// `static int g(int n) { int s = 0; for (int i = 0; i < n; i++) s += i;
/// return s; }`: two loop-carried locals and an invariant one, header at 4.
const SUM: [u8; 23] = [
    0x03, // 0:  iconst_0
    0x3c, // 1:  istore_1
    0x03, // 2:  iconst_0
    0x3d, // 3:  istore_2
    0x1c, // 4:  iload_2             <- loop header
    0x1a, // 5:  iload_0
    0xa2, 0x00, 0x0d, // 6:  if_icmpge +13 -> 19
    0x1b, // 9:  iload_1
    0x1c, // 10: iload_2
    0x60, // 11: iadd
    0x3c, // 12: istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0xa7, 0xff, 0xf4, // 16: goto -12 -> 4
    0x1b, // 19: iload_1
    0xac, // 20: ireturn
    0, 0,
];

/// `do { s += i; i++; } while (i < n)`: the back edge is the CONDITIONAL
/// `if_icmplt` at 13. The IR gives its taken edge a `Proj(0)` block of its
/// own, which every validated layout puts after the branch, so the poll runs there,
/// after the branch decided, on an unconditional edge to the header at 4.
const DO_WHILE: [u8; 20] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2
    0x1b, // 4: iload_1              <- loop header
    0x1c, // 5: iload_2
    0x60, // 6: iadd
    0x3c, // 7: istore_1
    0x84, 0x02, 0x01, // 8: iinc 2, 1
    0x1c, // 11: iload_2
    0x1a, // 12: iload_0
    0xa1, 0xff, 0xf7, // 13: if_icmplt -9 -> 4
    0x1b, // 16: iload_1
    0xac, // 17: ireturn
    0, 0,
];

fn lower(code: &[u8], params: usize, locals: usize, osr_door: bool) -> CompiledMethod {
    let mode = if osr_door {
        IrPollModeExits::OsrDoor
    } else {
        IrPollModeExits::Off
    };
    // Cast: helper address
    lower_with(
        code,
        params,
        locals,
        mode,
        verdict_poll as *const () as usize,
    )
}

/// [`lower`] with the mode and the slow-path helper spelled out.
fn lower_with(
    code: &[u8],
    params: usize,
    locals: usize,
    mode: IrPollModeExits,
    slow_path: usize,
) -> CompiledMethod {
    let graph = IrBuilder::new(params, locals)
        .build(code, code.len() - 2)
        .expect("IR build");
    let schedule = crate::ir_schedule::schedule(&graph);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the poll.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = slow_path;
    lower_inner_with_mode_exit_polls(
        &crate::DirectHelperTable::EMPTY,
        &graph,
        &schedule,
        params,
        locals,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &IrInlineFrameSites::default(),
        &HashMap::new(),
        &HashSet::new(),
        None,
        &[],
        Some(&[]),
        mode,
        // Wave 18: the bytecode liveness a mode exit may ask. Every local
        // of these shapes is live at its header, so it changes nothing.
        Some(PollExitBytecode {
            code,
            code_len: code.len() - 2,
            num_params: params,
            num_locals: locals,
            handlers: &[],
        }),
        None,
    )
    .expect("the loop lowers")
}

/// Run `cm` with the verdict turning non-zero from slow-path call
/// `leave_from` on; the raw result and the calls made.
fn run(cm: &CompiledMethod, args: &[i64], leave_from: i64) -> (i64, i64) {
    CALLS.with(|ids| ids.borrow_mut().clear());
    LEAVE_FROM.with(|c| c.set((leave_from, c.get().1)));
    let _ = crate::deopt::take_last_deopt_with_point();
    // SAFETY: the body was lowered by this test from valid bytecode for
    // exactly these int arguments, and calls only `verdict_poll`.
    let raw = unsafe { cm.try_call(args) }.expect("call");
    LEAVE_FROM.with(|c| c.set((i64::MAX, c.get().1)));
    // Cast: a test's call count is far below i64::MAX.
    (raw, CALLS.with(|ids| ids.borrow().len() as i64))
}

fn poll_exits(cm: &CompiledMethod) -> usize {
    cm._deopt_point_boxes
        .iter()
        .filter(|p| p.reason == DeoptReason::OsrExit)
        .count()
}

fn enabled() -> bool {
    crate::deopt_real_enabled()
        && crate::x64::jit_safepoint_polls_enabled()
        && ir_osr_guard_exit_entries_enabled()
}

struct Shape {
    code: &'static [u8],
    params: usize,
    locals: usize,
    args: Vec<i64>,
    header: u32,
    trips: i64,
    result: i32,
    /// The locals the header receives from the k-th back edge (1-based).
    at: fn(i64) -> Vec<i64>,
}

/// For each shape and for the 1st and 6th back edge: the exit happens at
/// exactly that poll (no poll after it), resumes at the header, and carries
/// exactly the locals the back edge carries; the planless resume accepts
/// it without an admitted bci.
#[test]
#[cfg(target_arch = "x86_64")]
fn an_osr_door_body_leaves_at_the_polling_back_edge_with_exact_locals() {
    if !enabled() {
        return;
    }
    let shapes = [
        Shape {
            code: &COUNT,
            params: 0,
            locals: 1,
            args: Vec::new(),
            header: 2,
            trips: 1000,
            result: 1000,
            at: |k| vec![k],
        },
        Shape {
            code: &SUM,
            params: 1,
            locals: 3,
            args: vec![100],
            header: 4,
            trips: 100,
            result: 4950,
            at: |k| vec![100, k * (k - 1) / 2, k],
        },
        // 100 body runs, 99 taken back edges; after the k-th, s is the sum
        // of 0..k and i is k, as in `SUM`.
        Shape {
            code: &DO_WHILE,
            params: 1,
            locals: 3,
            args: vec![100],
            header: 4,
            trips: 99,
            result: 4950,
            at: |k| vec![100, k * (k - 1) / 2, k],
        },
    ];
    for shape in &shapes {
        let cm = lower(shape.code, shape.params, shape.locals, true);
        assert_eq!(poll_exits(&cm), 1, "one unconditional back edge, one exit");
        assert!(crate::osr_exit::has_poll_mode_exits(&cm._deopt_point_boxes));
        // The exit is not an admission fact: the body keeps the facts it
        // has without it.
        assert!(cm.ir_osr_sentinel_free, "still sentinel-free for the door");
        if let Some(admitted) = cm.ir_osr_guard_exit_bcis.as_deref() {
            assert!(
                !admitted.contains(&shape.header),
                "the header is not admitted"
            );
        }
        // Verdict never: the loop completes; the calls it made are the
        // method-entry poll(s) plus one per back edge.
        let (raw, calls) = run(&cm, &shape.args, i64::MAX);
        // Cast: this tier's `int` result is the low 32 bits of RAX.
        assert_eq!(raw as i32, shape.result);
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        let entry_polls = calls - shape.trips;
        assert!(
            entry_polls >= 0,
            "{calls} polls for {} back edges",
            shape.trips
        );
        for k in [1i64, 6] {
            let (raw, calls) = run(&cm, &shape.args, entry_polls + k - 1);
            assert_eq!(raw, i64::MIN, "the exit returns the deopt sentinel");
            assert_eq!(
                calls,
                entry_polls + k,
                "left at back edge {k}, polled no more"
            );
            let (frame, cause, point) =
                crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
            assert_eq!(frame.bci, shape.header, "resumes at the loop header");
            assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
            assert!(cm.owns_deopt_point(point), "one of this artifact's points");
            assert!(frame.stack.is_empty());
            let want: Vec<FrameValue> =
                (shape.at)(k).into_iter().map(FrameValue::Int).collect();
            assert_eq!(frame.locals, want, "exact locals at back edge {k}");
            assert_eq!(
                crate::osr_exit::guard_exit_resume_bci(
                    &cm._deopt_point_boxes,
                    cm.ir_osr_guard_exit_bcis.as_deref(),
                    point,
                    frame.bci,
                ),
                Ok(shape.header as usize),
                "the planless transfer resumes it at the header"
            );
        }
    }
}

/// A compile that seeks no poll mode exit ([`IrPollModeExits::Off`]: a
/// method the method-entry admission refuses, or the exits switched off)
/// ignores the verdict and records no exit. (Until wave 18 this was every
/// method-entry compile.)
#[test]
#[cfg(target_arch = "x86_64")]
fn a_compile_without_poll_mode_exits_leaves_nothing() {
    if !enabled() {
        return;
    }
    for (code, params, locals, args, result) in [
        (&COUNT[..], 0usize, 1usize, Vec::new(), 1000i32),
        (&DO_WHILE[..], 1, 3, vec![100i64], 4950),
    ] {
        let entry = lower(code, params, locals, false);
        assert_eq!(poll_exits(&entry), 0);
        assert!(!crate::osr_exit::has_poll_mode_exits(
            &entry._deopt_point_boxes
        ));
        for verdict in [
            crate::SAFEPOINT_VERDICT_INNERMOST_FRAME,
            crate::SAFEPOINT_VERDICT_INNERMOST_FRAME | crate::SAFEPOINT_VERDICT_POLLING_BODY,
        ] {
            set_verdict(verdict);
            let (raw, _) = run(&entry, &args, 0);
            // Cast: this tier's `int` result is the low 32 bits of RAX.
            assert_eq!(
                raw as i32, result,
                "a verdict of {verdict} from the first call on is ignored"
            );
            assert!(crate::deopt::take_last_deopt_with_point().is_none());
            assert!(every_call_named(0), "a poll with no exit names no body");
        }
        set_verdict(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME);
    }
}


/// Interpreter round i1 wave 18, lane L2
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, IR item (c)):
/// an admitted METHOD-ENTRY compile's unconditional back-edge poll leaves
/// at its loop header on `SAFEPOINT_VERDICT_POLLING_BODY`, from exactly
/// the poll whose verdict carries that bit, with exactly the locals that
/// back edge carries, in a frame every stash sink resumes as a new one;
/// the innermost-frame bit alone (about some caller's interpreter frame)
/// is ignored; and every slow-path call names the body by its compile id
/// (0 when none was reserved), as the single-pass method-entry poll does.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_method_entry_body_leaves_at_the_polling_back_edge_on_the_polling_body_bit() {
    if !crate::deopt_real_enabled() || !crate::x64::jit_safepoint_polls_enabled() {
        return;
    }
    let innermost = crate::SAFEPOINT_VERDICT_INNERMOST_FRAME;
    let polling = crate::SAFEPOINT_VERDICT_POLLING_BODY;
    let shapes = [
        Shape {
            code: &COUNT,
            params: 0,
            locals: 1,
            args: Vec::new(),
            header: 2,
            trips: 1000,
            result: 1000,
            at: |k| vec![k],
        },
        Shape {
            code: &SUM,
            params: 1,
            locals: 3,
            args: vec![100],
            header: 4,
            trips: 100,
            result: 4950,
            at: |k| vec![100, k * (k - 1) / 2, k],
        },
        Shape {
            code: &DO_WHILE,
            params: 1,
            locals: 3,
            args: vec![100],
            header: 4,
            trips: 99,
            result: 4950,
            at: |k| vec![100, k * (k - 1) / 2, k],
        },
    ];
    for shape in &shapes {
        let cm = lower_with(
            shape.code,
            shape.params,
            shape.locals,
            IrPollModeExits::MethodEntry,
            // Cast: helper address
            verdict_poll as *const () as usize,
        );
        assert_eq!(poll_exits(&cm), 1, "one unconditional back edge, one exit");
        let body = i64::from(cm.compile_id);
        let names_the_body = || every_call_named(body);

        // Verdict never: the loop completes.
        let (raw, calls) = run(&cm, &shape.args, i64::MAX);
        // Cast: this tier's `int` result is the low 32 bits of RAX.
        assert_eq!(raw as i32, shape.result);
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        assert!(names_the_body(), "every slow-path call names the body");
        let entry_polls = calls - shape.trips;
        assert!(
            entry_polls >= 0,
            "{calls} polls for {} back edges",
            shape.trips
        );

        // The innermost-frame bit alone, from the first call on: ignored.
        set_verdict(innermost);
        let (raw, ignored_calls) = run(&cm, &shape.args, 0);
        // Cast: as above.
        assert_eq!(
            raw as i32, shape.result,
            "the innermost-frame bit is ignored"
        );
        assert_eq!(ignored_calls, calls);
        assert!(crate::deopt::take_last_deopt_with_point().is_none());

        for verdict in [polling, polling | innermost] {
            set_verdict(verdict);
            for k in [1i64, 6] {
                let (raw, calls) = run(&cm, &shape.args, entry_polls + k - 1);
                assert_eq!(raw, i64::MIN, "the exit returns the deopt sentinel");
                assert_eq!(
                    calls,
                    entry_polls + k,
                    "left at back edge {k}, polled no more"
                );
                assert!(names_the_body());
                let (frame, cause, point) = crate::deopt::take_last_deopt_with_point()
                    .expect("the exit stashes a frame");
                assert_eq!(frame.bci, shape.header, "resumes at the loop header");
                assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
                assert!(cm.owns_deopt_point(point), "one of this artifact's points");
                assert!(frame.semantics.reexecute, "re-executes the header");
                assert!(frame.caller_frames.is_empty() && frame.monitors.is_empty());
                assert!(frame.stack.is_empty());
                let want: Vec<FrameValue> =
                    (shape.at)(k).into_iter().map(FrameValue::Int).collect();
                assert_eq!(frame.locals, want, "exact locals at back edge {k}");
                let p = cm
                    ._deopt_point_boxes
                    .iter()
                    .find(|p| {
                        let p: &crate::deopt::DeoptimizationPoint = p;
                        p as *const crate::deopt::DeoptimizationPoint as usize == point
                    })
                    .expect("the stash names a box of this artifact");
                assert!(crate::osr_exit::guard_exit_point_resumable(p));
            }
        }
        set_verdict(innermost);
    }
}

/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of the i22-L2
/// caller-held proposal): a [`IrPollModeExits::MethodEntryNamedOnly`]
/// compile -- the trap-free, call-free synchronized shape -- leaves at its
/// header on `SAFEPOINT_VERDICT_NAMED_BODY` alone. The polling-body bit,
/// which the VM also sets for a body a caller-held site bound (and for one
/// it could not name), is ignored. The artifact is stamped
/// `named_poll_exits_only`; with no compile id nothing could name the body,
/// so no exit is sought.
#[test]
fn a_named_only_body_leaves_on_the_named_body_bit_alone() {
    if !crate::deopt_real_enabled() || !crate::x64::jit_safepoint_polls_enabled() {
        return;
    }
    let innermost = crate::SAFEPOINT_VERDICT_INNERMOST_FRAME;
    let polling = crate::SAFEPOINT_VERDICT_POLLING_BODY;
    let named = crate::SAFEPOINT_VERDICT_NAMED_BODY;
    let cm = lower_with(
        &COUNT,
        0,
        1,
        IrPollModeExits::MethodEntryNamedOnly,
        // Cast: helper address
        verdict_poll as *const () as usize,
    );
    if cm.compile_id == 0 {
        assert_eq!(poll_exits(&cm), 0, "no id, no exit");
        assert!(!cm.named_poll_exits_only);
        return;
    }
    assert_eq!(poll_exits(&cm), 1, "one unconditional back edge, one exit");
    assert!(cm.named_poll_exits_only, "its only box is the poll exit");
    assert!(cm.post_call_exit_sites.is_empty());
    let body = i64::from(cm.compile_id);

    let (raw, calls) = run(&cm, &[], i64::MAX);
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    assert_eq!(raw as i32, 1000);
    assert!(
        every_call_named(body),
        "every slow-path call names the body"
    );
    let entry_polls = calls - 1000;
    assert!(entry_polls >= 0);

    // Every bit but the named one, from the first call on: ignored.
    set_verdict(innermost | polling);
    let (raw, ignored_calls) = run(&cm, &[], 0);
    // Cast: as above.
    assert_eq!(raw as i32, 1000, "the polling-body bit is ignored");
    assert_eq!(ignored_calls, calls);
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    for verdict in [named, named | polling | innermost] {
        set_verdict(verdict);
        let k = 3i64;
        let (raw, calls) = run(&cm, &[], entry_polls + k - 1);
        assert_eq!(raw, i64::MIN, "the exit returns the deopt sentinel");
        assert_eq!(calls, entry_polls + k, "left at back edge {k}");
        let (frame, cause, point) =
            crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
        assert_eq!(frame.bci, 2, "resumes at the loop header");
        assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
        assert!(cm.owns_deopt_point(point));
        assert_eq!(frame.locals, vec![FrameValue::Int(k)]);
    }
    set_verdict(innermost);
}

/// Wave 18, lane L2: the method-entry admission is the VM stash sinks'
/// precise-resume rule (`deopt_resume::sink_precise_resume_allowed`): no
/// `ACC_SYNCHRONIZED`, no `monitorenter` / `monitorexit`, and the sink
/// resume switch on. A method it refuses compiles its polls blind. Wave
/// 19: a method with monitor bytecode is admitted on the sinks'
/// `can_deopt_resume` arm instead, which the lowering vouches for
/// (`i19_l2_monitor_block_exit_tests`). Wave 22: so is an
/// `ACC_SYNCHRONIZED` method, which the IR door vouches for
/// (`i22_l2_synchronized_and_debugger_tests`).
#[test]
fn the_method_entry_admission_is_the_sinks_resume_rule() {
    let on = crate::deopt_sink_resume_enabled();
    let synchronized_on =
        super::IR_SYNCHRONIZED_METHOD_MODE_EXITS_ENABLED && crate::deopt_real_enabled();
    assert_eq!(
        super::ir_entry_mode_exits_admitted(&COUNT, COUNT.len() - 2, false),
        on
    );
    assert_eq!(
        super::ir_entry_mode_exits_admitted(&COUNT, COUNT.len() - 2, true),
        synchronized_on
    );
    // `synchronized (this) { }` inside the loop's method.
    let locked: [u8; 9] = [
        0x2a, // 0: aload_0
        0x59, // 1: dup
        0x4c, // 2: astore_1
        0xc2, // 3: monitorenter
        0x2b, // 4: aload_1
        0xc3, // 5: monitorexit
        0xb1, // 6: return
        0, 0,
    ];
    assert_eq!(
        super::ir_entry_mode_exits_admitted(&locked, 7, false),
        super::IR_MONITOR_BLOCK_MODE_EXITS_ENABLED && crate::deopt_real_enabled()
    );
    assert_eq!(
        super::ir_entry_mode_exits_admitted(&locked, 7, true),
        synchronized_on
    );
    // A length past the buffer is refused, not walked, synchronized or not.
    for is_synchronized in [false, true] {
        assert!(!super::ir_entry_mode_exits_admitted(
            &COUNT,
            COUNT.len() + 1,
            is_synchronized
        ));
    }
}

/// Wave 18, lane L2 (`i9-L5-...`, IR item (b)): the bytecode liveness a
/// mode exit asks before refusing a local it cannot describe. In
/// `int t = n * 2; int i = 0; while (i < n) i++; return i;` the local `t`
/// is dead at the header and `n` and `i` are live; a bci inside an
/// instruction and a slot past `max_locals` are never called dead.
#[test]
fn a_local_the_bytecode_never_reads_again_is_dead_at_the_header() {
    const DEAD_TEMP: [u8; 21] = [
        0x1a, // 0:  iload_0
        0x05, // 1:  iconst_2
        0x68, // 2:  imul
        0x3c, // 3:  istore_1           (t, never read)
        0x03, // 4:  iconst_0
        0x3d, // 5:  istore_2
        0x1c, // 6:  iload_2             <- loop header
        0x1a, // 7:  iload_0
        0xa2, 0x00, 0x09, // 8:  if_icmpge +9 -> 17
        0x84, 0x02, 0x01, // 11: iinc 2, 1
        0xa7, 0xff, 0xf8, // 14: goto -8 -> 6
        0x1c, // 17: iload_2
        0xac, // 18: ireturn
        0, 0,
    ];
    let live = crate::regalloc::live_locals_per_pc_all(&DEAD_TEMP, 19, 1, &[], &[], 3);
    assert!(
        super::local_dead_at(&live, 3, 6, 1),
        "t is dead at the header"
    );
    assert!(
        !super::local_dead_at(&live, 3, 6, 0),
        "n is read by the test"
    );
    assert!(
        !super::local_dead_at(&live, 3, 6, 2),
        "i is read by the test"
    );
    assert!(
        !super::local_dead_at(&live, 3, 9, 1),
        "not an instruction start"
    );
    assert!(!super::local_dead_at(&live, 3, 6, 3), "past max_locals");
}

/// Wave 18, lane L2: a method-entry compile's polls are counted in their
/// own census, not the OSR door's. Process-global, so lower bounds only.
#[test]
fn a_method_entry_poll_given_a_mode_exit_is_counted_apart() {
    if !crate::deopt_real_enabled() || !crate::x64::jit_safepoint_polls_enabled() {
        return;
    }
    let (given_before, _) = super::ir_entry_poll_mode_exit_census();
    let cm = lower_with(
        &COUNT,
        0,
        1,
        IrPollModeExits::MethodEntry,
        // Cast: helper address
        verdict_poll as *const () as usize,
    );
    assert_eq!(poll_exits(&cm), 1);
    let (given_after, refused) = super::ir_entry_poll_mode_exit_census();
    assert!(
        given_after > given_before,
        "{given_before} -> {given_after}"
    );
    assert_eq!(refused.len(), super::ModeExitRefusal::CAUSE_NAMES.len());
    assert_ne!(super::CENSUS_METHOD_ENTRY, super::CENSUS_OSR_DOOR);
}

/// Interpreter round i1 wave 17, lane L1
/// (`i15-L2-ir-poll-mode-exit-coverage-has-no-census`): an OSR-door
/// compile's back-edge poll that got its exit is counted as given. The
/// census is process-global and other tests lower OSR-door bodies in
/// parallel, so only a lower bound on the delta is exact.
#[test]
fn a_poll_given_a_mode_exit_is_counted() {
    if !enabled() {
        return;
    }
    let (given_before, _) = super::ir_poll_mode_exit_census();
    let cm = lower(&COUNT, 0, 1, true);
    assert_eq!(poll_exits(&cm), 1);
    let (given_after, refused) = super::ir_poll_mode_exit_census();
    assert!(
        given_after > given_before,
        "{given_before} -> {given_after}"
    );
    assert_eq!(refused.len(), super::ModeExitRefusal::CAUSE_NAMES.len());
}

/// Every counted refusal cause has its own slot, in name order, and
/// `NotSought` has none.
#[test]
fn every_refusal_cause_has_its_own_census_slot() {
    use super::ModeExitRefusal as R;
    assert_eq!(R::NotSought.cause_index(), None);
    let causes = [
        R::Shape,
        R::NoHeaderSnapshot,
        R::HeaderPhiInput,
        R::ForeignPhi,
        R::NotCurrent,
        R::NoRegisterImage,
        R::Unlocatable,
        R::NotResumable,
    ];
    for (i, cause) in causes.into_iter().enumerate() {
        assert_eq!(cause.cause_index(), Some(i), "{cause:?}");
    }
    assert_eq!(causes.len(), R::CAUSE_NAMES.len());
    for row in &super::IR_POLL_MODE_EXIT_CENSUS {
        assert_eq!(row.len(), R::CAUSE_NAMES.len() + 1);
    }
}
