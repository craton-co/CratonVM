use super::{g1_aastore_screen_bytes, ir_g1_gated_aastore_enabled, CALL_ARG_REGS};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const REGION_SIZE: usize = 0x10_0000;
const ARENA_BASE: usize = 0x4000_0000 + 0x30;
const DECLINED: i64 = -1;

fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

#[test]
fn r21_g1_aastore_screen_separates_plain_cross_and_armed() {
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static GATE: AtomicU8 = AtomicU8::new(0);
    TABLE[0].store(ARENA_BASE, Ordering::Release);
    TABLE[2].store(!(REGION_SIZE - 1), Ordering::Release);
    TABLE[1].store(8 * REGION_SIZE, Ordering::Release);
    GATE.store(0, Ordering::Release);
    let plan = crate::x64::G1AastorePlan {
        pre: &GATE as *const AtomicU8 as usize, // Cast: the gate's address
        table: TABLE.as_ptr() as usize,         // Cast: the table's address
        post: 1,
    };
    let (bytes, decline, cross) = g1_aastore_screen_bytes(plan);
    assert_eq!(decline.len(), 1, "one decline edge: the armed gate");
    assert_eq!(cross.len(), 1, "one cross-region edge");
    let mut body: Vec<u8> = Vec::new();
    // RAX = arg0 (array) first, then RDX = arg1 (value): on Win64 arg1 IS
    // RDX, and arg0 (RCX) is read before anything writes it.
    body.extend_from_slice(&mov(0, CALL_ARG_REGS[0]));
    body.extend_from_slice(&mov(2, CALL_ARG_REGS[1]));
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET (plain)
    let cross_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let mut patch = |field: usize, to: usize| {
        let operand = at + field;
        let rel =
            i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    for d in decline {
        patch(d, decline_at);
    }
    for c in cross {
        patch(c, cross_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only
    // GATE and TABLE (statics) and writes RAX, RCX, RDX, R10, R11
    // (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
    };
    let ask = |a: usize, v: usize| f(a as u64, v as u64); // Cast: test addresses

    let r0 = ARENA_BASE + 8;
    let r1 = ARENA_BASE + REGION_SIZE + 8;
    let same_lo = ARENA_BASE + REGION_SIZE - 0x38;
    let same_hi = same_lo + 0x10;
    let last_of_r0 = ARENA_BASE + REGION_SIZE - 8;
    let first_of_r1 = ARENA_BASE + REGION_SIZE + 8;
    assert_ne!(same_lo >> 20, same_hi >> 20, "setup: two aligned blocks");
    assert_eq!(last_of_r0 >> 20, first_of_r1 >> 20, "setup: one aligned block");
    let cases: [(usize, usize, i64, &str); 6] = [
        (r0, 0, 0, "a null value records no edge"),
        (r0, r0 + 64, 0, "a same-region value records no edge"),
        (r0, r1, 1, "a cross-region value takes the lean barrier"),
        (r1, r0, 1, "cross-region the other way"),
        (same_lo, same_hi, 0, "one region across an aligned block boundary"),
        (last_of_r0, first_of_r1, 1, "two regions inside one aligned block"),
    ];
    for (a, v, want, why) in cases {
        assert_eq!(ask(a, v), want, "gate clear: {why}");
    }
    GATE.store(1, Ordering::Release);
    for (a, v, _, why) in cases {
        assert_eq!(ask(a, v), DECLINED, "gate armed: jit_aastore logs the old element ({why})");
    }
    GATE.store(0, Ordering::Release);
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

