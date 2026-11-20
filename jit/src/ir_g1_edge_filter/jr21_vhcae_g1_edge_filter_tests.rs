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
