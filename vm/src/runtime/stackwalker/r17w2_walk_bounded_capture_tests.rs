use super::{
    backtrace_frame_kind, bounded_capture_verified, capture_throwable_slots_with,
    BacktraceFrame,
};
use crate::classloading::ClassId;
use crate::jit::conservative_roots::{ActiveCompiledFrame, CompiledWalkCut};
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

fn compiled(depth: u32, chain_index: u32, label: &Arc<str>, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: depth,
        chain_index,
        label: Arc::clone(label),
        owner_class_id: 40,
        cm_ptr: 0,
        bci,
        inline_chain: Vec::new(),
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: Some(3),
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

fn text(trace: &[BacktraceFrame]) -> Vec<String> {
    trace
        .iter()
        .map(|f| {
            let e = f.to_entry();
            format!(
                "{} {}.{} @{}",
                backtrace_frame_kind(f),
                e.class_name,
                e.method_name,
                e.byte_code_index
            )
        })
        .collect()
}

/// `main` and `drive` interpreted; one compiled `down` at depth 1; then
/// the deep entry at depth 2: a boundary `outer`, `deep` activations of
/// `down` (bci 100 + position, outermost first, so every frame is
/// distinguishable), and the compiled `fillInStackTrace` on top.
fn whole(deep: usize) -> (Vec<Frame>, Vec<ActiveCompiledFrame>, usize) {
    let frames = vec![
        interp("probe/Walk", "main", "([Ljava/lang/String;)V"),
        interp("probe/Walk", "drive", "(I)V"),
    ];
    let down: Arc<str> = Arc::from("probe/Walk.down:(I)J");
    let outer: Arc<str> = Arc::from("probe/Walk.outer:(I)J");
    let fill: Arc<str> =
        Arc::from("java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;");
    let mut jit = vec![compiled(1, 0, &down, 5), compiled(2, 1, &outer, 9)];
    let entry_start = 1;
    for i in 0..deep {
        jit.push(compiled(2, 1, &down, 100 + i as i32));
    }
    jit.push(compiled(2, 1, &fill, 1));
    (frames, jit, entry_start)
}

/// The deep entry cut to its innermost `budget` activations, as
/// `active_compiled_frames_cut` reports it.
fn cut_to(
    jit: &[ActiveCompiledFrame],
    entry_start: usize,
    budget: usize,
) -> (Vec<ActiveCompiledFrame>, CompiledWalkCut) {
    let entry_len = jit.len() - entry_start;
    let dropped = entry_len.saturating_sub(budget);
    let mut bounded: Vec<ActiveCompiledFrame> = jit[..entry_start].to_vec();
    bounded.extend(jit[entry_start + dropped..].iter().cloned());
    let cut = CompiledWalkCut {
        interp_depth: 2,
        chain_index: 1,
        first_reported: entry_start,
    };
    (bounded, cut)
}

fn trims(_: Option<ClassId>, holder: &str) -> bool {
    holder == "java/lang/Throwable"
}

/// Whenever the verified door answers, it answers the plain door's capped
/// trace of the WHOLE walk; with the production budget (cap plus slack)
/// it always answers; and a budget below the cap is refused rather than
/// answered short.
#[test]
fn a_verified_bounded_capture_is_the_whole_walks_capped_trace() {
    let (frames, jit, entry_start) = whole(300);
    for max_depth in [1usize, 2, 5, 37, 120] {
        let mut is_a = trims;
        let plain = capture_throwable_slots_with::<BacktraceFrame>(
            &frames,
            jit.clone(),
            max_depth,
            &mut is_a,
            &[],
        );
        assert_eq!(plain.len(), max_depth);
        for budget in [
            1usize,
            max_depth,
            max_depth + 1,
            max_depth + 2,
            max_depth + 64,
        ] {
            let (bounded, cut) = cut_to(&jit, entry_start, budget);
            let mut is_a = trims;
            let got =
                bounded_capture_verified(&frames, bounded, cut, max_depth, &mut is_a, &[]);
            if let Some(got) = &got {
                assert_eq!(
                    text(got),
                    text(&plain),
                    "max_depth={max_depth} budget={budget}"
                );
            }
            if budget >= max_depth + 64 {
                assert!(got.is_some(), "max_depth={max_depth} budget={budget}");
            }
            if budget < max_depth {
                assert!(got.is_none(), "max_depth={max_depth} budget={budget}");
            }
        }
    }
}

/// A cap the trimmed bounded layout cannot fill from inside the cut, an
/// uncapped capture, and a walk with a compiled frame naming the
/// interpreter frame it was pushed from are all refused (redone whole).
#[test]
fn the_shapes_the_proof_does_not_cover_are_refused() {
    let (frames, jit, entry_start) = whole(50);
    let (bounded, cut) = cut_to(&jit, entry_start, 10);
    let mut is_a = trims;
    let short = bounded_capture_verified(&frames, bounded.clone(), cut, 40, &mut is_a, &[]);
    assert!(short.is_none());
    let mut is_a = trims;
    let uncapped = bounded_capture_verified(&frames, bounded.clone(), cut, 0, &mut is_a, &[]);
    assert!(uncapped.is_none());
    // `drive` compiled at depth 2 names `frames[1]`.
    let drive: Arc<str> = Arc::from("probe/Walk.drive:(I)V");
    let mut naming = bounded;
    naming.insert(entry_start, compiled(2, 1, &drive, 3));
    let mut is_a = trims;
    if super::osr_frame_dedupe_enabled() {
        assert!(bounded_capture_verified(&frames, naming, cut, 5, &mut is_a, &[]).is_none());
    }
}
