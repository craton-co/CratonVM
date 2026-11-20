// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::{
    g1_aastore_screen_with_geom, g1_aastore_screen_with_young,
    g1_edge_filter_hit_bytes_from_offsets, g1_edge_filter_hit_bytes_rip, G1EdgeFilterGeom,
    G1OwnedYoung, W_COUNT,
};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[cfg(windows)]
const ARG0: u8 = 1; // RCX
#[cfg(windows)]
const ARG1: u8 = 2; // RDX
#[cfg(not(windows))]
const ARG0: u8 = 7; // RDI
#[cfg(not(windows))]
const ARG1: u8 = 6; // RSI

/// Region-aligned (1 KiB regions) backing store with room for a skewed base.
#[repr(C, align(1024))]
struct Arena([u8; 8192]);

/// A fake G1 region array: 4 regions, 16-byte stride, type byte at +8.
#[repr(C, align(16))]
struct Regions([u8; 64]);

const STRIDE: usize = 16;
const TYPE_OFFSET: usize = 8;
const TYPE_EDEN: u8 = 0;
const TYPE_OLD: u8 = 2;

fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

/// The collector side of one test: arena, card / start / entry tables, the
/// filter words, the SATB gate.
struct Fixture {
    _arena: Box<Arena>,
    cards: Box<[AtomicU8; 8]>,
    starts: Box<[AtomicU8; 8]>,
    entries: Box<[AtomicU64; 4]>,
    _words: Box<[AtomicU64; W_COUNT]>,
    gate: Box<AtomicU8>,
    base: usize,
    g: G1EdgeFilterGeom,
}

fn fixture(skew: usize) -> Fixture {
    let arena = Box::new(Arena([0; 8192]));
    let cards: Box<[AtomicU8; 8]> = Box::new(std::array::from_fn(|_| AtomicU8::new(0)));
    let starts: Box<[AtomicU8; 8]> = Box::new(std::array::from_fn(|_| AtomicU8::new(0xFF)));
    let entries: Box<[AtomicU64; 4]> = Box::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let mut words: Box<[AtomicU64; W_COUNT]> = Box::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let base = arena.0.as_ptr() as usize + skew;
    let w = [
        0usize,
        base,
        4 << 10,
        10,
        cards.as_ptr() as usize,
        starts.as_ptr() as usize,
        entries.as_ptr() as usize,
        4,
        cratonvm_types::CARD_SHIFT as usize,
        3,
        1,
    ];
    for (cell, v) in words.iter_mut().zip(w) {
        *cell = AtomicU64::new(v as u64);
    }
    let g = G1EdgeFilterGeom::from_words(words.as_ptr() as usize, &w).expect("geom");
    Fixture {
        _arena: arena,
        cards,
        starts,
        entries,
        _words: words,
        gate: Box::new(AtomicU8::new(0)),
        base,
        g,
    }
}

fn young_of(regions: &Regions) -> G1OwnedYoung {
    G1OwnedYoung {
        regions: regions.0.as_ptr() as usize,
        count: 4,
        stride: STRIDE as i32,
        type_offset: TYPE_OFFSET as i32,
        young_max: 1,
    }
}

/// `extern "C" fn(holder, value) -> i64`: 0 = plain, 1 = cross + edge-filter
/// hit, 2 = cross + edge-filter miss, -1 = decline. Also returns the screen
/// and edge-filter bytes for shape checks.
fn build(fx: &Fixture, young: Option<G1OwnedYoung>) -> (crate::ExecutableBuffer, Vec<u8>, Vec<u8>) {
    let plan = crate::x64::G1AastorePlan {
        pre: (&*fx.gate as *const AtomicU8) as usize,
        table: 0,
        post: 1,
    };
    let mut buf = crate::ExecutableBuffer::new(2048).expect("executable buffer");
    buf.emit(&mov(0, ARG0));
    buf.emit(&mov(2, ARG1));
    let screen_at = buf.pos();
    let s = g1_aastore_screen_with_young(plan, &fx.g, buf.as_ptr() as usize + screen_at, young);
    buf.emit(&s.bytes);
    buf.emit(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET (plain = 0)
    let cross_at = buf.pos();
    // Simulate gp_load_index_rcx clobbering RCX while leaving RAX, RDX, R10 intact.
    buf.emit(&[0xB9, 0x2A, 0x00, 0x00, 0x00]); // MOV ECX, 42
    let ef_at = buf.pos();
    let here = buf.as_ptr() as usize + ef_at;
    let (ef, misses) = if s.holder_in_r10 {
        g1_edge_filter_hit_bytes_from_offsets(&fx.g, here)
    } else {
        g1_edge_filter_hit_bytes_rip(&fx.g, here)
    };
    buf.emit(&ef);
    buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET (hit = 1)
    let miss_at = buf.pos();
    buf.emit(&[0xB8, 0x02, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 2 ; RET (miss = 2)
    let decline_at = buf.pos();
    buf.emit(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let patch = |buf: &mut crate::ExecutableBuffer, field: usize, to: usize| {
        let rel = i32::try_from(to).expect("small") - i32::try_from(field + 4).expect("small");
        assert!(buf.try_patch_i32(field, rel).is_ok());
    };
    for d in &s.decline {
        patch(&mut buf, screen_at + d, decline_at);
    }
    for c in &s.cross {
        patch(&mut buf, screen_at + c, cross_at);
    }
    for m in misses {
        patch(&mut buf, ef_at + m, miss_at);
    }
    assert!(!buf.overflowed());
    buf.finalize();
    (buf, s.bytes, ef)
}

fn call(buf: &crate::ExecutableBuffer, holder: usize, value: usize) -> i64 {
    // SAFETY: self-contained extern "C" fn touching only caller-saved regs and live test tables.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr()) };
    f(holder as u64, value as u64)
}

fn count(code: &[u8], needle: &[u8]) -> usize {
    code.windows(needle.len()).filter(|w| *w == needle).count()
}

/// The answers every configuration must give for a holder in region 0 (card
/// 1, offset 0xC0) and a value in region 2, with a non-young holder.
fn assert_cross_region_answers(fx: &Fixture, buf: &crate::ExecutableBuffer) {
    let base = fx.base;
    let holder = base + 512 + 0xC0;
    let value = base + 0x0800 + 0x40;
    assert_eq!(call(buf, holder, 0), 0, "null -> plain");
    assert_eq!(
        call(buf, holder, base + 512 + 0x10),
        0,
        "same region -> plain"
    );
    assert_eq!(call(buf, holder, value), 2, "unrecorded cross -> miss");

    // Record card + start + edge for region 0 -> region 2.
    fx.cards[1].store(1, Ordering::Relaxed);
    fx.starts[1].store((0xC0 >> 3) as u8, Ordering::Relaxed);
    fx.entries[2].store(0 << 32 | 1, Ordering::Relaxed);
    assert_eq!(call(buf, holder, value), 1, "recorded cross -> hit");
    assert_eq!(call(buf, holder - 0x40, value), 2, "lower holder -> miss");
    assert_eq!(
        call(buf, holder, base + 4096),
        2,
        "out-of-arena value -> miss"
    );
    assert_eq!(
        call(buf, base - 64, value),
        2,
        "out-of-arena holder -> miss"
    );

    fx.gate.store(1, Ordering::Relaxed);
    assert_eq!(call(buf, holder, value), -1, "armed SATB gate -> decline");
    fx.gate.store(0, Ordering::Relaxed);
}

#[test]
fn screen_and_offset_edge_filter_execute_end_to_end() {
    // Region-aligned base, no young table: the absolute XOR screen, the
    // edge filter forming both offsets itself.
    let fx = fixture(0);
    let (buf, screen, ef) = build(&fx, None);
    assert_cross_region_answers(&fx, &buf);
    let mut base_imm = vec![0x49, 0xBB];
    base_imm.extend_from_slice(&(fx.base as u64).to_le_bytes());
    assert_eq!(
        count(&screen, &base_imm),
        0,
        "aligned base: no arena_base in the screen"
    );
    assert!(
        screen
            .windows(10)
            .any(|w| w == [0x48, 0x89, 0xD1, 0x48, 0x31, 0xC1, 0x48, 0xC1, 0xE9, 10]),
        "MOV RCX, RDX ; XOR RCX, RAX ; SHR RCX, region_shift"
    );
    assert_eq!(count(&ef, &base_imm), 1);
}

#[test]
fn owned_young_table_skips_young_holders_with_one_arena_base_load() {
    let fx = fixture(0);
    let mut regions = Box::new(Regions([0; 64]));
    for r in 0..4 {
        regions.0[r * STRIDE + TYPE_OFFSET] = TYPE_OLD;
    }
    let young = young_of(&regions);
    let (buf, screen, ef) = build(&fx, Some(young));
    assert_cross_region_answers(&fx, &buf);

    // The holder's region retyped young (Eden): no barrier at all.
    regions.0[TYPE_OFFSET] = TYPE_EDEN;
    let holder = fx.base + 512 + 0xC0;
    let value = fx.base + 0x0800 + 0x40;
    fx.cards[1].store(0, Ordering::Relaxed);
    assert_eq!(call(&buf, holder, value), 0, "young holder -> plain");
    regions.0[TYPE_OFFSET] = TYPE_OLD;
    assert_eq!(
        call(&buf, holder, value),
        2,
        "old holder, clean card -> miss"
    );

    // Shape: one arena_base load across screen + filter, no run-time liveness
    // compare, and the young test is the screen's LAST jump (`JA cross`), so
    // the plain store follows it with no `JBE` + `JMP` around it.
    let mut base_imm = vec![0x49, 0xBB];
    base_imm.extend_from_slice(&(fx.base as u64).to_le_bytes());
    assert_eq!(count(&screen, &base_imm) + count(&ef, &base_imm), 1);
    assert_eq!(
        count(&screen, &[0x48, 0x81, 0x79, 24]),
        0,
        "no CMP [table + 24], shift"
    );
    let tail = &screen[screen.len() - 10..screen.len() - 4];
    assert_eq!(
        tail,
        &[0x41, 0x83, 0xFB, 1, 0x0F, 0x87],
        "CMP R11D, young_max ; JA -> cross ends the screen"
    );
    assert_eq!(
        &ef[..9],
        &[0x48, 0x29, 0xC2, 0x4C, 0x01, 0xD2, 0x4C, 0x89, 0xD1],
        "value offset as RDX - RAX + R10"
    );
    assert!(
        ef.windows(8)
            .any(|w| w == [0x48, 0x89, 0xC8, 0x48, 0x09, 0xD0, 0x48, 0x3D]),
        "a power-of-two region span tests both offsets with one OR + CMP"
    );
}

#[test]
fn unaligned_arena_base_keeps_the_offset_xor() {
    // Base 512 bytes past a region boundary: `(h ^ v) >> shift` would call
    // base + 0x3F0 and base + 0x410 one region; their offsets are not.
    let fx = fixture(512);
    let (buf, screen, _) = build(&fx, None);
    let mut base_imm = vec![0x49, 0xBB];
    base_imm.extend_from_slice(&(fx.base as u64).to_le_bytes());
    assert_eq!(count(&screen, &base_imm), 1);
    assert_eq!(
        call(&buf, fx.base + 0x3F0, fx.base + 0x410),
        2,
        "regions 0 and 1 -> cross"
    );
    assert_cross_region_answers(&fx, &buf);
}

#[test]
fn owned_young_table_is_refused_for_another_collector() {
    use cratonvm_types::jit_g1_young::{
        W_BASE, W_COUNT as Y_COUNT, W_REGIONS, W_SHIFT, W_STRIDE, W_TYPE_OFFSET, W_YOUNG_MAX,
    };
    let fx = fixture(0);
    let regions = Box::new(Regions([0; 64]));
    let mut w = [0usize; 8];
    w[W_REGIONS] = regions.0.as_ptr() as usize;
    w[W_BASE] = fx.base;
    w[W_SHIFT] = 10;
    w[Y_COUNT] = 4;
    w[W_STRIDE] = STRIDE;
    w[W_TYPE_OFFSET] = TYPE_OFFSET;
    w[W_YOUNG_MAX] = 1;
    assert_eq!(
        G1OwnedYoung::from_table(&fx.g, &w),
        Some(young_of(&regions))
    );
    for (i, v) in [
        (W_BASE, fx.base + 4096),
        (W_SHIFT, 0),
        (W_SHIFT, 11),
        (Y_COUNT, 3),
        (W_REGIONS, 0),
        (W_TYPE_OFFSET, STRIDE),
        (W_YOUNG_MAX, 0x80),
    ] {
        let mut bad = w;
        bad[i] = v;
        assert_eq!(
            G1OwnedYoung::from_table(&fx.g, &bad),
            None,
            "word {i} = {v:#x}"
        );
    }
}

#[test]
fn live_screen_without_a_published_table_has_no_young_test() {
    // The process table is unpublished in unit tests unless a G1 heap claims
    // it; with no owner matching this fake arena the screen never bakes one.
    let fx = fixture(0);
    let plan = crate::x64::G1AastorePlan {
        pre: (&*fx.gate as *const AtomicU8) as usize,
        table: 0,
        post: 1,
    };
    let s = g1_aastore_screen_with_geom(plan, &fx.g, 0);
    assert!(!s.holder_in_r10);
    let n = s.bytes.len();
    assert_eq!(
        &s.bytes[n - 6..n - 4],
        &[0x0F, 0x85],
        "SHR ; JNZ -> cross ends the screen"
    );
}
