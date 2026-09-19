// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Verification-derived per-method type maps ("oop maps").
//!
//! # Why this module exists
//!
//! [`crate::bytecode_verifier::verify_bytecode`] walks every instruction of
//! every method and computes the exact JVMS verification type of every local
//! slot and every operand-stack slot at every reachable pc. Historically it
//! then **threw all of it away** and returned `Result<(), LinkageError>`.
//!
//! Four subsystems then re-derived the same information at runtime, badly:
//!
//! * `types::value::Value` carries a runtime tag in every 16-byte slot, and
//!   `vm::runtime::value_stack::ValueStack` additionally carries a parallel
//!   `kinds: Vec<u8>` because a 64-bit `long` cannot be NaN-boxed.
//! * The GC scans those tags, and tags get lost (see the comment at
//!   `vm/src/memory/roots.rs` about a JIT callee's object return value
//!   reaching an interpreter local under a non-object tag). That forces
//!   conservative scanning, which forces a non-moving collector.
//! * The interpreter fast path cannot be proven safe per method, so it is
//!   gated on a package-name deny-list
//!   (`vm::runtime::frame::class_disables_interp_fast_path`), whose own TODO
//!   names the fix: "a per-method `unsafe_for_fast_path` flag computed at
//!   verification time".
//! * The JIT re-infers types it was already handed.
//!
//! This module makes verification **retain what it proves**. It is populated
//! by the *same* walk that already verifies — both the Java 7+ StackMapTable
//! path (JVMS §4.10.1) and the pre-Java-7 inference path (JVMS §4.10.2) — and
//! it is built **unconditionally on the default build path**. There is no
//! feature flag, no `CRATONVM_*` env var, and no opt-in: if verification ran,
//! the maps exist.
//!
//! # Nothing consumes this yet — deliberately
//!
//! The GC root scan, the interpreter fast path, and the frame representation
//! will consume it in a following wave. This module's contract is designed for
//! those consumers; see [`MethodTypeMaps::oop_map_at`] for the *exact* index
//! spaces and the clamping rules a consumer MUST obey.
//!
//! # Index spaces (read this before consuming)
//!
//! CratonVM's runtime does **not** use the JVMS slot discipline uniformly:
//!
//! | array | JVMS discipline | CratonVM runtime | this module emits |
//! |---|---|---|---|
//! | locals | cat-2 occupies 2 slots | cat-2 occupies 2 slots (`copy_args_to_locals` leaves the upper half uninitialised) | **JVMS/runtime local index** — they agree |
//! | operand stack | cat-2 occupies 2 slots (`Long`,`Top`) | cat-2 occupies **1** `CompactValue` slot (see `ValueStack::Dup2`) | **runtime (compressed) slot index** |
//!
//! So [`MethodTypeMaps::local_oops_at`] is indexed exactly like
//! `Frame::locals`, and [`MethodTypeMaps::stack_oops_at`] /
//! [`MethodTypeMaps::stack_depth_at`] are indexed exactly like
//! `Frame::stack` (a `long` contributes **one** stack bit, always clear).
//! The JVMS operand-stack depth is intentionally *not* retained; a consumer
//! that needs it (the JIT) can re-derive it from the bytecode it is already
//! walking.
//!
//! # Memory
//!
//! This is per-method metadata for 15k+ classes in a Spring application, so
//! every representation choice is a size choice:
//!
//! * pcs are stored as `u16` when the method's `code_length < 65536` (JVMS
//!   §4.7.3 mandates that), `u32` only for out-of-spec synthetic bytecode.
//! * oop bitmaps use **byte** stride, not 64-bit word stride: a method with
//!   6 locals and `max_stack = 4` costs 1 + 1 = 2 bytes per recorded pc, not
//!   16.
//! * stack depths are stored as `u8` when `max_stack <= 255`.
//!
//! A typical method (20 instruction starts, `max_locals = 6`,
//! `max_stack = 4`) therefore costs
//! `20*2 (pcs) + 20*1 (local bits) + 20*1 (stack bits) + 20*1 (depth) = 80`
//! bytes of row data, plus a ~96-byte [`MethodTypeMaps`] header and four
//! heap allocations. See [`MethodTypeMaps::heap_bytes`] and
//! [`store_heap_bytes`]; the design doc
//! `verifier-type-maps.md` carries the full
//! per-method accounting.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use cratonvm_reader::instruction::Instruction;
use cratonvm_types::ClassId;

use super::verify_frame::VerificationFrame;
use super::vtype::VType;

// ---------------------------------------------------------------------------
// Bit views
// ---------------------------------------------------------------------------

/// A borrowed, allocation-free view of one row of an oop bitmap.
///
/// This is a `Copy` value type (a slice plus a bit count), **not** a reference
/// to a heap object: constructing, cloning, and passing one around performs no
/// allocation and no atomic traffic, which is why it is safe to use from a GC
/// stop-the-world root scan.
#[derive(Clone, Copy)]
pub struct OopBits<'a> {
    bytes: &'a [u8],
    bits: u16,
}

impl<'a> OopBits<'a> {
    // NOTE: there is deliberately no `EMPTY` constant. "No map" and "a map
    // with no references" are different answers and a consumer must never
    // conflate them — see `MethodTypeMaps::oop_map_at`, which returns
    // `Option` for exactly that reason.

    #[inline]
    fn new(bytes: &'a [u8], bits: u16) -> Self {
        // Defensive: never let `bits` claim more storage than we hold.
        let max_bits = (bytes.len().saturating_mul(8)).min(u16::MAX as usize) as u16;
        Self {
            bytes,
            bits: bits.min(max_bits),
        }
    }

    /// Number of slots described by this row.
    #[inline]
    pub fn len(&self) -> usize {
        self.bits as usize
    }

    /// True when this row describes zero slots.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Is slot `index` proven by the verifier to hold an object reference?
    ///
    /// Out-of-range indices answer `false`, never panic — a GC root scan must
    /// never abort the VM because a frame was wider than its map.
    #[inline]
    pub fn get(&self, index: usize) -> bool {
        if index >= self.bits as usize {
            return false;
        }
        let byte = index >> 3;
        // `byte < bytes.len()` is guaranteed by the `bits` clamp in `new`.
        (self.bytes[byte] >> (index & 7)) & 1 != 0
    }

    /// True when no slot in this row is an object reference. A GC scan can
    /// skip the whole array on this answer.
    #[inline]
    pub fn is_all_clear(&self) -> bool {
        self.bytes.iter().all(|b| *b == 0)
    }

    /// Narrow this row to the first `n` slots.
    ///
    /// Consumers **must** do this whenever the runtime array is shorter than
    /// the map (see [`MethodTypeMaps::oop_map_at`] for when that happens).
    #[inline]
    pub fn clamped(self, n: usize) -> Self {
        Self {
            bytes: self.bytes,
            bits: self.bits.min(n.min(u16::MAX as usize) as u16),
        }
    }

    /// Iterate the indices of the set bits, low to high. Allocation-free.
    #[inline]
    pub fn iter_set(&self) -> SetBitIter<'a> {
        SetBitIter {
            bytes: self.bytes,
            bits: self.bits,
            next: 0,
        }
    }

    /// Raw backing bytes (little-endian bit order within each byte).
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl std::fmt::Debug for OopBits<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OopBits[")?;
        for i in 0..self.len() {
            write!(f, "{}", if self.get(i) { '1' } else { '.' })?;
        }
        write!(f, "]")
    }
}

/// Allocation-free iterator over the set bits of an [`OopBits`] row.
pub struct SetBitIter<'a> {
    bytes: &'a [u8],
    bits: u16,
    next: u16,
}

impl Iterator for SetBitIter<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        while self.next < self.bits {
            let i = self.next as usize;
            self.next += 1;
            if (self.bytes[i >> 3] >> (i & 7)) & 1 != 0 {
                return Some(i);
            }
        }
        None
    }
}

/// The local-variable half of an oop map. See [`OopBits`].
pub type LocalOopBits<'a> = OopBits<'a>;
/// The operand-stack half of an oop map. See [`OopBits`].
pub type StackOopBits<'a> = OopBits<'a>;

/// The complete verified reference layout of one frame at one pc.
#[derive(Clone, Copy, Debug)]
pub struct FrameOopMap<'a> {
    /// The pc this map describes (an instruction start).
    pub pc: u32,
    /// Which `Frame::locals` slots hold an object reference.
    pub locals: LocalOopBits<'a>,
    /// Which `Frame::stack` slots hold an object reference, in **runtime**
    /// (compressed, cat-2-is-one-slot) index space.
    pub stack: StackOopBits<'a>,
    /// Operand-stack depth in runtime slot space at `pc`, *before* the
    /// instruction at `pc` executes.
    pub stack_depth: u16,
}

impl<'a> FrameOopMap<'a> {
    /// The stack half narrowed to the frame's *actual* runtime depth.
    ///
    /// **Every consumer must use this, not [`Self::stack`], when scanning a
    /// live frame.** See [`MethodTypeMaps::oop_map_at`] for why the runtime
    /// depth can legitimately be lower than `stack_depth`.
    #[inline]
    pub fn stack_for_runtime_depth(&self, runtime_depth: usize) -> StackOopBits<'a> {
        self.stack
            .clamped(runtime_depth.min(self.stack_depth as usize))
    }

    /// The locals half narrowed to the frame's actual `locals.len()`.
    #[inline]
    pub fn locals_for_runtime_len(&self, runtime_len: usize) -> LocalOopBits<'a> {
        self.locals.clamped(runtime_len)
    }
}

// ---------------------------------------------------------------------------
// Compact row storage
// ---------------------------------------------------------------------------

/// A fixed-stride array of small bitmaps, one row per recorded pc.
///
/// Stride is in **bytes** (`ceil(bits / 8)`), deliberately not machine words:
/// the median Java method has fewer than 8 locals and a `max_stack` under 8,
/// so a word-stride representation would waste 7/8ths of the allocation.
pub struct CompactBitmapArray {
    bytes: Box<[u8]>,
    stride: u32,
    bits: u16,
    rows: u32,
}

impl CompactBitmapArray {
    fn empty(bits: u16) -> Self {
        Self {
            bytes: Box::new([]),
            stride: 0,
            bits,
            rows: 0,
        }
    }

    /// Logical bit width of each row.
    #[inline]
    pub fn bits(&self) -> u16 {
        self.bits
    }

    /// Number of rows.
    #[inline]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Borrow row `index`. Out-of-range answers an empty view rather than
    /// panicking — a GC scan must degrade, not abort.
    #[inline]
    pub fn row(&self, index: u32) -> OopBits<'_> {
        if self.stride == 0 || index >= self.rows {
            return OopBits {
                bytes: &[],
                bits: 0,
            };
        }
        let start = (index as usize) * (self.stride as usize);
        let end = start + self.stride as usize;
        OopBits::new(&self.bytes[start..end], self.bits)
    }

    /// Bytes of heap this array owns (excluding the struct header itself).
    pub fn heap_bytes(&self) -> usize {
        self.bytes.len()
    }
}

/// Instruction-start pcs, ascending. `u16` unless the method is out of spec.
enum PcTable {
    Empty,
    U16(Box<[u16]>),
    U32(Box<[u32]>),
}

impl PcTable {
    #[inline]
    fn len(&self) -> usize {
        match self {
            PcTable::Empty => 0,
            PcTable::U16(v) => v.len(),
            PcTable::U32(v) => v.len(),
        }
    }

    #[inline]
    fn get(&self, index: usize) -> Option<u32> {
        match self {
            PcTable::Empty => None,
            PcTable::U16(v) => v.get(index).map(|p| *p as u32),
            PcTable::U32(v) => v.get(index).copied(),
        }
    }

    /// Exact-match binary search. `O(log n)`, allocation-free, no panics.
    #[inline]
    fn search(&self, pc: u32) -> Option<usize> {
        match self {
            PcTable::Empty => None,
            PcTable::U16(v) => {
                if pc > u16::MAX as u32 {
                    return None;
                }
                v.binary_search(&(pc as u16)).ok()
            }
            PcTable::U32(v) => v.binary_search(&pc).ok(),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            PcTable::Empty => 0,
            PcTable::U16(v) => v.len() * 2,
            PcTable::U32(v) => v.len() * 4,
        }
    }
}

/// Per-pc operand-stack depth in runtime slot space.
enum DepthTable {
    Empty,
    U8(Box<[u8]>),
    U16(Box<[u16]>),
}

impl DepthTable {
    #[inline]
    fn get(&self, index: usize) -> Option<u16> {
        match self {
            DepthTable::Empty => None,
            DepthTable::U8(v) => v.get(index).map(|d| *d as u16),
            DepthTable::U16(v) => v.get(index).copied(),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            DepthTable::Empty => 0,
            DepthTable::U8(v) => v.len(),
            DepthTable::U16(v) => v.len() * 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Fast-path veto reasons
// ---------------------------------------------------------------------------

/// Why a method was denied [`MethodTypeMaps::safe_for_fast_path`].
///
/// Recorded for diagnosis; the *decision* is always the conservative one
/// (`false`) whenever any veto fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastPathVeto {
    /// The verifier walk did not cover every reachable instruction — lenient
    /// mode skipped an unreachable region, or a decode failure truncated the
    /// walk. The unchecked interpreter handlers rely on the walk having
    /// proven *every* executed instruction's operands, so a partial walk is
    /// not enough.
    IncompleteWalk,
    /// The method uses `jsr` / `jsr_w` / `ret`. Subroutine type-state is
    /// modelled only approximately (a shared subroutine entry collapses two
    /// distinct `ReturnAddress` values to `Top`), so local slots are not
    /// provably typed.
    Subroutine,
    /// A local-variable instruction addresses a slot `>= max_locals` (or a
    /// category-2 instruction addresses `max_locals - 1`). The unchecked
    /// `set_local_unchecked` / `get_local_compact_unchecked` handlers index
    /// `Frame::locals` with the raw operand and would run out of bounds.
    LocalIndexOutOfRange,
    /// A recorded frame carried more locals than `max_locals`, so the map
    /// cannot describe the whole array.
    LocalsWiderThanMaxLocals,
    /// A recorded frame's runtime operand-stack depth exceeded `max_stack`,
    /// so `push_unchecked` could overrun the `ValueStack` backing store.
    StackDepthExceedsMaxStack,
    /// A category-2 operand-stack entry was not followed by its `Top` upper
    /// half, so the JVMS→runtime stack compression is not trustworthy.
    MalformedCategory2,
    /// A bare `wide` prefix reached the type-map builder as a standalone
    /// instruction (decoder should have folded it).
    StrayWidePrefix,
    /// Recorded pcs arrived out of order, so the binary-search index would be
    /// wrong. The map is discarded entirely in this case.
    UnsortedPcs,
    /// The method's row data would exceed [`MAX_METHOD_ROW_BYTES`]. Only a
    /// hostile or machine-generated classfile (`max_locals` in the thousands
    /// across thousands of instructions) can reach this; recording stops at
    /// the budget and the remaining pcs answer `None`.
    MapBudgetExceeded,
}

/// Per-method cap on oop-map row data.
///
/// `max_locals` and `max_stack` are attacker-controlled `u16`s, so an
/// adversarial classfile could otherwise demand
/// `65535/8 * 2 * instruction_count` bytes of metadata. Real methods use a
/// few hundred bytes; 256 KiB covers a 2000-instruction method with 500
/// locals and 500 stack slots with room to spare.
pub const MAX_METHOD_ROW_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// MethodTypeMaps
// ---------------------------------------------------------------------------

/// Everything bytecode verification proved about one method, retained.
///
/// Construct via [`MethodTypeMapsBuilder`]; look up via [`type_maps_for`].
pub struct MethodTypeMaps {
    /// Instruction-start pcs, ascending.
    pcs: PcTable,
    /// Per-pc bitmap: which local slots hold an object reference.
    local_oops: CompactBitmapArray,
    /// Per-pc bitmap: which operand-stack slots hold an object reference
    /// (runtime/compressed slot space).
    stack_oops: CompactBitmapArray,
    /// Per-pc operand stack depth (runtime/compressed slot space).
    stack_depth: DepthTable,
    max_locals: u16,
    max_stack: u16,
    /// True iff every recorded row described all `max_locals` slots.
    locals_complete: bool,
    /// True iff this method's bytecode is safe for the interpreter fast path.
    safe_for_fast_path: bool,
    veto: Option<FastPathVeto>,
}

impl MethodTypeMaps {
    /// Number of recorded instruction starts.
    #[inline]
    pub fn entry_count(&self) -> usize {
        self.pcs.len()
    }

    /// `max_locals` from the method's `Code` attribute — the width of
    /// [`Self::local_oops_at`].
    #[inline]
    pub fn max_locals(&self) -> u16 {
        self.max_locals
    }

    /// `max_stack` from the method's `Code` attribute — an upper bound on the
    /// width of [`Self::stack_oops_at`].
    #[inline]
    pub fn max_stack(&self) -> u16 {
        self.max_stack
    }

    /// True iff every unchecked interpreter fast-path precondition was proven
    /// for this method. **Conservatively `false`** whenever anything at all
    /// was unproven; see [`FastPathVeto`].
    ///
    /// The property this asserts, precisely:
    ///
    /// 1. The verifier walk covered every reachable instruction start.
    /// 2. Every `*load`/`*store`/`iinc` operand (and the `+1` upper half of
    ///    every category-2 access) is `< max_locals`, so
    ///    `Frame::set_local_unchecked` / `get_local_compact_unchecked` cannot
    ///    index out of bounds.
    /// 3. At every recorded pc the runtime operand-stack depth is known and
    ///    `<= max_stack`, so `ValueStack::pop_unchecked` cannot underflow and
    ///    `push_unchecked` cannot overrun.
    /// 4. No `jsr`/`ret`, no stray `wide`, no malformed category-2 pair.
    ///
    /// It does **not** assert anything about JIT/native bridge behaviour, so
    /// it is not by itself a replacement for
    /// `vm::runtime::frame::class_disables_interp_fast_path`; see the
    /// migration notes in the module docs.
    #[inline]
    pub fn safe_for_fast_path(&self) -> bool {
        self.safe_for_fast_path
    }

    /// The first veto that denied [`Self::safe_for_fast_path`], if any.
    #[inline]
    pub fn fast_path_veto(&self) -> Option<FastPathVeto> {
        self.veto
    }

    /// True iff every recorded row describes all `max_locals` slots.
    ///
    /// **A moving collector must check this.** When it is `false`, at least
    /// one recorded frame carried fewer locals than `max_locals`, so the
    /// bits for the undescribed tail are clear and would look like "not a
    /// reference" — a missed root. Consumers must scan locals conservatively
    /// for a method whose maps report `false` here.
    ///
    /// In practice this is always `true`: both verifier walks pad every frame
    /// to `max_locals` before recording. It exists so that a future change to
    /// the frame representation cannot silently degrade root scanning.
    #[inline]
    pub fn locals_fully_described(&self) -> bool {
        self.locals_complete
    }

    /// Index of `pc` in the recorded pc table. `O(log n)`.
    #[inline]
    pub fn index_of_pc(&self, pc: u32) -> Option<usize> {
        self.pcs.search(pc)
    }

    /// Is `pc` a recorded instruction start?
    #[inline]
    pub fn has_pc(&self, pc: u32) -> bool {
        self.pcs.search(pc).is_some()
    }

    /// The `n`-th recorded pc, ascending.
    #[inline]
    pub fn pc_at_index(&self, index: usize) -> Option<u32> {
        self.pcs.get(index)
    }

    /// The oop map at `pc`: which local slots and which operand-stack slots
    /// hold an object reference, immediately **before** the instruction at
    /// `pc` executes.
    ///
    /// `O(log n)`, allocation-free, no locks, no panics — safe to call from a
    /// GC stop-the-world root scan.
    ///
    /// Returns `None` when `pc` is not a recorded instruction start. A `None`
    /// answer means *"unproven"*, never *"no references here"*: the consumer
    /// must fall back to conservative scanning. `None` arises when the method
    /// was never verified (see [`verification_status`]), when `pc` is inside
    /// an instruction rather than at its start, or when `pc` is in a region
    /// lenient verification skipped as unreachable.
    ///
    /// # Consumer contract — clamping (mandatory)
    ///
    /// * **Locals.** `Frame::locals` may be *longer* than `max_locals`
    ///   (`vm::runtime::frame::effective_max_locals` widens it when an
    ///   argument list overflows the declared `max_locals`). Slots at or past
    ///   [`Self::max_locals`] are **not described** and must be scanned
    ///   conservatively. Use [`FrameOopMap::locals_for_runtime_len`].
    /// * **Operand stack.** The interpreter pops an invoked method's
    ///   arguments off the caller's operand stack *before* pushing the callee
    ///   frame, so while a callee runs the caller's runtime depth is lower
    ///   than the verifier's depth at the invoke pc. Because popping only ever
    ///   removes from the top, slots `[0, runtime_depth)` still carry exactly
    ///   the types this map states — so clamp with
    ///   [`FrameOopMap::stack_for_runtime_depth`] and never read past the
    ///   frame's real depth.
    /// * **Slot space.** `stack` bits are in runtime/compressed slot space: a
    ///   `long` or `double` occupies **one** stack bit (always clear), not
    ///   two. Local bits are in JVMS slot space, which is also the runtime
    ///   local space (a `long` occupies two local slots, both clear).
    #[inline]
    pub fn oop_map_at(&self, pc: u32) -> Option<(LocalOopBits<'_>, StackOopBits<'_>)> {
        let idx = self.pcs.search(pc)? as u32;
        Some((self.local_oops.row(idx), self.stack_oops.row(idx)))
    }

    /// Like [`Self::oop_map_at`] but also carries the pc and the verified
    /// operand-stack depth. Prefer this in a root scan: the depth is what you
    /// clamp against.
    #[inline]
    pub fn frame_map_at(&self, pc: u32) -> Option<FrameOopMap<'_>> {
        let idx = self.pcs.search(pc)?;
        Some(FrameOopMap {
            pc,
            locals: self.local_oops.row(idx as u32),
            stack: self.stack_oops.row(idx as u32),
            stack_depth: self.stack_depth.get(idx).unwrap_or(0),
        })
    }

    /// Local-slot oop bits at `pc`. See [`Self::oop_map_at`].
    #[inline]
    pub fn local_oops_at(&self, pc: u32) -> Option<LocalOopBits<'_>> {
        let idx = self.pcs.search(pc)?;
        Some(self.local_oops.row(idx as u32))
    }

    /// Operand-stack oop bits at `pc`. See [`Self::oop_map_at`].
    #[inline]
    pub fn stack_oops_at(&self, pc: u32) -> Option<StackOopBits<'_>> {
        let idx = self.pcs.search(pc)?;
        Some(self.stack_oops.row(idx as u32))
    }

    /// Verified operand-stack depth at `pc`, in runtime slot space.
    #[inline]
    pub fn stack_depth_at(&self, pc: u32) -> Option<u16> {
        let idx = self.pcs.search(pc)?;
        self.stack_depth.get(idx)
    }

    /// Bytes of heap owned by this map, excluding the struct header.
    ///
    /// Mirrors the established `heap_bytes()` accounting convention so a
    /// whole-VM footprint report can sum type-map cost alongside the other
    /// per-method side tables.
    pub fn heap_bytes(&self) -> usize {
        self.pcs.heap_bytes()
            + self.local_oops.heap_bytes()
            + self.stack_oops.heap_bytes()
            + self.stack_depth.heap_bytes()
    }

    /// `heap_bytes()` plus the struct header — what this map costs in total.
    pub fn total_bytes(&self) -> usize {
        self.heap_bytes() + std::mem::size_of::<MethodTypeMaps>()
    }
}

impl std::fmt::Debug for MethodTypeMaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MethodTypeMaps")
            .field("entries", &self.entry_count())
            .field("max_locals", &self.max_locals)
            .field("max_stack", &self.max_stack)
            .field("locals_complete", &self.locals_complete)
            .field("safe_for_fast_path", &self.safe_for_fast_path)
            .field("veto", &self.veto)
            .field("heap_bytes", &self.heap_bytes())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Builder — driven by the verifier walk
// ---------------------------------------------------------------------------

/// Accumulates [`MethodTypeMaps`] rows as the verifier walk visits each
/// instruction. One builder per method.
///
/// The builder is deliberately push-driven so that it rides the *existing*
/// verification walk instead of adding a second pass: the StackMapTable linear
/// walk calls [`Self::record`] at each instruction start just before
/// `verify_instruction`, and the pre-Java-7 worklist calls it once per
/// reachable pc over the settled fixpoint states, in ascending pc order.
pub struct MethodTypeMapsBuilder {
    max_locals: u16,
    max_stack: u16,
    local_stride: usize,
    stack_stride: usize,
    pcs: Vec<u32>,
    local_bytes: Vec<u8>,
    stack_bytes: Vec<u8>,
    depths: Vec<u16>,
    max_depth_seen: u16,
    locals_complete: bool,
    veto: Option<FastPathVeto>,
    sorted: bool,
}

#[inline]
fn stride_for(bits: u16) -> usize {
    ((bits as usize) + 7) / 8
}

impl MethodTypeMapsBuilder {
    /// Start a builder for a method with the given `Code` attribute limits.
    pub fn new(max_locals: u16, max_stack: u16) -> Self {
        Self {
            max_locals,
            max_stack,
            local_stride: stride_for(max_locals),
            stack_stride: stride_for(max_stack),
            pcs: Vec::new(),
            local_bytes: Vec::new(),
            stack_bytes: Vec::new(),
            depths: Vec::new(),
            max_depth_seen: 0,
            locals_complete: true,
            veto: None,
            sorted: true,
        }
    }

    /// Pre-size the row vectors for a method with `insn_hint` instructions.
    ///
    /// Reservations are clamped to [`MAX_METHOD_ROW_BYTES`] so an
    /// attacker-controlled `max_locals`/`max_stack` cannot turn a hint into a
    /// hundreds-of-megabytes allocation.
    pub fn reserve(&mut self, insn_hint: usize) {
        let hint = insn_hint.min(MAX_METHOD_ROW_BYTES);
        self.pcs.reserve(hint);
        self.depths.reserve(hint);
        self.local_bytes.reserve(
            hint.saturating_mul(self.local_stride)
                .min(MAX_METHOD_ROW_BYTES),
        );
        self.stack_bytes.reserve(
            hint.saturating_mul(self.stack_stride)
                .min(MAX_METHOD_ROW_BYTES),
        );
    }

    /// Would recording one more row push this method past its budget?
    #[inline]
    fn over_budget(&self) -> bool {
        self.local_bytes.len() + self.stack_bytes.len() + self.local_stride + self.stack_stride
            > MAX_METHOD_ROW_BYTES
    }

    /// Deny [`MethodTypeMaps::safe_for_fast_path`]. First veto wins (it is the
    /// earliest, hence most informative); later ones are dropped because the
    /// decision is already the conservative one.
    #[inline]
    pub fn mark_unsafe_for_fast_path(&mut self, reason: FastPathVeto) {
        if self.veto.is_none() {
            self.veto = Some(reason);
        }
    }

    /// Has a veto already fired?
    #[inline]
    pub fn is_vetoed(&self) -> bool {
        self.veto.is_some()
    }

    /// Number of rows recorded so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.pcs.len()
    }

    /// True when no row has been recorded.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pcs.is_empty()
    }

    /// Record the verified type state at instruction start `pc`.
    ///
    /// Must be called in ascending `pc` order; a repeat of the previous pc is
    /// ignored, a decrease vetoes and discards the map (defence in depth — the
    /// two in-tree callers are both ordered by construction).
    pub fn record(&mut self, pc: u32, frame: &VerificationFrame) {
        self.record_slots(pc, &frame.locals, &frame.stack);
    }

    /// Slice-level form of [`Self::record`], so the row encoding can be unit
    /// tested without constructing a whole [`VerificationFrame`].
    pub fn record_slots(&mut self, pc: u32, locals: &[VType], stack: &[VType]) {
        if let Some(&last) = self.pcs.last() {
            if pc == last {
                return; // idempotent re-record of the same instruction start
            }
            if pc < last {
                self.sorted = false;
                self.mark_unsafe_for_fast_path(FastPathVeto::UnsortedPcs);
                return;
            }
        }

        if self.over_budget() {
            // Stop recording rather than let a hostile classfile mint
            // unbounded metadata. Unrecorded pcs answer `None`, which every
            // consumer already has to handle as "scan conservatively".
            self.mark_unsafe_for_fast_path(FastPathVeto::MapBudgetExceeded);
            return;
        }

        // --- locals row: JVMS slot space, identical to `Frame::locals` ------
        let base = self.local_bytes.len();
        self.local_bytes.resize(base + self.local_stride, 0);
        if locals.len() > self.max_locals as usize {
            self.mark_unsafe_for_fast_path(FastPathVeto::LocalsWiderThanMaxLocals);
        }
        if locals.len() < self.max_locals as usize {
            // The tail bits would read as "not a reference" without ever
            // having been proven so — a missed root for a moving collector.
            // Flag the whole method rather than lie about the tail.
            self.locals_complete = false;
        }
        let n_locals = locals.len().min(self.max_locals as usize);
        for (i, ty) in locals.iter().take(n_locals).enumerate() {
            if ty.is_reference() {
                self.local_bytes[base + (i >> 3)] |= 1u8 << (i & 7);
            }
        }

        // --- stack row: runtime/compressed slot space -----------------------
        // The verifier models a category-2 value as two entries (`Long`/`Top`);
        // `ValueStack` stores it in one `CompactValue`. Compress here so the
        // emitted bit index is directly usable as a `Frame::stack` index.
        let sbase = self.stack_bytes.len();
        self.stack_bytes.resize(sbase + self.stack_stride, 0);
        let mut rt: usize = 0;
        let mut i: usize = 0;
        while i < stack.len() {
            let ty = &stack[i];
            if rt < self.max_stack as usize {
                if ty.is_reference() {
                    self.stack_bytes[sbase + (rt >> 3)] |= 1u8 << (rt & 7);
                }
            } else {
                self.mark_unsafe_for_fast_path(FastPathVeto::StackDepthExceedsMaxStack);
            }
            if ty.is_category2() {
                // The upper half must be present and must be `Top`.
                if stack.get(i + 1) == Some(&VType::Top) {
                    i += 2;
                } else {
                    self.mark_unsafe_for_fast_path(FastPathVeto::MalformedCategory2);
                    i += 1;
                }
            } else {
                i += 1;
            }
            rt += 1;
        }
        let depth = rt.min(u16::MAX as usize) as u16;
        if depth > self.max_stack {
            self.mark_unsafe_for_fast_path(FastPathVeto::StackDepthExceedsMaxStack);
        }
        if depth > self.max_depth_seen {
            self.max_depth_seen = depth;
        }

        self.pcs.push(pc);
        self.depths.push(depth);
    }

    /// Fold one decoded instruction into the fast-path safety decision.
    ///
    /// Called from the verifier walk right after `Instruction::decode`. This
    /// is what turns the runtime's package-name deny-list into a per-method
    /// proof: every unchecked local access the interpreter fast path performs
    /// is keyed on the raw bytecode operand, so the operand must be proven in
    /// range here.
    pub fn observe_instruction(&mut self, insn: &Instruction) {
        match insn {
            // Category-1 local accesses: slot must be < max_locals.
            Instruction::Iload(i)
            | Instruction::Fload(i)
            | Instruction::Aload(i)
            | Instruction::Istore(i)
            | Instruction::Fstore(i)
            | Instruction::Astore(i) => self.check_local_slot(*i, false),

            // Category-2 local accesses: slots `i` and `i + 1` must BOTH be
            // < max_locals (`lstore n` writes n and n+1).
            Instruction::Lload(i)
            | Instruction::Dload(i)
            | Instruction::Lstore(i)
            | Instruction::Dstore(i) => self.check_local_slot(*i, true),

            Instruction::Iinc { index, .. } => self.check_local_slot(*index, false),

            Instruction::Ret(i) => {
                self.check_local_slot(*i, false);
                self.mark_unsafe_for_fast_path(FastPathVeto::Subroutine);
            }
            Instruction::Jsr(_) | Instruction::JsrW(_) => {
                self.mark_unsafe_for_fast_path(FastPathVeto::Subroutine);
            }

            Instruction::Wide => {
                self.mark_unsafe_for_fast_path(FastPathVeto::StrayWidePrefix);
            }

            _ => {}
        }
    }

    #[inline]
    fn check_local_slot(&mut self, index: u16, category2: bool) {
        let highest = if category2 {
            match index.checked_add(1) {
                Some(hi) => hi,
                None => {
                    self.mark_unsafe_for_fast_path(FastPathVeto::LocalIndexOutOfRange);
                    return;
                }
            }
        } else {
            index
        };
        if highest >= self.max_locals {
            self.mark_unsafe_for_fast_path(FastPathVeto::LocalIndexOutOfRange);
        }
    }

    /// Freeze into a [`MethodTypeMaps`].
    ///
    /// `walk_complete` must be `false` whenever the caller's walk skipped or
    /// truncated any region (lenient unreachable-code skipping, a decode
    /// failure, a structural-only fallback); that denies
    /// [`MethodTypeMaps::safe_for_fast_path`] but keeps the rows that *were*
    /// proven usable for GC scanning, which is sound because a `None` answer
    /// at an unrecorded pc already means "scan conservatively".
    pub fn finish(mut self, walk_complete: bool) -> MethodTypeMaps {
        if !walk_complete {
            self.mark_unsafe_for_fast_path(FastPathVeto::IncompleteWalk);
        }

        if !self.sorted {
            // The binary-search index would be wrong, and a wrong oop map is
            // worse than none: drop every row.
            return MethodTypeMaps {
                pcs: PcTable::Empty,
                local_oops: CompactBitmapArray::empty(self.max_locals),
                stack_oops: CompactBitmapArray::empty(self.max_stack),
                stack_depth: DepthTable::Empty,
                max_locals: self.max_locals,
                max_stack: self.max_stack,
                locals_complete: false,
                safe_for_fast_path: false,
                veto: Some(FastPathVeto::UnsortedPcs),
            };
        }

        let rows = self.pcs.len() as u32;

        let pcs = if self.pcs.is_empty() {
            PcTable::Empty
        } else if self.pcs.last().copied().unwrap_or(0) <= u16::MAX as u32 {
            PcTable::U16(self.pcs.iter().map(|p| *p as u16).collect())
        } else {
            PcTable::U32(self.pcs.into_boxed_slice())
        };

        let local_oops = if self.local_stride == 0 || rows == 0 {
            CompactBitmapArray::empty(self.max_locals)
        } else {
            CompactBitmapArray {
                bytes: self.local_bytes.into_boxed_slice(),
                stride: self.local_stride as u32,
                bits: self.max_locals,
                rows,
            }
        };

        let stack_oops = if self.stack_stride == 0 || rows == 0 {
            CompactBitmapArray::empty(self.max_stack)
        } else {
            CompactBitmapArray {
                bytes: self.stack_bytes.into_boxed_slice(),
                stride: self.stack_stride as u32,
                bits: self.max_stack,
                rows,
            }
        };

        let stack_depth = if self.depths.is_empty() {
            DepthTable::Empty
        } else if self.max_depth_seen <= u8::MAX as u16 {
            DepthTable::U8(self.depths.iter().map(|d| *d as u8).collect())
        } else {
            DepthTable::U16(self.depths.into_boxed_slice())
        };

        MethodTypeMaps {
            pcs,
            local_oops,
            stack_oops,
            stack_depth,
            max_locals: self.max_locals,
            max_stack: self.max_stack,
            locals_complete: self.locals_complete,
            safe_for_fast_path: self.veto.is_none(),
            veto: self.veto,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-class side table
// ---------------------------------------------------------------------------

/// Identity of one method within its class, for the by-name lookup path.
struct MethodSig {
    hash: u32,
    name: Arc<str>,
    descriptor: Arc<str>,
}

/// The verification result for one class: a type map per method (or `None`
/// for abstract / native / bodyless methods).
pub struct ClassTypeMaps {
    /// `false` for the explicit "verification was skipped" marker published by
    /// [`mark_class_verification_skipped`].
    verified: bool,
    /// Indexed by position in `Class::methods`.
    methods: Box<[Option<MethodTypeMaps>]>,
    /// Parallel to `methods`; enables lookup by `(name, descriptor)` for
    /// consumers (the GC root scan) that hold a `Frame` rather than a method
    /// index.
    sigs: Box<[MethodSig]>,
}

impl ClassTypeMaps {
    /// Build from per-method results, in `Class::methods` order.
    ///
    /// `entries` must be `(name, descriptor, maps)` for **every** method of
    /// the class in order, including abstract/native ones (which pass `None`),
    /// so that indices line up with `Class::methods`.
    pub fn new(entries: Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)>) -> Self {
        let mut methods = Vec::with_capacity(entries.len());
        let mut sigs = Vec::with_capacity(entries.len());
        for (name, descriptor, maps) in entries {
            sigs.push(MethodSig {
                hash: sig_hash(&name, &descriptor),
                name,
                descriptor,
            });
            methods.push(maps);
        }
        Self {
            verified: true,
            methods: methods.into_boxed_slice(),
            sigs: sigs.into_boxed_slice(),
        }
    }

    fn skipped_marker() -> Self {
        Self {
            verified: false,
            methods: Box::new([]),
            sigs: Box::new([]),
        }
    }

    /// Did verification actually run for this class?
    #[inline]
    pub fn verified(&self) -> bool {
        self.verified
    }

    /// Number of methods described (equals `Class::methods.len()`).
    #[inline]
    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    /// Type maps for `method_index` (a position in `Class::methods`). `O(1)`.
    #[inline]
    pub fn method(&self, method_index: usize) -> Option<&MethodTypeMaps> {
        if !self.verified {
            return None;
        }
        self.methods.get(method_index)?.as_ref()
    }

    /// Resolve a `(name, descriptor)` pair to its `Class::methods` index.
    ///
    /// A `u32` hash prefilter over a small array (median Java class has well
    /// under 32 methods), with an exact string check on every hash hit — so a
    /// hash collision can never return the wrong method's map.
    #[inline]
    pub fn method_index_of(&self, name: &str, descriptor: &str) -> Option<usize> {
        let want = sig_hash(name, descriptor);
        for (i, sig) in self.sigs.iter().enumerate() {
            if sig.hash == want && &*sig.name == name && &*sig.descriptor == descriptor {
                return Some(i);
            }
        }
        None
    }

    /// Type maps for a method identified by name + descriptor.
    #[inline]
    pub fn method_named(&self, name: &str, descriptor: &str) -> Option<&MethodTypeMaps> {
        if !self.verified {
            return None;
        }
        self.method(self.method_index_of(name, descriptor)?)
    }

    /// Bytes of heap owned by this class's maps.
    pub fn heap_bytes(&self) -> usize {
        let mut total = self.methods.len() * std::mem::size_of::<Option<MethodTypeMaps>>()
            + self.sigs.len() * std::mem::size_of::<MethodSig>();
        for m in self.methods.iter().flatten() {
            total += m.heap_bytes();
        }
        total
    }
}

/// FxHash-flavoured 32-bit hash over `name` + `descriptor`.
#[inline]
fn sig_hash(name: &str, descriptor: &str) -> u32 {
    // Small, branch-light, and stable across runs. Correctness never depends
    // on it — every hit is confirmed with a full string compare.
    let mut h: u32 = 0x811c_9dc5;
    for b in name.as_bytes().iter().chain(descriptor.as_bytes()) {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// Global store
// ---------------------------------------------------------------------------

/// What is known about a class's verification state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationStatus {
    /// Nothing has been published for this `ClassId`. Either the class has not
    /// been verified yet, or it took a path that does not produce maps (the
    /// `jsr`/`ret` structural-only fallback). Consumers must scan
    /// conservatively.
    Unknown,
    /// Verification was deliberately skipped (`--noverify` /
    /// `-Xverify:none` / a per-class skip). No maps exist and none ever will
    /// for this class; consumers must scan conservatively **and** must not
    /// take the unchecked interpreter fast path.
    Skipped,
    /// Verification ran; maps are available for every method that had a
    /// `Code` attribute.
    Verified,
}

const CHUNK_SHIFT: u32 = 10;
const CHUNK_LEN: usize = 1 << CHUNK_SHIFT;
const CHUNK_MASK: u32 = (CHUNK_LEN as u32) - 1;
/// 4096 chunks × 1024 ids = 4,194,304 addressable class ids. Beyond that the
/// store answers `Unknown` and consumers degrade to conservative scanning.
const MAX_CHUNKS: usize = 4096;

struct Chunk {
    slots: [AtomicPtr<ClassTypeMaps>; CHUNK_LEN],
}

static STORE_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
static STORE_CLASS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The store hands out `&'static` references across threads through raw
/// pointers, which bypasses the usual auto-trait checking. Assert the two
/// properties that soundness depends on so a future field addition (an
/// `Rc`, a `Cell`) fails the build instead of racing at runtime.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn store_soundness_assertions() {
        assert_send_sync::<ClassTypeMaps>();
        assert_send_sync::<MethodTypeMaps>();
        assert_send_sync::<Chunk>();
    }
};

/// Top-level chunk directory.
///
/// Deliberately lock-free rather than an `RwLock<Vec<..>>`: the GC root scan
/// reads this while the world is stopped, and a thread suspended at a
/// safepoint *while holding a write lock* would deadlock the collector. Two
/// relaxed/acquire loads have no such failure mode.
#[inline]
fn chunk_directory() -> &'static [AtomicPtr<Chunk>] {
    static DIR: OnceLock<Box<[AtomicPtr<Chunk>]>> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut v = Vec::with_capacity(MAX_CHUNKS);
        for _ in 0..MAX_CHUNKS {
            v.push(AtomicPtr::new(ptr::null_mut()));
        }
        v.into_boxed_slice()
    })
}

#[inline]
fn chunk_for(class_id: ClassId) -> Option<&'static Chunk> {
    let raw = class_id.as_u32();
    let ci = (raw >> CHUNK_SHIFT) as usize;
    let dir = chunk_directory();
    let slot = dir.get(ci)?;
    let p = slot.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: chunks are allocated with `Box::into_raw` and never freed or
        // moved, so a non-null pointer read out of the directory is valid for
        // the remaining life of the process.
        Some(unsafe { &*p })
    }
}

fn chunk_for_or_create(class_id: ClassId) -> Option<&'static Chunk> {
    let raw = class_id.as_u32();
    let ci = (raw >> CHUNK_SHIFT) as usize;
    let dir = chunk_directory();
    let slot = dir.get(ci)?;
    let existing = slot.load(Ordering::Acquire);
    if !existing.is_null() {
        // SAFETY: see `chunk_for`.
        return Some(unsafe { &*existing });
    }
    let fresh = Box::into_raw(Box::new(Chunk {
        slots: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
    }));
    match slot.compare_exchange(ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: we published `fresh`; it is now immortal.
        Ok(_) => Some(unsafe { &*fresh }),
        Err(winner) => {
            // Another thread won the race — reclaim our loser allocation.
            // SAFETY: `fresh` was never published, so nobody else can see it.
            drop(unsafe { Box::from_raw(fresh) });
            // SAFETY: see `chunk_for`.
            Some(unsafe { &*winner })
        }
    }
}

/// All published type maps for `class_id`, or `None` if nothing is known.
///
/// `O(1)`: two atomic acquire loads and an array index. Lock-free and
/// allocation-free — safe from a GC stop-the-world root scan.
#[inline]
pub fn class_type_maps(class_id: ClassId) -> Option<&'static ClassTypeMaps> {
    let chunk = chunk_for(class_id)?;
    let p = chunk.slots[(class_id.as_u32() & CHUNK_MASK) as usize].load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: published `ClassTypeMaps` are leaked (never freed), so a
        // non-null pointer is valid for `'static`.
        Some(unsafe { &*p })
    }
}

/// Type maps for one method, `O(1)`.
///
/// `method_index` is the position of the method in `Class::methods`.
///
/// Returns `None` when the class was never verified, when verification was
/// skipped (distinguish with [`verification_status`]), when `method_index` is
/// out of range, or when that method has no `Code` attribute (abstract /
/// native). In every one of those cases the consumer must fall back to
/// conservative behaviour — a `None` is never a licence to assume "no
/// references".
#[inline]
pub fn type_maps_for(class_id: ClassId, method_index: usize) -> Option<&'static MethodTypeMaps> {
    class_type_maps(class_id)?.method(method_index)
}

/// Type maps for one method, identified by name and descriptor.
///
/// Convenience for consumers that hold a `Frame` (which carries the method
/// name and descriptor but not its index). Prefer [`type_maps_for`] once the
/// method index is cached in the frame.
#[inline]
pub fn type_maps_for_named(
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<&'static MethodTypeMaps> {
    class_type_maps(class_id)?.method_named(name, descriptor)
}

/// What is known about `class_id`'s verification.
#[inline]
pub fn verification_status(class_id: ClassId) -> VerificationStatus {
    match class_type_maps(class_id) {
        None => VerificationStatus::Unknown,
        Some(c) if c.verified => VerificationStatus::Verified,
        Some(_) => VerificationStatus::Skipped,
    }
}

fn install(class_id: ClassId, maps: ClassTypeMaps, replace: bool) -> bool {
    let Some(chunk) = chunk_for_or_create(class_id) else {
        return false; // class id beyond the addressable range
    };
    let idx = (class_id.as_u32() & CHUNK_MASK) as usize;
    let bytes = maps.heap_bytes() + std::mem::size_of::<ClassTypeMaps>();
    let fresh = Box::into_raw(Box::new(maps));

    if replace {
        let _old = chunk.slots[idx].swap(fresh, Ordering::AcqRel);
        // The previous maps are intentionally leaked. A GC root scan may hold
        // a `&'static MethodTypeMaps` borrowed from them at this instant, and
        // there is no epoch reclamation in this crate. JVMTI `RedefineClasses`
        // is the only path that reaches here, and it is rare; see the design
        // doc's "known limitations".
        STORE_HEAP_BYTES.fetch_add(bytes, Ordering::Relaxed);
        return true;
    }

    match chunk.slots[idx].compare_exchange(
        ptr::null_mut(),
        fresh,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            STORE_HEAP_BYTES.fetch_add(bytes, Ordering::Relaxed);
            STORE_CLASS_COUNT.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(existing) => {
            // SAFETY: `existing` is a published, immortal pointer.
            let existing_ref = unsafe { &*existing };
            if !existing_ref.verified {
                // Upgrade a "skipped" marker to real maps.
                let _old = chunk.slots[idx].swap(fresh, Ordering::AcqRel);
                STORE_HEAP_BYTES.fetch_add(bytes, Ordering::Relaxed);
                true
            } else {
                // Concurrent duplicate verification of the same class — keep
                // the winner and free our copy rather than leaking it.
                // SAFETY: `fresh` was never published.
                drop(unsafe { Box::from_raw(fresh) });
                false
            }
        }
    }
}

/// Publish `maps` for `class_id`.
///
/// Called unconditionally by [`crate::bytecode_verifier::verify_bytecode`] on
/// the default build path. Returns `false` when an equivalent entry already
/// existed (concurrent duplicate verification) or when `class_id` is outside
/// the addressable range.
pub fn publish_class_type_maps(class_id: ClassId, maps: ClassTypeMaps) -> bool {
    install(class_id, maps, false)
}

/// Replace the maps for `class_id` (JVMTI `RedefineClasses` / `RetransformClasses`).
///
/// The previous maps are leaked; see the note in [`install`].
pub fn replace_class_type_maps(class_id: ClassId, maps: ClassTypeMaps) -> bool {
    install(class_id, maps, true)
}

/// Record that verification was deliberately skipped for `class_id`
/// (`--noverify` / `-Xverify:none` / a per-class skip).
///
/// This is what lets a consumer tell "we have no map because nobody verified
/// this" apart from "we have no map because this pc is not an instruction
/// start". Without it, [`verification_status`] answers
/// [`VerificationStatus::Unknown`], which is equally conservative but less
/// diagnosable.
pub fn mark_class_verification_skipped(class_id: ClassId) -> bool {
    install(class_id, ClassTypeMaps::skipped_marker(), false)
}

// ---------------------------------------------------------------------------
// Class-name index (diagnostics only, off by default)
// ---------------------------------------------------------------------------

/// `class name -> ClassId`, for a consumer that has a NAME and needs the maps.
///
/// # Why this exists at all, when the class manager already answers it
///
/// The one consumer is the compiled-frame oop-map oracle
/// (`vm::jit::conservative_roots::verify_precise_covers_conservative`), and it
/// runs **inside a stop-the-world root scan** holding only a raw `rbp`. All it
/// has to identify the frame's method with is
/// `CompiledMethod::method_label` — `"org/h2/mvstore/MVStore.closeStore:(ZI)V"`
/// — because the JIT is handed class NAMES, not `ClassId`s, at every one of its
/// compile doors.
///
/// Resolving that through `class_id_by_name` means taking the class manager's
/// lock, and a thread suspended at a safepoint while holding it deadlocks the
/// collector. That is the same hazard [`chunk_directory`]'s doc records, and it
/// is why this index is lock-free: two atomic acquire loads and a string
/// compare, allocation-free, safe to call from a stopped world.
///
/// # It is OFF unless the oracle is
///
/// Nothing in production reads it, so nothing in production should pay for it.
/// [`note_class_name`] returns immediately unless `CRATONVM_DBG_VERIFY_OOP_MAPS`
/// is set, which is read once. With the flag off this costs one relaxed bool
/// load per class loaded and allocates nothing.
///
/// # What it deliberately cannot do
///
/// **It is keyed on the name alone, so it cannot tell two loaders' versions of
/// one class apart.** First writer wins and [`NAME_INDEX_COLLISIONS`] counts
/// the rest, so a reader can see whether the answer is trustworthy for the run
/// in front of them. That is acceptable for a diagnostic and would not be for a
/// consumer that acted on the answer — which is why this is `pub` next to a
/// doc comment saying so rather than folded into [`class_type_maps`].
struct NameEntry {
    name: Box<str>,
    class_id: u32,
}

/// Open-addressed, power-of-two, fixed. A Spring application loads 15k+
/// classes; 64Ki slots keeps the load factor under 25%, where linear probing
/// is still short. An index that fills up stops accepting entries rather than
/// growing — a full index degrades to "unknown", which is the fail-open
/// direction for something that only ever narrows a diagnostic.
const NAME_INDEX_SLOTS: usize = 1 << 16;

static NAME_INDEX_LIVE: AtomicUsize = AtomicUsize::new(0);
static NAME_INDEX_COLLISIONS: AtomicUsize = AtomicUsize::new(0);

fn name_index() -> &'static [AtomicPtr<NameEntry>] {
    static IDX: OnceLock<Box<[AtomicPtr<NameEntry>]>> = OnceLock::new();
    IDX.get_or_init(|| {
        let mut v = Vec::with_capacity(NAME_INDEX_SLOTS);
        for _ in 0..NAME_INDEX_SLOTS {
            v.push(AtomicPtr::new(ptr::null_mut()));
        }
        v.into_boxed_slice()
    })
}

/// Is the one consumer switched on? Read once.
fn name_index_wanted() -> bool {
    #[cfg(test)]
    if NAME_INDEX_FORCED_ON.load(Ordering::Relaxed) {
        // The gate below is a `OnceLock` over a process-wide variable, so a
        // test cannot turn it on for itself -- whichever test ran first would
        // decide it for the whole binary. See `NAME_INDEX_FORCED_ON`.
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VERIFY_OOP_MAPS").is_some()
    })
}

/// Test-only override for [`name_index_wanted`]. Never read in a release build.
#[cfg(test)]
static NAME_INDEX_FORCED_ON: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn name_hash(name: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    (h as usize) & (NAME_INDEX_SLOTS - 1)
}

/// Record `name -> class_id` for [`class_id_of_name`]. No-op unless the oracle
/// that consumes it is enabled; see [`NameEntry`].
///
/// Called beside every [`publish_class_type_maps`] so the index and the maps
/// are populated by the same event — a name that resolves but has no maps would
/// be worse than no name at all, because the oracle would then read a `None`
/// as "this pc is not an instruction start".
pub fn note_class_name(class_id: ClassId, name: &str) {
    if !name_index_wanted() || name.is_empty() {
        return;
    }
    let idx = name_index();
    let mut i = name_hash(name);
    for _ in 0..64 {
        let slot = &idx[i];
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            let fresh = Box::into_raw(Box::new(NameEntry {
                name: name.into(),
                class_id: class_id.as_u32(),
            }));
            match slot.compare_exchange(ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    NAME_INDEX_LIVE.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    // Lost the race for this slot; reclaim and re-probe it.
                    // SAFETY: `fresh` was never published.
                    drop(unsafe { Box::from_raw(fresh) });
                    continue;
                }
            }
        }
        // SAFETY: published entries are leaked, so a non-null pointer is valid
        // for the rest of the process.
        let e = unsafe { &*p };
        if &*e.name == name {
            if e.class_id != class_id.as_u32() {
                NAME_INDEX_COLLISIONS.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        i = (i + 1) & (NAME_INDEX_SLOTS - 1);
    }
}

/// `ClassId` for a binary class name (`org/h2/mvstore/MVStore`), or `None`.
///
/// Lock-free and allocation-free: safe from a stop-the-world root scan, which
/// is the only reason it exists. `None` whenever the index is off, full, or the
/// name was never loaded — every one of those is "cannot say", and every
/// consumer must treat it that way.
pub fn class_id_of_name(name: &str) -> Option<ClassId> {
    if !name_index_wanted() || name.is_empty() {
        return None;
    }
    let idx = name_index();
    let mut i = name_hash(name);
    for _ in 0..64 {
        let p = idx[i].load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: as in `note_class_name` — published entries are immortal.
        let e = unsafe { &*p };
        if &*e.name == name {
            return Some(ClassId::new(e.class_id));
        }
        i = (i + 1) & (NAME_INDEX_SLOTS - 1);
    }
    None
}

/// `(entries, name collisions)` — how much of the index is usable.
///
/// A non-zero collision count means at least one name was loaded by two
/// different loaders and this index answers with the FIRST, so a reader can
/// discount a diagnostic that depended on it.
pub fn name_index_shape() -> (usize, usize) {
    (
        NAME_INDEX_LIVE.load(Ordering::Relaxed),
        NAME_INDEX_COLLISIONS.load(Ordering::Relaxed),
    )
}

/// Total heap bytes held by all published type maps.
pub fn store_heap_bytes() -> usize {
    STORE_HEAP_BYTES.load(Ordering::Relaxed)
}

/// Number of classes with published type maps.
pub fn store_class_count() -> usize {
    STORE_CLASS_COUNT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn oref(n: &str) -> VType {
        VType::ObjectRef(Arc::from(n))
    }

    // -- Class-name index ----------------------------------------------------

    /// The index answers what the oop-map oracle asks it: a binary class name,
    /// back to the `ClassId` whose maps describe it.
    #[test]
    fn the_name_index_round_trips_a_class_name() {
        NAME_INDEX_FORCED_ON.store(true, Ordering::Relaxed);
        let id = ClassId::new(0x51_0001);
        note_class_name(id, "org/h2/mvstore/MVStore$NameIndexRoundTrip");
        assert_eq!(
            class_id_of_name("org/h2/mvstore/MVStore$NameIndexRoundTrip"),
            Some(id)
        );
        assert_eq!(class_id_of_name("org/h2/mvstore/NeverLoaded"), None);
    }

    /// Two loaders' versions of one class collapse onto the FIRST, and the
    /// collision is COUNTED.
    ///
    /// A name-keyed index cannot tell them apart, and the consumer is a
    /// diagnostic that would otherwise read the second class's frame against
    /// the first class's maps and report a coverage gap that is really a
    /// loader mix-up. Counting it is what lets a reader discount the run;
    /// silently answering with the first is what would make the diagnostic
    /// lie.
    #[test]
    fn a_duplicate_class_name_keeps_the_first_and_counts_the_collision() {
        NAME_INDEX_FORCED_ON.store(true, Ordering::Relaxed);
        let first = ClassId::new(0x51_0002);
        let second = ClassId::new(0x51_0003);
        note_class_name(first, "com/example/TwoLoaders");
        let (_, before) = name_index_shape();
        note_class_name(second, "com/example/TwoLoaders");
        let (_, after) = name_index_shape();
        assert_eq!(
            class_id_of_name("com/example/TwoLoaders"),
            Some(first),
            "first writer wins"
        );
        assert_eq!(
            after,
            before + 1,
            "and the collision is counted, not hidden"
        );
    }

    // -- Row encoding --------------------------------------------------------

    #[test]
    fn refs_in_locals_are_recorded() {
        let mut b = MethodTypeMapsBuilder::new(4, 2);
        b.record_slots(
            0,
            &[
                oref("java/lang/String"),
                VType::Int,
                VType::Null,
                VType::Top,
            ],
            &[],
        );
        let m = b.finish(true);

        let (locals, stack) = m.oop_map_at(0).expect("map at pc 0");
        assert_eq!(locals.len(), 4);
        assert!(locals.get(0), "slot 0 is an ObjectRef");
        assert!(!locals.get(1), "slot 1 is Int");
        assert!(locals.get(2), "slot 2 is Null — still a reference slot");
        assert!(!locals.get(3), "slot 3 is Top");
        assert!(stack.is_all_clear());
        assert_eq!(locals.iter_set().collect::<Vec<_>>(), vec![0, 2]);
        // Out-of-range reads answer false rather than panicking.
        assert!(!locals.get(99));
    }

    #[test]
    fn uninitialized_this_counts_as_a_reference() {
        // Between `new` and `<init>` the slot holds a real heap pointer, so a
        // moving collector MUST see it.
        let mut b = MethodTypeMapsBuilder::new(2, 2);
        b.record_slots(0, &[VType::UninitializedThis, VType::Uninitialized(7)], &[]);
        let m = b.finish(true);
        let locals = m.local_oops_at(0).unwrap();
        assert!(locals.get(0));
        assert!(locals.get(1));
    }

    #[test]
    fn long_spans_two_local_slots_and_one_stack_slot() {
        // locals: [ref, Long, Top, Int]  — JVMS slot space, cat-2 = 2 slots.
        // stack:  [ref, Long, Top]       — JVMS; runtime is [ref, long] = 2.
        let mut b = MethodTypeMapsBuilder::new(4, 3);
        b.record_slots(
            0,
            &[oref("Foo"), VType::Long, VType::Top, VType::Int],
            &[oref("Bar"), VType::Long, VType::Top],
        );
        let m = b.finish(true);

        let map = m.frame_map_at(0).expect("map at pc 0");
        // Locals keep the JVMS two-slot layout, matching `Frame::locals`.
        assert!(map.locals.get(0));
        assert!(!map.locals.get(1), "Long is not a reference");
        assert!(
            !map.locals.get(2),
            "upper half of a Long is not a reference"
        );
        assert!(!map.locals.get(3));

        // The operand stack is compressed: the Long/Top pair is ONE runtime
        // slot, so the depth is 2, not 3.
        assert_eq!(map.stack_depth, 2);
        assert!(map.stack.get(0), "Bar is a reference at runtime slot 0");
        assert!(!map.stack.get(1), "the long occupies runtime slot 1");
        assert_eq!(m.stack_depth_at(0), Some(2));
        assert!(m.safe_for_fast_path());
    }

    #[test]
    fn malformed_category_two_pair_vetoes() {
        let mut b = MethodTypeMapsBuilder::new(1, 2);
        // `Long` without its `Top` upper half.
        b.record_slots(0, &[VType::Top], &[VType::Long, VType::Int]);
        let m = b.finish(true);
        assert!(!m.safe_for_fast_path());
        assert_eq!(m.fast_path_veto(), Some(FastPathVeto::MalformedCategory2));
    }

    // -- Merge / handler shapes ---------------------------------------------

    #[test]
    fn branch_target_merge_narrows_to_the_merged_type() {
        // Two predecessors reach pc 8 with `String` and `Integer` in local 1;
        // the verifier merges to `Object`, which is still a reference, so the
        // map at the merge point must keep the bit set. Local 2 merges a ref
        // with an Int, which lands on `Top` — the bit must be CLEAR there,
        // because a moving collector must not treat an int as a pointer.
        let mut b = MethodTypeMapsBuilder::new(3, 1);
        b.record_slots(0, &[oref("A"), oref("java/lang/String"), oref("A")], &[]);
        b.record_slots(4, &[oref("A"), oref("java/lang/Integer"), VType::Int], &[]);
        // pc 8 = the merge point.
        b.record_slots(8, &[oref("A"), oref("java/lang/Object"), VType::Top], &[]);
        let m = b.finish(true);

        assert_eq!(m.entry_count(), 3);
        let merged = m.local_oops_at(8).unwrap();
        assert!(merged.get(0));
        assert!(merged.get(1), "merged reference stays a reference");
        assert!(!merged.get(2), "ref ⊔ int = Top, which is NOT scannable");

        // Each predecessor keeps its own row.
        assert!(m.local_oops_at(0).unwrap().get(2));
        assert!(!m.local_oops_at(4).unwrap().get(2));
        // An address that is not an instruction start is unproven.
        assert!(m.oop_map_at(6).is_none());
        assert!(m.oop_map_at(9).is_none());
    }

    #[test]
    fn exception_handler_entry_has_the_thrown_object_on_the_stack() {
        // A handler entry's frame is `locals unchanged, stack = [throwable]`.
        let mut b = MethodTypeMapsBuilder::new(2, 2);
        b.record_slots(0, &[oref("Self"), VType::Int], &[]);
        b.record_slots(
            12,
            &[oref("Self"), VType::Int],
            &[oref("java/lang/Throwable")],
        );
        let m = b.finish(true);

        let handler = m.frame_map_at(12).expect("handler map");
        assert_eq!(handler.stack_depth, 1);
        assert!(handler.stack.get(0), "the caught throwable is a root");
        assert!(handler.locals.get(0));
        assert!(!handler.locals.get(1));
        // Depth 0 at method entry.
        assert_eq!(m.stack_depth_at(0), Some(0));
    }

    // -- safe_for_fast_path --------------------------------------------------

    #[test]
    fn subroutine_method_is_not_safe_for_fast_path() {
        let mut b = MethodTypeMapsBuilder::new(4, 2);
        b.record_slots(0, &vec![VType::Top; 4], &[]);
        b.observe_instruction(&Instruction::Jsr(10));
        let m = b.finish(true);
        assert!(
            !m.safe_for_fast_path(),
            "jsr/ret must veto the unchecked fast path"
        );
        assert_eq!(m.fast_path_veto(), Some(FastPathVeto::Subroutine));
        // The rows are still usable for GC scanning.
        assert!(m.oop_map_at(0).is_some());
    }

    #[test]
    fn out_of_range_local_index_is_not_safe_for_fast_path() {
        let mut b = MethodTypeMapsBuilder::new(2, 2);
        b.record_slots(0, &[VType::Top, VType::Top], &[]);
        // `astore 5` with max_locals = 2 would index `Frame::locals` out of
        // bounds through `set_local_unchecked`.
        b.observe_instruction(&Instruction::Astore(5));
        let m = b.finish(true);
        assert!(!m.safe_for_fast_path());
        assert_eq!(m.fast_path_veto(), Some(FastPathVeto::LocalIndexOutOfRange));
    }

    #[test]
    fn category_two_local_needs_both_slots_in_range() {
        // max_locals = 2: `lstore 1` writes slots 1 and 2 → slot 2 is OOB.
        let mut b = MethodTypeMapsBuilder::new(2, 2);
        b.record_slots(0, &[VType::Top, VType::Top], &[]);
        b.observe_instruction(&Instruction::Lstore(1));
        assert!(b.is_vetoed());
        assert!(!b.finish(true).safe_for_fast_path());

        // max_locals = 3: slots 1 and 2 are both in range.
        let mut ok = MethodTypeMapsBuilder::new(3, 2);
        ok.record_slots(0, &vec![VType::Top; 3], &[]);
        ok.observe_instruction(&Instruction::Lstore(1));
        assert!(ok.finish(true).safe_for_fast_path());
    }

    #[test]
    fn incomplete_walk_is_not_safe_for_fast_path() {
        let mut b = MethodTypeMapsBuilder::new(1, 1);
        b.record_slots(0, &[VType::Top], &[]);
        let m = b.finish(/* walk_complete */ false);
        assert!(!m.safe_for_fast_path());
        assert_eq!(m.fast_path_veto(), Some(FastPathVeto::IncompleteWalk));
    }

    #[test]
    fn plain_method_is_safe_for_fast_path() {
        let mut b = MethodTypeMapsBuilder::new(3, 2);
        b.record_slots(0, &[oref("Self"), VType::Int, VType::Top], &[]);
        b.observe_instruction(&Instruction::Aload(0));
        b.record_slots(1, &[oref("Self"), VType::Int, VType::Top], &[oref("Self")]);
        b.observe_instruction(&Instruction::Astore(2));
        let m = b.finish(true);
        assert!(m.safe_for_fast_path());
        assert_eq!(m.fast_path_veto(), None);
    }

    #[test]
    fn unsorted_pcs_discard_the_map_entirely() {
        let mut b = MethodTypeMapsBuilder::new(1, 1);
        b.record_slots(10, &[VType::Top], &[]);
        b.record_slots(4, &[VType::Top], &[]);
        let m = b.finish(true);
        assert_eq!(m.entry_count(), 0, "a wrong oop map is worse than none");
        assert!(!m.safe_for_fast_path());
        assert_eq!(m.fast_path_veto(), Some(FastPathVeto::UnsortedPcs));
        assert!(m.oop_map_at(10).is_none());
    }

    #[test]
    fn duplicate_pc_records_are_idempotent() {
        let mut b = MethodTypeMapsBuilder::new(1, 1);
        b.record_slots(0, &[oref("A")], &[]);
        b.record_slots(0, &[VType::Int], &[]);
        let m = b.finish(true);
        assert_eq!(m.entry_count(), 1);
        assert!(m.local_oops_at(0).unwrap().get(0), "first record wins");
    }

    // -- Clamping contract ---------------------------------------------------

    #[test]
    fn stack_clamps_to_the_runtime_depth() {
        // At an invoke pc the verifier still sees the arguments on the stack,
        // but the interpreter has already popped them into the callee frame.
        let mut b = MethodTypeMapsBuilder::new(1, 4);
        b.record_slots(
            0,
            &[VType::Top],
            &[oref("Recv"), oref("Arg0"), oref("Arg1")],
        );
        let m = b.finish(true);
        let map = m.frame_map_at(0).unwrap();
        assert_eq!(map.stack_depth, 3);
        // While the callee runs, the caller's real depth is 0.
        let live = map.stack_for_runtime_depth(0);
        assert_eq!(live.len(), 0);
        assert_eq!(live.iter_set().count(), 0);
        // Halfway (a hypothetical partial pop) still reads only real slots.
        assert_eq!(map.stack_for_runtime_depth(1).iter_set().count(), 1);
        // A runtime depth larger than the map never over-reads.
        assert_eq!(map.stack_for_runtime_depth(99).len(), 3);
    }

    #[test]
    fn locals_clamp_to_the_runtime_array_length() {
        let mut b = MethodTypeMapsBuilder::new(8, 1);
        b.record_slots(
            0,
            &[
                oref("A"),
                oref("B"),
                VType::Int,
                VType::Top,
                VType::Top,
                VType::Top,
                VType::Top,
                oref("C"),
            ],
            &[],
        );
        let m = b.finish(true);
        let map = m.frame_map_at(0).unwrap();
        assert_eq!(map.locals.iter_set().collect::<Vec<_>>(), vec![0, 1, 7]);
        assert_eq!(
            map.locals_for_runtime_len(2).iter_set().collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    // -- Sizing --------------------------------------------------------------

    #[test]
    fn representation_is_compact() {
        // 20 instruction starts, 6 locals, max_stack 4 → 1-byte strides,
        // u16 pcs, u8 depths.
        let mut b = MethodTypeMapsBuilder::new(6, 4);
        for i in 0..20u32 {
            b.record_slots(i * 2, &vec![oref("A"); 6], &[]);
        }
        let m = b.finish(true);
        assert_eq!(m.entry_count(), 20);
        // 20*2 (pcs) + 20*1 (locals) + 20*1 (stack) + 20*1 (depths) = 100.
        assert_eq!(m.heap_bytes(), 100);
        // ~5 bytes of row data per instruction start, plus a fixed header.
        assert!(m.heap_bytes() / m.entry_count() <= 5);
        assert!(m.total_bytes() < 400);
    }

    #[test]
    fn wide_locals_use_a_wider_stride() {
        let mut b = MethodTypeMapsBuilder::new(70, 9);
        b.record_slots(0, &vec![VType::Int; 70], &[]);
        let m = b.finish(true);
        // ceil(70/8) = 9 local bytes, ceil(9/8) = 2 stack bytes, 2 pc, 1 depth.
        assert_eq!(m.heap_bytes(), 9 + 2 + 2 + 1);
        assert_eq!(m.local_oops_at(0).unwrap().len(), 70);
    }

    #[test]
    fn short_locals_row_flags_the_method_as_not_fully_described() {
        let mut b = MethodTypeMapsBuilder::new(4, 1);
        b.record_slots(0, &[oref("A"), VType::Int], &[]); // only 2 of 4 slots
        let m = b.finish(true);
        assert!(
            !m.locals_fully_described(),
            "an undescribed local tail must be advertised, not silently zeroed"
        );
        // A fully padded row keeps the flag set.
        let mut ok = MethodTypeMapsBuilder::new(4, 1);
        ok.record_slots(0, &[oref("A"), VType::Int, VType::Top, VType::Top], &[]);
        assert!(ok.finish(true).locals_fully_described());
    }

    #[test]
    fn zero_locals_and_zero_stack_allocate_nothing() {
        let mut b = MethodTypeMapsBuilder::new(0, 0);
        b.record_slots(0, &[], &[]);
        let m = b.finish(true);
        assert_eq!(m.entry_count(), 1);
        let (l, s) = m.oop_map_at(0).unwrap();
        assert!(l.is_empty() && s.is_empty());
        assert_eq!(m.stack_depth_at(0), Some(0));
        // pcs (2) + depths (1) only.
        assert_eq!(m.heap_bytes(), 3);
    }

    #[test]
    fn out_of_spec_pcs_widen_to_u32() {
        let mut b = MethodTypeMapsBuilder::new(1, 1);
        b.record_slots(0, &[VType::Top], &[]);
        b.record_slots(70_000, &[oref("A")], &[]);
        let m = b.finish(true);
        assert!(m.local_oops_at(70_000).unwrap().get(0));
        assert!(m.oop_map_at(1).is_none());
        // 2 pcs × 4 bytes now, plus 2 rows × (1 local + 1 stack + 1 depth).
        assert_eq!(m.heap_bytes(), 8 + 2 + 2 + 2);
    }

    // -- Store ---------------------------------------------------------------

    fn tiny_maps(safe: bool) -> MethodTypeMaps {
        let mut b = MethodTypeMapsBuilder::new(1, 1);
        b.record_slots(0, &[oref("A")], &[]);
        if !safe {
            b.mark_unsafe_for_fast_path(FastPathVeto::Subroutine);
        }
        b.finish(true)
    }

    #[test]
    fn store_round_trips_by_index_and_by_name() {
        let id = ClassId::new(900_001);
        let entries = vec![
            (
                Arc::<str>::from("<init>"),
                Arc::<str>::from("()V"),
                Some(tiny_maps(true)),
            ),
            (
                Arc::<str>::from("run"),
                Arc::<str>::from("()V"),
                Some(tiny_maps(false)),
            ),
            // An abstract/native method contributes an index but no maps.
            (Arc::<str>::from("nat"), Arc::<str>::from("()I"), None),
        ];
        assert!(publish_class_type_maps(id, ClassTypeMaps::new(entries)));

        assert_eq!(verification_status(id), VerificationStatus::Verified);
        assert!(type_maps_for(id, 0).unwrap().safe_for_fast_path());
        assert!(!type_maps_for(id, 1).unwrap().safe_for_fast_path());
        assert!(type_maps_for(id, 2).is_none(), "no Code attribute");
        assert!(type_maps_for(id, 3).is_none(), "index out of range");

        assert!(type_maps_for_named(id, "run", "()V").is_some());
        assert!(
            type_maps_for_named(id, "run", "()I").is_none(),
            "descriptor must match too"
        );
        assert_eq!(
            class_type_maps(id).unwrap().method_index_of("nat", "()I"),
            Some(2)
        );
        assert!(class_type_maps(id).unwrap().heap_bytes() > 0);
    }

    #[test]
    fn unknown_class_is_unknown_not_empty() {
        let id = ClassId::new(900_777);
        assert_eq!(verification_status(id), VerificationStatus::Unknown);
        assert!(class_type_maps(id).is_none());
        assert!(type_maps_for(id, 0).is_none());
    }

    #[test]
    fn skipped_verification_is_distinguishable_from_verified() {
        let id = ClassId::new(900_055);
        assert!(mark_class_verification_skipped(id));
        assert_eq!(verification_status(id), VerificationStatus::Skipped);
        // A skipped class yields NO maps — never a wrong-but-plausible one.
        assert!(type_maps_for(id, 0).is_none());
        assert!(type_maps_for_named(id, "run", "()V").is_none());

        // If the class is later verified for real, the marker is upgraded.
        let entries = vec![(
            Arc::<str>::from("run"),
            Arc::<str>::from("()V"),
            Some(tiny_maps(true)),
        )];
        assert!(publish_class_type_maps(id, ClassTypeMaps::new(entries)));
        assert_eq!(verification_status(id), VerificationStatus::Verified);
        assert!(type_maps_for(id, 0).is_some());
    }

    #[test]
    fn duplicate_publish_keeps_the_winner() {
        let id = ClassId::new(900_123);
        let mk = || {
            ClassTypeMaps::new(vec![(
                Arc::<str>::from("m"),
                Arc::<str>::from("()V"),
                Some(tiny_maps(true)),
            )])
        };
        assert!(publish_class_type_maps(id, mk()));
        assert!(
            !publish_class_type_maps(id, mk()),
            "second publish is a no-op"
        );
        assert!(type_maps_for(id, 0).is_some());

        // Redefinition explicitly replaces.
        assert!(replace_class_type_maps(
            id,
            ClassTypeMaps::new(vec![(
                Arc::<str>::from("m"),
                Arc::<str>::from("()V"),
                Some(tiny_maps(false)),
            )])
        ));
        assert!(!type_maps_for(id, 0).unwrap().safe_for_fast_path());
    }

    #[test]
    fn class_ids_beyond_the_addressable_range_degrade_gracefully() {
        let id = ClassId::new(u32::MAX);
        assert_eq!(verification_status(id), VerificationStatus::Unknown);
        assert!(!mark_class_verification_skipped(id));
        assert!(type_maps_for(id, 0).is_none());
    }
}
