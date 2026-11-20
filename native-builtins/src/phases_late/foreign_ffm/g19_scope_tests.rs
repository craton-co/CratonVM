use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// Build the synthetic segment shape `panama::pe_of_array_alias` and
/// `panama::pe_segment_slice` mint: eight slots, `[1]=byteSize`, and
/// `[2]` = the segment's own session.
fn stamped_segment(ctx: &mut dyn NativeContext, session: Value, byte_size: i64) -> ObjectRef {
    let seg = crate::panama::alloc_segment_carrier(ctx, 8).unwrap();
    ctx.set_field(seg, 0, Value::Long(0));
    ctx.set_field(seg, 1, Value::Long(byte_size));
    ctx.set_field(seg, P67_SEGMENT_ARENA, session);
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    seg
}

/// G19-1: `scope()` on a carrier stamped with its session must hand back
/// THAT session, not a fresh one.
///
/// MEASURED on 25.0.3+9-LTS: `heap.scope() == heap.scope()` and
/// `seg.asSlice(4,4).scope() == seg.scope()` are both true; CratonVM
/// answered false for both because this reader only ever looked for an
/// ARENA in slot 2, while `panama::pe_segment_slice` had been stamping the
/// SESSION there since W7-89. The `RForeignLayoutJdkInterfaces` assertion
/// "a heap segment's scope is stable" is this row.
#[test]
fn a_stamped_session_is_the_scope_and_it_is_the_same_object_every_time() {
    let mut ctx = mock_ctx();
    let session = p67_memory_session(&mut ctx).unwrap();
    let seg = stamped_segment(&mut ctx, session, 16);

    let first = p67_receiver_session(&mut ctx, seg).unwrap();
    let second = p67_receiver_session(&mut ctx, seg).unwrap();
    assert_eq!(first, session, "scope() must answer the stamped session");
    assert_eq!(
        first, second,
        "scope() must answer the SAME object on every call"
    );
}

/// The negative half, and it is not optional: slot 2's other tenant on an
/// `ofArray` MIRROR carrier is the Java backing array, and a reader that
/// accepted it as a session would read an array element as a state word.
///
/// A carrier with nothing in slot 2 keeps the historical behaviour (a fresh
/// session), which is a separate, still-open divergence — see the record's
/// §"what this lane did NOT do".
#[test]
fn an_array_in_slot_two_is_not_a_scope() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg = stamped_segment(&mut ctx, Value::Object(Some(array)), 16);
    let scope = p67_receiver_session(&mut ctx, seg).unwrap();
    assert_ne!(
        scope,
        Value::Object(Some(array)),
        "the backing array must never be handed out as a scope"
    );
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(match scope {
            Value::Object(Some(obj)) => obj,
            other => panic!("scope() answered {other:?}"),
        }))
        .as_deref(),
        Some("jdk/internal/foreign/MemorySessionImpl"),
        "the fallback is still a session"
    );
}

/// G19-1: and the stamped session is CHECKED, not merely reported.
///
/// The arm added to `p67_receiver_session` without the matching arm here
/// would be the W7-89 fail-open one branch over: the one shape whose scope
/// resolves would be the one shape that skips the validity check. Oracle:
/// a closed arena's segment answers `IllegalStateException: Already closed`
/// on every access.
#[test]
fn a_stamped_session_that_has_closed_refuses_the_access() {
    let mut ctx = mock_ctx();
    let session = p67_memory_session(&mut ctx).unwrap();
    let mut seg = stamped_segment(&mut ctx, session, 16);
    assert!(
        p67_segment_check_scope(&mut ctx, &mut seg).is_ok(),
        "an open session must let the access through"
    );

    let session_obj = match session {
        Value::Object(Some(obj)) => obj,
        other => panic!("p67_memory_session answered {other:?}"),
    };
    let slots = p67_session_slots(&ctx, session_obj);
    ctx.set_field(session_obj, slots.state, Value::Int(0));
    let err = p67_segment_check_scope(&mut ctx, &mut seg).unwrap_err();
    assert!(
        format!("{err:?}").contains("Already closed"),
        "a closed stamped session must refuse; got {err:?}"
    );
}

/// G19-1 §6.2: an `ofArray` MIRROR carrier keeps its session one slot
/// past the backing array, and the reader must find it there.
///
/// The negative half is the test above: with an array in slot 2 and
/// NOTHING in the mirror slot, the answer is still a fresh session and
/// never the array.
#[test]
fn a_mirror_carriers_session_lives_past_its_backing_array() {
    let mut ctx = mock_ctx();
    let session = p67_memory_session(&mut ctx).unwrap();
    let session_obj = match session {
        Value::Object(Some(obj)) => obj,
        other => panic!("p67_memory_session answered {other:?}"),
    };
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 8);
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 7).unwrap();
    ctx.set_field(seg, 0, Value::Long(0x1000));
    ctx.set_field(seg, 1, Value::Long(32));
    ctx.set_field(seg, P67_SEGMENT_ARENA, Value::Object(Some(array)));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(2));
    ctx.set_field(seg, 5, Value::Long(0));
    ctx.set_field(seg, 6, session);

    let first = p67_receiver_session(&mut ctx, seg).unwrap();
    let second = p67_receiver_session(&mut ctx, seg).unwrap();
    assert_eq!(first, session, "the mirror slot's session is the scope");
    assert_eq!(first, second, "and it is the same object every time");

    // …and it is CHECKED, not merely reported.
    let mut seg = seg;
    assert!(p67_segment_check_scope(&mut ctx, &mut seg).is_ok());
    let slots = p67_session_slots(&ctx, session_obj);
    ctx.set_field(session_obj, slots.state, Value::Int(0));
    let err = p67_segment_check_scope(&mut ctx, &mut seg).unwrap_err();
    assert!(
        format!("{err:?}").contains("Already closed"),
        "a closed mirror session must refuse; got {err:?}"
    );
}

/// The `FunctionDescriptor` carrier renders the oracle's spelling.
///
/// MEASURED on 25.0.3+9-LTS: `of(JAVA_LONG, ADDRESS).toString()` is
/// `(a8)j8` and `ofVoid(JAVA_INT)` is `(i4)v`. CratonVM printed
/// `java.lang.foreign.FunctionDescriptor@31e3` — an identity hash, which
/// is a cross-VM diff hazard as well as a divergence.
#[test]
fn a_descriptor_renders_as_the_oracle_spells_it() {
    let mut ctx = mock_ctx();
    let long = make_value_layout(&mut ctx, "OfLong", 8);
    let int = make_value_layout(&mut ctx, "OfInt", 4);

    let with_return = make_descriptor(&mut ctx, Some(long), &[int]);
    assert_eq!(p67_fd_render(&ctx, with_return), "(i4)j8");

    let void = make_descriptor(&mut ctx, None, &[int]);
    assert_eq!(p67_fd_render(&ctx, void), "(i4)v");

    let no_args = make_descriptor(&mut ctx, Some(int), &[]);
    assert_eq!(p67_fd_render(&ctx, no_args), "()i4");
}

/// `argumentLayouts()` hands back ONE list, not a fresh one per call.
///
/// MEASURED: `fd.argumentLayouts() == fd.argumentLayouts()` is true on the
/// oracle. The cache slot is what makes that hold, and a reader that
/// rebuilt the list on every call satisfied `.equals` and failed `==`.
#[test]
fn the_argument_list_is_minted_once() {
    let mut ctx = mock_ctx();
    let int = make_value_layout(&mut ctx, "OfInt", 4);
    let fd = make_descriptor(&mut ctx, None, &[int]);
    // The mock has no `java.util.List`, so the helper reports the refusal
    // rather than silently answering null — what matters here is that the
    // CACHE SLOT is the only place the answer is read from, which the
    // second call proves by returning the object the first one stored.
    let stored = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_field(fd, P67_FD_LIST, Value::Object(Some(stored)));
    assert_eq!(p67_fd_argument_list(&mut ctx, fd).unwrap(), stored);
    assert_eq!(p67_fd_argument_list(&mut ctx, fd).unwrap(), stored);
}

/// The two refusals `p67_fd_new` owes, and the boundary row that keeps the
/// padding rule from being over-applied.
#[test]
fn a_descriptor_refuses_a_null_and_a_padding_member() {
    let mut ctx = mock_ctx();
    let int = make_value_layout(&mut ctx, "OfInt", 4);
    let padding = make_padding_layout(&mut ctx, 4);

    let params = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params, 0, Value::Object(None));
    let err = p67_fd_new(&mut ctx, Some(int), params).unwrap_err();
    assert!(
        format!("{err:?}").contains("NullPointerException"),
        "a null member is a bare NPE; got {err:?}"
    );

    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let err = p67_fd_new(&mut ctx, Some(padding), empty).unwrap_err();
    assert!(
        format!("{err:?}").contains("Unsupported padding layout return in function descriptor"),
        "a padding RETURN names the return position; got {err:?}"
    );

    let params = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params, 0, Value::Object(Some(padding)));
    let err = p67_fd_new(&mut ctx, None, params).unwrap_err();
    assert!(
        format!("{err:?}")
            .contains("Unsupported padding layout argument in function descriptor"),
        "a padding ARGUMENT names the argument position; got {err:?}"
    );

    // The boundary: only PADDING is refused, not "anything that is not a
    // value layout". `of(sequenceLayout(2, JAVA_INT))` is accepted by the
    // oracle and renders `()[2:i4]`.
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    assert!(p67_fd_new(&mut ctx, Some(int), empty).is_ok());
}

/// A four-slot JDK-shaped value layout: `[0]=byteSize`, `[1]=byteAlignment`,
/// classified by CLASS NAME, which is the carrier `--real-jdk` has.
fn make_value_layout(ctx: &mut dyn NativeContext, suffix: &str, size: i64) -> ObjectRef {
    let class = format!("jdk/internal/foreign/layout/ValueLayouts${suffix}Impl");
    let cid = ctx.ensure_class_initialized(&class).unwrap();
    let layout = ctx.alloc_object(cid, 4);
    ctx.set_field(layout, 0, Value::Long(size));
    ctx.set_field(layout, 1, Value::Long(size));
    ctx.set_field(layout, 2, Value::Object(None));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

fn make_padding_layout(ctx: &mut dyn NativeContext, size: i64) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/PaddingLayout")
        .unwrap();
    let layout = ctx.alloc_object(cid, 4);
    ctx.set_field(layout, 0, Value::Long(size));
    ctx.set_field(layout, 1, Value::Long(1));
    ctx.set_field(layout, 2, Value::Object(None));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

fn make_descriptor(
    ctx: &mut dyn NativeContext,
    ret: Option<ObjectRef>,
    args: &[ObjectRef],
) -> ObjectRef {
    let params = ctx.new_array(cratonvm_types::ArrayElementType::Reference, args.len());
    for (i, a) in args.iter().enumerate() {
        ctx.set_array_element(params, i, Value::Object(Some(*a)));
    }
    match p67_fd_new(ctx, ret, params).unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("p67_fd_new answered {other:?}"),
    }
}

/// `MemorySegment.address()` on `ofArray(int[])` must answer 0, the same
/// as every other heap segment.
///
/// MEASURED on HotSpot 25.0.3+9-LTS: `MemorySegment.ofArray(new
/// int[8]).address()` is 0, the same as `ofArray(new byte[8]).address()`
/// — the JDK never exposes a heap segment's address, whatever its element
/// type. Between 2026-09-20 and 2026-09-21 `ofArray(int[]/long[]/float[]/
/// double[])` minted a MIRROR carrier with a real native buffer in slot
/// 0, and this function's `>= 6` branch read that slot positionally, the
/// same as it does for a genuine off-heap (`Arena`-backed) segment,
/// leaking a nondeterministic native address instead of 0 — fixed then by
/// gating this function on `crate::panama::is_heap_shaped_segment`, and
/// fixed for good the next day when `ofArray` stopped minting the mirror
/// shape at all (`pe_of_array_alias` now covers int[] directly, so this
/// test exercises the REAL registration rather than a hand-built shape).
#[test]
fn ofarray_ints_address_does_not_leak_a_native_buffer() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let seg = match crate::panama::pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))])
        .unwrap()
    {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(int[]) answered {other:?}"),
    };

    assert_eq!(
        p67_segment_address(&mut ctx, &[Value::Object(Some(seg))]).unwrap(),
        Some(Value::Long(0)),
        "ofArray(int[])'s address() must be 0 like every other heap segment"
    );
}

/// Negative control for the test above: a PLAIN off-heap segment — the
/// same six-slot shape `panama::pe_arena_allocate_impl` mints, with
/// nothing array-shaped in slot 2 — is not heap-shaped and must keep
/// reporting its real address. Without this the mirror fix could have
/// been "always answer 0 for a 6-or-7-field carrier", which would have
/// broken every ordinary `Arena.allocate` segment's `address()`.
#[test]
fn a_plain_native_segments_address_is_unaffected() {
    let mut ctx = mock_ctx();
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(0x7f00_0000)); // real native address
    ctx.set_field(seg, 1, Value::Long(8)); // byteSize
    ctx.set_field(seg, 2, Value::Object(None)); // arena, not an array
    ctx.set_field(seg, 3, Value::Int(0)); // read-write
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // offset

    assert_eq!(
        p67_segment_address(&mut ctx, &[Value::Object(Some(seg))]).unwrap(),
        Some(Value::Long(0x7f00_0000)),
        "an ordinary off-heap segment must still report its real address"
    );
}

/// j27 wave 4 (lane FFMMSG): a heap segment's `address()` is its offset
/// into the array (`HeapMemorySegmentImpl.address()` is `offset -
/// arrayBaseOffset`; HotSpot 25: `ofArray(new byte[32]).asSlice(3)
/// .address() == 3`). This answered 0 for every heap segment, slices
/// included. The slice is the alias shape `asSlice` mints.
#[test]
fn a_heap_slices_address_is_its_offset_into_the_array() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let seg = match crate::panama::pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))])
        .unwrap()
    {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(int[]) answered {other:?}"),
    };
    ctx.set_field(seg, 1, Value::Long(4));
    ctx.set_field(seg, crate::panama::SEG_HEAP_START_FIELD, Value::Long(4));
    assert_eq!(
        p67_segment_address(&mut ctx, &[Value::Object(Some(seg))]).unwrap(),
        Some(Value::Long(4))
    );
    let text = crate::panama::segment_to_string(&ctx, seg);
    assert!(text.ends_with("address: 0x4, byteSize: 4 }"), "{text}");
}

/// j27 wave 4 (lane FFMMSG): a misaligned native element access is
/// HotSpot's `checkEnclosingLayout` refusal (`get(layout, offset)` is
/// `layout.varHandle().get(this, offset)`); `Misaligned access at
/// address: N` is the byte-array view handles' wording. The `(of ..)`
/// clause is the layout's `toString()`: absent while the mock answers
/// none, present once it does (j27 wave 6, tested before the scope).
#[test]
fn a_misaligned_native_element_read_is_hotspots_target_offset_refusal() {
    let mut ctx = mock_ctx();
    let mut words = [0u64; 2];
    let ptr = words.as_mut_ptr() as i64;
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(16));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    let cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl")
        .unwrap();
    let layout = ctx.alloc_object(cid, 3);
    ctx.set_field(layout, 0, Value::Long(4));
    ctx.set_field(layout, 1, Value::Long(4));
    let args = [
        Value::Object(Some(seg)),
        Value::Object(Some(layout)),
        Value::Long(2),
    ];
    let err = format!("{:?}", p67_segment_get_width(&mut ctx, &args, 4).unwrap_err());
    assert!(
        err.contains(&format!(
            "Target offset 2 is incompatible with alignment constraint 4 for segment \
             MemorySegment{{ kind: native, address: 0x{ptr:x}, byteSize: 16 }}"
        )),
        "{err}"
    );
    // j27 wave 6 (lane T1LOOPFFM): the alignment is tested before the
    // scope check now, while the layout argument is current, so the
    // refusal carries the layout's `toString()` -- for a read and a write.
    let expected = format!(
        "Target offset 2 is incompatible with alignment constraint 4 (of i4) for segment \
         MemorySegment{{ kind: native, address: 0x{ptr:x}, byteSize: 16 }}"
    );
    let i4 = ctx.create_string("i4");
    ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
    let err = format!("{:?}", p67_segment_get_width(&mut ctx, &args, 4).unwrap_err());
    assert!(err.contains(&expected), "read: {err}");
    let i4 = ctx.create_string("i4");
    ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
    let set_args = [
        Value::Object(Some(seg)),
        Value::Object(Some(layout)),
        Value::Long(2),
        Value::Int(7),
    ];
    let err = format!("{:?}", p67_segment_set_width(&mut ctx, &set_args, 4, 3).unwrap_err());
    assert!(err.contains(&expected), "write: {err}");
    assert_eq!(words, [0, 0], "nothing written");
}

/// j27 wave 4 (lane FFMMSG): `getString(offset)` refuses as JDK 25's
/// `StringSupport.strlenByte` does: `checkBounds(offset, byteSize -
/// offset)` first (`outOfBoundException`, the length negative past the
/// end), then `IndexOutOfBoundsException: No null terminator found:
/// <segment> using region [offset, byteSize)` for an empty or unterminated
/// region (offset == byteSize included). It said `getString offset .. is
/// out of bounds for ..` / `getString found no NUL terminator ..`.
#[test]
fn get_string_refuses_with_hotspots_strlen_texts() {
    let mut ctx = mock_ctx();
    let mut bytes = *b"hixy";
    let ptr = bytes.as_mut_ptr() as i64;
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(4));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    let text = format!("MemorySegment{{ kind: native, address: 0x{ptr:x}, byteSize: 4 }}");
    let refusal = |ctx: &mut crate::test_utils::MockNativeContext, offset: i64| -> String {
        match p67_segment_get_string(ctx, &[Value::Object(Some(seg)), Value::Long(offset)]) {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IndexOutOfBoundsException { message: Some(message) },
            ))) => message,
            other => panic!("offset {offset}: expected IndexOutOfBoundsException, got {other:?}"),
        }
    };
    assert_eq!(
        refusal(&mut ctx, 1),
        format!("No null terminator found: {text} using region [1, 4)")
    );
    assert_eq!(
        refusal(&mut ctx, 4),
        format!("No null terminator found: {text} using region [4, 4)")
    );
    assert_eq!(
        refusal(&mut ctx, 6),
        format!("Out of bound access on segment {text}; new offset = 6; new length = -2")
    );
    assert_eq!(
        refusal(&mut ctx, -1),
        format!("Out of bound access on segment {text}; new offset = -1; new length = 5")
    );
    bytes[2] = 0;
    let read = p67_segment_get_string(&mut ctx, &[Value::Object(Some(seg)), Value::Long(0)]);
    let Ok(Some(Value::Object(Some(s)))) = read else {
        panic!("no string: {read:?}")
    };
    assert_eq!(ctx.read_string(s).as_deref(), Some("hi"));
}

/// The string `getString(offset)` answers, or the refusal's debug text.
fn get_string(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
) -> Result<String, String> {
    match p67_segment_get_string(ctx, &[Value::Object(Some(seg)), Value::Long(offset)]) {
        Ok(Some(Value::Object(Some(s)))) => Ok(ctx.read_string(s).unwrap_or_default()),
        other => Err(format!("{other:?}")),
    }
}

/// j27 wave 4 (lane FFMMSG): `getString` on a heap segment reads the
/// array, as HotSpot's `strlenByte` does over `unsafeGetBase()`; it
/// answered `getString on a segment with no address`. Bounds and the
/// terminator scan are over the heap view.
#[test]
fn get_string_reads_a_heap_segment() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 5);
    for (i, b) in b"hi\0yz".iter().enumerate() {
        ctx.set_array_element(array, i, Value::Int(i32::from(*b)));
    }
    let seg = match crate::panama::pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))])
        .unwrap()
    {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray answered {other:?}"),
    };
    assert_eq!(get_string(&mut ctx, seg, 0), Ok("hi".to_string()));
    assert_eq!(get_string(&mut ctx, seg, 1), Ok("i".to_string()));
    let text = crate::panama::segment_to_string(&ctx, seg);
    let err = get_string(&mut ctx, seg, 3).unwrap_err();
    assert!(
        err.contains(&format!("No null terminator found: {text} using region [3, 5)")),
        "{err}"
    );
    let err = get_string(&mut ctx, seg, 6).unwrap_err();
    assert!(err.contains("new offset = 6; new length = -1"), "{err}");
}

/// j27 wave 4 (lane FFMMSG): the scan covers the whole remaining segment,
/// as HotSpot's does; it stopped at 1 MiB and called a longer string
/// unterminated. The scan stays inside the segment: an unterminated
/// region one byte short of the terminator is refused, not read past.
#[test]
fn get_string_reads_past_one_mebibyte_and_never_past_the_segment() {
    let mut ctx = mock_ctx();
    let len = (3 << 20) / 2;
    let mut bytes = vec![b'a'; len + 1];
    bytes[len] = 0;
    let ptr = bytes.as_mut_ptr() as i64;
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(len as i64 + 1));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    assert_eq!(get_string(&mut ctx, seg, 0).map(|s| s.len()), Ok(len));
    ctx.set_field(seg, 1, Value::Long(len as i64));
    let err = get_string(&mut ctx, seg, 0).unwrap_err();
    assert!(err.contains(&format!("using region [0, {len})")), "{err}");
}

/// JDK 25's `requireWithinStringSize`: longer than
/// `ArraysSupport.SOFT_MAX_ARRAY_LENGTH` (`Integer.MAX_VALUE - 8`) is
/// `String too large`.
#[test]
fn get_string_length_limit_is_the_soft_max_array_length() {
    assert!(!p67_string_too_large(i64::from(i32::MAX) - 8));
    assert!(p67_string_too_large(i64::from(i32::MAX) - 7));
}

/// j27 wave 4 (lane FFMMSG): the native element accessors' bounds refusal
/// is `outOfBoundException`'s text for whatever segment reached them.
#[test]
fn the_native_accessor_bounds_text_is_the_segments_to_string() {
    let mut ctx = mock_ctx();
    let seg = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(0x7f00_0000));
    ctx.set_field(seg, 1, Value::Long(8));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    let err = format!("{:?}", p67_native_segment_out_of_bounds(&ctx, seg, 6, 4));
    assert!(
        err.contains(
            "Out of bound access on segment MemorySegment{ kind: native, address: \
             0x7f000000, byteSize: 8 }; new offset = 6; new length = 4"
        ),
        "{err}"
    );
}
