//! zt t32 (successor page section 7): the inline colored store encode,
//! run as machine code against `ZgcRealHeap::colored_store_word`'s rule.

use super::zgc_colored_store_encode_rdx_bytes;
use crate::ExecutableBuffer;
use cratonvm_types::narrow_oop::{
    COLORED_SLOT_OFFSET_MASK, COLORED_SLOT_REMAPPED, COLORED_SLOT_TAG,
};

/// `extern "C" fn(value) -> encoded`: MOV RDX, arg0; the encode; MOV RAX,
/// RDX; RET. R11 is caller-saved on both ABIs.
fn encode(base: u64, value: u64) -> u64 {
    let mut buf = ExecutableBuffer::new(1 << 12).expect("test buffer");
    #[cfg(windows)]
    buf.emit(&[0x48, 0x89, 0xCA]); // MOV RDX, RCX
    #[cfg(not(windows))]
    buf.emit(&[0x48, 0x89, 0xFA]); // MOV RDX, RDI
    buf.emit(&zgc_colored_store_encode_rdx_bytes(base));
    buf.emit(&[0x48, 0x89, 0xD0, 0xC3]); // MOV RAX, RDX; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over caller-saved registers.
    let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    f(value)
}

/// The rule: null stays 0, an arena address is `TAG | Remapped | offset`,
/// an address below the base or past the 42-bit field stays plain.
fn model(base: u64, value: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    let off = value.wrapping_sub(base);
    if value < base || off > COLORED_SLOT_OFFSET_MASK {
        return value;
    }
    COLORED_SLOT_TAG | COLORED_SLOT_REMAPPED | off
}

#[test]
fn zt32_the_colored_store_encode_is_the_heaps_store_word() {
    let base = 0x7f12_3400_0000u64;
    let limit = COLORED_SLOT_OFFSET_MASK + 1;
    for value in [
        0,
        base,
        base + 8,
        base + 0x1234_5678,
        base + limit - 8,
        base + limit,
        base + limit + 0x100,
        base - 8,
        0x1000,
        0x0000_7fff_ffff_fff8,
    ] {
        assert_eq!(encode(base, value), model(base, value), "value {value:#x}");
    }
    let word = encode(base, base + 0x40);
    assert_eq!(word & COLORED_SLOT_OFFSET_MASK, 0x40);
    assert_ne!(word & COLORED_SLOT_TAG, 0, "tagged");
    assert_ne!(word & COLORED_SLOT_REMAPPED, 0, "Remapped");
}
