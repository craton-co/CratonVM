#[allow(unused_imports)]
use super::*;

/// Reverse the low `width` bytes of `raw` (a 1-byte access is its own swap).
pub(in crate::panama) fn pe_swap_width(raw: u64, width: i64) -> u64 {
    match width {
        2 => u64::from((raw as u16).swap_bytes()),
        4 => u64::from((raw as u32).swap_bytes()),
        8 => raw.swap_bytes(),
        _ => raw,
    }
}

/// Is a native access at `addr` misaligned for the layout's `align`? HotSpot's
/// rule for a native segment is the ABSOLUTE address (`isAlignedForElement`
/// over `unsafeGetOffset() + offset`, `maxAlignMask` 0), which
/// `pe_segment_access_addr` has just computed, not `maxByteAlignment`.
pub(crate) fn native_misaligned(addr: usize, align: u64) -> bool {
    ffm_native_align_check() && align > 1 && (addr as u64) & (align - 1) != 0
}

/// HotSpot's refusal of a misaligned native access: `checkEnclosingLayout`'s
/// `Target offset .. is incompatible with alignment constraint .. (of ..) for
/// segment ..` (j27 wave 4, lane FFMMSG). It said `Misaligned access at
/// address: N`, which is the byte-array view handles' wording, not a
/// segment's. Pass `layout` only while that reference is known current (no
/// Java since it was read); `None` drops the `(of ..)` clause. Runs Java for
/// the clause: the caller closes its access window first.
pub(crate) fn native_misaligned_refusal(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: Option<ObjectRef>,
    offset: i64,
    align: i64,
) -> MethodCallFailed {
    let segment = segment_to_string(ctx, seg);
    RuntimeError::IllegalArgumentException {
        message: misaligned_message(ctx, &segment, layout, offset, align),
    }
    .into()
}

/// `Utils.checkElementAlignment(layout, "Layout alignment greater than its
/// size")`, the first thing every `getAtIndex`/`setAtIndex` overload does.
pub(in crate::panama) fn pe_check_element_alignment(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    kind: i32,
    elem_size: i64,
) -> Result<(), MethodCallFailed> {
    if ffm_native_align_check()
        && layout_kind_is_value(kind)
        && crate::panama_libffi::layout_align(ctx, layout) as i64 > elem_size
    {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Layout alignment greater than its size".into(),
        }
        .into());
    }
    Ok(())
}

/// The raw bits of a value `set` stores through a `kind` layout, or `None`
/// when the value's shape does not match the layout.
pub(in crate::panama) fn pe_raw_from_value(kind: i32, value: Value) -> Option<u64> {
    Some(match (kind, value) {
        (LAYOUT_BYTE | LAYOUT_BOOLEAN, Value::Int(v)) => u64::from(v as u8),
        (LAYOUT_SHORT | LAYOUT_CHAR, Value::Int(v)) => u64::from(v as u16),
        (LAYOUT_INT, Value::Int(v)) => u64::from(v as u32),
        (LAYOUT_LONG | LAYOUT_ADDRESS, Value::Long(v)) => v as u64,
        (LAYOUT_FLOAT, Value::Float(v)) => u64::from(v.to_bits()),
        (LAYOUT_DOUBLE, Value::Double(v)) => v.to_bits(),
        _ => return None,
    })
}

/// The Java value `get` answers for `raw` read through a `kind` layout, or
/// `None` for a kind that is not a primitive value layout (`ADDRESS` is
/// answered by its caller, which has to allocate).
pub(in crate::panama) fn pe_value_from_raw(kind: i32, raw: u64) -> Option<Value> {
    Some(match kind {
        LAYOUT_BYTE => Value::Int(i32::from(raw as u8 as i8)),
        // A `boolean` is 0 or 1, never the raw byte: the JDK reads the byte and
        // compares it to zero, so a stray 2 in the segment is `true`.
        LAYOUT_BOOLEAN => Value::Int(i32::from(raw as u8 != 0)),
        LAYOUT_SHORT => Value::Int(i32::from(raw as u16 as i16)),
        // `char` is UNSIGNED.
        LAYOUT_CHAR => Value::Int(i32::from(raw as u16)),
        LAYOUT_INT => Value::Int(raw as u32 as i32),
        LAYOUT_LONG => Value::Long(raw as i64),
        LAYOUT_FLOAT => Value::Float(f32::from_bits(raw as u32)),
        LAYOUT_DOUBLE => Value::Double(f64::from_bits(raw)),
        _ => return None,
    })
}

/// Read `width` bytes at `addr` in host order, zero-extended. UNALIGNED
/// reads: a `JAVA_INT_UNALIGNED` access at an odd address is legal, and a
/// plain `*(addr as *const i32)` there is undefined behaviour in Rust.
///
/// # Safety
/// `addr` must be valid for reads of `width` bytes.
pub(in crate::panama) unsafe fn pe_read_raw(addr: *const u8, width: i64) -> u64 {
    match width {
        1 => u64::from(addr.read()),
        2 => u64::from(addr.cast::<u16>().read_unaligned()),
        4 => u64::from(addr.cast::<u32>().read_unaligned()),
        8 => addr.cast::<u64>().read_unaligned(),
        _ => 0,
    }
}

/// Write the low `width` bytes of `raw` at `addr` in host order.
///
/// # Safety
/// `addr` must be valid for writes of `width` bytes.
pub(in crate::panama) unsafe fn pe_write_raw(addr: *mut u8, width: i64, raw: u64) {
    match width {
        1 => addr.write(raw as u8),
        2 => addr.cast::<u16>().write_unaligned(raw as u16),
        4 => addr.cast::<u32>().write_unaligned(raw as u32),
        8 => addr.cast::<u64>().write_unaligned(raw),
        _ => {}
    }
}

// Exact primitive/covariant descriptors used by real-JDK MemorySegment
// default methods. They share the checked erased implementation above.
pub(in crate::panama) fn pe_segment_get_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_segment_access(ctx, "getAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    let elem_size = ffi::layout_byte_size(kind) as i64;
    pe_check_element_alignment(ctx, layout, kind, elem_size)?;
    // JIT round 20 wave 8 (lane huntFFM): WRAPPING, as JDK 25's
    // `index * layout.byteSize()` (a plain `lmul`) is: `getAtIndex(JAVA_INT,
    // 1L << 62)` is offset 0 and READS element 0 on HotSpot, and
    // `getAtIndex(JAVA_LONG, Long.MAX_VALUE)` is offset -8 (the IOOBE names
    // `new offset = -8`). Saturating made the first an IOOBE and the second
    // name `Long.MAX_VALUE`. The bounds check below sees the wrapped offset,
    // exactly as the JDK's does.
    pe_segment_get_impl(ctx, this, layout, index.wrapping_mul(elem_size))
}

pub(in crate::panama) fn pe_segment_set_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_segment_access(ctx, "setAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    let elem_size = ffi::layout_byte_size(kind) as i64;
    pe_check_element_alignment(ctx, layout, kind, elem_size)?;
    // JIT round 20 wave 8 (lane huntFFM): wrapping, as `pe_segment_get_at_index`.
    pe_segment_set_impl(ctx, this, layout, index.wrapping_mul(elem_size), value)
}

pub(in crate::panama) fn pe_segment_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: get() dereferences the segment's raw `ptr` field.
    require_segment_access(ctx, "get")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    pe_segment_get_impl(ctx, this, layout, offset)}

pub(crate) fn pe_segment_get_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
) -> MethodCallResult {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    // An unclassifiable carrier must not silently become a one-byte read that
    // answers `Value::Int(0)`. `ffi::layout_byte_size` defaults to 1 and the
    // `_ =>` arm below defaults to 0, so before this the pair produced a
    // believable answer for a layout this VM could not decode at all.
    // `LAYOUT_UNKNOWN` is -2, so the `kind < 10` spellings that used to guard
    // this family admitted it; see [`layout_kind_is_value`].
    if kind == crate::panama_libffi::LAYOUT_UNKNOWN {
        return Err(unclassifiable_access_layout(ctx, layout, "get"));
    }
    // Reject reads that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the read widths below.
    let width = ffi::layout_byte_size(kind) as i64;
    // The layout's byte order and alignment (round 12 wave 5, lane ffm2). The
    // epoch is read first: it stamps the layout verdict published below.
    let traits_epoch = crate::ffm_fast::epoch();
    let (little, align) = pe_layout_access_traits(ctx, layout);

    // HEAP SEGMENTS READ THE BACKING JAVA ARRAY, not an address they do not
    // have. This is the whole of F27 NOM-3 item 2; before it, this path
    // dereferenced whatever `segment_address` answered for a heap carrier.
    if crate::panama_libffi::is_real_heap_segment(ctx, seg)
        || ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
    {
        if let Some(view) = heap_segment_view(ctx, seg) {
            heap_segment_check_access(ctx, seg, &view, layout, offset, width, false)?;
            let raw = heap_segment_read(ctx, &view, offset, width as usize);
            // `heap_segment_read` assembles a LITTLE-endian image; a
            // big-endian layout reads it reversed.
            let raw = if little {
                raw
            } else {
                pe_swap_width(raw, width)
            };
            let value = match kind {
                LAYOUT_BYTE => Value::Int(i32::from(raw as u8 as i8)),
                LAYOUT_BOOLEAN => Value::Int(i32::from(raw as u8 != 0)),
                LAYOUT_SHORT => Value::Int(i32::from(raw as u16 as i16)),
                LAYOUT_CHAR => Value::Int(i32::from(raw as u16)),
                LAYOUT_INT => Value::Int(raw as u32 as i32),
                LAYOUT_LONG => Value::Long(raw as i64),
                LAYOUT_FLOAT => Value::Float(f32::from_bits(raw as u32)),
                LAYOUT_DOUBLE => Value::Double(f64::from_bits(raw)),
                LAYOUT_ADDRESS => {
                    return Ok(Some(Value::Object(Some(pe_address_read_segment(
                        ctx, layout, raw as i64,
                    )?))))
                }
                other => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "MemorySegment.get: layout kind {other} is not a value layout"
                        ),
                    }
                    .into())
                }
            };
            return Ok(Some(value));
        } else if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
            // It IS a heap segment and its shape did not resolve. Refusing by
            // name beats falling through to the raw-address path, which would
            // dereference `segment_address`'s answer for it.
            return Err(unreadable_heap_segment(ctx, seg, "get"));
        }
    }

    // Opened before the epoch read and the scope check, held until the load
    // below is done: a close on another thread either sees this access in
    // flight and waits for it, or its closed state is what the scope check
    // reads. See `ffm_fast::AccessWindow`.
    let _window = crate::ffm_fast::AccessWindow::open();
    // Before the scope check: see `ffm_fast::note_validated_at`.
    let verdict_epoch = crate::ffm_fast::epoch();
    let addr = if ffm_bounds_before_scope() {
        // Round 13 wave 1 (lane mhffm): bounds, alignment, then the session.
        let addr = pe_segment_locate_addr(ctx, seg, offset, width)?;
        if native_misaligned(addr, align) {
            // No Java since `layout` was read; the clause's `toString` runs
            // after the window closes (j27 wave 4, lane FFMMSG).
            drop(_window);
            return Err(native_misaligned_refusal(ctx, seg, Some(layout), offset, align as i64));
        }
        pe_segment_check_scope(ctx, seg)?;
        addr
    } else {
        let addr = pe_segment_access_addr(ctx, seg, offset, width)?;
        if native_misaligned(addr, align) {
            // The scope check ran first and can run Java: no layout clause.
            drop(_window);
            return Err(native_misaligned_refusal(ctx, seg, None, offset, align as i64));
        }
        addr
    };
    ffm_publish_verdict(ctx, seg, false, verdict_epoch);
    pe_publish_layout_verdict(layout, little, align, width, traits_epoch);

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr); `pe_read_raw` reads unaligned. The width is the
    // layout kind's.
    let raw = unsafe { pe_read_raw(addr as *const u8, width) };
    let raw = if little == cfg!(target_endian = "little") {
        raw
    } else {
        pe_swap_width(raw, width)
    };
    // ADDRESS: its declared return type is `Ljava/lang/foreign/MemorySegment;`,
    // so it has to ALLOCATE, which `Value::Long` cannot stand in for -- a
    // reference slot receiving a primitive is the one shape this tree keeps
    // paying for.
    if kind == LAYOUT_ADDRESS {
        // Round 13 wave 13 (lane ffm7): sized and checked by the target.
        return Ok(Some(Value::Object(Some(pe_address_read_segment(
            ctx, layout, raw as i64,
        )?))));
    }
    let value = pe_value_from_raw(kind, raw).unwrap_or(Value::Int(0));
    if crate::nbflags().dbg_mh_dispatch && kind == LAYOUT_FLOAT && offset == 0 {
        eprintln!(
            "[PANAMA_GET_FLOAT] runtime={} ptr={:?} base_offset={:?} value={value:?}",
            ctx.class_name_of_id(ctx.class_id_of_object(seg))
                .unwrap_or_default(),
            ctx.get_field(seg, 0),
            ctx.get_field(seg, 5),
        );
    }
    Ok(Some(value))
}

pub(in crate::panama) fn pe_segment_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: set() dereferences the segment's raw `ptr` field.
    require_segment_access(ctx, "set")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    pe_segment_set_impl(ctx, this, layout, offset, value)
}

pub(crate) fn pe_segment_set_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
    value: Value,
) -> MethodCallResult {
    // A READ-ONLY SEGMENT REFUSES THE WRITE, and this body never asked.
    // MEASURED against HotSpot 25.0.4+7, in BOTH modes
    // (`FfmSegmentSweep.java`):
    //
    //   MemorySegment ro = Arena.ofConfined().allocate(16).asReadOnly();
    //   ro.set(ValueLayout.JAVA_INT, 0, 1)
    //     HotSpot   IllegalArgumentException: Attempt to write a read-only segment
    //     CratonVM  no-throw -- and the next read shows the write LANDED
    //
    // `isReadOnly()` already answered `true` on that receiver, so the flag was
    // right and nothing consulted it: `asReadOnly()` handed back a reference
    // that had not lost the one capability the call exists to remove. F26
    // records the same shape one level down ("a copying slice is a wrong
    // capability").
    //
    // The HEAP path has had this since `heap_segment_check_access`; the
    // raw-address path never did. Unlike the ALIGNMENT rule beside it -- which
    // that function's comment explains cannot be transplanted, because a native
    // segment's real `maxByteAlignment` is not knowable from the carrier -- the
    // read-only flag IS on the carrier, and `craton_segment_check_read_only`
    // reads it through `p67_segment_is_read_only`, the registered `isReadOnly()`
    // body. One source of truth, not a second copy.
    //
    // WHERE THIS HAD TO GO. `phases_late/foreign_ffm.rs` registers the same four
    // `set` descriptors and carries the identical check, and
    // `--dump-native-registry` says those rows are `owns_slot: false` while
    // these are `owns_slot: true`. The first attempt put the guard only there
    // and measured EXACTLY the pre-fix transcript. Both halves are kept, and
    // deliberately: registration order is what picks between them, and a pair
    // where only one half refuses is the half-fixed duplicate this file's own
    // history keeps finding.
    //
    // Round 14 wave 3 (lane ffm): except for an ADDRESS store, whose value JDK
    // 25 unboxes (`SharedUtils.unboxSegment` in a `filterValue` adapter)
    // BEFORE the access checks the target: `readOnly.set(ADDRESS, 0, null)` is
    // `NullPointerException` there, a heap-segment value
    // `IllegalArgumentException` whatever the target
    // (`CRATONVM_FFM_ADDRESS_STORE_VALUE_FIRST`). The kind is a plain read.
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    let address_value_first = kind == LAYOUT_ADDRESS && ffm_address_store_value_first();
    if !address_value_first {
        craton_segment_check_read_only(ctx, seg, false)?;
    }
    // Symmetric with `pe_segment_get_impl`: an unclassifiable carrier used to
    // reach the `_ => {}` arm below, i.e. a WRITE that silently did nothing.
    if kind == crate::panama_libffi::LAYOUT_UNKNOWN {
        return Err(unclassifiable_access_layout(ctx, layout, "set"));
    }
    // Reject writes that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the write widths below.
    let width = ffi::layout_byte_size(kind) as i64;

    // `set(ADDRESS, off, seg)` takes a MemorySegment, not a long: its address
    // is what gets stored. Without this the value arrives as `Value::Object`,
    // misses every arm below and the write is a SILENT no-op -- which is the
    // quieter half of the same defect as the missing registration.
    //
    // A HEAP SEGMENT HAS NO ADDRESS TO STORE, and since F27 `segment_address`
    // answers 0 for one — so this arm would have written a C NULL and said
    // nothing. The oracle refuses instead (measured, 25.0.3+9-LTS):
    //
    //     MemorySegment.ofArray(new byte[16])
    //         .set(ValueLayout.ADDRESS.withByteAlignment(1), 0,
    //              MemorySegment.ofArray(new byte[4]))
    //       -> IllegalArgumentException: Heap segment not allowed:
    //          MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 4 }
    //
    // and the same refusal, verbatim, when one is passed to a downcall as a
    // pointer. A wrong ADDRESS is a wrong capability; 0 is a legitimate C null
    // and cannot be told apart from one downstream.
    let value = match (kind, value) {
        (LAYOUT_ADDRESS, Value::Object(Some(target))) => {
            if let Some(refusal) = heap_segment_refusal(&*ctx, target) {
                return Err(refusal);
            }
            Value::Long(crate::panama_libffi::address_for_native_store(
                crate::panama_libffi::segment_address(ctx, target),
            ))
        }
        // Round 14 wave 1 (lane ffm): JDK 25 unboxes the value with
        // `SharedUtils.unboxSegment(segment)`, whose `segment.isNative()` is a
        // `NullPointerException` for `null` (`MemorySegment.NULL` is the C
        // null); this wrote 0 silently.
        // Message-less, as MEASURED on HotSpot 25 for `set` / `setAtIndex`
        // (read-only target included): unlike the VarHandle road
        // ([`ffm_null_address_npe_message`]), this NPE carries no helpful
        // message (round 14 wave 5 hand-back, `R14Fixup2AddressNullMessage`).
        (LAYOUT_ADDRESS, Value::Object(None)) if ffm_null_address_npe() => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        (LAYOUT_ADDRESS, Value::Object(None)) => Value::Long(0),
        _ => value,
    };
    if address_value_first {
        craton_segment_check_read_only(ctx, seg, false)?;
    }
    // Byte order and alignment: see `pe_segment_get_impl`.
    let traits_epoch = crate::ffm_fast::epoch();
    let (little, align) = pe_layout_access_traits(ctx, layout);

    // HEAP SEGMENTS WRITE THE BACKING JAVA ARRAY. See `pe_segment_get_impl`.
    if crate::panama_libffi::is_real_heap_segment(ctx, seg)
        || ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
    {
        if let Some(view) = heap_segment_view(ctx, seg) {
            heap_segment_check_access(ctx, seg, &view, layout, offset, width, true)?;
            // NOT a silent no-op, unlike the raw-address arm below. A value
            // whose Rust shape does not match its layout is a marshalling
            // defect upstream, and swallowing it is how a wrong write becomes
            // invisible.
            let Some(raw) = pe_raw_from_value(kind, value) else {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "MemorySegment.set on a heap segment: layout kind {kind} does not \
                         accept {value:?}"
                    ),
                }
                .into());
            };
            // `heap_segment_write` lays the image down little-endian.
            let raw = if little {
                raw
            } else {
                pe_swap_width(raw, width)
            };
            heap_segment_write(ctx, &view, offset, width as usize, raw);
            return Ok(None);
        } else if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
            return Err(unreadable_heap_segment(ctx, seg, "set"));
        }
    }

    // The close handshake's accessor half: see `pe_segment_get_impl`.
    let _window = crate::ffm_fast::AccessWindow::open();
    // Before the scope check: see `ffm_fast::note_validated_at`.
    let verdict_epoch = crate::ffm_fast::epoch();
    let addr = if ffm_bounds_before_scope() {
        // Round 13 wave 1 (lane mhffm): bounds, alignment, then the session.
        let addr = pe_segment_locate_addr(ctx, seg, offset, width)?;
        if native_misaligned(addr, align) {
            // As in `pe_segment_get_impl` (j27 wave 4, lane FFMMSG).
            drop(_window);
            return Err(native_misaligned_refusal(ctx, seg, Some(layout), offset, align as i64));
        }
        pe_segment_check_scope(ctx, seg)?;
        addr
    } else {
        let addr = pe_segment_access_addr(ctx, seg, offset, width)?;
        if native_misaligned(addr, align) {
            drop(_window);
            return Err(native_misaligned_refusal(ctx, seg, None, offset, align as i64));
        }
        addr
    };
    ffm_publish_verdict(ctx, seg, true, verdict_epoch);
    pe_publish_layout_verdict(layout, little, align, width, traits_epoch);

    // A value whose shape does not match the layout stays the silent no-op
    // this arm has always been.
    let Some(raw) = pe_raw_from_value(kind, value) else {
        return Ok(None);
    };
    let raw = if little == cfg!(target_endian = "little") {
        raw
    } else {
        pe_swap_width(raw, width)
    };
    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr); `pe_write_raw` writes unaligned. The width is
    // the layout kind's.
    unsafe { pe_write_raw(addr as *mut u8, width, raw) };
    Ok(None)
}

// --- SymbolLookup: load shared libraries and find symbols ---
// SymbolLookup synthetic: [0]=lib_index (Long — index into SharedVm.native_libraries), [1]=name

pub(crate) fn register_pe_symbol_lookup(r: &mut NativeMethodRegistry) {
    // Promote to `Bridge`: this function runs under whatever category was
    // ambient at the `register_pe_panama` call site, which defaults to
    // `SyntheticStub` (dropped entirely under strict-no-stubs / real-JDK
    // mode). These natives are real supporting glue for the Panama FFI
    // downcall path (real library loading via `ctx.load_native_library`,
    // real `Optional`/`Optional.empty()` wrapping) — not a placeholder — and
    // must survive that filtering so `phases_late.rs`'s `Linker.defaultLookup`
    // et al. (which allocate 0-field `SymbolLookup` objects) still resolve
    // `.find()` sanely via this implementation's `_ => -1` "unknown lookup"
    // fallback instead of a former duplicate stub here always returning a
    // bare Java `null`. See the `find`/`libraryLookup` doc comments below.
            let sl = "java/lang/foreign/SymbolLookup";

    // libraryLookup(path, arena) → SymbolLookup
    r.register(
        sl,
        "libraryLookup",
        "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
        |ctx, args| {
            // HotSpot's order (JDK 25 `SymbolLookup.libraryLookup(String, Arena)`):
            // the caller-sensitive native-access check, then `Utils.containsNullChars`
            // (a null name is ITS helpful NPE, a NUL is `IllegalArgumentException`),
            // then `requireNonNull(arena)` (bare NPE), then the load, whose failure is
            // `IllegalArgumentException("Cannot open library: " + name)`.
            if ffm_library_lookup_restricted_warn() {
                restricted_caller_check(ctx, "java/lang/foreign/SymbolLookup", "libraryLookup")?;
            } else {
                require_native_access(ctx, "libraryLookup")?;
            }
            let path_obj = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "Cannot invoke \"String.indexOf(int)\" because \"s\" is null"
                                .to_string(),
                        ),
                    }
                    .into())
                }
            };
            let path = ctx.read_string(path_obj).unwrap_or_default();
            if path.contains('\0') {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("Cannot open library: {path}"),
                }
                .into());
            }
            let arena = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let lib_index = match ctx.load_native_library(&path) {
                Ok(index) => {
                    ctx.set_native_library_loader(index, None);
                    index
                }
                Err(_) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("Cannot open library: {path}"),
                    }
                    .into())
                }
            };

            // gc-common w20-f: the name was stored through the lookup's
            // address from before the name's allocation. Pinned across it. The
            // arena (kept so `find` can answer `Already closed` as the JDK's
            // `MemorySegment.reinterpret(arena, ..)` does) is pinned likewise.
            let arena_pin = ctx.pin_native_root(arena);
            let lookup =
                match try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 3) {
                    Ok(l) => l,
                    Err(e) => {
                        ctx.unpin_native_roots(arena_pin);
                        return Err(e);
                    }
                };
            let lookup_pin = ctx.pin_native_root(lookup);
            let name_str = ctx.create_string(&path);
            let lookup: ObjectRef = ctx.read_native_pin(lookup_pin, lookup);
            let arena: ObjectRef = ctx.read_native_pin(arena_pin, arena);
            ctx.unpin_native_roots(lookup_pin);
            ctx.unpin_native_roots(arena_pin);
            ctx.set_field(lookup, 0, Value::Long(lib_index));
            ctx.set_field(lookup, 1, Value::Object(Some(name_str)));
            ctx.set_field(lookup, 2, Value::Object(Some(arena)));
            Ok(Some(Value::Object(Some(lookup))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // loaderLookup() → SymbolLookup (stub — returns lookup for default system library)
    r.register(
        sl,
        "loaderLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
        |ctx, _| {
            // The returned lookup searches every loaded library (index -1),
            // so obtaining one is itself a capability. Gated identically to
            // `libraryLookup` minus the `--enable-native-access` requirement,
            // which the JDK does not impose on `loaderLookup`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;
            let lookup = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
            ctx.set_field(lookup, 0, Value::Long(-1)); // -1 = default/system lookup
            Ok(Some(Value::Object(Some(lookup))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // find(name) → Optional<MemorySegment>
    r.register(
        sl,
        "find",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `Objects.requireNonNull(name)` on HotSpot: a BARE NullPointerException.
            let sym_name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let sym_name = ctx.read_string(sym_name_obj).unwrap_or_default();
            let lib_index = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => -1,
            };

            // A `loaderLookup()` receiver carries `lib_index == -1`, and
            // `find_native_symbol` treats that as "search every loaded
            // library", so `loaderLookup().find(x)` is an arbitrary-symbol
            // address oracle over libraries loaded by trusted code. Gate it
            // the same way as the load paths rather than leaving the read
            // side open when the load side is closed.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;

            // Round 14 wave 3 (lane ffm): the default lookup searches the C
            // runtime only, not every library the VM loaded
            // (`default_lookup_process_symbol`).
            let first = match default_lookup_process_symbol(ctx, this, lib_index, &sym_name) {
                Some(answer) => answer,
                None => ctx.find_native_symbol(lib_index, &sym_name),
            };
            let found = match first {
                Some(addr) => Some(addr),
                None => default_lookup_syslookup_symbol(ctx, this, lib_index, &sym_name)?,
            };
            match found {
                Some(addr) => {
                    // A `libraryLookup` receiver keeps its arena: the JDK answers the
                    // symbol through `MemorySegment.reinterpret(libArena, null)`, which
                    // checks the arena's session, so a closed one is `Already closed`
                    // (and only for a symbol that exists: a miss stays an empty Optional).
                    if lib_index >= 0 && ctx.object_num_fields(this) > 2 {
                        if let Value::Object(Some(arena)) = ctx.get_field(this, 2) {
                            if let Some(mut session) =
                                crate::phases_late::foreign_ffm::p67_arena_scope_for_reinterpret(
                                    ctx, arena,
                                )?
                            {
                                // A real JDK session (nio20 w3 L5): its own
                                // `checkValidState()`, which translates the
                                // raw check's `ScopedAccessError` to the
                                // `IllegalStateException` `reinterpret` throws.
                                if crate::phases_late::foreign_ffm::p67_session_is_real(
                                    ctx, session,
                                ) {
                                    ctx.invoke_virtual(session, "checkValidState", "()V", &[])?;
                                } else {
                                    crate::phases_late::foreign_ffm::p67_session_check_valid(
                                        ctx,
                                        &mut session,
                                    )?;
                                }
                            }
                        }
                    }
                    // Wrap address in MemorySegment and Optional.of()
                    //
                    // Round 14 wave 3 (lane ffm): the default and loader
                    // lookups answer `MemorySegment.ofAddress(addr)`, in the
                    // GLOBAL scope (`pe_zero_length_segment`,
                    // `CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE`). A synthetic-JDK
                    // `libraryLookup` receiver keeps no arena, so its symbols
                    // keep the empty scope slot.
                    let seg = if lib_index < 0 {
                        pe_zero_length_segment(ctx, addr as i64)?
                    } else {
                        let seg = alloc_segment_carrier(ctx, 6)?;
                        ctx.set_field(seg, 0, Value::Long(addr as i64));
                        ctx.set_field(seg, 1, Value::Long(0)); // function pointer — no byte size
                        ctx.set_field(seg, 2, Value::Object(None));
                        seg
                    };
                    // Round 14 wave 1 (lane ffm): JDK 25's lookups answer
                    // `MemorySegment.ofAddress(addr)` (`.reinterpret(arena,
                    // null)` for `libraryLookup`), which is NOT read-only; a
                    // read-only answer stayed read-only through `reinterpret`,
                    // so writing a C global variable -- jextract's generated
                    // setter, `find("var").get().reinterpret(4).set(..)` -- was
                    // `IllegalArgumentException` here.
                    let read_only = i32::from(false);
                    ctx.set_field(seg, 3, Value::Int(read_only));
                    ctx.set_field(seg, 4, Value::Int(1)); // alive
                    ctx.set_field(seg, 5, Value::Long(0));
                    // Return as Optional.of(segment)
                    //
                    // Round 14 wave 3 (lane ffm): the segment is pinned across
                    // the `Optional`'s allocation, which can move it; it was
                    // stored from its address from before.
                    let seg_pin = ctx.pin_native_root(seg);
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
                    let seg = ctx.read_native_pin(seg_pin, seg);
                    ctx.unpin_native_roots(seg_pin);
                    let opt = opt?;
                    ctx.set_field(opt, 0, Value::Object(Some(seg)));
                    Ok(Some(Value::Object(Some(opt))))
                }
                None => {
                    // Return Optional.empty()
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                    ctx.set_field(opt, 0, Value::Object(None));
                    Ok(Some(Value::Object(Some(opt))))
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `CRATONVM_FFM_NULL_ADDRESS_NPE` (default on; round 14 wave 1, lane ffm): a
/// Java `null` stored through an `ADDRESS` layout (`set`, `setAtIndex`) or
/// passed as a downcall's pointer argument is `NullPointerException`, as JDK
/// 25's `SharedUtils.unboxSegment` makes it. `0` restores the silent C null.
pub(crate) fn ffm_null_address_npe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The message of JDK 25's `NullPointerException` for a `null` ADDRESS value:
/// `SharedUtils.unboxSegment` calls `checkNative(segment)`, whose first
/// instruction pair is `aload_0; invokeinterface MemorySegment.isNative()`,
/// and the JDK's `SharedUtils` carries a `LocalVariableTable` naming the
/// parameter `segment` (read from the 25.0.3 `lib/modules` class bytes), so
/// the helpful NPE reads exactly this.
pub(crate) const FFM_NULL_ADDRESS_NPE_MESSAGE: &str =
    "Cannot invoke \"java.lang.foreign.MemorySegment.isNative()\" because \"segment\" is null";

/// `CRATONVM_FFM_NULL_ADDRESS_NPE_MESSAGE` (default on, both modes; round 14
/// wave 5, lane fixup2): a `null` ADDRESS value stored through a LAYOUT
/// VarHandle (`lang_invoke.rs` `layout_vh_set`) raises a
/// `NullPointerException` carrying [`FFM_NULL_ADDRESS_NPE_MESSAGE`], the JDK's
/// helpful message. VarHandle roads only: HotSpot 25's
/// `MemorySegment.set/setAtIndex(ADDRESS, .., null)` and a downcall's null
/// pointer argument raise a message-less NPE (measured,
/// `R14Fixup2AddressNullMessage`), so those sites keep `message: None`. `0`
/// restores each VarHandle site's previous message (`legacy`).
pub(crate) fn ffm_null_address_npe_message(legacy: Option<&str>) -> Option<String> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let jdk = *ON.get_or_init(|| {
        true
    });
    if jdk {
        Some(FFM_NULL_ADDRESS_NPE_MESSAGE.to_string())
    } else {
        legacy.map(str::to_string)
    }
}

/// `CRATONVM_FFM_ADDRESS_STORE_VALUE_FIRST` (default on; round 14 wave 3, lane
/// ffm): `MemorySegment.set(ADDRESS, ..)` refuses its VALUE (`null`, a heap
/// segment) before it checks the target segment is writable, the order of JDK
/// 25's `unboxSegment` filter. `0` checks read-only first, as before.
pub(crate) fn ffm_address_store_value_first() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE` (default on; round 14 wave 3, lane ffm):
/// a segment made from a raw pointer -- an ADDRESS read, a downcall's ADDRESS
/// return (`pe_zero_length_segment`) -- is in the global scope, as JDK 25's
/// `Utils.longToAddress` makes it, so its `scope()` is `Arena.global().scope()`.
/// `0` leaves the scope slot empty (`scope()` mints a fresh session per call).
pub(in crate::panama) fn ffm_address_global_scope() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}



/// `CRATONVM_FFM_DEFAULT_LOOKUP_SYSLOOKUP` (default on; round 14 wave 1, lane
/// ffm): a symbol `Linker.defaultLookup().find` misses in the loaded libraries
/// and the process's C runtime is looked up, on Windows, in the `funcs` table of
/// the JDK image's `syslookup.dll`, as JDK 25's `SystemLookup` does (the UCRT's
/// inline stdio functions: `snprintf`, `printf`, ...;
/// `r13w13-orch-default-lookup-misses-the-jdk-syslookup-symbols`). `0`
/// restores `Optional.empty()` for those.
pub(in crate::panama) fn ffm_default_lookup_syslookup() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}



/// The default lookup's first search, when `this` is a `Linker.defaultLookup()`
/// receiver: `NativeContext::find_process_native_symbol` (the C runtime only).
/// `None` for any other receiver, with the switch off, or when the context
/// cannot answer (the caller then searches as before).
pub(in crate::panama) fn default_lookup_process_symbol(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    lib_index: i64,
    name: &str,
) -> Option<Option<usize>> {
    if lib_index != -1
        || ctx.object_num_fields(this) < DEFAULT_LOOKUP_FIELDS
    {
        return None;
    }
    ctx.find_process_native_symbol(name)
}

/// Width of the `Linker.defaultLookup()` receiver
/// (`foreign_ffm`'s `defaultLookup` native): `[0]=lib_index (-1)`,
/// `[1]=name (null)`, `[2]=` [`DEFAULT_LOOKUP_SYSLOOKUP_SLOT`]. The third slot
/// is also what tells it from a `loaderLookup()` receiver (two slots, also
/// `-1`), which must not see `syslookup`.
pub(crate) const DEFAULT_LOOKUP_FIELDS: usize = 3;

/// The default lookup's `syslookup` table: `Long(0)` not loaded yet, `Long(p)`
/// (`p > 0`) the address of the library's `funcs` table, `Long(-1)`
/// unavailable (not Windows, no JDK image, or the load failed). Kept on the
/// receiver, so each lookup object loads it at most once.
pub(crate) const DEFAULT_LOOKUP_SYSLOOKUP_SLOT: usize = 2;

/// The JDK enum that names `syslookup.dll`'s `funcs` table, entry `i` being the
/// function of ordinal `i` (JDK 25 `SystemLookup.makeWindowsLookup`).
#[cfg(windows)]
pub(in crate::panama) const WINDOWS_FALLBACK_SYMBOLS: &str = "jdk/internal/foreign/SystemLookup$WindowsFallbackSymbols";

#[cfg(windows)]
extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        h_module: *mut std::ffi::c_void,
        lp_proc_name: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
}

/// Each VM's `syslookup` `funcs` table address, `-1` for unavailable (round 14
/// wave 3, lane ffm, FFM14-3). Keyed by `vm_identity` because the table comes
/// from the VM's own `java.home`; dropped by [`forget_vm_ffm_singletons`]. The
/// module itself stays loaded, as it always did. A `Scratch` leaf, no `ctx`
/// call under it.
pub(in crate::panama) fn syslookup_cell(// addr-keyed: not a heap address -- VM identity; dropped by forget_vm_ffm_singletons.
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, i64>> {
    use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
    // addr-keyed: the accessor's table (see above).
    static CELL: std::sync::OnceLock<OrderedPlMutex<std::collections::HashMap<usize, i64>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch))
}

/// The `funcs` table of the JDK image's `bin\syslookup.dll`, or `None`.
///
/// Loaded with a PRIVATE handle (`LoadLibraryW`), never entered in the VM's
/// native-library table, so `loaderLookup()`, `libraryLookup` searches and JNI
/// symbol resolution never see it. The module is never freed: HotSpot keeps it
/// loaded for the process too, and a second load only bumps its count.
#[cfg(windows)]
pub(in crate::panama) fn syslookup_funcs_table(ctx: &dyn NativeContext) -> Option<usize> {
    use std::os::windows::ffi::OsStrExt;
    let home = ctx.get_system_property("java.home")?;
    let path = std::path::Path::new(&home)
        .join("bin")
        .join("syslookup.dll");
    if !path.is_file() {
        return None;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: `module` is a live module handle (never freed) and the name is
    // NUL-terminated.
    let funcs = unsafe { GetProcAddress(module, b"funcs\0".as_ptr().cast()) };
    (!funcs.is_null()).then_some(funcs as usize)
}

/// `name`'s ordinal in the running JDK's `WindowsFallbackSymbols` (so the
/// table index is the JDK's own, never a copied list). Runs Java.
#[cfg(windows)]
pub(in crate::panama) fn windows_fallback_ordinal(ctx: &mut dyn NativeContext, name: &str) -> Option<usize> {
    let desc = format!("()[L{WINDOWS_FALLBACK_SYMBOLS};");
    let Ok(Some(Value::Object(Some(values)))) =
        ctx.invoke_static(WINDOWS_FALLBACK_SYMBOLS, "values", &desc, &[])
    else {
        return None;
    };
    if !ctx.object_is_array(values) {
        return None;
    }
    let pin = ctx.pin_native_root(values);
    let count = ctx.array_length(values);
    let mut found = None;
    for i in 0..count {
        let values = ctx.read_native_pin(pin, values);
        let Value::Object(Some(symbol)) = ctx.get_array_element(values, i) else {
            continue;
        };
        if let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(symbol, "name", "()Ljava/lang/String;", &[])
        {
            if ctx.read_string(s).as_deref() == Some(name) {
                found = Some(i);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    found
}

/// A symbol a `Linker.defaultLookup()` receiver (`this`, lib index `-1`)
/// missed in the loaded libraries and the process's C runtime: what JDK 25's
/// Windows `SystemLookup` answers from its fallback, the entry of
/// `syslookup.dll`'s `funcs` table at the name's `WindowsFallbackSymbols`
/// ordinal ([`ffm_default_lookup_syslookup`]). The DLL exports only that table,
/// not the names (round 14 wave 1 measured `snprintf` absent when it was
/// looked up by name). `Ok(None)` for any other receiver, with the switch off,
/// off Windows (glibc / libSystem already answer through the process lookup,
/// which is all `libsyslookup` re-exports), or for a name not in the table.
pub(in crate::panama) fn default_lookup_syslookup_symbol(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    lib_index: i64,
    name: &str,
) -> Result<Option<usize>, MethodCallFailed> {
    if lib_index != -1
        || ctx.object_num_fields(this) < DEFAULT_LOOKUP_FIELDS
        || !ffm_default_lookup_syslookup()
    {
        return Ok(None);
    }
    #[cfg(windows)]
    {
        let funcs = match ctx.get_field(this, DEFAULT_LOOKUP_SYSLOOKUP_SLOT) {
            Value::Long(p) if p > 0 => p as usize,
            Value::Long(0) => {
                // Round 14 wave 3 (lane ffm, FFM14-3): once per VM, not once
                // per `defaultLookup()` object (`syslookup_cell`).
                let vm = ctx.vm_identity();
                let cached = syslookup_cell().lock().get(&vm).copied();
                let table = match cached {
                    Some(table) => table,
                    None => {
                        let table = syslookup_funcs_table(&*ctx).map_or(-1, |p| p as i64);
                        syslookup_cell().lock().insert(vm, table);
                        table
                    }
                };
                ctx.set_field(this, DEFAULT_LOOKUP_SYSLOOKUP_SLOT, Value::Long(table));
                if table <= 0 {
                    return Ok(None);
                }
                table as usize
            }
            _ => return Ok(None),
        };
        // `this` is not used below: the ordinal lookup runs Java.
        let Some(ordinal) = windows_fallback_ordinal(ctx, name) else {
            return Ok(None);
        };
        // SAFETY: `funcs` is the table of a module that is never unloaded,
        // sized by the same enum (`WindowsFallbackSymbols.LAYOUT`), and
        // `ordinal` is below that enum's length.
        let addr = unsafe { std::ptr::read_unaligned((funcs as *const usize).add(ordinal)) };
        return Ok((addr != 0).then_some(addr));
    }
    #[cfg(not(windows))]
    {
        let _ = (ctx, this, name);
        Ok(None)
    }
}

// --- RawNativeLibraries: the real-JDK FFM native-library load path ---
//
// In real-JDK mode `java.lang.foreign.SymbolLookup.libraryLookup(name, arena)`
// runs the JDK's own bytecode (our higher-level `SymbolLookup.libraryLookup`
// override does not win over a concrete method body), which routes through
// `jdk.internal.loader.RawNativeLibraries`:
//
//   RawNativeLibraryImpl.open()  → RawNativeLibraries.load0(impl, name)  // dlopen/LoadLibrary
//   RawNativeLibraryImpl.find()  → NativeLibrary.findEntry0(handle, name) // dlsym/GetProcAddress
//   RawNativeLibraryImpl.close() → RawNativeLibraries.unload0(name, handle)// dlclose/FreeLibrary
//
// All three are real ACC_NATIVE methods, so a registry override always
// dispatches to them (unlike the concrete-bytecode methods above). Without
// them, any FFM downcall binding — e.g. Tomcat's `openssl_h` loading
// libssl/libcrypto — dies in `<clinit>` with
// `UnsatisfiedLinkError: RawNativeLibraries.load0`. Back them with CratonVM's
// existing native-library table (the same `load_native_library` /
// `find_native_symbol` the panama `SymbolLookup` above uses). This makes the
// VM behave like HotSpot: the load is attempted, and if the library is not
// present `load0` returns false (the caller returns null and Tomcat falls back
// to JSSE) rather than raising.
//
// NOTE: registered from `register_essential_natives` (the always-compiled,
// real-JDK path), NOT from `register_pe_panama` — the latter is only reached
// under `#[cfg(feature = "synthetic-jdk")]` and would be dead-code-eliminated
// in real-JDK mode, which is exactly where these natives are needed (real-JDK
// mode runs the JDK's own FFM bytecode, which calls these natives directly).
pub(crate) fn register_pe_raw_native_libraries(r: &mut NativeMethodRegistry) {
    let rnl = "jdk/internal/loader/RawNativeLibraries";

    // static native boolean load0(RawNativeLibraryImpl impl, String name)
    r.register(
        rnl,
        "load0",
        "(Ljdk/internal/loader/RawNativeLibraries$RawNativeLibraryImpl;Ljava/lang/String;)Z",
        |ctx, args| {
            let impl_obj = obj_arg(args, 0)?;
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            if ffm_library_lookup_restricted_warn() {
                raw_library_load_check(ctx)?;
            } else {
                require_native_access(ctx, "libraryLookup")?;
            }
            match ctx.load_native_library(&name) {
                Ok(lib_index) => {
                    ctx.set_native_library_loader(lib_index, None);
                    // Stash the library index as the opaque `handle` long.
                    // Offset by +1 so a valid index 0 never collides with the
                    // handle==0 "not loaded" sentinel that
                    // RawNativeLibraryImpl.open() checks before calling load0;
                    // findEntry0 below decodes it back.
                    ctx.set_field_by_name(impl_obj, "handle", Value::Long(lib_index + 1));
                    Ok(Some(Value::Int(1)))
                }
                // Match the real native: a failed load returns false (caller
                // returns null), it does NOT throw.
                Err(_) => Ok(Some(Value::Int(0))),
            }
        },
        NativeKind::Bridge,
    );

    // static native void unload0(String name, long handle)
    //
    // IMPLEMENTED (was a no-op) via `NativeContext::unload_native_library`, the
    // escalation this comment used to request. No new handle plumbing was
    // needed: `load0` above stashes `lib_index + 1`, which is exactly what
    // `RawNativeLibraryImpl.close()` hands back as `handle`, so the same `- 1`
    // decode `findEntry0` uses recovers the index.
    //
    // It is a LOGICAL unload, not a `dlclose`: the VM's library table is an
    // append-only `Vec<Library>` whose index IS the handle, so an entry cannot
    // be removed without renumbering every live handle. The VM implementation
    // therefore tombstones the index — subsequent `find_native_symbol` calls on
    // it fail, as they would against a closed handle — while leaving the mapping
    // resident. Staying mapped is the safe direction of the two errors: a
    // `RawNativeLibraries` handle can still back live function pointers (callers
    // cache `findEntry0` results, and any bound downcall stub holds one), so
    // unmapping underneath them segfaults, while not unmapping costs an
    // address-space mapping.
    //
    // Real `unload0` returns void and reports nothing, so the `false` an
    // implementation without an unload path returns is intentionally ignored.
    r.register(
        rnl,
        "unload0",
        "(Ljava/lang/String;J)V",
        |ctx, args| {
            let handle = match args.get(1) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            };
            // SBR5 ffmlink: NO LONGER tombstoned. With `SymbolLookup.libraryLookup`
            // running the JDK's own bytecode, a lookup used after its arena closed
            // calls `NativeLibrary.find` (-> `findEntry0`) FIRST and fails later,
            // in `MemorySegment.reinterpret(arena, ..)`, with `IllegalStateException:
            // Already closed`, because HotSpot's `dlclose` of a library the process
            // keeps referenced (libc, anything another loader holds) leaves
            // `dlsym` answering. A tombstone made `find` answer empty instead
            // (`Sbr5FfmlinkAcct lookup.lib.closed`). The mapping stays resident
            // either way (see above), so answering is the HotSpot-shaped error.
            let _ = handle;
            Ok(None)
        },
        NativeKind::Bridge,
    );

    // static native long findEntry0(long handle, String name)  (in NativeLibrary)
    r.register(
        "jdk/internal/loader/NativeLibrary",
        "findEntry0",
        "(JLjava/lang/String;)J",
        |ctx, args| {
            let handle = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            // `handle` is caller-supplied and is only `lib_index + 1`, a small
            // dense integer, so without this gate a guest can walk 1, 2, 3, …
            // and `dlsym` any library loaded by anyone in the process — a
            // symbol-address oracle that never passes the load-time check.
            // Same gate as the load paths, so trusted callers are unaffected:
            // it denies only under CRATONVM_UNTRUSTED_CODE or a SecurityManager
            // policy that withholds `loadLibrary.*`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "findEntry")?;
            // Undo the +1 offset applied by load0 to recover the library index.
            let lib_index = handle - 1;
            let addr = ctx.find_native_symbol(lib_index, &name).unwrap_or(0);
            Ok(Some(Value::Long(addr as i64)))
        },
        NativeKind::Bridge,
    );
}

// --- Linker: create downcall handles ---
//
// The carrier a downcall handle is allocated AS is a real
// `java/lang/invoke/MethodHandle`. See [`alloc_downcall_handle`] for why, and
// for the slot layout that used to be a five-field
// `java/lang/foreign/DowncallHandle`.

/// First slot of the downcall state on a [`DOWNCALL_CARRIER_CLASS`] carrier.
///
/// Anchored well past `lang_invoke.rs`'s synthetic MethodHandle window, for the
/// same reason that window is itself anchored at 16 rather than at the real
/// JDK's field count of 6: the window GREW once already (`MH_BOUND + 1` to
/// `MH_VARARGS + 1`, W7-19 §5.2), so a base chosen to sit exactly on top of
/// today's last slot is a collision waiting for the next combinator.
///
/// The gap between the two windows is not addressed by this file and is left
/// null by [`alloc_downcall_handle`] — see the null-fill there.
pub(crate) const DOWNCALL_BASE: usize = 32;

/// Discriminator slot: `Int(DOWNCALL_TAG_MAGIC)` on a downcall carrier and
/// nothing else. See [`is_downcall_handle`].
pub(crate) const DOWNCALL_TAG: usize = DOWNCALL_BASE;
/// Target function pointer (`Long`). Never dereferenced without
/// [`validated_fn_ptr`].
pub(crate) const DOWNCALL_FN_ADDR: usize = DOWNCALL_BASE + 1;
/// The `java/lang/foreign/FunctionDescriptor` this handle was linked against.
pub(crate) const DOWNCALL_DESCRIPTOR: usize = DOWNCALL_BASE + 2;
/// Index of the first variadic argument, or `-1` for a non-variadic call.
pub(crate) const DOWNCALL_FIRST_VARIADIC: usize = DOWNCALL_BASE + 3;
/// T5.6.3 cached `Box<Cif>` raw pointer as `u64` (`0` = not yet built).
pub(crate) const DOWNCALL_CIF: usize = DOWNCALL_BASE + 4;
/// `Int(1)` when `Linker.Option.captureCallState` added a leading
/// `MemorySegment` parameter.
pub(crate) const DOWNCALL_CAPTURE_CALL_STATE: usize = DOWNCALL_BASE + 5;
/// gcd d5/f: `Int(1)` when the handle was linked with
/// `Linker.Option.critical(..)` ([`downcall_set_critical`]). A critical
/// downcall stays a counted mutator for its C call even with
/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` on, as HotSpot keeps it in Java state
/// (no thread-state transition; a pause waits for it).
pub(crate) const DOWNCALL_CRITICAL: usize = DOWNCALL_BASE + 6;
/// JIT round 16 wave 11 (lane ffmcost): the handle's plain-call memo
/// (`Long`), written by [`pe_downcall_invoke`]'s first call when
/// `CRATONVM_FFM_DOWNCALL_PLAIN_MEMO` is on: `0` undecided,
/// [`PLAIN_MEMO_NOT_PLAIN`], or a [`PLAIN_MEMO_TAG`] encoding of the
/// primitive parameter and return kinds (see [`plain_memo_encode`]).
pub(crate) const DOWNCALL_PLAIN: usize = DOWNCALL_BASE + 7;
/// Width [`alloc_downcall_handle`] requests.
pub(crate) const DOWNCALL_SLOT_COUNT: usize = DOWNCALL_PLAIN + 1;

/// `"DCH1"`. Distinctive on purpose: [`is_downcall_handle`] is asked about
/// arbitrary references, and a magic that could plausibly be a stored `int`
/// would make the predicate answer yes for a handle that merely happens to be
/// wide enough.
pub(in crate::panama) const DOWNCALL_TAG_MAGIC: i32 = 0x4443_4831;

/// The class a downcall handle is allocated as.
///
/// **This is the whole of the `--real-jdk` fix.** It used to be
/// `java/lang/foreign/DowncallHandle`, a name NO JDK image declares — it is in
/// `native-api/src/no_image_receiver.rs`'s `NO_IMAGE_JDK_RECEIVERS`. Strict
/// mode refuses to fabricate such a class, correctly, so
/// `Linker.downcallHandle` died with `NoClassDefFoundError:
/// java/lang/foreign/DowncallHandle` and took all of Panama with it. The
/// refusal was right; the caller surviving to ask for it was the defect.
pub(crate) const DOWNCALL_CARRIER_CLASS: &str = "java/lang/invoke/MethodHandle";

/// The class a downcall handle is allocated as over a real JDK image (JVMS
/// 6.5, nio21 close): `MethodHandle` is ABSTRACT, so an instance of it is a
/// receiver no bytecode could have produced, and HotSpot's own handle is a
/// concrete `BoundMethodHandle$Species_*`. A `DirectMethodHandle` is concrete,
/// has the same `type` first field, and is castable to `MethodHandle` and
/// signature-polymorphic through its name, so the three compensations
/// [`DOWNCALL_CARRIER_CLASS`] records stay deleted. The bare `MethodHandle` is
/// still what a synthetic image (no real `DirectMethodHandle`) mints.
pub(crate) const DOWNCALL_CONCRETE_CLASS: &str = "java/lang/invoke/DirectMethodHandle";

/// The class [`alloc_downcall_handle`] mints on this image.
pub(in crate::panama) fn downcall_carrier_class(ctx: &dyn NativeContext) -> &'static str {
    if ctx.would_fabricate_synthetic_stub(DOWNCALL_CONCRETE_CLASS)
        || ctx.is_class_synthetic_stub(DOWNCALL_CONCRETE_CLASS)
    {
        DOWNCALL_CARRIER_CLASS
    } else {
        DOWNCALL_CONCRETE_CLASS
    }
}

/// Is `obj` a downcall handle?
///
/// Replaces the `class_name == "java/lang/foreign/DowncallHandle"` test that
/// the consumers in `lang_invoke.rs` used while the carrier had a class of its
/// own. Three screens, cheapest first:
///
/// 1. **Width.** Every MethodHandle `lang_invoke.rs` mints is 22 slots, so this
///    rejects the entire ordinary population on one integer compare — and it is
///    what makes the tag read safe rather than an out-of-bounds access. Same
///    discipline as `mh_is_varargs_collector`.
/// 2. **Exact class.** Our carrier is allocated as `java/lang/invoke/
///    MethodHandle` itself, so a real-JDK `BoundMethodHandle$Species_L` or
///    `DirectMethodHandle$Constructor` is not one of ours no matter how wide.
/// 3. **Tag.** [`DOWNCALL_TAG_MAGIC`], written by [`alloc_downcall_handle`].
pub(crate) fn is_downcall_handle(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.object_num_fields(obj) > DOWNCALL_TAG
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(obj))
            .as_deref()
            .is_some_and(|n| n == DOWNCALL_CARRIER_CLASS || n == DOWNCALL_CONCRETE_CLASS)
        && matches!(ctx.get_field(obj, DOWNCALL_TAG), Value::Int(tag) if tag == DOWNCALL_TAG_MAGIC)
}

/// Mint the `MethodHandle` that `Linker.downcallHandle` hands back.
///
/// # Why a real `MethodHandle` and not a class of our own
///
/// `Linker.downcallHandle` is DECLARED to return `java.lang.invoke.
/// MethodHandle`, and on a real JDK that is what it returns. Returning an
/// invented `java/lang/foreign/DowncallHandle` instead cost three separate
/// compensations, every one of which this carrier deletes rather than moves:
///
/// * `vm_exec.rs::is_method_handle_signature_polymorphic_receiver` had to name
///   the class, or `invokeExact` would not link signature-polymorphically. A
///   real `java/lang/invoke/MethodHandle` already satisfies that predicate's
///   first arm. (ES-FAIL-FAMILY-20260710 is that compensation being added.)
/// * `typecheck.rs` had to hard-code that the invented class is castable to
///   `java/lang/invoke/MethodHandle`. Now it IS one.
/// * `lang_invoke.rs`'s `asType` had to refuse to write the `type` field,
///   because on the compact layout slot 0 was the FUNCTION ADDRESS and writing
///   a `MethodType` over it turned a later void `invokeExact` into a silent
///   no-op. Here slot 0 is the genuine `type` field and the write is correct.
///
/// # The `type` field is populated here, not lazily
///
/// It has to be. `pe_downcall_type` was registered as `DowncallHandle.type()`;
/// with a `MethodHandle` receiver, `type()` resolves to `lang_invoke.rs`'s
/// registration on `java/lang/invoke/MethodHandle`, which reads slot 0 and
/// falls back to `()V` when it is null. A downcall that reported `()V` would
/// mis-link every signature-polymorphic call site, so the MethodType is
/// derived from the FunctionDescriptor and stored at mint time.
///
/// # GC
///
/// `build_method_type_from_descriptor` allocates, so both the carrier and the
/// caller's `descriptor` are pinned across it and re-read after.
pub(crate) fn alloc_downcall_handle(
    ctx: &mut dyn NativeContext,
    fn_addr: i64,
    descriptor: ObjectRef,
    first_variadic: i64,
    capture_call_state: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // Linker.downcallHandle is restricted, but JDK 25's default is WARN,
    // not DENY. Check before minting the executable handle. Invocation must
    // apply the same policy rather than demanding an explicit module grant.
    // Pinned BEFORE the allocation below, which can itself collect.
    let desc_pin = ctx.pin_native_root(descriptor);
    if let Err(error) = restricted_downcall_link_check(ctx) {
        ctx.unpin_native_roots(desc_pin);
        return Err(error);
    }
    let carrier = downcall_carrier_class(&*ctx);
    let handle = try_alloc_concurrent_synthetic(ctx, carrier, DOWNCALL_SLOT_COUNT)?;
    let handle_pin = ctx.pin_native_root(handle);
    let descriptor = ctx.read_native_pin(desc_pin, descriptor);

    let method_descriptor = downcall_carrier_descriptor(
        ctx,
        descriptor,
        capture_call_state,
        fn_addr == DOWNCALL_ADDRESS_FROM_ARG0,
    )?;
    let method_type =
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &method_descriptor)?
            .ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!(
                        "Unable to construct MethodType for DowncallHandle descriptor \
                     {method_descriptor}"
                    ),
                }
                .into()
            })?;

    let handle = ctx.read_native_pin(handle_pin, handle);
    let descriptor = ctx.read_native_pin(desc_pin, descriptor);

    // Everything between the real JDK's `type` field and our base belongs to
    // neither of us: slots 1-5 are the JDK's other MethodHandle fields and
    // 16-21 are `lang_invoke.rs`'s synthetic MethodHandle window. Null rather
    // than left raw, so a reader that reaches one — `mh_is_varargs_collector`
    // tests `Int(1)` at slot 21 and would otherwise be interpreting whatever
    // the allocator left there — gets a definite "absent" instead of padding.
    // Null is the safe filler in both directions: a reference slot reads as a
    // null oop and an int-shaped reader's `match` falls to its default arm.
    for slot in 1..DOWNCALL_BASE {
        ctx.set_field(handle, slot, Value::Object(None));
    }
    // Slot 0 is the real JDK `MethodHandle.type` field.
    ctx.set_field(handle, 0, Value::Object(Some(method_type)));

    ctx.set_field(handle, DOWNCALL_TAG, Value::Int(DOWNCALL_TAG_MAGIC));
    ctx.set_field(handle, DOWNCALL_FN_ADDR, Value::Long(fn_addr));
    ctx.set_field(handle, DOWNCALL_DESCRIPTOR, Value::Object(Some(descriptor)));
    ctx.set_field(handle, DOWNCALL_FIRST_VARIADIC, Value::Long(first_variadic));
    ctx.set_field(handle, DOWNCALL_CIF, Value::Long(0));
    ctx.set_field(
        handle,
        DOWNCALL_CAPTURE_CALL_STATE,
        Value::Int(capture_call_state as i32),
    );
    // gcd d5/f: not critical unless the linking site says so afterwards
    // (`downcall_set_critical`).
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(0));
    // JIT round 16 wave 11 (lane ffmcost): undecided until the first call.
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));

    ctx.unpin_native_roots(desc_pin);
    Ok(handle)
}

/// gcd d5/f: is `option` a `Linker.Option.critical(..)`? The real JDK's
/// `jdk.internal.foreign.abi.LinkerOptions$Critical` (an enum in JDK 25: either
/// constant), or this file's synthetic `Linker$Option` of kind 1
/// (`register_pe_linker_options`, which the shipping image registers).
pub(crate) fn downcall_option_is_critical(ctx: &dyn NativeContext, option: ObjectRef) -> bool {
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(option))
        .as_deref()
    {
        Some("jdk/internal/foreign/abi/LinkerOptions$Critical") => true,
        Some("java/lang/foreign/Linker$Option") => {
            matches!(ctx.get_field(option, 0), Value::Int(1))
        }
        _ => false,
    }
}

/// JIT round 16 wave 13 (lane ffmseg): is the critical `option` the one that
/// allows heap access -- `Linker.Option.critical(true)`? The real JDK's
/// `LinkerOptions$Critical.ALLOW_HEAP` (ordinal 0: `allowHeapAccess()` is
/// `ordinal() == 0`), or the synthetic `Linker$Option` of kind 1 whose payload
/// (slot 1) is non-zero. `false` for anything else.
pub(crate) fn downcall_option_allows_heap(ctx: &dyn NativeContext, option: ObjectRef) -> bool {
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(option))
        .as_deref()
    {
        Some("jdk/internal/foreign/abi/LinkerOptions$Critical") => {
            matches!(ctx.get_field_by_name(option, "ordinal"), Value::Int(0))
        }
        Some("java/lang/foreign/Linker$Option") => {
            matches!(ctx.get_field(option, 0), Value::Int(1))
                && matches!(ctx.get_field(option, 1), Value::Long(n) if n != 0)
        }
        _ => false,
    }
}

/// [`DOWNCALL_CRITICAL`] of a handle linked `critical(true)`
/// ([`downcall_option_allows_heap`]); `1` is `critical(false)`.
pub(in crate::panama) const DOWNCALL_CRITICAL_ALLOW_HEAP: i32 = 2;

/// gcd d5/f: mark a freshly minted downcall handle critical
/// ([`DOWNCALL_CRITICAL`]). Called by a linking site whose option array held a
/// [`downcall_option_is_critical`] option, with the handle
/// [`alloc_downcall_handle`] returned. `allow_heap` (wave 13, lane ffmseg):
/// the option was `critical(true)` ([`downcall_option_allows_heap`]).
pub(crate) fn downcall_set_critical(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    allow_heap: bool,
) {
    let mark = if allow_heap {
        DOWNCALL_CRITICAL_ALLOW_HEAP
    } else {
        1
    };
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(mark));
}

/// gcd d5/f: was `handle` linked critical ([`DOWNCALL_CRITICAL`]), with or
/// without heap access?
pub(in crate::panama) fn downcall_is_critical(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    matches!(
        ctx.get_field(handle, DOWNCALL_CRITICAL),
        Value::Int(1 | DOWNCALL_CRITICAL_ALLOW_HEAP)
    )
}

/// JIT round 16 wave 13 (lane ffmseg): was `handle` linked
/// `critical(true)`, so a heap segment may be passed for an `ADDRESS`
/// parameter (`CRATONVM_FFM_CRITICAL_HEAP_ACCESS`)?
pub(in crate::panama) fn downcall_allows_heap_access(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    matches!(
        ctx.get_field(handle, DOWNCALL_CRITICAL),
        Value::Int(DOWNCALL_CRITICAL_ALLOW_HEAP)
    )
}

/// [`DOWNCALL_FN_ADDR`] of an ADDRESS-LESS handle
/// (`Linker.downcallHandle(FunctionDescriptor, Linker.Option...)`): the
/// handle's first parameter is the target `MemorySegment`, read per call by
/// [`pe_downcall_invoke`]. No code address is `i64::MIN`, and
/// `validated_fn_ptr` would refuse it if one reached a call.
pub(crate) const DOWNCALL_ADDRESS_FROM_ARG0: i64 = i64::MIN;

/// Is `handle` address-less ([`DOWNCALL_ADDRESS_FROM_ARG0`])?
pub(in crate::panama) fn downcall_address_from_arg0(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    downcall_fn_addr(ctx, handle) == DOWNCALL_ADDRESS_FROM_ARG0
}

/// `Linker.downcallHandle(FunctionDescriptor, Linker.Option...)`: the handle
/// the three-argument form builds, without a bound address; its `type()`
/// gains a leading `MemorySegment` (the target, as on HotSpot)
/// (`r12w8-orch-probe-findings-on-w8` item 3).
pub(crate) fn pe_downcall_handle_unbound(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let descriptor = obj_arg(args, 1)?;
    // Round 13 wave 9 (lane ffm5): `checkLayouts(function)` before the options
    // (`CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS`). Plain reads.
    if crate::panama_upcall::downcall_check_layouts_enabled() {
        crate::panama_upcall::check_linker_layouts(ctx, descriptor)?;
    }
    let (variadic_fixed, capture_call_state) = match args.get(2) {
        Some(Value::Object(Some(opts))) => downcall_scan_options(ctx, *opts),
        _ => (-1, false),
    };
    // gcd d5/f: read before the allocation below, which can move `opts`.
    let critical = match args.get(2) {
        Some(Value::Object(Some(opts))) => downcall_options_critical(ctx, *opts),
        _ => false,
    };
    // Wave 13 (lane ffmseg): `critical(true)`, read with it.
    let allow_heap = match args.get(2) {
        Some(Value::Object(Some(opts))) => downcall_options_allow_heap(ctx, *opts),
        _ => false,
    };
    let handle = alloc_downcall_handle(
        ctx,
        DOWNCALL_ADDRESS_FROM_ARG0,
        descriptor,
        variadic_fixed,
        capture_call_state,
    )?;
    if critical {
        downcall_set_critical(ctx, handle, allow_heap);
    }
    Ok(Some(Value::Object(Some(handle))))
}

/// The `Linker.Option[]` scan the option-taking `downcallHandle` rows share:
/// `(first variadic index or -1, captures call state)`. Synthetic option
/// layout: field 0 = kind (`Int`, 0 = `firstVariadicArg`), field 1 = payload.
pub(in crate::panama) fn downcall_scan_options(ctx: &dyn NativeContext, opts: ObjectRef) -> (i64, bool) {
    let mut variadic_fixed: i64 = -1;
    let mut capture_call_state = false;
    for i in 0..ctx.array_length(opts) {
        if let Value::Object(Some(opt)) = ctx.get_array_element(opts, i) {
            if downcall_option_captures_call_state(ctx, opt) {
                capture_call_state = true;
            }
            if let Some(index) = downcall_option_first_variadic(ctx, opt) {
                variadic_fixed = index;
            }
        }
    }
    (variadic_fixed, capture_call_state)
}

/// gcd d5/f: the index a `Linker.Option.firstVariadicArg(index)` names, or
/// `None` for any other option.
///
/// Class-aware. The scans used to read EVERY option through the synthetic
/// `Linker$Option` layout (field 0 = kind, `0` meaning firstVariadicArg; field
/// 1 = payload), but under the real JDK the options are
/// `jdk.internal.foreign.abi.LinkerOptions` records with ONE field each: a real
/// `FirstVariadicArg(int index)` read as "kind = index" was ignored unless its
/// index was 0 (and then its "payload" was read past its only field)
/// (`gcd-d5f-real-jdk-first-variadic-arg-option-is-ignored-FIXED-20260928.md`).
pub(crate) fn downcall_option_first_variadic(
    ctx: &dyn NativeContext,
    option: ObjectRef,
) -> Option<i64> {
    let int_field = |slot: usize| match ctx.get_field(option, slot) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(v as i64),
        _ => None,
    };
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(option))
        .as_deref()
    {
        Some("jdk/internal/foreign/abi/LinkerOptions$FirstVariadicArg") => int_field(0),
        Some("java/lang/foreign/Linker$Option") => {
            if matches!(ctx.get_field(option, 0), Value::Int(0)) {
                Some(int_field(1).unwrap_or(-1))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// gcd d5/f: does the `Linker.Option[]` `opts` hold a critical option
/// ([`downcall_option_is_critical`])?
pub(in crate::panama) fn downcall_options_critical(ctx: &dyn NativeContext, opts: ObjectRef) -> bool {
    (0..ctx.array_length(opts)).any(|i| {
        matches!(ctx.get_array_element(opts, i),
            Value::Object(Some(opt)) if downcall_option_is_critical(ctx, opt))
    })
}

/// JIT round 16 wave 13 (lane ffmseg): does `opts` hold `critical(true)`
/// ([`downcall_option_allows_heap`])?
pub(in crate::panama) fn downcall_options_allow_heap(ctx: &dyn NativeContext, opts: ObjectRef) -> bool {
    (0..ctx.array_length(opts)).any(|i| {
        matches!(ctx.get_array_element(opts, i),
            Value::Object(Some(opt)) if downcall_option_allows_heap(ctx, opt))
    })
}

/// The function pointer this handle targets, or 0.
pub(crate) fn downcall_fn_addr(ctx: &dyn NativeContext, handle: ObjectRef) -> i64 {
    match ctx.get_field(handle, DOWNCALL_FN_ADDR) {
        Value::Long(n) => n,
        _ => 0,
    }
}

/// The `FunctionDescriptor` this handle was linked against.
pub(in crate::panama) fn downcall_descriptor(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(handle, DOWNCALL_DESCRIPTOR) {
        Value::Object(Some(d)) => Some(d),
        _ => None,
    }
}

pub(crate) fn register_pe_linker_options(r: &mut NativeMethodRegistry) {
    // `Linker.Option.critical(boolean)` is JDK 22+ bytecode answering the real
    // `LinkerOptions$Critical` constants; this stand-in is for the Java 21
    // image that lacks it. On a newer image it shadowed that bytecode and
    // handed `LinkerOptions.forShared` an instance of the INTERFACE
    // `Linker$Option` (`ClassCastException` to `LinkerOptionImpl`:
    // `R16FfmdoorBind`, `R16FfmcostPlainMemo`, `R16FfmsegBind`, both modes).
    // Real class bytes are authoritative: yield to them.
    if r.real_jdk_at_least(22) {
        return;
    }
    let option = "java/lang/foreign/Linker$Option";
    r.register(
        option,
        "critical",
        "(Z)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let enabled = args.first().and_then(|v| v.as_int()).unwrap_or(0) != 0;
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(1)); // kind = critical
            ctx.set_field(opt, 1, Value::Long(enabled as i64));
            Ok(Some(Value::Object(Some(opt))))
        }, NativeKind::SyntheticStub,
    );
}

/// True for the real-JDK option that adds a leading MemorySegment state
/// argument to a downcall handle.
pub(crate) fn downcall_option_captures_call_state(
    ctx: &dyn NativeContext,
    option: ObjectRef,
) -> bool {
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(option))
            .as_deref(),
        Some("jdk/internal/foreign/abi/LinkerOptions$CaptureCallState")
    )
}

pub(in crate::panama) fn register_pe_linker(r: &mut NativeMethodRegistry) {
    let linker = "java/lang/foreign/Linker";

    // nativeLinker() → Linker
    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _| {
            let l = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 1)?;
            Ok(Some(Value::Object(Some(l))))
        }, NativeKind::SyntheticStub,
    );

    // downcallHandle(MemorySegment address, FunctionDescriptor desc) → MethodHandle
    //
    // Layout and carrier class: see `alloc_downcall_handle`.
    r.register(linker, "downcallHandle", "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        let addr_seg = obj_arg(args, 1)?;
        let descriptor = obj_arg(args, 2)?;
        let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

        let handle = alloc_downcall_handle(ctx, fn_addr, descriptor, -1, false)?;
        Ok(Some(Value::Object(Some(handle))))
    }, NativeKind::SyntheticStub);

    // downcallHandle with Linker.Option[] for variadic etc. (NEW-18.3).
    r.register(linker, "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let addr_seg = obj_arg(args, 1)?;
            let descriptor = obj_arg(args, 2)?;
            let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

            // Scan the option array for a firstVariadicArg option (kind=0).
            // Linker.Option synthetic layout: field 0 = kind (Int), field 1 = payload (Long).
            let mut variadic_fixed: i64 = -1;
            let mut capture_call_state = false;
            // gcd d5/f: read before the allocation below, which can move `opts`.
            let critical = match args.get(3) {
                Some(Value::Object(Some(opts))) => downcall_options_critical(ctx, *opts),
                _ => false,
            };
            // Wave 13 (lane ffmseg): `critical(true)`, read with it.
            let allow_heap = match args.get(3) {
                Some(Value::Object(Some(opts))) => downcall_options_allow_heap(ctx, *opts),
                _ => false,
            };
            if let Some(Value::Object(Some(opts))) = args.get(3) {
                let n = ctx.array_length(*opts);
                for i in 0..n {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
                        if downcall_option_captures_call_state(ctx, opt) {
                            capture_call_state = true;
                        }
                        // gcd d5/f: class-aware (see the function).
                        if let Some(index) = downcall_option_first_variadic(ctx, opt) {
                            variadic_fixed = index;
                        }
                    }
                }
            }

            if crate::nbflags().dbg_linker {
                eprintln!(
                    "[PANAMA_LINKER] option downcall addr=0x{fn_addr:x} options={}",
                    args.get(3).is_some()
                );
            }
            let handle = alloc_downcall_handle(
                ctx,
                fn_addr,
                descriptor,
                variadic_fixed,
                capture_call_state,
            )?;
            if critical {
                downcall_set_critical(ctx, handle, allow_heap);
            }
            Ok(Some(Value::Object(Some(handle))))
        }, NativeKind::SyntheticStub,
    );

    // Linker.Option.firstVariadicArg(int n) → Linker$Option synthetic (kind=0, payload=n)
    r.register(
        "java/lang/foreign/Linker$Option",
        "firstVariadicArg",
        "(I)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let n = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(0)); // kind = firstVariadicArg
            ctx.set_field(opt, 1, Value::Long(n as i64)); // payload
            Ok(Some(Value::Object(Some(opt))))
        }, NativeKind::SyntheticStub,
    );
    register_pe_linker_options(r);

    // DowncallHandle.invoke(Object... args) → Object
    //
    // UNREACHABLE since the carrier became a real `java/lang/invoke/
    // MethodHandle` ([`alloc_downcall_handle`]): nothing in the VM allocates a
    // `java/lang/foreign/DowncallHandle` any more, so no receiver can ever
    // resolve to these four rows. Invocation now arrives at `lang_invoke.rs`'s
    // `MethodHandle.invoke`/`invokeExact`, whose `mh_dispatch` routes downcalls
    // to `pe_downcall_invoke` before decoding the generic MethodHandle layout.
    //
    // NOT DELETED HERE, deliberately. These four rows are frozen by name in
    // `scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
    // `jdk-only-gated-never-delete.tsv` and `jdk-only-dead-everywhere-GATED.tsv`,
    // and removing a registration those gates enumerate is a census change that
    // has to be made with the gate run, not alongside a behaviour change. The
    // rows are inert in the meantime, which is the safe direction: a dead
    // registration decides nothing, whereas deleting one the gate still expects
    // reddens the gate for a reason unrelated to this fix.
    //
    // Do NOT re-point these at `java/lang/invoke/MethodHandle`. `register` is
    // last-write-wins on (class, method, descriptor), so registering
    // `pe_downcall_invoke` on `MethodHandle.invoke([Ljava/lang/Object;)…` would
    // silently replace `lang_invoke.rs`'s body for EVERY method handle in the
    // VM, not only for downcalls.
    let dh = "java/lang/foreign/DowncallHandle";
    r.register(
        dh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke, NativeKind::SyntheticStub,
    );
    r.register(
        dh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke, NativeKind::SyntheticStub,
    );
    r.register(
        dh,
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke, NativeKind::SyntheticStub,
    );
    r.register(
        dh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        pe_downcall_type, NativeKind::SyntheticStub,
    );

    // upcallHandle(target, descriptor, arena) → MemorySegment wrapping trampoline address
    r.register(linker, "upcallHandle",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/MemorySegment;",
        pe_upcall_handle, NativeKind::SyntheticStub);

    // UpcallStub.invoke — dispatches from a trampoline back into Java
    let us = "java/lang/foreign/UpcallStub";
    r.register(
        us,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_upcall_invoke, NativeKind::SyntheticStub,
    );
}

/// The JVM method descriptor a downcall handle's `MethodType` is built from.
///
/// Derived from the `FunctionDescriptor`'s layouts rather than exposing an
/// untyped `Object[]` invoker, because a signature-polymorphic call site links
/// against whatever `type()` reports.
///
/// Allocation-free (reads layout fields only), so [`alloc_downcall_handle`]
/// may call it before it has anything pinned.
pub(in crate::panama) fn downcall_carrier_descriptor(
    ctx: &dyn NativeContext,
    descriptor: ObjectRef,
    captures_call_state: bool,
    address_first: bool,
) -> Result<String, MethodCallFailed> {
    use crate::panama_libffi as plf;

    let mut method_descriptor = String::from("(");
    // An address-less handle's target comes first, before the capture state
    // (HotSpot's `DowncallLinker` order).
    if address_first {
        method_descriptor.push_str("Ljava/lang/foreign/MemorySegment;");
    }
    // Round 13 wave 5 (lane ffm3): a by-value group return takes a leading
    // `SegmentAllocator`, after the address and before the capture state
    // (`Linker.downcallHandle`; `SharedUtils.adaptDowncallForIMR`).
    if downcall_returns_group(ctx, plf::descriptor_return_layout(ctx, descriptor))
    {
        method_descriptor.push_str("Ljava/lang/foreign/SegmentAllocator;");
    }
    if captures_call_state {
        method_descriptor.push_str("Ljava/lang/foreign/MemorySegment;");
    }
    for layout in plf::descriptor_param_layouts(ctx, descriptor) {
        let layout = layout.ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: "DowncallHandle FunctionDescriptor has null parameter layout".into(),
            }
            .into()
        })?;
        method_descriptor.push_str(downcall_layout_carrier_descriptor(ctx, layout)?);
    }
    method_descriptor.push(')');
    match plf::descriptor_return_layout(ctx, descriptor) {
        Some(layout) => {
            method_descriptor.push_str(downcall_layout_carrier_descriptor(ctx, layout)?)
        }
        None => method_descriptor.push('V'),
    }
    Ok(method_descriptor)
}

/// Return the `MethodType` represented by a downcall handle.
///
/// **Now a fallback, not the primary path.** While the carrier was a class of
/// its own this was registered as `DowncallHandle.type()`. On a real
/// `java/lang/invoke/MethodHandle` carrier, `type()` resolves to
/// `lang_invoke.rs`'s registration, which reads the real `type` field at slot
/// 0 — populated by [`alloc_downcall_handle`] for exactly that reason. This
/// stays because the registration on the old receiver is still present (see
/// the note at the `DowncallHandle` registration block) and because it is the
/// one place the descriptor→`MethodType` derivation is spelled out.
pub(crate) fn pe_downcall_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let descriptor = downcall_descriptor(ctx, handle).ok_or_else(|| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "DowncallHandle has no FunctionDescriptor".into(),
        }
        .into()
    })?;
    let captures = downcall_handle_captures_call_state(ctx, handle);
    let method_descriptor = downcall_carrier_descriptor(
        ctx,
        descriptor,
        captures,
        downcall_address_from_arg0(ctx, handle),
    )?;

    let method_type =
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &method_descriptor)?
            .ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!(
                "Unable to construct MethodType for DowncallHandle descriptor {method_descriptor}"
            ),
                }
                .into()
            })?;
    Ok(Some(Value::Object(Some(method_type))))
}

/// The JVM descriptor letter a layout's carrier contributes to a downcall's
/// `MethodType`.
///
/// **Takes the LAYOUT, not a bare kind, and returns a `Result`.** It used to
/// take an `i32` and answer `"Ljava/lang/foreign/MemorySegment;"` for anything
/// it did not recognise — including `panama_libffi::LAYOUT_UNKNOWN`, whose
/// whole purpose is to stop an undecodable carrier from getting a plausible
/// answer. A wrong descriptor here is not a local wrong answer: it is the
/// signature the `MethodType` is built from, so every argument after it lands
/// in the wrong slot kind. Refusing needs the layout's class name to be
/// actionable, which is why the parameter changed.
pub(in crate::panama) fn downcall_layout_carrier_descriptor(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
) -> Result<&'static str, MethodCallFailed> {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    Ok(match kind {
        LAYOUT_BOOLEAN => "Z",
        LAYOUT_BYTE => "B",
        LAYOUT_SHORT => "S",
        LAYOUT_CHAR => "C",
        LAYOUT_INT => "I",
        LAYOUT_LONG => "J",
        LAYOUT_FLOAT => "F",
        LAYOUT_DOUBLE => "D",
        crate::panama_libffi::LAYOUT_UNKNOWN => {
            return Err(unclassifiable_access_layout(
                ctx,
                layout,
                "downcall carrier",
            ))
        }
        // ADDRESS and aggregate layouts use the FFM MemorySegment carrier.
        _ => "Ljava/lang/foreign/MemorySegment;",
    })
}

pub(in crate::panama) fn downcall_handle_captures_call_state(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    matches!(ctx.get_field(handle, DOWNCALL_CAPTURE_CALL_STATE), Value::Int(flag) if flag != 0)
}

/// gcd d5/f: a downcall's capturable call state, read the moment `ffi_call`
/// returned, in `Linker.Option.captureStateLayout()` order -- Linux
/// `[i4(errno)]`; Windows `[i4(GetLastError)i4(WSAGetLastError)i4(errno)]`
/// (HotSpot 25's layout, printed on the Windows host). Other targets capture
/// nothing, as before.
#[derive(Clone, Copy)]
pub(in crate::panama) struct CapturedCallState(pub(in crate::panama) [i32; CAPTURED_STATE_WORDS]);

#[cfg(windows)]
pub(in crate::panama) const CAPTURED_STATE_WORDS: usize = 3;
#[cfg(not(windows))]
pub(in crate::panama) const CAPTURED_STATE_WORDS: usize = 1;

impl CapturedCallState {
    /// Read the calling thread's state now. Call straight after the C call:
    /// anything in between may overwrite it.
    // On Linux and Windows a `return` precedes the fallback tail.
    #[allow(unreachable_code)]
    #[inline(always)]
    pub(in crate::panama) fn now() -> Self {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `__errno_location` answers this thread's errno slot.
            return CapturedCallState([unsafe { *libc::__errno_location() }]);
        }
        #[cfg(windows)]
        {
            extern "system" {
                fn GetLastError() -> u32;
            }
            #[link(name = "ws2_32")]
            extern "system" {
                fn WSAGetLastError() -> i32;
            }
            extern "C" {
                fn _errno() -> *mut i32;
            }
            // SAFETY: plain reads of this thread's error slots; `_errno`
            // answers the CRT's per-thread errno. `GetLastError` first: the
            // other two reads do not set it.
            let (last_error, wsa, errno) =
                unsafe { (GetLastError() as i32, WSAGetLastError(), *_errno()) };
            return CapturedCallState([last_error, wsa, errno]);
        }
        CapturedCallState([0; CAPTURED_STATE_WORDS])
    }
}

/// Fill a `Linker.Option.captureCallState` segment after a downcall.
///
/// gcd d5/f: `captured` is the C call's state, read the moment `ffi_call`
/// returned ([`CapturedCallState::now`] in [`pe_downcall_invoke`]). This used
/// to read the thread's `errno` HERE, after everything the downcall does once
/// the call is over -- pin re-reads, session releases (atomics, and a lock on
/// contention), the heap-segment copy-back, `free` of the scratch buffers, and
/// with `CRATONVM_FFM_DOWNCALL_GC_SAFE` the barrier's leave -- any of which may
/// set it; HotSpot's stub saves the state straight after the call. And on
/// Windows nothing was written at all, so `captureCallState("GetLastError")`
/// always read 0; now the three words are written, each only where the
/// segment's declared size holds it (HotSpot writes the selected states only;
/// the handle does not keep the selection, and the layout's other words are
/// the caller's to ignore).
pub(in crate::panama) fn write_downcall_capture_state(
    ctx: &dyn NativeContext,
    state: ObjectRef,
    captured: CapturedCallState,
) {
    let addr = crate::panama_libffi::segment_address(ctx, state);
    // Only Windows bounds the words by the segment's size; the Linux layout
    // is the one `errno` word, written whatever the size (as before).
    #[cfg(windows)]
    let size = crate::panama_libffi::segment_byte_size(ctx, state);
    #[cfg(not(windows))]
    let size = i64::MAX;
    write_capture_state_at(addr, size, captured);
}

/// `Linker.Option.captureStateLayout().byteSize()` on this target: the
/// words [`CapturedCallState`] holds, four bytes each (Linux and macOS
/// `errno`; Windows `GetLastError`, `WSAGetLastError`, `errno`).
pub(in crate::panama) const CAPTURE_STATE_BYTES: i64 = (CAPTURED_STATE_WORDS * std::mem::size_of::<i32>()) as i64;

/// `CRATONVM_FFM_CAPTURE_SEGMENT_SIZE_CHECK` (default ON, `=0` off; JIT round
/// 17 wave 2, lane ffm2): a `captureCallState` downcall whose state segment
/// is smaller than the capture layout raises HotSpot's
/// `IndexOutOfBoundsException` before the call (the general path), and the
/// memo, segment and direct rows decline it to the general path. Without it
/// the state was written past the segment's end (Linux writes `errno`
/// whatever the size). `=0` restores the unchecked write.
pub(in crate::panama) fn ffm_capture_segment_size_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Is a state segment of `size` bytes too small for the capture layout (and
/// the check on)?
pub(in crate::panama) fn capture_segment_too_small(size: i64) -> bool {
    size < CAPTURE_STATE_BYTES && ffm_capture_segment_size_check()
}

/// HotSpot's refusal of a too-small state segment:
/// `AbstractMemorySegmentImpl.outOfBoundException(0, layout size)` from
/// `asSlice(0, captureStateLayout())` (j27 wave 4, lane FFMMSG: the segment
/// text is its `toString()`; a heap one lacked its `heapBase`).
pub(in crate::panama) fn capture_segment_out_of_bounds(ctx: &dyn NativeContext, state: ObjectRef) -> MethodCallFailed {
    segment_out_of_bounds(ctx, state, 0, CAPTURE_STATE_BYTES)
}

/// `captureStateLayout().byteAlignment()`: a struct of `JAVA_INT` words.
pub(in crate::panama) const CAPTURE_STATE_ALIGN: i64 = std::mem::align_of::<i32>() as i64;



/// Must a fast row (memo, segment row, direct segment row) leave a native
/// state segment at `addr` of `size` bytes to the general path, which raises
/// HotSpot's refusal? Too small, `MemorySegment.NULL`, or misaligned.
pub(in crate::panama) fn capture_segment_declined(addr: usize, size: i64) -> bool {
    capture_segment_too_small(size) || (addr == 0 || addr % CAPTURE_STATE_ALIGN as usize != 0)
}

/// HotSpot's refusal of a state segment equal to `MemorySegment.NULL`
/// (`SharedUtils.checkCaptureSegment`; the segment text is `toString()`,
/// measured in the `Symbol is NULL: ` twin of `checkSymbol`).
pub(in crate::panama) fn capture_segment_is_null(ctx: &dyn NativeContext, state: ObjectRef) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: format!(
            "Capture segment is NULL: MemorySegment{{ kind: native, address: 0x0, byteSize: {} }}",
            crate::panama_libffi::segment_byte_size(ctx, state)
        ),
    }
    .into()
}

/// HotSpot's refusal of a heap state segment that passed the slice checks
/// (JIT round 17 wave 5, lane ffm4): `SharedUtils.unboxSegment`'s
/// `IllegalArgumentException: Heap segment not allowed: <segment>`, the
/// segment text being `toString()` ([`heap_segment_text`]). j27 wave 4
/// (lane FFMFLIP): also `marshal_arg`'s refusal of a heap segment passed as a
/// plain downcall's pointer argument, the same `unboxSegment` call on HotSpot;
/// (lane FFMMSG) and every other `checkNative` refusal: `set(ADDRESS, ..)`,
/// the layout `VarHandle`'s ADDRESS value, a heap group-return buffer and an
/// upcall's ADDRESS return.
pub(crate) fn heap_segment_not_allowed(
    ctx: &dyn NativeContext,
    state: ObjectRef,
) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: format!("Heap segment not allowed: {}", heap_segment_text(ctx, state)),
    }
    .into()
}

/// `SharedUtils.checkNative(segment)`: [`heap_segment_not_allowed`] for a
/// heap segment ([`is_heap_shaped_segment`]), `None` for a native one.
pub(crate) fn heap_segment_refusal(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
) -> Option<MethodCallFailed> {
    is_heap_shaped_segment(ctx, seg).then(|| heap_segment_not_allowed(ctx, seg))
}

/// `AbstractMemorySegmentImpl.outOfBoundException(offset, length)`'s message,
/// `segment` being the segment's `toString()` (j27 wave 4, lane FFMMSG).
pub(crate) fn out_of_bounds_message(segment: &str, offset: i64, length: i64) -> String {
    format!("Out of bound access on segment {segment}; new offset = {offset}; new length = {length}")
}

/// [`out_of_bounds_message`] for `seg`, as the `IndexOutOfBoundsException`.
pub(crate) fn segment_out_of_bounds(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    length: i64,
) -> MethodCallFailed {
    RuntimeError::IndexOutOfBoundsException {
        message: Some(out_of_bounds_message(&segment_to_string(ctx, seg), offset, length)),
    }
    .into()
}

/// `checkEnclosingLayout`'s `Target offset %d is incompatible with alignment
/// constraint %d (of %s) for segment %s` (j27 wave 4, lane FFMMSG).
/// `segment` is the segment's `toString()`, built by the caller before this
/// runs Java; the `(of ..)` clause is `layout`'s own `toString()`, left out
/// when there is no layout or it is not a real JDK one ([`real_layout_text`]).
pub(crate) fn misaligned_message(
    ctx: &mut dyn NativeContext,
    segment: &str,
    layout: Option<ObjectRef>,
    offset: i64,
    align: i64,
) -> String {
    let of_layout = layout
        .and_then(|layout| real_layout_text(ctx, layout))
        .map(|text| format!(" (of {text})"))
        .unwrap_or_default();
    format!(
        "Target offset {offset} is incompatible with alignment constraint \
         {align}{of_layout} for segment {segment}"
    )
}

/// `AbstractMemorySegmentImpl.toString()` of any segment (j27 wave 4, lane
/// FFMMSG): the carrier's registered `toString()` and every refusal that
/// embeds the segment (`outOfBoundException`, `checkEnclosingLayout`,
/// `checkNative`). A heap segment is [`heap_segment_text`]; any other is
/// [`native_segment_text`].
pub(crate) fn segment_to_string(ctx: &dyn NativeContext, seg: ObjectRef) -> String {
    if is_heap_shaped_segment(ctx, seg) {
        heap_segment_text(ctx, seg)
    } else {
        native_segment_text(ctx, seg)
    }
}

/// `MemorySegment{ kind: native|mapped, address: 0x<address()>, byteSize: <n>
/// }` (a native segment has no `heapBase`). `mapped` exactly when
/// `isMapped()` says so (`p67_segment_impl_is_mapped`: the real
/// `MappedMemorySegmentImpl` only); a CratonVM carrier always answers
/// `isMapped() == false`, so it prints `native`, whatever memory backs it.
pub(in crate::panama) fn native_segment_text(ctx: &dyn NativeContext, seg: ObjectRef) -> String {
    let mapped = ctx.class_name_of_id(ctx.class_id_of_object(seg)).as_deref()
        == Some("jdk/internal/foreign/MappedMemorySegmentImpl");
    format!(
        "MemorySegment{{ kind: {}, address: 0x{:x}, byteSize: {} }}",
        if mapped { "mapped" } else { "native" },
        pe_segment_base_address(ctx, seg),
        crate::panama_libffi::segment_byte_size(ctx, seg)
    )
}

/// A heap segment's `AbstractMemorySegmentImpl.toString()`: `MemorySegment{
/// kind: heap, heapBase: [I@<identity hash>, address: 0x<offset>, byteSize:
/// <n> }`. The `heapBase` clause is `heapBase()`, which a read-only segment
/// withholds (j27 wave 4, lane FFMMSG: it was printed for one).
pub(in crate::panama) fn heap_segment_text(ctx: &dyn NativeContext, seg: ObjectRef) -> String {
    match heap_segment_view(ctx, seg) {
        Some(view) => heap_view_text(ctx, &view),
        None => format!(
            "MemorySegment{{ kind: heap, address: 0x0, byteSize: {} }}",
            crate::panama_libffi::segment_byte_size(ctx, seg)
        ),
    }
}

/// [`heap_segment_text`] of a decoded heap segment.
pub(in crate::panama) fn heap_view_text(ctx: &dyn NativeContext, view: &HeapSegmentView) -> String {
    let heap_base = if view.read_only {
        String::new()
    } else {
        format!(
            ", heapBase: {}@{:x}",
            heap_array_class_name(view.elem_type),
            ctx.identity_hash_code(view.base) as u32
        )
    };
    format!(
        "MemorySegment{{ kind: heap{heap_base}, address: 0x{:x}, byteSize: {} }}",
        view.start, view.size
    )
}

/// A real JDK layout's own `toString()` (`i4`, `8%J8`, ..), for the `(of
/// %s)` clause of `checkEnclosingLayout`; `None` for any other layout object,
/// whose text this VM cannot vouch for. Runs Java: build every other part of
/// the message first.
pub(in crate::panama) fn real_layout_text(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Option<String> {
    let real = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .is_some_and(|name| name.starts_with("jdk/internal/foreign/layout/"));
    if !real {
        return None;
    }
    crate::lang_invoke::member_name::jdk_object_to_string(ctx, layout)
}

/// `getClass().getName()` of a heap segment's backing array.
pub(in crate::panama) fn heap_array_class_name(elem_type: cratonvm_types::ArrayElementType) -> &'static str {
    use cratonvm_types::ArrayElementType as A;
    match elem_type {
        A::Boolean => "[Z",
        A::Char => "[C",
        A::Float => "[F",
        A::Double => "[D",
        A::Byte => "[B",
        A::Short => "[S",
        A::Int => "[I",
        A::Long => "[J",
        A::Reference => "[Ljava.lang.Object;",
    }
}

/// [`write_downcall_capture_state`] at a segment address and byte size the
/// caller already read (JIT round 17 wave 1, lane ffmseg: the segment row
/// read both with the segment's other slots).
pub(in crate::panama) fn write_capture_state_at(addr: i64, size: i64, captured: CapturedCallState) {
    #[cfg(target_os = "linux")]
    {
        let _ = size;
        // SBR5 ffmlink: the state segment of an `Arena` has an arena HANDLE for an address.
        let handle = addr;
        let addr = crate::panama_libffi::host_base_address(handle, 4);
        if addr != 0 {
            // The standard Linux capture-state layout starts with errno.
            // SAFETY: `addr` is the live base of the caller's state segment.
            unsafe { *(addr as *mut i32) = captured.0[0] };
        }
        if crate::panama_libffi::is_arena_handle(handle) {
            crate::unsafe_natives_ext::unsafe_arena_release_translation(handle);
        }
    }
    #[cfg(windows)]
    {
        let size = size.max(0) as usize;
        let handle = addr;
        let addr = crate::panama_libffi::host_base_address(handle, size.min(12) as i64);
        if addr != 0 {
            for (i, word) in captured.0.iter().enumerate() {
                if (i + 1) * std::mem::size_of::<i32>() <= size {
                    // SAFETY: inside the segment's declared size (checked
                    // above); `addr` is its live base.
                    unsafe { *((addr as *mut i32).add(i)) = *word };
                }
            }
        }
        // As the Linux arm: give the arena block's pointer back once written
        // (it was left translated, so the arena's close counted a stale free).
        if addr != 0 && crate::panama_libffi::is_arena_handle(handle) {
            crate::unsafe_natives_ext::unsafe_arena_release_translation(handle);
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (addr, size, captured);
    }
}

/// The memo path's capture write: at the address and size the segment row
/// read (`known`), else through [`write_downcall_capture_state`]'s readers.
pub(in crate::panama) fn write_memo_capture_state(
    ctx: &dyn NativeContext,
    state: ObjectRef,
    known: Option<(usize, i64)>,
    captured: CapturedCallState,
) {
    match known {
        Some((addr, size)) => write_capture_state_at(addr as i64, size, captured),
        None => write_downcall_capture_state(ctx, state, captured),
    }
}



/// Execute a downcall via libffi (NEW-18).
///
/// The handle carries the function pointer, the FunctionDescriptor, an
/// optional fixed-arg count for variadic calls (-1 means non-variadic) and a
/// T5.6.3 cached `Box<Cif>` raw pointer (0 means unpopulated) at the
/// `DOWNCALL_*` slots — see [`alloc_downcall_handle`]. The descriptor carries
/// the parameter and return layouts.
///
/// libffi handles ABI classification (integer vs float register
/// allocation, struct-by-value, alignment, padding) for every
/// supported platform — replacing the previous 8-arg integer-only
/// dispatcher. See `panama_libffi` for the layout↔ffi_type bridge.
///
/// TODO(T5.6.3 finalization): the `Box<Cif>` stashed on `DOWNCALL_CIF` is
/// currently leaked when the DowncallHandle is garbage-collected —
/// the Panama synthetic objects don't yet route through a finalizer
/// callback. Since `Linker::downcallHandle` is called once per native
/// function per VM run the leak is bounded (typically a handful of
/// Cifs) and matches HotSpot's own permanent FFI metadata. When a
/// finalizer path lands, call `plf::free_cached_cif(field3)` from it.
pub(crate) fn pe_downcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_downcall_invoke_with(ctx, args, ffm_downcall_plain_memo())
}

/// [`pe_downcall_invoke`] with the plain-memo switch passed in (JIT round 16
/// wave 11, lane ffmcost), so a unit test can run both arms in one process.
///
/// JIT round 18 wave 11 (lane ffmret;
/// `CRATONVM_FFM_GROUP_RETURN_ALLOCATE_BEFORE_CALL`): a group returned IN
/// MEMORY calls its `SegmentAllocator` first, before any other argument is
/// looked at and before the C call, as HotSpot's `adaptDowncallForIMR` does
/// (the allocation is the outermost argument filter of the handle): a throwing
/// or `null`-answering allocator raises with the C function never run, and a
/// heap segment is `Heap segment not allowed` before the call
/// ([`downcall_imr_preallocate`]). The segment is pinned for the whole call
/// and is the result ([`pe_downcall_invoke_general`]). A register-returned
/// group keeps the allocator after the call: HotSpot's `Allocate` binding is
/// a return binding there.
pub(in crate::panama) fn pe_downcall_invoke_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    plain_memo: bool,
) -> MethodCallResult {
    // Round 19: the arena blocks this call passes C, until its result is built.
    let _arena_call = crate::panama_libffi::ArenaCallScope::open();
    if plain_memo {
        if let Some(result) =
            pe_downcall_invoke_memo(ctx, args, jit_downcall_bind_segments_enabled())
        {
            return result;
        }
    }
    let Some((fresh_args, buffer)) = downcall_imr_preallocate(ctx, args)? else {
        return pe_downcall_invoke_general(ctx, args, plain_memo, None);
    };
    let pin = ctx.pin_native_root(buffer);
    let result = pe_downcall_invoke_general(ctx, &fresh_args, plain_memo, Some((buffer, pin)));
    ctx.unpin_native_roots(pin);
    result
}



/// Does the target's C ABI return the group `rl` of `size` bytes in
/// registers? [`group_returned_in_registers`] read off the Java layout,
/// before any CIF exists: the size rules, and on AArch64 the homogeneous
/// float aggregate test on the layout's (unprepared) libffi type, whose
/// element kinds are what the prepared one would hold.
pub(in crate::panama) fn group_layout_returned_in_registers(ctx: &dyn NativeContext, rl: ObjectRef, size: usize) -> bool {
    if cfg!(target_arch = "aarch64") && size > 16 {
        return match crate::panama_libffi::layout_to_ffi_type(ctx, rl, 0) {
            // SAFETY: a live type; only its kinds and elements are read.
            Ok(ty) => unsafe { group_returned_in_registers(size, ty.as_raw_ptr()) },
            Err(_) => false,
        };
    }
    // SAFETY: off AArch64 the type is never read.
    unsafe { group_returned_in_registers(size, std::ptr::null()) }
}

/// The allocator-before-call half of [`pe_downcall_invoke_with`]. `Ok(None)`
/// -- nothing done -- unless the switch is on, the handle returns a group in
/// MEMORY through a leading allocator, and that allocator is a non-`null`
/// argument (anything else keeps the general path's handling and
/// exceptions). Otherwise every reference argument is pinned, the allocator
/// is called with the group's `(byteSize, byteAlignment)` (what
/// `SegmentAllocator.allocate(MemoryLayout)` passes), the arguments are
/// re-read, and the answer is screened as `unboxSegment` screens it (`null`:
/// `NullPointerException`; a heap segment: `IllegalArgumentException: Heap
/// segment not allowed`). `Ok(Some((arguments, segment)))`: the current
/// arguments and the buffer; nothing pinned.
pub(in crate::panama) fn downcall_imr_preallocate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(Vec<Value>, ObjectRef)>, MethodCallFailed> {
    let Some(Value::Object(Some(handle))) = args.first().copied() else {
        return Ok(None);
    };
    if !is_downcall_handle(&*ctx, handle) {
        return Ok(None);
    }
    let Some(descriptor) = downcall_descriptor(ctx, handle) else {
        return Ok(None);
    };
    let return_layout = crate::panama_libffi::descriptor_return_layout(ctx, descriptor);
    if !downcall_returns_group(&*ctx, return_layout) {
        return Ok(None);
    }
    let Some(rl) = return_layout else {
        return Ok(None);
    };
    let size = crate::phases_late::foreign_ffm::p67_layout_size_of(&*ctx, rl).max(0) as usize;
    let align = crate::phases_late::foreign_ffm::p67_layout_align_of(&*ctx, rl).max(1) as usize;
    if group_layout_returned_in_registers(&*ctx, rl, size) {
        return Ok(None);
    }
    // The allocator: the first call argument (after the target segment of an
    // address-less handle), in the argument list or the legacy bridge's
    // `Object[]`, exactly where the general path takes it.
    let address_from_arg0 = downcall_fn_addr(ctx, handle) == DOWNCALL_ADDRESS_FROM_ARG0;
    let index = usize::from(address_from_arg0);
    let allocator = match args.get(1) {
        Some(Value::Object(Some(arr))) if args.len() == 2 && ctx.object_is_array(*arr) => {
            if ctx.array_length(*arr) > index {
                ctx.get_array_element(*arr, index)
            } else {
                Value::Object(None)
            }
        }
        _ => args.get(1 + index).copied().unwrap_or(Value::Object(None)),
    };
    let Value::Object(Some(allocator)) = allocator else {
        return Ok(None);
    };
    // Every reference argument pinned across the Java call, then re-read.
    let mut pins: Vec<(usize, usize)> = Vec::new();
    for (i, value) in args.iter().enumerate() {
        if let Value::Object(Some(obj)) = value {
            pins.push((i, ctx.pin_native_root(*obj)));
        }
    }
    let allocated = ctx.invoke_virtual(
        allocator,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        &[Value::Long(size as i64), Value::Long(align as i64)],
    );
    let mut fresh: Vec<Value> = args.to_vec();
    for &(i, pin) in &pins {
        if let Value::Object(Some(obj)) = args[i] {
            fresh[i] = Value::Object(Some(ctx.read_native_pin(pin, obj)));
        }
    }
    if let Some(&(_, base)) = pins.first() {
        ctx.unpin_native_roots(base);
    }
    let Some(Value::Object(Some(buffer))) = allocated? else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    if let Some(refusal) = heap_segment_refusal(&*ctx, buffer) {
        return Err(refusal);
    }
    Ok(Some((fresh, buffer)))
}

/// zt t18/hunt1: one scratch buffer of [`pe_downcall_invoke_general`]'s
/// heap-segment pass: the arguments it serves (indices into the pass's
/// candidate list, in argument order), the byte start of its lowest one
/// within the array's data, and the bytes it spans.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::panama) struct PermissiveHeapGroup {
    pub(in crate::panama) members: Vec<usize>,
    pub(in crate::panama) lo: i64,
    pub(in crate::panama) size: usize,
}

/// zt t18/hunt1: group the pass's heap-segment arguments, given as
/// `(backing array address, byte start, byte size)`, so the ones on the same
/// array share one buffer spanning the union of their ranges (they alias on
/// HotSpot, which hands C the array itself). Groups come out in the order of
/// their first member. A segment whose start is negative or whose end
/// overflows is never shared: it keeps the lone buffer it always had (whose
/// read the pass's `heap_read_bytes` then refuses, as before).
pub(in crate::panama) fn group_permissive_heap_args(segs: &[(usize, i64, usize)]) -> Vec<PermissiveHeapGroup> {
    let mut out: Vec<PermissiveHeapGroup> = Vec::new();
    // Per group: the array it may share (`None`: never shared) and its end.
    let mut keys: Vec<(Option<usize>, i64)> = Vec::new();
    for (i, &(base, start, size)) in segs.iter().enumerate() {
        let end = i64::try_from(size)
            .ok()
            .and_then(|s| start.checked_add(s))
            .filter(|_| start >= 0);
        if let Some(end) = end {
            if let Some(g) = keys.iter().position(|&(k, _)| k == Some(base)) {
                let hi = keys[g].1.max(end);
                keys[g].1 = hi;
                let group = &mut out[g];
                group.lo = group.lo.min(start);
                group.size = (hi - group.lo) as usize;
                group.members.push(i);
                continue;
            }
        }
        keys.push((end.map(|_| base), end.unwrap_or(0)));
        out.push(PermissiveHeapGroup {
            members: vec![i],
            lo: start,
            size,
        });
    }
    out
}

/// zt t20/x16 (x10 review page item 23j): the copy-back ranges of one
/// scratch buffer of [`pe_downcall_invoke_general`]'s heap-segment pass, from
/// its members' `(offset into the buffer, byte size)`: each clamped to the
/// buffer's `size`, empty ones dropped, overlapping or touching ones merged,
/// in ascending order, as `[from, to)`. A lone member yields `[(0, size)]`,
/// the whole buffer, as before; disjoint members leave their gap out.
pub(in crate::panama) fn permissive_copy_back_ranges(
    members: impl Iterator<Item = (usize, usize)>,
    size: usize,
) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = members
        .map(|(from, len)| (from.min(size), from.saturating_add(len).min(size)))
        .filter(|&(from, to)| from < to)
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (from, to) in spans {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
}

/// The general downcall path ([`pe_downcall_invoke_with`] after the memo path
/// and the in-memory allocation). `prealloc`: the in-memory group's buffer
/// the allocator already answered, with its pin.
pub(in crate::panama) fn pe_downcall_invoke_general(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    plain_memo: bool,
    prealloc: Option<(ObjectRef, usize)>,
) -> MethodCallResult {
    use crate::panama_libffi as plf;
    use libffi::middle::{arg as ffi_arg, CodePtr};

    // JIT round 18 wave 11 (lane ffmret): the in-memory buffer allocated
    // before the call, current (nothing since the re-read was a GC point).
    let prealloc_buffer: Option<ObjectRef> =
        prealloc.map(|(buffer, pin)| ctx.read_native_pin(pin, buffer));

    // JIT round 16 wave 11 (lane ffmcost): with the plain-memo switch, the
    // trivial verdict (native access on, no SecurityManager, no capability
    // policy in the process) is taken without `require_native_access`'s
    // formatted permission name and two policy lookups; anything else takes
    // the full checks, which raise what they always raised.
    if !(plain_memo && downcall_access_is_trivial(ctx)) {
        require_segment_access(ctx, "downcall")?;
    }
    let handle = obj_arg(args, 0)?;
    let mut fn_addr = downcall_fn_addr(ctx, handle);
    // An address-less handle (`downcallHandle(FunctionDescriptor, Option...)`):
    // the target is its first argument. Taken off the argument list below.
    let address_from_arg0 = fn_addr == DOWNCALL_ADDRESS_FROM_ARG0;
    if address_from_arg0 {
        let first = match args.get(1) {
            Some(Value::Object(Some(arr))) if args.len() == 2 && ctx.object_is_array(*arr) => {
                if ctx.array_length(*arr) > 0 {
                    ctx.get_array_element(*arr, 0)
                } else {
                    Value::Object(None)
                }
            }
            Some(v) => *v,
            None => Value::Object(None),
        };
        let Value::Object(Some(seg)) = first else {
            return Err(RuntimeError::NullPointerException {
                message: Some("downcall target segment is null".into()),
            }
            .into());
        };
        fn_addr = crate::panama_libffi::segment_address(ctx, seg);
    }
    // Work-list item 14. The symbol name is not carried on the handle, so the
    // scope is the target address — which is what a denial needs to report and
    // what an operator would have to grant. Permissive by default.
    // JIT round 16 wave 11 (lane ffmcost): with the plain-memo switch and no
    // capability policy installed anywhere, the gate is `Ok` without its
    // per-call `String`.
    if !(plain_memo && !cratonvm_native_api::capability::any_capabilities_installed()) {
        crate::capability_gate::gate_foreign_downcall(&*ctx, &format!("0x{fn_addr:x}"))?;
    }
    let descriptor = downcall_descriptor(ctx, handle).ok_or_else(|| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "No FunctionDescriptor".into(),
        }
        .into()
    })?;

    if fn_addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer".into(),
        }
        .into());
    }
    // gcd d5/f: the GC-safe decision, taken while `handle` is fresh (nothing
    // above allocates). Off by default; a critical handle never goes GC-safe.
    // JIT round 16 wave 14 (lane ffm3): or the lazy in-native state.
    let gc_safe = (ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy())
        && !downcall_is_critical(ctx, handle);
    // Run the validated_fn_ptr alignment check.
    let _: unsafe extern "C" fn() = validated_fn_ptr(fn_addr)?;

    // Variadic flag — the Linker.Option.firstVariadicArg(int) overload
    // populates this when registering the downcall handle. -1 (or
    // missing field) = non-variadic.
    let variadic_fixed: Option<usize> = match ctx.get_field(handle, DOWNCALL_FIRST_VARIADIC) {
        Value::Long(n) if n >= 0 => Some(n as usize),
        Value::Int(n) if n >= 0 => Some(n as usize),
        _ => None,
    };

    // T5.6.3 — previously cached `Box<Cif>` pointer (0 = miss). See
    // `panama_libffi::box_cif_to_u64` for the encoding.
    let cached_cif_u64: u64 = match ctx.get_field(handle, DOWNCALL_CIF) {
        Value::Long(n) => n as u64,
        _ => 0,
    };

    // ----- Read descriptor layouts -----
    let param_layout_objs = plf::descriptor_param_layouts(ctx, descriptor);
    let return_layout = plf::descriptor_return_layout(ctx, descriptor);

    // ----- Read incoming Java arguments -----
    //
    // The legacy synthetic bridge calls invoke(Object[]) and supplies one
    // boxed array after the handle. Signature-polymorphic real-JDK calls
    // arrive as [handle, arg0, arg1, ...], so preserve those concrete
    // values instead of interpreting the first reference argument as an
    // array.
    let mut call_args: Vec<Value> = match args.get(1) {
        Some(Value::Object(Some(arr))) if args.len() == 2 && ctx.object_is_array(*arr) => {
            let len = ctx.array_length(*arr);
            (0..len).map(|i| ctx.get_array_element(*arr, i)).collect()
        }
        _ => args[1..].to_vec(),
    };
    if address_from_arg0 && !call_args.is_empty() {
        call_args.remove(0);
    }
    // Round 13 wave 5 (lane ffm3): a by-value group return's leading
    // `SegmentAllocator` (see `downcall_carrier_descriptor`). HotSpot calls it
    // BEFORE the native call; this model calls it after (below), so a
    // refusing allocator throws once the C function has run. A null one is
    // the NPE HotSpot's `allocator.allocate(layout)` raises, before the call.
    let return_allocator: Option<ObjectRef> =
        if downcall_returns_group(ctx, return_layout) {
            if call_args.is_empty() {
                return Err(RuntimeError::IllegalStateException {
                    message: "Panama downcall returning a group expected a leading \
                              SegmentAllocator argument, got none"
                        .into(),
                }
                .into());
            }
            match call_args.remove(0) {
                Value::Object(Some(allocator)) => Some(allocator),
                _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
            }
        } else {
            None
        };

    let capture_state = if downcall_handle_captures_call_state(ctx, handle) {
        if call_args.len() != param_layout_objs.len() + 1 {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Panama capture-state downcall expected {} arguments, got {}",
                    param_layout_objs.len() + 1,
                    call_args.len()
                ),
            }
            .into());
        }
        match call_args.remove(0) {
            Value::Object(Some(state)) => Some(state),
            // JIT round 17 wave 2 (lane ffm3): `checkCaptureSegment`'s
            // `Objects.requireNonNull`, a message-less NPE.
            Value::Object(None) if true => {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "Panama capture-state argument is not a MemorySegment".into(),
                }
                .into());
            }
        }
    } else {
        None
    };

    if call_args.len() != param_layout_objs.len() {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama downcall arity mismatch: descriptor has {} params, got {} args",
                param_layout_objs.len(),
                call_args.len()
            ),
        }
        .into());
    }

    // JIT round 17 wave 2 (lane ffm2; `CRATONVM_FFM_CAPTURE_SEGMENT_SIZE_CHECK`):
    // HotSpot slices the state segment to the capture layout before the call
    // (`SharedUtils.checkCaptureSegment`: `asSlice(0, captureStateLayout())`),
    // so one smaller than the layout is `IndexOutOfBoundsException`; the state
    // write after the call used to land past its end.
    if let Some(state) = capture_state {
        // JIT round 17 wave 2 (lane ffm3; `CRATONVM_FFM_CAPTURE_SEGMENT_NULL_ALIGN_CHECK`):
        // an address-0 native segment equals `MemorySegment.NULL`, which
        // `checkCaptureSegment` refuses before it slices; then the slice's
        // alignment half (the capture layout is 4-byte aligned). With `=0`
        // both pass, and the state write skips address 0, as before.
        let null_align = true;
        let null_segment = !is_heap_shaped_segment(ctx, state)
            && crate::panama_libffi::segment_address(ctx, state) == 0;
        if null_align && null_segment {
            return Err(capture_segment_is_null(ctx, state));
        }
        if ffm_capture_segment_size_check() {
            let size = crate::panama_libffi::segment_byte_size(ctx, state);
            if capture_segment_too_small(size) && !null_segment {
                return Err(capture_segment_out_of_bounds(ctx, state));
            }
        }
        if null_align {
            pe_slice_alignment_check(ctx, state, 0, CAPTURE_STATE_ALIGN)?;
            // JIT round 17 wave 5 (lane ffm4): the slice passed, so the state
            // segment reaches the binding's `unboxAddress`, which refuses a
            // heap segment (`Heap segment not allowed: <segment>`) before the
            // call. A `byte[]` one never gets here (its 1-byte alignment fails
            // the slice above, as on HotSpot); an `int[]`/`long[]`/... one
            // did, and the call ran with the state write skipped. A
            // `critical(true)` handle keeps its heap admission.
            let heap_admitted =
                downcall_allows_heap_access(ctx, handle);
            if !heap_admitted && is_heap_shaped_segment(ctx, state) {
                return Err(heap_segment_not_allowed(ctx, state));
            }
        }
    }

    // Round 12 wave 5 (lane ffm2): every segment handed to native code must be
    // alive, and confined to this thread if confined -- HotSpot acquires each
    // by-reference argument's session for the call, so a closed arena's
    // segment is `IllegalStateException: Already closed` and another thread's
    // confined one is `WrongThreadException`. This passed their addresses
    // straight to C, which is a use-after-free once an arena frees its blocks
    // on close. `pe_segment_check_scope` only reads fields, class-gated, so a
    // non-segment argument (a legacy bridge's boxed value) passes untouched.
    // The sessions are also ACQUIRED for the call itself, just before it (round
    // 12 wave 6, lane ffm3; see `pe_downcall_acquire_sessions` below); this
    // early check keeps the exception ahead of any argument marshalling.
    let acquire_segments: Vec<ObjectRef> = call_args
        .iter()
        .filter_map(|value| match *value {
            Value::Object(Some(segment)) => Some(segment),
            _ => None,
        })
        .chain(capture_state)
        .chain(prealloc_buffer)
        .collect();
    if ffm_bulk_scope_check() {
        for value in call_args.iter() {
            if let Value::Object(Some(segment)) = *value {
                pe_segment_check_scope(ctx, segment)?;
            }
        }
        if let Some(state) = capture_state {
            pe_segment_check_scope(ctx, state)?;
        }
        // JIT round 18 wave 11 (lane ffmret): the in-memory return buffer is
        // a by-reference argument of the C call (HotSpot acquires its session
        // like any other).
        if let Some(buffer) = prealloc_buffer {
            pe_segment_check_scope(ctx, buffer)?;
        }
    }

    // Elasticsearch's bulk-vector downcalls depend on handing
    // `MemorySegment.ofArray(int[]|long[]|float[]|double[])` straight to a
    // downcall. The oracle refuses that outright for EVERY heap segment,
    // numeric or not (measured, `FfmRepro2.java`:
    // `strlen.invoke(MemorySegment.ofArray(new int[]{1,2,3}))` throws
    // `IllegalArgumentException: Heap segment not allowed`). Since j27 wave 4
    // (lane FFMFLIP) this VM refuses them too: the pass admits a heap segment
    // only for a `critical(true)` handle's `ADDRESS` parameter, or, under the
    // opt-in `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT=1`, for the
    // four numeric element types (the old carve-out). `marshal_arg`'s
    // `is_alias_heap_segment` refuses every heap segment this pass leaves;
    // the pass runs FIRST and substitutes a scratch native buffer for an
    // admitted argument before `marshal_arg` ever sees it.
    //
    // The array itself cannot be hived off to native code (moving GC), so
    // the buffer stands in for exactly this call: copied in now, copied
    // back and freed right after the call returns. This replaces the
    // permanent native mirror `ofArray` used to allocate and never free —
    // see the retired `alloc_mirror_carrier` — with one that is scoped to a
    // single call and does not leak.
    //
    // zt t18/hunt1: ONE buffer per backing ARRAY, not one per argument. Two
    // heap segments of the same array in one call -- `memmove(seg.asSlice(1),
    // seg, n)` through a `critical(true)` handle -- alias on HotSpot, which
    // hands C the array's own bytes. A buffer per argument gave each its own
    // snapshot, and the copy-back of the source's UNCHANGED snapshot (it runs
    // after the destination's) wrote the original bytes back over what the
    // native had just moved: the array came out of the call unchanged. The
    // buffer now spans the union of the group's ranges and every argument
    // points at its own offset inside it, so C sees one memory, as on
    // HotSpot. A lone segment gets exactly the buffer it got before.
    struct PermissiveHeapArg {
        alloc_id: i64,
        ptr: usize,
        size: usize,
        segment: ObjectRef,
        /// zt t18/hunt1: how far `segment`'s first byte lies above the
        /// buffer's first byte (the group's lowest start); 0 for a lone
        /// segment.
        rel: usize,
        /// zt t20/x16 (x10 review page item 23j): the byte ranges of the
        /// buffer the copy-back writes, `[from, to)` from its first byte:
        /// the writable members' own ranges, merged. Not the whole union: a
        /// gap between two disjoint slices of one array was snapshotted
        /// before the call and written back after it, undoing a Java write
        /// another thread made to the gap meanwhile (on HotSpot that write
        /// lands in the array and stays). A read-only member is not written
        /// back, as a lone read-only segment never was.
        ranges: Vec<(usize, usize)>,
    }
    let mut permissive_heap_args: Vec<PermissiveHeapArg> = Vec::new();
    // zt t18/hunt1: the admitted arguments, collected before any buffer is
    // allocated so the ones sharing an array can share one.
    let mut permissive_candidates: Vec<(usize, ObjectRef, HeapSegmentView)> = Vec::new();
    // JIT round 16 wave 13 (lane ffmseg): `Linker.Option.critical(true)`
    // admits any heap segment for an `ADDRESS` parameter, as HotSpot does
    // (`CRATONVM_FFM_CRITICAL_HEAP_ACCESS`); it takes the same scratch buffer.
    let allow_heap = downcall_allows_heap_access(ctx, handle);
    for (index, value) in call_args.iter().enumerate() {
        let Value::Object(Some(segment)) = *value else {
            continue;
        };
        let heap_admitted = allow_heap
            && matches!(
                param_layout_objs.get(index).copied().flatten(),
                Some(layout) if plf::read_layout_kind(ctx, layout) == LAYOUT_ADDRESS
            );
        // A real JDK heap segment gets no carve-out; leave it for
        // `marshal_arg` to refuse.
        if !heap_admitted && crate::panama_libffi::is_real_heap_segment(ctx, segment) {
            continue;
        }
        let Some(view) = heap_segment_view(ctx, segment) else {
            continue;
        };
        use cratonvm_types::ArrayElementType as A;
        if !heap_admitted && !matches!(view.elem_type, A::Int | A::Long | A::Float | A::Double) {
            continue; // byte[]/short[]/char[]: stays refused, matching the oracle
        }
        // WARNING j27-user-flip-20261028 (HotSpot's refusal default ON;
        // CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT=1 restores the old default)
        // j27 wave 4 (lane FFMFLIP): without `critical(true)` every heap
        // segment is left for `marshal_arg` to refuse, as HotSpot does.
        // j27 wave 8 (lane W8FFM): asked last, so only a numeric heap
        // segment of a plain downcall reads the switch (the refusal, a cold
        // path) and no native argument pays for it: no cached static.
        if !heap_admitted && !ffm_ofarray_heap_downcall_carveout() {
            continue;
        }
        permissive_candidates.push((index, segment, view));
    }
    // zt t18/hunt1: group by backing array, in argument order (a group is
    // allocated when its first member is reached, so buffers are made and
    // freed in the order they always were). Nothing below allocates on the
    // Java heap, so the `base` addresses compared here cannot move.
    let groups = group_permissive_heap_args(
        &permissive_candidates
            .iter()
            .map(|(_, _, view)| {
                (
                    view.base.as_ptr() as usize,
                    view.start,
                    view.size.max(0) as usize,
                )
            })
            .collect::<Vec<_>>(),
    );
    for group in &groups {
        let (_, _, first_view) = permissive_candidates[group.members[0]];
        // The member whose view writes the buffer back: a writable one when
        // the group has one (a read-only view refuses `heap_write_bytes`).
        let rep = group
            .members
            .iter()
            .copied()
            .find(|&m| !permissive_candidates[m].2.read_only)
            .unwrap_or(group.members[0]);
        let (_, rep_segment, rep_view) = permissive_candidates[rep];
        let size = group.size;
        let Some((alloc_id, ptr)) = ctx.allocate_native_memory(size.max(1), first_view.elem_width)
        else {
            // Round 12 wave 7 (lane ffm4): the earlier arguments' buffers.
            {
                for arg in &permissive_heap_args {
                    ctx.free_native_memory(arg.alloc_id);
                }
            }
            return Err(RuntimeError::OutOfMemoryError {
                message: "Failed to allocate a scratch buffer for a downcall argument".into(),
            }
            .into());
        };
        // `rep_view.start >= group.lo` (the group's lowest start), so the
        // offset is `<= 0` and the read starts at `group.lo`.
        let rel = (rep_view.start - group.lo) as usize;
        if let Some(bytes) = heap_read_bytes(ctx, &rep_view, -(rel as i64), size) {
            // SAFETY: `ptr` was just allocated with room for `size` bytes;
            // `bytes.len() == size` by construction of `heap_read_bytes`.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, size) };
        }
        let ranges = permissive_copy_back_ranges(
            group
                .members
                .iter()
                .map(|&m| &permissive_candidates[m].2)
                .filter(|view| !view.read_only)
                .map(|view| ((view.start - group.lo) as usize, view.size.max(0) as usize)),
            size,
        );
        permissive_heap_args.push(PermissiveHeapArg {
            alloc_id,
            ptr: ptr as usize,
            size,
            segment: rep_segment,
            rel,
            ranges,
        });
        for &m in &group.members {
            let (index, _, view) = permissive_candidates[m];
            let at = (ptr as usize).wrapping_add((view.start - group.lo) as usize);
            call_args[index] = Value::Long(at as i64);
        }
    }
    drop(permissive_candidates);

    // Round 12 wave 7 (lane ffm4): every fallible step between the scratch
    // buffers above and the session acquire below -- marshalling, the CIF,
    // the return slot -- is one call, so a refusal frees the buffers instead
    // of leaking them (`r12w6-ffm3-downcall-scratch-buffers-leak-on-error-exits`).
    let prepared = pe_downcall_prepare(
        ctx,
        handle,
        &call_args,
        &param_layout_objs,
        return_layout,
        cached_cif_u64,
        variadic_fixed,
    );
    let (slots, raw_cif_ptr, mut ret_slot) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            {
                for arg in &permissive_heap_args {
                    ctx.free_native_memory(arg.alloc_id);
                }
            }
            return Err(err);
        }
    };
    // JIT round 16 wave 11 (lane ffmcost): classify the handle for the plain
    // path once its CIF is cached. Nothing since the top of this function has
    // allocated on the Java heap, so `handle` is fresh.
    // JIT round 16 wave 12 (lane ffmdoor): the JIT's bound downcall row binds
    // only a classified handle, so its switch records the memo too.
    if (plain_memo || jit_downcall_bind_enabled()) && ctx.object_num_fields(handle) > DOWNCALL_PLAIN
    {
        // JIT round 16 wave 13 (lane ffmseg): a capture handle is classified
        // too (a segment encoding, served only with
        // `CRATONVM_JIT_FFM_DOWNCALL_BIND_SEGMENTS`).
        // JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): a return allocator
        // is a shape the classification encodes under
        // `CRATONVM_FFM_DOWNCALL_MEMO_STRUCT_RETURN`.
        let shape_ok = !address_from_arg0
            && variadic_fixed.is_none()
            && (return_allocator.is_none() || true);
        downcall_record_plain_memo(
            ctx,
            handle,
            shape_ok,
            capture_state.is_some(),
            &param_layout_objs,
            return_layout,
        );
    }

    // libffi expects the args slice to be `&[Arg]` where each `Arg`
    // wraps a `*mut c_void` — which is the address of the typed slot
    // bytes. The slot Vec must outlive the call.
    let arg_refs: Vec<libffi::middle::Arg> = slots
        .iter()
        .map(|s| ffi_arg(unsafe { &*(s.as_ptr() as *const u8) }))
        .collect();

    // Round 12 wave 6 (lane ffm3): acquire every argument segment's session
    // for exactly the call, the last fallible step before it, so that a
    // close meanwhile (a shared close on another thread, or an upcall closing
    // the arena on this one) is refused rather than freeing the block under
    // the C code. Nothing below can move an object before the pins; a refusal
    // releases what it acquired, and the scratch buffers are freed as the
    // call's own exit would.
    let acquired_sessions = match pe_downcall_acquire_sessions(ctx, &acquire_segments) {
        Ok(acquired) => acquired,
        Err(err) => {
            for arg in &permissive_heap_args {
                ctx.free_native_memory(arg.alloc_id);            }
            return Err(err);
        }
    };
    drop(acquire_segments);

    // gc-common w20-f: an upcall reached through a libffi closure during
    // `ffi_call` re-enters Java, which can allocate and move the capture-state
    // segment and every permissive heap-segment argument. Both were used after
    // the call at their pre-call addresses: the copy-back read the backing
    // array through a stale segment (writing into whatever lived there) and
    // `errno` was written into a stale capture-state segment. Pin them across
    // the call; nothing below returns before the pins are released. The
    // return layout is read after the call as well (kind, size, alignment).
    // Round 13 wave 5 (lane ffm3): the return allocator too, pinned first so
    // it is the base whenever it is present.
    let allocator_pin: Option<usize> = return_allocator.map(|a| ctx.pin_native_root(a));
    let return_layout_pin: Option<usize> = return_layout.map(|rl| ctx.pin_native_root(rl));
    let state_pin: Option<usize> = capture_state.map(|state| ctx.pin_native_root(state));
    let segment_pins: Vec<usize> = permissive_heap_args
        .iter()
        .map(|arg| ctx.pin_native_root(arg.segment))
        .collect();
    let call_pin_base: Option<usize> = allocator_pin
        .or(return_layout_pin)
        .or(state_pin)
        .or_else(|| segment_pins.first().copied());

    // ----- Make the call -----
    // Install the active NativeContext so any libffi-closure-backed
    // upcall fired from C during this downcall can re-enter Java.
    // The guard restores the previous slot on drop.
    let ctx_guard = plf::ActiveContextGuard::install(ctx);
    // JIT round 16 wave 14 (lane ffm3): see `pe_downcall_invoke_memo`.
    let spill = if gc_safe {
        Some(plf::CalleeSavedSpill::capture())
    } else {
        None
    };
    // Round 13 wave 5 (lane ffm3), `CRATONVM_FFM_DOWNCALL_GC_SAFE` (default
    // OFF): run the C call inside a GC blocking region, so a stop-the-world
    // pause on another thread does not wait for it
    // (`r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`). Every
    // reference used after the call is pinned above, as an upcall's
    // collection already required; an upcall leaves the region before it
    // runs Java (`panama_upcall::upcall_entry`), and so does a JNIEnv function
    // the C code calls (gcd d5/f, `native::jni::ForeignJniEntry`). `gc_safe`
    // was decided above: a `Linker.Option.critical` handle stays counted, as
    // HotSpot keeps a critical downcall in Java state (gcd d5/f).
    if gc_safe {
        // Round 14 wave 1 (lane ffm): RUNNABLE while in C, as HotSpot reports
        // a thread in native (`begin_blocking_region` reported WAITING).
        // JIT round 16 wave 9 (lane ffm): the quiet in-native transition by
        // default (`ffm_downcall_in_native_quiet`); the guard records which,
        // so an upcall leaves and re-enters the same way.
        // JIT round 16 wave 14 (lane ffm3): or the lazy in-native state
        // (`ffm_downcall_in_native_lazy`).
        downcall_enter_gc_safe(ctx, &ctx_guard);
    }

    // JIT round 17 wave 2 (lane ffm3; EINTR17-1,
    // `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`): see the memo path.
    let masked = plf::downcall_takeover_mask_now();
    let mask = plf::TakeoverSignalMask::block(masked);
    // SAFETY: fn_addr was validated above for non-null and alignment
    // via validated_fn_ptr. The CIF was built from the descriptor, so
    // arg/return types match the slot byte layout. The slot vectors
    // outlive the call (held until return). `raw_cif_ptr` points into
    // the stashed `Box<Cif>` on the handle synthetic (field 3), which
    // lives for the remainder of the handle's lifetime.
    unsafe {
        // ffi_call requires a real writable result address even for a void
        // descriptor. Do not use low::call<()> here: its zero-sized return
        // slot gives libffi a dangling result pointer and caused native void
        // calls (the Elasticsearch bulk-vector symbols) to leave outputs
        // untouched on this ABI.
        let mut void_sink = [0u8; std::mem::size_of::<usize>()];
        let result_ptr = if ret_slot.is_empty() {
            void_sink.as_mut_ptr() as *mut std::ffi::c_void
        } else {
            ret_slot.as_mut_ptr() as *mut std::ffi::c_void
        };
        libffi::raw::ffi_call(
            raw_cif_ptr,
            Some(*CodePtr::from_ptr(fn_addr as *const std::ffi::c_void).as_safe_fun()),
            result_ptr,
            arg_refs.as_ptr() as *mut *mut std::ffi::c_void,
        );
    }
    // gcd d5/f: the call's state for a capture-state segment, read before
    // anything else can overwrite it (`write_downcall_capture_state`).
    let captured = CapturedCallState::now();
    drop(mask);
    if gc_safe {
        // JIT round 16 wave 9 (lane ffm): `leave_region` clears the guard's
        // state and ends the region the way it was opened, with the answer of
        // its latest entry (an upcall re-enters with a fresh one).
        // The barrier's leave can make system calls that overwrite `errno`;
        // it is put back so the Java side sees the thread as the C call left
        // it (the capture state reads `captured`, taken above, since gcd
        // d5/f).
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `__errno_location` answers this thread's errno slot.
            let saved_errno = unsafe { *libc::__errno_location() };
            ctx_guard.leave_region(ctx);
            // SAFETY: as above.
            unsafe { *libc::__errno_location() = saved_errno };
        }
        #[cfg(not(target_os = "linux"))]
        ctx_guard.leave_region(ctx);
    }
    if let Some(spill) = &spill {
        spill.keep_alive();
    }

    // gc-common w20-f: re-read what the call may have moved (see the pins
    // above), then release the pins.
    for (arg, pin) in permissive_heap_args.iter_mut().zip(segment_pins.iter()) {
        arg.segment = ctx.read_native_pin(*pin, arg.segment);
    }
    let capture_state: Option<ObjectRef> = match (capture_state, state_pin) {
        (Some(state), Some(pin)) => Some(ctx.read_native_pin(pin, state)),
        (state, _) => state,
    };
    let return_layout: Option<ObjectRef> = match (return_layout, return_layout_pin) {
        (Some(rl), Some(pin)) => Some(ctx.read_native_pin(pin, rl)),
        (rl, _) => rl,
    };
    let return_allocator: Option<ObjectRef> = match (return_allocator, allocator_pin) {
        (Some(allocator), Some(pin)) => Some(ctx.read_native_pin(pin, allocator)),
        (allocator, _) => allocator,
    };
    if let Some(base) = call_pin_base {
        ctx.unpin_native_roots(base);
    }
    // JIT round 17 wave 1 (lane ffmseg): the state is written while the
    // sessions are still held (`CRATONVM_FFM_CAPTURE_STATE_BEFORE_RELEASE`);
    // nothing in between can move the re-read `capture_state`.
    let capture_written = true;
    if capture_written {
        if let Some(state) = capture_state {
            write_downcall_capture_state(ctx, state, captured);
        }
    }
    // Round 12 wave 6 (lane ffm3): the call is over; release the sessions it
    // held. Their pins were taken before the call pins above, so that
    // truncation left them in place; re-read, unpin, then release.
    if !acquired_sessions.is_empty() {
        let sessions: Vec<ObjectRef> = acquired_sessions
            .iter()
            .map(|&(session, pin)| ctx.read_native_pin(pin, session))
            .collect();
        if let Some(&(_, base)) = acquired_sessions.first() {
            ctx.unpin_native_roots(base);
        }
        pe_downcall_release_sessions(ctx, &sessions);
    }

    // Copy each scratch buffer back into its Java array and free it — the
    // buffer's whole lifetime is this one call. `heap_segment_view` is
    // re-resolved rather than reusing the pre-call `view`: an upcall reached
    // through a libffi closure during the call could re-enter Java and
    // allocate, moving the backing array; `segment` itself is re-read through
    // its pin above (gc-common w20-f).
    for arg in &permissive_heap_args {
        if let Some(view) = heap_segment_view(ctx, arg.segment) {
            let mut buf = vec![0u8; arg.size];
            // SAFETY: `arg.ptr` was allocated with room for `arg.size` bytes
            // above and is still live — it is freed only after this copy.
            unsafe {
                std::ptr::copy_nonoverlapping(arg.ptr as *const u8, buf.as_mut_ptr(), arg.size)
            };
            // zt t18/hunt1: from the buffer's first byte, `rel` below
            // `segment`'s (0 unless the buffer is shared; see above).
            // zt t20/x16: only the writable members' own ranges, so a gap
            // between disjoint slices keeps what Java wrote there.
            for &(from, to) in &arg.ranges {
                heap_write_bytes(ctx, &view, from as i64 - arg.rel as i64, &buf[from..to]);
            }
        }
        ctx.free_native_memory(arg.alloc_id);
    }

    // A diagnostic must not dereference `call_args`' pre-call ObjectRefs:
    // an upcall or a native claim may have moved them, and the sessions have
    // already been released. Argument count alone proves neither the last
    // segment's bounds nor the lifetime/alignment of an f32 at its address.
    if crate::nbflags().dbg_mh_dispatch && return_layout.is_none() && call_args.len() == 5 {
        eprintln!("[PANAMA_POST_VOID] fn=0x{fn_addr:x} returned");
    }
    if !capture_written {
        if let Some(state) = capture_state {
            write_downcall_capture_state(ctx, state, captured);
        }
    }

    // ----- Unmarshal return -----
    let result = match return_layout {
        None => Value::Object(None),
        Some(rl) => {
            let kind = plf::read_layout_kind(ctx, rl);
            if kind == LAYOUT_ADDRESS {
                let address = match plf::unmarshal_return_primitive(kind, &ret_slot) {
                    Value::Long(address) => address,
                    _ => 0,
                };
                // A pointer into an `Arena` block an argument passed is that
                // block's handle plus the offset, the only address the Java
                // side knows it by (`ArgSlot::handle_for`).
                let address = if address != 0 {
                    slots
                        .iter()
                        .find_map(|slot| slot.handle_for(address))
                        .unwrap_or(address)
                } else {
                    address
                };
                // Round 14 wave 1 (lane ffm): JDK 25 boxes an address return
                // with `Utils.longToAddress(addr, pointeeByteSize,
                // pointeeByteAlign)` (the `BoxAddress` binding), so an
                // `ADDRESS.withTargetLayout(T)` return is a segment of `T`'s
                // size and a misaligned pointer is refused -- the same rule as
                // an address read (`pe_address_read_segment`, whose switch
                // `CRATONVM_FFM_ADDRESS_READ_TARGET` restores the zero-length
                // answer; `r13w13-ffm7-address-read-ignores-target-layout`).
                // zs s2/nd2: after the C call nothing raw is held but `rl`,
                // which the target read uses before the mint (the comment
                // above `call_args`), so the mint may collect.
                let collecting = ffm_mint_collects(ctx);
                Value::Object(Some(pe_address_read_segment_with(
                    ctx, rl, address, collecting,
                )?))
            } else if kind == plf::LAYOUT_UNKNOWN {
                // Cannot arrive today — `alloc_return_slot` refused this
                // carrier before the call was made, and it is on every path to
                // here. Named anyway: the old spelling of the arm below was
                // `kind < 10`, which is TRUE for -2, so an unknown carrier
                // would have been unmarshalled as whatever
                // `unmarshal_return_primitive`'s default arm answers rather
                // than reported. See [`layout_kind_is_value`].
                return Err(unclassifiable_access_layout(ctx, rl, "downcall return"));
            } else if layout_kind_is_value(kind) {
                plf::unmarshal_return_primitive(kind, &ret_slot)
            } else {
                // Struct/union/sequence return: copy the bytes into a
                // freshly allocated MemorySegment via the global arena
                // path. The Java caller will then read the segment.
                //
                // Sized from the layout's own `byteSize`/`byteAlignment` and
                // not from `plf::layout_total_size` — see the note at
                // `Arena.allocate(MemoryLayout)`. `layout_total_size` answers 8
                // for EVERY group layout (its `read_layout_kind` resolves a
                // `Long`-slot-0 carrier by class name, and no GROUP class is on
                // that list, so it defaults to `LAYOUT_LONG`), so a by-value
                // struct return of any width got an 8-byte segment.
                let total =
                    crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, rl).max(0) as usize;
                let align =
                    crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, rl).max(1) as usize;
                // Round 13 wave 5 (lane ffm3): into the caller's allocator.
                // JIT round 18 wave 11 (lane ffmret;
                // `CRATONVM_FFM_GROUP_RETURN_ALLOCATE_BEFORE_CALL`): an
                // in-memory group's buffer was allocated before the call; the
                // bytes go into it (re-read through its pin).
                if let Some((buffer, pin)) = prealloc {
                    let buffer = ctx.read_native_pin(pin, buffer);
                    downcall_store_group(ctx, buffer, total, &ret_slot, false, None)?;
                    return Ok(Some(Value::Object(Some(buffer))));
                }
                if let Some(allocator) = return_allocator {
                    // JIT round 18 wave 11 (lane ffmret;
                    // `CRATONVM_FFM_GROUP_RETURN_HEAP_ALLOCATOR`): where the ABI
                    // returned the group decides whether a heap segment may
                    // take it.
                    // SAFETY: `raw_cif_ptr` is this handle's prepared CIF,
                    // which outlives the handle.
                    let in_registers = unsafe { cif_group_returned_in_registers(raw_cif_ptr) };
                    // SAFETY: as above.
                    let hfa_width = unsafe { group_hfa_width((*raw_cif_ptr).rtype) };
                    return downcall_copy_group_return_in(
                        ctx,
                        allocator,
                        total,
                        align,
                        &ret_slot,
                        in_registers,
                        hfa_width,
                    )
                    .map(|seg| Some(Value::Object(Some(seg))));
                }
                let (alloc_id, ptr) = ctx.allocate_native_memory(total, align.max(8)).ok_or_else(
                    || -> MethodCallFailed {
                        RuntimeError::OutOfMemoryError {
                            message: "Failed to allocate result MemorySegment".into(),
                        }
                        .into()
                    },
                )?;
                // THE COPY LENGTH IS CLAMPED TO `ret_slot`, DELIBERATELY.
                //
                // `ret_slot` is sized by `panama_libffi::alloc_return_slot`
                // and `total` by `foreign_ffm::p67_layout_size_of` — two size
                // functions in two files, with one `unsafe
                // copy_nonoverlapping` between them. Copying `total` bytes
                // unconditionally would read past the end of `ret_slot` the
                // moment those two disagree in that direction, which is what
                // fixing the size on only one side of the pair produces.
                //
                // THE MATCHING FIX LANDED (F27, 2026-08-13).
                // `panama_libffi::layout_total_size` now reads the carrier's
                // own `[0]=byteSize`, and `alloc_return_slot` rounds that up to
                // the layout's alignment because libffi writes the C size of an
                // aggregate (measured: struct(JAVA_LONG, JAVA_INT) is 12 to the
                // JDK and 16 to C). So `ret_slot.len() >= total` always and this
                // clamp no longer truncates anything.
                //
                // IT STAYS ANYWAY. `total` is computed by `p67_layout_size_of`
                // in foreign_ffm.rs and `ret_slot.len()` by `alloc_return_slot`
                // in panama_libffi.rs; the clamp is the LOCAL proof that the
                // unsafe copy below is in bounds. Removing it would make an
                // unsafe block's safety argument depend on two functions in two
                // files continuing to agree, with nothing at the site saying so.
                let copy_len = total.min(ret_slot.len());
                // SAFETY: `ptr` was freshly allocated with `total >= copy_len`
                // bytes, and `copy_len <= ret_slot.len()`, so both sides are in
                // bounds. The regions cannot overlap — one is a fresh
                // allocation.
                unsafe {
                    std::ptr::copy_nonoverlapping(ret_slot.as_ptr(), ptr, copy_len);
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(total as i64));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                let _ = alloc_id;
                Value::Object(Some(seg))
            }
        }
    };

    Ok(Some(result))
}

/// What [`pe_downcall_prepare`] builds: the marshalled argument slots, the
/// (cached or fresh) CIF, and the return slot.
pub(in crate::panama) type PeDowncallPrepared = (
    Vec<crate::panama_libffi::ArgSlot>,
    *mut libffi::low::ffi_cif,
    Vec<u8>,
);

/// The fallible prelude of [`pe_downcall_invoke`]: marshal the arguments,
/// fetch or build and cache the CIF, allocate the return slot. None of it
/// allocates on the Java heap or runs Java, so a shared context is enough and
/// no reference the caller holds can move (round 12 wave 7, lane ffm4: split
/// out so that every refusal here reaches the caller's one scratch-buffer
/// free).
pub(in crate::panama) fn pe_downcall_prepare(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    call_args: &[Value],
    param_layout_objs: &[Option<ObjectRef>],
    return_layout: Option<ObjectRef>,
    cached_cif_u64: u64,
    variadic_fixed: Option<usize>,
) -> Result<PeDowncallPrepared, MethodCallFailed> {
    use crate::panama_libffi as plf;

    // ----- Marshal arguments into per-slot scratch storage -----
    // Must happen before the CIF hit/miss fork because it allocates
    // per-call storage that depends on the actual Value inputs.
    let mut slots: Vec<plf::ArgSlot> = Vec::with_capacity(call_args.len());
    for (i, val) in call_args.iter().enumerate() {
        let layout =
            param_layout_objs
                .get(i)
                .copied()
                .flatten()
                .ok_or_else(|| -> MethodCallFailed {
                    RuntimeError::IllegalStateException {
                        message: format!("Panama downcall parameter {} layout is null", i),
                    }
                    .into()
                })?;
        slots.push(plf::marshal_arg(ctx, layout, val)?);
    }

    // ----- CIF: cache hit / miss -----
    //
    // If the DowncallHandle carries a previously boxed Cif, reuse it.
    // Otherwise build a fresh Cif, box it, and stash the raw pointer
    // on field 3 so the next invocation skips layout→ffi_type
    // translation and the libffi `prep_cif` call.
    //
    // After this block, `raw_cif_ptr` always points at a Cif living
    // inside the stashed Box on the handle synthetic. The Box outlives
    // us (it lives until handle finalization).
    let raw_cif_ptr: *mut libffi::low::ffi_cif = if cached_cif_u64 != 0 {
        // SAFETY: `cached_cif_u64` was produced by box_cif_to_u64 in
        // a prior invocation on this same handle; the Box outlives
        // this call (freed on handle finalization). libffi only
        // reads the Cif during ffi_call.
        let cif_ref =
            unsafe { plf::cached_cif_ref(cached_cif_u64) }.expect("non-zero pointer must deref");
        cif_ref.as_raw_ptr()
    } else {
        // ----- Build libffi types for each parameter -----
        let mut ffi_arg_types = Vec::with_capacity(param_layout_objs.len());
        for (i, pl) in param_layout_objs.iter().enumerate() {
            let layout = pl.ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!("Panama downcall parameter {} layout is null", i),
                }
                .into()
            })?;
            ffi_arg_types.push(plf::layout_to_ffi_type(ctx, layout, 0)?);
        }

        // Return type
        let ret_ffi_type = match return_layout {
            Some(rl) => plf::layout_to_ffi_type(ctx, rl, 0)?,
            None => libffi::middle::Type::void(),
        };

        // Build CIF (variadic-aware) and record the build for tests.
        let cif = plf::build_cif_and_record(ffi_arg_types, ret_ffi_type, variadic_fixed)?;
        // Move the Cif into a Box, stash the raw pointer, and return a
        // pointer to the Box-owned Cif for this call.
        let stash_u64 = plf::box_cif_to_u64(cif);
        ctx.set_field(handle, DOWNCALL_CIF, Value::Long(stash_u64 as i64));
        // SAFETY: stash_u64 was produced above; Box is live for the
        // remainder of this call and beyond.
        let cif_ref =
            unsafe { plf::cached_cif_ref(stash_u64) }.expect("just-stashed pointer must deref");
        cif_ref.as_raw_ptr()
    };

    // ----- Allocate return slot -----
    let ret_slot = plf::alloc_return_slot(ctx, return_layout)?;
    Ok((slots, raw_cif_ptr, ret_slot))
}

// --- FunctionDescriptor: native function signature ---
// FunctionDescriptor: [0]=return layout (Object or null for void), [1]=param layouts (Object array)

/// Mint a `FunctionDescriptor` carrier: [0] return layout (null for void),
/// [1] parameter-layout array.
///
/// gc-common w20-f: the four factories stored both argument references at
/// their addresses from before the carrier's allocation. They are pinned across
/// it and re-read; a failed allocation releases the pins before its `?` exit.
pub(in crate::panama) fn pe_new_function_descriptor(
    ctx: &mut dyn NativeContext,
    ret_layout: Value,
    params: Value,
) -> MethodCallResult {
    let ret_pin: Option<usize> = match ret_layout {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let params_pin: Option<usize> = match params {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let desc_alloc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2);
    let ret_layout = match (ret_layout, ret_pin) {
        (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        (v, _) => v,
    };
    let params = match (params, params_pin) {
        (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        (v, _) => v,
    };
    if let Some(base) = ret_pin.or(params_pin) {
        ctx.unpin_native_roots(base);
    }
    let desc: ObjectRef = desc_alloc?;
    ctx.set_field(desc, 0, ret_layout);
    ctx.set_field(desc, 1, params);
    Ok(Some(Value::Object(Some(desc))))
}

pub(in crate::panama) fn register_pe_function_descriptor(r: &mut NativeMethodRegistry) {
    let fd = "java/lang/foreign/FunctionDescriptor";

            // of(returnLayout, paramLayouts...) → FunctionDescriptor
    r.register(fd, "of", "(Ljava/lang/foreign/ValueLayout;[Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        pe_new_function_descriptor(ctx, ret_layout, params)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(fd, "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        pe_new_function_descriptor(ctx, ret_layout, params)
    }, cratonvm_native_api::NativeKind::Bridge);

    // ofVoid(paramLayouts...) → FunctionDescriptor
    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            // void return
            pe_new_function_descriptor(ctx, Value::Object(None), params)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            pe_new_function_descriptor(ctx, Value::Object(None), params)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // returnLayout() → Optional<ValueLayout>
    r.register(fd, "returnLayout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rl = ctx.get_field(this, 0);
        // Round 14 wave 3 (lane ffm): through `p67_optional`, which pins the
        // layout across the `Optional`'s allocation; it was stored from its
        // address from before.
        let opt = crate::phases_late::foreign_ffm::p67_optional(ctx, rl)?;
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // argumentLayouts() → java.util.List
    //
    // G19-1 NOM-1. THE SIGNATURE WAS `()[Ljava/lang/foreign/ValueLayout;` —
    // the pre-JDK-22 PREVIEW spelling. MEASURED on 25.0.3+9-LTS:
    // `argumentLayouts()` returns `java.util.List`
    // (`java.util.ImmutableCollections$List12` for a one-argument
    // descriptor), so a caller writing `fd.argumentLayouts().size()` against
    // the shipping API got `AbstractMethodError: ...argumentLayouts()
    // Ljava/util/List; has no Code attribute` even where this registrar IS
    // reached — which is `--synthetic-jdk` only, since
    // `register_pe_function_descriptor` runs from `register_pe_panama` /
    // `register_synthetic_overrides` and neither shipping binary executes it.
    // The row that a `--real-jdk` binary answers is
    // `foreign_ffm::register_p67_foreign_memory`'s; the two now agree on the
    // descriptor, so whichever wins a given build answers the same question.
    r.register(fd, "argumentLayouts", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let array = match ctx.get_field(this, 1) {
            Value::Object(Some(arr)) => arr,
            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
        };
        let pin = ctx.pin_native_root(array);
        let list = ctx
            .invoke(
                "java/util/List",
                "of",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten();
        let list = match list {
            Some(Value::Object(Some(_))) => list,
            _ => {
                let array = ctx.read_native_pin(pin, array);
                ctx.invoke(
                    "java/util/Arrays",
                    "asList",
                    "([Ljava/lang/Object;)Ljava/util/List;",
                    &[Value::Object(Some(array))],
                )
                .ok()
                .flatten()
            }
        };
        ctx.unpin_native_roots(pin);
        Ok(Some(list.unwrap_or(Value::Object(None))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// =============================================================================
// FFM upcall stubs: the entry points (round 12 wave 7, lane upcall)
// =============================================================================
//
// The stubs themselves -- the libffi closures, their registry, the dispatch
// back into Java, the GC halves -- are `panama_upcall.rs`. What stays here is
// what needs this file's private gates (`require_native_access`) and the two
// `pub` GC entry points `vm/src/memory/native_roots.rs` calls by path.
//
// DELETED with this change: the "Phase 85.4" fixed trampoline pool
// (`MAX_UPCALL_TRAMPOLINES`, `UPCALL_DISPATCH_TABLE`, `gen_trampoline!` x 64,
// `UPCALL_TRAMPOLINE_FNS`). Nothing ever handed its function pointers out
// (`allocate_trampoline_slot` had no caller) and its dispatcher returned 0
// without calling Java; it was two process-global statics of dead code. The
// NEW-18 libffi closures that replaced it moved to `panama_upcall.rs`, and
// `UPCALL_REGISTRY` with them (as `panama_upcall::UPCALL_STUBS`).

// =============================================================================
// Phase E2: Struct/Union Layouts, Upcalls, String Marshaling
// =============================================================================

/// Step 5 GAP C — GC root scan for FFM upcall stubs: each stub holds its Java
/// target (a `MethodHandle`) and, for a closeable arena, that arena's session,
/// through state the collector cannot otherwise see. Only `vm`'s stubs:
/// another VM's objects are in another heap (gc-common w8-a). Companion of
/// [`gc_update_upcall_target_refs`]; both are `panama_upcall`'s, kept here
/// because `vm/src/memory/native_roots.rs` calls them by this path.
pub fn gc_scan_upcall_target_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    crate::panama_upcall::gc_scan_roots(vm, out);
}

/// Step 5 GAP C — post-move remap for the upcall stub roots (companion of
/// [`gc_scan_upcall_target_roots`]).
pub fn gc_update_upcall_target_refs(vm: usize, map: &cratonvm_types::PointerMap) {
    crate::panama_upcall::gc_update_refs(vm, map);
}

/// A scope for an upcall stub: the class of the Java target it dispatches into.
///
/// A `ForeignUpcall` denial that cannot say *which* callback was refused is not
/// actionable, and this is the narrowest name reachable here — the stub's
/// descriptor is a layout list, not a method signature. Falls back to
/// `<unknown>` so a denial always names something.
pub(in crate::panama) fn upcall_target_name(ctx: &dyn NativeContext, target: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(target))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// `Linker.upcallStub(MethodHandle, FunctionDescriptor, Arena, Linker.Option...)`
/// -- registered by `panama_upcall::register_panama_upcall` -- and, through
/// [`pe_upcall_handle`], the synthetic-JDK `upcallHandle` row.
///
/// The gates, in `AbstractLinker.upcallStub`'s order: native access (GAP F3:
/// fails closed, like the downcall path; a stub hands C a real entry into
/// Java), then `Objects.requireNonNull` on the arena, the target and the
/// descriptor. Then the `ForeignUpcall` capability row (permissive by default)
/// and the kill switch; the stub itself is `panama_upcall::create_upcall_stub`.
pub(crate) fn pe_upcall_stub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The check runs Java (`Module.ensureNativeAccess`), a GC point: every
    // reference argument is pinned across it and read back after, since the
    // stub stores `target` raw in its registry, where a stale address is a
    // root nothing remaps (`R22FfmUpcallChurn`).
    let pins: Vec<Option<usize>> = args
        .iter()
        .map(|arg| match arg {
            Value::Object(Some(obj)) => Some(ctx.pin_native_root(*obj)),
            _ => None,
        })
        .collect();
    let checked = restricted_linker_check(ctx, "upcallStub");
    let args: Vec<Value> = args
        .iter()
        .zip(&pins)
        .map(|(arg, pin)| match (arg, pin) {
            (Value::Object(Some(obj)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(*pin, *obj)))
            }
            _ => *arg,
        })
        .collect();
    if let Some(base) = pins.iter().flatten().next() {
        ctx.unpin_native_roots(*base);
    }
    checked?;
    let required = |index: usize| -> Result<ObjectRef, MethodCallFailed> {
        match args.get(index) {
            Some(Value::Object(Some(obj))) => Ok(*obj),
            _ => Err(RuntimeError::NullPointerException { message: None }.into()),
        }
    };
    let arena = required(3)?;
    let target = required(1)?;
    let descriptor = required(2)?;
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;
    if !crate::panama_upcall::upcall_stubs_enabled() {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "FFM upcall stubs are disabled (CRATONVM_FFM_UPCALL_STUBS=0)".into(),
        }
        .into());
    }
    let options = match args.get(4) {
        Some(Value::Object(Some(options))) => Some(*options),
        _ => None,
    };
    crate::panama_upcall::create_upcall_stub(ctx, target, descriptor, arena, options)}

/// `Linker.upcallHandle(target, descriptor, arena)`: the pre-release
/// spelling, registered only by the synthetic-JDK `register_pe_linker`. Same
/// stub as `upcallStub` with no options. It used to also record the target in
/// the VM's legacy upcall slot table (`register_upcall`), a root nothing ever
/// released; the `UpcallStub.invoke` row that read that table is unreachable.
pub(in crate::panama) fn pe_upcall_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_upcall_stub(ctx, args)
}

/// The synthetic-JDK `UpcallStub.invoke(Object[])` row: the target of a legacy
/// upcall slot, called with the array's elements. Nothing in production feeds
/// that slot table any more (`pe_upcall_handle` stopped; only tests call
/// `register_upcall`), so every real call answers "slot not found"; C reaches
/// Java through `panama_upcall`'s closures.
pub(in crate::panama) fn pe_upcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let slot = match ctx.get_field(handle, 0) {
        Value::Long(n) => n as usize,
        _ => 0,
    };

    let (target, _param_kinds, return_kind) =
        ctx.get_upcall_info(slot)
            .ok_or_else(|| RuntimeError::IllegalStateException {
                message: format!("Upcall slot {} not found", slot),
            })?;

    // GAP F4: invoking an upcall stub was ungated. The `ForeignUpcall` check is
    // permissive by default; the scope is the callback's class, resolved after
    // the slot lookup so an unknown slot still reports "slot not found".
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;

    // Round 15 wave 5 (lane upcall1; `r15w5-mh4-review-findings` item 6): the
    // arguments go to the target as the `Object[]` they arrived in, through
    // `invokeWithArguments`, whose door types the call as
    // `genericMethodType(n)` itself. This spread them into
    // `invoke` under the one-parameter descriptor
    // `(Ljava/lang/Object;)Ljava/lang/Object;`, which the `invoke` door
    // publishes as the call site: every arity refusal that reads the site
    // then judged an N-argument call against one parameter. A missing array is
    // the empty argument list, as before (`invokeWithArguments((Object[]) null)`
    // is the zero-argument call on HotSpot too).
    let arguments = match args.get(1) {
        Some(Value::Object(Some(arr))) => Value::Object(Some(*arr)),
        _ => Value::Object(None),
    };
    let result = ctx.invoke_virtual(
        target,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[arguments],
    );
    match result {
        Ok(val) => {
            let _ = return_kind;
            Ok(val.or(Some(Value::Object(None))))
        }
        Err(e) => Err(e),
    }
}

// --- MemoryLayout$PathElement ---
//
// This registrar KEEPS ITS NAME and has lost its subject. The
// StructLayout/UnionLayout/SequenceLayout family it was written for lives in
// `phases_late/foreign_ffm.rs` now; what is left is the two path-element
// factories, which have no twin there. The banner that used to sit here
// described the deleted 6-slot `[kind, size, members, names, offsets, align]`
// carrier — the authoritative one is 4-slot `[byteSize, byteAlignment,
// payload, name]` and is documented at `foreign_ffm.rs::p67_member_size_align`.
pub(in crate::panama) fn register_pe2_struct_layouts(r: &mut NativeMethodRegistry) {
    let ml = "java/lang/foreign/MemoryLayout";

    // THE WHOLE GROUP-LAYOUT FAMILY WAS DELETED FROM HERE
    // (F16, 2026-08-13). It is now `phases_late/foreign_ffm.rs`'s, alone.
    //
    // What stood here: `structLayout`, `unionLayout`, `sequenceLayout` and
    // `paddingLayout`, each registered TWICE — once under the JDK-true return
    // type and once under a fabricated `…)Ljava/lang/foreign/MemoryLayout;`
    // one — plus `byteSize`/`byteAlignment`/`name`/`withName`/`memberLayouts`
    // on `StructLayout` and `GroupLayout`, `byteOffset` on `StructLayout`, and
    // `withName`/`varHandle`/`name`/`byteSize` on `MemoryLayout`.
    //
    // EVERY ONE OF THEM HAD A TWIN in `register_p67_foreign_memory`, and the
    // twins disagreed, because the two files carry two different layout
    // objects:
    //
    //     panama       [0]=Int(kind) [1]=size [2]=members [3]=names
    //                  [4]=offsets   [5]=align
    //     foreign_ffm  [0]=Long(byteSize) [1]=Long(byteAlignment)
    //                  [2]=payload        [3]=name
    //
    // Which one a caller got was decided by REGISTRATION ORDER, not by the
    // descriptor it wrote: `register_pe_panama` runs after
    // `register_p67_foreign_memory`, and `register()` is last-write-wins, so
    // these rows took the JDK-true keys away from the JDK-true bodies — but
    // only in synthetic-JDK mode, since real-JDK mode never calls
    // `register_pe_panama` at all. One VM, two answers, chosen by which mode
    // you booted.
    //
    // The surviving implementation is also the CORRECT one, which the deleted
    // `pe_struct_layout` was not. Measured on HotSpot 25.0.3+9-LTS:
    //
    //     structLayout(JAVA_BYTE, JAVA_INT)  -> IllegalArgumentException
    //     structLayout(JAVA_INT, JAVA_LONG)  -> IllegalArgumentException
    //     structLayout(JAVA_LONG, JAVA_INT)  -> byteSize=12  (NOT 16)
    //
    // `pe_struct_layout` auto-padded the first two into a fabricated success
    // and rounded the third up to 16. The JDK never pads a struct: the caller
    // writes `paddingLayout(...)`, and an under-aligned member is an error.
    //
    // `MemoryLayout$PathElement`'s two factories are the ONLY thing kept, and
    // deliberately: `foreign_ffm.rs` decodes path elements but mints none,
    // because in real-JDK mode `PathElement.groupElement("c")` runs the JDK's
    // own bytecode and yields a `jdk.internal.foreign.LayoutPath$…` record.
    // Synthetic-JDK mode has no such bytecode, so these two rows are its only
    // source — and the 2-field carrier they build is a shape
    // `p67_classify_path_element` explicitly accepts.

    // PathElement.groupElement(name) → PathElement
    let pe = "java/lang/foreign/MemoryLayout$PathElement";
    r.register(
        pe,
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let elem = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/foreign/MemoryLayout$PathElement",
                2,
            )?;
            ctx.set_field(elem, 0, name); // field name
            ctx.set_field(elem, 1, Value::Int(0)); // kind=group
            Ok(Some(Value::Object(Some(elem))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        pe,
        "sequenceElement",
        "()Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, _| {
            let elem = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/foreign/MemoryLayout$PathElement",
                2,
            )?;
            ctx.set_field(elem, 0, Value::Object(None));
            ctx.set_field(elem, 1, Value::Int(1)); // kind=sequence
            Ok(Some(Value::Object(Some(elem))))
        }, NativeKind::SyntheticStub,
    );
}

// THE LAST SURVIVOR OF THE GROUP-LAYOUT FAMILY IS GONE TOO (G6, 2026-08-16).
//
// `pe_memory_layout_width` stood here — a size reader that understood BOTH
// this file's old `[0]=Int(kind)` carrier and `foreign_ffm`'s
// `[0]=Long(byteSize)` one. It had exactly one caller,
// `asSlice(long, MemoryLayout)`, and that caller now reads size AND alignment
// out of the four-slot carrier in one call through
// `foreign_ffm::p67_layout_size_align`, because the JDK's own body is
// `asSlice(offset, layout.byteSize(), layout.byteAlignment())` and a reader
// that answers only the size cannot express the second half.
//
// This closes F16-1's remaining question. The reconciling arm this function
// existed for — "which of the two encodings am I looking at?" — has nothing
// left to reconcile: `pe_make_layout`, the only minter of `[0]=Int(kind)`, is
// `#[cfg(test)]`, and every shipping carrier comes from `p67_layout_object` or
// the four group-layout factories beside it. One encoding, one reader.

// --- String marshaling helpers ---

pub(in crate::panama) fn register_pe2_string_marshaling(r: &mut NativeMethodRegistry) {
    for ms in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        register_pe2_string_marshaling_on(r, ms);
    }
}

/// `MemorySegment.reinterpret(long)` -- a new segment over the SAME address,
/// with a caller-chosen size.
///
/// A free function rather than a closure because two registrars need to name
/// it. They each carried their OWN body until 2026-08-24, and the shipping one
/// -- the only body a `--real-jdk` process ever had -- copied slots 0..5
/// verbatim and performed no native-access check at all.
///
/// The address is read through `panama_libffi::segment_address` so a REAL
/// JDK-loaded segment is not mistaken for the synthetic six-slot carrier, on
/// which slot 0 means something else entirely.
pub(crate) fn pe_segment_reinterpret(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `reinterpret` IS `@Restricted` in the JDK — it grants an arbitrary access
    // window over a possibly-raw address. But restricted does not mean refused:
    // a JDK 25 launcher warns and proceeds unless `--illegal-native-access=deny`
    // (measured, see [`IllegalNativeAccess`]). Refusing unconditionally is a
    // policy CratonVM invented, and it is load-bearing in the worst place — a
    // static initialiser, where the throw is permanent for the class.
    restricted_caller_check(ctx, "java/lang/foreign/MemorySegment", "reinterpret")?;
    let new_size = match args.get(1) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    pe_reinterpret_checks(ctx, this, new_size)?;
    // `reinterpretInternal` with no cleanup ends in `checkValidState()`: a
    // closed segment cannot be reinterpreted (IllegalStateException), nor
    // another thread's confined one (WrongThreadException).
    {
        pe_segment_check_scope(ctx, this)?;
    }
    let ptr = crate::panama_libffi::segment_address(ctx, this);
    // Slot 3 only where there is one (round 12 wave 7, lane ffm4): an
    // `ADDRESS` read through a layout VarHandle mints a TWO-slot carrier, and
    // `get(ADDRESS, ..).reinterpret(n)` is the common way to widen it.
    let read_only = match ctx.get_field_by_name(this, "readOnly") {
        Value::Int(n) => Value::Int(n),
        _ if ctx.object_num_fields(this) > 3 => ctx.get_field(this, 3),
        _ => Value::Int(0),
    };

    // G19-1 §2.2: A REINTERPRETED SEGMENT STAYS IN ITS PARENT'S SCOPE.
    //
    // MEASURED on 25.0.3+9-LTS: `cs.reinterpret(8).scope() == cs.scope()` is
    // **true** — the one-argument overload keeps the original's scope; only
    // the three-argument `reinterpret(long, Arena, Consumer)` moves a segment
    // to a different one. Slot 2 was an unconditional `Object(None)` here,
    // which is the same "no scope resolvable" hole `asSlice`'s two arms each
    // had before W7-89 and G19-1 closed them, and it has the same second
    // consequence: `p67_receiver_session` minted a fresh ALWAYS-OPEN session,
    // so a reinterpretation of a closed arena's segment reported a live scope
    // and every liveness check resolving through it passed.
    let parent_session = pe_segment_session(ctx, this);
    let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
    let seg = match alloc_segment_carrier(ctx, 6) {
        Ok(seg) => seg,
        Err(err) => {
            // Round 14 wave 3 (lane ffm): not with the pin taken.
            pe_release_pins(ctx, session_pin);
            return Err(err);
        }
    };
    // The allocation above is a safepoint and can move the session; re-read it
    // through the pin before storing it.
    let scope_value = match (parent_session, session_pin) {
        (Some(session), Some(pin)) => {
            let session = ctx.read_native_pin(pin, session);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(session))
        }
        _ => Value::Object(None),
    };
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(new_size));
    ctx.set_field(seg, 2, scope_value);
    ctx.set_field(seg, 3, read_only);
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}



/// `Utils.checkNonNegativeArgument(newSize, "newSize")` then `if (!isNative())
/// throw new UnsupportedOperationException("Not a native segment")`, in the
/// JDK's order. A heap segment has no address to re-window: before this the
/// answer was a native carrier at address 0.
pub(in crate::panama) fn pe_reinterpret_checks(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    new_size: i64,
) -> Result<(), MethodCallFailed> {
    if new_size < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("The provided newSize is negative: {new_size}"),
        }
        .into());
    }
    if crate::panama_libffi::is_real_heap_segment(ctx, this)
        || heap_segment_view(ctx, this).is_some()
    {
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Not a native segment".into(),
        }
        .into());
    }
    Ok(())
}

/// `MemorySegment.reinterpret(long, Arena, Consumer)` (`sized`) and
/// `reinterpret(Arena, Consumer)` (round 12 wave 7, lane ffm4).
///
/// A new native segment over the same address, `newSize` bytes (or this
/// segment's size), in `arena`'s scope; a non-null `cleanup` is registered as
/// a close action of that scope and receives a fresh global-scope segment
/// over the same memory, so it runs at close with the memory still live --
/// before the arena's own blocks are freed, since those are recorded at the
/// arena's first allocation and the close walk is newest-first (see
/// `foreign_ffm::p67_session_run_close_actions`). A closed or foreign-confined
/// arena throws (`IllegalStateException` / `WrongThreadException`) without
/// running the cleanup -- `addCloseAction` is `addInternal`, not
/// `addOrCleanupIfFail` -- and so does a null cleanup (`checkValidState`).
///
/// A scope this model does not own (the real `GLOBAL_SESSION`, which never
/// closes) keeps the segment unscoped and registers nothing.
pub(crate) fn pe_segment_reinterpret_scoped(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    sized: bool,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (new_size, arena_index) = if sized {
        let size = match args.get(1) {
            Some(Value::Long(n)) => *n,
            _ => 0,
        };
        (size, 2)    } else {
        (crate::panama_libffi::segment_byte_size(ctx, this), 1)    };
    // `Objects.requireNonNull(arena)` comes before everything else.
    let Some(Value::Object(Some(arena))) = args.get(arena_index).copied() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let cleanup = match args.get(arena_index + 1) {
        Some(Value::Object(Some(cleanup))) => Some(*cleanup),
        _ => None,
    };
    restricted_caller_check(ctx, "java/lang/foreign/MemorySegment", "reinterpret")?;
    pe_reinterpret_checks(ctx, this, new_size)?;
    // Everything read off `this` before anything below can run Java. Slot 3
    // only where there is one: an `ADDRESS` read mints a two-slot carrier.
    let ptr = crate::panama_libffi::segment_address(ctx, this);
    let read_only = match ctx.get_field_by_name(this, "readOnly") {
        Value::Int(n) => Value::Int(n),
        _ if ctx.object_num_fields(this) > 3 => ctx.get_field(this, 3),
        _ => Value::Int(0),
    };
    // `arena.scope()` can run Java: `cleanup` is pinned across it and across
    // the allocations below. Pins are a stack: unpinning the first one taken
    // (`base`) releases every later one too.
    let cleanup_pin = cleanup.map(|c| ctx.pin_native_root(c));
    let scope = match crate::phases_late::foreign_ffm::p67_arena_scope_for_reinterpret(ctx, arena) {
        Ok(scope) => scope,
        Err(err) => {
            pe_release_pins(ctx, cleanup_pin);
            return Err(err);
        }
    };
    // A REAL JDK session (`--real-jdk`'s `Arena.ofShared()` / `ofConfined()`):
    // this carrier model cannot represent it -- its liveness checks read the
    // modelled session's slots -- so the segment is the JDK's own, minted the
    // way `AbstractMemorySegmentImpl.reinterpret` mints it. Round nio20 wave 3
    // fix-back (lane L5): the real scope used to read as "no scope", so the
    // result was a global-scope carrier that kept reading after the arena
    // closed (`Nio20ArenaClose cleanup`: worker TIMEOUT, closer read
    // succeeded) and the cleanup was never registered.
    if let Some(real) = scope {
        if crate::phases_late::foreign_ffm::p67_session_is_real(ctx, real) {
            let session_pin = ctx.pin_native_root(real);
            let base_pin = cleanup_pin.or(Some(session_pin));
            let cleanup = cleanup.zip(cleanup_pin);
            let result = pe_reinterpret_real_session(
                ctx,
                ptr,
                new_size,
                !matches!(read_only, Value::Int(0)),
                (session_pin, real),
                cleanup,
            );
            pe_release_pins(ctx, base_pin);
            return result;
        }
    }
    let session_pin = scope.map(|s| ctx.pin_native_root(s));
    let base_pin = cleanup_pin.or(session_pin);
    let seg = match alloc_segment_carrier(ctx, 6) {
        Ok(seg) => seg,
        Err(err) => {
            pe_release_pins(ctx, base_pin);
            return Err(err);
        }
    };
    let seg_pin = ctx.pin_native_root(seg);
    let base_pin = base_pin.or(Some(seg_pin));
    // The segment the cleanup receives: `makeNativeSegmentUnchecked(address(),
    // newSize)`, i.e. the global scope, so it stays accessible while the
    // arena's own scope is closing.
    let cleanup_seg = match cleanup {
        Some(_) => match alloc_segment_carrier(ctx, 6) {
            Ok(cleanup_seg) => {
                ctx.set_field(cleanup_seg, 0, Value::Long(ptr));
                ctx.set_field(cleanup_seg, 1, Value::Long(new_size));
                ctx.set_field(cleanup_seg, 2, Value::Object(None));
                ctx.set_field(cleanup_seg, 3, Value::Int(0));
                ctx.set_field(cleanup_seg, 4, Value::Int(1));
                ctx.set_field(cleanup_seg, 5, Value::Long(0));
                Some(cleanup_seg)
            }
            Err(err) => {
                pe_release_pins(ctx, base_pin);
                return Err(err);
            }
        },
        None => None,
    };
    let mut session = match (scope, session_pin) {
        (Some(s), Some(pin)) => Some(ctx.read_native_pin(pin, s)),
        _ => None,
    };
    let cleanup = match (cleanup, cleanup_pin) {
        (Some(c), Some(pin)) => Some(ctx.read_native_pin(pin, c)),
        (c, _) => c,
    };
    // Register (or, with no cleanup, just check). Both can run Java, so the
    // result segment and the session stay pinned and are re-read after.
    let registered = match (session.as_mut(), cleanup, cleanup_seg) {
        (Some(session), Some(cleanup), Some(cleanup_seg)) => {
            crate::phases_late::foreign_ffm::p67_session_add_cleanup(
                ctx,
                session,
                cleanup,
                cleanup_seg,
            )
        }
        (Some(session), _, _) => {
            crate::phases_late::foreign_ffm::p67_session_check_valid(ctx, session)
        }
        (None, _, _) => Ok(()),
    };
    let session = match (session, session_pin) {
        (Some(s), Some(pin)) => Some(ctx.read_native_pin(pin, s)),
        (s, _) => s,
    };
    let seg = ctx.read_native_pin(seg_pin, seg);
    pe_release_pins(ctx, base_pin);
    registered?;
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(new_size));
    ctx.set_field(
        seg,
        2,
        session.map_or(Value::Object(None), |s| Value::Object(Some(s))),
    );
    ctx.set_field(seg, 3, read_only);
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// `reinterpret(newSize, arena, cleanup)` into a REAL JDK session: the JDK's
/// own `AbstractMemorySegmentImpl.reinterpretInternal` bytecode on a fresh
/// global-scope `NativeMemorySegmentImpl` at `ptr`
/// (`SegmentFactories.makeNativeSegmentUnchecked(ptr, 0)`), so the session
/// check, the cleanup registration (`addCloseAction` of the JDK's own
/// `cleanup.accept(..)` action) and the segment are HotSpot's. The caller has
/// already run the restricted-method check against the real caller, so the
/// JDK's `ensureNativeAccess` is handed `MemorySegment` (java.base). `session`
/// and `cleanup` are `(pin, value)` pairs the caller releases.
pub(in crate::panama) fn pe_reinterpret_real_session(
    ctx: &mut dyn NativeContext,
    ptr: i64,
    new_size: i64,
    read_only: bool,
    session: (usize, ObjectRef),
    cleanup: Option<(ObjectRef, usize)>,
) -> MethodCallResult {
    let base = match ctx.invoke_static(
        "jdk/internal/foreign/SegmentFactories",
        "makeNativeSegmentUnchecked",
        "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
        &[Value::Long(ptr), Value::Long(0)],
    )? {
        Some(Value::Object(Some(base))) => base,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "reinterpret: no native segment".into(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(base);
    let caller = match ctx.load_class("java/lang/foreign/MemorySegment")? {
        Some(Value::Object(Some(mirror))) => mirror,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "reinterpret: no MemorySegment class".into(),
            }
            .into())
        }
    };
    let base = ctx.read_native_pin(base_pin, base);
    let session = ctx.read_native_pin(session.0, session.1);
    let cleanup = cleanup.map(|(c, pin)| ctx.read_native_pin(pin, c));
    let seg = ctx.invoke_special_bytecode_only(
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "reinterpretInternal",
        "(Ljava/lang/Class;JLjdk/internal/foreign/MemorySessionImpl;Ljava/util/function/Consumer;)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
        &[
            Value::Object(Some(base)),
            Value::Object(Some(caller)),
            Value::Long(new_size),
            Value::Object(Some(session)),
            Value::Object(cleanup),
        ],
    )?;
    match seg {
        Some(Value::Object(Some(seg))) if read_only => ctx.invoke_virtual(
            seg,
            "asReadOnly",
            "()Ljava/lang/foreign/MemorySegment;",
            &[],
        ),
        other => Ok(other),
    }
}

/// Release every native pin from `base` up (none when `base` is `None`).
pub(in crate::panama) fn pe_release_pins(ctx: &mut dyn NativeContext, base: Option<usize>) {
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
}

pub(in crate::panama) fn register_pe2_string_marshaling_on(r: &mut NativeMethodRegistry, ms: &str) {
    // `getUtf8String(J)` and `reinterpret(J)` USED TO BE REGISTERED HERE, on
    // both class names, and both drifted: `register_p67_foreign_memory`
    // registers the same triples, and THAT pass is the one a shipping binary
    // reaches. This pass is synthetic-only, so its bodies won the
    // last-write-wins race under `--features synthetic-jdk` and were absent
    // from every other mode -- i.e. every test built that way measured code
    // that does not ship, and the code that DID ship was the weaker of the two
    // in both cases. One body each now, registered by the shipping pass:
    // `p67_segment_get_string` and `pe_segment_reinterpret` above.
    //
    // `setUtf8String` and `allocateUtf8String` below have NO shipping twin and
    // stay exactly where they are.

    // setUtf8String(long offset, String value) → void
    r.register(
        ms,
        "setUtf8String",
        "(JLjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let str_obj = obj_arg(args, 2)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();

            let ptr = crate::panama_libffi::segment_host_address(ctx, this);
            // Bounds check: string + null terminator must fit within segment.
            // A segment with size 0 has unknown bounds (e.g. created via
            // ofAddress or wrapping a raw function pointer); writing to such a
            // segment is an arbitrary-native-write primitive. The READ path
            // (getUtf8String) rejects zero-size segments, so the WRITE path must
            // be symmetric and reject them too rather than skipping the bounds
            // check and writing blindly to the raw address.
            let seg_size = match crate::panama_libffi::segment_byte_size(ctx, this) {
                n if n > 0 => n,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "setUtf8String on a segment with unknown bounds \
                                  (size 0): reinterpret the segment with a known \
                                  size before writing a C string"
                            .into(),
                    }
                    .into());
                }
            };
            let str_bytes = s.as_bytes();
            let needed = str_bytes.len() as i64 + 1; // +1 for null terminator
            if offset < 0 || offset + needed > seg_size {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "setUtf8String: offset {} + {} bytes exceeds segment size {}",
                        offset, needed, seg_size
                    ),
                }
                .into());
            }

            // Validate address arithmetic doesn't overflow
            let total = (ptr as u64).checked_add(offset as u64);
            if let Some(addr_val) = total {
                let addr = addr_val as *mut u8;
                if !addr.is_null() {
                    // SAFETY: bounds-checked against segment size, address
                    // arithmetic verified, null-checked.
                    unsafe {
                        std::ptr::copy_nonoverlapping(str_bytes.as_ptr(), addr, str_bytes.len());
                        *addr.add(str_bytes.len()) = 0; // null terminator
                    }
                }
            } else {
                return Err(RuntimeError::IllegalStateException {
                    message: "address arithmetic overflow in setUtf8String".into(),                }
                .into());            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    // Arena.allocateUtf8String(String) → MemorySegment. JDK 22 renamed this to
    // `allocateFrom`, which shares the body — see the registration in
    // `phases_late::foreign_ffm`, where the `allocateFrom` spelling was still
    // handing back a stand-in segment with address 0 and byteSize 0.
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "allocateUtf8String",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        pe_arena_allocate_from_string, NativeKind::SyntheticStub,
    );
}

// =============================================================================
// R3: Resource Loading — Class.getResourceAsStream, InputStreamReader, BufferedReader
//
// Synthetic InputStream layout (2 fields):
//   field 0: String[] ref array — all lines of the resource file
//   field 1: int — current read position (line index)
//
// InputStreamReader layout (1 field):
//   field 0: InputStream reference
//
// BufferedReader layout (1 field):
//   field 0: Reader reference (InputStreamReader)
// =============================================================================

/// Traverse BufferedReader → InputStreamReader → InputStream chain.
/// Returns the synthetic InputStream ObjectRef, or None if the chain is broken.
pub(in crate::panama) fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
    // BufferedReader.field[0] = Reader (InputStreamReader)
    let reader = match ctx.get_field(buffered_reader, 0) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    // InputStreamReader.field[0] = InputStream
    match ctx.get_field(reader, 0) {
        Value::Object(Some(is)) => Some(is),
        _ => None,
    }
}

/// `Arena.allocateFrom(String)` / `Arena.allocateUtf8String(String)`: a segment
/// holding the string's UTF-8 bytes plus a NUL terminator.
///
/// Named rather than inline so `foreign_ffm`'s `allocateFrom` registration can
/// share it — the two spellings used to be two bodies, and only one of them was
/// repaired. That spelling used to answer a `p67_arena_segment` stand-in: it
/// allocated NO memory (`set_field(segment, 1, Long(0)) // address`) and wrote
/// the SIZE into slot 0 — the inverse of the convention
/// `panama_libffi::segment_address` and `pe_arena_allocate_impl` use. So
/// `segment_address` fell through to `get_field(seg, 0)`, read `Long(5)` — the
/// byte length of `"abcd\0"` — and libffi passed 5 as the `char *`. `strlen`
/// then dereferenced address 0x5.
///
/// Measured 2026-08-12: the same object reported `byteSize() == 0` and
/// `address() == 5`, inverted on both. The downcall carrier, `invoke` dispatch,
/// CIF build and return unmarshal were all correct — a positive control
/// building the argument with `allocate(5)` plus explicit stores returns
/// `strlen(abcd) == 4`, matching HotSpot. It is the second half of residual 3
/// in `ffm-elements-spliterator-and-allocatefrom-gaps-20260813`.
///
/// A failed allocation RAISES rather than answering a null segment. The whole
/// defect above was a segment that looked allocated and was not; handing back
/// `null` here would put the same silence one call further out, where the
/// caller dereferences it in a downcall.
pub(crate) fn pe_arena_allocate_from_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let str_obj = obj_arg(args, 1)?;
    let s = ctx.read_string(str_obj).unwrap_or_default();
    let bytes = s.as_bytes();
    let size = (bytes.len() + 1) as i64; // +1 for null terminator
                                         // Round 12 wave 6 (lane ffm3): one access window over the allocation and
                                         // the write, so a shared close racing this call either refuses the
                                         // allocation or waits for the write before it frees the new block.
    let _window = crate::ffm_fast::AccessWindow::open();

    // Allocate through the SAME path as `Arena.allocate(long, long)`, so the
    // segment carries a real off-heap base in the `[0]=ptr, [1]=size` layout
    // every reader expects.
    let seg = match pe_arena_allocate_impl(ctx, this, size, 1)? {
        Some(Value::Object(Some(seg))) => seg,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Arena.allocateFrom could not allocate a segment".into(),
            }
            .into())
        }
    };
    // Write the string bytes + null terminator.
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    if ptr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena.allocateFrom segment is not writable".into(),
        }
        .into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        *(ptr as *mut u8).add(bytes.len()) = 0;
    }
    Ok(Some(Value::Object(Some(seg))))
}

