use super::{
    arm_trap_capture, lookup_memo_on_armed_trap_capture, BacktraceFrame, ThrowableCaptureKey,
    ThrowableCaptureMemo, ThrowableCaptureMemos,
};
use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use std::sync::Arc;

fn compiled(method: &str, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        frame_rbp: None,
        interp_depth: 0,
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

fn trace_of(frames: &[ActiveCompiledFrame]) -> Arc<[BacktraceFrame]> {
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

fn method(name: &str) -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(2),
            class_name: Arc::from("probe/Cached"),
            method_name: Arc::from(name),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(vec![0xb1u8]),
            exception_table: Arc::from(Vec::new()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ))
}

/// A hit hands out the memo's own frames: the stored trace and the
/// memo are one allocation, and every hit is that allocation again.
#[test]
fn a_hit_shares_the_memos_trace() {
    let walk = vec![compiled("phase", 12)];
    let trace = trace_of(&walk);
    let memo = ThrowableCaptureMemo::new_shared(
        key(),
        &[],
        walk.clone(),
        Default::default(),
        Arc::clone(&trace),
    )
    .expect("memoisable");
    let a = memo.lookup(&key(), &[], &walk).expect("hit");
    let b = memo.lookup(&key(), &[], &walk).expect("hit");
    assert!(Arc::ptr_eq(&a, &trace) && Arc::ptr_eq(&b, &trace));
    // `new` from a borrowed slice still answers with equal frames.
    let copied =
        ThrowableCaptureMemo::new(key(), &[], walk.clone(), Default::default(), &trace)
            .expect("memoisable");
    let c = copied.lookup(&key(), &[], &walk).expect("hit");
    assert_eq!(format!("{:?}", c[0]), format!("{:?}", trace[0]));
}

/// The armed trap snapshot is compared in place: a memo made from the
/// same frames answers and marks the snapshot used; another bci, or a
/// capture below the snapshot's base depth, does not.
#[test]
fn the_armed_snapshot_is_compared_without_a_clone() {
    let walk = vec![compiled("trap", 7)];
    let trace = trace_of(&walk);
    let mut memos = ThrowableCaptureMemos::default();
    memos.remember(
        ThrowableCaptureMemo::new_shared(
            key(),
            &[],
            walk.clone(),
            Default::default(),
            Arc::clone(&trace),
        )
        .expect("memoisable"),
    );
    // Nothing armed: no answer.
    assert!(lookup_memo_on_armed_trap_capture(&mut memos, &key(), &[]).is_none());
    let armed = arm_trap_capture(walk.clone(), 0).expect("armed");
    let hit = lookup_memo_on_armed_trap_capture(&mut memos, &key(), &[]).expect("hit");
    assert!(Arc::ptr_eq(&hit, &trace));
    let (frames, used) = armed.disarm();
    assert!(used, "a hit uses the snapshot");
    assert_eq!(frames.map(|f| f.len()), Some(1));
    let armed = arm_trap_capture(vec![compiled("trap", 8)], 0).expect("armed");
    assert!(lookup_memo_on_armed_trap_capture(&mut memos, &key(), &[]).is_none());
    let (_, used) = armed.disarm();
    assert!(!used, "a miss leaves the snapshot to the walk");
    let armed = arm_trap_capture(walk, 1).expect("armed");
    assert!(
        lookup_memo_on_armed_trap_capture(&mut memos, &key(), &[]).is_none(),
        "a capture below the snapshot's base walks"
    );
    drop(armed);
}

/// The precise-locals answer is computed once per method record, and a
/// record of the same text is another key.
#[test]
fn precise_locals_are_computed_once_per_record() {
    let mut memos = ThrowableCaptureMemos::default();
    let a = method("a");
    let mut calls = 0;
    for _ in 0..3 {
        assert!(memos.handler_precise_locals(&a, || {
            calls += 1;
            true
        }));
    }
    assert_eq!(calls, 1);
    let twin = method("a");
    assert!(!memos.handler_precise_locals(&twin, || {
        calls += 1;
        false
    }));
    assert_eq!(calls, 2);
    // Round robin over the ways: four more records evict `a`.
    let others: Vec<_> = (0..4).map(|i| method(&format!("m{i}"))).collect();
    for m in &others {
        memos.handler_precise_locals(m, || false);
    }
    assert!(!memos.handler_precise_locals(&a, || {
        calls += 1;
        false
    }));
    assert_eq!(calls, 3);
}

/// The reflective raise site is remembered under its key; an unknown
/// answer is not (nio21 wave 17 L1: an absent one is).
#[test]
fn the_reflective_raise_site_is_keyed() {
    use super::RaiseSiteLookup as L;
    use super::ReflectiveRaiseCause as Cause;
    use cratonvm_native_api::ReflectiveCallKind;
    let mut memos = ThrowableCaptureMemos::default();
    let kind = ReflectiveCallKind::MethodInvoke;
    let found = (ClassId::new(4), 2usize, 40usize);
    let site = Some(found);
    let ite = "java/lang/reflect/InvocationTargetException";
    let mut calls = 0;
    let mut ask = |memos: &mut ThrowableCaptureMemos, class: &str, cause, answer: L| {
        memos.reflective_raise_site(kind, class, cause, || {
            calls += 1;
            answer
        })
    };
    assert_eq!(ask(&mut memos, ite, Cause::Other, L::Found(found)), site);
    assert_eq!(
        ask(&mut memos, ite, Cause::Other, L::Unknown),
        site,
        "remembered"
    );
    assert_eq!(
        ask(&mut memos, ite, Cause::ClassCast, L::Unknown),
        None,
        "another arm"
    );
    assert_eq!(
        ask(&mut memos, ite, Cause::Other, L::Found(found)),
        site,
        "kept across a miss"
    );
    assert_eq!(
        ask(&mut memos, "java/lang/Error", Cause::Other, L::Unknown),
        None
    );
    drop(ask);
    assert_eq!(calls, 3);
}

/// nio21 wave 17 L1: an absent site is remembered beside a found one, so a
/// compiled target's own throwable and the wrapper alternate without a
/// recompute; four ways round robin.
#[test]
fn an_absent_reflective_raise_site_is_remembered_beside_a_found_one() {
    use super::RaiseSiteLookup as L;
    use super::ReflectiveRaiseCause as Cause;
    use cratonvm_native_api::ReflectiveCallKind;
    let mut memos = ThrowableCaptureMemos::default();
    let kind = ReflectiveCallKind::MethodInvoke;
    let found = (ClassId::new(4), 2usize, 40usize);
    let ite = "java/lang/reflect/InvocationTargetException";
    let ise = "java/lang/IllegalStateException";
    let mut calls = 0;
    let mut ask = |memos: &mut ThrowableCaptureMemos, class: &str, answer: L| {
        memos.reflective_raise_site(kind, class, Cause::Other, || {
            calls += 1;
            answer
        })
    };
    for _ in 0..3 {
        assert_eq!(ask(&mut memos, ise, L::Absent), None);
        assert_eq!(ask(&mut memos, ite, L::Found(found)), Some(found));
    }
    drop(ask);
    assert_eq!(calls, 2, "one compute per key");
    // Four more keys evict both.
    let mut calls = 0;
    for i in 0..4 {
        let name = format!("p/E{i}");
        memos.reflective_raise_site(kind, &name, Cause::Other, || {
            calls += 1;
            L::Absent
        });
    }
    assert_eq!(
        memos.reflective_raise_site(kind, ise, Cause::Other, || {
            calls += 1;
            L::Absent
        }),
        None
    );
    assert_eq!(calls, 5);
}
