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
//! * A method whose linear walk hits a decode error is not quickened at
//!   all (`build` returns `None`); such methods keep decoding on demand
//!   and still fail at exactly the same pc they always did.
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
}

/// Live count of quickened methods (one per distinct code allocation).
static STAT_METHODS: AtomicUsize = AtomicUsize::new(0);
/// Total pre-decoded instruction records across all quickened methods.
static STAT_OPS: AtomicUsize = AtomicUsize::new(0);
/// Total heap bytes owned by quickened streams (records + pc table +
/// interned switch tables), excluding the pinned bytecode itself.
static STAT_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Total bytecode bytes covered by quickened streams.
static STAT_CODE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Methods whose linear walk failed to decode (never quickened).
static STAT_UNQUICKENABLE: AtomicUsize = AtomicUsize::new(0);

impl QuickenedCode {
    /// Pre-decode `code` into a quickened stream, or return `None` if the
    /// linear walk cannot decode the whole method.
    ///
    /// `code` is the **padded** array the interpreter dispatches from
    /// (two trailing zero bytes); the walk stops at `len - 2`, which is the
    /// same unpadded bound the dispatch loop uses.
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
        while pc < limit {
            let (insn, next) = match Instruction::decode(code, pc) {
                Ok(v) => v,
                Err(_) => {
                    STAT_UNQUICKENABLE.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            // `decode` must make progress, otherwise the walk would spin.
            if next <= pc || next > u32::MAX as usize {
                STAT_UNQUICKENABLE.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            pcs.push(pc as u32);
            ops.push(insn);
            pc = next;
        }
        // Sentinel: the `next_pc` the final instruction reported.
        pcs.push(pc as u32);

        let q = Arc::new(QuickenedCode {
            code: Arc::clone(code),
            ops: ops.into_boxed_slice(),
            pcs: pcs.into_boxed_slice(),
        });
        let n = STAT_METHODS.fetch_add(1, Ordering::Relaxed) + 1;
        STAT_OPS.fetch_add(q.ops.len(), Ordering::Relaxed);
        STAT_BYTES.fetch_add(q.heap_bytes(), Ordering::Relaxed);
        STAT_CODE_BYTES.fetch_add(limit, Ordering::Relaxed);
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

    /// Resolve a bytecode pc to a stream index, or `None` when `pc` is not
    /// an instruction start in this stream (the caller must then fall back
    /// to `Instruction::decode`).
    ///
    /// `hint` is checked first and is expected to be `previous_index + 1`:
    /// straight-line execution — the overwhelmingly common case — resolves
    /// in a single compare with no search and no auxiliary pc->index table.
    /// Anything else falls back to a binary search over the pc table, which
    /// is exact because `pcs` is strictly increasing.
    #[inline]
    pub fn index_of_pc(&self, pc: usize, hint: usize) -> Option<usize> {
        if pc > u32::MAX as usize {
            return None;
        }
        let pc32 = pc as u32;
        let n = self.ops.len();
        if hint < n && self.pcs[hint] == pc32 {
            return Some(hint);
        }
        match self.pcs[..n].binary_search(&pc32) {
            Ok(i) => Some(i),
            Err(_) => None,
        }
    }

    /// Heap bytes owned by this stream: the record array, the pc table, and
    /// every interned switch table it points at. Excludes the pinned
    /// bytecode array (shared with the class data, not new footprint).
    pub fn heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<QuickenedCode>()
            + self.ops.len() * std::mem::size_of::<Instruction>()
            + self.pcs.len() * std::mem::size_of::<u32>();
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

/// Is `CRATONVM_QUICKEN_STATS` set? Read once — this gates a diagnostic only.
fn stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CRATONVM_QUICKEN_STATS").is_some())
}

/// Print the running footprint of the quickened streams to stderr.
///
/// This is how the per-method memory cost of quickening is measured: run any
/// workload with `CRATONVM_QUICKEN_STATS=1` and read the last line.
pub fn report_stats() {
    let (methods, ops, bytes, code_bytes, unq) = quicken_stats();
    let per_method = if methods > 0 { bytes / methods } else { 0 };
    let per_insn = if ops > 0 { bytes as f64 / ops as f64 } else { 0.0 };
    let per_code_byte = if code_bytes > 0 {
        bytes as f64 / code_bytes as f64
    } else {
        0.0
    };
    eprintln!(
        "[quicken] methods={methods} insns={ops} stream_bytes={bytes} \
bytecode_bytes={code_bytes} unquickenable={unq} bytes_per_method={per_method} \
bytes_per_insn={per_insn:.1} bytes_per_code_byte={per_code_byte:.2} \
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
    fn wrong_hint_still_resolves_via_binary_search() {
        let code = padded(vec![0x03, 0x3c, 0x1b, 0x10, 0x07, 0x60, 0x3c, 0xb1]);
        let q = QuickenedCode::build(&code).unwrap();
        for i in 0..q.len() {
            let pc = q.pc_of(i);
            // Every possible (including nonsensical) hint must agree.
            for hint in 0..q.len() + 3 {
                assert_eq!(q.index_of_pc(pc, hint), Some(i), "pc={pc} hint={hint}");
            }
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

    #[test]
    fn unquickenable_code_returns_none() {
        // 0xba (invokedynamic) needs 4 operand bytes; truncating mid-operand
        // makes the linear walk fail.
        let code: Arc<[u8]> = Arc::from(vec![0xba, 0x00].into_boxed_slice());
        assert!(QuickenedCode::build(&code).is_none());
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
