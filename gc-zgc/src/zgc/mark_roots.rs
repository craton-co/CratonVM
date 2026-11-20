// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The per-object extra-root filter: four global lookups per marked object,
//! reduced to two indexed loads.
//!
//! # What this is for
//!
//! Every marked object's out-edges are its own reference slots PLUS four
//! things the heap does not store in it:
//!
//! | edge | keyed by | table |
//! |---|---|---|
//! | the pinned defining `ClassLoader` | CLASS id | `loader_pin` |
//! | the class mirrors a loader owns | owner ADDRESS | `mirror_pin` |
//! | the metadata a loader owns | owner ADDRESS | `metadata_pin` |
//! | a native collection's overlay | owner ADDRESS | `external_roots` |
//!
//! Each of the three mark loops asks all four, per object, and each answer
//! cost (before this filter) a lock plus a hash lookup that overwhelmingly
//! returns nothing. Ten million marked objects is forty million lock-and-hash
//! operations to learn four times over that an ordinary `String` owns no
//! classes.
//!
//! # Why the existing fast paths are not enough
//!
//! Three of the four latch a `NON_EMPTY` flag and exit on one relaxed load
//! when their table has never been written -- so on a program that loads no
//! class through a user `ClassLoader` they really are free, and this filter
//! changes nothing.
//!
//! `external_roots` is the exception, and it is the one that matters:
//! `native-collections` registers its overlay provider at VM startup, so
//! `PROVIDER_COUNT` is non-zero on *every* run and the latch never fires. Each
//! marked object entered the provider callback (the provider list is a
//! lock-free published pointer today; it was an `RwLock` when this was
//! written) and took that module's own lock over the owner index. That is the per-object cost
//! this filter exists to remove, and the other three come along for free
//! because they are keyed the same way.
//!
//! # The shape
//!
//! Two lock-free structures, built once per collection and read per object:
//!
//! * a **class-id bitmap** for the loader pin -- exact, because class ids are
//!   dense and small, with an overflow latch for the ones that do not fit;
//! * an **address bloom** for the other three, which share a key space. A
//!   false positive costs one real lookup that returns nothing, which is the
//!   status quo; a false negative would drop a live edge. Inside a PAUSE the
//!   bloom cannot produce one: every owner address is inserted before the
//!   mark begins and no owner is added during it (the tables are written by
//!   class definition and by native registration, both of which are mutator
//!   work).
//!
//! Across a CONCURRENT phase that clause does not hold -- mutators run and can
//! register an owner the snapshot has never seen -- and the filter IS armed
//! there by default (`rebuild_within` at mark start, since zr z7/cm;
//! `CRATONVM_ZGC_MARK_ROOT_FILTER_CONC=0` kills it). Its soundness there rests
//! on the mark-end difference pass ([`ZMarkRootFilter::late_owners_within`],
//! harvested by `collect_garbage` before its own rebuild) and the fail-closed
//! [`ZLateRoots::certifiable`] refusal in `finish_concurrent_mark`. The one
//! piece still unwired is the per-batch validity check
//! ([`ZMarkRootFilter::is_current`], stage 3c of
//! `docs/feature-designs/zgc-architecture.md`).
//! (Corrected zs s2/mc2: this said none of it was wired.)
//!
//! Same two-hash bloom the reference-skip set uses a few hundred lines away in
//! `zgc.rs`, for the same reason: it is the cheapest structure that can answer
//! "definitely not" without a lock.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

/// 512 words = 32 Kib of bloom, over however many owner addresses exist.
/// Owners are class loaders and native collection handles -- thousands at
/// most, against 32,768 bits, so the false-positive rate stays negligible
/// without tuning.
pub const Z_ROOT_BLOOM_WORDS: usize = 512;

/// 1,024 words = 65,536 class ids, exactly. Beyond that the overflow latch
/// sends every lookup down the slow path, which is correct and merely slower.
pub const Z_ROOT_CLASS_WORDS: usize = 1024;

/// `CRATONVM_ZGC_MARK_ROOT_FILTER`: consult the filter before the four global
/// tables. Default on; `0`/`off`/`false`/`no` asks them per object as before.
///
/// # The measurement
///
/// `BinTreesClassic 16` at `-Xmx192m`, release, interleaved on a quiet host,
/// identical checksums:
///
/// ```text
///   mark_us (mean per cycle)     wall clock
///   on     4774   4154   4079     2455  2277  2332 ms
///   off    6903   7747   9319     2445  2302  2597 ms
/// ```
///
/// The mark pause halves, 3/3. **The wall clock does not move**, and that is
/// not a contradiction to explain away: on this workload the mark is 4-9 ms of
/// a ~2,400 ms run, so halving it is worth about a percent and the run-to-run
/// spread is wider than that. This is a PAUSE change. It pays where the live
/// set is large and the tables are populated -- a Spring or H2 workload with
/// user class loaders and native-backed collections -- and the number to read
/// there is `mark_us` on the `[GC] zgc-pause:` line, not the benchmark's
/// total.
///
/// Read per COLLECTION, not latched, so both arms can run in one process.
/// `CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA` (zr z8/rv7): default ON, `=0`
/// kill. The concurrent mark-end repair
/// ([`ZMarkRootFilter::late_owners_within`]) declines to certify a mark set
/// when a pin-table row present at mark start is gone at mark end. With this
/// on, only a row whose OWNER lies in this heap's arena counts; a row another
/// VM in the same process removed during the phase (its own collection's
/// `replace_*`, its teardown's `forget_vm_*`) no longer forces a
/// stop-the-world re-mark (`parallel_mark_fallbacks`). `=0` counts every row
/// in the process, as before. Read once per mark start / mark end.
pub fn zgc_root_filter_repair_own_arena() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => true,
    }
}

/// The `loader_pin` class ids with a row inside `arena`, sorted (zr z8/rv7).
/// `snapshot_where` answers the single row of an id without consulting the
/// predicate, so the address is screened here.
fn own_loader_pin_ids(arena: &std::ops::Range<usize>) -> Vec<u32> {
    let mut own: Vec<u32> = cratonvm_types::loader_pin::snapshot_where(|a| arena.contains(&a))
        .map(|m| {
            m.into_iter()
                .filter(|&(_, addr)| arena.contains(&addr))
                .map(|(id, _)| id)
                .collect()
        })
        .unwrap_or_default();
    own.sort_unstable();
    own
}

pub fn filter_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// One **published, immutable** snapshot of the four tables' key sets.
///
/// Every field is written only by a [`ZMarkRootFilter::rebuild`] that is
/// filling this bank while some *other* bank is the live one, and is read only
/// after that rebuild published it. The atomics are therefore not there for
/// mutual exclusion; they are there because the fill and the reads are on
/// different threads and the publish is a release/acquire pair on
/// `ZMarkRootFilter::state`.
struct ZRootSnapshot {
    /// Class ids with a pinned defining loader.
    class_bits: [AtomicU64; Z_ROOT_CLASS_WORDS],
    /// Owner addresses in `mirror_pin`, `metadata_pin` or any external-root
    /// provider.
    addr_bloom: [AtomicU64; Z_ROOT_BLOOM_WORDS],
    /// A class id too large for `class_bits`: every class then takes the slow
    /// path, because the bitmap can no longer answer "definitely not".
    class_overflow: AtomicBool,
    /// Is any class pinned at all? Skips even the bitmap load.
    any_class: AtomicBool,
    /// Is any address an owner at all?
    any_addr: AtomicBool,
    /// At least one external-root provider declined to enumerate its owners,
    /// so no address may be excluded on its behalf. See
    /// `ExternalRootProvider::owner_addrs`.
    overlay_unfilterable: AtomicBool,
    /// The three `types/` registries' contents versions at the moment this
    /// bank was filled, and the external-root **provider table**'s version.
    ///
    /// Lock-free and separate from [`ZRootKeySets`] deliberately:
    /// [`ZMarkRootFilter::is_current`] is designed to be called once per drain
    /// batch from every mark worker, and a `Mutex` there would be a shared
    /// cache line taken exclusive N times per 1,024 objects — the exact shape
    /// this module exists to delete.
    class_gen: AtomicU64,
    mirror_gen: AtomicU64,
    metadata_gen: AtomicU64,
    overlay_provider_gen: AtomicU64,
    /// The key sets themselves, retained so the mark-end repair pass
    /// ([`ZMarkRootFilter::late_owners`]) can compute a DIFFERENCE rather than
    /// re-root everything. Read once per collection, never on the per-object
    /// path, so the lock costs nothing that matters.
    key_sets: Mutex<ZRootKeySets>,
}

/// The four key sets a [`ZRootSnapshot`] was built from, kept verbatim.
///
/// The bloom and the bitmap are lossy on purpose — they answer "definitely not"
/// and nothing else. The mark-end repair needs the other direction: *which
/// owners exist now that did not exist when the snapshot was taken*, and a
/// bloom cannot be asked that. So the exact key sets are retained beside it.
///
/// Sorted vectors rather than hash sets for the three `types/` tables: they are
/// built by one sort per collection and consumed by `binary_search`, which is
/// cheaper than hashing for the few thousand entries these tables ever hold and
/// allocates once instead of per bucket. The overlay owners arrive as a
/// `HashSet` from `external_roots` and are kept in that form.
#[derive(Default)]
struct ZRootKeySets {
    /// Sorted, from `loader_pin::pinned_class_ids_with_generation`.
    class_ids: Vec<u32>,
    /// Sorted, from `mirror_pin::pinned_owner_addrs_with_generation`.
    mirror_owners: Vec<usize>,
    /// Sorted, from `metadata_pin::pinned_owner_addrs_with_generation`.
    metadata_owners: Vec<usize>,
    /// From `external_roots::owner_addrs_and_completeness_with_generation`.
    overlay_owners: HashSet<usize>,
    /// Whether every provider was able to enumerate its owners when the
    /// snapshot was taken. A provider that goes from "can" to "cannot" mid-
    /// cycle cannot be repaired by a difference set, because it will not say
    /// what to repair.
    overlay_complete: bool,
    /// zr z8/rv7: the class ids among `class_ids` that had a row inside THIS
    /// heap's arena when the snapshot was taken, sorted -- `None` when the
    /// snapshot was taken without an arena ([`ZMarkRootFilter::rebuild`]),
    /// in which case every id counts as this heap's (the pre-z8 check). Filled
    /// by [`ZMarkRootFilter::rebuild_within`] only. See
    /// [`zgc_root_filter_repair_own_arena`].
    class_ids_own: Option<Vec<u32>>,
}

impl ZRootKeySets {
    fn clear(&mut self) {
        self.class_ids_own = None;
        self.class_ids.clear();
        self.mirror_owners.clear();
        self.metadata_owners.clear();
        self.overlay_owners.clear();
        self.overlay_complete = false;
    }
}

impl ZRootSnapshot {
    fn empty() -> Box<Self> {
        Box::new(ZRootSnapshot {
            class_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            addr_bloom: std::array::from_fn(|_| AtomicU64::new(0)),
            class_overflow: AtomicBool::new(false),
            any_class: AtomicBool::new(false),
            any_addr: AtomicBool::new(false),
            overlay_unfilterable: AtomicBool::new(false),
            class_gen: AtomicU64::new(0),
            mirror_gen: AtomicU64::new(0),
            metadata_gen: AtomicU64::new(0),
            overlay_provider_gen: AtomicU64::new(0),
            key_sets: Mutex::new(ZRootKeySets::default()),
        })
    }

    /// Zero everything, ready to be filled. Only legal on the bank that is
    /// **not** live.
    fn clear(&self) {
        for w in self.class_bits.iter() {
            w.store(0, Ordering::Relaxed);
        }
        for w in self.addr_bloom.iter() {
            w.store(0, Ordering::Relaxed);
        }
        self.class_overflow.store(false, Ordering::Relaxed);
        self.any_class.store(false, Ordering::Relaxed);
        self.any_addr.store(false, Ordering::Relaxed);
        self.overlay_unfilterable.store(false, Ordering::Relaxed);
        self.class_gen.store(0, Ordering::Relaxed);
        self.mirror_gen.store(0, Ordering::Relaxed);
        self.metadata_gen.store(0, Ordering::Relaxed);
        self.overlay_provider_gen.store(0, Ordering::Relaxed);
        self.key_sets.lock().clear();
    }
}

/// What the mark-end repair pass found — see [`ZMarkRootFilter::late_owners`].
///
/// Deliberately **not** `Default`: the derived one would hand out
/// `certifiable: false`, and a value that refuses to certify a mark set should
/// only ever come from a function that decided to.
#[derive(Debug)]
pub struct ZLateRoots {
    /// Addresses that must be pushed through `ZMarkCoordinator::push_roots`
    /// **before** the mark-end fixed point, because the filter may have
    /// excluded their owner while the owner was not yet in the snapshot.
    ///
    /// Empty on every cycle in which nothing registered — which is almost all
    /// of them.
    pub addrs: Vec<u64>,
    /// **`false` means this mark set must not be certified.**
    ///
    /// A row that was in the snapshot and is gone now breaks the one
    /// precondition the repair rests on (see the function docs): an owner that
    /// was added *and then removed* inside the cycle can have had a wrong
    /// `false` taken against it and leaves nothing behind to re-push. The safe
    /// response is the one `finish_concurrent_mark` already has for an
    /// uncertified mark set — fall back to a stop-the-world mark — not a
    /// best-effort repair.
    pub certifiable: bool,
    /// How many of the four sources changed during the cycle. Telemetry: it
    /// distinguishes "the repair was empty because nothing happened" from "the
    /// repair was empty because the difference was empty", which are the same
    /// `addrs.len() == 0` and very different facts about a workload.
    pub sources_moved: u32,
    /// WHICH source refused certification (zs s2/mc2), a bit set:
    /// [`Z_LATE_REFUSED_LOADER`], [`Z_LATE_REFUSED_MIRROR`],
    /// [`Z_LATE_REFUSED_METADATA`], [`Z_LATE_REFUSED_OVERLAY_REMOVED`],
    /// [`Z_LATE_REFUSED_OVERLAY_INCOMPLETE`]. `0` iff `certifiable`. Before it
    /// every refusal was logged as "a pin registry row was REMOVED", which is
    /// wrong for a provider that stopped enumerating its owners.
    pub refused: u8,
    /// zt t1/mk (residual 4 of `zgc-zs2mc2-mark-engine-review-residuals`):
    /// late additions NOT pushed because they belong to another heap -- a
    /// foreign loader row, a foreign mirror / metadata owner, or an overlay
    /// root outside this heap's arena. `claim_child` refused every one of
    /// them anyway (`OffHeap`), so dropping them here changes no mark bit; it
    /// saves the per-owner table lookups and keeps `late_owners=` about this
    /// heap. Zero when the arena is empty or under
    /// `CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA=0` (every row counts).
    pub screened_foreign: usize,
}

/// [`ZLateRoots::refused`]: a `loader_pin` row of this heap was removed.
pub const Z_LATE_REFUSED_LOADER: u8 = 1;
/// [`ZLateRoots::refused`]: a `mirror_pin` owner of this heap was removed.
pub const Z_LATE_REFUSED_MIRROR: u8 = 2;
/// [`ZLateRoots::refused`]: a `metadata_pin` owner of this heap was removed.
pub const Z_LATE_REFUSED_METADATA: u8 = 4;
/// [`ZLateRoots::refused`]: an overlay owner of this heap was removed.
pub const Z_LATE_REFUSED_OVERLAY_REMOVED: u8 = 8;
/// [`ZLateRoots::refused`]: an overlay provider stopped enumerating its owners.
pub const Z_LATE_REFUSED_OVERLAY_INCOMPLETE: u8 = 16;

impl ZLateRoots {
    /// Nothing was proved from a snapshot, so there is nothing to repair.
    fn nothing_to_repair() -> Self {
        ZLateRoots {
            addrs: Vec::new(),
            certifiable: true,
            sources_moved: 0,
            refused: 0,
            screened_foreign: 0,
        }
    }
}

/// `state` value meaning "no snapshot is published; prove nothing".
const Z_ROOT_DISARMED: usize = 0;

/// Which objects and classes have edges the heap does not store in them.
///
/// Rebuilt at the start of every mark. Reading it never takes a lock, which is
/// the whole point: it is consulted once per marked object.
///
/// # Why there are two banks (2026-09-21)
///
/// *From
/// `docs/feature-designs/zgc-architecture.md`.*
///
/// This structure is rebuilt in the collection safepoint — `zgc.rs`'s
/// `collect_garbage` calls `rebuild()` and only *then* calls
/// `finish_concurrent_mark`, which is what drives the concurrent mark workers
/// to a fixed point and joins them. So in the concurrent configuration
/// **`rebuild` runs while mark worker threads are live and calling
/// `extra_root_probe`**. The mutators are stopped, so the snapshot it builds is
/// fresh; the readers are not.
///
/// The 2026-09-20 fix stored `armed = false` before clearing and commented that
/// it bought "this can only ever be read as the old snapshot or the new one,
/// never as a mixture". A release store does not buy that. It orders *this*
/// thread's later writes after it, and it becomes visible to an acquire load —
/// but a reader that has *already executed* its `armed` load and is now indexing
/// the bitmaps observes the clearing loop regardless. This structure's `false`
/// is a **proof**, so half a bloom is not a slower answer, it is a wrong one:
/// a dropped live edge and a reachable object swept.
///
/// It was unreachable, because `armed` happens to already be `false` at every
/// rebuild — the previous collection's `disarm()` is the last statement of
/// `collect_garbage`. That is an invariant owned by a different file, holding up
/// memory safety in this one, asserted nowhere.
///
/// Two banks remove the question instead of documenting it. A rebuild fills the
/// bank that is not live and publishes it with one release store of `state`; a
/// reader either sees the old state and reads the old bank — **intact, because
/// nothing writes to it** — or sees the new state and reads the new bank,
/// complete by release/acquire. There is no third outcome, and `rebuild` becomes
/// legal to call at any time, which is the precondition for arming the filter
/// during a concurrent phase at all.
///
/// The hot path pays nothing for it: `state` is loaded exactly where `armed` was,
/// with the same `Acquire` and the same branch, and the bank is then a computed
/// offset off an already-loaded pointer.
///
/// # What two banks do NOT buy
///
/// They do not make an *older* snapshot true. A reader that observes the old
/// state reads a snapshot that was armed — i.e. believed valid — when its probe
/// began, and that is exactly as sound as the decision to arm it was.
///
/// # What makes it safe to consult DURING a concurrent phase (2026-09-21)
///
/// *Stages 1 and 2 of `handoff-i-arm-root-filter-concurrently.md`. Stage 3 —
/// actually moving the arm point — is **not** in this tree; see the handoff
/// `handoff-r-wire-the-concurrent-root-filter.md` for what is left.*
///
/// Three pieces, and none of them is sufficient alone:
///
/// 1. **A validity signal per source.** The three `types/` registries now carry
///    a contents generation bumped inside the write lock that mutates them
///    (`cratonvm_types::loader_pin::generation` and its two siblings), read
///    paired with the key set under the read lock so the pair cannot straddle a
///    mutation. [`ZMarkRootFilter::is_current`] is that check, cheap enough for
///    a per-drain-batch call.
/// 2. **A mark-end repair, because (1) is a false proof on its own.** A
///    generation check protects proofs taken *after* the mismatch. An owner
///    scanned early, which then gains a root, is already marked — nothing
///    re-visits it, and noticing that the generation moved does not un-hand-out
///    the `false` it was given. [`ZMarkRootFilter::late_owners`] is the
///    difference-set pass that repairs those, at the mark-end safepoint where
///    the tables are final.
/// 3. **A fail-closed answer where neither works.** An owner added *and then
///    removed* inside the cycle leaves nothing to re-push;
///    [`ZLateRoots::certifiable`] reports it and the caller must decline to
///    certify the mark set.
///
/// The overlay half is deliberately handled by (2) alone: `external_roots`'
/// generation versions the *provider table*, not the owner sets, so there is no
/// cheap mid-phase signal for it and the owner sets are compared directly at
/// mark end.
pub struct ZMarkRootFilter {
    /// The two snapshot banks. Exactly one is live at a time, or neither.
    banks: [Box<ZRootSnapshot>; 2],
    /// [`Z_ROOT_DISARMED`], or `bank_index + 1`.
    ///
    /// One word carries both "is anything published" and "which", so a reader
    /// makes one acquire load and cannot observe an armed flag paired with the
    /// wrong bank.
    state: AtomicUsize,
    /// The bank the next `rebuild` will fill. Touched only by `rebuild`, which
    /// is single-threaded by contract (once per collection, at a safepoint).
    next_bank: AtomicUsize,
    /// [`Z_ROOT_DISARMED`], or `bank_index + 1` for the bank whose snapshot
    /// this cycle's **proofs were taken against**.
    ///
    /// # Why this is not [`Self::state`]
    ///
    /// `state` answers "may a probe right now exclude anything", and
    /// [`Self::disarm`] clears it. This answers "whose `false` answers are
    /// already out there", and a disarm must NOT clear it: a filter that
    /// disarmed half way through the concurrent phase — which is exactly what
    /// [`Self::is_current`] asks a caller to do — has still handed out proofs
    /// from that bank for every object scanned before the disarm, and those are
    /// the proofs [`Self::late_owners`] repairs. Clearing it on disarm would
    /// make the repair silently empty in the one case it exists for.
    ///
    /// Reset to [`Z_ROOT_DISARMED`] by a `rebuild` that publishes nothing (the
    /// kill switch), so a stale bank can never be compared against.
    repair_bank: AtomicUsize,
}

impl Default for ZMarkRootFilter {
    fn default() -> Self {
        Self {
            banks: [ZRootSnapshot::empty(), ZRootSnapshot::empty()],
            state: AtomicUsize::new(Z_ROOT_DISARMED),
            next_bank: AtomicUsize::new(0),
            repair_bank: AtomicUsize::new(Z_ROOT_DISARMED),
        }
    }
}

impl std::fmt::Debug for ZMarkRootFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.load(Ordering::Relaxed);
        let armed = state != Z_ROOT_DISARMED;
        let bank: &ZRootSnapshot = &self.banks[state.wrapping_sub(1) & 1];
        f.debug_struct("ZMarkRootFilter")
            .field("armed", &armed)
            .field("state", &state)
            .field(
                "any_class",
                &(armed && bank.any_class.load(Ordering::Relaxed)),
            )
            .field(
                "any_addr",
                &(armed && bank.any_addr.load(Ordering::Relaxed)),
            )
            .field(
                "class_overflow",
                &(armed && bank.class_overflow.load(Ordering::Relaxed)),
            )
            .finish()
    }
}

impl ZMarkRootFilter {
    /// Snapshot the four tables' KEY SETS. Called once, at mark start, with
    /// the world stopped or the tables otherwise quiescent.
    ///
    /// `CRATONVM_ZGC_MARK_ROOT_FILTER=0` leaves it disarmed, which restores
    /// the per-object lookups exactly -- the arms differ only in whether a
    /// question is asked of a bloom or of four global tables, so the switch is
    /// a clean A/B rather than a different collector.
    ///
    /// # Concurrency (2026-09-21)
    ///
    /// **This may now be called with readers running**, which is what the
    /// concurrent configuration actually does — see the type docs. It fills the
    /// bank that is not live and publishes it with one release store; the other
    /// bank is not written at all, so a reader mid-probe on it is unaffected.
    ///
    /// Still single-threaded with respect to *itself*: two concurrent rebuilds
    /// would pick the same `next_bank` and interleave their fills. Every caller
    /// runs it once per collection, at a safepoint, which is where the
    /// underlying key sets are quiescent anyway.
    pub fn rebuild(&self) {
        // Pick the bank that is NOT live. `disarm` does not change it, so this
        // also holds when nothing is published: the live bank is the one the
        // previous rebuild filled, and a reader that raced past a disarm is
        // reading it.
        let idx = self.next_bank.load(Ordering::Relaxed) & 1;
        let bank: &ZRootSnapshot = &self.banks[idx];

        // DISARM FIRST, and this is still worth doing even though the fill
        // below no longer touches the live bank: it makes the window between
        // "the old snapshot stopped being current" and "the new one is
        // published" answer *maybe*, which is the slow path, rather than
        // answering from a snapshot that is one table-mutation out of date.
        self.state.store(Z_ROOT_DISARMED, Ordering::Release);
        if !filter_enabled() {
            // No snapshot is published, so no proof will be taken from one and
            // there is nothing for `late_owners` to repair. Say so, rather than
            // leaving the previous collection's bank nominated.
            self.repair_bank.store(Z_ROOT_DISARMED, Ordering::Release);
            return;
        }
        bank.clear();
        let mut keys = bank.key_sets.lock();

        // ---- the three `types/` tables, each read PAIRED with its version ---
        //
        // The paired readers take one read lock and read the key set and the
        // generation under it, so the pair is exact. Taking them separately is
        // the ordering that turns a validity signal into a false proof — see
        // `cratonvm_types::mirror_pin::pinned_owner_addrs_with_generation`.
        let (class_ids, class_gen) = cratonvm_types::loader_pin::pinned_class_ids_with_generation();
        let mut any_class = false;
        for class_id in &class_ids {
            any_class = true;
            let bit = *class_id as usize;
            match bank.class_bits.get(bit >> 6) {
                Some(word) => {
                    word.fetch_or(1u64 << (bit & 63), Ordering::Relaxed);
                }
                None => bank.class_overflow.store(true, Ordering::Relaxed),
            }
        }
        bank.any_class.store(any_class, Ordering::Relaxed);
        keys.class_ids = class_ids;
        keys.class_ids.sort_unstable();

        let mut any_addr = false;
        let mut insert = |addr: usize| {
            any_addr = true;
            let (w1, b1, w2, b2) = Self::slots(addr);
            bank.addr_bloom[w1].fetch_or(1u64 << b1, Ordering::Relaxed);
            bank.addr_bloom[w2].fetch_or(1u64 << b2, Ordering::Relaxed);
        };
        let (mirror_owners, mirror_gen) =
            cratonvm_types::mirror_pin::pinned_owner_addrs_with_generation();
        for addr in &mirror_owners {
            insert(*addr);
        }
        keys.mirror_owners = mirror_owners;
        keys.mirror_owners.sort_unstable();

        let (metadata_owners, metadata_gen) =
            cratonvm_types::metadata_pin::pinned_owner_addrs_with_generation();
        for owner in &metadata_owners {
            insert(*owner);
        }
        keys.metadata_owners = metadata_owners;
        keys.metadata_owners.sort_unstable();

        // ---- the external-root providers -----------------------------------
        //
        // `provider_gen` versions the PROVIDER TABLE, not the owner sets, and
        // is kept only so a provider appearing mid-cycle is noticed. The owner
        // set itself is retained because it is the only thing that can answer
        // "which owners are new" — `external_roots`' own `GENERATION` doc says
        // why it cannot.
        let (overlay_owners, overlay_complete, overlay_provider_gen) =
            cratonvm_gc_core::external_roots::owner_addrs_and_completeness_with_generation();
        for owner in &overlay_owners {
            insert(*owner);
        }
        keys.overlay_owners = overlay_owners;
        keys.overlay_complete = overlay_complete;

        bank.overlay_unfilterable
            .store(!overlay_complete, Ordering::Relaxed);
        bank.any_addr.store(any_addr, Ordering::Relaxed);
        bank.class_gen.store(class_gen, Ordering::Relaxed);
        bank.mirror_gen.store(mirror_gen, Ordering::Relaxed);
        bank.metadata_gen.store(metadata_gen, Ordering::Relaxed);
        bank.overlay_provider_gen
            .store(overlay_provider_gen, Ordering::Relaxed);
        drop(keys);

        // PUBLISH. Release pairs with the `Acquire` in `live`, so a reader that
        // observes this state observes every relaxed store above it. The bank
        // swap is the same store, so there is no window in which "armed" and
        // "which bank" disagree.
        self.state.store(idx + 1, Ordering::Release);
        // ...and nominate it as the bank whose proofs this cycle will have to
        // answer for. Ordered AFTER the publish for the same reason: a repair
        // aimed at a bank nothing has read yet is harmless, one aimed at the
        // wrong bank is not.
        self.repair_bank.store(idx + 1, Ordering::Release);
        self.next_bank.store(idx ^ 1, Ordering::Relaxed);
    }

    /// [`Self::rebuild`] for the snapshot a CONCURRENT phase's proofs will be
    /// taken against, also recording which `loader_pin` class ids had a row
    /// inside this heap's `arena` (zr z8/rv7), so the mark-end repair can tell
    /// a row THIS heap lost from one another VM in the process lost.
    ///
    /// Called at the mark-start safepoint, with this VM's mutators stopped:
    /// this VM's own rows cannot change between the two reads below. Another
    /// VM's can, and that does not matter -- a foreign row is never the row a
    /// proof about one of this heap's objects was taken against.
    pub fn rebuild_within(&self, arena: std::ops::Range<usize>) {
        self.rebuild();
        if arena.is_empty() || !zgc_root_filter_repair_own_arena() {
            return;
        }
        let nominated = self.repair_bank.load(Ordering::Acquire);
        if nominated == Z_ROOT_DISARMED {
            return;
        }
        let own = own_loader_pin_ids(&arena);
        self.banks[(nominated - 1) & 1]
            .key_sets
            .lock()
            .class_ids_own = Some(own);
    }

    /// Is the published snapshot still a proof?
    ///
    /// *Stage 2 of
    /// `docs/feature-designs/zgc-architecture.md`.*
    ///
    /// A `false` answer is not an error: the caller disarms, every question
    /// then answers "maybe", and the marker falls back to the real tables —
    /// exactly what it does today for the whole concurrent phase. What changes
    /// is that the fallback becomes an *event* rather than the steady state.
    ///
    /// # What it checks, and the one thing it cannot
    ///
    /// The three `types/` registries version their **contents**, so an
    /// unchanged generation there is a proof that their key sets are unchanged
    /// (see `cratonvm_types::loader_pin::GENERATION`'s contract). Those three
    /// are checked here, lock-free, as three loads against three stored words.
    ///
    /// `external_roots`' generation versions the **provider table**, not the
    /// owner sets: it moves when a subsystem registers a provider, and *not*
    /// when an already-registered provider gains an owner — which is the common
    /// case and happens inside the provider's own module, where this registry
    /// never sees it. So an unchanged value there proves nothing about the
    /// overlay half, and this function deliberately does not consult it.
    /// The overlay half is made sound the other way, by
    /// [`late_owners`](Self::late_owners)' difference pass at the mark-end
    /// safepoint, which compares the owner *sets* directly.
    ///
    /// # Why it is not sufficient on its own
    ///
    /// **A generation check protects proofs taken AFTER the mismatch and does
    /// nothing for the ones already taken.** An owner scanned early, which then
    /// gains a root, is not protected by noticing that the generation moved;
    /// the object is already marked, so nothing re-visits it. This is a bound
    /// on how much [`late_owners`](Self::late_owners) has to repair, not a
    /// replacement for it, and arming the filter concurrently on this check
    /// alone would be a false proof.
    ///
    /// Answers `true` when nothing is published: an unarmed filter excludes
    /// nothing, so there is nothing to invalidate.
    ///
    /// zt t1/mk (residual 12 of `zgc-zs2mc2-mark-engine-review-residuals`):
    /// `#[cfg(test)]`. No production caller exists -- the per-drain
    /// revalidation (stage 3c) was never wired, and the mark-end harvest
    /// compares the generations itself -- so the tests are its only users.
    /// Drop the attribute in the change that wires it.
    #[cfg(test)]
    #[inline]
    pub fn is_current(&self) -> bool {
        let Some(bank) = self.live() else {
            return true;
        };
        bank.class_gen.load(Ordering::Relaxed) == cratonvm_types::loader_pin::generation()
            && bank.mirror_gen.load(Ordering::Relaxed) == cratonvm_types::mirror_pin::generation()
            && bank.metadata_gen.load(Ordering::Relaxed)
                == cratonvm_types::metadata_pin::generation()
    }

    /// The owners present **now** that were absent from the snapshot this
    /// cycle's proofs were taken against, as addresses to re-push.
    ///
    /// *Stage 2 of
    /// `docs/feature-designs/zgc-architecture.md`,
    /// and the half that
    /// `gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`'s staging
    /// plan understates.*
    ///
    /// # The hole this closes, stated exactly
    ///
    /// [`is_current`](Self::is_current) protects proofs taken **after** a
    /// mismatch. It does nothing for the ones already taken: a loader `L`
    /// marked and scanned at `T` with the filter answering *"owns nothing"*,
    /// which then defines a class at `T+1`, has a mirror the marker will never
    /// visit — `L` is already marked, so nothing re-claims it and nothing
    /// re-scans it. Without this pass a generation check is a **false proof**,
    /// not merely an incomplete one.
    ///
    /// # What it does
    ///
    /// **Call at the mark-end safepoint, with the mutators stopped and the
    /// tables final, and push [`ZLateRoots::addrs`] before driving the mark to
    /// its fixed point.** For each of the four sources:
    ///
    /// * if its validity signal is unchanged, it contributes nothing, and that
    ///   is a proof rather than an optimisation;
    /// * otherwise every owner present now and absent from the snapshot
    ///   contributes the addresses it owns — the mirrors, the metadata, the
    ///   overlay roots, or (for `loader_pin`, which is keyed by class id) the
    ///   defining loader address itself.
    ///
    /// The difference is empty on almost every cycle, so this is a handful of
    /// comparisons per collection.
    ///
    /// Over-approximate in the safe direction: an owner that appeared mid-cycle
    /// contributes its roots whether or not the owner itself is reachable, so
    /// an unreachable late owner's values survive one extra cycle. That is
    /// floating garbage, which this collector already produces by the cycle
    /// under SATB.
    ///
    /// # Precondition R, and what happens when it is violated
    ///
    /// The difference set can only repair an owner that is **still an owner**
    /// at mark end. An owner added *and then removed* inside the cycle can have
    /// had a wrong `false` taken against it and leaves nothing behind to
    /// re-push, and no comparison of the two key sets can recover its values.
    ///
    /// So arming the filter concurrently rests on: **no row is removed from any
    /// of the four tables between `rebuild()` and `late_owners()`.** That holds
    /// by construction for a single-VM process — every remover in the tree
    /// (`remove_loader_pin`, the three `replace_*`, the three `forget_vm_*`,
    /// `set_metadata_weak_mode(false)`) runs at a collection safepoint or at VM
    /// teardown, and the mutator-side writers are all additions. A second VM
    /// in the same process tearing down or collecting during this VM's
    /// concurrent phase violates it.
    ///
    /// This function **detects** that violation — the snapshot's key set is no
    /// longer a subset of the current one — and reports
    /// [`ZLateRoots::certifiable`] `== false`. The caller must then decline to
    /// certify the mark set, which `finish_concurrent_mark` already knows how
    /// to do: fall back to a stop-the-world mark. Fail closed, because the
    /// alternative is a best-effort repair of a hole whose size is unknown.
    ///
    /// # Ordering contract
    ///
    /// Compares against the bank named by
    /// [`repair_bank`](Self::repair_bank) — the snapshot the cycle's proofs
    /// came from — so a mid-cycle [`disarm`](Self::disarm) does not make it
    /// silently empty. It must still be called **before the next
    /// [`rebuild`](Self::rebuild)**, which retires that bank's key sets.
    #[cfg(test)]
    pub fn late_owners(&self) -> ZLateRoots {
        self.late_owners_within(0..0)
    }

    /// [`late_owners`](Self::late_owners) for the heap whose arena envelope
    /// is `arena`. A `loader_pin` class id that several live VMs share
    /// resolves to the row inside `arena` (`loader_pin_addr_where`), i.e. this
    /// VM's own defining loader -- live heaps do not overlap. An empty range
    /// accepts nothing and so gives the VM-less (most recent writer) answer.
    pub fn late_owners_within(&self, arena: std::ops::Range<usize>) -> ZLateRoots {
        let nominated = self.repair_bank.load(Ordering::Acquire);
        if nominated == Z_ROOT_DISARMED {
            // Nothing was ever published, so no probe ever excluded anything.
            return ZLateRoots::nothing_to_repair();
        }
        let bank: &ZRootSnapshot = &self.banks[(nominated - 1) & 1];
        let snap = bank.key_sets.lock();

        let mut addrs: Vec<u64> = Vec::new();
        let mut sources_moved = 0u32;
        // zs s2/mc2: which source refused; `certifiable` is `refused == 0`.
        let mut refused: u8 = 0;

        // zr z8/rv7 (`CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA`, default on):
        // judge the REMOVALS by this heap's own rows only. The four tables are
        // process-wide, and another VM tearing down or collecting during this
        // VM's phase removes and re-keys ITS rows; counting those declined to
        // certify a sound mark set (a stop-the-world re-mark) on every such
        // overlap. A proof is only ever taken about an object this heap's
        // marker scans, i.e. an owner inside `arena`, so only a removal there
        // can strand one. Empty `arena` (or `=0`): every row counts, as before.
        //
        // zt t1/mk (residual 4 of `zgc-zs2mc2-mark-engine-review-residuals`):
        // the same test now screens the ADDITIONS. A foreign row's loader, a
        // foreign owner's mirrors / metadata and an overlay root outside the
        // arena were pushed and then refused one by one by `claim_child`
        // (`OffHeap`); they are counted in `screened_foreign` instead. Same
        // kill switch: `=0` (or an empty arena) pushes every addition again.
        let own_only = !arena.is_empty() && zgc_root_filter_repair_own_arena();
        let ours = |a: usize| !own_only || arena.contains(&a);
        let mut screened_foreign = 0usize;
        // Per-source counts of snapshot keys missing now, for the refusal
        // diagnostic below; zero for a source whose generation did not move.
        let mut missing_loader_classes = 0usize;
        let mut missing_mirror_owners = 0usize;
        let mut missing_metadata_owners = 0usize;

        // zt t1/mk (residual 3 of the same page): each source's generation is
        // compared BEFORE its key set is cloned. The counters only ever
        // `fetch_add`, and a bank stores the generation its key set was read
        // at, so an equal value here means no mutation since the snapshot --
        // what the locked pair read reported -- and the clone, sort and walk
        // are skipped. A moved generation still takes the locked
        // `*_with_generation` read, as before.

        // ---- loader_pin: keyed by CLASS ID, so the value is the loader ------
        if cratonvm_types::loader_pin::generation() != bank.class_gen.load(Ordering::Relaxed) {
            let (mut class_ids_now, _class_gen_now) =
                cratonvm_types::loader_pin::pinned_class_ids_with_generation();
            class_ids_now.sort_unstable();
            sources_moved += 1;
            let mut still_present = 0usize;
            for id in &class_ids_now {
                if snap.class_ids.binary_search(id).is_ok() {
                    still_present += 1;
                } else if let Some(loader) =
                    cratonvm_types::loader_pin::loader_pin_addr_where(*id, |a| arena.contains(&a))
                {
                    if ours(loader) {
                        addrs.push(loader as u64);
                    } else {
                        screened_foreign += 1;
                    }
                }
            }
            let ok = match (&snap.class_ids_own, own_only) {
                (Some(own_then), true) => {
                    let own_now = own_loader_pin_ids(&arena);
                    missing_loader_classes = own_then
                        .iter()
                        .filter(|id| own_now.binary_search(id).is_err())
                        .count();
                    missing_loader_classes == 0
                }
                _ => {
                    missing_loader_classes = snap
                        .class_ids
                        .iter()
                        .filter(|id| class_ids_now.binary_search(id).is_err())
                        .count();
                    still_present == snap.class_ids.len()
                }
            };
            if !ok {
                refused |= Z_LATE_REFUSED_LOADER;
            }
        }

        // ---- mirror_pin: keyed by owner ADDRESS -----------------------------
        if cratonvm_types::mirror_pin::generation() != bank.mirror_gen.load(Ordering::Relaxed) {
            let (mut mirror_owners_now, _mirror_gen_now) =
                cratonvm_types::mirror_pin::pinned_owner_addrs_with_generation();
            // Registry key sets have hash-map order. The removal checks below
            // binary-search the current set as well as the sorted snapshot.
            mirror_owners_now.sort_unstable();
            sources_moved += 1;
            let mut still_present = 0usize;
            for owner in &mirror_owners_now {
                if snap.mirror_owners.binary_search(owner).is_ok() {
                    still_present += 1;
                } else if !ours(*owner) {
                    screened_foreign += 1;
                } else if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(*owner)
                {
                    addrs.extend(mirrors.into_iter().map(|m| m as u64));
                }
            }
            missing_mirror_owners = snap
                .mirror_owners
                .iter()
                .filter(|o| ours(**o) && mirror_owners_now.binary_search(o).is_err())
                .count();
            let ok = if own_only {
                missing_mirror_owners == 0
            } else {
                still_present == snap.mirror_owners.len()
            };
            if !ok {
                refused |= Z_LATE_REFUSED_MIRROR;
            }
        }

        // ---- metadata_pin: keyed by owner ADDRESS ---------------------------
        if cratonvm_types::metadata_pin::generation() != bank.metadata_gen.load(Ordering::Relaxed) {
            let (mut metadata_owners_now, _metadata_gen_now) =
                cratonvm_types::metadata_pin::pinned_owner_addrs_with_generation();
            metadata_owners_now.sort_unstable();
            sources_moved += 1;
            let mut still_present = 0usize;
            for owner in &metadata_owners_now {
                if snap.metadata_owners.binary_search(owner).is_ok() {
                    still_present += 1;
                } else if !ours(*owner) {
                    screened_foreign += 1;
                } else if let Some(roots) = cratonvm_types::metadata_pin::roots_for_loader(*owner) {
                    addrs.extend(roots.into_iter().map(|m| m as u64));
                }
            }
            missing_metadata_owners = snap
                .metadata_owners
                .iter()
                .filter(|o| ours(**o) && metadata_owners_now.binary_search(o).is_err())
                .count();
            let ok = if own_only {
                missing_metadata_owners == 0
            } else {
                still_present == snap.metadata_owners.len()
            };
            if !ok {
                refused |= Z_LATE_REFUSED_METADATA;
            }
        }

        // ---- the overlay providers, compared SET AGAINST SET ----------------
        //
        // Not by generation: `external_roots`' counter versions the provider
        // table, and an already-registered provider gaining an owner — the
        // common case, and the one this filter is mostly about — does not move
        // it. Its own doc says so. The owner set is the only honest comparison.
        let (overlay_owners_now, overlay_complete_now, overlay_provider_gen_now) =
            cratonvm_gc_core::external_roots::owner_addrs_and_completeness_with_generation();
        let overlay_added: Vec<usize> = overlay_owners_now
            .difference(&snap.overlay_owners)
            .copied()
            .collect();
        // zt t1/mk (residual 3): one pass over the snapshot's owners answers
        // both removal questions (any removal moves the source; a removal of
        // THIS heap's owner refuses), where there were two `any` scans.
        let mut overlay_removed_any = false;
        let mut overlay_removed = false;
        for owner in snap.overlay_owners.iter() {
            if !overlay_owners_now.contains(owner) {
                overlay_removed_any = true;
                if ours(*owner) {
                    overlay_removed = true;
                    break;
                }
            }
        }
        if !overlay_added.is_empty()
            || overlay_removed_any
            || overlay_complete_now != snap.overlay_complete
            || overlay_provider_gen_now != bank.overlay_provider_gen.load(Ordering::Relaxed)
        {
            sources_moved += 1;
        }
        if !overlay_added.is_empty() {
            let added: HashSet<usize> = overlay_added.into_iter().collect();
            let is_new = |owner: usize| added.contains(&owner);
            for root in cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&is_new) {
                let root = root.as_ptr() as usize;
                if ours(root) {
                    addrs.push(root as u64);
                } else {
                    screened_foreign += 1;
                }
            }
        }
        if overlay_removed {
            refused |= Z_LATE_REFUSED_OVERLAY_REMOVED;
        }
        // A provider that could enumerate its owners at mark start and cannot
        // now is unrepairable by construction: the snapshot excluded addresses
        // on its behalf and it will not say which. `overlay_unfilterable`
        // handles the provider that declined from the start; this is the one
        // that changed its mind mid-cycle.
        if snap.overlay_complete && !overlay_complete_now {
            refused |= Z_LATE_REFUSED_OVERLAY_INCOMPLETE;
        }

        if refused != 0 {
            // Identify the registry that invalidated the concurrent proof. A
            // count of changed registries alone cannot distinguish a lost
            // metadata snapshot from an overlay owner removed by its provider.
            // zt t1/mk: each count is taken inside its source's arm, and the
            // loader count is the one the refusal was judged by (this heap's
            // own ids under the own-arena rule; it counted every id before).
            tracing::error!(
                target: "zgc",
                missing_loader_classes,
                missing_mirror_owners,
                missing_metadata_owners,
                overlay_removed,
                overlay_became_incomplete = snap.overlay_complete && !overlay_complete_now,
                "zgc concurrent root-filter certification failed"
            );
        }

        ZLateRoots {
            addrs,
            certifiable: refused == 0,
            sources_moved,
            refused,
            screened_foreign,
        }
    }

    /// Disarm. Every question then answers "maybe", which is the behaviour
    /// that existed before this filter did.
    ///
    /// Leaves the published bank's contents alone: a reader that raced past
    /// this store keeps reading a complete snapshot rather than a cleared one.
    ///
    /// It also leaves [`Self::repair_bank`] alone, which is the load-bearing
    /// half once the filter is armed across a concurrent phase: a disarm
    /// triggered mid-phase by [`Self::is_current`] stops NEW proofs being
    /// taken and does nothing about the ones already handed out, and those are
    /// exactly what [`Self::late_owners`] has to repair at mark end.
    pub fn disarm(&self) {
        self.state.store(Z_ROOT_DISARMED, Ordering::Release);
    }

    /// The published snapshot, or `None` when nothing is published.
    ///
    /// **One acquire load and one branch — the same shape the `armed` flag
    /// had.** The bank is then a computed index into an inline array of two
    /// pointers, so the load of the bitmap word is no more dependent than it
    /// was when there was one `Box` per bitmap.
    #[inline]
    fn live(&self) -> Option<&ZRootSnapshot> {
        match self.state.load(Ordering::Acquire) {
            Z_ROOT_DISARMED => None,
            // `& 1` rather than an index: `state` is only ever written by
            // `rebuild` as `idx + 1` with `idx` already masked, so this cannot
            // be out of range — and masking makes that true by construction
            // instead of by inspection of the writer.
            n => Some(&*self.banks[(n - 1) & 1]),
        }
    }

    /// Could the class at `class_id` have a pinned defining loader?
    ///
    /// `false` is a proof; `true` means ask the real table.
    #[inline]
    pub fn may_pin_loader(&self, class_id: u32) -> bool {
        let Some(bank) = self.live() else {
            return true;
        };
        if !bank.any_class.load(Ordering::Relaxed) {
            return false;
        }
        let bit = class_id as usize;
        match bank.class_bits.get(bit >> 6) {
            Some(word) => word.load(Ordering::Relaxed) & (1u64 << (bit & 63)) != 0,
            None => bank.class_overflow.load(Ordering::Relaxed),
        }
    }

    /// Could the object at `addr` own mirrors, metadata or a native overlay?
    ///
    /// `false` is a proof; `true` means ask the three real tables.
    ///
    /// The single-question form of [`extra_root_probe`](Self::extra_root_probe),
    /// which is what `zgc.rs` actually calls (both answers come off one bloom
    /// probe there). Kept because it is the honest name for the question and
    /// because the tests below state the soundness properties in terms of it.
    /// Test-only since zt t1/mk (residual 12): every production probe is
    /// [`extra_root_probe`](Self::extra_root_probe).
    #[cfg(test)]
    #[inline]
    pub fn may_own_extra_roots(&self, addr: usize) -> bool {
        self.extra_root_probe(addr).0
    }

    /// Could the object at `addr` own a native-collection overlay?
    ///
    /// Separate from [`Self::may_own_extra_roots`] because the overlay
    /// providers are the one source that can decline to enumerate its owners.
    /// When any of them does, this answers `true` for everything and the
    /// per-object lookup runs exactly as it did before the filter existed --
    /// a provider that will not say which objects own roots may own any of
    /// them, and excluding one on its behalf would drop a live edge.
    /// Test-only since zt t1/mk (residual 12), like
    /// [`may_own_extra_roots`](Self::may_own_extra_roots).
    #[cfg(test)]
    #[inline]
    pub fn may_own_overlay(&self, addr: usize) -> bool {
        self.extra_root_probe(addr).1
    }

    /// Both address-keyed answers from **one** bloom probe.
    ///
    /// # Why this exists (2026-09-20)
    ///
    /// The two callers in `zgc.rs` — `visit_pin_edges` and `visit_overlay_edges`
    /// — are invoked back to back for the same object, and each of them called
    /// [`may_own_extra_roots`](Self::may_own_extra_roots) (the second by way of
    /// [`may_own_overlay`](Self::may_own_overlay)). So every marked object paid
    /// the splitmix64 mixing, the two word loads and the state acquire-load
    /// **twice**, for an answer that cannot have changed in between: the filter
    /// is rebuilt once per collection and is read-only for the whole of it.
    ///
    /// This is the entire per-object cost of the filter, doubled. Halving it is
    /// not a large absolute number, but the filter exists precisely because
    /// small per-object costs are the mark pause — see this module's header
    /// measurement, where removing four global lookups per object halved
    /// `mark_us`.
    ///
    /// The two single-question forms are now *this* function, rather than the
    /// other way round (2026-09-21), so there is exactly one place where the
    /// snapshot is read and the three answers cannot drift apart.
    ///
    /// Returns `(may_own_pins, may_own_overlay)`. Both are "maybe"; `false` is
    /// the proof.
    #[inline]
    pub fn extra_root_probe(&self, addr: usize) -> (bool, bool) {
        let Some(bank) = self.live() else {
            return (true, true);
        };
        // An unfilterable provider only forces the OVERLAY answer. The mirror
        // and metadata key sets are exact whatever the providers do, so the
        // first element of the pair is unaffected — collapsing the two would
        // give up the filter for tables that can still prove a negative.
        let overlay_forced = bank.overlay_unfilterable.load(Ordering::Relaxed);
        if !bank.any_addr.load(Ordering::Relaxed) {
            return (false, overlay_forced);
        }
        let (w1, b1, w2, b2) = Self::slots(addr);
        let hit = bank.addr_bloom[w1].load(Ordering::Relaxed) & (1u64 << b1) != 0
            && bank.addr_bloom[w2].load(Ordering::Relaxed) & (1u64 << b2) != 0;
        (hit, hit || overlay_forced)
    }

    /// The live snapshot, for this module's tests only.
    ///
    /// Tests need to poke `any_addr` / `class_overflow` / `overlay_unfilterable`
    /// to simulate states that cannot be produced from inside one process (a
    /// provider that declines to enumerate its owners is registered globally and
    /// permanently). Returns `None` when nothing is published, which is itself
    /// the thing several of them assert.
    #[cfg(test)]
    fn live_for_test(&self) -> Option<&ZRootSnapshot> {
        self.live()
    }

    /// Two independent positions from one splitmix64 pass over the address's
    /// 8-byte grid index. Identical construction to `ZgcRealHeap::skip_bloom_slots`.
    #[inline]
    fn slots(addr: usize) -> (usize, u32, usize, u32) {
        let mut x = (addr >> 3) as u64;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        x ^= x >> 33;
        let bits = (Z_ROOT_BLOOM_WORDS * 64) as u64;
        let h1 = x % bits;
        let h2 = (x >> 32) % bits;
        (
            (h1 / 64) as usize,
            (h1 % 64) as u32,
            (h2 / 64) as usize,
            (h2 % 64) as u32,
        )
    }
}

#[cfg(test)]
mod tests;
