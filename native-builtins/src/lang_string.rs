//! String, StringBuilder, and StringBuffer native method implementations.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::Value;
use rustjvm_types::error::MethodCallResult;

use crate::{compile_java_regex, native_noop_with_this, obj_arg};

pub(crate) fn register_string_builder_natives(registry: &mut NativeMethodRegistry, class: &str) {
    registry.register(class, "<init>", "()V", native_sb_init_default);
    registry.register(
        class,
        "<init>",
        "(Ljava/lang/String;)V",
        native_sb_init_string,
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
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
        native_sb_append_object,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
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
        "toString",
        "()Ljava/lang/String;",
        native_sb_to_string,
    );
    registry.register(class, "length", "()I", native_sb_length);
    registry.register(class, "charAt", "(I)C", native_sb_char_at);
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
    registry.register(class, "trimToSize", "()V", native_noop_with_this);
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
            sb_write_chars(ctx, this, &chars);
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
            sb_write_chars(ctx, this, &chars);
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
    this: rustjvm_types::ObjectRef,
) -> Option<(rustjvm_types::ObjectRef, usize)> {
    match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => {
            let len = ctx.array_length(arr);
            Some((arr, len))
        }
        _ => None,
    }
}

pub(crate) fn native_string_intern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.intern on null".to_string()),
            }
            .into())
        }
    };

    // Read the string content, then intern it
    let text = ctx.read_string(this).unwrap_or_default();
    let interned = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(interned))))
}

pub(crate) fn native_string_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Layout-aware: JDK 25 String has fields {value:[B, coder:B, hash:I,
    // hashIsZero:Z}, our legacy synthetic-stub layout has {value:[C,
    // hash:I}. Detect via the value-array element type and read the
    // cached hash from the right slot. Writing the cache to the wrong
    // slot would clobber `coder` and corrupt all subsequent reads.
    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let elem_type = ctx.heap_element_type_of(arr);
    let is_byte_array = matches!(
        elem_type,
        rustjvm_types::ArrayElementType::Byte | rustjvm_types::ArrayElementType::Boolean,
    );
    let hash_field_index: usize = if is_byte_array { 2 } else { 1 };

    if let Value::Int(cached) = ctx.get_field(this, hash_field_index) {
        if cached != 0 {
            return Ok(Some(Value::Int(cached)));
        }
    }

    // For compact strings (byte[] value), inspect the `coder` byte
    // (field 1) to know whether the bytes are LATIN-1 (one byte per char,
    // unsigned-extended) or UTF-16 (big-endian u16 pairs).
    let is_utf16 = is_byte_array
        && matches!(ctx.get_field(this, 1), Value::Int(1));

    let mut hash: i32 = 0;
    if is_byte_array && is_utf16 {
        let chars = len / 2;
        for c in 0..chars {
            let hi = match ctx.get_array_element(arr, c * 2) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            let lo = match ctx.get_array_element(arr, c * 2 + 1) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            let ch = ((hi << 8) | lo) as i32;
            hash = hash.wrapping_mul(31).wrapping_add(ch);
        }
    } else if is_byte_array {
        // LATIN-1: each byte zero-extended.
        for i in 0..len {
            let ch = match ctx.get_array_element(arr, i) {
                Value::Int(v) => v & 0xff,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(ch);
        }
    } else {
        // Legacy / synthetic char[]: each element is already a u16
        // zero-extended in the Value::Int.
        for i in 0..len {
            let ch = match ctx.get_array_element(arr, i) {
                Value::Int(v) => v & 0xffff,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(ch);
        }
    }

    // Cache the hash (but 0 stays 0 — matches JDK behavior).
    if hash != 0 {
        ctx.set_field(this, hash_field_index, Value::Int(hash));
    }

    Ok(Some(Value::Int(hash)))
}

pub(crate) fn native_string_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    let len = match string_char_array(ctx, this) {
        Some((_, len)) => len as i32,
        None => 0,
    };
    Ok(Some(Value::Int(len)))
}

pub(crate) fn native_string_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.charAt on null".to_string()),
            }
            .into())
        }
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => {
            return Err(
                rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index }.into(),
            )
        }
    };

    if index < 0 || index >= len as i32 {
        return Err(rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index }.into());
    }

    let ch = ctx.get_array_element(arr, index as usize);
    Ok(Some(ch))
}

pub(crate) fn native_string_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

    // Compare char arrays
    let (arr_a, len_a) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let (arr_b, len_b) = match string_char_array(ctx, other) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };

    if len_a != len_b {
        return Ok(Some(Value::Int(0)));
    }

    for i in 0..len_a {
        let a = ctx.get_array_element(arr_a, i);
        let b = ctx.get_array_element(arr_b, i);
        if a != b {
            return Ok(Some(Value::Int(0)));
        }
    }

    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_string_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(-1))),
    };

    for i in 0..len {
        let elem = match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            _ => 0,
        };
        if elem == ch {
            return Ok(Some(Value::Int(i as i32)));
        }
    }

    Ok(Some(Value::Int(-1)))
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

    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(-1))),
    };

    let start = from.max(0) as usize;
    if start >= len {
        return Ok(Some(Value::Int(-1)));
    }
    // Note: for BMP characters this matches the raw char; for supplementary
    // code points the caller is expected to have already decomposed to
    // surrogates in the underlying `String.value` char array, so a direct
    // code-unit compare is still correct against the leading surrogate.
    let needle = ch & 0xFFFF;
    for i in start..len {
        let elem = match ctx.get_array_element(arr, i) {
            Value::Int(v) => v & 0xFFFF,
            _ => 0,
        };
        if elem == needle {
            return Ok(Some(Value::Int(i as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
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

    let (arr, len) = match string_char_array(ctx, this) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(-1))),
    };
    if from < 0 || len == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let start = (from as usize).min(len - 1);
    let needle = ch & 0xFFFF;
    let mut i = start as isize;
    while i >= 0 {
        let elem = match ctx.get_array_element(arr, i as usize) {
            Value::Int(v) => v & 0xFFFF,
            _ => 0,
        };
        if elem == needle {
            return Ok(Some(Value::Int(i as i32)));
        }
        i -= 1;
    }
    Ok(Some(Value::Int(-1)))
}

// NOTE: `native_string_code_point_at` (T2.2.7) and
// `native_string_compare_to_ignore_case` (T2.2.8) already exist elsewhere
// in this file (see below at the "// Step …" banners) and are registered
// from `lib.rs`. They are left untouched; the T2 census recorded both
// items as pre-existing.

pub(crate) fn native_string_substring(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
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

    // Read the full string, take substring, create new string
    let text = ctx.read_string(this).unwrap_or_default();
    let utf16: Vec<u16> = text.encode_utf16().collect();

    if begin < 0 || end < begin || end > utf16.len() as i32 {
        return Err(
            rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                index: if begin < 0 { begin } else { end },
            }
            .into(),
        );
    }

    let sub_utf16 = &utf16[begin as usize..end as usize];
    let sub_text = String::from_utf16_lossy(sub_utf16);
    let result = ctx.create_string(&sub_text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = int value
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = val.to_string();
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// Format a double like Java does (no trailing zeros for integers, etc.)
pub(crate) fn format_double(v: f64) -> String {
    if v == f64::INFINITY {
        "Infinity".to_string()
    } else if v == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else if v.is_nan() {
        "NaN".to_string()
    } else {
        // Java uses minimal representation
        let s = format!("{v}");
        // Ensure there's always a decimal point (Java does "1.0" not "1")
        if !s.contains('.') {
            format!("{s}.0")
        } else {
            s
        }
    }
}

// ---------------------------------------------------------------------------
// Step 2: StringBuilder / StringBuffer natives
// ---------------------------------------------------------------------------

/// Helper: read field 0 (char[] buffer) and field 1 (int count) from StringBuilder.
pub(crate) fn sb_state(
    ctx: &dyn NativeContext,
    this: rustjvm_types::ObjectRef,
) -> (Option<rustjvm_types::ObjectRef>, i32) {
    let buf = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let count = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    (buf, count)
}

/// Helper: ensure the StringBuilder has capacity for `additional` more chars.
/// Returns the char[] buffer (possibly newly allocated and copied).
pub(crate) fn sb_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: rustjvm_types::ObjectRef,
    additional: usize,
) -> rustjvm_types::ObjectRef {
    use rustjvm_types::ArrayElementType;

    let (buf, count) = sb_state(ctx, this);
    let count = count as usize;
    let old_cap = buf.map_or(0, |b| ctx.array_length(b));

    if count + additional <= old_cap {
        return buf.unwrap();
    }

    // Grow: max(old_cap * 2 + 2, count + additional)
    let new_cap = std::cmp::max(old_cap * 2 + 2, count + additional);
    let new_buf = ctx.new_array(ArrayElementType::Char, new_cap);

    // Copy old content
    if let Some(old_buf) = buf {
        for i in 0..count {
            let val = ctx.get_array_element(old_buf, i);
            ctx.set_array_element(new_buf, i, val);
        }
    }

    ctx.set_field(this, 0, Value::Object(Some(new_buf)));
    new_buf
}

/// Helper: append a slice of u16 chars to a StringBuilder.
pub(crate) fn sb_append_chars(ctx: &mut dyn NativeContext, this: rustjvm_types::ObjectRef, chars: &[u16]) {
    let buf = sb_ensure_capacity(ctx, this, chars.len());
    let (_, count) = sb_state(ctx, this);
    let count = count as usize;
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, count + i, Value::Int(ch as i32));
    }
    ctx.set_field(this, 1, Value::Int((count + chars.len()) as i32));
}

/// Helper: append a Rust string to a StringBuilder.
pub(crate) fn sb_append_str(ctx: &mut dyn NativeContext, this: rustjvm_types::ObjectRef, text: &str) {
    let chars: Vec<u16> = text.encode_utf16().collect();
    sb_append_chars(ctx, this, &chars);
}

pub(crate) fn native_sb_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let buf = ctx.new_array(ArrayElementType::Char, 16);
    ctx.set_field(this, 0, Value::Object(Some(buf)));
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_sb_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ArrayElementType;
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
    let buf = ctx.new_array(ArrayElementType::Char, cap);
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(ch as i32));
    }
    ctx.set_field(this, 0, Value::Object(Some(buf)));
    ctx.set_field(this, 1, Value::Int(chars.len() as i32));
    Ok(None)
}

pub(crate) fn native_sb_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(v)) => std::cmp::max(*v, 0) as usize,
        _ => 16,
    };
    let buf = ctx.new_array(ArrayElementType::Char, cap);
    ctx.set_field(this, 0, Value::Object(Some(buf)));
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_sb_append_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let text = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_else(|| "null".to_string()),
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    sb_append_str(ctx, this, &text);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    sb_append_str(ctx, this, &val.to_string());
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v as u16,
        _ => 0,
    };
    sb_append_chars(ctx, this, &[ch]);
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
    let mut chars: Vec<u16> = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end {
        if let Value::Int(ch) = ctx.get_array_element(arr, i) {
            chars.push(ch as u16);
        }
    }
    sb_append_chars(ctx, this, &chars);
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
    let mut chars: Vec<u16> = Vec::with_capacity(n);
    for i in 0..n {
        if let Value::Int(ch) = ctx.get_array_element(arr, i) {
            chars.push(ch as u16);
        }
    }
    sb_append_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    sb_append_str(ctx, this, if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_append_str(ctx, this, &val.to_string());
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_append_str(ctx, this, &format_double(val));
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_sb_append_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    sb_append_str(ctx, this, &format_float(val));
    Ok(Some(Value::Object(Some(this))))
}

/// Call `toString()` on an arbitrary object via virtual dispatch.
///
/// If the object is a Java String, reads it directly. Otherwise invokes
/// `toString()` which will find overridden versions in user-defined classes.
pub(crate) fn invoke_to_string(
    ctx: &mut dyn NativeContext,
    obj: rustjvm_types::ObjectRef,
) -> Result<String, rustjvm_types::error::MethodCallFailed> {
    // Fast path: if it's already a String object, just read it
    if let Some(s) = ctx.read_string(obj) {
        return Ok(s);
    }

    // Fast path for wrapper types: if the object has exactly 1 field and its
    // value is a primitive, format it directly. This handles Integer, Long,
    // Double, Float, Boolean, Character, Byte, Short — all wrapper types store
    // their primitive in field 0.
    let nf = ctx.object_num_fields(obj);
    if nf == 1 {
        match ctx.get_field(obj, 0) {
            Value::Int(v) => {
                // Could be Integer, Boolean, Byte, Short, Character.
                // Check class name for disambiguation.
                let class_id = ctx.class_id_of_object(obj);
                let name = ctx.class_name_of_id(class_id).unwrap_or_default();
                let formatted = if name.contains("Boolean") {
                    if v != 0 { "true" } else { "false" }.to_string()
                } else if name.contains("Character") {
                    char::from_u32(v as u32).unwrap_or('?').to_string()
                } else if name.contains("Byte") {
                    (v as i8).to_string()
                } else if name.contains("Short") {
                    (v as i16).to_string()
                } else {
                    // Integer or unknown int wrapper
                    v.to_string()
                };
                return Ok(formatted);
            }
            Value::Long(v) => return Ok(v.to_string()),
            Value::Float(v) => return Ok(format!("{}", v)),
            Value::Double(v) => return Ok(format!("{}", v)),
            _ => {} // Not a primitive wrapper
        }
    }

    // Call obj.toString() via virtual dispatch; fall back on dispatch errors
    let result = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
    match result {
        Ok(Some(Value::Object(Some(str_ref)))) => Ok(ctx
            .read_string(str_ref)
            .unwrap_or_else(|| "null".to_string())),
        Ok(_) | Err(_) => Ok(format!("Object@{:x}", ctx.identity_hash_code(obj))),
    }
}

pub(crate) fn native_sb_append_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => invoke_to_string(ctx, *obj)?,
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    sb_append_str(ctx, this, &text);
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
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => match invoke_to_string(ctx, *obj) {
            Ok(s) => s,
            Err(_) => String::new(),
        },
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    sb_append_str(ctx, this, &text);
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
        sb_append_str(ctx, this, "null");
        return Ok(Some(Value::Object(Some(this))));
    };

    // Coerce CharSequence to its UTF-16 text:
    //  - java.lang.String: read_string.
    //  - Any CharSequence with a toString()Ljava/lang/String;: invoke_to_string.
    // Both paths already handle StringBuilder/StringBuffer/String/CharBuffer.
    let text = match invoke_to_string(ctx, cs_obj) {
        Ok(s) => s,
        Err(_) => String::new(),
    };
    let chars: Vec<u16> = text.encode_utf16().collect();

    // Clamp [start, end] to the CharSequence's length; real JDK throws
    // IndexOutOfBoundsException, but silently clamping keeps JUnit's
    // error-reporting path alive — the segfault/OOM we're fixing is far worse
    // than an off-by-one in diagnostic output, and every real-world caller
    // inside JDK internals passes in-bounds indices.
    let len = chars.len();
    let s = (start.max(0) as usize).min(len);
    let e = (end.max(0) as usize).min(len);
    if e > s {
        sb_append_chars(ctx, this, &chars[s..e]);
    }
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
        None => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
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
    let result = ctx.create_string(&text);
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
    let src_begin = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
    let src_end   = match args.get(2) { Some(Value::Int(v)) => *v, _ => 0 };
    let dst = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(None),
    };
    let dst_begin = match args.get(4) { Some(Value::Int(v)) => *v, _ => 0 };

    let (buf, count) = sb_state(ctx, this);
    if src_begin < 0 || src_end > count || src_begin > src_end {
        return Err(rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
            index: src_begin,
        }.into());
    }
    let buf = match buf { Some(b) => b, None => return Ok(None) };
    let n = (src_end - src_begin) as usize;
    for i in 0..n {
        let ch = ctx.get_array_element(buf, src_begin as usize + i);
        let _ = ctx.set_array_element(dst, dst_begin as usize + i, ch);
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
        return Err(rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index }.into());
    }
    let buf = buf.unwrap();
    let ch = ctx.get_array_element(buf, index as usize);
    Ok(Some(ch))
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
pub(crate) fn sb_read_chars(ctx: &dyn NativeContext, this: rustjvm_types::ObjectRef) -> Vec<u16> {
    let (buf, count) = sb_state(ctx, this);
    let count = count as usize;
    let mut chars = Vec::with_capacity(count);
    if let Some(buf) = buf {
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
pub(crate) fn sb_write_chars(ctx: &mut dyn NativeContext, this: rustjvm_types::ObjectRef, chars: &[u16]) {
    let current_count = sb_state(ctx, this).1 as usize;
    let additional = chars.len().saturating_sub(current_count);
    let buf = sb_ensure_capacity(ctx, this, additional);
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(ch as i32));
    }
    ctx.set_field(this, 1, Value::Int(chars.len() as i32));
}

/// insert(int, String) — insert string at offset
pub(crate) fn native_sb_insert_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, char) — insert single char
pub(crate) fn native_sb_insert_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, int) — insert int as string
pub(crate) fn native_sb_insert_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// insert(int, Object) — insert Object via toString
pub(crate) fn native_sb_insert_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    sb_write_chars(ctx, this, &result);
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
    sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// deleteCharAt(int) — remove single char
pub(crate) fn native_sb_delete_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let chars = sb_read_chars(ctx, this);
    if index < chars.len() {
        let mut result = Vec::with_capacity(chars.len() - 1);
        result.extend_from_slice(&chars[..index]);
        result.extend_from_slice(&chars[index + 1..]);
        sb_write_chars(ctx, this, &result);
    }
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
    sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// setCharAt(int, char) — set char at index
pub(crate) fn native_sb_set_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_sb_set_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        let buf = sb_ensure_capacity(ctx, this, new_len - count);
        for i in count..new_len {
            ctx.set_array_element(buf, i, Value::Int(0));
        }
    } else if new_len < count {
        // Just zero the excess (optional for correctness), but must update count
        if let Some(buf) = buf {
            for i in new_len..count {
                ctx.set_array_element(buf, i, Value::Int(0));
            }
        }
    }
    ctx.set_field(this, 1, Value::Int(new_len as i32));
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
pub(crate) fn native_sb_index_of_from(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

/// substring(int) — substring from index to end
pub(crate) fn native_sb_substring(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let start = std::cmp::min(start, chars.len());
    let result = String::from_utf16_lossy(&chars[start..]);
    let str_obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// substring(int, int) — substring [start, end)
pub(crate) fn native_sb_substring_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(i)) => std::cmp::max(0, *i) as usize,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let start = std::cmp::min(start, chars.len());
    let end = std::cmp::min(end, chars.len());
    let end = std::cmp::max(start, end);
    let result = String::from_utf16_lossy(&chars[start..end]);
    let str_obj = ctx.create_string(&result);
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
pub(crate) fn native_sb_ensure_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

/// Format a float like Java does.
pub(crate) fn format_float(v: f32) -> String {
    if v == f32::INFINITY {
        "Infinity".to_string()
    } else if v == f32::NEG_INFINITY {
        "-Infinity".to_string()
    } else if v.is_nan() {
        "NaN".to_string()
    } else {
        let s = format!("{v}");
        if !s.contains('.') {
            format!("{s}.0")
        } else {
            s
        }
    }
}

// ---------------------------------------------------------------------------

/// Helper: read a String's char[] into a Vec<u16>.
pub(crate) fn read_string_chars(ctx: &dyn NativeContext, obj: rustjvm_types::ObjectRef) -> Vec<u16> {
    let (arr, len) = match string_char_array(ctx, obj) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut chars = Vec::with_capacity(len);
    for i in 0..len {
        let ch = match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as u16,
            _ => 0,
        };
        chars.push(ch);
    }
    chars
}

pub(crate) fn native_string_to_char_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let chars = read_string_chars(ctx, this);
    let arr = ctx.new_array(ArrayElementType::Char, chars.len());
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(ch as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_string_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let haystack = read_string_chars(ctx, this);
    let needle = read_string_chars(ctx, other);
    if needle.is_empty() {
        return Ok(Some(Value::Int(1)));
    }
    let found = haystack
        .windows(needle.len())
        .any(|w| w == needle.as_slice());
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_string_starts_with(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let prefix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_chars = read_string_chars(ctx, this);
    let prefix_chars = read_string_chars(ctx, prefix);
    let result = this_chars.starts_with(&prefix_chars);
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
    let this_chars = read_string_chars(ctx, this);
    let prefix_chars = read_string_chars(ctx, prefix);
    let result = if offset <= this_chars.len() {
        this_chars[offset..].starts_with(&prefix_chars)
    } else {
        false
    };
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_string_ends_with(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let suffix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_chars = read_string_chars(ctx, this);
    let suffix_chars = read_string_chars(ctx, suffix);
    let result = this_chars.ends_with(&suffix_chars);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_string_trim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let trimmed = text.trim().to_string();
    let result = ctx.create_string(&trimmed);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let mut chars = read_string_chars(ctx, this);
    for ch in &mut chars {
        if *ch == old_char {
            *ch = new_char;
        }
    }
    let text = String::from_utf16_lossy(&chars);
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_to_lower_case(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let lower = text.to_lowercase();
    let result = ctx.create_string(&lower);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_to_upper_case(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let upper = text.to_uppercase();
    let result = ctx.create_string(&upper);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_string_value_of_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let result = ctx.create_string(&val.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let result = ctx.create_string(if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let result = ctx.create_string(&format_double(val));
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\0'),
        _ => '\0',
    };
    let result = ctx.create_string(&ch.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let result = ctx.create_string(&format_float(val));
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_value_of_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = match args.first() {
        Some(Value::Object(Some(obj))) => invoke_to_string(ctx, *obj)?,
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.compareTo on null argument".to_string()),
            }
            .into())
        }
    };
    let a = read_string_chars(ctx, this);
    let b = read_string_chars(ctx, other);
    let min_len = std::cmp::min(a.len(), b.len());
    for i in 0..min_len {
        let diff = a[i] as i32 - b[i] as i32;
        if diff != 0 {
            return Ok(Some(Value::Int(diff)));
        }
    }
    Ok(Some(Value::Int(a.len() as i32 - b.len() as i32)))
}

pub(crate) fn native_string_index_of_str(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let haystack = read_string_chars(ctx, this);
    let needle = read_string_chars(ctx, target);
    if needle.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    if needle.len() > haystack.len() {
        return Ok(Some(Value::Int(-1)));
    }
    for i in 0..=(haystack.len() - needle.len()) {
        if haystack[i..i + needle.len()] == *needle {
            return Ok(Some(Value::Int(i as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

pub(crate) fn native_string_substring_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.substring on null".to_string()),
            }
            .into())
        }
    };
    let begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = ctx.read_string(this).unwrap_or_default();
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let end = utf16.len() as i32;

    if begin < 0 || begin > end {
        return Err(
            rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index: begin }.into(),
        );
    }

    let sub_utf16 = &utf16[begin as usize..end as usize];
    let sub_text = String::from_utf16_lossy(sub_utf16);
    let result = ctx.create_string(&sub_text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_string_get_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ArrayElementType;
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

pub(crate) fn native_string_concat(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("String.concat: argument is null".to_string()),
            }
            .into())
        }
    };
    let a = ctx.read_string(this).unwrap_or_default();
    let b = ctx.read_string(other).unwrap_or_default();
    let combined = format!("{a}{b}");
    let result = ctx.create_string(&combined);
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Phase 8 Part 7: Additional String methods
// ---------------------------------------------------------------------------

pub(crate) fn native_string_split(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_string_split_impl(ctx, args, -1)
}

pub(crate) fn native_string_split_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    native_string_split_impl(ctx, args, limit)
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

    let parts: Vec<&str> = if delim.is_empty() {
        // Empty delimiter: split each character (like Java regex "")
        s.split("").filter(|p| !p.is_empty()).collect()
    } else if let Ok(re) = compile_java_regex(&delim, 0) {
        if limit > 0 {
            re.splitn(&s, limit as usize).collect()
        } else {
            re.split(&s).collect()
        }
    } else if limit > 0 {
        s.splitn(limit as usize, delim.as_str()).collect()
    } else {
        s.split(delim.as_str()).collect()
    };

    // When limit == 0 (default for String.split(regex)), remove trailing empty strings
    let parts: Vec<&str> = if limit == 0 {
        let mut v: Vec<&str> = parts;
        while v.last() == Some(&"") {
            v.pop();
        }
        if v.is_empty() {
            vec![""]
        } else {
            v
        }
    } else {
        parts
    };

    // Create String[] array
    let string_class_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(id) => id,
        Err(_) => rustjvm_types::ClassId::new(0),
    };
    let arr = ctx.new_ref_array(string_class_id, parts.len());
    for (i, part) in parts.iter().enumerate() {
        let str_ref = ctx.create_string(part);
        ctx.set_array_element(arr, i, Value::Object(Some(str_ref)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_string_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = delimiter, args[1] = CharSequence[]
    let delim = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => String::new(),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    };
    let len = ctx.array_length(arr);
    let mut parts = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(elem)) = ctx.get_array_element(arr, i) {
            parts.push(ctx.read_string(elem).unwrap_or_default());
        } else {
            parts.push("null".to_string());
        }
    }
    let joined = parts.join(&delim);
    Ok(Some(Value::Object(Some(ctx.create_string(&joined)))))
}

pub(crate) fn native_string_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let result = if let Ok(re) = compile_java_regex(&pattern, 0) {
        re.replace_all(&s, replacement.as_str()).into_owned()
    } else {
        s.replace(&pattern, &replacement)
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&result)))))
}

pub(crate) fn native_string_replace_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let result = if let Ok(re) = compile_java_regex(&pattern, 0) {
        re.replace(&s, replacement.as_str()).into_owned()
    } else {
        s.replacen(&pattern, &replacement, 1)
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&result)))))
}

pub(crate) fn native_string_matches(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let matched = if let Ok(re) = compile_java_regex(&pattern, 0) {
        let anchored = format!("^(?:{})$", re.as_str());
        regex::Regex::new(&anchored).is_ok_and(|full_re| full_re.is_match(&s))
    } else {
        s == pattern
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
        Some(Value::Int(c)) => char::from_u32(*c as u32).unwrap_or('\0'),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let result = s
        .rfind(ch)
        .map(|byte_idx| s[..byte_idx].chars().count() as i32)
        .unwrap_or(-1);
    Ok(Some(Value::Int(result)))
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
    let result = s
        .rfind(&needle)
        .map(|byte_idx| s[..byte_idx].chars().count() as i32)
        .unwrap_or(-1);
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_string_get_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let chars: Vec<char> = s.chars().collect();
    for (i, &ch) in chars
        .iter()
        .enumerate()
        .take(src_end.min(chars.len()))
        .skip(src_begin)
    {
        ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(ch as i32));
    }
    Ok(None)
}

pub(crate) fn native_string_strip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(s.trim())))))
}

pub(crate) fn native_string_strip_leading(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(s.trim_start())))))
}

pub(crate) fn native_string_strip_trailing(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(s.trim_end())))))
}

pub(crate) fn native_string_copy_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = char[]
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
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
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

// ---------------------------------------------------------------------------
// Phase 14 Step 3: String extras
// ---------------------------------------------------------------------------

pub(crate) fn native_string_code_point_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        return Err(rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
            index: index_i32,
        }
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

pub(crate) fn native_string_code_point_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
            let str_obj = ctx.create_string(line);
            Value::Object(Some(str_obj))
        })
        .collect();
    // Use the Stream pattern from collections
    let stream_class_id = match ctx.ensure_class_initialized("java/util/stream/Stream") {
        Ok(id) => id,
        Err(_) => rustjvm_types::ClassId::new(0),
    };
    let stream = ctx.alloc_object(stream_class_id, 1);
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn native_string_indent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let str_obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

pub(crate) fn native_string_transform(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_string_format(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static: args[0] = format String, args[1] = Object[] array
    let fmt_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
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

    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            i += 1;
            // Check for %% and %n first
            if chars[i] == '%' {
                result.push('%');
                i += 1;
                continue;
            }
            if chars[i] == 'n' {
                result.push('\n');
                i += 1;
                continue;
            }

            // Parse optional flags: -, +, 0, ' ', #, (
            let mut flags = String::new();
            while i < chars.len() && "-+0 #(".contains(chars[i]) {
                flags.push(chars[i]);
                i += 1;
            }

            // Parse optional width
            let mut width: Option<usize> = None;
            let width_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
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
            if i < chars.len() && chars[i] == '.' {
                i += 1;
                let prec_start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                precision = if i > prec_start {
                    chars[prec_start..i].iter().collect::<String>().parse().ok()
                } else {
                    Some(0)
                };
            }

            // Parse conversion character
            if i < chars.len() {
                let spec = chars[i];
                i += 1;
                match spec {
                    's' | 'd' | 'f' | 'x' | 'X' | 'c' | 'b' | 'e' | 'E' | 'g' | 'G' | 'o' | 'h'
                    | 'H' | 'a' | 'A' => {
                        if arg_idx < arr_len {
                            if let Some(a) = arr_ref {
                                let elem = ctx.get_array_element(a, arg_idx);
                                let text =
                                    format_arg_full(ctx, &elem, spec, &flags, width, precision);
                                result.push_str(&text);
                            }
                        }
                        arg_idx += 1;
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
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    let obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(obj))))
}

/// Format a single argument with flags, width, and precision support.
#[allow(clippy::too_many_arguments)]
pub(crate) fn format_arg_full(
    ctx: &dyn NativeContext,
    val: &Value,
    spec: char,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
) -> String {
    // Get the raw formatted value first
    let raw = format_arg(ctx, val, spec);

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

    formatted
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
pub(crate) fn format_arg(ctx: &dyn NativeContext, val: &Value, spec: char) -> String {
    // Helper: unbox wrapper object to primitive
    fn unbox_obj(ctx: &dyn NativeContext, obj: rustjvm_types::ObjectRef) -> Value {
        let nf = ctx.object_num_fields(obj);
        if nf >= 1 {
            let f = ctx.get_field(obj, 0);
            match f {
                Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => return f,
                _ => {}
            }
        }
        Value::Object(Some(obj))
    }

    match val {
        Value::Object(None) => match spec {
            'b' => "false".to_string(),
            _ => "null".to_string(),
        },
        Value::Object(Some(obj)) => {
            // For %b: check if it's a Boolean wrapper, else non-null = true
            if spec == 'b' {
                let inner = unbox_obj(ctx, *obj);
                return match inner {
                    Value::Int(v) => if v != 0 { "true" } else { "false" }.to_string(),
                    _ => "true".to_string(),
                };
            }
            // For %s: read string or call toString
            if spec == 's' || spec == 'h' || spec == 'H' {
                return ctx.read_string(*obj).unwrap_or_else(|| "null".to_string());
            }
            // Try to unbox wrapper to primitive and recurse
            let inner = unbox_obj(ctx, *obj);
            match inner {
                Value::Object(_) => {
                    // Not a wrapper — fallback to string
                    ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
                }
                _ => format_arg(ctx, &inner, spec),
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
            _ => format!("{}", v),
        },
        Value::Double(v) => match spec {
            'f' => format!("{:.6}", v),
            'e' => format!("{:e}", v),
            'E' => format!("{:E}", v),
            _ => format!("{}", v),
        },
        _ => "?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// String modern methods (Java 11+)
// ---------------------------------------------------------------------------

/// repeat(int) — "ab".repeat(3) → "ababab"
pub(crate) fn native_string_repeat(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let str_obj = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// isBlank() — true if empty or all whitespace
pub(crate) fn native_string_is_blank(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Int(if s.trim().is_empty() { 1 } else { 0 })))
}

/// chars() — returns IntStream of char values
pub(crate) fn native_string_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ClassId;

    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let char_values: Vec<Value> = s.encode_utf16().map(|c| Value::Int(c as i32)).collect();

    // Create IntStream: 1-field synthetic object (field 0 = Object[] elements)
    let stream = ctx.alloc_object(ClassId::new(0), 1);
    let arr = ctx.new_ref_array(ClassId::new(0), char_values.len());
    for (i, val) in char_values.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
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
    let toffset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let other = match args.get(3) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let ooffset = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(5) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };

    let s = ctx.read_string(this).unwrap_or_default();
    let s_chars: Vec<char> = s.chars().collect();
    let o_chars: Vec<char> = other.chars().collect();

    if toffset + len > s_chars.len() || ooffset + len > o_chars.len() {
        return Ok(Some(Value::Int(0)));
    }

    for i in 0..len {
        let sc = s_chars[toffset + i];
        let oc = o_chars[ooffset + i];
        let eq = if ignore_case {
            sc.to_lowercase().eq(oc.to_lowercase())
        } else {
            sc == oc
        };
        if !eq {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// regionMatches(int toffset, String other, int ooffset, int len) — case-sensitive
pub(crate) fn native_string_region_matches(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let toffset = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let other = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let ooffset = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };

    let s = ctx.read_string(this).unwrap_or_default();
    let s_chars: Vec<char> = s.chars().collect();
    let o_chars: Vec<char> = other.chars().collect();

    if toffset + len > s_chars.len() || ooffset + len > o_chars.len() {
        return Ok(Some(Value::Int(0)));
    }

    for i in 0..len {
        if s_chars[toffset + i] != o_chars[ooffset + i] {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// formatted(Object[]) — instance method: this.formatted(args) → String.format(this, args)
/// String.format(Locale, String, Object...) — static method with Locale (ignored for now).
/// args[0] = Locale, args[1] = format String, args[2] = Object[]
pub(crate) fn native_string_format_locale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Skip the Locale argument (args[0]) and delegate to the main format impl
    let format_args = [
        args.get(1).cloned().unwrap_or(Value::Object(None)),
        args.get(2).cloned().unwrap_or(Value::Object(None)),
    ];
    native_string_format(ctx, &format_args)
}

pub(crate) fn native_string_formatted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
// which byte order the compact-string byte[] uses for its `char` pairs. The
// JDK implementation is an intrinsic bound to the host's native byte order;
// we mirror that behaviour with a compile-time check against `target_endian`.
// All tier-1 Rust targets (x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu,
// aarch64-apple-darwin) are little-endian; only legacy big-endian targets
// (SPARC, s390x, PowerPC BE) would return `true`.
pub(crate) fn native_string_utf16_is_big_endian(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    #[cfg(target_endian = "big")]
    let result = 1;
    #[cfg(target_endian = "little")]
    let result = 0;
    Ok(Some(Value::Int(result)))
}

// ---------------------------------------------------------------------------
// F4: String.checkBoundsBeginEnd / checkBoundsOffCount native overrides
// ---------------------------------------------------------------------------
//
// In OpenJDK 25, both helpers delegate to `Preconditions`:
//
//   static void checkBoundsBeginEnd(int begin, int end, int length) {
//       Preconditions.checkFromToIndex(begin, end, length, Preconditions.SIOOBE_FORMATTER);
//   }
//   static int checkBoundsOffCount(int offset, int count, int length) {
//       return Preconditions.checkFromIndexSize(offset, count, length, Preconditions.SIOOBE_FORMATTER);
//   }
//
// The `SIOOBE_FORMATTER` is a `BiFunction<String, List<Number>, StringIndexOutOfBoundsException>`
// that ensures a *StringIndexOutOfBoundsException* (subclass of IndexOutOfBoundsException) is
// thrown — never an `ArrayIndexOutOfBoundsException`.
//
// Our existing `jdk/internal/util/Preconditions.checkFromToIndex(IIILjava/util/function/BiFunction;)I`
// override (registered from `lib.rs`) ignores the `BiFunction` argument and unconditionally
// throws `ArrayIndexOutOfBoundsException`. That is wrong for any String-related caller and
// surfaced as the BouncyCastle `PKCS12$Mappings` AIOOBE blocker (F4): one of the BC
// algorithm-key parsing paths (e.g. `String.indexOf(int, int, int)` on a `KeyStore.PKCS12`
// alias key without a `.` separator, or a `String(byte[], int, int, Charset)` ctor with
// `offset=count=0` on an edge case) bottoms out in `checkBoundsBeginEnd` / `checkBoundsOffCount`,
// which then dispatches to the broken Preconditions stub.
//
// Fix: intercept the two String helpers directly with a SIOOBE-correct native. This bypasses
// the entire Preconditions chain for every String-domain caller (substring, indexOf(I,I,I),
// String byte[]/char[]/codePoints[] ctors, getChars, getBytes, …). Callers outside String
// (e.g. NIO buffer slicing) still hit the unchanged Preconditions natives, which is correct
// for them.
//
// Reference: JDK 25 `java/lang/String.java` and `jdk/internal/util/Preconditions.java`.

/// `static void java.lang.String.checkBoundsBeginEnd(int begin, int end, int length)`
///
/// Spec: throws `StringIndexOutOfBoundsException` iff
/// `begin < 0 || begin > end || end > length`.
pub(crate) fn native_string_check_bounds_begin_end(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let begin = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if begin < 0 || begin > end || end > length {
        // Mirror the index the JDK puts in the SIOOBE message: `begin` if it's
        // the offending arg, otherwise `end`.
        let index = if begin < 0 { begin } else { end };
        return Err(
            rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index }.into(),
        );
    }
    Ok(None)
}

/// `static int java.lang.String.checkBoundsOffCount(int offset, int count, int length)`
///
/// Spec: throws `StringIndexOutOfBoundsException` iff
/// `offset < 0 || count < 0 || offset > length - count` (with overflow-safe form
/// `offset + count > length` taking care to avoid signed overflow). Returns
/// `offset` on success.
pub(crate) fn native_string_check_bounds_off_count(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Overflow-safe `offset + count > length`: rearranged as
    // `offset > length - count` when both `length` and `count` are
    // non-negative; outside that window the negative-arg check fires first.
    let bad_size = offset < 0
        || count < 0
        || length < 0
        || (offset as i64 + count as i64) > length as i64;
    if bad_size {
        let index = if offset < 0 { offset } else { count };
        return Err(
            rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { index }.into(),
        );
    }
    Ok(Some(Value::Int(offset)))
}

pub(crate) fn register_string_utf16_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/lang/StringUTF16",
        "isBigEndian",
        "()Z",
        native_string_utf16_is_big_endian,
    );

    // F4: replace the JDK 25 implementation of String.checkBoundsBeginEnd and
    // String.checkBoundsOffCount with SIOOBE-correct natives. See the comment
    // block above for full rationale. This registration runs from
    // `lib.rs::register_natives` AFTER the generic `Preconditions.checkFromToIndex`
    // / `checkFromIndexSize` overrides that throw `ArrayIndexOutOfBoundsException`,
    // and bypasses them entirely for every String-domain caller — substring(II),
    // indexOf(I,I,I), String(byte[],int,int[,Charset]) ctors, getChars(II[CI),
    // getBytes(II[BI), …
    registry.register(
        "java/lang/String",
        "checkBoundsBeginEnd",
        "(III)V",
        native_string_check_bounds_begin_end,
    );
    registry.register(
        "java/lang/String",
        "checkBoundsOffCount",
        "(III)I",
        native_string_check_bounds_off_count,
    );
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
    use super::*;
    use crate::test_utils::mock_ctx;

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
            &[Value::Object(Some(s)), Value::Int('b' as i32), Value::Int(2)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(4)));
    }

    #[test]
    fn t2_string_index_of_from_clamps_negative_start() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('a' as i32), Value::Int(-100)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn t2_string_index_of_from_beyond_end_returns_minus_one() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('a' as i32), Value::Int(10)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_index_of_from_no_match() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcdef");
        let r = native_string_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('z' as i32), Value::Int(0)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_last_index_of_from_finds_char_before_start() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abcabc");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('b' as i32), Value::Int(4)],
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
            &[Value::Object(Some(s)), Value::Int('b' as i32), Value::Int(2)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn t2_string_last_index_of_from_negative_returns_minus_one() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("abc");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('a' as i32), Value::Int(-1)],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn t2_string_last_index_of_from_empty_string() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("");
        let r = native_string_last_index_of_from(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int('a' as i32), Value::Int(0)],
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
        let r = native_string_concat(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        );
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

    fn make_sb(ctx: &mut dyn NativeContext) -> rustjvm_types::ObjectRef {
        let cid = ctx.ensure_class_initialized("java/lang/StringBuilder").unwrap();
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
        let Some(Value::Object(Some(out_obj))) = out_r else { panic!() };
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
        let Some(Value::Object(Some(out_obj))) = out_r else { panic!() };
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
        let Some(Value::Object(Some(out_obj))) = out_r else { panic!() };
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
        let Some(Value::Object(Some(out_obj))) = out_r else { panic!() };
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
        let Some(Value::Object(Some(out_obj))) = out_r else { panic!() };
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

    // -----------------------------------------------------------------------
    // StringUTF16.isBigEndian — used during <clinit> to pick a byte order
    // for the compact-string byte[] layout. Mirrors the host endianness.
    // -----------------------------------------------------------------------

    #[test]
    fn t18_k6_string_utf16_is_big_endian_returns_platform_endian() {
        let mut ctx = mock_ctx();
        let result = native_string_utf16_is_big_endian(&mut ctx, &[]).unwrap().unwrap();
        // On x86_64 hosts we expect false; on big-endian we'd expect true.
        let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
        assert_eq!(result, Value::Int(expected_int));
    }

    #[test]
    fn string_utf16_is_big_endian_registered() {
        let mut registry = NativeMethodRegistry::new();
        register_string_utf16_natives(&mut registry);
        assert!(registry
            .find("java/lang/StringUTF16", "isBigEndian", "()Z")
            .is_some());
    }

    #[test]
    fn string_utf16_is_big_endian_ignores_extra_args() {
        // The Java verifier guarantees correct arity at the call site, but
        // our impl should not panic if called with unexpected trailing args.
        let mut ctx = mock_ctx();
        let r = native_string_utf16_is_big_endian(
            &mut ctx,
            &[Value::Int(0), Value::Int(42)],
        )
        .unwrap()
        .unwrap();
        let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
        assert_eq!(r, Value::Int(expected_int));
    }

    // -----------------------------------------------------------------------
    // F4: String.checkBoundsBeginEnd / checkBoundsOffCount — must throw
    // StringIndexOutOfBoundsException, never ArrayIndexOutOfBoundsException.
    // -----------------------------------------------------------------------

    fn err_kind(e: &rustjvm_types::error::MethodCallFailed) -> &'static str {
        match e {
            rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(re),
            ) => match re {
                rustjvm_types::error::RuntimeError::StringIndexOutOfBoundsException { .. } => "sioobe",
                rustjvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException { .. } => "aioobe",
                _ => "other-runtime",
            },
            _ => "other-failed",
        }
    }

    #[test]
    fn f4_check_bounds_begin_end_in_range_returns_void() {
        let mut ctx = mock_ctx();
        let r = native_string_check_bounds_begin_end(
            &mut ctx,
            &[Value::Int(0), Value::Int(3), Value::Int(5)],
        )
        .unwrap();
        assert_eq!(r, None); // void return
    }

    #[test]
    fn f4_check_bounds_begin_end_zero_zero_zero_is_valid() {
        // Empty-string edge case used by BC PKCS12$Mappings during Provider.put
        // for keys whose substring extraction falls into a 0-length window.
        let mut ctx = mock_ctx();
        let r = native_string_check_bounds_begin_end(
            &mut ctx,
            &[Value::Int(0), Value::Int(0), Value::Int(0)],
        )
        .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn f4_check_bounds_begin_end_negative_begin_throws_sioobe() {
        let mut ctx = mock_ctx();
        let err = native_string_check_bounds_begin_end(
            &mut ctx,
            &[Value::Int(-1), Value::Int(2), Value::Int(5)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe", "must be SIOOBE not AIOOBE");
    }

    #[test]
    fn f4_check_bounds_begin_end_begin_greater_than_end_throws_sioobe() {
        let mut ctx = mock_ctx();
        let err = native_string_check_bounds_begin_end(
            &mut ctx,
            &[Value::Int(3), Value::Int(2), Value::Int(5)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }

    #[test]
    fn f4_check_bounds_begin_end_end_greater_than_length_throws_sioobe() {
        let mut ctx = mock_ctx();
        let err = native_string_check_bounds_begin_end(
            &mut ctx,
            &[Value::Int(0), Value::Int(10), Value::Int(5)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }

    #[test]
    fn f4_check_bounds_off_count_in_range_returns_offset() {
        let mut ctx = mock_ctx();
        let r = native_string_check_bounds_off_count(
            &mut ctx,
            &[Value::Int(2), Value::Int(3), Value::Int(10)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(2));
    }

    #[test]
    fn f4_check_bounds_off_count_zero_zero_zero_is_valid() {
        let mut ctx = mock_ctx();
        let r = native_string_check_bounds_off_count(
            &mut ctx,
            &[Value::Int(0), Value::Int(0), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn f4_check_bounds_off_count_negative_offset_throws_sioobe() {
        let mut ctx = mock_ctx();
        let err = native_string_check_bounds_off_count(
            &mut ctx,
            &[Value::Int(-1), Value::Int(2), Value::Int(5)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }

    #[test]
    fn f4_check_bounds_off_count_offset_plus_count_overflow_throws_sioobe() {
        // Overflow guard: offset + count must not silently overflow i32.
        // (offset=i32::MAX, count=1, length=i32::MAX) is out-of-bounds; without
        // i64 widening this would wrap to negative and slip past the check.
        let mut ctx = mock_ctx();
        let err = native_string_check_bounds_off_count(
            &mut ctx,
            &[Value::Int(i32::MAX), Value::Int(1), Value::Int(i32::MAX)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }

    #[test]
    fn f4_check_bounds_natives_registered_on_string() {
        let mut registry = NativeMethodRegistry::new();
        register_string_utf16_natives(&mut registry);
        assert!(
            registry
                .find("java/lang/String", "checkBoundsBeginEnd", "(III)V")
                .is_some(),
            "checkBoundsBeginEnd must be registered to bypass broken Preconditions stub"
        );
        assert!(
            registry
                .find("java/lang/String", "checkBoundsOffCount", "(III)I")
                .is_some(),
            "checkBoundsOffCount must be registered to bypass broken Preconditions stub"
        );
    }

    #[test]
    fn f4_code_point_at_negative_throws_sioobe_not_aioobe() {
        // Audit: native_string_code_point_at used to throw AIOOBE on out-of-range.
        // Spec mandates SIOOBE so callers' `catch (StringIndexOutOfBoundsException)`
        // handlers fire correctly.
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hi");
        let err = native_string_code_point_at(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int(-1)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }

    #[test]
    fn f4_code_point_at_too_large_throws_sioobe_not_aioobe() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hi");
        let err = native_string_code_point_at(
            &mut ctx,
            &[Value::Object(Some(s)), Value::Int(99)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&err), "sioobe");
    }
}

