use super::*;
use std::cell::Cell;

thread_local! {
    static CELL_A: Cell<usize> = const { Cell::new(0) };
    static CELL_B: Cell<usize> = const { Cell::new(0) };
    /// Set only on the test's own thread: steers [`a_here_b_elsewhere`].
    static IS_MEASURING_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn addr_a() -> Option<usize> {
    CELL_A.try_with(|c| c as *const Cell<usize> as usize).ok()
}

/// A cell whose offset differs between the measuring thread and every
/// other thread: what dynamic TLS looks like to a one-thread probe.
fn a_here_b_elsewhere() -> Option<usize> {
    if IS_MEASURING_THREAD.with(Cell::get) {
        CELL_A.try_with(|c| c as *const Cell<usize> as usize).ok()
    } else {
        CELL_B.try_with(|c| c as *const Cell<usize> as usize).ok()
    }
}

const SENTINEL: usize = 0x5446_5354_4C53_3230;

#[test]
fn a_static_tls_cell_is_proved_on_two_threads_and_reads_through_fs_on_a_third() {
    // The test binary links this crate statically: static TLS, so the
    // proof must succeed (a `None` here would silently disable every
    // mirror in the shipped launcher too).
    let disp = prove_fs_disp(addr_a, SENTINEL).expect("static TLS offset proved");
    assert!(disp < 0, "variant II offset is negative, got {disp}");
    assert_eq!(CELL_A.with(Cell::get), 0, "the proof restores the cell");
    // The proved offset reaches each thread's own cell.
    let seen = std::thread::spawn(move || {
        CELL_A.with(|c| c.set(0x1234_5678));
        // SAFETY: `disp` was proved thread-invariant just above.
        unsafe { read_fs_qword(disp as isize) }
    })
    .join()
    .unwrap();
    assert_eq!(seen, 0x1234_5678);
    CELL_A.with(|c| c.set(0x0BAD_F00D));
    // SAFETY: as above, on this thread.
    assert_eq!(unsafe { read_fs_qword(disp as isize) }, 0x0BAD_F00D);
    CELL_A.with(|c| c.set(0));
}

#[test]
fn an_offset_that_differs_on_another_thread_is_refused() {
    IS_MEASURING_THREAD.with(|f| f.set(true));
    // Positive control: the one-thread probe (what the mirrors used to
    // run) accepts it...
    assert!(measure_on_this_thread(&a_here_b_elsewhere, SENTINEL).is_some());
    // ...and the two-thread proof does not.
    assert_eq!(prove_fs_disp(a_here_b_elsewhere, SENTINEL), None);
    IS_MEASURING_THREAD.with(|f| f.set(false));
}

#[test]
fn a_non_tls_address_is_refused() {
    fn heap() -> Option<usize> {
        // A per-call heap cell: never at a fixed fs offset.
        Some(Box::into_raw(Box::new(Cell::new(0usize))) as usize)
    }
    assert_eq!(prove_fs_disp(heap, SENTINEL), None);
    assert_eq!(prove_fs_disp(|| None, SENTINEL), None);
    assert_eq!(prove_fs_disp(|| Some(0), SENTINEL), None);
}
