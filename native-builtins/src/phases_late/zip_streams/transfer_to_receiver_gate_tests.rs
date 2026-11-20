/// `native_input_stream_transfer_to` must ask the receiver's CLASS before
/// it indexes the receiver's fields.
///
/// The fast path here exists for `ByteArrayInputStream` and recognises it
/// by reading slots 0, 1 and 3. Any stream with fewer slots than that is
/// read out of bounds by the probe itself — measured 2026-08-24, every
/// `CoyoteInputStream` (one field, `ib`) that reached this native produced
/// `zgc real: field index OOB index=1..4`: 16 per run of
/// `TestDefaultServletRfc9110Section13` and 8 per run of
/// `TestWebdavServletOptionsUnknown`, both of which PASS, which is how it
/// went unnoticed. Worse, a probe that MATCHED on a wrong class would then
/// `set_field(input, 1, Value::Int(..))` — a primitive into whatever slot 1
/// is there, in general a reference field, which is the punned cell that
/// made a compiled `arraylength` dereference the integer 1.
///
/// A SOURCE guard for the ORDER: the receiver gate must precede the first
/// indexed read of `input`. Reordering is the way this defect comes back,
/// and no behavioural test can see an ordering.
///
/// The BEHAVIOUR is covered separately, by `byte_array_stream_layout_tests`
/// above. An earlier revision of this comment said there was no mock
/// `NativeContext` in this crate that could carry a real receiver; that is
/// wrong — `crate::test_utils::MockNativeContext` gives a class id and an
/// exact slot count to an allocation (`ensure_class_initialized` +
/// `alloc_object`), which is all this predicate reads. Those three tests
/// drive a one-slot `CoyoteInputStream`, a four-slot
/// `ByteArrayInputStream`, and a four-slot unrelated stream.
///
/// The measured signature is `index=1` and `index=3` with `num_slots=1`,
/// and never index 2 or 4: the pair is `pos` and `count` of the
/// `ByteArrayInputStream` layout, which is what points at the shape test.
#[test]
fn the_class_gate_precedes_any_indexed_read_of_the_receiver() {
    let src = include_str!("../zip_streams.rs");
    let start = src
        .find("fn native_input_stream_transfer_to")
        .expect("the native must still exist under this name");
    let body = &src[start..];
    // Split so this test does not match its own source.
    let probe = concat!("ctx.get_", "field(input, ");
    let gate = concat!("has_byte_array_", "stream_layout(ctx, input)");

    let gate_at = body.find(gate).unwrap_or_else(|| {
        panic!(
            "native_input_stream_transfer_to no longer asks \
             has_byte_array_stream_layout before probing its layout; a stream with \
             fewer slots than the ByteArrayInputStream shape is read out of bounds"
        )
    });
    let probe_at = body
        .find(probe)
        .expect("the layout probe should still be there");
    assert!(
        gate_at < probe_at,
        "the class gate must come BEFORE the first indexed read of `input` \
         (gate at {gate_at}, probe at {probe_at}) — otherwise the probe \
         itself is the out-of-bounds access"
    );
}
