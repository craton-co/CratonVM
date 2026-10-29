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
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cratonvm_reader::instruction::Instruction;
use cratonvm_types::{ClassId, ThinArcStr};

use super::verify_frame::VerificationFrame;
use super::vtype::{ClassHierarchy, VType};

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

/// zs s1/fp (2026-10-14): a method's four row tables -- the pc table, the
/// locals rows, the stack rows and the depth table -- live in ONE allocation
/// ([`MethodTypeMaps::data`]), in that order. Before, each was its own boxed
/// slice behind its own enum or struct header: a 120-byte `MethodTypeMaps`
/// and four heap blocks per method, for row data that is ~100 bytes on a
/// typical method. The deep census
/// (`classloading/tests/zr8_fm_class_metadata_census.rs`,
/// `zs1fp-map store_type_maps`) put the verifier type maps at ~3.1 KB of a
/// java.base class's ~16.4 KB; most of it was those headers.
///
/// Row bytes and their order are unchanged (byte stride, `u16` pcs unless
/// out of spec, `u8` depths unless a depth exceeds 255); only where they live
/// moved. Multi-byte entries are read with `from_ne_bytes`, so the buffer
/// needs no alignment.
#[derive(Clone, Copy)]
struct RowLayout {
    rows: usize,
    pc_width: usize,
    local_stride: usize,
    stack_stride: usize,
    depth_width: usize,
}

impl RowLayout {
    #[inline]
    fn locals_at(&self) -> usize {
        self.rows * self.pc_width
    }

    #[inline]
    fn stack_at(&self) -> usize {
        self.locals_at() + self.rows * self.local_stride
    }

    #[inline]
    fn depths_at(&self) -> usize {
        self.stack_at() + self.rows * self.stack_stride
    }

    #[inline]
    fn len(&self) -> usize {
        self.depths_at() + self.rows * self.depth_width
    }
}

/// [`MethodTypeMaps::flags`]: the pc table is `u32` (out-of-spec code).
const TM_PC_WIDE: u8 = 1;
/// [`MethodTypeMaps::flags`]: the depth table is `u16` (a depth over 255).
const TM_DEPTH_WIDE: u8 = 2;
/// [`MethodTypeMaps::flags`]: every recorded row described all `max_locals`.
const TM_LOCALS_COMPLETE: u8 = 4;

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
#[derive(Clone)]
pub struct MethodTypeMaps {
    /// zs s1/fp: the pc table, then the locals rows, then the stack rows,
    /// then the depth table, in one allocation (see [`RowLayout`]). Empty
    /// when no row was recorded.
    data: Box<[u8]>,
    /// Recorded instruction starts (rows).
    rows: u32,
    max_locals: u16,
    max_stack: u16,
    /// `TM_PC_WIDE` | `TM_DEPTH_WIDE` | `TM_LOCALS_COMPLETE`.
    flags: u8,
    /// The first fast-path veto; `None` iff the method is safe for the
    /// interpreter fast path.
    veto: Option<FastPathVeto>,
}

impl MethodTypeMaps {
    /// Number of recorded instruction starts.
    #[inline]
    pub fn entry_count(&self) -> usize {
        self.rows as usize
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
        self.veto.is_none()
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
        self.flags & TM_LOCALS_COMPLETE != 0
    }

    /// Index of `pc` in the recorded pc table. `O(log n)`.
    #[inline]
    pub fn index_of_pc(&self, pc: u32) -> Option<usize> {
        self.search_pc(pc)
    }

    /// Is `pc` a recorded instruction start?
    #[inline]
    pub fn has_pc(&self, pc: u32) -> bool {
        self.search_pc(pc).is_some()
    }

    /// The `n`-th recorded pc, ascending.
    #[inline]
    pub fn pc_at_index(&self, index: usize) -> Option<u32> {
        (index < self.rows as usize).then(|| self.pc_unchecked(index))
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
        let idx = self.search_pc(pc)?;
        Some((self.local_row(idx), self.stack_row(idx)))
    }

    /// Like [`Self::oop_map_at`] but also carries the pc and the verified
    /// operand-stack depth. Prefer this in a root scan: the depth is what you
    /// clamp against.
    #[inline]
    pub fn frame_map_at(&self, pc: u32) -> Option<FrameOopMap<'_>> {
        let idx = self.search_pc(pc)?;
        Some(FrameOopMap {
            pc,
            locals: self.local_row(idx),
            stack: self.stack_row(idx),
            stack_depth: self.depth(idx).unwrap_or(0),
        })
    }

    /// Local-slot oop bits at `pc`. See [`Self::oop_map_at`].
    #[inline]
    pub fn local_oops_at(&self, pc: u32) -> Option<LocalOopBits<'_>> {
        let idx = self.search_pc(pc)?;
        Some(self.local_row(idx))
    }

    /// Operand-stack oop bits at `pc`. See [`Self::oop_map_at`].
    #[inline]
    pub fn stack_oops_at(&self, pc: u32) -> Option<StackOopBits<'_>> {
        let idx = self.search_pc(pc)?;
        Some(self.stack_row(idx))
    }

    /// Verified operand-stack depth at `pc`, in runtime slot space.
    #[inline]
    pub fn stack_depth_at(&self, pc: u32) -> Option<u16> {
        let idx = self.search_pc(pc)?;
        self.depth(idx)
    }

    /// zs s1/fp: where each table sits in [`Self::data`].
    #[inline]
    fn layout(&self) -> RowLayout {
        let rows = self.rows as usize;
        RowLayout {
            rows,
            pc_width: if self.flags & TM_PC_WIDE != 0 { 4 } else { 2 },
            local_stride: if rows == 0 {
                0
            } else {
                stride_for(self.max_locals)
            },
            stack_stride: if rows == 0 {
                0
            } else {
                stride_for(self.max_stack)
            },
            depth_width: if self.flags & TM_DEPTH_WIDE != 0 {
                2
            } else {
                1
            },
        }
    }

    /// The `index`-th pc; `index < rows`.
    #[inline]
    fn pc_unchecked(&self, index: usize) -> u32 {
        let d = &self.data;
        if self.flags & TM_PC_WIDE != 0 {
            let i = index * 4;
            u32::from_ne_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
        } else {
            let i = index * 2;
            u16::from_ne_bytes([d[i], d[i + 1]]) as u32
        }
    }

    /// Exact-match binary search over the pc table. `O(log n)`,
    /// allocation-free, no panics.
    #[inline]
    fn search_pc(&self, pc: u32) -> Option<usize> {
        if self.flags & TM_PC_WIDE == 0 && pc > u16::MAX as u32 {
            return None;
        }
        let (mut lo, mut hi) = (0usize, self.rows as usize);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.pc_unchecked(mid).cmp(&pc) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Row `index` of a bitmap table at `at` with `stride` bytes a row and
    /// `bits` slots. Out of range, or a zero stride, answers an empty view
    /// rather than panicking -- a GC scan must degrade, not abort.
    #[inline]
    fn bit_row(&self, at: usize, stride: usize, bits: u16, index: usize) -> OopBits<'_> {
        if stride == 0 || index >= self.rows as usize {
            return OopBits {
                bytes: &[],
                bits: 0,
            };
        }
        let start = at + index * stride;
        OopBits::new(&self.data[start..start + stride], bits)
    }

    #[inline]
    fn local_row(&self, index: usize) -> LocalOopBits<'_> {
        let l = self.layout();
        self.bit_row(l.locals_at(), l.local_stride, self.max_locals, index)
    }

    #[inline]
    fn stack_row(&self, index: usize) -> StackOopBits<'_> {
        let l = self.layout();
        self.bit_row(l.stack_at(), l.stack_stride, self.max_stack, index)
    }

    #[inline]
    fn depth(&self, index: usize) -> Option<u16> {
        if index >= self.rows as usize {
            return None;
        }
        let l = self.layout();
        let at = l.depths_at() + index * l.depth_width;
        let d = &self.data;
        Some(if l.depth_width == 2 {
            u16::from_ne_bytes([d[at], d[at + 1]])
        } else {
            d[at] as u16
        })
    }

    /// Bytes of heap owned by this map, excluding the struct header.
    ///
    /// Mirrors the established `heap_bytes()` accounting convention so a
    /// whole-VM footprint report can sum type-map cost alongside the other
    /// per-method side tables.
    pub fn heap_bytes(&self) -> usize {
        self.data.len()
    }

    /// `heap_bytes()` plus the struct header — what this map costs in total.
    pub fn total_bytes(&self) -> usize {
        self.heap_bytes() + std::mem::size_of::<MethodTypeMaps>()
    }

    /// zs s3/fp3: drop the per-pc rows and keep the method's verdicts
    /// ([`Self::safe_for_fast_path`], [`Self::fast_path_veto`],
    /// [`Self::locals_fully_described`], `max_locals` / `max_stack`). Every
    /// row query then answers `None` at every pc: "unproven, scan
    /// conservatively", the answer each consumer already handles (an
    /// unrecorded pc, a skipped region, an over-budget method).
    pub fn strip_rows(&mut self) {
        self.data = Box::new([]);
        self.rows = 0;
        self.flags &= TM_LOCALS_COMPLETE;
    }
}

impl std::fmt::Debug for MethodTypeMaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MethodTypeMaps")
            .field("entries", &self.entry_count())
            .field("max_locals", &self.max_locals)
            .field("max_stack", &self.max_stack)
            .field("locals_complete", &self.locals_fully_described())
            .field("safe_for_fast_path", &self.safe_for_fast_path())
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
    /// Epoch of the locals the most recently written locals row was derived
    /// from (`None` when that row came from a plain slice), and their length.
    /// See [`MethodTypeMapsBuilder::record`].
    last_row_epoch: Option<u64>,
    last_row_len: usize,
    /// Phase 5 fast boot (lane p5-define): keep the verdicts only. Every
    /// veto, `locals_complete` and the sort check are decided exactly as when
    /// the rows are kept (the row budget is charged by the bytes the rows
    /// WOULD take), but no row byte is stored, and [`Self::finish`] returns
    /// what [`MethodTypeMaps::strip_rows`] leaves of the full maps. For a
    /// walk whose store strips the rows at install
    /// (`TypeMapStore::drops_rows`): the installed maps are the same.
    verdict_only: bool,
    /// Verdict-only: the pc of the last row (the sort / repeat check).
    last_pc: Option<u32>,
    /// Verdict-only: the row bytes the kept rows would hold (the budget).
    row_bytes: usize,
}

#[inline]
fn stride_for(bits: u16) -> usize {
    ((bits as usize) + 7) / 8
}

/// JIT round 23 wave 5 (proxy-define page; `CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE`,
/// default ON, `=0` kill): the four row vectors of a finished
/// [`MethodTypeMapsBuilder`], emptied and kept with their capacity, so the next
/// method of the same class walk ([`MethodTypeMapsBuilder::with_spare`]) does not
/// allocate them again. Every builder produced its rows into fresh vectors and
/// then freed them at [`MethodTypeMapsBuilder::finish`]: four allocations and
/// four frees per method, ~2 % of the proxy row through the Windows heap. The
/// maps themselves are copied out by `finish` exactly as before.
#[derive(Default)]
pub struct TypeMapSpare {
    pcs: Vec<u32>,
    local_bytes: Vec<u8>,
    stack_bytes: Vec<u8>,
    depths: Vec<u16>,
}

impl MethodTypeMapsBuilder {
    /// [`Self::new`] reusing `spare`'s emptied vectors (their capacity only;
    /// their contents are cleared). `spare` is left empty.
    pub fn with_spare(max_locals: u16, max_stack: u16, spare: &mut TypeMapSpare) -> Self {
        let mut builder = Self::new(max_locals, max_stack);
        builder.pcs = std::mem::take(&mut spare.pcs);
        builder.local_bytes = std::mem::take(&mut spare.local_bytes);
        builder.stack_bytes = std::mem::take(&mut spare.stack_bytes);
        builder.depths = std::mem::take(&mut spare.depths);
        builder.pcs.clear();
        builder.local_bytes.clear();
        builder.stack_bytes.clear();
        builder.depths.clear();
        builder
    }

    /// [`Self::finish`], handing the emptied row vectors back to `spare` for
    /// the next builder ([`Self::with_spare`]). The returned maps are
    /// identical: `finish` copies the rows out before the vectors move.
    pub fn finish_into_spare(
        self,
        walk_complete: bool,
        spare: &mut TypeMapSpare,
    ) -> MethodTypeMaps {
        let (maps, rest) = self.finish_keep(walk_complete);
        *spare = rest;
        maps
    }
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
            last_row_epoch: None,
            last_row_len: 0,
            verdict_only: false,
            last_pc: None,
            row_bytes: 0,
        }
    }

    /// Keep the verdicts only (see the `verdict_only` field). Call before the
    /// first [`Self::record`].
    pub fn set_verdict_only(&mut self) {
        debug_assert!(self.pcs.is_empty(), "set before the first row");
        self.verdict_only = true;
    }

    /// The verdict-only form of [`Self::record_inner`]: the same vetoes and
    /// flags, decided from the same inputs, with no row stored.
    fn record_verdict(&mut self, pc: u32, locals_len: usize, stack: &[VType]) {
        if let Some(last) = self.last_pc {
            if pc == last {
                return;
            }
            if pc < last {
                self.sorted = false;
                self.mark_unsafe_for_fast_path(FastPathVeto::UnsortedPcs);
                return;
            }
        }
        if self.row_bytes + self.local_stride + self.stack_stride > MAX_METHOD_ROW_BYTES {
            self.mark_unsafe_for_fast_path(FastPathVeto::MapBudgetExceeded);
            return;
        }
        if locals_len > self.max_locals as usize {
            self.mark_unsafe_for_fast_path(FastPathVeto::LocalsWiderThanMaxLocals);
        }
        if locals_len < self.max_locals as usize {
            self.locals_complete = false;
        }
        let mut rt: usize = 0;
        let mut i: usize = 0;
        while i < stack.len() {
            let ty = &stack[i];
            if rt >= self.max_stack as usize {
                self.mark_unsafe_for_fast_path(FastPathVeto::StackDepthExceedsMaxStack);
            }
            if ty.is_category2() {
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
        self.row_bytes += self.local_stride + self.stack_stride;
        self.last_pc = Some(pc);
    }

    /// Pre-size the row vectors for a method with `insn_hint` instructions.
    ///
    /// Reservations are clamped to [`MAX_METHOD_ROW_BYTES`] so an
    /// attacker-controlled `max_locals`/`max_stack` cannot turn a hint into a
    /// hundreds-of-megabytes allocation.
    pub fn reserve(&mut self, insn_hint: usize) {
        if self.verdict_only {
            return;
        }
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
        self.record_inner(pc, &frame.locals, Some(frame.locals.epoch()), &frame.stack);
    }

    /// Slice-level form of [`Self::record`], so the row encoding can be unit
    /// tested without constructing a whole [`VerificationFrame`].
    pub fn record_slots(&mut self, pc: u32, locals: &[VType], stack: &[VType]) {
        self.record_inner(pc, locals, None, stack);
    }

    /// The shared body. `locals_epoch` (from [`crate::verify_frame::Locals`])
    /// is `Some` when the caller can vouch that equal epochs mean equal
    /// contents: the locals row is then copied from the previous row, not
    /// re-derived from every slot, whenever the epoch is the one that row was
    /// derived from.
    fn record_inner(
        &mut self,
        pc: u32,
        locals: &[VType],
        locals_epoch: Option<u64>,
        stack: &[VType],
    ) {
        if self.verdict_only {
            self.record_verdict(pc, locals.len(), stack);
            return;
        }
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
        if locals.len() > self.max_locals as usize {
            self.mark_unsafe_for_fast_path(FastPathVeto::LocalsWiderThanMaxLocals);
        }
        if locals.len() < self.max_locals as usize {
            // The tail bits would read as "not a reference" without ever
            // having been proven so — a missed root for a moving collector.
            // Flag the whole method rather than lie about the tail.
            self.locals_complete = false;
        }
        let reuse_previous_row = base >= self.local_stride
            && !self.pcs.is_empty()
            && locals_epoch.is_some()
            && locals_epoch == self.last_row_epoch
            && locals.len() == self.last_row_len;
        if reuse_previous_row {
            // Same locals as the row just written (same epoch, same length):
            // the bitmap is the same bytes.
            if self.local_stride <= 8 {
                // (a handful of bytes: pushing them is cheaper than the
                // `memmove` call `extend_from_within` makes)
                for k in 0..self.local_stride {
                    let byte = self.local_bytes[base - self.local_stride + k];
                    self.local_bytes.push(byte);
                }
            } else {
                self.local_bytes
                    .extend_from_within(base - self.local_stride..base);
            }
        } else {
            self.local_bytes.resize(base + self.local_stride, 0);
            let n_locals = locals.len().min(self.max_locals as usize);
            let row = &mut self.local_bytes[base..base + self.local_stride];
            for (byte, chunk) in row.iter_mut().zip(locals[..n_locals].chunks(8)) {
                let mut bits = 0u8;
                for (j, ty) in chunk.iter().enumerate() {
                    bits |= (ty.is_reference() as u8) << j;
                }
                *byte = bits;
            }
        }
        self.last_row_epoch = locals_epoch;
        self.last_row_len = locals.len();

        // --- stack row: runtime/compressed slot space -----------------------
        // The verifier models a category-2 value as two entries (`Long`/`Top`);
        // `ValueStack` stores it in one `CompactValue`. Compress here so the
        // emitted bit index is directly usable as a `Frame::stack` index.
        let sbase = self.stack_bytes.len();
        // The row is accumulated in a register when it fits one (every method
        // with `max_stack <= 64`) and appended byte by byte; the wider rows
        // are or-ed into the vector as before.
        let narrow = self.max_stack <= 64;
        let mut acc: u64 = 0;
        if !narrow {
            self.stack_bytes.resize(sbase + self.stack_stride, 0);
        }
        let mut rt: usize = 0;
        let mut i: usize = 0;
        while i < stack.len() {
            let ty = &stack[i];
            if rt < self.max_stack as usize {
                if ty.is_reference() {
                    if narrow {
                        acc |= 1u64 << rt;
                    } else {
                        self.stack_bytes[sbase + (rt >> 3)] |= 1u8 << (rt & 7);
                    }
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
        if narrow {
            for k in 0..self.stack_stride {
                self.stack_bytes.push((acc >> (8 * k)) as u8);
            }
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
    pub fn finish(self, walk_complete: bool) -> MethodTypeMaps {
        self.finish_keep(walk_complete).0
    }

    /// [`Self::finish`], also returning the builder's row vectors, emptied,
    /// as a [`TypeMapSpare`] (see [`Self::finish_into_spare`]).
    fn finish_keep(mut self, walk_complete: bool) -> (MethodTypeMaps, TypeMapSpare) {
        let maps = self.finish_rows(walk_complete);
        self.pcs.clear();
        self.local_bytes.clear();
        self.stack_bytes.clear();
        self.depths.clear();
        let spare = TypeMapSpare {
            pcs: std::mem::take(&mut self.pcs),
            local_bytes: std::mem::take(&mut self.local_bytes),
            stack_bytes: std::mem::take(&mut self.stack_bytes),
            depths: std::mem::take(&mut self.depths),
        };
        (maps, spare)
    }

    /// The body of [`Self::finish`]: copies the rows out, leaving the vectors.
    fn finish_rows(&mut self, walk_complete: bool) -> MethodTypeMaps {
        if !walk_complete {
            self.mark_unsafe_for_fast_path(FastPathVeto::IncompleteWalk);
        }

        if !self.sorted {
            // The binary-search index would be wrong, and a wrong oop map is
            // worse than none: drop every row.
            return MethodTypeMaps {
                data: Box::new([]),
                rows: 0,
                max_locals: self.max_locals,
                max_stack: self.max_stack,
                flags: 0,
                veto: Some(FastPathVeto::UnsortedPcs),
            };
        }

        if self.verdict_only {
            // What `MethodTypeMaps::strip_rows` leaves of the full maps.
            return MethodTypeMaps {
                data: Box::new([]),
                rows: 0,
                max_locals: self.max_locals,
                max_stack: self.max_stack,
                flags: if self.locals_complete {
                    TM_LOCALS_COMPLETE
                } else {
                    0
                },
                veto: self.veto,
            };
        }

        let rows = self.pcs.len();
        let mut flags = 0u8;
        if self.locals_complete {
            flags |= TM_LOCALS_COMPLETE;
        }
        if self.pcs.last().copied().unwrap_or(0) > u16::MAX as u32 {
            flags |= TM_PC_WIDE;
        }
        if self.max_depth_seen > u8::MAX as u16 {
            flags |= TM_DEPTH_WIDE;
        }
        let layout = RowLayout {
            rows,
            pc_width: if flags & TM_PC_WIDE != 0 { 4 } else { 2 },
            local_stride: if rows == 0 { 0 } else { self.local_stride },
            stack_stride: if rows == 0 { 0 } else { self.stack_stride },
            depth_width: if flags & TM_DEPTH_WIDE != 0 { 2 } else { 1 },
        };
        // One exact-size allocation (zs s1/fp): the builder's vectors carry
        // `reserve` slack, which `into_boxed_slice` used to shed with one
        // reallocation per table.
        let mut data: Vec<u8> = Vec::with_capacity(layout.len());
        for &pc in &self.pcs {
            if layout.pc_width == 4 {
                data.extend_from_slice(&pc.to_ne_bytes());
            } else {
                data.extend_from_slice(&(pc as u16).to_ne_bytes());
            }
        }
        if layout.local_stride != 0 {
            data.extend_from_slice(&self.local_bytes);
        }
        if layout.stack_stride != 0 {
            data.extend_from_slice(&self.stack_bytes);
        }
        for &depth in &self.depths {
            if layout.depth_width == 2 {
                data.extend_from_slice(&depth.to_ne_bytes());
            } else {
                data.push(depth as u8);
            }
        }
        debug_assert_eq!(data.len(), layout.len());

        MethodTypeMaps {
            data: data.into_boxed_slice(),
            rows: rows as u32,
            max_locals: self.max_locals,
            max_stack: self.max_stack,
            flags,
            veto: self.veto,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-class side table
// ---------------------------------------------------------------------------

/// Identity of one method within its class, for the by-name lookup path.
///
/// zs s2/fp2: the names are [`ThinArcStr`]s (the same shared `Arc<str>`
/// allocations, 12 bytes each at 4-byte alignment), so a signature is 28
/// bytes, not 40.
#[derive(Clone)]
struct MethodSig {
    hash: u32,
    name: ThinArcStr,
    descriptor: ThinArcStr,
}

/// The verification result for one class: a type map per method (or `None`
/// for abstract / native / bodyless methods).
#[derive(Clone)]
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
                name: ThinArcStr::from(name),
                descriptor: ThinArcStr::from(descriptor),
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

    /// zs s3/fp3: [`MethodTypeMaps::strip_rows`] on every method.
    pub fn strip_rows(&mut self) {
        for m in self.methods.iter_mut().flatten() {
            m.strip_rows();
        }
    }
}

/// zs s3/fp3 (2026-10-14, footprint): does a class store drop the verifier's
/// per-pc rows at publish (`TypeMapStore::set_drop_rows`)?
///
/// On the default path nothing reads a row. The VM's readers are the two
/// `CRATONVM_DBG_VERIFY_OOP_MAPS` comparisons: the interpreter-frame shadow
/// (`vm/src/runtime/frame.rs`, `shadow_map_rows`, behind
/// `local_liveness::oop_map_shadow::enabled`) and the compiled-frame oracle
/// (`vm/src/jit/conservative_roots.rs`, `verifier_local_verdict`, whose store
/// is bound only when the key is present). The one production reader,
/// `interpreter.rs`'s fast-path gate, reads the per-method verdict, which
/// stripping keeps. So the rows (~1.3 KB a java.base class) are kept only
/// when that key is present (any value: the oracle arms on presence), or
/// under the kill switch `CRATONVM_TYPE_MAP_DROP_ROWS=0`. Read per call: a
/// class store calls it once, at construction.
///
/// Interpreter round i6 wave 121's precise interpreter oop maps
/// (`CRATONVM_INTERP_PRECISE_OOP_MAPS`, default OFF, the same
/// `runtime_flag_on` reading as `VmConfig::interp_precise_oop_maps`) read the
/// rows too, so the rows are also kept while that switch is on
/// (`TypeMapStore::set_precise_scan` keeps them for the maps installed after
/// VM creation as well).
pub fn rows_dropped_by_default() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_TYPE_MAP_DROP_ROWS")
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VERIFY_OOP_MAPS").is_none()
        && !cratonvm_types::flags::runtime_flag_on("CRATONVM_INTERP_PRECISE_OOP_MAPS")
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
// Per-VM store
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

/// Process totals over every live [`TypeMapStore`] (each store also keeps its
/// own); a dropped store subtracts what it added.
static STORE_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
static STORE_CLASS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A published `ClassTypeMaps` displaced by a replace or a skipped-marker
/// upgrade. A reader may still hold a borrow of it, so it is freed with its
/// store rather than at the swap.
struct Retired(*mut ClassTypeMaps);

// SAFETY: the pointer is the sole owner of a leaked `Box<ClassTypeMaps>` that
// nothing writes through, and `ClassTypeMaps` is `Send + Sync` (asserted
// below), so moving that ownership to the thread that drops the store is sound.
unsafe impl Send for Retired {}

/// The store hands out references across threads through raw pointers, which
/// bypasses the usual auto-trait checking. Assert the two properties that
/// soundness depends on so a future field addition (an `Rc`, a `Cell`) fails
/// the build instead of racing at runtime.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn store_soundness_assertions() {
        assert_send_sync::<ClassTypeMaps>();
        assert_send_sync::<MethodTypeMaps>();
        assert_send_sync::<Chunk>();
        assert_send_sync::<TypeMapStore>();
    }
};

/// One VM's verifier type maps, keyed by `ClassId`.
///
/// # Why one per VM
///
/// `ClassId` is a per-`ClassStore` index (`ClassStore::next_id`), so two VMs in
/// one process (`libcratonvm` / `cratonvm-embed` embedders, every unit-test
/// binary) number their classes from the same origin. The table used to be a
/// single process-wide one keyed by the id alone: VM B's class `X` was answered
/// with the maps VM A published for ITS class `X`, and first-writer-wins
/// discarded B's own publish. `interpreter::frame_fast_path_admitted` reads
/// these maps to decide whether a method may run on the unchecked fast path,
/// so B could run a method its own verifier vetoed on the strength of A's
/// same-named method.
///
/// Each `ClassStore` now owns one store (`ClassStore::type_maps`), the VM keeps
/// an `Arc` of it outside the class-manager lock (`ClassRealm::type_maps`), and
/// the verifier publishes through [`ClassHierarchy::type_map_store`].
///
/// # The process store
///
/// [`process_store`] is where a publisher with no VM lands (unit tests,
/// tooling, a verifier call whose hierarchy supplies no store) and what the
/// free functions below ([`type_maps_for`], [`publish_class_type_maps`], ...)
/// read and write. A VM store never writes into it. Both diagnostic oracles
/// read the VM's own store: the interpreter-frame one through
/// `Frame::scan_local_objects_mapped` (interpreter round i1 wave 7), the
/// compiled-frame one (`conservative_roots::verifier_local_verdict`) through
/// the store its root pass binds on the scanning thread, keyed by the
/// artifact's owner class id (wave 8). Until wave 8 every VM store mirrored
/// its publishes here, and a process-wide class-NAME index resolved the
/// compiled frame's class, so in a multi-VM process that oracle could read
/// another VM's maps.
///
/// # Reads are lock-free
///
/// The GC root scan may read a store while the world is stopped, and a thread
/// suspended at a safepoint *while holding a lock* would deadlock the
/// collector. A read is two acquire loads and an array index. Maps displaced by
/// a replace are retired, not freed, until the store drops, because a reader
/// may still hold a borrow of them; the borrow checker ties every answer to
/// the `&self` it came from.
pub struct TypeMapStore {
    /// Top-level chunk directory, allocated on the first publish so a store
    /// that never verifies anything (most unit-test class stores) costs no
    /// 32 KiB directory.
    dir: OnceLock<Box<[AtomicPtr<Chunk>]>>,
    retired: Mutex<Vec<Retired>>,
    heap_bytes: AtomicUsize,
    class_count: AtomicUsize,
    /// The VM's `CRATONVM_INTERP_PRECISE_OOP_MAPS` answer (interpreter round
    /// i6 wave 121, lane L7; stage 3 of
    /// `i1-L4-proposal-precise-interpreter-oop-maps`), set once at VM creation
    /// before any frame is scanned. It lives here because every mapped frame
    /// scan and its remap twin already receive this store, so the two read the
    /// same per-VM answer for every collection.
    precise_scan: AtomicBool,
    /// zs s3/fp3: strip the per-pc rows of every map this store installs
    /// ([`ClassTypeMaps::strip_rows`]). `false` for [`TypeMapStore::new`];
    /// a class store sets it from [`rows_dropped_by_default`].
    drop_rows: AtomicBool,
}

impl Default for TypeMapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TypeMapStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TypeMapStore({} classes, {} bytes)",
            self.class_count(),
            self.heap_bytes()
        )
    }
}

impl TypeMapStore {
    /// An empty store for one VM.
    pub fn new() -> Self {
        Self {
            dir: OnceLock::new(),
            retired: Mutex::new(Vec::new()),
            heap_bytes: AtomicUsize::new(0),
            class_count: AtomicUsize::new(0),
            precise_scan: AtomicBool::new(false),
            drop_rows: AtomicBool::new(false),
        }
    }

    /// Whether the interpreter's frame scans and remaps that receive this
    /// store take the verifier's map as authoritative where it has a row
    /// (`CRATONVM_INTERP_PRECISE_OOP_MAPS`; see [`Self::set_precise_scan`]).
    #[inline]
    pub fn precise_scan(&self) -> bool {
        self.precise_scan.load(Ordering::Relaxed)
    }

    /// Set [`Self::precise_scan`]; called once, at VM creation, before any
    /// frame of the VM exists (interpreter round i6 wave 121, lane L7).
    /// The precise scan reads the per-pc rows, so turning it on also keeps
    /// the rows of every map installed from now on (zs s3/fp3's
    /// [`Self::set_drop_rows`] would otherwise leave it no row to read; see
    /// [`rows_dropped_by_default`] for the maps installed before this call).
    pub fn set_precise_scan(&self, on: bool) {
        self.precise_scan.store(on, Ordering::Relaxed);
        if on {
            self.set_drop_rows(false);
        }
    }

    /// zs s3/fp3: strip (`true`) or keep (`false`) the per-pc rows of the
    /// maps installed from now on. Maps already installed are unchanged.
    pub fn set_drop_rows(&self, drop: bool) {
        self.drop_rows.store(drop, Ordering::Relaxed);
    }

    /// zs s3/fp3: does this store strip the rows it installs?
    pub fn drops_rows(&self) -> bool {
        self.drop_rows.load(Ordering::Relaxed)
    }

    #[inline]
    fn chunk_for(&self, class_id: ClassId) -> Option<&Chunk> {
        let ci = (class_id.as_u32() >> CHUNK_SHIFT) as usize;
        let slot = self.dir.get()?.get(ci)?;
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: chunks are allocated with `Box::into_raw` and freed only
            // by `Drop` (`&mut self`), so a non-null pointer read out of the
            // directory is valid for as long as `&self` is.
            Some(unsafe { &*p })
        }
    }

    fn chunk_for_or_create(&self, class_id: ClassId) -> Option<&Chunk> {
        let ci = (class_id.as_u32() >> CHUNK_SHIFT) as usize;
        if ci >= MAX_CHUNKS {
            return None; // class id beyond the addressable range
        }
        let dir = self.dir.get_or_init(|| {
            let mut v = Vec::with_capacity(MAX_CHUNKS);
            for _ in 0..MAX_CHUNKS {
                v.push(AtomicPtr::new(ptr::null_mut()));
            }
            v.into_boxed_slice()
        });
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
            // SAFETY: we published `fresh`; it lives until the store drops.
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
    pub fn class_type_maps(&self, class_id: ClassId) -> Option<&ClassTypeMaps> {
        let chunk = self.chunk_for(class_id)?;
        let p = chunk.slots[(class_id.as_u32() & CHUNK_MASK) as usize].load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: a published `ClassTypeMaps` is freed only by `Drop`
            // (`&mut self`); one displaced by a replace is retired, not freed.
            Some(unsafe { &*p })
        }
    }

    /// Type maps for one method, `O(1)`; see the free [`type_maps_for`].
    #[inline]
    pub fn type_maps_for(&self, class_id: ClassId, method_index: usize) -> Option<&MethodTypeMaps> {
        self.class_type_maps(class_id)?.method(method_index)
    }

    /// Type maps for one method by name and descriptor; see the free
    /// [`type_maps_for_named`].
    #[inline]
    pub fn type_maps_for_named(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<&MethodTypeMaps> {
        self.class_type_maps(class_id)?
            .method_named(name, descriptor)
    }

    /// What is known about `class_id`'s verification.
    #[inline]
    pub fn verification_status(&self, class_id: ClassId) -> VerificationStatus {
        match self.class_type_maps(class_id) {
            None => VerificationStatus::Unknown,
            Some(c) if c.verified => VerificationStatus::Verified,
            Some(_) => VerificationStatus::Skipped,
        }
    }

    /// Publish `maps` for `class_id`: first writer wins, except that real maps
    /// upgrade a "skipped" marker. Returns `false` when verified maps already
    /// existed (concurrent duplicate verification) or when `class_id` is
    /// outside the addressable range.
    pub fn publish(&self, class_id: ClassId, maps: ClassTypeMaps) -> bool {
        self.install(class_id, maps, false)
    }

    /// Replace the maps for `class_id` (JVMTI `RedefineClasses` /
    /// `RetransformClasses`). The previous maps are retired until the store
    /// drops.
    pub fn replace(&self, class_id: ClassId, maps: ClassTypeMaps) -> bool {
        self.install(class_id, maps, true)
    }

    /// Record that verification was deliberately skipped for `class_id`; see
    /// the free [`mark_class_verification_skipped`].
    pub fn mark_skipped(&self, class_id: ClassId) -> bool {
        self.install(class_id, ClassTypeMaps::skipped_marker(), false)
    }

    /// Heap bytes held by this store's maps, retired ones included.
    pub fn heap_bytes(&self) -> usize {
        self.heap_bytes.load(Ordering::Relaxed)
    }

    /// Classes with maps (or a skipped marker) in this store.
    pub fn class_count(&self) -> usize {
        self.class_count.load(Ordering::Relaxed)
    }

    fn note_added(&self, bytes: usize, new_class: bool) {
        self.heap_bytes.fetch_add(bytes, Ordering::Relaxed);
        STORE_HEAP_BYTES.fetch_add(bytes, Ordering::Relaxed);
        if new_class {
            self.class_count.fetch_add(1, Ordering::Relaxed);
            STORE_CLASS_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn retire(&self, old: *mut ClassTypeMaps) {
        if old.is_null() {
            return;
        }
        // Poison only means another publisher panicked mid-push; the vector
        // itself is intact, and dropping the entry would leak it.
        let mut retired = match self.retired.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        retired.push(Retired(old));
    }

    fn install(&self, class_id: ClassId, mut maps: ClassTypeMaps, replace: bool) -> bool {
        let Some(chunk) = self.chunk_for_or_create(class_id) else {
            return false; // class id beyond the addressable range
        };
        if self.drop_rows.load(Ordering::Relaxed) {
            maps.strip_rows();
        }
        let idx = (class_id.as_u32() & CHUNK_MASK) as usize;
        let bytes = maps.heap_bytes() + std::mem::size_of::<ClassTypeMaps>();
        let fresh = Box::into_raw(Box::new(maps));

        if replace {
            let old = chunk.slots[idx].swap(fresh, Ordering::AcqRel);
            // A GC root scan may hold a borrow of the previous maps at this
            // instant, and there is no epoch reclamation here; retire them.
            // JVMTI `RedefineClasses` is the only path that reaches here.
            self.note_added(bytes, old.is_null());
            self.retire(old);
            return true;
        }

        match chunk.slots[idx].compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.note_added(bytes, true);
                true
            }
            Err(existing) => {
                // SAFETY: `existing` is published in this store and freed only
                // by `Drop`.
                let existing_ref = unsafe { &*existing };
                if !existing_ref.verified {
                    // Upgrade a "skipped" marker to real maps.
                    let old = chunk.slots[idx].swap(fresh, Ordering::AcqRel);
                    self.note_added(bytes, false);
                    self.retire(old);
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
}

impl Drop for TypeMapStore {
    fn drop(&mut self) {
        let bytes = *self.heap_bytes.get_mut();
        let classes = *self.class_count.get_mut();
        let _ = STORE_HEAP_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
            Some(t.saturating_sub(bytes))
        });
        let _ = STORE_CLASS_COUNT.try_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
            Some(t.saturating_sub(classes))
        });
        if let Some(dir) = self.dir.get_mut() {
            for chunk_slot in dir.iter_mut() {
                let chunk = std::mem::replace(chunk_slot.get_mut(), ptr::null_mut());
                if chunk.is_null() {
                    continue;
                }
                // SAFETY: `&mut self` — no borrow handed out by this store is
                // still alive. The chunk came from `Box::into_raw` in
                // `chunk_for_or_create` and was published only here.
                let mut chunk = unsafe { Box::from_raw(chunk) };
                for slot in chunk.slots.iter_mut() {
                    let p = std::mem::replace(slot.get_mut(), ptr::null_mut());
                    if !p.is_null() {
                        // SAFETY: as above; each published maps pointer is in
                        // exactly one slot, and a displaced one is in `retired`
                        // instead.
                        drop(unsafe { Box::from_raw(p) });
                    }
                }
            }
        }
        let retired = match self.retired.get_mut() {
            Ok(v) => v,
            Err(poisoned) => poisoned.into_inner(),
        };
        for Retired(p) in retired.drain(..) {
            // SAFETY: `retire` received each pointer exactly once, from a swap
            // that removed it from its slot.
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

/// The store for publishers with no VM; see [`TypeMapStore`]. Lives for the
/// process.
pub fn process_store() -> &'static TypeMapStore {
    static PROCESS: OnceLock<TypeMapStore> = OnceLock::new();
    PROCESS.get_or_init(TypeMapStore::new)
}

/// The store the verifier publishes a class's maps into: the VM's, when the
/// hierarchy it verifies against belongs to one, else [`process_store`].
pub fn store_for(hierarchy: &dyn ClassHierarchy) -> &TypeMapStore {
    match hierarchy.type_map_store() {
        Some(store) => store,
        None => process_store(),
    }
}

/// All published type maps for `class_id` in the [`process_store`], or `None`.
///
/// A VM's own maps are in its store (`ClassRealm::type_maps`); this reads the
/// no-VM store. Nothing that gates execution may read it.
#[inline]
pub fn class_type_maps(class_id: ClassId) -> Option<&'static ClassTypeMaps> {
    process_store().class_type_maps(class_id)
}

/// Type maps for one method, `O(1)`, from the [`process_store`].
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
    process_store().type_maps_for(class_id, method_index)
}

/// Type maps for one method, identified by name and descriptor, from the
/// [`process_store`].
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
    process_store().type_maps_for_named(class_id, name, descriptor)
}

/// What the [`process_store`] knows about `class_id`'s verification.
#[inline]
pub fn verification_status(class_id: ClassId) -> VerificationStatus {
    process_store().verification_status(class_id)
}

/// Publish `maps` for `class_id` into the [`process_store`].
///
/// Returns `false` when an equivalent entry already existed (concurrent
/// duplicate verification) or when `class_id` is outside the addressable
/// range. The verifier publishes through [`store_for`] instead.
pub fn publish_class_type_maps(class_id: ClassId, maps: ClassTypeMaps) -> bool {
    process_store().publish(class_id, maps)
}

/// Replace the maps for `class_id` in the [`process_store`].
pub fn replace_class_type_maps(class_id: ClassId, maps: ClassTypeMaps) -> bool {
    process_store().replace(class_id, maps)
}

/// Record in the [`process_store`] that verification was deliberately skipped
/// for `class_id` (`--noverify` / `-Xverify:none` / a per-class skip).
///
/// This is what lets a consumer tell "we have no map because nobody verified
/// this" apart from "we have no map because this pc is not an instruction
/// start". Without it, [`verification_status`] answers
/// [`VerificationStatus::Unknown`], which is equally conservative but less
/// diagnosable.
pub fn mark_class_verification_skipped(class_id: ClassId) -> bool {
    process_store().mark_skipped(class_id)
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

    // -- Locals epoch: row reuse must equal row recomputation ------------------

    /// SBR round 5, lane verify: `record` (with the frame's locals epoch) reuses
    /// the previous locals row when nothing touched the locals. Drive a frame
    /// through stores, a same-value store, a declared-frame adoption, a pad and
    /// an in-place replace, record every step both ways, and require identical
    /// maps.
    #[test]
    fn epoch_row_reuse_matches_recomputing_every_row() {
        use crate::verify_frame::VerificationFrame;
        use std::sync::Arc as A;

        let this: A<str> = A::from("Foo");
        let (_, mut frame) =
            VerificationFrame::initial_frames(&this, "m", "(Ljava/lang/String;J)V", false, 6, 4);
        let mut declared = frame.clone();
        declared.locals[3] = oref("Bar");
        declared.locals[5] = oref("Baz");

        let mut fast = MethodTypeMapsBuilder::new(6, 4);
        let mut slow = MethodTypeMapsBuilder::new(6, 4);
        let mut step = 0u32;
        let mut record = |frame: &VerificationFrame, fast: &mut MethodTypeMapsBuilder| {
            fast.record(step, frame);
            slow.record_slots(step, &frame.locals, &frame.stack);
            step += 1;
        };

        record(&frame, &mut fast); // initial
        frame.push(VType::Int).unwrap();
        record(&frame, &mut fast); // stack change only: locals row reused
        frame.local_store(4, oref("Qux")).unwrap();
        record(&frame, &mut fast); // a store: recomputed
        frame.local_store(4, oref("Qux")).unwrap();
        record(&frame, &mut fast); // same value: reused, still identical
        frame.local_store(4, VType::Int).unwrap();
        record(&frame, &mut fast); // reference -> int: recomputed
        frame.adopt_declared(&declared, 6);
        record(&frame, &mut fast); // declared frame adopted
        frame.pad_locals_to(8);
        record(&frame, &mut fast); // widened past max_locals
        for slot in frame.locals.iter_mut() {
            if *slot == oref("Bar") {
                *slot = VType::Null;
            }
        }
        record(&frame, &mut fast); // in-place replace
        frame.push(VType::Float).unwrap();
        record(&frame, &mut fast);

        let (fast, slow) = (fast.finish(true), slow.finish(true));
        assert_eq!(fast.entry_count(), slow.entry_count());
        assert_eq!(fast.locals_fully_described(), slow.locals_fully_described());
        assert_eq!(fast.fast_path_veto(), slow.fast_path_veto());
        for pc in 0..step {
            let (a, b) = (
                fast.frame_map_at(pc).unwrap(),
                slow.frame_map_at(pc).unwrap(),
            );
            assert_eq!(
                a.locals.as_bytes(),
                b.locals.as_bytes(),
                "locals row at {pc}"
            );
            assert_eq!(a.stack.as_bytes(), b.stack.as_bytes(), "stack row at {pc}");
            assert_eq!(a.stack_depth, b.stack_depth, "depth at {pc}");
        }
    }

    // -- Row encoding --------------------------------------------------------

    /// zs s2/fp2: a method signature is 28 bytes (thin names), and the
    /// by-name lookup answers as before, sharing the caller's interned `Arc`s.
    #[test]
    fn zs2fp2_method_sig_is_28_bytes_and_lookup_is_unchanged() {
        assert_eq!(std::mem::size_of::<MethodSig>(), 28);
        let name: Arc<str> = Arc::from("run");
        let d1: Arc<str> = Arc::from("()V");
        let d2: Arc<str> = Arc::from("(I)V");
        let maps = ClassTypeMaps::new(vec![
            (Arc::clone(&name), Arc::clone(&d1), None),
            (Arc::clone(&name), Arc::clone(&d2), None),
        ]);
        assert_eq!(maps.method_index_of("run", "()V"), Some(0));
        assert_eq!(maps.method_index_of("run", "(I)V"), Some(1));
        assert_eq!(maps.method_index_of("run", "(J)V"), None);
        assert!(maps.sigs[1].name.ptr_eq_arc(&name));
        // The two signatures and the local `name` hold it.
        assert_eq!(Arc::strong_count(&name), 3);
        drop(maps);
        assert_eq!(Arc::strong_count(&name), 1);
    }

    /// zs s1/fp: the four tables share one exact-size block and the header is
    /// a quarter of the old 120 bytes; every answer is the same, on the
    /// narrow arms and on the wide-pc / wide-depth ones.
    #[test]
    fn zs1fp_method_type_maps_are_one_block_with_a_small_header() {
        assert!(
            std::mem::size_of::<MethodTypeMaps>() <= 32,
            "{}",
            std::mem::size_of::<MethodTypeMaps>()
        );
        assert_eq!(
            std::mem::size_of::<Option<MethodTypeMaps>>(),
            std::mem::size_of::<MethodTypeMaps>(),
            "the Box niche keeps an absent method's slot the same size"
        );
        for (pc_base, depth) in [(0u32, 3usize), (70_000, 3), (0, 300), (70_000, 300)] {
            let max_stack = (depth + 1) as u16;
            let mut b = MethodTypeMapsBuilder::new(9, max_stack);
            let locals = [
                oref("A"),
                VType::Int,
                VType::Int,
                VType::Int,
                VType::Int,
                VType::Int,
                VType::Int,
                VType::Int,
                oref("B"),
            ];
            let mut stack = vec![VType::Int; depth];
            stack[0] = oref("C");
            b.record_slots(pc_base, &locals, &[]);
            b.record_slots(pc_base + 3, &locals[..1], &stack);
            b.record_slots(pc_base + 7, &locals, &stack[..1]);
            let m = b.finish(true);
            let pc_width = if pc_base > u16::MAX as u32 { 4 } else { 2 };
            let depth_width = if depth > 255 { 2 } else { 1 };
            let stack_stride = stride_for(max_stack);
            assert_eq!(
                m.heap_bytes(),
                3 * (pc_width + 2 + stack_stride + depth_width),
                "pc={pc_base} depth={depth}"
            );
            assert_eq!(m.entry_count(), 3);
            assert_eq!(m.pc_at_index(1), Some(pc_base + 3));
            assert_eq!(m.pc_at_index(3), None);
            assert_eq!(m.index_of_pc(pc_base + 7), Some(2));
            assert!(!m.has_pc(pc_base + 1));
            let f = m.frame_map_at(pc_base + 3).unwrap();
            assert_eq!(f.stack_depth as usize, depth);
            assert!(f.stack.get(0) && !f.stack.get(1));
            assert!(
                f.locals.get(0) && !f.locals.get(8),
                "a short row's tail is clear"
            );
            assert_eq!(m.stack_depth_at(pc_base + 7), Some(1));
            assert_eq!(m.stack_depth_at(pc_base), Some(0));
            let l = m.local_oops_at(pc_base + 7).unwrap();
            assert_eq!(l.iter_set().collect::<Vec<_>>(), vec![0, 8]);
            assert!(!m.locals_fully_described(), "the pc+3 row was short");
            assert!(m.safe_for_fast_path());
        }
        // No rows: one empty block, every lookup `None`.
        let m = MethodTypeMapsBuilder::new(4, 4).finish(true);
        assert_eq!(m.heap_bytes(), 0);
        assert!(m.oop_map_at(0).is_none() && m.pc_at_index(0).is_none());
        assert!(m.locals_fully_described());
    }

    /// JIT round 23 wave 5 (`CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE`): maps
    /// built through a reused [`TypeMapSpare`] are byte-identical to fresh
    /// builders' ones, for a larger method followed by a smaller one (the spare
    /// carries more capacity and stale contents, which must not leak).
    #[test]
    fn a_reused_spare_builds_identical_maps() {
        let first = |b: &mut MethodTypeMapsBuilder| {
            for pc in 0..9u32 {
                b.record_slots(
                    pc * 2,
                    &[oref("java/lang/String"), VType::Int, VType::Null, VType::Top],
                    &[oref("A"), VType::Int],
                );
            }
        };
        let second = |b: &mut MethodTypeMapsBuilder| {
            b.record_slots(0, &[VType::Int, oref("B")], &[]);
            b.record_slots(3, &[oref("C"), VType::Int], &[VType::Int]);
        };
        let same = |a: &MethodTypeMaps, b: &MethodTypeMaps| {
            assert_eq!(a.data, b.data);
            assert_eq!(a.rows, b.rows);
            assert_eq!(a.flags, b.flags);
            assert_eq!(a.veto, b.veto);
            assert_eq!((a.max_locals, a.max_stack), (b.max_locals, b.max_stack));
        };
        let mut fresh1 = MethodTypeMapsBuilder::new(4, 2);
        first(&mut fresh1);
        let fresh1 = fresh1.finish(true);
        let mut fresh2 = MethodTypeMapsBuilder::new(2, 1);
        second(&mut fresh2);
        let fresh2 = fresh2.finish(false);

        let mut spare = TypeMapSpare::default();
        let mut reused1 = MethodTypeMapsBuilder::with_spare(4, 2, &mut spare);
        first(&mut reused1);
        let reused1 = reused1.finish_into_spare(true, &mut spare);
        assert!(spare.local_bytes.capacity() > 0, "the spare keeps the capacity");
        let mut reused2 = MethodTypeMapsBuilder::with_spare(2, 1, &mut spare);
        second(&mut reused2);
        let reused2 = reused2.finish_into_spare(false, &mut spare);
        same(&fresh1, &reused1);
        same(&fresh2, &reused2);
    }

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

    fn one_method(name: &str, safe: bool) -> ClassTypeMaps {
        ClassTypeMaps::new(vec![(
            Arc::<str>::from(name),
            Arc::<str>::from("()I"),
            Some(tiny_maps(safe)),
        )])
    }

    /// Two VMs number their classes from the same origin, so one class id
    /// names a different class in each. VM B's vetoed maps must not be
    /// shadowed by VM A's proven maps for the same `(id, name, descriptor)` —
    /// in the old process-wide table B's publish was dropped as a "duplicate"
    /// and B's fast-path gate read A's "proven".
    #[test]
    fn two_vm_stores_keep_their_own_maps_for_one_class_id() {
        let a = TypeMapStore::new();
        let b = TypeMapStore::new();
        let id = ClassId::new(7);
        assert!(a.publish(id, one_method("m", true)));
        assert!(
            b.publish(id, one_method("m", false)),
            "not a duplicate in B"
        );
        assert!(a
            .type_maps_for_named(id, "m", "()I")
            .unwrap()
            .safe_for_fast_path());
        assert!(!b
            .type_maps_for_named(id, "m", "()I")
            .unwrap()
            .safe_for_fast_path());
        // A class published in one VM is unknown in another.
        let only_in_c = ClassId::new(0x0030_0007);
        let c = TypeMapStore::new();
        assert!(c.publish(only_in_c, one_method("m", true)));
        assert_eq!(
            a.verification_status(only_in_c),
            VerificationStatus::Unknown
        );
        assert_eq!(
            c.verification_status(only_in_c),
            VerificationStatus::Verified
        );
        assert_eq!(b.class_count(), 1);
    }

    /// Replace retires the displaced maps instead of freeing them under a
    /// possible reader, and the store frees everything, retired maps included,
    /// when it drops (the old process-wide table leaked every VM's maps).
    #[test]
    fn a_replaced_store_answers_the_new_maps_and_drops_cleanly() {
        let store = TypeMapStore::new();
        let id = ClassId::new(3);
        assert!(store.mark_skipped(id));
        assert_eq!(store.verification_status(id), VerificationStatus::Skipped);
        assert!(
            store.publish(id, one_method("m", true)),
            "upgrades the marker"
        );
        assert!(store.replace(id, one_method("m", false)));
        assert!(!store.type_maps_for(id, 0).unwrap().safe_for_fast_path());
        assert_eq!(store.class_count(), 1);
        assert!(store.heap_bytes() > 0);
        assert_eq!(store.retired.lock().map(|r| r.len()).unwrap_or(0), 2);
        drop(store);
    }

    /// A VM store keeps its maps to itself: nothing it publishes appears in
    /// the process store. (Until wave 8 an armed `CRATONVM_DBG_VERIFY_OOP_MAPS`
    /// mirrored every publish there, first writer winning across VMs.)
    #[test]
    fn a_vm_store_never_writes_the_process_store() {
        let id = ClassId::new(0x0030_0101);
        let store = TypeMapStore::new();
        assert!(store.publish(id, one_method("not_mirrored", true)));
        assert!(store
            .type_maps_for_named(id, "not_mirrored", "()I")
            .is_some());
        assert!(type_maps_for_named(id, "not_mirrored", "()I").is_none());
    }

    #[test]
    fn class_ids_beyond_the_addressable_range_degrade_gracefully() {
        let id = ClassId::new(u32::MAX);
        assert_eq!(verification_status(id), VerificationStatus::Unknown);
        assert!(!mark_class_verification_skipped(id));
        assert!(type_maps_for(id, 0).is_none());
    }

    /// zs s3/fp3: a store that drops rows keeps every per-method verdict and
    /// answers every row query `None` (the conservative "unproven"); a plain
    /// store keeps the rows. The kill switch and the debug readers' key are
    /// read by `rows_dropped_by_default`, which a class store applies.
    #[test]
    fn zs3fp3_a_row_dropping_store_keeps_the_verdicts() {
        let id = ClassId::new(0x0030_0201);
        let keep = TypeMapStore::new();
        assert!(!keep.drops_rows());
        let drop = TypeMapStore::new();
        drop.set_drop_rows(true);
        for (safe, name) in [(true, "s"), (false, "u")] {
            assert!(keep.publish(
                ClassId::new(id.as_u32() + safe as u32),
                one_method(name, safe)
            ));
            assert!(drop.publish(
                ClassId::new(id.as_u32() + safe as u32),
                one_method(name, safe)
            ));
            let k = keep
                .type_maps_for(ClassId::new(id.as_u32() + safe as u32), 0)
                .unwrap();
            let d = drop
                .type_maps_for(ClassId::new(id.as_u32() + safe as u32), 0)
                .unwrap();
            assert_eq!(k.safe_for_fast_path(), safe);
            assert_eq!(d.safe_for_fast_path(), k.safe_for_fast_path());
            assert_eq!(d.fast_path_veto(), k.fast_path_veto());
            assert_eq!(d.max_locals(), k.max_locals());
            assert_eq!(d.max_stack(), k.max_stack());
            assert_eq!(d.locals_fully_described(), k.locals_fully_described());
            // Positive control: the plain store has the row.
            assert_eq!(k.entry_count(), 1);
            assert!(k.local_oops_at(0).is_some());
            assert_eq!(d.entry_count(), 0);
            assert_eq!(d.heap_bytes(), 0);
            assert!(d.oop_map_at(0).is_none());
            assert!(d.frame_map_at(0).is_none());
            assert!(d.local_oops_at(0).is_none());
            assert!(d.stack_oops_at(0).is_none());
            assert!(d.stack_depth_at(0).is_none());
            assert!(!d.has_pc(0));
            assert!(drop
                .type_maps_for_named(ClassId::new(id.as_u32() + safe as u32), name, "()I")
                .is_some());
        }
        assert!(drop.heap_bytes() < keep.heap_bytes());
        // Switched back off, later installs keep their rows.
        drop.set_drop_rows(false);
        let later = ClassId::new(id.as_u32() + 5);
        assert!(drop.publish(later, one_method("k", true)));
        assert_eq!(drop.type_maps_for(later, 0).unwrap().entry_count(), 1);
    }

    /// Phase 5 fast boot (lane p5-define): a verdict-only builder finishes to
    /// exactly what `strip_rows` leaves of the full builder's maps, over
    /// random row sequences that hit every veto (unsorted / repeated pcs,
    /// over-wide locals, short locals, stack past `max_stack`, broken
    /// category-2 pairs) and the row budget.
    #[test]
    fn p5_verdict_only_builder_matches_stripped_full_maps() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n.max(1)
        };
        let pick = |k: u64| match k {
            0 => VType::Int,
            1 => VType::Top,
            2 => VType::Long,
            3 => VType::Null,
            _ => oref("A"),
        };
        for case in 0..4000u32 {
            let max_locals = next(12) as u16 + if case % 97 == 0 { 3000 } else { 0 };
            let max_stack = next(10) as u16 + if case % 89 == 0 { 3000 } else { 0 };
            let mut full = MethodTypeMapsBuilder::new(max_locals, max_stack);
            let mut verdict = MethodTypeMapsBuilder::new(max_locals, max_stack);
            verdict.set_verdict_only();
            let rows = next(if case % 50 == 0 { 3000 } else { 40 });
            let mut pc = 0u32;
            for _ in 0..rows {
                pc = match next(20) {
                    0 => pc.saturating_sub(1),
                    1 => pc,
                    _ => pc + 1 + next(4) as u32,
                };
                let nl = (u64::from(max_locals) + next(3)).saturating_sub(1).min(4000);
                let locals: Vec<VType> = (0..nl).map(|_| pick(next(5))).collect();
                let ns = next(u64::from(max_stack) + 3).min(4000);
                let stack: Vec<VType> = (0..ns).map(|_| pick(next(5))).collect();
                full.record_slots(pc, &locals, &stack);
                verdict.record_slots(pc, &locals, &stack);
            }
            let complete = next(4) != 0;
            let mut full = full.finish(complete);
            full.strip_rows();
            let verdict = verdict.finish(complete);
            assert_eq!(full.veto, verdict.veto, "case {case}");
            assert_eq!(full.flags, verdict.flags, "case {case}");
            assert_eq!(full.rows, verdict.rows, "case {case}");
            assert_eq!(full.data, verdict.data, "case {case}");
            assert_eq!(
                (full.max_locals, full.max_stack),
                (verdict.max_locals, verdict.max_stack)
            );
        }
    }
}
