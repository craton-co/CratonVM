use super::*;

fn drain(fifo: &mut MarkPrefetchFifo, worklist: &mut Vec<*mut u8>) -> Vec<usize> {
    let mut out = Vec::new();
    while let Some(p) = fifo.pop(worklist) {
        out.push(p as usize);
    }
    out
}

/// Off, the ring is `Vec::pop`: the same LIFO order, nothing prefetched.
#[test]
fn gcj_j2og2_an_off_ring_pops_exactly_as_the_worklist() {
    let mut fifo = MarkPrefetchFifo::new(false);
    let mut w: Vec<*mut u8> = (1..=20usize).map(|i| (i * 16) as *mut u8).collect();
    let got = drain(&mut fifo, &mut w);
    let want: Vec<usize> = (1..=20usize).rev().map(|i| i * 16).collect();
    assert_eq!(got, want);
    assert_eq!(fifo.prefetched(), 0);
}

/// On, every entry is popped exactly once, including entries pushed while
/// the ring holds others, and `None` comes only when both are empty.
#[test]
fn gcj_j2og2_an_armed_ring_pops_every_entry_once() {
    let mut fifo = MarkPrefetchFifo::new(true);
    let mut w: Vec<*mut u8> = (1..=5usize).map(|i| (i * 16) as *mut u8).collect();
    let mut seen = Vec::new();
    let mut next = 6usize;
    while let Some(p) = fifo.pop(&mut w) {
        seen.push(p as usize);
        // Each of the first 30 scans pushes one child, as a mark would.
        if next <= 35 {
            w.push((next * 16) as *mut u8);
            next += 1;
        }
    }
    assert!(w.is_empty());
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    let want: Vec<usize> = (1..=35usize).map(|i| i * 16).collect();
    assert_eq!(sorted, want, "every entry exactly once");
    assert_eq!(fifo.prefetched(), 35);
    // The ring is FIFO over what it took from the LIFO top: the first
    // pops are the top `DEPTH` entries, oldest-taken first.
    let mut fifo = MarkPrefetchFifo::new(true);
    let mut w: Vec<*mut u8> = (1..=20usize).map(|i| (i * 16) as *mut u8).collect();
    assert_eq!(fifo.pop(&mut w), Some((20 * 16) as *mut u8));
    assert_eq!(w.len(), 20 - MARK_PREFETCH_DEPTH);
    assert_eq!(fifo.pop(&mut w), Some((19 * 16) as *mut u8));
}
