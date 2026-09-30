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
//!     the emitted code probes exactly the two adjacent entries of one set;
//!   * so do the VM-wide [`MegaDispatchTable`]'s four ways per set (key word
//!     for class id, `RETIRED_KEY` for the retired sentinel), which the
//!     hashed stub has probed in machine code since round 12 wave 3.
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
    CompiledMethod, IcSlotHolder, IcSlotKind, JitMICSlot, IC_RETIRED_INSTALL_ROLLBACKS,
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
    /// Non-zero once this site has overflowed its four inline ways: a fifth
    /// live receiver class found every way holding another live class
    /// ([`Self::install`]). Sticky. At [`Self::MEGAMORPHIC_OFFSET`], next to
    /// the inline ways, where the megamorphic gate
    /// (`runtime_lowering::emit_megamorphic_gate`, round 12 wave 6, lane
    /// mega5, proposal W5-4) reads it to send the site straight to the hashed
    /// stub, which probes the receiver's class cell first. A stale or racing
    /// read only picks which of two complete cache paths runs.
    ///
    /// This and [`Self::site_bits`] took the 8 bytes where the per-way
    /// `needs_context` flags lived before the flag moved into the entry words,
    /// so `hits`, `misses` and the hashed table keep their offsets.
    megamorphic: std::sync::atomic::AtomicU32,
    /// Compile-time facts about the site's generated code
    /// ([`Self::SITE_INLINE_WAYS_PROBED`]); never read by machine code.
    site_bits: std::sync::atomic::AtomicU32,
    /// Per-way hit counter (diagnostic).
    pub hits: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// Total cache misses (receiver not in any way).
    pub misses: std::sync::atomic::AtomicU64,
    /// Compact hashed table used after the four inline ways miss. Part of the
    /// generated-code-visible prefix. Same write-once protocol as the ways.
    pub(crate) mega_class_ids: [std::sync::atomic::AtomicU32; JIT_MEGA_ENTRIES],
    pub(crate) mega_entry_words: [std::sync::atomic::AtomicU64; JIT_MEGA_ENTRIES],
    /// The VM's shared [`MegaDispatchTable`] (its [`MegaDispatchTable::slots_base`]),
    /// bound by the resolving helper the first time this site publishes into
    /// it; `0` = unbound (round 12 wave 2, lane calls, proposal W2-1). At
    /// [`Self::MEGA_DISPATCH_TABLE_OFFSET`], where the hashed stub's
    /// shared-table probe reads it (round 12 wave 3,
    /// `runtime_lowering::emit_mega_dispatch_table_probe`).
    mega_dispatch_table: std::sync::atomic::AtomicUsize,
    /// This site's selector in that table (`1..`), `0` = unbound. Stored AFTER
    /// the table word, so a reader that sees a selector sees the table. At
    /// [`Self::MEGA_DISPATCH_SELECTOR_OFFSET`].
    mega_selector: std::sync::atomic::AtomicU32,
    /// This site's COLUMN in the table's per-class arrays
    /// ([`MegaDispatchTable::install_with_class_slot`]): the resolved method's
    /// vtable slot plus one, `0` = none (round 12 wave 4, lane mega3, M3-1).
    /// Stored AFTER [`Self::mega_class_root`] and after the selector, so a
    /// reader that sees a column sees both. At [`Self::MEGA_CLASS_SLOT_OFFSET`]
    /// (what was the selector's trailing padding: no earlier offset moves).
    mega_class_slot: std::sync::atomic::AtomicU32,
    /// The address of the table's class-directory root
    /// ([`MegaDispatchTable::class_root_addr`]), `0` = unbound. At
    /// [`Self::MEGA_CLASS_ROOT_OFFSET`].
    mega_class_root: std::sync::atomic::AtomicUsize,
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
    /// The class id each inline way held when it was last retired (round 12
    /// wave 7, lane mega6): a retired way may take THAT class again before
    /// its grace period ends ([`Self::way_refillable`]). Written and read
    /// only under `writer`; never read by generated code.
    way_retired_class: [std::sync::atomic::AtomicU32; JIT_PIC_ENTRIES],
    /// The same, for the hashed table's ways.
    mega_retired_class: [std::sync::atomic::AtomicU32; JIT_MEGA_ENTRIES],
    /// [`same_key_refill_enabled`], read once when the slot is made.
    same_class_refill: bool,
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
    /// This slot's own `(holder body, index in its _jit_pic_slots)`, bound at
    /// publication (round 11 wave 19, lane lhm; see [`crate::IcHolderRef`]).
    /// A tail field, like `bci`; generated code never reads it.
    pub(crate) holder: IcSlotHolder,
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
    /// Byte offset of [`Self::megamorphic`] (round 12 wave 6, lane mega5).
    pub const MEGAMORPHIC_OFFSET: usize = 48;
    /// [`Self::site_bits`]: the site's generated code probes the four inline
    /// ways for EVERY receiver (the single-pass PIC cascade sets it when it
    /// emits one), so a receiver published there need not also take a hashed
    /// way (proposal W5-6). Until the site turns megamorphic.
    pub const SITE_INLINE_WAYS_PROBED: u32 = 1;
    /// Generated-code-visible offsets for the compact hashed table. The
    /// preceding hot prefix is: ids(16), entry words(32), megamorphic flag and
    /// site bits(8), hits(32), misses(8) = 96 bytes.
    pub const MEGA_CLASS_IDS_OFFSET: usize = 96;
    pub const MEGA_ENTRY_PTRS_OFFSET: usize = Self::MEGA_CLASS_IDS_OFFSET + JIT_MEGA_ENTRIES * 4;
    /// [`Self::mega_dispatch_table`]'s offset: right after the hashed table's
    /// words, which keeps every earlier offset unchanged.
    pub const MEGA_DISPATCH_TABLE_OFFSET: usize =
        Self::MEGA_ENTRY_PTRS_OFFSET + JIT_MEGA_ENTRIES * 8;
    /// [`Self::mega_selector`]'s offset.
    pub const MEGA_DISPATCH_SELECTOR_OFFSET: usize = Self::MEGA_DISPATCH_TABLE_OFFSET + 8;
    /// [`Self::mega_class_slot`]'s offset (round 12 wave 4, lane mega3).
    pub const MEGA_CLASS_SLOT_OFFSET: usize = Self::MEGA_DISPATCH_SELECTOR_OFFSET + 4;
    /// [`Self::mega_class_root`]'s offset.
    pub const MEGA_CLASS_ROOT_OFFSET: usize = Self::MEGA_DISPATCH_SELECTOR_OFFSET + 8;
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
            megamorphic: std::sync::atomic::AtomicU32::new(0),
            site_bits: std::sync::atomic::AtomicU32::new(0),
            hits: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            misses: std::sync::atomic::AtomicU64::new(0),
            mega_class_ids: std::array::from_fn(|_| {
                std::sync::atomic::AtomicU32::new(Self::EMPTY_WAY_CLASS_ID)
            }),
            mega_entry_words: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            mega_dispatch_table: std::sync::atomic::AtomicUsize::new(0),
            mega_selector: std::sync::atomic::AtomicU32::new(0),
            mega_class_slot: std::sync::atomic::AtomicU32::new(0),
            mega_class_root: std::sync::atomic::AtomicUsize::new(0),
            class_names: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            compiled_owners: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            mega_compiled_owners: std::array::from_fn(|_| parking_lot::Mutex::new(None)),
            way_retired_gen: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            mega_retired_gen: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            // No live receiver has this id, so a fresh way is never "retired
            // from" a class that could ask to refill it.
            way_retired_class: std::array::from_fn(|_| {
                std::sync::atomic::AtomicU32::new(Self::RETIRED_WAY_CLASS_ID)
            }),
            mega_retired_class: std::array::from_fn(|_| {
                std::sync::atomic::AtomicU32::new(Self::RETIRED_WAY_CLASS_ID)
            }),
            same_class_refill: same_key_refill_enabled(),
            writer: parking_lot::Mutex::new(()),
            bci: usize::MAX,
            holder: std::sync::OnceLock::new(),
        }
    }

    /// Record, on `owner`, that this slot now publishes it (round 11 wave 19,
    /// lane lhm), and withdraw that publication at once if `owner` was already
    /// superseded — the supersede walk may have taken its list before this
    /// record landed. The mirror of `JitMICSlot::note_published_owner`; no-op
    /// for an unbound slot.
    fn note_published_owner(&self, owner: &Arc<CompiledMethod>) {
        let Some((holder, index)) = self.holder.get() else {
            return;
        };
        owner.note_ic_holder(holder, *index, IcSlotKind::Pic);
        if owner.superseded.load(std::sync::atomic::Ordering::Acquire) {
            self.retire_entry(owner.entry_ptr() as usize);
        }
    }

    /// `bind_inline_cache_holders`' half for a slot that published before it
    /// was bound (a way seeded from the MIC while the body was being built).
    pub(crate) fn register_current_owners(&self) {
        let mut owners: Vec<Arc<CompiledMethod>> = Vec::new();
        for cell in self
            .compiled_owners
            .iter()
            .chain(self.mega_compiled_owners.iter())
        {
            if let Some(owner) = cell.lock().clone() {
                if !owners.iter().any(|o| Arc::ptr_eq(o, &owner)) {
                    owners.push(owner);
                }
            }
        }
        for owner in &owners {
            self.note_published_owner(owner);
        }
    }

    /// Retire every inline way and hashed-table way whose target is `entry`
    /// (round 11 wave 19, lane lhm: the supersede walk,
    /// `CompiledMethod::retarget_superseded_inline_caches`). Returns whether any
    /// way was retired. Same protocol as [`Self::invalidate_targets`].
    pub(crate) fn retire_entry(&self, entry: usize) -> bool {
        if entry == 0 {
            return false;
        }
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut retired = false;
        {
            let _writer = self.writer.lock();
            for i in 0..JIT_PIC_ENTRIES {
                let (way_entry, _) = jit_ic_entry_decode(
                    self.entry_words[i].load(std::sync::atomic::Ordering::SeqCst),
                );
                if way_entry as usize == entry {
                    self.retire_way_locked(i, &mut deferred);
                    retired = true;
                }
            }
            for index in 0..JIT_MEGA_ENTRIES {
                let (way_entry, _) = jit_ic_entry_decode(
                    self.mega_entry_words[index].load(std::sync::atomic::Ordering::SeqCst),
                );
                if way_entry as usize == entry {
                    self.retire_mega_locked(index, &mut deferred);
                    retired = true;
                }
            }
        }
        defer_jit_owners(deferred);
        retired
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
        defer_jit_owners(deferred);
    }

    /// Fast-path lookup: find a cached way for `class_id`. Returns
    /// `Some((entry, needs_context))` on hit, `None` on miss.
    ///
    /// The pure-Rust mirror of what the JIT-generated cascade does. Two ways
    /// can briefly name the same receiver (two concurrent installs of a
    /// recompiled target); both are valid targets for it, and the first match
    /// wins here exactly as it does in generated code.
    ///
    /// A way whose guard matches but whose WORD is zero is a MISS, not a hit
    /// with a zero target — because that is what the emitted cascade does with
    /// it (`MOV R11,[slot]; TEST R11,R11; JZ .miss`, in `x64/op_invoke.rs`,
    /// `ir_lower.rs` and the hashed stub), and [`Self::lookup_megamorphic`]
    /// already filtered it. Such a way is reachable two ways: a profile seed
    /// publishes the class id with no target for `install` to fill in later,
    /// and a retirement clears the word AFTER the guard. Returning
    /// `Some((0, _))` made this mirror answer "dispatch to address 0" for a
    /// state generated code sends to the resolving helper, and charged the way
    /// a hit for a call it cannot serve.
    #[inline]
    pub fn lookup(&self, class_id: u32) -> Option<(u64, bool)> {
        use std::sync::atomic::Ordering;
        if Self::is_live_class_id(class_id) {
            for i in 0..JIT_PIC_ENTRIES {
                if self.class_ids[i].load(Ordering::Acquire) == class_id {
                    let (entry, needs_context) =
                        jit_ic_entry_decode(self.entry_words[i].load(Ordering::Acquire));
                    if entry == 0 {
                        continue;
                    }
                    self.hits[i].fetch_add(1, Ordering::Relaxed);
                    return Some((entry, needs_context));
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
    ///
    /// Returns whether the receiver was refused ONLY for a lagging grace: no
    /// inline way or hashed way of its set took it, while one of them is a
    /// retired way whose retirement is not graced yet (round 13 wave 11, lane
    /// mega9: the measurement proposal M13-4 asks for before a poll-word grace
    /// request is built; `jit_invoke_virtual_mic` counts it as
    /// `mic_grace_lag_refused` under `CRATONVM_DBG=mic-prof`).
    ///
    /// Round 14 wave 2 (lane mic, proposal M8-1): before answering "refused
    /// for a lagging grace", try the per-thread grace proof once
    /// ([`crate::jit_ic_grace_catch_up`], rate-limited per thread, outside the
    /// writer lock) and, if it advanced the grace, install once more. So the
    /// caller's handshake request (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`) follows
    /// only a refusal the cheap proof could not lift.
    pub fn install(
        &self,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_ctx: bool,
        jdk_only: bool,
    ) -> bool {
        let refused = self.install_once(class_id, class_name, entry_ptr, needs_ctx, jdk_only);
        if refused && crate::jit_ic_grace_catch_up() {
            return self.install_once(class_id, class_name, entry_ptr, needs_ctx, jdk_only);
        }
        refused
    }

    /// One attempt of [`Self::install`].
    fn install_once(
        &self,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_ctx: bool,
        jdk_only: bool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !Self::is_live_class_id(class_id) {
            return false;
        }
        let owner = resolve_jit_entry_owner(entry_ptr as usize);
        if !jit_entry_publishable(entry_ptr, &owner, jdk_only) || owner_is_retired(&owner) {
            return false;
        }
        let word = jit_ic_entry_word(entry_ptr, needs_ctx);
        if word == 0 {
            return false;
        }
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut rolled_back = false;
        let newly_published;
        let grace_lag_refused;
        {
            let _writer = self.writer.lock();
            let held_before = self.holds_word_locked(class_id, word);
            // Round 12 wave 6 (lane mega5, W5-6): at a site whose code probes
            // the inline ways for every receiver, a receiver an inline way
            // takes does not also spend a hashed way; the hashed set gets it
            // only when the inline ways refuse it. Everywhere else (a site
            // whose machine code never reads the inline ways, the Rust
            // helper's own `lookup_megamorphic`, a megamorphic site whose gate
            // skips them) the hashed ways stay first, as before.
            let inline_first = self.inline_first();
            if inline_first {
                self.install_way_locked(class_id, class_name, word, &owner, &mut deferred);
                if self.inline_holds_word_locked(class_id, word) {
                    self.retire_stale_mega_locked(class_id, word, &mut deferred);
                } else {
                    self.install_megamorphic_locked(class_id, word, &owner, &mut deferred);
                }
            } else {
                self.install_megamorphic_locked(class_id, word, &owner, &mut deferred);
                self.install_way_locked(class_id, class_name, word, &owner, &mut deferred);
            }
            // The overflow that makes the site megamorphic (W5-4). At an
            // inline-first site its gate is about to skip the inline ways, so
            // their receivers take hashed ways now, under this same lock.
            if self.note_overflow_locked(class_id) && inline_first {
                self.promote_inline_ways_locked(&mut deferred);
            }
            newly_published = !held_before && self.holds_word_locked(class_id, word);
            grace_lag_refused =
                !self.holds_word_locked(class_id, word) && self.grace_lags_locked(class_id);
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
        defer_jit_owners(deferred);
        // The supersede reverse index (round 11 wave 19, lane lhm). Recorded
        // only when this call made the body newly reachable from this slot: a
        // megamorphic site whose receivers overflow every way re-enters
        // `install` on each helper call, and must not pay the record's lock
        // and scan each time. A body already held here was recorded when it
        // landed (or at binding).
        if !rolled_back && newly_published {
            if let Some(owner) = owner.as_ref() {
                self.note_published_owner(owner);
            }
        }
        grace_lag_refused
    }

    /// Whether an inline way, or a hashed way of `class_id`'s set, is a
    /// retired way whose retirement is not graced yet: one a receiver that
    /// found no free way would have taken had every thread passed a quiescent
    /// point since. Caller holds `writer`. Round 13 wave 11, lane mega9 (the
    /// `install` refusal count).
    fn grace_lags_locked(&self, class_id: u32) -> bool {
        use std::sync::atomic::Ordering;
        let lagging = |observed: u32, retired_gen: &std::sync::atomic::AtomicU64| {
            observed == Self::RETIRED_WAY_CLASS_ID
                && !retire_generation_is_graced(retired_gen.load(Ordering::Acquire))
        };
        if (0..JIT_PIC_ENTRIES)
            .any(|i| lagging(self.class_ids[i].load(Ordering::Acquire), &self.way_retired_gen[i]))
        {
            return true;
        }
        let base = Self::mega_base_index(class_id);
        (base..base + JIT_MEGA_WAYS).any(|index| {
            lagging(
                self.mega_class_ids[index].load(Ordering::Acquire),
                &self.mega_retired_gen[index],
            )
        })
    }

    /// Whether an inline way or this class's hashed-table set holds `word` for
    /// `class_id`. Caller holds `writer`.
    fn holds_word_locked(&self, class_id: u32, word: u64) -> bool {
        use std::sync::atomic::Ordering;
        let inline = (0..JIT_PIC_ENTRIES).any(|i| {
            self.class_ids[i].load(Ordering::Acquire) == class_id
                && self.entry_words[i].load(Ordering::Acquire) == word
        });
        if inline {
            return true;
        }
        let base = Self::mega_base_index(class_id);
        (base..base + JIT_MEGA_WAYS).any(|index| {
            self.mega_class_ids[index].load(Ordering::Acquire) == class_id
                && self.mega_entry_words[index].load(Ordering::Acquire) == word
        })
    }

    /// Whether an INLINE way holds `word` for `class_id`. Caller holds `writer`.
    fn inline_holds_word_locked(&self, class_id: u32, word: u64) -> bool {
        use std::sync::atomic::Ordering;
        (0..JIT_PIC_ENTRIES).any(|i| {
            self.class_ids[i].load(Ordering::Acquire) == class_id
                && self.entry_words[i].load(Ordering::Acquire) == word
        })
    }

    /// Retire a hashed way that still names `class_id` with another target
    /// than `word` (a callee recompiled while the receiver sat in an inline
    /// way): what `install_megamorphic_locked` does for its own publication,
    /// for an inline-first publication that skips it. Caller holds `writer`.
    fn retire_stale_mega_locked(
        &self,
        class_id: u32,
        word: u64,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        let base = Self::mega_base_index(class_id);
        for index in base..base + JIT_MEGA_WAYS {
            if self.mega_class_ids[index].load(Ordering::Acquire) == class_id
                && self.mega_entry_words[index].load(Ordering::Acquire) != word
            {
                self.retire_mega_locked(index, deferred);
            }
        }
    }

    /// Whether this site has overflowed its inline ways ([`Self::megamorphic`]).
    #[inline]
    pub(crate) fn is_megamorphic(&self) -> bool {
        self.megamorphic
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Record that the site's generated code probes the four inline ways for
    /// every receiver ([`Self::SITE_INLINE_WAYS_PROBED`]). Called by the
    /// emitter of the single-pass PIC cascade while it compiles the site,
    /// before the slot is reachable from any published code.
    pub(crate) fn note_inline_ways_probed(&self) {
        self.site_bits.fetch_or(
            Self::SITE_INLINE_WAYS_PROBED,
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Round 13 wave 11 (lane mega9, proposal M13-9): mark a slot that is
    /// still being built megamorphic when its site's receiver profile already
    /// shows more classes than the four inline ways hold
    /// ([`profile_names_megamorphic_site`]). The site's gate
    /// (`runtime_lowering::emit_megamorphic_gate`) then sends every receiver
    /// past the MIC (optimizing tier) or inline way 0 (single-pass) to the
    /// cell-first hashed stub from the first call. Without the seed, a
    /// recompiled caller's fresh slot started clear: its first receivers
    /// filled the inline ways and took the helper, and only a fifth live class
    /// set the flag again.
    ///
    /// Called only by the compile-time planners (`lib.rs`), before the slot
    /// can be reached from published code, like [`Self::note_inline_ways_probed`].
    /// The flag only picks which of two complete cache paths runs
    /// ([`Self::megamorphic`]), so a wrong seed costs time, not an answer.
    /// `CRATONVM_JIT_IC_PROFILE_MEGA_SEED=0` seeds nothing. Returns whether
    /// it set the flag.
    pub(crate) fn seed_megamorphic_from_profile(
        &self,
        counts: &crate::profile::ReceiverCounts,
    ) -> bool {
        if !profile_mega_seed_enabled() || !profile_names_megamorphic_site(counts) {
            return false;
        }
        self.megamorphic.store(1, std::sync::atomic::Ordering::Release);
        true
    }

    /// Publish into the inline ways first (proposal W5-6): the site's code
    /// reads them for every receiver, and it is not megamorphic yet (a
    /// megamorphic site's gate skips them).
    fn inline_first(&self) -> bool {
        self.site_bits.load(std::sync::atomic::Ordering::Acquire) & Self::SITE_INLINE_WAYS_PROBED
            != 0
            && !self.is_megamorphic()
    }

    /// Mark the site megamorphic when `class_id`, a live receiver the caller
    /// just tried to publish, holds no inline way while every way holds a live
    /// receiver of another class: five distinct live classes. A way retired
    /// for a recompiled callee and still in its grace period is not live, so
    /// a four-class site whose callee tiered up is not counted. Returns
    /// whether this call set the flag. Caller holds `writer`.
    fn note_overflow_locked(&self, class_id: u32) -> bool {
        use std::sync::atomic::Ordering;
        if self.is_megamorphic() {
            return false;
        }
        for i in 0..JIT_PIC_ENTRIES {
            let observed = self.class_ids[i].load(Ordering::Acquire);
            if observed == class_id || !Self::is_live_class_id(observed) {
                return false;
            }
        }
        self.megamorphic.store(1, Ordering::Release);
        true
    }

    /// Give every published inline way's receiver a hashed way too (with its
    /// retained owner), for a site that just turned megamorphic after
    /// publishing inline-first: its gate no longer reads ways 1..3, and the
    /// hashed stub it goes to reads hashed ways. A full set publishes nothing,
    /// as for any receiver; the helper then serves it. Caller holds `writer`.
    fn promote_inline_ways_locked(&self, deferred: &mut Vec<Arc<CompiledMethod>>) {
        use std::sync::atomic::Ordering;
        for i in 0..JIT_PIC_ENTRIES {
            let class_id = self.class_ids[i].load(Ordering::Acquire);
            let word = self.entry_words[i].load(Ordering::Acquire);
            if !Self::is_live_class_id(class_id) || word == 0 {
                continue;
            }
            let owner = self.compiled_owners[i].lock().clone();
            self.install_megamorphic_locked(class_id, word, &owner, deferred);
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
        let mut refill: Option<usize> = None;
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
            } else if refill.is_none()
                && self.way_refillable(observed, &self.way_retired_class[i], class_id)
            {
                refill = Some(i);
            }
        }
        if let Some(i) = free.or(refill) {
            self.publish_way_locked(i, class_id, class_name, word, owner, deferred);
        }
    }

    /// Whether a way that reads `observed` and was last retired from
    /// `retired_class` may take `class_id` again before its grace period ends
    /// (round 12 wave 7, lane mega6; `CRATONVM_JIT_IC_SAME_KEY_REFILL`).
    ///
    /// Grace protects exactly one reader: one that matched the way's OLD class
    /// id and has not loaded the word yet. Such a reader's receiver IS of the
    /// class the way was retired from, so a word published for that same
    /// class is a correct target for it -- the reader loads the old word
    /// (kept mapped by the retirement queue), zero (a miss) or the new one.
    /// This is the refill `JitMICSlot` has always done for its one class
    /// (`clear_compiled_entry` keeps the class id; `update` fills the word).
    ///
    /// Without it a way retired while its thread stays inside compiled code
    /// (an OSR loop never returns to depth 0, so nothing is graced) cannot
    /// take its receiver's recompiled body: every callee that tiers up
    /// during such a loop lost its inline way for the rest of the site's life.
    #[inline]
    fn way_refillable(
        &self,
        observed: u32,
        retired_class: &std::sync::atomic::AtomicU32,
        class_id: u32,
    ) -> bool {
        self.same_class_refill
            && observed == Self::RETIRED_WAY_CLASS_ID
            && Self::is_live_class_id(class_id)
            && retired_class.load(std::sync::atomic::Ordering::Relaxed) == class_id
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
        // Only that class may refill the way before it is graced.
        self.way_retired_class[i].store(observed, Ordering::Relaxed);
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
        let mut refill: Option<usize> = None;
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
            } else if refill.is_none()
                && self.way_refillable(observed, &self.mega_retired_class[index], class_id)
            {
                refill = Some(index);
            }
        }
        // Both ways occupied by live receivers: the resolving helper is the
        // overflow path. Never evict a live way under lock-free readers. A
        // way retired from this very class may take it again at once
        // (`way_refillable`).
        if let Some(index) = free.or(refill) {
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
        self.mega_retired_class[index].store(observed, Ordering::Relaxed);
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
        defer_jit_owners(deferred);
    }

    pub(crate) fn invalidate_targets(&self, targets: &std::collections::HashSet<usize>) {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let _writer = self.writer.lock();
            self.retire_matching_locked(|entry| targets.contains(&entry), &mut deferred);
        }
        defer_jit_owners(deferred);
    }

    /// Retire every way and hashed-table way whose retained compiled owner
    /// satisfies `stale` (interpreter round i1 wave 21, lane L1): matched by
    /// the owner, so a body a tier-up superseded -- in no cache map, so in no
    /// entry set an invalidation builds -- is found too.
    pub(crate) fn invalidate_owned_targets(&self, stale: &dyn Fn(&Arc<CompiledMethod>) -> bool) {
        use std::sync::atomic::Ordering;
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let _writer = self.writer.lock();
            // A way with no target word retains no owner: skipped unlocked.
            for i in 0..JIT_PIC_ENTRIES {
                let doomed = self.entry_words[i].load(Ordering::Acquire) != 0
                    && self.compiled_owners[i].lock().as_ref().is_some_and(stale);
                if doomed {
                    self.retire_way_locked(i, &mut deferred);
                }
            }
            for index in 0..JIT_MEGA_ENTRIES {
                let doomed = self.mega_entry_words[index].load(Ordering::Acquire) != 0
                    && self.mega_compiled_owners[index]
                        .lock()
                        .as_ref()
                        .is_some_and(stale);
                if doomed {
                    self.retire_mega_locked(index, &mut deferred);
                }
            }
        }
        defer_jit_owners(deferred);
    }

    /// Retire every inline way and hashed-table way keyed on a RECEIVER class
    /// `dead` names (a class unload; round 13 wave 8, lane mega7,
    /// `r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`). Returns
    /// how many ways were retired.
    ///
    /// The unload pass retires by TARGET, which leaves a way whose target a
    /// surviving class declares (an inherited or default method, a surviving
    /// base's concrete method) keyed on a class id no receiver carries again
    /// (ids are never reused). Such a way is dead weight for the site's life:
    /// it never matches, it is never evicted, and it counts as a live fifth
    /// class in [`Self::note_overflow_locked`], so loader churn fills a site's
    /// four inline ways and sixteen hashed ways with the dead and sends every
    /// later receiver past them. Retired by the ordinary protocol (guard
    /// first, owner through the retirement queue), so another class may take
    /// the way once graced. Sound without the grace too: no reader can be
    /// between a dead class's compare and its word load, since its receiver
    /// would have kept the class alive.
    ///
    /// Production goes through `Self::retire_receiver_classes_deferring`
    /// (round 13 wave 10); this one-slot form is the tests'.
    #[cfg(test)]
    pub(crate) fn retire_receiver_classes(&self, dead: &dyn Fn(u32) -> bool) -> usize {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut free_at_once: Option<bool> = None;
        let retired =
            self.retire_receiver_classes_deferring(dead, &mut free_at_once, &mut deferred);
        defer_jit_owners(deferred);
        retired
    }

    /// `Self::retire_receiver_classes`, handing the withdrawn owners to
    /// `deferred` instead of the retirement queue, so a caller retiring many
    /// slots queues them in one batch ([`defer_jit_owners`]).
    ///
    /// Round 13 wave 10 (lane mega8): with `free_at_once` (read from
    /// [`ic_dead_ways_free_at_once_enabled`] the first time a slot holds a
    /// dead receiver, then reused by the caller's later slots) a withdrawn
    /// way is stamped graced (generation 0, a fresh way's stamp) instead of
    /// waiting out a grace period. Grace protects exactly one reader: one
    /// that matched the way's class id and has not loaded its word yet. That
    /// reader holds a receiver of the class, and no receiver of an unloaded
    /// class exists (the argument `JitMICSlot::forget_dead_receiver` already
    /// empties a MIC on). So the way may take another class at once. Before,
    /// a thread that stays in compiled code (the case
    /// `r12w7-mega6-grace-starves-while-a-thread-stays-compiled-CLOSED-20260929.md`
    /// is about) kept every way an unload freed parked, and the next loader
    /// generation's receivers found the site's four inline ways and its
    /// hashed sets full of retired ways.
    fn retire_receiver_classes_deferring(
        &self,
        dead: &dyn Fn(u32) -> bool,
        free_at_once: &mut Option<bool>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) -> usize {
        use std::sync::atomic::Ordering;
        let holds_dead = |class_id: u32| Self::is_live_class_id(class_id) && dead(class_id);
        // Unlocked screen: almost every site holds no dead receiver, and the
        // unload pass visits every site of every published body.
        let any = self
            .class_ids
            .iter()
            .chain(self.mega_class_ids.iter())
            .any(|class_id| holds_dead(class_id.load(Ordering::Acquire)));
        if !any {
            return 0;
        }
        let free_at_once = *free_at_once.get_or_insert_with(ic_dead_ways_free_at_once_enabled);
        let mut retired = 0usize;
        {
            let _writer = self.writer.lock();
            for i in 0..JIT_PIC_ENTRIES {
                if holds_dead(self.class_ids[i].load(Ordering::Acquire)) {
                    self.retire_way_locked(i, deferred);
                    if free_at_once {
                        self.way_retired_gen[i].store(0, Ordering::Release);
                    }
                    retired += 1;
                }
            }
            for index in 0..JIT_MEGA_ENTRIES {
                if holds_dead(self.mega_class_ids[index].load(Ordering::Acquire)) {
                    self.retire_mega_locked(index, deferred);
                    if free_at_once {
                        self.mega_retired_gen[index].store(0, Ordering::Release);
                    }
                    retired += 1;
                }
            }
        }
        retired
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

    /// This site's selector in its VM's [`MegaDispatchTable`], `0` while the
    /// site is unbound ([`Self::bind_mega_dispatch`]).
    #[inline]
    pub fn mega_selector(&self) -> u32 {
        self.mega_selector
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Bind this site to `table` under `selector` (round 12 wave 2, W2-1).
    /// The table word is stored before the selector, so a reader that sees the
    /// selector sees the table. Idempotent: a site's selector is a function of
    /// its own call-site record, so racing binders store the same two values.
    pub fn bind_mega_dispatch(&self, table: &MegaDispatchTable, selector: u32) {
        if selector == 0 {
            return;
        }
        self.mega_dispatch_table
            .store(table.slots_base(), std::sync::atomic::Ordering::Release);
        self.mega_selector
            .store(selector, std::sync::atomic::Ordering::Release);
    }

    /// This site's column in the table's per-class arrays, `0` while unbound
    /// ([`Self::bind_mega_class_slot`]).
    #[inline]
    pub fn mega_class_slot(&self) -> u32 {
        self.mega_class_slot
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Bind this site's column in `table`'s per-class arrays (round 12 wave 4,
    /// lane mega3, M3-1): the root first, the column last, so the hashed
    /// stub's class-slot probe that sees a column sees the root. The caller
    /// binds the selector ([`Self::bind_mega_dispatch`]) before this.
    ///
    /// Only an INDEX is bound here, never a target: a class-slot hit still
    /// compares the way's full `(selector, class id)` key
    /// ([`MegaDispatchTable::key`]), so a column that another resolution
    /// shares -- a wrong or stale vtable slot, a package-private method that
    /// took a new slot in a subclass -- can only miss. A bound column is
    /// never changed, and `0` or one past [`MEGA_CLASS_SLOT_MAX`] binds
    /// nothing.
    pub fn bind_mega_class_slot(&self, table: &MegaDispatchTable, column: u32) {
        use std::sync::atomic::Ordering;
        if column == 0 || column > MEGA_CLASS_SLOT_MAX || self.mega_class_slot() != 0 {
            return;
        }
        self.mega_class_root
            .store(table.class_root_addr(), Ordering::Release);
        self.mega_class_slot.store(column, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Round 12 wave 2 (lane calls, proposal W2-1, step 1): the per-VM shared
// megamorphic dispatch table
// ---------------------------------------------------------------------------

/// `log2` of [`MegaDispatchTable`]'s set count.
pub const MEGA_DISPATCH_SET_BITS: u32 = 8;
/// Sets in a [`MegaDispatchTable`].
pub const MEGA_DISPATCH_SETS: usize = 1 << MEGA_DISPATCH_SET_BITS;
/// Ways per set.
pub const MEGA_DISPATCH_WAYS: usize = 4;
/// Ways in the whole table.
pub const MEGA_DISPATCH_ENTRIES: usize = MEGA_DISPATCH_SETS * MEGA_DISPATCH_WAYS;
/// Most selectors a table interns; a site past it simply stays unbound.
const MEGA_DISPATCH_SELECTOR_CAP: usize = 1 << 16;

/// One VM's shared megamorphic dispatch table: `(receiver class id, resolved
/// method selector) -> compiled entry`, shared by every call site of the VM
/// that resolves the same method (round 12 wave 2, lane calls, proposal W2-1
/// in `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`).
///
/// # Why
///
/// A site's own [`JitPICSlot`] holds four inline ways and a sixteen-way hashed
/// table, and a receiver past them pays the Rust helper's full resolution on
/// each new `(site, class, thread)` and its prelude on every call. The target a
/// virtual call selects is a function of the receiver's class and of the
/// RESOLVED method alone (JVMS 5.4.6), not of the site, so one table keyed on
/// the pair serves every site that resolves that method.
///
/// # Key
///
/// `(selector << 32) | class_id`, with `selector >= 1` from
/// [`Self::selector_id_in_context`], which interns the site record's class
/// name, method name, descriptor, invoke kind, its RESOLUTION CONTEXT and the
/// substituted owner id -- every input the helper's resolution reads. So two
/// sites share a selector only when the helper would resolve them identically,
/// and no key is ever `0` or one of the two sentinels (their high halves are
/// `u32::MAX`, past any selector).
///
/// The context was the caller's class id until round 12 wave 4; since lane
/// mega3's M3-2 the publisher passes the caller's DEFINING LOADER instead
/// ([`mega_selector_loader_context`]), because the helper reads the caller only
/// through `find_class_by_name_for_class`, i.e. through its loader. Loader ids
/// are never reused (a process-wide monotonic counter), and a loader context
/// (`1 << 32 | loader`) can never equal a class-id context (`< 1 << 32`).
///
/// # Per-class columns (round 12 wave 4, lane mega3, M3-1)
///
/// Beside the hashed ways, a table keeps one array per RECEIVER class, indexed
/// by a column: for an `invokevirtual` site, the resolved method's vtable slot
/// plus one, bound on the site's [`JitPICSlot`]. Each cell is a way of the same
/// shape and protocol as the hashed ways (full key, tagged word, owner, retire
/// generation), so the column is only an index: a cell is called only when its
/// key equals the reading site's `(selector, class id)`, exactly as a hashed
/// way is. Two resolutions that share a column for one class (another
/// selector of the same method, a package-private method that took a new slot,
/// a column bound before a redefinition moved the slot) take turns: the first
/// publication holds the cell and the other falls back to the hashed ways. The
/// machine reader is `runtime_lowering::emit_mega_class_slot_probe`:
///
/// ```text
///   root -> directory [len, array(cid 0), array(cid 1), ...]
///   array  [columns, 0, key(1), word(1), key(2), word(2), ...]
/// ```
///
/// Arrays and directories are allocated under `writer`, published after they
/// are initialised, and never moved (a grown directory is a new copy; the old
/// one stays readable for a reader in flight). Nothing is freed before the
/// table is dropped except an UNLOADED class's array, and only once the
/// retirement stamped after its row was zeroed is graced (round 12 wave 7,
/// [`Self::retire_receiver_classes`]). Every retirement pass below reaches the
/// cells as it reaches the hashed ways.
///
/// # Owner index (round 12 wave 4, lane mega3, M3-3)
///
/// `writer`'s state maps every live way's entry to the ways naming it, so
/// the supersede walk ([`Self::retire_entry`]) and an invalidation
/// ([`Self::invalidate_targets`]) touch only the named bodies' ways instead of
/// scanning every way of every array under the lock. The redefinition and
/// full-clear passes still sweep everything. `CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX=0`
/// makes every pass sweep (the index is kept either way).
///
/// # Protocol
///
/// Exactly the hashed table's in [`JitPICSlot`]: a way is published word
/// first and key last, never overwritten while live, retired key first (to
/// [`Self::RETIRED_KEY`]) then word, reused only once its retirement is graced;
/// strong owners are kept per way and released through the retirement queue;
/// all writers serialize on `writer`. Readers take no lock.
///
/// # Readers
///
/// Two since round 12 wave 3 (lane mega2, W2-1 step 2):
///
/// * the Rust helper (`jit_invoke_virtual_mic`'s megamorphic arm), which
///   validates every hit against its owner's `superseded`/`retired` flags and
///   the redefinition epoch before calling it;
/// * the hashed stub's machine-code probe
///   (`runtime_lowering::emit_mega_dispatch_table_probe`, on the per-site
///   hashed table's miss, reading [`JitPICSlot::MEGA_DISPATCH_TABLE_OFFSET`] /
///   [`JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET`] and [`Self::slots_base`]),
///   which validates NOTHING: it calls the word of any way whose key matches.
///
/// So the second reader makes the retirement passes load-bearing, exactly as
/// they are for [`JitPICSlot`]'s hashed ways: every `JitCache` pass that
/// retires inline-cache ways must retire this table's ways too, key first,
/// before the state that dooms them can be observed by a new call; and a way's
/// layout (key at `+16i`, tagged word at `+16i + 8`, four ways per set,
/// [`Self::set_base`], [`Self::key`]) is part of the machine-code contract,
/// pinned by `runtime_lowering`'s `r12w3_mega2_table_probe_tests`, which run
/// the emitted probe against this table (and the class arrays' layout by
/// `r12w4_mega3_class_slot_probe_tests`).
pub struct MegaDispatchTable {
    /// `2 * MEGA_DISPATCH_ENTRIES` words: way `i`'s key at `2i`, its tagged
    /// entry word ([`jit_ic_entry_word`]) at `2i + 1`. Boxed once, never moved.
    slots: Box<[std::sync::atomic::AtomicU64]>,
    owners: Box<[parking_lot::Mutex<Option<Arc<CompiledMethod>>>]>,
    retired_gen: Box<[std::sync::atomic::AtomicU64]>,
    /// The key each way held when it was last retired: that key alone may
    /// take the way again before the retirement is graced (round 12 wave 7,
    /// [`same_key_refill_enabled`]). Writers only, under `writer`.
    retired_keys: Box<[std::sync::atomic::AtomicU64]>,
    /// [`same_key_refill_enabled`], read once per table.
    same_key_refill: bool,
    /// [`mega_table_reclaim_enabled`], read once per table.
    reclaim_dead_arrays: bool,
    /// [`mega_table_superseded_rollback_enabled`], read once per table.
    superseded_rollback: bool,
    /// [`mega_table_dir_reclaim_enabled`], read once per table.
    reclaim_old_dirs: bool,
    /// [`ic_dead_ways_free_at_once_enabled`], read once per table (round 13
    /// wave 10, lane mega8; see [`Self::retire_receiver_classes`]).
    free_dead_receiver_ways: bool,
    /// Serializes every writer, and holds what only writers read: the class
    /// arrays, the directories and the owner index.
    writer: parking_lot::Mutex<MegaWriterState>,
    selectors: parking_lot::Mutex<rustc_hash::FxHashMap<MegaSelectorKey, u32>>,
    /// Next selector id to hand out (round 13 wave 4, lane mega3,
    /// `r13w2-mega-dead-loader-selectors-patch`): monotonic, so an id forgotten
    /// with a dead loader ([`Self::forget_loader_selectors`]) is never
    /// reissued. Read and advanced only under `selectors`.
    next_selector: std::sync::atomic::AtomicU32,
    /// The current class directory's address (see "Per-class columns"). Boxed
    /// so its own address, which sites bind, never moves.
    class_root: Box<std::sync::atomic::AtomicUsize>,
    /// [`mega_table_owner_index_enabled`], read once per table.
    owner_index: bool,
    /// `CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`, read once per table (per VM): the
    /// minimum interval between two inline-cache grace handshake requests,
    /// `0` = never request (the default). JIT round 13 wave 11 lane mega9's
    /// patch, applied in round 14 wave 1 by lane codecache; see
    /// [`Self::claim_grace_request`].
    grace_request_interval_ms: u64,
    /// When this VM last asked for a grace handshake, in milliseconds since
    /// `born` plus one (`0`: never).
    grace_request_at_ms: std::sync::atomic::AtomicU64,
    /// The table's birth, the origin of `grace_request_at_ms`.
    born: std::time::Instant,
}

/// `CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS` (default `0` = off; a positive value is
/// the per-VM minimum interval, in milliseconds, between two requests). An
/// inline-cache install refused only for a lagging grace asks for a handshake
/// pause, whose cooperative arm graces every retired way
/// (`r13w11-mega9-ic-grace-handshake-patch-FIXED-20260929.md`). Read once per table.
fn ic_grace_handshake_interval_ms() -> u64 {
    cratonvm_types::flags::runtime_var("CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// `(class name, method name, descriptor, invoke kind, resolution context,
/// substituted owner id)`: see [`MegaDispatchTable`]'s "Key".
type MegaSelectorKey = (Box<str>, Box<str>, Box<str>, u8, u64, u32);

/// Most columns one class array holds; a vtable longer than this leaves its
/// tail to the hashed ways.
pub const MEGA_CLASS_SLOT_MAX: u32 = 1 << 12;
/// Receiver class ids a directory can cover.
const MEGA_CLASS_DIR_MAX: usize = 1 << 21;
/// Columns over every class array of one table.
const MEGA_CLASS_COLUMNS_TOTAL_MAX: usize = 1 << 18;
/// The smallest directory ever allocated past the empty one.
const MEGA_CLASS_DIR_MIN: usize = 256;

/// The resolution context of a caller whose defining loader has the native id
/// `loader` (`ClassLoaderId::to_native_id`), for
/// [`MegaDispatchTable::selector_id_in_context`] (round 12 wave 4, lane mega3,
/// M3-2). Disjoint from every caller-class-id context, which is below `1 << 32`.
#[inline]
pub const fn mega_selector_loader_context(loader: u32) -> u64 {
    (1u64 << 32) | loader as u64
}

/// `CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX` (default on; `0` retires by sweeping
/// every way of the table, hashed and per-class, as round 12 wave 3 did).
/// Round 12 wave 4, lane mega3, M3-3. Read once per table.
fn mega_table_owner_index_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX")
}

/// `CRATONVM_JIT_IC_SAME_KEY_REFILL` (default on; `0` keeps every retired way
/// and cell parked until its retirement is graced, the round-12-wave-6 rule).
/// Round 12 wave 7, lane mega6: a way or cell retired from key `K` -- a
/// [`JitPICSlot`] way's receiver class, a [`MegaDispatchTable`] way's or
/// cell's `(selector, class)` key -- may be published under that same `K`
/// again before the grace period ends. Read once per slot and once per table,
/// when each is made.
///
/// Why it matters: grace needs every running thread to return to compiled
/// depth 0, and a thread inside an OSR loop never does. Every callee that
/// tiered up while such a loop ran had its ways and its class cell retired by
/// the supersede and could not take them back; its receivers fell to the
/// site's hashed ways or the shared table for the life of the VM (nothing
/// reaches the helper again to re-publish the cell), so a megamorphic site
/// ran a different, longer probe chain per receiver class and its cost grew
/// with the class count (`r12w7-mega6-retired-cells-never-refill-20260927.md`).
pub(crate) fn same_key_refill_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_SAME_KEY_REFILL")
}

/// `CRATONVM_JIT_MEGA_TABLE_RECLAIM` (default on; `0` keeps an unloaded
/// class's cell array allocated for the table's life, the round-12-wave-4
/// behaviour). Round 12 wave 7, lane mega6: `retire_receiver_classes` buries
/// the arrays of the classes it retires and frees them once their stamp is
/// graced, returning their columns to `MEGA_CLASS_COLUMNS_TOTAL_MAX`. Read
/// once per table.
fn mega_table_reclaim_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_RECLAIM")
}

/// `CRATONVM_JIT_MEGA_TABLE_SUPERSEDED_ROLLBACK` (default on; `0` rolls back
/// only a RETIRED body's publication, as before). Round 12 wave 7, lane
/// mega6: [`MegaDispatchTable::install_with_class_slot`] also withdraws a
/// publication of a body a supersede displaced while the publisher was
/// resolving it. Read once per table.
fn mega_table_superseded_rollback_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_SUPERSEDED_ROLLBACK")
}

/// `CRATONVM_JIT_MEGA_TABLE_DIR_RECLAIM` (default on; `0` keeps every class
/// directory a grown copy replaced for the table's life, as before). Round 13
/// wave 4, lane mega3: a replaced directory is buried with a retirement stamp
/// taken after `class_root` was re-pointed and freed once that stamp is graced
/// (`r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md` item 2). Read once
/// per table.
fn mega_table_dir_reclaim_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_TABLE_DIR_RECLAIM")
}

/// `CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS` (default on; `0` leaves a site's
/// MIC and PIC ways keyed on an unloaded receiver class in place, as before).
/// Round 13 wave 8, lane mega7: the class-unload pass
/// (`JitCache::invalidate_unloaded_classes`) empties every published body's
/// MIC and retires its PIC ways that name an unloaded receiver
/// ([`forget_dead_receivers_in`]). Read once per unload pass.
pub(crate) fn ic_forget_dead_receivers_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS")
}

/// Forget, in every inline-cache slot of `body`, the receiver classes `dead`
/// names (round 13 wave 8, lane mega7): its MICs go back to empty
/// (`JitMICSlot::forget_dead_receiver_deferring`) and its PIC ways are retired
/// (`JitPICSlot::retire_receiver_classes_deferring`). Returns how many were
/// forgotten.
pub(crate) fn forget_dead_receivers_in(body: &CompiledMethod, dead: &dyn Fn(u32) -> bool) -> usize {
    // Round 13 wave 10 (lane mega8): the owners of every slot of the body go
    // to the retirement queue in ONE batch ([`defer_jit_owners`]). This pass
    // runs inside the collector's pause, and one queue trip per slot ran one
    // drain per slot, each partitioning the whole queue while any thread is
    // in compiled code.
    let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
    let mics = body
        ._jit_mic_slots
        .iter()
        .filter(|slot| slot.forget_dead_receiver_deferring(dead, &mut deferred))
        .count();
    let mut free_at_once: Option<bool> = None;
    let pics: usize = body
        ._jit_pic_slots
        .iter()
        .map(|slot| slot.retire_receiver_classes_deferring(dead, &mut free_at_once, &mut deferred))
        .sum();
    defer_jit_owners(deferred);
    mics + pics
}

/// `CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS` (default on; `0` hands every owner
/// an inline-cache writer withdrew to the retirement queue one at a time, as
/// before). Round 13 wave 10, lane mega8: see [`defer_jit_owners`]. Read only
/// when a writer withdrew two owners or more.
fn ic_batch_retired_owners_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS")
}

/// `CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE` (default on; `0` keeps a site way
/// or shared-table way withdrawn for an UNLOADED receiver parked until its
/// retirement is graced, as before). Round 13 wave 10, lane mega8: see
/// [`JitPICSlot::retire_receiver_classes_deferring`]. Read once per unload
/// pass over a body and once per [`MegaDispatchTable`].
fn ic_dead_ways_free_at_once_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE")
}

/// Hand the owners one inline-cache writer withdrew to the retirement queue,
/// every one of them already unpublished (its way's guard or key was retired,
/// and its word zeroed, before the owner was taken).
///
/// Round 13 wave 10 (lane mega8): as ONE batch (`enqueue_retired_batch`),
/// which is exactly as sound as one trip per owner -- the batch's single
/// retirement stamp postdates every member's unpublication, and a thread's
/// witness has to reach that one stamp -- and costs one drain instead of
/// one per owner. Each drain made while a thread is in compiled code
/// collects per-thread evidence and partitions the whole queue, so a writer
/// that withdrew `n` owners (a full clear, an unload's receiver pass, a
/// redefinition sweep of a large table) paid O(n x queue) on its own thread,
/// and inside the collector's pause for the unload passes.
/// `CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS=0` restores the loop.
pub(crate) fn defer_jit_owners(deferred: Vec<Arc<CompiledMethod>>) {
    if deferred.len() > 1 && ic_batch_retired_owners_enabled() {
        let batch: Vec<(crate::RetiredCode, usize, u64)> = deferred
            .into_iter()
            .map(|owner| {
                // The same two figures `defer_jit_owner` records.
                let buffer_start = owner.entry as usize;
                let bytes = owner._buffer.capacity() as u64;
                (crate::RetiredCode::Owner(owner), buffer_start, bytes)
            })
            .collect();
        crate::enqueue_retired_batch(batch);
        return;
    }
    for previous in deferred {
        defer_jit_owner(Some(previous));
    }
}

impl JitMICSlot {
    /// Empty this MIC when the receiver class it holds is one `dead` names (a
    /// class unload; round 13 wave 8, lane mega7). Returns whether it did.
    ///
    /// A MIC is monomorphic for its life ([`JitMICSlot::update`] never
    /// retargets a populated slot), so one filled by a class that later
    /// unloaded served nobody again: every receiver of the site took the PIC
    /// or the helper, and a site that turned monomorphic on the NEXT
    /// generation of classes (a redeployed application's) never got its
    /// monomorphic fast path back. Emptying is sound where retargeting is not:
    /// the retarget hazard is a reader that matched the OLD class and loads the
    /// NEW word, and no reader can hold a receiver of an unloaded class. The
    /// class id goes through [`Self::INSTALLING_CLASS_ID`] (which no receiver
    /// matches and which a concurrent `update` waits out) while the word and
    /// its owner are withdrawn, exactly as `clear_compiled_entry` does.
    ///
    /// Production goes through [`Self::forget_dead_receiver_deferring`] (round
    /// 13 wave 10); this one-slot form is the tests'.
    #[cfg(test)]
    pub(crate) fn forget_dead_receiver(&self, dead: &dyn Fn(u32) -> bool) -> bool {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let forgotten = self.forget_dead_receiver_deferring(dead, &mut deferred);
        defer_jit_owners(deferred);
        forgotten
    }

    /// `Self::forget_dead_receiver`, handing the withdrawn owner to
    /// `deferred` instead of the retirement queue (round 13 wave 10, lane
    /// mega8: [`forget_dead_receivers_in`] queues a body's owners in one batch).
    fn forget_dead_receiver_deferring(
        &self,
        dead: &dyn Fn(u32) -> bool,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let current = self.cached_class_id.load(Ordering::Acquire);
        if !JitPICSlot::is_live_class_id(current) || !dead(current) {
            return false;
        }
        if self
            .cached_class_id
            .compare_exchange(
                current,
                Self::INSTALLING_CLASS_ID,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        let owner = {
            let mut slot_owner = self.compiled_owner.lock();
            self.cached_entry_word.store(0, Ordering::SeqCst);
            slot_owner.take()
        };
        *self.cached_class_name.lock() = None;
        self.cached_class_id
            .store(Self::EMPTY_CLASS_ID, Ordering::Release);
        if let Some(owner) = owner {
            deferred.push(owner);
        }
        true
    }
}

/// Round 13 wave 4 (lane mega3, proposal M13-3): the `JvmThread` displacement
/// of the native-stack floor word, when a compiled inline-cache stack check can
/// read it through the `JIT_THREAD` TLS mirror (`MOV RAX, <seg>:[mirror];
/// TEST RAX, RAX; JZ <exact>; CMP RSP, [RAX + off]; JBE <exact>`), or `None`
/// to keep the frame-cached floor. Needs the VM to publish the displacement
/// (`CRATONVM_JIT_STACK_FLOOR_WORD`, default on), a mirror displacement
/// (`CRATONVM_JIT_TLS_THREAD_FETCH`), and the leaf floor helper the check's
/// exact side calls for a thread with no mirror. One definition, read by both
/// the site (`x64/op_invoke.rs`, the IC stack-guard block) and the frame
/// layout (`x64/driver.rs`, `reserve_stack_floor`), so a method never reserves
/// a slot its sites do not read, nor reads one it did not reserve.
pub(crate) fn ic_stack_floor_word_offset(helpers: &crate::JitRuntimeHelpers) -> Option<i32> {
    if helpers.jit_stack_floor_offset_in_thread == 0
        || helpers.native_stack_floor_fn == 0
        || crate::x64::jit_thread_tls_disp() == 0
    {
        return None;
    }
    i32::try_from(helpers.jit_stack_floor_offset_in_thread).ok()
}

/// Most outgoing stack words (context ABI) an inline-cache hit or the hashed
/// stub marshalled before round 13 wave 11.
const IC_STACK_WORDS_NARROW: usize = 4;
/// The cap since round 13 wave 11 (lane mega9, proposal M13-8): a receiver
/// plus eight arguments on Win64, plus ten on SysV.
const IC_STACK_WORDS_WIDE: usize = 8;

/// The ONE cap on the outgoing stack words an inline-cache hit or the hashed
/// stub marshals, read by the single-pass MIC (`x64/op_invoke.rs`
/// `ic_stack_arg_block`), the optimizing tier's cache admission and hit block
/// (`ir_lower.rs` `ir_ic_admits_arity` / `ic_wide_stack_block`, asked by the
/// planner too) and the stub (`runtime_lowering.rs` `hashed_stub_admission`),
/// so a site that has a cache can have the stub. A bound on code size at one
/// site, not an ABI limit: every compiled prologue reads any number of caller
/// stack words, and every consumer is parametric in it.
///
/// `CRATONVM_JIT_IC_WIDE_STACK_WORDS` (default on; `0` restores the cap of 4,
/// under which a wider site pays the resolving or dispatch helper on every
/// call, `r12w3-mega2-...` shape 2). Read per site compile.
pub(crate) fn ic_max_stack_words() -> usize {
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_WIDE_STACK_WORDS") {
        IC_STACK_WORDS_WIDE
    } else {
        IC_STACK_WORDS_NARROW
    }
}

/// `CRATONVM_JIT_IC_PROFILE_MEGA_SEED` (default on; `0` allocates every
/// planned slot with its megamorphic flag clear, as before). Round 13 wave 11,
/// lane mega9: [`JitPICSlot::seed_megamorphic_from_profile`]. Read per planned
/// slot at compile time.
fn profile_mega_seed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_PROFILE_MEGA_SEED")
}

/// Whether a call site's receiver profile shows a megamorphic site (proposal
/// M13-9): at least `profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS`
/// observations, more distinct receiver classes than [`JIT_PIC_ENTRIES`], and
/// the classes past the four most frequent holding at least a sixteenth of
/// them. The runtime rule ([`JitPICSlot::install`]'s overflow) flags a site at
/// its fifth live class however rare; the profile is older than the code it
/// seeds, so a class seen a few times during warm-up does not count here.
pub(crate) fn profile_names_megamorphic_site(counts: &crate::profile::ReceiverCounts) -> bool {
    let mut seen: Vec<u64> = counts
        .values()
        .filter(|&&count| count > 0)
        .map(|&count| u64::from(count))
        .collect();
    if seen.len() <= JIT_PIC_ENTRIES {
        return false;
    }
    let total: u64 = seen.iter().sum();
    if total < u64::from(crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS) {
        return false;
    }
    seen.sort_unstable_by(|a, b| b.cmp(a));
    let overflow: u64 = seen.get(JIT_PIC_ENTRIES..).map_or(0, |rest| rest.iter().sum());
    overflow.saturating_mul(16) >= total
}

/// Can an ARRAY be the receiver of a call whose constant pool names
/// `class_name` as the receiver's static type? The optimizing tier's copy
/// (round 13 wave 11, lane mega9, proposal M13-6) of the single-pass
/// `x64::inlining::static_receiver_admits_arrays` (`pub(super)` there), which
/// `x64/op_invoke.rs`'s `r13w11_mega9_kind_screen_tests` pins equal to it. An
/// array is assignable only to `Object`, `Cloneable` and `Serializable`
/// (JVMS 4.10.1.2); an array owner (`[I.clone()`) and an unknown (empty) one
/// answer `true`. Asked of `invokevirtual` sites only: an interface site
/// always screens (the verifier treats interface types as `Object`).
pub(crate) fn ic_static_receiver_admits_arrays(class_name: &str) -> bool {
    class_name.is_empty()
        || class_name.starts_with('[')
        || matches!(
            class_name,
            "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
        )
}

/// One way of a [`MegaDispatchTable`]: a hashed way by index, or a per-class
/// cell by `(receiver class id, column)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MegaWay {
    Shared(usize),
    Class { class_id: u32, column: u32 },
}

/// One receiver class's cells. `words` is what machine code reads:
/// `[columns, 0, key(1), word(1), .., key(n), word(n)]`, so column `c`'s key is
/// at byte `16c` and its word at `16c + 8`, and `c <= columns` is the bound.
/// `owners` and `retired_gen` are indexed by `c - 1`.
struct ClassSlotArray {
    words: Box<[std::sync::atomic::AtomicU64]>,
    owners: Box<[parking_lot::Mutex<Option<Arc<CompiledMethod>>>]>,
    retired_gen: Box<[std::sync::atomic::AtomicU64]>,
    /// The key each cell held when it was last retired, indexed by `c - 1`
    /// like `owners` (round 12 wave 7, [`same_key_refill_enabled`]).
    retired_keys: Box<[std::sync::atomic::AtomicU64]>,
}

impl ClassSlotArray {
    fn new(columns: u32) -> Box<Self> {
        use std::sync::atomic::AtomicU64;
        let n = columns as usize;
        Box::new(Self {
            words: (0..2 * (n + 1))
                .map(|i| {
                    AtomicU64::new(if i == 0 {
                        n as u64
                    } else if i >= 2 && i % 2 == 0 {
                        MegaDispatchTable::EMPTY_KEY
                    } else {
                        0
                    })
                })
                .collect(),
            owners: (0..n).map(|_| parking_lot::Mutex::new(None)).collect(),
            retired_gen: (0..n).map(|_| AtomicU64::new(0)).collect(),
            retired_keys: (0..n)
                .map(|_| AtomicU64::new(MegaDispatchTable::EMPTY_KEY))
                .collect(),
        })
    }

    /// Column `c`'s last retired key; `None` for column 0 or past the array.
    #[inline]
    fn retired_key(&self, column: u32) -> Option<&std::sync::atomic::AtomicU64> {
        self.retired_keys.get((column as usize).checked_sub(1)?)
    }

    #[inline]
    fn columns(&self) -> u32 {
        // Cast: at most `MEGA_CLASS_SLOT_MAX` owners.
        self.owners.len() as u32
    }

    #[inline]
    fn base(&self) -> usize {
        self.words.as_ptr() as usize
    }

    /// Column `c`'s `(key, word, owner, retire generation)`; `None` for
    /// column 0 (the header) or past the array.
    #[allow(clippy::type_complexity)]
    fn cell(
        &self,
        column: u32,
    ) -> Option<(
        &std::sync::atomic::AtomicU64,
        &std::sync::atomic::AtomicU64,
        &parking_lot::Mutex<Option<Arc<CompiledMethod>>>,
        &std::sync::atomic::AtomicU64,
    )> {
        let c = column as usize;
        let index = c.checked_sub(1)?;
        Some((
            self.words.get(2 * c)?,
            self.words.get(2 * c + 1)?,
            self.owners.get(index)?,
            self.retired_gen.get(index)?,
        ))
    }
}

/// What only [`MegaDispatchTable`]'s writers touch, under its `writer` lock.
#[derive(Default)]
struct MegaWriterState {
    /// Every live way, by the entry its word names (M3-3).
    by_entry: rustc_hash::FxHashMap<usize, Vec<MegaWay>>,
    /// Every directory ever published, the current one last. Each is
    /// `[len, array base of class 0, .., of class len - 1]`, `0` = none.
    dirs: Vec<Box<[std::sync::atomic::AtomicUsize]>>,
    /// Every class array, by receiver class id. Removed only when its class
    /// is unloaded (round 12 wave 7), into `graveyard`.
    classes: rustc_hash::FxHashMap<u32, Box<ClassSlotArray>>,
    /// Columns allocated over `classes` and `graveyard`.
    columns: usize,
    /// Arrays of unloaded classes, each with the retirement generation
    /// stamped after its directory row was zeroed: freed once that
    /// generation is graced, when no reader can still be between a load of
    /// the row and a load of the array (round 12 wave 7, lane mega6,
    /// `r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md` item 1).
    graveyard: Vec<(u64, Box<ClassSlotArray>)>,
    /// Directories a grown copy replaced, each with the retirement generation
    /// stamped after `class_root` was re-pointed: freed once that generation
    /// is graced, when no reader can still hold the old root (round 13 wave 4,
    /// lane mega3, `r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md` item
    /// 2; [`mega_table_dir_reclaim_enabled`]). Empty with the switch off, when
    /// `dirs` keeps every directory instead.
    dir_graveyard: Vec<(u64, Box<[std::sync::atomic::AtomicUsize]>)>,
}

impl MegaWriterState {
    fn index_add(&mut self, entry: usize, way: MegaWay) {
        let ways = self.by_entry.entry(entry).or_default();
        if !ways.contains(&way) {
            ways.push(way);
        }
    }

    fn index_remove(&mut self, entry: usize, way: MegaWay) {
        if let Some(ways) = self.by_entry.get_mut(&entry) {
            ways.retain(|w| *w != way);
            if ways.is_empty() {
                self.by_entry.remove(&entry);
            }
        }
    }

    /// The current directory's length.
    fn dir_len(&self) -> usize {
        self.dirs
            .last()
            .and_then(|d| d.first())
            .map_or(0, |len| len.load(std::sync::atomic::Ordering::Acquire))
    }
}

impl MegaDispatchTable {
    /// A never-published way. No real key has an all-ones high half.
    pub const EMPTY_KEY: u64 = u64::MAX;
    /// A retired way, free again once its retirement is graced.
    pub const RETIRED_KEY: u64 = u64::MAX - 1;

    pub fn new() -> Self {
        // The empty directory (`len` 0) is published from birth, so a bound
        // site never reads a null root.
        let first_dir: Box<[std::sync::atomic::AtomicUsize]> =
            vec![std::sync::atomic::AtomicUsize::new(0)].into_boxed_slice();
        let class_root = Box::new(std::sync::atomic::AtomicUsize::new(
            first_dir.as_ptr() as usize,
        ));
        let mut state = MegaWriterState::default();
        state.dirs.push(first_dir);
        Self {
            slots: (0..2 * MEGA_DISPATCH_ENTRIES)
                .map(|i| {
                    let initial = if i % 2 == 0 { Self::EMPTY_KEY } else { 0 };
                    std::sync::atomic::AtomicU64::new(initial)
                })
                .collect(),
            owners: (0..MEGA_DISPATCH_ENTRIES)
                .map(|_| parking_lot::Mutex::new(None))
                .collect(),
            retired_gen: (0..MEGA_DISPATCH_ENTRIES)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            retired_keys: (0..MEGA_DISPATCH_ENTRIES)
                .map(|_| std::sync::atomic::AtomicU64::new(Self::EMPTY_KEY))
                .collect(),
            same_key_refill: same_key_refill_enabled(),
            reclaim_dead_arrays: mega_table_reclaim_enabled(),
            superseded_rollback: mega_table_superseded_rollback_enabled(),
            reclaim_old_dirs: mega_table_dir_reclaim_enabled(),
            free_dead_receiver_ways: ic_dead_ways_free_at_once_enabled(),
            writer: parking_lot::Mutex::new(state),
            selectors: parking_lot::Mutex::new(rustc_hash::FxHashMap::default()),
            next_selector: std::sync::atomic::AtomicU32::new(1),
            class_root,
            owner_index: mega_table_owner_index_enabled(),
            grace_request_interval_ms: ic_grace_handshake_interval_ms(),
            grace_request_at_ms: std::sync::atomic::AtomicU64::new(0),
            born: std::time::Instant::now(),
        }
    }

    /// Whether the caller may request an inline-cache grace handshake now: the
    /// switch is on and this VM has not asked within its interval (a CAS on
    /// the last request's time, so one of several racing writers wins). JIT
    /// round 13 wave 11 lane mega9's patch (`r13w11-mega9-ic-grace-handshake-patch`),
    /// applied in round 14 wave 1 by lane codecache. Since round 14 wave 2
    /// (lane mic, M8-1) the refusal that leads here has first been offered to
    /// the per-thread catch-up proof inside [`JitPICSlot::install`] (rate-limited
    /// per thread), so a request usually means some running thread has neither
    /// returned to depth 0 nor entered a miss handler since the retirement: the
    /// pure machine-code spinner only a stop can witness.
    pub fn claim_grace_request(&self) -> bool {
        use std::sync::atomic::Ordering;
        let interval = self.grace_request_interval_ms;
        if interval == 0 {
            return false;
        }
        // Cast: milliseconds since the table was made fit u64 for 584 My.
        let now = self.born.elapsed().as_millis() as u64 + 1;
        let last = self.grace_request_at_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < interval {
            return false;
        }
        self.grace_request_at_ms
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Address of the class-directory root word, which a site binds
    /// ([`JitPICSlot::bind_mega_class_slot`]). Stable for the table's life.
    #[inline]
    pub fn class_root_addr(&self) -> usize {
        &*self.class_root as *const std::sync::atomic::AtomicUsize as usize
    }

    /// The key of `(class_id, selector)`.
    #[inline]
    pub const fn key(class_id: u32, selector: u32) -> u64 {
        ((selector as u64) << 32) | class_id as u64
    }

    /// Index of the first way of `(class_id, selector)`'s set.
    #[inline]
    pub const fn set_base(class_id: u32, selector: u32) -> usize {
        let mixed = class_id ^ selector.rotate_left(16);
        let h = mixed.wrapping_mul(JitPICSlot::MEGA_HASH_MULTIPLIER);
        ((h >> (32 - MEGA_DISPATCH_SET_BITS)) as usize) * MEGA_DISPATCH_WAYS
    }

    /// Address of way 0's key; way `i`'s key is at `+16i`, its word at
    /// `+16i + 8`. Stable for the table's life.
    #[inline]
    pub fn slots_base(&self) -> usize {
        self.slots.as_ptr() as usize
    }

    #[inline]
    fn key_at(&self, way: usize) -> &std::sync::atomic::AtomicU64 {
        &self.slots[2 * way]
    }

    #[inline]
    fn word_at(&self, way: usize) -> &std::sync::atomic::AtomicU64 {
        &self.slots[2 * way + 1]
    }

    /// [`Self::selector_id_in_context`] with the caller's class id as the
    /// context: the round-12-wave-3 key, kept for the tests that pin it.
    #[cfg(test)]
    pub fn selector_id(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        invoke_kind: u8,
        caller_class_id: u32,
        owner_class_id: u32,
    ) -> Option<u32> {
        self.selector_id_in_context(
            class_name,
            method_name,
            descriptor,
            invoke_kind,
            u64::from(caller_class_id),
            owner_class_id,
        )
    }

    /// Intern a call-site record as a selector (`1..`), or `None` past the
    /// cap. `context` is what the helper's resolution reads of the CALLER: its
    /// class id (below `1 << 32`) or, since round 12 wave 4 (M3-2), its
    /// defining loader ([`mega_selector_loader_context`]). Cold: callers
    /// memoize it on the site's [`JitPICSlot`].
    pub fn selector_id_in_context(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        invoke_kind: u8,
        context: u64,
        owner_class_id: u32,
    ) -> Option<u32> {
        let key: MegaSelectorKey = (
            Box::from(class_name),
            Box::from(method_name),
            Box::from(descriptor),
            invoke_kind,
            context,
            owner_class_id,
        );
        let mut map = self.selectors.lock();
        if let Some(&id) = map.get(&key) {
            return Some(id);
        }
        if map.len() >= MEGA_DISPATCH_SELECTOR_CAP {
            return None;
        }
        // Monotonic, never `map.len() + 1`: once a dead loader's selectors are
        // forgotten the length falls and would reissue a live id. Read and
        // advanced under `selectors`, so no wrap and no two keys share an id;
        // `u32::MAX` would make an all-ones key half, which the sentinels own.
        let id = self.next_selector.load(std::sync::atomic::Ordering::Relaxed);
        if id == 0 || id == u32::MAX {
            return None;
        }
        self.next_selector
            .store(id + 1, std::sync::atomic::Ordering::Relaxed);
        map.insert(key, id);
        Some(id)
    }

    /// The selector [`Self::selector_id_in_context`] would answer for this
    /// record if it is ALREADY interned; never interns one. Round 14 wave 7,
    /// lane mega: the VM's shape-4 census (`mic_unbound_table_held` under
    /// `CRATONVM_DBG=mic-prof`) asks whether an unbound site's resolution
    /// could have been a table hit, and must not bind or grow anything to find
    /// out. Cold (allocates the lookup key).
    pub fn interned_selector_in_context(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        invoke_kind: u8,
        context: u64,
        owner_class_id: u32,
    ) -> Option<u32> {
        let key: MegaSelectorKey = (
            Box::from(class_name),
            Box::from(method_name),
            Box::from(descriptor),
            invoke_kind,
            context,
            owner_class_id,
        );
        self.selectors.lock().get(&key).copied()
    }

    /// Forget every selector interned under a loader context whose loader
    /// `dead` names, and retire every hashed way and class cell keyed on one of
    /// them (round 13 wave 4, lane mega3; `r13w2-mega-dead-loader-selectors-patch`).
    /// Returns how many selectors were forgotten.
    ///
    /// Sound because a loader-context selector is bound only by call sites of
    /// classes that loader defined; a dead loader has no class left, so those
    /// sites' bodies were retired by the same unload, and loader ids are never
    /// reused, so no future site interns the same key. Ids are monotonic
    /// (`next_selector`), so a forgotten id is never handed out again
    /// and no way keyed on it can answer a new site. Its ways are withdrawn by
    /// the ordinary protocol (key first, owner through the retirement queue),
    /// so they are free for other keys once graced instead of holding a set
    /// for the table's life. Caller-class contexts (below `1 << 32`) are never
    /// touched.
    pub fn forget_loader_selectors(&self, dead: &dyn Fn(u32) -> bool) -> usize {
        use std::sync::atomic::Ordering;
        let forgotten: rustc_hash::FxHashSet<u32> = {
            let mut map = self.selectors.lock();
            let mut ids = rustc_hash::FxHashSet::default();
            map.retain(|key, id| {
                let context = key.4;
                // Cast: the low half of a loader context is the native loader id.
                let doomed = context >> 32 == 1 && dead((context & 0xFFFF_FFFF) as u32);
                if doomed {
                    ids.insert(*id);
                }
                !doomed
            });
            ids
        };
        if forgotten.is_empty() {
            return 0;
        }
        // Cast: a key's high half is its selector (`Self::key`).
        let doomed_key = |key: u64| {
            key != Self::EMPTY_KEY
                && key != Self::RETIRED_KEY
                && forgotten.contains(&((key >> 32) as u32))
        };
        self.with_writer(|state, deferred| {
            for way in 0..MEGA_DISPATCH_ENTRIES {
                if doomed_key(self.key_at(way).load(Ordering::Acquire)) {
                    self.retire_locked(state, way, deferred);
                }
            }
            let cells: Vec<MegaWay> = state
                .classes
                .iter()
                .flat_map(|(&class_id, array)| {
                    (1..=array.columns())
                        .filter(|&column| {
                            array
                                .cell(column)
                                .is_some_and(|(key, _, _, _)| doomed_key(key.load(Ordering::Acquire)))
                        })
                        .map(move |column| MegaWay::Class { class_id, column })
                        .collect::<Vec<_>>()
                })
                .collect();
            for cell in cells {
                self.retire_way_locked(state, cell, deferred);
            }
        });
        forgotten.len()
    }

    /// The way for `(class_id, selector)`: its decoded `(entry, needs_context)`
    /// and the owner the way retains. `None` on a miss or a zero word. Reads
    /// the hashed ways only: every class-cell publication is also offered to
    /// them ([`Self::install_with_class_slot`]).
    pub fn lookup(
        &self,
        class_id: u32,
        selector: u32,
    ) -> Option<(u64, bool, Option<Arc<CompiledMethod>>)> {
        use std::sync::atomic::Ordering;
        if selector == 0 {
            return None;
        }
        let key = Self::key(class_id, selector);
        let base = Self::set_base(class_id, selector);
        for way in base..base + MEGA_DISPATCH_WAYS {
            if self.key_at(way).load(Ordering::Acquire) != key {
                continue;
            }
            let (entry, needs_context) =
                jit_ic_entry_decode(self.word_at(way).load(Ordering::Acquire));
            if entry == 0 {
                continue;
            }
            let owner = self.owners[way].lock().clone();
            // The owner is read after the word: a way retired in between has
            // taken its owner, and the caller must not call a word it cannot
            // keep alive. A way re-published in between names a different
            // owner, which the entry check below refuses.
            if owner
                .as_ref()
                .is_some_and(|o| o.entry_ptr() as u64 == entry)
            {
                return Some((entry, needs_context, owner));
            }
        }
        None
    }

    /// Publish `(class_id, selector) -> entry` into a free way of its set.
    /// Nothing happens for an entry that cannot be retained, one already
    /// retired, a key already holding exactly this word, or a full set (a
    /// live way is never evicted). A way holding a different word for the key
    /// is retired first. A publication that races an invalidation is rolled
    /// back, as [`JitPICSlot::install`]'s is.
    pub fn install(
        &self,
        class_id: u32,
        selector: u32,
        entry_ptr: u64,
        needs_ctx: bool,
        jdk_only: bool,
    ) {
        self.install_with_class_slot(class_id, selector, None, entry_ptr, needs_ctx, jdk_only);
    }

    /// [`Self::install`], and with `class_slot = Some((column, columns))` the
    /// same publication into `class_id`'s cell at `column` too (round 12
    /// wave 4, lane mega3, M3-1). `columns` sizes the class's array the first
    /// time one is needed (the receiver's vtable length; a later, larger
    /// column is refused). The cell obeys the hashed ways' rules exactly: the
    /// same admission, the same key, written word first and key last, never
    /// overwritten while live -- a cell another selector holds is left to it
    /// -- and the same rollback. The hashed ways are offered the entry either
    /// way, so the Rust reader ([`Self::lookup`]) and a site with no column
    /// see what they saw before.
    pub fn install_with_class_slot(
        &self,
        class_id: u32,
        selector: u32,
        class_slot: Option<(u32, u32)>,
        entry_ptr: u64,
        needs_ctx: bool,
        jdk_only: bool,
    ) {
        use std::sync::atomic::Ordering;
        if selector == 0 || entry_ptr == 0 {
            return;
        }
        let owner = resolve_jit_entry_owner(entry_ptr as usize);
        // An owner is required, not merely admitted: `lookup` hands out only
        // retained entries.
        if owner.is_none()
            || !jit_entry_publishable(entry_ptr, &owner, jdk_only)
            || owner_is_retired(&owner)
        {
            return;
        }
        let word = jit_ic_entry_word(entry_ptr, needs_ctx);
        let key = Self::key(class_id, selector);
        let base = Self::set_base(class_id, selector);
        // Cast: an entry address.
        let target = entry_ptr as usize;
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let mut state = self.writer.lock();
            // Buried arrays of unloaded classes go once graced (round 12
            // wave 7); nothing to do, and one branch, while there are none.
            Self::sweep_graveyard_locked(&mut state, retire_generation_is_graced);
            let mut free: Option<usize> = None;
            let mut refill: Option<usize> = None;
            let mut held = false;
            for way in base..base + MEGA_DISPATCH_WAYS {
                let observed = self.key_at(way).load(Ordering::Acquire);
                if observed == key {
                    if self.word_at(way).load(Ordering::Acquire) == word {
                        held = true;
                        continue;
                    }
                    self.retire_locked(&mut state, way, &mut deferred);
                }
                let observed = self.key_at(way).load(Ordering::Acquire);
                if free.is_none() && self.way_is_free(observed, way) {
                    free = Some(way);
                } else if refill.is_none()
                    && self.refillable(observed, self.retired_keys.get(way), key)
                {
                    refill = Some(way);
                }
            }
            if !held {
                if let Some(way) = free.or(refill) {
                    if let Some(previous) =
                        std::mem::replace(&mut *self.owners[way].lock(), owner.clone())
                    {
                        deferred.push(previous);
                    }
                    self.word_at(way).store(word, Ordering::SeqCst);
                    self.key_at(way).store(key, Ordering::Release);
                    state.index_add(target, MegaWay::Shared(way));
                }
            }
            if let Some((column, columns)) = class_slot {
                self.install_class_cell_locked(
                    &mut state,
                    class_id,
                    column,
                    columns,
                    key,
                    word,
                    &owner,
                    &mut deferred,
                );
            }
            // Round 12 wave 7 (lane mega6): a SUPERSEDED body too. The
            // supersede sets the flag and then retires the body's ways under
            // this lock (`JitCache::put`), so a publication that raced it --
            // a helper that resolved the old body just before -- either lands
            // before that retirement, which withdraws it, or sees the flag
            // here. Before, it stayed: the machine probe validates nothing, so
            // the receiver called the displaced (C1) body for the life of the
            // table, where a site's own `JitPICSlot` withdraws the same race
            // (`note_published_owner`).
            let superseded = self.superseded_rollback
                && owner
                    .as_ref()
                    .is_some_and(|o| o.superseded.load(Ordering::Acquire));
            if owner_is_retired(&owner) || superseded {
                self.retire_matching_locked(
                    &mut state,
                    &[target],
                    |entry| entry == target,
                    &mut deferred,
                );
            }
        }
        defer_jit_owners(deferred);
    }

    /// Publish `word` under `key` into `class_id`'s cell at `column`, making
    /// the class's array (and a directory that covers it) first when there is
    /// none. Returns whether the cell holds `word` afterwards. Caller holds
    /// `writer` and has run the admission.
    #[allow(clippy::too_many_arguments)]
    fn install_class_cell_locked(
        &self,
        state: &mut MegaWriterState,
        class_id: u32,
        column: u32,
        columns: u32,
        key: u64,
        word: u64,
        owner: &Option<Arc<CompiledMethod>>,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if column == 0 || column > MEGA_CLASS_SLOT_MAX || word == 0 {
            return false;
        }
        if !self.class_array_covers_locked(state, class_id, column, columns) {
            return false;
        }
        let observed = state
            .classes
            .get(&class_id)
            .and_then(|array| array.cell(column))
            .map(|(k, w, _, _)| (k.load(Ordering::Acquire), w.load(Ordering::Acquire)));
        let Some((observed_key, observed_word)) = observed else {
            return false;
        };
        if observed_key == key {
            if observed_word == word {
                return true;
            }
            // A recompiled target for this key: withdraw the old one. The
            // same key refills the cell below at once (round 12 wave 7);
            // under `CRATONVM_JIT_IC_SAME_KEY_REFILL=0` it is reused once
            // that retirement is graced, by a later publication.
            Self::retire_class_locked(state, class_id, column, deferred);
        }
        let published = {
            let Some(array) = state.classes.get(&class_id) else {
                return false;
            };
            let Some((cell_key, cell_word, cell_owner, cell_gen)) = array.cell(column) else {
                return false;
            };
            let current = cell_key.load(Ordering::Acquire);
            // A cell holding another selector's live key is that selector's: a
            // live way is never evicted (only a retired and graced one, or an
            // empty one, is free). A cell retired from THIS key takes it back
            // at once (round 12 wave 7, `Self::refillable`): a class has one
            // cell per column, so a cell a recompiled callee's supersede
            // retired inside a long-running compiled loop was otherwise never
            // published again -- the next publication came before the grace,
            // and after it nothing reaches the helper for that receiver.
            let free = current == Self::EMPTY_KEY
                || (current == Self::RETIRED_KEY
                    && retire_generation_is_graced(cell_gen.load(Ordering::Acquire)))
                || self.refillable(current, array.retired_key(column), key);
            if free {
                if let Some(previous) = std::mem::replace(&mut *cell_owner.lock(), owner.clone()) {
                    deferred.push(previous);
                }
                cell_word.store(word, Ordering::SeqCst);
                cell_key.store(key, Ordering::Release);
            }
            free
        };
        if published {
            let (entry, _) = jit_ic_entry_decode(word);
            // Cast: an entry address.
            state.index_add(entry as usize, MegaWay::Class { class_id, column });
        }
        published
    }

    /// Whether `class_id` has an array with a cell at `column`, allocating the
    /// array (`max(columns, column)` cells, capped) and growing the directory
    /// to cover the class when needed. Caller holds `writer`.
    fn class_array_covers_locked(
        &self,
        state: &mut MegaWriterState,
        class_id: u32,
        column: u32,
        columns: u32,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if let Some(array) = state.classes.get(&class_id) {
            return column <= array.columns();
        }
        let columns = columns.max(column).min(MEGA_CLASS_SLOT_MAX);
        if column > columns
            || state.columns.saturating_add(columns as usize) > MEGA_CLASS_COLUMNS_TOTAL_MAX
        {
            return false;
        }
        // Cast: a u32 class id widens losslessly.
        let index = class_id as usize;
        if index >= MEGA_CLASS_DIR_MAX || !self.directory_covers_locked(state, index) {
            return false;
        }
        let array = ClassSlotArray::new(columns);
        let base = array.base();
        state.columns += columns as usize;
        state.classes.insert(class_id, array);
        // Published after the array is initialised (its construction happens
        // before this release store), into the directory readers now load.
        if let Some(cell) = state.dirs.last().and_then(|dir| dir.get(1 + index)) {
            cell.store(base, Ordering::Release);
        }
        true
    }

    /// Make the current directory cover class index `index`, publishing a
    /// larger copy when it does not. The previous directory stays allocated:
    /// a reader that loaded the root before the swap still reads it, and it
    /// names the same arrays. Caller holds `writer`.
    fn directory_covers_locked(&self, state: &mut MegaWriterState, index: usize) -> bool {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let current = state.dir_len();
        if index < current {
            return true;
        }
        let Some(needed) = index.checked_add(1) else {
            return false;
        };
        let len = needed
            .checked_next_power_of_two()
            .unwrap_or(MEGA_CLASS_DIR_MAX)
            .max(MEGA_CLASS_DIR_MIN)
            .min(MEGA_CLASS_DIR_MAX);
        if needed > len {
            return false;
        }
        let grown: Box<[AtomicUsize]> = {
            let old = state.dirs.last();
            (0..=len)
                .map(|i| {
                    let value = if i == 0 {
                        len
                    } else {
                        old.and_then(|d| d.get(i))
                            .map_or(0, |cell| cell.load(Ordering::Acquire))
                    };
                    AtomicUsize::new(value)
                })
                .collect()
        };
        self.class_root
            .store(grown.as_ptr() as usize, Ordering::Release);
        state.dirs.push(grown);
        if self.reclaim_old_dirs && state.dirs.len() > 1 {
            // Stamped AFTER the root names the new copy: a reader that loads
            // the root later never sees the old ones, and one that loaded it
            // before is inside compiled code until the stamp is graced. The
            // old copies name only arrays the new one names too, or arrays the
            // array graveyard frees under the same grace.
            let stamp = bump_retire_generation();
            let replaced = state.dirs.len() - 1;
            let old: Vec<Box<[AtomicUsize]>> = state.dirs.drain(..replaced).collect();
            state
                .dir_graveyard
                .extend(old.into_iter().map(|dir| (stamp, dir)));
        }
        true
    }

    /// An empty way, or a retired one no reader can still be inside.
    #[inline]
    fn way_is_free(&self, key: u64, way: usize) -> bool {
        key == Self::EMPTY_KEY
            || (key == Self::RETIRED_KEY
                && retire_generation_is_graced(
                    self.retired_gen[way].load(std::sync::atomic::Ordering::Acquire),
                ))
    }

    /// Whether a way or cell that reads `observed`, and was last retired from
    /// the key in `retired_key`, may take `key` again before its retirement is
    /// graced (round 12 wave 7, lane mega6; [`same_key_refill_enabled`]).
    ///
    /// Grace protects a reader that matched the OLD key and has not loaded the
    /// word yet. When the old key is `key` itself, that reader's receiver and
    /// selector are the ones the new word was resolved for, so it may load
    /// the old word (the retirement queue keeps it mapped), zero (a miss of
    /// this way) or the new word, and each is a correct call. A different key
    /// still waits for the grace: a reader of the old key must never pair with
    /// another key's word. `JitPICSlot::way_refillable` is the same rule for a
    /// site's own ways.
    #[inline]
    fn refillable(
        &self,
        observed: u64,
        retired_key: Option<&std::sync::atomic::AtomicU64>,
        key: u64,
    ) -> bool {
        self.same_key_refill
            && observed == Self::RETIRED_KEY
            && key != Self::EMPTY_KEY
            && key != Self::RETIRED_KEY
            && retired_key.is_some_and(|k| k.load(std::sync::atomic::Ordering::Relaxed) == key)
    }

    /// Withdraw hashed way `way`. Caller holds `writer`.
    fn retire_locked(
        &self,
        state: &mut MegaWriterState,
        way: usize,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        let observed = self.key_at(way).load(Ordering::Acquire);
        if observed == Self::EMPTY_KEY || observed == Self::RETIRED_KEY {
            return;
        }
        let (entry, _) = jit_ic_entry_decode(self.word_at(way).load(Ordering::SeqCst));
        if let Some(retired) = self.retired_keys.get(way) {
            retired.store(observed, Ordering::Relaxed);
        }
        self.key_at(way).store(Self::RETIRED_KEY, Ordering::Release);
        self.word_at(way).store(0, Ordering::SeqCst);
        if let Some(previous) = self.owners[way].lock().take() {
            deferred.push(previous);
        }
        self.retired_gen[way].store(bump_retire_generation(), Ordering::Release);
        // Cast: an entry address.
        state.index_remove(entry as usize, MegaWay::Shared(way));
    }

    /// Withdraw `class_id`'s cell at `column`, key first, exactly as a hashed
    /// way. Caller holds `writer`.
    fn retire_class_locked(
        state: &mut MegaWriterState,
        class_id: u32,
        column: u32,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        use std::sync::atomic::Ordering;
        let entry = {
            let Some(array) = state.classes.get(&class_id) else {
                return;
            };
            let Some((key, word, owner, generation)) = array.cell(column) else {
                return;
            };
            let observed = key.load(Ordering::Acquire);
            if observed == Self::EMPTY_KEY || observed == Self::RETIRED_KEY {
                return;
            }
            let (entry, _) = jit_ic_entry_decode(word.load(Ordering::SeqCst));
            if let Some(retired) = array.retired_key(column) {
                retired.store(observed, Ordering::Relaxed);
            }
            key.store(Self::RETIRED_KEY, Ordering::Release);
            word.store(0, Ordering::SeqCst);
            if let Some(previous) = owner.lock().take() {
                deferred.push(previous);
            }
            generation.store(bump_retire_generation(), Ordering::Release);
            entry
        };
        // Cast: an entry address.
        state.index_remove(entry as usize, MegaWay::Class { class_id, column });
    }

    fn retire_way_locked(
        &self,
        state: &mut MegaWriterState,
        way: MegaWay,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        match way {
            MegaWay::Shared(index) => self.retire_locked(state, index, deferred),
            MegaWay::Class { class_id, column } => {
                Self::retire_class_locked(state, class_id, column, deferred)
            }
        }
    }

    /// Every live way, hashed and per-class, whose word satisfies `matches`
    /// (`matches(0)` is never asked). A sweep: the fallback of the owner index
    /// and the path of the rare whole-table passes. Caller holds `writer`.
    fn live_ways_locked(
        &self,
        state: &MegaWriterState,
        matches: impl Fn(usize, MegaWay) -> bool,
    ) -> Vec<MegaWay> {
        use std::sync::atomic::Ordering;
        let mut ways = Vec::new();
        for way in 0..MEGA_DISPATCH_ENTRIES {
            let (entry, _) = jit_ic_entry_decode(self.word_at(way).load(Ordering::SeqCst));
            // Cast: an entry address.
            if entry != 0 && matches(entry as usize, MegaWay::Shared(way)) {
                ways.push(MegaWay::Shared(way));
            }
        }
        for (&class_id, array) in &state.classes {
            for column in 1..=array.columns() {
                let Some((_, word, _, _)) = array.cell(column) else {
                    continue;
                };
                let (entry, _) = jit_ic_entry_decode(word.load(Ordering::SeqCst));
                let cell = MegaWay::Class { class_id, column };
                // Cast: an entry address.
                if entry != 0 && matches(entry as usize, cell) {
                    ways.push(cell);
                }
            }
        }
        ways
    }

    /// Retire every way whose entry satisfies `matches`: with the owner index,
    /// exactly the ways its rows for `listed` name (every entry `matches`
    /// accepts must be in `listed`); without it, by a sweep that asks
    /// `matches` of every live way. Caller holds `writer`.
    fn retire_matching_locked(
        &self,
        state: &mut MegaWriterState,
        listed: &[usize],
        matches: impl Fn(usize) -> bool,
        deferred: &mut Vec<Arc<CompiledMethod>>,
    ) {
        let ways: Vec<MegaWay> = if self.owner_index {
            listed
                .iter()
                .filter(|&&entry| entry != 0 && matches(entry))
                .filter_map(|entry| state.by_entry.get(entry))
                .flatten()
                .copied()
                .collect()
        } else {
            self.live_ways_locked(state, |entry, _| matches(entry))
        };
        for way in ways {
            self.retire_way_locked(state, way, deferred);
        }
    }

    /// Run `f` under `writer`, then release what it deferred.
    fn with_writer(
        &self,
        f: impl FnOnce(&mut MegaWriterState, &mut Vec<Arc<CompiledMethod>>),
    ) {
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        {
            let mut state = self.writer.lock();
            f(&mut *state, &mut deferred);
        }
        defer_jit_owners(deferred);
    }

    /// Retire every way naming `entry` (a superseded body).
    pub fn retire_entry(&self, entry: usize) {
        if entry == 0 {
            return;
        }
        self.with_writer(|state, deferred| {
            self.retire_matching_locked(state, &[entry], |e| e == entry, deferred)
        });
    }

    /// Retire every way naming one of `targets` (an invalidation).
    pub(crate) fn invalidate_targets(&self, targets: &std::collections::HashSet<usize>) {
        self.with_writer(|state, deferred| {
            // The index's rows the targets name: from the smaller side.
            let listed: Vec<usize> = if !self.owner_index {
                Vec::new()
            } else if targets.len() > state.by_entry.len() {
                state
                    .by_entry
                    .keys()
                    .filter(|&&entry| targets.contains(&entry))
                    .copied()
                    .collect()
            } else {
                targets.iter().copied().collect()
            };
            self.retire_matching_locked(state, &listed, |e| targets.contains(&e), deferred)
        });
    }

    /// Retire every way whose retained owner satisfies `stale` (a redefinition).
    /// A sweep of every way, hashed and per-class.
    pub(crate) fn invalidate_owned_targets(&self, stale: &dyn Fn(&Arc<CompiledMethod>) -> bool) {
        self.with_writer(|state, deferred| {
            let doomed = self.live_ways_locked(state, |_, way| match way {
                MegaWay::Shared(index) => self
                    .owners
                    .get(index)
                    .is_some_and(|owner| owner.lock().as_ref().is_some_and(stale)),
                MegaWay::Class { class_id, column } => state
                    .classes
                    .get(&class_id)
                    .and_then(|array| array.cell(column))
                    .is_some_and(|(_, _, owner, _)| owner.lock().as_ref().is_some_and(stale)),
            });
            for way in doomed {
                self.retire_way_locked(state, way, deferred);
            }
        });
    }

    /// Retire every way and cell whose RECEIVER class `dead` names (a class
    /// unload; round 12 wave 4, applying
    /// `r12w4-hunter2-mega-table-keeps-unloaded-receiver-ways-patch`).
    ///
    /// The unload pass retires by TARGET ([`Self::invalidate_targets`]), which
    /// leaves a way whose target a surviving class declares (an inherited or
    /// default method) keyed on a class id no receiver carries again; a set is
    /// never evicted, so churned loaders would fill it. A dead class's cells go
    /// too, and its row in the current directory is cleared, so the probe stops
    /// there; the array stays allocated for a reader in flight (and is not
    /// reclaimed: `r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md`).
    pub(crate) fn retire_receiver_classes(&self, dead: &dyn Fn(u32) -> bool) {
        use std::sync::atomic::Ordering;
        self.with_writer(|state, deferred| {
            for way in 0..MEGA_DISPATCH_ENTRIES {
                let key = self.key_at(way).load(Ordering::Acquire);
                if key == Self::EMPTY_KEY || key == Self::RETIRED_KEY {
                    continue;
                }
                // Cast: a key's low half is its receiver class id (`Self::key`).
                if dead(key as u32) {
                    self.retire_locked(state, way, deferred);
                    // Round 13 wave 10 (lane mega8): free for another key at
                    // once. The grace protects a reader that matched this key,
                    // and its receiver would be of the unloaded class
                    // (`JitPICSlot::retire_receiver_classes_deferring`). The
                    // dead class's cells and array need no such step: its
                    // array is buried below, and no live key maps to it.
                    if self.free_dead_receiver_ways {
                        self.retired_gen[way].store(0, Ordering::Release);
                    }
                }
            }
            let dead_classes: Vec<u32> =
                state.classes.keys().copied().filter(|&c| dead(c)).collect();
            for &class_id in &dead_classes {
                let columns = state.classes.get(&class_id).map_or(0, |array| array.columns());
                for column in 1..=columns {
                    Self::retire_class_locked(state, class_id, column, deferred);
                }
                // Cast: a u32 class id widens losslessly.
                let row = 1 + class_id as usize;
                if let Some(cell) = state.dirs.last().and_then(|dir| dir.get(row)) {
                    cell.store(0, Ordering::Release);
                }
            }
            if self.reclaim_dead_arrays && !dead_classes.is_empty() {
                // Stamped AFTER the rows were zeroed: a reader that loads a
                // row later finds zero, and one that loaded the array's
                // address before is inside compiled code until the stamp is
                // graced. Older directories still name the arrays, but only a
                // reader that loaded the root before they were replaced can
                // read them, and the same grace covers it. Every cell was
                // retired above, so the arrays retain no owner.
                let stamp = bump_retire_generation();
                for class_id in dead_classes {
                    if let Some(array) = state.classes.remove(&class_id) {
                        state.graveyard.push((stamp, array));
                    }
                }
            }
            Self::sweep_graveyard_locked(state, retire_generation_is_graced);
        });
    }

    /// Free every buried class array whose stamp `graced` accepts, and give
    /// its columns back to the table's budget. Caller holds `writer`.
    fn sweep_graveyard_locked(state: &mut MegaWriterState, graced: impl Fn(u64) -> bool) {
        // Replaced directories (round 13 wave 4, lane mega3): same rule.
        if !state.dir_graveyard.is_empty() {
            state.dir_graveyard.retain(|(stamp, _)| !graced(*stamp));
        }
        if state.graveyard.is_empty() {
            return;
        }
        let mut freed = 0usize;
        state.graveyard.retain(|(stamp, array)| {
            if graced(*stamp) {
                freed = freed.saturating_add(array.columns() as usize);
                false
            } else {
                true
            }
        });
        state.columns = state.columns.saturating_sub(freed);
    }

    /// Retire every way, hashed and per-class.
    pub fn clear_entries(&self) {
        self.with_writer(|state, deferred| {
            for way in 0..MEGA_DISPATCH_ENTRIES {
                self.retire_locked(state, way, deferred);
            }
            let cells: Vec<MegaWay> = state
                .classes
                .iter()
                .flat_map(|(&class_id, array)| {
                    (1..=array.columns()).map(move |column| MegaWay::Class { class_id, column })
                })
                .collect();
            for cell in cells {
                self.retire_way_locked(state, cell, deferred);
            }
            // Every live way was just retired, and each retirement removed its
            // own index row; nothing may survive a full clear.
            state.by_entry.clear();
        });
    }

    /// Ways holding a live key (diagnostics, tests).
    pub fn entries_used(&self) -> usize {
        (0..MEGA_DISPATCH_ENTRIES)
            .filter(|&way| {
                let key = self.key_at(way).load(std::sync::atomic::Ordering::Relaxed);
                key != Self::EMPTY_KEY && key != Self::RETIRED_KEY
            })
            .count()
    }

    /// The class cell `(class_id, column)` for `selector`: its decoded
    /// `(entry, needs_context)`. The Rust mirror of the machine probe, for tests.
    #[cfg(test)]
    pub(crate) fn lookup_class_cell(
        &self,
        class_id: u32,
        selector: u32,
        column: u32,
    ) -> Option<(u64, bool)> {
        use std::sync::atomic::Ordering;
        let state = self.writer.lock();
        let (key, word, _, _) = state.classes.get(&class_id)?.cell(column)?;
        if key.load(Ordering::Acquire) != Self::key(class_id, selector) {
            return None;
        }
        let decoded = jit_ic_entry_decode(word.load(Ordering::Acquire));
        (decoded.0 != 0).then_some(decoded)
    }

    /// `(live class cells, owner-index rows)`, for tests.
    #[cfg(test)]
    pub(crate) fn class_cells_and_index_rows(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering;
        let state = self.writer.lock();
        let live = state
            .classes
            .values()
            .flat_map(|array| (1..=array.columns()).filter_map(move |c| array.cell(c)))
            .filter(|(key, _, _, _)| {
                let key = key.load(Ordering::Relaxed);
                key != Self::EMPTY_KEY && key != Self::RETIRED_KEY
            })
            .count();
        (live, state.by_entry.len())
    }
}

impl Default for MegaDispatchTable {
    fn default() -> Self {
        Self::new()
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

    /// A way whose guard matches but whose word is zero — a profile seed the
    /// helper has not filled in yet, or a way retired between the two stores —
    /// is what generated code sends to the resolving helper. The Rust mirror
    /// must answer the same, and must not charge that way a hit.
    #[test]
    fn a_class_only_way_is_a_miss_not_a_zero_target() {
        let pic = JitPICSlot::new();
        // Publish way 0 exactly as a profile seed does: the class id with no
        // target behind it.
        pic.entry_words[0].store(0, Ordering::SeqCst);
        pic.class_ids[0].store(7, Ordering::Release);
        assert_eq!(pic.entries_used(), 1, "the way holds a live receiver");
        assert!(
            pic.lookup(7).is_none(),
            "a zero word must miss, not dispatch to address 0"
        );
        assert_eq!(pic.hits[0].load(Ordering::Relaxed), 0);
        assert_eq!(pic.misses.load(Ordering::Relaxed), 1);

        // A LATER way holding a real target for the same receiver still wins:
        // the cascade keeps comparing after a zero word takes its miss edge.
        pic.entry_words[1].store(jit_ic_entry_word(0x7000, true), Ordering::SeqCst);
        pic.class_ids[1].store(7, Ordering::Release);
        assert_eq!(pic.lookup(7), Some((0x7000, true)));
        assert_eq!(
            pic.hits[0].load(Ordering::Relaxed),
            0,
            "way 0 served nothing"
        );
        assert_eq!(pic.hits[1].load(Ordering::Relaxed), 1);
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

#[cfg(test)]
mod r11w19_lhm_supersede_retarget_tests {
    //! Round 11 wave 19, lane lhm
    //! (`r11w18-hashmap-inline-caches-keep-a-superseded-callee`): a publication
    //! that supersedes a callee body retires the inline caches that still
    //! publish the old one, so their next call re-resolves to the new body.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};
    use std::sync::atomic::Ordering;

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn holder_body() -> CompiledMethod {
        let mut cm = ret_body();
        cm._jit_mic_slots.push(Box::new(JitMICSlot::new_at(3)));
        cm._jit_mic_slots.push(Box::new(JitMICSlot::new_at(9)));
        cm._jit_pic_slots.push(Box::new(JitPICSlot::new_at(3)));
        cm
    }

    #[test]
    fn a_supersede_retires_the_mic_and_pic_ways_that_publish_the_old_body() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(1);
        let holder_class: Arc<str> = Arc::from("R11w19LhmHolder");
        let callee_class: Arc<str> = Arc::from("R11w19LhmCallee");
        let h_name: Arc<str> = Arc::from("h");
        let m_name: Arc<str> = Arc::from("m");
        let desc: Arc<str> = Arc::from("()V");
        cache.put(
            holder_class.clone(),
            h_name.clone(),
            desc.clone(),
            cid,
            holder_body(),
        );
        cache.put(
            callee_class.clone(),
            m_name.clone(),
            desc.clone(),
            cid,
            ret_body(),
        );
        let holder = cache
            .get(&holder_class, &h_name, &desc, cid)
            .expect("holder published");
        let first = cache
            .get(&callee_class, &m_name, &desc, cid)
            .expect("callee published");
        let mic = &holder._jit_mic_slots[0];
        let pic = &holder._jit_pic_slots[0];
        assert!(mic.holder.get().is_some(), "put binds the holder's MIC");
        assert!(pic.holder.get().is_some(), "put binds the holder's PIC");

        let first_entry = first.entry_ptr() as u64;
        mic.update(7, "Seven", first_entry, false, false);
        pic.install(7, "Seven", first_entry, false, false);
        assert_eq!(mic.cached_entry().0, first_entry);
        assert_eq!(pic.lookup(7), Some((first_entry, false)));
        assert_eq!(
            first.ic_holders.lock().len(),
            2,
            "one record for the MIC, one for the PIC"
        );

        // The supersede.
        cache.put(
            callee_class.clone(),
            m_name.clone(),
            desc.clone(),
            cid,
            ret_body(),
        );
        assert!(first.superseded.load(Ordering::Acquire));
        assert_eq!(
            mic.cached_entry().0,
            0,
            "the MIC still publishes the superseded body"
        );
        assert_eq!(
            mic.cached_class_id.load(Ordering::Acquire),
            7,
            "the MIC keeps its receiver, so its next miss re-resolves in place"
        );
        assert!(
            pic.lookup(7).is_none(),
            "a PIC way still publishes the superseded body"
        );
        assert!(
            pic.lookup_megamorphic(7).is_none(),
            "a hashed-table way still publishes the superseded body"
        );
        assert!(first.ic_holders.lock().is_empty(), "the walk consumed the record");

        // The next resolution publishes the current body into the same MIC.
        let second = cache
            .get(&callee_class, &m_name, &desc, cid)
            .expect("replacement published");
        let second_entry = second.entry_ptr() as u64;
        assert_ne!(second_entry, first_entry);
        mic.update(7, "Seven", second_entry, false, false);
        assert_eq!(mic.cached_entry().0, second_entry);
        assert_eq!(second.ic_holders.lock().len(), 1);

        // A publication of the superseded body that lands AFTER the walk
        // withdraws itself.
        let late = &holder._jit_mic_slots[1];
        late.update(8, "Eight", first_entry, false, false);
        assert_eq!(
            late.cached_entry().0,
            0,
            "a publication of an already-superseded body must not stick"
        );
    }

    #[test]
    fn an_unbound_slot_records_nothing() {
        let callee = ret_body();
        let mic = JitMICSlot::new();
        // No owner resolves for a body that was never registered, and an
        // unbound slot never records: nothing lands on the callee either way.
        mic.update(7, "Seven", callee.entry_ptr() as u64, false, false);
        assert!(callee.ic_holders.lock().is_empty());
        assert!(mic.holder.get().is_none());
    }
}

#[cfg(test)]
mod r12w2_mega_dispatch_table_tests {
    //! Round 12 wave 2, lane calls (proposal W2-1, step 1): the shared
    //! megamorphic dispatch table's publication protocol.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};
    use std::sync::atomic::Ordering;

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    /// A published body (so its entry has an owner the table can retain).
    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x1202);
        let class: Arc<str> = Arc::from("R12w2Mega");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    #[test]
    fn keys_never_collide_with_the_sentinels_and_sets_stay_in_range() {
        for class_id in [0u32, 1, 7, 12_345, u32::MAX - 2] {
            for selector in [1u32, 2, 65_536] {
                let key = MegaDispatchTable::key(class_id, selector);
                assert_ne!(key, MegaDispatchTable::EMPTY_KEY);
                assert_ne!(key, MegaDispatchTable::RETIRED_KEY);
                let base = MegaDispatchTable::set_base(class_id, selector);
                assert_eq!(base % MEGA_DISPATCH_WAYS, 0);
                assert!(base + MEGA_DISPATCH_WAYS <= MEGA_DISPATCH_ENTRIES);
            }
        }
    }

    #[test]
    fn selectors_intern_per_resolution_record() {
        let table = MegaDispatchTable::new();
        let a = table.selector_id("p/A", "f", "()V", 0, 11, 0);
        assert_eq!(a, Some(1));
        assert_eq!(table.selector_id("p/A", "f", "()V", 0, 11, 0), a, "same record");
        // Any field that the resolution reads makes another selector.
        assert_eq!(table.selector_id("p/A", "f", "()V", 2, 11, 0), Some(2));
        assert_eq!(table.selector_id("p/A", "f", "()V", 0, 12, 0), Some(3));
        assert_eq!(table.selector_id("p/A", "g", "()V", 0, 11, 0), Some(4));
    }

    #[test]
    fn install_lookup_retire_and_invalidate() {
        let cache = JitCache::new();
        let body = published(&cache, "m");
        let other = published(&cache, "n");
        let table = MegaDispatchTable::new();
        let entry = body.entry_ptr() as u64;
        // An address with no owner is never published.
        table.install(5, 1, 0x7000, false, false);
        assert!(table.lookup(5, 1).is_none());
        table.install(5, 1, entry, true, false);
        let (got, ctx, owner) = table.lookup(5, 1).expect("hit");
        assert_eq!((got, ctx), (entry, true));
        assert!(owner.is_some_and(|o| Arc::ptr_eq(&o, &body)));
        // Another selector or class misses.
        assert!(table.lookup(5, 2).is_none());
        assert!(table.lookup(6, 1).is_none());
        // Re-installing the same word is a no-op; a different target for
        // the key retires the old way and publishes into another.
        table.install(5, 1, entry, true, false);
        assert_eq!(table.entries_used(), 1);
        table.install(5, 1, other.entry_ptr() as u64, false, false);
        assert_eq!(table.entries_used(), 1);
        assert_eq!(
            table.lookup(5, 1).map(|(e, c, _)| (e, c)),
            Some((other.entry_ptr() as u64, false))
        );
        // Supersede-style retirement by entry.
        table.retire_entry(other.entry_ptr() as usize);
        assert!(table.lookup(5, 1).is_none());
        // Invalidation by target set.
        table.install(9, 3, entry, false, false);
        let mut doomed = std::collections::HashSet::new();
        doomed.insert(entry as usize);
        table.invalidate_targets(&doomed);
        assert!(table.lookup(9, 3).is_none());
        // Redefinition-style retirement by owner, and the full clear.
        table.install(9, 4, entry, false, false);
        table.invalidate_owned_targets(&|o: &Arc<CompiledMethod>| Arc::ptr_eq(o, &body));
        assert!(table.lookup(9, 4).is_none());
        table.install(9, 5, entry, false, false);
        table.clear_entries();
        assert_eq!(table.entries_used(), 0);
        for b in [body, other] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// A retired body is never published, and a full set never evicts a live
    /// way (the resolving helper stays the overflow path).
    #[test]
    fn retired_bodies_and_full_sets_publish_nothing() {
        let cache = JitCache::new();
        let body = published(&cache, "r");
        let table = MegaDispatchTable::new();
        body.retired.store(true, Ordering::SeqCst);
        table.install(5, 1, body.entry_ptr() as u64, false, false);
        assert!(table.lookup(5, 1).is_none());
        body.retired.store(false, Ordering::SeqCst);
        // Fill one set with WAYS keys of distinct classes that hash there.
        let selector = 7;
        let target = MegaDispatchTable::set_base(1, selector);
        let same_set: Vec<u32> = (1u32..200_000)
            .filter(|&c| MegaDispatchTable::set_base(c, selector) == target)
            .take(MEGA_DISPATCH_WAYS + 1)
            .collect();
        assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 1);
        for &c in &same_set {
            table.install(c, selector, body.entry_ptr() as u64, false, false);
        }
        let hits = same_set
            .iter()
            .filter(|&&c| table.lookup(c, selector).is_some())
            .count();
        assert_eq!(hits, MEGA_DISPATCH_WAYS, "the fifth class of a full set is not published");
        table.clear_entries();
        crate::defer_jit_owner(Some(body));
    }

    #[test]
    fn a_site_binds_the_table_word_before_its_selector_at_fixed_offsets() {
        let table = MegaDispatchTable::new();
        let pic = JitPICSlot::new();
        assert_eq!(pic.mega_selector(), 0);
        pic.bind_mega_dispatch(&table, 0);
        assert_eq!(pic.mega_selector(), 0, "selector 0 binds nothing");
        pic.bind_mega_dispatch(&table, 3);
        assert_eq!(pic.mega_selector(), 3);
        let base = &pic as *const JitPICSlot as usize;
        assert_eq!(
            &pic.mega_dispatch_table as *const _ as usize - base,
            JitPICSlot::MEGA_DISPATCH_TABLE_OFFSET
        );
        assert_eq!(
            &pic.mega_selector as *const _ as usize - base,
            JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET
        );
        assert_eq!(
            pic.mega_dispatch_table.load(Ordering::Acquire),
            table.slots_base()
        );
    }
}

#[cfg(test)]
mod r12w4_mega3_class_slot_tests {
    //! Round 12 wave 4, lane mega3: the per-class columns (M3-1), the loader
    //! resolution context (M3-2) and the owner index (M3-3) of the shared
    //! megamorphic table. The machine reader is exercised by
    //! `runtime_lowering::r12w4_mega3_class_slot_probe_tests`.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x1204);
        let class: Arc<str> = Arc::from("R12w4Mega3");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    /// The current directory's address.
    fn current_dir(table: &MegaDispatchTable) -> usize {
        // SAFETY: `class_root_addr` is the address of the table's own boxed
        // root word, alive as long as `table`.
        let root = unsafe { &*(table.class_root_addr() as *const AtomicUsize) };
        root.load(Ordering::Acquire)
    }

    #[test]
    fn a_site_binds_its_column_after_the_root_at_fixed_offsets() {
        let table = MegaDispatchTable::new();
        let pic = JitPICSlot::new();
        let base = &pic as *const JitPICSlot as usize;
        assert_eq!(
            &pic.mega_class_slot as *const _ as usize - base,
            JitPICSlot::MEGA_CLASS_SLOT_OFFSET
        );
        assert_eq!(
            &pic.mega_class_root as *const _ as usize - base,
            JitPICSlot::MEGA_CLASS_ROOT_OFFSET
        );
        // The hashed-stub contract's earlier offsets did not move.
        assert_eq!(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET, 296);
        assert_eq!(JitPICSlot::MEGA_CLASS_SLOT_OFFSET, 300);
        assert_eq!(JitPICSlot::MEGA_CLASS_ROOT_OFFSET, 304);
        pic.bind_mega_class_slot(&table, 0);
        pic.bind_mega_class_slot(&table, MEGA_CLASS_SLOT_MAX + 1);
        assert_eq!(pic.mega_class_slot(), 0, "0 and past the cap bind nothing");
        assert_eq!(pic.mega_class_root.load(Ordering::Acquire), 0);
        pic.bind_mega_class_slot(&table, 7);
        assert_eq!(pic.mega_class_slot(), 7);
        assert_eq!(
            pic.mega_class_root.load(Ordering::Acquire),
            table.class_root_addr()
        );
        pic.bind_mega_class_slot(&table, 9);
        assert_eq!(pic.mega_class_slot(), 7, "a bound column never changes");
        // The root names a published (empty) directory from birth.
        assert_ne!(current_dir(&table), 0);
    }

    #[test]
    fn class_cells_publish_share_by_key_only_and_retire_with_every_pass() {
        let cache = JitCache::new();
        let body = published(&cache, "c1");
        let other = published(&cache, "c2");
        let table = MegaDispatchTable::new();
        let entry = body.entry_ptr() as u64;
        let other_entry = other.entry_ptr() as u64;
        table.install_with_class_slot(5, 1, Some((3, 8)), entry, true, false);
        assert_eq!(table.lookup_class_cell(5, 1, 3), Some((entry, true)));
        assert!(table.lookup(5, 1).is_some(), "the hashed ways are offered it too");
        // A column is an index, not a target: another selector at the same
        // column of the same class is refused (the first publication holds
        // the cell) and never reads the first one's body.
        table.install_with_class_slot(5, 2, Some((3, 8)), other_entry, false, false);
        assert_eq!(table.lookup_class_cell(5, 2, 3), None);
        assert_eq!(table.lookup_class_cell(5, 1, 3), Some((entry, true)));
        // Another class has its own array.
        table.install_with_class_slot(6, 2, Some((3, 8)), other_entry, false, false);
        assert_eq!(table.lookup_class_cell(6, 2, 3), Some((other_entry, false)));
        // Past the array the first publication sized: refused.
        table.install_with_class_slot(5, 1, Some((9, 16)), entry, true, false);
        assert_eq!(table.lookup_class_cell(5, 1, 9), None);
        // Supersede-style retirement by entry reaches the cell.
        table.retire_entry(entry as usize);
        assert_eq!(table.lookup_class_cell(5, 1, 3), None);
        assert!(table.lookup(5, 1).is_none());
        // Invalidation by target set.
        let mut doomed = std::collections::HashSet::new();
        doomed.insert(other_entry as usize);
        table.invalidate_targets(&doomed);
        assert_eq!(table.lookup_class_cell(6, 2, 3), None);
        // Redefinition-style retirement by owner, and the full clear.
        table.install_with_class_slot(7, 4, Some((2, 4)), entry, false, false);
        assert!(table.lookup_class_cell(7, 4, 2).is_some());
        table.invalidate_owned_targets(&|o: &Arc<CompiledMethod>| Arc::ptr_eq(o, &body));
        assert_eq!(table.lookup_class_cell(7, 4, 2), None);
        table.install_with_class_slot(8, 4, Some((2, 4)), entry, false, false);
        assert!(table.lookup_class_cell(8, 4, 2).is_some());
        table.clear_entries();
        assert_eq!(table.lookup_class_cell(8, 4, 2), None);
        assert_eq!(table.class_cells_and_index_rows(), (0, 0));
        for b in [body, other] {
            crate::defer_jit_owner(Some(b));
        }
    }

    #[test]
    fn a_retired_body_is_never_published_into_a_cell() {
        let cache = JitCache::new();
        let body = published(&cache, "c3");
        let table = MegaDispatchTable::new();
        body.retired.store(true, Ordering::SeqCst);
        table.install_with_class_slot(5, 1, Some((1, 4)), body.entry_ptr() as u64, false, false);
        assert_eq!(table.lookup_class_cell(5, 1, 1), None);
        assert_eq!(table.class_cells_and_index_rows(), (0, 0));
        body.retired.store(false, Ordering::SeqCst);
        crate::defer_jit_owner(Some(body));
    }

    /// The owner index names every live way once per entry, and a retirement
    /// through it leaves no row behind.
    #[test]
    fn the_owner_index_tracks_hashed_ways_and_cells() {
        let cache = JitCache::new();
        let a = published(&cache, "i1");
        let b = published(&cache, "i2");
        let table = MegaDispatchTable::new();
        for class_id in 10..40u32 {
            let body = if class_id % 2 == 0 { &a } else { &b };
            let entry = body.entry_ptr() as u64;
            table.install_with_class_slot(class_id, 3, Some((1, 2)), entry, false, false);
        }
        let (cells, rows) = table.class_cells_and_index_rows();
        assert_eq!(cells, 30);
        assert_eq!(rows, 2, "one row per distinct entry");
        table.retire_entry(a.entry_ptr() as usize);
        assert_eq!(table.class_cells_and_index_rows(), (15, 1));
        for class_id in (10..40u32).filter(|c| c % 2 == 1) {
            assert!(table.lookup_class_cell(class_id, 3, 1).is_some());
        }
        table.retire_entry(b.entry_ptr() as usize);
        assert_eq!(table.class_cells_and_index_rows(), (0, 0));
        assert_eq!(table.entries_used(), 0);
        for body in [a, b] {
            crate::defer_jit_owner(Some(body));
        }
    }

    /// A class id past the current directory grows it; the arrays published
    /// before the growth stay reachable through the new directory.
    #[test]
    fn the_directory_grows_and_keeps_every_array() {
        let cache = JitCache::new();
        let body = published(&cache, "d1");
        let table = MegaDispatchTable::new();
        let first = current_dir(&table);
        let entry = body.entry_ptr() as u64;
        table.install_with_class_slot(3, 1, Some((1, 1)), entry, false, false);
        table.install_with_class_slot(5000, 1, Some((1, 1)), entry, false, false);
        let dir = current_dir(&table);
        assert_ne!(dir, first, "the directory was replaced");
        // SAFETY: a published directory is `[len, array..]` and lives as long
        // as the table.
        let len = unsafe { &*(dir as *const AtomicUsize) }.load(Ordering::Acquire);
        assert!(len > 5000);
        assert!(table.lookup_class_cell(3, 1, 1).is_some());
        assert!(table.lookup_class_cell(5000, 1, 1).is_some());
        table.clear_entries();
        crate::defer_jit_owner(Some(body));
    }

    /// An unloaded RECEIVER's ways and cells go even when their target (an
    /// inherited body) survives; other receivers' stay
    /// (`r12w4-hunter2-mega-table-keeps-unloaded-receiver-ways-patch`).
    #[test]
    fn an_unloaded_receiver_class_loses_its_ways_and_cells() {
        let cache = JitCache::new();
        let body = published(&cache, "u1");
        let table = MegaDispatchTable::new();
        let entry = body.entry_ptr() as u64;
        table.install_with_class_slot(7, 1, Some((2, 4)), entry, false, false);
        table.install_with_class_slot(8, 1, Some((2, 4)), entry, false, false);
        assert!(table.lookup(7, 1).is_some() && table.lookup_class_cell(7, 1, 2).is_some());
        table.retire_receiver_classes(&|id: u32| id == 7);
        assert!(table.lookup(7, 1).is_none(), "the dead receiver's hashed way");
        assert_eq!(table.lookup_class_cell(7, 1, 2), None, "the dead receiver's cell");
        assert!(table.lookup(8, 1).is_some(), "another receiver's way survives");
        assert_eq!(table.lookup_class_cell(8, 1, 2), Some((entry, false)));
        assert_eq!(table.class_cells_and_index_rows(), (1, 1));
        table.clear_entries();
        crate::defer_jit_owner(Some(body));
    }

    #[test]
    fn a_loader_context_never_shares_a_selector_with_a_caller_class() {
        let table = MegaDispatchTable::new();
        let loader = |id| mega_selector_loader_context(id);
        let by_class = table.selector_id("p/A", "f", "()V", 0, 3, 0);
        let by_loader = table.selector_id_in_context("p/A", "f", "()V", 0, loader(3), 0);
        assert!(by_class.is_some() && by_loader.is_some());
        assert_ne!(by_class, by_loader);
        // Two callers of one loader share it; another loader does not.
        assert_eq!(
            table.selector_id_in_context("p/A", "f", "()V", 0, loader(3), 0),
            by_loader
        );
        assert_ne!(
            table.selector_id_in_context("p/A", "f", "()V", 0, loader(4), 0),
            by_loader
        );
    }
}

#[cfg(test)]
mod r12w6_mega5_site_state_tests {
    //! Round 12 wave 6, lane mega5: the megamorphic flag the gate reads (W5-4)
    //! and the inline-first publication (W5-6). The machine-code side is
    //! executed in `runtime_lowering::r12w6_mega5_cell_first_tests`.
    use super::*;
    use std::sync::atomic::Ordering;

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x1000 + (u64::from(class_id) << 4)
    }

    fn hashed_holds(pic: &JitPICSlot, class_id: u32) -> bool {
        pic.lookup_megamorphic(class_id).is_some()
    }

    #[test]
    fn the_flag_sits_where_the_gate_reads_it_and_nothing_else_moved() {
        let pic = JitPICSlot::new();
        let base = &pic as *const JitPICSlot as usize;
        assert_eq!(
            &pic.megamorphic as *const _ as usize - base,
            JitPICSlot::MEGAMORPHIC_OFFSET
        );
        assert_eq!(&pic.site_bits as *const _ as usize - base, 52);
        assert_eq!(&pic.hits as *const _ as usize - base, 56);
        assert_eq!(&pic.misses as *const _ as usize - base, 88);
        assert_eq!(
            &pic.mega_class_ids as *const _ as usize - base,
            JitPICSlot::MEGA_CLASS_IDS_OFFSET
        );
        assert_eq!(pic.megamorphic.load(Ordering::Relaxed), 0);
        assert_eq!(pic.site_bits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn five_live_classes_make_a_site_megamorphic_and_nothing_less_does() {
        let pic = JitPICSlot::new();
        for c in 1..=4 {
            pic.install(c, "C", word(c), false, false);
        }
        pic.install(1, "C", word(1), false, false);
        assert!(!pic.is_megamorphic(), "four classes fit");
        // A receiver whose callee recompiled retires its way first; the
        // retired way is not a live fifth class.
        pic.install(2, "C", word(2) + 0x100_0000, false, false);
        assert!(!pic.is_megamorphic());
        // An unpublishable entry (zero) never counts.
        pic.install(9, "C", 0, false, false);
        assert!(!pic.is_megamorphic());
        let full = JitPICSlot::new();
        for c in 1..=5 {
            full.install(c, "C", word(c), false, false);
        }
        assert!(full.is_megamorphic());
        full.clear_entries();
        assert!(full.is_megamorphic(), "sticky across a clear");
        pic.clear_entries();
    }

    #[test]
    fn an_unmarked_site_still_duplicates_its_inline_receivers() {
        let pic = JitPICSlot::new();
        pic.install(3, "C", word(3), false, false);
        assert_eq!(pic.entries_used(), 1);
        assert!(hashed_holds(&pic, 3), "the historical hashed-first order");
        pic.clear_entries();
    }

    #[test]
    fn a_marked_site_publishes_inline_first_until_it_overflows() {
        let pic = JitPICSlot::new();
        pic.note_inline_ways_probed();
        assert!(pic.inline_first());
        for c in 1..=4 {
            pic.install(c, "C", word(c), false, false);
            assert!(!hashed_holds(&pic, c), "class {c} took an inline way only");
        }
        assert_eq!(pic.entries_used(), 4);
        // The overflow: the fifth class goes to the hashed set, the site turns
        // megamorphic, and every live inline receiver gains a hashed way (a
        // full set aside) with its own word.
        pic.install(5, "C", word(5), false, false);
        assert!(pic.is_megamorphic());
        assert!(!pic.inline_first(), "a megamorphic site is hashed-first again");
        for i in 0..JIT_PIC_ENTRIES {
            let (class_id, entry, _) = pic.way(i).expect("a way");
            if JitPICSlot::is_live_class_id(class_id) && entry != 0 {
                let base = JitPICSlot::mega_base_index(class_id);
                let set_full_of_others = (base..base + JIT_MEGA_WAYS).all(|index| {
                    let other = pic.mega_class_ids[index].load(Ordering::Acquire);
                    other != class_id && JitPICSlot::is_live_class_id(other)
                });
                assert!(
                    set_full_of_others
                        || pic.lookup_megamorphic(class_id) == Some((entry, false)),
                    "inline receiver {class_id} was not promoted"
                );
            }
        }
        assert_eq!(pic.lookup_megamorphic(5), Some((word(5), false)));
        pic.clear_entries();

        // A recompiled callee at a full marked site: the old way retires (it
        // is skipped as a candidate in the same pass), so the inline ways
        // refuse the new word and the hashed set takes it; a retired way is
        // not a fifth live class.
        let site = JitPICSlot::new();
        site.note_inline_ways_probed();
        for c in 1..=4 {
            site.install(c, "C", word(c), false, false);
        }
        site.install(1, "C", word(1) + 0x100_0000, false, false);
        assert!(!site.is_megamorphic());
        assert_eq!(site.lookup_megamorphic(1), Some((word(1) + 0x100_0000, false)));
        site.clear_entries();
    }

    /// Inline-first leaves no stale hashed way behind: a hashed way that
    /// still names a receiver with an older target is retired when the
    /// receiver's new target is published inline.
    #[test]
    fn an_inline_first_publication_retires_a_stale_hashed_way() {
        let pic = JitPICSlot::new();
        // Published before the mark: hashed and inline both hold class 6.
        pic.install(6, "C", word(6), false, false);
        assert!(hashed_holds(&pic, 6));
        pic.note_inline_ways_probed();
        // Its callee recompiled: the inline way retires, a free way takes the
        // new target, and the hashed way with the old one goes.
        pic.install(6, "C", word(6) + 0x100_0000, false, false);
        assert_ne!(pic.lookup_megamorphic(6), Some((word(6), false)), "stale way kept");
        pic.clear_entries();
    }
}

#[cfg(test)]
mod r12w7_mega6_refill_tests {
    //! Round 12 wave 7, lane mega6: a way or cell retired from key `K` takes
    //! `K` again before its retirement is graced, and only `K`
    //! (`CRATONVM_JIT_IC_SAME_KEY_REFILL`). Every test holds a JIT execution
    //! token across the retirement and the republication: that is the state
    //! of a mutator inside an OSR loop, and it keeps any other test's drain
    //! from gracing the retirement in between (the process-wide proof needs
    //! no execution in flight; the per-thread one is bounded by this thread's
    //! last return to depth 0, which predates the stamp). The machine-code
    //! side is executed in `runtime_lowering::r12w7_mega6_refill_probe_tests`.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x1000 + (u64::from(class_id) << 4)
    }

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x1207);
        let class: Arc<str> = Arc::from("R12w7Mega6");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    /// The four inline ways of a site whose code probes them, with class 2's
    /// way retired by the supersede walk while a thread is in compiled code.
    fn full_site_with_class_two_retired(refill: bool) -> JitPICSlot {
        let mut pic = JitPICSlot::new();
        pic.same_class_refill = refill;
        pic.note_inline_ways_probed();
        for c in 1..=4 {
            pic.install(c, "C", word(c), false, false);
        }
        assert_eq!(pic.way(1).map(|w| w.0), Some(2));
        assert!(pic.retire_entry(word(2) as usize), "the supersede walk");
        assert_eq!(pic.lookup(2), None);
        pic
    }

    #[test]
    fn a_retired_inline_way_takes_its_own_class_back_before_the_grace() {
        let execution = crate::jit_execution_enter();
        let pic = full_site_with_class_two_retired(true);
        // Another class must not take it: a reader of class 2 may still sit
        // between its compare and its word load. It overflows to the hashed
        // set, and a retired way is not a live fifth class.
        pic.install(9, "C", word(9), false, false);
        assert_eq!(pic.lookup(9), None, "class 9 took the way class 2 was retired from");
        assert_eq!(pic.lookup_megamorphic(9), Some((word(9), false)));
        assert!(!pic.is_megamorphic());
        // Class 2's recompiled body takes its own way back at once.
        let recompiled = word(2) + 0x100_0000;
        pic.install(2, "C", recompiled, false, false);
        assert_eq!(pic.lookup(2), Some((recompiled, false)));
        assert_eq!(pic.way(1).map(|w| (w.0, w.1)), Some((2, recompiled)));
        assert_eq!(pic.lookup_megamorphic(2), None, "inline only, no duplicate");

        // `CRATONVM_JIT_IC_SAME_KEY_REFILL=0`: the way stays parked until the
        // grace, so the recompiled body goes to the hashed set (the behaviour
        // that stranded every callee tiering up inside an OSR loop).
        let off = full_site_with_class_two_retired(false);
        off.install(2, "C", recompiled, false, false);
        assert_eq!(off.lookup(2), None, "the parked way was refilled with the switch off");
        assert_eq!(off.lookup_megamorphic(2), Some((recompiled, false)));
        crate::jit_execution_leave(execution);
        pic.clear_entries();
        off.clear_entries();
    }

    #[test]
    fn a_retired_hashed_way_takes_its_own_class_back_before_the_grace() {
        let target = JitPICSlot::mega_base_index(1);
        let same_set: Vec<u32> = (1u32..100_000)
            .filter(|&c| JitPICSlot::mega_base_index(c) == target)
            .take(JIT_MEGA_WAYS + 1)
            .collect();
        assert_eq!(same_set.len(), JIT_MEGA_WAYS + 1);
        let (a, b, c) = (same_set[0], same_set[1], same_set[2]);
        let execution = crate::jit_execution_enter();
        for refill in [true, false] {
            let mut pic = JitPICSlot::new();
            pic.same_class_refill = refill;
            // An unmarked site: the hashed set first.
            pic.install(a, "A", word(a), false, false);
            pic.install(b, "B", word(b), false, false);
            assert_eq!(pic.lookup_megamorphic(a), Some((word(a), false)));
            assert!(pic.retire_entry(word(a) as usize));
            pic.install(c, "C", word(c), false, false);
            assert_eq!(pic.lookup_megamorphic(c), None, "another class took a retired way");
            let recompiled = word(a) + 0x100_0000;
            pic.install(a, "A", recompiled, false, false);
            let expect = if refill { Some((recompiled, false)) } else { None };
            assert_eq!(pic.lookup_megamorphic(a), expect, "refill {refill}");
            pic.clear_entries();
        }
        crate::jit_execution_leave(execution);
    }

    /// The class cell (one per class and column: nowhere else to go) and a
    /// shared way, retired by a supersede, take their own key back at once;
    /// another key waits for the grace.
    #[test]
    fn a_retired_cell_and_shared_way_take_their_own_key_back_before_the_grace() {
        let cache = JitCache::new();
        let first = published(&cache, "rf-c1");
        let second = published(&cache, "rf-c2");
        let (e1, e2) = (first.entry_ptr() as u64, second.entry_ptr() as u64);
        let execution = crate::jit_execution_enter();
        for refill in [true, false] {
            let mut table = MegaDispatchTable::new();
            table.same_key_refill = refill;
            table.install_with_class_slot(5, 1, Some((3, 8)), e1, false, false);
            assert_eq!(table.lookup_class_cell(5, 1, 3), Some((e1, false)));
            table.retire_entry(e1 as usize);
            assert_eq!(table.lookup_class_cell(5, 1, 3), None);
            // Another selector at the same column of the same class.
            table.install_with_class_slot(5, 2, Some((3, 8)), e2, false, false);
            assert_eq!(table.lookup_class_cell(5, 2, 3), None, "another key took the cell");
            // The same key, with the recompiled body.
            table.install_with_class_slot(5, 1, Some((3, 8)), e2, true, false);
            let expect = if refill { Some((e2, true)) } else { None };
            assert_eq!(table.lookup_class_cell(5, 1, 3), expect, "refill {refill}");
            assert!(table.lookup(5, 1).is_some(), "a free hashed way took it either way");

            // A full shared set: a retired way takes its own key back, and
            // not a fifth key.
            let selector = 7;
            let set = MegaDispatchTable::set_base(1, selector);
            let same_set: Vec<u32> = (1u32..200_000)
                .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
                .take(MEGA_DISPATCH_WAYS + 1)
                .collect();
            assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 1);
            let (head, fifth) = (same_set[0], same_set[MEGA_DISPATCH_WAYS]);
            table.install(head, selector, e1, false, false);
            for &c in &same_set[1..MEGA_DISPATCH_WAYS] {
                table.install(c, selector, e2, false, false);
            }
            table.retire_entry(e1 as usize);
            table.install(fifth, selector, e2, false, false);
            assert!(table.lookup(fifth, selector).is_none(), "a fifth key took a retired way");
            table.install(head, selector, e2, false, false);
            assert_eq!(
                table.lookup(head, selector).is_some(),
                refill,
                "refill {refill}: the head's own way"
            );
            table.clear_entries();
        }
        crate::jit_execution_leave(execution);
        for b in [first, second] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// A publication of a body a supersede displaced while the publisher was
    /// resolving it is withdrawn under the writer lock, from the cell and the
    /// hashed way alike; the current body is published as ever.
    #[test]
    fn a_superseded_body_is_rolled_back_out_of_the_table() {
        let cache = JitCache::new();
        let old = published(&cache, "rf-s1");
        let new = published(&cache, "rf-s1");
        assert!(old.superseded.load(std::sync::atomic::Ordering::Acquire));
        assert!(!Arc::ptr_eq(&old, &new));
        let (e_old, e_new) = (old.entry_ptr() as u64, new.entry_ptr() as u64);
        for rollback in [true, false] {
            let mut table = MegaDispatchTable::new();
            table.superseded_rollback = rollback;
            table.install_with_class_slot(5, 1, Some((1, 2)), e_old, false, false);
            let kept = if rollback { None } else { Some((e_old, false)) };
            assert_eq!(table.lookup_class_cell(5, 1, 1), kept, "rollback {rollback}");
            assert_eq!(table.lookup(5, 1).is_some(), !rollback, "rollback {rollback}");
            table.install_with_class_slot(6, 1, Some((1, 2)), e_new, false, false);
            assert_eq!(table.lookup_class_cell(6, 1, 1), Some((e_new, false)));
            table.clear_entries();
        }
        for b in [old, new] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// An unloaded class's cell array is buried, not freed, until the stamp
    /// taken after its row was zeroed is graced; then its columns go back to
    /// the budget (`r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md`,
    /// item 1). With the switch off it stays, as before.
    #[test]
    fn an_unloaded_class_array_is_freed_once_graced() {
        let cache = JitCache::new();
        let body = published(&cache, "rf-u1");
        let entry = body.entry_ptr() as u64;
        let execution = crate::jit_execution_enter();
        for reclaim in [true, false] {
            let mut table = MegaDispatchTable::new();
            table.reclaim_dead_arrays = reclaim;
            for class_id in 10..20u32 {
                table.install_with_class_slot(class_id, 1, Some((2, 4)), entry, false, false);
            }
            assert_eq!(table.writer.lock().columns, 40);
            table.retire_receiver_classes(&|id: u32| id < 15);
            {
                let state = table.writer.lock();
                let buried = if reclaim { 5 } else { 0 };
                assert_eq!(state.classes.len(), 10 - buried, "reclaim {reclaim}");
                assert_eq!(state.graveyard.len(), buried, "buried, not freed");
                assert_eq!(state.columns, 40, "nothing freed before the grace");
            }
            assert_eq!(table.lookup_class_cell(12, 1, 2), None);
            assert_eq!(table.lookup_class_cell(17, 1, 2), Some((entry, false)));
            {
                let mut state = table.writer.lock();
                MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| false);
                assert_eq!(state.graveyard.len(), if reclaim { 5 } else { 0 });
                MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| true);
                assert!(state.graveyard.is_empty());
                assert_eq!(state.columns, if reclaim { 20 } else { 40 });
            }
            table.clear_entries();
        }
        crate::jit_execution_leave(execution);
        crate::defer_jit_owner(Some(body));
    }
}

#[cfg(test)]
mod r13w4_mega3_selector_tests {
    //! Round 13 wave 4, lane mega3 (`r13w2-mega-dead-loader-selectors-patch`):
    //! a dead loader's selectors are forgotten, their ids are never reissued,
    //! and the ways keyed on them are withdrawn.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x1304);
        let class: Arc<str> = Arc::from("R13w4Mega3");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    #[test]
    fn a_dead_loaders_selectors_are_forgotten_and_never_reissued() {
        let table = MegaDispatchTable::new();
        let loader = mega_selector_loader_context;
        let s7 = table
            .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
            .expect("interned");
        let s7g = table
            .selector_id_in_context("p/A", "g", "()V", 0, loader(7), 0)
            .expect("interned");
        let s8 = table
            .selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0)
            .expect("interned");
        // A caller-CLASS context 7 is not loader 7.
        let c7 = table.selector_id("p/A", "f", "()V", 0, 7, 0).expect("interned");
        assert_eq!(table.forget_loader_selectors(&|l| l == 7), 2);
        assert_eq!(table.forget_loader_selectors(&|l| l == 7), 0, "already forgotten");
        // Survivors keep their ids.
        assert_eq!(table.selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0), Some(s8));
        assert_eq!(table.selector_id("p/A", "f", "()V", 0, 7, 0), Some(c7));
        // A re-intern of the forgotten key gets an id no selector ever had.
        let again = table
            .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
            .expect("interned");
        for old in [s7, s7g, s8, c7] {
            assert_ne!(again, old, "a forgotten id was reissued");
        }
        assert!(again > s7g && again > s8 && again > c7, "ids are monotonic");
    }

    #[test]
    fn a_dead_loaders_ways_and_cells_are_withdrawn() {
        let cache = JitCache::new();
        let body = published(&cache, "fl-1");
        let entry = body.entry_ptr() as u64;
        let table = MegaDispatchTable::new();
        let loader = mega_selector_loader_context;
        let dead = table
            .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
            .expect("interned");
        let live = table
            .selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0)
            .expect("interned");
        table.install_with_class_slot(5, dead, Some((2, 4)), entry, false, false);
        table.install_with_class_slot(6, live, Some((2, 4)), entry, false, false);
        assert!(table.lookup(5, dead).is_some() && table.lookup_class_cell(5, dead, 2).is_some());
        assert_eq!(table.forget_loader_selectors(&|l| l == 7), 1);
        assert!(table.lookup(5, dead).is_none(), "the dead selector's hashed way");
        assert_eq!(table.lookup_class_cell(5, dead, 2), None, "the dead selector's cell");
        assert!(table.lookup(6, live).is_some(), "another loader's way survives");
        assert_eq!(table.lookup_class_cell(6, live, 2), Some((entry, false)));
        assert_eq!(table.class_cells_and_index_rows(), (1, 1));
        table.clear_entries();
        crate::defer_jit_owner(Some(body));
    }

    /// A directory replaced by a grown copy is buried, not kept, and freed
    /// once its stamp is graced (`r12w4-mega3-shared-table-never-reclaims`
    /// item 2); with the switch off every directory stays, as before. The
    /// execution token keeps any other test's drain from gracing the stamp.
    #[test]
    fn a_replaced_directory_is_freed_once_graced() {
        let cache = JitCache::new();
        let body = published(&cache, "fl-dir");
        let entry = body.entry_ptr() as u64;
        let execution = crate::jit_execution_enter();
        for reclaim in [true, false] {
            let mut table = MegaDispatchTable::new();
            table.reclaim_old_dirs = reclaim;
            // The empty birth directory, then one covering class 3, then one
            // covering class 5000.
            table.install_with_class_slot(3, 1, Some((1, 1)), entry, false, false);
            table.install_with_class_slot(5000, 1, Some((1, 1)), entry, false, false);
            {
                let mut state = table.writer.lock();
                let kept = if reclaim { 1 } else { 3 };
                assert_eq!(state.dirs.len(), kept, "reclaim {reclaim}");
                assert_eq!(state.dir_graveyard.len(), 3 - kept, "buried, not freed");
                MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| false);
                assert_eq!(state.dir_graveyard.len(), 3 - kept);
                MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| true);
                assert!(state.dir_graveyard.is_empty());
                assert_eq!(state.dirs.len(), kept, "the current directory stays");
            }
            assert!(table.lookup_class_cell(3, 1, 1).is_some());
            assert!(table.lookup_class_cell(5000, 1, 1).is_some());
            table.clear_entries();
        }
        crate::jit_execution_leave(execution);
        crate::defer_jit_owner(Some(body));
    }

    /// M13-3: the inline-cache stack check reads the thread's floor word only
    /// when the VM published its displacement, the leaf floor helper exists
    /// (the check's exact side) and the thread mirror has a displacement;
    /// anything less keeps the frame-slot form (`None`).
    #[test]
    fn the_floor_word_is_used_only_when_fully_wired() {
        let mut helpers = crate::JitRuntimeHelpers::default();
        assert_eq!(ic_stack_floor_word_offset(&helpers), None, "nothing wired");
        helpers.jit_stack_floor_offset_in_thread = 0x2A8;
        assert_eq!(ic_stack_floor_word_offset(&helpers), None, "no leaf floor helper");
        helpers.native_stack_floor_fn = 0x1150;
        let expect = (crate::x64::jit_thread_tls_disp() != 0).then_some(0x2A8);
        assert_eq!(ic_stack_floor_word_offset(&helpers), expect);
        helpers.jit_stack_floor_offset_in_thread = 0;
        assert_eq!(ic_stack_floor_word_offset(&helpers), None, "switch off: 0 published");
    }
}

#[cfg(test)]
mod r13w8_mega7_dead_receiver_tests {
    //! Round 13 wave 8, lane mega7
    //! (`r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`): a class
    //! unload empties a MIC and retires the PIC ways keyed on the unloaded
    //! receiver, whatever their target; live receivers keep theirs.
    use super::*;
    use std::sync::atomic::Ordering;

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x2000 + (u64::from(class_id) << 4)
    }

    #[test]
    fn an_unloaded_receiver_leaves_the_site_caches() {
        let pic = JitPICSlot::new();
        // An unmarked site: every receiver takes a hashed way and an inline way.
        for c in 1..=4 {
            pic.install(c, "C", word(c), false, false);
        }
        let dead = |c: u32| c == 2 || c == 3;
        let hashed_dead = [2u32, 3]
            .iter()
            .filter(|&&c| pic.lookup_megamorphic(c).is_some())
            .count();
        assert_eq!(
            pic.retire_receiver_classes(&|c| c == 99),
            0,
            "nothing to retire"
        );
        assert_eq!(pic.retire_receiver_classes(&dead), 2 + hashed_dead);
        for c in [2u32, 3] {
            assert_eq!(pic.lookup(c), None, "class {c}'s inline way");
            assert_eq!(pic.lookup_megamorphic(c), None, "class {c}'s hashed way");
        }
        for c in [1u32, 4] {
            assert_eq!(pic.lookup(c), Some((word(c), false)), "live class {c}");
        }
        assert_eq!(pic.entries_used(), 2);
        assert_eq!(pic.retire_receiver_classes(&dead), 0, "already retired");
        pic.clear_entries();

        let mic = JitMICSlot::new();
        mic.update(5, "C", word(5), false, false);
        assert_eq!(mic.cached_entry(), (word(5), false));
        assert!(
            !mic.forget_dead_receiver(&|c| c == 6),
            "a live receiver stays"
        );
        assert!(mic.forget_dead_receiver(&|c| c == 5));
        assert_eq!(
            mic.cached_class_id.load(Ordering::Acquire),
            JitMICSlot::EMPTY_CLASS_ID
        );
        assert_eq!(mic.cached_entry(), (0, false));
        assert!(!mic.forget_dead_receiver(&|c| c == 5), "already empty");
        // Empty again, so the next receiver class gets the monomorphic path
        // back; before, a MIC filled by an unloaded class served nobody again.
        mic.update(8, "D", word(8), true, false);
        assert_eq!(mic.cached_class_id.load(Ordering::Acquire), 8);
        assert_eq!(mic.cached_entry(), (word(8), true));
        mic.clear_compiled_entry();
    }
}

#[cfg(test)]
mod r13w10_mega8_dead_way_tests {
    //! Round 13 wave 10, lane mega8: a way withdrawn for an UNLOADED receiver
    //! is free for another class at once (`CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE`),
    //! while a way withdrawn for any other reason still waits for its grace.
    //! Every test holds a JIT execution token across the retirement and the
    //! republication, the state of a thread spinning in a compiled loop: no
    //! drain of another test can grace the retirement in between.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x3000 + (u64::from(class_id) << 4)
    }

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x130A);
        let class: Arc<str> = Arc::from("R13w10Mega8");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    /// Four inline ways (a site whose code probes them), classes 2 and 3
    /// unloaded: with the switch on a new class takes a freed inline way at
    /// once; with it off it overflows to the hashed set, as before.
    #[test]
    fn an_unloaded_receivers_inline_way_is_free_at_once() {
        let execution = crate::jit_execution_enter();
        for free in [true, false] {
            let pic = JitPICSlot::new();
            pic.note_inline_ways_probed();
            for c in 1..=4 {
                pic.install(c, "C", word(c), false, false);
            }
            let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
            let mut free_at_once = Some(free);
            let retired = pic.retire_receiver_classes_deferring(
                &|c| c == 2 || c == 3,
                &mut free_at_once,
                &mut deferred,
            );
            assert_eq!(retired, 2, "free {free}");
            assert!(deferred.is_empty(), "raw words retain no owner");
            pic.install(9, "C", word(9), false, false);
            let inline = if free { Some((word(9), false)) } else { None };
            assert_eq!(pic.lookup(9), inline, "free {free}: class 9's inline way");
            assert_eq!(
                pic.lookup_megamorphic(9).is_some(),
                !free,
                "free {free}: class 9 overflowed to the hashed set"
            );
            assert!(!pic.is_megamorphic(), "free {free}: a retired way is not live");
            // A way withdrawn for a recompiled callee (not an unload) still
            // waits for the grace, switch or not.
            assert!(pic.retire_entry(word(1) as usize));
            pic.install(11, "C", word(11), false, false);
            let took_way_zero = pic.way(0).is_some_and(|w| w.0 == 11);
            assert!(!took_way_zero, "free {free}: class 11 took a way before its grace");
            pic.clear_entries();
        }
        crate::jit_execution_leave(execution);
    }

    /// A full hashed set of an unmarked site: a way freed by an unload takes
    /// the set's next class at once with the switch on.
    #[test]
    fn an_unloaded_receivers_hashed_way_is_free_at_once() {
        let target = JitPICSlot::mega_base_index(1);
        let same_set: Vec<u32> = (1u32..100_000)
            .filter(|&c| JitPICSlot::mega_base_index(c) == target)
            .take(JIT_MEGA_WAYS + 1)
            .collect();
        assert_eq!(same_set.len(), JIT_MEGA_WAYS + 1);
        let (a, b, c) = (same_set[0], same_set[1], same_set[2]);
        let execution = crate::jit_execution_enter();
        for free in [true, false] {
            let pic = JitPICSlot::new();
            pic.install(a, "A", word(a), false, false);
            pic.install(b, "B", word(b), false, false);
            let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
            let mut free_at_once = Some(free);
            pic.retire_receiver_classes_deferring(&|x| x == a, &mut free_at_once, &mut deferred);
            assert_eq!(pic.lookup_megamorphic(a), None);
            pic.install(c, "C", word(c), false, false);
            let expect = if free { Some((word(c), false)) } else { None };
            assert_eq!(pic.lookup_megamorphic(c), expect, "free {free}");
            assert_eq!(pic.lookup_megamorphic(b), Some((word(b), false)), "b survives");
            pic.clear_entries();
        }
        crate::jit_execution_leave(execution);
    }

    /// The shared table: a full set whose head is unloaded takes a fifth key
    /// at once with the switch on; a supersede's retirement still waits.
    #[test]
    fn an_unloaded_receivers_shared_way_is_free_at_once() {
        let cache = JitCache::new();
        let first = published(&cache, "dw-1");
        let second = published(&cache, "dw-2");
        let (e1, e2) = (first.entry_ptr() as u64, second.entry_ptr() as u64);
        let selector = 3;
        let set = MegaDispatchTable::set_base(1, selector);
        let same_set: Vec<u32> = (1u32..200_000)
            .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
            .take(MEGA_DISPATCH_WAYS + 2)
            .collect();
        assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 2);
        let (head, last, fifth, sixth) = (
            same_set[0],
            same_set[MEGA_DISPATCH_WAYS - 1],
            same_set[MEGA_DISPATCH_WAYS],
            same_set[MEGA_DISPATCH_WAYS + 1],
        );
        let execution = crate::jit_execution_enter();
        for free in [true, false] {
            let mut table = MegaDispatchTable::new();
            table.free_dead_receiver_ways = free;
            for &c in &same_set[..MEGA_DISPATCH_WAYS - 1] {
                table.install(c, selector, e2, false, false);
            }
            table.install(last, selector, e1, false, false);
            assert_eq!(table.entries_used(), MEGA_DISPATCH_WAYS);
            table.retire_receiver_classes(&|id: u32| id == head);
            assert!(table.lookup(head, selector).is_none());
            table.install(fifth, selector, e2, false, false);
            assert_eq!(table.lookup(fifth, selector).is_some(), free, "free {free}");
            // A supersede-style retirement of a LIVE receiver's way is not
            // freed early.
            table.retire_entry(e1 as usize);
            assert!(table.lookup(last, selector).is_none());
            table.install(sixth, selector, e2, false, false);
            assert!(table.lookup(sixth, selector).is_none(), "free {free}: sixth before grace");
            table.clear_entries();
        }
        crate::jit_execution_leave(execution);
        for body in [first, second] {
            crate::defer_jit_owner(Some(body));
        }
    }
}

#[cfg(test)]
mod r13w11_mega9_profile_seed_tests {
    //! Round 13 wave 11, lane mega9, proposal M13-9: a slot whose site's
    //! receiver profile already shows a megamorphic site is born flagged, so
    //! its gate sends receivers to the cell-first stub from the first call.
    use super::*;

    fn counts(rows: &[(u32, u32)]) -> crate::profile::ReceiverCounts {
        rows.iter().copied().collect()
    }

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x1000 + (u64::from(class_id) << 4)
    }

    #[test]
    fn only_a_profile_past_the_inline_ways_with_a_real_overflow_names_a_megamorphic_site() {
        fn names(rows: &[(u32, u32)]) -> bool {
            profile_names_megamorphic_site(&counts(rows))
        }
        // Four classes fit the inline ways, however many observations.
        assert!(!names(&[(1, 100), (2, 100), (3, 100), (4, 100)]));
        // Eight uniform classes: half the calls overflow the top four.
        let uniform: Vec<(u32, u32)> = (1..=8).map(|c| (c, 10)).collect();
        assert!(names(&uniform));
        // A fifth class seen once in a hundred is a trace, not a site shape.
        assert!(!names(&[(1, 30), (2, 30), (3, 20), (4, 19), (5, 1)]));
        // Exactly a sixteenth overflowing counts.
        assert!(names(&[(1, 4), (2, 4), (3, 4), (4, 3), (5, 1)]));
        // Too few observations say nothing.
        assert!(!names(&[(1, 2), (2, 2), (3, 2), (4, 2), (5, 2)]));
        // A zero row is not a class seen.
        assert!(!names(&[(1, 10), (2, 10), (3, 10), (4, 10), (5, 0)]));
        assert!(!names(&[]));
    }

    #[test]
    fn a_seeded_slot_is_megamorphic_from_birth_and_publishes_hashed_first() {
        let uniform: Vec<(u32, u32)> = (1..=8).map(|c| (c, 10)).collect();
        let pic = JitPICSlot::new();
        pic.note_inline_ways_probed();
        assert!(pic.inline_first());
        assert!(pic.seed_megamorphic_from_profile(&counts(&uniform)));
        assert!(pic.is_megamorphic());
        assert!(!pic.inline_first(), "a megamorphic site is hashed-first");
        pic.install(3, "C", word(3), false, false);
        assert!(
            pic.lookup_megamorphic(3).is_some(),
            "the receiver takes a hashed way, where the cell-first stub reads it"
        );
        pic.clear_entries();
        // A thin profile seeds nothing.
        let thin = JitPICSlot::new();
        assert!(!thin.seed_megamorphic_from_profile(&counts(&[(1, 50), (2, 50)])));
        assert!(!thin.is_megamorphic());
    }

    #[test]
    fn the_seed_switch_turns_it_off() {
        let uniform: Vec<(u32, u32)> = (1..=8).map(|c| (c, 10)).collect();
        let pic = JitPICSlot::new();
        let seeded = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IC_PROFILE_MEGA_SEED", Some("0"))],
            || pic.seed_megamorphic_from_profile(&counts(&uniform)),
        );
        assert!(!seeded);
        assert!(!pic.is_megamorphic());
    }

    /// Round 13 wave 11 (lane mega9, M13-8): one cap for every inline cache
    /// and the stub, 8 by default and 4 under the kill switch.
    #[test]
    fn the_shared_stack_word_cap_follows_its_switch() {
        assert_eq!(ic_max_stack_words(), IC_STACK_WORDS_WIDE);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IC_WIDE_STACK_WORDS", Some("0"))],
            ic_max_stack_words,
        );
        assert_eq!(off, IC_STACK_WORDS_NARROW);
    }
}

#[cfg(test)]
mod r13w11_mega9_grace_lag_refusal_tests {
    //! Round 13 wave 11, lane mega9: `JitPICSlot::install` reports a receiver
    //! it could place nowhere while a way it could have taken waits for its
    //! grace -- the count proposal M13-4 is gated on. The test holds a JIT
    //! execution token across the retirement, the state of a thread spinning
    //! in compiled code, so no other test's drain graces it in between.
    use super::*;

    /// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x5000 + (u64::from(class_id) << 4)
    }

    /// Two classes whose hashed set is `probe`'s, and two whose set is not.
    fn classes(probe: u32) -> ([u32; 2], [u32; 2]) {
        let set = JitPICSlot::mega_base_index(probe);
        let mut same: Vec<u32> = Vec::new();
        let mut other: Vec<u32> = Vec::new();
        for c in (probe + 1)..(probe + 100_000) {
            if JitPICSlot::mega_base_index(c) == set {
                if same.len() < 2 {
                    same.push(c);
                }
            } else if other.len() < 2 {
                other.push(c);
            }
            if same.len() == 2 && other.len() == 2 {
                break;
            }
        }
        assert!(same.len() == 2 && other.len() == 2, "the hash spreads");
        ([same[0], same[1]], [other[0], other[1]])
    }

    #[test]
    fn a_receiver_refused_only_for_a_lagging_grace_is_reported() {
        // This one thread plays both the installer and the thread lagging in
        // compiled code. Round 14 wave 7's catch-up self-stamp
        // (`CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`) would let the installer vouch
        // for itself and lift the refusal this test models, so pin it off.
        crate::THREAD_QUIESCENCE.with(|h| h.ic_catch_up_self_stamp_flag.set(1));
        let execution = crate::jit_execution_enter();
        let probe = 9u32;
        let ([a, b], [c, d]) = classes(probe);
        // Hashed-first slot: A and B fill the probe's hashed set and inline
        // ways 0 and 1; C and D take ways 2 and 3.
        let full = JitPICSlot::new();
        for class in [a, b, c, d] {
            assert!(!full.install(class, "C", word(class), false, false));
        }
        // Every way the probe could take holds a live receiver: refused, but
        // not for the grace.
        assert!(!full.install(probe, "C", word(probe), false, false));
        full.clear_entries();

        let lagging = JitPICSlot::new();
        for class in [a, b, c, d] {
            lagging.install(class, "C", word(class), false, false);
        }
        // A's callee recompiled: its inline way and its hashed way retire and
        // wait for the grace the held token withholds.
        assert!(lagging.retire_entry(word(a) as usize));
        assert!(
            lagging.install(probe, "C", word(probe), false, false),
            "only retired, ungraced ways were left for the probe"
        );
        assert!(lagging.lookup_megamorphic(probe).is_none());
        // A itself may take its ways back at once (same-key refill): not a
        // grace refusal.
        assert!(!lagging.install(a, "C", word(a) + 0x100_0000, false, false));
        lagging.clear_entries();
        crate::jit_execution_leave(execution);
    }
}

#[cfg(test)]
mod r14w7_mega_interned_selector_tests {
    //! Round 14 wave 7, lane mega: the shape-4 census peeks at the selector
    //! map and must never intern (a census that bound or grew anything would
    //! change what it measures).
    use super::*;

    #[test]
    fn a_peek_answers_only_an_interned_selector_and_interns_nothing() {
        let table = MegaDispatchTable::new();
        let ctx = mega_selector_loader_context(3);
        assert_eq!(
            table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
            None
        );
        assert_eq!(
            table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
            None,
            "the first peek interned nothing"
        );
        let id = table
            .selector_id_in_context("p/A", "f", "()V", 0, ctx, 0)
            .expect("interned");
        assert_eq!(
            table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
            Some(id)
        );
        // Every field of the key counts: another kind, context or owner is
        // another selector.
        assert_eq!(table.interned_selector_in_context("p/A", "f", "()V", 2, ctx, 0), None);
        assert_eq!(
            table.interned_selector_in_context(
                "p/A",
                "f",
                "()V",
                0,
                mega_selector_loader_context(4),
                0
            ),
            None
        );
        assert_eq!(table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 9), None);
        // And the next interned id is the one after `id`: no peek took one.
        let next = table
            .selector_id_in_context("p/A", "g", "()V", 0, ctx, 0)
            .expect("interned");
        assert_eq!(next, id + 1);
    }
}
