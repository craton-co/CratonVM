use super::*;
use std::hint::black_box;
use std::time::Instant;

/// The pre-fix `with_cell`, kept verbatim so the A/B has a real control.
fn old_shape<R>(seed: ThreadExecState, f: impl FnOnce(&ThreadStateCell) -> R) -> R {
    let cell = SELF_CELL.with(|c| {
        if let Some(existing) = c.borrow().as_ref() {
            return Arc::clone(&existing.0);
        }
        let fresh = Arc::new(ThreadStateCell {
            thread_id: AtomicU64::new(PENDING_THREAD_ID.with(|p| p.get())),
            state: AtomicU8::new(seed.as_u8()),
        });
        *c.borrow_mut() = Some(CellHandle(Arc::clone(&fresh)));
        cells().write().push(Arc::clone(&fresh));
        fresh
    });
    f(&cell)
}

#[test]
#[ignore = "measurement, not an assertion — see the module doc"]
fn borrow_versus_arc_clone() {
    const ROUNDS: u32 = 2_000_000;
    // Seed the cell so neither arm ever takes the cold branch.
    record_transition(ThreadExecState::JavaRunning, "with_cell-ab:seed");

    let read_state = |cell: &ThreadStateCell| cell.state.load(Ordering::Relaxed);

    let mut new_ns = Vec::new();
    let mut old_ns = Vec::new();
    // A-B-A-B, six passes each. Alternating on every pass (rather than
    // running one arm's passes and then the other's) is the point: a
    // drift that clusters into one arm is what a block layout cannot
    // separate from the effect.
    for _ in 0..6 {
        let t0 = Instant::now();
        for _ in 0..ROUNDS {
            black_box(with_cell(ThreadExecState::JavaRunning, read_state));
        }
        new_ns.push(t0.elapsed().as_nanos() as f64 / f64::from(ROUNDS));

        let t0 = Instant::now();
        for _ in 0..ROUNDS {
            black_box(old_shape(ThreadExecState::JavaRunning, read_state));
        }
        old_ns.push(t0.elapsed().as_nanos() as f64 / f64::from(ROUNDS));
    }

    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    println!(
        "with_cell   borrow (new): {new_ns:?}  min={:.2} ns",
        min(&new_ns)
    );
    println!(
        "with_cell Arc::clone (old): {old_ns:?}  min={:.2} ns",
        min(&old_ns)
    );
    println!(
        "minima separate by {:.2} ns/call; the native funnel performs TWO \
         transitions per call",
        min(&old_ns) - min(&new_ns)
    );
}
