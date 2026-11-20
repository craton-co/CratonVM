#[allow(unused_imports)]
use super::*;

pub(in crate::panama) fn register_pe_memory_segment_on(r: &mut NativeMethodRegistry, ms: &str) {
    // Real-JDK callers dispatch these interface methods directly; retain the
    // concrete native bridges when SyntheticStub registrations are filtered.
        
    // byteSize() → long
    r.register(ms, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(crate::panama_libffi::segment_byte_size(
            ctx, this,
        ))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // address() → long.
    //
    // TWO DIFFERENT QUESTIONS SHARE ONE FUNCTION, and this registration asks
    // the wrong one for a heap segment. `panama_libffi::segment_address` is
    // "the machine address to dereference", and since F27 it deliberately
    // answers **0** for a heap carrier so that every raw-pointer consumer
    // refuses instead of dereferencing a `length` (the `0x10` of W7-89 §7.1).
    // `MemorySegment.address()` is the JDK's own accessor and has a defined
    // answer that is NOT always 0. Measured, 25.0.3+9-LTS:
    //
    //     ofArray(new byte[32]).address()            == 0   (offset field 16)
    //     ofArray(new byte[32]).asSlice(3).address()  == 3   (offset field 19)
    //
    // i.e. `offset - Unsafe.arrayBaseOffset`, which is exactly
    // `HeapSegmentView::start`. Answering 0 for the slice is the same class of
    // wrong answer as answering the length was — a plausible number from a
    // reader that could not decode the carrier — it is just quieter, because 0
    // is also the right answer for the un-sliced case that every existing test
    // uses.
    r.register(ms, "address", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): a 6-slot
        // native carrier -- the shape every `--compatible` arena allocation and
        // pointer return mints -- answers base + offset, which is where the
        // two arms below arrive for it (no heap view below 8 slots; the class
        // declares no `min`; not a real heap segment), after a by-name miss
        // and two class tests: ~450 ns a call, against ~3 ns on HotSpot.
        if ctx.object_num_fields(this) == 6
            && craton_segment_class_id(ctx) == Some(ctx.class_id_of_object(this))
        {
            if let (Value::Long(base), Value::Long(off)) =
                (ctx.get_field(this, 0), ctx.get_field(this, 5))
            {
                return Ok(Some(Value::Long(base.wrapping_add(off))));
            }
        }
        if let Some(view) = heap_segment_view(ctx, this) {
            return Ok(Some(Value::Long(view.start)));
        }
        Ok(Some(Value::Long(crate::panama_libffi::segment_address(
            ctx, this,
        ))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // isNative() → boolean.
    //
    // Was an unconditional `true` ("always true for our segments"), which is
    // wrong for the `ofArray(...)` segments registered further down in THIS
    // file: those are heap segments, and `isNative()` is exactly the query a
    // caller uses to decide whether `address()` is meaningful and whether the
    // segment may be handed to a downcall.
    //
    // NOTE: `foreign_ffm.rs` registers this same class+method+descriptor, and
    // this registrar runs AFTER it on both paths that reach them
    // (lib.rs:9736→9740, and :22995→23051), so THIS is the live answer — the
    // one over there was dead, and said the opposite. They now agree.
    //
    // `heap_segment_view` is the ONE discriminator: it recognizes every heap
    // carrier this crate mints or reads (a real `HeapMemorySegmentImpl$Of*`,
    // and this file's own alias carrier, `[6]=array, [7]=start`, which is what
    // `ofArray` mints for all seven primitive array types and what `asSlice`
    // normalizes every heap segment down to). Measured on the oracle:
    // `ofArray(new byte[32]).asSlice(3, 4).isNative()` is **false**.
    r.register(ms, "isNative", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let heap_backed = crate::panama_libffi::is_real_heap_segment(ctx, this)
            || heap_segment_view(ctx, this).is_some();
        Ok(Some(Value::Int(i32::from(!heap_backed))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // get(ValueLayout, long offset) → value
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        pe_segment_get, cratonvm_native_api::NativeKind::Bridge,
    );

    // set(ValueLayout, long offset, value)
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        pe_segment_set, cratonvm_native_api::NativeKind::Bridge,
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signature above.
    //
    // All nine of each. `java.lang.foreign.MemorySegment` declares nine
    // `get`/`set` pairs and every one of the eighteen is `public abstract`, so
    // a descriptor nobody registers is not a slow path — it is
    // `AbstractMethodError: … has no Code attribute`, thrown at the interface
    // method itself.
    //
    // The `set` half of this loop did not exist. Only the erased
    // `(ValueLayout;JLjava/lang/Object;)V` above was registered, and real
    // bytecode never emits that; `phases_late/foreign_ffm.rs` separately
    // covered Byte/Short/Int/Long, which is why four of the nine worked and
    // `set(JAVA_DOUBLE, …)` raised. Keeping the two lists adjacent and
    // identical is the point: an asymmetry between them is exactly the defect,
    // and it is only visible when they are read together.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "get", desc, pe_segment_get, cratonvm_native_api::NativeKind::Bridge);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "set", desc, pe_segment_set, cratonvm_native_api::NativeKind::Bridge);
    }
    // getAtIndex / setAtIndex, ERASED descriptors.
    //
    // These used to open-code the stride as `get_field(layout, 1)` matched
    // against `Value::Int` — the DELETED three-slot layout encoding, in which
    // slot 1 was `Int(byteSize)`. Since F16 consolidated on the JDK-true
    // four-slot carrier, slot 1 is `Long(byteAlignment)`, so:
    //
    //   * the `Value::Int` arm cannot match any layout this VM or the real JDK
    //     mints, so every call took the `_ => 1` default and the stride was
    //     **1 byte** — `getAtIndex(JAVA_INT, 2)` read offset 2, not 8;
    //   * and had it matched, it would have been the ALIGNMENT, which is 1 for
    //     `JAVA_INT_UNALIGNED` and 8 for `JAVA_LONG` — a different wrong number
    //     per layout.
    //
    // The covariant registrations a few lines below route to
    // `pe_segment_get_at_index`, which derives the stride from
    // `ffi::layout_byte_size(read_layout_kind(...))` and is correct. So one
    // rule had two implementations, adjacent, disagreeing — and only the
    // erased one was wrong. Route both spellings at the same function; real
    // bytecode emits the covariant descriptors, so this is reachable by
    // reflection and by `MethodHandle`, not by a `javac` call site.
    r.register(
        ms,
        "getAtIndex",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        pe_segment_get_at_index, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ms,
        "setAtIndex",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        pe_segment_set_at_index, cratonvm_native_api::NativeKind::Bridge,
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signatures above.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "getAtIndex", desc, pe_segment_get_at_index, cratonvm_native_api::NativeKind::Bridge);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "setAtIndex", desc, pe_segment_set_at_index, cratonvm_native_api::NativeKind::Bridge);
    }

    // asSlice(long offset, long size) → MemorySegment
    //
    // A NAMED FUNCTION, not an inline closure, so the heap arm below is
    // reachable from a unit test. An anonymous closure inside a registrar can
    // only be exercised by standing a whole `NativeMethodRegistry` up, which is
    // why the defect it now fixes had no test either way.
    r.register(
        ms,
        "asSlice",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        pe_segment_as_slice, cratonvm_native_api::NativeKind::Bridge,
    );

    // asSlice(long offset) → the rest of the segment.
    //
    // `MemorySegment` declares BOTH arities and both are `public abstract`, so
    // the one nobody registered was not a slow path — it was
    // `AbstractMethodError: … has no Code attribute` at the interface method.
    // It shares the two-argument body rather than restating the bounds check,
    // which is the asymmetry that produced the gap in the first place.
    r.register(
        ms,
        "asSlice",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            // JIT round 20 wave 8 (lane huntFFM): JDK 25 is `checkBounds(offset,
            // 0)` then `length - offset`, so an out-of-range offset reports
            // `new length = 0`; `size - offset` overflowed for `Long.MIN_VALUE`
            // (a debug-build panic) and named a huge length for any negative one.
            let new_size = if (0..=size).contains(&offset) {
                size - offset
            } else {
                0
            };
            pe_segment_slice(ctx, this, offset, new_size, None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // asReadOnly() → the same memory, refused for writes.
    //
    // A full-size slice with the read-only flag FORCED on rather than
    // inherited. Without it `asReadOnly()` was `AbstractMethodError`, so
    // nothing could obtain a read-only view at all — and
    // `AbstractMemorySegmentImpl.asByteBuffer()` reaches for exactly this to
    // decide whether to hand back a read-only buffer.
    //
    // The other direction is the F21 rule, and it is `pe_segment_slice`'s:
    // read-only is CONTAGIOUS, so every `asSlice` above inherits the parent's
    // flag and there is no route back to writable.
    r.register(
        ms,
        "asReadOnly",
        "()Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            pe_segment_slice(ctx, this, 0, size, Some(true))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // asByteBuffer() → a ByteBuffer over the segment's own memory: DIRECT for a
    // native segment, HEAP for an `ofArray` one. See `pe_segment_as_byte_buffer`.
    r.register(
        ms,
        "asByteBuffer",
        "()Ljava/nio/ByteBuffer;",
        pe_segment_as_byte_buffer, cratonvm_native_api::NativeKind::Bridge,
    );

    // asSlice(long offset, long size, long byteAlignment) — the alignment-
    // checked form. A third arity, therefore a third AbstractMethodError; it
    // shares the bounds-checked body above and adds the check the argument is
    // FOR, rather than accepting and ignoring it.
    r.register(
        ms,
        "asSlice",
        "(JJJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let new_size = pe_long_arg(args, 2);
            let align = pe_long_arg(args, 3);
            // Bounds, then power-of-two, then alignment — the oracle's order,
            // measured. See [`pe_slice_bounds_check`].
            pe_slice_bounds_check(ctx, this, offset, new_size)?;
            pe_slice_alignment_check(ctx, this, offset, align)?;
            pe_segment_slice(ctx, this, offset, new_size, None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // asSlice(long offset, MemoryLayout layout) — size AND ALIGNMENT taken
    // from the layout.
    //
    // The JDK's body is `asSlice(offset, layout.byteSize(),
    // layout.byteAlignment())`, so the layout's alignment is a CONSTRAINT, not
    // merely a width. Dropping it made this the one slice arity that could
    // hand back a view the oracle refuses to create:
    //
    // | call | oracle | before |
    // |---|---|---|
    // | `ofArray(byte[16]).asSlice(0, JAVA_INT)` | IAE | 4-byte slice |
    // | `ofArray(byte[16]).asSlice(0, JAVA_INT_UNALIGNED)` | 4-byte slice | 4-byte slice |
    // | `ofArray(int[8]).asSlice(2, JAVA_INT)` | IAE | 4-byte slice |
    // | `ofArray(int[8]).asSlice(4, JAVA_INT)` | 4-byte slice | 4-byte slice |
    // | `ofArray(byte[16]).asSlice(0, sequenceLayout(2, JAVA_INT))` | IAE | 8-byte slice |
    // | `ofArray(byte[16]).asSlice(0, paddingLayout(4))` | 4-byte slice | 4-byte slice |
    //
    // (measured, `FfmProbe` C16/C17 and `FfmProbe3` M3a–M3i). The width comes
    // from [`p67_layout_size_align`] rather than `pe_memory_layout_width` so
    // that the size and the alignment are read out of the SAME carrier slots
    // in one call — two readers for one four-slot object is how the two got to
    // disagree in the first place.
    r.register(
        ms,
        "asSlice",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let layout = obj_arg(args, 2)?;
            let (width, align) =
                crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
            pe_slice_bounds_check(ctx, this, offset, width)?;
            pe_slice_alignment_check(ctx, this, offset, align)?;
            pe_segment_slice(ctx, this, offset, width, None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // maxByteAlignment().
    //
    // SETTLED 2026-08-16 against the oracle; the previous body was a
    // hypothesis and both of its arms were wrong.
    //
    // * **Heap.** The answer is the BACKING ARRAY'S ELEMENT ALIGNMENT, capped
    //   by the low bit of the offset within that array — not the offset alone.
    //   Measured: `ofArray(byte[16])`→1, `short[8]`→2, `char[8]`→2,
    //   `int[8]`→4, `long[8]`→8, `float[8]`→4, `double[8]`→8, and
    //   `ofArray(byte[0])`→1, `ofArray(long[0])`→8 (an empty segment still
    //   knows its element type). The old body answered **8 for every one of
    //   them** at offset 0, and 1/2/4 by accident of the offset elsewhere:
    //   `ofArray(byte[16]).maxByteAlignment()` was 8 where the oracle says 1,
    //   which is the difference between refusing and admitting
    //   `get(JAVA_LONG, 0)` on a byte array.
    // * **Native.** A base of 0 answers **2^62**, not 8 — measured on
    //   `MemorySegment.NULL` and `ofAddress(0)`. 8 made `NULL.asSlice(0,0,16)`
    //   an `IllegalArgumentException` where HotSpot returns a segment.
    //
    // Both arms live in [`pe_segment_max_byte_alignment`], which is the same
    // reader `asSlice`'s alignment arms and the heap `get`/`set` gate go
    // through, so this method and the refusals it explains cannot drift apart.
    r.register(ms, "maxByteAlignment", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(pe_segment_max_byte_alignment(ctx, this))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // heapBase() — present only for a heap segment, whose Java array this
    // answers; a native segment has no heap base.
    //
    // Asked through `heap_segment_view`, which is the same reader `isNative()`
    // and `address()` above use, so the three cannot disagree. The plain
    // `SEG_BACKING_ARRAY_FIELD` read this used to do knew only ONE of the three
    // heap carriers — the `ofArray` off-heap mirror — and answered empty for a
    // real `HeapMemorySegmentImpl$Of*` and for this file's own alias carrier
    // (`[6]=array, [7]=start`), the two that `isNative()` already reports as
    // non-native. The mirror is kept as the fallback because its array lives in
    // slot 2 and `heap_segment_view` does not claim it.
    // A READ-ONLY SEGMENT HAS NO `heapBase`. MEASURED, and it is a
    // CAPABILITY, not a formatting detail.
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[16]).heapBase().isPresent()` | `true` |
    // | `ofArray(byte[16]).asReadOnly().heapBase().isPresent()` | **`false`** |
    // | `asReadOnly().asSlice(3,4).heapBase().isPresent()` | `false` |
    // | `asReadOnly().elements(JAVA_BYTE).findFirst().get().heapBase().isPresent()` | `false` |
    // | `ofBuffer(ByteBuffer.allocate(8).asReadOnlyBuffer()).heapBase().isPresent()` | `false` |
    // | native `allocate(8).asReadOnly().heapBase().isPresent()` | `false` |
    //
    // (`FfmProbe` B12, `FfmProbe3` M4a–M4k.) The array `heapBase()` hands back
    // is the segment's own storage and is fully writable through plain array
    // stores, so returning it from a read-only view gives away exactly the
    // capability `asReadOnly()` was called to remove — F26's "a wrong
    // capability" and F21's "read-only is contagious", one call apart. Reads
    // through the segment still work (`asReadOnly().toArray(JAVA_BYTE)` is
    // 16 elements on the oracle); it is only the escape hatch that closes.
    r.register(ms, "heapBase", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let base = pe_segment_heap_base(ctx, this);
        ctx.invoke(
            "java/util/Optional",
            "ofNullable",
            "(Ljava/lang/Object;)Ljava/util/Optional;",
            &[base],
        )
    }, cratonvm_native_api::NativeKind::Bridge);

    // isAccessibleBy(Thread) — delegate to the SESSION rather than re-deriving
    // confinement here. The owner slot belongs to `foreign_ffm`'s session model,
    // and a second copy of that index is exactly the drift this file's
    // scope-resolution comment warns about.
    r.register(
        ms,
        "isAccessibleBy",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let thread = args.get(1).copied().unwrap_or(Value::Object(None));
            match pe_segment_session(ctx, this) {
                Some(session) => ctx.invoke_virtual(
                    session,
                    "isAccessibleBy",
                    "(Ljava/lang/Thread;)Z",
                    &[thread],
                ),
                // No modelled session means unconfined, which every thread may reach.
                None => Ok(Some(Value::Int(1))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // isLoaded / load / unload / force — specified to throw for a segment that
    // did not come from `FileChannel.map`, which is every segment this VM mints.
    // Registered rather than left absent so the caller sees the JDK's own
    // exception (message included, measured) instead of an AbstractMethodError
    // that names dispatch.
    r.register(ms, "isLoaded", "()Z", |_ctx, _args| {
        Err(RuntimeError::UnsupportedOperationException {
            message: "Not a mapped segment".to_string(),
        }
        .into())
    }, cratonvm_native_api::NativeKind::Bridge);
    for method in ["load", "unload", "force"] {
        r.register(ms, method, "()V", |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "Not a mapped segment".to_string(),
            }
            .into())
        }, cratonvm_native_api::NativeKind::Bridge);
    }

    // ofAddress(long address) → MemorySegment (wraps a raw address, zero-length)
    r.register(
        ms,
        "ofAddress",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let addr = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            // `ofAddress` is NOT restricted, and the JDK does not gate it —
            // measured on Adoptium 25.0.4, it answers a segment even under
            // `--illegal-native-access=deny`, which is the strictest setting
            // the launcher has:
            //
            //     java                                 ofAddress -> OK, byteSize=0
            //     java --illegal-native-access=deny    ofAddress -> OK, byteSize=0
            //
            // The safety is structural rather than a flag: the segment it
            // returns has length ZERO, so every access through it is an
            // IndexOutOfBoundsException. Widening it needs `reinterpret`, and
            // THAT is the restricted call — which is where the check now lives.
            // The refusal that used to sit here had no counterpart in the JDK
            // at any setting, and the `addr != 0` carve-out it needed for
            // `MemorySegment.NULL`'s own <clinit> was the tell.
            // G19-1 §2.2: A RAW-ADDRESS SEGMENT IS IN THE GLOBAL SCOPE.
            //
            // MEASURED on 25.0.3+9-LTS: `MemorySegment.ofAddress(16).scope()
            // == Arena.global().scope()` is **true**, and it was false here —
            // slot 2 was the "no arena" marker, so `p67_receiver_session`
            // minted a fresh session on every `scope()` call and no two reads
            // of the same segment's scope agreed with each other either.
            // A raw address has no owner to close it, so the global arena is
            // not a placeholder here: it is the answer.
            let arena = crate::phases_late::foreign_ffm::p67_global_arena(ctx)?;
            let arena_pin = ctx.pin_native_root(arena);
            let seg = alloc_segment_carrier(ctx, 6);
            let arena = ctx.read_native_pin(arena_pin, arena);
            ctx.unpin_native_roots(arena_pin);
            // Round 14 wave 3 (lane ffm): after the unpin (a failed
            // allocation returned with the pin taken).
            let seg = seg?;
            // The global arena's session, not the arena: see `pe_carrier_scope`.
            let scope = pe_carrier_scope(ctx, arena);
            ctx.set_field(seg, 0, Value::Long(addr));
            ctx.set_field(seg, 1, Value::Long(0)); // unknown size
            ctx.set_field(seg, 2, Value::Object(Some(scope)));
            ctx.set_field(seg, 3, Value::Int(0));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // allocateFrom(Arena, ValueLayout, value) → MemorySegment
    // Allocates a segment for a single value and writes the value into it.
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;I)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 4,
            };
            // Round 12 wave 6 (lane ffm3): the allocation can collect and move
            // the layout, which the write below reads: pinned across it.
            let layout_pin = ctx.pin_native_root(layout);
            let allocated = pe_arena_allocate_impl(ctx, arena, size, size);
            let layout = ctx.read_native_pin(layout_pin, layout);
            ctx.unpin_native_roots(layout_pin);
            let seg = allocated?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Int(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // allocateFrom(Arena, ValueLayout.OfLong, long) → MemorySegment
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 8,
            };
            // Round 12 wave 6 (lane ffm3): the allocation can collect and move
            // the layout, which the write below reads: pinned across it.
            let layout_pin = ctx.pin_native_root(layout);
            let allocated = pe_arena_allocate_impl(ctx, arena, size, size);
            let layout = ctx.read_native_pin(layout_pin, layout);
            ctx.unpin_native_roots(layout_pin);
            let seg = allocated?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Long(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ofArray(byte[] | short[] | char[] | int[] | long[] | float[] | double[])
    // → MemorySegment ALIASING the array, for all seven.
    //
    // `java.lang.foreign.MemorySegment` declares SEVEN `ofArray` overloads
    // (`javap`, 25.0.3+9-LTS: `byte[] char[] short[] int[] float[] long[]
    // double[]` — and NO `boolean[]`). `ofArray` is on
    // `native_override.rs`'s force-route NAME list, but a forced name with no
    // registration for that descriptor falls back to real bytecode, so a
    // missing arm produced a real `HeapMemorySegmentImpl$Of*` under
    // `--real-jdk` — and until F27 this file read one's `length` as its
    // address and the VM died with `EXCEPTION_ACCESS_VIOLATION … at address
    // 0x10` (W7-89 §7.1). In synthetic-JDK mode there is no real bytecode to
    // fall back to at all.
    //
    // UNTIL 2026-09-21, int[]/long[]/float[]/double[] used a SEPARATE
    // registration (`alloc_mirror_carrier`) that copied the array into an
    // off-heap native buffer, so those four could be handed straight to a
    // downcall (`sync_heap_backed_segment` re-synced the copy around the
    // call) — CratonVM cannot hand a moving Java array to native code, and
    // Elasticsearch's bulk-vector downcalls depend on passing one of these
    // four directly. Measured, the oracle does not permit that at all:
    //
    //     strlen(MemorySegment.ofArray("hi\0".getBytes()))
    //       -> IllegalArgumentException: Heap segment not allowed: MemorySegment{ kind: heap, … }
    //     strlen(MemorySegment.ofArray(new int[]{1,2,3}))
    //       -> IllegalArgumentException: Heap segment not allowed: MemorySegment{ kind: heap, … }
    //
    // and it DOES require aliasing, for every one of the seven:
    //
    //     byte[] a = new byte[8];
    //     MemorySegment.ofArray(a).set(JAVA_INT_UNALIGNED, 0, 0x01020304);
    //     // a == [4, 3, 2, 1, 0, 0, 0, 0]
    //     MemorySegment.ofArray(new int[2]).address() == 0L
    //       // true on the oracle for EVERY ofArray segment, numeric or not
    //
    // So the mirror was a wrong capability in both directions for the four
    // numeric types too — not just the accessor gap G6-1 found and fixed
    // (`heap_segment_view`'s former H3 arm), but the identity and address()
    // semantics themselves — and it leaked its native buffer on every call
    // (`allocate_native_memory` with no matching `free_native_memory`; see
    // the retired G6-1 follow-up). All seven now go through
    // [`pe_of_array_alias`], the SAME carrier `asSlice` already normalizes
    // every heap segment down to regardless of source. The element width
    // comes from the ARRAY, through the same `heap_element_width` the
    // accessors use, so the stride cannot drift between the factory and the
    // reader.
    //
    // Elasticsearch's downcall dependency is preserved, but moved to where it
    // belongs: `pe_downcall_invoke` now stands up a SCRATCH native buffer for
    // exactly the duration of one call when an int[]/long[]/float[]/double[]
    // alias segment is passed as a downcall pointer argument — copied in
    // before the call, copied back and freed right after — rather than
    // baking a permanent native mirror into the segment `ofArray` returns.
    // byte[]/short[]/char[] get no such carve-out, matching the oracle's
    // blanket refusal for those three (unchanged from before). j27 wave 4
    // (lane FFMFLIP): the four numeric types are refused too now, as on the
    // oracle; `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT=1` restores
    // the carve-out.
    for descriptor in [
        "([B)Ljava/lang/foreign/MemorySegment;",
        "([S)Ljava/lang/foreign/MemorySegment;",
        "([C)Ljava/lang/foreign/MemorySegment;",
        "([I)Ljava/lang/foreign/MemorySegment;",
        "([J)Ljava/lang/foreign/MemorySegment;",
        "([F)Ljava/lang/foreign/MemorySegment;",
        "([D)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "ofArray", descriptor, pe_of_array_alias, cratonvm_native_api::NativeKind::Bridge);
    }
    // `SegmentFactories.fromArray([F)` is the real-bytecode internal factory
    // `ofArray(float[])` itself dispatches through on some call paths; see
    // the historical note this carried before unification (2026-09-20,
    // still-open F27 residual for the other five primitives, resolved by the
    // loop above covering `ofArray` directly).
    r.register(
        "jdk/internal/foreign/SegmentFactories",
        "fromArray",
        "([F)Ljdk/internal/foreign/HeapMemorySegmentImpl$OfFloat;",
        pe_of_array_alias, cratonvm_native_api::NativeKind::Bridge,
    );

    // Copy primitive array elements into a native segment. Elasticsearch uses
    // this overload to stage float[] rows for bulk vector kernels.
    r.register(
        ms,
        "copy",
        "(Ljava/lang/Object;ILjava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JI)V",
        |ctx, args| {
            require_segment_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            // Round 12 wave 6 (lane ffm3): a negative index or count is the
            // JDK's `IndexOutOfBoundsException`; both used to read as 0 and
            // the copy quietly did nothing.
            if ffm_bulk_heap_segments()
                && [args.get(1), args.get(5)]
                    .iter()
                    .any(|arg| matches!(arg, Some(Value::Int(v)) if *v < 0))
            {
                // `AbstractMemorySegmentImpl.copy`'s `Objects.checkFromIndexSize(
                // srcIndex, elementCount, Array.getLength(srcArray))` text
                // (wave 100 L7; this said "negative array index or element
                // count").
                return Err(panama_copy_range_error(ctx, src, args));
            }
            let src_index = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let layout = obj_arg(args, 3)?;
            let dst_offset = match args.get(4) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(5) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let length = ctx.array_length(src);
            if src_index
                .checked_add(count)
                .map_or(true, |end| end > length)
            {
                // The same check's IOOBE, never an AIOOBE "Array index out of
                // range: n" (wave 100 L7).
                return Err(panama_copy_range_error(ctx, src, args));
            }
            let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
            let width = ffi::layout_byte_size(kind) as i64;
            for i in 0..count {
                let value = ctx.get_array_element(src, src_index + i);
                // Primitive float arrays use their raw IEEE-754 bits in the
                // interpreter's array representation. The set helper expects
                // the typed Value::Float form; otherwise its type switch
                // silently falls through and leaves the destination zeroed.
                let value = match (kind, value) {
                    (LAYOUT_FLOAT, Value::Int(bits)) => Value::Float(f32::from_bits(bits as u32)),
                    (_, value) => value,
                };
                pe_segment_set_impl(ctx, dst, layout, dst_offset + (i as i64) * width, value)?;
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // copy(src, srcOffset, dst, dstOffset, bytes) — memcpy
    r.register(
        ms,
        "copy",
        "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        |ctx, args| {
            // Defense-in-depth: copy dereferences both segments' raw `ptr` fields.
            require_segment_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            let src_offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let dst_offset = match args.get(3) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let bytes_arg = match args.get(4) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            // Round 12 wave 6 (lane ffm3): a negative length is the JDK's
            // bounds failure (`checkFromIndexSize`), not a huge `usize` that
            // then tripped the size cap's `IllegalStateException`.
            if bytes_arg < 0 && ffm_bulk_heap_segments() {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!("negative copy length {bytes_arg}")),
                }
                .into());
            }
            let bytes = bytes_arg as usize;

            // Round 12 wave 5 (lane ffm2): this copy never checked either
            // segment's scope or the destination's read-only flag, so it read
            // and wrote the blocks of a CLOSED arena (`R12Hunt2FfmLayouts`
            // `closed-ise`; a use-after-free once close frees) and wrote
            // through `asReadOnly()` views. The JDK refuses a read-only
            // destination (`checkAccess(.., false)`) and then either closed
            // session (`ScopedMemoryAccess.copyMemory`). The access window is
            // held until the last byte is copied.
            let _window = crate::ffm_fast::AccessWindow::open();
            if ffm_bulk_scope_check() {
                craton_segment_check_read_only(ctx, dst, false)?;
                pe_segment_check_scope(ctx, src)?;
                pe_segment_check_scope(ctx, dst)?;
            }

            // The HOST base: a segment over a direct buffer carries the
            // buffer's `Unsafe` arena handle, which is not dereferenceable.
            let src_ptr = crate::panama_libffi::segment_host_address(ctx, src);
            let dst_ptr = crate::panama_libffi::segment_host_address(ctx, dst);

            // Validate offsets against segment sizes to prevent out-of-bounds access
            let src_size = crate::panama_libffi::segment_byte_size(ctx, src);
            let dst_size = crate::panama_libffi::segment_byte_size(ctx, dst);

            if bytes > 0 {
                if bytes > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "copy size {} exceeds maximum of {} bytes",
                            bytes, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // Bounds check: offset + bytes must fit within segment size.
                // FIX: validate UNCONDITIONALLY (not gated on size > 0). A segment
                // with a declared byteSize() of 0 must still reject a non-zero
                // `bytes` copy — otherwise a 0-size segment drives an OOB
                // read/write of up to MAX_COPY_SIZE. This mirrors the correct
                // single-element path (`pe_segment_access_addr`), which rejects
                // any non-zero access on a zero-size segment. Uses checked_add so
                // a malicious offset cannot wrap past the size comparison.
                //
                // `IndexOutOfBoundsException`, not `IllegalStateException`:
                // the JDK's `checkAccess` -> `checkBounds` raises IOOBE for a
                // copy that does not fit, as every element access here does
                // (round 12 wave 5, lane ffm2).
                let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if src_offset < 0 || src_end > src_size {
                    return Err(RuntimeError::IndexOutOfBoundsException {
                        message: Some(format!(
                            "source offset {} + {} bytes exceeds segment size {}",
                            src_offset, bytes, src_size
                        )),
                    }
                    .into());
                }
                let dst_end = dst_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if dst_offset < 0 || dst_end > dst_size {
                    return Err(RuntimeError::IndexOutOfBoundsException {
                        message: Some(format!(
                            "destination offset {} + {} bytes exceeds segment size {}",
                            dst_offset, bytes, dst_size
                        )),
                    }
                    .into());
                }

                // A heap segment has no address at all, so the pointer
                // path below cannot express it: `segment_address` answers
                // 0 and the `is_null` guard then skipped the copy without
                // a word — `MemorySegment.copy` into an `ofArray(int[])`
                // destination wrote NOTHING and reported success. Route
                // any side that is heap-backed through the array itself.
                let src_heap = heap_segment_view(ctx, src);
                let dst_heap = heap_segment_view(ctx, dst);
                if src_heap.is_some() || dst_heap.is_some() {
                    let staged = match &src_heap {
                        Some(view) => heap_read_bytes(ctx, view, src_offset, bytes),
                        None => {
                            let addr = (src_ptr as u64).checked_add(src_offset as u64);
                            addr.filter(|a| *a != 0).map(|a| {
                                let mut buf = vec![0u8; bytes];
                                // SAFETY: the source range was bounds-checked
                                // against the segment size above, and the
                                // destination is a fresh owned buffer.
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        a as *const u8,
                                        buf.as_mut_ptr(),
                                        bytes,
                                    )
                                };
                                buf
                            })
                        }
                    };
                    let Some(staged) = staged else {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MemorySegment.copy: source is neither addressable nor \
                                      array-backed"
                                .into(),
                        }
                        .into());
                    };
                    let wrote = match &dst_heap {
                        Some(view) => heap_write_bytes(ctx, view, dst_offset, &staged),
                        None => {
                            let addr = (dst_ptr as u64).checked_add(dst_offset as u64);
                            match addr.filter(|a| *a != 0) {
                                Some(a) => {
                                    // SAFETY: bounds-checked above; `staged`
                                    // is exactly `bytes` long.
                                    unsafe { std::ptr::copy(staged.as_ptr(), a as *mut u8, bytes) };
                                    true
                                }
                                None => false,
                            }
                        }
                    };
                    if !wrote {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MemorySegment.copy: destination is read-only, not \
                                      addressable, or not array-backed"
                                .into(),
                        }
                        .into());
                    }
                    return Ok(None);
                }

                // Validate address arithmetic doesn't overflow
                let src_total = (src_ptr as u64).checked_add(src_offset as u64);
                let dst_total = (dst_ptr as u64).checked_add(dst_offset as u64);

                if let (Some(s), Some(d)) = (src_total, dst_total) {
                    let src_addr = s as *const u8;
                    let dst_addr = d as *mut u8;
                    if !src_addr.is_null() && !dst_addr.is_null() {
                        // The JDK's MemorySegment.copy is defined for overlapping
                        // src/dst (it is specified as a memmove-equivalent bulk
                        // copy). Decide between memmove and the faster
                        // `copy_nonoverlapping` by testing whether the two
                        // byte ranges actually intersect.
                        //
                        // Ranges are `[s, s+bytes)` and `[d, d+bytes)` over the
                        // *absolute* addresses computed above. They overlap iff
                        // `s < d+bytes && d < s+bytes`. `bytes` is bounded by
                        // MAX_COPY_SIZE and both endpoints derive from the
                        // checked address arithmetic, so the `+ bytes` cannot
                        // wrap a u64.
                        let bytes_u64 = bytes as u64;
                        let overlap =
                            s < d.saturating_add(bytes_u64) && d < s.saturating_add(bytes_u64);
                        // SAFETY: addresses are non-null, bounds-checked against
                        // segment sizes, and bytes is bounded by MAX_COPY_SIZE.
                        // Overlapping ranges use `copy` (memmove), which is
                        // defined for overlap; provably-disjoint ranges use the
                        // faster `copy_nonoverlapping`.
                        if overlap {
                            unsafe { std::ptr::copy(src_addr, dst_addr, bytes) };
                        } else {
                            unsafe { std::ptr::copy_nonoverlapping(src_addr, dst_addr, bytes) };
                        }
                    }
                } else {
                    return Err(RuntimeError::IllegalStateException {
                        message: "address arithmetic overflow in MemorySegment.copy".into(),
                    }
                    .into());
                }
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ---- the rest of the surface `java.lang.foreign` declares -------------
    //
    // Every method below is `public abstract` on the `MemorySegment`
    // interface, so one that nobody registers is not a slow path — it is
    // `AbstractMethodError: … has no Code attribute` at the call site, an
    // error that names dispatch rather than the missing feature. They were
    // found by running the whole public surface one call at a time against
    // both VMs (`FfmAudit`), which is the only way to find this shape: each
    // one otherwise surfaces a single application at a time.

    // copyFrom(src) — bulk copy, sizes must match exactly.
    r.register(
        ms,
        "copyFrom",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let src = obj_arg(args, 1)?;
            // Round 12 wave 5 (lane ffm2): held from before the scope checks
            // to the last byte copied (see `ffm_fast::AccessWindow`), and the
            // destination's read-only flag is honoured as `copy`'s is.
            let _window = crate::ffm_fast::AccessWindow::open();
            if ffm_bulk_scope_check() {
                craton_segment_check_read_only(ctx, this, false)?;
            }
            pe_segment_check_scope(ctx, this)?;
            pe_segment_check_scope(ctx, src)?;
            let dst_size = crate::panama_libffi::segment_byte_size(ctx, this);
            let src_size = crate::panama_libffi::segment_byte_size(ctx, src);
            // The JDK's `copyFrom(src)` is `MemorySegment.copy(src, 0, this,
            // 0, src.byteSize())`: only a destination SMALLER than the source
            // is out of bounds (round 12 wave 6, lane ffm3; it used to demand
            // equal sizes).
            let heap_aware = ffm_bulk_heap_segments();
            if src_size > dst_size || (!heap_aware && src_size != dst_size) {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!(
                        "Cannot copy {} bytes into a {}-byte segment",
                        src_size, dst_size
                    )),
                }
                .into());
            }
            let n = usize::try_from(src_size).unwrap_or(0);
            if n > MAX_COPY_SIZE {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("copy size {} exceeds maximum of {} bytes", n, MAX_COPY_SIZE),
                }
                .into());
            }
            // A heap segment has no address, and the pointer path below
            // copied NOTHING for one and returned normally. Staged through
            // the backing array(s) instead, as `copy` does.
            if heap_aware && n > 0 && pe_any_heap_segment(ctx, this, src) {
                let Some(staged) = pe_segment_read_bytes(ctx, src, 0, n) else {
                    return Err(RuntimeError::IllegalStateException {
                        message: "MemorySegment.copyFrom: source is neither addressable nor \
                                  array-backed"
                            .into(),
                    }
                    .into());
                };
                if !pe_segment_write_bytes(ctx, this, 0, &staged) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "MemorySegment.copyFrom: destination is read-only, not \
                                  addressable, or not array-backed"
                            .into(),
                    }
                    .into());
                }
                return Ok(Some(Value::Object(Some(this))));
            }
            let dst_addr = crate::panama_libffi::segment_host_address(ctx, this) as *mut u8;
            let src_addr = crate::panama_libffi::segment_host_address(ctx, src) as *const u8;
            if n > 0 && !dst_addr.is_null() && !src_addr.is_null() {
                // SAFETY: both addresses come from JVM-managed segments whose
                // sizes were just checked equal to `n`, and the two blocks may
                // overlap (`copyFrom` does not forbid it), hence `copy` and not
                // `copy_nonoverlapping`.
                unsafe { std::ptr::copy(src_addr, dst_addr, n) };
            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // mismatch(other) — index of the first differing byte, or -1.
    r.register(
        ms,
        "mismatch",
        "(Ljava/lang/foreign/MemorySegment;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            // Held to the last byte compared: see `ffm_fast::AccessWindow`.
            let _window = crate::ffm_fast::AccessWindow::open();
            pe_segment_check_scope(ctx, this)?;
            pe_segment_check_scope(ctx, other)?;
            let a_len = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            let b_len = crate::panama_libffi::segment_byte_size(ctx, other).max(0);
            let common = a_len.min(b_len);
            // Round 12 wave 6 (lane ffm3): a heap segment has no address, and
            // the length rule below answered -1 for two same-sized heap
            // segments whatever their contents. Compare the bytes.
            if ffm_bulk_heap_segments() && pe_any_heap_segment(ctx, this, other) {
                let n = usize::try_from(common).unwrap_or(0).min(MAX_COPY_SIZE);
                if let (Some(a_bytes), Some(b_bytes)) = (
                    pe_segment_read_bytes(ctx, this, 0, n),
                    pe_segment_read_bytes(ctx, other, 0, n),
                ) {
                    return Ok(Some(Value::Long(pe_first_mismatch(
                        &a_bytes, &b_bytes, a_len, b_len, common,
                    ))));
                }
            }
            let a = crate::panama_libffi::segment_host_address(ctx, this) as *const u8;
            let b = crate::panama_libffi::segment_host_address(ctx, other) as *const u8;
            if a.is_null() || b.is_null() {
                // Nothing to compare through; fall back to the length rule.
                return Ok(Some(Value::Long(if a_len == b_len { -1 } else { common })));
            }
            let n = usize::try_from(common).unwrap_or(0).min(MAX_COPY_SIZE);
            // SAFETY: both pointers are segment bases and `n` is bounded by the
            // shorter of the two segment sizes.
            let (a_bytes, b_bytes) = unsafe {
                (
                    std::slice::from_raw_parts(a, n),
                    std::slice::from_raw_parts(b, n),
                )
            };
            let index = a_bytes
                .iter()
                .zip(b_bytes.iter())
                .position(|(x, y)| x != y)
                .map(|i| i as i64);
            // The JDK's contract: the first differing byte; else the common
            // length when one segment is a prefix of the other; else -1.
            Ok(Some(Value::Long(match index {
                Some(i) => i,
                None if a_len == b_len => -1,
                None => common,
            })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getString(offset[, charset]) — a NUL-terminated string, read in place.
    for desc in [
        "(J)Ljava/lang/String;",
        "(JLjava/nio/charset/Charset;)Ljava/lang/String;",
    ] {
        r.register(ms, "getString", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The block is read inside an access window (round 12 wave 5,
            // lane ffm2: see `ffm_fast::AccessWindow`) and copied out before
            // the window closes, so the string allocation below runs outside it.
            let window = crate::ffm_fast::AccessWindow::open();
            pe_segment_check_scope(ctx, this)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            if offset < 0 || offset > size {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!("offset {} out of bounds for size {}", offset, size)),
                }
                .into());
            }
            let avail = usize::try_from(size - offset)
                .unwrap_or(0)
                .min(MAX_COPY_SIZE);
            // A segment over an `Arena` block whose declared size runs past the
            // block (the unbounded C-pointer view a downcall returns into it,
            // `byteSize == Long.MAX_VALUE`) is read as far as the block goes:
            // `host_base_address` refuses the whole declared size and answered
            // `null` (`R18FfmretShapes` `strchr-cptr-read`; HotSpot `ello`).
            let raw = crate::panama_libffi::segment_address(ctx, this);
            if crate::panama_libffi::is_arena_handle(raw) && avail > 0 {
                let at = raw.wrapping_add(offset);
                if let Some((ptr, remaining)) = crate::unsafe_natives_ext::unsafe_arena_real_ptr(at)
                {
                    let len = avail.min(remaining);
                    // SAFETY: `ptr` is the live block's storage at `at`, with
                    // `remaining` bytes to the block's end; `len <= remaining`.
                    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
                    let end = bytes.iter().position(|b| *b == 0).unwrap_or(len);
                    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
                    crate::unsafe_natives_ext::unsafe_arena_release_translation(at);
                    drop(window);
                    // A fresh `String`, as `MemorySegment.getString` returns.
                    let s = ctx.create_string_uninterned(&text);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            let base = crate::panama_libffi::segment_host_address(ctx, this) as *const u8;
            if base.is_null() {
                // Round 12 wave 6 (lane ffm3): a heap segment decodes its
                // backing array's bytes, as HotSpot does; it answered null.
                if ffm_bulk_heap_segments() && heap_segment_view(ctx, this).is_some() {
                    if let Some(bytes) = pe_segment_read_bytes(ctx, this, offset, avail) {
                        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                        let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
                        drop(window);
                        let s = ctx.create_string(&text);
                        return Ok(Some(Value::Object(Some(s))));
                    }
                }
                return Ok(Some(Value::Object(None)));
            }
            // SAFETY: `offset` is within the segment and `avail` is the
            // remaining length from there.
            let bytes = unsafe { std::slice::from_raw_parts(base.add(offset as usize), avail) };
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(avail);
            // The charset overload is accepted and read as UTF-8: that is the
            // only decoder available here, and it is the default the no-charset
            // form uses. Non-UTF-8 bytes are replaced rather than refused,
            // matching `String::from_utf8_lossy` — recorded rather than silent.
            let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
            drop(window);
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge);
    }

    // setString(offset, value[, charset]) — write the bytes plus a NUL.
    for desc in [
        "(JLjava/lang/String;)V",
        "(JLjava/lang/String;Ljava/nio/charset/Charset;)V",
    ] {
        r.register(ms, "setString", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Round 12 wave 5 (lane ffm2): an access window to the last store,
            // and a read-only view refuses the write as `set` does.
            let _window = crate::ffm_fast::AccessWindow::open();
            if ffm_bulk_scope_check() {
                craton_segment_check_read_only(ctx, this, false)?;
            }
            pe_segment_check_scope(ctx, this)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let text = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            let needed = text.len() as i64 + 1;
            if offset < 0 || offset.saturating_add(needed) > size {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!(
                        "writing {} bytes at offset {} exceeds segment size {}",
                        needed, offset, size
                    )),
                }
                .into());
            }
            let base = crate::panama_libffi::segment_host_address(ctx, this) as *mut u8;
            if base.is_null() {
                // Round 12 wave 6 (lane ffm3): a heap segment is written
                // through its backing array; this was a silent no-op.
                if ffm_bulk_heap_segments() && heap_segment_view(ctx, this).is_some() {
                    let mut bytes = Vec::with_capacity(text.len() + 1);
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.push(0);
                    if !pe_segment_write_bytes(ctx, this, offset, &bytes) {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MemorySegment.setString: destination is read-only or \
                                      not array-backed"
                                .into(),
                        }
                        .into());
                    }
                }
                return Ok(None);
            }
            // SAFETY: the bounds check above guarantees `offset + text.len() + 1`
            // bytes are inside the segment.
            unsafe {
                let dst = base.add(offset as usize);
                std::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
                dst.add(text.len()).write(0);
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
    }

    // asOverlappingSlice(other) — the shared region, as an Optional.
    r.register(
        ms,
        "asOverlappingSlice",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let that = obj_arg(args, 1)?;
            // Round 12 wave 6 (lane ffm3): every heap segment's address is 0,
            // so the address ranges below said two disjoint arrays overlapped
            // and misplaced the overlap of one array's slices. The JDK
            // compares heap segments by backing array and byte range, and a
            // heap segment never overlaps a native one.
            if ffm_bulk_heap_segments() {
                let a_heap = heap_segment_view(ctx, this);
                let b_heap = heap_segment_view(ctx, that);
                if a_heap.is_some() || b_heap.is_some() {
                    let value = match (a_heap, b_heap) {
                        (Some(a), Some(b)) if a.base == b.base => {
                            let lo = a.start.max(b.start);
                            let hi = a
                                .start
                                .saturating_add(a.size)
                                .min(b.start.saturating_add(b.size));
                            if lo < hi {
                                pe_segment_slice(ctx, this, lo - a.start, hi - lo, None)?
                                    .unwrap_or(Value::Object(None))
                            } else {
                                Value::Object(None)
                            }
                        }
                        _ => Value::Object(None),
                    };
                    let opt = crate::phases_late::foreign_ffm::p67_optional(ctx, value)?;
                    return Ok(Some(Value::Object(Some(opt))));
                }
            }
            let a_start = crate::panama_libffi::segment_address(ctx, this);
            let a_end = a_start.saturating_add(crate::panama_libffi::segment_byte_size(ctx, this));
            let b_start = crate::panama_libffi::segment_address(ctx, that);
            let b_end = b_start.saturating_add(crate::panama_libffi::segment_byte_size(ctx, that));
            let lo = a_start.max(b_start);
            let hi = a_end.min(b_end);
            let empty = lo >= hi;
            let value = if empty {
                Value::Object(None)
            } else {
                pe_segment_slice(ctx, this, lo - a_start, hi - lo, None)?
                    .unwrap_or(Value::Object(None))
            };
            let opt = crate::phases_late::foreign_ffm::p67_optional(ctx, value)?;
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // toArray(elementLayout) — copy the whole segment out into a Java array.
    //
    // Eight overloads, one per primitive layout, each with its own array return
    // type. They share one body because `register` takes a plain fn pointer —
    // a per-descriptor closure could not capture its element width — so the
    // width and the array kind come from the layout argument, exactly as
    // `get`/`set` take theirs.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;)[Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;)[B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;)[C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;)[S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;)[I",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;)[F",
        "(Ljava/lang/foreign/ValueLayout$OfLong;)[J",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;)[D",
    ] {
        r.register(ms, "toArray", desc, pe_segment_to_array, cratonvm_native_api::NativeKind::Bridge);
    }

    // allocateFrom(elementLayout, values...) — the seven array overloads.
    //
    // These are `default` methods on `SegmentAllocator`, so real JDK bytecode
    // runs for them, and it ends in
    // `((AbstractMemorySegmentImpl) segment).copyFrom(...)` — a cast that can
    // never succeed while CratonVM fabricates segments as instances of the
    // `MemorySegment` INTERFACE. The failure was therefore not "no Code
    // attribute" like its neighbours but a `ClassCastException` from inside the
    // JDK, which is why it survived the interface audit that found the rest.
    // A native for each descriptor keeps that bytecode from running at all.
    for allocator in [
        "java/lang/foreign/Arena",
        // An arena is minted as the real `ArenaImpl` since 2026-08-29, and
        // native dispatch is keyed on the receiver's CLASS. Without this row
        // the JDK default above runs and `RJdkForeign`'s downcall SIGSEGVs in
        // `heap_read_bytes` on the segment it builds.
        "jdk/internal/foreign/ArenaImpl",
        "java/lang/foreign/SegmentAllocator",
    ] {
        // Seven, not eight: `SegmentAllocator` declares no `boolean...`
        // overload. Registering a descriptor the JDK does not declare would be
        // dead weight that reads like coverage.
        for desc in [
            "(Ljava/lang/foreign/ValueLayout$OfByte;[B)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfChar;[C)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfShort;[S)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfInt;[I)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfFloat;[F)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfLong;[J)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfDouble;[D)Ljava/lang/foreign/MemorySegment;",
        ] {
            r.register(allocator, "allocateFrom", desc, pe_allocate_from_array, cratonvm_native_api::NativeKind::Bridge);
        }
    }

    // spliterator(elementLayout) / elements(elementLayout).
    //
    // The last two `MemorySegment` methods that answered
    // `AbstractMethodError`. They are implemented LAZILY — see
    // `pe_segment_spliterator` for why a materialised list of slices would be
    // wrong for exactly the case these methods exist for.
    r.register(
        ms,
        "spliterator",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/util/Spliterator;",
        pe_segment_spliterator, cratonvm_native_api::NativeKind::Bridge,
    );
    // `--real-jdk` hands out the JDK's own stream over the splitter (SBR w5
    // FJP2): the synthetic `Stream` carrier below is the `cratonvm/stream/LazyOp`
    // model, whose every intermediate op is a refused class there (measured:
    // `RJdkFfmSegment` §C's `elements(..).skip(1)`, the last LazyOp refusal in
    // the suite). `--compatible` keeps the carrier byte for byte. The mode is
    // read at registration, like `lang_system`'s `Runtime.load*` pair.
    r.register(
        ms,
        "elements",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/util/stream/Stream;",
        if r.compatibility_mode().is_jdk_only() {
            pe_segment_elements_jdk_stream
        } else {
            pe_segment_elements
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // The splitter's own surface. Registered on ITS class, not on
    // `java/util/Spliterator`: that interface already carries the
    // array-backed collections implementation (field 0 = `Object[]`, 1 =
    // cursor, 2 = fence), registered later than this file runs, and a second
    // registration of the same triple would silently take those calls over.
    // A distinct receiver class keeps the two implementations apart with no
    // ordering dependency and no shape-sniffing.
    let splitter = PE_SEGMENT_SPLITTER;
    r.register(
        splitter,
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
        pe_splitter_try_advance, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        splitter,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        |ctx, args| {
            // `Spliterator.forEachRemaining` has a default body, but it is not
            // reachable here: the receiver's class is a fabricated stub with no
            // declared interfaces, so nothing routes the call to
            // `java.util.Spliterator`. Every method the contract exposes is
            // therefore registered explicitly rather than inherited.
            while matches!(pe_splitter_try_advance(ctx, args)?, Some(Value::Int(1))) {}
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        splitter,
        "trySplit",
        "()Ljava/util/Spliterator;",
        |ctx, args| {
            // The JDK's `SegmentSplitter.trySplit`: split only before the first
            // element is consumed, hand the LOW half to the new splitter and keep
            // the high half (plus the odd element) here.
            let this = obj_arg(args, 0)?;
            let (elem_count, elem_size, index) = pe_splitter_state(ctx, this);
            if index != 0 || elem_count <= 1 {
                return Ok(Some(Value::Object(None)));
            }
            let (split, lobound, hibound) = pe_split_bounds(elem_count, elem_size);
            // What THIS splitter keeps: everything the low half did not take,
            // i.e. the JDK's `split + rem` with `rem = elemCount % 2`.
            let high_count = elem_count - split;
            let segment = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let low = match pe_segment_slice(ctx, segment, 0, lobound, None)? {
                Some(Value::Object(Some(s))) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let low_pin = ctx.pin_native_root(low);
            let prefix = pe_new_splitter(ctx, low, split, elem_size)?;
            ctx.unpin_native_roots(low_pin);
            // Only now narrow this splitter to its own (high) half.
            let this = obj_arg(args, 0)?;
            let segment = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(Value::Object(Some(high))) =
                pe_segment_slice(ctx, segment, lobound, hibound, None)?
            {
                ctx.set_field(this, 0, Value::Object(Some(high)));
            }
            ctx.set_field(this, 1, Value::Long(high_count));
            Ok(Some(Value::Object(Some(prefix))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `elemCount`, NOT `elemCount - currentIndex`. The JDK returns the former
    // verbatim, so a half-drained splitter still reports its ORIGINAL size —
    // measured, `tryAdvance` x3 over 8 elements still answers 8 on HotSpot.
    // Subtracting would be the more sensible number and the wrong one.
    r.register(splitter, "estimateSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (elem_count, _, _) = pe_splitter_state(ctx, this);
        Ok(Some(Value::Long(elem_count.max(0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(splitter, "getExactSizeIfKnown", "()J", |ctx, args| {
        // SIZED, so the exact size IS known — and it is `estimateSize()`,
        // which is what the interface default returns.
        let this = obj_arg(args, 0)?;
        let (elem_count, _, _) = pe_splitter_state(ctx, this);
        Ok(Some(Value::Long(elem_count.max(0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(splitter, "characteristics", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(PE_SPLITTER_CHARACTERISTICS)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(splitter, "hasCharacteristics", "(I)Z", |_ctx, args| {
        let wanted = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let has = (PE_SPLITTER_CHARACTERISTICS & wanted) == wanted;
        Ok(Some(Value::Int(i32::from(has))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        splitter,
        "getComparator",
        "()Ljava/util/Comparator;",
        |_ctx, _args| {
            // Not SORTED — the spec'd answer is to throw, not to return null
            // (null means "sorted in natural order"). `Spliterator`'s default
            // throws `new IllegalStateException()`, so `getMessage()` is empty;
            // measured on HotSpot as `IllegalStateException: null`.
            Err(RuntimeError::IllegalStateException {
                message: String::new(),
            }
            .into())
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // fill(byte value) — memset
    r.register(
        ms,
        "fill",
        "(B)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            // Defense-in-depth: fill writes to the segment's raw `ptr` field.
            require_segment_access(ctx, "fill")?;
            let this = obj_arg(args, 0)?;
            let byte_val = match args.get(1) {
                Some(Value::Int(n)) => *n as u8,
                _ => 0,
            };
            // Round 12 wave 5 (lane ffm2): `fill` never asked whether the
            // segment was writable or its scope alive, so a closed arena's
            // segment was filled (a use-after-close; a use-after-FREE once the
            // block is freed on close) and a read-only view was written. The
            // JDK's `checkAccess(0, length, false)` refuses the write first,
            // then the session check. The window is held until the last store.
            let _window = crate::ffm_fast::AccessWindow::open();
            if ffm_bulk_scope_check() {
                craton_segment_check_read_only(ctx, this, false)?;
                pe_segment_check_scope(ctx, this)?;
                // A heap segment has no address (`segment_address` answers 0
                // for one), so the raw path below skipped it and `fill` on an
                // `ofArray` segment wrote nothing. Write the array instead.
                if let Some(view) = heap_segment_view(ctx, this) {
                    let size = view.size.max(0) as usize;
                    if size > MAX_COPY_SIZE {
                        return Err(RuntimeError::IllegalStateException {
                            message: format!(
                                "fill size {} exceeds maximum of {} bytes",
                                size, MAX_COPY_SIZE
                            ),
                        }
                        .into());
                    }
                    heap_write_bytes(ctx, &view, 0, &vec![byte_val; size]);
                    return Ok(Some(Value::Object(Some(this))));
                }
            }
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0) as usize;
            let addr = crate::panama_libffi::segment_host_address(ctx, this) as *mut u8;
            if size > 0 && !addr.is_null() {
                if size > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "fill size {} exceeds maximum of {} bytes",
                            size, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // SAFETY: addr is non-null, and size is bounded by MAX_COPY_SIZE.
                // The address comes from a JVM-managed MemorySegment.
                unsafe { std::ptr::write_bytes(addr, byte_val, size) };
            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `MemorySegment.toArray(ValueLayout$OfX)` — the segment copied out into a
/// fresh Java array of the matching primitive type.
///
/// The element kind is taken from the layout argument's class name, which is
/// how the eight overloads share one body. `checkArraySize` in the JDK refuses
/// a segment whose size is not a whole number of elements; that is the
/// `size % width` arm here.
pub(in crate::panama) fn pe_segment_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType as AET;
    let this = obj_arg(args, 0)?;
    // Round 12 wave 5 (lane ffm2): an access window from before the scope
    // check to the last element read (see `ffm_fast::AccessWindow`). It spans
    // the result array's allocation, which is allowed: a closer waits for it
    // in a blocking region.
    let _window = crate::ffm_fast::AccessWindow::open();
    pe_segment_check_scope(ctx, this)?;
    let layout = obj_arg(args, 1)?;
    let layout_class = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    let (width, kind) = if layout_class.contains("OfBoolean") {
        (1_i64, AET::Boolean)
    } else if layout_class.contains("OfByte") {
        (1, AET::Byte)
    } else if layout_class.contains("OfChar") {
        (2, AET::Char)
    } else if layout_class.contains("OfShort") {
        (2, AET::Short)
    } else if layout_class.contains("OfInt") {
        (4, AET::Int)
    } else if layout_class.contains("OfFloat") {
        (4, AET::Float)
    } else if layout_class.contains("OfLong") {
        (8, AET::Long)
    } else if layout_class.contains("OfDouble") {
        (8, AET::Double)
    } else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("toArray: unsupported element layout {}", layout_class),
        }
        .into());
    };

    let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
    if size % width != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Segment size is not a multiple of {}. Size: {}",
                width, size
            ),
        }
        .into());
    }
    let count = size / width;
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`, the JDK's own bound.
    if count > i64::from(i32::MAX) - 8 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Segment is too large to wrap as an array. Size: {}", size),
        }
        .into());
    }

    // The alignment gate, AFTER the size gate — that order is the JDK's and it
    // is measured, not assumed. `AbstractMemorySegmentImpl.toArray` runs
    // `checkArraySize` (the `IllegalStateException` above) and only then hands
    // the segment to `MemorySegment.copy`, which is where the alignment
    // `IllegalArgumentException` comes from. So a segment that is BOTH badly
    // sized and badly aligned reports the size:
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[15]).toArray(JAVA_INT)` | ISE `Segment size is not a multiple of 4. Size: 15` |
    // | `ofArray(byte[16]).toArray(JAVA_INT)` | IAE `Source segment incompatible with alignment constraints` |
    // | `ofArray(byte[16]).toArray(JAVA_INT_UNALIGNED)` | `[50462976, 117835012, 185207048, 252579084]` |
    // | `ofArray(int[8]).toArray(JAVA_LONG)` | IAE, same message |
    // | `ofArray(int[8]).asSlice(4).toArray(JAVA_INT).length` | 7 |
    //
    // Note the message is `Source segment ...`, not the `Incompatible
    // alignment constraints` that `spliterator` answers: they come from
    // different JDK call sites and both are transcribed
    // (`FfmProbe` G2/G3/G5/G50, `FfmProbe3` M1n/M2a).
    let layout_align = crate::panama_libffi::layout_align(ctx, layout) as i64;
    if layout_align > pe_segment_max_byte_alignment(ctx, this) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Source segment incompatible with alignment constraints".into(),
        }
        .into());
    }

    // A HEAP SEGMENT'S BYTES ARE IN A JAVA ARRAY, NOT AT AN ADDRESS.
    //
    // `segment_address` answers 0 for every heap carrier (F27), and the raw
    // loop below then took its `base.is_null()` early return and handed back a
    // correctly-sized array of ZEROS. `MemorySegment.ofArray(new byte[]{1,2,3})
    // .toArray(JAVA_BYTE)` answered `[0, 0, 0]` where the oracle answers
    // `[1, 2, 3]` — a silent wrong answer, not a refusal, on the one method
    // whose entire job is to hand the bytes back. It reached real
    // `HeapMemorySegmentImpl$Of*` receivers and every heap slice this file
    // mints (`asSlice` is force-routed, so a slice of a real heap segment is
    // one of ours).
    //
    // `heap_segment_read` is the same reader `get` uses, so `toArray` and a
    // loop of `get`s cannot disagree about the stride or the byte order.
    //
    // Round 12 wave 5 (lane ffm2): the element layout's byte order is honoured
    // here as in `get` (`toArray(JAVA_INT.withOrder(BIG_ENDIAN))` swaps on
    // HotSpot), and the view is resolved AFTER the result array's allocation:
    // resolved before it, the backing array it names could have been moved by
    // that allocation (native stale-local family), so the segment is pinned
    // across it instead.
    let little = {
        crate::phases_late::foreign_ffm::p67_layout_is_little(ctx, layout)
    };
    if heap_segment_view(ctx, this).is_some() {
        let this_pin = ctx.pin_native_root(this);
        let arr = ctx.new_array(kind, count as usize);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        let Some(view) = heap_segment_view(ctx, this) else {
            return Err(unreadable_heap_segment(ctx, this, "toArray"));
        };
        for i in 0..count as usize {
            let raw = heap_segment_read(ctx, &view, i as i64 * width, width as usize);
            let raw = if little {
                raw
            } else {
                pe_swap_width(raw, width)
            };
            let value = match kind {
                AET::Boolean => Value::Int(i32::from((raw & 0xff) != 0)),
                AET::Byte => Value::Int(i32::from(raw as u8 as i8)),
                AET::Char => Value::Int(i32::from(raw as u16)),
                AET::Short => Value::Int(i32::from(raw as u16 as i16)),
                AET::Int => Value::Int(raw as u32 as i32),
                AET::Float => Value::Float(f32::from_bits(raw as u32)),
                AET::Long => Value::Long(raw as i64),
                _ => Value::Double(f64::from_bits(raw)),
            };
            ctx.set_array_element(arr, i, value);
        }
        return Ok(Some(Value::Object(Some(arr))));
    }

    let base = crate::panama_libffi::segment_host_address(ctx, this) as *const u8;
    let arr = ctx.new_array(kind, count as usize);
    if base.is_null() {
        return Ok(Some(Value::Object(Some(arr))));
    }
    let swap = little != cfg!(target_endian = "little");
    for i in 0..count as usize {
        // SAFETY: `i * width` is inside the segment by the size check above,
        // and each read is exactly `width` bytes wide (unaligned).
        let raw = unsafe { pe_read_raw(base.add(i * width as usize), width) };
        let raw = if swap { pe_swap_width(raw, width) } else { raw };
        let value = match kind {
            AET::Boolean => Value::Int(i32::from((raw & 0xff) != 0)),
            AET::Byte => Value::Int(i32::from(raw as u8 as i8)),
            AET::Char => Value::Int(i32::from(raw as u16)),
            AET::Short => Value::Int(i32::from(raw as u16 as i16)),
            AET::Int => Value::Int(raw as u32 as i32),
            AET::Float => Value::Float(f32::from_bits(raw as u32)),
            AET::Long => Value::Long(raw as i64),
            _ => Value::Double(f64::from_bits(raw)),
        };
        ctx.set_array_element(arr, i, value);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// A `long` argument, tolerant of the `Int` an interpreter frame may carry.
pub(in crate::panama) fn pe_long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    }
}

/// `SegmentAllocator.allocateFrom(ValueLayout$OfX, X... elements)`.
///
/// One body for all eight overloads: the element width comes from the layout
/// argument and the values from the Java array, the same way `toArray` reads
/// its kind from the layout rather than the descriptor.
pub(in crate::panama) fn pe_allocate_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arena = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let Some(Value::Object(Some(array))) = args.get(2).copied() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("allocateFrom: elements array is null".to_string()),
        }
        .into());
    };
    let (width, align) = crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
    if width <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid byte size: {}", width),
        }
        .into());
    }
    let count = ctx.array_length(array) as i64;
    let size = count.saturating_mul(width);
    // Round 12 wave 6 (lane ffm3): one access window over the allocation and
    // the element writes, so a shared close racing this call either refuses
    // the allocation or waits for the writes before it frees the new block
    // (the allocation records the block against the session first).
    let _window = crate::ffm_fast::AccessWindow::open();
    // The segment's allocation can collect and move the source array, which
    // is read below: pinned across it (it was held raw).
    let array_pin = ctx.pin_native_root(array);
    // Zero elements still allocates: the JDK hands back an empty segment rather
    // than null, and `Arena.allocate(0)` is legal.
    let allocated = pe_arena_allocate_impl(ctx, arena, size.max(1), align);
    let array = ctx.read_native_pin(array_pin, array);
    ctx.unpin_native_roots(array_pin);
    let Some(Value::Object(Some(segment))) = allocated? else {
        return Ok(Some(Value::Object(None)));
    };
    // The allocator over-allocates one byte for an empty array (`size.max(1)`)
    // so the pointer is real; report the size the caller asked for.
    ctx.set_field(segment, 1, Value::Long(size));
    let base = crate::panama_libffi::segment_address(ctx, segment) as *mut u8;
    if base.is_null() || count == 0 {
        return Ok(Some(Value::Object(Some(segment))));
    }
    for i in 0..count as usize {
        let value = ctx.get_array_element(array, i);
        // SAFETY: `i * width` is inside the block just allocated for
        // `count * width` bytes, and each write is exactly `width` wide.
        unsafe {
            let p = base.add(i * width as usize);
            match width {
                1 => p.write(match value {
                    Value::Int(v) => v as u8,
                    _ => 0,
                }),
                2 => {
                    let v = match value {
                        Value::Int(v) => v as u16,
                        _ => 0,
                    };
                    p.cast::<[u8; 2]>().write(v.to_ne_bytes());
                }
                4 => {
                    let bits = match value {
                        Value::Float(f) => f.to_bits(),
                        Value::Int(v) => v as u32,
                        _ => 0,
                    };
                    p.cast::<[u8; 4]>().write(bits.to_ne_bytes());
                }
                _ => {
                    let bits = match value {
                        Value::Double(d) => d.to_bits(),
                        Value::Long(v) => v as u64,
                        Value::Int(v) => i64::from(v) as u64,
                        _ => 0,
                    };
                    p.cast::<[u8; 8]>().write(bits.to_ne_bytes());
                }
            }
        }
    }
    Ok(Some(Value::Object(Some(segment))))
}

/// The receiver class of `MemorySegment.spliterator(...)`.
///
/// Deliberately NOT `java/util/Spliterator`. That name is already the runtime
/// class of the array-backed collections spliterators, whose natives are
/// registered by `native-collections` *after* this file's registrar runs — so
/// re-registering `tryAdvance` there would take over every `ArrayList`
/// spliterator in the VM. A distinct class also makes
/// `StreamSupport.stream(spliterator, false)` treat this as a real
/// `Spliterator` implementation and drain it lazily through `tryAdvance`,
/// which is exactly the behaviour wanted here.
///
/// The real JDK's own name for this class (`javap`, 25.0.3+9-LTS:
/// `jdk.internal.foreign.AbstractMemorySegmentImpl$SegmentSplitter`), not a
/// plausible-looking one under `MemorySegment`, which has no such nested
/// class at all (`javap java.lang.foreign.MemorySegment$SegmentSplitter` ->
/// `class not found`). A `--real-jdk` run refuses to fabricate a class under
/// a name that names nothing real, so the wrong string here was invisible in
/// Compatible mode — where fabrication is unconditional — and surfaced only
/// as `NoClassDefFoundError` under `--real-jdk`, on the first
/// `elements`/`spliterator` call any vector made. MEASURED: building this
/// tree's own `origin/dev` and running `RJdkFfmSegment` under `--real-jdk`
/// reproduces the crash at check B8 (`RJdkFfmSegment.java:73`) even after the
/// G2-1/G6-1/G7-1 retirement that landed everything else this file measures —
/// that retirement's own gate run did not exercise this vector with
/// `CRATONVM_ARGS="--real-jdk"` set, so the crash went unseen.
pub(in crate::panama) const PE_SEGMENT_SPLITTER: &str = "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter";

/// `NONNULL | SUBSIZED | SIZED | IMMUTABLE | ORDERED`, the value the JDK's
/// `AbstractMemorySegmentImpl.SegmentSplitter.characteristics()` returns.
pub(in crate::panama) const PE_SPLITTER_CHARACTERISTICS: i32 = 0x100 | 0x4000 | 0x40 | 0x400 | 0x10;

/// `(low half element count, low half byte size, high half byte size)`.
///
/// The JDK's `SegmentSplitter.trySplit` arithmetic, lifted out so it can be
/// checked without a heap: the LOW half gets `elemCount / 2` elements and the
/// odd one stays with the high half, so an odd count splits 2/3, not 3/2. An
/// off-by-one here silently drops or duplicates an element in every parallel
/// stream over a segment, which is exactly the bug a differential probe over
/// an even count cannot see.
pub(in crate::panama) fn pe_split_bounds(elem_count: i64, elem_size: i64) -> (i64, i64, i64) {
    let rem = elem_count % 2;
    let split = elem_count / 2;
    let lobound = split * elem_size;
    let hibound = lobound + (rem * elem_size);
    (split, lobound, hibound)
}

/// `(elemCount, elementSize, currentIndex)` off a splitter carrier.
///
/// Slots: `[0]=segment, [1]=elemCount, [2]=elementSize, [3]=currentIndex`.
pub(in crate::panama) fn pe_splitter_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> (i64, i64, i64) {
    let long_at = |ctx: &mut dyn NativeContext, i: usize| match ctx.get_field(this, i) {
        Value::Long(v) => v,
        Value::Int(v) => i64::from(v),
        _ => 0,
    };
    (long_at(ctx, 1), long_at(ctx, 2), long_at(ctx, 3))}

/// Allocate a splitter over `segment`.
pub(in crate::panama) fn pe_new_splitter(
    ctx: &mut dyn NativeContext,
    segment: ObjectRef,
    elem_count: i64,
    elem_size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let segment_pin = ctx.pin_native_root(segment);
    let splitter = try_alloc_concurrent_synthetic(ctx, PE_SEGMENT_SPLITTER, 4)?;
    let segment = ctx.read_native_pin(segment_pin, segment);
    ctx.unpin_native_roots(segment_pin);
    ctx.set_field(splitter, 0, Value::Object(Some(segment)));
    ctx.set_field(splitter, 1, Value::Long(elem_count));
    ctx.set_field(splitter, 2, Value::Long(elem_size));
    ctx.set_field(splitter, 3, Value::Long(0));
    Ok(splitter)
}

/// `Spliterator.tryAdvance` — hand the consumer the next slice, one at a time.
///
/// This is where the laziness lives: the slice for element `i` is minted when
/// `i` is reached, so a segment with more elements than fit in memory as
/// separate `MemorySegment` objects still streams. A materialised list of
/// slices would answer the same two calls and be wrong for exactly the case
/// these methods exist for.
pub(in crate::panama) fn pe_splitter_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (elem_count, elem_size, index) = pe_splitter_state(ctx, this);
    if index >= elem_count {
        return Ok(Some(Value::Int(0)));
    }
    let segment = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let slice = pe_segment_slice(ctx, segment, index * elem_size, elem_size, None)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    // Advance BEFORE the callback: the JDK increments in a `finally`, so a
    // consumer that throws still leaves the splitter past this element.
    ctx.set_field(this, 3, Value::Long(index + 1));
    if let Some(Value::Object(Some(consumer))) = args.get(1) {
        let slice = slice.unwrap_or(Value::Object(None));
        ctx.invoke_virtual(*consumer, "accept", "(Ljava/lang/Object;)V", &[slice])?;
    }
    Ok(Some(Value::Int(1)))
}

/// `MemorySegment.spliterator(MemoryLayout)`.
///
/// The four `IllegalArgumentException`s are the JDK's, in its order — see
/// `AbstractMemorySegmentImpl.spliterator`.
pub(in crate::panama) fn pe_segment_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    pe_segment_check_scope(ctx, this)?;
    let (elem_size, elem_align) =
        crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
    if elem_size == 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Element layout size cannot be zero".into(),
        }
        .into());
    }
    if elem_size % elem_align != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Element layout size is not multiple of alignment".into(),
        }
        .into());
    }
    // `maxByteAlignment`, NOT `segment_address % elem_align`.
    //
    // On a heap carrier `segment_address` is deliberately 0 (F27), and
    // `0 % anything == 0`, so this gate admitted EVERY element layout on every
    // heap segment. Measured refusals it let through:
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[16]).spliterator(JAVA_INT)` | IAE `Incompatible alignment constraints` |
    // | `ofArray(byte[16]).elements(JAVA_INT)` | IAE, same message |
    // | `ofArray(int[8]).asSlice(2).elements(JAVA_INT)` | IAE, same message |
    // | `ofArray(byte[16]).elements(JAVA_INT_UNALIGNED)` | 4 elements |
    // | `ofArray(int[8]).elements(JAVA_INT)` | 8 elements |
    // | `ofArray(int[8]).asSlice(4).elements(JAVA_INT)` | 7 elements |
    //
    // (`FfmProbe` G22/G24/G48, `FfmProbe3` M1k/M1l/M1o.) The alignment gate
    // also precedes the size-multiple gate below: `ofArray(byte[15])
    // .elements(JAVA_INT)` is the alignment message, not
    // `Segment size is not a multiple of layout size` (M2b/M2c), which is the
    // order these four checks are already written in.
    if elem_align > pe_segment_max_byte_alignment(ctx, this) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Incompatible alignment constraints".into(),
        }
        .into());
    }
    let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
    if size % elem_size != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Segment size is not a multiple of layout size".into(),
        }
        .into());
    }
    let splitter = pe_new_splitter(ctx, this, size / elem_size, elem_size)?;
    Ok(Some(Value::Object(Some(splitter))))
}

/// `MemorySegment.elements(MemoryLayout)` — `StreamSupport.stream(spliterator, false)`.
///
/// Built here rather than by calling that method, because `NativeContext` has
/// no `invoke_static`. The shape is not invented: it is byte for byte what
/// `service_loader::native_stream_support_stream_from_spliterator` builds for a
/// non-synthetic spliterator — a `java/util/stream/Stream` carrier with a null
/// element array in slot 0 and the spliterator parked in the lazy slot 2, which
/// `native-collections`' `materialize_lazy_stream` / `stream_lazy_spliterator`
/// drain on demand. Going through that path is what keeps `elements()` lazy:
/// `forEach` interleaves `tryAdvance` and `accept` instead of buffering.
/// `MemorySegment.elements(elementLayout)` under `--real-jdk`: what the JDK's
/// `AbstractMemorySegmentImpl.elements` is, `StreamSupport.stream(
/// spliterator(elementLayout), false)`, over the same lazy splitter
/// [`pe_segment_spliterator`] mints. The splitter is an instance of the real
/// `AbstractMemorySegmentImpl$SegmentSplitter` (which implements `Spliterator`),
/// so the real `ReferencePipeline` drives it through ordinary interface calls
/// that land on the splitter natives registered here.
pub(in crate::panama) fn pe_segment_elements_jdk_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(splitter))) = pe_segment_spliterator(ctx, args)? else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.invoke(
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        &[Value::Object(Some(splitter)), Value::Int(0)],
    )
}

pub(in crate::panama) fn pe_segment_elements(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    pe_segment_elements_jdk_stream(ctx, args)
}

/// The base address a segment reports through `address()`.
///
/// NOT `panama_libffi::segment_address`: on a heap carrier that is deliberately
/// 0 (F27 — slot 0 stays 0 so no raw-pointer consumer is ever handed a small
/// integer to dereference), while `address()` answers the offset of the
/// segment's first byte within its backing array. Anything that reasons about
/// WHERE a segment starts — the alignment arms of `asSlice`/`maxByteAlignment`
/// — must use this one, or it reasons about a native address on a segment that
/// has none.
pub(in crate::panama) fn pe_segment_base_address(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => view.start,
        None => crate::panama_libffi::segment_address(ctx, seg),
    }
}

/// The value inside the `Optional` that `MemorySegment.heapBase()` answers.
///
/// A free function rather than a closure body so the read-only rule below has
/// a test that does not need a whole `NativeMethodRegistry` stood up — the same
/// reason F35 lifted `pe_segment_as_slice` out of its closure.
///
/// `Value::Object(None)` means the empty `Optional`.
pub(in crate::panama) fn pe_segment_heap_base(ctx: &dyn NativeContext, seg: ObjectRef) -> Value {
    let read_only = match heap_segment_view(ctx, seg) {
        Some(view) => view.read_only,
        None => matches!(
            match ctx.get_field_by_name(seg, "readOnly") {
                v @ Value::Int(_) => v,
                _ => ctx.get_field(seg, 3),
            },
            Value::Int(n) if n != 0
        ),
    };
    if read_only {
        return Value::Object(None);
    }
    match heap_segment_view(ctx, seg) {
        Some(view) => Value::Object(Some(view.base)),
        None => match ctx.get_field(seg, SEG_BACKING_ARRAY_FIELD) {
            Value::Object(Some(array)) if ctx.object_is_array(array) => Value::Object(Some(array)),
            _ => Value::Object(None),
        },
    }
}

/// The alignment a native carrier can promise, given the address it starts at.
///
/// MEASURED on 25.0.3+9-LTS (`FfmProbe` rows H18–H21, A30, P6):
/// `ofAddress(16)`→16, `ofAddress(12)`→4, `ofAddress(1)`→1, and
/// `MemorySegment.NULL`/`ofAddress(0)`→**4611686018427387904 = 2^62**, not 8.
/// The JDK's rule is `lowestOneBit(address | maxAlignMask)` with the mask
/// falling back to the largest representable power of two when there is no
/// address to constrain it — a zero address constrains nothing, so nothing is
/// refused on it. Answering 8 (what this file used to answer) refused
/// `NULL.asSlice(0, 0, 16)` where the oracle allows it.
pub(in crate::panama) fn native_max_byte_alignment(addr: i64) -> i64 {
    if addr == 0 {
        1_i64 << 62
    } else {
        addr & addr.wrapping_neg()
    }
}

/// The alignment a HEAP carrier can promise at `byte_offset` bytes into its
/// backing array.
///
/// MEASURED (`FfmProbe2` row P1, every offset 0..=16 of seven element types):
/// the answer is `min(elementAlignment, lowestOneBit(byteOffset))`, with
/// offset 0 answering the element alignment outright. Transcribed:
///
/// ```text
/// byte[32]   0:1 1:1 2:1 3:1 4:1 ... 16:1
/// short[16]  0:2 1:1 2:2 3:1 4:2 ... 16:2
/// int[8]     0:4 1:1 2:2 3:1 4:4 ... 16:4
/// long[4]    0:8 1:1 2:2 3:1 4:4 8:8 12:4 16:8
/// ```
///
/// `byte_offset` is the ABSOLUTE offset within the array — `view.start` for
/// the segment itself, `view.start + access_offset` for an access inside it.
/// Passing only `view.start` and then re-checking the access offset modulo the
/// alignment is NOT the same rule and is stricter than the oracle: a slice
/// starting at 2 of an `int[]` has `maxByteAlignment()==2`, yet
/// `int[8].asSlice(2).get(JAVA_INT, 2)` SUCCEEDS on HotSpot (absolute offset
/// 4) while `get(JAVA_INT, 0)` is refused — measured, `FfmProbe3` rows M1c/M1d.
pub(in crate::panama) fn heap_max_byte_alignment(elem_width: i64, byte_offset: i64) -> i64 {
    if byte_offset == 0 {
        elem_width.max(1)
    } else {
        elem_width
            .max(1)
            .min(byte_offset & byte_offset.wrapping_neg())
    }
}

/// `MemorySegment.maxByteAlignment()` for any carrier this VM can decode.
///
/// One reader for the one rule. Every alignment decision in this file —
/// `maxByteAlignment()` itself, `asSlice(long,long,long)`,
/// `asSlice(long,MemoryLayout)`, `spliterator`/`elements`, `toArray`, and the
/// heap `get`/`set` gate — is `constraint <= maxByteAlignment(base + offset)`,
/// and that equivalence is MEASURED, not assumed: `FfmProbe2` §P2/§P3/§P4
/// cross every offset 0..=16 against alignments 1/2/4/8/16 on four heap
/// element types and a native segment, comparing each call's success against
/// `seg.asSlice(off).maxByteAlignment()` **as HotSpot itself computes it**, and
/// report zero mismatches in all nine sweeps.
pub(in crate::panama) fn pe_segment_max_byte_alignment(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => heap_max_byte_alignment(view.elem_width as i64, view.start),
        None => native_max_byte_alignment(crate::panama_libffi::segment_address(ctx, seg)),
    }
}

/// The alignment available at `offset` bytes into `seg`, i.e. what
/// `seg.asSlice(offset).maxByteAlignment()` answers.
///
/// `pub(crate)`: also the alignment check for the FFM layout `VarHandle`
/// path (`lang_invoke::layout_vh_access`), which has no alignment check of
/// its own — see `docs/feature-designs/native-dispatch-architecture.md
/// ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`.
pub(crate) fn pe_segment_max_byte_alignment_at(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => {
            heap_max_byte_alignment(view.elem_width as i64, view.start.saturating_add(offset))
        }
        None => native_max_byte_alignment(
            crate::panama_libffi::segment_address(ctx, seg).saturating_add(offset),
        ),
    }
}

/// The bounds half of every slice, extracted so the alignment-checked arity
/// can run it FIRST.
///
/// MEASURED order (`FfmProbe3`/`FfmProbe2` §P5): `byte[16].asSlice(20, 4, 3)`
/// is an `IndexOutOfBoundsException`, not the `IllegalArgumentException:
/// Invalid alignment constraint : 3` that the same bad alignment produces in
/// bounds — so bounds precede both the power-of-two check and the alignment
/// check. Doing them in the other order reports the second-most-interesting
/// problem.
pub(in crate::panama) fn pe_slice_bounds_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    new_size: i64,
) -> Result<(), MethodCallFailed> {
    let size = crate::panama_libffi::segment_byte_size(ctx, this);
    let end = offset.checked_add(new_size);
    if offset < 0 || new_size < 0 || end.map_or(true, |n| n > size) {
        // `IndexOutOfBoundsException`, NOT `IllegalStateException`.
        //
        // MEASURED, every out-of-range slice arity on both carriers:
        // `IndexOutOfBoundsException: Out of bound access on segment
        // MemorySegment{ kind: heap, heapBase: [B@7c1503a3, address: 0x0,
        // byteSize: 16 }; new offset = 17; new length = 0`. A caller writing
        // `catch (IndexOutOfBoundsException)` — the idiom for a bounds check,
        // and what `heap_segment_check_access` already answers for `get`/`set`
        // — could not catch the `IllegalStateException` this used to raise.
        // The bracketed receiver text is HotSpot's `toString()`
        // (`segment_to_string`; j27 wave 4, lane FFMMSG: the `heapBase`
        // clause was missing).
        return Err(segment_out_of_bounds(ctx, this, offset, new_size));
    }
    Ok(())
}

/// The alignment half, shared by `asSlice(long,long,long)` and
/// `asSlice(long,MemoryLayout)`.
///
/// MEASURED refusal texts, transcribed character for character (note the space
/// before the colon in the first — it is HotSpot's, not a typo):
///
/// * `IllegalArgumentException: Invalid alignment constraint : 3`
/// * `IllegalArgumentException: Target offset incompatible with alignment constraints`
///
/// Neither message interpolates the offset. The version this replaces wrote
/// `Target offset {offset} incompatible with alignment {align}`, which is a
/// different string on every row.
pub(in crate::panama) fn pe_slice_alignment_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    align: i64,
) -> Result<(), MethodCallFailed> {
    if align <= 0 || (align & (align - 1)) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid alignment constraint : {align}"),
        }
        .into());
    }
    if align > pe_segment_max_byte_alignment_at(ctx, this, offset) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Target offset incompatible with alignment constraints".into(),
        }
        .into());
    }
    Ok(())
}

/// The body behind `asSlice(long,long)`, `asSlice(long)`, `asSlice(long,long,
/// long)`, `asSlice(long,MemoryLayout)` and `asReadOnly()`.
///
/// `read_only_override` is `None` to inherit the parent's flag (what a slice
/// does) and `Some(true)` to force it on (what `asReadOnly` does). Inheriting
/// is the F21 rule and not a convenience: read-only is CONTAGIOUS, there is no
/// route back to writable, and a derived view that quietly cleared the flag
/// would be the wrong CAPABILITY one call after the fix.
pub(in crate::panama) fn pe_segment_slice(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    new_size: i64,
    read_only_override: Option<bool>,
) -> MethodCallResult {
    pe_slice_bounds_check(ctx, this, offset, new_size)?;
    // A SLICE OF A HEAP SEGMENT IS A HEAP SEGMENT.
    //
    // The address arithmetic below is only meaningful for a carrier
    // that HAS an address. For a heap one `segment_address` answers 0
    // (F27), so `slice_ptr` came out as the slice's OFFSET and was
    // stamped into slot 0 of a synthetic segment — reconstructing, one
    // level up, exactly the defect F27 closed: a small integer that is
    // not an address, sitting where every consumer reads an address.
    // `ofArray(new byte[16]).asSlice(3, 4).get(JAVA_BYTE, 0)` then
    // dereferenced the literal address 3. (At offset 0 it stayed 0 and
    // refused, which is why the un-sliced repro looked fixed.)
    //
    // Measured on the oracle: `ofArray(new byte[16]).asSlice(3, 4)` has
    // `byteSize=4`, `address()=3`, `isNative()=false`, and a write
    // through it lands at `src[3..7]`.
    if let Some(view) = heap_segment_view(ctx, this) {
        let read_only = i32::from(read_only_override.unwrap_or(view.read_only));
        let start = view.start.saturating_add(offset);
        // G19-1: A SLICE OF A HEAP SEGMENT SHARES ITS PARENT'S SCOPE.
        //
        // MEASURED on 25.0.3+9-LTS (`G19Probe` SC8/SC9):
        // `heap.asSlice(4,4).scope() == heap.scope()` and
        // `heap.asReadOnly().scope() == heap.scope()` are both **true** — and
        // `asReadOnly()` reaches this same body, so one write covers both. Slot
        // 2 was an unconditional `Object(None)` here, which is the same
        // "no scope resolvable" hole the NATIVE arm below closed for W7-89 and
        // this arm never did; the fresh session `p67_receiver_session` minted
        // instead was a different object on every call.
        let parent_session = pe_segment_session(ctx, this);
        // The allocation below can MOVE the backing array and the session (the
        // native stale-local family), so pin both and re-read them through the
        // pins — the same discipline the session handling below uses.
        // `unpin_native_roots` truncates to its argument, so releasing the
        // FIRST handle releases both.
        let base_pin = ctx.pin_native_root(view.base);
        let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
        // gc-common w20-f: the `?` used to sit on this line and leaked both
        // pins on a failed allocation; they are released first now.
        let slice_alloc = alloc_segment_carrier(ctx, SEG_HEAP_FIELDS);
        let base = ctx.read_native_pin(base_pin, view.base);
        let scope_value = match (parent_session, session_pin) {
            (Some(session), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, session))),
            _ => Value::Object(None),
        };
        ctx.unpin_native_roots(base_pin);
        let slice: ObjectRef = slice_alloc?;
        // Slot 0 stays 0: a heap slice has no machine address either,
        // and `segment_address` must keep answering the value every
        // raw-pointer consumer already refuses on.
        ctx.set_field(slice, 0, Value::Long(0));
        ctx.set_field(slice, 1, Value::Long(new_size));
        ctx.set_field(slice, 2, scope_value);
        ctx.set_field(slice, 3, Value::Int(read_only));
        ctx.set_field(slice, 4, Value::Int(1));
        ctx.set_field(slice, 5, Value::Long(0));
        ctx.set_field(slice, SEG_HEAP_BASE_FIELD, Value::Object(Some(base)));
        ctx.set_field(slice, SEG_HEAP_START_FIELD, Value::Long(start));
        return Ok(Some(Value::Object(Some(slice))));
    }
    let base_ptr = crate::panama_libffi::segment_address(ctx, this);
    let slice_ptr = base_ptr.checked_add(offset).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalStateException {
            message: "address arithmetic overflow in MemorySegment.asSlice".into(),
        })
    })?;

    // A synthetic slice cannot retain the real implementation's
    // private scope object.  It stores an already-adjusted absolute
    // address instead, which is valid for both real and synthetic
    // source segments and avoids treating real field 0/5 as ptr/off.
    let read_only = match read_only_override {
        Some(forced) => Value::Int(i32::from(forced)),
        None => match ctx.get_field_by_name(this, "readOnly") {
            Value::Int(n) => Value::Int(n),
            _ => ctx.get_field(this, 3),
        },
    };

    // W7-89: a slice stays inside its parent's scope. Slot 2 used to be
    // written as the "no arena" marker unconditionally, so
    // `pe_segment_session` answered `None` for every slice and
    // `pe_segment_check_scope` let it through — HotSpot raises
    // `IllegalStateException: Already closed` for a slice of a closed
    // arena exactly as it does for the parent (measured,
    // `MemorySessionValidStateProbe` row `C.closed.slice.get`).
    //
    // Only a session we MODELLED is propagated, which is all
    // `pe_segment_session` can return. This fallback only ever sees a
    // genuine off-heap (`Arena`-backed) receiver now — every heap segment,
    // `ofArray`'s included, is caught by the `heap_segment_view` branch
    // above and never reaches here.
    let parent_session = pe_segment_session(ctx, this);
    let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
    let slice_alloc = alloc_segment_carrier(ctx, 6);
    // The allocation above can move the session (native stale-local
    // family), so re-read it through the pin before storing it. (gc-common
    // w20-f: the pin is released before the allocation's `?` exit, which
    // used to leak it.)
    let scope_value = match (parent_session, session_pin) {
        (Some(session), Some(pin)) => {
            let session = ctx.read_native_pin(pin, session);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(session))
        }
        _ => Value::Object(None),
    };
    let slice: ObjectRef = slice_alloc?;
    ctx.set_field(slice, 0, Value::Long(slice_ptr));
    ctx.set_field(slice, 1, Value::Long(new_size));
    ctx.set_field(slice, 2, scope_value);
    ctx.set_field(slice, 3, read_only);
    ctx.set_field(slice, 4, Value::Int(1));
    ctx.set_field(slice, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(slice))))
}

/// `MemorySegment.asByteBuffer()` — a `ByteBuffer` over the segment: a DIRECT
/// one for a native segment, a HEAP one for an `ofArray` segment.
///
/// This is the JDK 25 bridge between `java.lang.foreign` and every existing
/// NIO API, and the reason netty's non-`sun.misc.Unsafe` allocator could not
/// allocate at all on CratonVM: `CleanerJava25` is FFM-`Arena`-backed, and
/// every one of its allocations ended in
/// `AbstractMethodError: MemorySegment.asByteBuffer() has no Code attribute`.
///
/// Mirrors `AbstractMemorySegmentImpl.asByteBuffer()`:
///
/// * `checkArraySize("ByteBuffer", 1)` first — a segment larger than a byte
///   array can hold is an `IllegalStateException`, not a truncated buffer;
/// * `NativeMemorySegmentImpl.makeByteBuffer()` is
///   `NIO_ACCESS.newDirectByteBuffer(min, (int) length, null, this)`, so in
///   real-JDK mode run that very constructor and inherit every `Buffer`
///   invariant instead of restating them. Passing the segment as the
///   constructor's `MemorySegment` argument is also what keeps the arena
///   reachable for the buffer's lifetime — the buffer holds the segment, the
///   segment holds its scope;
/// * a read-only segment yields a read-only buffer (`_bb.asReadOnlyBuffer()`).
///
/// Deliberately NOT gated on `require_native_access`, unlike `get`/`set`/`fill`
/// in this file: `asByteBuffer` is not `@Restricted` in the JDK, an
/// `Arena`-derived segment is a safe (bounds- and lifetime-checked) one, and
/// gating it would deny the exact call netty makes on a VM where HotSpot allows
/// it — trading a real divergence for no security the `Arena.allocate` call
/// that produced the segment already gave away.
pub(in crate::panama) fn pe_segment_as_byte_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    pe_segment_check_scope(ctx, this)?;
    let size = crate::panama_libffi::segment_byte_size(ctx, this);
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`, the bound `checkArraySize` uses.
    const SOFT_MAX_ARRAY_LENGTH: i64 = i32::MAX as i64 - 8;
    if size < 0 || size > SOFT_MAX_ARRAY_LENGTH {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Segment is too large to wrap as ByteBuffer. Size: {}", size),
        }
        .into());
    }
    // A HEAP segment does not become a direct buffer. `MemorySegment.ofArray`
    // has no native address at all, and the direct path below was minting a
    // `DirectByteBuffer` over `segment_address`, which is why
    // `ofArray(byte[16]).asByteBuffer()` answered `isDirect() == true` and
    // `hasArray() == false` where the oracle answers `false`/`true`.
    //
    // `HeapMemorySegmentImpl.makeByteBuffer()` is
    // `newHeapByteBuffer(baseByte, (int) offset - BYTE_ARR_BASE, (int) byteSize(), this)`
    // — a buffer whose `array()` is the segment's OWN backing array and whose
    // `arrayOffset()` is where the segment starts in it. `ByteBuffer.wrap(base,
    // start, size).slice()` produces exactly that shape (`wrap` sets
    // position/limit, `slice` turns the remaining window into capacity and
    // folds the position into `arrayOffset`), and it does so through two
    // natives that are registered in every mode — unlike `slice(II)`, which
    // this VM registers for `CharBuffer`/`IntBuffer`/`LongBuffer`/`FloatBuffer`
    // and not for `ByteBuffer`.
    //
    // The JDK REFUSES a non-`byte[]` base rather than reinterpreting it, and
    // the refusal is observable — `W7-58`'s probe measures
    // `ofArray(int[8]).asByteBuffer()` as `UnsupportedOperationException` on
    // HotSpot 25.
    if let Some(view) = heap_segment_view(ctx, this) {
        if view.elem_type != cratonvm_types::ArrayElementType::Byte {
            return Err(RuntimeError::UnsupportedOperationException {
                message: "Not an address to an heap-allocated byte array".to_string(),
            }
            .into());
        }
        let wrapped = ctx.invoke(
            "java/nio/ByteBuffer",
            "wrap",
            "([BII)Ljava/nio/ByteBuffer;",
            &[
                Value::Object(Some(view.base)),
                Value::Int(view.start as i32),
                Value::Int(view.size as i32),
            ],
        )?;
        let Some(Value::Object(Some(wrapped))) = wrapped else {
            // `wrap` is a registered native in every mode, so this is
            // unreachable; refuse by NAME rather than inventing a buffer.
            return Err(RuntimeError::UnsupportedOperationException {
                message: "ByteBuffer.wrap did not return a heap buffer".to_string(),
            }
            .into());
        };
        let sliced = ctx.invoke_virtual(wrapped, "slice", "()Ljava/nio/ByteBuffer;", &[])?;
        if !view.read_only {
            return Ok(sliced);
        }
        let Some(Value::Object(Some(buf))) = sliced else {
            return Ok(sliced);
        };
        return ctx.invoke_virtual(buf, "asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;", &[]);
    }

    // A heap segment whose SHAPE `heap_segment_view` cannot validate is still a
    // heap segment: `ofArray(int[8])` reaches here with a backing array and no
    // usable byte view, and falling through to the direct path below would hand
    // back a `DirectByteBuffer` over an address the segment does not own. The
    // oracle refuses by name, so refuse by name.
    if let Value::Object(Some(base)) = ctx.get_field(this, SEG_BACKING_ARRAY_FIELD) {
        if ctx.object_is_array(base)
            && ctx.heap_element_type_of(base) != cratonvm_types::ArrayElementType::Byte
        {
            return Err(RuntimeError::UnsupportedOperationException {
                message: "Not an address to an heap-allocated byte array".to_string(),
            }
            .into());
        }
    }

    let addr = crate::panama_libffi::segment_address(ctx, this);
    let read_only = matches!(
        match ctx.get_field_by_name(this, "readOnly") {
            v @ Value::Int(_) => v,
            _ => ctx.get_field(this, 3),
        },
        Value::Int(n) if n != 0
    );

    // Real-JDK mode: `java.nio.DirectByteBuffer(long addr, int cap, Object ob,
    // MemorySegment segment)` is the package-private constructor
    // `JavaNioAccess.newDirectByteBuffer` calls. Both probes are needed for the
    // same reason `ByteBuffer.allocateDirect` needs both — see that registrar.
    let real_direct_byte_buffer = !ctx.would_fabricate_synthetic_stub("java/nio/DirectByteBuffer")
        && !ctx.is_class_synthetic_stub("java/nio/DirectByteBuffer");
    let buffer = if real_direct_byte_buffer {
        ctx.new_object_initialized(
            "java/nio/DirectByteBuffer",
            "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V",
            &[
                Value::Long(addr),
                Value::Int(size as i32),
                Value::Object(None),
                Value::Object(Some(this)),
            ],
        )?
    } else {
        // Synthetic-JDK mode: no `DirectByteBuffer` bytecode to run. Seed the
        // same field set `direct_buffer::dbb_allocate_direct0` does, minus the
        // cleaner — this buffer does not own the memory, the arena does, and
        // registering a cleaner here would free the segment out from under it.
        //
        // Round 14 wave 3 (lane ffm, FFM7-1): this buffer holds only the
        // address, not the segment, so it does not keep an `Arena.ofAuto()`
        // session alive: keep that session's blocks from the sweep.
        match pe_segment_session(ctx, this) {
            Some(session)
                if crate::phases_late::foreign_ffm::p67_session_is_implicit(ctx, session) =>
            {
                auto_arena_blocks_keep(ctx, session);
            }
            // Round 15 wave 3 (lane ffm2): nor a closed shared session alive,
            // so its blocks must never be freed at its death. Round 15 wave 5
            // (lane upcall1; `r15w3-ffm2-synthetic-bytebuffer-view-reads-freed-block`):
            // nor may a CONFINED close free them (`p67_session_run_close_actions`),
            // nor the `--synthetic-jdk` four-slot arena's (`pe_arena_close`),
            // whose segments have no session: the flag is noted for them too.
            _ => auto_arena_note_address_only_view(ctx.vm_identity()),
        }
        // Round 15 wave 5 (lane upcall1): the liveness check again, AFTER the
        // flag. A close on another thread either read the flag (under the
        // table lock) after it was set, and so frees nothing, or read it
        // before -- and then its state flip, which precedes that read, is
        // visible here and this refuses with "Already closed" before a buffer
        // over the freed block exists.
        pe_segment_check_scope(ctx, this)?;
        let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/DirectByteBuffer", 8)?;
        ctx.set_field_by_name(buf, "address", Value::Long(addr));
        ctx.set_field_by_name(buf, "capacity", Value::Int(size as i32));
        ctx.set_field_by_name(buf, "limit", Value::Int(size as i32));
        ctx.set_field_by_name(buf, "position", Value::Int(0));
        ctx.set_field_by_name(buf, "mark", Value::Int(-1));
        ctx.set_field_by_name(buf, "bigEndian", Value::Int(1));
        ctx.set_field_by_name(
            buf,
            "nativeByteOrder",
            Value::Int(i32::from(cfg!(target_endian = "big"))),
        );
        let named_ok = matches!(
            ctx.get_field_by_name(buf, "capacity"),
            Value::Int(c) if c == size as i32
        );
        if !named_ok {
            ctx.set_field(buf, 0, Value::Int(0)); // position
            ctx.set_field(buf, 1, Value::Int(size as i32)); // limit
            ctx.set_field(buf, 2, Value::Int(size as i32)); // capacity
            ctx.set_field(buf, 3, Value::Int(-1)); // mark
            ctx.set_field(buf, 4, Value::Long(addr)); // address
            ctx.set_field(buf, 5, Value::Long(size)); // native size
            ctx.set_field(buf, 6, Value::Int(0)); // no cleaner: the arena owns it
            ctx.set_field(buf, 7, Value::Int(0));
        }
        Some(Value::Object(Some(buf)))
    };

    if !read_only {
        return Ok(buffer);
    }
    let Some(Value::Object(Some(buf))) = buffer else {
        return Ok(buffer);
    };
    ctx.invoke_virtual(buf, "asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;", &[])
}

// --- Scope validity: refuse access through a closed Arena ---
//
// `phases_late::foreign_ffm` gives every synthetic `Arena` a *session* object
// and `Arena.close()` now genuinely closes it and runs its close actions — so
// the off-heap block a segment points at is really freed. The raw load/store
// below would then read or write freed memory; HotSpot raises
// `IllegalStateException` instead, and so must we.
//
// The two layouts are `foreign_ffm`'s. They are MIRRORED here rather than
// shared because those helpers are private to that module, and the one public
// resolver it does expose (`p67_receiver_session`) mints a fresh — therefore
// always-open — session when it finds nothing, which would make every check
// below trivially pass:
//
//   Arena   : [0] = open (Int), [1] = session
//   session : four words, whose SLOTS are `foreign_ffm`'s to decide — see
//             `P67SessionSlots`. This file used to hard-code `state` at slot 0
//             and a width of 4; both are deleted (W7-89) because that second
//             copy of the index is what made the check dead in Compatible mode,
//             where the real `MemorySessionImpl` types slot 0 as a REFERENCE.
//
// Both are `alloc_concurrent_synthetic` objects, so their class names are
// exactly the two constants below. Every step of the resolution is gated on
// those names, which is what makes it self-validating rather than
// shape-guessing: a miss answers "no resolvable scope" (access proceeds
// unchanged) and never "closed".
pub(in crate::panama) const PE_ARENA_CLASS: &str = "java/lang/foreign/Arena";
/// The class `foreign_ffm` now mints an arena as. It is a REAL JDK class, so
/// unlike `PE_ARENA_CLASS` it is not a fabrication -- and because the two
/// producers finally differ, a class test can replace the width test below.
pub(in crate::panama) const PE_ARENA_IMPL_CLASS: &str = crate::phases_late::foreign_ffm::P67_ARENA_IMPL;
pub(in crate::panama) const PE_SESSION_CLASS: &str = "jdk/internal/foreign/MemorySessionImpl";
pub(in crate::panama) const PE_ARENA_SESSION_FIELD: usize = 1;
/// Slot 2 of a synthetic segment names the arena that allocated it — this
/// file's own convention (see the `// no arena` writes in `ofAddress` and
/// `asSlice`), which `foreign_ffm`'s 3-field segments adopted as well.
///
/// The slot is REUSED by `ofArray` segments to retain the Java backing array
/// ([`SEG_BACKING_ARRAY_FIELD`]), and on a real-JDK segment it is whatever
/// field happens to sit at index 2 — hence the class-name gate in
/// [`pe_arena_session`]: we must never follow a non-Arena object's slots.
pub(in crate::panama) const PE_SEGMENT_ARENA_FIELD: usize = 2;

/// Single-entry positive/negative memo for an exact class-name test.
///
/// The test itself is what makes the resolution safe — unlike a field-shape
/// probe it cannot mistake a primitive array, whose raw slots decode to
/// arbitrary `Value`s, for an object we modelled — but `class_name_of_id`
/// takes the class-manager lock and allocates a `String`, and this runs on
/// every FFM load/store. Class ids are process-stable and each site sees
/// exactly two shapes (the arena / the `ofArray` backing array), so one
/// remembered hit id and one remembered miss id keep the steady state at an
/// integer compare.
pub(in crate::panama) struct PeClassMemo {
    pub(in crate::panama) hit: std::sync::atomic::AtomicU32,
    pub(in crate::panama) miss: std::sync::atomic::AtomicU32,
}

impl PeClassMemo {
    /// `u32::MAX` is the "nothing remembered" sentinel; no real class id
    /// reaches it.
    pub(in crate::panama) const fn new() -> Self {
        Self {
            hit: std::sync::atomic::AtomicU32::new(u32::MAX),
            miss: std::sync::atomic::AtomicU32::new(u32::MAX),
        }
    }

    pub(in crate::panama) fn matches(&self, ctx: &dyn NativeContext, obj: ObjectRef, expected: &str) -> bool {
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        let class_id = ctx.class_id_of_object(obj);
        let raw = class_id.as_u32();
        // G19-1: THE MEMO IS BYPASSED UNDER `cfg(test)`, AND ONLY THERE.
        //
        // Class ids are process-stable on the real VM, which is the whole
        // premise of remembering one. Under `MockNativeContext` they are a
        // PER-CONTEXT counter, so id 7 names `MemorySegment` in one test and
        // `MemorySessionImpl` in the next — and this struct remembers exactly
        // one hit and one miss for the whole process, so the second test is
        // handed the first test's answer for a class it never saw. That made
        // every test of a session-shaped predicate non-deterministic (it
        // depends on which sibling test ran last, and they run in threads).
        // The memo is a pure cache: skipping it changes no answer, only the
        // number of `class_name_arc_of_id` calls.
        if cfg!(test) {
            return ctx.class_name_arc_of_id(class_id).as_deref() == Some(expected);
        }
        if raw == self.hit.load(relaxed) {
            return true;
        }
        if raw == self.miss.load(relaxed) {
            return false;
        }
        let matched = ctx.class_name_arc_of_id(class_id).as_deref() == Some(expected);
        if matched {
            self.hit.store(raw, relaxed);
        } else {
            self.miss.store(raw, relaxed);
        }
        matched
    }
}

pub(in crate::panama) static PE_ARENA_CLASS_MEMO: PeClassMemo = PeClassMemo::new();
pub(in crate::panama) static PE_ARENA_IMPL_MEMO: PeClassMemo = PeClassMemo::new();

/// Whether `arena` is one of the two carriers this workspace mints for
/// `java.lang.foreign.Arena`. One memo per name: `PeClassMemo` remembers a
/// single hit and a single miss, so asking one memo about two names would make
/// each answer evict the other's.
pub(in crate::panama) fn pe_is_modelled_arena(ctx: &dyn NativeContext, arena: ObjectRef) -> bool {
    PE_ARENA_CLASS_MEMO.matches(ctx, arena, PE_ARENA_CLASS)
        || PE_ARENA_IMPL_MEMO.matches(ctx, arena, PE_ARENA_IMPL_CLASS)
}
static PE_SESSION_CLASS_MEMO: PeClassMemo = PeClassMemo::new();

/// Whether `session` carries the layout `foreign_ffm` writes. Anything else —
/// a real JDK `ConfinedSession`/`SharedSession`, or an object that merely
/// happens to sit in the session slot — is left strictly alone.
///
/// The state word is located through `foreign_ffm`'s own slot map rather than
/// through a second copy of the index. That map is the W7-89 repair: in
/// Compatible mode the carrier is the REAL `MemorySessionImpl`, whose slot 0 is
/// a declared REFERENCE (`resourceList`), so the model's `Int` state word never
/// read back as an `Int` there and this predicate answered false for every
/// session — which is why the choke point below, though correctly wired since
/// W7-58, never once fired. Calling the shared resolver keeps the decision in
/// ONE implementation; open-coding the index here is what let the two files
/// drift out of step in the first place.
pub(crate) fn pe_session_modelled(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if !PE_SESSION_CLASS_MEMO.matches(ctx, session, PE_SESSION_CLASS) {
        return false;
    }
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    ctx.object_num_fields(session) >= slots.required_width()
        && matches!(ctx.get_field(session, slots.state), Value::Int(_))
}

/// The session stored on a synthetic `Arena`, if `arena` is one.
///
/// Rejects [`register_pe_arena`]'s rival 4-slot arena, whose slot 1 is an
/// int-array of allocation ids rather than a session: the array fails
/// [`pe_session_modelled`]'s class-name test, so the arena resolves to `None`
/// (no scope) instead of to a bogus "closed" session. That shape is the one
/// that wins under `--synthetic-jdk`, where a false throw here would break
/// every FFM access.
pub(in crate::panama) fn pe_arena_session(ctx: &dyn NativeContext, arena: ObjectRef) -> Option<ObjectRef> {
    if !pe_is_modelled_arena(ctx, arena) {
        return None;
    }
    // Through the shared resolver, not a second copy of the index: on
    // `ArenaImpl` the session is the class's own declared field and is NOT at
    // `PE_ARENA_SESSION_FIELD`. Open-coding it here is what let this file and
    // `foreign_ffm` drift apart over the session layout, as the comment on
    // `pe_session_modelled` above records.
    let session_slot = crate::phases_late::foreign_ffm::p67_arena_slots(ctx, arena).session;
    if ctx.object_num_fields(arena) <= session_slot {
        return None;
    }
    match ctx.get_field(arena, session_slot) {
        Value::Object(Some(session)) if pe_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// What a carrier minted for `arena` keeps in slot 2 (round 13 wave 5, lane
/// ffm3): the arena's SESSION when this model built the arena, else the arena.
///
/// Slot 2 is where `AbstractMemorySegmentImpl.scope` sits (`length`,
/// `readOnly`, `scope`), and `sessionImpl()` is `final` there, so JDK bytecode
/// that reaches a carrier through an `AbstractMemorySegmentImpl` cast --
/// `AbstractMemorySegmentImpl.copy` from `MemorySegment.copy`, and every
/// `Buffer.session()` of an `asByteBuffer()` view -- runs `return scope`
/// against this slot instead of the carrier's registered `sessionImpl` native.
/// With the ARENA there it handed `ScopedMemoryAccess` an `ArenaImpl` and
/// died in `NoSuchMethodError: ArenaImpl.checkValidStateRaw()`
/// (`R13Ffm3AllocateFromShapes`). Every reader of slot 2 in this crate already
/// accepts a session there (`pe_segment_session`,
/// `foreign_ffm::p67_receiver_session`, `p67_segment_check_scope`): slices have
/// been stamped that way since G19-1. The synthetic-JDK four-slot arena keeps
/// itself there (it has no session; `pe_segment_in_closed_synthetic_arena`
/// reads its closed flag through the slot).
pub(crate) fn pe_carrier_scope(ctx: &dyn NativeContext, arena: ObjectRef) -> ObjectRef {
    {
        if let Some(session) = pe_arena_session(ctx, arena) {
            return session;
        }
    }
    arena
}

/// Allocation-free session getter for this module's own segment carriers.
/// Only a stored session is served: arena, array and unscoped shapes retain
/// `p67_receiver_session`'s complete resolution and possible allocation.
/// Keeping the slot convention here prevents the JIT from guessing its layout.
pub fn jit_carrier_stored_session(
    ctx: &dyn NativeContext,
    segment: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    if ctx.class_id_by_name(CRATON_SEGMENT_CLASS) != Some(ctx.class_id_of_object(segment))
        || ctx.object_is_array(segment)
        || ctx.object_num_fields(segment) <= PE_SEGMENT_ARENA_FIELD
    {
        return None;
    }
    match ctx.get_field(segment, PE_SEGMENT_ARENA_FIELD) {
        Value::Object(Some(session)) if pe_session_modelled(ctx, session) => {
            Some((session, PE_SEGMENT_ARENA_FIELD))
        }
        _ => None,
    }
}



/// The session governing `seg`'s lifetime, or `None` when the segment has no
/// scope we can resolve — `MemorySegment.ofAddress`, `asSlice`, `ofArray`, the
/// global arena and every segment this file allocates outside an arena. Those
/// must keep working exactly as before, so an unresolvable scope is NOT an
/// error.
///
/// Allocation-free and safepoint-free by construction (plain field/class
/// reads only), so no caller has to pin `seg` across the check.
pub(in crate::panama) fn pe_segment_session(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<ObjectRef> {
    // Fast path: our own slot-2 convention. One field read decides it for the
    // overwhelmingly common shapes — an arena-allocated segment resolves here,
    // and an explicit `Object(None)` is the "no arena" marker this file writes,
    // which needs no further lookup.
    if ctx.object_num_fields(seg) > PE_SEGMENT_ARENA_FIELD {
        match ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) {
            Value::Object(None) => return None,
            Value::Object(Some(owner)) => {
                if let Some(session) = pe_arena_session(ctx, owner) {
                    return Some(session);
                }
                // Tolerate a segment stamped with the session directly.
                if pe_session_modelled(ctx, owner) {
                    return Some(owner);
                }
                // Not our convention (an `ofArray` backing array, or a real
                // segment's own reference field) — fall through.
            }
            // A primitive there means this is not our layout at all.
            _ => {}
        }
    }
    // An `ofArray` MIRROR carrier keeps the array in slot 2 and the session one
    // slot further out. G19-1 §6.2.
    if let Some(session) = pe_mirror_session(ctx, seg) {
        return Some(session);
    }
    // A real-JDK segment carries its session in `AbstractMemorySegmentImpl
    // .scope`, and that field holds one of OUR sessions because the
    // `createConfined`/`createShared` factories are force-dispatched into
    // `foreign_ffm`. A real session we did not build is an honest miss: we
    // cannot read its state word without guessing its encoding, so we let the
    // access through rather than throw on a shape we misread.
    match ctx.get_field_by_name(seg, "scope") {
        Value::Object(Some(scope)) if pe_session_modelled(ctx, scope) => Some(scope),
        _ => None,
    }
}

/// The session stamped into an `ofArray` MIRROR carrier's
/// [`SEG_MIRROR_SESSION_FIELD`], if `seg` is one.
///
/// RETIRED 2026-09-21: `ofArray` no longer mints the seven-field mirror shape
/// this recognizes (see [`SEG_MIRROR_SESSION_FIELD`]), so this now always
/// answers `None`. Left in place, `pub(crate)`, and still called from
/// `foreign_ffm::p67_receiver_session` — an inert recognizer costs nothing to
/// keep and removing it would mean also auditing that file's own
/// `object_is_array(owner)` guard (kept for the same reason: defensive, now
/// unreachable, not worth the extra risk to touch in this change).
///
/// The width test is an EQUALITY, not a `>`: slot 6 on an eight-field alias
/// carrier is the backing ARRAY ([`SEG_HEAP_BASE_FIELD`]), and while
/// [`pe_session_modelled`] would reject it on the class-name test anyway, an
/// exact width says which carrier we are looking at rather than relying on the
/// occupant to disqualify itself.
pub(crate) fn pe_mirror_session(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(seg) != SEG_MIRROR_FIELDS {
        return None;
    }
    match ctx.get_field(seg, SEG_MIRROR_SESSION_FIELD) {
        Value::Object(Some(session)) if pe_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// Raise `IllegalStateException` if `seg`'s scope has already been closed, or
/// `WrongThreadException` if it is confined to a different thread.
///
/// This is the single choke point for `get`/`set`/`getAtIndex`/`setAtIndex`:
/// all four reach [`pe_segment_access_addr`], which calls this before it
/// computes an address.
pub(crate) fn pe_segment_check_scope(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let Some(session) = pe_segment_session(ctx, seg) else {
        if pe_segment_in_closed_synthetic_arena(ctx, seg) {
            return Err(RuntimeError::IllegalStateException {
                message: "Already closed".into(),
            }
            .into());
        }
        // A real JDK session (`--real-jdk`), checked as `checkValidState`.
        if let Some((session, _)) = pe_segment_real_session(ctx, seg) {
            return pe_real_session_check(ctx, session);
        }
        return Ok(());
    };
    pe_session_check_open(ctx, session)
}

/// Whether `seg` was allocated by `--synthetic-jdk`'s four-slot arena
/// ([`pe_new_arena_carrier`]) and that arena has been closed.
///
/// Round 12 wave 3 (lane ffm). That arena has no session, so
/// [`pe_segment_session`] answers `None` for its segments and the scope check
/// let every access through — while [`pe_arena_close`] really frees their
/// blocks. A segment of a closed synthetic arena was therefore read and written
/// after the free, with no exception, on every path (and the free's epoch bump
/// only sent the JIT fast path back to that same unchecked native). The closed
/// flag the arena already keeps (slot 2) is the whole answer; nothing new is
/// recorded. Kill switch: `CRATONVM_FFM_SYNTHETIC_ARENA_CLOSED_CHECK=0`.
///
/// Class-gated before any slot of the owner is read, like
/// [`pe_arena_session`]: slot 2 of a segment is also an `ofArray` backing
/// array. Slices (`asSlice`) of such a segment carry no arena and are still
/// unchecked; see `r12w3-ffm-arena-close-frees-nothing-in-the-default-model`.
pub(in crate::panama) fn pe_segment_in_closed_synthetic_arena(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| {
        true
    });
    if !on || ctx.object_num_fields(seg) <= PE_SEGMENT_ARENA_FIELD {
        return false;
    }
    let Value::Object(Some(owner)) = ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) else {
        return false;
    };
    PE_ARENA_CLASS_MEMO.matches(ctx, owner, PE_ARENA_CLASS)
        && ctx.object_num_fields(owner) > 3
        && matches!(ctx.get_field(owner, 2), Value::Int(1))
}

/// The close half of the FFM access handshake, for a native that has just
/// marked a scope closed and has not yet freed anything it governs. See
/// `ffm_fast::close_handshake_begin`.
///
/// Retires every fast-path verdict that could name the scope's segments (all
/// of them when `shared`, the closing owner's when confined), and when
/// `shared` waits for every element access that was already past its check.
/// Pass `shared` for anything but a session whose owner check this thread has
/// just passed. The wait runs in a blocking region
/// (the accessor may be suspended by, or be allocating into, a collection),
/// so a collection can move objects meanwhile: `refs` are re-synced, and a
/// caller must not hold any other raw reference across this call.
pub(crate) fn pe_close_handshake(ctx: &mut dyn NativeContext, shared: bool, refs: &mut [Value]) {
    let in_flight = crate::ffm_fast::close_handshake_begin(shared);
    if in_flight.is_empty() {
        return;
    }
    // JIT round 17 wave 2 (lane ffm2; `CRATONVM_FFM_CLOSE_WAIT_RUNNABLE`): the
    // wait is VM code (HotSpot's close handshake runs `_thread_in_vm`), so the
    // closer reports RUNNABLE, not WAITING. Same deposit and wake either way.
    {
        ctx.begin_vm_wait_region();
    }
    in_flight.wait();
    ctx.end_blocking_region_refs(refs);
}



/// `Err` if this MODELLED session is confined to another thread
/// (`WrongThreadException`) or has been closed (`IllegalStateException`),
/// thread checked first — the JDK's own order (`MemorySessionImpl
/// .checkValidStateRaw()`), so a confined session touched from the wrong
/// thread reports the thread error even once it has been closed.
///
/// One implementation, called by both the access path and
/// `pe_arena_allocate_impl`: a second open-coded copy of "state == 0 means
/// closed" is precisely the drift W7-89 had to unpick between this file and
/// `foreign_ffm`. The caller is responsible for having resolved `session`
/// through [`pe_segment_session`] / [`pe_arena_session`], both of which gate on
/// [`pe_session_modelled`] -- so a session whose encoding we do not own never
/// reaches here.
///
/// W7-89 §6.3: this used to check liveness only, so a confined arena's
/// `get`/`set`/`allocate` off the owning thread never raised
/// `WrongThreadException` — `p67_session_check_valid` had the owner check all
/// along, but nothing on this file's access path called it. Reusing the same
/// slot map and the same exception builder keeps the two checks from drifting
/// apart again.
pub(in crate::panama) fn pe_session_check_open(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    if let Value::Object(Some(owner)) = ctx.get_field(session, slots.owner) {
        if !crate::phases_late::foreign_ffm::p67_owner_is_current_thread(ctx, owner) {
            return Err(crate::phases_late::foreign_ffm::p67_wrong_thread(ctx));
        }
    }
    if matches!(ctx.get_field(session, slots.state), Value::Int(0)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".into(),
        }
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Real JDK sessions (`FFM-door-seg-probe-residuals-FIXED-20261007.md`)
// ---------------------------------------------------------------------------
//
// Under `--real-jdk` the JDK's own `Arena.ofConfined()` / `ofShared()` bytecode
// builds the session: a `jdk/internal/foreign/ConfinedSession` or
// `SharedSession`, not this file's modelled `MemorySessionImpl`, so
// `pe_segment_session` (modelled sessions only) answered `None` and every
// downcall skipped the scope check and the acquire. A closed arena's segment
// then reached `marshal_arg` with a freed arena handle (`IllegalStateException:
// ... is a CratonVM arena handle, not a machine address` instead of HotSpot's
// `Already closed`), and a confined segment passed from another thread ran the
// call (`R16FfmsegBind` `wrong-thread: 5` instead of `WrongThreadException`).
// HotSpot's downcall binding acquires each by-reference argument's session
// (`acquire0`) for the call and releases it after; these do the same over the
// real fields, which JDK 25 declares on `MemorySessionImpl` (`owner`, `state`,
// `acquireCount`; `foreign_ffm::p67_session_slots` resolves them by name).

/// Which real JDK session class, by what its `acquire0` / `release0` do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::panama) enum RealSessionKind {
    /// `ConfinedSession`: `checkValidState(); acquireCount++` on the owner.
    Confined,
    /// `SharedSession`: a compare-and-swap on `acquireCount`, `-1` once closed.
    Shared,
    /// `GlobalSession` / `ImplicitSession`: `acquire0` does nothing and the
    /// session is never closed by a call (`Arena.global()`, `Arena.ofAuto()`).
    NoOp,
}

/// `session`'s [`RealSessionKind`], or `None` for any other object (a modelled
/// session, an arena, a class this does not know).
pub(in crate::panama) fn pe_real_session_kind(ctx: &dyn NativeContext, session: ObjectRef) -> Option<RealSessionKind> {
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(session))
        .as_deref()?
    {
        "jdk/internal/foreign/ConfinedSession" => Some(RealSessionKind::Confined),
        "jdk/internal/foreign/SharedSession" => Some(RealSessionKind::Shared),
        "jdk/internal/foreign/GlobalSession" | "jdk/internal/foreign/ImplicitSession" => {
            Some(RealSessionKind::NoOp)
        }
        _ => None,
    }
}

/// The real JDK session governing `seg` (`AbstractMemorySegmentImpl.scope`),
/// when `seg` has no modelled one. Field reads only.
pub(in crate::panama) fn pe_segment_real_session(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
) -> Option<(ObjectRef, RealSessionKind)> {
    let Value::Object(Some(scope)) = ctx.get_field_by_name(seg, "scope") else {
        return None;
    };
    pe_real_session_kind(ctx, scope).map(|kind| (scope, kind))
}

/// `MemorySessionImpl.checkValidState()` of a real session, natively and in
/// the JDK's order: another owner is `WrongThreadException: Attempted access
/// outside owning thread`, then a negative `state` is `IllegalStateException:
/// Already closed` (both `ScopedAccessError.newRuntimeException()` texts).
/// A field that does not read as an `int` / reference passes, as an
/// unresolvable scope always has.
pub(in crate::panama) fn pe_real_session_check(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    if let Value::Object(Some(owner)) = ctx.get_field(session, slots.owner) {
        if !crate::phases_late::foreign_ffm::p67_owner_is_current_thread(ctx, owner) {
            return Err(crate::phases_late::foreign_ffm::p67_wrong_thread(ctx));
        }
    }
    if matches!(ctx.get_field(session, slots.state), Value::Int(state) if state < 0) {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".into(),
        }
        .into());
    }
    Ok(())
}

/// `acquire0()` of a real session for one downcall. Allocates only on a
/// refusal (the exception), so a caller's references stay current on `Ok`.
pub(in crate::panama) fn pe_real_session_acquire(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    kind: RealSessionKind,
) -> Result<(), MethodCallFailed> {
    pe_real_session_check(ctx, session)?;
    if kind == RealSessionKind::NoOp {
        return Ok(());
    }
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    loop {
        let current = ctx.get_field(session, slots.acquires);
        let Value::Int(count) = current else {
            return Ok(());
        };
        // `SharedSession.acquire0`: a negative count is a closed session
        // (`justClose` swaps `0 -> -1`), whose answer is `Already closed`.
        if count < 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Already closed".into(),
            }
            .into());
        }
        if count == i32::MAX {
            return Err(RuntimeError::IllegalStateException {
                message: "Session acquire limit exceeded".into(),
            }
            .into());
        }
        if ctx.compare_and_swap_field(session, slots.acquires, current, Value::Int(count + 1)) {
            return Ok(());
        }
    }
}

/// `release0()` of a real session acquired by [`pe_real_session_acquire`].
/// Total: it runs after the call whatever the call did.
pub(in crate::panama) fn pe_real_session_release(ctx: &mut dyn NativeContext, session: ObjectRef, kind: RealSessionKind) {
    if kind == RealSessionKind::NoOp {
        return;
    }
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    loop {
        let current = ctx.get_field(session, slots.acquires);
        let Value::Int(count) = current else {
            return;
        };
        if count <= 0
            || ctx.compare_and_swap_field(session, slots.acquires, current, Value::Int(count - 1))
        {
            return;
        }
    }
}

/// The zero-length `MemorySegment` that `get(AddressLayout, long)` returns.
///
/// The JDK's contract for an address read is a segment of size 0 at that
/// address -- the caller must `reinterpret` it before dereferencing, which is
/// precisely the safety property that makes the read legal at all. It carries
/// no closeable arena, so no close ever frees memory this VM did not allocate;
/// its scope is the global one (round 14 wave 3, `ffm_address_global_scope`).
///
/// Shape matches `pe_arena_allocate_impl`: `[0]=ptr, [1]=size, [2]=scope,
/// [3]=readOnly, [4]=alive, [5]=offset`.
pub(crate) fn pe_zero_length_segment(
    ctx: &mut dyn NativeContext,
    addr: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    pe_zero_length_segment_with(ctx, addr, false)
}

/// [`pe_zero_length_segment`] with the carrier minted by
/// [`alloc_segment_carrier_with`] (zs s2/nd2: `collecting` only where the
/// caller holds no unpinned reference; the global arena is pinned across).
pub(in crate::panama) fn pe_zero_length_segment_with(
    ctx: &mut dyn NativeContext,
    addr: i64,
    collecting: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // Round 14 wave 3 (lane ffm): in the global scope, as `ofAddress` stamps
    // it (`CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE`). The arena is made (once per
    // VM) BEFORE the carrier and pinned across it. A failure to make it leaves
    // the slot empty, the old answer, rather than failing the read.
    // zs s3/nd3: on the collecting arm the once-per-VM global arena is minted
    // with a collection too (it was the one one-attempt door left here).
    let global = if ffm_address_global_scope() {
        crate::phases_late::foreign_ffm::p67_global_arena_with(ctx, collecting).ok()
    } else {
        None
    };
    let global_pin = global.map(|arena| (ctx.pin_native_root(arena), arena));
    let seg = alloc_segment_carrier_with(ctx, 6, collecting);
    let scope = global_pin.map(|(pin, arena)| {
        let arena = ctx.read_native_pin(pin, arena);
        ctx.unpin_native_roots(pin);
        pe_carrier_scope(ctx, arena)
    });
    let seg = seg?;
    ctx.set_field(seg, 0, Value::Long(addr));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(scope));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(seg)
}

/// `CRATONVM_FFM_ADDRESS_READ_TARGET` (default on; round 13 wave 13, lane
/// ffm7): an ADDRESS read through a layout with a target layout answers a
/// segment of the target's size and refuses a misaligned pointer
/// ([`pe_address_read_segment`]). `0` answers the zero-length segment for every
/// layout, as before.
pub(crate) fn ffm_address_read_target() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// What `MemorySegment.get(AddressLayout, long)` answers for the pointer
/// `raw`: JDK 25 `Utils.longToAddress(raw, pointeeByteSize(layout),
/// pointeeByteAlign(layout))` -- a native segment of the target layout's byte
/// size (0 without a target), and `IllegalArgumentException: Invalid alignment
/// constraint for address: 0x..` for a pointer the target's alignment does not
/// divide (`r13w13-ffm7-address-read-ignores-target-layout`). The target is
/// read before the allocation, so `layout` need not be rooted.
pub(in crate::panama) fn pe_address_read_segment(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
    raw: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    pe_address_read_segment_with(ctx, layout, raw, false)
}

/// [`pe_address_read_segment`] with the mint's door chosen by the caller
/// (zs s2/nd2: `collecting` -- [`ffm_mint_collects`] -- only where nothing
/// raw is held after the target was read, i.e. a downcall's return).
pub(in crate::panama) fn pe_address_read_segment_with(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
    raw: i64,
    collecting: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let (size, align) = if ffm_address_read_target() {
        (
            crate::panama_upcall::address_target_size(ctx, layout),
            crate::panama_upcall::address_target_align(ctx, layout),
        )
    } else {
        (0, 1)
    };
    address_return_segment(ctx, raw, size, align, collecting)
}

/// [`pe_address_read_segment`] once the target's `size` and `align` are known:
/// the alignment refusal, then the global-scope segment of `size` bytes at
/// `raw` ([`pe_zero_length_segment`]). JIT round 18 wave 11 (lane ffmret):
/// shared with the memo paths' `ADDRESS` return
/// ([`memo_address_return_segment`]), so the two answers cannot drift.
///
/// zs s2/nd2: `collecting` ([`ffm_mint_collects`]) makes both native mints
/// (the JDK segment and the carrier) collect and retry on a refusal, then
/// answer the catchable `OutOfMemoryError`; the caller holds no unpinned
/// reference. `false` is the one-attempt door, as before.
pub(in crate::panama) fn address_return_segment(
    ctx: &mut dyn NativeContext,
    raw: i64,
    size: i64,
    align: i64,
    collecting: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // Round 19: a pointer into an arena block the enclosing downcall passed
    // is that block's handle again (`panama_libffi::arena_call_pointer_to_handle`),
    // and so is a pointer into any live arena block read back out of memory
    // (`ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`).
    // Before the alignment check: Java aligned the handle, not the storage.
    let raw = crate::panama_libffi::arena_call_pointer_to_handle(raw);
    if align > 1 && raw & (align - 1) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid alignment constraint for address: 0x{raw:x}"),
        }
        .into());
    }
    if let Some(seg) = pe_jdk_native_segment_with(ctx, raw, size, collecting) {
        return seg;
    }
    // Under `--real-jdk` the segment is the JDK's own: `Utils.longToAddress`
    // (`SegmentFactories.makeNativeSegmentUnchecked`), a real
    // `NativeMemorySegmentImpl` in `MemorySessionImpl.GLOBAL_SESSION`, as on
    // HotSpot. This model's carrier answered another class and a modelled
    // session, so `at.scope().equals(Arena.global().scope())` was false
    // (`R18FfmretShapes` `globalScope`). Built natively first
    // (`pe_jdk_native_segment`, round 19: the JDK body is four field stores);
    // where its shape is not resolved yet, the JDK body; where that class
    // cannot be run, the model answers. `--compatible` too under the
    // `JdkForeignSegments` row ([`ffm_jdk_pointer_segments`]; MEM20-3).
    if ffm_jdk_pointer_segments(ctx) {
        match ctx.invoke_static(
            "jdk/internal/foreign/Utils",
            "longToAddress",
            "(JJJ)Ljava/lang/foreign/MemorySegment;",
            &[Value::Long(raw), Value::Long(size), Value::Long(align)],
        ) {
            Ok(Some(Value::Object(Some(seg)))) => {
                note_compat_pointer_segment(ctx);
                return Ok(seg);
            }
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                return Err(MethodCallFailed::ExceptionThrown(exc))
            }
            _ => {}
        }
    }
    let seg = pe_zero_length_segment_with(ctx, raw, collecting)?;
    if size > 0 {
        ctx.set_field(seg, 1, Value::Long(size));
    }
    Ok(seg)
}

/// Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): under `--real-jdk`,
/// the segment JDK 25's `Utils.longToAddress` makes for a pointer from C --
/// `SegmentFactories.makeNativeSegmentUnchecked(raw, size)`, a
/// `NativeMemorySegmentImpl` in `MemorySessionImpl.GLOBAL_SESSION` -- built
/// natively (its constructors only store the four fields). That is the class
/// and the scope every other segment has in that mode, so
/// `seg.scope() == Arena.global().scope()` holds as on HotSpot (the VM carrier
/// in the model's global arena answered `false`). `None` (the carrier is used)
/// in `--compatible` without the `JdkForeignSegments` row
/// ([`ffm_jdk_pointer_segments`]), or when the classes or the session are not
/// there yet. Allocates: a GC point; the session is read after the allocation.
pub(crate) fn pe_jdk_native_segment(
    ctx: &mut dyn NativeContext,
    raw: i64,
    size: i64,
) -> Option<ObjectRef> {
    // The one-attempt door never answers `Err` (it unwinds or aborts).
    pe_jdk_native_segment_with(ctx, raw, size, false).and_then(Result::ok)
}

/// [`pe_jdk_native_segment`] with the door chosen by the caller (zs s2/nd2):
/// `collecting` ([`ffm_mint_collects`]) collects and retries on a refusal and
/// answers `Some(Err(OutOfMemoryError))` once the ladder is spent; the caller
/// holds no unpinned reference. `None`: not this mode, or the shape is not
/// resolved yet (the caller's fallbacks follow).
pub(crate) fn pe_jdk_native_segment_with(
    ctx: &mut dyn NativeContext,
    raw: i64,
    size: i64,
    collecting: bool,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    let mint = jdk_segment_mint(ctx, collecting)?;
    let seg = match mint.alloc(ctx) {
        Ok(seg) => seg,
        Err(oome) => return Some(Err(oome)),
    };
    let global = mint.global(ctx)?;
    mint.init(ctx, seg, raw, size, global);
    Some(Ok(seg))
}

/// j27 wave 5 (lane FFMUP): [`pe_jdk_native_segment_with`] in its three
/// steps, so a caller minting several segments at once (an up-call's address
/// arguments, `panama_upcall::upcall_dispatch`) resolves the shape and the
/// door once, allocates every segment, and then reads `GLOBAL_SESSION` once
/// for all of them. `None` from [`jdk_segment_mint`]: not this mode, or the
/// shape is not resolved yet (the carrier fallbacks follow).
#[derive(Clone, Copy)]
pub(crate) struct JdkSegmentMint {
    pub(in crate::panama) shape: JdkNativeSegmentShape,
    pub(in crate::panama) collecting: bool,
}

/// The [`JdkSegmentMint`] of this thread's VM, with `collecting` chosen by the
/// caller ([`ffm_mint_collects`]).
pub(crate) fn jdk_segment_mint(ctx: &dyn NativeContext, collecting: bool) -> Option<JdkSegmentMint> {
    if !ffm_jdk_pointer_segments(ctx) {
        return None;
    }
    let shape = jdk_native_segment_shape(ctx)?;
    Some(JdkSegmentMint { shape, collecting })
}

impl JdkSegmentMint {
    /// The exact ordinary-object class used by `alloc`, including after a
    /// moving collection. A successful batch mint may hand this to the
    /// callback memo instead of reading the newly minted object's header.
    pub(crate) fn class_id(&self) -> cratonvm_types::ClassId {
        self.shape.class
    }

    /// A `NativeMemorySegmentImpl` with every field zero (`scope` null):
    /// allocates, a GC point. A refused collecting door answers the
    /// `OutOfMemoryError`; the one-attempt door never answers `Err`.
    pub(crate) fn alloc(&self, ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
        let (class, n) = (self.shape.class, self.shape.fields);
        if self.collecting {
            return ffm_alloc_object_collecting(
                ctx,
                class,
                n,
                "jdk/internal/foreign/NativeMemorySegmentImpl",
            );
        }
        Ok(ctx
            .try_alloc_object_gc_safe(class, n)
            .unwrap_or_else(|| ctx.alloc_object(class, n)))
    }

    /// `MemorySessionImpl.GLOBAL_SESSION`, `None` before it is set. A raw
    /// reference: read it after the last allocation it must survive.
    pub(crate) fn global(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        match ctx.get_static_field(self.shape.session_class, self.shape.global) {
            Value::Object(Some(global)) => Some(global),
            _ => None,
        }
    }

    /// The constructor's four stores into an [`Self::alloc`]ed `seg`: `raw`,
    /// `size`, writable, in `global`'s scope. No allocation.
    pub(crate) fn init(
        &self,
        ctx: &mut dyn NativeContext,
        seg: ObjectRef,
        raw: i64,
        size: i64,
        global: ObjectRef,
    ) {
        let shape = &self.shape;
        // j27 wave 2 (lane INVOKE; `CRATONVM_FFM_SEGMENT_MINT_ONE_BATCH`): the
        // `scope` reference joins the batch (the batch door's reference store
        // runs the same SATB pre-barrier and typed store as `set_field`), so a
        // mint pays one door, not two -- two per `(MemorySegment,
        // MemorySegment)` up-call.
        if ffm_segment_mint_one_batch_enabled() {
            ctx.set_fields_typed(
                seg,
                &[
                    (shape.min, b'J'),
                    (shape.length, b'J'),
                    (shape.read_only, b'Z'),
                    (shape.scope, b'L'),
                ],
                &[
                    Value::Long(raw),
                    Value::Long(size),
                    Value::Int(0),
                    Value::Object(Some(global)),
                ],
            );
            note_compat_pointer_segment(ctx);
            return;
        }
        // JIT round 22 (lane memory, MEM20-6 direction 5): the three primitive
        // stores in one batch (one receiver canonicalisation and one layout
        // resolution instead of three); the reference store keeps its barrier.
        ctx.set_fields_typed(
            seg,
            &[
                (shape.min, b'J'),
                (shape.length, b'J'),
                (shape.read_only, b'Z'),
            ],
            &[Value::Long(raw), Value::Long(size), Value::Int(0)],
        );
        ctx.set_field(seg, shape.scope, Value::Object(Some(global)));
        note_compat_pointer_segment(ctx);
    }
}

/// `CRATONVM_FFM_SEGMENT_MINT_ONE_BATCH` -- perf, default ON, `=0` off (j27 wave 2,
/// lane INVOKE; page `docs/feature-designs/native-dispatch-architecture.md`
/// item 2): [`pe_jdk_native_segment_with`] stores the segment's `scope` in the same
/// typed batch as its three primitives. Asked per mint: memoised.
pub(in crate::panama) fn ffm_segment_mint_one_batch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// MEM20-3 (JIT round 20 wave 19, lane yCompatSeg): is a pointer from C
/// (a downcall's `ADDRESS` return, an upcall's address argument, a native
/// `get(AddressLayout, long)`) the JDK's own `NativeMemorySegmentImpl` in
/// `MemorySessionImpl.GLOBAL_SESSION` ([`pe_jdk_native_segment`],
/// [`address_return_segment`]) rather than the VM's carrier? `--real-jdk`
/// always; `--compatible` with the `JdkForeignSegments` row AND the
/// `JdkForeign` row, because the JDK segment is only answered where every
/// other segment is the JDK's too (with `-jdk-foreign` the model's natives
/// read segments by the carrier's slot layout, so a real segment among them
/// would be misread). Either row off restores the carrier mint exactly.
pub(crate) fn ffm_jdk_pointer_segments(ctx: &dyn NativeContext) -> bool {
    ctx.is_jdk_only()
        || (ctx.aligned(cratonvm_types::HotSpotAlignment::JdkForeign)
            && ctx.aligned(cratonvm_types::HotSpotAlignment::JdkForeignSegments))
}

/// [`ffm_jdk_pointer_segments`]'s census: `--compatible` minted the JDK
/// segment where the row-off route mints the carrier (the
/// `jdk-foreign-segments=` count of the `CRATONVM_DBG=access` exit line).
pub(in crate::panama) fn note_compat_pointer_segment(ctx: &dyn NativeContext) {
    if !ctx.is_jdk_only() {
        ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkForeignSegments);
    }
}

/// [`pe_jdk_native_segment`]'s class ids and slots, resolved once per VM and
/// thread (ids and indices only; both classes are bootstrap classes).
#[derive(Clone, Copy)]
pub(in crate::panama) struct JdkNativeSegmentShape {
    pub(in crate::panama) vm: usize,
    pub(in crate::panama) class: cratonvm_types::ClassId,
    pub(in crate::panama) session_class: cratonvm_types::ClassId,
    pub(in crate::panama) global: usize,
    pub(in crate::panama) min: usize,
    pub(in crate::panama) length: usize,
    pub(in crate::panama) read_only: usize,
    pub(in crate::panama) scope: usize,
    /// The instance width `class_num_total_fields` answers (a bootstrap
    /// class: fixed; it was asked on every mint, ~2 % of a pointer return).
    pub(in crate::panama) fields: usize,
}

thread_local! {
    pub(in crate::panama) static JDK_NATIVE_SEGMENT_SHAPE: std::cell::Cell<Option<JdkNativeSegmentShape>> =
        const { std::cell::Cell::new(None) };
}

pub(in crate::panama) fn jdk_native_segment_shape(ctx: &dyn NativeContext) -> Option<JdkNativeSegmentShape> {
    let vm = ctx.vm_identity();
    if let Some(shape) = JDK_NATIVE_SEGMENT_SHAPE.with(std::cell::Cell::get) {
        if shape.vm == vm && !cfg!(test) {
            return Some(shape);
        }
    }
    let class = ctx.class_id_by_name("jdk/internal/foreign/NativeMemorySegmentImpl")?;
    let session_class = ctx.class_id_by_name("jdk/internal/foreign/MemorySessionImpl")?;
    let shape = JdkNativeSegmentShape {
        vm,
        class,
        session_class,
        global: ctx.static_field_index_by_name(session_class, "GLOBAL_SESSION")?,
        min: ctx.resolve_field_index_by_class_id(class, "min")?,
        length: ctx.resolve_field_index_by_class_id(class, "length")?,
        read_only: ctx.resolve_field_index_by_class_id(class, "readOnly")?,
        scope: ctx.resolve_field_index_by_class_id(class, "scope")?,
        fields: ctx.class_num_total_fields(class),
    };
    JDK_NATIVE_SEGMENT_SHAPE.with(|c| c.set(Some(shape)));
    Some(shape)
}

/// JIT round 18 wave 11 (lane ffmret; FFMCOST18-2): the segment an
/// [`PLAIN_MEMO_RET_ADDRESS`] encoding answers for the pointer `raw` the C
/// function returned: the general path's answer for the target the
/// classification recorded ([`memo_address_return_bits`]: none, or an
/// unbounded byte sequence; both alignment 1). Allocates: a GC point, so the
/// caller holds no raw reference across it.
pub(in crate::panama) fn memo_address_return_segment(
    ctx: &mut dyn NativeContext,
    memo: i64,
    raw: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let size = if memo & PLAIN_MEMO_ADDRESS_UNBOUNDED != 0 {
        i64::MAX
    } else {
        0
    };
    // zs s2/nd2: after every release, nothing raw held (the doc above).
    // Merge with dev (1d682c68d2): only scalar registers live here and the
    // sessions have been released, so the mint reclaims and retries without
    // re-running C on every collector. zt t1/nd: except on ZGC under its kill
    // switch ([`memo_address_return_collects`]).
    let collecting = memo_address_return_collects(ctx.heap_is_zgc(), ffm_mint_collects(ctx));
    address_return_segment(ctx, raw, size, 1, collecting)
}

/// zt t1/nd (the zs/dev merge regression): may the memo paths' `ADDRESS`
/// return mint ([`memo_address_return_segment`]) collect? Dev (1d682c68d2)
/// collects there on every collector, so Generational and G1 always answer
/// `true`; on ZGC the answer is zs s2/nd2's [`ffm_mint_collects`] again, so
/// `CRATONVM_ZGC_FFM_MINT_COLLECT=0` (or `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0`)
/// restores the one-attempt door there, as on the general path. The merge had
/// made the memo site collect unconditionally, so the documented ZGC kill no
/// longer reached it.
pub(in crate::panama) fn memo_address_return_collects(heap_is_zgc: bool, ffm_mint_collects: bool) -> bool {
    !heap_is_zgc || ffm_mint_collects
}

/// JIT round 18 wave 11 (lane ffmret): an encoding's Java result from the raw
/// return registers / slot `ret`, for every return kind: a primitive or `null`
/// for void ([`memo_return_value`]), the minted segment of an `ADDRESS`
/// return, and for a by-value group the segment `allocator` answers with the
/// group's bytes copied in ([`downcall_copy_group_return_in`]; `group` is the
/// size, the alignment and the in-registers verdict, from the cached CIF).
/// Runs after every release: the allocations and the
/// allocator call are GC points, and `allocator` must be current.
pub(in crate::panama) fn memo_finish_return(
    ctx: &mut dyn NativeContext,
    memo: i64,
    ret: [u64; 2],
    allocator: Option<ObjectRef>,
    group: (usize, usize, bool, Option<usize>),
) -> MethodCallResult {
    if memo & PLAIN_MEMO_RET_ADDRESS != 0 {
        // The general path's read of an address return (`0` for anything
        // that is not a `long`).
        let raw = match memo_return_value(memo, ret) {
            Value::Long(raw) => raw,
            _ => 0,
        };
        return memo_address_return_segment(ctx, memo, raw)
            .map(|seg| Some(Value::Object(Some(seg))));
    }
    if memo & PLAIN_MEMO_RET_GROUP != 0 {
        let Some(allocator) = allocator else {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        };
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&ret[0].to_ne_bytes());
        bytes[8..].copy_from_slice(&ret[1].to_ne_bytes());
        let (size, align, in_registers, hfa_width) = group;
        return downcall_copy_group_return_in(
            ctx,
            allocator,
            size,
            align,
            &bytes[..size.min(16)],
            in_registers,
            hfa_width,
        )
        .map(|seg| Some(Value::Object(Some(seg))));
    }
    Ok(Some(memo_return_value(memo, ret)))
}

// ===================================================================
// Heap segments: a `MemorySegment` whose bytes live in a Java array
// ===================================================================
//
// F27 (2026-08-13) established that this file could not read or write one at
// all, and that `segment_address` was answering `new byte[16].length` — 16, the
// `read at address 0x10` W7-89 §7.1 records and misattributes to a
// `Buffer.address` read. F27 turned that fatal SIGSEGV into
// `IllegalStateException: Null segment address`, which is an improvement and
// not a fix: the JDK's own answer is to read and write the backing array.
//
// TWO CARRIERS reach the code below.
//
//   H1 — a real JDK `jdk.internal.foreign.HeapMemorySegmentImpl$Of*`. Measured
//        (25.0.3+9-LTS): five fields, `length`/`readOnly`/`scope` from
//        `AbstractMemorySegmentImpl` then `offset`/`base`. `offset` is
//        `Unsafe`-style — see [`HEAP_ARRAY_BASE_OFFSET`]. `--real-jdk` gets one
//        of these from `MemorySegment.ofBuffer(ByteBuffer.allocate(n))`
//        (measured: a `HeapMemorySegmentImpl$OfByte`) and from `asSlice(long)`
//        on a real segment `ofArray` never touches.
//
//   H2 — a CratonVM-minted alias carrier, `[6]=array, [7]=byte start`, slot 0
//        deliberately 0. `MemorySegment.ofArray(...)` mints one directly for
//        all SEVEN primitive array types (`pe_of_array_alias`); `asSlice`
//        mints one whenever its receiver is any heap segment, H1 included —
//        before that it computed `segment_address(this) + offset` and
//        stamped the RESULT into slot 0, so a slice of a heap segment was a
//        synthetic segment whose "address" was its offset — F27's fix made
//        the un-sliced read refuse and left `asSlice(3, 4).get(...)`
//        dereferencing the literal address 3.
//
// A third shape, H3 — a native-mirror carrier `ofArray(int[]/long[]/float[]/
// double[])` used to mint instead of H2, so those four types could be handed
// straight to a downcall — is retired as of 2026-09-21; see the `ofArray`
// registration site and `pe_downcall_invoke`'s scratch-buffer pass for what
// replaced it.

/// A `MemorySegment` whose bytes live in a Java primitive array, resolved into
/// the pieces an access needs.
///
/// `pub(crate)`: also the heap-addressing resolver for the FFM layout
/// `VarHandle` path (`lang_invoke::layout_vh_access`), which used to refuse
/// EVERY heap-backed segment outright — see
/// `docs/feature-designs/native-dispatch-architecture.md
/// ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`.
#[derive(Clone, Copy)]
pub(crate) struct HeapSegmentView {
    /// The Java primitive array holding the bytes.
    pub(crate) base: ObjectRef,
    /// Byte offset of the segment's first byte within `base`'s DATA — i.e.
    /// the JDK's `address()`, not its `offset` field.
    pub(crate) start: i64,
    /// The segment's `byteSize`.
    pub(in crate::panama) size: i64,
    pub(in crate::panama) read_only: bool,
    /// 1, 2, 4 or 8 — the width of one `base` element.
    pub(crate) elem_width: usize,
    pub(crate) elem_type: cratonvm_types::ArrayElementType,
}

impl HeapSegmentView {
    /// The segment's `byteSize` (JIT round 23 wave 2c, lane L8: the lean
    /// `SegmentVarHandle` heap road's bounds check).
    pub(crate) fn size(&self) -> i64 {
        self.size
    }

    /// The segment is read-only.
    pub(crate) fn read_only(&self) -> bool {
        self.read_only
    }
}

/// Read a named field off either heap carrier.
///
/// `get_field_by_name` answers `Value::Object(None)` both for a name it cannot
/// resolve and for a null reference field, so a miss falls back to the
/// class-side field table — the resolution `lang_invoke.rs::segment_raw_access`
/// uses for these same two fields.
pub(in crate::panama) fn heap_seg_field(ctx: &dyn NativeContext, seg: ObjectRef, name: &str) -> Value {
    match ctx.get_field_by_name(seg, name) {
        Value::Object(None) => {
            match ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(seg), name) {
                Some(index) => ctx.get_field(seg, index),
                None => Value::Object(None),
            }
        }
        other => other,
    }
}

/// Byte width of one array element, or `None` for a shape that cannot back a
/// `MemorySegment`.
///
/// `Reference` is refused because there is no byte view of an object array.
/// `Boolean` is refused because `java.lang.foreign.MemorySegment` has **no**
/// `ofArray(boolean[])` overload — measured, `NoSuchMethodException:
/// java.lang.foreign.MemorySegment.ofArray([Z)` — so a `boolean[]`-backed
/// segment is not a thing the JDK can produce, and inventing a byte semantics
/// for one is how a defaulting reader becomes a quiet wrong write.
pub(crate) fn heap_element_width(elem: cratonvm_types::ArrayElementType) -> Option<usize> {
    use cratonvm_types::ArrayElementType as A;
    Some(match elem {
        A::Byte => 1,
        A::Char | A::Short => 2,
        A::Int | A::Float => 4,
        A::Long | A::Double => 8,
        A::Boolean | A::Reference => return None,
    })
}

/// Read `len` bytes of a heap segment's payload, starting `offset` bytes
/// into the segment.
///
/// A heap segment has no address, so `MemorySegment.copy`'s pointer path
/// cannot see it at all — which is exactly how a copy INTO one used to
/// write nothing at all and say nothing about it. The bytes have to come
/// out of (or go into) the Java array itself.
///
/// The `Byte` and `Int` arms go through the bulk array accessors, which
/// the VM implements as one `copy_nonoverlapping` over the heap arena;
/// everything else, and any access not aligned to its element width,
/// falls back to an element-at-a-time loop that is correct for every
/// width and offset. That fallback is why this returns bytes rather than
/// borrowing them: an unaligned read spans element boundaries.
pub(crate) fn heap_read_bytes(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    len: usize,
) -> Option<Vec<u8>> {
    let start = view.start.checked_add(offset)?;
    if start < 0 || len == 0 {
        return if len == 0 { Some(Vec::new()) } else { None };
    }
    let start = start as usize;
    let width = view.elem_width;
    if width == 1 && view.elem_type == cratonvm_types::ArrayElementType::Byte {
        let mut out = vec![0u8; len];
        let got = ctx.read_byte_array_into(view.base, start, &mut out);
        return (got == len).then_some(out);
    }
    if width == 4
        && view.elem_type == cratonvm_types::ArrayElementType::Int
        && start % 4 == 0
        && len % 4 == 0
    {
        let mut words = vec![0i32; len / 4];
        let got = ctx.read_int_array_into(view.base, start / 4, &mut words);
        if got != words.len() {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        return Some(out);
    }
    // General case: walk the elements the byte range touches and take the
    // bytes out of each one's little-endian image.
    let first = start / width;
    let last = (start + len - 1) / width;
    let mut staged = Vec::with_capacity((last - first + 1) * width);
    for index in first..=last {
        staged.extend_from_slice(&heap_element_le_bytes(ctx, view, index)?);
    }
    let skip = start - first * width;
    Some(staged[skip..skip + len].to_vec())
}

/// Write `src` into a heap segment's payload at `offset`. Mirror of
/// [`heap_read_bytes`]; an unaligned or sub-element write reads the
/// element it lands in, patches the bytes, and writes it back.
pub(crate) fn heap_write_bytes(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    src: &[u8],
) -> bool {
    if view.read_only {
        return false;
    }
    let Some(start) = view.start.checked_add(offset) else {
        return false;
    };
    if start < 0 {
        return false;
    }
    if src.is_empty() {
        return true;
    }
    let start = start as usize;
    let width = view.elem_width;
    if width == 1 && view.elem_type == cratonvm_types::ArrayElementType::Byte {
        return ctx.write_byte_array_from(view.base, start, src);
    }
    if width == 4
        && view.elem_type == cratonvm_types::ArrayElementType::Int
        && start % 4 == 0
        && src.len() % 4 == 0
    {
        let words: Vec<i32> = src
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        return ctx.write_int_array_from(view.base, start / 4, &words);
    }
    let first = start / width;
    let last = (start + src.len() - 1) / width;
    let mut staged = Vec::with_capacity((last - first + 1) * width);
    for index in first..=last {
        match heap_element_le_bytes(ctx, view, index) {
            Some(bytes) => staged.extend_from_slice(&bytes),
            None => return false,
        }
    }
    let skip = start - first * width;
    staged[skip..skip + src.len()].copy_from_slice(src);
    for (n, index) in (first..=last).enumerate() {
        let chunk = &staged[n * width..(n + 1) * width];
        if !heap_element_from_le_bytes(ctx, view, index, chunk) {
            return false;
        }
    }
    true
}

/// `CRATONVM_FFM_BULK_HEAP_SEGMENTS` (default on): the bulk segment natives
/// that only knew the raw-address path -- `copyFrom`, `mismatch`,
/// `getString`, `setString`, `asOverlappingSlice`, the segment-to-array
/// `copy` -- serve a heap (`ofArray`) segment through its backing array
/// instead of silently doing nothing, `copyFrom` accepts a destination larger
/// than its source (the JDK's `copy(src, 0, this, 0, src.byteSize())`), and a
/// negative copy length or element count is `IndexOutOfBoundsException`
/// (round 12 wave 6, lane ffm3). Off restores the old answers.
pub(crate) fn ffm_bulk_heap_segments() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Whether either segment is heap-backed, i.e. has no address a raw-pointer
/// path could use.
pub(in crate::panama) fn pe_any_heap_segment(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> bool {
    heap_segment_view(ctx, a).is_some() || heap_segment_view(ctx, b).is_some()
}

/// `len` bytes of `seg` from `offset`, whichever carrier it is: a heap
/// segment through its backing array, a native one through its address.
/// `None` when the range is not inside the segment or the segment resolves
/// neither way. No Java allocation, no safepoint.
pub(crate) fn pe_segment_read_bytes(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    len: usize,
) -> Option<Vec<u8>> {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let end = offset.checked_add(i64::try_from(len).ok()?)?;
    if offset < 0 || end > size {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }
    if let Some(view) = heap_segment_view(ctx, seg) {
        return heap_read_bytes(ctx, &view, offset, len);
    }
    let base = crate::panama_libffi::segment_host_address(ctx, seg);
    if base == 0 {
        return None;
    }
    let addr = (base as u64).checked_add(offset as u64)?;
    let mut out = vec![0u8; len];
    // SAFETY: `[offset, offset + len)` was checked against the segment's
    // size above, and `out` is a fresh buffer of exactly `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), len) };
    Some(out)
}

/// Write-side twin of [`pe_segment_read_bytes`]. `false` when the range is
/// not inside the segment, the segment resolves neither way, or a heap
/// segment is read-only.
pub(in crate::panama) fn pe_segment_write_bytes(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    src: &[u8],
) -> bool {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let Some(end) = i64::try_from(src.len())
        .ok()
        .and_then(|len| offset.checked_add(len))
    else {
        return false;
    };
    if offset < 0 || end > size {
        return false;
    }
    if src.is_empty() {
        return true;
    }
    if let Some(view) = heap_segment_view(ctx, seg) {
        return heap_write_bytes(ctx, &view, offset, src);
    }
    let base = crate::panama_libffi::segment_host_address(ctx, seg);
    let Some(addr) = (base as u64)
        .checked_add(offset as u64)
        .filter(|_| base != 0)
    else {
        return false;
    };
    // SAFETY: bounds-checked above; `src` is an owned buffer distinct from
    // the segment's block.
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), addr as *mut u8, src.len()) };
    true
}

/// `MemorySegment.mismatch`'s answer over the first `common` bytes of two
/// segments of `a_len` and `b_len` bytes: the first differing index; else
/// the common length when one is a prefix of the other; else -1.
pub(in crate::panama) fn pe_first_mismatch(a: &[u8], b: &[u8], a_len: i64, b_len: i64, common: i64) -> i64 {
    match a.iter().zip(b.iter()).position(|(x, y)| x != y) {
        Some(i) => i as i64,
        None if a_len == b_len => -1,
        None => common,
    }
}

/// One array element's little-endian byte image, whatever its width.
pub(in crate::panama) fn heap_element_le_bytes(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    index: usize,
) -> Option<Vec<u8>> {
    if index >= ctx.array_length(view.base) {
        return None;
    }
    let bytes = match ctx.get_array_element(view.base, index) {
        Value::Int(v) => match view.elem_width {
            1 => vec![v as u8],
            2 => (v as u16).to_le_bytes().to_vec(),
            4 => v.to_le_bytes().to_vec(),
            // An `Int`-typed answer for an 8-byte element (a zeroed slot from
            // an implementation that does not type its defaults): widen it,
            // so the image is still one element wide.
            _ => i64::from(v).to_le_bytes().to_vec(),
        },
        Value::Long(v) => v.to_le_bytes().to_vec(),
        Value::Float(v) => v.to_bits().to_le_bytes().to_vec(),
        Value::Double(v) => v.to_bits().to_le_bytes().to_vec(),
        _ => return None,
    };
    // Exactly one element wide, or nothing: `heap_read_bytes` /
    // `heap_write_bytes` slice the staged images by `elem_width`, and a short
    // image would be an out-of-range slice panic there (JIT round 18 wave 11
    // close, lane testfix18).
    (bytes.len() == view.elem_width).then_some(bytes)
}

/// Inverse of [`heap_element_le_bytes`].
pub(in crate::panama) fn heap_element_from_le_bytes(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    index: usize,
    bytes: &[u8],
) -> bool {
    use cratonvm_types::ArrayElementType as A;
    if index >= ctx.array_length(view.base) || bytes.len() != view.elem_width {
        return false;
    }
    let value = match view.elem_type {
        A::Byte => Value::Int(bytes[0] as i8 as i32),
        A::Short => Value::Int(i16::from_le_bytes([bytes[0], bytes[1]]) as i32),
        A::Char => Value::Int(u16::from_le_bytes([bytes[0], bytes[1]]) as i32),
        A::Int => Value::Int(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        A::Float => Value::Float(f32::from_bits(u32::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        A::Long => {
            let mut w = [0u8; 8];
            w.copy_from_slice(bytes);
            Value::Long(i64::from_le_bytes(w))
        }
        A::Double => {
            let mut w = [0u8; 8];
            w.copy_from_slice(bytes);
            Value::Double(f64::from_bits(u64::from_le_bytes(w)))
        }
        A::Boolean | A::Reference => return false,
    };
    ctx.set_array_element(view.base, index, value);
    true
}

/// Resolve `seg` if — and only if — it is a heap segment (H1 or H2).
///
/// `None` means "not a heap segment", which every caller reads as "take the
/// raw-address path". A heap segment whose shape does not add up (a `base`
/// that is not an array, a negative start, a size that runs off the end of the
/// array) also answers `None` rather than a plausible view; the caller turns
/// that into a named refusal.
///
/// A third shape, H3 — a six/seven-field MIRROR carrier that
/// `ofArray(int[]|long[]|float[]|double[])` used to mint, with the array in
/// [`SEG_BACKING_ARRAY_FIELD`] instead of [`SEG_HEAP_BASE_FIELD`] — was
/// recognized here between 2026-09-20 and 2026-09-21. `ofArray` no longer
/// mints that shape (all seven primitive array types now share H2, the same
/// carrier `asSlice` already normalized every heap segment down to), so the
/// arm was retired along with [`SEG_BACKING_ARRAY_FIELD`]'s minting sites.
pub(crate) fn heap_segment_view(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
) -> Option<HeapSegmentView> {
    let (base, start) = if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
        let base = match heap_seg_field(ctx, seg, "base") {
            Value::Object(Some(array)) => array,
            _ => return None,
        };
        let raw_offset = match heap_seg_field(ctx, seg, "offset") {
            Value::Long(n) => n,
            Value::Int(n) => i64::from(n),
            _ => return None,
        };
        (
            base,
            crate::panama_libffi::heap_segment_byte_start(raw_offset),
        )
    } else if ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD {
        let base = match ctx.get_field(seg, SEG_HEAP_BASE_FIELD) {
            Value::Object(Some(array)) => array,
            _ => return None,
        };
        match ctx.get_field(seg, SEG_HEAP_START_FIELD) {
            Value::Long(n) => (base, n),
            _ => return None,
        }
    } else {
        return None;
    };
    if start < 0 || !ctx.object_is_array(base) {
        return None;
    }
    let elem_type = ctx.heap_element_type_of(base);
    let elem_width = heap_element_width(elem_type)?;
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    // The whole segment must lie inside the array. Every index computed below
    // is derived from `start + offset` with `offset + width <= size`, so this
    // one check is what keeps `get_array_element` in range without a per-byte
    // bound.
    let array_bytes = (ctx.array_length(base) as i64).saturating_mul(elem_width as i64);
    if size < 0 || start.saturating_add(size) > array_bytes {
        return None;
    }
    // Slot 3 is the read-write flag on H2's eight-field shape (the factory
    // writes `3: Int(0) // read-write`), so the field-count gate only needs
    // to keep it off the real-JDK arm (H1), which answers through
    // `heap_seg_field`'s name resolution instead.
    let read_only = matches!(heap_seg_field(ctx, seg, "readOnly"), Value::Int(n) if n != 0)
        || (ctx.object_num_fields(seg) > 3
            && matches!(ctx.get_field(seg, 3), Value::Int(n) if n != 0));
    Some(HeapSegmentView {
        base,
        start,
        size,
        read_only,
        elem_width,
        elem_type,
    })
}

/// Whether `seg` is a heap-backed segment with NO native buffer behind it: a
/// real JDK `HeapMemorySegmentImpl` (H1), or this VM's `ofArray` ALIAS carrier
/// for `byte[]`/`short[]`/`char[]`/`int[]`/`long[]`/`float[]`/`double[]` (H2,
/// [`pe_of_array_alias`], which mints all seven now).
///
/// MEASURED on HotSpot 25.0.3+9-LTS (`FfmRepro2.java`): passing either a
/// `byte[]` or an `int[]` heap segment straight to a downcall throws, in both
/// cases —
///
/// ```text
/// java.lang.IllegalArgumentException: Heap segment not allowed:
///     MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 3 }
/// ```
///
/// so the oracle draws no distinction between element types at all; CratonVM
/// is permissive for int/long/float/double only under the opt-in
/// `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT=1` (j27 wave 4), and
/// only at the downcall boundary
/// itself (`pe_downcall_invoke`'s scratch-buffer pass runs BEFORE this
/// predicate's caller, `marshal_arg`, ever sees those four — see that pass's
/// own comment), not by making their `ofArray` segment a different shape. Once
/// a heap segment reaches `marshal_arg` directly, it is refused regardless of
/// element type. Before this predicate existed, `marshal_arg`'s
/// downcall-pointer refusal checked only
/// [`crate::panama_libffi::is_real_heap_segment`] (H1), so an H2 alias
/// carrier fell through to `segment_address`, which answers 0 for one — a
/// legitimate C null the marshaller deliberately lets through — and a
/// `MemorySegment.ofArray(byte[])` handed to a downcall silently became
/// `NULL` instead of the oracle's refusal.
///
/// Equivalent to [`is_heap_shaped_segment`] as of 2026-09-21 (both are
/// exactly "H1 or H2" now that H3 is retired); kept as a separate, cheaper
/// field-count-only check because that is what its one caller
/// (`marshal_arg`'s hot downcall-argument path) has always wanted.
pub(crate) fn is_alias_heap_segment(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    crate::panama_libffi::is_real_heap_segment(ctx, seg)
        || ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
}

/// Whether `seg` is ANY heap-shaped segment this crate recognizes — H1 or H2
/// (see [`is_alias_heap_segment`]'s doc for why there were briefly three).
///
/// MEASURED on HotSpot 25.0.3+9-LTS: `MemorySegment.ofArray(new
/// int[8]).address()` is 0, the same as `ofArray(new byte[8]).address()` —
/// `FfmRepro2`-style probing once found `p67_segment_address`
/// (`phases_late/foreign_ffm.rs`, the registration behind the Java-visible
/// `MemorySegment.address()`) answering a since-retired mirror carrier's real
/// native pointer instead, because its shape-detection only ever recognised
/// "6 or more fields" as the off-heap `(ptr, size, …, offset)` carrier that a
/// PLAIN native (`Arena`-backed) segment also is. A caller that needs the
/// universal "heap segments have no machine address" rule wants this
/// predicate — it validates the FULL shape via [`heap_segment_view`], not
/// just a field count.
pub(crate) fn is_heap_shaped_segment(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    crate::panama_libffi::is_real_heap_segment(ctx, seg) || heap_segment_view(ctx, seg).is_some()
}

/// The raw bits of one array element, zero-extended into a `u64`.
pub(crate) fn heap_element_bits(ctx: &dyn NativeContext, base: ObjectRef, index: usize) -> u64 {
    match ctx.get_array_element(base, index) {
        // A `byte`/`short`/`char`/`int` element arrives as `Value::Int`; the
        // `as u32` is what stops a negative byte from smearing 0xFF across the
        // upper 56 bits and corrupting the neighbouring bytes of a wider read.
        Value::Int(v) => u64::from(v as u32),
        Value::Long(v) => v as u64,
        Value::Float(v) => u64::from(v.to_bits()),
        Value::Double(v) => v.to_bits(),
        _ => 0,
    }
}

/// Store the raw bits of one array element back, in that array's own encoding.
///
/// Takes `base`/`elem_type` directly rather than a [`HeapSegmentView`] so
/// [`heap_bytes_write`] can serve a caller (`lang_invoke.rs::segment_heap_set`)
/// that has an array and an `ArrayElementType` but never builds a view.
pub(crate) fn heap_store_element(
    ctx: &dyn NativeContext,
    base: ObjectRef,
    elem_type: cratonvm_types::ArrayElementType,
    index: usize,
    bits: u64,
) {
    use cratonvm_types::ArrayElementType as A;
    let value = match elem_type {
        A::Byte => Value::Int(i32::from(bits as u8 as i8)),
        A::Char => Value::Int(i32::from(bits as u16)),
        A::Short => Value::Int(i32::from(bits as u16 as i16)),
        A::Int => Value::Int(bits as u32 as i32),
        A::Long => Value::Long(bits as i64),
        A::Float => Value::Float(f32::from_bits(bits as u32)),
        A::Double => Value::Double(f64::from_bits(bits)),
        // Unreachable: `heap_element_width` refused both, so no view exists.
        A::Boolean | A::Reference => return,
    };
    ctx.set_array_element(base, index, value);
}

/// Read `width` bytes at `offset` bytes into the segment, LITTLE-ENDIAN.
///
/// Endianness is measured, not assumed: on the oracle,
/// `MemorySegment.ofArray(new byte[8]).set(JAVA_INT_UNALIGNED, 0, 0x01020304)`
/// leaves `[4, 3, 2, 1, 0, 0, 0, 0]`, and the same write into a `short[4]` with
/// `JAVA_LONG_UNALIGNED 0x0102030405060708` leaves `[0x0708, 0x0506, 0x0304,
/// 0x0102]` — so a byte index runs low-to-high through the element AND through
/// the array.
///
/// The caller has already bounds-checked `offset + width <= view.size`, and
/// [`heap_segment_view`] has checked `start + size <= array bytes`, so every
/// index here is in range.
pub(in crate::panama) fn heap_segment_read(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    width: usize,
) -> u64 {
    heap_bytes_read(ctx, view.base, view.elem_width, view.start + offset, width)
}

/// The element-width-aware core of [`heap_segment_read`], taking the backing
/// array and its element width directly rather than a [`HeapSegmentView`] —
/// so `lang_invoke.rs::segment_heap_get`'s `SegmentVarHandle` path can share
/// it instead of carrying its own byte-array-only copy (NOM F35-3: that copy
/// read every backing array as if it were a `byte[]`, so a `VarHandle` over
/// an `int[]`-backed `MemorySegment` read element `k` where it should have
/// read byte `k`).
pub(crate) fn heap_bytes_read(
    ctx: &dyn NativeContext,
    base: ObjectRef,
    elem_width: usize,
    byte_start: i64,
    width: usize,
) -> u64 {
    let mut raw: u64 = 0;
    for i in 0..width {
        let byte_index = byte_start as usize + i;
        let element = byte_index / elem_width;
        let shift_in = byte_index % elem_width;
        let byte = (heap_element_bits(ctx, base, element) >> (8 * shift_in)) & 0xff;
        raw |= byte << (8 * i);
    }
    raw
}

/// Write `width` bytes at `offset` bytes into the segment, little-endian.
///
/// Read-modify-write per byte rather than per element: an access is at most 8
/// bytes wide and may start and end mid-element (measured: on an `int[4]`
/// segment `set(JAVA_BYTE, 0, 0x7f)` leaves `iarr[0] == 127`, i.e. byte 0 is
/// the element's low byte and the other three are untouched).
pub(in crate::panama) fn heap_segment_write(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    width: usize,
    raw: u64,
) {
    heap_bytes_write(
        ctx,
        view.base,
        view.elem_type,
        view.elem_width,
        view.start + offset,
        width,
        raw,
    );
}

/// The element-width-aware core of [`heap_segment_write`] — see
/// [`heap_bytes_read`], its read-side twin.
pub(crate) fn heap_bytes_write(
    ctx: &dyn NativeContext,
    base: ObjectRef,
    elem_type: cratonvm_types::ArrayElementType,
    elem_width: usize,
    byte_start: i64,
    width: usize,
    raw: u64,
) {
    for i in 0..width {
        let byte_index = byte_start as usize + i;
        let element = byte_index / elem_width;
        let shift_in = 8 * (byte_index % elem_width);
        let byte = (raw >> (8 * i)) & 0xff;
        let bits = heap_element_bits(ctx, base, element);
        heap_store_element(
            ctx,
            base,
            elem_type,
            element,
            (bits & !(0xffu64 << shift_in)) | (byte << shift_in),
        );
    }
}

/// Validate a heap-segment access: scope, read-only, bounds, alignment.
///
/// **Every exception class here is the oracle's**, measured on 25.0.3+9-LTS:
///
/// | call | thrown |
/// |---|---|
/// | `ofArray(byte[8]).get(JAVA_INT_UNALIGNED, 6)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[8]).get(JAVA_BYTE, -1)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[0]).get(JAVA_BYTE, 0)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[8]).asReadOnly().set(JAVA_BYTE, 0, 1)` | `IllegalArgumentException: Attempt to write a read-only segment` |
/// | `ofArray(byte[32]).get(JAVA_INT, 0)` | `IllegalArgumentException: Target offset 0 is incompatible with alignment constraint 4 (of i4) …` |
/// | `ofArray(int[8]).get(JAVA_INT, 1)` | `IllegalArgumentException` (offset not a multiple of 4) |
/// | `ofArray(int[8]).get(JAVA_LONG, 0)` | `IllegalArgumentException` (`maxByteAlignment` is 4) |
/// | `ofArray(int[8]).get(JAVA_LONG_UNALIGNED, 0)` | OK |
///
/// The alignment rule is ONE predicate, and CORRECTED 2026-08-16: the
/// constraint must be no larger than the alignment available at the
/// **absolute** offset `view.start + offset`, i.e. exactly
/// `seg.asSlice(offset).maxByteAlignment()`. The previous spelling was
/// `align <= maxByteAlignment(view.start) && (view.start + offset) % align == 0`,
/// which is one conjunct too many and is STRICTER than the oracle whenever a
/// slice starts at a worse alignment than the offset inside it reaches:
///
/// | call | oracle | previous |
/// |---|---|---|
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 2)` | reads (absolute 4) | refused |
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 0)` | refused (absolute 2) | refused |
/// | `ofArray(long[4]).asSlice(4).get(JAVA_LONG, 4)` | reads (absolute 8) | refused |
///
/// (measured, `FfmProbe3` rows M1c/M1d/M1h; `int[8].asSlice(2)` reports
/// `maxByteAlignment()==2` and `asSlice(2).asSlice(2)` reports 4.) Both halves
/// are still present — they are just the two halves of
/// [`heap_max_byte_alignment`]: the element width, and the low bit of the
/// offset. A `byte[]` segment's element width is 1, which is what refuses
/// `get(JAVA_INT, 0)` on one where a bare modulo would admit it.
///
/// This is enforced on the HEAP path only. The raw-address path has never
/// checked alignment and is not changed here — a native segment's real
/// `maxByteAlignment` is not knowable from the carrier (the oracle answers 32
/// for a malloc'd one), so the same rule cannot be transplanted, and adding a
/// half-rule to a path that works today would be a regression. NOMINATED in
/// the record instead.
pub(in crate::panama) fn heap_segment_check_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    view: &HeapSegmentView,
    layout: ObjectRef,
    offset: i64,
    width: i64,
    writing: bool,
) -> Result<(), MethodCallFailed> {
    // Round 13 wave 1 (lane mhffm): the session last under
    // `ffm_bounds_before_scope` (JDK 25's order); it runs no Java, so `view`
    // stays current.
    let scope_last = ffm_bounds_before_scope();
    if !scope_last {
        pe_segment_check_scope(ctx, seg)?;
    }
    heap_segment_check_bounds(ctx, view, layout, offset, width, writing)?;
    if scope_last {
        pe_segment_check_scope(ctx, seg)?;
    }
    Ok(())
}

/// [`heap_segment_check_access`] without the session check: read-only,
/// bounds, alignment.
pub(in crate::panama) fn heap_segment_check_bounds(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    layout: ObjectRef,
    offset: i64,
    width: i64,
    writing: bool,
) -> Result<(), MethodCallFailed> {
    if writing && view.read_only {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Attempt to write a read-only segment".into(),
        }
        .into());
    }

    let end = offset.checked_add(width);
    if offset < 0 || end.map_or(true, |e| e > view.size) {
        // `outOfBoundException`: the segment's `toString()` (j27 wave 4, lane
        // FFMMSG: the `heapBase` clause was missing).
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(out_of_bounds_message(&heap_view_text(ctx, view), offset, width)),
        }
        .into());
    }

    let align = crate::panama_libffi::layout_align(ctx, layout) as i64;
    if align > 1 {
        let max_align =
            heap_max_byte_alignment(view.elem_width as i64, view.start.saturating_add(offset));
        if align > max_align {
            return Err(heap_alignment_refusal(ctx, view, layout, offset, align));
        }
    }
    Ok(())
}

/// `checkEnclosingLayout`'s `Target offset %d is incompatible with alignment
/// constraint %d (of %s) for segment %s` (j27 wave 4, lane FFMMSG: the
/// `(of <layout>)` and `heapBase` clauses were missing). The clause is left
/// out for a layout that is not a real JDK one ([`real_layout_text`]). The
/// segment text is built before the layout's `toString` runs Java.
pub(in crate::panama) fn heap_alignment_refusal(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    layout: ObjectRef,
    offset: i64,
    align: i64,
) -> MethodCallFailed {
    let segment = heap_view_text(ctx, view);
    RuntimeError::IllegalArgumentException {
        message: misaligned_message(ctx, &segment, Some(layout), offset, align),
    }
    .into()
}

/// The refusal for a heap carrier whose shape does not add up.
///
/// A named refusal, not a `0`: the whole family this file keeps paying for is a
/// reader that answers a plausible number for a carrier it could not decode.
pub(in crate::panama) fn unreadable_heap_segment(ctx: &dyn NativeContext, seg: ObjectRef, op: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!(
            "MemorySegment.{op}: {} is a heap segment whose backing array cannot be resolved \
             (base/offset unreadable, or its bytes do not fit the array)",
            ctx.class_name_of_id(ctx.class_id_of_object(seg))
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
    .into()
}

/// `MemorySegment.asSlice(long offset, long size)`.
///
/// Extracted from an inline closure by F35 so its heap arm can be tested. The
/// body itself is [`pe_segment_slice`] — the same one `asSlice(long)`,
/// `asSlice(long,long,long)`, `asSlice(long,MemoryLayout)` and `asReadOnly()`
/// call, so the heap arm and the read-only contagion cannot be present on one
/// arity and absent on the next. Two bodies for one rule is what this file's
/// `getAtIndex` banner is about; there is now one.
pub(in crate::panama) fn pe_segment_as_slice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let offset = pe_long_arg(args, 1);
    let new_size = pe_long_arg(args, 2);
    pe_segment_slice(ctx, this, offset, new_size, None)
}

