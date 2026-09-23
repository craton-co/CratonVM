// SPDX-License-Identifier: Apache-2.0
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
//! costs an `RwLock` read plus a hash lookup that overwhelmingly returns
//! nothing. Ten million marked objects is forty million lock-and-hash
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
//! marked object takes the provider `RwLock`, enters the callback, and takes
//! that module's own `Mutex` over the owner index. That is the per-object cost
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
//!   status quo; a false negative would drop a live edge, and the bloom cannot
//!   produce one because every owner address is inserted before the mark
//!   begins and no owner is added during it (the tables are written by class
//!   definition and by native registration, both of which are mutator work).
//!
//! That last clause is why the filter is armed **inside the collection pause
//! only**. Holding it across a concurrent phase, where mutators run and can
//! register an owner the snapshot has never seen, needs three more things —
//! a per-source validity generation, a mark-end difference pass, and a
//! fail-closed answer for the case neither covers. All three are implemented
//! here ([`ZMarkRootFilter::is_current`], [`ZMarkRootFilter::late_owners`],
//! [`ZLateRoots::certifiable`]); none of them is **wired**, and the arm point
//! has not moved. See the type docs on [`ZMarkRootFilter`] and
//! `docs/internal/zgc-round-20260920/handoff-r-wire-the-concurrent-root-filter.md`.
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
pub(crate) const Z_ROOT_BLOOM_WORDS: usize = 512;

/// 1,024 words = 65,536 class ids, exactly. Beyond that the overflow latch
/// sends every lookup down the slow path, which is correct and merely slower.
pub(crate) const Z_ROOT_CLASS_WORDS: usize = 1024;

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
pub(crate) fn filter_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_MARK_ROOT_FILTER") {
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
}

impl ZRootKeySets {
    fn clear(&mut self) {
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
pub(crate) struct ZLateRoots {
    /// Addresses that must be pushed through `ZMarkCoordinator::push_roots`
    /// **before** the mark-end fixed point, because the filter may have
    /// excluded their owner while the owner was not yet in the snapshot.
    ///
    /// Empty on every cycle in which nothing registered — which is almost all
    /// of them.
    pub(crate) addrs: Vec<u64>,
    /// **`false` means this mark set must not be certified.**
    ///
    /// A row that was in the snapshot and is gone now breaks the one
    /// precondition the repair rests on (see the function docs): an owner that
    /// was added *and then removed* inside the cycle can have had a wrong
    /// `false` taken against it and leaves nothing behind to re-push. The safe
    /// response is the one `finish_concurrent_mark` already has for an
    /// uncertified mark set — fall back to a stop-the-world mark — not a
    /// best-effort repair.
    pub(crate) certifiable: bool,
    /// How many of the four sources changed during the cycle. Telemetry: it
    /// distinguishes "the repair was empty because nothing happened" from "the
    /// repair was empty because the difference was empty", which are the same
    /// `addrs.len() == 0` and very different facts about a workload.
    pub(crate) sources_moved: u32,
}

impl ZLateRoots {
    /// Nothing was proved from a snapshot, so there is nothing to repair.
    fn nothing_to_repair() -> Self {
        ZLateRoots {
            addrs: Vec::new(),
            certifiable: true,
            sources_moved: 0,
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
/// `docs/internal/zgc-round-20260920/gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`.*
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
pub(crate) struct ZMarkRootFilter {
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
    pub(crate) fn rebuild(&self) {
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
            crate::external_roots::owner_addrs_and_completeness_with_generation();
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

    /// Is the published snapshot still a proof?
    ///
    /// *Stage 2 of
    /// `docs/internal/zgc-round-20260920/handoff-i-arm-root-filter-concurrently.md`.*
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
    #[inline]
    pub(crate) fn is_current(&self) -> bool {
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
    /// `docs/internal/zgc-round-20260920/handoff-i-arm-root-filter-concurrently.md`,
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
    pub(crate) fn late_owners(&self) -> ZLateRoots {
        let nominated = self.repair_bank.load(Ordering::Acquire);
        if nominated == Z_ROOT_DISARMED {
            // Nothing was ever published, so no probe ever excluded anything.
            return ZLateRoots::nothing_to_repair();
        }
        let bank: &ZRootSnapshot = &self.banks[(nominated - 1) & 1];
        let snap = bank.key_sets.lock();

        let mut addrs: Vec<u64> = Vec::new();
        let mut sources_moved = 0u32;
        let mut certifiable = true;

        // ---- loader_pin: keyed by CLASS ID, so the value is the loader ------
        let (class_ids_now, class_gen_now) =
            cratonvm_types::loader_pin::pinned_class_ids_with_generation();
        if class_gen_now != bank.class_gen.load(Ordering::Relaxed) {
            sources_moved += 1;
            let mut still_present = 0usize;
            for id in &class_ids_now {
                if snap.class_ids.binary_search(id).is_ok() {
                    still_present += 1;
                } else if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(*id) {
                    addrs.push(loader as u64);
                }
            }
            certifiable &= still_present == snap.class_ids.len();
        }

        // ---- mirror_pin: keyed by owner ADDRESS -----------------------------
        let (mirror_owners_now, mirror_gen_now) =
            cratonvm_types::mirror_pin::pinned_owner_addrs_with_generation();
        if mirror_gen_now != bank.mirror_gen.load(Ordering::Relaxed) {
            sources_moved += 1;
            let mut still_present = 0usize;
            for owner in &mirror_owners_now {
                if snap.mirror_owners.binary_search(owner).is_ok() {
                    still_present += 1;
                } else if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(*owner)
                {
                    addrs.extend(mirrors.into_iter().map(|m| m as u64));
                }
            }
            certifiable &= still_present == snap.mirror_owners.len();
        }

        // ---- metadata_pin: keyed by owner ADDRESS ---------------------------
        let (metadata_owners_now, metadata_gen_now) =
            cratonvm_types::metadata_pin::pinned_owner_addrs_with_generation();
        if metadata_gen_now != bank.metadata_gen.load(Ordering::Relaxed) {
            sources_moved += 1;
            let mut still_present = 0usize;
            for owner in &metadata_owners_now {
                if snap.metadata_owners.binary_search(owner).is_ok() {
                    still_present += 1;
                } else if let Some(roots) = cratonvm_types::metadata_pin::roots_for_loader(*owner) {
                    addrs.extend(roots.into_iter().map(|m| m as u64));
                }
            }
            certifiable &= still_present == snap.metadata_owners.len();
        }

        // ---- the overlay providers, compared SET AGAINST SET ----------------
        //
        // Not by generation: `external_roots`' counter versions the provider
        // table, and an already-registered provider gaining an owner — the
        // common case, and the one this filter is mostly about — does not move
        // it. Its own doc says so. The owner set is the only honest comparison.
        let (overlay_owners_now, overlay_complete_now, overlay_provider_gen_now) =
            crate::external_roots::owner_addrs_and_completeness_with_generation();
        let overlay_added: Vec<usize> = overlay_owners_now
            .difference(&snap.overlay_owners)
            .copied()
            .collect();
        let overlay_removed = snap
            .overlay_owners
            .iter()
            .any(|owner| !overlay_owners_now.contains(owner));
        if !overlay_added.is_empty()
            || overlay_removed
            || overlay_complete_now != snap.overlay_complete
            || overlay_provider_gen_now != bank.overlay_provider_gen.load(Ordering::Relaxed)
        {
            sources_moved += 1;
        }
        if !overlay_added.is_empty() {
            let added: HashSet<usize> = overlay_added.into_iter().collect();
            let is_new = |owner: usize| added.contains(&owner);
            for root in crate::external_roots::external_roots_for_matching_owners(&is_new) {
                addrs.push(root.as_ptr() as u64);
            }
        }
        certifiable &= !overlay_removed;
        // A provider that could enumerate its owners at mark start and cannot
        // now is unrepairable by construction: the snapshot excluded addresses
        // on its behalf and it will not say which. `overlay_unfilterable`
        // handles the provider that declined from the start; this is the one
        // that changed its mind mid-cycle.
        certifiable &= !(snap.overlay_complete && !overlay_complete_now);

        ZLateRoots {
            addrs,
            certifiable,
            sources_moved,
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
    pub(crate) fn disarm(&self) {
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
    pub(crate) fn may_pin_loader(&self, class_id: u32) -> bool {
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
    #[inline]
    pub(crate) fn may_own_extra_roots(&self, addr: usize) -> bool {
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
    #[inline]
    pub(crate) fn may_own_overlay(&self, addr: usize) -> bool {
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
    pub(crate) fn extra_root_probe(&self, addr: usize) -> (bool, bool) {
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
mod tests {
    use super::*;

    /// The filter must never answer "definitely not" for an owner in the
    /// coherent snapshot it has published -- a false negative drops a live
    /// edge and the object it names is swept while reachable.
    ///
    /// Do not read the process-global registries after `rebuild` here. Mutator
    /// tests may register an owner between the snapshot and that read; that is
    /// precisely the concurrent mutation this filter reports through
    /// `is_current` / `late_owners`, not an assertion the old snapshot can
    /// satisfy.
    #[test]
    fn a_snapshotted_owner_is_never_rejected() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        let Some(bank) = filter.live_for_test() else {
            return; // the kill switch is set in this process
        };
        let keys = bank.key_sets.lock();
        for addr in &keys.mirror_owners {
            assert!(
                filter.may_own_extra_roots(*addr),
                "snapshotted mirror owner {addr:#x} rejected"
            );
        }
        for owner in &keys.metadata_owners {
            assert!(
                filter.may_own_extra_roots(*owner),
                "snapshotted metadata owner {owner:#x} rejected"
            );
        }
        for owner in &keys.overlay_owners {
            assert!(
                filter.may_own_extra_roots(*owner),
                "snapshotted overlay owner {owner:#x} rejected"
            );
        }
        for class_id in &keys.class_ids {
            assert!(
                filter.may_pin_loader(*class_id),
                "snapshotted class {class_id} rejected"
            );
        }
    }

    /// A provider that will not enumerate its owners must disable overlay
    /// filtering ENTIRELY, for every address.
    ///
    /// This is the soundness case, and it is not hypothetical: `zgc.rs`'s own
    /// test provider returns `None` from `owner_addrs` while serving roots
    /// from `roots_for_owner`. An address bloom built from an index that
    /// provider is absent from would answer "definitely not" for the very
    /// object it owns roots for, and the overlay would be swept while its
    /// owner survived.
    #[test]
    fn a_provider_that_will_not_name_its_owners_disables_the_overlay_filter() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        let Some(bank) = filter.live_for_test() else {
            return; // the kill switch is set in this process; nothing to assert
        };
        // Simulate the decline rather than registering a provider: provider
        // registration is process-global and permanent, so a test that did it
        // for real would change every later test in this binary.
        bank.overlay_unfilterable.store(true, Ordering::Relaxed);
        for addr in [0x1000usize, 0x2000, 0xdead_beef_0000] {
            assert!(
                filter.may_own_overlay(addr),
                "{addr:#x} was excluded from the overlay lookup while a provider \
                 had declined to enumerate its owners"
            );
        }
        // The other two tables are unaffected: their key sets are exact.
        bank.overlay_unfilterable.store(false, Ordering::Relaxed);
        if !bank.any_addr.load(Ordering::Relaxed) {
            assert!(!filter.may_own_overlay(0x1000));
        }
    }

    /// The switch must leave the filter DISARMED, not merely empty. An empty
    /// armed filter answers "definitely not" to everything, which is the
    /// opposite of the fallback it is supposed to restore.
    #[test]
    fn the_kill_switch_disarms_rather_than_emptying() {
        let filter = ZMarkRootFilter::default();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_MARK_ROOT_FILTER", Some("0"))],
            || {
                assert!(!filter_enabled());
                filter.rebuild();
            },
        );
        assert!(
            filter.may_own_extra_roots(0x4000) && filter.may_pin_loader(3),
            "the switched-off filter excluded something; it must prove nothing"
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_MARK_ROOT_FILTER", Some("1"))],
            || {
                assert!(filter_enabled());
                filter.rebuild();
            },
        );
    }

    /// An unbuilt filter must answer "maybe" to everything: a caller that
    /// forgets to arm it gets the old cost, not a dropped edge.
    #[test]
    fn an_unarmed_filter_proves_nothing() {
        let filter = ZMarkRootFilter::default();
        assert!(filter.may_own_extra_roots(0xdead_0000));
        assert!(filter.may_pin_loader(7));
        assert!(filter.may_pin_loader(u32::MAX));
        filter.rebuild();
        filter.disarm();
        assert!(filter.may_own_extra_roots(0xdead_0000));
        assert!(filter.may_pin_loader(7));
    }

    /// A class id past the bitmap must fall back rather than answer from a
    /// bit it does not have.
    #[test]
    fn a_class_id_past_the_bitmap_takes_the_slow_path() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        let Some(bank) = filter.live_for_test() else {
            return; // the kill switch is set in this process
        };
        bank.any_class.store(true, Ordering::Relaxed);
        let past = (Z_ROOT_CLASS_WORDS * 64) as u32;
        // No overflow recorded: nothing that large is registered, so the
        // bitmap's "definitely not" still stands.
        assert!(!filter.may_pin_loader(past));
        bank.class_overflow.store(true, Ordering::Relaxed);
        assert!(
            filter.may_pin_loader(past),
            "with a class id past the bitmap registered, every out-of-range id \
             must take the real lookup"
        );
    }

    /// The empty case is the common one and must cost nothing: with no table
    /// written, every question is answered by one flag.
    #[test]
    fn an_empty_process_rejects_every_address() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        let Some(bank) = filter.live_for_test() else {
            return; // the kill switch is set in this process
        };
        if bank.any_addr.load(Ordering::Relaxed) {
            return; // another test in this process registered something
        }
        for addr in [0x1000usize, 0x2000, 0xffff_0000] {
            assert!(!filter.may_own_extra_roots(addr));
        }
    }

    /// A rebuild must write the bank that is **not** published, so a reader
    /// that raced past the disarm keeps reading a complete snapshot.
    ///
    /// This is the whole of the 2026-09-21 change, and it is not decoration:
    /// `zgc.rs`'s `collect_garbage` calls `rebuild()` *before*
    /// `finish_concurrent_mark`, i.e. while the concurrent mark workers are
    /// still live and still calling `extra_root_probe`. With one bank in place,
    /// the clearing loop is visible to a reader that has already executed its
    /// `armed` load — and this filter's `false` is a proof, so what that reader
    /// gets is not a slower answer but a dropped live edge.
    ///
    /// A single-bank regression is caught by the middle assertion: it would
    /// find the marker word cleared.
    #[test]
    fn a_rebuild_writes_the_other_bank_and_leaves_the_published_one_intact() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        let first = filter.state.load(Ordering::Relaxed);
        if first == Z_ROOT_DISARMED {
            return; // the kill switch is set in this process
        }

        // A word only this bank has, standing in for the snapshot a racing
        // reader is in the middle of consulting.
        const MARKER: u64 = 0xDEAD_BEEF_CAFE_F00D;
        filter.live_for_test().expect("just armed").addr_bloom[0].store(MARKER, Ordering::Relaxed);

        filter.rebuild();
        let second = filter.state.load(Ordering::Relaxed);
        assert_ne!(
            second, first,
            "a rebuild must publish the OTHER bank; publishing the same one is \
             the single-bank arrangement with extra steps"
        );
        assert_eq!(
            filter.banks[(first - 1) & 1].addr_bloom[0].load(Ordering::Relaxed),
            MARKER,
            "the rebuild cleared the bank a concurrent reader could still be \
             reading — that reader's `false` is a proof, and it is now wrong"
        );

        // Two banks, alternating, so the arrangement is bounded rather than a
        // growing list of snapshots nobody frees.
        filter.rebuild();
        assert_eq!(filter.state.load(Ordering::Relaxed), first);
    }

    /// The three read entry points must never disagree: they are one probe of
    /// one snapshot, and `zgc.rs` uses the pair form while the tests here state
    /// the properties in terms of the single-question forms.
    #[test]
    fn the_three_read_entry_points_agree() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        for addr in [0usize, 0x1000, 0x2000, 0xdead_beef_0000, usize::MAX & !7] {
            let (pins, overlay) = filter.extra_root_probe(addr);
            assert_eq!(filter.may_own_extra_roots(addr), pins, "addr={addr:#x}");
            assert_eq!(filter.may_own_overlay(addr), overlay, "addr={addr:#x}");
        }
        filter.disarm();
        for addr in [0x1000usize, 0x2000] {
            assert_eq!(filter.extra_root_probe(addr), (true, true));
            assert!(filter.may_own_extra_roots(addr));
            assert!(filter.may_own_overlay(addr));
        }
    }

    // ---- stage 1 + stage 2: the concurrent validity machinery --------------
    //
    // `handoff-i-arm-root-filter-concurrently.md`. These tests write the
    // PROCESS-GLOBAL pin registries, so they take a lock against each other;
    // nothing else in the `gc` crate writes them, and the `cratonvm-types`
    // unit tests are a different binary.

    static REGISTRY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// A VM identity and an address range no other test in this crate uses.
    const R_VM: usize = 0x00F0_0D01;
    const R_LOADER: usize = 0x0070_0000;
    const R_MIRROR: usize = 0x0070_0080;

    /// An unarmed filter proved nothing, so there is nothing to repair and
    /// nothing that could have gone stale.
    ///
    /// This is the shape every non-concurrent caller sees, and it must cost a
    /// load and a branch rather than four table walks.
    #[test]
    fn an_unarmed_filter_has_nothing_to_repair() {
        let filter = ZMarkRootFilter::default();
        assert!(
            filter.is_current(),
            "nothing published, nothing to invalidate"
        );
        let late = filter.late_owners();
        assert!(late.addrs.is_empty());
        assert!(late.certifiable);
        assert_eq!(late.sources_moved, 0);
    }

    /// **The hole the generation check does not close, and the pass that does.**
    ///
    /// A loader that is absent from the snapshot is *proved* to own no mirrors,
    /// and the marker skips its mirror edge. If that loader then defines a class
    /// while the concurrent phase runs, the mirror is one the marker will never
    /// visit: the loader is already marked, so nothing re-claims it and nothing
    /// re-scans it. Noticing afterwards that the generation moved does not
    /// un-hand-out the proof.
    ///
    /// `late_owners` is what makes the armed filter sound rather than merely
    /// fast, and this test states that in the only terms that matter: the
    /// mirror address must come back out.
    #[test]
    fn an_owner_registered_after_the_snapshot_comes_back_from_late_owners() {
        let _g = REGISTRY_LOCK.lock();
        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        if filter.live_for_test().is_none() {
            return; // the kill switch is set in this process
        }
        let proved_absent = !filter.may_own_extra_roots(R_LOADER);

        cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);

        assert!(
            !filter.is_current(),
            "a registration must invalidate the snapshot; without this the \
             per-drain-batch check is blind and the filter keeps proving a \
             negative that stopped being true"
        );
        if proved_absent {
            assert!(
                !filter.may_own_extra_roots(R_LOADER),
                "the snapshot is a snapshot -- it still answers the stale \
                 `false`, which is exactly why the mark-end repair exists"
            );
        }

        let late = filter.late_owners();
        assert!(
            late.addrs.contains(&(R_MIRROR as u64)),
            "the mirror of a loader registered mid-cycle must be re-pushed; \
             got {:#x?}",
            late.addrs
        );
        assert!(late.sources_moved >= 1);

        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
    }

    /// **A disarm mid-cycle must not make the repair silently empty.**
    ///
    /// `is_current() == false` tells a caller to disarm, and that is the whole
    /// point of the check: stop handing out proofs. It says nothing about the
    /// proofs already handed out, and those are precisely what has to be
    /// repaired. If `late_owners` keyed off `state` rather than `repair_bank`,
    /// the one path that most needs the repair -- the cycle where something
    /// really did register -- is the one where it would return nothing.
    #[test]
    fn a_disarm_mid_cycle_does_not_discard_the_repair() {
        let _g = REGISTRY_LOCK.lock();
        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        if filter.live_for_test().is_none() {
            return;
        }
        cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);
        // What a per-drain-batch `is_current()` failure asks the caller to do.
        filter.disarm();
        assert!(
            filter.may_own_extra_roots(R_LOADER),
            "disarmed: prove nothing"
        );

        let late = filter.late_owners();
        assert!(
            late.addrs.contains(&(R_MIRROR as u64)),
            "disarming stops NEW proofs; it does not repair the old ones"
        );

        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
    }

    /// **A row that disappears during the cycle is unrepairable, and must be
    /// reported as such rather than repaired badly.**
    ///
    /// The difference set can only re-push an owner that is still an owner at
    /// mark end. An owner added and then removed inside the cycle can have had
    /// a wrong `false` taken against it and leaves nothing behind to push, so
    /// no comparison of key sets recovers it. A removal is the one observable
    /// sign that the append-only precondition has been broken, and the caller
    /// must then decline to certify the mark set -- which is a fallback to a
    /// stop-the-world mark, not a dropped edge.
    #[test]
    fn a_row_removed_during_the_cycle_refuses_to_certify() {
        let _g = REGISTRY_LOCK.lock();
        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
        cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);

        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        if filter.live_for_test().is_none() {
            return;
        }
        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

        let late = filter.late_owners();
        assert!(
            !late.certifiable,
            "a snapshot key set that is no longer a subset of the live one \
             means this mark set cannot be certified"
        );
    }

    /// A class that gains a pinned defining loader mid-cycle must re-push that
    /// LOADER, because `loader_pin` is keyed by class id and the value is the
    /// edge.
    #[test]
    fn a_class_pinned_after_the_snapshot_repushes_its_loader() {
        // Inside the class bitmap, so the exclusion below is exact rather than
        // an `class_overflow` fallback.
        const R_CLASS: u32 = 60_001;
        let _g = REGISTRY_LOCK.lock();
        cratonvm_types::loader_pin::forget_vm_loader_pins(R_VM);

        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        if filter.live_for_test().is_none() {
            return;
        }
        assert!(
            !filter.may_pin_loader(R_CLASS),
            "an unregistered class id must be excluded -- otherwise the stale \
             proof this test is about was never taken"
        );
        cratonvm_types::loader_pin::set_loader_pin(R_VM, R_CLASS, R_LOADER);
        assert!(!filter.is_current());

        let late = filter.late_owners();
        assert!(
            late.addrs.contains(&(R_LOADER as u64)),
            "the defining loader of a class pinned mid-cycle must be re-pushed"
        );

        cratonvm_types::loader_pin::forget_vm_loader_pins(R_VM);
    }

    /// With the filter switched off nothing is published, so nothing was
    /// proved -- and the repair must say so rather than diff against the
    /// previous collection's bank, which would re-root the world.
    #[test]
    fn the_kill_switch_leaves_no_bank_nominated_for_repair() {
        let _g = REGISTRY_LOCK.lock();
        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_MARK_ROOT_FILTER", Some("0"))],
            || filter.rebuild(),
        );
        cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);

        let late = filter.late_owners();
        assert!(
            late.addrs.is_empty() && late.certifiable && late.sources_moved == 0,
            "a filter that published nothing proved nothing"
        );

        cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
    }
}
