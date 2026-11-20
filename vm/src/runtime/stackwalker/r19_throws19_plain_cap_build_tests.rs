//! JIT round 19 proposal wave (lane throws19, TF17-6;
//! `CRATONVM_THROWABLE_PLAIN_CAP_BUILD=1`): the plain door's capped build
//! equals building every survivor and draining the excess, frame for
//! frame, at every cap, for both retained forms.
use super::{
    backtrace_frame_kind, trace_from_compiled_layout_with, BacktraceFrame, OsrBciOverrides,
    StackTraceEntry,
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

/// A deep compiled recursion over three interpreted frames, an alternating
/// pair, an unparsable label (no slot), an inlined factory with an
/// over-bound level, and the throwing activation with its inlined
/// `<init>` chain and the compiled `fillInStackTrace` on top: about 60
/// slots, so caps below and above the survivor count both occur.
fn shape() -> (Vec<Frame>, Vec<ActiveCompiledFrame>) {
    let frames = vec![
        interp("probe/Cap", "main", "([Ljava/lang/String;)V"),
        interp("probe/Cap", "drive", "(I)V"),
        interp("probe/Cap", "loop", "(I)V"),
    ];
    let rec: Arc<str> = Arc::from("probe/Cap.rec:(I)V");
    let a: Arc<str> = Arc::from("probe/Cap.altA:(I)J");
    let b: Arc<str> = Arc::from("probe/Cap.altB:(I)J");
    let bad: Arc<str> = Arc::from("no-colon-here");
    let fill: Arc<str> =
        Arc::from("java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;");
    let mut jit = Vec::new();
    for i in 0..40 {
        jit.push(compiled(1, &rec, 5 + (i % 3), vec![]));
    }
    jit.push(compiled(2, &bad, 1, vec![]));
    for i in 0..10 {
        jit.push(compiled(2, if i % 2 == 0 { &a } else { &b }, 7, vec![]));
    }
    jit.push(compiled(
        3,
        &a,
        9,
        vec![
            level("probe/Cap.made:(I)Ljava/lang/RuntimeException;", 3, 41),
            level("probe/Cap.factory:(I)V", 2, 41),
            level("probe/Cap.overBound:()V", u32::MAX, 41),
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

#[test]
fn the_capped_build_equals_the_build_then_drain() {
    let (frames, jit) = shape();
    let no_osr = OsrBciOverrides::default();
    let mut whole_len = 0usize;
    for max_depth in [0usize, 1, 2, 3, 5, 9, 17, 40, 55, 60, 61, 64, 1024] {
        for trims in [false, true] {
            let run = |cap_build: bool| {
                let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
                let backtrace = trace_from_compiled_layout_with::<BacktraceFrame>(
                    &frames,
                    &jit,
                    &no_osr,
                    max_depth,
                    &mut is_a,
                    &[],
                    cap_build,
                );
                let mut is_a = |_: Option<ClassId>, holder: &str| trims && fill_holder(holder);
                let entries = trace_from_compiled_layout_with::<StackTraceEntry>(
                    &frames,
                    &jit,
                    &no_osr,
                    max_depth,
                    &mut is_a,
                    &[],
                    cap_build,
                );
                (backtrace_text(&backtrace), entry_text(&entries))
            };
            let (plain_bt, plain_e) = run(false);
            let (capped_bt, capped_e) = run(true);
            assert_eq!(
                capped_bt, plain_bt,
                "backtrace max_depth={max_depth} trims={trims}"
            );
            assert_eq!(
                capped_e, plain_e,
                "entries max_depth={max_depth} trims={trims}"
            );
            if max_depth != 0 {
                assert!(plain_bt.len() <= max_depth);
            }
            if max_depth == 0 && !trims {
                whole_len = plain_bt.len();
            }
        }
    }
    // The fixture is deep enough that small caps cut and large ones do not.
    assert!(whole_len > 17 && whole_len < 1024, "{whole_len}");
}
