/// gcj j1/cm (P4/ph): the dispatch loop's slow path polls the handshake
/// queue after its one `Acquire` read of the poll word (which a kick
/// synchronizes with) and before the pause branch.
#[test]
fn gcj1cm_the_loop_top_polls_handshakes_after_the_acquire_and_before_the_pause() {
    let src = concat!(
        include_str!("../interpreter/mod.rs"),
        "\n",
        include_str!("../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../interpreter/execute.rs"),
        "\n",
        include_str!("../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::runtime::interpreter) fn loop_top_poll(")
        .expect("loop_top_poll");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let acquire = body
        .find("let word = poll.load_acquire();")
        .expect("the acquire read");
    let poll = body
        .find("thread.run_pending_handshakes(shared);")
        .expect("the handshake poll");
    let pause = body
        .find("safepoint_check(shared, thread);")
        .expect("the pause branch");
    assert!(acquire < poll && poll < pause);
}

/// gcj j1/cm (P4/ph): a kick (`note_code_moved_on_every_loop`) changes a
/// registered loop word outside its pause byte, so the loop's fast-path
/// compare (`loop_poll.load() != poll_gate`) sends it to `loop_top_poll`.
#[test]
fn gcj1cm_a_kick_moves_a_registered_loop_word_off_its_gate() {
    use crate::threading::gc_barrier::{GcBarrier, LoopPollWord};
    let barrier = GcBarrier::new();
    // SAFETY: this test thread's own word (or the barrier's), alive for
    // the test.
    let word = unsafe { &*barrier.loop_poll_word_for_this_thread() };
    let gate = word.load() & !LoopPollWord::PAUSES;
    barrier.note_code_moved_on_every_loop();
    let after = word.load();
    assert_ne!(after, gate);
    assert_eq!(after & LoopPollWord::PAUSES, 0, "no pause counted");
}

/// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): the loop top lowers the
/// handshake arm BEFORE it drains the queue (no lost wakeup), and the
/// word is published under either switch.
#[test]
fn t1e_the_loop_top_lowers_the_handshake_arm_before_the_drain() {
    let src = concat!(
        include_str!("../interpreter/mod.rs"),
        "\n",
        include_str!("../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../interpreter/execute.rs"),
        "\n",
        include_str!("../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::runtime::interpreter) fn loop_top_poll(")
        .expect("loop_top_poll");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let acquire = body
        .find("let word = poll.load_acquire();")
        .expect("the acquire read");
    let clear = body
        .find("poll.take_handshake_arm();")
        .expect("the clear");
    let drain = body
        .find("thread.run_pending_handshakes(shared);")
        .expect("the drain");
    assert!(acquire < clear && clear < drain);
    assert!(src.contains(
        "|| shared.threads.thread_registry.handshake_poll_bit()\n    {\n        thread\n            .gc_block_state\n            .publish_loop_poll_word("
    ));
}
