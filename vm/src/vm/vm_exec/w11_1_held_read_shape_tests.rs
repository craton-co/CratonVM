use super::held_read_shape_ok;

/// W11-1: a waiting thread's held read is answered only with a native on
/// top and, when it holds monitors, an attribution for that trace; a
/// thread that is not waiting is answered as is.
#[test]
fn w11_1_a_waiting_read_needs_its_native_and_its_attribution() {
    assert!(held_read_shape_ok(true, true, 0, false));
    assert!(held_read_shape_ok(true, true, 1, true));
    assert!(
        !held_read_shape_ok(true, false, 0, false),
        "no native on top"
    );
    assert!(
        !held_read_shape_ok(true, true, 1, false),
        "monitor, no attribution"
    );
    assert!(held_read_shape_ok(false, false, 1, false), "not waiting");
}

/// W11 residual: a waiting read answered as running reports RUNNABLE,
/// no waited-on lock and no parked sample (so no stand-in rows), and
/// keeps its trace and its locked monitors.
#[test]
fn w11_residual_a_running_phase_read_is_answered_runnable() {
    use super::{HeldThreadRead, HeldTrace, ParkedSample};
    let mut read = HeldThreadRead {
        run_state: 5,
        trace: HeldTrace {
            trace: Vec::new(),
            parked: ParkedSample {
                block: 3,
                kind: None,
            },
        },
        book: Some((None, None, Vec::new(), Vec::new(), 0, 0, 0, 0)),
        frame_monitors: None,
        stale: false,
    };
    read.answer_running();
    assert_eq!(read.run_state, 1);
    assert_eq!(read.trace.parked.block, 0);
    assert!(read.book.is_some_and(|book| book.1.is_none()));
}

/// W10-7 review (source pin): a `ThreadInfo` dump reads every target in
/// ONE pause per attempt (the pending ones together), retries only the
/// stale or unshaped ones, serves its reads only while current, and
/// `end_thread_dump` drops them.
#[test]
fn w10_7_review_a_thread_info_dump_reads_all_targets_per_pause() {
    let src =
        concat!(include_str!("gen_referent_read_keep_alive.rs"), "\n", include_str!("append_native_standin_frames.rs")).replace("\r\n", "\n");
    let begin = &src[src
        .find("    fn begin_thread_info_dump(&mut self, threads: &[ObjectRef]) {")
        .expect("the batch")..];
    let begin = &begin[..begin.find("\n    }\n").expect("its end")];
    assert_eq!(
        begin
            .matches(concat!("stw_publish_frame_traces_", "with("))
            .count(),
        1
    );
    assert!(begin.contains("for attempt in 1..=attempts {"));
    assert!(begin.contains("HeldThreadRead::read(shared, targets[i].0, targets[i].1, seq)"));
    assert!(begin.contains("if read.stale || !shaped {"));
    let take = &src[src
        .find("fn take_dump_held_read(&self, tid: ThreadId)")
        .expect("the take")..];
    assert!(take[..take.find("\n    }\n").expect("its end")].contains("dump_traces_current(self)"));
    let end = &src[src
        .find("    fn end_thread_dump(&mut self) {")
        .expect("the end")..];
    assert!(end[..end.find("\n    }\n").expect("its end")].contains("dump_held.lock().clear()"));
}

/// W11 residual (source pin): the held read's state is the block state
/// its trace was read with, and the last unshaped read without a native
/// is answered running only when retrying.
#[test]
fn w11_residual_one_block_read_and_the_running_fallback() {
    let src =
        concat!(include_str!("gen_referent_read_keep_alive.rs"), "\n", include_str!("append_native_standin_frames.rs")).replace("\r\n", "\n");
    let start = src
        .find(concat!("fn jmx_thread_", "snapshot(\n"))
        .expect("snapshot");
    let rest = &src[start..];
    let snapshot = &rest[..rest.find("\n    }\n").expect("its end")];
    assert!(snapshot.contains("if attempts > 1 && lacks_native {"));
    assert!(snapshot.contains("r.answer_running();"));
    // W10-7 review: a `ThreadInfo` dump's own read comes first.
    let taken = snapshot
        .find("held = self.take_dump_held_read(registry_tid);")
        .expect("the dump's read");
    let pause = snapshot
        .find(concat!("stw_publish_frame_traces_", "with("))
        .expect("the own pause");
    assert!(taken < pause);
    assert!(src.contains(concat!(
        "run_state: registry_run_state_for_block(registry, tid, thread_obj, ",
        "block),"
    )));
}

/// W11-1 (source pin): the snapshot retries an unshaped read as it does
/// a stale one, within the same attempts.
#[test]
fn w11_1_an_unshaped_held_read_is_taken_again() {
    let src =
        concat!(include_str!("gen_referent_read_keep_alive.rs"), "\n", include_str!("append_native_standin_frames.rs")).replace("\r\n", "\n");
    let start = src
        .find(concat!("fn jmx_thread_", "snapshot(\n"))
        .expect("snapshot");
    let rest = &src[start..];
    let snapshot = &rest[..rest.find("\n    }\n").expect("its end")];
    let shape = snapshot
        .find("self.held_read_shape(r, thread_obj)")
        .expect("the shape check");
    let retry = snapshot
        .find("if !(stale || unshaped) || attempt == attempts")
        .expect("the retry");
    assert!(shape < retry);
}
