use super::*;

/// A test runs on its own OS thread, so its poll word starts at zero. The
/// peek sees a handshake arm and a counted pause, lowers nothing, and
/// reads clear again once both are gone.
#[test]
fn t1gate_the_pending_peek_sees_an_arm_and_a_pause_and_lowers_neither() {
    let word = this_threads_loop_poll_word().expect("this thread's poll word");
    assert!(!this_threads_poll_word_pending(), "nothing armed yet");
    assert!(word.arm_handshake());
    assert!(this_threads_poll_word_pending(), "an armed handshake is pending");
    assert!(this_threads_poll_word_pending(), "the peek lowers nothing");
    assert!(take_this_threads_handshake_arm());
    assert!(!this_threads_poll_word_pending(), "lowered: clear again");
    word.note_pause_begin();
    assert!(this_threads_poll_word_pending(), "a counted pause is pending");
    word.note_pause_end();
    assert!(!this_threads_poll_word_pending());
}

/// The loop's own gate bits (the move trigger) are not "a poll is owed":
/// only the [`LoopPollWord::PAUSES`] byte is.
#[test]
fn t1gate_the_move_trigger_alone_is_not_a_pending_poll() {
    let word = this_threads_loop_poll_word().expect("this thread's poll word");
    assert!(word.kick());
    assert!(!this_threads_poll_word_pending());
}
