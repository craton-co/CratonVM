use super::sp_vh_const_handle_compare_bytes;

#[cfg(windows)]
const ARG0: u8 = 1; // RCX
#[cfg(not(windows))]
const ARG0: u8 = 7; // RDI

/// `extern "C" fn(handle) -> i64`: `1` when the operand equals static
/// `field`'s value, `0` on the decline edge.
fn build(base_cell: usize, field: usize) -> crate::ExecutableBuffer {
    let (bytes, edges) = sp_vh_const_handle_compare_bytes(base_cell, field).expect("fits");
    // MOV R11, arg0 (REX.W|REX.B, 89 /r, reg = arg0, rm = R11).
    let mut body: Vec<u8> = vec![
        0x49 | (u8::from(ARG0 >= 8) << 2),
        0x89,
        0xC3 | ((ARG0 & 7) << 3),
    ];
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0xC3]); // MOV EAX, 1 ; RET
    let decline = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    for e in edges {
        let operand = at + e;
        let rel =
            i32::try_from(decline).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

#[test]
fn the_proof_compares_the_static_s_current_value() {
    // Three 16-byte cells; static 2's 64-bit payload is the handle.
    let mut block = vec![0u64; 8];
    let word =
        (2 * cratonvm_types::SLOT_SIZE + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) / 8;
    block[word] = 0xABCD_EF00;
    let cell: Box<usize> = Box::new(block.as_ptr() as usize); // Cast: the block's address
    let buf = build(&*cell as *const usize as usize, 2); // Cast: the cell's address
                                                         // SAFETY: a complete `extern "C" fn(u64) -> i64` reading only `cell`
                                                         // and `block` (alive across the calls), writing RAX, RCX and R11.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
    assert_eq!(f(0xABCD_EF00), 1, "the filed handle");
    assert_eq!(f(0xABCD_EF08), 0, "another object declines");
    assert_eq!(f(0), 0, "null declines");
    block[word] = 0x1234_0000;
    assert_eq!(f(0x1234_0000), 1, "the CURRENT value is what is compared");
    drop((cell, block));
}
