use super::*;

#[test]
fn from_words_reads_buf_addr_index_and_row() {
    let mut w = [0u64; crate::direct_helpers::NIO_CHARSET_IR_LAYOUT_QUERY_WORDS];
    w[0] = 42;
    w[23] = 1;
    w[31] = 4;
    w[32] = 9;
    w[33] = 100;
    w[34] = 5;
    w[35] = 12;
    w[36] = 101;
    w[37] = 2;
    w[38] = 3;
    let layout = NioCharsetIrLayout::from_words(&w).expect("valid layout");
    assert_eq!(layout.buf_addr_index, 4);
    assert_eq!(layout.buf_addr_row, 9);
    assert_eq!(layout.iso_dec_class, 100);
    assert_eq!(layout.dec_state_index, 5);
    assert_eq!(layout.dec_state_row, 12);
    assert_eq!(layout.coder_result_class, 101);
    assert_eq!(layout.cr_underflow_static_index, 2);
    assert_eq!(layout.cr_overflow_static_index, 3);
}

fn sample_nio_layout() -> NioCharsetIrLayout {
    let mut w = [0u64; crate::direct_helpers::NIO_CHARSET_IR_LAYOUT_QUERY_WORDS];
    w[0] = 10; // hbb_class
    w[1] = 8; // hbb_num_fields
    w[2] = 11; // hcb_class
    w[3] = 8; // hcb_num_fields
    w[4] = 12; // iso_class
    w[5] = 0; // mark
    w[6] = 1;
    w[7] = 1; // pos
    w[8] = 2;
    w[9] = 2; // lim
    w[10] = 3;
    w[11] = 3; // cap
    w[12] = 4;
    w[13] = 4; // bb_hb
    w[14] = 5;
    w[15] = 5; // bb_off
    w[16] = 6;
    w[17] = 6; // bb_be
    w[18] = 7;
    w[19] = 4; // cb_hb
    w[20] = 8;
    w[21] = 5; // cb_off
    w[22] = 9;
    w[23] = 1; // little endian
    w[24] = 13; // str_class
    w[25] = 2; // str_num_fields
    w[26] = 0; // str_val_index
    w[27] = 10;
    w[28] = 1; // str_coder_index
    w[29] = 11;
    w[30] = 1; // compact_strings
    w[31] = 7; // buf_addr_index
    w[32] = 12;
    w[33] = 14; // iso_dec_class
    w[34] = 4; // dec_state_index
    w[35] = 13;
    w[36] = 15; // coder_result_class
    w[37] = 0; // UNDERFLOW
    w[38] = 1; // OVERFLOW
    NioCharsetIrLayout::from_words(&w).expect("sample layout")
}

fn leak_info(
    class_name: &'static str,
    method_name: &'static str,
    descriptor: &'static str,
    num_jit_args: usize,
    return_type: u8,
    invoke_kind: u8,
) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name,
        method_name,
        descriptor,
        num_jit_args,
        return_type,
        invoke_kind,
        declaring_class_id: 1,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

#[test]
fn nio_charset_wrap_decode_if_else_fuses_to_string_return() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    #[rustfmt::skip]
    let code: [u8; 41] = [
        0x1a,             //  0: iload_0
        0x99, 0x00, 0x11, //  1: ifeq 18 (+17)
        0x2b,             //  4: aload_1 (charset)
        0x2c,             //  5: aload_2 (buff)
        0x1d,             //  6: iload_3 (off)
        0x05,             //  7: iconst_2 (len)
        0xb8, 0x00, 0x01, //  8: invokestatic ByteBuffer.wrap([BII)
        0xb6, 0x00, 0x02, // 11: invokevirtual Charset.decode(ByteBuffer)
        0x4e,             // 14: astore_3 (cb)
        0xa7, 0x00, 0x06, // 15: goto 21 (+6 -> skips 18..21)
        0x01,             // 18: aconst_null (else branch)
        0x4e,             // 19: astore_3
        0x00,             // 20: nop
        0xbb, 0x00, 0x03, // 21: new String
        0x59,             // 24: dup
        0x2d,             // 25: aload_3
        0xb6, 0x00, 0x04, // 26: invokevirtual CharBuffer.array()[C
        0x2d,             // 29: aload_3
        0xb6, 0x00, 0x05, // 30: invokevirtual CharBuffer.arrayOffset()I
        0x2d,             // 33: aload_3
        0xb6, 0x00, 0x06, // 34: invokevirtual CharBuffer.length()I
        0xb7, 0x00, 0x07, // 37: invokespecial String.<init>([CII)V
        0xb0,             // 40: areturn
    ];
    let mut b = IrBuilder::new(4, 5);
    b.set_param_types(&[IrType::Int, IrType::Ref, IrType::Ref, IrType::Int]);
    b.set_nio_charset_layout(Some(sample_nio_layout()));
    b.set_thread_local_site_plan(ThreadLocalSitePlan::for_compile(
        None,
        None,
        Some(std::sync::Arc::new(crate::deopt::DespecRegistry::new())),
        "ByteChunk.toStringInternal:()Ljava/lang/String;".to_string(),
    ));
    b.set_new_info(std::collections::HashMap::from([(21usize, (13u32, 2usize))]), Default::default());
    b.set_invoke_info(std::collections::HashMap::from([
        (8usize, (leak_info("java/nio/ByteBuffer", "wrap", "([BII)Ljava/nio/ByteBuffer;", 3, b'L', 3), 3, b'L')),
        (11usize, (leak_info("java/nio/charset/Charset", "decode", "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;", 2, b'L', 0), 2, b'L')),
        (26usize, (leak_info("java/nio/CharBuffer", "array", "()[C", 1, b'L', 0), 1, b'L')),
        (30usize, (leak_info("java/nio/CharBuffer", "arrayOffset", "()I", 1, b'I', 0), 1, b'I')),
        (34usize, (leak_info("java/nio/CharBuffer", "length", "()I", 1, b'I', 0), 1, b'I')),
        (37usize, (leak_info("java/lang/String", "<init>", "([CII)V", 4, b'V', 1), 4, b'V')),
    ]));
    let g = b.build(&code, code.len()).expect("if/else fusion builds");
    assert!(
        g.nodes.iter().any(|n| n.op == Op::ByteArrayCopy),
        "expected fused ByteArrayCopy in if/else decode-to-String graph"
    );
}

#[test]
fn nio_charset_decoder_decode_and_allocate_build_ir() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    #[rustfmt::skip]
    let code: [u8; 16] = [
        0x1c,             //  0: iload_2 (cap)
        0xb8, 0x00, 0x01, //  1: invokestatic CharBuffer.allocate(I)
        0x4e,             //  4: astore_3 (cb)
        0x2a,             //  5: aload_0 (decoder)
        0x2b,             //  6: aload_1 (bb)
        0x2d,             //  7: aload_3 (cb)
        0x04,             //  8: iconst_1 (endOfInput)
        0xb6, 0x00, 0x02, //  9: invokevirtual CharsetDecoder.decode(BB,CB,Z)
        0xb0,             // 12: areturn
        0x00, 0x00, 0x00,
    ];
    let mut b = IrBuilder::new(4, 5);
    b.set_param_types(&[IrType::Ref, IrType::Ref, IrType::Int]);
    b.set_nio_charset_layout(Some(sample_nio_layout()));
    b.set_invoke_info(std::collections::HashMap::from([
        (1usize, (leak_info("java/nio/CharBuffer", "allocate", "(I)Ljava/nio/CharBuffer;", 1, b'L', 3), 1, b'L')),
        (9usize, (leak_info("java/nio/charset/CharsetDecoder", "decode", "(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;Z)Ljava/nio/charset/CoderResult;", 4, b'L', 0), 4, b'L')),
    ]));
    let g = b.build(&code, 13).expect("allocate + decoder.decode builds");
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::NewArray { element_type: 5, .. })));
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::LoadStatic { class_id: 15, .. })));
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::ArrayStore(MemKind::Char))));
}
