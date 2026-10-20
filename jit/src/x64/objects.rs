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

/// zr z3/ja: compiled ZGC `anewarray` sites, BOTH tiers, as
/// `[inline bump, declined]` -- kept apart from the `newarray` pair above so
/// that line's numbers mean what they meant before. A diagnostic census (no
/// per-VM or compatibility state), read by `vm-cli`'s
/// `CRATONVM_INTRINSIC_STATS=1` block.
static INLINE_REF_ARRAY_SITES: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `(inline bump, declined)` counts of compiled ZGC `anewarray` sites.
pub fn inline_ref_array_site_counts() -> (u64, u64) {
    (
        INLINE_REF_ARRAY_SITES[0].load(std::sync::atomic::Ordering::Relaxed),
        INLINE_REF_ARRAY_SITES[1].load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Count one compiled ZGC `anewarray` site (`inline`: it took the bump).
pub(crate) fn note_inline_ref_array_site(inline: bool) {
    let i = usize::from(!inline);
    INLINE_REF_ARRAY_SITES[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `CRATONVM_JIT_INLINE_TLAB_ANEWARRAY` (zr z3/ja; default ON, `=0` restores
/// the helper): the inline TLAB bump for a compiled REFERENCE array
/// (`anewarray`, `new Object[n]`), in BOTH tiers, **on ZGC only**
/// (`jit_tlab_registration_required`).
///
/// Before it every compiled `new Object[4]` called `jit_anewarray_object`
/// (panic guard, JIT-boundary note, SATB drain, three concurrent-start probes,
/// the thread fetch, the size arithmetic and only then the Rust bump) where a
/// `new long[4]` and a `new Node()` bump inline
/// (`zgc-zr2a2-compiled-reference-arrays-never-bump-inline-FIXED-20261012.md`).
///
/// The sequence is the primitive arm's, with two differences and nothing
/// else: the class word is the site's component class id (what
/// `jit_anewarray_object` passes `tlab_alloc_array_guarded_refill`, which
/// stamps `ObjectHeader::new(component, Array, Reference, len, len)`), and the
/// element size is `narrow_oop::ref_element_size()`, a process constant fixed
/// at VM init before the first compile (4 under compressed oops, else 8). A
/// null reference is the all-zero word in both encodings, so the refill's
/// zeroed chunk is a body of nulls -- exactly what the Rust TLAB path relies
/// on (it does not write the body either). The announce is the primitive
/// arm's (`CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE`). Read per compiled
/// site, no static.
pub(crate) fn inline_tlab_anewarray_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_TLAB_ANEWARRAY")
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
pub(super) const ZGC_TLAB_CURSOR_OFFSET: i32 = 0;
/// `Tlab::start` (`Tlab::START_OFFSET`).
pub(super) const ZGC_TLAB_START_OFFSET: i32 = 16;
/// `Tlab::zgc_owned_epoch` (`Tlab::ZGC_OWNED_EPOCH_OFFSET`).
pub(super) const ZGC_TLAB_OWNED_EPOCH_OFFSET: i32 = 24;
/// `Tlab::zgc_announce`, the table address or 0 (`Tlab::ZGC_ANNOUNCE_OFFSET`).
pub(super) const ZGC_TLAB_ANNOUNCE_OFFSET: i32 = 32;
/// `Tlab::zgc_owned_words` (`Tlab::ZGC_OWNED_WORDS_OFFSET`, zr z11/ia): the
/// buffer's owned start-bitmap word window, two dwords -- the first owned word
/// index at `+0`, one past the last at `+4` -- counted from the table's `base`.
/// Read by the LONG inline start-bit store under
/// [`zgc_announce_owned_window_enabled`].
pub(super) const ZGC_TLAB_OWNED_WORDS_OFFSET: i32 = 40;
/// `JitZgcAnnounceTable::words`: the start-bitmap word array, 0 = unpublished.
pub(super) const ZGC_ANNOUNCE_WORDS: i32 = 0;
/// `JitZgcAnnounceTable::base`: the address bit 0 denotes.
pub(super) const ZGC_ANNOUNCE_BASE: i32 = 8;
/// `JitZgcAnnounceTable::span`: bytes covered from `base`.
pub(super) const ZGC_ANNOUNCE_SPAN: i32 = 16;
/// `JitZgcAnnounceTable::blocked`: non-zero while marking or generational.
pub(super) const ZGC_ANNOUNCE_BLOCKED: i32 = 24;
/// `JitZgcAnnounceTable::epoch`: the live VM-TLAB ownership epoch.
pub(super) const ZGC_ANNOUNCE_EPOCH: i32 = 32;
/// `JitZgcAnnounceTable::short_blocked`: zero exactly when the SHORT store
/// may set a bit (published, not blocked, 512-aligned base). zr z9/bo.
pub(super) const ZGC_ANNOUNCE_SHORT_BLOCKED: i32 = 48;
/// `JitZgcAnnounceTable::granule_bias`: `words - (base >> 9) * 8`. zr z9/bo.
pub(super) const ZGC_ANNOUNCE_GRANULE_BIAS: i32 = 56;

/// `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE` (zr z9/bo; default OFF, opt-in with
/// `1`): every inline ZGC start-bit store -- both tiers' inline `new` /
/// `newarray` / `anewarray`, the box prefixes, and the call-free pair of the
/// inline `StringBuilder.toString()` -- emits the SHORT form
/// ([`emit_zgc_inline_announce_short_raw`]): 25 instructions (counting the
/// closing `mov rax, r11; jmp`, not the commit), 5 branches and 7 loads plus
/// the `or`'s own, against the long form's 38 / 9 / 10.
///
/// It sets the same bit of the same bitmap word under the same ownership
/// conditions, so every reader of the start registry answers exactly as
/// before; what it drops are tests that an ARMED buffer already passed when
/// it was armed, and arithmetic the collector now does once per heap:
///
/// - `words != 0`, `base <= obj < base + span` and the 8-byte grid: a buffer
///   is armed (`Tlab::zgc_announce != 0` under the live epoch) only for a
///   chunk inside `[base, base + span)` of a published table
///   (`cratonvm_gc::tlab`'s `zgc_inline_announce_for_chunk`), and the object
///   is an 8-aligned bump of that buffer;
/// - the withdrawal and the geometry: the table's `short_blocked` word is
///   zero only while `words != 0`, `blocked == 0` and `base` is 512-aligned
///   (`JitZgcAnnounceTable::short_geometry`), so the short form tests ONE
///   word where the long form tests `blocked` and `words`;
/// - the word address: `granule_bias + (obj >> 9) * 8`, the bias published
///   once per heap, instead of `words + ((obj - base) >> 9) * 8` with `base`
///   loaded twice; the granule is `obj & !511` (a 512-aligned base).
///
/// The collector maintains both new table words whatever this switch says;
/// the switch only picks the emitted form. Read per compiled site.
pub(crate) fn zgc_jit_short_announce_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_JIT_SHORT_ANNOUNCE")
}

/// `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` (default ON since the round 9 wave 8
/// integration; `=0` opts out): emit the inline start-bit
/// store in front of the `ZgcAnnounce` helper call. The collector reads the
/// same variable before it publishes the table, and an unpublished table
/// leaves every buffer's `zgc_announce` at 0, which the emitted code sends to
/// the helper -- so either half alone is inert. Read per compiled `new` site.
fn zgc_jit_inline_announce_requested() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE")
}

/// `CRATONVM_JIT_IR_ZGC_INLINE_ANNOUNCE` (round 16 lane alloc; default ON
/// since round 16, `=0`/`false`/`off`/`no` turns it off): the
/// OPTIMIZING tier's twin of the single-pass inline start-bit store. Under
/// ZGC -- the default collector since 2026-08-10 -- every IR-tier inline
/// `new` of a post-init no-op class called `zgc_note_tlab_object` after its
/// bump (`runtime_lowering::emit_inline_tlab_new_ir_impl`, `plan.announce`),
/// while the single-pass tier has set the bit inline since round 9 wave 8.
/// That Rust call is per allocation and is the largest JIT-side mutator term
/// of `R16AllocPremium` on the default collector. Default ON since round 16
/// (measured: alloc-ref-dead 20.5→10.5 ns, alloc-store 20.6→11.6 ns,
/// R16Bench bintrees 124→87 ms, alloc 13.9→10.4 s); the
/// collector half is `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` (default ON), and
/// with it off the table stays unpublished and this sequence always takes the
/// helper. Read per compiled site.
pub(crate) fn ir_zgc_inline_announce_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_ZGC_INLINE_ANNOUNCE")
        && zgc_jit_inline_announce_requested()
}

/// `CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE` (round 16 wave 7, lane strings;
/// default ON, `=0` restores the helper): the ARRAY twin of the ZGC announce,
/// in BOTH tiers.
///
/// Under ZGC (`jit_tlab_registration_required`) the single-pass inline
/// primitive `newarray` declined outright (`"tlab registration required"`)
/// and the optimizing tier had no inline array bump at all, so every compiled
/// `new byte[n]` -- the `byte[16]` of every `new StringBuilder()` -- paid the
/// whole `jit_newarray` helper (58% of `R16StringsSplit sbNew` in the
/// sampler). With this switch both tiers bump the TLAB inline, stamp the same
/// header as the non-ZGC arm, commit, and announce the array exactly as the
/// object arm announces a `new`: the inline start-bit store
/// (`emit_zgc_inline_announce`, armed only when the collector published its
/// table under `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`) in front of a
/// `zgc_note_tlab_object(vm, array, footprint)` call. No collector change:
/// `note_tlab_object` registers any TLAB object's base (plus allocate-black
/// and the young grain) and reads `footprint` only for forensics; it is the
/// cursor advance, as for `new`. Read per compiled site, no static.
pub(crate) fn zgc_inline_array_announce_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE")
}

/// The collector half of the inline start-bit store
/// (`CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`, default ON), for a caller outside this
/// module that uses the CALL-FREE pair
/// [`Compiler::emit_zgc_start_bit_precheck`] /
/// [`Compiler::emit_zgc_start_bit_store_prechecked`] (JIT round 17 wave 2,
/// lane sbzgc: the inline `StringBuilder.toString()`). With it off the
/// collector never publishes its table, so the pre-check could only ever
/// decline; the caller declines at compile time instead.
pub(super) fn zgc_inline_start_bit_requested() -> bool {
    zgc_jit_inline_announce_requested()
}

/// `Compiler::emit_zgc_inline_announce` written straight into a buffer, for
/// the optimizing tier's inline `new` (`runtime_lowering`), byte for byte the
/// same sequence. Same contract: entered after the bump committed a fully
/// headed object with R10 = the `JvmThread*` and R11 = the object; clobbers
/// RAX, RCX, RDX, R8, R9 and the flags; preserves R10 / R11 for the helper
/// arm. Returns `(done, to_helper)`: `done` is a `JMP rel32` with RAX = the
/// object, to be patched past the helper call; every `to_helper` patch goes
/// to the helper call's argument set-up.
pub(crate) fn emit_zgc_inline_announce_raw(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_raw_committing(
        buf,
        cursor_off,
        end_off,
        ZgcAnnounceCommit::AlreadyCommitted,
    )
}

/// `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT` (nio21 wave 10 L1; default ON,
/// `=0` restores the commit-then-announce order): the inline ZGC allocation,
/// in BOTH tiers, sets the object's start bit BEFORE it commits the TLAB
/// cursor rather than after it
/// (`docs/internal/fixed-bugs/r21-zgc-frozen-peer-in-flight-object-is-swept-CLOSED-20261012.md`).
///
/// The order is header, start bit, cursor. A peer the take-over freezes
/// anywhere in it owns an object that is EITHER inside the reserved tail it
/// publishes (`[cursor, end)`, withheld from the sweep and the slide) OR
/// registered -- never neither, which is the state the old order had between
/// its commit and its `or`. A registered in-flight object is fully headed (the
/// header stores precede the `or`; x86-64 TSO keeps them in order, so no
/// reader of the registry sees a base whose header is still zero -- the role
/// HotSpot gives the klass word's release store), and the frozen peer's R11
/// holds it, which `is_object_address` now resolves to a root.
///
/// This switch reorders the INLINE arm; the helper arm (every refusal of the
/// start-bit store) is reordered by [`zgc_helper_before_commit_enabled`]
/// (nio21 wave 12), and with that off it commits first and then calls
/// `zgc_note_tlab_object`, leaving a few-instruction window that the GC side
/// (`CRATONVM_ZGC_SKIP_COVERS_INFLIGHT`) covers. Read per compiled site.
pub(crate) fn zgc_start_bit_before_commit_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT")
}

/// How the inline ZGC start-bit store relates to the TLAB cursor commit
/// (nio21 wave 10 L1, [`zgc_start_bit_before_commit_enabled`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZgcAnnounceCommit {
    /// The caller committed the cursor before the store (the order before
    /// nio21 wave 10). Emits nothing.
    AlreadyCommitted,
    /// Commit `R11 + size`: a `new`, whose footprint is a compile-time
    /// constant (the bump's own LEA displacement).
    Size(i32),
    /// Commit the end of the PRIMITIVE array at R11, re-derived from the
    /// length its header already holds:
    /// `R11 + ARRAY_DATA_OFFSET + round_up_8(length << log2_elem)` -- the
    /// bump's own arithmetic, which the store sequence's clobbers (RAX) do not
    /// let the caller keep.
    ArrayFromHeader { log2_elem: u8 },
}

/// Commit the TLAB cursor for the object at R11 (R10 = the `JvmThread*`) as
/// `commit` says. Clobbers RCX only (and the flags for the array form);
/// `AlreadyCommitted` emits nothing. Used on the inline arm of the start-bit
/// store (after its `or`) and on its helper arm (before the call).
pub(crate) fn emit_zgc_announce_commit(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    commit: ZgcAnnounceCommit,
) {
    if !emit_zgc_object_end_into_rcx(buf, commit) {
        return;
    }
    // MOV [R10 + cursor], RCX: REX.W+B 89 /r, modrm 10 001 010.
    buf.emit(&[0x49, 0x89, 0x8A]);
    buf.emit(&cursor_off.to_le_bytes());
}

/// `CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT` (nio21 wave 12 L5; default ON,
/// `=0` keeps the wave-10 helper arm, which commits and then calls; also off
/// whenever [`zgc_start_bit_before_commit_enabled`] is): the HELPER arm of a
/// ZGC inline allocation calls `zgc_note_tlab_object` BEFORE it commits the
/// cursor, closing the last window of
/// `r21-zgc-frozen-peer-in-flight-object-is-swept-CLOSED-20261012.md` (window 1).
///
/// The sequence is: header written, cursor NOT committed, the call (the
/// object is inside the frozen-peer tail `[cursor, end)` until the helper's
/// registration, and registered after it), then [`emit_zgc_commit_after_helper`].
/// After the call R10 / R11 are gone, and the commit re-derives both from
/// state the call cannot move: the thread from its frame slot, and the
/// object from the STILL-UNCOMMITTED cursor (`align8(cursor)` is exactly
/// where the bump put it; the helper only registers, never allocates from
/// this thread's TLAB, and reaches no safepoint -- a take-over finding the
/// peer inside it resumes it, as for any PC outside JIT code). So the
/// commit is independent of the helper's answer.
///
/// The `0` return (a pending exception or a contained panic) commits too, as
/// the old order did: the object becomes a dead, possibly registered object
/// BELOW the cursor, never a registered base above it that a later bump or a
/// tail filler would overlap. A peer frozen after the call and before the
/// commit owns a registered object (R11 / RAX hold it, or the helper's 0
/// left it unregistered inside the tail). Read per compiled site.
pub(crate) fn zgc_helper_before_commit_enabled() -> bool {
    zgc_start_bit_before_commit_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT")
}

/// `CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH` (zr z7/ca; default ON, `=0`
/// restores the old join): in the OPTIMIZING tier's inline ZGC allocation
/// (`runtime_lowering::emit_inline_tlab_newarray_ir_impl` and
/// `emit_inline_tlab_new_ir_impl`), the inline start-bit arm -- the one that
/// made no call -- jumps straight to the site's join instead of running the
/// post-call frame republish that follows the `zgc_note_tlab_object` CALL.
///
/// The republish (`emit_post_call_frame_republish`) restores the innermost-RBP
/// mirror and zeroes the frame-identity mirror after a CALL, because the
/// callee may have published its own; the site's caller then writes this
/// method's identity back (`emit_identity_after_shared_stub`). On the inline
/// arm nothing was called, so both mirrors still hold this frame's values and
/// the republish rewrote them to themselves (the identity via a transient 0).
/// Skipping it is value-identical at the join and removes two TLS stores and
/// a jump per compiled allocation (`Zr3JaRefArrayAlloc`'s `burstPrim [c2]`);
/// it also closes the few-instruction window in which a peer frozen there
/// showed identity 0. The helper arm is unchanged. ZGC only: off ZGC the
/// sequences carry no announce and emit no republish of their own. Read per
/// compiled site.
pub(crate) fn zgc_inline_arm_skips_republish_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH")
}

/// `CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW` (zr z11/ia; default ON, `=0`
/// restores the round-9 granule test): the LONG inline ZGC start-bit store
/// ([`emit_zgc_inline_announce_long_raw`], both tiers) proves that the bitmap
/// word it ORs into is owned by comparing the word's index, which it computes
/// for the `or` anyway, against the buffer's precomputed owned-word window
/// (`Tlab::zgc_owned_words`, written by `Tlab::new` beside the ownership
/// epoch). The round-9 form re-derived the word's 512-byte granule from the
/// table base (a second load of it) and compared it against `start` and `end`.
///
/// Word `k` lies wholly inside `[start, end)` exactly when
/// `ceil((start - base) / 512) <= k < floor((end - base) / 512)`, the window
/// `Tlab::new` stores; the old sequence asked `base + 512k >= start` and
/// `end - (base + 512k) >= 512`, the same two inequalities. `start`, `end` and
/// `base` are fixed for an armed buffer's life (`end` is written only by
/// `Tlab::new` and `Tlab::retire`, which disarms; the table's geometry is
/// published once per heap), so the answer is the same on every object. The
/// window is empty (helper) when the span has more than `u32::MAX` words,
/// which is what lets the comparison read the index's low dword. The bit
/// shift's `AND RCX, 63` goes too: `SHL r64, CL` masks its count to six bits.
/// Eleven instructions (one of them a load of `base`, two the `start` and
/// `end` reads) become four (two compares against the window, two
/// branches): seven fewer per compiled allocation, one load fewer. The SHORT
/// form (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`) keeps its own granule test. Read
/// per compiled site.
pub(crate) fn zgc_announce_owned_window_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW")
}

/// `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_GRID_TEST` (zr z11/ia; default ON, `=0`
/// restores the test): the LONG inline ZGC start-bit store drops its
/// `TEST RCX, 7; JNZ helper` (is `obj - base` on the 8-byte grid?). Every
/// caller enters it with R11 = an 8-aligned bump result -- aligned by its own
/// `ADD R11, 7; AND R11, -8`, or, in the optimizing tier on ZGC since zs
/// s6/ca3, proven aligned by `TEST R11B, 7; JNE slow`
/// (`runtime_lowering::zgc_tlab_align_test_enabled`) -- and the collector
/// publishes the table only for an 8-aligned
/// `base` (`publish_jit_inline_announce`), so the branch was never taken. The
/// SHORT form never had it. A new caller that enters with an unaligned R11
/// must keep the test (`=0`); none does. Read per compiled site.
pub(crate) fn zgc_announce_skip_grid_test_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_GRID_TEST")
}

/// `CRATONVM_JIT_ZGC_ANNOUNCE_COMMIT_FROM_RAX` (zr z11/ia; default ON, `=0`
/// restores the re-derived commit): in BOTH tiers' inline ZGC allocation
/// (`new`, `newarray` / `anewarray`, box prefixes), which enters the start-bit
/// store before the commit with RAX = the bump's new cursor
/// ([`emit_zgc_inline_announce_raw_committing_end_in_rax`]),
/// the store keeps the announce table in R9 instead of RAX, so RAX still holds
/// the object's end at the `or`, and the inline arm commits with ONE store,
/// `MOV [R10 + cursor], RAX`. The old arm re-derived the end: for an array
/// from the header's length (`MOV ECX; SHL; ADD; AND; LEA`, then the store),
/// for a `new` with `LEA RCX`. It is the bump's own `LEA RAX` result in every
/// case. Both forms: the LONG one only with
/// [`zgc_announce_owned_window_enabled`] (the window frees R9, which the
/// granule arithmetic used while the table was live), the SHORT one by moving
/// its granule test to RCX. The helper arm is unchanged: the call clobbers RAX,
/// so its commit still re-derives. Read per compiled site.
pub(crate) fn zgc_announce_commit_from_rax_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_ANNOUNCE_COMMIT_FROM_RAX")
}

/// `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST` (zr s1/ca; default ON, `=0`
/// restores the tests): with the owned-word window
/// ([`zgc_announce_owned_window_enabled`]) the LONG inline start-bit store
/// drops `JB helper` after `SUB RCX, [T + base]` and `CMP RCX, [T + span];
/// JAE helper` (three instructions, one load).
///
/// Both tests are implied by the window. The window is non-empty only for a
/// buffer armed inside `[base, base + span)` with `span >> 9 < 2^32`
/// (`gc::tlab::zgc_owned_word_window`, `zgc_inline_announce_for_chunk`), and
/// every caller enters with R11 = an object of that buffer (`start <= R11 <
/// end`: a bump result whose own `end` test passed), so `R11 - base` is in
/// `[0, span)` and its word index fits the dword the window is compared as.
/// `base` and `span` are those of the arming heap for as long as the buffer's
/// epoch matches: a new heap bumps the ownership epoch before it publishes
/// (`invalidate_owned_vm_tlab_chunks` in `with_capacity`), and the epoch test
/// runs first. The one interleaving the dropped tests still caught is a heap
/// WITHDRAWING its table (`Drop`) between this sequence's `words` load and its
/// `base` load while a thread is still bumping into that heap's chunk, which
/// is a use of freed memory whatever this sequence does. The `words != 0`
/// test stays. The SHORT form never had these tests (zr z9/bo). Read per
/// compiled site.
pub(crate) fn zgc_announce_skip_span_test_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST")
}

/// `CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE` (zs s2/ca2; default OFF, opt-in):
/// the LONG inline start-bit store drops its `blocked` test (`MOV RDX, [T +
/// blocked]; TEST; JNE`) and the zero test of `words` (`TEST RDX, RDX; JE`;
/// the load stays, the `or` needs it): five instructions, two branches and
/// one load per compiled ZGC allocation. The ownership-epoch compare that
/// runs first answers both questions, because the collector keeps one more
/// invariant while the same switch is on (`cratonvm_gc::zgc::vm_tlab`,
/// `zgc_jit_announce_epoch_gate_enabled`, latched per heap into
/// `JitZgcAnnounceTable::epoch_gate`):
///
/// - raising `blocked` (`set_jit_inline_announce_blocked(true)`: a mark start,
///   a generational switch under `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0`) bumps
///   the ownership epoch AFTER the raise, so every buffer armed before it
///   fails the compare;
/// - `Tlab::new` does not arm a buffer while `blocked` is set (its epoch was
///   read at the carve, before this read, so a buffer carved across a raise
///   holds either the old epoch or sees `blocked`);
/// - a withdrawal bumps the epoch BEFORE it clears `words`.
///
/// So a buffer whose recorded epoch equals the live one was armed while
/// `blocked == 0` and `words != 0`, and neither has changed since. The race
/// that remains is today's: a thread past the gate test when the raise lands
/// (the raise is inside the mark-start pause, so only a frozen peer can be
/// there, exactly as a peer past today's `blocked` test). What it costs: after
/// a raise, each thread's current buffer takes the announce helper until its
/// next refill, and a buffer carved while `blocked` is up stays on the helper
/// for its whole life (a concurrent mark only; a stop-the-world mark retires
/// every buffer anyway). Read per compiled site; the collector reads the same
/// key per heap, so the two halves agree within a process.
pub(crate) fn zgc_announce_epoch_gate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE")
}

/// `CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN` (zr s1/ca; default ON, `=0` joins
/// where [`zgc_inline_arm_skips_republish_enabled`] put it): in the OPTIMIZING
/// tier's inline ZGC allocation (`ir_lower`'s `Op::New` and `Op::NewArray`
/// arms), the inline start-bit arm -- the one that made no call -- lands at
/// the site's `allocated` label, past the frame-identity store
/// (`emit_identity_after_shared_stub`) and the `TEST RAX, RAX; JNZ` that only
/// the stub owes. The ZGC twin of round 13 wave 8's
/// `CRATONVM_JIT_IR_ALLOC_FAST_JOIN` (the deferred `new`'s fast path), with
/// the same argument: no stub ran, so no republish zeroed the published
/// identity (which still holds this frame's id, as z7/ca's republish skip
/// already relies on), and RAX is the bumped address (`MOV RAX, R11`), never
/// null. Taken only at a site whose shadow-stack reload emits nothing (a
/// frame-block body, or no live reference to publish), so a push is never
/// left unretracted; the safepoint-id store before the site is unchanged
/// (`zgc-zr11ia-inline-alloc-trims-what-remains-SUPERSEDED-20261019.md`, A1: the frame
/// liveness claims read it). Saves one TLS store and a taken branch per
/// compiled allocation. ZGC only: off ZGC there is no inline start-bit arm.
/// Read per compiled site.
pub(crate) fn zgc_inline_arm_fast_join_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN")
}

/// The commit after a helper-first call ([`zgc_helper_before_commit_enabled`]),
/// entered with R10 = the `JvmThread*` (reloaded by the caller): R11 =
/// `align8([R10 + cursor])` -- the object, the cursor being uncommitted --
/// then [`emit_zgc_announce_commit`]. Clobbers R11, RCX and the flags; RAX
/// (the helper's answer) survives.
pub(crate) fn emit_zgc_commit_after_helper(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    commit: ZgcAnnounceCommit,
) {
    // MOV R11, [R10 + disp32]: REX.W+R+B 8B /r, modrm 10 011 010.
    buf.emit(&[0x4D, 0x8B, 0x9A]);
    buf.emit(&cursor_off.to_le_bytes());
    buf.emit(&[0x49, 0x83, 0xC3, 0x07]); // ADD R11, 7
    buf.emit(&[0x49, 0x83, 0xE3, 0xF8]); // AND R11, -8
    emit_zgc_announce_commit(buf, cursor_off, commit);
}

/// RCX = the end of the object at R11 as `commit` describes it; `false` (and
/// nothing emitted) for `AlreadyCommitted`, or when the header offsets do not
/// fit a disp8 (the compile is then marked unencodable). Clobbers RCX and
/// the flags only.
pub(crate) fn emit_zgc_object_end_into_rcx(
    buf: &mut crate::ExecutableBuffer,
    commit: ZgcAnnounceCommit,
) -> bool {
    match commit {
        ZgcAnnounceCommit::AlreadyCommitted => return false,
        ZgcAnnounceCommit::Size(size) => {
            // LEA RCX, [R11 + disp32]: REX.W+B 8D /r, modrm 10 001 011.
            buf.emit(&[0x49, 0x8D, 0x8B]);
            buf.emit(&size.to_le_bytes());
        }
        ZgcAnnounceCommit::ArrayFromHeader { log2_elem } => {
            // Both header offsets as disp8, through `try_from` rather than a
            // cast (`header_offset_emission_site_inventory_matches_the_doc`
            // counts the cast form). Every caller already screened them to
            // disp8; a header past that refuses the compile here rather than
            // committing a wrong cursor.
            let disp8 = |off: usize| u8::try_from(off).ok().filter(|d| *d <= 127);
            let (Some(len), Some(data)) = (
                disp8(cratonvm_types::ARRAY_LENGTH_OFFSET),
                disp8(cratonvm_types::ARRAY_DATA_OFFSET),
            ) else {
                buf.mark_codegen_unencodable("zgc-announce-commit-array-header-offset");
                return false;
            };
            // MOV ECX, [R11 + len] (zero-extends): REX.B 8B /r, modrm 01 001 011.
            buf.emit(&[0x41, 0x8B, 0x4B, len]);
            if log2_elem != 0 {
                // SHL RCX, imm8: REX.W C1 /4, modrm 11 100 001.
                buf.emit(&[0x48, 0xC1, 0xE1, log2_elem]);
            }
            // ADD RCX, 7 ; AND RCX, -8.
            buf.emit(&[0x48, 0x83, 0xC1, 0x07]);
            buf.emit(&[0x48, 0x83, 0xE1, 0xF8]);
            // LEA RCX, [R11 + RCX + data]: REX.W+B 8D /r, modrm 01 001 100,
            // SIB 00 001 011, disp8.
            buf.emit(&[0x49, 0x8D, 0x4C, 0x0B, data]);
        }
    }
    true
}

/// [`emit_zgc_inline_announce_raw`] with the cursor commit placed by
/// `commit`: with anything but `AlreadyCommitted` the sequence is entered
/// BEFORE the commit (header written, cursor not), and its inline arm commits
/// right after the `or`, so the start bit is visible no later than the cursor.
/// Every `to_helper` edge then reaches the caller with the cursor NOT yet
/// committed; the caller either emits [`emit_zgc_announce_commit`] there (the
/// helper arm of a `new`) or declines with nothing to roll back (a box).
pub(crate) fn emit_zgc_inline_announce_raw_committing(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_raw_form(
        buf,
        cursor_off,
        end_off,
        commit,
        zgc_jit_short_announce_enabled(),
    )
}

/// [`emit_zgc_inline_announce_raw_committing`] for a caller that enters it
/// BEFORE the commit with RAX = the object's end (the bump's new cursor, its
/// `LEA RAX` result): both tiers' inline `new`, `newarray` / `anewarray` and
/// box prefixes (zr z11/ia,
/// [`zgc_announce_commit_from_rax_enabled`]). The inline arm then commits RAX
/// itself. At every `to_helper` edge RAX still holds the end (the table lives
/// in R9), which no caller reads. With the switch off, or for
/// `AlreadyCommitted`, the bytes are [`emit_zgc_inline_announce_raw_committing`]'s.
pub(crate) fn emit_zgc_inline_announce_raw_committing_end_in_rax(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_raw_form_end(
        buf,
        cursor_off,
        end_off,
        commit,
        zgc_jit_short_announce_enabled(),
        true,
    )
}

/// [`emit_zgc_inline_announce_raw_committing`] with the form chosen by the
/// caller (`short`: [`zgc_jit_short_announce_enabled`]). The seam the
/// executed-code tests drive, so they do not depend on the environment.
pub(crate) fn emit_zgc_inline_announce_raw_form(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    short: bool,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_raw_form_end(buf, cursor_off, end_off, commit, short, false)
}

/// [`emit_zgc_inline_announce_raw_form`] with the caller's `end_in_rax`
/// promise (zr z11/ia, [`emit_zgc_inline_announce_raw_committing_end_in_rax`]).
/// The seam the executed-code tests drive.
pub(crate) fn emit_zgc_inline_announce_raw_form_end(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    short: bool,
    end_in_rax: bool,
) -> (usize, Vec<usize>) {
    let (done, to_helper) = emit_zgc_inline_announce_raw_form_end_ft(
        buf, cursor_off, end_off, commit, short, end_in_rax, false,
    );
    (done.unwrap_or_default(), to_helper)
}

/// zs s9/ca4 (`CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH`): the inline ZGC
/// start-bit store of [`emit_zgc_inline_announce_raw_committing_end_in_rax`]
/// whose inline arm FALLS THROUGH -- it ends at `MOV RAX, R11` (the object in
/// RAX, the cursor committed) with no `JMP done`, so the caller puts the
/// site's continuation right after it and the helper arm out of line. The
/// bytes are the jumping form's minus its final `JMP rel32`. Returns the
/// `to_helper` patch sites.
pub(crate) fn emit_zgc_inline_announce_raw_committing_end_in_rax_fall_through(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
) -> Vec<usize> {
    emit_zgc_inline_announce_raw_form_end_ft(
        buf,
        cursor_off,
        end_off,
        commit,
        zgc_jit_short_announce_enabled(),
        true,
        true,
    )
    .1
}

/// `MOV RAX, R11`, then -- unless `fall_through` -- `JMP rel32 done` (its
/// operand returned for the caller to patch): the inline arm's exit, shared
/// by every form (zs s9/ca4).
fn emit_zgc_announce_exit(
    buf: &mut crate::ExecutableBuffer,
    fall_through: bool,
    to_helper: Vec<usize>,
) -> (Option<usize>, Vec<usize>) {
    buf.emit(&[0x4C, 0x89, 0xD8]);
    if fall_through {
        return (None, to_helper);
    }
    buf.emit_byte(0xE9);
    let done = buf.pos();
    buf.emit(&[0; 4]);
    (Some(done), to_helper)
}

fn emit_zgc_inline_announce_raw_form_end_ft(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    short: bool,
    end_in_rax: bool,
    fall_through: bool,
) -> (Option<usize>, Vec<usize>) {
    // zr z11/ia: commit RAX on the inline arm (table in R9) only for a caller
    // that promised RAX = the end, and only for a store that commits.
    let rax_end = end_in_rax
        && commit != ZgcAnnounceCommit::AlreadyCommitted
        && zgc_announce_commit_from_rax_enabled();
    if short {
        emit_zgc_inline_announce_short_raw_trimmed_ft(
            buf,
            cursor_off,
            end_off,
            commit,
            rax_end,
            fall_through,
        )
    } else {
        emit_zgc_inline_announce_long_raw_trimmed_ft(
            buf,
            cursor_off,
            end_off,
            commit,
            LongAnnounceTrims::from_env(rax_end),
            fall_through,
        )
    }
}

/// zr z11/ia: which trims the LONG inline start-bit store applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LongAnnounceTrims {
    /// [`zgc_announce_owned_window_enabled`].
    pub(crate) owned_window: bool,
    /// [`zgc_announce_skip_grid_test_enabled`].
    pub(crate) skip_grid: bool,
    /// [`zgc_announce_commit_from_rax_enabled`] for a caller with RAX = the
    /// end; honoured only with `owned_window` (R9 holds the table).
    pub(crate) rax_end: bool,
    /// zr s1/ca, [`zgc_announce_skip_span_test_enabled`]: drop the `base <=
    /// obj` and `obj - base < span` tests; honoured only with `owned_window`
    /// (the window is what implies them).
    pub(crate) skip_span: bool,
    /// zs s2/ca2, [`zgc_announce_epoch_gate_enabled`]: drop the `blocked`
    /// test and the `words` zero test (the epoch compare implies both).
    pub(crate) epoch_gate: bool,
}

impl LongAnnounceTrims {
    /// None: the round-9 / nio21 sequence byte for byte.
    pub(crate) const NONE: Self = Self {
        owned_window: false,
        skip_grid: false,
        rax_end: false,
        skip_span: false,
        epoch_gate: false,
    };

    fn from_env(rax_end: bool) -> Self {
        Self {
            owned_window: zgc_announce_owned_window_enabled(),
            skip_grid: zgc_announce_skip_grid_test_enabled(),
            rax_end,
            skip_span: zgc_announce_skip_span_test_enabled(),
            epoch_gate: zgc_announce_epoch_gate_enabled(),
        }
    }
}

/// `jcc rel32` with a zero displacement; returns the patch site.
fn zgc_announce_jcc(buf: &mut crate::ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let p = buf.pos();
    buf.emit(&[0; 4]);
    p
}

/// `CMP QWORD [RAX + disp32], 0`: REX.W 83 /7 ib, modrm 10 111 000.
pub(super) fn emit_cmp_qword_rax_disp_zero(buf: &mut crate::ExecutableBuffer, disp: i32) {
    buf.emit(&[0x48, 0x83, 0xB8]);
    buf.emit(&disp.to_le_bytes());
    buf.emit_byte(0);
}

/// zr z9/bo: the SHORT inline start-bit store
/// ([`zgc_jit_short_announce_enabled`]). Same entry state, clobbers, commit
/// placement and `(done, to_helper)` contract as the long form
/// ([`emit_zgc_inline_announce_long_raw`]):
///
/// ```text
///   mov  rax, [r10 + tlab+32] ; test rax, rax ; je helper   ; armed table
///   mov  rdx, [rax + 32] ; cmp rdx, [r10 + tlab+24] ; jne helper  ; epoch
///   cmp  qword [rax + 48], 0 ; jne helper                   ; short_blocked
///   mov  r9, r11 ; and r9, -512                             ; word_lo
///   cmp  r9, [r10 + tlab+16] ; jb helper                    ; word_lo >= start
///   or   r9, 511 ; cmp r9, [r10 + end] ; jae helper         ; word_hi < end
///   mov  r8, r11 ; shr r8, 9                                ; word index
///   mov  rcx, r11 ; shr rcx, 3                              ; bit (CL & 63)
///   mov  r9d, 1 ; shl r9, cl
///   mov  rdx, [rax + 56]                                    ; granule_bias
///   or   [rdx + r8*8], r9                                   ; the start bit
///   <commit> ; mov rax, r11 ; jmp done
/// ```
///
/// The `or` is the long form's own encoding, on the same word: with a
/// 512-aligned `base`, `bias + (obj >> 9) * 8 == words + ((obj - base) >> 9) * 8`
/// and `(obj >> 3) & 63 == ((obj - base) >> 3) & 63`.
pub(crate) fn emit_zgc_inline_announce_short_raw(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_short_raw_trimmed(buf, cursor_off, end_off, commit, false)
}

/// [`emit_zgc_inline_announce_short_raw`], and with `rax_end` (zr z11/ia,
/// [`zgc_announce_commit_from_rax_enabled`]; entered with RAX = the object's
/// end) the same tests on the same words with the table in R9, the granule
/// test in RCX, and the commit `MOV [R10 + cursor], RAX`:
///
/// ```text
///   mov  r9, [r10 + tlab+32] ; test r9, r9 ; je helper
///   mov  rdx, [r9 + 32] ; cmp rdx, [r10 + tlab+24] ; jne helper
///   cmp  qword [r9 + 48], 0 ; jne helper
///   mov  rcx, r11 ; and rcx, -512 ; cmp rcx, [r10 + tlab+16] ; jb helper
///   or   rcx, 511 ; cmp rcx, [r10 + end] ; jae helper
///   mov  r8, r11 ; shr r8, 9
///   mov  rdx, [r9 + 56]                                     ; granule_bias
///   mov  rcx, r11 ; shr rcx, 3 ; mov r9d, 1 ; shl r9, cl
///   or   [rdx + r8*8], r9
///   mov  [r10 + cursor], rax ; mov rax, r11 ; jmp done
/// ```
pub(crate) fn emit_zgc_inline_announce_short_raw_trimmed(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    rax_end: bool,
) -> (usize, Vec<usize>) {
    let (done, to_helper) = emit_zgc_inline_announce_short_raw_trimmed_ft(
        buf, cursor_off, end_off, commit, rax_end, false,
    );
    (done.unwrap_or_default(), to_helper)
}

/// [`emit_zgc_inline_announce_short_raw_trimmed`] whose inline arm, with
/// `fall_through` (zs s9/ca4, [`emit_zgc_announce_exit`]), ends at `MOV RAX,
/// R11` with no `JMP done`: `done` is then `None`.
fn emit_zgc_inline_announce_short_raw_trimmed_ft(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    rax_end: bool,
    fall_through: bool,
) -> (Option<usize>, Vec<usize>) {
    if rax_end && commit != ZgcAnnounceCommit::AlreadyCommitted {
        return emit_zgc_inline_announce_short_raw_rax_end(buf, cursor_off, end_off, fall_through);
    }
    let jcc = zgc_announce_jcc;
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    let mut to_helper: Vec<usize> = Vec::with_capacity(6);
    // MOV RAX, [R10 + tlab + announce]; TEST RAX, RAX; JE helper.
    buf.emit(&[0x49, 0x8B, 0x82]);
    buf.emit(&(tlab + ZGC_TLAB_ANNOUNCE_OFFSET).to_le_bytes());
    buf.emit(&[0x48, 0x85, 0xC0]);
    to_helper.push(jcc(buf, 0x84));
    // MOV RDX, [RAX + epoch]; CMP RDX, [R10 + tlab + owned_epoch]; JNE helper.
    buf.emit(&[0x48, 0x8B, 0x90]);
    buf.emit(&ZGC_ANNOUNCE_EPOCH.to_le_bytes());
    buf.emit(&[0x49, 0x3B, 0x92]);
    buf.emit(&(tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET).to_le_bytes());
    to_helper.push(jcc(buf, 0x85));
    // CMP QWORD [RAX + short_blocked], 0; JNE helper.
    emit_cmp_qword_rax_disp_zero(buf, ZGC_ANNOUNCE_SHORT_BLOCKED);
    to_helper.push(jcc(buf, 0x85));
    // MOV R9, R11; AND R9, -512.
    buf.emit(&[0x4D, 0x89, 0xD9]);
    buf.emit(&[0x49, 0x81, 0xE1]);
    buf.emit(&(-512i32).to_le_bytes());
    // CMP R9, [R10 + tlab + start]; JB helper.
    buf.emit(&[0x4D, 0x3B, 0x8A]);
    buf.emit(&(tlab + ZGC_TLAB_START_OFFSET).to_le_bytes());
    to_helper.push(jcc(buf, 0x82));
    // OR R9, 511; CMP R9, [R10 + end]; JAE helper.
    buf.emit(&[0x49, 0x81, 0xC9]);
    buf.emit(&511i32.to_le_bytes());
    buf.emit(&[0x4D, 0x3B, 0x8A]);
    buf.emit(&end_off.to_le_bytes());
    to_helper.push(jcc(buf, 0x83));
    // MOV R8, R11; SHR R8, 9.
    buf.emit(&[0x4D, 0x89, 0xD8]);
    buf.emit(&[0x49, 0xC1, 0xE8, 0x09]);
    // MOV RCX, R11; SHR RCX, 3.
    buf.emit(&[0x4C, 0x89, 0xD9]);
    buf.emit(&[0x48, 0xC1, 0xE9, 0x03]);
    // MOV R9D, 1; SHL R9, CL.
    buf.emit(&[0x41, 0xB9]);
    buf.emit(&1u32.to_le_bytes());
    buf.emit(&[0x49, 0xD3, 0xE1]);
    // MOV RDX, [RAX + granule_bias].
    buf.emit(&[0x48, 0x8B, 0x90]);
    buf.emit(&ZGC_ANNOUNCE_GRANULE_BIAS.to_le_bytes());
    // OR [RDX + R8*8], R9 -- the start bit (owned word, no lock).
    buf.emit(&[0x4E, 0x09, 0x0C, 0xC2]);
    emit_zgc_announce_commit(buf, cursor_off, commit);
    // MOV RAX, R11; JMP done (zs s9/ca4: no JMP on a fall-through arm).
    emit_zgc_announce_exit(buf, fall_through, to_helper)
}

/// The body of [`emit_zgc_inline_announce_short_raw_trimmed`]'s `rax_end` arm.
fn emit_zgc_inline_announce_short_raw_rax_end(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    fall_through: bool,
) -> (Option<usize>, Vec<usize>) {
    let jcc = zgc_announce_jcc;
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    let mut to_helper: Vec<usize> = Vec::with_capacity(6);
    // MOV R9, [R10 + tlab + announce]; TEST R9, R9; JE helper.
    buf.emit(&[0x4D, 0x8B, 0x8A]);
    buf.emit(&(tlab + ZGC_TLAB_ANNOUNCE_OFFSET).to_le_bytes());
    buf.emit(&[0x4D, 0x85, 0xC9]);
    to_helper.push(jcc(buf, 0x84));
    // MOV RDX, [R9 + epoch]; CMP RDX, [R10 + tlab + owned_epoch]; JNE helper.
    buf.emit(&[0x49, 0x8B, 0x91]);
    buf.emit(&ZGC_ANNOUNCE_EPOCH.to_le_bytes());
    buf.emit(&[0x49, 0x3B, 0x92]);
    buf.emit(&(tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET).to_le_bytes());
    to_helper.push(jcc(buf, 0x85));
    // CMP QWORD [R9 + short_blocked], 0: REX.W+B 83 /7 ib, modrm 10 111 001.
    buf.emit(&[0x49, 0x83, 0xB9]);
    buf.emit(&ZGC_ANNOUNCE_SHORT_BLOCKED.to_le_bytes());
    buf.emit_byte(0);
    to_helper.push(jcc(buf, 0x85));
    // MOV RCX, R11; AND RCX, -512 (REX.W 81 /4, modrm 11 100 001).
    buf.emit(&[0x4C, 0x89, 0xD9]);
    buf.emit(&[0x48, 0x81, 0xE1]);
    buf.emit(&(-512i32).to_le_bytes());
    // CMP RCX, [R10 + tlab + start]; JB helper (REX.W+B 3B /r, modrm 10 001 010).
    buf.emit(&[0x49, 0x3B, 0x8A]);
    buf.emit(&(tlab + ZGC_TLAB_START_OFFSET).to_le_bytes());
    to_helper.push(jcc(buf, 0x82));
    // OR RCX, 511 (REX.W 81 /1, modrm 11 001 001); CMP RCX, [R10 + end]; JAE helper.
    buf.emit(&[0x48, 0x81, 0xC9]);
    buf.emit(&511i32.to_le_bytes());
    buf.emit(&[0x49, 0x3B, 0x8A]);
    buf.emit(&end_off.to_le_bytes());
    to_helper.push(jcc(buf, 0x83));
    // MOV R8, R11; SHR R8, 9.
    buf.emit(&[0x4D, 0x89, 0xD8]);
    buf.emit(&[0x49, 0xC1, 0xE8, 0x09]);
    // MOV RDX, [R9 + granule_bias] (before R9 becomes the mask).
    buf.emit(&[0x49, 0x8B, 0x91]);
    buf.emit(&ZGC_ANNOUNCE_GRANULE_BIAS.to_le_bytes());
    // MOV RCX, R11; SHR RCX, 3.
    buf.emit(&[0x4C, 0x89, 0xD9]);
    buf.emit(&[0x48, 0xC1, 0xE9, 0x03]);
    // MOV R9D, 1; SHL R9, CL.
    buf.emit(&[0x41, 0xB9]);
    buf.emit(&1u32.to_le_bytes());
    buf.emit(&[0x49, 0xD3, 0xE1]);
    // OR [RDX + R8*8], R9 -- the start bit (owned word, no lock).
    buf.emit(&[0x4E, 0x09, 0x0C, 0xC2]);
    // MOV [R10 + cursor], RAX -- the bump's own end, after the bit.
    buf.emit(&[0x49, 0x89, 0x82]);
    buf.emit(&cursor_off.to_le_bytes());
    // MOV RAX, R11; JMP done (zs s9/ca4: no JMP on a fall-through arm).
    emit_zgc_announce_exit(buf, fall_through, to_helper)
}

/// The LONG inline start-bit store (round 9 wave 8; the only form before zr
/// z9/bo, byte for byte), every condition of the table's contract tested
/// per object. Untrimmed ([`LongAnnounceTrims::NONE`]); the emitters go
/// through [`emit_zgc_inline_announce_long_raw_trimmed`].
pub(crate) fn emit_zgc_inline_announce_long_raw(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
) -> (usize, Vec<usize>) {
    emit_zgc_inline_announce_long_raw_trimmed(
        buf,
        cursor_off,
        end_off,
        commit,
        LongAnnounceTrims::NONE,
    )
}

/// The LONG inline start-bit store with the zr z11/ia `trims`
/// ([`LongAnnounceTrims`]). With every trim on and `rax_end` (zr s1/ca's
/// `skip_span` drops the `jb` after the `sub` and the `span` compare that
/// z11/ia kept):
///
/// ```text
///   mov  r9, [r10 + tlab+32] ; test r9, r9 ; jz helper   ; the table, in R9
///   mov  rdx, [r9 + 32] ; cmp rdx, [r10 + tlab+24] ; jne helper   ; epoch
///   mov  rdx, [r9 + 24] ; test ; jnz helper            ; blocked
///   mov  rdx, [r9 + 0]  ; test ; jz helper             ; words
///   mov  rcx, r11 ; sub rcx, [r9 + 8]                  ; off = obj - base
///   shr  rcx, 3 ; mov r8, rcx ; shr r8, 6              ; bit, word index
///   cmp  r8d, [r10 + tlab+40] ; jb helper              ; owned window
///   cmp  r8d, [r10 + tlab+44] ; jae helper
///   mov  r9d, 1 ; shl r9, cl ; or [rdx + r8*8], r9
///   mov  [r10 + cursor], rax ; mov rax, r11 ; jmp done
/// ```
///
/// zs s2/ca2's opt-in `epoch_gate` then drops the `blocked` line and the
/// `test ; jz` of the `words` line (22 instructions with the commit, from 27).
pub(crate) fn emit_zgc_inline_announce_long_raw_trimmed(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    trims: LongAnnounceTrims,
) -> (usize, Vec<usize>) {
    let (done, to_helper) = emit_zgc_inline_announce_long_raw_trimmed_ft(
        buf, cursor_off, end_off, commit, trims, false,
    );
    (done.unwrap_or_default(), to_helper)
}

/// [`emit_zgc_inline_announce_long_raw_trimmed`] with the zs s9/ca4
/// `fall_through` exit ([`emit_zgc_announce_exit`]).
fn emit_zgc_inline_announce_long_raw_trimmed_ft(
    buf: &mut crate::ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    commit: ZgcAnnounceCommit,
    trims: LongAnnounceTrims,
    fall_through: bool,
) -> (Option<usize>, Vec<usize>) {
    fn jcc(buf: &mut crate::ExecutableBuffer, cc: u8) -> usize {
        buf.emit(&[0x0F, cc]);
        let p = buf.pos();
        buf.emit(&[0; 4]);
        p
    }
    let window = trims.owned_window;
    let rax_end = trims.rax_end && window && commit != ZgcAnnounceCommit::AlreadyCommitted;
    // The table register's REX extension bit and ModRM low bits: RAX (0, 000)
    // or, with `rax_end`, R9 (1, 001).
    let (t_ext, t_low): (u8, u8) = if rax_end { (1, 1) } else { (0, 0) };
    // `op reg, [table + disp32]` for `reg` in RCX (1) / RDX (2).
    let table_mem = |buf: &mut crate::ExecutableBuffer, op: u8, reg: u8, disp: i32| {
        buf.emit(&[0x48 | t_ext, op, 0x80 | (reg << 3) | t_low]);
        buf.emit(&disp.to_le_bytes());
    };
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    let mut to_helper: Vec<usize> = Vec::with_capacity(12);
    // MOV T, [R10 + tlab + announce]; TEST T, T; JE helper.
    buf.emit(&[0x49 | (t_ext << 2), 0x8B, 0x82 | (t_low << 3)]);
    buf.emit(&(tlab + ZGC_TLAB_ANNOUNCE_OFFSET).to_le_bytes());
    buf.emit(&[
        0x48 | (t_ext << 2) | t_ext,
        0x85,
        0xC0 | (t_low << 3) | t_low,
    ]);
    to_helper.push(jcc(buf, 0x84));
    // MOV RDX, [T + epoch]; CMP RDX, [R10 + tlab + owned_epoch]; JNE helper.
    table_mem(buf, 0x8B, 2, ZGC_ANNOUNCE_EPOCH);
    buf.emit(&[0x49, 0x3B, 0x92]);
    buf.emit(&(tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET).to_le_bytes());
    to_helper.push(jcc(buf, 0x85));
    // zs s2/ca2 (`zgc_announce_epoch_gate_enabled`): the epoch compare above
    // implies `blocked == 0` and `words != 0`; only the `words` load stays.
    if !trims.epoch_gate {
        // MOV RDX, [T + blocked]; TEST RDX, RDX; JNE helper.
        table_mem(buf, 0x8B, 2, ZGC_ANNOUNCE_BLOCKED);
        buf.emit(&[0x48, 0x85, 0xD2]);
        to_helper.push(jcc(buf, 0x85));
    }
    // MOV RDX, [T + words]; TEST RDX, RDX; JE helper.
    table_mem(buf, 0x8B, 2, ZGC_ANNOUNCE_WORDS);
    if !trims.epoch_gate {
        buf.emit(&[0x48, 0x85, 0xD2]);
        to_helper.push(jcc(buf, 0x84));
    }
    // zr s1/ca: the window implies both bounds tests (see
    // `zgc_announce_skip_span_test_enabled`).
    let skip_span = trims.skip_span && window;
    // MOV RCX, R11; SUB RCX, [T + base]; JB helper.
    buf.emit(&[0x4C, 0x89, 0xD9]);
    table_mem(buf, 0x2B, 1, ZGC_ANNOUNCE_BASE);
    if !skip_span {
        to_helper.push(jcc(buf, 0x82));
        // CMP RCX, [T + span]; JAE helper.
        table_mem(buf, 0x3B, 1, ZGC_ANNOUNCE_SPAN);
        to_helper.push(jcc(buf, 0x83));
    }
    if !trims.skip_grid {
        // TEST RCX, 7 (imm32); JNE helper.
        buf.emit(&[0x48, 0xF7, 0xC1]);
        buf.emit(&7i32.to_le_bytes());
        to_helper.push(jcc(buf, 0x85));
    }
    if window {
        // zr z11/ia: SHR RCX, 3 (the bit index); MOV R8, RCX; SHR R8, 6 (the
        // word index, below 2^32 whenever the window is non-empty).
        buf.emit(&[0x48, 0xC1, 0xE9, 0x03]);
        buf.emit(&[0x49, 0x89, 0xC8]);
        buf.emit(&[0x49, 0xC1, 0xE8, 0x06]);
        // CMP R8D, [R10 + tlab + owned_words]; JB helper: below the window.
        // REX.R+B 3B /r, modrm 10 000 010.
        buf.emit(&[0x45, 0x3B, 0x82]);
        buf.emit(&(tlab + ZGC_TLAB_OWNED_WORDS_OFFSET).to_le_bytes());
        to_helper.push(jcc(buf, 0x82));
        // CMP R8D, [R10 + tlab + owned_words + 4]; JAE helper: past it.
        buf.emit(&[0x45, 0x3B, 0x82]);
        buf.emit(&(tlab + ZGC_TLAB_OWNED_WORDS_OFFSET + 4).to_le_bytes());
        to_helper.push(jcc(buf, 0x83));
    } else {
        // MOV R9, RCX; AND R9, -512; MOV R8, [RAX + base]; ADD R9, R8.
        buf.emit(&[0x49, 0x89, 0xC9]);
        buf.emit(&[0x49, 0x81, 0xE1]);
        buf.emit(&(-512i32).to_le_bytes());
        buf.emit(&[0x4C, 0x8B, 0x80]);
        buf.emit(&ZGC_ANNOUNCE_BASE.to_le_bytes());
        buf.emit(&[0x4D, 0x01, 0xC1]);
        // CMP R9, [R10 + tlab + start]; JB helper.
        buf.emit(&[0x4D, 0x3B, 0x8A]);
        buf.emit(&(tlab + ZGC_TLAB_START_OFFSET).to_le_bytes());
        to_helper.push(jcc(buf, 0x82));
        // MOV R8, [R10 + end]; SUB R8, R9; CMP R8, 512; JB helper.
        buf.emit(&[0x4D, 0x8B, 0x82]);
        buf.emit(&end_off.to_le_bytes());
        buf.emit(&[0x4D, 0x29, 0xC8]);
        buf.emit(&[0x49, 0x81, 0xF8]);
        buf.emit(&512i32.to_le_bytes());
        to_helper.push(jcc(buf, 0x82));
        // SHR RCX, 3; MOV R8, RCX; SHR R8, 6; AND RCX, 63.
        buf.emit(&[0x48, 0xC1, 0xE9, 0x03]);
        buf.emit(&[0x49, 0x89, 0xC8]);
        buf.emit(&[0x49, 0xC1, 0xE8, 0x06]);
        buf.emit(&[0x48, 0x83, 0xE1, 0x3F]);
    }
    // MOV R9D, 1 (zero-extends; `emit_mov_imm32_sx`'s positive form); SHL R9, CL
    // (the count is masked to six bits by the instruction).
    buf.emit(&[0x41, 0xB9]);
    buf.emit(&1u32.to_le_bytes());
    buf.emit(&[0x49, 0xD3, 0xE1]);
    // OR [RDX + R8*8], R9 -- the start bit (owned word, no lock).
    buf.emit(&[0x4E, 0x09, 0x0C, 0xC2]);
    // nio21 wave 10 L1: the commit, AFTER the bit (nothing when the caller
    // committed first).
    if rax_end {
        // zr z11/ia: MOV [R10 + cursor], RAX -- the bump's own end.
        buf.emit(&[0x49, 0x89, 0x82]);
        buf.emit(&cursor_off.to_le_bytes());
    } else {
        emit_zgc_announce_commit(buf, cursor_off, commit);
    }
    // MOV RAX, R11; JMP done (zs s9/ca4: no JMP on a fall-through arm).
    emit_zgc_announce_exit(buf, fall_through, to_helper)
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

/// `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE` (zs s3/bt) -- default ON, `=0`
/// kill: the optimizing tier's gated reference stores (`putfield`, `aastore`)
/// on an age-FLOOR plan open with ONE 16-bit test of the adjacent
/// `pre_active` / `post_active` bytes and store at once when both read zero.
/// Read per compiled site, not cached.
pub(crate) fn zgc_fused_ref_store_gate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE")
}

/// zs s3/bt: the address of the 16-bit word `pre_active | post_active << 8`
/// when one compare of it may stand in front of a published plan's whole gate
/// sequence, `None` otherwise.
///
/// # Why one word answers both gates
///
/// Every gated reference store reaches its barrier-free store exactly when
/// `pre_active == 0` and (the receiver passes the young floor, or
/// `post_active == 0`) -- and for an `aastore`, also when the value is null and
/// `pre_active == 0`. `pre_active == 0 && post_active == 0` implies each of
/// those disjunctions, so a zero WORD is a sufficient condition for the store
/// the full sequence would have reached. On any other answer the full sequence
/// runs, unchanged, behind it. The word is read in one load, which is one
/// snapshot of both bytes: no interleaving of the two separate loads it
/// replaces can say "no work" where this one does not.
///
/// # When it applies
///
/// * a plan is published (`ref_store_gates_of`);
/// * it is the age-FLOOR shape (no skip mask). Only ZGC publishes that shape
///   in production (`ZgcRealHeap::jit_ref_store_plan`); the generational
///   collector's plan is the mask shape and keeps its bytes;
/// * `post == pre + 1`: the two bytes are the adjacent `pre_active` /
///   `post_active` fields of one `JitRefStoreGates` block, so the word at
///   `pre` IS the two gates. Decided from the two addresses alone; nothing
///   is read at compile time.
///
/// ZGC publishes `post_active = 0` until a generational promotion
/// (`CRATONVM_ZGC_JIT_POST_GATE_TRUTH`), so on the default non-generational
/// collector the word reads zero whenever no mark is running.
pub(crate) fn zgc_fused_ref_store_gate_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<usize> {
    let (pre, post, _floor) = ref_store_gates_of(helpers)?;
    let floor_shape = ref_store_post_skip_mask_of(helpers).is_none();
    (floor_shape && post == pre.wrapping_add(1) && zgc_fused_ref_store_gate_enabled())
        .then_some(pre)
}

/// `CRATONVM_JIT_ZGC_FUSED_GATE_ALL_SITES` (zs s5/bt2) -- default ON, `=0`
/// kill: the fused gate of [`zgc_fused_ref_store_gate_of`] also opens the
/// reference stores outside the optimizing tier's `putfield` / `aastore`: the
/// single-pass `putfield` (`emit_gated_compact_ref_putfield`) and `aastore`
/// (`op_array.rs`), and the constant-`VarHandle` / `Unsafe` reference-store
/// gate screen both tiers share (`ir_lower::vh_ref_store_gate_screen`). Needs
/// `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE`. Read per compiled site.
pub(crate) fn zgc_fused_gate_all_sites_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ZGC_FUSED_GATE_ALL_SITES")
}

/// zs s5/bt2: the fused word for a store site outside the optimizing tier's
/// two hot stores -- [`zgc_fused_ref_store_gate_of`] AND
/// [`zgc_fused_gate_all_sites_enabled`]. The soundness argument is that
/// function's: a zero word is sufficient for every arm of the full gate
/// sequence that reaches the barrier-free store, and every other answer runs
/// the full sequence unchanged.
pub(crate) fn zgc_fused_ref_store_gate_sites_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<usize> {
    zgc_fused_ref_store_gate_of(helpers).filter(|_| zgc_fused_gate_all_sites_enabled())
}

/// `CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE` (zs s5/bt2) -- default OFF
/// (opt-in, `=1`): the optimizing tier's gated reference `putfield` whose
/// receiver is an `Op::New` of the same block, with nothing but pure
/// arithmetic and stores into that same object scheduled in between, and
/// whose field no earlier store in that window wrote, stores with no gate at
/// all (`ir_lower::zgc_fresh_receiver_stores`, which carries the soundness
/// argument). Engages only where the fused gate does (ZGC's age-floor plan).
/// Read per compile.
pub(crate) fn zgc_fresh_receiver_store_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE")
}

/// `CRATONVM_JIT_IR_G1_GATED_REF_STORE` (g1put) -- default on, `=0` off: under
/// G1 a compiled reference store whose field holds a non-null value stays inline
/// while the SATB pre-gate byte (`helpers.ref_store_pre_gate`) is clear. Read
/// per compiled site, not cached. The optimizing tier reads the same switch.
pub(super) fn g1_gated_ref_store_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_G1_GATED_REF_STORE")
}

/// `CRATONVM_JIT_FRESH_CTOR_OLD_VALUE_TEST=0` — kill switch of the round-16
/// (lane alloc) old-value test in
/// `Compiler::emit_inline_fresh_ctor_compact_ref_putfield`: a constructor's
/// first syntactic reference store that is not proven to start from null
/// takes the helper when the field's old value is non-null. A correctness fix
/// (the SATB pre-barrier), so default ON; `0` restores the previous bytes.
/// Read per compiled site, not cached.
pub(super) fn fresh_ctor_old_value_test_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_FRESH_CTOR_OLD_VALUE_TEST")
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

/// r21 barriers (BARRIERS21-1 / BARRIERS21-2): what a G1 gated reference
/// `aastore` bakes. `pre` is the SATB pre-gate byte, `table` is
/// `JIT_G1_BARRIER` (arena base, region mask), and `post` is the lean
/// `jit_g1_post_write_barrier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct G1AastorePlan {
    pub(crate) pre: usize,
    pub(crate) table: usize,
    pub(crate) post: usize,
}

/// r21 barriers: the G1 gated-`aastore` plan, or `None`. A pure function of
/// the helper table's words, shared by both tiers.
///
/// The heap is recognised by what THIS VM's helper table publishes, not by
/// the process-wide `JIT_G1_BARRIER` table alone. `build_helpers_opt` gives a
/// G1 heap exactly `(pre gate, 0, 0)` and mask 0 (`ref_store_plan_for_heap`),
/// and no generational card view. A Generational or ZGC heap publishes a post
/// gate. So:
///
/// * `pre` must be wired;
/// * no post gate, floor, mask or reference-store plan is published;
/// * there is no card view;
/// * the G1 table is live (`x64::licm::g1_barrier_table_live`);
/// * the lean G1 post barrier is wired.
///
/// Why each skipped step is a no-op:
///
/// * **SATB.** The pre-gate byte is armed before G1's queue activates and
///   disarmed after it deactivates, so a clear byte means
///   `jit_satb_pre_write_barrier` -> `satb_barrier` returns without logging.
///   A frozen take-over peer is scanned by `scan_frozen_held_objects`.
/// * **Post barrier.** Under G1, `jit_write_barrier` -> `VmHeap::write_barrier`
///   -> `G1Collector::write_barrier` is `post_write_barrier_rset(array,
///   value)` for a non-null value. `jit_g1_post_write_barrier` makes that same
///   call after the same screens. The inline filter dismisses only the null
///   value and the same-region pair, which are that function's own first
///   returns.
pub(crate) fn g1_aastore_plan_from(
    pre: usize,
    plan_published: bool,
    card_view: bool,
    table: usize,
    post: usize,
) -> Option<G1AastorePlan> {
    (pre != 0
        && !plan_published
        && !card_view
        && post != 0
        && super::g1_barrier_table_live(table))
    .then_some(G1AastorePlan { pre, table, post })
}

/// [`g1_aastore_plan_from`] over a helper table (the single-pass tier's
/// spelling).
pub(crate) fn g1_aastore_plan_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<G1AastorePlan> {
    let plan_published = ref_store_gates_of(helpers).is_some()
        || helpers.ref_store_post_gate != 0
        || helpers.ref_store_post_young_floor != 0
        || helpers.ref_store_post_skip_mask != 0;
    g1_aastore_plan_from(
        helpers.ref_store_pre_gate,
        plan_published,
        gen_card_view_of(helpers).is_some(),
        helpers.g1_barrier_addr,
        helpers.g1_post_write_barrier,
    )
}

/// r21 barriers (BARRIERS21-1) -- `CRATONVM_JIT_SP_G1_GATED_AASTORE=1`
/// (opt-in, default OFF). Under G1 the single-pass reference `aastore` skips
/// the old-element load and the `satb_pre_write_barrier` call while the SATB
/// pre-gate byte is clear. After the store it runs the F-08 filter (null
/// value, same region) and calls the lean `jit_g1_post_write_barrier` only for
/// a cross-region value, where the default arm always calls `write_barrier`.
/// Read per compiled site, as `g1_gated_ref_store_enabled` is.
pub(crate) fn sp_g1_gated_aastore_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_SP_G1_GATED_AASTORE")
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
/// gcf f1/p: `JitCardView::TRACE_SP_DIRTY_OFFSET` / `TRACE_SP_CLEAN_OFFSET`,
/// two of the view's four trace-only engagement counters (the optimizing
/// tier's pair is `ir_lower.rs`'s `IR_CARD_TRACE_*`). Each is a disp8 off the
/// view address the check already holds in R11.
const CARD_VIEW_TRACE_SP_DIRTY: u8 = 40;
const CARD_VIEW_TRACE_SP_CLEAN: u8 = 48;
/// gci i23/lc (`gcg-g1p-proposal-card-check-counts-its-skip-edges`):
/// `JitCardView::SKIP_TRACE_*_OFFSET`, the single-pass check's four skip
/// edges (young receiver, null value, old -> old, outside the view), and
/// `JitCardView::LEAN_POST_TRACE_OFFSET`, the lean POST's one-compare answers.
/// Trace-only, each a disp8 off the view.
const CARD_VIEW_SKIP_YOUNG: u8 = 88;
const CARD_VIEW_SKIP_NULL: u8 = 96;
const CARD_VIEW_SKIP_OLD_VALUE: u8 = 104;
const CARD_VIEW_SKIP_OUTSIDE: u8 = 112;
const CARD_VIEW_LEAN_POST: u8 = 120;
/// gcf f2/p: `JitCardView::SCAN_BOUND_OFFSET` -- the mirror of the card
/// table's self-maintained scan bound, in cards, that the card-store barrier
/// compares a card index against before it writes the byte itself.
pub(crate) const CARD_VIEW_SCAN_BOUND: i32 = 72;
/// gcf f2/p: `JitCardView::SUMMARY_NEG_OFFSET` -- the summary map's address,
/// negated (subtracted, as `CARD_VIEW_CARDS_NEG` is).
pub(crate) const CARD_VIEW_SUMMARY_NEG: i32 = 80;
/// gcf f2/p: `JitCardView::FLAG_SUMMARY`, read at compile time.
const CARD_VIEW_FLAG_SUMMARY: usize = 2;
/// gcf f2/p: `log2(cratonvm_gc::card_table::SUMMARY_GROUP)` -- one summary byte
/// per 64 cards, so a summary index is the card index shifted by 6 more.
pub(crate) const CARD_SUMMARY_SHIFT: u8 = 6;
/// gcj j2/jt2: `JitCardView::CARD_BASE_NEG_OFFSET` -- HotSpot's
/// `byte_map_base`, negated: `&cards[(mark - old_base) >> CARD_SHIFT]` is
/// `(mark >> CARD_SHIFT) - [view + this]` on a card-aligned table.
pub(crate) const CARD_VIEW_CARD_BASE_NEG: i32 = 128;
/// gcj j2/jt2: `JitCardView::POST_ONLY_SITES_OFFSET` -- bumped at COMPILE time
/// once per post-only store site this backend emits.
const CARD_VIEW_POST_ONLY_SITES: usize = 136;
/// gcj j2/jt2: `JitCardView::FLAG_BYTE_MAP_BASE`, read at compile time.
const CARD_VIEW_FLAG_BYTE_MAP_BASE: usize = 4;

/// gcj j2/jt2 — `CRATONVM_JIT_POST_ONLY_CARD_MARK` (default OFF, opt-in; `=1`
/// arms it): every inline generational reference-store barrier of both tiers
/// drops its PRE card check and keeps only the check after the store --
/// HotSpot's one-sided shape (`CardTableBarrierSetAssembler::store_check`
/// comes after the store), still behind this VM's young / null / range
/// filters (the card table covers the old generation only).
///
/// # Why the PRE check existed, and what replaces it
///
/// A BUG-03 take-over freezes a peer at an arbitrary compiled instruction and
/// holds it across the collection; a peer frozen between the store and its
/// card byte would leave the edge in the heap and the card clean. PRE made
/// sure the card was dirty BEFORE the store (`Compiler::emit_gen_card_barrier`'s
/// doc). With this switch the take-over finishes the mark instead: the
/// freeze records the peer's general-purpose registers
/// (`vm/src/jit/xt_root_scan.rs` `TakenOver::note_frozen_registers`) and the
/// card of every register word that lies in the old generation is dirtied
/// twice (`CardTable::complete_frozen_register_marks`): right after the freeze,
/// before the collection reads a card (`gc_and_alloc::run_takeover_pass`: the
/// pause sees the edge, as behind PRE), and at the resume, after the pause's
/// card scan and clear (`xt_root_scan::resume_completing_post_only_card_marks`). Through the whole window
/// from the store to the card byte the site keeps the holder (and, for an
/// element-precise `aastore`, the array) and the young value in registers
/// (the check's contract: `holder`, `val` and `mark` survive every edge), so
/// the holder's header card -- "scan the whole object" -- comes back dirty,
/// and the young value, a conservative root of the frozen peer, stayed where
/// it was. Over-marking costs a card scan.
///
/// The compiler counts every post-only site in the card view
/// (`JitCardView::post_only_sites`, [`Compiler::note_post_only_card_site`])
/// BEFORE the code can run, and the completion runs only while that count is
/// non-zero, so the two halves cannot disagree.
///
/// Refused (the PRE/POST pair stays) under
/// `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD` (gcd d5/s, opt-in), whose scan
/// trusts element cards over a dirty header card: a peer frozen before the
/// element address is formed would get only its header card back.
/// G1 and ZGC publish no generational card view, so no byte of theirs moves.
/// Read once per store site (both halves of the site read the one value).
pub(crate) fn jit_post_only_card_mark_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_POST_ONLY_CARD_MARK")
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD")
}

/// gcj j2/jt2 — `CRATONVM_JIT_CARD_BYTE_MAP_BASE` (default OFF, opt-in; `=1`
/// arms it): the inline card CHECK of both tiers forms the card address as
/// HotSpot does, `(mark >> CARD_SHIFT) + byte_map_base`, here
/// `shr r10, CARD_SHIFT; sub r10, [r11 + card_base_neg]` -- one shift and one
/// load instead of `sub r10, [r11 + old_base]; shr; sub r10, [r11 +
/// cards_neg]`. Engages only on a view whose `JitCardView::FLAG_BYTE_MAP_BASE`
/// says the word is exact (the covered range starts on a card boundary), so
/// it names the same card byte the classic form does (the collector's
/// `gcj2jt2_the_shift_first_card_address_names_the_same_card`). The
/// clean-card store paths keep the classic index (they compare it against
/// the scan-bound mirror). Read per compiled site.
pub(crate) fn jit_card_byte_map_base_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CARD_BYTE_MAP_BASE")
}

/// gcj j2/jt2: does the view at `view` (validated by [`gen_card_view_of`])
/// carry an exact `card_base_neg` word? Read at compile time.
pub(crate) fn card_view_has_byte_map_base(view: usize) -> bool {
    if view == 0 || (view & 7) != 0 {
        return false;
    }
    // SAFETY: as `card_view_has_summary`: a live, magic-checked view whose
    // `flags` word is written once at construction.
    let flags = unsafe { std::ptr::read((view + CARD_VIEW_FLAGS) as *const usize) };
    (flags & CARD_VIEW_FLAG_BYTE_MAP_BASE) != 0
}

/// gcj j2/jt2: count one post-only store site in the card view at `view`
/// (`JitCardView::post_only_sites`), at compile time. The take-over's
/// completion reads it; see [`jit_post_only_card_mark_enabled`].
pub(crate) fn note_post_only_card_site_in(view: usize) {
    if view == 0 || (view & 7) != 0 {
        return;
    }
    // SAFETY: `view` is a live, magic-checked `JitCardView` (the caller took
    // it from `gen_card_view_of`), whose `post_only_sites` word is an
    // `AtomicU64` at this offset (pinned by
    // `r4w4_cards4_tests::the_card_view_offsets_match_the_collector_s`), and
    // the view lives as long as the heap the compiled code runs against.
    let sites =
        unsafe { &*((view + CARD_VIEW_POST_ONLY_SITES) as *const std::sync::atomic::AtomicU64) };
    sites.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// gcf f2/p — `CRATONVM_JIT_CARD_STORE_BARRIER` (default ON since gcg g6/jm, `=0`
/// kills; read per compiled
/// store site, and only where the inline card check itself is emitted, i.e.
/// under `CRATONVM_JIT_INLINE_CARD_MARK=1` with a generational card view).
///
/// Turns the inline check's CLEAN-card edge from a call to the collector's
/// barrier into the barrier's own stores, emitted inline: if the card index is
/// below the view's scan-bound mirror, store the group's summary byte, the card
/// byte and the summary byte again (`CardTable::mark_dirty_lockfree`'s order),
/// then continue; an index at or above the mirror still calls, so the Rust
/// barrier raises the bound. With the already-dirty edge this makes the
/// compiled post barrier call-free in steady state: HotSpot Serial's
/// conditional card mark (`UseCondCardMark`) shape, with the PRE/POST pair the
/// BUG-03 take-over needs (`emit_gen_card_barrier`'s doc).
pub(crate) fn jit_card_store_barrier_enabled() -> bool {
    // gcg g6/jm: default ON; `=0` restores the check-only form (see the
    // optimizing tier's `ir_card_store_barrier_enabled` for the rows).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CARD_STORE_BARRIER")
}

/// gci i23/lc — `CRATONVM_JIT_LEAN_CARD_POST` (default OFF, opt-in; `=1` arms
/// it): the single-pass tier's lean POST card check
/// (`gcf-f1p-proposal-lean-inline-card-check-20261001` item 1).
///
/// Read ONCE per compiled store site (the PRE and the POST half of that site
/// must agree, so the site reads it into a local and hands it to both). With
/// it off every byte the site emits is the bytes it emitted before.
///
/// With it on, PRE ([`Compiler::emit_gen_card_barrier_pre`]) leaves
/// `R10 = &card` on its ONE "card already dirty" edge and `R10 = 0` on every
/// other edge (young receiver, null, old -> old, outside the view, the slow
/// call, a card marked inline), and POST
/// ([`Compiler::emit_gen_card_barrier_post`]) starts with
///
/// ```text
///   test r10, r10            ; jz full
///   cmp  byte [r10], DIRTY   ; je done     ; PRE's card, still dirty
/// full:
///   <the full POST barrier, unchanged>
/// done:
/// ```
///
/// so the steady state of an old-receiver store (card already dirty) runs
/// one compare after the store instead of the ~18-instruction, 5-load second
/// check. The take-over argument is unchanged: POST still reads the card byte
/// AFTER the store, and a card a pause cleaned between PRE and POST reads
/// clean and falls into the full check, which dirties it. HotSpot Serial
/// emits no POST at all; this keeps the BUG-03 PRE/POST pair.
pub(crate) fn jit_lean_card_post_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_LEAN_CARD_POST")
}

/// gci i23/lc: the inline check's patch sites, by edge. `dirty` is the one
/// "card already dirty" edge, the only one on which R10 holds `&card` -- the
/// lean POST's premise. [`Compiler::emit_gen_card_check_full`] merges it back
/// into `done`.
pub(super) struct GenCardCheckEdges {
    pub(super) slow: Vec<usize>,
    pub(super) done: Vec<usize>,
    pub(super) dirty: usize,
}

/// gcf f2/p — `CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM` (default ON; `=0` restores
/// the legacy rule): a compiled `new` of a class that allocates COMPACT and has
/// no finalizer does not need `jit_post_tlab_init` for its `long`/`float`/
/// `double` fields, whose compact (tagless) typed zero is zero bytes. Read per
/// compiled `new` site (`op_object.rs`'s `emit_resolved_new_alloc`).
pub(super) fn compact_new_zero_prim_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM")
}

/// gcf f2/p: does the generational card view at `view` carry a summary map
/// (`JitCardView::FLAG_SUMMARY`)? Read at compile time.
///
/// Callers pass the address [`gen_card_view_of`] validated (magic checked), so
/// word 4 is the view's `flags`.
pub(crate) fn card_view_has_summary(view: usize) -> bool {
    if view == 0 || (view & 7) != 0 {
        return false;
    }
    // SAFETY: `view` is a live `JitCardView` (the caller holds it from
    // `gen_card_view_of`, which checked the magic); `flags` is a plain word
    // written once at construction.
    let flags = unsafe { std::ptr::read((view + CARD_VIEW_FLAGS) as *const usize) };
    (flags & CARD_VIEW_FLAG_SUMMARY) != 0
}

/// `LOCK INC qword [R11 + disp8]` — bump one of the card view's trace
/// counters (gcf f1/p). R11 must hold the view address; clobbers the flags
/// only. `F0` LOCK, `49` REX.W+B, `FF /0` INC r/m64, ModRM `43` (mod=01,
/// reg=000, rm=011 = R11), then the disp8.
fn card_view_trace_inc_bytes(disp8: u8) -> [u8; 5] {
    debug_assert!(disp8 < 0x80, "a positive disp8");
    [0xF0, 0x49, 0xFF, 0x43, disp8]
}

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
/// (gen r4w4/cards4). Default ON since the gcg triage (tg1; `=0` restores the
/// helper call); read per compiled site so
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
    /// * A clean card BELOW the view's scan-bound mirror is marked inline in
    ///   the collector's own order (summary, card, summary;
    ///   [`Self::emit_gen_card_store_inline`]) under
    ///   `CRATONVM_JIT_CARD_STORE_BARRIER` (gcf f2/p; default ON since gcg
    ///   g6/jm), and a card at or above the mirror is a call to the
    ///   collector's own barrier, which keeps the scan bound and the summary
    ///   map armed (`cratonvm_gc::card_table::JitCardView`). With the switch
    ///   at `0` every clean card is that call, and compiled code writes no
    ///   card byte. (gch h1/fr: this bullet used to lead with "compiled code
    ///   never writes a card byte", the `=0` arm.)
    /// * The view's geometry is loaded at run time, never baked, and comes from
    ///   THIS VM's heap (`build_helpers_for`).
    ///
    /// Requires `CRATONVM_JIT_INLINE_CARD_MARK` (default ON since the gcg
    /// triage tg1; `=0` is the helper call), a
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

    /// gcj j2/jt2: this store site's one reading of
    /// [`jit_post_only_card_mark_enabled`], for a site whose inline
    /// generational barrier is emitted (`inline_card`). `true` means: emit no
    /// PRE half, no lean POST, and count the site
    /// ([`Self::note_post_only_card_site`]) -- the caller does all three.
    pub(super) fn post_only_card_site(&self, inline_card: bool) -> bool {
        inline_card && jit_post_only_card_mark_enabled()
    }

    /// gcj j2/jt2: count this compile's post-only store site in its heap's
    /// card view, at compile time (before the code can run): the BUG-03
    /// take-over completes the card mark of a frozen peer only while that
    /// count is non-zero ([`jit_post_only_card_mark_enabled`]).
    pub(super) fn note_post_only_card_site(&self) {
        if let Some((view, _)) = gen_card_view_of(&self.helpers) {
            note_post_only_card_site_in(view);
        }
    }

    /// gcj j2/jt2: may this compile's inline card check use the shift-first
    /// card address ([`jit_card_byte_map_base_enabled`])? Asked only from the
    /// check itself, whose caller validated the view
    /// ([`Self::inline_card_mark_available`]), as the summary flag is.
    fn gen_card_byte_map_base_form(&self) -> bool {
        jit_card_byte_map_base_enabled()
            && card_view_has_byte_map_base(self.helpers.jit_card_table_addr)
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
            self.emit_load_vm_context(ARG_REGS[0]);
        } else {
            // Byte-for-byte the sequence these arms emitted before.
            self.emit_load_vm_context(ARG_REGS[0]);
            self.load_slot_to_reg(ARG_REGS[1], array_slot);
            self.load_slot_to_reg(ARG_REGS[2], val_slot);
        }
    }

    /// gcf f4/p (`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot`,
    /// item 1, single-pass half): load the VM context pointer into `dst`, or
    /// REFUSE the compile when this method reserved no context slot.
    ///
    /// The slot exists only when the `needs_heap` scan
    /// (`x64/bytecode_compat.rs`) asked for it; otherwise `heap_local_offset`
    /// is `0` and `[rbp - 0]` is the saved caller RBP, which a raw load hands
    /// to a helper as a `SharedVm` -- the silent wild pointer of the
    /// `Catalina.setParentClassLoader` putfield and of gcd d1/mon. Here an
    /// unreserved slot is a named codegen failure
    /// (`vm-context-slot-unreserved`) instead: the method is discarded and runs
    /// interpreted. With the slot reserved the bytes are exactly the raw load's.
    ///
    /// The barrier and allocation sites this lane owns go through it; the
    /// remaining raw loads elsewhere in the backend are frozen by
    /// `gcf_f4p_raw_vm_context_loads_do_not_grow`.
    pub(super) fn emit_load_vm_context(&mut self, dst: u8) {
        if self.heap_local_offset == 0 {
            self.buf
                .mark_codegen_unencodable("vm-context-slot-unreserved");
            return;
        }
        self.emit_load_local(dst, self.heap_local_offset);
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
        let trace = crate::x64::sp_ref_store_trace_enabled();
        self.emit_gen_card_check_with(holder, val, mark, trace)
    }

    /// [`Self::emit_gen_card_check`] with the run-time engagement trace decided
    /// by the caller (gcf f1/p) — the seam the executed tests drive, since the
    /// production switch (`CRATONVM_DBG_SP_REF_STORE_TRACE`) latches once per
    /// process.
    ///
    /// `trace == false` emits exactly the bytes this check always emitted.
    /// `trace == true` replaces the final `JE done` with
    ///
    /// ```text
    ///   jne  clean
    ///   lock inc qword [r11 + TRACE_SP_DIRTY]     ; the call is spared
    ///   jmp  done
    /// clean:
    ///   lock inc qword [r11 + TRACE_SP_CLEAN]     ; falls into slow: a call
    /// ```
    ///
    /// R11 holds the view on that edge (it is loaded before the first range
    /// test and not touched again), and the counters are words of the view
    /// itself (`JitCardView::trace`), so the trace adds no process `static`.
    /// Out-of-view and past-the-end edges reach `slow` without a card-byte
    /// count: they are calls the card byte did not decide. (gci i23/lc: they,
    /// and the three skip-to-`done` edges, are counted in the view's
    /// `skip_trace` instead -- [`Self::emit_gen_card_check_edges`].)
    pub(super) fn emit_gen_card_check_with(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        trace: bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let store = jit_card_store_barrier_enabled();
        self.emit_gen_card_check_full(holder, val, mark, trace, store)
    }

    /// [`Self::emit_gen_card_check_with`] with the card-store form decided by
    /// the caller (gcf f2/p, `CRATONVM_JIT_CARD_STORE_BARRIER`) -- the seam the
    /// executed tests drive.
    ///
    /// `store == false` emits exactly the bytes the check always emitted.
    /// `store == true` appends, at the CLEAN-card fall-through (after the
    /// traced `clean` count, when traced), the inline mark of
    /// [`Self::emit_gen_card_store_inline`] and a `JMP done`: a clean card is
    /// then marked here rather than by a call, unless its index is at or above
    /// the view's scan-bound mirror, which still falls into `slow`.
    pub(super) fn emit_gen_card_check_full(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        trace: bool,
        store: bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let GenCardCheckEdges {
            slow,
            mut done,
            dirty,
        } = self.emit_gen_card_check_edges(holder, val, mark, trace, store);
        // Patch order does not change a byte: every `done` site is patched to
        // the same label.
        done.push(dirty);
        (slow, done)
    }

    /// [`Self::emit_gen_card_check_full`] with the "card already dirty" edge
    /// kept apart (gci i23/lc): the lean POST
    /// ([`jit_lean_card_post_enabled`]) needs to know which edge left
    /// `R10 = &card`.
    ///
    /// Untraced (`trace == false`) the bytes are exactly the ones this check
    /// always emitted. Traced, each SKIP edge (young receiver, null value,
    /// old -> old, receiver or mark outside the view) no longer branches
    /// straight to `done` / `slow` but through an out-of-line stub that bumps
    /// its `JitCardView::skip_trace` counter first
    /// (`gcg-g1p-proposal-card-check-counts-its-skip-edges`), so the four skip
    /// counts plus `sp_dirty` / `sp_clean` add up to every check executed. The
    /// young-receiver and null tests run before R11 holds the view, so their
    /// stubs load it themselves (R11 is the check's scratch on every edge).
    /// The stubs follow the check: a traced check that would fall through
    /// into `slow` jumps there first.
    pub(super) fn emit_gen_card_check_edges(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        trace: bool,
        store: bool,
    ) -> GenCardCheckEdges {
        debug_assert!(!matches!(holder, R10 | R11 | RSP | R12));
        debug_assert!(!matches!(val, R10 | R11));
        debug_assert!(!matches!(mark, R10 | R11));
        let view = self.helpers.jit_card_table_addr;
        let mut slow: Vec<usize> = Vec::new();
        let mut done: Vec<usize> = Vec::new();
        // gci i23/lc: the skip edges, routed through counting stubs when
        // traced and straight to `done` / `slow` otherwise.
        let mut skip_young: Vec<usize> = Vec::new();
        let mut skip_null: Vec<usize> = Vec::new();
        let mut skip_old_value: Vec<usize> = Vec::new();
        let mut skip_outside: Vec<usize> = Vec::new();

        // Young receiver: no card. The published mask's own test.
        self.emit_test_mem8_imm8(
            holder,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_OLD_GEN,
        );
        skip_young.push(self.emit_jcc_rel32_patch(0x84)); // JZ → young receiver
                                                          // Null store: no edge.
        self.emit_test_r64_r64(val);
        skip_null.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null value

        self.emit_mov_imm64_full(R11, view as i64); // Cast: baked data address
        self.emit_mov_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
        // A flagged-old receiver outside the covered range is the Rust
        // barrier's call (it knows the heap's live geometry), never a skip.
        self.emit_cmp_r64_r64(holder, R10);
        skip_outside.push(self.emit_jcc_rel32_patch(0x82)); // JB → slow
        self.emit_cmp_r64_mem_disp32(holder, R11, CARD_VIEW_OLD_END);
        skip_outside.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
        if mark != holder {
            // The element slot lies inside the holder, so this cannot fire for
            // a well-formed array; it keeps the card index below the map's
            // length even for one that is not.
            self.emit_cmp_r64_mem_disp32(mark, R11, CARD_VIEW_OLD_END);
            skip_outside.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
        }
        // Old → old needs no card.
        self.emit_cmp_r64_r64(val, R10);
        let young_target = self.emit_jcc_rel32_patch(0x82); // JB → young target
        self.emit_cmp_r64_mem_disp32(val, R11, CARD_VIEW_OLD_END);
        skip_old_value.push(self.emit_jcc_rel32_patch(0x82)); // JB → old target
        self.patch_rel32_to_here(young_target);

        // &cards[(mark - old_base) >> CARD_SHIFT]. The shift is
        // `cratonvm_types::CARD_SHIFT`, the one constant every card table in the
        // VM is indexed by (never a literal `9`).
        self.emit_mov_r64_r64(R10, mark);
        if self.gen_card_byte_map_base_form() {
            // gcj j2/jt2 (`CRATONVM_JIT_CARD_BYTE_MAP_BASE`): HotSpot's
            // `(mark >> shift) + byte_map_base`, the same card byte.
            self.emit_shr_r64_imm8(R10, cratonvm_types::CARD_SHIFT as u8); // Cast: 9
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_CARD_BASE_NEG);
        } else {
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
            self.emit_shr_r64_imm8(R10, cratonvm_types::CARD_SHIFT as u8); // Cast: 9
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_CARDS_NEG);
        }
        self.emit_cmp_mem8_imm8(R10, 0, CARD_DIRTY_BYTE);
        let dirty;
        if trace {
            let clean = self.emit_jcc_rel32_patch(0x85); // JNE → clean
            self.buf
                .emit(&card_view_trace_inc_bytes(CARD_VIEW_TRACE_SP_DIRTY));
            dirty = self.emit_jmp_rel32_patch(); // already dirty
            self.patch_rel32_to_here(clean);
            self.buf
                .emit(&card_view_trace_inc_bytes(CARD_VIEW_TRACE_SP_CLEAN));
        } else {
            dirty = self.emit_jcc_rel32_patch(0x84); // JE → already dirty
        }
        if store {
            // gcf f2/p: the clean card is marked here, not by a call.
            self.emit_gen_card_store_inline(mark, card_view_has_summary(view), &mut slow);
            done.push(self.emit_jmp_rel32_patch()); // marked
        }

        if !trace {
            // The untraced bytes: every skip edge is its target's own branch.
            done.extend(skip_young);
            done.extend(skip_null);
            done.extend(skip_old_value);
            slow.extend(skip_outside);
        } else {
            if !store {
                // The check's fall-through is `slow`; the stubs below must not
                // be reached by falling off its end.
                slow.push(self.emit_jmp_rel32_patch());
            }
            // Young receiver and null value: R11 is not the view yet.
            for (edges, counter) in [
                (skip_young, CARD_VIEW_SKIP_YOUNG),
                (skip_null, CARD_VIEW_SKIP_NULL),
            ] {
                for p in edges {
                    self.patch_rel32_to_here(p);
                }
                self.emit_mov_imm64_full(R11, view as i64); // Cast: baked data address
                self.buf.emit(&card_view_trace_inc_bytes(counter));
                done.push(self.emit_jmp_rel32_patch());
            }
            // Old -> old: R11 holds the view on this edge.
            for p in skip_old_value {
                self.patch_rel32_to_here(p);
            }
            self.buf
                .emit(&card_view_trace_inc_bytes(CARD_VIEW_SKIP_OLD_VALUE));
            done.push(self.emit_jmp_rel32_patch());
            // Outside the view: R11 holds the view; still the call.
            for p in skip_outside {
                self.patch_rel32_to_here(p);
            }
            self.buf
                .emit(&card_view_trace_inc_bytes(CARD_VIEW_SKIP_OUTSIDE));
            slow.push(self.emit_jmp_rel32_patch());
        }
        GenCardCheckEdges { slow, done, dirty }
    }

    /// gcf f2/p — the compiled CARD STORE: the collector's
    /// `CardTable::mark_dirty_lockfree` clean->dirty arm, inline
    /// (`CRATONVM_JIT_CARD_STORE_BARRIER`).
    ///
    /// Entry: R11 = the card view, `mark` = the address whose card is dirtied
    /// (already range-tested against the view, so its index is below the
    /// map's length), the card byte read CLEAN. Falls through after the
    /// stores; pushes the one refusal -- index at or above the view's
    /// scan-bound mirror -- onto `slow` (the Rust barrier then raises the bound
    /// before it stores, which compiled code cannot do).
    ///
    /// ```text
    ///   r10 = (mark - old_base) >> CARD_SHIFT        ; card index
    ///   cmp  r10, [r11 + scan_bound] ; jae slow
    ///  (r10 >>= 6 ; r10 -= [r11 + summary_neg] ; mov byte [r10], DIRTY)   summary, first
    ///  (r10 = (mark - old_base) >> CARD_SHIFT)
    ///   r10 -= [r11 + cards_neg]     ; mov byte [r10], DIRTY              card
    ///  (r10 = ((mark - old_base) >> (CARD_SHIFT + 6)) - summary_neg
    ///   mov byte [r10], DIRTY)                                            summary, again
    /// ```
    ///
    /// The parenthesised steps exist only when the view carries a summary map
    /// (`JitCardView::FLAG_SUMMARY`). Why each store is sound:
    ///
    /// * **Scan bound.** The mirror is raised only after the table's own bound
    ///   (`CardTable::set_scan_bound_bytes`), and both only rise, so a card
    ///   stored under the mirror lies under the bound every scan stops at.
    /// * **Summary.** Before AND after the card byte, the order
    ///   `mark_dirty_lockfree` keeps and for the same reason: a scan clears a
    ///   group's summary byte before it reads the group, so a peer frozen
    ///   between the first summary store and the card store would otherwise
    ///   leave a dirty card under a clean summary byte.
    /// * **Take-over.** Every store site still runs this check PRE and POST the
    ///   reference store (`emit_gen_card_barrier`'s doc): frozen before the
    ///   card store, the slot holds its old value or POST marks again; frozen
    ///   after it, the card is dirty for the collection.
    ///
    /// The bytes are plain stores, as the Rust barrier's `Release` stores are
    /// on x86-64. Clobbers R10 and the flags; `mark` and R11 survive.
    pub(super) fn emit_gen_card_store_inline(
        &mut self,
        mark: u8,
        summary: bool,
        slow: &mut Vec<usize>,
    ) {
        debug_assert!(!matches!(mark, R10 | R11));
        let shift = cratonvm_types::CARD_SHIFT as u8; // Cast: 9
        self.emit_mov_r64_r64(R10, mark);
        self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
        self.emit_shr_r64_imm8(R10, shift);
        self.emit_cmp_r64_mem_disp32(R10, R11, CARD_VIEW_SCAN_BOUND);
        slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow (above the mirror)
        if summary {
            self.emit_shr_r64_imm8(R10, CARD_SUMMARY_SHIFT);
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_SUMMARY_NEG);
            self.emit_mov_byte_mem_disp32_imm8(R10, 0, CARD_DIRTY_BYTE);
            self.emit_mov_r64_r64(R10, mark);
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
            self.emit_shr_r64_imm8(R10, shift);
        }
        self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_CARDS_NEG);
        self.emit_mov_byte_mem_disp32_imm8(R10, 0, CARD_DIRTY_BYTE);
        if summary {
            self.emit_mov_r64_r64(R10, mark);
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_OLD_BASE);
            self.emit_shr_r64_imm8(R10, shift + CARD_SUMMARY_SHIFT);
            self.emit_sub_r64_mem_disp32(R10, R11, CARD_VIEW_SUMMARY_NEG);
            self.emit_mov_byte_mem_disp32_imm8(R10, 0, CARD_DIRTY_BYTE);
        }
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
        self.emit_gen_card_barrier_impl(holder, val, mark, reload, false);
    }

    /// gci i23/lc — the PRE half of a store site's barrier pair. `lean` is the
    /// site's one reading of [`jit_lean_card_post_enabled`]; `false` emits
    /// exactly [`Self::emit_gen_card_barrier`]. `true` emits the same check
    /// and call, and then at the join
    ///
    /// ```text
    /// other:                      ; young, null, old->old, outside, slow call,
    ///   xor  r10d, r10d           ;   card marked inline: no card address
    /// dirty:                      ; PRE's "card already dirty" edge: R10 = &card
    /// ```
    ///
    /// so R10 tells the POST half ([`Self::emit_gen_card_barrier_post`])
    /// whether it may answer with one compare. The caller's store between the
    /// two halves must leave R10 alone: every store site that passes
    /// `lean == true` was audited for it (the compact stores write through
    /// RAX/RDX, the legacy cell stores through RCX/RDX, `emit_ref_astore_regs`
    /// through R11, the path trace through R11), and the one arm that does use
    /// R10 (the single-pass inline putfield's LEGACY shape) zeroes it.
    pub(super) fn emit_gen_card_barrier_pre(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        reload: &[(u8, StackSlot)],
        lean: bool,
    ) {
        self.emit_gen_card_barrier_impl(holder, val, mark, reload, lean);
    }

    /// gci i23/lc — the POST half of a store site's barrier pair: with
    /// `lean == false` exactly [`Self::emit_gen_card_barrier`]; with `true`,
    /// [`Self::emit_gen_card_lean_post_head`] and then that same full barrier
    /// as its miss arm. `lean` must be the value the site's PRE half got.
    pub(super) fn emit_gen_card_barrier_post(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        reload: &[(u8, StackSlot)],
        lean: bool,
    ) {
        if !lean {
            self.emit_gen_card_barrier(holder, val, mark, reload);
            return;
        }
        let hit = self.emit_gen_card_lean_post_head();
        self.emit_gen_card_barrier(holder, val, mark, reload);
        self.patch_rel32_to_here(hit);
        // Two paths join here (patch_rel32_to_here drops the mirror too).
        self.slot_mirror = None;
    }

    /// gci i23/lc — the lean POST's head: one compare of the card PRE left in
    /// R10, and the patch site of its hit edge (the caller patches it past
    /// the full POST it emits next). Falls through, R10 zero or the card
    /// clean, into that full POST.
    ///
    /// ```text
    ///   test r10, r10            ; jz  full     ; PRE took another edge
    ///   cmp  byte [r10], DIRTY   ; je  hit      ; still dirty: no second check
    /// full:
    /// ```
    ///
    /// Traced (`CRATONVM_DBG_SP_REF_STORE_TRACE=1`), the hit edge bumps the
    /// view's `lean_post_trace` first (`mov r11, imm64 &view; lock inc`); R11
    /// is free here (the full POST would clobber it anyway).
    pub(super) fn emit_gen_card_lean_post_head(&mut self) -> usize {
        let trace = crate::x64::sp_ref_store_trace_enabled();
        self.emit_gen_card_lean_post_head_with(trace)
    }

    /// [`Self::emit_gen_card_lean_post_head`] with the trace decided by the
    /// caller -- the seam the executed tests drive (the production switch
    /// latches once per process).
    pub(super) fn emit_gen_card_lean_post_head_with(&mut self, trace: bool) -> usize {
        self.emit_test_r64_r64(R10);
        let full = self.emit_jcc_rel32_patch(0x84); // JZ → full POST
        self.emit_cmp_mem8_imm8(R10, 0, CARD_DIRTY_BYTE);
        let hit = if trace {
            let miss = self.emit_jcc_rel32_patch(0x85); // JNE → full POST
            let view = self.helpers.jit_card_table_addr;
            self.emit_mov_imm64_full(R11, view as i64); // Cast: baked data address
            self.buf
                .emit(&card_view_trace_inc_bytes(CARD_VIEW_LEAN_POST));
            let hit = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(miss);
            hit
        } else {
            self.emit_jcc_rel32_patch(0x84) // JE → hit: PRE's card still dirty
        };
        self.patch_rel32_to_here(full);
        hit
    }

    /// [`Self::emit_gen_card_barrier`], with the lean PRE join
    /// ([`Self::emit_gen_card_barrier_pre`]) when `lean_pre`.
    fn emit_gen_card_barrier_impl(
        &mut self,
        holder: u8,
        val: u8,
        mark: u8,
        reload: &[(u8, StackSlot)],
        lean_pre: bool,
    ) {
        debug_assert!(self.inline_card_mark_available());
        // The reloads run AFTER a call, so their sources must survive one: a
        // frame slot or a callee-saved register, never the R8/R9 scratch cache
        // (every caller flushed it before popping these slots).
        debug_assert!(reload
            .iter()
            .all(|(_, s)| matches!(s, StackSlot::Frame(_) | StackSlot::CalleeSaved(_))));
        // gci i23/lc: a lean PRE keeps the dirty edge apart; otherwise the
        // check is exactly `emit_gen_card_check`.
        let (slow, done, dirty) = if lean_pre {
            let trace = crate::x64::sp_ref_store_trace_enabled();
            let store = jit_card_store_barrier_enabled();
            let e = self.emit_gen_card_check_edges(holder, val, mark, trace, store);
            (e.slow, e.done, Some(e.dirty))
        } else {
            let (slow, done) = self.emit_gen_card_check(holder, val, mark);
            (slow, done, None)
        };
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
        self.emit_load_vm_context(ARG_REGS[0]);
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
        if let Some(d) = dirty {
            // gci i23/lc: every edge but the dirty one says "no card address"
            // to the lean POST. The slow arm reaches this after its call
            // (which clobbered R10) and its reloads.
            self.emit_xor_reg_self(R10);
            self.patch_rel32_to_here(d);
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
        grow: Option<&mut Vec<usize>>,
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

        // JGE — full, or a corrupt count; the native decides. With a growth
        // stub (`str17`) a full payload grows there and this body runs again.
        let full = self.emit_jcc_rel32_patch(0x8D);
        match grow {
            Some(g) => g.push(full),
            None => decline.push(full),
        }

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

    /// The growth stub of one layout's inline `StringBuilder.append` body
    /// (strings round 2, `str17`; `CRATONVM_JIT_SB_GROW`).
    ///
    /// A full LATIN1 payload used to take the decline edge, the generic
    /// `jit_invoke_dispatch` into the JDK's `append` and its compiled growth
    /// chain: ~2 400 cycles, on 29% of `R16Bench strings`' iterations. The
    /// stub calls `DirectHelperTable::sb_grow` instead, which installs a copy
    /// grown by `newCapacity`'s rule, and runs the body again from `retry`;
    /// the second pass finds room. A `0` (not served) takes the decline edge
    /// as before, which re-reads its operands from their homes.
    ///
    /// No safepoint and no oop map, like the FFM accessors: the helper only
    /// bumps the thread's TLAB and never collects, so nothing it is handed
    /// can move. On entry RAX = the builder, and RCX / R8 / R9 hold whatever
    /// the body needs on its retry (the char; the source base and count),
    /// all four saved in the four words at `save_base` across the call.
    /// `extra_in_r9`: the append adds R9D characters (a String or the digit
    /// buffer, whose source coder R10D is LATIN1 on the retry) rather than
    /// one.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_sb_grow_stub(
        &mut self,
        grow: Vec<usize>,
        retry: usize,
        count_offset: i32,
        value_offset: i32,
        extra_in_r9: bool,
        save_base: i32,
        helper: usize,
        decline: &mut Vec<usize>,
    ) {
        if grow.is_empty() {
            return;
        }
        for p in grow {
            self.patch_rel32_to_here(p);
        }
        let save = |i: i32| save_base + 8 * i;
        self.emit_store_local(save(0), RAX);
        self.emit_store_local(save(1), RCX);
        self.emit_store_local(save(2), R8);
        self.emit_store_local(save(3), R9);
        // R10D = count + what this append adds: the capacity it needs. A
        // 32-bit wrap reads as a huge positive word the helper declines.
        self.emit_mov_r32_mem_disp32(R10, RAX, count_offset);
        if extra_in_r9 {
            self.buf.emit(&[0x45, 0x01, 0xCA]); // ADD R10D,R9D
        } else {
            self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        }
        // (vm_ptr, builder, value_offset, min_capacity). R10 and R11
        // are no argument register on either ABI, so ARG3 first, then the
        // rest, and ARG0 (RCX on Win64) last.
        self.emit_mov_r64_r64(R11, RAX);
        self.emit_mov_r64_r64(ARG_REGS[3], R10);
        self.emit_mov_r64_r64(ARG_REGS[1], R11);
        self.emit_mov_imm32_sx(ARG_REGS[2], value_offset);
        self.emit_load_vm_context(ARG_REGS[0]);
        crate::assert_direct_helper_call_shape!("sb_grow", int_args = 4, returns_value = true);
        self.emit_call_absolute(helper);
        self.slot_mirror = None;
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_load_local(RAX, save(0));
        self.emit_load_local(RCX, save(1));
        self.emit_load_local(R8, save(2));
        self.emit_load_local(R9, save(3));
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ — not served
        if extra_in_r9 {
            // The retried string body reads the source coder from R10D.
            self.emit_xor_reg_self(R10);
        }
        let back = self.emit_jmp_rel32_patch();
        let rel = retry as i32 - (back as i32 + 4); // Cast: rel32 displacement
        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        self.slot_mirror = None;
    }

    /// The four save words of [`Self::emit_sb_grow_stub`], reserved ABOVE the
    /// popped operands' frame homes (the decline edge re-reads its operands
    /// from there after a stub that was not served) -- the `append(int)`
    /// scratch's placement. `None` = no room; the site then declines a full
    /// payload as it always did. The caller rewinds the cursor before the
    /// decline edge reserves its own buffer.
    pub(super) fn reserve_sb_grow_save(&mut self, homes: &[StackSlot]) -> Option<i32> {
        let before = self.next_spill_offset;
        let mut floor = before;
        for slot in homes {
            if let StackSlot::Frame(off) = *slot {
                floor = floor.max(off.saturating_add(8));
            }
        }
        self.next_spill_offset = floor;
        let base = self.reserve_spill_slots(4, SpillReason::HelperArgs);
        if base.is_none() {
            self.next_spill_offset = before;
        }
        base
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
        grow: Option<&mut Vec<usize>>,
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
        self.emit_sb_append_string_copy(count_offset, 1, true, src_is_heap_array, decline, None);
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

        // Only the LATIN1-into-LATIN1 arm grows (`str17`): its retry needs no
        // coder of its own, the source's being LATIN1 too.
        self.emit_sb_append_string_copy(count_offset, 0, false, src_is_heap_array, decline, grow);
        let latin1_done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(utf16_same);
        self.emit_sb_append_string_copy(count_offset, 1, false, src_is_heap_array, decline, None);
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
        grow: Option<&mut Vec<usize>>,
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
        let full = self.emit_jcc_rel32_patch(0x8F); // JG
        match grow {
            Some(g) => g.push(full),
            None => decline.push(full),
        }

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
        if implicit_ok
            && crate::implicit_null::active()
            && !self.null_check_explicit_in_try(bc_pc)
            && !self.null_check_explicit_after_traps(bc_pc)
        {
            self.implicit_null_pending.push((self.buf.pos(), bc_pc));
            super::null_check_elim::note_receiver_null_check_implicit(arm);
            return Vec::new();
        }
        super::null_check_elim::note_receiver_null_check_emitted();
        self.emit_trusted_oop_receiver_check()
    }

    /// nio21 wave 19 L1 (`CRATONVM_JIT_EXPLICIT_NULL_CHECK_IN_TRY`, default
    /// ON; `0` restores the implicit form everywhere): a `getfield` inside a
    /// protected range of this method keeps its explicit `TEST`/`JZ`.
    ///
    /// The implicit form turns every null receiver into a hardware fault, a
    /// signal delivery and a `sigreturn` before the slow path runs -- about
    /// 2-3 us on the host, which is what kept `Nio21ImplicitCatch`'s
    /// `fieldnpe` row at ~2900 ns/op with the compiled catch and the fast throw
    /// on, against ~170 for the bounds-check row whose guard is explicit.
    /// HotSpot pays a trap once and recompiles the site with an explicit check
    /// (`too_many_traps(Reason_null_check)`); a site whose own method catches
    /// what it throws is where a null receiver is expected, so it gets the
    /// explicit check up front. The slow path is the same one the fault
    /// recovery jumps to, so the exception, its message and its trace are
    /// unchanged; the cost is one `TEST`/`JZ` on the non-null path.
    /// Compile-time only, read per protected site, no static.
    pub(super) fn null_check_explicit_in_try(&self, bc_pc: usize) -> bool {
        self.pc_is_protected(bc_pc)
            && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_EXPLICIT_NULL_CHECK_IN_TRY",
            )
    }

    /// JIT round 20 wave 12, lane qNull (L1W19-2,
    /// `CRATONVM_JIT_NULL_TRAP_RECOMPILE`, default ON; `0` restores the
    /// implicit form at every site): a `getfield` whose implicit null check
    /// has faulted `PER_BCI_DESPEC_LIMIT` times in this VM keeps its explicit
    /// `TEST`/`JZ` -- HotSpot's `too_many_traps(Reason_null_check)` recompile.
    ///
    /// [`Self::null_check_explicit_in_try`] covers a null caught by this
    /// method's own handler; this covers the one that escapes to a CALLER's
    /// handler, which no compile-time fact predicts. The count is kept by
    /// `jit_getfield` (the `GETFIELD_IMPLICIT_NULL_SITE` mark this tier puts
    /// on the slow path, [`Self::implicit_null_site_mark`]) in the VM's
    /// de-spec registry, under this compile's `method_key` and the site's
    /// interpreter bci; the helper evicts the method so this recompile runs.
    /// The slow path the explicit `JZ` reaches is the one the fault recovery
    /// jumps to, so the NPE, its helpful message and its trace are unchanged.
    /// No VM (no registry), or the switch off: `false`.
    pub(super) fn null_check_explicit_after_traps(&self, bc_pc: usize) -> bool {
        let Some(registry) = self.despec.as_ref() else {
            return false;
        };
        let Ok(bci) = u32::try_from(self.orig_bci(bc_pc)) else {
            return false;
        };
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_NULL_TRAP_RECOMPILE")
            && registry.implicit_null_trapped(&self.method_key, bci)
    }

    /// L1W19-2: the `GETFIELD_IMPLICIT_NULL_SITE` mark for the `jit_getfield`
    /// argument of a slow path that `bind_implicit_null_recovery` just made an
    /// implicit null check's recovery address (`bound`), so the helper can
    /// count the faults that reach it; `0` when nothing was bound, with no VM
    /// to count in, or with `CRATONVM_JIT_NULL_TRAP_RECOMPILE=0` (the argument
    /// is then today's, byte for byte).
    pub(super) fn implicit_null_site_mark(&self, bound: bool) -> u64 {
        if bound
            && self.despec.is_some()
            && !self.failed
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_NULL_TRAP_RECOMPILE")
        {
            cratonvm_jit_api::GETFIELD_IMPLICIT_NULL_SITE
        } else {
            0
        }
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

    /// g1put (`CRATONVM_JIT_IR_G1_GATED_REF_STORE`, default on; the optimizing
    /// tier's `emit_g1_ir_ref_putfield` has the argument in full): under G1 the
    /// old value of a reference field matters only while the SATB pre-gate byte
    /// (`helpers.ref_store_pre_gate`) is armed. G1's queue arms it before it
    /// activates (inside the STW initial mark) and disarms it after it
    /// deactivates, and the caller puts no safepoint between this test and its
    /// store. (gci i12/wb: "no safepoint" does not stop the BUG-03 takeover,
    /// which can freeze this thread between the test and the store across the
    /// activating pause. The receiver stays in RAX across that window, and the
    /// G1 concurrent-start pause scans every object a frozen peer holds,
    /// `G1Collector::scan_frozen_held_objects`, so the value the store will
    /// overwrite is gray before the peer resumes; see
    /// `gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 1. Keep the
    /// receiver in a register or a scanned slot up to the store.)
    /// Emits `CMP byte [gate], 0; JE` and returns the patch, which the
    /// caller lands right after its old-value test; `None` (nothing emitted)
    /// off G1, without a wired gate, or under the kill switch. Clobbers R11.
    pub(super) fn emit_g1_satb_gate_skip(&mut self, g1: bool) -> Option<usize> {
        if !g1 || self.helpers.ref_store_pre_gate == 0 || !g1_gated_ref_store_enabled() {
            return None;
        }
        self.emit_mov_imm64_full(R11, self.helpers.ref_store_pre_gate as i64);
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        Some(self.emit_jcc_rel32_patch(0x84)) // JE -> no mark: past the old-value test
    }

    /// The single-pass `aastore`'s SATB gate: `MOV R11, imm64 &pre ; CMP byte
    /// [R11], 0 ; JE`. Returns the JE patch, which the caller lands past its
    /// old-element load and `satb_pre_write_barrier` call. These are the bytes
    /// the generational gated arm has always emitted (r9-ea). r21 barriers
    /// (BARRIERS21-1) gives them to G1 too, with G1's pre-gate byte. Clobbers
    /// R11 and the flags.
    pub(super) fn emit_sp_aastore_satb_gate_skip(&mut self, pre: usize) -> usize {
        self.emit_mov_imm64_full(R11, pre as i64); // Cast: a baked data address
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        self.emit_jcc_rel32_patch(0x84)
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
        self.emit_load_vm_context(ARG_REGS[0]);
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
        self.emit_load_vm_context(ARG_REGS[0]);
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

        // g1put: under G1 the old value matters only while a mark runs.
        let no_satb = self.emit_g1_satb_gate_skip(g1);
        // A non-null old value needs the SATB pre-barrier.
        self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
        self.emit_test_r64_r64(RCX);
        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null -> helper
        if let Some(p) = no_satb {
            self.patch_rel32_to_here(p);
        }

        // No field-count bounds check: the receiver was just proven compact,
        // a compact instance's 8-byte header has no count word (offset 8 is
        // its first field), and its layout is its class's -- which the
        // verifier proved declares this field.

        // Compact reference fields are bare 8-byte pointers.
        self.load_slot_to_reg(RDX, val_slot);
        // gen r4w4/cards4: the inline generational barrier, PRE half (see
        // `emit_gen_card_barrier` for why the card is checked on both sides).
        let inline_card = !g1 && self.inline_card_mark_available();
        // gcj j2/jt2: post-only (no PRE half, no lean POST), one reading.
        let post_only = self.post_only_card_site(inline_card);
        // gci i23/lc: one reading for both halves. The store and the path
        // trace between them touch RAX/RDX/R11 only, never R10.
        let lean = inline_card && !post_only && jit_lean_card_post_enabled();
        if post_only {
            self.note_post_only_card_site();
        } else if inline_card {
            self.emit_gen_card_barrier_pre(
                RAX,
                RDX,
                RAX,
                &[(RAX, obj_slot), (RDX, val_slot)],
                lean,
            );
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
            self.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], lean);
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
    ///
    /// `old_value_proven_null`: the caller proved the same
    /// `inline_ctor_store_starts_from_null` (under any collector). When it did
    /// not, the site is only the body's first SYNTACTIC store to that slot
    /// index (`inline_site_is_fresh_ctor_first_store`), which says nothing
    /// about a receiver other than `this` (`prev.next = this`) or about a
    /// field an earlier spliced call wrote. Round 16 lane alloc: such a site
    /// now tests the old value and takes the helper (SATB pre-barrier) when it
    /// is non-null, as `emit_inline_body_compact_ref_putfield` does; the
    /// generational collector arms SATB during its concurrent old mark, and
    /// this arm reads no pre gate. Kill switch
    /// `CRATONVM_JIT_FRESH_CTOR_OLD_VALUE_TEST=0` (default ON).
    pub(super) fn emit_inline_fresh_ctor_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
        receiver_is_new_this: bool,
        old_value_proven_null: bool,
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
        // Round 16 lane alloc: a first syntactic store that is not proven to
        // start from null owes the SATB pre-barrier for a non-null old value
        // (see the doc). RCX is dead past the test.
        if !old_value_proven_null && fresh_ctor_old_value_test_enabled() {
            self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
            self.emit_test_r64_r64(RCX);
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old -> helper
        }

        self.load_slot_to_reg(RDX, val_slot);
        // gen r4w4/cards4: PRE half of the inline generational barrier. A fresh
        // `new` result is young unless allocation spilled it into old gen, so
        // this is normally the one-instruction young-receiver skip.
        let inline_card = !g1 && self.inline_card_mark_available();
        // gcj j2/jt2: post-only drops this PRE half (one reading for both).
        let post_only = self.post_only_card_site(inline_card);
        if post_only {
            self.note_post_only_card_site();
        } else if inline_card {
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
    ///
    /// `commit` (nio21 wave 10 L1, [`zgc_start_bit_before_commit_enabled`];
    /// see [`emit_zgc_inline_announce_raw_committing`], whose bytes this
    /// matches): `AlreadyCommitted` is the round-9 contract above. Anything
    /// else is entered BEFORE the commit (header written, cursor not), emits
    /// the commit on the inline arm right after the `or` (clobbering RCX, dead
    /// there), and reaches every `to_helper` edge uncommitted.
    pub(super) fn emit_zgc_inline_announce(
        &mut self,
        cursor_off: i32,
        end_off: i32,
        commit: ZgcAnnounceCommit,
    ) -> (usize, Vec<usize>) {
        // zr z9/bo: the short form is one sequence for both tiers. zr z11/ia:
        // so is the long one (the trims are read in one place); this used to
        // be the round-9 copy of its bytes.
        emit_zgc_inline_announce_raw_committing(&mut self.buf, cursor_off, end_off, commit)
    }

    /// [`Self::emit_zgc_inline_announce`] for a site that enters it with RAX =
    /// the bump's new cursor (zr z11/ia,
    /// [`emit_zgc_inline_announce_raw_committing_end_in_rax`]): this tier's
    /// inline `new`, `newarray` / `anewarray` and box prefix, whose bump
    /// leaves its `LEA RAX` end there for the commit (`MOV [R10 + cursor],
    /// RAX` on the commit-first order).
    pub(super) fn emit_zgc_inline_announce_end_in_rax(
        &mut self,
        cursor_off: i32,
        end_off: i32,
        commit: ZgcAnnounceCommit,
    ) -> (usize, Vec<usize>) {
        emit_zgc_inline_announce_raw_committing_end_in_rax(
            &mut self.buf,
            cursor_off,
            end_off,
            commit,
        )
    }

    /// The CALL-FREE form of [`Self::emit_zgc_inline_announce`], half one: every
    /// condition of `cratonvm_gc::tlab::JitZgcAnnounceTable`'s contract, asked
    /// BEFORE the caller's first store, for each object in `objs`, with every
    /// refusal pushed onto `decline` (JIT round 17 wave 2, lane sbzgc; the
    /// inline `StringBuilder.toString()`, which bumps a `byte[]` and a `String`
    /// together and has no helper arm to fall into after its commit).
    ///
    /// Entered with R10 = the `JvmThread*` and each `objs` register holding an
    /// object address the caller is about to head and commit inside the
    /// thread's TLAB. Clobbers RAX, R8 and the flags; preserves everything
    /// else, so `objs` must name none of RAX, R8, R10.
    ///
    /// Asking before the commit rather than after is the same question with
    /// the same answer. Conditions 1-3 (`zgc_announce`, the ownership epoch,
    /// `blocked`) and the table words change only while the mutator is stopped
    /// at a poll, the property the plain-store arm already rests on, and the
    /// caller emits no poll and no call between this check and
    /// [`Self::emit_zgc_start_bit_store_prechecked`]. Conditions 4-5 are
    /// functions of the object address and the buffer's `[start, end)`, which
    /// the commit does not move (it writes only the cursor).
    ///
    /// The fifth condition is spelled `word_lo + 512 <= end` rather than
    /// `end - word_lo >= 512`: equal for any word inside the arena, and it needs
    /// one scratch register instead of two.
    ///
    /// `count` (JIT round 17 wave 6, lane sbzgc2, SBZGC17-5) counts each
    /// decline cause at run time into its own
    /// [`crate::metrics::SB_TO_STRING_CENSUS`] row; the inline `toString()`,
    /// this pre-check's only production caller, passes `CRATONVM_DBG_JITC`.
    /// `false` emits byte for byte the uncounted form. A counted edge is
    /// [`Self::emit_decline_jcc_counted`]: the fall-through path is
    /// unchanged, and the decline path clobbers R8, which every edge of this
    /// pre-check already leaves dead.
    pub(super) fn emit_zgc_start_bit_precheck(
        &mut self,
        cursor_off: i32,
        end_off: i32,
        objs: &[u8],
        decline: &mut Vec<usize>,
        count: bool,
    ) {
        let short = zgc_jit_short_announce_enabled();
        self.emit_zgc_start_bit_precheck_form(cursor_off, end_off, objs, decline, count, short);
    }

    /// [`Self::emit_zgc_start_bit_precheck`] with the form chosen by the
    /// caller (zr z9/bo, [`zgc_jit_short_announce_enabled`]).
    pub(super) fn emit_zgc_start_bit_precheck_form(
        &mut self,
        cursor_off: i32,
        end_off: i32,
        objs: &[u8],
        decline: &mut Vec<usize>,
        count: bool,
        short: bool,
    ) {
        if short {
            self.emit_zgc_start_bit_precheck_short(cursor_off, end_off, objs, decline, count);
            return;
        }
        debug_assert!(
            objs.iter().all(|r| *r != RAX && *r != R8 && *r != R10),
            "the pre-check clobbers RAX and R8 and reads the thread from R10"
        );
        use crate::metrics as m;
        let row = |r: usize| -> Option<&'static std::sync::atomic::AtomicU64> {
            if count {
                m::SB_TO_STRING_CENSUS.get(r)
            } else {
                None
            }
        };
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        // 1. The buffer's armed table, or 0.
        self.emit_mov_r64_mem_disp32(RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        self.emit_test_r64_r64(RAX);
        self.emit_decline_jcc_counted(0x84, row(m::SB_TS_ZGC_NO_ANNOUNCE), decline); // JE
                                                                                     // 2. Ownership still valid.
        self.emit_mov_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_EPOCH);
        self.emit_cmp_r64_mem_disp32(R8, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
        self.emit_decline_jcc_counted(0x85, row(m::SB_TS_ZGC_EPOCH), decline); // JNE
                                                                               // 3. Not marking (allocate-black), not generational (young grain).
        self.emit_mov_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_BLOCKED);
        self.emit_test_r64_r64(R8);
        self.emit_decline_jcc_counted(0x85, row(m::SB_TS_ZGC_BLOCKED), decline); // JNE
                                                                                 // 4a. A published bitmap.
        self.emit_mov_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_WORDS);
        self.emit_test_r64_r64(R8);
        self.emit_decline_jcc_counted(0x84, row(m::SB_TS_ZGC_NO_BITMAP), decline); // JE
        for &obj in objs {
            // 4b. R8 = obj - base, in range and on the 8-byte grid.
            self.emit_mov_r64_r64(R8, obj);
            self.emit_sub_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_BASE);
            // JB: obj < base
            self.emit_decline_jcc_counted(0x82, row(m::SB_TS_ZGC_OUTSIDE_ARENA), decline);
            self.emit_cmp_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_SPAN);
            // JAE: off >= span
            self.emit_decline_jcc_counted(0x83, row(m::SB_TS_ZGC_OUTSIDE_ARENA), decline);
            self.emit_test_r64_imm32(R8, 7);
            // JNE: off-grid
            self.emit_decline_jcc_counted(0x85, row(m::SB_TS_ZGC_OUTSIDE_ARENA), decline);
            // 5. R8 = word_lo = base + (off & -512) ...
            // AND r8, imm32 (-512): REX.W+B 81 /4, modrm 11 100 000.
            self.buf.emit(&[0x49, 0x81, 0xE0]);
            self.buf.emit(&(-512i32).to_le_bytes());
            // ADD r8, [rax + base]: REX.W+R 03 /r, modrm 10 000 000.
            self.buf.emit(&[0x4C, 0x03, 0x80]);
            self.buf.emit(&ZGC_ANNOUNCE_BASE.to_le_bytes());
            // ... >= start ...
            self.emit_cmp_r64_mem_disp32(R8, R10, tlab + ZGC_TLAB_START_OFFSET);
            // JB
            self.emit_decline_jcc_counted(0x82, row(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB), decline);
            // ... and word_lo + 512 <= end. ADD r8, imm32 (512): REX.W+B 81 /0.
            self.buf.emit(&[0x49, 0x81, 0xC0]);
            self.buf.emit(&512i32.to_le_bytes());
            self.emit_cmp_r64_mem_disp32(R8, R10, end_off);
            // JA
            self.emit_decline_jcc_counted(0x87, row(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB), decline);
        }
    }

    /// zr z9/bo: the SHORT pre-check ([`zgc_jit_short_announce_enabled`]):
    /// the armed table, the epoch, the table's `short_blocked` word, then per
    /// object only the owned-word test (`obj & -512 >= start`,
    /// `obj | 511 < end`). Same entry state, clobbers (RAX, R8, flags) and
    /// decline contract as the long form; a `short_blocked` decline counts
    /// under `SB_TS_ZGC_BLOCKED` (it also covers an unpublished table and a
    /// misaligned base, which the long form counts apart). Uncounted, it is
    /// byte for byte `concat_emit::emit_zgc_start_bit_precheck_form`'s short arm.
    fn emit_zgc_start_bit_precheck_short(
        &mut self,
        cursor_off: i32,
        end_off: i32,
        objs: &[u8],
        decline: &mut Vec<usize>,
        count: bool,
    ) {
        debug_assert!(
            objs.iter().all(|r| *r != RAX && *r != R8 && *r != R10),
            "the pre-check clobbers RAX and R8 and reads the thread from R10"
        );
        use crate::metrics as m;
        let row = |r: usize| -> Option<&'static std::sync::atomic::AtomicU64> {
            if count {
                m::SB_TO_STRING_CENSUS.get(r)
            } else {
                None
            }
        };
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        self.emit_mov_r64_mem_disp32(RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        self.emit_test_r64_r64(RAX);
        self.emit_decline_jcc_counted(0x84, row(m::SB_TS_ZGC_NO_ANNOUNCE), decline); // JE
        self.emit_mov_r64_mem_disp32(R8, RAX, ZGC_ANNOUNCE_EPOCH);
        self.emit_cmp_r64_mem_disp32(R8, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
        self.emit_decline_jcc_counted(0x85, row(m::SB_TS_ZGC_EPOCH), decline); // JNE
        emit_cmp_qword_rax_disp_zero(&mut self.buf, ZGC_ANNOUNCE_SHORT_BLOCKED);
        self.emit_decline_jcc_counted(0x85, row(m::SB_TS_ZGC_BLOCKED), decline); // JNE
        for &obj in objs {
            self.emit_mov_r64_r64(R8, obj);
            // AND r8, imm32 (-512): REX.W+B 81 /4, modrm 11 100 000.
            self.buf.emit(&[0x49, 0x81, 0xE0]);
            self.buf.emit(&(-512i32).to_le_bytes());
            self.emit_cmp_r64_mem_disp32(R8, R10, tlab + ZGC_TLAB_START_OFFSET);
            // JB
            self.emit_decline_jcc_counted(0x82, row(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB), decline);
            // OR r8, imm32 (511): REX.W+B 81 /1, modrm 11 001 000.
            self.buf.emit(&[0x49, 0x81, 0xC8]);
            self.buf.emit(&511i32.to_le_bytes());
            self.emit_cmp_r64_mem_disp32(R8, R10, end_off);
            // JAE
            self.emit_decline_jcc_counted(0x83, row(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB), decline);
        }
    }

    /// `MOV R8, &counter ; LOCK INC QWORD [R8]` (JIT round 17 wave 6, lane
    /// sbzgc2). Clobbers R8 and the flags, nothing else.
    pub(super) fn emit_lock_inc_counter_via_r8(
        &mut self,
        counter: &'static std::sync::atomic::AtomicU64,
    ) {
        // Cast: the 'static counter's address, baked.
        self.emit_mov_imm64_full(R8, counter as *const _ as i64);
        // LOCK INC QWORD [R8]: F0, REX.W+B, FF /0, modrm 00 000 000.
        self.buf.emit(&[0xF0, 0x49, 0xFF, 0x00]);
    }

    /// A decline edge `Jcc(cc) -> decline`, counted on the way out when
    /// `counter` is `Some` (JIT round 17 wave 6, lane sbzgc2).
    ///
    /// `None` is exactly `decline.push(self.emit_jcc_rel32_patch(cc))`. `Some`
    /// inverts the condition over a three-instruction stub (`cc ^ 1`: every
    /// x86 `Jcc` pair differs in the low bit) that bumps the counter and
    /// jumps to the decline target, so the fall-through path keeps every
    /// register and the flags; the taken path clobbers R8 and the flags, so
    /// use it only on edges whose target re-derives both (the inline
    /// `toString()`'s, which re-reads the receiver from its slot).
    pub(super) fn emit_decline_jcc_counted(
        &mut self,
        cc: u8,
        counter: Option<&'static std::sync::atomic::AtomicU64>,
        decline: &mut Vec<usize>,
    ) {
        match counter {
            None => decline.push(self.emit_jcc_rel32_patch(cc)),
            Some(c) => {
                let stay = self.emit_jcc_rel32_patch(cc ^ 1);
                self.emit_lock_inc_counter_via_r8(c);
                decline.push(self.emit_jmp_rel32_patch());
                self.patch_rel32_to_here(stay);
            }
        }
    }

    /// Half two of the call-free announce: the start bit of `obj`, set with the
    /// same unlocked `or` as [`Self::emit_zgc_inline_announce`]. Sound ONLY
    /// behind a passed [`Self::emit_zgc_start_bit_precheck`] that named `obj`,
    /// with no poll and no call in between (see that function), and only after
    /// the object's header and the cursor commit are stored.
    ///
    /// R10 = the `JvmThread*`. Clobbers RAX, RCX, RDX, R8 and the flags; `obj`
    /// must be none of those nor R10.
    pub(super) fn emit_zgc_start_bit_store_prechecked(&mut self, cursor_off: i32, obj: u8) {
        let short = zgc_jit_short_announce_enabled();
        self.emit_zgc_start_bit_store_prechecked_form(cursor_off, obj, short);
    }

    /// [`Self::emit_zgc_start_bit_store_prechecked`] with the form chosen by
    /// the caller (zr z9/bo). The short form must follow a SHORT pre-check
    /// (same switch, read in the same compile): it trusts `short_blocked` and
    /// reaches the word through `granule_bias`.
    pub(super) fn emit_zgc_start_bit_store_prechecked_form(
        &mut self,
        cursor_off: i32,
        obj: u8,
        short: bool,
    ) {
        debug_assert!(
            ![RAX, RCX, RDX, R8, R10].contains(&obj),
            "the start-bit store clobbers RAX, RCX, RDX, R8 and reads R10"
        );
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        if short {
            self.emit_mov_r64_mem_disp32(RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
            self.emit_mov_r64_mem_disp32(RDX, RAX, ZGC_ANNOUNCE_GRANULE_BIAS);
            // R8 = word index (obj >> 9), RCX = obj >> 3 (CL & 63 = the bit).
            self.emit_mov_r64_r64(R8, obj);
            self.emit_shr_r64_imm8(R8, 9);
            self.emit_mov_r64_r64(RCX, obj);
            self.emit_shr_r64_imm8(RCX, 3);
            self.emit_mov_imm32_sx(RAX, 1);
            // SHL rax, cl: REX.W D3 /4, modrm 11 100 000.
            self.buf.emit(&[0x48, 0xD3, 0xE0]);
            // OR [rdx + r8*8], rax.
            self.buf.emit(&[0x4A, 0x09, 0x04, 0xC2]);
            return;
        }
        self.emit_mov_r64_mem_disp32(RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        self.emit_mov_r64_mem_disp32(RDX, RAX, ZGC_ANNOUNCE_WORDS);
        // RCX = bit index, R8 = word index, RCX = bit within the word.
        self.emit_mov_r64_r64(RCX, obj);
        self.emit_sub_r64_mem_disp32(RCX, RAX, ZGC_ANNOUNCE_BASE);
        self.emit_shr_r64_imm8(RCX, 3);
        self.emit_mov_r64_r64(R8, RCX);
        self.emit_shr_r64_imm8(R8, 6);
        self.emit_and_r64_imm8(RCX, 63);
        // RAX = 1 << CL. SHL rax, cl: REX.W D3 /4, modrm 11 100 000.
        self.emit_mov_imm32_sx(RAX, 1);
        self.buf.emit(&[0x48, 0xD3, 0xE0]);
        // OR [rdx + r8*8], rax: REX.W+X 09 /r, modrm 00 000 100, SIB 11 000 010.
        self.buf.emit(&[0x4A, 0x09, 0x04, 0xC2]);
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
        // nio21 wave 10 L1: on the DECLINE form `inline_announce` means "this
        // box must be announced to ZGC here" (no decline-form caller passed
        // `true` before): the start bit is set before the commit
        // (`zgc_start_bit_before_commit_enabled`), or, with that switch off,
        // the al16 commit-then-`emit_zgc_announce_or_rollback`; every refusal
        // joins `decline`. RCX, RDX, R8 and R9 survive either way.
        let box_announce = inline_announce && decline_form;
        let inline_announce = inline_announce && !decline_form;
        let skip_post_init_helper = post_init == TlabPostInit::Skip;
        // History (round 15 wave 5, lane osrtier: this used to read as the
        // CURRENT rule, and `jit_bridge::jit_new_site_flags`' doc and the OSR
        // door's literal `(true, true)` row were justified by it). A raw
        // compiled bump that updated `Tlab::cursor` outside the allocator's
        // publication protocol once left a malformed young-space span in
        // concurrent Elasticsearch merge churn, and the bump was routed
        // through the checked runtime helper for a while. The bump below is
        // inline under `inline_tlab_new_enabled()`
        // (`CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` is the helper route): it writes
        // the whole header first and commits the cursor LAST (see "Commit the
        // bump LAST" below), and every method-entry body ships it.
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
            self.emit_load_vm_context(ARG_REGS[0]);
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
        // the slow path would incur. (gcg g4/jm: the opt-in that skipped
        // these two instructions was deleted; triage tg1 measured no gain.)
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
        //
        // nio21 wave 10 L1 (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`, default
        // ON): on the inline ZGC announce arm the commit moves BEHIND the
        // start-bit store, into `emit_zgc_inline_announce` (inline
        // arm) and in front of the helper call (helper arm), so a peer frozen
        // between the two never owns an object that is neither inside its
        // published tail nor registered.
        let start_bit_first = post_init == TlabPostInit::ZgcAnnounce
            && inline_announce
            && zgc_start_bit_before_commit_enabled();
        let box_first = box_announce && zgc_start_bit_before_commit_enabled();
        // nio21 wave 12 L5 (`CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT`): the
        // helper arm calls `zgc_note_tlab_object` uncommitted too, and commits
        // after it from the reloaded thread slot -- so only where that slot is
        // the thread's home (a method that caches it; otherwise the wave-10
        // helper arm, commit then call).
        let helper_first = post_init == TlabPostInit::ZgcAnnounce
            && self.jit_thread_slot_off != 0
            && zgc_helper_before_commit_enabled();
        if !start_bit_first && !box_first && !helper_first {
            self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);
        }

        if skip_post_init_helper && box_first {
            // nio21 wave 10 L1: the decline form's ZGC announce (a box),
            // BEFORE the commit, with RCX, RDX, R8 and R9 saved around it as
            // `runtime_lowering::emit_zgc_announce_or_rollback` saves them.
            // The inline arm commits `R11 + total_size` after its `or` and
            // leaves RAX = the object; a refusal has committed nothing and
            // declines with no cursor to put back.
            // PUSH RCX; PUSH RDX; PUSH R8; PUSH R9.
            self.buf.emit(&[0x51, 0x52, 0x41, 0x50, 0x41, 0x51]);
            let (done, to_helper) = self.emit_zgc_inline_announce_end_in_rax(
                cursor_off,
                end_off,
                ZgcAnnounceCommit::Size(total_size as i32), // Cast: <= 256, x86-64 disp32
            );
            for b in to_helper {
                self.patch_rel32_to_here(b);
            }
            // POP R9; POP R8; POP RDX; POP RCX.
            self.buf.emit(&[0x41, 0x59, 0x41, 0x58, 0x5A, 0x59]);
            let refused = self.emit_jmp_rel32_patch();
            if let Some(d) = decline.as_deref_mut() {
                d.push(refused);
            }
            self.patch_rel32_to_here(done);
            self.buf.emit(&[0x41, 0x59, 0x41, 0x58, 0x5A, 0x59]);
        } else if skip_post_init_helper {
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
            if box_announce {
                // `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT=0`: the al16
                // order -- committed above, announced or rolled back here.
                let rolled_back = crate::runtime_lowering::emit_zgc_announce_or_rollback(
                    &mut self.buf,
                    cursor_off,
                    end_off,
                );
                if let Some(d) = decline.as_deref_mut() {
                    d.extend(rolled_back);
                }
            }
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
            //
            // nio21 wave 10 L1: with `start_bit_first` the store is entered
            // UNCOMMITTED and commits `R11 + total_size` after its `or`; its
            // refusals commit here, before the call (the helper arm's order is
            // unchanged: commit, then register).
            //
            // nio21 wave 12 L5: with `helper_first` the helper arm calls
            // uncommitted and commits after the call (`emit_zgc_commit_after_helper`).
            let commit = if start_bit_first || helper_first {
                ZgcAnnounceCommit::Size(total_size as i32) // Cast: <= 256, x86-64 disp32
            } else {
                ZgcAnnounceCommit::AlreadyCommitted
            };
            let inline = inline_announce
                .then(|| self.emit_zgc_inline_announce_end_in_rax(cursor_off, end_off, commit));
            if let Some((_, to_helper)) = &inline {
                for &b in to_helper {
                    self.patch_rel32_to_here(b);
                }
            }
            if !helper_first {
                emit_zgc_announce_commit(&mut self.buf, cursor_off, commit);
            }
            cratonvm_jit_api::assert_helper_call_shape!(
                "zgc_note_tlab_object",
                int_args = 3,
                returns_value = true
            );
            self.emit_load_vm_context(ARG_REGS[0]);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], total_size as i32); // Cast: <= 256, x86-64 imm32
            self.emit_call_absolute(self.helpers.zgc_note_tlab_object);
            if helper_first {
                // R10 / R11 died in the call: the thread from its slot, the
                // object from the still-uncommitted cursor. RAX survives.
                self.emit_load_local(R10, self.jit_thread_slot_off);
                emit_zgc_commit_after_helper(&mut self.buf, cursor_off, commit);
            }
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
            self.emit_load_vm_context(ARG_REGS[0]);
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
        self.emit_load_vm_context(ARG_REGS[0]);
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
    /// # Reference arrays: ZGC only (zr z3/ja)
    ///
    /// `component_class_id: Some(id)` is an `anewarray` site
    /// (`element_type == Reference`), admitted only on ZGC
    /// (`jit_tlab_registration_required`) under
    /// [`inline_tlab_anewarray_enabled`]: the class word is `id`, the element
    /// size `narrow_oop::ref_element_size()` (a process constant fixed at VM
    /// init, before the first compile), and the slow path is
    /// `helpers.anewarray_object(vm, id, length)`. Generational and G1 never
    /// reach this arm with a reference element (the caller does not offer
    /// it), so their code is byte for byte what it was. A `newarray` site
    /// (`None`) with a reference atype is declined, as before.
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
        component_class_id: Option<u32>,
        count_slot: StackSlot,
    ) -> bool {
        use cratonvm_types::{ClassId, ObjectHeader, ObjectKind};
        let is_ref = element_type == cratonvm_types::ArrayElementType::Reference;

        // Name the refusal rather than returning a bare `false`. This arm is
        // default-ON and carries the same documented heap-corruption risk its
        // object-shaped sibling does, and the sibling's own history is that a
        // silent `false` is how a path comes to be held responsible for a
        // number it never produced: a census across all five collectors found
        // `emit_inline_tlab_new_ir` emitting ZERO sites in every one of them,
        // while its doc comment was being cited as the reason another feature
        // stayed shut.
        //
        // `zgc_announce`: the collector must be told about the array (ZGC), so
        // the fast path ends in the announce below instead of `MOV RAX, R11`.
        let zgc_announce = cratonvm_types::jit_tlab_registration_required();
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
        } else if zgc_announce && !zgc_inline_array_announce_enabled() {
            // A collector whose sweep is driven by an allocation-base registry
            // rather than by walking the chunk (ZGC) has to be told about every
            // object, and an unannounced object is not an object to
            // `is_object_address`: its first use as a receiver decodes as
            // `null`. Round 16 wave 7 (lane strings): the array arm announces
            // through the `new` arm's thin helper, `zgc_note_tlab_object`,
            // behind `CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE` (default OFF).
            Some("tlab registration required")
        } else if zgc_announce && self.helpers.zgc_note_tlab_object == 0 {
            // Nothing to announce the array with (a hand-built helper table).
            Some("zgc announce helper unwired")
        } else if !self.needs_heap {
            // `vm_ptr` in the heap slot is what the slow path needs.
            Some("no heap slot")
        } else if !is_ref && self.helpers.newarray == 0 {
            Some("helpers.newarray is null")
        } else if self.helpers.get_current_thread == 0 && self.jit_thread_slot_off == 0 {
            // …and a thread is what the fast path needs.
            Some("no thread source")
        } else if is_ref && (component_class_id.is_none() || !zgc_announce) {
            // A `newarray` site cannot name a reference atype, and a reference
            // array bumps inline on ZGC only (zr z3/ja; see the doc comment).
            Some("reference element")
        } else if is_ref && !inline_tlab_anewarray_enabled() {
            Some("CRATONVM_JIT_INLINE_TLAB_ANEWARRAY=0")
        } else if is_ref && self.helpers.anewarray_object == 0 {
            Some("helpers.anewarray_object is null")
        } else {
            None
        };
        if let Some(why) = declined {
            self.note_inline_array_site_of(is_ref, Some(why));
            return false;
        }
        // The class word: the component class for `anewarray` (what
        // `jit_anewarray_object` hands the allocator), `0` for a primitive
        // array (what `jit_newarray` hands it).
        let class_word: u32 = if is_ref {
            component_class_id.unwrap_or(0)
        } else {
            0
        };

        let elem_size = cratonvm_types::element_byte_size(element_type);
        let log2_elem = match elem_size {
            1 => 0u8,
            2 => 1,
            4 => 2,
            8 => 3,
            // Unreachable for both families (a reference element is 4 or 8),
            // and a refusal rather than an assert: a new element width should
            // cost coverage, not a panic.
            _ => {
                self.note_inline_array_site_of(is_ref, Some("element width"));
                return false;
            }
        };
        // `array_data_size_checked` rounds the data area up to 8, so the count
        // cap is derived from the size cap — the rounding cannot then push a
        // just-admitted array past it.
        let max_len =
            (Self::INLINE_ARRAY_TLAB_MAX_ALLOC - cratonvm_types::ARRAY_DATA_OFFSET) / elem_size;
        let Ok(max_len) = i32::try_from(max_len) else {
            self.note_inline_array_site_of(is_ref, Some("cap does not fit imm32"));
            return false;
        };
        // Every header displacement below is encoded as a disp8.
        if cratonvm_types::IDENTITY_HASH_OFFSET + 4 > 127
            || cratonvm_types::ARRAY_LENGTH_OFFSET > 127
            || cratonvm_types::ARRAY_DATA_OFFSET > 127
        {
            self.note_inline_array_site_of(is_ref, Some("header offset past disp8"));
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
        // `class_id` is `ClassId::new(0)` for a primitive array because that
        // is what `jit_newarray` passes `tlab_alloc_array_guarded_refill`: the
        // element type lives in the mark word, not in a class. For a reference
        // array it is the component class, as `jit_anewarray_object` passes.
        // Cast: the class id's bits as an imm32.
        self.emit_mov_dword_mem_disp32_imm32(R11, 0, class_word as i32);
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
        let mark = ObjectHeader::new(
            ClassId::new(class_word),
            ObjectKind::Array,
            element_type,
            0,
            0,
        )
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
        //
        // nio21 wave 10 L1 (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`): with
        // the inline start-bit store armed, the commit moves behind it (see
        // the `new` arm); the cursor is re-derived from the header's length.
        let inline_store = zgc_announce && zgc_jit_inline_announce_requested();
        // nio21 wave 12 L5 (`CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT`): the
        // helper arm calls uncommitted too, then commits from the reloaded
        // thread slot (see the `new` arm).
        let helper_first =
            zgc_announce && self.jit_thread_slot_off != 0 && zgc_helper_before_commit_enabled();
        let commit = if (inline_store && zgc_start_bit_before_commit_enabled()) || helper_first {
            ZgcAnnounceCommit::ArrayFromHeader { log2_elem }
        } else {
            self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);
            ZgcAnnounceCommit::AlreadyCommitted
        };
        if zgc_announce {
            // Round 16 wave 7 (lane strings,
            // `CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE`): enter the array in
            // ZGC's start registry, exactly as the `new` arm's `ZgcAnnounce`
            // post-init does -- the inline start-bit store when the collector
            // published its table (R10 = thread, R11 = the array, as that
            // sequence requires), else / on any of its refusals
            // `zgc_note_tlab_object(vm, array, footprint) -> array | 0`. A `0`
            // is a pending exception, routed by the caller's
            // `emit_post_alloc_oom_check` like `jit_newarray`'s.
            let inline = inline_store
                .then(|| self.emit_zgc_inline_announce_end_in_rax(cursor_off, end_off, commit));
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
            if helper_first {
                // The footprint without a commit: the array's end, re-derived
                // from its header into RCX, minus R11. ARG2 first (on Win64
                // ARG0 is RCX itself).
                emit_zgc_object_end_into_rcx(&mut self.buf, commit);
                self.emit_mov_r64_r64(ARG_REGS[2], RCX);
                self.emit_sub_r64_r64(ARG_REGS[2], R11);
            } else {
                // The helper arm commits first, as before (nothing when the
                // commit already happened above).
                emit_zgc_announce_commit(&mut self.buf, cursor_off, commit);
                // The footprint is what the bump advanced the cursor by from
                // the array's own (aligned) address: the committed cursor
                // minus R11. Re-read from the TLAB because the inline arm
                // clobbered RAX. ARG2 first: R10/R11 are no argument register
                // in either ABI.
                self.emit_mov_r64_mem_disp32(ARG_REGS[2], R10, cursor_off);
                self.emit_sub_r64_r64(ARG_REGS[2], R11);
            }
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_load_vm_context(ARG_REGS[0]);
            self.emit_call_absolute(self.helpers.zgc_note_tlab_object);
            if helper_first {
                // R10 / R11 died in the call: the thread from its slot, the
                // array from the still-uncommitted cursor. RAX survives.
                self.emit_load_local(R10, self.jit_thread_slot_off);
                emit_zgc_commit_after_helper(&mut self.buf, cursor_off, commit);
            }
            // The inline arm rejoins with RAX = the array, where the helper's
            // answer lands.
            if let Some((inline_done, _)) = inline {
                self.patch_rel32_to_here(inline_done);
            }
        } else {
            // Both arms converge with RAX = the array pointer.
            self.emit_mov_r64_r64(RAX, R11);
        }
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        for patch in slow {
            self.patch_rel32_to_here(patch);
        }
        // `jit_newarray(vm, atype, length)` (or, for a reference array,
        // `jit_anewarray_object(vm, component_class_id, length)`). The caller
        // has already emitted the pre-safepoint spill, and emits the oop map
        // and the OOM check after the merge, so this edge needs only the
        // arguments.
        self.emit_load_vm_context(ARG_REGS[0]);
        if is_ref {
            self.emit_mov_imm32_sx(ARG_REGS[1], class_word as i32); // Cast: x86-64 immediate encoding
            self.load_slot_to_reg(ARG_REGS[2], count_slot);
            // (vm_ptr, component_class_id, length) -> array | 0.
            cratonvm_jit_api::assert_helper_call_shape!(
                "anewarray_object",
                int_args = 3,
                returns_value = true
            );
            self.emit_call_absolute(self.helpers.anewarray_object);
        } else {
            self.emit_mov_imm32_sx(ARG_REGS[1], atype);
            self.load_slot_to_reg(ARG_REGS[2], count_slot);
            // (vm_ptr, atype, length) -> array | 0.
            cratonvm_jit_api::assert_helper_call_shape!(
                "newarray",
                int_args = 3,
                returns_value = true
            );
            self.emit_call_absolute(self.helpers.newarray);
        }

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
        self.note_inline_array_site_of(is_ref, None);
        true
    }

    /// [`Self::note_inline_array_site`] for a site of either family: a
    /// reference-array (`anewarray`, ZGC only) site goes to its own census
    /// ([`note_inline_ref_array_site`]) so the `newarray` pair keeps meaning
    /// what it meant before zr z3/ja.
    fn note_inline_array_site_of(&self, is_ref: bool, why: Option<&'static str>) {
        if is_ref {
            note_inline_ref_array_site(why.is_none());
        } else {
            self.note_inline_array_site(why);
        }
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
        // Round 16 (lane fieldguard, FG-b3): the epoch as of the compile's
        // start, before its compact rows were resolved, not as of now
        // (`crate::compile_baked_layout_epoch`). Top-level and inlined-callee
        // sites both come here.
        let expected = crate::compile_baked_layout_epoch(expected);
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

        // ── zs s5/bt2: the fused gate (ZGC's age-floor plan) ─────────────
        // One 16-bit compare of the adjacent `pre_active` / `post_active`
        // bytes; both zero is sufficient for every arm below that reaches the
        // store without a barrier (`zgc_fused_ref_store_gate_of`). On that
        // answer a COMPACT receiver stores here and leaves; a set word or a
        // LEGACY receiver falls into the unchanged sequence below, which
        // re-reads everything it needs. The post gates of this tier sit AFTER
        // its store, so the fused arm carries its own copy of the compact
        // store rather than jumping into the shared one. Never with the inline
        // card arm (the generational mask plan, which the fused word already
        // excludes).
        let mut fused_done: Option<usize> = None;
        let fused_word = if self.inline_card_mark_available() {
            None
        } else {
            zgc_fused_ref_store_gate_sites_of(&self.helpers)
        };
        if let Some(word) = fused_word {
            self.emit_movzx_r32_mem8(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);
            self.emit_zgc_fused_gate_cmp(word);
            let busy = self.emit_jcc_rel32_patch(0x85); // JNE → the full sequence
            self.load_slot_to_reg(RDX, val_slot);
            self.emit_test_r8_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT);
            let legacy = self.emit_jcc_rel32_patch(0x84); // JZ → the full sequence
            self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
            self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_INLINE_TAKEN);
            fused_done = Some(self.emit_jmp_rel32_patch());
            self.patch_rel32_to_here(busy);
            self.patch_rel32_to_here(legacy);
        }

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
        // gcj j2/jt2: post-only (no PRE half, no lean POST), one reading. With
        // no PRE there is no call before the store, so CL still holds the
        // flags byte read above and is not re-read.
        let post_only = self.post_only_card_site(inline_card);
        // gci i23/lc: one reading for both halves. The COMPACT store keeps
        // R10; the LEGACY shape below writes it and zeroes it again under
        // `lean`, so its POST takes the full check.
        let lean = inline_card && !post_only && jit_lean_card_post_enabled();
        if post_only {
            self.note_post_only_card_site();
        } else if inline_card {
            self.emit_gen_card_barrier_pre(
                RAX,
                RDX,
                RAX,
                &[(RAX, obj_slot), (RDX, val_slot)],
                lean,
            );
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
        if lean {
            // gci i23/lc: R10 held the field index and then the tag above, not
            // PRE's card: tell the lean POST so (it runs the full check).
            self.emit_xor_reg_self(R10);
        }
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
            // survive the store, both shapes, the trace and both gates (and,
            // gci i23/lc, R10: the gates use RCX/R11 only).
            self.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], lean);
        } else {
            self.emit_load_vm_context(ARG_REGS[0]);
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
        self.emit_load_vm_context(ARG_REGS[0]);
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
        if let Some(d) = fused_done {
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
        assert_eq!(
            run_declining(&mut thread, 1),
            base as i64,
            "room: the object"
        );
        // The footprint is the `new` site's own (the ZGC test above pins its
        // value); read it back rather than restate it.
        let size = thread[0] - base;
        assert!(
            size >= 16 && size % 8 == 0,
            "room: the cursor advanced by {size}"
        );
        assert_eq!(heap[0] as u32, CID, "room: class id published");

        let mut full: [u64; 2] = [base, base + size - 8];
        assert_eq!(run_declining(&mut full, 1), DECLINED, "full TLAB declines");
        assert_eq!(full[0], base, "full TLAB: the cursor is untouched");

        let mut unused: [u64; 2] = [base, base + 512];
        assert_eq!(
            run_declining(&mut unused, 0),
            DECLINED,
            "null thread declines"
        );
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
    /// collector's (`cursor, end, start, zgc_owned_epoch, zgc_announce`,
    /// `zgc_owned_words`).
    #[repr(C)]
    #[allow(dead_code)] // read by the emitted code, not by Rust
    struct FakeThread {
        cursor: u64,
        end: u64,
        start: u64,
        owned_epoch: u64,
        announce: u64,
        owned_words: u64,
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

    /// zr z11/ia: `Tlab::zgc_owned_words` as the one u64 the fake thread
    /// carries (first index in the low dword), from the collector's own
    /// derivation.
    fn owned_words_word(base: u64, span: u64, start: u64, end: u64) -> u64 {
        let u = |v: u64| usize::try_from(v).unwrap();
        let [first, past] =
            cratonvm_gc::tlab::zgc_owned_word_window(u(base), u(span), u(start), u(end));
        u64::from(first) | (u64::from(past) << 32)
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
                owned_words: 0,
            };
            edit(&mut thread, &mut table, base);
            // zr z11/ia: the window `Tlab::new` derives from the edited
            // geometry (the long form's owned-word test reads it).
            thread.owned_words = owned_words_word(table.base, table.span, thread.start, thread.end);
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

// JIT round 17 wave 2, lane sbzgc: the CALL-FREE start-bit pair the inline
// `StringBuilder.toString()` uses under ZGC, executed on its own against a fake
// thread and table laid out as `r9w8_zgc_inline_announce_tests`' are. x86-64
// only, for the reason given above that module.
#[cfg(all(test, target_arch = "x86_64"))]
mod r17_sbzgc_call_free_start_bit_tests {
    use super::*;

    /// The fake `JvmThread`: its `Tlab` at offset 0
    /// (`cursor, end, start, zgc_owned_epoch, zgc_announce`).
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

    /// Emit the pre-check for two objects (R11, R9), then both stores, run it
    /// with R10 = `thread`, and return 1 on the store arm, 0 on a decline.
    fn run(thread: &mut FakeThread, a: u64, b: u64) -> i64 {
        run_counted(thread, a, b, false)
    }

    /// [`run`], with the pre-check's run-time decline counters armed or not
    /// (round 17 wave 6, lane sbzgc2).
    fn run_counted(thread: &mut FakeThread, a: u64, b: u64, count: bool) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        // R10 / R11 are no argument register in either ABI; R9 is written
        // last because it is Win64's fourth argument register.
        c.emit_mov_r64_r64(R10, ARG_REGS[0]);
        c.emit_mov_r64_r64(R11, ARG_REGS[1]);
        c.emit_mov_r64_r64(R9, ARG_REGS[2]);
        let mut decline: Vec<usize> = Vec::new();
        c.emit_zgc_start_bit_precheck(0, 8, &[R11, R9], &mut decline, count);
        c.emit_zgc_start_bit_store_prechecked(0, R11);
        c.emit_zgc_start_bit_store_prechecked(0, R9);
        c.emit_mov_imm32_sx(RAX, 1);
        c.buf.emit(&[0xC3]); // ret
        for p in decline {
            c.patch_rel32_to_here(p);
        }
        c.emit_xor_reg_self(RAX);
        c.buf.emit(&[0xC3]); // ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());

        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: a leaf snippet that touches no stack, reads the fake thread
        // and the fake table it names, writes only the bitmap they name (all
        // alive across the call), calls nothing, and returns an i64. `c` owns
        // the buffer and outlives the call.
        let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        // Cast: addresses as the i64 arguments the snippet takes.
        let r = f(thread as *mut FakeThread as i64, a as i64, b as i64);
        drop(c);
        r
    }

    /// One way to break (or keep) the contract: `(thread, table, heap base)`.
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
    fn first_word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
        t.end = base + 1000;
    }
    /// Only the SECOND object's word (1024..1536) leaves the chunk: the
    /// pre-check must ask about every object, not only the first.
    fn second_word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
        t.end = base + 1500;
    }

    /// Both objects' start bits are set with every condition met (same word
    /// and different words); each condition broken on its own -- including one
    /// that only the second object fails, and an off-grid second object --
    /// declines with the bitmap untouched.
    #[test]
    fn both_start_bits_are_set_or_the_site_declines_with_the_bitmap_untouched() {
        // a at heap offset 520: bit 65 = word 1, bit 1.
        const A: u64 = 520;
        let cases: [(&str, u64, Option<(usize, u64, usize, u64)>, Edit); 12] = [
            // b at 560: bit 70 = word 1, bit 6.
            ("armed, same word", 560, Some((1, 1 << 1, 1, 1 << 6)), keep),
            // b at 1104: bit 138 = word 2, bit 10.
            (
                "armed, next word",
                1104,
                Some((1, 1 << 1, 2, 1 << 10)),
                keep,
            ),
            ("unarmed buffer", 560, None, unarm),
            ("stale epoch", 560, None, stale_epoch),
            ("marking", 560, None, marking),
            ("unpublished", 560, None, unpublished),
            ("below base", 560, None, below_base),
            ("beyond span", 560, None, beyond_span),
            (
                "word starts before the chunk",
                560,
                None,
                word_starts_before_chunk,
            ),
            (
                "first word ends after the chunk",
                560,
                None,
                first_word_ends_after_chunk,
            ),
            (
                "second word ends after the chunk",
                1104,
                None,
                second_word_ends_after_chunk,
            ),
            ("second object off the grid", 1101, None, keep),
        ];
        for (name, b_off, expect, edit) in cases {
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
                cursor: base + A,
                end: base + 2048,
                start: base,
                owned_epoch: 41,
                announce: 1,
            };
            edit(&mut thread, &mut table, base);
            if thread.announce != 0 {
                thread.announce = &table as *const FakeTable as u64;
            }

            let r = run(&mut thread, base + A, base + b_off);

            match expect {
                Some((wa, ba, wb, bb)) => {
                    assert_eq!(r, 1, "{name}: the store arm");
                    let mut want = [0u64; 8];
                    want[wa] |= ba;
                    want[wb] |= bb;
                    assert_eq!(
                        bits.as_slice(),
                        &want[..],
                        "{name}: exactly both start bits"
                    );
                }
                None => {
                    assert_eq!(r, 0, "{name}: declined");
                    assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
                }
            }
            assert_eq!(
                thread.cursor,
                base + A,
                "{name}: the pair never moves the cursor"
            );
            drop(bits);
            drop(heap);
        }
    }

    /// Round 17 wave 6, lane sbzgc2: an uncounted decline edge is the plain
    /// `Jcc rel32` it always was; a counted one inverts the condition over the
    /// `MOV R8, imm64 ; LOCK INC [R8] ; JMP` stub.
    #[test]
    fn a_counted_decline_edge_inverts_over_its_stub_and_an_uncounted_one_is_unchanged() {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let start = c.buf.pos();
        let mut decline: Vec<usize> = Vec::new();
        c.emit_decline_jcc_counted(0x85, None, &mut decline);
        assert_eq!(c.buf.pos() - start, 6, "0F 85 rel32");
        assert_eq!(decline, vec![start + 2]);
        let counter = crate::metrics::SB_TO_STRING_CENSUS.get(crate::metrics::SB_TS_ENTERED);
        let at = c.buf.pos();
        c.emit_decline_jcc_counted(0x85, counter, &mut decline);
        // JE stay (6) + MOV R8,imm64 (10) + LOCK INC [R8] (4) + JMP rel32 (5).
        assert_eq!(c.buf.pos() - at, 25);
        assert_eq!(decline.len(), 2);
        assert_eq!(decline[1], at + 21, "the stub's JMP is the decline edge");
        // SAFETY: `at + 1` and `at + 16..at + 20` are inside the bytes just
        // emitted into the live buffer.
        let (cc, inc) = unsafe {
            let p = c.buf.as_ptr();
            (
                *p.add(at + 1),
                std::slice::from_raw_parts(p.add(at + 16), 4).to_vec(),
            )
        };
        assert_eq!(cc, 0x84, "the condition is inverted");
        assert_eq!(inc, vec![0xF0, 0x49, 0xFF, 0x00]);
        assert!(!c.buf.overflowed());
    }

    /// Round 17 wave 6, lane sbzgc2 (SBZGC17-5): with the counters armed the
    /// pre-check answers exactly as before, and each decline lands in the
    /// census row of its own cause. The rows are process-wide, so the test
    /// asserts that its row grew, never an exact value.
    #[test]
    fn counted_declines_keep_the_answer_and_land_in_their_cause_row() {
        use crate::metrics as m;
        const A: u64 = 520;
        let cases: [(&str, u64, Option<usize>, Edit); 11] = [
            ("armed, same word", 560, None, keep),
            ("unarmed buffer", 560, Some(m::SB_TS_ZGC_NO_ANNOUNCE), unarm),
            ("stale epoch", 560, Some(m::SB_TS_ZGC_EPOCH), stale_epoch),
            ("marking", 560, Some(m::SB_TS_ZGC_BLOCKED), marking),
            (
                "unpublished",
                560,
                Some(m::SB_TS_ZGC_NO_BITMAP),
                unpublished,
            ),
            (
                "below base",
                560,
                Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
                below_base,
            ),
            (
                "beyond span",
                560,
                Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
                beyond_span,
            ),
            (
                "word starts before the chunk",
                560,
                Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
                word_starts_before_chunk,
            ),
            (
                "second word ends after the chunk",
                1104,
                Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
                second_word_ends_after_chunk,
            ),
            (
                "second object off the grid",
                1101,
                Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
                keep,
            ),
            (
                "first word ends after the chunk",
                560,
                Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
                first_word_ends_after_chunk,
            ),
        ];
        let read = |r: usize| {
            m::SB_TO_STRING_CENSUS
                .get(r)
                .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
        };
        for (name, b_off, cause, edit) in cases {
            let mut heap = vec![0u64; 256];
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
                cursor: base + A,
                end: base + 2048,
                start: base,
                owned_epoch: 41,
                announce: 1,
            };
            edit(&mut thread, &mut table, base);
            if thread.announce != 0 {
                thread.announce = &table as *const FakeTable as u64;
            }
            let before = cause.map(read);
            let r = run_counted(&mut thread, base + A, base + b_off, true);
            match cause {
                None => {
                    assert_eq!(r, 1, "{name}: the store arm");
                    assert!(bits.iter().any(|w| *w != 0), "{name}: the bits are set");
                }
                Some(row) => {
                    assert_eq!(r, 0, "{name}: declined");
                    assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
                    assert!(
                        read(row) > before.unwrap_or(0),
                        "{name}: the decline is counted in row {}",
                        m::SB_TO_STRING_CENSUS_NAMES[row]
                    );
                }
            }
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
            CARD_VIEW_TRACE_SP_DIRTY as usize,
            JitCardView::TRACE_SP_DIRTY_OFFSET
        );
        assert_eq!(
            CARD_VIEW_TRACE_SP_CLEAN as usize,
            JitCardView::TRACE_SP_CLEAN_OFFSET
        );
        assert_eq!(
            card_view_trace_inc_bytes(CARD_VIEW_TRACE_SP_DIRTY),
            [0xF0, 0x49, 0xFF, 0x43, 40]
        );
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
        // gcf f2/p: the card-store form's words.
        assert_eq!(
            CARD_VIEW_SCAN_BOUND as usize,
            JitCardView::SCAN_BOUND_OFFSET
        );
        assert_eq!(
            CARD_VIEW_SUMMARY_NEG as usize,
            JitCardView::SUMMARY_NEG_OFFSET
        );
        assert_eq!(CARD_VIEW_FLAG_SUMMARY, JitCardView::FLAG_SUMMARY);
        // gci i23/lc: the skip-edge and lean-POST counters.
        assert_eq!(
            CARD_VIEW_SKIP_YOUNG as usize,
            JitCardView::SKIP_TRACE_YOUNG_OFFSET
        );
        assert_eq!(
            CARD_VIEW_SKIP_NULL as usize,
            JitCardView::SKIP_TRACE_NULL_OFFSET
        );
        assert_eq!(
            CARD_VIEW_SKIP_OLD_VALUE as usize,
            JitCardView::SKIP_TRACE_OLD_VALUE_OFFSET
        );
        assert_eq!(
            CARD_VIEW_SKIP_OUTSIDE as usize,
            JitCardView::SKIP_TRACE_OUTSIDE_OFFSET
        );
        assert_eq!(
            CARD_VIEW_LEAN_POST as usize,
            JitCardView::LEAN_POST_TRACE_OFFSET
        );
        // gcj j2/jt2: the shift-first card word, the post-only site count and
        // the byte-map-base flag.
        assert_eq!(
            CARD_VIEW_CARD_BASE_NEG as usize,
            JitCardView::CARD_BASE_NEG_OFFSET
        );
        assert_eq!(
            CARD_VIEW_POST_ONLY_SITES,
            JitCardView::POST_ONLY_SITES_OFFSET
        );
        assert_eq!(
            CARD_VIEW_FLAG_BYTE_MAP_BASE,
            JitCardView::FLAG_BYTE_MAP_BASE
        );
        assert_eq!(
            1usize << CARD_SUMMARY_SHIFT,
            cratonvm_gc::card_table::SUMMARY_GROUP,
            "the summary index is the card index shifted by log2(SUMMARY_GROUP)"
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
        run_check_traced(view, element_mark, holder, val, mark, false)
    }

    /// [`run_check`] with the gcf f1/p engagement trace emitted or not.
    fn run_check_traced(
        view: usize,
        element_mark: bool,
        holder: usize,
        val: usize,
        mark: usize,
        trace: bool,
    ) -> i64 {
        run_check_full(view, element_mark, holder, val, mark, trace, false)
    }

    /// [`run_check_traced`] with the gcf f2/p card-store form emitted or not.
    fn run_check_full(
        view: usize,
        element_mark: bool,
        holder: usize,
        val: usize,
        mark: usize,
        trace: bool,
        store: bool,
    ) -> i64 {
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
        let (slow, done) = c.emit_gen_card_check_full(RAX, RDX, mark_reg, trace, store);
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

    /// gcf f1/p: the traced check answers exactly as the untraced one and
    /// counts, in the view, one `sp_dirty` per spared call and one `sp_clean`
    /// per call the card byte decided — and nothing for an edge the card byte
    /// did not decide (young receiver, null, old -> old, outside the view).
    #[test]
    fn gcf_f1p_the_traced_card_check_counts_spared_and_made_calls_in_the_view() {
        let mut old = Old::new();
        let view = old.ct.jit_card_view_addr();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let young_holder = old.header(1024, 0);
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let old_value = old.base() + 8192;
        let run = |h: usize, v: usize| run_check_traced(view, false, h, v, h, true);

        assert_eq!(run(young_holder, y), 0, "young receiver");
        assert_eq!(run(holder, 0), 0, "null value");
        assert_eq!(run(holder, old_value), 0, "old -> old");
        assert_eq!(old.ct.jit_card_check_trace(), [0; 4], "no card byte read");
        assert_eq!(run(holder, y), 1, "clean card: call");
        assert_eq!(run(holder, y), 1, "still clean: the check stores nothing");
        assert_eq!(old.ct.jit_card_check_trace(), [0, 2, 0, 0]);
        old.ct.mark_dirty_lockfree(holder);
        assert_eq!(run(holder, y), 0, "dirty card: no call");
        assert_eq!(run(holder, y), 0);
        assert_eq!(run(holder, y), 0);
        assert_eq!(old.ct.jit_card_check_trace(), [3, 2, 0, 0]);
        // The untraced form of the same check counts nothing.
        assert_eq!(run_check(view, false, holder, y, holder), 0);
        assert_eq!(old.ct.jit_card_check_trace(), [3, 2, 0, 0]);
        drop(young);
    }

    /// gcf f4/p (`gcd-d1a` item 1): the checked context loader refuses the
    /// compile on an unreserved slot (offset 0 = `[rbp - 0]`, the saved RBP)
    /// with a named reason, and emits the raw load's bytes otherwise.
    #[test]
    fn gcf_f4p_an_unreserved_context_slot_refuses_the_compile() {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.heap_local_offset = 0;
        c.emit_load_vm_context(ARG_REGS[0]);
        assert_eq!(
            c.buf.codegen_failure_reason(),
            Some("vm-context-slot-unreserved")
        );

        let mut raw = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        raw.heap_local_offset = 16;
        let a0 = raw.buf.pos();
        raw.emit_load_local(ARG_REGS[0], 16);
        let a1 = raw.buf.pos();
        let mut checked = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        checked.heap_local_offset = 16;
        let b0 = checked.buf.pos();
        checked.emit_load_vm_context(ARG_REGS[0]);
        let b1 = checked.buf.pos();
        assert!(checked.buf.codegen_failure_reason().is_none());
        // SAFETY: both ranges are prefixes of live buffers just written.
        let (ra, rb) = unsafe {
            (
                std::slice::from_raw_parts(raw.buf.as_ptr().add(a0), a1 - a0),
                std::slice::from_raw_parts(checked.buf.as_ptr().add(b0), b1 - b0),
            )
        };
        assert_eq!(ra, rb, "a reserved slot loads exactly as before");
    }

    /// gcf f2/p — the card-store form, EXECUTED: a clean card under the
    /// view's scan-bound mirror is marked by the compiled code (no call: the
    /// done edge), with its summary group, so the collector's summary-guided
    /// scan finds it; above the mirror it is the call (the slow edge) and
    /// nothing is written; every edge the check decides before the card byte
    /// (young receiver, null, old -> old) still stores nothing.
    #[test]
    fn gcf_f2p_the_card_store_form_marks_a_clean_card_under_the_mirror() {
        let mut old = Old::new();
        let view = old.ct.jit_card_view_addr();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let young_holder = old.header(1024, 0);
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let old_value = old.base() + 8192;
        let element = holder + 16 + 600 * 8;
        let card = (element - old.base()) / CARD_SIZE;
        let store = |h: usize, v: usize, m: usize, elem: bool| {
            run_check_full(view, elem, h, v, m, false, true)
        };

        // Nothing dirtied yet: the mirror is 0, so a clean card calls.
        assert_eq!(
            old.ct
                .jit_card_view()
                .scan_bound
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert_eq!(store(holder, y, holder, false), 1, "above the mirror: call");
        assert!(
            old.ct.dirty_card_indices().is_empty(),
            "and nothing written"
        );
        assert_eq!(
            store(young_holder, y, young_holder, false),
            0,
            "young receiver"
        );
        assert_eq!(store(holder, 0, holder, false), 0, "null value");
        assert_eq!(store(holder, old_value, holder, false), 0, "old -> old");
        assert!(
            old.ct.dirty_card_indices().is_empty(),
            "none of those marks"
        );

        // A Rust mark of the last card raises the bound (and the mirror) over
        // the whole simulated generation.
        old.ct.mark_dirty_lockfree(old.end() - 8);
        old.ct.clear_all();
        assert_eq!(
            store(holder, y, element, true),
            0,
            "clean element card: marked inline"
        );
        assert!(old.ct.is_dirty(card), "the element's card");
        assert!(old.dirty(element));
        assert!(!old.dirty(holder), "not the header's");
        assert_eq!(
            store(holder, y, element, true),
            0,
            "now dirty: no call either"
        );
        assert_eq!(
            old.ct.take_dirty_cards(),
            vec![card],
            "the summary-guided scan finds it"
        );
        assert!(
            !old.ct.raw_card_address_escaped(),
            "the bound and summary stay armed"
        );
        drop(young);
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
            &[
                ("CRATONVM_JIT_INLINE_CARD_MARK", Some("1")),
                // gcg g6/jm: the card-store form is default ON; these tests
                // are about the check-only barrier.
                ("CRATONVM_JIT_CARD_STORE_BARRIER", Some("0")),
            ],
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

    /// gci i23/lc (`gcg-g1p-proposal-card-check-counts-its-skip-edges`): the
    /// traced check answers exactly as the untraced one and counts every SKIP
    /// edge in the view's `skip_trace` -- `[young, null, old->old, outside]`
    /// -- while the card-byte pair (`trace`) still counts only the card-byte
    /// decisions. The untraced form counts nothing.
    #[test]
    fn gci_i23lc_the_traced_check_counts_every_skip_edge() {
        let mut old = Old::new();
        let view = old.ct.jit_card_view_addr();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let young_holder = old.header(1024, 0);
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let old_value = old.base() + 8192;
        let run = |h: usize, v: usize, m: usize, elem: bool| run_check_traced(view, elem, h, v, m, true);

        assert_eq!(run(young_holder, y, young_holder, false), 0, "young receiver");
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 0, 0, 0]);
        assert_eq!(run(holder, 0, holder, false), 0, "null value");
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 1, 0, 0]);
        assert_eq!(run(holder, old_value, holder, false), 0, "old -> old");
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 1, 1, 0]);
        // Flagged old but outside the view: still the call, now counted.
        let mut stray = vec![0u64; 4];
        // SAFETY: the flags byte of the 32-byte buffer's header.
        unsafe {
            *((stray.as_mut_ptr() as usize + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *mut u8) =
                cratonvm_types::GC_FLAG_OLD_GEN;
        }
        let s = stray.as_ptr() as usize;
        assert_eq!(run(s, y, s, false), 1, "outside the view: call");
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 1, 1, 1]);
        assert_eq!(run(holder, y, old.end(), true), 1, "mark past the end: call");
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 1, 1, 2]);
        assert_eq!(
            old.ct.jit_card_check_trace(),
            [0; 4],
            "no skip edge reads the card byte"
        );
        // The card-byte edges count where they always did, not as skips.
        assert_eq!(run(holder, y, holder, false), 1, "clean card: call");
        old.ct.mark_dirty_lockfree(holder);
        assert_eq!(run(holder, y, holder, false), 0, "dirty card: no call");
        assert_eq!(old.ct.jit_card_check_trace(), [1, 1, 0, 0]);
        assert_eq!(old.ct.jit_card_skip_trace(), [1, 1, 1, 2]);
        // The card-store form, traced: the same stubs, the same answers.
        assert_eq!(
            run_check_full(view, false, young_holder, y, young_holder, true, true),
            0
        );
        assert_eq!(old.ct.jit_card_skip_trace(), [2, 1, 1, 2]);
        // Untraced: every answer, no count.
        assert_eq!(run_check(view, false, young_holder, y, young_holder), 0);
        assert_eq!(run_check(view, false, holder, 0, holder), 0);
        assert_eq!(run_check(view, false, s, y, s), 1);
        assert_eq!(old.ct.jit_card_skip_trace(), [2, 1, 1, 2]);
        assert_eq!(old.ct.jit_card_lean_post_trace(), 0);
        drop((young, stray));
    }

    /// A compiler that may emit the inline barrier over `old`, its slow arm
    /// calling `recording_barrier`. The caller holds the flag overrides.
    fn barrier_test_compiler(old: &Old) -> Compiler {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.helpers.jit_card_table_addr = old.ct.jit_card_view_addr();
        c.helpers.jit_card_old_base = old.base();
        c.helpers.jit_card_old_end = old.end();
        c.helpers.write_barrier = recording_barrier as *const () as usize; // Cast: fn → slot
        c.heap_local_offset = 8;
        c
    }

    /// gci i23/lc: with the site's `lean == false` the PRE / POST pair emits
    /// exactly the two `emit_gen_card_barrier` calls it replaced, byte for
    /// byte; `lean == true` changes PRE only by the trailing
    /// `xor r10d, r10d` (and its dirty edge's displacement past it).
    #[test]
    fn gci_i23lc_the_lean_switch_off_emits_the_old_barrier_bytes() {
        let old = Old::new();
        // Each emission gets its own buffer, so a `CALL rel32` to the helper
        // differs by where the buffer landed: zero every recorded helper-call
        // displacement before comparing (the 12-byte absolute form, when the
        // helper is out of reach, is position-independent already).
        let bytes_of = |emit: &dyn Fn(&mut Compiler)| -> Vec<u8> {
            let mut c = barrier_test_compiler(&old);
            let start = c.buf.pos();
            emit(&mut c);
            let end = c.buf.pos();
            assert!(c.buf.codegen_failure_reason().is_none());
            // SAFETY: `[start, end)` is the prefix of the live buffer just written.
            let mut v =
                unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(start), end - start) }
                    .to_vec();
            for &p in &c.helper_call_patches {
                if p >= start && p + 4 <= end {
                    v[p - start..p - start + 4].fill(0);
                }
            }
            v
        };
        let reload = [
            (RAX, StackSlot::Frame(16)),
            (RDX, StackSlot::Frame(24)),
        ];
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_INLINE_CARD_MARK", Some("1")),
                ("CRATONVM_JIT_CARD_STORE_BARRIER", Some("0")),
            ],
            || {
                let old_pre = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier(RAX, RDX, RAX, &reload)
                });
                let off_pre = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier_pre(RAX, RDX, RAX, &reload, false)
                });
                assert_eq!(off_pre, old_pre, "PRE, lean off");
                let old_post = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier(RAX, RDX, RAX, &[])
                });
                let off_post = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], false)
                });
                assert_eq!(off_post, old_post, "POST, lean off");
                let lean_pre = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier_pre(RAX, RDX, RAX, &reload, true)
                });
                assert_eq!(lean_pre.len(), old_pre.len() + 3);
                assert_eq!(&lean_pre[old_pre.len()..], &[0x45u8, 0x31, 0xD2][..], "xor r10d, r10d");
                let lean_post = bytes_of(&|c: &mut Compiler| {
                    c.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], true)
                });
                // test r10, r10 (3) ; jz rel32 (6) ; cmp byte [r10], 1 (4..5) ;
                // je rel32 (6) ahead of the unchanged full POST.
                assert!(lean_post.ends_with(&old_post), "the miss arm is the full POST");
                assert_eq!(&lean_post[..3], &[0x4Du8, 0x85, 0xD2][..], "test r10, r10");
            },
        );
    }

    /// The lean pair executed in a frame the way a store site uses it:
    /// `PRE(lean); [the simulated pause]; store; POST-head; full POST`, the
    /// POST head traced so `lean_post_trace` counts its one-compare answers.
    /// `clean_between` stores CARD_CLEAN through R10 between PRE and the
    /// store -- a pause that consumed PRE's card while this thread was frozen
    /// there (only valid when PRE took the dirty edge, i.e. R10 = &card).
    fn run_store_lean(
        old: &Old,
        rec: &mut Recorder,
        holder: usize,
        val: usize,
        mark_off: usize,
        clean_between: bool,
        store_form: bool,
    ) -> i64 {
        let body = || {
            let mut c = barrier_test_compiler(old);
            assert!(c.inline_card_mark_available());
            let entry_off = c.buf.pos();
            c.buf.emit(&[0x55]); // push rbp
            c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
            c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
            c.emit_store_local(8, ARG_REGS[0]); // vm (the Recorder)
            c.emit_store_local(16, ARG_REGS[1]); // holder
            c.emit_store_local(24, ARG_REGS[2]); // value
            let (obj_slot, val_slot) = (StackSlot::Frame(16), StackSlot::Frame(24));
            c.load_slot_to_reg(RAX, obj_slot);
            c.load_slot_to_reg(RDX, val_slot);
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
            c.emit_gen_card_barrier_pre(RAX, RDX, mark, &[(RAX, obj_slot), (RDX, val_slot)], true);
            if clean_between {
                c.emit_mov_byte_mem_disp32_imm8(R10, 0, cratonvm_gc::card_table::CARD_CLEAN);
            }
            let header_disp = i32::try_from(HEADER_SIZE).expect("a small header");
            c.emit_mov_mem_disp32_r64(RAX, RDX, header_disp);
            // POST, as the aastore site forms it: the head, then the mark,
            // then the full barrier as the miss arm.
            let hit = c.emit_gen_card_lean_post_head_with(true);
            let mark = emit_mark(&mut c);
            c.emit_gen_card_barrier(RAX, RDX, mark, &[(RAX, obj_slot), (RDX, val_slot)]);
            c.patch_rel32_to_here(hit);
            c.buf.emit(&[0xC9, 0xC3]); // leave; ret
            assert!(!c.buf.overflowed());
            assert!(c.buf.codegen_failure_reason().is_none());
            crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
                .expect("the test buffer must be flippable to RX");
            // SAFETY: `entry_off` is a byte offset inside the same allocation.
            let entry = unsafe { c.buf.as_ptr().add(entry_off) };
            // SAFETY: as `run_store`; the only extra store is the simulated
            // pause's CARD_CLEAN through R10, which the callers request only
            // when PRE's dirty edge left R10 = &card in the live card map.
            let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
            // Casts: test addresses as the i64 arguments of the C ABI.
            let r = f(rec as *mut Recorder as i64, holder as i64, val as i64);
            drop(c);
            r
        };
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_INLINE_CARD_MARK", Some("1")),
                (
                    "CRATONVM_JIT_CARD_STORE_BARRIER",
                    Some(if store_form { "1" } else { "0" }),
                ),
            ],
            body,
        )
    }

    /// gci i23/lc (`gcf-f1p-proposal-lean-inline-card-check` item 1),
    /// EXECUTED: the lean POST answers a still-dirty card with one compare
    /// (no call, `lean_post_trace` counts it), and every other case keeps the
    /// full POST's answer -- a clean card is still marked (by PRE's call), a
    /// card a pause cleaned between PRE and the store is marked by POST's
    /// call AFTER the store, a young receiver makes no call, an element mark
    /// lands on the element's card, and the card-store form marks inline.
    #[test]
    fn gci_i23lc_the_lean_post_keeps_every_card_and_spares_the_second_check() {
        let mut old = Old::new();
        let holder = old.header(64, cratonvm_types::GC_FLAG_OLD_GEN);
        let young_holder = old.header(2048, 0);
        let field = holder + HEADER_SIZE;
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let young2 = Box::new([0u64; 2]);
        let y2 = young2.as_ptr() as usize;
        let mut rec = Recorder {
            ct: &old.ct,
            calls: 0,
            last: (0, 0),
            field_at_first_call: u64::MAX,
            field,
        };
        let read_field = || {
            // SAFETY: the holder's first body word, a live 8-aligned slot.
            unsafe { std::ptr::read_volatile(field as *const u64) }
        };

        // Clean card: PRE's call marks it (before the store); PRE's join
        // zeroed R10, so POST runs the full check, finds it dirty, no call.
        let r = run_store_lean(&old, &mut rec, holder, y, 0, false, false);
        assert_eq!(r as usize, holder, "the slow arm reloaded the holder");
        assert_eq!(rec.calls, 1);
        assert_eq!(rec.field_at_first_call, 0, "PRE ran BEFORE the store");
        assert!(old.dirty(holder));
        assert_eq!(read_field(), y as u64);
        assert_eq!(old.ct.jit_card_lean_post_trace(), 0, "PRE took the slow edge");

        // Dirty card: PRE's dirty edge, POST's one compare. No call.
        let r = run_store_lean(&old, &mut rec, holder, y2, 0, false, false);
        assert_eq!(r as usize, holder);
        assert_eq!(rec.calls, 1);
        assert_eq!(read_field(), y2 as u64);
        assert_eq!(old.ct.jit_card_lean_post_trace(), 1, "the lean hit");

        // PRE saw the card dirty, a pause cleaned it before the store: POST's
        // compare reads clean, the full POST calls, AFTER the store.
        rec.calls = 0;
        rec.field_at_first_call = u64::MAX;
        let r = run_store_lean(&old, &mut rec, holder, y, 0, true, false);
        assert_eq!(r as usize, holder);
        assert_eq!(rec.calls, 1, "POST re-marked the card the pause consumed");
        assert_eq!(rec.field_at_first_call, y as u64, "the call followed the store");
        assert!(old.dirty(holder));
        assert_eq!(old.ct.jit_card_lean_post_trace(), 1, "a miss is not a hit");

        // Young receiver: no call on either half, the store lands.
        rec.calls = 0;
        rec.field = 0;
        run_store_lean(&old, &mut rec, young_holder, y, 0, false, false);
        assert_eq!(rec.calls, 0);
        // SAFETY: the young holder's first body word, inside the live buffer.
        assert_eq!(
            unsafe { std::ptr::read_volatile((young_holder + HEADER_SIZE) as *const u64) },
            y as u64
        );
        assert_eq!(old.ct.jit_card_lean_post_trace(), 1);

        // Element mark: its own card, then the lean hit on it.
        old.ct.clear_all();
        let element = holder + HEADER_SIZE + 600 * cratonvm_types::narrow_oop::ref_element_size();
        run_store_lean(&old, &mut rec, holder, y, HEADER_SIZE + 600 * 8, false, false);
        assert_eq!(rec.calls, 1);
        assert_eq!(rec.last.0 as usize, element & !7);
        assert!(old.dirty(element));
        assert!(!old.dirty(holder), "an element mark leaves the header card clean");
        run_store_lean(&old, &mut rec, holder, y, HEADER_SIZE + 600 * 8, false, false);
        assert_eq!(rec.calls, 1);
        assert_eq!(old.ct.jit_card_lean_post_trace(), 2);

        // The card-store form: raise the mirror over the generation, then a
        // clean card is marked inline by PRE (R10 zeroed: the full POST finds
        // it dirty), and the next store is a lean hit. No call at all.
        old.ct.mark_dirty_lockfree(old.end() - 8);
        old.ct.clear_all();
        rec.calls = 0;
        run_store_lean(&old, &mut rec, holder, y, 0, false, true);
        assert_eq!(rec.calls, 0, "marked inline");
        assert!(old.dirty(holder));
        assert_eq!(old.ct.jit_card_lean_post_trace(), 2);
        run_store_lean(&old, &mut rec, holder, y, 0, false, true);
        assert_eq!(rec.calls, 0);
        assert_eq!(old.ct.jit_card_lean_post_trace(), 3);
        assert!(!old.ct.raw_card_address_escaped());
        drop((young, young2));
    }

    /// gcj j2/jt2: [`run_store`] in the POST-ONLY shape every single-pass
    /// site emits under `CRATONVM_JIT_POST_ONLY_CARD_MARK=1` -- the store,
    /// then one full card check (no PRE half, `lean == false`) -- header mark,
    /// check-only form. Counts the site the way a site does.
    fn run_post_only_store(old: &Old, rec: &mut Recorder, holder: usize, val: usize) -> i64 {
        let view = old.ct.jit_card_view_addr();
        let body = || {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            c.helpers.jit_card_table_addr = view;
            c.helpers.jit_card_old_base = old.base();
            c.helpers.jit_card_old_end = old.end();
            c.helpers.write_barrier = recording_barrier as *const () as usize; // Cast: fn → slot
            let inline_card = c.inline_card_mark_available();
            assert!(inline_card, "the flag is on for this compile");
            assert!(
                c.post_only_card_site(inline_card),
                "the switch is on for this compile"
            );
            c.note_post_only_card_site();
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
            // The store itself: the holder's first body word. (Not the header
            // constant cast inline: the x64 header-offset tripwire counts that
            // spelling; this test store is not a site.)
            let header_disp = i32::try_from(HEADER_SIZE).expect("a small header");
            c.emit_mov_mem_disp32_r64(RAX, RDX, header_disp);
            c.emit_gen_card_barrier_post(
                RAX,
                RDX,
                RAX,
                &[(RAX, obj_slot), (RDX, val_slot)],
                false,
            );
            c.buf.emit(&[0xC9, 0xC3]); // leave; ret
            assert!(!c.buf.overflowed());
            assert!(c.buf.codegen_failure_reason().is_none());
            crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
                .expect("the test buffer must be flippable to RX");
            // SAFETY: `entry_off` is a byte offset inside the same allocation.
            let entry = unsafe { c.buf.as_ptr().add(entry_off) };
            // SAFETY: as `run_store`: the snippet builds and tears down its own
            // aligned frame, takes three i64s, stores only into the holder's
            // first body word (a live test buffer), calls only
            // `recording_barrier` with the live `Recorder`, and returns an i64.
            // `c` outlives the call.
            let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
            // Casts: test addresses as the i64 arguments of the C ABI.
            let r = f(rec as *mut Recorder as i64, holder as i64, val as i64);
            drop(c);
            r
        };
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_INLINE_CARD_MARK", Some("1")),
                ("CRATONVM_JIT_CARD_STORE_BARRIER", Some("0")),
                ("CRATONVM_JIT_POST_ONLY_CARD_MARK", Some("1")),
            ],
            body,
        )
    }

    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`), EXECUTED: the
    /// post-only shape calls the collector AFTER the store (the field already
    /// holds the value at the call), the card is dirty after it, a dirty card
    /// spares the next call, and the compile counted its site in the view.
    /// Then the composition the switch's soundness rests on: a pause that
    /// scans and clears the card while a peer is frozen between such a store
    /// and its card byte, and the take-over's completion over that peer's
    /// registers (the holder and the young value) at resume, which hands the
    /// card back dirty.
    #[test]
    fn gcj2jt2_a_post_only_store_marks_its_card_after_the_store() {
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
        assert_eq!(old.ct.jit_post_only_card_sites(), 0);
        let r = run_post_only_store(&old, &mut rec, holder, y);
        assert_eq!(r as usize, holder, "the slow arm reloaded the holder");
        assert_eq!(rec.calls, 1, "a clean card: one call");
        assert_eq!(
            rec.field_at_first_call, y as u64,
            "the store ran BEFORE the card check"
        );
        assert_eq!(rec.last, (holder as i64, y as i64));
        assert!(old.dirty(holder));
        assert_eq!(old.ct.jit_post_only_card_sites(), 1, "the site was counted");
        let r = run_post_only_store(&old, &mut rec, holder, y);
        assert_eq!(r as usize, holder);
        assert_eq!(rec.calls, 1, "a dirty card: no call");

        old.ct.clear_all();
        assert!(!old.dirty(holder), "the pause cleared the card");
        assert_eq!(
            old.ct.complete_frozen_register_marks(&[holder, y, 0]),
            1,
            "the holder is the one in-range register word"
        );
        assert!(old.dirty(holder), "the holder's card comes back dirty");
        drop(young);
    }

    /// gcj j2/jt2: `CRATONVM_JIT_POST_ONLY_CARD_MARK` is opt-in, and refused
    /// beside `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD` (whose scan trusts
    /// element cards over a dirty header card, which the register completion
    /// cannot always supply).
    #[test]
    fn gcj2jt2_the_post_only_switch_is_opt_in_and_refused_beside_trusted_element_cards() {
        let with = |edits: &[(&str, Option<&str>)]| {
            cratonvm_types::flags::with_thread_overrides(edits, jit_post_only_card_mark_enabled)
        };
        assert!(!with(&[("CRATONVM_JIT_POST_ONLY_CARD_MARK", None)]));
        assert!(with(&[
            ("CRATONVM_JIT_POST_ONLY_CARD_MARK", Some("1")),
            ("CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD", None),
        ]));
        assert!(!with(&[
            ("CRATONVM_JIT_POST_ONLY_CARD_MARK", Some("1")),
            ("CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD", Some("1")),
        ]));
    }

    /// gcj j2/jt2 (`CRATONVM_JIT_CARD_BYTE_MAP_BASE=1`), EXECUTED: on a
    /// card-aligned table the shift-first check separates every card case the
    /// classic one does (header and element marks, clean and dirty), and its
    /// bytes carry the `card_base_neg` subtract instead of the `cards_neg`
    /// one, and are shorter; off, the bytes are the classic ones.
    #[test]
    fn gcj2jt2_the_shift_first_check_answers_as_the_classic_one() {
        let buf = vec![0u64; 4096 + 64];
        let base = (buf.as_ptr() as usize + CARD_SIZE - 1) & !(CARD_SIZE - 1);
        let ct = CardTable::with_options(base, 4096 * 8, PRECISE);
        let view = ct.jit_card_view_addr();
        assert!(card_view_has_byte_map_base(view), "a card-aligned table");
        let holder = base + 64;
        // SAFETY: the flags byte of a header 64 bytes into the aligned span,
        // which lies inside `buf`.
        unsafe {
            *((holder + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *mut u8) =
                cratonvm_types::GC_FLAG_OLD_GEN
        };
        let element = holder + 16 + 600 * 8; // a different card than the header
        let young = Box::new([0u64; 2]);
        let y = young.as_ptr() as usize;
        let on = [("CRATONVM_JIT_CARD_BYTE_MAP_BASE", Some("1"))];
        let check = |element_mark: bool, mark: usize| -> i64 {
            cratonvm_types::flags::with_thread_overrides(&on, || {
                run_check(view, element_mark, holder, y, mark)
            })
        };
        assert_eq!(check(false, holder), 1, "clean header card: call");
        ct.mark_dirty_lockfree(holder);
        assert_eq!(check(false, holder), 0, "dirty header card: no call");
        assert_eq!(check(true, element), 1, "clean element card: call");
        ct.mark_dirty_lockfree(element);
        assert_eq!(check(true, element), 0, "dirty element card: no call");

        let bytes = |edits: &[(&str, Option<&str>)]| -> Vec<u8> {
            cratonvm_types::flags::with_thread_overrides(edits, || {
                let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
                c.helpers.jit_card_table_addr = view;
                let start = c.buf.pos();
                let _ = c.emit_gen_card_check_full(RAX, RDX, RAX, false, false);
                let end = c.buf.pos();
                // SAFETY: `[start, end)` is the prefix of the live buffer just
                // written.
                unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(start), end - start) }
                    .to_vec()
            })
        };
        let sub_r10_r11 = |disp: i32| -> Vec<u8> {
            let mut v = vec![0x4D, 0x2B, 0x93]; // SUB R10, [R11 + disp32]
            v.extend_from_slice(&disp.to_le_bytes());
            v
        };
        let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
        let classic = bytes(&[]);
        let shifted = bytes(&on);
        assert!(has(&classic, &sub_r10_r11(CARD_VIEW_CARDS_NEG)));
        assert!(!has(&classic, &sub_r10_r11(CARD_VIEW_CARD_BASE_NEG)));
        assert!(has(&shifted, &sub_r10_r11(CARD_VIEW_CARD_BASE_NEG)));
        assert!(!has(&shifted, &sub_r10_r11(CARD_VIEW_CARDS_NEG)));
        assert!(shifted.len() < classic.len(), "one instruction fewer");
        drop((young, buf));
    }
}

/// Round 16 lane alloc: the optimizing tier's copy of the inline ZGC
/// start-bit store is the single-pass sequence, byte for byte, so the executed
/// tests of `r9w8_zgc_inline_announce_tests` cover it too.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16_alloc_ir_zgc_announce_tests {
    use super::*;

    #[test]
    fn the_raw_announce_is_the_single_pass_sequence_byte_for_byte() {
        for (cursor_off, end_off) in [(0xF0i32, 0xF8i32), (0x1000, 0x1008), (8, 16)] {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let a0 = c.buf.pos();
            let (done_a, helper_a) = c.emit_zgc_inline_announce(
                cursor_off,
                end_off,
                ZgcAnnounceCommit::AlreadyCommitted,
            );
            let a = c.buf.as_slice()[a0..c.buf.pos()].to_vec();

            let mut d = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let b0 = d.buf.pos();
            let (done_b, helper_b) = emit_zgc_inline_announce_raw(&mut d.buf, cursor_off, end_off);
            let b = d.buf.as_slice()[b0..d.buf.pos()].to_vec();

            assert_eq!(a, b, "cursor_off={cursor_off:#x} end_off={end_off:#x}");
            assert_eq!(done_a - a0, done_b - b0, "the `done` patch offsets agree");
            let rel = |v: &[usize], base: usize| v.iter().map(|p| p - base).collect::<Vec<_>>();
            assert_eq!(
                rel(&helper_a, a0),
                rel(&helper_b, b0),
                "helper patches agree"
            );
        }
    }

    #[test]
    fn the_ir_announce_switch_is_default_on() {
        // Default ON since round 16; `=0` is the kill switch. The collector
        // half (`CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`) is pinned unset so this
        // reads only the IR switch's default.
        let _unset = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[
                ("CRATONVM_JIT_IR_ZGC_INLINE_ANNOUNCE", None),
                ("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE", None),
            ]),
        );
        assert!(ir_zgc_inline_announce_enabled());
        drop(_unset);
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[
                ("CRATONVM_JIT_IR_ZGC_INLINE_ANNOUNCE", Some("0")),
                ("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE", None),
            ]),
        );
        assert!(!ir_zgc_inline_announce_enabled());
    }

    /// Round 16 wave 7 (lane strings): the array arm, default ON since the w7d A/B
    /// (newBytes 27 -> 12 ns/op); `=0` is the kill switch.
    #[test]
    fn the_array_announce_switch_is_opt_in() {
        let _unset = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE",
                Some("0"),
            )]),
        );
        assert!(!zgc_inline_array_announce_enabled());
        drop(_unset);
        let _on = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE",
                Some("1"),
            )]),
        );
        assert!(zgc_inline_array_announce_enabled());
    }
}

/// nio21 wave 10 L1 (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`): where the
/// single-pass ZGC inline allocations put the start-bit store relative to the
/// TLAB cursor commit, read from the emitted bytes
/// (`docs/internal/fixed-bugs/r21-zgc-frozen-peer-in-flight-object-is-swept-CLOSED-20261012.md`).
/// The executed twins live in `runtime_lowering::nio21w10_start_bit_before_commit_tests`
/// and `r9w8_zgc_inline_announce_tests` (which runs with the default order).
#[cfg(all(test, target_arch = "x86_64"))]
mod nio21w10_start_bit_before_commit_tests {
    use super::*;

    const CURSOR_OFF: i32 = 0x40;
    const CID: u32 = 0x0765_4323;
    /// `OR [RDX + R8*8], R9`: the start-bit store.
    const OR_BIT: [u8; 4] = [0x4E, 0x09, 0x0C, 0xC2];

    /// `MOV [R10 + CURSOR_OFF], reg` for `reg` = RAX (`modrm` 0x82, the old
    /// commit) or RCX (0x8A, `emit_zgc_announce_commit`'s).
    fn commit_bytes(modrm: u8) -> Vec<u8> {
        let mut v = vec![0x49, 0x89, modrm];
        v.extend_from_slice(&CURSOR_OFF.to_le_bytes());
        v
    }

    fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
        hay.windows(needle.len())
            .enumerate()
            .filter(|(_, w)| *w == needle)
            .map(|(i, _)| i)
            .collect()
    }

    fn compiler() -> Compiler {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        // Never executed: any non-zero, in-reach address will do.
        let any = compiler as *const () as usize; // Cast: fn pointer to a helper address
        c.helpers.get_current_thread = any;
        c.helpers.tlab_post_init = any;
        c.helpers.new_object = any;
        c.helpers.newarray = any;
        c.helpers.zgc_note_tlab_object = any;
        c.helpers.tlab_cursor_offset_in_thread = CURSOR_OFF as usize; // Cast: a small offset
        c.helpers.tlab_end_offset_in_thread = CURSOR_OFF as usize + 8; // Cast: a small offset
        c.helpers.class_id_offset_in_obj = 0;
        c.heap_local_offset = 8;
        c.jit_thread_slot_off = 16;
        c
    }

    /// The inline `ZgcAnnounce` `new`'s bytes under `setting` of the switch.
    fn new_bytes(setting: Option<&str>) -> Vec<u8> {
        let _flag = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT",
                setting,
            )]),
        );
        let mut c = compiler();
        let at = c.buf.pos();
        c.emit_inline_tlab_new_with(CID, 2, TlabPostInit::ZgcAnnounce, true, None);
        c.buf.as_slice()[at..c.buf.pos()].to_vec()
    }

    #[test]
    fn the_inline_new_sets_the_start_bit_before_it_commits_the_cursor() {
        if !inline_tlab_new_enabled() {
            return; // opted out by the environment: the arm is a bare helper call
        }
        let b = new_bytes(None);
        let or_at = find_all(&b, &OR_BIT);
        assert_eq!(or_at.len(), 1, "one start-bit store");
        let header_at = find_all(&b, &CID.to_le_bytes());
        assert!(
            header_at.first().is_some_and(|h| *h < or_at[0]),
            "the class id is stored before the start bit: {header_at:?} vs {or_at:?}"
        );
        // zr z11/ia (`CRATONVM_JIT_ZGC_ANNOUNCE_COMMIT_FROM_RAX`, default ON):
        // the inline arm may commit the bump's end straight from RAX -- after
        // its `or`, like every other commit.
        let mut commits = find_all(&b, &commit_bytes(0x8A));
        commits.extend(find_all(&b, &commit_bytes(0x82)));
        assert_eq!(
            commits.len(),
            2,
            "the inline arm's commit and the helper arm's"
        );
        assert!(
            commits.iter().all(|c| *c > or_at[0]),
            "every commit follows the start-bit store: {commits:?} vs {or_at:?}"
        );
    }

    #[test]
    fn the_kill_switch_restores_commit_then_announce() {
        if !inline_tlab_new_enabled() {
            return;
        }
        let b = new_bytes(Some("0"));
        let or_at = find_all(&b, &OR_BIT);
        let commits = find_all(&b, &commit_bytes(0x82));
        assert_eq!((or_at.len(), commits.len()), (1, 1));
        assert!(
            commits[0] < or_at[0],
            "the round-9 order: commit, then the bit"
        );
        assert!(find_all(&b, &commit_bytes(0x8A)).is_empty());
    }

    /// What a decline edge of the box snippet returns.
    const DECLINED: i64 = -1;

    /// The single-pass DECLINE form with `inline_announce = true` -- the
    /// box's ZGC announce -- run against a fake thread block
    /// (`cursor, end, start, owned epoch, table`) whose address is the cached
    /// thread slot. Returns RAX, or `DECLINED` from every decline edge.
    fn run_box(thread: &mut [u64; 6]) -> i64 {
        let mut c = compiler();
        c.helpers.tlab_cursor_offset_in_thread = 0;
        c.helpers.tlab_end_offset_in_thread = 8;
        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55, 0x48, 0x89, 0xE5]); // push rbp; mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
        c.emit_store_local(16, ARG_REGS[0]);
        let mut decline: Vec<usize> = Vec::new();
        c.emit_inline_tlab_new_with(CID, 1, TlabPostInit::Skip, true, Some(&mut decline));
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
        // SAFETY: the snippet builds and tears down its own frame, reads the
        // thread block and the table it names, writes the TLAB, the cursor and
        // one bitmap word (all alive across the call), and calls nothing.
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f(thread.as_mut_ptr() as i64); // Cast: the thread block's address
        drop(c);
        r
    }

    #[repr(C, align(512))]
    struct Chunk([u64; 512]);

    /// The box answers alike under both orders: announced (its bit set, the
    /// cursor committed) on a valid table, declined (no bit, the cursor at
    /// the object) on every refusal.
    #[test]
    fn the_single_pass_box_announces_or_declines_alike_under_both_orders() {
        if !inline_tlab_new_enabled() {
            return;
        }
        for setting in [None, Some("0")] {
            let _flag = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT",
                    setting,
                )]),
            );
            for (refuse, blocked, epoch) in [(false, 0u64, 7u64), (true, 1, 7), (true, 0, 8)] {
                let chunk = Box::new(Chunk([0; 512]));
                let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
                let mut bitmap = [0u64; 8];
                // words, base, span, blocked, epoch.
                let tab = [bitmap.as_mut_ptr() as u64, base, 4096, blocked, epoch]; // Cast: an address
                                                                                    // zr z11/ia: the owned-word window last, words [0, 8).
                let tab_addr = tab.as_ptr() as u64; // Cast: an address
                let mut thread = [base + 64, base + 4096, base, 7, tab_addr, 8 << 32];
                let r = run_box(&mut thread);
                let what = format!("{setting:?} blocked={blocked} epoch={epoch}");
                if refuse {
                    assert_eq!(r, DECLINED, "{what}: declines");
                    assert_eq!(thread[0], base + 64, "{what}: the cursor at the object");
                    assert_eq!(bitmap[0], 0, "{what}: no start bit");
                } else {
                    assert_eq!(r, (base + 64) as i64, "{what}: RAX = the box"); // Cast: an address
                    assert!(thread[0] > base + 64, "{what}: committed");
                    assert_eq!(bitmap[0], 1 << 8, "{what}: its start bit");
                }
                drop(chunk);
            }
        }
    }

    /// What the stand-in announce helper saw (reached through the VM-pointer
    /// argument, so no process global): the thread's cursor AT the call.
    #[repr(C)]
    struct HelperProbe {
        thread: *const u64,
        cursor_at_call: u64,
        obj: i64,
        calls: u64,
        answer_zero: bool,
    }

    // SAFETY: extern "C" test stub; `vm` is the live `HelperProbe` the harness
    // passed as the VM pointer, whose `thread` points at the live fake thread.
    unsafe extern "C" fn probe_note(vm: i64, obj: i64, _size: i64) -> i64 {
        let p = &mut *(vm as *mut HelperProbe);
        p.cursor_at_call = *p.thread;
        p.obj = obj;
        p.calls += 1;
        if p.answer_zero {
            0
        } else {
            obj
        }
    }

    /// The inline `ZgcAnnounce` `new` against an UNARMED fake thread (so the
    /// helper arm runs), under `setting` of the wave-12 switch: `(RAX, cursor
    /// at the call, cursor after, the object)` as offsets from the TLAB.
    fn run_helper_arm(setting: Option<&str>, answer_zero: bool) -> (i64, u64, u64, u64) {
        let _flag = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT",
                setting,
            )]),
        );
        let chunk = Box::new(Chunk([0; 512]));
        let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
                                            // cursor (unaligned by 4: the bump aligns), end, start, epoch, no table.
        let mut thread = [base + 60, base + 4096, base, 7, 0u64, 0];
        let mut probe = HelperProbe {
            thread: thread.as_ptr(),
            cursor_at_call: 0,
            obj: 0,
            calls: 0,
            answer_zero,
        };
        let mut c = compiler();
        c.helpers.zgc_note_tlab_object = probe_note as *const () as usize; // Cast: a helper address
        c.helpers.tlab_cursor_offset_in_thread = 0;
        c.helpers.tlab_end_offset_in_thread = 8;
        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55, 0x48, 0x89, 0xE5]); // push rbp; mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
        c.emit_store_local(8, ARG_REGS[0]);
        c.emit_store_local(16, ARG_REGS[1]);
        c.emit_inline_tlab_new_with(CID, 2, TlabPostInit::ZgcAnnounce, true, None);
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: the snippet builds and tears down its own frame, reads and
        // writes the fake thread and the chunk (alive across the call), and
        // calls only `probe_note`, which writes `probe`.
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f(
            &mut probe as *mut HelperProbe as i64, // Cast: the VM pointer
            thread.as_mut_ptr() as i64,            // Cast: the thread block
        );
        drop(c);
        assert_eq!(probe.calls, 1, "the helper arm ran once");
        let off = |a: u64| a.wrapping_sub(base);
        let rax = if r == 0 { 0 } else { r - base as i64 }; // Cast: an address
        let obj = off(probe.obj as u64); // Cast: an address
        drop(chunk);
        (rax, off(probe.cursor_at_call), off(thread[0]), obj)
    }

    /// nio21 wave 12 L5: the helper arm calls `zgc_note_tlab_object` with the
    /// cursor still UNCOMMITTED and commits after it, re-deriving the object
    /// from that cursor; a `0` answer commits too. `=0` restores commit-then-
    /// call.
    #[test]
    fn the_helper_arm_registers_before_it_commits() {
        if !inline_tlab_new_enabled() {
            return;
        }
        let (rax, at_call, after, obj) = run_helper_arm(None, false);
        assert_eq!(obj, 64, "the bump aligned 60 up to 64");
        assert_eq!(at_call, 60, "the helper ran with the cursor uncommitted");
        assert!(
            after > 64 && after % 8 == 0,
            "committed after the call: {after}"
        );
        assert_eq!(rax, 64, "RAX = the helper's answer");
        let size = after - 64;

        let (rax, at_call, after, _) = run_helper_arm(None, true);
        assert_eq!((rax, at_call), (0, 60), "a 0 answer is routed as before");
        assert_eq!(after, 64 + size, "and the object is committed all the same");

        let (rax, at_call, after, _) = run_helper_arm(Some("0"), false);
        assert_eq!(rax, 64);
        assert_eq!(
            at_call,
            64 + size,
            "the wave-10 helper arm: commit, then call"
        );
        assert_eq!(after, 64 + size);
    }

    #[test]
    fn the_helper_switch_is_default_on_and_follows_the_start_bit_switch() {
        let on = |edits: &[(&str, Option<&str>)]| {
            let _g = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(edits),
            );
            zgc_helper_before_commit_enabled()
        };
        assert!(on(&[
            ("CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT", None),
            ("CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT", None),
        ]));
        assert!(!on(&[
            ("CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT", Some("0")),
            ("CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT", None),
        ]));
        assert!(!on(&[
            ("CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT", None),
            ("CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT", Some("0")),
        ]));
    }

    #[test]
    fn the_committing_store_matches_its_raw_twin_byte_for_byte() {
        for commit in [
            ZgcAnnounceCommit::AlreadyCommitted,
            ZgcAnnounceCommit::Size(40),
            ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 0 },
            ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 3 },
        ] {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let a0 = c.buf.pos();
            let (done_a, help_a) = c.emit_zgc_inline_announce(0x40, 0x48, commit);
            let a = c.buf.as_slice()[a0..c.buf.pos()].to_vec();
            let mut d = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let b0 = d.buf.pos();
            let (done_b, help_b) =
                emit_zgc_inline_announce_raw_committing(&mut d.buf, 0x40, 0x48, commit);
            let b = d.buf.as_slice()[b0..d.buf.pos()].to_vec();
            assert_eq!(a, b, "{commit:?}");
            assert_eq!(done_a - a0, done_b - b0, "{commit:?}");
            let rel = |v: &[usize], base: usize| v.iter().map(|p| p - base).collect::<Vec<_>>();
            assert_eq!(rel(&help_a, a0), rel(&help_b, b0), "{commit:?}");
            // The commit sits between the bit and the `MOV RAX, R11; JMP done`.
            let or_at = find_all(&a, &OR_BIT);
            let rcx_commit = find_all(&a, &commit_bytes(0x8A));
            match commit {
                ZgcAnnounceCommit::AlreadyCommitted => assert!(rcx_commit.is_empty()),
                _ => assert!(
                    rcx_commit.len() == 1 && rcx_commit[0] > or_at[0],
                    "{commit:?}: {rcx_commit:?} vs {or_at:?}"
                ),
            }
        }
    }
}

/// gcf f4/p (`gcd-d1a` item 1): the single-pass backend's raw VM-context
/// loads (a local load of the context slot offset), which read `[rbp - 0]`
/// -- the saved caller RBP -- when the method reserved no context slot, may only
/// shrink (the pattern is spelled in pieces below so this module does not count itself). New sites go through `Compiler::emit_load_vm_context`, which refuses
/// the compile instead. Counted per line over the files that held one on
/// 2026-10-01; lower `BASELINE` when you convert a site.
#[cfg(test)]
mod gcf_f4p_vm_context_load_tests {
    // gch h2/fj (d1a item 1): every site converted; the one left is the
    // loader's own load in `emit_load_vm_context`.
    const BASELINE: usize = 1;

    fn raw_loads(src: &str) -> usize {
        src.lines()
            .filter(|l| {
                l.contains(concat!("emit_load_", "local("))
                    && l.contains(concat!("self.heap_", "local_offset"))
            })
            .count()
    }

    #[test]
    fn gcf_f4p_raw_vm_context_loads_do_not_grow() {
        let n = raw_loads(include_str!("deopt_stubs.rs"))
            + raw_loads(include_str!("frames.rs"))
            + raw_loads(include_str!("inlining.rs"))
            + raw_loads(include_str!("objects.rs"))
            + raw_loads(include_str!("op_array.rs"))
            + raw_loads(include_str!("op_field.rs"))
            + raw_loads(include_str!("op_invoke.rs"))
            + raw_loads(include_str!("op_object.rs"));
        assert!(
            n <= BASELINE,
            "{n} raw VM-context loads (baseline {BASELINE}): \
             load the VM context through `emit_load_vm_context`, which refuses an unreserved slot"
        );
    }
}

// zr z9/bo: the SHORT inline start-bit store (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`),
// EXECUTED: against hand-built tables over random allocation patterns (every
// path, inline or helper, ends with exactly the bit `HeapBitmap::insert` sets,
// and the short and long forms decide identically wherever both may run),
// and against a real `ZgcRealHeap` with a peer thread querying the oracles
// while the owner allocates through the short form. x86-64 only, as the
// modules above.
#[cfg(all(test, target_arch = "x86_64"))]
mod zr9bo_short_announce_tests {
    use super::*;

    #[test]
    fn the_short_form_offsets_match_the_collector_s() {
        use cratonvm_gc::tlab::JitZgcAnnounceTable as T;
        assert_eq!(ZGC_ANNOUNCE_SHORT_BLOCKED as usize, T::SHORT_BLOCKED_OFFSET);
        assert_eq!(ZGC_ANNOUNCE_GRANULE_BIAS as usize, T::GRANULE_BIAS_OFFSET);
    }

    /// A tiny deterministic generator (no `rand` dependency in this crate).
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// `extern "C" fn(thread, obj) -> u64` around ONE form of the inline
    /// store: R10 = `thread`, R11 = `obj`; returns `obj` on the inline arm and
    /// 0 on every helper exit.
    struct Snippet {
        c: Compiler,
        entry_off: usize,
    }

    impl Snippet {
        fn announce(short: bool, commit: ZgcAnnounceCommit) -> Self {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let entry_off = c.buf.pos();
            c.emit_mov_r64_r64(R10, ARG_REGS[0]);
            c.emit_mov_r64_r64(R11, ARG_REGS[1]);
            let (done, to_helper) =
                emit_zgc_inline_announce_raw_form(&mut c.buf, 0, 8, commit, short);
            for p in to_helper {
                c.patch_rel32_to_here(p);
            }
            c.emit_xor_reg_self(RAX);
            c.buf.emit(&[0xC3]); // ret
            c.patch_rel32_to_here(done);
            c.buf.emit(&[0xC3]); // ret (RAX = the object)
            Self::finish(c, entry_off)
        }

        /// The call-free pair for two objects (R11 = `a`, R9 = `b`): returns 1
        /// on the store arm, 0 on a decline.
        fn pair(short: bool, count: bool) -> Self {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let entry_off = c.buf.pos();
            c.emit_mov_r64_r64(R10, ARG_REGS[0]);
            c.emit_mov_r64_r64(R11, ARG_REGS[1]);
            c.emit_mov_r64_r64(R9, ARG_REGS[2]);
            let mut decline: Vec<usize> = Vec::new();
            c.emit_zgc_start_bit_precheck_form(0, 8, &[R11, R9], &mut decline, count, short);
            c.emit_zgc_start_bit_store_prechecked_form(0, R11, short);
            c.emit_zgc_start_bit_store_prechecked_form(0, R9, short);
            c.emit_mov_imm32_sx(RAX, 1);
            c.buf.emit(&[0xC3]); // ret
            for p in decline {
                c.patch_rel32_to_here(p);
            }
            c.emit_xor_reg_self(RAX);
            c.buf.emit(&[0xC3]); // ret
            Self::finish(c, entry_off)
        }

        fn finish(c: Compiler, entry_off: usize) -> Self {
            assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
            assert!(c.buf.codegen_failure_reason().is_none());
            crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
                .expect("the test buffer must be flippable to RX");
            Self { c, entry_off }
        }

        fn call3(&self, thread: u64, a: u64, b: u64) -> u64 {
            // SAFETY: `entry_off` is a byte offset inside the buffer `self.c`
            // owns, which outlives the call.
            let entry = unsafe { self.c.buf.as_ptr().add(self.entry_off) };
            // SAFETY: a leaf snippet that touches no stack, reads the thread
            // block and the table it names, writes at most one bitmap word per
            // object (and the cursor for a committing form), calls nothing,
            // and returns a u64. The caller keeps all of them alive.
            let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { std::mem::transmute(entry) };
            f(thread, a, b)
        }

        fn call(&self, thread: u64, obj: u64) -> u64 {
            self.call3(thread, obj, 0)
        }
    }

    /// Thread block (`cursor, end, start, owned_epoch, announce`) and table
    /// (`words, base, span, blocked, epoch, owner, short_blocked,
    /// granule_bias`), maintained as the collector maintains them.
    struct World {
        bits: Vec<u64>,
        table: Box<[u64; 8]>,
        thread: Box<[u64; 6]>,
    }

    const EPOCH: u64 = 41;
    const SPAN: u64 = 64 * 1024;

    /// zr z11/ia: `Tlab::zgc_owned_words` as the one u64 the fake thread
    /// carries (first index in the low dword), from the collector's own
    /// derivation.
    fn owned_words_word(base: u64, span: u64, start: u64, end: u64) -> u64 {
        let u = |v: u64| usize::try_from(v).unwrap();
        let [first, past] =
            cratonvm_gc::tlab::zgc_owned_word_window(u(base), u(span), u(start), u(end));
        u64::from(first) | (u64::from(past) << 32)
    }

    impl World {
        /// `base` is never dereferenced: the store reads only the thread and
        /// the table and writes only `bits`.
        fn new(base: u64, start: u64, end: u64) -> Self {
            let mut bits = vec![0u64; (SPAN / 512) as usize + 1];
            let words = bits.as_mut_ptr() as u64; // Cast: the bitmap's address
            let (bias, ok) = cratonvm_gc::tlab::JitZgcAnnounceTable::short_geometry(
                words as usize, // Cast: an address
                base as usize,  // Cast: an address
            );
            let table = Box::new([
                words,
                base,
                SPAN,
                0,
                EPOCH,
                base,
                u64::from(!ok),
                bias as u64, // Cast: an address-sized bias
            ]);
            let mut w = Self {
                bits,
                table,
                // zr z11/ia: the owned-word window last, as `Tlab::new` writes it.
                thread: Box::new([
                    start,
                    end,
                    start,
                    EPOCH,
                    0,
                    owned_words_word(base, SPAN, start, end),
                ]),
            };
            w.thread[4] = w.table.as_ptr() as u64; // Cast: the table's address
            w
        }

        /// `blocked`, raised or lowered as `set_jit_inline_announce_blocked`
        /// does (the short gate follows it, and stays closed on a misaligned
        /// base).
        fn set_blocked(&mut self, blocked: bool) {
            let base = self.table[1];
            self.table[3] = u64::from(blocked);
            self.table[6] = u64::from(blocked || base & 511 != 0 || self.table[0] == 0);
        }

        /// Withdrawn as `withdraw_jit_inline_announce` does.
        fn unpublish(&mut self) {
            self.table[6] = 1;
            self.table[0] = 0;
        }

        fn thread_addr(&mut self) -> u64 {
            self.thread.as_mut_ptr() as u64 // Cast: the thread block's address
        }

        /// The bit `HeapBitmap::insert` sets for `obj` (what the announce
        /// helper ends in).
        fn helper_insert(&mut self, obj: u64) {
            let bit = (obj - self.table[1]) >> 3;
            self.bits[(bit >> 6) as usize] |= 1u64 << (bit & 63); // Cast: a word index
        }
    }

    /// One random trial's configuration break (or none).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Break {
        None,
        Unarmed,
        StaleEpoch,
        Blocked,
        Unpublished,
    }

    fn apply(w: &mut World, b: Break) {
        match b {
            Break::None => {}
            Break::Unarmed => w.thread[4] = 0,
            Break::StaleEpoch => w.thread[3] = EPOCH - 1,
            Break::Blocked => w.set_blocked(true),
            Break::Unpublished => w.unpublish(),
        }
    }

    const BASE_ALIGNED: u64 = 0x5A5A_0000_0000;

    /// Random chunks, object sizes and alignments, every configuration break,
    /// aligned and misaligned bitmap bases: on every allocation the short and
    /// the long form either both set the bit inline or both refuse (the
    /// short one also refuses on a misaligned base), and after the helper
    /// serves each refusal both bitmaps hold exactly the bits the registry's
    /// own insert would have set for every object -- the oracle reads the same.
    #[test]
    fn the_short_and_long_forms_leave_the_registry_identical_over_random_patterns() {
        let long = Snippet::announce(false, ZgcAnnounceCommit::AlreadyCommitted);
        let short = Snippet::announce(true, ZgcAnnounceCommit::AlreadyCommitted);
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let (mut inline_hits, mut helper_hits, mut objects) = (0u64, 0u64, 0u64);
        for trial in 0..600 {
            let skew = if trial % 4 == 3 {
                [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
            } else {
                0
            };
            let base = BASE_ALIGNED + skew;
            let start = base + 8 * rng.below(2048);
            let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
            let brk = match rng.below(12) {
                0 => Break::Unarmed,
                1 => Break::StaleEpoch,
                2 => Break::Blocked,
                3 => Break::Unpublished,
                _ => Break::None,
            };
            let mut wl = World::new(base, start, end);
            let mut ws = World::new(base, start, end);
            let mut expect = World::new(base, start, end);
            apply(&mut wl, brk);
            apply(&mut ws, brk);
            let mut cur = start;
            loop {
                if rng.below(5) == 0 {
                    cur = (cur + 15) & !15;
                }
                let size = 16 + 8 * rng.below(32);
                if cur + size > end {
                    break;
                }
                let obj = cur;
                let tl = wl.thread_addr();
                let ts = ws.thread_addr();
                let rl = long.call(tl, obj);
                let rs = short.call(ts, obj);
                assert!(rl == 0 || rl == obj, "trial {trial}: long answers obj or 0");
                assert!(
                    rs == 0 || rs == obj,
                    "trial {trial}: short answers obj or 0"
                );
                if skew == 0 {
                    assert_eq!(
                        rl,
                        rs,
                        "trial {trial} {brk:?}: obj {:#x} in [{:#x}, {:#x})",
                        obj - base,
                        start - base,
                        end - base
                    );
                } else {
                    assert_eq!(
                        rs, 0,
                        "trial {trial}: a misaligned base keeps the short form shut"
                    );
                }
                if rl == 0 {
                    wl.helper_insert(obj);
                }
                if rs == 0 {
                    ws.helper_insert(obj);
                    helper_hits += 1;
                } else {
                    inline_hits += 1;
                }
                expect.helper_insert(obj);
                objects += 1;
                cur += size;
            }
            assert_eq!(
                wl.bits, expect.bits,
                "trial {trial} {brk:?}: long form vs insert"
            );
            assert_eq!(
                ws.bits, expect.bits,
                "trial {trial} {brk:?}: short form vs insert"
            );
        }
        eprintln!(
            "zr9bo random: objects={objects} short_inline={inline_hits} short_helper={helper_hits}"
        );
        // Positive controls: both arms of the short form ran.
        assert!(
            inline_hits > objects / 3,
            "the short form set most bits inline"
        );
        assert!(helper_hits > 0, "and refused some");
    }

    /// The committing form (`ZgcAnnounceCommit::Size`): the short inline arm
    /// sets the bit, then commits `obj + size`; a refusal commits nothing.
    /// Same as the long form.
    #[test]
    fn the_short_form_commits_after_its_bit_and_never_on_a_refusal() {
        for short in [false, true] {
            let snip = Snippet::announce(short, ZgcAnnounceCommit::Size(40));
            let base = BASE_ALIGNED;
            for (brk, inline) in [
                (Break::None, true),
                (Break::Blocked, false),
                (Break::StaleEpoch, false),
            ] {
                let mut w = World::new(base, base + 1024, base + 4096);
                apply(&mut w, brk);
                let obj = base + 1024 + 64;
                w.thread[0] = obj;
                let t = w.thread_addr();
                let r = snip.call(t, obj);
                if inline {
                    assert_eq!(r, obj, "short={short}: inline");
                    assert_eq!(w.thread[0], obj + 40, "short={short}: committed");
                    assert_eq!(
                        w.bits[((obj - base) >> 9) as usize],
                        1u64 << (((obj - base) >> 3) & 63)
                    ); // Cast: word index
                } else {
                    assert_eq!(r, 0, "short={short} {brk:?}: refused");
                    assert_eq!(w.thread[0], obj, "short={short} {brk:?}: nothing committed");
                    assert!(
                        w.bits.iter().all(|x| *x == 0),
                        "short={short} {brk:?}: no bit"
                    );
                }
            }
        }
    }

    /// The call-free pair (`toString()`): the short pre-check + stores
    /// accept and refuse exactly as the long pair on random object pairs, set
    /// the same two bits, and the counted short pre-check decides as the
    /// uncounted one.
    #[test]
    fn the_short_call_free_pair_decides_and_stores_as_the_long_pair() {
        let long = Snippet::pair(false, false);
        let short = Snippet::pair(true, false);
        let counted = Snippet::pair(true, true);
        let mut rng = XorShift(0xC0FF_EE00_1234_5678);
        let mut stored = 0u64;
        for trial in 0..2000 {
            let base = BASE_ALIGNED;
            let start = base + 8 * rng.below(1024);
            let end = (start + 8 * (4 + rng.below(1024))).min(base + SPAN);
            let a = start + 8 * rng.below((end - start) / 8);
            let b = a + 8 * (2 + rng.below(16));
            if b + 16 > end {
                continue;
            }
            let brk = match rng.below(10) {
                0 => Break::Blocked,
                1 => Break::StaleEpoch,
                2 => Break::Unpublished,
                _ => Break::None,
            };
            let mut ws = World::new(base, start, end);
            let mut wl = World::new(base, start, end);
            let mut wc = World::new(base, start, end);
            for w in [&mut ws, &mut wl, &mut wc] {
                apply(w, brk);
            }
            let (ts, tl, tc) = (ws.thread_addr(), wl.thread_addr(), wc.thread_addr());
            let rs = short.call3(ts, a, b);
            let rl = long.call3(tl, a, b);
            let rc = counted.call3(tc, a, b);
            assert_eq!(rs, rl, "trial {trial} {brk:?}");
            assert_eq!(rc, rs, "trial {trial}: counted == uncounted");
            assert_eq!(ws.bits, wl.bits, "trial {trial}: same bits");
            assert_eq!(wc.bits, ws.bits, "trial {trial}: same bits (counted)");
            if rs == 1 {
                let mut e = World::new(base, start, end);
                e.helper_insert(a);
                e.helper_insert(b);
                assert_eq!(ws.bits, e.bits, "trial {trial}: exactly a's and b's bits");
                stored += 1;
            } else {
                assert!(
                    ws.bits.iter().all(|x| *x == 0),
                    "trial {trial}: a decline writes nothing"
                );
            }
        }
        assert!(stored > 100, "the store arm ran ({stored})");
        // A misaligned base: the long pair may store, the short one declines.
        let base = BASE_ALIGNED + 8;
        let mut w = World::new(base, base + 1024, base + 8192);
        let t = w.thread_addr();
        assert_eq!(short.call3(t, base + 2048, base + 2096), 0);
        assert!(w.bits.iter().all(|x| *x == 0));
    }

    /// The single-pass and the shared (optimizing / concat) emitters agree
    /// byte for byte on the short forms; with the switch on, the single-pass
    /// `emit_zgc_inline_announce` IS the raw short form.
    #[test]
    fn the_short_forms_are_byte_identical_across_the_tiers() {
        let bytes = |c: &Compiler, from: usize| c.buf.as_slice()[from..].to_vec();
        // The pre-check and both stores.
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let c0 = c.buf.pos();
        let mut d = Vec::new();
        c.emit_zgc_start_bit_precheck_form(0x40, 0x48, &[R11, R9], &mut d, false, true);
        c.emit_zgc_start_bit_store_prechecked_form(0x40, R11, true);
        c.emit_zgc_start_bit_store_prechecked_form(0x40, R9, true);
        let mut b = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let b0 = b.buf.pos();
        let mut d2 = Vec::new();
        super::super::concat_emit::emit_zgc_start_bit_precheck_form(
            &mut b.buf,
            0x40,
            0x48,
            &[R11, R9],
            &mut d2,
            true,
        );
        super::super::concat_emit::emit_zgc_start_bit_store_prechecked_form(
            &mut b.buf, 0x40, R11, true,
        );
        super::super::concat_emit::emit_zgc_start_bit_store_prechecked_form(
            &mut b.buf, 0x40, R9, true,
        );
        assert_eq!(bytes(&c, c0), bytes(&b, b0));
        let rel = |v: &[usize], from: usize| v.iter().map(|p| p - from).collect::<Vec<_>>();
        assert_eq!(rel(&d, c0), rel(&d2, b0));
        // The inline store, through the env switch, against the raw form.
        let _flag = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_JIT_SHORT_ANNOUNCE",
                Some("1"),
            )]),
        );
        assert!(
            zgc_jit_short_announce_enabled(),
            "the override reaches the switch"
        );
        let mut s = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let s0 = s.buf.pos();
        let (done_a, help_a) = s.emit_zgc_inline_announce(0x40, 0x48, ZgcAnnounceCommit::Size(24));
        let mut r = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let r0 = r.buf.pos();
        let (done_b, help_b) =
            emit_zgc_inline_announce_short_raw(&mut r.buf, 0x40, 0x48, ZgcAnnounceCommit::Size(24));
        assert_eq!(bytes(&s, s0), bytes(&r, r0));
        assert_eq!(done_a - s0, done_b - r0);
        assert_eq!(rel(&help_a, s0), rel(&help_b, r0));
        assert_eq!(help_a.len(), 5, "five refusal edges");
    }

    /// Against the REAL collector: an owner thread allocates into a real
    /// VM-TLAB chunk of a `ZgcRealHeap` and announces every object through the
    /// emitted SHORT store (the helper -- `note_tlab_object` -- on a refusal),
    /// publishing every third base; a peer thread asks `is_object_address`,
    /// `is_heap_addr` and `resolve_interior_for_pin` about each published base
    /// and an interior word while the owner keeps bumping. Afterwards every
    /// word of the consumed span answers exactly as the bases it holds say.
    #[test]
    fn a_real_heap_answers_every_oracle_after_short_announces_across_threads() {
        use cratonvm_gc::zgc::ZgcRealHeap;
        use cratonvm_gc::{ArrayElementType, ObjectHeader, ObjectKind};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let edits = [
            ("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE", Some("1")),
            ("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1")),
        ];
        let heap = cratonvm_types::flags::with_thread_overrides(&edits, || {
            ZgcRealHeap::new_shared(32 * 1024 * 1024)
        });
        heap.set_vm_tlab_enabled(true);
        let Some(open) = heap.jit_short_announce_open() else {
            eprintln!("zr9bo real heap: another heap owns the announce table; skipped");
            return;
        };
        let published = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let owner = {
            let heap = Arc::clone(&heap);
            let published = Arc::clone(&published);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let snip = Snippet::announce(true, ZgcAnnounceCommit::AlreadyCommitted);
                let mut rng = XorShift(0x5151_7777);
                let (mut bases, mut inline, mut armed) = (Vec::new(), 0usize, 0usize);
                let mut spans = Vec::new();
                for _chunk in 0..16 {
                    let (ptr, size) = cratonvm_types::flags::with_thread_overrides(&edits, || {
                        heap.refill_tlab(512 * 1024)
                    })
                    .expect("a VM-TLAB chunk");
                    // SAFETY: freshly carved and zeroed for this buffer.
                    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
                    armed += usize::from(tlab.zgc_inline_announce_state().1 != 0);
                    let lo = ptr as usize; // Cast: an address
                    let tlab_addr = &mut tlab as *mut cratonvm_gc::Tlab as u64; // Cast: R10
                    loop {
                        let (header, total) = if rng.below(3) == 0 {
                            let len = rng.below(48) as u32; // Cast: a small length
                            let h = ObjectHeader::new(
                                cratonvm_types::ClassId::new(0),
                                ObjectKind::Array,
                                ArrayElementType::Byte,
                                len,
                                len,
                            );
                            (
                                h,
                                cratonvm_types::ARRAY_DATA_OFFSET + ((len as usize + 7) & !7),
                            ) // Cast: widening
                        } else {
                            let slots = rng.below(5) as u32; // Cast: a small count
                            let h = ObjectHeader::new(
                                cratonvm_types::ClassId::new(7),
                                ObjectKind::Object,
                                ArrayElementType::Reference,
                                0,
                                slots,
                            );
                            (
                                h,
                                cratonvm_types::HEADER_SIZE
                                    + slots as usize * cratonvm_types::SLOT_SIZE,
                            ) // Cast: widening
                        };
                        let align = if rng.below(5) == 0 { 16 } else { 8 };
                        let Some(p) = tlab.alloc(total, align) else {
                            break;
                        };
                        // SAFETY: `p` is this buffer's fresh, unpublished reservation.
                        unsafe { std::ptr::write(p as *mut ObjectHeader, header) };
                        // The emitted store reads the live `Tlab` through R10, as
                        // compiled code does (`start`, `end`, the epoch, the table).
                        let r = snip.call(tlab_addr, p as u64); // Cast: R11
                        if r == 0 {
                            heap.note_tlab_object(p, total);
                        } else {
                            inline += 1;
                        }
                        bases.push((p as usize, total)); // Cast: an address
                        if bases.len() % 3 == 0 {
                            published.store(p as usize, Ordering::Release); // Cast: an address
                        }
                    }
                    spans.push((lo, lo + tlab.consumed_bytes()));
                    std::mem::forget(tlab); // keep the span live for the checks below
                }
                done.store(true, Ordering::Release);
                (bases, inline, armed, spans)
            })
        };
        let mut checked = 0usize;
        while !done.load(Ordering::Acquire) || checked == 0 {
            let p = published.load(Ordering::Acquire);
            if p == 0 {
                std::hint::spin_loop();
                continue;
            }
            assert_eq!(
                heap.is_object_address(p).map(|o| o.as_ptr() as usize),
                Some(p)
            ); // Cast: an address
            assert_eq!(heap.is_heap_addr(p).map(|o| o.as_ptr() as usize), Some(p)); // Cast: an address
            assert!(
                heap.is_object_address(p + 8).is_none(),
                "an interior word is no base"
            );
            checked += 1;
        }
        let (bases, inline, armed, spans) = owner.join().unwrap();
        eprintln!(
            "zr9bo real heap: open={open} armed={armed} objects={} inline={inline} peer_checks={checked}",
            bases.len()
        );
        assert!(bases.len() > 4000, "the owner allocated ({})", bases.len());
        assert!(checked > 0, "the peer checked at least one base");
        if open && armed > 0 {
            // Positive control. The epoch is process-wide, so a collection in
            // a concurrent test can disarm the buffer: then the helper serves
            // the rest, which is equally correct.
            assert!(inline > 0, "the short store set bits inline");
        }
        let starts: std::collections::HashSet<usize> = bases.iter().map(|b| b.0).collect();
        for &(lo, top) in &spans {
            for a in (lo..top).step_by(8) {
                assert_eq!(
                    heap.is_object_address(a).is_some(),
                    starts.contains(&a),
                    "word {:#x}",
                    a - lo
                );
            }
        }
        for &(b, total) in bases.iter().step_by(7) {
            let inner = b + (total / 2 & !7);
            assert_eq!(
                heap.is_heap_addr(inner).map(|o| o.as_ptr() as usize),
                Some(b)
            ); // Cast: an address
            assert_eq!(
                heap.resolve_interior_for_pin(inner)
                    .map(|o| o.as_ptr() as usize),
                Some(b)
            ); // Cast: an address
        }
    }

    // ---- zr z11/ia: the trimmed forms ------------------------------------

    /// Which store a zr z11/ia snippet runs.
    #[derive(Clone, Copy, Debug)]
    enum Form {
        Long(LongAnnounceTrims),
        Short { rax_end: bool },
    }

    /// `extern "C" fn(thread, obj, end) -> u64` around one explicitly trimmed
    /// form: R10 = `thread`, R11 = `obj`, RAX = `end` (the bump's new cursor,
    /// what the optimizing tier leaves there); returns `obj` on the inline arm
    /// and 0 on every helper exit. No environment read.
    fn trimmed(form: Form, commit: ZgcAnnounceCommit) -> Snippet {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        c.emit_mov_r64_r64(R10, ARG_REGS[0]);
        c.emit_mov_r64_r64(R11, ARG_REGS[1]);
        c.emit_mov_r64_r64(RAX, ARG_REGS[2]);
        let (done, to_helper) = match form {
            Form::Long(t) => emit_zgc_inline_announce_long_raw_trimmed(&mut c.buf, 0, 8, commit, t),
            Form::Short { rax_end } => {
                emit_zgc_inline_announce_short_raw_trimmed(&mut c.buf, 0, 8, commit, rax_end)
            }
        };
        for p in to_helper {
            c.patch_rel32_to_here(p);
        }
        c.emit_xor_reg_self(RAX);
        c.buf.emit(&[0xC3]); // ret
        c.patch_rel32_to_here(done);
        c.buf.emit(&[0xC3]); // ret (RAX = the object)
        Snippet::finish(c, entry_off)
    }

    fn snippet_len(form: Form, commit: ZgcAnnounceCommit) -> usize {
        let mut buf = crate::ExecutableBuffer::new(1024).expect("buffer");
        match form {
            Form::Long(t) => emit_zgc_inline_announce_long_raw_trimmed(&mut buf, 0, 8, commit, t),
            Form::Short { rax_end } => {
                emit_zgc_inline_announce_short_raw_trimmed(&mut buf, 0, 8, commit, rax_end)
            }
        };
        buf.pos()
    }

    const ALL_LONG_TRIMS: LongAnnounceTrims = LongAnnounceTrims {
        owned_window: true,
        skip_grid: true,
        rax_end: true,
        skip_span: true,
        epoch_gate: false,
    };

    /// zr z11/ia: every trimmed form against its untrimmed twin over random
    /// chunks, sizes, configuration breaks and bitmap bases (aligned and
    /// skewed by multiples of 8): on every allocation both answer the same
    /// (`obj` or 0), commit the same cursor (`obj + size` on the inline arm,
    /// nothing on a refusal), and after the helper serves each refusal hold
    /// exactly the bits the registry's own insert sets.
    #[test]
    fn zr11ia_trimmed_forms_decide_and_commit_as_the_untrimmed_ones() {
        const SIZE: u64 = 40;
        let commit = ZgcAnnounceCommit::Size(40);
        let pairs: [(&str, Form, Form, ZgcAnnounceCommit); 6] = [
            (
                "long window+span skip only, size (zr s1/ca)",
                Form::Long(LongAnnounceTrims::NONE),
                Form::Long(LongAnnounceTrims {
                    owned_window: true,
                    skip_grid: false,
                    rax_end: false,
                    skip_span: true,
                    epoch_gate: false,
                }),
                commit,
            ),
            (
                "long window+grid, committed",
                Form::Long(LongAnnounceTrims::NONE),
                Form::Long(LongAnnounceTrims {
                    owned_window: true,
                    skip_grid: true,
                    rax_end: false,
                    skip_span: false,
                    epoch_gate: false,
                }),
                ZgcAnnounceCommit::AlreadyCommitted,
            ),
            (
                "long window only, size",
                Form::Long(LongAnnounceTrims::NONE),
                Form::Long(LongAnnounceTrims {
                    owned_window: true,
                    skip_grid: false,
                    rax_end: false,
                    skip_span: false,
                    epoch_gate: false,
                }),
                commit,
            ),
            (
                "long all trims, size",
                Form::Long(LongAnnounceTrims::NONE),
                Form::Long(ALL_LONG_TRIMS),
                commit,
            ),
            (
                "long rax_end without the window (inert)",
                Form::Long(LongAnnounceTrims::NONE),
                Form::Long(LongAnnounceTrims {
                    owned_window: false,
                    skip_grid: true,
                    rax_end: true,
                    skip_span: false,
                    epoch_gate: false,
                }),
                commit,
            ),
            (
                "short rax_end, size",
                Form::Short { rax_end: false },
                Form::Short { rax_end: true },
                commit,
            ),
        ];
        for (name, reference, candidate, commit) in pairs {
            let committing = commit != ZgcAnnounceCommit::AlreadyCommitted;
            let a = trimmed(reference, commit);
            let b = trimmed(candidate, commit);
            let mut rng = XorShift(0x2111_1A00_0000_0001);
            let (mut inline, mut refused) = (0u64, 0u64);
            for trial in 0..400 {
                let skew = if trial % 4 == 3 {
                    [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
                } else {
                    0
                };
                let base = BASE_ALIGNED + skew;
                let start = base + 8 * rng.below(2048);
                let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
                let brk = match rng.below(12) {
                    0 => Break::Unarmed,
                    1 => Break::StaleEpoch,
                    2 => Break::Blocked,
                    3 => Break::Unpublished,
                    _ => Break::None,
                };
                let mut wa = World::new(base, start, end);
                let mut wb = World::new(base, start, end);
                let mut expect = World::new(base, start, end);
                apply(&mut wa, brk);
                apply(&mut wb, brk);
                let mut cur = start;
                loop {
                    if rng.below(5) == 0 {
                        cur = (cur + 15) & !15;
                    }
                    let size = if committing {
                        SIZE
                    } else {
                        16 + 8 * rng.below(32)
                    };
                    if cur + size > end {
                        break;
                    }
                    let obj = cur;
                    wa.thread[0] = obj;
                    wb.thread[0] = obj;
                    let (ta, tb) = (wa.thread_addr(), wb.thread_addr());
                    let ra = a.call3(ta, obj, obj + size);
                    let rb = b.call3(tb, obj, obj + size);
                    assert!(ra == 0 || ra == obj, "{name} trial {trial}: obj or 0");
                    assert_eq!(
                        ra,
                        rb,
                        "{name} trial {trial} {brk:?}: obj {:#x} in [{:#x}, {:#x})",
                        obj - base,
                        start - base,
                        end - base
                    );
                    let committed = if committing && ra != 0 {
                        obj + size
                    } else {
                        obj
                    };
                    assert_eq!(
                        wa.thread[0], committed,
                        "{name} trial {trial}: reference cursor"
                    );
                    assert_eq!(wb.thread[0], committed, "{name} trial {trial}: cursor");
                    if ra == 0 {
                        wa.helper_insert(obj);
                        wb.helper_insert(obj);
                        refused += 1;
                    } else {
                        inline += 1;
                    }
                    expect.helper_insert(obj);
                    cur += size;
                }
                assert_eq!(
                    wa.bits, expect.bits,
                    "{name} trial {trial}: reference vs insert"
                );
                assert_eq!(
                    wb.bits, expect.bits,
                    "{name} trial {trial}: trimmed vs insert"
                );
            }
            eprintln!("zr11ia {name}: inline={inline} refused={refused}");
            // Positive controls: both arms ran.
            assert!(inline > 1000, "{name}: the inline arm ran ({inline})");
            assert!(refused > 100, "{name}: the helper arm ran ({refused})");
        }
    }

    /// zr z11/ia positive control: the trims emit fewer bytes, and with every
    /// trim off the long form is the round-9 sequence byte for byte
    /// (`emit_zgc_inline_announce_long_raw`, which the r16/nio21 byte tests
    /// pin against the single-pass copy).
    #[test]
    fn zr11ia_trims_shorten_the_sequence_and_none_is_the_old_bytes() {
        let size = ZgcAnnounceCommit::Size(40);
        let array = ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 3 };
        let none = Form::Long(LongAnnounceTrims::NONE);
        let window_grid = Form::Long(LongAnnounceTrims {
            owned_window: true,
            skip_grid: true,
            rax_end: false,
            skip_span: false,
            epoch_gate: false,
        });
        for commit in [size, array, ZgcAnnounceCommit::AlreadyCommitted] {
            let n = snippet_len(none, commit);
            let w = snippet_len(window_grid, commit);
            // Ten instructions of granule arithmetic -> two compares, the AND
            // and the grid test gone.
            assert!(n >= w + 20, "{commit:?}: {n} -> {w}");
        }
        let all = Form::Long(ALL_LONG_TRIMS);
        assert!(snippet_len(all, array) + 10 < snippet_len(window_grid, array));
        assert!(snippet_len(all, size) < snippet_len(window_grid, size));
        assert!(
            snippet_len(Form::Short { rax_end: true }, array)
                < snippet_len(Form::Short { rax_end: false }, array)
        );
        // `AlreadyCommitted` has no commit to move: rax_end changes nothing.
        let ac = ZgcAnnounceCommit::AlreadyCommitted;
        let all_but_rax = Form::Long(LongAnnounceTrims {
            rax_end: false,
            ..ALL_LONG_TRIMS
        });
        assert_eq!(snippet_len(all, ac), snippet_len(all_but_rax, ac));
        // zr s1/ca: the span skip takes the `jb`, the `cmp [span]` and the
        // `jae` (6 + 7 + 6 bytes) on every commit kind.
        for commit in [size, array, ac] {
            assert_eq!(
                snippet_len(window_grid, commit),
                snippet_len(
                    Form::Long(LongAnnounceTrims {
                        skip_span: true,
                        ..match window_grid {
                            Form::Long(t) => t,
                            Form::Short { .. } => unreachable!(),
                        }
                    }),
                    commit
                ) + 19,
                "{commit:?}"
            );
        }
        let mut a = crate::ExecutableBuffer::new(1024).expect("buffer");
        let mut b = crate::ExecutableBuffer::new(1024).expect("buffer");
        let ra = emit_zgc_inline_announce_long_raw(&mut a, 0x40, 0x48, size);
        let rb = emit_zgc_inline_announce_long_raw_trimmed(
            &mut b,
            0x40,
            0x48,
            size,
            LongAnnounceTrims::NONE,
        );
        assert_eq!(a.as_slice(), b.as_slice());
        assert_eq!(ra, rb);
    }

    /// zr z11/ia: the three switches are default ON and `=0` turns each off;
    /// the env-reading entry point honours them (the inline `new` / array arm
    /// emits the trimmed bytes by default, the round-9 ones with all three
    /// `=0`).
    #[test]
    fn zr11ia_switches_are_default_on_and_zero_kills_each() {
        let keys = [
            "CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW",
            "CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_GRID_TEST",
            "CRATONVM_JIT_ZGC_ANNOUNCE_COMMIT_FROM_RAX",
            // zr s1/ca.
            "CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST",
        ];
        let size = ZgcAnnounceCommit::Size(40);
        let env_bytes = || {
            let mut buf = crate::ExecutableBuffer::new(1024).expect("buffer");
            emit_zgc_inline_announce_raw_form_end(&mut buf, 0x40, 0x48, size, false, true);
            buf.as_slice().to_vec()
        };
        let explicit = |t: LongAnnounceTrims| {
            let mut buf = crate::ExecutableBuffer::new(1024).expect("buffer");
            emit_zgc_inline_announce_long_raw_trimmed(&mut buf, 0x40, 0x48, size, t);
            buf.as_slice().to_vec()
        };
        {
            let edits: Vec<(&str, Option<&str>)> = keys.iter().map(|k| (*k, None)).collect();
            let _unset = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&edits),
            );
            assert!(zgc_announce_owned_window_enabled());
            assert!(zgc_announce_skip_grid_test_enabled());
            assert!(zgc_announce_commit_from_rax_enabled());
            assert!(zgc_announce_skip_span_test_enabled());
            assert_eq!(env_bytes(), explicit(ALL_LONG_TRIMS));
        }
        {
            let edits: Vec<(&str, Option<&str>)> = keys.iter().map(|k| (*k, Some("0"))).collect();
            let _off = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&edits),
            );
            assert!(!zgc_announce_owned_window_enabled());
            assert!(!zgc_announce_skip_grid_test_enabled());
            assert!(!zgc_announce_commit_from_rax_enabled());
            assert!(!zgc_announce_skip_span_test_enabled());
            assert_eq!(env_bytes(), explicit(LongAnnounceTrims::NONE));
        }
        {
            // zr s1/ca: the span skip alone off keeps the other three.
            let edits = [("CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST", Some("0"))];
            let _off = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&edits),
            );
            assert_eq!(
                env_bytes(),
                explicit(LongAnnounceTrims {
                    skip_span: false,
                    ..ALL_LONG_TRIMS
                })
            );
        }
    }

    // ---- zs s2/ca2: the epoch gate ---------------------------------------

    const GATED_LONG_TRIMS: LongAnnounceTrims = LongAnnounceTrims {
        epoch_gate: true,
        ..ALL_LONG_TRIMS
    };

    /// zs s2/ca2: the gated long form against the full one, over random
    /// chunks, sizes, bases and configuration breaks, with the collector's
    /// gate invariant modelled on the gated world (a raise of `blocked` and a
    /// withdrawal bump the epoch; `World::set_blocked` / `unpublish` are the
    /// non-gated collector). Same answer, same cursor, same bits on every
    /// allocation, for every commit kind.
    #[test]
    fn zs2ca2_the_epoch_gate_decides_and_commits_as_the_full_form() {
        let commits = [
            ZgcAnnounceCommit::Size(40),
            ZgcAnnounceCommit::AlreadyCommitted,
        ];
        for commit in commits {
            let committing = commit != ZgcAnnounceCommit::AlreadyCommitted;
            let a = trimmed(Form::Long(ALL_LONG_TRIMS), commit);
            let b = trimmed(Form::Long(GATED_LONG_TRIMS), commit);
            let mut rng = XorShift(0x2522_CA20_0000_0001);
            let (mut inline, mut refused) = (0u64, 0u64);
            for trial in 0..400 {
                let skew = if trial % 4 == 3 {
                    [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
                } else {
                    0
                };
                let base = BASE_ALIGNED + skew;
                let start = base + 8 * rng.below(2048);
                let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
                let brk = match rng.below(12) {
                    0 => Break::Unarmed,
                    1 => Break::StaleEpoch,
                    2 => Break::Blocked,
                    3 => Break::Unpublished,
                    _ => Break::None,
                };
                let mut wa = World::new(base, start, end);
                let mut wb = World::new(base, start, end);
                let mut expect = World::new(base, start, end);
                apply(&mut wa, brk);
                apply(&mut wb, brk);
                // The gate's collector half: the raise and the withdrawal
                // move the epoch (`set_jit_inline_announce_blocked`,
                // `withdraw_jit_inline_announce` under the latched gate).
                if matches!(brk, Break::Blocked | Break::Unpublished) {
                    wb.table[4] += 1;
                }
                let mut cur = start;
                loop {
                    if rng.below(5) == 0 {
                        cur = (cur + 15) & !15;
                    }
                    let size = if committing {
                        40
                    } else {
                        16 + 8 * rng.below(32)
                    };
                    if cur + size > end {
                        break;
                    }
                    let obj = cur;
                    wa.thread[0] = obj;
                    wb.thread[0] = obj;
                    let (ta, tb) = (wa.thread_addr(), wb.thread_addr());
                    let ra = a.call3(ta, obj, obj + size);
                    let rb = b.call3(tb, obj, obj + size);
                    assert!(ra == 0 || ra == obj, "trial {trial}: obj or 0");
                    assert_eq!(ra, rb, "{commit:?} trial {trial} {brk:?}");
                    let committed = if committing && ra != 0 {
                        obj + size
                    } else {
                        obj
                    };
                    assert_eq!(wa.thread[0], committed, "trial {trial}: full cursor");
                    assert_eq!(wb.thread[0], committed, "trial {trial}: gated cursor");
                    if ra == 0 {
                        wa.helper_insert(obj);
                        wb.helper_insert(obj);
                        refused += 1;
                    } else {
                        inline += 1;
                    }
                    expect.helper_insert(obj);
                    cur += size;
                }
                assert_eq!(wa.bits, expect.bits, "trial {trial}: full vs insert");
                assert_eq!(wb.bits, expect.bits, "trial {trial}: gated vs insert");
            }
            eprintln!("zs2ca2 gate {commit:?}: inline={inline} refused={refused}");
            assert!(inline > 1000, "the inline arm ran ({inline})");
            assert!(refused > 100, "the helper arm ran ({refused})");
        }
    }

    /// zs s2/ca2 positive control: the gate really dropped the two tests (a
    /// `blocked` table WITHOUT the collector's epoch bump is stored into by
    /// the gated form and refused by the full one), it is exactly 7 + 3 + 6 +
    /// 3 + 6 = 25 bytes shorter on every commit kind, and the switch is default
    /// OFF with `=1` turning it on through the env-reading entry point.
    #[test]
    fn zs2ca2_the_epoch_gate_drops_two_tests_and_is_opt_in() {
        let commit = ZgcAnnounceCommit::Size(40);
        let base = BASE_ALIGNED;
        let (start, end) = (base + 4096, base + 8192);
        let full = trimmed(Form::Long(ALL_LONG_TRIMS), commit);
        let gated = trimmed(Form::Long(GATED_LONG_TRIMS), commit);
        let mut wa = World::new(base, start, end);
        let mut wb = World::new(base, start, end);
        wa.set_blocked(true);
        wb.set_blocked(true);
        let (ta, tb) = (wa.thread_addr(), wb.thread_addr());
        assert_eq!(full.call3(ta, start, start + 40), 0, "full form refuses");
        assert_eq!(
            gated.call3(tb, start, start + 40),
            start,
            "the gated form trusts the epoch: the collector half is load-bearing"
        );
        for c in [
            commit,
            ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 3 },
            ZgcAnnounceCommit::AlreadyCommitted,
        ] {
            assert_eq!(
                snippet_len(Form::Long(ALL_LONG_TRIMS), c),
                snippet_len(Form::Long(GATED_LONG_TRIMS), c) + 25,
                "{c:?}"
            );
        }
        let env_trims = || LongAnnounceTrims::from_env(true);
        {
            let edits = [("CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE", None)];
            let _unset = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&edits),
            );
            assert!(!zgc_announce_epoch_gate_enabled());
            assert!(!env_trims().epoch_gate);
        }
        {
            let edits = [("CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE", Some("1"))];
            let _on = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&edits),
            );
            assert!(zgc_announce_epoch_gate_enabled());
            assert!(env_trims().epoch_gate);
        }
    }

    /// zr z11/ia, against the REAL collector: a buffer carved by a
    /// `ZgcRealHeap` carries the owned-word window its chunk's geometry gives
    /// (the JIT reads it at `Tlab::ZGC_OWNED_WORDS_OFFSET`), and the window's
    /// offset is the JIT's copy.
    #[test]
    fn zr11ia_a_real_buffer_carries_its_owned_word_window() {
        assert_eq!(
            ZGC_TLAB_OWNED_WORDS_OFFSET as usize,
            cratonvm_gc::Tlab::ZGC_OWNED_WORDS_OFFSET
        );
        use cratonvm_gc::zgc::ZgcRealHeap;
        let edits = [
            ("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE", Some("1")),
            ("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1")),
        ];
        let heap = cratonvm_types::flags::with_thread_overrides(&edits, || {
            ZgcRealHeap::new_shared(32 * 1024 * 1024)
        });
        heap.set_vm_tlab_enabled(true);
        if heap.jit_short_announce_open().is_none() {
            eprintln!("zr11ia real heap: another heap owns the announce table; skipped");
            return;
        }
        let (ptr, size) =
            cratonvm_types::flags::with_thread_overrides(&edits, || heap.refill_tlab(512 * 1024))
                .expect("a VM-TLAB chunk");
        // SAFETY: freshly carved and zeroed for this buffer.
        let tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        let table = &cratonvm_gc::tlab::JIT_ZGC_ANNOUNCE;
        let base = table.base.load(std::sync::atomic::Ordering::Acquire);
        let span = table.span.load(std::sync::atomic::Ordering::Acquire);
        let lo = ptr as usize; // Cast: an address
        let want = cratonvm_gc::tlab::zgc_owned_word_window(base, span, lo, lo + size);
        if tlab.zgc_inline_announce_state().1 != 0 {
            assert_eq!(tlab.zgc_owned_word_window(), want);
            assert!(want[0] < want[1], "a 512 KiB chunk owns whole words");
        } else {
            // Disarmed (a concurrent test's collection moved the epoch): empty.
            assert_eq!(tlab.zgc_owned_word_window(), [0, 0]);
        }
        std::mem::forget(tlab);
    }
}
