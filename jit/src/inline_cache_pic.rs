// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The polymorphic inline cache (PIC): the four-way inline cache and its
//! megamorphic secondary table, as generated code sees them.
//!
//! Split out of `lib.rs` on 2026-09-16, following `jfr_compile_decision`. The
//! boundary here is a concurrency protocol, not a topic.
//!
//! [`JitPICSlot`] is read by JIT-compiled machine code, without a lock, on
//! threads that are not the one writing it. Its correctness therefore does not
//! rest on any caller doing the right thing; it rests on a handful of rules
//! that are all enforced inside this file and nowhere else:
//!
//!   * a way is published class-id-LAST (a `Release` store to `class_ids[i]`
//!     after the entry word is already in place), so a reader that sees a class
//!     id is guaranteed to see the entry word that goes with it;
//!   * a way is write-once until it is explicitly retired -- nothing overwrites
//!     a live way in place, because generated code has no way to observe the
//!     two stores atomically;
//!   * a retired way parks on `RETIRED_WAY_CLASS_ID` rather than going back to
//!     zero, so a probe in flight cannot mistake it for a free way and race an
//!     installer;
//!   * a way that has never been published is NOT zero either: it starts at
//!     `EMPTY_WAY_CLASS_ID`, because `ClassId(0)` is a real receiver class
//!     and a zero guard word would match it during the word-first,
//!     class-id-last publication of that way;
//!   * the megamorphic table's two ways per set follow that same protocol, and
//!     the emitted code probes exactly the two adjacent entries of one set.
//!
//! Every one of those invariants is argued about a field of this struct and
//! checked by a method of this struct. That is what makes this a module rather
//! than a section: the whole proof obligation fits in one file, so a reader
//! auditing the publication order has exactly one file to read.
//!
//! The seam it does NOT own is the entry WORD -- `jit_ic_entry_word` /
//! `jit_ic_entry_decode` and the owner/retirement bookkeeping -- which stays at
//! the crate root because [`crate::JitMICSlot`] and the code cache's retirement
//! machinery share it. It is imported below rather than copied, which is what
//! keeps the two cache shapes provably encoding the same word.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not.

use std::sync::Arc;

use crate::{
    bump_retire_generation, defer_jit_owner, jit_entry_publishable, jit_ic_entry_decode,
    jit_ic_entry_word, owner_is_retired, resolve_jit_entry_owner, retire_generation_is_graced,
    CompiledMethod, JitMICSlot, IC_RETIRED_INSTALL_ROLLBACKS,
};

// ---------------------------------------------------------------------------
// T5.2.5 — Polymorphic Inline Cache (PIC)
// ---------------------------------------------------------------------------

/// Maximum number of entries a `JitPICSlot` holds.
///
/// Four entries cover the common bimorphic-to-small-polymorphic interface
/// shapes (including the architecture probe's four implementations) without
/// entering LFU eviction on every cycle.
pub const JIT_PIC_ENTRIES: usize = 4;
/// Megamorphic secondary cache: eight hash sets with two write-once ways each
/// (same protocol as the inline ways — see [`JitPICSlot`]). Generated code
/// probes exactly two adjacent entries.
pub const JIT_MEGA_SETS: usize = 8;
pub const JIT_MEGA_WAYS: usize = 2;
pub const JIT_MEGA_ENTRIES: usize = JIT_MEGA_SETS * JIT_MEGA_WAYS;

/// Polymorphic inline cache slot: a 4-way associative cache for virtual call
/// dispatch at a site that has seen more than one receiver type, backed by a
/// compact hashed table for receivers the four ways cannot hold.
///
/// # Allocation and population
///
/// Every virtual/interface site gets one of these eagerly, beside its
/// [`JitMICSlot`], at first compile (HIGH-7). Nothing promotes or recompiles a
/// site: the resolving helper `jit_invoke_virtual_mic` installs into both on a
/// miss, and generated code starts hitting on the next call.
///
/// # Ways are write-once under lock-free readers
///
/// Generated code — the single-pass cascade in `x64/bytecode_walk.rs`, the
/// optimizing tier's in `ir_lower.rs`, and the hashed stub in
/// `runtime_lowering.rs` — reads a way with no lock:
///
/// ```text
///   CMP  EAX, [R10 + CLASS_ID_OFFSETS[i]]   ; JNE next way
///   MOV  R11, [R10 + ENTRY_PTR_OFFSETS[i]]  ; ONE load: target and its ABI
///   TEST R11, R11                           ; JZ  resolving helper
///   BTR  R11, 0                             ; CF = needs-context, R11 = target
///   JNC  .no_context                        ; marshal ... CALL R11
/// ```
///
/// A reader past the class compare may load the entry word at any later
/// instant, so a way whose class id can still match must never be RETARGETED:
/// that would pair one receiver's guard with another receiver's body, which is
/// exactly why [`JitMICSlot::update`] refuses to retarget and why the hashed
/// table was already install-once. A published way is therefore never
/// overwritten. It can only be *retired*: its class id is replaced by
/// [`Self::RETIRED_WAY_CLASS_ID`], which no receiver matches, and only then is
/// its word zeroed. A reader already past the compare loads the old word, whose
/// body the deferred owner keeps mapped, or zero, and takes the helper.
///
/// A retired way is reused only once every thread has passed a quiescent point
/// after the retirement ([`retire_generation_is_graced`]): until then some
/// reader may still sit between that compare and its load, and a new word
/// published under it would be called for the old receiver.
///
/// When all four ways hold live receivers, a fifth is served by the hashed
/// table (same protocol) or by the helper. There is no LFU eviction — evicting
/// a live way is the retarget above.
///
/// All writers (install, retire, clear, seed) serialize on `writer`; readers
/// never take it.
///
/// # JDK-ONLY-WAVE2 — CLOSED 2026-08-06, with [`JitMICSlot`]; see the note there
///
/// `entry_words[i]` and `mega_entry_words[i]` hold raw addresses with no kind
/// beside them, so a hit cannot re-check policy. Both install paths funnel
/// through `jit_entry_publishable`, which under `JdkOnly` refuses unowned
/// (native/builtin) targets, so wave 1 keeps them empty of natives rather than
/// letting them hold an unverifiable one.
///
/// This used to prescribe a wave-2 `AtomicU8` kind array appended at the TAIL,
/// checked instead of refusing outright. **Do not do that** — see the same
/// retraction on [`JitMICSlot`], which applies here unchanged and with one
/// extra reason: a kind array would need four more loads and branches inside
/// the guard cascade, on the hottest dispatch shape in the JIT, to police a
/// state `jit_entry_publishable`'s policy-independent `owner.is_some()` early
/// return already makes unreachable in both modes.
///
/// # Memory layout
///
/// Hot fields use a structure-of-arrays prefix so the generated x86-64 stub
/// can linearly probe four packed class ids at offsets 0–12, then load the
/// matching entry word at offsets 16–40. The entire generated-code-visible
/// prefix of the four ways fits in one cache line.
///
/// # Stable layout (CRIT-8 prerequisite)
///
/// Marked `#[repr(C)]` and laid out so the JIT codegen can emit raw
/// `CMP eax, [pic_ptr + CLASS_ID_OFFSETS[i]]` for the inline 4-way dispatch.
/// The `parking_lot::Mutex` arrays — whose internal representation is not
/// guaranteed stable across `parking_lot` versions — sit at the **tail** so
/// their layout cannot disturb the hot-path offsets.
///
/// Offsets are asserted to match the constants in
/// [`JitPICSlot::CLASS_ID_OFFSETS`] et al. via a runtime test
/// (`test_jit_pic_slot_offsets`).
#[repr(C)]
pub struct JitPICSlot {
    /// Cached ClassIds, parallel to `entry_words`. A way that has never
    /// published starts at [`Self::EMPTY_WAY_CLASS_ID`] (a value no receiver
    /// matches, graced from birth), and a retired one parks on
    /// [`Self::RETIRED_WAY_CLASS_ID`] while it waits out its grace period.
    ///
    /// `0` is still ACCEPTED as "empty" by every reader and writer here, but
    /// it is never what a fresh slot holds, and that is load-bearing:
    /// `ClassId(0)` is a REAL receiver class (`java/lang/Object` itself, and
    /// the untyped `ClassId(0)`-minted synthetic containers). The emitted
    /// cascade compares the receiver's class word against every way BEFORE
    /// it loads the entry word, and a way is published word-first,
    /// class-id-last. With empty ways at `0`, a class-0 receiver matched an
    /// empty way and, in the window between those two stores, CALLed the
    /// entry word just published for some OTHER class. See
    /// `docs/internal/jit-review-r9/NOTES-invoke.md` (F1).
    /// **JIT-hot — offsets 0, 4, 8, 12.**
    pub class_ids: [std::sync::atomic::AtomicU32; JIT_PIC_ENTRIES],
    /// Tagged target words, parallel to `class_ids`: the entry address with
    /// [`JIT_IC_NEEDS_CONTEXT_TAG`] set when the target takes the VM context
    /// pointer. `0` = no target. **JIT-hot — offsets 16, 24, 32, 40.**
    pub entry_words: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// Where the per-way `needs_context` flags and their padding lived before
    /// the flag moved into the entry words. Reserved so `hits`, `misses` and
    /// the hashed table keep the offsets generated code bakes in.
    _pad1: [u8; 8],
    /// Per-way hit counter (diagnostic).
    pub hits: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// Total cache misses (receiver not in any way).
    pub misses: std::sync::atomic::AtomicU64,
    /// Compact hashed table used after the four inline ways miss. Part of the
    /// generated-code-visible prefix. Same write-once protocol as the ways.
    pub(crate) mega_class_ids: [std::sync::atomic::AtomicU32; JIT_MEGA_ENTRIES],
    pub(crate) mega_entry_words: [std::sync::atomic::AtomicU64; JIT_MEGA_ENTRIES],
    /// Cached class names (mutex-protected). Parallel to `class_ids`.
    pub class_names: [parking_lot::Mutex<Option<String>>; JIT_PIC_ENTRIES],
    /// Strong owners for compiled `entry_words`; tail-only so hot offsets stay
    /// stable. Native targets leave the corresponding element empty.
    pub(crate) compiled_owners: [parking_lot::Mutex<Option<Arc<CompiledMethod>>>; JIT_PIC_ENTRIES],
    /// Strong owners for the hashed table's raw entry words.
    mega_compiled_owners: [parking_lot::Mutex<Option<Arc<CompiledMethod>>>; JIT_MEGA_ENTRIES],
    /// The retire generation each way was stamped with when it was retired.
    /// Meaningful only while its class id is [`Self::RETIRED_WAY_CLASS_ID`].
    way_retired_gen: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// The same, for the hashed table's ways.
    mega_retired_gen: [std::sync::atomic::AtomicU64; JIT_MEGA_ENTRIES],
    /// Serializes every writer of the ways and the hashed table. Generated
    /// code never takes it.
    writer: parking_lot::Mutex<()>,
    /// The bytecode index this slot's call site lives at, or `usize::MAX` for a
    /// slot with no site. Mirrors [`JitMICSlot::bci`]; read by
    /// [`CompiledMethod::dominant_receiver_at_bci`] to refuse a site that has
    /// already proven polymorphic.
    ///
    /// Everything up to and including the `mega_*` word array is addressed by
    /// generated code at the fixed offsets in `MEGA_CLASS_IDS_OFFSET` and
    /// friends, so this and the other tail fields must stay after it. Putting
    /// `bci` after `misses` — where it reads naturally — moved
    /// `MEGA_CLASS_IDS_OFFSET` from 96 to 104 once, and
    /// `test_jit_mega_offsets_match_generated_stub_contract` caught it.
    pub bci: usize,
}

/// Miss count on a `JitMICSlot` past which
/// [`CompiledMethod::dominant_receiver_at_bci`] treats the site as
/// polymorphic: every helper entry after the install is a receiver the slot
/// could not serve.
pub const MIC_TO_PIC_THRESHOLD: u64 = 3;

impl JitPICSlot {
    /// Class id that marks a retired way (and a retired hashed-table way).
    ///
    /// No receiver matches it: class ids are allocated densely from zero, and
    /// the only other reserved value is [`JitMICSlot::INSTALLING_CLASS_ID`]
    /// (`u32::MAX`).
    pub const RETIRED_WAY_CLASS_ID: u32 = u32::MAX - 1;
    /// Class id a never-published way (and hashed-table way) starts at.
    ///
    /// Deliberately the SAME value as [`Self::RETIRED_WAY_CLASS_ID`], stamped
    /// with retire generation `0`, which [`retire_generation_is_graced`] always
    /// admits (the graced generation starts at 0 and only grows). So a fresh
    /// way is "a retired way whose grace period is already over": free to
    /// publish into, and — unlike `0` — impossible for any receiver to match,
    /// including a `ClassId(0)` one. Reusing the retired sentinel rather than
    /// minting a third reserved id keeps every reader's "is this live?" test
    /// ([`Self::is_live_class_id`]) unchanged.
    pub const EMPTY_WAY_CLASS_ID: u32 = Self::RETIRED_WAY_CLASS_ID;
    /// Byte offsets of [`Self::class_ids`] entries from the start of
    /// the struct. JIT codegen uses these to emit
    /// `CMP eax, [pic_ptr + CLASS_ID_OFFSETS[i]]` for the inline 4-way
    /// class comparisons.
    pub const CLASS_ID_OFFSETS: [usize; JIT_PIC_ENTRIES] = [0, 4, 8, 12];
    /// Byte offsets of [`Self::entry_words`] entries from the start of
    /// the struct. JIT codegen loads the tagged target word from here on a
    /// PIC hit.
    pub const ENTRY_PTR_OFFSETS: [usize; JIT_PIC_ENTRIES] = [16, 24, 32, 40];
    /// Generated-code-visible offsets for the compact hashed table. The
    /// preceding hot prefix is: ids(16), entry words(32), reserved(8),
    /// hits(32), misses(8) = 96 bytes.
    pub const MEGA_CLASS_IDS_OFFSET: usize = 96;
    pub const MEGA_ENTRY_PTRS_OFFSET: usize = Self::MEGA_CLASS_IDS_OFFSET + JIT_MEGA_ENTRIES * 4;
    pub const MEGA_HASH_MULTIPLIER: u32 = 0x9E37_79B1;
    pub const MEGA_SET_SHIFT: u8 = 29;

    #[inline]
    pub const fn mega_base_index(class_id: u32) -> usize {
        (((class_id.wrapping_mul(Self::MEGA_HASH_MULTIPLIER)) >> Self::MEGA_SET_SHIFT) as usize)
            * JIT_MEGA_WAYS
    }

    /// [`Self::new`] for a slot that serves a known call site.
    pub fn new_at(bci: usize) -> Self {
        Self { bci, ..Self::new() }
    }

    /// Create an empty PIC slot.
    pub fn new() -> Self {
        // Empty ways start at `EMPTY_WAY_CLASS_ID` with retire generation 0
        // (always graced), NOT at 0 — see the `class_ids` field doc.
        Self {
            class_ids: std::array::from_fn(|_| {
                std::sync::atomic::AtomicU32::new(Self::EMPTY_WAY_CLASS_ID)
            }),
            entry_words: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            _pad1: [0; 8],
            hits: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            misses: std::sync::atomic::AtomicU64::new(0),
            mega_class_ids: std::array::from_fn(|_| {
                std::sync::atomic::AtomicU32::new(Self::EMPTY_WAY_CLASS_ID)
            }),
            mega_entry_words: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            class_names: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            compiled_owners: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            mega_compiled_owners: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            way_retired_gen: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            mega_retired_gen: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            writer: parking_lot::Mutex::new(()),
            bci: usize::MAX,
        }
    }

    /// Whether `class_id` can name a real receiver: not an empty, installing or
    /// retired way, nor the MIC's own empty sentinel
    /// ([`JitMICSlot::EMPTY_CLASS_ID`], round 9 wave 2 — `seed_from_mic` reads a
    /// MIC class id through this test, and a fresh MIC now holds that value
    /// rather than `0`).
    #[inline]
    pub(crate) fn is_live_class_id(class_id: u32) -> bool {
        class_id != 0
            && class_id != JitMICSlot::INSTALLING_CLASS_ID
            && class_id != JitMICSlot::EMPTY_CLASS_ID
            && class_id != Self::RETIRED_WAY_CLASS_ID
    }

    /// Way `i`'s class id and decoded target `(class_id, entry, needs_context)`,
    /// for diagnostics. `None` past the last way.
    pub fn way(&self, i: usize) -> Option<(u32, u64, bool)> {
        use std::sync::atomic::Ordering;
        let class_id = self.class_ids.get(i)?.load(Ordering::Acquire);
        let (entry, needs_context) =
            jit_ic_entry_decode(self.entry_words[i].load(Ordering::Acquire));
        Some((class_id, entry, needs_context))
    }

    /// Seed the PIC from an existing MIC, so a profile-seeded or already
    /// resolved MIC entry lands in way 0 and no cache warm-up is lost. Only
    /// ever called while the slot is being built, before generated code can
    /// reach it; it still goes through the writer lock and the admission check
    /// so it cannot publish anything an install would refuse.
    pub fn seed_from_mic(&self, mic: &JitMICSlot, jdk_only: bool) {
        use std::sync::atomic::Ordering;
        let class_id = mic.cached_class_id.load(Ordering::Acquire);
        if !Self::is_live_class_id(class_id) {
            return;
        }
        // Read the owner and the word under the MIC's owner lock, which is the
        // lock every MIC writer changes the two under. Reading them separately
        // admitted the interleaving that copies a live raw word into the PIC
        // together with an owner that has already been taken: a callable
        // pointer with no keep-alive, which the emitted cascade `CALL`s after
        // the body is unmapped.
        let (owner, word) = {
            let slot_owner = mic.compiled_owner.lock();
            (
                slot_owner.clone(),
                mic.cached_entry_word.load(Ordering::Acquire),
            )
        };
        let (entry, needs_context) = jit_ic_entry_decode(word);
        // PIC slots keep `String` names; the MIC caches `Arc<str>` (cheap
        // per-hit clones in the dispatch helper) — convert on this rare path.
        let class_name = mic.cached_class_name.lock().as_deref().map(String::from);
        // Re-run the same admission the install paths run: an owner-less
        // address that is still a `jit_entry_owners` key, or that lies inside a
        // live JIT region, is a body we failed to retain — not a native
        // trampoline. Copying it forward would launder a refusal the MIC itself
        // would make today.
        let word = if jit_entry_publishable(entry, &owner, jdk_only) && !owner_is_retired(&owner) {
            jit_ic_entry_word(entry, needs_context)
        } else {
            0
        };
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let _writer = self.writer.lock();
            // "Way 0 is free", not "way 0 reads 0": a fresh way holds
            // `EMPTY_WAY_CLASS_ID`, and a comparison against `0` here would
            // silently stop every seed.
            if self.way_is_free(
                self.class_ids[0].load(Ordering::Acquire),
                &self.way_retired_gen[0],
            ) {
                let kept = if word != 0 { owner } else { None };
                if let Some(previous) =
                    std::mem::replace(&mut *self.compiled_owners[0].lock(), kept)
                {
                    deferred.push(previous);
                }
                self.entry_words[0].store(word, Ordering::SeqCst);
                *self.class_names[0].lock() = class_name;
                // Carry the hit count so the per-way counters keep the
                // cumulative picture.
                self.hits[0].store(mic.hits.load(Ordering::Relaxed), Ordering::Relaxed);
                // Publish the class id last.
                self.class_ids[0].store(class_id, Ordering::Release);
            }
        }
        for previous in deferred {
            defer_jit_owner(Some(previous));
        }
    }

    /// Fast-path lookup: find a cached way for `class_id`. Returns
    /// `Some((entry, needs_context))` on hit, `None` on miss.
    ///
    /// The pure-Rust mirror of what the JIT-generated cascade does. Two ways
    /// can briefly name the same receiver (two concurrent installs of a
    /// recompiled target); both are valid targets for it, and the first match
    /// wins here exactly as it does in generated code.
    #[inline]
    pub fn lookup(&self, class_id: u32) -> Option<(u64, bool)> {
        use std::sync::atomic::Ordering;
        if Self::is_live_class_id(class_id) {
            for i in 0..JIT_PIC_ENTRIES {
                if self.class_ids[i].load(Ordering::Acquire) == class_id {
                    let decoded = jit_ic_entry_decode(self.entry_words[i].load(Ordering::Acquire));
                    self.hits[i].fetch_add(1, Ordering::Relaxed);
                    return Some(decoded);
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Install a `(class_id → entry)` mapping into the hashed table and, if a
    /// way is free, into the four inline ways.
    ///
    /// - A way already holding exactly this target is left alone.
    /// - A class-only way for this receiver (a profile seed) is filled in
    ///   place: its guard already matches, and one store of the whole word
    ///   cannot be seen half-written.
    /// - A way holding a DIFFERENT target for this receiver (the callee was
    ///   recompiled) is retired, and the new target is published into a free
    ///   way.
    /// - A free way is an empty one, or a retired one whose grace period has
    ///   passed. With none free the hashed table or the helper serves the
    ///   receiver: a live way is never evicted.
    ///
    /// Nothing is published for a target that cannot be retained or that an
    /// invalidation has retired, and a publication that races such a
    /// retirement is rolled back after the fact.
    pub fn install(
        &self,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_ctx: bool,
        jdk_only: bool,
    ) {
        use std::sync::atomic::Ordering;
        if !Self::is_live_class_id(class_id) {
            return;
        }
        let owner = resolve_jit_entry_owner(entry_ptr as usize);
        if !jit_entry_publishable(entry_ptr, &owner, jdk_only) || owner_is_retired(&owner) {
            return;
        }
        let word = jit_ic_entry_word(entry_ptr, needs_ctx);
        if word == 0 {
            return;
        }
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut rolled_back = false;
        {
            let _writer = self.writer.lock();
            self.install_megamorphic_locked(class_id, word, &owner, &mut deferred);
            self.install_way_locked(class_id, class_name, word, &owner, &mut deferred);
            // See `JitMICSlot::update`: an invalidation sets `retired` before
            // clearing inline caches, and its clearing pass takes this lock. So
            // either that pass runs after this publication and withdraws it, or
            // it ran before and `retired` is already visible here.
            if owner_is_retired(&owner) {
                let target = entry_ptr as usize;
                self.retire_matching_locked(|entry| entry == target, &mut deferred);
                rolled_back = true;
            }
        }
        if rolled_back {
            IC_RETIRED_INSTALL_ROLLBACKS.fetch_add(1, Ordering::Relaxed);
        }
        for previous in deferred {
            defer_jit_owner(Some(previous));
        }
    }

    fn install_way_locked(
        &self,
        class_id: u32,
        class_name: &str,
        word: u64,
        owner: &Option<Arc<CompiledMethod>>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        let mut free: Option<usize> = None;
        for i in 0..JIT_PIC_ENTRIES {
            let observed = self.class_ids[i].load(Ordering::Acquire);
            if observed == class_id {
                let current = self.entry_words[i].load(Ordering::Acquire);
                if current == word {
                    return;
                }
                if current == 0 {
                    self.publish_way_locked(i, class_id, class_name, word, owner, deferred);
                    return;
                }
                self.retire_way_locked(i, deferred);
                continue;
            }
            if free.is_none() && self.way_is_free(observed, &self.way_retired_gen[i]) {
                free = Some(i);
            }
        }
        if let Some(i) = free {
            self.publish_way_locked(i, class_id, class_name, word, owner, deferred);
        }
    }

    /// An empty way, or a retired one no reader can still be inside.
    ///
    /// A fresh way (`EMPTY_WAY_CLASS_ID`, generation 0) takes the second arm:
    /// generation 0 is graced from process start. The `== 0` arm is kept only
    /// so a slot zeroed by something other than [`Self::new`] still reads as
    /// free.
    #[inline]
    fn way_is_free(&self, class_id: u32, retired_gen: &std::sync::atomic::AtomicU64) -> bool {
        class_id == 0
            || (class_id == Self::RETIRED_WAY_CLASS_ID
                && retire_generation_is_graced(
                    retired_gen.load(std::sync::atomic::Ordering::Acquire),
                ))
    }

    fn publish_way_locked(
        &self,
        i: usize,
        class_id: u32,
        class_name: &str,
        word: u64,
        owner: &Option<Arc<CompiledMethod>>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        if let Some(previous) =
            std::mem::replace(&mut *self.compiled_owners[i].lock(), owner.clone())
        {
            deferred.push(previous);
        }
        self.entry_words[i].store(word, Ordering::SeqCst);
        *self.class_names[i].lock() = Some(class_name.to_string());
        self.hits[i].store(0, Ordering::Relaxed);
        // Publish the class id last: until it lands no receiver matches, so no
        // reader can see this way half-written.
        self.class_ids[i].store(class_id, Ordering::Release);
    }

    /// Withdraw way `i`. Caller holds `writer`.
    fn retire_way_locked(&self, i: usize, deferred: &mut Vec<Arc<CompiledMethod>>) {
        use std::sync::atomic::Ordering;
        let observed = self.class_ids[i].load(Ordering::Acquire);
        if observed == 0 || observed == Self::RETIRED_WAY_CLASS_ID {
            return;
        }
        // The guard first: a reader that has not compared yet now misses.
        self.class_ids[i].store(Self::RETIRED_WAY_CLASS_ID, Ordering::Release);
        // Then the target. A reader already past the guard loads the old word
        // (whose owner is deferred, not dropped) or zero.
        self.entry_words[i].store(0, Ordering::SeqCst);
        self.hits[i].store(0, Ordering::Relaxed);
        *self.class_names[i].lock() = None;
        if let Some(previous) = self.compiled_owners[i].lock().take() {
            deferred.push(previous);
        }
        // Stamp AFTER the writes, so only a quiescent instant later than them
        // can grace a reuse of this way.
        self.way_retired_gen[i].store(bump_retire_generation(), Ordering::Release);
    }

    fn install_megamorphic_locked(
        &self,
        class_id: u32,
        word: u64,
        owner: &Option<Arc<CompiledMethod>>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        let base = Self::mega_base_index(class_id);
        let mut free: Option<usize> = None;
        for index in base..base + JIT_MEGA_WAYS {
            let observed = self.mega_class_ids[index].load(Ordering::Acquire);
            if observed == class_id {
                let current = self.mega_entry_words[index].load(Ordering::Acquire);
                if current == word {
                    return;
                }
                if current == 0 {
                    self.publish_mega_locked(index, class_id, word, owner, deferred);
                    return;
                }
                self.retire_mega_locked(index, deferred);
                continue;
            }
            if free.is_none() && self.way_is_free(observed, &self.mega_retired_gen[index]) {
                free = Some(index);
            }
        }
        // Both ways occupied by live receivers: the resolving helper is the
        // overflow path. Never evict a live way under lock-free readers.
        if let Some(index) = free {
            self.publish_mega_locked(index, class_id, word, owner, deferred);
        }
    }

    fn publish_mega_locked(
        &self,
        index: usize,
        class_id: u32,
        word: u64,
        owner: &Option<Arc<CompiledMethod>>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        if let Some(previous) =
            std::mem::replace(&mut *self.mega_compiled_owners[index].lock(), owner.clone())
        {
            deferred.push(previous);
        }
        self.mega_entry_words[index].store(word, Ordering::SeqCst);
        self.mega_class_ids[index].store(class_id, Ordering::Release);
    }

    /// Withdraw hashed-table way `index`. Caller holds `writer`.
    fn retire_mega_locked(&self, index: usize, deferred: &mut Vec<Arc<CompiledMethod>>) {
        use std::sync::atomic::Ordering;
        let observed = self.mega_class_ids[index].load(Ordering::Acquire);
        if observed == 0 || observed == Self::RETIRED_WAY_CLASS_ID {
            return;
        }
        self.mega_class_ids[index].store(Self::RETIRED_WAY_CLASS_ID, Ordering::Release);
        self.mega_entry_words[index].store(0, Ordering::SeqCst);
        if let Some(previous) = self.mega_compiled_owners[index].lock().take() {
            deferred.push(previous);
        }
        self.mega_retired_gen[index].store(bump_retire_generation(), Ordering::Release);
    }

    /// Retire every way and hashed-table way whose target satisfies `matches`.
    /// Caller holds `writer`.
    fn retire_matching_locked(
        &self,
        matches: impl Fn(usize) -> bool,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        for i in 0..JIT_PIC_ENTRIES {
            let (entry, _) = jit_ic_entry_decode(self.entry_words[i].load(Ordering::SeqCst));
            if entry != 0 && matches(entry as usize) {
                self.retire_way_locked(i, deferred);
            }
        }
        for index in 0..JIT_MEGA_ENTRIES {
            let (entry, _) =
                jit_ic_entry_decode(self.mega_entry_words[index].load(Ordering::SeqCst));
            if entry != 0 && matches(entry as usize) {
                self.retire_mega_locked(index, deferred);
            }
        }
    }

    /// Lookup used by the shared helper after the generated four-way cascade
    /// misses. The returned entry remains executable because this slot retains
    /// its compiled owner until the way is retired, and a retired owner goes
    /// through the retirement queue.
    #[inline]
    pub fn lookup_megamorphic(&self, class_id: u32) -> Option<(u64, bool)> {
        use std::sync::atomic::Ordering;
        if !Self::is_live_class_id(class_id) {
            return None;
        }
        let base = Self::mega_base_index(class_id);
        for index in base..base + JIT_MEGA_WAYS {
            if self.mega_class_ids[index].load(Ordering::Acquire) == class_id {
                let (entry, needs_context) =
                    jit_ic_entry_decode(self.mega_entry_words[index].load(Ordering::Acquire));
                if entry != 0 {
                    return Some((entry, needs_context));
                }
            }
        }
        None
    }

    /// Retire every cached target in this PIC.
    ///
    /// Every live way and hashed-table way is retired (guard first, then word),
    /// and every owner is released through the retirement queue.
    pub fn clear_entries(&self) {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let _writer = self.writer.lock();
            for i in 0..JIT_PIC_ENTRIES {
                self.retire_way_locked(i, &mut deferred);
            }
            for index in 0..JIT_MEGA_ENTRIES {
                self.retire_mega_locked(index, &mut deferred);
            }
        }
        for previous in deferred {
            defer_jit_owner(Some(previous));
        }
    }

    pub(crate) fn invalidate_targets(&self, targets: &std::collections::HashSet<usize>) {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let _writer = self.writer.lock();
            self.retire_matching_locked(|entry| targets.contains(&entry), &mut deferred);
        }
        for previous in deferred {
            defer_jit_owner(Some(previous));
        }
    }

    /// Total observed invocations (hits across all ways + misses).
    pub fn total_observations(&self) -> u64 {
        let hits: u64 = self
            .hits
            .iter()
            .map(|h| h.load(std::sync::atomic::Ordering::Relaxed))
            .sum();
        hits + self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of ways currently holding a live receiver (0..=4).
    pub fn entries_used(&self) -> usize {
        self.class_ids
            .iter()
            .filter(|c| Self::is_live_class_id(c.load(std::sync::atomic::Ordering::Relaxed)))
            .count()
    }
}

impl Default for JitPICSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod empty_way_tests {
    //! A never-published way must not be matchable by ANY receiver, including
    //! a `ClassId(0)` one — see the `class_ids` field doc and
    //! `docs/internal/jit-review-r9/NOTES-invoke.md` (F1).
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_fresh_slot_has_no_way_a_class_zero_receiver_can_match() {
        let pic = JitPICSlot::new();
        for i in 0..JIT_PIC_ENTRIES {
            let cid = pic.class_ids[i].load(Ordering::Acquire);
            assert_ne!(cid, 0, "fresh way {i} would match a ClassId(0) receiver");
            assert!(
                !JitPICSlot::is_live_class_id(cid),
                "fresh way {i} reads live"
            );
        }
        for i in 0..JIT_MEGA_ENTRIES {
            assert_ne!(
                pic.mega_class_ids[i].load(Ordering::Acquire),
                0,
                "fresh hashed-table way {i} would match a ClassId(0) receiver"
            );
        }
        assert_eq!(pic.entries_used(), 0);
        assert!(pic.lookup(0).is_none());
        assert!(pic.lookup_megamorphic(0).is_none());
    }

    #[test]
    fn a_fresh_way_is_still_free_to_publish_into() {
        let pic = JitPICSlot::new();
        pic.install(5, "Five", 0x5000, false, false);
        assert_eq!(pic.lookup(5), Some((0x5000, false)));
        assert_eq!(pic.lookup_megamorphic(5), Some((0x5000, false)));
        assert_eq!(pic.entries_used(), 1);
        // Every way the install did not use is still unmatchable by class 0.
        for i in 0..JIT_PIC_ENTRIES {
            let cid = pic.class_ids[i].load(Ordering::Acquire);
            if cid != 5 {
                assert_ne!(cid, 0, "unused way {i} reads 0 after an install");
            }
        }
        // All four ways fill, exactly as before the sentinel change.
        pic.install(6, "Six", 0x6000, false, false);
        pic.install(7, "Seven", 0x7000, true, false);
        pic.install(8, "Eight", 0x8000, false, false);
        assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);
    }

    #[test]
    fn seeding_from_a_mic_still_lands_in_way_zero() {
        let mic = JitMICSlot::new();
        mic.update(7, "Seven", 0x7000, true, false);
        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic, false);
        assert_eq!(pic.class_ids[0].load(Ordering::Acquire), 7);
        assert_eq!(pic.lookup(7), Some((0x7000, true)));
    }

    /// Round 9 wave 2: the MIC's empty sentinel is not a class, so seeding a
    /// PIC from a never-filled MIC must copy nothing into way 0.
    #[test]
    fn the_mic_empty_sentinel_is_not_a_live_class_id() {
        assert!(!JitPICSlot::is_live_class_id(JitMICSlot::EMPTY_CLASS_ID));
        assert!(!JitPICSlot::is_live_class_id(0));
        assert!(JitPICSlot::is_live_class_id(7));
        let mic = JitMICSlot::new();
        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic, false);
        assert_eq!(pic.entries_used(), 0);
        assert!(pic.lookup(JitMICSlot::EMPTY_CLASS_ID).is_none());
    }
}
