use super::*;

/// The fake `JvmThread`: its `Tlab` at offset 0
/// (`cursor, end, start, zgc_owned_epoch, zgc_announce`).
#[repr(C)]
#[allow(dead_code)] // read by the emitted code, not by Rust
struct FakeThread {
    cursor: u64,
    end: u64,
    start: u64,
    owned_epoch: u64,
    announce: u64,
}

/// The fake `JitZgcAnnounceTable` (`owner` is never read by the JIT).
#[repr(C)]
#[allow(dead_code)] // read by the emitted code, not by Rust
struct FakeTable {
    words: u64,
    base: u64,
    span: u64,
    blocked: u64,
    epoch: u64,
    owner: u64,
}

/// Emit the pre-check for two objects (R11, R9), then both stores, run it
/// with R10 = `thread`, and return 1 on the store arm, 0 on a decline.
fn run(thread: &mut FakeThread, a: u64, b: u64) -> i64 {
    run_counted(thread, a, b, false)
}

/// [`run`], with the pre-check's run-time decline counters armed or not
/// (round 17 wave 6, lane sbzgc2).
fn run_counted(thread: &mut FakeThread, a: u64, b: u64, count: bool) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    // R10 / R11 are no argument register in either ABI; R9 is written
    // last because it is Win64's fourth argument register.
    c.emit_mov_r64_r64(R10, ARG_REGS[0]);
    c.emit_mov_r64_r64(R11, ARG_REGS[1]);
    c.emit_mov_r64_r64(R9, ARG_REGS[2]);
    let mut decline: Vec<usize> = Vec::new();
    c.emit_zgc_start_bit_precheck(0, 8, &[R11, R9], &mut decline, count);
    c.emit_zgc_start_bit_store_prechecked(0, R11);
    c.emit_zgc_start_bit_store_prechecked(0, R9);
    c.emit_mov_imm32_sx(RAX, 1);
    c.buf.emit(&[0xC3]); // ret
    for p in decline {
        c.patch_rel32_to_here(p);
    }
    c.emit_xor_reg_self(RAX);
    c.buf.emit(&[0xC3]); // ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());

    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: a leaf snippet that touches no stack, reads the fake thread
    // and the fake table it names, writes only the bitmap they name (all
    // alive across the call), calls nothing, and returns an i64. `c` owns
    // the buffer and outlives the call.
    let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    // Cast: addresses as the i64 arguments the snippet takes.
    let r = f(thread as *mut FakeThread as i64, a as i64, b as i64);
    drop(c);
    r
}

/// One way to break (or keep) the contract: `(thread, table, heap base)`.
type Edit = fn(&mut FakeThread, &mut FakeTable, u64);

fn keep(_: &mut FakeThread, _: &mut FakeTable, _: u64) {}
fn unarm(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
    t.announce = 0;
}
fn stale_epoch(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
    t.owned_epoch -= 1;
}
fn marking(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.blocked = 1;
}
fn unpublished(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.words = 0;
}
fn below_base(_: &mut FakeThread, tb: &mut FakeTable, base: u64) {
    tb.base = base + 1024;
}
fn beyond_span(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.span = 520;
}
fn word_starts_before_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
    t.start = base + 516;
}
fn first_word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
    t.end = base + 1000;
}
/// Only the SECOND object's word (1024..1536) leaves the chunk: the
/// pre-check must ask about every object, not only the first.
fn second_word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
    t.end = base + 1500;
}

/// Both objects' start bits are set with every condition met (same word
/// and different words); each condition broken on its own -- including one
/// that only the second object fails, and an off-grid second object --
/// declines with the bitmap untouched.
#[test]
fn both_start_bits_are_set_or_the_site_declines_with_the_bitmap_untouched() {
    // a at heap offset 520: bit 65 = word 1, bit 1.
    const A: u64 = 520;
    let cases: [(&str, u64, Option<(usize, u64, usize, u64)>, Edit); 12] = [
        // b at 560: bit 70 = word 1, bit 6.
        ("armed, same word", 560, Some((1, 1 << 1, 1, 1 << 6)), keep),
        // b at 1104: bit 138 = word 2, bit 10.
        (
            "armed, next word",
            1104,
            Some((1, 1 << 1, 2, 1 << 10)),
            keep,
        ),
        ("unarmed buffer", 560, None, unarm),
        ("stale epoch", 560, None, stale_epoch),
        ("marking", 560, None, marking),
        ("unpublished", 560, None, unpublished),
        ("below base", 560, None, below_base),
        ("beyond span", 560, None, beyond_span),
        (
            "word starts before the chunk",
            560,
            None,
            word_starts_before_chunk,
        ),
        (
            "first word ends after the chunk",
            560,
            None,
            first_word_ends_after_chunk,
        ),
        (
            "second word ends after the chunk",
            1104,
            None,
            second_word_ends_after_chunk,
        ),
        ("second object off the grid", 1101, None, keep),
    ];
    for (name, b_off, expect, edit) in cases {
        let mut heap = vec![0u64; 256]; // 2 KiB of "arena"
        let base = heap.as_mut_ptr() as u64;
        let mut bits = vec![0u64; 8];
        let mut table = FakeTable {
            words: bits.as_mut_ptr() as u64,
            base,
            span: 2048,
            blocked: 0,
            epoch: 41,
            owner: base,
        };
        let mut thread = FakeThread {
            cursor: base + A,
            end: base + 2048,
            start: base,
            owned_epoch: 41,
            announce: 1,
        };
        edit(&mut thread, &mut table, base);
        if thread.announce != 0 {
            thread.announce = &table as *const FakeTable as u64;
        }

        let r = run(&mut thread, base + A, base + b_off);

        match expect {
            Some((wa, ba, wb, bb)) => {
                assert_eq!(r, 1, "{name}: the store arm");
                let mut want = [0u64; 8];
                want[wa] |= ba;
                want[wb] |= bb;
                assert_eq!(
                    bits.as_slice(),
                    &want[..],
                    "{name}: exactly both start bits"
                );
            }
            None => {
                assert_eq!(r, 0, "{name}: declined");
                assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
            }
        }
        assert_eq!(
            thread.cursor,
            base + A,
            "{name}: the pair never moves the cursor"
        );
        drop(bits);
        drop(heap);
    }
}

/// Round 17 wave 6, lane sbzgc2: an uncounted decline edge is the plain
/// `Jcc rel32` it always was; a counted one inverts the condition over the
/// `MOV R8, imm64 ; LOCK INC [R8] ; JMP` stub.
#[test]
fn a_counted_decline_edge_inverts_over_its_stub_and_an_uncounted_one_is_unchanged() {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let start = c.buf.pos();
    let mut decline: Vec<usize> = Vec::new();
    c.emit_decline_jcc_counted(0x85, None, &mut decline);
    assert_eq!(c.buf.pos() - start, 6, "0F 85 rel32");
    assert_eq!(decline, vec![start + 2]);
    let counter = crate::metrics::SB_TO_STRING_CENSUS.get(crate::metrics::SB_TS_ENTERED);
    let at = c.buf.pos();
    c.emit_decline_jcc_counted(0x85, counter, &mut decline);
    // JE stay (6) + MOV R8,imm64 (10) + LOCK INC [R8] (4) + JMP rel32 (5).
    assert_eq!(c.buf.pos() - at, 25);
    assert_eq!(decline.len(), 2);
    assert_eq!(decline[1], at + 21, "the stub's JMP is the decline edge");
    // SAFETY: `at + 1` and `at + 16..at + 20` are inside the bytes just
    // emitted into the live buffer.
    let (cc, inc) = unsafe {
        let p = c.buf.as_ptr();
        (
            *p.add(at + 1),
            std::slice::from_raw_parts(p.add(at + 16), 4).to_vec(),
        )
    };
    assert_eq!(cc, 0x84, "the condition is inverted");
    assert_eq!(inc, vec![0xF0, 0x49, 0xFF, 0x00]);
    assert!(!c.buf.overflowed());
}

/// Round 17 wave 6, lane sbzgc2 (SBZGC17-5): with the counters armed the
/// pre-check answers exactly as before, and each decline lands in the
/// census row of its own cause. The rows are process-wide, so the test
/// asserts that its row grew, never an exact value.
#[test]
fn counted_declines_keep_the_answer_and_land_in_their_cause_row() {
    use crate::metrics as m;
    const A: u64 = 520;
    let cases: [(&str, u64, Option<usize>, Edit); 11] = [
        ("armed, same word", 560, None, keep),
        ("unarmed buffer", 560, Some(m::SB_TS_ZGC_NO_ANNOUNCE), unarm),
        ("stale epoch", 560, Some(m::SB_TS_ZGC_EPOCH), stale_epoch),
        ("marking", 560, Some(m::SB_TS_ZGC_BLOCKED), marking),
        (
            "unpublished",
            560,
            Some(m::SB_TS_ZGC_NO_BITMAP),
            unpublished,
        ),
        (
            "below base",
            560,
            Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
            below_base,
        ),
        (
            "beyond span",
            560,
            Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
            beyond_span,
        ),
        (
            "word starts before the chunk",
            560,
            Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
            word_starts_before_chunk,
        ),
        (
            "second word ends after the chunk",
            1104,
            Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
            second_word_ends_after_chunk,
        ),
        (
            "second object off the grid",
            1101,
            Some(m::SB_TS_ZGC_OUTSIDE_ARENA),
            keep,
        ),
        (
            "first word ends after the chunk",
            560,
            Some(m::SB_TS_ZGC_WORD_OUTSIDE_TLAB),
            first_word_ends_after_chunk,
        ),
    ];
    let read = |r: usize| {
        m::SB_TO_STRING_CENSUS
            .get(r)
            .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    for (name, b_off, cause, edit) in cases {
        let mut heap = vec![0u64; 256];
        let base = heap.as_mut_ptr() as u64;
        let mut bits = vec![0u64; 8];
        let mut table = FakeTable {
            words: bits.as_mut_ptr() as u64,
            base,
            span: 2048,
            blocked: 0,
            epoch: 41,
            owner: base,
        };
        let mut thread = FakeThread {
            cursor: base + A,
            end: base + 2048,
            start: base,
            owned_epoch: 41,
            announce: 1,
        };
        edit(&mut thread, &mut table, base);
        if thread.announce != 0 {
            thread.announce = &table as *const FakeTable as u64;
        }
        let before = cause.map(read);
        let r = run_counted(&mut thread, base + A, base + b_off, true);
        match cause {
            None => {
                assert_eq!(r, 1, "{name}: the store arm");
                assert!(bits.iter().any(|w| *w != 0), "{name}: the bits are set");
            }
            Some(row) => {
                assert_eq!(r, 0, "{name}: declined");
                assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
                assert!(
                    read(row) > before.unwrap_or(0),
                    "{name}: the decline is counted in row {}",
                    m::SB_TO_STRING_CENSUS_NAMES[row]
                );
            }
        }
        drop(bits);
        drop(heap);
    }
}
