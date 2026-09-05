// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-call cost removal for the NIO socket transfer path.
//!
//! # What this module exists to delete
//!
//! Every `SocketChannel` transfer used to allocate and zero a fresh `Vec<u8>`
//! sized to the DESTINATION BUFFER'S REMAINING CAPACITY before it asked the
//! kernel how many bytes were actually waiting:
//!
//! ```text
//! let len = limit - position;          // the buffer the app OFFERED
//! let mut buf = vec![0u8; len];        // alloc + zero, every call
//! ```
//!
//! An HTTP event loop reading a 120-byte keep-alive request into netty's
//! 64 KiB receive buffer therefore paid a 64 KiB allocation and a 64 KiB
//! zeroing to move 120 bytes. The cost scaled with the buffer the application
//! offered rather than with the traffic it received, which is exactly backwards
//! for the small-message, high-frequency shape HTTP has.
//!
//! [`Scratch`] replaces that with a thread-local buffer held at its high-water
//! mark. In steady state a transfer allocates nothing and zeroes nothing: the
//! buffer is already long enough, so `resize` is a no-op and the bytes are
//! overwritten by the kernel. Only a request LARGER than anything this thread
//! has seen before grows (and therefore zeroes) the tail.
//!
//! # Why the buffer is not simply removed
//!
//! The obvious fix — hand the kernel the Java destination and skip the bounce
//! entirely — is **not available for a heap `ByteBuffer` under this VM's moving
//! collectors**. `array_data_ptr` yields a flat base pointer, but the transfer
//! happens inside [`NativeContext::begin_blocking_region`], during which a
//! concurrent collection may relocate the array. `pin_native_root` repairs a
//! *reference* across such a move; it does not stop the object moving, so a raw
//! data pointer taken before the syscall is invalid after it. Removing this
//! copy needs genuine address pinning (HotSpot's
//! `GetPrimitiveArrayCritical` equivalent), which does not exist here.
//!
//! For a DIRECT buffer the destination is off-heap and cannot move, so the copy
//! is removable in principle — but only when `Buffer.address` is a real
//! pointer. This tree has already measured that `ByteBuffer.allocateDirect`
//! hands back an `Unsafe`-arena HANDLE rather than a pointer
//! (`CRATONVM_DBG_DBB_ELEM=1` reports `raw-pointer=0 arena-handle=N`), and a
//! handle is not dereferenceable. Rather than build an unsafe fast path for a
//! case the evidence says never occurs, this module COUNTS the two populations
//! ([`stats::DIRECT_RAW`] vs [`stats::DIRECT_ARENA`]) so the question is
//! answered by measurement on a real HTTP workload before anyone writes it.
//!
//! # The slot cache
//!
//! `buffer_access` resolved `position`, `limit`, `hb` and `offset` through
//! `get_field_by_name` — and `buffer_advance` did one more get and one set the
//! same way. Each of those is
//! `resolve_field_index_in_hierarchy(class_id, name)` under the class-manager
//! read lock: a string hash and a hierarchy walk, six to eight times per
//! transfer, re-deriving a constant.
//!
//! [`BbSlots`] memoizes the resolution per receiver `ClassId`, which is the
//! same fix that took `ArrayList.size()` from 1014 ns to 337 ns in
//! `native-collections`. It is keyed on `ClassId` and not on a class NAME
//! because two loaders can define the same name with different layouts.
//!
//! # Kill switches
//!
//! Each cut is separately switchable so its A/B is one binary and an
//! environment variable, per the discipline that closed the `FileChannel` gap:
//!
//! * `CRATONVM_SC_SCRATCH=0` — per-call allocation returns.
//! * `CRATONVM_SC_BB_SLOTS=0` — `ByteBuffer` field access returns to by-name.
//! * `CRATONVM_SC_IO_STATS=1` — print the engagement census at exit.
//!
//! A census is not optional decoration. Without it, "the fast path ran on every
//! read" and "every read refused while the host happened to be quiet" produce
//! the same wall clock.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Kill switches
// ---------------------------------------------------------------------------

/// Is the reusable scratch buffer engaged? `CRATONVM_SC_SCRATCH=0` restores
/// the per-call `vec![0u8; remaining]`.
///
/// Latched in a `OnceLock` deliberately: a switch that could change mid-run
/// would make the two arms of an A/B non-comparable, and every consumer here
/// is on a hot path where a per-call environment read would itself be the cost
/// under measurement.
pub(crate) fn scratch_engaged() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_SC_SCRATCH")
            .ok()
            .as_deref()
            != Some("0")
    })
}

/// Is the per-`ClassId` `ByteBuffer` slot cache engaged?
/// `CRATONVM_SC_BB_SLOTS=0` restores `get_field_by_name`.
pub(crate) fn bb_slots_engaged() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_SC_BB_SLOTS")
            .ok()
            .as_deref()
            != Some("0")
    })
}

// ---------------------------------------------------------------------------
// Engagement census
// ---------------------------------------------------------------------------

pub mod stats {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Transfers that took a scratch buffer already long enough — no
    /// allocation, no zeroing. This is the row that says the fix engaged.
    pub static SCRATCH_HIT: AtomicU64 = AtomicU64::new(0);
    /// Transfers that had to grow the thread's scratch buffer. Should fall to
    /// ~zero after warm-up; a count that tracks `SCRATCH_HIT` means buffer
    /// sizes are unbounded and the high-water strategy is not paying.
    pub static SCRATCH_GROW: AtomicU64 = AtomicU64::new(0);
    /// Transfers that allocated because the switch is off or the request
    /// exceeded [`super::SCRATCH_MAX_RETAIN`].
    pub static SCRATCH_MISS: AtomicU64 = AtomicU64::new(0);

    /// `ByteBuffer` layout resolutions served from the per-`ClassId` cache.
    pub static SLOTS_HIT: AtomicU64 = AtomicU64::new(0);
    /// Layout resolutions that had to walk the hierarchy (one per new class).
    pub static SLOTS_FILL: AtomicU64 = AtomicU64::new(0);
    /// Layout resolutions that fell back to `get_field_by_name` because the
    /// class did not resolve a usable slot set.
    pub static SLOTS_REFUSED: AtomicU64 = AtomicU64::new(0);

    /// Socket transfers against a heap (`hb`-backed) buffer.
    pub static HEAP_BUF: AtomicU64 = AtomicU64::new(0);
    /// Socket transfers against a direct buffer whose `address` is a REAL
    /// pointer — the population for which the bounce copy is removable.
    pub static DIRECT_RAW: AtomicU64 = AtomicU64::new(0);
    /// Socket transfers against a direct buffer whose `address` is an
    /// `Unsafe`-arena handle — not dereferenceable, so the bounce is
    /// structural and a locked arena probe is paid per transfer.
    pub static DIRECT_ARENA: AtomicU64 = AtomicU64::new(0);

    /// Gathering writes served by a single scratch buffer (one copy pass).
    pub static GATHER_FAST: AtomicU64 = AtomicU64::new(0);
    /// Source buffers those gathering writes covered — the count of
    /// per-buffer allocations no longer made.
    pub static GATHER_BUFFERS: AtomicU64 = AtomicU64::new(0);

    /// `select()` ticks that mirrored nothing and therefore never took the
    /// process-global `sk_table` write lock.
    pub static SEL_TICK_CLEAN: AtomicU64 = AtomicU64::new(0);
    /// `select()` ticks that did take the write lock.
    pub static SEL_TICK_DIRTY: AtomicU64 = AtomicU64::new(0);
    /// Key rows actually written, across all dirty ticks. Compare against
    /// [`SEL_KEYS_WALKED`]: the ratio is the O(ready)/O(registered) win.
    pub static SEL_KEYS_MIRRORED: AtomicU64 = AtomicU64::new(0);
    /// Keys examined under the selector's own lock (cheap, uncontended).
    pub static SEL_KEYS_WALKED: AtomicU64 = AtomicU64::new(0);

    /// Ready keys appended straight into netty's `SelectedSelectionKeySet`.
    pub static SELKEYS_FAST: AtomicU64 = AtomicU64::new(0);
    /// Ready keys published through `invoke_virtual("add")` — an interpreter
    /// re-entry whose callee can never tier up.
    pub static SELKEYS_GENERIC: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(crate) fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// Print the census, if `CRATONVM_SC_IO_STATS` asks for it.
    ///
    /// Deliberately prints even when every row is zero: a run that reports
    /// `scratch hit=0` is telling you the path never engaged, which is the
    /// single most useful thing the census can say and the one an
    /// "only print if non-empty" guard would hide.
    pub fn report() {
        let asked = cratonvm_types::flags::runtime_var("CRATONVM_SC_IO_STATS")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
        if !asked {
            return;
        }
        eprintln!(
            "[cratonvm] socket fast I/O: scratch hit={} grow={} miss={}",
            get(&SCRATCH_HIT),
            get(&SCRATCH_GROW),
            get(&SCRATCH_MISS),
        );
        eprintln!(
            "[cratonvm] socket fast I/O: bb-slots hit={} fill={} refused={}",
            get(&SLOTS_HIT),
            get(&SLOTS_FILL),
            get(&SLOTS_REFUSED),
        );
        eprintln!(
            "[cratonvm] socket fast I/O: buffers heap={} direct-raw={} direct-arena={}",
            get(&HEAP_BUF),
            get(&DIRECT_RAW),
            get(&DIRECT_ARENA),
        );
        eprintln!(
            "[cratonvm] socket fast I/O: gather fast={} buffers={}",
            get(&GATHER_FAST),
            get(&GATHER_BUFFERS),
        );
        eprintln!(
            "[cratonvm] socket fast I/O: select ticks clean={} dirty={} keys mirrored={} walked={}",
            get(&SEL_TICK_CLEAN),
            get(&SEL_TICK_DIRTY),
            get(&SEL_KEYS_MIRRORED),
            get(&SEL_KEYS_WALKED),
        );
        eprintln!(
            "[cratonvm] socket fast I/O: selected-keys fast={} generic={}",
            get(&SELKEYS_FAST),
            get(&SELKEYS_GENERIC),
        );
    }
}

// ---------------------------------------------------------------------------
// The reusable transfer buffer
// ---------------------------------------------------------------------------

/// Largest buffer a thread will HOLD between transfers, in bytes.
///
/// A request above this is served by a one-off allocation and dropped, exactly
/// as before this module existed. The cap exists because the buffer is retained
/// for the life of the thread: without it, one `read` into a 256 MiB
/// `ByteBuffer` would pin 256 MiB per event-loop thread forever, trading a
/// throughput win for an unbounded RSS regression. 1 MiB comfortably covers
/// every socket buffer size an HTTP stack uses (netty's adaptive allocator
/// caps at 65 536) while bounding the retention at one page-run per thread.
const SCRATCH_MAX_RETAIN: usize = 1 << 20;

thread_local! {
    /// Per-thread transfer buffer, held at its high-water mark.
    ///
    /// Thread-local rather than pooled: every consumer is an event-loop or
    /// worker thread doing one transfer at a time, so a pool would add
    /// synchronisation to remove an allocation, which is the wrong trade. The
    /// `RefCell` is never borrowed across a call that can re-enter Java —
    /// [`Scratch::new`] MOVES the buffer out and [`Scratch::drop`] moves it
    /// back, so a nested transfer on the same thread simply gets its own
    /// buffer instead of panicking on a double borrow.
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A transfer buffer of exactly the requested length, returned to the thread's
/// high-water slot on drop.
///
/// Drop-based return rather than an explicit call because the transfer paths
/// have a dozen early returns each (closed channel, interrupt, connect
/// pending, async close, EOF); an explicit `give` would be forgotten on one of
/// them and the leak would show up only as the fix quietly not engaging.
pub(crate) struct Scratch {
    buf: Vec<u8>,
    len: usize,
}

impl Scratch {
    /// Obtain a buffer with at least `len` usable bytes.
    ///
    /// The contents are UNSPECIFIED, not zeroed — a reused buffer still holds
    /// the previous transfer's bytes. Every caller must therefore treat only
    /// the first `n` bytes the kernel reports as meaningful, which is what the
    /// socket paths already do. This is the whole point of the type: zeroing
    /// `limit - position` bytes per call was the cost being removed.
    pub(crate) fn new(len: usize) -> Self {
        if !scratch_engaged() || len > SCRATCH_MAX_RETAIN {
            stats::bump(&stats::SCRATCH_MISS);
            return Scratch {
                buf: vec![0u8; len],
                len,
            };
        }
        let mut buf = SCRATCH.with(|c| std::mem::take(&mut *c.borrow_mut()));
        if buf.len() < len {
            // Grows to the new high-water mark and zeroes only the ADDED tail.
            // Never shrinks: a `truncate` here would make the next larger
            // request re-zero ground it had already paid for.
            buf.resize(len, 0);
            stats::bump(&stats::SCRATCH_GROW);
        } else {
            stats::bump(&stats::SCRATCH_HIT);
        }
        Scratch { buf, len }
    }

    /// The writable region: exactly `len` bytes.
    #[inline]
    pub(crate) fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }

    /// The readable region: exactly `len` bytes.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// The first `n` bytes — the transferred prefix.
    ///
    /// Clamped to `len` rather than allowed to panic: `n` comes from a kernel
    /// return value, and a transport that answered more bytes than it was
    /// offered should truncate to what was asked for, not abort the VM.
    #[inline]
    pub(crate) fn prefix(&self, n: usize) -> &[u8] {
        &self.buf[..n.min(self.len)]
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !scratch_engaged() {
            return;
        }
        let buf = std::mem::take(&mut self.buf);
        if buf.capacity() == 0 || buf.len() > SCRATCH_MAX_RETAIN {
            return;
        }
        SCRATCH.with(|c| {
            // Keep whichever buffer is longer. A nested transfer that took its
            // own buffer must not shrink the outer one back down.
            let mut slot = c.borrow_mut();
            if buf.len() > slot.len() {
                *slot = buf;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Memoized ByteBuffer layout
// ---------------------------------------------------------------------------

/// The field slots `buffer_access` needs out of a `java.nio.ByteBuffer`.
///
/// `position` and `limit` are required — a buffer whose cursor cannot be found
/// cannot be transferred against at all. The rest are optional because the two
/// buffer families genuinely differ: a `HeapByteBuffer` has a non-null `hb`,
/// a `DirectByteBuffer` inherits `hb` as null and carries a non-zero `address`.
#[derive(Clone, Copy)]
pub(crate) struct BbSlots {
    pub(crate) position: usize,
    pub(crate) limit: usize,
    pub(crate) capacity: Option<usize>,
    pub(crate) hb: Option<usize>,
    pub(crate) offset: Option<usize>,
    pub(crate) address: Option<usize>,
}

/// Layout cache, keyed by `(vm_identity, ClassId)`.
///
/// `None` is a NEGATIVE entry: a class whose `position`/`limit` did not
/// resolve is remembered as unusable so the by-name fallback is not preceded
/// by a failed hierarchy walk on every subsequent call.
///
/// **The VM identity is load-bearing, not defensive.** A `ClassId` is an INDEX
/// into its own VM's `ClassStore` (`ClassId::new(self.classes.len())`), and
/// several `SharedVm`s can live in one process — which is why the JIT
/// invalidation hook has to fan out over `live_hook_vms()` rather than address
/// one. Keyed on `ClassId` alone, this cache would let one VM's
/// `HeapByteBuffer` layout answer another VM's query for whatever class
/// happens to share that index, and the result would be a wrong-but-in-bounds
/// slot read: no exception, just a channel that silently desynchronises.
/// `NativeContext::vm_identity`'s own doc states this requirement for exactly
/// this reason.
type BbCacheKey = (usize, ClassId);
fn bb_slot_cache(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<BbCacheKey, Option<BbSlots>>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::RwLock<rustc_hash::FxHashMap<BbCacheKey, Option<BbSlots>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Drop every cached layout.
///
/// Class redefinition can change a class's field layout under a live
/// `ClassId`, which would leave this cache naming slots that have moved — the
/// silent-wrong-answer failure mode, not a crash. The VM calls this from its
/// redefinition path for the same reason the quickening caches are keyed by
/// class generation.
pub fn invalidate_bb_slot_cache() {
    bb_slot_cache().write().clear();
}

/// Resolve (and memoize) the layout of `bb`'s class.
///
/// Returns `None` when the class has no usable cursor, in which case the
/// caller must fall back to `get_field_by_name` — which is not merely a slower
/// route to the same answer: it also serves the synthetic layouts that
/// `resolve_field_index_by_class_id` does not model.
pub(crate) fn bb_slots(ctx: &mut dyn NativeContext, bb: ObjectRef) -> Option<BbSlots> {
    if !bb_slots_engaged() {
        return None;
    }
    // FORWARDED, not the raw `class_id_of_object`: this id is paired with
    // `get_field(bb, slot)`, which forwards internally. Taking the id from an
    // un-forwarded address whose memory has since been reused would name a
    // different class's layout and read the wrong slot, in bounds and silently.
    let class_id = ctx.class_id_of_object_forwarded(bb);
    // `ClassId(0)` is `java.lang.Object` and is also what a failed validation
    // answers, so it is never a real buffer layout. Caching under it would let
    // one stale receiver poison every subsequent transfer.
    if class_id == ClassId::new(0) {
        stats::bump(&stats::SLOTS_REFUSED);
        return None;
    }
    let key = (ctx.vm_identity(), class_id);
    if let Some(hit) = bb_slot_cache().read().get(&key).copied() {
        if hit.is_some() {
            stats::bump(&stats::SLOTS_HIT);
        } else {
            stats::bump(&stats::SLOTS_REFUSED);
        }
        return hit;
    }
    let resolved = (|| {
        let position = ctx.resolve_field_index_by_class_id(class_id, "position")?;
        let limit = ctx.resolve_field_index_by_class_id(class_id, "limit")?;
        Some(BbSlots {
            position,
            limit,
            capacity: ctx.resolve_field_index_by_class_id(class_id, "capacity"),
            hb: ctx.resolve_field_index_by_class_id(class_id, "hb"),
            offset: ctx.resolve_field_index_by_class_id(class_id, "offset"),
            address: ctx.resolve_field_index_by_class_id(class_id, "address"),
        })
    })();
    bb_slot_cache().write().insert(key, resolved);
    if resolved.is_some() {
        stats::bump(&stats::SLOTS_FILL);
    } else {
        stats::bump(&stats::SLOTS_REFUSED);
    }
    resolved
}

/// Read an `int` field through a cached slot, falling back to by-name.
#[inline]
pub(crate) fn slot_int(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    slot: Option<usize>,
    name: &str,
) -> Value {
    match slot {
        Some(i) => ctx.get_field(obj, i),
        None => ctx.get_field_by_name(obj, name),
    }
}

/// Record which direct-buffer population this transfer served.
///
/// The distinction decides whether the bounce copy in the direct-buffer path is
/// removable at all — the kernel can be handed a real pointer and cannot be
/// handed an arena handle — so it is counted rather than assumed. The
/// classification lives on [`NativeContext`] because the tag bit belongs to
/// `native-builtins`, which depends on this crate.
#[inline]
pub(crate) fn note_direct_kind(ctx: &dyn NativeContext, addr: i64) {
    if ctx.native_addr_is_arena_handle(addr) {
        stats::bump(&stats::DIRECT_ARENA);
    } else {
        stats::bump(&stats::DIRECT_RAW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The high-water buffer must hand back exactly the requested length, and
    /// must not shrink when a smaller request follows a larger one — the
    /// shrink is what would reintroduce the zeroing this module removes.
    #[test]
    fn scratch_holds_high_water_mark() {
        {
            let mut big = Scratch::new(4096);
            assert_eq!(big.as_mut().len(), 4096);
            big.as_mut()[0] = 0xAB;
        }
        {
            let small = Scratch::new(16);
            assert_eq!(small.as_slice().len(), 16);
        }
        {
            // Back up to the earlier high-water mark: served without growing.
            let again = Scratch::new(4096);
            assert_eq!(again.as_slice().len(), 4096);
        }
    }

    /// A request above the retention cap must still be served correctly — it
    /// is a one-off allocation, not a refusal.
    #[test]
    fn scratch_serves_oversized_requests() {
        let big = Scratch::new(SCRATCH_MAX_RETAIN + 1);
        assert_eq!(big.as_slice().len(), SCRATCH_MAX_RETAIN + 1);
    }

    /// `prefix` clamps rather than panicking: `n` is a kernel return value and
    /// a transport answering more than it was offered must truncate.
    #[test]
    fn prefix_clamps_to_length() {
        let s = Scratch::new(8);
        assert_eq!(s.prefix(4).len(), 4);
        assert_eq!(s.prefix(99).len(), 8);
    }

    /// A nested `Scratch` on one thread must not deadlock or panic on the
    /// thread-local borrow, and the outer buffer must survive.
    #[test]
    fn nested_scratch_does_not_double_borrow() {
        let outer = Scratch::new(2048);
        let inner = Scratch::new(64);
        assert_eq!(outer.as_slice().len(), 2048);
        assert_eq!(inner.as_slice().len(), 64);
    }
}
