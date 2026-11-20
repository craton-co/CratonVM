use super::*;

/// Serialises this module's tests against each other. **Every test below
/// must hold it for its whole body.**
///
/// The table these tests drive is process-global by design — it is the
/// VM-wide registry [`active_class_ids`] exposes to the GC root walk — so
/// there is no per-test state to hand out instead. `clear()` walks
/// `STATES` and zeroes *every* thread's slots, and `active_class_ids()`
/// reads *every* thread's slots. Cargo runs a module's tests in parallel
/// threads of one process, so without this lock every assertion here races
/// its siblings, in three separate ways:
///
///   * a sibling's `clear()` wipes an activation this test is about to
///     assert on. That is the failure this lock was added for:
///     `a_peer_threads_activations_are_visible` reported "a peer thread's
///     compiled frame must root its defining loader" on roughly one run in
///     eight (2/12 on 2026-08-03), because a sibling cleared class 11
///     between the peer publishing it and the assertion reading it;
///   * a sibling's *live* activation shows up in this test's
///     `active_class_ids()`, which three tests compare by exact equality;
///   * a sibling's `enter` can take the freed slot that
///     `a_slot_is_reused_once_its_class_leaves` expects to be reused.
///
/// Only the first had been observed, but all three are reachable, and the
/// second and third would read as far more alarming failures than a flake
/// — an exact-equality mismatch on the GC's root list looks like a
/// retention bug.
///
/// This is not papering over an ordering defect in the code under test.
/// Publication is correctly ordered on both hops — `acquire_state` pushes
/// onto `STATES` with `Release` against `active_class_ids`' `Acquire`, and
/// `enter` publishes the slot's class id with `Release` against the same
/// `Acquire` — and the peer test's channel adds its own happens-before
/// edge on top. Isolated, the test does not fail.
///
/// `parking_lot::Mutex`, not `std::sync::Mutex`: it does not poison, so
/// one test's genuine failure stays one failure instead of cascading into
/// four poisoning errors that bury which assertion actually broke.
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn active_owner_counts_survive_nested_entries() {
    let _serialised = TEST_LOCK.lock();
    clear();
    let outer = enter(7).expect("a real class id is tracked");
    let inner = enter(7).expect("a real class id is tracked");
    assert_eq!(active_class_ids(), vec![7]);
    exit(inner);
    assert_eq!(
        active_class_ids(),
        vec![7],
        "the outer frame still holds it"
    );
    exit(outer);
    assert!(active_class_ids().is_empty());
    assert_eq!(enter(NO_OWNER_CLASS), None);
    clear();
}

#[test]
fn distinct_classes_get_distinct_slots() {
    let _serialised = TEST_LOCK.lock();
    clear();
    let a = enter(3).expect("tracked");
    let b = enter(4).expect("tracked");
    assert_eq!(active_class_ids(), vec![3, 4]);
    exit(a);
    assert_eq!(active_class_ids(), vec![4]);
    exit(b);
    assert!(active_class_ids().is_empty());
    clear();
}

#[test]
fn more_live_classes_than_one_chunk_holds() {
    let _serialised = TEST_LOCK.lock();
    clear();
    let n = SLOTS_PER_CHUNK + 5;
    let tokens: Vec<_> = (0..n).map(|cid| enter(cid).expect("tracked")).collect();
    let active = active_class_ids();
    assert_eq!(active.len(), n as usize, "every class stays visible");
    assert_eq!(active.first(), Some(&0));
    assert_eq!(active.last(), Some(&(n - 1)));
    for token in tokens {
        exit(token);
    }
    assert!(active_class_ids().is_empty());
    clear();
}

#[test]
fn a_slot_is_reused_once_its_class_leaves() {
    let _serialised = TEST_LOCK.lock();
    clear();
    let first = enter(21).expect("tracked");
    exit(first);
    let second = enter(22).expect("tracked");
    assert_eq!(second.slot, first.slot, "the freed slot is taken again");
    assert_eq!(active_class_ids(), vec![22]);
    exit(second);
    clear();
}

#[test]
fn a_peer_threads_activations_are_visible() {
    // Held across the peer's whole lifetime. The peer never takes this
    // lock, so holding it while the peer runs cannot deadlock.
    let _serialised = TEST_LOCK.lock();
    clear();
    let (marked_tx, marked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let token = enter(11).expect("tracked");
        marked_tx.send(()).expect("main thread is waiting");
        release_rx.recv().expect("main thread signals completion");
        exit(token);
    });
    marked_rx.recv().expect("peer marks its activation");
    assert!(
        active_class_ids().contains(&11),
        "a peer thread's compiled frame must root its defining loader"
    );
    release_tx.send(()).expect("peer is waiting");
    peer.join().expect("peer finishes");
    assert!(!active_class_ids().contains(&11));
    clear();
}

/// Every `#[test]` in this module must take [`TEST_LOCK`].
///
/// A test added without it does not fail; it makes the *other* tests fail,
/// rarely, somewhere else. That is what this module already cost once:
/// isolated, `a_peer_threads_activations_are_visible` passed 100 runs out
/// of 100, while the suite failed 3 runs out of 25 — a shape that reads as
/// "flaky test" and hides that the assertion was right all along.
///
/// Reads its own source through `file!()`, which is the compiler's answer
/// and therefore follows a rename or a move of this module to another
/// file. It fails loudly rather than vacuously when the file cannot be
/// read or when it finds no tests at all.
///
/// The scan itself is [`tests_missing_lock`], which is a pure function so
/// that [`the_serialisation_guard_catches_a_test_that_omits_the_lock`] can
/// prove it is not vacuous. It was: the first version searched a FIXED
/// twelve-line window after each `#[test]`, so a short unserialised test
/// followed by a serialised one was satisfied by *its neighbour's* lock.
/// Injecting a deliberately unlocked three-line test into this module and
/// running the guard passed, on 2026-08-04. Every test here is short, so
/// that was the normal case, not a corner.
#[test]
fn every_test_in_this_module_serialises() {
    let _serialised = TEST_LOCK.lock();

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the types crate directory has a parent")
        .join(file!());
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read this module's own source at {} ({e}) — failing \
             rather than passing without having checked anything",
            path.display()
        )
    });

    let (checked, missing) = tests_missing_lock(&src);

    assert!(
        checked >= 5,
        "found only {checked} tests in {} — the scan is broken, and it was \
         about to pass without checking anything",
        path.display()
    );
    assert!(
        missing.is_empty(),
        "test(s) in this module do not take TEST_LOCK. Every test here \
         drives ONE process-global table (see the lock's doc), so one that \
         runs unserialised does not fail itself — it makes its siblings \
         fail rarely and somewhere else. Add `let _serialised = \
         TEST_LOCK.lock();` as the first statement:\n  {}",
        missing.join("\n  ")
    );
}

/// `(tests seen, one description per test that never takes [`TEST_LOCK`])`.
///
/// Each test's window ends at that test's own closing brace — the first
/// `    }` at module-item indentation — or at the next `#[test]`,
/// whichever comes first. Never at a fixed line count. A fixed window is
/// what made the first version of this guard vacuous: it ran past the end
/// of a short test and found the lock belonging to the following one. The
/// closing-brace bound additionally stops a *helper* defined below a test
/// from vouching for it.
///
/// Erring is one-directional by construction: a window that ends too early
/// reports a lock it did not see (loud, and wrong in the safe direction),
/// never the reverse.
///
/// Pure, and takes the source as an argument, so the guard's own behaviour
/// is testable on synthetic input instead of only on the file that is
/// currently correct — a source scan that silently matches nothing passes
/// exactly as happily as one that matches everything.
fn tests_missing_lock(src: &str) -> (usize, Vec<String>) {
    // Assembled so these needles do not match the lines that define them.
    let attr = format!("#[{}]", "test");
    let takes_lock = format!("{}.lock()", "TEST_LOCK");

    let lines: Vec<&str> = src.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == attr)
        .map(|(i, _)| i)
        .collect();

    let mut missing = Vec::new();
    for (n, &i) in starts.iter().enumerate() {
        let next_attr = starts.get(n + 1).copied().unwrap_or(lines.len());
        // A fn body's own lines are indented at least 8 spaces inside
        // `mod tests`, so the first 4-space `}` is this test's closer.
        let closer = lines[i + 1..next_attr]
            .iter()
            .position(|l| *l == "    }")
            .map(|p| i + 1 + p + 1)
            .unwrap_or(next_attr);
        let end = closer.min(next_attr);
        let body = lines[i + 1..end].join("\n");
        if !body.contains(&takes_lock) {
            let name = lines[i + 1..end.min(i + 5)]
                .iter()
                .find(|l| l.contains("fn "))
                .unwrap_or(&"<unknown>")
                .trim();
            missing.push(format!("line {}: {name}", i + 1));
        }
    }
    (starts.len(), missing)
}

/// The guard has to fail on the shape that actually gets written: someone
/// adds a short test, forgets the lock, and the next test down has one.
///
/// This is the case the fixed-window version passed. Asserting it here —
/// on synthetic source, so it cannot be quietly satisfied by whatever the
/// real module happens to look like today — is the difference between a
/// tripwire and a comment claiming there is one.
#[test]
fn the_serialisation_guard_catches_a_test_that_omits_the_lock() {
    let _serialised = TEST_LOCK.lock();

    // Built rather than written literally: a `#[test]` in a string in this
    // file would be counted by the guard scanning its own source.
    let attr = format!("#[{}]", "test");
    let lock = format!("        let _s = {}.lock();", "TEST_LOCK");
    let synthetic = format!(
        "{attr}\n    fn forgot_the_lock() {{\n        clear();\n    }}\n\n\
         {attr}\n    fn took_the_lock() {{\n{lock}\n        clear();\n    }}\n"
    );

    let (checked, missing) = tests_missing_lock(&synthetic);
    assert_eq!(checked, 2, "both tests must be seen");
    assert_eq!(
        missing.len(),
        1,
        "exactly the unserialised test must be reported, not its neighbour \
         — got {missing:?}"
    );
    assert!(
        missing[0].contains("forgot_the_lock"),
        "the report must name the offender: {missing:?}"
    );

    // A helper defined BELOW a test must not vouch for it either — that is
    // the same borrowed-lock mistake with a non-test neighbour.
    let with_helper = format!(
        "{attr}\n    fn forgot_the_lock() {{\n        clear();\n    }}\n\n\
             fn helper() {{\n{lock}\n    }}\n"
    );
    let (checked, missing) = tests_missing_lock(&with_helper);
    assert_eq!(checked, 1);
    assert_eq!(
        missing.len(),
        1,
        "a helper's lock, below the test, must not satisfy it: {missing:?}"
    );

    // ...and it must not cry wolf on a module that is correct.
    let (checked, missing) = tests_missing_lock(&format!(
        "{attr}\n    fn a() {{\n{lock}\n    }}\n\n{attr}\n    fn b() {{\n{lock}\n    }}\n"
    ));
    assert_eq!(checked, 2);
    assert!(missing.is_empty(), "no false positives: {missing:?}");
}
