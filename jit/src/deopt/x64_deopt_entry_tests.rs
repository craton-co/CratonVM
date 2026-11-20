use super::*;

/// The 3-arg entry resolves `FrameValue::Register(r)` against the PASSED
/// register file (proving the in-stub spill + 3-arg wiring), where
/// `ir_deopt_entry`'s default-zeros path would yield 0; constants pass
/// through; and the frame is stashed for `take_last_deopt`.
#[test]
fn resolves_registers_against_passed_regfile_and_stashes() {
    let mut regs = SavedRegisters::default();
    regs.gpr[3] = 0xDEAD_BEEF; // architectural reg 3 (RBX) holds a live value
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 7,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 7,
            locals: vec![FrameValue::Register(3), FrameValue::Int(5)],
            stack: vec![FrameValue::Register(3)],
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let _ = take_last_deopt(); // clear any prior stash
                               // Null guard ⇒ no staleness check (the production / unstamped path).
    let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, std::ptr::null());
    assert_eq!(r, i64::MIN, "entry returns the deopt sentinel");

    let frame = take_last_deopt().expect("entry stashes a reconstructed frame");
    assert_eq!(frame.bci, 7);
    // Register(3) resolved to gpr[3]; Int passes through.
    assert_eq!(frame.locals[0], FrameValue::Int(0xDEAD_BEEF));
    assert_eq!(frame.locals[1], FrameValue::Int(5));
    assert_eq!(frame.stack[0], FrameValue::Int(0xDEAD_BEEF));
}

/// Interpreter round i6 wave 123, lane L2 (slice 4 of stage 3b of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
/// the trampoline's frame base rides beside the stashed frame, is read by
/// peek and, after a take with the point, by `taken_frame_rbp`, and a
/// restash of the same point puts it back. No value names a stack slot,
/// so the base is never dereferenced.
#[test]
fn the_frame_base_rides_beside_the_stash_through_a_take_and_restash() {
    let regs = SavedRegisters::default();
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 9,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 9,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    while take_last_deopt().is_some() {}
    assert_eq!(peek_last_deopt_frame_rbp(), 0);
    let rbp = 0x7FFF_1230u64;
    let r = x64_deopt_entry(&point, rbp, &regs as *const SavedRegisters, std::ptr::null());
    assert_eq!(r, i64::MIN);
    assert_eq!(peek_last_deopt_frame_rbp(), rbp);
    let (frame, cause, addr) = take_last_deopt_with_point().expect("stashed");
    assert_eq!(peek_last_deopt_frame_rbp(), 0);
    assert_eq!(taken_frame_rbp(addr), rbp);
    assert_eq!(taken_frame_rbp(addr ^ 8), 0);
    assert_eq!(taken_frame_rbp(0), 0);
    restash_last_deopt_with_point(frame, cause, addr);
    assert_eq!(peek_last_deopt_frame_rbp(), rbp);
    // A plain restash carries no point, hence no base.
    let frame = take_last_deopt().expect("restashed");
    restash_last_deopt(frame);
    assert_eq!(peek_last_deopt_frame_rbp(), 0);
    let _ = take_last_deopt();
    // A plain take records the popped entry for `last_taken_frame` too.
    let r = x64_deopt_entry(&point, rbp, &regs as *const SavedRegisters, std::ptr::null());
    assert_eq!(r, i64::MIN);
    let _ = take_last_deopt().expect("stashed");
    let (taken_rbp, body, bci) = last_taken_frame();
    assert_eq!((taken_rbp, bci), (rbp, 9));
    assert!(body.is_none(), "a null guard names no body");
    // Wave 124 (L2): the consuming read answers the same once, then
    // nothing until the next pop.
    let (taken_rbp, body, bci) = take_last_taken_frame();
    assert_eq!((taken_rbp, bci), (rbp, 9));
    assert!(body.is_none());
    let (taken_rbp, body, bci) = take_last_taken_frame();
    assert_eq!((taken_rbp, bci), (0, 0));
    assert!(body.is_none());
    assert_eq!(last_taken_frame().0, 0);
    let r = x64_deopt_entry(&point, rbp, &regs as *const SavedRegisters, std::ptr::null());
    assert_eq!(r, i64::MIN);
    let _ = take_last_deopt().expect("stashed");
    assert_eq!(last_taken_frame().0, rbp);
    clear_last_taken_frame();
    let (taken_rbp, body, bci) = last_taken_frame();
    assert_eq!((taken_rbp, bci), (0, 0));
    assert!(body.is_none());
}

/// Interpreter round i6 wave 123, lane L2: the frame-base equality the
/// debugger's compiled-home write rests on. The VM reads a compiled
/// activation's home slot of local `idx` at `frame_rbp - (idx + 1) * 8`
/// (`interpreter::read_compiled_homes`, the single-pass backend's
/// `local_offset`), and records `frame_rbp` as the write's key. The deopt
/// stub's map names the same home as `StackSlot(-(idx + 1) * 8)` resolved
/// against the `rbp` it hands the trampoline. So when the trampoline reads
/// every home the VM read, from the same words, its `rbp` -- the base the
/// stash keeps beside the frame -- is the listing's `frame_rbp`. Pinned on
/// a fake frame: the words come back through `StackSlot` exactly at the
/// VM's addresses, and the stash keeps that base.
#[test]
fn the_trampoline_reads_the_homes_at_the_listing_frame_base() {
    let mut frame_words = [0u64; 8];
    for (i, w) in frame_words.iter_mut().enumerate() {
        *w = 0x1000 + i as u64;
    }
    // The base sits just past the buffer: homes grow down from it.
    let base = frame_words.as_ptr_range().end as u64;
    let homes = 4usize;
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 3,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 3,
            // Cast: a small home offset.
            locals: (0..homes)
                .map(|idx| FrameValue::StackSlotLong(-(((idx + 1) * 8) as i32)))
                .collect(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let regs = SavedRegisters::default();
    while take_last_deopt().is_some() {}
    let r = x64_deopt_entry(&point, base, &regs as *const SavedRegisters, std::ptr::null());
    assert_eq!(r, i64::MIN);
    assert_eq!(peek_last_deopt_frame_rbp(), base);
    let frame = take_last_deopt().expect("stashed");
    for idx in 0..homes {
        // The VM's home address of local `idx` (`read_compiled_homes`).
        let addr = (base as usize) - (idx + 1) * 8;
        // SAFETY: inside `frame_words` (idx < 8), aligned.
        let word = unsafe { (addr as *const u64).read() };
        assert_eq!(
            frame.locals[idx],
            FrameValue::Long(word as i64),
            "local {idx} read at the VM's home address"
        );
    }
}

/// Null args are tolerated: the sentinel is returned AND the identity-less
/// re-run frame is stashed, exactly as `ir_deopt_entry` does for a null
/// point. Stashing nothing (the pre-r9 behaviour this test used to pin)
/// made the VM's `result == i64::MIN && take_last_deopt().is_some()`
/// detection read the sentinel as a legitimate `Long.MIN_VALUE` return.
#[test]
fn null_args_return_sentinel_and_stash_the_rerun_frame() {
    for (null_point, null_regs) in [(true, true), (true, false), (false, true)] {
        let regs = SavedRegisters::default();
        let point = DeoptimizationPoint {
            native_offset: 0,
            bci: 3,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                class_id: 0,
                method_key: "T.m:()V".to_string(),
                bci: 3,
                locals: vec![FrameValue::Register(3)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let p: *const DeoptimizationPoint = if null_point { std::ptr::null() } else { &point };
        let r_ptr: *const SavedRegisters = if null_regs { std::ptr::null() } else { &regs };
        let _ = take_last_deopt();
        let r = x64_deopt_entry(p, 0, r_ptr, std::ptr::null());
        assert_eq!(r, i64::MIN);
        let frame = take_last_deopt()
            .expect("a null argument must still stash a frame so i64::MIN reads as a deopt");
        assert_eq!(frame.bci, u32::MAX, "the identity-less re-run sentinel");
        assert!(frame.method_key.is_empty());
        assert!(frame.locals.is_empty());
    }
}

/// deopt-osr Step 9 follow-up (a), REVISED by the
/// jit-invokedynamic-groovy-regression identity fix: a SUPERSEDED guard
/// does not short-circuit. The trapping frame owns its artifact, so the
/// box is valid and its snapshot is self-consistent with the (stale,
/// still-executing) code that trapped. The entry must proceed to a normal
/// reconstruction; short-circuiting here stashed an identity-less
/// `bci == u32::MAX` sentinel that forced every post-supersession trap
/// onto the corrupting imprecise re-run. (The short-circuit survived under
/// a `CRATONVM_JIT_FREE_CODE=1` mode, since deleted.)
#[test]
fn superseded_guard_still_reconstructs_in_retain_mode() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let live = Box::new(AtomicU64::new(3)); // live epoch = 3
    let guard = DeoptEpochGuard::new();
    guard.creation_epoch.store(1, Ordering::Relaxed); // artifact made at epoch 1 < 3
    guard.live_epoch_cell.store(
        live.as_ref() as *const AtomicU64 as *mut AtomicU64,
        Ordering::Release,
    );
    assert!(guard.is_superseded());

    let regs = SavedRegisters::default();
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 21,
        reason: DeoptReason::UnreachedCode,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "T.m:()V".to_string(),
            bci: 21,
            locals: vec![FrameValue::Int(7)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let _ = take_last_deopt();
    let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, &guard);
    assert_eq!(r, i64::MIN);
    let frame = take_last_deopt().expect("retain-mode superseded path reconstructs normally");
    assert_eq!(frame.bci, 21, "real bci, not the u32::MAX re-run sentinel");
    assert_eq!(
        frame.method_key, "T.m:()V",
        "identity preserved for the resume sinks"
    );
    assert_eq!(frame.locals[0], FrameValue::Int(7));
}

/// A FRESH guard (creation epoch == live epoch) does NOT short-circuit: the
/// entry proceeds to reconstruct from the box as normal.
#[test]
fn fresh_guard_proceeds_to_reconstruct() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let live = Box::new(AtomicU64::new(2));
    let guard = DeoptEpochGuard::new();
    guard.creation_epoch.store(2, Ordering::Relaxed); // == live ⇒ fresh
    guard.live_epoch_cell.store(
        live.as_ref() as *const AtomicU64 as *mut AtomicU64,
        Ordering::Release,
    );
    assert!(!guard.is_superseded());

    let regs = SavedRegisters::default();
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 11,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 11,
            locals: vec![FrameValue::Int(99)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let _ = take_last_deopt();
    let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, &guard);
    assert_eq!(r, i64::MIN);
    let frame = take_last_deopt().expect("fresh path reconstructs from the box");
    assert_eq!(frame.bci, 11);
    assert_eq!(frame.locals[0], FrameValue::Int(99));
}

/// Round 14 wave 2 (lane deopt; R13RP6-1): the entry stashes the trapping
/// body's own bytecode when the guard's compile id names the artifact that
/// owns that very guard -- and nothing for a guard the bound artifact does
/// not own, for a guard with no id, or for the re-run sentinel.
#[test]
fn the_guard_names_the_trapping_body_and_its_own_bytecode() {
    use std::sync::Arc;
    let id = crate::reserve_compile_id();
    if id == 0 {
        // Every id live or awaiting grace (parallel tests): nothing to bind.
        return;
    }
    let source = Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(9),
            class_name: Arc::from("craton/test/r14/Own"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            // `return`, padded the way the VM pads bytecode.
            code: Arc::from(&[0xb1u8, 0, 0][..]),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 0,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let guard: &'static DeoptEpochGuard = Box::leak(Box::new(DeoptEpochGuard::for_compile(id)));
    let mut cm = Box::new(crate::CompiledMethod::new(
        crate::ExecutableBuffer::new(64).expect("alloc executable"),
    ));
    // Dropping `cm` releases the id and its binding.
    cm.compile_id = id;
    cm.deopt_epoch_guard = guard as *const DeoptEpochGuard;
    assert!(cm.stamp_compiled_source(Arc::clone(&source)));
    crate::bind_compile_id(id, &*cm as *const crate::CompiledMethod as usize);

    let regs = SavedRegisters::default();
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 0,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "craton/test/r14/Own.m:()V".to_string(),
            bci: 0,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let _ = take_last_deopt();
    assert_eq!(x64_deopt_entry(&point, 0, &regs, guard), i64::MIN);
    let (named, stamp) = peek_last_deopt_trap_source().expect("the body is named");
    assert!(Arc::ptr_eq(&named, &source), "the body's own template");
    assert_eq!(stamp, cm.compile_cp_stamp());
    assert_eq!(peek_last_deopt_point(), &point as *const _ as usize);
    let _ = take_last_deopt();
    assert!(
        peek_last_deopt_trap_source().is_none(),
        "the take clears it"
    );

    // Same id, a guard the bound artifact does not own: not its body.
    let stray: &'static DeoptEpochGuard = Box::leak(Box::new(DeoptEpochGuard::for_compile(id)));
    assert_eq!(x64_deopt_entry(&point, 0, &regs, stray), i64::MIN);
    assert!(peek_last_deopt_trap_source().is_none());
    let _ = take_last_deopt();

    // No id at all.
    let unnamed = DeoptEpochGuard::new();
    assert_eq!(x64_deopt_entry(&point, 0, &regs, &unnamed), i64::MIN);
    assert!(peek_last_deopt_trap_source().is_none());
    let _ = take_last_deopt();

    // A null register file stashes the re-run sentinel, which names nothing.
    assert_eq!(
        x64_deopt_entry(&point, 0, std::ptr::null(), guard),
        i64::MIN
    );
    assert!(peek_last_deopt_trap_source().is_none());
    let _ = take_last_deopt();

    // A restash carries no source.
    assert_eq!(x64_deopt_entry(&point, 0, &regs, guard), i64::MIN);
    let (f, cause, addr) = take_last_deopt_with_point().expect("stashed");
    restash_last_deopt_with_point(f, cause, addr);
    assert!(peek_last_deopt_trap_source().is_none());
    let _ = take_last_deopt();

    // Round 14 wave 3 (R14DP-3): the source-keeping pair puts it back.
    assert_eq!(x64_deopt_entry(&point, 0, &regs, guard), i64::MIN);
    let (f, cause, addr, kept) = take_last_deopt_with_source().expect("stashed");
    assert!(
        peek_last_deopt_trap_source().is_none(),
        "the take clears it"
    );
    assert!(kept
        .as_ref()
        .is_some_and(|(named, _)| Arc::ptr_eq(named, &source)));
    restash_last_deopt_with_source(f, cause, addr, kept);
    let (again, _) = peek_last_deopt_trap_source().expect("restashed with its source");
    assert!(Arc::ptr_eq(&again, &source));
    assert_eq!(peek_last_deopt_point(), addr);
    let _ = take_last_deopt();
    drop(cm);
}

/// Interpreter round i2 wave 60, lane L2
/// (`i59-L2-proposal-every-stash-names-its-trapping-body`): the IR entry
/// that its stub hands a compile id names the body bound to it when that
/// body owns the point; the name rides a take / restash of the same
/// point, is gone for another point, and dies with the body.
#[test]
fn the_ir_entry_names_its_trapping_body_by_compile_id() {
    use std::sync::Arc;
    let id = crate::reserve_compile_id();
    if id == 0 {
        // Every id live or awaiting grace (parallel tests): nothing to bind.
        return;
    }
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 3,
        reason: DeoptReason::OsrExit,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "craton/test/i60/Ir.m:()V".to_string(),
            bci: 3,
            locals: vec![FrameValue::Int(7)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let mut cm =
        crate::CompiledMethod::new(crate::ExecutableBuffer::new(64).expect("alloc executable"));
    // Dropping the body releases the id and its binding.
    cm.compile_id = id;
    cm.deopt_points = vec![point.clone()];
    let body = Arc::new(cm);
    body.note_bound_arc();
    crate::bind_compile_id(id, Arc::as_ptr(&body) as usize);
    let owned = &body.deopt_points[0] as *const DeoptimizationPoint;

    let _ = take_last_deopt();
    assert_eq!(
        ir_deopt_entry_named(owned, 0, std::ptr::null(), u64::from(id)),
        i64::MIN
    );
    // Wave 61: a sink that decides before it takes sees the same name.
    assert!(
        peek_last_deopt_trapping_body().is_some_and(|b| Arc::ptr_eq(&b, &body)),
        "the peek names the body"
    );
    let (f, cause, addr) = take_last_deopt_with_point().expect("stashed");
    assert_eq!(addr, owned as usize);
    assert!(
        taken_trapping_body(addr).is_some_and(|b| Arc::ptr_eq(&b, &body)),
        "the entry named the body"
    );
    assert!(taken_trapping_body(addr + 8).is_none(), "another point");
    restash_last_deopt_with_point(f, cause, addr);
    let _ = take_last_deopt_with_point().expect("restashed");
    assert!(
        taken_trapping_body(addr).is_some_and(|b| Arc::ptr_eq(&b, &body)),
        "a restash of the same point keeps the name"
    );

    // A point the bound body does not own names nothing.
    assert_eq!(
        ir_deopt_entry_named(&point, 0, std::ptr::null(), u64::from(id)),
        i64::MIN
    );
    let (_, _, stray) = take_last_deopt_with_point().expect("stashed");
    assert!(taken_trapping_body(stray).is_none());
    // The plain entry (no id) names nothing either.
    assert_eq!(ir_deopt_entry(owned, 0, std::ptr::null()), i64::MIN);
    let (_, _, plain) = take_last_deopt_with_point().expect("stashed");
    assert!(taken_trapping_body(plain).is_none());

    // The name dies with the body.
    assert_eq!(
        ir_deopt_entry_named(owned, 0, std::ptr::null(), u64::from(id)),
        i64::MIN
    );
    let (_, _, last) = take_last_deopt_with_point().expect("stashed");
    drop(body);
    assert!(taken_trapping_body(last).is_none());
}
