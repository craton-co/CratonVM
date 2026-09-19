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
//! `ret`, malformed or truncated bytecode, more than 64 locals, oversized
//! methods) makes the whole method fall back to "all locals live" — i.e.
//! exactly the previous behaviour. A slot the analysis reports dead is
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

/// All-slots-live mask (the conservative fallback).
pub const ALL_LIVE: u64 = u64::MAX;

/// Analysis caps: beyond these the method falls back to all-live. 64 locals
/// is the mask width; the code cap bounds worst-case fixpoint cost for
/// pathological generated methods.
const MAX_TRACKED_LOCALS: u16 = 64;
const MAX_CODE_LEN: usize = 32 * 1024;

/// live-in masks per instruction-start pc. `None` table = fall back.
struct LivenessTable {
    live_in: HashMap<u32, u64>,
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

/// Return the union of live-local masks at the given pcs (bit i = local slot
/// i is possibly-live), or [`ALL_LIVE`] when the method cannot be analyzed.
/// Querying a pc that is not a known instruction start yields [`ALL_LIVE`]
/// for that pc (never under-approximates).
pub fn live_locals_mask(
    code: &Arc<[u8]>,
    exception_table: &[ExceptionTableEntry],
    max_locals: u16,
    query_pcs: [usize; 2],
) -> u64 {
    if max_locals > MAX_TRACKED_LOCALS || code.len() > MAX_CODE_LEN {
        return ALL_LIVE;
    }

    let key: CacheKey = (Arc::as_ptr(code) as *const u8 as usize, code.len());
    let table: Option<Arc<LivenessTable>> = {
        let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
        match cache.get(&key) {
            Some((weak, tbl))
                if weak
                    .upgrade()
                    .map(|live| Arc::ptr_eq(&live, code))
                    .unwrap_or(false) =>
            {
                tbl.clone()
            }
            _ => {
                if cache.len() >= CACHE_CAP {
                    cache.clear();
                }
                let tbl = analyze(code, exception_table).map(Arc::new);
                cache.insert(key, (Arc::downgrade(code), tbl.clone()));
                tbl
            }
        }
    };

    let Some(table) = table else {
        return ALL_LIVE;
    };

    let mut mask = 0u64;
    for &pc in &query_pcs {
        mask |= table.live_in.get(&(pc as u32)).copied().unwrap_or(ALL_LIVE);
    }
    // Slot 0 stays live unconditionally (receiver insurance; see module doc).
    mask | 1
}

/// One decoded instruction's dataflow facts.
struct Instr {
    /// Byte length (next sequential pc = pc + len).
    len: usize,
    use_mask: u64,
    def_mask: u64,
    /// Explicit branch targets (absolute pcs).
    targets: Vec<usize>,
    /// Whether control can fall through to `pc + len`.
    falls_through: bool,
}

fn bit(idx: usize) -> Option<u64> {
    if idx < MAX_TRACKED_LOCALS as usize {
        Some(1u64 << idx)
    } else {
        None
    }
}

/// Two-slot (long/double) mask for local `idx`.
fn bit2(idx: usize) -> Option<u64> {
    if idx + 1 < MAX_TRACKED_LOCALS as usize {
        Some((1u64 << idx) | (1u64 << (idx + 1)))
    } else {
        None
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
            use_mask: 0,
            def_mask: 0,
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
                bit2(idx)?
            } else {
                bit(idx)?
            };
            Some(Instr {
                len: 2,
                use_mask: mask,
                def_mask: 0,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        0x36..=0x3a => {
            // istore/lstore/fstore/dstore/astore idx
            let idx = *code.get(pc + 1)? as usize;
            let mask = if op == 0x37 || op == 0x39 {
                bit2(idx)?
            } else {
                bit(idx)?
            };
            Some(Instr {
                len: 2,
                use_mask: 0,
                def_mask: mask,
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
                bit2(idx)?
            } else {
                bit(idx)?
            };
            Some(Instr {
                len: 1,
                use_mask: mask,
                def_mask: 0,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── shorthand stores: istore_0..astore_3 (0x3b..=0x4e) ──────────
        0x3b..=0x4e => {
            let rel = (op - 0x3b) as usize;
            let (kind, idx) = (rel / 4, rel % 4);
            let mask = if kind == 1 || kind == 3 {
                bit2(idx)?
            } else {
                bit(idx)?
            };
            Some(Instr {
                len: 1,
                use_mask: 0,
                def_mask: mask,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── iinc idx const: read+write (model as use so it never kills) ─
        0x84 => {
            let idx = *code.get(pc + 1)? as usize;
            let mask = bit(idx)?;
            Some(Instr {
                len: 3,
                use_mask: mask,
                def_mask: 0,
                targets: Vec::new(),
                falls_through: true,
            })
        }
        // ── conditional branches: 2-byte offset, falls through ──────────
        0x99..=0xa6 | 0xc6 | 0xc7 => Some(Instr {
            len: 3,
            use_mask: 0,
            def_mask: 0,
            targets: vec![branch16(code, pc)?],
            falls_through: true,
        }),
        // ── goto ────────────────────────────────────────────────────────
        0xa7 => Some(Instr {
            len: 3,
            use_mask: 0,
            def_mask: 0,
            targets: vec![branch16(code, pc)?],
            falls_through: false,
        }),
        // goto_w
        0xc8 => {
            let off = read_i32(code, pc + 1)? as isize;
            Some(Instr {
                len: 5,
                use_mask: 0,
                def_mask: 0,
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
                use_mask: 0,
                def_mask: 0,
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
                use_mask: 0,
                def_mask: 0,
                targets,
                falls_through: false,
            })
        }
        // ── returns / athrow: no successors (exception edges are added
        //    separately from the exception table) ─────────────────────────
        0xac..=0xb1 | 0xbf => Some(Instr {
            len: 1,
            use_mask: 0,
            def_mask: 0,
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
                        bit2(idx)?
                    } else {
                        bit(idx)?
                    };
                    Some(Instr {
                        len: 4,
                        use_mask: mask,
                        def_mask: 0,
                        targets: Vec::new(),
                        falls_through: true,
                    })
                }
                0x36..=0x3a => {
                    let mask = if wop == 0x37 || wop == 0x39 {
                        bit2(idx)?
                    } else {
                        bit(idx)?
                    };
                    Some(Instr {
                        len: 4,
                        use_mask: 0,
                        def_mask: mask,
                        targets: Vec::new(),
                        falls_through: true,
                    })
                }
                0x84 => {
                    let mask = bit(idx)?;
                    Some(Instr {
                        len: 6,
                        use_mask: mask,
                        def_mask: 0,
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
    // transfer to the handler.
    for entry in exception_table {
        let (start, end, handler) = (
            entry.start_pc as usize,
            entry.end_pc as usize,
            entry.handler_pc as usize,
        );
        let Some(&h) = index_of.get(&handler) else {
            return None;
        };
        for (i, (pc, _)) in instrs.iter().enumerate() {
            if *pc >= start && *pc < end {
                succs[i].push(h);
            }
        }
    }

    // Pass 2: backward may-liveness fixpoint.
    let mut live_in: Vec<u64> = vec![0; n];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..n).rev() {
            let mut out = 0u64;
            for &s in &succs[i] {
                out |= live_in[s];
            }
            let (_, instr) = &instrs[i];
            let new_in = instr.use_mask | (out & !instr.def_mask);
            if new_in != live_in[i] {
                live_in[i] = new_in;
                changed = true;
            }
        }
    }

    Some(LivenessTable {
        live_in: instrs
            .iter()
            .enumerate()
            .map(|(i, (pc, _))| (*pc as u32, live_in[i]))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_at(code: &[u8], table: &[ExceptionTableEntry], pc: usize) -> u64 {
        let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
        live_locals_mask(&arc, table, 8, [pc, pc])
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
            let mask = live_locals_mask(&arc, &table, 12, [pc, pc]);
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
        assert_eq!(at_pop & (1u64 << 10), 0, "slot 10 is dead after the call");
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
}
