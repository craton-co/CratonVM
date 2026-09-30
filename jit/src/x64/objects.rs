// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object headers, fields, allocation and the string layout.
//!
//! Everything that addresses an object interior: the inline TLAB bump
//! allocator and the header words it has to initialise, the compact-layout
//! reference `putfield` fast paths, the card-mark and receiver-check helpers,
//! and the `java.lang.String` accessors the string intrinsics are built on.
//!
//! Like `x64/arrays.rs`, every displacement here is a header-offset emission
//! site, and the inline TLAB path in particular writes header words directly
//! rather than through the allocator — `emit_inline_tlab_new` and
//! `Tlab::alloc_initialized` are two implementations of one contract.

use super::*;

/// `newarray` sites that took the inline TLAB bump, and those that kept the
/// helper. See `Compiler::note_inline_array_site` for why both are recorded.
static INLINE_ARRAY_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STUB_ONLY_ARRAY_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Why the inline `newarray` bump last declined, and how often each reason fired.
static INLINE_ARRAY_DECLINES: std::sync::Mutex<Option<Vec<(&'static str, u64)>>> =
    std::sync::Mutex::new(None);

/// `(inline bump, stub only)` counts of compiled `newarray` sites.
pub fn inline_array_site_counts() -> (u64, u64) {
    (
        INLINE_ARRAY_SITES.load(std::sync::atomic::Ordering::Relaxed),
        STUB_ONLY_ARRAY_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(reason, count)` for every inline `newarray` decline this process has seen.
pub fn inline_array_declines() -> Vec<(&'static str, u64)> {
    INLINE_ARRAY_DECLINES
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// What [`Compiler::emit_inline_tlab_new`] does after the bump has published a
/// complete header, on its fast path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TlabPostInit {
    /// No call. The class needs no primitive default and no finalizer, and the
    /// collector finds objects by walking the chunk.
    Skip,
    /// `helpers.tlab_post_init(vm_ptr, obj_ptr, class_id, num_fields)`: the
    /// class needs primitive defaults or finalizer registration (or the site
    /// opted in with `CRATONVM_JIT_ENABLE_INLINE_NEW`).
    Helper,
    /// `helpers.zgc_note_tlab_object(vm_ptr, obj_ptr, size_bytes)`. The class
    /// needs no post-init work, but the collector must be told about every
    /// object (`jit_tlab_registration_required()`, i.e. ZGC with VM TLABs).
    /// This helper only registers the object, so the site no longer pays
    /// `jit_post_tlab_init`'s layout lookup, header re-stores and recipe
    /// probe. See `perf-zgc-compiled-new-always-takes-the-rust-helper-20260918.md`.
    ZgcAnnounce,
}

/// Pick the post-init mode for an inline-TLAB `new`.
///
/// * `skip_helper`: the `new` arm's decision to drop the call, which is
///   `helper_is_noop && !jit_tlab_registration_required()`.
/// * `helper_is_noop`: the class needs no primitive default and no finalizer.
/// * `zgc_note_tlab_object`: the helper-table address, `0` when not wired.
///
/// `helper_is_noop && !skip_helper` holds only when the collector requires
/// registration. That is the one case the thin announce helper covers. A
/// class with real post-init work always keeps `tlab_post_init`, because the
/// announce helper does not write primitive defaults or register finalizers.
pub(super) fn tlab_post_init_mode(
    skip_helper: bool,
    helper_is_noop: bool,
    zgc_note_tlab_object: usize,
) -> TlabPostInit {
    if skip_helper {
        TlabPostInit::Skip
    } else if helper_is_noop && zgc_note_tlab_object != 0 {
        TlabPostInit::ZgcAnnounce
    } else {
        TlabPostInit::Helper
    }
}

// ---- The inline ZGC start-bit store (round 9 wave 8, `zgc8`) ---------------
//
// Copies of the collector's layout constants: this crate cannot depend on
// `cratonvm-gc` (dev-dependency only), so the numbers are duplicated here and
// pinned to `cratonvm_gc::Tlab::{START_OFFSET, ZGC_OWNED_EPOCH_OFFSET,
// ZGC_ANNOUNCE_OFFSET}` and `cratonvm_gc::tlab::JitZgcAnnounceTable::*_OFFSET`
// by `r9w8_zgc_inline_announce_tests::the_offsets_match_the_collector_s`.
// `cratonvm_gc::tlab::JitZgcAnnounceTable` states the contract.

/// `Tlab::cursor`'s offset inside the `Tlab` (`Tlab::CURSOR_OFFSET`).
const ZGC_TLAB_CURSOR_OFFSET: i32 = 0;
/// `Tlab::start` (`Tlab::START_OFFSET`).
const ZGC_TLAB_START_OFFSET: i32 = 16;
/// `Tlab::zgc_owned_epoch` (`Tlab::ZGC_OWNED_EPOCH_OFFSET`).
const ZGC_TLAB_OWNED_EPOCH_OFFSET: i32 = 24;
/// `Tlab::zgc_announce`, the table address or 0 (`Tlab::ZGC_ANNOUNCE_OFFSET`).
const ZGC_TLAB_ANNOUNCE_OFFSET: i32 = 32;
/// `JitZgcAnnounceTable::words`: the start-bitmap word array, 0 = unpublished.
const ZGC_ANNOUNCE_WORDS: i32 = 0;
/// `JitZgcAnnounceTable::base`: the address bit 0 denotes.
const ZGC_ANNOUNCE_BASE: i32 = 8;
/// `JitZgcAnnounceTable::span`: bytes covered from `base`.
const ZGC_ANNOUNCE_SPAN: i32 = 16;
/// `JitZgcAnnounceTable::blocked`: non-zero while marking or generational.
const ZGC_ANNOUNCE_BLOCKED: i32 = 24;
/// `JitZgcAnnounceTable::epoch`: the live VM-TLAB ownership epoch.
const ZGC_ANNOUNCE_EPOCH: i32 = 32;

/// `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` (default ON since the round 9 wave 8
/// integration; `=0` opts out): emit the inline start-bit
/// store in front of the `ZgcAnnounce` helper call. The collector reads the
/// same variable before it publishes the table, and an unpublished table
/// leaves every buffer's `zgc_announce` at 0, which the emitted code sends to
/// the helper -- so either half alone is inert. Read per compiled `new` site.
fn zgc_jit_inline_announce_requested() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE")
}

/// Reference-store sites that received the GATED inline barrier sequence.
///
/// A count needs a denominator to be readable: zero here means either that no
/// collector published a barrier plan or that the workload compiles no
/// reference stores, and those are different facts. Reported by
/// `jit-method-stats` beside the declined count below.
static GATED_REF_STORE_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reference-store sites that asked for the gated sequence and were declined —
/// compiled with the full-helper path instead.
static UNGATED_REF_STORE_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_gated_ref_store() {
    GATED_REF_STORE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn note_ungated_ref_store() {
    UNGATED_REF_STORE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(pre, post, young_floor)` when a collector has published a usable
/// reference-store barrier plan.
///
/// Both gate bytes, plus EXACTLY ONE of the two post-barrier shapes. A
/// publisher that supplied neither has no way to rule the post barrier out; one
/// that supplied both would emit two independent skips for one question — and
/// for a mask publisher the floor is not merely redundant but WRONG (an old-gen
/// object with `gc_age == 0` sits below it), which is the whole reason the mask
/// shape exists.
///
/// A free function so the rule can be tested against a helper table alone,
/// without standing up a `Compiler`.
pub(crate) fn ref_store_gates_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<(usize, usize, usize)> {
    let pre = helpers.ref_store_pre_gate;
    let post = helpers.ref_store_post_gate;
    let floor = helpers.ref_store_post_young_floor;
    let mask = helpers.ref_store_post_skip_mask;
    let one_post_shape = (floor != 0) ^ (mask != 0);
    (pre != 0 && post != 0 && one_post_shape).then_some((pre, post, floor))
}

/// The published post-barrier skip mask, when the plan uses that shape.
///
/// A VALUE, baked as an immediate: which collector is running cannot change
/// after start-up. See `JitRuntimeHelpers::ref_store_post_skip_mask`.
pub(crate) fn ref_store_post_skip_mask_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<u8> {
    let mask = helpers.ref_store_post_skip_mask;
    // A mask wider than the flags byte would be a publisher bug, and baking it
    // would test bits that byte does not have.
    (mask != 0 && mask <= u8::MAX as usize).then_some(mask as u8)
}

/// `(gated, declined)` reference-store site counts.
pub fn ref_store_site_counts() -> (u64, u64) {
    (
        GATED_REF_STORE_SITES.load(std::sync::atomic::Ordering::Relaxed),
        UNGATED_REF_STORE_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

// ---- The generational card view (gen r4w4/cards4, 2026-09-24) -------------
//
// Copies of `cratonvm_gc::card_table::JitCardView`'s layout: this crate cannot
// depend on `cratonvm-gc` (dev-dependency only), so the numbers are duplicated
// here and pinned by `r4w4_cards4_tests::the_card_view_offsets_match_the_collector_s`.

/// `JitCardView::magic` (`cratonvm_gc::card_table::JIT_CARD_VIEW_MAGIC`,
/// "CARDVIEW"). Checked at COMPILE time before any other word is trusted.
const CARD_VIEW_MAGIC: usize = 0x5745_4956_4452_4143;
/// `JitCardView::old_base`: loaded by the emitted barrier at run time.
pub(crate) const CARD_VIEW_OLD_BASE: i32 = 8;
/// `JitCardView::old_end`: loaded by the emitted barrier at run time.
pub(crate) const CARD_VIEW_OLD_END: i32 = 16;
/// `JitCardView::cards_neg` (the card map's address, negated): loaded at run
/// time and SUBTRACTED, so `idx - cards_neg == &cards[idx]` with the
/// `SUB r64, m64` form this backend already executes in its G1 filter test.
pub(crate) const CARD_VIEW_CARDS_NEG: i32 = 24;
/// `JitCardView::flags`, read at compile time.
const CARD_VIEW_FLAGS: usize = 32;
/// `JitCardView::FLAG_PRECISE_REF_ARRAYS`.
const CARD_VIEW_FLAG_PRECISE_REF_ARRAYS: usize = 1;
/// `cratonvm_gc::card_table::CARD_DIRTY`.
pub(crate) const CARD_DIRTY_BYTE: u8 = 1;

/// The generational card view the helper table names, as
/// `(view_address, precise_reference_array_marks)`, or `None` when there is
/// none (G1, ZGC, a hand-built test table) or it does not look like one.
///
/// Read at COMPILE time, and only the two words the collector never changes
/// after construction (`magic`, `flags`); the three the emitted code uses are
/// loaded at run time. A free function so the rule can be tested against a
/// helper table alone.
pub(crate) fn gen_card_view_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<(usize, bool)> {
    let addr = helpers.jit_card_table_addr;
    if addr == 0 || (addr & 7) != 0 || helpers.jit_card_old_base >= helpers.jit_card_old_end {
        return None;
    }
    // SAFETY: a non-zero, 8-aligned `jit_card_table_addr` is only ever
    // produced by `vm/src/jit/helpers.rs::build_helpers_opt` from
    // `GenerationalHeap::jit_card_table_info()` — the address of the card
    // table's boxed `#[repr(C)] JitCardView`, live for the heap's (and so the
    // VM's, and so this compile's) lifetime — or, in this crate's tests, from
    // a live `CardTable`. Words 0 and 4 are plain `usize`s written once at
    // construction and never again, so two unsynchronised reads of them cannot
    // race a writer.
    //
    // gen r4w5/review5 (2026-09-24): word 0 first, and word 4 only once word 0
    // has proved this IS a view. The magic exists to decline "a table that names
    // something else" -- and the old code read word 4 (32 bytes in) of that
    // something else before looking at the magic, so the check that was meant
    // to guard the read came after it. A raw card map smaller than 40 bytes (a
    // test table, or a producer that regresses to the pre-cards4 meaning of
    // this field) would have been read past its end.
    let magic = unsafe { std::ptr::read(addr as *const usize) };
    if magic != CARD_VIEW_MAGIC {
        return None;
    }
    // SAFETY: as above; the magic matched, so `addr` is a whole `JitCardView`
    // (40 bytes) and word 4 is its `flags`.
    let flags = unsafe { std::ptr::read((addr + CARD_VIEW_FLAGS) as *const usize) };
    Some((addr, (flags & CARD_VIEW_FLAG_PRECISE_REF_ARRAYS) != 0))
}

/// `CRATONVM_JIT_INLINE_CARD_MARK` — the inline generational post barrier
/// (gen r4w4/cards4). Opt-in, default OFF this wave; read per compiled site so
/// a test can drive it with `flags::with_thread_overrides`.
fn jit_inline_card_mark_enabled() -> bool {
    cratonvm_types::flags().jit.inline_card_mark
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Vectorised and bulk loop bodies
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/simd.rs`. Admission lives in `x64/simd_analysis.rs`; the
    // raw VEX encodings live in `x64/emit.rs`.

    /// May this compile emit the inline GENERATIONAL post barrier
    /// ([`Self::emit_gen_card_barrier`]) on old receivers, instead of sending
    /// them to the collector's barrier helper?
    ///
    /// # History, and why it was off
    ///
    /// Until 2026-07-30 an inline sequence stored `CARD_DIRTY` straight into
    /// the card map after the reference store. A WildFly JIT boot audit
    /// (`CRATONVM_DBG_RSET_AUDIT`) then found an old `org/jboss/modules/Module`
    /// holding a young child on a CLEAN card, and `494aa83b3` made this a
    /// constant `false`. gen r4w4/cards4 traced that observation to the
    /// sequence's NON-ATOMICITY against the BUG-03 stop-the-world takeover
    /// (`vm/src/jit/xt_root_scan.rs`): a peer running compiled code is frozen
    /// at an arbitrary instruction, so one frozen between the reference store
    /// and the card store is inside a collection with the edge in the heap and
    /// the card clean. A thread inside the Rust helper is never frozen there —
    /// the takeover resumes it and lets it reach a safepoint — which is why
    /// the helper path never showed it. See
    /// `docs/internal/reviews/gengc-round4-w4-cards4-20260924.md` for the full
    /// store-form table and the argument.
    ///
    /// # What is different now
    ///
    /// * The card is checked BEFORE and AFTER the store (see
    ///   [`Self::emit_gen_card_barrier`]), so a peer frozen anywhere in the
    ///   sequence leaves either the old value in the slot or a dirty card.
    /// * Compiled code never writes a card byte: a clean card is a call to the
    ///   collector's own barrier, which keeps the scan bound and the summary
    ///   map armed (`cratonvm_gc::card_table::JitCardView`).
    /// * The view's geometry is loaded at run time, never baked, and comes from
    ///   THIS VM's heap (`build_helpers_for`).
    ///
    /// Requires `CRATONVM_JIT_INLINE_CARD_MARK=1` (default off this wave), a
    /// valid card view, and a wired `write_barrier` helper (the slow arm calls
    /// it; `0` there would be a call to address 0).
    pub(super) fn inline_card_mark_available(&self) -> bool {
        jit_inline_card_mark_enabled()
            && gen_card_view_of(&self.helpers).is_some()
            && self.helpers.write_barrier != 0
    }

    /// Does this helper table's card table carry ELEMENT-precise
    /// reference-array marks? Then a compiled `aastore` hands the barrier the
    /// element's slot address instead of the array's, on every arm — inline or
    /// helper. `false` without a generational card view.
    pub(super) fn gen_card_view_precise_arrays(&self) -> bool {
        gen_card_view_of(&self.helpers).is_some_and(|(_, precise)| precise)
    }

    /// `LEA dst, [array + index*ref_element_size + HEADER_SIZE]` — the address
    /// of the reference element an `aastore` writes, which is the address
    /// `emit_ref_astore_regs` stores through (wide and narrow alike).
    ///
    /// Encoded through the verified `isel` table (`lea_r64_m`, whose SIB and
    /// RSP-index rules `isel`'s own tests decode), not hand-assembled; an
    /// encoding refusal refuses the method rather than emitting a wrong
    /// address.
    pub(super) fn emit_ref_element_address(&mut self, dst: u8, array: u8, index: u8) {
        let scale = cratonvm_types::narrow_oop::ref_element_size() as u8; // Cast: 4 or 8
        let args = super::isel::Args {
            dst,
            mem: super::isel::Mem::base_index(array, index, scale, HEADER_SIZE as i64), // Cast: small layout constant
            ..Default::default()
        };
        match super::isel::encode_named("lea_r64_m", &args) {
            Ok(enc) => self.buf.emit(&enc.bytes),
            Err(_) => self
                .buf
                .mark_codegen_unencodable("gen-card-element-address-unencodable"),
        }
    }

    /// Load `write_barrier(vm_ptr, card_addr, value)`'s arguments for a
    /// compiled `aastore` whose post barrier is the helper CALL (the default
    /// arms, `CRATONVM_JIT_INLINE_CARD_MARK` off).
    ///
    /// gen r4w4/cards4 (precise-array residual, item 1): on an element-precise
    /// generational card table `card_addr` is the ELEMENT's slot, rounded down
    /// to 8 (the same card), so one compiled store into a wide old array
    /// dirties one card instead of re-queueing the whole array every young
    /// pause. Otherwise — no card view (G1, ZGC, test tables) or
    /// `CRATONVM_GC_PRECISE_ARRAY_CARDS=0` — it is the array, exactly as
    /// before. Every source is a frame slot or R10, and R10 is an argument
    /// register on neither ABI, so no load clobbers a later one's source.
    /// Clobbers RAX, RCX and R10 on the precise path.
    pub(super) fn emit_aastore_write_barrier_args(
        &mut self,
        array_slot: StackSlot,
        index_slot: StackSlot,
        val_slot: StackSlot,
    ) {
        if self.gen_card_view_precise_arrays() {
            self.load_slot_to_reg(RAX, array_slot);
            self.load_slot_to_reg(RCX, index_slot);
            self.emit_ref_element_address(R10, RAX, RCX);
            self.emit_and_r64_imm8(R10, -8);
            self.load_slot_to_reg(ARG_REGS[2], val_slot);
            self.emit_mov_r64_r64(ARG_REGS[1], R10);
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        } else {
            // Byte-for-byte the sequence these arms emitted before.
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.load_slot_to_reg(ARG_REGS[1], array_slot);
            self.load_slot_to_reg(ARG_REGS[2], val_slot);
        }
    }

    /// The inline generational card CHECK, without the call it guards.
    ///
    /// `holder` is the receiver (an object or array header), `val` the stored
    /// reference, `mark` the address whose card the barrier dirties: `holder`
    /// itself for every object store, the element's slot for a precise
    /// reference-array store. Returns `(slow, done)` patch sites; the code
    /// FALLS THROUGH into the slow arm, so a caller that emits the call right
    /// after this must still patch `slow` to it. Clobbers R10, R11 and the
    /// flags only; `holder`, `val` and `mark` survive on every edge.
    ///
    /// ```text
    ///   test byte [holder + GC_FLAGS_BYTE], OLD_GEN ; jz done   ; young receiver
    ///   test val, val                          ; jz done        ; null store
    ///   mov  r11, imm64 &view
    ///   mov  r10, [r11 + old_base]
    ///   cmp  holder, r10                       ; jb slow        ; flagged old, outside
    ///   cmp  holder, [r11 + old_end]           ; jae slow       ;   the view: Rust decides
    ///  (cmp  mark, [r11 + old_end]             ; jae slow)      ; mark != holder only
    ///   cmp  val, r10                          ; jb young
    ///   cmp  val, [r11 + old_end]              ; jb done        ; old -> old
    /// young:
    ///   mov  r10, mark
    ///   sub  r10, [r11 + old_base]
    ///   shr  r10, CARD_SHIFT                                    ; card index
    ///   sub  r10, [r11 + cards_neg]                             ; &cards[index]
    ///   cmp  byte [r10], CARD_DIRTY            ; je done        ; already dirty
    /// slow:                                                      ; (fall through)
    /// ```
    ///
    /// Every "not sure" edge goes to `slow`, never to `done`: the Rust barrier
    /// re-decides from the heap's live geometry, so the worst a wrong range
    /// test here can cost is a call. The one `done` that trusts something other
    /// than the card byte is the young-receiver test, which is exactly the
    /// published mask's own test (`JitRefStoreGates::post_skip_mask`) and the
    /// one every non-inline arm already makes.
    ///
    /// Reading the card byte outside a pause is harmless: bytes are only
    /// CLEARED at stop-the-world, so a stale "clean" costs a redundant call and
    /// a stale "dirty" cannot happen.
    pub(super) fn emit_gen_card_check(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
    ) -> (Vec<usize>, Vec<usize>) {
        debug_assert!(!matches!(holder, R10 | R11 | RSP | R12));
        debug_assert!(!matches!(val, R10 | R11));
        debug_assert!(!matches!(mark, R10 | R11));
        let view = self.helpers.jit_card_table_addr;
        let mut slow: Vec<usize> = Vec::new();
        let mut done: Vec<usize> = Vec::new();

        // Young receiver: no card. The published mask's own test.
        self.emit_test_mem8_imm8(
            holder,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_OLD_GEN,
        );
        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → young receiver
                                                    // Null store: no edge.
        self.emit_test_r64_r64(val);
        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null value

        self.emit_mov_imm64_full(R11, view as i64); // Cast: baked data address
        self.emit_mov_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
        // A flagged-old receiver outside the covered range is the Rust
        // barrier's call (it knows the heap's live geometry), never a skip.
        self.emit_cmp_r64_r64(holder, R10);
        slow.push(self.emit_jcc_rel32_patch(0x82)); // JB → slow
        self.emit_cmp_r64_mem_disp32(holder, R11, CARD_VIEW_OLD_END);
        slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
        if mark != holder {
            // The element slot lies inside the holder, so this cannot fire for
            // a well-formed array; it keeps the card index below the map's
            // length even for one that is not.
            self.emit_cmp_r64_mem_disp32(mark, R11, CARD_VIEW_OLD_END);
            slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
        }
        // Old → old needs no card.
        self.emit_cmp_r64_r64(val, R10);
        let young_target = self.emit_jcc_rel32_patch(0x82); // JB → young target
        self.emit_cmp_r64_mem_disp32(val, R11, CARD_VIEW_OLD_END);
        done.push(self.emit_jcc_rel32_patch(0x82)); // JB → old target
        self.patch_rel32_to_here(young_target);

        // &cards[(mark - old_base) >> CARD_SHIFT]. The shift is
        // `cratonvm_types::CARD_SHIFT`, the one constant every card table in the
        // VM is indexed by (never a literal `9`).
        self.emit_mov_r64_r64(R10, mark);
        self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
        self.emit_shr_r64_imm8(R10, cratonvm_types::CARD_SHIFT as u8); // Cast: 9
        self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_CARDS_NEG);
        self.emit_cmp_mem8_imm8(R10, 0, CARD_DIRTY_BYTE);
        done.push(self.emit_jcc_rel32_patch(0x84)); // JE → already dirty
        (slow, done)
    }

    /// The inline generational post barrier for one reference store: the
    /// check above, and on its slow edge a call to the collector's own
    /// barrier, `write_barrier(vm_ptr, mark & !7, val)`.
    ///
    /// # PRE and POST: why every store site calls this twice
    ///
    /// A BUG-03 takeover freezes a peer running compiled code at an arbitrary
    /// instruction and holds it across the collection. With the barrier only
    /// AFTER the store, a peer frozen between the two is exactly the WildFly
    /// observation: the edge is in the heap and its card is clean. Call this
    /// once BEFORE the store and once AFTER it:
    ///
    /// * frozen before the PRE barrier has made the card dirty: the slot still
    ///   holds its old value, so there is no new edge to miss;
    /// * frozen between the PRE barrier and the store: the collection consumes
    ///   the card (holder still has the old value); after resume the store
    ///   runs and the POST barrier finds the card clean and dirties it;
    /// * frozen between the store and the POST barrier: the card is still the
    ///   one PRE dirtied, so the collection scans the holder and sees the edge.
    ///
    /// The young referent is in the frozen peer's registers throughout, so the
    /// takeover roots it conservatively and the cycle does not move it. The
    /// slow arm is Rust code, which the takeover never freezes.
    ///
    /// # Registers
    ///
    /// Fast path: R10, R11, flags. Slow path: a helper call, so every
    /// caller-saved register is clobbered; `reload` names the `(register,
    /// slot)` pairs to restore after the call (at least `holder` and `val`, if
    /// the caller uses them again). Either way, register state at the join is
    /// what the caller had, and the reload-elision mirror is dropped there, so
    /// no later reload can be elided against a register only ONE path set.
    pub(super) fn emit_gen_card_barrier(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        reload: &[(u8, StackSlot)],
    ) {
        debug_assert!(self.inline_card_mark_available());
        // The reloads run AFTER a call, so their sources must survive one: a
        // frame slot or a callee-saved register, never the R8/R9 scratch cache
        // (every caller flushed it before popping these slots).
        debug_assert!(reload
            .iter()
            .all(|(_, s)| matches!(s, StackSlot::Frame(_) | StackSlot::CalleeSaved(_))));
        let (slow, done) = self.emit_gen_card_check(holder, val, mark);
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        // Copy both operands out of the argument registers' way first: R10 and
        // R11 are an argument register on neither ABI, so the three ABI loads
        // below cannot clobber a source they still need.
        self.emit_mov_r64_r64(R10, mark);
        // An 8-aligned address in the same card (the old gen is 8-aligned and a
        // card is 512 bytes), which is what `jit_write_barrier`'s plausibility
        // screen admits for a 4-byte narrow element.
        self.emit_and_r64_imm8(R10, -8);
        self.emit_mov_r64_r64(R11, val);
        self.emit_mov_r64_r64(ARG_REGS[2], R11);
        self.emit_mov_r64_r64(ARG_REGS[1], R10);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        // (vm_ptr, obj_ptr, val_ptr) -> (). `obj_ptr` names the CARD: the
        // generational barrier uses it as an address only (see
        // `vm/src/jit/helpers.rs::jit_write_barrier`).
        cratonvm_jit_api::assert_helper_call_shape!(
            "write_barrier",
            int_args = 3,
            returns_value = false
        );
        self.emit_call_absolute(self.helpers.write_barrier);
        for &(reg, slot) in reload {
            self.load_slot_to_reg(reg, slot);
        }
        for p in done {
            self.patch_rel32_to_here(p);
        }
        // Two paths join here; only one of them performed the reloads above.
        self.slot_mirror = None;
        // Engagement, not assumption: one line per emitted barrier with
        // `RUST_LOG=cratonvm_jit=debug`. No counter, because a counter is a
        // new process `static` (`jit/tests/process_global_statics_ratchet.rs`).
        tracing::debug!(
            "jit: inline generational card barrier emitted (CRATONVM_JIT_INLINE_CARD_MARK, mark={})",
            if mark == holder { "header" } else { "element" }
        );
    }

    /// Emit an inline "decode the String character at `idx`" sequence for
    /// the STRING_SEARCH `compareTo` / `indexOf` intrinsics.
    ///
    /// Reads the code unit at element index `idx_reg` of the backing
    /// `byte[]` whose payload starts at `val_reg + HEADER_SIZE`, branching
    /// on `coder_reg` (0 = LATIN1, one byte/char zero-extended; non-zero =
    /// UTF16, two little-endian bytes/char). The zero-extended `u16` result
    /// lands in the low 16 bits of `dst` (upper bits cleared). The four
    /// register operands are distinct 0..=15 GPR numbers; `idx_reg` is the
    /// SIB index and so must not be RSP (4) — and an index field of 100
    /// means "no index", so it must not be R12 (12) either. `val_reg` is
    /// the SIB base and may be any register (R12/RSP as a base is legal
    /// with the disp8 ModRM used here). No CALL; no memory beyond the array
    /// payload is touched.
    pub(super) fn emit_string_decode_char(
        &mut self,
        dst: u8,
        val_reg: u8,
        idx_reg: u8,
        coder_reg: u8,
    ) {
        // TEST coder_reg, coder_reg ; JNZ utf16
        let mut rex = 0x48u8;
        if coder_reg >= 8 {
            rex |= 0x05;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x85);
        self.buf
            .emit_byte(0xC0 | ((coder_reg & 7) << 3) | (coder_reg & 7));
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ

        // LATIN1: MOVZX dst32, BYTE [val_reg + idx_reg*1 + HEADER_SIZE].
        // 0F B6 /r with a SIB byte (scale=00 → *1).
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB6]);
        // ModRM: mod=01 (disp8), reg=dst, r/m=100 (SIB follows).
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=00, index=idx_reg, base=val_reg.
        self.buf.emit_byte(((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        let done = self.emit_jmp_rel32_patch();

        // UTF16: MOVZX dst32, WORD [val_reg + idx_reg*2 + HEADER_SIZE].
        self.patch_rel32_to_here(utf16);
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=01 (*2), index=idx_reg, base=val_reg.
        self.buf
            .emit_byte(0x40 | ((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        self.patch_rel32_to_here(done);
    }

    /// Load a String receiver's `value` field (the backing `byte[]`/`char[]`
    /// ref) from `base` into `dst`, correctly handling BOTH object layouts
    /// that can coexist for `java/lang/String` at runtime:
    ///
    ///   * compact-ref-field layout — the address is
    ///     `StringFieldLayout::value_compact_offset`;
    ///   * a LEGACY-laid-out instance of the same class — per the getfield
    ///     (opcode 0xb4) inline path's own comment, "a class with a
    ///     registered compact layout may still have LEGACY-laid-out
    ///     instances" (e.g. an allocation whose field count didn't match the
    ///     registered `CompactLayout` at alloc time). Its address is
    ///     `StringFieldLayout::value_legacy_offset`.
    ///
    /// Both offsets are exact payload addresses computed independently by
    /// `StringFieldLayout::new` (see BUG-STRING-CODER-COMPACT-20260726 there
    /// for why neither may be derived from the other). Dispatches per-object
    /// via the `GC_FLAG_COMPACT` header bit, exactly mirroring the getfield
    /// 0xb4 inline path. No scratch register needed.
    ///
    /// # Compressed oops
    ///
    /// Only the **compact** arm is affected: `cratonvm_types::narrow_oop`
    /// narrows compact reference *instance fields* and reference *array
    /// elements* and nothing else, so a LEGACY-laid-out instance still carries
    /// a full 64-bit pointer in its 16-byte tagged `Value` cell and its arm is
    /// unchanged. This was hole 1 of `gc/src/compressed_oops.rs`'s "two
    /// correctness holes": the compact arm used to be an unconditional 64-bit
    /// load, which under narrow oops read 4 bytes of narrow oop plus 4 bytes of
    /// the adjacent field and dereferenced the result.
    pub(super) fn emit_load_string_value_ptr(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ (flag clear => legacy)
                                                      // The compact slot is narrow only when a `CompactLayout` was actually
                                                      // registered for this String class — `StringFieldLayout::new`'s
                                                      // fallback points `value_compact_offset` at the LEGACY cell payload,
                                                      // which stays 8 bytes wide. Matching the offset makes this
                                                      // self-checking rather than trusting the caller and the layout to
                                                      // agree.
        let narrow = narrow_oops_enabled()
            && self.string_layout.is_some_and(|l| {
                l.value_compact_offset == compact_offset && l.value_compact_is_narrow
            });
        if narrow {
            self.emit_load_narrow_ref_field(dst, base, compact_offset);
        } else {
            self.emit_mov_r64_mem_disp32(dst, base, compact_offset);
        }
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_mov_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }

    /// Load a **narrow** compact reference field at `[base + offset]` into
    /// `dst` as a full 64-bit pointer, so every consumer downstream is
    /// unchanged. `dst` may alias `base` (the field is read before the base is
    /// clobbered).
    ///
    /// The slot holds `(addr - narrow_base) >> narrow_shift`, with 0 reserved
    /// for null, so the decode is `narrow_base + (n << shift)` — except for
    /// null, which must stay 0 rather than becoming the base. `SHL` sets ZF
    /// from its result, so the null test is free at the shifts the live heap
    /// actually uses; a pinned shift of 0 needs an explicit `TEST`.
    ///
    /// Unlike [`Self::emit_narrow_ref_aload_regs`], this emitter runs at sites
    /// where the register allocator has already parked values in the extended
    /// registers, so it cannot claim R11 outright. It borrows R11 inside the
    /// non-null arm and restores it before falling through: the `PUSH`/`POP`
    /// pair is balanced, straddles no `CALL` and no RSP-relative access, and is
    /// only ever emitted when the (default-off) narrow-oop gate is on.
    fn emit_load_narrow_ref_field(&mut self, dst: u8, base: u8, offset: i32) {
        // MOV dst32, DWORD [base + offset] — writing a 32-bit GPR zero-extends.
        self.emit_mov_r32_mem_disp32(dst, base, offset);
        let shift = cratonvm_types::narrow_oop::narrow_shift();
        if shift > 0 {
            // SHL dst, shift — sets ZF from the result, so null stays testable.
            self.buf.emit_byte(0x48 | ((dst >= 8) as u8));
            self.buf.emit(&[0xC1, 0xE0 | (dst & 7)]);
            // Truncation: `narrow_shift()` is <= 3 (`narrow_oop::enable`).
            self.buf.emit_byte(shift as u8);
        } else {
            self.emit_test_r64_r64(dst);
        }
        let done = self.emit_jcc_rel32_patch(0x84); // JZ — a null oop stays 0.
        self.buf.emit(&[0x41, 0x53]); // PUSH R11
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().to_le_bytes());
        // ADD dst, R11
        self.buf.emit_byte(0x4C | ((dst >= 8) as u8));
        self.buf.emit(&[0x01, 0xD8 | (dst & 7)]);
        self.buf.emit(&[0x41, 0x5B]); // POP R11
        self.patch_rel32_to_here(done);
    }

    /// Load `String.coder` / `String.hash` into `dst` with the same
    /// per-object compact/legacy dispatch as
    /// [`Self::emit_load_string_value_ptr`].
    ///
    /// `compact_is_byte` selects a zero-extending BYTE load for the compact
    /// arm: `CompactLayout` stores `coder` at its natural one-byte Java
    /// width, and the bytes that follow it are the class's padding — a
    /// 4-byte load there would fold that padding into the value. The legacy
    /// arm is always a sign-extended 4-byte `Value` payload load. `coder`
    /// and `hash` are non-negative in practice, so sign- vs zero-extension
    /// is behaviourally identical for the widths that do overlap.
    pub(super) fn emit_load_string_i32_field(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        compact_is_byte: bool,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84);
        if compact_is_byte {
            // MOVZX dst64, BYTE [base + compact_offset]
            self.emit_movx_r64_mem_disp32(dst, base, compact_offset, 8, false);
        } else {
            self.emit_movsxd_r64_mem_disp32(dst, base, compact_offset);
        }
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_movsxd_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }

    /// One layout's arm of the inline `StringBuilder.append(char)` body.
    ///
    /// Emitted twice — once per instance layout — because a per-field
    /// compact/legacy test would put four branches over the same header bit
    /// around ten instructions. On entry RAX is the receiver (already
    /// null-checked and class-id-guarded) and ECX is the character (already
    /// screened to 0..=0xFF). RDX, R8 and R9 are scratch. RAX is unchanged on
    /// exit, which is what `append` returns.
    ///
    /// Every edge that cannot serve the append pushes a patch into `decline`,
    /// and the caller routes them all to the ordinary native dispatch — see
    /// the STRINGBUILDER_ACCESS region for why these are calls and not
    /// uncommon traps.
    ///
    /// # What the guards buy
    ///
    /// `coder == LATIN1` and `count < value.length` are exactly the conditions
    /// under which `sb_append_units` takes its own in-place arm: it checks
    /// `v.count + units.len() <= v.capacity` and that every unit is
    /// representable, then writes the bytes and stores the new count. Anything
    /// else — a UTF16 payload, a full payload, a character above LATIN1 —
    /// makes it grow or inflate the array, which is the native's job and stays
    /// the native's job here.
    ///
    /// # Why no GC barrier
    ///
    /// The two stores are a BYTE into a `byte[]` and an `int` field. Neither
    /// writes a reference, so neither can create a cross-generational edge for
    /// a card mark to record or overwrite one for SATB to retain. The payload
    /// array is reached through the receiver and is not published or moved
    /// here; this path allocates nothing and so cannot reach a collection.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_sb_append_char_body(
        &mut self,
        count_offset: i32,
        value_offset: i32,
        coder_offset: i32,
        coder_is_byte: bool,
        decline: &mut Vec<usize>,
    ) {
        // R9D = coder. LATIN1 (0) or take the call.
        if coder_is_byte {
            // MOVZX R9D, BYTE [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x0F, 0xB6, 0x88]);
        } else {
            // MOV R9D, [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x8B, 0x88]);
        }
        self.buf.emit(&coder_offset.to_le_bytes());
        self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D, R9D
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNZ

        // RDX = value (the byte[] payload). Null takes the call — a builder
        // whose payload the native has not installed yet.
        self.buf.emit(&[0x48, 0x8B, 0x90]); // MOV RDX, [RAX + value_offset]
        self.buf.emit(&value_offset.to_le_bytes());
        self.emit_test_r64_r64(RDX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        // R8D = count, R9D = value.length. LATIN1 is one byte per character,
        // so the array's element count IS the capacity in characters.
        self.buf.emit(&[0x44, 0x8B, 0x80]); // MOV R8D, [RAX + count_offset]
        self.buf.emit(&count_offset.to_le_bytes());
        // Truncation: a fixed header offset that fits a disp8.
        self.buf
            .emit(&[0x44, 0x8B, 0x4A, cratonvm_types::ARRAY_LENGTH_OFFSET as u8]); // MOV R9D,[RDX+len]
        self.buf.emit(&[0x45, 0x39, 0xC8]); // CMP R8D, R9D
        decline.push(self.emit_jcc_rel32_patch(0x8D)); // JGE — full, or a
                                                       // corrupt count; the
                                                       // native decides.

        // value[count] = (byte) ch
        //   MOV [RDX + R8*1 + ARRAY_DATA_OFFSET], CL
        self.buf.emit(&[0x42, 0x88, 0x4C, 0x02]);
        // Truncation: a fixed header offset that fits a disp8.
        self.buf.emit(&[cratonvm_types::ARRAY_DATA_OFFSET as u8]);
        // count += 1
        self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
        self.buf.emit(&[0x44, 0x89, 0x80]); // MOV [RAX + count_offset], R8D
        self.buf.emit(&count_offset.to_le_bytes());
    }

    /// One layout's arm of the inline `StringBuilder.charAt(int)` body.
    ///
    /// Emitted twice — once per instance layout — for the same reason
    /// [`Self::emit_sb_append_char_body`] is. On entry RAX is the receiver
    /// (already null-checked and class-id-guarded) and R9D is the index
    /// (already loaded — screened by nothing beforehand, because the bounds
    /// check below is what screens it). RDX, R8, R10 and R11 are scratch.
    /// RAX is OVERWRITTEN with the char result on the hit path: unlike
    /// `append`, `charAt` does not return its receiver, so nothing needs RAX
    /// to survive past the point its old value (the receiver pointer) is no
    /// longer needed.
    ///
    /// Every edge that cannot serve the read pushes a patch into `decline`,
    /// same convention as `append`: an out-of-range index is what every
    /// caller that walks to the end of a builder does once, not an uncommon
    /// event worth a whole-method deopt, and the decline edge reproduces the
    /// exact `StringIndexOutOfBoundsException` the ordinary dispatch would.
    ///
    /// The instruction sequence mirrors `java/lang/String`'s own inline
    /// `charAt(I)C` (`op_invoke.rs`'s STRING_ACCESS region) almost exactly —
    /// same register plan, same bounds compare, same coder branch — except
    /// bounds failure DECLINES into a call instead of deopting into a trap,
    /// and `count` is a stored field read directly rather than derived from
    /// `value.length >> coder`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_sb_char_at_body(
        &mut self,
        count_offset: i32,
        value_offset: i32,
        coder_offset: i32,
        coder_is_byte: bool,
        decline: &mut Vec<usize>,
    ) {
        // R10D = coder. LATIN1 (0) or UTF16 (1).
        if coder_is_byte {
            // MOVZX R10D, BYTE [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x0F, 0xB6, 0x90]);
        } else {
            // MOV R10D, [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x8B, 0x90]);
        }
        self.buf.emit(&coder_offset.to_le_bytes());

        // R11D = count.
        self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D,[RAX+count_offset]
        self.buf.emit(&count_offset.to_le_bytes());

        // Bounds: (unsigned) index >= count -> decline. Unsigned also
        // catches a negative index, same trick the String intrinsic uses.
        // CMP R9D, R11D ; JAE decline
        self.buf.emit(&[0x45, 0x39, 0xD9]);
        decline.push(self.emit_jcc_rel32_patch(0x83));

        // R8 = value (the byte[] payload). Null takes the call — a builder
        // whose payload the native has not installed yet, the same
        // defensive check `emit_sb_append_char_body` makes.
        self.buf.emit(&[0x4C, 0x8B, 0x80]); // MOV R8,[RAX+value_offset]
        self.buf.emit(&value_offset.to_le_bytes());
        self.emit_test_r64_r64(R8);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        // Decode: coder branch. R10D = coder, R9 = index, R8 = value ptr.
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ
                                                     // LATIN1: MOVZX EAX, BYTE [R8+R9*1+ARRAY_DATA_OFFSET]
        self.buf.emit(&[
            0x43,
            0x0F,
            0xB6,
            0x44,
            0x08,
            // Truncation: a fixed header offset that fits a disp8.
            cratonvm_types::ARRAY_DATA_OFFSET as u8,
        ]);
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(utf16);
        // UTF16: MOVZX EAX, WORD [R8+R9*2+ARRAY_DATA_OFFSET]
        self.buf.emit(&[
            0x43,
            0x0F,
            0xB7,
            0x44,
            0x48,
            // Truncation: a fixed header offset that fits a disp8.
            cratonvm_types::ARRAY_DATA_OFFSET as u8,
        ]);
        self.patch_rel32_to_here(done);
        // char result already zero-extended in EAX; sign-extend into the
        // 64-bit operand slot (harmless — a char is never negative).
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
    }

    /// One layout's arm of the inline `StringBuilder.append(String)` body.
    ///
    /// Emitted twice — once per DESTINATION instance layout — same reason
    /// [`Self::emit_sb_append_char_body`] is. On entry RAX is the receiver
    /// (already null-checked and class-id-guarded), R8 is the source
    /// String's `value` ptr, R9D its char count and R10D its coder — all
    /// three read ONCE by the caller before either layout arm runs, because
    /// none of them depends on the DESTINATION's layout. RDX, RCX and R11
    /// are scratch. RAX is unchanged on exit, which is what `append` returns.
    ///
    /// # The two servable coder combinations, and the one that is not
    ///
    /// `same-coder` (both LATIN1 or both UTF16) copies each source unit
    /// verbatim. `LATIN1 source into a UTF16 destination` WIDENS: one source
    /// byte per destination two-byte unit, zero-extended — always
    /// representable, so there is nothing to check. The reverse — a UTF16
    /// source narrowing into a LATIN1 destination — declines: some of the
    /// source's sixteen-bit units may not fit in eight bits, and checking
    /// that is the native's job, not this path's.
    ///
    /// # Why capacity and coder are read fresh here rather than passed in
    ///
    /// The DESTINATION's `count`/`coder` depend on which of the two layout
    /// arms is running, exactly like `count` in `emit_sb_append_char_body`;
    /// unlike that body, this one also needs the destination's `coder` to
    /// decide same-width vs. widen vs. decline, so it is read here rather
    /// than threaded in.
    ///
    /// `src_is_heap_array` says whether R8 (the source read base) points at
    /// a heap `byte[]` object needing `+ARRAY_DATA_OFFSET` to reach its
    /// element data (`append(String)`'s source, a real `String.value`) or
    /// already sits at the first byte to read (`append(int)`'s source, a
    /// stack scratch buffer with no object header at all). Threaded through
    /// to [`Self::emit_sb_append_string_copy`], which is the one that
    /// actually emits the read.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_sb_append_string_body(
        &mut self,
        count_offset: i32,
        value_offset: i32,
        coder_offset: i32,
        coder_is_byte: bool,
        src_is_heap_array: bool,
        decline: &mut Vec<usize>,
    ) {
        // RDX = dest.value ptr. Null takes the call — a builder whose
        // payload the native has not installed yet, the same defensive
        // check `emit_sb_append_char_body` makes.
        self.buf.emit(&[0x48, 0x8B, 0x90]); // MOV RDX,[RAX+value_offset]
        self.buf.emit(&value_offset.to_le_bytes());
        self.emit_test_r64_r64(RDX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        // R11D = dest.coder.
        if coder_is_byte {
            // MOVZX R11D, BYTE [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x0F, 0xB6, 0x98]);
        } else {
            // MOV R11D, [RAX + coder_offset]
            self.buf.emit(&[0x44, 0x8B, 0x98]);
        }
        self.buf.emit(&coder_offset.to_le_bytes());

        // Coder compare: R10D = src.coder (read once by the caller), R11D =
        // dest.coder. Equal -> same-width copy below. Otherwise the only
        // combination this path serves is src LATIN1 (0) widening into a
        // UTF16 (1) destination, so a src UTF16 here (the JNZ) means the
        // unservable narrowing direction.
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        let same_width = self.emit_jcc_rel32_patch(0x84); // JE
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNZ

        // Widen: src LATIN1 -> dest UTF16. R10D = dest.count; R10's old
        // value (src.coder, known 0 here) is no longer needed.
        self.buf.emit(&[0x44, 0x8B, 0x90]); // MOV R10D,[RAX+count_offset]
        self.buf.emit(&count_offset.to_le_bytes());
        self.emit_sb_append_string_copy(count_offset, 1, true, src_is_heap_array, decline);
        let done = self.emit_jmp_rel32_patch();

        self.patch_rel32_to_here(same_width);
        // R10D = dest.count; R10's old value (src.coder) equalled
        // dest.coder to reach here and is no longer needed either.
        self.buf.emit(&[0x44, 0x8B, 0x90]); // MOV R10D,[RAX+count_offset]
        self.buf.emit(&count_offset.to_le_bytes());
        // R11D = dest.coder still holds its value from above (nothing
        // between there and here wrote it); branch on it once to pick the
        // element width both sides share.
        self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D,R11D
        let utf16_same = self.emit_jcc_rel32_patch(0x85); // JNZ
        self.emit_sb_append_string_copy(count_offset, 0, false, src_is_heap_array, decline);
        let latin1_done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(utf16_same);
        self.emit_sb_append_string_copy(count_offset, 1, false, src_is_heap_array, decline);
        self.patch_rel32_to_here(latin1_done);
        self.patch_rel32_to_here(done);
    }

    /// Shared tail of [`Self::emit_sb_append_string_body`], once the coder
    /// combination is known: capacity check, fold the destination's old
    /// `count` into the write base, bump `count`, and copy.
    ///
    /// `dst_scale_log2` is 0 for a LATIN1 destination (1 byte/char) or 1
    /// for a UTF16 one (2 bytes/char) — always the DESTINATION's element
    /// width, which decides both the capacity shift and the write
    /// stride. `widen` is `true` only for a LATIN1 source into a UTF16
    /// destination: the loop then reads one source BYTE per iteration and
    /// zero-extends it into a two-byte destination unit, instead of
    /// copying `1 << dst_scale_log2` bytes verbatim.
    ///
    /// Register contract on entry: RAX = dest receiver, RDX = dest.value
    /// ptr, R8 = src read base, R9D = src.count (chars), R10D = dest.count
    /// (freshly loaded by the caller — NOT dest.coder; the caller has
    /// already branched on that to choose `dst_scale_log2`/`widen`). RCX
    /// and R11 are free. On exit RDX, RCX, R10 and R11 are clobbered; RAX,
    /// R8 and R9 are unchanged (R8/R9 so a caller that needed to retry a
    /// different arm could, though none does today).
    ///
    /// `src_is_heap_array` is the same flag `emit_sb_append_string_body`
    /// takes, and it is why R8 is a "read base" rather than unconditionally
    /// "src.value ptr": a heap `byte[]`'s element 0 sits `ARRAY_DATA_OFFSET`
    /// bytes past the object pointer (past the class id, GC flags and
    /// length header fields), while `append(int)`'s stack scratch buffer is
    /// bare bytes with no such header to skip. Getting this wrong for the
    /// heap case doesn't fault — the header bytes are readable — it just
    /// silently copies four bytes of class id and header instead of the
    /// string's own content, which is exactly what shipped the first time:
    /// `append("alpha")` measured 5 characters copied (the length header
    /// answers `.length` correctly) and read back as `[0, 0, 0, 0, 5]`, all
    /// header bytes, not one of them 'a'.
    fn emit_sb_append_string_copy(
        &mut self,
        count_offset: i32,
        dst_scale_log2: u8,
        widen: bool,
        src_is_heap_array: bool,
        decline: &mut Vec<usize>,
    ) {
        // ECX = dest.value.length (bytes) -> capacity (chars).
        self.buf
            .emit(&[0x8B, 0x4A, cratonvm_types::ARRAY_LENGTH_OFFSET as u8]); // MOV ECX,[RDX+ARRAY_LENGTH_OFFSET]
        if dst_scale_log2 == 1 {
            self.buf.emit(&[0xD1, 0xE9]); // SHR ECX,1
        }
        // ECX = remaining = capacity - dest.count(R10D).
        self.buf.emit(&[0x44, 0x29, 0xD1]); // SUB ECX,R10D
                                            // decline when src.count(R9D) > remaining(ECX).
        self.buf.emit(&[0x41, 0x39, 0xC9]); // CMP R9D,ECX
        decline.push(self.emit_jcc_rel32_patch(0x8F)); // JG

        // Fold the old count into the write base: RDX = &value[count] (in
        // the DESTINATION's own element width) + ARRAY_DATA_OFFSET.
        if dst_scale_log2 == 1 {
            // LEA RDX,[RDX+R10*2+ARRAY_DATA_OFFSET]
            self.buf.emit(&[
                0x4A,
                0x8D,
                0x54,
                0x52,
                cratonvm_types::ARRAY_DATA_OFFSET as u8,
            ]);
        } else {
            // LEA RDX,[RDX+R10*1+ARRAY_DATA_OFFSET]
            self.buf.emit(&[
                0x4A,
                0x8D,
                0x54,
                0x12,
                cratonvm_types::ARRAY_DATA_OFFSET as u8,
            ]);
        }
        // new_count = dest.count(R10D, old) + src.count(R9D).
        self.buf.emit(&[0x45, 0x01, 0xCA]); // ADD R10D,R9D

        // Copy loop: RCX = i = 0.
        self.buf.emit(&[0x31, 0xC9]); // XOR ECX,ECX
        let loop_top = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xC9]); // CMP ECX,R9D
        let loop_done = self.emit_jcc_rel32_patch(0x8D); // JGE
        if src_is_heap_array {
            if widen || dst_scale_log2 == 0 {
                // MOVZX R11D, BYTE [R8+RCX*1+ARRAY_DATA_OFFSET]
                self.buf.emit(&[0x45, 0x0F, 0xB6, 0x5C, 0x08]);
            } else {
                // MOVZX R11D, WORD [R8+RCX*2+ARRAY_DATA_OFFSET]
                self.buf.emit(&[0x45, 0x0F, 0xB7, 0x5C, 0x48]);
            }
            self.buf.emit(&[cratonvm_types::ARRAY_DATA_OFFSET as u8]);
        } else if widen || dst_scale_log2 == 0 {
            // MOVZX R11D, BYTE [R8+RCX*1]
            self.buf.emit(&[0x45, 0x0F, 0xB6, 0x1C, 0x08]);
        } else {
            // MOVZX R11D, WORD [R8+RCX*2]
            self.buf.emit(&[0x45, 0x0F, 0xB7, 0x1C, 0x48]);
        }
        if dst_scale_log2 == 1 {
            // MOV [RDX+RCX*2], R11W (0x66 operand-size prefix precedes REX)
            self.buf.emit(&[0x66, 0x44, 0x89, 0x1C, 0x4A]);
        } else {
            // MOV [RDX+RCX*1], R11B
            self.buf.emit(&[0x44, 0x88, 0x1C, 0x0A]);
        }
        self.buf.emit(&[0xFF, 0xC1]); // INC ECX
        let back = self.emit_jmp_rel32_patch();
        let rel = loop_top as i32 - (back as i32 + 4); // Cast: rel32 displacement
        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        self.patch_rel32_to_here(loop_done);

        // count = new_count (already computed into R10D above).
        self.buf.emit(&[0x44, 0x89, 0x90]); // MOV [RAX+count_offset],R10D
        self.buf.emit(&count_offset.to_le_bytes());
    }

    /// Emit a compiled `getstatic` as a direct load, with no helper `CALL`.
    ///
    /// Returns `false` when the site cannot be inlined, in which case the
    /// caller must keep the `jit_getstatic` path; `true` means the value has
    /// been pushed (and the oop mark / volatile fence emitted) already.
    ///
    /// # Shape
    ///
    /// ```text
    ///   MOV RAX, imm64          ; &statics_index[class].base  (the POINTER cell)
    ///   MOV RAX, [RAX]          ; the class's statics block base
    ///   MOV/MOVSXD RAX, [RAX + field_index*16 + payload_off]
    /// ```
    ///
    /// The first two are what replaces a helper round trip; the third is the
    /// same load the inline `getfield` arms emit, against the same 16-byte
    /// `Value` cell layout (`FIELD_CELL_PAYLOAD*_OFFSET`, pinned by
    /// `field_cell_layout_matches_value_enum`). Result conventions match
    /// `jit_getstatic` exactly: `MOVSXD` for the int category (`Value::Int(i)
    /// => i as i64`), a 32-bit zero-extending `MOV` for float (`f.to_bits() as
    /// i64`), a 64-bit `MOV` of the payload word for long/double/reference
    /// (`Object(None)` leaves that word zero, i.e. JVM null).
    ///
    /// # What is NOT emitted, and why that is safe
    ///
    /// * **No class-init check.** The resolver only answers for a class that is
    ///   already initialized, and initialization is monotonic.
    /// * **No exception check.** With no call there is no `i64::MIN` deopt
    ///   sentinel to disambiguate — which also removes a latent bug the helper
    ///   path still has, where a `static long` legitimately holding
    ///   `Long.MIN_VALUE` is indistinguishable from a thrown `<clinit>`.
    /// * **No `flush_scratch_registers`.** Nothing here clobbers a register the
    ///   operand-stack cache can hold: `SCRATCH_REGS` is `[R8, R9]` and this
    ///   sequence touches only RAX, which the helper path clobbers anyway.
    /// * **No plausibility check on a reference payload.** Same contract as the
    ///   inline `getfield` arms, which also raw-load the payload word.
    pub(super) fn try_emit_inline_getstatic(
        &mut self,
        class_id_raw: u32,
        field_index: usize,
        type_tag: u8,
        is_volatile: bool,
    ) -> bool {
        if !inline_getstatic_enabled() {
            return false;
        }
        let Some(base_cell) = self
            .direct_helpers
            .resolve_static_base(class_id_raw, field_index)
        else {
            return false;
        };
        // Cast: cell byte offset within the class's statics block -> disp32.
        let Ok(cell_off) = i32::try_from(field_index.saturating_mul(SLOT_SIZE)) else {
            return false;
        };
        // Cast: the baked address of the never-freed base-pointer cell.
        self.emit_mov_imm64(RAX, base_cell as i64);
        self.emit_mov_r64_mem_disp32(RAX, RAX, 0);
        match type_tag {
            b'J' | b'D' | b'L' | b'[' => self.emit_mov_r64_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
            ),
            b'F' => self.emit_mov_r32_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            ),
            _ => self.emit_movsxd_r64_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            ),
        }
        // Volatile static: no fence after the read. x86-64 (TSO) already gives
        // a plain load acquire semantics, and sequential consistency comes
        // from the StoreLoad barrier after every volatile STORE (`LOCK ADD
        // [RSP], 0`, `emit_volatile_store_fence` — the JMM mapping HotSpot
        // uses). `CRATONVM_JIT_VOLATILE_LOAD_FENCE=1` puts the fence back, in
        // lock-step with the helper arm and the IR tier.
        if is_volatile && crate::runtime_lowering::volatile_load_fence_enabled() {
            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
        }
        self.push_from_rax();
        // A reference-typed static's loaded value is a live oop — same
        // obligation as the helper arm (T1.1.a).
        if type_tag == b'L' || type_tag == b'[' {
            self.mark_top_as_oop();
        }
        true
    }

    /// Null / alignment / three-region containment on the receiver in RAX,
    /// returning the patch sites the caller must route to its slow path.
    ///
    /// **Which table `bounds_addr` names is the caller's decision, and the two
    /// kinds of caller decide differently.** The six-word
    /// `[b0, e0, b1, e1, b2, e2]` layout is shared, so the emitted bytes are
    /// identical and only the baked immediate differs -- but the two tables
    /// answer different questions:
    ///
    /// * READ callers (the `getfield` arms, and `ir_lower`'s copy of this
    ///   sequence) pass `helpers.read_bounds_addr` -- `JIT_READ_BOUNDS`, the
    ///   "is this address mapped, so a raw load cannot fault" table, which
    ///   Generational and G1 both fill and ZGC deliberately does not;
    /// * STORE callers (the inline reference-`putfield` arms) pass
    ///   `helpers.region_bounds_addr` -- `JIT_REGION_BOUNDS`, whose EMPTINESS
    ///   under G1/ZGC is load-bearing: it is what stops an inline store from
    ///   skipping `post_write_barrier_rset` and losing the remembered-set edge
    ///   a JNI-pinned, CSet-excluded region is reachable only through
    ///   (`g1-audit.md` 8.1). Those callers additionally gate on
    ///   [`region_bounds_are_live`], which reads that table's CONTENT.
    ///
    /// Handing the read table to a store caller would silently unblock exactly
    /// the fast path G1-2 exists to block. Two tables rather than one is what
    /// makes that mistake something you have to type out rather than inherit.
    pub(super) fn emit_guarded_getfield_receiver_check(
        &mut self,
        bounds_addr: usize,
    ) -> Vec<usize> {
        let mut slow: Vec<usize> = Vec::new();
        // 1. null → slow (helper throws the NPE).
        self.emit_test_r64_r64(RAX);
        slow.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                    // 2. alignment: low 3 bits must be clear.
        self.emit_mov_r64_r64(RCX, RAX);
        self.emit_and_r64_imm8(RCX, 7);
        slow.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
                                                    // 3. region containment. RDX = the caller's bounds table (six
                                                    //    usize words: [b0, e0, b1, e1, b2, e2]) -- READ callers pass
                                                    //    JIT_READ_BOUNDS, STORE callers JIT_REGION_BOUNDS; see above.
        self.emit_mov_imm64(RDX, bounds_addr as i64);
        // region 0: RAX >= b0 && RAX < e0 → ok
        self.emit_cmp_r64_mem_disp(RAX, RDX, 0);
        let below_b0 = self.emit_jcc_rel32_patch(0x82); // JB → try region 1
        self.emit_cmp_r64_mem_disp(RAX, RDX, 8);
        let ok0 = self.emit_jcc_rel32_patch(0x82); // JB → in region 0
        self.patch_rel32_to_here(below_b0);
        // region 1
        self.emit_cmp_r64_mem_disp(RAX, RDX, 16);
        let below_b1 = self.emit_jcc_rel32_patch(0x82); // JB → try region 2
        self.emit_cmp_r64_mem_disp(RAX, RDX, 24);
        let ok1 = self.emit_jcc_rel32_patch(0x82); // JB → in region 1
        self.patch_rel32_to_here(below_b1);
        // region 2 — last chance: outside → slow.
        self.emit_cmp_r64_mem_disp(RAX, RDX, 32);
        slow.push(self.emit_jcc_rel32_patch(0x82)); // JB → slow
        self.emit_cmp_r64_mem_disp(RAX, RDX, 40);
        slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
                                                    // fall-through / ok: receiver is inside a published live region.
        self.patch_rel32_to_here(ok0);
        self.patch_rel32_to_here(ok1);
        slow
    }

    /// Cheaper receiver guard for a value whose operand-stack type is already
    /// proven to be an oop by the bytecode/type tracker. Such a value cannot be
    /// an unaligned integer or an arbitrary out-of-heap address without an
    /// earlier JIT/GC correctness failure, so repeating the six arena-bound
    /// comparisons at every field access is redundant. Null remains a real
    /// Java exceptional case and is routed to the existing checked helper.
    pub(super) fn emit_trusted_oop_receiver_check(&mut self) -> Vec<usize> {
        self.emit_test_r64_r64(RAX);
        vec![self.emit_jcc_rel32_patch(0x84)] // JZ -> checked helper
    }

    /// [`Self::emit_trusted_oop_receiver_check`], with the check DROPPED when
    /// the null-check dataflow already proves the receiver non-null at
    /// `bc_pc`. Returns an empty patch list in that case, which every caller
    /// already handles — the slow path is simply unreachable.
    ///
    /// # Only `getfield` may call this
    ///
    /// The proof is `preceding_aload_nonnull_local`: the local named by the
    /// `aload` that ends exactly at `bc_pc`. That local is the receiver only
    /// when the receiver is the value on TOP of the operand stack, which is
    /// true of `getfield` and of nothing else nearby:
    ///
    /// * `putfield`'s stack is `[…, objectref, value]` — the preceding push is
    ///   the stored VALUE, and attributing the receiver's fact to it is the
    ///   exact shape of the Tomcat `MessageBytes.setString` miscompile that
    ///   `opcode_dereferences_receiver` documents at length.
    /// * `checkcast` does not throw on null at all — a null cast succeeds —
    ///   so its `JZ` targets a legal null path, not an NPE. Eliding it would
    ///   let a null receiver fall into the `KIND_TAGS` byte compare and fault.
    ///
    /// # Why this cannot see a spliced callee's bytecode
    ///
    /// `self.null_check_info` is analysed from the caller's `code` and indexed
    /// by caller bci. `compile_bytecode` is called exactly once, on that same
    /// array (`driver.rs`), and the inline splicer emits callee bodies through
    /// its own emitters rather than re-entering the walk — so `code` and
    /// `bc_pc` here always denote the method the analysis actually ran on. A
    /// future splicer that DID re-enter `compile_bytecode` with a callee's
    /// bytecode would break that pairing silently, which is why it is written
    /// down rather than left to be rediscovered.
    pub(super) fn emit_trusted_oop_receiver_check_at(
        &mut self,
        code: &[u8],
        bc_pc: usize,
        implicit_ok: bool,
        arm: usize,
        starts: &[bool],
    ) -> Vec<usize> {
        if super::null_check_elim::receiver_null_elim_enabled() {
            // Built INSIDE the flag test, not at the top of the function. The
            // flag defaults on, but `CRATONVM_JIT_RECEIVER_NULL_ELIM=0` is a
            // bisect switch, and its whole value is that the off arm is the
            // previous binary — an allocation and an O(code.len()) walk per
            // getfield site on the disabled path would make the two arms
            // differ in compile cost as well as in codegen. This is therefore
            // the outermost point in a function this change may edit at which
            // the map can be computed without changing what the disabled arm
            // does.
            //
            // `code.len()` is the required length: it is the one
            // `preceding_aload_nonnull_local` builds internally, and the map
            // has to be that same map for the decode below to mean what its
            // SOUNDNESS FIX doc says it means.
            // The map arrives as a parameter, built once per method by
            // `compile_bytecode`. It used to be rebuilt here, per site.
            //
            // A MERGE POINT REFUSES, and its absence here was a bug. The rule
            // is stated on `array_receiver`: the pattern "names the receiver
            // by TEXTUAL adjacency, which is the dataflow only when neither
            // the access nor the index push can be jumped to. A caller that
            // elides a check on the strength of the pair must therefore also
            // refuse when either PC is a merge point". The four sibling sites
            // obey it (`arrays.rs` twice, `op_control.rs` twice); this one did
            // not, and it is default-ON.
            //
            // `is_local_nonnull` being sound does not rescue it. That query is
            // a meet-over-paths dataflow, so a fact it reports about a local
            // holds on every incoming path — but at a join the textually
            // preceding `aload` is not necessarily the one that ran. In
            //
            //     (c ? a : b).v
            //
            // the `getfield` sits at the join, the preceding push is
            // `aload b`, and if `b` is provably non-null the receiver check
            // disappears. On the other path the receiver is `a`, about which
            // nothing was proved, so a null `a` reaches the dereference with
            // no guard — and, because the elision returns early, with no
            // recovery address bound either. The result is a bare SIGSEGV
            // where the JVMS requires a NullPointerException.
            //
            // Refusing is the cheap direction: the site falls through to the
            // implicit null check below, which is what it did before this
            // elision existed.
            if !self.null_check_info.is_merge_point(bc_pc) {
                if let Some(local) =
                    super::null_check_elim::preceding_aload_nonnull_local_with_starts(
                        code, bc_pc, &starts,
                    )
                {
                    if self.is_local_nonnull(bc_pc, local) {
                        super::null_check_elim::note_receiver_null_check_elided();
                        return Vec::new();
                    }
                }
            }
        }
        // No proof. The check can still leave the fast path, if the fault it
        // would have prevented is caught and translated instead — which is
        // what the implicit null check is. The instruction that will occupy
        // `self.buf.pos()` is the receiver dereference; the caller binds its
        // recovery address, and verifies its encoding, in
        // `bind_implicit_null_recovery`.
        //
        // `implicit_ok` is the caller asserting that it emits such a
        // dereference NEXT and unconditionally. Only the compact `getfield`
        // arm does: its `GC_FLAGS` read at `[RAX + 7]` always follows. The
        // second arm emits that read only under
        // `compact_ref_fields_enabled()`, so it passes `false` rather than
        // make the guarantee conditional — the verification would catch a
        // wrong answer, but as a failed compile on a live workload rather than
        // as a decision made here.
        if implicit_ok && crate::implicit_null::active() {
            self.implicit_null_pending.push((self.buf.pos(), bc_pc));
            super::null_check_elim::note_receiver_null_check_implicit(arm);
            return Vec::new();
        }
        super::null_check_elim::note_receiver_null_check_emitted();
        self.emit_trusted_oop_receiver_check()
    }

    // -----------------------------------------------------------------
    // F-08 — the inline G1 post-write barrier
    // -----------------------------------------------------------------

    /// Byte offsets into the published `JIT_G1_BARRIER` table
    /// (`gc/src/gen_heap.rs::JitG1BarrierTable`). Five `usize` words:
    /// `[arena_base, arena_len, region_mask, card_table_base, card_shift]`.
    pub(super) const G1B_ARENA_BASE: i32 = 0;
    pub(super) const G1B_ARENA_LEN: i32 = 8;
    pub(super) const G1B_REGION_MASK: i32 = 16;

    /// F-08 — may this compile emit a real G1 post-write barrier inline,
    /// instead of routing every reference store to `jit_putfield_object`?
    ///
    /// Three things must hold, and each of them is a separate hazard:
    ///
    /// * the opt-in flag is set (`CRATONVM_G1_INLINE_BARRIER`, default OFF);
    /// * a G1 collector has published its geometry into `JIT_G1_BARRIER`, so
    ///   the arena base, length and region mask the sequence loads are real;
    /// * the lean barrier helper is wired, since the inline arm's slow path
    ///   CALLs it and a zero there would be a call to address 0. A hand-built
    ///   test helper table leaves it zero; that is the "not wired" contract
    ///   every optional slot in `JitRuntimeHelpers` carries.
    ///
    /// **This does not, and must not, re-open defect G1-2.** That defect is
    /// about an inline store that SKIPS the barrier; `region_bounds_are_live`
    /// stays false under G1 and every generational-style barrier-free arm stays
    /// unreachable there. What this enables is an arm that EMITS the barrier —
    /// the same remembered-set edge `post_write_barrier_rset` records, with the
    /// two cases in which that function provably does nothing filtered out
    /// inline. See `emit_g1_post_write_barrier_regs` for that argument in full.
    ///
    /// It is also not `inline_card_mark_available()`. That is the GENERATIONAL
    /// card barrier, disabled after a WildFly boot audit found an old
    /// `org/jboss/modules/Module` reference to a young child left on a CLEAN
    /// card, and re-offered by gen r4w4/cards4 behind its own opt-in
    /// (`CRATONVM_JIT_INLINE_CARD_MARK`) with the root cause closed. Different
    /// mechanism, different table, different collector; the two are kept
    /// separate so that enabling one never silently enables the other.
    /// # Which methods this can reach at all (F-08 residual, measured)
    ///
    /// Only the ones compiled by THIS tier. `ir_lower` — the IR tier — has no
    /// reference-store site whatsoever; its own `read_bounds_addr` doc says so
    /// in as many words ("this tier emits no inline reference STORE... The
    /// store question... has no site here to ask it"), and it asks only the
    /// read-side mapped-address question. A method the IR tier compiles
    /// therefore keeps the out-of-line `putfield_object` helper no matter what
    /// this predicate answers.
    ///
    /// That is visible from outside, and was measured rather than assumed. With
    /// `RUST_LOG=cratonvm_jit=info`, the ACTIVE line below appears exactly once
    /// on `apps/g1_probe/G1CardChurn` with the flag on and never with it off —
    /// and never on `probes/G1ChurnPauseProbe` in either arm, whose hot stores
    /// are constructor field writes in a method this tier does not own.
    ///
    /// So the barrier's reach is bounded by which tier compiles the storing
    /// method, and the workload that exhibits the barrier and the workload that
    /// exhibits pause behaviour are not the same one. That is the honest reason
    /// F-08 still has no pause-level number, and it is a `jit/` change to fix,
    /// not a `gc/` one.
    ///
    /// (A note on measuring this: a bare `RUST_LOG=info` shows nothing. The
    /// launcher builds its filter as `from_default_env().add_directive(WARN)`,
    /// and a global WARN ties with a global `info` on specificity, resolving
    /// last-added-wins. A target-scoped `RUST_LOG=cratonvm_jit=info` is more
    /// specific and wins. Getting that wrong reads as "the arm never engages".)
    pub(super) fn g1_inline_barrier_available(&self) -> bool {
        g1_inline_barrier_enabled()
            && g1_barrier_table_live(self.helpers.g1_barrier_addr)
            && self.helpers.g1_post_write_barrier != 0
    }

    /// F-08 — receiver guard for the inline G1 store arm: null, alignment and
    /// containment in a mapped arena, returning the patch sites the caller
    /// routes to its slow path.
    ///
    /// **Why this passes `read_bounds_addr` where the store arms pass
    /// `region_bounds_addr`, and why that is not the mistake the doc on
    /// [`Self::emit_guarded_getfield_receiver_check`] warns about.**
    ///
    /// That warning says handing the READ table to a STORE caller "would
    /// silently unblock exactly the fast path G1-2 exists to block". It is
    /// about a caller that uses containment as its LICENCE TO SKIP THE
    /// BARRIER: under G1 the store-side table is empty, every receiver is
    /// rejected, and that rejection is what forces the helper. Swapping in a
    /// table G1 does publish would let those receivers through with no barrier
    /// at all.
    ///
    /// This caller does not skip the barrier. It emits one
    /// ([`Self::emit_g1_post_write_barrier_regs`]) on the path this guard
    /// admits. Containment here is doing its ORIGINAL job and only that job —
    /// "is this address inside mapped arena memory, so the header reads and the
    /// 8-byte field store that follow cannot fault" — which is precisely the
    /// question `JIT_READ_BOUNDS` answers and which G1 publishes into. The
    /// store-side table is untouched and `region_bounds_are_live` still reads
    /// it and still says no.
    ///
    /// If this guard admitted a receiver it should not, the failure mode is a
    /// fault or a corrupt store, not a lost remembered-set edge; the barrier
    /// below runs for every admitted receiver regardless.
    pub(super) fn emit_g1_store_receiver_check(&mut self) -> Vec<usize> {
        self.emit_guarded_getfield_receiver_check(self.helpers.read_bounds_addr)
    }

    /// F-08 — G1's post-write barrier, inline.
    ///
    /// Preconditions: `obj_reg` holds the receiver and `val_reg` the stored
    /// reference, the inline store has already happened, and `scratch` is a
    /// register the caller does not need afterwards. All three are clobbered.
    /// `obj_slot` / `val_slot` are the stack slots the operands came from, so
    /// the slow arm can reload them into the ABI argument registers without
    /// depending on what the filter did to the originals.
    ///
    /// # The sequence
    ///
    /// ```text
    ///   test val, val                 ; a null store records nothing
    ///   jz   done
    ///   mov  scratch, imm64 &JIT_G1_BARRIER
    ///   sub  obj, [scratch + 0]       ; obj - arena_base
    ///   sub  val, [scratch + 0]       ; val - arena_base
    ///   xor  obj, val
    ///   and  obj, [scratch + 16]      ; & region_mask, sets ZF
    ///   jz   done                     ; same region: nothing to remember
    ///   <reload ABI args from slots>
    ///   call g1_post_write_barrier
    /// done:
    /// ```
    ///
    /// # Why eliding those two cases is sound
    ///
    /// `G1Collector::post_write_barrier_rset` opens with exactly the same two
    /// tests and returns without touching anything when either fires:
    ///
    /// * a null stored reference has `lookup_region_for_addr(0) == None`, so
    ///   the `(Some(s), Some(d)) if s != d` match arm cannot be taken;
    /// * two addresses in the same region take the same `_ => return` arm.
    ///
    /// So the inline filter removes calls whose callee would have returned, and
    /// never a call that would have recorded. The remaining cases — an address
    /// outside G1's arena, a destination region that is Free, an edge this
    /// thread already recorded — are all left to the callee, which already
    /// distinguishes them and which is where the F-05 card store lives.
    ///
    /// # Why the arena base is SUBTRACTED rather than the XOR taken raw
    ///
    /// `(obj ^ val) & region_mask == 0` asks whether the two addresses share an
    /// aligned `region_size` block of the address space, and G1's arena is not
    /// region-aligned, so an aligned block is NOT a region: exactly one region
    /// boundary falls inside each block, and two addresses straddling it would
    /// be called "same region", the barrier skipped, and a live cross-region
    /// edge lost. That is a use-after-free, so the two subtractions are
    /// load-bearing rather than tidy.
    ///
    /// The alignment the arena actually has has already changed once under this
    /// reasoning and the argument must not come to depend on it. It was a
    /// `Vec<u8>` (malloc-aligned) when this was written; since F-16 it is an
    /// `mmap` / `VirtualAlloc` reservation, so 4 KiB on Linux and 64 KiB on
    /// Windows. Neither is a region — the default region size is 1 MiB and the
    /// ergonomic can take it to 32 MiB — and neither is guaranteed by anything
    /// the collector promises. The subtractions are correct for ANY base, which
    /// is the property to preserve.
    ///
    /// An out-of-arena operand makes its subtraction wrap to a huge value; that
    /// can only make the XOR differ and send the store to the helper, which
    /// then no-ops. The failure direction is a wasted call, never a lost edge.
    ///
    /// # Why the card is not dirtied here
    ///
    /// It could be — the table carries the card base and shift — but it would
    /// buy nothing. The remembered-set ENTRY still has to be recorded, and that
    /// is a hash-map insert keyed on a (source region, target region) pair with
    /// no inline form. Dirtying the card inline and then calling anyway is
    /// duplicated work; the callee dirties it on the way through. What would
    /// change this is Phase 2 taking its source set from the card table instead
    /// of from the region-index remembered set, which is a collector policy
    /// change and not an emitter one.
    /// Count a G1 filter execution that answered "nothing to remember".
    ///
    /// Emitted at the caller's skip label, not inside the filter: the filter
    /// hands its two skip patches back and the CALLER decides where they land,
    /// so this is the only place the skipped edge is addressable.
    pub(super) fn emit_g1_barrier_skip_trace(&mut self) {
        self.emit_ref_store_path_trace(&crate::metrics::G1_INLINE_BARRIER_SKIPPED);
    }

    pub(super) fn emit_g1_post_write_barrier_regs(
        &mut self,
        obj_reg: u8,
        val_reg: u8,
        scratch: u8,
        obj_slot: StackSlot,
        val_slot: StackSlot,
    ) {
        let nothing_to_do = self.emit_g1_barrier_filter(obj_reg, val_reg, scratch);
        // Everything the filter could not dismiss: the collector's own barrier.
        // Both operands are reloaded from their stack slots, because the filter
        // destroyed the registers they were in.
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.load_slot_to_reg(ARG_REGS[2], val_slot);
        // ABI-3 — (vm_ptr, obj_ptr, val_ptr) -> (). The slow arm of the INLINE
        // G1 barrier: the filter above dismissed nothing, so the collector's
        // own barrier runs. Nothing but this assertion connects the three
        // registers written here to the helper's declared arity, because
        // `emit_call_absolute` takes a bare address.
        cratonvm_jit_api::assert_helper_call_shape!(
            "g1_post_write_barrier",
            int_args = 3,
            returns_value = false
        );
        self.emit_call_absolute(self.helpers.g1_post_write_barrier);
        let past_skip = self.emit_jmp_rel32_patch();
        // The filter's "nothing to remember" edge, counted where it lands.
        for patch in nothing_to_do {
            self.patch_rel32_to_here(patch);
        }
        self.emit_g1_barrier_skip_trace();
        self.patch_rel32_to_here(past_skip);
    }

    /// F-08 — the two-test filter alone, without the call it guards.
    ///
    /// Returns the jump sites the caller must patch to "nothing to remember".
    /// Clobbers all three registers.
    ///
    /// Split from [`Self::emit_g1_post_write_barrier_regs`] so the filter can
    /// be EXECUTED in a unit test without a compiled frame — the call arm
    /// reloads its operands through `emit_load_local` / `load_slot_to_reg`,
    /// which need a real prologue and a real heap local, and that requirement
    /// would otherwise put the part of this sequence that can silently
    /// miscompile (three instruction encodings this file had no other user for,
    /// and an address-arithmetic argument) beyond the reach of any test that
    /// runs the code. Same motive as `RememberedSet::add_reference_in_generation_within`:
    /// make the risky half addressable on its own.
    pub(super) fn emit_g1_barrier_filter(
        &mut self,
        obj_reg: u8,
        val_reg: u8,
        scratch: u8,
    ) -> Vec<usize> {
        // Engagement, not assumption. The codebase's own rule ("never assume a
        // gated path was taken -- verify") is why `g1: parallel evacuation
        // ACTIVE` exists, and it applies twice over to a barrier that is opt-in
        // AND behind three conjoined conditions: a run whose checksum matches
        // HotSpot proves nothing about this arm unless something says the arm
        // was emitted. One line per process, at `info`.
        {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "jit: G1 inline post-write barrier ACTIVE (F-08, CRATONVM_G1_INLINE_BARRIER)"
                );
            }
        }
        debug_assert!(
            self.helpers.g1_barrier_addr != 0,
            "F-08: emit_g1_barrier_filter called with no JIT_G1_BARRIER table — \
             the sequence would load through a null table address"
        );
        // 1. Null stored reference: `post_write_barrier_rset` returns.
        self.emit_test_r64_r64(val_reg);
        let done_null = self.emit_jcc_rel32_patch(0x84); // JZ
                                                         // 2. Same region: `post_write_barrier_rset` returns.
        self.emit_mov_imm64(scratch, self.helpers.g1_barrier_addr as i64);
        self.emit_sub_r64_mem_disp32(obj_reg, scratch, Self::G1B_ARENA_BASE);
        self.emit_sub_r64_mem_disp32(val_reg, scratch, Self::G1B_ARENA_BASE);
        self.emit_xor_r64_r64(obj_reg, val_reg);
        self.emit_and_r64_mem_disp32(obj_reg, scratch, Self::G1B_REGION_MASK);
        let done_same = self.emit_jcc_rel32_patch(0x84); // JZ
        crate::metrics::note_g1_inline_barrier_site();
        // The run-time half of the engagement question. `tracing::info!` above
        // says the arm was EMITTED; only this says how often its two tests
        // actually spared the call, which is the whole reason the arm exists.
        // Emitted here, on the fall-through, because that is the one edge the
        // caller does not own: the two skip patches belong to the caller and
        // are counted where it lands them.
        self.emit_ref_store_path_trace(&crate::metrics::G1_INLINE_BARRIER_CALLED);
        vec![done_null, done_same]
    }

    /// The full-barrier route every inline reference-`putfield` arm falls back
    /// to: `jit_putfield_object(heap, obj, field_index, value)`, which performs
    /// the SATB pre-barrier and the collector's OWN post-write barrier — G1's
    /// `post_write_barrier_rset` included, which is the remembered-set edge a
    /// JNI-pinned (CSet-excluded) young region is reachable only through.
    ///
    /// Factored out for G1-2 so the "bounds are not live ⇒ take the helper"
    /// short-circuit is literally the same instruction sequence as the bail
    /// target the fast paths already patch to.
    fn emit_ref_putfield_helper_call(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
    ) {
        // What DECLINING costs, at run time. The compile-time census counts
        // declined SITES; this counts the stores they actually make.
        self.emit_ref_store_path_trace(&crate::metrics::REF_STORE_FULL_HELPER_TAKEN);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        // (vm_ptr, obj_ptr, field_index, value) -> ().
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_object",
            int_args = 4,
            returns_value = false
        );
        self.emit_call_absolute(self.helpers.putfield_object);
    }

    /// Emit a compact reference-field store with a barrier-free fast path and
    /// the validated helper as its slow path.
    ///
    /// Small callees such as constructors are emitted by
    /// `try_emit_inline_body`, not the top-level bytecode loop. Keeping this
    /// emitter shared inside `Compiler` makes their field stores follow the
    /// same safety contract as top-level compact `putfield`: only a mapped,
    /// genuinely compact, young receiver whose old field is null is written
    /// directly. Every case requiring SATB/card barriers goes through
    /// `jit_putfield_object`.
    pub(super) fn emit_inline_body_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
    ) {
        let cell_off = compact_body_offset as i32; // absolute compact displacement

        // F-08 — the G1 arm. Entered only when a G1 collector has published
        // its geometry AND the opt-in flag is set; it emits a REAL G1
        // post-write barrier after the store instead of borrowing the
        // generational arm's "a young receiver needs no barrier" premise,
        // which is false under G1 (see G1-2 below and `g1-audit.md`
        // §10). `region_bounds_are_live` is deliberately NOT consulted for it
        // and stays false under G1 — the store-side table is untouched.
        let g1 = self.g1_inline_barrier_available();

        // G1-2: no published bounds ⇒ no generational card metadata ⇒ the
        // "young receiver needs no post barrier" premise does not hold (G1's
        // RSet edge into a JNI-pinned, CSet-excluded region would be lost).
        // The containment guard below would reject every receiver anyway with
        // an all-zero table, and with an unwired table it would bake a
        // `MOV RDX,0` + `CMP RAX,[RDX]` that faults — so take the helper
        // outright instead of emitting an inline path that can never run.
        if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();
        // The baked `cell_off` is a compile-time claim about this class's
        // compact layout — see `emit_layout_epoch_guard`.
        bail.extend(self.emit_layout_epoch_guard());

        self.load_slot_to_reg(RAX, obj_slot);
        if g1 {
            bail.extend(self.emit_g1_store_receiver_check());
        } else {
            bail.extend(self.emit_guarded_getfield_receiver_check(self.helpers.region_bounds_addr));
        }

        // A registered compact class may still have legacy instances when a
        // synthetic/native allocation used a mismatched slot count.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper

        // Without direct generational card metadata, old receivers retain the
        // collector-specific helper. Otherwise the post-store mark is inline.
        //
        // F-08: the G1 arm skips this test entirely, and that is the point.
        // `GC_FLAG_OLD_GEN` is a GENERATIONAL bit; G1 stamps it (defect G1-1's
        // fix) but its own post barrier does not care about it, because a G1
        // remembered-set edge is cross-REGION, not old-to-young. Testing it
        // here would send every promoted receiver to the helper for no reason
        // while doing nothing for the young-into-pinned-region case that
        // actually needs the barrier.
        if !g1 && !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        // A non-null old value needs the SATB pre-barrier.
        self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
        self.emit_test_r64_r64(RCX);
        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null -> helper

        // No field-count bounds check: the receiver was just proven compact,
        // a compact instance's 8-byte header has no count word (offset 8 is
        // its first field), and its layout is its class's -- which the
        // verifier proved declares this field.

        // Compact reference fields are bare 8-byte pointers.
        self.load_slot_to_reg(RDX, val_slot);
        // gen r4w4/cards4: the inline generational barrier, PRE half (see
        // `emit_gen_card_barrier` for why the card is checked on both sides).
        let inline_card = !g1 && self.inline_card_mark_available();
        if inline_card {
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[(RAX, obj_slot), (RDX, val_slot)]);
        }
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_BODY_TAKEN);
        if g1 {
            // F-08. RCX is dead here (it last held the field's old value), so it
            // is the scratch; RAX and RDX are clobbered by the filter and the
            // slow arm reloads both from their slots.
            self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, obj_slot, val_slot);
        } else if inline_card {
            // ...and the POST half. Nothing is live after it.
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[]);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(done);
    }

    /// Constructor-only specialization for the first syntactic write to a
    /// compact reference field.
    ///
    /// JVM verification only permits `<init>` on a non-null uninitialized
    /// object produced by `new`. The inline resolver additionally admits only
    /// empty super-constructor chains and forward control flow. Therefore the
    /// first write to a given field starts from null and its resolved slot is
    /// in bounds. A young compact receiver needs no barrier; the only runtime
    /// checks retained are the per-object compact flag (synthetic allocations
    /// can still use legacy cells) and old-generation bit (allocation spill).
    ///
    /// G1-2 (`g1-audit.md` §8.1): "a young compact receiver needs no
    /// barrier" is a GENERATIONAL claim. This emitter used to state it with no
    /// receiver guard whatsoever — not even the null test its two sibling
    /// emitters have — so on a backend that publishes no region bounds it wrote
    /// the reference inline and lost the collector's post-write barrier. Under
    /// G1 that is the JNI-pinned-young-region remembered-set edge (a pinned
    /// region is excluded from the CSet, so its rset is the ONLY way in), i.e. a
    /// use-after-free. It now takes the helper outright when
    /// [`region_bounds_are_live`] is false, and when it is true it emits the
    /// same null test the trusted-oop arms emit.
    ///
    /// `receiver_is_new_this`: the caller proved
    /// `licm::inline_ctor_store_starts_from_null` — the receiver is the
    /// constructor's `this`, the result of a `new`. Then the G1 arm takes the
    /// null test instead of the containment check: that check exists so the
    /// header read and the store cannot fault, and a `new` result is an object
    /// this heap handed out. The barrier is emitted either way.
    pub(super) fn emit_inline_fresh_ctor_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
        receiver_is_new_this: bool,
    ) {
        let cell_off = compact_body_offset as i32; // absolute compact displacement

        // F-08 — the G1 arm, as in `emit_inline_body_compact_ref_putfield`.
        // §10 of the audit named THIS emitter as where closing G1-2 costs
        // measurable time: `n.left = newChild` inside `<init>` is the shape
        // that dominates allocation-heavy code, and it became a helper call.
        // The arm below stores inline and then runs a real G1 post barrier,
        // whose common case for that shape — parent and child allocated back
        // to back in one Eden region — is two instructions and a not-taken
        // branch.
        //
        // §10 also explains why the barrier cannot simply be ELIDED for a
        // freshly allocated receiver: G1 pinning is region-granular, the
        // allocator does not avoid pinned regions, and a pinned young region
        // is held out of the collection set and reached only through its
        // remembered set. So the store must run a barrier; it just does not
        // have to run a CALL.
        let g1 = self.g1_inline_barrier_available();

        // G1-2: bounds not live ⇒ not the generational backend ⇒ every
        // reference store must run the collector's own post-write barrier.
        if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();
        // The baked `cell_off` is a compile-time claim about this class's
        // compact layout — see `emit_layout_epoch_guard`.
        bail.extend(self.emit_layout_epoch_guard());

        self.load_slot_to_reg(RAX, obj_slot);
        // G1-2: receiver guard, consistent with the other two emitters. The
        // full containment check is deliberately NOT repeated here — with
        // bounds live the backend is Generational, and this receiver is the
        // `new`-produced uninitialized object the JVM verifier requires for
        // `<init>` (see the precondition above), so the remaining exceptional
        // case is null. It is unreachable in practice (the caller already
        // emitted `emit_precise_null_check_field_store`) and therefore costs a
        // perfectly-predicted not-taken branch; without it a null receiver
        // faulted on the `gc_flags` header read below instead of reaching the
        // helper's defined no-op semantics.
        // F-08: the G1 arm takes the FULL containment guard rather than the
        // bare null test. The trusted-oop substitution's premise is "with
        // bounds live the backend is Generational", which is exactly what this
        // arm falsifies, so it cannot inherit the cheaper check — and the
        // header reads and 8-byte store below need the receiver to be inside
        // mapped arena memory whatever the collector is — unless the receiver
        // is proven to be a `new` result (`receiver_is_new_this`).
        if g1 && !receiver_is_new_this {
            bail.extend(self.emit_g1_store_receiver_check());
        } else {
            bail.extend(self.emit_trusted_oop_receiver_check());
        }
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper
        if !g1 && !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        self.load_slot_to_reg(RDX, val_slot);
        // gen r4w4/cards4: PRE half of the inline generational barrier. A fresh
        // `new` result is young unless allocation spilled it into old gen, so
        // this is normally the one-instruction young-receiver skip.
        let inline_card = !g1 && self.inline_card_mark_available();
        if inline_card {
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[(RAX, obj_slot), (RDX, val_slot)]);
        }
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_FRESH_CTOR_TAKEN);
        if g1 {
            // F-08 — RCX is untouched by this emitter, so it is free scratch.
            self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, obj_slot, val_slot);
        } else if inline_card {
            // POST half.
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[]);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(done);
    }

    /// The inline ZGC start-bit store (round 9 wave 8, `zgc8`;
    /// `perf-zgc-compiled-new-always-takes-the-rust-helper-20260918.md`).
    ///
    /// Entered after the bump has committed a fully headed object: R10 = the
    /// `JvmThread*`, R11 = the object. Performs what
    /// `ZgcRealHeap::note_tlab_object` does on its common path -- ONE unlocked
    /// `or` of the object's bit into the start bitmap
    /// (`ZObjectStarts::insert_in_owned_chunk`'s plain-store arm) -- when every
    /// condition of `cratonvm_gc::tlab::JitZgcAnnounceTable`'s contract holds,
    /// and leaves RAX = the object. Returns `(done, to_helper)`: `done` is a
    /// `JMP rel32` the caller patches to the point after the helper call, and
    /// every `to_helper` branch must be patched to the helper call's first
    /// instruction.
    ///
    /// Clobbers RAX, RCX, RDX, R8, R9 (and flags) -- all caller-saved in both
    /// ABIs, and all clobbered by the helper call this fronts, so nothing live
    /// can be in them. R10 and R11 are preserved for the helper arm.
    ///
    /// ```text
    ///   mov  rax, [r10 + tlab+32]      ; Tlab::zgc_announce (the table) or 0
    ///   test rax, rax ; jz helper
    ///   mov  rdx, [rax + 32]           ; table.epoch
    ///   cmp  rdx, [r10 + tlab+24]      ; == Tlab::zgc_owned_epoch ?
    ///   jne  helper
    ///   mov  rdx, [rax + 24] ; test ; jnz helper    ; table.blocked
    ///   mov  rdx, [rax + 0]  ; test ; jz helper     ; table.words
    ///   mov  rcx, r11 ; sub rcx, [rax + 8] ; jb helper   ; off = obj - base
    ///   cmp  rcx, [rax + 16] ; jae helper                ; off < span
    ///   test rcx, 7 ; jnz helper                         ; on the 8-byte grid
    ///   mov  r9, rcx ; and r9, -512 ; mov r8, [rax + 8] ; add r9, r8  ; word_lo
    ///   cmp  r9, [r10 + tlab+16] ; jb helper             ; word_lo >= start
    ///   mov  r8, [r10 + end] ; sub r8, r9 ; cmp r8, 512 ; jb helper  ; whole word owned
    ///   shr  rcx, 3 ; mov r8, rcx ; shr r8, 6 ; and rcx, 63
    ///   mov  r9d, 1 ; shl r9, cl
    ///   or   [rdx + r8*8], r9          ; the start bit (no lock: owned word)
    ///   mov  rax, r11 ; jmp done
    /// ```
    pub(super) fn emit_zgc_inline_announce(
        &mut self,
        cursor_off: i32,
        end_off: i32,
    ) -> (usize, Vec<usize>) {
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        let mut to_helper: Vec<usize> = Vec::with_capacity(12);

        // The buffer's armed table, or 0.
        self.emit_mov_r64_mem_disp32(RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        self.emit_test_r64_r64(RAX);
        to_helper.push(self.emit_jcc_rel32_patch(0x84)); // JE helper
                                                         // Ownership still valid: the live epoch is the one the chunk was
                                                         // carved under.
        self.emit_mov_r64_mem_disp32(RDX, RAX, ZGC_ANNOUNCE_EPOCH);
        self.emit_cmp_r64_mem_disp32(RDX, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
        to_helper.push(self.emit_jcc_rel32_patch(0x85)); // JNE helper
                                                         // Not marking, not generational.
        self.emit_mov_r64_mem_disp32(RDX, RAX, ZGC_ANNOUNCE_BLOCKED);
        self.emit_test_r64_r64(RDX);
        to_helper.push(self.emit_jcc_rel32_patch(0x85)); // JNE helper
                                                         // The bitmap (RDX stays = words to the end).
        self.emit_mov_r64_mem_disp32(RDX, RAX, ZGC_ANNOUNCE_WORDS);
        self.emit_test_r64_r64(RDX);
        to_helper.push(self.emit_jcc_rel32_patch(0x84)); // JE helper
                                                         // RCX = obj - base, in range and on the grid.
        self.emit_mov_r64_r64(RCX, R11);
        self.emit_sub_r64_mem_disp32(RCX, RAX, ZGC_ANNOUNCE_BASE);
        to_helper.push(self.emit_jcc_rel32_patch(0x82)); // JB helper: obj < base
        self.emit_cmp_r64_mem_disp32(RCX, RAX, ZGC_ANNOUNCE_SPAN);
        to_helper.push(self.emit_jcc_rel32_patch(0x83)); // JAE helper: off >= span
        self.emit_test_r64_imm32(RCX, 7);
        to_helper.push(self.emit_jcc_rel32_patch(0x85)); // JNE helper: off-grid
                                                         // R9 = word_lo = base + (off & !511).
        self.emit_mov_r64_r64(R9, RCX);
        // AND r9, imm32 (-512, sign-extended): REX.W+B 81 /4.
        self.buf.emit(&[0x49, 0x81, 0xE1]);
        self.buf.emit(&(-512i32).to_le_bytes());
        self.emit_mov_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_BASE);
        // ADD r9, r8: REX.W+R+B 01 /r, modrm 11 000 001.
        self.buf.emit(&[0x4D, 0x01, 0xC1]);
        // The whole 512-byte word inside [start, end): word_lo >= start ...
        self.emit_cmp_r64_mem_disp32(R9, R10, tlab + ZGC_TLAB_START_OFFSET);
        to_helper.push(self.emit_jcc_rel32_patch(0x82)); // JB helper
                                                         // ... and end - word_lo >= 512 (end >= cursor > obj >= word_lo).
        self.emit_mov_r64_mem_disp32(R8, R10, end_off);
        self.emit_sub_r64_r64(R8, R9);
        // CMP r8, imm32 (512): REX.W+B 81 /7, modrm 11 111 000.
        self.buf.emit(&[0x49, 0x81, 0xF8]);
        self.buf.emit(&512i32.to_le_bytes());
        to_helper.push(self.emit_jcc_rel32_patch(0x82)); // JB helper
                                                         // RCX = bit index, R8 = word index, RCX = bit within the word.
        self.emit_shr_r64_imm8(RCX, 3);
        self.emit_mov_r64_r64(R8, RCX);
        self.emit_shr_r64_imm8(R8, 6);
        self.emit_and_r64_imm8(RCX, 63);
        // R9 = 1 << CL.
        self.emit_mov_imm32_sx(R9, 1);
        // SHL r9, cl: REX.W+B D3 /4, modrm 11 100 001.
        self.buf.emit(&[0x49, 0xD3, 0xE1]);
        // OR [rdx + r8*8], r9: REX.W+R+X 09 /r, modrm 00 001 100, SIB 11 000 010.
        self.buf.emit(&[0x4E, 0x09, 0x0C, 0xC2]);
        self.emit_mov_r64_r64(RAX, R11);
        let done = self.emit_jmp_rel32_patch();
        (done, to_helper)
    }

    pub(super) fn emit_inline_tlab_new(
        &mut self,
        class_id_raw: u32,
        num_fields: usize,
        post_init: TlabPostInit,
    ) {
        // The inline start-bit store only ever fronts the announce helper.
        let inline_announce =
            post_init == TlabPostInit::ZgcAnnounce && zgc_jit_inline_announce_requested();
        self.emit_inline_tlab_new_with(class_id_raw, num_fields, post_init, inline_announce, None);
    }

    /// [`Self::emit_inline_tlab_new`] with the inline ZGC start-bit store
    /// decided by the caller (`inline_announce`, honoured only on the
    /// `ZgcAnnounce` arm). The seam the executed-code tests drive, so they do
    /// not depend on the process environment.
    ///
    /// `decline` (round 12 wave 1, lane `calls`; proposal W17-1): `None` is
    /// the `new` site, byte for byte what this emitted before. `Some` is the
    /// DECLINE form a guarded prefix uses (`op_invoke.rs`'s inline
    /// `Integer.valueOf` box): the bump emits NO call at all -- the thread
    /// comes from the cached frame slot or the `JIT_THREAD` mirror, never
    /// from `get_current_thread` -- and every edge that would have reached
    /// `new_object` is pushed onto the vector instead, for the caller to
    /// patch to its own slow path. The fast path falls through with RAX = the
    /// object. Only `TlabPostInit::Skip` has a call-free fast path, so any
    /// other mode (and the inline-TLAB opt-out) declines unconditionally.
    /// Written by the fast path: RAX, R10, R11, and RDX only under the
    /// zero-elision opt-out; RCX survives.
    pub(super) fn emit_inline_tlab_new_with(
        &mut self,
        class_id_raw: u32,
        num_fields: usize,
        // CRIT-2 — when both `has_nonzero_tag_primitive_init` and
        // `has_finalizer` are statically known false at the call site, the post-init
        // helper has nothing meaningful to do beyond writing the
        // identity-hash and num_slots header words. We can emit those
        // inline and skip the helper call (which otherwise costs a
        // class_manager.read() and a finalizer-queue lock). When
        // unknown (the conservative default in `try_compile`), we
        // still issue the helper call. Under a registering collector
        // (ZGC) such a class takes the thin announce helper instead
        // (`TlabPostInit::ZgcAnnounce`, round 9 wave 4).
        post_init: TlabPostInit,
        inline_announce: bool,
        mut decline: Option<&mut Vec<usize>>,
    ) {
        let decline_form = decline.is_some();
        if let Some(d) = decline.as_deref_mut() {
            // A decline form that cannot keep its promise (no call on the fast
            // path, no thread-fetch call, no withheld spill to discharge)
            // emits one unconditional decline and nothing else.
            let thread_source = self.jit_thread_slot_off != 0 || jit_thread_tls_disp() != 0;
            if post_init != TlabPostInit::Skip
                || !inline_tlab_new_enabled()
                || !thread_source
                || self.deferred_alloc_blind_spill
                || self.deferred_alloc_shadow_push
            {
                d.push(self.emit_jmp_rel32_patch());
                return;
            }
        }
        let inline_announce = inline_announce && !decline_form;
        let skip_post_init_helper = post_init == TlabPostInit::Skip;
        // A raw compiled bump updates `Tlab::cursor` without going through
        // the allocator's publication protocol. In concurrent Elasticsearch
        // merge churn that left a malformed young-space span before the next
        // collection could obtain an exact object map. Route through the
        // checked runtime helper until the raw JIT path can share the same
        // atomic publication contract as `Tlab::alloc_initialized`.
        //
        // The helper retains TLAB allocation (and its fast path); it merely
        // removes the unsynchronised machine-code cursor writer.
        // Allocation spill sink (`alloc_spill_sink_enabled`) — the `new` arm
        // asked `emit_pre_safepoint_spill` to withhold every blind-spill
        // register this fast path does not clobber, and that emitter agreed.
        // Two obligations follow, both discharged below: the preamble must stay
        // within `ALLOC_FAST_PATH_CLOBBERS`, and every slow-path edge must emit
        // the withheld stores before it can reach `new_object`.
        let sink_spill = self.deferred_alloc_blind_spill;
        if !inline_tlab_new_enabled() {
            // Unreachable under the sink (the request checks this gate), but the
            // withheld stores are this arm's to emit if it ever becomes so.
            self.emit_deferred_alloc_blind_spill();
            if std::mem::take(&mut self.deferred_alloc_shadow_push) {
                // Same reasoning; the call below is then the only path.
                self.emit_shadow_push();
            }
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32);
            self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32);
            // (vm_ptr, class_id, num_fields) -> obj | 0. The inline-TLAB gate is
            // off, so the whole allocation is the helper's.
            cratonvm_jit_api::assert_helper_call_shape!(
                "new_object",
                int_args = 3,
                returns_value = true
            );
            self.emit_call_absolute(self.helpers.new_object);
            return;
        }

        // Compact reference-field layout: when a per-class layout is registered
        // for exactly this field count, allocate the layout's total size (the
        // 8-byte header included) and mark the object compact (GC_FLAG_COMPACT
        // in the mark word's flag byte; there is no shape word) inline
        // — no helper call, no per-alloc layout lookup. `class_layout` here runs
        // once at JIT-compile time, not per allocation.
        // GROOVY-CLUSTER-20260717 → GUARDED RESTORE (perf/halfgap-20260717):
        // class_layout(class_id_raw) is snapshotted ONCE at JIT-compile time
        // and its body_size gets baked as immediate constants below
        // (bump-allocation size, array_length header write). When a class's
        // registered compact layout is later REPLACED (class_manager.rs's
        // recompute_subclass_layouts — the synthetic-stub→real-bytecode
        // upgrade, the exact shape of ANTLR/Groovy-generated parser
        // classes), an already-compiled site would keep allocating at the
        // OLD size while field access (correctly, per-object) uses the
        // CURRENT layout — confirmed heap corruption; the interim fix
        // disabled this path entirely (compact_body = None).
        //
        // The restore bakes the ADDRESS + compile-time VALUE of the class's
        // layout-REPLACE counter (`types::field_layout::layout_replace_guard`
        // — fixed-capacity table, addresses stable for process life; new
        // class REGISTRATIONS don't bump it, only replacements do) and
        // emits a 3-instruction guard at the top of the inline path:
        //     mov r11, imm64(count_addr)
        //     mov eax, [r11]
        //     cmp eax, imm32(count_at_compile_time)  ;  jne slow_path
        // A replaced layout therefore permanently routes this site to the
        // always-correct `new_object` helper — for classes that never get
        // replaced (every benchmark and the overwhelming majority of real
        // classes), the full compact inline path is back.
        // Round 11 wave 15 (lane hdr): through `compact_tlab_total_size`, the
        // interpreter TLAB's own predicate, rather than a bare `class_layout`.
        // `class_layout` is DOMAIN-BLIND: in a process with a second
        // `ClassStore` it answers a class id this VM never registered with
        // ANOTHER VM's layout (every store numbers its classes from 0), and a
        // matching field count then stamped `GC_FLAG_COMPACT` on an object
        // sized, and later oop-mapped by every collector, from a different
        // class's `ref_disps`. `compact_tlab_total_size` refuses unless the
        // process has one layout domain -- the only case in which a bare
        // lookup is safe -- and is otherwise the same predicate (current
        // layout, exact field count, compact fields on). Single-VM processes
        // allocate exactly as before.
        let compact_snapshot: Option<(usize, *const u32, u32)> =
            cratonvm_types::compact_tlab_total_size(class_id_raw, num_fields).map(|total| {
                let (addr, expected) = cratonvm_types::layout_replace_guard(class_id_raw);
                (total, addr, expected)
            });
        // The compact instance's TOTAL size (8-byte header included).
        let compact_body: Option<usize> = compact_snapshot.map(|(total, _, _)| total);
        // The inline allocator's OWN legacy-fallback census.
        //
        // `plan_object_alloc` prints `[compact-legacy]` when the helper path
        // cannot find a matching layout; this path never reaches it, so an
        // inline `new` that falls back to the uniform 16-byte-cell layout was
        // INVISIBLE to that census — a class could allocate legacy on the
        // hottest path in the program and still be absent from the only report
        // that names legacy allocations. That blind spot is how
        // `SHA256Digest` came to be 100% of the `jit_getfield` helper's
        // receivers on Generational while appearing in no legacy census at all.
        if compact_body.is_none() && cratonvm_types::flags().gc.dbg_compact_legacy {
            eprintln!(
                "[compact-legacy] JIT inline-new class_id={class_id_raw} num_fields={num_fields} \
                 registered_field_count={:?} compact_enabled={} -> LEGACY object",
                cratonvm_types::class_layout(class_id_raw).map(|l| l.field_count()),
                cratonvm_types::compact_ref_fields_enabled(),
            );
        }
        // Object total size (header + body). Computed at compile time.
        let total_size = compact_body.unwrap_or(HEADER_SIZE + num_fields * SLOT_SIZE);
        // Cast: value to i32 (encoding immediate/displacement)
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;

        // Step 0: layout-replace guard (see the GUARDED RESTORE note above).
        // Runs before anything else so a stale-layout site diverts to the
        // helper with zero state to unwind. R11/RAX are scratch here.
        let layout_guard_patch = compact_snapshot.map(|(_, count_addr, expected)| {
            self.emit_mov_imm64_full(R11, count_addr as i64);
            self.emit_mov_r32_mem_disp32(RAX, R11, 0);
            // CMP EAX, imm32 (EAX-only short form 0x3D).
            self.buf.emit_byte(0x3D);
            self.buf.emit(&(expected as i32).to_le_bytes());
            self.emit_jcc_rel32_patch(0x85) // JNE slow_path
        });

        // Step 1: fetch the JvmThread*. Allocation-heavy methods cache it in
        // the prologue/OSR trampoline; otherwise use the small TLS helper.
        //
        // HIGH-2 / Fix 2 — direct `MOV reg, FS:[off]` TLS load is the
        // ideal sequence (saves ~5 ns per `new`). It is NOT applied
        // here in this round because it requires runtime cooperation
        // we do not yet have:
        //
        //   * Rust's `thread_local!` macro hides the TLS slot offset
        //     entirely — there is no portable API to extract the
        //     FS/GS-relative offset of `JIT_THREAD` at JIT-compile
        //     time. A `#[thread_local]` static (unstable on stable
        //     Rust) would still need a startup probe (inline asm
        //     `mov rax, fs:[OFFSET]` against a known sentinel) to
        //     recover the loader-assigned displacement.
        //   * On Windows the slot lives at GS:[0x58 + slot*8] where
        //     `slot` is allocated dynamically by `TlsAlloc`; the same
        //     probe machinery applies but with a different segment
        //     prefix and one extra indirection. Per task scope, this
        //     arm is intentionally left on the helper.
        //   * The current `JitRuntimeHelpers` table exposes only the
        //     helper function pointer; wiring an `Option<(SegPrefix,
        //     u32)>` field plus a startup probe in the VM is a
        //     cross-crate change outside the scope of this fix
        //     round.
        //
        // Until that plumbing lands, the helper call stays — see the
        // task notes for the planned approach.
        // Common case: one prologue/OSR helper call per invocation, not per `new`.
        if sink_spill {
            // Sink form: the cached slot is the ONLY thread source here. The
            // fallback below calls `get_current_thread`, and a CALL clobbers the
            // whole caller-saved file — which would invalidate the eleven
            // registers this site is about to spill at the slow-path label
            // instead of here. A null cached slot therefore diverts to
            // `new_object`, which resolves its own thread and allocates
            // correctly; `emit_prologue` writes this slot on every entry
            // (inherited on a proven self-call, fetched otherwise), so the
            // divert only happens for a genuinely non-Java thread, where the
            // fallback would have returned null and diverted anyway.
            self.emit_load_local(RAX, self.jit_thread_slot_off);
        } else if decline_form {
            // Decline form: no call may be emitted before the caller's slow
            // path (its operand lives in a caller-saved register). The cached
            // slot, else one `JIT_THREAD` mirror load; the entry check above
            // proved one of them exists. A null answer declines below.
            if self.jit_thread_slot_off != 0 {
                self.emit_load_local(RAX, self.jit_thread_slot_off);
            } else {
                // Cast: the mirror's TLS displacement, a small positive offset.
                self.emit_mov_rax_tls_disp32(jit_thread_tls_disp() as u32);
            }
        } else if self.jit_thread_slot_off != 0 {
            self.emit_load_local(RAX, self.jit_thread_slot_off);
            self.emit_test_r64_r64(RAX);
            let have_cached_thread = self.emit_jcc_rel32_patch(0x85); // JNE have_thread
            self.emit_fetch_current_thread_into_rax();
            self.emit_store_local(self.jit_thread_slot_off, RAX);
            self.patch_rel32_to_here(have_cached_thread);
        } else {
            self.emit_fetch_current_thread_into_rax();
        }
        self.emit_test_r64_r64(RAX);
        let null_thread_patch = self.emit_jcc_rel32_patch(0x84); // JE slow_path

        // R10 = thread; R11 = cursor.
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);

        // Align cursor up to 8 bytes (matches `Tlab::alloc(_, 8)`'s
        // behaviour). Without this, an interleaved array allocation that
        // left the cursor misaligned would force this `new` object onto a
        // non-8-aligned address — the GC walker assumes 8-aligned object
        // headers and would mis-decode the layout. Total cost: 2
        // instructions (8 bytes encoded) — negligible vs the cache miss
        // the slow path would incur.
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);

        // RAX = R11 + total_size (new cursor).
        self.emit_lea_r64_mem_disp32(RAX, R11, total_size as i32); // Cast: x86-64 immediate encoding

        // CMP RAX, [R10 + end_off]; JA slow_path (TLAB exhausted).
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        let tlab_full_patch = self.emit_jcc_rel32_patch(0x87); // JA slow_path

        // JVM default initialization and TLAB-reuse safety. All refill
        // backends return zeroed TLAB ranges, including cells reused by a
        // non-moving sweep, so the default path does not repeat those stores
        // per object. The opt-out retains the older defensive clear. Both
        // layouts are qword-sized here (legacy fields are 16 bytes; compact
        // fields are 8 or 16 bytes).
        //
        // This also makes the all-zero-tag primitive family (int, boolean,
        // byte, char, short) fully initialized inline as `Value::Int(0)`.
        // Only long/float/double need the post-init helper to install a non-zero
        // Value discriminant; reference fields in the compact layout are null
        // bare pointers after this clear.
        let zero_elision = inline_tlab_zero_elision_enabled();
        if !zero_elision {
            let body_start = if compact_body.is_some() {
                cratonvm_types::COMPACT_HEADER_SIZE
            } else {
                HEADER_SIZE
            };
            debug_assert_eq!((total_size - body_start) % 8, 0);
            self.emit_mov_imm32_sx(RDX, 0);
            for body_off in (body_start..total_size).step_by(8) {
                self.emit_mov_mem_disp32_r64(R11, RDX, body_off as i32);
            }
        }

        // BinTrees-18 heap-corruption fix (jit/gc audit, 2026-06):
        // *** Write the full object header BEFORE committing the TLAB
        // cursor. ***
        //
        // The previous order committed the bump (published the object's
        // address into `thread.tlab.cursor`) and only THEN wrote the
        // header fields. That left a window in which the object region was
        // already part of the "used" portion of the TLAB / young arena but
        // its header was still the TLAB-zeroed pattern (class_id=0,
        // kind=Object, num_slots=0). Any heap walk that observed the object
        // during that window — the non-moving young sweep that runs while
        // JIT frames are active (`gc_quiescence`), the Cheney to-space
        // scan, or a background-thread STW collection that parks this
        // mutator at a poll inside the in-between helper — computed
        // `size = HEADER_SIZE + 0*SLOT_SIZE = HEADER_SIZE` and stepped 40
        // bytes into the object's own field region. There it decoded the
        // first `Value` field cell (discriminant word = 4 = `Object`) as a
        // bogus header: `class_id=4`, `array_length=1` (upper half of the
        // 8-byte object-pointer payload), `num_slots=384` (the next cell's
        // discriminant region) — exactly the
        // "kind=Object but array_length=1 (num_slots=384, class_id=4)"
        // inconsistency reported by `gen_object_total_size`, after which
        // the walker desynced / looped (rc=124 timeout on `bintrees18`).
        //
        // Writing the header first means the object is fully walker-coherent
        // at the instant its address becomes reachable via the committed
        // cursor: the store to `cursor` below is the single linearization
        // point, and on x86-64 it is not reordered ahead of the header
        // stores (TSO: stores are not reordered with older stores). So no
        // walker can ever see a committed-but-unheadered object.
        //
        // Since the 8-byte header (2026-09-24):
        //   off 0     → class_id
        //   off 4     → the mark word: NEUTRAL, kind=Object, and the flag
        //               byte (GC_FLAG_HEADER, plus GC_FLAG_COMPACT for a
        //               compact instance) -- the walker's shape decision
        //   off 8     → LEGACY only: num_slots (the walker's stride,
        //               HEADER_SIZE + n*SLOT_SIZE) and the hash word at 12,
        //               written as "no hash yet". A COMPACT instance's
        //               header ends at 8; bytes 8.. are its fields and the
        //               walker sizes it from its layout's total_size.
        //
        // The identity hash is minted lazily on demand (a compact instance's
        // into the NEUTRAL mark word). The `jit_post_tlab_init` helper below
        // still runs for the primitive-init / finalizer paths, but the header
        // is already walker-coherent before the object is ever published.
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            class_id_off,
            class_id_raw as i32, // Cast: ClassId immediate fits in 32 bits
        );
        // The mark word: ONE dword at `MARK_WORD_OFFSET` (4), the second half
        // of the 8-byte header word. UNCONDITIONAL, and `zero_elision` must
        // never gate it: the mark word carries `kind`, `element_type`,
        // `gc_age` and `gc_flags`, and a TLAB slot that was not zero (which
        // has been observed on long runs) would otherwise leave a stale
        // `GC_FLAG_COMPACT` or `kind` on the object -- the failure that took
        // the Spring Boot suite down when this write was briefly elided.
        //
        // NEUTRAL, unhashed, kind=Object, element_type=0, age 0, and the flag
        // byte (`GC_FLAGS_BYTE_OFFSET` = byte 3 of this dword) holding
        // `GC_FLAG_HEADER` -- the bit that keeps a `new Object()` header word
        // from being all-zero -- plus `GC_FLAG_COMPACT` for a compact
        // instance. `header_offset_contract_gc_flags_live_in_the_mark_words_top_byte`
        // pins the byte position this shift relies on.
        let flags = if compact_body.is_some() {
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER
        } else {
            cratonvm_types::GC_FLAG_HEADER
        };
        if let Some(total) = compact_body {
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_COMPACT_INLINE") {
                eprintln!("[compact-inline] new class_id={class_id_raw} total={total}");
            }
        }
        let mark_imm = (flags as i32) << 24; // Cast: flag byte into the mark word's top byte
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::MARK_WORD_OFFSET as i32,
            mark_imm,
        );
        // A LEGACY instance has a long header: its field count at
        // `NUM_SLOTS_OFFSET` (8) and its identity-hash word at 12, written
        // explicitly for the same stale-TLAB reason as the mark word. A
        // compact instance has no such word -- bytes 8.. are its fields, left
        // as the TLAB's zeroes (or cleared by the opt-out loop above).
        if compact_body.is_none() {
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::NUM_SLOTS_OFFSET as i32,
                num_fields as i32, // Cast: field count fits in 32 bits
            );
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::IDENTITY_HASH_OFFSET as i32,
                0,
            );
        }

        // Commit the bump LAST: [R10 + cursor_off] = RAX. This publishes the
        // object's end as the new cursor (and, transitively, the object's
        // address as a live allocation). x86-64 TSO preserves the required
        // header/body-before-cursor store order; the STW handshake provides
        // the acquire side. Do not add an SFENCE here: it is unnecessary on
        // this backend and would tax every fast-path allocation.
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);

        if skip_post_init_helper {
            // CRIT-2 fast path — no primitive defaults to apply and no
            // finalizer to register. With class_id + num_slots already
            // written inline above, the header is complete enough for
            // both the GC walker and the runtime; no helper call needed.
            //
            // Class, kind/flags, shape and the whole mark word are explicitly
            // published above — the mark word unconditionally, and with
            // `GC_FLAG_HEADER` set, so the span is parseable as an object by
            // the collector's linear walk. Only the body defaults come from
            // the refill zeroing invariant.
            //
            // RAX = obj_ptr — both arms converge with RAX holding the
            // freshly-allocated object pointer.
            self.emit_mov_r64_r64(RAX, R11);
        } else if post_init == TlabPostInit::ZgcAnnounce {
            // Round 9 wave 4 (x64obj4), `NOTES-w3-x64obj3.md` request 3:
            // zgc_note_tlab_object(vm_ptr, obj_ptr, size_bytes) -> obj_ptr | 0.
            //
            // The header above is complete, exactly as on the `Skip` arm; the
            // helper (`vm/src/jit/helpers.rs::jit_zgc_note_tlab_object`) only
            // enters the object in ZGC's start registry and writes nothing.
            // `size_bytes` must be what the bump advanced the cursor by, which
            // is `total_size`, the LEA displacement above, and never a
            // re-derived size. A `0` return is a pending exception, and the
            // caller's `emit_post_alloc_oom_check` routes it exactly as it
            // routes `tlab_post_init`'s.
            //
            // Round 9 wave 8 (`zgc8`): with `inline_announce`, the common case
            // -- the thread's own armed chunk, no mark, not generational --
            // sets the start bit inline and jumps past the call. Every other
            // case falls through to the call unchanged.
            let inline =
                inline_announce.then(|| self.emit_zgc_inline_announce(cursor_off, end_off));
            if let Some((_, to_helper)) = &inline {
                for &b in to_helper {
                    self.patch_rel32_to_here(b);
                }
            }
            cratonvm_jit_api::assert_helper_call_shape!(
                "zgc_note_tlab_object",
                int_args = 3,
                returns_value = true
            );
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], total_size as i32); // Cast: <= 256, x86-64 imm32
            self.emit_call_absolute(self.helpers.zgc_note_tlab_object);
            // The inline arm rejoins here with RAX = obj_ptr, exactly where
            // the helper's answer lands.
            if let Some((inline_done, _)) = inline {
                self.patch_rel32_to_here(inline_done);
            }
        } else {
            // Hand off to post-init: tlab_post_init(vm_ptr, obj_ptr, cid, nf).
            // The helper now only does the cold work (identity-hash mint,
            // primitive-typed default values, finalizer registration); the
            // walker-coherent header bits (class_id + num_slots) are
            // already in place from the inline writes above.
            // (vm_ptr, obj_ptr, class_id, num_fields). FOUR registers, not
            // `new_object`'s three: the object already exists — the inline bump
            // above produced it — and the helper does only the cold work.
            cratonvm_jit_api::assert_helper_call_shape!(
                "tlab_post_init",
                int_args = 4,
                returns_value = true
            );
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], class_id_raw as i32); // Cast: ClassId fits in 32 bits
            self.emit_mov_imm32_sx(ARG_REGS[3], num_fields as i32); // Cast: x86-64 immediate encoding
            self.emit_call_absolute(self.helpers.tlab_post_init);
        }

        // Decline form: the fast path falls through with RAX = obj_ptr, and
        // every edge that would have reached `new_object` below is the
        // caller's. Nothing was withheld (the entry check refused the sink).
        if let Some(d) = decline {
            d.push(null_thread_patch);
            d.push(tlab_full_patch);
            if let Some(patch) = layout_guard_patch {
                d.push(patch);
            }
            return;
        }

        // Jump over the slow path; both arms converge with RAX = obj_ptr.
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        self.patch_rel32_to_here(null_thread_patch);
        self.patch_rel32_to_here(tlab_full_patch);
        if let Some(patch) = layout_guard_patch {
            // Layout-replace guard mismatch: the baked compact size is stale;
            // the helper allocates per the CURRENT layout.
            self.patch_rel32_to_here(patch);
        }
        // Every edge that reaches `new_object` — and therefore a collection —
        // converges here, so this is the one place the withheld half of the
        // safepoint's blind spill has to be. Emitted before the argument setup
        // below for the same reason the deopt stub spills before its own: the
        // ARG_REGS are part of the spilled file.
        self.emit_deferred_alloc_blind_spill();
        // The withheld shadow push, likewise: only this edge can collect.
        let shadow_here = std::mem::take(&mut self.deferred_alloc_shadow_push);
        if shadow_here {
            self.emit_shadow_push();
        }
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        // (vm_ptr, class_id, num_fields) -> obj | 0.
        cratonvm_jit_api::assert_helper_call_shape!(
            "new_object",
            int_args = 3,
            returns_value = true
        );
        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: ClassId fits in 32 bits
        self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32); // Cast: x86-64 immediate encoding
        self.emit_call_absolute(self.helpers.new_object);
        if shadow_here {
            // At the call's own return address, and with the reload that pops
            // the push above -- on this edge only.
            self.emit_oop_map_for_safepoint();
            self.alloc_safepoint_on_slow_edge = true;
        }

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
    }

    /// The largest array this path will bump inline, as an element COUNT.
    ///
    /// `tlab_alloc_array_guarded_refill` refuses anything whose total size
    /// reaches `cratonvm_gc::tlab::tlab_max_alloc()` and routes it to the
    /// ordinary path, which owns the young-vs-old-gen (humongous) routing
    /// decision. This path must not quietly take that decision away from it,
    /// so the same ceiling applies here — converted to a count at compile time,
    /// because a count is one unsigned `CMP` against the length register while
    /// a size is three more instructions after the shift.
    ///
    /// `the_inline_array_cap_tracks_the_allocator_s_own` pins the constant to
    /// the allocator's, which this crate can only see from a test
    /// (`cratonvm-gc` is a dev-dependency).
    pub(super) const INLINE_ARRAY_TLAB_MAX_ALLOC: usize = 32 * 1024;

    /// Whether a `newarray` site took the inline bump, and why one did not.
    ///
    /// BOTH, always, and for the reason `ir_alloc_site_counts` gives next door:
    /// a checksum from a workload whose arrays all took the helper proves
    /// nothing about the bump, so a zero on the left has to be
    /// distinguishable from "emitted and refused". The decline reasons are a
    /// work list — `no thread slot` and `zgc registration` mean different next
    /// steps, and a bare count cannot be acted on.
    fn note_inline_array_site(&self, why: Option<&'static str>) {
        match why {
            None => {
                INLINE_ARRAY_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(why) => {
                STUB_ONLY_ARRAY_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut g) = INLINE_ARRAY_DECLINES.lock() {
                    let v = g.get_or_insert_with(Vec::new);
                    match v.iter_mut().find(|(k, _)| *k == why) {
                        Some((_, n)) => *n += 1,
                        None => v.push((why, 1)),
                    }
                }
            }
        }
    }

    /// Inline TLAB bump allocation for a PRIMITIVE `newarray`, with the
    /// existing `helpers.newarray` call as its slow path.
    ///
    /// Returns `false` without emitting anything when the shape is not
    /// admitted, in which case the caller emits its helper call alone —
    /// exactly the previous behaviour.
    ///
    /// # Why this exists
    ///
    /// [`Self::emit_inline_tlab_new`] has given `new` a bump-and-publish
    /// sequence since 2024; `newarray` never had one. `jit_newarray`'s own body
    /// says so — "unlike the `new` site there is no inline TLAB bump in codegen
    /// for arrays, so this helper is not a slow path — it is the ONLY path a
    /// JIT-compiled `newarray` has" — and it is a long helper: a JIT-boundary
    /// note, an SATB drain, two concurrent-mark pokes, the atype decode, the
    /// size arithmetic, a thread-local fetch with a guard, and only then the
    /// bump this emits in eight instructions. Measured on this tree at 150 ns
    /// for `new byte[8]` against HotSpot's 10 ns, with `-Xmx` varied 4 GB →
    /// 16 GB to rule the collector out.
    ///
    /// Every `String` CratonVM creates is a `byte[]` plus a `String`, so this
    /// is most of what `Matcher.group()` costs.
    ///
    /// # PRIMITIVE only, deliberately
    ///
    /// `anewarray` keeps the helper. A reference element's size is
    /// `narrow_oop::ref_element_size()` — a value this emitter would have to
    /// bake and guard, exactly as the compact-layout snapshot above is baked
    /// and guarded. That is a second change the size of this one, and it buys a
    /// shape the String path does not allocate.
    ///
    /// # What is shared with `new`, and why
    ///
    /// The ordering argument is the same one, and it is the whole safety case:
    /// **every header word lands before the cursor commits**. The commit store
    /// is the single linearization point and x86-64 TSO does not reorder it
    /// ahead of older stores, so no walker can observe a committed object whose
    /// header is still whatever the TLAB slot held. See
    /// [`Self::emit_inline_tlab_new`] for the BinTrees-18 heap corruption that
    /// argument was paid for.
    ///
    /// The header is not hand-encoded: the mark word comes from
    /// `ObjectHeader::new`, the same constructor `TlabShape::init_header` calls,
    /// so the bytes this stamps and the bytes the allocator stamps agree by
    /// construction rather than by inspection.
    pub(super) fn emit_inline_tlab_newarray(
        &mut self,
        element_type: cratonvm_types::ArrayElementType,
        atype: i32,
        count_slot: StackSlot,
    ) -> bool {
        use cratonvm_types::{ClassId, ObjectHeader, ObjectKind};

        // Name the refusal rather than returning a bare `false`. This arm is
        // default-ON and carries the same documented heap-corruption risk its
        // object-shaped sibling does, and the sibling's own history is that a
        // silent `false` is how a path comes to be held responsible for a
        // number it never produced: a census across all five collectors found
        // `emit_inline_tlab_new_ir` emitting ZERO sites in every one of them,
        // while its doc comment was being cited as the reason another feature
        // stayed shut.
        let declined = if !inline_tlab_newarray_enabled() {
            // This arm's OWN opt-out, separate from the `new` one below it, so
            // the array bump can be priced and bisected without also taking
            // away the object bump that has been shipping since 2024. Both
            // arms of an A/B are then one binary, which is the only kind of
            // control this project trusts on a host whose absolute timings
            // drift — see `docs/benchmarking/methodology.md`.
            Some("CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY")
        } else if !inline_tlab_new_enabled() {
            // The `new` opt-out covers this arm too: one lever for "route every
            // compiled allocation through its always-correct helper" is worth
            // more than two that have to be remembered together.
            Some("CRATONVM_NO_JIT_INLINE_TLAB_NEW")
        } else if !inline_tlab_zero_elision_enabled() {
            // Zero elision OFF means "re-emit the defensive per-object clears".
            // `new` can: its body size is a compile-time constant and the clears
            // are a straight-line run of stores. An array's body length is a
            // RUNTIME value, so the same request is a loop — and the point of
            // the lever is to bisect against a path that behaves like the
            // helper. Handing arrays back to the helper IS that behaviour.
            Some("zero elision off")
        } else if cratonvm_types::jit_tlab_registration_required() {
            // A collector whose sweep is driven by an allocation-base registry
            // rather than by walking the chunk (ZGC) has to be told about every
            // object, and an inline bump has nothing to tell it with — the `new`
            // arm keeps `jit_post_tlab_init` for exactly this, and there is no
            // array-shaped twin of that helper to keep. An unannounced object is
            // not an object to `is_object_address`, and its first use as a
            // receiver decodes as `null`.
            Some("tlab registration required")
        } else if !self.needs_heap {
            // `vm_ptr` in the heap slot is what the slow path needs.
            Some("no heap slot")
        } else if self.helpers.newarray == 0 {
            Some("helpers.newarray is null")
        } else if self.helpers.get_current_thread == 0 && self.jit_thread_slot_off == 0 {
            // …and a thread is what the fast path needs.
            Some("no thread source")
        } else if element_type == cratonvm_types::ArrayElementType::Reference {
            // A reference array's element size is `ref_element_size()`, which
            // this arm does not bake — see the doc comment.
            Some("reference element")
        } else {
            None
        };
        if let Some(why) = declined {
            self.note_inline_array_site(Some(why));
            return false;
        }

        let elem_size = cratonvm_types::element_byte_size(element_type);
        let log2_elem = match elem_size {
            1 => 0u8,
            2 => 1,
            4 => 2,
            8 => 3,
            // Unreachable for the primitive family, and a refusal rather than an
            // assert: a new element width should cost coverage, not a panic.
            _ => {
                self.note_inline_array_site(Some("element width"));
                return false;
            }
        };
        // `array_data_size_checked` rounds the data area up to 8, so the count
        // cap is derived from the size cap — the rounding cannot then push a
        // just-admitted array past it.
        let max_len =
            (Self::INLINE_ARRAY_TLAB_MAX_ALLOC - cratonvm_types::ARRAY_DATA_OFFSET) / elem_size;
        let Ok(max_len) = i32::try_from(max_len) else {
            self.note_inline_array_site(Some("cap does not fit imm32"));
            return false;
        };
        // Every header displacement below is encoded as a disp8.
        if cratonvm_types::IDENTITY_HASH_OFFSET + 4 > 127
            || cratonvm_types::ARRAY_LENGTH_OFFSET > 127
            || cratonvm_types::ARRAY_DATA_OFFSET > 127
        {
            self.note_inline_array_site(Some("header offset past disp8"));
            return false;
        }

        // Cast: value to i32 (encoding immediate/displacement)
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;

        let mut slow: Vec<usize> = Vec::new();

        // Step 1 — the cached `JvmThread*`, exactly as the `new` arm fetches it.
        if self.jit_thread_slot_off != 0 {
            self.emit_load_local(RAX, self.jit_thread_slot_off);
            self.emit_test_r64_r64(RAX);
            let have_cached_thread = self.emit_jcc_rel32_patch(0x85); // JNE have_thread
            self.emit_fetch_current_thread_into_rax();
            self.emit_store_local(self.jit_thread_slot_off, RAX);
            self.patch_rel32_to_here(have_cached_thread);
        } else {
            self.emit_fetch_current_thread_into_rax();
        }
        self.emit_test_r64_r64(RAX);
        slow.push(self.emit_jcc_rel32_patch(0x84)); // JE slow_path

        // Step 2 — the length, and the ONE check that screens both a negative
        // length and an oversized one.
        //
        // RCX holds whatever the operand slot holds, which `jit_newarray`'s own
        // body documents may be a NaN-boxed `CompactValue` rather than a bare
        // integer — it narrows with `length as i32 as i64` for that reason.
        // Reading ECX is that same narrowing, so the two paths agree on what the
        // length is.
        //
        // The compare is UNSIGNED (`JA`), which is what makes it one
        // instruction: a negative length reads as >= 0x8000_0000, so it is above
        // the cap and diverts to the helper — which raises
        // `NegativeArraySizeException` through the pending-exception channel,
        // the behaviour this arm must not change.
        self.load_slot_to_reg(RCX, count_slot);
        self.buf.emit_byte(0x81); // CMP ECX, imm32
        self.buf.emit_byte(0xF9);
        self.buf.emit(&max_len.to_le_bytes());
        slow.push(self.emit_jcc_rel32_patch(0x87)); // JA slow_path

        // Step 3 — cursor, aligned up to 8. An interleaved allocation can leave
        // it unaligned and the walker assumes 8-aligned headers.
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);

        // Step 4 — the data size, then the bump. This is
        // `array_data_size_checked` in four instructions: `length * elem_size`,
        // rounded up to 8. `length` is capped above and `elem_size` is at most
        // 8, so the shift cannot reach the pointer range and the `checked_mul`
        // its Rust twin needs has nothing left to catch.
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX  (zero-extends into RDX)
        if log2_elem != 0 {
            self.buf.emit(&[0x48, 0xC1, 0xE2, log2_elem]); // SHL RDX, log2_elem
        }
        self.emit_add_r64_imm8(RDX, 7);
        self.emit_and_r64_imm8(RDX, -8);
        // LEA RAX, [R11 + RDX + ARRAY_DATA_OFFSET] — the end of the object,
        // which is also the new cursor.
        self.buf.emit(&[0x49, 0x8D, 0x44, 0x13]);
        // Cast: bounded by the disp8 screen above.
        self.buf.emit_byte(cratonvm_types::ARRAY_DATA_OFFSET as u8);
        // CMP RAX, [R10 + end_off]; JA slow_path (TLAB exhausted).
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        slow.push(self.emit_jcc_rel32_patch(0x87)); // JA slow_path

        // Step 5 — the whole header, before the commit below.
        //
        // `class_id` is `ClassId::new(0)` because that is what `jit_newarray`
        // passes `tlab_alloc_array_guarded_refill` for a primitive array: the
        // element type lives in the mark word, not in a class.
        self.emit_mov_dword_mem_disp32_imm32(R11, 0, 0);
        // `shape` IS the length for an array kind — `ObjectHeader::new` picks
        // `array_length` over `num_slots` on `ObjectKind::Array`.
        // MOV DWORD [R11 + ARRAY_LENGTH_OFFSET], ECX
        self.buf.emit(&[0x41, 0x89, 0x4B]);
        // Cast: bounded by the disp8 screen above.
        self.buf
            .emit_byte(cratonvm_types::ARRAY_LENGTH_OFFSET as u8);
        // The mark word, taken from the constructor the allocator itself calls
        // rather than re-derived here: `kind`, `element_type`, `gc_age` and
        // `gc_flags` live in its top two bytes, and `GC_FLAG_HEADER` must be
        // among them or the span is not parseable as an object by the
        // collector's linear walk (a wholly zero header is reclaimed arena
        // space). One dword: the mark word is the header word's high half.
        let mark = ObjectHeader::new(ClassId::new(0), ObjectKind::Array, element_type, 0, 0)
            .mark_word
            .load(std::sync::atomic::Ordering::Relaxed);
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::MARK_WORD_OFFSET as i32,
            mark as i32, // Cast: the mark word's bits as an imm32
        );
        // The identity-hash word (offset 12): "no hash yet".
        self.emit_mov_dword_mem_disp32_imm32(R11, cratonvm_types::IDENTITY_HASH_OFFSET as i32, 0);

        // The BODY is not cleared here, and Java requires it to read as zero. It
        // does: every production `VmHeap::refill_tlab` backend returns a fully
        // zeroed chunk, including cells reused by a non-moving sweep. That is
        // the same invariant `emit_inline_tlab_new` relies on for its field
        // slots under `inline_tlab_zero_elision_enabled` — and this arm has
        // already declined when that lever is off.

        // Step 6 — commit the bump LAST. x86-64 TSO preserves the
        // header-before-cursor store order; do not add an SFENCE.
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);
        // Both arms converge with RAX = the array pointer.
        self.emit_mov_r64_r64(RAX, R11);
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        for patch in slow {
            self.patch_rel32_to_here(patch);
        }
        // `jit_newarray(vm, atype, length)`. The caller has already emitted the
        // pre-safepoint spill, and emits the oop map and the OOM check after the
        // merge, so this edge needs only the arguments.
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.emit_mov_imm32_sx(ARG_REGS[1], atype);
        self.load_slot_to_reg(ARG_REGS[2], count_slot);
        // (vm_ptr, atype, length) -> array | 0.
        cratonvm_jit_api::assert_helper_call_shape!("newarray", int_args = 3, returns_value = true);
        self.emit_call_absolute(self.helpers.newarray);

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
        self.note_inline_array_site(None);
        true
    }

    // -----------------------------------------------------------------------
    // Array access, bounds checks and null checks
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/arrays.rs`.

    /// Emit an `ldc <Class>` site: call `helpers.ldc_class_cp` and push the
    /// returned mirror as an oop. Returns `false` when this pc is not a
    /// class-`ldc` **or** the site cannot be served (the helper is unwired, or
    /// this artifact has no VM context to pass it), leaving the caller to fall
    /// through to the immediate/string arms or refuse the method.
    ///
    /// The mirror is re-fetched on every execution rather than baked, exactly
    /// as `helpers.ldc_string` re-interns its String: both are heap objects a
    /// relocating collector may move between two runs of this body.
    /// `ldc <String>` — the interned literal named at `cp_idx` in
    /// `holder_class_id`'s constant pool, materialised by
    /// `helpers.ldc_string_cp`.
    ///
    /// The exact twin of [`Self::emit_ldc_class`] below, down to the stub: the
    /// two helpers take the same `(vm_ptr, holder_class_id, cp_idx)` shape and
    /// share the same `0 = pending exception` convention, so they share the
    /// emitter. That is what keeps the two sites' ABIs from drifting apart.
    ///
    /// Returns `false` when the site is not a string `ldc`, when the helper is
    /// unwired (a hand-built test table) or when the artifact has no context
    /// slot — and then the caller bails the site exactly as it did before this
    /// helper existed.
    pub(super) fn emit_ldc_string(&mut self, pc: usize) -> bool {
        let Some(&idx) = self.ldc_string_info_idx.get(&pc) else {
            return false;
        };
        if self.helpers.ldc_string_cp == 0 || !self.needs_heap {
            return false;
        }
        let (_, holder_class_id, cp_idx) = self.ldc_string_info[idx];
        let slot_addr = self.ldc_site_slot(holder_class_id, cp_idx);
        // Interning allocates, so the helper arm is a real safepoint. A `0`
        // return is a published pending exception (the constant-pool entry
        // could not be re-read), not a value — the same convention
        // `emit_ldc_class` takes.
        self.emit_ldc_cp_site(
            self.helpers.ldc_string_cp,
            holder_class_id,
            cp_idx,
            slot_addr,
        );
        true
    }

    /// The GC-maintained slot the VM handed out for the `ldc` site
    /// `(holder_class_id, cp_idx)`, or `0` when there is none.
    ///
    /// The slot comes from `vm::jit::helpers::compiled_ldc_slot_for` through
    /// the compile-time `cp_ldc_resolver` (`JitLdcConstant::StringSlot` /
    /// `ClassMirrorSlot`), then `BackendRequest::ldc_slots` and
    /// `Compiler::ldc_slots` (round 9 wave 7b, wire7b). An empty map (the
    /// kill switch `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0`, a door that
    /// fills none, or a site with no slot yet) answers `0`, and the site keeps
    /// the helper call, byte for byte.
    ///
    /// Keyed by the SITE `(holder, cp index)`, not by bytecode pc, so the
    /// bytecode loop rewriter's pc remapping cannot pair a slot with the
    /// wrong `ldc`.
    fn ldc_site_slot(&self, holder_class_id: u32, cp_idx: u16) -> usize {
        self.ldc_slots
            .get(&(holder_class_id, cp_idx))
            .copied()
            .unwrap_or(0)
    }

    /// Emit one constant-pool-indexed `ldc` site (`ldc <String>` or
    /// `ldc <Class>`), leaving the reference pushed and oop-marked.
    ///
    /// With `slot_addr == 0` this is exactly the sequence both sites always
    /// emitted: spill, `helper(vm, holder, cp_idx)`, oop map, pending-exception
    /// check. With a slot it first tries the slot:
    ///
    /// ```text
    ///   (flush the R8/R9/XMM operand-stack cache to the frame)
    ///   MOV  RAX, imm64 slot_addr
    ///   MOV  RAX, [RAX]          ; the resolved constant, GC-maintained
    ///   TEST RAX, RAX
    ///   JNZ  done                ; a filled slot IS the whole instruction
    ///   <spill; CALL helper; oop map; exception check>   ; unchanged
    /// done:
    ///   push RAX (oop)
    /// ```
    ///
    /// Why the word can be read without a barrier and without a call: the slot
    /// is a JNI-global-reference `Box<ObjectRef>` owned by this VM. The
    /// collector reports it as a root and rewrites it in place
    /// (`JniGlobalRefs::update_after_gc`) during every moving collection, and
    /// it does that only while this thread is parked at a safepoint, which a
    /// thread executing this sequence is not. That is the same guarantee every
    /// raw reference a compiled frame holds in a register relies on. The value
    /// never changes otherwise: JVMS §5.4.3 resolves the entry once.
    ///
    /// Why the fast arm must come BEFORE the spill rather than after it:
    /// `emit_pre_safepoint_spill` pushes the frame's oops onto the shadow
    /// stack, and only `emit_oop_map_for_safepoint`, after the CALL, pops them.
    /// A fast arm that jumped from after the spill to `done` would leak one
    /// shadow push per execution. Instead, both arms start from the same
    /// compile-time operand-stack model: the scratch-register cache is flushed
    /// first, so the spill's own flush on the helper arm finds nothing to move,
    /// and the two arms meet at `done` with identical `self.stack`. The fast
    /// arm clobbers only RAX and the flags, a strict subset of what the CALL
    /// clobbers.
    pub(super) fn emit_ldc_cp_site(
        &mut self,
        helper: usize,
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    ) {
        let done = if slot_addr != 0 {
            self.flush_scratch_registers();
            Some(self.emit_ldc_slot_probe(slot_addr))
        } else {
            None
        };
        self.emit_pre_safepoint_spill();
        crate::runtime_lowering::emit_ldc_class_cp_stub(
            &mut self.buf,
            self.heap_local_offset,
            helper,
            holder_class_id,
            cp_idx,
            self.helpers.frame_record,
        );
        self.emit_identity_after_shared_stub();
        self.emit_oop_map_for_safepoint();
        self.emit_post_alloc_oom_check();
        if let Some(patch) = done {
            self.patch_rel32_to_here(patch);
            // `done` is a join: no reload elision may pair a store emitted on
            // the helper arm with a load after it.
            self.slot_mirror = None;
        }
        self.push_from_rax();
        self.mark_top_as_oop();
    }

    /// `MOV RAX, slot; MOV RAX, [RAX]; TEST RAX, RAX; JNZ rel32`, returning the
    /// JNZ's patch offset. RAX holds the slot's word on both outcomes; the
    /// branch is taken when the slot is filled.
    fn emit_ldc_slot_probe(&mut self, slot_addr: usize) -> usize {
        // Cast: a heap address as the imm64 operand of MOVABS.
        self.emit_mov_imm64(RAX, slot_addr as i64);
        self.emit_mov_r64_mem_disp32(RAX, RAX, 0);
        self.emit_test_r64_r64(RAX);
        self.emit_jcc_rel32_patch(0x85) // JNZ done
    }

    pub(super) fn emit_ldc_class(&mut self, pc: usize) -> bool {
        let Some(&idx) = self.ldc_class_info_idx.get(&pc) else {
            return false;
        };
        if self.helpers.ldc_class_cp == 0 || !self.needs_heap {
            return false;
        }
        let (_, holder_class_id, cp_idx) = self.ldc_class_info[idx];
        let slot_addr = self.ldc_site_slot(holder_class_id, cp_idx);
        // Resolution can load a class — arbitrary Java, hence a GC point — so
        // the helper arm is a real safepoint, and its `0` return is a published
        // pending exception (`NoClassDefFoundError` and friends), not a value.
        // A failed resolution is never recorded, so it never gets a slot.
        self.emit_ldc_cp_site(
            self.helpers.ldc_class_cp,
            holder_class_id,
            cp_idx,
            slot_addr,
        );
        true
    }

    /// The three published reference-store gate addresses, or `None` when this
    /// process's collector did not publish a plan.
    ///
    /// All three or none: a plan with a live pre-gate and a zero post-gate
    /// would let compiled code skip the post barrier on the strength of a word
    /// nobody maintains, so the tuple is destructured as a unit and a single
    /// zero declines the whole fast path.
    pub(super) fn ref_store_gates(&self) -> Option<(usize, usize, usize)> {
        ref_store_gates_of(&self.helpers)
    }

    /// The published post-barrier skip mask, when the plan uses that shape.
    pub(super) fn ref_store_post_skip_mask(&self) -> Option<u8> {
        ref_store_post_skip_mask_of(&self.helpers)
    }

    /// `LOCK INC qword [counter]` when the single-pass reference-store path
    /// trace is on, and nothing at all otherwise.
    ///
    /// Uses R11, which is scratch at every call site here, and clobbers flags
    /// — so it is only ever emitted where the next instruction sets them again
    /// or does not read them.
    fn emit_ref_store_path_trace(&mut self, counter: &'static std::sync::atomic::AtomicU64) {
        if !crate::x64::sp_ref_store_trace_enabled() {
            return;
        }
        self.emit_mov_imm64_full(R11, counter as *const _ as i64);
        self.buf.emit_byte(0xF0); // LOCK
        self.buf.emit_byte(0x49); // REX.W + REX.B
        self.buf.emit_byte(0xFF); // INC r/m64 (/0)
        self.buf.emit_byte(0x03); // ModRM mod=00 reg=000 rm=011 (R11)
    }

    /// Guard a baked COMPACT CELL OFFSET against a layout replacement, and
    /// return the patch site the caller must route to its helper.
    ///
    /// Every emitter that bakes `HEADER_SIZE + packed_body_offset` as an
    /// immediate is making a compile-time claim about a layout that the class
    /// manager can replace at run time — `recompute_subclass_layouts`, the
    /// synthetic-stub→real-bytecode upgrade. The two ALLOCATION emitters have
    /// guarded that since perf/halfgap-20260717, and the comment there calls an
    /// unguarded baked layout "confirmed heap corruption". The FIELD-ACCESS
    /// emitters never had one: they resolve their offset from a constant-pool
    /// entry and have no class id to name a per-class counter with.
    ///
    /// So they guard on the process-wide replacement epoch instead — coarser,
    /// and affordable because only a REPLACEMENT bumps it, never a new class
    /// registration. Two instructions; a mismatch permanently routes the site
    /// to the always-correct helper.
    ///
    /// The compare is `CMP DWORD [rip+disp32], imm32` — see
    /// [`Self::emit_cmp_mem32_abs_imm32`] — which is why "two" and not the
    /// "four" this comment said until 2026-09-10.
    ///
    /// Whether that form is REACHABLE is not this emitter's decision. The
    /// counter has to be within ±2GB of the buffer, which took moving it off
    /// `.data` (Windows: ~140TB away as a `static`) and then, on System V,
    /// moving the CODE — `jit::platform`'s `near_globals`, where
    /// `mmap(NULL, …)` had been putting buffers ~130TB from the heap the
    /// counter lives in. Where neither holds, the range check below declines
    /// and the long form is emitted. See `LAYOUT_REPLACE_EPOCH` in
    /// `cratonvm_types::field_layout`, and
    /// `docs/internal/performance/c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`.
    ///
    /// Reach is best-effort, so the materialize-the-address form stays as the
    /// fallback — and `CRATONVM_JIT_SP_EPOCH_GUARD_RIP=0` selects it
    /// deliberately rather than waiting for an address space that produces it.
    ///
    /// Returns `None` when the caller should emit no guard at all, which today
    /// never happens — the epoch address is always available — but keeps the
    /// shape honest if that ever changes.
    pub(super) fn emit_layout_epoch_guard(&mut self) -> Option<usize> {
        let (addr, expected) = cratonvm_types::layout_replace_epoch_guard();
        if addr.is_null() {
            return None;
        }
        if !jit_sp_epoch_guard_rip_enabled()
            || !self.emit_cmp_mem32_abs_imm32(addr as usize, expected)
        {
            // Out of ±2GB RIP reach, or the encoding switch is off:
            // materialize the address and read through it. This is the shape
            // the guard had before 2026-09-10, kept verbatim as the fallback.
            self.emit_mov_imm64_full(R11, addr as i64);
            self.emit_mov_r32_mem_disp32(RCX, R11, 0);
            // CMP ECX, imm32.
            self.buf.emit_byte(0x81);
            self.buf.emit_byte(0xF9);
            self.buf.emit(&(expected as i32).to_le_bytes());
        }
        Some(self.emit_jcc_rel32_patch(0x85)) // JNE -> helper
    }

    /// Emit a compact reference `putfield` whose barriers are **gated inline**
    /// rather than paid as a call.
    ///
    /// Returns `false` without emitting anything when the shape is not
    /// admitted, in which case the caller keeps whichever arm it has today.
    ///
    /// # What makes this sound
    ///
    /// Every gate below names a PREFIX of the barrier helper's own control
    /// flow, read from the word the helper itself reads:
    ///
    /// | inline test | the helper's own first act |
    /// |---|---|
    /// | `pre_active == 0` | `satb_pre_barrier` loads `mark_active` and returns |
    /// | `flags_byte < young_floor` | `note_ref_store_slow` compares `gc_age` to the promotion age and returns |
    /// | `post_active == 0` | `note_ref_store` loads `has_old_objects` and returns |
    ///
    /// So a skipped call is a call that would have returned having done
    /// nothing. On any other answer this path calls the collector's OWN
    /// `write_barrier`, which is the same code that records the edge today —
    /// no remembered-set contract is reimplemented here, which is the mistake
    /// the previous inline store path made and what
    /// `inline_card_mark_available` was hard-`false`d to stop.
    ///
    /// The gates are published conservatively: each may read "there may be
    /// work" while the truth is "no work" (a call that was not needed), and
    /// never the reverse. See `gc::gen_heap::JitRefStoreGates`.
    ///
    /// # What this path does NOT have to prove, and why that is the win
    ///
    /// The arm it replaces required the field's **old value to be null**, so
    /// every re-assignment of an already-set reference took the helper. That
    /// condition existed to make the SATB pre-barrier unnecessary; with
    /// `pre_active` read directly, the old value stops mattering and the
    /// ordinary `node.next = other` store stays inline.
    ///
    /// It also drops the published-region containment test, which under the
    /// default collector could never pass — the emitter laid down six compares
    /// against an all-zero table and then called the helper anyway. Receiver
    /// validity is still established, by the READ-side bounds table for an
    /// unproven receiver and by a null test for a type-tracker-proven oop.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_gated_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        cell_off: i32,
        receiver_is_trusted_oop: bool,
    ) -> bool {
        let Some((pre, post, floor)) = self.ref_store_gates() else {
            return false;
        };
        // The value has to survive to the store and, on the barriered path, to
        // the helper call — both of which read it out of its frame slot, so no
        // register constraint travels across the guards.
        // The bail sets are kept APART rather than in one vector so the
        // run-time trace can say which gate refused — see `SP_REF_STORE_BAIL`.
        // Without the trace they are concatenated and all land on the same
        // helper label, which is exactly what they did before this split.
        let mut bail_recv: Vec<usize> = Vec::new();
        let mut bail_pre: Vec<usize> = Vec::new();
        // The baked `cell_off` below is a compile-time claim about this class's
        // compact layout. See `emit_layout_epoch_guard`.
        let mut bail_layout: Vec<usize> = Vec::new();
        bail_layout.extend(self.emit_layout_epoch_guard());
        self.load_slot_to_reg(RAX, obj_slot);

        // ── receiver validity ───────────────────────────────────────────
        //
        // The READ table (`read_bounds_addr`), not the store table. The
        // question here is only "is this address one this heap handed out, so
        // the header reads below cannot fault" — the barrier question is the
        // gates' job now, and conflating the two is what left this arm dead
        // under every non-publishing collector.
        bail_recv.extend(if receiver_is_trusted_oop {
            self.emit_trusted_oop_receiver_check()
        } else {
            self.emit_guarded_getfield_receiver_check(self.helpers.read_bounds_addr)
        });

        // ── SATB pre-barrier gate ───────────────────────────────────────
        // Marking armed ⇒ the overwritten reference has to reach the snapshot,
        // which is the helper's job. Rare: armed only during a concurrent
        // mark phase.
        self.emit_mov_imm64_full(R11, pre as i64);
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        bail_pre.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper

        // ── the receiver flags byte, read ONCE ──────────────────────────
        // `GC_FLAGS_BYTE_OFFSET` carries the flags in bits 0..3 and `gc_age` in
        // bits 4..7, so this single byte answers both the young-receiver
        // question the post gate asks and the per-object layout question the
        // store shape below asks. Read as a byte rather than as the dword the
        // older arms use: that dword starts 15 bytes into a 16-byte header and
        // takes three of its four bytes from the first instance field.
        self.emit_movzx_r32_mem8(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);

        // ── the store, in whichever shape this OBJECT has ───────────────
        //
        // Both layouts store inline, and the reason is measured rather than
        // assumed. This arm used to send a non-compact receiver to the helper,
        // which made it an arm that essentially never fired:
        // `init_object_header` — the TLAB fast path serving nearly every
        // allocation for the interpreter and `jit_new_object` alike — writes a
        // LEGACY header unconditionally (`array_length = 0`, no
        // `GC_FLAG_COMPACT`) whatever layout the class has registered, because
        // it never consults `plan_object_alloc`. A run-time path census on
        // `RefStoreLoopProbe` put a number on it: `inline=0` out of
        // **16,380,000**, every one bailing at the compactness test, while the
        // compile-time census reported `gated=2 declined=0` and looked healthy.
        //
        // It is the same trap the inline `getfield` read fell into and climbed
        // out of on 2026-08-18, and the optimizing tier's twin of this arm on
        // 2026-09-02. The fix is theirs: emit both shapes and pick per OBJECT
        // on the header bit, exactly as `jit_putfield_object` does.
        self.load_slot_to_reg(RDX, val_slot);
        // gen r4w4/cards4: PRE half of the inline generational barrier (the
        // plan here is the generational MASK shape whenever the card view is
        // published, so its young-receiver test is this arm's own). Its slow
        // arm is a call, so CL — the flags byte — is re-read either way.
        let inline_card = self.inline_card_mark_available();
        if inline_card {
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[(RAX, obj_slot), (RDX, val_slot)]);
            self.emit_movzx_r32_mem8(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);
        }
        self.emit_test_r8_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT);
        let legacy_shape = self.emit_jcc_rel32_patch(0x84); // JZ → the 16-byte cell
                                                            // COMPACT: a reference field is the bare 8-byte pointer at the cell
                                                            // base, which is what the guarded inline `getfield` reads back.
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        let shaped = self.emit_jmp_rel32_patch();
        // LEGACY: the uniform 16-byte `Value` cell — tag qword (the dword tag
        // plus its pad) then the pointer payload. `field_index < num_slots` was
        // checked above, and for a legacy object `num_slots` counts exactly
        // these cells, which is what makes this stride addressable.
        self.patch_rel32_to_here(legacy_shape);
        // ── slot bounds, LEGACY shape only ───────────────────────────────
        // `field_index < num_slots`. A failure DROPS the store, matching
        // `jit_putfield_object`'s own out-of-bounds behaviour, so it targets
        // its own label rather than the helper. Only a legacy (long-header)
        // object has a field count at `NUM_SLOTS_OFFSET`; a compact
        // instance's 8-byte header has none -- offset 8 is its first field --
        // and its layout is its class's, which the verifier already proved
        // declares this field.
        self.emit_mov_r32_mem_disp32(R11, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32);
        self.emit_mov_imm64(R10, field_index as i64);
        self.emit_cmp_r32_r32(R10, R11);
        let oob = self.emit_jcc_rel32_patch(0x83); // JAE → drop
        let legacy_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: disp32
        self.emit_mov_imm64(R10, i64::from(cratonvm_types::FIELD_CELL_TAG_OBJECT));
        self.emit_mov_mem_disp32_r64(
            RAX,
            R10,
            legacy_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32,
        );
        self.emit_mov_mem_disp32_r64(
            RAX,
            RDX,
            legacy_off + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32,
        );
        self.patch_rel32_to_here(shaped);
        // Counted here rather than at the top: everything above can still
        // leave for the helper, and "reached the store" is the fact the
        // compile-time census cannot supply.
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_INLINE_TAKEN);

        // ── post-barrier gates ──────────────────────────────────────────
        // CL still holds the receiver's flags byte, which carries `gc_age` in
        // bits 4..7 and the GC flags in bits 0..3. Two shapes can rule the post
        // barrier out, and a publisher supplies exactly one of them
        // (`ref_store_gates` enforces that).
        let mut done: Vec<usize> = Vec::new();
        // A NULL value records no edge — `jit_write_barrier`'s own first test,
        // and the gate the `aastore` twin of this sequence has always had.
        // Without it every `obj.ref = null` into an old receiver paid the
        // helper call (r11 x64gen). RDX still holds the stored value: the
        // stores above read it and the path trace touches only R11.
        self.emit_test_r64_r64(RDX);
        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null value, no card
        if let Some(mask) = self.ref_store_post_skip_mask() {
            // MASK — "the receiver carries none of the bits that could make a
            // post barrier necessary". The generational collector's shape:
            // `GC_FLAG_OLD_GEN` clear means the receiver is young, and a young
            // receiver needs no card. One `test r8, imm8` with no memory
            // operand, because the mask is a property of which collector is
            // running and that cannot change after start-up.
            self.emit_test_r8_imm8(RCX, mask);
            done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → young receiver, no card
        } else {
            // FLOOR — `age << 4 | flags` compared unsigned against
            // `promotion_floor << 4` is an EXACT test of
            // `gc_age < promotion_floor`, because the flags nibble is at most
            // 15 and cannot carry `a << 4` up to `(a + 1) << 4`.
            self.emit_mov_imm64_full(R11, floor as i64);
            self.emit_cmp_r8_mem8(RCX, R11, 0);
            done.push(self.emit_jcc_rel32_patch(0x82)); // JB → young receiver, no card
        }

        self.emit_mov_imm64_full(R11, post as i64);
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → no old objects, no card

        // Neither gate could rule the barrier out: run the collector's own.
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_BARRIER_TAKEN);
        if inline_card {
            // gen r4w4/cards4: POST half — the card check, and the collector's
            // barrier only for a clean card. RAX (receiver) and RDX (value)
            // survive the store, both shapes, the trace and both gates.
            self.emit_gen_card_barrier(RAX, RDX, RAX, &[]);
        } else {
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
            self.load_slot_to_reg(ARG_REGS[2], val_slot);
            // (vm_ptr, obj_ptr, val_ptr) -> ().
            cratonvm_jit_api::assert_helper_call_shape!(
                "write_barrier",
                int_args = 3,
                returns_value = false
            );
            self.emit_call_absolute(self.helpers.write_barrier);
        }
        done.push(self.emit_jmp_rel32_patch());

        // ── helper fallback: the full SATB + post barrier + store ───────
        //
        // With the trace on, each bail set gets a one-instruction stub naming
        // it before joining the helper; without it they all land here directly
        // and cost nothing.
        let bail_groups = [(bail_recv, 0usize), (bail_pre, 1), (bail_layout, 2)];
        let mut to_helper: Vec<usize> = Vec::new();
        if crate::x64::sp_ref_store_trace_enabled() {
            for (patches, reason) in bail_groups {
                if patches.is_empty() {
                    continue;
                }
                for b in patches {
                    self.patch_rel32_to_here(b);
                }
                self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_BAIL[reason]);
                to_helper.push(self.emit_jmp_rel32_patch());
            }
        } else {
            for (patches, _) in bail_groups {
                to_helper.extend(patches);
            }
        }
        for b in to_helper {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_HELPER_TAKEN);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 imm32
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        // (vm_ptr, obj_ptr, field_index, value) -> ().
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_object",
            int_args = 4,
            returns_value = false
        );
        self.emit_call_absolute(self.helpers.putfield_object);

        self.patch_rel32_to_here(oob);
        for d in done {
            self.patch_rel32_to_here(d);
        }
        note_gated_ref_store();
        true
    }
}

// x86-64 only: this module EXECUTES the x86-64 bytes it emits, which on
// aarch64 is not an illegal instruction the harness can report but a `SIGSEGV`
// that takes the whole crate's test binary with it -- every test after it in
// the run simply never happens. Found 2026-09-21 by running the suite as a
// real aarch64 binary under `qemu-user`. Gated at the MODULE, for the reason
// `x64/tests.rs` and `ir_lower`'s test module already are: the property is
// "this module is about x86-64", not a per-test accident.
#[cfg(all(test, target_arch = "x86_64"))]
mod r9w4_zgc_announce_tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    #[test]
    fn the_post_init_mode_follows_the_three_questions() {
        let wired = 0x1234usize;
        assert_eq!(tlab_post_init_mode(true, true, wired), TlabPostInit::Skip);
        assert_eq!(tlab_post_init_mode(true, true, 0), TlabPostInit::Skip);
        // ZGC: a no-op class still has to be announced.
        assert_eq!(
            tlab_post_init_mode(false, true, wired),
            TlabPostInit::ZgcAnnounce
        );
        // ...and without the thin helper it keeps the full one.
        assert_eq!(tlab_post_init_mode(false, true, 0), TlabPostInit::Helper);
        // A class with real post-init work never takes the announce helper:
        // it writes no primitive default and registers no finalizer.
        assert_eq!(
            tlab_post_init_mode(false, false, wired),
            TlabPostInit::Helper
        );
        assert_eq!(tlab_post_init_mode(false, false, 0), TlabPostInit::Helper);
    }

    static NOTE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static NOTE_ARGS: [AtomicI64; 3] = [AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)];
    static POST_INIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static POST_INIT_ARGS: [AtomicI64; 4] = [
        AtomicI64::new(0),
        AtomicI64::new(0),
        AtomicI64::new(0),
        AtomicI64::new(0),
    ];
    static NEW_OBJECT_CALLS: AtomicUsize = AtomicUsize::new(0);

    // SAFETY: extern "C" test stub; records its integer arguments in statics
    // and dereferences nothing.
    unsafe extern "C" fn stub_note(vm: i64, obj: i64, size: i64) -> i64 {
        NOTE_CALLS.fetch_add(1, Ordering::SeqCst);
        NOTE_ARGS[0].store(vm, Ordering::SeqCst);
        NOTE_ARGS[1].store(obj, Ordering::SeqCst);
        NOTE_ARGS[2].store(size, Ordering::SeqCst);
        obj
    }
    // SAFETY: as above.
    unsafe extern "C" fn stub_post_init(vm: i64, obj: i64, cid: i64, nf: i64) -> i64 {
        POST_INIT_CALLS.fetch_add(1, Ordering::SeqCst);
        POST_INIT_ARGS[0].store(vm, Ordering::SeqCst);
        POST_INIT_ARGS[1].store(obj, Ordering::SeqCst);
        POST_INIT_ARGS[2].store(cid, Ordering::SeqCst);
        POST_INIT_ARGS[3].store(nf, Ordering::SeqCst);
        obj
    }
    // SAFETY: as above; returns a sentinel no bump can produce.
    unsafe extern "C" fn stub_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
        NEW_OBJECT_CALLS.fetch_add(1, Ordering::SeqCst);
        0x0BAD_0B1E
    }
    // SAFETY: as above.
    unsafe extern "C" fn stub_null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    /// Far above any class a test registers a compact layout for, so the
    /// allocation is the legacy `HEADER_SIZE + n * SLOT_SIZE` shape.
    const CID: u32 = 0x0765_4321;
    const NF: usize = 2;
    const VM: i64 = 0x0C0F_FEE0;

    /// Emit `emit_inline_tlab_new(CID, NF, mode)` inside a tiny frame of its
    /// own, run it against a fake thread (`[cursor, end]`), and return RAX.
    fn run(mode: TlabPostInit, thread: &mut [u64; 2]) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        // Cast: fn pointer to usize helper address (all five below).
        c.helpers.get_current_thread = stub_null_thread as *const () as usize;
        c.helpers.tlab_post_init = stub_post_init as *const () as usize;
        c.helpers.new_object = stub_new_object as *const () as usize;
        c.helpers.zgc_note_tlab_object = stub_note as *const () as usize;
        c.helpers.tlab_cursor_offset_in_thread = 0;
        c.helpers.tlab_end_offset_in_thread = 8;
        c.helpers.class_id_offset_in_obj = 0;
        // [rbp-8] = vm_ptr, [rbp-16] = the cached JvmThread*. A non-null
        // cached slot means the emitter never fetches the thread itself.
        c.heap_local_offset = 8;
        c.jit_thread_slot_off = 16;

        // `Compiler::new` already emitted a prologue; enter after it.
        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55]); // push rbp
        c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40: aligned, with shadow space
        c.emit_store_local(8, ARG_REGS[0]);
        c.emit_store_local(16, ARG_REGS[1]);
        // Without the inline start-bit store whatever the environment says:
        // this fake thread is only `[cursor, end]` (that arm has its own
        // test, `r9w8_zgc_inline_announce_tests`).
        c.emit_inline_tlab_new_with(CID, NF, mode, false, None);
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());

        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: the snippet builds and tears down its own frame, takes two
        // i64 arguments in the platform's first two argument registers, writes
        // only through the fake thread and the TLAB it points at (both alive
        // across the call), calls only the stubs above, and returns an i64.
        // `c` owns the buffer and outlives the call.
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f(VM, thread.as_mut_ptr() as i64);
        drop(c);
        r
    }

    /// Round 9 wave 4 (`NOTES-w3-x64obj3.md` request 3): the ZGC mode bumps
    /// and publishes the header exactly as the skip arm does, then calls
    /// `zgc_note_tlab_object(vm_ptr, obj_ptr, size_bytes)` with the footprint
    /// the bump advanced the cursor by, and returns the helper's answer. It
    /// never reaches `tlab_post_init` or `new_object`. The other two modes are
    /// run through the same harness as controls.
    #[test]
    fn the_zgc_mode_announces_the_bumped_object_and_nothing_else() {
        if !inline_tlab_new_enabled() {
            return; // opted out by the environment: the arm is a bare helper call
        }
        let size = (HEADER_SIZE + NF * SLOT_SIZE) as i64;
        for mode in [
            TlabPostInit::ZgcAnnounce,
            TlabPostInit::Helper,
            TlabPostInit::Skip,
        ] {
            NOTE_CALLS.store(0, Ordering::SeqCst);
            POST_INIT_CALLS.store(0, Ordering::SeqCst);
            NEW_OBJECT_CALLS.store(0, Ordering::SeqCst);
            let mut heap = vec![0u64; 64];
            let base = heap.as_mut_ptr() as i64;
            let mut thread: [u64; 2] = [base as u64, base as u64 + 512];

            let r = run(mode, &mut thread);

            assert_eq!(r, base, "{mode:?}: RAX is the bumped object");
            assert_eq!(
                thread[0] as i64,
                base + size,
                "{mode:?}: the cursor advanced by one object"
            );
            assert_eq!(heap[0] as u32, CID, "{mode:?}: class id published");
            assert_eq!(
                NEW_OBJECT_CALLS.load(Ordering::SeqCst),
                0,
                "{mode:?}: no slow path"
            );
            match mode {
                TlabPostInit::ZgcAnnounce => {
                    assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 1);
                    assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 0);
                    assert_eq!(NOTE_ARGS[0].load(Ordering::SeqCst), VM, "vm_ptr");
                    assert_eq!(NOTE_ARGS[1].load(Ordering::SeqCst), base, "obj_ptr");
                    assert_eq!(NOTE_ARGS[2].load(Ordering::SeqCst), size, "size_bytes");
                }
                TlabPostInit::Helper => {
                    assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 0);
                    assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 1);
                    assert_eq!(POST_INIT_ARGS[0].load(Ordering::SeqCst), VM);
                    assert_eq!(POST_INIT_ARGS[1].load(Ordering::SeqCst), base);
                    assert_eq!(POST_INIT_ARGS[2].load(Ordering::SeqCst), i64::from(CID));
                    assert_eq!(POST_INIT_ARGS[3].load(Ordering::SeqCst), NF as i64);
                }
                TlabPostInit::Skip => {
                    assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 0);
                    assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 0);
                }
            }
            drop(heap);
        }
    }

    /// What the decline edges of the snippet below return: no bump and no
    /// helper can produce it.
    const DECLINED: i64 = -1;

    /// The DECLINE form (round 12 wave 1, lane `calls`): the bump inside a
    /// tiny frame whose every decline edge lands on `MOV RAX, DECLINED`.
    fn run_declining(thread: &mut [u64; 2], thread_slot: u64) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        // Cast: fn pointer to usize helper address (all three below).
        c.helpers.get_current_thread = stub_null_thread as *const () as usize;
        c.helpers.tlab_post_init = stub_post_init as *const () as usize;
        c.helpers.new_object = stub_new_object as *const () as usize;
        c.helpers.tlab_cursor_offset_in_thread = 0;
        c.helpers.tlab_end_offset_in_thread = 8;
        c.helpers.class_id_offset_in_obj = 0;
        c.heap_local_offset = 8;
        c.jit_thread_slot_off = 16;

        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55]); // push rbp
        c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
        c.emit_store_local(8, ARG_REGS[0]);
        c.emit_store_local(16, ARG_REGS[1]);
        let mut decline: Vec<usize> = Vec::new();
        c.emit_inline_tlab_new_with(CID, NF, TlabPostInit::Skip, false, Some(&mut decline));
        let done = c.emit_jmp_rel32_patch();
        for p in decline {
            c.patch_rel32_to_here(p);
        }
        c.emit_mov_imm64(RAX, DECLINED);
        c.patch_rel32_to_here(done);
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());

        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: as `run` above; the snippet calls nothing at all.
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let slot = if thread_slot == 0 {
            0
        } else {
            thread.as_mut_ptr() as i64
        };
        let r = f(VM, slot);
        drop(c);
        r
    }

    /// The decline form bumps exactly as the `new` site does when the TLAB
    /// has room, and otherwise lands on the CALLER's edge with the cursor
    /// untouched: a full TLAB and a null cached thread both decline, and
    /// neither ever reaches `new_object` (whose stub answers a sentinel no
    /// decline returns).
    #[test]
    fn r12_the_decline_form_bumps_or_declines_and_never_calls() {
        if !inline_tlab_new_enabled() {
            return; // opted out by the environment: the form declines outright
        }
        let mut heap = vec![0u64; 64];
        let base = heap.as_mut_ptr() as u64;

        let mut thread: [u64; 2] = [base, base + 512];
        assert_eq!(run_declining(&mut thread, 1), base as i64, "room: the object");
        // The footprint is the `new` site's own (the ZGC test above pins its
        // value); read it back rather than restate it.
        let size = thread[0] - base;
        assert!(size >= 16 && size % 8 == 0, "room: the cursor advanced by {size}");
        assert_eq!(heap[0] as u32, CID, "room: class id published");

        let mut full: [u64; 2] = [base, base + size - 8];
        assert_eq!(run_declining(&mut full, 1), DECLINED, "full TLAB declines");
        assert_eq!(full[0], base, "full TLAB: the cursor is untouched");

        let mut unused: [u64; 2] = [base, base + 512];
        assert_eq!(run_declining(&mut unused, 0), DECLINED, "null thread declines");
        assert_eq!(unused[0], base, "null thread: nothing bumped");
        drop(heap);
    }
}

// x86-64 only: this module EXECUTES the x86-64 bytes it emits, which on
// aarch64 is not an illegal instruction the harness can report but a `SIGSEGV`
// that takes the whole crate's test binary with it -- every test after it in
// the run simply never happens. Found 2026-09-21 by running the suite as a
// real aarch64 binary under `qemu-user`. Gated at the MODULE, for the reason
// `x64/tests.rs` and `ir_lower`'s test module already are: the property is
// "this module is about x86-64", not a per-test accident.
#[cfg(all(test, target_arch = "x86_64"))]
mod r9w7_ldc_slot_tests {
    use super::*;

    /// What the miss arm of the test snippet loads, so a miss is visible.
    const MISS: i64 = 0x5EED_0BAD;

    /// Emit `emit_ldc_slot_probe(slot)` inside a tiny frame, with the miss
    /// arm loading [`MISS`], and return RAX. This is the fast path
    /// `emit_ldc_cp_site` puts in front of the helper call.
    fn run_probe(slot: &std::sync::atomic::AtomicU64) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55]); // push rbp
        c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        let done = c.emit_ldc_slot_probe(slot as *const std::sync::atomic::AtomicU64 as usize);
        c.emit_mov_imm64(RAX, MISS);
        c.patch_rel32_to_here(done);
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: the snippet builds and tears down its own frame, reads one
        // aligned word through `slot` (alive across the call), calls nothing
        // and returns an i64. `c` owns the buffer and outlives the call.
        let f: extern "C" fn() -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f();
        drop(c);
        r
    }

    /// Round 9 wave 7 (`perf-compiled-ldc-string-is-a-helper-call-per-execution`):
    /// a filled slot answers the `ldc` with no call, and the answer is read
    /// THROUGH the slot on every execution, so a collector that rewrites the
    /// slot in place is followed. An empty slot falls to the helper arm.
    #[test]
    fn the_ldc_slot_probe_reads_through_the_slot_and_misses_on_zero() {
        let slot = std::sync::atomic::AtomicU64::new(0x0000_7F00_1234_5670);
        assert_eq!(run_probe(&slot), 0x0000_7F00_1234_5670);
        // The collector moved the literal: the same code answers the new word.
        slot.store(0x0000_7F00_8765_4320, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(run_probe(&slot), 0x0000_7F00_8765_4320);
        slot.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            run_probe(&slot),
            MISS,
            "an empty slot must take the helper arm"
        );
    }

    /// With a slot, the site starts with the probe (so the shadow-stack push in
    /// the spill is on the helper arm only), still pushes exactly one oop, and
    /// leaves no operand in a scratch register (the two arms meet with one
    /// stack model). With no slot, no probe is emitted.
    #[test]
    fn a_slotted_ldc_site_probes_first_and_pushes_one_oop() {
        // Any in-reach address: the snippet is emitted, never executed.
        let helper = bounds_check_probe_target as *const () as usize;
        let fake_slot: usize = 0x7FFF_1234_5678_0000;
        for slot in [fake_slot, 0] {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            c.heap_local_offset = 8;
            let start = c.buf.pos();
            let depth = c.stack.len();
            c.emit_ldc_cp_site(helper, 0x0123, 7, slot);
            assert!(c.buf.codegen_failure_reason().is_none());
            assert_eq!(c.stack.len(), depth + 1, "slot={slot:#x}: one push");
            assert_eq!(
                c.stack_oop_marks.last(),
                Some(&true),
                "slot={slot:#x}: an oop"
            );
            assert!(
                !c.stack.iter().any(|s| matches!(s, StackSlot::Scratch(_))),
                "slot={slot:#x}: no operand may stay in a scratch register across the join"
            );
            // SAFETY: `start..start + 10` was just written by this compiler.
            let head = unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(start), 10) };
            let mut movabs = vec![0x48u8, 0xB8];
            movabs.extend_from_slice(&(fake_slot as u64).to_le_bytes());
            if slot != 0 {
                assert_eq!(head, &movabs[..], "the probe comes before anything else");
            } else {
                assert_ne!(head, &movabs[..], "no slot, no probe");
            }
        }
    }

    // SAFETY: never called; only its address is baked into unexecuted code.
    unsafe extern "C" fn bounds_check_probe_target(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0
    }
}

// x86-64 only: this module EXECUTES the x86-64 bytes it emits, which on
// aarch64 is not an illegal instruction the harness can report but a `SIGSEGV`
// that takes the whole crate's test binary with it -- every test after it in
// the run simply never happens. Found 2026-09-21 by running the suite as a
// real aarch64 binary under `qemu-user`. Gated at the MODULE, for the reason
// `x64/tests.rs` and `ir_lower`'s test module already are: the property is
// "this module is about x86-64", not a per-test accident.
#[cfg(all(test, target_arch = "x86_64"))]
mod r9w8_zgc_inline_announce_tests {
    use super::*;

    /// The JIT's copies of the collector's layout constants are the
    /// collector's (`cratonvm-gc` is reachable from a test, not from the
    /// emitter).
    #[test]
    fn the_offsets_match_the_collector_s() {
        use cratonvm_gc::tlab::JitZgcAnnounceTable as T;
        assert_eq!(
            ZGC_TLAB_CURSOR_OFFSET as usize,
            cratonvm_gc::Tlab::CURSOR_OFFSET
        );
        assert_eq!(
            ZGC_TLAB_START_OFFSET as usize,
            cratonvm_gc::Tlab::START_OFFSET
        );
        assert_eq!(
            ZGC_TLAB_OWNED_EPOCH_OFFSET as usize,
            cratonvm_gc::Tlab::ZGC_OWNED_EPOCH_OFFSET
        );
        assert_eq!(
            ZGC_TLAB_ANNOUNCE_OFFSET as usize,
            cratonvm_gc::Tlab::ZGC_ANNOUNCE_OFFSET
        );
        assert_eq!(ZGC_ANNOUNCE_WORDS as usize, T::WORDS_OFFSET);
        assert_eq!(ZGC_ANNOUNCE_BASE as usize, T::BASE_OFFSET);
        assert_eq!(ZGC_ANNOUNCE_SPAN as usize, T::SPAN_OFFSET);
        assert_eq!(ZGC_ANNOUNCE_BLOCKED as usize, T::BLOCKED_OFFSET);
        assert_eq!(ZGC_ANNOUNCE_EPOCH as usize, T::EPOCH_OFFSET);
    }

    /// What the stubs saw. Reached through the `vm_ptr` argument every
    /// helper receives first (the harness passes this struct's address as the
    /// VM pointer), so the test needs no process-global state
    /// (`jit/tests/process_global_statics_ratchet.rs`).
    #[repr(C)]
    #[derive(Default)]
    struct Calls {
        note: u64,
        note_obj: i64,
        other: u64,
    }

    // SAFETY: extern "C" test stub; `vm` is the address of the live `Calls`
    // the harness passed as the VM pointer, and nothing else is dereferenced.
    unsafe extern "C" fn stub_note(vm: i64, obj: i64, _size: i64) -> i64 {
        let calls = &mut *(vm as *mut Calls);
        calls.note += 1;
        calls.note_obj = obj;
        obj
    }
    // SAFETY: as above.
    unsafe extern "C" fn stub_post_init(vm: i64, obj: i64, _cid: i64, _nf: i64) -> i64 {
        (*(vm as *mut Calls)).other += 1;
        obj
    }
    // SAFETY: as above; returns a sentinel no bump can produce.
    unsafe extern "C" fn stub_new_object(vm: i64, _cid: i64, _nf: i64) -> i64 {
        (*(vm as *mut Calls)).other += 1;
        0x0BAD_0B1E
    }
    // SAFETY: extern "C" test stub that dereferences nothing.
    unsafe extern "C" fn stub_null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    /// Far above any class a test registers a compact layout for.
    const CID: u32 = 0x0765_4322;
    const NF: usize = 2;

    /// The fake `JvmThread`: its `Tlab` at offset 0, laid out as the
    /// collector's (`cursor, end, start, zgc_owned_epoch, zgc_announce`).
    #[repr(C)]
    #[allow(dead_code)] // read by the emitted code, not by Rust
    struct FakeThread {
        cursor: u64,
        end: u64,
        start: u64,
        owned_epoch: u64,
        announce: u64,
    }

    /// The fake `JitZgcAnnounceTable` (`owner` is never read by the JIT).
    #[repr(C)]
    #[allow(dead_code)] // read by the emitted code, not by Rust
    struct FakeTable {
        words: u64,
        base: u64,
        span: u64,
        blocked: u64,
        epoch: u64,
        owner: u64,
    }

    /// Emit the `ZgcAnnounce` inline `new` WITH the inline start-bit store,
    /// run it against `thread` with `calls` as the VM pointer, and return RAX.
    fn run(thread: &mut FakeThread, calls: &mut Calls) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        // Cast: fn pointer to usize helper address (all four below).
        c.helpers.get_current_thread = stub_null_thread as *const () as usize;
        c.helpers.tlab_post_init = stub_post_init as *const () as usize;
        c.helpers.new_object = stub_new_object as *const () as usize;
        c.helpers.zgc_note_tlab_object = stub_note as *const () as usize;
        c.helpers.tlab_cursor_offset_in_thread = 0;
        c.helpers.tlab_end_offset_in_thread = 8;
        c.helpers.class_id_offset_in_obj = 0;
        c.heap_local_offset = 8;
        c.jit_thread_slot_off = 16;

        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55]); // push rbp
        c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
        c.emit_store_local(8, ARG_REGS[0]);
        c.emit_store_local(16, ARG_REGS[1]);
        c.emit_inline_tlab_new_with(CID, NF, TlabPostInit::ZgcAnnounce, true, None);
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());

        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: the snippet builds and tears down its own frame, takes two
        // i64 arguments, reads the fake thread and the fake table it names,
        // writes the TLAB and the bitmap they name (all alive across the
        // call), calls only the stubs above (which write only `calls`), and
        // returns an i64. `c` owns the buffer and outlives the call.
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f(calls as *mut Calls as i64, thread as *mut FakeThread as i64);
        drop(c);
        r
    }

    /// One way to break (or keep) the contract, applied to a fully valid setup:
    /// `(thread, table, heap base)`.
    type Edit = fn(&mut FakeThread, &mut FakeTable, u64);

    fn keep(_: &mut FakeThread, _: &mut FakeTable, _: u64) {}
    fn unarm(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
        t.announce = 0;
    }
    fn stale_epoch(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
        t.owned_epoch -= 1;
    }
    fn marking(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
        tb.blocked = 1;
    }
    fn unpublished(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
        tb.words = 0;
    }
    fn below_base(_: &mut FakeThread, tb: &mut FakeTable, base: u64) {
        tb.base = base + 1024;
    }
    fn beyond_span(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
        tb.span = 520;
    }
    fn word_starts_before_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
        t.start = base + 516;
    }
    fn word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
        t.end = base + 1000;
    }

    /// Round 9 wave 8 (`perf-zgc-compiled-new-always-takes-the-rust-helper`):
    /// with every condition of `JitZgcAnnounceTable`'s contract met, the
    /// object's start bit is set inline and the announce helper is NOT called;
    /// each condition broken on its own sends the site to the helper with the
    /// bitmap untouched. Every arm returns the bumped object and reaches no
    /// other helper.
    #[test]
    fn the_inline_start_bit_store_sets_the_bit_or_defers_to_the_helper() {
        if !inline_tlab_new_enabled() {
            return; // opted out by the environment: the arm is a bare helper call
        }
        // Object at heap offset 520: bit 65 = word 1, bit 1.
        const OFF: u64 = 520;
        let cases: [(&str, bool, Edit); 9] = [
            ("armed", true, keep),
            ("unarmed buffer", false, unarm),
            ("stale epoch", false, stale_epoch),
            ("marking", false, marking),
            ("unpublished", false, unpublished),
            ("below base", false, below_base),
            ("beyond span", false, beyond_span),
            (
                "word starts before the chunk",
                false,
                word_starts_before_chunk,
            ),
            ("word ends after the chunk", false, word_ends_after_chunk),
        ];
        for (name, inline, edit) in cases {
            let mut calls = Calls::default();
            let mut heap = vec![0u64; 256]; // 2 KiB of "arena"
            let base = heap.as_mut_ptr() as u64;
            let mut bits = vec![0u64; 8];
            let mut table = FakeTable {
                words: bits.as_mut_ptr() as u64,
                base,
                span: 2048,
                blocked: 0,
                epoch: 41,
                owner: base,
            };
            let mut thread = FakeThread {
                cursor: base + OFF,
                end: base + 2048,
                start: base,
                owned_epoch: 41,
                announce: 1,
            };
            edit(&mut thread, &mut table, base);
            if thread.announce != 0 {
                thread.announce = &table as *const FakeTable as u64;
            }

            let r = run(&mut thread, &mut calls);

            let obj = (base + OFF) as i64;
            // The footprint the bump took (header plus two legacy cells),
            // read back rather than re-derived from the layout constants.
            let size = thread.cursor - (base + OFF);
            assert!(size > 0 && size % 8 == 0, "{name}: bumped by {size}");
            assert_eq!(r, obj, "{name}: RAX is the bumped object");
            assert_eq!(heap[(OFF / 8) as usize] as u32, CID, "{name}: class id");
            assert_eq!(calls.other, 0, "{name}: no other helper");
            if inline {
                assert_eq!(calls.note, 0, "{name}: no call");
                assert_eq!(bits[1], 1u64 << 1, "{name}: the object's start bit");
                assert!(
                    bits.iter().enumerate().all(|(i, w)| i == 1 || *w == 0),
                    "{name}: no other bit"
                );
                // A second object in the same word keeps the first bit.
                let r2 = run(&mut thread, &mut calls);
                assert_eq!(r2, obj + size as i64, "{name}: second object");
                let bit2 = ((OFF + size) / 8) % 64;
                assert_eq!(bits[1], (1u64 << 1) | (1u64 << bit2), "{name}: both bits");
                assert_eq!(calls.note, 0, "{name}: still no call");
            } else {
                assert_eq!(calls.note, 1, "{name}: the helper");
                assert_eq!(calls.note_obj, obj, "{name}: obj_ptr");
                assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
            }
            // `table` is not dropped explicitly: `FakeTable` is six `u64`s with no
            // `Drop` impl and no borrows, so `drop(table)` said nothing — and is
            // what clippy's `drop_non_drop` refuses. `bits` and `heap` own
            // allocations, so their drops are real and stay.
            drop(bits);
            drop(heap);
        }
    }
}

// gen r4w4/cards4 (2026-09-24): the inline generational card barrier at the
// EMITTER level — the check executed on its own (every case it separates), the
// full barrier executed in a frame against a recording `write_barrier`, the
// element-address encoding, and the view contract. The store-FORM coverage
// (aastore, both reference-putfield arms, the flag without a view) executes
// through the backend in `jit/tests/r4w4_cards4_inline_card_barrier.rs`.
// x86-64 only, for the reason the modules above give.
#[cfg(all(test, target_arch = "x86_64"))]
mod r4w4_cards4_tests {
    use super::*;
    use cratonvm_gc::card_table::{CardTable, CardTableOptions, JitCardView, CARD_SIZE};

    const PRECISE: CardTableOptions = CardTableOptions {
        summary: true,
        precise_ref_array_marks: true,
    };

    /// The JIT's copies of the view layout are the collector's.
    #[test]
    fn the_card_view_offsets_match_the_collector_s() {
        assert_eq!(
            CARD_VIEW_MAGIC,
            cratonvm_gc::card_table::JIT_CARD_VIEW_MAGIC
        );
        assert_eq!(CARD_VIEW_OLD_BASE as usize, JitCardView::OLD_BASE_OFFSET);
        assert_eq!(CARD_VIEW_OLD_END as usize, JitCardView::OLD_END_OFFSET);
        assert_eq!(CARD_VIEW_CARDS_NEG as usize, JitCardView::CARDS_NEG_OFFSET);
        assert_eq!(CARD_VIEW_FLAGS, JitCardView::FLAGS_OFFSET);
        assert_eq!(
            CARD_VIEW_FLAG_PRECISE_REF_ARRAYS,
            JitCardView::FLAG_PRECISE_REF_ARRAYS
        );
        assert_eq!(CARD_DIRTY_BYTE, cratonvm_gc::card_table::CARD_DIRTY);
        assert_eq!(
            1usize << cratonvm_types::CARD_SHIFT,
            CARD_SIZE,
            "the emitted shift indexes the table's cards"
        );
    }

    fn helpers_for(view: usize, lo: usize, hi: usize) -> cratonvm_jit_api::JitRuntimeHelpers {
        let mut h = cratonvm_jit_api::JitRuntimeHelpers::default();
        h.jit_card_table_addr = view;
        h.jit_card_old_base = lo;
        h.jit_card_old_end = hi;
        h
    }

    /// The compile-time reading of the view: magic, alignment, a non-empty
    /// range, and the precise flag.
    #[test]
    fn the_card_view_is_read_only_when_it_is_one() {
        let buf = vec![0u64; 512];
        let (lo, hi) = (buf.as_ptr() as usize, buf.as_ptr() as usize + 4096);
        let precise = CardTable::with_options(lo, 4096, PRECISE);
        let legacy = CardTable::with_options(lo, 4096, CardTableOptions::LEGACY);
        let pv = precise.jit_card_view_addr();
        assert_eq!(gen_card_view_of(&helpers_for(pv, lo, hi)), Some((pv, true)));
        let lv = legacy.jit_card_view_addr();
        assert_eq!(
            gen_card_view_of(&helpers_for(lv, lo, hi)),
            Some((lv, false))
        );
        assert_eq!(gen_card_view_of(&helpers_for(0, lo, hi)), None, "no view");
        assert_eq!(
            gen_card_view_of(&helpers_for(pv, hi, lo)),
            None,
            "empty range"
        );
        assert_eq!(
            gen_card_view_of(&helpers_for(pv + 4, lo, hi)),
            None,
            "misaligned"
        );
        let not_a_view = [0usize; 5];
        let nv = not_a_view.as_ptr() as usize;
        assert_eq!(
            gen_card_view_of(&helpers_for(nv, lo, hi)),
            None,
            "wrong magic"
        );
        assert!(!precise.raw_card_address_escaped());
    }

    /// gen r4w5/review5: a table that is NOT a view is declined on its FIRST
    /// word alone. The old code read word 4 (`flags`, 32 bytes in) before it
    /// compared the magic, i.e. read past the end of a table shorter than a
    /// view — here, one 8-byte word (Miri reports the old read as
    /// out-of-bounds; a native run cannot see it, which is why the fix is the
    /// ordering and this test pins the answer on the smallest input).
    #[test]
    fn a_non_view_is_declined_on_its_first_word() {
        let buf = vec![0u64; 512];
        let (lo, hi) = (buf.as_ptr() as usize, buf.as_ptr() as usize + 4096);
        let one_word: Box<usize> = Box::new(CARD_VIEW_MAGIC ^ 1);
        let addr = &*one_word as *const usize as usize;
        assert_eq!(gen_card_view_of(&helpers_for(addr, lo, hi)), None);
    }

    /// `emit_ref_element_address` is `LEA dst, [array + index*scale + 16]`,
    /// encoded by the verified `isel` row: `48 8D 4C C8 10` for RCX from
    /// RAX/RCX with 8-byte elements (`4C` = mod 01, reg RCX, SIB; `C8` = *8,
    /// RCX, RAX; `10` = HEADER_SIZE), `88` in the SIB for 4-byte narrow ones.
    #[test]
    fn the_element_address_is_one_lea() {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let start = c.buf.pos();
        c.emit_ref_element_address(RCX, RAX, RCX);
        let end = c.buf.pos();
        // SAFETY: `[start, end)` is the prefix of the live buffer just written.
        let bytes = unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(start), end - start) };
        let sib = if cratonvm_types::narrow_oop::ref_element_size() == 8 {
            0xC8
        } else {
            0x88
        };
        // Deliberately not the header constant cast straight to a byte: the
        // x64 header-offset tripwire counts that spelling as an emission site
        // (`flag_and_header_contracts.rs`), and this is a test's expected
        // byte, not one.
        let disp8 = u8::try_from(HEADER_SIZE).expect("the header is a disp8");
        assert_eq!(bytes, &[0x48, 0x8D, 0x4C, sib, disp8][..]);
        assert!(c.buf.codegen_failure_reason().is_none());
    }

    /// A simulated old generation: a zeroed, 8-aligned buffer and a card table
    /// over exactly it.
    struct Old {
        buf: Vec<u64>,
        ct: CardTable,
    }

    impl Old {
        fn new() -> Self {
            let buf = vec![0u64; 4096]; // 32 KiB = 64 cards
            let ct = CardTable::with_options(buf.as_ptr() as usize, buf.len() * 8, PRECISE);
            Self { buf, ct }
        }
        fn base(&self) -> usize {
            self.buf.as_ptr() as usize
        }
        fn end(&self) -> usize {
            self.base() + self.buf.len() * 8
        }
        /// A header at byte offset `off` with the given flags byte.
        fn header(&mut self, off: usize, flags: u8) -> usize {
            let a = self.base() + off;
            // SAFETY: `off + 16` is inside the buffer for every caller.
            unsafe { *((a + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *mut u8) = flags };
            a
        }
        fn dirty(&self, addr: usize) -> bool {
            self.ct.is_dirty((addr - self.base()) / CARD_SIZE)
        }
    }

    /// Emit `emit_gen_card_check(RAX, RDX, mark)` as a leaf returning 1 on
    /// the slow edge and 0 on the done edge, and run it on
    /// `(holder, value, mark_address)`.
    fn run_check(view: usize, element_mark: bool, holder: usize, val: usize, mark: usize) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.helpers.jit_card_table_addr = view;
        let entry_off = c.buf.pos();
        // RAX = holder, RDX = value, RCX = mark, on either ABI: park the second
        // and third arguments in R11/R10 first (neither is an argument
        // register), then place them.
        c.emit_mov_r64_r64(RAX, ARG_REGS[0]);
        c.emit_mov_r64_r64(R11, ARG_REGS[1]);
        c.emit_mov_r64_r64(R10, ARG_REGS[2]);
        c.emit_mov_r64_r64(RDX, R11);
        c.emit_mov_r64_r64(RCX, R10);
        let mark_reg = if element_mark { RCX } else { RAX };
        let (slow, done) = c.emit_gen_card_check(RAX, RDX, mark_reg);
        for p in slow {
            c.patch_rel32_to_here(p);
        }
        c.emit_mov_imm64(RAX, 1);
        c.emit_ret();
        for p in done {
            c.patch_rel32_to_here(p);
        }
        c.emit_xor_reg_self(RAX);
        c.emit_ret();
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: a leaf taking three i64s in the platform's first three
        // argument registers, clobbering only RAX/RCX/RDX/R10/R11 (caller-saved
        // on both ABIs), reading the holder's flags byte (a live test buffer)
        // and the card view and map of a live `CardTable`, returning an i64.
        let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        // Casts: test addresses as the i64 arguments of the C ABI.
        let r = f(holder as i64, val as i64, mark as i64);
        drop(c);
        r
    }

    /// Every case the check separates, EXECUTED — and not one card byte
    /// written by it.
    #[test]
    fn the_card_check_separates_every_case_when_executed() {
        let mut old = Old::new();
        let view = old.ct.jit_card_view_addr();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let young_holder = old.header(1024, 0);
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let old_value = old.base() + 8192;
        let element = holder + 16 + 600 * 8; // a different card than the header
        assert_ne!(
            (element - old.base()) / CARD_SIZE,
            (holder - old.base()) / CARD_SIZE
        );

        // Header mark.
        assert_eq!(
            run_check(view, false, young_holder, y, young_holder),
            0,
            "young receiver"
        );
        assert_eq!(run_check(view, false, holder, 0, holder), 0, "null value");
        assert_eq!(
            run_check(view, false, holder, old_value, holder),
            0,
            "old -> old"
        );
        assert_eq!(
            run_check(view, false, holder, y, holder),
            1,
            "clean card: call"
        );
        // A value ABOVE the old generation is young too.
        let above = old.end() + 64;
        assert_eq!(
            run_check(view, false, holder, above, holder),
            1,
            "young above old"
        );
        assert!(
            old.ct.dirty_card_indices().is_empty(),
            "the check stores nothing"
        );
        old.ct.mark_dirty_lockfree(holder);
        assert_eq!(
            run_check(view, false, holder, y, holder),
            0,
            "dirty card: no call"
        );

        // Element mark: its own card decides, not the header's.
        assert_eq!(
            run_check(view, true, holder, y, element),
            1,
            "clean element card"
        );
        old.ct.mark_dirty_lockfree(element);
        assert_eq!(
            run_check(view, true, holder, y, element),
            0,
            "dirty element card"
        );
        old.ct.clear_all();
        old.ct.mark_dirty_lockfree(holder);
        assert_eq!(
            run_check(view, true, holder, y, element),
            1,
            "a dirty HEADER card does not answer for an element mark"
        );
        // An element address at or past the view's end is never indexed.
        assert_eq!(
            run_check(view, true, holder, y, old.end()),
            1,
            "mark past the end: call"
        );

        // Flagged old but outside the view: the collector decides.
        let mut stray = vec![0u64; 4];
        // SAFETY: the flags byte of the 32-byte buffer's header.
        unsafe {
            *((stray.as_mut_ptr() as usize + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *mut u8) =
                cratonvm_types::GC_FLAG_OLD_GEN;
        }
        let s = stray.as_ptr() as usize;
        assert_eq!(
            run_check(view, false, s, y, s),
            1,
            "outside the view: call, never skip"
        );

        assert!(!old.ct.raw_card_address_escaped());
        drop((young, stray));
    }

    /// What the stub saw, reached through the `vm_ptr` argument (the harness
    /// stores this struct's address as the frame's VM slot), so the test needs
    /// no process `static` (`jit/tests/process_global_statics_ratchet.rs`).
    struct Recorder {
        ct: *const CardTable,
        calls: usize,
        last: (i64, i64),
        /// The holder's field word at the moment of the first call.
        field_at_first_call: u64,
        field: usize,
    }

    // SAFETY: an `extern "C"` test stub; `vm` is the address of a live
    // `Recorder` the test owns, `ct` a live `CardTable`, and `field` a live,
    // 8-aligned test slot. It dereferences nothing else.
    unsafe extern "C" fn recording_barrier(vm: i64, obj: i64, val: i64) {
        let r = &mut *(vm as *mut Recorder);
        if r.calls == 0 && r.field != 0 {
            r.field_at_first_call = std::ptr::read_volatile(r.field as *const u64);
        }
        r.calls += 1;
        r.last = (obj, val);
        (*r.ct).mark_dirty_lockfree(obj as usize);
    }

    /// The full barrier, executed in a frame the way a store site uses it:
    /// `PRE; store; POST`, with the store in between. Returns RAX at the end,
    /// which must be the holder again on every path (the slow arm reloads it).
    fn run_store(old: &Old, rec: &mut Recorder, holder: usize, val: usize, mark_off: usize) -> i64 {
        let view = old.ct.jit_card_view_addr();
        let body = || {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            c.helpers.jit_card_table_addr = view;
            c.helpers.jit_card_old_base = old.base();
            c.helpers.jit_card_old_end = old.end();
            c.helpers.write_barrier = recording_barrier as *const () as usize; // Cast: fn → slot
            assert!(
                c.inline_card_mark_available(),
                "the flag is on for this compile"
            );
            c.heap_local_offset = 8;
            let entry_off = c.buf.pos();
            c.buf.emit(&[0x55]); // push rbp
            c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
            c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40: aligned, shadow space below the locals
            c.emit_store_local(8, ARG_REGS[0]); // vm (the Recorder)
            c.emit_store_local(16, ARG_REGS[1]); // holder
            c.emit_store_local(24, ARG_REGS[2]); // value
            let (obj_slot, val_slot) = (StackSlot::Frame(16), StackSlot::Frame(24));
            c.load_slot_to_reg(RAX, obj_slot);
            c.load_slot_to_reg(RDX, val_slot);
            // An element mark is formed as the aastore arm forms it: an index
            // in RCX, then `emit_ref_element_address` over it. `mark_off` is
            // `HEADER_SIZE + i * 8`; with 4-byte elements the same `i` lands
            // at `HEADER_SIZE + i * 4`, which is what the assertions compute.
            let index = mark_off.saturating_sub(HEADER_SIZE) / 8;
            let emit_mark = |c: &mut Compiler| -> u8 {
                if mark_off == 0 {
                    return RAX;
                }
                c.emit_mov_imm64(RCX, index as i64); // Cast: a small test index
                c.emit_ref_element_address(RCX, RAX, RCX);
                RCX
            };
            let mark = emit_mark(&mut c);
            c.emit_gen_card_barrier(RAX, RDX, mark, &[(RAX, obj_slot), (RDX, val_slot)]);
            // The store itself: the holder's first body word.
            // (Not the header constant cast inline: the x64 header-offset
            // tripwire counts that spelling; this test store is not a site.)
            let header_disp = i32::try_from(HEADER_SIZE).expect("a small header");
            c.emit_mov_mem_disp32_r64(RAX, RDX, header_disp);
            let mark = emit_mark(&mut c);
            c.emit_gen_card_barrier(RAX, RDX, mark, &[(RAX, obj_slot), (RDX, val_slot)]);
            c.buf.emit(&[0xC9, 0xC3]); // leave; ret
            assert!(!c.buf.overflowed());
            assert!(c.buf.codegen_failure_reason().is_none());
            crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
                .expect("the test buffer must be flippable to RX");
            // SAFETY: `entry_off` is a byte offset inside the same allocation.
            let entry = unsafe { c.buf.as_ptr().add(entry_off) };
            // SAFETY: the snippet builds and tears down its own aligned frame,
            // takes three i64s, stores only into the holder's first body word
            // (a live test buffer), calls only `recording_barrier` with the
            // live `Recorder`, and returns an i64. `c` outlives the call.
            let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
            // Casts: test addresses as the i64 arguments of the C ABI.
            let r = f(rec as *mut Recorder as i64, holder as i64, val as i64);
            drop(c);
            r
        };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_INLINE_CARD_MARK", Some("1"))],
            body,
        )
    }

    /// PRE before the store, POST spares the call once the card is dirty, the
    /// slow arm reloads the holder, and an element mark lands on the element's
    /// card.
    #[test]
    fn the_full_barrier_calls_the_collector_only_for_a_clean_card() {
        let mut old = Old::new();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let field = holder + HEADER_SIZE;
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let mut rec = Recorder {
            ct: &old.ct,
            calls: 0,
            last: (0, 0),
            field_at_first_call: u64::MAX,
            field,
        };

        // Clean card: PRE calls (the field still holds its old value, 0), POST
        // finds the card dirty. The holder comes back in RAX.
        let r = run_store(&old, &mut rec, holder, y, 0);
        assert_eq!(r as usize, holder, "the slow arm reloaded the holder");
        assert_eq!(rec.calls, 1, "one call: PRE; POST found the card dirty");
        assert_eq!(rec.field_at_first_call, 0, "PRE ran BEFORE the store");
        assert_eq!(rec.last, (holder as i64, y as i64));
        assert!(old.dirty(holder));
        // SAFETY: the holder's first body word, a live 8-aligned slot.
        assert_eq!(
            unsafe { std::ptr::read_volatile(field as *const u64) },
            y as u64
        );

        // Dirty card: no call at all.
        let r = run_store(&old, &mut rec, holder, y, 0);
        assert_eq!(r as usize, holder);
        assert_eq!(rec.calls, 1);

        // Element mark, element 600: its own card, handed over 8-aligned.
        old.ct.clear_all();
        rec.calls = 0;
        rec.field = 0;
        let element = holder + HEADER_SIZE + 600 * cratonvm_types::narrow_oop::ref_element_size();
        run_store(&old, &mut rec, holder, y, HEADER_SIZE + 600 * 8);
        assert_eq!(rec.calls, 1);
        assert_eq!(rec.last.0 as usize, element & !7);
        assert!(old.dirty(element));
        assert!(
            !old.dirty(holder),
            "an element mark leaves the header card clean"
        );
        assert!(!old.ct.raw_card_address_escaped());
        drop(young);
    }
}
