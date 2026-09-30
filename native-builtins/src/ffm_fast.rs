// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The FFM element-accessor fast path: a validation memo the JIT can consult.
//!
//! # The problem this exists to solve
//!
//! `MemorySegment.getAtIndex`/`setAtIndex` are the FFM ELEMENT accessors — any
//! segment-backed array drives them one element at a time. Reached through the
//! ordinary native dispatch funnel they cost ~1158 ns/element, against ~0.8 ns
//! for a `short[]` element and ~303 ns for `Unsafe.getShort(long)` (a
//! maximally lean native through the SAME funnel). So ~300 ns is the funnel and
//! ~850 ns is [`crate::panama::pe_segment_access_addr`] and its callees, which
//! are ~10 `NativeContext` round-trips per element — a scope-liveness walk
//! (segment → arena → session → state), then the address and the byte size,
//! each re-probing the carrier's shape, each paying heap validation.
//!
//! Measured consequence: kfusion's 256³ TSDF volume is a segment-backed
//! `ShortArray`, so every voxel is one of these, and that app runs ~126x slower
//! than HotSpot.
//!
//! # Why this is a memo and not a second copy of the checks
//!
//! The obvious fix — teach the JIT to do the checks itself — is the one this
//! module deliberately does NOT take. The liveness model spans two synthetic
//! classes whose slot conventions are owned by two different files
//! (`Arena{[0]=open,[1]=session}` here, the session's own slots in
//! `phases_late::foreign_ffm`), a segment's slot 2 and slot 4 are REUSED with
//! different meanings by `ofArray` carriers, and the carrier shapes (2, 3, 6
//! and 8 slots) are told apart by `object_num_fields` rather than by class.
//!
//! That is not a hypothetical hazard. The W7-89 note on `PE_ARENA_CLASS`
//! records that this file once kept its own copy of the session's `state` slot
//! index, and that second copy is what made the liveness check silently DEAD in
//! Compatible mode. A copy of it in the JIT would fail the same way, and the
//! failure mode is a read of freed native memory.
//!
//! So the checks stay in exactly one place — the native — and this module only
//! records THAT they passed:
//!
//! * the native publishes [`note_validated`] at the point where it has already
//!   proven the carrier is a plain native segment with a live scope;
//! * the JIT consults [`is_validated`] and, on a hit, re-reads the carrier's
//!   address/size slots itself and does the load.
//!
//! The JIT therefore never encodes the liveness model, only the three slot
//! indices it re-reads — which `ffm_fast_slot_indices_match_the_carrier` pins
//! against this file's own constants.
//!
//! # Why re-reading the slots is required, not an optimisation
//!
//! The memo caches the VERDICT, never the address or the size. A carrier's
//! `ptr`/`size`/`offset` slots are ordinary mutable slots, so caching their
//! values would go stale the moment anything rewrites them; re-reading costs
//! three loads and removes that entire class of staleness from the design.
//!
//! # What makes the verdict safe to reuse
//!
//! A verdict is keyed by `(carrier address, epoch)` and is per-thread.
//!
//! * **The address identifies the object** only for as long as no GC has run:
//!   a reclaimed-and-reallocated object can land on an address a stale verdict
//!   names. So a GC cycle bumps the epoch.
//! * **The scope can close** under the carrier, which frees the native block
//!   while the carrier still points at it. So freeing native memory bumps the
//!   epoch.
//!
//! * **The scope can be closed WITHOUT a free.** Round 12 wave 3 (lane ffm):
//!   the default arena model (`foreign_ffm`'s `ArenaImpl` carrier, with
//!   segments minted by `panama::pe_arena_allocate_impl`) never frees a
//!   block on close, so the free-side bump never ran and a verdict outlived
//!   `arena.close()` for as long as no collection happened. A thread already
//!   hitting on a segment kept hitting after the close and never threw
//!   `Already closed` — the hung worker of `R12RtSharedArenaClose`. A shared
//!   session's close now bumps too, and a confined one's drops the closing
//!   (owning) thread's verdicts ([`close_handshake_begin`]).
//!
//! All are coarse — one increment per GC, per free and per shared close, never
//! per access — and all are conservative: a bump only ever costs a
//! re-validation, which is the ordinary native path.
//!
//! Per-thread because a verdict is only ever produced by a full checked access
//! this thread performed, which keeps the memo a plain `Cell` with no sharing
//! and no lock on the hot path.
//!
//! # The close handshake
//!
//! A bump retires verdicts for accesses that START after it. An access already
//! past its check when another thread closes the scope would still complete
//! after `close()` returned — against a freed block, where the close frees.
//! HotSpot closes that window with a handshake in `closeScope0`. This module's
//! equivalent is [`AccessWindow`] (the accessor half, one per element access)
//! and [`close_handshake_begin`] / [`InFlight::wait`] (the close half); see
//! [`AccessWindow`] for the ordering argument.

use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The current validation epoch.
///
/// The counter itself lives in `cratonvm_types::ffm_epoch` because the two
/// events that must retire a verdict — freeing native memory, and a collection
/// — happen in `vm` and `gc`, crates that cannot see each other. See that
/// module for why there is exactly one counter and not one per crate.
#[inline]
pub fn epoch() -> u64 {
    cratonvm_types::ffm_epoch::ffm_epoch()
}

/// Invalidate every published verdict, on every thread.
///
/// Call this from anything that can (a) free native memory a carrier points at,
/// or (b) move or reclaim the carrier object itself. Both are covered at their
/// choke points: `free_native_memory` and the collector's cycle end.
///
/// Deliberately blunt. A verdict is cheap to rebuild — it costs one ordinary
/// native access — and the cost of being too clever here is a read of freed
/// memory.
#[inline]
pub fn bump_epoch() {
    cratonvm_types::ffm_epoch::bump_ffm_epoch();
}

/// One published verdict.
#[derive(Clone, Copy)]
struct Validated {
    /// The carrier object's address.
    carrier: u64,
    /// The epoch the verdict was published at.
    epoch: u64,
    /// A full checked READ of this carrier succeeded at that epoch.
    read: bool,
    /// A full checked WRITE succeeded — i.e. the carrier is also not read-only.
    write: bool,
}

/// Verdict slots per thread. See [`VALIDATED`].
const VERDICT_WAYS: usize = 4;

/// `CRATONVM_FFM_VERDICT_WAYS` clamps how many of [`VERDICT_WAYS`] are used.
///
/// `1` restores the single-slot behaviour this cache replaced, which is the
/// control arm for pricing the change on one binary. Anything outside
/// `1..=VERDICT_WAYS` is clamped into it.
fn effective_ways() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_FFM_VERDICT_WAYS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(VERDICT_WAYS)
            .clamp(1, VERDICT_WAYS)
    })
}

thread_local! {
    /// Carriers a full checked access has succeeded on, on this thread.
    ///
    /// # This was ONE slot, and the workload it was written for thrashed it
    ///
    /// AUDIT 2026-09-02. The original comment here read: "One entry, not a
    /// map: the workloads this exists for sweep ONE segment in a loop (a TSDF
    /// volume, a tensor, a pooled buffer), so a single slot has the same hit
    /// rate as a map... A second interleaved segment simply keeps missing,
    /// which is the ordinary native path and therefore correct, only not
    /// faster."
    ///
    /// The TSDF volume is kfusion, and kfusion is the app this whole fast
    /// path was built for. Measured there once the engagement census had a
    /// reporter (three frames of `kfusion.java.Benchmark`):
    ///
    /// ```text
    ///   consults=129,445,168  hits=92,226,077 (71.2%)  misses=37,219,091
    ///   native verdicts published=38,306,106
    /// ```
    ///
    /// 38.3M publishes against 129M consults — better than one full native
    /// verdict for every four elements. The same bench that sweeps a SINGLE
    /// segment publishes once for 10.5M consults, so this is not the fast
    /// path failing, it is the single slot being evicted by the second
    /// carrier and rebuilt, forever. Integration alternates the volume with
    /// the images it reads, and "only not faster" turned out to mean "pays
    /// the expensive path 28.8% of the time".
    ///
    /// Four ways, checked in order, most-recently-published first. A hit is
    /// at most four `u64` compares against a `Cell` copy, which is still far
    /// cheaper than the native round-trip it avoids, and a workload that
    /// really does sweep one segment still hits on the first compare.
    ///
    /// An empty slot is `carrier == 0`, which [`note_validated`] refuses to
    /// store, so no real carrier can collide with it.
    static VALIDATED: [Cell<Validated>; VERDICT_WAYS] = const {
        [const {
            Cell::new(Validated { carrier: 0, epoch: 0, read: false, write: false })
        }; VERDICT_WAYS]
    };
    /// Next way to evict, round-robin. Round-robin rather than
    /// least-recently-used: LRU needs a per-access write to record the use,
    /// and this path is per ELEMENT.
    static VICTIM: Cell<usize> = const { Cell::new(0) };
}

/// Record that a FULL, checked access to `carrier` just succeeded.
///
/// `write` says the access also cleared the read-only check. Called from the
/// native, at a point where every shape and liveness check has already passed.
///
/// A later `write` verdict on a carrier already validated for reads is merged
/// rather than replacing it, so a read-then-write loop does not alternate
/// between two verdicts and miss on every access.
///
/// Stamps the verdict with the epoch read HERE, after the checks, so a free on
/// another thread that lands between the checks and this call is not seen:
/// the verdict then outlives the block it vouches for. Prefer
/// [`note_validated_at`] with the epoch read before the checks.
pub fn note_validated(carrier: u64, write: bool) {
    note_validated_at(carrier, write, epoch());
}

/// [`note_validated`] for checks that began at epoch `observed`, which the
/// caller read ([`epoch`]) BEFORE its first liveness or shape check.
///
/// Publishes nothing when the epoch has moved since. The epoch only moves
/// forward, and each move is a free of native memory or a collection, so a
/// verdict stamped with the current epoch would vouch for a scope another
/// thread may have closed (and a block it freed) after the checks read it
/// live: every later fast-path access on this thread would then hit and touch
/// freed memory, until the next free or collection. Round 12 wave 2, lane rt
/// (`r12w2-rt-ffm-shared-arena-close-has-no-access-handshake`).
pub fn note_validated_at(carrier: u64, write: bool, observed: u64) {
    if carrier == 0 {
        return;
    }
    let now = epoch();
    if now != observed {
        return;
    }
    let ways = effective_ways();
    VALIDATED.with(|slots| {
        // Refresh this carrier's own way if it has one, so a read-then-write
        // loop merges into one entry instead of consuming two ways.
        for slot in slots.iter().take(ways) {
            let prev = slot.get();
            if prev.carrier == carrier && prev.epoch == now {
                slot.set(Validated {
                    carrier,
                    epoch: now,
                    read: true,
                    write: prev.write || write,
                });
                return;
            }
        }
        // Otherwise take a free way, preferring one that is empty or stale
        // before evicting a live verdict.
        let victim = slots
            .iter()
            .take(ways)
            .position(|s| {
                let v = s.get();
                v.carrier == 0 || v.epoch != now
            })
            .unwrap_or_else(|| {
                VICTIM.with(|v| {
                    let i = v.get() % ways;
                    v.set((i + 1) % ways);
                    i
                })
            });
        slots[victim].set(Validated {
            carrier,
            epoch: now,
            read: true,
            write,
        });
    });
}

/// Has `carrier` been fully validated on this thread, at the current epoch?
///
/// `want_write` asks for the stronger verdict (not read-only). Returns `false`
/// for anything not positively known, which is what makes every unknown case
/// fall back to the ordinary native path.
#[inline]
pub fn is_validated(carrier: u64, want_write: bool) -> bool {
    if carrier == 0 {
        return false;
    }
    let now = epoch();
    let ways = effective_ways();
    VALIDATED.with(|slots| {
        for slot in slots.iter().take(ways) {
            let v = slot.get();
            if v.carrier == carrier && v.epoch == now && v.read && (!want_write || v.write) {
                return true;
            }
        }
        false
    })
}

/// Drop this thread's verdict. Used by a confined close
/// ([`close_handshake_begin`]), by tests, and by any embedder that needs a
/// hard reset without waiting for an epoch bump. A no-op once the thread's
/// locals are being torn down.
pub fn forget_validated() {
    let _ = VALIDATED.try_with(|slots| {
        for slot in slots.iter() {
            slot.set(Validated {
                carrier: 0,
                epoch: 0,
                read: false,
                write: false,
            });
        }
    });
    let _ = VICTIM.try_with(|v| v.set(0));
}

// ---------------------------------------------------------------------------
// Layout verdicts (round 12 wave 5, lane ffm2)
// ---------------------------------------------------------------------------
//
// A carrier verdict says the SEGMENT may be touched; it says nothing about
// the LAYOUT the access goes through. The JIT element fast path used to take
// its element width from the call site's descriptor and nothing else, so a
// compiled `getAtIndex(JAVA_INT.withOrder(BIG_ENDIAN), i)` read host order and
// a compiled `getAtIndex(JAVA_LONG, 0)` on a misaligned slice never raised
// `IllegalArgumentException` once any access had validated the carrier
// (`r12w4-hunter2-ffm-element-access-ignores-layout-order-and-alignment`).
//
// The layout's properties stay with the native, exactly like the scope: the
// native publishes `(layout address, epoch) -> alignment mask` for a value
// layout it has read in the HOST byte order, and the helper declines unless
// the layout it was handed has a live verdict and the absolute address it is
// about to touch satisfies the mask. A layout in the other byte order never
// gets a verdict, so every such access stays on the native, which swaps.
// Keyed by address and retired by the same epoch as the carrier verdicts: a
// layout can only move (or its address be reused) across a collection, and a
// collection bumps the epoch.

/// One published layout verdict.
#[derive(Clone, Copy)]
struct LayoutVerdict {
    /// The layout object's address; 0 is an empty way.
    layout: u64,
    /// The epoch the verdict was published at.
    epoch: u64,
    /// `byteAlignment - 1` for the layout.
    align_mask: u64,
}

/// Layout verdicts per thread. More than [`VERDICT_WAYS`]: one hot loop can
/// touch several value layouts (a struct walked field by field) through one or
/// two segments.
const LAYOUT_WAYS: usize = 8;

thread_local! {
    /// Value layouts a native access has read in the host byte order, on this
    /// thread. See the section comment above.
    static LAYOUT_VERDICTS: [Cell<LayoutVerdict>; LAYOUT_WAYS] = const {
        [const { Cell::new(LayoutVerdict { layout: 0, epoch: 0, align_mask: 0 }) }; LAYOUT_WAYS]
    };
    /// Next layout way to evict, round-robin (see [`VICTIM`]).
    static LAYOUT_VICTIM: Cell<usize> = const { Cell::new(0) };
}

/// Record that `layout` is a value layout in the HOST byte order whose byte
/// alignment is `align`, as read by a native whose reads began at epoch
/// `observed` ([`epoch`], read BEFORE the layout's fields were).
///
/// Publishes nothing for a zero address, an alignment that is not a power of
/// two, or when the epoch has moved since `observed` (the layout may have
/// moved, and its old address may now name another object).
pub fn note_layout_validated_at(layout: u64, align: u64, observed: u64) {
    if layout == 0 || !align.is_power_of_two() {
        return;
    }
    let now = epoch();
    if now != observed {
        return;
    }
    let verdict = LayoutVerdict {
        layout,
        epoch: now,
        align_mask: align - 1,
    };
    let _ = LAYOUT_VERDICTS.try_with(|slots| {
        // Refresh this layout's own way if it has one.
        for slot in slots.iter() {
            let prev = slot.get();
            if prev.layout == layout && prev.epoch == now {
                slot.set(verdict);
                return;
            }
        }
        let victim = slots
            .iter()
            .position(|s| {
                let v = s.get();
                v.layout == 0 || v.epoch != now
            })
            .unwrap_or_else(|| {
                LAYOUT_VICTIM
                    .try_with(|v| {
                        let i = v.get() % LAYOUT_WAYS;
                        v.set((i + 1) % LAYOUT_WAYS);
                        i
                    })
                    .unwrap_or(0)
            });
        if let Some(slot) = slots.get(victim) {
            slot.set(verdict);
        }
    });
}

/// The alignment mask of `layout` if this thread holds a live verdict that it
/// is a host-order value layout; `None` (decline) for anything not positively
/// known, including a layout in the other byte order.
#[inline]
pub fn layout_align_mask(layout: u64) -> Option<u64> {
    if layout == 0 {
        return None;
    }
    let now = epoch();
    LAYOUT_VERDICTS
        .try_with(|slots| {
            slots
                .iter()
                .map(Cell::get)
                .find(|v| v.layout == layout && v.epoch == now)
                .map(|v| v.align_mask)
        })
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// The close handshake (round 12 wave 3, lane ffm)
// ---------------------------------------------------------------------------

/// `CRATONVM_FFM_CLOSE_RETIRES_VERDICTS` (default on): closing a shared session
/// bumps the epoch and closing a confined one drops the owner's verdicts, so no
/// verdict survives the close. Off restores the old behaviour, where only a
/// free or a collection retired one.
fn close_retires_verdicts() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_CLOSE_RETIRES_VERDICTS")
    })
}

/// `CRATONVM_FFM_ACCESS_HANDSHAKE` (default on): element accesses open an
/// [`AccessWindow`] and a close waits for the windows that were open when it
/// began. Off is the control arm for pricing the per-access cost.
fn access_handshake_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_ACCESS_HANDSHAKE")
    })
}

/// One thread's access sequence word: odd while the thread is inside an
/// [`AccessWindow`], even otherwise. Only the leasing thread changes the
/// value; a closer only probes it with a value-preserving RMW.
///
/// Aligned to its own cache lines so the per-access RMW never shares a line
/// with another thread's word.
#[repr(align(128))]
struct AccessWord {
    seq: AtomicU64,
    /// Held by a live thread. Read and written only under [`ACCESS_WORDS`].
    leased: AtomicBool,
}

/// Every access word ever minted. A word is leaked on purpose and recycled
/// through `leased`, so a `&'static` to it can never dangle and the set a
/// closer scans is bounded by the peak number of threads that touched a
/// segment at once, not by thread churn.
///
/// Process-wide rather than per VM: an access window is a property of a
/// thread, and a close in one VM that waits on another VM's in-flight element
/// access only waits a few instructions longer. It carries no compatibility
/// state.
static ACCESS_WORDS: OrderedPlMutex<Vec<&'static AccessWord>> =
    OrderedPlMutex::new(Vec::new(), LockLevel::Scratch);

fn lease_access_word() -> &'static AccessWord {
    let mut words = ACCESS_WORDS.lock();
    for &word in words.iter() {
        if !word.leased.load(Ordering::Relaxed) {
            word.leased.store(true, Ordering::Relaxed);
            return word;
        }
    }
    let word: &'static AccessWord = Box::leak(Box::new(AccessWord {
        seq: AtomicU64::new(0),
        leased: AtomicBool::new(true),
    }));
    words.push(word);
    word
}

/// This thread's lease on an [`AccessWord`], taken on its first element
/// access and handed back when the thread exits.
struct AccessLease {
    word: Cell<Option<&'static AccessWord>>,
    /// Open [`AccessWindow`]s on this thread. Only the outermost one toggles
    /// the word: the native path's window can reach Java (an exception
    /// constructor), and Java can reach another element access.
    depth: Cell<u32>,
}

impl Drop for AccessLease {
    fn drop(&mut self) {
        if let Some(word) = self.word.take() {
            // A thread exits outside every window; if one leaked, even the
            // word anyway so no closer waits on a thread that is gone.
            let seq = word.seq.load(Ordering::Relaxed);
            if seq & 1 == 1 {
                word.seq.store(seq.wrapping_add(1), Ordering::Release);
            }
            let _words = ACCESS_WORDS.lock();
            word.leased.store(false, Ordering::Relaxed);
        }
    }
}

thread_local! {
    static ACCESS_LEASE: AccessLease = const {
        AccessLease { word: Cell::new(None), depth: Cell::new(0) }
    };
}

/// The accessor half of the close handshake: open for the whole of one FFM
/// element access, from BEFORE the verdict's (or the scope check's) epoch read
/// until AFTER the last load or store of the block.
///
/// # Why this is sound
///
/// Opening does `fetch_add(1, Acquire)` on the thread's own word; closing it
/// stores the next even value with `Release`. A closer first marks the session
/// closed, then bumps the epoch, then probes every word with
/// `fetch_add(0, AcqRel)` ([`close_handshake_begin`]). The open and the probe
/// are RMWs on the same word, so one of them is first in its modification
/// order:
///
/// * **the open is first:** the probe reads an odd value, and the closer waits
///   ([`InFlight::wait`]) until the word changes, which it can only do through
///   this window's `Release` close (or a later RMW in its release sequence) —
///   so the whole access happens-before anything the closer does next,
///   including a free;
/// * **the probe is first:** the open reads from it and synchronises with it,
///   so the closed state and the bump both happen-before this access's epoch
///   read. The JIT verdict misses, and the native scope check reads the state
///   as closed and throws `Already closed`.
///
/// A word leased after the closer's scan is covered by the second case through
/// `ACCESS_WORDS`' lock: the bump precedes the scan's lock, which precedes
/// the lease.
///
/// # Cost
///
/// One uncontended locked RMW and one plain store per element, on a line only
/// this thread writes, plus two thread-local reads. Nothing else is added to
/// the path; the close side carries the rest (a scan and, rarely, a wait).
#[must_use]
pub struct AccessWindow {
    /// This guard toggled the depth (and possibly the word).
    opened: bool,
    /// The fast path may run: the window is open, or the handshake is off.
    admits: bool,
    /// Not `Send`: the depth it undoes on drop is the opening thread's.
    _thread_bound: std::marker::PhantomData<*const ()>,
}

impl AccessWindow {
    /// Open this thread's window. Never allocates on the Java heap and never
    /// reaches a safepoint, so it is callable from the JIT's no-safepoint
    /// helpers; a thread's first window takes the registry lock once, briefly.
    #[inline]
    pub fn open() -> AccessWindow {
        if !access_handshake_enabled() {
            return AccessWindow {
                opened: false,
                admits: true,
                _thread_bound: std::marker::PhantomData,
            };
        }
        let opened = ACCESS_LEASE
            .try_with(|lease| {
                let depth = lease.depth.get();
                if depth == 0 {
                    let word = match lease.word.get() {
                        Some(word) => word,
                        None => {
                            let word = lease_access_word();
                            lease.word.set(Some(word));
                            word
                        }
                    };
                    // Acquire: pairs with the closer's probe. See the type docs.
                    word.seq.fetch_add(1, Ordering::Acquire);
                }
                lease.depth.set(depth.saturating_add(1));
            })
            .is_ok();
        // During thread-local teardown there is no word to open. The JIT fast
        // path then declines; the native path proceeds as it did before the
        // handshake existed (no Java code runs that late in a thread's life).
        AccessWindow {
            opened,
            admits: opened,
            _thread_bound: std::marker::PhantomData,
        }
    }

    /// Whether the JIT fast path may use its verdict under this window.
    #[inline]
    pub fn admits_fast_path(&self) -> bool {
        self.admits
    }
}

impl Drop for AccessWindow {
    #[inline]
    fn drop(&mut self) {
        if !self.opened {
            return;
        }
        let _ = ACCESS_LEASE.try_with(|lease| {
            let depth = lease.depth.get().saturating_sub(1);
            lease.depth.set(depth);
            if depth == 0 {
                if let Some(word) = lease.word.get() {
                    // Only this thread changes the value, so the load reads the
                    // odd value this window's open wrote.
                    let seq = word.seq.load(Ordering::Relaxed);
                    word.seq.store(seq.wrapping_add(1), Ordering::Release);
                }
            }
        });
    }
}

/// The accessors a close must wait for: each word that was odd when probed,
/// with the value it was probed at.
#[must_use]
#[derive(Default)]
pub struct InFlight {
    pending: Vec<(&'static AccessWord, u64)>,
}

impl InFlight {
    /// No accessor was inside a window when the close probed.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Wait until every probed window has closed.
    ///
    /// The caller must be GC-safe while this runs (a native blocking region):
    /// an accessor can be suspended by a stop-the-world takeover, or be inside
    /// an exception constructor that allocates, and the collector must not
    /// wait for the closer while the closer waits for it. It waits for each
    /// word to CHANGE, not to become even, so an accessor that keeps opening
    /// new windows cannot starve the close.
    pub fn wait(self) {
        for (word, probed) in self.pending {
            let mut spins = 0u32;
            while word.seq.load(Ordering::Acquire) == probed {
                if spins < 128 {
                    std::hint::spin_loop();
                    spins += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }
}

/// The close half of the handshake. Call it AFTER the session's state word
/// says closed and BEFORE any block the session governs is freed; then, if
/// the answer is not empty, [`InFlight::wait`] on it inside a blocking region.
///
/// `shared` is whether any thread but the caller may hold a verdict on, or be
/// inside an access to, the session's segments:
///
/// * **shared:** every verdict on every thread is retired (a bump), and every
///   window open at the probe is returned for the caller to wait on;
/// * **confined:** only this thread's verdicts are dropped, and nothing is
///   scanned. A verdict is published only after the native's owner check
///   passed on the publishing thread, so the owner — the only thread allowed
///   to close a confined session — is the only thread that can hold one. Not
///   bumping keeps a `try (Arena a = Arena.ofConfined())` loop on one thread
///   from sending every other thread's hot segments back through the native
///   path on every close.
///
/// The calling thread's own word is never waited for: its window, if open,
/// encloses this call.
pub fn close_handshake_begin(shared: bool) -> InFlight {
    if !shared {
        if close_retires_verdicts() {
            forget_validated();
        }
        return InFlight::default();
    }
    if close_retires_verdicts() {
        bump_epoch();
    }
    if !access_handshake_enabled() {
        return InFlight::default();
    }
    let own = ACCESS_LEASE.try_with(|lease| lease.word.get()).ok().flatten();
    let words = ACCESS_WORDS.lock();
    let mut pending = Vec::new();
    for &word in words.iter() {
        if own.is_some_and(|own| std::ptr::eq(own, word)) {
            continue;
        }
        // A value-preserving RMW, not a load: see `AccessWindow`.
        let seq = word.seq.fetch_add(0, Ordering::AcqRel);
        if seq & 1 == 1 {
            pending.push((word, seq));
        }
    }
    InFlight { pending }
}

// ---------------------------------------------------------------------------
// Counters — engagement, not decoration
// ---------------------------------------------------------------------------
//
// A fast path that is structurally present but never taken looks exactly like
// a fast path that works, right up until someone prices it. These separate
// "the JIT asked" from "the JIT was allowed", so a disappointing measurement
// can be attributed instead of guessed at.

static FAST_HITS: AtomicU64 = AtomicU64::new(0);
static FAST_MISSES: AtomicU64 = AtomicU64::new(0);

/// Consults a thread has counted but not yet added to [`FAST_HITS`] /
/// [`FAST_MISSES`].
///
/// Round 11: the consult ran a `lock xadd` on one of two process-wide words
/// per ELEMENT access — the ~1 ns target this module exists for — and every
/// thread walking a segment bounced the same cache line. The tally is now a
/// plain per-thread `Cell`, published in batches of [`CONSULT_FLUSH_EVERY`],
/// whenever the shared total is still zero (so "the JIT never asked" is never
/// reported for a thread that did ask), when the thread exits (`Drop`), and by
/// the reporting thread itself in [`fast_path_counts`]. What another live
/// thread has not flushed yet — fewer than `CONSULT_FLUSH_EVERY` per kind — is
/// the only thing the exit line can now under-count.
struct ConsultTally {
    hits: Cell<u64>,
    misses: Cell<u64>,
}

impl ConsultTally {
    fn flush(&self) {
        let hits = self.hits.replace(0);
        if hits != 0 {
            FAST_HITS.fetch_add(hits, Ordering::Relaxed);
        }
        let misses = self.misses.replace(0);
        if misses != 0 {
            FAST_MISSES.fetch_add(misses, Ordering::Relaxed);
        }
    }
}

impl Drop for ConsultTally {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Batch size for [`ConsultTally`]: one shared RMW per this many consults.
const CONSULT_FLUSH_EVERY: u64 = 1024;

thread_local! {
    static CONSULT_TALLY: ConsultTally = const {
        ConsultTally { hits: Cell::new(0), misses: Cell::new(0) }
    };
}

/// Count one consult of [`is_validated`] from compiled code.
///
/// A thread-local increment in the steady state (see [`ConsultTally`]). This
/// runs per ELEMENT, so it must stay that cheap — an earlier version also did
/// a modulo and an env lookup here to print progress, which is real work on
/// the hot path this exists to make fast. Read the totals with
/// [`fast_path_counts`] instead.
#[inline]
pub fn note_fast_consult(hit: bool) {
    let global: &AtomicU64 = if hit { &FAST_HITS } else { &FAST_MISSES };
    let counted = CONSULT_TALLY.try_with(|t| {
        let cell = if hit { &t.hits } else { &t.misses };
        let n = cell.get() + 1;
        // A relaxed LOAD of a word nobody is writing stays in every core's
        // cache; it is the RMW that bounced.
        if n >= CONSULT_FLUSH_EVERY || global.load(Ordering::Relaxed) == 0 {
            cell.set(0);
            global.fetch_add(n, Ordering::Relaxed);
        } else {
            cell.set(n);
        }
    });
    if counted.is_err() {
        // The thread's tally is already destroyed (a consult during TLS
        // teardown): count straight into the total.
        global.fetch_add(1, Ordering::Relaxed);
    }
}

static PUBLISHES: AtomicU64 = AtomicU64::new(0);

/// Count one verdict publish from the native. One relaxed increment.
#[inline]
pub fn note_publish() {
    PUBLISHES.fetch_add(1, Ordering::Relaxed);
}

/// Verdicts published by the native — the denominator that says whether the
/// compiled side ever ASKED. `publishes` high with `fast_hits` zero is the
/// signature of an intrinsic registered in a door the workload does not use,
/// which is how both wiring defects in this feature were found.
pub(crate) fn publish_count() -> u64 {
    PUBLISHES.load(Ordering::Relaxed)
}

/// `(hits, misses)` for the compiled FFM element fast path. Flushes the
/// calling thread's own unpublished tally first (see [`ConsultTally`]).
pub(crate) fn fast_path_counts() -> (u64, u64) {
    let _ = CONSULT_TALLY.try_with(ConsultTally::flush);
    (
        FAST_HITS.load(Ordering::Relaxed),
        FAST_MISSES.load(Ordering::Relaxed),
    )
}

/// One line at exit, when this process touched an FFM segment at all.
///
/// # This existed as three counters that nothing printed
///
/// AUDIT 2026-09-02. The header of this module says it plainly -- "a fast
/// path that is structurally present but never taken looks exactly like a
/// fast path that works, right up until someone prices it" -- and then
/// `fast_path_counts` and `publish_count` had no caller anywhere in the
/// workspace. The counters were correct and invisible, which is the same
/// state as not having them.
///
/// How to read it:
///
/// * `publishes` is the denominator: verdicts the NATIVE published. High
///   publishes with zero consults means compiled code never asked, i.e.
///   the intrinsic is registered in a door this workload does not use, or
///   the hot code is not compiled at all.
/// * `hits` vs `misses` is whether the fast path, once asked, was
///   ALLOWED. Misses are the declines: a heap carrier, a closed scope, an
///   index the bounds check refused.
///
/// Silent when nothing published and nothing consulted, so a run that
/// never touches FFM does not grow a line.
pub fn exit_summary() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let (hits, misses) = fast_path_counts();
    let publishes = publish_count();
    if hits + misses + publishes == 0 {
        return;
    }
    ONCE.call_once(|| {
        let consults = hits + misses;
        eprintln!(
            "[cratonvm] ffm element fast path: consults={consults} hits={hits} \
             misses={misses} ({:.1}% of consults hit); native verdicts \
             published={publishes}",
            100.0 * hits as f64 / consults.max(1) as f64,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests in this module.
    ///
    /// The verdict table is per-THREAD but the epoch is a process-global,
    /// and `a_verdict_is_scoped_to_its_carrier_and_epoch` bumps it. Without
    /// this, that bump lands in the middle of a sibling running on another
    /// thread and invalidates verdicts it just published — which is exactly
    /// how the two carrier tests below failed in the suite and passed when
    /// run alone.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the lock and start from an empty table.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        forget_validated();
        g
    }

    /// The per-thread consult tally (round 11) loses nothing the reporting
    /// thread counted: `fast_path_counts` flushes the caller's own batch, and
    /// other threads can only add.
    #[test]
    fn batched_consults_are_all_visible_to_the_counting_thread() {
        let (h0, m0) = fast_path_counts();
        for _ in 0..(CONSULT_FLUSH_EVERY + 5) {
            note_fast_consult(true);
        }
        for _ in 0..3 {
            note_fast_consult(false);
        }
        let (h1, m1) = fast_path_counts();
        assert!(h1 >= h0 + CONSULT_FLUSH_EVERY + 5, "hits {h0} -> {h1}");
        assert!(m1 >= m0 + 3, "misses {m0} -> {m1}");
    }

    #[test]
    fn a_verdict_is_scoped_to_its_carrier_and_epoch() {
        let _g = guard();
        note_validated(0x1000, false);
        assert!(is_validated(0x1000, false), "the published carrier hits");
        assert!(
            !is_validated(0x2000, false),
            "a different carrier must never hit a single-slot memo"
        );
        assert!(
            !is_validated(0x1000, true),
            "a read verdict must not answer a write question"
        );

        // Anything that could free the block or move the carrier invalidates.
        bump_epoch();
        assert!(
            !is_validated(0x1000, false),
            "an epoch bump must retire every published verdict"
        );
    }

    #[test]
    fn a_write_verdict_merges_rather_than_replacing_the_read_one() {
        let _g = guard();
        note_validated(0x1000, false);
        note_validated(0x1000, true);
        assert!(is_validated(0x1000, false));
        assert!(
            is_validated(0x1000, true),
            "the write verdict must be retained"
        );
        // And the merge must not manufacture a write verdict for a carrier that
        // only ever passed the read checks.
        note_validated(0x3000, false);
        assert!(!is_validated(0x3000, true));
    }

    /// The defect the census found: two carriers used alternately, which is
    /// what kfusion's integration stage does (the TSDF volume and the images
    /// it reads). With one slot each switch evicted the other and republished.
    ///
    /// Verified able to fail: with `CRATONVM_FFM_VERDICT_WAYS=1` — the
    /// single-slot behaviour this replaced — the second assertion fails on
    /// the first alternation.
    #[test]
    fn two_carriers_used_alternately_both_stay_validated() {
        let _g = guard();
        let a = 0x1_0000u64;
        let b = 0x2_0000u64;
        note_validated(a, false);
        note_validated(b, false);
        for _ in 0..8 {
            assert!(is_validated(a, false), "carrier A was evicted by B");
            assert!(is_validated(b, false), "carrier B was evicted by A");
        }
    }

    /// Four is the width, so a fifth carrier must cost one of the others —
    /// but only one, and the survivors must stay valid. A cache that dropped
    /// everything on an overflow would be the single slot again with extra
    /// steps.
    #[test]
    fn a_fifth_carrier_evicts_one_way_not_the_table() {
        let _g = guard();
        for i in 1..=(VERDICT_WAYS as u64 + 1) {
            note_validated(i * 0x1000, false);
        }
        let live = (1..=(VERDICT_WAYS as u64 + 1))
            .filter(|i| is_validated(i * 0x1000, false))
            .count();
        assert_eq!(
            live, VERDICT_WAYS,
            "expected exactly {VERDICT_WAYS} carriers to survive, got {live}"
        );
        assert!(
            is_validated((VERDICT_WAYS as u64 + 1) * 0x1000, false),
            "the most recent carrier must always be the one that is kept"
        );
    }

    /// Round 12 wave 2 (lane rt): a verdict whose checks began before an epoch
    /// bump (a free on another thread, a collection) is never published, so it
    /// cannot vouch for a block freed while the checks ran.
    #[test]
    fn a_verdict_whose_checks_straddle_an_epoch_bump_is_not_published() {
        let _g = guard();
        let observed = epoch();
        // The free (or collection) lands after the checks read the scope live.
        bump_epoch();
        note_validated_at(0x5000, true, observed);
        assert!(
            !is_validated(0x5000, false),
            "a verdict from before the bump vouched for a freed block"
        );
        // Checks that began after the bump publish as usual.
        note_validated_at(0x5000, true, epoch());
        assert!(is_validated(0x5000, true));
    }

    /// Round 12 wave 3 (lane ffm): the default arena model closes a session
    /// without freeing anything, so before the close bumped the epoch a
    /// thread's verdict survived `arena.close()` and its compiled accesses
    /// kept hitting — the hung worker of `R12RtSharedArenaClose`.
    #[test]
    fn a_close_retires_every_verdict_even_without_a_free() {
        let _g = guard();
        note_validated(0x6000, true);
        assert!(is_validated(0x6000, true));
        let in_flight = close_handshake_begin(false);
        assert!(in_flight.is_empty(), "a confined close scans nothing");
        assert!(
            !is_validated(0x6000, false),
            "a verdict outlived the close of its scope"
        );
    }

    /// A shared close retires verdicts on every thread (the bump), not only on
    /// the closer: the hung worker held its verdict on ANOTHER thread.
    #[test]
    fn a_shared_close_retires_verdicts_published_on_another_thread() {
        let _g = guard();
        let (published_tx, published_rx) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            note_validated(0x7000, false);
            let before = is_validated(0x7000, false);
            let _ = published_tx.send(());
            let _ = closed_rx.recv();
            (before, is_validated(0x7000, false))
        });
        let _ = published_rx.recv();
        close_handshake_begin(true).wait();
        let _ = closed_tx.send(());
        let (before, after) = worker.join().unwrap_or((false, true));
        assert!(before, "the worker's verdict was never published");
        assert!(!after, "the worker's verdict survived a shared close");
    }

    /// The accessor half: a close that probes while another thread is inside
    /// a window waits until that window closes, and not before.
    #[test]
    fn a_close_waits_for_an_access_window_that_was_open() {
        use std::sync::mpsc;
        let _g = guard();
        let (opened_tx, opened_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let accessor = std::thread::spawn(move || {
            let outer = AccessWindow::open();
            assert!(outer.admits_fast_path());
            // Nested: only the outermost window toggles the word.
            let inner = AccessWindow::open();
            drop(inner);
            let _ = opened_tx.send(());
            let _ = release_rx.recv();
            drop(outer);
        });
        let _ = opened_rx.recv();
        let in_flight = close_handshake_begin(true);
        assert!(
            !in_flight.is_empty(),
            "the open window (still open after its nested window closed) was not probed"
        );
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let waiter = {
            let done = std::sync::Arc::clone(&done);
            std::thread::spawn(move || {
                in_flight.wait();
                done.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !done.load(Ordering::SeqCst),
            "the close stopped waiting while the access was still in flight"
        );
        let _ = release_tx.send(());
        let _ = accessor.join();
        let _ = waiter.join();
        assert!(done.load(Ordering::SeqCst));
    }

    /// A close made from inside this thread's own window (a close action run
    /// from an access, however unlikely) must not wait for itself.
    #[test]
    fn a_close_never_waits_for_the_closing_thread() {
        let _g = guard();
        let window = AccessWindow::open();
        close_handshake_begin(true).wait();
        drop(window);
    }

    /// Round 12 wave 5 (lane ffm2): a layout verdict is scoped to its address
    /// and its epoch, carries the layout's alignment mask, and is never
    /// published for a checks-straddling-a-bump read or a bogus alignment.
    #[test]
    fn a_layout_verdict_is_scoped_to_its_address_and_epoch() {
        let _g = guard();
        assert_eq!(layout_align_mask(0x9_0000), None, "nothing published yet");
        assert_eq!(layout_align_mask(0), None, "null is never validated");
        // The epoch is process-wide and other test modules bump it (a shared
        // close handshake does), so each positive check is retried until one
        // attempt saw a stable epoch; the negative checks hold regardless.
        let stable = |check: &dyn Fn(u64) -> bool| {
            (0..32).any(|attempt| {
                let now = epoch();
                let ok = check(0x9_1000 + attempt * 0x100);
                epoch() == now && ok
            })
        };
        assert!(
            stable(&|layout: u64| {
                note_layout_validated_at(layout, 4, epoch());
                layout_align_mask(layout) == Some(3) && layout_align_mask(layout + 8) == None
            }),
            "a published layout must hit with its mask, and only at its address"
        );
        assert!(
            stable(&|layout: u64| {
                note_layout_validated_at(layout, 1, epoch());
                layout_align_mask(layout) == Some(0)
            }),
            "an unaligned layout's mask admits every address"
        );

        let layout = 0x9_8000u64;
        note_layout_validated_at(layout, 4, epoch());
        bump_epoch();
        assert_eq!(
            layout_align_mask(layout),
            None,
            "a collection (epoch bump) must retire a layout verdict"
        );
        let observed = epoch();
        bump_epoch();
        note_layout_validated_at(layout, 8, observed);
        assert_eq!(
            layout_align_mask(layout),
            None,
            "a verdict whose reads straddled a bump must not be published"
        );
        note_layout_validated_at(layout, 3, epoch());
        assert_eq!(layout_align_mask(layout), None, "3 is not an alignment");
    }

    /// More value layouts than ways: the newest one always survives and the
    /// table keeps exactly `LAYOUT_WAYS` live.
    #[test]
    fn layout_verdicts_evict_one_way_at_a_time() {
        let _g = guard();
        let n = LAYOUT_WAYS as u64 + 3;
        // The epoch is process-wide and other test modules bump it (a shared
        // close handshake does), so retry until one pass saw a stable epoch.
        for _attempt in 0..32 {
            let now = epoch();
            for i in 1..=n {
                note_layout_validated_at(0xA_0000 + i * 0x100, 8, now);
            }
            let live = (1..=n)
                .filter(|i| layout_align_mask(0xA_0000 + i * 0x100).is_some())
                .count();
            let newest = layout_align_mask(0xA_0000 + n * 0x100);
            if epoch() != now {
                continue;
            }
            assert_eq!(live, LAYOUT_WAYS);
            assert_eq!(newest, Some(7), "the newest layout must always be kept");
            return;
        }
    }

    #[test]
    fn a_zero_carrier_is_never_validated() {
        let _g = guard();
        note_validated(0, true);
        assert!(!is_validated(0, false), "null must not be fast-pathed");
    }
}
