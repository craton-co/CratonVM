// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Active compiled-frame ownership used to keep defining loaders alive.
//!
//! A compiled frame carries no receiver oop for a static method, so nothing
//! else on the stack roots its defining loader. [`enter`] marks a class active
//! for as long as one of its compiled frames is running and [`exit`] clears the
//! mark; [`active_class_ids`] is what the GC root walk reads to turn those
//! marks into loader roots.
//!
//! # Why this is not one map under one lock
//!
//! `enter`/`exit` run on **every** compiled-frame activation — twice per
//! interpreter/JIT boundary crossing, from every mutator at once. The original
//! implementation was a single process-wide `Mutex<Registry>` holding two
//! `FxHashMap`s (`entry_ptr -> class_id` and `class_id -> count`), and once the
//! `class_manager` read lock came off the JIT dispatch path it became the
//! dominant serialization point in the VM
//! (`fixed-suite-bugs/tomcat/23-charsetcache-pathological-slowdown.md`).
//!
//! Both halves are gone rather than merely made cheaper:
//!
//! * the `entry_ptr -> class_id` map is unnecessary — the artifact whose frame
//!   is being entered is already in hand, so its declaring class travels on
//!   [`crate::ClassId`]-shaped `CompiledMethod::owner_class_id` instead. That
//!   is also strictly more correct: a class id read off a live artifact cannot
//!   be a stale entry for a retired one.
//! * the `class_id -> count` map is now per thread. `enter` and `exit` only
//!   read and write the calling thread's own cache lines;
//!   [`active_class_ids`] walks every thread's table from the outside, which is
//!   sound because each slot's published class id is an `AtomicU32`.
//!
//! Set `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX=1` to restore the old global-lock
//! counting, so one binary can be A/B'd against itself.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

/// `CompiledMethod::owner_class_id` when an artifact was never published under
/// a declaring class (test fixtures, un-published probe bodies). Such an
/// activation is not tracked — exactly as an unregistered entry was not before.
pub const NO_OWNER_CLASS: u32 = u32::MAX;

/// Slots per chunk. A thread's *simultaneously* active distinct classes is
/// bounded by its compiled-frame nesting depth, which is small; one chunk
/// covers essentially every real stack and further chunks are appended on
/// demand so the bound is never a correctness cliff.
const SLOTS_PER_CHUNK: u32 = 16;

/// One (class id, nesting count) pair owned by exactly one thread.
struct Slot {
    /// The class this slot currently marks active, or [`NO_OWNER_CLASS`].
    ///
    /// Written **only** by the owning thread, read by [`active_class_ids`]
    /// from whichever thread is collecting roots. `Release`/`Acquire` is
    /// enough in both directions: the publishing store happens strictly before
    /// the compiled body can run, and the clearing store strictly after it has
    /// returned, so a racing reader can only ever be *conservative* — it may
    /// still see a class whose last frame just returned, which over-retains a
    /// loader for one collection and is always safe.
    class_id: AtomicU32,
    /// Nesting count for `class_id`. Owner-thread only: plain loads and
    /// stores, never a read-modify-write, and never read cross-thread. It is
    /// an atomic purely so `Slot` is `Sync`.
    count: AtomicU32,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            class_id: AtomicU32::new(NO_OWNER_CLASS),
            count: AtomicU32::new(0),
        }
    }
}

struct Chunk {
    slots: [Slot; SLOTS_PER_CHUNK as usize],
    /// Appended by the owning thread only; walked by readers.
    next: AtomicPtr<Chunk>,
}

impl Chunk {
    const fn new() -> Self {
        Self {
            slots: [const { Slot::empty() }; SLOTS_PER_CHUNK as usize],
            next: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

/// One thread's activation table. Leaked for the process lifetime and handed
/// to a later thread when its owner exits, so a program that creates many
/// short-lived threads does not grow this list without bound.
struct ThreadState {
    head: Chunk,
    /// `true` while a live thread owns this table. A table is released (and so
    /// becomes reusable) only when every one of its slots is empty.
    owned: AtomicBool,
    next: AtomicPtr<ThreadState>,
}

static STATES: AtomicPtr<ThreadState> = AtomicPtr::new(std::ptr::null_mut());

/// Counts for activations that cannot reach thread-local storage — today only
/// a thread whose TLS has already been destroyed while compiled code is still
/// unwinding — plus every activation under the global-mutex A/B mode. Keeps
/// the pre-existing behaviour for that path rather than dropping the retention.
fn fallback() -> &'static Mutex<FxHashMap<u32, usize>> {
    static FALLBACK: OnceLock<Mutex<FxHashMap<u32, usize>>> = OnceLock::new();
    FALLBACK.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// A/B opt-out: restore the pre-2026-07-31 single global `Mutex`, so the same
/// binary can be measured with and without this change.
fn global_mutex_mode() -> bool {
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        crate::flags::runtime_var_os("CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX").is_some()
    })
}

/// Handle to this thread's table. The `Drop` impl is what returns the table to
/// the free list, so it lives directly in the thread-local rather than as a
/// separate guard that would have to touch a second thread-local from a
/// destructor.
struct StateHandle(Cell<*const ThreadState>);

impl Drop for StateHandle {
    fn drop(&mut self) {
        let ptr = self.0.get();
        if ptr.is_null() {
            return;
        }
        // SAFETY: `ThreadState`s are leaked for the process lifetime.
        let state = unsafe { &*ptr };
        // Only recycle a table whose slots are all clear. A thread torn down
        // with a compiled frame still counted keeps its table (and its marks)
        // forever, which over-retains rather than under-retains.
        if state_is_empty(state) {
            state.owned.store(false, Ordering::Release);
        }
        self.0.set(std::ptr::null());
    }
}

thread_local! {
    static MY_STATE: StateHandle = const { StateHandle(Cell::new(std::ptr::null())) };
}

fn state_is_empty(state: &ThreadState) -> bool {
    for_each_chunk(state, |chunk| {
        chunk
            .slots
            .iter()
            .all(|s| s.class_id.load(Ordering::Acquire) == NO_OWNER_CLASS)
    })
}

/// Run `f` over every chunk of `state`, stopping (and returning `false`) as
/// soon as `f` returns `false`.
fn for_each_chunk(state: &ThreadState, mut f: impl FnMut(&Chunk) -> bool) -> bool {
    let mut chunk: &Chunk = &state.head;
    loop {
        if !f(chunk) {
            return false;
        }
        let next = chunk.next.load(Ordering::Acquire);
        if next.is_null() {
            return true;
        }
        // SAFETY: chunks are leaked for the process lifetime once linked.
        chunk = unsafe { &*next };
    }
}

/// Claim a table for this thread: reuse one whose owner has exited, else leak
/// a fresh one and push it on the global list.
fn acquire_state() -> &'static ThreadState {
    let mut cursor = STATES.load(Ordering::Acquire);
    while !cursor.is_null() {
        // SAFETY: leaked for the process lifetime.
        let state = unsafe { &*cursor };
        if !state.owned.load(Ordering::Relaxed)
            && state
                .owned
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return state;
        }
        cursor = state.next.load(Ordering::Acquire);
    }

    let fresh = Box::into_raw(Box::new(ThreadState {
        head: Chunk::new(),
        owned: AtomicBool::new(true),
        next: AtomicPtr::new(std::ptr::null_mut()),
    }));
    // SAFETY: just allocated, and leaked from here on.
    let fresh_ref: &'static ThreadState = unsafe { &*fresh };
    loop {
        let head = STATES.load(Ordering::Acquire);
        fresh_ref.next.store(head, Ordering::Release);
        if STATES
            .compare_exchange(head, fresh, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return fresh_ref;
        }
    }
}

/// This thread's table, or `None` when thread-local storage is already gone.
fn my_state() -> Option<&'static ThreadState> {
    MY_STATE
        .try_with(|handle| {
            let ptr = handle.0.get();
            if !ptr.is_null() {
                // SAFETY: leaked for the process lifetime.
                return unsafe { &*ptr };
            }
            let state = acquire_state();
            handle.0.set(state as *const ThreadState);
            state
        })
        .ok()
}

/// Where an activation was recorded, so [`exit`] does not have to search for
/// it. Returned by [`enter`] and stored in the caller's RAII guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    class_id: u32,
    /// Slot index across this thread's chunks, or [`Self::FALLBACK_SLOT`].
    slot: u32,
}

impl Activation {
    const FALLBACK_SLOT: u32 = u32::MAX;

    /// The class whose defining loader this activation keeps alive.
    pub fn class_id(self) -> u32 {
        self.class_id
    }
}

fn enter_fallback(class_id: u32) -> Option<Activation> {
    *fallback().lock().entry(class_id).or_insert(0) += 1;
    Some(Activation {
        class_id,
        slot: Activation::FALLBACK_SLOT,
    })
}

fn slot_at(state: &'static ThreadState, index: u32) -> Option<&'static Slot> {
    let mut chunk: &'static Chunk = &state.head;
    let mut remaining = index;
    loop {
        if remaining < SLOTS_PER_CHUNK {
            return Some(&chunk.slots[remaining as usize]);
        }
        remaining -= SLOTS_PER_CHUNK;
        let next = chunk.next.load(Ordering::Acquire);
        if next.is_null() {
            return None;
        }
        // SAFETY: leaked for the process lifetime.
        chunk = unsafe { &*next };
    }
}

/// Mark one compiled activation of `class_id` active.
///
/// Returns the token [`exit`] must be given, or `None` when the artifact has
/// no declaring class ([`NO_OWNER_CLASS`]) and so nothing to retain.
pub fn enter(class_id: u32) -> Option<Activation> {
    if class_id == NO_OWNER_CLASS {
        return None;
    }
    if global_mutex_mode() {
        return enter_fallback(class_id);
    }
    let Some(state) = my_state() else {
        return enter_fallback(class_id);
    };

    // One pass: take an existing slot for this class if there is one, else the
    // first free slot seen along the way.
    let mut index = 0u32;
    let mut free: Option<(u32, &Slot)> = None;
    let mut chunk: &'static Chunk = &state.head;
    loop {
        for slot in &chunk.slots {
            let held = slot.class_id.load(Ordering::Relaxed);
            if held == class_id {
                slot.count
                    .store(slot.count.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                return Some(Activation {
                    class_id,
                    slot: index,
                });
            }
            if held == NO_OWNER_CLASS && free.is_none() {
                free = Some((index, slot));
            }
            index += 1;
        }
        let next = chunk.next.load(Ordering::Acquire);
        if next.is_null() {
            break;
        }
        // SAFETY: leaked for the process lifetime.
        chunk = unsafe { &*next };
    }

    if let Some((free_index, slot)) = free {
        slot.count.store(1, Ordering::Relaxed);
        // Publish last: a reader must never see a class id whose count has not
        // been set. `Release` pairs with `active_class_ids`' `Acquire`.
        slot.class_id.store(class_id, Ordering::Release);
        return Some(Activation {
            class_id,
            slot: free_index,
        });
    }

    // Every slot is taken — append a chunk. `chunk` is this thread's last one
    // and only this thread appends, so a plain store cannot lose a link.
    let fresh = Box::into_raw(Box::new(Chunk::new()));
    // SAFETY: just allocated, leaked from here on.
    let fresh_ref = unsafe { &*fresh };
    fresh_ref.slots[0].count.store(1, Ordering::Relaxed);
    fresh_ref.slots[0]
        .class_id
        .store(class_id, Ordering::Release);
    chunk.next.store(fresh, Ordering::Release);
    Some(Activation {
        class_id,
        slot: index,
    })
}

/// Drop one activation previously returned by [`enter`].
pub fn exit(activation: Activation) {
    if activation.slot == Activation::FALLBACK_SLOT {
        let mut map = fallback().lock();
        if let Some(count) = map.get_mut(&activation.class_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&activation.class_id);
            }
        }
        return;
    }
    let Some(state) = my_state() else {
        return;
    };
    let Some(slot) = slot_at(state, activation.slot) else {
        return;
    };
    // A slot is only ever reassigned by its owning thread while its count is
    // zero, so a token this thread still holds always names its own class.
    if slot.class_id.load(Ordering::Relaxed) != activation.class_id {
        return;
    }
    let count = slot.count.load(Ordering::Relaxed);
    if count > 1 {
        slot.count.store(count - 1, Ordering::Relaxed);
    } else {
        slot.count.store(0, Ordering::Relaxed);
        slot.class_id.store(NO_OWNER_CLASS, Ordering::Release);
    }
}

/// Every class with at least one compiled frame running anywhere.
///
/// Read by the GC root walk. Duplicates are removed so the caller pushes one
/// loader root per class, matching the map-keyed behaviour this replaced.
pub fn active_class_ids() -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut cursor = STATES.load(Ordering::Acquire);
    while !cursor.is_null() {
        // SAFETY: leaked for the process lifetime.
        let state = unsafe { &*cursor };
        for_each_chunk(state, |chunk| {
            for slot in &chunk.slots {
                let held = slot.class_id.load(Ordering::Acquire);
                if held != NO_OWNER_CLASS {
                    out.push(held);
                }
            }
            true
        });
        cursor = state.next.load(Ordering::Acquire);
    }
    out.extend(fallback().lock().keys().copied());
    out.sort_unstable();
    out.dedup();
    out
}

/// Drop every activation record. Used when the VM tears its class state down.
pub fn clear() {
    let mut cursor = STATES.load(Ordering::Acquire);
    while !cursor.is_null() {
        // SAFETY: leaked for the process lifetime.
        let state = unsafe { &*cursor };
        for_each_chunk(state, |chunk| {
            for slot in &chunk.slots {
                slot.count.store(0, Ordering::Relaxed);
                slot.class_id.store(NO_OWNER_CLASS, Ordering::Release);
            }
            true
        });
        cursor = state.next.load(Ordering::Acquire);
    }
    fallback().lock().clear();
}

#[cfg(test)]
mod tests {
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
}
