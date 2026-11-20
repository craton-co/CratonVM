use super::{
    backtrace_frame_kind, capture_throwable_backtrace_fused_with, capture_throwable_slots_with,
    fused_capture_refusal, fused_layout_and_build, trace_from_compiled_layout, BacktraceFrame,
    FusedRefusal, OsrBciOverrides,
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

fn text(trace: &[BacktraceFrame]) -> Vec<String> {
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

fn fill_holder(holder: &str) -> bool {
    holder == "java/lang/Throwable" || holder == "java/lang/RuntimeException"
}

/// `main`, `drive`, `loop` interpreted; compiled `drive` pushed from
/// `drive` (it names its caller: the dedupe's input), a compiled
/// alternation, an inlined factory, the throwing activation with its
/// inlined `<init>` chain and the compiled `fillInStackTrace` on top.
fn naming_shape() -> (Vec<Frame>, Vec<ActiveCompiledFrame>) {
    let frames = vec![
        interp("probe/Fill", "main", "([Ljava/lang/String;)V"),
        interp("probe/Fill", "drive", "(I)V"),
        interp("probe/Fill", "loop", "(I)V"),
    ];
    let drive: Arc<str> = Arc::from("probe/Fill.drive:(I)V");
    let a: Arc<str> = Arc::from("probe/Fill.altA:(I)J");
    let b: Arc<str> = Arc::from("probe/Fill.altB:(I)J");
    let fill: Arc<str> =
        Arc::from("java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;");
    let mut jit = vec![compiled(2, &drive, 4, vec![])];
    for i in 0..6 {
        jit.push(compiled(2, if i % 2 == 0 { &a } else { &b }, 7, vec![]));
    }
    jit.push(compiled(
        3,
        &a,
        9,
        vec![
            level("probe/Fill.made:(I)Ljava/lang/RuntimeException;", 3, 41),
            level("probe/Fill.factory:(I)V", 2, 41),
        ],
    ));
    jit.push(compiled(
        3,
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
    jit.push(compiled(3, &fill, 1, vec![]));
    (frames, jit)
}

/// A walk with a compiled frame naming its caller is the `Rewritten`
/// refusal; with the switch the fused door lays it out itself, and its
/// trace is the plain door's at every cap, with and without the trim.
/// Without the switch it is the plain door, as before.
#[test]
fn a_rewritten_walk_through_the_fused_door_is_the_plain_door() {
    let (frames, jit) = naming_shape();
    if !super::osr_frame_dedupe_enabled() || !super::compact_compiled_backtrace_enabled() {
        return;
    }
    assert_eq!(
        fused_capture_refusal(&frames, &jit, &[]),
        Some(FusedRefusal::Rewritten)
    );
    for max_depth in [0usize, 1, 2, 5, 9, 1024] {
        for trims in [false, true] {
            for rewritten in [false, true] {
                let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
                let plain = capture_throwable_slots_with::<BacktraceFrame>(
                    &frames,
                    jit.clone(),
                    max_depth,
                    &mut is_a,
                    &[],
                );
                let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
                let fused = capture_throwable_backtrace_fused_with(
                    &frames,
                    jit.clone(),
                    max_depth,
                    rewritten,
                    &mut is_a,
                    &[],
                );
                assert_eq!(
                    text(&fused),
                    text(&plain),
                    "max_depth={max_depth} trims={trims} rewritten={rewritten}"
                );
            }
        }
    }
}

/// The fused layout with the dedupe's bci overrides is the plain layout
/// with them: an overridden frame at the override's bci followed by the
/// override's chain (outermost first, no compile stamp, cut at the first
/// level that cannot be named), next to compiled frames with their own
/// chains, at every cap, with and without the trim.
#[test]
fn the_fused_layout_with_osr_overrides_is_the_plain_layout() {
    let (frames, jit) = naming_shape();
    if !super::compact_compiled_backtrace_enabled() {
        return;
    }
    // The deduped walk: the `drive` continuation dropped, its bci and
    // chain handed to `frames[1]`; `frames[0]` overridden with a chain
    // whose outer level cannot be named, `frames[2]` with none.
    let kept: Vec<ActiveCompiledFrame> = jit[1..].to_vec();
    let mut overrides = OsrBciOverrides::default();
    overrides.insert(
        1,
        (
            4,
            vec![
                level("probe/Fill.inner:()V", 6, 41),
                level("probe/Fill.outer:(I)V", 5, 0),
            ],
        ),
    );
    overrides.insert(
        0,
        (
            12,
            vec![
                level("probe/Fill.seen:()V", 2, 41),
                level("no-colon-here", 1, 41),
            ],
        ),
    );
    overrides.insert(2, (21, Vec::new()));
    for max_depth in [0usize, 1, 2, 4, 7, 1024] {
        for trims in [false, true] {
            let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
            let plain = trace_from_compiled_layout::<BacktraceFrame>(
                &frames,
                &kept,
                &overrides,
                max_depth,
                &mut is_a,
                &[],
            );
            let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
            let fused =
                fused_layout_and_build(&frames, kept.clone(), &overrides, max_depth, &mut is_a);
            assert_eq!(
                text(&fused),
                text(&plain),
                "max_depth={max_depth} trims={trims}"
            );
        }
    }
    // The override's chain follows its frame, outermost first; the
    // unnameable level ends `frames[0]`'s chain before `seen`.
    let mut is_a = |_: Option<ClassId>, _: &str| false;
    let fused = text(&fused_layout_and_build(
        &frames, kept, &overrides, 0, &mut is_a,
    ));
    let drive = fused
        .iter()
        .position(|s| s.contains("probe/Fill.drive"))
        .expect("the overridden drive frame");
    assert!(fused[drive].contains("@4"), "{}", fused[drive]);
    assert!(fused[drive + 1].contains("outer"), "{}", fused[drive + 1]);
    assert!(fused[drive + 2].contains("inner"), "{}", fused[drive + 2]);
    assert!(fused.iter().all(|s| !s.contains("seen")));
    assert!(
        fused[0].contains("main") && fused[0].contains("@12"),
        "{}",
        fused[0]
    );
}
