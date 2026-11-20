use super::{
    drop_osr_continuations, same_compiled_frame, throwable_capture_memoisable, BacktraceFrame,
    ThrowableCaptureKey, ThrowableCaptureMemo,
};
use crate::classloading::ClassId;
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use crate::runtime::frame::Frame;
use std::sync::Arc;

fn owned_frame(method: &str) -> Frame {
    Frame::new(
        ClassId::new(1),
        "probe/Target".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        4,
        4,
        &[],
    )
}

fn compiled(interp_depth: u32, method: &str, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth,
        chain_index: 0,
        label: Arc::from(format!("probe/Target.{method}:()V")),
        owner_class_id: 1,
        cm_ptr: 0x1000,
        bci,
        inline_chain: vec![InlinedLevel {
            label: Arc::from("java/lang/Throwable.<init>:(Ljava/lang/String;)V"),
            bci: 4,
            class_id: 2,
            hidden: None,
        }],
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: Some(0),
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

fn key() -> ThrowableCaptureKey {
    ThrowableCaptureKey {
        throwable_class: ClassId::new(9),
        redefinitions: 0,
        max_depth: 1024,
        thread_obj: 0x10,
        loader_miss: None,
    }
}

fn trace_of(frames: &[ActiveCompiledFrame]) -> Vec<BacktraceFrame> {
    frames
        .iter()
        .map(|f| BacktraceFrame::Compiled {
            label: Arc::clone(&f.label),
            owner_class_id: f.owner_class_id,
            bci: f.bci,
            cp_stamp: f.compile_cp_stamp,
        })
        .collect()
}

#[test]
fn every_field_of_a_compiled_frame_takes_part() {
    let base = compiled(0, "phase", 12);
    assert!(same_compiled_frame(&base, &compiled(0, "phase", 12)));
    let changes: [fn(&mut ActiveCompiledFrame); 14] = [
        |f| f.interp_depth = 1,
        |f| f.chain_index = 1,
        |f| f.label = Arc::from("probe/Target.other:()V"),
        |f| f.owner_class_id = 3,
        |f| f.cm_ptr = 0x2000,
        |f| f.bci = 13,
        |f| f.chain_exact = false,
        |f| f.trap_site_exact = true,
        |f| f.chain_poisoned = true,
        |f| f.compile_cp_stamp = Some(1),
        |f| f.inline_chain.clear(),
        |f| f.inline_chain[0].bci = 5,
        |f| f.inline_chain[0].class_id = 0,
        |f| f.inline_chain[0].label = Arc::from("java/lang/Exception.<init>:()V"),
    ];
    for (i, change) in changes.iter().enumerate() {
        let mut other = compiled(0, "phase", 12);
        change(&mut other);
        assert!(!same_compiled_frame(&base, &other), "change {i}");
    }
}

/// JIT round 20 wave 12 (lane qMisc, L4W15-2): a memo made under
/// reflective splices answers only a capture with the same splices (their
/// depths, and their frame lists and raise lists by allocation), and an
/// unspliced memo never answers a spliced capture.
#[test]
fn a_spliced_memo_answers_the_same_splices_only() {
    use super::{unparsed_label_entry, ReflectiveSplice};
    use crate::native::registry::StackTraceEntry;
    let walk = vec![compiled(0, "phase", 12)];
    let trace = trace_of(&walk);
    let entries: Arc<[StackTraceEntry]> = Arc::from(vec![unparsed_label_entry(
        &Arc::from("jdk/internal/reflect/DirectMethodHandleAccessor.invoke"),
        None,
        3,
    )]);
    let splice = |entries: &Arc<[StackTraceEntry]>, raise: Option<Arc<[StackTraceEntry]>>| {
        ReflectiveSplice {
            interp_depth: 0,
            jit_depth: 0,
            entries: Arc::clone(entries),
            raise,
        }
    };
    let spliced = vec![splice(&entries, None)];
    let memo = ThrowableCaptureMemo::new(key(), &[], walk.clone(), Default::default(), &trace)
        .expect("memoisable")
        .with_splices(&spliced);
    assert!(memo.lookup_spliced(&key(), &[], &walk, &spliced).is_some());
    assert!(
        memo.lookup(&key(), &[], &walk).is_none(),
        "an unspliced capture"
    );
    let copy: Arc<[StackTraceEntry]> = Arc::from(entries.to_vec());
    assert!(
        memo.lookup_spliced(&key(), &[], &walk, &[splice(&copy, None)])
            .is_none(),
        "equal frames in another list"
    );
    let mut deeper = spliced.clone();
    deeper[0].jit_depth = 1;
    assert!(
        memo.lookup_spliced(&key(), &[], &walk, &deeper).is_none(),
        "another depth"
    );
    let raised = [splice(&entries, Some(Arc::clone(&entries)))];
    assert!(
        memo.lookup_spliced(&key(), &[], &walk, &raised).is_none(),
        "a raise"
    );
    let plain = ThrowableCaptureMemo::new(key(), &[], walk.clone(), Default::default(), &trace)
        .expect("memoisable");
    assert!(plain.lookup_spliced(&key(), &[], &walk, &spliced).is_none());
    assert!(plain.lookup(&key(), &[], &walk).is_some());
}

#[test]
fn a_memo_answers_its_own_capture_only() {
    let walk = vec![compiled(0, "phase", 12), compiled(0, "fillInStackTrace", 1)];
    let trace = trace_of(&walk[..1]);
    let memo = ThrowableCaptureMemo::new(key(), &[], walk.clone(), Default::default(), &trace)
        .expect("memoisable");
    let hit = memo.lookup(&key(), &[], &walk).expect("same capture");
    assert_eq!(hit.len(), 1);
    assert_eq!(format!("{:?}", hit[0]), format!("{:?}", trace[0]));
    let mut moved = walk.clone();
    moved[0].bci = 20;
    assert!(memo.lookup(&key(), &[], &moved).is_none(), "another bci");
    assert!(
        memo.lookup(&key(), &[], &walk[..1]).is_none(),
        "another depth"
    );
    for other in [
        ThrowableCaptureKey {
            throwable_class: ClassId::new(8),
            ..key()
        },
        ThrowableCaptureKey {
            redefinitions: 1,
            ..key()
        },
        ThrowableCaptureKey {
            max_depth: 1,
            ..key()
        },
        ThrowableCaptureKey {
            thread_obj: 0x20,
            ..key()
        },
        // Interpreter round i3 wave 73, lane L2: a served loader's miss.
        ThrowableCaptureKey {
            loader_miss: Some(ClassId::new(3)),
            ..key()
        },
    ] {
        assert!(memo.lookup(&other, &[], &walk).is_none(), "{other:?}");
    }
}

/// 2026-10-07: the ways keep several sites; a walk-compared hit adds its
/// raw key to the way that answered (an unrecorded one is not kept); the
/// back-off skips the memo path after an unproductive window and tries
/// again after it.
#[test]
fn memo_ways_aliases_and_back_off() {
    use super::{
        ThrowableCaptureMemos, THROWABLE_CAPTURE_MEMO_BACKOFF, THROWABLE_CAPTURE_MEMO_WINDOW,
    };
    let mut memos = ThrowableCaptureMemos::default();
    let a = vec![compiled(0, "leaf", 3)];
    let b = vec![compiled(0, "leaf", 9)];
    for walk in [&a, &b] {
        let memo = ThrowableCaptureMemo::new(key(), &[], walk.clone(), Default::default(), &[])
            .expect("memoisable");
        memos.remember(memo);
    }
    assert_eq!(memos.lookup(&key(), &[], &a).map(|(way, _)| way), Some(0));
    assert_eq!(memos.lookup(&key(), &[], &b).map(|(way, _)| way), Some(1));
    assert!(memos
        .lookup(&key(), &[], &[compiled(0, "leaf", 4)])
        .is_none());
    // No raw key was recorded for these memos, so none answers without
    // the walk, and an unrecorded alias is not kept.
    assert!(memos.lookup_raw(&key(), &[]).is_none());
    memos.alias(0, Default::default());
    assert!(memos.lookup_raw(&key(), &[]).is_none());
    // Back-off: a window of misses turns the path off for a while.
    assert!(memos.attempt());
    for _ in 0..THROWABLE_CAPTURE_MEMO_WINDOW {
        memos.note_outcome(false);
    }
    for _ in 0..THROWABLE_CAPTURE_MEMO_BACKOFF {
        assert!(!memos.attempt());
    }
    assert!(memos.attempt());
    // A productive window keeps it on.
    for i in 0..THROWABLE_CAPTURE_MEMO_WINDOW {
        memos.note_outcome(i % 4 == 0);
    }
    assert!(memos.attempt());
}

/// An owned (uncached) interpreter frame is memoised by its names'
/// allocations (2026-10-07: refusing it refused every stack under the
/// launcher's `main`); the same frame matches, another owned frame of the
/// same text does not.
#[test]
fn stacks_the_memo_is_not_made_for() {
    let caller = [owned_frame("phase")];
    assert!(throwable_capture_memoisable(&caller, &[]), "an owned frame");
    let memo = ThrowableCaptureMemo::new(key(), &caller, Vec::new(), Default::default(), &[])
        .expect("an owned frame is memoised");
    assert!(memo.lookup(&key(), &caller, &[]).is_some());
    assert!(
        memo.lookup(&key(), &[owned_frame("phase")], &[]).is_none(),
        "another owned frame is not the memoised one"
    );
    assert!(throwable_capture_memoisable(
        &[],
        &[compiled(0, "phase", 1)]
    ));
    // Only a frame pushed FROM an interpreter frame of the same method
    // counts; the depth-0 entry has none.
    assert!(!super::compiled_frame_names_its_caller(
        &caller,
        &compiled(0, "phase", 1)
    ));
    assert!(super::compiled_frame_names_its_caller(
        &caller,
        &compiled(1, "phase", 1)
    ));
    assert!(!super::compiled_frame_names_its_caller(
        &caller,
        &compiled(1, "leaf", 1)
    ));
}

/// No entry names its caller: the walk comes back as it went in, with no
/// overrides.
#[test]
fn the_dedupe_keeps_a_walk_that_names_no_caller_as_it_came() {
    let frames = [owned_frame("main"), owned_frame("phase")];
    let walk = vec![
        compiled(1, "leaf", 3),
        compiled(2, "fillInStackTrace", 1),
        compiled(2, "other", 7),
    ];
    let (kept, overrides) = drop_osr_continuations(&frames, walk.clone());
    assert!(overrides.is_empty());
    assert_eq!(kept.len(), walk.len());
    for (a, b) in kept.iter().zip(&walk) {
        assert!(same_compiled_frame(a, b));
    }
}

/// Interpreter round i6 wave 127, lane L3
/// (`CRATONVM_JIT_OSR_DEDUPE_SPARES_INVOKE_CALLER`, default ON): with no
/// registry answer, rule 1's pc heuristic no longer drops the compiled
/// recursive callee of an interpreter frame suspended at an invoke, even
/// though the frame's `pc` (already past the invoke, at an empty-stack
/// `return`) is an OSR-enterable pc of the callee's artifact. A frame
/// parked at a back-edge branch is still deduped as before.
#[test]
fn the_osr_heuristic_spares_an_interpreted_caller_suspended_at_an_invoke() {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    // OSR-enterable at every pc, as the single-pass tier publishes one
    // for every empty-stack pc.
    cm.osr_pc_to_native = Some(vec![0; 16]);
    let cm_ptr = &cm as *const cratonvm_jit::CompiledMethod as usize;
    let entry = |depth: u32| {
        let mut f = compiled(depth, "phase", 7);
        f.cm_ptr = cm_ptr;
        f
    };
    // 0: aload_0  1: iconst_1  2: isub  3: invokestatic #1  6: return
    //  7: nop  8: goto -8 (a back edge to 0)
    let code = vec![0x2a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0xb1, 0x00, 0xa7, 0xff, 0xf8];
    let mut caller = owned_frame("phase");
    caller.code = Arc::from(code.clone());
    caller.last_instr_pc = 3;
    caller.pc = 6;
    let frames = [owned_frame("main"), caller];
    let (kept, overrides) =
        super::drop_osr_continuations_with(&frames, vec![entry(2)], |_| None);
    assert!(overrides.is_empty());
    assert_eq!(kept.len(), 1, "the recursive callee of a caller at an invoke survives");

    let mut parked = owned_frame("phase");
    parked.code = Arc::from(code);
    parked.last_instr_pc = 8;
    parked.pc = 0;
    let frames = [owned_frame("main"), parked];
    let (kept, overrides) =
        super::drop_osr_continuations_with(&frames, vec![entry(2)], |_| None);
    assert!(overrides.is_empty(), "the heuristic arm moves no bci");
    assert!(kept.is_empty(), "a back-edge-parked frame's continuation is still dropped");
    drop(cm);
}

/// Wave 9: a compiled frame naming its caller no longer refuses the memo;
/// its dedupe inputs are recorded instead, one per naming frame, with the
/// registry's answer for exactly that frame's index.
#[test]
fn a_naming_frame_records_its_dedupe_inputs() {
    let frames = [owned_frame("main"), owned_frame("phase")];
    assert!(throwable_capture_memoisable(
        &[],
        &[compiled(1, "phase", 1)]
    ));
    let mut naming = compiled(2, "phase", 7);
    naming.cm_ptr = 0;
    let walk = vec![compiled(1, "leaf", 3), naming, compiled(2, "other", 1)];
    let mut asked = Vec::new();
    let inputs = super::osr_dedupe_inputs(&frames, &walk, &mut |i| {
        asked.push(i);
        Some(0x77)
    });
    assert_eq!(asked, [1]);
    assert_eq!(
        inputs,
        [super::OsrDedupeInput {
            live: Some(0x77),
            can_osr_enter: false,
            suspended_at_invoke: false,
        }]
    );
    let none = super::osr_dedupe_inputs(&frames, &walk[..1], &mut |_| Some(1));
    assert!(none.is_empty());
}

/// The capture that hands its walk back lays out the same trace as the
/// one that consumes it, and returns the walk unchanged.
#[test]
fn the_walk_returning_capture_is_the_same_capture() {
    let frames = [owned_frame("main"), owned_frame("phase")];
    let walk = vec![compiled(1, "leaf", 3), compiled(2, "fillInStackTrace", 1)];
    let mut keep_all = |_: Option<ClassId>, _: &str| false;
    let plain: Vec<BacktraceFrame> =
        super::capture_throwable_slots_with(&frames, walk.clone(), 0, &mut keep_all, &[]);
    let (kept, back) = super::capture_throwable_backtrace_returning_walk(
        &frames,
        walk.clone(),
        0,
        &mut keep_all,
        &[],
    );
    assert_eq!(format!("{plain:?}"), format!("{kept:?}"));
    assert_eq!(back.len(), walk.len());
    for (a, b) in back.iter().zip(&walk) {
        assert!(same_compiled_frame(a, b));
    }
}
