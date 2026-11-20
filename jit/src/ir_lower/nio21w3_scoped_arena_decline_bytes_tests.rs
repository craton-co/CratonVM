use super::{scoped_arena_resolve_decline_bytes, CALL_ARG_REGS};

/// The decline-form resolve, EXECUTED over a fake four-entry table: a live
/// slotted handle resolves to `ptr + offset` for every access that fits
/// its block; a past-the-end access, a stale key (another `seq` on the
/// same entry), a legacy handle (bit 61) and a cleared entry decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_resolve_answers_live_blocks_and_declines_everything_else() {
    use crate::ExecutableBuffer;
    let tag = 1u64 << 62;
    let handle = |seq: u64| tag | (seq << 30);
    // [key, len, ptr, pad] per entry.
    let mut table = vec![[0u64; 4]; 4];
    table[1] = [handle(1), 64, 0x5000, 0];
    table[2] = [handle(2), 4, 0x9000, 0];
    // Entry 3 cleared (a freed block): key 0.
    table[3] = [0, 64, 0x7000, 0];
    let base = table.as_ptr() as u64;
    let build = |w: u8| {
        let (bytes, decline) = scoped_arena_resolve_decline_bytes(base, w);
        // MOV R11, arg0 ; <resolve> ; MOV RAX, RDX ; RET ; decline: MOV RAX,-1 ; RET
        let a0 = CALL_ARG_REGS[0];
        let mut body: Vec<u8> = vec![
            0x49 | (u8::from(a0 >= 8) << 2),
            0x89,
            0xC3 | ((a0 & 7) << 3),
        ];
        let at = body.len();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(&[0x48, 0x89, 0xD0, 0xC3]);
        let decline_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
        for d in decline {
            let operand = at + d;
            let rel = i32::try_from(decline_at).unwrap() - i32::try_from(operand + 4).unwrap();
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        }
        let mut buf = ExecutableBuffer::new(512).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    };
    for (w, cases) in [
        (
            4u8,
            vec![
                (handle(1), 0x5000i64),
                (handle(1) + 8, 0x5008),
                (handle(1) + 60, 0x503C),
                (handle(1) + 61, -1),
                (handle(2), 0x9000),
                (handle(2) + 1, -1),
                // Same entry, another seq; a legacy handle; a cleared
                // entry; an entry never published.
                (handle(1 + (1 << 15)), -1),
                (handle(1) | (1u64 << 61), -1),
                (handle(3), -1),
                (handle(0), -1),
            ],
        ),
        (
            8u8,
            vec![
                (handle(1) + 56, 0x5038),
                (handle(1) + 57, -1),
                (handle(2), -1),
            ],
        ),
    ] {
        let buf = build(w);
        // SAFETY: a complete `extern "C" fn(u64) -> u64` that reads the
        // table and writes only RAX, RDX, R10 and R11, all caller-saved
        // on both ABIs.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr())
        };
        for (h, want) in cases {
            assert_eq!(f(h) as i64, want, "w={w} handle {h:#x}");
        }
    }
    drop(table);
}
