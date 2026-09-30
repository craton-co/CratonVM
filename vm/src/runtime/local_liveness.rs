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
//! size while HotSpot (whose interpreter oop maps apply per-bci liveness)
//! runs the same bytecode in 16 MB. Diagnosed via
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
//! to the handler). Slot 0 is ALWAYS kept live as extra insurance for
//! receiver-peeking diagnostic paths (mirrors HotSpot's special case for
//! synchronized-method receivers).
//!
//! Kill-switch: `CRATONVM_NO_LOCAL_LIVENESS=1` restores the unfiltered scan.

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
    _max_locals: u16,
    query_pcs: [usize; 2],
) -> LiveMask {
    if code.len() > MAX_CODE_LEN {
        return LiveMask::all_live();
    }

    let way = memo_way(code);
    if let Some(mask) = memo_lookup(way, code, query_pcs) {
        return mask;
    }

    let table = global_table(code, exception_table);
    memo_fill(way, code, table.clone());
    mask_from(table.as_ref(), query_pcs)
}

/// The union of `table`'s live-in sets at `query_pcs`, with slot 0 forced
/// live. `None` (an unanalyzable method) and a pc that is not an instruction
/// start both answer all-live.
#[inline]
fn mask_from(table: Option<&Arc<LivenessTable>>, query_pcs: [usize; 2]) -> LiveMask {
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
    // Slot 0 stays live unconditionally (receiver insurance; see module doc).
    let low = table.rows[r0 as usize * words] | table.rows[r1 as usize * words] | 1;
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
fn memo_lookup(way: usize, code: &Arc<[u8]>, query_pcs: [usize; 2]) -> Option<LiveMask> {
    TLS_MEMO
        .try_with(|m| {
            let m = m.try_borrow().ok()?;
            let e = m[way].as_ref()?;
            if !Arc::ptr_eq(&e.code, code) {
                return None;
            }
            Some(mask_from(e.table.as_ref(), query_pcs))
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

    Some(LivenessTable {
        row_of: instrs
            .iter()
            .enumerate()
            .map(|(i, (pc, _))| (*pc as u32, i as u32))
            .collect(),
        words,
        rows: live_in.into_boxed_slice(),
    })
}

/// Stage 1 of
/// `docs/known-issues/interpreter/i1-L4-proposal-precise-interpreter-oop-maps-20260923.md`:
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
            mask_from(Some(&fresh), [1, 2]).low_word()
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
}
