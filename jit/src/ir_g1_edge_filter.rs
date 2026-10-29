// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane vhcae (VHCAE21-1; page
//! `docs/internal/gc/jr21-vhcae-g1-post-barrier-has-no-jit-addressable-refinement-queue-FIXED-20261014.md`):
//! the compiled G1 post barrier's EDGE FILTER.
//!
//! Under G1 a compiled cross-region reference store calls
//! `jit_g1_post_write_barrier` after the store: the single-pass tier's F-08
//! arm (`x64::Compiler::emit_g1_post_write_barrier_regs`, every `putfield` /
//! `aastore` site it serves) and the optimizing tier's gated `putfield`
//! (`Lowerer::emit_g1_ir_ref_putfield`) and G1 `aastore`
//! (`Lowerer::emit_g1_ir_aastore`). In the steady state that call does
//! nothing: the source card is already dirty, its block offset is already low
//! enough, and the `(source region, destination region)` edge is already in
//! the destination's remembered set -- the barrier's per-thread memos answer
//! "hit" and return. The call itself (boundary note, heap lookup, two region
//! lookups, the thread-local reads) is what an old holder taking fresh values
//! pays over the Generational arm.
//!
//! The collector now keeps a per-collector, JIT-addressable copy of the memo's
//! claim (`cratonvm_gc::g1::G1JitEdgeFilter`: per destination region, the last
//! source region recorded in the current reclassification epoch, plus a
//! mirror of that epoch), and this module emits the READ-ONLY test that skips
//! the call when all three facts already hold:
//!
//! 1. both addresses lie in the collector's regions (`addr - base <
//!    region_count << region_shift`) -- anything else is the call's business;
//! 2. the holder's card is dirty and its block offset is at or below the
//!    holder's offset in the card -- exactly the state
//!    `G1CardTable::dirty_addr_at_object_start` would leave, so its stores
//!    would be no-ops (a card is cleaned only inside a pause, and this
//!    sequence holds no safepoint);
//! 3. `entries[dst] == epoch_mirror << 32 | (src + 1)` -- the edge was
//!    recorded in the current epoch, the per-thread memo's own soundness rule
//!    (an rset is a set; only a recycle/retype phase, which bumps the epoch
//!    first, clears or prunes it).
//!
//! Every miss is the unchanged barrier call, which re-records the entry. So
//! the filter can only ever skip a call that would have done nothing.
//!
//! `CRATONVM_JIT_G1_EDGE_FILTER` (**default ON**, `=0` keeps every
//! cross-region store on its call, as before). Per VM: the filter's address
//! comes from this VM's collector through the well-known resolver
//! ([`crate::ir_lower::WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG`]); no process
//! table is read for it.

use crate::ir_lower::{vh_const_ask, vh_const_query, WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG};

/// Word indices of `cratonvm_gc::g1::G1JitEdgeFilter` (pinned there by
/// `offset_of!` asserts). Word 0 is the epoch mirror, read at run time; words
/// 1..=10 are baked at compile time.
const W_ARENA_BASE: usize = 1;
const W_REGIONS_LEN: usize = 2;
const W_REGION_SHIFT: usize = 3;
const W_CARDS_BASE: usize = 4;
const W_STARTS_BASE: usize = 5;
const W_ENTRIES: usize = 6;
const W_REGION_COUNT: usize = 7;
const W_CARD_SHIFT: usize = 8;
const W_BLOCK_GRAIN_SHIFT: usize = 9;
const W_CARD_DIRTY: usize = 10;
/// Words 0..=10.
const W_COUNT: usize = 11;

/// `CRATONVM_JIT_G1_EDGE_FILTER` (JIT round 21, lane vhcae, VHCAE21-1;
/// **default ON**, `=0` keeps every compiled cross-region G1 store on its
/// `jit_g1_post_write_barrier` call). Read per site.
pub fn g1_edge_filter_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_G1_EDGE_FILTER")
}

/// The filter's geometry as one compile bakes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct G1EdgeFilterGeom {
    /// The filter's address: the epoch mirror is its word 0.
    pub(crate) filter: usize,
    pub(crate) arena_base: usize,
    pub(crate) regions_len: usize,
    pub(crate) region_shift: u8,
    pub(crate) cards_base: usize,
    pub(crate) starts_base: usize,
    pub(crate) entries: usize,
    pub(crate) card_shift: u8,
    pub(crate) block_grain_shift: u8,
    pub(crate) card_dirty: u8,
}

impl G1EdgeFilterGeom {
    /// The geometry the words describe, `None` for anything this emitter
    /// cannot express or would express wrongly.
    pub(crate) fn from_words(filter: usize, w: &[usize; W_COUNT]) -> Option<Self> {
        let region_shift = u8::try_from(w[W_REGION_SHIFT]).ok()?;
        let card_shift = u8::try_from(w[W_CARD_SHIFT]).ok()?;
        let block_grain_shift = u8::try_from(w[W_BLOCK_GRAIN_SHIFT]).ok()?;
        let card_dirty = u8::try_from(w[W_CARD_DIRTY]).ok()?;
        let region_count = w[W_REGION_COUNT];
        // The card shift is baked into every other card emitter from
        // `cratonvm_types::CARD_SHIFT`; a collector describing another one is
        // refused rather than trusted.
        if filter == 0
            || w[W_ARENA_BASE] == 0
            || w[W_CARDS_BASE] == 0
            || w[W_STARTS_BASE] == 0
            || w[W_ENTRIES] == 0
            || card_dirty == 0
            || u32::from(card_shift) != cratonvm_types::CARD_SHIFT
            || card_shift >= 31
            || block_grain_shift >= card_shift
            || region_shift < card_shift
            || region_shift >= 48
            || region_count == 0
            // `src + 1` must stay below bit 32 (the epoch's half of the key).
            || region_count >= (1 << 31)
            || w[W_REGIONS_LEN] != region_count << region_shift
        {
            return None;
        }
        Some(Self {
            filter,
            arena_base: w[W_ARENA_BASE],
            regions_len: w[W_REGIONS_LEN],
            region_shift,
            cards_base: w[W_CARDS_BASE],
            starts_base: w[W_STARTS_BASE],
            entries: w[W_ENTRIES],
            card_shift,
            block_grain_shift,
            card_dirty,
        })
    }
}

/// This VM's edge filter, or `None` (switch off, not G1, no resolver, or a
/// geometry [`G1EdgeFilterGeom::from_words`] refuses).
pub(crate) fn g1_edge_filter_geom(helpers: &crate::DirectHelperTable) -> Option<G1EdgeFilterGeom> {
    if !g1_edge_filter_enabled() {
        return None;
    }
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG, 0, 0),
    );
    let addr = usize::try_from(addr)
        .ok()
        .filter(|&a| a != 0 && a % std::mem::align_of::<u64>() == 0)?;
    let mut w = [0usize; W_COUNT];
    for (i, word) in w.iter_mut().enumerate().skip(1) {
        // SAFETY: the VM answers with the address of the collector's boxed
        // `G1JitEdgeFilter`, `#[repr(C)]` with at least `W_COUNT` leading
        // word-sized fields, alive for the VM's whole life. Words 1..=10 are
        // written once, before the address is handed out, and never again;
        // word 0 (written concurrently) is not read here.
        *word = unsafe { (addr as *const usize).add(i).read() };
    }
    G1EdgeFilterGeom::from_words(addr, &w)
}

/// `0F 8x rel32` with a zero operand; returns the operand's offset.
fn jcc(b: &mut Vec<u8>, cc: u8) -> usize {
    b.extend_from_slice(&[0x0F, cc]);
    let at = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    at
}

/// `MOV R11, imm64`.
fn mov_r11(b: &mut Vec<u8>, imm: usize) {
    b.extend_from_slice(&[0x49, 0xBB]);
    b.extend_from_slice(&(imm as u64).to_le_bytes()); // Cast: an address
}

/// The filter's bytes, for RAX = the holder (an object start, non-null) and
/// RDX = the stored value, after the caller's same-region / null screens sent
/// the store to its barrier. Writes RAX, RCX, RDX, R10, R11 (caller-saved on
/// both ABIs; both callers stand where a helper call may clobber all of them)
/// and the flags; reads memory only.
///
/// Falls through when the barrier would do nothing (the HIT: skip the call);
/// every offset in the returned list is the rel32 operand of a MISS branch
/// (take the call), for the caller to patch.
pub(crate) fn g1_edge_filter_hit_bytes(g: &G1EdgeFilterGeom) -> (Vec<u8>, Vec<usize>) {
    let mut b: Vec<u8> = Vec::with_capacity(128);
    let mut miss: Vec<usize> = Vec::new();
    // RCX = holder - base, R10 = value - base.
    mov_r11(&mut b, g.arena_base);
    b.extend_from_slice(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
    b.extend_from_slice(&[0x4C, 0x29, 0xD9]); // SUB RCX, R11
    b.extend_from_slice(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
    b.extend_from_slice(&[0x4D, 0x29, 0xDA]); // SUB R10, R11
                                              // Both inside the regions (unsigned: below the base wraps high).
    mov_r11(&mut b, g.regions_len);
    b.extend_from_slice(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
    miss.push(jcc(&mut b, 0x83)); // JAE -> miss
    b.extend_from_slice(&[0x4D, 0x39, 0xDA]); // CMP R10, R11
    miss.push(jcc(&mut b, 0x83)); // JAE -> miss
                                  // EAX = the holder's block offset in its card (grain units), RCX = card.
    let card_mask: u32 = (1u32 << g.card_shift) - 1;
    b.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    b.push(0x25); // AND EAX, imm32
    b.extend_from_slice(&card_mask.to_le_bytes());
    b.extend_from_slice(&[0xC1, 0xE8, g.block_grain_shift]); // SHR EAX, grain
    b.extend_from_slice(&[0x48, 0xC1, 0xE9, g.card_shift]); // SHR RCX, card_shift
                                                            // The card must already be dirty.
    mov_r11(&mut b, g.cards_base);
    b.extend_from_slice(&[0x41, 0x80, 0x3C, 0x0B, g.card_dirty]); // CMP byte [R11+RCX], dirty
    miss.push(jcc(&mut b, 0x85)); // JNE -> miss
                                  // Its block offset must already be at or below the holder's.
    mov_r11(&mut b, g.starts_base);
    b.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x14, 0x0B]); // MOVZX EDX, byte [R11+RCX]
    b.extend_from_slice(&[0x39, 0xC2]); // CMP EDX, EAX
    miss.push(jcc(&mut b, 0x87)); // JA -> miss (the barrier lowers it)
                                  // R10 = destination region, RCX = source region.
    b.extend_from_slice(&[0x49, 0xC1, 0xEA, g.region_shift]); // SHR R10, region_shift
    let to_region = g.region_shift - g.card_shift;
    if to_region != 0 {
        b.extend_from_slice(&[0x48, 0xC1, 0xE9, to_region]); // SHR RCX, imm8
    }
    // RAX = epoch << 32 | (src + 1), the collector's `G1JitEdgeFilter::key`.
    mov_r11(&mut b, g.filter);
    b.extend_from_slice(&[0x49, 0x8B, 0x03]); // MOV RAX, [R11] (the epoch mirror)
    b.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x20]); // SHL RAX, 32
    b.extend_from_slice(&[0x48, 0x8D, 0x44, 0x08, 0x01]); // LEA RAX, [RAX+RCX+1]
    mov_r11(&mut b, g.entries);
    b.extend_from_slice(&[0x4B, 0x3B, 0x04, 0xD3]); // CMP RAX, [R11+R10*8]
    miss.push(jcc(&mut b, 0x85)); // JNE -> miss
    (b, miss)
}

/// Executed bytes against a fake collector layout: 1 KiB regions, 512-byte
/// cards, a 4 KiB arena (4 regions, 8 cards).
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_vhcae_g1_edge_filter_tests {
    use super::{g1_edge_filter_hit_bytes, G1EdgeFilterGeom, W_COUNT};
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

    #[cfg(windows)]
    const ARG0: u8 = 1; // RCX
    #[cfg(windows)]
    const ARG1: u8 = 2; // RDX
    #[cfg(not(windows))]
    const ARG0: u8 = 7; // RDI
    #[cfg(not(windows))]
    const ARG1: u8 = 6; // RSI

    const REGION_SHIFT: usize = 10;
    const REGIONS: usize = 4;

    #[repr(C, align(1024))]
    struct Arena([u8; 4096]);

    #[repr(C)]
    struct Fake {
        arena: Box<Arena>,
        cards: Box<[AtomicU8; 8]>,
        starts: Box<[AtomicU8; 8]>,
        entries: Box<[AtomicU64; REGIONS]>,
        /// The filter's words; word 0 is the epoch mirror.
        words: Box<[AtomicU64; W_COUNT]>,
    }

    impl Fake {
        fn new() -> Self {
            let f = Fake {
                arena: Box::new(Arena([0; 4096])),
                cards: Box::new(std::array::from_fn(|_| AtomicU8::new(0))),
                starts: Box::new(std::array::from_fn(|_| AtomicU8::new(0xFF))),
                entries: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
                words: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            };
            let w = [
                0usize,
                f.base(),
                REGIONS << REGION_SHIFT,
                REGION_SHIFT,
                f.cards.as_ptr() as usize,   // Cast: an address
                f.starts.as_ptr() as usize,  // Cast: an address
                f.entries.as_ptr() as usize, // Cast: an address
                REGIONS,
                cratonvm_types::CARD_SHIFT as usize, // Cast: a small shift
                3,
                1,
            ];
            for (cell, v) in f.words.iter().zip(w) {
                cell.store(v as u64, Ordering::Relaxed); // Cast: a word
            }
            f
        }
        fn base(&self) -> usize {
            self.arena.0.as_ptr() as usize // Cast: an address
        }
        fn geom(&self) -> G1EdgeFilterGeom {
            let mut w = [0usize; W_COUNT];
            for (i, cell) in self.words.iter().enumerate() {
                w[i] = cell.load(Ordering::Relaxed) as usize; // Cast: a word
            }
            G1EdgeFilterGeom::from_words(self.words.as_ptr() as usize, &w).expect("geometry")
        }
        /// The collector's `note_edge` + card half, for `holder -> value`.
        fn record(&self, holder: usize, value: usize, epoch: u64) {
            let rel = holder - self.base();
            let card = rel >> cratonvm_types::CARD_SHIFT;
            self.cards[card].store(1, Ordering::Relaxed);
            let want = ((rel & ((1 << cratonvm_types::CARD_SHIFT) - 1)) >> 3) as u8; // Cast: < 64
            let cur = self.starts[card].load(Ordering::Relaxed);
            self.starts[card].store(cur.min(want), Ordering::Relaxed);
            let src = rel >> REGION_SHIFT;
            let dst = (value - self.base()) >> REGION_SHIFT;
            self.entries[dst].store((epoch << 32) | (src as u64 + 1), Ordering::Relaxed);
            // Cast: small
        }
    }

    /// `MOV dst, src` (64-bit).
    fn mov(dst: u8, src: u8) -> [u8; 3] {
        [
            0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
            0x89,
            0xC0 | ((src & 7) << 3) | (dst & 7),
        ]
    }

    /// `extern "C" fn(holder, value) -> i64`: 1 on a hit, 0 on a miss.
    fn build(g: &G1EdgeFilterGeom) -> crate::ExecutableBuffer {
        let (bytes, misses) = g1_edge_filter_hit_bytes(g);
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&mov(0, ARG0)); // RAX = holder
        body.extend_from_slice(&mov(2, ARG1)); // RDX = value
        let at = body.len();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0xC3]); // MOV EAX, 1 ; RET
        let miss_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        for m in misses {
            let operand = at + m;
            let rel =
                i32::try_from(miss_at).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn run(buf: &crate::ExecutableBuffer, holder: usize, value: usize) -> i64 {
        // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only
        // the fake tables the caller keeps alive and writes RAX, RCX, RDX,
        // R10, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
        };
        f(holder as u64, value as u64) // Cast: addresses
    }

    #[test]
    fn a_recorded_edge_with_a_dirty_card_is_a_hit_and_everything_else_misses() {
        let f = Fake::new();
        let buf = build(&f.geom());
        let holder = f.base() + 0x0C0 + 512; // region 0, card 1, offset 0xC0 in it
        let value = f.base() + 0x0800 + 0x40; // region 2
        assert_eq!(run(&buf, holder, value), 0, "nothing recorded");
        f.record(holder, value, 0);
        assert_eq!(run(&buf, holder, value), 1, "recorded in this epoch");
        // A lower holder in the same card: its block offset is not covered.
        assert_eq!(run(&buf, holder - 0x40, value), 0, "block offset to lower");
        // A higher holder in the same card and region: covered.
        assert_eq!(run(&buf, holder + 0x40, value), 1, "block offset covers it");
        // Another source region into the same destination: another edge.
        assert_eq!(run(&buf, f.base() + 0x0400 + 512, value), 0, "another edge");
        // The epoch moves: every entry is stale.
        f.words[0].store(1, Ordering::Relaxed);
        assert_eq!(run(&buf, holder, value), 0, "an older epoch");
        f.record(holder, value, 1);
        assert_eq!(run(&buf, holder, value), 1, "re-recorded");
        // A clean card (as after a region reset) misses although the edge is
        // recorded.
        f.cards[1].store(0, Ordering::Relaxed);
        assert_eq!(run(&buf, holder, value), 0, "a clean card");
        f.cards[1].store(1, Ordering::Relaxed);
        // Addresses outside the regions miss before any table read.
        assert_eq!(
            run(&buf, holder, f.base() + 4096),
            0,
            "value past the regions"
        );
        assert_eq!(run(&buf, f.base() - 64, value), 0, "holder below the base");
        assert_eq!(run(&buf, holder, 0), 0, "a null value");
        assert_eq!(run(&buf, holder, value), 1);
    }

    #[test]
    fn the_key_keeps_only_the_epochs_low_half_like_the_collector() {
        let f = Fake::new();
        let buf = build(&f.geom());
        let holder = f.base() + 0x0C00; // region 3, card 6
        let value = f.base() + 0x0010; // region 0
        let epoch = (7u64 << 32) | 5;
        f.words[0].store(epoch, Ordering::Relaxed);
        f.record(holder, value, epoch);
        assert_eq!(run(&buf, holder, value), 1);
        f.words[0].store(5, Ordering::Relaxed);
        assert_eq!(
            run(&buf, holder, value),
            1,
            "only the low 32 bits are compared"
        );
    }

    #[test]
    fn geometry_the_emitter_cannot_express_is_refused() {
        let f = Fake::new();
        let g = f.geom();
        let mut w = [0usize; W_COUNT];
        for (i, cell) in f.words.iter().enumerate() {
            w[i] = cell.load(Ordering::Relaxed) as usize; // Cast: a word
        }
        let addr = f.words.as_ptr() as usize; // Cast: an address
        assert_eq!(G1EdgeFilterGeom::from_words(addr, &w), Some(g));
        assert_eq!(G1EdgeFilterGeom::from_words(0, &w), None);
        let mut bad = w;
        bad[super::W_CARD_SHIFT] += 1;
        assert_eq!(
            G1EdgeFilterGeom::from_words(addr, &bad),
            None,
            "another card size"
        );
        let mut bad = w;
        bad[super::W_REGION_SHIFT] = 8;
        assert_eq!(
            G1EdgeFilterGeom::from_words(addr, &bad),
            None,
            "regions below a card"
        );
        let mut bad = w;
        bad[super::W_REGIONS_LEN] += 1;
        assert_eq!(
            G1EdgeFilterGeom::from_words(addr, &bad),
            None,
            "inconsistent span"
        );
        let mut bad = w;
        bad[super::W_ENTRIES] = 0;
        assert_eq!(G1EdgeFilterGeom::from_words(addr, &bad), None, "no entries");
    }
}
