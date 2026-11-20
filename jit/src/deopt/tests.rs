use super::*;

fn fs(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> FrameState {
    FrameState {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 0,
        locals,
        stack,
        monitors: Vec::new(),
        caller: None,
    }
}

/// Regression witness for the 2026-07-31 `unresumable-indy-trap` compile
/// bail (`x64.rs`, opcode `0xba`). `Unsupported` in ANY live slot -- locals
/// or operand stack -- makes the frame unresumable; everything the resume
/// sink can map (including `Undefined`, which becomes `Int(0)`) does not.
/// Inverting this predicate re-arms the `InternalError: precise
/// deoptimization unavailable ... refusing side-effecting replay` failure
/// that took out javac `ClassReader.readInnerClasses` and the whole Spring
/// AOT in-process-compilation cluster.
#[test]
fn frame_state_resumability_tracks_unsupported_slots() {
    assert!(frame_state_is_resumable(&fs(vec![], vec![])));
    assert!(frame_state_is_resumable(&fs(
        vec![
            FrameValue::Object(0),
            FrameValue::Int(3),
            FrameValue::Undefined
        ],
        vec![FrameValue::Object(0x1000), FrameValue::Long(7)],
    )));
    assert!(
        !frame_state_is_resumable(&fs(vec![FrameValue::Unsupported], vec![])),
        "an Unsupported LOCAL must make the frame unresumable"
    );
    assert!(
        !frame_state_is_resumable(&fs(
            vec![FrameValue::Object(0)],
            vec![
                FrameValue::Object(0x1000),
                FrameValue::Unsupported,
                FrameValue::Object(0x2000),
            ],
        )),
        "an Unsupported STACK slot under an indy argument -- the javac \
         ClassReader.readInnerClasses shape -- must make the frame unresumable"
    );
}

/// The predicate follows `caller`.
///
/// The predicate used to inspect the innermost scope alone, which was
/// correct only because no producer built a chain. Its callers are the two
/// compile-time admission gates (`x64.rs`'s unresumable-indy-trap bail and
/// `CompiledMethod::osr_exit_policy`), and both must answer "no" for an
/// artifact whose trap sits under a caller frame nobody can describe —
/// otherwise a clean innermost scope admits it. (A handle-side
/// `chain_is_resumable` used to mirror this on the interned representation;
/// that representation was retired 2026-09-21 and this is the only one
/// left.)
#[test]
fn resumability_follows_the_whole_caller_chain() {
    let clean = || fs(vec![FrameValue::Int(1)], vec![]);
    let mut chain = clean();
    chain.caller = Some(Box::new(clean()));
    assert!(
        frame_state_is_resumable(&chain),
        "a clean chain stays resumable"
    );

    for dirty in [
        FrameValue::Unsupported,
        FrameValue::MaterializationRequired(EliminatedValue::unknown(
            EliminationCause::ElidedLock,
        )),
    ] {
        let mut bad_caller = fs(vec![dirty.clone()], vec![]);
        bad_caller.caller = None;
        let mut inner = clean();
        inner.caller = Some(Box::new(bad_caller));
        assert!(
            frame_state_is_resumable(&clean()),
            "precondition: the innermost scope alone is clean"
        );
        assert!(
            !frame_state_is_resumable(&inner),
            "an unrebuildable CALLER slot ({dirty:?}) must sink the whole chain"
        );

        // …at depth 3 as well, not just as an immediate caller.
        let mut deep = fs(vec![dirty.clone()], vec![]);
        for _ in 0..3 {
            let mut next = clean();
            next.caller = Some(Box::new(deep));
            deep = next;
        }
        assert!(!frame_state_is_resumable(&deep));
    }
}

/// A caller chain deeper than `MAX_SCOPE_CHAIN` is refused rather than
/// walked further: that deep is a metadata defect, and refusing costs only
/// a whole-method re-run.
#[test]
fn an_absurdly_deep_chain_is_refused_not_walked() {
    let mut deep = fs(vec![FrameValue::Int(0)], vec![]);
    for _ in 0..(MAX_SCOPE_CHAIN + 4) {
        let mut next = fs(vec![FrameValue::Int(0)], vec![]);
        next.caller = Some(Box::new(deep));
        deep = next;
    }
    assert!(!frame_state_is_resumable(&deep));
}

// -- per-bci de-spec registry (Step 9 follow-up c) ---------------------

/// `DespecRegistry::{insert, contains, count_for}` are per-(method, bci):
/// a recorded site matches only its own key+bci, an empty key never matches,
/// and inserts are idempotent. The speculation id is the third key
/// component (M4) and has its own tests below.
#[test]
fn despec_registry_is_per_method_bci() {
    let registry = DespecRegistry::new();
    let m = "DespecTest$Unique.loop:(I)I";
    assert!(!registry.contains(m, 7));
    registry.insert(m, 7, SPECULATION_ID_ANY);
    registry.insert(m, 7, SPECULATION_ID_ANY); // idempotent
    registry.insert(m, 12, SPECULATION_ID_ANY);
    assert!(registry.contains(m, 7));
    assert!(registry.contains(m, 12));
    assert!(!registry.contains(m, 8), "a different bci must not match");
    assert!(
        !registry.contains("OtherClass.m:()V", 7),
        "a different method must not match"
    );
    assert!(!registry.contains("", 7), "an empty key never matches");
    assert_eq!(registry.count_for(m), 2);
}

/// gc-common w36-c: an unload forgets the unloaded classes' de-spec rows,
/// by the key's class field, and nothing else: the same match as
/// `DeoptimizationLog::clear_class`, and one pass for the whole set.
#[test]
fn despec_registry_forgets_unloaded_classes_by_class_field() {
    let registry = DespecRegistry::new();
    for key in [
        "w36c/A.m:()V",
        "w36c/A.n:(I)V",
        "w36c/Ab.m:()V",
        "w36c/A.b.m:()V",
        "x/Y.run:()V",
        "",
    ] {
        registry.insert(key, 3, SPECULATION_ID_ANY);
    }
    assert_eq!(registry.forget_classes(&FxHashSet::default()), 0);
    assert_eq!(
        registry.forget_classes(&["w36c/A", "x/Y", "absent/Z"].into_iter().collect()),
        3
    );
    assert!(!registry.contains("w36c/A.m:()V", 3));
    assert!(!registry.contains("w36c/A.n:(I)V", 3));
    assert!(!registry.contains("x/Y.run:()V", 3));
    assert!(registry.contains("w36c/Ab.m:()V", 3), "a different class");
    assert!(
        registry.contains("w36c/A.b.m:()V", 3),
        "a different class whose key merely begins with the name"
    );
}

/// **The M4 fix.** Two speculations at one bci — the ordinary shape of a
/// protected invoke, per `osr_exit.rs`: a `ReceiverTypeChanged` guard and a
/// `PendingException` frame — are withdrawn independently. Before this the
/// key was `(method, bci)`, so four traps on either took both.
#[test]
fn two_speculations_at_one_bci_are_withdrawn_independently() {
    let registry = DespecRegistry::new();
    let m = "DespecTest$Protected.call:(I)I";
    let guard = speculation_id(9, DeoptReason::ReceiverTypeChanged);
    let frame = speculation_id(9, DeoptReason::PendingException);
    assert_ne!(
        guard, frame,
        "the two speculations at one bci must be distinguishable, or the \
         key change buys nothing"
    );

    registry.insert(m, 9, frame);
    assert!(
        registry.contains_speculation(m, 9, frame),
        "the exceptional frame that kept trapping is withdrawn"
    );
    assert!(
        !registry.contains_speculation(m, 9, guard),
        "the receiver guard at the same bci never failed and must survive"
    );
    // The coarse question still answers yes: `recommend_action_at_bci` and
    // the loop-hoist gates ask whether ANYTHING at this bci was given up on.
    assert!(registry.contains(m, 9));
}

/// A producer that cannot say WHICH speculation failed records
/// `SPECULATION_ID_ANY`, and that entry is a wildcard — the coarse
/// behaviour the whole tree had before M4, kept for the IR tier and for an
/// ambiguous reason lookup. Suppressing too much costs an optimisation;
/// suppressing too little leaves a guard trapping forever.
#[test]
fn an_unassigned_speculation_id_withdraws_every_speculation_at_its_bci() {
    let registry = DespecRegistry::new();
    let m = "DespecTest$Mixed.run:()V";
    registry.insert(m, 4, SPECULATION_ID_ANY);
    assert!(registry.contains_speculation(
        m,
        4,
        speculation_id(4, DeoptReason::ReceiverTypeChanged)
    ));
    assert!(registry.contains_speculation(
        m,
        4,
        speculation_id(4, DeoptReason::PendingException)
    ));
    assert!(
        !registry.contains_speculation(
            m,
            5,
            speculation_id(5, DeoptReason::ReceiverTypeChanged)
        ),
        "the wildcard is per bci, not per method"
    );
}

/// The derivation never produces the reserved `0`, which would read as
/// "no id assigned" and turn a precise withdrawal into a wildcard.
#[test]
fn a_derived_speculation_id_is_never_the_reserved_zero() {
    for bci in 0..4096u32 {
        for reason in [
            DeoptReason::ReceiverTypeChanged,
            DeoptReason::PendingException,
            DeoptReason::ClassCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::UncommonTrap,
        ] {
            assert_ne!(speculation_id(bci, reason), SPECULATION_ID_ANY);
        }
    }
}

/// Two VMs, two registries: a verdict recorded by one is invisible to the
/// other. This is the property the process-global set did not have.
#[test]
fn despec_registries_do_not_share_verdicts() {
    let first_vm = DespecRegistry::new();
    let second_vm = DespecRegistry::new();
    first_vm.insert("Shared.m:()V", 3, SPECULATION_ID_ANY);
    assert!(first_vm.contains("Shared.m:()V", 3));
    assert!(
        !second_vm.contains("Shared.m:()V", 3),
        "a second VM must not inherit the first VM's despeculation"
    );
    assert_eq!(second_vm.count_for("Shared.m:()V"), 0);
}

// -- helpers -----------------------------------------------------------

fn make_event(reason: DeoptReason, bci: u32) -> DeoptEvent {
    DeoptEvent {
        reason,
        action: DeoptAction::Reinterpret,
        bci,
        timestamp_ms: 1000,
        speculation_id: 0,
    }
}

fn simple_frame_state() -> FrameState {
    FrameState {
        class_id: 0,
        method_key: "Foo.bar:()V".to_string(),
        bci: 10,
        locals: vec![FrameValue::Int(42), FrameValue::Object(0)],
        stack: vec![FrameValue::Int(7)],
        monitors: Vec::new(),
        caller: None,
    }
}

fn simple_deopt_point() -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x100,
        bci: 10,
        reason: DeoptReason::NullCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: simple_frame_state(),
        semantics: ResumeSemantics::REEXECUTE,
    }
}

// -- real-frame-deopt type source -------------------------------------

/// `resolve_value` (via `reconstruct_frame_from_machine_state`) reads a
/// `StackSlotRef` slot as an `Object` (the raw word IS the heap pointer),
/// a `StackSlot` slot as an `Int`, a `StackSlotLong` slot as a cat-2 `Long`
/// (the full 64-bit word), and passes `Unsupported` through — so the resume
/// can build a real `Value::Object`/`Value::Long` for ref/long slots instead
/// of a truncated `Value::Int`.
#[test]
fn reconstruct_resolves_typed_slots() {
    // A fake native frame. `resolve_value` reads `*(rbp + off)`; the IR
    // convention stores spills below rbp, so we point `rbp` past the buffer
    // and use negative offsets.
    //   buf[0] @ rbp-24 (ref), buf[1] @ rbp-16 (int), buf[2] @ rbp-8 (long).
    let buf: [u64; 4] = [
        0x1111_2222_3333_4444,
        0x0000_0000_DEAD_BEEF,
        0xFEDC_BA98_7654_3210, // a full 64-bit long (high bits set)
        0,
    ];
    let rbp = (&buf[3] as *const u64) as u64;
    let regs = SavedRegisters::default();
    let fs = FrameState {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 3,
        locals: vec![
            FrameValue::StackSlotRef(-24),
            FrameValue::StackSlot(-16),
            FrameValue::StackSlotLong(-8),
            FrameValue::Unsupported,
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 3,
        reason: DeoptReason::DivByZero,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
    assert_eq!(rf.locals[0], FrameValue::Object(0x1111_2222_3333_4444));
    assert_eq!(rf.locals[1], FrameValue::Int(0xDEAD_BEEF));
    // StackSlotLong reads the full 64-bit word (NOT truncated to i32).
    assert_eq!(
        rf.locals[2],
        FrameValue::Long(0xFEDC_BA98_7654_3210u64 as i64)
    );
    assert_eq!(rf.locals[3], FrameValue::Unsupported);
}

/// `resolve_value` reads FP slots and XMM-resident FP values precisely: a
/// `StackSlotFloat`/`XmmFloat` as the low 32 bits (a cat-1 `Float`), a
/// `StackSlotDouble`/`XmmDouble` as the full 64 bits (a cat-2 `Double`). The
/// high garbage in the float sources proves the low-32 mask/read — so the
/// resume builds a real `Value::Float`/`Value::Double`, never a truncated int.
#[test]
fn reconstruct_resolves_fp_slots_and_xmm_registers() {
    let float_bits: u32 = 1.5f32.to_bits(); // 0x3FC0_0000
    let double_bits: u64 = std::f64::consts::PI.to_bits();
    // buf[0] @ rbp-16 (float bits in low 32, high garbage), buf[1] @ rbp-8 (double).
    let buf: [u64; 3] = [0xDEAD_BEEF_0000_0000 | float_bits as u64, double_bits, 0];
    let rbp = (&buf[2] as *const u64) as u64;

    let mut regs = SavedRegisters::default();
    regs.xmm[5] = 0xCAFE_F00D_0000_0000 | float_bits as u64; // high garbage masked off
    regs.xmm[9] = double_bits;
    // A cat-2 `long` in GPR 7 — full 64 bits (high bits set, would truncate
    // if mistyped as a cat-1 `Register`).
    regs.gpr[7] = 0xFEDC_BA98_7654_3210;
    // An object reference in GPR 3 — the full pointer word.
    regs.gpr[3] = 0x0000_7F12_3456_7890;

    let fs = FrameState {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 0,
        locals: vec![
            FrameValue::StackSlotFloat(-16),
            FrameValue::StackSlotDouble(-8),
            FrameValue::XmmFloat(5),
            FrameValue::XmmDouble(9),
            FrameValue::RegisterLong(7),
            FrameValue::RegisterRef(3),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 0,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
    assert_eq!(rf.locals[0], FrameValue::Float(float_bits as u64));
    assert_eq!(rf.locals[1], FrameValue::Double(double_bits));
    assert_eq!(rf.locals[2], FrameValue::Float(float_bits as u64));
    assert_eq!(rf.locals[3], FrameValue::Double(double_bits));
    // RegisterLong keeps all 64 bits (NOT truncated like Register -> Int).
    assert_eq!(
        rf.locals[4],
        FrameValue::Long(0xFEDC_BA98_7654_3210u64 as i64)
    );
    // RegisterRef carries the full heap pointer as an Object (NOT a truncated
    // Int that would also drop it from the GC scan).
    assert_eq!(rf.locals[5], FrameValue::Object(0x0000_7F12_3456_7890));
}

// -- register-homed locals vs. a stale frame slot ----------------------
//
// These pin the invariant the register allocator's cross-call publication
// policy depends on: once a local is described by a REGISTER `FrameValue`,
// reconstruction never consults that local's canonical frame slot. The
// slot may therefore hold a stale copy — which is exactly what happens if
// a call site is allowed to skip publishing a primitive register-homed
// local. See `regalloc::SafepointPublishPlan` and
// `jit-regalloc-and-deopt.md`.

/// Every register-homed local reconstructs from `SavedRegisters`, and the
/// frame slot that would be its canonical home is filled with a *wrong*
/// value to prove it is never read.
#[test]
fn register_homed_locals_ignore_a_stale_canonical_frame_slot() {
    // A fake native frame whose local slots hold DELIBERATELY STALE data:
    // slot for local 0 @ rbp-8, local 1 @ rbp-16, local 2 @ rbp-24 — the
    // `[rbp - (idx+1)*8]` layout the x64 emitter uses.
    let stale: [u64; 4] = [
        0xBAAD_F00D_BAAD_F00D, // rbp-32 (unused)
        0xDEAD_DEAD_DEAD_DEAD, // rbp-24  <- local 2's canonical slot
        0xBEEF_BEEF_BEEF_BEEF, // rbp-16  <- local 1's canonical slot
        0xFACE_FACE_FACE_FACE, // rbp-8   <- local 0's canonical slot
    ];
    let rbp = (stale.as_ptr() as u64) + 32;

    let mut regs = SavedRegisters::default();
    regs.gpr[12] = 41; // local 0: a live int in R12
    regs.gpr[13] = 0x0000_0001_0000_002A; // local 1: a live long in R13
    regs.gpr[14] = 0x0000_7F00_1234_5678; // local 2: a live ref in R14

    let fs = FrameState {
        class_id: 0,
        method_key: "T.fib:(I)I".to_string(),
        bci: 10,
        locals: vec![
            FrameValue::Register(12),
            FrameValue::RegisterLong(13),
            FrameValue::RegisterRef(14),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 10,
        reason: DeoptReason::UncommonTrap,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
    assert_eq!(
        rf.locals[0],
        FrameValue::Int(41),
        "a register-homed int must come from the GPR file, not the frame slot"
    );
    assert_eq!(
        rf.locals[1],
        FrameValue::Long(0x0000_0001_0000_002A),
        "a register-homed long must keep all 64 bits from the GPR file"
    );
    assert_eq!(
        rf.locals[2],
        FrameValue::Object(0x0000_7F00_1234_5678),
        "a register-homed ref must resolve as a GC-tracked Object from the GPR file"
    );
    // None of the stale words leaked through.
    for v in &rf.locals {
        match v {
            FrameValue::Int(i) | FrameValue::Long(i) => {
                assert!(!stale.contains(&(*i as u64)), "stale slot leaked: {v:?}")
            }
            FrameValue::Object(o) => {
                assert!(!stale.contains(o), "stale slot leaked: {v:?}")
            }
            other => panic!("unexpected reconstruction {other:?}"),
        }
    }
}

/// The same value in the same GPR reconstructs to a *different* category
/// depending on the descriptor variant the snapshot emitter chose. This is
/// what makes `Register` vs `RegisterLong` vs `RegisterRef` load-bearing:
/// they select cat-1/cat-2 local placement and GC tracking on resume, not
/// just a numeric read.
#[test]
fn register_descriptor_variant_selects_the_resumed_value_category() {
    let mut regs = SavedRegisters::default();
    regs.gpr[15] = 0x0000_7F55_0000_0007;
    let rbp = 0u64; // never dereferenced — no slot descriptors here
    assert_eq!(
        resolve_value(&FrameValue::Register(15), &regs, rbp),
        FrameValue::Int(0x0000_7F55_0000_0007)
    );
    assert_eq!(
        resolve_value(&FrameValue::RegisterLong(15), &regs, rbp),
        FrameValue::Long(0x0000_7F55_0000_0007)
    );
    assert_eq!(
        resolve_value(&FrameValue::RegisterRef(15), &regs, rbp),
        FrameValue::Object(0x0000_7F55_0000_0007)
    );
}

/// Inlined caller frames share the physical frame AND the register file, so
/// a register-homed local in an inlined *caller* must resolve from the same
/// `SavedRegisters` — an inliner that raises its budget (the concurrent
/// work on `jit/src/lib.rs`) deepens exactly this chain.
#[test]
fn inlined_caller_frames_resolve_register_locals_from_the_same_regfile() {
    let mut regs = SavedRegisters::default();
    regs.gpr[3] = 7; // RBX — callee-saved, so it survives the inlined call
    regs.gpr[12] = 9;

    let caller = FrameState {
        class_id: 0,
        method_key: "T.outer:()V".to_string(),
        bci: 4,
        locals: vec![FrameValue::Register(3)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let callee = FrameState {
        class_id: 0,
        method_key: "T.inner:()V".to_string(),
        bci: 1,
        locals: vec![FrameValue::Register(12)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: Some(Box::new(caller)),
    };
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 1,
        reason: DeoptReason::SpeculationFailed,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: callee,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
    assert_eq!(rf.locals[0], FrameValue::Int(9));
    assert_eq!(rf.caller_frames.len(), 1);
    assert_eq!(rf.caller_frames[0].method_key, "T.outer:()V");
    assert_eq!(rf.caller_frames[0].locals[0], FrameValue::Int(7));
}

/// A monitor whose object is register-homed must also resolve from the
/// register file — otherwise a deopt inside a synchronized region rebuilds
/// the interpreter frame holding the *wrong* monitor and the unlock on
/// resume targets a different object.
#[test]
fn monitors_resolve_register_homed_objects() {
    let mut regs = SavedRegisters::default();
    regs.gpr[14] = 0x0000_7F99_0000_1000;
    let fs = FrameState {
        class_id: 0,
        method_key: "T.sync:()V".to_string(),
        bci: 2,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: vec![MonitorInfo {
            object: FrameValue::RegisterRef(14),
            lock_depth: 1,
            relock: true,
        }],
        caller: None,
    };
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 2,
        reason: DeoptReason::UncommonTrap,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
    assert_eq!(rf.monitors.len(), 1);
    assert_eq!(
        rf.monitors[0].object,
        FrameValue::Object(0x0000_7F99_0000_1000)
    );
    assert_eq!(rf.monitors[0].lock_depth, 1);
}

/// `SavedRegisters` is the ABI contract between the x64 deopt stub (which
/// spills 16 GPRs then 16 XMMs into a contiguous 256-byte region and passes
/// `&gpr[0]`) and this resolver. A layout change on either side silently
/// mis-resolves every register-homed value, so pin it here.
#[test]
fn saved_registers_layout_matches_the_stub_spill_region() {
    assert_eq!(std::mem::size_of::<SavedRegisters>(), 256);
    let sr = SavedRegisters::default();
    let base = &sr as *const SavedRegisters as usize;
    assert_eq!(
        &sr.gpr[0] as *const u64 as usize, base,
        "gpr[0] must be at the struct base — the stub passes &gpr[0] as the pointer"
    );
    assert_eq!(
        &sr.xmm[0] as *const u64 as usize - base,
        128,
        "the XMM half must start 128 bytes in (16 GPRs x 8 bytes)"
    );
    // Highest indices addressable by a FrameValue register descriptor.
    assert_eq!(sr.gpr.len(), 16);
    assert_eq!(sr.xmm.len(), 16);
}

// -- DeoptReason -------------------------------------------------------

#[test]
fn deopt_reason_null_check() {
    assert_eq!(DeoptReason::NullCheck, DeoptReason::NullCheck);
}

#[test]
fn deopt_reason_class_check() {
    assert_ne!(DeoptReason::ClassCheck, DeoptReason::NullCheck);
}

#[test]
fn deopt_reason_all_variants_distinct() {
    let variants = [
        DeoptReason::NullCheck,
        DeoptReason::ClassCheck,
        DeoptReason::BoundsCheck,
        DeoptReason::DivByZero,
        DeoptReason::ReceiverTypeChanged,
        DeoptReason::ClassLoading,
        DeoptReason::UninitializedAccess,
        DeoptReason::TransferToInterpreter,
        DeoptReason::UncommonTrap,
        DeoptReason::SpeculationFailed,
        DeoptReason::NotCompiled,
        DeoptReason::UnreachedCode,
        DeoptReason::OsrExit,
        DeoptReason::PendingException,
        DeoptReason::UnstableIf,
    ];
    for (i, a) in variants.iter().enumerate() {
        for (j, b) in variants.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
            }
        }
    }
}

#[test]
fn deopt_reason_is_hashable() {
    let mut set = std::collections::HashSet::new();
    set.insert(DeoptReason::NullCheck);
    set.insert(DeoptReason::NullCheck);
    assert_eq!(set.len(), 1);
}

// -- DeoptAction -------------------------------------------------------

#[test]
fn deopt_action_all_variants() {
    let actions = [
        DeoptAction::Reinterpret,
        DeoptAction::RecompileAndReinterpret,
        DeoptAction::MakeNotEntrant,
        DeoptAction::MakeNotCompilable,
    ];
    assert_eq!(actions.len(), 4);
    assert_ne!(actions[0], actions[1]);
}

// -- DeoptimizationPoint -----------------------------------------------

#[test]
fn deopt_point_creation() {
    let dp = simple_deopt_point();
    assert_eq!(dp.native_offset, 0x100);
    assert_eq!(dp.bci, 10);
    assert_eq!(dp.reason, DeoptReason::NullCheck);
    assert_eq!(dp.action, DeoptAction::Reinterpret);
    assert_eq!(dp.speculation_id, 0);
}

// -- FrameState --------------------------------------------------------

#[test]
fn frame_state_locals_and_stack() {
    let fs = simple_frame_state();
    assert_eq!(fs.locals.len(), 2);
    assert_eq!(fs.stack.len(), 1);
    assert_eq!(fs.bci, 10);
}

#[test]
fn frame_state_nested_caller() {
    let outer = FrameState {
        class_id: 0,
        method_key: "Outer.run:()V".to_string(),
        bci: 5,
        locals: vec![FrameValue::Int(1)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let inner = FrameState {
        class_id: 0,
        method_key: "Inner.go:()V".to_string(),
        bci: 20,
        locals: vec![FrameValue::Int(2)],
        stack: vec![FrameValue::Int(3)],
        monitors: Vec::new(),
        caller: Some(Box::new(outer)),
    };
    assert!(inner.caller.is_some());
    assert_eq!(inner.caller.as_ref().unwrap().method_key, "Outer.run:()V");
}

// -- FrameValue --------------------------------------------------------

#[test]
fn frame_value_int() {
    assert_eq!(FrameValue::Int(42), FrameValue::Int(42));
}

#[test]
fn frame_value_float() {
    let bits = f64::to_bits(3.14);
    assert_eq!(FrameValue::Float(bits), FrameValue::Float(bits));
}

#[test]
fn frame_value_object_null() {
    assert_eq!(FrameValue::Object(0), FrameValue::Object(0));
}

#[test]
fn frame_value_register() {
    assert_eq!(FrameValue::Register(7), FrameValue::Register(7));
    assert_ne!(FrameValue::Register(0), FrameValue::Register(1));
}

#[test]
fn frame_value_stack_slot() {
    assert_eq!(FrameValue::StackSlot(-8), FrameValue::StackSlot(-8));
}

#[test]
fn frame_value_undefined() {
    assert_eq!(FrameValue::Undefined, FrameValue::Undefined);
}

// -- VirtualObjectState ------------------------------------------------

#[test]
fn virtual_object_state_fields() {
    let vo = VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: 42,
        num_fields: 2,
        field_values: vec![FrameValue::Int(1), FrameValue::Object(0)],
    };
    assert_eq!(vo.class_id, 42);
    assert_eq!(vo.num_fields, 2);
    assert_eq!(vo.field_values.len(), 2);
}

#[test]
fn resolve_value_recurses_virtual_object_fields() {
    // Guard-surviving SR: the producer emits a `VirtualObject` whose fields
    // are still machine forms (`StackSlot*`/`Register*`). `resolve_value`
    // must resolve each field from the live machine state so the VM
    // materializer receives concrete values; a `VirtualObjectRef` field (an
    // intra-frame id edge) passes through unchanged.
    let mut regs = SavedRegisters::default();
    regs.gpr[3] = 0x1234; // a RegisterRef field reads this GPR as an object ptr
                          // Stand-in native frame: a StackSlot(off) reads *(rbp + off) as i64.
    let buf: [i64; 4] = [0, 111, 0xBEEFi64, 0];
    let rbp = buf.as_ptr() as u64;
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 9,
        class_id: 7,
        num_fields: 4,
        field_values: vec![
            FrameValue::StackSlot(8),        // buf[1] = 111 → Int(111)
            FrameValue::StackSlotRef(16),    // buf[2] = 0xBEEF → Object(0xBEEF)
            FrameValue::RegisterRef(3),      // gpr[3] = 0x1234 → Object(0x1234)
            FrameValue::VirtualObjectRef(5), // intra-frame edge → unchanged
        ],
    });
    match resolve_value(&vo, &regs, rbp) {
        FrameValue::VirtualObject(s) => {
            assert_eq!(s.id, 9);
            assert_eq!(
                s.field_values,
                vec![
                    FrameValue::Int(111),
                    FrameValue::Object(0xBEEF),
                    FrameValue::Object(0x1234),
                    FrameValue::VirtualObjectRef(5),
                ]
            );
        }
        other => panic!("expected VirtualObject, got {other:?}"),
    }
    // A bare ref edge resolves to itself (no machine location).
    assert_eq!(
        resolve_value(&FrameValue::VirtualObjectRef(5), &regs, rbp),
        FrameValue::VirtualObjectRef(5)
    );
}

// -- MonitorInfo -------------------------------------------------------

#[test]
fn monitor_info_tracking() {
    let mi = MonitorInfo {
        object: FrameValue::Object(0xDEAD),
        lock_depth: 2,
        relock: true,
    };
    assert_eq!(mi.lock_depth, 2);
    assert_eq!(mi.object, FrameValue::Object(0xDEAD));
}

// -- DeoptimizationLog -------------------------------------------------

#[test]
fn log_new_empty() {
    let log = DeoptimizationLog::new();
    assert_eq!(log.total_deopts(), 0);
    assert_eq!(log.deopt_count("any"), 0);
}

#[test]
fn log_empty_history() {
    let log = DeoptimizationLog::new();
    assert!(log.history("nonexistent").is_empty());
}

#[test]
fn log_record_and_count() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("Foo.bar", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("Foo.bar", make_event(DeoptReason::BoundsCheck, 5));
    assert_eq!(log.deopt_count("Foo.bar"), 2);
    assert_eq!(log.total_deopts(), 2);
}

#[test]
fn log_total_deopts_counter() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("A", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("B", make_event(DeoptReason::DivByZero, 0));
    log.record_deopt("A", make_event(DeoptReason::NullCheck, 1));
    assert_eq!(log.total_deopts(), 3);
}

#[test]
fn log_should_give_up_after_threshold() {
    let mut log = DeoptimizationLog::new_with_threshold(3);
    assert!(!log.should_give_up("m"));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
    assert!(!log.should_give_up("m"));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 2));
    assert!(log.should_give_up("m"));
}

#[test]
fn log_most_common_reason() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("m", make_event(DeoptReason::BoundsCheck, 1));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 2));
    assert_eq!(log.most_common_reason("m"), Some(DeoptReason::NullCheck));
}

#[test]
fn log_most_common_reason_empty() {
    let log = DeoptimizationLog::new();
    assert_eq!(log.most_common_reason("m"), None);
}

#[test]
fn log_clear_history() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
    assert_eq!(log.deopt_count("m"), 2);
    log.clear_history("m");
    assert_eq!(log.deopt_count("m"), 0);
    // total_deopts is a lifetime counter — not decremented.
    assert_eq!(log.total_deopts(), 2);
}

/// A reason nothing charges must not push a reason that IS charged over
/// the escalation cliff. Ten exceptions out of a hot `try`/`catch` used to
/// make the very next genuine trap come back `MakeNotEntrant`.
#[test]
fn soft_reasons_do_not_inflate_the_escalation_count() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    for i in 0..40 {
        log.record_deopt("m", make_event(DeoptReason::PendingException, i));
        log.record_deopt("m", make_event(DeoptReason::TransferToInterpreter, i));
    }
    assert_eq!(
        log.deopt_count("m"),
        0,
        "an exception routed out of compiled code is not a failed speculation"
    );
    assert_eq!(
        log.lifetime_deopt_count("m"),
        80,
        "but the raw deopt rate is still observable"
    );
    assert_eq!(
        log.recommend_action("m", DeoptReason::UncommonTrap),
        DeoptAction::Reinterpret,
        "the first genuine trap is still the FIRST one"
    );
}

/// r9 wave 2: `OsrExit` escalates on its own `(OsrExit, bci)` site when
/// the VM asks with a bci (`recommend_action_at_bci`, its only production
/// caller), not on the method's aggregate charged count.
#[test]
fn osr_exit_escalates_on_its_own_site_not_the_aggregate() {
    let despec = DespecRegistry::new();
    let m = "OsrSite.m:()V";
    let mut log = DeoptimizationLog::new_with_threshold(20);
    // Ten charged ordinary traps elsewhere in the method (= `half`).
    for _ in 0..10 {
        log.record_deopt(m, make_event(DeoptReason::UncommonTrap, 5));
    }
    // The old aggregate rule, still answered by the bci-less entry point.
    assert_eq!(
        log.recommend_action(m, DeoptReason::OsrExit),
        DeoptAction::MakeNotCompilable
    );
    assert_eq!(
        log.recommend_action_at_bci(m, DeoptReason::OsrExit, 40, &despec),
        DeoptAction::Reinterpret,
        "the first exit at bci 40 must not blacklist a method for traps \
         at another bci"
    );
    // A structurally-always-exiting bci still retires after `half` exits.
    for _ in 0..10 {
        log.record_deopt(m, make_event(DeoptReason::OsrExit, 40));
    }
    assert_eq!(
        log.recommend_action_at_bci(m, DeoptReason::OsrExit, 40, &despec),
        DeoptAction::MakeNotCompilable
    );
    // …while a different loop bci of the same method is below `half` but
    // the aggregate (20) has now reached `full`: the backstop retires it.
    assert_eq!(
        log.recommend_action_at_bci(m, DeoptReason::OsrExit, 77, &despec),
        DeoptAction::MakeNotCompilable,
        "the aggregate backstop at `full` still applies"
    );
}

/// The per-site rule alone, with no other traps: nine exits at one bci
/// are tolerated, the tenth (`half` of 20) is not; exits spread over two
/// bcis are tolerated until the aggregate backstop.
#[test]
fn osr_exit_site_rule_tolerates_half_per_site() {
    let despec = DespecRegistry::new();
    let m = "OsrSite.n:()V";
    let mut log = DeoptimizationLog::new_with_threshold(20);
    for _ in 0..9 {
        log.record_deopt(m, make_event(DeoptReason::OsrExit, 12));
        log.record_deopt(m, make_event(DeoptReason::OsrExit, 30));
    }
    assert_eq!(log.deopt_count(m), 18);
    assert_eq!(
        log.recommend_action_at_bci(m, DeoptReason::OsrExit, 12, &despec),
        DeoptAction::Reinterpret
    );
    log.record_deopt(m, make_event(DeoptReason::OsrExit, 12));
    assert_eq!(
        log.recommend_action_at_bci(m, DeoptReason::OsrExit, 12, &despec),
        DeoptAction::MakeNotCompilable
    );
}

/// The event history is a bounded ring, not an unbounded `Vec`: a method
/// that throws out of compiled code forever used to retain one event per
/// throw for the life of the process.
#[test]
fn the_event_history_is_bounded() {
    let mut log = DeoptimizationLog::new();
    for i in 0..(TRAP_HISTORY_RING as u32 * 4) {
        log.record_deopt("m", make_event(DeoptReason::PendingException, i));
    }
    let hist = log.history("m");
    assert_eq!(hist.len(), TRAP_HISTORY_RING);
    assert_eq!(
        hist.last().map(|e| e.bci),
        Some(TRAP_HISTORY_RING as u32 * 4 - 1),
        "the ring keeps the most recent events"
    );
}

/// The per-`(reason, bci)` site table is bounded too, and a method that
/// spreads its traps over thousands of bcis still escalates: the total is
/// charged even when the site is not admitted.
#[test]
fn the_trap_site_table_is_bounded_but_the_total_is_not_escapable() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    for i in 0..(MAX_TRAP_SITES_PER_METHOD as u32 * 4) {
        log.record_deopt("m", make_event(DeoptReason::BoundsCheck, i));
    }
    assert_eq!(
        log.deopt_count("m"),
        MAX_TRAP_SITES_PER_METHOD * 4,
        "spreading traps over many bcis does not hide them from the total"
    );
    assert_eq!(
        log.recommend_action("m", DeoptReason::BoundsCheck),
        DeoptAction::MakeNotCompilable
    );
}

/// Traps at one bci are counted per bci, and an uncharged reason at that
/// same bci does not add to it — which is the ordinary shape of a
/// protected invoke, where a `PendingException` point and a receiver guard
/// share one bci.
#[test]
fn a_pending_exception_does_not_despeculate_the_guard_sharing_its_bci() {
    let mut log = DeoptimizationLog::new();
    for _ in 0..20 {
        log.record_deopt("m", make_event(DeoptReason::PendingException, 7));
    }
    assert_eq!(log.deopt_count_at_bci("m", 7), 0);
    log.record_deopt("m", make_event(DeoptReason::ReceiverTypeChanged, 7));
    assert_eq!(log.deopt_count_at_bci("m", 7), 1);
    assert_eq!(
        log.deopt_count_at_site("m", DeoptReason::ReceiverTypeChanged, 7),
        1
    );
    assert_eq!(
        log.deopt_count_at_site("m", DeoptReason::PendingException, 7),
        0
    );
}

/// Unloading `a/b` forgets `a/b`'s methods and nothing else. The old prefix
/// match was a claim about the whole key's spelling rather than about its
/// class field.
#[test]
fn clear_class_matches_the_class_field_not_a_string_prefix() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("a/b.m:()V", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("a/bc.m:()V", make_event(DeoptReason::NullCheck, 0));
    // A DIFFERENT class, whose key nonetheless begins with the exact
    // characters the old `starts_with("a/b.")` test looked for: its class
    // field is `a/b.c`, which is what the prefix match could not see.
    log.record_deopt("a/b.c.m:()V", make_event(DeoptReason::NullCheck, 0));
    log.clear_class("a/b");
    assert_eq!(log.deopt_count("a/b.m:()V"), 0);
    assert_eq!(log.deopt_count("a/bc.m:()V"), 1, "a different class");
    assert_eq!(
        log.deopt_count("a/b.c.m:()V"),
        1,
        "a different class whose key merely begins with the prefix"
    );
}

/// gc-common w36-c: the batch form leaves exactly what a loop of the
/// single form leaves, for the empty set, an absent name, and a mixed set
/// that includes a malformed key (which no class matches).
#[test]
fn clear_classes_matches_a_loop_of_clear_class() {
    fn fill() -> DeoptimizationLog {
        let mut log = DeoptimizationLog::new();
        for key in [
            "a/b.m:()V",
            "a/b.n:(I)V",
            "a/bc.m:()V",
            "a/b.c.m:()V",
            "x/Y.run:()V",
            "",
            "no-descriptor",
        ] {
            log.record_deopt(key, make_event(DeoptReason::NullCheck, 0));
        }
        log
    }
    fn keys(log: &DeoptimizationLog) -> Vec<String> {
        let mut keys: Vec<String> = log.methods.keys().map(|k| k.to_string()).collect();
        keys.sort();
        keys
    }
    let sets: [&[&str]; 4] = [
        &[],
        &["absent/Class"],
        &["a/b"],
        &["a/b", "a/b.c", "x/Y", "q"],
    ];
    for set in sets {
        let mut looped = fill();
        for name in set {
            looped.clear_class(name);
        }
        let mut batched = fill();
        batched.clear_classes(&set.iter().copied().collect());
        assert_eq!(keys(&looped), keys(&batched), "set {set:?}");
    }
    let mut all = fill();
    all.clear_classes(&["a/b", "a/b.c", "x/Y"].into_iter().collect());
    assert_eq!(keys(&all), vec!["", "a/bc.m:()V", "no-descriptor"]);
}

/// Charged counts halve once a decay period has passed, so twenty traps
/// spread over days do not read like twenty in a second. Exercised against
/// the ledger directly: the log reads a monotonic process clock that a test
/// cannot wind forward.
#[test]
fn charged_counts_halve_once_a_decay_period_has_passed() {
    let mut ledger = MethodTrapLedger::new(0);
    for _ in 0..20 {
        ledger.charge(DeoptReason::ClassCheck, 5);
    }
    assert_eq!(ledger.decayed_total(0), 20);
    // A read one period later already sees the decay, because
    // `recommend_action` is asked BEFORE `record_deopt` writes.
    assert_eq!(ledger.decayed_total(crate::tiered::TRAP_DECAY_MS), 10);
    assert_eq!(ledger.decayed_total(crate::tiered::TRAP_DECAY_MS * 2), 5);

    ledger.decay(crate::tiered::TRAP_DECAY_MS * 2);
    assert_eq!(ledger.charged_total, 5, "and the write applies both");
    assert_eq!(
        ledger.decayed_site(
            &(DeoptReason::ClassCheck, 5),
            crate::tiered::TRAP_DECAY_MS * 2
        ),
        5
    );
}

/// A ledger idle for a very long time is not left permanently owing more
/// halvings than it can apply — which would make every later charge read
/// as zero.
#[test]
fn a_long_idle_ledger_still_counts_its_next_trap() {
    let mut ledger = MethodTrapLedger::new(0);
    ledger.charge(DeoptReason::ClassCheck, 5);
    let far = crate::tiered::TRAP_DECAY_MS * 10_000;
    ledger.decay(far);
    assert_eq!(ledger.charged_total, 0, "everything decayed away");
    ledger.charge(DeoptReason::ClassCheck, 5);
    assert_eq!(
        ledger.decayed_total(far),
        1,
        "and the ledger is caught up, so the next trap counts"
    );
}

/// r11-tier: charges that landed after the site table was full are halved
/// by a decay like every other charge, not erased by it. `decay` used to
/// recompute the total from the site rows alone, so 64 sites at 1 plus 100
/// overflow charges read 0 after one period instead of 82.
#[test]
fn overflow_charges_survive_a_decay() {
    let mut ledger = MethodTrapLedger::new(0);
    for bci in 0..MAX_TRAP_SITES_PER_METHOD as u32 {
        ledger.charge(DeoptReason::ClassCheck, bci);
        ledger.charge(DeoptReason::ClassCheck, bci);
    }
    for bci in 0..100u32 {
        ledger.charge(DeoptReason::ClassCheck, 10_000 + bci);
    }
    let before = ledger.charged_total;
    assert_eq!(before as usize, MAX_TRAP_SITES_PER_METHOD * 2 + 100);
    ledger.decay(crate::tiered::TRAP_DECAY_MS);
    assert_eq!(
        ledger.charged_total,
        before / 2,
        "one halving of every charge, the overflow included"
    );
    assert_eq!(
        ledger.charged_total,
        ledger.decayed_total(crate::tiered::TRAP_DECAY_MS)
    );
}

/// A trap site that keeps failing decays to zero and is dropped, so the
/// site table does not grow monotonically either.
#[test]
fn a_decayed_out_trap_site_is_dropped_from_the_table() {
    let mut ledger = MethodTrapLedger::new(0);
    ledger.charge(DeoptReason::NullCheck, 1);
    ledger.decay(crate::tiered::TRAP_DECAY_MS);
    assert!(ledger.charged.is_empty());
    assert_eq!(ledger.charged_total, 0);
}

/// A method charged `PER_METHOD_TRAP_CUTOFF` traps over its life is
/// retired, however slowly they arrived (the decayed count alone would
/// recompile it forever). Uncharged reasons still never retire it.
#[test]
fn a_lifetime_of_charged_traps_retires_the_method() {
    let mut log = DeoptimizationLog::new_with_threshold(20);
    log.record_deopt("m", make_event(DeoptReason::ClassCheck, 0));
    if let Some(ledger) = log.methods.get_mut("m") {
        ledger.lifetime_charged = PER_METHOD_TRAP_CUTOFF - 1;
    }
    assert_eq!(
        log.recommend_action("m", DeoptReason::ClassCheck),
        DeoptAction::RecompileAndReinterpret
    );
    log.record_deopt("m", make_event(DeoptReason::ClassCheck, 0));
    assert_eq!(
        log.recommend_action("m", DeoptReason::ClassCheck),
        DeoptAction::MakeNotCompilable
    );
    assert_ne!(
        log.recommend_action("m", DeoptReason::PendingException),
        DeoptAction::MakeNotCompilable
    );
}

#[test]
fn log_recommend_action_first_deopt() {
    let log = DeoptimizationLog::new_with_threshold(10);
    assert_eq!(
        log.recommend_action("m", DeoptReason::NullCheck),
        DeoptAction::Reinterpret
    );
}

#[test]
fn log_recommend_action_few_deopts() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
    assert_eq!(
        log.recommend_action("m", DeoptReason::NullCheck),
        DeoptAction::RecompileAndReinterpret
    );
}

#[test]
fn log_recommend_action_many_deopts() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    for i in 0..6 {
        log.record_deopt("m", make_event(DeoptReason::NullCheck, i));
    }
    assert_eq!(
        log.recommend_action("m", DeoptReason::NullCheck),
        DeoptAction::MakeNotEntrant
    );
}

#[test]
fn log_recommend_action_too_many() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    for i in 0..10 {
        log.record_deopt("m", make_event(DeoptReason::NullCheck, i));
    }
    assert_eq!(
        log.recommend_action("m", DeoptReason::NullCheck),
        DeoptAction::MakeNotCompilable
    );
}

#[test]
fn log_multiple_reasons() {
    let mut log = DeoptimizationLog::new();
    log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
    log.record_deopt("m", make_event(DeoptReason::BoundsCheck, 1));
    log.record_deopt("m", make_event(DeoptReason::DivByZero, 2));
    let hist = log.history("m");
    assert_eq!(hist.len(), 3);
    assert_eq!(hist[0].reason, DeoptReason::NullCheck);
    assert_eq!(hist[1].reason, DeoptReason::BoundsCheck);
    assert_eq!(hist[2].reason, DeoptReason::DivByZero);
}

#[test]
fn log_event_timestamp() {
    let event = DeoptEvent {
        reason: DeoptReason::NullCheck,
        action: DeoptAction::Reinterpret,
        bci: 0,
        timestamp_ms: 123456789,
        speculation_id: 7,
    };
    assert_eq!(event.timestamp_ms, 123456789);
    assert_eq!(event.speculation_id, 7);
}

// -- reconstruct_frame -------------------------------------------------

#[test]
fn reconstruct_frame_basic() {
    let dp = simple_deopt_point();
    let rf = reconstruct_frame_from_machine_state(&dp, &SavedRegisters::default(), 0);
    assert_eq!(rf.method_key, "Foo.bar:()V");
    assert_eq!(rf.bci, 10);
    assert_eq!(rf.locals.len(), 2);
    assert_eq!(rf.stack.len(), 1);
    assert!(rf.caller_frames.is_empty());
}

#[test]
fn reconstruct_frame_with_inlined_caller() {
    let outer = FrameState {
        class_id: 0,
        method_key: "Outer.run:()V".to_string(),
        bci: 5,
        locals: vec![FrameValue::Int(1)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let inner = FrameState {
        class_id: 0,
        method_key: "Inner.go:()V".to_string(),
        bci: 20,
        locals: vec![FrameValue::Int(2)],
        stack: vec![FrameValue::Int(3)],
        monitors: Vec::new(),
        caller: Some(Box::new(outer)),
    };
    let dp = DeoptimizationPoint {
        native_offset: 0x200,
        bci: 20,
        reason: DeoptReason::ClassCheck,
        action: DeoptAction::RecompileAndReinterpret,
        speculation_id: 1,
        frame_state: inner,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &SavedRegisters::default(), 0);
    assert_eq!(rf.method_key, "Inner.go:()V");
    assert_eq!(rf.caller_frames.len(), 1);
    assert_eq!(rf.caller_frames[0].method_key, "Outer.run:()V");
    assert_eq!(rf.caller_frames[0].bci, 5);
}

// -- count_virtual_objects ----------------------------------------------
//
// (The placeholder `materialize_virtual_objects` this section also tested
// was deleted in round 11 wave 3; the real materializer is the VM's
// `deopt_materialize::materialize_virtual_objects`.)

#[test]
fn count_virtual_objects_counts_locals_and_stack() {
    let frame = FrameState {
        class_id: 0,
        method_key: "M".to_string(),
        bci: 0,
        locals: vec![
            FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 0,
                class_id: 1,
                num_fields: 1,
                field_values: vec![FrameValue::Int(10)],
            }),
            FrameValue::Int(5),
        ],
        stack: vec![FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 1,
            class_id: 2,
            num_fields: 0,
            field_values: Vec::new(),
        })],
        monitors: Vec::new(),
        caller: None,
    };
    assert_eq!(count_virtual_objects(&frame), 2);
}

#[test]
fn count_virtual_objects_none() {
    let frame = FrameState {
        class_id: 0,
        method_key: "M".to_string(),
        bci: 0,
        locals: vec![FrameValue::Int(1)],
        stack: vec![FrameValue::Int(2)],
        monitors: Vec::new(),
        caller: None,
    };
    assert_eq!(count_virtual_objects(&frame), 0);
}

#[test]
fn log_recommend_action_receiver_type_changed() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    log.record_deopt("m", make_event(DeoptReason::ReceiverTypeChanged, 0));
    // ReceiverTypeChanged should aggressively recompile regardless of count.
    assert_eq!(
        log.recommend_action("m", DeoptReason::ReceiverTypeChanged),
        DeoptAction::RecompileAndReinterpret,
    );
}

#[test]
fn log_recommend_action_not_compiled() {
    let log = DeoptimizationLog::new_with_threshold(10);
    // NotCompiled should immediately give up.
    assert_eq!(
        log.recommend_action("m", DeoptReason::NotCompiled),
        DeoptAction::MakeNotCompilable,
    );
}

#[test]
fn log_recommend_action_unreached_code() {
    let log = DeoptimizationLog::new_with_threshold(10);
    assert_eq!(
        log.recommend_action("m", DeoptReason::UnreachedCode),
        DeoptAction::MakeNotCompilable,
    );
}

#[test]
fn log_recommend_action_transfer_to_interpreter() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    for i in 0..5 {
        log.record_deopt("m", make_event(DeoptReason::TransferToInterpreter, i));
    }
    // TransferToInterpreter always reinterprets regardless of count.
    assert_eq!(
        log.recommend_action("m", DeoptReason::TransferToInterpreter),
        DeoptAction::Reinterpret,
    );
}

#[test]
fn log_recommend_action_speculation_failed_recompiles() {
    let mut log = DeoptimizationLog::new_with_threshold(10);
    log.record_deopt("m", make_event(DeoptReason::SpeculationFailed, 0));
    assert_eq!(
        log.recommend_action("m", DeoptReason::SpeculationFailed),
        DeoptAction::RecompileAndReinterpret,
    );
}

/// W17L4-5: the FIRST unstable-if trap recompiles (an `UncommonTrap`'s
/// first answer is `Reinterpret`, which keeps the pruned body trapping),
/// and the aggregate backstop still retires a method that keeps trapping.
#[test]
fn log_recommend_action_unstable_if_recompiles_on_the_first_trap() {
    let mut log = DeoptimizationLog::new_with_threshold(4);
    assert_eq!(
        log.recommend_action("m", DeoptReason::UnstableIf),
        DeoptAction::RecompileAndReinterpret,
    );
    assert_eq!(
        log.recommend_action("m", DeoptReason::UncommonTrap),
        DeoptAction::Reinterpret,
    );
    for i in 0..4 {
        log.record_deopt("m", make_event(DeoptReason::UnstableIf, i));
    }
    assert_eq!(
        log.recommend_action("m", DeoptReason::UnstableIf),
        DeoptAction::MakeNotCompilable,
    );
    assert!(!ResumeSemantics::for_reason(DeoptReason::UnstableIf).rethrow_exception);
    assert!(crate::tiered::deopt_reason_is_charged(
        DeoptReason::UnstableIf
    ));
}

#[test]
fn log_recommend_action_class_check_gives_up_at_threshold() {
    let mut log = DeoptimizationLog::new_with_threshold(3);
    for i in 0..3 {
        log.record_deopt("m", make_event(DeoptReason::ClassCheck, i));
    }
    assert_eq!(
        log.recommend_action("m", DeoptReason::ClassCheck),
        DeoptAction::MakeNotCompilable,
    );
}

#[test]
fn reconstruct_frame_monitors() {
    let fs = FrameState {
        class_id: 0,
        method_key: "Sync.lock:()V".to_string(),
        bci: 3,
        locals: vec![FrameValue::Object(0x1000)],
        stack: Vec::new(),
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(0x1000),
            lock_depth: 1,
            relock: true,
        }],
        caller: None,
    };
    let dp = DeoptimizationPoint {
        native_offset: 0x50,
        bci: 3,
        reason: DeoptReason::TransferToInterpreter,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&dp, &SavedRegisters::default(), 0);
    assert_eq!(rf.monitors.len(), 1);
    assert_eq!(rf.monitors[0].lock_depth, 1);
}
