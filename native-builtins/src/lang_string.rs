// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! String, StringBuilder, and StringBuffer native method implementations.

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::intern_arc;
use cratonvm_types::Value;

use crate::{alloc_concurrent_synthetic, compile_java_regex, native_noop_with_this, obj_arg};

// ---------------------------------------------------------------------------
// Thread-local scratch buffers for per-element char[] reads.
//
// SAFETY/RATIONALE: each thread gets its own `RefCell<Vec<u16>>`. Native
// method handlers are called synchronously on the executing thread, never
// re-entrantly with overlapping borrows on the same scratch buffer (each
// call clears + fills + clones, then releases the borrow before returning).
// We expose helpers (`with_string_chars_scratch`, `with_two_string_chars_scratches`)
// that hand out exclusive borrows; callers must NOT call back into other
// `read_string_chars*` helpers while holding one. The two-buffer helper is
// provided for callers (compareTo / indexOf / starts/ends-with / contains)
// that need both sides simultaneously.
//
// Reusing a `Vec<u16>` across calls eliminates the heap allocation that
// `read_string_chars` previously did per invocation. On hot paths with
// 5+ callers (compareTo, equalsIgnoreCase, indexOf(String), regionMatches,
// startsWith(String)) this turns N allocations per string-pair op into 0
// (steady-state, once the buffer has grown).
thread_local! {
    static STRING_CHARS_SCRATCH_A: std::cell::RefCell<Vec<u16>> =
        std::cell::RefCell::new(Vec::with_capacity(64));
    static STRING_CHARS_SCRATCH_B: std::cell::RefCell<Vec<u16>> =
        std::cell::RefCell::new(Vec::with_capacity(64));
}

/// Decode a String object's backing `value` array into a `Vec<u16>` of
/// UTF-16 code units, handling all three storage layouts:
///   * legacy `char[]` value (one u16 per element);
///   * JDK 9+ compact `byte[]` value, LATIN-1 coder (one byte per char,
///     zero-extended);
///   * JDK 9+ compact `byte[]` value, UTF-16 coder (two big-endian bytes
///     per char).
///
/// This is the single source of truth for "String object -> Vec<u16>".
/// The crucial detail is that for a UTF-16-coded compact string the
/// `value` byte[] has length `2 * char_count`: callers MUST NOT treat the
/// raw array length as the character count, and MUST NOT read individual
/// bytes as characters. Getting this wrong silently corrupts every
/// non-LATIN-1 string (e.g. `CharacterData00`'s packed lookup-table data,
/// which is loaded via `String.toCharArray()` in its `<clinit>`).
fn decode_string_chars(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    dst: &mut Vec<u16>,
) {
    dst.clear();
    let (arr, raw_len) = match string_char_array(ctx, obj) {
        Some(v) => v,
        None => return,
    };
    let elem_type = ctx.heap_element_type_of(arr);
    let is_byte_array = matches!(
        elem_type,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
    );

    if !is_byte_array {
        // Legacy `char[]` value: one u16 per element. Bulk-copy fast path.
        dst.resize(raw_len, 0);
        let written = ctx.read_char_array_into(arr, 0, &mut dst[..]);
        // Defensive: trim to what the VM actually wrote (the default
        // impl can short-circuit on a non-Int slot).
        dst.truncate(written);
        return;
    }

    // Compact `byte[]` value. `coder` lives in field index 1: 1 = UTF-16,
    // 0 = LATIN-1.
    let is_utf16 = matches!(ctx.get_field(obj, 1), Value::Int(1));
    if is_utf16 {
        // Two bytes per char, little-endian (low byte first) — matches
        // StringUTF16.isBigEndian()==false on x86/ARM and the byte order
        // written by create_java_string. `raw_len` is 2 * char_count.
        let char_count = raw_len / 2;
        if dst.capacity() < char_count {
            dst.reserve(char_count - dst.capacity());
        }
        for c in 0..char_count {
            let lo = match ctx.get_array_element(arr, c * 2) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            let hi = match ctx.get_array_element(arr, c * 2 + 1) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            dst.push((hi << 8) | lo);
        }
    } else {
        // LATIN-1: one byte per char, zero-extended.
        if dst.capacity() < raw_len {
            dst.reserve(raw_len - dst.capacity());
        }
        for i in 0..raw_len {
            let ch = match ctx.get_array_element(arr, i) {
                Value::Int(v) => (v & 0xff) as u16,
                _ => 0,
            };
            dst.push(ch);
        }
    }
}

/// Fill the given `Vec<u16>` with the characters of the String object's
/// underlying value (char[] or compact-string byte[]). The vector is
/// cleared first.
fn fill_string_chars(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef, dst: &mut Vec<u16>) {
    decode_string_chars(ctx, obj, dst);
}

/// Run `f` with the thread-local scratch buffer A populated from `obj`.
/// The buffer is reused across calls — no allocation on the hot path
/// once it has grown to the steady-state size.
fn with_string_chars_scratch<R>(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    f: impl FnOnce(&[u16]) -> R,
) -> R {
    STRING_CHARS_SCRATCH_A.with(|cell| {
        let mut buf = cell.borrow_mut();
        fill_string_chars(ctx, obj, &mut buf);
        f(&buf)
    })
}

/// Run `f` with both thread-local scratch buffers populated from `obj_a`
/// and `obj_b` respectively. Used by binary string ops (compareTo,
/// indexOf(String), startsWith(String), endsWith, contains, replace).
fn with_two_string_chars_scratches<R>(
    ctx: &dyn NativeContext,
    obj_a: cratonvm_types::ObjectRef,
    obj_b: cratonvm_types::ObjectRef,
    f: impl FnOnce(&[u16], &[u16]) -> R,
) -> R {
    STRING_CHARS_SCRATCH_A.with(|cell_a| {
        STRING_CHARS_SCRATCH_B.with(|cell_b| {
            let mut buf_a = cell_a.borrow_mut();
            let mut buf_b = cell_b.borrow_mut();
            fill_string_chars(ctx, obj_a, &mut buf_a);
            fill_string_chars(ctx, obj_b, &mut buf_b);
            f(&buf_a, &buf_b)
        })
    })
}

pub(crate) fn register_string_builder_natives(registry: &mut NativeMethodRegistry, class: &str) {
    registry.register(class, "<init>", "()V", native_sb_init_default);
    registry.register(
        class,
        "<init>",
        "(Ljava/lang/String;)V",
        native_sb_init_string,
    );
    registry.register(
        class,
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sb_init_charsequence,
    );
    registry.register(class, "<init>", "(I)V", native_sb_init_capacity);
    registry.register(
        class,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
        native_sb_append_string,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_append_string,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/String;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_string,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuilder;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuffer;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/StringBuffer;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "appendNull",
        "()Ljava/lang/AbstractStringBuilder;",
        native_sb_append_null,
    );
    registry.register(
        class,
        "append",
        "(I)Ljava/lang/StringBuilder;",
        native_sb_append_int,
    );
    registry.register(
        class,
        "append",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_append_int,
    );
    registry.register(
        class,
        "append",
        "(I)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_int,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/StringBuilder;",
        native_sb_append_char,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/StringBuffer;",
        native_sb_append_char,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_char,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/Appendable;",
        native_sb_append_char,
    );
    registry.register(
        class,
        "appendCodePoint",
        "(I)Ljava/lang/StringBuilder;",
        native_sb_append_codepoint,
    );
    registry.register(
        class,
        "appendCodePoint",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_append_codepoint,
    );
    registry.register(
        class,
        "appendCodePoint",
        "(I)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_codepoint,
    );
    // JDK 21+ `repeat(int codePoint, int count)` — intercept so it operates on
    // the synthetic char[] layout instead of running real bytecode that hits
    // `ensureCapacityNewCoder` → `Arrays.copyOf([B)` over a char[] (ArrayStore).
    registry.register(
        class,
        "repeat",
        "(II)Ljava/lang/StringBuilder;",
        native_sb_repeat_codepoint,
    );
    registry.register(
        class,
        "repeat",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_repeat_codepoint,
    );
    registry.register(
        class,
        "repeat",
        "(II)Ljava/lang/AbstractStringBuilder;",
        native_sb_repeat_codepoint,
    );
    registry.register(
        class,
        "repeat",
        "(CI)Ljava/lang/AbstractStringBuilder;",
        native_sb_repeat_codepoint,
    );
    registry.register(
        class,
        "append",
        "([CII)Ljava/lang/StringBuilder;",
        native_sb_append_char_array_off_len,
    );
    registry.register(
        class,
        "append",
        "([CII)Ljava/lang/StringBuffer;",
        native_sb_append_char_array_off_len,
    );
    registry.register(
        class,
        "append",
        "([CII)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_char_array_off_len,
    );
    registry.register(
        class,
        "append",
        "([C)Ljava/lang/StringBuilder;",
        native_sb_append_char_array,
    );
    registry.register(
        class,
        "append",
        "([C)Ljava/lang/StringBuffer;",
        native_sb_append_char_array,
    );
    registry.register(
        class,
        "append",
        "([C)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_char_array,
    );
    registry.register(
        class,
        "append",
        "(Z)Ljava/lang/StringBuilder;",
        native_sb_append_boolean,
    );
    registry.register(
        class,
        "append",
        "(Z)Ljava/lang/StringBuffer;",
        native_sb_append_boolean,
    );
    registry.register(
        class,
        "append",
        "(Z)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_boolean,
    );
    registry.register(
        class,
        "append",
        "(J)Ljava/lang/StringBuilder;",
        native_sb_append_long,
    );
    registry.register(
        class,
        "append",
        "(J)Ljava/lang/StringBuffer;",
        native_sb_append_long,
    );
    registry.register(
        class,
        "append",
        "(J)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_long,
    );
    registry.register(
        class,
        "append",
        "(D)Ljava/lang/StringBuilder;",
        native_sb_append_double,
    );
    registry.register(
        class,
        "append",
        "(D)Ljava/lang/StringBuffer;",
        native_sb_append_double,
    );
    registry.register(
        class,
        "append",
        "(D)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_double,
    );
    registry.register(
        class,
        "append",
        "(F)Ljava/lang/StringBuilder;",
        native_sb_append_float,
    );
    registry.register(
        class,
        "append",
        "(F)Ljava/lang/StringBuffer;",
        native_sb_append_float,
    );
    registry.register(
        class,
        "append",
        "(F)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_float,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
        native_sb_append_object,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
        native_sb_append_object,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_object,
    );
    // C36: intercept the (CharSequence, int, int) variants used by
    // Formatter internals. Real JDK bytecode of this method reads slot
    // 2 (count) from our synthetic StringBuilder layout and blows up
    // with a bogus capacity request.
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/StringBuilder;",
        native_sb_append_charsequence_off_len,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/StringBuffer;",
        native_sb_append_charsequence_off_len,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_charsequence_off_len,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
        native_sb_append_charsequence_off_len,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/StringBuilder;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/StringBuffer;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "toString",
        "()Ljava/lang/String;",
        native_sb_to_string,
    );
    registry.register(class, "length", "()I", native_sb_length);
    registry.register(class, "charAt", "(I)C", native_sb_char_at);
    // codePointAt/codePointBefore/codePointCount/appendCodePoint: MUST be
    // native for the same reason as getValue/getCoder below — unregistered,
    // real JDK `AbstractStringBuilder` bytecode operates on the compact
    // `byte[] value` + `byte coder` layout, which CratonVM's synthetic
    // `char[]`-backed StringBuilder does not have. See the doc comments on
    // `native_sb_code_point_at` / `native_sb_append_code_point`.
    registry.register(class, "codePointAt", "(I)I", native_sb_code_point_at);
    registry.register(
        class,
        "codePointBefore",
        "(I)I",
        native_sb_code_point_before,
    );
    registry.register(class, "codePointCount", "(II)I", native_sb_code_point_count);
    registry.register(
        class,
        "appendCodePoint",
        "(I)Ljava/lang/StringBuilder;",
        native_sb_append_code_point,
    );
    registry.register(
        class,
        "appendCodePoint",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_append_code_point,
    );
    // BUG-TC0622: real-JDK bytecode (String.nonSyncContentEquals, reached via
    // String.contentEquals(CharSequence)) reads the builder's value/coder
    // directly. Our synthetic char[]+count layout has no compact byte[]/coder,
    // so without these natives getCoder() returns `count` and getValue() hands
    // back the char[] mis-typed as a byte[], crashing in StringUTF16.contentEquals.
    registry.register(class, "getValue", "()[B", native_sb_get_value);
    registry.register(class, "getCoder", "()B", native_sb_get_coder);
    registry.register(class, "getChars", "(II[CI)V", native_sb_get_chars);
    registry.register(
        class,
        "reverse",
        "()Ljava/lang/StringBuilder;",
        native_sb_reverse,
    );
    registry.register(
        class,
        "reverse",
        "()Ljava/lang/StringBuffer;",
        native_sb_reverse,
    );

    // --- Mutation methods ---
    registry.register(
        class,
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuilder;",
        native_sb_insert_string,
    );
    registry.register(
        class,
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_insert_string,
    );
    registry.register(
        class,
        "insert",
        "(IC)Ljava/lang/StringBuilder;",
        native_sb_insert_char,
    );
    registry.register(
        class,
        "insert",
        "(IC)Ljava/lang/StringBuffer;",
        native_sb_insert_char,
    );
    registry.register(
        class,
        "insert",
        "(II)Ljava/lang/StringBuilder;",
        native_sb_insert_int,
    );
    registry.register(
        class,
        "insert",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_insert_int,
    );
    registry.register(
        class,
        "insert",
        "(ILjava/lang/Object;)Ljava/lang/StringBuilder;",
        native_sb_insert_object,
    );
    registry.register(
        class,
        "insert",
        "(ILjava/lang/Object;)Ljava/lang/StringBuffer;",
        native_sb_insert_object,
    );
    registry.register(
        class,
        "insert",
        "(I[CII)Ljava/lang/StringBuilder;",
        native_sb_insert_char_array_off_len,
    );
    registry.register(
        class,
        "insert",
        "(I[CII)Ljava/lang/StringBuffer;",
        native_sb_insert_char_array_off_len,
    );
    registry.register(
        class,
        "insert",
        "(I[CII)Ljava/lang/AbstractStringBuilder;",
        native_sb_insert_char_array_off_len,
    );
    registry.register(
        class,
        "insert",
        "(I[C)Ljava/lang/StringBuilder;",
        native_sb_insert_char_array,
    );
    registry.register(
        class,
        "insert",
        "(I[C)Ljava/lang/StringBuffer;",
        native_sb_insert_char_array,
    );
    registry.register(
        class,
        "insert",
        "(I[C)Ljava/lang/AbstractStringBuilder;",
        native_sb_insert_char_array,
    );
    registry.register(
        class,
        "delete",
        "(II)Ljava/lang/StringBuilder;",
        native_sb_delete,
    );
    registry.register(
        class,
        "delete",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_delete,
    );
    registry.register(
        class,
        "deleteCharAt",
        "(I)Ljava/lang/StringBuilder;",
        native_sb_delete_char_at,
    );
    registry.register(
        class,
        "deleteCharAt",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_delete_char_at,
    );
    registry.register(
        class,
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuilder;",
        native_sb_replace,
    );
    registry.register(
        class,
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_replace,
    );
    registry.register(class, "setCharAt", "(IC)V", native_sb_set_char_at);
    registry.register(class, "setLength", "(I)V", native_sb_set_length);
    registry.register(
        class,
        "indexOf",
        "(Ljava/lang/String;)I",
        native_sb_index_of,
    );
    registry.register(
        class,
        "indexOf",
        "(Ljava/lang/String;I)I",
        native_sb_index_of_from,
    );
    registry.register(
        class,
        "lastIndexOf",
        "(Ljava/lang/String;)I",
        native_sb_last_index_of,
    );
    registry.register(
        class,
        "lastIndexOf",
        "(Ljava/lang/String;I)I",
        native_sb_last_index_of_from,
    );
    registry.register(
        class,
        "substring",
        "(I)Ljava/lang/String;",
        native_sb_substring,
    );
    registry.register(
        class,
        "substring",
        "(II)Ljava/lang/String;",
        native_sb_substring_range,
    );
    registry.register(class, "capacity", "()I", native_sb_capacity);
    registry.register(class, "ensureCapacity", "(I)V", native_sb_ensure_cap);
    registry.register(class, "trimToSize", "()V", native_sb_trim_to_size);
    // Java 21: StringBuilder.repeat(CharSequence, int) / repeat(int codePoint, int count)
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/CharSequence;I)Ljava/lang/StringBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cs = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let count = match args.get(2) {
                Some(Value::Int(v)) => (*v).max(0) as usize,
                _ => 0,
            };
            let cs_str = ctx.read_string(cs).unwrap_or_default();
            let cs_chars: Vec<u16> = cs_str.encode_utf16().collect();
            let mut chars = sb_read_chars(ctx, this);
            for _ in 0..count {
                chars.extend_from_slice(&cs_chars);
            }
            let this = sb_write_chars(ctx, this, &chars);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/String;I)Ljava/lang/StringBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let count = match args.get(2) {
                Some(Value::Int(v)) => (*v).max(0) as usize,
                _ => 0,
            };
            let s_str = ctx.read_string(s).unwrap_or_default();
            let s_chars: Vec<u16> = s_str.encode_utf16().collect();
            let mut chars = sb_read_chars(ctx, this);
            for _ in 0..count {
                chars.extend_from_slice(&s_chars);
            }
            let this = sb_write_chars(ctx, this, &chars);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/CharSequence;I)Ljava/lang/StringBuffer;",
        native_sb_repeat_charsequence,
    );
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;",
        native_sb_repeat_charsequence,
    );
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/String;I)Ljava/lang/StringBuffer;",
        native_sb_repeat_charsequence,
    );
    registry.register(
        class,
        "repeat",
        "(Ljava/lang/String;I)Ljava/lang/AbstractStringBuilder;",
        native_sb_repeat_charsequence,
    );
    // Java 21: StringBuilder.repeat(int codePoint, int count). MUST be a native:
    // unregistered, it falls through to the real `AbstractStringBuilder.repeat`
    // → `ensureCapacityNewCoder` → `Arrays.copyOf(value, …)` bytecode, which
    // treats `value` as a compact-string `byte[]`. CratonVM's StringBuilder
    // backing is a `char[]`, so the real bytecode's `System.arraycopy` copies
    // char[]→byte[] and throws `ArrayStoreException: incompatible array element
    // types (src=Char, dest=Byte)`. `java.time.format.DateTimeFormatter` uses
    // `buf.repeat('0', n)` for zero-padding, so this broke every timestamp/
    // temporal literal (35 Hibernate suite classes).
    registry.register(
        class,
        "repeat",
        "(II)Ljava/lang/StringBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let code_point = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let count = match args.get(2) {
                Some(Value::Int(v)) => (*v).max(0) as usize,
                _ => 0,
            };
            // Encode the code point to UTF-16 units (surrogate pair for
            // supplementary planes); fall back to a single unit for an invalid
            // code point (e.g. a lone surrogate) rather than dropping it.
            let mut units: Vec<u16> = Vec::with_capacity(2);
            match char::from_u32(code_point as u32) {
                Some(c) => {
                    let mut buf = [0u16; 2];
                    units.extend_from_slice(c.encode_utf16(&mut buf));
                }
                None => units.push(code_point as u16),
            }
            let mut chars = sb_read_chars(ctx, this);
            for _ in 0..count {
                chars.extend_from_slice(&units);
            }
            let this = sb_write_chars(ctx, this, &chars);
            Ok(Some(Value::Object(Some(this))))
        },
    );
}

// ---------------------------------------------------------------------------
// java.lang.String natives
// ---------------------------------------------------------------------------

/// Helper: get the char[] value array from a String object's field 0.
pub(crate) fn string_char_array(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Option<(cratonvm_types::ObjectRef, usize)> {
    match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => {
            let len = ctx.array_length(arr);
            Some((arr, len))
        }
        _ => None,
    }
}

/// Number of UTF-16 code units (== `String.length()`) for a String object,
/// accounting for the JDK 9+ compact layout: a UTF-16-coded `byte[]` value
/// holds `2 * length` bytes, so the raw array length must be halved.
pub(crate) fn string_char_count(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> usize {
    let (arr, raw_len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return 0,
    };
    let is_byte_array = matches!(
        ctx.heap_element_type_of(arr),
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
    );
    if is_byte_array && matches!(ctx.get_field(this, 1), Value::Int(1)) {
        raw_len / 2
    } else {
        raw_len
    }
}

pub(crate) fn native_string_intern(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.intern on null".to_string()),
            }
            .into())
        }
    };

    // `ctx.create_string` already deduplicates against the VM's
    // `shared.string_pool` (returns the existing `ObjectRef` on a hit), so
    // for already-interned content this is a single hashmap lookup + the
    // unavoidable `read_string` decode. We additionally run the content
    // through the types-level `intern_arc` pool: that pool dedupes the
    // backing UTF-8 bytes globally, so repeated `intern()` calls on
    // different physical String objects with the same content share a
    // single `Arc<str>` allocation in the global pool — eliminating one
    // alloc per cold call after the first. The `Arc<str>` itself is
    // dropped at end-of-scope (intentionally — we only needed its pool
    // side-effect), but its underlying bytes remain interned globally so
    // subsequent calls hit the pool's read path with no allocation.
    let text = ctx.read_string(this).unwrap_or_default();
    let arc = intern_arc(&text);
    let interned = ctx.create_string(&arc);
    Ok(Some(Value::Object(Some(interned))))
}

/// `String(AbstractStringBuilder, Void)` — private/package constructor used by
/// real-JDK `StringBuilder.toString()`.
///
/// CratonVM's StringBuilder/StringBuffer natives keep a synthetic
/// `char[] + count` layout, while JDK 25 `StringBuilder.toString()` assumes
/// `AbstractStringBuilder.value` is a compact-string `byte[]`. Byte Buddy can
/// execute that real bytecode while retransformation is in progress, so bridge
/// the constructor at the String boundary: read the builder through the
/// synthetic helper and write this String's real compact fields directly.
pub(crate) fn native_string_init_abstract_string_builder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;

    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let chars = match args.get(1) {
        Some(Value::Object(Some(builder))) => sb_read_chars(ctx, *builder),
        _ => Vec::new(),
    };
    let latin1 = chars.iter().all(|&u| u <= 0xFF);
    let byte_len = if latin1 { chars.len() } else { chars.len() * 2 };

    // `this` crosses an allocating call, so retain it only through the
    // collector-updated slot. The scope closes automatically on every exit.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let value = scope.new_array(ArrayElementType::Byte, byte_len);
    let this = scope.get(&this_h);

    if latin1 {
        for (i, &u) in chars.iter().enumerate() {
            scope.set_array_element(value, i, Value::Int((u & 0xFF) as i32));
        }
    } else {
        for (i, &u) in chars.iter().enumerate() {
            scope.set_array_element(value, i * 2, Value::Int((u & 0xFF) as i32));
            scope.set_array_element(value, i * 2 + 1, Value::Int(((u >> 8) & 0xFF) as i32));
        }
    }

    scope.set_field(this, 0, Value::Object(Some(value)));
    scope.set_field(this, 1, Value::Int(if latin1 { 0 } else { 1 }));
    scope.set_field(this, 2, Value::Int(0));
    scope.set_field(this, 3, Value::Int(0));
    Ok(None)
}

/// Which field slot holds `String.hash`, from the element type of the string's
/// backing `value` array — or `None` when this receiver cannot decide it.
///
/// JDK 9+ compact layout is `{value:[B, coder:B, hash:I, hashIsZero:Z}`, so the
/// hash is slot **2**; the legacy synthetic-stub layout is `{value:[C, hash:I}`,
/// so it is slot **1**. A `Boolean` element type is the byte-array alias this
/// VM uses in some paths and means the compact layout too.
///
/// **`None` is the case that matters, and it used to be folded into slot 1.**
/// `string_char_array` returns `None` for a `String` whose `value` field is
/// null — an object allocated but not yet initialised. That receiver carries no
/// evidence about the layout, and the caller latches this answer in a
/// process-wide `OnceLock` that is never reset. Guessing 1 there, against the
/// JDK 25 layout, selects `coder`: every later call would read the coder as a
/// cached hash (so every UTF-16 string hashes to `1`) and, on the recompute
/// path, WRITE the computed hash into `coder` — silent corruption of the
/// string's encoding flag, for the whole process, decided by whichever string
/// happened to be hashed first.
///
/// Returning `None` costs nothing: the caller returns 0 for that receiver
/// either way, and the first readable string still latches the right slot.
fn hash_slot_for(element_type: Option<cratonvm_types::ArrayElementType>) -> Option<usize> {
    match element_type {
        Some(cratonvm_types::ArrayElementType::Byte)
        | Some(cratonvm_types::ArrayElementType::Boolean) => Some(2),
        Some(_) => Some(1),
        None => None,
    }
}

pub(crate) fn native_string_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Layout-aware: JDK 25 String has fields {value:[B, coder:B, hash:I,
    // hashIsZero:Z} (hash at slot 2), our legacy synthetic-stub layout has
    // {value:[C, hash:I} (hash at slot 1). The slot is determined by the
    // value-array element type, which is constant for the whole process —
    // so resolve it ONCE (from a real string's value array) and cache it.
    // Then read the cached hash FIRST, before touching the value array, so a
    // cache hit is a single field read like HotSpot. (Resolving the slot per
    // call — either by class+field name or by re-reading the value array —
    // was itself the bottleneck that kept cache hits ~20x slower than a plain
    // field read, dwarfing the hashing win.)
    //
    // The latch is process-wide and permanent, so it must only ever be set from
    // a receiver that actually carries the evidence — see `hash_slot_for`.
    static HASH_SLOT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let hash_field_index: usize = match HASH_SLOT.get() {
        Some(i) => *i,
        None => {
            let elem = string_char_array(ctx, this).map(|(arr, _)| ctx.heap_element_type_of(arr));
            match hash_slot_for(elem) {
                Some(slot) => {
                    let _ = HASH_SLOT.set(slot);
                    slot
                }
                // Unreadable receiver and no layout learned yet: hash 0 and
                // latch NOTHING, leaving the decision to the first string that
                // can answer it. The fall-through below returns 0 for this same
                // receiver anyway, so the only behaviour that changes is that a
                // guess no longer becomes permanent.
                None => return Ok(Some(Value::Int(0))),
            }
        }
    };
    if let Value::Int(cached) = ctx.get_field(this, hash_field_index) {
        if cached != 0 {
            return Ok(Some(Value::Int(cached)));
        }
    }

    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let elem_type = ctx.heap_element_type_of(arr);
    let is_byte_array = matches!(
        elem_type,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
    );

    // For compact strings (byte[] value), inspect the `coder` byte
    // (field 1) to know whether the bytes are LATIN-1 (one byte per char,
    // unsigned-extended) or UTF-16 (LITTLE-endian u16 pairs).
    //
    // This said "big-endian" while the loop below has always read little-endian.
    // The identical stale claim on `vectorizedHashCode` is the one that named
    // the contract that native was not implementing while it hashed bytes for a
    // day, so these are corrected rather than left as harmless prose. The layout
    // is little-endian in all four places that touch it: `create_java_string`
    // writes the low byte at the even index, `decode_string_value` reads it back,
    // `native_string_index_of_static_helper` decodes the same pairs, and
    // `StringUTF16.isBigEndian()` answers false.
    let is_utf16 = is_byte_array && matches!(ctx.get_field(this, 1), Value::Int(1));

    // Strategy: drain the array into a thread-local i32 scratch buffer in
    // ONE tight virtual-dispatch loop (per element, but at least the loop
    // body is trivial), then run the hashing arithmetic in a second
    // non-virtual loop. This separates the trait-dispatch cost from the
    // hashing inner loop so the compiler can vectorize / unroll the latter.
    //
    // For 1000-char strings: down from 1000 interleaved (dispatch + arith)
    // iterations to 1000 dispatch + 1000 plain arithmetic — same dispatch
    // count, but the arithmetic phase becomes inlineable. The bigger win
    // is the cache cache (the dst buffer fits in L1) and that subsequent
    // hash_code calls reuse the same scratch allocation.
    let hash: i32 = STRING_CHARS_SCRATCH_A.with(|cell| {
        // We're not storing u16s here — the existing branches needed i32s
        // to compose UTF-16 BE pairs and to mask. Use the same buffer by
        // re-typing element semantics (we always store the post-decoded
        // i32 char value, ANDed appropriately for the source layout).
        let mut buf = cell.borrow_mut();
        buf.clear();
        let mut h: i32 = 0;
        if is_byte_array && is_utf16 {
            let chars = len / 2;
            let cap = buf.capacity();
            if cap < chars {
                buf.reserve(chars - cap);
            }
            // Phase 1: drain (virtual-dispatched but trivial body).
            // Little-endian: low byte at even index, high byte at odd index.
            for c in 0..chars {
                let lo = match ctx.get_array_element(arr, c * 2) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                let hi = match ctx.get_array_element(arr, c * 2 + 1) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                buf.push(((hi << 8) | lo) as u16);
            }
        } else if is_byte_array {
            let cap = buf.capacity();
            if cap < len {
                buf.reserve(len - cap);
            }
            for i in 0..len {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xff) as u16,
                    _ => 0,
                };
                buf.push(ch);
            }
        } else {
            let cap = buf.capacity();
            if cap < len {
                buf.reserve(len - cap);
            }
            for i in 0..len {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xffff) as u16,
                    _ => 0,
                };
                buf.push(ch);
            }
        }
        // Phase 2: non-virtual hashing loop. The compiler can inline /
        // unroll / autovectorize this since `buf` is a plain slice.
        for &c in buf.iter() {
            h = h.wrapping_mul(31).wrapping_add(c as i32);
        }
        h
    });

    // Cache the hash (but 0 stays 0 — matches JDK behavior).
    if hash != 0 {
        ctx.set_field(this, hash_field_index, Value::Int(hash));
    }

    Ok(Some(Value::Int(hash)))
}

pub(crate) fn native_string_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    // `String.length()` is the code-unit count. For a UTF-16-coded compact
    // string the backing byte[] is 2 bytes/char, so the raw array length
    // must be halved.
    let len = string_char_count(ctx, this) as i32;
    Ok(Some(Value::Int(len)))
}

pub(crate) fn native_string_char_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.charAt on null".to_string()),
            }
            .into())
        }
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    let (arr, raw_len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::sioobe_no_length(index).into())
        }
    };
    let is_byte_array = matches!(
        ctx.heap_element_type_of(arr),
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
    );
    let is_utf16 = is_byte_array && matches!(ctx.get_field(this, 1), Value::Int(1));
    // Code-unit count: UTF-16-coded compact strings store 2 bytes/char.
    let char_count = if is_utf16 { raw_len / 2 } else { raw_len };

    if index < 0 || index >= char_count as i32 {
        // `String.charAt` reaches `Preconditions.checkIndex(index, length,
        // SIOOBE_FORMATTER)` in the real JDK, so it owes the caller that
        // formatter's message as well as its class. This native stands in for
        // the whole chain — including from the interpreter's inline-cache
        // intrinsic table, which is why only the FIRST out-of-range `charAt`
        // at a call site used to carry a message and every later one did not.
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_index(index, char_count as i32).into(),
        );
    }

    let i = index as usize;
    let ch = if is_utf16 {
        // Two bytes per char, little-endian (low byte first).
        let lo = match ctx.get_array_element(arr, i * 2) {
            Value::Int(v) => (v as u8) as i32,
            _ => 0,
        };
        let hi = match ctx.get_array_element(arr, i * 2 + 1) {
            Value::Int(v) => (v as u8) as i32,
            _ => 0,
        };
        Value::Int((hi << 8) | lo)
    } else if is_byte_array {
        // LATIN-1: one byte per char, zero-extended.
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => Value::Int(v & 0xff),
            other => other,
        }
    } else {
        // Legacy char[] value.
        ctx.get_array_element(arr, i)
    };
    Ok(Some(ch))
}

pub(crate) fn native_string_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))), // null != anything
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))), // null argument → false
    };

    // Same object → true
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }

    // Cheap rejection: different backing-array lengths cannot be equal.
    // Avoids the bulk decode for the common "different strings" case.
    let (_, len_a) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let (_, len_b) = match string_char_array(ctx, other) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    if len_a != len_b {
        return Ok(Some(Value::Int(0)));
    }

    // Bulk-read both char arrays into thread-local scratch buffers and
    // compare on the local slices in one shot — the inner loop is a
    // plain `==` on `[u16]` which the compiler can SIMD.
    let equal = with_two_string_chars_scratches(ctx, this, other, |a, b| a == b);
    Ok(Some(Value::Int(if equal { 1 } else { 0 })))
}

pub(crate) fn native_string_index_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Bulk-decode the receiver into the thread-local scratch buffer (one
    // VM-bulk copy when the backing array is `char[]`), then scan the
    // local slice. Avoids N per-element virtual `get_array_element`
    // calls when the haystack is long.
    let needle = (ch & 0xFFFF) as u16;
    let pos = with_string_chars_scratch(ctx, this, |buf| {
        buf.iter()
            .position(|&c| c == needle)
            .map(|i| i as i32)
            .unwrap_or(-1)
    });
    Ok(Some(Value::Int(pos)))
}

/// T2.2.6: `String.indexOf(int ch, int fromIndex)`.
///
/// Searches the string for the first occurrence of the given code-unit
/// (after treating the `ch` argument the same way the JDK does: values
/// outside the BMP are matched via the surrogate pair). Clamps
/// `fromIndex` to `[0, len)`; a value ≥ length returns `-1`.
pub(crate) fn native_string_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let from = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Note: for BMP characters this matches the raw char; for supplementary
    // code points the caller is expected to have already decomposed to
    // surrogates in the underlying `String.value` char array, so a direct
    // code-unit compare is still correct against the leading surrogate.
    let needle = (ch & 0xFFFF) as u16;
    let pos = with_string_chars_scratch(ctx, this, |buf| {
        let start = from.max(0) as usize;
        if start >= buf.len() {
            return -1i32;
        }
        buf[start..]
            .iter()
            .position(|&c| c == needle)
            .map(|i| (start + i) as i32)
            .unwrap_or(-1)
    });
    Ok(Some(Value::Int(pos)))
}

/// T2.2.6: `String.lastIndexOf(int ch, int fromIndex)`.
///
/// Searches backward from `min(fromIndex, len-1)` for the last
/// occurrence of `ch`. A negative `fromIndex` always returns `-1`.
pub(crate) fn native_string_last_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let from = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    if from < 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let needle = (ch & 0xFFFF) as u16;
    let pos = with_string_chars_scratch(ctx, this, |buf| {
        if buf.is_empty() {
            return -1i32;
        }
        let start = (from as usize).min(buf.len() - 1);
        // rposition scans backwards; map onto the original index.
        buf[..=start]
            .iter()
            .rposition(|&c| c == needle)
            .map(|i| i as i32)
            .unwrap_or(-1)
    });
    Ok(Some(Value::Int(pos)))
}

// NOTE: `native_string_code_point_at` (T2.2.7) and
// `native_string_compare_to_ignore_case` (T2.2.8) already exist elsewhere
// in this file (see below at the "// Step …" banners) and are registered
// from `lib.rs`. They are left untouched; the T2 census recorded both
// items as pre-existing.

pub(crate) fn native_string_substring(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.substring on null".to_string()),
            }
            .into())
        }
    };
    let begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Fast path: peek at the value array's length to validate bounds
    // and read only the requested range, instead of materializing the
    // entire String first (the previous implementation always encoded
    // the WHOLE string to UTF-16 even for a 3-char prefix).
    let (arr_opt, char_count, is_byte_array, is_utf16) = match string_char_array(ctx, this) {
        Some((arr, total_len)) => {
            let elem_type = ctx.heap_element_type_of(arr);
            let is_byte_array = matches!(
                elem_type,
                cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
            );
            let (char_count, is_utf16) = if is_byte_array {
                let utf16 = matches!(ctx.get_field(this, 1), Value::Int(1));
                if utf16 {
                    (total_len / 2, true)
                } else {
                    (total_len, false)
                }
            } else {
                (total_len, false)
            };
            (Some(arr), char_count, is_byte_array, is_utf16)
        }
        None => (None, 0usize, false, false),
    };

    if begin < 0 || end < begin || end > char_count as i32 {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(begin, end, char_count as i32).into());
    }

    let b = begin as usize;
    let e = end as usize;
    let sub_len = e - b;
    let mut sub_utf16: Vec<u16> = Vec::with_capacity(sub_len);
    if let Some(arr) = arr_opt {
        if is_byte_array && is_utf16 {
            // Read just the bytes in [b*2 .. e*2).
            // Little-endian: low byte at even index, high byte at odd index.
            for c in b..e {
                let lo = match ctx.get_array_element(arr, c * 2) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                let hi = match ctx.get_array_element(arr, c * 2 + 1) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                sub_utf16.push((hi << 8) | lo);
            }
        } else if is_byte_array {
            // LATIN-1: each byte zero-extended
            for i in b..e {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xff) as u16,
                    _ => 0,
                };
                sub_utf16.push(ch);
            }
        } else {
            // Legacy char[]
            for i in b..e {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xffff) as u16,
                    _ => 0,
                };
                sub_utf16.push(ch);
            }
        }
    }
    let sub_text = String::from_utf16_lossy(&sub_utf16);
    // `_gc_safe`: `sub_text` is already Rust-owned; `this`/`arr` are not
    // dereferenced again below, so a moving young GC here is safe. Without
    // this, String.substring() -- unconditionally forced native, hot path
    // for Response.toAbsolute()-style URI manipulation -- hard-aborts the
    // whole process on young-gen exhaustion instead of collecting and
    // continuing. See docs/known-issues/tomcat-08-07/
    // silent-hang-no-signature-cluster.md.
    let result = ctx.create_string_uninterned_gc_safe(&sub_text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = int value
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = val.to_string();
    let result = ctx.create_string_uninterned(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// Format a double like Java does (no trailing zeros for integers, etc.)
pub(crate) fn format_double(v: f64) -> String {
    // Java's `Double.toString` layout (incl. the 10^-3..10^7 scientific-notation
    // threshold) lives in the shared `cratonvm_types` formatter so every copy
    // stays correct. The old local `format!("{v}")` never used E-notation, so
    // large/small magnitudes (1e7, 1e-4, 1e300, Double.MAX_VALUE) printed in
    // full decimal instead of "1.0E7"/"1.0E-4"/... .
    cratonvm_types::java_double_to_string(v)
}

// ---------------------------------------------------------------------------
// Step 2: StringBuilder / StringBuffer natives
// ---------------------------------------------------------------------------

/// Helper: read field 0 (char[] buffer) and field 1 (int count) from StringBuilder.
pub(crate) fn sb_state(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> (Option<cratonvm_types::ObjectRef>, i32) {
    let buf = match ctx.get_field(this, 0) {
        Value::Object(Some(arr))
            if ctx.object_is_array(arr)
                && ctx.heap_element_type_of(arr) == cratonvm_types::ArrayElementType::Char =>
        {
            Some(arr)
        }
        _ => None,
    };
    let count = if ctx.object_num_fields(this) >= 3 {
        match ctx.get_field(this, 2) {
            Value::Int(v) => v,
            _ => 0,
        }
    } else {
        match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        }
    };
    (buf, count)
}

/// Helper: write the StringBuilder count in the layout-appropriate slot.
///
/// CratonVM's own synthetic StringBuilder/StringBuffer layout is 2 slots:
/// `value: char[]` @0, `count: int` @1 (see `instance_fields(2)` in
/// `classloading/src/class_manager.rs`) — this is the layout every object
/// actually gets allocated with unless real `AbstractStringBuilder`
/// bytecode itself constructs one (e.g. during Byte Buddy
/// retransformation), which uses the real JDK 9+ 3-slot layout `value:
/// byte[]` @0, `coder: byte` @1, `count: int` @2 instead.
///
/// A prior version of this helper unconditionally wrote slot 1 = 0 (as a
/// LATIN1 `coder` placeholder) and mirrored `count` into slot 2, on the
/// assumption every StringBuilder has the 3-slot real layout. Every
/// StringBuilder actually allocated through CratonVM's own 2-slot
/// synthetic path (i.e. essentially all of them) instead had its *real*
/// count slot (slot 1) stomped to 0 on every append/insert/setLength call,
/// and the slot-2 mirror silently dropped by the `gen_heap` OOB-write
/// guard (num_slots=2 < index 2) — so `StringBuilder.length()` always
/// read back 0 immediately after the write that was supposed to grow it.
/// Java code with a growth loop keyed on `sb.length()` (e.g.
/// `while (sb.length() < n) sb.append(c);`, seen during real
/// `java.desktop`/`java.beans` clinit) never observed the length increase
/// and spun forever, hammering the OOB guard on every iteration.
///
/// `object_num_fields` reports the object's *actual* allocated slot count
/// (matching the `class_num_total_fields` idiom used elsewhere to avoid
/// hard-coding a layout — see the `Timestamp` nanos-field fix), so branch
/// on it instead of assuming either shape.
/// Helper: write the StringBuilder count in the layout-appropriate slot.
///
/// CratonVM's own synthetic StringBuilder/StringBuffer layout is 2 slots:
/// `value: char[]` @0, `count: int` @1 (see `instance_fields(2)` in
/// `classloading/src/class_manager.rs`) — this is the layout every object
/// actually gets allocated with unless real `AbstractStringBuilder`
/// bytecode itself constructs one (e.g. during Byte Buddy
/// retransformation), which uses the real JDK 9+ 3-slot layout `value:
/// byte[]` @0, `coder: byte` @1, `count: int` @2 instead.
///
/// A prior version of this helper unconditionally wrote slot 1 = 0 (as a
/// LATIN1 `coder` placeholder) and mirrored `count` into slot 2, on the
/// assumption every StringBuilder has the 3-slot real layout. Every
/// StringBuilder actually allocated through CratonVM's own 2-slot
/// synthetic path (i.e. essentially all of them) instead had its *real*
/// count slot (slot 1) stomped to 0 on every append/insert/setLength call,
/// and the slot-2 mirror silently dropped by the `gen_heap` OOB-write
/// guard (num_slots=2 < index 2) — so `StringBuilder.length()` always
/// read back 0 immediately after the write that was supposed to grow it.
/// Java code with a growth loop keyed on `sb.length()` (e.g.
/// `while (sb.length() < n) sb.append(c);`, seen during real
/// `java.desktop`/`java.beans` clinit) never observed the length increase
/// and spun forever, hammering the OOB guard on every iteration.
///
/// `object_num_fields` reports the object's *actual* allocated slot count
/// (matching the `class_num_total_fields` idiom used elsewhere to avoid
/// hard-coding a layout — see the `Timestamp` nanos-field fix), so branch
/// on it instead of assuming either shape.
///
/// # The by-name mirror (2026-07-26)
///
/// The 3-slot branch's `@2` is the JDK 9 layout `value/coder/count`. JDK 25's
/// `AbstractStringBuilder` declares FOUR instance fields —
/// `value @0, coder @1, maybeLatin1 @2, count @3` — so on a real JDK 25 image
/// slot 2 is `maybeLatin1` and the field genuinely named `count` was never
/// written at all.
///
/// That stayed invisible while CratonVM's own natives were the only readers:
/// they agree with each other on whichever slot this helper picked. It becomes
/// visible the moment real `AbstractStringBuilder` bytecode runs against one of
/// these objects — which is exactly what happens once Mockito's inline mock
/// maker redefines `StringBuilder`/`AbstractStringBuilder` in place. From then
/// on `length()` cedes to the woven advice (deliberately, so a MOCK's advice
/// can run), and the advice's "not mocked" fallthrough is the original
/// `getfield count:I`: it read the real, never-written `count` and returned
/// **0** for a genuinely real builder. That is the "KNOWN REMAINING GAP" the
/// 2026-07-23 MockitoBean session documented and left open.
///
/// It is not academic: `org.springframework.cglib.core.TypeUtils.map` does
/// `type.substring(0, type.length() - sb.length() * 2)`, so a zero
/// `sb.length()` left the trailing `[]` unstripped and
/// `MethodInterceptorGenerator`'s `static final GET_DECLARED_METHODS` signature
/// came out as the malformed `()[Ljava/lang/reflect/Method[];`. That field
/// initialises once per class, so ONE Mockito mock anywhere in the process
/// poisoned every cglib proxy generated afterwards, each dying in
/// `CGLIB$STATICHOOK1` with `NoSuchMethodError: java.lang.Class
/// .getDeclaredMethods` — what `AotIntegrationTests
/// #endToEndTestsForBeanOverrides` aborted on.
///
/// So mirror the count into the field actually NAMED `count` as well. The
/// index-based writes stay exactly as they were (every native in this file
/// reads them back, and the unit-test `NativeContext` mock has no class model
/// to resolve names against), and the extra by-name write is a no-op when no
/// such field exists.
fn sb_set_count(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef, count: i32) {
    if ctx.object_num_fields(this) >= 3 {
        // Real JDK 9+ layout: value@0, coder@1, count@2.
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(count));
        // …plus wherever THIS JDK actually puts `count` (JDK 25: slot 3).
        ctx.set_field_by_name(this, "count", Value::Int(count));
    } else {
        // CratonVM synthetic layout: value(char[])@0, count@1.
        ctx.set_field(this, 1, Value::Int(count));
    }
}

/// Helper: ensure the StringBuilder has capacity for `additional` more chars.
/// Returns the char[] buffer (possibly newly allocated and copied).
/// Returns `(updated_this, buf)`.  The first element is the post-GC ObjectRef
/// for `this`; callers MUST use it for all subsequent writes to the StringBuilder
/// because `ctx.new_array` can trigger a moving GC that relocates `this`.
pub(crate) fn sb_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    additional: usize,
) -> (cratonvm_types::ObjectRef, cratonvm_types::ObjectRef) {
    use cratonvm_types::ArrayElementType;

    let (buf, count) = sb_state(ctx, this);
    let old_cap = buf.map_or(0, |b| ctx.array_length(b));
    let count = (count.max(0) as usize).min(old_cap);

    if count + additional <= old_cap {
        return (this, buf.unwrap());
    }

    // Grow: max(old_cap * 2 + 2, count + additional)
    let new_cap = std::cmp::max(
        old_cap.saturating_mul(2).saturating_add(2),
        count + additional,
    );

    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let new_buf = scope.new_array(ArrayElementType::Char, new_cap);
    let this = scope.get(&this_handle);

    // Re-read old_buf via the GC-updated `this` (GC also updates object fields).
    // audit-round5 fix #6 (HIGH): use the `bulk_array_copy` intrinsic
    // (single `copy_nonoverlapping` in the VM override) instead of a
    // per-element `get_array_element` / `set_array_element` loop. This
    // collapses 2N virtual trait dispatches into one bulk call on the
    // StringBuilder grow path.
    if let Value::Object(Some(old_buf)) = scope.get_field(this, 0) {
        if scope.object_is_array(old_buf)
            && scope.heap_element_type_of(old_buf) == cratonvm_types::ArrayElementType::Char
        {
            let _ = scope.bulk_array_copy(old_buf, 0, new_buf, 0, count);
        }
    }

    scope.set_field(this, 0, Value::Object(Some(new_buf)));
    (this, new_buf)
}

/// Helper: append a slice of u16 chars to a StringBuilder.
/// Returns the CURRENT (pin-refreshed) `this`: the grow path allocates, and
/// callers that return `this` to Java (every `append` overload — chained
/// `.append(...)` dispatches on that return value) must hand back the
/// post-move address, not their raw pre-call copy (cceres5, live-captured at
/// `JndiName.getAbsoluteName`'s chained appends).
pub(crate) fn sb_append_chars(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    chars: &[u16],
) -> cratonvm_types::ObjectRef {
    // Most appends do not grow. Reuse this first state read instead of
    // entering `sb_ensure_capacity` and then reading the buffer/count again.
    let (current_buf, current_count) = sb_state(ctx, this);
    let current_cap = current_buf.map_or(0, |buf| ctx.array_length(buf));
    let current_count = (current_count.max(0) as usize).min(current_cap);
    let (this, buf, count) = if let Some(buf) =
        current_buf.filter(|_| current_count.saturating_add(chars.len()) <= current_cap)
    {
        (this, buf, current_count)
    } else {
        let (this, buf) = sb_ensure_capacity(ctx, this, chars.len());
        let (_, count) = sb_state(ctx, this);
        (this, buf, count.max(0) as usize)
    };

    // Char arrays are compact u16 payloads in the VM. The bulk override is a
    // single checked copy; retain the element loop only for mock contexts or
    // unusual heaps that decline the fast path.
    if !ctx.write_char_array_from(buf, count, chars) {
        for (i, &ch) in chars.iter().enumerate() {
            ctx.set_array_element(buf, count + i, Value::Int(ch as i32));
        }
    }
    sb_set_count(ctx, this, (count + chars.len()) as i32);
    this
}

/// Helper: append a Rust string to a StringBuilder. Returns the CURRENT
/// `this` (see `sb_append_chars`).
pub(crate) fn sb_append_str(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    text: &str,
) -> cratonvm_types::ObjectRef {
    let chars: Vec<u16> = text.encode_utf16().collect();
    sb_append_chars(ctx, this, &chars)
}

pub(crate) fn native_sb_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let buf = scope.new_array(ArrayElementType::Char, 16);
    let this = scope.get(&this_h);
    scope.set_field(this, 0, Value::Object(Some(buf)));
    sb_set_count(&mut *scope, this, 0);
    Ok(None)
}

pub(crate) fn native_sb_init_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let text = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let chars: Vec<u16> = text.encode_utf16().collect();
    let cap = chars.len() + 16;
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let buf = scope.new_array(ArrayElementType::Char, cap);
    let this = scope.get(&this_h);
    for (i, &ch) in chars.iter().enumerate() {
        scope.set_array_element(buf, i, Value::Int(ch as i32));
    }
    scope.set_field(this, 0, Value::Object(Some(buf)));
    sb_set_count(&mut *scope, this, chars.len() as i32);
    Ok(None)
}

/// `StringBuilder(CharSequence)` / `StringBuffer(CharSequence)`.
///
/// Without this native the real JDK `StringBuilder(CharSequence)` ctor runs
/// bytecode that delegates to `AbstractStringBuilder.<init>(CharSequence)`,
/// which populates the *real* JDK field layout (`value`/`coder`/`count`).
/// CratonVM's StringBuilder uses a synthetic layout (char[] at slot 0, count
/// at slot 1), so the real ctor leaves the object inconsistent: subsequent
/// synthetic `append`/`toString` natives misread the slots, prepending
/// `seq.length()` NUL chars (picocli's `Help.Ansi.Text` copy-ctor —
/// `new StringBuilder(other.plain)` — was the visible symptom: blank
/// `--help` output). Intercept it so the synthetic layout stays consistent,
/// mirroring `native_sb_init_string` but coercing any CharSequence to text.
pub(crate) fn native_sb_init_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Real JDK `AbstractStringBuilder(CharSequence)` calls `seq.length()`, so a
    // null sequence throws NPE; coerce any non-null CharSequence to its text.
    // `this` crosses TWO allocating calls here —
    // `invoke_to_string` (arbitrary Java `toString()`, can allocate/GC at
    // will) and `new_array`; the handle slot remains current across both.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let text = match args.get(1) {
        Some(Value::Object(Some(o))) => invoke_to_string(&mut *scope, *o).unwrap_or_default(),
        _ => String::new(),
    };
    let chars: Vec<u16> = text.encode_utf16().collect();
    let cap = chars.len() + 16;
    let buf = scope.new_array(ArrayElementType::Char, cap);
    let this = scope.get(&this_h);
    for (i, &ch) in chars.iter().enumerate() {
        scope.set_array_element(buf, i, Value::Int(ch as i32));
    }
    scope.set_field(this, 0, Value::Object(Some(buf)));
    sb_set_count(&mut *scope, this, chars.len() as i32);
    Ok(None)
}

pub(crate) fn native_sb_init_capacity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(v)) => std::cmp::max(*v, 0) as usize,
        _ => 16,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    // HotSpot throws a catchable OutOfMemoryError for an over-large value array
    // (e.g. `new StringBuilder(Integer.MAX_VALUE)`); mirror that instead of the
    // panicking allocator, which would abort the whole VM.
    let buf = match scope.try_new_array(ArrayElementType::Char, cap) {
        Some(b) => b,
        None => {
            return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: "Requested array size exceeds VM limit".to_string(),
            }
            .into());
        }
    };
    let this = scope.get(&this_h);
    scope.set_field(this, 0, Value::Object(Some(buf)));
    sb_set_count(&mut *scope, this, 0);
    Ok(None)
}

pub(crate) fn native_sb_append_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let text = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_else(|| "null".to_string()),
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    let this = sb_append_str(ctx, this, &text);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let this = sb_append_str(ctx, this, &val.to_string());
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v as u16,
        _ => 0,
    };
    let this = sb_append_chars(ctx, this, &[ch]);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_null(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let this = sb_append_str(ctx, this, "null");
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_codepoint(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let code_point = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let repeat_args = [
        Value::Object(Some(this)),
        Value::Int(code_point),
        Value::Int(1),
    ];
    native_sb_repeat_codepoint(ctx, &repeat_args)
}

/// `StringBuilder.repeat(int codePoint, int count)` / `StringBuffer.repeat(...)`
/// (JDK 21+). Without this native the real `AbstractStringBuilder.repeat`
/// bytecode runs against our synthetic `char[]` layout: it reaches
/// `ensureCapacityNewCoder`, which on a capacity grow calls
/// `Arrays.copyOf([B,I)` over the (actually `char[]`) `value` field →
/// `ArrayStoreException: arraycopy: incompatible array element types
/// (src=Char, dest=Byte)`. `java.time.format.DateTimeFormatter` uses
/// `repeat` for zero-padding, so this surfaced across many date/time paths.
pub(crate) fn native_sb_repeat_codepoint(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let code_point = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if count < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("count is negative: {count}"),
            }
            .into(),
        );
    }
    if count == 0 {
        return Ok(Some(Value::Object(Some(this))));
    }
    // Expand the code point to UTF-16 units, matching `AbstractStringBuilder`:
    //   * 0x0000..=0xFFFF  -> one unit (Java treats it as `repeat((char)cp, n)`,
    //                        so lone surrogates are appended verbatim, not rejected);
    //   * 0x10000..=0x10FFFF -> surrogate pair;
    //   * anything else (incl. negative) -> IllegalArgumentException.
    let cp = code_point as u32;
    let unit: Vec<u16> = if cp <= 0xFFFF {
        vec![cp as u16]
    } else if cp <= 0x10_FFFF {
        let v = cp - 0x1_0000;
        vec![0xD800 + (v >> 10) as u16, 0xDC00 + (v & 0x3FF) as u16]
    } else {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("Not a valid Unicode code point: 0x{cp:X}"),
            }
            .into(),
        );
    };
    let total = unit.len().saturating_mul(count as usize);
    let mut chars: Vec<u16> = Vec::with_capacity(total);
    for _ in 0..count {
        chars.extend_from_slice(&unit);
    }
    let this = sb_append_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_repeat_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cs = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let count = match args.get(2) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    if count == 0 {
        return Ok(Some(Value::Object(Some(this))));
    }
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let text = invoke_to_string(&mut *scope, cs).unwrap_or_default();
    let this = scope.get(&this_handle);
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut chars = sb_read_chars(&*scope, this);
    for _ in 0..count {
        chars.extend_from_slice(&units);
    }
    let this = sb_write_chars(&mut *scope, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// `StringBuilder.append(char[], int, int)` — real-JDK bytecode would write
/// to `value`/`count` fields, but our synthetic layout uses index 0/1.
/// Register a native so the synthetic layout stays consistent.  Without
/// this, `BufferedReader.readLine()` (which appends into a fresh
/// `StringBuilder` via this overload) silently produces empty strings.
pub(crate) fn native_sb_append_char_array_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    let end = off.saturating_add(len).min(arr_len);
    let start = off.min(arr_len);
    let copy_len = end.saturating_sub(start);
    // audit-round5 fix #6 (HIGH): both sides are Java `char[]` — go
    // through `bulk_array_copy` (single `copy_nonoverlapping` in the VM
    // override) instead of materialising a per-element Rust `Vec<u16>`.
    let (this, buf) = sb_ensure_capacity(ctx, this, copy_len);
    let (_, count) = sb_state(ctx, this);
    let count = count as usize;
    let _ = ctx.bulk_array_copy(arr, start, buf, count, copy_len);
    sb_set_count(ctx, this, (count + copy_len) as i32);
    Ok(Some(Value::Object(Some(this))))
}

/// `StringBuilder.append(char[])` — full-array variant.
pub(crate) fn native_sb_append_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let n = ctx.array_length(arr);
    // audit-round5 fix #6 (HIGH): bulk-copy directly into the SB buffer
    // (see `native_sb_append_char_array_off_len` for rationale).
    let (this, buf) = sb_ensure_capacity(ctx, this, n);
    let (_, count) = sb_state(ctx, this);
    let count = count as usize;
    let _ = ctx.bulk_array_copy(arr, 0, buf, count, n);
    sb_set_count(ctx, this, (count + n) as i32);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let this = sb_append_str(ctx, this, if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // WP4.5 — invokevirtual with a long arg type-erases the value to
    // Value::Double via `CompactValue::to_value()`. Until the interpreter
    // does descriptor-aware popping, accept the bit-reinterpreted Double
    // and convert back to long bits.
    let val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    let this = sb_append_str(ctx, this, &val.to_string());
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        Some(Value::Long(l)) => f64::from_bits(*l as u64),
        Some(Value::Float(f)) => *f as f64,
        Some(Value::Int(i)) => *i as f64,
        _ => 0.0,
    };
    let this = sb_append_str(ctx, this, &format_double(val));
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let this = sb_append_str(ctx, this, &format_float(val));
    Ok(Some(Value::Object(Some(this))))
}

/// Call `toString()` on an arbitrary object via virtual dispatch.
///
/// If the object is a Java String, reads it directly. Otherwise invokes
/// `toString()` which will find overridden versions in user-defined classes.
///
/// A `toString()` override that legitimately returns Java `null` (legal —
/// see `bug54241b` below) is coerced to the text `"null"` here, matching
/// what every caller of this function needs: `StringBuilder.append(Object)`,
/// string concatenation, etc. all ultimately go through
/// `AbstractStringBuilder.append(String)`, which substitutes the text
/// `"null"` for a null argument. `String.valueOf(Object)` is the ONE
/// exception — its JDK contract for a non-null `obj` is exactly
/// `obj.toString()`, preserving nullness rather than substituting text — so
/// it must use [`invoke_to_string_opt`] directly instead of this wrapper.
pub(crate) fn invoke_to_string(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    Ok(invoke_to_string_opt(ctx, obj)?.unwrap_or_else(|| "null".to_string()))
}

/// Like [`invoke_to_string`], but returns `Ok(None)` when `toString()`
/// legitimately returns a Java `null` reference, instead of coercing it to
/// the text `"null"`. Needed by `String.valueOf(Object)` — see its call site.
fn invoke_to_string_opt(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Option<String>, cratonvm_types::error::MethodCallFailed> {
    // Fast path: if it's already a String object, just read it
    if let Some(s) = ctx.read_string(obj) {
        return Ok(Some(s));
    }

    // Fast path for wrapper types: if the object has exactly 1 field and its
    // value is a primitive, format it directly. This handles Integer, Long,
    // Double, Float, Boolean, Character, Byte, Short — all wrapper types store
    // their primitive in field 0.
    //
    // MUST exclude arrays: a heap array's num_slots is its LENGTH, so a
    // length-1 array masquerades as a wrapper here — and get_field(0) on a
    // packed primitive array reads a full Value slot over 4/8-byte elements,
    // yielding garbage (long[]{1} rendered as "0"/"6" instead of "[J@hash").
    let nf = ctx.object_num_fields(obj);
    if nf == 1 && ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        // This fast path MUST be gated on the class actually being a
        // `java.lang.*` boxed primitive. An arbitrary class that happens to
        // have a single primitive field has the same heap shape — e.g.
        // `java.util.Collections$EmptyList`, whose only instance field is the
        // inherited `AbstractList.modCount` (int 0). Without the gate it was
        // rendered as that raw field value ("0") instead of dispatching its
        // real `toString()` ("[]"), so `String.valueOf(emptyList)` /
        // `"" + emptyList` / `sb.append(emptyList)` all produced "0". Mirrors
        // the same gate already present in
        // `native-collections::obj_to_display_string`.
        let class_id = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(class_id).unwrap_or_default();
        let is_wrapper = matches!(
            name.as_str(),
            "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Boolean"
                | "java/lang/Character"
                | "java/lang/Float"
                | "java/lang/Double"
        );
        if is_wrapper {
            match ctx.get_field(obj, 0) {
                Value::Int(v) => {
                    let formatted = if name == "java/lang/Boolean" {
                        if v != 0 { "true" } else { "false" }.to_string()
                    } else if name == "java/lang/Character" {
                        char::from_u32(v as u32).unwrap_or('?').to_string()
                    } else if name == "java/lang/Byte" {
                        (v as i8).to_string()
                    } else if name == "java/lang/Short" {
                        (v as i16).to_string()
                    } else {
                        // Integer
                        v.to_string()
                    };
                    return Ok(Some(formatted));
                }
                Value::Long(v) => return Ok(Some(v.to_string())),
                // Use the Java-spec formatters (NOT raw `{}`), so a boxed Double/Float
                // rendered via String.valueOf(Object) / StringBuilder.append(Object) /
                // object string-concat matches `Double.toString` — incl. the
                // 10^-3..10^7 scientific-notation threshold, "Infinity", and "-0.0".
                // Raw `format!("{}")` dropped the ".0", printed "inf"/"-0", and never
                // used E-notation (e.g. boxed 1e7 -> "10000000.0", -0.0 -> "-0").
                Value::Float(v) => return Ok(Some(format_float(v))),
                Value::Double(v) => return Ok(Some(format_double(v))),
                _ => {} // Not a primitive wrapper
            }
        }
    }

    // Call obj.toString() via virtual dispatch. A Java exception from the
    // override is observable and must reach the caller; only an absent or
    // malformed return value uses the historical identity fallback.
    let result = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
    match result {
        Ok(Some(Value::Object(Some(str_ref)))) => Ok(Some(
            ctx.read_string(str_ref)
                .unwrap_or_else(|| "null".to_string()),
        )),
        // toString() legitimately returned null (e.g. TestJspWriterImpl's
        // bug54241b: an anonymous class whose toString() explicitly `return
        // null;`) — this is NOT a dispatch failure, don't fall through to the
        // ClassName@hash fallback below.
        Ok(Some(Value::Object(None))) => Ok(None),
        Ok(_) => {
            // Honest fallback name: arrays render their JVMS array-class name
            // like HotSpot ([Ljava.lang.Class; / [I), not "Object".
            let name = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                crate::lang_class::array_descriptor_for(ctx, obj).replace('/', ".")
            } else {
                "Object".to_string()
            };
            Ok(Some(format!("{}@{:x}", name, ctx.identity_hash_code(obj))))
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn native_sb_append_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Producer-#12 fix (stw-residual-close 20260723): `invoke_to_string`
    // re-enters Java (obj.toString() — e.g. Class.toString() in infinispan's
    // ClassToExternalizerMap.toString append chain); a nested moving young
    // collection relocates `this`, and the raw copy would append into
    // from-space memory `Arena::reset` has already zeroed AND be returned
    // stale, poisoning every later link of the javac chain (the WildFly
    // [stale-recv] StringBuilder family). Pin + re-read, exactly like the
    // in-tree exemplar in the insert-CharSequence native.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => invoke_to_string(&mut *scope, *obj)?,
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    let this = scope.get(&this_handle);
    let this = sb_append_str(&mut *scope, this, &text);
    Ok(Some(Value::Object(Some(this))))
}

/// C36: `AbstractStringBuilder.append(CharSequence)` — same JDK-layout
/// mismatch as the 3-arg variant below; intercept to append the whole
/// sequence natively.
pub(crate) fn native_sb_append_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Producer-#12 fix: same re-entrant `invoke_to_string` hazard as
    // `native_sb_append_object` just above — pin + re-read `this`.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => match invoke_to_string(&mut *scope, *obj) {
            Ok(s) => s,
            Err(_) => String::new(),
        },
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    let this = scope.get(&this_handle);
    let this = sb_append_str(&mut *scope, this, &text);
    Ok(Some(Value::Object(Some(this))))
}

/// C36: `AbstractStringBuilder.append(CharSequence, int, int)` — real JDK
/// bytecode for this reads our synthetic StringBuilder's slot 2 as `count`
/// (expecting the JDK layout byte[]/byte/int at slots 0/1/2). Our layout is
/// char[] at 0, int count at 1, so the JDK read of slot 2 returns an
/// uninitialised int and its `ensureCapacitySameCoder` tries to allocate a
/// bogus multi-megabyte buffer → OutOfMemoryError deep inside
/// `java.util.Formatter.format`. Intercept the call natively so JDK
/// bytecode never touches our incompatible field layout.
pub(crate) fn native_sb_append_charsequence_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cs = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let start = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let end = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // null CharSequence → append "null" per AbstractStringBuilder.appendNull.
    let Some(cs_obj) = cs else {
        let this = sb_append_str(ctx, this, "null");
        return Ok(Some(Value::Object(Some(this))));
    };

    // Coerce CharSequence to its UTF-16 text:
    //  - java.lang.String: read_string.
    //  - Any CharSequence with a toString()Ljava/lang/String;: invoke_to_string.
    // Both paths already handle StringBuilder/StringBuffer/String/CharBuffer.
    // Producer-#12 fix: re-entrant `invoke_to_string` can move `this`.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let text = match invoke_to_string(&mut *scope, cs_obj) {
        Ok(s) => s,
        Err(_) => String::new(),
    };
    let this = scope.get(&this_handle);
    let chars: Vec<u16> = text.encode_utf16().collect();

    // Clamp [start, end] to the CharSequence's length; real JDK throws
    // IndexOutOfBoundsException, but silently clamping keeps JUnit's
    // error-reporting path alive — the segfault/OOM we're fixing is far worse
    // than an off-by-one in diagnostic output, and every real-world caller
    // inside JDK internals passes in-bounds indices.
    let len = chars.len();
    let s = (start.max(0) as usize).min(len);
    let e = (end.max(0) as usize).min(len);
    let this = if e > s {
        sb_append_chars(&mut *scope, this, &chars[s..e])
    } else {
        this
    };
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (buf, count) = sb_state(ctx, this);
    let buf = match buf {
        Some(b) => b,
        None => return Ok(Some(Value::Object(Some(ctx.create_string_uninterned(""))))),
    };
    let count = count as usize;
    // Read chars and build Rust string
    let mut chars = Vec::with_capacity(count);
    for i in 0..count {
        let ch = match ctx.get_array_element(buf, i) {
            Value::Int(v) => v as u16,
            _ => 0,
        };
        chars.push(ch);
    }
    let text = String::from_utf16_lossy(&chars);
    // `StringBuilder.toString()` must return a *fresh* String distinct from
    // any equal literal — the JVM spec only pools literals and `intern()`.
    // Routing it through the interned pool made `==` wrongly report identity
    // (e.g. `sb.toString() == "literal"`), breaking identity-based symbol
    // comparisons such as xerces' `NamespaceSupport`.
    // `_gc_safe`: `text` is already Rust-owned; `this`/`buf` are not
    // dereferenced again below, so a moving young GC here is safe. Without
    // this, a StringBuilder.toString()-heavy hot loop (e.g. Response.
    // toAbsolute()) hard-aborts the whole process on young-gen exhaustion
    // instead of collecting and continuing -- see docs/known-issues/
    // tomcat-08-07/silent-hang-no-signature-cluster.md.
    let result = ctx.create_string_uninterned_gc_safe(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// AbstractStringBuilder.getChars(int srcBegin, int srcEnd, char[] dst, int dstBegin)
///
/// Copies characters from the builder's buffer into `dst` starting at
/// `dstBegin`.  Throws StringIndexOutOfBoundsException if
/// srcBegin < 0, srcEnd > count, or srcBegin > srcEnd.
pub(crate) fn native_sb_get_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // `dst` is dereferenced (`dst.length`, element stores) — a null array is a
    // NullPointerException per the JDK, not a silent no-op.
    let dst = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot store to null char[] in AbstractStringBuilder.getChars".to_string(),
                ),
            }
            .into())
        }
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    let (buf, count) = sb_state(ctx, this);
    // Source-range check: `AbstractStringBuilder.getChars` first validates the
    // [srcBegin, srcEnd) window against the builder length via
    // `checkRangeSIOOBE`, throwing StringIndexOutOfBoundsException.
    if src_begin < 0 || src_end > count || src_begin > src_end {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(src_begin, src_end, count).into());
    }
    let n = (src_end - src_begin) as usize;
    // Destination-range check: the underlying `System.arraycopy` into `dst`
    // throws (Array)IndexOutOfBoundsException when `dstBegin < 0` or the copied
    // window `[dstBegin, dstBegin + n)` would run past `dst.length`. Previously
    // this was silently ignored, dropping the out-of-bounds writes.
    let dst_len = ctx.array_length(dst) as i64;
    // Use widening i64 arithmetic so `dstBegin + n` cannot wrap (n is bounded by
    // the validated source window, so it fits in i32, but stay defensive).
    let copy_end = i64::from(dst_begin) + (n as i64);
    if dst_begin < 0 || copy_end > dst_len {
        // The first offending destination index, matching JDK arraycopy
        // semantics: a negative dstBegin reports dstBegin; an overrun reports
        // the last index written.
        let bad_index = if dst_begin < 0 {
            dst_begin
        } else {
            (copy_end - 1).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        return Err(
            cratonvm_types::error::RuntimeError::aioobe_no_length(bad_index)
            .into(),
        );
    }
    let buf = match buf {
        Some(b) => b,
        // No backing buffer means a length-0 builder; the source check above
        // already guarantees `n == 0`, so there is nothing to copy.
        None => return Ok(None),
    };
    // `set_array_element` is infallible (returns no Result), so every store is
    // guarded by the explicit destination-range check above rather than relying
    // on a downstream bounds error.
    for i in 0..n {
        let ch = ctx.get_array_element(buf, src_begin as usize + i);
        ctx.set_array_element(dst, dst_begin as usize + i, ch);
    }
    Ok(None)
}

pub(crate) fn native_sb_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, count) = sb_state(ctx, this);
    Ok(Some(Value::Int(count)))
}

/// `AbstractStringBuilder.getCoder()B` — synthetic-layout accessor.
///
/// The real JDK `getCoder()` returns the `coder` byte field of the compact-string
/// `AbstractStringBuilder` (`byte[] value`, `byte coder`, `int count`). CratonVM
/// instead backs StringBuilder/StringBuffer with a synthetic layout (slot 0 =
/// `char[] buffer`, slot 1 = `int count`) that has no `coder` field. Real-JDK
/// bytecode such as `String.nonSyncContentEquals` (reached from
/// `String.contentEquals(CharSequence)`) calls `sb.getCoder()` / `sb.getValue()`
/// directly; with no native override it would read our `int count` as the coder
/// byte and the `char[]` buffer as a compact `byte[]`, forcing a bogus UTF16
/// branch and a `StringIndexOutOfBoundsException` in `StringUTF16.contentEquals`
/// (BUG-TC0622, same family as BUG-M `lastIndexOf`).
///
/// We derive the coder exactly the way `vm_object::create_java_string` does:
/// LATIN1 (0) iff every code unit fits in a byte, otherwise UTF16 (1). Matching
/// that choice keeps the comparison correct: the receiver String's coder and our
/// builder's coder agree whenever the contents do.
pub(crate) fn native_sb_get_coder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let chars = sb_read_chars(ctx, this);
    let coder = if chars.iter().all(|&u| u <= 0xFF) {
        0
    } else {
        1
    };
    Ok(Some(Value::Int(coder)))
}

/// `AbstractStringBuilder.getValue()[B` — synthetic-layout accessor.
///
/// Companion to [`native_sb_get_coder`]: returns a freshly allocated compact
/// `byte[]` view of the synthetic `char[]` buffer, in the SAME layout CratonVM's
/// own Strings use (`vm_object::create_java_string`): LATIN1 packs one byte per
/// char, UTF16 packs two little-endian bytes per char (low byte first). The
/// `coder` implied by this array must match [`native_sb_get_coder`] for the real
/// `nonSyncContentEquals` path to compute correctly.
pub(crate) fn native_sb_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Snapshot the chars into a Rust-local Vec BEFORE allocating, so the moving
    // GC that `new_array` may trigger cannot leave us with a stale `this`/buffer.
    let chars = sb_read_chars(ctx, this);
    let latin1 = chars.iter().all(|&u| u <= 0xFF);
    let byte_len = if latin1 { chars.len() } else { chars.len() * 2 };
    let arr = ctx.new_array(ArrayElementType::Byte, byte_len);
    if latin1 {
        for (i, &u) in chars.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int((u & 0xFF) as i32));
        }
    } else {
        // Little-endian UTF16: low byte at even index, high byte at odd index —
        // consistent with create_java_string and StringUTF16.isBigEndian()==false.
        for (i, &u) in chars.iter().enumerate() {
            ctx.set_array_element(arr, i * 2, Value::Int((u & 0xFF) as i32));
            ctx.set_array_element(arr, i * 2 + 1, Value::Int(((u >> 8) & 0xFF) as i32));
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_sb_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (buf, count) = sb_state(ctx, this);
    if index < 0 || index >= count {
        // Message as well as class: `AbstractStringBuilder.charAt` delegates
        // to `String.checkIndex` → `Preconditions.checkIndex(index, count,
        // SIOOBE_FORMATTER)`.
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    }
    let buf = buf.unwrap();
    let ch = ctx.get_array_element(buf, index as usize);
    Ok(Some(ch))
}

/// `AbstractStringBuilder.codePointAt(int index)`.
///
/// MUST be a native: unregistered, this falls through to the real JDK
/// bytecode, which calls `checkIndex`/`isLatin1()` and then either indexes
/// the compact `byte[] value` field directly or delegates to
/// `StringUTF16.codePointAt(value, index, count)`. CratonVM's StringBuilder
/// backing is a synthetic `char[]` (slot 0 = `char[] buffer`, slot 1 =
/// `int count`; no `coder`/compact `byte[] value` fields), so that real
/// bytecode reads garbage out of the mismatched layout — surfacing as a
/// bare `ArrayIndexOutOfBoundsException` (no message, since it is CratonVM's
/// own array-bounds-check codegen faulting on the miscomputed index/array,
/// not a real `new ArrayIndexOutOfBoundsException(...)` call) and, because
/// it depends on whatever the interpreter/JIT happens to have left in the
/// aliased slot, non-deterministically. This is the same layout-mismatch
/// family as `getValue`/`getCoder` (BUG-TC0622) and `charAt` above — see
/// `native_string_code_point_at` for the equivalent `String` native, whose
/// surrogate-pair handling this mirrors.
pub(crate) fn native_sb_code_point_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index_i32 = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    if index_i32 < 0 || (index_i32 as usize) >= chars.len() {
        // `AbstractStringBuilder.codePointAt` also delegates to
        // `String.checkIndex`, so it carries the same text.
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(
            index_i32,
            chars.len() as i32,
        )
        .into());
    }
    let index = index_i32 as usize;
    let ch = chars[index];
    if (0xD800..=0xDBFF).contains(&ch) && index + 1 < chars.len() {
        let low = chars[index + 1];
        if (0xDC00..=0xDFFF).contains(&low) {
            let cp = 0x10000 + ((ch as i32 - 0xD800) << 10) + (low as i32 - 0xDC00);
            return Ok(Some(Value::Int(cp)));
        }
    }
    Ok(Some(Value::Int(ch as i32)))
}

/// `AbstractStringBuilder.codePointBefore(int index)` — companion to
/// [`native_sb_code_point_at`], same layout-mismatch rationale.
pub(crate) fn native_sb_code_point_before(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index_i32 = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    if index_i32 <= 0 || (index_i32 as usize) > chars.len() {
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index_i32, chars.len() as i32).into());
    }
    let index = index_i32 as usize;
    let ch = chars[index - 1];
    if (0xDC00..=0xDFFF).contains(&ch) && index >= 2 {
        let high = chars[index - 2];
        if (0xD800..=0xDBFF).contains(&high) {
            let cp = 0x10000 + ((high as i32 - 0xD800) << 10) + (ch as i32 - 0xDC00);
            return Ok(Some(Value::Int(cp)));
        }
    }
    Ok(Some(Value::Int(ch as i32)))
}

/// `AbstractStringBuilder.codePointCount(int beginIndex, int endIndex)` —
/// same layout-mismatch rationale as [`native_sb_code_point_at`].
pub(crate) fn native_sb_code_point_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let begin = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let end = end.min(chars.len());
    let mut count = 0;
    let mut i = begin;
    while i < end {
        let ch = chars[i];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < end {
            let low = chars[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                i += 2;
                count += 1;
                continue;
            }
        }
        i += 1;
        count += 1;
    }
    Ok(Some(Value::Int(count)))
}

/// `AbstractStringBuilder.appendCodePoint(int codePoint)`.
///
/// MUST be a native for the same reason as [`native_sb_code_point_at`]:
/// unregistered, real JDK bytecode would encode the code point into the
/// compact `byte[] value` field (growing/re-coding it via
/// `ensureCapacityNewCoder`), which CratonVM's synthetic `char[]`-backed
/// StringBuilder does not have — corrupting the backing array.
pub(crate) fn native_sb_append_code_point(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cp = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let mut buf = [0u16; 2];
    let encoded: &[u16] = match char::from_u32(cp as u32) {
        Some(c) => c.encode_utf16(&mut buf),
        None => {
            // Not a valid Unicode scalar value (e.g. an unpaired surrogate
            // used internally by the parser) — Character.toChars would throw
            // IllegalArgumentException for these, but WHATWG callers only
            // ever appendCodePoint values already validated as scalar
            // values/ASCII, so fall back to truncating to a single UTF-16
            // unit rather than diverging further.
            buf[0] = cp as u16;
            &buf[..1]
        }
    };
    let this = sb_append_chars(ctx, this, encoded);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_reverse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (buf, count) = sb_state(ctx, this);
    if let Some(buf) = buf {
        let count = count as usize;
        let mut i = 0;
        let mut j = if count > 0 { count - 1 } else { 0 };
        while i < j {
            let a = ctx.get_array_element(buf, i);
            let b = ctx.get_array_element(buf, j);
            ctx.set_array_element(buf, i, b);
            ctx.set_array_element(buf, j, a);
            i += 1;
            j -= 1;
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// StringBuilder mutation methods
// ---------------------------------------------------------------------------

/// Helper: read the current content of a StringBuilder as a Vec<u16>.
pub(crate) fn sb_read_chars(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> Vec<u16> {
    let (buf, count) = sb_state(ctx, this);
    let count = count.max(0) as usize;
    let mut chars = Vec::with_capacity(count);
    if let Some(buf) = buf {
        let count = count.min(ctx.array_length(buf));
        for i in 0..count {
            let val = ctx.get_array_element(buf, i);
            chars.push(match val {
                Value::Int(c) => c as u16,
                _ => 0,
            });
        }
    }
    chars
}

/// Helper: write a Vec<u16> back into a StringBuilder, replacing all content.
pub(crate) fn sb_write_chars(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    chars: &[u16],
) -> cratonvm_types::ObjectRef {
    let current_count = sb_state(ctx, this).1 as usize;
    let additional = chars.len().saturating_sub(current_count);
    let (this, buf) = sb_ensure_capacity(ctx, this, additional);
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(ch as i32));
    }
    sb_set_count(ctx, this, chars.len() as i32);
    this
}

/// insert(int, String) — insert string at offset
pub(crate) fn native_sb_insert_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let insert_str = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => "null".to_string(),
    };
    let insert_chars: Vec<u16> = insert_str.encode_utf16().collect();

    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, char) — insert single char
pub(crate) fn native_sb_insert_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let ch = match args.get(2) {
        Some(Value::Int(c)) => *c as u16,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + 1);
    result.extend_from_slice(&chars[..offset]);
    result.push(ch);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, int) — insert int as string
pub(crate) fn native_sb_insert_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let val = match args.get(2) {
        Some(Value::Int(v)) => v.to_string(),
        _ => "0".to_string(),
    };
    let insert_chars: Vec<u16> = val.encode_utf16().collect();
    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, Object) — insert Object via toString
pub(crate) fn native_sb_insert_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let text = match args.get(2) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        Some(Value::Int(v)) => v.to_string(),
        Some(Value::Long(v)) => v.to_string(),
        _ => "null".to_string(),
    };
    let insert_chars: Vec<u16> = text.encode_utf16().collect();
    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `insert(int, char[], int, int)` — insert a char[] slice at `offset`.
///
/// Unlike the other `insert` overloads above, this specific 4-arg signature
/// had no native override, so real-JDK bytecode for `AbstractStringBuilder
/// .insert(int, char[], int, int)` ran directly against CratonVM's synthetic
/// StringBuilder/StringBuffer layout (`char[] buffer` @0, `int count` @1 —
/// see `native_sb_get_coder`'s doc comment for the full layout mismatch).
/// That method's real bytecode reads the REAL JDK field `count` (a field
/// slot that doesn't exist in our synthetic layout, so it reads back 0) and
/// calls `checkOffset(dstOffset, count)`, which throws
/// `ArrayIndexOutOfBoundsException` for any nonzero `dstOffset` — exactly
/// the failure Log4j2's `FormattingInfo.format`/`ColorConverter` hits when
/// padding a partially-built line (`sbuf.insert(fieldStart, spaces, 0, n)`
/// with `fieldStart > 0`), producing "An exception occurred processing
/// Appender STDOUT" and the log line never reaching `System.out`.
pub(crate) fn native_sb_insert_char_array_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let arr = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let src_off = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let src_len = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    let src_start = src_off.min(arr_len);
    let src_end = src_off.saturating_add(src_len).min(arr_len);
    let mut insert_chars = Vec::with_capacity(src_end.saturating_sub(src_start));
    for i in src_start..src_end {
        insert_chars.push(match ctx.get_array_element(arr, i) {
            Value::Int(c) => c as u16,
            _ => 0,
        });
    }

    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `insert(int, char[])` — full-array variant (no offset/len). Same
/// synthetic-vs-real-layout rationale as `native_sb_insert_char_array_off_len`
/// above: `StringBuilder`/`AbstractStringBuilder` both declare this overload
/// with real (non-`native`) bytecode, which — without a native override —
/// reads the real-layout `count` field (absent from our synthetic char[]/int
/// layout) and throws `ArrayIndexOutOfBoundsException` from `checkOffset`.
pub(crate) fn native_sb_insert_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let arr = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let arr_len = ctx.array_length(arr);
    let mut insert_chars = Vec::with_capacity(arr_len);
    for i in 0..arr_len {
        insert_chars.push(match ctx.get_array_element(arr, i) {
            Value::Int(c) => c as u16,
            _ => 0,
        });
    }

    let chars = sb_read_chars(ctx, this);
    let offset = std::cmp::min(offset, chars.len());
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// delete(int, int) — remove range [start, end)
pub(crate) fn native_sb_delete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let mut chars = sb_read_chars(ctx, this);
    let start = std::cmp::min(start, chars.len());
    let end = std::cmp::min(end, chars.len());
    if start < end {
        chars.drain(start..end);
    }
    let this = sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// deleteCharAt(int) — remove single char
pub(crate) fn native_sb_delete_char_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let chars = sb_read_chars(ctx, this);
    let this = if index < chars.len() {
        let mut result = Vec::with_capacity(chars.len() - 1);
        result.extend_from_slice(&chars[..index]);
        result.extend_from_slice(&chars[index + 1..]);
        sb_write_chars(ctx, this, &result)
    } else {
        this
    };
    Ok(Some(Value::Object(Some(this))))
}

/// replace(int, int, String) — replace range with string
pub(crate) fn native_sb_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let replacement = match args.get(3) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => String::new(),
    };
    let repl_chars: Vec<u16> = replacement.encode_utf16().collect();
    let mut chars = sb_read_chars(ctx, this);
    let start = std::cmp::min(start, chars.len());
    let end = std::cmp::min(end, chars.len());
    if start <= end {
        chars.splice(start..end, repl_chars);
    }
    let this = sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// setCharAt(int, char) — set char at index
pub(crate) fn native_sb_set_char_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => return Ok(None),
    };
    let ch = match args.get(2) {
        Some(Value::Int(c)) => *c as u16,
        _ => return Ok(None),
    };
    let (buf, count) = sb_state(ctx, this);
    if let Some(buf) = buf {
        if index < count as usize {
            ctx.set_array_element(buf, index, Value::Int(ch as i32));
        }
    }
    Ok(None)
}

/// setLength(int) — truncate or extend with null chars
pub(crate) fn native_sb_set_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let new_len = match args.get(1) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => return Ok(None),
    };
    let (buf, count) = sb_state(ctx, this);
    let count = count as usize;
    if new_len > count {
        // Extend with null chars
        let (this, buf) = sb_ensure_capacity(ctx, this, new_len - count);
        for i in count..new_len {
            ctx.set_array_element(buf, i, Value::Int(0));
        }
        sb_set_count(ctx, this, new_len as i32);
    } else {
        if new_len < count {
            // Just zero the excess (optional for correctness), but must update count
            if let Some(buf) = buf {
                for i in new_len..count {
                    ctx.set_array_element(buf, i, Value::Int(0));
                }
            }
        }
        sb_set_count(ctx, this, new_len as i32);
    }
    Ok(None)
}

/// indexOf(String) — find substring, return -1 if not found
pub(crate) fn native_sb_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chars = sb_read_chars(ctx, this);
    let haystack: String = String::from_utf16_lossy(&chars);
    match haystack.find(&target) {
        Some(byte_pos) => {
            // Convert byte position to char position
            let char_pos = haystack[..byte_pos].encode_utf16().count();
            Ok(Some(Value::Int(char_pos as i32)))
        }
        None => Ok(Some(Value::Int(-1))),
    }
}

/// indexOf(String, int) — find substring from offset
pub(crate) fn native_sb_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from = match args.get(2) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    if from >= chars.len() {
        return Ok(Some(Value::Int(-1)));
    }
    let haystack: String = String::from_utf16_lossy(&chars[from..]);
    match haystack.find(&target) {
        Some(byte_pos) => {
            let char_pos = haystack[..byte_pos].encode_utf16().count();
            Ok(Some(Value::Int((from + char_pos) as i32)))
        }
        None => Ok(Some(Value::Int(-1))),
    }
}

/// Last occurrence of `needle` in `haystack` at a start index <= `from`
/// (UTF-16 code-unit indices), or -1. Matches String/AbstractStringBuilder
/// `lastIndexOf` semantics, including the empty-needle case.
fn u16_last_index_of(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    let n = haystack.len() as i32;
    let m = needle.len() as i32;
    if m == 0 {
        return from.clamp(0, n);
    }
    if m > n {
        return -1;
    }
    let mut k = (n - m).min(from);
    while k >= 0 {
        let s = k as usize;
        if &haystack[s..s + m as usize] == needle {
            return k;
        }
        k -= 1;
    }
    -1
}

/// lastIndexOf(String) — last occurrence of substring, or -1.
pub(crate) fn native_sb_last_index_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx
            .read_string(*obj)
            .unwrap_or_default()
            .encode_utf16()
            .collect(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(u16_last_index_of(
        &chars,
        &target,
        chars.len() as i32,
    ))))
}

/// lastIndexOf(String, int) — last occurrence at a start index <= fromIndex.
pub(crate) fn native_sb_last_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx
            .read_string(*obj)
            .unwrap_or_default()
            .encode_utf16()
            .collect(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(u16_last_index_of(&chars, &target, from))))
}

/// substring(int) — substring from index to end
pub(crate) fn native_sb_substring(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let count = chars.len() as i32;
    // `substring(start)` is `substring(start, count)`, and the real body's
    // first statement is `Preconditions.checkFromToIndex(start, end, count,
    // SIOOBE_FORMATTER)`. Clamping instead — which this used to do — turned
    // `sb.substring(-1)` into the whole sequence and `sb.substring(99)` into
    // "": a silent wrong answer where the caller asked for an exception.
    if let Some(failure) = sb_check_from_to_index(start, count, count) {
        return Err(failure);
    }
    let result = String::from_utf16_lossy(&chars[start as usize..]);
    let str_obj = ctx.create_string_uninterned(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// `Preconditions.checkFromToIndex(start, end, count, SIOOBE_FORMATTER)` — the
/// range check `AbstractStringBuilder.substring`/`subSequence` open with.
///
/// Returns the failure to raise, or `None` when the range is valid.
fn sb_check_from_to_index(
    start: i32,
    end: i32,
    count: i32,
) -> Option<cratonvm_types::error::MethodCallFailed> {
    if start >= 0 && start <= end && end <= count {
        return None;
    }
    Some(cratonvm_types::error::RuntimeError::sioobe_range(start, end, count).into())
}

/// substring(int, int) — substring [start, end)
pub(crate) fn native_sb_substring_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let count = chars.len() as i32;
    // See `native_sb_substring`: the real body checks the range before doing
    // anything, so `sb.substring(3, 2)` is a `StringIndexOutOfBoundsException`
    // and not the empty string this used to clamp it into.
    if let Some(failure) = sb_check_from_to_index(start, end, count) {
        return Err(failure);
    }
    let result = String::from_utf16_lossy(&chars[start as usize..end as usize]);
    let str_obj = ctx.create_string_uninterned(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// capacity() — return backing array length
pub(crate) fn native_sb_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (buf, _) = sb_state(ctx, this);
    let cap = buf.map_or(0, |b| ctx.array_length(b));
    Ok(Some(Value::Int(cap as i32)))
}

/// ensureCapacity(int) — grow if needed
pub(crate) fn native_sb_ensure_cap(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let min_cap = match args.get(1) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => return Ok(None),
    };
    let (buf, count) = sb_state(ctx, this);
    let old_cap = buf.map_or(0, |b| ctx.array_length(b));
    if min_cap > old_cap {
        let needed = min_cap.saturating_sub(count as usize);
        let _ = sb_ensure_capacity(ctx, this, needed);
    }
    Ok(None)
}

/// `trimToSize()` — shrink the backing `char[]` to exactly `count`.
///
/// This was `native_noop_with_this` on the assumption that capacity is not
/// observable. It IS observable on this VM: `native_sb_capacity` (registered
/// on the same class, three lines above `trimToSize`) answers
/// `array_length(value)`, so after a no-op trim `capacity()` still reported
/// the pre-trim buffer size — a directly visible divergence from
/// `AbstractStringBuilder.trimToSize`, which reallocates `value` to exactly
/// `count`. The buffer is also the only thing holding the surplus memory
/// alive, so the no-op defeated the method's entire purpose.
///
/// Mirrors `sb_ensure_capacity`'s discipline for the reverse direction:
/// allocating the replacement array can move `this`, so root it in a
/// `NativeHandleScope`, re-read the receiver, and bulk-copy the live prefix
/// out of the OLD buffer read back through the (post-GC) receiver.
///
/// A receiver on the real JDK 9+ 3-slot layout (`value: byte[]`) is left
/// untouched: `sb_state` only recognises a `char[]` payload and yields `None`,
/// and the compact byte[] representation is not managed by these natives.
pub(crate) fn native_sb_trim_to_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;

    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (buf, count) = sb_state(ctx, this);
    let Some(buf) = buf else {
        return Ok(None);
    };
    let old_cap = ctx.array_length(buf);
    let count = (count.max(0) as usize).min(old_cap);
    if count == old_cap {
        // Already exact — `trimToSize` is defined to be a no-op in that case,
        // and skipping the copy keeps the common path allocation-free.
        return Ok(None);
    }

    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let new_buf = scope.new_array(ArrayElementType::Char, count);
    let this = scope.get(&this_handle);

    if let Value::Object(Some(old_buf)) = scope.get_field(this, 0) {
        if scope.object_is_array(old_buf)
            && scope.heap_element_type_of(old_buf) == ArrayElementType::Char
        {
            let _ = scope.bulk_array_copy(old_buf, 0, new_buf, 0, count);
        }
    }
    scope.set_field(this, 0, Value::Object(Some(new_buf)));
    Ok(None)
}

/// Format a float like Java does.
pub(crate) fn format_float(v: f32) -> String {
    // See `format_double` — shared with the scientific-notation threshold fix.
    cratonvm_types::java_float_to_string(v)
}

// ---------------------------------------------------------------------------

/// Helper: read a String's char[] into a Vec<u16>.
///
/// Retained for back-compat at call sites that still need an owned Vec.
/// Performance-critical binary callers (compareTo, indexOf, startsWith,
/// endsWith, contains, replace) should prefer `with_string_chars_scratch`
/// or `with_two_string_chars_scratches` to avoid this allocation entirely.
pub(crate) fn read_string_chars(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Vec<u16> {
    // Layout-aware decode: handles legacy char[] values as well as JDK 9+
    // compact byte[] values in either LATIN-1 or UTF-16 coding. Reading the
    // raw byte[] length as a char count (or each byte as a char) silently
    // corrupts every non-LATIN-1 string.
    let mut chars = Vec::new();
    decode_string_chars(ctx, obj, &mut chars);
    chars
}

pub(crate) fn native_string_to_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // We need exclusive &mut for new_array, which would clash with holding
    // the scratch borrow + an immutable &dyn NativeContext. Read the chars
    // first via the scratch (drops the borrow on return), then copy out the
    // length so we can allocate, then re-read into the destination array.
    // For this call we drop down to read_string_chars (one Vec alloc) since
    // the chars must outlive the new_array call.
    let chars = read_string_chars(ctx, this);
    let arr = ctx.new_array(ArrayElementType::Char, chars.len());
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(ch as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_string_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let found = with_two_string_chars_scratches(ctx, this, other, |haystack, needle| {
        if needle.is_empty() {
            return true;
        }
        if needle.len() > haystack.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    });
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_string_starts_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let prefix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = with_two_string_chars_scratches(ctx, this, prefix, |this_chars, prefix_chars| {
        this_chars.starts_with(prefix_chars)
    });
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_string_starts_with_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let prefix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let result = with_two_string_chars_scratches(ctx, this, prefix, |this_chars, prefix_chars| {
        if offset <= this_chars.len() {
            this_chars[offset..].starts_with(prefix_chars)
        } else {
            false
        }
    });
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_string_ends_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let suffix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = with_two_string_chars_scratches(ctx, this, suffix, |this_chars, suffix_chars| {
        this_chars.ends_with(suffix_chars)
    });
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_string_trim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let trimmed = text.trim().to_string();
    let result = ctx.create_string_uninterned(&trimmed);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_replace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let old_char = match args.get(1) {
        Some(Value::Int(v)) => *v as u16,
        _ => 0,
    };
    let new_char = match args.get(2) {
        Some(Value::Int(v)) => *v as u16,
        _ => 0,
    };
    // Build the replaced string into the thread-local scratch, then
    // materialize a Rust String once for `create_string`. (We can't keep
    // the &str borrow alive across the create_string call because that
    // takes &mut NativeContext, so the scratch borrow must end first —
    // hence we copy into an owned String.)
    let text = with_string_chars_scratch(ctx, this, |chars| {
        let mut out: Vec<u16> = Vec::with_capacity(chars.len());
        for &ch in chars {
            out.push(if ch == old_char { new_char } else { ch });
        }
        String::from_utf16_lossy(&out)
    });
    let result = ctx.create_string_uninterned(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// `String.replace(CharSequence target, CharSequence replacement)` — **literal**
/// (non-regex) all-occurrences replacement. The real-JDK bytecode runs a
/// per-char interpreted scan that is ~13× slower than HotSpot under CratonVM (the
/// dominant cost in the `PluginXmlParser.format` chain after the regex methods
/// are routed to natives). Rust's `str::replace(from, to)` performs the exact
/// same left-to-right, non-overlapping, all-occurrences literal replacement,
/// including the empty-target case (`"abc".replace("", "X")` → `"XaXbXcX"`),
/// so the result is byte-identical to Java. Targets/replacements are coerced via
/// `invoke_to_string` so `StringBuilder`/`StringBuffer`/`CharBuffer` arguments
/// (not just `String`) are handled. Routed only under
/// `CRATONVM_NATIVE_STRING_REGEX` (see `register_essential_natives`).
pub(crate) fn native_string_replace_charseq(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let target = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            invoke_to_string(&mut *scope, *o).unwrap_or_default()
        }
        // A `null` target is an NPE, matching the JDK. This used to return a
        // null `String` on the reasoning that "real callers never pass null" —
        // which is a claim about callers, not about the method, and it made
        // `s.replace(null, "x")` hand back `null` where HotSpot throws.
        _ => return Err(regex_arg_npe("target").into()),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(o))) => {
            invoke_to_string(&mut *scope, *o).unwrap_or_default()
        }
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let this = scope.get(&this_handle);
    let s = scope.read_string(this).unwrap_or_default();
    let result = s.replace(&target, &replacement);
    Ok(Some(Value::Object(Some(
        scope.create_string_uninterned(&result),
    ))))
}

/// Shared body of every `String.to{Lower,Upper}Case` native.
///
/// `locale` is the explicit `Locale` argument (`None` for the no-arg overloads,
/// which the JDK defines as `toXxxCase(Locale.getDefault())`). Its language
/// decides which of two paths runs:
///
/// * **Ordinary locales** keep the historical fast path: an in-place ASCII fold,
///   or Rust's Unicode full mapping. That is byte-identical to HotSpot for the
///   root locale — `ß → SS`, `ΣΣ → σς`, `İ → i̇` are all verified
///   differentially.
/// * **`tr`/`az`/`lt`** take [`crate::case_map`], the port of the JDK's
///   `ConditionalSpecialCasing`.
///
/// `memoize` selects the per-receiver `ascii_case_string` cache, which only some
/// of the registration sites used before they were unified here (see
/// [`native_string_to_lower_case`] vs [`native_string_to_lower_case_uncached`]).
/// The locale-dependent path never uses it: the cache is keyed by receiver +
/// direction only, so `s.toLowerCase()` and `s.toLowerCase(TURKISH)` would share
/// an entry and one would be served the other's answer.
///
/// An unchanged String returns the receiver itself, as `String.toLowerCase`
/// does; a changed one is a fresh, uninterned String.
fn string_case_impl(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    locale: Option<cratonvm_types::ObjectRef>,
    lowercase: bool,
    memoize: bool,
) -> MethodCallResult {
    // A cached result is valid only for the same Locale object. Checking it
    // before resolving the language avoids a contended synthetic-locale lookup
    // for repeated ASCII case conversion, while preserving Turkish/Lithuanian
    // and other locale-specific mappings.
    if memoize {
        if let Some(result) = ctx.get_ascii_case_string_cached(this, locale, !lowercase) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    let lang = crate::locale_language_for_case_mapping(ctx, locale);
    if crate::case_map::is_locale_dependent(&lang) {
        let src = ctx.read_string(this).unwrap_or_default();
        let mapped = if lowercase {
            crate::case_map::to_lower_case(&src, &lang)
        } else {
            crate::case_map::to_upper_case(&src, &lang)
        };
        if mapped == src {
            return Ok(Some(Value::Object(Some(this))));
        }
        return Ok(Some(Value::Object(Some(
            ctx.create_string_uninterned_gc_safe(&mapped),
        ))));
    }

    let mut folded = ctx.read_string(this).unwrap_or_default();
    let changed = if folded.is_ascii() {
        let changed = folded.bytes().any(|byte| {
            if lowercase {
                byte.is_ascii_uppercase()
            } else {
                byte.is_ascii_lowercase()
            }
        });
        if lowercase {
            folded.make_ascii_lowercase();
        } else {
            folded.make_ascii_uppercase();
        }
        changed
    } else {
        let mapped = if lowercase {
            folded.to_lowercase()
        } else {
            folded.to_uppercase()
        };
        if mapped == folded {
            false
        } else {
            folded = mapped;
            true
        }
    };
    let result = if !changed {
        this
    } else if memoize {
        ctx.create_ascii_case_string_cached(this, locale, &folded, !lowercase)
    } else {
        ctx.create_string_uninterned_gc_safe(&folded)
    };
    Ok(Some(Value::Object(Some(result))))
}

/// The `Locale` operand of a `to{Lower,Upper}Case(Locale)` native, if the call
/// has one. A null (or absent) argument means "use the default locale".
pub(crate) fn locale_arg(args: &[Value], index: usize) -> Option<cratonvm_types::ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn string_case_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    lowercase: bool,
    memoize: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    string_case_impl(ctx, this, locale_arg(args, 1), lowercase, memoize)
}

/// `String.toLowerCase()` / `toLowerCase(Locale)` **with** the per-receiver
/// memo — the synthetic-JDK registration and the `StringLatin1` delegate, whose
/// callers re-lowercase the same receiver in a loop.
pub(crate) fn native_string_to_lower_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, true, true)
}

/// `String.toUpperCase()` / `toUpperCase(Locale)` **with** the per-receiver memo.
pub(crate) fn native_string_to_upper_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, false, true)
}

/// `String.toLowerCase()` / `toLowerCase(Locale)` **without** the memo — the
/// real-JDK essential set and the `phases_early` overloads, which each allocated
/// a fresh String per call before they shared this implementation. Keeping them
/// uncached means unifying the four sites changes only the locale handling.
pub(crate) fn native_string_to_lower_case_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, true, false)
}

/// StringLatin1.toLowerCase(String, byte[], Locale).
///
/// The compact-string helper carries the source String and Locale in slots zero
/// and two respectively. Keep it as a named callback so the cached static
/// invoke path can recognize its fixed reference-only signature without
/// re-resolving the method descriptor on every lookup.
pub fn native_string_latin1_to_lower_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = args.first().cloned().unwrap_or(Value::Object(None));
    let locale = args.get(2).cloned().unwrap_or(Value::Object(None));
    native_string_to_lower_case(ctx, &[this, locale])
}

/// Whether a cached virtual-native call is one of the String lower-case
/// implementations with the one-object Locale argument.
///
/// The interpreter uses this identity check to avoid re-resolving the
/// descriptor and allocating an argument Vec at every already-cached call
/// site. Both variants have the exact same Java signature and native safety
/// contract.
pub fn is_lower_case_native_callback(callback: cratonvm_native_api::NativeCallback) -> bool {
    let callback = callback as usize;
    callback == native_string_to_lower_case as usize
        || callback == native_string_to_lower_case_uncached as usize
        || callback == native_string_latin1_to_lower_case as usize
}

/// `String.toLowerCase(Locale)` for the JIT's thin direct-call helpers, which
/// hold raw `ObjectRef`s rather than a `&[Value]`.
///
/// Same implementation (and same per-receiver memo) as the interpreted native —
/// which is the point: the JIT helper used to carry its own copy of the mapping
/// and ignore the `Locale`, so `s.toLowerCase(TURKISH)` returned the Turkish
/// answer interpreted and the root answer once the caller tiered up.
pub fn jit_string_to_lower_case(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    locale: Option<cratonvm_types::ObjectRef>,
) -> Option<cratonvm_types::ObjectRef> {
    match string_case_impl(ctx, this, locale, true, true) {
        Ok(Some(Value::Object(Some(o)))) => Some(o),
        _ => None,
    }
}

/// `String.toUpperCase()` / `toUpperCase(Locale)` **without** the memo.
pub(crate) fn native_string_to_upper_case_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, false, false)
}

pub(crate) fn native_string_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let len = match string_char_array(ctx, this) {
        Some((_, len)) => len,
        None => 0,
    };
    Ok(Some(Value::Int(if len == 0 { 1 } else { 0 })))
}

pub(crate) fn native_string_value_of_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let result = ctx.create_string_uninterned(&val.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let result = ctx.create_string_uninterned(if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let result = ctx.create_string_uninterned(&format_double(val));
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\0'),
        _ => '\0',
    };
    let result = ctx.create_string_uninterned(&ch.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let result = ctx.create_string_uninterned(&format_float(val));
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // JDK contract: `return (obj == null) ? "null" : obj.toString();` — for a
    // non-null obj this returns EXACTLY whatever obj.toString() returns,
    // including a legitimate Java null if toString() itself returns null
    // (legal — e.g. TestJspWriterImpl's bug54241b, an anonymous class whose
    // toString() explicitly `return null;`). That null must propagate as an
    // actual null reference, NOT the text "null": JspWriterImpl.print(Object)
    // is `write(String.valueOf(obj))`, and java.io.Writer's default
    // write(String) legitimately throws NullPointerException on a real null
    // (str.length()) but would silently write 4 chars for the text "null".
    match args.first() {
        Some(Value::Object(Some(obj))) => match invoke_to_string_opt(ctx, *obj)? {
            Some(text) => {
                let result = ctx.create_string_uninterned(&text);
                Ok(Some(Value::Object(Some(result))))
            }
            None => Ok(Some(Value::Object(None))),
        },
        _ => {
            let result = ctx.create_string_uninterned("null");
            Ok(Some(Value::Object(Some(result))))
        }
    }
}

pub(crate) fn native_string_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.compareTo on null argument".to_string()),
            }
            .into())
        }
    };
    let result = with_two_string_chars_scratches(ctx, this, other, |a, b| {
        let min_len = std::cmp::min(a.len(), b.len());
        for i in 0..min_len {
            let diff = a[i] as i32 - b[i] as i32;
            if diff != 0 {
                return diff;
            }
        }
        a.len() as i32 - b.len() as i32
    });
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_string_index_of_str(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let result = with_two_string_chars_scratches(ctx, this, target, |haystack, needle| -> i32 {
        if needle.is_empty() {
            return 0;
        }
        if needle.len() > haystack.len() {
            return -1;
        }
        for i in 0..=(haystack.len() - needle.len()) {
            if &haystack[i..i + needle.len()] == needle {
                return i as i32;
            }
        }
        -1
    });
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_string_substring_one(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.substring on null".to_string()),
            }
            .into())
        }
    };
    let begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Fast path: validate against array length and read only [begin..end),
    // instead of allocating a full UTF-16 vector for the whole string.
    let (arr_opt, char_count, is_byte_array, is_utf16) = match string_char_array(ctx, this) {
        Some((arr, total_len)) => {
            let elem_type = ctx.heap_element_type_of(arr);
            let is_byte_array = matches!(
                elem_type,
                cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
            );
            let (char_count, is_utf16) = if is_byte_array {
                let utf16 = matches!(ctx.get_field(this, 1), Value::Int(1));
                if utf16 {
                    (total_len / 2, true)
                } else {
                    (total_len, false)
                }
            } else {
                (total_len, false)
            };
            (Some(arr), char_count, is_byte_array, is_utf16)
        }
        None => (None, 0usize, false, false),
    };
    let end = char_count as i32;

    if begin < 0 || begin > end {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(begin, end, char_count as i32).into());
    }

    let b = begin as usize;
    let e = end as usize;
    let sub_len = e - b;
    let mut sub_utf16: Vec<u16> = Vec::with_capacity(sub_len);
    if let Some(arr) = arr_opt {
        if is_byte_array && is_utf16 {
            // Little-endian: low byte at even index, high byte at odd index.
            for c in b..e {
                let lo = match ctx.get_array_element(arr, c * 2) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                let hi = match ctx.get_array_element(arr, c * 2 + 1) {
                    Value::Int(v) => (v as u8) as u16,
                    _ => 0,
                };
                sub_utf16.push((hi << 8) | lo);
            }
        } else if is_byte_array {
            for i in b..e {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xff) as u16,
                    _ => 0,
                };
                sub_utf16.push(ch);
            }
        } else {
            for i in b..e {
                let ch = match ctx.get_array_element(arr, i) {
                    Value::Int(v) => (v & 0xffff) as u16,
                    _ => 0,
                };
                sub_utf16.push(ch);
            }
        }
    }
    let sub_text = String::from_utf16_lossy(&sub_utf16);
    let result = ctx.create_string_uninterned(&sub_text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let bytes = text.as_bytes();
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_string_concat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.concat: argument is null".to_string()),
            }
            .into())
        }
    };
    let a = ctx.read_string(this).unwrap_or_default();
    let b = ctx.read_string(other).unwrap_or_default();
    let combined = format!("{a}{b}");
    let result = ctx.create_string_uninterned(&combined);
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Phase 8 Part 7: Additional String methods
// ---------------------------------------------------------------------------

pub(crate) fn native_string_split(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_string_split_impl(ctx, args, 0)
}

pub(crate) fn native_string_split_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    native_string_split_impl(ctx, args, limit)
}

pub(crate) fn native_string_split_private(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let with_delimiters = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    if !with_delimiters {
        return native_string_split_impl(ctx, args, limit);
    }
    native_string_split_with_delimiters(ctx, args, limit)
}

fn string_array_from_parts(ctx: &mut dyn NativeContext, parts: &[String]) -> MethodCallResult {
    let string_class_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(id) => id,
        Err(_) => ctx.ensure_synthetic_class("java/lang/String", 8),
    };
    let arr = ctx.new_ref_array(string_class_id, parts.len());
    for (i, part) in parts.iter().enumerate() {
        let str_ref = ctx.create_string_uninterned(part);
        ctx.set_array_element(arr, i, Value::Object(Some(str_ref)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_string_split_with_delimiters(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    limit: i32,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let delim_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let delim = ctx.read_string(delim_obj).unwrap_or_default();
    let limited = limit > 0;
    let mut parts = Vec::new();
    let mut last = 0usize;
    let mut pos = 0usize;
    let mut matches_seen = 0i32;

    if let Ok(re) = compile_java_regex(&delim, 0) {
        while pos <= s.len() {
            if limited && matches_seen >= limit - 1 {
                break;
            }
            let Some(m) = re.find(&s[pos..]) else {
                break;
            };
            let start = pos + m.start;
            let end = pos + m.end;
            parts.push(s[last..start].to_string());
            parts.push(s[start..end].to_string());
            matches_seen += 1;
            last = end;
            if start == end {
                let mut next = end + 1;
                while next < s.len() && !s.is_char_boundary(next) {
                    next += 1;
                }
                pos = next.max(end);
            } else {
                pos = end;
            }
        }
    } else if !delim.is_empty() {
        while let Some(rel) = s[pos..].find(&delim) {
            if limited && matches_seen >= limit - 1 {
                break;
            }
            let start = pos + rel;
            let end = start + delim.len();
            parts.push(s[last..start].to_string());
            parts.push(delim.clone());
            matches_seen += 1;
            last = end;
            pos = end;
        }
    }

    if parts.is_empty() && last == 0 {
        parts.push(s);
    } else {
        parts.push(s[last..].to_string());
    }
    if limit == 0 {
        while parts.last().map(|s| s.is_empty()).unwrap_or(false) {
            parts.pop();
        }
    }
    string_array_from_parts(ctx, &parts)
}

fn is_java_ascii_regex_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | b'\x0c' | b'\r')
}

fn split_on_whitespace_comma(s: &str, limit: i32) -> Vec<String> {
    let bytes = s.as_bytes();
    let limited = limit > 0;
    let mut parts = Vec::new();
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if limited && parts.len() + 1 >= limit as usize {
            break;
        }
        if bytes[i] == b',' {
            let mut start = i;
            while start > last && is_java_ascii_regex_whitespace(bytes[start - 1]) {
                start -= 1;
            }
            parts.push(s[last..start].to_string());
            i += 1;
            while i < bytes.len() && is_java_ascii_regex_whitespace(bytes[i]) {
                i += 1;
            }
            last = i;
        } else {
            i += 1;
        }
    }
    parts.push(s[last..].to_string());
    if limit == 0 {
        while parts.last().map(|p| p.is_empty()).unwrap_or(false) {
            parts.pop();
        }
        if parts.is_empty() {
            parts.push(String::new());
        }
    }
    parts
}

pub(crate) fn native_string_split_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    limit: i32,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let delim_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let delim = ctx.read_string(delim_obj).unwrap_or_default();

    // Keycloak's model runner uses `String.split("\\s*,\\s*")` to parse
    // comma-separated provider parameters. Keep this common Java shorthand
    // regex on a direct path so it does not depend on the heavier regex bridge
    // during early test-suite bootstrap.
    if delim == r"\s*,\s*" {
        let parts = split_on_whitespace_comma(&s, limit);
        return string_array_from_parts(ctx, &parts);
    }

    let parts: Vec<String> = if delim.is_empty() {
        // Empty delimiter: split each character (like Java regex "")
        s.split("")
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect()
    } else if let Ok(re) = compile_java_regex(&delim, 0) {
        if limit > 0 {
            re.splitn(&s, limit as usize)
        } else {
            re.split(&s)
        }
    } else if limit > 0 {
        s.splitn(limit as usize, delim.as_str())
            .map(|p| p.to_string())
            .collect()
    } else {
        s.split(delim.as_str()).map(|p| p.to_string()).collect()
    };

    // When limit == 0 (default for String.split(regex)), remove trailing empty strings
    let parts: Vec<String> = if limit == 0 {
        let mut v = parts;
        while v.last().map(|s| s.is_empty()).unwrap_or(false) {
            v.pop();
        }
        if v.is_empty() {
            vec![String::new()]
        } else {
            v
        }
    } else {
        parts
    };

    string_array_from_parts(ctx, &parts)
}

pub(crate) fn native_string_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = delimiter, args[1] = CharSequence[]
    let delim = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => String::new(),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string_uninterned(""))))),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let arr_handle = scope.root(arr);
    let len = scope.array_length(scope.get(&arr_handle));
    let mut parts = Vec::with_capacity(len);
    for i in 0..len {
        let arr = scope.get(&arr_handle);
        if let Value::Object(Some(elem)) = scope.get_array_element(arr, i) {
            let mut element_scope = NativeHandleScope::new(&mut *scope);
            let elem_handle = element_scope.root(elem);
            let elem = element_scope.get(&elem_handle);
            // Preserve the String fast path, but use real polymorphic
            // dispatch for StringBuilder, custom CharSequences, and
            // application classes such as Spring Boot's Regex.
            let text = match element_scope.read_string(elem) {
                Some(text) => text,
                None => invoke_to_string(&mut *element_scope, elem)?,
            };
            parts.push(text);
        } else {
            parts.push("null".to_string());
        }
    }
    let joined = parts.join(&delim);
    Ok(Some(Value::Object(Some(
        scope.create_string_uninterned(&joined),
    ))))
}

pub(crate) fn native_string_join_iterable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let delim = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => String::new(),
    };
    let iterable = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string_uninterned(""))))),
    };
    let iterator = match ctx.invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(obj))) => obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string_uninterned(""))))),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let iterator_handle = scope.root(iterator);
    let mut parts = Vec::new();
    for _ in 0..1_000_000 {
        let iterator = scope.get(&iterator_handle);
        let has_next = match scope.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
            Some(Value::Int(v)) => v != 0,
            _ => false,
        };
        if !has_next {
            break;
        }
        let iterator = scope.get(&iterator_handle);
        let elem = scope.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])?;
        let text = match elem {
            Some(Value::Object(Some(obj))) => invoke_to_string(&mut *scope, obj)?,
            Some(Value::Object(None)) | None => "null".to_string(),
            _ => "null".to_string(),
        };
        parts.push(text);
    }
    let joined = parts.join(&delim);
    Ok(Some(Value::Object(Some(
        scope.create_string_uninterned(&joined),
    ))))
}

/// A `null` reference argument to one of the SBR-02 fast-regex natives.
///
/// The JDK reaches its NPE by dereferencing the argument deep inside
/// `Pattern.compile` / `Matcher.appendReplacement`, so its message names an
/// internal field. We cannot reproduce that text without running the bytecode,
/// and the *class* is what control flow depends on, so raise the right class
/// with a message naming the parameter that was null.
///
/// This is not a detail. Until 2026-08-04 these natives returned a null
/// `String` (or `false`) for a null argument, so `s.replaceFirst(null, "x")`
/// produced `null` where HotSpot throws — a wrong value handed to the caller
/// instead of an exception, which is the failure mode a native shadowing
/// bytecode is most likely to have and least likely to have noticed.
fn regex_arg_npe(parameter: &str) -> cratonvm_types::error::RuntimeError {
    cratonvm_types::error::RuntimeError::NullPointerException {
        message: Some(format!("null {parameter} argument")),
    }
}

pub(crate) fn native_string_replace_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // A pattern this engine cannot compile is a `PatternSyntaxException`, not a
    // licence to do something else. This used to fall through to a LITERAL
    // `str::replace` of the pattern TEXT, so `"Hello, World".replaceAll("[",
    // "x")` returned the input unchanged where HotSpot throws — a silently
    // wrong answer produced by the error path of a fast path.
    let re = compile_java_regex(&pattern, 0)?;
    let result = re
        .replace_all_java(&s, replacement.as_str())
        .map_err(crate::regex_matcher::no_group_error)?;
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&result),
    ))))
}

pub(crate) fn native_string_replace_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let re = compile_java_regex(&pattern, 0)?;
    let result = re
        .replace_first_java(&s, replacement.as_str())
        .map_err(crate::regex_matcher::no_group_error)?;
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&result),
    ))))
}

pub(crate) fn native_string_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // Same rule as the two above. The old fallback compared the subject to the
    // pattern TEXT, so `"Hello, World".matches("[")` answered `false` — a
    // plausible verdict for a question that should have raised.
    let re = compile_java_regex(&pattern, 0)?;
    let anchored = format!("^(?:{})$", re.as_str());
    let matched = match crate::compile_anchored_cached(&anchored) {
        Some(full) => full.is_match(&s),
        None => re.is_match(&s),
    };
    Ok(Some(Value::Int(if matched { 1 } else { 0 })))
}

pub(crate) fn native_string_equals_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = ctx.read_string(this).unwrap_or_default();
    let b = ctx.read_string(other).unwrap_or_default();
    Ok(Some(Value::Int(if a.to_lowercase() == b.to_lowercase() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_string_compare_to_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = ctx.read_string(this).unwrap_or_default().to_lowercase();
    let b = ctx.read_string(other).unwrap_or_default().to_lowercase();
    Ok(Some(Value::Int(match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })))
}

pub(crate) fn native_string_last_index_of_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(c)) => *c,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // bug nb-lang-string: lastIndexOf is defined over UTF-16 code UNITS, not
    // Unicode code points. The old `rfind` + `chars().count()` returned a
    // code-point index, off by the count of preceding supplementary chars.
    // Encode both haystack and the target code point to UTF-16 and search the
    // u16 slice (a supplementary `ch` becomes a surrogate pair).
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let needle_units: Vec<u16> = match char::from_u32(ch as u32) {
        Some(c) => {
            let mut buf = [0u16; 2];
            c.encode_utf16(&mut buf).to_vec()
        }
        None => return Ok(Some(Value::Int(-1))),
    };
    let result = last_index_of_units(&s_units, &needle_units);
    Ok(Some(Value::Int(result)))
}

/// Helper: last index (in UTF-16 code units) of `needle` within `haystack`.
/// Returns -1 when not found. An empty needle returns `haystack.len()` to
/// match `String.lastIndexOf("")` semantics used by the str variant.
fn last_index_of_units(haystack: &[u16], needle: &[u16]) -> i32 {
    if needle.is_empty() {
        return haystack.len() as i32;
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let mut i = haystack.len() - needle.len();
    loop {
        if &haystack[i..i + needle.len()] == needle {
            return i as i32;
        }
        if i == 0 {
            return -1;
        }
        i -= 1;
    }
}

pub(crate) fn native_string_last_index_of_str(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let needle = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // bug nb-lang-string: lastIndexOf(String) must return a UTF-16 code-UNIT
    // index. The old `rfind` + `chars().count()` returned a code-point index,
    // off by preceding supplementary chars. Search over u16 code units.
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let needle_units: Vec<u16> = needle.encode_utf16().collect();
    let result = last_index_of_units(&s_units, &needle_units);
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_string_get_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let dst = match args.get(3) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // bug nb-lang-string: getChars indexes over UTF-16 code UNITS, not Unicode
    // code points. The old `s.chars()` (one entry per code point) put
    // supplementary chars in a single slot and shifted every later index,
    // corrupting the dest array. Decode to u16 and copy one code unit per slot.
    let units: Vec<u16> = s.encode_utf16().collect();
    let end = src_end.min(units.len());
    for (i, &cu) in units.iter().enumerate().take(end).skip(src_begin) {
        ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(cu as i32));
    }
    Ok(None)
}

pub(crate) fn native_string_strip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(s.trim()),
    ))))
}

pub(crate) fn native_string_strip_leading(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(s.trim_start()),
    ))))
}

pub(crate) fn native_string_strip_trailing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(s.trim_end()),
    ))))
}

pub(crate) fn native_string_copy_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = char[]
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string_uninterned(""))))),
    };
    let len = ctx.array_length(arr);
    let mut chars = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(c) = ctx.get_array_element(arr, i) {
            if let Some(ch) = char::from_u32(c as u32) {
                chars.push(ch);
            }
        }
    }
    let s: String = chars.into_iter().collect();
    Ok(Some(Value::Object(Some(ctx.create_string_uninterned(&s)))))
}

// ---------------------------------------------------------------------------
// Phase 14 Step 3: String extras
// ---------------------------------------------------------------------------

pub(crate) fn native_string_code_point_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Spec: `String.codePointAt(int index)` raises StringIndexOutOfBoundsException
    // (NOT ArrayIndexOutOfBoundsException) for `index < 0 || index >= length()`.
    // Both flavours subclass IndexOutOfBoundsException, but BouncyCastle and other
    // libraries `catch (StringIndexOutOfBoundsException)` specifically; throwing
    // AIOOBE here would silently bypass that handler and surface as an
    // unrelated runtime error in callers that expect SIOOBE.
    let index_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let chars: Vec<u16> = s.encode_utf16().collect();
    if index_i32 < 0 || (index_i32 as usize) >= chars.len() {
        // Message as well as class: the real `codePointAt` reaches
        // `Preconditions.checkIndex(index, length, SIOOBE_FORMATTER)`.
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(
            index_i32,
            chars.len() as i32,
        )
        .into());
    }
    let index = index_i32 as usize;
    let ch = chars[index];
    // Check for surrogate pair
    if (0xD800..=0xDBFF).contains(&ch) && index + 1 < chars.len() {
        let low = chars[index + 1];
        if (0xDC00..=0xDFFF).contains(&low) {
            let cp = 0x10000 + ((ch as i32 - 0xD800) << 10) + (low as i32 - 0xDC00);
            return Ok(Some(Value::Int(cp)));
        }
    }
    Ok(Some(Value::Int(ch as i32)))
}

pub(crate) fn native_string_code_point_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let begin = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let chars: Vec<u16> = s.encode_utf16().collect();
    let end = end.min(chars.len());
    let mut count = 0;
    let mut i = begin;
    while i < end {
        let ch = chars[i];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < end {
            let low = chars[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                i += 2;
                count += 1;
                continue;
            }
        }
        i += 1;
        count += 1;
    }
    Ok(Some(Value::Int(count)))
}

pub(crate) fn native_string_offset_by_code_points(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };
    let code_point_offset = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let chars: Vec<u16> = s.encode_utf16().collect();
    let mut pos = index;
    if code_point_offset >= 0 {
        for _ in 0..code_point_offset {
            if pos >= chars.len() {
                break;
            }
            let ch = chars[pos];
            if (0xD800..=0xDBFF).contains(&ch) && pos + 1 < chars.len() {
                let low = chars[pos + 1];
                if (0xDC00..=0xDFFF).contains(&low) {
                    pos += 2;
                    continue;
                }
            }
            pos += 1;
        }
    } else {
        for _ in 0..(-code_point_offset) {
            if pos == 0 {
                break;
            }
            pos -= 1;
            if pos > 0
                && (0xDC00..=0xDFFF).contains(&chars[pos])
                && (0xD800..=0xDBFF).contains(&chars[pos - 1])
            {
                pos -= 1;
            }
        }
    }
    Ok(Some(Value::Int(pos as i32)))
}

pub(crate) fn native_string_lines(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // Split on \r\n, \n, or \r
    let mut lines: Vec<&str> = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&s[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 1;
            }
            start = i + 1;
        } else if bytes[i] == b'\n' {
            lines.push(&s[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if start <= bytes.len() {
        let tail = &s[start..];
        if !tail.is_empty() {
            lines.push(tail);
        }
    }
    // Create Stream of strings
    let elements: Vec<Value> = lines
        .iter()
        .map(|line| {
            let str_obj = ctx.create_string_uninterned(line);
            Value::Object(Some(str_obj))
        })
        .collect();
    // Use the Stream pattern from collections
    let stream_class_id = match ctx.ensure_class_initialized("java/util/stream/Stream") {
        Ok(id) => id,
        Err(_) => ctx.ensure_synthetic_class("java/util/stream/Stream", 1),
    };
    let stream = ctx.alloc_object(stream_class_id, 1);
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn native_string_indent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let n = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let mut result = String::new();
    for line in s.lines() {
        if n > 0 {
            for _ in 0..n {
                result.push(' ');
            }
            result.push_str(line);
        } else if n < 0 {
            // Remove up to |n| leading whitespace characters
            let remove = (-n) as usize;
            let spaces = line.len() - line.trim_start().len();
            let skip = remove.min(spaces);
            result.push_str(&line[skip..]);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    let str_obj = ctx.create_string_uninterned(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

pub(crate) fn native_string_transform(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(this))],
    )
}

// --- String.format (basic %s/%d/%f support) ---

pub(crate) fn native_string_format(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static: args[0] = format String, args[1] = Object[] array
    let fmt_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.format: format is null".to_string()),
            }
            .into())
        }
    };
    let fmt_str = ctx.read_string(fmt_obj).unwrap_or_default();

    // Get the varargs array
    let arr_ref = match args.get(1) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };

    let arr_len = arr_ref.map_or(0, |a| ctx.array_length(a));

    // Format string parser supporting flags, width, precision:
    // %[flags][width][.precision]conversion
    let mut result = String::new();
    let chars: Vec<char> = fmt_str.chars().collect();
    let mut i = 0;
    let mut arg_idx = 0;
    // Index used by the most recent conversion, for the `%<` relative-index
    // flag ("reuse the previous argument").
    let mut last_used_index: Option<usize> = None;

    while i < chars.len() {
        if chars[i] == '%' {
            // A '%' that is the final character of the format string is a
            // truncated conversion — real java.util.Formatter throws an
            // UnknownFormatConversionException (an IllegalFormatException,
            // which extends IllegalArgumentException).
            if i + 1 >= chars.len() {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: "Format string ends with a lone '%'".to_string(),
                    }
                    .into(),
                );
            }
            i += 1;
            // Check for %% and %n first
            if chars[i] == '%' {
                result.push('%');
                i += 1;
                continue;
            }
            if chars[i] == 'n' {
                // Formatter's %n conversion emits the platform line separator
                // (System.lineSeparator(), "\r\n" on Windows), not a literal
                // '\n' -- see native_system_line_separator in lang_system.rs
                // for the same platform check.
                result.push_str(if cfg!(windows) { "\r\n" } else { "\n" });
                i += 1;
                continue;
            }

            // Parse the optional argument-index prefix: a run of digits
            // immediately followed by '$' selects an explicit 1-based argument
            // (e.g. `%2$d` → args[1]). Digits NOT followed by '$' are a width,
            // so only consume them here when the '$' is present (look ahead
            // before committing `i`). Without this, `%2$d` parsed the `2` as a
            // width and the `$` as an unknown conversion, so the spec was
            // emitted literally and consumed no argument — e.g. WildFly's
            // `String.format(Locale.ROOT, "subsystem_%2$d_%3$d.xml", …)` came
            // back unformatted and the test resource URL resolved to null.
            let mut explicit_index: Option<usize> = None;
            {
                let mut j = i;
                while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                    j += 1;
                }
                if j > i && chars.get(j) == Some(&'$') {
                    if let Ok(n) = chars[i..j].iter().collect::<String>().parse::<usize>() {
                        if n >= 1 {
                            explicit_index = Some(n - 1);
                        }
                    }
                    i = j + 1; // consume the digits and the '$'
                }
            }

            // Parse optional flags: -, +, 0, ' ', #, (, ',' (grouping), and
            // '<' (relative argument index — reuse the previous conversion's
            // argument).
            // ',' was previously missing, so `%,d` failed to parse and the
            // whole spec was emitted literally ("%,d") — and worse, the arg it
            // should have consumed shifted onto the next conversion.
            // Every chars[i] read below is guarded by `i < chars.len()` via
            // chars.get(i) — a format specifier that runs off the end of the
            // string must throw, not panic.
            let mut flags = String::new();
            while chars.get(i).is_some_and(|c| "-+0 #(,<".contains(*c)) {
                flags.push(chars[i]);
                i += 1;
            }

            // Parse optional width
            let mut width: Option<usize> = None;
            let width_start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            if i > width_start {
                width = chars[width_start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .ok();
            }

            // Parse optional .precision
            let mut precision: Option<usize> = None;
            if chars.get(i) == Some(&'.') {
                i += 1;
                let prec_start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                precision = if i > prec_start {
                    chars[prec_start..i].iter().collect::<String>().parse().ok()
                } else {
                    Some(0)
                };
            }

            // Parse conversion character
            if let Some(&spec) = chars.get(i) {
                i += 1;
                match spec {
                    's' | 'S' | 'd' | 'f' | 'x' | 'X' | 'c' | 'C' | 'b' | 'B' | 'e' | 'E' | 'g'
                    | 'G' | 'o' | 'h' | 'H' | 'a' | 'A' => {
                        // Select the argument this conversion consumes:
                        //   `%<x`  → reuse the previous conversion's index
                        //   `%N$x` → explicit 1-based index N
                        //   `%x`   → next ordinary index (advances the counter)
                        // Only ordinary conversions advance `arg_idx`, matching
                        // java.util.Formatter (explicit/relative specs do not).
                        let use_idx = if flags.contains('<') {
                            last_used_index.unwrap_or(0)
                        } else if let Some(ei) = explicit_index {
                            ei
                        } else {
                            let cur = arg_idx;
                            arg_idx += 1;
                            cur
                        };
                        last_used_index = Some(use_idx);
                        if use_idx < arr_len {
                            if let Some(a) = arr_ref {
                                let elem = ctx.get_array_element(a, use_idx);
                                let text =
                                    format_arg_full(ctx, &elem, spec, &flags, width, precision)?;
                                result.push_str(&text);
                            }
                        }
                    }
                    't' | 'T' => {
                        // Date/time conversion: 't'/'T' is a *prefix*, not a
                        // complete conversion — the next character selects the
                        // actual field (e.g. `%tb` = abbreviated month, `%tY` =
                        // 4-digit year). Real java.util.Formatter upper-cases
                        // the whole result when the prefix itself is 'T'.
                        let uppercase = spec == 'T';
                        if let Some(&field) = chars.get(i) {
                            i += 1;
                            let use_idx = if flags.contains('<') {
                                last_used_index.unwrap_or(0)
                            } else if let Some(ei) = explicit_index {
                                ei
                            } else {
                                let cur = arg_idx;
                                arg_idx += 1;
                                cur
                            };
                            last_used_index = Some(use_idx);
                            if use_idx < arr_len {
                                if let Some(a) = arr_ref {
                                    let elem = ctx.get_array_element(a, use_idx);
                                    let text =
                                        format_temporal_field(ctx, &elem, field, &flags, width)?;
                                    result.push_str(&if uppercase {
                                        text.to_uppercase()
                                    } else {
                                        text
                                    });
                                }
                            }
                        } else {
                            return Err(
                                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                                    message:
                                        "Format string ends with an incomplete date/time conversion"
                                            .to_string(),
                                }
                                .into(),
                            );
                        }
                    }
                    _ => {
                        result.push('%');
                        result.push_str(&flags);
                        if let Some(w) = width {
                            result.push_str(&w.to_string());
                        }
                        if let Some(p) = precision {
                            result.push('.');
                            result.push_str(&p.to_string());
                        }
                        result.push(spec);
                    }
                }
            } else {
                // Reached end of string after consuming flags/width/precision
                // with no conversion character — a truncated specifier.
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: "Format string ends with an incomplete conversion".to_string(),
                    }
                    .into(),
                );
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    let obj = ctx.create_string_uninterned(&result);
    Ok(Some(Value::Object(Some(obj))))
}

/// Epoch-day/calendar math for `%t`/`%T` formatting, self-contained rather
/// than reusing `util_time`'s equivalents: that module sits behind the
/// `synthetic-jdk` feature, but `String.format`/`Formatter` (this file) is a
/// core native available regardless of feature flags.
const TEMPORAL_DAYS_IN_MONTH: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn temporal_is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn temporal_days_in_month(year: i32, month: i32) -> i32 {
    if month == 2 && temporal_is_leap_year(year) {
        29
    } else {
        TEMPORAL_DAYS_IN_MONTH[(month - 1) as usize]
    }
}

fn temporal_day_of_year(year: i32, month: i32, day: i32) -> i32 {
    let mut doy = day;
    for m in 1..month {
        doy += temporal_days_in_month(year, m);
    }
    doy
}

fn temporal_to_epoch_day(year: i32, month: i32, day: i32) -> i64 {
    let y = year as i64;
    let mut total: i64 = 365 * y + y / 4 - y / 100 + y / 400;
    for m in 1..month {
        total += temporal_days_in_month(year, m) as i64;
    }
    total += day as i64;
    total - 719_528 // days from year 0 to 1970-01-01
}

fn temporal_from_epoch_day(epoch_day: i64) -> (i32, i32, i32) {
    let abs_day = epoch_day + 719_528;
    let mut y = ((abs_day * 400) / 146_097) as i32;
    loop {
        let year_start = 365 * y as i64 + y as i64 / 4 - y as i64 / 100 + y as i64 / 400;
        if year_start >= abs_day {
            y -= 1;
        } else {
            let next_start = 365 * (y + 1) as i64 + (y + 1) as i64 / 4 - (y + 1) as i64 / 100
                + (y + 1) as i64 / 400;
            if next_start < abs_day {
                y += 1;
            } else {
                break;
            }
        }
    }
    let year_start = 365 * y as i64 + y as i64 / 4 - y as i64 / 100 + y as i64 / 400;
    let mut remaining = (abs_day - year_start) as i32;
    let mut month = 1;
    loop {
        let dim = temporal_days_in_month(y, month);
        if remaining <= dim {
            break;
        }
        remaining -= dim;
        month += 1;
    }
    (y, month, remaining)
}

/// Extract the wall-clock `(year, month[1-12], day, hour[0-23], minute,
/// second, nanosecond)` fields a `%t`/`%T` conversion needs from a
/// `Date`/`Calendar`/`Long`/`TemporalAccessor` argument.
///
/// CratonVM's Calendar/Date model (see `cal_to_epoch_millis`/
/// `cal_from_epoch_millis` in `phases_early.rs`) treats epoch millis as raw
/// wall-clock fields with no timezone shift applied anywhere in the VM — so
/// the same zero-offset breakdown is used here for `Date`/`Long`/`Calendar`
/// to stay consistent with the rest of the VM (and with how those values
/// were constructed in the first place, e.g. `Timestamp.valueOf`).
fn extract_temporal_fields(
    ctx: &mut dyn NativeContext,
    val: &Value,
) -> Result<(i64, i32, i32, i32, i32, i32, i32), MethodCallFailed> {
    fn millis_to_fields(millis: i64) -> (i64, i32, i32, i32, i32, i32, i32) {
        let day_millis = 86_400_000i64;
        let epoch_day = millis.div_euclid(day_millis);
        let mut tod = millis.rem_euclid(day_millis);
        let hour = (tod / 3_600_000) as i32;
        tod %= 3_600_000;
        let minute = (tod / 60_000) as i32;
        tod %= 60_000;
        let second = (tod / 1000) as i32;
        let ms = (tod % 1000) as i32;
        let (y, m, d) = temporal_from_epoch_day(epoch_day);
        (y as i64, m, d, hour, minute, second, ms * 1_000_000)
    }

    fn invoke_i32(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef, name: &str) -> i32 {
        match ctx.invoke_virtual(obj, name, "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            _ => 0,
        }
    }

    match val {
        Value::Long(ms) => Ok(millis_to_fields(*ms)),
        Value::Object(Some(obj)) => {
            let obj = *obj;
            let cid = ctx.class_id_of_object(obj);
            let is_a = |ctx: &dyn NativeContext, name: &str| {
                ctx.class_id_by_name(name)
                    .is_some_and(|parent| ctx.is_subclass(cid, parent))
            };
            // A `long` argument (e.g. `String.format("%tY", date.getTime())`)
            // arrives here already autoboxed into the varargs Object[] array.
            if is_a(ctx, "java/lang/Long") {
                let millis = match ctx.get_field(obj, 0) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                Ok(millis_to_fields(millis))
            } else if is_a(ctx, "java/util/Date") {
                let millis = match ctx.invoke_virtual(obj, "getTime", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                Ok(millis_to_fields(millis))
            } else if is_a(ctx, "java/util/Calendar") {
                let millis = match ctx.invoke_virtual(obj, "getTimeInMillis", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                Ok(millis_to_fields(millis))
            } else if is_a(ctx, "java/time/Instant") {
                let sec = match ctx.invoke_virtual(obj, "getEpochSecond", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                let nano = match ctx.invoke_virtual(obj, "getNano", "()I", &[])? {
                    Some(Value::Int(v)) => v,
                    _ => 0,
                };
                let (y, m, d, h, mi, s, _) = millis_to_fields(sec.saturating_mul(1000));
                Ok((y, m, d, h, mi, s, nano))
            } else if is_a(ctx, "java/time/LocalDate") {
                let year = invoke_i32(ctx, obj, "getYear") as i64;
                let month = invoke_i32(ctx, obj, "getMonthValue");
                let day = invoke_i32(ctx, obj, "getDayOfMonth");
                Ok((year, month, day, 0, 0, 0, 0))
            } else if is_a(ctx, "java/time/LocalTime") {
                let hour = invoke_i32(ctx, obj, "getHour");
                let minute = invoke_i32(ctx, obj, "getMinute");
                let second = invoke_i32(ctx, obj, "getSecond");
                let nano = invoke_i32(ctx, obj, "getNano");
                Ok((1970, 1, 1, hour, minute, second, nano))
            } else if is_a(ctx, "java/time/LocalDateTime")
                || is_a(ctx, "java/time/ZonedDateTime")
                || is_a(ctx, "java/time/OffsetDateTime")
            {
                let year = invoke_i32(ctx, obj, "getYear") as i64;
                let month = invoke_i32(ctx, obj, "getMonthValue");
                let day = invoke_i32(ctx, obj, "getDayOfMonth");
                let hour = invoke_i32(ctx, obj, "getHour");
                let minute = invoke_i32(ctx, obj, "getMinute");
                let second = invoke_i32(ctx, obj, "getSecond");
                let nano = invoke_i32(ctx, obj, "getNano");
                Ok((year, month, day, hour, minute, second, nano))
            } else {
                Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!(
                            "{} cannot be formatted as a date",
                            ctx.class_name_of_id(cid).unwrap_or_default()
                        ),
                    }
                    .into(),
                )
            }
        }
        _ => Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Illegal date/time conversion argument".to_string(),
            }
            .into(),
        ),
    }
}

/// Zeller/Sakamoto-style day-of-week for the `%tA`/`%ta` name tables below.
/// Returns 0=Sunday .. 6=Saturday (matching `DAYS_ABBR`/`DAYS_FULL` order) —
/// the same algorithm as the `'E'` pattern letter in `dtf_apply_pattern`
/// (`util_time.rs`), duplicated here in its 0=Sunday form rather than reused
/// since that copy is private and returns the opposite (Java `DayOfWeek`,
/// 1=Monday) convention.
fn day_of_week_sun0(year: i32, month: i32, day: i32) -> usize {
    let (mut y, m, d) = (year, month, day);
    let t = [0i32, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 {
        y -= 1;
    }
    (((y + y / 4 - y / 100 + y / 400 + t[(m - 1).max(0) as usize] + d) % 7) as usize) % 7
}

/// Format a single `%t`/`%T` date/time field conversion (the character
/// following the `t`/`T` prefix — e.g. `b` in `%tb`). See
/// [`extract_temporal_fields`] for how the source value is decoded and
/// `java.util.Formatter`'s own `%t` conversion table for the field meanings.
fn format_temporal_field(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
    flags: &str,
    width: Option<usize>,
) -> Result<String, MethodCallFailed> {
    const MONTHS_ABBR: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const MONTHS_FULL: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const DAYS_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const DAYS_FULL: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];

    let (year, month, day, hour, minute, second, nanos) = extract_temporal_fields(ctx, val)?;
    let year32 = year as i32;
    let millis = nanos / 1_000_000;
    let dow = day_of_week_sun0(year32, month, day);
    let month_idx = (month.clamp(1, 12) - 1) as usize;
    let hour12 = {
        let h = hour % 12;
        if h == 0 {
            12
        } else {
            h
        }
    };
    let ampm = if hour < 12 { "am" } else { "pm" };
    let epoch_sec = || {
        temporal_to_epoch_day(year32, month, day) * 86_400
            + hour as i64 * 3600
            + minute as i64 * 60
            + second as i64
    };

    let mut out = match field {
        // Time
        'H' => format!("{:02}", hour),
        'k' => format!("{}", hour),
        'I' => format!("{:02}", hour12),
        'l' => format!("{}", hour12),
        'M' => format!("{:02}", minute),
        'S' => format!("{:02}", second),
        'L' => format!("{:03}", millis),
        'N' => format!("{:09}", nanos),
        'p' => ampm.to_string(),
        // CratonVM's Calendar/Date model has no real timezone offset (see
        // extract_temporal_fields) — 'z'/'Z' report the fixed UTC identity
        // consistent with that.
        'z' => "+0000".to_string(),
        'Z' => "UTC".to_string(),
        's' => format!("{}", epoch_sec()),
        'Q' => format!("{}", epoch_sec() * 1000 + millis as i64),
        // Date
        'B' => MONTHS_FULL[month_idx].to_string(),
        'b' | 'h' => MONTHS_ABBR[month_idx].to_string(),
        'A' => DAYS_FULL[dow].to_string(),
        'a' => DAYS_ABBR[dow].to_string(),
        'C' => format!("{:02}", year.div_euclid(100)),
        'Y' => format!("{:04}", year),
        'y' => format!("{:02}", year.rem_euclid(100)),
        'j' => format!("{:03}", temporal_day_of_year(year32, month, day)),
        'm' => format!("{:02}", month),
        'd' => format!("{:02}", day),
        'e' => format!("{}", day),
        // Composites
        'R' => format!("{:02}:{:02}", hour, minute),
        'T' => format!("{:02}:{:02}:{:02}", hour, minute, second),
        'r' => format!(
            "{:02}:{:02}:{:02} {}",
            hour12,
            minute,
            second,
            ampm.to_uppercase()
        ),
        'D' => format!("{:02}/{:02}/{:02}", month, day, year.rem_euclid(100)),
        'F' => format!("{:04}-{:02}-{:02}", year, month, day),
        'c' => format!(
            "{} {} {:2} {:02}:{:02}:{:02} UTC {:04}",
            DAYS_ABBR[dow], MONTHS_ABBR[month_idx], day, hour, minute, second, year
        ),
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("Unknown date/time conversion '%t{}'", field),
                }
                .into(),
            );
        }
    };

    if let Some(w) = width {
        if out.len() < w {
            let pad = w - out.len();
            if flags.contains('-') {
                out = format!("{out}{}", " ".repeat(pad));
            } else {
                out = format!("{}{out}", " ".repeat(pad));
            }
        }
    }

    Ok(out)
}

/// Format a single argument with flags, width, and precision support.
#[allow(clippy::too_many_arguments)]
pub(crate) fn format_arg_full(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
) -> Result<String, MethodCallFailed> {
    // Uppercase string-family conversions ('S'/'B'/'C') format identically to
    // their lowercase form, then the whole result is upper-cased — per
    // java.util.Formatter's "If the conversion is 'S', 'B' or 'C' … the result is
    // converted to upper case". (The numeric uppercase conversions 'X'/'E'/'G'/'A'
    // already emit upper-case digits in `format_arg`, so they are NOT remapped
    // here.) Format with the lowercase spec, then upper-case at the very end.
    // Without 'S' support, Groovy's `"COMMERCIAL_%SREPO_URL".formatted(id)` left
    // the specifier literal, so the env-var key never matched (SpringRepos).
    let (spec, uppercase_result) = match spec {
        'S' => ('s', true),
        'B' => ('b', true),
        'C' => ('c', true),
        other => (other, false),
    };

    // Get the raw formatted value first
    let raw = format_arg(ctx, val, spec)?;

    // Apply precision for %f/%e/%g — override default
    let raw = match spec {
        'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' if precision.is_some() => {
            let prec = precision.unwrap();
            let fval = extract_float_value(ctx, val);
            match spec {
                'f' => format!("{:.prec$}", fval),
                'e' => format!("{:.prec$e}", fval),
                'E' => format!("{:.prec$E}", fval),
                _ => format!("{:.prec$}", fval),
            }
        }
        's' if precision.is_some() => {
            let prec = precision.unwrap();
            if raw.len() > prec {
                raw[..prec].to_string()
            } else {
                raw
            }
        }
        _ => raw,
    };

    // Apply width and flags
    let left_justify = flags.contains('-');
    let zero_pad = flags.contains('0') && !left_justify;
    let plus_sign = flags.contains('+');

    let mut formatted = raw;

    // ',' grouping flag: insert a thousands separator into the integer part of
    // %d / %f values (Java's Formatter; the locale separator is ',' for the
    // root/US locale, which is what CratonVM formats against).
    if flags.contains(',') && matches!(spec, 'd' | 'f' | 'g' | 'G') {
        formatted = group_thousands(&formatted);
    }

    // Add sign for numeric types
    if plus_sign && matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') && !formatted.starts_with('-')
    {
        formatted = format!("+{formatted}");
    }

    // Apply width padding
    if let Some(w) = width {
        if formatted.len() < w {
            let pad = w - formatted.len();
            if left_justify {
                formatted = format!("{formatted}{}", " ".repeat(pad));
            } else if zero_pad && matches!(spec, 'd' | 'f' | 'e' | 'E' | 'x' | 'X' | 'o') {
                if formatted.starts_with('-') || formatted.starts_with('+') {
                    let (sign, rest) = formatted.split_at(1);
                    formatted = format!("{sign}{}{rest}", "0".repeat(pad));
                } else {
                    formatted = format!("{}{formatted}", "0".repeat(pad));
                }
            } else {
                formatted = format!("{}{formatted}", " ".repeat(pad));
            }
        }
    }

    if uppercase_result {
        formatted = formatted.to_uppercase();
    }
    Ok(formatted)
}

/// Insert ',' thousands separators into the integer part of a numeric string
/// (for the `%,d` / `%,f` grouping flag). Preserves a leading sign and any
/// fractional part (`.xxx`).
fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac_part) = match rest.find('.') {
        Some(p) => (&rest[..p], &rest[p..]),
        None => (rest, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit()) || int_part.len() <= 3 {
        return s.to_string();
    }
    let digits: Vec<char> = int_part.chars().collect();
    let len = digits.len();
    let mut grouped = String::with_capacity(len + len / 3);
    for (idx, ch) in digits.iter().enumerate() {
        // Comma before this digit when the number of digits remaining (incl.
        // this one) is a positive multiple of 3, and it's not the leading digit.
        if idx != 0 && (len - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*ch);
    }
    format!("{sign}{grouped}{frac_part}")
}

/// Extract a float value from a Value (unboxing wrappers as needed).
fn extract_float_value(ctx: &dyn NativeContext, val: &Value) -> f64 {
    match val {
        Value::Float(v) => *v as f64,
        Value::Double(v) => *v,
        Value::Int(v) => *v as f64,
        Value::Long(v) => *v as f64,
        Value::Object(Some(obj)) => match ctx.get_field(*obj, 0) {
            Value::Float(v) => v as f64,
            Value::Double(v) => v,
            Value::Int(v) => v as f64,
            Value::Long(v) => v as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// Format a single argument for String.format.
pub(crate) fn format_arg(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
) -> Result<String, MethodCallFailed> {
    // Helper: unbox wrapper object to primitive. Arrays are NEVER wrappers —
    // num_slots is the array LENGTH and get_field(0) on packed primitive
    // arrays reads garbage (String.format("%s", int[]) printed a bogus
    // number instead of "[I@hash").
    fn unbox_obj(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> Value {
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
            return Value::Object(Some(obj));
        }
        // ONLY unbox genuine primitive-wrapper classes. Other objects have an
        // int/long at slot 0 too — notably java.math.BigInteger (slot 0 =
        // `signum`), so blindly reading slot 0 made String.format("%x"/"%d"/"%s",
        // bigInteger) format the signum (1) instead of the magnitude. Gate on the
        // wrapper class name so non-wrappers fall through to toString()/radix.
        let cname = ctx
            .class_name_of_id(ctx.class_id_of_object(obj))
            .unwrap_or_default();
        let is_wrapper = matches!(
            cname.as_str(),
            "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Character"
                | "java/lang/Boolean"
                | "java/lang/Float"
                | "java/lang/Double"
        );
        if is_wrapper {
            let nf = ctx.object_num_fields(obj);
            if nf >= 1 {
                let f = ctx.get_field(obj, 0);
                match f {
                    Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                        return f
                    }
                    _ => {}
                }
            }
        }
        Value::Object(Some(obj))
    }

    let formatted = match val {
        Value::Object(None) => match spec {
            'b' => "false".to_string(),
            _ => "null".to_string(),
        },
        Value::Object(Some(obj)) => {
            // For %b: check if it's a Boolean wrapper, else non-null = true
            if spec == 'b' {
                let inner = unbox_obj(ctx, *obj);
                return Ok(match inner {
                    Value::Int(v) => if v != 0 { "true" } else { "false" }.to_string(),
                    _ => "true".to_string(),
                });
            }
            // For %s: real OpenJDK does `String.valueOf(arg)` == `arg.toString()`.
            // A String formats as its characters; a boxed primitive wrapper
            // formats as its value; any other object (enum, record, bean, …)
            // formats via its `toString()`. The previous code only handled
            // String + wrappers and returned "null" for everything else, so e.g.
            // `String.format("%s", Color.GREEN)` printed "null".
            if spec == 's' {
                // String.valueOf(arg) == arg.toString(). Fast-path a *real* String
                // (read its chars directly); every other object — boxed wrappers
                // (incl. Boolean), enums, records, beans — goes through toString().
                // The old code called read_string FIRST on any object, which
                // mis-read a Boolean's value slot and yielded "1"/"0" instead of
                // "true"/"false" (and likewise for other non-String objects whose
                // slot-0 happens to read as text).
                let is_string =
                    ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(*obj));
                if is_string {
                    if let Some(s) = ctx.read_string(*obj) {
                        return Ok(s);
                    }
                }
                return match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => {
                        Ok(ctx.read_string(s).unwrap_or_else(|| "null".to_string()))
                    }
                    Ok(_) => Ok("null".to_string()),
                    Err(err) => Err(err),
                };
            }
            // %h / %H: hashcode hex (left as-is — String fast path or "null").
            if spec == 'h' || spec == 'H' {
                return Ok(ctx.read_string(*obj).unwrap_or_else(|| "null".to_string()));
            }
            // BigInteger numeric conversions: its slot-0 field is `signum`, not the
            // value, so it must NOT be unboxed. Java's Formatter formats a
            // BigInteger via its real radix toString (e.g. %x => toString(16));
            // BigInteger.toString(radix) works correctly on CratonVM.
            // (new BigInteger(1, sha256).%064x -> Spring Boot buildpack LayerId.)
            if matches!(spec, 'd' | 'x' | 'X' | 'o') {
                let cname = ctx
                    .class_name_of_id(ctx.class_id_of_object(*obj))
                    .unwrap_or_default();
                if cname == "java/math/BigInteger" {
                    let radix = match spec {
                        'o' => 8,
                        'd' => 10,
                        _ => 16,
                    };
                    match ctx.invoke_virtual(
                        *obj,
                        "toString",
                        "(I)Ljava/lang/String;",
                        &[Value::Int(radix)],
                    ) {
                        Ok(Some(Value::Object(Some(s)))) => {
                            let str = ctx.read_string(s).unwrap_or_default();
                            return Ok(if spec == 'X' { str.to_uppercase() } else { str });
                        }
                        Ok(_) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
            // Try to unbox wrapper to primitive and recurse
            let inner = unbox_obj(ctx, *obj);
            match inner {
                Value::Object(_) => {
                    // Not a wrapper — fallback to string
                    ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
                }
                Value::Int(v) if matches!(spec, 'x' | 'X' | 'o') => {
                    // Java's Formatter masks a Byte argument to 8 bits and a
                    // Short to 16 bits for the unsigned conversions (%x/%X/%o);
                    // an Integer keeps all 32. After unbox_obj all three collapse
                    // to Value::Int, so a negative Byte would sign-extend
                    // (String.format("%02x", (byte)-54) → "ffffffca" instead of
                    // "ca"). Recover the width from the wrapper class.
                    let cname = ctx
                        .class_name_of_id(ctx.class_id_of_object(*obj))
                        .unwrap_or_default();
                    let masked = match cname.as_str() {
                        "java/lang/Byte" => v & 0xFF,
                        "java/lang/Short" => v & 0xFFFF,
                        _ => v,
                    };
                    return format_arg(ctx, &Value::Int(masked), spec);
                }
                _ => return format_arg(ctx, &inner, spec),
            }
        }
        Value::Int(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u32),
            'X' => format!("{:X}", *v as u32),
            'o' => format!("{:o}", *v as u32),
            'c' => char::from_u32(*v as u32).unwrap_or('?').to_string(),
            'b' => ((*v) != 0).to_string(),
            'f' => format!("{:.6}", *v as f64),
            'e' => format!("{:e}", *v as f64),
            'E' => format!("{:E}", *v as f64),
            _ => v.to_string(),
        },
        Value::Long(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u64),
            'X' => format!("{:X}", *v as u64),
            'o' => format!("{:o}", *v as u64),
            'f' => format!("{:.6}", *v as f64),
            'e' => format!("{:e}", *v as f64),
            'E' => format!("{:E}", *v as f64),
            _ => v.to_string(),
        },
        Value::Float(v) => match spec {
            'f' => format!("{:.6}", v),
            'e' => format!("{:e}", *v as f64),
            'E' => format!("{:E}", *v as f64),
            // `%s`/no-spec of a float -> Java Double.toString form, not raw `{}`.
            _ => format_float(*v),
        },
        Value::Double(v) => match spec {
            'f' => format!("{:.6}", v),
            'e' => format!("{:e}", v),
            'E' => format!("{:E}", v),
            // `%s`/no-spec of a double -> Java Double.toString form, not raw `{}`.
            _ => format_double(*v),
        },
        _ => "?".to_string(),
    };
    Ok(formatted)
}

// ---------------------------------------------------------------------------
// String modern methods (Java 11+)
// ---------------------------------------------------------------------------

/// repeat(int) — "ab".repeat(3) → "ababab"
pub(crate) fn native_string_repeat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match args.get(1) {
        Some(Value::Int(n)) => std::cmp::max(0, *n) as usize,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let result = s.repeat(count);
    let str_obj = ctx.create_string_uninterned(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// isBlank() — true if empty or all whitespace
pub(crate) fn native_string_is_blank(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Int(if s.trim().is_empty() { 1 } else { 0 })))
}

/// chars() — returns IntStream of char values
///
/// FMT-REGRESSION-ANONOBJ: previously allocated the result via the raw
/// `ctx.alloc_object(ClassId::new(0), 1)` fallback instead of stamping it
/// with the `java/util/stream/IntStream` interface name like every other
/// synthetic IntStream factory (`make_int_stream`, `native_int_stream_range`,
/// etc. in native-collections) does. `alloc_object`'s ClassId(0)
/// defense-in-depth (vm_exec.rs, added to fix a WildFly GC-corruption bug)
/// now redirects any ClassId(0)+num_fields>0 allocation to a shared
/// `cratonvm/synthetic/AnonymousObject$N` class instead of leaving it with a
/// broken/undersized layout -- correct for that fix's own purpose, but it
/// meant this stream's class name was never `java/util/stream/IntStream`, so
/// `forEach`/`allMatch`/etc. (registered in native-collections keyed on that
/// exact class name) resolved to `NoSuchMethodError` on
/// `AnonymousObject$1.forEach(IntConsumer)` /
/// `AnonymousObject$1.allMatch(IntPredicate)` for any caller of
/// `String.chars()`/`codePoints()` (e.g. Spring's
/// `BasicJsonWriter.write` -> `input.chars().forEach(...)`, used by both
/// `BasicJsonWriterTests` and `RuntimeHintsWriterTests` via
/// `RuntimeHintsWriter.write`).
///
/// Fix: allocate via `alloc_concurrent_synthetic` stamped with the
/// `java/util/stream/IntStream` interface name and the standard 2-field
/// synthetic-stream layout (field 0 = elements, field 1 = close handlers --
/// see `STREAM_FIELD_ELEMENTS`/`STREAM_FIELD_CLOSE_HANDLERS` in
/// native-collections/src/lib.rs), matching every other IntStream factory.
pub(crate) fn native_string_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let char_values: Vec<Value> = s.encode_utf16().map(|c| Value::Int(c as i32)).collect();

    // 2-field synthetic stream layout: field 0 = elements array, field 1 =
    // close handlers (None -- chars()/codePoints() never register any).
    // Must be 2 fields (not 1) to match STREAM_NUM_FIELDS in
    // native-collections/src/lib.rs -- the shared stream layout every other
    // IntStream factory uses, which reserves field 1 for BaseStream.onClose
    // handlers; a 1-field object would make any downstream onClose()
    // registration on one of these streams an out-of-bounds field write.
    // (A concurrent fix independently found this same root cause -- e.g. it
    // also breaks `StringUtils.containsWhitespace` -> `"...".chars().anyMatch(...)`
    // -- via a 1-field allocation; reconciled to the 2-field layout here.)
    let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 2);
    // Must be a primitive `int[]`, not a reference array: this stream's
    // consumers (`IntStream.forEach`/`toArray`/etc.) read field 0 as an
    // int-element array. A `new_ref_array` allocation stored `Value::Int`s
    // into Object-shaped slots, which silently read back as 0 -- every
    // `"...".chars()` consumer therefore saw a correctly-SIZED but all-zero
    // stream (confirmed via a standalone `chars().toArray()` repro).
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, char_values.len());
    for (i, val) in char_values.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.set_field(stream, 1, Value::Object(None));
    Ok(Some(Value::Object(Some(stream))))
}

/// regionMatches(boolean ignoreCase, int toffset, String other, int ooffset, int len)
pub(crate) fn native_string_region_matches_ic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ignore_case = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let toffset_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let other = match args.get(3) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let ooffset_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if toffset_i < 0 || ooffset_i < 0 || len_i < 0 {
        return Ok(Some(Value::Int(0)));
    }
    let toffset = toffset_i as usize;
    let ooffset = ooffset_i as usize;
    let len = len_i as usize;

    let s = ctx.read_string(this).unwrap_or_default();
    // bug nb-lang-string: regionMatches offsets/len are in UTF-16 code UNITS,
    // not Unicode code points. Index over u16 to agree with charAt/length.
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let o_units: Vec<u16> = other.encode_utf16().collect();

    if toffset
        .checked_add(len)
        .map_or(true, |end| end > s_units.len())
        || ooffset
            .checked_add(len)
            .map_or(true, |end| end > o_units.len())
    {
        return Ok(Some(Value::Int(0)));
    }

    for i in 0..len {
        let su = s_units[toffset + i];
        let ou = o_units[ooffset + i];
        let eq = if ignore_case {
            code_unit_eq_ignore_case(su, ou)
        } else {
            su == ou
        };
        if !eq {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// Case-insensitive comparison of two UTF-16 code units, mirroring
/// `String.regionMatches(true, ...)`: equal directly, or after folding both to
/// upper-case, or (per the JDK) to lower-case. Surrogate code units (which are
/// not assignable to a `char` scalar) only compare equal when bit-identical.
fn code_unit_eq_ignore_case(a: u16, b: u16) -> bool {
    if a == b {
        return true;
    }
    match (char::from_u32(a as u32), char::from_u32(b as u32)) {
        (Some(ca), Some(cb)) => {
            ca.to_uppercase().eq(cb.to_uppercase()) || ca.to_lowercase().eq(cb.to_lowercase())
        }
        _ => false,
    }
}

/// regionMatches(int toffset, String other, int ooffset, int len) — case-sensitive
pub(crate) fn native_string_region_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let toffset_i = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let other = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let ooffset_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if toffset_i < 0 || ooffset_i < 0 || len_i < 0 {
        return Ok(Some(Value::Int(0)));
    }
    let toffset = toffset_i as usize;
    let ooffset = ooffset_i as usize;
    let len = len_i as usize;

    let s = ctx.read_string(this).unwrap_or_default();
    // bug nb-lang-string: regionMatches offsets/len are in UTF-16 code UNITS,
    // not Unicode code points. Index over u16 to agree with charAt/length.
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let o_units: Vec<u16> = other.encode_utf16().collect();

    if toffset
        .checked_add(len)
        .map_or(true, |end| end > s_units.len())
        || ooffset
            .checked_add(len)
            .map_or(true, |end| end > o_units.len())
    {
        return Ok(Some(Value::Int(0)));
    }

    for i in 0..len {
        if s_units[toffset + i] != o_units[ooffset + i] {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// formatted(Object[]) — instance method: this.formatted(args) → String.format(this, args)
/// String.format(Locale, String, Object...) — static method with Locale (ignored for now).
/// args[0] = Locale, args[1] = format String, args[2] = Object[]
pub(crate) fn native_string_format_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Skip the Locale argument (args[0]) and delegate to the main format impl
    let format_args = [
        args.get(1).cloned().unwrap_or(Value::Object(None)),
        args.get(2).cloned().unwrap_or(Value::Object(None)),
    ];
    native_string_format(ctx, &format_args)
}

pub(crate) fn native_string_formatted(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (the format string), args[1] = Object[]
    let format_args = [
        args.first().cloned().unwrap_or(Value::Object(None)),
        args.get(1).cloned().unwrap_or(Value::Object(None)),
    ];
    native_string_format(ctx, &format_args)
}

// ---------------------------------------------------------------------------
// java.lang.StringUTF16 — static helpers used during <clinit>
// ---------------------------------------------------------------------------
//
// `isBigEndian()Z` is called from `StringUTF16.<clinit>` on JDK 25 to decide
// which byte order the compact-string `byte[]` uses for its `char` pairs —
// it seeds the `HI_BYTE_SHIFT` / `LO_BYTE_SHIFT` constants that `putChar` /
// `getChar` / `compress` / `toBytes` use throughout `StringUTF16`. In the real
// JDK this value is `UNSAFE.isBigEndian()`, i.e. the *host* CPU byte order.
//
// This MUST agree with the byte order CratonVM's own Rust string code uses.
// CratonVM stores every compact UTF-16 string `byte[]` **little-endian** (low
// byte first) regardless of host architecture — see `create_java_string` in
// `vm/src/vm/vm_object.rs` (`lo = unit & 0xFF` written at `i*2`, `hi =
// unit >> 8` at `i*2+1`), and every native String accessor in this file
// decodes the matching `(hi << 8) | lo` from those same little-endian slots
// (see `decode_string_value` above: `lo` at `c*2`, `hi` at `c*2+1`).
// Constant-pool strings, `String.substring`, `String.concat`, interning, etc.
// all go through that little-endian Rust path. On the x86-64 target this VM
// runs on, that also matches the real host endianness.
//
// FIX: this previously returned `Int(1)` (big-endian), justified by a doc
// comment that claimed `create_java_string` wrote big-endian (`hi` at `i*2`).
// That claim was stale/incorrect: the VM has always written little-endian
// (low byte at the even index). Returning `true` made the *real-JDK*
// `StringUTF16` bytecode (e.g. the `String(char[])` constructor, which has no
// native override) build a **big-endian** `byte[]`, while CratonVM's native
// `charAt` / `toCharArray` / `hashCode` / `equals` read it little-endian — so
// the two halves of the VM disagreed on every non-Latin-1 string (byte-swapped
// `char` values). Returning `false` (`Int(0)`) makes the JDK bytecode use the
// same little-endian layout as CratonVM's Rust code, keeping every code path
// consistent. `HI_BYTE_SHIFT == 8` in OpenJDK <=> big-endian; here it is 0.
//
// # On JDK 25 this registration never fires, and that is not a defect
//
// `--dump-native-registry` against Temurin 25.0.3, checked while closing the
// `String.hashCode` record because it asked whether `HI_BYTE_SHIFT` /
// `LO_BYTE_SHIFT` are populated at all:
//
//   java/lang/StringUTF16.isBigEndian()Z   loaded: true  declared: FALSE
//                                          has_code: false  invocations: 0
//
// `declared: false` — the method does not exist on JDK 25's `StringUTF16`. Its
// `<clinit>` reads the byte order from `UNSAFE`, which this VM answers through
// `jdk/internal/misc/UnsafeConstants.BIG_ENDIAN` (the boot log's
// "UnsafeConstants populated (5/5)"). The statics are populated and
// little-endian: `probes/StringUtf16ClassShapeProbe` reads them back as
// `HI_BYTE_SHIFT=0` / `LO_BYTE_SHIFT=8`, identical to HotSpot.
//
// So this stays registered for images that DO declare the method (JDK 17/21),
// where it must give the same answer `UnsafeConstants` gives, which it does. A
// census row reading `has_code: false` here means "absent from this image", not
// "an unimplemented native something is waiting on" — the distinction cost a
// paragraph of doubt in the record that filed the UTF-16 hash defect.
//
// Arity is guaranteed by the verifier (`()Z`); we ignore any extra args
// defensively and return the constant unconditionally.
pub(crate) fn native_string_utf16_is_big_endian(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // FIX: little-endian to match create_java_string + this file's decoders
    // (was Int(1)). Matches the x86-64 host byte order.
    Ok(Some(Value::Int(0)))
}

/// `java.lang.StringUTF16.getChars(byte[] value, int srcBegin, int srcEnd,
/// char[] dst, int dstBegin)`.
///
/// OpenJDK implements this as a per-code-unit bytecode loop. Hibernate's
/// metamodel bootstrap calls it heavily while constructing annotations and
/// generated proxies, leaving a cold one-shot transaction entirely in the
/// interpreter. Bulk-read the compact UTF-16 source and decode it in Rust,
/// while retaining the JDK's deliberately asymmetric bounds behavior: when
/// `srcBegin >= srcEnd` it performs no source or destination dereference.
pub(crate) fn native_string_utf16_get_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if src_begin >= src_end {
        return Ok(None);
    }
    let value = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    // `isub` in the JDK bytecode has wrapping `int` arithmetic.  Keep that
    // behavior before the bounds check so extreme malformed ranges report a
    // StringIndexOutOfBoundsException rather than overflowing Rust arithmetic.
    let count = src_end.wrapping_sub(src_begin);
    let source_len = (ctx.array_length(value) / 2) as i32;
    if bounds_off_count_violation(src_begin, count, source_len).is_some() {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range_size(src_begin, count, source_len).into());
    }
    // The bytecode validates the source before its first destination access.
    // Preserve that ordering when both inputs are invalid/null.
    let dst = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = count as usize;
    let dst_len = ctx.array_length(dst) as i64;
    let copy_end = i64::from(dst_begin) + count as i64;
    if dst_begin < 0 || copy_end > dst_len {
        let index = if dst_begin < 0 {
            dst_begin
        } else {
            (copy_end - 1).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        return Err(
            cratonvm_types::error::RuntimeError::aioobe_no_length(index).into(),
        );
    }
    let mut bytes = vec![0u8; count * 2];
    if ctx.read_byte_array_into(value, src_begin as usize * 2, &mut bytes) != bytes.len() {
        return Err(
            cratonvm_types::error::RuntimeError::aioobe_no_length(src_begin)
            .into(),
        );
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from(pair[0]) | (u16::from(pair[1]) << 8))
        .collect();
    // The VM implementation performs one checked memcpy into the compact
    // char-array payload. Keep the element fallback for test/mock contexts
    // and unusual heaps that do not expose the bulk hook.
    if !ctx.write_char_array_from(dst, dst_begin as usize, &units) {
        for (index, unit) in units.into_iter().enumerate() {
            ctx.set_array_element(dst, dst_begin as usize + index, Value::Int(unit as i32));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// String bounds-check helpers
// ---------------------------------------------------------------------------
//
// In OpenJDK 25, `String`'s two package-private bounds helpers delegate to
// `Preconditions`, handing it the `SIOOBE_FORMATTER` that makes the thrown
// class a `StringIndexOutOfBoundsException`:
//
//   static void checkBoundsBeginEnd(int begin, int end, int length) {
//       Preconditions.checkFromToIndex(begin, end, length, Preconditions.SIOOBE_FORMATTER);
//   }
//   static int checkBoundsOffCount(int offset, int count, int length) {
//       return Preconditions.checkFromIndexSize(offset, count, length, Preconditions.SIOOBE_FORMATTER);
//   }
//
// CratonVM used to intercept **both helpers** with SIOOBE-correct natives (the
// "F4" workaround, written for a BouncyCastle `PKCS12$Mappings` AIOOBE
// blocker), because the generic `Preconditions` override underneath discarded
// that formatter and threw `ArrayIndexOutOfBoundsException` for everyone. That
// bypassed the whole `Preconditions` chain for `String`-domain callers while
// leaving every other caller — NIO buffer slicing via `Objects.checkFromToIndex`
// — on the still-wrong generic path.
//
// `native-builtins/src/preconditions.rs` now honours the formatter, so the
// bytecode above produces the right class on its own and the two interceptors
// are gone. If a `String` bounds failure ever reports
// `ArrayIndexOutOfBoundsException` again, that module — not this one — is where
// the regression is.
//
// Reference: JDK 25 `java/lang/String.java` and `jdk/internal/util/Preconditions.java`.

/// Shared bounds check backing the `String(char[], int, int)` constructor
/// native and `getChars` below, both of which must agree with the real
/// `String.checkBoundsOffCount` they stand in for. Spec: bad iff
/// `offset < 0 || count < 0 || offset > length - count` (overflow-safe form:
/// `offset + count > length`). Returns `Some(index)` (the offending arg,
/// matching the JDK's SIOOBE message convention) when bad, `None` when the
/// range is valid.
fn bounds_off_count_violation(offset: i32, count: i32, length: i32) -> Option<i32> {
    let bad_size =
        offset < 0 || count < 0 || length < 0 || (offset as i64 + count as i64) > length as i64;
    if bad_size {
        Some(if offset < 0 { offset } else { count })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// PERF: String(char[]) / String(char[], int, int) constructor intrinsics
// ---------------------------------------------------------------------------
//
// Neither constructor had a CratonVM native override before this. `new
// String(char[])` ran as the real JDK's own bytecode end to end —
// `String(char[])` -> `String(char[], int, int)` -> the private
// `String(char[], int, int, Void)` helper -> `StringUTF16.compress`/
// `StringUTF16.toBytes` — a per-character Latin1-fits-in-a-byte check + copy
// loop executed one bytecode at a time by the interpreter. For large arrays
// this is purely an interpreter-throughput problem: measured at roughly
// 500-650ns/char (5.1-6.5s for a 10,000,000-element array) because the loop
// runs inside a single constructor invocation and never approaches the JIT's
// invocation-count warm-up threshold. This one conversion was slow enough to
// blow past an unrelated 3-second Tomcat connector read-timeout in
// `org.apache.catalina.core.TestSwallowAbortedUploads`'s `AbortedPOSTClient`
// tests — see
// `fixed-suite-bugs/tomcat/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`
// ("AbortedPOSTClient empty-response bug root-caused") for the full
// investigation.
//
// Fix: intercept both public constructors directly and do the Latin1-fits
// bulk scan + copy in Rust via `NativeContext::init_string_from_units`
// (backed by `populate_java_string_fields` in `vm/src/vm/vm_object.rs`,
// the same layout logic `create_string` uses), bypassing the interpreted
// loop entirely. `read_char_array_into` bulk-reads the source `char[]` (a
// single `copy_nonoverlapping` in the VM's override, not a per-element
// loop), so the whole constructor becomes O(n) Rust-side work with none of
// the per-bytecode interpreter dispatch overhead.
//
// This is intentionally scoped to exactly the two public constructor
// entry points real Java source can name (`new String(char[])` and
// `new String(char[], int, int)`) — the package-private
// `String(char[], int, int, Void)` helper those two delegate to in real JDK
// bytecode is never reached once these are registered, so it needs no
// override of its own.

/// `java.lang.String(char[] value)`
///
/// Real JDK semantics: `this(value, 0, value.length)`, i.e. throws
/// `NullPointerException` for a null array (dereferencing `.length`) and
/// otherwise always succeeds (offset 0 / count == length is always in
/// range).
pub(crate) fn native_string_init_from_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let len = ctx.array_length(arr);
    let mut units = vec![0u16; len];
    let written = ctx.read_char_array_into(arr, 0, &mut units);
    units.truncate(written);
    ctx.init_string_from_units(this, &units);
    Ok(None)
}

/// `java.lang.String(char[] value, int offset, int count)`
///
/// Real JDK semantics: NPE for a null array (dereferencing `.length` inside
/// `rangeCheck`), then `StringIndexOutOfBoundsException` per
/// `checkBoundsOffCount`'s spec (see [`bounds_off_count_violation`]) — both
/// checked *before* touching the array contents, matching the real
/// constructor's evaluation order.
pub(crate) fn native_string_init_from_char_array_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = ctx.array_length(arr) as i32;
    if bounds_off_count_violation(offset, count, length).is_some() {
        // HotSpot's `Preconditions.checkFromIndexSize` wording, verbatim.
        return Err(cratonvm_types::error::RuntimeError::sioobe_range_size(offset, count, length).into());
    }
    let mut units = vec![0u16; count as usize];
    let written = ctx.read_char_array_into(arr, offset as usize, &mut units);
    units.truncate(written);
    ctx.init_string_from_units(this, &units);
    Ok(None)
}

pub(crate) fn register_string_utf16_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/lang/StringUTF16",
        "isBigEndian",
        "()Z",
        native_string_utf16_is_big_endian,
    );
    registry.register(
        "java/lang/StringUTF16",
        "getChars",
        "([BII[CI)V",
        native_string_utf16_get_chars,
    );

    // `java/lang/String.checkBoundsBeginEnd(III)V` and `checkBoundsOffCount(III)I`
    // used to be registered here as `Intrinsic` (the "F4" workaround). They are
    // gone deliberately: they existed only to route `String`-domain bounds
    // failures around a generic `Preconditions` override that threw
    // `ArrayIndexOutOfBoundsException` for every caller, and
    // `native-builtins/src/preconditions.rs` now honours the `SIOOBE_FORMATTER`
    // those helpers pass, so the real bytecode produces
    // `StringIndexOutOfBoundsException` by itself. Retiring them is the test
    // that the underlying fix works, and it also fixes the callers F4 could
    // never reach (NIO buffer slicing, via `Objects.checkFromToIndex`).
    //
    // Do not re-add them as a `Bridge`: every `java/lang/String` `Bridge` is
    // dropped in real-JDK mode by `NativeMethodRegistry::register` (contract
    // §1.4), which is how F4 silently regressed twice. The two-sided pin in
    // `vm/tests/wp8_10_9_string_contains_native.rs` is what catches that now.

    // PERF: String(char[]) / String(char[], int, int) constructor
    // intrinsics — bulk Latin1-fits scan + copy in Rust instead of the
    // interpreted per-char loop in real JDK's `StringUTF16.compress`/
    // `toBytes`. See the comment block above `native_string_init_from_char_array`.
    registry.register(
        "java/lang/String",
        "<init>",
        "([C)V",
        native_string_init_from_char_array,
    );
    registry.register(
        "java/lang/String",
        "<init>",
        "([CII)V",
        native_string_init_from_char_array_range,
    );

    // T19.H1 fix: cglib TypeUtils.map relies on String.indexOf(String, int)
    // which JDK 25 implements via the package-private static helper
    // String.indexOf([BBILjava/lang/String;I)I. That helper dispatches into
    // StringLatin1.indexOf / StringUTF16.indexOf / StringUTF16.indexOfLatin1
    // — none of which we wire up. The fall-through path corrupts/loops in
    // the cglib map() loop because the helper returns 0 every iteration.
    // Register the public 2-arg form directly AND the static helper, so the
    // dispatch is short-circuited regardless of how it's called.
    registry.register(
        "java/lang/String",
        "indexOf",
        "(Ljava/lang/String;I)I",
        native_string_index_of_str_from,
    );
    registry.register(
        "java/lang/String",
        "indexOf",
        "([BBILjava/lang/String;I)I",
        native_string_index_of_static_helper,
    );
}

/// `String.indexOf(String tgt, int fromIndex)` — the public 2-arg form.
/// Cglib's TypeUtils.map sits in a tight loop calling this with a needle of
/// "[]" and incrementing `from`; if the implementation does not honor `from`
/// (or returns 0 when "[]" is absent) the loop never terminates. This is the
/// unambiguous, authoritative implementation: read both strings, search the
/// substring of `this` at offset `from`.
fn native_string_index_of_str_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let tgt = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from_raw = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src = ctx.read_string(this).unwrap_or_default();
    let needle = ctx.read_string(tgt).unwrap_or_default();
    // bug nb-lang-string: index over UTF-16 code UNITS, not Unicode code
    // points. Rust `char` is a code point, so `chars()`-based indexing is off
    // by one per preceding supplementary (>U+FFFF) char and disagrees with
    // charAt/length. Match `encode_utf16()` as charAt and the static helper do.
    let src_units: Vec<u16> = src.encode_utf16().collect();
    let needle_units: Vec<u16> = needle.encode_utf16().collect();
    let src_len = src_units.len() as i32;
    let from = from_raw.max(0);
    if needle_units.is_empty() {
        // JDK: empty needle → clamp(from, 0, length)
        return Ok(Some(Value::Int(from.min(src_len))));
    }
    if from >= src_len {
        return Ok(Some(Value::Int(-1)));
    }
    let nlen = needle_units.len();
    if nlen > src_units.len() {
        return Ok(Some(Value::Int(-1)));
    }
    let max_start = src_units.len() - nlen;
    let mut i = from as usize;
    while i <= max_start {
        if src_units[i..i + nlen] == needle_units[..] {
            return Ok(Some(Value::Int(i as i32)));
        }
        i += 1;
    }
    Ok(Some(Value::Int(-1)))
}

/// `String.indexOf(byte[] src, byte coder, int srcCount, String tgt, int from)`
/// — the package-private static helper invoked by `indexOf(String,int)` in JDK
/// 25's compact-string layout. We bypass the byte-array decoding entirely:
/// the source bytes are exactly the receiver's `value` field of *some* String
/// just decomposed by the JDK bytecode; rather than reverse-engineer the
/// coder format, we recompute the same answer character-wise from the target
/// alone if `srcCount` and `coder` are sufficient. Concretely we decode the
/// `[B` array per `coder` (0 = LATIN1 single-byte zero-extended, 1 = UTF16
/// LITTLE-endian u16 pairs) and search for the target's UTF-16 code units.
/// (The doc said big-endian; the code below reads `lo` at `2i` and `hi` at
/// `2i+1`, which is little-endian and is what the rest of the VM writes.)
fn native_string_index_of_static_helper(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let src_arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let coder = match args.get(1) {
        Some(Value::Int(v)) => *v & 0xff,
        _ => 0,
    };
    let src_count = match args.get(2) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let tgt = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from_raw = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Decode src bytes -> Vec<u16> per coder.
    let src_bytes_len = ctx.array_length(src_arr);
    let mut src_units: Vec<u16> = Vec::with_capacity(src_count);
    if coder == 1 {
        // UTF-16: little-endian u16 pairs (low byte first), srcCount = number
        // of code units.
        let pairs = src_count.min(src_bytes_len / 2);
        for i in 0..pairs {
            let lo = match ctx.get_array_element(src_arr, i * 2) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            let hi = match ctx.get_array_element(src_arr, i * 2 + 1) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            src_units.push((hi << 8) | lo);
        }
    } else {
        // LATIN1: 1 byte per code unit, zero-extended.
        let n = src_count.min(src_bytes_len);
        for i in 0..n {
            let b = match ctx.get_array_element(src_arr, i) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            src_units.push(b);
        }
    }

    let needle_str = ctx.read_string(tgt).unwrap_or_default();
    let needle_units: Vec<u16> = needle_str.encode_utf16().collect();

    let src_len = src_units.len() as i32;
    let from = from_raw.max(0);
    if needle_units.is_empty() {
        return Ok(Some(Value::Int(from.min(src_len))));
    }
    if from >= src_len {
        return Ok(Some(Value::Int(-1)));
    }
    let nlen = needle_units.len();
    let max_start = src_units.len().saturating_sub(nlen);
    let mut i = from as usize;
    while i <= max_start {
        if src_units[i..i + nlen] == needle_units[..] {
            return Ok(Some(Value::Int(i as i32)));
        }
        i += 1;
    }
    Ok(Some(Value::Int(-1)))
}

// ---------------------------------------------------------------------------
// java.lang.StringBuffer — delegates to StringBuilder natives
// ---------------------------------------------------------------------------
pub(crate) fn register_phase52_string_buffer(r: &mut NativeMethodRegistry) {
    let sb = "java/lang/StringBuffer";
    r.register(sb, "<init>", "()V", native_sb_init_default);
    r.register(sb, "<init>", "(I)V", native_sb_init_capacity);
    r.register(sb, "<init>", "(Ljava/lang/String;)V", native_sb_init_string);
    r.register(
        sb,
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sb_init_charsequence,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_append_string,
    );
    r.register(
        sb,
        "append",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_append_int,
    );
    r.register(
        sb,
        "append",
        "(J)Ljava/lang/StringBuffer;",
        native_sb_append_long,
    );
    r.register(
        sb,
        "append",
        "(D)Ljava/lang/StringBuffer;",
        native_sb_append_double,
    );
    r.register(
        sb,
        "append",
        "(F)Ljava/lang/StringBuffer;",
        native_sb_append_float,
    );
    r.register(
        sb,
        "append",
        "(Z)Ljava/lang/StringBuffer;",
        native_sb_append_boolean,
    );
    r.register(
        sb,
        "append",
        "(C)Ljava/lang/StringBuffer;",
        native_sb_append_char,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
        native_sb_append_object,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/StringBuffer;",
        native_sb_append_string,
    );
    r.register(sb, "toString", "()Ljava/lang/String;", native_sb_to_string);
    r.register(sb, "length", "()I", native_sb_length);
    r.register(sb, "charAt", "(I)C", native_sb_char_at);
    r.register(
        sb,
        "substring",
        "(I)Ljava/lang/String;",
        native_sb_substring,
    );
    r.register(
        sb,
        "substring",
        "(II)Ljava/lang/String;",
        native_sb_substring_range,
    );
    r.register(
        sb,
        "delete",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_delete,
    );
    r.register(
        sb,
        "deleteCharAt",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_delete_char_at,
    );
    r.register(
        sb,
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_replace,
    );
    r.register(
        sb,
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_insert_string,
    );
    r.register(
        sb,
        "insert",
        "(IC)Ljava/lang/StringBuffer;",
        native_sb_insert_char,
    );
    r.register(
        sb,
        "insert",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_insert_int,
    );
    r.register(
        sb,
        "reverse",
        "()Ljava/lang/StringBuffer;",
        native_sb_reverse,
    );
    r.register(sb, "indexOf", "(Ljava/lang/String;)I", native_sb_index_of);
    r.register(
        sb,
        "indexOf",
        "(Ljava/lang/String;I)I",
        native_sb_index_of_from,
    );
    r.register(sb, "setCharAt", "(IC)V", native_sb_set_char_at);
    r.register(sb, "setLength", "(I)V", native_sb_set_length);
    r.register(sb, "capacity", "()I", native_sb_capacity);
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_types::ArrayElementType;

    // -----------------------------------------------------------------------
    // Pure helper functions (no context needed)
    // -----------------------------------------------------------------------

    #[test]
    fn format_double_positive() {
        assert_eq!(format_double(1.0), "1.0");
    }

    #[test]
    fn format_double_infinity() {
        assert_eq!(format_double(f64::INFINITY), "Infinity");
    }

    #[test]
    fn format_double_neg_infinity() {
        assert_eq!(format_double(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn format_double_nan() {
        assert_eq!(format_double(f64::NAN), "NaN");
    }

    #[test]
    fn format_float_positive() {
        assert_eq!(format_float(1.0), "1.0");
    }

    #[test]
    fn format_float_nan() {
        assert_eq!(format_float(f32::NAN), "NaN");
    }

    fn join_custom_charsequence_to_string(
        ctx: &mut MockNativeContext,
        receiver: cratonvm_types::ObjectRef,
        method_name: &str,
        descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "toString" && descriptor == "()Ljava/lang/String;" {
            return Some(Ok(Some(ctx.get_field(receiver, 1))));
        }
        None
    }

    #[test]
    fn string_join_array_uses_to_string_for_custom_charsequence() {
        let mut ctx = mock_ctx();
        let delimiter = ctx.create_string("|");
        let prefix = ctx.create_string("prefix");
        let custom = ctx.fresh_object_ref();
        let custom_text = ctx.create_string("custom");
        // Leave field 0 non-reference so the mock's String-layout reader
        // cannot mistake this custom object for a String.
        ctx.set_field(custom, 1, Value::Object(Some(custom_text)));
        let sequences = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 3);
        ctx.set_array_element(sequences, 0, Value::Object(Some(prefix)));
        ctx.set_array_element(sequences, 1, Value::Object(Some(custom)));
        ctx.set_array_element(sequences, 2, Value::Object(None));
        ctx.set_invoke_virtual_hook(join_custom_charsequence_to_string);

        let result = native_string_join(
            &mut ctx,
            &[
                Value::Object(Some(delimiter)),
                Value::Object(Some(sequences)),
            ],
        )
        .unwrap();
        let Some(Value::Object(Some(joined))) = result else {
            panic!("String.join should return a String");
        };
        assert_eq!(
            ctx.read_string(joined).as_deref(),
            Some("prefix|custom|null")
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "String.join must release native roots"
        );
    }

    // -----------------------------------------------------------------------
    // bug nb-lang-string: UTF-16 code-UNIT indexing helpers
    // -----------------------------------------------------------------------

    #[test]
    fn last_index_of_units_basic() {
        // "abcabc" — last "bc" at code-unit index 4.
        let h: Vec<u16> = "abcabc".encode_utf16().collect();
        let n: Vec<u16> = "bc".encode_utf16().collect();
        assert_eq!(last_index_of_units(&h, &n), 4);
        // absent needle → -1
        let z: Vec<u16> = "zz".encode_utf16().collect();
        assert_eq!(last_index_of_units(&h, &z), -1);
        // empty needle → haystack length (in code units)
        assert_eq!(last_index_of_units(&h, &[]), 6);
    }

    #[test]
    fn last_index_of_units_supplementary() {
        // U+1F600 GRINNING FACE is a surrogate pair = 2 UTF-16 code units.
        // "\u{1F600}a\u{1F600}b": code-unit indices: [0,1]=face,2='a',
        // [3,4]=face,5='b'. lastIndexOf("a") (one unit) must be 2, and
        // lastIndexOf(face) must be 3 — code-unit indices, not code points.
        let s = "\u{1F600}a\u{1F600}b";
        let h: Vec<u16> = s.encode_utf16().collect();
        assert_eq!(h.len(), 6);
        let a: Vec<u16> = "a".encode_utf16().collect();
        assert_eq!(last_index_of_units(&h, &a), 2);
        let face: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        assert_eq!(face.len(), 2);
        assert_eq!(last_index_of_units(&h, &face), 3);
    }

    #[test]
    fn code_unit_eq_ignore_case_works() {
        let a: u16 = 'A' as u16;
        let lower_a: u16 = 'a' as u16;
        assert!(code_unit_eq_ignore_case(a, lower_a));
        assert!(code_unit_eq_ignore_case(a, a));
        assert!(!code_unit_eq_ignore_case('a' as u16, 'b' as u16));
        // Lone surrogate code units only match when bit-identical.
        assert!(code_unit_eq_ignore_case(0xD83D, 0xD83D));
        assert!(!code_unit_eq_ignore_case(0xD83D, 0xDE00));
    }

    // -----------------------------------------------------------------------
    // String length
    // -----------------------------------------------------------------------

    #[test]
    fn string_length_empty() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("");
        let r = native_string_length(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_length_hello() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let r = native_string_length(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r.unwrap(), Some(Value::Int(5)));
    }

    #[test]
    fn string_length_null_returns_zero() {
        let mut ctx = mock_ctx();
        let r = native_string_length(&mut ctx, &[Value::Object(None)]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // String charAt
    // -----------------------------------------------------------------------

    #[test]
    fn string_char_at_valid() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(1)]);
        assert_eq!(r.unwrap(), Some(Value::Int('b' as i32)));
    }

    #[test]
    fn string_char_at_out_of_bounds() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(5)]);
        assert!(r.is_err());
    }

    #[test]
    fn string_char_at_negative_index() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(-1)]);
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------------
    // T2.2.6: String.indexOf(int ch, int fromIndex) / lastIndexOf(II)I
    // -----------------------------------------------------------------------

    #[test]
    fn t2_string_index_of_from_finds_char_after_start() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcabc");
        let r = native_string_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('b' as i32),
                Value::Int(2),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(4)));
    }

    #[test]
    fn t2_string_index_of_from_clamps_negative_start() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('a' as i32),
                Value::Int(-100),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn t2_string_index_of_from_beyond_end_returns_minus_one() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('a' as i32),
                Value::Int(10),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_index_of_from_no_match() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcdef");
        let r = native_string_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('z' as i32),
                Value::Int(0),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_last_index_of_from_finds_char_before_start() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcabc");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('b' as i32),
                Value::Int(4),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(4)));
    }

    #[test]
    fn t2_string_last_index_of_from_stops_at_from_index() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcabc");
        // fromIndex=2 should find the first 'b' at index 1, not the second
        // at index 4.
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('b' as i32),
                Value::Int(2),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn t2_string_last_index_of_from_negative_returns_minus_one() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('a' as i32),
                Value::Int(-1),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_last_index_of_from_empty_string() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[
                Value::Object(Some(s)),
                Value::Int('a' as i32),
                Value::Int(0),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    // -----------------------------------------------------------------------
    // String equals
    // -----------------------------------------------------------------------

    #[test]
    fn string_equals_same_content() {
        let mut ctx = mock_ctx();
        let a = ctx.create_string("hello");
        let b = ctx.create_string("hello");
        let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn string_equals_different_content() {
        let mut ctx = mock_ctx();
        let a = ctx.create_string("hello");
        let b = ctx.create_string("world");
        let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_equals_null_arg() {
        let mut ctx = mock_ctx();
        let a = ctx.create_string("hello");
        let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(None)]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // String indexOf (char)
    // -----------------------------------------------------------------------

    #[test]
    fn string_index_of_found() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let r = native_string_index_of(&mut ctx, &[Value::Object(Some(s)), Value::Int('l' as i32)]);
        assert_eq!(r.unwrap(), Some(Value::Int(2)));
    }

    #[test]
    fn string_index_of_not_found() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let r = native_string_index_of(&mut ctx, &[Value::Object(Some(s)), Value::Int('z' as i32)]);
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    // -----------------------------------------------------------------------
    // String contains, startsWith, endsWith
    // -----------------------------------------------------------------------

    #[test]
    fn string_contains_true() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello world");
        let sub = ctx.create_string("world");
        let r = native_string_contains(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Object(Some(sub))],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn string_contains_false() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let sub = ctx.create_string("xyz");
        let r = native_string_contains(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Object(Some(sub))],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_starts_with_true() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello world");
        let prefix = ctx.create_string("hello");
        let r = native_string_starts_with(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Object(Some(prefix))],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn string_ends_with_true() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello world");
        let suffix = ctx.create_string("world");
        let r = native_string_ends_with(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Object(Some(suffix))],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    // -----------------------------------------------------------------------
    // String case conversion and trim
    // -----------------------------------------------------------------------

    #[test]
    fn string_to_upper_case() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let r = native_string_to_upper_case(&mut ctx, &[Value::Object(Some(s))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "HELLO");
    }

    #[test]
    fn string_to_lower_case() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("HELLO");
        let r = native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "hello");
    }

    #[test]
    fn string_trim_whitespace() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("  hello  ");
        let r = native_string_trim(&mut ctx, &[Value::Object(Some(s))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "hello");
    }

    // -----------------------------------------------------------------------
    // String isEmpty, isBlank
    // -----------------------------------------------------------------------

    #[test]
    fn string_is_empty_true() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("");
        let r = native_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn string_is_empty_false() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("a");
        let r = native_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // String valueOf
    // -----------------------------------------------------------------------

    #[test]
    fn string_value_of_int() {
        let mut ctx = mock_ctx();
        let r = native_string_value_of_int(&mut ctx, &[Value::Int(42)]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "42");
    }

    #[test]
    fn string_value_of_long() {
        let mut ctx = mock_ctx();
        let r = native_string_value_of_long(&mut ctx, &[Value::Long(123456789)]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "123456789");
    }

    #[test]
    fn string_value_of_boolean_true() {
        let mut ctx = mock_ctx();
        let r = native_string_value_of_boolean(&mut ctx, &[Value::Int(1)]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "true");
    }

    // -----------------------------------------------------------------------
    // String concat
    // -----------------------------------------------------------------------

    #[test]
    fn string_concat_basic() {
        let mut ctx = mock_ctx();
        let a = ctx.create_string("hello ");
        let b = ctx.create_string("world");
        let r = native_string_concat(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "hello world");
    }

    // -----------------------------------------------------------------------
    // String hashCode
    // -----------------------------------------------------------------------

    #[test]
    fn string_hash_code_consistent() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let r1 = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
        let r2 = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r1.unwrap(), r2.unwrap());
    }

    #[test]
    fn string_hash_code_empty_is_zero() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("");
        let r = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    /// A `String` that cannot answer "which layout am I" must not answer it.
    ///
    /// `native_string_hash_code` latches the hash field slot in a process-wide
    /// `OnceLock` that is never reset, from whichever `String` it happens to
    /// hash first. That is fine for a receiver with a readable `value` array
    /// and wrong for one without: `string_char_array` returns `None` for a
    /// `String` allocated but not yet initialised, and folding `None` into
    /// slot 1 — as this did — points every later call at `coder` on the JDK 25
    /// layout. The read side returns `1` as the hash of every UTF-16 string;
    /// the write side stores the hash INTO `coder`, silently corrupting the
    /// encoding flag of every string the process hashes afterwards.
    ///
    /// The `OnceLock` makes the whole-native version of this untestable — one
    /// latch per test binary — so the decision lives in a pure function and the
    /// test is on that. `None => None` is the entire fix; the two `Some` arms
    /// are here so a rewrite cannot quietly swap the layouts.
    #[test]
    fn hash_slot_is_never_guessed_from_an_unreadable_string() {
        use cratonvm_types::ArrayElementType;

        assert_eq!(
            hash_slot_for(None),
            None,
            "a String with a null `value` carries no layout evidence; latching a \
             guess from it points the whole process at `coder` (slot 1) and makes \
             every later hash write corrupt the string it hashed"
        );
        assert_eq!(
            hash_slot_for(Some(ArrayElementType::Byte)),
            Some(2),
            "compact JDK 9+ layout: value:[B, coder:B, hash:I, hashIsZero:Z"
        );
        assert_eq!(
            hash_slot_for(Some(ArrayElementType::Boolean)),
            Some(2),
            "boolean[] is this VM's byte[] alias, so it is the compact layout too"
        );
        assert_eq!(
            hash_slot_for(Some(ArrayElementType::Char)),
            Some(1),
            "legacy synthetic-stub layout: value:[C, hash:I"
        );
    }

    // -----------------------------------------------------------------------
    // Registration
    // -----------------------------------------------------------------------

    #[test]
    fn register_string_builder_natives_does_not_panic() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
        assert!(registry.len() > 20);
    }

    // -----------------------------------------------------------------------
    // C36: append(CharSequence, int, int) native interception
    // -----------------------------------------------------------------------
    //
    // These tests verify that the append(CharSequence)/append(CharSequence, int, int)
    // natives take effect on a StringBuilder initialised via native_sb_init_default
    // and correctly update field 1 (count) — the field layout where real JDK
    // bytecode would otherwise misread slot 2 as `count` and trigger huge array
    // allocations inside `Formatter.format`.

    fn make_sb(ctx: &mut dyn NativeContext) -> cratonvm_types::ObjectRef {
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        // Allocate enough slots to match real JDK layout width (value/coder/count ≈ 3).
        let sb = ctx.alloc_object(cid, 4);
        native_sb_init_default(ctx, &[Value::Object(Some(sb))]).unwrap();
        sb
    }

    #[test]
    fn sb_append_charsequence_full_string() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let s = ctx.create_string("hello");
        let r = native_sb_append_charsequence(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(s))],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "hello");
    }

    #[test]
    fn sb_append_charsequence_null_appends_null_literal() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let r = native_sb_append_charsequence(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(None)],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "null");
    }

    #[test]
    fn sb_append_charsequence_off_len_slice() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let s = ctx.create_string("abcdefg");
        let r = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(Some(s)),
                Value::Int(2),
                Value::Int(5),
            ],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "cde");
    }

    #[test]
    fn sb_append_charsequence_off_len_clamps_out_of_range() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let s = ctx.create_string("abc");
        // [-1, 100) should clamp to [0, 3) — never panic, never crash.
        let r = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(Some(s)),
                Value::Int(-1),
                Value::Int(100),
            ],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "abc");
    }

    #[test]
    fn sb_append_charsequence_off_len_empty_slice() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let s = ctx.create_string("abc");
        let r = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(Some(s)),
                Value::Int(1),
                Value::Int(1),
            ],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "");
    }

    #[test]
    fn sb_append_charsequence_registration_includes_cs_variants() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
        assert!(registry
            .find(
                "java/lang/StringBuilder",
                "append",
                "(Ljava/lang/CharSequence;II)Ljava/lang/StringBuilder;",
            )
            .is_some());
        assert!(registry
            .find(
                "java/lang/StringBuilder",
                "append",
                "(Ljava/lang/CharSequence;)Ljava/lang/StringBuilder;",
            )
            .is_some());
    }

    #[test]
    fn sb_append_charsequence_off_len_registered_for_abstract_sb() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
        // Real JDK dispatch names the AbstractStringBuilder return type.
        assert!(registry
            .find(
                "java/lang/AbstractStringBuilder",
                "append",
                "(Ljava/lang/CharSequence;II)Ljava/lang/AbstractStringBuilder;",
            )
            .is_some());
    }

    #[test]
    fn sb_abstract_builder_registration_includes_layout_sensitive_append_variants() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
        for (name, desc) in [
            (
                "append",
                "(Ljava/lang/String;)Ljava/lang/AbstractStringBuilder;",
            ),
            (
                "append",
                "(Ljava/lang/Object;)Ljava/lang/AbstractStringBuilder;",
            ),
            ("append", "([C)Ljava/lang/AbstractStringBuilder;"),
            ("append", "([CII)Ljava/lang/AbstractStringBuilder;"),
            ("append", "(I)Ljava/lang/AbstractStringBuilder;"),
            ("appendCodePoint", "(I)Ljava/lang/AbstractStringBuilder;"),
            ("repeat", "(II)Ljava/lang/AbstractStringBuilder;"),
            (
                "repeat",
                "(Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;",
            ),
        ] {
            assert!(
                registry
                    .find("java/lang/AbstractStringBuilder", name, desc)
                    .is_some(),
                "missing native for {name}{desc}"
            );
        }
    }

    #[test]
    fn sb_appendable_bridge_registration_includes_charsequence_variants() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
        assert!(registry
            .find(
                "java/lang/StringBuilder",
                "append",
                "(C)Ljava/lang/Appendable;"
            )
            .is_some());
        assert!(registry
            .find(
                "java/lang/StringBuilder",
                "append",
                "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
            )
            .is_some());
        assert!(registry
            .find(
                "java/lang/StringBuilder",
                "append",
                "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
            )
            .is_some());
    }

    // -----------------------------------------------------------------------
    // StringUTF16.isBigEndian — used during <clinit> to pick a byte order
    // for the compact-string byte[] layout. CratonVM always stores compact
    // UTF-16 strings little-endian (see `create_java_string` and the decoders
    // in this file), so this must return `false` to keep the real-JDK
    // StringUTF16 bytecode consistent with CratonVM's native string code. On
    // the x86-64 target this VM runs on, that also matches the host order.
    // -----------------------------------------------------------------------

    // FIX: was `..._returns_true` asserting Int(1). CratonVM is little-endian,
    // so isBigEndian() must be false; align expectation with host endianness
    // (the same model as `..._ignores_extra_args`).
    #[test]
    fn t18_k6_string_utf16_is_big_endian_returns_false() {
        let mut ctx = mock_ctx();
        let result = native_string_utf16_is_big_endian(&mut ctx, &[])
            .unwrap()
            .unwrap();
        // Little-endian on x86-64: isBigEndian() is false.
        let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
        assert_eq!(result, Value::Int(expected_int));
    }

    #[test]
    fn string_utf16_natives_are_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_string_utf16_natives(&mut registry);
        assert!(registry
            .find("java/lang/StringUTF16", "isBigEndian", "()Z")
            .is_some());
        assert!(registry
            .find("java/lang/StringUTF16", "getChars", "([BII[CI)V")
            .is_some());
    }

    #[test]
    fn string_utf16_get_chars_copies_little_endian_code_units() {
        let mut ctx = mock_ctx();
        let source = ctx.new_array(ArrayElementType::Byte, 6);
        // CratonVM compact UTF-16 storage is little-endian: A, omega, B.
        for (index, byte) in [0x41, 0x00, 0xA9, 0x03, 0x42, 0x00].into_iter().enumerate() {
            ctx.set_array_element(source, index, Value::Int(byte));
        }
        let dst = ctx.new_array(ArrayElementType::Char, 2);
        assert_eq!(
            native_string_utf16_get_chars(
                &mut ctx,
                &[
                    Value::Object(Some(source)),
                    Value::Int(1),
                    Value::Int(3),
                    Value::Object(Some(dst)),
                    Value::Int(0),
                ],
            )
            .unwrap(),
            None
        );
        assert_eq!(ctx.get_array_element(dst, 0), Value::Int(0x03A9));
        assert_eq!(ctx.get_array_element(dst, 1), Value::Int(0x0042));
    }

    #[test]
    fn string_utf16_get_chars_empty_range_does_not_dereference_arrays() {
        let mut ctx = mock_ctx();
        assert_eq!(
            native_string_utf16_get_chars(
                &mut ctx,
                &[
                    Value::Object(None),
                    Value::Int(3),
                    Value::Int(3),
                    Value::Object(None),
                    Value::Int(-1),
                ],
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn string_utf16_is_big_endian_ignores_extra_args() {
        // The Java verifier guarantees correct arity at the call site, but
        // our impl should not panic if called with unexpected trailing args.
        let mut ctx = mock_ctx();
        let r = native_string_utf16_is_big_endian(&mut ctx, &[Value::Int(0), Value::Int(42)])
            .unwrap()
            .unwrap();
        let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
        assert_eq!(r, Value::Int(expected_int));
    }

    // -----------------------------------------------------------------------
    // String bounds checks — must report StringIndexOutOfBoundsException,
    // never ArrayIndexOutOfBoundsException. The two are siblings, so
    // `catch (StringIndexOutOfBoundsException)` does not see an AIOOBE.
    // -----------------------------------------------------------------------

    /// Name the exception class a native raised, whichever of the two shapes
    /// it used.
    ///
    /// A native can fail two ways and the class matters in both: a
    /// `RuntimeError` the VM maps to a class later, or an already-materialised
    /// throwable (`ExceptionThrown`), which is what
    /// `crate::preconditions::throw_out_of_bounds` produces when it has to
    /// build the class an application-supplied exception formatter asked for.
    /// Classifying only the first shape would report `other-failed` for the
    /// second, so a test asserting `"sioobe"` would fail on a *correct* answer
    /// and — worse — a test asserting anything else would pass on a wrong one.
    fn err_kind(
        ctx: &dyn NativeContext,
        e: &cratonvm_types::error::MethodCallFailed,
    ) -> &'static str {
        match e {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(re),
            ) => match re {
                cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException { .. } => {
                    "sioobe"
                }
                cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException { .. } => {
                    "aioobe"
                }
                cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { .. } => "ioobe",
                _ => "other-runtime",
            },
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
                let class_id = ctx.class_id_of_object(*obj);
                match ctx.class_name_of_id(class_id).unwrap_or_default().as_str() {
                    "java/lang/StringIndexOutOfBoundsException" => "sioobe",
                    "java/lang/ArrayIndexOutOfBoundsException" => "aioobe",
                    "java/lang/IndexOutOfBoundsException" => "ioobe",
                    _ => "other-thrown",
                }
            }
            _ => "other-failed",
        }
    }

    #[test]
    fn bounds_off_count_accepts_an_in_range_window() {
        assert_eq!(bounds_off_count_violation(2, 3, 10), None);
        // Empty-string edge case: BC `PKCS12$Mappings` reaches this during
        // `Provider.put` for keys whose substring extraction falls into a
        // zero-length window.
        assert_eq!(bounds_off_count_violation(0, 0, 0), None);
        assert_eq!(bounds_off_count_violation(0, 10, 10), None);
    }

    #[test]
    fn bounds_off_count_names_the_offending_argument() {
        // The JDK's SIOOBE message convention: `offset` when it is the bad
        // one, otherwise `count`.
        assert_eq!(bounds_off_count_violation(-1, 2, 5), Some(-1));
        assert_eq!(bounds_off_count_violation(0, -2, 5), Some(-2));
        assert_eq!(bounds_off_count_violation(0, 6, 5), Some(6));
    }

    #[test]
    fn bounds_off_count_widens_before_adding() {
        // (offset=i32::MAX, count=1, length=i32::MAX) is out of bounds; without
        // the i64 widening the sum wraps negative and slips past the check.
        assert_eq!(bounds_off_count_violation(i32::MAX, 1, i32::MAX), Some(1));
    }

    /// The inverse of the old `f4_check_bounds_natives_registered_on_string`.
    ///
    /// `String.checkBoundsBeginEnd` / `checkBoundsOffCount` are no longer
    /// intercepted: their real bytecode hands `Preconditions` the
    /// `SIOOBE_FORMATTER`, and `crate::preconditions` honours it. Re-adding an
    /// interceptor here would mean the underlying override had regressed —
    /// fix that instead, because a native here reaches only the `String`
    /// callers and leaves NIO's on the generic path.
    #[test]
    fn string_bounds_helpers_are_left_to_their_bytecode() {
        let mut registry = NativeMethodRegistry::new();
        register_string_utf16_natives(&mut registry);
        assert!(
            registry
                .find("java/lang/String", "checkBoundsBeginEnd", "(III)V")
                .is_none(),
            "checkBoundsBeginEnd is the real bytecode's job now — see crate::preconditions"
        );
        assert!(
            registry
                .find("java/lang/String", "checkBoundsOffCount", "(III)I")
                .is_none(),
            "checkBoundsOffCount is the real bytecode's job now — see crate::preconditions"
        );
    }

    #[test]
    fn f4_code_point_at_negative_throws_sioobe_not_aioobe() {
        // Audit: native_string_code_point_at used to throw AIOOBE on out-of-range.
        // Spec mandates SIOOBE so callers' `catch (StringIndexOutOfBoundsException)`
        // handlers fire correctly.
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hi");
        let err = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(-1)])
            .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");
    }

    #[test]
    fn f4_code_point_at_too_large_throws_sioobe_not_aioobe() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hi");
        let err = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(99)])
            .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");
    }

    // -----------------------------------------------------------------------
    // AbstractStringBuilder.getChars(srcBegin, srcEnd, dst, dstBegin) — must
    // validate BOTH the source window (SIOOBE) and the destination window
    // (NPE on null dst, AIOOBE on negative dstBegin / overrun) before copying,
    // instead of silently dropping out-of-bounds writes.
    // -----------------------------------------------------------------------

    /// Build a StringBuilder pre-loaded with `text` for getChars tests.
    fn make_sb_with(ctx: &mut dyn NativeContext, text: &str) -> cratonvm_types::ObjectRef {
        let sb = make_sb(ctx);
        let chars: Vec<u16> = text.encode_utf16().collect();
        let buf = ctx.new_array(ArrayElementType::Char, chars.len().max(1));
        for (i, &c) in chars.iter().enumerate() {
            ctx.set_array_element(buf, i, Value::Int(i32::from(c)));
        }
        ctx.set_field(sb, 0, Value::Object(Some(buf)));
        // `make_sb` allocates 4 slots (mimicking the real JDK
        // value/coder/count layout width), so the count belongs in slot 2,
        // not slot 1 -- go through `sb_set_count` rather than poking a
        // slot directly so this stays correct regardless of the mock's
        // allocated width. A raw `set_field(sb, 1, ...)` here only wrote
        // the slot `sb_state` reads as a *fallback*, which a 4-slot object
        // never falls back to (slot 2 already holds a valid Int from
        // `make_sb`'s `native_sb_init_default` call) -- every
        // `sb_get_chars`/`sb_get_value`/`sb_get_coder` test using this
        // helper silently saw count=0 regardless of `text`.
        sb_set_count(ctx, sb, chars.len() as i32);
        sb
    }

    #[test]
    fn sb_get_chars_valid_copy() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "hello");
        let dst = ctx.new_array(ArrayElementType::Char, 8);
        // Copy "ell" (indices 1..4) into dst starting at offset 2.
        let r = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(1),
                Value::Int(4),
                Value::Object(Some(dst)),
                Value::Int(2),
            ],
        );
        assert!(r.unwrap().is_none());
        let read = |i: usize| match ctx.get_array_element(dst, i) {
            Value::Int(v) => v as u16,
            _ => 0,
        };
        assert_eq!(read(2), u16::from(b'e'));
        assert_eq!(read(3), u16::from(b'l'));
        assert_eq!(read(4), u16::from(b'l'));
        // Untouched slots stay zero.
        assert_eq!(read(0), 0);
        assert_eq!(read(5), 0);
    }

    #[test]
    fn sb_get_chars_null_dst_throws_npe() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abc");
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(3),
                Value::Object(None),
                Value::Int(0),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::NullPointerException { .. }
                )
            )
        ));
    }

    #[test]
    fn sb_get_chars_negative_dst_begin_throws_aioobe() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abc");
        let dst = ctx.new_array(ArrayElementType::Char, 8);
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(3),
                Value::Object(Some(dst)),
                Value::Int(-1),
            ],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "aioobe");
    }

    #[test]
    fn sb_get_chars_dst_overrun_throws_aioobe() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        let dst = ctx.new_array(ArrayElementType::Char, 4);
        // Copying 5 chars starting at offset 2 would write through index 6 > len 4.
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(5),
                Value::Object(Some(dst)),
                Value::Int(2),
            ],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "aioobe");
        // The out-of-bounds store must NOT have silently written anything past
        // the array; in-range slots remain at their zero default.
        for i in 0..ctx.array_length(dst) {
            assert_eq!(ctx.get_array_element(dst, i), Value::Int(0));
        }
    }

    #[test]
    fn sb_get_chars_bad_src_range_throws_sioobe() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abc");
        let dst = ctx.new_array(ArrayElementType::Char, 8);
        // srcEnd (9) exceeds the builder length (3) -> SIOOBE, not AIOOBE.
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(9),
                Value::Object(Some(dst)),
                Value::Int(0),
            ],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");
    }

    #[test]
    fn sb_get_chars_empty_window_at_dst_end_is_valid() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abc");
        let dst = ctx.new_array(ArrayElementType::Char, 4);
        // Zero-length copy with dstBegin == dst.length is exactly in bounds.
        let r = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(1),
                Value::Int(1),
                Value::Object(Some(dst)),
                Value::Int(4),
            ],
        );
        assert!(r.unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // BUG-TC0622: AbstractStringBuilder.getValue()[B / getCoder()B — synthetic
    // char[] buffer must present a compact byte[]+coder view matching CratonVM's
    // own String layout, so real-JDK String.nonSyncContentEquals computes right.
    // -----------------------------------------------------------------------

    fn read_bytes(ctx: &dyn NativeContext, arr: cratonvm_types::ObjectRef) -> Vec<u8> {
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as u8,
                _ => 0,
            })
            .collect()
    }

    #[test]
    fn sb_get_coder_latin1_for_ascii() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "http://localhost:8080");
        let r = native_sb_get_coder(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        assert_eq!(r, Some(Value::Int(0)), "all-ASCII builder must be LATIN1");
    }

    #[test]
    fn sb_get_coder_utf16_when_non_latin1_present() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "caf\u{00e9}\u{4e2d}"); // contains U+4E2D > 0xFF
        let r = native_sb_get_coder(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        assert_eq!(r, Some(Value::Int(1)), "char > 0xFF must force UTF16");
    }

    #[test]
    fn sb_get_value_latin1_one_byte_per_char() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "AbZ");
        let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(arr))) = r else {
            panic!("expected byte[]")
        };
        assert_eq!(read_bytes(&ctx, arr), vec![b'A', b'b', b'Z']);
    }

    #[test]
    fn sb_get_value_utf16_little_endian_pairs() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "A\u{4e2d}"); // 'A'=0x0041, U+4E2D
        let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(arr))) = r else {
            panic!("expected byte[]")
        };
        // Little-endian: low byte first. 0x0041 -> [0x41,0x00]; 0x4E2D -> [0x2D,0x4E].
        assert_eq!(read_bytes(&ctx, arr), vec![0x41, 0x00, 0x2D, 0x4E]);
    }

    #[test]
    fn sb_get_value_empty_builder_is_empty_array() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(arr))) = r else {
            panic!("expected byte[]")
        };
        assert_eq!(ctx.array_length(arr), 0);
    }

    #[test]
    fn sb_get_value_getcoder_registered_for_abstract_sb() {
        let mut registry = NativeMethodRegistry::new();
        register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
        assert!(registry
            .find("java/lang/AbstractStringBuilder", "getValue", "()[B")
            .is_some());
        assert!(registry
            .find("java/lang/AbstractStringBuilder", "getCoder", "()B")
            .is_some());
    }
}
