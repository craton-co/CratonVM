use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const STRIDE: usize = 16;
const TYPE_OFFSET: usize = 8;
const SHIFT: u8 = 20;

fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

/// `f(holder_or_xor, value_offset) -> 0 (skip) | 1 (call)`.
fn build(table: &[AtomicUsize; 5], xor: bool) -> crate::ExecutableBuffer {
    let (bytes, skips) = young_holder_skip_bytes_with(
        RAX,
        xor.then_some(RDX),
        R11,
        table.as_ptr() as u64, // Cast: the table's address
        SHIFT,
        STRIDE,
        TYPE_OFFSET,
        1,
    )
    .expect("encodable registers");
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&mov(RAX, ARG_REGS[0]));
    body.extend_from_slice(&mov(RDX, ARG_REGS[1]));
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET (call)
    let skip_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET (skip)
    for s in skips {
        let operand = at + s;
        let rel = i32::try_from(skip_at).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

#[test]
fn r23_w3_the_young_filter_skips_only_eden_and_survivor_holders() {
    // Four regions: Eden, Old, Survivor, Free (the `RegionType` bytes).
    let mut regions = vec![0u8; 4 * STRIDE];
    for (i, ty) in [0u8, 2, 1, 5].into_iter().enumerate() {
        regions[i * STRIDE + TYPE_OFFSET] = ty;
    }
    let table: Box<[AtomicUsize; 5]> = Box::new([
        AtomicUsize::new(regions.as_ptr() as usize), // Cast: the array's address
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(usize::from(SHIFT)),
        AtomicUsize::new(4),
    ]);
    let region = |i: usize, off: usize| (i << SHIFT) + off;
    for xor in [false, true] {
        let buf = build(&table, xor);
        // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` reading only
        // the table and the region bytes, writing RAX, RDX, R11.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
        };
        // The value lives in region 3; in the XOR form the first argument
        // is the masked XOR the existing filters leave behind.
        let ask = |holder: usize| {
            let v = region(3, 0x40);
            let a = if xor { (holder ^ v) & !((1usize << SHIFT) - 1) } else { holder };
            f(a as u64, v as u64) // Cast: test offsets
        };
        assert_eq!(ask(region(0, 0x10)), 0, "Eden holder: skip ({xor})");
        assert_eq!(ask(region(2, 0x10)), 0, "Survivor holder: skip ({xor})");
        assert_eq!(ask(region(1, 0x10)), 1, "Old holder: call ({xor})");
        assert_eq!(ask(region(9, 0x10)), 1, "past the region count: call ({xor})");
        table[3].store(0, Ordering::Release);
        assert_eq!(ask(region(0, 0x10)), 1, "unpublished: call ({xor})");
        table[3].store(usize::from(SHIFT) + 1, Ordering::Release);
        assert_eq!(ask(region(0, 0x10)), 1, "another shift: call ({xor})");
        table[3].store(usize::from(SHIFT), Ordering::Release);
    }
    drop(regions);
}

#[test]
fn r23_w3_a_sib_register_is_refused() {
    assert!(young_holder_skip_bytes_with(RSP, None, R11, 0, 20, 16, 8, 1).is_none());
    assert!(young_holder_skip_bytes_with(RAX, None, R12, 0, 20, 16, 8, 1).is_none());
    assert!(young_holder_skip_bytes_with(RAX, Some(RAX), R11, 0, 20, 16, 8, 1).is_none());
    assert!(young_holder_skip_bytes_with(R10, None, R11, 0, 20, 16, 8, 1).is_some());
}
