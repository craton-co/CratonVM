//! JIT round 19 wave 2 (lane throws2): a walk whose inline chains are
//! SHARED with the artifact (THROWS18-2,
//! `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS=1`) lays out, builds and
//! memo-compares exactly as the same walk with copied chains, through the
//! plain door, the fused door and THROWS19-8's memo-miss fused door.
use super::{
    backtrace_frame_kind, capture_throwable_backtrace_fused_returning_walk,
    capture_throwable_backtrace_fused_with, capture_throwable_backtrace_returning_walk,
    same_compiled_frame, trace_from_compiled_layout_with, BacktraceFrame, OsrBciOverrides,
    StackTraceEntry,
};
use crate::classloading::ClassId;
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel, SharedInlineChain};
use crate::runtime::frame::Frame;
use std::sync::Arc;

fn interp(class: &str, method: &str, descriptor: &str) -> Frame {
    Frame::new(
        ClassId::new(7),
        class.to_string(),
        method.to_string(),
        descriptor.to_string(),
        None,
        vec![0; 16],
        vec![],
        4,
        4,
        &[],
    )
}

fn compiled(
    depth: u32,
    label: &Arc<str>,
    bci: i32,
    chain: Vec<InlinedLevel>,
) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index: 0,
        label: Arc::clone(label),
        owner_class_id: 40,
        cm_ptr: 0,
        bci,
        inline_chain: chain,
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: Some(3),
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

fn level(label: &str, bci: u32, class_id: u32) -> InlinedLevel {
    InlinedLevel {
        label: Arc::from(label),
        bci,
        class_id,
        hidden: None,
    }
}

fn backtrace_text(trace: &[BacktraceFrame]) -> Vec<String> {
    trace
        .iter()
        .map(|f| {
            let e = f.to_entry();
            format!(
                "{} {}.{}{} @{} id={:?} stamp={:?}",
                backtrace_frame_kind(f),
                e.class_name,
                e.method_name,
                e.method_descriptor.as_deref().unwrap_or(""),
                e.byte_code_index,
                e.class_id,
                f.compile_cp_stamp(),
            )
        })
        .collect()
}

fn entry_text(trace: &[StackTraceEntry]) -> Vec<String> {
    trace
        .iter()
        .map(|e| {
            format!(
                "{}.{}{} @{} id={:?}",
                e.class_name,
                e.method_name,
                e.method_descriptor.as_deref().unwrap_or(""),
                e.byte_code_index,
                e.class_id,
            )
        })
        .collect()
}

fn fill_holder(holder: &str) -> bool {
    holder == "java/lang/Throwable" || holder == "java/lang/RuntimeException"
}

/// A compiled recursion whose every activation inlined a two-level
/// chain, an unparsable label, a factory with an over-bound level, and
/// the throwing activation with its inlined `<init>` chain under a
/// compiled `fillInStackTrace`.
fn shape() -> (Vec<Frame>, Vec<ActiveCompiledFrame>) {
    let frames = vec![
        interp("probe/Sh", "main", "([Ljava/lang/String;)V"),
        interp("probe/Sh", "drive", "(I)V"),
        interp("probe/Sh", "loop", "(I)V"),
    ];
    let rec: Arc<str> = Arc::from("probe/Sh.rec:(I)V");
    let a: Arc<str> = Arc::from("probe/Sh.altA:(I)J");
    let bad: Arc<str> = Arc::from("no-colon-here");
    let fill: Arc<str> =
        Arc::from("java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;");
    let mut jit = Vec::new();
    for i in 0..30 {
        jit.push(compiled(
            1,
            &rec,
            5 + (i % 3),
            vec![
                level("probe/Sh.step:(I)I", 4, 41),
                level("probe/Sh.hop:(I)I", 6, 41),
            ],
        ));
    }
    jit.push(compiled(
        2,
        &bad,
        1,
        vec![level("probe/Sh.lost:()V", 1, 41)],
    ));
    jit.push(compiled(
        2,
        &a,
        9,
        vec![
            level("probe/Sh.made:(I)Ljava/lang/RuntimeException;", 3, 41),
            level("probe/Sh.factory:(I)V", 2, 41),
            level("probe/Sh.overBound:()V", u32::MAX, 41),
        ],
    ));
    jit.push(compiled(
        3,
        &a,
        13,
        vec![
            level("java/lang/Throwable.<init>:(Ljava/lang/String;)V", 8, 1),
            level(
                "java/lang/RuntimeException.<init>:(Ljava/lang/String;)V",
                2,
                50,
            ),
        ],
    ));
    jit.push(compiled(3, &fill, 1, vec![]));
    (frames, jit)
}

/// The same walk with every non-empty chain moved into the shared field,
/// as the shared-chain walk reports it.
fn shared(jit: &[ActiveCompiledFrame]) -> Vec<ActiveCompiledFrame> {
    jit.iter()
        .cloned()
        .map(|mut f| {
            if !f.inline_chain.is_empty() {
                let chain: SharedInlineChain = Arc::from(std::mem::take(&mut f.inline_chain));
                f.inline_chain_shared = Some(chain);                }
            f
        })
        .collect()
}

#[test]
fn a_shared_chain_walk_reads_as_the_copied_one() {
    let (_, jit) = shape();
    let sh = shared(&jit);
    assert!(sh.iter().any(|f| f.inline_chain_shared.is_some()));
    for (owned, shared) in jit.iter().zip(&sh) {
        assert!(same_compiled_frame(owned, shared));
        assert!(same_compiled_frame(shared, owned));
        assert_eq!(owned.chain(), shared.chain());
        assert_eq!(
            owned.clone().into_chain_vec(),
            shared.clone().into_chain_vec()
        );
    }
    // A differing level still differs through the shared field.
    let mut other = sh[0].clone();
    other.inline_chain_shared = Some(Arc::from(vec![level("probe/Sh.step:(I)I", 5, 41)]));
    assert!(!same_compiled_frame(&jit[0], &other));
}

#[test]
fn every_door_answers_a_shared_chain_walk_alike() {
    let (frames, jit) = shape();
    let sh = shared(&jit);
    let no_osr = OsrBciOverrides::default();
    for max_depth in [0usize, 1, 2, 5, 17, 40, 64, 1024] {
        for trims in [false, true] {
            let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
            let plain = backtrace_text(&trace_from_compiled_layout_with::<BacktraceFrame>(
                &frames,
                &jit,
                &no_osr,
                max_depth,
                &mut is_a,
                &[],
                false,
            ));
            let plain_shared =
                backtrace_text(&trace_from_compiled_layout_with::<BacktraceFrame>(
                    &frames,
                    &sh,
                    &no_osr,
                    max_depth,
                    &mut is_a,
                    &[],
                    false,
                ));
            let entries = entry_text(&trace_from_compiled_layout_with::<StackTraceEntry>(
                &frames,
                &jit,
                &no_osr,
                max_depth,
                &mut is_a,
                &[],
                true,
            ));
            let entries_shared =
                entry_text(&trace_from_compiled_layout_with::<StackTraceEntry>(
                    &frames,
                    &sh,
                    &no_osr,
                    max_depth,
                    &mut is_a,
                    &[],
                    true,
                ));
            let fused = backtrace_text(&capture_throwable_backtrace_fused_with(
                &frames,
                jit.clone(),
                max_depth,
                false,
                &mut is_a,
                &[],
            ));
            let fused_shared = backtrace_text(&capture_throwable_backtrace_fused_with(
                &frames,
                sh.clone(),
                max_depth,
                false,
                &mut is_a,
                &[],
            ));
            let at = format!("max_depth={max_depth} trims={trims}");
            assert_eq!(plain_shared, plain, "plain {at}");
            assert_eq!(entries_shared, entries, "entries {at}");
            assert_eq!(fused, plain, "fused {at}");
            assert_eq!(fused_shared, plain, "fused shared {at}");
            // THROWS19-8: the memo-miss fused door is the plain door's
            // trace and hands back the walk it was given.
            let (miss_plain, walk_plain) = capture_throwable_backtrace_returning_walk(
                &frames,
                sh.clone(),
                max_depth,
                &mut is_a,
                &[],
            );
            let (miss_fused, walk_fused) = capture_throwable_backtrace_fused_returning_walk(
                &frames,
                sh.clone(),
                max_depth,
                false,
                &mut is_a,
                &[],
            );
            assert_eq!(backtrace_text(&miss_plain), plain, "miss plain {at}");
            assert_eq!(backtrace_text(&miss_fused), plain, "miss fused {at}");
            assert_eq!(walk_plain.len(), sh.len());
            assert_eq!(walk_fused.len(), sh.len());
            for ((p, f), o) in walk_plain.iter().zip(&walk_fused).zip(&jit) {
                assert!(
                    same_compiled_frame(p, o) && same_compiled_frame(f, o),
                    "{at}"
                );
            }
        }
    }
}
