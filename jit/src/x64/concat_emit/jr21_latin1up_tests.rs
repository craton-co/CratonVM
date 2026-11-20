use super::*;

#[cfg(windows)]
const A0: u8 = 1; // RCX
#[cfg(not(windows))]
const A0: u8 = 7; // RDI

const STRING_CID: u32 = 78;
const TOTAL: i32 = 32;
const VALUE_OFF: i32 = 16;
const CODER: (i32, bool) = (24, true);

fn raw(w: &[u64]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
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
        char_array_mark: 0,
    }
}

/// The UTF16 allocation over a TLAB of `tlab_words` with RCX = `bytes`
/// (64-bit): answers RAX (-1 on a decline), the TLAB's bytes, its base and
/// how far the cursor moved.
fn alloc(s: &CharsIrSite, bytes: u64, tlab_words: usize) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0xA5A5_A5A5_A5A5_A5A5u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    // Cast: a small count.
    thread[2] = base + 8 * tlab_words as u64;
    // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    let mut decline = Vec::new();
    // Cast: the count's bits.
    mov_imm64_full(&mut buf, RCX, bytes as i64);
    emit_ir_utf16_string_alloc(&mut buf, s, &mut decline);
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
    let r = f(rbp, 0);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

fn u32_at(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(raw[at..at + 4].try_into().expect("4 bytes"))
}

#[test]
fn jr21_latin1up_utf16_alloc_builds_a_utf16_string_over_a_fresh_byte_array() {
    let flag: u8 = 1;
    let guard: u32 = 5;
    let s = site(&flag, &guard, 5);
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    for bytes in [2usize, 6, 8, 10, 62, 64] {
        // Cast: a small count.
        let (r, t, base, moved) = alloc(&s, bytes as u64, 64);
        assert_ne!(r, -1, "{bytes} bytes: not declined");
        assert_eq!(u32_at(&t, len_at) as usize, bytes, "a byte[{bytes}]");
        let string_at = data + bytes.div_ceil(8) * 8;
        // Cast: the answer back to an offset into the TLAB.
        assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
        assert_eq!(u32_at(&t, string_at), STRING_CID);
        let vo = string_at + VALUE_OFF as usize; // Cast: a small offset
        assert_eq!(
            u64::from_le_bytes(t[vo..vo + 8].try_into().expect("8 bytes")),
            base,
            "String.value is the fresh byte[]"
        );
        let co = string_at + CODER.0 as usize; // Cast: a small offset
        assert_eq!(t[co], 1, "coder UTF16");
        // Cast: small offsets.
        assert_eq!(moved as usize, string_at + TOTAL as usize, "one bump");
    }
    // Declines, each with the cursor untouched.
    let off: u8 = 0;
    let replaced = site(&flag, &guard, 6);
    let compact_off = site(&off, &guard, 5);
    let declines: [(&CharsIrSite, u64, usize, &str); 6] = [
        (&s, 0, 64, "empty"),
        (&s, 4096 + 2, 1024, "past the inline cap"),
        (&s, (1u64 << 32) + 2, 64, "a byte count past 2^32"),
        (&replaced, 4, 64, "replaced layout"),
        (&compact_off, 4, 64, "compact strings off"),
        (&s, 4, 4, "full TLAB"),
    ];
    for (site, bytes, tlab_words, what) in declines {
        let (r, _, _, moved) = alloc(site, bytes, tlab_words);
        assert_eq!((r, moved), (-1, 0), "{what}");
    }
}
