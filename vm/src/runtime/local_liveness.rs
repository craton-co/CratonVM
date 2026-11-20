// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-pc liveness of local-variable slots, computed from raw JVM bytecode.
//!
//! Used to filter the interpreter frame GC root scan
//! ([`crate::runtime::frame::Frame::scan_local_objects`]). Without it, every
//! object-typed local slot is a root for the frame's WHOLE lifetime, so a
//! scoped-out local (e.g. a loop construction temp) retains its last referent
//! until the frame pops. For linked structures that is unbounded retention:
//! in the SteadyChurn G1 repro a setup-loop temp retained `node[4095]` — and,
//! through `next` chains, every node appended afterwards — OOMing any heap
//! size while HotSpot runs the same bytecode in 16 MB -- with its loop
//! COMPILED (OSR), whose oop maps apply liveness; HotSpot's interpreter oop
//! maps are type maps, see "The type half" below (gci i10/np). Diagnosed via
//! `CRATONVM_G1_DBG_ROOTCENSUS=1`.
//!
//! Design: a classic backward may-liveness dataflow over the method's
//! bytecode, one bit per local slot, cached per code blob. Everything is
//! CONSERVATIVE: any construct the analyzer does not fully model (`jsr`/
//! `ret`, malformed or truncated bytecode, oversized methods) makes the whole
//! method fall back to "all locals live" — i.e. exactly the previous
//! behaviour. The answer is a [`LiveMask`]: one inline word for slots 0..64
//! and, for a method with more locals, rows of further words in the cached
//! table; slots from `MAX_TRACKED_LOCALS` up are untracked and always live.
//! A slot the analysis reports dead is
//! guaranteed dead under bytecode semantics: every path from the queried pc
//! writes the slot before reading it, so the value can never be observed
//! again (exception edges are modelled as successors from every covered pc
//! to the handler). Slot 0 is kept live as extra insurance for
//! receiver-peeking diagnostic paths (mirrors HotSpot's special case for
//! synchronized-method receivers) -- for a receiver only: a frame known to
//! run a STATIC method asks through [`live_locals_mask_for`] with
//! `keep_slot0 = false` (gci i3/il), since its slot 0 is a parameter.
//!
//! Kill-switch: `CRATONVM_NO_LOCAL_LIVENESS=1` restores the unfiltered scan.
//!
//! # The type half (gci i10/np)
//!
//! HotSpot's INTERPRETER oop maps are type maps, not liveness maps:
//! `OopMapCache::compute_one_oop_map` runs `GenerateOopMap`, a forward type
//! flow, and `frame::oops_interpreted_do` roots every local whose cell is a
//! reference at the bci, read again or not (`MethodLiveness` feeds only the
//! compilers). A parked interpreted frame therefore keeps a scoped-out
//! reference local alive on HotSpot (`-Xint` and a cold frame under the
//! JIT alike; `GciOb5ParkedDeadLocalProbe.java` prints
//! `cleared-while-parked=false` there). The same table therefore also carries,
//! per instruction start, the GenerateOopMap answer as two forward rows (see
//! [`TypedMask`]): `typed` -- on EVERY path from the method entry the slot's
//! last write was an `astore` (or there was none) -- and `unwritten` -- on
//! SOME path the slot was never written. A slot is a reference at the pc iff
//! `typed && (!unwritten || slot is a parameter)`; the merge of a reference
//! with a primitive or with an uninitialised slot is NOT one, exactly as
//! GenerateOopMap's conflict / `uninit` merge. `Frame::scan_local_objects`
//! roots a liveness-dead slot that answers yes (default ON;
//! `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS=0` restores liveness only).

use std::sync::{Arc, Mutex, OnceLock, Weak};

// PERF (2026-07-15, round 2 of the RequestMappingMessageConversionIntegrationTests
// bootstrap-slowness investigation): `live_locals_mask` runs on the per-native-call
// GC root-snapshot path (`update_root_snapshot` -> `scan_frame_roots` ->
// `scan_local_objects` -> here), so its two HashMap lookups (the code-blob cache
// and the per-pc liveness table) are paid on essentially every native call. Both
// used `std::collections::HashMap`'s default `RandomState` (SipHash-1-3) hasher —
// the DoS-resistant hasher meant for untrusted external input, not an internal
// lookup table keyed by small integers/pointers. Every other hot-path HashMap in
// this codebase already uses `FxHashMap` for exactly this reason (see
// `native-api/src/registry.rs`, `classloading/src/fx_hash.rs`, and the
// `41cc90ef` "HashMap native-dispatch overhead" fix this mirrors). Swapping the
// hasher is a pure, behavior-preserving perf change: same keys, same values,
// same collision-correctness contract, just a cheaper hash function.
use rustc_hash::FxHashMap as HashMap;

use cratonvm_reader::attribute::ExceptionTableEntry;

/// All-slots-live low word (the conservative fallback).
pub const ALL_LIVE: u64 = u64::MAX;

/// Slots `0..MAX_TRACKED_LOCALS` are tracked. A method with MORE locals is
/// still analysed: its slots at and beyond this index are *untracked* -- their
/// loads and stores contribute nothing to the dataflow and
/// [`LiveMask::is_live`] answers `true` for them. Liveness is per-slot
/// independent, so leaving some slots out cannot change the answer for the
/// others.
///
/// History: until 2026-09-23 (wave 2) a method with more than 64 locals fell
/// back to all-live wholesale; wave 2 analysed its first 64 slots; wave 3
/// widened the mask so up to this many are tracked. The bound is a memory
/// bound: the table holds `ceil(slots / 64)` words per instruction, so at
/// [`MAX_CODE_LEN`] the worst case is 8 words x 64 Ki instructions = 4 MiB.
const MAX_TRACKED_LOCALS: u32 = 512;
/// Every method a class file can hold: `code_length` is below 65536
/// (JVMS 4.7.3), plus the two zero bytes `frame::padded_bytecode` appends.
/// Until wave 4 this was 32 KiB, because the fixpoint was a whole-method
/// re-sweep; the worklist in [`analyze`] removed that cost, so the generated
/// parser / big-switch methods above 32 KiB now get liveness too. A longer
/// blob (not a class-file method) still falls back to all-live.
const MAX_CODE_LEN: usize = u16::MAX as usize + 2;

/// Live-in bitsets per instruction start. `None` table = fall back.
struct LivenessTable {
    /// Instruction-start pc -> row index into `rows`.
    row_of: HashMap<u32, u32>,
    /// Words per row: `1` for a method whose locals all fit in slots 0..64.
    words: usize,
    /// `row_of.len() * words` words; row `r` covers slots `64 * k + bit` in
    /// word `r * words + k`.
    rows: Box<[u64]>,
    /// gci i10/np, the type half (module doc): per instruction start, in
    /// `rows`' layout, the slots whose last write on EVERY path from the
    /// entry was an `astore` or that no path wrote. Zero for an instruction
    /// the entry does not reach.
    typed: Box<[u64]>,
    /// The slots SOME path from the entry reaches the instruction without
    /// writing (they still hold the entry value: a parameter, or nothing).
    unwritten: Box<[u64]>,
}

/// The live-local set of one frame at its current pc(s): bit `i` = slot `i`
/// may still be read. Ask it with [`Self::is_live`].
///
/// Slots 0..64 are an inline word, so the common method costs nothing beyond
/// what the old bare `u64` did. For a method with more locals the remaining
/// words stay in the cached table and are read in place (one `Arc` clone per
/// query, no allocation). A slot beyond the tracked range, or any slot of a
/// method that could not be analysed, is live.
#[derive(Clone)]
pub struct LiveMask {
    low: u64,
    high: Option<HighRows>,
}

#[derive(Clone)]
struct HighRows {
    table: Arc<LivenessTable>,
    /// The (at most two, possibly equal) rows whose union this mask is.
    rows: [u32; 2],
}

impl LiveMask {
    /// Every slot live: the conservative answer, and the kill-switch answer.
    pub fn all_live() -> Self {
        Self {
            low: ALL_LIVE,
            high: None,
        }
    }

    /// May local slot `slot` still be read?
    #[inline]
    pub fn is_live(&self, slot: usize) -> bool {
        if slot < 64 {
            return self.low & (1u64 << slot) != 0;
        }
        let Some(high) = &self.high else {
            return true;
        };
        let words = high.table.words;
        let word = slot / 64;
        if word >= words {
            return true;
        }
        let [r0, r1] = high.rows;
        let bits = high.table.rows[r0 as usize * words + word]
            | high.table.rows[r1 as usize * words + word];
        bits & (1u64 << (slot % 64)) != 0
    }

    /// Slots 0..64 as a bit word (tests and diagnostics).
    pub fn low_word(&self) -> u64 {
        self.low
    }
}

impl std::fmt::Debug for LiveMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveMask")
            .field("low", &format_args!("{:#x}", self.low))
            .field("high_words", &self.high.as_ref().map(|h| h.table.words - 1))
            .finish()
    }
}

/// Cache keyed by the code blob's allocation identity. The `Weak` keeps the
/// `Arc<[u8]>` CONTROL BLOCK alive, which prevents the allocator from
/// re-issuing the same address to a different method's code while the entry
/// exists — `upgrade()` + pointer equality then makes stale-key reuse
/// impossible.
type CacheKey = (usize, usize);
type CacheVal = (Weak<[u8]>, Option<Arc<LivenessTable>>);

fn cache() -> &'static Mutex<HashMap<CacheKey, CacheVal>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheVal>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::default()))
}

/// Bound the cache; on overflow drop everything (entries are cheap to
/// recompute lazily and this only triggers under extreme class churn).
const CACHE_CAP: usize = 8192;

/// Return the union of live-local sets at the given pcs, or
/// [`LiveMask::all_live`] when the method cannot be analyzed. Querying a pc
/// that is not a known instruction start yields all-live (never
/// under-approximates). `_max_locals` is kept in the signature; the analysis
/// sizes its bitsets from the slots the bytecode actually names.
pub fn live_locals_mask(
    code: &Arc<[u8]>,
    exception_table: &[ExceptionTableEntry],
    max_locals: u16,
    query_pcs: [usize; 2],
) -> LiveMask {
    live_locals_mask_for(code, exception_table, max_locals, query_pcs, true)
}

/// [`live_locals_mask`] with the slot-0 insurance stated by the caller:
/// `keep_slot0` is `true` for an instance method (slot 0 is its receiver,
/// kept live whatever the bytecode says; see the module doc) and for any frame
/// whose static-ness is not known, and `false` only for a method KNOWN to be
/// static, whose slot 0 is its first parameter and gets the same per-pc
/// liveness as every other slot.
///
/// gci i3/il (`gci-i2sp-argsp-spliced-and-static-cases-are-held-by-an-interpreted-caller-chain`,
/// holder 3): forcing bit 0 for a static method kept an interpreted static
/// caller's dead first reference argument (`relay(a)` suspended in its
/// `invokestatic`) as a root for the rest of the call. No receiver-peeking
/// path reads slot 0 of a static frame: a static `synchronized` method locks
/// the class mirror (`Frame::monitor_on_exit`), not slot 0, and every other
/// dead slot of every frame is already filtered the same way.
pub fn live_locals_mask_for(
    code: &Arc<[u8]>,
    exception_table: &[ExceptionTableEntry],
    _max_locals: u16,
    query_pcs: [usize; 2],
    keep_slot0: bool,
) -> LiveMask {
    if code.len() > MAX_CODE_LEN {
        return LiveMask::all_live();
    }

    let way = memo_way(code);
    if let Some(mask) = memo_lookup(way, code, query_pcs, keep_slot0) {
        return mask;
    }

    let table = global_table(code, exception_table);
    memo_fill(way, code, table.clone());
    mask_from(table.as_ref(), query_pcs, keep_slot0)
}

/// The union of `table`'s live-in sets at `query_pcs`, with slot 0 forced
/// live when `keep_slot0` (see [`live_locals_mask_for`]). `None` (an
/// unanalyzable method) and a pc that is not an instruction start both answer
/// all-live.
#[inline]
fn mask_from(
    table: Option<&Arc<LivenessTable>>,
    query_pcs: [usize; 2],
    keep_slot0: bool,
) -> LiveMask {
    let Some(table) = table else {
        return LiveMask::all_live();
    };
    let (Some(&r0), Some(&r1)) = (
        table.row_of.get(&(query_pcs[0] as u32)),
        table.row_of.get(&(query_pcs[1] as u32)),
    ) else {
        return LiveMask::all_live();
    };
    let words = table.words;
    // Slot 0 stays live for a receiver (insurance; see module doc), and for a
    // frame not known to be static.
    let low = table.rows[r0 as usize * words]
        | table.rows[r1 as usize * words]
        | u64::from(keep_slot0);
    let high = (words > 1).then(|| HighRows {
        table: Arc::clone(table),
        rows: [r0, r1],
    });
    LiveMask { low, high }
}

// ── Per-OS-thread memo in front of the global cache ─────────────────────────
//
// `live_locals_mask` is asked once per frame per root scan, and root scans are
// not rare: every safepoint snapshot and every blocking native call walks the
// thread's whole stack. Answering each of those through `cache()` took the one
// process-wide `Mutex` per frame, so every thread's stack walk serialised on
// every other thread's, and a deep stack paid a lock round trip per frame even
// uncontended.
//
// A frame's method changes slowly relative to how often it is scanned, so a
// small direct-mapped table per thread answers almost every query with a
// pointer compare. Each entry holds a STRONG `Arc<[u8]>` of the code blob it
// describes: while the entry exists no other allocation can occupy that
// address, so `Arc::ptr_eq` is exact identity -- the same guarantee the global
// cache gets from its `Weak` + `upgrade()` pair, without the upgrade's atomic
// round trip. The cost is that a thread keeps up to `TLS_MEMO_WAYS` code blobs
// alive after their methods are gone, which is bounded and small.
//
// The table is a pure function of the code blob (see `frame.rs`'s
// padded-bytecode memo for why distinct methods never share one), so the memo
// can never disagree with the global cache.

/// Entries per thread. A power of two so the index is a mask.
const TLS_MEMO_WAYS: usize = 64;

struct MemoEntry {
    code: Arc<[u8]>,
    table: Option<Arc<LivenessTable>>,
}

thread_local! {
    static TLS_MEMO: std::cell::RefCell<[Option<MemoEntry>; TLS_MEMO_WAYS]> =
        const { std::cell::RefCell::new([const { None }; TLS_MEMO_WAYS]) };
}

/// Direct-mapped slot for `code`. Code blobs are heap allocations at least
/// 8-aligned, so the low three address bits carry nothing; fold in higher
/// bits so blobs from one arena chunk spread over the table.
#[inline]
fn memo_way(code: &Arc<[u8]>) -> usize {
    let p = Arc::as_ptr(code) as *const u8 as usize;
    ((p >> 3) ^ (p >> 11)) & (TLS_MEMO_WAYS - 1)
}

/// The mask for `code` from this thread's memo, or `None` on a miss (or while
/// the thread-local is being torn down).
#[inline]
fn memo_lookup(
    way: usize,
    code: &Arc<[u8]>,
    query_pcs: [usize; 2],
    keep_slot0: bool,
) -> Option<LiveMask> {
    TLS_MEMO
        .try_with(|m| {
            let m = m.try_borrow().ok()?;
            let e = m[way].as_ref()?;
            if !Arc::ptr_eq(&e.code, code) {
                return None;
            }
            Some(mask_from(e.table.as_ref(), query_pcs, keep_slot0))
        })
        .ok()
        .flatten()
}

#[inline]
fn memo_fill(way: usize, code: &Arc<[u8]>, table: Option<Arc<LivenessTable>>) {
    let _ = TLS_MEMO.try_with(|m| {
        if let Ok(mut m) = m.try_borrow_mut() {
            m[way] = Some(MemoEntry {
                code: Arc::clone(code),
                table,
            });
        }
    });
}

/// The table for `code` from this thread's memo: `None` on a miss, `Some(None)`
/// for a memoised unanalysable method.
#[inline]
fn memo_table(way: usize, code: &Arc<[u8]>) -> Option<Option<Arc<LivenessTable>>> {
    TLS_MEMO
        .try_with(|m| {
            let m = m.try_borrow().ok()?;
            let e = m[way].as_ref()?;
            if !Arc::ptr_eq(&e.code, code) {
                return None;
            }
            Some(e.table.clone())
        })
        .ok()
        .flatten()
}

// ── The type half (gci i10/np; module doc "The type half") ──────────────────

/// The reference-typed local set of one frame at its current pc(s), in the
/// sense of HotSpot's interpreter oop map (`GenerateOopMap`): bit `i` = slot
/// `i` holds a reference on every path to the pc. Ask it with
/// [`Self::is_ref`]. Built by [`typed_locals_mask_for`].
#[derive(Clone)]
pub struct TypedMask {
    table: Arc<LivenessTable>,
    /// The (at most two, possibly equal) rows whose union this mask is.
    rows: [u32; 2],
    /// Slots `0..param_slots` are the method's parameters (receiver
    /// included): an unwritten one still holds its entry value.
    param_slots: usize,
}

impl TypedMask {
    /// Is local slot `slot` reference-typed at either queried pc?
    ///
    /// A slot past the rows the table holds answers `true`: the analysis does
    /// not track it, and the liveness half calls it live anyway.
    #[inline]
    pub fn is_ref(&self, slot: usize) -> bool {
        let words = self.table.words;
        let word = slot / 64;
        if word >= words {
            return true;
        }
        let bit = 1u64 << (slot % 64);
        let is_param = slot < self.param_slots;
        self.rows.iter().any(|&r| {
            let at = r as usize * words + word;
            let typed = self.table.typed[at] & bit != 0;
            let unwritten = self.table.unwritten[at] & bit != 0;
            typed && (!unwritten || is_param)
        })
    }
}

impl std::fmt::Debug for TypedMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedMask")
            .field("rows", &self.rows)
            .field("param_slots", &self.param_slots)
            .finish()
    }
}

/// The reference-typed locals at `query_pcs` (their union), or `None` when the
/// method cannot be analysed or a pc is not an instruction start -- the caller
/// then keeps the liveness answer alone. `param_slots` is
/// [`entry_param_slots`] of the frame's method. Shares the liveness table and
/// both its caches: one analysis per code blob answers both questions.
pub fn typed_locals_mask_for(
    code: &Arc<[u8]>,
    exception_table: &[ExceptionTableEntry],
    query_pcs: [usize; 2],
    param_slots: usize,
) -> Option<TypedMask> {
    if code.len() > MAX_CODE_LEN {
        return None;
    }
    let way = memo_way(code);
    let table = match memo_table(way, code) {
        Some(table) => table,
        None => {
            let table = global_table(code, exception_table);
            memo_fill(way, code, table.clone());
            table
        }
    }?;
    let r0 = *table.row_of.get(&(query_pcs[0] as u32))?;
    let r1 = *table.row_of.get(&(query_pcs[1] as u32))?;
    Some(TypedMask {
        table,
        rows: [r0, r1],
        param_slots,
    })
}

/// How many local slots a method's parameters occupy at entry: one for the
/// receiver when `has_receiver`, two per `long` / `double`, one per other
/// parameter. A descriptor that does not parse answers `usize::MAX` ("any
/// unwritten slot may be a parameter"): the scan still roots only a slot
/// whose runtime value is a reference, and an unwritten non-parameter slot
/// holds none.
pub fn entry_param_slots(descriptor: &str, has_receiver: bool) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return usize::MAX;
    }
    let mut slots = usize::from(has_receiver);
    let mut i = 1usize;
    // Index just past the `;` of the class name starting at `from`.
    let past_class = |from: usize| {
        bytes[from..]
            .iter()
            .position(|&b| b == b';')
            .map(|p| from + p + 1)
    };
    while let Some(&c) = bytes.get(i) {
        match c {
            b')' => return slots,
            b'J' | b'D' => {
                slots += 2;
                i += 1;
            }
            b'L' => {
                slots += 1;
                match past_class(i) {
                    Some(next) => i = next,
                    None => return usize::MAX,
                }
            }
            b'[' => {
                slots += 1;
                while bytes.get(i) == Some(&b'[') {
                    i += 1;
                }
                match bytes.get(i).copied() {
                    Some(b'L') => match past_class(i) {
                        Some(next) => i = next,
                        None => return usize::MAX,
                    },
                    Some(_) => i += 1,
                    None => return usize::MAX,
                }
            }
            _ => {
                slots += 1;
                i += 1;
            }
        }
    }
    usize::MAX
}

/// Does the instruction at `pc` store a REFERENCE into a local (`astore`,
/// `astore_<n>`, `wide astore`)? Every other store (and `iinc`, which the
/// verifier admits only on an `int` slot) leaves a non-reference.
fn stores_reference(code: &[u8], pc: usize) -> bool {
    match code.get(pc).copied() {
        Some(0x3a) | Some(0x4b..=0x4e) => true,
        Some(0xc4) => code.get(pc + 1).copied() == Some(0x3a),
        _ => false,
    }
}

/// The type half's forward fixpoint (module doc). `succs` are the liveness
/// pass's successor lists (normal and exception edges); `handlers[i]` the
/// exception handlers covering instruction `i`, which also receive its
/// PRE-store state (an exception raised by `i` leaves before `i` writes).
/// Returns `(typed, unwritten)` in the liveness table's row layout.
fn type_rows(
    code: &[u8],
    instrs: &[(usize, Instr)],
    succs: &[Vec<usize>],
    handlers: &[Vec<usize>],
    words: usize,
) -> (Vec<u64>, Vec<u64>) {
    let n = instrs.len();
    // `typed` is a must-row (meet = AND, top = all ones); `unwritten` a
    // may-row (join = OR, bottom = zero). Only instructions the entry reaches
    // are ever visited, so the optimistic top never leaks.
    let mut typed: Vec<u64> = vec![u64::MAX; n * words];
    let mut unwritten: Vec<u64> = vec![0; n * words];
    let mut reached: Vec<bool> = vec![false; n];
    if n == 0 {
        return (typed, unwritten);
    }
    // The entry: nothing written yet, every slot "typed" by the unwritten
    // rule (whether it holds a reference is the parameter test's question).
    unwritten[..words].fill(u64::MAX);
    reached[0] = true;
    let mut worklist: Vec<u32> = vec![0];
    let mut queued: Vec<bool> = vec![false; n];
    queued[0] = true;
    let mut out_t: Vec<u64> = vec![0; words];
    let mut out_u: Vec<u64> = vec![0; words];
    while let Some(i) = worklist.pop() {
        let i = i as usize;
        queued[i] = false;
        let (pc, instr) = &instrs[i];
        out_t.copy_from_slice(&typed[i * words..(i + 1) * words]);
        out_u.copy_from_slice(&unwritten[i * words..(i + 1) * words]);
        if instr.defs.count > 0 {
            instr.defs.clear_in(&mut out_t);
            if stores_reference(code, *pc) {
                instr.defs.set_in(&mut out_t);
            }
            instr.defs.clear_in(&mut out_u);
        }
        // Each edge: meet `typed`, join `unwritten`; queue on change.
        let mut flow = |s: usize,
                        from_t: &[u64],
                        from_u: &[u64],
                        typed: &mut [u64],
                        unwritten: &mut [u64],
                        reached: &mut [bool]| {
            let mut changed = !reached[s];
            reached[s] = true;
            let row_t = &mut typed[s * words..(s + 1) * words];
            let row_u = &mut unwritten[s * words..(s + 1) * words];
            for (((t, u), ft), fu) in row_t
                .iter_mut()
                .zip(row_u.iter_mut())
                .zip(from_t)
                .zip(from_u)
            {
                let nt = *t & *ft;
                let nu = *u | *fu;
                if nt != *t || nu != *u {
                    *t = nt;
                    *u = nu;
                    changed = true;
                }
            }
            if changed && !queued[s] {
                queued[s] = true;
                worklist.push(s as u32);
            }
        };
        for &s in &succs[i] {
            flow(
                s,
                &out_t[..],
                &out_u[..],
                &mut typed[..],
                &mut unwritten[..],
                &mut reached[..],
            );
        }
        if !handlers[i].is_empty() {
            let in_t: Vec<u64> = typed[i * words..(i + 1) * words].to_vec();
            let in_u: Vec<u64> = unwritten[i * words..(i + 1) * words].to_vec();
            for &h in &handlers[i] {
                flow(
                    h,
                    &in_t[..],
                    &in_u[..],
                    &mut typed[..],
                    &mut unwritten[..],
                    &mut reached[..],
                );
            }
        }
    }
    for (i, r) in reached.iter().enumerate() {
        if !r {
            typed[i * words..(i + 1) * words].fill(0);
        }
    }
    (typed, unwritten)
}

/// The process-wide cache: look `code` up, analysing it on a miss.
fn global_table(
    code: &Arc<[u8]>,
    exception_table: &[ExceptionTableEntry],
) -> Option<Arc<LivenessTable>> {
    let key: CacheKey = (Arc::as_ptr(code) as *const u8 as usize, code.len());
    {
        let cache = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((weak, tbl)) = cache.get(&key) {
            if weak
                .upgrade()
                .map(|live| Arc::ptr_eq(&live, code))
                .unwrap_or(false)
            {
                return tbl.clone();
            }
        }
    }
    // Analyse with the process-wide lock RELEASED: a 64 KiB method takes a
    // while, and every other thread's root scan that misses its memo would
    // otherwise wait on it. Two threads missing the same blob both analyse it
    // and the second insert wins; the tables are identical (a pure function of
    // the blob, which `code` keeps alive).
    let tbl = analyze(code, exception_table).map(Arc::new);
    let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, (Arc::downgrade(code), tbl.clone()));
    tbl
}

/// One decoded instruction's dataflow facts.
struct Instr {
    /// Byte length (next sequential pc = pc + len).
    len: usize,
    /// Local slots read.
    uses: Slots,
    /// Local slots written (killed).
    defs: Slots,
    /// Explicit branch targets (absolute pcs).
    targets: Vec<usize>,
    /// Whether control can fall through to `pc + len`.
    falls_through: bool,
}

/// The local slots one instruction reads or writes: `count` (0, 1, or 2 for a
/// long/double) consecutive slots from `first`.
#[derive(Clone, Copy)]
struct Slots {
    first: u32,
    count: u8,
}

impl Slots {
    const NONE: Slots = Slots { first: 0, count: 0 };

    /// `idx` alone (never `None`: the `Option` keeps the decoder's `?` shape).
    fn one(idx: usize) -> Option<Slots> {
        Some(Slots {
            first: u32::try_from(idx).ok()?,
            count: 1,
        })
    }

    /// `idx` and `idx + 1` (a long/double).
    fn two(idx: usize) -> Option<Slots> {
        Some(Slots {
            first: u32::try_from(idx).ok()?,
            count: 2,
        })
    }

    fn end(self) -> u32 {
        self.first + u32::from(self.count)
    }

    /// Set this range's bits in `row`; slots past the row are untracked.
    fn set_in(self, row: &mut [u64]) {
        for s in self.first..self.end() {
            if let Some(word) = row.get_mut((s / 64) as usize) {
                *word |= 1u64 << (s % 64);
            }
        }
    }

    /// Clear this range's bits in `row`; slots past the row are untracked.
    fn clear_in(self, row: &mut [u64]) {
        for s in self.first..self.end() {
            if let Some(word) = row.get_mut((s / 64) as usize) {
                *word &= !(1u64 << (s % 64));
            }
        }
    }
}

fn read_u16(code: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
}

fn read_i16(code: &[u8], at: usize) -> Option<i16> {
    read_u16(code, at).map(|v| v as i16)
}

fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes([
        *code.get(at)?,
        *code.get(at + 1)?,
        *code.get(at + 2)?,
        *code.get(at + 3)?,
    ]))
}

fn branch16(code: &[u8], pc: usize) -> Option<usize> {
    let off = read_i16(code, pc + 1)? as isize;
    usize::try_from(pc as isize + off).ok()
}

/// Decode the instruction at `pc`. `None` = unanalyzable (caller falls back
/// to all-live for the whole method).
#[allow(clippy::too_many_lines)]
fn decode(code: &[u8], pc: usize) -> Option<Instr> {
    let op = *code.get(pc)?;
    let simple = |len: usize| {
        Some(Instr {
            len,
            uses: Slots::NONE,
            defs: Slots::NONE,
            targets: Vec::new(),
            falls_through: true,
        })
    };
    match op {
        // ── loads/stores with inline index byte ────────────────────────
        0x15..=0x19 => {
            // iload/lload/fload/dload/aload idx
            let idx = *code.get(pc + 1)? as usize;
            let mask = if op == 0x16 || op == 0x18 {
                Slots::two(idx)?
            } else {
                Slots::one(idx)?
            };
            Some(Instr {
                len: 2,
                uses: mask,
                defs: Slots::NONE,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        0x36..=0x3a => {
            // istore/lstore/fstore/dstore/astore idx
            let idx = *code.get(pc + 1)? as usize;
            let mask = if op == 0x37 || op == 0x39 {
                Slots::two(idx)?
            } else {
                Slots::one(idx)?
            };
            Some(Instr {
                len: 2,
                uses: Slots::NONE,
                defs: mask,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── shorthand loads: iload_0..aload_3 (0x1a..=0x2d) ─────────────
        0x1a..=0x2d => {
            let rel = (op - 0x1a) as usize;
            let (kind, idx) = (rel / 4, rel % 4);
            // kind: 0=iload 1=lload 2=fload 3=dload 4=aload
            let mask = if kind == 1 || kind == 3 {
                Slots::two(idx)?
            } else {
                Slots::one(idx)?
            };
            Some(Instr {
                len: 1,
                uses: mask,
                defs: Slots::NONE,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── shorthand stores: istore_0..astore_3 (0x3b..=0x4e) ──────────
        0x3b..=0x4e => {
            let rel = (op - 0x3b) as usize;
            let (kind, idx) = (rel / 4, rel % 4);
            let mask = if kind == 1 || kind == 3 {
                Slots::two(idx)?
            } else {
                Slots::one(idx)?
            };
            Some(Instr {
                len: 1,
                uses: Slots::NONE,
                defs: mask,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── iinc idx const: read+write (model as use so it never kills) ─
        0x84 => {
            let idx = *code.get(pc + 1)? as usize;
            let mask = Slots::one(idx)?;
            Some(Instr {
                len: 3,
                uses: mask,
                defs: Slots::NONE,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── conditional branches: 2-byte offset, falls through ──────────
        0x99..=0xa6 | 0xc6 | 0xc7 => Some(Instr {
            len: 3,
            uses: Slots::NONE,
            defs: Slots::NONE,
            targets: vec![branch16(code, pc)?],
            falls_through: true,
        }),
        // ── goto ────────────────────────────────────────────────────────
        0xa7 => Some(Instr {
            len: 3,
            uses: Slots::NONE,
            defs: Slots::NONE,
            targets: vec![branch16(code, pc)?],
            falls_through: false,
        }),
        // goto_w
        0xc8 => {
            let off = read_i32(code, pc + 1)? as isize;
            Some(Instr {
                len: 5,
                uses: Slots::NONE,
                defs: Slots::NONE,
                targets: vec![usize::try_from(pc as isize + off).ok()?],
                falls_through: false,
            })
        }
        // ── jsr / jsr_w / ret: subroutines — refuse to analyze ──────────
        0xa8 | 0xc9 | 0xa9 => None,
        // ── tableswitch ─────────────────────────────────────────────────
        0xaa => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let base = pc + 1 + pad;
            let default = read_i32(code, base)? as isize;
            let lo = read_i32(code, base + 4)? as i64;
            let hi = read_i32(code, base + 8)? as i64;
            if hi < lo || (hi - lo) as usize > code.len() {
                return None;
            }
            let n = (hi - lo + 1) as usize;
            let mut targets = Vec::with_capacity(n + 1);
            targets.push(usize::try_from(pc as isize + default).ok()?);
            for k in 0..n {
                let off = read_i32(code, base + 12 + 4 * k)? as isize;
                targets.push(usize::try_from(pc as isize + off).ok()?);
            }
            Some(Instr {
                len: base + 12 + 4 * n - pc,
                uses: Slots::NONE,
                defs: Slots::NONE,
                targets,
                falls_through: false,
            })
        }
        // ── lookupswitch ────────────────────────────────────────────────
        0xab => {
            let pad = (4 - ((pc + 1) % 4)) % 4;
            let base = pc + 1 + pad;
            let default = read_i32(code, base)? as isize;
            let npairs = read_i32(code, base + 4)?;
            if npairs < 0 || npairs as usize > code.len() {
                return None;
            }
            let n = npairs as usize;
            let mut targets = Vec::with_capacity(n + 1);
            targets.push(usize::try_from(pc as isize + default).ok()?);
            for k in 0..n {
                let off = read_i32(code, base + 8 + 8 * k + 4)? as isize;
                targets.push(usize::try_from(pc as isize + off).ok()?);
            }
            Some(Instr {
                len: base + 8 + 8 * n - pc,
                uses: Slots::NONE,
                defs: Slots::NONE,
                targets,
                falls_through: false,
            })
        }
        // ── returns / athrow: no successors (exception edges are added
        //    separately from the exception table) ─────────────────────────
        0xac..=0xb1 | 0xbf => Some(Instr {
            len: 1,
            uses: Slots::NONE,
            defs: Slots::NONE,
            targets: Vec::new(),
            falls_through: false,
        }),
        // ── wide ────────────────────────────────────────────────────────
        0xc4 => {
            let wop = *code.get(pc + 1)?;
            let idx = read_u16(code, pc + 2)? as usize;
            match wop {
                0x15..=0x19 => {
                    let mask = if wop == 0x16 || wop == 0x18 {
                        Slots::two(idx)?
                    } else {
                        Slots::one(idx)?
                    };
                    Some(Instr {
                        len: 4,
                        uses: mask,
                        defs: Slots::NONE,
                        targets: Vec::new(),
                        falls_through: true,
                    })
                }
                0x36..=0x3a => {
                    let mask = if wop == 0x37 || wop == 0x39 {
                        Slots::two(idx)?
                    } else {
                        Slots::one(idx)?
                    };
                    Some(Instr {
                        len: 4,
                        uses: Slots::NONE,
                        defs: mask,
                        targets: Vec::new(),
                        falls_through: true,
                    })
                }
                0x84 => {
                    let mask = Slots::one(idx)?;
                    Some(Instr {
                        len: 6,
                        uses: mask,
                        defs: Slots::NONE,
                        targets: Vec::new(),
                        falls_through: true,
                    })
                }
                0xa9 => None, // wide ret
                _ => None,
            }
        }
        // ── fixed-length opcodes with no local/control effects ──────────
        0x00..=0x0f => simple(1), // nop, consts
        0x10 => simple(2),        // bipush
        0x11 => simple(3),        // sipush
        0x12 => simple(2),        // ldc
        0x13 | 0x14 => simple(3), // ldc_w, ldc2_w
        0x2e..=0x35 => simple(1), // array loads
        0x4f..=0x56 => simple(1), // array stores
        0x57..=0x5f => simple(1), // pop..swap
        0x60..=0x83 => simple(1), // arithmetic
        0x85..=0x93 => simple(1), // conversions
        0x94..=0x98 => simple(1), // comparisons
        0xb2..=0xb5 => simple(3), // get/putstatic, get/putfield
        0xb6..=0xb8 => simple(3), // invokevirtual/special/static
        0xb9 | 0xba => simple(5), // invokeinterface, invokedynamic
        0xbb => simple(3),        // new
        0xbc => simple(2),        // newarray
        0xbd => simple(3),        // anewarray
        0xbe => simple(1),        // arraylength
        0xc0 | 0xc1 => simple(3), // checkcast, instanceof
        0xc2 | 0xc3 => simple(1), // monitorenter/exit
        0xc5 => simple(4),        // multianewarray
        _ => None,                // breakpoint/impdep/unknown
    }
}

/// Build the per-pc live-in table, or `None` if the method is unanalyzable.
fn analyze(code: &[u8], exception_table: &[ExceptionTableEntry]) -> Option<LivenessTable> {
    // Pass 1: decode every instruction reachable by linear sweep. The
    // trailing `padded_bytecode` zero bytes decode as `nop` and are harmless.
    let mut instrs: Vec<(usize, Instr)> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let instr = decode(code, pc)?;
        let next = pc + instr.len;
        if instr.len == 0 || next > code.len() {
            return None;
        }
        instrs.push((pc, instr));
        pc = next;
    }

    // addr-keyed: bytecode offsets, not object addresses.
    let index_of: HashMap<usize, usize> = instrs
        .iter()
        .enumerate()
        .map(|(i, (pc, _))| (*pc, i))
        .collect();

    // Successor lists as instruction indices; a branch target that is not an
    // instruction start means mis-decoded code — bail.
    let n = instrs.len();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (pc, instr)) in instrs.iter().enumerate() {
        if instr.falls_through {
            let next = pc + instr.len;
            if next < code.len() {
                succs[i].push(*index_of.get(&next)?);
            }
        }
        for t in &instr.targets {
            succs[i].push(*index_of.get(t)?);
        }
    }
    // Exception edges: every instruction whose pc lies in [start, end) may
    // transfer to the handler. `instrs` is sorted by pc, so the covered run is
    // found by binary search rather than a scan of the whole method per entry.
    // gci i10/np: the same edges, kept apart for the type half (`type_rows`).
    let mut handlers: Vec<Vec<usize>> = vec![Vec::new(); n];
    for entry in exception_table {
        let (start, end, handler) = (
            entry.start_pc as usize,
            entry.end_pc as usize,
            entry.handler_pc as usize,
        );
        let Some(&h) = index_of.get(&handler) else {
            return None;
        };
        let first = instrs.partition_point(|(pc, _)| *pc < start);
        let last = instrs.partition_point(|(pc, _)| *pc < end);
        for succ in succs.iter_mut().take(last).skip(first) {
            succ.push(h);
        }
        for covered in handlers.iter_mut().take(last).skip(first) {
            covered.push(h);
        }
    }

    // Row width: enough words for the highest slot the bytecode names, capped
    // at `MAX_TRACKED_LOCALS` (slots above are untracked, i.e. always live).
    let slot_end = instrs
        .iter()
        .map(|(_, instr)| instr.uses.end().max(instr.defs.end()))
        .max()
        .unwrap_or(0)
        .min(MAX_TRACKED_LOCALS);
    let words = (slot_end.div_ceil(64) as usize).max(1);

    // Pass 2: backward may-liveness fixpoint, `live_in = uses | (out & !defs)`
    // over `words`-wide rows, driven by a worklist: an instruction is
    // re-evaluated only when a successor's row changed. Rows only grow and are
    // bounded by `words * 64` bits, so each instruction changes at most that
    // many times and the work is O(edges x words x changes), independent of
    // loop nesting. (Until wave 4 this re-swept the whole method until a sweep
    // changed nothing — one extra full pass per level of back-edge nesting —
    // which is why `MAX_CODE_LEN` was 32 KiB.)
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, ss) in succs.iter().enumerate() {
        for &s in ss {
            // `n` instructions of at least one byte each fit a `u32` because
            // `code.len() <= MAX_CODE_LEN`.
            preds[s].push(i as u32);
        }
    }
    let mut live_in: Vec<u64> = vec![0; n * words];
    let mut scratch: Vec<u64> = vec![0; words];
    // Seeded so the first pops run from the end of the method backwards, the
    // order the old sweep used, which settles straight-line code in one visit.
    let mut worklist: Vec<u32> = (0..n as u32).collect();
    let mut queued: Vec<bool> = vec![true; n];
    while let Some(i) = worklist.pop() {
        let i = i as usize;
        queued[i] = false;
        scratch.fill(0);
        for &s in &succs[i] {
            for (out, v) in scratch.iter_mut().zip(&live_in[s * words..(s + 1) * words]) {
                *out |= *v;
            }
        }
        let (_, instr) = &instrs[i];
        instr.defs.clear_in(&mut scratch);
        instr.uses.set_in(&mut scratch);
        let row = &mut live_in[i * words..(i + 1) * words];
        if *row != *scratch {
            row.copy_from_slice(&scratch);
            for &p in &preds[i] {
                if !queued[p as usize] {
                    queued[p as usize] = true;
                    worklist.push(p);
                }
            }
        }
    }

    // Pass 3 (gci i10/np): the type half, a forward fixpoint over the same
    // edges and row width.
    let (typed, unwritten) = type_rows(code, &instrs, &succs, &handlers, words);

    Some(LivenessTable {
        row_of: instrs
            .iter()
            .enumerate()
            .map(|(i, (pc, _))| (*pc as u32, i as u32))
            .collect(),
        words,
        rows: live_in.into_boxed_slice(),
        typed: typed.into_boxed_slice(),
        unwritten: unwritten.into_boxed_slice(),
    })
}

/// Stage 1 of
/// `docs/feature-designs/precise-jit-maps-default.md`:
/// compare each interpreter frame's heuristic root set (runtime tags, kind
/// marks, lost-tag probes, liveness) against the verifier's type map
/// (`cratonvm_classloading::type_maps`) intersected with liveness — the
/// HotSpot interpreter oop map — and report the differences.
///
/// Armed by `CRATONVM_DBG_VERIFY_OOP_MAPS` (the JIT's oop-map oracle gate; the
/// question is the same one, asked of interpreter frames). The map is the VM's
/// own `TypeMapStore`, handed to `Frame::scan_local_objects_mapped` and its
/// remap twin `Frame::update_frame_refs_mapped` by the per-thread root walks
/// that hold the VM; a scan without the VM is not compared. Off, the cost is
/// one cached load per such scan or remap. On, every scan re-asks the scan one
/// slot at a time, every remap snapshots the frame first, and both read the
/// type map, so it is slow.
///
/// Verdicts per slot (see [`classify`]):
/// * `Missed` — the map says a LIVE reference and the slot holds a non-zero
///   value, but the heuristic scan did not root it: a latent use-after-free.
/// * `Extra` — the heuristic rooted a slot every mapped pc calls a
///   non-reference (primitive or `Top`): over-retention, and on a moving
///   collection a primitive the remap may rewrite.
/// * `Unknown` — no map row for the slot (unmapped pc, slot past the row).
///
/// Nothing here changes a root set. Output: the first 40 differences with
/// their frame, then a summary line at every power-of-two difference count.
pub mod oop_map_shadow {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    /// Is the shadow comparison armed? Read once.
    #[inline]
    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| false)    }

    /// What the type map says about one slot, across the frame's mapped pcs.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MapAnswer {
        /// At least one mapped pc says reference.
        Reference,
        /// Every mapped pc (at least one) says non-reference.
        NotReference,
        /// No mapped pc describes the slot.
        Unknown,
    }

    impl MapAnswer {
        /// Fold the per-pc answers (`true` = reference) of the rows that exist.
        /// A slot past a row's width reads `false` there (`OopBits::get`), which
        /// is the verifier's own answer for a slot it never typed.
        pub fn from_rows(answers: impl Iterator<Item = bool>) -> Self {
            let mut any = false;
            let mut seen = false;
            for a in answers {
                seen = true;
                any |= a;
            }
            match (seen, any) {
                (false, _) => MapAnswer::Unknown,
                (true, true) => MapAnswer::Reference,
                (true, false) => MapAnswer::NotReference,
            }
        }
    }

    /// One slot's comparison result.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Verdict {
        Agree,
        Missed,
        Extra,
        Unknown,
    }

    /// Which half of the frame a slot is in, and which pass compared it: the
    /// root scan (`Local`, `Stack`) or the post-collection remap
    /// (`RemapLocal`, `RemapStack`, where "rooted" means "rewritten" and
    /// "holds a value" means "held a moved object's old address").
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Area {
        Local,
        Stack,
        RemapLocal,
        RemapStack,
    }

    impl Area {
        fn is_remap(self) -> bool {
            matches!(self, Area::RemapLocal | Area::RemapStack)
        }
    }

    /// The comparison itself. `rooted`: the heuristic scan rooted the slot;
    /// `holds_value`: the slot is neither null nor all-zero bits; `live`: the
    /// liveness filter keeps the slot (always `true` for the stack).
    pub fn classify(rooted: bool, holds_value: bool, map: MapAnswer, live: bool) -> Verdict {
        match map {
            MapAnswer::Unknown => Verdict::Unknown,
            MapAnswer::Reference if live && holds_value && !rooted => Verdict::Missed,
            MapAnswer::NotReference if rooted => Verdict::Extra,
            _ => Verdict::Agree,
        }
    }

    static FRAMES: AtomicU64 = AtomicU64::new(0);
    static UNMAPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
    static STACK_DEPTH_MISMATCH: AtomicU64 = AtomicU64::new(0);
    static SLOTS: AtomicU64 = AtomicU64::new(0);
    static UNKNOWN: AtomicU64 = AtomicU64::new(0);
    static MISSED: AtomicU64 = AtomicU64::new(0);
    static EXTRA: AtomicU64 = AtomicU64::new(0);
    static REMAP_FRAMES: AtomicU64 = AtomicU64::new(0);
    static REMAP_MISSED: AtomicU64 = AtomicU64::new(0);
    static REMAP_EXTRA: AtomicU64 = AtomicU64::new(0);
    static PRECISE_DROPPED: AtomicU64 = AtomicU64::new(0);
    static HOTSPOT_RETAINED: AtomicU64 = AtomicU64::new(0);
    static PRINTED: AtomicU64 = AtomicU64::new(0);

    /// A slot the heuristic scan roots that the PRECISE scan
    /// (`CRATONVM_INTERP_PRECISE_OOP_MAPS`) leaves out: the retention stage 3
    /// removes. Counted only while the switch is on; with it on, `extra`
    /// reads what the precise scan still roots against the map (a `long` /
    /// `double` slot's smuggled-handle root), so this is the number that
    /// replaces the off-arm `extra`.
    pub fn note_precise_dropped() {
        PRECISE_DROPPED.fetch_add(1, Ordering::Relaxed);
    }

    /// A liveness-dead local that HotSpot's interpreter oop map
    /// (`GenerateOopMap`, `TypedMask`) still roots but the verifier's
    /// StackMapTable-based map calls a non-reference (javac drops a
    /// scoped-out local from the next frame). Not an `extra`: the scan keeps
    /// it on purpose, with or without the precise switch, for HotSpot parity
    /// (`gci-i10np-interpreted-frames-drop-liveness-dead-reference-locals-hotspot-keeps`).
    pub fn note_hotspot_retained() {
        HOTSPOT_RETAINED.fetch_add(1, Ordering::Relaxed);
    }

    /// `[precise_dropped, hotspot_retained]`.
    pub fn parity_counts() -> [u64; 2] {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [g(&PRECISE_DROPPED), g(&HOTSPOT_RETAINED)]
    }

    /// A frame the comparison could read a map for.
    pub fn note_mapped_frame() {
        FRAMES.fetch_add(1, Ordering::Relaxed);
    }

    /// A mapped frame whose post-collection remap was compared.
    pub fn note_remap_frame() {
        REMAP_FRAMES.fetch_add(1, Ordering::Relaxed);
    }

    /// `[remap_frames, remap_missed, remap_extra]`.
    pub fn remap_counts() -> [u64; 3] {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [g(&REMAP_FRAMES), g(&REMAP_MISSED), g(&REMAP_EXTRA)]
    }

    /// A frame with no map at either pc (unverified class, `--noverify`, a
    /// synthetic frame, a pc the verifier never reached).
    pub fn note_unmapped_frame() {
        UNMAPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
    }

    /// A mapped frame whose operand-stack depth matched no mapped pc.
    pub fn note_stack_depth_mismatch() {
        STACK_DEPTH_MISMATCH.fetch_add(1, Ordering::Relaxed);
    }

    /// `[frames, unmapped_frames, stack_depth_mismatch, slots, unknown, missed, extra]`.
    pub fn counts() -> [u64; 7] {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [
            g(&FRAMES),
            g(&UNMAPPED_FRAMES),
            g(&STACK_DEPTH_MISMATCH),
            g(&SLOTS),
            g(&UNKNOWN),
            g(&MISSED),
            g(&EXTRA),
        ]
    }

    /// Record one slot's verdict; print the first differences in detail and a
    /// running summary at every power-of-two difference count.
    pub fn note(area: Area, verdict: Verdict, slot: usize, bits: u64, frame: &dyn Fn() -> String) {
        let remap = area.is_remap();
        if !remap {
            SLOTS.fetch_add(1, Ordering::Relaxed);
        }
        let total = match verdict {
            Verdict::Agree => return,
            Verdict::Unknown => {
                if !remap {
                    UNKNOWN.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            Verdict::Missed if remap => REMAP_MISSED.fetch_add(1, Ordering::Relaxed) + 1,
            Verdict::Extra if remap => REMAP_EXTRA.fetch_add(1, Ordering::Relaxed) + 1,
            Verdict::Missed => MISSED.fetch_add(1, Ordering::Relaxed) + 1,
            Verdict::Extra => EXTRA.fetch_add(1, Ordering::Relaxed) + 1,
        };
        if PRINTED.fetch_add(1, Ordering::Relaxed) < 40 {
            eprintln!(
                "[interp-oopmap] {verdict:?} {area:?}[{slot}] bits=0x{bits:x} in {}",
                frame()
            );
        }
        if total.is_power_of_two() {
            let [frames, unmapped, depth_mismatch, slots, unknown, missed, extra] = counts();
            let [remap_frames, remap_missed, remap_extra] = remap_counts();
            let [precise_dropped, hotspot_retained] = parity_counts();
            eprintln!(
                "[interp-oopmap] summary frames={frames} unmapped_frames={unmapped} \
                 stack_depth_mismatch={depth_mismatch} slots={slots} unknown={unknown} \
                 missed={missed} extra={extra} remap_frames={remap_frames} \
                 remap_missed={remap_missed} remap_extra={remap_extra} \
                 precise_dropped={precise_dropped} hotspot_retained={hotspot_retained}"
            );
        }
    }

    /// Print the run totals once, at VM exit, when the comparison is armed;
    /// nothing otherwise. The running summary stops at power-of-two
    /// difference counts, so this is the only exact reading, and it prints
    /// even with no difference at all (`frames` is the engagement count: a
    /// zero there means no scan reached a mapped frame, not agreement).
    pub fn report_at_exit() {
        if !enabled() {
            return;
        }
        let [frames, unmapped, depth_mismatch, slots, unknown, missed, extra] = counts();
        let [remap_frames, remap_missed, remap_extra] = remap_counts();
        let [precise_dropped, hotspot_retained] = parity_counts();
        eprintln!(
            "[interp-oopmap-summary] frames={frames} unmapped_frames={unmapped} \
             stack_depth_mismatch={depth_mismatch} slots={slots} unknown={unknown} \
             missed={missed} extra={extra} remap_frames={remap_frames} \
             remap_missed={remap_missed} remap_extra={remap_extra} \
             precise_dropped={precise_dropped} hotspot_retained={hotspot_retained}"
        );
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(test)]
mod tests;
