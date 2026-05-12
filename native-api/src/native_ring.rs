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

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const RING_SIZE: usize = 64;

#[derive(Clone, Copy)]
struct Entry {
    cb_ptr: usize,
    thread_id: u64,
    enter_ms: u128,
    exit_ms: u128,
}

const EMPTY: Entry = Entry {
    cb_ptr: 0,
    thread_id: 0,
    enter_ms: 0,
    exit_ms: 0,
};

struct Ring {
    entries: [Entry; RING_SIZE],
    next: usize,
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    entries: [EMPTY; RING_SIZE],
    next: 0,
});

fn name_map() -> &'static Mutex<FxHashMap<usize, String>> {
    static MAP: OnceLock<Mutex<FxHashMap<usize, String>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Register a callback pointer → name mapping.
pub fn register_name(cb_ptr: usize, triple: &str) {
    if cb_ptr == 0 {
        return;
    }
    let mut m = name_map().lock();
    m.entry(cb_ptr).or_insert_with(|| triple.to_string());
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Record a native-method entry. Returns a slot index for `record_exit`.
#[inline]
pub fn record_enter(cb_ptr: usize) -> usize {
    let tid = thread_id_u64();
    let mut ring = RING.lock();
    let idx = ring.next;
    ring.entries[idx] = Entry {
        cb_ptr,
        thread_id: tid,
        enter_ms: now_ms(),
        exit_ms: 0,
    };
    ring.next = (ring.next + 1) % RING_SIZE;
    idx
}

#[inline]
pub fn record_exit(idx: usize) {
    let now = now_ms();
    let mut ring = RING.lock();
    if let Some(slot) = ring.entries.get_mut(idx) {
        slot.exit_ms = now;
    }
}

/// Dump the ring buffer to stderr, oldest entry first.
pub fn dump_to_stderr() {
    let ring = RING.lock();
    let names = name_map().lock();
    eprintln!("--- native-call ring buffer (last {RING_SIZE} entries, oldest first) ---");
    let now = now_ms();
    let mut any = false;
    for i in 0..RING_SIZE {
        let idx = (ring.next + i) % RING_SIZE;
        let e = ring.entries[idx];
        if e.cb_ptr == 0 {
            continue;
        }
        any = true;
        let name = names
            .get(&e.cb_ptr)
            .cloned()
            .unwrap_or_else(|| format!("<unknown cb@{:#x}>", e.cb_ptr));
        let dur = if e.exit_ms == 0 {
            format!("STILL-IN-NATIVE({}ms ago)", now.saturating_sub(e.enter_ms))
        } else {
            format!("{}ms", e.exit_ms.saturating_sub(e.enter_ms))
        };
        eprintln!("  [{:02}] tid={} {} {}", i, e.thread_id, dur, name);
    }
    if !any {
        eprintln!("  (empty — no native methods recorded)");
    }
    eprintln!("--- end native-call ring buffer ---");
}

fn thread_id_u64() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    format!("{:?}", std::thread::current().id()).hash(&mut h);
    h.finish()
}
