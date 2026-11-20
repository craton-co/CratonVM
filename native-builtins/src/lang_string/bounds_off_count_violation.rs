#[allow(unused_imports)]
use super::*;

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
pub fn bounds_off_count_violation(offset: i32, count: i32, length: i32) -> Option<i32> {
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
// `swallowabortedupploads-unexpected-socketexception-RESOLVED.md`
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
pub fn native_string_init_from_char_array(
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
    // gc-common w20-b: a refusal (a full young space; the primitive does not
    // collect) used to be ignored, publishing a String with a null `value`.
    init_string_from_units_or_oom(ctx, this, &units, "String(char[])")?;
    Ok(None)
}

/// `java.lang.String(char[] value, int offset, int count)`
///
/// Real JDK semantics: NPE for a null array (dereferencing `.length` inside
/// `rangeCheck`), then `StringIndexOutOfBoundsException` per
/// `checkBoundsOffCount`'s spec (see [`bounds_off_count_violation`]) — both
/// checked *before* touching the array contents, matching the real
/// constructor's evaluation order.
pub fn native_string_init_from_char_array_range(
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
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_range_size(offset, count, length).into(),
        );
    }
    let mut units = vec![0u16; count as usize];
    let written = ctx.read_char_array_into(arr, offset as usize, &mut units);
    units.truncate(written);
    // gc-common w20-b: a refusal (a full young space; the primitive does not
    // collect) used to be ignored, publishing a String with a null `value`.
    init_string_from_units_or_oom(ctx, this, &units, "String(char[],int,int)")?;
    Ok(None)
}

pub fn register_string_utf16_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/lang/StringUTF16",
        "isBigEndian",
        "()Z",
        native_string_utf16_is_big_endian, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/StringUTF16",
        "getChars",
        "([BII[CI)V",
        native_string_utf16_get_chars, cratonvm_native_api::NativeKind::SyntheticStub,
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
        native_string_init_from_char_array, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/String",
        "<init>",
        "([CII)V",
        native_string_init_from_char_array_range, cratonvm_native_api::NativeKind::SyntheticStub,
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
        native_string_index_of_str_from, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/String",
        "indexOf",
        "([BBILjava/lang/String;I)I",
        native_string_index_of_static_helper, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

/// `String.indexOf(String tgt, int fromIndex)` — the public 2-arg form.
/// Cglib's TypeUtils.map sits in a tight loop calling this with a needle of
/// "[]" and incrementing `from`; if the implementation does not honor `from`
/// (or returns 0 when "[]" is absent) the loop never terminates. This is the
/// unambiguous, authoritative implementation: read both strings, search the
/// substring of `this` at offset `from`.
pub fn native_string_index_of_str_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let tgt = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // E18. E8 fixed the ONE-argument `indexOf(String)` and left this
        // overload swallowing its null into a `-1` — the same
        // indistinguishable-from-a-real-miss answer, from the sibling
        // registered twelve lines away. The two overloads even have DIFFERENT
        // helpful-NPE texts, measured on OpenJDK 25.0.3+9:
        //   "abc".indexOf(null)     -> Cannot invoke "String.coder()"  because "str"    is null
        //   "abc".indexOf(null, 0)  -> Cannot invoke "String.length()" because "tgtStr" is null
        // and the offset does NOT gate it: `indexOf(null, -1)` and
        // `indexOf(null, 99)` throw as well, so unlike `startsWith`/
        // `regionMatches` there is no short-circuit to transcribe here.
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"String.length()\" because \"tgtStr\" is null",
            ))
        }
    };
    let from = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // bug nb-lang-string: index over UTF-16 code UNITS, not Unicode code
    // points. Rust `char` is a code point, so `chars()`-based indexing is off
    // by one per preceding supplementary (>U+FFFF) char and disagrees with
    // charAt/length. E18: reading through `ctx.read_string` (a Rust `String`)
    // additionally turned every unpaired surrogate into U+FFFD on BOTH sides,
    // so two different lone surrogates matched — the scratch decode is the
    // lossless one and is what `indexOf(String)` already used.
    let result = with_two_string_chars_scratches(ctx, this, tgt, |haystack, needle| {
        index_of_units_from(haystack, needle, from)
    });
    Ok(Some(Value::Int(result)))
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
pub fn native_string_index_of_static_helper(
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

    // E18: the same scan as the two public overloads. The private copy that
    // stood here had a `saturating_sub` where they had a length guard, so a
    // needle LONGER than `srcCount` re-examined index 0 instead of failing —
    // one rule, three loops, three bounds expressions.
    let needle_units = with_string_chars_scratch(ctx, tgt, |units| units.to_vec());
    let result = index_of_units_from(&src_units, &needle_units, from_raw);
    Ok(Some(Value::Int(result)))
}

// ---------------------------------------------------------------------------
// java.lang.StringBuffer — delegates to StringBuilder natives
// ---------------------------------------------------------------------------
pub fn register_phase52_string_buffer(r: &mut NativeMethodRegistry) {
    let sb = "java/lang/StringBuffer";
    r.register(sb, "<init>", "()V", native_sb_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "<init>", "(I)V", native_sb_init_capacity, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "<init>", "(Ljava/lang/String;)V", native_sb_init_string, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sb,
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sb_init_charsequence, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_append_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_append_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(J)Ljava/lang/StringBuffer;",
        native_sb_append_long, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(D)Ljava/lang/StringBuffer;",
        native_sb_append_double, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(F)Ljava/lang/StringBuffer;",
        native_sb_append_float, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(Z)Ljava/lang/StringBuffer;",
        native_sb_append_boolean, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(C)Ljava/lang/StringBuffer;",
        native_sb_append_char, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
        native_sb_append_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/StringBuffer;",
        native_sb_append_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sb, "toString", "()Ljava/lang/String;", native_sb_to_string, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "length", "()I", native_sb_length, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "charAt", "(I)C", native_sb_char_at, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sb,
        "substring",
        "(I)Ljava/lang/String;",
        native_sb_substring, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "substring",
        "(II)Ljava/lang/String;",
        native_sb_substring_range, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "delete",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_delete, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "deleteCharAt",
        "(I)Ljava/lang/StringBuffer;",
        native_sb_delete_char_at, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_replace, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuffer;",
        native_sb_insert_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "insert",
        "(IC)Ljava/lang/StringBuffer;",
        native_sb_insert_char, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "insert",
        "(II)Ljava/lang/StringBuffer;",
        native_sb_insert_int, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sb,
        "reverse",
        "()Ljava/lang/StringBuffer;",
        native_sb_reverse, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sb, "indexOf", "(Ljava/lang/String;)I", native_sb_index_of, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sb,
        "indexOf",
        "(Ljava/lang/String;I)I",
        native_sb_index_of_from, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sb, "setCharAt", "(IC)V", native_sb_set_char_at, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "setLength", "(I)V", native_sb_set_length, cratonvm_native_api::NativeKind::Bridge);
    r.register(sb, "capacity", "()I", native_sb_capacity, cratonvm_native_api::NativeKind::Bridge);
}

