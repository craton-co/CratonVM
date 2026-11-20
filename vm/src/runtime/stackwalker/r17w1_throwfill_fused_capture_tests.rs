use super::{
    backtrace_frame_kind, capture_screens, capture_screens_memoised,
    capture_throwable_backtrace_fused, capture_throwable_slots_with, BacktraceFrame,
};
use crate::classloading::ClassId;
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
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

/// Everything a retained frame carries, and its label's address.
fn describe(trace: &[BacktraceFrame]) -> Vec<(String, usize)> {
    trace
        .iter()
        .map(|f| {
            let e = f.to_entry();
            let (stamp, label_at) = match f {
                BacktraceFrame::Compiled {
                    cp_stamp, label, ..
                }
                | BacktraceFrame::Inlined {
                    cp_stamp, label, ..
                } => (*cp_stamp, label.as_ptr() as usize),
                _ => (None, 0),
            };
            (
                format!(
                    "{} {}.{}{} @{} id={:?} stamp={stamp:?}",
                    backtrace_frame_kind(f),
                    e.class_name,
                    e.method_name,
                    e.method_descriptor.as_deref().unwrap_or(""),
                    e.byte_code_index,
                    e.class_id,
                ),
                label_at,
            )
        })
        .collect()
}

fn text(trace: &[BacktraceFrame]) -> Vec<String> {
    describe(trace).into_iter().map(|(s, _)| s).collect()
}

/// A compiled recursion (one shared label), two methods calling each
/// other, an inlined factory, an unparsable label, a chain whose
/// outermost level is past the bci bound, and `new RuntimeException(..)`
/// on top: the throwing activation with its inlined `<init>` chain, then
/// the compiled `fillInStackTrace`.
fn shape() -> (Vec<Frame>, Vec<ActiveCompiledFrame>) {
    let frames = vec![
        interp("probe/Fill", "main", "([Ljava/lang/String;)V"),
        interp("probe/Fill", "drive", "(I)V"),
    ];
    let rec: Arc<str> = Arc::from("probe/Fill.down:(I)J");
    let a: Arc<str> = Arc::from("probe/Fill.altA:(I)J");
    let b: Arc<str> = Arc::from("probe/Fill.altB:(I)J");
    let bad: Arc<str> = Arc::from("no-colon-here");
    let fill: Arc<str> =
        Arc::from("java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;");
    let mut jit = vec![compiled(1, &rec, 5, vec![])];
    for i in 0..6 {
        jit.push(compiled(2, &rec, 5 + (i % 2), vec![]));
    }
    jit.push(compiled(
        2,
        &bad,
        1,
        vec![level("probe/Fill.lost:()V", 1, 9)],
    ));
    for i in 0..5 {
        jit.push(compiled(2, if i % 2 == 0 { &a } else { &b }, 7, vec![]));
    }
    // Inline chains are stored innermost first.
    jit.push(compiled(
        2,
        &a,
        9,
        vec![
            level("probe/Fill.made:(I)Ljava/lang/RuntimeException;", 3, 41),
            level("probe/Fill.factory:(I)V", 2, 41),
        ],
    ));
    jit.push(compiled(
        2,
        &b,
        11,
        vec![
            level("probe/Fill.kept:()V", 4, 0),
            level("probe/Fill.deep:()V", 70_000, 41),
        ],
    ));
    jit.push(compiled(
        2,
        &b,
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
    jit.push(compiled(2, &fill, 1, vec![]));
    (frames, jit)
}

fn fill_holder(holder: &str) -> bool {
    holder == "java/lang/Throwable" || holder == "java/lang/RuntimeException"
}

/// Every cap, with and without the fill-frame trim: the fused door's
/// trace is the plain door's, frame for frame, and every retained label
/// is an allocation the walk owned (moved, not rebuilt).
#[test]
fn the_fused_door_is_the_plain_door() {
    let (frames, jit) = shape();
    for max_depth in [0usize, 1, 2, 5, 9, 1024] {
        for trims in [false, true] {
            let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
            let plain = capture_throwable_slots_with::<BacktraceFrame>(
                &frames,
                jit.clone(),
                max_depth,
                &mut is_a,
                &[],
            );
            let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
            let fused = capture_throwable_backtrace_fused(
                &frames,
                jit.clone(),
                max_depth,
                &mut is_a,
                &[],
            );
            assert_eq!(
                text(&fused),
                text(&plain),
                "max_depth={max_depth} trims={trims}"
            );
            for (_, at) in describe(&fused) {
                if at != 0 {
                    assert!(
                        jit.iter().any(|f| f.label.as_ptr() as usize == at
                            || f.inline_chain
                                .iter()
                                .any(|l| l.label.as_ptr() as usize == at)),
                        "a fused label that is not the walk's"
                    );
                }
            }
        }
    }
}

#[test]
fn the_fused_door_trims_the_fill_frames_and_keeps_the_rest() {
    let (frames, jit) = shape();
    let mut is_a = |_: Option<ClassId>, holder: &str| fill_holder(holder);
    let fused = text(&capture_throwable_backtrace_fused(
        &frames,
        jit,
        0,
        &mut is_a,
        &[],
    ));
    // The top is the activation that made the throwable.
    let last = fused.last().cloned().unwrap_or_default();
    assert!(last.starts_with("compiled probe/Fill.altB"), "{last}");
    assert!(last.contains("@13"), "{last}");
    // The unparsable label goes with its chain; a chain whose outermost
    // level is past the bound contributes nothing.
    assert!(fused
        .iter()
        .all(|s| !s.contains("lost") && !s.contains("deep") && !s.contains("kept")));
    // Inlined levels follow their activation, outermost first.
    let factory = fused.iter().position(|s| s.contains("factory")).unwrap();
    assert!(fused[factory - 1].starts_with("compiled probe/Fill.altA"));
    assert!(fused[factory + 1].contains("made"));
    // Interpreter frames keep their place: `main`, the depth-1 activation,
    // then `drive`.
    assert!(fused[0].contains("main"));
    assert!(fused[1].starts_with("compiled probe/Fill.down"));
    assert!(fused[2].contains("drive"));
}

#[test]
fn a_walk_the_fused_layout_does_not_cover_takes_the_plain_door() {
    // An empty walk (the bounded interpreter window) and a compiled frame
    // naming the interpreter frame it was pushed from (the dedupe).
    let (frames, _) = shape();
    let own: Arc<str> = Arc::from("probe/Fill.drive:(I)V");
    let naming = vec![compiled(2, &own, 3, vec![])];
    for jit in [Vec::new(), naming] {
        let mut is_a = |_: Option<ClassId>, _: &str| false;
        let plain = capture_throwable_slots_with::<BacktraceFrame>(
            &frames,
            jit.clone(),
            0,
            &mut is_a,
            &[],
        );
        let mut is_a = |_: Option<ClassId>, _: &str| false;
        let fused = capture_throwable_backtrace_fused(&frames, jit, 0, &mut is_a, &[]);
        assert_eq!(text(&fused), text(&plain));
    }
}

#[test]
fn the_memoised_screens_answer_the_one_pass_screens() {
    let (frames, jit) = shape();
    let mut is_a = |_: Option<ClassId>, _: &str| false;
    let trace = capture_throwable_backtrace_fused(&frames, jit, 0, &mut is_a, &[]);
    assert_eq!(capture_screens_memoised(&trace), capture_screens(&trace));
    // Every prefix and suffix too (the pair rules read each frame's caller).
    for cut in 0..trace.len() {
        assert_eq!(
            capture_screens_memoised(&trace[..cut]),
            capture_screens(&trace[..cut])
        );
        assert_eq!(
            capture_screens_memoised(&trace[cut..]),
            capture_screens(&trace[cut..])
        );
    }
    // A hidden-class label repeated, a `run()V` task repeated under one
    // label with different owners, the same text in two allocations, and
    // an inlined level with and without a class id.
    let hidden: Arc<str> = Arc::from("probe/Fill$$Lambda/0x0000000801001000.run:()V");
    let task: Arc<str> = Arc::from("probe/Task.run:()V");
    let task_copy: Arc<str> = Arc::from("probe/Task.run:()V");
    let compiled_at = |label: &Arc<str>, owner: u32| BacktraceFrame::Compiled {
        label: Arc::clone(label),
        owner_class_id: owner,
        bci: 1,
        cp_stamp: None,
    };
    let inlined_at = |label: &Arc<str>, class_id: u32| BacktraceFrame::Inlined {
        label: Arc::clone(label),
        class_id,
        bci: 1,
        cp_stamp: None,
        hidden: None,
    };
    let shapes: Vec<Vec<BacktraceFrame>> = vec![
        vec![
            compiled_at(&task, 3),
            compiled_at(&task, 3),
            compiled_at(&task, 4),
            compiled_at(&task_copy, 4),
        ],
        vec![
            compiled_at(&hidden, 3),
            compiled_at(&hidden, 3),
            inlined_at(&hidden, 0),
            inlined_at(&hidden, 3),
        ],
        vec![
            inlined_at(&task, 0),
            inlined_at(&task, 5),
            compiled_at(&task, 5),
            compiled_at(&hidden, 5),
        ],
    ];
    for trace in &shapes {
        assert_eq!(capture_screens_memoised(trace), capture_screens(trace));
    }
}
