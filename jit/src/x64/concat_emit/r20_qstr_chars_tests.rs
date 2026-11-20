use super::*;

#[cfg(windows)]
const A0: u8 = 1; // RCX
#[cfg(windows)]
const A1: u8 = 2; // RDX
#[cfg(not(windows))]
const A0: u8 = 7; // RDI
#[cfg(not(windows))]
const A1: u8 = 6; // RSI

const STRING_CID: u32 = 77;
const TOTAL: i32 = 32;
const VALUE_OFF: i32 = 16;
const CODER: (i32, bool) = (24, true);
const SSE_ARMS: [Option<&str>; 2] = [None, Some("0")];

fn words(raw: &[u8]) -> Vec<u64> {
    let mut w = vec![0u64; raw.len().div_ceil(8)];
    for (i, b) in raw.iter().enumerate() {
        w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
    }
    w
}

fn raw(w: &[u64]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn u16le(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// A fake primitive array of kind `desc` with header length `len` and
/// payload `payload`.
fn array(desc: &str, len: u32, payload: &[u8]) -> Vec<u64> {
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut r = vec![0u8; data + payload.len() + 8];
    r[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
        cratonvm_types::primitive_array_kind_tags_byte(desc).expect("array tag");
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    r[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    r[data..data + payload.len()].copy_from_slice(payload);
    words(&r)
}

fn char_mark() -> i32 {
    use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
    let m = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Char,
        0,
        0,
    )
    .mark_word
    .load(std::sync::atomic::Ordering::Relaxed);
    i32::from_le_bytes(m.to_le_bytes())
}

fn site(flag: &u8, guard: &u32, guard_value: u32) -> CharsIrSite {
    CharsIrSite {
        alloc: SbToStringPlan::for_test(
            STRING_CID,
            TOTAL,
            VALUE_OFF,
            // Cast: the guard word's address.
            (guard as *const u32 as usize, guard_value),
        ),
        gc: SbToStringGc {
            zgc_start_bits: false,
            g1_same_region: false,
        },
        thread: ConcatThread::Frame(8),
        cursor_off: 8,
        end_off: 16,
        g1_table: 0,
        class_id_off: 0,
        // Cast: the flag's address.
        compact_flag_addr: flag as *const u8 as usize,
        young_bounds_addr: 0,
        coder: CODER,
        kind_d8: u8::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET).expect("disp8"),
        byte_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B"),
        char_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C"),
        char_array_mark: char_mark(),
    }
}

/// Emit `body` as `extern "C" fn(rbp, a1) -> i64` (RBP = `rbp`, R9 = `a1`
/// on entry), every `decline` edge answering `-1`, and run it.
fn run(body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>), rbp: u64, a1: u64) -> i64 {
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    mov_r64_r64(&mut buf, R9, A1);
    let mut decline = Vec::new();
    body(&mut buf, &mut decline);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffers (alive across the
    // call) and caller-saved registers; RBP is saved and restored.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    f(rbp, a1)
}

/// A TLAB of `tlab_words`, a thread over it, and a frame whose `[rbp-8]`
/// is that thread; `go(rbp)` runs the tail. Answers the tail's RAX, the
/// TLAB's bytes, the TLAB's base and how far the cursor moved.
fn with_tlab(tlab_words: usize, go: impl Fn(u64) -> i64) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    thread[2] = base + 8 * tlab_words as u64; // Cast: a small count
                                              // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let r = go(rbp);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

fn header_u32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(raw[at..at + 4].try_into().expect("4 bytes"))
}





#[test]
fn the_young_receiver_check_reads_both_semispaces() {
    let table: [u64; 6] = [0x1000, 0x2000, 0x8000, 0x9000, 0x20000, 0x30000];
    // Cast: the fake table's address.
    let t = table.as_ptr() as usize;
    let check = |addr: u64| {
        run(
            |b, d| {
                emit_young_receiver_check(b, t, d);
                mov_imm32_sx(b, RAX, 1);
            },
            0,
            addr,
        )
    };
    let cases: [(u64, i64); 9] = [
        (0x1000, 1),
        (0x1FF8, 1),
        (0x8000, 1),
        (0x8FF8, 1),
        (0x0FF8, -1),
        (0x2000, -1),
        (0x5000, -1),
        (0x9000, -1),
        (0x20010, -1),
    ];
    for (addr, want) in cases {
        assert_eq!(check(addr), want, "{addr:#x}");
    }
}
