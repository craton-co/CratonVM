use super::{unsafe_offheap_address_decline_bytes, CALL_ARG_REGS};

/// Run the generator's bytes as `fn(addr) -> RDX or -1 on a decline`.
fn build(table: Option<u64>, w: u8) -> crate::ExecutableBuffer {
    let (bytes, decline) = unsafe_offheap_address_decline_bytes(table, w);
    // MOV R11, arg0 ; <address> ; MOV RAX, RDX ; RET ; decline: MOV RAX,-1 ; RET
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
    let mut buf = crate::ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &crate::ExecutableBuffer, addr: u64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64) -> i64` that reads only its
    // argument and the caller's fake table (alive across every call) and
    // writes only RAX, RDX, R10 and R11, all caller-saved on both ABIs.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
    f(addr)
}

/// No table: a positive untagged address passes through; zero, a negative
/// value and any tagged handle (slotted or legacy) decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn without_a_table_only_a_real_pointer_passes() {
    let buf = build(None, 4);
    let tag = 1u64 << 62;
    assert_eq!(run(&buf, 0x1234_5678), 0x1234_5678);
    assert_eq!(run(&buf, tag - 1), (tag - 1) as i64);
    // R2: the synthetic static-offset band declines, its neighbours pass.
    assert_eq!(run(&buf, 0x1FFF_FFFF), 0x1FFF_FFFF);
    assert_eq!(run(&buf, 0x2000_0000), -1);
    assert_eq!(run(&buf, 0x2000_0010), -1);
    assert_eq!(run(&buf, 0x3FFF_FFFF), -1);
    assert_eq!(run(&buf, 0x4000_0000), 0x4000_0000);
    assert_eq!(run(&buf, 0), -1);
    assert_eq!(run(&buf, u64::MAX), -1, "negative declines");
    assert_eq!(run(&buf, 1u64 << 63), -1);
    assert_eq!(
        run(&buf, tag | (1 << 30)),
        -1,
        "a slotted handle with no table"
    );
    assert_eq!(
        run(&buf, tag | (1 << 61) | 0x10_0000_0000),
        -1,
        "a legacy handle"
    );
}

/// With a table: a real pointer still passes through untouched; a live
/// slotted handle resolves to `ptr + offset`; a past-the-end access, a
/// stale key and a legacy handle decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn with_a_table_a_live_handle_resolves_and_the_rest_declines() {
    let tag = 1u64 << 62;
    let handle = |seq: u64| tag | (seq << 30);
    let mut table = vec![[0u64; 4]; 4];
    table[1] = [handle(1), 64, 0x5000, 0];
    let base = table.as_ptr() as u64;
    let buf = build(Some(base), 8);
    assert_eq!(run(&buf, 0x7000), 0x7000, "a real pointer");
    assert_eq!(run(&buf, handle(1)), 0x5000);
    assert_eq!(run(&buf, handle(1) + 56), 0x5038);
    assert_eq!(run(&buf, handle(1) + 57), -1, "past the block");
    assert_eq!(
        run(&buf, handle(1 + (1 << 15))),
        -1,
        "same entry, another seq"
    );
    // Entry 1 again (the index bits), so the read stays inside the table.
    assert_eq!(run(&buf, handle(1) | (1u64 << 61)), -1, "a legacy handle");
    assert_eq!(run(&buf, 0), -1);
    drop(table);
}
