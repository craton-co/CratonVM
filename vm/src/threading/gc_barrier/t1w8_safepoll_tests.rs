use super::*;
use cratonvm_jit::poll_page::ARMED_OFFSET;

fn leaked_cell() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

fn word_with_cell(cell: &'static AtomicUsize) -> Arc<LoopPollWord> {
    let w = LoopPollWord::with_word(0);
    *w.page_cell.lock() = cell as *const AtomicUsize as usize;
    Arc::new(w)
}

#[test]
fn the_page_cell_mirrors_the_handshake_bit_not_the_pause_count() {
    let cell = leaked_cell();
    let w = word_with_cell(cell);
    assert!(w.arm_handshake());
    assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
    assert!(w.arm_handshake(), "a second arm keeps it armed");
    assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
    assert!(w.take_handshake_arm());
    assert_eq!(cell.load(Ordering::SeqCst), 0, "the owner's take disarms");
    assert!(!w.take_handshake_arm());
    assert_eq!(cell.load(Ordering::SeqCst), 0);
    // A pause is the barrier's stop page, never the cell.
    w.note_pause_begin();
    assert_eq!(cell.load(Ordering::SeqCst), 0);
    w.note_pause_end();
}

#[test]
fn a_word_without_a_cell_arms_only_its_bit() {
    let w = Arc::new(LoopPollWord::with_word(0));
    assert!(w.arm_handshake());
    assert_eq!(w.load() & LoopPollWord::HANDSHAKE, LoopPollWord::HANDSHAKE);
    assert!(w.take_handshake_arm());
    assert_eq!(*w.page_cell.lock(), 0);
}

#[test]
fn a_dropped_handle_disarms_and_forgets_its_cell() {
    let cell = leaked_cell();
    let handle = LoopPollHandle(word_with_cell(cell));
    assert!(handle.word().arm_handshake());
    assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
    let kept = Arc::clone(handle.word());
    drop(handle);
    assert_eq!(cell.load(Ordering::SeqCst), 0, "left disarmed");
    assert_eq!(*kept.page_cell.lock(), 0, "and forgotten");
    assert!(!kept.arm_handshake());
    assert_eq!(cell.load(Ordering::SeqCst), 0);
}

/// Requesters and the owner racing: once everyone is done the cell says
/// what the bit says (the store-under-lock protocol of `sync_page_cell`).
#[test]
fn racing_arms_and_takes_leave_the_cell_agreeing_with_the_bit() {
    let cell = leaked_cell();
    let w = word_with_cell(cell);
    let mut joins = Vec::new();
    for _ in 0..3 {
        let w = Arc::clone(&w);
        joins.push(std::thread::spawn(move || {
            for _ in 0..2_000 {
                w.arm_handshake();
            }
        }));
    }
    {
        let w = Arc::clone(&w);
        joins.push(std::thread::spawn(move || {
            for _ in 0..2_000 {
                w.take_handshake_arm();
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    let up = w.load() & LoopPollWord::HANDSHAKE != 0;
    let expect = if up { ARMED_OFFSET } else { 0 };
    assert_eq!(cell.load(Ordering::SeqCst), expect);
    w.take_handshake_arm();
    assert_eq!(cell.load(Ordering::SeqCst), 0);
}

/// Off (the default), a barrier maps no pair and its pauses change nothing
/// a page-form poll could see.
#[test]
fn a_barrier_without_the_switch_has_no_poll_page() {
    if cratonvm_jit::poll_page::enabled() {
        return;
    }
    let barrier = GcBarrier::new();
    assert_eq!(barrier.page_poll_base(), 0);
    assert!(barrier.request_stw(ThreadId(1), 1));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.page_poll_base(), 0);
}
