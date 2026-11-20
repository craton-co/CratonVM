use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Shapes chosen to reach every branch of the block grammar: an empty
/// block, one shorter than the 15-byte literal-length escape, one long
/// enough to force multi-byte literal *and* match lengths, a run that
/// encodes as an overlapping match (offset 1), and near-incompressible
/// data that is almost all literals.
fn lz4_decode_shapes() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        b"abc".to_vec(),
        b"the quick brown fox jumps over the lazy dog".to_vec(),
        vec![0x5Au8; 70_000],
        (0..40_000u32).map(|i| (i / 3) as u8).collect(),
        (0..30_000u32)
            .map(|i| i.wrapping_mul(2654435761).to_le_bytes()[0])
            .collect(),
    ]
}

#[test]
fn lz4_block_decode_round_trips_and_reports_the_source_length() {
    for original in lz4_decode_shapes() {
        let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len()) + 16];
        let clen = lz4_flex::block::compress_into(&original, &mut comp).expect("compress");

        // The source is deliberately longer than the block — this is the
        // `LZ4_decompress_fast` shape, where the caller cannot say where the
        // block ends and the tail is whatever was already in the buffer.
        for tail in [0u8, 0xFF] {
            let mut src = comp[..clen].to_vec();
            src.extend(std::iter::repeat_n(tail, 512));

            let mut out = vec![0u8; original.len()];
            let (consumed, produced) =
                lz4_block_decode(&src, &mut out).expect("decode a well-formed block");
            assert_eq!(out, original, "round trip (tail {tail:#x})");
            assert_eq!(produced, original.len());
            assert_eq!(
                consumed, clen,
                "consumed must be the block length, not the buffer length (tail {tail:#x})"
            );
        }
    }
}

#[test]
fn lz4_block_decode_refuses_malformed_input_instead_of_panicking() {
    let original = b"the quick brown fox jumps over the lazy dog".to_vec();
    let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len()) + 16];
    let clen = lz4_flex::block::compress_into(&original, &mut comp).expect("compress");

    // Truncated: every prefix must refuse rather than index out of range.
    // (A prefix can legitimately decode when the output happens to fill
    // first, so this asserts termination and bounds, not failure.)
    for cut in 0..clen {
        let mut out = vec![0u8; original.len()];
        let _ = lz4_block_decode(&comp[..cut], &mut out);
    }

    // An output buffer smaller than the block produces is a refusal, not a
    // truncated write — `LZ4_decompress_safe` reports that as a negative.
    let mut small = vec![0u8; original.len() - 1];
    assert!(lz4_block_decode(&comp[..clen], &mut small).is_none());

    // The other direction: an output bound LARGER than the block produces
    // must succeed and report the real produced length, because that is how
    // `LZ4_decompress_safe` is normally called.
    let mut roomy = vec![0u8; original.len() + 4096];
    let (consumed, produced) =
        lz4_block_decode(&comp[..clen], &mut roomy).expect("oversized output is not an error");
    assert_eq!(consumed, clen);
    assert_eq!(produced, original.len());
    assert_eq!(&roomy[..produced], &original[..]);

    // A match offset of 0 is invalid, and one reaching before the start of
    // the output would copy uninitialised bytes. Token 0x00 = no literals,
    // then a zero offset.
    let mut out = vec![0u8; 8];
    assert!(lz4_block_decode(&[0x00, 0x00, 0x00], &mut out).is_none());
    assert!(lz4_block_decode(&[0x00, 0x04, 0x00], &mut out).is_none());
}

#[test]
fn lz4_compress_bound_matches_liblz4() {
    // liblz4's LZ4_COMPRESSBOUND(n) == n + n/255 + 16, which is what HotSpot
    // answers through the real .so. The previous body used lz4_flex's own
    // (larger) estimate and diverged for every input.
    let mut ctx = crate::test_utils::mock_ctx();
    for n in [0i64, 1, 255, 256, 4096, 65_536, 1_000_000] {
        let got = lz4_compress_bound(&mut ctx, &[Value::Int(n as i32)])
            .unwrap()
            .unwrap();
        assert_eq!(got, Value::Int((n + n / 255 + 16) as i32), "bound for {n}");
    }
}

#[test]
fn snappy_block_round_trip() {
    let original = b"the quick brown fox jumps over the lazy dog, repeatedly.....";
    let mut comp = vec![0u8; snap::raw::max_compress_len(original.len())];
    let clen = snap::raw::Encoder::new()
        .compress(original, &mut comp)
        .unwrap();
    let dlen = snap::raw::decompress_len(&comp[..clen]).unwrap();
    assert_eq!(dlen, original.len());
    let mut out = vec![0u8; dlen];
    let n = snap::raw::Decoder::new()
        .decompress(&comp[..clen], &mut out)
        .unwrap();
    assert_eq!(&out[..n], original);
}

#[test]
fn lz4_block_round_trip() {
    let original: Vec<u8> = (0..3000u32).map(|i| (i % 97) as u8).collect();
    let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len())];
    let clen = lz4_flex::block::compress_into(&original, &mut comp).unwrap();
    assert!(clen > 0 && clen <= comp.len());
    // safe-decompress path (the one Kafka uses), bounded by max output size.
    let out = lz4_flex::block::decompress(&comp[..clen], original.len()).unwrap();
    assert_eq!(out, original);
}

#[test]
fn xxhash32_oneshot_matches_streaming_and_reference() {
    use std::hash::Hasher;
    // Reference xxHash32 of the empty input with seed 0 is 0x02CC5D05.
    let mut empty = twox_hash::XxHash32::with_seed(0);
    empty.write(&[]);
    assert_eq!(empty.finish() as u32, 0x02CC_5D05);

    // One-shot over "hello world" must equal the streamed "hello"+" world".
    let mut one = twox_hash::XxHash32::with_seed(0);
    one.write(b"hello world");
    let mut stream = twox_hash::XxHash32::with_seed(0);
    stream.write(b"hello");
    stream.write(b" world");
    assert_eq!(one.finish(), stream.finish());
}

#[test]
fn zstd_streaming_round_trip() {
    // Mirror the JNI contract end-to-end with raw zstd_safe buffers: a
    // multi-call compress (continue + end) then a single decompress.
    let original: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

    let mut cctx = CCtx::try_create().unwrap();
    let mut framed = Vec::new();
    // continue
    {
        let mut inb = InBuffer::around(&original);
        let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_CStreamOutSize() }];
        let mut outb = OutBuffer::around(&mut tmp[..]);
        cctx.compress_stream2(&mut outb, &mut inb, ZSTD_EndDirective::ZSTD_e_continue)
            .unwrap();
        framed.extend_from_slice(outb.as_slice());
        assert_eq!(inb.pos, original.len());
    }
    // end (drain)
    loop {
        let empty: [u8; 0] = [];
        let mut inb = InBuffer::around(&empty);
        let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_CStreamOutSize() }];
        let mut outb = OutBuffer::around(&mut tmp[..]);
        let rem = cctx
            .compress_stream2(&mut outb, &mut inb, ZSTD_EndDirective::ZSTD_e_end)
            .unwrap();
        framed.extend_from_slice(outb.as_slice());
        if rem == 0 {
            break;
        }
    }

    let mut dctx = DCtx::try_create().unwrap();
    let mut restored = Vec::new();
    let mut inb = InBuffer::around(&framed);
    while inb.pos < framed.len() {
        let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_DStreamOutSize() }];
        let mut outb = OutBuffer::around(&mut tmp[..]);
        dctx.decompress_stream(&mut outb, &mut inb).unwrap();
        restored.extend_from_slice(outb.as_slice());
    }
    assert_eq!(restored, original);
}
