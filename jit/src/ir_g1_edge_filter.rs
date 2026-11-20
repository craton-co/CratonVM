// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane vhcae (VHCAE21-1; page
//! `docs/feature-designs/jit-barrier-lowering.md`):
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
    true
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

fn patch_rel32(b: &mut [u8], at: usize, target: usize) {
    let rel = i32::try_from(target).expect("small") - i32::try_from(at + 4).expect("small");
    b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
}

fn rip_disp32(here: usize, len: usize, target: usize) -> Option<i32> {
    let next = (here as i128).checked_add(len as i128)?;
    i32::try_from((target as i128) - next).ok()
}

/// What [`g1_aastore_screen_with_geom`] emitted: the bytes, the decline and
/// cross-region rel32 fields, and whether `R10` holds `holder - arena_base`
/// on the cross edge (then [`g1_edge_filter_hit_bytes_from_offsets`] applies;
/// otherwise [`g1_edge_filter_hit_bytes_rip`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct G1AastoreScreen {
    pub(crate) bytes: Vec<u8>,
    pub(crate) decline: Vec<usize>,
    pub(crate) cross: Vec<usize>,
    pub(crate) holder_in_r10: bool,
}

/// THIS collector's `JIT_G1_YOUNG` table, baked: the region array, its count
/// and the type-byte geometry. Only for a table OWNED by the collector the
/// edge filter describes (its `W_BASE` is `g.arena_base`, with the same shift
/// and region count): that collector publishes both tables
/// (`G1Collector::new`), its region array never moves or resizes, and while
/// it lives no other collector can take the claim (a second one only poisons
/// the shift to `0` and leaves the words). Code compiled for this VM never
/// outlives its collector, so the run-time `CMP [table + 24], shift; JNE`
/// liveness test of the shared encoder (`x64::g1_young_holder_skip_bytes`)
/// and its loads of the array and the count have nothing left to catch here.
/// A table owned by ANOTHER collector is refused: its regions are not the
/// ones this VM's arena offsets index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct G1OwnedYoung {
    pub(crate) regions: usize,
    pub(crate) count: i32,
    pub(crate) stride: i32,
    pub(crate) type_offset: i32,
    pub(crate) young_max: u8,
}

impl G1OwnedYoung {
    /// The baked table, from the words of `JIT_G1_YOUNG` (index layout
    /// `cratonvm_types::jit_g1_young::W_*`), or `None` unless they describe
    /// `g`'s own collector with geometry the encoder can use (`young_max`
    /// below `0x80`: it is compared as a sign-extended imm8).
    pub(crate) fn from_table(g: &G1EdgeFilterGeom, w: &[usize; 8]) -> Option<Self> {
        use cratonvm_types::jit_g1_young::{
            W_BASE, W_COUNT as Y_COUNT, W_REGIONS, W_SHIFT, W_STRIDE, W_TYPE_OFFSET, W_YOUNG_MAX,
        };
        let count = w[Y_COUNT];
        if w[W_SHIFT] != usize::from(g.region_shift)
            || w[W_BASE] != g.arena_base
            || w[W_REGIONS] == 0
            || count == 0
            || count.checked_mul(1usize << g.region_shift) != Some(g.regions_len)
            || w[W_STRIDE] == 0
            || w[W_TYPE_OFFSET] >= w[W_STRIDE]
            || w[W_YOUNG_MAX] >= 0x80
        {
            return None;
        }
        Some(Self {
            regions: w[W_REGIONS],
            count: i32::try_from(count).ok()?,
            stride: i32::try_from(w[W_STRIDE]).ok()?,
            type_offset: i32::try_from(w[W_TYPE_OFFSET]).ok()?,
            young_max: u8::try_from(w[W_YOUNG_MAX]).ok()?,
        })
    }
}

/// [`G1OwnedYoung`] for `g`, read from the live table at compile time: the
/// shift (the liveness word, stored last by the publisher) first, then the
/// rest. `None` under `CRATONVM_JIT_G1_INLINE_YOUNG_FILTER=0` or with no
/// published table (both answered by the shared reader).
fn g1_owned_young(g: &G1EdgeFilterGeom) -> Option<G1OwnedYoung> {
    crate::x64::g1_young_holder_skip_bytes(11, None, 1)?;
    let t = &cratonvm_types::jit_g1_young::JIT_G1_YOUNG.words;
    let shift = cratonvm_types::jit_g1_young::W_SHIFT;
    let mut w = [0usize; 8];
    w[shift] = t[shift].load(std::sync::atomic::Ordering::Acquire);
    for (i, word) in w.iter_mut().enumerate().filter(|(i, _)| *i != shift) {
        *word = t[i].load(std::sync::atomic::Ordering::Acquire);
    }
    G1OwnedYoung::from_table(g, &w)
}

/// Combined G1 `aastore` screen using baked [`G1EdgeFilterGeom`].
///
/// Entry: `RAX = holder` (non-null), `RDX = value`. Clobbers `RCX, R10, R11`
/// and flags; `RAX` and `RDX` survive on every exit. Falling off the end is
/// the barrier-free store (null value, same region, young holder).
///
/// ```text
///   cmp byte [pre], 0 ; jne decline
///   test rdx, rdx ; jz plain
///   mov rcx, rdx ; xor rcx, rax ; shr rcx, region_shift ; jz plain
///   mov r11, arena_base ; mov r10, rax ; sub r10, r11      ; R10 = holder offset
///   mov r11, r10 ; shr r11, region_shift ; cmp r11, count ; jae cross
///   imul r11, r11, stride ; mov rcx, regions
///   movzx r11d, byte [r11 + rcx + type_offset] ; cmp r11d, young_max ; ja cross
/// plain:
/// ```
///
/// The same-region test needs no `arena_base` when the base is
/// region-aligned: `(h - b) >> s == (h >> s) - (b >> s)` (mod `2^(64-s)`)
/// for every `h`, so `((h - b) ^ (v - b)) >> s == 0` exactly when
/// `(h ^ v) >> s == 0` -- in or out of the arena, the decision the offset
/// form made. An unaligned base keeps the offset form. The young-holder test
/// is [`G1OwnedYoung`]'s (no young test without one: the cross edge is taken
/// directly). A holder outside the arena fails its index bound and takes the
/// cross edge, whose filter and call decide.
pub(crate) fn g1_aastore_screen_with_geom(
    plan: crate::x64::G1AastorePlan,
    g: &G1EdgeFilterGeom,
    here: usize,
) -> G1AastoreScreen {
    g1_aastore_screen_with_young(plan, g, here, g1_owned_young(g))
}

/// [`g1_aastore_screen_with_geom`] with the young table given (tests).
pub(crate) fn g1_aastore_screen_with_young(
    plan: crate::x64::G1AastorePlan,
    g: &G1EdgeFilterGeom,
    here: usize,
    young: Option<G1OwnedYoung>,
) -> G1AastoreScreen {
    let mut b: Vec<u8> = Vec::with_capacity(128);
    let mut decline: Vec<usize> = Vec::new();
    let mut cross: Vec<usize> = Vec::new();
    if let Some(disp) = rip_disp32(here + b.len(), 7, plan.pre) {
        b.extend_from_slice(&[0x80, 0x3D]); // CMP BYTE [RIP + disp32], 0
        b.extend_from_slice(&disp.to_le_bytes());
        b.push(0x00);
    } else {
        mov_r11(&mut b, plan.pre);
        b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    }
    decline.push(jcc(&mut b, 0x85)); // JNE -> decline
    b.extend_from_slice(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    let mut plain = vec![jcc(&mut b, 0x84)]; // JZ -> plain
    let aligned = g.arena_base & ((1usize << g.region_shift) - 1) == 0;
    let holder_in_r10 = young.is_some() || !aligned;
    if aligned {
        b.extend_from_slice(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
        b.extend_from_slice(&[0x48, 0x31, 0xC1]); // XOR RCX, RAX
        b.extend_from_slice(&[0x48, 0xC1, 0xE9, g.region_shift]); // SHR RCX, region_shift
        if young.is_some() {
            plain.push(jcc(&mut b, 0x84)); // JZ -> plain
            mov_r11(&mut b, g.arena_base);
            b.extend_from_slice(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
            b.extend_from_slice(&[0x4D, 0x29, 0xDA]); // SUB R10, R11 (holder - base)
        }
    } else {
        mov_r11(&mut b, g.arena_base);
        b.extend_from_slice(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
        b.extend_from_slice(&[0x4D, 0x29, 0xDA]); // SUB R10, R11 (holder - base)
        b.extend_from_slice(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
        b.extend_from_slice(&[0x4C, 0x29, 0xD9]); // SUB RCX, R11 (value - base)
        b.extend_from_slice(&[0x4C, 0x31, 0xD1]); // XOR RCX, R10
        b.extend_from_slice(&[0x48, 0xC1, 0xE9, g.region_shift]); // SHR RCX, region_shift
        if young.is_some() {
            plain.push(jcc(&mut b, 0x84)); // JZ -> plain
        }
    }
    match young {
        Some(y) => {
            b.extend_from_slice(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
            b.extend_from_slice(&[0x49, 0xC1, 0xEB, g.region_shift]); // SHR R11, region_shift
            b.extend_from_slice(&[0x49, 0x81, 0xFB]); // CMP R11, imm32 (region count)
            b.extend_from_slice(&y.count.to_le_bytes());
            cross.push(jcc(&mut b, 0x83)); // JAE -> cross (outside the regions)
            b.extend_from_slice(&[0x4D, 0x69, 0xDB]); // IMUL R11, R11, imm32 (stride)
            b.extend_from_slice(&y.stride.to_le_bytes());
            b.extend_from_slice(&[0x48, 0xB9]); // MOV RCX, imm64 (regions)
            b.extend_from_slice(&(y.regions as u64).to_le_bytes()); // Cast: an address
            b.extend_from_slice(&[0x45, 0x0F, 0xB6, 0x9C, 0x0B]); // MOVZX R11D, byte [R11 + RCX + disp32]
            b.extend_from_slice(&y.type_offset.to_le_bytes());
            b.extend_from_slice(&[0x41, 0x83, 0xFB, y.young_max]); // CMP R11D, imm8
            cross.push(jcc(&mut b, 0x87)); // JA -> cross (not young)
        }
        None => cross.push(jcc(&mut b, 0x85)), // JNZ -> cross
    }
    let end = b.len();
    for p in plain {
        patch_rel32(&mut b, p, end);
    }
    G1AastoreScreen {
        bytes: b,
        decline,
        cross,
        holder_in_r10,
    }
}

/// The filter's bytes, for `RAX = holder` (an object start, non-null) and
/// `RDX = value`, after the caller's same-region / null screens sent the store
/// to its barrier. Writes `RAX, RCX, RDX, R10, R11` and the flags; reads
/// memory only.
pub(crate) fn g1_edge_filter_hit_bytes(g: &G1EdgeFilterGeom) -> (Vec<u8>, Vec<usize>) {
    g1_edge_filter_hit_bytes_impl(g, None, false)
}

/// [`g1_edge_filter_hit_bytes`] with the RIP-relative epoch-mirror load when
/// `here` is in reach: the cross edge of a screen that left `R10` unset.
pub(crate) fn g1_edge_filter_hit_bytes_rip(
    g: &G1EdgeFilterGeom,
    here: usize,
) -> (Vec<u8>, Vec<usize>) {
    g1_edge_filter_hit_bytes_impl(g, Some(here), false)
}

/// Offset-preserving twin of [`g1_edge_filter_hit_bytes`]: `R10` already holds
/// `holder - g.arena_base` from [`g1_aastore_screen_with_geom`], `RAX` the
/// holder and `RDX` the value, so `value - base` is `RDX - RAX + R10` with no
/// second `MOV R11, arena_base`; `here` enables RIP-relative epoch-mirror load
/// when in reach.
pub(crate) fn g1_edge_filter_hit_bytes_from_offsets(
    g: &G1EdgeFilterGeom,
    here: usize,
) -> (Vec<u8>, Vec<usize>) {
    g1_edge_filter_hit_bytes_impl(g, Some(here), true)
}

fn g1_edge_filter_hit_bytes_impl(
    g: &G1EdgeFilterGeom,
    here: Option<usize>,
    holder_in_r10: bool,
) -> (Vec<u8>, Vec<usize>) {
    let mut b: Vec<u8> = Vec::with_capacity(128);
    let mut miss: Vec<usize> = Vec::new();
    // RCX = holder - base, RDX = value - base.
    if holder_in_r10 {
        b.extend_from_slice(&[0x48, 0x29, 0xC2]); // SUB RDX, RAX (value - holder)
        b.extend_from_slice(&[0x4C, 0x01, 0xD2]); // ADD RDX, R10 (value - base)
        b.extend_from_slice(&[0x4C, 0x89, 0xD1]); // MOV RCX, R10
    } else {
        mov_r11(&mut b, g.arena_base);
        b.extend_from_slice(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
        b.extend_from_slice(&[0x4C, 0x29, 0xD9]); // SUB RCX, R11
        b.extend_from_slice(&[0x4C, 0x29, 0xDA]); // SUB RDX, R11
    }
    // Both inside the regions (unsigned: below the base wraps high). For a
    // power-of-two span one test covers both: `(h | v) < L` iff `h < L` and
    // `v < L` (RAX is free: the block offset below overwrites it).
    if g.regions_len.is_power_of_two() {
        b.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        b.extend_from_slice(&[0x48, 0x09, 0xD0]); // OR RAX, RDX
        if let Ok(len32) = i32::try_from(g.regions_len) {
            b.extend_from_slice(&[0x48, 0x3D]); // CMP RAX, imm32
            b.extend_from_slice(&len32.to_le_bytes());
            miss.push(jcc(&mut b, 0x83)); // JAE -> miss
        } else {
            let log2 = u8::try_from(g.regions_len.trailing_zeros()).unwrap_or(63);
            b.extend_from_slice(&[0x48, 0xC1, 0xE8, log2]); // SHR RAX, log2
            miss.push(jcc(&mut b, 0x85)); // JNZ -> miss
        }
    } else if let Ok(len32) = i32::try_from(g.regions_len) {
        b.extend_from_slice(&[0x48, 0x81, 0xF9]); // CMP RCX, imm32
        b.extend_from_slice(&len32.to_le_bytes());
        miss.push(jcc(&mut b, 0x83)); // JAE -> miss
        b.extend_from_slice(&[0x48, 0x81, 0xFA]); // CMP RDX, imm32
        b.extend_from_slice(&len32.to_le_bytes());
        miss.push(jcc(&mut b, 0x83)); // JAE -> miss
    } else {
        mov_r11(&mut b, g.regions_len);
        b.extend_from_slice(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
        miss.push(jcc(&mut b, 0x83)); // JAE -> miss
        b.extend_from_slice(&[0x4C, 0x39, 0xDA]); // CMP RDX, R11
        miss.push(jcc(&mut b, 0x83)); // JAE -> miss
    }
    // EAX = the holder's block offset in its card (grain units), RCX = card.
    let card_mask: u32 = (1u32 << g.card_shift) - 1;
    b.extend_from_slice(&[0x89, 0xC8]); // MOV EAX, ECX
    b.push(0x25); // AND EAX, imm32
    b.extend_from_slice(&card_mask.to_le_bytes());
    b.extend_from_slice(&[0xC1, 0xE8, g.block_grain_shift]); // SHR EAX, grain
    b.extend_from_slice(&[0x48, 0xC1, 0xE9, g.card_shift]); // SHR RCX, card_shift
                                                            // The card must already be dirty.
    mov_r11(&mut b, g.cards_base);
    b.extend_from_slice(&[0x41, 0x80, 0x3C, 0x0B, g.card_dirty]); // CMP byte [R11+RCX], dirty
    miss.push(jcc(&mut b, 0x85)); // JNE -> miss
                                  // Its block offset must already be at or below the holder's (preserve RDX).
    mov_r11(&mut b, g.starts_base);
    b.extend_from_slice(&[0x45, 0x0F, 0xB6, 0x14, 0x0B]); // MOVZX R10D, byte [R11+RCX]
    b.extend_from_slice(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
    miss.push(jcc(&mut b, 0x87)); // JA -> miss (the barrier lowers it)
                                  // RDX = destination region, RCX = source region.
    b.extend_from_slice(&[0x48, 0xC1, 0xEA, g.region_shift]); // SHR RDX, region_shift
    let to_region = g.region_shift - g.card_shift;
    if to_region != 0 {
        b.extend_from_slice(&[0x48, 0xC1, 0xE9, to_region]); // SHR RCX, imm8
    }
    // RAX = epoch << 32 | (src + 1), the collector's `G1JitEdgeFilter::key`.
    if let Some(disp) = here.and_then(|h| rip_disp32(h + b.len(), 7, g.filter)) {
        b.extend_from_slice(&[0x48, 0x8B, 0x05]); // MOV RAX, [RIP + disp32]
        b.extend_from_slice(&disp.to_le_bytes());
    } else {
        mov_r11(&mut b, g.filter);
        b.extend_from_slice(&[0x49, 0x8B, 0x03]); // MOV RAX, [R11] (the epoch mirror)
    }
    b.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x20]); // SHL RAX, 32
    b.extend_from_slice(&[0x48, 0x8D, 0x44, 0x08, 0x01]); // LEA RAX, [RAX+RCX+1]
    mov_r11(&mut b, g.entries);
    b.extend_from_slice(&[0x49, 0x3B, 0x04, 0xD3]); // CMP RAX, [R11+RDX*8]
    miss.push(jcc(&mut b, 0x85)); // JNE -> miss
    (b, miss)
}

/// Executed bytes against a fake collector layout: 1 KiB regions, 512-byte
/// cards, a 4 KiB arena (4 regions, 8 cards).
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_vhcae_g1_edge_filter_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod w6_g1_aastore_screen_geom_tests;
