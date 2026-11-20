use super::{lock_jit_threads, lock_retire_queue};

/// The inversion fires. Taking the retirement queue while holding
/// `jit_threads()` is the order `drain_deferred_jit_owners` exists to
/// forbid, and it must now be reported rather than merely argued against.
///
/// `force_enable_for_testing` makes this run under `cargo test --release`
/// too, where enforcement would otherwise be off and the test would fail
/// for a reason unrelated to the rule -- the trap the framework's own doc
/// records costing 18 false regressions per release run.
#[test]
#[should_panic(expected = "lock order violation")]
fn taking_the_queue_under_jit_threads_is_reported() {
    cratonvm_types::lock_order::tracking::force_enable_for_testing();
    let _threads = lock_jit_threads();
    let _queue = lock_retire_queue();
}

/// The order `drain_deferred_jit_owners` actually uses is accepted, and so
/// is taking each alone. Without this the test above would pass just as
/// well against a check that refused every acquisition.
#[test]
fn the_queue_first_order_and_either_lock_alone_are_accepted() {
    cratonvm_types::lock_order::tracking::force_enable_for_testing();
    {
        let _queue = lock_retire_queue();
        let _threads = lock_jit_threads();
    }
    {
        let _threads = lock_jit_threads();
    }
    {
        let _queue = lock_retire_queue();
    }
}

/// A released `jit_threads()` guard leaves nothing behind: the queue may be
/// taken after it. The count is decremented on drop, and a guard that
/// leaked its count would turn every later drain on this thread into a
/// spurious violation.
#[test]
fn a_dropped_jit_threads_guard_does_not_poison_the_thread() {
    cratonvm_types::lock_order::tracking::force_enable_for_testing();
    drop(lock_jit_threads());
    drop(lock_jit_threads());
    let _queue = lock_retire_queue();
}

/// Every acquisition goes through the two helpers, or the check sees only
/// some of them.
///
/// The check lives in `lock_retire_queue` and depends on
/// `lock_jit_threads` maintaining the count, so a new site written as
/// `jit_threads().lock()` or `deferred_jit_owners().lock()` would compile,
/// work, and silently fall outside the rule. This counts raw acquisitions
/// in production code and expects exactly the two inside the helpers.
///
///
/// Counted over CODE lines only -- every line whose first non-space
/// characters are `//` is skipped. That matters twice: this very doc
/// comment names the raw spelling it forbids, and so does the explanation
/// above the helpers, and counting prose would make the expected number a
/// function of how the rule is documented. And it scans the WHOLE file
/// rather than "everything before the first `#[cfg(test)] mod`", because
/// `lib.rs` interleaves test modules with production code and that prefix
/// ends thousands of lines before these helpers -- the first version of
/// this test did exactly that and counted zero of each.
///
/// The needles are assembled at runtime so the test body is not counted.
#[test]
fn no_raw_acquisition_bypasses_the_ordering_check() {
    let threads = ["jit_threads()", ".lock()"].concat();
    let queue = ["deferred_jit_owners()", ".lock()"].concat();
    let mut all_src = include_str!("lib.rs").to_string();
    let lib_parts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib_parts");
    for entry in std::fs::read_dir(&lib_parts).expect("lib_parts").flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "rs")
            && !p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_tests.rs"))
        {
            all_src.push('\n');
            all_src.push_str(&std::fs::read_to_string(&p).expect("read lib_part"));
        }
    }
    let code_lines = all_src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"));
    let (mut n_threads, mut n_queue) = (0usize, 0usize);
    for line in code_lines {
        n_threads += line.matches(threads.as_str()).count();
        n_queue += line.matches(queue.as_str()).count();
    }
    assert_eq!(
        (n_threads, n_queue),
        (1, 1),
        "expected exactly one raw `jit_threads` and one raw `deferred_jit_owners` \
         acquisition -- the ones inside `lock_jit_threads` and `lock_retire_queue`. \
         Route any new site through those helpers, or the retirement queue's ordering \
         check does not see it.",
    );
}
