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
//! and `arch-2026-07-26/quickened-dispatch-o1.md` for the
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
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Live count of quickened methods (one per distinct code allocation).
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

/// Is `CRATONVM_QUICKEN_STATS` set? Read once — this gates a diagnostic only.
fn stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_QUICKEN_STATS").is_some())
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
insn_record={}",
        std::mem::size_of::<Instruction>()
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
pub fn quicken_index_stats() -> (usize, usize, usize) {
    (
        STAT_INDEX_BYTES.load(Ordering::Relaxed),
        STAT_NO_INDEX.load(Ordering::Relaxed),
        STAT_TRUNCATED.load(Ordering::Relaxed),
    )
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
    guard.entry(key).or_insert_with(|| built.clone());
    built
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

    #[test]
    fn intern_returns_the_same_stream_for_the_same_allocation() {
        let code = padded(vec![0x03, 0xb1]);
        let a = intern(&code).unwrap();
        let b = intern(&code).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(a.code(), &code));
    }
}
