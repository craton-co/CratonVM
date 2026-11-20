    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write;

    /// Unpacks a `Deflater.deflate*` result long into
    /// `(inputConsumed, outputConsumed, finished)`.
    fn unpack_deflate_result(packed: i64) -> (usize, usize, bool) {
        let p = packed as u64;
        (
            (p & 0x7FFF_FFFF) as usize,
            ((p >> 31) & 0x7FFF_FFFF) as usize,
            (p >> 62) & 1 == 1,
        )
    }

    fn assert_decompresses_to(compressed: &[u8], original: &[u8]) {
        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], original);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    fn new_deflater(ctx: &mut dyn NativeContext) -> i64 {
        match defl_init(ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        }
    }

    /// Regression guard for the direct-ByteBuffer `Deflater.deflate*`
    /// overloads (`deflateBytesBuffer`/`deflateBufferBytes`/
    /// `deflateBufferBuffer`): these used to throw `NotImplemented`, then
    /// were implemented for real (see `defl_deflate_bytes_buffer`'s doc
    /// comment for the Jetty `GzipHttpOutputInterceptor` bug this fixed).
    /// Exercise all three overloads end-to-end and confirm the compressed
    /// output actually decompresses back to the original input, rather than
    /// just checking that *some* non-error long comes back.
    #[test]
    fn direct_buffer_deflate_paths_produce_correct_output() {
        let mut ctx = mock_ctx();
        let original = b"hello hello hello hello hello world world world";

        // deflateBytesBuffer: heap byte[] input, direct ByteBuffer output.
        let addr = new_deflater(&mut ctx);
        let input_arr = ctx.new_array(ArrayElementType::Byte, original.len());
        for (i, b) in original.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let mut output_buf = vec![0u8; 1024];
        let output_addr = output_buf.as_mut_ptr() as i64;
        let packed = match defl_deflate_bytes_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(original.len() as i32),
                Value::Long(output_addr),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(output_consumed > 0, "deflateBytesBuffer produced no output");
        assert!(
            finished,
            "deflateBytesBuffer must report finished on FINISH"
        );
        assert_decompresses_to(&output_buf[..output_consumed], original);

        // deflateBufferBytes: direct ByteBuffer input, heap byte[] output.
        let addr = new_deflater(&mut ctx);
        let mut input_buf = original.to_vec();
        let input_addr = input_buf.as_mut_ptr() as i64;
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);
        let packed = match defl_deflate_buffer_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input_addr),
                Value::Int(original.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(output_consumed > 0, "deflateBufferBytes produced no output");
        assert!(
            finished,
            "deflateBufferBytes must report finished on FINISH"
        );
        let compressed = read_byte_array(&ctx, output_arr, 0, output_consumed);
        assert_decompresses_to(&compressed, original);

        // deflateBufferBuffer: both input and output are direct ByteBuffers.
        let addr = new_deflater(&mut ctx);
        let mut input_buf = original.to_vec();
        let input_addr = input_buf.as_mut_ptr() as i64;
        let mut output_buf = vec![0u8; 1024];
        let output_addr = output_buf.as_mut_ptr() as i64;
        let packed = match defl_deflate_buffer_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input_addr),
                Value::Int(original.len() as i32),
                Value::Long(output_addr),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(
            output_consumed > 0,
            "deflateBufferBuffer produced no output"
        );
        assert!(
            finished,
            "deflateBufferBuffer must report finished on FINISH"
        );
        assert_decompresses_to(&output_buf[..output_consumed], original);
    }

    /// Regression guard for the three direct-ByteBuffer `Inflater` overloads.
    #[test]
    fn direct_buffer_inflate_paths_produce_correct_output() {
        let original = b"hello hello hello hello hello world world world";
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();

        fn unpack(packed: i64) -> (usize, usize, bool, bool) {
            let p = packed as u64;
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 62) & 1 == 1,
                (p >> 63) & 1 == 1,
            )
        }
        fn new_inflater(ctx: &mut dyn NativeContext) -> i64 {
            match infl_init(ctx, &[Value::Int(1)]).unwrap().unwrap() {
                Value::Long(addr) => addr,
                other => panic!("expected Long handle, got {other:?}"),
            }
        }
        fn packed(result: MethodCallResult) -> i64 {
            match result.unwrap().unwrap() {
                Value::Long(value) => value,
                other => panic!("expected Long result, got {other:?}"),
            }
        }

        let mut ctx = mock_ctx();
        let addr = new_inflater(&mut ctx);
        let input_arr = ctx.new_array(ArrayElementType::Byte, compressed.len());
        for (i, b) in compressed.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i8 as i32));
        }
        let mut output = vec![0u8; 1024];
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_bytes_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(compressed.len() as i32),
                Value::Long(output.as_mut_ptr() as i64),
                Value::Int(output.len() as i32),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(&output[..used_out], original);
        assert!(finished && !need_dict);

        let addr = new_inflater(&mut ctx);
        let mut input = compressed.clone();
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_buffer_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input.as_mut_ptr() as i64),
                Value::Int(input.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(read_byte_array(&ctx, output_arr, 0, used_out), original);
        assert!(finished && !need_dict);

        let addr = new_inflater(&mut ctx);
        let mut input = compressed.clone();
        let mut output = vec![0u8; 1024];
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_buffer_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input.as_mut_ptr() as i64),
                Value::Int(input.len() as i32),
                Value::Long(output.as_mut_ptr() as i64),
                Value::Int(output.len() as i32),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(&output[..used_out], original);
        assert!(finished && !need_dict);
    }

    /// Interpreter round i3 wave 73, lane L5 (`CRATONVM_INFLATER_REUSE_BUFFERS`):
    /// a raw stream inflated in uneven slices (700-byte input windows, 3000-byte
    /// output windows at moving offsets, so the kept buffers hold stale bytes
    /// from the previous call) answers the same packed results, the same output
    /// bytes and the same `getAdler()` through the scratch buffers as through
    /// fresh ones; and a call larger than the cap (one 70 000-byte window)
    /// takes the allocating path with the switch on and still agrees.
    #[test]
    fn i73_reused_inflater_buffers_inflate_exactly_as_fresh_ones() {
        let original: Vec<u8> = (0..20_000u32)
            .map(|i| ((i * 7) % 251) as u8 ^ (i / 300) as u8)
            .collect();
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&original).unwrap();
        let compressed = enc.finish().unwrap();

        fn run(
            reuse: bool,
            compressed: &[u8],
            room: usize,
            in_step: usize,
            out_step: usize,
        ) -> (Vec<i64>, Vec<u8>, i32) {
            let mut ctx = mock_ctx();
            let addr = match infl_init(&mut ctx, &[Value::Int(1)]).unwrap().unwrap() {
                Value::Long(a) => a,
                other => panic!("expected Long handle, got {other:?}"),
            };
            let input = ctx.new_array(ArrayElementType::Byte, compressed.len());
            for (i, b) in compressed.iter().enumerate() {
                ctx.set_array_element(input, i, Value::Int(*b as i8 as i32));
            }
            let out = ctx.new_array(ArrayElementType::Byte, room);
            let mut packs = Vec::new();
            let mut in_pos = 0usize;
            let mut out_pos = 0usize;
            for _ in 0..10_000 {
                let in_len = (compressed.len() - in_pos).min(in_step);
                let out_len = (room - out_pos).min(out_step);
                let packed = match inflate_bytes_bytes(
                    &mut ctx,
                    &[
                        Value::Object(None),
                        Value::Long(addr),
                        Value::Object(Some(input)),
                        Value::Int(in_pos as i32),
                        Value::Int(in_len as i32),
                        Value::Object(Some(out)),
                        Value::Int(out_pos as i32),
                        Value::Int(out_len as i32),
                    ],
                    reuse,
                )
                .unwrap()
                .unwrap()
                {
                    Value::Long(p) => p,
                    other => panic!("expected Long result, got {other:?}"),
                };
                packs.push(packed);
                let p = packed as u64;
                in_pos += (p & 0x7FFF_FFFF) as usize;
                out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
                if (p >> 62) & 1 == 1 {
                    break;
                }
            }
            let adler = match infl_get_adler(&mut ctx, &[Value::Long(addr)])
                .unwrap()
                .unwrap()
            {
                Value::Int(a) => a,
                other => panic!("expected Int adler, got {other:?}"),
            };
            let bytes = read_byte_array(&ctx, out, 0, out_pos);
            infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
            (packs, bytes, adler)
        }

        let room = original.len() + 64;
        let fresh = run(false, &compressed, room, 700, 3000);
        assert_eq!(fresh.1, original);
        assert!(fresh.0.len() > 2, "the slices make several calls");
        assert_eq!(run(true, &compressed, room, 700, 3000), fresh);

        let big_room = 70_000;
        assert!(big_room > INFLATER_SCRATCH_CAP);
        let fresh_big = run(false, &compressed, big_room, compressed.len(), big_room);
        assert_eq!(fresh_big.1, original);
        assert_eq!(
            run(true, &compressed, big_room, compressed.len(), big_room),
            fresh_big
        );
    }

    /// What one stream driven the way `Inflater` drives it answers: the packed
    /// result of every call, the bytes produced, `getAdler()` at the end, and
    /// the index of the call that threw (if one did).
    type Driven = (Vec<i64>, Vec<u8>, i32, Option<usize>);

    /// Drives one stream through `inflateBytesBytes` as `Inflater.inflate`
    /// does -- every call hands over ALL the input not consumed yet (the
    /// `setInput` buffer from `inputPos` to `inputLim`) and asks for the
    /// next `out_steps` window -- with `CRATONVM_INFLATER_INPUT_WINDOW`'s
    /// choice passed in; answers a needDict with `dict`.
    fn drive_inflate(
        window: bool,
        nowrap: bool,
        compressed: &[u8],
        dict: Option<&[u8]>,
        out_steps: &[usize],
        room: usize,
    ) -> Driven {
        let mut ctx = mock_ctx();
        let addr = match infl_init(&mut ctx, &[Value::Int(i32::from(nowrap))])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let input = byte_array(&mut ctx, compressed);
        let out = ctx.new_array(ArrayElementType::Byte, room);
        let mut packs = Vec::new();
        let mut thrown = None;
        let (mut in_pos, mut out_pos) = (0usize, 0usize);
        for call in 0..200_000usize {
            let out_len = (room - out_pos).min(out_steps[call % out_steps.len()]);
            let result = inflate_bytes_bytes_with(
                &mut ctx,
                &[
                    Value::Object(None),
                    Value::Long(addr),
                    Value::Object(Some(input)),
                    Value::Int(in_pos as i32),
                    Value::Int((compressed.len() - in_pos) as i32),
                    Value::Object(Some(out)),
                    Value::Int(out_pos as i32),
                    Value::Int(out_len as i32),
                ],
                false,
                window,
            );
            let packed = match result {
                Ok(Some(Value::Long(p))) => p,
                Ok(other) => panic!("expected Long result, got {other:?}"),
                Err(_) => {
                    thrown = Some(call);
                    break;
                }
            };
            packs.push(packed);
            let p = packed as u64;
            in_pos += (p & 0x7FFF_FFFF) as usize;
            out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
            if (p >> 62) & 1 == 1 {
                break;
            }
            if (p >> 63) & 1 == 1 {
                let d = dict.expect("a needDict without a dictionary");
                let d_arr = byte_array(&mut ctx, d);
                infl_set_dictionary(
                    &mut ctx,
                    &[
                        Value::Long(addr),
                        Value::Object(Some(d_arr)),
                        Value::Int(0),
                        Value::Int(d.len() as i32),
                    ],
                )
                .unwrap();
            } else if packed == 0 {
                // No progress at all: the input ran out (or the room did).
                break;
            }
        }
        let adler = match infl_get_adler(&mut ctx, &[Value::Long(addr)])
            .unwrap()
            .unwrap()
        {
            Value::Int(a) => a,
            other => panic!("expected Int adler, got {other:?}"),
        };
        // On a throw the bytes the failing call wrote are in the array too:
        // compare the whole room then.
        let shown = if thrown.is_some() { room } else { out_pos };
        let bytes = read_byte_array(&ctx, out, 0, shown);
        infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
        (packs, bytes, adler, thrown)
    }

    /// Interpreter round i3 wave 75, lane L5 (`CRATONVM_INFLATER_INPUT_WINDOW`):
    /// a stream inflated through bounded input windows answers, call for
    /// call, exactly what the whole-input call answers -- the packed result
    /// (input consumed, output produced, finished, needDict), the bytes, and
    /// `getAdler()` -- over 1-byte and odd-sized output calls with the whole
    /// remaining input handed over each time (`InflaterInputStream.read()`'s
    /// shape), for a raw stream, a zlib-wrapped one, a stored (level 0) one
    /// whose output needs more input than one window, a preset-dictionary
    /// one, and a corrupt one (the same call throws, with the same bytes
    /// written).
    #[test]
    fn i75_windowed_inflate_matches_the_whole_input_call() {
        assert_eq!(inflater_input_window(20_000, 1), INFLATER_INPUT_WINDOW_MIN);
        assert_eq!(inflater_input_window(20_000, 4096), 4096);
        assert_eq!(inflater_input_window(300, 4096), 300);

        let original: Vec<u8> = (0..20_000u32)
            .map(|i| ((i * 7) % 251) as u8 ^ (i / 300) as u8 ^ ((i * i) % 7) as u8)
            .collect();
        let room = original.len() + 64;
        let steps: &[&[usize]] = &[
            &[1],
            &[1, 1, 7, 1, 300, 1, 4096, 2],
            &[3, 1000, 1, 513],
            &[600],
            &[65_536],
        ];
        let check = |name: &str, nowrap: bool, compressed: &[u8], dict: Option<&[u8]>| {
            assert!(
                compressed.len() > 2 * INFLATER_INPUT_WINDOW_MIN,
                "{name}: too short to window"
            );
            for out_steps in steps {
                let whole = drive_inflate(false, nowrap, compressed, dict, out_steps, room);
                let windowed = drive_inflate(true, nowrap, compressed, dict, out_steps, room);
                assert_eq!(windowed, whole, "{name} {out_steps:?}");
                assert!(
                    whole.0.len() > 1 || out_steps[0] >= original.len(),
                    "{name}: several calls"
                );
            }
        };

        let mut raw = DeflateEncoder::new(Vec::new(), Compression::default());
        raw.write_all(&original).unwrap();
        let raw = raw.finish().unwrap();
        check("raw", true, &raw, None);
        assert_eq!(
            drive_inflate(true, true, &raw, None, &[1, 5], room).1,
            original
        );

        let mut wrapped = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        wrapped.write_all(&original).unwrap();
        let wrapped = wrapped.finish().unwrap();
        check("wrapped", false, &wrapped, None);

        let mut stored = DeflateEncoder::new(Vec::new(), Compression::none());
        stored.write_all(&original).unwrap();
        let stored = stored.finish().unwrap();
        check("stored", true, &stored, None);
        assert_eq!(
            drive_inflate(true, true, &stored, None, &[5000], room).1,
            original
        );

        let dict: Vec<u8> = (0..300u32).map(|i| ((i * 7) % 251) as u8).collect();
        let mut c = Compress::new(Compression::default(), true);
        c.set_dictionary(&dict).unwrap();
        let mut with_dict = Vec::with_capacity(original.len() * 2 + 1024);
        let status = c
            .compress_vec(&original, &mut with_dict, FlushCompress::Finish)
            .unwrap();
        assert_eq!(status, flate2::Status::StreamEnd);
        check("dictionary", false, &with_dict, Some(dict.as_slice()));
        assert_eq!(
            drive_inflate(
                true,
                false,
                &with_dict,
                Some(dict.as_slice()),
                &[1, 9],
                room
            )
            .1,
            original
        );

        let mut corrupt = raw.clone();
        let mid = corrupt.len() / 2;
        for b in &mut corrupt[mid..mid + 16] {
            *b ^= 0xA5;
        }
        for out_steps in steps {
            let whole = drive_inflate(false, true, &corrupt, None, out_steps, room);
            let windowed = drive_inflate(true, true, &corrupt, None, out_steps, room);
            assert_eq!(windowed, whole, "corrupt {out_steps:?}");
        }
    }

    /// Interpreter round i3 wave 74, lane L5 (`CRATONVM_INFLATER_RAW_ADLER_ZERO`):
    /// with the switch a raw (`nowrap`) stream answers `getAdler()` = 0 fresh,
    /// after inflating (through both buffer paths) and after `reset()` --
    /// zlib's raw `strm->adler` -- while the packed results and the bytes are
    /// the same as without it; a zlib-wrapped stream keeps its running
    /// checksum (the trailer's ADLER-32) either way; and without the switch a
    /// raw stream still answers the running checksum (seed 1).
    #[test]
    fn i74_raw_inflater_keeps_no_adler_with_the_switch() {
        let original: Vec<u8> = (0..9_000u32).map(|i| ((i * 13) % 241) as u8).collect();
        let mut raw = DeflateEncoder::new(Vec::new(), Compression::default());
        raw.write_all(&original).unwrap();
        let raw = raw.finish().unwrap();
        let mut wrapped = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        wrapped.write_all(&original).unwrap();
        let wrapped = wrapped.finish().unwrap();
        let expected_adler = adler32_update(1, &original) as i32;

        fn adler_of(ctx: &mut crate::test_utils::MockNativeContext, addr: i64) -> i32 {
            match infl_get_adler(ctx, &[Value::Long(addr)]).unwrap().unwrap() {
                Value::Int(a) => a,
                other => panic!("expected Int adler, got {other:?}"),
            }
        }

        /// (fresh adler, packed results, bytes, adler after, adler after reset)
        fn run(
            nowrap: bool,
            raw_adler_zero: bool,
            reuse: bool,
            stream: &[u8],
            n: usize,
        ) -> (i32, Vec<i64>, Vec<u8>, i32, i32) {
            let mut ctx = mock_ctx();
            let addr = inflater_init(nowrap, raw_adler_zero);
            let fresh = adler_of(&mut ctx, addr);
            let input = ctx.new_array(ArrayElementType::Byte, stream.len());
            for (i, b) in stream.iter().enumerate() {
                ctx.set_array_element(input, i, Value::Int(*b as i8 as i32));
            }
            let out = ctx.new_array(ArrayElementType::Byte, n + 16);
            let (mut in_pos, mut out_pos, mut packs) = (0usize, 0usize, Vec::new());
            for _ in 0..1_000 {
                let packed = match inflate_bytes_bytes(
                    &mut ctx,
                    &[
                        Value::Object(None),
                        Value::Long(addr),
                        Value::Object(Some(input)),
                        Value::Int(in_pos as i32),
                        Value::Int((stream.len() - in_pos).min(512) as i32),
                        Value::Object(Some(out)),
                        Value::Int(out_pos as i32),
                        Value::Int((n + 16 - out_pos).min(2048) as i32),
                    ],
                    reuse,
                )
                .unwrap()
                .unwrap()
                {
                    Value::Long(p) => p,
                    other => panic!("expected Long result, got {other:?}"),
                };
                packs.push(packed);
                let p = packed as u64;
                in_pos += (p & 0x7FFF_FFFF) as usize;
                out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
                if (p >> 62) & 1 == 1 {
                    break;
                }
            }
            let after = adler_of(&mut ctx, addr);
            let bytes = read_byte_array(&ctx, out, 0, out_pos);
            infl_reset(&mut ctx, &[Value::Long(addr)]).unwrap();
            let after_reset = adler_of(&mut ctx, addr);
            infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
            (fresh, packs, bytes, after, after_reset)
        }

        let n = original.len();
        for reuse in [false, true] {
            let off = run(true, false, reuse, &raw, n);
            assert_eq!(off.2, original);
            assert_eq!((off.0, off.3, off.4), (1, expected_adler, 1));
            let on = run(true, true, reuse, &raw, n);
            assert_eq!((on.0, on.3, on.4), (0, 0, 0));
            assert_eq!((&on.1, &on.2), (&off.1, &off.2));

            for raw_adler_zero in [false, true] {
                let w = run(false, raw_adler_zero, reuse, &wrapped, n);
                assert_eq!(w.2, original);
                assert_eq!((w.0, w.3, w.4), (1, expected_adler, 1));
            }
        }
    }

    /// Interpreter round i3 wave 74, lane L5 (`CRATONVM_ZIP_UTILS_INTRINSICS`):
    /// `get16` / `get32` answer the little-endian unsigned field in range (at
    /// the first and the last possible offset, with high bits set so a sign
    /// extension would show), and decline everything the JDK body must answer
    /// (negative offset, a range past the end by one, a null array).
    #[test]
    fn i74_zip_utils_reads_answer_the_little_endian_field() {
        let mut ctx = mock_ctx();
        let bytes: [u8; 6] = [0xFE, 0xCA, 0x81, 0x92, 0xA3, 0xF4];
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        let at = |off: i32| [Value::Object(Some(arr)), Value::Int(off)];
        assert_eq!(zip_utils_read_le(&ctx, &at(0), 2), Some(0xCAFE));
        assert_eq!(zip_utils_read_le(&ctx, &at(4), 2), Some(0xF4A3));
        assert_eq!(zip_utils_read_le(&ctx, &at(0), 4), Some(0x9281_CAFE));
        assert_eq!(zip_utils_read_le(&ctx, &at(2), 4), Some(0xF4A3_9281));
        assert_eq!(
            zip_utils_get16(&mut ctx, &at(4)).unwrap(),
            Some(Value::Int(0xF4A3))
        );
        assert_eq!(
            zip_utils_get32(&mut ctx, &at(2)).unwrap(),
            Some(Value::Long(0xF4A3_9281))
        );
        assert_eq!(zip_utils_read_le(&ctx, &at(5), 2), None);
        assert_eq!(zip_utils_read_le(&ctx, &at(3), 4), None);
        assert_eq!(zip_utils_read_le(&ctx, &at(-1), 2), None);
        assert_eq!(
            zip_utils_read_le(&ctx, &[Value::Object(None), Value::Int(0)], 2),
            None
        );
    }


    /// Interpreter round i3 wave 75, lane L5
    /// (`CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS`): every accessor in the table
    /// reads its own field. The headers are built field by field in the
    /// APPNOTE 4.3.7 / 4.3.12 order (no offsets written here: each field's
    /// offset is wherever the fields before it end), with a distinct value per
    /// field whose high bits are set, so a wrong offset, width or sign
    /// extension shows. The CEN header sits at a non-zero `pos`.
    #[test]
    fn i75_zip_header_accessors_read_their_own_field() {
        // (name, width, value) in header order.
        let cen_fields: [(&str, usize, u32); 16] = [
            ("CENSIG", 4, 0x0201_4B50),
            ("CENVEM", 2, 0x83A1),
            ("CENVER", 2, 0x8414),
            ("CENFLG", 2, 0x8808),
            ("CENHOW", 2, 0x8C08),
            ("CENTIM", 4, 0x9A5B_C3D7),
            ("CENCRC", 4, 0xDEAD_BEEF),
            ("CENSIZ", 4, 0xF100_0601),
            ("CENLEN", 4, 0xF200_0FDD),
            ("CENNAM", 2, 0x9017),
            ("CENEXT", 2, 0x9124),
            ("CENCOM", 2, 0x9235),
            ("CENDSK", 2, 0x9346),
            ("CENATT", 2, 0x9457),
            ("CENATX", 4, 0xA1ED_C0DE),
            ("CENOFF", 4, 0xF3AB_CDEF),
        ];
        let loc_fields: [(&str, usize, u32); 10] = [
            ("LOCSIG", 4, 0x0403_4B50),
            ("LOCVER", 2, 0x8014),
            ("LOCFLG", 2, 0x8108),
            ("LOCHOW", 2, 0x8208),
            ("LOCTIM", 4, 0x9A5B_C3D7),
            ("LOCCRC", 4, 0xCAFE_F00D),
            ("LOCSIZ", 4, 0xF400_0601),
            ("LOCLEN", 4, 0xF500_0FDD),
            ("LOCNAM", 2, 0x9617),
            ("LOCEXT", 2, 0x9724),
        ];
        let lay_out = |prefix: usize, fields: &[(&str, usize, u32)]| {
            let mut bytes = vec![0x5Au8; prefix];
            let mut at = std::collections::HashMap::new();
            for (name, width, value) in fields {
                at.insert(name.to_string(), (bytes.len() - prefix, *width, *value));
                bytes.extend_from_slice(&value.to_le_bytes()[..*width]);
            }
            bytes.extend_from_slice(&[0xA5; 3]);
            (bytes, at)
        };
        const POS: i32 = 7;
        let (cen, cen_at) = lay_out(POS as usize, &cen_fields);
        assert_eq!(
            cen.len(),
            POS as usize + 46 + 3,
            "a CEN header is CENHDR = 46 bytes"
        );
        let (loc, loc_at) = lay_out(0, &loc_fields);
        assert_eq!(loc.len(), 30 + 3, "a LOC header is LOCHDR = 30 bytes");

        let mut ctx = mock_ctx();
        let cen_arr = byte_array(&mut ctx, &cen);
        let loc_arr = byte_array(&mut ctx, &loc);
        assert_eq!(ZIP_HEADER_ACCESSORS.len(), 21);
        for (f, callback) in ZIP_HEADER_ACCESSORS {
            let expected = match f.name {
                // `Byte.toUnsignedInt(b[pos + 5])`: the high byte of CENVEM.
                "CENVEM_FA" => cen_at["CENVEM"].2 >> 8,
                // `get16(b, pos + 40)`: the high half of CENATX.
                "CENATX_PERMS" => cen_at["CENATX"].2 >> 16,
                name if f.takes_pos() => cen_at[name].2,
                name => loc_at[name].2,
            };
            let width = match f.name {
                "CENVEM_FA" => 1,
                "CENATX_PERMS" => 2,
                name if f.takes_pos() => cen_at[name].1,
                name => loc_at[name].1,
            };
            assert_eq!(f.width, width, "{} width", f.name);
            let args = if f.takes_pos() {
                vec![Value::Object(Some(cen_arr)), Value::Int(POS)]
            } else {
                vec![Value::Object(Some(loc_arr))]
            };
            let want = if f.returns_long() {
                Value::Long(i64::from(expected))
            } else {
                Value::Int(expected as i32)
            };
            assert_eq!(f.answer(&ctx, &args), Some(want), "{}", f.name);
            assert_eq!(
                callback(&mut ctx, &args).unwrap(),
                Some(want),
                "{} callback",
                f.name
            );
            // The descriptor's return type is the JDK's: get32 fields are
            // `long`, get16 / byte fields `int`.
            assert_eq!(f.returns_long(), width == 4, "{} return type", f.name);
        }
    }

    /// The header accessors decline (the JDK body answers) exactly where the
    /// JDK body would throw: a null array, a negative `pos + k`, a `pos + k`
    /// that wraps past `Integer.MAX_VALUE`, a field past the end by one byte.
    #[test]
    fn i75_zip_header_accessors_decline_what_the_jdk_body_throws() {
        let mut ctx = mock_ctx();
        let cen = byte_array(&mut ctx, &[0x11; 46]);
        let field = |name: &str| {
            ZIP_HEADER_ACCESSORS
                .iter()
                .find(|(f, _)| f.name == name)
                .map(|(f, _)| *f)
                .unwrap()
        };
        let at = |pos: i32| vec![Value::Object(Some(cen)), Value::Int(pos)];
        let off = field("CENOFF");
        assert_eq!(off.answer(&ctx, &at(0)), Some(Value::Long(0x1111_1111)));
        assert_eq!(off.answer(&ctx, &at(1)), None, "42 + 4 > 46 - 1");
        assert_eq!(off.answer(&ctx, &at(-42)), Some(Value::Long(0x1111_1111)));
        assert_eq!(off.answer(&ctx, &at(-43)), None, "pos + 42 < 0");
        assert_eq!(
            off.answer(&ctx, &at(i32::MAX)),
            None,
            "pos + 42 wraps negative"
        );
        let fa = field("CENVEM_FA");
        assert_eq!(fa.answer(&ctx, &at(40)), Some(Value::Int(0x11)));
        assert_eq!(fa.answer(&ctx, &at(41)), None, "b[46] is past the end");
        assert_eq!(
            off.answer(&ctx, &[Value::Object(None), Value::Int(0)]),
            None,
            "null array"
        );
        let short_loc = byte_array(&mut ctx, &[0x22; 29]);
        assert_eq!(
            field("LOCEXT").answer(&ctx, &[Value::Object(Some(short_loc))]),
            None,
            "LOCEXT reads bytes 28..30 of a 29-byte array"
        );
        assert_eq!(
            field("LOCNAM").answer(&ctx, &[Value::Object(Some(short_loc))]),
            Some(Value::Int(0x2222))
        );
    }


/// Interpreter round i3 wave 75, lane L5
    /// (`CRATONVM_ZIP_CODER_COMPARE_INTRINSIC`): `UTF8ZipCoder.compare`'s
    /// answer for ASCII names, worked through the JDK body's
    /// `Arrays.mismatch` / `hasTrailingSlash` rule by hand, and the cases left
    /// to the JDK body.
    #[test]
    fn i75_zip_coder_compare_answers_the_jdk_bodys_byte() {
        let mut ctx = mock_ctx();
        // A "CEN" with the encoded names at offsets 3 ("a/b"), 6 ("a/b/"),
        // 10 ("ab/").
        let cen = byte_array(&mut ctx, b"xyza/ba/b/ab/");
        let compare = |ctx: &mut crate::test_utils::MockNativeContext,
                       name: &str,
                       off: i32,
                       len: i32,
                       dir: bool| {
            let s = ctx.create_string(name);
            let args = [
                Value::Object(None), // the coder; unused
                Value::Object(Some(s)),
                Value::Object(Some(cen)),
                Value::Int(off),
                Value::Int(len),
                Value::Int(i32::from(dir)),
            ];
            zip_coder_compare_answer(&*ctx, &args)
        };
        let (exact, directory, none) = (
            Some(ZIP_CODER_EXACT_MATCH),
            Some(ZIP_CODER_DIRECTORY_MATCH),
            Some(ZIP_CODER_NO_MATCH),
        );
        assert_eq!(compare(&mut ctx, "a/b", 3, 3, true), exact);
        assert_eq!(compare(&mut ctx, "a/b", 3, 3, false), exact);
        // "a/b" against "a/b/": mismatch = 3 = len - 1, trailing '/'.
        assert_eq!(compare(&mut ctx, "a/b", 6, 4, true), directory);
        assert_eq!(compare(&mut ctx, "a/b", 6, 4, false), none);
        // "a/b/" against "a/b": a proper prefix the other way round.
        assert_eq!(compare(&mut ctx, "a/b/", 3, 3, true), none);
        // The JDK quirk: "abc" against "ab/" -- same length, the last byte
        // differs (mismatch = 2 = len - 1) and it is '/'.
        assert_eq!(compare(&mut ctx, "abc", 10, 3, true), directory);
        assert_eq!(compare(&mut ctx, "abc", 10, 3, false), none);
        assert_eq!(compare(&mut ctx, "abd", 3, 3, true), none);
        // Empty against empty: equal lengths, no difference.
        assert_eq!(compare(&mut ctx, "", 2, 0, true), exact);
        assert_eq!(
            compare(&mut ctx, "", 3, 1, true),
            none,
            "\"\" vs \"a\": mismatch 0, len 1, no '/'"
        );
        // Left to the JDK body.
        assert_eq!(compare(&mut ctx, "a/\u{e9}", 3, 3, true), None, "non-ASCII");
        assert_eq!(compare(&mut ctx, "a/b", -1, 3, true), None, "off < 0");
        assert_eq!(compare(&mut ctx, "a/b", 3, -1, true), None, "len < 0");
        assert_eq!(compare(&mut ctx, "a/b", 11, 3, true), None, "past the end");
        assert_eq!(
            compare(&mut ctx, "a/b", i32::MAX, 3, true),
            None,
            "off + len overflows"
        );
        let null_name = [
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(cen)),
            Value::Int(3),
            Value::Int(3),
            Value::Int(1),
        ];
        assert_eq!(
            zip_coder_compare_answer(&ctx, &null_name),
            None,
            "null name"
        );

        // A name longer than the on-stack window.
        let long: String = (0..300).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut bytes = long.clone().into_bytes();
        bytes.push(b'/');
        let long_cen = byte_array(&mut ctx, &bytes);
        let s = ctx.create_string(&long);
        let args = |len: i32, dir: bool| {
            [
                Value::Object(None),
                Value::Object(Some(s)),
                Value::Object(Some(long_cen)),
                Value::Int(0),
                Value::Int(len),
                Value::Int(i32::from(dir)),
            ]
        };
        assert_eq!(zip_coder_compare_answer(&ctx, &args(300, true)), exact);
        assert_eq!(zip_coder_compare_answer(&ctx, &args(301, true)), directory);
        assert_eq!(zip_coder_compare_answer(&ctx, &args(301, false)), none);
    }

    fn int_array(ctx: &mut dyn NativeContext, values: &[i32]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Int, values.len());
        for (i, v) in values.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*v));
        }
        arr
    }

    /// A `ZipFile$Source` stand-in on the mock: the class with its four walk
    /// fields declared at slots 0..4 (the JDK descriptors), and `table` /
    /// `entries` filled the way JDK 25's `initCEN` fills them: `tablelen =
    /// (total / 2) | 1`, entry `i` at `3 * i` as (hash, next, pos), each entry
    /// pushed at the head of its chain. Answers (receiver, table, entries).
    fn zip_source(
        ctx: &mut crate::test_utils::MockNativeContext,
        names: &[&str],
    ) -> (ObjectRef, ObjectRef, ObjectRef) {
        let cid = ctx
            .ensure_class_initialized(ZIP_SOURCE)
            .expect("Source class");
        let field =
            |name: &str, descriptor: &str, slot_index: usize| cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0x0002,
                slot_index,
                declaring_class_id: cid,
                is_static: false,
            };
        ctx.set_declared_fields(
            cid,
            vec![
                field("total", "I", 0),
                field("table", "[I", 1),
                field("tablelen", "I", 2),
                field("entries", "[I", 3),
            ],
        );
        let total = names.len();
        let tablelen = (total / 2) | 1;
        let mut table = vec![ZIP_ENDCHAIN; tablelen];
        let mut entries = vec![0i32; 3 * total];
        for (i, name) in names.iter().enumerate() {
            let units: Vec<u16> = name.encode_utf16().collect();
            let hash = zip_coder_hash(&units);
            let head = ((hash & 0x7fff_ffff) as usize) % tablelen;
            entries[3 * i] = hash;
            entries[3 * i + 1] = table[head];
            entries[3 * i + 2] = (46 * i) as i32;
            table[head] = (3 * i) as i32;
        }
        let table = int_array(ctx, &table);
        let entries = int_array(ctx, &entries);
        let this = ctx.alloc_object(cid, 4);
        ctx.set_field(this, 0, Value::Int(total as i32));
        ctx.set_field(this, 1, Value::Object(Some(table)));
        ctx.set_field(this, 2, Value::Int(tablelen as i32));
        ctx.set_field(this, 3, Value::Object(Some(entries)));
        (this, table, entries)
    }

    /// `getEntryPos(name, true, coder)`'s arguments for `this` and `name`.
    fn entry_pos_args(
        ctx: &mut crate::test_utils::MockNativeContext,
        this: ObjectRef,
        name: Option<&str>,
    ) -> [Value; 4] {
        let name = name.map(|n| ctx.create_string_uninterned(n));
        [
            Value::Object(Some(this)),
            Value::Object(name),
            Value::Int(1),
            Value::Object(None),
        ]
    }

    /// Interpreter round i3 wave 76, lane L5
    /// (`CRATONVM_ZIP_ENTRY_MISS_INTRINSIC`): `ZipCoder.hash` is
    /// `String.hashCode` plus a trailing `/` when the name has none, so a name
    /// and its directory form hash alike (the JDK's own design: `getEntry("a")`
    /// finds `a/` through the same chain).
    #[test]
    fn i76_zip_coder_hash_is_the_jdks() {
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        assert_eq!(hash(""), 0);
        // "a/".hashCode() = 97 * 31 + 47.
        assert_eq!(hash("a/"), 3054);
        assert_eq!(hash("a"), 3054);
        // "Aa" and "BB" share a String.hashCode, and so every name built
        // around them in the same place.
        assert_eq!(hash("q/AaX.class"), hash("q/BBX.class"));
        // A long name wraps as Java's int arithmetic does.
        let long: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let java = long
            .encode_utf16()
            .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)));
        assert_eq!(hash(&long), java.wrapping_mul(31).wrapping_add(47));
        // Non-ASCII units hash as UTF-16 units.
        assert_eq!(hash("\u{e9}/"), 0xe9 * 31 + 47);
    }

    /// The intrinsic answers `null` exactly for the names whose hash no
    /// stored entry shares (a model check: a chain holds every entry of its
    /// hash, so "no match on the chain" is "no stored name hashes alike"), and
    /// declines every hash hit -- an exact name, the directory form `q0` of
    /// `q0/`, a String.hashCode collision -- for the JDK body to compare.
    #[test]
    fn i76_zip_entry_miss_answers_only_the_no_hash_match_miss() {
        let mut ctx = mock_ctx();
        let stored = [
            "META-INF/MANIFEST.MF",
            "q0/",
            "q0/P1.class",
            "q0/AaX.class",
            "q0/P2.class",
            "dir/",
            "dir/x.txt",
            "\u{e9}t\u{e9}.txt",
        ];
        let (this, _, _) = zip_source(&mut ctx, &stored);
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        for name in stored {
            let args = entry_pos_args(&mut ctx, this, Some(name));
            assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "stored {name:?}");
        }
        for hit in ["q0", "dir", "q0/BBX.class"] {
            let args = entry_pos_args(&mut ctx, this, Some(hit));
            assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "hash hit {hit:?}");
        }
        let probes = [
            "q0/P3.class",
            "q1/P1.class",
            "q0/P1.clas",
            "META-INF/",
            "META-INF/INDEX.LIST",
            "dir/y.txt",
            "",
            "/",
            "\u{e9}t\u{e9}",
            "java/lang/Object.class",
        ];
        let mut answered = 0;
        for name in probes {
            let model_miss = !stored.iter().any(|s| hash(*s) == hash(name));
            let args = entry_pos_args(&mut ctx, this, Some(name));
            let got = zip_entry_pos_miss(&ctx, &args).is_some();
            assert_eq!(got, model_miss, "probe {name:?}");
            answered += usize::from(got);
        }
        assert!(
            answered >= 5,
            "the probes must exercise the answered path: {answered}"
        );

        // The callback answers `null` itself on a miss.
        let args = entry_pos_args(&mut ctx, this, Some("q0/P3.class"));
        assert_eq!(
            zip_source_get_entry_pos(&mut ctx, &args).unwrap(),
            Some(Value::Object(None))
        );

        // A null name with entries: the JDK body's NPE.
        let args = entry_pos_args(&mut ctx, this, None);
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "null name");
    }

    /// `total == 0` answers `null` before the name is read (a null name too);
    /// a closed source, `tablelen == 0`, an out-of-range chain index and a
    /// chain cycle all run the JDK body.
    #[test]
    fn i76_zip_entry_miss_declines_what_the_body_must_answer() {
        let mut ctx = mock_ctx();
        let (empty, _, _) = zip_source(&mut ctx, &[]);
        let args = entry_pos_args(&mut ctx, empty, None);
        assert_eq!(
            zip_entry_pos_miss(&ctx, &args),
            Some(()),
            "empty, null name"
        );
        let args = entry_pos_args(&mut ctx, empty, Some("a"));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), Some(()), "empty");

        // One entry: tablelen 1, so every name walks the one chain.
        let (one, table, entries) = zip_source(&mut ctx, &["a/b"]);
        let args = entry_pos_args(&mut ctx, one, Some("c/d"));
        assert_eq!(
            zip_entry_pos_miss(&ctx, &args),
            Some(()),
            "well-formed miss"
        );

        // A chain cycle: the entry's `next` names itself.
        ctx.set_array_element(entries, 1, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "cycle");
        ctx.set_array_element(entries, 1, Value::Int(ZIP_ENDCHAIN));
        // A next index past the end, and a negative one.
        ctx.set_array_element(entries, 1, Value::Int(3));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "next past the end");
        ctx.set_array_element(entries, 1, Value::Int(-7));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "negative next");
        ctx.set_array_element(entries, 1, Value::Int(ZIP_ENDCHAIN));
        // A head index past the end.
        ctx.set_array_element(table, 0, Value::Int(30));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "head past the end");
        ctx.set_array_element(table, 0, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), Some(()), "restored");

        // `tablelen == 0`: the body's ArithmeticException.
        ctx.set_field(one, 2, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "tablelen 0");
        ctx.set_field(one, 2, Value::Int(1));
        // Closed (`Source.close` nulls `table` and `entries`).
        ctx.set_field(one, 1, Value::Object(None));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "closed");
    }


    // -----------------------------------------------------------------------
    // CRATONVM_ZIP_ENTRY_HIT_INTRINSIC (interpreter round i3 wave 77, lane L5)
    // -----------------------------------------------------------------------

    /// A `ZipFile$Source` with a real CEN: entry `i` is a 46-byte header
    /// (`CENNAM` at +28) followed by its name's bytes, `entries[3i + 2]` its
    /// CEN position; the class declares `cen` at slot 4 besides the walk's
    /// four fields. Also the `UTF8ZipCoder` class (and one coder object) and
    /// the `ZipFile$EntryPos` record class (`name` slot 0, `pos` slot 1).
    /// Answers (receiver, coder, CEN positions).
    fn zip_source_with_cen(
        ctx: &mut crate::test_utils::MockNativeContext,
        names: &[&str],
    ) -> (ObjectRef, ObjectRef, Vec<i32>) {
        let (this, _, entries) = zip_source(ctx, names);
        let cid = ctx.class_id_of_object(this);
        let field = |name: &str, descriptor: &str, slot_index: usize, owner: ClassId| {
            cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0x0002,
                slot_index,
                declaring_class_id: owner,
                is_static: false,
            }
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("total", "I", 0, cid),
                field("table", "[I", 1, cid),
                field("tablelen", "I", 2, cid),
                field("entries", "[I", 3, cid),
                field("cen", "[B", 4, cid),
            ],
        );
        let mut cen = Vec::new();
        let mut positions = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let pos = cen.len();
            positions.push(pos as i32);
            let mut header = [0u8; 46];
            header[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            cen.extend_from_slice(&header);
            cen.extend_from_slice(name.as_bytes());
            ctx.set_array_element(entries, 3 * i + 2, Value::Int(pos as i32));
        }
        let cen = byte_array(ctx, &cen);
        // `zip_source` allocated four slots; a fifth for `cen`.
        let wide = ctx.alloc_object(cid, 5);
        for slot in 0..4 {
            let v = ctx.get_field(this, slot);
            ctx.set_field(wide, slot, v);
        }
        ctx.set_field(wide, 4, Value::Object(Some(cen)));
        let coder_cid = ctx
            .ensure_class_initialized(ZIP_CODER_UTF8)
            .expect("coder class");
        let coder = ctx.alloc_object(coder_cid, 0);
        let pos_cid = ctx
            .ensure_class_initialized(ZIP_ENTRY_POS)
            .expect("EntryPos class");
        ctx.set_declared_fields(
            pos_cid,
            vec![
                field("name", "Ljava/lang/String;", 0, pos_cid),
                field("pos", "I", 1, pos_cid),
            ],
        );
        (wide, coder, positions)
    }

    fn hit_args(
        ctx: &mut crate::test_utils::MockNativeContext,
        this: ObjectRef,
        name: &str,
        add_slash: bool,
        coder: Option<ObjectRef>,
    ) -> [Value; 4] {
        let name = ctx.create_string_uninterned(name);
        [
            Value::Object(Some(this)),
            Value::Object(Some(name)),
            Value::Int(i32::from(add_slash)),
            Value::Object(coder),
        ]
    }

    /// The hit answer: an exact ASCII name is `Exact(its CEN position)`, a
    /// `String.hashCode` collision continues the walk (to the exact entry, or
    /// to `null`), a miss is `null`; the directory form, a non-ASCII name and
    /// a coder of another class decline. Without `hits` every hash match
    /// declines, as the wave-76 miss intrinsic does.
    #[test]
    fn i77_the_hit_walk_answers_exact_matches_and_collisions() {
        let mut ctx = mock_ctx();
        let stored = [
            "META-INF/MANIFEST.MF",
            "q0/",
            "q0/P1.class",
            "q0/AaX.class",
            "q0/BBX.class",
            "\u{e9}.txt",
        ];
        let (this, coder, positions) = zip_source_with_cen(&mut ctx, &stored);
        for (i, name) in stored.iter().enumerate().take(5) {
            let args = hit_args(&mut ctx, this, name, true, Some(coder));
            assert_eq!(
                zip_entry_pos_walk(&ctx, &args, true),
                EntryPosWalk::Exact(positions[i]),
                "{name:?}"
            );
            assert_eq!(
                zip_entry_pos_walk(&ctx, &args, false),
                EntryPosWalk::Decline,
                "{name:?} no hits"
            );
        }
        // "q0/AaX.class" and "q0/BBX.class" share a hash, so one chain: the
        // loop above found each, the one behind the other by continuing past
        // a NO_MATCH. With only one of them stored, the other walks past it to
        // `null`.
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        assert_eq!(hash("q0/AaX.class"), hash("q0/BBX.class"));
        let (lone, lone_coder, _) = zip_source_with_cen(&mut ctx, &["q0/AaX.class", "q0/P1.class"]);
        let args = hit_args(&mut ctx, lone, "q0/BBX.class", true, Some(lone_coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "collision, not stored"
        );
        let args = hit_args(&mut ctx, this, "q0/BBX.clas", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "plain miss"
        );
        // `getEntry("q0")` finds the directory "q0/": DIRECTORY_MATCH declines.
        let args = hit_args(&mut ctx, this, "q0", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "directory"
        );
        // ... and without `addSlash` (getInputStream's lookup) it is no match.
        let args = hit_args(&mut ctx, this, "q0", false, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "no addSlash"
        );
        // A non-ASCII name: the JDK encodes it.
        let args = hit_args(&mut ctx, this, "\u{e9}.txt", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "non-ASCII"
        );
        // Another coder class, or none.
        let other_cid = ctx
            .ensure_class_initialized("java/util/zip/ZipCoder")
            .expect("ZipCoder");
        let other = ctx.alloc_object(other_cid, 0);
        let args = hit_args(&mut ctx, this, "q0/P1.class", true, Some(other));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "other coder"
        );
        let args = hit_args(&mut ctx, this, "q0/P1.class", true, None);
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "null coder"
        );
    }

    /// A corrupt CEN (`CENNAM` past the end, `entries[idx + 2]` past the end)
    /// and a closed `cen` decline; the callback builds the record with the
    /// caller's own `name` and the position.
    #[test]
    fn i77_the_hit_callback_builds_the_record_and_declines_corrupt_input() {
        let mut ctx = mock_ctx();
        let (this, coder, positions) = zip_source_with_cen(&mut ctx, &["a/B.class", "a/C.class"]);
        let args = hit_args(&mut ctx, this, "a/C.class", true, Some(coder));
        let got = zip_source_get_entry_pos_hits(&mut ctx, &args).expect("answered");
        let Some(Value::Object(Some(record))) = got else {
            panic!("expected an EntryPos, got {got:?}");
        };
        let pos_cid = ctx.class_id_by_name(ZIP_ENTRY_POS).expect("EntryPos");
        assert_eq!(ctx.class_id_of_object(record), pos_cid);
        assert_eq!(ctx.get_field(record, 0), args[1], "the caller's own name");
        assert_eq!(ctx.get_field(record, 1), Value::Int(positions[1]));
        let miss = hit_args(&mut ctx, this, "a/D.class", true, Some(coder));
        assert_eq!(
            zip_source_get_entry_pos_hits(&mut ctx, &miss).expect("answered"),
            Some(Value::Object(None))
        );

        // `entries[idx + 2]` (the CEN position) pointing past the CEN.
        let Value::Object(Some(entries)) = ctx.get_field(this, 3) else {
            panic!("entries");
        };
        ctx.set_array_element(entries, 5, Value::Int(10_000));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "CENNAM out of range"
        );
        ctx.set_array_element(entries, 5, Value::Int(-30));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "negative position"
        );
        ctx.set_array_element(entries, 5, Value::Int(positions[1]));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Exact(positions[1]),
            "restored"
        );
        // A closed source's `cen`.
        ctx.set_field(this, 4, Value::Object(None));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "null cen"
        );
    }


    /// Corrupt deflate input must FAIL, not answer "no progress".
    ///
    /// All four `Inflater.inflate*` natives are declared `throws
    /// DataFormatException` and `Inflater.inflate` has no Java-level check
    /// that converts a zero-progress packed result into an exception, so a
    /// swallowed decode error reaches the caller as `inflate() == 0` — a
    /// fabricated success. `RJdkJni.zipNatives:165` asserts the throw.
    ///
    /// The mock context cannot construct a real
    /// `java.util.zip.DataFormatException`, so what is asserted here is the
    /// half that is testable off-VM: the call must be `Err`, never `Ok`. The
    /// exception CLASS is chosen in `throw_data_format`.
    #[test]
    fn corrupt_input_fails_instead_of_reporting_no_progress() {
        let mut ctx = mock_ctx();
        // nowrap = 0 → a zlib-wrapped stream is expected. {1,2,...} has
        // CM = 1 (not 8) and a bad two-byte header check, so zlib rejects it
        // before producing a single byte.
        let addr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(addr) => addr,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let corrupt: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let input_arr = ctx.new_array(ArrayElementType::Byte, corrupt.len());
        for (i, b) in corrupt.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 64);
        let result = infl_inflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(corrupt.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(64),
            ],
        );
        assert!(
            result.is_err(),
            "corrupt deflate input must raise, not answer no-progress"
        );

        // A VALID stream through the same path must still succeed — the guard
        // has to reject corrupt input without rejecting good input.
        let original = b"hello hello hello hello world world";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let good = enc.finish().unwrap();
        let addr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(addr) => addr,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let input_arr = ctx.new_array(ArrayElementType::Byte, good.len());
        for (i, b) in good.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i8 as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 256);
        let packed = match infl_inflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(good.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(256),
            ],
        )
        .expect("valid zlib stream must inflate")
        .expect("inflate returns a long")
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long result, got {other:?}"),
        };
        let produced = ((packed >> 31) & 0x7FFF_FFFF) as usize;
        assert_eq!(read_byte_array(&ctx, output_arr, 0, produced), original);
        assert!((packed >> 62) & 1 == 1, "valid stream must report finished");
    }

    #[test]
    fn round_trip_inflate_raw() {
        let original = b"hello hello hello hello hello world world world";

        // Produce raw DEFLATE (no zlib header) of the payload.
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();

        // init with nowrap=true (raw deflate).
        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(&compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], original);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    #[test]
    fn crc32_matches_known_vectors() {
        // Empty input — running state starts at 0xFFFFFFFF (the inner step
        // takes the complemented form), no bytes processed, result equals
        // the input.
        assert_eq!(crc32_step(0xFFFF_FFFF, b""), 0xFFFF_FFFF);

        // "123456789" — classic CRC-32/IEEE check vector is 0xCBF43926
        // (final, complemented). Internally the running state at the end is
        // !0xCBF43926 = 0x340BC6D9.
        let running = crc32_step(0xFFFF_FFFF, b"123456789");
        assert_eq!(!running, 0xCBF4_3926);

        // "abc" — CRC-32/IEEE = 0x352441C2.
        let running = crc32_step(0xFFFF_FFFF, b"abc");
        assert_eq!(!running, 0x3524_41C2);
    }

    /// `crc32_update_public` matches the JDK `CRC32.update*` contract:
    /// public CRC in, public CRC out (initial state 0, no caller-side
    /// complementation). Regression guard for the DaCapo `avrora`
    /// "invalid entry CRC" crash where the native was treating the
    /// JDK-public `int crc` field as the complemented running state.
    #[test]
    fn crc32_public_contract_matches_jdk() {
        // Fresh CRC32 starts at 0; getValue() must return 0 (matches real JDK).
        assert_eq!(crc32_update_public(0, b""), 0);

        // "123456789" → 0xCBF43926 (canonical IEEE check vector, also what
        // real JDK 25 java.util.zip.CRC32 returns).
        assert_eq!(crc32_update_public(0, b"123456789"), 0xCBF4_3926);

        // "abc" → 0x352441C2.
        assert_eq!(crc32_update_public(0, b"abc"), 0x3524_41C2);

        // Chained call must equal one-shot.
        let half = crc32_update_public(0, b"1234");
        assert_eq!(crc32_update_public(half, b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn pack_layout_matches_jdk_unpacking() {
        // JDK unpacks:
        //   inputConsumed  = (packed & 0x7FFFFFFF)
        //   outputConsumed = ((packed >>> 31) & 0x7FFFFFFF)
        //   finished       = ((packed >>> 62) & 1) != 0
        //   needDict       = ((packed >>> 63) & 1) != 0
        let p = pack_inflate_result(42, 1000, true, false) as u64;
        assert_eq!(p & 0x7FFF_FFFF, 42);
        assert_eq!((p >> 31) & 0x7FFF_FFFF, 1000);
        assert_eq!((p >> 62) & 1, 1);
        assert_eq!((p >> 63) & 1, 0);

        let p2 = pack_inflate_result(0x7FFF_FFFF, 0x7FFF_FFFF, false, true) as u64;
        assert_eq!(p2 & 0x7FFF_FFFF, 0x7FFF_FFFF);
        assert_eq!((p2 >> 31) & 0x7FFF_FFFF, 0x7FFF_FFFF);
        assert_eq!((p2 >> 62) & 1, 0);
        assert_eq!((p2 >> 63) & 1, 1);
    }

    #[test]
    fn deflater_matches_hotspot_for_a_large_json_string() {
        let mut body = Vec::with_capacity(10_002);
        body.push(b'[');
        body.extend(std::iter::repeat_n(b'a', 10_000));
        body.push(b']');

        let actual = defl_compress_finished(&body, 6, false)
            .expect("raw deflate compression should succeed");
        let expected = [
            0xed, 0xc1, 0x31, 0x0d, 0x00, 0x00, 0x0c, 0x03, 0x20, 0xa1, 0x4b, 0x8f, 0xf9, 0x37,
            0x51, 0x1f, 0x0d, 0x70, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x03, 0x52,
        ];
        assert_eq!(actual, expected);
    }

    /// Regression guard for the Jetty `compressionOfResponseToGetRequest`
    /// hang (`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`): a mid-stream `SYNC_FLUSH` (zlib flush code
    /// 2) must produce real compressed bytes immediately instead of being
    /// silently buffered until `FINISH`. A streaming gzip writer that waits
    /// for a flush to make progress before handing over more input would
    /// hang forever against the old buffer-until-FINISH implementation.
    #[test]
    fn deflate_sync_flush_produces_output_without_finish() {
        let mut ctx = mock_ctx();

        // nowrap=true (raw deflate, no zlib header) to match `Decompress::new(false)` below.
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        let original = b"hello hello hello hello hello world world world";
        let input_arr = ctx.new_array(ArrayElementType::Byte, original.len());
        for (i, b) in original.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);

        let packed = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(original.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(2), // SYNC_FLUSH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        let input_consumed = (packed & 0x7FFF_FFFF) as usize;
        let output_consumed = ((packed >> 31) & 0x7FFF_FFFF) as usize;
        assert_eq!(input_consumed, original.len());
        assert!(
            output_consumed > 0,
            "SYNC_FLUSH must flush compressed bytes immediately, not defer to FINISH"
        );

        let mut compressed = Vec::new();
        for i in 0..output_consumed {
            match ctx.get_array_element(output_arr, i) {
                Value::Int(v) => compressed.push(v as u8),
                other => panic!("expected byte, got {other:?}"),
            }
        }

        // Finish the stream with no further input and collect the tail.
        let empty_arr = ctx.new_array(ArrayElementType::Byte, 0);
        let output_arr2 = ctx.new_array(ArrayElementType::Byte, 1024);
        let packed2 = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(empty_arr)),
                Value::Int(0),
                Value::Int(0),
                Value::Object(Some(output_arr2)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        let output_consumed2 = ((packed2 >> 31) & 0x7FFF_FFFF) as usize;
        let finished2 = (packed2 >> 62) & 1 == 1;
        assert!(finished2, "FINISH flush must report stream completion");
        for i in 0..output_consumed2 {
            match ctx.get_array_element(output_arr2, i) {
                Value::Int(v) => compressed.push(v as u8),
                other => panic!("expected byte, got {other:?}"),
            }
        }

        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(&compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], &original[..]);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    /// Real `Deflater.deflate()` is documented as a no-op once FINISH has
    /// produced `Z_STREAM_END`: callers may call it again before `reset()`
    /// and must see 0 bytes consumed/produced with `finished` still true.
    /// Re-entering zlib past `Z_STREAM_END` is not part of its contract;
    /// guard against it explicitly rather than relying on the backend's
    /// behavior for an out-of-contract call.
    #[test]
    fn deflate_after_finish_is_a_no_op_not_a_reentry() {
        let mut ctx = mock_ctx();
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        let input_arr = ctx.new_array(ArrayElementType::Byte, 0);
        let output_arr = ctx.new_array(ArrayElementType::Byte, 64);
        let finish_args = |input, output| {
            vec![
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input)),
                Value::Int(0),
                Value::Int(0),
                Value::Object(Some(output)),
                Value::Int(0),
                Value::Int(64),
                Value::Int(4), // FINISH
                Value::Int(0),
            ]
        };

        let first = match defl_deflate_bytes_bytes(&mut ctx, &finish_args(input_arr, output_arr))
            .unwrap()
            .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        assert_eq!(
            (first >> 62) & 1,
            1,
            "first FINISH call must report finished"
        );

        // Calling deflate() again after finished must stay a clean no-op —
        // not an error, not a re-entry into zlib.
        let output_arr2 = ctx.new_array(ArrayElementType::Byte, 64);
        let second = match defl_deflate_bytes_bytes(&mut ctx, &finish_args(input_arr, output_arr2))
            .unwrap()
            .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        assert_eq!(
            second & 0x7FFF_FFFF,
            0,
            "post-finish call must consume no input"
        );
        assert_eq!(
            (second >> 31) & 0x7FFF_FFFF,
            0,
            "post-finish call must produce no output"
        );
        assert_eq!(
            (second >> 62) & 1,
            1,
            "post-finish call must still report finished"
        );
    }

    /// RFC 1950 §9 known-answer vectors for the rolling helper the Adler32
    /// natives (and the Deflater/Inflater `getAdler` bridges) share.
    #[test]
    fn adler32_known_vectors() {
        // Fresh state is 1 (s1 = 1, s2 = 0), never 0 — unlike CRC32.
        assert_eq!(adler32_update(1, b""), 1);
        // "abc": s1 = 1+97+98+99 = 295 = 0x127;
        //        s2 = 98+196+295 = 589 = 0x24D.
        assert_eq!(adler32_update(1, b"abc"), 0x024D_0127);
        // "123456789": the vector RJdkJni.zipNatives asserts.
        assert_eq!(adler32_update(1, b"123456789"), 0x091E_01DE);
        // Streaming in pieces must equal hashing the concatenation — this is
        // the property `Adler32.update` relies on across native calls.
        let split = adler32_update(adler32_update(1, b"1234"), b"56789");
        assert_eq!(split, 0x091E_01DE);
    }

    /// The three registered natives must reproduce those vectors through the
    /// `Value` boundary, byte-array reads included.
    #[test]
    fn adler32_natives_match_vectors() {
        let mut ctx = mock_ctx();

        // update(int adler, int b): 'a' from the seed => s1 = 98, s2 = 98.
        let one = adler32_update_int(&mut ctx, &[Value::Int(1), Value::Int(b'a' as i32)])
            .unwrap()
            .unwrap();
        assert_eq!(one, Value::Int(0x0062_0062));

        // Only the low 8 bits of `b` participate (JNI jbyte truncation).
        let masked = adler32_update_int(&mut ctx, &[Value::Int(1), Value::Int(0x1_0061)])
            .unwrap()
            .unwrap();
        assert_eq!(masked, Value::Int(0x0062_0062));

        // updateBytes(int adler, byte[] b, int off, int len) over "123456789"
        // embedded in a larger array, to exercise the off/len slice.
        let data = b"XX123456789XX";
        let arr = ctx.new_array(ArrayElementType::Byte, data.len());
        for (i, b) in data.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        let bytes = adler32_update_bytes(
            &mut ctx,
            &[
                Value::Int(1),
                Value::Object(Some(arr)),
                Value::Int(2),
                Value::Int(9),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(bytes, Value::Int(0x091E_01DE));

        // A null array is a no-op rather than a panic.
        let null_arr = adler32_update_bytes(
            &mut ctx,
            &[
                Value::Int(1),
                Value::Object(None),
                Value::Int(0),
                Value::Int(9),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(null_arr, Value::Int(1));
    }

    /// Copy `bytes` into a fresh mock `byte[]`.
    fn byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        arr
    }

    /// A zlib PRESET DICTIONARY must actually reach zlib, in both directions.
    ///
    /// Both `setDictionary` natives used to be no-ops, on the stated grounds
    /// that flate2's `set_dictionary` was "gated behind a zlib backend feature
    /// we don't enable" — untrue, `native-builtins/Cargo.toml` has always used
    /// `features = ["zlib"]`. SPDY compresses every header block with a preset
    /// dictionary, so `SpdyHeaderBlockZlibDecoder` handed the dictionary over,
    /// got silence, inflated 0 bytes and reported `Invalid Header Block`.
    ///
    /// Asserting a round trip through our OWN Deflater+Inflater would not have
    /// caught it: with both halves ignoring the dictionary the round trip is
    /// green and the bytes on the wire are still wrong. So the compressing
    /// half is pinned on the header's `FDICT` bit and the decompressing half on
    /// the `needDict` result bit — both of which are only reachable when the
    /// dictionary genuinely reaches zlib.
    #[test]
    fn set_dictionary_reaches_zlib_on_both_halves() {
        const DICT: &[u8] = b"optionsgetheadpostputdeletetraceacceptaccept-charset";
        let payload = b"accept-charset: utf-8 accept: text/html options get head post";

        let mut ctx = mock_ctx();

        // --- compressing half: the emitted zlib header must set FDICT -------
        let daddr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let dict_arr = byte_array(&mut ctx, DICT);
        defl_set_dictionary(
            &mut ctx,
            &[
                Value::Long(daddr),
                Value::Object(Some(dict_arr)),
                Value::Int(0),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let in_arr = byte_array(&mut ctx, payload);
        let out_arr = ctx.new_array(ArrayElementType::Byte, 512);
        let packed = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(daddr),
                Value::Object(Some(in_arr)),
                Value::Int(0),
                Value::Int(payload.len() as i32),
                Value::Object(Some(out_arr)),
                Value::Int(0),
                Value::Int(512),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        let (_, produced, _) = unpack_deflate_result(packed);
        assert!(produced > 2, "deflate produced nothing");
        let compressed = read_byte_array(&ctx, out_arr, 0, produced);

        // zlib header: CMF, FLG. FDICT is bit 5 of FLG, and a DICTID follows.
        assert_eq!(compressed[0] & 0x0f, 8, "expected deflate method in CMF");
        assert_ne!(
            compressed[1] & 0x20,
            0,
            "FDICT must be set — the dictionary never reached the compressor \
             (this is the bug: the stream is then unreadable by any peer that \
             expects the preset dictionary)"
        );

        // --- decompressing half: needDict, then progress after setDictionary -
        let iaddr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let cin = byte_array(&mut ctx, &compressed);
        let cout = ctx.new_array(ArrayElementType::Byte, 512);
        // `Inflater` keeps its own input cursor: the header bytes the first
        // call consumes must NOT be handed to the second one, or zlib sees a
        // second header mid-stream and raises DataFormatException.
        let inflate = |ctx: &mut dyn NativeContext, off: usize| -> (usize, usize, bool) {
            let p = match infl_inflate_bytes_bytes(
                ctx,
                &[
                    Value::Object(None),
                    Value::Long(iaddr),
                    Value::Object(Some(cin)),
                    Value::Int(off as i32),
                    Value::Int((compressed.len() - off) as i32),
                    Value::Object(Some(cout)),
                    Value::Int(0),
                    Value::Int(512),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Long(v) => v as u64,
                other => panic!("expected Long, got {other:?}"),
            };
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 63) & 1 == 1,
            )
        };

        let (consumed_first, produced_first, need_dict) = inflate(&mut ctx, 0);
        assert_eq!(
            produced_first, 0,
            "no output is possible before the dictionary"
        );
        assert!(
            need_dict,
            "inflate must report needDict for an FDICT stream"
        );

        let dict_arr2 = byte_array(&mut ctx, DICT);
        infl_set_dictionary(
            &mut ctx,
            &[
                Value::Long(iaddr),
                Value::Object(Some(dict_arr2)),
                Value::Int(0),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let (_, produced_second, _) = inflate(&mut ctx, consumed_first);
        assert_eq!(
            produced_second,
            payload.len(),
            "after setDictionary the stream must decode — a no-op setDictionary \
             leaves this at 0, which is what made SPDY report Invalid Header Block"
        );
        assert_eq!(read_byte_array(&ctx, cout, 0, produced_second), payload);
    }

    /// Deflate `payload` with `dict_addr` as a DIRECT-buffer preset dictionary
    /// (or with no dictionary at all when `dict_addr` is `None`), returning the
    /// compressed bytes.
    fn deflate_with_buffer_dictionary(
        ctx: &mut dyn NativeContext,
        payload: &[u8],
        dict: Option<(i64, usize)>,
    ) -> Vec<u8> {
        let addr = match defl_init(ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        if let Some((dict_addr, dict_len)) = dict {
            defl_set_dictionary_buffer(
                ctx,
                &[
                    Value::Long(addr),
                    Value::Long(dict_addr),
                    Value::Int(dict_len as i32),
                ],
            )
            .unwrap();
        }
        let in_arr = byte_array(ctx, payload);
        let out_arr = ctx.new_array(ArrayElementType::Byte, 512);
        let packed = match defl_deflate_bytes_bytes(
            ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(in_arr)),
                Value::Int(0),
                Value::Int(payload.len() as i32),
                Value::Object(Some(out_arr)),
                Value::Int(0),
                Value::Int(512),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        let (_, produced, _) = unpack_deflate_result(packed);
        read_byte_array(ctx, out_arr, 0, produced)
    }

    /// The DIRECT-`ByteBuffer` `setDictionary` overloads must reach zlib too.
    ///
    /// These two stayed no-ops when the heap overloads were fixed, on the
    /// stated grounds that "the direct-buffer inflate natives are themselves
    /// unsupported (`infl_direct_buffer_unsupported`), so a caller cannot get
    /// far enough to need this". No such function exists: all four `inflate*`
    /// and all four `deflate*` overloads read and write direct-buffer memory
    /// for real, so the gap was reachable — the same wrong-on-the-wire stream
    /// SPDY tripped over, reached through `setDictionary(ByteBuffer)`.
    ///
    /// Pinned on FDICT and on the compressed LENGTH against a no-dictionary
    /// control, for the same reason as the heap test: a round trip through our
    /// own encoder is green either way.
    #[test]
    fn set_dictionary_buffer_reaches_zlib_on_both_halves() {
        const DICT: &[u8] = b"optionsgetheadpostputdeletetraceacceptaccept-charset";
        let payload = b"accept-charset: utf-8 accept: text/html options get head post";

        let mut ctx = mock_ctx();

        // A direct ByteBuffer's payload is plain native memory as far as the
        // native is concerned; a `Vec`'s backing store is the same thing here,
        // and `MockNativeContext` copies from real pointers.
        let dict_mem = DICT.to_vec();
        let dict_addr = dict_mem.as_ptr() as i64;

        let plain = deflate_with_buffer_dictionary(&mut ctx, payload, None);
        let with_dict =
            deflate_with_buffer_dictionary(&mut ctx, payload, Some((dict_addr, DICT.len())));

        assert_ne!(
            with_dict[1] & 0x20,
            0,
            "FDICT must be set — the direct-buffer dictionary never reached the \
             compressor"
        );
        assert!(
            with_dict.len() < plain.len(),
            "a preset dictionary must shrink this payload: {} bytes with the \
             dictionary vs {} without (equal lengths mean the no-op is still \
             there)",
            with_dict.len(),
            plain.len()
        );

        // --- decompressing half -------------------------------------------
        let iaddr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let cin = byte_array(&mut ctx, &with_dict);
        let cout = ctx.new_array(ArrayElementType::Byte, 512);
        let mut inflate = |ctx: &mut dyn NativeContext, off: usize| -> (usize, usize, bool) {
            let p = match infl_inflate_bytes_bytes(
                ctx,
                &[
                    Value::Object(None),
                    Value::Long(iaddr),
                    Value::Object(Some(cin)),
                    Value::Int(off as i32),
                    Value::Int((with_dict.len() - off) as i32),
                    Value::Object(Some(cout)),
                    Value::Int(0),
                    Value::Int(512),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Long(v) => v as u64,
                other => panic!("expected Long, got {other:?}"),
            };
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 63) & 1 == 1,
            )
        };

        let (consumed_first, produced_first, need_dict) = inflate(&mut ctx, 0);
        assert_eq!(
            produced_first, 0,
            "no output is possible before the dictionary"
        );
        assert!(
            need_dict,
            "inflate must report needDict for an FDICT stream"
        );

        infl_set_dictionary_buffer(
            &mut ctx,
            &[
                Value::Long(iaddr),
                Value::Long(dict_addr),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let (_, produced_second, _) = inflate(&mut ctx, consumed_first);
        assert_eq!(
            produced_second,
            payload.len(),
            "after setDictionary(ByteBuffer) the stream must decode — the no-op \
             left this at 0"
        );
        assert_eq!(read_byte_array(&ctx, cout, 0, produced_second), payload);

        // Keep the dictionary alive until every native has copied out of it.
        drop(dict_mem);
    }

    /// An unreadable dictionary address is reported, not silently dropped —
    /// and `IllegalArgumentException` is chosen because it is UNCHECKED:
    /// `setDictionary` declares no checked exception, so a `DataFormatException`
    /// (what the sibling `inflate*` natives raise for a bad address) would
    /// escape every caller's `catch`.
    #[test]
    fn set_dictionary_buffer_reports_an_unreadable_address() {
        let mut ctx = mock_ctx();
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        assert!(
            defl_set_dictionary_buffer(
                &mut ctx,
                &[Value::Long(addr), Value::Long(0), Value::Int(8)]
            )
            .is_err(),
            "a null buffer address with a non-zero length must be reported"
        );
        // A zero-length dictionary reads nothing, so the address is irrelevant.
        assert!(defl_set_dictionary_buffer(
            &mut ctx,
            &[Value::Long(addr), Value::Long(0), Value::Int(0)]
        )
        .is_ok());
    }

    #[test]
    fn deflate_windowed_large_input_small_output_matches_reference() {
        let mut ctx = mock_ctx();
        let addr = new_deflater(&mut ctx);
        // > DEFLATER_SCRATCH_CAP (64 KiB) incompressible-ish pattern, deflated into 512B chunks.
        let mut raw = Vec::with_capacity(200 * 1024);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..(200 * 1024) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            raw.push((x >> 24) as u8);
        }
        let in_arr = byte_array(&mut ctx, &raw);
        let out_arr = byte_array(&mut ctx, &[0u8; 512]);
        let mut compressed = Vec::new();
        let mut in_off = 0usize;
        let mut finished = false;
        while !finished {
            let flush = if in_off < raw.len() { 0 } else { 4 }; // NO_FLUSH then FINISH
            let packed = match defl_deflate_bytes_bytes(
                &mut ctx,
                &[
                    Value::Object(None),
                    Value::Long(addr),
                    Value::Object(Some(in_arr)),
                    Value::Int(in_off as i32),
                    Value::Int((raw.len() - in_off) as i32),
                    Value::Object(Some(out_arr)),
                    Value::Int(0),
                    Value::Int(512),
                    Value::Int(flush),
                    Value::Int(0),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Long(v) => v,
                other => panic!("expected Long, got {other:?}"),
            };
            let (used_in, used_out, fin) = unpack_deflate_result(packed);
            in_off += used_in;
            if used_out > 0 {
                compressed.extend_from_slice(&read_byte_array(&ctx, out_arr, 0, used_out));
            }
            finished = fin;
            assert!(used_in > 0 || used_out > 0 || finished, "deflater stalled");
        }
        assert_eq!(in_off, raw.len());
        let _ = defl_end(&mut ctx, &[Value::Long(addr)]);

        let mut decomp = Decompress::new(false);
        let mut restored = Vec::with_capacity(raw.len() + 64);
        decomp
            .decompress_vec(&compressed, &mut restored, FlushDecompress::Finish)
            .expect("decompress ok");
        assert_eq!(restored, raw);
    }
