// SPDX-License-Identifier: Apache-2.0
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
//! JIT alike; `tools/probes/GciOb5ParkedDeadLocalProbe.java` prints
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
/// `docs/internal/proposals/interpreter/i1-L4-proposal-precise-interpreter-oop-maps-20260923.md`:
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
        *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_VERIFY_OOP_MAPS"))
    }

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
    static PRINTED: AtomicU64 = AtomicU64::new(0);

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
            eprintln!(
                "[interp-oopmap] summary frames={frames} unmapped_frames={unmapped} \
                 stack_depth_mismatch={depth_mismatch} slots={slots} unknown={unknown} \
                 missed={missed} extra={extra} remap_frames={remap_frames} \
                 remap_missed={remap_missed} remap_extra={remap_extra}"
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
        eprintln!(
            "[interp-oopmap-summary] frames={frames} unmapped_frames={unmapped} \
             stack_depth_mismatch={depth_mismatch} slots={slots} unknown={unknown} \
             missed={missed} extra={extra} remap_frames={remap_frames} \
             remap_missed={remap_missed} remap_extra={remap_extra}"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn map_answers_fold_across_the_mapped_pcs() {
            assert_eq!(MapAnswer::from_rows([].into_iter()), MapAnswer::Unknown);
            assert_eq!(
                MapAnswer::from_rows([false, true].into_iter()),
                MapAnswer::Reference
            );
            assert_eq!(
                MapAnswer::from_rows([false, false].into_iter()),
                MapAnswer::NotReference
            );
        }

        #[test]
        fn classify_names_each_disagreement() {
            use MapAnswer::*;
            // A live, non-null reference the scan did not root.
            assert_eq!(classify(false, true, Reference, true), Verdict::Missed);
            // Null or dead references are not owed a root.
            assert_eq!(classify(false, false, Reference, true), Verdict::Agree);
            assert_eq!(classify(false, true, Reference, false), Verdict::Agree);
            // A rooted primitive / `Top` slot.
            assert_eq!(classify(true, true, NotReference, true), Verdict::Extra);
            // Agreement both ways, and no map.
            assert_eq!(classify(true, true, Reference, true), Verdict::Agree);
            assert_eq!(classify(false, true, NotReference, true), Verdict::Agree);
            assert_eq!(classify(true, true, Unknown, true), Verdict::Unknown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_at(code: &[u8], table: &[ExceptionTableEntry], pc: usize) -> u64 {
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        live_locals_mask(&arc, table, 8, [pc, pc]).low_word()
    }

    /// aload_1; astore_2; return  — after the store both 1 and 2 are dead at
    /// `return` (bit 0 is always forced live).
    #[test]
    fn store_then_never_read_is_dead() {
        let code = [0x2b, 0x4d, 0xb1];
        assert_eq!(mask_at(&code, &[], 2), 1); // return: only forced slot 0
        assert_eq!(mask_at(&code, &[], 0), 0b10 | 1); // aload_1 uses slot 1
    }

    /// Backward branch keeps the loop variable live across the body:
    ///   0: aload_1 ; 1: pop ; 2: goto 0
    #[test]
    fn loop_backedge_keeps_slot_live() {
        let code = [0x2b, 0x57, 0xa7, 0xff, 0xfe]; // goto -2 → pc 0
        assert_eq!(mask_at(&code, &[], 2) & 0b10, 0b10);
    }

    /// A handler that reads slot 3 keeps it live throughout the try range
    /// even though the mainline never reads it.
    ///   0: nop ; 1: nop ; 2: return ; 3: aload_3 ; 4: athrow
    #[test]
    fn exception_handler_use_extends_liveness() {
        let code = [0x00, 0x00, 0xb1, 0x2d, 0xbf];
        let table = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 2,
            handler_pc: 3,
            catch_type: 0,
        }];
        assert_eq!(mask_at(&code, &table, 0) & 0b1000, 0b1000);
        assert_eq!(mask_at(&code, &table, 1) & 0b1000, 0b1000);
        // Outside the guarded range the handler edge is gone and slot 3 dead.
        assert_eq!(mask_at(&code, &table, 2) & 0b1000, 0);
    }

    /// jsr makes the whole method unanalyzable → all-live.
    #[test]
    fn jsr_bails_to_all_live() {
        let code = [0xa8, 0x00, 0x03, 0xb1, 0xb1];
        assert_eq!(mask_at(&code, &[], 3), ALL_LIVE | 1);
    }

    /// lstore_1 kills BOTH slots 1 and 2.
    ///   0: lload_1 ; 1: lstore_1 ; 2: return
    #[test]
    fn cat2_store_kills_both_slots() {
        let code = [0x1f, 0x40, 0xb1];
        let m = mask_at(&code, &[], 2);
        assert_eq!(m & 0b110, 0);
        // And lload_1 at 0 uses both.
        let m0 = mask_at(&code, &[], 0);
        assert_eq!(m0 & 0b110, 0b110);
    }

    /// iinc never kills (read+write).
    #[test]
    fn iinc_is_a_use() {
        // 0: iinc 2, 1 ; 3: goto 0
        let code = [0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd];
        assert_eq!(mask_at(&code, &[], 0) & 0b100, 0b100);
    }

    /// H2-CID0-BLOCKED regression: the enhanced-for iterator must stay live at
    /// the call the thread parks in.
    ///
    /// `for (Future<Void> job : jobs) job.get(5, MINUTES);` — javac emits a
    /// synthetic `Iterator` local (slot 9 in
    /// `TestMultiThread.testConcurrentUpdate`) that is read only at the loop
    /// head, i.e. only across the BACKWARD branch from after the blocking
    /// call. A liveness analysis that did not reach a fixpoint over that back
    /// edge would report the slot dead exactly where the thread parks — and
    /// this mask is what `Frame::scan_local_objects` filters the blocked-thread
    /// root snapshot with, so a `false` here means the collector reclaims a
    /// live iterator and the owner wakes to
    /// `NoSuchMethodError java/lang/Object.hasNext()Z`. The whole enclosing
    /// range is also inside a `try`, whose handler never reads the slot, so
    /// the exception successor must not be allowed to kill it either.
    ///
    /// Byte offsets mirror the real method (loop head 7, `Future.get` at 37).
    #[test]
    fn enhanced_for_iterator_is_live_at_the_blocking_call() {
        #[rustfmt::skip]
        let code = [
            0x19, 0x08,                    //  0: aload 8      (jobs)
            0xb6, 0x01, 0x0c,              //  2: invokevirtual iterator
            0x3a, 0x09,                    //  5: astore 9     (the iterator)
            0x19, 0x09,                    //  7: aload 9      <- loop head
            0xb9, 0x01, 0x10, 0x01, 0x00,  //  9: invokeinterface hasNext
            0x99, 0x00, 0x20,              // 14: ifeq -> 46
            0x19, 0x09,                    // 17: aload 9
            0xb9, 0x01, 0x15, 0x01, 0x00,  // 19: invokeinterface next
            0xc0, 0x01, 0x17,              // 24: checkcast Future
            0x3a, 0x0a,                    // 27: astore 10    (job)
            0x19, 0x0a,                    // 29: aload 10
            0x14, 0x01, 0x4c,              // 31: ldc2_w 5L
            0xb2, 0x01, 0x4e,              // 34: getstatic MINUTES
            0xb9, 0x01, 0x51, 0x04, 0x00,  // 37: invokeinterface Future.get
            0x57,                          // 42: pop
            0xa7, 0xff, 0xdc,              // 43: goto -> 7
            0xb1,                          // 46: return
            0x3a, 0x0b,                    // 47: astore 11    (handler)
            0x19, 0x0b,                    // 49: aload 11
            0xbf,                          // 51: athrow
        ];
        let table = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 46,
            handler_pc: 47,
            catch_type: 0,
        }];
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        let iterator = 1u64 << 9;
        for pc in [37usize, 42, 43] {
            let mask = live_locals_mask(&arc, &table, 12, [pc, pc]).low_word();
            assert_ne!(
                mask & iterator,
                0,
                "the enhanced-for iterator (slot 9) must be live at pc={pc}: a blocked \
                 thread's root snapshot is filtered by this mask, and dropping it is a \
                 use-after-free the owner sees as ClassId(0)",
            );
        }
        // `job` (slot 10) is genuinely dead once `get()` has consumed it — the
        // analysis is doing real work here, not returning ALL_LIVE.
        let at_pop = live_locals_mask(&arc, &table, 12, [42, 42]);
        assert!(!at_pop.is_live(10), "slot 10 is dead after the call");
    }

    /// The per-thread memo is keyed by code-blob IDENTITY, never by content.
    ///
    /// Two blobs with identical bytes but different exception tables have
    /// different liveness (the handler edge keeps slot 3 live), and the memo
    /// answering the second from the first would drop a GC root. Each blob is
    /// queried twice so the second answer comes from the memo, not the
    /// analysis.
    #[test]
    fn thread_memo_is_keyed_by_code_identity_not_content() {
        let bytes = [0x00u8, 0x00, 0xb1, 0x2d, 0xbf];
        let with_handler: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        let without_handler: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        let table = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 2,
            handler_pc: 3,
            catch_type: 0,
        }];
        for _ in 0..2 {
            let a = live_locals_mask(&with_handler, &table, 8, [0, 0]);
            let b = live_locals_mask(&without_handler, &[], 8, [0, 0]);
            assert!(a.is_live(3), "handler keeps slot 3 live");
            assert!(!b.is_live(3), "no handler, slot 3 dead");
        }
        // A memo hit answers exactly what the uncached analysis answers.
        let fresh = Arc::new(analyze(&with_handler, &table).expect("analyzable"));
        assert_eq!(
            live_locals_mask(&with_handler, &table, 8, [1, 2]).low_word(),
            mask_from(Some(&fresh), [1, 2], true).low_word()
        );
    }

    /// Unknown / mis-decoding bytecode falls back to all-live rather than
    /// under-approximating.
    #[test]
    fn garbage_bails_to_all_live() {
        let code = [0xfe, 0x00];
        assert_eq!(mask_at(&code, &[], 0), ALL_LIVE | 1);
    }

    /// The SteadyChurn shape at unit scale: a temp stored in the setup block
    /// and never read afterwards is DEAD in the loop that follows.
    ///   0: aload_1 ; 1: astore_3   (temp := param)
    ///   2: aload_2 ; 3: pop ; 4: goto 2   (endless loop reading only slot 2)
    #[test]
    fn scoped_out_temp_is_dead_in_later_loop() {
        let code = [0x2b, 0x4e, 0x2c, 0x57, 0xa7, 0xff, 0xfe];
        let in_loop = mask_at(&code, &[], 2);
        assert_eq!(in_loop & 0b1000, 0, "temp (slot 3) must be dead in loop");
        assert_eq!(in_loop & 0b100, 0b100, "loop variable (slot 2) live");
    }

    /// A method with more than 64 locals is analysed for ALL its slots up to
    /// `MAX_TRACKED_LOCALS` (wave 2 covered only 0..64; before that it fell
    /// back to all-live wholesale).
    ///   0: aload_1 ; 1: astore_3        (temp := param)
    ///   2: aload 70 ; 4: astore 71      (slots in the second word)
    ///   6: dload 63 ; 8: pop2           (cat-2 straddling the word boundary)
    ///   9: aload_2 ; 10: pop ; 11: goto 2
    #[test]
    fn slots_beyond_64_are_tracked() {
        let code = [
            0x2b, 0x4e, 0x19, 70, 0x3a, 71, 0x18, 63, 0x58, 0x2c, 0x57, 0xa7, 0xff, 0xf7,
        ];
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        let in_loop = live_locals_mask(&arc, &[], 80, [9, 9]);
        assert_ne!(
            in_loop.low_word(),
            ALL_LIVE,
            "a >64-local method must be analysed"
        );
        assert!(!in_loop.is_live(3), "temp (slot 3) must be dead in loop");
        assert!(in_loop.is_live(2), "loop variable (slot 2) live");
        assert!(in_loop.is_live(63), "dload 63 keeps slot 63 live");
        assert!(in_loop.is_live(64), "... and its upper half, slot 64");
        assert!(
            in_loop.is_live(70),
            "aload 70 at the loop head keeps 70 live"
        );
        assert!(
            !in_loop.is_live(71),
            "slot 71 is stored before any read on every path: dead"
        );
        assert!(!in_loop.is_live(100), "a slot never touched is dead");
        // Past the words the table holds, every slot is live.
        assert!(in_loop.is_live(200));
    }

    /// Slots from `MAX_TRACKED_LOCALS` up stay untracked (live), and the rows
    /// below them are still exact.
    ///   0: aconst_null ; 1: wide astore 600 ; 5: aload_1 ; 6: pop ; 7: goto 5
    #[test]
    fn slots_beyond_the_tracked_cap_are_live() {
        let code = [0x01, 0xc4, 0x3a, 0x02, 0x58, 0x2b, 0x57, 0xa7, 0xff, 0xfe];
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        let in_loop = live_locals_mask(&arc, &[], 601, [5, 5]);
        assert!(in_loop.is_live(1));
        assert!(in_loop.is_live(600), "untracked slot must read as live");
        assert!(!in_loop.is_live(100), "tracked, never read: dead");
        assert!(!in_loop.is_live(511), "the last tracked slot is exact too");
    }

    /// A method above the old 32 KiB cap is analysed (wave 4 raised the cap to
    /// the class-file maximum once the fixpoint became a worklist).
    ///   0: aload_1 ; 1: astore_3 ; 40_000 x nop ; aload_2 ; pop ; goto -2
    #[test]
    fn methods_above_32k_are_analysed() {
        let mut code: Vec<u8> = vec![0x2b, 0x4e];
        code.resize(2 + 40_000, 0x00);
        let head = code.len();
        code.extend_from_slice(&[0x2c, 0x57, 0xa7, 0xff, 0xfe]);
        let in_loop = mask_at(&code, &[], head);
        assert_ne!(in_loop, ALL_LIVE | 1, "a 40 KB method must be analysed");
        assert_eq!(in_loop & 0b1000, 0, "temp (slot 3) must be dead in loop");
        assert_eq!(in_loop & 0b100, 0b100, "loop variable (slot 2) live");
        // Liveness reaches back across the 40 000 straight-line nops too.
        assert_eq!(mask_at(&code, &[], 2) & 0b100, 0b100);
    }

    /// Past the class-file maximum (plus padding) the method is not analysed.
    #[test]
    fn code_past_the_class_file_limit_falls_back() {
        let code = vec![0x00u8; MAX_CODE_LEN + 1];
        assert_eq!(mask_at(&code, &[], 0), ALL_LIVE | 1);
    }

    /// Liveness must cross an inner back edge AND the outer one: slot 5 is read
    /// only at the outer loop head, so inside the inner loop it is live only
    /// through inner exit -> outer back edge -> head.
    ///   0: aload 5 ; 2: pop ; 3: aload_2 ; 4: pop ; 5: iload_3 ;
    ///   6: ifne -> 3 ; 9: goto -> 0
    #[test]
    fn nested_back_edges_reach_the_fixpoint() {
        let code = [
            0x19, 0x05, 0x57, 0x2c, 0x57, 0x1d, 0x9a, 0xff, 0xfd, 0xa7, 0xff, 0xf7,
        ];
        for pc in [3usize, 4, 5, 6, 9] {
            let m = mask_at(&code, &[], pc);
            assert_ne!(m & (1 << 5), 0, "slot 5 must be live at pc={pc}");
            assert_eq!(m & (1 << 4), 0, "slot 4 is never touched at pc={pc}");
        }
        assert_ne!(mask_at(&code, &[], 3) & 0b1100, 0);
    }

    /// gci i3/il: a static method's slot 0 is its first parameter, not a
    /// receiver -- `relay(Object a) { dropStatic(a); ... }` suspended in its
    /// `invokestatic` with `a` dead must not root `a`.
    ///   0: aload_0 ; 1: invokestatic #2 ; 4: return
    #[test]
    fn gci_i3il_a_static_methods_dead_first_parameter_is_not_forced_live() {
        let code: Arc<[u8]> = Arc::from(vec![0x2a, 0xb8, 0x00, 0x02, 0xb1].into_boxed_slice());
        // Suspended inside the invoke: pc and last_instr_pc both name it.
        let static_mask = live_locals_mask_for(&code, &[], 1, [1, 1], false);
        assert!(!static_mask.is_live(0), "static: slot 0 is dead after its last read");
        // Before the read, liveness keeps it on its own.
        let at_load = live_locals_mask_for(&code, &[], 1, [0, 0], false);
        assert!(at_load.is_live(0), "slot 0 is read at pc 0");
        // An instance method (and the legacy entry point) keeps the receiver.
        let instance_mask = live_locals_mask_for(&code, &[], 1, [1, 1], true);
        assert!(instance_mask.is_live(0), "the receiver stays live");
        assert!(live_locals_mask(&code, &[], 1, [1, 1]).is_live(0));
        // The per-thread memo answers each question for its own caller: the
        // second static query is a memo hit and must not inherit bit 0.
        let again = live_locals_mask_for(&code, &[], 1, [1, 1], false);
        assert!(!again.is_live(0), "the memo must not cache the forced bit");
    }

    /// An unanalysable method stays all-live for a static frame too.
    #[test]
    fn gci_i3il_static_fallback_is_still_all_live() {
        let code: Arc<[u8]> = Arc::from(vec![0xfe, 0x00].into_boxed_slice());
        let m = live_locals_mask_for(&code, &[], 1, [0, 0], false);
        assert_eq!(m.low_word(), ALL_LIVE);
    }

    fn typed_at(code: &[u8], table: &[ExceptionTableEntry], pc: usize, params: usize) -> TypedMask {
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        typed_locals_mask_for(&arc, table, [pc, pc], params).expect("analyzable")
    }

    /// gci i10/np: the SteadyChurn shape keeps its temp TYPED in the later
    /// loop (both edges into the loop head carry the reference), although
    /// liveness calls it dead -- HotSpot's interpreter oop map keeps it too.
    ///   0: aload_1 ; 1: astore_3 ; 2: aload_2 ; 3: pop ; 4: goto 2
    #[test]
    fn gci_i10np_a_scoped_out_reference_stays_typed_in_a_later_loop() {
        let code = [0x2b, 0x4e, 0x2c, 0x57, 0xa7, 0xff, 0xfe];
        assert_eq!(mask_at(&code, &[], 2) & 0b1000, 0, "fixture: slot 3 dead");
        let typed = typed_at(&code, &[], 2, 3);
        assert!(typed.is_ref(3), "the temp is a reference on every path");
        assert!(typed.is_ref(1) && typed.is_ref(2), "unwritten parameters keep their type");
        // Before the store the temp slot holds nothing: not a parameter.
        assert!(!typed_at(&code, &[], 1, 3).is_ref(3));
    }

    /// A reference on one path and nothing on the other is a conflict at the
    /// join (GenerateOopMap's ref/uninit merge), unless the slot is a
    /// parameter (then both paths hold a reference-or-entry value).
    ///   0: iload_1 ; 1: ifeq -> 7 ; 4: aload_2 ; 5: astore_3 ; 6: nop ; 7: return
    #[test]
    fn gci_i10np_a_one_sided_store_is_not_typed_at_the_join() {
        let code = [0x1b, 0x99, 0x00, 0x06, 0x2c, 0x4e, 0x00, 0xb1];
        assert!(typed_at(&code, &[], 6, 3).is_ref(3), "only the storing path reaches pc 6");
        assert!(!typed_at(&code, &[], 7, 3).is_ref(3), "join of ref and unwritten");
        assert!(typed_at(&code, &[], 7, 4).is_ref(3), "a parameter slot keeps its type");
    }

    /// A primitive store (here `istore_3`, and a `long` store over the pair)
    /// ends the reference type.
    ///   0: aload_1 ; 1: astore_3 ; 2: iconst_0 ; 3: istore_3 ; 4: nop ;
    ///   5: aload_1 ; 6: astore 4 ; 8: lconst_0 ; 9: lstore_3 ; 10: return
    #[test]
    fn gci_i10np_a_primitive_store_ends_the_type() {
        let code = [
            0x2b, 0x4e, 0x03, 0x3e, 0x00, 0x2b, 0x3a, 0x04, 0x09, 0x42, 0xb1,
        ];
        assert!(typed_at(&code, &[], 2, 2).is_ref(3));
        assert!(!typed_at(&code, &[], 4, 2).is_ref(3), "istore_3 ends it");
        assert!(typed_at(&code, &[], 8, 2).is_ref(4));
        let at_return = typed_at(&code, &[], 10, 2);
        assert!(!at_return.is_ref(3) && !at_return.is_ref(4), "lstore_3 covers 3 and 4");
    }

    /// An exception handler meets the state BEFORE each covered instruction:
    /// a store inside the try does not type the slot at the handler.
    ///   0: nop ; 1: aload_1 ; 2: astore_3 ; 3: return ; 4: astore 5 ; 6: return
    ///   (try [0, 3) -> 4)
    #[test]
    fn gci_i10np_a_handler_sees_the_pre_store_state() {
        let code = [0x00, 0x2b, 0x4e, 0xb1, 0x3a, 0x05, 0xb1];
        let table = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 3,
            handler_pc: 4,
            catch_type: 0,
        }];
        assert!(typed_at(&code, &table, 3, 2).is_ref(3), "mainline: stored");
        assert!(!typed_at(&code, &table, 4, 2).is_ref(3), "handler: maybe never stored");
        assert!(typed_at(&code, &table, 6, 2).is_ref(5), "the handler's own astore types slot 5");
    }

    /// A back edge into pc 0 meets the entry state there.
    ///   0: aload_1 ; 1: astore_2 ; 2: iconst_0 ; 3: istore_1 ; 4: goto 0
    #[test]
    fn gci_i10np_a_loop_back_to_the_entry_meets_the_entry_state() {
        let code = [0x2b, 0x4d, 0x03, 0x3c, 0xa7, 0xff, 0xfc];
        let at_entry = typed_at(&code, &[], 0, 2);
        assert!(!at_entry.is_ref(1), "slot 1 is an int after the first iteration");
        assert!(!at_entry.is_ref(2), "slot 2 is unwritten on the entry path");
        assert!(typed_at(&code, &[], 2, 2).is_ref(2));
    }

    /// The parameter count is read from the descriptor.
    #[test]
    fn gci_i10np_entry_param_slots_counts_the_descriptor() {
        assert_eq!(entry_param_slots("()V", false), 0);
        assert_eq!(entry_param_slots("()V", true), 1);
        assert_eq!(entry_param_slots("(IJ[Ljava/lang/String;D)V", true), 7);
        assert_eq!(entry_param_slots("([[I[[Ljava/lang/Object;Z)I", false), 3);
        assert_eq!(entry_param_slots("IV", false), usize::MAX);
        assert_eq!(entry_param_slots("(Ljava/lang/Object", false), usize::MAX);
        assert_eq!(entry_param_slots("([", false), usize::MAX);
    }

    /// An unanalysable method has no type answer (the caller keeps liveness).
    #[test]
    fn gci_i10np_an_unanalysable_method_has_no_type_answer() {
        let arc: Arc<[u8]> = Arc::from(vec![0xfe, 0x00].into_boxed_slice());
        assert!(typed_locals_mask_for(&arc, &[], [0, 0], 1).is_none());
    }
}
