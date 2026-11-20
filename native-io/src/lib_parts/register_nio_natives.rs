#[allow(unused_imports)]
use super::super::*;

pub(crate) fn register_nio_natives(registry: &mut NativeMethodRegistry) {
            // Register under both ByteBuffer and HeapByteBuffer for dispatch
    for c in &["java/nio/ByteBuffer", "java/nio/HeapByteBuffer"] {
        // --- Factory methods ---
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/ByteBuffer;",
            native_bb_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([B)Ljava/nio/ByteBuffer;", native_bb_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "wrap",
            "([BII)Ljava/nio/ByteBuffer;",
            native_bb_wrap_range, cratonvm_native_api::NativeKind::Bridge,
        );

        // --- Position / limit / capacity ---
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/Buffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/ByteBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/Buffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/ByteBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);

        // --- Mark / reset / clear / flip / rewind ---
        registry.register(c, "mark", "()Ljava/nio/Buffer;", native_bb_mark, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "mark", "()Ljava/nio/ByteBuffer;", native_bb_mark, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "reset", "()Ljava/nio/Buffer;", native_bb_reset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "reset", "()Ljava/nio/ByteBuffer;", native_bb_reset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/Buffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/ByteBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/Buffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/ByteBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/Buffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/ByteBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/ByteBuffer;", native_bb_compact, cratonvm_native_api::NativeKind::Bridge);

        // --- Get / put ---
        registry.register(c, "get", "()B", native_bb_get, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)B", native_bb_get_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "([BII)Ljava/nio/ByteBuffer;", native_bb_get_bulk, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(B)Ljava/nio/ByteBuffer;", native_bb_put, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(IB)Ljava/nio/ByteBuffer;", native_bb_put_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "([BII)Ljava/nio/ByteBuffer;", native_bb_put_bulk, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
            native_bb_put_bb, cratonvm_native_api::NativeKind::Bridge,
        );

        // --- Typed get/put (big-endian by default) ---
        registry.register(c, "getInt", "()I", native_bb_get_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "getInt", "(I)I", native_bb_get_int_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "putInt", "(I)Ljava/nio/ByteBuffer;", native_bb_put_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "putInt",
            "(II)Ljava/nio/ByteBuffer;",
            native_bb_put_int_abs, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "getLong", "()J", native_bb_get_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "putLong", "(J)Ljava/nio/ByteBuffer;", native_bb_put_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "getShort", "()S", native_bb_get_short, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "putShort",
            "(S)Ljava/nio/ByteBuffer;",
            native_bb_put_short, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "getFloat", "()F", native_bb_get_float, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "putFloat",
            "(F)Ljava/nio/ByteBuffer;",
            native_bb_put_float, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "getDouble", "()D", native_bb_get_double, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "putDouble",
            "(D)Ljava/nio/ByteBuffer;",
            native_bb_put_double, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "getChar", "()C", native_bb_get_char, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "putChar", "(C)Ljava/nio/ByteBuffer;", native_bb_put_char, cratonvm_native_api::NativeKind::Bridge);

        // --- Misc ---
        registry.register(c, "array", "()[B", native_bb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "isDirect", "()Z", native_bb_is_direct, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "isReadOnly", "()Z", native_bb_is_read_only, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/ByteBuffer;",
            native_bb_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "slice", "()Ljava/nio/ByteBuffer;", native_bb_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_bb_to_string, cratonvm_native_api::NativeKind::Bridge);
    }

    // Also register under java/nio/Buffer (parent abstract class)
    let buf = "java/nio/Buffer";
    // `isReadOnly`/`isDirect` are abstract in every real-JDK Buffer subclass
    // and were missing from this catch-all-on-Buffer fallback (unlike the
    // 8 accessors above). Any typed buffer allocated straight against an
    // abstract class name (e.g. literal `java/nio/CharBuffer`, not
    // `HeapCharBuffer`) has no closer override to resolve to, so the
    // interpreter's abstract-method dispatch walks all the way up to
    // `Buffer.isReadOnly()` (no Code) and throws AbstractMethodError unless
    // something is registered here. `native_bb_is_read_only` used to be a flat
    // "never read-only" default; it now reads the `isReadOnly` FIELD, the same
    // one `hasArray`/`array`/`arrayOffset` consult, so this catch-all and the
    // three accessors cannot disagree about one receiver.
    registry.register(buf, "isReadOnly", "()Z", native_bb_is_read_only, cratonvm_native_api::NativeKind::Bridge);
    registry.register(buf, "isDirect", "()Z", native_bb_is_direct, cratonvm_native_api::NativeKind::Bridge);

    // FileChannel basics
    let fc = "java/nio/channels/FileChannel";
    registry.register(
        fc,
        "open",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/FileChannel;",
        native_fc_open, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(fc, "read", "(Ljava/nio/ByteBuffer;)I", native_fc_read, cratonvm_native_api::NativeKind::Bridge);
    registry.register(fc, "write", "(Ljava/nio/ByteBuffer;)I", native_fc_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(fc, "position", "()J", native_fc_position, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        fc,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        native_fc_set_position, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(fc, "size", "()J", native_fc_size, cratonvm_native_api::NativeKind::Bridge);
    registry.register(fc, "close", "()V", native_fc_close, cratonvm_native_api::NativeKind::Bridge);

    // =========================================================================
    // Phase 42: CharBuffer + typed NIO buffers
    // =========================================================================
    // All typed buffers share the 5-field layout (array, pos, limit, capacity, mark)
    // but differ in element type and class name.

    // --- CharBuffer ---
    for c in &["java/nio/CharBuffer", "java/nio/HeapCharBuffer"] {
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/CharBuffer;",
            native_cb_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([C)Ljava/nio/CharBuffer;", native_cb_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "wrap",
            "(Ljava/lang/CharSequence;)Ljava/nio/CharBuffer;",
            native_cb_wrap_charseq, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "wrap",
            "(Ljava/lang/CharSequence;II)Ljava/nio/CharBuffer;",
            native_cb_wrap_charseq_range, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/CharBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/CharBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/CharBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/CharBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/CharBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "mark", "()Ljava/nio/CharBuffer;", native_bb_mark, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "reset", "()Ljava/nio/CharBuffer;", native_bb_reset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()C", native_cb_get, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)C", native_cb_get_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(C)Ljava/nio/CharBuffer;", native_cb_put, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(IC)Ljava/nio/CharBuffer;", native_cb_put_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(Ljava/lang/String;)Ljava/nio/CharBuffer;",
            native_cb_put_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "array", "()[C", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_cb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "length", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "charAt", "(I)C", native_cb_char_at, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/CharBuffer;", native_cb_compact, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_cb_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/CharBuffer;", native_cb_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/CharBuffer;", native_cb_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/CharBuffer;",
            native_cb_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/CharBuffer;",
            native_cb_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- IntBuffer ---
    for c in &["java/nio/IntBuffer", "java/nio/HeapIntBuffer"] {
        registry.register(c, "allocate", "(I)Ljava/nio/IntBuffer;", native_ib_allocate, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "wrap", "([I)Ljava/nio/IntBuffer;", native_ib_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/IntBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/IntBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/IntBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/IntBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/IntBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()I", native_tb_get_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)I", native_tb_get_int_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(I)Ljava/nio/IntBuffer;", native_tb_put_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(II)Ljava/nio/IntBuffer;", native_tb_put_int_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "array", "()[I", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_tb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/IntBuffer;", native_tb_compact, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_ib_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/IntBuffer;", native_ib_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/IntBuffer;", native_ib_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/IntBuffer;",
            native_ib_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/IntBuffer;",
            native_ib_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- LongBuffer ---
    for c in &["java/nio/LongBuffer", "java/nio/HeapLongBuffer"] {
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/LongBuffer;",
            native_lb_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([J)Ljava/nio/LongBuffer;", native_lb_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/LongBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/LongBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/LongBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/LongBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/LongBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()J", native_tb_get_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)J", native_tb_get_long_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(J)Ljava/nio/LongBuffer;", native_tb_put_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(IJ)Ljava/nio/LongBuffer;",
            native_tb_put_long_abs, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "array", "()[J", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_tb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/LongBuffer;", native_tb_compact, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_lb_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/LongBuffer;", native_lb_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/LongBuffer;", native_lb_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/LongBuffer;",
            native_lb_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/LongBuffer;",
            native_lb_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- FloatBuffer ---
    for c in &["java/nio/FloatBuffer", "java/nio/HeapFloatBuffer"] {
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/FloatBuffer;",
            native_fb_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([F)Ljava/nio/FloatBuffer;", native_fb_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/FloatBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/FloatBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/FloatBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/FloatBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/FloatBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()F", native_tb_get_float, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)F", native_tb_get_float_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(F)Ljava/nio/FloatBuffer;", native_tb_put_float, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(IF)Ljava/nio/FloatBuffer;",
            native_tb_put_float_abs, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "array", "()[F", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_tb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/FloatBuffer;", native_tb_compact, cratonvm_native_api::NativeKind::Bridge);
        // `order()` is ALSO registered directly on the literal
        // "java/nio/FloatBuffer" class in native-builtins (Wave 2 D), which
        // wins for that exact triple since it registers after this module —
        // registering it again here too so `HeapFloatBuffer`-stamped
        // receivers (this loop's second class name) resolve identically
        // instead of relying on registration order across two crates.
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_fb_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/FloatBuffer;", native_fb_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/FloatBuffer;", native_fb_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/FloatBuffer;",
            native_fb_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/FloatBuffer;",
            native_fb_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- DoubleBuffer ---
    for c in &["java/nio/DoubleBuffer", "java/nio/HeapDoubleBuffer"] {
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/DoubleBuffer;",
            native_db_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([D)Ljava/nio/DoubleBuffer;", native_db_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/DoubleBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "limit",
            "(I)Ljava/nio/DoubleBuffer;",
            native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/DoubleBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/DoubleBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/DoubleBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()D", native_tb_get_double, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)D", native_tb_get_double_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(D)Ljava/nio/DoubleBuffer;", native_tb_put_double, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(ID)Ljava/nio/DoubleBuffer;",
            native_tb_put_double_abs, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "array", "()[D", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_tb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/DoubleBuffer;", native_tb_compact, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_db_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/DoubleBuffer;", native_db_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/DoubleBuffer;", native_db_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/DoubleBuffer;",
            native_db_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/DoubleBuffer;",
            native_db_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- ShortBuffer ---
    for c in &["java/nio/ShortBuffer", "java/nio/HeapShortBuffer"] {
        registry.register(
            c,
            "allocate",
            "(I)Ljava/nio/ShortBuffer;",
            native_sb_allocate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "wrap", "([S)Ljava/nio/ShortBuffer;", native_sb_wrap, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "position", "()I", native_bb_position, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "position",
            "(I)Ljava/nio/ShortBuffer;",
            native_bb_set_position, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "limit", "()I", native_bb_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "limit", "(I)Ljava/nio/ShortBuffer;", native_bb_set_limit, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "capacity", "()I", native_bb_capacity, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "remaining", "()I", native_bb_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasRemaining", "()Z", native_bb_has_remaining, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "clear", "()Ljava/nio/ShortBuffer;", native_bb_clear, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "flip", "()Ljava/nio/ShortBuffer;", native_bb_flip, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "rewind", "()Ljava/nio/ShortBuffer;", native_bb_rewind, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "()S", native_tb_get_short, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "get", "(I)S", native_tb_get_short_abs, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "put", "(S)Ljava/nio/ShortBuffer;", native_tb_put_short, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "put",
            "(IS)Ljava/nio/ShortBuffer;",
            native_tb_put_short_abs, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(c, "array", "()[S", native_tb_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "hasArray", "()Z", native_bb_has_array, cratonvm_native_api::NativeKind::Bridge);
        // `arrayOffset` — F14-1 N3. `array` and `hasArray` were registered
        // for all six typed families and `arrayOffset` for NEITHER, so in
        // `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved
        // to the Code-less `java/nio/Buffer` declaration and threw
        // AbstractMethodError. Same native as ByteBuffer's: the JDK body is
        // generated from one template and MEASURED identical for all seven
        // families (`*.heap.arrayOffset OK 0`, `*.ro.arrayOffset` ->
        // ReadOnlyBufferException, `*.view.arrayOffset` ->
        // UnsupportedOperationException, jdk-25.0.3+9).
        registry.register(c, "arrayOffset", "()I", native_bb_array_offset, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "toString", "()Ljava/lang/String;", native_tb_to_string, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "compact", "()Ljava/nio/ShortBuffer;", native_tb_compact, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "order", "()Ljava/nio/ByteOrder;", native_sb_order, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "()Ljava/nio/ShortBuffer;", native_sb_slice, cratonvm_native_api::NativeKind::Bridge);
        registry.register(c, "slice", "(II)Ljava/nio/ShortBuffer;", native_sb_slice2, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            c,
            "duplicate",
            "()Ljava/nio/ShortBuffer;",
            native_sb_duplicate, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            c,
            "asReadOnlyBuffer",
            "()Ljava/nio/ShortBuffer;",
            native_sb_as_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

// --- ByteBuffer factory methods ---

pub(crate) fn native_bb_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    // gc-common w13-e: caller-sized, so the backing array reclaims and retries
    // before the `OutOfMemoryError`. `live` audit: this native holds nothing
    // (its only argument is an `int`).
    let bb = alloc_byte_buffer_reclaiming(ctx, cap, "byte[] (ByteBuffer.allocate)")?;
    Ok(Some(Value::Object(Some(bb))))
}

/// `ByteBuffer.wrap(byte[])` — **an ALIAS of the caller's array, not a copy.**
///
/// This copied every byte into a fresh array, which breaks the one property
/// `wrap` exists for. `ByteBuffer.wrap(bytes)` is how a caller says "give me a
/// buffer view OF THESE BYTES"; HotSpot's `wrap` is literally
/// `new HeapByteBuffer(array, array.length, null)` and installs the reference.
///
/// MEASURED, jdk-25.0.3+9 (`scratchpad/f26/W.java`):
///
/// ```text
/// wrap(arr).array()==arr : true          <- identity, not equality
/// put(0,0x7F) -> raw[0]  : 127           <- writes reach the caller's array
/// raw[1]=0x11 -> get(1)  : 17            <- and are seen in the other direction
/// wrap(arr).arrayOffset() : 0
/// wrap(arr).pos/lim/cap   : 0/8/8
/// ```
///
/// Both directions were broken by the copy, and both are silent: a caller that
/// wraps a scratch array, fills the buffer and then reads the ARRAY got zeros;
/// a caller that mutates the array and reads through the buffer got stale
/// bytes. Neither raises anything.
///
/// `offset` is `0` here — `wrap` does not shift the base — so this aliases in
/// BOTH jdk modes: [`buf_try_set_heap_offset`] reports base 0 representable
/// unconditionally, because every layout expresses it.
pub(crate) fn native_bb_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
    let window = BufferWindow {
        offset: 0,
        pos: 0,
        lim: len,
        cap: len,
        mark: -1,
    };
    // `offset == 0` always round-trips, so this cannot be `None`; the `?`-free
    // fallback exists so a future non-zero-offset caller cannot get a silent
    // wrong answer out of this site.
    // gc-common w13-e: the fallback reclaims before its OOME. `live` audit:
    // `src` is not used after `alloc_byte_buffer_over` returns.
    match alloc_byte_buffer_over(ctx, src, window) {
        Some(bb) => Ok(Some(Value::Object(Some(bb)))),
        None => Ok(Some(Value::Object(Some(alloc_byte_buffer_reclaiming(
            ctx,
            len.max(0) as usize,
            "byte[] (ByteBuffer.wrap)",
        )?)))),
    }
}

/// `ByteBuffer.wrap(byte[], int offset, int length)` — the same alias, plus
/// the range check it never had.
///
/// HotSpot's geometry, MEASURED: `wrap(raw,2,3)` gives `pos/lim/cap = 2/5/8`,
/// `array() == raw` and `arrayOffset() == 0`. So `offset`/`length` move
/// POSITION and LIMIT; the capacity stays the whole array and the array-base
/// `offset` stays `0`. The old body already had that geometry right — what it
/// had wrong was the array (copied) and the absence of any bounds check.
///
/// **The bounds check is not scope creep; it is what makes the aliasing
/// safe.** `ByteBuffer.wrap(array, off, len)` is
/// `try { new HeapByteBuffer(array, off, len, null) } catch
/// (IllegalArgumentException x) { throw new IndexOutOfBoundsException(); }`,
/// and MEASURED both `wrap(raw,0,9)` and `wrap(raw,-1,2)` raise
/// `java.lang.IndexOutOfBoundsException` with a NULL message — hence
/// [`RuntimeError::ioobe_no_message`], the same spelling `servlet.rs`'s copy of
/// this method uses. While the body copied, an out-of-range `length` merely
/// produced a limit past the end of a private array; now that the array is the
/// caller's, an unchecked limit would index past the end of a REAL array on
/// every subsequent read.
pub(crate) fn native_bb_wrap_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr_len = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
    if offset < 0 || length < 0 || i64::from(offset) + i64::from(length) > i64::from(arr_len) {
        return Err(RuntimeError::ioobe_no_message().into());
    }
    let window = BufferWindow {
        offset: 0,
        pos: offset,
        lim: offset + length,
        cap: arr_len,
        mark: -1,
    };
    // gc-common w13-e: as in `native_bb_wrap`, `src` is dead past this point.
    match alloc_byte_buffer_over(ctx, src, window) {
        Some(bb) => Ok(Some(Value::Object(Some(bb)))),
        None => Ok(Some(Value::Object(Some(alloc_byte_buffer_reclaiming(
            ctx,
            arr_len.max(0) as usize,
            "byte[] (ByteBuffer.wrap)",
        )?)))),
    }
}

// --- Position / limit / capacity ---

pub(crate) fn native_bb_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(buf_read_position(ctx, this))))
}

pub(crate) fn native_bb_set_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_pos = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let lim = buf_read_limit(ctx, this);
    // `java.nio.Buffer.position(int)` range-CHECKS, it does not clamp:
    // "if (newPosition > limit | newPosition < 0) throw
    // createPositionException(newPosition)", specified as "@throws
    // IllegalArgumentException If the preconditions on newPosition do not
    // hold". `new_pos.clamp(0, lim)` returned `this` for every out-of-range
    // call, so a caller that mis-computed an offset got a buffer silently
    // parked at `limit` (or 0) and read the wrong bytes from it, instead of
    // the exception that names the bad offset.
    if new_pos < 0 || new_pos > lim {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("newPosition {new_pos} out of range [0, {lim}]"),
        }
        .into());
    }
    buf_set_position(ctx, this, new_pos);
    if buf_read_mark(ctx, this) > new_pos {
        buf_set_mark(ctx, this, -1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(buf_read_limit(ctx, this))))
}

pub(crate) fn native_bb_set_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_lim = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let cap = buf_read_capacity(ctx, this);
    // Same contract as `position(int)` one function up:
    // "if (newLimit > capacity | newLimit < 0) throw
    // createLimitException(newLimit)", "@throws IllegalArgumentException If
    // the preconditions on newLimit do not hold". A clamped `limit(cap + 1)`
    // is the more dangerous of the pair — it hands back a buffer whose
    // `remaining()` is smaller than the caller asked for, so the short read
    // that follows reads as a short read from the CHANNEL.
    if new_lim < 0 || new_lim > cap {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("newLimit {new_lim} out of range [0, {cap}]"),
        }
        .into());
    }
    buf_set_limit(ctx, this, new_lim);
    if buf_read_position(ctx, this) > new_lim {
        buf_set_position(ctx, this, new_lim);
    }
    if buf_read_mark(ctx, this) > new_lim {
        buf_set_mark(ctx, this, -1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(buf_read_capacity(ctx, this))))
}

pub(crate) fn native_bb_remaining(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // MIGRATED. This is the site that produced `RJdkDefineClass`'s
    // "ByteBuffer missing backing array (field 0 returned Int(-1))", from the
    // `int len = b.remaining()` that opens real
    // `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)`. It
    // asked `bb_state` for the whole tuple and threw the array away — the
    // refusal it inherited was over storage it never touched. `remaining()` is
    // `limit - position` and is defined on `java.nio.Buffer`, above any notion
    // of storage; it must answer for a direct buffer, a heap buffer, and a
    // read-only view alike. `.max(0)` matches `Buffer.remaining()`'s
    // `Math.max(lim - pos, 0)`.
    let (pos, lim, _) = buf_metadata(ctx, this);
    Ok(Some(Value::Int((lim - pos).max(0))))
}

pub(crate) fn native_bb_has_remaining(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // MIGRATED, same reason as `native_bb_remaining` above: `hasRemaining()`
    // is `position < limit`, storage-free by definition.
    let (pos, lim, _) = buf_metadata(ctx, this);
    Ok(Some(Value::Int(if pos < lim { 1 } else { 0 })))
}

// --- Mark / reset / clear / flip / rewind / compact ---

pub(crate) fn native_bb_mark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pos = buf_read_position(ctx, this);
    buf_set_mark(ctx, this, pos);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mark = buf_read_mark(ctx, this);
    if mark < 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "InvalidMarkException".to_string(),
        }
        .into());
    }
    buf_set_position(ctx, this, mark);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cap = match ctx.get_field(this, BB_FIELD_CAPACITY) {
        Value::Int(v) => v,
        _ => 0,
    };
    buf_set_position(ctx, this, 0);
    buf_set_limit(ctx, this, cap);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_flip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pos = buf_read_position(ctx, this);
    buf_set_limit(ctx, this, pos);
    buf_set_position(ctx, this, 0);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_rewind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buf_set_position(ctx, this, 0);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_compact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    let cap = view.cap;
    let remaining = (lim - pos) as usize;
    // Copy remaining bytes to beginning
    let mut bytes = vec![0u8; remaining];
    bb_read_bytes(ctx, view, pos as usize, &mut bytes)?;
    for i in 0..remaining {
        bb_write_byte(ctx, view, i, bytes[i])?;
    }
    buf_set_position(ctx, this, remaining as i32);
    buf_set_limit(ctx, this, cap);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

// --- Get / put (relative) ---

pub(crate) fn native_bb_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos >= lim {
        // `BufferUnderflowException` is a real `RuntimeError` variant; an
        // `IllegalStateException` carrying its *name* is not catchable as what
        // the relative `get` documents.
        return Err(RuntimeError::BufferUnderflowException.into());
    }
    let byte = Value::Int(bb_read_byte(ctx, view, pos as usize)? as i8 as i32);
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(byte))
}

pub(crate) fn native_bb_get_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let cap = view.cap;
    if index < 0 || index >= cap {
        return Err(buffer_index_out_of_bounds());
    }
    let byte = Value::Int(bb_read_byte(ctx, view, index as usize)? as i8 as i32);
    Ok(Some(byte))
}

pub(crate) fn native_bb_get_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dst = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JDK `ByteBuffer.get(byte[] dst, int offset, int length)` first validates
    // `offset`/`length` against `dst.length` (Objects.checkFromIndexSize →
    // IndexOutOfBoundsException), rejecting negatives and overflow, BEFORE
    // checking the buffer's `remaining`. Without this a negative Java offset
    // becomes a huge usize and the per-element loop writes out of range.
    check_array_bounds(offset, length, ctx.array_length(dst))?;
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    let length = length as usize;
    let offset = offset as usize;
    if length > remaining {
        return Err(RuntimeError::BufferUnderflowException.into());
    }
    for i in 0..length {
        let v = bb_read_byte(ctx, view, pos as usize + i)? as i8 as i32;
        ctx.set_array_element(dst, offset + i, Value::Int(v));
    }
    buf_set_position(ctx, this, pos + length as i32);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let byte = args.get(1).copied().unwrap_or(Value::Int(0));
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos >= lim {
        // Same shape as the underflow sites: raise the documented class, not
        // an `IllegalStateException` naming it.
        return Err(RuntimeError::BufferOverflowException.into());
    }
    bb_write_byte(ctx, view, pos as usize, byte.as_int().unwrap_or(0) as u8)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_put_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let byte = args.get(2).copied().unwrap_or(Value::Int(0));
    let view = bb_storage_view(ctx, this)?;
    let cap = view.cap;
    if index < 0 || index >= cap {
        return Err(buffer_index_out_of_bounds());
    }
    bb_write_byte(ctx, view, index as usize, byte.as_int().unwrap_or(0) as u8)?;
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_put_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let src = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JDK `ByteBuffer.put(byte[] src, int offset, int length)` validates
    // `offset`/`length` against `src.length` (Objects.checkFromIndexSize →
    // IndexOutOfBoundsException), rejecting negatives and overflow, BEFORE
    // checking the buffer's `remaining`.
    check_array_bounds(offset, length, ctx.array_length(src))?;
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    let length = length as usize;
    let offset = offset as usize;
    if length > remaining {
        return Err(RuntimeError::BufferOverflowException.into());
    }
    for i in 0..length {
        let v = match ctx.get_array_element(src, offset + i) {
            Value::Int(v) => v as u8,
            _ => 0,
        };
        bb_write_byte(ctx, view, pos as usize + i, v)?;
    }
    buf_set_position(ctx, this, pos + length as i32);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_put_bb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    // Specified: `IllegalArgumentException` when the source is this buffer.
    if src == this {
        return Err(RuntimeError::IllegalArgumentException {
            message: "The source buffer is this buffer".to_string(),
        }
        .into());
    }
    let src_view = bb_storage_view(ctx, src)?;
    let src_pos = src_view.pos;
    let src_lim = src_view.lim;
    let src_remaining = (src_lim - src_pos) as usize;
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    if src_remaining > remaining {
        // `BufferOverflowException` is a real `RuntimeError` variant; raising
        // an `IllegalStateException` carrying its *name* — as this did — is
        // not catchable as what `ByteBuffer.put` documents.
        return Err(RuntimeError::BufferOverflowException.into());
    }
    for i in 0..src_remaining {
        let v = bb_read_byte(ctx, src_view, src_pos as usize + i)?;
        bb_write_byte(ctx, view, pos as usize + i, v)?;
    }
    buf_set_position(ctx, this, pos + src_remaining as i32);
    buf_set_position(ctx, src, src_lim);
    Ok(Some(Value::Object(Some(this))))
}

// --- Typed get/put (big-endian) ---

pub(crate) fn native_bb_get_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 4 > lim {
        // `BufferUnderflowException` is a real `RuntimeError` variant; an
        // `IllegalStateException` carrying its *name* is not catchable as what
        // the relative `get` documents.
        return Err(RuntimeError::BufferUnderflowException.into());
    }
    let mut bytes = [0u8; 4];
    bb_read_bytes(ctx, view, pos as usize, &mut bytes)?;
    buf_set_position(ctx, this, pos + 4);
    Ok(Some(Value::Int(i32::from_be_bytes(bytes))))
}

pub(crate) fn native_bb_get_int_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let cap = view.cap;
    if !abs_access_in_bounds(index, 4, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    let mut bytes = [0u8; 4];
    bb_read_bytes(ctx, view, index as usize, &mut bytes)?;
    Ok(Some(Value::Int(i32::from_be_bytes(bytes))))
}

pub(crate) fn native_bb_put_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 4 > lim {
        // Same shape as the underflow sites: raise the documented class, not
        // an `IllegalStateException` naming it.
        return Err(RuntimeError::BufferOverflowException.into());
    }
    let bytes = val.to_be_bytes();
    bb_write_bytes(ctx, view, pos as usize, &bytes)?;
    buf_set_position(ctx, this, pos + 4);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_put_int_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let cap = view.cap;
    if !abs_access_in_bounds(index, 4, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    let bytes = val.to_be_bytes();
    bb_write_bytes(ctx, view, index as usize, &bytes)?;
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_get_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 8 > lim {
        // `BufferUnderflowException` is a real `RuntimeError` variant; an
        // `IllegalStateException` carrying its *name* is not catchable as what
        // the relative `get` documents.
        return Err(RuntimeError::BufferUnderflowException.into());
    }
    let mut bytes = [0u8; 8];
    bb_read_bytes(ctx, view, pos as usize, &mut bytes)?;
    buf_set_position(ctx, this, pos + 8);
    Ok(Some(Value::Long(i64::from_be_bytes(bytes))))
}

pub(crate) fn native_bb_put_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 8 > lim {
        // Same shape as the underflow sites: raise the documented class, not
        // an `IllegalStateException` naming it.
        return Err(RuntimeError::BufferOverflowException.into());
    }
    let bytes = val.to_be_bytes();
    bb_write_bytes(ctx, view, pos as usize, &bytes)?;
    buf_set_position(ctx, this, pos + 8);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_get_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 2 > lim {
        // `BufferUnderflowException` is a real `RuntimeError` variant; an
        // `IllegalStateException` carrying its *name* is not catchable as what
        // the relative `get` documents.
        return Err(RuntimeError::BufferUnderflowException.into());
    }
    let mut bytes = [0u8; 2];
    bb_read_bytes(ctx, view, pos as usize, &mut bytes)?;
    buf_set_position(ctx, this, pos + 2);
    Ok(Some(Value::Int(i16::from_be_bytes(bytes) as i32)))
}

pub(crate) fn native_bb_put_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v as i16,
        _ => 0,
    };
    let view = bb_storage_view(ctx, this)?;
    let pos = view.pos;
    let lim = view.lim;
    if pos + 2 > lim {
        // Same shape as the underflow sites: raise the documented class, not
        // an `IllegalStateException` naming it.
        return Err(RuntimeError::BufferOverflowException.into());
    }
    let bytes = val.to_be_bytes();
    bb_write_bytes(ctx, view, pos as usize, &bytes)?;
    buf_set_position(ctx, this, pos + 2);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_bb_get_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let int_val = native_bb_get_int(ctx, args)?;
    match int_val {
        Some(Value::Int(v)) => Ok(Some(Value::Float(f32::from_bits(v as u32)))),
        _ => Ok(Some(Value::Float(0.0))),
    }
}

pub(crate) fn native_bb_put_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    native_bb_put_int(
        ctx,
        &[Value::Object(Some(this)), Value::Int(val.to_bits() as i32)],
    )
}

pub(crate) fn native_bb_get_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let long_val = native_bb_get_long(ctx, args)?;
    match long_val {
        Some(Value::Long(v)) => Ok(Some(Value::Double(f64::from_bits(v as u64)))),
        _ => Ok(Some(Value::Double(0.0))),
    }
}

pub(crate) fn native_bb_put_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    native_bb_put_long(
        ctx,
        &[Value::Object(Some(this)), Value::Long(val.to_bits() as i64)],
    )
}

pub(crate) fn native_bb_get_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let short_val = native_bb_get_short(ctx, args)?;
    match short_val {
        Some(Value::Int(v)) => Ok(Some(Value::Int(v & 0xFFFF))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_bb_put_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_bb_put_short(ctx, args)
}

// --- Misc ---

/// `ByteBuffer.array()[B` — the JDK's three-way split, not a two-way
/// storage classification.
///
/// This asked only "Heap or Direct?" and **never consulted `isReadOnly`**, so a
/// read-only heap buffer got its backing `byte[]` handed straight out where
/// HotSpot raises `ReadOnlyBufferException`. Paired with the same omission in
/// [`native_bb_has_array`] that was not a wrong ANSWER but a wrong
/// **CAPABILITY**: a caller following the documented `hasArray()` → `array()`
/// protocol was told `true` and then given a mutable alias to storage the JDK
/// refuses to expose, and every write through it corrupted a read-only buffer
/// with no exception anywhere. See [`buffer_array_access`] for the measured
/// table and the ordering argument.
pub(crate) fn native_bb_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `bb_resolve_heap_array` rather than `bb_storage_view`, for the same
    // reason `native_tb_array` uses it: this native's question is literally
    // `hb == null`, and `bb_storage_view` answers a THREE-valued
    // heap/direct/InternalError where the JDK has two. A storage-less receiver
    // used to raise the internal "ByteBuffer missing backing storage"
    // diagnostic here; HotSpot raises `UnsupportedOperationException`, which is
    // what the array-less arm below now gives it. The diagnostic is not lost —
    // every get/put on the same receiver still goes through `bb_state` /
    // `bb_storage_view` and still reports it, at a site where a missing
    // backing store really is a VM inconsistency rather than a legal Java
    // state. `bb_resolve_heap_array` is `bb_storage_view`'s own first step, so
    // the heap arm and its slot-5 kind screen are unchanged.
    let arr = bb_resolve_heap_array(ctx, this);
    match buffer_array_access(arr.is_some(), buffer_is_read_only(ctx, this)) {
        BufferArrayAccess::Absent => Err(buffer_no_backing_array()),
        BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
        BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
    }
}

/// `ByteBuffer.hasArray()Z` — `(hb != null) && !isReadOnly`.
///
/// The `&& !isReadOnly` half was absent: a `HeapByteBufferR` answered `true`.
/// Also registered on the six typed families (Char/Int/Long/Float/Double/Short
/// Buffer), whose read-only siblings measure `false` identically.
pub(crate) fn native_bb_has_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `hasArray()` NEVER throws on HotSpot — it is one `&&` over two fields.
    // Routing it through `bb_storage_view` gave it an error path (a
    // storage-less receiver raised the internal "missing backing storage"
    // diagnostic where the JDK answers a plain `false`), which is why this
    // reads `hb` directly like the other three accessors do.
    let has_hb = bb_resolve_heap_array(ctx, this).is_some();
    Ok(Some(Value::Int(matches!(
        buffer_array_access(has_hb, buffer_is_read_only(ctx, this)),
        BufferArrayAccess::Accessible
    ) as i32)))
}

/// `ByteBuffer.arrayOffset()I` — the SAME two refusals as [`native_bb_array`],
/// and it had only one of them. A read-only heap receiver answered a plain
/// integer, which is indistinguishable from a legitimate offset.
pub(crate) fn native_bb_array_offset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Same resolution as `native_bb_array` — see its comment. The offset comes
    // from `bb_resolve_heap_offset`, which is the field `bb_storage_view`'s
    // `Heap` arm was carrying, so the happy path is byte-for-byte what it was.
    let has_hb = bb_resolve_heap_array(ctx, this).is_some();
    match buffer_array_access(has_hb, buffer_is_read_only(ctx, this)) {
        BufferArrayAccess::Absent => Err(buffer_no_backing_array()),
        BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
        BufferArrayAccess::Accessible => {
            Ok(Some(Value::Int(bb_resolve_heap_offset(ctx, this) as i32)))
        }
    }
}

pub(crate) fn native_bb_is_direct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `isDirect()` never throws on HotSpot. A storage-less receiver used to
    // raise `bb_storage_view`'s internal "missing backing storage" error here
    // (interpreter round i5 wave 105, lane L5); it answers `false` now, the
    // heap answer, and the error stays on the accessors that need the bytes.
    // Same order as `bb_storage_view`: a resolvable array wins.
    let direct = bb_resolve_heap_array(ctx, this).is_none()
        && bb_resolve_direct_address(ctx, this).is_some();
    Ok(Some(Value::Int(i32::from(direct))))
}

/// `Buffer.isReadOnly()Z` — the flag, not a constant.
///
/// This was a flat `Ok(Int(0))`: every buffer in this registrar answered
/// "writable", including the ones the typed `$ro_fn` macro arm had just marked
/// `isReadOnly = 1` by name. Left alone it now CONTRADICTS its own siblings:
/// after the three-way split landed in [`native_bb_has_array`] a read-only heap
/// buffer answers `hasArray() == false` and `array()` throws
/// `ReadOnlyBufferException`, while this said the buffer was writable — a state
/// no real receiver can be in, and one that steers a caller which branches on
/// `isReadOnly()` (rather than on `hasArray()`) straight into the throw.
///
/// MEASURED, jdk-25.0.3+9: `ByteBuffer.allocate(8).isReadOnly()` is `false`,
/// `.asReadOnlyBuffer().isReadOnly()` is `true`, and the same pair holds for
/// all six typed families (`HeapLongBufferR` &c.).
///
/// Reads the same FIELD [`buffer_is_read_only`] does, so the four accessors agree
/// by construction. A layout with no such field still answers `false`, which is
/// the previous behaviour for every receiver that had it — so nothing that was
/// right before changes.
pub(crate) fn native_bb_is_read_only(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(buffer_is_read_only(ctx, this) as i32)))
}

/// `ByteBuffer.duplicate()` — and the `isReadOnly` flag it never carried.
///
/// This copied pos/lim/cap/mark and pointed the result at the source's backing
/// array, and wrote NOTHING to `isReadOnly`. So `readOnlyBuffer.duplicate()`
/// came back writable, which re-opens the wrong CAPABILITY F14-1 closed one
/// call further along: the duplicate answers `hasArray() == true`, `array()`
/// hands its backing store over, and it is the SAME array — writes through it
/// land in the read-only buffer, with no exception at any point.
///
/// MEASURED, jdk-25.0.3+9 (`scratchpad/f21/F21ViewContagionProbe.java`):
/// `ByteBuffer.allocate(8).asReadOnlyBuffer().duplicate()` is a
/// `java.nio.HeapByteBufferR` with `isReadOnly() == true`,
/// `hasArray() == false` and `array()` → `ReadOnlyBufferException`. See
/// [`BufferDerivation`] for the whole seven-family table.
/// **TWO residuals of that repair closed here (F26-1).**
///
/// 1. **`offset` was dropped.** F21 taught this to share the source's array
///    but wrote nothing to `ByteBuffer.offset`, so the duplicate inherited the
///    fresh allocation's `0`. That is invisible only while nothing can produce
///    a source with a non-zero `offset` — and `servlet.rs`'s
///    `ByteBuffer.slice(int,int)` and `asReadOnlyBuffer()` already could
///    (`s2_bb_new_heap_view` writes `offset` by name), and NEITHER descriptor
///    is registered by `register_nio_natives`, so neither is shadowed. So
///    `bb.slice(2,4).duplicate().get(0)` read the array from index 0 instead
///    of 2 — a wrong VALUE, silently. MEASURED on HotSpot:
///    `wrap(raw,2,4).slice().duplicate().arrayOffset()` is `2` and its
///    `get(0)` equals `raw[2]`.
/// 2. **A DIRECT source was turned into a heap COPY.** The `else` arm fired
///    for every `allocateDirect(n)` receiver (they have no `hb`), so the
///    duplicate was a heap buffer holding a snapshot: `isDirect()` flipped
///    `true` → `false`, and writes stopped being shared. MEASURED:
///    `allocateDirect(8).duplicate()` is a `java.nio.DirectByteBuffer`,
///    `isDirect() == true`, and a write through it is `SHARED`.
///
/// Both now go through [`bb_derive_view`], which is also what `slice()` uses,
/// so the two cannot drift on the storage question again. The geometry is
/// [`duplicate_window`]: `offset`, `capacity` and the MARK are all carried
/// across, which is the entire difference from [`slice_window`].
pub(crate) fn native_bb_duplicate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let view = bb_storage_view(ctx, this)?;
    let src_offset = match view.storage {
        BbStorage::Heap { offset, .. } => offset,
        BbStorage::Direct { .. } => 0,
    };
    let window = duplicate_window(
        src_offset,
        view.pos,
        view.lim,
        view.cap,
        buf_read_mark(ctx, this),
    );
    // Read the flag BEFORE deriving — see [`buf_write_read_only`].
    let src_ro = buffer_is_read_only(ctx, this);
    let dup = bb_derive_view(ctx, view, window)?;
    // The `duplicate()` half of the read-only contagion — see this function's
    // doc comment. It goes LAST because `bb_derive_view` writes `hb` and
    // `offset` by name, and on a real-JDK-shaped receiver that is the same
    // by-name field machinery this write uses.
    buf_write_read_only(ctx, dup, BufferDerivation::Inherit, src_ro);
    Ok(Some(Value::Object(Some(dup))))
}

/// `ByteBuffer.slice()` — the same missing `isReadOnly` propagation as
/// [`native_bb_duplicate`].
///
/// MEASURED, jdk-25.0.3+9: `allocate(8).asReadOnlyBuffer().position(1).slice()`
/// is a `java.nio.HeapByteBufferR`, `isReadOnly() == true`, `hasArray()` false,
/// `array()`/`arrayOffset()` → `ReadOnlyBufferException`.
///
/// **THE STORAGE MODEL, repaired (F26-1).** This body used to COPY the
/// remaining bytes into a fresh buffer. That is not a cosmetic difference from
/// HotSpot; it is a WRONG CAPABILITY of the same species F14-1 and F21-1
/// closed one call earlier. `slice()` exists to hand out a second view of the
/// SAME bytes — "changes to this buffer's content will be visible in the new
/// buffer, and vice versa" (`java.nio.ByteBuffer`) — so a program that slices a
/// buffer, hands the slice to a decoder and reads the result back through the
/// original silently LOSES every write, with no exception anywhere. That is
/// the quiet kind of wrong.
///
/// MEASURED, jdk-25.0.3+9, `scratchpad/f26/F26AliasProbe.java`:
///
/// | expression | HotSpot | the copying body |
/// |---|---|---|
/// | `hbb.position(2).slice().array()` | the SOURCE array, by `==` | a fresh 6-byte array |
/// | `hbb.position(2).slice().arrayOffset()` | `2` | `0`, unreachably |
/// | write through slice, read through source | `SHARED` | `ISOLATED` |
/// | `hbb.slice().slice(1,2).arrayOffset()` | `3` (offsets compose) | `0` |
/// | `allocate(4).position(4).slice().arrayOffset()` | `4` (empty slice, offset still moves) | `0` |
///
/// The three storage kinds and what each now does:
///
/// * **heap, `offset` representable** — share `hb`, record
///   [`slice_window`]'s composed `offset`. This is real
///   `HeapByteBuffer.slice()`, argument for argument.
/// * **direct** — share the native `address`, advanced by `pos`. A direct
///   slice was previously turned into a HEAP copy, which also flipped
///   `isDirect()` from `true` to `false`.
/// * **heap, `offset` NOT representable** (synthetic-JDK mode: no
///   `ByteBuffer.offset` field to write) — keep the copy. Aliasing at a
///   non-zero base is not expressible in that layout, and a wrong `offset`
///   reads the wrong bytes, which is worse than a copy. `servlet.rs` takes the
///   same fallback for the same reason. See [`buf_try_set_heap_offset`].
///
/// Why this repair had to land HERE and not in `servlet.rs`, where an aliasing
/// implementation of this exact descriptor already existed: `register_io_natives`
/// runs AFTER `register_essential_natives_with_shims` in both `vm_init`
/// real-JDK arms (VERIFIED, `vm/src/vm/vm_init.rs` L2055 < L2252 and L2593 <
/// L2788) and registration is last-write-wins, so `servlet.rs`'s version of
/// `slice()` is shadowed and never ran.
pub(crate) fn native_bb_slice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let view = bb_storage_view(ctx, this)?;
    let src_offset = match view.storage {
        BbStorage::Heap { offset, .. } => offset,
        BbStorage::Direct { .. } => 0,
    };
    let window = slice_window(src_offset, view.pos, view.lim);
    // Read the flag BEFORE deriving — `bb_derive_view` allocates and `this`
    // must not be dereferenced afterwards. See [`buf_write_read_only`].
    let src_ro = buffer_is_read_only(ctx, this);
    let derived = bb_derive_view(ctx, view, window)?;
    buf_write_read_only(ctx, derived, BufferDerivation::Inherit, src_ro);
    Ok(Some(Value::Object(Some(derived))))
}

/// Mint the buffer a view-producing native must return for `window` over
/// `src`'s storage — ALIASING where the storage model can express it, copying
/// where it cannot.
///
/// One function rather than one per view-producing native, for the reason
/// [`buffer_array_access`] is one: the copying/aliasing decision has three
/// arms and a fallback, and three transcriptions of it is how this family's
/// `hasArray`/`array`/`arrayOffset` rule came to exist in three drifted
/// copies.
///
/// `window.offset` is in the same units as `BbStorage::Heap`'s `offset` and as
/// the direct arm's byte advance, i.e. BYTES — this is the `ByteBuffer` family
/// only. The typed families scale by element width and are NOT routed here
/// (see the record's nomination list).
pub(crate) fn bb_derive_view(
    ctx: &mut dyn NativeContext,
    src: BbView,
    window: BufferWindow,
) -> Result<ObjectRef, MethodCallFailed> {
    // `window.offset` is ABSOLUTE in the backing array; `bb_read_byte` indexes
    // RELATIVE to the source's own logical element 0 (it adds the source's
    // `offset` itself). The difference is the window's start in the source's
    // own index space — `pos` for a slice, `0` for a duplicate — and getting
    // this wrong double-counts the source's offset, which is exactly the
    // silent wrong-bytes read this whole record is about.
    let src_offset = match src.storage {
        BbStorage::Heap { offset, .. } => offset,
        BbStorage::Direct { .. } => 0,
    };
    let rel_start = window.offset.saturating_sub(src_offset);

    if let BbStorage::Direct { addr } = src.storage {
        // A direct view shares the native memory, advanced to the window's
        // byte 0. Nothing to allocate and no `offset` field involved: the
        // advanced `address` IS the offset, which is why the direct arm has
        // no representability question and never falls back.
        let obj = alloc_bb_object(ctx);
        buf_write_metadata(ctx, obj, window.pos, window.lim, window.cap, window.mark);
        ctx.set_field_by_name(
            obj,
            "address",
            Value::Long(addr.saturating_add(rel_start as i64)),
        );
        return Ok(obj);
    }
    if let BbStorage::Heap { arr, .. } = src.storage {
        if let Some(obj) = alloc_byte_buffer_over(ctx, arr, window) {
            return Ok(obj);
        }
    }
    // Fallback: the layout cannot carry a non-zero `offset`, so copy. Content
    // is right; aliasing is not representable and `arrayOffset()` answers 0.
    let cap = window.cap.max(0) as usize;
    // `alloc_byte_buffer` allocates twice (the object and its array) and can
    // relocate the SOURCE's backing array, which `src` holds as a bare
    // `ObjectRef` resolved before this call. Pin it across the allocation and
    // rebuild the view from the post-GC reference. The direct arm has already
    // returned above, so the only storage kind reaching here is `Heap`.
    //
    // gc-common w13-e: the copy is source-sized, so its array reclaims and
    // retries before the OOME. `live` audit: the source array is pinned here;
    // `window`/`rel_start` are plain data; both callers (`duplicate`,
    // `slice`) read everything they need from `this` before calling in.
    let (copy, src) = match src.storage {
        BbStorage::Heap { arr, offset } => {
            let pin = ctx.pin_native_root(arr);
            let copy = match alloc_byte_buffer_reclaiming(ctx, cap, "byte[] (ByteBuffer view copy)")
            {
                Ok(copy) => copy,
                Err(oome) => {
                    ctx.unpin_native_roots(pin);
                    return Err(oome);
                }
            };
            let arr = ctx.read_native_pin(pin, arr);
            ctx.unpin_native_roots(pin);
            (
                copy,
                BbView {
                    storage: BbStorage::Heap { arr, offset },
                    pos: src.pos,
                    lim: src.lim,
                    cap: src.cap,
                },
            )
        }
        BbStorage::Direct { .. } => (alloc_byte_buffer(ctx, cap), src),
    };
    let copy_view = bb_storage_view(ctx, copy)?;
    for i in 0..cap {
        let b = bb_read_byte(ctx, src, rel_start + i)?;
        bb_write_byte(ctx, copy_view, i, b)?;
    }
    buf_write_metadata(ctx, copy, window.pos, window.lim, window.cap, window.mark);
    Ok(copy)
}

pub(crate) fn native_bb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let view = bb_storage_view(ctx, this)?;
    let kind = match view.storage {
        BbStorage::Heap { .. } => "HeapByteBuffer",
        BbStorage::Direct { .. } => "DirectByteBuffer",
    };
    let s = format!(
        "java.nio.{kind}[pos={} lim={} cap={}]",
        view.pos, view.lim, view.cap
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

// --- FileChannel ---

/// The access a `FileChannel.open(Path, OpenOption...)` call asks for.
///
/// Separated from the native so the JDK's option ALGEBRA — which is four
/// rules and two refusals, not a switch — can be tested without a receiver.
/// See [`file_channel_open_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileChannelOpenMode {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub create: bool,
    pub truncate: bool,
}

/// `FileChannel.open`'s option algebra, from already-normalised option names
/// (see `normalize_afc_open_option_name`: the simple name, upper-cased).
///
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
/// (`scratchpad/f37/FcOpen.java`), every row a pasted transcript:
///
/// ```text
/// open(p)                            -> READ; size()==5; write() throws
///                                       java.nio.channels.NonWritableChannelException
/// open(p, READ, APPEND)              -> java.lang.IllegalArgumentException:
///                                       READ + APPEND not allowed
/// open(p, APPEND, TRUNCATE_EXISTING) -> java.lang.IllegalArgumentException:
///                                       APPEND + TRUNCATE_EXISTING not allowed
/// open(missing, WRITE)               -> java.nio.file.NoSuchFileException
/// open(missing, WRITE, CREATE)       -> ok, size()==0
/// open(p, WRITE) then write(1 byte)  -> file length still 5
/// ```
///
/// That last row is the one a plausible implementation gets wrong: **`WRITE`
/// alone does NOT truncate**, and it does NOT create. Only
/// `TRUNCATE_EXISTING` truncates and only `CREATE`/`CREATE_NEW` creates. This
/// matters here because `FileDescriptorTable::open_write` is
/// `.create(true).truncate(!append)`, i.e. it does BOTH unconditionally — so
/// the naive "writable ⇒ `open_write`" mapping would silently destroy the
/// contents of a file opened `WRITE` with no other option.
///
/// The two refusals carry the JDK's exact detail messages, quoted above; a
/// `matches!(.., IllegalArgumentException { .. })` assertion cannot see a
/// wrong one, which is why they are string constants here and compared as
/// strings in the tests.
///
/// Unrecognised names are IGNORED rather than refused, and that is deliberate
/// and different from `parse_afc_open_options`'s policy next door.
/// `AsynchronousFileChannel.open` really does reject an option it cannot
/// honour; `FileChannel.open` accepts `SPARSE`, `SYNC`, `DSYNC`,
/// `DELETE_ON_CLOSE` and provider-specific `ExtendedOpenOption`s, and this VM
/// honours none of them. Refusing them would turn a working open into a
/// failure; ignoring them opens the file with the access the caller asked for
/// and loses only the durability/lifecycle hint. Losing a hint is the smaller
/// wrong answer, and it is the one this function makes deliberately.
pub fn file_channel_open_mode(names: &[&str]) -> Result<FileChannelOpenMode, &'static str> {
    let mut read_opt = false;
    let mut write_opt = false;
    let mut append = false;
    let mut create = false;
    let mut truncate = false;
    for name in names {
        match *name {
            "READ" => read_opt = true,
            "WRITE" => write_opt = true,
            "APPEND" => append = true,
            "CREATE" | "CREATE_NEW" => create = true,
            "TRUNCATE_EXISTING" => truncate = true,
            _ => {}
        }
    }
    if read_opt && append {
        return Err(FC_OPEN_READ_PLUS_APPEND);
    }
    if append && truncate {
        return Err(FC_OPEN_APPEND_PLUS_TRUNCATE);
    }
    let write = write_opt || append;
    Ok(FileChannelOpenMode {
        // "If no options are present then this method works as if the READ
        // option is present" (JDK 25 `FileChannel.open` javadoc) — and the
        // implementation's actual rule is broader than "no options": READ is
        // implied whenever no ACCESS option (READ/WRITE/APPEND) is present, so
        // `open(p, CREATE)` is a read. Written as `!write` rather than
        // `names.is_empty()` for that reason.
        read: read_opt || !write,
        write,
        append,
        create,
        truncate,
    })
}

/// MEASURED detail message, jdk-25.0.3+9. Named constants so the native and
/// its test compare the SAME string and a typo cannot pass both.
pub(crate) const FC_OPEN_READ_PLUS_APPEND: &str = "READ + APPEND not allowed";
pub(crate) const FC_OPEN_APPEND_PLUS_TRUNCATE: &str = "APPEND + TRUNCATE_EXISTING not allowed";

/// `FileChannel.open(Path, OpenOption...)`.
///
/// **The `OpenOption[]` used to be discarded** — the parameter was documented
/// as "(ignored)" and the body carried `// Simplified: open for read (a full
/// impl would check OpenOptions)`, calling `open_read` unconditionally. So
/// `FileChannel.open(p, WRITE, CREATE)` handed back a READ-ONLY fd, and the
/// first `write()` through it failed at the fd layer with a "bad fd"-shaped
/// `IOException` naming nothing the caller had done wrong. A method whose
/// entire second parameter selects the capability, answering with one fixed
/// capability, is the constant-where-state shape at its widest.
///
/// **This native is NOT shadowed** — unlike `size()`/`position()`/`read()`/
/// `write()`/`close()`, which `register_phase57_nio_file` re-registers,
/// `("java/nio/channels/FileChannel", "open",
/// "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/FileChannel;")`
/// is registered HERE and nowhere else (VERIFIED by grepping every
/// `register(fc_cls, …)` in `native-builtins/src/phases_late/nio_file.rs`:
/// `size`, `position` ×2, `close`, `isOpen`, `write`, `read` — no `open`). So
/// this body is the live one in every mode.
///
/// The option algebra and its two refusals are [`file_channel_open_mode`],
/// measured. The mapping from that mode onto `FileDescriptorTable` used to
/// stop being exact right here (NOM F37 N2): every writable arm reused
/// `open_write`/`open_read_write`, neither of which can express "create
/// exactly when asked, truncate exactly when asked" independently, so
/// `APPEND` on a missing file and `WRITE`+`TRUNCATE_EXISTING` without
/// `CREATE` both created a file HotSpot would refuse to. Closed by
/// [`FileDescriptorTable::open_write_options`], which takes `create`,
/// `truncate` and `append` as three independent bits — the same bits
/// [`file_channel_open_mode`] already produced — instead of inferring any of
/// them from the others:
///
/// * **writable** (`WRITE` or `APPEND`) → `open_write_options(path,
///   mode.create, mode.truncate, mode.append)` — exact for every option
///   combination [`file_channel_open_mode`] can produce, including the two
///   rows above.
/// * **read-only** → `open_read(path)`, unchanged.
///
/// A writable channel is opened `.read(true)` as well, so
/// `NonWritableChannelException` / `NonReadableChannelException` are still not
/// enforced (MEASURED: `FileChannel.open(p).write(..)` raises
/// `java.nio.channels.NonWritableChannelException`). That is a missing
/// capability this change neither adds nor worsens — before it, EVERY channel
/// was read-only and `write()` failed on all of them.
pub(crate) fn native_fc_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Path (1-field synthetic, field 0 = String path),
    // args[1] = OpenOption[] — decoded below, not ignored.
    let path_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Read path string from Path object (field 0 is String) or directly if it's a String
    let path_str = ctx
        .read_string(path_obj)
        .or_else(|| path_slot_string(ctx, path_obj))
        .unwrap_or_default();

    // Decode the `OpenOption[]`. Names are read through the same two helpers
    // `AsynchronousFileChannel.open` uses (`read_afc_open_option_name` /
    // `normalize_afc_open_option_name`), so a `StandardOpenOption` enum
    // constant resolves identically for the two entry points — the shape that
    // drifts otherwise is two spellings of "which field holds an enum's name".
    let mut option_names: Vec<String> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        for i in 0..ctx.array_length(*arr) {
            if let Value::Object(Some(option)) = ctx.get_array_element(*arr, i) {
                if let Some(raw) = read_afc_open_option_name(ctx, option) {
                    option_names.push(normalize_afc_open_option_name(&raw));
                }
            }
        }
    }
    let borrowed: Vec<&str> = option_names.iter().map(String::as_str).collect();
    let mode = file_channel_open_mode(&borrowed).map_err(|message| {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalArgumentException {
            message: message.to_string(),
        }))
    })?;

    // Real `FileChannel.open` throws `java.nio.file.NoSuchFileException` (not
    // `FileNotFoundException`) when the target is missing — callers like
    // `FileSystemResource.readableChannel()` explicitly catch
    // `NoSuchFileException` and translate it to `FileNotFoundException`
    // (ResourceTests#resourceCreateRelativeUnknown). Mapping every open
    // failure to a generic `IOException` (as before) made that catch miss,
    // so the raw `IOException` propagated instead. Unchanged by this lane and
    // it now covers the writable arms too: `open(missing, WRITE)` without
    // `CREATE` is a `NoSuchFileException` on HotSpot (MEASURED).
    // The `fd_table()` borrow is scoped to this block: `ctx` is `&mut dyn` and
    // is used mutably again below (`ensure_class_initialized`, `alloc_object`),
    // and a `&FileDescriptorTable` held across those is E0502.
    let opened = {
        let table = ctx.fd_table();
        if mode.write {
            table.open_write_options(&path_str, mode.create, mode.truncate, mode.append)
        } else {
            table.open_read(&path_str)
        }
    };
    let fd_id = opened.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NoSuchFileException {
                path: path_str.clone(),
            }))
        } else {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
                message: format!("FileChannel.open: {e}"),
            }))
        }
    })?;

    // The width and the accessors' base come from ONE function
    // (`synthetic_file_channel`), so the allocator and every reader cannot
    // disagree about where the private map starts. `alloc_object` still clamps
    // UP, so the declared fields are always present too.
    let fc_slots = cratonvm_native_api::synthetic_file_channel::alloc_slots(ctx);
    let fc = match ctx.ensure_class_initialized("java/nio/channels/FileChannel") {
        Ok(cid) => ctx.alloc_object(cid, fc_slots),
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), fc_slots),
    };
    cratonvm_native_api::synthetic_file_channel::set_fd_value(ctx, fc, Value::Int(fd_id as i32));
    cratonvm_native_api::synthetic_file_channel::set_position_value(ctx, fc, Value::Long(0));
    Ok(Some(Value::Object(Some(fc))))
}

/// Read a NUL-terminated byte string from native memory at `addr`.
pub(crate) fn read_native_c_string(ctx: &dyn NativeContext, addr: i64) -> Option<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    for i in 0..4096i64 {
        if !ctx.copy_from_native_memory(addr + i, &mut one) {
            return None;
        }
        if one[0] == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(one[0]);
    }
    None
}

/// `UnixNativeDispatcher.open0(long, int, int)I` -- see its registration.
///
/// Linux `open(2)` flag values. `O_CREAT|O_EXCL` on an existing file is
/// `EEXIST`; a missing file without `O_CREAT` is `ENOENT`. The residuals are
/// the same two `native_fc_open` names: `FdTable` has no "open for write
/// without creating" entry point, and a truncating read-write open is not
/// truncated.
pub(crate) fn native_unix_open0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const O_ACCMODE: i32 = 3;
    const O_CREAT: i32 = 0o100;
    const O_EXCL: i32 = 0o200;
    const O_TRUNC: i32 = 0o1000;
    const O_APPEND: i32 = 0o2000;
    // `UnixChannelFactory.newFileChannel` (real bytecode) sets this bit
    // itself when `LinkOption.NOFOLLOW_LINKS` was requested, and its own
    // caller special-cases the resulting `ELOOP` into a plain `IOException`
    // (see `p57_nofollow_reject`'s doc comment) — so this native's only job
    // is to answer the flag honestly, not to pick the Java exception type.
    // Previously ignored entirely: `UnixFileSystemProvider` overrides
    // `newFileChannel`/`newByteChannel` with its OWN real bytecode (unlike
    // the `Files`-level methods, which are `Bridge` natives over non-native
    // bytecode), so this VM's registered nofollow pre-check on the abstract
    // `FileSystemProvider` base — `p57_nofollow_reject`, called from THIS
    // module's `newFileChannel`/`newByteChannel` registrations — is dead
    // code for a Unix receiver: dispatch resolves to the subclass override
    // before either registration is ever a candidate, and real bytecode
    // funnels every open down to here regardless. `O_NOFOLLOW` silently
    // followed the link (`fd_table().open_*` below has no such flag), so
    // `FileChannel.open(<symlink>, WRITE, CREATE, NOFOLLOW_LINKS)` wrote
    // straight through to the link's target instead of refusing — exactly
    // the attack (an attacker-replaced PID file) the option exists to stop.
    #[cfg(unix)]
    const O_NOFOLLOW: i32 = libc::O_NOFOLLOW;
    #[cfg(not(unix))]
    const O_NOFOLLOW: i32 = 0; // dead: sun/nio/fs/UnixNativeDispatcher never loads here
    let addr = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("open0: path buffer address is not a long".into()),
            }
            .into())
        }
    };
    let flags = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let path = read_native_c_string(&*ctx, addr).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: "open0: unreadable path buffer".into(),
        })
    })?;
    let unix_exception = |ctx: &mut dyn NativeContext, errno: i32| -> MethodCallFailed {
        cratonvm_native_api::throwable_builder::throw_new(
            ctx,
            "sun/nio/fs/UnixException",
            "(I)V",
            &[Value::Int(errno)],
            || {
                RuntimeError::IOException {
                    message: format!("open0 failed, errno {errno}"),
                }
                .into()
            },
        )
    };
    let exists = std::path::Path::new(&path).exists();
    if flags & O_CREAT != 0 && flags & O_EXCL != 0 && exists {
        return Err(unix_exception(ctx, 17));
    }
    // `O_NOFOLLOW`: refuse when the FINAL component is itself a symlink,
    // same `lstat`-before-open approximation `p57_nofollow_reject` uses (real
    // `O_NOFOLLOW` fails atomically inside the kernel open; we cannot do that
    // through `fd_table`'s `std::fs`-based opener, so check immediately
    // before it instead). ELOOP (40) is what a real `open(2)` returns for
    // this case on Linux, and what `UnixChannelFactory`'s caller pattern-
    // matches on to produce the plain `IOException` this option's callers
    // expect.
    if flags & O_NOFOLLOW != 0 {
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_link {
            return Err(unix_exception(ctx, 40));
        }
    }
    let create = flags & O_CREAT != 0;
    // `open_write` (the two write-only arms below) creates unconditionally,
    // which `open(2)` without `O_CREAT` does not: `FileChannel.open(missing,
    // APPEND)` or `(missing, WRITE, TRUNCATE_EXISTING)` handed back a channel
    // over a file it had just created, where HotSpot throws
    // `NoSuchFileException` (`ENOENT`). nio20 wave 3 L1 fix-back
    // (`Nio20CompatCarriers` `fcopts`, `--real-jdk`).
    if !create && flags & O_ACCMODE != 0 && !exists {
        return Err(unix_exception(ctx, 2));
    }
    // `open(2)` applies `mode & ~umask` to a file it creates; `fd_table`'s
    // `std::fs` opener has no mode parameter, so create the file here first
    // (`O_EXCL`, kernel-applied umask) and let the table open the existing one.
    #[cfg(unix)]
    if create && !exists {
        let mode = args.get(2).and_then(|v| v.as_int()).unwrap_or(0o666) as libc::mode_t;
        if let Ok(c_path) = std::ffi::CString::new(path.as_bytes()) {
            // SAFETY: `c_path` is NUL-terminated and owned by this frame.
            let fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                    mode as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` was just opened and is closed exactly once.
                unsafe { libc::close(fd) };
            }
        }
    }
    let opened = {
        let table = ctx.fd_table();
        match flags & O_ACCMODE {
            0 => table.open_read(&path),
            _ if flags & O_APPEND != 0 => table.open_write(&path, true),
            1 if flags & O_TRUNC != 0 => table.open_write(&path, false),
            _ => table.open_read_write(&path, create),
        }
    };
    match opened {
        Ok(id) => Ok(Some(Value::Int(id as i32))),
        Err(e) => {
            let errno = match e.kind() {
                io::ErrorKind::NotFound => 2,
                io::ErrorKind::PermissionDenied => 13,
                io::ErrorKind::AlreadyExists => 17,
                _ => e.raw_os_error().unwrap_or(5),
            };
            Err(unix_exception(ctx, errno))
        }
    }
}

/// Build a `sun.nio.fs.UnixException(errno)`, falling back to an `IOException`
/// when the class will not construct (synthetic-JDK mode has no such class).
///
/// Same shape as the closure inside [`native_unix_open0`]; lifted out when
/// `lstat0` became the second caller.
#[cfg(unix)]
pub(crate) fn unix_exception(ctx: &mut dyn NativeContext, what: &str, errno: i32) -> MethodCallFailed {
    match ctx.new_object_initialized("sun/nio/fs/UnixException", "(I)V", &[Value::Int(errno)]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IOException {
            message: format!("{what} failed, errno {errno}"),
        }
        .into(),
    }
}

/// `std::io::Error` -> the errno the JDK's Unix code branches on.
///
/// `raw_os_error` is the real one whenever the error came from a syscall,
/// which is every case these three natives can produce; the `match` is the
/// fallback for an error synthesised inside `std`.
#[cfg(unix)]
pub(crate) fn unix_errno_of(e: &std::io::Error) -> i32 {
    if let Some(n) = e.raw_os_error() {
        return n;
    }
    match e.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        _ => libc::EIO,
    }
}

/// Copy a `std::fs::Metadata` into a real `sun.nio.fs.UnixFileAttributes`.
///
/// The field names are the ones `javap -p sun.nio.fs.UnixFileAttributes`
/// prints on JDK 25 (Temurin 25.0.4+7, checked on the host this was measured
/// on), and every one of them is read by real JDK bytecode:
/// `isDirectory`/`isRegularFile`/`permissions` read `st_mode`,
/// `UnixFileKey` is `(st_dev, st_ino)`, `owner()`/`group()` read
/// `st_uid`/`st_gid`, and the three `FileTime` accessors read the SEC+NSEC
/// pairs rather than an unsplit millis field -- the split that
/// `unix_attr_time_fields` in `native-builtins` documents, and that made every
/// timestamp on Linux read 1970 while it was written to the wrong name.
///
/// `birthtime_available` is written `false` rather than left at the
/// allocation's zero: the JDK's `creationTime()` branches on it and falls back
/// to `ctime()`, which is what `std::fs::Metadata` can actually supply. A
/// `true` here would publish whatever happened to be in `st_birthtime_sec`.
/// The two `st_birthtime_*` fields are written zero explicitly for the same
/// reason rather than left at the allocation's: `creationTime()` is only
/// supposed to read them when the flag is set, and a field nobody wrote is a
/// field whose value depends on the allocator.
#[cfg(unix)]
pub fn unix_attrs_fill(ctx: &mut dyn NativeContext, attrs: ObjectRef, md: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    for (name, value) in [
        ("st_mode", Value::Int(md.mode() as i32)),
        ("st_nlink", Value::Int(md.nlink() as i32)),
        ("st_uid", Value::Int(md.uid() as i32)),
        ("st_gid", Value::Int(md.gid() as i32)),
        ("st_ino", Value::Long(md.ino() as i64)),
        ("st_dev", Value::Long(md.dev() as i64)),
        ("st_rdev", Value::Long(md.rdev() as i64)),
        ("st_size", Value::Long(md.size() as i64)),
        ("st_atime_sec", Value::Long(md.atime())),
        ("st_atime_nsec", Value::Long(md.atime_nsec())),
        ("st_mtime_sec", Value::Long(md.mtime())),
        ("st_mtime_nsec", Value::Long(md.mtime_nsec())),
        ("st_ctime_sec", Value::Long(md.ctime())),
        ("st_ctime_nsec", Value::Long(md.ctime_nsec())),
        ("st_birthtime_sec", Value::Long(0)),
        ("st_birthtime_nsec", Value::Long(0)),
        ("birthtime_available", Value::Int(0)),
    ] {
        ctx.set_field_by_name(attrs, name, value);
    }
    // `--real-jdk` on Linux: the birth time HotSpot 25's `stat0` reads from
    // `statx(STATX_BTIME)`, which is what `Metadata::created` asks. Paired
    // with `init()` claiming `SUPPORTS_BIRTHTIME` in the same mode (see its
    // registration); `--compatible` keeps the zeros above.
    #[cfg(target_os = "linux")]
    if ctx.is_jdk_only() {
        if let Ok(born) = md.created() {
            let (sec, nsec) = match born.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => (d.as_secs() as i64, i64::from(d.subsec_nanos())),
                Err(e) => {
                    let d = e.duration();
                    let (s, n) = (d.as_secs() as i64, i64::from(d.subsec_nanos()));
                    if n == 0 {
                        (-s, 0)
                    } else {
                        (-s - 1, 1_000_000_000 - n)
                    }
                }
            };
            ctx.set_field_by_name(attrs, "st_birthtime_sec", Value::Long(sec));
            ctx.set_field_by_name(attrs, "st_birthtime_nsec", Value::Long(nsec));
            ctx.set_field_by_name(attrs, "birthtime_available", Value::Int(1));
        }
    }
}

/// The `(pathAddress, attrs)` argument pair `stat0` and `lstat0` share.
#[cfg(unix)]
pub(crate) fn unix_stat_args(what: &str, args: &[Value]) -> Result<(i64, ObjectRef), MethodCallFailed> {
    let addr = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(format!("{what}: path buffer address is not a long")),
            }
            .into())
        }
    };
    let attrs = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(format!("{what}: attrs is null")),
            }
            .into())
        }
    };
    Ok((addr, attrs))
}

/// `sun.nio.fs.UnixNativeDispatcher.stat0(long pathAddress, UnixFileAttributes)`.
///
/// **Returns the errno, and 0 IS success** -- it does not throw. That is the
/// JDK's own signature (`private static native int stat0`), and the half of
/// the contract a reader is most likely to invert: `stat` throws
/// `UnixException(errno)` from the JAVA wrapper, while `stat2` returns the
/// int straight through, which is what `readAttributesIfExists` uses to make
/// "the file is not there" a non-exceptional answer.
///
/// # Why this exists at all (wave 18, linux/x86_64)
///
/// Nothing dispatched it while `java.nio.file.Files` was native. Wave 17
/// retired all 32 `Files` rows, measured on Windows, and on LINUX the retired
/// `Files.exists` then reaches `UnixFileSystemProvider` bytecode ->
/// `UnixFileAttributes.getIfExists` -> here. MEASURED: without these three
/// natives, seven of the lane's own probes and the whole `Files` surface die
/// with `UnsatisfiedLinkError: sun/nio/fs/UnixNativeDispatcher.stat0(...)`,
/// `lstat0(...)` or `access0(JI)I`. They are the only three the entire lane-4
/// probe battery reaches, which is why the wave implements three rather than
/// the class's forty.
#[cfg(unix)]
pub(crate) fn native_unix_stat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (addr, attrs) = unix_stat_args("stat0", args)?;
    let Some(path) = read_native_c_string(&*ctx, addr) else {
        return Ok(Some(Value::Int(libc::EFAULT)));
    };
    match std::fs::metadata(&path) {
        Ok(md) => {
            unix_attrs_fill(ctx, attrs, &md);
            Ok(Some(Value::Int(0)))
        }
        Err(e) => Ok(Some(Value::Int(unix_errno_of(&e)))),
    }
}

/// `sun.nio.fs.UnixNativeDispatcher.lstat0(long pathAddress, UnixFileAttributes)`.
///
/// `void`, and it THROWS `UnixException(errno)` -- the other half of the
/// contract described on [`native_unix_stat0`]. `symlink_metadata` is `lstat`:
/// it must not follow the link, or `Files.walkFileTree`'s symlink-loop
/// detection and every `NOFOLLOW_LINKS` caller read the target's inode.
#[cfg(unix)]
pub(crate) fn native_unix_lstat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (addr, attrs) = unix_stat_args("lstat0", args)?;
    let Some(path) = read_native_c_string(&*ctx, addr) else {
        return Err(unix_exception(ctx, "lstat0", libc::EFAULT));
    };
    match std::fs::symlink_metadata(&path) {
        Ok(md) => {
            unix_attrs_fill(ctx, attrs, &md);
            Ok(None)
        }
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, "lstat0", errno))
        }
    }
}

/// `sun.nio.fs.UnixNativeDispatcher.fstat0(int fd, UnixFileAttributes attrs)`.
///
/// The fd-based sibling of [`native_unix_stat0`], reached from the
/// `FileChannel.size()`/`force()`-adjacent real bytecode paths once those run
/// under `--real-jdk`. Carried over from the independent implementation that
/// landed on `dev` in parallel with this one; it is the one member of the
/// family that side had and this one did not.
///
/// `clone_file` hands back a real `std::fs::File` over this VM's own `FdTable`
/// entry for `fd` -- an actual `fstat(2)` through that open descriptor, not a
/// path re-lookup, which could race a rename or unlink between the `open` and
/// this call. A bad fd is `EBADF` rather than a Java NPE, because that is what
/// the JDK's caller catches and maps.
#[cfg(unix)]
pub(crate) fn native_unix_fstat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Int(fd)) = args.first().copied() else {
        return Err(unix_exception(ctx, "fstat0", libc::EFAULT));
    };
    let attrs = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("fstat0: attrs is null".into()),
            }
            .into())
        }
    };
    let file = match ctx.fd_table().clone_file(fd as FdId) {
        Ok(f) => f,
        Err(_) => return Err(unix_exception(ctx, "fstat0", libc::EBADF)),
    };
    match file.metadata() {
        Ok(md) => {
            unix_attrs_fill(ctx, attrs, &md);
            Ok(None)
        }
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, "fstat0", errno))
        }
    }
}

/// `sun.nio.fs.UnixNativeDispatcher.access0(long pathAddress, int amode)`.
///
/// Returns the errno (0 on success), like [`native_unix_stat0`]. The real
/// `access(2)` is used rather than a permission-bit computation from
/// `Metadata`: the answer depends on the effective uid/gid, the supplementary
/// groups, ACLs and the mount's `ro` flag, and a re-derivation from `st_mode`
/// gets every one of those wrong for root (who passes `W_OK` on a 0444 file)
/// and for a read-only mount (who does not, on a 0666 one).
#[cfg(unix)]
pub(crate) fn native_unix_access0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("access0: path buffer address is not a long".into()),
            }
            .into())
        }
    };
    let amode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let Some(path) = read_native_c_string(&*ctx, addr) else {
        return Ok(Some(Value::Int(libc::EFAULT)));
    };
    let Ok(c_path) = std::ffi::CString::new(path) else {
        // An interior NUL cannot name a file on this platform.
        return Ok(Some(Value::Int(libc::ENOENT)));
    };
    // SAFETY: `c_path` is a NUL-terminated buffer owned by this frame and
    // `access` does not retain it.
    let rc = unsafe { libc::access(c_path.as_ptr(), amode) };
    if rc == 0 {
        Ok(Some(Value::Int(0)))
    } else {
        Ok(Some(Value::Int(unix_errno_of(
            &std::io::Error::last_os_error(),
        ))))
    }
}

/// Read the one `long pathAddress` argument every path-based
/// `UnixNativeDispatcher` native starts with.
///
/// A `None` is `EFAULT` rather than a Rust panic or a Java NPE: the JDK's
/// callers catch `UnixException` and map it, and an unreadable buffer is
/// exactly what `EFAULT` names.
#[cfg(unix)]
pub(crate) fn unix_path_arg(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Result<String, i32> {
    match args.get(idx) {
        Some(Value::Long(addr)) => read_native_c_string(ctx, *addr).ok_or(libc::EFAULT),
        _ => Err(libc::EFAULT),
    }
}

/// `Result<(), std::io::Error>` -> the `void`-returning native's contract:
/// `Ok(None)`, or a thrown `UnixException(errno)`.
#[cfg(unix)]
pub(crate) fn unix_void_result(
    ctx: &mut dyn NativeContext,
    what: &str,
    r: std::io::Result<()>,
) -> MethodCallResult {
    match r {
        Ok(()) => Ok(None),
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, what, errno))
        }
    }
}

/// The path-based `sun.nio.fs.UnixNativeDispatcher` natives that the retired
/// `java.nio.file.Files` surface reaches on Linux.
///
/// # How this set was chosen -- one measured layer at a time
///
/// Not from the class's declaration list, which is 48 natives. Wave 18 ran the
/// lane's whole probe battery under `--real-jdk` and implemented exactly the
/// natives the run named in an `UnsatisfiedLinkError`, then re-ran it: the
/// first pass named `stat0`, `lstat0` and `access0`; with those in, every
/// remaining failure named `unlink0`. Each layer of the JDK's own file code is
/// only reachable once the layer above it stops throwing, so a list written up
/// front is a guess and a list written from the errors is a measurement. The
/// batch here is that second layer plus its immediate siblings (the create /
/// delete / rename / link / permission family), because they are one `mkdir`
/// away from each other in `UnixFileSystemProvider` and a build costs 15
/// minutes.
///
/// NOT implemented, and each for a reason rather than an oversight:
///
/// * `openat0`, `unlinkat0`, `renameat0`, `fchmodat0`, `utimensat0`,
///   `futimens0`, `fchmodatNoFollowSupported0` -- the `*at` family is
///   capability-gated behind `init()`, which this VM answers `0` (no optional
///   syscalls), so the JDK takes its portable fallbacks and never calls them.
/// * `opendir0`/`readdir0`/`closedir`/`fdopendir` -- directory iteration does
///   not reach the JDK: `FileSystemProvider.newDirectoryStream` is this VM's
///   own native and materialises the listing itself.
/// * `read0`/`write0`/`fgetxattr0`/... -- the channel and xattr families, which
///   the `sun/nio/ch` package verdict keeps native.
/// * `statvfs0` -- `getFileStore` is a VM native, so `UnixFileStore` never
///   asks. The first probe that reaches it is its measurement.
#[cfg(unix)]
pub(crate) fn native_unix_unlink0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => unix_void_result(ctx, "unlink0", std::fs::remove_file(p)),
        Err(errno) => Err(unix_exception(ctx, "unlink0", errno)),
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_rmdir0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => unix_void_result(ctx, "rmdir0", std::fs::remove_dir(p)),
        Err(errno) => Err(unix_exception(ctx, "rmdir0", errno)),
    }
}

/// `mkdir0(long pathAddress, int mode)`.
///
/// `libc::mkdir` rather than `std::fs::create_dir`: the mode is the caller's
/// (`Files.createDirectory(dir, PosixFilePermissions.asFileAttribute(...))`
/// passes it), and `create_dir` would apply the process umask default and
/// silently drop it -- the same defect the `Files.createDirectory` native had.
#[cfg(unix)]
pub(crate) fn native_unix_mkdir0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let p = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "mkdir0", errno)),
    };
    let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0o777) as libc::mode_t;
    let Ok(c_path) = std::ffi::CString::new(p) else {
        return Err(unix_exception(ctx, "mkdir0", libc::ENOENT));
    };
    // SAFETY: `c_path` is NUL-terminated and owned by this frame.
    if unsafe { libc::mkdir(c_path.as_ptr(), mode) } == 0 {
        Ok(None)
    } else {
        let errno = unix_errno_of(&std::io::Error::last_os_error());
        Err(unix_exception(ctx, "mkdir0", errno))
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_rename0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "rename0", errno)),
    };
    let to = match unix_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "rename0", errno)),
    };
    unix_void_result(ctx, "rename0", std::fs::rename(from, to))
}

/// `link0(long existingAddress, long newAddress)` -- argument order is
/// EXISTING first, which is `link(2)`'s and the opposite of how the Java
/// wrapper reads (`link(UnixPath existing, UnixPath newfile)`).
#[cfg(unix)]
pub(crate) fn native_unix_link0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let existing = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "link0", errno)),
    };
    let newfile = match unix_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "link0", errno)),
    };
    unix_void_result(ctx, "link0", std::fs::hard_link(existing, newfile))
}

/// `symlink0(long name1, long name2)` = `symlink(target, linkpath)`.
///
/// The Java wrapper is `symlink(byte[] name1, UnixPath name2)` and `name1` is
/// the TARGET -- the string stored IN the link, which need not exist. Getting
/// this pair the wrong way round produces a link that resolves to the wrong
/// place and no error at all, which is why it is spelled out here.
#[cfg(unix)]
pub(crate) fn native_unix_symlink0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let target = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "symlink0", errno)),
    };
    let link = match unix_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "symlink0", errno)),
    };
    unix_void_result(ctx, "symlink0", std::os::unix::fs::symlink(target, link))
}

/// `readlink0` and `realpath0` both answer a `byte[]`, and both must NOT be
/// `String`-round-tripped through anything lossy: a Unix path is bytes, and
/// `read_link` hands them back as an `OsString` whose bytes we take directly.
#[cfg(unix)]
pub(crate) fn unix_bytes_result(
    ctx: &mut dyn NativeContext,
    what: &str,
    r: std::io::Result<std::path::PathBuf>,
) -> MethodCallResult {
    use std::os::unix::ffi::OsStrExt;
    match r {
        Ok(p) => {
            let bytes = p.as_os_str().as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, bytes);
            Ok(Some(Value::Object(Some(arr))))
        }
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, what, errno))
        }
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_readlink0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => {
            let r = std::fs::read_link(p);
            unix_bytes_result(ctx, "readlink0", r)
        }
        Err(errno) => Err(unix_exception(ctx, "readlink0", errno)),
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_realpath0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => {
            let r = std::fs::canonicalize(p);
            unix_bytes_result(ctx, "realpath0", r)
        }
        Err(errno) => Err(unix_exception(ctx, "realpath0", errno)),
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_chmod0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let p = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "chmod0", errno)),
    };
    let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u32;
    use std::os::unix::fs::PermissionsExt;
    let r = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
    unix_void_result(ctx, "chmod0", r)
}

/// `chown0` / `lchown0`. `libc::chown` has no `std` equivalent; the two differ
/// only in whether a symlink is followed, which is the whole point of the
/// second spelling.
#[cfg(unix)]
pub(crate) fn unix_chown(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    what: &str,
    follow: bool,
) -> MethodCallResult {
    let p = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, what, errno)),
    };
    let uid = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1) as libc::uid_t;
    let gid = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1) as libc::gid_t;
    let Ok(c_path) = std::ffi::CString::new(p) else {
        return Err(unix_exception(ctx, what, libc::ENOENT));
    };
    // SAFETY: `c_path` is NUL-terminated and owned by this frame.
    let rc = unsafe {
        if follow {
            libc::chown(c_path.as_ptr(), uid, gid)
        } else {
            libc::lchown(c_path.as_ptr(), uid, gid)
        }
    };
    if rc == 0 {
        Ok(None)
    } else {
        let errno = unix_errno_of(&std::io::Error::last_os_error());
        Err(unix_exception(ctx, what, errno))
    }
}

#[cfg(unix)]
pub(crate) fn native_unix_chown0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unix_chown(ctx, args, "chown0", true)
}

#[cfg(unix)]
pub(crate) fn native_unix_lchown0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unix_chown(ctx, args, "lchown0", false)
}

/// `close0(int fd)`. The fd is an `FdTable` id -- the same id space `open0`
/// hands out -- not an OS descriptor, so this closes through the table.
#[cfg(unix)]
pub(crate) fn native_unix_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    if fd < 0 {
        return Err(unix_exception(ctx, "close0", libc::EBADF));
    }
    let r = ctx.fd_table().close(fd as u32);
    match r {
        Ok(()) => Ok(None),
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, "close0", errno))
        }
    }
}

/// `strerror(int errno)` -- what `UnixException.getMessage()` prints. Without
/// it every `UnixException` message is an `UnsatisfiedLinkError` instead of
/// the reason the operation failed.
#[cfg(unix)]
pub(crate) fn native_unix_strerror(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let errno = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    // `std` renders "No such file or directory (os error 2)"; the JDK prints
    // only what `strerror` returns, and the suffix would land in every
    // `NoSuchFileException`'s message.
    let text = text.split(" (os error").next().unwrap_or(&text).to_string();
    let bytes = text.as_bytes();
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    ctx.write_byte_array_from(arr, 0, bytes);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// SBR wave 2 (lane fs): the `UnixNativeDispatcher` / `LinuxNativeDispatcher`
// natives the retired `sun/nio/fs` Linux shadows reach once their real bytecode
// runs. See `RETIRED_SHADOW_SBR_FS_TRIPLES` in `native-api/src/retired_shadow.rs`
// and `docs/feature-designs/real-jdk-mode.md`.
//
// Every one is genuinely `ACC_NATIVE` in the JDK 25 image (`javap -p`), so each
// is a §1.5 bridge with no bytecode to prefer. They are registered in both
// modes (`register_nio_file_natives`): `--real-jdk` may only refuse rows, never
// add them, and in `--compatible` a correct `ACC_NATIVE` body can only move an
// answer from `UnsatisfiedLinkError` toward HotSpot.
//
// Descriptor fds are `FdTable` ids (the id space `open0` hands out), never OS
// descriptors; `clone_file` answers a `dup` of the entry's kernel file.
// ---------------------------------------------------------------------------

/// The `FdTable` entry `fd` as a `dup`'d kernel file, or the errno the JDK's
/// caller maps (`EBADF`).
#[cfg(unix)]
pub(crate) fn unix_fd_file(ctx: &mut dyn NativeContext, fd: i32) -> Result<std::fs::File, i32> {
    if fd < 0 {
        return Err(libc::EBADF);
    }
    ctx.fd_table()
        .clone_file(fd as FdId)
        .map_err(|_| libc::EBADF)
}

/// `last_os_error` as a thrown `UnixException`.
#[cfg(unix)]
pub(crate) fn unix_last_errno(ctx: &mut dyn NativeContext, what: &str) -> MethodCallFailed {
    let errno = unix_errno_of(&std::io::Error::last_os_error());
    unix_exception(ctx, what, errno)
}

/// `opendir0(long pathAddress)` -> the `DIR*` as a `long`. Reached from real
/// `UnixFileSystemProvider.newDirectoryStream`, which takes the plain
/// `UnixDirectoryStream` arm because `init()` reports no `openat` support.
#[cfg(unix)]
pub(crate) fn native_unix_opendir0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let p = match unix_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "opendir0", errno)),
    };
    let Ok(c_path) = std::ffi::CString::new(p) else {
        return Err(unix_exception(ctx, "opendir0", libc::ENOENT));
    };
    // SAFETY: `c_path` is NUL-terminated and owned by this frame; the returned
    // `DIR*` is owned by the Java `UnixDirectoryStream`, which `closedir`s it.
    let dir = unsafe { libc::opendir(c_path.as_ptr()) };
    if dir.is_null() {
        return Err(unix_last_errno(ctx, "opendir0"));
    }
    Ok(Some(Value::Long(dir as i64)))
}

/// `readdir0(long dir)` -> `d_name` bytes, or `null` at the end of the stream.
/// `.`/`..` are returned as the kernel returns them; `UnixDirectoryStream`'s
/// iterator skips them itself.
#[cfg(unix)]
pub(crate) fn native_unix_readdir0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dir = match args.first() {
        Some(Value::Long(v)) if *v != 0 => *v as *mut libc::DIR,
        _ => return Err(unix_exception(ctx, "readdir0", libc::EBADF)),
    };
    #[cfg(target_os = "linux")]
    unsafe fn errno_ptr() -> *mut i32 {
        libc::__errno_location()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn errno_ptr() -> *mut i32 {
        libc::__error()
    }
    // SAFETY: `dir` is a `DIR*` this module's `opendir0` returned and the Java
    // stream has not yet closed (it serialises readdir/closedir under its own
    // lock). errno is cleared first so end-of-stream and failure are told apart.
    let (ent, errno) = unsafe {
        *errno_ptr() = 0;
        let ent = libc::readdir(dir);
        (ent, *errno_ptr())
    };
    if ent.is_null() {
        if errno != 0 {
            return Err(unix_exception(ctx, "readdir0", errno));
        }
        return Ok(Some(Value::Object(None)));
    }
    // SAFETY: `ent` is non-null and `d_name` is a NUL-terminated array inside it.
    let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
    let arr = ctx.new_array(ArrayElementType::Byte, name.len());
    ctx.write_byte_array_from(arr, 0, name);
    Ok(Some(Value::Object(Some(arr))))
}

/// `closedir(long dir)`.
#[cfg(unix)]
pub(crate) fn native_unix_closedir(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dir = match args.first() {
        Some(Value::Long(v)) if *v != 0 => *v as *mut libc::DIR,
        _ => return Err(unix_exception(ctx, "closedir", libc::EBADF)),
    };
    // SAFETY: see `native_unix_readdir0`; the Java side closes exactly once.
    if unsafe { libc::closedir(dir) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "closedir"))
    }
}

/// Nanoseconds since the epoch -> the `timespec` the kernel takes.
#[cfg(unix)]
pub(crate) fn unix_timespec_of_nanos(nanos: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: nanos.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: nanos.rem_euclid(1_000_000_000) as _,
    }
}

/// `futimens0(int fd, long accessNanos, long modifiedNanos)` -- what real
/// `UnixFileAttributeViews$Basic.setTimes` calls on the descriptor
/// `openForAttributeAccess` opened (`Files.setLastModifiedTime`,
/// `BasicFileAttributeView.setTimes`, `Files.copy(COPY_ATTRIBUTES)`).
#[cfg(unix)]
pub(crate) fn native_unix_futimens0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let times = [
        unix_timespec_of_nanos(args.get(1).and_then(|v| v.as_long()).unwrap_or(0)),
        unix_timespec_of_nanos(args.get(2).and_then(|v| v.as_long()).unwrap_or(0)),
    ];
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "futimens0", errno)),
    };
    // SAFETY: `file` owns a live descriptor for the call; `times` is two
    // initialised `timespec`s.
    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "futimens0"))
    }
}

/// `utimensat0(int dfd, long pathAddress, long accessNanos, long modifiedNanos,
/// int flags)` -- the path arm of `setTimes`, taken when the file could not be
/// opened (a symlink under `NOFOLLOW_LINKS`, a FIFO). Only `AT_FDCWD` reaches
/// here: every `*at` directory descriptor comes from `SecureDirectoryStream`,
/// which `init()`'s capability answer keeps the JDK from building.
#[cfg(unix)]
pub(crate) fn native_unix_utimensat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dfd = args
        .first()
        .and_then(|v| v.as_int())
        .unwrap_or(libc::AT_FDCWD);
    if dfd != libc::AT_FDCWD {
        return Err(unix_exception(ctx, "utimensat0", libc::EBADF));
    }
    let p = match unix_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "utimensat0", errno)),
    };
    let times = [
        unix_timespec_of_nanos(args.get(2).and_then(|v| v.as_long()).unwrap_or(0)),
        unix_timespec_of_nanos(args.get(3).and_then(|v| v.as_long()).unwrap_or(0)),
    ];
    let flags = args.get(4).and_then(|v| v.as_int()).unwrap_or(0);
    let Ok(c_path) = std::ffi::CString::new(p) else {
        return Err(unix_exception(ctx, "utimensat0", libc::ENOENT));
    };
    // SAFETY: NUL-terminated path and two initialised `timespec`s, both owned
    // by this frame.
    if unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), flags) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "utimensat0"))
    }
}

/// `fchmod0(int fd, int mode)` -- `Files.copy(COPY_ATTRIBUTES)`'s permission
/// copy and `PosixFileAttributeView.setPermissions` on an opened file.
#[cfg(unix)]
pub(crate) fn native_unix_fchmod0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as libc::mode_t;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fchmod0", errno)),
    };
    // SAFETY: `file` owns a live descriptor for the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "fchmod0"))
    }
}

/// `fchown0(int fd, int uid, int gid)`.
#[cfg(unix)]
pub(crate) fn native_unix_fchown0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let uid = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1) as libc::uid_t;
    let gid = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1) as libc::gid_t;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fchown0", errno)),
    };
    // SAFETY: `file` owns a live descriptor for the call.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "fchown0"))
    }
}

/// Copy the rest of `src` into `dst`, both `FdTable` ids, through their shared
/// kernel offsets -- what `copy_file_range`/`sendfile` with NULL offsets do in
/// the JDK's `directCopy0` and what its `bufferedCopy0` loop does through a
/// direct buffer. The buffer and the cancel-poll address are not needed: the
/// copy is one uninterruptible call either way.
#[cfg(unix)]
pub(crate) fn unix_fd_copy(
    ctx: &mut dyn NativeContext,
    what: &str,
    dst: i32,
    src: i32,
) -> Result<(), MethodCallFailed> {
    let mut from = match unix_fd_file(ctx, src) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, what, errno)),
    };
    let mut to = match unix_fd_file(ctx, dst) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, what, errno)),
    };
    match std::io::copy(&mut from, &mut to) {
        Ok(_) => Ok(()),
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, what, errno))
        }
    }
}

/// `LinuxNativeDispatcher.directCopy0(int dst, int src, long cancel)` -> 0.
#[cfg(unix)]
pub(crate) fn native_linux_direct_copy0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dst = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let src = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    unix_fd_copy(ctx, "directCopy0", dst, src)?;
    Ok(Some(Value::Int(0)))
}

/// `UnixFileSystem.bufferedCopy0(int dst, int src, long address, int size,
/// long cancel)` -- the zero-length-file arm of `copyFile`.
#[cfg(unix)]
pub(crate) fn native_unix_buffered_copy0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dst = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let src = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    unix_fd_copy(ctx, "bufferedCopy0", dst, src)?;
    Ok(None)
}

// --- the `*at` family behind `SUPPORTS_OPENAT` ------------------------------
//
// HotSpot 25 on Linux answers `openatSupported()`, so real
// `UnixFileSystemProvider.newDirectoryStream` hands back a
// `UnixSecureDirectoryStream` -- the type a caller checks for before a safe
// recursive delete (Guava's `MoreFiles.deleteRecursively` refuses an insecure
// stream). Claiming the capability (under `--real-jdk`, see `init()`) is only
// honest with the natives a secure stream calls: `dup`, `fdopendir`,
// `openat0`, `fstatat0`, `unlinkat0`, `renameat0`, `fchmodat0` and
// `fchmodatNoFollowSupported0`. Directory descriptors are `FdTable` ids like
// every other; each call borrows a `dup` of the entry's kernel fd.

/// The C path at `args[idx]` as a `CString`, or the errno to throw.
#[cfg(unix)]
pub(crate) fn unix_c_path_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<std::ffi::CString, i32> {
    let p = unix_path_arg(ctx, args, idx)?;
    std::ffi::CString::new(p).map_err(|_| libc::ENOENT)
}

/// Park a kernel fd this module just opened in the `FdTable`, answering its id.
#[cfg(unix)]
pub(crate) fn unix_insert_raw_fd(ctx: &mut dyn NativeContext, raw: i32) -> FdId {
    use std::os::unix::io::FromRawFd;
    // SAFETY: `raw` is a descriptor the caller just obtained from the kernel
    // and owns exclusively; the `File` takes that ownership.
    let file = unsafe { std::fs::File::from_raw_fd(raw) };
    ctx.fd_table().insert_win32_file(file)
}

/// `dup(int fd)` -> a second `FdTable` id over a `dup` of the same kernel file.
#[cfg(unix)]
pub(crate) fn native_unix_dup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    match unix_fd_file(ctx, fd) {
        Ok(f) => Ok(Some(Value::Int(ctx.fd_table().insert_win32_file(f) as i32))),
        Err(errno) => Err(unix_exception(ctx, "dup", errno)),
    }
}

/// `fdopendir(int dfd)` -> `DIR*`. Like the C call, it CONSUMES `dfd`: the
/// JDK never closes the descriptor it passes here (`closedir` does), so the
/// table entry is released and the `DIR*` owns the kernel fd from now on.
#[cfg(unix)]
pub(crate) fn native_unix_fdopendir(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::IntoRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fdopendir", errno)),
    };
    let raw = file.into_raw_fd();
    // SAFETY: `raw` is owned here (a `dup`); on success the `DIR*` owns it.
    let dir = unsafe { libc::fdopendir(raw) };
    if dir.is_null() {
        let err = unix_last_errno(ctx, "fdopendir");
        // SAFETY: fdopendir failed, so `raw` is still ours to close.
        unsafe { libc::close(raw) };
        return Err(err);
    }
    let _ = ctx.fd_table().close(fd as FdId);
    Ok(Some(Value::Long(dir as i64)))
}

/// `openat0(int dfd, long pathAddress, int oflags, int mode)` -> new fd id.
#[cfg(unix)]
pub(crate) fn native_unix_openat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let dfd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let path = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "openat0", errno)),
    };
    let flags = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let mode = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as libc::c_uint;
    let dir = match unix_fd_file(ctx, dfd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "openat0", errno)),
    };
    // SAFETY: `dir` keeps its descriptor alive across the call; `path` is
    // NUL-terminated and owned by this frame.
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            path.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if raw < 0 {
        return Err(unix_last_errno(ctx, "openat0"));
    }
    Ok(Some(Value::Int(unix_insert_raw_fd(ctx, raw) as i32)))
}

/// `fstatat0(int dfd, long pathAddress, int flag, UnixFileAttributes attrs)`.
/// Opened `O_PATH` (plus `O_NOFOLLOW` for `AT_SYMLINK_NOFOLLOW`) and `fstat`ed,
/// so the fill is the same `Metadata` one `stat0` uses, birth time included.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_fstatat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let dfd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let path = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "fstatat0", errno)),
    };
    let flag = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let attrs = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("fstatat0: attrs is null".into()),
            }
            .into())
        }
    };
    let dir = match unix_fd_file(ctx, dfd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fstatat0", errno)),
    };
    let mut oflags = libc::O_PATH | libc::O_CLOEXEC;
    if flag & libc::AT_SYMLINK_NOFOLLOW != 0 {
        oflags |= libc::O_NOFOLLOW;
    }
    // SAFETY: as in `native_unix_openat0`.
    let raw = unsafe { libc::openat(dir.as_raw_fd(), path.as_ptr(), oflags) };
    if raw < 0 {
        return Err(unix_last_errno(ctx, "fstatat0"));
    }
    // SAFETY: `raw` was just opened and is owned by this `File`.
    let target = unsafe { std::fs::File::from_raw_fd(raw) };
    match target.metadata() {
        Ok(md) => {
            unix_attrs_fill(ctx, attrs, &md);
            Ok(None)
        }
        Err(e) => {
            let errno = unix_errno_of(&e);
            Err(unix_exception(ctx, "fstatat0", errno))
        }
    }
}

/// `unlinkat0(int dfd, long pathAddress, int flag)`.
#[cfg(unix)]
pub(crate) fn native_unix_unlinkat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let dfd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let path = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "unlinkat0", errno)),
    };
    let flag = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let dir = match unix_fd_file(ctx, dfd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "unlinkat0", errno)),
    };
    // SAFETY: as in `native_unix_openat0`.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), path.as_ptr(), flag) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "unlinkat0"))
    }
}

/// `renameat0(int fromfd, long from, int tofd, long to)`.
#[cfg(unix)]
pub(crate) fn native_unix_renameat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fromfd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let tofd = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1);
    let (from, to) = match (
        unix_c_path_arg(&*ctx, args, 1),
        unix_c_path_arg(&*ctx, args, 3),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(errno), _) | (_, Err(errno)) => return Err(unix_exception(ctx, "renameat0", errno)),
    };
    let (fdir, tdir) = match (unix_fd_file(ctx, fromfd), unix_fd_file(ctx, tofd)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(errno), _) | (_, Err(errno)) => return Err(unix_exception(ctx, "renameat0", errno)),
    };
    // SAFETY: both descriptors live across the call; both paths NUL-terminated.
    if unsafe {
        libc::renameat(
            fdir.as_raw_fd(),
            from.as_ptr(),
            tdir.as_raw_fd(),
            to.as_ptr(),
        )
    } == 0
    {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "renameat0"))
    }
}

/// `fchmodat0(int dfd, long pathAddress, int mode, int flag)`.
#[cfg(unix)]
pub(crate) fn native_unix_fchmodat0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let dfd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let path = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "fchmodat0", errno)),
    };
    let mode = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as libc::mode_t;
    let flag = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let dir = match unix_fd_file(ctx, dfd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fchmodat0", errno)),
    };
    // SAFETY: as in `native_unix_openat0`.
    if unsafe { libc::fchmodat(dir.as_raw_fd(), path.as_ptr(), mode, flag) } == 0 {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "fchmodat0"))
    }
}

// --- extended attributes, as `Files.copy(COPY_ATTRIBUTES)` reaches them ------
//
// `LinuxFileSystem.copyNonPosixAttributes` copies every extended attribute
// with `flistxattr`/`fgetxattr0`/`fsetxattr0` UNCONDITIONALLY -- it does not
// consult `xattrSupported()` -- so a real `Files.copy(.., COPY_ATTRIBUTES)`
// died with `UnsatisfiedLinkError: flistxattr` (MEASURED, `SbrW2Fs` on the
// fourth trial binary). The buffers are JDK `NativeBuffer`s: native memory
// addressed by `long`, copied through `copy_{from,to}_native_memory`.

/// Read `len` bytes of native memory at `addr` (`None` if unreadable).
#[cfg(target_os = "linux")]
pub(crate) fn unix_native_bytes(ctx: &dyn NativeContext, addr: i64, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    if len == 0 || ctx.copy_from_native_memory(addr, &mut buf) {
        Some(buf)
    } else {
        None
    }
}

/// `flistxattr(int fd, long listAddress, int size)` -> bytes of the NUL-
/// separated name list (or the needed size when `size` is 0).
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_flistxattr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let addr = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    let size = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "flistxattr", errno)),
    };
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is `size` writable bytes owned by this frame.
    let n = unsafe { libc::flistxattr(file.as_raw_fd(), buf.as_mut_ptr().cast(), size) };
    if n < 0 {
        return Err(unix_last_errno(ctx, "flistxattr"));
    }
    if size > 0 && !ctx.copy_to_native_memory(addr, &buf[..n as usize]) {
        return Err(unix_exception(ctx, "flistxattr", libc::EFAULT));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `fgetxattr0(int fd, long nameAddress, long valueAddress, int valueLen)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_fgetxattr0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let name = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(n) => n,
        Err(errno) => return Err(unix_exception(ctx, "fgetxattr0", errno)),
    };
    let addr = args.get(2).and_then(|v| v.as_long()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fgetxattr0", errno)),
    };
    let mut buf = vec![0u8; len];
    // SAFETY: NUL-terminated name, `len` writable bytes, live descriptor.
    let n = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            len,
        )
    };
    if n < 0 {
        return Err(unix_last_errno(ctx, "fgetxattr0"));
    }
    if len > 0 && !ctx.copy_to_native_memory(addr, &buf[..n as usize]) {
        return Err(unix_exception(ctx, "fgetxattr0", libc::EFAULT));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `statvfs0(long pathAddress, UnixFileStoreAttributes attrs)` -- the
/// `f_frsize`/`f_blocks`/`f_bfree`/`f_bavail` the JDK's own native copies.
/// Reached from `UnixFileSystem.temporaryBufferSize`, i.e. from every real
/// `Files.copy` of an EMPTY file (the `bufferedCopy` arm; MEASURED, `SbrW2Fs`
/// on the fifth trial binary).
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_statvfs0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let path = match unix_c_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "statvfs0", errno)),
    };
    let attrs = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("statvfs0: attrs is null".into()),
            }
            .into())
        }
    };
    // SAFETY: an all-zero `statvfs` is a valid value for the kernel to fill;
    // `path` is NUL-terminated and owned by this frame.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `st` is writable for this call.
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Err(unix_last_errno(ctx, "statvfs0"));
    }
    for (name, v) in [
        ("f_frsize", st.f_frsize as i64),
        ("f_blocks", st.f_blocks as i64),
        ("f_bfree", st.f_bfree as i64),
        ("f_bavail", st.f_bavail as i64),
    ] {
        ctx.set_field_by_name(attrs, name, Value::Long(v));
    }
    Ok(None)
}

/// `fsetxattr0(int fd, long nameAddress, long valueAddress, int valueLen)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_fsetxattr0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let name = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(n) => n,
        Err(errno) => return Err(unix_exception(ctx, "fsetxattr0", errno)),
    };
    let addr = args.get(2).and_then(|v| v.as_long()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let Some(value) = unix_native_bytes(&*ctx, addr, len) else {
        return Err(unix_exception(ctx, "fsetxattr0", libc::EFAULT));
    };
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "fsetxattr0", errno)),
    };
    // SAFETY: NUL-terminated name, `len` readable bytes, live descriptor.
    if unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            len,
            0,
        )
    } == 0
    {
        Ok(None)
    } else {
        Err(unix_last_errno(ctx, "fsetxattr0"))
    }
}

// ---------------------------------------------------------------------------
// SBR wave 5 (lane fs2): the natives under the real `LinuxWatchService` and
// the real `LinuxFileStore`, which `RETIRED_SHADOW_SBR_FS2_TRIPLES`
// (`native-api/src/retired_shadow.rs`) hands to JDK bytecode. Each is a
// genuine `ACC_NATIVE` method on the JDK 25 image (`javap -p`), implemented
// with the same system call the JDK's own C does, registered in BOTH modes
// explicitly `Bridge` (see `register_nio_file_natives`). Record:
// `docs/feature-designs/real-jdk-mode.md`.
//
// Descriptors are `FdTable` ids like every other `UnixNativeDispatcher` fd
// (`close0` closes through the table), so the inotify instance and both
// socketpair ends are parked there (`unix_insert_raw_fd`) and each call
// borrows a `dup` of the kernel fd (`unix_fd_file`). `O_NONBLOCK` is a file
// STATUS flag shared by every dup, and an inotify watch belongs to the
// instance, not the descriptor, so a dup is exact for every call below.
// ---------------------------------------------------------------------------

/// `UnixNativeDispatcher.read0(int fd, long buf, int nbyte)` -- `read(2)` into
/// native memory. The real `LinuxWatchService$Poller` reads the inotify
/// instance and the wakeup socket through it; both are non-blocking, and an
/// `EAGAIN` is thrown as `UnixException(EAGAIN)`, which the poller catches.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let addr = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "read", errno)),
    };
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is `len` writable bytes owned by this frame; live fd.
    let n = unsafe { libc::read(file.as_raw_fd(), buf.as_mut_ptr().cast(), len) };
    if n < 0 {
        return Err(unix_last_errno(ctx, "read"));
    }
    if n > 0 && !ctx.copy_to_native_memory(addr, &buf[..n as usize]) {
        return Err(unix_exception(ctx, "read", libc::EFAULT));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `UnixNativeDispatcher.write0(int fd, long buf, int nbyte)` -- `write(2)`
/// from native memory (the watch service's one-byte wakeup).
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let addr = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
    let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let Some(data) = unix_native_bytes(&*ctx, addr, len) else {
        return Err(unix_exception(ctx, "write", libc::EFAULT));
    };
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "write", errno)),
    };
    // SAFETY: `data` is `len` readable bytes owned by this frame; live fd.
    let n = unsafe { libc::write(file.as_raw_fd(), data.as_ptr().cast(), len) };
    if n < 0 {
        return Err(unix_last_errno(ctx, "write"));
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `LinuxWatchService.eventSize()` -- `sizeof(struct inotify_event)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_event_size(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        std::mem::size_of::<libc::inotify_event>() as i32
    )))
}

/// `LinuxWatchService.eventOffsets()` -- `offsetof` `wd`, `mask`, `cookie`,
/// `len`, `name`, in that order (the JDK's C fills a 5-element array).
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_event_offsets(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let offsets = [
        std::mem::offset_of!(libc::inotify_event, wd) as i32,
        std::mem::offset_of!(libc::inotify_event, mask) as i32,
        std::mem::offset_of!(libc::inotify_event, cookie) as i32,
        std::mem::offset_of!(libc::inotify_event, len) as i32,
        // `name` is the flexible array member right after the fixed header.
        std::mem::size_of::<libc::inotify_event>() as i32,
    ];
    let arr = ctx.new_array(ArrayElementType::Int, offsets.len());
    for (i, v) in offsets.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*v));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `LinuxWatchService.inotifyInit()` -- `inotify_init()`; the instance is
/// parked in the `FdTable` and its id answered.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_inotify_init(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // SAFETY: no arguments; answers a new descriptor or -1.
    let raw = unsafe { libc::inotify_init() };
    if raw < 0 {
        return Err(unix_last_errno(ctx, "inotify_init"));
    }
    Ok(Some(Value::Int(unix_insert_raw_fd(ctx, raw) as i32)))
}

/// `LinuxWatchService.inotifyAddWatch(int fd, long pathAddress, int mask)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_inotify_add_watch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let path = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "inotify_add_watch", errno)),
    };
    let mask = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as u32;
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "inotify_add_watch", errno)),
    };
    // SAFETY: live inotify descriptor, NUL-terminated path owned by this frame.
    let wd = unsafe { libc::inotify_add_watch(file.as_raw_fd(), path.as_ptr(), mask) };
    if wd < 0 {
        return Err(unix_last_errno(ctx, "inotify_add_watch"));
    }
    Ok(Some(Value::Int(wd)))
}

/// `LinuxWatchService.inotifyRmWatch(int fd, int wd)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_inotify_rm_watch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let wd = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    let file = match unix_fd_file(ctx, fd) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "inotify_rm_watch", errno)),
    };
    // SAFETY: live inotify descriptor; `wd` is validated by the kernel.
    if unsafe { libc::inotify_rm_watch(file.as_raw_fd(), wd) } < 0 {
        return Err(unix_last_errno(ctx, "inotify_rm_watch"));
    }
    Ok(None)
}

/// `LinuxWatchService.configureBlocking(int fd, boolean blocking)` -- toggles
/// `O_NONBLOCK` exactly as the JDK's C does (no exception on failure; the JDK
/// ignores `fcntl`'s answer too).
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_configure_blocking(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
    if let Ok(file) = unix_fd_file(ctx, fd) {
        let raw = file.as_raw_fd();
        // SAFETY: plain flag get/set on a live descriptor.
        unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFL);
            if flags >= 0 {
                if !blocking && flags & libc::O_NONBLOCK == 0 {
                    libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
                } else if blocking && flags & libc::O_NONBLOCK != 0 {
                    libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                }
            }
        }
    }
    Ok(None)
}

/// `LinuxWatchService.socketpair(int[] sv)` -- `socketpair(AF_UNIX,
/// SOCK_STREAM)`; both ends parked in the `FdTable`, their ids stored in `sv`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_socketpair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sv = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("socketpair: sv is null".into()),
            }
            .into())
        }
    };
    let mut raw = [0i32; 2];
    // SAFETY: `raw` is the two-element out array `socketpair` fills.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, raw.as_mut_ptr()) } < 0 {
        return Err(unix_last_errno(ctx, "socketpair"));
    }
    let a = unix_insert_raw_fd(ctx, raw[0]) as i32;
    let b = unix_insert_raw_fd(ctx, raw[1]) as i32;
    ctx.set_array_element(sv, 0, Value::Int(a));
    ctx.set_array_element(sv, 1, Value::Int(b));
    Ok(None)
}

/// `LinuxWatchService.poll(int fd1, int fd2)` -- `poll(2)` both for `POLLIN`
/// with no timeout, answering the ready count; `EINTR` answers 0 exactly as the
/// JDK's C does (this VM's cross-thread root scan signals every thread, and the
/// real `Poller.run` loop simply polls again).
///
/// The wait is unbounded, so it runs inside a blocking region: the poller
/// thread is GC-safe while parked here (its arguments are ints, nothing to
/// re-sync).
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_ws_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::unix::io::AsRawFd;
    let fd1 = args.first().and_then(|v| v.as_int()).unwrap_or(-1);
    let fd2 = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    let f1 = match unix_fd_file(ctx, fd1) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "poll", errno)),
    };
    let f2 = match unix_fd_file(ctx, fd2) {
        Ok(f) => f,
        Err(errno) => return Err(unix_exception(ctx, "poll", errno)),
    };
    let mut fds = [
        libc::pollfd {
            fd: f1.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: f2.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    // SAFETY: `fds` is a two-element `pollfd` array over live descriptors.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
    let errno = if n < 0 {
        unix_errno_of(&std::io::Error::last_os_error())
    } else {
        0
    };
    ctx.end_blocking_region();
    if n < 0 {
        if errno == libc::EINTR {
            return Ok(Some(Value::Int(0)));
        }
        return Err(unix_exception(ctx, "poll", errno));
    }
    Ok(Some(Value::Int(n)))
}

/// `LinuxNativeDispatcher.setmntent0(long pathAddress, long modeAddress)` --
/// the `FILE*` as a `long`; the Java side only hands it back to
/// `getmntent0`/`getlinelen`/`rewind`/`endmntent`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_setmntent0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let path = match unix_c_path_arg(&*ctx, args, 0) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "setmntent", errno)),
    };
    let mode = match unix_c_path_arg(&*ctx, args, 1) {
        Ok(p) => p,
        Err(errno) => return Err(unix_exception(ctx, "setmntent", errno)),
    };
    // SAFETY: both NUL-terminated and owned by this frame.
    let fp = unsafe { libc::setmntent(path.as_ptr(), mode.as_ptr()) };
    if fp.is_null() {
        return Err(unix_last_errno(ctx, "setmntent"));
    }
    Ok(Some(Value::Long(fp as i64)))
}

/// `LinuxNativeDispatcher.getmntent0(long fp, UnixMountEntry entry, long
/// buffer, int bufLen)` -- 0 with `name`/`dir`/`fstype`/`opts` filled, or -1 at
/// the end. The JDK passes a native scratch buffer for `getmntent_r`; its
/// contents are never read back by Java, so a frame-owned buffer of the same
/// length stands in for it.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_getmntent0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fp = args.first().and_then(|v| v.as_long()).unwrap_or(0) as *mut libc::FILE;
    let entry = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("getmntent0: entry is null".into()),
            }
            .into())
        }
    };
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(1) as usize;
    if fp.is_null() {
        return Ok(Some(Value::Int(-1)));
    }
    let mut buf = vec![0u8; len];
    // SAFETY: an all-zero `mntent` is a valid out value; `fp` is the stream
    // `setmntent0` answered and Java has not yet passed to `endmntent`.
    let mut ent: libc::mntent = unsafe { std::mem::zeroed() };
    // SAFETY: `fp` is live; `ent` and the `len`-byte buffer are writable.
    let m = unsafe { libc::getmntent_r(fp, &mut ent, buf.as_mut_ptr().cast(), len as i32) };
    if m.is_null() {
        return Ok(Some(Value::Int(-1)));
    }
    // SAFETY: on success every string field points into `buf`, NUL-terminated.
    let fields = unsafe {
        [
            (
                "name",
                std::ffi::CStr::from_ptr(ent.mnt_fsname).to_bytes().to_vec(),
            ),
            (
                "dir",
                std::ffi::CStr::from_ptr(ent.mnt_dir).to_bytes().to_vec(),
            ),
            (
                "fstype",
                std::ffi::CStr::from_ptr(ent.mnt_type).to_bytes().to_vec(),
            ),
            (
                "opts",
                std::ffi::CStr::from_ptr(ent.mnt_opts).to_bytes().to_vec(),
            ),
        ]
    };
    let entry_pin = ctx.pin_native_root(entry);
    for (name, bytes) in fields {
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        ctx.write_byte_array_from(arr, 0, &bytes);
        let entry = ctx.read_native_pin(entry_pin, entry);
        ctx.set_field_by_name(entry, name, Value::Object(Some(arr)));
    }
    ctx.unpin_native_roots(entry_pin);
    Ok(Some(Value::Int(0)))
}

/// `LinuxNativeDispatcher.endmntent(long fp)`.
#[cfg(target_os = "linux")]
pub(crate) fn native_linux_endmntent(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fp = args.first().and_then(|v| v.as_long()).unwrap_or(0) as *mut libc::FILE;
    if !fp.is_null() {
        // SAFETY: the stream `setmntent0` answered; Java ends it exactly once.
        unsafe { libc::endmntent(fp) };
    }
    Ok(None)
}

/// `UnixNativeDispatcher.getlinelen(long fp)` -- the length of the next line
/// (`getline`), or -1 at end of file, exactly as the JDK's C: `feof` is
/// checked BEFORE the error, so the read that hits EOF answers -1.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_getlinelen(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fp = args.first().and_then(|v| v.as_long()).unwrap_or(0) as *mut libc::FILE;
    if fp.is_null() {
        return Err(unix_exception(ctx, "getline", libc::EBADF));
    }
    let mut line: *mut libc::c_char = std::ptr::null_mut();
    let mut size: libc::size_t = 0;
    // SAFETY: `getline` allocates `line` (freed below); `fp` is a live stream.
    let res = unsafe { libc::getline(&mut line, &mut size, fp) };
    let saved = unix_errno_of(&std::io::Error::last_os_error());
    // SAFETY: `line` came from getline and `fp` is still a live stream.
    unsafe {
        if !line.is_null() {
            libc::free(line.cast());
        }
        if libc::feof(fp) != 0 {
            return Ok(Some(Value::Int(-1)));
        }
    }
    if res == -1 {
        return Err(unix_exception(ctx, "getline", saved));
    }
    if res > i32::MAX as isize {
        return Err(unix_exception(ctx, "getline", libc::EOVERFLOW));
    }
    Ok(Some(Value::Int(res as i32)))
}

/// `UnixNativeDispatcher.rewind(long fp)` -- throws only when the stream's
/// error indicator is set afterwards, with the `errno` `rewind` left.
#[cfg(target_os = "linux")]
pub(crate) fn native_unix_rewind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fp = args.first().and_then(|v| v.as_long()).unwrap_or(0) as *mut libc::FILE;
    if fp.is_null() {
        return Err(unix_exception(ctx, "rewind", libc::EBADF));
    }
    // SAFETY: a live stream `setmntent0` answered.
    unsafe {
        *libc::__errno_location() = 0;
        libc::rewind(fp);
    }
    let saved = unix_errno_of(&std::io::Error::last_os_error());
    // SAFETY: `fp` remains the live stream just rewound above.
    if unsafe { libc::ferror(fp) } != 0 {
        return Err(unix_exception(ctx, "rewind", saved));
    }
    Ok(None)
}

pub(crate) fn native_fc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let bb = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let fd_id = match cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this) {
        Value::Int(v) if v >= 0 => v as u32,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let view = bb_storage_view(ctx, bb)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    if remaining == 0 {
        return Ok(Some(Value::Int(0)));
    }

    // Read bytes from fd into temp buffer.
    // I/O-error-fix: previously a failed `read_bytes` was `unwrap_or(0)`-ed
    // into a 0-byte result, which we then reported as EOF (-1) — silently
    // masking a genuine I/O failure as a clean end-of-stream. Propagate the
    // error as an IOException (matching `native_br_read`) so callers see the
    // real failure instead of phantom EOF.
    let mut buf = vec![0u8; remaining];
    let n = ctx.fd_table().read_bytes(fd_id, &mut buf).map_err(io_err)?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }

    for (i, &b) in buf.iter().enumerate().take(n) {
        bb_write_byte(ctx, view, pos as usize + i, b)?;
    }
    buf_set_position(ctx, bb, pos + n as i32);
    // Update file position
    let fc_pos = match cratonvm_native_api::synthetic_file_channel::position_value(ctx, this) {
        Value::Long(v) => v,
        _ => 0,
    };
    cratonvm_native_api::synthetic_file_channel::set_position_value(
        ctx,
        this,
        Value::Long(fc_pos + n as i64),
    );
    Ok(Some(Value::Int(n as i32)))
}

pub(crate) fn native_fc_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bb = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let fd_id = match cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this) {
        Value::Int(v) if v >= 0 => v as u32,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_storage_view(ctx, bb)?;
    let pos = view.pos;
    let lim = view.lim;
    let remaining = (lim - pos) as usize;
    if remaining == 0 {
        return Ok(Some(Value::Int(0)));
    }

    let mut buf = vec![0u8; remaining];
    bb_read_bytes(ctx, view, pos as usize, &mut buf)?;
    // I/O-error-fix: previously the write result was `unwrap_or(())`-ed and
    // we unconditionally claimed `n == buf.len()` bytes written, advancing
    // the buffer position and file position even when the underlying write
    // failed — silently losing data and corrupting the reported position.
    // Propagate the failure as an IOException; only on success do we advance
    // the buffer/file position by the bytes actually written.
    ctx.fd_table().write_bytes(fd_id, &buf).map_err(io_err)?;
    let n = buf.len();
    buf_set_position(ctx, bb, pos + n as i32);
    let fc_pos = match cratonvm_native_api::synthetic_file_channel::position_value(ctx, this) {
        Value::Long(v) => v,
        _ => 0,
    };
    cratonvm_native_api::synthetic_file_channel::set_position_value(
        ctx,
        this,
        Value::Long(fc_pos + n as i64),
    );
    Ok(Some(Value::Int(n as i32)))
}

pub(crate) fn native_fc_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let pos = match cratonvm_native_api::synthetic_file_channel::position_value(ctx, this) {
        Value::Long(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Long(pos)))
}

pub(crate) fn native_fc_set_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_pos = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    cratonvm_native_api::synthetic_file_channel::set_position_value(
        ctx,
        this,
        Value::Long(new_pos),
    );
    Ok(Some(Value::Object(Some(this))))
}

/// `FileChannel.size()` — the file's length, from the fd.
///
/// **This used to resolve the fd and then discard it** (`let _ = fd_id;`) and
/// return a flat `Ok(Some(Value::Long(0)))`, with the comment "Simplified:
/// return 0 (a full impl would query the underlying file)". That is the
/// constant-where-the-JDK-reads-state shape, and it is the quiet kind: real
/// `FileChannelImpl.size()` fstats, callers size their reads and their `map()`
/// regions from the answer, and a `0` makes every one of them see an EMPTY
/// file with no exception at any point.
///
/// The instrument it needed already existed one crate away:
/// `FileDescriptorTable::file_size` (`native-api/src/fd_table.rs`), whose own
/// doc comment names `FileChannel.size()` as its caller and which handles all
/// three file entry kinds (`FileRead`, `FileWrite` — flushing first so
/// buffered-but-unwritten bytes count — and `FileReadWrite`), saving and
/// restoring the cursor in each. So the repair is to call it.
///
/// **Reachability, stated rather than assumed.** `java/nio/channels/FileChannel
/// .size()J` is registered TWICE: here, and by
/// `native-builtins::phases_late::nio_file::register_phase57_nio_file` as
/// `p57_fc_size`, which already fstats. `register()` is last-write-wins, and
/// VERIFIED in `vm/src/vm/vm_init.rs`:
///
/// * real-JDK arms — `register_io_natives` at L2252 / L2834, then
///   `register_phase57_nio_file` at L2339 / L2904. **p57 wins**, so the flat
///   `0` was DEAD in the shipping modes.
/// * synthetic-jdk arm — `register_builtins` at L1934 (which reaches phase 57
///   through `register_synthetic_overrides`), then `register_io_natives` at
///   L1935. **This body wins**, so the flat `0` was live there.
///
/// One live mode is still a mode, and "correct in the other arm" is exactly
/// the shape `[2cfgs]` keeps costing. Both bodies now answer from the same
/// `file_size`, so the outcome no longer depends on which registrar ran last.
///
/// The error mapping matches `p57_fc_size`'s: an `IOException` carrying the
/// underlying error's text. The two `Long(0)` early returns are kept — a
/// missing receiver or an unset `fd` slot is a synthetic FileChannel that was
/// never opened, which has no file to measure, and inventing an `IOException`
/// for it would be a different answer from the one this VM gives today.
pub(crate) fn native_fc_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let fd_id = match cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this) {
        Value::Int(v) if v >= 0 => v as u32,
        _ => return Ok(Some(Value::Long(0))),
    };
    let size = ctx
        .fd_table()
        .file_size(fd_id)
        .map_err(|e| RuntimeError::IOException {
            message: format!("FileChannel.size: {e}"),
        })?;
    Ok(Some(Value::Long(size as i64)))
}

pub(crate) fn native_fc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);

    // `close()` is declared `final` in `AbstractInterruptibleChannel`, a
    // GRANDPARENT of any real `FileChannelImpl`
    // (`FileChannelImpl extends FileChannel extends
    // AbstractInterruptibleChannel`) -- `FileChannel` itself has no
    // `close()` bytecode of its own, the method is pure inheritance. This
    // native is registered on `java/nio/channels/FileChannel` only to
    // service the fully-synthetic 2-field object `native_fc_open`
    // constructs as a DIRECT instance of that literal abstract class (the
    // only way to reach it is the 2-arg `FileChannel.open(Path,
    // OpenOption[])` static overload's simplified fallback). But this
    // registration also sits in the vtable walk the interpreter performs to
    // resolve an INHERITED method for any real subclass -- so a genuine
    // `FileChannelImpl` (built by `native_fcimpl_open` in real-JDK mode,
    // e.g. every H2 MVStore file) reaching `close()` through a
    // `FileChannel`-typed call site (H2's own `fileChannel.close()`,
    // `RandomAccessFile.close()`'s `fc.close()`, ...) lands here too,
    // instead of the real `AbstractInterruptibleChannel.close()` bytecode.
    // That skipped `implCloseChannel()` entirely: the fileLockTable release
    // loop never ran, `closed` never flipped to `true` (so `isOpen()` kept
    // reporting the channel open forever), and the registered `closer`
    // Cleaner action never ran synchronously -- the underlying fd and its
    // JVM-level FileLockTable bookkeeping were only ever cleaned up later,
    // asynchronously, whenever the background Cleaner thread happened to
    // run -- a real resource-lifecycle correctness gap in its own right
    // (independent of any specific caller), and a contributing factor to
    // `docs/known-issues/h2/!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md`'s
    // `OverlappingFileLockException` investigation (that doc's residual
    // occurrences trace to a separate, H2-level chunk-reclaim race --
    // see the doc for the full picture).
    //
    // Detect a real instance (anything other than the literal synthetic
    // `java/nio/channels/FileChannel` class `native_fc_open` allocates) and
    // replicate `AbstractInterruptibleChannel.close()`'s exact contract --
    // idempotent on `closed`, then invoke the real `implCloseChannel()`
    // bytecode, which is never itself intercepted by a native -- instead of
    // the synthetic single-fd close below.
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
    if class_name.as_deref() != Some("java/nio/channels/FileChannel") {
        if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
            return Ok(None);
        }
        ctx.set_field_by_name(this, "closed", Value::Int(1));
        ctx.invoke_virtual(this, "implCloseChannel", "()V", &[])?;
        return Ok(None);
    }
    let this = ctx.read_native_pin(this_pin, this);

    let fd_id = match cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this) {
        Value::Int(v) if v >= 0 => v as u32,
        _ => {
            // Still flip `closed`: an already-fd-less synthetic channel is
            // closed, and `isOpen()` reads that field on both receiver kinds.
            ctx.set_field_by_name(this, "closed", Value::Int(1));
            return Ok(None);
        }
    };
    let _ = ctx.fd_table().close(fd_id);
    cratonvm_native_api::synthetic_file_channel::set_fd_value(ctx, this, Value::Int(-1));
    // The real `AbstractInterruptibleChannel.closed`, which is what every
    // `isOpen()` — ours and the JDK's — answers the negation of. It could not be
    // used while the file position aliased it; W7-72-ssc-socket-and-filechannel.md.
    ctx.set_field_by_name(this, "closed", Value::Int(1));
    Ok(None)
}

// ===========================================================================
// StringReader — GC-stable side table (see SR_STATE below).
// StringWriter — 2-field synthetic (field 0 = char[] buffer, field 1 = Int count)
// ===========================================================================

pub(crate) const SW_FIELD_BUF: usize = 0;
pub(crate) const SW_FIELD_COUNT: usize = 1;

// The `java/io/StringReader` natives below (registered unconditionally, not
// gated on `synthetic-jdk`) used to store content/position/length in object
// fields 0/1/2, matching the SYNTHETIC stub's 3 generic `_f0..2` slots. But
// this native also wins dispatch against a REAL, bytecode-loaded
// `java.io.StringReader` (`CRATONVM_REAL` is off by default — see
// `RealSelector` in `env_cache.rs` — so a `SyntheticStub` native always
// pre-empts real bytecode unless explicitly opted out of). Real JDK 25's
// `StringReader` was rewritten to hold a single `private final Reader r`
// delegate (`javap` confirms — no `str`/`next`/`length` fields survive), so
// writing "field 1"/"field 2" landed on whatever slot the real class
// actually declares there and silently discarded the `Value::Int` position
// write (read back as `Value::Object(None)`, the zero value for a
// reference-typed slot) — `read()` always saw `pos == 0` and returned the
// same first character forever. Track state in a GC-stable side table
// instead, exactly like `ISR_PENDING` above for `InputStreamReader`.
//
// gc-common w15-c (`common-w14e-reader-and-watch-side-tables-keyed-by-bare-identity-hash`):
// the key is `(vm_identity, identity_hash_code)` ([`io_side_key`]) and each row
// names its reader (`owner`). The bare hash let VM B's `StringReader.<init>`
// REPLACE VM A's live row, after which A read B's characters; and a reader
// dropped without `close()` (a `StringReader` handed to `Properties.load` or
// a parser) kept its whole input for the life of the process.
// `gc_sweep_io_side_tables` now drops a dead reader's row, content and all,
// and follows a moved one; a live reader's row is never judged dead, so the
// "missing row means closed" rule below is unchanged.
//
// gc-common w29-c: keyed by the full [`IoRowKey`] (bucket plus the reader's
// current address). A second live reader with the same identity hash used to
// REPLACE the first one's row at `<init>`, after which the first read the
// second's characters, and `close()` on either closed both.
pub(crate) static SR_STATE: OnceLock<Mutex<HashMap<IoRowKey, SrState>>> = OnceLock::new();

pub(crate) struct SrState {
    pub(crate) units: Vec<u16>,
    pub(crate) pos: usize,
    /// Position stashed by `mark(int)`. Real `StringReader.reset()` rewinds to
    /// the LAST MARK (0 only when `mark` was never called), so this has to be
    /// tracked here or `markSupported() == true` is a lie — see
    /// `native_sr_mark`.
    pub(crate) mark: usize,
    /// The reader's address (see [`IoWeakRow`]).
    pub(crate) owner: usize,
}

impl IoWeakRow for SrState {
    fn owner(&self) -> usize {
        self.owner
    }
    fn set_owner(&mut self, owner: usize) {
        self.owner = owner;
    }
}

pub(crate) fn sr_state() -> &'static Mutex<HashMap<IoRowKey, SrState>> {
    SR_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What `ensureOpen()` throws in every `java.io` reader: the exact
/// `IOException("Stream closed")` the JDK raises when a `read`/`ready`/`skip`/
/// `reset` arrives after `close()`. Shared by the `StringReader` and
/// `CharArrayReader` natives, which each detect "closed" from their own
/// dropped state (a missing `SR_STATE` entry / a null `buf` slot).
pub(crate) fn ioe_stream_closed() -> MethodCallFailed {
    RuntimeError::IOException {
        message: "Stream closed".to_string(),
    }
    .into()
}

// The synthetic StringWriter layout stores the content `char[]` at slot 0 and
// the logical length at slot 1. The REAL `java.io.StringWriter` field layout is
// `Writer.lock` (slot 0, `Ljava/lang/Object;`) + `StringWriter.buf` (slot 1,
// `Ljava/io/StringBuffer;`) — BOTH reference-typed. CratonVM's descriptor-aware
// `ctx.set_field` (vm_exec) coerces any value to the declared field type, and
// `coerce_field_value_by_descriptor` maps an `Int`/`Long` written to an `L`/`[`
// slot to `Object(None)` (heap.rs:1323). So a naive `set_field(this, 1,
// Int(count))` is silently dropped to null — every write appears to land but the
// count reads back as 0, and `toString()` returns "" (the JBoss DMR
// `ModelNode.toString` → empty `WFLYCTL0013` symptom). To survive the coercion
// we keep the count inside a 1-element `int[]` holder (a heap object, so it
// matches the slot-1 reference descriptor and is traced/relocated by the GC).
pub(crate) fn sw_count(ctx: &mut dyn NativeContext, this: ObjectRef) -> usize {
    match ctx.get_field(this, SW_FIELD_COUNT) {
        Value::Object(Some(holder)) => match ctx.get_array_element(holder, 0) {
            Value::Int(c) => c.max(0) as usize,
            _ => 0,
        },
        // Legacy/uninitialized: an `Int` here would have been coerced to null on
        // write, so a surviving `Int` only appears if the descriptor path was
        // bypassed; honor it for robustness.
        Value::Int(c) => c.max(0) as usize,
        _ => 0,
    }
}

pub(crate) fn sw_set_count(ctx: &mut dyn NativeContext, this: ObjectRef, count: usize) {
    if let Value::Object(Some(holder)) = ctx.get_field(this, SW_FIELD_COUNT) {
        ctx.set_array_element(holder, 0, Value::Int(count as i32));
        return;
    }
    // GC: the holder array is allocated first and stored into `this` after.
    let pin = ctx.pin_native_root(this);
    let holder = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    let this = ctx.read_native_pin(pin, this);
    ctx.set_array_element(holder, 0, Value::Int(count as i32));
    ctx.set_field(this, SW_FIELD_COUNT, Value::Object(Some(holder)));
    ctx.unpin_native_roots(pin);
}

// JDK-ONLY-CLASSIFY: stub — `java.io.StringReader` and `java.io.StringWriter`
// are pure Java: neither declares a single ACC_NATIVE method in JDK 25, and 22
// of these 25 registrations shadow concrete bytecode. The `Bridge` set on the
// next line is therefore wrong on the merits for the StringWriter half; the
// StringReader half below already opts back down to `SyntheticStub` explicitly.
// Both halves are string-buffer manipulation with no OS boundary anywhere. The
// two categories inside one function make this a good split candidate: the
// stub-tagged inner block should stay, the surrounding `Bridge` should not.
pub(crate) fn register_string_rw_natives(registry: &mut NativeMethodRegistry) {
    // reads "stub" for java.io.StringReader/StringWriter. Only the TAG disagreed.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`): registrations 1,
    // invocations 0, 0 ACC_NATIVE targets.
    //
    // Note what this REMOVES: some of these registrations sit on
    // `java/lang/AutoCloseable` and `java/io/Closeable` — interfaces, so the
    // shim was deciding dispatch for every USER class implementing them, not
    // just for the JDK's. Refusing it under `--real-jdk` narrows that blast
    // radius rather than widening it.
        // RDR-MIGRATION 2026-06-01: these StringReader natives track state in the
    // GC-stable `SR_STATE` side table (not object fields — see the comment
    // above `SR_STATE`), so they work against either the synthetic stub or a
    // real, bytecode-loaded `java.io.StringReader`. Registered as
    // SyntheticStub for census purposes; note that (unlike the comment below
    // once assumed) real JDK bytecode does NOT win by default — the
    // `CRATONVM_REAL` differential switch must explicitly opt a class in for
    // that (see `RealSelector` in `env_cache.rs`).
    {
        let sr = "java/io/StringReader";
        registry.register(sr, "<init>", "(Ljava/lang/String;)V", native_sr_init, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "read", "()I", native_sr_read, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "read", "([CII)I", native_sr_read_chars, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "ready", "()Z", native_sr_ready, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "close", "()V", native_sr_close, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "skip", "(J)J", native_sr_skip, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sr, "reset", "()V", native_sr_reset, cratonvm_native_api::NativeKind::SyntheticStub);
        // `mark(I)V` was missing entirely, which made the `markSupported()`
        // below a lie and left `reset()` rewinding to 0 instead of the mark.
        registry.register(sr, "mark", "(I)V", native_sr_mark, cratonvm_native_api::NativeKind::SyntheticStub);
        // KEEP: real `StringReader.markSupported()` is `return true;` (it is
        // backed by an in-memory String), so `true` is the correct answer,
        // not a placeholder — and `mark`/`reset` are now both implemented
        // against `SR_STATE` (see `native_sr_mark`, which stores the mark and
        // rejects a negative read-ahead limit, and `native_sr_reset`, which
        // rewinds to it). Re-verified wave 4, 2026-07-28.
        registry.register(sr, "markSupported", "()Z", |_ctx, _args| {
            Ok(Some(Value::Int(1)))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    };

    // HIB-CV-25b sibling: the synthetic StringWriter natives below model the
    // writer as a `char[] buf` (slot 0) + `int count` (slot 1). The REAL JDK
    // layout is `Writer.lock` (slot 0, Object) + `StringWriter.buf`
    // (slot 1, StringBuffer) — so the synthetic state squats two real,
    // reference-typed fields: a subclass reading the inherited `this.lock` saw
    // a `char[]`/`int[]` instead of the JDK's `lock == buf` StringBuffer, and a
    // moving GC could fail to trace a `char[]` parked in the (primitive-context)
    // mislabelled slot. Real StringWriter bytecode is self-contained (it just
    // delegates to a real `StringBuffer`, which works on CratonVM), so — exactly
    // like the StringReader migration above — keep the synthetic natives under
    // `synthetic-jdk` only and let the real bytecode run by default. This also
    // retires the base-class `Writer.write(I)V` → `native_sw_write_int`
    // registration (below), which applied the StringWriter layout to EVERY
    // Writer subclass (same base-class hazard the Reader.read migration fixed).
    #[cfg(feature = "synthetic-jdk")]
    {
        let sw = "java/io/StringWriter";
        registry.register(sw, "<init>", "()V", native_sw_init, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sw, "<init>", "(I)V", native_sw_init_cap, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sw, "write", "(I)V", native_sw_write_int, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sw, "write", "(Ljava/lang/String;)V", native_sw_write_string, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sw, "write", "([CII)V", native_sw_write_chars, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            sw,
            "write",
            "(Ljava/lang/String;II)V",
            native_sw_write_string_off, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(sw, "toString", "()Ljava/lang/String;", native_sw_to_string, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            sw,
            "getBuffer",
            "()Ljava/lang/StringBuffer;",
            native_sw_get_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // KEEP (real JDK body is empty) — audited wave 4, 2026-07-28.
        // `java.io.StringWriter.flush()` and `close()` in java.base are
        // literally `public void flush() {}` / `public void close() throws
        // IOException {}`: the writer's sink is an in-memory StringBuffer, so
        // there is nothing to push and nothing to release, and the class
        // documents that "Closing a StringWriter has no effect" — a closed
        // StringWriter must keep accepting writes. A no-op here is the
        // specified behaviour, not an unimplemented one; making either throw
        // or drop the buffer would be a spec violation.
        registry.register(sw, "flush", "()V", native_noop_void, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(sw, "close", "()V", native_noop_void, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            sw,
            "append",
            "(C)Ljava/io/StringWriter;",
            native_sw_append_char, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            sw,
            "append",
            "(Ljava/lang/CharSequence;)Ljava/io/StringWriter;",
            native_sw_append_cs, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // RDR-MIGRATION 2026-06-01: the blanket `java/io/Reader.read()I` native
    // (backed by `native_sr_read`, which assumes the synthetic StringReader
    // 3-field layout) was registered on the base class and so applied to EVERY
    // Reader subclass — corrupting real FileReader/CharArrayReader/etc. The
    // real `Reader.read()I` and `Reader.read(CharBuffer)` are concrete bytecode
    // that delegate to the subclass's `read([CII)I`, so they run correctly
    // without a native. Keep the synthetic base-Reader natives under
    // `synthetic-jdk` only. `Reader.close()` stays registered universally, but
    // is no longer a no-op — see `native_reader_close` for why it is reachable
    // (synthetic stub subclasses) and what it now does. The older comment here
    // claimed "the real default is a no-op anyway"; that is wrong,
    // `java.io.Reader.close()` is ABSTRACT.
    registry.register("java/io/Reader", "close", "()V", native_reader_close, cratonvm_native_api::NativeKind::SyntheticStub);
    // The cfg above is not itself the guard, and this is the base class,
    // which maximizes the blast radius: a `--features synthetic-jdk` binary
    // run against a real JDK still registers `native_sr_read` on
    // `java/io/Reader.read()I`, and native dispatch keys on the resolved
    // method's DECLARING class — so it intercepts `read()` for every real
    // Reader subclass that inherits the concrete base-class body instead of
    // overriding it (FileReader, CharArrayReader, BufferedReader, ...), not
    // just StringReader. `native_sr_read` looks up `this` in a side table
    // (`sr_state()`) that only `StringReader.<init>` populates, so on any
    // other real subclass it hits `None` and throws "Stream closed" instead
    // of running the real, correct, concrete `Reader.read()I` bytecode.
    // See synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        registry.register("java/io/Reader", "read", "()I", native_sr_read, cratonvm_native_api::NativeKind::SyntheticStub);
        // RA.3: Reader.read(java.nio.CharBuffer) default fills the buffer via
        // char[] + read([CII)I, then advances the buffer's position.
        registry.register(
            "java/io/Reader",
            "read",
            "(Ljava/nio/CharBuffer;)I",
            native_reader_read_charbuffer, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    // `Writer.write(I)V` default is concrete JDK bytecode (`writeBuffer[0]=(char)c;
    // write(writeBuffer,0,1)`) that delegates to the subclass `write([CII)`, so it
    // runs correctly without a native. The synthetic `native_sw_write_int` here
    // assumed the StringWriter `char[]`+`count` layout and so corrupted slot 0/1 of
    // every OTHER Writer subclass — keep it under `synthetic-jdk` only (same
    // base-class hazard the `Reader.read` migration above fixed).
    //
    // `Writer.flush()V` / `Writer.close()V` used to be no-ops in this same
    // block. They are gone: both are ABSTRACT in the real JDK (so unreachable
    // in real-JDK mode — a concrete Writer must declare them, and the override
    // lookup stops at the receiver's own method), and in synthetic mode the
    // hierarchy walk can never get past an intermediate. Only `BufferedWriter`,
    // `OutputStreamWriter`, `PrintWriter`, `StringWriter` and `FileWriter` have
    // a stub superclass chain reaching `java/io/Writer` at all (`jdk_superclass`
    // in classloading/src/class_manager.rs; `CharArrayWriter`/`PipedWriter`/
    // `FilterWriter` extend `java/lang/Object` there), and every one of those
    // registers its own `close`/`flush` — `FileWriter` inherits them from
    // `OutputStreamWriter` one hop below `Writer`. So the two entries were dead
    // in both modes. `write(I)V` stays: it is CONCRETE bytecode in the real JDK
    // and is a genuine synthetic stand-in.
    //
    // Same base-class hazard as `Reader.read()I` above, and the cfg alone is
    // not the guard: a `--features synthetic-jdk` binary run against a real
    // JDK still registers `native_sw_write_int` on the base class, which
    // reads/writes `SW_FIELD_BUF` — StringWriter's own `buf` slot — on
    // whichever real Writer subclass inherits this concrete method (any
    // subclass other than StringWriter has a different field at that slot),
    // corrupting it via `set_array_element` on a non-array field or worse.
    // See synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        registry.register("java/io/Writer", "write", "(I)V", native_sw_write_int, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    }

/// RA.3 — `java.io.Reader.read(Ljava/nio/CharBuffer;)I`.
///
/// Mirrors the real-JDK default: read into a temporary `char[]` via
/// `this.read(char[], int, int)`, then copy into the buffer via
/// `CharBuffer.put(char[], int, int)`. Both calls go through
/// `invoke_virtual`, so this works on any Reader subclass (ISR,
/// BufferedReader, StringReader, ...) and for any CharBuffer
/// implementation (heap-backed, direct, read-only, ...), bypassing the
/// `Buffer.checkIndex` AIOOBE path (RA.1) and any missing NIO Buffer
/// intrinsics.
///
/// Steps:
///   1. `remaining = target.limit() - target.position()` via `invoke_virtual`.
///   2. `char[] chars = new char[min(remaining, 4096)]`.
///   3. `int n = this.read(chars, 0, chars.length)`.
///   4. If `n > 0`, `target.put(chars, 0, n)` via `invoke_virtual`.
///   5. Return `n` (or `-1` at EOF).
pub(crate) fn native_reader_read_charbuffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // target == null — spec says NullPointerException.
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "Reader.read(CharBuffer): target is null".to_string(),
            }));
        }
    };

    // Every virtual dispatch below may move the reader, target and temporary
    // char array. Keep the values that survive a dispatch rooted and reload
    // them before their next use.
    let this_pin = ctx.pin_native_root(this);
    let target_pin = ctx.pin_native_root(target);
    // remaining = target.limit() - target.position()
    let limit = match ctx.invoke_virtual(target, "limit", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        Ok(_) => 0,
        Err(error) => {
            ctx.unpin_native_roots(this_pin);
            return Err(error);
        }
    };
    let target = ctx.read_native_pin(target_pin, target);
    let position = match ctx.invoke_virtual(target, "position", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        Ok(_) => 0,
        Err(error) => {
            ctx.unpin_native_roots(this_pin);
            return Err(error);
        }
    };
    let target = ctx.read_native_pin(target_pin, target);
    let remaining = (limit - position).max(0) as usize;
    if remaining == 0 {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }

    // char[] chars = new char[min(remaining, 4096)]
    let chunk = remaining.min(4096);
    let chars = ctx.new_array(ArrayElementType::Char, chunk);
    let chars_pin = ctx.pin_native_root(chars);

    // int n = this.read(chars, 0, chars.length)
    let this = ctx.read_native_pin(this_pin, this);
    let chars = ctx.read_native_pin(chars_pin, chars);
    let read_result = match ctx.invoke_virtual(
        this,
        "read",
        "([CII)I",
        &[
            Value::Object(Some(chars)),
            Value::Int(0),
            Value::Int(chunk as i32),
        ],
    ) {
        Ok(result) => result,
        Err(error) => {
            ctx.unpin_native_roots(this_pin);
            return Err(error);
        }
    };
    let target = ctx.read_native_pin(target_pin, target);
    let chars = ctx.read_native_pin(chars_pin, chars);
    let n = match read_result {
        Some(Value::Int(v)) => v,
        _ => -1,
    };

    if n > 0 {
        // target.put(chars, 0, n)
        let put_result = ctx.invoke_virtual(
            target,
            "put",
            "([CII)Ljava/nio/CharBuffer;",
            &[Value::Object(Some(chars)), Value::Int(0), Value::Int(n)],
        );
        ctx.unpin_native_roots(this_pin);
        put_result?;
    } else {
        ctx.unpin_native_roots(this_pin);
    }

    Ok(Some(Value::Int(n)))
}

pub(crate) fn native_noop_void(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub(crate) fn native_sr_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let units: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx
            .read_string(*s)
            .map(|s| s.encode_utf16().collect())
            .unwrap_or_default(),
        // `new StringReader(null)` is a `NullPointerException` on the JDK
        // (`Reader.of(s)` is `Objects.requireNonNull(s)`), thrown before any
        // state exists. This used to install an empty, readable reader.
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    // gc-common w15-c: per-VM key; the row names `this` (current: nothing
    // above can move it) as its weak owner, and (w29-c) is filed under it.
    let key = io_side_row_key(ctx, this);
    sr_state().lock().insert(
        key,
        SrState {
            units,
            pos: 0,
            mark: 0,
            owner: io_side_owner(this),
        },
    );
    Ok(None)
}

/// The row of the reader in `key`'s bucket at `owner` (its CURRENT address),
/// or the JDK's `IOException("Stream closed")` when `close()` removed it.
pub(crate) fn sr_row<'a>(
    table: &'a mut HashMap<IoRowKey, SrState>,
    key: IoSideKey,
    owner: usize,
) -> Result<&'a mut SrState, MethodCallFailed> {
    table
        .get_mut(&io_row_key(key, owner))
        .ok_or_else(ioe_stream_closed)
}

pub(crate) fn native_sr_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let key = io_side_key(ctx, this);
    let mut table = sr_state().lock();
    let state = sr_row(&mut table, key, io_side_owner(this))?;
    if state.pos >= state.units.len() {
        return Ok(Some(Value::Int(-1)));
    }
    let ch = state.units[state.pos];
    state.pos += 1;
    Ok(Some(Value::Int(ch as i32)))
}

pub(crate) fn native_sr_read_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let key = io_side_key(ctx, this);
    let owner = io_side_owner(this);
    // The JDK's order (gc-common w15-c): `ensureOpen()`, then
    // `Objects.checkFromIndexSize(off, len, cbuf.length)` (a null `cbuf` is its
    // `NullPointerException`), then `len == 0` answers 0, then EOF answers -1.
    // This used to answer -1 for a null `cbuf`, -1 for `len == 0` at EOF, and
    // to cast a negative `off`/`len` to `usize` and write through it: an
    // out-of-range write where the JDK throws `IndexOutOfBoundsException`.
    let open = sr_state().lock().contains_key(&io_row_key(key, owner));
    if !open {
        return Err(ioe_stream_closed());
    }
    let Some(buf) = buf else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    check_array_bounds(off, len, ctx.array_length(buf))?;
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let chars = {
        let mut table = sr_state().lock();
        let state = sr_row(&mut table, key, owner)?;
        if state.pos >= state.units.len() {
            return Ok(Some(Value::Int(-1)));
        }
        let available = state.units.len() - state.pos;
        let to_read = (len as usize).min(available);
        let chars = state.units[state.pos..state.pos + to_read].to_vec();
        state.pos += to_read;
        chars
    };
    // One bulk copy where this used to box one `Value` per char; in bounds by
    // the check above. Nothing here allocates, so `buf` is still current.
    let copied = ctx.write_char_array_from(buf, off as usize, &chars);
    debug_assert!(copied, "the checked range holds the chars");
    Ok(Some(Value::Int(chars.len() as i32)))
}

pub(crate) fn native_sr_ready(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = io_side_row_key(ctx, this);
    let ready = match sr_state().lock().get(&key) {
        Some(s) => s.pos < s.units.len(),
        None => return Err(ioe_stream_closed()),
    };
    Ok(Some(Value::Int(if ready { 1 } else { 0 })))
}

