// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-global ring buffer recording the last N native methods entered.
//!
//! Diagnostic aid for hangs in native (Rust) code. When the watchdog
//! fires with zero Java thread responses, it dumps this buffer so we
//! can see exactly which native method we were last inside.
//!
//! Design:
//! - At each `callback(&mut ctx, &args)` call site, we record the
//!   callback function-pointer into a ring buffer (cheap, lock-held
//!   only for a few stores).
//! - `NativeMethodRegistry::register` calls `register_name` so we have
//!   a `fn-ptr → "class.method desc"` map for the dump.
//!
//! Performance:
//! - Recording is **off by default**. With ~3,100 registered natives,
//!   the prior unconditional `parking_lot::Mutex<Ring>` round-trip on
//!   every native call (twice — enter + exit) was a dominant dispatch
//!   cost, serializing across all OS threads.
//! - Hot path when disabled: a single `AtomicBool` relaxed load + branch.
//! - Call [`enable`] (e.g. from the watchdog arm site) to turn recording
//!   on. [`is_enabled`] reports current state.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const RING_SIZE: usize = 64;
const TOKEN_SLOT_BITS: usize = 6;
const TOKEN_SLOT_MASK: usize = RING_SIZE - 1;
const TOKEN_GENERATION_MASK: usize = usize::MAX >> TOKEN_SLOT_BITS;
const DISABLED_TOKEN: usize = usize::MAX;

/// Master switch. When `false` (the default), `record_enter` and
/// `record_exit` early-return after a single relaxed load. The watchdog
/// (or other diagnostic code) should call `enable(true)` when arming.
///
/// This default of `false` is intentional, not an accidental disable: the
/// ring is deliberately dormant until a diagnostic is requested, at which
/// point the recording path imposes no runtime cost.
///
/// WIRED UP: the CLI arms this via `native_ring::enable(true)` from
/// `vm-cli/src/main.rs` when `ring_recording_requested` is set
/// (`--stack-dump-on-timeout=N>0` or `CRATONVM_ENABLE_NATIVE_RING=1`). The
/// previous "TODO: re-arm" note was stale and has been removed.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn ring-buffer recording on or off. Off by default.
#[inline]
pub fn enable(b: bool) {
    ENABLED.store(b, Ordering::Relaxed);
}

/// Is ring-buffer recording currently enabled?
#[inline]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Would [`record_enter`] / [`record_exit`] do any work at all?
///
/// `record_enter` has *two* independent gates: the runtime-toggleable ring
/// ([`enable`]) and the `CRATONVM_TRACK_NATIVE` per-thread native stack, which
/// is checked first and is a separate env var. A caller that wants to skip the
/// call entirely on the hot path has to know about both, and `track_enabled`
/// is private — so ask here rather than approximating with [`is_enabled`],
/// which would silently disable native tracking.
///
/// ARCH-2026-08-04 A3: `safe_native_call_impl` uses this to fold both gates
/// into its single diagnostic mask, so the common (all-off) native call does
/// not reach `record_enter` at all.
#[inline]
pub fn any_recording_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) || track_enabled()
}

#[derive(Clone, Copy)]
struct Entry {
    cb_ptr: usize,
    thread_id: u64,
    os_tid: u64,
    enter_ms: u128,
    exit_ms: u128,
    generation: usize,
}

const EMPTY: Entry = Entry {
    cb_ptr: 0,
    thread_id: 0,
    os_tid: 0,
    enter_ms: 0,
    exit_ms: 0,
    generation: 0,
};

struct Ring {
    entries: [Entry; RING_SIZE],
    next: usize,
    next_generation: usize,
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    entries: [EMPTY; RING_SIZE],
    next: 0,
    next_generation: 0,
});

fn name_map() -> &'static Mutex<FxHashMap<usize, String>> {
    static MAP: OnceLock<Mutex<FxHashMap<usize, String>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Deferred-resolution name map: `cb_ptr → fn() -> String`.
///
/// FIX (review STUB/P2, native_ring.rs ~119): boot registers ~3,100 natives
/// and the eager `register_name` paid a `format!` allocation **plus** a
/// `Mutex` lock per native at boot for a feature that is OFF by default. We
/// can't simply skip-while-disabled, because boot registration happens
/// *before* the watchdog arms recording, so skipped names would be lost and
/// the eventual dump would show only raw `<unknown cb@0x...>` pointers
/// (useless for diagnosing a native livelock). Instead, callers can register
/// a cheap zero-allocation `fn() -> String` resolver (a bare function
/// pointer, no per-call `String`), and the name is materialized lazily only
/// when the ring is actually inspected (`name_of` / `dump_to_stderr`).
fn lazy_name_map() -> &'static Mutex<FxHashMap<usize, fn() -> String>> {
    static MAP: OnceLock<Mutex<FxHashMap<usize, fn() -> String>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Resolve a callback pointer back to its registered
/// `class.method desc` name, if known. Used by diagnostic code (the
/// dispatch tracer / watchdog) to render the hung native by name.
///
/// Checks the eagerly-populated string map first, then falls back to the
/// deferred resolver map (materializing — and caching — the name on first
/// use so a repeated dump pays the resolver only once).
pub fn name_of(cb_ptr: usize) -> Option<String> {
    if cb_ptr == 0 {
        return None;
    }
    if let Some(s) = name_map().lock().get(&cb_ptr).cloned() {
        return Some(s);
    }
    // Lazy fallback: run the resolver, then promote the result into the
    // eager map so subsequent lookups (and the watchdog dump) skip it.
    let resolver = lazy_name_map().lock().get(&cb_ptr).copied();
    if let Some(f) = resolver {
        let s = f();
        name_map().lock().entry(cb_ptr).or_insert_with(|| s.clone());
        return Some(s);
    }
    None
}

/// Register a callback pointer → name mapping eagerly.
///
/// NOTE: this allocates a `String` at the call site. Prefer
/// [`register_name_lazy`] on the boot hot path (~3,100 natives) so the
/// name is only materialized if/when a diagnostic dump actually needs it.
pub fn register_name(cb_ptr: usize, triple: &str) {
    if cb_ptr == 0 {
        return;
    }
    let mut m = name_map().lock();
    m.entry(cb_ptr).or_insert_with(|| triple.to_string());
}

/// Register a callback pointer → name *resolver* (deferred / lazy).
///
/// The resolver is a bare `fn() -> String` (no captured state, no heap
/// allocation to store), invoked only when [`name_of`] / [`dump_to_stderr`]
/// actually need the human-readable name. This keeps boot cheap: a single
/// map insert of a function pointer, with no `format!` allocation per native
/// while the ring is dormant (the common case).
///
/// CROSS-FILE FOLLOW-UP (out of this file's scope): to realize the full boot
/// win, `NativeMethodRegistry::register` in `native-api/src/registry.rs`
/// (~2629) should call this with a resolver that formats
/// `"{class}.{method}{descriptor}"` on demand instead of eagerly building
/// the `String` and calling [`register_name`]. That requires the registry to
/// hold the three name parts in a form a `fn` pointer can reach (e.g. via an
/// interned/indexed table), so it is flagged here rather than edited.
pub fn register_name_lazy(cb_ptr: usize, resolver: fn() -> String) {
    if cb_ptr == 0 {
        return;
    }
    lazy_name_map().lock().entry(cb_ptr).or_insert(resolver);
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn make_token(slot: usize, generation: usize) -> usize {
    ((generation & TOKEN_GENERATION_MASK) << TOKEN_SLOT_BITS) | (slot & TOKEN_SLOT_MASK)
}

fn token_slot(token: usize) -> usize {
    token & TOKEN_SLOT_MASK
}

fn token_generation(token: usize) -> usize {
    (token >> TOKEN_SLOT_BITS) & TOKEN_GENERATION_MASK
}

/// Record a native-method entry. Returns a token for `record_exit`.
///
/// When recording is disabled (the default), this is a single relaxed
/// atomic load + branch. Returns `usize::MAX` as a sentinel so
/// `record_exit` can also short-circuit without touching the lock.
#[inline]
pub fn record_enter(cb_ptr: usize) -> usize {
    // Per-thread current-native stack push (crash-handler diagnostic). Done
    // FIRST and independently of the ring's `ENABLED` gate; balanced by the
    // pop in `record_exit`. No cross-thread lock — see the stack's doc below.
    if track_enabled() {
        let _ = NATIVE_STACK.try_with(|s| s.borrow_mut().push(cb_ptr));
    }
    if !ENABLED.load(Ordering::Relaxed) {
        return usize::MAX;
    }
    let tid = thread_id_u64();
    let os_tid = os_thread_id_u64();
    let mut ring = RING.lock();
    let idx = ring.next;
    let mut generation = ring.next_generation & TOKEN_GENERATION_MASK;
    let mut token = make_token(idx, generation);
    if token == DISABLED_TOKEN {
        generation = generation.wrapping_add(1) & TOKEN_GENERATION_MASK;
        token = make_token(idx, generation);
    }
    ring.entries[idx] = Entry {
        cb_ptr,
        thread_id: tid,
        os_tid,
        enter_ms: now_ms(),
        exit_ms: 0,
        generation,
    };
    ring.next = (ring.next + 1) % RING_SIZE;
    ring.next_generation = generation.wrapping_add(1) & TOKEN_GENERATION_MASK;
    token
}

#[inline]
pub fn record_exit(token: usize) {
    // Pop the per-thread current-native stack (see `record_enter`). Done
    // FIRST and independently of the ring's `ENABLED`/sentinel gate so the
    // stack stays balanced even if `enable`/`track` toggled mid-call.
    if track_enabled() {
        let _ = NATIVE_STACK.try_with(|s| {
            s.borrow_mut().pop();
        });
    }
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // Sentinel returned by `record_enter` when disabled at entry time.
    // (Recording could have been toggled on between enter and exit; in
    // that case we skip this exit rather than write to a bogus slot.)
    if token == DISABLED_TOKEN {
        return;
    }
    let now = now_ms();
    let mut ring = RING.lock();
    let idx = token_slot(token);
    let generation = token_generation(token);
    if let Some(slot) = ring.entries.get_mut(idx) {
        if slot.generation == generation {
            slot.exit_ms = now;
        }
    }
}

// ---------------------------------------------------------------------------
// Low-overhead per-thread current-native stack (crash-handler diagnostic).
//
// The full ring above takes a process-global `Mutex` on EVERY native enter and
// exit — by design useful only for the watchdog's one-shot hang dump, but far
// too heavy to leave on under a multi-thread allocation storm (it serializes
// all OS threads and perturbs the very timing-sensitive races we want to catch,
// e.g. HIB-CV-37 `testQueryConcurrency`). This parallel mechanism is a thread-
// LOCAL stack of active native callback pointers: a push on enter, a pop on
// exit, no cross-thread lock. The VEH crash handler reads the faulting thread's
// innermost entry to name the native that was driving a lambda when a stranded
// (GC-relocated, un-rewritten Rust-local) `ObjectRef` faulted.
//
// Gated by `CRATONVM_TRACK_NATIVE=1` so the default path is byte-identical (a
// single cached bool load + not-taken branch; no thread-local touch, no alloc).

fn track_enabled() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_TRACK_NATIVE").is_some())
}

thread_local! {
    static NATIVE_STACK: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Innermost active native callback pointer on THIS thread, or 0 if none.
/// Safe to call from the VEH crash handler (it runs on the faulting thread, so
/// it reads that thread's own thread-local). Returns 0 unless
/// `CRATONVM_TRACK_NATIVE=1` armed the tracker.
pub fn innermost_native_cb() -> usize {
    if !track_enabled() {
        return 0;
    }
    NATIVE_STACK
        .try_with(|s| s.borrow().last().copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Resolve the innermost active native on this thread to its
/// `class.method descriptor` name, for the crash report. `None` if the tracker
/// is disabled, the stack is empty, or the name is unregistered.
pub fn innermost_native_name() -> Option<String> {
    let cb = innermost_native_cb();
    if cb == 0 {
        return None;
    }
    name_of(cb)
}

/// Dump the ring buffer to stderr, oldest entry first.
///
/// If recording was never enabled (or no native methods were recorded
/// while it was on), prints a notice that the ring is empty rather
/// than nothing — so the watchdog dump is still self-explanatory.
pub fn dump_to_stderr() {
    // Snapshot the ring under its own lock, then release it *before*
    // resolving names. Name resolution goes through `name_of`, which locks
    // the (separate, non-reentrant) name maps internally — so we must NOT
    // hold any name-map lock here, and we keep the ring lock for the copy
    // only. FIX (review STUB): names may now live in the lazy resolver map,
    // so resolve via `name_of` rather than reading `name_map()` directly.
    let snapshot: Vec<(usize, Entry)> = {
        let ring = RING.lock();
        let mut v = Vec::with_capacity(RING_SIZE);
        for i in 0..RING_SIZE {
            let idx = (ring.next + i) % RING_SIZE;
            v.push((i, ring.entries[idx]));
        }
        v
    };
    eprintln!("--- native-call ring buffer (last {RING_SIZE} entries, oldest first) ---");
    if !ENABLED.load(Ordering::Relaxed) {
        eprintln!("  (recording disabled — call native_ring::enable(true) to capture)");
    }
    let now = now_ms();
    let mut any = false;
    for (i, e) in snapshot {
        if e.cb_ptr == 0 {
            continue;
        }
        any = true;
        let name = name_of(e.cb_ptr).unwrap_or_else(|| format!("<unknown cb@{:#x}>", e.cb_ptr));
        let dur = if e.exit_ms == 0 {
            format!("STILL-IN-NATIVE({}ms ago)", now.saturating_sub(e.enter_ms))
        } else {
            format!("{}ms", e.exit_ms.saturating_sub(e.enter_ms))
        };
        eprintln!(
            "  [{:02}] tid={} os_tid={} {} {}",
            i, e.thread_id, e.os_tid, dur, name
        );
    }
    if !any {
        eprintln!("  (empty — no native methods recorded)");
    }
    eprintln!("--- end native-call ring buffer ---");
}

/// Per-thread stable u64 id, assigned on first call and cached for the
/// life of the thread. Avoids the `String` allocation + SipHash that
/// `format!("{:?}", ThreadId).hash(...)` cost per native call.
fn thread_id_u64() -> u64 {
    static NEXT_TID: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static TID: u64 = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    }
    TID.with(|t| *t)
}

#[cfg(target_os = "linux")]
fn os_thread_id_u64() -> u64 {
    // SAFETY: gettid has no preconditions and returns the calling kernel TID.
    unsafe { libc::syscall(libc::SYS_gettid) as u64 }
}

#[cfg(not(target_os = "linux"))]
fn os_thread_id_u64() -> u64 {
    thread_id_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    // Distinct high sentinel pointers, unlikely to collide with real
    // registrations in the shared process-global maps.
    const PTR_EAGER: usize = 0xDEAD_0001;
    const PTR_LAZY: usize = 0xDEAD_0002;
    const PTR_WRAP: usize = 0xDEAD_0003;

    fn ring_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn reset_ring_for_test() {
        let mut ring = RING.lock();
        ring.entries = [EMPTY; RING_SIZE];
        ring.next = 0;
        ring.next_generation = 0;
        let _ = NATIVE_STACK.try_with(|s| s.borrow_mut().clear());
    }

    struct RingResetGuard;

    impl Drop for RingResetGuard {
        fn drop(&mut self) {
            enable(false);
            reset_ring_for_test();
        }
    }

    #[test]
    fn eager_register_resolves() {
        register_name(PTR_EAGER, "java/lang/Foo.bar()V");
        assert_eq!(name_of(PTR_EAGER).as_deref(), Some("java/lang/Foo.bar()V"));
    }

    #[test]
    fn lazy_register_resolves_and_promotes() {
        // FIX (review STUB): zero-allocation deferred registration. The
        // resolver runs only when the name is actually needed.
        fn resolver() -> String {
            "java/lang/Baz.qux()V".to_string()
        }
        register_name_lazy(PTR_LAZY, resolver);
        // First lookup runs the resolver...
        assert_eq!(name_of(PTR_LAZY).as_deref(), Some("java/lang/Baz.qux()V"));
        // ...and promotes the result into the eager map for subsequent hits.
        assert!(name_map().lock().contains_key(&PTR_LAZY));
        assert_eq!(name_of(PTR_LAZY).as_deref(), Some("java/lang/Baz.qux()V"));
    }

    #[test]
    fn null_ptr_is_ignored() {
        register_name(0, "should/not/Register.x()V");
        assert_eq!(name_of(0), None);
    }

    #[test]
    fn unknown_ptr_is_none() {
        assert_eq!(name_of(0x0BAD_BEEF_usize), None);
    }

    #[test]
    fn record_exit_ignores_stale_token_after_wraparound() {
        let _guard = ring_test_lock().lock();
        let _reset = RingResetGuard;
        enable(true);
        reset_ring_for_test();

        let stale = record_enter(PTR_WRAP);
        let stale_slot = token_slot(stale);
        let stale_generation = token_generation(stale);
        for i in 0..RING_SIZE {
            let _ = record_enter(0xBEEF_0000usize + i);
        }

        {
            let ring = RING.lock();
            let reused = ring.entries[stale_slot];
            assert_ne!(reused.cb_ptr, PTR_WRAP);
            assert_ne!(reused.generation, stale_generation);
            assert_eq!(reused.exit_ms, 0);
        }

        record_exit(stale);

        {
            let ring = RING.lock();
            let reused = ring.entries[stale_slot];
            assert_ne!(reused.cb_ptr, PTR_WRAP);
            assert_ne!(reused.generation, stale_generation);
            assert_eq!(reused.exit_ms, 0);
        }
    }
}
