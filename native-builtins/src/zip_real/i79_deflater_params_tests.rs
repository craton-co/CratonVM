use super::*;

const HUFFMAN_ONLY: i32 = 2;

fn params(level: i32, strategy: i32) -> i32 {
    1 | (strategy << 1) | (level << 3)
}

/// Deflate `input` to the end on `st` (FINISH), with a roomy buffer.
fn finish(st: &mut DeflaterState, input: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; input.len() + 1024];
    let (used_in, used_out, finished, pending) =
        st.step(0, 4, input, &mut out).expect("deflate");
    assert_eq!(used_in as usize, input.len());
    assert!(finished && !pending);
    out.truncate(used_out as usize);
    out
}

fn inflate_raw(compressed: &[u8], len: usize) -> Vec<u8> {
    let mut d = Decompress::new(false);
    let mut out = vec![0u8; len + 16];
    d.decompress(compressed, &mut out, FlushDecompress::Finish)
        .expect("inflate");
    out.truncate(d.total_out() as usize);
    out
}

/// `setStrategy(HUFFMAN_ONLY)` reaches zlib: a parameter call on a fresh
/// stream consumes and produces nothing (`deflateParams` only), and the
/// stream it leaves cannot use LZ77 matches, so 10 000 `'a'` bytes cost
/// about a bit each where the default strategy needs a few dozen bytes.
/// Before wave 79 the strategy bits were dropped (`set_level` only).
#[test]
fn i79_a_strategy_change_reaches_zlib_and_does_not_compress_in_that_call() {
    let input = vec![b'a'; 10_000];
    let mut huff = DeflaterState::new(6, 0, true).expect("init");
    let mut out = vec![0u8; 64];
    let got = huff
        .step(params(6, HUFFMAN_ONLY), 0, &input, &mut out)
        .expect("params");
    assert_eq!(got, (0, 0, false, false), "a parameter call only switches");
    let huff_bytes = finish(&mut huff, &input);

    let mut plain = DeflaterState::new(6, 0, true).expect("init");
    let plain_bytes = finish(&mut plain, &input);

    assert!(
        huff_bytes.len() > 1000,
        "Huffman-only: {} bytes",
        huff_bytes.len()
    );
    assert!(
        plain_bytes.len() < 100,
        "default: {} bytes",
        plain_bytes.len()
    );
    assert_eq!(inflate_raw(&huff_bytes, input.len()), input);
    assert_eq!(inflate_raw(&plain_bytes, input.len()), input);
}

/// `deflateParams` answering `Z_BUF_ERROR` (pending output larger than the
/// buffer, input not taken) reports what it wrote and bit 63, so
/// `Deflater.deflate` keeps `setParams` and retries -- `Deflater.c`'s
/// `checkDeflateStatus`. The level change was lost before wave 79.
#[test]
fn i79_a_params_buf_error_keeps_the_change_pending() {
    let mut seed = 0x1234_5678u32;
    let input: Vec<u8> = (0..4096)
        .map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 24) as u8
        })
        .collect();
    let mut st = DeflaterState::new(6, 0, true).expect("init");
    let mut small = [0u8; 16];
    // SYNC_FLUSH into 16 bytes: all input taken, a block left pending.
    let (used_in, used_out, finished, pending) =
        st.step(0, 2, &input, &mut small).expect("deflate");
    assert_eq!(
        (used_in, used_out, finished, pending),
        (4096, 16, false, false)
    );
    // A level change now cannot flush: Z_BUF_ERROR, 16 bytes out, none in.
    let got = st
        .step(params(1, 0), 0, &input, &mut small)
        .expect("params");
    assert_eq!(got, (0, 16, false, true));
    assert_eq!(
        pack_deflate_result_params(got.0, got.1, got.2, got.3) as u64 >> 63,
        1,
        "bit 63 = params still pending"
    );
}

/// `getAdler()` is zlib's `strm->adler`: 1 forever for a raw stream
/// (zlib updates it only when `wrap == 1`), the running ADLER-32 for a
/// wrapped one, and the seed again after `reset()`
/// (i73-L5-inflater-getadler-differs-from-zlib, case 2).
#[test]
fn i79_deflater_adler_is_zlibs() {
    let input = b"hello hello hello world";
    let mut raw = DeflaterState::new(6, 0, true).expect("init");
    assert_eq!(raw.adler(), 1);
    let _ = finish(&mut raw, input);
    assert_eq!(raw.adler(), 1, "raw stream keeps the seed");

    let mut wrapped = DeflaterState::new(6, 0, false).expect("init");
    let _ = finish(&mut wrapped, input);
    assert_eq!(wrapped.adler(), adler32_update(1, input));
    wrapped.reset();
    assert_eq!(wrapped.adler(), 1);
}

/// i73 page case 3: at `Z_NEED_DICT` the inflater's `getAdler()` is the
/// stream's DICTID (the dictionary's ADLER-32), it stays so across
/// `setDictionary`, and the next inflate re-seeds it before producing the
/// output (zlib's `DICT` state), ending at the output's ADLER-32.
#[test]
fn i79_inflater_reports_the_dictid_at_need_dict() {
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};
    const DICT: &[u8] = b"optionsgetheadpostputdelete";
    let payload: &[u8] = b"options get head post put delete options get";
    let mut c = Compress::new(flate2::Compression::default(), true);
    c.set_dictionary(DICT).expect("dict");
    let mut compressed = Vec::with_capacity(256);
    c.compress_vec(payload, &mut compressed, FlushCompress::Finish)
        .expect("compress");

    let mut ctx = mock_ctx();
    let addr = inflater_init(false, true);
    fn adler_of(ctx: &mut dyn NativeContext, addr: i64) -> u32 {
        match infl_get_adler(ctx, &[Value::Long(addr)]).unwrap().unwrap() {
            Value::Int(a) => a as u32,
            other => panic!("expected Int, got {other:?}"),
        }
    }
    let mut out = vec![0u8; 256];
    let first = infl_do_decompress(addr, &compressed, &mut out);
    assert!(first.need_dict && first.output_consumed == 0);
    assert_eq!(
        adler_of(&mut ctx, addr),
        adler32_update(1, DICT),
        "DICTID at needsDictionary()"
    );

    let dict_arr = ctx.new_array(ArrayElementType::Byte, DICT.len());
    for (i, b) in DICT.iter().enumerate() {
        ctx.set_array_element(dict_arr, i, Value::Int(*b as i8 as i32));
    }
    infl_set_dictionary(
        &mut ctx,
        &[
            Value::Long(addr),
            Value::Object(Some(dict_arr)),
            Value::Int(0),
            Value::Int(DICT.len() as i32),
        ],
    )
    .unwrap();
    assert_eq!(
        adler_of(&mut ctx, addr),
        adler32_update(1, DICT),
        "kept across setDictionary"
    );

    let rest = &compressed[first.input_consumed as usize..];
    let second = infl_do_decompress(addr, rest, &mut out);
    assert!(second.finished);
    assert_eq!(&out[..second.output_consumed as usize], payload);
    assert_eq!(
        adler_of(&mut ctx, addr),
        adler32_update(1, payload),
        "the output's ADLER-32"
    );
    infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
}
