// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Quickening: one-time, per-method pre-decode of a bytecode array.
//!
//! # Why
//!
//! The interpreter's spec-correct dispatch path calls
//! [`Instruction::decode`] on **every execution of every instruction**.
//! For a method executed a million times that is a million redundant
//! opcode matches and operand reads, and — because the `tableswitch` /
//! `lookupswitch` payloads are owned out-of-line — a million heap
//! allocations for every switch that executes.
//!
//! A [`QuickenedCode`] decodes the method exactly once and hands the
//! interpreter a borrowed `&Instruction` per dispatch. Nothing is cloned
//! and nothing is allocated on the hot path.
//!
//! # Correctness contract
//!
//! Quickening is a **pure memoization** of `Instruction::decode`:
//!
//! * The stream is built by walking the *same* padded byte array the
//!   interpreter dispatches from, calling the *same* `Instruction::decode`,
//!   starting at pc 0 and following each instruction's reported `next_pc`.
//! * Records are keyed by their **bytecode pc**, never by an ordinal that
//!   the rest of the VM could observe. [`QuickenedCode::index_of_pc`]
//!   only ever returns an index `i` with `pcs[i] == pc`, and
//!   [`QuickenedCode::next_pc`] returns exactly the `next_pc` that
//!   `Instruction::decode` returned at that pc. Exception handlers, stack
//!   maps, line-number tables, JVMTI single-step, and the JIT/OSR entry
//!   points therefore continue to see identical pc values.
//! * A pc that is **not** an instruction start in the linear walk (dead
//!   data between instructions, a landing pad the linear walk desynchronised
//!   on, ...) returns `None`, and the caller falls back to the original
//!   `Instruction::decode` at that pc. The quickened stream can therefore
//!   never *change* behaviour — at worst it is not used.
//! * A method whose linear walk hits a decode error keeps the records for
//!   the prefix it *did* decode and stops there (see `build`). Every pc at
//!   or beyond the failure point reports "not found" and routes through
//!   `Instruction::decode`, which fails at exactly the pc it always did.
//!   A method that cannot decode even its first instruction is not
//!   quickened at all (`build` returns `None`).
//!
//! # pc → index resolution is O(1)
//!
//! Dispatch resolves a bytecode pc to a stream index on *every* instruction,
//! so that lookup is itself on the hot path. A hint covers straight-line
//! fall-through, but every taken branch, loop back-edge, exception-handler
//! entry and `switch` target arrives with an arbitrary pc — precisely the
//! control-flow-heavy code where the interpreter already hurts most.
//!
//! Those arbitrary pcs are served by a dense side index built from `pcs`:
//! a **bitmap of instruction starts** plus a **per-block cumulative count**,
//! so a lookup is two loads from one cache line and a `popcount`, with no
//! search and no data-dependent branching. See [`PcBlock`] for the layout
//! and `quickened-dispatch-o1.md` for the
//! measurements behind the choice.
//!
//! The side index is a pure accelerator: it indexes exactly the same set of
//! pcs as `pcs[..ops.len()]`, so it cannot make a lookup succeed that the
//! binary search would have failed, or vice versa.
//!
//! # Ownership / lifetime
//!
//! A [`QuickenedCode`] holds a strong `Arc<[u8]>` to the byte array it
//! was built from. That is load-bearing: callers memoize the stream keyed
//! on the *address* of the code allocation, and pinning the allocation is
//! what makes that address stable and non-recyclable for as long as the
//! stream is reachable.

use crate::instruction::Instruction;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Bytecode bytes covered by one [`PcBlock`] — one bit per byte in a `u64`.
const BLOCK_BYTES: usize = 64;

/// Largest instruction start pc (inclusive) the dense index is built for.
///
/// JVMS §4.7.3 requires `code_length < 65536`, so the highest pc an
/// instruction can legally start at is 65534 — every legal method body is
/// covered with a byte to spare. A start pc beyond this bound cannot come
/// from a valid classfile; such a stream simply keeps the binary-search path
/// (see [`QuickenedCode::has_dense_index`]). The worst-case index this
/// admits is 1024 blocks = 16 KiB.
///
/// This is a structural bound, not a tunable: there is no switch that turns
/// the dense index off for a method that qualifies. It was measured against
/// 2,345,014 methods (Maven corpus + JDK 25 + this repo's fixtures) whose
/// largest `code_length` was 60,399 — see the design doc.
const MAX_DENSE_START_PC: usize = 65_535;

/// One 64-byte window of the bytecode, for O(1) pc → stream-index lookup.
///
/// `starts` has bit `k` set exactly when the pc `block_base + k` is an
/// instruction start in this stream. `cum` is the number of instruction
/// starts in all *preceding* blocks. The index of a start is therefore
/// `cum + (starts & below_mask).count_ones()` — no search, no branch on
/// data, and both fields live in the same 16 bytes.
///
/// `cum` is `u32` rather than `u16` purely for headroom; the struct is
/// 16 bytes either way because `starts` forces 8-byte alignment.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PcBlock {
    /// Bitmap of instruction starts within this block, LSB = lowest pc.
    starts: u64,
    /// Count of instruction starts in every block before this one.
    cum: u32,
}

/// A method's bytecode, pre-decoded once into a fixed-size record stream.
#[derive(Debug)]
pub struct QuickenedCode {
    /// The exact byte array this stream was decoded from. Held to pin the
    /// allocation (see the module docs) and to let callers assert identity.
    code: Arc<[u8]>,
    /// Pre-decoded instructions in ascending pc order. `Instruction` is a
    /// fixed-size record — the `tableswitch` / `lookupswitch` payloads are
    /// interned out-of-line behind an `Arc`, so no variant owns a `Vec`.
    ops: Box<[Instruction]>,
    /// `pcs[i]` is the bytecode pc of `ops[i]`. Length is `ops.len() + 1`;
    /// the trailing sentinel is the `next_pc` of the final instruction, so
    /// `next_pc(i) == pcs[i + 1]` holds for every valid `i` without a
    /// second parallel array.
    pcs: Box<[u32]>,
    /// Dense pc → index accelerator over `pcs[..ops.len()]`, or `None` for a
    /// stream whose highest start pc exceeds [`MAX_DENSE_START_PC`] (not
    /// reachable from a valid classfile), which falls back to binary search.
    starts: Option<Box<[PcBlock]>>,
}

/// Count of quickened streams built. Not a live count: nothing decrements it
/// when a stream is dropped, and a build that loses an intern race to another
/// thread (see [`intern`]) is counted although its stream is discarded.
static STAT_METHODS: AtomicUsize = AtomicUsize::new(0);
/// Total pre-decoded instruction records across all quickened methods.
static STAT_OPS: AtomicUsize = AtomicUsize::new(0);
/// Total heap bytes owned by quickened streams (records + pc table +
/// dense index + interned switch tables), excluding the pinned bytecode.
static STAT_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Total bytecode bytes covered by quickened streams.
static STAT_CODE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Methods that could not decode even one instruction (never quickened).
static STAT_UNQUICKENABLE: AtomicUsize = AtomicUsize::new(0);
/// Methods quickened only up to a decode failure (prefix salvaged).
static STAT_TRUNCATED: AtomicUsize = AtomicUsize::new(0);
/// Heap bytes owned by the dense pc → index side tables.
static STAT_INDEX_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Methods that fell back to binary search (no dense index).
static STAT_NO_INDEX: AtomicUsize = AtomicUsize::new(0);
/// Methods the interpreter's per-method fast-path admission sent to the
/// decoded path because the verifier vetoed `safe_for_fast_path` (see
/// `vm::runtime::interpreter::frame_fast_path_admitted`). Counted per
/// admission decision, which that side memoizes per method, so a memo
/// eviction can count a method twice. Only bumped under
/// `CRATONVM_QUICKEN_STATS`.
static STAT_FAST_PATH_VETOED: AtomicUsize = AtomicUsize::new(0);

impl QuickenedCode {
    /// Pre-decode `code` into a quickened stream, or return `None` if not
    /// even the first instruction decodes.
    ///
    /// `code` is the **padded** array the interpreter dispatches from
    /// (two trailing zero bytes); the walk stops at `len - 2`, which is the
    /// same unpadded bound the dispatch loop uses.
    ///
    /// If the linear walk hits a decode error partway through, the records
    /// decoded so far are kept and the walk stops. That is sound under the
    /// module's correctness contract — every retained record is exactly what
    /// `Instruction::decode` returned at its pc, and every pc from the
    /// failure point onwards reports "not found" and routes through
    /// `Instruction::decode`, which raises the identical error at the
    /// identical pc. Salvaging the prefix keeps the interpreter on the
    /// allocation-free path for any `tableswitch` / `lookupswitch` that sits
    /// before the undecodable bytes.
    pub fn build(code: &Arc<[u8]>) -> Option<Arc<QuickenedCode>> {
        let limit = code.len().saturating_sub(2);
        if limit == 0 || limit > u32::MAX as usize {
            return None;
        }
        // A method body is capped at 65535 bytes by the classfile format, so
        // these vectors are small; reserve on the shortest possible encoding.
        let mut ops: Vec<Instruction> = Vec::with_capacity(limit / 2 + 1);
        let mut pcs: Vec<u32> = Vec::with_capacity(limit / 2 + 2);
        let mut pc = 0usize;
        let mut truncated = false;
        while pc < limit {
            let (insn, next) = match Instruction::decode(code, pc) {
                Ok(v) => v,
                Err(_) => {
                    truncated = true;
                    break;
                }
            };
            // `decode` must make progress, otherwise the walk would spin.
            if next <= pc || next > u32::MAX as usize {
                truncated = true;
                break;
            }
            pcs.push(pc as u32);
            ops.push(insn);
            pc = next;
        }
        if ops.is_empty() {
            STAT_UNQUICKENABLE.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if truncated {
            STAT_TRUNCATED.fetch_add(1, Ordering::Relaxed);
        }
        // Sentinel: the `next_pc` the last decoded instruction reported (or,
        // when truncated, the pc the walk stopped at — which is the same
        // value, since that pc is the failed instruction's start).
        pcs.push(pc as u32);

        let starts = build_start_index(&pcs[..ops.len()]);
        let q = Arc::new(QuickenedCode {
            code: Arc::clone(code),
            ops: ops.into_boxed_slice(),
            pcs: pcs.into_boxed_slice(),
            starts,
        });
        let n = STAT_METHODS.fetch_add(1, Ordering::Relaxed) + 1;
        STAT_OPS.fetch_add(q.ops.len(), Ordering::Relaxed);
        STAT_BYTES.fetch_add(q.heap_bytes(), Ordering::Relaxed);
        STAT_CODE_BYTES.fetch_add(limit, Ordering::Relaxed);
        STAT_INDEX_BYTES.fetch_add(q.index_bytes(), Ordering::Relaxed);
        if q.starts.is_none() {
            STAT_NO_INDEX.fetch_add(1, Ordering::Relaxed);
        }
        if stats_enabled() && (n.is_power_of_two() || n % 5_000 == 0) {
            report_stats();
        }
        Some(q)
    }

    /// Number of pre-decoded instructions.
    #[inline]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// True when the stream holds no instructions.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The byte array this stream was decoded from.
    #[inline]
    pub fn code(&self) -> &Arc<[u8]> {
        &self.code
    }

    /// The pre-decoded instruction at stream index `idx`.
    #[inline]
    pub fn op(&self, idx: usize) -> &Instruction {
        &self.ops[idx]
    }

    /// The bytecode pc that `idx` decodes at.
    #[inline]
    pub fn pc_of(&self, idx: usize) -> usize {
        self.pcs[idx] as usize
    }

    /// The `next_pc` that `Instruction::decode` returned for `idx` — i.e.
    /// the pc of the following instruction. Exact by construction: the
    /// build walk stored each instruction's reported `next_pc` as the pc of
    /// its successor, with a trailing sentinel for the last one.
    #[inline]
    pub fn next_pc(&self, idx: usize) -> usize {
        self.pcs[idx + 1] as usize
    }

    /// Whether this stream carries the dense O(1) pc → index accelerator.
    /// False only for a stream whose highest instruction start exceeds
    /// [`MAX_DENSE_START_PC`], which no valid classfile can produce.
    #[inline]
    pub fn has_dense_index(&self) -> bool {
        self.starts.is_some()
    }

    /// Resolve a bytecode pc to a stream index, or `None` when `pc` is not
    /// an instruction start in this stream (the caller must then fall back
    /// to `Instruction::decode`).
    ///
    /// `hint` is checked first and is expected to be `previous_index + 1`,
    /// which resolves straight-line fall-through in a single compare.
    /// **Any other pc — a taken branch, a loop back-edge, an
    /// exception-handler entry, a `switch` target — is resolved in O(1) by
    /// the dense start index**, not by a search.
    ///
    /// `hint` is now only a peephole over the O(1) path and may be any
    /// value: it is never trusted without the `pcs[hint] == pc` check, so a
    /// stale or nonsensical hint costs one predictable compare and changes
    /// nothing. See [`QuickenedCode::resolve`] for the shape callers should
    /// prefer.
    #[inline]
    pub fn index_of_pc(&self, pc: usize, hint: usize) -> Option<usize> {
        if hint < self.ops.len() && self.pcs[hint] as usize == pc {
            return Some(hint);
        }
        self.index_of_pc_direct(pc)
    }

    /// Hint-free O(1) form of [`QuickenedCode::index_of_pc`].
    ///
    /// Returns `Some(i)` only when `pcs[i] == pc`; `None` for any pc that is
    /// not an instruction start in this stream.
    #[inline]
    pub fn index_of_pc_direct(&self, pc: usize) -> Option<usize> {
        match &self.starts {
            // Dense path. The index covers exactly `pcs[..ops.len()]`, so a
            // miss here is authoritative — there is nothing to fall back to.
            Some(blocks) => {
                let block = blocks.get(pc / BLOCK_BYTES)?;
                let bit = (pc % BLOCK_BYTES) as u32;
                if (block.starts >> bit) & 1 == 0 {
                    return None;
                }
                // Bits strictly below `bit` are the starts earlier in this
                // block; `bit < 64`, so the mask never overflows.
                let below = block.starts & ((1u64 << bit) - 1);
                Some(block.cum as usize + below.count_ones() as usize)
            }
            // No dense index (start pc beyond `MAX_DENSE_START_PC`): exact
            // because `pcs` is strictly increasing.
            None => {
                if pc > u32::MAX as usize {
                    return None;
                }
                let n = self.ops.len();
                self.pcs[..n].binary_search(&(pc as u32)).ok()
            }
        }
    }

    /// Resolve `pc` straight to its pre-decoded instruction and `next_pc`.
    ///
    /// This is the shape a dispatch loop wants: one O(1) call replacing
    /// `index_of_pc` + `op` + `next_pc` and their three bounds checks.
    /// `None` means `pc` is not an instruction start and the caller must
    /// fall back to `Instruction::decode`.
    #[inline]
    pub fn resolve(&self, pc: usize) -> Option<(&Instruction, usize)> {
        let idx = self.index_of_pc_direct(pc)?;
        Some((&self.ops[idx], self.pcs[idx + 1] as usize))
    }

    /// Heap bytes owned by the dense pc → index accelerator.
    #[inline]
    pub fn index_bytes(&self) -> usize {
        self.starts
            .as_ref()
            .map_or(0, |b| b.len() * std::mem::size_of::<PcBlock>())
    }

    /// Heap bytes owned by this stream: the record array, the pc table, the
    /// dense start index, and every interned switch table it points at.
    /// Excludes the pinned bytecode array (shared with the class data, not
    /// new footprint).
    pub fn heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<QuickenedCode>()
            + self.ops.len() * std::mem::size_of::<Instruction>()
            + self.pcs.len() * std::mem::size_of::<u32>()
            + self.index_bytes();
        for op in self.ops.iter() {
            match op {
                Instruction::Tableswitch(t) => {
                    total += std::mem::size_of::<crate::instruction::TableSwitch>()
                        + t.offsets.len() * std::mem::size_of::<i32>();
                }
                Instruction::Lookupswitch(l) => {
                    total += std::mem::size_of::<crate::instruction::LookupSwitch>()
                        + l.pairs.len() * std::mem::size_of::<(i32, i32)>();
                }
                _ => {}
            }
        }
        total
    }

    /// The stream of `code` (padded) with the one-byte `ldc` at each pc of
    /// `widened` decoded as `ldc_w` of the paired index instead of its
    /// operand byte.
    ///
    /// The one deliberate exception to the module's "pure memoization"
    /// contract, for a frame that runs a method body a JVMTI redefinition
    /// replaced (`vm::runtime::interpreter::obsolete_frames`): its constants
    /// were appended to the class's pool above index 255, where the one-byte
    /// operand cannot name them, and rewriting `ldc` to `ldc_w` in the bytes
    /// would move every later pc. The caller keeps such a frame off every
    /// path that reads an `ldc` operand from the raw bytes.
    ///
    /// `None` unless the linear walk decodes the whole body (a pc it did not
    /// reach would fall back to `Instruction::decode` of the raw bytes) and
    /// every pc of `widened` starts a one-byte `ldc`.
    pub fn with_widened_ldc(code: &Arc<[u8]>, widened: &[(usize, u16)]) -> Option<Arc<Self>> {
        let mut q = Arc::try_unwrap(Self::build(code)?).ok()?;
        let end = q.pcs.get(q.ops.len()).map(|&pc| pc as usize);
        if end != Some(code.len().saturating_sub(2)) {
            return None;
        }
        for &(pc, index) in widened {
            let idx = q.index_of_pc_direct(pc)?;
            let op = q.ops.get_mut(idx)?;
            if !matches!(op, Instruction::Ldc(_)) {
                return None;
            }
            *op = Instruction::LdcW(index);
        }
        Some(Arc::new(q))
    }
}

/// Build the dense start bitmap for `starts_pcs` (the instruction start pcs,
/// ascending and strictly increasing), or `None` when the highest start pc
/// exceeds [`MAX_DENSE_START_PC`].
fn build_start_index(starts_pcs: &[u32]) -> Option<Box<[PcBlock]>> {
    let max_pc = *starts_pcs.last()? as usize;
    if max_pc > MAX_DENSE_START_PC {
        return None;
    }
    let nblocks = max_pc / BLOCK_BYTES + 1;
    let mut blocks = vec![PcBlock { starts: 0, cum: 0 }; nblocks];
    for &pc in starts_pcs {
        let pc = pc as usize;
        blocks[pc / BLOCK_BYTES].starts |= 1u64 << (pc % BLOCK_BYTES);
    }
    // Prefix-sum the per-block populations. `starts_pcs.len() <= max_pc + 1
    // <= 65536`, so the running total cannot overflow `u32`.
    let mut running: u32 = 0;
    for block in blocks.iter_mut() {
        block.cum = running;
        running += block.starts.count_ones();
    }
    Some(blocks.into_boxed_slice())
}

/// How many local-variable slots executing `code` can touch, or `None` when
/// that cannot be bounded from the bytecode alone.
///
/// The interpreter's raw-bytecode fast path indexes a frame's locals with no
/// bounds check on the short forms (`iload_0..3`, `lstore_3`, ...), on the
/// verifier's proof that every index is below `max_locals`. A method with no
/// such proof (no published type maps: a CDS class, a per-class
/// `skip_verification`, a synthetic frame) is admitted to that path only when
/// this bound fits its locals — see `fast_path_admitted_uncached` in
/// `vm::runtime::interpreter`, which memoizes the answer per method.
///
/// Each access counts as its index plus its width: a category-2 load or store
/// at `n` reaches `n + 2` (JVMS §6.5 `lload`: "both `index` and `index + 1`").
///
/// The bound is taken over the linear instruction walk [`QuickenedCode::build`]
/// makes, so it covers every pc the fast path can dispatch only while control
/// cannot leave that walk. `None` is returned when it can:
///
/// * the walk stops on a decode error before the end of the code (the fast
///   path would dispatch the bytes past it);
/// * a branch or switch target, or one of `handler_pcs` (the exception
///   table's handler entries), is not an instruction start of the walk — a
///   jump into an operand byte would dispatch it as an opcode;
/// * the method contains `ret`, whose target is a runtime value.
///
/// `code` is the padded array the interpreter dispatches from (two trailing
/// zero bytes), exactly as for [`QuickenedCode::build`]. Allocates two small
/// side tables; called once per method, not per dispatch.
pub fn fast_path_local_reach(
    code: &[u8],
    handler_pcs: impl IntoIterator<Item = usize>,
) -> Option<usize> {
    fast_path_reach(code, handler_pcs, None).map(|r| r.locals)
}

/// What [`fast_path_reach`] proves about a method's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastPathReach {
    /// Local-variable slots the code can touch ([`fast_path_local_reach`]).
    pub locals: usize,
    /// The deepest the operand stack can get, in JVM words (a `long` or
    /// `double` counts two, JVMS §2.6.2), on every path from the entry and
    /// from every handler; `None` when that cannot be proven (see
    /// [`fast_path_reach`]).
    pub stack: Option<usize>,
}

/// [`fast_path_local_reach`], and in the same decode (interpreter round i1
/// wave 7) the operand-stack depth bound the verifier would otherwise
/// provide: the precondition of raw-pointer stack access for methods with no
/// verifier maps (`i1-L1-proposal-dispatch-loop-register-state-20260923.md`,
/// Stage 2). `None` exactly when [`fast_path_local_reach`] is `None`.
///
/// The depth pass is an abstract interpretation over the decoded
/// instructions: depth 0 at pc 0 and 1 (the thrown object) at each of
/// `handler_pcs`, each instruction's stack effect applied (field and method
/// descriptors read from `cp`), propagated along fall-through, branch and
/// switch edges. `stack` is `None` — unproven, never a guess — when `cp` is
/// `None` or a member reference does not resolve to a descriptor, an
/// instruction pops more than the stack holds, two paths reach one
/// instruction at different depths (JVMS §4.10.2.2 rejects that too), control
/// can fall off the end of the code, or the method uses `jsr` (whose `ret`
/// the walk already refuses).
///
/// Every depth here is in words. The interpreter's operand stack holds a
/// `long` or `double` in one slot, so its slot count never exceeds the word
/// count at the same pc: a word bound within `max_stack` bounds the slots.
pub fn fast_path_reach(
    code: &[u8],
    handler_pcs: impl IntoIterator<Item = usize>,
    cp: Option<&crate::constant_pool::ConstantPool>,
) -> Option<FastPathReach> {
    let handler_pcs: Vec<usize> = handler_pcs.into_iter().collect();
    let limit = code.len().saturating_sub(2);
    let mut starts = vec![0u64; limit / BLOCK_BYTES + 1];
    let mut targets: Vec<i64> = Vec::new();
    let mut reach = 0usize;
    let mut pc = 0usize;
    // The decoded instructions, for the stack pass.
    let mut insns: Vec<(usize, Instruction, usize)> = Vec::new();
    while pc < limit {
        let (insn, next) = Instruction::decode(code, pc).ok()?;
        if next <= pc {
            return None;
        }
        starts[pc / BLOCK_BYTES] |= 1u64 << (pc % BLOCK_BYTES);
        // Cast: a pc below the 64 KiB code bound, as a signed branch base.
        let here = pc as i64;
        let slots = match &insn {
            Instruction::Iload(n)
            | Instruction::Fload(n)
            | Instruction::Aload(n)
            | Instruction::Istore(n)
            | Instruction::Fstore(n)
            | Instruction::Astore(n) => usize::from(*n) + 1,
            Instruction::Lload(n)
            | Instruction::Dload(n)
            | Instruction::Lstore(n)
            | Instruction::Dstore(n) => usize::from(*n) + 2,
            Instruction::Iinc { index, .. } => usize::from(*index) + 1,
            Instruction::Ret(_) => return None,
            Instruction::Ifeq(off)
            | Instruction::Ifne(off)
            | Instruction::Iflt(off)
            | Instruction::Ifge(off)
            | Instruction::Ifgt(off)
            | Instruction::Ifle(off)
            | Instruction::IfIcmpeq(off)
            | Instruction::IfIcmpne(off)
            | Instruction::IfIcmplt(off)
            | Instruction::IfIcmpge(off)
            | Instruction::IfIcmpgt(off)
            | Instruction::IfIcmple(off)
            | Instruction::IfAcmpeq(off)
            | Instruction::IfAcmpne(off)
            | Instruction::Ifnull(off)
            | Instruction::Ifnonnull(off)
            | Instruction::Goto(off)
            | Instruction::Jsr(off) => {
                targets.push(here + i64::from(*off));
                0
            }
            Instruction::GotoW(off) | Instruction::JsrW(off) => {
                targets.push(here + i64::from(*off));
                0
            }
            Instruction::Tableswitch(t) => {
                targets.push(here + i64::from(t.default));
                targets.extend(t.offsets.iter().map(|off| here + i64::from(*off)));
                0
            }
            Instruction::Lookupswitch(l) => {
                targets.push(here + i64::from(l.default));
                targets.extend(l.pairs.iter().map(|(_, off)| here + i64::from(*off)));
                0
            }
            _ => 0,
        };
        reach = reach.max(slots);
        if cp.is_some() {
            insns.push((pc, insn, next));
        }
        pc = next;
    }
    // Every start recorded above is `< limit`, so `t < limit` also keeps the
    // bitmap index in range.
    let is_start = |t: usize| t < limit && (starts[t / BLOCK_BYTES] >> (t % BLOCK_BYTES)) & 1 == 1;
    // Cast: a non-negative in-method pc.
    let targets_ok = targets.iter().all(|&t| t >= 0 && is_start(t as usize));
    if !(targets_ok && handler_pcs.iter().all(|&h| is_start(h))) {
        return None;
    }
    let stack = cp.and_then(|cp| stack_depth_reach(&insns, limit, &handler_pcs, cp));
    Some(FastPathReach {
        locals: reach,
        stack,
    })
}

/// The stack pass of [`fast_path_reach`] over its decoded `insns` (every
/// branch target and handler already checked to be an instruction start).
fn stack_depth_reach(
    insns: &[(usize, Instruction, usize)],
    limit: usize,
    handler_pcs: &[usize],
    cp: &crate::constant_pool::ConstantPool,
) -> Option<usize> {
    if insns.is_empty() {
        return Some(0);
    }
    // pc -> instruction index (`u32::MAX`: not a start).
    let mut index = vec![u32::MAX; limit];
    for (i, (pc, _, _)) in insns.iter().enumerate() {
        // Cast: at most 64 Ki instructions.
        index[*pc] = i as u32;
    }
    let index_of = |pc: usize| -> Option<usize> {
        let i = *index.get(pc)?;
        (i != u32::MAX).then_some(i as usize) // Cast: widening
    };
    let mut depth: Vec<Option<u32>> = vec![None; insns.len()];
    let mut work: Vec<usize> = Vec::new();
    let mut max_depth = 0u32;
    // Record `d` as the depth on entry to the instruction at `pc`; `false`
    // when it conflicts with a depth already recorded there.
    let enter = |pc: usize, d: u32, depth: &mut Vec<Option<u32>>, work: &mut Vec<usize>| {
        let Some(i) = index_of(pc) else {
            return false;
        };
        match depth[i] {
            Some(seen) => seen == d,
            None => {
                depth[i] = Some(d);
                work.push(i);
                true
            }
        }
    };
    if !enter(0, 0, &mut depth, &mut work) {
        return None;
    }
    for &h in handler_pcs {
        if !enter(h, 1, &mut depth, &mut work) {
            return None;
        }
    }
    max_depth = max_depth.max(u32::from(!handler_pcs.is_empty()));
    while let Some(i) = work.pop() {
        let (pc, insn, next) = &insns[i];
        let d = depth[i]?;
        let (pops, pushes) = stack_effect(insn, cp)?;
        let after = d.checked_sub(u32::from(pops))? + u32::from(pushes);
        // The words an instruction holds at once never exceed max(before,
        // after): every effect pops its operands before pushing results.
        max_depth = max_depth.max(after);
        // Cast: a pc below the 64 KiB code bound, as a signed branch base.
        let here = *pc as i64;
        let target = |off: i64| usize::try_from(here + off).ok();
        let mut succ: Vec<usize> = Vec::new();
        let falls_through = match insn {
            Instruction::Ifeq(off)
            | Instruction::Ifne(off)
            | Instruction::Iflt(off)
            | Instruction::Ifge(off)
            | Instruction::Ifgt(off)
            | Instruction::Ifle(off)
            | Instruction::IfIcmpeq(off)
            | Instruction::IfIcmpne(off)
            | Instruction::IfIcmplt(off)
            | Instruction::IfIcmpge(off)
            | Instruction::IfIcmpgt(off)
            | Instruction::IfIcmple(off)
            | Instruction::IfAcmpeq(off)
            | Instruction::IfAcmpne(off)
            | Instruction::Ifnull(off)
            | Instruction::Ifnonnull(off) => {
                succ.push(target(i64::from(*off))?);
                true
            }
            Instruction::Goto(off) => {
                succ.push(target(i64::from(*off))?);
                false
            }
            Instruction::GotoW(off) => {
                succ.push(target(i64::from(*off))?);
                false
            }
            Instruction::Tableswitch(t) => {
                succ.push(target(i64::from(t.default))?);
                for off in t.offsets.iter() {
                    succ.push(target(i64::from(*off))?);
                }
                false
            }
            Instruction::Lookupswitch(l) => {
                succ.push(target(i64::from(l.default))?);
                for (_, off) in l.pairs.iter() {
                    succ.push(target(i64::from(*off))?);
                }
                false
            }
            Instruction::Ireturn
            | Instruction::Lreturn
            | Instruction::Freturn
            | Instruction::Dreturn
            | Instruction::Areturn
            | Instruction::Return
            | Instruction::Athrow => false,
            _ => true,
        };
        if falls_through {
            // Falling off the end of the code is unprovable (JVMS §4.9.2
            // forbids it; the fast path would dispatch the padding).
            if *next >= limit {
                return None;
            }
            succ.push(*next);
        }
        for s in succ {
            if !enter(s, after, &mut depth, &mut work) {
                return None;
            }
        }
    }
    // Cast: widening.
    Some(max_depth as usize)
}

/// `(words popped, words pushed)` by `insn` (JVMS §6.5), with field and
/// method descriptors from `cp`. `None` for an instruction the stack pass
/// does not prove (`jsr`, `ret`, a stray `wide`) or a member reference that
/// does not resolve.
fn stack_effect(insn: &Instruction, cp: &crate::constant_pool::ConstantPool) -> Option<(u16, u16)> {
    use Instruction as I;
    Some(match insn {
        I::Nop | I::Iinc { .. } | I::Goto(_) | I::GotoW(_) | I::Return => (0, 0),
        I::AconstNull
        | I::IconstM1
        | I::Iconst0
        | I::Iconst1
        | I::Iconst2
        | I::Iconst3
        | I::Iconst4
        | I::Iconst5
        | I::Fconst0
        | I::Fconst1
        | I::Fconst2
        | I::Bipush(_)
        | I::Sipush(_)
        | I::Ldc(_)
        | I::LdcW(_)
        | I::Iload(_)
        | I::Fload(_)
        | I::Aload(_)
        | I::New(_) => (0, 1),
        I::Lconst0
        | I::Lconst1
        | I::Dconst0
        | I::Dconst1
        | I::Ldc2W(_)
        | I::Lload(_)
        | I::Dload(_) => (0, 2),
        I::Iaload | I::Faload | I::Aaload | I::Baload | I::Caload | I::Saload => (2, 1),
        I::Laload | I::Daload => (2, 2),
        I::Istore(_) | I::Fstore(_) | I::Astore(_) | I::Pop => (1, 0),
        I::Lstore(_) | I::Dstore(_) | I::Pop2 => (2, 0),
        I::Iastore | I::Fastore | I::Aastore | I::Bastore | I::Castore | I::Sastore => (3, 0),
        I::Lastore | I::Dastore => (4, 0),
        I::Dup => (1, 2),
        I::DupX1 => (2, 3),
        I::DupX2 => (3, 4),
        I::Dup2 => (2, 4),
        I::Dup2X1 => (3, 5),
        I::Dup2X2 => (4, 6),
        I::Swap => (2, 2),
        I::Iadd
        | I::Isub
        | I::Imul
        | I::Idiv
        | I::Irem
        | I::Ishl
        | I::Ishr
        | I::Iushr
        | I::Iand
        | I::Ior
        | I::Ixor
        | I::Fadd
        | I::Fsub
        | I::Fmul
        | I::Fdiv
        | I::Frem
        | I::Fcmpl
        | I::Fcmpg => (2, 1),
        I::Ladd
        | I::Lsub
        | I::Lmul
        | I::Ldiv
        | I::Lrem
        | I::Land
        | I::Lor
        | I::Lxor
        | I::Dadd
        | I::Dsub
        | I::Dmul
        | I::Ddiv
        | I::Drem => (4, 2),
        I::Lshl | I::Lshr | I::Lushr => (3, 2),
        I::Ineg
        | I::Fneg
        | I::I2f
        | I::F2i
        | I::I2b
        | I::I2c
        | I::I2s
        | I::Newarray(_)
        | I::Anewarray(_)
        | I::Arraylength
        | I::Checkcast(_)
        | I::Instanceof(_) => (1, 1),
        I::Lneg | I::Dneg | I::L2d | I::D2l => (2, 2),
        I::I2l | I::I2d | I::F2l | I::F2d => (1, 2),
        I::L2i | I::L2f | I::D2i | I::D2f => (2, 1),
        I::Lcmp | I::Dcmpl | I::Dcmpg => (4, 1),
        I::Ifeq(_)
        | I::Ifne(_)
        | I::Iflt(_)
        | I::Ifge(_)
        | I::Ifgt(_)
        | I::Ifle(_)
        | I::Ifnull(_)
        | I::Ifnonnull(_)
        | I::Tableswitch(_)
        | I::Lookupswitch(_)
        | I::Ireturn
        | I::Freturn
        | I::Areturn
        | I::Athrow
        | I::Monitorenter
        | I::Monitorexit => (1, 0),
        I::IfIcmpeq(_)
        | I::IfIcmpne(_)
        | I::IfIcmplt(_)
        | I::IfIcmpge(_)
        | I::IfIcmpgt(_)
        | I::IfIcmple(_)
        | I::IfAcmpeq(_)
        | I::IfAcmpne(_)
        | I::Lreturn
        | I::Dreturn => (2, 0),
        I::Getstatic(idx) => (0, field_words(cp, *idx)?),
        I::Putstatic(idx) => (field_words(cp, *idx)?, 0),
        I::Getfield(idx) => (1, field_words(cp, *idx)?),
        I::Putfield(idx) => (1 + field_words(cp, *idx)?, 0),
        I::Invokestatic(idx) | I::Invokedynamic(idx) => member_method_words(cp, *idx)?,
        I::Invokevirtual(idx) | I::Invokespecial(idx) | I::Invokeinterface { index: idx, .. } => {
            let (args, ret) = member_method_words(cp, *idx)?;
            (args.checked_add(1)?, ret)
        }
        I::Multianewarray { dimensions, .. } => (u16::from(*dimensions), 1),
        I::Jsr(_) | I::JsrW(_) | I::Ret(_) | I::Wide => return None,
    })
}

/// The descriptor of the member (field, method, interface method or
/// `invokedynamic` call site) constant `index` names.
fn member_descriptor(cp: &crate::constant_pool::ConstantPool, index: u16) -> Option<&str> {
    use crate::constant_pool::ConstantPoolEntry as E;
    let nat = match cp.get(index)? {
        E::FieldReference {
            name_and_type_index,
            ..
        }
        | E::MethodReference {
            name_and_type_index,
            ..
        }
        | E::InterfaceMethodReference {
            name_and_type_index,
            ..
        }
        | E::InvokeDynamic {
            name_and_type_index,
            ..
        } => *name_and_type_index,
        _ => return None,
    };
    cp.get_name_and_type(nat).map(|(_, descriptor)| descriptor)
}

/// Words a value of field type `descriptor` occupies: 2 for `J` / `D`.
fn type_words(descriptor: &str) -> Option<u16> {
    match descriptor.as_bytes().first()? {
        b'J' | b'D' => Some(2),
        b'V' => None,
        _ => Some(1),
    }
}

/// Words of the field constant `index` names.
fn field_words(cp: &crate::constant_pool::ConstantPool, index: u16) -> Option<u16> {
    type_words(member_descriptor(cp, index)?)
}

/// `(argument words, return words)` of the method constant `index` names
/// (no receiver).
fn member_method_words(cp: &crate::constant_pool::ConstantPool, index: u16) -> Option<(u16, u16)> {
    method_descriptor_words(member_descriptor(cp, index)?)
}

/// `(argument words, return words)` of a method descriptor such as
/// `(IJ[Ljava/lang/String;)D` — `(1 + 2 + 1, 2)`.
fn method_descriptor_words(descriptor: &str) -> Option<(u16, u16)> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1;
    let mut args: u16 = 0;
    while *bytes.get(i)? != b')' {
        let start = i;
        while *bytes.get(i)? == b'[' {
            i += 1;
        }
        if *bytes.get(i)? == b'L' {
            while *bytes.get(i)? != b';' {
                i += 1;
            }
        }
        i += 1;
        let words = if i == start + 1 {
            type_words(&descriptor[start..i])?
        } else {
            1 // an array or a class reference
        };
        args = args.checked_add(words)?;
    }
    let ret = match bytes.get(i + 1)? {
        b'V' => 0,
        _ => type_words(descriptor.get(i + 1..)?)?,
    };
    Some((args, ret))
}

/// Is `CRATONVM_QUICKEN_STATS` set? Read once — this gates a diagnostic only.
///
/// Public so the interpreter's fast-path admission census reports under the
/// same switch as the quickening footprint it sits beside.
pub fn stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_QUICKEN_STATS").is_some())
}

/// Record one fast-path veto (see [`STAT_FAST_PATH_VETOED`]) and return the
/// running total, for the caller's census line.
pub fn note_fast_path_veto() -> usize {
    STAT_FAST_PATH_VETOED.fetch_add(1, Ordering::Relaxed) + 1
}

/// Print the running footprint of the quickened streams to stderr.
///
/// This is how the per-method memory cost of quickening is measured: run any
/// workload with `CRATONVM_QUICKEN_STATS=1` and read the last line.
pub fn report_stats() {
    let (methods, ops, bytes, code_bytes, unq) = quicken_stats();
    let (index_bytes, no_index, truncated) = quicken_index_stats();
    let per_method = if methods > 0 { bytes / methods } else { 0 };
    let per_insn = if ops > 0 {
        bytes as f64 / ops as f64
    } else {
        0.0
    };
    let per_code_byte = if code_bytes > 0 {
        bytes as f64 / code_bytes as f64
    } else {
        0.0
    };
    let index_per_method = if methods > 0 {
        index_bytes / methods
    } else {
        0
    };
    eprintln!(
        "[quicken] methods={methods} insns={ops} stream_bytes={bytes} \
bytecode_bytes={code_bytes} unquickenable={unq} truncated={truncated} \
bytes_per_method={per_method} bytes_per_insn={per_insn:.1} \
bytes_per_code_byte={per_code_byte:.2} index_bytes={index_bytes} \
index_bytes_per_method={index_per_method} no_dense_index={no_index} \
insn_record={} fast_path_vetoed={}",
        std::mem::size_of::<Instruction>(),
        STAT_FAST_PATH_VETOED.load(Ordering::Relaxed)
    );
}

/// Aggregate footprint of every quickened stream built so far.
///
/// Returns `(methods, instructions, stream_bytes, bytecode_bytes,
/// unquickenable_methods)`.
pub fn quicken_stats() -> (usize, usize, usize, usize, usize) {
    (
        STAT_METHODS.load(Ordering::Relaxed),
        STAT_OPS.load(Ordering::Relaxed),
        STAT_BYTES.load(Ordering::Relaxed),
        STAT_CODE_BYTES.load(Ordering::Relaxed),
        STAT_UNQUICKENABLE.load(Ordering::Relaxed),
    )
}

/// Footprint and coverage of the dense pc → index accelerator.
///
/// Returns `(index_bytes, methods_without_a_dense_index, truncated_methods)`.
/// `index_bytes` is already included in `quicken_stats().2`.
pub(crate) fn quicken_index_stats() -> (usize, usize, usize) {
    (
        STAT_INDEX_BYTES.load(Ordering::Relaxed),
        STAT_NO_INDEX.load(Ordering::Relaxed),
        STAT_TRUNCATED.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Opcode-pair census (dispatch-loop proposal, Stage 0)
// ---------------------------------------------------------------------------
//
// `CRATONVM_QUICKEN_STATS=pairs` counts, process-wide, every pair of bytecodes
// the interpreter's raw-bytecode fast path dispatches back to back in one
// frame where the second is the FALL-THROUGH successor of the first — the
// pairs a superinstruction could fuse — and prints the most frequent at exit.
// It is the measurement the superinstruction proposal asks for before any new
// fusion is chosen by reading. A fused group dispatches once, so its interior
// pairs do not appear: the census ranks what still dispatches separately.
// Methods the fast path does not admit (the decoded path) are not counted.
//
// Any other value of the variable keeps its old meaning (the footprint
// report) and arms nothing here. When the census is off the interpreter pays
// nothing new: its hook shares the loop-top test that the debugger gate
// already costs.

/// Slots in [`PAIR_COUNTS`]: one per `(first, second)` opcode pair.
const PAIR_SLOTS: usize = 256 * 256;

/// Dispatch counts per `(first << 8) | second` opcode pair.
static PAIR_COUNTS: [AtomicU64; PAIR_SLOTS] = [const { AtomicU64::new(0) }; PAIR_SLOTS];

/// Is the opcode-pair census armed (`CRATONVM_QUICKEN_STATS=pairs`)? Read
/// once; the interpreter hoists it per dispatch-loop entry.
pub fn pair_census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_QUICKEN_STATS")
            .is_some_and(|v| v.to_str() == Some("pairs"))
    })
}

/// Count one dispatch of `second` straight after its fall-through
/// predecessor `first`.
#[inline]
pub fn record_opcode_pair(first: u8, second: u8) {
    PAIR_COUNTS[(usize::from(first) << 8) | usize::from(second)].fetch_add(1, Ordering::Relaxed);
}

/// The `n` most frequent recorded pairs, most frequent first, as
/// `(first, second, count)`. Ties are broken by opcode order so the report is
/// deterministic.
pub fn opcode_pair_census_top(n: usize) -> Vec<(u8, u8, u64)> {
    let mut rows: Vec<(u8, u8, u64)> = PAIR_COUNTS
        .iter()
        .enumerate()
        .filter_map(|(slot, c)| {
            let count = c.load(Ordering::Relaxed);
            // Cast: `slot < 65536`, so both halves fit a byte.
            (count > 0).then_some(((slot >> 8) as u8, (slot & 0xff) as u8, count))
        })
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    rows.truncate(n);
    rows
}

/// Print the census (top 60 pairs with their share of all counted pairs) to
/// stderr. A no-op unless [`pair_census_enabled`]; called from the VM's exit
/// report.
pub fn report_opcode_pair_census() {
    if !pair_census_enabled() {
        return;
    }
    let total: u64 = PAIR_COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    eprintln!("[quicken] opcode-pair census: {total} fall-through pairs dispatched separately");
    for (i, (a, b, count)) in opcode_pair_census_top(60).into_iter().enumerate() {
        // Cast: a share for display only.
        let pct = if total > 0 {
            count as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        eprintln!(
            "[quicken]   #{:<2} {:>14} -> {:<14} {count:>14} {pct:5.2}%",
            i + 1,
            opcode_mnemonic(a),
            opcode_mnemonic(b),
        );
    }
}

/// The length of the instruction `opcode` begins when it is fixed by the
/// opcode alone (JVMS §6.5), or 0 for the variable-length `tableswitch`,
/// `lookupswitch` and `wide` and for every unassigned opcode — which the
/// census then never pairs with a successor.
pub const fn fixed_instruction_length(opcode: u8) -> u8 {
    match opcode {
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa8
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        0xaa | 0xab | 0xc4 | 0xca..=0xff => 0,
        _ => 1,
    }
}

/// JVMS mnemonic of `opcode`, `"?"` for an unassigned one.
pub fn opcode_mnemonic(opcode: u8) -> &'static str {
    const NAMES: [&str; 0xca] = [
        "nop",
        "aconst_null",
        "iconst_m1",
        "iconst_0",
        "iconst_1",
        "iconst_2",
        "iconst_3",
        "iconst_4",
        "iconst_5",
        "lconst_0",
        "lconst_1",
        "fconst_0",
        "fconst_1",
        "fconst_2",
        "dconst_0",
        "dconst_1",
        "bipush",
        "sipush",
        "ldc",
        "ldc_w",
        "ldc2_w",
        "iload",
        "lload",
        "fload",
        "dload",
        "aload",
        "iload_0",
        "iload_1",
        "iload_2",
        "iload_3",
        "lload_0",
        "lload_1",
        "lload_2",
        "lload_3",
        "fload_0",
        "fload_1",
        "fload_2",
        "fload_3",
        "dload_0",
        "dload_1",
        "dload_2",
        "dload_3",
        "aload_0",
        "aload_1",
        "aload_2",
        "aload_3",
        "iaload",
        "laload",
        "faload",
        "daload",
        "aaload",
        "baload",
        "caload",
        "saload",
        "istore",
        "lstore",
        "fstore",
        "dstore",
        "astore",
        "istore_0",
        "istore_1",
        "istore_2",
        "istore_3",
        "lstore_0",
        "lstore_1",
        "lstore_2",
        "lstore_3",
        "fstore_0",
        "fstore_1",
        "fstore_2",
        "fstore_3",
        "dstore_0",
        "dstore_1",
        "dstore_2",
        "dstore_3",
        "astore_0",
        "astore_1",
        "astore_2",
        "astore_3",
        "iastore",
        "lastore",
        "fastore",
        "dastore",
        "aastore",
        "bastore",
        "castore",
        "sastore",
        "pop",
        "pop2",
        "dup",
        "dup_x1",
        "dup_x2",
        "dup2",
        "dup2_x1",
        "dup2_x2",
        "swap",
        "iadd",
        "ladd",
        "fadd",
        "dadd",
        "isub",
        "lsub",
        "fsub",
        "dsub",
        "imul",
        "lmul",
        "fmul",
        "dmul",
        "idiv",
        "ldiv",
        "fdiv",
        "ddiv",
        "irem",
        "lrem",
        "frem",
        "drem",
        "ineg",
        "lneg",
        "fneg",
        "dneg",
        "ishl",
        "lshl",
        "ishr",
        "lshr",
        "iushr",
        "lushr",
        "iand",
        "land",
        "ior",
        "lor",
        "ixor",
        "lxor",
        "iinc",
        "i2l",
        "i2f",
        "i2d",
        "l2i",
        "l2f",
        "l2d",
        "f2i",
        "f2l",
        "f2d",
        "d2i",
        "d2l",
        "d2f",
        "i2b",
        "i2c",
        "i2s",
        "lcmp",
        "fcmpl",
        "fcmpg",
        "dcmpl",
        "dcmpg",
        "ifeq",
        "ifne",
        "iflt",
        "ifge",
        "ifgt",
        "ifle",
        "if_icmpeq",
        "if_icmpne",
        "if_icmplt",
        "if_icmpge",
        "if_icmpgt",
        "if_icmple",
        "if_acmpeq",
        "if_acmpne",
        "goto",
        "jsr",
        "ret",
        "tableswitch",
        "lookupswitch",
        "ireturn",
        "lreturn",
        "freturn",
        "dreturn",
        "areturn",
        "return",
        "getstatic",
        "putstatic",
        "getfield",
        "putfield",
        "invokevirtual",
        "invokespecial",
        "invokestatic",
        "invokeinterface",
        "invokedynamic",
        "new",
        "newarray",
        "anewarray",
        "arraylength",
        "athrow",
        "checkcast",
        "instanceof",
        "monitorenter",
        "monitorexit",
        "wide",
        "multianewarray",
        "ifnull",
        "ifnonnull",
        "goto_w",
        "jsr_w",
    ];
    NAMES.get(usize::from(opcode)).copied().unwrap_or("?")
}

// ---------------------------------------------------------------------------
// Process-wide interning of quickened streams
// ---------------------------------------------------------------------------

/// Number of shards in the intern table. Quickening happens once per method,
/// so contention is low; the shards exist so unrelated threads warming up
/// different classes do not serialise on a single lock.
const SHARDS: usize = 16;

/// Soft cap on entries **per shard**. A pathological workload that redefines
/// classes in a loop would otherwise pin every historical bytecode array
/// forever. On overflow the shard is cleared wholesale: in-flight users keep
/// their `Arc`s alive, and the next lookup simply rebuilds.
const SHARD_CAP: usize = 8192;

type Shard = RwLock<FxHashMap<(usize, usize), Option<Arc<QuickenedCode>>>>;

fn shards() -> &'static [Shard; SHARDS] {
    static SHARDS_ONCE: std::sync::OnceLock<[Shard; SHARDS]> = std::sync::OnceLock::new();
    SHARDS_ONCE.get_or_init(|| std::array::from_fn(|_| RwLock::new(FxHashMap::default())))
}

/// Get (or build) the quickened stream for `code`, interned process-wide so
/// that every call site sharing this bytecode allocation shares one stream.
///
/// The key is `(address, length)` of the code allocation. That is sound
/// because every cached `Some` entry holds a strong `Arc<[u8]>` to its own
/// code (via [`QuickenedCode::code`]), so a cached address can never be
/// recycled by a different live allocation while the entry exists. `None`
/// (not quickenable) entries hold no such pin, so a stale-address hit could
/// at worst mislabel a *different* method as unquickenable — a pure
/// pessimisation that routes it through the original decode path.
pub fn intern(code: &Arc<[u8]>) -> Option<Arc<QuickenedCode>> {
    let key = (code.as_ptr() as usize, code.len());
    let shard = &shards()[(key.0 >> 4) % SHARDS];
    if let Some(hit) = shard.read().get(&key) {
        return hit.clone();
    }
    let built = QuickenedCode::build(code);
    let mut guard = shard.write();
    if guard.len() >= SHARD_CAP && !guard.contains_key(&key) {
        guard.clear();
    }
    // Return what the TABLE holds, not what this thread built. Another thread
    // can intern the same allocation between the read lock above and this
    // write lock; handing back our own copy gave one code allocation two live
    // streams (twice the footprint, and two answers to "this method's stream")
    // although the doc above promises one. The losing build is dropped here.
    guard.entry(key).or_insert(built).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(mut v: Vec<u8>) -> Arc<[u8]> {
        v.push(0);
        v.push(0);
        Arc::from(v.into_boxed_slice())
    }

    /// Ground truth: the exact set of `(pc, insn, next_pc)` the interpreter's
    /// original `Instruction::decode` walk produces, computed independently
    /// of `QuickenedCode`.
    fn linear_walk(code: &Arc<[u8]>) -> Vec<(usize, Instruction, usize)> {
        let limit = code.len() - 2;
        let mut out = Vec::new();
        let mut pc = 0usize;
        while pc < limit {
            match Instruction::decode(code, pc) {
                Ok((insn, next)) if next > pc => {
                    out.push((pc, insn, next));
                    pc = next;
                }
                _ => break,
            }
        }
        out
    }

    /// A method body exercising every shape that matters: single- and
    /// multi-byte instructions, a `wide` prefix, a `tableswitch` and a
    /// `lookupswitch` (both with alignment padding), and a back-edge.
    fn mixed_method() -> Arc<[u8]> {
        let mut b: Vec<u8> = Vec::new();
        b.push(0x03); // 0: iconst_0
        b.push(0x3c); // 1: istore_1
        b.push(0xc4); // 2: wide iload 300
        b.push(0x15);
        b.extend(300u16.to_be_bytes());
        b.push(0xc4); // 6: wide iinc 300, 5
        b.push(0x84);
        b.extend(300u16.to_be_bytes());
        b.extend(5i16.to_be_bytes());
        b.push(0x10); // 12: bipush 7
        b.push(0x07);
        b.push(0x60); // 14: iadd
        b.push(0x3c); // 15: istore_1
                      // 16: tableswitch (pc 16, next byte 17 -> pad to 20)
        b.push(0xaa);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend(40i32.to_be_bytes()); // default
        b.extend(0i32.to_be_bytes()); // low
        b.extend(2i32.to_be_bytes()); // high  -> 3 offsets
        b.extend(4i32.to_be_bytes());
        b.extend(8i32.to_be_bytes());
        b.extend(12i32.to_be_bytes());
        // lookupswitch
        b.push(0xab);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend(16i32.to_be_bytes()); // default
        b.extend(2i32.to_be_bytes()); // npairs
        b.extend(100i32.to_be_bytes());
        b.extend(20i32.to_be_bytes());
        b.extend(200i32.to_be_bytes());
        b.extend(24i32.to_be_bytes());
        b.push(0x1b); // iload_1
        b.push(0xa7); // goto -2 (back-edge)
        b.extend((-2i16).to_be_bytes());
        b.push(0xb1); // return
        padded(b)
    }

    #[test]
    fn pc_block_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<PcBlock>(), 16);
        assert_eq!(std::mem::align_of::<PcBlock>(), 8);
    }

    #[test]
    fn quickened_matches_decode_at_every_pc() {
        // iconst_0; istore_1; iload_1; bipush 7; iadd; istore_1; return
        let code = padded(vec![0x03, 0x3c, 0x1b, 0x10, 0x07, 0x60, 0x3c, 0xb1]);
        let q = QuickenedCode::build(&code).expect("quickenable");
        let mut pc = 0usize;
        let mut idx = 0usize;
        while pc < code.len() - 2 {
            let (expect_insn, expect_next) = Instruction::decode(&code, pc).unwrap();
            let i = q.index_of_pc(pc, idx).expect("pc is an instruction start");
            assert_eq!(i, idx);
            assert_eq!(q.pc_of(i), pc);
            assert_eq!(*q.op(i), expect_insn);
            assert_eq!(q.next_pc(i), expect_next);
            pc = expect_next;
            idx = i + 1;
        }
        assert_eq!(q.len(), idx);
    }

    /// The core O(1) requirement: for a method containing `wide`,
    /// `tableswitch` and `lookupswitch`, resolving an *arbitrary* pc must
    /// agree exactly with the independent linear walk — both for the pcs
    /// that are instruction starts and for every interior byte that is not.
    #[test]
    fn random_access_pc_lookup_matches_linear_walk() {
        let code = mixed_method();
        let truth = linear_walk(&code);
        assert!(truth.len() > 8, "fixture should be non-trivial");
        let q = QuickenedCode::build(&code).expect("quickenable");
        assert!(q.has_dense_index());
        assert_eq!(q.len(), truth.len());

        // Every byte offset in the method, probed out of order and with a
        // deliberately wrong hint every time.
        let limit = code.len() - 2;
        for pc in (0..limit).rev() {
            let expected = truth.iter().position(|(p, _, _)| *p == pc);
            for hint in [0usize, 1, 3, q.len(), q.len() + 7, usize::MAX] {
                assert_eq!(
                    q.index_of_pc(pc, hint),
                    expected,
                    "pc={pc} hint={hint} disagrees with the linear walk"
                );
            }
            assert_eq!(q.index_of_pc_direct(pc), expected, "pc={pc} (hintless)");
            match expected {
                Some(i) => {
                    let (insn, next) = q.resolve(pc).expect("start resolves");
                    assert_eq!(*insn, truth[i].1, "pc={pc} instruction");
                    assert_eq!(next, truth[i].2, "pc={pc} next_pc");
                    assert_eq!(q.pc_of(i), pc);
                    assert_eq!(q.next_pc(i), truth[i].2);
                }
                None => assert!(q.resolve(pc).is_none(), "pc={pc} must not resolve"),
            }
        }
        // Past the end of the method is never a start.
        assert_eq!(q.index_of_pc_direct(limit + 1), None);
        assert_eq!(q.index_of_pc_direct(usize::MAX), None);
    }

    #[test]
    fn wide_prefixed_instruction_is_one_record() {
        // wide iload 300; wide iinc 300, 5; return
        let mut b = vec![0xc4, 0x15];
        b.extend(300u16.to_be_bytes());
        b.push(0xc4);
        b.push(0x84);
        b.extend(300u16.to_be_bytes());
        b.extend(5i16.to_be_bytes());
        b.push(0xb1);
        let code = padded(b);
        let q = QuickenedCode::build(&code).unwrap();
        assert_eq!(q.len(), 3);
        assert_eq!(*q.op(0), Instruction::Iload(300));
        assert_eq!(q.pc_of(0), 0);
        assert_eq!(q.next_pc(0), 4);
        assert_eq!(
            *q.op(1),
            Instruction::Iinc {
                index: 300,
                constant: 5
            }
        );
        assert_eq!(q.pc_of(1), 4);
        assert_eq!(q.next_pc(1), 10);
        assert_eq!(*q.op(2), Instruction::Return);
        // Interior bytes of both wide instructions are not starts.
        for pc in [1usize, 2, 3, 5, 6, 7, 8, 9] {
            assert_eq!(q.index_of_pc_direct(pc), None, "interior pc={pc}");
        }
    }

    #[test]
    fn interior_pc_is_not_an_instruction_start() {
        // bipush 7 occupies pc 0..2; pc 1 is its operand byte.
        let code = padded(vec![0x10, 0x07, 0xb1]);
        let q = QuickenedCode::build(&code).unwrap();
        assert_eq!(q.index_of_pc(0, 0), Some(0));
        assert_eq!(q.index_of_pc(1, 0), None);
        assert_eq!(q.index_of_pc(1, 1), None);
        assert_eq!(q.index_of_pc(2, 1), Some(1));
    }

    #[test]
    fn wrong_hint_still_resolves() {
        let code = padded(vec![0x03, 0x3c, 0x1b, 0x10, 0x07, 0x60, 0x3c, 0xb1]);
        let q = QuickenedCode::build(&code).unwrap();
        for i in 0..q.len() {
            let pc = q.pc_of(i);
            // Every possible (including nonsensical) hint must agree.
            for hint in 0..q.len() + 3 {
                assert_eq!(q.index_of_pc(pc, hint), Some(i), "pc={pc} hint={hint}");
            }
            assert_eq!(q.index_of_pc(pc, usize::MAX), Some(i));
        }
    }

    #[test]
    fn switch_tables_are_interned_out_of_line() {
        // tableswitch at pc 0 with 3-byte alignment padding.
        let mut body = vec![0xaa, 0, 0, 0];
        body.extend(&12i32.to_be_bytes()); // default
        body.extend(&0i32.to_be_bytes()); // low
        body.extend(&1i32.to_be_bytes()); // high
        body.extend(&20i32.to_be_bytes()); // offsets[0]
        body.extend(&24i32.to_be_bytes()); // offsets[1]
        let code = padded(body);
        let q = QuickenedCode::build(&code).unwrap();
        match q.op(0) {
            Instruction::Tableswitch(t) => {
                assert_eq!(t.low, 0);
                assert_eq!(t.high, 1);
                assert_eq!(t.offsets, vec![20, 24]);
            }
            other => panic!("expected tableswitch, got {other:?}"),
        }
        // Executing a switch must not clone the table: the record is a
        // fixed-size `Instruction` regardless of table size.
        assert!(std::mem::size_of::<Instruction>() <= 16);
    }

    /// Both switch flavours must land in the stream as borrowed, interned
    /// payloads that repeated lookups hand back without re-decoding.
    #[test]
    fn both_switch_kinds_resolve_without_reallocating() {
        let code = mixed_method();
        let q = QuickenedCode::build(&code).unwrap();
        let mut table = None;
        let mut lookup = None;
        for i in 0..q.len() {
            match q.op(i) {
                Instruction::Tableswitch(t) => table = Some((i, Arc::as_ptr(t))),
                Instruction::Lookupswitch(l) => lookup = Some((i, Arc::as_ptr(l))),
                _ => {}
            }
        }
        let (ti, tptr) = table.expect("fixture has a tableswitch");
        let (li, lptr) = lookup.expect("fixture has a lookupswitch");
        // Re-resolving by pc yields the *same* payload allocation every time.
        for _ in 0..4 {
            let (insn, _) = q.resolve(q.pc_of(ti)).unwrap();
            match insn {
                Instruction::Tableswitch(t) => assert!(std::ptr::eq(Arc::as_ptr(t), tptr)),
                other => panic!("expected tableswitch, got {other:?}"),
            }
            let (insn, _) = q.resolve(q.pc_of(li)).unwrap();
            match insn {
                Instruction::Lookupswitch(l) => assert!(std::ptr::eq(Arc::as_ptr(l), lptr)),
                other => panic!("expected lookupswitch, got {other:?}"),
            }
        }
    }

    #[test]
    fn unquickenable_code_returns_none() {
        // 0xba (invokedynamic) needs 4 operand bytes; truncating mid-operand
        // makes the linear walk fail.
        let code: Arc<[u8]> = Arc::from(vec![0xba, 0x00].into_boxed_slice());
        assert!(QuickenedCode::build(&code).is_none());
        // A first instruction that cannot decode at all is also not quickened.
        let bad = padded(vec![0xfe, 0xb1]);
        assert!(QuickenedCode::build(&bad).is_none());
    }

    /// A decode failure partway through salvages the decodable prefix, so a
    /// switch before the bad bytes still dispatches from the interned stream
    /// instead of re-decoding (and re-allocating) on every execution.
    #[test]
    fn decode_failure_salvages_the_prefix() {
        let mut b = vec![0xaa, 0, 0, 0];
        b.extend(&12i32.to_be_bytes()); // default
        b.extend(&0i32.to_be_bytes()); // low
        b.extend(&1i32.to_be_bytes()); // high
        b.extend(&20i32.to_be_bytes()); // offsets[0]
        b.extend(&24i32.to_be_bytes()); // offsets[1]
        let bad_pc = b.len();
        b.push(0xfe); // undefined opcode: the walk stops here
        b.push(0x03);
        let code = padded(b);
        let q = QuickenedCode::build(&code).expect("prefix is salvageable");
        assert_eq!(q.len(), 1);
        assert!(matches!(q.op(0), Instruction::Tableswitch(_)));
        assert_eq!(q.index_of_pc_direct(0), Some(0));
        assert_eq!(q.next_pc(0), bad_pc);
        // The undecodable pc reports "not found" so the caller re-decodes
        // there and fails at exactly the pc it always did.
        assert_eq!(q.index_of_pc_direct(bad_pc), None);
        assert!(Instruction::decode(&code, bad_pc).is_err());
    }

    /// A method too large for the dense index falls back to binary search and
    /// must stay exactly as correct.
    #[test]
    fn oversized_method_falls_back_to_binary_search() {
        // One byte past the JVMS code_length ceiling, so the highest start pc
        // exceeds MAX_DENSE_START_PC.
        let n = MAX_DENSE_START_PC + 2;
        let mut body = vec![0x00u8; n - 1]; // nops
        body.push(0xb1); // return
        let code = padded(body);
        let q = QuickenedCode::build(&code).expect("quickenable");
        assert!(
            !q.has_dense_index(),
            "a start pc past the ceiling must not build a dense index"
        );
        assert_eq!(q.index_bytes(), 0);
        assert_eq!(q.len(), n);
        // Still exact, via the binary-search path.
        for pc in [0usize, 1, 63, 64, 65_534, 65_535, n - 1] {
            assert_eq!(q.index_of_pc_direct(pc), Some(pc), "pc={pc}");
            assert_eq!(q.pc_of(pc), pc);
        }
        assert_eq!(*q.op(n - 1), Instruction::Return);
        assert_eq!(q.index_of_pc_direct(n), None);
        assert_eq!(q.index_of_pc_direct(usize::MAX), None);

        // The largest method that *does* qualify keeps the dense index.
        let mut body = vec![0x00u8; MAX_DENSE_START_PC];
        body.push(0xb1);
        let code = padded(body);
        let q = QuickenedCode::build(&code).expect("quickenable");
        assert!(q.has_dense_index());
        assert_eq!(
            q.index_of_pc_direct(MAX_DENSE_START_PC),
            Some(MAX_DENSE_START_PC)
        );
        assert_eq!(*q.op(MAX_DENSE_START_PC), Instruction::Return);
    }

    /// Multi-block methods must carry the cumulative counts correctly across
    /// block boundaries — the part a single-block fixture cannot exercise.
    #[test]
    fn cumulative_counts_span_blocks() {
        // 200 bipush (2 bytes each) = 400 bytes, spanning 7 blocks, so every
        // start lands at an even pc and every odd pc is an operand byte.
        let mut body = Vec::new();
        for _ in 0..200 {
            body.push(0x10);
            body.push(0x2a);
        }
        let code = padded(body);
        let q = QuickenedCode::build(&code).unwrap();
        assert!(q.has_dense_index());
        assert_eq!(q.len(), 200);
        for i in 0..200 {
            assert_eq!(q.index_of_pc_direct(i * 2), Some(i), "start pc={}", i * 2);
            assert_eq!(
                q.index_of_pc_direct(i * 2 + 1),
                None,
                "operand pc={}",
                i * 2 + 1
            );
        }
    }

    #[test]
    fn heap_bytes_accounts_for_the_dense_index() {
        let code = mixed_method();
        let q = QuickenedCode::build(&code).unwrap();
        assert!(q.index_bytes() > 0);
        assert_eq!(q.index_bytes() % std::mem::size_of::<PcBlock>(), 0);
        let floor = q.index_bytes() + q.len() * std::mem::size_of::<Instruction>();
        assert!(
            q.heap_bytes() >= floor,
            "heap_bytes must include the dense index"
        );
    }

    /// The census's fall-through lengths agree with the decoder for every
    /// opcode whose length the opcode alone fixes, and name the three
    /// variable-length ones 0.
    #[test]
    fn fixed_instruction_length_matches_decode() {
        let mut checked = 0;
        for op in 0u8..=0xc9 {
            let len = fixed_instruction_length(op);
            if matches!(op, 0xaa | 0xab | 0xc4) {
                assert_eq!(len, 0, "{} is variable-length", opcode_mnemonic(op));
                continue;
            }
            assert!(len > 0, "{} has a fixed length", opcode_mnemonic(op));
            // Operands of 1 keep every index / count / dimension decodable.
            let mut body = vec![op];
            body.extend([1u8; 8]);
            if let Ok((_, next)) = Instruction::decode(&padded(body), 0) {
                assert_eq!(next, usize::from(len), "{}", opcode_mnemonic(op));
                checked += 1;
            }
        }
        assert!(checked > 190, "only {checked} opcodes decoded");
        assert_eq!(fixed_instruction_length(0xca), 0, "unassigned");
        assert_eq!(opcode_mnemonic(0x1a), "iload_0");
        assert_eq!(opcode_mnemonic(0xc9), "jsr_w");
        assert_eq!(opcode_mnemonic(0xca), "?");
    }

    /// Recorded pairs come back from the census, most frequent first.
    #[test]
    fn opcode_pair_census_counts_and_ranks() {
        // Unassigned opcodes: nothing else in the process records them.
        for _ in 0..3 {
            record_opcode_pair(0xfd, 0xfe);
        }
        record_opcode_pair(0xfd, 0xff);
        let top = opcode_pair_census_top(PAIR_SLOTS);
        let pos = |a: u8, b: u8| top.iter().position(|r| r.0 == a && r.1 == b);
        let three = pos(0xfd, 0xfe).expect("recorded");
        let one = pos(0xfd, 0xff).expect("recorded");
        assert!(top[three].2 >= 3 && top[one].2 >= 1);
        assert!(three < one, "ranked by count");
    }

    #[test]
    fn intern_returns_the_same_stream_for_the_same_allocation() {
        let code = padded(vec![0x03, 0xb1]);
        let a = intern(&code).unwrap();
        let b = intern(&code).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(a.code(), &code));
    }

    /// `fast_path_local_reach`: short, explicit and `wide` forms count their
    /// index plus their width; a category-2 access counts two slots.
    #[test]
    fn local_reach_counts_every_local_access_form() {
        let reach = |v: Vec<u8>| fast_path_local_reach(&padded(v), []);
        assert_eq!(reach(vec![0x03, 0xac]), Some(0), "no locals");
        assert_eq!(reach(vec![0x1d, 0xac]), Some(4), "iload_3");
        assert_eq!(reach(vec![0x09, 0x42, 0xb1]), Some(5), "lstore_3 reaches 4");
        assert_eq!(reach(vec![0x18, 0x06, 0xaf]), Some(8), "dload 6 reaches 7");
        assert_eq!(reach(vec![0x84, 0x07, 0x01, 0xb1]), Some(8), "iinc 7");
        // wide iload 300; ireturn
        assert_eq!(reach(vec![0xc4, 0x15, 0x01, 0x2c, 0xac]), Some(301));
    }

    /// Control that can leave the linear walk makes the reach unknown.
    #[test]
    fn local_reach_refuses_code_control_can_leave() {
        // iload_0; ifeq +4 (-> 5); iconst_1; ireturn — a branch to a start.
        let ok = vec![0x1a, 0x99, 0x00, 0x04, 0x04, 0xac];
        assert_eq!(fast_path_local_reach(&padded(ok.clone()), []), Some(1));
        assert_eq!(fast_path_local_reach(&padded(ok.clone()), [4]), Some(1));
        assert_eq!(
            fast_path_local_reach(&padded(ok), [2]),
            None,
            "a handler inside an instruction"
        );
        let refused = |v: Vec<u8>| fast_path_local_reach(&padded(v), []).is_none();
        // bipush 0x1d; goto -1 — lands on the operand byte, which is iload_3.
        assert!(
            refused(vec![0x10, 0x1d, 0xa7, 0xff, 0xff]),
            "into an operand"
        );
        assert!(refused(vec![0xa7, 0xff, 0xfb]), "goto -5 at pc 0");
        assert!(refused(vec![0xa9, 0x00]), "ret: a runtime target");
        // iload_0; <undefined opcode>; ireturn: the walk stops early.
        assert!(refused(vec![0x1a, 0xcb, 0xac]), "a truncated walk");
    }

    /// A pool with a `long` field `A.f` (#6) and a static method
    /// `A.m(IJLjava/lang/String;)D` (#10).
    fn member_pool() -> crate::constant_pool::ConstantPool {
        use crate::constant_pool::ConstantPoolEntry as E;
        let utf8 = |s: &str| E::Utf8(Arc::from(s));
        crate::constant_pool::ConstantPool::new(vec![
            E::Tombstone,
            utf8("f"),
            utf8("J"),
            E::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            },
            E::ClassReference { name_index: 5 },
            utf8("A"),
            E::FieldReference {
                class_index: 4,
                name_and_type_index: 3,
            },
            utf8("m"),
            utf8("(IJLjava/lang/String;)D"),
            E::NameAndType {
                name_index: 7,
                descriptor_index: 8,
            },
            E::MethodReference {
                class_index: 4,
                name_and_type_index: 9,
            },
        ])
    }

    /// Wave 7: `fast_path_reach` proves the operand-stack depth in words —
    /// member descriptors from the pool, loops at a consistent depth,
    /// handlers entered at depth 1 — and refuses (`stack: None`, locals still
    /// proven) an underflow, a join at two depths, an unresolvable member and
    /// code that falls off its end.
    #[test]
    fn stack_reach_is_proven_in_words_or_refused() {
        let cp = member_pool();
        let stack = |v: Vec<u8>, handlers: Vec<usize>| {
            fast_path_reach(&padded(v), handlers, Some(&cp)).and_then(|r| r.stack)
        };
        // getstatic #6 (J); lconst_1; ladd; putstatic #6; iconst_0; lconst_0;
        // aconst_null; invokestatic #10 (IJL)D; dreturn
        let members = vec![
            0xb2, 0, 6, 0x0a, 0x61, 0xb3, 0, 6, 0x03, 0x09, 0x01, 0xb8, 0, 10, 0xaf,
        ];
        assert_eq!(stack(members, vec![]), Some(4));
        // iconst_0; istore_0; L: iload_0; bipush 10; if_icmpge +9; iinc 0 1;
        // goto L (-9); return
        let lp = vec![
            0x03, 0x3b, 0x1a, 0x10, 10, 0xa2, 0, 9, 0x84, 0, 1, 0xa7, 0xff, 0xf7, 0xb1,
        ];
        assert_eq!(stack(lp, vec![]), Some(2));
        // iconst_0; ireturn; handler 2: astore_0; aload_0; athrow
        assert_eq!(stack(vec![0x03, 0xac, 0x4b, 0x2a, 0xbf], vec![2]), Some(1));
        assert_eq!(stack(vec![0x57, 0xb1], vec![]), None, "pop of nothing");
        // iconst_0; ifeq +5 (-> 6, depth 0); iconst_1; nop; return (depth 1)
        let join = vec![0x03, 0x99, 0, 5, 0x04, 0x00, 0xb1];
        assert_eq!(stack(join.clone(), vec![]), None, "a join at two depths");
        assert_eq!(fast_path_local_reach(&padded(join), []), Some(0));
        assert_eq!(
            stack(vec![0xb2, 0, 99, 0x57, 0xb1], vec![]),
            None,
            "no member #99"
        );
        assert_eq!(stack(vec![0x03], vec![]), None, "falls off the end");
        assert_eq!(
            fast_path_reach(&padded(vec![0x03, 0xac]), [], None).map(|r| r.stack),
            Some(None),
            "no pool, no stack proof"
        );
        assert_eq!(method_descriptor_words("()J"), Some((0, 2)));
        assert_eq!(
            method_descriptor_words("(IJ[DLjava/lang/String;[[Ljava/lang/Object;)V"),
            Some((6, 0))
        );
        assert_eq!(method_descriptor_words("(V)V"), None);
    }

    /// An obsolete body's `ldc` whose constant sits above index 255 is
    /// decoded as `ldc_w` of that index at its own pc; every other record,
    /// and every pc, is the plain build's. A pc that is not an `ldc`, and a
    /// body the walk cannot finish, are refused.
    #[test]
    fn widened_ldc_streams_replace_only_the_named_records() {
        // 0: ldc #4; 2: ldc #5; 4: pop; 5: pop; 6: return
        let code = padded(vec![0x12, 4, 0x12, 5, 0x57, 0x57, 0xb1]);
        let q = QuickenedCode::with_widened_ldc(&code, &[(2, 300)]).expect("widens");
        assert_eq!(q.resolve(0), Some((&Instruction::Ldc(4), 2)));
        assert_eq!(q.resolve(2), Some((&Instruction::LdcW(300), 4)));
        assert_eq!(q.resolve(4), Some((&Instruction::Pop, 5)));
        assert_eq!(q.len(), QuickenedCode::build(&code).map_or(0, |b| b.len()));
        assert!(
            QuickenedCode::with_widened_ldc(&code, &[(4, 300)]).is_none(),
            "not an ldc"
        );
        assert!(
            QuickenedCode::with_widened_ldc(&code, &[(1, 300)]).is_none(),
            "not an instruction start"
        );
        // ldc #4, then an undecodable opcode: the walk stops short.
        let truncated = padded(vec![0x12, 4, 0xff, 0xb1]);
        assert!(QuickenedCode::with_widened_ldc(&truncated, &[(0, 300)]).is_none());
    }
}
