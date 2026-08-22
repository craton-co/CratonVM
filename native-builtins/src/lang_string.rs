// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! String, StringBuilder, and StringBuffer native method implementations.

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::intern_arc;
use cratonvm_types::Value;

use crate::{try_alloc_concurrent_synthetic, compile_java_regex, native_noop_with_this, obj_arg};

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
        &format!("(Ljava/lang/String;)L{class};"),
        native_sb_append_string,
    );
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/StringBuffer;)L{class};"),
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
        &format!("(I)L{class};"),
        native_sb_append_int,
    );
    registry.register(
        class,
        "append",
        &format!("(C)L{class};"),
        native_sb_append_char,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/Appendable;",
        native_sb_append_char,
    );
    // `appendCodePoint(I)` is registered ONCE, below, next to the code-point
    // family it belongs with. A second registration used to stand here, 113
    // lines earlier in this same function, pointing at
    // `native_sb_append_codepoint`; `register()` is last-write-wins, so it
    // never owned the slot. MEASURED from
    // `--dump-native-registry --explain-jdk-only --jdk-only`:
    //
    // ```text
    //   appendCodePoint (I)L<class>;   lang_string.rs:215  owns_slot=false
    //   appendCodePoint (I)L<class>;   lang_string.rs:328  owns_slot=true
    // ```
    //
    // for each of the three classes this registrar is called for — six
    // registrations, three slots. Deleting the loser is inert by construction:
    // the winner's body IS `native_sb_append_codepoint` (see
    // `native_sb_append_code_point`, which is a one-line delegation to it), so
    // the surviving registration runs exactly the code the deleted one named.
    //
    // The tree previously recorded a decision to keep the duplicate "because
    // removing it would move a census count for no behavioural gain". That
    // sentence was written before the census became the effort's success
    // metric, and it is now an argument for the deletion rather than against
    // it — `H25-3` N4.
    // JDK 21+ `repeat(int codePoint, int count)` — intercept so it operates on
    // the synthetic char[] layout instead of running real bytecode that hits
    // `ensureCapacityNewCoder` → `Arrays.copyOf([B)` over a char[] (ArrayStore).
    registry.register(
        class,
        "repeat",
        &format!("(II)L{class};"),
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
        &format!("([CII)L{class};"),
        native_sb_append_char_array_off_len,
    );
    registry.register(
        class,
        "append",
        &format!("([C)L{class};"),
        native_sb_append_char_array,
    );
    registry.register(
        class,
        "append",
        &format!("(Z)L{class};"),
        native_sb_append_boolean,
    );
    registry.register(
        class,
        "append",
        &format!("(J)L{class};"),
        native_sb_append_long,
    );
    registry.register(
        class,
        "append",
        &format!("(D)L{class};"),
        native_sb_append_double,
    );
    registry.register(
        class,
        "append",
        &format!("(F)L{class};"),
        native_sb_append_float,
    );
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/Object;)L{class};"),
        native_sb_append_object,
    );
    // C36: intercept the (CharSequence, int, int) variants used by
    // Formatter internals. Real JDK bytecode of this method reads slot
    // 2 (count) from our synthetic StringBuilder layout and blows up
    // with a bogus capacity request.
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/CharSequence;II)L{class};"),
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
        &format!("(Ljava/lang/CharSequence;)L{class};"),
        native_sb_append_charsequence,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
        native_sb_append_charsequence,
    );
    // DO NOT RETIRE for `class == "java/lang/AbstractStringBuilder"`.
    //
    // MEASURED, `javap -p --system <image> java.lang.AbstractStringBuilder` on
    // all nine supported images:
    //
    // ```text
    //   abstract class java.lang.AbstractStringBuilder implements Appendable, CharSequence {
    //     public abstract java.lang.String toString();
    // ```
    //
    // `public abstract`, no `Code`, not `ACC_NATIVE`. This registration is the
    // ONLY implementation that exists for that receiver, so the standard
    // retirement argument — "real JDK bytecode is behind it, deleting the
    // native leaves something to run" — is FALSE here. It is the third known
    // instance of `H14-1` 4's two-row "do not touch" bucket, after
    // `java/nio/file/Path.toString()` and `Path.equals(Object)`, and one data
    // point behind `H25-2`'s finding that the same shape is 1,405 registrations
    // over 193 classes registry-wide, not 2. `H25-3` R5; marked here per
    // `H25-2` N3, because a comment at the registrar is the difference between
    // a future lane retiring this row and not.
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
        &format!("(I)L{class};"),
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
        &format!("()L{class};"),
        native_sb_reverse,
    );

    // --- Mutation methods ---
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/String;)L{class};"),
        native_sb_insert_string,
    );
    registry.register(
        class,
        "insert",
        &format!("(IC)L{class};"),
        native_sb_insert_char,
    );
    registry.register(
        class,
        "insert",
        &format!("(II)L{class};"),
        native_sb_insert_int,
    );
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/Object;)L{class};"),
        native_sb_insert_object,
    );
    registry.register(
        class,
        "insert",
        &format!("(I[CII)L{class};"),
        native_sb_insert_char_array_off_len,
    );
    registry.register(
        class,
        "insert",
        &format!("(I[C)L{class};"),
        native_sb_insert_char_array,
    );
    // The four scalar `insert` overloads that had NO native. Without them real
    // `AbstractStringBuilder.insert` bytecode ran against the synthetic
    // char[]/count layout — the layout mismatch every neighbour above exists to
    // prevent. See `native_sb_insert_boolean`'s doc comment and
    // docs/known-issues/jdk-only/W7-3-format-conversions-and-stringbuilder-bounds.md.
    registry.register(
        class,
        "insert",
        &format!("(IZ)L{class};"),
        native_sb_insert_boolean,
    );
    registry.register(
        class,
        "insert",
        &format!("(IJ)L{class};"),
        native_sb_insert_long,
    );
    registry.register(
        class,
        "insert",
        &format!("(IF)L{class};"),
        native_sb_insert_float,
    );
    registry.register(
        class,
        "insert",
        &format!("(ID)L{class};"),
        native_sb_insert_double,
    );
    // The TWO REFERENCE overloads that still had no native, which is the same
    // gap the four scalars above closed and the last one on `insert`. Real
    // `AbstractStringBuilder.insert(int, CharSequence[, int, int])` bytecode
    // ran against the synthetic char[]/count layout and silently OVERWROTE
    // instead of inserting — `new StringBuilder("xy").insert(1,
    // (CharSequence) "AB")` answered `xA` where HotSpot answers `xABy` — and
    // left the receiver inconsistent, so the next `charAt` aborted the VM.
    // MEASURED both ways; see `native_sb_insert_charsequence_range`.
    //
    // A `String` argument reaches these only through a `CharSequence`-typed
    // call site, because javac picks the `(ILjava/lang/String;)` overload for
    // a `String`-typed one. Both must agree, and the 2-arg body delegates so
    // that they agree by construction rather than by two copies matching.
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/CharSequence;)L{class};"),
        native_sb_insert_charsequence,
    );
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/CharSequence;II)L{class};"),
        native_sb_insert_charsequence_range,
    );
    registry.register(
        class,
        "delete",
        &format!("(II)L{class};"),
        native_sb_delete,
    );
    registry.register(
        class,
        "deleteCharAt",
        &format!("(I)L{class};"),
        native_sb_delete_char_at,
    );
    registry.register(
        class,
        "replace",
        &format!("(IILjava/lang/String;)L{class};"),
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
    // Java 21: StringBuilder.repeat(CharSequence, int) / repeat(String, int).
    //
    // These MUST be natives: unregistered, they fall through to the real
    // `AbstractStringBuilder.repeat` → `ensureCapacityNewCoder` →
    // `Arrays.copyOf(value, …)` bytecode, which treats `value` as a
    // compact-string `byte[]`. CratonVM's StringBuilder backing is a `char[]`,
    // so the real bytecode's `System.arraycopy` copies char[]→byte[] and throws
    // `ArrayStoreException: incompatible array element types (src=Char,
    // dest=Byte)`. `java.time.format.DateTimeFormatter` uses `buf.repeat('0',
    // n)` for zero-padding, so this broke every timestamp/temporal literal
    // (35 Hibernate suite classes).
    //
    // DE-DUPLICATED 2026-08-13 (lane E38). This registrar used to register
    // `repeat` SIX times for three descriptors: each of these three lines was
    // preceded by an inline closure spelling `Ljava/lang/StringBuilder;`
    // literally instead of `L{class};`. `register()` is last-registration-wins,
    // so for a `class` of `java/lang/StringBuilder` the pairs collided and the
    // winner was decided by source order alone — the two `CharSequence`/`String`
    // closures lost harmlessly, but the `(II)` closure came AFTER its
    // `format!`-spelled sibling and won, and it was the worse body: it clamped a
    // negative count with `.max(0)` (where the JDK throws
    // `IllegalArgumentException`) and encoded through `char::from_u32`, falling
    // back to `code_point as u16` — so `repeat(0x110000, 1)` appended U+0000 and
    // `repeat(-1, 1)` appended U+FFFF where the JDK refuses both. Because the
    // losing spelling named `StringBuilder` literally, `StringBuffer` and
    // `AbstractStringBuilder` kept the correct body: the same call had two
    // answers, chosen by the receiver's static type. Removing the duplicates
    // (rather than repairing them) leaves ONE body per descriptor for all three
    // classes.
    registry.register(
        class,
        "repeat",
        &format!("(Ljava/lang/CharSequence;I)L{class};"),
        native_sb_repeat_charsequence,
    );
    // `repeat(Ljava/lang/String;I)` is NOT registered, and its absence is the
    // result of a measurement rather than an oversight.
    //
    // MEASURED, `javap -p -s --system <image> java.lang.AbstractStringBuilder`
    // over the nine supported images (JDK 17.0.20 / 21.0.12 / 25.0.4 x
    // linux/windows/macos):
    //
    // ```text
    //   JDK 17            no `repeat` of any descriptor
    //   JDK 21, JDK 25    repeat(CI)      repeat(II)      repeat(Ljava/lang/CharSequence;I)
    // ```
    //
    // There has never been a `repeat(String,int)` overload. `String` implements
    // `CharSequence`, so `sb.repeat("x", 3)` compiles to an invocation of the
    // `CharSequence` descriptor and hits the registration above; the `String`
    // spelling could not be named by any call site javac emits. Three
    // registrations (one per class this registrar is called for), each with
    // `owns_slot: true` and `invocations: 0`, that have never once executed.
    //
    // This is `H25-1` 2.2's NEAR_MISS species — an interception somebody
    // intended that silently never fires because the descriptor matches no
    // overload the image declares — and it is invisible to the census by
    // construction, so retiring it predicts a census delta of ZERO. That is a
    // PASS, not a failure.
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
    // A string whose content is not representable as Rust text cannot go
    // through the pools above: `read_string` decodes an unpaired surrogate to
    // U+FFFD and `create_string` re-encodes that substitution, so `intern()`
    // answered a DIFFERENT string than the receiver. MEASURED on both VMs at
    // `89e2c56f1`, alongside seven siblings that are all exact
    // (`substring`, `concat`, `StringBuilder.append`, `toCharArray`,
    // `indexOf`, `equals`, and String construction itself):
    //
    // ```text
    //   "a<U+D800>b".intern().charAt(1)     HotSpot d800    CratonVM fffd
    // ```
    //
    // Returning the receiver would fix that row and BREAK the contract that
    // makes `intern` worth having: `s.equals(t)` must imply
    // `s.intern() == t.intern()`, and two equal lone-surrogate strings would
    // then answer two different objects. So the unrepresentable case gets its
    // own pool, keyed on the UTF-16 units — which ARE Java's equality — with
    // the first caller's receiver becoming the canonical instance.
    let units = read_string_chars(&*ctx, this);
    if has_unpaired_surrogate(&units) {
        return Ok(Some(Value::Object(Some(intern_unrepresentable(
            ctx, this, units,
        )))));
    }

    let text = ctx.read_string(this).unwrap_or_default();
    let arc = intern_arc(&text);
    let interned = ctx.create_string(&arc);
    Ok(Some(Value::Object(Some(interned))))
}

/// The intern pool for strings Rust text cannot hold, keyed by UTF-16 units.
///
/// Values are global-root HANDLES, never `ObjectRef`s: the collector owns the
/// reference and hands back its current, possibly relocated address. Same
/// shape, and the same reason, as `net_phase_e::HttpsCarrierSession`'s cached
/// session.
///
/// Unbounded in principle, exactly as the real intern pool is — and in
/// practice bounded by how many distinct lone-surrogate strings a program
/// interns, which is a set every measurement in this tree has found empty
/// outside a test.
///
/// **One table, two callers.** `String.intern()` reaches it through
/// [`intern_unrepresentable`] below; the interpreter's `ldc` of a
/// surrogate-bearing *literal* reaches it through
/// [`surrogate_intern_probe`] / [`surrogate_intern_claim`], which exist
/// because that caller lives in the `vm` crate and holds a `SharedVm` rather
/// than a `NativeContext`. It has to be the SAME table: JVMS §5.1 interns
/// string literals, so `"\uD800" == "\uD800"` and
/// `LITERAL == LITERAL.intern()` are both required to hold, and two tables
/// would answer the second one `false`.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions —
/// [`surrogate_intern_probe`]'s read and [`surrogate_intern_claim`]'s
/// `entry().or_insert()` — copy a `usize` handle out and drop the guard in the
/// same statement, so neither can hold it across a call back into the VM.
fn surrogate_intern_pool(
) -> &'static cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<Vec<u16>, usize>> {
    static P: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<Vec<u16>, usize>>,
    > = std::sync::OnceLock::new();
    P.get_or_init(|| cratonvm_types::lock_order::OrderedMutex::new(std::collections::HashMap::new(), cratonvm_types::lock_order::LockLevel::Scratch))
}

/// The global-root handle already canonical for `units`, if any.
///
/// Read half of the pool, for a caller that owns a different root API. A hit
/// lets `ldc` skip allocating the `String` at all.
pub fn surrogate_intern_probe(units: &[u16]) -> Option<usize> {
    let pool = surrogate_intern_pool().lock().ok()?;
    pool.get(units).copied()
}

/// Publish `handle` as the canonical instance for `units`, returning whichever
/// handle won. A caller whose handle lost must release it.
///
/// Write half of [`surrogate_intern_probe`]. Takes the handle rather than the
/// `ObjectRef` for the reason the module comment gives: the collector owns the
/// reference, and a raw `ObjectRef` parked in a `static` would be a stale
/// address after the next relocating cycle.
pub fn surrogate_intern_claim(units: Vec<u16>, handle: usize) -> usize {
    match surrogate_intern_pool().lock() {
        Ok(mut pool) => *pool.entry(units).or_insert(handle),
        // A poisoned pool must not silently de-intern: returning the caller's
        // own handle keeps this call's identity self-consistent, which is the
        // same direction `intern_unrepresentable` takes on the same failure.
        Err(_) => handle,
    }
}

/// Canonical instance for a string the Rust-text pools cannot represent.
///
/// The root is taken BEFORE the table is consulted, and a loser releases its
/// own root rather than leaving it held for the life of the VM — the
/// `https_session_object` idiom. Written that way because the alternative,
/// holding the lock across `add_global_root`, is the "native holds a
/// process-global lock across a call that re-enters the VM" cycle this
/// workspace has already paid for once.
fn intern_unrepresentable(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: Vec<u16>,
) -> cratonvm_types::ObjectRef {
    // Probe before rooting: an already-interned literal (the common case once
    // `ldc` populates this table) costs one lock and no root traffic.
    if let Some(winner) = surrogate_intern_probe(&units) {
        if let Some(obj) = ctx.resolve_global_root(winner) {
            return obj;
        }
    }
    let handle = ctx.add_global_root(this);
    let winner = surrogate_intern_claim(units, handle);
    if winner != handle {
        ctx.remove_global_root(handle);
    }
    // `resolve_global_root` is the only correct way back: the winning object
    // may have been relocated since it was rooted.
    ctx.resolve_global_root(winner).unwrap_or(this)
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

    // E18. `String.equals(Object)` is
    // `(anObject instanceof String aString) && …`, and the type test is not
    // decoration here: everything below reaches the ARGUMENT's `value` field
    // by SLOT INDEX (`string_char_array` is `get_field(obj, 0)`), and
    // CratonVM's own synthetic `StringBuilder` layout is `char[] value @0`
    // too. A builder whose backing array happens to be exactly as long as the
    // receiver therefore compared EQUAL to it. That is reachable, not
    // theoretical: `new StringBuilder(3).append("abc")` has `capacity() == 3`
    // on OpenJDK 25.0.3+9 (measured), so `"abc".equals(sb)` answered `true`
    // where HotSpot answers `false` — and `equals` is what every `Map` and
    // `List.contains` in the VM ultimately calls.
    //
    // `java/lang/String` is final and bootstrap-defined, so every String in a
    // VM shares the receiver's class id: the test is one integer compare and
    // needs no name lookup on this very hot path. It is written against
    // `this`'s id rather than a resolved `java/lang/String` id deliberately —
    // if the receiver's id were ever surprising, two Strings would still agree
    // with each other and the answer would be unchanged.
    if ctx.class_id_of_object(other) != ctx.class_id_of_object(this) {
        return Ok(Some(Value::Int(0)));
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
    //
    // The needle is [`code_point_needle`]'s, not `(ch & 0xFFFF)`: a
    // supplementary `ch` is a surrogate PAIR and an invalid one matches
    // nothing. See that function for the measured rows.
    let Some(needle) = code_point_needle(ch) else {
        return Ok(Some(Value::Int(-1)));
    };
    let pos =
        with_string_chars_scratch(ctx, this, |buf| index_of_units_from(buf, needle.units(), 0));
    Ok(Some(Value::Int(pos)))
}

/// T2.2.6: `String.indexOf(int ch, int fromIndex)`.
///
/// Searches the string for the first occurrence of the given code point,
/// starting at `max(fromIndex, 0)`; a `fromIndex` past the last possible start
/// returns `-1`.
///
/// E18. The doc above used to say values outside the BMP "are matched via the
/// surrogate pair" while the body one line below it read
/// `let needle = (ch & 0xFFFF) as u16;` — the doc described the JDK and the
/// code did the opposite, so `"abc".indexOf(0x10061, 0)` answered `0` where
/// HotSpot answers `-1`. Both halves now come from [`code_point_needle`].
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

    let Some(needle) = code_point_needle(ch) else {
        return Ok(Some(Value::Int(-1)));
    };
    let pos = with_string_chars_scratch(ctx, this, |buf| {
        index_of_units_from(buf, needle.units(), from)
    });
    Ok(Some(Value::Int(pos)))
}

/// T2.2.6: `String.lastIndexOf(int ch, int fromIndex)`.
///
/// Searches backward from `min(fromIndex, len - needleWidth)` for the last
/// occurrence of `ch`. A negative `fromIndex` always returns `-1`.
///
/// E18. `len - 1` in the old doc is right only for a BMP `ch`; the JDK's
/// `lastIndexOfSupplementary` starts at `len - 2` because the pair needs two
/// units, which is why [`last_index_of_units_from`] takes the width instead of
/// assuming it. The old body masked with `& 0xFFFF`, so
/// `"abc".lastIndexOf(0x10061, 2)` answered `0` where HotSpot answers `-1`.
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

    let Some(needle) = code_point_needle(ch) else {
        return Ok(Some(Value::Int(-1)));
    };
    let pos = with_string_chars_scratch(ctx, this, |buf| {
        last_index_of_units_from(buf, needle.units(), i64::from(from))
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
/// Resolve one of `AbstractStringBuilder`'s instance fields for THIS receiver
/// from the class model, rather than from an index this file guessed.
///
/// The index heuristic these helpers used is right for exactly one of the three
/// layouts this VM meets, and `sb_set_count` already half-knew it — it mirrors
/// the count into the field NAMED `count` precisely because slot 2 is not it on
/// a modern image. `sb_state` never got the same treatment, so the writer and
/// the reader disagreed on every real-JDK-layout builder:
///
/// ```text
///   MEASURED — javap -p --system <image> java.lang.AbstractStringBuilder
///     JDK 17.0.20          value@0  coder@1  count@2
///     JDK 21.0.12          value@0  coder@1  maybeLatin1@2  count@3
///     JDK 25.0.4           value@0  coder@1  maybeLatin1@2  count@3
///   CratonVM synthetic     value(char[])@0   count@1
/// ```
///
/// so on a JDK 21 or 25 image the reader returned `maybeLatin1` — a boolean —
/// as the builder's length. MEASURED with the enforcement dial armed on
/// `java/lang/StringBuilder,java/lang/AbstractStringBuilder`: real bytecode
/// appended `"ab"` and left `count = 2` (read back through reflection), while
/// `sb.length()` answered `0` and `toString()` answered `""`. That is half of
/// the "every append silently discarded, `toString()` empty, `rc=0`" behaviour
/// `H22` measured on this registrar and could not explain.
///
/// A slot COUNT cannot decide this, which is why the lookup is by name:
/// JDK 17's `StringBuffer` also has four slots — `value, coder, count,
/// toStringCache` — and its `count` is at 2, not 3.
///
/// Returns `None` when the class model cannot answer — the unit-test
/// `MockNativeContext` has no hierarchy for `java/lang/StringBuilder`, and a
/// receiver allocated with the 2-slot synthetic layout under a class whose
/// model describes the 4-slot real one must not be indexed past its end. Every
/// caller then falls back to the historical index heuristic, so behaviour under
/// the mock and under the synthetic layout is unchanged.
fn sb_field_slot(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    name: &str,
) -> Option<usize> {
    // The 2-slot synthetic layout is unambiguous — `value@0, count@1`, nothing
    // else to confuse — and it is the hot one. Answer it without touching the
    // class model at all: `resolve_field_index_by_class_id` takes a read lock
    // on the class manager and walks the hierarchy, and `sb_state` sits on the
    // `append` path.
    let fields = ctx.object_num_fields(this);
    if fields <= 2 {
        return None;
    }
    let slot = ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), name)?;
    if slot < fields {
        Some(slot)
    } else {
        None
    }
}

/// The builder's UTF-16 content, whatever layout its `value` slot holds.
///
/// `sb_state` recognises only a `char[]` payload, and yields `None` for a
/// receiver whose `value` is the real compact `byte[]`. Every caller that
/// treats that `None` as "empty" then reports a real builder as empty. This is
/// the builder-side twin of `decode_string_chars`, which has handled all three
/// String layouts since JDK 9 compact strings landed.
///
/// Returns `None` only when there is no readable payload at all — a caller that
/// cannot proceed must REFUSE (`native_sb_char_at` throws) rather than fabricate
/// a value, because "" and 0 are answers a caller cannot tell from the truth.
pub(crate) fn sb_value_units(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Option<Vec<u16>> {
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) if ctx.object_is_array(arr) => arr,
        _ => return None,
    };
    let raw_len = ctx.array_length(arr);
    let elem = ctx.heap_element_type_of(arr);
    if elem == cratonvm_types::ArrayElementType::Char {
        let mut out = vec![0u16; raw_len];
        let written = ctx.read_char_array_into(arr, 0, &mut out[..]);
        out.truncate(written);
        return Some(out);
    }
    if !matches!(
        elem,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
    ) {
        return None;
    }
    // Real compact layout. `coder` is 1 for UTF-16 and 0 for LATIN-1; read it
    // by name, because its slot moved between the images above just as
    // `count`'s did.
    let coder_slot = sb_field_slot(ctx, this, "coder").unwrap_or(1);
    let is_utf16 = matches!(ctx.get_field(this, coder_slot), Value::Int(1));
    let mut out = Vec::with_capacity(if is_utf16 { raw_len / 2 } else { raw_len });
    if is_utf16 {
        for c in 0..raw_len / 2 {
            let lo = match ctx.get_array_element(arr, c * 2) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            let hi = match ctx.get_array_element(arr, c * 2 + 1) {
                Value::Int(v) => (v as u8) as u16,
                _ => 0,
            };
            out.push((hi << 8) | lo);
        }
    } else {
        for i in 0..raw_len {
            out.push(match ctx.get_array_element(arr, i) {
                Value::Int(v) => (v & 0xff) as u16,
                _ => 0,
            });
        }
    }
    Some(out)
}

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
    let count = match sb_field_slot(ctx, this, "count") {
        Some(slot) => match ctx.get_field(this, slot) {
            Value::Int(v) => v,
            _ => 0,
        },
        // No class model (the mock) — the historical heuristic, unchanged.
        None if ctx.object_num_fields(this) >= 3 => match ctx.get_field(this, 2) {
            Value::Int(v) => v,
            _ => 0,
        },
        None => match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        },
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
/// `object_num_fields` was the answer that fix reached for, and it is not
/// enough on its own — a slot COUNT cannot separate JDK 17's `StringBuffer`
/// (`value, coder, count, toStringCache`, count at 2) from JDK 25's
/// (`value, coder, maybeLatin1, count, toStringCache`, count at 3). Both the
/// writer here and the reader in `sb_state` now resolve the slot by NAME
/// through `sb_field_slot`, and fall back to the slot-count heuristic only
/// where there is no class model to ask.
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
    // Ask the class model where `count` is, exactly as `sb_state` now does —
    // the two must not disagree, and for two years they did. See
    // `sb_field_slot` for the three layouts and the measurement.
    if let Some(slot) = sb_field_slot(ctx, this, "count") {
        // The payload these natives maintain is a `char[]`, and
        // `native_sb_get_coder` reports LATIN1 for it, so keep the real
        // `coder` field agreeing with that answer. Resolve it by name too:
        // the previous unconditional `set_field(this, 1, 0)` was a `coder`
        // write on the JDK 17 layout and a `count` write on the 2-slot
        // synthetic one, and the unconditional `set_field(this, 2, count)`
        // wrote an int over `maybeLatin1` on every JDK 21+ image.
        if let Some(coder) = sb_field_slot(ctx, this, "coder") {
            ctx.set_field(this, coder, Value::Int(0));
        }
        ctx.set_field(this, slot, Value::Int(count));
        return;
    }
    // No class model (the mock) — the historical heuristic, unchanged.
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

    let (buf, raw_count) = sb_state(ctx, this);
    let old_cap = buf.map_or(0, |b| ctx.array_length(b));
    // Clamp to the CHAR[] capacity only when there is one. For a receiver whose
    // payload is a compact `byte[]` there is no char[] to clamp against and
    // `old_cap` is 0, so the old clamp reported every such builder as empty and
    // the grow below had nothing to preserve.
    let count = if buf.is_some() {
        (raw_count.max(0) as usize).min(old_cap)
    } else {
        raw_count.max(0) as usize
    };

    if buf.is_some() && count + additional <= old_cap {
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
        } else {
            // A receiver whose `value` is the real compact `byte[]` — a builder
            // real `AbstractStringBuilder` bytecode constructed (Mockito's
            // inline mock maker and Byte Buddy retransformation both produce
            // them), or one built while the `--jdk-only` enforcement dial was
            // armed on `java/lang/AbstractStringBuilder`.
            //
            // The `char[]` guard above is a copy CONDITION, so this arm used to
            // fall straight through to the `set_field` below and install an
            // empty `char[]` over the payload: every character already in the
            // builder was DISCARDED, silently, with no exception and `rc=0`.
            // Widen it instead — `sb_value_units` reads either layout — so the
            // conversion this function was already performing stops losing the
            // content it converts.
            let existing = sb_value_units(&*scope, this).unwrap_or_default();
            let keep = existing.len().min(count).min(new_cap);
            if !scope.write_char_array_from(new_buf, 0, &existing[..keep]) {
                for (i, &ch) in existing[..keep].iter().enumerate() {
                    scope.set_array_element(new_buf, i, Value::Int(ch as i32));
                }
            }
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
    // Code UNITS, not `read_string` + `encode_utf16`: a Rust `str` cannot hold
    // an unpaired surrogate, so `new StringBuilder(s)` for an `s` holding one
    // lone `\uDC00` built a builder containing U+FFFD — and the builder then
    // disagreed with the String it was constructed from
    // (`s.contentEquals(new StringBuilder(s))` answered false). MEASURED,
    // `scratchpad/g26/G26Builder.java` rows c1/c2/c3/c5/c7/e1/e4.
    let chars: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(s))) => read_string_chars(&*ctx, *s),
        _ => Vec::new(),
    };
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
    // `charsequence_chars`, not `invoke_to_string`: `AbstractStringBuilder`'s
    // own `(CharSequence)` constructor is `this(seq.length() + 16);
    // append(seq);`, so the characters it stores are the ones `append` reads —
    // by `charAt` for anything but the fast-path shapes, never `toString()`.
    // The units form also carries an unpaired surrogate, which the `str` this
    // replaces could not: MEASURED rows c6/c8/c9 of
    // `scratchpad/g26/G26Builder.java`.
    let chars: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(o))) => charsequence_chars(&mut *scope, *o, None)?,
        _ => Vec::new(),
    };
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
    // A NEGATIVE capacity is a throw, not a clamp. `AbstractStringBuilder(int
    // capacity)`'s whole body is the allocation `value = new byte[capacity]`
    // (or `StringUTF16.newBytesFor(capacity)`), so a negative argument fails in
    // `anewarray` with `NegativeArraySizeException` whose message is the raw
    // size. Clamping to 0 with `max(v, 0)` silently built a usable empty
    // builder instead — `new StringBuilder(-1).length()` answered 0 where
    // HotSpot throws.
    //
    // This is deliberately a DIFFERENT exception class from `setLength(-1)`'s
    // `StringIndexOutOfBoundsException`, and `RJdkBridge1`'s `sbidx` rows
    // assert both, one right after the other, precisely because the two
    // negative-length paths in this class do not agree. MEASURED on
    // jdk-25.0.3.9: `new StringBuilder(-1)` -> `NegativeArraySizeException: -1`,
    // `new StringBuffer(-7)` -> `NegativeArraySizeException: -7`.
    let cap = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v < 0 {
                return Err(
                    cratonvm_types::error::RuntimeError::NegativeArraySizeException { size: *v }
                        .into(),
                );
            }
            *v as usize
        }
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
    // Code UNITS: `sb.append(s)` must store what `s` holds, and a Rust `str`
    // cannot hold an unpaired surrogate. MEASURED, `G26Builder` rows
    // a1/a2/a12 — U+DC00 on HotSpot, U+FFFD here. The `"null"` substitution
    // for a null argument is `AbstractStringBuilder.appendNull` and is
    // unchanged.
    let chars: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(s))) => read_string_chars(&*ctx, *s),
        _ => "null".encode_utf16().collect(),
    };
    let this = sb_append_chars(ctx, this, &chars);
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

/// `AbstractStringBuilder.repeat(CharSequence cs, int count)` (JDK 25
/// `AbstractStringBuilder.java:2166-2217`).
///
/// Two contract items were missing and both were `.max(0)`-shaped silence:
///
///   * `count < 0` is `IllegalArgumentException("count is negative: " + count)`,
///     the method's only documented throw. Clamping to 0 answered "did nothing"
///     for a call the JDK refuses.
///   * a null `cs` repeats the four characters `"null"` — "If `cs` is `null`,
///     then the four characters `"null"` are repeated into this sequence" —
///     where this returned the receiver untouched.
pub(crate) fn native_sb_repeat_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let count_i32 = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if count_i32 < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("count is negative: {count_i32}"),
            }
            .into(),
        );
    }
    let count = count_i32 as usize;
    if count == 0 {
        return Ok(Some(Value::Object(Some(this))));
    }
    let cs = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        // null CharSequence -> the literal "null", per the javadoc.
        _ => None,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    // `charsequence_chars`, not `invoke_to_string`: the JDK's
    // `repeat(CharSequence, int)` appends the sequence's CHARACTERS, and code
    // units carry an unpaired surrogate where a Rust `str` cannot. MEASURED,
    // `G26Builder` rows r2/r3.
    let units: Vec<u16> = match cs {
        Some(o) => charsequence_chars(&mut *scope, o, None)?,
        None => "null".encode_utf16().collect(),
    };
    let this = scope.get(&this_handle);
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
///
/// The window check is `String.checkRange(offset, offset + len, str.length)` —
/// the IOOBE_FORMATTER sibling of the one `insert(int, char[], int, int)` uses,
/// so this overload's javadoc names the plain `IndexOutOfBoundsException` and
/// not the String one. Clamping instead (what this did) appended a SHORT slice
/// for a window that runs off the array, which reads as a successful append of
/// the wrong text.
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
    let off_i32 = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i32 = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    // i64 so a wrapped `offset + len` cannot read as in-range.
    let end_i64 = i64::from(off_i32) + i64::from(len_i32);
    if off_i32 < 0 || len_i32 < 0 || end_i64 > arr_len as i64 {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(off_i32),
                end_i64,
                arr_len as i64,
            ),
        )
        .into());
    }
    let start = off_i32 as usize;
    let copy_len = len_i32 as usize;
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
///
/// A thin lossy view of [`invoke_to_string_units_opt`], which is where the
/// dispatch lives. `String::from_utf16_lossy` over those units is what
/// `ctx.read_string` already answered for every one of this function's
/// callers, so nothing here moves.
fn invoke_to_string_opt(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Option<String>, cratonvm_types::error::MethodCallFailed> {
    Ok(invoke_to_string_units_opt(ctx, obj)?.map(|u| String::from_utf16_lossy(&u)))
}

/// [`invoke_to_string`]'s text as raw UTF-16 code units, with the same
/// `Ok(None)` for a `toString()` that really returned Java `null`.
///
/// # Why the units form is the primary one
///
/// Every arm below that produced a Rust `String` from a Java `String` went
/// through `ctx.read_string`, which cannot carry an unpaired surrogate: a
/// `str` is well-formed UTF-8, so each lone `\uD800..\uDFFF` silently became
/// U+FFFD. `sb.append((Object) s)` and `sb.insert(0, (Object) s)` therefore
/// lost a surrogate that `sb.append(char)` on the line before had kept.
///
/// MEASURED before the change (`scratchpad/g26/G26Builder.java` rows a4 and
/// i2, HotSpot 25.0.3+9-LTS as the oracle): U+DC00 there, U+FFFD here.
///
/// The wrapper and fallback arms are unchanged and simply
/// `encode_utf16` their own ASCII text — none of them can produce a
/// surrogate, so the conversion is exact and the only behavioural difference
/// is on the two arms that read a Java `String`.
fn invoke_to_string_units_opt(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Option<Vec<u16>>, cratonvm_types::error::MethodCallFailed> {
    // Fast path: if it's already a String object, read its code units.
    //
    // The class test comes FIRST and `read_string` stays as the fallback: on a
    // class the VM cannot name, `read_string`'s structural decode is still the
    // best answer available, and keeping it means this refactor cannot lose a
    // route it used to serve.
    let this_class = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if this_class == "java/lang/String" {
        return Ok(Some(read_string_chars(&*ctx, obj)));
    }
    if let Some(s) = ctx.read_string(obj) {
        return Ok(Some(s.encode_utf16().collect()));
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
                        // A boxed `Character` can hold a lone surrogate --
                        // `Character.valueOf('\ud800')` is legal -- and
                        // `char::from_u32` rejects exactly those, so this
                        // printed '?' for a value it could have passed through
                        // untouched. Emit the raw unit.
                        return Ok(Some(vec![v as u16]));
                    } else if name == "java/lang/Byte" {
                        (v as i8).to_string()
                    } else if name == "java/lang/Short" {
                        (v as i16).to_string()
                    } else {
                        // Integer
                        v.to_string()
                    };
                    return Ok(Some(formatted.encode_utf16().collect()));
                }
                Value::Long(v) => return Ok(Some(v.to_string().encode_utf16().collect())),
                // Use the Java-spec formatters (NOT raw `{}`), so a boxed Double/Float
                // rendered via String.valueOf(Object) / StringBuilder.append(Object) /
                // object string-concat matches `Double.toString` — incl. the
                // 10^-3..10^7 scientific-notation threshold, "Infinity", and "-0.0".
                // Raw `format!("{}")` dropped the ".0", printed "inf"/"-0", and never
                // used E-notation (e.g. boxed 1e7 -> "10000000.0", -0.0 -> "-0").
                Value::Float(v) => return Ok(Some(format_float(v).encode_utf16().collect())),
                Value::Double(v) => return Ok(Some(format_double(v).encode_utf16().collect())),
                _ => {} // Not a primitive wrapper
            }
        }
    }

    // A `java/nio/*CharBuffer*` special case used to sit here, reading the
    // buffer's text directly instead of asking it, because
    // `java.nio.StringCharBuffer.toString()` answered an EMPTY string through
    // this route. It is DELETED rather than kept as an optimisation: the cause
    // was a duplicate `java/nio/CharBuffer.toString()` registration shadowing
    // the correct one (see `phases_late::charset_buffers`), and a workaround
    // that keeps a fixed route unexercised is how the next regression there
    // goes unnoticed. `probes/CharBufferUsersProbe` and the route matrix in
    // fixed-suite-bugs/stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md
    // are byte-identical to HotSpot with
    // this gone.

    // Call obj.toString() via virtual dispatch. A Java exception from the
    // override is observable and must reach the caller; only an absent or
    // malformed return value uses the historical identity fallback.
    let result = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
    match result {
        // The override's answer is read as UNITS, so a `toString()` that
        // itself returns a lone surrogate is carried through rather than
        // replaced. `read_string_chars` on a non-String is empty, which is
        // why the `read_string` fallback stays for the class the VM cannot
        // name — and the descriptor guarantees a `String` here in every
        // ordinary case.
        Ok(Some(Value::Object(Some(str_ref)))) => {
            let ret_class = ctx
                .class_name_of_id(ctx.class_id_of_object(str_ref))
                .unwrap_or_default();
            if ret_class == "java/lang/String" {
                Ok(Some(read_string_chars(&*ctx, str_ref)))
            } else {
                Ok(Some(
                    ctx.read_string(str_ref)
                        .unwrap_or_else(|| "null".to_string())
                        .encode_utf16()
                        .collect(),
                ))
            }
        }
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
            Ok(Some(
                format!("{}@{:x}", name, ctx.identity_hash_code(obj))
                    .encode_utf16()
                    .collect(),
            ))
        }
        Err(err) => Err(err),
    }
}

/// [`invoke_to_string`] as raw UTF-16 code units — the text `"null"` for a
/// `toString()` that returned Java `null`, exactly as the `String` form does.
pub(crate) fn invoke_to_string_units(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Vec<u16>, cratonvm_types::error::MethodCallFailed> {
    Ok(invoke_to_string_units_opt(ctx, obj)?.unwrap_or_else(|| "null".encode_utf16().collect()))
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
    // UNITS, not text. `invoke_to_string` returns a Rust `String`, which cannot
    // hold an unpaired UTF-16 surrogate -- so `sb.append(someObject)` replaced
    // one with U+FFFD while `sb.append(someString)` right beside it did not.
    //
    // `invoke_to_string_units` is its exact twin, same `"null"` fallback and
    // all, and it sits DIRECTLY ABOVE this function. It was built by `G26` for
    // this hazard and this caller never switched to it -- the same
    // mechanism-without-a-consumer shape as `G58-1`'s `BaisEvent` and `G51-1`'s
    // `record_local_cert_chain`, except here the consumer existed and kept
    // calling the lossy one.
    //
    // This is the last mile of the collection renderers: real JDK
    // `AbstractCollection.toString` is a `sb.append(e)` loop over `Object`, so
    // `Arrays.asList(s).toString()` and any nested collection came back through
    // here and lost the unit that native-collections had just preserved.
    let units = match args.get(1) {
        Some(Value::Object(Some(obj))) => invoke_to_string_units(&mut *scope, *obj)?,
        _ => "null".encode_utf16().collect(),
    };
    let this = scope.get(&this_handle);
    let this = sb_append_chars(&mut *scope, this, &units);
    Ok(Some(Value::Object(Some(this))))
}

/// The characters a `CharSequence` exposes over `[start, end)`, read the way
/// `AbstractStringBuilder.append(CharSequence)` reads them.
///
/// The JDK fast-paths exactly two types -- `String` and `AbstractStringBuilder`
/// -- and reads **everything else through `charAt`**, never `toString()`. So a
/// sequence whose `toString()` disagrees with its characters appends its
/// CHARACTERS. CratonVM asked `toString()`, which is the same answer for the
/// common types and the wrong one for any other implementation: a `CharSequence`
/// over `"Hello, World"` whose `toString()` returns `"CUSTOM"` appended
/// `"CUSTOM"` where HotSpot appends `"Hello, World"`.
///
/// The Rust fast paths are limited to classes whose `charAt` is the JDK's own
/// and provably agrees with `toString()`: `java.lang.String`, the two
/// `AbstractStringBuilder`s, and the `java.nio` CharBuffer family (whose
/// constructors are package-private, so nothing outside `java.nio` can override
/// `charAt`). Everything else walks `charAt`, at JDK parity by construction.
///
/// `range` is `None` for the 1-arg form (the whole sequence). An exception from
/// `length()` or `charAt` propagates, as it does on HotSpot.
fn charsequence_chars(
    ctx: &mut dyn NativeContext,
    cs: cratonvm_types::ObjectRef,
    range: Option<(i32, i32)>,
) -> Result<Vec<u16>, MethodCallFailed> {
    if let Some(units) = charsequence_fast_units(ctx, cs)? {
        let n = units.len() as i32;
        let (lo, hi) = match range {
            None => (0, n),
            Some((s, e)) => {
                let lo = s.clamp(0, n);
                (lo, e.clamp(lo, n))
            }
        };
        return Ok(units[lo as usize..hi as usize].to_vec());
    }
    // GC safety: `invoke_virtual` re-enters Java and can move `cs`, so re-read
    // it through its handle on every iteration -- the same obligation the
    // `invoke_to_string` call sites in this file discharge.
    let mut scope = NativeHandleScope::new(ctx);
    let cs_handle = scope.root(cs);
    let len = {
        let cs_now = scope.get(&cs_handle);
        match (*scope).invoke_virtual(cs_now, "length", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => 0,
        }
    };
    // Clamp rather than throw, matching what this native already did for an
    // out-of-range request: the comment on `native_sb_append_charsequence_off_len`
    // records why (a JUnit error-reporting path that must not die here).
    let (lo, hi) = match range {
        None => (0, len),
        Some((s, e)) => {
            let lo = s.clamp(0, len);
            (lo, e.clamp(lo, len))
        }
    };
    let mut out: Vec<u16> = Vec::with_capacity((hi - lo).max(0) as usize);
    for i in lo..hi {
        let cs_now = scope.get(&cs_handle);
        match (*scope).invoke_virtual(cs_now, "charAt", "(I)C", &[Value::Int(i)])? {
            Some(Value::Int(c)) => out.push(c as u16),
            _ => break,
        }
    }
    Ok(out)
}

/// `CharSequence.length()`, by the same three-shapes-then-`invoke_virtual`
/// route [`charsequence_chars`] uses to read the characters.
///
/// Exists because `AbstractStringBuilder.append(CharSequence, int, int)` must
/// range-check against the sequence's length BEFORE it reads a character, and
/// `charsequence_chars` clamps rather than reports — the clamp being the whole
/// defect W7-3 left standing.
fn charsequence_length(
    ctx: &mut dyn NativeContext,
    cs: cratonvm_types::ObjectRef,
) -> Result<i32, MethodCallFailed> {
    if let Some(units) = charsequence_fast_units(ctx, cs)? {
        return Ok(units.len() as i32);
    }
    match ctx.invoke_virtual(cs, "length", "()I", &[])? {
        Some(Value::Int(n)) => Ok(n),
        _ => Ok(0),
    }
}

/// The three shapes whose text can be read in Rust without changing the answer
/// -- see [`charsequence_chars`]. `Ok(None)` means "walk `charAt`".
///
/// # Code UNITS, not a Rust `String`
///
/// A Rust `str` is well-formed UTF-8 and cannot hold an unpaired surrogate, so
/// every arm that went through one replaced each lone `\uD800..\uDFFF` with
/// U+FFFD — while the `charAt` walk in [`charsequence_chars`], the slow path
/// this function exists to skip, carried it through untouched. The two arms of
/// one function disagreed about the same sequence.
///
/// MEASURED on HotSpot 25.0.3+9-LTS and this VM before the change
/// (`scratchpad/g26/G26Builder.java`, rows a3/a5/a6/a10/a11/c6/c8/c9/r3):
/// `sb.append((CharSequence) s)` where `s` holds one `\uDC00` answered U+FFFD
/// here and U+DC00 there. Nothing threw; the substitution is unrecoverable.
/// [`sb_string_from_units`] is the same fix on the write side.
fn charsequence_fast_units(
    ctx: &mut dyn NativeContext,
    cs: cratonvm_types::ObjectRef,
) -> Result<Option<Vec<u16>>, MethodCallFailed> {
    let cid = ctx.class_id_of_object(cs);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    if name == "java/lang/String" {
        return Ok(Some(read_string_chars(&*ctx, cs)));
    }
    if name == "java/lang/StringBuilder" || name == "java/lang/StringBuffer" {
        // `sb_read_chars` is what `StringBuilder.toString()` answers on this
        // VM — `native_sb_to_string` reads the same two slots — so this is the
        // same value the `invoke_to_string` it replaces produced, minus the
        // `str` round trip and minus a Java re-entry that could move `cs`.
        return Ok(Some(sb_read_chars(&*ctx, cs)));
    }
    if name.starts_with("java/nio/") && name.contains("CharBuffer") {
        // `cb_read_text` still answers a Rust `String`; it lives in
        // `phases_late::charset_buffers`, which this lane does not own, so a
        // lone surrogate inside a `CharBuffer` is still lossy on this one arm.
        // Recorded as a NOMINATION in
        // `docs/known-issues/jdk-only/G26-1-four-families-of-RJdkIntrinsics3-20260817.md`
        // rather than fixed from here.
        return Ok(crate::phases_late::charset_buffers::cb_read_text(ctx, cs)
            .map(|t| t.encode_utf16().collect()));
    }
    Ok(None)
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
    // Producer-#12 fix: same re-entrant hazard as `native_sb_append_object`
    // just above (`charsequence_chars` calls back into Java for anything but
    // the three fast-path shapes) — pin + re-read `this`.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let chars = match args.get(1) {
        Some(Value::Object(Some(obj))) => charsequence_chars(&mut *scope, *obj, None)?,
        // `AbstractStringBuilder.appendNull`.
        _ => "null".encode_utf16().collect(),
    };
    let this = scope.get(&this_handle);
    let this = sb_append_chars(&mut *scope, this, &chars);
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
///
/// **The window is range-checked, not clamped — reversed 2026-08-12.** A prior
/// session clamped deliberately ("silent clamping keeps JUnit's
/// error-reporting path alive, and every caller inside JDK internals passes
/// in-bounds indices") and pinned the clamp with a unit test; W7-3 listed it as
/// the same species as the twelve bounds defects it fixed and declined to
/// reverse a tested, argued decision. The argument does not survive the
/// species: a clamp converts an argument the caller got wrong into one the
/// callee accepts, so `sb.append(cs, 0, 100)` on a 3-char sequence appended a
/// SHORT slice and reported success. The in-bounds callers the comment names
/// are unaffected by a check they pass, and JUnit's reporting path builds its
/// windows from `length()` — it never relied on the clamp, it merely never
/// exercised it.
///
/// JDK 25's body is `if (s == null) s = "null"; checkRange(start, end,
/// s.length());`, so the null substitution happens FIRST and the check runs
/// against 4 — `append((CharSequence) null, 0, 9)` throws. `checkRange` is the
/// `IndexOutOfBoundsException` sibling of `checkRangeSIOOBE`, i.e. the same
/// check [`native_sb_append_char_array_off_len`] runs, and the message helper
/// is shared with it so the two overloads cannot drift. Only the exception
/// CLASS is pinned by the javadoc; the text is `Preconditions`' rather than
/// `checkRange`'s hand-rolled wording, unverified against JDK 25 — the same
/// caveat W7-3 records for `setLength(-1)`.
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

    // Producer-#12 fix: the length probe and the charAt walk both re-enter
    // Java and can move `this` and the sequence, so both are rooted before
    // either runs.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let cs_handle = cs.map(|o| scope.root(o));

    // `s == null` becomes the four-character "null" BEFORE the range check, so
    // the length the check runs against is 4 and not the caller's window.
    let length = match &cs_handle {
        Some(handle) => {
            let cs_now = scope.get(handle);
            charsequence_length(&mut *scope, cs_now)?
        }
        None => 4,
    };
    // `checkRange(start, end, length)`.
    if start < 0 || start > end || end > length {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(start),
                i64::from(end),
                i64::from(length),
            ),
        )
        .into());
    }

    let Some(cs_handle) = cs_handle else {
        // null CharSequence → append "null" per AbstractStringBuilder.appendNull,
        // over the window the check above admitted.
        let this = scope.get(&this_handle);
        let text: String = "null"[start as usize..end as usize].to_string();
        let this = sb_append_str(&mut *scope, this, &text);
        return Ok(Some(Value::Object(Some(this))));
    };

    // Read the sequence's CHARACTERS over [start, end) — `charsequence_chars`
    // states why `toString()` is the wrong question here. Its own clamp is now
    // unreachable from this caller: the range was validated above.
    let cs_obj = scope.get(&cs_handle);
    let chars = charsequence_chars(&mut *scope, cs_obj, Some((start, end)))?;
    let this = scope.get(&this_handle);
    let this = sb_append_chars(&mut *scope, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// True when `units` holds a surrogate code unit that is NOT part of a
/// well-formed high+low pair — i.e. exactly the content a Rust `str` cannot
/// represent.
pub(crate) fn has_unpaired_surrogate(units: &[u16]) -> bool {
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u) {
            if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                i += 2;
                continue;
            }
            return true;
        }
        if (0xDC00..=0xDFFF).contains(&u) {
            return true;
        }
        i += 1;
    }
    false
}

/// Materialise a fresh (uninterned) Java `String` from raw UTF-16 code units.
///
/// `String::from_utf16_lossy` — which every `String`-returning builder native
/// used to end in — cannot carry an UNPAIRED surrogate: a Rust `str` is
/// well-formed UTF-8, so each lone `\uD800..\uDFFF` silently becomes U+FFFD.
/// Nothing threw, and the substitution is unrecoverable: after
/// `sb.append((char) 0xD800)`, `sb.charAt(0)` answered 0xD800 while
/// `sb.toString().charAt(0)` answered 0xFFFD — the builder and its own
/// `toString` disagreed about their contents.
///
/// The lossless reader for the other direction ([`read_string_chars`]) has been
/// in this file all along; this is its write-side twin.
///
/// The ordinary case is byte-for-byte the previous behaviour: well-formed units
/// still go through `create_string_uninterned_gc_safe(&str)`, so no allocation
/// path, interning rule or GC-safety property moves for text that has no lone
/// surrogate. Only a slice that actually contains one takes the units path —
/// `new_object("java/lang/String")` plus `init_string_from_units`, the same
/// pair `native_string_init_from_char_array` uses, documented as preserving
/// "raw code units byte-for-byte (including unpaired surrogates)". Both of
/// those allocate with the non-triggering `try_alloc_*` primitives, but the
/// fresh String is not reachable from any Java root, so it is pinned across
/// them anyway.
pub(crate) fn sb_string_from_units(
    ctx: &mut dyn NativeContext,
    units: &[u16],
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if !has_unpaired_surrogate(units) {
        let text = String::from_utf16_lossy(units);
        return Ok(ctx.create_string_uninterned_gc_safe(&text));
    }
    let obj = match ctx.new_object("java/lang/String")? {
        Some(Value::Object(Some(o))) => o,
        // Allocation refused without reporting a failure: fall back to the
        // lossy form rather than hand a null out of a String-typed method.
        _ => {
            let text = String::from_utf16_lossy(units);
            return Ok(ctx.create_string_uninterned_gc_safe(&text));
        }
    };
    let h = ctx.pin_native_root(obj);
    let obj = ctx.read_native_pin(h, obj);
    let ok = ctx.init_string_from_units(obj, units);
    let obj = ctx.read_native_pin(h, obj);
    ctx.unpin_native_roots(h);
    if !ok {
        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
            message: "Java heap space (String from UTF-16 units)".to_string(),
        }
        .into());
    }
    Ok(obj)
}

pub(crate) fn native_sb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (buf, count) = sb_state(ctx, this);
    let count = count.max(0) as usize;
    // Read chars and build Rust string
    let chars: Vec<u16> = match buf {
        Some(buf) => {
            let count = count.min(ctx.array_length(buf));
            let mut chars = Vec::with_capacity(count);
            for i in 0..count {
                let ch = match ctx.get_array_element(buf, i) {
                    Value::Int(v) => v as u16,
                    _ => 0,
                };
                chars.push(ch);
            }
            chars
        }
        // NOT `""`. A receiver whose `value` is the real compact `byte[]` has a
        // payload this function can read perfectly well — it is the same
        // decode `decode_string_chars` has always done for `String` — and
        // answering "" for it is a FABRICATION, not a refusal: the caller
        // cannot tell an empty builder from one whose content this native
        // declined to look at. That answer is the visible half of the "every
        // append silently discarded, `toString()` empty, `rc=0`" behaviour
        // `H22` measured on this registrar.
        //
        // A receiver with no readable payload at all still yields "", which is
        // what a freshly-allocated builder legitimately holds.
        None => {
            let mut units = sb_value_units(ctx, this).unwrap_or_default();
            units.truncate(count.min(units.len()));
            units
        }
    };
    // `StringBuilder.toString()` must return a *fresh* String distinct from
    // any equal literal — the JVM spec only pools literals and `intern()`.
    // Routing it through the interned pool made `==` wrongly report identity
    // (e.g. `sb.toString() == "literal"`), breaking identity-based symbol
    // comparisons such as xerces' `NamespaceSupport`.
    // `_gc_safe` (inside `sb_string_from_units`): the units are already
    // Rust-owned; `this`/`buf` are not dereferenced again below, so a moving
    // young GC here is safe. Without this, a StringBuilder.toString()-heavy hot
    // loop (e.g. Response.toAbsolute()) hard-aborts the whole process on
    // young-gen exhaustion instead of collecting and continuing -- see
    // docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md.
    let result = sb_string_from_units(ctx, &chars)?;
    Ok(Some(Value::Object(Some(result))))
}

/// AbstractStringBuilder.getChars(int srcBegin, int srcEnd, char[] dst, int dstBegin)
///
/// Copies characters from the builder's buffer into `dst` starting at
/// `dstBegin`.
///
/// **The two range checks throw DIFFERENT exception classes, and the JDK says
/// so in one line each.** `AbstractStringBuilder.getChars` is:
///
/// ```text
/// Preconditions.checkFromToIndex(srcBegin, srcEnd, count, Preconditions.SIOOBE_FORMATTER);
/// int n = srcEnd - srcBegin;
/// Preconditions.checkFromToIndex(dstBegin, dstBegin + n, dst.length, Preconditions.IOOBE_FORMATTER);
/// ```
///
/// so a bad SOURCE window is a `StringIndexOutOfBoundsException` while a bad
/// DESTINATION window is the PLAIN `IndexOutOfBoundsException` — not the
/// `ArrayIndexOutOfBoundsException` the element stores below would otherwise
/// suggest, and not the `StringIndexOutOfBoundsException` its own source-side
/// neighbour throws. Both wrong choices are subclasses of the class the JDK
/// actually throws, so a test that catches the SUPERTYPE cannot tell the
/// difference; `RJdkBridge1`'s `sbidx` rows assert the EXACT class and can.
/// MEASURED on jdk-25.0.3.9 — see
/// `docs/known-issues/jdk-only/G53-1-the-exact-exception-class-and-the-rest-of-sbidx-20260817.md`.
///
/// The ORDER is load-bearing too, and is four checks deep:
/// 1. the source check runs FIRST, so `getChars(3, 1, null, 0)` is a
///    `StringIndexOutOfBoundsException` and never reaches the null `dst`;
/// 2. `dst.length` is then read, so a null `dst` is a `NullPointerException`
///    even when `n == 0` — `getChars(0, 0, null, 0)` throws;
/// 3. only then is the destination window checked.
///
/// Note `java.lang.String.getChars` does NOT share this split: it checks both
/// windows with `SIOOBE_FORMATTER`, so its too-small-destination case is a
/// `StringIndexOutOfBoundsException`. It is a different contract in a different
/// class, and this file does not serve it (`String.getChars` has no native
/// registration at all — real JDK bytecode runs). Do not unify the two.
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
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    let (buf, count) = sb_state(ctx, this);
    // (1) Source-range check FIRST, with `SIOOBE_FORMATTER`. This precedes the
    // null-`dst` dereference, which is why a bad source window beats a null
    // destination rather than the other way round.
    if src_begin < 0 || src_end > count || src_begin > src_end {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(src_begin, src_end, count).into());
    }
    let n = (src_end - src_begin) as usize;
    // (2) `dst.length` is read next, so a null `dst` is a NullPointerException —
    // unconditionally, including the `n == 0` case where nothing would be
    // copied. HotSpot's helpful-NPE wording names the array being read.
    let dst = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Cannot read the array length because \"dst\" is null".to_string()),
            }
            .into())
        }
    };
    // (3) Destination-range check, with `IOOBE_FORMATTER`: the PLAIN
    // `IndexOutOfBoundsException`, carrying `checkFromToIndex`'s range wording
    // over the DESTINATION window `[dstBegin, dstBegin + n)`.
    let dst_len = ctx.array_length(dst) as i64;
    // Use widening i64 arithmetic so `dstBegin + n` cannot wrap (n is bounded by
    // the validated source window, so it fits in i32, but stay defensive).
    let copy_end = i64::from(dst_begin) + (n as i64);
    if dst_begin < 0 || copy_end > dst_len {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(dst_begin),
                copy_end,
                dst_len,
            ),
        )
        .into());
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
    // `buf.unwrap()` here ABORTED THE VM. A builder whose slot 0 is not a
    // `char[]` reaches this with `count > 0` and `buf == None`, and a Rust
    // panic is not a Java throwable: it terminates the process instead of
    // unwinding to the `catch` the caller wrote.
    //
    // MEASURED reproducer before the fix (`scratchpad/g26/G26Builder.java`
    // row i6): `new StringBuilder("xy").insert(1, (CharSequence) s)` had no
    // native, so real `AbstractStringBuilder` bytecode ran against this VM's
    // synthetic `char[]`/`count` layout and left the receiver inconsistent;
    // the next `charAt` panicked at this line —
    // `thread 'main-vm' panicked ... called Option::unwrap() on a None value`,
    // exit without a stack trace. The registration gap is closed below
    // (`insert(int, CharSequence)` and its 4-arg sibling), so nothing in the
    // suite reaches this arm any more; it stays because the guard must not
    // depend on that.
    //
    // `sioobe_index` is the same refusal the bounds arm above raises, so a
    // caller sees one class for "this index is not readable" either way. Its
    // message is TRANSCRIBED from HotSpot and asserted by the probe rows
    // n21/n22/n23 (`Index 5 out of bounds for length 2`), which match today
    // and must keep matching.
    let Some(buf) = buf else {
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    };
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
///
/// It opens with `checkRangeSIOOBE(beginIndex, endIndex, count)`; unlike
/// `delete`/`replace` there is no clamp before it, so an over-long `endIndex`
/// throws rather than counting to the end (which is what clamping here did).
pub(crate) fn native_sb_code_point_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let begin_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let end_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    // NOT `sb_check_from_to_index` — that helper throws the
    // `StringIndexOutOfBoundsException` its `delete`/`replace`/`substring`
    // callers want, and `codePointCount` is the one row in this class that
    // wants the PLAIN superclass. The JDK passes a null formatter here:
    //
    // ```text
    // Preconditions.checkFromToIndex(beginIndex, endIndex, count, null);
    // ```
    //
    // against `SIOOBE_FORMATTER` two methods away, and a null formatter yields
    // `java.lang.IndexOutOfBoundsException`. `RJdkBridge1`'s
    // `sbidx-step=codePointCount(0, len+1)` asserts the exact class, so the
    // subclass does not pass even though `catch (IndexOutOfBoundsException)`
    // would not notice. MEASURED on jdk-25.0.3.9.
    let count_i32 = chars.len() as i32;
    if begin_i32 < 0 || begin_i32 > end_i32 || end_i32 > count_i32 {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(begin_i32),
                i64::from(end_i32),
                i64::from(count_i32),
            ),
        )
        .into());
    }
    let begin = begin_i32 as usize;
    let end = end_i32 as usize;
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
///
/// **The truncation W7-3 left standing is gone, and the argument that justified
/// it was resolvable rather than a trade-off.** The old body ran
/// `char::from_u32`, which refuses an unpaired surrogate because it is not a
/// Unicode SCALAR value, and truncated on that refusal — with a comment saying
/// `Character.toChars` would throw for these but WHATWG callers must not die.
/// Both halves of that sentence are wrong about the JDK: `appendCodePoint`'s
/// first statement is `if (Character.isBmpCodePoint(codePoint)) return
/// append((char) codePoint);`, and `0xD800..=0xDFFF` **are** BMP code points,
/// so `Character.toChars` is never reached and an unpaired surrogate is
/// appended verbatim. The WHATWG path therefore keeps working by the JDK's own
/// rule instead of by a local exemption, and the only inputs left for the
/// truncation to cover are the ones the JDK genuinely refuses:
/// `codePoint < 0 || codePoint > 0x10FFFF`, where `Character.toChars` throws
/// `IllegalArgumentException`. Truncating those wrote `cp as u16` — a
/// low-order-16-bits alias of a number that is not a code point at all, i.e. a
/// silent wrong character where the caller asked for a refusal.
///
/// `char::from_u32` was not the predicate for this method: it answers "is this
/// a Rust `char`", which excludes exactly the surrogates the JDK admits.
///
/// **The correct expansion was already in this file, and this registration was
/// shadowing it.** `register_string_builder_natives` registers
/// `appendCodePoint(I)` TWICE — first to [`native_sb_append_codepoint`], which
/// delegates to [`native_sb_repeat_codepoint`]'s three-way expansion (BMP
/// verbatim including surrogates / surrogate pair / `IllegalArgumentException`,
/// which is the JDK's rule exactly), and then to this function, whose body
/// truncated. `register()` is last-registration-wins, so the truncating copy
/// owned the slot and the correct one never ran — the `Integer.toString(II)`
/// shape from docs/architecture/natives-over-real-jdk-classes.md §3, inside one
/// registrar function. This body is now the delegation, so there is one
/// expansion rule in the file rather than two that disagree; the duplicate
/// registration is left in place because removing it would move a census count
/// for no behavioural gain.
pub(crate) fn native_sb_append_code_point(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_sb_append_codepoint(ctx, args)
}

/// `AbstractStringBuilder.reverse()` (JDK 25
/// `AbstractStringBuilder.java:1696-1733`, delegating to
/// `StringUTF16.reverse`).
///
/// The javadoc is explicit that this is NOT a plain code-unit reversal, which
/// is what this used to be:
///
/// > Causes this character sequence to be replaced by the reverse of the
/// > sequence. **If there are any surrogate pairs included in the sequence,
/// > these are treated as single characters for the reverse operation. Thus,
/// > the order of the high-low surrogates is never reversed.**
///
/// > Note that the reverse operation may result in producing surrogate pairs
/// > that were unpaired low-surrogates and high-surrogates before the
/// > operation. For example, reversing `"\uDC00\uD800"` produces
/// > `"𐀀"` which is a valid surrogate pair.
///
/// The JDK does it in two passes and so does this: reverse every code unit,
/// then walk the result and swap back any `(low, high)` neighbour — that
/// neighbour is exactly a pair the first pass inverted. The second pass runs
/// only when the first saw a surrogate, and it advances TWO units after a swap
/// (`putChar(val, i++, c1)`), so `"\uDC00\uD800"` in the *input* is left as the
/// valid pair the javadoc promises rather than being swapped a second time.
pub(crate) fn native_sb_reverse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (buf, count) = sb_state(ctx, this);
    if let Some(buf) = buf {
        let count = count.max(0) as usize;
        let mut units: Vec<u16> = Vec::with_capacity(count);
        let count = count.min(ctx.array_length(buf));
        for i in 0..count {
            units.push(match ctx.get_array_element(buf, i) {
                Value::Int(c) => c as u16,
                _ => 0,
            });
        }
        let had_surrogate = units.iter().any(|&u| (0xD800..=0xDFFF).contains(&u));
        units.reverse();
        if had_surrogate {
            let mut i = 0usize;
            while i + 1 < units.len() {
                // A LOW surrogate followed by a HIGH one is a pair the
                // reversal inverted; put it back.
                if (0xDC00..=0xDFFF).contains(&units[i])
                    && (0xD800..=0xDBFF).contains(&units[i + 1])
                {
                    units.swap(i, i + 1);
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        for (i, &u) in units.iter().enumerate() {
            ctx.set_array_element(buf, i, Value::Int(i32::from(u)));
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
        return chars;
    }
    // `sb_state` yields `None` for a builder whose `value` is the real compact
    // `byte[]`, and returning an empty vector here was NOT a refusal — it was a
    // wrong answer with a plausible shape.
    //
    // The caller that matters is `native_string_init_abstract_string_builder`,
    // i.e. `String(AbstractStringBuilder, Void)`. MEASURED, `javap -p -c
    // --system <jdk-25> java.lang.StringBuilder`, that IS `toString()`:
    //
    // ```text
    //    1: invokevirtual  Method length:()I
    //    4: ifne  10
    //    7: ldc   String ""            // the empty-builder fast path
    //   16: invokespecial Method java/lang/String."<init>":(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V
    // ```
    //
    // so any builder that real `AbstractStringBuilder` bytecode constructed —
    // which the doc comment on that constructor already names ("Byte Buddy can
    // execute that real bytecode while retransformation is in progress") —
    // stringified to "". No exception, `rc=0`, and `length()` answering the
    // right number the whole time.
    //
    // MEASURED reproduction with the `--jdk-only` enforcement dial armed on
    // `java/lang/StringBuilder,java/lang/AbstractStringBuilder`:
    // `value = byte[16]`, `coder = 0`, `count = 2`, `sb.length() == 2`, and
    // `sb.toString().length() == 0` with a `byte[0]` behind it. That is the
    // "every append silently discarded, `toString()` empty, `rc=0`" behaviour
    // `H22` measured on `register_string_builder_natives`, and this is where it
    // was produced.
    //
    // `sb_value_units` reads either layout, so the answer is now the builder's
    // actual content in both.
    let mut units = sb_value_units(ctx, this).unwrap_or_default();
    units.truncate(count.min(units.len()));
    units
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

/// `insert(int, String)` — insert a string at `offset`.
///
/// `checkOffset(offset, count)` first (see [`sb_check_offset`]); the null
/// substitution is second, and unlike `replace` this overload really does
/// substitute "null" rather than throwing.
pub(crate) fn native_sb_insert_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // Code UNITS — `G26Builder` row i1. See `native_sb_append_string`.
    let insert_chars: Vec<u16> = match args.get(2) {
        Some(Value::Object(Some(obj))) => read_string_chars(&*ctx, *obj),
        _ => "null".encode_utf16().collect(),
    };

    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `insert(int, CharSequence, int, int)` — JDK 25's four-argument insert.
///
/// # Why this has to exist at all
///
/// It had NO native, so real `AbstractStringBuilder` bytecode ran — and that
/// bytecode writes the JDK's compact `byte[] value` / `byte coder` / `int
/// count` layout, which this VM's builders do not have (slot 0 is a `char[]`,
/// slot 1 is the count). The same layout mismatch the ten registrations above
/// exist to prevent. MEASURED before the fix
/// (`scratchpad/g26/G26Insert.java`, HotSpot 25.0.3+9-LTS as the oracle):
///
/// | call | HotSpot | before |
/// |---|---|---|
/// | `new StringBuilder("xy").insert(1, (CharSequence) "AB")` | `xABy` | `xA` |
/// | `…insert(1, (CharSequence) sb)` | `xABy` | `xA` |
/// | `…insert(1, seq, 0, 1)` | `xPy` | `xP` |
/// | `…insert(1, (CharSequence) null)` | `xnully` | `xn` |
/// | `new StringBuffer("xy").insert(1, (CharSequence) "AB")` | `xABy` | `xA` |
///
/// It did not throw — it OVERWROTE and truncated, which reads as a successful
/// insert of the wrong text. Worse, it left the receiver's count and buffer
/// inconsistent, and the next `charAt` on it hit an `unwrap()` in
/// [`native_sb_char_at`] and **aborted the VM** (`G26Builder` row i6). That
/// `unwrap` is now a throw, but the registration is the actual repair.
///
/// # The contract, TRANSCRIBED
///
/// Every row below is measured on HotSpot 25.0.3+9-LTS
/// (`scratchpad/g26/G26InsCs.java`), not derived from the javadoc. The two
/// refusals are DIFFERENT classes and the order between them is observable:
///
/// | call, on `new StringBuilder("xy")` | HotSpot |
/// |---|---|
/// | `insert(5, "AB", 0, 1)` | `StringIndexOutOfBoundsException: Range [5, 2) out of bounds for length 2` |
/// | `insert(-1, "AB", 0, 1)` | `StringIndexOutOfBoundsException: Range [-1, 2) out of bounds for length 2` |
/// | `insert(1, "AB", 0, 9)` | `IndexOutOfBoundsException: Range [0, 9) out of bounds for length 2` |
/// | `insert(1, "AB", -1, 1)` | `IndexOutOfBoundsException: Range [-1, 1) out of bounds for length 2` |
/// | `insert(1, "AB", 2, 1)` | `IndexOutOfBoundsException: Range [2, 1) out of bounds for length 2` |
/// | `insert(9, "AB", 0, 9)` — BOTH wrong | the `StringIndex…` one: the offset check is first |
/// | `insert(1, null, 0, 9)` | `IndexOutOfBoundsException: Range [0, 9) out of bounds for length 4` |
/// | `insert(1, null, 0, 4)` | no throw, `xnully` |
/// | `insert(5, cs)` whose `length()` throws | that `IllegalStateException` — see the 2-arg form |
/// | `insert(5, cs, 0, 1)` whose `length()` throws | the offset `StringIndex…`; `length()` is never called |
/// | `insert(1, "AB", 1, 1)` | no throw, `xy` — an empty window is legal |
///
/// So the order is: substitute `"null"`, then `checkOffset` (a
/// `StringIndexOutOfBoundsException`), then `checkRange` against
/// `s.length()` (the PLAIN `IndexOutOfBoundsException`). The null
/// substitution happens before the range check, which is why the length in
/// the `insert(1, null, 0, 9)` message is 4 and not the receiver's 2 — the
/// same rule [`native_sb_append_charsequence_off_len`] records, and the same
/// two message helpers, so the two overloads cannot drift.
pub(crate) fn native_sb_insert_charsequence_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, dstOffset, s, start, end]
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let cs = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let start = match args.get(3) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let end = match args.get(4) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };

    // GC: `charsequence_length` and `charsequence_chars` both re-enter Java
    // for anything but the fast-path shapes, so the receiver AND the sequence
    // are rooted before either runs — the obligation every re-entrant native
    // in this file discharges.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let cs_handle = cs.map(|o| scope.root(o));

    // 1. checkOffset(dstOffset, count) — BEFORE the sequence is touched at
    //    all. The `Thrower` row is what pins this ordering: a `length()` that
    //    throws is never reached when the offset is already wrong.
    let this_now = scope.get(&this_handle);
    let count = sb_read_chars(&*scope, this_now).len() as i32;
    if let Some(failure) = sb_check_offset(offset, count) {
        return Err(failure);
    }

    // 2. checkRange(start, end, s.length()) — a null sequence is the four
    //    characters "null", so the length checked against is 4.
    let s_len = match &cs_handle {
        Some(handle) => {
            let cs_now = scope.get(handle);
            charsequence_length(&mut *scope, cs_now)?
        }
        None => 4,
    };
    if start < 0 || end < start || end > s_len {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(start),
                i64::from(end),
                i64::from(s_len),
            ),
        )
        .into());
    }

    // 3. The window's CHARACTERS. `charsequence_chars` reads them by `charAt`
    //    for every implementation but the fast-path shapes, which is what the
    //    JDK does, and answers code units so an unpaired surrogate survives.
    let insert_chars: Vec<u16> = match &cs_handle {
        Some(handle) => {
            let cs_now = scope.get(handle);
            charsequence_chars(&mut *scope, cs_now, Some((start, end)))?
        }
        None => "null".encode_utf16().collect::<Vec<u16>>()[start as usize..end as usize].to_vec(),
    };

    // 4. Splice. The receiver is re-read because steps 2 and 3 may have run
    //    arbitrary Java; a `length()`/`charAt` that mutated the receiver could
    //    have shortened it under us, and slicing on the stale `offset` would
    //    panic. Refusing with the same class the offset check uses keeps a
    //    pathological sequence from aborting the VM.
    let this_now = scope.get(&this_handle);
    let chars = sb_read_chars(&*scope, this_now);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this_now = sb_write_chars(&mut *scope, this_now, &result);
    Ok(Some(Value::Object(Some(this_now))))
}

/// `insert(int, CharSequence)` — JDK 25's two-argument insert.
///
/// The body is `if (s == null) s = "null"; if (s instanceof String) return
/// insert(dstOffset, (String) s); return insert(dstOffset, s, 0, s.length());`
/// and BOTH branches are observable:
///
/// * a `String` (and a null, which becomes one) takes the `insert(int,
///   String)` path, so `length()` is never called on it;
/// * anything else evaluates `s.length()` as an ARGUMENT of the four-argument
///   call, i.e. **before** that call's offset check. MEASURED: on
///   `new StringBuilder("xy")`, `insert(5, seq)` whose `length()` throws
///   answers that `IllegalStateException`, while `insert(5, seq, 0, 1)`
///   answers the offset `StringIndexOutOfBoundsException`. Two calls, same
///   receiver, same bad offset, same sequence, different exceptions —
///   transcribed rather than derived, because either ordering looks equally
///   reasonable from the javadoc.
pub(crate) fn native_sb_insert_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, dstOffset, s]
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let cs = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        // `s == null` becomes the String "null", and a String goes to the
        // `insert(int, String)` overload — which already substitutes "null"
        // for a null argument, after its own `checkOffset`.
        _ => return native_sb_insert_string(ctx, args),
    };
    let name = ctx
        .class_name_of_id(ctx.class_id_of_object(cs))
        .unwrap_or_default();
    if name == "java/lang/String" {
        return native_sb_insert_string(ctx, args);
    }

    // GC: `charsequence_length` re-enters Java, so both refs are rooted and
    // re-read before they are handed to the four-argument body.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let cs_handle = scope.root(cs);
    let len = {
        let cs_now = scope.get(&cs_handle);
        charsequence_length(&mut *scope, cs_now)?
    };
    let this_now = scope.get(&this_handle);
    let cs_now = scope.get(&cs_handle);
    native_sb_insert_charsequence_range(
        &mut *scope,
        &[
            Value::Object(Some(this_now)),
            Value::Int(offset),
            Value::Object(Some(cs_now)),
            Value::Int(0),
            Value::Int(len),
        ],
    )
}

/// `insert(int, char)` — insert a single char. `checkOffset(offset, count)`.
pub(crate) fn native_sb_insert_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let ch = match args.get(2) {
        Some(Value::Int(c)) => *c as u16,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + 1);
    result.extend_from_slice(&chars[..offset]);
    result.push(ch);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `insert(int, int)` — insert an int as its string. `checkOffset(offset, count)`.
pub(crate) fn native_sb_insert_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let val = match args.get(2) {
        Some(Value::Int(v)) => v.to_string(),
        _ => "0".to_string(),
    };
    let insert_chars: Vec<u16> = val.encode_utf16().collect();
    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `insert(int, Object)` — insert an Object via toString.
/// `checkOffset(offset, count)`.
pub(crate) fn native_sb_insert_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // `insert(int, Object)` is `insert(dstOffset, String.valueOf(obj))`, so
    // the object must be ASKED. This read `ctx.read_string(obj)` instead,
    // which answers `None` for everything that is not a `java.lang.String` —
    // so every other object was inserted as the four characters `"null"`.
    // MEASURED (`scratchpad/g26/G26Insert.java`, HotSpot 25.0.3+9-LTS):
    //
    // | call | HotSpot | before |
    // |---|---|---|
    // | `insert(0, Integer.valueOf(7))` | `7ab` | `nullab` |
    // | `insert(0, obj with toString()="CUSTOM")` | `CUSTOMab` | `nullab` |
    // | `insert(0, Boolean.TRUE)` | `trueab` | `nullab` |
    // | `insert(0, Double.valueOf(1.5))` | `1.5ab` | `nullab` |
    // | `insert(0, new int[]{1})` | `[I@…ab` | `nullab` |
    //
    // `append(Object)` two hundred lines up already used `invoke_to_string`
    // and already measured correct — the sibling overload, same file, one
    // helper apart. The units form is [`invoke_to_string`]'s, so a
    // `toString()` answering a lone surrogate survives too (row i2).
    //
    // GC: `invoke_to_string_units` re-enters Java and can move `this`, so the
    // receiver is pinned across it — the same obligation
    // `native_sb_append_object` discharges.
    let mut scope = NativeHandleScope::new(ctx);
    let this_handle = scope.root(this);
    let insert_chars: Vec<u16> = match args.get(2) {
        Some(Value::Object(Some(obj))) => invoke_to_string_units(&mut *scope, *obj)?,
        Some(Value::Int(v)) => v.to_string().encode_utf16().collect(),
        Some(Value::Long(v)) => v.to_string().encode_utf16().collect(),
        _ => "null".encode_utf16().collect(),
    };
    let this = scope.get(&this_handle);
    let chars = sb_read_chars(&*scope, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(&mut *scope, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// The splice every scalar `insert(int, X)` overload performs: `checkOffset`,
/// then insert `text` at `offset`.
///
/// Extracted so the four overloads below are one line each and cannot drift
/// from the six that already had natives — JDK 25 writes them the same way
/// (`insert(int offset, long l) { return insert(offset, String.valueOf(l)); }`),
/// so the check, the bound and the splice are shared by construction rather
/// than by four copies agreeing.
fn sb_insert_text(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    offset: i32,
    text: &str,
) -> MethodCallResult {
    let insert_chars: Vec<u16> = text.encode_utf16().collect();
    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// Read `args[1]` as an `insert` destination offset.
fn sb_insert_offset(args: &[Value]) -> i32 {
    match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

/// `insert(int, boolean)`.
///
/// **These four overloads had no native at all**, which is not the same defect
/// as the clamps W7-3 fixed: with nothing registered, real
/// `AbstractStringBuilder.insert` bytecode ran against CratonVM's synthetic
/// `char[] value` @0 / `int count` @1 layout, reading slot 2 as `count` and
/// slot 0 as a compact `byte[]` — the layout mismatch every other member of
/// this family has a native to prevent (see `native_sb_get_coder`). W7-3
/// recorded them as "a layout bug, not a bounds bug, and not in this lane".
pub(crate) fn native_sb_insert_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = match args.get(2) {
        Some(Value::Int(v)) if *v != 0 => "true",
        _ => "false",
    };
    sb_insert_text(ctx, this, sb_insert_offset(args), text)
}

/// `insert(int, long)`.
pub(crate) fn native_sb_insert_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    };
    sb_insert_text(ctx, this, sb_insert_offset(args), &v.to_string())
}

/// `insert(int, float)` — rendered by `Float.toString`, not by Rust's
/// `Display`; see [`format_float`].
pub(crate) fn native_sb_insert_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match args.get(2) {
        Some(Value::Float(v)) => *v,
        Some(Value::Double(v)) => *v as f32,
        _ => 0.0,
    };
    sb_insert_text(ctx, this, sb_insert_offset(args), &format_float(v))
}

/// `insert(int, double)` — rendered by `Double.toString`; see [`format_double`].
pub(crate) fn native_sb_insert_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match args.get(2) {
        Some(Value::Double(v)) => *v,
        Some(Value::Float(v)) => f64::from(*v),
        _ => 0.0,
    };
    sb_insert_text(ctx, this, sb_insert_offset(args), &format_double(v))
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
///
/// It takes TWO bounds checks, both `StringIndexOutOfBoundsException` per the
/// `StringBuilder.insert(int, char[], int, int)` javadoc:
/// `checkOffset(index, count)` for the destination and
/// `checkRangeSIOOBE(offset, offset + len, str.length)` for the source. Both
/// used to be silent clamps, so an out-of-range source window inserted a SHORT
/// slice and an out-of-range destination appended at the end.
pub(crate) fn native_sb_insert_char_array_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let arr = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let src_off = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_len = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Destination first, source second — the JDK's order, and it is observable:
    // `insert(99, str, -1, 2)` on a short builder reports the destination.
    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;

    let arr_len = ctx.array_length(arr) as i32;
    // `offset + len` is computed in i64 because a wrapped i32 sum would read as
    // in-range for a large enough pair — the same reason the JDK's own
    // `checkFromToIndex` does not evaluate it in int.
    let src_end = i64::from(src_off) + i64::from(src_len);
    if src_off < 0 || src_len < 0 || src_end > i64::from(arr_len) {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(
            src_off,
            src_end.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            arr_len,
        )
        .into());
    }
    let (src_start, src_end) = (src_off as usize, src_end as usize);
    let mut insert_chars = Vec::with_capacity(src_end - src_start);
    for i in src_start..src_end {
        insert_chars.push(match ctx.get_array_element(arr, i) {
            Value::Int(c) => c as u16,
            _ => 0,
        });
    }

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
/// The one bounds check it owns is `checkOffset(offset, count)`.
pub(crate) fn native_sb_insert_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let offset = match args.get(1) {
        Some(Value::Int(i)) => *i,
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
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
    result.extend_from_slice(&chars[..offset]);
    result.extend_from_slice(&insert_chars);
    result.extend_from_slice(&chars[offset..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `delete(int, int)` — remove the range `[start, end)`.
///
/// `AbstractStringBuilder.delete` clamps `end` DOWN to the length and only then
/// runs `checkRangeSIOOBE(start, end, count)`, so an over-long `end` alone is
/// legal (`delete(0, 100)` empties the builder) while a `start` past the length
/// is not: `new StringBuilder("ab").delete(5, 6)` is a
/// `StringIndexOutOfBoundsException`, because after the clamp `start 5 > end 2`.
/// Clamping BOTH ends — what this did — turned that into a silent no-op, the
/// fabricated-success shape `docs/known-issues/jdk-only/W2-7-fabricated-success-where-the-spec-mandates-failure.md`
/// inventories. A caller that guards a loop with the exception never leaves it.
pub(crate) fn native_sb_delete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let mut chars = sb_read_chars(ctx, this);
    let count = chars.len() as i32;
    let end = std::cmp::min(end, count);
    if let Some(failure) = sb_check_from_to_index(start, end, count) {
        return Err(failure);
    }
    let (start, end) = (start as usize, end as usize);
    if start < end {
        chars.drain(start..end);
    }
    let this = sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// `deleteCharAt(int)` — remove a single char.
///
/// `checkIndex(index, count)`: the index is EXCLUSIVE of the length, unlike
/// `insert`'s offset. Returning the builder untouched for an out-of-range index
/// — what this did — is the same fabricated success as `delete` above.
pub(crate) fn native_sb_delete_char_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    let count = chars.len() as i32;
    if index < 0 || index >= count {
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    }
    let index = index as usize;
    let mut result = Vec::with_capacity(chars.len() - 1);
    result.extend_from_slice(&chars[..index]);
    result.extend_from_slice(&chars[index + 1..]);
    let this = sb_write_chars(ctx, this, &result);
    Ok(Some(Value::Object(Some(this))))
}

/// `replace(int, int, String)` — replace a range with a string.
///
/// Same shape as `delete`: `end` is clamped to the length, then
/// `checkRangeSIOOBE(start, end, count)`. The null replacement is NOT the same
/// as `insert(int, String)`'s — `replace` reads `str.length()` with no guard, so
/// it is a NullPointerException, and it is raised only AFTER the range check.
pub(crate) fn native_sb_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let mut chars = sb_read_chars(ctx, this);
    let count = chars.len() as i32;
    let end = std::cmp::min(end, count);
    if let Some(failure) = sb_check_from_to_index(start, end, count) {
        return Err(failure);
    }
    // Code UNITS — `G26Builder` row r1. See `native_sb_append_string`.
    let repl_chars: Vec<u16> = match args.get(3) {
        Some(Value::Object(Some(obj))) => read_string_chars(&*ctx, *obj),
        Some(Value::Object(None)) => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.length()\" because \"str\" is null".to_string(),
                ),
            }
            .into())
        }
        _ => Vec::new(),
    };
    chars.splice(start as usize..end as usize, repl_chars);
    let this = sb_write_chars(ctx, this, &chars);
    Ok(Some(Value::Object(Some(this))))
}

/// `setCharAt(int, char)` — set the char at `index`.
///
/// `checkIndex(index, count)`. Dropping the store for an out-of-range index —
/// what the `if index < count` guard did — is a WRITE that silently did not
/// happen, the worst reading of the fabricated-success species.
pub(crate) fn native_sb_set_char_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    let ch = match args.get(2) {
        Some(Value::Int(c)) => *c as u16,
        _ => return Ok(None),
    };
    let (buf, count) = sb_state(ctx, this);
    if index < 0 || index >= count {
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    }
    if let Some(buf) = buf {
        ctx.set_array_element(buf, index as usize, Value::Int(ch as i32));
    }
    Ok(None)
}

/// `setLength(int)` — truncate, or extend with NUL chars.
///
/// A negative length is the one thing `AbstractStringBuilder.setLength` rejects,
/// and it does so before touching the buffer; clamping it to 0 (what this did)
/// silently emptied the builder instead.
///
/// The message is `StringIndexOutOfBoundsException(int)`'s own wording rather
/// than one of the `Preconditions` shapes, because this check is hand-rolled in
/// the JDK and not routed through a formatter. UNVERIFIED against JDK 25: only
/// the exception CLASS is pinned by the javadoc.
pub(crate) fn native_sb_set_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let new_len = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    if new_len < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                index: new_len,
                message: Some(format!("String index out of range: {new_len}")),
            }
            .into(),
        );
    }
    let new_len = new_len as usize;
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

/// `String.indexOf(value, coder, count, str, fromIndex)`'s contract — the body
/// `AbstractStringBuilder.indexOf(String, int)` delegates to
/// (`StringLatin1`/`StringUTF16.indexOf`):
///
/// ```text
///     if (fromIndex >= valueCount) {
///         return (strCount == 0 ? valueCount : -1);
///     }
///     if (fromIndex < 0) { fromIndex = 0; }
///     if (strCount == 0) { return fromIndex; }
/// ```
///
/// Note the first arm: `sb.indexOf("", 99)` is the LENGTH, not -1 and not 99.
///
/// The counterpart of [`u16_last_index_of`], and the same reason for existing:
/// both operands are UTF-16 code units. The two `indexOf` natives used to
/// build a Rust `String` with `String::from_utf16_lossy` and call `str::find`,
/// which (a) cannot represent an unpaired surrogate in either operand, so a
/// lone `\uD800` needle was searched for as U+FFFD and matched the wrong
/// position — or matched a DIFFERENT lone surrogate, since every one of them
/// collapses to the same replacement character — and (b) then converted a UTF-8
/// BYTE offset back to a code-unit index by re-encoding the prefix.
fn u16_index_of(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    let n = haystack.len() as i32;
    let m = needle.len() as i32;
    if from >= n {
        return if m == 0 { n } else { -1 };
    }
    let from = from.max(0);
    if m == 0 {
        return from;
    }
    if m > n - from {
        return -1;
    }
    let mut k = from;
    while k <= n - m {
        let s = k as usize;
        if &haystack[s..s + m as usize] == needle {
            return k;
        }
        k += 1;
    }
    -1
}

/// indexOf(String) — find substring, return -1 if not found
pub(crate) fn native_sb_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // `read_string_chars`, not `read_string`: the needle may itself hold an
    // unpaired surrogate.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(u16_index_of(&chars, &target, 0))))
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
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(u16_index_of(&chars, &target, from))))
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
    // `read_string_chars`: an unpaired surrogate in the needle cannot survive
    // `read_string().encode_utf16()`, which routes through a Rust `str`.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
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
    // `read_string_chars`: an unpaired surrogate in the needle cannot survive
    // `read_string().encode_utf16()`, which routes through a Rust `str`.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
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
    // Units, not `from_utf16_lossy`: `new StringBuilder("a\uD800b").substring(1, 2)`
    // is a one-char String holding an unpaired surrogate. See
    // [`sb_string_from_units`].
    let str_obj = sb_string_from_units(ctx, &chars[start as usize..])?;
    Ok(Some(Value::Object(Some(str_obj))))
}

/// `Preconditions.checkFromToIndex(start, end, count, SIOOBE_FORMATTER)` — the
/// range check `AbstractStringBuilder.substring`/`subSequence` open with, and
/// the body of `String.checkRangeSIOOBE`, which `delete`/`replace`/
/// `codePointCount` open with in turn.
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

/// `String.checkOffset(offset, count)` — the check every `insert` overload
/// opens with.
///
/// Unlike `checkIndex` the upper bound is INCLUSIVE: inserting at `count`
/// appends, which is why this cannot just be `sioobe_index`. The JDK spells it
/// `Preconditions.checkFromToIndex(offset, length, length, SIOOBE_FORMATTER)`,
/// so the message names a range and not an index.
fn sb_check_offset(offset: i32, count: i32) -> Option<cratonvm_types::error::MethodCallFailed> {
    sb_check_from_to_index(offset, count, count)
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
    let str_obj = sb_string_from_units(ctx, &chars[start as usize..end as usize])?;
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
        // `contains` is `indexOf(s.toString()) >= 0`; the JDK's NPE comes from
        // that `toString()`. See `string_arg_npe`.
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"java.lang.CharSequence.toString()\" because \"s\" is null",
            ))
        }
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
        _ => return Err(string_arg_npe(STARTS_WITH_NPE)),
    };
    let result = with_two_string_chars_scratches(ctx, this, prefix, |this_chars, prefix_chars| {
        this_chars.starts_with(prefix_chars)
    });
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// HotSpot's helpful-NPE text for a null `prefix`, shared by both
/// `startsWith` overloads (the one-argument form is `startsWith(prefix, 0)`).
const STARTS_WITH_NPE: &str = "Cannot invoke \"String.length()\" because \"prefix\" is null";

/// `String.startsWith(String prefix, int toffset)`.
///
/// The JDK's guard is `if (toffset < 0 || toffset > length() - prefix.length())
/// return false;` — a short-circuiting `||` whose SECOND term dereferences
/// `prefix`, exactly like `regionMatches`. So a negative `toffset` answers
/// `false` for a null prefix and every other `toffset` throws. Measured on
/// OpenJDK 25.0.3+9 with `"abc"`: `startsWith(null, -1)` is `false`;
/// `startsWith(null, 0)`, `startsWith(null, 3)` and
/// `startsWith(null, Integer.MAX_VALUE)` all throw NullPointerException.
///
/// The old `offset` handling was also wrong for a negative `toffset` in a way
/// the null rows exposed: `*v as usize` reinterprets `-1` as `usize::MAX`,
/// which then failed the `offset <= len` test and answered `false` — the right
/// answer by accident, from an expression that would have panicked had the
/// comparison been written the other way round.
pub(crate) fn native_string_starts_with_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let toffset = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Term one, before the prefix is touched.
    if toffset < 0 {
        return Ok(Some(Value::Int(0)));
    }
    let prefix = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(string_arg_npe(STARTS_WITH_NPE)),
    };
    let offset = toffset as usize;
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
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"String.length()\" because \"suffix\" is null",
            ))
        }
    };
    let result = with_two_string_chars_scratches(ctx, this, suffix, |this_chars, suffix_chars| {
        this_chars.ends_with(suffix_chars)
    });
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `String.trim()` — remove leading/trailing chars whose value is `<= U+0020`.
///
/// W7-95a. This is NOT `String.strip()` and it is NOT `str::trim()`; the tree
/// had all three collapsed onto Rust's. `trim()` predates Unicode-aware
/// whitespace in Java and is defined by a raw code-unit comparison against
/// the space character, while `strip()` uses `Character.isWhitespace`. Three
/// rules, and Rust's `str::trim` (Unicode `White_Space`) is none of them.
///
/// Measured on OpenJDK 25.0.3+9, printed as code units so the console cannot
/// lie about what survived:
///
/// ```text
///  input (as code units)      trim()            strip()          str::trim() gave
///  [00A0, 0078, 00A0]         [160,120,160]     [160,120,160]    [120]
///  [0000, 0078, 0000]         [120]             [0,120,0]        [0,120,0]
///  [001C, 0078, 001C]         [120]             [120]            [28,120,28]
///  [2028, 0078, 2028]         [8232,120,8232]   [120]            [120]
/// ```
///
/// Every row where `trim()` and `strip()` differ is a row Rust's `trim` gets
/// wrong for at least one of them, which is why one shared implementation was
/// never going to work.
pub(crate) fn native_string_trim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let units = read_string_chars(ctx, this);
    let start = units.iter().position(|&u| u > 0x20).unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|&u| u > 0x20)
        .map_or(start, |i| i + 1);
    let trimmed = String::from_utf16_lossy(&units[start..end]);
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
    let target_units = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            invoke_to_string_units_opt(&mut *scope, *o)?.unwrap_or_default()
        }
        // A `null` target is an NPE, matching the JDK. This used to return a
        // null `String` on the reasoning that "real callers never pass null" —
        // which is a claim about callers, not about the method, and it made
        // `s.replace(null, "x")` hand back `null` where HotSpot throws.
        _ => return Err(regex_arg_npe("target").into()),
    };
    let replacement_units = match args.get(2) {
        Some(Value::Object(Some(o))) => {
            invoke_to_string_units_opt(&mut *scope, *o)?.unwrap_or_default()
        }
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let this = scope.get(&this_handle);
    // Units, not text, all the way through. This method took its receiver
    // through `read_string` and handed the result to `create_string`, so an
    // unpaired surrogate anywhere in it became U+FFFD — MEASURED on both VMs
    // at `89e2c56f1`, including on a call that matches NOTHING, which is what
    // proves the loss is the round trip and not the replacement:
    //
    //   "a<U+D800>b".replace("q", "z")   HotSpot 61,d800,62   CratonVM 61,fffd,62
    //
    // `replace_units` is the only implementation rather than a second one
    // behind a surrogate guard: `sb_string_from_units` already takes the
    // plain-text path when the units are representable, so behaviour for
    // every input that works today is unchanged, and there is no rarely-taken
    // copy to drift.
    let s_units = read_string_chars(&*scope, this);
    let out = replace_units(&s_units, &target_units, &replacement_units);
    let result = sb_string_from_units(&mut *scope, &out)?;
    Ok(Some(Value::Object(Some(result))))
}

/// Literal find-and-replace over UTF-16 code units — `str::replace`'s contract,
/// in the space Java strings actually live in.
///
/// The empty-target case is the one worth stating: both Java and Rust insert
/// the replacement before every unit and once at the end, so `"abc"` with
/// `("", "-")` is `-a-b-c-`. It is handled explicitly because the obvious loop
/// silently produces `a-b-c` instead, and no test in this tree would have
/// caught the difference.
fn replace_units(haystack: &[u16], target: &[u16], replacement: &[u16]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(haystack.len());
    if target.is_empty() {
        out.extend_from_slice(replacement);
        for &u in haystack {
            out.push(u);
            out.extend_from_slice(replacement);
        }
        return out;
    }
    let mut i = 0usize;
    while i < haystack.len() {
        if i + target.len() <= haystack.len() && &haystack[i..i + target.len()] == target {
            out.extend_from_slice(replacement);
            i += target.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
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
        // `str::to_uppercase` (the FULL mapping) is the right primitive here
        // and must stay: `String.toUpperCase()` really does answer "SS" for a
        // sharp s. Only `Character.toUpperCase(char)` is the 1:1 mapping, and
        // that lives in `java_char_to_upper_case`.
        //
        // The one correction is the version skew, and it is now applied by
        // `case_map`'s shared helper rather than by a loop written out here:
        // this arm and `case_map::map_locale_dependent`'s fallback arms are the
        // same rule for different locales, and only one of them had the fix.
        // The helper keeps the cheap early-out — the ASCII fast path above
        // never reaches here, and every skewed code point is above U+A7CD, so
        // ordinary text still takes a single bulk `str::to_uppercase` call.
        let mapped = if lowercase {
            crate::case_map::jdk_to_lowercase(&folded)
        } else {
            crate::case_map::jdk_to_uppercase(&folded)
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
///
/// **This collapses two different calls**, so it is correct only where the two
/// cannot occur at the same site. A native reached through an argument slice CAN
/// tell them apart — `toUpperCase()` has one argument and `toUpperCase(null)`
/// has two — and must use [`locale_arg_checked`].
///
/// The JIT's direct helper was previously listed here as a caller that
/// "genuinely cannot tell them apart". It can, and the claim was the whole
/// defect: see [`jit_string_to_lower_case`]. Its one bound descriptor,
/// `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`, has a
/// MANDATORY `Locale` slot, so "absent" is not a state that site can be in and
/// a `None` there is an explicit `null`.
pub(crate) fn locale_arg(args: &[Value], index: usize) -> Option<cratonvm_types::ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// [`locale_arg`], but an explicitly-passed `null` `Locale` is the JDK's
/// `NullPointerException` rather than "use the default".
///
/// Measured on OpenJDK 25.0.3+9: `"abc".toUpperCase((Locale) null)`,
/// `"abc".toLowerCase((Locale) null)` and `"".toUpperCase((Locale) null)` all
/// throw NPE, while the no-argument `"abc".toUpperCase()` answers `"ABC"`. The
/// throw comes from `StringLatin1.toLowerCase`'s `locale.getLanguage()`, which
/// is why an empty receiver throws too — there is no short circuit in front of
/// it. This VM answered the default-locale result for all three.
///
/// The two cases are distinguished by ARITY, which is sound because the two
/// overloads have different descriptors: a slot that is present and holds
/// `Object(None)` is a real `null` argument, an absent slot is the no-arg
/// overload.
fn locale_arg_checked(
    args: &[Value],
    index: usize,
) -> Result<Option<cratonvm_types::ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    if matches!(args.get(index), Some(Value::Object(None))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    Ok(locale_arg(args, index))
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
    string_case_impl(ctx, this, locale_arg_checked(args, 1)?, lowercase, memoize)
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
    // An ABSENT locale slot must stay absent. Since `string_case_native` began
    // distinguishing "no `Locale` argument" from "an explicit null `Locale`" by
    // arity, materialising `Value::Object(None)` for a missing slot here would
    // have turned every short call into a NullPointerException. (A slot that is
    // present and null is forwarded unchanged — the JDK's own NPE for
    // `toLowerCase(null)` is raised inside this very method, by
    // `StringLatin1.toLowerCase`'s `locale.getLanguage()`.)
    match args.get(2) {
        Some(locale) => native_string_to_lower_case(ctx, &[this, locale.clone()]),
        None => native_string_to_lower_case(ctx, &[this]),
    }
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
/// # It is the SAME body as the interpreted native, and that is the fix
///
/// This used to call [`string_case_impl`] directly, one layer BELOW
/// [`string_case_native`] — so when E8 added the null-`Locale` check to
/// [`locale_arg_checked`], the interpreter began throwing and the compiled path
/// kept answering the default-locale result. A `s.toLowerCase((Locale) null)`
/// inside a loop therefore threw for its first few hundred executions and then
/// silently started returning a string, at whatever iteration the caller tiered
/// up. That is the `aastore` store-check shape: correct until it is hot.
///
/// It now goes through `string_case_native`, i.e. the exact function the
/// registered native calls, with the `Locale` argument reconstituted as a
/// PRESENT slot. Present-and-null is what this site always is: the JIT binds
/// exactly one descriptor here,
/// `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;`
/// (`jit/src/lib.rs`, the `STRING_LATIN1_LOWER_DIRECT_FN` ladder), whose third
/// parameter is mandatory — there is no arity by which this caller could be the
/// no-argument overload. So the arity rule that separates `toLowerCase()` from
/// `toLowerCase(null)` is not being bypassed here, it is being *supplied*.
///
/// The one-line predecessor also silently discarded every `Err`: an exception
/// raised inside the case mapping became a `null` return and then a
/// wrong-place NPE in the caller. Returning [`MethodCallResult`] is what lets
/// `vm/src/jit/helpers.rs` route it through `handle_jit_dispatch_error`, the
/// same way its `jit_hashmap_get_direct` sibling already does.
///
/// Measured on OpenJDK 25.0.3+9: `"AbC".toLowerCase((Locale) null)` throws
/// NullPointerException, and so does it on the 200 000th warm iteration — the
/// oracle's answer does not depend on its tier either.
pub fn jit_string_to_lower_case(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    locale: Option<cratonvm_types::ObjectRef>,
) -> MethodCallResult {
    string_case_native(
        ctx,
        &[Value::Object(Some(this)), Value::Object(locale)],
        true,
        true,
    )
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
        Some(Value::Object(Some(obj))) => {
            // `String.toString()` returns `this`, so for a String argument the
            // JDK contract is an identity: `String.valueOf(s) == s`. Returning
            // the object is exact AND allocation-free on the hottest arm of a
            // very hot method — the previous code decoded it to Rust text and
            // built a second String on every call.
            //
            // Exactness matters here, not just cost. `invoke_to_string_opt` is
            // the LOSSY wrapper over `invoke_to_string_units_opt`: it exists
            // only to hand back a Rust `String`, which cannot carry an
            // unpaired surrogate. MEASURED on both VMs at `89e2c56f1`:
            //
            //   String.valueOf((Object) "a<U+D800>b")
            //     HotSpot   61,d800,62      CratonVM   61,fffd,62
            //
            // The units form was built by `G26` for exactly this reason and
            // this call site never moved to it.
            if ctx
                .class_name_of_id(ctx.class_id_of_object(*obj))
                .as_deref()
                == Some("java/lang/String")
            {
                return Ok(Some(Value::Object(Some(*obj))));
            }
            match invoke_to_string_units_opt(ctx, *obj)? {
                Some(units) => {
                    let result = sb_string_from_units(ctx, &units)?;
                    Ok(Some(Value::Object(Some(result))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        }
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
        // `-1` — "not found" — was the single most misleading of the swallowed
        // nulls: it is the method's ordinary answer, so a caller could not
        // distinguish it from a real miss. HotSpot throws.
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"String.coder()\" because \"str\" is null",
            ))
        }
    };
    let result = with_two_string_chars_scratches(ctx, this, target, |haystack, needle| {
        index_of_units_from(haystack, needle, 0)
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
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). `java.lang.String`
        // is in every image, so this arm is unreachable on a working one; a run
        // that reaches it has no `java.base`, and fabricating a `String`
        // stand-in there is a second failure wearing the first one's name.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    let arr = ctx.new_ref_array(string_class_id, parts.len());
    for (i, part) in parts.iter().enumerate() {
        let str_ref = ctx.create_string_uninterned(part);
        ctx.set_array_element(arr, i, Value::Object(Some(str_ref)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// HotSpot's helpful-NPE text for a null `regex`, shared by every `split` entry
/// point.
///
/// All of them used to answer a NULL `String[]`, which is worse than a wrong
/// array: `for (String p : s.split(null))` then fails with an NPE at the
/// CALLER's line, blaming the caller for the native's contract violation.
/// Measured on OpenJDK 25.0.3+9: `"a,b".split(null)` and `"a,b".split(null, 2)`
/// both throw.
const SPLIT_REGEX_NPE: &str = "Cannot invoke \"String.length()\" because \"regex\" is null";

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
        _ => return Err(string_arg_npe(SPLIT_REGEX_NPE)),
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
        _ => return Err(string_arg_npe(SPLIT_REGEX_NPE)),
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

/// `String.join(CharSequence delimiter, CharSequence... elements)`.
///
/// # Three different nulls, three different answers
///
/// Measured on OpenJDK 25.0.3+9. The JDK's body opens
/// `var delim = delimiter.toString(); var elems = new String[elements.length];`
/// and renders each element with `String.valueOf`:
///
/// ```text
/// String.join(null, "a", "b")                 NullPointerException  (delimiter, FIRST)
/// String.join(",", (CharSequence[]) null)     NullPointerException  (elements)
/// String.join(null, (CharSequence[]) null)    NullPointerException  naming the DELIMITER
/// String.join(",", "a", null, "b")            "a,null,b"            <- an ELEMENT renders
/// String.join(",", new String[0])             ""
/// ```
///
/// This VM answered `""` for the first three and already rendered `"null"` for
/// the fourth. The delimiter is checked before the array because the JDK's
/// order is observable through the message, and because an empty `elements`
/// array does not excuse a null delimiter — `String.join(null, new String[0])`
/// throws.
pub(crate) fn native_string_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = delimiter, args[1] = CharSequence[]
    let delim = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(string_arg_npe(JOIN_DELIMITER_NPE)),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read the array length because \"elements\" is null",
            ))
        }
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

/// HotSpot's helpful-NPE text for a null `delimiter`, shared by both `join`
/// overloads.
const JOIN_DELIMITER_NPE: &str =
    "Cannot invoke \"java.lang.CharSequence.toString()\" because \"delimiter\" is null";

/// `String.join(CharSequence delimiter, Iterable<? extends CharSequence>)`.
///
/// Same three-nulls table as [`native_string_join`], with one difference the
/// message records: this overload opens with two explicit
/// `Objects.requireNonNull` calls rather than implicit dereferences, so HotSpot
/// raises a NullPointerException with a NULL message for both. Measured:
/// `String.join(",", (Iterable) null)` and `String.join(null, List.of())` both
/// throw with no message. The delimiter is still checked first.
pub(crate) fn native_string_join_iterable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let delim = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into())
        }
    };
    let iterable = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into())
        }
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

/// The `NullPointerException` a `String` native owes for a reference argument
/// its JDK counterpart dereferences.
///
/// # Why this is a family and not a handful of one-offs
///
/// `String`'s natives are written as `match args.get(n) { Some(Object(Some(o)))
/// => o, _ => <default> }`, and that `_` arm swallows a null argument into
/// whatever the failure value happens to be: `false` for the predicates, `-1`
/// for the searches, a NULL `String[]` for `split`, `""` for `join` and
/// `copyValueOf`, a silent no-op for `getChars`. Every one of those is a wrong
/// *value* handed back where HotSpot raises — the failure mode a native
/// shadowing bytecode is most likely to have and least likely to have noticed,
/// because nothing crashes and no test that only exercises valid inputs can
/// see it.
///
/// # The contracts are NOT uniform, so each one is measured
///
/// Measured on Microsoft OpenJDK 25.0.3+9 (`probes/e8/NullContracts`):
///
/// ```text
/// equals(null)                     false            <- NOT a throw
/// equalsIgnoreCase(null)           false            <- NOT a throw
/// startsWith(null, -1)             false            <- NOT a throw (toffset < 0 wins)
/// regionMatches(-1, null, 0, 1)    false            <- NOT a throw (see region_matches_impl)
/// String.valueOf((Object) null)    "null"           <- NOT a throw
/// String.join(",", "a", null, "b") "a,null,b"       <- a null ELEMENT renders
/// String.format("%s", (Object[]) null)  "null"      <- a null varargs ARRAY is one null arg
/// contains(null) startsWith(null) endsWith(null) concat(null) compareTo(null)
/// compareToIgnoreCase(null) indexOf((String) null) lastIndexOf((String) null)
/// split(null) matches(null) replace(null, x) transform(null)
/// String.join(null, …) String.join(",", (CharSequence[]) null)
/// String.valueOf((char[]) null) String.copyValueOf(null) getChars(…, null, …)
/// toUpperCase((Locale) null) toLowerCase((Locale) null)   ->  NullPointerException
/// ```
///
/// The two `false` rows and the three rendering rows are the reason this is a
/// per-method measurement rather than a rule applied by shape: "reference
/// parameter" does not imply "throws", and `equals`/`equalsIgnoreCase` in
/// particular are specified to answer `false`.
///
/// `message` is HotSpot's helpful-NPE text where it was captured verbatim.
/// Control flow depends on the *class*, but the text is what a caller logging
/// the exception will print, and it is free to be right.
fn string_arg_npe(message: &str) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::NullPointerException {
        message: Some(message.to_string()),
    }
    .into()
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

/// `String.equalsIgnoreCase(String)`.
///
/// **The null argument is `false`, NOT a throw** — measured on
/// OpenJDK 25.0.3+9, and the JDK source says why:
/// `(anotherString != null) && ...`. This is one of the two rows in the whole
/// `String` null sweep that is specified to answer rather than raise (the other
/// is `equals`), which is why the sweep had to be a measurement per method.
///
/// # The comparison rule, which was not the JDK's
///
/// The old body was `a.to_lowercase() == b.to_lowercase()` — Rust's FULL
/// (SpecialCasing) mappings over whole strings. The JDK is
/// `regionMatches(true, 0, other, 0, length())` after a length check, i.e. the
/// per-code-unit `StringUTF16.regionMatchesCI` rule already transcribed in
/// [`code_unit_eq_ignore_case`]. The two disagree, measured:
///
/// ```text
///                                        HotSpot 25   to_lowercase() gave
/// "İ".equalsIgnoreCase("i")         true         false   (full mapping is i + U+0307)
/// "ꟓ".equalsIgnoreCase("꟒")    false        true    (Rust pairs them, the JDK does not)
/// "ꟕ".equalsIgnoreCase("꟔")    false        true
/// "꟏".equalsIgnoreCase("꟎")    false        true
/// "ꟑ".equalsIgnoreCase("Ꟑ")    true         true    <- control: a REAL pair
/// "K".equalsIgnoreCase("k")         true         true
/// "ß".equalsIgnoreCase("ss")        false        false
/// ```
///
/// The doc on `case_map::JDK_UNMAPPED_CASE_CODE_POINTS` already claimed
/// `equalsIgnoreCase` among the four answers it had measured and corrected.
/// The claim was true of the measurement and false of the code: the fix went
/// into `java_char_to_upper_case`, which this method never called.
/// `[1 of 10 callsites]` — the helper existed and one caller used it.
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
        // Specified, not swallowed: `equalsIgnoreCase(null)` IS `false`.
        _ => return Ok(Some(Value::Int(0))),
    };
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    // The thread-local scratches, not `read_string_chars`: this method is on
    // the hot path for HTTP header matching and must not start allocating two
    // Vecs per call to gain its correctness.
    let equal = with_two_string_chars_scratches(ctx, this, other, |a, b| {
        a.len() == b.len()
            && a.iter()
                .zip(b.iter())
                .all(|(&x, &y)| code_unit_eq_ignore_case(x, y))
    });
    Ok(Some(Value::Int(if equal { 1 } else { 0 })))
}

/// `String.compareToIgnoreCase(String)`.
///
/// Unlike its `equalsIgnoreCase` sibling, a null argument here THROWS —
/// measured on OpenJDK 25.0.3+9 (`Cannot read field "value" because "s2" is
/// null`), because `CASE_INSENSITIVE_ORDER.compare` dereferences both operands
/// with no guard. The two methods sit next to each other, take the same
/// parameter type, and have opposite null contracts; this VM answered `0` —
/// "equal" — for the null.
///
/// # The magnitude, not just the sign
///
/// The old body lower-cased both strings with Rust's full mapping and returned
/// `-1`/`0`/`1` from an `Ordering`. The JDK's `StringLatin1.compareToCI` /
/// `StringUTF16.compareToCI` return the DIFFERENCE of the two folded code
/// units, and the fold is the same upper-then-lower composition as
/// [`code_unit_eq_ignore_case`]:
///
/// ```text
/// c1 == c2                       -> keep going
/// u1 = toUpper(c1), u2 = toUpper(c2); u1 == u2  -> keep going
/// l1 = toLower(u1), l2 = toLower(u2); l1 == l2  -> keep going
/// otherwise                      -> return l1 - l2
/// exhausted                      -> return len1 - len2
/// ```
///
/// Measured rows the old shape got wrong: `"_".compareToIgnoreCase("a")` is
/// `-2` (the fold leaves `_` alone and lowercases `A` back to `a`, so it is
/// `0x5F - 0x61`) and was `-1`; `"İ".compareToIgnoreCase("i")` is `0` and
/// was non-zero; `"ꟓ".compareToIgnoreCase("꟒")` is `1` and was `0`.
/// A comparator only needs the sign, but `compareToIgnoreCase` is a public
/// method whose javadoc specifies the value, and the first two rows are sign
/// errors anyway.
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
        _ => {
            return Err(string_arg_npe(
                "Cannot read field \"value\" because \"s2\" is null",
            ))
        }
    };
    let result = with_two_string_chars_scratches(ctx, this, other, |a, b| {
        for (&c1, &c2) in a.iter().zip(b.iter()) {
            if c1 == c2 {
                continue;
            }
            let u1 = java_char_to_upper_case(c1);
            let u2 = java_char_to_upper_case(c2);
            if u1 == u2 {
                continue;
            }
            let l1 = java_char_to_lower_case(u1);
            let l2 = java_char_to_lower_case(u2);
            if l1 != l2 {
                return i32::from(l1) - i32::from(l2);
            }
        }
        a.len() as i32 - b.len() as i32
    });
    Ok(Some(Value::Int(result)))
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
    // bug nb-lang-string: lastIndexOf is defined over UTF-16 code UNITS, not
    // Unicode code points. The old `rfind` + `chars().count()` returned a
    // code-point index, off by the count of preceding supplementary chars.
    //
    // E18: the `char::from_u32(ch)` + `encode_utf16` that replaced it is the
    // ONE member of this family that got the surrogate pair right, and it is
    // still not the JDK's rule — `char::from_u32` rejects the surrogate code
    // points, so a lone-surrogate `ch` returned `-1` where HotSpot finds it
    // (`"xзy𐐷z".lastIndexOf(0xDC37)` is `4`). [`code_point_needle`] is the
    // rule; this native and its three siblings now share it.
    let Some(needle) = code_point_needle(ch) else {
        return Ok(Some(Value::Int(-1)));
    };
    let result = with_string_chars_scratch(ctx, this, |buf| {
        last_index_of_units_from(buf, needle.units(), i64::MAX)
    });
    Ok(Some(Value::Int(result)))
}

/// The UTF-16 code units that `String.{indexOf,lastIndexOf}(int ch[, int])`
/// actually searches for — `None` when no sequence of code units can match.
///
/// # E18. Three copies of this rule, and the two that answered were wrong
///
/// The JDK does **not** narrow `ch` to a code unit. `String.indexOf(int)` is
///
/// ```text
/// isLatin1() ? StringLatin1.indexOf(value, ch, …)   // if (!canEncode(ch)) return -1;
///            : StringUTF16 .indexOf(value, ch, …)   // !isValidCodePoint  -> -1
///                                                   //  ch <  0x10000     -> scan for (char) ch
///                                                   //  ch >= 0x10000     -> scan for the PAIR
/// ```
///
/// so a supplementary `ch` is matched as a surrogate *pair* and never as its
/// low half. `native_string_index_of`, `native_string_index_of_from` and
/// `native_string_last_index_of_from` all wrote `(ch & 0xFFFF) as u16`, which
/// finds the masked half; `native_string_last_index_of_char` instead wrote
/// `char::from_u32(ch)` + `encode_utf16`, which gets the pair right but drops
/// every LONE SURROGATE `ch` on the floor (`char::from_u32(0xDC37)` is `None`).
/// One JVMS rule, four implementations counting the JIT's, four different sets
/// of wrong answers. This is now the only place the rule is written down.
///
/// Measured on OpenJDK 25.0.3+9 (`scratchpad/e18/CharSearch.java`,
/// `Idx3.java`), receiver `"xзy𐐷z"` unless noted:
///
/// ```text
/// "abc".indexOf(0x10061)          -1   masking finds 'a' at 0        <- the KEY row
/// "з".indexOf(0x10437)       -1   masking finds it at 0
/// mixed.indexOf(0x10437)           3   the pair, not the low half
/// mixed.indexOf(0xD801)            3   a lone HIGH surrogate is a plain unit scan
/// mixed.indexOf(0xDC37)            4   a lone LOW surrogate too — char::from_u32 says None
/// "￿q".indexOf(0xFFFF)        0   0xFFFF is a valid (non-)character
/// "￿q".indexOf(-1)           -1   NOT (char)(-1) == 0xFFFF: isValidCodePoint gates first
/// "abc".indexOf(0x110000)         -1
/// "abc".lastIndexOf(0x10061)      -1                                 <- the KEY row again
/// ```
///
/// The `-1` row is the one that shows the rule is `isValidCodePoint`, not a
/// narrowing cast: `(char) -1` is `0xFFFF`, the receiver holds `0xFFFF`, and
/// HotSpot still answers `-1`.
fn code_point_needle(ch: i32) -> Option<CharNeedle> {
    if !(0..=0x10_FFFF).contains(&ch) {
        // `Character.isValidCodePoint(ch)` — checked before any narrowing.
        return None;
    }
    if ch <= 0xFFFF {
        // A BMP code point, INCLUDING an unpaired surrogate value: the JDK
        // scans for the single code unit and so must this.
        return Some(CharNeedle {
            units: [ch as u16, 0],
            len: 1,
        });
    }
    let off = ch - 0x1_0000;
    Some(CharNeedle {
        units: [
            (0xD800 + (off >> 10)) as u16,
            (0xDC00 + (off & 0x3FF)) as u16,
        ],
        len: 2,
    })
}

/// One or two UTF-16 code units — the output of [`code_point_needle`].
#[derive(Clone, Copy)]
struct CharNeedle {
    units: [u16; 2],
    len: usize,
}

impl CharNeedle {
    fn units(&self) -> &[u16] {
        &self.units[..self.len]
    }
}

/// Forward scan for `needle` in `haystack`, both in UTF-16 code units, starting
/// at `max(from, 0)`.
///
/// The empty-needle answer is `clamp(from, 0, haystack.len())`, which is the
/// JDK's — measured: `"abcabc".indexOf("", 99)` is `6` and `indexOf("", -5)` is
/// `0`. Shared by the code-point family and the `String`-needle family; the four
/// hand-rolled copies of this loop it replaces disagreed only in their bounds
/// arithmetic, which is the part worth having in one place.
fn index_of_units_from(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    if needle.is_empty() {
        return from.clamp(0, haystack.len() as i32);
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let max_start = haystack.len() - needle.len();
    let mut i = from.max(0) as usize;
    while i <= max_start {
        if &haystack[i..i + needle.len()] == needle {
            return i as i32;
        }
        i += 1;
    }
    -1
}

/// Backward scan for `needle` in `haystack`, starting at
/// `min(from, haystack.len() - needle.len())`.
///
/// That subtraction is the JDK's own: `StringUTF16.lastIndexOfChar` starts at
/// `min(fromIndex, length - 1)` and `lastIndexOfSupplementary` at
/// `min(fromIndex, length - 2)`, which is the same expression once the needle
/// width is a parameter rather than a hard-coded 1 or 2.
///
/// `from` is `i64` so that a NEGATIVE `from` finds nothing rather than
/// saturating to a `usize`: the JDK's loop counter simply starts below zero and
/// never runs. Measured — `"abc".lastIndexOf('a', -1)` is `-1`, and
/// `mixed.lastIndexOf(0x10437, -1)` is `-1` even though the pair is present.
fn last_index_of_units_from(haystack: &[u16], needle: &[u16], from: i64) -> i32 {
    if needle.is_empty() {
        return haystack.len() as i32;
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let mut i = from.min((haystack.len() - needle.len()) as i64);
    while i >= 0 {
        let at = i as usize;
        if &haystack[at..at + needle.len()] == needle {
            return i as i32;
        }
        i -= 1;
    }
    -1
}

/// Helper: last index (in UTF-16 code units) of `needle` within `haystack`.
/// Returns -1 when not found. An empty needle returns `haystack.len()` to
/// match `String.lastIndexOf("")` semantics used by the str variant.
fn last_index_of_units(haystack: &[u16], needle: &[u16]) -> i32 {
    // "no `fromIndex`" is "start as far right as the needle fits", which is
    // what [`last_index_of_units_from`] does with a `from` it cannot exceed.
    last_index_of_units_from(haystack, needle, i64::MAX)
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
        // See `native_string_index_of_str`: `-1` is indistinguishable from a
        // legitimate miss. HotSpot throws.
        _ => {
            return Err(string_arg_npe(
                "Cannot read field \"value\" because \"tgtStr\" is null",
            ))
        }
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

/// `String.getChars(int srcBegin, int srcEnd, char[] dst, int dstBegin)`.
///
/// # Three checks, in the JDK's order, none of which this body had
///
/// The old version silently clamped `srcEnd` to the receiver's length, ignored
/// a negative `srcBegin`, and treated a null `dst` as a NO-OP that returned
/// normally. `getChars` is the JDK's bulk copy out of a `String`, so a caller
/// that mis-sizes its buffer got a partly-filled array and no signal at all.
///
/// Measured on OpenJDK 25.0.3+9 with the receiver `"abc"`:
///
/// ```text
/// getChars(0, 9, null, 0)          StringIndexOutOfBoundsException: Range [0, 9) out of bounds for length 3
/// getChars(2, 1, null, 0)          StringIndexOutOfBoundsException: Range [2, 1) out of bounds for length 3
/// getChars(-1, 1, null, 0)         StringIndexOutOfBoundsException: Range [-1, 1) out of bounds for length 3
/// getChars(0, 1, null, 0)          NullPointerException: Cannot read the array length because "dst" is null
/// getChars(0, 1, null, -1)         NullPointerException                       <- null beats dstBegin
/// getChars(0, 1, new char[4], -1)  StringIndexOutOfBoundsException: Range [-1, -1 + 1) out of bounds for length 4
/// getChars(0, 3, new char[4], 2)   StringIndexOutOfBoundsException: Range [2, 2 + 3) out of bounds for length 4
/// getChars(0, 0, new char[0], 0)   (returns normally)
/// ```
///
/// So the order is: `checkBoundsBeginEnd(srcBegin, srcEnd, length())` first —
/// it fires even when `dst` is null — then the null check, then
/// `checkBoundsOffCount(dstBegin, srcEnd - srcBegin, dst.length)`. Both range
/// failures are `StringIndexOutOfBoundsException`, not
/// `ArrayIndexOutOfBoundsException`, including the one about the destination
/// ARRAY: `String.getChars` does its own checking before the `arraycopy`.
pub(crate) fn native_string_get_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // bug nb-lang-string: getChars indexes over UTF-16 code UNITS, not Unicode
    // code points. The old `s.chars()` (one entry per code point) put
    // supplementary chars in a single slot and shifted every later index,
    // corrupting the dest array.
    //
    // `read_string_chars`, not `ctx.read_string(..).encode_utf16()`: the range
    // check below is against `length()`, so it must read the `value` array
    // through the SAME path `String.length()` does. Going via a Rust `String`
    // also substitutes U+FFFD for every unpaired surrogate, which this method
    // must copy out intact.
    let units = read_string_chars(ctx, this);
    let length = units.len() as i32;
    // Check one: the SOURCE range, before `dst` is looked at.
    if src_begin < 0 || src_begin > src_end || src_end > length {
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_range(src_begin, src_end, length).into(),
        );
    }
    // Check two: the destination reference.
    let dst = match args.get(3) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read the array length because \"dst\" is null",
            ))
        }
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Check three: the destination range, `checkBoundsOffCount` style.
    let count = src_end - src_begin;
    let dst_len = ctx.array_length(dst) as i32;
    if bounds_off_count_violation(dst_begin, count, dst_len).is_some() {
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_range_size(dst_begin, count, dst_len)
                .into(),
        );
    }
    let src_begin = src_begin as usize;
    let dst_begin = dst_begin as usize;
    for (i, &cu) in units
        .iter()
        .enumerate()
        .take(src_end as usize)
        .skip(src_begin)
    {
        ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(cu as i32));
    }
    Ok(None)
}

/// The `String.strip*` family's shared walk.
///
/// W7-95a. `strip`/`stripLeading`/`stripTrailing` are specified in terms of
/// `Character.isWhitespace`, which is not `char::is_whitespace` — see
/// [`java_char_is_whitespace`] for the eight code points that disagree and for
/// the measurement. Measured, code units in / code units out: `strip()` of
/// `[2007, 0078, 2007]` is `[8199, 120, 8199]` on HotSpot 25.0.3+9 and was
/// `[120]` here (`U+2007` FIGURE SPACE is a non-breaking space, which Java
/// deliberately does not treat as whitespace); `strip()` of
/// `[001C, 0078, 001C]` is `[120]` there and was `[28, 120, 28]` here.
///
/// Java strips by code POINT, so the walk skips a whole surrogate pair — but
/// no supplementary code point is whitespace, so the pair never strips and
/// walking by code unit gives the same answer. Reading units rather than a
/// `str` also keeps an unpaired surrogate out of the U+FFFD substitution.
fn string_strip_range(units: &[u16], leading: bool, trailing: bool) -> (usize, usize) {
    let mut start = 0;
    let mut end = units.len();
    if leading {
        while start < end && java_char_is_whitespace(units[start]) {
            start += 1;
        }
    }
    if trailing {
        while end > start && java_char_is_whitespace(units[end - 1]) {
            end -= 1;
        }
    }
    (start, end)
}

fn native_string_strip_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    leading: bool,
    trailing: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let units = read_string_chars(ctx, this);
    let (start, end) = string_strip_range(&units, leading, trailing);
    let stripped = String::from_utf16_lossy(&units[start..end]);
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&stripped),
    ))))
}

pub(crate) fn native_string_strip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_string_strip_impl(ctx, args, true, true)
}

pub(crate) fn native_string_strip_leading(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_string_strip_impl(ctx, args, true, false)
}

pub(crate) fn native_string_strip_trailing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_string_strip_impl(ctx, args, false, true)
}

/// `String.copyValueOf(char[])` — and, registered under the same body,
/// `String.valueOf(char[])`.
///
/// Both are `new String(data)`, which dereferences the array for its length.
/// Measured on OpenJDK 25.0.3+9: `String.copyValueOf(null)`,
/// `String.valueOf((char[]) null)`, `new String((char[]) null)` and
/// `new String((char[]) null, 0, 1)` all throw NullPointerException. This body
/// answered `""` — and `""` is a legal result of this method, so nothing
/// downstream could tell the difference.
///
/// `String.valueOf((Object) null)` is the row that does NOT throw: it is
/// specified as the four-character string `"null"`, and
/// `native_string_value_of_object` already answers that.
pub(crate) fn native_string_copy_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = char[]
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read the array length because \"value\" is null",
            ))
        }
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
    // `read_string_chars`, NOT `ctx.read_string(..).encode_utf16()`. W7-95a:
    // that round trip goes through a Rust `String`, a Rust `str` cannot hold
    // an unpaired surrogate, and `String::from_utf16_lossy` therefore replaces
    // one with U+FFFD. `"x\uD800y".codePointAt(1)` measured 65533 here against
    // HotSpot 25.0.3+9's 55296 — a 65533 in a code-point answer is the
    // diagnosis, not a coincidence. `read_string_chars` decodes the String's
    // own `value` array to UTF-16 code units and never sees UTF-8.
    let chars = read_string_chars(ctx, this);
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

/// `String.codePointCount(int, int)`.
///
/// W7-95a. Two defects, both from the same three lines: the bounds were cast
/// straight to `usize` (so a negative `beginIndex` became a huge index that
/// the `while` simply skipped) and `endIndex` was *clamped* to the length
/// instead of rejected. Every out-of-range call therefore returned a number
/// where HotSpot throws. Measured on 25.0.3+9:
///
/// ```text
/// "a<U+1F600>b".codePointCount(3, 1)   IndexOutOfBoundsException: Range [3, 1) out of bounds for length 4
/// "a<U+1F600>b".codePointCount(0, 9)   IndexOutOfBoundsException: Range [0, 9) out of bounds for length 4
/// "a<U+1F600>b".codePointCount(-1, 2)  IndexOutOfBoundsException: Range [-1, 2) out of bounds for length 4
/// ```
///
/// The thrown class is the SUPERCLASS `IndexOutOfBoundsException`, not
/// `StringIndexOutOfBoundsException`: this call site reaches `Preconditions`
/// without the `String` domain's exception formatter. `codePointAt` a few
/// lines above *does* get the SIOOBE formatter, so the two neighbours throw
/// different classes on purpose — a `catch (StringIndexOutOfBoundsException)`
/// around this one would not fire on HotSpot either.
pub(crate) fn native_string_code_point_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let begin_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let end_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // `read_string_chars` for the surrogate reason spelled out in
    // `native_string_code_point_at`: a lossy UTF-8 round trip turns the
    // unpaired surrogates this method exists to count into U+FFFD.
    let chars = read_string_chars(ctx, this);
    let length = chars.len() as i32;
    if begin_i32 < 0 || begin_i32 > end_i32 || end_i32 > length {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                i64::from(begin_i32),
                i64::from(end_i32),
                i64::from(length),
            ),
        )
        .into());
    }
    let begin = begin_i32 as usize;
    let end = end_i32 as usize;
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

/// `String.offsetByCodePoints(int, int)`.
///
/// W7-95a. The old body was **VM-fatal**, not merely wrong. `index` was cast
/// to `usize`, so any `index > length()` survived; the backward branch then
/// did `pos -= 1` followed by an unguarded `chars[pos]`, and
/// `"a<U+1F600>b".offsetByCodePoints(10, -1)` indexed a 4-element `Vec` at 9.
/// A Rust panic is not a Java throwable — it terminates the VM, from one line
/// of ordinary application bytecode. `for _ in 0..(-code_point_offset)` was a
/// second one: negating `Integer.MIN_VALUE` overflows `i32`, which panics in
/// any debug-assertions profile.
///
/// Both branches now transcribe `Character.offsetByCodePoints`, whose loops
/// are bounded by `i < limit` / `i > start` and which reports a *shortfall*
/// (`x > 0` after the loop) as the throw. Measured on OpenJDK 25.0.3+9, all
/// four of these throw `IndexOutOfBoundsException` with a **null** message —
/// the plain superclass, no `Preconditions` text, unlike `codePointCount`'s
/// neighbouring `Range [a, b)` wording:
///
/// ```text
/// "a<U+1F600>b".offsetByCodePoints(0, 9)     IndexOutOfBoundsException  (ran off the end)
/// "a<U+1F600>b".offsetByCodePoints(0, -1)    IndexOutOfBoundsException  (ran off the start)
/// "a<U+1F600>b".offsetByCodePoints(10, -1)   IndexOutOfBoundsException  (index > length)
/// "a<U+1F600>b".offsetByCodePoints(-1, 1)    IndexOutOfBoundsException  (index < 0)
/// ```
///
/// while the in-range answers are `offsetByCodePoints(0, 2) == 3` and
/// `offsetByCodePoints(4, -2) == 1` — the surrogate pair counts as one step.
pub(crate) fn native_string_offset_by_code_points(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let code_point_offset = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // `read_string_chars`: see `native_string_code_point_at`.
    let chars = read_string_chars(ctx, this);
    let length = chars.len() as i32;
    // `String.offsetByCodePoints`'s own precondition. Note `index > length()`,
    // not `>=`: the position one past the last char is a legal starting point
    // for a backward walk.
    if index_i32 < 0 || index_i32 > length {
        return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
    }
    let mut pos = index_i32 as usize;
    if code_point_offset >= 0 {
        // `Character.offsetByCodePoints`, forward arm: `x` counts the steps
        // still owed, and a nonzero `x` at the end is the throw.
        let mut x = code_point_offset;
        while x > 0 && pos < chars.len() {
            let ch = chars[pos];
            pos += 1;
            if (0xD800..=0xDBFF).contains(&ch)
                && pos < chars.len()
                && (0xDC00..=0xDFFF).contains(&chars[pos])
            {
                pos += 1;
            }
            x -= 1;
        }
        if x > 0 {
            return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
        }
    } else {
        // Backward arm. `code_point_offset.unsigned_abs()` rather than
        // `-code_point_offset`: the latter panics on `Integer.MIN_VALUE`.
        let mut x = i64::from(code_point_offset.unsigned_abs());
        while x > 0 && pos > 0 {
            pos -= 1;
            if (0xDC00..=0xDFFF).contains(&chars[pos])
                && pos > 0
                && (0xD800..=0xDBFF).contains(&chars[pos - 1])
            {
                pos -= 1;
            }
            x -= 1;
        }
        if x > 0 {
            return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
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
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): a 1-field
        // instance of the `java.util.stream.Stream` INTERFACE is a synthetic
        // stream stand-in, which is precisely what a strict run must not get in
        // place of `java.base`'s own pipeline.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/util/stream/Stream", 1)?,
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
            // W7-95a, found while auditing this file for panics. Java's rule is
            // `s.substring(Math.min(-n, s.indexOfNonWhitespace()))` — a count of
            // CHARACTERS, over `Character.isWhitespace`. The old body was
            // `line.len() - line.trim_start().len()`, a count of BYTES over
            // Rust's Unicode `White_Space`, followed by `&line[skip..]`. Two
            // defects, and the second is VM-fatal:
            //
            //  * Wrong table. Measured on OpenJDK 25.0.3+9, printed as code
            //    units: `indent(-1)` of `[00A0, 0061]` is `[160, 97, 10]` —
            //    Java KEEPS a leading non-breaking space, because
            //    `Character.isWhitespace` excludes it. Rust's `trim_start`
            //    removes it.
            //  * **Slicing a `str` at a byte offset that is not a character
            //    boundary panics**, and a Rust panic is not a Java throwable:
            //    it terminates the VM. That exact input reached it — U+00A0 is
            //    two UTF-8 bytes, `trim_start` reported `spaces == 2`, `-n`
            //    was 1, and `&line[1..]` landed inside the character.
            //
            // `n.unsigned_abs()` rather than `-n` for the third one:
            // `indent(Integer.MIN_VALUE)` is specified as `stripLeading()`, and
            // negating `Integer.MIN_VALUE` overflows `i32`. Capping the
            // character count at `non_ws` makes the MIN_VALUE case fall out as
            // "strip all of it", which is what HotSpot answers.
            let remove = n.unsigned_abs() as usize;
            let non_ws = line
                .chars()
                .position(|c| !char_is_java_whitespace(c))
                .unwrap_or_else(|| line.chars().count());
            let skip_chars = remove.min(non_ws);
            // Convert a CHARACTER count back to a byte offset through
            // `char_indices`, which can only ever land on a boundary.
            let byte_off = line
                .char_indices()
                .nth(skip_chars)
                .map_or(line.len(), |(i, _)| i);
            result.push_str(&line[byte_off..]);
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
        // Returning the receiver made `s.transform(null)` behave like the
        // identity function — a plausible, silently wrong answer. `transform`
        // is `f.apply(this)`, so a null `f` is an NPE on OpenJDK 25.0.3+9.
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"java.util.function.Function.apply(Object)\" because \"f\" is null",
            ))
        }
    };
    ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(this))],
    )
}

// ---------------------------------------------------------------------------
// java.util.Formatter's NEGATIVE surface: the refusals, and the flags that
// change a value rather than decorate it.
// ---------------------------------------------------------------------------
//
// `probes/ShadowDifferentialProbe` measured five refusals against HotSpot 25
// on 2026-08-12 and CratonVM answered `no-throw` to every one of them
// (`docs/known-issues/jdk-only/W7-32-round-2-differential-run.md`). A
// formatter that never refuses is the shape W2-7 named: the negative half of
// the API answering "here you go".
//
// The type matters as much as the throw. `java.util.Formatter` specifies a
// DISTINCT `IllegalFormatException` subclass per failure, and callers catch
// the subclass — a mistyped refusal walks past the `catch` that was written
// for it and lands in one that was not. So each fault below carries the
// class the javadoc names, and is constructed through that class's REAL
// constructor rather than by allocating a Throwable and stuffing a message
// into slot 0: all six override `getMessage()` and never set
// `Throwable.detailMessage` (the same trap that kept
// `java/util/regex/PatternSyntaxException` out of the synthetic-exception
// bridge list in `lib.rs`).

/// A `java.util.Formatter` refusal, as the class the javadoc names for it.
///
/// Every variant's payload is exactly its JDK constructor's arguments, so
/// [`fmt_raise`] is a straight translation with no message reconstruction.
///
/// The variants cover every member of the `java.util.IllegalFormatException`
/// family that `Formatter`'s own parser and printers can actually reach on
/// JDK 25. Two members of the family are deliberately absent:
///
/// * `UnknownFormatFlagsException` is UNREACHABLE from `Formatter`. Its only
///   throw site is `Flags.parse(char)`'s `default` arm, and the only caller of
///   `Flags.parse` is `FormatSpecifier.flags(s, start, end)` over exactly the
///   run of characters `FormatSpecifierParser.parseFlag` accepted — and that
///   accepts precisely the eight characters `Flags.parse(char)` recognises.
///   The `default` arm is dead for every format string; only a hand-built
///   caller of the package-private `Flags` could reach it. Modelling it here
///   would be a variant no input can produce.
/// * `FormatterClosedException` is not in the family at all — it extends
///   `IllegalStateException`, not `IllegalFormatException`, and belongs to
///   `Formatter`'s lifecycle rather than to its format strings.
enum FmtFault {
    /// `%q` — "Conversion = 'q'". The conversion character is not one the
    /// javadoc's table defines.
    UnknownConversion(String),
    /// `%s %s` with one argument — "Format specifier '%s'". Carries the
    /// specifier's own text, which is what the JDK's message quotes.
    MissingArgument(String),
    /// `%d` of a String — "d != java.lang.String". Carries the conversion
    /// character and the argument's class.
    WrongType(char, cratonvm_types::ClassId),
    /// `%-08d` — "Flags = '-0'". Two flags that contradict each other; the
    /// message lists the specifier's WHOLE flag set.
    IllegalFlags(String),
    /// `%,x` — "Conversion = x, Flags = ,". A flag that is legal in general
    /// but not for this conversion; the message lists only the OFFENDING
    /// flags.
    FlagsMismatch(String, char),
    /// `%.2d` — "2". A precision on a conversion that has no fractional part.
    IllegalPrecision(i32),
    /// `%5n` — "5". A width on a conversion that has no field to justify in.
    IllegalWidth(i32),
    /// `%-d` — "%-d". `'-'` and `'0'` are relative to a field width, so they
    /// are meaningless without one; the message is the specifier's own text.
    MissingWidth(String),
    /// `%--8d` — "Flags = '-'". The same flag given twice.
    DuplicateFlags(String),
    /// `%c` of `0x110000` — "Code point = 0x110000". An `int`/`short`/`byte`
    /// argument to `%c` that `Character.isValidCodePoint` rejects. The message
    /// is `String.format("Code point = %#x", c)`, so a NEGATIVE code point
    /// renders as its unsigned 32-bit hex ("Code point = 0xffffffff").
    IllegalCodePoint(i32),
    /// `%0$s` — "Illegal format argument index = 0". An explicit argument
    /// index that is not a positive `int`. `Integer.MIN_VALUE` is the JDK's
    /// sentinel for "the digits did not parse as an int at all", and it prints
    /// a different message; see [`FmtFault::message`].
    ArgumentIndex(i32),
}

impl FmtFault {
    /// The class the javadoc names, and its constructor.
    fn class_and_ctor(&self) -> (&'static str, &'static str) {
        match self {
            FmtFault::UnknownConversion(_) => (
                "java/util/UnknownFormatConversionException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::MissingArgument(_) => (
                "java/util/MissingFormatArgumentException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::WrongType(..) => (
                "java/util/IllegalFormatConversionException",
                "(CLjava/lang/Class;)V",
            ),
            FmtFault::IllegalFlags(_) => (
                "java/util/IllegalFormatFlagsException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::FlagsMismatch(..) => (
                "java/util/FormatFlagsConversionMismatchException",
                "(Ljava/lang/String;C)V",
            ),
            FmtFault::IllegalPrecision(_) => ("java/util/IllegalFormatPrecisionException", "(I)V"),
            FmtFault::IllegalWidth(_) => ("java/util/IllegalFormatWidthException", "(I)V"),
            FmtFault::MissingWidth(_) => (
                "java/util/MissingFormatWidthException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::DuplicateFlags(_) => (
                "java/util/DuplicateFormatFlagsException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::IllegalCodePoint(_) => {
                ("java/util/IllegalFormatCodePointException", "(I)V")
            }
            // The one member of the family that is PACKAGE-PRIVATE: JDK 25
            // declares `final class IllegalFormatArgumentIndexException` with
            // a package-private constructor, so only `java.util` code can name
            // it. `getClass().getName()` still reports the full name, which is
            // what a differential transcript records, and `fmt_raise` falls
            // back to the base class if the construction is refused — so
            // asking for it costs nothing if this VM ever grows the access
            // check that HotSpot would apply to a non-`java.util` caller.
            FmtFault::ArgumentIndex(_) => {
                ("java/util/IllegalFormatArgumentIndexException", "(I)V")
            }
        }
    }

    /// The message the JDK's overridden `getMessage()` would build. Used ONLY
    /// by the fallback below, where the specified class could not be
    /// constructed — the real objects build it themselves.
    fn message(&self, ctx: &dyn NativeContext) -> String {
        match self {
            FmtFault::UnknownConversion(s) => format!("Conversion = '{s}'"),
            FmtFault::MissingArgument(s) => format!("Format specifier '{s}'"),
            FmtFault::WrongType(c, cid) => format!(
                "{c} != {}",
                ctx.class_name_of_id(*cid).unwrap_or_default().replace('/', ".")
            ),
            FmtFault::IllegalFlags(f) => format!("Flags = '{f}'"),
            FmtFault::FlagsMismatch(f, c) => format!("Conversion = {c}, Flags = {f}"),
            FmtFault::IllegalPrecision(p) | FmtFault::IllegalWidth(p) => p.to_string(),
            FmtFault::MissingWidth(s) => s.clone(),
            FmtFault::DuplicateFlags(f) => format!("Flags = '{f}'"),
            // `String.format("Code point = %#x", c)` over an `int`: `%x`
            // renders a negative `int` as unsigned 32-bit, so the cast is part
            // of the message and not a convenience.
            FmtFault::IllegalCodePoint(c) => format!("Code point = {:#x}", *c as u32),
            FmtFault::ArgumentIndex(i) => {
                if *i == i32::MIN {
                    "Format argument index: (not representable as int)".to_string()
                } else {
                    format!("Illegal format argument index = {i}")
                }
            }
        }
    }
}

/// Is the named `java.util.IllegalFormatException` subclass available to be
/// thrown as itself?
///
/// This is the whole of W7-41. The predicate used to be
/// `ctx.class_id_by_name(name).is_some()`, and `class_id_by_name` is
/// `find_unique_class_by_name` — an index read over the ALREADY-LOADED
/// classes, with no loading of its own. Nothing in a normal program ever
/// touches `java.util.UnknownFormatConversionException` before the moment
/// `String.format` needs to throw it, so that predicate was false on every
/// first refusal and the fallback below ran instead: right message, wrong
/// class. `format.unknownConversion` was measured as
/// `java.lang.IllegalArgumentException: Conversion = 'q'` — the base class
/// carrying the message this file reconstructs — which is that fallback's
/// exact signature. See W7-40-differential-at-14.md.
///
/// So: ask the class loader, not the index. Two screens keep the load from
/// making things worse than the defect it fixes:
///
/// * `would_fabricate_synthetic_stub` is checked FIRST and is non-destructive.
///   A build with no real `java.util` exception hierarchy would answer the
///   load with a minted stub that has no `<init>` and does not extend
///   `IllegalArgumentException`; throwing that would turn a wrong-superclass
///   defect into an object no `catch (IllegalArgumentException)` can catch.
/// * after the load, the class must actually BE an `IllegalArgumentException`.
///   That is the invariant the fallback relies on (see [`fmt_raise`]), and it
///   is cheap to confirm rather than assume.
fn fmt_exception_class_available(ctx: &mut dyn NativeContext, class_name: &str) -> bool {
    let class_id = match ctx.class_id_by_name(class_name) {
        Some(id) => id,
        None => {
            if ctx.would_fabricate_synthetic_stub(class_name) {
                return false;
            }
            // The `Err` is dropped on purpose. A `ClassNotFoundException` from
            // this speculative load is not the answer `String.format` owes its
            // caller — the format refusal is — and CratonVM carries a thrown
            // exception in the return value rather than in thread state, so
            // there is nothing left pending to clear.
            if ctx.load_class(class_name).is_err() {
                return false;
            }
            match ctx.class_id_by_name(class_name) {
                Some(id) => id,
                None => return false,
            }
        }
    };
    match ctx.class_id_by_name("java/lang/IllegalArgumentException") {
        Some(base) => ctx.is_subclass(class_id, base),
        // ACCEPT on an unanswerable screen, do not refuse. `class_id_by_name`
        // is `find_unique_class_by_name`, which returns `None` for an
        // AMBIGUOUS name as well as an absent one — and refusing on that would
        // re-create the exact defect this function exists to fix, silently, on
        // whatever configuration defines `IllegalArgumentException` twice.
        // Nothing is lost by accepting: the stub screen above has already run,
        // so what is being accepted here is a real loaded class named by the
        // javadoc, which is a better answer than the base class either way.
        None => true,
    }
}

/// Turn a [`FmtFault`] into the thrown Java exception.
///
/// Constructed through the class's real `<init>` because every one of them
/// overrides `getMessage()` off its own fields — allocating the class and
/// setting a `detailMessage` would produce an object whose `getMessage()` is
/// still null.
///
/// The fallback is `IllegalArgumentException`, which is the SUPERCLASS of
/// `IllegalFormatException` and therefore never sends a caller down a `catch`
/// branch it did not ask for; it is reached only when the specified class is
/// genuinely unavailable (see [`fmt_exception_class_available`]), never to
/// make a diff go away.
fn fmt_raise(ctx: &mut dyn NativeContext, fault: &FmtFault) -> MethodCallFailed {
    let (class_name, ctor) = fault.class_and_ctor();
    if fmt_exception_class_available(ctx, class_name) {
        let ctor_args: Vec<Value> = match fault {
            FmtFault::UnknownConversion(s)
            | FmtFault::MissingArgument(s)
            | FmtFault::IllegalFlags(s)
            | FmtFault::MissingWidth(s)
            | FmtFault::DuplicateFlags(s) => {
                let obj = ctx.create_string(s);
                vec![Value::Object(Some(obj))]
            }
            FmtFault::WrongType(c, cid) => {
                let mirror = ctx.get_class_mirror(*cid);
                vec![Value::Int(*c as i32), Value::Object(Some(mirror))]
            }
            FmtFault::FlagsMismatch(f, c) => {
                let obj = ctx.create_string(f);
                vec![Value::Object(Some(obj)), Value::Int(*c as i32)]
            }
            FmtFault::IllegalPrecision(p)
            | FmtFault::IllegalWidth(p)
            | FmtFault::IllegalCodePoint(p)
            | FmtFault::ArgumentIndex(p) => vec![Value::Int(*p)],
        };
        match ctx.new_object_initialized(class_name, ctor, &ctor_args) {
            Ok(Some(Value::Object(Some(exc)))) => return MethodCallFailed::ExceptionThrown(exc),
            // An `InternalError` is a VM fault (heap exhaustion, a broken
            // class file) and must not be dressed up as a format refusal.
            // A thrown Java exception, though, means the constructor itself
            // refused — a missing or inaccessible `<init>` on a class that
            // passed the screens above — and the caller is owed the refusal it
            // asked for, in the base class the subclass would have extended.
            Err(err @ MethodCallFailed::InternalError(_)) => return err,
            Err(MethodCallFailed::ExceptionThrown(_)) | Ok(_) => {}
        }
    }
    cratonvm_types::error::RuntimeError::IllegalArgumentException {
        message: fault.message(ctx),
    }
    .into()
}

/// The flag characters `java.util.Formatter` accepts, in `Flags.toString`'s
/// canonical order — which is the order every flag-bearing message lists them
/// in, regardless of the order they were written in the format string
/// (`%+ d` reports "Flags = '+ '", `%-08d` reports "Flags = '-0'").
const FMT_FLAG_ORDER: &str = "-#+ 0,(<";

/// Every character `java.util.Formatter`'s package-private `DateTime.isValid`
/// admits after a `%t`/`%T` prefix, verbatim from its `switch` on JDK 25.
///
/// The list is the JDK's, not CratonVM's: this decides whether a field is an
/// `UnknownFormatConversionException` ("t" + the character), which is a
/// question about the SPECIFIER and must be answered the same way whether or
/// not this VM happens to implement the field. `format_temporal_field`
/// currently implements all 31 of them, so the two sets coincide today —
/// but they are separate questions and a future gap must not silently become
/// a mistyped refusal.
const FMT_DATETIME_FIELDS: &str = "HIklMNLQpsSTzZaAbBCdehjmyYrRcDF";

/// `CharSequence.length()` for a Rust `str` — the count of UTF-16 code units.
///
/// Every width decision `java.util.Formatter` makes is
/// `width - cs.length()` in `appendJustified`, and a Java `length()` counts
/// UTF-16 code units. Rust's `str::len()` counts BYTES, and reaching for it
/// here padded non-ASCII output short: `String.format("%5s|", "\u{00e9}")`
/// came back with three spaces where HotSpot 25 writes four (é is one code
/// unit but two UTF-8 bytes), and an astral character — four bytes, TWO code
/// units — lost two more. ASCII is the case where the two agree, which is why
/// every existing row passed.
fn fmt_utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.substring(0, precision)` — the precision truncation every GENERAL
/// conversion applies, counted in UTF-16 code units like `String.substring`.
///
/// This is now a plain `Vec::truncate` on code UNITS, and that is the whole
/// point. It replaced a `char_indices` walk over a Rust `&str`, which got two
/// separate things wrong:
///
///   * Counting CODE POINTS is wrong even when nothing straddles the cut:
///     `%.3s` of `"a<U+1F600>b"` is `"a<U+1F600>"` on HotSpot (three code
///     UNITS) and was `"a<U+1F600>b"` here.
///   * When the cut lands INSIDE a surrogate pair the JDK keeps the LONE HIGH
///     surrogate, and a Rust `String` cannot hold one — so the walk had to stop
///     before the pair and came back one code unit short. Measured on HotSpot
///     25 (`String.format(Locale.ROOT, …)` of `"😀"`, U+1F600):
///
///     | format | HotSpot | units |
///     |---|---|---|
///     | `%.1s` | length 1 | `D83D` |
///     | `[%5.1s]` | length **7** | `005B 0020 0020 0020 0020 D83D 005D` |
///     | `%.1S` | length 1 | `D83D` |
///     | `%.2s` of `"\uD800\uD800\uD800"` | length 2 | `D800 D800` |
///
///     The `[%5.1s]` row is the one that shows why the truncation and the
///     justifier had to move to units TOGETHER: the pad is computed from the
///     truncated length, so a truncation that came back short also padded one
///     space too many.
fn fmt_truncate_units(units: &mut Vec<u16>, prec: usize) {
    units.truncate(prec);
}

/// A run of `n` copies of the ASCII padding character `ch`, allocated
/// **fallibly**.
///
/// Every padding run in this file goes through here, and the reason is that a
/// width is caller data. `FormatSpecifier.width` refuses only a digit run that
/// overflows an `int`, so `%2000000000d` is a perfectly legal specifier that
/// asks for two billion spaces. `str::repeat` reaches `handle_alloc_error` when
/// that allocation fails, and a Rust abort is not a Java throwable: HotSpot 25
/// answers the same format string with a **catchable**
/// `OutOfMemoryError: Java heap space` (measured at `-Xmx256m`; at `-Xmx1g` the
/// same VM SERVES `%100000000d` and hands back a 100-million-character String,
/// which is why this is a fallible allocation and not a fixed cap — a cap would
/// refuse a width HotSpot answers).
///
/// `try_reserve_exact` is the same primitive [`native_string_repeat`] uses for
/// the same reason, one screen away in this file.
fn fmt_repeat(ch: char, n: usize) -> Result<String, MethodCallFailed> {
    let bytes = n
        .checked_mul(ch.len_utf8())
        .ok_or_else(fmt_out_of_memory)?;
    let mut s = String::new();
    if s.try_reserve_exact(bytes).is_err() {
        return Err(fmt_out_of_memory());
    }
    for _ in 0..n {
        s.push(ch);
    }
    Ok(s)
}

/// Ask the allocator for `n` bytes and give them straight back — the same
/// question [`fmt_repeat`] asks, for the callers that then build the run
/// themselves.
///
/// The PRECISION is the second unbounded axis and it behaves exactly like the
/// width. Measured on HotSpot 25 at `-Xmx256m`, all with a `Double` argument:
///
/// ```text
/// %.2000000000f   OutOfMemoryError: Java heap space
/// %.2000000000e   OutOfMemoryError: Java heap space
/// %.2000000000a   OutOfMemoryError: Java heap space
/// %.1000000f      SERVED — a 1,000,002-character String
/// ```
///
/// so, again, a catchable throwable and not a fixed cap. `fmt_render_fixed`,
/// `fmt_render_scientific` and `fmt_hex_pad` each push one character per
/// fraction digit into a growing `String`, which is a `handle_alloc_error`
/// abort at two billion; this probe runs once, at the single point in
/// `format_arg_full` where a precision reaches any of them, so none of their
/// signatures has to become fallible for the abort to stop being one.
fn fmt_reserve_probe(n: usize) -> Result<(), MethodCallFailed> {
    let mut s = String::new();
    if s.try_reserve_exact(n).is_err() {
        return Err(fmt_out_of_memory());
    }
    Ok(())
}

/// The `OutOfMemoryError` HotSpot 25 raises when a format's own width or
/// precision outruns the heap, message included (measured).
fn fmt_out_of_memory() -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: "Java heap space".to_string(),
    }
    .into()
}

/// `java.util.Formatter.appendJustified` — **the** width justifier, for every
/// conversion in the table.
///
/// It used to be one of two. This body served `%t`/`%T` and the null path,
/// while `format_arg_full` carried a second, hand-inlined copy of the same
/// `width - cs.length()` for everything else; the two had already disagreed
/// once (the second counted UTF-8 bytes where this one counts UTF-16 code
/// units) and were fixed in parallel rather than merged. The survivor is this
/// one, and it counts code units through [`fmt_utf16_len`] — checked by
/// reading the body below, which has no `str::len` in it and no second
/// definition anywhere: `grep -n 'fmt_pad_to_width(' lang_string.rs` now
/// reaches every justification decision the file makes.
///
/// The zero-padding `format_arg_full` still does is NOT this function and was
/// not merged into it: the JDK's `trailingZeros` runs *inside* the numeric
/// printers, before `appendJustified` ever sees the buffer, and by the time it
/// has run the buffer is already `width` units long so this pads nothing.
///
/// Fallible for the reason [`fmt_repeat`] documents.
///
/// Takes and returns UTF-16 code UNITS. It used to take a `String`, and every
/// caller's output had already been through a lossy `str` round trip by the
/// time it arrived — so the one function that is *defined* as counting code
/// units was the one place a lone surrogate could no longer be present to be
/// counted. `out.len()` is now the count directly, which is also why
/// [`fmt_utf16_len`] no longer appears in this body: it survives only for the
/// `String`-shaped numeric decoration in `format_arg_full`, whose output is
/// ASCII by construction.
fn fmt_pad_to_width(
    out: Vec<u16>,
    flags: &str,
    width: Option<usize>,
) -> Result<Vec<u16>, MethodCallFailed> {
    match width {
        Some(w) if out.len() < w => {
            let pad = fmt_repeat(' ', w - out.len())?;
            let pad: Vec<u16> = pad.encode_utf16().collect();
            let mut joined = Vec::new();
            if joined
                .try_reserve_exact(out.len().saturating_add(pad.len()))
                .is_err()
            {
                return Err(fmt_out_of_memory());
            }
            if flags.contains('-') {
                joined.extend_from_slice(&out);
                joined.extend_from_slice(&pad);
            } else {
                joined.extend_from_slice(&pad);
                joined.extend_from_slice(&out);
            }
            Ok(joined)
        }
        _ => Ok(out),
    }
}

/// [`fmt_pad_to_width`] for a caller that still holds `str`-shaped output.
///
/// Deliberately a one-line adapter and NOT a second justifier: the width rule
/// has exactly one body, for the reason [`fmt_pad_to_width`]'s own doc records
/// (it used to be one of two, and the two had already disagreed once).
fn fmt_pad_str_to_width(
    out: &str,
    flags: &str,
    width: Option<usize>,
) -> Result<Vec<u16>, MethodCallFailed> {
    fmt_pad_to_width(out.encode_utf16().collect(), flags, width)
}

/// [`fmt_upper_case`] on code UNITS.
///
/// `toUpperCaseWithLocale` is defined on a `String`, and the mapping tables
/// `case_map` owns are indexed by scalar value — neither can say anything about
/// an unpaired surrogate, and the JDK's own `String.toUpperCase` leaves one
/// alone. So the units are split on the surrogates that cannot participate:
/// each well-formed run is case-mapped as text, and each lone surrogate is
/// carried through verbatim. That keeps the two length-changing properties this
/// family gets wrong:
///
///   * the mapping may GROW the text (`%S` of `"ß"` is `"SS"`), which is
///     why this runs BEFORE the justifier — `String.format(ROOT, "%5S", "ß")`
///     is `"   SS"`, five units, measured on HotSpot 25; and
///   * a lone surrogate is one unit before and one unit after.
fn fmt_upper_case_units(ctx: &mut dyn NativeContext, units: &[u16], locale: FmtLocale) -> Vec<u16> {
    if !has_unpaired_surrogate(units) {
        let text = String::from_utf16_lossy(units);
        return fmt_upper_case(ctx, &text, locale).encode_utf16().collect();
    }
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut run: Vec<u16> = Vec::new();
    let mut i = 0usize;
    while i < units.len() {
        let u = units[i];
        let paired = (0xD800..=0xDBFF).contains(&u)
            && units
                .get(i + 1)
                .is_some_and(|n| (0xDC00..=0xDFFF).contains(n));
        if (0xD800..=0xDFFF).contains(&u) && !paired {
            if !run.is_empty() {
                let text = String::from_utf16_lossy(&run);
                out.extend(fmt_upper_case(ctx, &text, locale).encode_utf16());
                run.clear();
            }
            out.push(u);
            i += 1;
            continue;
        }
        run.push(u);
        i += 1;
        if paired {
            run.push(units[i]);
            i += 1;
        }
    }
    if !run.is_empty() {
        let text = String::from_utf16_lossy(&run);
        out.extend(fmt_upper_case(ctx, &text, locale).encode_utf16());
    }
    out
}

/// The word `java.util.Formatter`'s printers substitute for a **null**
/// argument, before any decoration.
///
/// Every printer in the table opens with
/// `if (arg == null) { print(fmt, "null", l); return; }` — except
/// `printBoolean`, which reaches the same shared printer with
/// `Boolean.toString(false)`. That one-conversion exception is the whole of
/// this function, and it exists so the rule is written once: `format_arg_full`
/// (which then applies the precision, the upper-caser and the justifier, as
/// `print(Formatter, String, Locale)` does) and [`format_arg`]'s own null arm
/// now read the SAME table rather than each carrying a copy of it.
fn fmt_null_text(spec: char) -> &'static str {
    if spec == 'b' || spec == 'B' {
        "false"
    } else {
        "null"
    }
}

/// `FormatSpecifier.toUpperCaseWithLocale` — the general family's upper-caser,
/// which is **locale-sensitive**.
///
/// ```java
/// s.toUpperCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))
/// ```
///
/// This used to be a bare `str::to_uppercase`, which is Rust's
/// locale-independent mapping. Measured on HotSpot 25:
///
/// | | `Locale.ROOT` | `Locale.forLanguageTag("tr")` |
/// |---|---|---|
/// | `%S` of `"i"` | `I` | `U+0130` |
/// | `%C` of `Character.valueOf('i')` | `I` | `U+0130` |
/// | `%TA` of a Monday | `MON` | `PAZARTES` + `U+0130` |
///
/// and, with `Locale.getDefault(FORMAT)` forced to `tr`, an EXPLICIT-null
/// locale (`String.format((Locale) null, "%S", "i")`) is `U+0130` as well —
/// which is why the two non-`Some` [`FmtLocale`] arms both resolve the default
/// here and neither takes the root mapping. (That is the opposite of
/// [`fmt_symbols_for`], where an explicit null really does mean "no
/// localization"; the two questions are asked of the same `Locale` argument and
/// answered differently by the JDK, so they are answered differently here.)
///
/// The rules themselves are NOT reimplemented: `case_map` already owns the
/// `tr`/`az`/`lt` port of `ConditionalSpecialCasing`, `String.toUpperCase`
/// already calls it, and this is its third caller. `case_map::to_upper_case`
/// early-outs to `jdk_to_uppercase` for every other language, so the ROOT path
/// gains only the JDK-version-skew correction that `String.toUpperCase` has
/// had since W7-95a.
///
/// Resolving the language runs no bytecode — `locale_language_for_case_mapping`
/// reads `baseLocale.language` off the `Locale` object, or a `OnceLock`'d
/// `user.language` for the default — so there is no re-entrancy latch here and
/// none is needed.
fn fmt_upper_case(ctx: &mut dyn NativeContext, s: &str, locale: FmtLocale) -> String {
    let lang = crate::locale_language_for_case_mapping(
        ctx,
        match locale {
            FmtLocale::Given(Some(l)) => Some(l),
            FmtLocale::Given(None) | FmtLocale::DefaultFormat => None,
        },
    );
    crate::case_map::to_upper_case(s, &lang)
}

/// The LOWER-case twin of [`fmt_upper_case`], for `%tp`'s AM/PM marker — the
/// only place `java.util.Formatter` lower-cases anything.
///
/// ```java
/// String[] ampm = { "AM", "PM" };
/// if (l != null && l != Locale.US) { ampm = DateFormatSymbols.getInstance(l).getAmPmStrings(); }
/// sb.append(ampm[…].toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT))));
/// ```
///
/// Identical in both of `printDateTime`'s printers, and the locale rule is the
/// upper-caser's: an explicit `null` takes the DEFAULT locale here, not
/// `Locale.US`, even though the STRING it is applied to is the English literal
/// in that case. That is why the two `FmtLocale` non-`Some` arms collapse
/// together below exactly as they do in [`fmt_upper_case`].
///
/// # This is a family convergence, and NO measured row moves
///
/// The body it replaces was `str::to_lowercase`, Rust's locale-independent
/// mapping — the exact mirror of the defect `W8-F4-1` N3 recorded on the UPPER
/// side and fixed there by routing through `case_map`. F28-1 §5.4 flagged the
/// twin without a live row and said so. I looked for one and there is none:
///
///  * Swept all **1158** locales this JDK reports available. For every one, and
///    for both of its `getAmPmStrings()` entries, `s.toLowerCase(locale)`
///    equals `s.toLowerCase(Locale.ROOT)` — **zero** divergent rows. Repeated
///    for the English literal `"AM"`/`"PM"` against every locale as the
///    case-mapping locale (the explicit-null and `Locale.US` paths): zero
///    again. The `tr`/`az`/`lt` rules need a dotted/dotless `I` or a
///    combining-dot sequence, and no AM/PM string in CLDR carries one.
///  * The other half of the change — `str::to_lowercase` → `jdk_to_lowercase`
///    — can only differ on [`crate::case_map::JDK_UNMAPPED_CASE_CODE_POINTS`],
///    which is `U+A7CE`, `U+A7CF`, `U+A7D2`..`U+A7D5`. No AM/PM string is in
///    Latin Extended-D.
///
/// It is landed anyway because the alternative is leaving one JDK rule with two
/// implementations, one of them fixed — this codebase's most common defect
/// shape, and the reason `%TA` under `tr` was wrong for as long as it was.
/// Whichever locale eventually grows a dotted `I` in its AM/PM data, this side
/// will already be right.
fn fmt_lower_case(ctx: &mut dyn NativeContext, s: &str, locale: FmtLocale) -> String {
    let lang = crate::locale_language_for_case_mapping(
        ctx,
        match locale {
            FmtLocale::Given(Some(l)) => Some(l),
            FmtLocale::Given(None) | FmtLocale::DefaultFormat => None,
        },
    );
    crate::case_map::to_lower_case(s, &lang)
}

/// `printString`'s first line: `if (arg instanceof Formattable)
/// { ((Formattable) arg).formatTo(fmt, flags, width, precision); }`.
///
/// `Ok(None)` means "not a `Formattable`" (or a VM with no usable
/// `java.util.Formatter`), and the caller falls through to `String.valueOf`.
/// `Ok(Some(units))` is the callee's output VERBATIM — the conversion applies
/// no precision, no upper-casing and no width on top of it, because `formatTo`
/// was handed all three and owns the decision. Measured on HotSpot 25 with a
/// `Formattable` whose `formatTo` writes `"x\uD800y"`: `%s`, `%S` and `%-10s`
/// all answer `length() == 3`, units `0078 D800 0079` — the `%S` row proving
/// the upper-caser does not run (an `x` that survived as `x`) and the `%-10s`
/// row proving the justifier does not; one whose `formatTo` writes `"😀"` under
/// `%.1s` answers TWO units, `D83D DE00`, so the precision does not truncate
/// either.
///
/// The output is read as UTF-16 code **units**. `formatTo` writes through a
/// `StringBuilder`, and `ctx.read_string` on that `StringBuilder`'s
/// `toString()` is a UTF-8 round trip that turns every unpaired surrogate into
/// U+FFFD — measured above, HotSpot keeps `D800`. This was the last general
/// conversion route still lossy after the rest of the pipeline moved to units,
/// and the loss was at THIS read, not downstream: the caller already returns
/// these units unchanged.
///
/// Measured on HotSpot 25 (a `Formattable` whose `formatTo` prints its three
/// arguments):
///
/// | format | flags | width | precision |
/// |---|---|---|---|
/// | `%s` | 0 | -1 | -1 |
/// | `%S` | 2 | -1 | -1 |
/// | `%#s` | 4 | -1 | -1 |
/// | `%-10s` | 1 | 10 | -1 |
/// | `%#-10.3S` | 7 | 10 | 3 |
/// | `%s %<s` (second) | **256** | -1 | -1 |
///
/// The first three bits are `java.util.FormattableFlags`' public constants
/// (`LEFT_JUSTIFY`/`UPPERCASE`/`ALTERNATE`); 256 is `Flags.PREVIOUS`, the
/// package-private `'<'` bit, which reaches a `Formattable` because
/// `checkGeneral` never removes it. Absent width and precision are `-1`, not
/// `0`. Also measured: `"[%10s]"` of a `Formattable` that writes nothing is
/// `"[]"` (the width is NOT applied afterwards), a `formatTo` that throws
/// propagates the exception out of `String.format`, and `%b`/`%h` of the same
/// object do NOT dispatch (`printBoolean` and `printHashCode` have no such
/// branch) — hence the `spec == 's'` guard at the call site.
///
/// **Why the interface is looked up but never loaded.** A `class_id_by_name`
/// miss is a conclusive "no" here: for `obj` to implement `Formattable`, the
/// interface must already have been resolved when `obj`'s own class was
/// linked, and `is_subclass` could not answer through an unloaded interface
/// anyway. So the hot `%s` path pays one index read and never a speculative
/// class load — unlike [`fmt_exception_class_available`], whose class genuinely
/// may not be loaded yet at the moment it is needed.
fn fmt_formattable_dispatch(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    uppercase_conversion: bool,
    locale: FmtLocale,
) -> Result<Option<Vec<u16>>, MethodCallFailed> {
    let Some(iface) = ctx.class_id_by_name("java/util/Formattable") else {
        return Ok(None);
    };
    let cid = ctx.class_id_of_object(obj);
    if cid != iface && !ctx.is_subclass(cid, iface) {
        return Ok(None);
    }

    // `java.util.FormattableFlags`, and `Flags.PREVIOUS` for the `'<'` bit.
    let mut bits = 0i32;
    if flags.contains('-') {
        bits |= 1;
    }
    if uppercase_conversion {
        bits |= 2;
    }
    if flags.contains('#') {
        bits |= 4;
    }
    if flags.contains('<') {
        bits |= 256;
    }
    let w = width.map_or(-1i32, |v| v as i32);
    let p = precision.map_or(-1i32, |v| v as i32);

    // GC: everything below allocates and runs bytecode, so every reference
    // held across a call is pinned and re-derived. `pin_base` is the FIRST
    // handle, so one `unpin_native_roots(pin_base)` releases the whole batch.
    let pin_base = ctx.pin_native_root(obj);

    // The `Locale` the callee's own `Formatter` must carry:
    // `if (fmt.locale() != l) fmt = new Formatter(fmt.out(), l);`. An EXPLICIT
    // null locale is handed on as null (measured: the callee sees
    // `formatter.locale() == null`), which is why `Given(None)` is not folded
    // into the default arm the way [`fmt_upper_case`] folds it.
    let locale_ref = match locale {
        FmtLocale::Given(l) => l,
        // The no-`Locale` overload formats against `Locale.getDefault(FORMAT)`;
        // this VM keeps a single default `Locale`, which is what
        // `fmt_symbols_for`'s `DefaultFormat` arm resolves through the JDK as
        // well. A failure here is not worth failing the format over — the
        // callee then sees a null locale, which is the old behaviour of every
        // no-`Locale` surface in this file.
        FmtLocale::DefaultFormat => match ctx.invoke(
            "java/util/Locale",
            "getDefault",
            "()Ljava/util/Locale;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(l)))) => Some(l),
            _ => None,
        },
    };
    let locale_pin = locale_ref.map(|l| ctx.pin_native_root(l));

    let sb = match ctx.new_object_initialized("java/lang/StringBuilder", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        // No usable `StringBuilder` means no `Appendable` to hand the callee.
        // Decline rather than refuse: the caller then prints `toString()`,
        // which is this conversion's behaviour before this function existed.
        _ => {
            ctx.unpin_native_roots(pin_base);
            return Ok(None);
        }
    };
    let sb_pin = ctx.pin_native_root(sb);
    let locale_ref = match (locale_ref, locale_pin) {
        (Some(l), Some(h)) => Some(ctx.read_native_pin(h, l)),
        _ => None,
    };
    let formatter = match ctx.new_object_initialized(
        "java/util/Formatter",
        "(Ljava/lang/Appendable;Ljava/util/Locale;)V",
        &[Value::Object(Some(sb)), Value::Object(locale_ref)],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(pin_base);
            return Ok(None);
        }
    };
    let fmt_pin = ctx.pin_native_root(formatter);

    let obj = ctx.read_native_pin(pin_base, obj);
    let formatter = ctx.read_native_pin(fmt_pin, formatter);
    let call = ctx.invoke_virtual(
        obj,
        "formatTo",
        "(Ljava/util/Formatter;III)V",
        &[
            Value::Object(Some(formatter)),
            Value::Int(bits),
            Value::Int(w),
            Value::Int(p),
        ],
    );
    let sb = ctx.read_native_pin(sb_pin, sb);
    if let Err(err) = call {
        // "one that throws must propagate" — `formatTo` is declared to throw
        // and HotSpot lets an `IllegalStateException` out of `String.format`
        // untouched. Unpin first; the exception is the caller's answer.
        ctx.unpin_native_roots(pin_base);
        return Err(err);
    }
    // `read_string_chars`, NOT `ctx.read_string(..)`: see the surrogate rows in
    // this function's doc. The `StringBuilder` holds whatever `formatTo` wrote,
    // including a lone surrogate, and the UTF-8 round trip is where it died.
    let out = match ctx.invoke_virtual(sb, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => read_string_chars(ctx, s),
        Ok(_) => Vec::new(),
        Err(err) => {
            ctx.unpin_native_roots(pin_base);
            return Err(err);
        }
    };
    ctx.unpin_native_roots(pin_base);
    Ok(Some(out))
}

/// Render a flag set in `Flags.toString`'s canonical order.
fn fmt_flags_string(flags: &str) -> String {
    FMT_FLAG_ORDER
        .chars()
        .filter(|f| flags.contains(*f))
        .collect()
}

/// Rebuild a specifier's own text, which is what `FormatSpecifier.toString`
/// produces and what the `MissingFormatArgument` / `MissingFormatWidth`
/// messages quote.
fn fmt_spec_text(
    explicit_index: Option<usize>,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    conversion: char,
) -> String {
    // `FormatSpecifier.toString` writes the FLAGS before the argument index,
    // and writes them in `Flags.toString`'s canonical order rather than the
    // order they were typed — `%#-s` reports itself as `%-#s`. The '<' flag
    // is one of them, so it survives here as written.
    let mut s = String::from("%");
    s.push_str(&fmt_flags_string(flags));
    if let Some(i) = explicit_index {
        s.push_str(&format!("{}$", i + 1));
    }
    if let Some(w) = width {
        s.push_str(&w.to_string());
    }
    if let Some(p) = precision {
        s.push('.');
        s.push_str(&p.to_string());
    }
    s.push(conversion);
    s
}

/// `java.util.Formatter`'s `checkGeneral`/`checkCharacter`/`checkInteger`/
/// `checkFloat`/`checkNumeric`, in their JDK order — the order decides WHICH
/// exception a doubly-illegal specifier gets.
///
/// The rules are the javadoc's, quoted where they are not obvious:
///
/// * numeric conversions — "If the `'-'` or `'0'` flags are given, then the
///   width is required" and `'+'` with `' '`, or `'-'` with `'0'`, is
///   "an illegal combination of flags".
/// * `'d'` — "If the `'#'` flag is given then a
///   FormatFlagsConversionMismatchException will be thrown."
/// * `'o'`/`'x'`/`'X'` — likewise for `','`, and (from `print(long, Locale)`)
///   for `'('`, `' '` and `'+'`, none of which a two's-complement rendering
///   has anywhere to put.
/// * every integral conversion — "If a precision is provided then an
///   IllegalFormatPrecisionException will be thrown", and the same for
///   `'c'`/`'C'`.
/// * `'e'`/`'E'` — grouping is a mismatch; `'a'`/`'A'` — grouping and
///   parentheses both are; `'g'`/`'G'` — `'#'` is.
fn fmt_check_spec(
    explicit_index: Option<usize>,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    conversion: char,
) -> Result<(), FmtFault> {
    let spec_text = || fmt_spec_text(explicit_index, flags, width, precision, conversion);
    let mismatch = |bad: &str| {
        let offending: String = fmt_flags_string(flags)
            .chars()
            .filter(|c| bad.contains(*c))
            .collect();
        if offending.is_empty() {
            Ok(())
        } else {
            // `failMismatch` reports the LOWER-case conversion, because
            // `Conversion.isValid` folds `%X`/`%E`/`%S` down to `x`/`e`/`s`
            // and keeps the upper-casing in a separate internal flag. Only
            // `UnknownFormatConversionException` sees the character as typed,
            // since an unknown one is never folded — `%Q` really does say
            // "Conversion = 'Q'".
            Err(FmtFault::FlagsMismatch(
                offending,
                conversion.to_ascii_lowercase(),
            ))
        }
    };
    // `IllegalFormatFlagsException` reports `Flags.toString(flags)` over the
    // WHOLE set, and that set carries the JDK's INTERNAL `UPPERCASE` flag —
    // added by `Conversion.isValid` for every upper-case conversion and
    // rendered as `'^'`, between `'-'` and `'#'`. So `%+ X` says
    // "Flags = '^+ '" where `%+ x` says "Flags = '+ '". `FormatSpecifier`'s
    // own `toString` explicitly REMOVES it again, which is why `spec_text`
    // does not carry it.
    let all_flags = || {
        let mut s = fmt_flags_string(flags);
        if conversion.is_ascii_uppercase() {
            s.insert(usize::from(s.starts_with('-')), '^');
        }
        s
    };
    // `checkNumeric`, shared by the integral and float families.
    let check_numeric = || -> Result<(), FmtFault> {
        if width.is_none() && (flags.contains('-') || flags.contains('0')) {
            return Err(FmtFault::MissingWidth(spec_text()));
        }
        if (flags.contains('+') && flags.contains(' '))
            || (flags.contains('-') && flags.contains('0'))
        {
            return Err(FmtFault::IllegalFlags(all_flags()));
        }
        Ok(())
    };

    match conversion {
        // General: 'b'/'B' 'h'/'H' 's'/'S'.
        'b' | 'B' | 'h' | 'H' | 's' | 'S' => {
            // `checkGeneral` rejects '#' up front for 'b'/'h' only; for
            // 's' the JDK gets there later, inside `printString`, which is
            // why `%#-s` reports the MISSING WIDTH and `%#-b` reports the
            // flag mismatch. Order is the whole of the difference.
            if matches!(conversion, 'b' | 'B' | 'h' | 'H') {
                mismatch("#")?;
            }
            if width.is_none() && flags.contains('-') {
                return Err(FmtFault::MissingWidth(spec_text()));
            }
            mismatch("+ 0,(")?;
            // `'#'` for 's'/'S' is NOT refused here, and the omission is the
            // point. `printString` opens with the `Formattable` dispatch and
            // only reaches `failMismatch(ALTERNATE, 's')` in its `else` — so
            // `%#s` of a `Formattable` is LEGAL and hands `ALTERNATE` to the
            // callee, while `%#s` of anything else still throws. Both halves
            // are measured on HotSpot 25 and both are in `format_arg_full`,
            // which is the only place the argument's class is known.
            //
            // Deferring it is visible in three other orderings, all measured:
            // `%#,s` is the ','  mismatch (this layer, before the argument),
            // `%#-s` is `MissingFormatWidthException: %-#s` (this layer), and
            // `%#s` with NO argument is
            // `MissingFormatArgumentException: Format specifier '%#s'` — the
            // argument fetch sits between the two layers, so hoisting the
            // refusal back up here would answer that last one wrongly.
        }
        'c' | 'C' => {
            if let Some(p) = precision {
                return Err(FmtFault::IllegalPrecision(p as i32));
            }
            mismatch("#+ 0,(")?;
            if width.is_none() && flags.contains('-') {
                return Err(FmtFault::MissingWidth(spec_text()));
            }
        }
        'd' | 'o' | 'x' | 'X' => {
            check_numeric()?;
            if let Some(p) = precision {
                return Err(FmtFault::IllegalPrecision(p as i32));
            }
            if conversion == 'd' {
                mismatch("#")?;
            } else {
                mismatch(",")?;
                // `print(long, Locale)`'s own `checkBadFlags(PARENTHESES |
                // LEADING_SPACE | PLUS)` is NOT here, deliberately: it runs
                // inside the printer, so it does not apply to a BigInteger
                // argument (whose printer has no such check) nor to a null one
                // (which never reaches a printer). It lives in
                // `format_arg_full`, where the argument's class is known.
            }
        }
        'e' | 'E' | 'f' | 'g' | 'G' | 'a' | 'A' => {
            check_numeric()?;
            match conversion {
                'a' | 'A' => mismatch("(,")?,
                'e' | 'E' => mismatch(",")?,
                'g' | 'G' => mismatch("#")?,
                _ => {}
            }
        }
        other => return Err(FmtFault::UnknownConversion(other.to_string())),
    }
    Ok(())
}

/// The four `java.text.DecimalFormatSymbols` characters
/// `java.util.Formatter` reads off a `Locale`.
///
/// The JDK's own
/// `getZero`/`getDecimalSeparator`/`getGroupingSeparator`/`getMinusSign`
/// helpers read exactly these four off
/// `DecimalFormatSymbols.getInstance(locale)`, and answer
/// `'0'`/`'.'`/`','`/`'-'` for a null locale.
///
/// # `minus` has exactly ONE consumer, and it is not a numeric conversion
///
/// F28-1 §5.3 recorded this symbol as missing and described its blast radius as
/// "`%tF`'s minus, and every negative `%d`/`%f`/`%e`/`%g`". **The second half of
/// that is wrong**, and the correction is why `minus` is affordable at all.
/// `getMinusSign` has a SINGLE call site in `java.base/java/util/Formatter.java`
/// (line 4635 of the JDK 25 source), inside the `TemporalAccessor` printer's
/// `ISO_STANDARD_DATE` arm. Every other negative number takes `leadingSign`,
/// which appends a bare ASCII `'-'` (or `'('`) and consults no locale at all.
///
/// Measured on HotSpot 25 under `lt-LT`, `et-EE`, `sl-SI`, `sv-SE` and `fi-FI`
/// — five of the 59 available locales whose `getMinusSign()` is U+2212 MINUS
/// SIGN — dumped as UTF-16 code units:
///
/// | conversion | argument | HotSpot 25 |
/// |---|---|---|
/// | `%d` | `-5` | `002D 0035` — ASCII |
/// | `%,d` | `-1234567` | `002D` … — ASCII |
/// | `%.2f` `%e` `%g` `%a` | `-1234.5` | `002D` … — ASCII |
/// | `%d` | `BigInteger("-5")` | `002D 0035` — ASCII |
/// | `%.2f` | `BigDecimal("-1.5")` | `002D` … — ASCII |
/// | `%tz` | `OffsetDateTime` at `-03` | `002D 0030 0033 0030 0030` — ASCII |
/// | `%tF` | `LocalDate.of(-44,3,15)` | **`2212`** `0030 0030 0034 0034 002D …` |
///
/// The last row is the whole of it: the leading year sign is localized and the
/// two `'-'` DATE SEPARATORS in the same field are not. So this symbol costs one
/// extra `getMinusSign` call per already-cached [`fmt_symbols_for`] resolution
/// and closes the row completely, rather than "one quarter of one row" against a
/// hot path — which is the trade F28 declined on a premise that had not been
/// measured.
#[derive(Clone, Copy)]
struct FmtSymbols {
    grouping: char,
    decimal: char,
    zero: char,
    /// `DecimalFormatSymbols.getMinusSign()`. Read by [`fmt_iso_year`] and by
    /// nothing else — see the type doc.
    minus: char,
}

impl Default for FmtSymbols {
    fn default() -> Self {
        FmtSymbols {
            grouping: ',',
            decimal: '.',
            zero: '0',
            // `getMinusSign(Locale)` is `locale == null ? '-' : …`, the same
            // shape as the other three. Measured: `String.format((Locale) null,
            // "%tF", LocalDate.of(-44,3,15))` is `-0044-03-15` with an ASCII
            // U+002D even on a host whose default locale's minus is U+2212.
            minus: '-',
        }
    }
}

/// Which `Locale` a `java.util.Formatter` call localizes against.
///
/// This exists because the `Option<Locale>` it replaced could not tell two
/// different requests apart, and `java.util.Formatter` answers them
/// differently:
///
/// * **The overload has no `Locale` parameter** — `String.format(String,
///   Object...)`, `String.formatted`, `PrintStream.printf(String, Object...)`,
///   `new Formatter()`. Every one of these is specified as formatting with
///   "the locale returned by `Locale.getDefault(Locale.Category.FORMAT)`".
/// * **The overload was given one, and it is `null`** —
///   `String.format((Locale) null, …)`. "If `l` is `null` then no localization
///   is applied", i.e. the root separators and ASCII digits.
///
/// Collapsing the two onto one `None` is the defect this type removes (W7-91
/// §5, and the last open `format` row of
/// `docs/known-issues/jdk-only/W7-34-formatter-family-residuals.md`): the
/// no-`Locale` overload took the explicit-null branch and localized against
/// `Locale.ROOT` on every host. Invisible on a ROOT/en-US host, and wrong
/// everywhere else — separators, grouping and digits all diverge.
///
/// The distinction is resolved AT CONSUMPTION, in [`fmt_symbols_for`] and
/// [`fmt_date_name`], never by re-encoding one variant as the other. Those two
/// are one JDK rule implemented twice and they had already drifted: the date
/// half was reading `DateFormatSymbols.getInstance()` (the FORMAT default) for
/// the absent-locale case while the number half took the root constants.
#[derive(Clone, Copy)]
enum FmtLocale {
    /// The overload has no `Locale` parameter, so the JDK supplies
    /// `Locale.getDefault(Locale.Category.FORMAT)`.
    DefaultFormat,
    /// The overload carries an explicit `Locale` argument, possibly `null`.
    Given(Option<cratonvm_types::ObjectRef>),
}

thread_local! {
    /// Re-entrancy latch for [`fmt_symbols_for`].
    ///
    /// Resolving symbols runs real JDK bytecode (resource bundles, locale
    /// providers) which is free to call `String.format(Locale, …)` itself. It
    /// would then re-enter this native and ask for symbols again, on a locale
    /// whose symbols are still mid-construction. While the latch is set the
    /// inner call takes the root defaults, which is the same answer the JDK's
    /// own null-locale branch gives and cannot recurse.
    static FMT_SYMBOLS_RESOLVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read a `java.util.Locale`'s formatting symbols, or the root defaults for an
/// explicit null locale.
///
/// Goes through `DecimalFormatSymbols.getInstance(Locale)` rather than a
/// hard-coded table because that is what `java.util.Formatter` does, and
/// because the separators are not guessable: France's grouping separator is
/// U+202F NARROW NO-BREAK SPACE, not U+0020 — measured on HotSpot 25, and
/// CratonVM's own `DecimalFormatSymbols` was measured to answer the identical
/// three characters for ROOT/US/GERMANY/FRANCE, so this reads a real value
/// rather than reproducing one.
///
/// The three [`FmtLocale`] arms, and why each is what the JDK does:
///
/// * `Given(None)` — an explicit `null` `Locale`. The JDK's own
///   `Formatter.zero(Locale)` is `if ((l != null) && !l.equals(Locale.US)) {…}
///   return '0';`, so a null locale takes the three constants and runs no
///   bytecode at all. Unchanged, and it is also the arm every `new Formatter()`
///   currently arrives on (see the residual note below).
/// * `Given(Some(l))` — resolve `l`. Unchanged, deliberately: the
///   very common `String.format(Locale.ROOT, …)` must not become slower or
///   different because the no-`Locale` overload was fixed. The only fast path
///   ROOT ever had is the LAZINESS in [`format_impl`] — this function is not
///   reached at all unless a conversion actually localizes — and that is
///   untouched.
/// * `DefaultFormat` — the overload had no `Locale`. Resolves
///   through the NO-ARG `DecimalFormatSymbols.getInstance()`, whose body is
///   `getInstance(Locale.getDefault(Locale.Category.FORMAT))`. Asking the JDK
///   for the default rather than composing one here is what keeps this from
///   becoming a fourth opinion about what the default locale is — the same
///   argument [`fmt_date_name`] already makes, and the reason these two agree
///   again.
///
/// W7-34 left this arm undone and named the hazard: "resolving a default locale
/// on the no-locale path is the one place where the re-entrancy hazard is
/// worst — every internal `String.format` in the VM, including the logging
/// shims, goes through it." Two things bound it, and neither is new code.
/// First, LAZINESS: `format_impl` only calls this at a `%d`/`%f`/`%e`/`%g`
/// conversion, so an internal `String.format("{}: {}"-shaped %s format)` still
/// runs zero locale bytecode. Second, `FMT_SYMBOLS_RESOLVING`: an inner
/// `String.format` raised while the outer one is resolving answers the root
/// constants and cannot recurse. `fmt_date_name` has been taking exactly this
/// route on exactly this path since the `%t` name fields landed.
fn fmt_symbols_for(ctx: &mut dyn NativeContext, locale: FmtLocale) -> FmtSymbols {
    if let FmtLocale::Given(None) = locale {
        return FmtSymbols::default();
    }
    if FMT_SYMBOLS_RESOLVING.with(std::cell::Cell::get) {
        return FmtSymbols::default();
    }
    FMT_SYMBOLS_RESOLVING.with(|f| f.set(true));
    let resolved = (|| {
        let instance = match locale {
            FmtLocale::Given(Some(l)) => ctx.invoke(
                "java/text/DecimalFormatSymbols",
                "getInstance",
                "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;",
                &[Value::Object(Some(l))],
            ),
            // `DefaultFormat`. `Given(None)` returned above, so this arm is
            // only ever the no-`Locale` overload.
            _ => ctx.invoke(
                "java/text/DecimalFormatSymbols",
                "getInstance",
                "()Ljava/text/DecimalFormatSymbols;",
                &[],
            ),
        };
        let dfs = match instance {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let read = |ctx: &mut dyn NativeContext, name: &str, fallback: char| -> char {
            match ctx.invoke_virtual(dfs, name, "()C", &[]) {
                Ok(Some(Value::Int(c))) => char::from_u32(c as u32).unwrap_or(fallback),
                _ => fallback,
            }
        };
        Some(FmtSymbols {
            grouping: read(ctx, "getGroupingSeparator", ','),
            decimal: read(ctx, "getDecimalSeparator", '.'),
            zero: read(ctx, "getZeroDigit", '0'),
            // The fourth read. It is the same `()C` shape as the other three
            // and it is resolved eagerly with them rather than lazily at
            // [`fmt_iso_year`], because its only consumer sits inside
            // `format_temporal_field`, which has no `ctx` re-entrancy budget of
            // its own and is already handed the resolved `FmtSymbols`. 59 of
            // this JDK's 1158 available locales answer U+2212 here.
            minus: read(ctx, "getMinusSign", '-'),
        })
    })();
    FMT_SYMBOLS_RESOLVING.with(|f| f.set(false));
    resolved.unwrap_or_default()
}

thread_local! {
    /// Re-entrancy latch for [`fmt_date_name`] — the `DateFormatSymbols` twin
    /// of [`FMT_SYMBOLS_RESOLVING`], and it exists for the same reason.
    ///
    /// `DateFormatSymbols.getInstance` runs real JDK bytecode: the locale
    /// provider chain, `ResourceBundle`, and (since W7-80) the CLDR bundle
    /// classes out of the JDK image. Any of that is free to call
    /// `String.format` itself — a `tracing`-style diagnostic or an exception
    /// message is enough — which re-enters this native and asks for the same
    /// locale's symbols while they are still mid-construction. While the latch
    /// is set the inner call answers `None` and the caller falls back to the
    /// English table, which is what the JDK's own null-locale branch prints
    /// and cannot recurse.
    static FMT_DATE_NAMES_RESOLVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One element of one `java.text.DateFormatSymbols` name array, for the
/// `%t`/`%T` conversions that render a NAME rather than a number.
///
/// `getter` is the no-arg `()[Ljava/lang/String;` accessor
/// `java.util.Formatter` itself calls — `getMonths`, `getShortMonths`,
/// `getWeekdays`, `getShortWeekdays` or `getAmPmStrings` — and `index` is the
/// index into it, in that array's own convention (months 0-based over 13
/// entries, weekdays 1-based over 8 with slot 0 unused).
///
/// `locale` is the call's [`FmtLocale`]. `Given(Some(l))` resolves
/// `DateFormatSymbols.getInstance(l)`; every other arm resolves through the
/// NO-ARG `DateFormatSymbols.getInstance()`, whose body is
/// `getInstance(Locale.getDefault(Locale.Category.FORMAT))` — i.e. exactly the
/// `Locale` a real `java.util.Formatter` built by `String.format(String,
/// Object...)` would be carrying. Asking the JDK for it rather than composing
/// one here is what keeps this from becoming a fourth opinion about what the
/// default locale is.
///
/// # `Given(None)` is `Locale.US`, which the English tables below already are
///
/// `Formatter.printDateTime` opens every name field with `Locale lt =
/// Objects.requireNonNullElse(l, Locale.US)`, so a literal
/// `String.format((Locale) null, "%tB", d)` renders ENGLISH. This arm used to
/// resolve the default FORMAT locale instead. MEASURED on HotSpot 25 with the
/// host default forced to a non-US locale — the case F22 could not reach and
/// recorded rather than guess at:
///
/// | expression | `-Duser.language=tr -Duser.country=TR` | `ru_RU` |
/// |---|---|---|
/// | `format((Locale) null, "%tb", d)` | `Nov` | `Nov` |
/// | `format("%tb", d)` (default) | `Kas` | `нояб.` |
/// | `format((Locale) null, "%tA", d)` | `Wednesday` | `Wednesday` |
/// | `format((Locale) null, "%tp", d)` | `am` | `am` |
/// | `format("%tp", d)` (default) | `öö` | `am` |
///
/// So `Given(None)` returns `None` HERE — before the latch, before any
/// bytecode — and the caller prints its English table. Those four tables ARE
/// `Locale.US`'s answers: `DateFormatSymbols.getInstance(Locale.US)` gives
/// `Jan..Dec`, `January..December`, `Sun..Sat`, `Sunday..Saturday` and
/// `{"AM","PM"}`, which is also why `printDateTime`'s own `AM_PM` arm skips
/// `DateFormatSymbols` entirely for a null (and for `Locale.US`) and uses the
/// literal `String[] ampm = { "AM", "PM" }`. Asking the JDK for US data would
/// be a slower route to the same characters, with a re-entrancy hazard the
/// early return does not have.
///
/// **This shape matches [`fmt_symbols_for`], deliberately.** That helper has
/// answered `Given(None)` with the root constants since it was written — the
/// JDK's `getGroupingSeparator(Locale)` is literally `locale == null ? ',' :
/// …` — and the two now give an explicit null locale ONE meaning across the
/// whole file. Measured: `format((Locale) null, "%,d", 1234567)` is
/// `1,234,567` on a `tr_TR` host where the default gives `1.234.567`.
/// [`fmt_upper_case`] is the deliberate exception and stays as it is:
/// `toUpperCaseWithLocale` is `s.toUpperCase(Objects.requireNonNullElse(l,
/// Locale.getDefault(FORMAT)))`, the DEFAULT — measured, `format((Locale)
/// null, "%S", "i")` is U+0130 on a `tr_TR` host, not `I`. Three sites, two
/// JDK rules, and the JDK does mean both.
///
/// # What this costs until the `Formatter` constructors are fixed
///
/// In this VM `Given(None)` is not only an explicit null: `new Formatter()`
/// and `new Formatter(Appendable)` reach `format` through natives in
/// `native-builtins/src/lib.rs` (and `register_formatter_natives` in the same
/// file) that write `null` into the receiver's locale slot, where the real
/// `java.util.Formatter()` constructor writes
/// `Locale.getDefault(Locale.Category.FORMAT)`; `format` then reads slot 1 and
/// arrives here as `Given(None)`. For THOSE the right answer is the default
/// locale, and this arm now gets them wrong. That is not a new trade — it is
/// the trade `fmt_symbols_for` has always made on the same path, so the change
/// makes the file self-consistent and moves the whole residual into ONE fix,
/// in the constructor rather than in three consumers. See W7-34's residuals,
/// which already carry that constructor row as open, and F28-1's NOMINATION.
///
/// `None` on any failure, and the caller then prints the English name it
/// printed before. Every failure mode is a legitimate one: synthetic-JDK mode
/// has no `java.text.DateFormatSymbols.getInstance`, a jlinked image can be
/// missing `jdk.localedata`, and an entry can be the empty string (both arrays
/// carry one). The `Err` from `invoke` is dropped on purpose — the caller is
/// owed a formatted string, not this lookup's failure, and CratonVM carries a
/// thrown exception in the return value rather than in thread state, so
/// nothing is left pending to clear (the same argument `fmt_raise` makes for
/// its speculative `load_class`).
fn fmt_date_name(
    ctx: &mut dyn NativeContext,
    locale: FmtLocale,
    getter: &str,
    index: usize,
) -> Option<String> {
    // An explicit null `Locale` is `Locale.US`, and the caller's English table
    // IS `DateFormatSymbols.getInstance(Locale.US)` for all five getters — see
    // the measured rows above. Returning before the latch keeps this the one
    // arm that runs no bytecode at all, the same shape `fmt_symbols_for` uses
    // for the same request.
    if let FmtLocale::Given(None) = locale {
        return None;
    }
    if FMT_DATE_NAMES_RESOLVING.with(std::cell::Cell::get) {
        return None;
    }
    FMT_DATE_NAMES_RESOLVING.with(|f| f.set(true));
    let resolved = (|| {
        let instance = match locale {
            FmtLocale::Given(Some(l)) => ctx.invoke(
                "java/text/DateFormatSymbols",
                "getInstance",
                "(Ljava/util/Locale;)Ljava/text/DateFormatSymbols;",
                &[Value::Object(Some(l))],
            ),
            // `Given(None)` returned above, so this arm is only ever the
            // no-`Locale` overload.
            _ => ctx.invoke(
                "java/text/DateFormatSymbols",
                "getInstance",
                "()Ljava/text/DateFormatSymbols;",
                &[],
            ),
        };
        let dfs = match instance {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let arr = match ctx.invoke_virtual(dfs, getter, "()[Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => return None,
        };
        if index >= ctx.array_length(arr) {
            return None;
        }
        match ctx.get_array_element(arr, index) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
    })();
    FMT_DATE_NAMES_RESOLVING.with(|f| f.set(false));
    resolved.filter(|s| !s.is_empty())
}

/// Rewrite an ASCII numeric body into the locale's digits and separators.
///
/// Runs LAST, after grouping, signs and width padding, so everything upstream
/// keeps working in ASCII where a byte length and a character count agree —
/// U+202F is three UTF-8 bytes, and a width pad computed over it would be
/// short by two. The substitution is one character for one, so the padded
/// character count survives it unchanged.
///
/// Applied only to the conversions the JDK localizes (`%d %f %e %g`): `%x`,
/// `%o` and `%a` are documented as "No localization is applied", and a `%s`
/// argument's own digits are the caller's text, not a magnitude.
/// [`fmt_localize`]'s DIGIT half, for the `%t` family.
///
/// `localizedMagnitude` does two things — substitute the zero digit, and
/// substitute the separators — and the `%t` conversions only ever ask for the
/// first: a date/time field carries no grouping separator (its
/// `localizedMagnitude` calls pass `Flags.NONE` or `Flags.ZERO_PAD`, never
/// `Flags.GROUP`) and no decimal point. Mapping `','` and `'.'` here as well
/// would be wrong rather than merely unnecessary — `%tD` is `mm/dd/yy` and a
/// `DateFormatSymbols` month name may legitimately END in `'.'` (`ru`'s
/// `нояб.`).
///
/// So this is deliberately NOT a call to [`fmt_localize`] with the separators
/// zeroed out: the two functions answer different questions and share the one
/// mechanism that is genuinely common, the `zero - '0'` shift. Both are
/// one-character-for-one, which is what lets the width justifier keep counting
/// after them.
///
/// The `sym.zero == '0'` short circuit is the whole cost on every Latin-digit
/// locale, which is nearly all of them.
fn fmt_localize_digits(s: &str, sym: FmtSymbols) -> String {
    if sym.zero == '0' {
        return s.to_string();
    }
    let shift = sym.zero as u32 - '0' as u32;
    s.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                char::from_u32(c as u32 + shift).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn fmt_localize(s: &str, sym: FmtSymbols) -> String {
    if sym.grouping == ',' && sym.decimal == '.' && sym.zero == '0' {
        return s.to_string();
    }
    let shift = sym.zero as u32 - '0' as u32;
    s.chars()
        .map(|c| match c {
            ',' => sym.grouping,
            '.' => sym.decimal,
            '0'..='9' => char::from_u32(c as u32 + shift).unwrap_or(c),
            other => other,
        })
        .collect()
}

// --- String.format (basic %s/%d/%f support) ---

/// `String.format(String, Object...)` — the overload with NO `Locale`.
///
/// [`FmtLocale::DefaultFormat`], not a null locale: `java.util.Formatter`
/// formats this overload against `Locale.getDefault(Locale.Category.FORMAT)`.
/// It used to pass `None` here, which [`fmt_symbols_for`] read as "no
/// localization" — so `String.format("%,.2f", 1234.5)` answered the ROOT
/// `1,234.50` on a German host where HotSpot answers `1.234,50`. Every
/// no-`Locale` surface funnels through here ([`native_string_formatted`], and
/// `PrintStream.printf`/`format` and `PrintWriter.printf`/`format` in
/// `native-builtins/src/lib.rs` and `logging_shims.rs`), so they all move
/// together and cannot drift.
pub(crate) fn native_string_format(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    format_impl(ctx, args, FmtLocale::DefaultFormat)
}

/// The body of every `String.format` / `Formatter.format` overload.
///
/// `locale` says which `java.util.Locale` this call localizes against — see
/// [`FmtLocale`] for why the absent-parameter and explicit-null cases are not
/// the same request. It is resolved to [`FmtSymbols`] LAZILY, at the first
/// conversion that actually localizes, so the very common
/// `String.format(Locale.ROOT, "%s", x)` pays nothing for a locale it never
/// consults — and, since the fix above, neither does the no-`Locale` overload,
/// which is the same laziness doing the same job on a hotter path.
/// `CRATONVM_NO_FORMAT_ARG_PIN=1` restores the unpinned formatter, so the two
/// arms are one binary apart.
fn format_arg_pin_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_FORMAT_ARG_PIN").is_none()
    })
}

fn format_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    locale: FmtLocale,
) -> MethodCallResult {
    // GC. `format_impl_pinned` RUNS BYTECODE: every numeric conversion resolves
    // `FmtSymbols` through `DecimalFormatSymbols.getInstance()`, which allocates
    // and can trigger a young collection, and `format_arg_full` dispatches
    // `hashCode`/`toString` on the argument. The format string and the varargs
    // array arrive as raw `ObjectRef`s in `args` and were held across all of it
    // with nothing rooting them, so a collection mid-format reclaimed the array
    // together with every box still inside it. The next conversion then read an
    // all-zero header at the argument's address -- `ClassId(0)`, which the class
    // manager names `java.lang.Object` -- and the formatter refused it as
    // `IllegalFormatConversionException: d != java.lang.Object`. See
    // `bug-generational-ntru-unpinned-jit-reference-20260821.md`: the signature
    // reads like a lost JIT root and is neither JIT- nor relocation-related.
    //
    // Pin both for the duration, and re-derive the array from its handle before
    // every element read so a moving collection's new address is used. Same
    // idiom as `fmt_format_to` further up this file; `pin_base` is the FIRST
    // handle, so one `unpin_native_roots(pin_base)` releases the batch on every
    // return path -- which is why the body is a separate function rather than
    // an early-returning block.
    if format_arg_pin_enabled() {
        let fmt_obj = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("String.format: format is null".to_string()),
                }
                .into())
            }
        };
        let arr_obj = match args.get(1) {
            Some(Value::Object(Some(obj))) => Some(*obj),
            _ => None,
        };
        let pin_base = ctx.pin_native_root(fmt_obj);
        let arr_pin = arr_obj.map(|a| ctx.pin_native_root(a));
        let out = format_impl_pinned(ctx, args, locale, arr_pin);
        ctx.unpin_native_roots(pin_base);
        return out;
    }
    format_impl_pinned(ctx, args, locale, None)
}

fn format_impl_pinned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    locale: FmtLocale,
    arr_pin: Option<usize>,
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
    // The format string's own code UNITS. `read_string` was lossy here too, and
    // silently: a format string is ordinary caller text and the literal runs
    // between the specifiers are copied to the output verbatim, so a lone
    // surrogate in one of them became U+FFFD before the parser ever ran.
    let fmt_units = read_string_chars(ctx, fmt_obj);

    // Get the varargs array
    let arr_ref = match args.get(1) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };

    let arr_len = arr_ref.map_or(0, |a| ctx.array_length(a));
    let mut symbols: Option<FmtSymbols> = None;

    // Format string parser supporting flags, width, precision:
    // %[flags][width][.precision]conversion
    // The output accumulates as UTF-16 code UNITS and becomes a `String` once,
    // at the end, through [`sb_string_from_units`]. Every producer that feeds it
    // — `format_arg_full`, `format_temporal_field`, the literal runs below —
    // can legitimately emit an unpaired surrogate, and a Rust `String` here
    // would have destroyed it however carefully they preserved it.
    let mut result: Vec<u16> = Vec::new();
    // `chars` is index-aligned with `fmt_units`: ONE `char` per code UNIT, not
    // per code point. That is a change from a `str::chars()` collect, and it is
    // what lets the literal-text branch at the bottom of the loop push
    // `fmt_units[i]` — the raw unit — while the parser keeps reading `chars[i]`.
    // The parser is unaffected: every character it tests for is ASCII (`'%'`,
    // `'$'`, the flag set, `is_ascii_digit`, `is_ascii_alphabetic`), and a
    // surrogate — half of a pair or lone — is none of those, so it falls to the
    // literal branch exactly as a non-ASCII `char` always did. A surrogate PAIR
    // is now two `chars` instead of one and is pushed as its two original units,
    // which reassemble into the same code point.
    let chars: Vec<char> = fmt_units
        .iter()
        .map(|&u| char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'))
        .collect();
    let mut i = 0;
    let mut arg_idx = 0;
    // Index used by the most recent conversion, for the `%<` relative-index
    // flag ("reuse the previous argument").
    let mut last_used_index: Option<usize> = None;

    while i < chars.len() {
        if chars[i] == '%' {
            // A '%' that is the final character of the format string is a
            // truncated conversion. The JDK's specifier regex simply fails to
            // match and it reports the character that FOLLOWS the '%' — or the
            // '%' itself when there is none, which is why `String.format("abc%")`
            // says "Conversion = '%'" rather than naming the whole tail.
            if i + 1 >= chars.len() {
                return Err(fmt_raise(ctx, &FmtFault::UnknownConversion("%".to_string())));
            }
            i += 1;
            // Held for the truncated-specifier report below, which names this
            // character however much of the specifier was consumed afterwards
            // (`%5` reports '5', `%1$` reports '1').
            let first_after_pct = chars[i];
            // Check for %% and %n first
            if chars[i] == '%' {
                result.push(u16::from(b'%'));
                i += 1;
                continue;
            }
            if chars[i] == 'n' {
                // Formatter's %n conversion emits the platform line separator
                // (System.lineSeparator(), "\r\n" on Windows), not a literal
                // '\n' -- see native_system_line_separator in lang_system.rs
                // for the same platform check.
                result.extend(fmt_line_separator_units());
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
            //
            // The index is SCANNED here and VALIDATED later, at
            // `deferred_fault` below. `java.util.Formatter` splits the same two
            // jobs across `FormatSpecifierParser.parse` (which only measures
            // the pieces, and returns 0 for a specifier it cannot complete) and
            // the `FormatSpecifier` constructor (which is the only thing that
            // throws). So `%0$s` is an illegal INDEX while a bare `%0$` — with
            // no conversion character to complete it — is an unknown
            // CONVERSION, and the difference is which phase gets there first.
            let mut explicit_index: Option<usize> = None;
            let mut deferred_fault: Option<FmtFault> = None;
            {
                let mut j = i;
                while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                    j += 1;
                }
                if j > i && chars.get(j) == Some(&'$') {
                    // "If the argument index does not correspond to an
                    // available argument ... " is a different fault; this is
                    // `FormatSpecifier.index`, which refuses a non-POSITIVE
                    // index outright and reports `Integer.MIN_VALUE` for digits
                    // that overflow an `int` (its `NumberFormatException` arm).
                    match chars[i..j].iter().collect::<String>().parse::<i32>() {
                        Ok(n) if n >= 1 => explicit_index = Some(n as usize - 1),
                        Ok(n) => deferred_fault = Some(FmtFault::ArgumentIndex(n)),
                        Err(_) => deferred_fault = Some(FmtFault::ArgumentIndex(i32::MIN)),
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
            //
            // `Flags.parse` refuses a repeated flag ("If a flag is given more
            // than once ... a DuplicateFormatFlagsException will be thrown").
            // The refusal is DEFERRED for the same reason the index one is:
            // `%--d` is a duplicate flag, but `%--` is an unknown conversion,
            // because the JDK's scanner never reaches `Flags.parse` for a
            // specifier it could not complete.
            let mut flags = String::new();
            while chars.get(i).is_some_and(|c| "-+0 #(,<".contains(*c)) {
                let flag = chars[i];
                if flags.contains(flag) {
                    if deferred_fault.is_none() {
                        deferred_fault = Some(FmtFault::DuplicateFlags(flag.to_string()));
                    }
                } else {
                    flags.push(flag);
                }
                i += 1;
            }

            // Parse optional width. `FormatSpecifier.width` runs the digits
            // through `Integer.parseInt` and answers a run that overflows with
            // `IllegalFormatWidthException(Integer.MIN_VALUE)` — so a width
            // that does not fit an `int` is a REFUSAL, not a very wide field.
            // (Parsing it into a `usize` and padding to it is how
            // `String.format("%2147483648d", 1)` became an allocation of two
            // billion spaces on a 64-bit host.)
            let mut width: Option<usize> = None;
            let width_start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            if i > width_start {
                match chars[width_start..i]
                    .iter()
                    .collect::<String>()
                    .parse::<i32>()
                {
                    Ok(w) => width = Some(w as usize),
                    Err(_) => {
                        if deferred_fault.is_none() {
                            deferred_fault = Some(FmtFault::IllegalWidth(i32::MIN));
                        }
                    }
                }
            }

            // Parse optional .precision. A '.' with NO digits after it is not a
            // zero precision — `FormatSpecifierParser.parsePrecision` returns
            // -1 for it and `parse()` then returns 0, which the outer loop
            // reports as `UnknownFormatConversionException` naming the
            // character after the '%'. `String.format("%.d", 1)` is
            // "Conversion = '.'" on HotSpot, not a precision of 0.
            let mut precision: Option<usize> = None;
            let mut malformed_precision = false;
            if chars.get(i) == Some(&'.') {
                i += 1;
                let prec_start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                if i > prec_start {
                    match chars[prec_start..i]
                        .iter()
                        .collect::<String>()
                        .parse::<i32>()
                    {
                        Ok(p) => precision = Some(p as usize),
                        Err(_) => {
                            if deferred_fault.is_none() {
                                deferred_fault = Some(FmtFault::IllegalPrecision(i32::MIN));
                            }
                        }
                    }
                } else {
                    malformed_precision = true;
                }
            }

            // Parse conversion character
            if malformed_precision {
                return Err(fmt_raise(
                    ctx,
                    &FmtFault::UnknownConversion(first_after_pct.to_string()),
                ));
            }
            if let Some(&spec) = chars.get(i) {
                i += 1;
                // `FormatSpecifier`'s constructor validates in source order —
                // index, flags, width, precision — and every one of those comes
                // before `conversion()` and `check()`. So a specifier with two
                // faults reports the LEFTMOST, and this is where the ones the
                // scan deferred are raised: after the scan proved there IS a
                // conversion character, before any conversion-specific check.
                if let Some(fault) = deferred_fault {
                    return Err(fmt_raise(ctx, &fault));
                }
                match spec {
                    's' | 'S' | 'd' | 'f' | 'x' | 'X' | 'c' | 'C' | 'b' | 'B' | 'e' | 'E' | 'g'
                    | 'G' | 'o' | 'h' | 'H' | 'a' | 'A' => {
                        // Select the argument this conversion consumes:
                        //   `%<x`  → reuse the previous conversion's index
                        //   `%N$x` → explicit 1-based index N
                        //   `%x`   → next ordinary index (advances the counter)
                        // Only ordinary conversions advance `arg_idx`, matching
                        // java.util.Formatter (explicit/relative specs do not).
                        //
                        // A `'<'` with NO previous conversion is a REFUSAL, not
                        // a fallback to argument 0. `Formatter.format`'s loop
                        // is `case -1 -> { if (last < 0 || …) throw new
                        // MissingFormatArgumentException(fs.toString()); … }`
                        // and `last` starts at -1. Measured on HotSpot
                        // 25.0.3+9: `String.format("%<s", "a")` is
                        // `MissingFormatArgumentException: Format specifier
                        // '%<s'`, and so is `String.format("%<tY", 0L)`. This
                        // answered args[0] instead, silently.
                        let relative_without_previous =
                            flags.contains('<') && last_used_index.is_none();
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
                        // Flag/width/precision legality is decided BEFORE the
                        // argument is fetched, as it is in the JDK — the
                        // `FormatSpecifier` constructor runs every `check*`
                        // before `format` ever sees a value. So `%.2d` refuses
                        // even when no argument was supplied.
                        if let Err(fault) =
                            fmt_check_spec(explicit_index, &flags, width, precision, spec)
                        {
                            return Err(fmt_raise(ctx, &fault));
                        }
                        // ...and the missing PREVIOUS argument is raised here,
                        // with the same standing as a missing NEXT one: both
                        // are `format`'s own range test, which runs after every
                        // `FormatSpecifier` constructor has passed its checks.
                        // `%<0d` is therefore the flag mismatch, not this.
                        if relative_without_previous {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::MissingArgument(fmt_spec_text(
                                    explicit_index,
                                    &flags,
                                    width,
                                    precision,
                                    spec,
                                )),
                            ));
                        }
                        // "If there are fewer arguments than format specifiers,
                        // the argument index is out of range ... a
                        // MissingFormatArgumentException is thrown." Answering
                        // the empty string instead let a caller's own
                        // arity-guard `catch` never fire, and silently shifted
                        // every later conversion's argument.
                        //
                        // A NULL varargs array is not the same thing: the JDK's
                        // `format` loop guards the range check with `args !=
                        // null` and then passes `null` for every conversion, so
                        // `String.format("%s", (Object[]) null)` is "null" and
                        // not a refusal.
                        let arg = match arr_ref {
                            None => Value::Object(None),
                            Some(a) if use_idx < arr_len => ctx.get_array_element(a, use_idx),
                            Some(_) => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::MissingArgument(fmt_spec_text(
                                        explicit_index,
                                        &flags,
                                        width,
                                        precision,
                                        spec,
                                    )),
                                ))
                            }
                        };
                        // Only a conversion that localizes needs the locale, so
                        // this is where the `DecimalFormatSymbols` lookup is
                        // paid for — never on a format string of plain `%s`.
                        let sym = if matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') {
                            match symbols {
                                Some(s) => s,
                                None => {
                                    let s = fmt_symbols_for(ctx, locale);
                                    symbols = Some(s);
                                    s
                                }
                            }
                        } else {
                            FmtSymbols::default()
                        };
                        // `fmt_symbols_for` above may have run
                        // `DecimalFormatSymbols.getInstance()` -- real bytecode,
                        // real allocation, so a collection can have happened
                        // since `arg` was read. The array is pinned, so the
                        // element is still LIVE; re-read it through the handle
                        // so a moving collection's new address is used rather
                        // than the copy taken before the call.
                        let arg = match (arr_ref, arr_pin) {
                            (Some(a), Some(h)) if use_idx < arr_len => {
                                let a = ctx.read_native_pin(h, a);
                                ctx.get_array_element(a, use_idx)
                            }
                            _ => arg,
                        };
                        let text = format_arg_full(
                            ctx, &arg, spec, &flags, width, precision, sym, locale,
                        )?;
                        result.extend_from_slice(&text);
                    }
                    't' | 'T' => {
                        // Date/time conversion: 't'/'T' is a *prefix*, not a
                        // complete conversion — the next character selects the
                        // actual field (e.g. `%tb` = abbreviated month, `%tY` =
                        // 4-digit year). Real java.util.Formatter upper-cases
                        // the whole result when the prefix itself is 'T'.
                        let uppercase = spec == 'T';
                        // The prefix only IS a prefix when a conversion
                        // character follows: `FormatSpecifierParser.parse`
                        // requires `isConversion(c1)` before it consumes two
                        // characters, and otherwise falls back to reading the
                        // 't' itself as the conversion — which
                        // `Conversion.isValid` rejects. So `%t1` reports
                        // "Conversion = 't'", NOT an unknown date/time field,
                        // and neither does it consume the '1'.
                        let field = match chars.get(i) {
                            Some(&c) if c.is_ascii_alphabetic() || c == '%' => {
                                i += 1;
                                c
                            }
                            // A 't'/'T' with no usable field character after it
                            // never matches the JDK's specifier regex either, so
                            // it is the same truncated-specifier refusal: HotSpot
                            // 25 answers `UnknownFormatConversionException:
                            // Conversion = 't'` for `String.format("%t")`.
                            _ => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::UnknownConversion(spec.to_string()),
                                ))
                            }
                        };
                        // `FormatSpecifier.toString` for a date/time specifier
                        // re-emits the 't'/'T' prefix and upper-cases the field
                        // when the prefix was 'T' — `%-Ty` reports itself as
                        // "%-TY". It is what the `MissingFormatWidth` and
                        // `MissingFormatArgument` messages quote.
                        let dt_spec_text = || {
                            let mut s = String::from("%");
                            s.push_str(&fmt_flags_string(&flags));
                            if let Some(idx) = explicit_index {
                                s.push_str(&format!("{}$", idx + 1));
                            }
                            if let Some(w) = width {
                                s.push_str(&w.to_string());
                            }
                            if let Some(p) = precision {
                                s.push('.');
                                s.push_str(&p.to_string());
                            }
                            s.push(if uppercase { 'T' } else { 't' });
                            s.push(if uppercase {
                                field.to_ascii_uppercase()
                            } else {
                                field
                            });
                            s
                        };
                        // `checkDateTime`, in the JDK's order. None of it ran
                        // before: a `%t` specifier skipped every legality check
                        // the other conversions go through, so `%.2tY` formatted
                        // instead of refusing and `%,tY` was accepted outright.
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if !FMT_DATETIME_FIELDS.contains(field) {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::UnknownConversion(format!("t{field}")),
                            ));
                        }
                        {
                            // checkBadFlags(ALTERNATE | PLUS | LEADING_SPACE |
                            // ZERO_PAD | GROUP | PARENTHESES): the message names
                            // the offending SUBSET and the field character.
                            let offending: String = fmt_flags_string(&flags)
                                .chars()
                                .filter(|c| "#+ 0,(".contains(*c))
                                .collect();
                            if !offending.is_empty() {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::FlagsMismatch(offending, field),
                                ));
                            }
                        }
                        if width.is_none() && flags.contains('-') {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::MissingWidth(dt_spec_text()),
                            ));
                        }
                        // `'<'` with no previous conversion — the same refusal
                        // the general arm makes, quoting this arm's own
                        // specifier text. Measured on HotSpot 25.0.3+9:
                        // `String.format("%<tY", 0L)` is
                        // `MissingFormatArgumentException: Format specifier
                        // '%<tY'`.
                        if flags.contains('<') && last_used_index.is_none() {
                            return Err(fmt_raise(ctx, &FmtFault::MissingArgument(dt_spec_text())));
                        }
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
                        // Same argument-availability rule as the general
                        // conversions above, which this arm did not have: an
                        // absent argument APPENDED NOTHING and the format string
                        // came back silently short, where HotSpot raises
                        // `MissingFormatArgumentException`.
                        let elem = match arr_ref {
                            None => Value::Object(None),
                            Some(a) if use_idx < arr_len => ctx.get_array_element(a, use_idx),
                            Some(_) => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::MissingArgument(dt_spec_text()),
                                ))
                            }
                        };
                        // The `Locale` goes through so the name fields can ask
                        // `DateFormatSymbols` for it.
                        //
                        // `FmtSymbols` goes through too, and shares the SAME
                        // per-call cache the numeric conversions use, because
                        // every number a `%t` field renders is a
                        // `localizedMagnitude` and takes the locale's zero
                        // digit (measured under `ar-EG`; see
                        // `fmt_localize_digits`). The seven pure-NAME fields
                        // still resolve nothing — a `%tb`-only format string
                        // runs no `DecimalFormatSymbols` bytecode — which is
                        // the same laziness the `'d'`/`'f'` arm above applies,
                        // one screen up, over the same cache.
                        //
                        // `uppercase` goes through too rather than being
                        // applied to the returned text. `printDateTime` is
                        // `appendJustified(a, toUpperCaseWithLocale(sb, l))` —
                        // the upper-caser runs INSIDE the width, and it is the
                        // locale-sensitive one (`%TA` of a Monday under `tr` is
                        // `PAZARTES` + U+0130 on HotSpot 25, measured). Doing it
                        // out here meant a mapping that changes length padded
                        // to the wrong field, and did it with Rust's
                        // locale-independent table.
                        let sym = if matches!(field, 'B' | 'b' | 'h' | 'A' | 'a' | 'p' | 'Z') {
                            FmtSymbols::default()
                        } else {
                            match symbols {
                                Some(s) => s,
                                None => {
                                    let s = fmt_symbols_for(ctx, locale);
                                    symbols = Some(s);
                                    s
                                }
                            }
                        };
                        // Same re-derivation as the general-conversion arm: the
                        // symbols lookup above can run `DecimalFormatSymbols`
                        // bytecode, so `elem` may name a pre-collection address.
                        // The array is pinned, so the element is still live.
                        let elem = match (arr_ref, arr_pin) {
                            (Some(a), Some(h)) if use_idx < arr_len => {
                                let a = ctx.read_native_pin(h, a);
                                ctx.get_array_element(a, use_idx)
                            }
                            _ => elem,
                        };
                        let text = format_temporal_field(
                            ctx, &elem, field, &flags, width, sym, locale, uppercase,
                        )?;
                        result.extend_from_slice(&text);
                    }
                    // The literal-percent conversion, reached only when it
                    // carried flags or a width — a bare `%%` is short-circuited
                    // above. `checkText` admits `'-'` and nothing else ("The
                    // flags ... are the same as for the general conversions,
                    // except that only the '-' flag is allowed"), and `'-'`
                    // still needs a width.
                    '%' => {
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if !flags.is_empty() && flags != "-" {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::IllegalFlags(fmt_flags_string(&flags)),
                            ));
                        }
                        if flags == "-" && width.is_none() {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::MissingWidth(fmt_spec_text(
                                    explicit_index,
                                    &flags,
                                    width,
                                    precision,
                                    '%',
                                )),
                            ));
                        }
                        // `fmt_repeat`, not `str::repeat`: `%2000000000%` is a
                        // legal specifier and a two-gigabyte allocation.
                        let pad = fmt_repeat(' ', width.unwrap_or(1).saturating_sub(1))?;
                        if flags == "-" {
                            result.push(u16::from(b'%'));
                            result.extend(pad.encode_utf16());
                        } else {
                            result.extend(pad.encode_utf16());
                            result.push(u16::from(b'%'));
                        }
                    }
                    // Likewise `%n`, reached only when decorated. It takes no
                    // width at all ("If the width is set, an
                    // IllegalFormatWidthException will be thrown") and no flags.
                    'n' => {
                        // `checkText` tests the PRECISION before it switches on
                        // the conversion, so it outranks both the width and the
                        // flag refusals: `%5.2n` is IllegalFormatPrecisionException
                        // on HotSpot 25, not IllegalFormatWidthException. This arm
                        // had no precision check at all, so `%.2n` emitted a line
                        // separator and formatted clean — the `'%'` arm two above
                        // has always had the same check, which is how a family
                        // written twice drifts in one member.
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if let Some(w) = width {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalWidth(w as i32)));
                        }
                        if !flags.is_empty() {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::IllegalFlags(fmt_flags_string(&flags)),
                            ));
                        }
                        result.extend(fmt_line_separator_units());
                    }
                    // "If the conversion is not one of the conversions defined
                    // above, an UnknownFormatConversionException is thrown."
                    // Echoing the specifier back instead made every typo a
                    // silent pass-through, and the argument it should have
                    // consumed stayed queued for the NEXT conversion.
                    other => {
                        return Err(fmt_raise(
                            ctx,
                            &FmtFault::UnknownConversion(other.to_string()),
                        ))
                    }
                }
            } else {
                // Reached end of string after consuming flags/width/precision
                // with no conversion character — a truncated specifier.
                return Err(fmt_raise(
                    ctx,
                    &FmtFault::UnknownConversion(first_after_pct.to_string()),
                ));
            }
        } else {
            // The raw code UNIT, not `chars[i]`: `chars` is the parser's
            // ASCII-testable view and carries U+FFFD wherever the format string
            // held a surrogate.
            result.push(fmt_units[i]);
            i += 1;
        }
    }

    // [`sb_string_from_units`], this file's single lossless `String` writer. For
    // a well-formed result it still takes exactly the
    // `create_string_uninterned_gc_safe` path the `&str` build took, so no
    // allocation, interning or GC-safety property moves for output that has no
    // lone surrogate; only a result that actually carries one takes the units
    // path.
    let obj = sb_string_from_units(ctx, &result)?;
    Ok(Some(Value::Object(Some(obj))))
}

/// `System.lineSeparator()` as code units — `%n`'s output, in one place so the
/// bare `%n` short-circuit and the decorated `%n` arm cannot drift apart on the
/// platform test. (They are ~440 lines apart and have already drifted once, on
/// the precision check; see the decorated arm.)
fn fmt_line_separator_units() -> Vec<u16> {
    if cfg!(windows) {
        vec![u16::from(b'\r'), u16::from(b'\n')]
    } else {
        vec![u16::from(b'\n')]
    }
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
///
/// `field` is the `%t` field character, carried only so that an argument this
/// conversion cannot accept is refused as `IllegalFormatConversionException`
/// naming it — `Formatter.printDateTime`'s `else` arm is `failConversion(c,
/// arg)` and `c` for a date/time specifier is the FIELD, not the 't'.
/// The zone the `%t` fields were computed IN.
///
/// `java.util.Formatter.printDateTime` never formats an instant directly: it
/// builds `Calendar.getInstance(l == null ? Locale.US : l)` — which carries
/// `TimeZone.getDefault()` — and calls `setTimeInMillis`, so every field it then
/// reads is a LOCAL field. A `Calendar` argument is used as it stands and
/// carries its own zone.
///
/// This VM had no zone at all: `millis_to_fields` divided the epoch millis
/// directly, so every field was UTC and the `'z'`/`'Z'` arms answered a fixed
/// `"+0000"`/`"UTC"` that was *consistent with that*. The hard-coded zone name
/// was the visible half; the wrong HOUR, DAY, MONTH and YEAR were the quiet
/// half. Measured on HotSpot 25 for `new Date(1699999999000L)`:
///
/// | default zone | `%tc` | `%tZ` | `%tz` | `%tH` |
/// |---|---|---|---|---|
/// | `UTC` | `Tue Nov 14 22:13:19 UTC 2023` | `UTC` | `+0000` | `22` |
/// | `America/New_York` | `Tue Nov 14 17:13:19 EST 2023` | `EST` | `-0500` | `17` |
/// | `Asia/Kolkata` | `Wed Nov **15** 03:43:19 IST 2023` | `IST` | `+0530` | `03` |
/// | `Europe/Berlin` | `Tue Nov 14 23:13:19 CET 2023` | `CET` | `+0100` | `23` |
///
/// (the Kolkata row rolls the DAY over, which is why this could not be fixed by
/// naming the zone alone). Those `%tZ` values are the `Locale.US` display
/// names; under `Locale.ROOT` the same rows read `GMT-05:00`, `GMT+05:30` and
/// `GMT+01:00`, and in July — with DST in force — New York reads `EDT` /
/// `GMT-04:00` and Berlin `CEST` / `GMT+02:00`. All measured.
#[derive(Clone, Copy)]
struct FmtZone {
    /// `Calendar.ZONE_OFFSET + Calendar.DST_OFFSET` in milliseconds, i.e. what
    /// `TimeZone.getOffset(millis)` answers, at the instant being formatted.
    offset_ms: i32,
    /// `Calendar.DST_OFFSET != 0` — the `daylight` argument of
    /// `TimeZone.getDisplayName`, and the reason `getRawOffset()` is asked for
    /// as well as `getOffset(millis)`.
    dst: bool,
    /// False when no zone could be resolved — the `java.time` types that carry
    /// none, and any configuration where `java.util.TimeZone` could not be
    /// reached. The fields are then UTC and the `'z'`/`'Z'` arms keep their old
    /// fixed answers.
    ///
    /// **`known == false` is not the same question as "must refuse".** A
    /// `LocalDateTime` has no zone and HotSpot REFUSES `%tZ` on it; a VM that
    /// simply cannot reach `java.util.TimeZone` must keep printing `UTC`
    /// rather than start throwing. The refusal is decided by
    /// [`FmtSupport::zone`], which the degraded path leaves TRUE, and this
    /// flag only decides what a non-refusing `%tZ` prints.
    known: bool,
    /// The offset came off a `java.time` object rather than a
    /// `java.util.TimeZone`, so the `%tZ` NAME lookup goes back through the
    /// object ([`fmt_temporal_zone_name`]) and `dst` above is not meaningful —
    /// the daylight question is asked there, of the object's own zone.
    temporal: bool,
}

impl FmtZone {
    const NONE: FmtZone = FmtZone {
        offset_ms: 0,
        dst: false,
        known: false,
        temporal: false,
    };
}

/// Which `java.time.temporal.ChronoField`s the `%t` argument answers — the
/// question `Formatter.print(Formatter, TemporalAccessor, char, Locale)` asks
/// implicitly, by calling `t.get(field)` and turning the resulting
/// `DateTimeException` into `IllegalFormatConversionException(c,
/// t.getClass())`.
///
/// **This is a per-FIELD refusal, not a per-type one, and the brief that
/// commissioned it named only `%tZ`.** F22 measured that `Instant` and
/// `LocalDateTime` throw on `%tZ`; sweeping all 31 fields against all six
/// `java.time` sources on HotSpot 25 shows the refusal is much wider — an
/// `Instant` answers only `%tL`, `%tN`, `%ts` and `%tQ` and throws on the
/// other 27, and a `LocalDate` throws on every time field. Every one of those
/// was previously answered here with a FABRICATED value (`invoke_i32` returns
/// 0 for a method the class does not have), so `%tH` of an `Instant` printed
/// a plausible UTC hour where HotSpot refuses.
///
/// The flags are the field GROUPS the JDK's switch actually partitions
/// on, not a per-type table — a table would have to be re-derived for a
/// further source type, and this does not. Measured group membership (G2-1
/// swept 11 sources × all 31 fields on HotSpot 25.0.3+9, 2026-08-16):
///
/// | source | year | month | day | time | sub_second | instant | zone |
/// |---|---|---|---|---|---|---|---|
/// | `long` / `Long` / `Date` / `Calendar` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `Instant` | | | | | ✓ | ✓ | |
/// | `LocalDate` | ✓ | ✓ | ✓ | | | | |
/// | `LocalTime` | | | | ✓ | ✓ | | |
/// | `LocalDateTime` | ✓ | ✓ | ✓ | ✓ | ✓ | | |
/// | `ZonedDateTime` / `OffsetDateTime` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `OffsetTime` | | | | ✓ | ✓ | | ✓ |
/// | `Year` | ✓ | | | | | | |
/// | `YearMonth` | ✓ | ✓ | | | | | |
/// | `MonthDay` | | ✓ | ✓ | | | | |
/// | `Month` | | ✓ | | | | | |
///
/// The epoch-shaped sources are `ALL` because they do not take the
/// `TemporalAccessor` printer at all — `printDateTime` builds a `Calendar`,
/// whose printer has an arm for every field and refuses none of them.
///
/// **Why `date` is three flags and not one.** It was one until G2-1 swept the
/// partial `java.time` types. `Year` answers `%tY %ty %tC` and refuses `%tm`;
/// `YearMonth` answers those and `%tm %tB %tb %th` and refuses `%td`;
/// `MonthDay` answers the month and day fields and refuses `%tY`. A single
/// `date` flag cannot express any of the three, and — because `%tD`, `%tF` and
/// `%tc` each report the FIRST missing inner field — it cannot express their
/// refusal CHARACTERS either (measured: `%tF` of a `Year` is `m`, of a
/// `YearMonth` is `d`, of a `MonthDay` is `F`).
#[derive(Clone, Copy)]
struct FmtSupport {
    /// `YEAR_OF_ERA` — `%tY %ty %tC`, and the year slots of `%tD`/`%tF`/`%tc`.
    year: bool,
    /// `MONTH_OF_YEAR` — `%tm %tB %tb %th`, and the month slots of the three
    /// composites.
    month: bool,
    /// `DAY_OF_MONTH` — `%td %te`, and the day slots of the three composites.
    day: bool,
    /// `HOUR_OF_DAY`, `CLOCK_HOUR_OF_AMPM`, `MINUTE_OF_HOUR`,
    /// `SECOND_OF_MINUTE`, `AMPM_OF_DAY` — `%tH %tk %tI %tl %tM %tS %tp`, plus
    /// `%tR %tT %tr`.
    time: bool,
    /// `MILLI_OF_SECOND` / `NANO_OF_SECOND` — `%tL` and `%tN`. Separate from
    /// `time` because an `Instant` answers these two and no other clock field.
    sub_second: bool,
    /// `INSTANT_SECONDS` — `%ts`, and (with `sub_second`) `%tQ`.
    instant: bool,
    /// The zone query answers non-null and `OFFSET_SECONDS` is supported —
    /// `%tz` and `%tZ`. Left TRUE on the degraded `java.util.TimeZone` path so
    /// that a VM which cannot resolve a zone keeps PRINTING one rather than
    /// starting to refuse; see [`FmtZone::known`].
    zone: bool,
    /// **Which of `printDateTime`'s two printers this source takes**, not a
    /// support question — and it cannot be derived from the flags above,
    /// because a `ZonedDateTime` supports every group and still takes the
    /// OTHER printer.
    ///
    /// `java.util.Formatter.printDateTime` dispatches
    /// `long`/`Long`/`Date`/`Calendar` to `print(Formatter, Calendar, char,
    /// Locale)` and every `TemporalAccessor` to `print(Formatter,
    /// TemporalAccessor, char, Locale)`. The two are near-copies of each other
    /// and they disagree about exactly one conversion, `%tF`:
    ///
    /// ```text
    /// Calendar          case ISO_STANDARD_DATE:  print(YEAR_4) '-' print(MONTH) '-' print(DAY)
    /// TemporalAccessor  case ISO_STANDARD_DATE:  int year = t.get(YEAR);          // PROLEPTIC
    ///                                            if (year < 0) { getMinusSign(l); year = -year; }
    ///                                            else if (year > 9999) '+';
    ///                                            localizedMagnitude(year, ZERO_PAD, 4) …
    /// ```
    ///
    /// `YEAR_4` is `Calendar.YEAR`, which is ERA-RELATIVE and never negative, so
    /// the `Calendar` arm has no sign rule, no `'+'` rule and no localized minus
    /// at all. Measured on HotSpot 25, `Locale.ROOT`, `Asia/Kolkata`:
    ///
    /// | argument | `%tF` | `%tY` |
    /// |---|---|---|
    /// | `LocalDate.of(-44,3,15)` | `-0044-03-15` | `0045` |
    /// | `GregorianCalendar` ERA=BC YEAR=45 (the SAME instant) | **`0045-03-15`** | `0045` |
    /// | `new Date(-63549548877000L)` (the same instant) | **`0045-03-15`** | `0045` |
    /// | `LocalDate.of(12345,3,4)` | `+12345-03-04` | `12345` |
    /// | `GregorianCalendar` AD 12345 | **`12345-03-04`** | `12345` |
    ///
    /// The `%tY` column is identical down both printers, which is the point of
    /// §5.2's correction: see [`format_temporal_field`]'s `year_of_era`.
    calendar_printer: bool,
}

impl FmtSupport {
    /// The epoch-shaped sources, which take the `Calendar` printer and refuse
    /// nothing.
    const ALL: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: true,
        time: true,
        sub_second: true,
        instant: true,
        zone: true,
        calendar_printer: true,
    };

    /// `DAY_OF_WEEK` (`%tA %ta`) and `DAY_OF_YEAR` (`%tj`) — both DERIVED,
    /// because neither can be answered without a complete year/month/day and
    /// every source that has all three answers both.
    ///
    /// Measured over the 11 sources G2-1 swept: `LocalDate`, `LocalDateTime`,
    /// `ZonedDateTime`, `OffsetDateTime` and the four `ChronoLocalDate`s have
    /// y+m+d and answer `%tA %ta %tj`; `Instant`, `LocalTime`, `OffsetTime`,
    /// `Year`, `YearMonth`, `MonthDay` and `Month` each lack at least one of
    /// the three and refuse all three fields.
    ///
    /// **One measured source contradicts the derivation and is deliberately
    /// not decoded**: `java.time.DayOfWeek` answers `%tA`/`%ta` while having
    /// no year, month or day at all (and its `%tc` reports `b`, not the `a`
    /// this derivation would give). `extract_temporal_fields` refuses it, so
    /// the shape never reaches here. Decoding it needs a day-of-week that does
    /// not come from a date, which is a plumbing change through
    /// `format_temporal_field`; see G2-1's residuals.
    const fn full_date(self) -> bool {
        self.year && self.month && self.day
    }
}

/// The character `IllegalFormatConversionException` reports for `field` on a
/// source with this support, or `None` when the field is answerable.
///
/// The reported character is NOT always the one written in the format string:
/// the composite conversions delegate to nested `print` calls and it is the
/// INNER field that raises, so `%tT` of a `LocalDate` reports `H` and `%tc` of
/// an `Instant` reports `a`. Every row below was measured on HotSpot 25:
///
/// | source | `%tR` `%tT` | `%tr` | `%tD` | `%tF` | `%tc` |
/// |---|---|---|---|---|---|
/// | `Instant` | `H` | `I` | `m` | `F` | `a` |
/// | `LocalDate` | `H` | `I` | ok | ok | `H` |
/// | `LocalTime` | ok | ok | `m` | `F` | `a` |
/// | `LocalDateTime` | ok | ok | ok | ok | `Z` |
/// | `OffsetTime` | ok | ok | `m` | `F` | `a` |
/// | `Year` | `H` | `I` | `m` | `m` | `a` |
/// | `YearMonth` | `H` | `I` | `d` | `d` | `a` |
/// | `MonthDay` | `H` | `I` | `y` | `F` | `a` |
/// | `Month` | `H` | `I` | `d` | `F` | `a` |
///
/// The last four rows are G2-1's, measured 2026-08-16, and they are what
/// forces the three composites to walk their inner fields in ORDER rather than
/// consult a single `date` flag: `%tD` is `mm/dd/yy` and reports `m`, `d` or
/// `y`; `%tF` is `YYYY-mm-dd` and reports its OWN `F` only when the YEAR is
/// the missing one (`ISO_STANDARD_DATE` reads the year with `t.get(yearField)`
/// inline instead of delegating), then `m`, then `d`.
fn fmt_temporal_fault_char(field: char, s: FmtSupport) -> Option<char> {
    fn need(ok: bool, c: char) -> Option<char> {
        if ok {
            None
        } else {
            Some(c)
        }
    }
    match field {
        'H' | 'k' | 'I' | 'l' | 'M' | 'S' | 'p' => need(s.time, field),
        'L' | 'N' => need(s.sub_second, field),
        's' => need(s.instant, field),
        // `MILLISECOND_SINCE_EPOCH` reads INSTANT_SECONDS *and*
        // MILLI_OF_SECOND, so it needs both — and every source that has the
        // first here also has the second, which is why the row is not
        // separately observable.
        'Q' => need(s.instant && s.sub_second, field),
        'z' | 'Z' => need(s.zone, field),
        'B' | 'b' | 'h' | 'm' => need(s.month, field),
        'd' | 'e' => need(s.day, field),
        'C' | 'Y' | 'y' => need(s.year, field),
        // DAY_OF_WEEK and DAY_OF_YEAR — see [`FmtSupport::full_date`] for why
        // both are the same derived predicate and which source contradicts it.
        'A' | 'a' | 'j' => need(s.full_date(), field),
        'R' | 'T' => need(s.time, 'H'),
        'r' => need(s.time, 'I'),
        // `mm/dd/yy`, in that order.
        'D' => {
            if !s.month {
                Some('m')
            } else if !s.day {
                Some('d')
            } else if !s.year {
                Some('y')
            } else {
                None
            }
        }
        // `YYYY-mm-dd`, in that order, and the year slot reports the OUTER
        // character.
        'F' => {
            if !s.year {
                Some(field)
            } else if !s.month {
                Some('m')
            } else if !s.day {
                Some('d')
            } else {
                None
            }
        }
        // `a`(DAY_OF_WEEK) `b`(month) `d`(day) `T`(->`H`, time) `Z`(zone)
        // `Y`(year), in that order, so the FIRST unsupported group names the
        // character.
        'c' => {
            if !s.full_date() {
                Some('a')
            } else if !s.month {
                Some('b')
            } else if !s.day {
                Some('d')
            } else if !s.time {
                Some('H')
            } else if !s.zone {
                Some('Z')
            } else if !s.year {
                Some('Y')
            } else {
                None
            }
        }
        // Not a field this VM implements; `format_temporal_field`'s own
        // catch-all raises `UnknownFormatConversionException` for it, which is
        // a question about the SPECIFIER and outranks a support question.
        _ => None,
    }
}

/// Resolve the `java.util.TimeZone` for an epoch-shaped `%t` argument and read
/// its offset at `millis`.
///
/// `calendar` is `Some(cal)` for a `java.util.Calendar` argument, whose own zone
/// is authoritative; `None` takes `TimeZone.getDefault()`, which is what
/// `Calendar.getInstance()` would have given the JDK.
///
/// Returns [`FmtZone::NONE`] rather than failing if the class is not reachable:
/// a VM without `java.util.TimeZone` keeps the previous UTC behaviour instead of
/// refusing a conversion that used to work.
fn fmt_resolve_zone(
    ctx: &mut dyn NativeContext,
    calendar: Option<cratonvm_types::ObjectRef>,
    millis: i64,
) -> FmtZone {
    let tz = match calendar {
        Some(cal) => match ctx.invoke_virtual(cal, "getTimeZone", "()Ljava/util/TimeZone;", &[]) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return FmtZone::NONE,
        },
        None => match ctx.invoke(
            "java/util/TimeZone",
            "getDefault",
            "()Ljava/util/TimeZone;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return FmtZone::NONE,
        },
    };
    // `getOffset(long)` is ZONE_OFFSET + DST_OFFSET together; the raw offset is
    // ZONE_OFFSET alone, so the difference IS DST_OFFSET. Asking the zone rather
    // than reading a Calendar field keeps the `long`/`Date` and `Calendar`
    // sources on one code path.
    let total = match ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return FmtZone::NONE,
    };
    let raw = match ctx.invoke_virtual(tz, "getRawOffset", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        // A zone that answered its offset but not its raw offset is still a
        // usable zone; only the DST *name* selection degrades.
        _ => total,
    };
    FmtZone {
        offset_ms: total,
        dst: total != raw,
        known: true,
        temporal: false,
    }
}

/// The offset a `java.time.ZonedDateTime` / `OffsetDateTime` carries, as a
/// [`FmtZone`].
///
/// `getOffset()` is declared on BOTH and returns a `ZoneOffset`, whose
/// `getTotalSeconds()` is `ChronoField.OFFSET_SECONDS` — which is exactly what
/// `Formatter`'s `ZONE_NUMERIC` arm reads. Measured on HotSpot 25: `%tz` of an
/// `Instant.atZone("Asia/Tokyo")` is `+0900`, of `atZone("Asia/Kolkata")`
/// `+0530`, of `atOffset(-3)` `-0300`, of `atOffset(-3,-30)` `-0330`, and of
/// `atOffset(UTC)` `+0000` — none of which move with `user.timezone`.
///
/// `dst` is left false and is not read for these: the daylight question is
/// asked of the object's own zone in [`fmt_temporal_zone_name`], which is also
/// the only consumer of it here. `known` is true so the `'z'` arm renders the
/// real offset instead of the fixed `+0000`.
fn fmt_temporal_zone(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> FmtZone {
    let off = match ctx.invoke_virtual(obj, "getOffset", "()Ljava/time/ZoneOffset;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return FmtZone::NONE,
    };
    let secs = match ctx.invoke_virtual(off, "getTotalSeconds", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return FmtZone::NONE,
    };
    FmtZone {
        offset_ms: secs.saturating_mul(1000),
        dst: false,
        known: true,
        temporal: true,
    }
}

/// `%tZ` — `tz.getDisplayName(DST_OFFSET != 0, TimeZone.SHORT, l)`.
///
/// Re-resolves the `TimeZone` from the argument instead of carrying an
/// `ObjectRef` in [`FmtZone`]: everything between the field extraction and this
/// call runs Java bytecode (`fmt_date_name` alone invokes three methods for
/// `%tc`), and a held reference across that is a moved-object hazard. `val` is
/// the caller's own varargs element, so it is rooted for the whole conversion.
///
/// `None` means "no display name available" and the caller falls back to the
/// numeric `GMT±HH:MM` form, which is also what HotSpot itself prints under a
/// locale with no short name for the zone (measured: `Locale.ROOT` gives
/// `GMT-05:00` where `Locale.US` gives `EST`).
fn fmt_zone_display_name(
    ctx: &mut dyn NativeContext,
    val: &Value,
    zone: FmtZone,
    locale: FmtLocale,
) -> Option<String> {
    if !zone.known {
        return None;
    }
    // A `java.time` source names its own zone; there is no `TimeZone.getDefault`
    // in that answer at all.
    if zone.temporal {
        return fmt_temporal_zone_name(ctx, val, locale);
    }
    let calendar = fmt_calendar_arg(ctx, val);
    let tz = match calendar {
        Some(cal) => match ctx.invoke_virtual(cal, "getTimeZone", "()Ljava/util/TimeZone;", &[]) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return None,
        },
        None => match ctx.invoke(
            "java/util/TimeZone",
            "getDefault",
            "()Ljava/util/TimeZone;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return None,
        },
    };
    fmt_zone_name_of(ctx, tz, zone.dst, locale)
}

/// `tz.getDisplayName(daylight, TimeZone.SHORT, l)` with this file's
/// [`FmtLocale`] resolution — the one body both `%tZ` name lookups share so
/// they cannot answer the locale question two ways.
///
/// `TimeZone.SHORT` is 0. `Given(None)` takes `Locale.US`, which is the JDK's
/// own `Objects.requireNonNullElse(l, Locale.US)` — the same rule
/// [`fmt_date_name`] now applies, and the reason it can no longer be reached
/// by falling through to the two-argument overload (whose locale is the
/// DEFAULT). If `Locale.US` cannot be read — an uninitialized
/// `java.util.Locale`, a build with no such field — this degrades to that
/// two-argument overload, i.e. to exactly the answer it gave before, rather
/// than refusing.
fn fmt_zone_name_of(
    ctx: &mut dyn NativeContext,
    tz: cratonvm_types::ObjectRef,
    dst: bool,
    locale: FmtLocale,
) -> Option<String> {
    const TZ_SHORT: i32 = 0;
    let explicit = match locale {
        FmtLocale::Given(Some(l)) => Some(l),
        FmtLocale::Given(None) => fmt_locale_us(ctx),
        FmtLocale::DefaultFormat => None,
    };
    let name = match explicit {
        Some(l) => ctx.invoke_virtual(
            tz,
            "getDisplayName",
            "(ZILjava/util/Locale;)Ljava/lang/String;",
            &[
                Value::Int(i32::from(dst)),
                Value::Int(TZ_SHORT),
                Value::Object(Some(l)),
            ],
        ),
        None => ctx.invoke_virtual(
            tz,
            "getDisplayName",
            "(ZI)Ljava/lang/String;",
            &[Value::Int(i32::from(dst)), Value::Int(TZ_SHORT)],
        ),
    };
    match name {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// `Locale.US`, read as a static field.
///
/// The JDK spells the explicit-null locale `Objects.requireNonNullElse(l,
/// Locale.US)` in five places, and the four name-array ones are served by
/// [`fmt_date_name`]'s English tables without needing a `Locale` object at
/// all. `%tZ`'s display name cannot be tabulated, so this is the one site that
/// needs the object itself.
///
/// A static-field READ, not a class load and not `Locale.of("en","US")`: this
/// runs no bytecode, allocates nothing, and answers `None` — degrading to the
/// caller's previous behaviour — on any configuration where `java.util.Locale`
/// is absent, is not yet initialized, or has no such field. Nothing here may
/// fabricate a `Locale`; a wrong one would silently rename every zone.
fn fmt_locale_us(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ObjectRef> {
    let cid = ctx.class_id_by_name("java/util/Locale")?;
    let idx = ctx.static_field_index_by_name(cid, "US")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(l)) => Some(l),
        _ => None,
    }
}

/// `%tZ` for a `java.time.ZonedDateTime` / `OffsetDateTime` — the
/// `DateTime.ZONE` arm of `print(Formatter, TemporalAccessor, char, Locale)`,
/// which is a genuinely different lookup from the `TimeZone.getDefault()` one
/// above:
///
/// ```text
/// ZoneId zid = t.query(TemporalQueries.zone());
/// if (zid == null) throw new IllegalFormatConversionException(c, t.getClass());
/// if (!(zid instanceof ZoneOffset) && t.isSupported(INSTANT_SECONDS)) {
///     ... TimeZone.getTimeZone(zid.getId())
///                 .getDisplayName(zid.getRules().isDaylightSavings(instant),
///                                 TimeZone.SHORT, requireNonNullElse(l, US));
/// }
/// sb.append(zid.getId());
/// ```
///
/// So a FIXED-offset zone prints its own id and never a display name, and a
/// REGION zone prints the `java.util.TimeZone` short name. Measured on
/// HotSpot 25, at `1699999999000L`, under `-Duser.timezone=Asia/Kolkata` (the
/// default zone is visible in none of them):
///
/// | argument | `Locale.ROOT` | `Locale.US` | `(Locale) null` |
/// |---|---|---|---|
/// | `atZone("Asia/Tokyo")` | `GMT+09:00` | `JST` | `JST` |
/// | `atZone("Asia/Kolkata")` | `GMT+05:30` | `IST` | |
/// | `atZone("UTC")` | `UTC` | | |
/// | `atZone(ZoneOffset.ofHours(5))` | `+05:00` | | |
/// | `atOffset(ZoneOffset.UTC)` | `Z` | | |
/// | `atOffset(-3)` | `-03:00` | | |
/// | `atOffset(-3,-30)` | `-03:30` | | |
/// | `atZone("America/New_York")` in **July** | | `EDT` | |
/// | `atZone("America/New_York")` in November | | `EST` | |
///
/// The `Z` row is why the id is taken from the object rather than composed
/// from the offset: `ZoneOffset.UTC.getId()` is the single letter `Z`, and
/// `+05:00` / `-03:30` are colon-separated where `%tz` is not.
///
/// The DST flag is computed the way [`fmt_resolve_zone`] computes it —
/// `tz.getOffset(millis) != tz.getRawOffset()` — rather than through
/// `ZoneRules.isDaylightSavings`, so the two `%tZ` routes ask the daylight
/// question exactly once, in one form. The July/November New York rows are the
/// pair that would separate a right answer from a hard-coded one.
///
/// `None` on any failure, and the caller falls back to
/// [`fmt_zone_gmt_form`] over the offset already read — which IS HotSpot's own
/// `Locale.ROOT` answer for a region zone (the `GMT+09:00` cell above).
fn fmt_temporal_zone_name(
    ctx: &mut dyn NativeContext,
    val: &Value,
    locale: FmtLocale,
) -> Option<String> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    let obj = *obj;
    // GC: every call below runs bytecode and allocates, so the argument and the
    // `ZoneId` are pinned and re-derived across each one. `pin_base` is the
    // FIRST handle, so one `unpin_native_roots(pin_base)` releases the batch.
    let pin_base = ctx.pin_native_root(obj);
    // `ZonedDateTime.getZone()` is the REGION it was built with, which is what
    // `TemporalQueries.zone()` answers for it; `OffsetDateTime` has NO
    // `getZone()` at all and its `getOffset()` is itself a `ZoneId`. The class
    // is tested rather than the call being tried and allowed to fail, because
    // a speculative `getZone` on an `OffsetDateTime` is a `NoSuchMethodError`
    // raised and discarded on every `%tZ`.
    let is_zdt = match ctx.class_id_by_name("java/time/ZonedDateTime") {
        Some(z) => {
            ctx.class_id_of_object(obj) == z || ctx.is_subclass(ctx.class_id_of_object(obj), z)
        }
        None => false,
    };
    let call = if is_zdt {
        ctx.invoke_virtual(obj, "getZone", "()Ljava/time/ZoneId;", &[])
    } else {
        ctx.invoke_virtual(obj, "getOffset", "()Ljava/time/ZoneOffset;", &[])
    };
    let zid = match call {
        Ok(Some(Value::Object(Some(z)))) => Some(z),
        _ => None,
    };
    let Some(zid) = zid else {
        ctx.unpin_native_roots(pin_base);
        return None;
    };
    let zid_pin = ctx.pin_native_root(zid);
    let out = fmt_temporal_zone_name_inner(ctx, obj, pin_base, zid, zid_pin, locale);
    ctx.unpin_native_roots(pin_base);
    out
}

/// The pinned body of [`fmt_temporal_zone_name`], split out so that every early
/// return goes through its caller's single `unpin_native_roots`.
fn fmt_temporal_zone_name_inner(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    obj_pin: usize,
    zid: cratonvm_types::ObjectRef,
    zid_pin: usize,
    locale: FmtLocale,
) -> Option<String> {
    let zid = ctx.read_native_pin(zid_pin, zid);
    // A class-IDENTITY test, not `is_subclass`: `java.time.ZoneOffset` is
    // final, and the JDK's own screen is `zid instanceof ZoneOffset`.
    let fixed = match ctx.class_id_by_name("java/time/ZoneOffset") {
        Some(off) => ctx.class_id_of_object(zid) == off,
        None => false,
    };
    if fixed {
        // `sb.append(zid.getId())` — `Z`, `+05:00`, `-03:30`.
        return match ctx.invoke_virtual(zid, "getId", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|s| !s.is_empty()),
            _ => None,
        };
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    let inst = match ctx.invoke_virtual(obj, "toInstant", "()Ljava/time/Instant;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => return None,
    };
    let millis = match ctx.invoke_virtual(inst, "toEpochMilli", "()J", &[]) {
        Ok(Some(Value::Long(v))) => v,
        _ => return None,
    };
    let zid = ctx.read_native_pin(zid_pin, zid);
    let tz = match ctx.invoke(
        "java/util/TimeZone",
        "getTimeZone",
        "(Ljava/time/ZoneId;)Ljava/util/TimeZone;",
        &[Value::Object(Some(zid))],
    ) {
        Ok(Some(Value::Object(Some(z)))) => z,
        _ => return None,
    };
    let tz_pin = ctx.pin_native_root(tz);
    let total = match ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return None,
    };
    let tz = ctx.read_native_pin(tz_pin, tz);
    let raw = match ctx.invoke_virtual(tz, "getRawOffset", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => total,
    };
    let tz = ctx.read_native_pin(tz_pin, tz);
    fmt_zone_name_of(ctx, tz, total != raw, locale)
}

/// The argument as a `java.util.Calendar`, or `None` for every other `%t`
/// source. One test, used by both zone helpers so they cannot disagree about
/// which arguments carry their own zone.
fn fmt_calendar_arg(ctx: &mut dyn NativeContext, val: &Value) -> Option<cratonvm_types::ObjectRef> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    let parent = ctx.class_id_by_name("java/util/Calendar")?;
    let cid = ctx.class_id_of_object(*obj);
    if cid == parent || ctx.is_subclass(cid, parent) {
        Some(*obj)
    } else {
        None
    }
}

/// `TimeZone`'s own `GMT±HH:MM` fallback form, for a zone with no short display
/// name. Measured as HotSpot's `Locale.ROOT` answer for `%tZ`.
fn fmt_zone_gmt_form(offset_ms: i32) -> String {
    let neg = offset_ms < 0;
    let mins = (offset_ms / 60_000).abs();
    format!(
        "GMT{}{:02}:{:02}",
        if neg { '-' } else { '+' },
        mins / 60,
        mins % 60
    )
}

/// Decode a `%t` argument into `(year, month, day, hour, minute, second,
/// nanos)` plus its [`FmtZone`] and its [`FmtSupport`].
///
/// The third element is the one that decides whether the fields are ANSWERS or
/// fabrications. `invoke_i32` below returns 0 for a method the argument's class
/// does not have, so a `LocalDate` asked for its hour used to come back with a
/// perfectly plausible `00` — where HotSpot 25 raises
/// `IllegalFormatConversionException: H != java.time.LocalDate`. The caller
/// screens `field` against the support BEFORE it renders anything; see
/// [`fmt_temporal_fault_char`] for the measured matrix.
fn extract_temporal_fields(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
) -> Result<((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport), MethodCallFailed> {
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

    // Shift the instant into the zone's local time before splitting it into
    // fields, which is `cal.setTimeInMillis(ms)` on a zoned `Calendar`. Every
    // field the caller then reads — hour, day, month, year — is local, and the
    // `'s'`/`'Q'` arms undo the shift because an epoch second is not a local
    // quantity. `saturating_add` keeps a `Long.MIN_VALUE` argument from
    // wrapping into a far-future date.
    fn zoned_fields(
        ctx: &mut dyn NativeContext,
        calendar: Option<cratonvm_types::ObjectRef>,
        millis: i64,
    ) -> ((i64, i32, i32, i32, i32, i32, i32), FmtZone) {
        let zone = fmt_resolve_zone(ctx, calendar, millis);
        (
            millis_to_fields(millis.saturating_add(i64::from(zone.offset_ms))),
            zone,
        )
    }

    // The epoch-shaped sources take `printDateTime`'s `Calendar` branch, whose
    // printer has an arm for every field and refuses none — hence
    // [`FmtSupport::ALL`] on all four of them.
    fn epoch(
        pair: ((i64, i32, i32, i32, i32, i32, i32), FmtZone),
    ) -> ((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport) {
        (pair.0, pair.1, FmtSupport::ALL)
    }

    match val {
        Value::Long(ms) => Ok(epoch(zoned_fields(ctx, None, *ms))),
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
                Ok(epoch(zoned_fields(ctx, None, millis)))
            } else if is_a(ctx, "java/util/Date") {
                let millis = match ctx.invoke_virtual(obj, "getTime", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                Ok(epoch(zoned_fields(ctx, None, millis)))
            } else if is_a(ctx, "java/util/Calendar") {
                let millis = match ctx.invoke_virtual(obj, "getTimeInMillis", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                // The Calendar's OWN zone, not the default: measured on
                // HotSpot 25, `%tc` of a `Calendar.getInstance(TimeZone
                // .getTimeZone("Asia/Tokyo"))` set to 1699999999000 is
                // `Wed Nov 15 07:13:19 GMT+09:00 2023` whatever
                // `user.timezone` is.
                Ok(epoch(zoned_fields(ctx, Some(obj), millis)))
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
                // An `Instant` answers FOUR fields and refuses the other 27.
                // The date and clock values computed above are UTC and are
                // never rendered — `fmt_temporal_fault_char` refuses every
                // field that would read them — but they are computed anyway
                // because `%tL`/`%tN` need the nanos and `%ts`/`%tQ` need the
                // epoch, and `epoch_sec()` reconstructs the epoch from these.
                Ok((
                    (y, m, d, h, mi, s, nano),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: false,
                        sub_second: true,
                        instant: true,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/LocalDate") {
                let year = invoke_i32(ctx, obj, "getYear") as i64;
                let month = invoke_i32(ctx, obj, "getMonthValue");
                let day = invoke_i32(ctx, obj, "getDayOfMonth");
                Ok((
                    (year, month, day, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: true,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/LocalTime") {
                let hour = invoke_i32(ctx, obj, "getHour");
                let minute = invoke_i32(ctx, obj, "getMinute");
                let second = invoke_i32(ctx, obj, "getSecond");
                let nano = invoke_i32(ctx, obj, "getNano");
                Ok((
                    (1970, 1, 1, hour, minute, second, nano),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: true,
                        sub_second: true,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/OffsetTime") {
                // G2-1. `OffsetTime` was the one `java.time` type with a
                // printer on HotSpot that this decoder had no arm for, so all
                // 31 fields fell to the `else` below and refused — including
                // the FOURTEEN the JDK answers. Measured on HotSpot 25.0.3+9
                // for `2025-08-16T05:00:45.123+05:30`:
                // `%tH`=`05` `%tI`=`05` `%tk`=`5` `%tl`=`5` `%tM`=`00`
                // `%tS`=`45` `%tL`=`123` `%tN`=`123000000` `%tp`=`am`
                // `%tz`=`+0530` `%tZ`=`+05:30` `%tR`=`05:00` `%tT`=`05:00:45`
                // `%tr`=`05:00:45 AM`.
                //
                // It is `LocalTime` plus a zone and NOT an instant: measured,
                // `%ts` is `s != java.time.OffsetTime` and `%tQ` is
                // `Q != java.time.OffsetTime`, because `INSTANT_SECONDS` needs
                // a date the type does not carry. `getOffset()` is a
                // `ZoneOffset`, which is what [`fmt_temporal_zone`] and
                // [`fmt_temporal_zone_name`]'s non-`ZonedDateTime` arm already
                // read — the latter's `getId()` is where `+05:30` comes from.
                let hour = invoke_i32(ctx, obj, "getHour");
                let minute = invoke_i32(ctx, obj, "getMinute");
                let second = invoke_i32(ctx, obj, "getSecond");
                let nano = invoke_i32(ctx, obj, "getNano");
                let zone = fmt_temporal_zone(ctx, obj);
                Ok((
                    (1970, 1, 1, hour, minute, second, nano),
                    zone,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: true,
                        sub_second: true,
                        instant: false,
                        zone: true,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/YearMonth") {
                // G2-1, and the three arms below it. These are the PARTIAL
                // `java.time` types: each supports a proper subset of the date
                // group, which is why [`FmtSupport`] carries `year`/`month`/
                // `day` separately instead of one `date` flag. Measured on
                // HotSpot 25.0.3+9 (`Locale.US`):
                //
                // | source | answers | refuses |
                // |---|---|---|
                // | `YearMonth.of(2020,2)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` `%tm`=`02` `%tB`=`February` `%tb`=`%th`=`Feb` | 24 |
                // | `MonthDay.of(1,2)` | `%tm`=`01` `%td`=`02` `%te`=`2` `%tB`=`January` `%tb`=`%th`=`Jan` | 25 |
                // | `Year.of(2020)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` | 28 |
                // | `Month.JANUARY` | `%tm`=`01` `%tB`=`January` `%tb`=`%th`=`Jan` | 27 |
                //
                // The unread slots are left at the epoch defaults; every field
                // that would read one is refused by
                // [`fmt_temporal_fault_char`], exactly as for `Instant`.
                let year = invoke_i32(ctx, obj, "getYear") as i64;
                let month = invoke_i32(ctx, obj, "getMonthValue");
                Ok((
                    (year, month, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/MonthDay") {
                let month = invoke_i32(ctx, obj, "getMonthValue");
                let day = invoke_i32(ctx, obj, "getDayOfMonth");
                Ok((
                    (1970, month, day, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: true,
                        day: true,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/Year") {
                // `Year.getValue()`, not `getYear()` — the type has no
                // `getYear`.
                let year = invoke_i32(ctx, obj, "getValue") as i64;
                Ok((
                    (year, 1, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: false,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
            } else if is_a(ctx, "java/time/Month") {
                // `Month` is an enum and its `getValue()` is 1..=12, which is
                // already this decoder's month convention.
                let month = invoke_i32(ctx, obj, "getValue");
                Ok((
                    (1970, month, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: true,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                    },
                ))
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
                // All three read LOCAL fields off the object, so no shift
                // applies. The two that carry an offset get it from the object
                // ([`fmt_temporal_zone`]) and are `instant`-capable with it —
                // `%ts` of a `ZonedDateTime` is `1699999999`, which
                // `epoch_sec()` reaches by subtracting the offset back out of
                // these local fields. A `LocalDateTime` has NO zone and no
                // instant: `%tz`, `%tZ`, `%ts` and `%tQ` all refuse
                // (measured), and `%tc` refuses at its zone slot with `Z`.
                let zoned =
                    is_a(ctx, "java/time/ZonedDateTime") || is_a(ctx, "java/time/OffsetDateTime");
                let zone = if zoned {
                    fmt_temporal_zone(ctx, obj)
                } else {
                    FmtZone::NONE
                };
                Ok((
                    (year, month, day, hour, minute, second, nano),
                    zone,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: true,
                        time: true,
                        sub_second: true,
                        instant: zoned,
                        zone: zoned,
                        calendar_printer: false,
                    },
                ))
            } else {
                // `printDateTime`'s `else` arm. HotSpot reports
                // "Y != java.lang.String" for `String.format("%tY", "x")`, a
                // typed refusal a caller can catch as
                // `IllegalFormatConversionException`; the base
                // `IllegalArgumentException` and its invented
                // "cannot be formatted as a date" wording were CratonVM's own.
                Err(fmt_raise(ctx, &FmtFault::WrongType(field, cid)))
            }
        }
        // A primitive that is not a `long` cannot arrive here at all: the
        // varargs array boxes everything, and the `Long` case is handled
        // above. Report it the same way `failConversion` would if it could.
        _ => Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Illegal date/time conversion argument".to_string(),
            }
            .into(),
        ),
    }
}

/// `%tF`'s year, which is NOT `%tY`'s.
///
/// `ISO_STANDARD_DATE` is the one composite that renders a year itself instead
/// of delegating, and its rule is its own:
///
/// ```text
/// if (year < 0)      { sb.append(getMinusSign(l)); year = -year; }
/// else if (year > 9999) { sb.append('+'); }
/// sb.append(localizedMagnitude(fmt, null, year, Flags.ZERO_PAD, 4, l));
/// ```
///
/// so the four-digit zero pad is applied to the ABSOLUTE value and the sign
/// sits outside it. Measured on HotSpot 25 under `Locale.ROOT`:
/// `LocalDate.of(-44, 3, 15)` is `-0044-03-15`, `LocalDate.of(-1, 6, 5)` is
/// `-0001-06-05`, `LocalDate.of(12345, 3, 4)` is `+12345-03-04`, and
/// `LocalDate.of(9999, 12, 31)` is `9999-12-31` with no sign at all.
///
/// Rust's `{:04}` counts the sign INSIDE the width (`-044`) and has no '+'
/// rule, which is what this replaces. The digits stay ASCII here and are
/// substituted by [`fmt_localize_digits`] with the rest of the field — the
/// sign is not a digit, so it survives that pass unchanged.
///
/// # This is the ONLY caller of `getMinusSign` in the whole class
///
/// `minus` is [`FmtSymbols::minus`], i.e.
/// `DecimalFormatSymbols.getInstance(l).getMinusSign()`, and F28-1 §5.3's
/// residual is closed here. The JDK's `getMinusSign(Locale)` has exactly one
/// call site and it is this arm: every other negative number in
/// `java.util.Formatter` goes through `leadingSign`, which appends an ASCII
/// `'-'`. Measured under `lt-LT` (`getMinusSign()` = U+2212):
/// `String.format(lt, "%d", -5)` is `002D 0035` while
/// `String.format(lt, "%tF", LocalDate.of(-44,3,15))` is
/// `2212 0030 0030 0034 0034` `002D` `…` — the year's sign localized, the two
/// date separators in the same field left ASCII. See [`FmtSymbols`] for the
/// full measured table and for why the earlier "every negative `%d`/`%f`"
/// framing was wrong.
///
/// The `'+'` past 9999 is NOT symmetric with it and stays ASCII: the JDK writes
/// `sb.append('+')` as a literal in the same block it calls `getMinusSign(l)`
/// in, and `String.format(lt, "%tF", LocalDate.of(12345,3,4))` is measured
/// `+12345-03-04` with an ASCII U+002B.
///
/// **Only the `TemporalAccessor` printer reaches here.** The `Calendar` printer
/// renders `%tF`'s year as a plain era-relative `YEAR_4` with no sign rule at
/// all — see [`FmtSupport::calendar_printer`], which is what selects between
/// the two.
fn fmt_iso_year(year: i64, minus: char) -> String {
    let (head, y) = if year < 0 {
        (Some(minus), year.saturating_neg())
    } else if year > 9999 {
        (Some('+'), year)
    } else {
        (None, year)
    };
    match head {
        Some(sign) => format!("{sign}{y:04}"),
        None => format!("{y:04}"),
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
///
/// The six name-bearing fields — `%tB` `%tb`/`%th` `%tA` `%ta` `%tp`, and
/// `%tc`, which is a composite over two of them — go through
/// [`fmt_date_name`], i.e. through `java.text.DateFormatSymbols`, because that
/// is what `java.util.Formatter.print(TemporalAccessor, char, Locale)` does.
/// They used to be answered from the four English tables below **whatever the
/// locale was**, so every `java.util.logging.SimpleFormatter` line rendered its
/// month in English on a host that is not English — its default pattern opens
/// `%1$tb`. The tables survive as the fallback for the configurations where
/// there is no `DateFormatSymbols` to ask, which is the same value they always
/// produced.
fn format_temporal_field(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
    flags: &str,
    width: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
    uppercase: bool,
) -> Result<Vec<u16>, MethodCallFailed> {
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

    // "If the argument arg is null, then the result is 'null'" —
    // `printDateTime` returns before it ever looks at the field, so a null
    // date is not a refusal. This used to reach `extract_temporal_fields`'
    // catch-all and come back as an `IllegalArgumentException`.
    if matches!(val, Value::Object(None)) {
        // Same shared printer as every other conversion's null — the word,
        // then the (locale-sensitive) upper-caser, then the justifier.
        let mut s = fmt_null_text('t').to_string();
        if uppercase {
            s = fmt_upper_case(ctx, &s, locale);
        }
        return fmt_pad_str_to_width(&s, flags, width);
    }
    let ((year, month, day, hour, minute, second, nanos), zone, support) =
        extract_temporal_fields(ctx, val, field)?;
    // `t.get(ChronoField)` on a field the argument does not support is a
    // `DateTimeException`, which `print(Formatter, TemporalAccessor, char,
    // Locale)` catches and rethrows as `IllegalFormatConversionException(c,
    // t.getClass())`. Screening here rather than at each arm is what keeps the
    // 31 arms from each needing to know which sources can answer them — and
    // the fields decoded above are, for an unsupported field, FABRICATIONS
    // (`invoke_i32` answers 0 for an absent method), so nothing downstream may
    // see them. The character reported is not always `field`; see
    // [`fmt_temporal_fault_char`].
    //
    // A `Value::Long` argument cannot reach the `Some` arm — `FmtSupport::ALL`
    // refuses nothing — which is also why the class needed for the message is
    // always available here.
    if let (Some(c), Value::Object(Some(obj))) = (fmt_temporal_fault_char(field, support), val) {
        let cid = ctx.class_id_of_object(*obj);
        return Err(fmt_raise(ctx, &FmtFault::WrongType(c, cid)));
    }
    let year32 = year as i32;
    // `%tY` `%ty` `%tC` — and the year slots of the `%tD` and `%tc` composites
    // — render the ERA-RELATIVE year, never the proleptic one. Both of the
    // JDK's printers do, by two different routes that agree:
    //
    //   TemporalAccessor   int i = t.get(ChronoField.YEAR_OF_ERA);
    //   Calendar           int i = t.get(Calendar.YEAR);   // already era-relative
    //
    // so ONE expression serves every source this VM decodes. F28-1 §5.2 left
    // this open on the premise that "`Calendar.YEAR` is already era-relative, so
    // a single expression cannot serve both". That premise is about the JDK's
    // two printers and it does not transfer, because CratonVM never reads
    // `Calendar.YEAR`: `extract_temporal_fields` decodes EVERY epoch-shaped
    // source through `millis_to_fields` → `temporal_from_epoch_day`, which
    // yields a PROLEPTIC year exactly like `getYear()` does for the `java.time`
    // sources. Both halves were measured on HotSpot 25 (`Locale.ROOT`,
    // `Asia/Kolkata`) at the one instant, 45 BC:
    //
    // | argument | `%tY` | `%ty` | `%tC` | `%tD` | `%tc` tail |
    // |---|---|---|---|---|---|
    // | `LocalDate.of(-44,3,15)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `GregorianCalendar` ERA=BC YEAR=45 | `0045` | `45` | `00` | `03/15/45` | `0045` |
    // | `new Date(-63549548877000L)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `Long.valueOf(-63549548877000L)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `LocalDate.of(-1,6,5)` | `0002` | `02` | `00` | | |
    // | `LocalDate.of(0,2,29)` | `0001` | `01` | `00` | | |
    //
    // The proleptic year is still what `%tF`'s TemporalAccessor arm, the
    // day-of-week and the day-of-year read, so it is not overwritten here.
    //
    // `1 - year` and not `-year`: proleptic 0 is 1 BC, so the mapping is
    // 0 → 1, -1 → 2, -44 → 45. Verified against `ChronoField.YEAR_OF_ERA` on
    // all four of those inputs.
    let year_of_era = if year <= 0 { 1 - year } else { year };
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
    // `Calendar.AM`/`Calendar.PM` are 0 and 1, which is the index into
    // `getAmPmStrings()`. The English pair is the JDK's own literal
    // `String[] ampm = { "AM", "PM" }` for the null-locale branch, lower-cased
    // the way `printDateTime` lower-cases whatever it read.
    let ampm_idx = if hour < 12 { 0usize } else { 1usize };
    let ampm_en = if hour < 12 { "am" } else { "pm" };
    // `%ts`/`%tQ` are `Calendar.getTimeInMillis()` — an EPOCH quantity, and the
    // one pair of fields that is not local. The fields above have already been
    // shifted into the zone, so the shift is subtracted back out here.
    // Measured on HotSpot 25: `%ts` of `new Date(1699999999000L)` is
    // `1699999999` and `%tQ` is `1699999999000` under UTC, America/New_York,
    // Asia/Kolkata and Europe/Berlin alike — the value does not move with the
    // zone, while `%tH` does.
    let epoch_sec = || {
        temporal_to_epoch_day(year32, month, day) * 86_400
            + hour as i64 * 3600
            + minute as i64 * 60
            + second as i64
            - i64::from(zone.offset_ms) / 1000
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
        // `s.toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))`
        // — the locale's lower-caser, through the same `case_map` the `%S`/`%T`
        // upper-caser goes through. This was `str::to_lowercase`, Rust's
        // locale-independent one; see [`fmt_lower_case`], which also records
        // that the 1158-locale sweep found no row that moves.
        //
        // The `unwrap_or_else` fallback is the JDK's own English literal, which
        // is already lower-case in ASCII and is what the null-locale branch
        // lower-cases. It is not routed through the case mapper because there
        // is nothing to map and this arm is reached exactly when
        // `DateFormatSymbols` could not be asked at all.
        'p' => {
            // Two `ctx` uses in sequence, bound out like the `'r'` arm below
            // does with its upper-caser, rather than chained through a closure.
            let name = fmt_date_name(ctx, locale, "getAmPmStrings", ampm_idx);
            match name {
                Some(s) => fmt_lower_case(ctx, &s, locale),
                None => ampm_en.to_string(),
            }
        }
        // `ZONE_OFFSET + DST_OFFSET`, rendered `{-|+}HHMM`. See [`FmtZone`] for
        // the measured table and for why an unknown zone keeps the old fixed
        // `+0000`/`UTC` pair rather than inventing one.
        'z' => {
            let i = zone.offset_ms;
            let mins = (i / 60_000).abs();
            format!(
                "{}{:02}{:02}",
                if i < 0 { '-' } else { '+' },
                mins / 60,
                mins % 60
            )
        }
        'Z' => fmt_zone_display_name(ctx, val, zone, locale).unwrap_or_else(|| {
            if zone.known {
                fmt_zone_gmt_form(zone.offset_ms)
            } else {
                "UTC".to_string()
            }
        }),
        's' => format!("{}", epoch_sec()),
        'Q' => format!("{}", epoch_sec() * 1000 + millis as i64),
        // Date
        'B' => fmt_date_name(ctx, locale, "getMonths", month_idx)
            .unwrap_or_else(|| MONTHS_FULL[month_idx].to_string()),
        'b' | 'h' => fmt_date_name(ctx, locale, "getShortMonths", month_idx)
            .unwrap_or_else(|| MONTHS_ABBR[month_idx].to_string()),
        // `getWeekdays()` is indexed by `Calendar.SUNDAY`..`SATURDAY`, i.e.
        // 1..7 with slot 0 unused, so the 0=Sunday `dow` shifts by one. The
        // JDK reaches the same index from the other direction, as
        // `DAY_OF_WEEK % 7 + 1` over an ISO 1=Monday field.
        'A' => fmt_date_name(ctx, locale, "getWeekdays", dow + 1)
            .unwrap_or_else(|| DAYS_FULL[dow].to_string()),
        'a' => fmt_date_name(ctx, locale, "getShortWeekdays", dow + 1)
            .unwrap_or_else(|| DAYS_ABBR[dow].to_string()),
        // `CENTURY`/`YEAR_2`/`YEAR_4` are one JDK arm over one value:
        // `i = YEAR_OF_ERA; C -> i /= 100; y -> i %= 100; Y -> size = 4`.
        // `year_of_era` is >= 1 by construction, so plain `/` and `%` are the
        // euclidean ones here and no sign can reach the zero pad.
        'C' => format!("{:02}", year_of_era / 100),
        'Y' => format!("{:04}", year_of_era),
        'y' => format!("{:02}", year_of_era % 100),
        'j' => format!("{:03}", temporal_day_of_year(year32, month, day)),
        'm' => format!("{:02}", month),
        'd' => format!("{:02}", day),
        'e' => format!("{}", day),
        // Composites
        'R' => format!("{:02}:{:02}", hour, minute),
        'T' => format!("{:02}:{:02}:{:02}", hour, minute, second),
        'r' => {
            // `sb.append(toUpperCaseWithLocale(tsb.toString(), l))` — %tr
            // upper-cases the AM/PM marker against the call's locale, and that
            // is a separate site from the `%T` prefix's own upper-caser.
            let ampm = fmt_date_name(ctx, locale, "getAmPmStrings", ampm_idx)
                .unwrap_or_else(|| ampm_en.to_string());
            let ampm = fmt_upper_case(ctx, &ampm, locale);
            // The clock is `localizedMagnitude`d, the MARKER is not — it is a
            // `DateFormatSymbols` name. Measured under `ar-EG`: `%tr` is
            // U+0660 U+0663 ':' U+0664 U+0663 ':' U+0661 U+0669 ' ' U+0635.
            let hms = fmt_localize_digits(&format!("{hour12:02}:{minute:02}:{second:02}"), sym);
            format!("{hms} {ampm}")
        }
        // `%tD` is `print(MONTH) '/' print(DAY_OF_MONTH_0) '/' print(YEAR_2)`,
        // so its year slot is `%ty`'s and takes the era-relative year too:
        // measured, `%tD` of `LocalDate.of(-44,3,15)` is `03/15/45`, not
        // `03/15/56` (which is what the proleptic `(-44).rem_euclid(100)` gave).
        'D' => format!("{:02}/{:02}/{:02}", month, day, year_of_era % 100),
        // `ISO_STANDARD_DATE` is the one conversion where the JDK's two
        // printers DISAGREE, so it is the one arm that has to ask which source
        // it has. See [`FmtSupport::calendar_printer`] for the measured table.
        //
        //  * `TemporalAccessor`: reads the PROLEPTIC year inline and renders it
        //    itself — a negative year is `getMinusSign(l)` followed by the
        //    ABSOLUTE value zero-padded to four, and a year past 9999 carries a
        //    literal '+'. `LocalDate.of(-44,3,15)` is `-0044-03-15`,
        //    `LocalDate.of(12345,3,4)` is `+12345-03-04`. Rust's `{:04}` counts
        //    the sign inside the width and answers `-044-03-15`, and it has no
        //    '+' rule at all.
        //  * `Calendar`: delegates to `print(YEAR_4)`, i.e. to `%tY`'s
        //    era-relative value, which is never negative — so no sign rule and
        //    no '+'. The SAME instant as above, as a `GregorianCalendar` with
        //    ERA=BC YEAR=45, is `0045-03-15`, and AD 12345 is `12345-03-04`
        //    with NO leading '+'.
        //
        // Rendering the second one through `fmt_iso_year` was a live
        // divergence: `%tF` of a `Date`/`Calendar`/`long` past year 9999 gained
        // a '+' the JDK does not write there.
        'F' => {
            let y = if support.calendar_printer {
                format!("{year_of_era:04}")
            } else {
                fmt_iso_year(year, sym.minus)
            };
            format!("{y}-{month:02}-{day:02}")
        }
        // `%tc` is the JDK's own composite `%ta %tb %td %tT %tZ %tY` — so its
        // day is `DAY_OF_MONTH_0`, ZERO-padded (`Sat Nov 04 …`, the javadoc's
        // own example). It was space-padded here, which is `%te`'s rule.
        // The zone slot is `print(fmt, sb, t, DateTime.ZONE, l)` — the SAME
        // lookup as `%tZ`, not a hard-coded word. It used to be the literal
        // "UTC", which was consistent with the fields being UTC and wrong with
        // them: `%tc` of `new Date(1699999999000L)` under `America/New_York` is
        // `Tue Nov 14 17:13:19 EST 2023` on HotSpot 25 and was
        // `Tue Nov 14 22:13:19 UTC 2023` here — five hours, the zone name, and
        // under `Asia/Kolkata` the DATE as well.
        'c' => {
            let weekday = fmt_date_name(ctx, locale, "getShortWeekdays", dow + 1)
                .unwrap_or_else(|| DAYS_ABBR[dow].to_string());
            let month_name = fmt_date_name(ctx, locale, "getShortMonths", month_idx)
                .unwrap_or_else(|| MONTHS_ABBR[month_idx].to_string());
            let zone_name = fmt_zone_display_name(ctx, val, zone, locale).unwrap_or_else(|| {
                if zone.known {
                    fmt_zone_gmt_form(zone.offset_ms)
                } else {
                    "UTC".to_string()
                }
            });
            // Three name slots and two numeric ones. Only the numeric ones are
            // `localizedMagnitude`d — measured under `ar-EG`, `%tc` of a
            // `Date` keeps the Arabic weekday and month as text, keeps the
            // zone name `IST` in ASCII, and renders the day, the clock and the
            // year in Arabic-Indic digits. Localizing the composed string
            // instead would rewrite the digits INSIDE a `GMT+05:30` zone name.
            let dt =
                fmt_localize_digits(&format!("{day:02} {hour:02}:{minute:02}:{second:02}"), sym);
            // `%tc`'s tail is `print(fmt, sb, t, DateTime.YEAR_4, l)` — `%tY`'s
            // arm, so the ERA-RELATIVE year and not the proleptic one.
            // Measured: `%tc` of a `ZonedDateTime` in 45 BC ends `… 0045`, and
            // so does `%tc` of the `GregorianCalendar` for the same instant.
            let y = fmt_localize_digits(&format!("{year_of_era:04}"), sym);
            format!("{weekday} {month_name} {dt} {zone_name} {y}")
        }
        // Unreachable today: the caller already screened `field` against
        // `FMT_DATETIME_FIELDS`, which is `DateTime.isValid`'s own set, and
        // every one of its 31 characters has an arm above. Kept as the arm a
        // future divergence between the two sets would land in, raising the
        // refusal the JDK names rather than the base class.
        _ => {
            return Err(fmt_raise(
                ctx,
                &FmtFault::UnknownConversion(format!("t{field}")),
            ));
        }
    };

    // Every NUMBER a `%t` field renders goes through `localizedMagnitude`,
    // which substitutes the locale's zero digit — this whole family did it in
    // ASCII. Measured on HotSpot 25 under `ar-EG` (zero digit U+0660):
    // `%tH` of `new Date(1699999999000L)` is `U+0660 U+0663`, `%tT` is
    // `U+0660 U+0663 ':' U+0664 U+0663 ':' U+0661 U+0669` (the SEPARATORS stay
    // ASCII), `%tz` is `'+' U+0660 U+0665 U+0663 U+0660` (the SIGN stays
    // ASCII), `%ts`, `%tQ`, `%tY`, `%tj`, `%td`, `%tD`, `%tF`, `%tL`, `%tN`,
    // `%tC`, `%ty`, `%tm`, `%tI`, `%tk`, `%tl` likewise. `java.util.logging`'s
    // default `SimpleFormatter` pattern is `%1$tb %1$td, %1$tY …`, so this is
    // every log line on such a host.
    //
    // The seven NAME fields are excluded because they are
    // `DateFormatSymbols`/`TimeZone` text, not magnitudes — measured, `%tZ`
    // under `ar-EG` is the ASCII `IST` — and the two composites that MIX names
    // with numbers have already localized their own numeric slots above.
    // Width padding is applied after this, in ASCII spaces, which is also
    // HotSpot's order (`[%10tH]` under `ar-EG` is eight U+0020 then two
    // Arabic-Indic digits).
    if !matches!(field, 'B' | 'b' | 'h' | 'A' | 'a' | 'p' | 'Z' | 'c' | 'r') {
        out = fmt_localize_digits(&out, sym);
    }

    // `printDateTime`: `appendJustified(fmt.a, toUpperCaseWithLocale(sb, l))` —
    // the `%T` upper-caser is INSIDE the width and is locale-sensitive.
    if uppercase {
        out = fmt_upper_case(ctx, &out, locale);
    }

    // `_str_` adapter, not a second justifier: a `%t` field is a number, a
    // separator or a `DateFormatSymbols` name, none of which can be an unpaired
    // surrogate, so the `str` shape is not lossy on this path.
    fmt_pad_str_to_width(&out, flags, width)
}

// ---------------------------------------------------------------------------
// java.util.Formatter's floating-point conversions: %f %e %E %g %G %a %A
//
// These are NOT Rust's, and delegating to Rust's was the W7-1 divergence
// `probes/ShadowDifferentialProbe` measured against HotSpot 25:
//
//   String.format("%.3f|%e|%g", 1.0/3, 1234.5, 0.0001)
//     HotSpot   0.333|1.234500e+03|0.000100000
//     CratonVM  0.333|1.2345e3|1.0E-4
//
// Three separate reasons, one per conversion:
//
//   * `%e` went to Rust's `{:e}`, which writes the exponent bare (`e3`).
//     Formatter's is always signed and at least two digits (`e+03`), and its
//     default precision is 6 — the no-precision path did not even reach the
//     precision handling, so it printed the shortest round-trip mantissa.
//   * `%g` went to `{:.prec$}`, i.e. fixed notation, and with no precision it
//     fell through to `Double.toString` (hence `1.0E-4`). Formatter's %g is a
//     third algorithm: it picks scientific or fixed by the exponent of the
//     ROUNDED magnitude against the precision, and — unlike C's %g — never
//     strips trailing zeros, which is why 1e-4 prints as `0.000100000`.
//   * `%a` had no arm at all and fell through to `Double.toString`.
//
// Rounding is the fourth reason and it is not visible in that one probe line.
// Rust rounds ties to EVEN; java.util.Formatter specifies HALF_UP for
// %f/%e/%g, so `%.1f` of 0.25 is Java "0.3" against Rust's "0.2". Rather than
// correct that per call site, everything below does its own digit arithmetic
// (see `fmt_decimal_digits`), where HALF_UP is one comparison. %a is the
// exception: the JDK rounds it in BINARY, half to even, inside
// `Formatter.hexDouble`, and `fmt_hex_float` reproduces that rather than the
// decimal rule.
//
// `Double.toString`'s own 10^-3..10^7 scientific threshold (`format_double`,
// `cratonvm_types::java_double_to_string`) is a DIFFERENT set of rules and is
// deliberately not shared with any of this: %g at default precision switches
// to scientific below 10^-4, not below 10^-3. Only the DIGITS are shared — see
// `fmt_shortest_decimal` — never the layout.
// ---------------------------------------------------------------------------

/// The decimal digits a Java number STRING carries — most significant first,
/// no leading and no trailing zeros — plus the base-10 exponent of the leading
/// digit: `value == d0.d1d2… × 10^exp`. A zero of any spelling answers
/// `([0], 0)`; a leading `-` is ignored, since callers track the sign.
///
/// Accepts both spellings the two producers emit: `Double.toString`'s
/// `1.0E-4` / `123.45`, and `BigDecimal.toPlainString`'s never-scientific
/// form.
fn fmt_decimal_digits(s: &str) -> (Vec<u8>, i32) {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp10) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let (int_str, frac_str) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: Vec<u8> = int_str
        .bytes()
        .chain(frac_str.bytes())
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    // `exp` starts at the exponent of the first INTEGER-part digit and drops by
    // one for every leading zero skipped.
    let mut exp = int_str.len() as i32 - 1 + exp10;
    match digits.iter().position(|&d| d != 0) {
        None => (vec![0], 0),
        Some(lead) => {
            digits.drain(..lead);
            exp -= lead as i32;
            while digits.len() > 1 && digits[digits.len() - 1] == 0 {
                digits.pop();
            }
            (digits, exp)
        }
    }
}

/// A finite, non-negative `f64` as the digits `java.util.Formatter` rounds
/// from — which are `Double.toString`'s SHORTEST round-trip digits, not the
/// value's exact decimal expansion.
///
/// This is the spec, not an approximation of it. The javadoc for `%f`, `%e`
/// and `%g` all say the same sentence: "If the precision is less than the
/// number of digits which would appear after the decimal point in the string
/// returned by `Double#toString(double)`, then the value will be rounded using
/// the round half up algorithm. Otherwise, zeros may be appended to reach the
/// precision."
///
/// W7-3 rounded the EXACT expansion instead and recorded the difference as a
/// residual where "beyond roughly 20 significant digits this implementation is
/// more exact than HotSpot" — the guess being that `FloatingDecimal`'s
/// `char[20]` digit buffer was the cap. Measured against HotSpot 25 on
/// 2026-08-12, the cap is not 20 digits, it is the shortest representation, and
/// so the divergence starts at the FIRST digit past it rather than in some rare
/// tail:
///
/// | expression | HotSpot 25 | exact-expansion |
/// |---|---|---|
/// | `%.1f` of 0.35 | `0.4` | `0.3` (exact is 0.34999999999999997…) |
/// | `%.2f` of 1.005 | `1.01` | `1.00` (exact is 1.00499999999999989…) |
/// | `%.17f` of 0.1 | `0.10000000000000000` | `0.10000000000000001` |
/// | `%.3f` of 1.2345678901234569e23 | `123456789012345690000000.000` | `…685803008.000` |
///
/// Those first two are `format.floatRoundingHalfUp` and
/// `format.formatterAppendable` in `probes/ShadowDifferentialProbe.java`. The
/// exact expansion is the *better* number and the *wrong* answer: a caller who
/// asked `%.2f` of 1.005 and got 1.00 disagrees with every other Java runtime.
///
/// `format_double` is `Double.toString` (`cratonvm_types::java_double_to_string`),
/// which was measured byte-identical to HotSpot 25's on the values above — so
/// taking the digits from it is also what keeps `%s` and `%f` of the same value
/// telling the same story, which is precisely what the sentence above asks for.
fn fmt_shortest_decimal(v: f64) -> (Vec<u8>, i32) {
    fmt_decimal_digits(&format_double(v))
}

/// Round an exact digit string to `n` significant digits, HALF_UP — ties away
/// from zero, which is the rule `java.util.Formatter` names for %e/%f/%g.
///
/// Answers exactly `n` digits plus the exponent the caller must now use: 9.99
/// rounded to two digits carries into 10., i.e. gains a leading digit.
fn fmt_round_significant(digits: &[u8], exp: i32, n: usize) -> (Vec<u8>, i32) {
    let n = n.max(1);
    let mut out: Vec<u8> = digits.iter().copied().take(n).collect();
    out.resize(n, 0);
    // The discarded tail is >= half an ulp of the kept part exactly when its
    // first digit is >= 5 — including the "5 followed by nothing" tie, which is
    // the single case where half-even would round the other way.
    if digits.get(n).is_some_and(|&d| d >= 5) {
        let mut i = n;
        loop {
            if i == 0 {
                // Every kept digit was a 9: 999 -> 1000, one digit wider.
                out.insert(0, 1);
                out.pop();
                return (out, exp + 1);
            }
            i -= 1;
            if out[i] == 9 {
                out[i] = 0;
            } else {
                out[i] += 1;
                break;
            }
        }
    }
    (out, exp)
}

/// Round at a FRACTION-digit position rather than a significant-digit one —
/// what %f asks for, where the precision counts digits after the point.
fn fmt_round_at_fraction(digits: &[u8], exp: i32, frac: usize) -> (Vec<u8>, i32) {
    // The digit at 10^-frac is significant digit number `exp + 1 + frac`.
    let nsig = exp + 1 + frac as i32;
    if nsig >= 1 {
        fmt_round_significant(digits, exp, nsig as usize)
    } else if nsig == 0 {
        // Nothing survives but a possible carry: the leading digit IS the
        // rounding digit, so the answer is either zero or one unit in the last
        // place (0.06 at %.1f is 0.1).
        if digits.first().copied().unwrap_or(0) >= 5 {
            (vec![1], -(frac as i32))
        } else {
            (vec![0], 0)
        }
    } else {
        // |v| < 0.5 × 10^-frac: it rounds away entirely.
        (vec![0], 0)
    }
}

/// `digits × 10^exp` in plain decimal with exactly `frac` fraction digits.
/// Trailing zeros are WRITTEN, not trimmed — %f and %g both pad to the
/// precision.
fn fmt_render_fixed(digits: &[u8], exp: i32, frac: usize) -> String {
    let mut out = String::with_capacity(frac + 8);
    if exp < 0 {
        out.push('0');
    } else {
        for i in 0..=exp {
            out.push(char::from(
                b'0' + digits.get(i as usize).copied().unwrap_or(0),
            ));
        }
    }
    if frac > 0 {
        out.push('.');
        // Fraction digit j (1-based) sits at 10^-j, i.e. index `exp + j` into
        // `digits`; a negative index is one of the zeros after the point.
        for j in 1..=frac as i32 {
            let idx = exp + j;
            let d = if idx < 0 {
                0
            } else {
                digits.get(idx as usize).copied().unwrap_or(0)
            };
            out.push(char::from(b'0' + d));
        }
    }
    out
}

/// `digits × 10^exp` in Formatter's scientific form: one digit before the
/// point, `frac` after, then `e`/`E`, an ALWAYS-explicit sign, and at least two
/// exponent digits. Rust's `{:e}` writes neither the sign nor the padding,
/// which is the whole of the `1.2345e3` vs `1.234500e+03` divergence.
fn fmt_render_scientific(digits: &[u8], exp: i32, frac: usize, upper: bool) -> String {
    let mut out = String::with_capacity(frac + 8);
    out.push(char::from(b'0' + digits.first().copied().unwrap_or(0)));
    if frac > 0 {
        out.push('.');
        for j in 1..=frac {
            out.push(char::from(b'0' + digits.get(j).copied().unwrap_or(0)));
        }
    }
    out.push(if upper { 'E' } else { 'e' });
    out.push(if exp < 0 { '-' } else { '+' });
    out.push_str(&format!("{:02}", exp.unsigned_abs()));
    out
}

/// `Double.toHexString`'s digits for a finite, non-negative `v`, WITHOUT the
/// `0x` prefix that `Formatter` writes for itself: `1.0p0`, `0.0p0`,
/// `1.a36e2eb1c432dp-14`, or a subnormal's `0.<digits>p-1022`.
fn fmt_hex_digits(v: f64) -> String {
    if v == 0.0 {
        return "0.0p0".to_string();
    }
    let bits = v.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    // 52 significand bits are exactly 13 hex digits. `Double.toHexString` drops
    // trailing zeros but always keeps at least one digit ("0x1.0p0", not
    // "0x1.p0").
    let mut hex = format!("{frac:013x}");
    while hex.len() > 1 && hex.ends_with('0') {
        hex.pop();
    }
    if raw_exp == 0 {
        // Subnormal: no implicit leading 1, and the exponent is pinned.
        format!("0.{hex}p-1022")
    } else {
        format!("1.{hex}p{}", raw_exp - 1023)
    }
}

/// `java.util.Formatter.hexDouble` — %a's magnitude for a finite, non-negative
/// `v`, without the `0x` prefix.
///
/// `prec` is the JDK's already-normalised precision: 0 means "every digit"
/// (Formatter maps a MISSING precision to 0 and an explicit `%.0a` to 1), and
/// >= 13 is likewise every digit, because 13 hex digits is all a double has.
///
/// This is the one member of the family that is not HALF_UP. The JDK rounds
/// the SIGNIFICAND in binary, half to even — the round/sticky/least-significant
/// test below is `hexDouble`'s — and then re-renders the rounded double through
/// `Double.toHexString`. That is why `%.4a` of 1.0 is `0x1.0p0` and not
/// `0x1.0000p0`: nothing pads the digits back out afterwards.
fn fmt_hex_float(v: f64, prec: usize) -> String {
    if v == 0.0 || prec == 0 || prec >= 13 {
        return fmt_hex_digits(v);
    }
    // Subnormals carry no implicit leading 1, so normalise by 2^54 first and put
    // the exponent back afterwards — what `hexDouble` does for the same reason.
    let subnormal = (v.to_bits() >> 52) & 0x7ff == 0;
    let scaled = if subnormal { v * 2.0f64.powi(54) } else { v };

    // 1 implicit bit + 4 bits per hex digit kept, out of SIGNIFICAND_WIDTH = 53.
    // prec is 1..=12 here, so `shift` is 4..=48 and `shift - 1` is in range.
    let precision_bits = 1 + prec * 4;
    let shift = 53 - precision_bits as u32;
    let doppel = scaled.to_bits();
    // Exponent and significand together, sign masked off (v >= 0 anyway).
    let mut new_signif = (doppel & 0x7fff_ffff_ffff_ffff) >> shift;
    let rounding_bits = doppel & !(!0u64 << shift);
    let least_zero = new_signif & 1 == 0;
    let round = ((1u64 << (shift - 1)) & rounding_bits) != 0;
    let sticky = shift > 1 && (!(1u64 << (shift - 1)) & rounding_bits) != 0;
    if (least_zero && round && sticky) || (!least_zero && round) {
        new_signif += 1;
    }
    let rounded = f64::from_bits(new_signif << shift);
    if rounded.is_infinite() {
        // The carry ran out of the exponent field; `hexDouble` hard-codes this.
        return "1.0p1024".to_string();
    }
    let res = fmt_hex_digits(rounded);
    if !subnormal {
        return res;
    }
    // Undo the 2^54 normalisation in the printed exponent.
    match res.find('p') {
        Some(idx) => {
            let e = res[idx + 1..].parse::<i32>().unwrap_or(0) - 54;
            format!("{}p{e}", &res[..idx])
        }
        None => res,
    }
}

/// `Formatter.addZeros` over a `digits.digitsPexp` hex float: pad the
/// fractional hex digits out to `prec`, leaving the exponent alone.
///
/// `prec == 0` is the JDK's "all of the digits", where nothing is padded.
/// This applies above 13 too — `hexDouble` stops ROUNDING at 13 hex digits
/// but `addZeros` still pads, so `%.14a` of `Double.MIN_VALUE` is
/// `0x0.00000000000010p-1022`.
fn fmt_hex_pad(s: &str, prec: usize) -> String {
    if prec == 0 {
        return s.to_string();
    }
    let idx = match s.find('p') {
        Some(i) => i,
        None => return s.to_string(),
    };
    let (mant, exp) = s.split_at(idx);
    let have = match mant.find('.') {
        Some(dot) => mant.len() - dot - 1,
        // `fmt_hex_digits` always writes a point, but `addZeros` adds one when
        // it has to and this stays faithful to that.
        None => return format!("{mant}.{}{exp}", "0".repeat(prec)),
    };
    if have >= prec {
        return s.to_string();
    }
    format!("{mant}{}{exp}", "0".repeat(prec - have))
}

/// One value through `java.util.Formatter`'s floating-point conversions.
///
/// `precision` is the spec's precision if it carried one. The DEFAULTS live
/// here rather than at the call sites because the no-precision path used to
/// bypass precision handling entirely and answer Rust's `{:e}` /
/// `Double.toString`; a default that only exists on the with-precision branch
/// is not a default.
pub(crate) fn java_float_conversion(v: f64, spec: char, precision: Option<usize>) -> String {
    let upper = matches!(spec, 'E' | 'G' | 'A');
    // Formatter tests NaN before it reads the sign, so a NaN never prints one.
    if v.is_nan() {
        return if upper { "NAN" } else { "NaN" }.to_string();
    }
    // The sign test is `Double.compare(value, 0.0) == -1`, so -0.0 DOES print
    // its sign: `%f` of -0.0 is "-0.000000".
    let sign = if v.is_sign_negative() { "-" } else { "" };
    if v.is_infinite() {
        return format!("{sign}{}", if upper { "INFINITY" } else { "Infinity" });
    }
    let mag = v.abs();
    if matches!(spec, 'a' | 'A') {
        // Formatter emits the prefix itself and upper-cases the digits and
        // the 'p' for %A; the exponent is decimal either way.
        //
        // The JDK normalises a MISSING precision to 0 ("assume that we want all
        // of the digits") and an explicit `%.0a` to 1, then pads the mantissa
        // back out to it — `if (prec != 0) addZeros(va, prec)`, outside
        // `hexDouble`. W7-3 argued the opposite from the javadoc, that "nothing
        // pads the digits back out, and `%.4a` of 1.0 is `0x1.0p0`, not
        // `0x1.0000p0`". Measured on HotSpot 25 on 2026-08-12 it is
        // `0x1.0000p0`: a 1763-case sweep of the float family against HotSpot
        // failed on this row and nothing else.
        let prec = precision.map_or(0, |p| p.max(1));
        let digits = fmt_hex_pad(&fmt_hex_float(mag, prec), prec);
        let body = if upper {
            format!("0X{}", digits.to_uppercase())
        } else {
            format!("0x{digits}")
        };
        return format!("{sign}{body}");
    }
    let (digits, exp) = fmt_shortest_decimal(mag);
    format!(
        "{sign}{}",
        fmt_decimal_conversion(&digits, exp, spec, precision)
    )
}

/// A `java.math.BigDecimal`'s `toPlainString()` through %f/%e/%g.
///
/// Formatter formats a BigDecimal from its OWN digits — `print(BigDecimal,
/// Locale)` never converts it to a double — so `%.2f` of `new
/// BigDecimal("2.345")` is `2.35`, the HALF_UP rounding of the exact literal
/// 2.345, and not the 2.34 that the nearest double (2.34499999999999975…)
/// would give. `%a` has no BigDecimal arm at all in the JDK and raises
/// `IllegalFormatConversionException`; the caller rejects it before reaching
/// here.
///
/// The sign follows `signum()`, not the leading character: `new
/// BigDecimal("-0.00")` has signum 0 and prints without a sign.
fn java_decimal_conversion(plain: &str, spec: char, precision: Option<usize>) -> String {
    let (digits, exp) = fmt_decimal_digits(plain);
    let negative = plain.starts_with('-') && digits.iter().any(|&d| d != 0);
    let sign = if negative { "-" } else { "" };
    format!(
        "{sign}{}",
        fmt_decimal_conversion(&digits, exp, spec, precision)
    )
}

/// %f / %e / %g over an already-extracted non-negative magnitude
/// `digits × 10^exp`, which is the only part of the family that does not care
/// where the digits came from — a double's `Double.toString` or a
/// BigDecimal's `toPlainString`.
fn fmt_decimal_conversion(
    digits: &[u8],
    exp: i32,
    spec: char,
    precision: Option<usize>,
) -> String {
    let upper = matches!(spec, 'E' | 'G');
    match spec {
        'e' | 'E' => {
            let prec = precision.unwrap_or(6);
            // One digit before the point plus `prec` after it.
            let (digits, exp) = fmt_round_significant(digits, exp, prec + 1);
            fmt_render_scientific(&digits, exp, prec, upper)
        }
        'g' | 'G' => {
            // Javadoc: the precision defaults to 6, a precision of 0 is taken to
            // be 1, and it counts SIGNIFICANT digits rather than fraction ones.
            let prec = match precision {
                None => 6,
                Some(0) => 1,
                Some(p) => p,
            };
            if digits.iter().all(|&d| d == 0) {
                // Formatter special-cases zero to mantissa "0" with a rounded
                // exponent of 0, which lands in the decimal branch: `%g` of 0.0
                // is "0.00000", never "0.000000e+00".
                fmt_render_fixed(&[0], 0, prec - 1)
            } else {
                // The branch is decided by the magnitude AFTER rounding, which
                // is why 999999.5 at `%g` prints 1.00000e+06 and not 1000000.
                let (digits, exp) = fmt_round_significant(digits, exp, prec);
                if exp >= -4 && exp < prec as i32 {
                    fmt_render_fixed(&digits, exp, (prec as i32 - 1 - exp) as usize)
                } else {
                    fmt_render_scientific(&digits, exp, prec - 1, upper)
                }
            }
        }
        // 'f', and any other spec routed here by a caller that already decided
        // this argument is a float.
        _ => {
            let prec = precision.unwrap_or(6);
            let (digits, exp) = fmt_round_at_fraction(digits, exp, prec);
            fmt_render_fixed(&digits, exp, prec)
        }
    }
}

/// Format a single argument with flags, width, and precision support.
#[allow(clippy::too_many_arguments)]
fn format_arg_full(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
) -> Result<Vec<u16>, MethodCallFailed> {
    // Uppercase string-family conversions ('S'/'B'/'C'/'H') format identically
    // to their lowercase form, then the whole result is upper-cased — per
    // java.util.Formatter's "If the conversion is 'S', 'B' or 'C' … the result is
    // converted to upper case". (The numeric uppercase conversions 'X'/'E'/'G'/'A'
    // already emit upper-case digits in `format_arg`, so they are NOT remapped
    // here.) Format with the lowercase spec, then upper-case — BEFORE the width
    // justifier, which is where `print(Formatter, String, Locale)` does it and
    // which matters whenever the mapping changes the length; see that step.
    // Without 'S' support, Groovy's `"COMMERCIAL_%SREPO_URL".formatted(id)` left
    // the specifier literal, so the env-var key never matched (SpringRepos).
    //
    // 'H' was MISSING from this list, and the omission cost three separate
    // answers, because everything downstream keys off the remapped `spec`:
    // `%H` never upper-cased at all, `%H` of null was "null" where HotSpot 25
    // writes "NULL", and `%.2H` never truncated because the precision arm
    // below matches 's'/'b'/'h'. One line, three divergences — which is the
    // shape of a table populated by hand.
    // Whether the conversion AS WRITTEN was an upper-case one. Not the same
    // question as `uppercase_result` below, which asks only whether this
    // function has to case the result itself: `%X`/`%E`/`%G`/`%A` already emit
    // upper-case digits from `format_arg`, so they are not remapped — but they
    // ARE upper-case conversions, and the null path a few lines down is the
    // place where that distinction is load-bearing.
    let uppercase_conversion = spec.is_ascii_uppercase();
    let (spec, uppercase_result) = match spec {
        'S' => ('s', true),
        'B' => ('b', true),
        'C' => ('c', true),
        'H' => ('h', true),
        other => (other, false),
    };

    // `printString`'s FIRST line, ahead of both the '#' refusal and the null
    // test: "If the argument implements Formattable, then its formatTo method
    // is invoked" — and it, not this table, writes the output. The flags,
    // width and precision are handed over and nothing is applied on top: a
    // `Formattable` that writes nothing produces nothing even under `%10s`
    // (measured `"[%10s]"` -> `"[]"`), and one that throws propagates.
    if spec == 's' {
        if let Value::Object(Some(obj)) = val {
            if let Some(out) = fmt_formattable_dispatch(
                ctx,
                *obj,
                flags,
                width,
                precision,
                uppercase_conversion,
                locale,
            )? {
                // The callee's output VERBATIM, in code UNITS. Measured on
                // HotSpot 25: a `formatTo` that writes `"x\uD800y"` answers
                // `length() == 3` / `0078 D800 0079` under `%s`, `%S` AND
                // `%-10s`, so nothing is applied on top and nothing may be
                // lost on the way — which is why the read inside
                // `fmt_formattable_dispatch` is `read_string_chars`.
                return Ok(out);
            }
        }
        // `failMismatch(Flags.ALTERNATE, 's')`, in `printString`'s `else` —
        // so it fires for a non-`Formattable` argument INCLUDING a null one
        // (`%#s` of null is a mismatch on HotSpot 25, not "null"), and after
        // the argument fetch (`%#s` with no argument is
        // `MissingFormatArgumentException`). See `fmt_check_spec`, which used
        // to raise this and now deliberately does not.
        if flags.contains('#') {
            return Err(fmt_raise(ctx, &FmtFault::FlagsMismatch("#".to_string(), 's')));
        }
    }

    // A NULL argument converges on ONE printer for every conversion in the
    // table. `printInteger`, `printFloat`, `printCharacter`, `printString`,
    // `printHashCode` and `printDateTime` each open with
    //
    //     if (arg == null) { print(fmt, "null", l); return; }
    //
    // and `printBoolean` reaches the same `print(Formatter, String, Locale)`
    // with "false" instead. That printer does exactly three things — truncate
    // to the precision, upper-case when the conversion is an upper-case one,
    // and SPACE-justify to the width — so none of the numeric decoration below
    // this point applies to a null. Every one of the following was measured on
    // HotSpot 25 and every one of them was wrong here:
    //
    // * `%+d` / `% d` of null is "null", not "+null" / " null".
    // * `%#x` / `%#o` of null is "null", not "0xnull" / "0null".
    // * `%08d` / `%08x` / `%08e` of null is "    null" — padded with SPACES,
    //   because zero padding lives in the numeric printers this never enters.
    // * `%.2f` of null is "nu" and `%,(.2f` is "nu": the precision truncates a
    //   null argument even on the float conversions, whose own precision has
    //   nothing to do with a character count.
    // * `%X`, `%E`, `%G`, `%A` of null are "NULL", because the upper-casing is
    //   this shared printer's, not the digit rendering's.
    //
    // The flag LEGALITY checks are unaffected and have already run in
    // `fmt_check_spec`: `%08s` of null is still a FormatFlagsConversionMismatch
    // on HotSpot. The one exception is the '(' / '+' / ' ' refusal for %o/%x,
    // which is inside `print(long, Locale)` and so is skipped for a null — see
    // the note below it.
    if matches!(val, Value::Object(None)) {
        // `%b`/`%B` is the one conversion whose null is not the word "null" —
        // the rule lives in `fmt_null_text` so that `format_arg`'s own null arm
        // cannot drift from it.
        // `print(Formatter, String, Locale)`'s three steps, IN ITS ORDER:
        // truncate, then upper-case, then justify. The order is observable
        // because the upper-caser can GROW the text past the precision —
        // measured on HotSpot 25, `String.format(ROOT, "%.1S", "ß")` is `"SS"`,
        // length 2, not `"S"`. (It does not bite on the word "null", which is
        // why an ASCII-only row cannot tell the two orders apart.)
        let mut units: Vec<u16> = fmt_null_text(spec).encode_utf16().collect();
        if let Some(p) = precision {
            fmt_truncate_units(&mut units, p);
        }
        if uppercase_conversion {
            units = fmt_upper_case_units(ctx, &units, locale);
        }
        return fmt_pad_to_width(units, flags, width);
    }

    // The GENERAL family ('s', 'b', 'h') and the character conversion ('c')
    // are `print(Formatter, String, Locale)` and nothing else: truncate to the
    // precision, upper-case when the conversion was written upper-case, and
    // justify to the width. Splitting them out here is not a shortcut — every
    // step below this point is inapplicable to them by the JDK's own structure,
    // and each of the four exclusions is enforced somewhere already:
    //
    //   * grouping (','), '#', '(' , '+' and ' ' are refused for these
    //     conversions by `fmt_check_spec` before an argument is even fetched;
    //   * the zero-padding and localization arms below list only the numeric
    //     specs ('d','f','e','E','g','G','x','X','o','a','A'); and
    //   * the BigInteger sign handling is reached only from %o/%x/%X.
    //
    // What the split BUYS is that the three steps that do apply can be done in
    // UTF-16 code UNITS, and all three can produce a LONE SURROGATE that a Rust
    // `String` cannot hold:
    //
    //   * the truncation splits a surrogate pair — `%.1s` of U+1F600 is a lone
    //     high surrogate with `length() == 1` on HotSpot 25, so `[%5.1s]` is
    //     SEVEN characters; see `fmt_truncate_units` for the measured table;
    //   * the argument itself may contain one — `%s` of `"x\uD800y"` is three
    //     units on HotSpot, and was `x` + U+FFFD + `y` here; and
    //   * `%c` of a lone-surrogate code point is that surrogate — HotSpot
    //     answers `length() == 1`, `D800`, where this answered '?'.
    //
    // The order is the JDK's and is observable: the upper-caser runs AFTER the
    // truncation and may grow the result past the precision (`%.1S` of "ß" is
    // "SS"), and BEFORE the justifier, which is what makes `%5S` of "ß" five
    // units rather than six.
    if matches!(spec, 's' | 'b' | 'h' | 'c') {
        let mut units = fmt_general_units(ctx, val, spec)?;
        // `checkCharacter` refuses a precision on %c outright, so only the
        // 's'/'b'/'h' members can arrive here with one — the same set the
        // `String`-shaped arm this replaced matched on.
        if let Some(p) = precision {
            fmt_truncate_units(&mut units, p);
        }
        if uppercase_result {
            units = fmt_upper_case_units(ctx, &units, locale);
        }
        return fmt_pad_to_width(units, flags, width);
    }

    // Get the raw formatted value first
    let raw = format_arg(ctx, val, spec)?;

    // Apply the spec's own precision for the floating-point conversions,
    // overriding the default `format_arg` just applied. Every one of them goes
    // through the same `java_float_conversion` the no-precision path uses — the
    // arm this replaced sent %e to Rust's `{:.prec$e}` (bare exponent) and
    // %g/%a to `{:.prec$}` (fixed notation), which is W7-1's float row.
    let raw = match spec {
        // A null argument never reaches the conversion at all: Formatter's
        // `printFloat` prints "null" before it looks at the spec, and
        // `extract_float_value` would have answered 0.0. The null test is now
        // redundant — the shared-null-printer branch above returns first — and
        // is kept because it is the reason this arm is CORRECT, not merely the
        // mechanism: `float_source` of a null is `None`, which would fall
        // through to `raw` and silently drop the precision.
        'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A'
            if precision.is_some() && !matches!(val, Value::Object(None)) =>
        {
            // The precision is one character of output per unit, and it is
            // caller data with no upper bound but `Integer.MAX_VALUE` —
            // `%.2000000000f` is a legal specifier. See `fmt_reserve_probe`
            // for the HotSpot rows; this is the ONE point at which a precision
            // reaches `fmt_render_fixed`/`fmt_render_scientific`/`fmt_hex_pad`,
            // so asking here keeps all three non-fallible.
            fmt_reserve_probe(precision.unwrap_or(0))?;
            match float_source(ctx, val) {
                // `%.Nf` of a BigDecimal must round the BigDecimal's OWN
                // digits, which is why the source is asked for rather than
                // `extract_float_value`d into a double.
                Some(FloatSource::Decimal(plain)) => {
                    java_decimal_conversion(&plain, spec, precision)
                }
                Some(FloatSource::Double(v)) => java_float_conversion(v, spec, precision),
                None => raw,
            }
        }
        // "The precision is the maximum number of characters to be written to
        // the output" — for every GENERAL conversion, not just %s (`%.2b` of
        // true is "tr"). That arm moved UP, into the units-carrying general
        // branch above, and is `fmt_truncate_units` there; 's'/'b'/'h'/'c'
        // cannot reach this point any more.
        _ => raw,
    };

    // `java.math.BigInteger` is the one argument class whose %o/%x/%X goes
    // through a printer that renders a SIGN, so it is the one class for which
    // '(' / '+' / ' ' are legal on those conversions. See the refusal below.
    let big_integer = match val {
        Value::Object(Some(o)) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/math/BigInteger")
        }
        _ => false,
    };

    // `print(long, Locale)` opens its %o and %x arms with
    // `checkBadFlags(PARENTHESES | LEADING_SPACE | PLUS)` — INSIDE the printer,
    // not in `checkInteger`, and that placement is the entire behaviour:
    //
    // * `print(BigInteger, Locale)` has no such check and calls
    //   `leadingSign`/`trailingSign` like the decimal arm does, so `%(x` of
    //   `BigInteger("-255")` is "(ff)", `%+x` of `BigInteger("255")` is "+ff"
    //   and `% o` of `BigInteger("8")` is " 10" on HotSpot 25.
    // * a NULL argument never reaches either printer — `printInteger` writes
    //   "null" first — so `String.format("%(x", (Object) null)` is "null" and
    //   not a refusal. That case has already returned above.
    // * a wrong-typed argument fails `printInteger`'s dispatch first, so
    //   `%(x` of a String is IllegalFormatConversionException, not a flags
    //   mismatch — hence this standing AFTER `format_arg`.
    //
    // Hoisting it into `fmt_check_spec` — where it used to live, next to the
    // ',' refusal that really is argument-independent — refused all three of
    // those cases. The check has to stand HERE, after the argument's class is
    // known, because that is where the JDK put it.
    if matches!(spec, 'o' | 'x' | 'X') && !big_integer {
        let offending: String = fmt_flags_string(flags)
            .chars()
            .filter(|c| "( +".contains(*c))
            .collect();
        if !offending.is_empty() {
            return Err(fmt_raise(
                ctx,
                &FmtFault::FlagsMismatch(offending, spec.to_ascii_lowercase()),
            ));
        }
    }

    // Apply width and flags
    let left_justify = flags.contains('-');
    let zero_pad = flags.contains('0') && !left_justify;
    let numeric_sign = matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A')
        || (big_integer && matches!(spec, 'o' | 'x' | 'X'));

    let mut formatted = raw;

    // ',' grouping flag: insert a thousands separator into the integer part of
    // %d / %f values. The separator inserted here is the ASCII ',' whatever the
    // locale is; `fmt_localize` rewrites it at the very end, once every
    // length-sensitive step is done — see that function for why.
    if flags.contains(',') && matches!(spec, 'd' | 'f' | 'g' | 'G') {
        formatted = group_thousands(&formatted);
    }

    // `Formatter.leadingSign`/`trailingSign`. A negative value with the '('
    // flag is written in accountancy form — "the result will enclose negative
    // numbers in parentheses" — and the '-' disappears rather than being kept
    // alongside; a positive value takes '+' or, failing that, the ' ' flag's
    // leading space. `%x`/`%o` reject all three flags above unless the
    // argument is a BigInteger, whose printer applies them exactly as the
    // decimal one does — `%(x` of -255 is "(ff)", the two's-complement
    // rendering having been replaced by `abs().toString(16)` upstream.
    if numeric_sign {
        if let Some(magnitude) = formatted.strip_prefix('-') {
            if flags.contains('(') {
                formatted = format!("({magnitude})");
            }
        } else if flags.contains('+') {
            formatted = format!("+{formatted}");
        } else if flags.contains(' ') {
            formatted = format!(" {formatted}");
        }
    }

    // '#' alternate form. Only the radix conversions have one here: "the output
    // will always begin with the radix indicator '0x'" for %x (and '0X' for
    // %X), and "the output will always begin with a '0'" for %o. It goes on
    // BEFORE the zero padding, so `%#010x` of 255 is `0x000000ff` and not
    // `00000000xff` — hence the prefix-aware split below.
    //
    // The indicator goes INSIDE any leading sign, because the JDK appends it
    // to a StringBuilder that `leadingSign` has already written into: `%#x` of
    // `BigInteger("-255")` is "-0xff" and `%#(x` is "(0xff)". Inserting at
    // byte 0 unconditionally was invisible while %x/%o refused every sign
    // flag; admitting BigInteger above is what makes the position matter.
    if flags.contains('#') {
        let at = usize::from(formatted.starts_with(['-', '+', ' ', '(']));
        match spec {
            'x' => formatted.insert_str(at, "0x"),
            'X' => formatted.insert_str(at, "0X"),
            'o' => formatted.insert(at, '0'),
            _ => {}
        }
    }

    // `print(Formatter, String, Locale)` upper-cases BEFORE it justifies, and
    // the order is load-bearing whenever the mapping changes the LENGTH: a
    // full uppercase mapping can grow a string (U+00DF -> "SS"), and
    // `String.format(ROOT, "%5S", "\u{00DF}")` is `"   SS"` on HotSpot 25 —
    // five units, three spaces. Upper-casing after the pad, which is where
    // this step used to sit, produced SIX. Locale-sensitive since this lane;
    // see `fmt_upper_case`.
    //
    // Only the general family reaches this: 'X'/'E'/'G'/'A' are not remapped
    // (they emit upper-case digits from `format_arg`), so `uppercase_result`
    // is true only for the remapped 'S'/'B'/'C'/'H', none of which takes the
    // grouping, sign, '#' or localization steps above.
    //
    // ...which is exactly why the step now lives in the units-carrying general
    // branch near the top of this function and NOT here: 'S'/'B'/'C'/'H' are
    // the complete set that reached it, and all four return before this point.
    // `uppercase_result` is therefore false for everything that gets this far,
    // and the guard was kept only as a place for a second copy of the rule to
    // grow. The ordering constraint it documents is unchanged and is asserted
    // in the branch above; the `debug_assert` is what keeps a future
    // conversion from being added to the remap table without its upper-caser.
    debug_assert!(
        !uppercase_result,
        "an upper-case-remapped conversion reached the numeric decoration path"
    );

    // `trailingZeros` — the numeric printers' own zero padding, which the JDK
    // does INSIDE the printer, before `appendJustified` ever sees the buffer.
    // Once it has run the buffer is already `width` units long, so the shared
    // justifier below pads nothing; that is why the two are not one step.
    if let Some(w) = width {
        let len = fmt_utf16_len(&formatted);
        // %g/%G were missing from the zero-pad set even though Formatter
        // accepts '0' for them. %a/%A are IN it since 2026-08-12: their
        // zeros go after the "0x" prefix, which the prefix-aware split
        // below now does — `%020a` of 1.0 is `0x00000000000001.0p0`, and
        // with a sign the zeros go after BOTH (`%+020a` is
        // `+0x0000000000001.0p0`). Before this they fell through to the
        // justifier and got leading spaces. W7-3 left this as the family's
        // last flag defect; it needed the `lead` split W7-34 added for
        // `%#010x`, which is why the two waves could not close it together.
        //
        // The non-finite test stands in for Formatter's structure, where
        // zero padding happens only inside the FINITE branch —
        // "Infinity"/"NaN" reach the width justifier and get spaces. It
        // used to be "ends with an ASCII digit", which mistook two finite
        // renderings for infinities the moment this lane gave them
        // non-digit tails: `%#010x` ends in a HEX digit and `%(08d` ends in
        // the closing parenthesis, and both silently reverted to space
        // padding.
        if len < w
            && zero_pad
            && matches!(
                spec,
                'd' | 'f' | 'e' | 'E' | 'g' | 'G' | 'x' | 'X' | 'o' | 'a' | 'A'
            )
            && !formatted.ends_with("Infinity")
            && !formatted.ends_with("INFINITY")
            && !formatted.ends_with("NaN")
            && !formatted.ends_with("NAN")
        {
            // The zeros go INSIDE whatever the value already leads with —
            // a sign, an opening parenthesis, or a radix indicator — never
            // in front of it. The two stack: `%a` puts the sign OUTSIDE the
            // `0x` prefix (`+0x1.0p0`), so a signed hex float leads with
            // three characters, and testing the radix indicator only at
            // byte 0 would have put the zeros in front of the `0x`.
            let mut lead = 0usize;
            if formatted.starts_with(['-', '+', ' ', '(']) {
                lead = 1;
            }
            if formatted[lead..].starts_with("0x") || formatted[lead..].starts_with("0X") {
                lead += 2;
            }
            let zeros = fmt_repeat('0', w - len)?;
            formatted.insert_str(lead, &zeros);
        }
    }

    // Localization comes after the zero padding so that the padding zeros are
    // localized too — `trailingZeros` appends the locale's OWN zero digit, and
    // `fmt_localize` is what supplies it here. Every width decision above was
    // still made on ASCII, and "no localization is applied" to %x, %o and %a,
    // nor to a %s argument's digits, which are the caller's text.
    if matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') {
        formatted = fmt_localize(&formatted, sym);
    }

    // `appendJustified`, shared with `%t`/`%T` and the null path. Spaces are
    // not digits, so standing after `fmt_localize` rather than before it is
    // the same answer.
    //
    // The `_str_` adapter, not a second justifier: everything that reaches here
    // is a NUMERIC conversion whose rendering is ASCII digits, signs, a radix
    // indicator and the locale's own separator characters — none of which can
    // be an unpaired surrogate, so the `str` shape is not lossy on this path.
    fmt_pad_str_to_width(&formatted, flags, width)
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

/// Where a float conversion's digits come from.
///
/// `java.util.Formatter.print(Object, Locale)` dispatches the float family to
/// two different printers — `print(double, …)` and `print(BigDecimal, …)` —
/// and they are not the same algorithm. Collapsing a BigDecimal into a double
/// first would round twice and lose the very digits the caller chose a
/// BigDecimal to keep.
enum FloatSource {
    Double(f64),
    /// A `java.math.BigDecimal`, as its `toPlainString()`.
    Decimal(String),
}

/// Classify a `%f`/`%e`/`%g` argument, unboxing wrappers as needed.
///
/// `None` means the argument is not one of the types the float conversions
/// accept — the caller raises `IllegalFormatConversionException` rather than
/// inventing a value for it.
fn float_source(ctx: &mut dyn NativeContext, val: &Value) -> Option<FloatSource> {
    match val {
        Value::Float(v) => Some(FloatSource::Double(*v as f64)),
        Value::Double(v) => Some(FloatSource::Double(*v)),
        Value::Int(v) => Some(FloatSource::Double(*v as f64)),
        Value::Long(v) => Some(FloatSource::Double(*v as f64)),
        Value::Object(Some(obj)) => {
            let cname = ctx
                .class_name_of_id(ctx.class_id_of_object(*obj))
                .unwrap_or_default();
            if cname == "java/math/BigDecimal" {
                // `toPlainString`, not `toString`: the latter switches to
                // scientific notation for some scales, and `fmt_decimal_digits`
                // would then have to trust an exponent this path can avoid
                // producing at all.
                return match ctx.invoke_virtual(*obj, "toPlainString", "()Ljava/lang/String;", &[])
                {
                    Ok(Some(Value::Object(Some(s)))) => {
                        ctx.read_string(s).map(FloatSource::Decimal)
                    }
                    _ => None,
                };
            }
            if !matches!(cname.as_str(), "java/lang/Float" | "java/lang/Double") {
                return None;
            }
            match ctx.get_field(*obj, 0) {
                Value::Float(v) => Some(FloatSource::Double(v as f64)),
                Value::Double(v) => Some(FloatSource::Double(v)),
                Value::Int(v) => Some(FloatSource::Double(v as f64)),
                Value::Long(v) => Some(FloatSource::Double(v as f64)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The raw rendering of a GENERAL-family argument ('s', 'b', 'h') or of the
/// character conversion ('c'), as UTF-16 code **units**.
///
/// Everything here is [`format_arg`]'s answer; the two departures are the two
/// places `format_arg`'s `String` return type destroys a code unit.
///
/// **`%s` of a `java/lang/String`.** `String.valueOf(arg)` on a String is the
/// argument's own characters, and `read_string_chars` hands them over without
/// the UTF-8 round trip that turns each unpaired surrogate into U+FFFD.
/// Measured on HotSpot 25: `String.format(ROOT, "%s", "x\uD800y")` has
/// `length() == 3` and units `0078 D800 0079`.
///
/// The class test is the SAME one `format_arg`'s own `%s` arm makes (`is_string`
/// there) rather than a second copy of it: this function runs first and returns,
/// so on a real String that arm is no longer reached, and on everything else
/// this falls through to it. Note it is a class-IDENTITY test, not
/// `is_subclass` — `java.lang.String` is final, and a subclass test would be
/// both wider and slower.
///
/// Taking this route also means a `java/lang/String` argument never reaches
/// `fmt_formattable_dispatch`'s interface probe — F13's suggested `%s` fast
/// path, obtained by hoisting the comparison that already existed further down
/// this file rather than adding a second one. **The speedup is UNMEASURED**;
/// the change was made because it is the lossless path anyway, and it is
/// behaviour-identical because `java.lang.String` does not implement
/// `java.util.Formattable` and is final, so the probe it skips could only ever
/// have answered "no".
///
/// **`%c` of a lone-surrogate code point.** `Character.isValidCodePoint`
/// admits U+D800..U+DFFF and `Character.toChars` hands one back as a single
/// unpaired `char`; `char::from_u32` refuses it, so the arm in [`format_arg`]
/// substituted '?'. Measured on HotSpot 25:
/// `String.format(ROOT, "%c", (int) 0xD800)` has `length() == 1` and unit
/// `D800`. [`format_arg`] still owns the VALIDATION — it is called first and
/// its `IllegalFormatCodePointException` / `IllegalFormatConversionException`
/// refusals propagate unchanged — and this only RE-RENDERS an argument it has
/// already accepted, so the two cannot drift on which code points are legal.
fn fmt_general_units(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
) -> Result<Vec<u16>, MethodCallFailed> {
    if spec == 's' {
        if let Value::Object(Some(obj)) = val {
            if ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(*obj)) {
                return Ok(read_string_chars(ctx, *obj));
            }
            // Every OTHER object: `%s` is `String.valueOf(arg)` ==
            // `arg.toString()`, and that result can hold an unpaired surrogate
            // exactly as a String argument can -- `String.format("%s", x)` where
            // `x.toString()` returns one, and any boxed `Character` holding one.
            // `format_arg` renders it and hands back a Rust `String`, so the
            // unit was already gone by the time this function saw it.
            //
            // These are the SAME two calls `format_arg`'s own `%s` arm makes,
            // in the same order, differing only in reading the result as units
            // -- so the two cannot disagree about anything except the loss.
            return match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => Ok(ctx
                    .read_string_units(s)
                    .unwrap_or_else(|| "null".encode_utf16().collect())),
                Ok(_) => Ok("null".encode_utf16().collect()),
                Err(err) => Err(err),
            };
        }
    }
    let rendered = format_arg(ctx, val, spec)?;
    if spec == 'c' {
        // The range test is a DISAGREEMENT guard, not a second validation:
        // `format_arg` has already refused anything outside it, so this can
        // only fire if the two ever stopped unboxing the same argument the same
        // way — and then the right answer is `format_arg`'s rendering, not a
        // unit fabricated from a value it never saw.
        if let Some(cp) = fmt_char_code_point(ctx, val).filter(|cp| (0..=0x10FFFF).contains(cp)) {
            // `Character.toChars`: one unit for a BMP code point (INCLUDING a
            // lone surrogate), a high+low pair for a supplementary one.
            let cp = cp as u32;
            return Ok(if cp > 0xFFFF {
                let v = cp - 0x10000;
                vec![(0xD800 + (v >> 10)) as u16, (0xDC00 + (v & 0x3FF)) as u16]
            } else {
                vec![cp as u16]
            });
        }
    }
    Ok(rendered.encode_utf16().collect())
}

/// The code point a `%c` argument denotes, once [`format_arg`] has accepted it.
///
/// `None` means "not one of the shapes `%c` takes", which after `format_arg`
/// has returned `Ok` cannot happen — the caller falls back to `format_arg`'s
/// own rendering rather than inventing a character. Deliberately does NOT
/// re-check `isValidCodePoint`: that refusal has exactly one home, in
/// [`format_arg`]'s `'c'` arm, and a second copy here would be a second
/// chance to disagree with it.
///
/// The wrapper set is `printCharacter`'s: `Character`, `Byte`, `Short`,
/// `Integer`. `Byte` and `Short` are NOT masked the way the unsigned integer
/// conversions mask them — `printCharacter` widens them as signed and lets
/// `isValidCodePoint` reject a negative, which is why this reads the slot
/// straight through.
fn fmt_char_code_point(ctx: &mut dyn NativeContext, val: &Value) -> Option<i32> {
    match val {
        Value::Int(v) => Some(*v),
        Value::Object(Some(obj)) => {
            let cname = ctx.class_name_of_id(ctx.class_id_of_object(*obj))?;
            if !matches!(
                cname.as_str(),
                "java/lang/Character" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Integer"
            ) {
                return None;
            }
            match ctx.get_field(*obj, 0) {
                Value::Int(v) => Some(v),
                _ => None,
            }
        }
        _ => None,
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
        // W8-F4-1 N4. Dead on the `String.format` path — `format_arg_full`'s
        // shared null printer returns before this is reached — and ROUTED
        // rather than deleted: the word is now [`fmt_null_text`]'s, the same
        // table that printer reads, so a future caller that makes `format_arg`
        // an entry point again gets the JDK's answer for WHICH word instead of
        // a second copy of the rule that can drift. What this arm still does
        // not do is the decoration (precision, upper-casing, justification);
        // `format_arg` does not decorate anything, for a null or otherwise.
        Value::Object(None) => fmt_null_text(spec).to_string(),
        Value::Object(Some(obj)) => {
            // `printBoolean`, which reads only the CLASS: "If the argument arg
            // is null, then the result is 'false'. If arg is a boolean or
            // Boolean, then the result is the string returned by
            // String.valueOf(arg). Otherwise, the result is 'true'."
            //
            // `unbox_obj` collapses every INTEGRAL wrapper to `Value::Int`, so
            // testing the unboxed shape alone made a zero-valued Integer,
            // Short, Byte or Character answer "false" — `String.format("%b",
            // 0)` was "false" where HotSpot 25 says "true", because an Integer
            // of zero is a non-null object and nothing else. The class is the
            // question, so ask it before unboxing.
            if spec == 'b' {
                let is_boolean = ctx
                    .class_name_of_id(ctx.class_id_of_object(*obj))
                    .is_some_and(|n| n == "java/lang/Boolean");
                if !is_boolean {
                    return Ok("true".to_string());
                }
                return Ok(match unbox_obj(ctx, *obj) {
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
            // `printHashCode`, whose whole body is
            //   String s = (arg == null ? "null"
            //                           : Integer.toHexString(arg.hashCode()));
            // so `%h` is the hex of the argument's VIRTUAL hashCode and has
            // nothing to do with the argument's text.
            //
            // This arm used to answer `read_string(arg)`, which made `%h` an
            // alias for `%s` on a String — `String.format("%h", "a")` was "a"
            // where HotSpot 25 says "61" — and "null" on every argument that
            // is not a String, since `read_string` has nothing to read off an
            // Integer, an enum or a bean. Both halves were wrong and the
            // second one is the quieter: a `%h` of a bean printed "null" with
            // no refusal anywhere.
            //
            // `Integer.toHexString` is UNSIGNED, so the `as u32` is the whole
            // of the negative-hash case: `%h` of an object hashing to -1 is
            // "ffffffff", not "-1". `%H` upper-cases the result, and that is
            // done once for the whole general family in `format_arg_full`
            // rather than here.
            if spec == 'h' || spec == 'H' {
                let hash = match ctx.invoke_virtual(*obj, "hashCode", "()I", &[])? {
                    Some(Value::Int(h)) => h,
                    // The descriptor is `()I`, so bytecode cannot reach this;
                    // answering 0 keeps a broken interpreter a wrong answer
                    // rather than a panic.
                    _ => 0,
                };
                return Ok(format!("{:x}", hash as u32));
            }

            // Everything past here is a TYPED conversion, and
            // `java.util.Formatter.print(Object, Locale)` reaches its
            // `failConversion` default for an argument whose class the
            // conversion does not name: "If the argument arg is ... not
            // otherwise applicable to this conversion, then an
            // IllegalFormatConversionException will be thrown."
            //
            // Answering something anyway is how `String.format("%d",
            // "notANumber")` became a no-throw and `%.2f` of a BigDecimal became
            // `0.00` — `unbox_obj` fell through to a slot-0 read that meant
            // nothing on either class. The refusal has to carry the argument's
            // Class, because that is half of the message a caller reads.
            {
                let class_id = ctx.class_id_of_object(*obj);
                let cname = ctx.class_name_of_id(class_id).unwrap_or_default();
                let applicable = match spec {
                    'd' | 'o' | 'x' | 'X' => matches!(
                        cname.as_str(),
                        "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Integer"
                            | "java/lang/Long"
                            | "java/math/BigInteger"
                    ),
                    // %a has no BigDecimal printer in the JDK at all, which is
                    // why it is the one float conversion that refuses one.
                    'f' | 'e' | 'E' | 'g' | 'G' => matches!(
                        cname.as_str(),
                        "java/lang/Float" | "java/lang/Double" | "java/math/BigDecimal"
                    ),
                    'a' | 'A' => matches!(cname.as_str(), "java/lang/Float" | "java/lang/Double"),
                    'c' => matches!(
                        cname.as_str(),
                        "java/lang/Character"
                            | "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Integer"
                    ),
                    _ => true,
                };
                if !applicable {
                    // DIAGNOSTIC (`CRATONVM_DBG_FMT_WRONGTYPE`): this is the
                    // exact site that produces
                    // `IllegalFormatConversionException: d != java.lang.Object`.
                    // The message names only the class, which cannot separate a
                    // reclaimed cell (all-zero header), a stale reference to a
                    // moved object (forwarded header), and an argument that
                    // really is of the wrong class. Dump the header so the run
                    // says which.
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FMT_WRONGTYPE")
                        .is_some()
                    {
                        let a = obj.as_ptr() as usize;
                        // SAFETY: diagnostic-only aligned read of the header
                        // words of an argument object the formatter was just
                        // handed, so the address is mapped.
                        let w = unsafe { std::slice::from_raw_parts(a as *const u64, 4) };
                        eprintln!(
                            "[fmt-wrongtype] spec={spec} obj=0x{a:x} cid={} cname={cname:?} \
                             kind={:?} w0=0x{:x} w1=0x{:x} w2=0x{:x} w3=0x{:x}",
                            class_id.as_u32(),
                            ctx.heap_kind_of(*obj),
                            w[0],
                            w[1],
                            w[2],
                            w[3],
                        );
                    }
                    // The conversion character the exception carries is the
                    // LOWER-case one. `FormatSpecifier.conversion(char)` folds
                    // every upper-case conversion down —
                    // `if (Character.isUpperCase(conv)) { f.add(Flags.UPPERCASE);
                    // this.c = Character.toLowerCase(conv); }` — and `c` is
                    // what `failConversion` hands
                    // `IllegalFormatConversionException`. `format_arg_full`
                    // remaps only `S`/`B`/`C`/`H` (the four whose RENDERING
                    // changes), so `X`/`E`/`G`/`A` arrive here as written and
                    // this is the one place the fold has to be repeated.
                    // Measured on HotSpot 25.0.3+9, `Boolean.TRUE` as the
                    // argument, message and `getConversion()` both:
                    //
                    //   %X -> "x != java.lang.Boolean", conv='x'
                    //   %E -> "e != java.lang.Boolean", conv='e'
                    //   %G -> "g != java.lang.Boolean", conv='g'
                    //   %A -> "a != java.lang.Boolean", conv='a'
                    //
                    // `%tY` is NOT folded — `printDateTime` reports the FIELD
                    // character as typed ("Y != java.lang.String", measured) —
                    // which is why this is done here and not in `fmt_raise`.
                    return Err(fmt_raise(
                        ctx,
                        &FmtFault::WrongType(spec.to_ascii_lowercase(), class_id),
                    ));
                }
                // A BigDecimal at its DEFAULT precision still has to come from
                // its own digits — `%f` of `new BigDecimal("2.3")` is
                // "2.300000", six fraction digits of the decimal literal, not of
                // a double it was never turned into.
                if cname == "java/math/BigDecimal" {
                    if let Some(FloatSource::Decimal(plain)) = float_source(ctx, val) {
                        return Ok(java_decimal_conversion(&plain, spec, None));
                    }
                }
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
        // The float family is routed through `java_float_conversion` with NO
        // precision, so its Formatter defaults (6 for %f/%e/%g, every digit for
        // %a) apply here. This is the path a bare `%e` takes, and it used to
        // answer Rust's `{:e}` — `1.2345e3` where HotSpot writes
        // `1.234500e+03`. A Float argument widens to double first, as
        // `Formatter.print(float, Locale)` does.
        Value::Int(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u32),
            'X' => format!("{:X}", *v as u32),
            'o' => format!("{:o}", *v as u32),
            // `printCharacter` gates every non-`Character` argument on
            // `Character.isValidCodePoint` and refuses the rest with
            // `IllegalFormatCodePointException` — `String.format("%c",
            // 0x110000)` is a THROW, not the '?' this answered. A `Character`
            // argument is never checked by the JDK and never needs to be: it
            // unboxes into 0..=0xFFFF, which is always valid.
            'c' => {
                if !(0..=0x10FFFF).contains(v) {
                    return Err(fmt_raise(ctx, &FmtFault::IllegalCodePoint(*v)));
                }
                // A lone surrogate (0xD800..=0xDFFF) IS a valid code point by
                // `Character.isValidCodePoint`, and `Character.toChars` hands
                // it back as a single unpaired `char`. Rust's `char` cannot
                // hold one and `create_string` takes a `&str`, so the '?' is
                // kept for exactly that range — a residual of the UTF-8 string
                // representation, not of this check. See
                // W7-41-format-exception-subclasses.md.
                char::from_u32(*v as u32).unwrap_or('?').to_string()
            }
            'b' => ((*v) != 0).to_string(),
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                java_float_conversion(*v as f64, spec, None)
            }
            _ => v.to_string(),
        },
        Value::Long(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u64),
            'X' => format!("{:X}", *v as u64),
            'o' => format!("{:o}", *v as u64),
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                java_float_conversion(*v as f64, spec, None)
            }
            _ => v.to_string(),
        },
        Value::Float(v) => match spec {
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                java_float_conversion(*v as f64, spec, None)
            }
            // `%s`/no-spec of a float -> Java Double.toString form, not raw `{}`.
            _ => format_float(*v),
        },
        Value::Double(v) => match spec {
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => java_float_conversion(*v, spec, None),
            // `%s`/no-spec of a double -> Java Double.toString form, not raw `{}`.
            _ => format_double(*v),
        },
        _ => "?".to_string(),
    };
    Ok(formatted)
}

// ---------------------------------------------------------------------------
// Java's character tables, for the `String` methods that are specified in
// terms of them.
//
// W7-95a. Every one of the three predicates below exists because the Rust
// standard-library method with the plausibly-matching name implements the
// UNICODE definition and Java's is deliberately different. Reaching for
// `char::is_whitespace` / `char::to_uppercase` here is the single root cause
// behind `String.isBlank` and `String.regionMatches(true, ...)` disagreeing
// with HotSpot, and it is the same reflex W7-95 measured across the whole
// `java/lang/Character` family.
// ---------------------------------------------------------------------------

/// `Character.isWhitespace` for one UTF-16 code unit — the JAVADOC rule, not
/// `char::is_whitespace`.
///
/// * Rust's `char::is_whitespace` is the Unicode **White_Space** property:
///   `Zs ∪ Zl ∪ Zp ∪ {U+0009..U+000D, U+0085}`.
/// * Java's `isWhitespace` is "a Unicode space character (`Zs`/`Zl`/`Zp`) that
///   is **not** a non-breaking space (`U+00A0`, `U+2007`, `U+202F`), **or** one
///   of `U+0009..U+000D`, `U+001C..U+001F`".
///
/// Eight code points disagree. Measured on Microsoft OpenJDK 25.0.3+9 —
/// written by NAME here, never as the character itself, because a doc comment
/// holding a literal U+00A0 is one re-encode away from holding a space:
/// `isBlank()` of a lone `U+00A0` NBSP, `U+0085` NEL, `U+2007` FIGURE SPACE
/// or `U+202F` NARROW NBSP is `false` there and was `true` here; of
/// `U+001C`..`U+001F` (file/group/record/unit separator) it is `true` there
/// and was `false` here.
///
/// A surrogate code unit is `Cs`, never whitespace, and no arm below admits
/// one — which is what HotSpot answers.
///
/// **This is an ENUMERATION, deliberately, and not the derivation it replaced.**
/// The first version computed `Zs | Zl | Zp` as
/// `char::is_whitespace(cp) && !matches!(cp, 0x09..=0x0D | 0x85)` and subtracted
/// the three non-breaking spaces. That was correct — verified over all
/// 1,114,112 code points against `Character.isWhitespace` on OpenJDK 25.0.3+9,
/// zero mismatches, and agreeing with the enumeration below at every one of
/// them — but it was correct *by consulting a Rust Unicode table at runtime*,
/// and that is the dependency this file has already been bitten by once: see
/// `case_map::JDK_UNMAPPED_CASE_CODE_POINTS`, where Rust's tables being NEWER
/// than the JDK's produced a wrong answer that no amount of care about the Java
/// side could have caught. Whitespace is a small, stable, enumerable set, so the
/// version-skew hazard is removed rather than managed.
///
/// The enumeration itself was swept exhaustively: `WsSweep`, all 1,114,112
/// code points, `proposed mismatches=0`.
///
/// **Twin.** `native_character_is_whitespace` in
/// `native-builtins/src/lang_math.rs` is the same rule for the
/// `java/lang/Character` triples (W7-98a). That lane independently replaced its
/// derivation with this same enumeration; the two now agree character for
/// character AND line for line, which is the point — two enumerations of the
/// same constants cannot silently drift the way two derivations over a moving
/// table can. They should still be hoisted onto one predicate; see N5.
fn java_char_is_whitespace(ch: u16) -> bool {
    matches!(u32::from(ch),
        0x0009..=0x000D      // TAB, LF, VT, FF, CR
        | 0x001C..=0x0020    // FILE/GROUP/RECORD/UNIT SEPARATOR, SPACE
        | 0x1680             // OGHAM SPACE MARK
        | 0x2000..=0x2006    // EN QUAD .. SIX-PER-EM SPACE  (2007 FIGURE SPACE excluded)
        | 0x2008..=0x200A    // PUNCTUATION SPACE .. HAIR SPACE
        | 0x2028..=0x2029    // LINE SEPARATOR, PARAGRAPH SEPARATOR
        | 0x205F             // MEDIUM MATHEMATICAL SPACE
        | 0x3000             // IDEOGRAPHIC SPACE
    )
    // Absent on purpose, each measured `false` on HotSpot: 0x00A0 NBSP,
    // 0x2007 FIGURE SPACE, 0x202F NARROW NBSP (non-breaking spaces are
    // excluded by the javadoc), and 0x0085 NEL.
}

// The JDK-vs-Rust Unicode version skew — `JDK_UNMAPPED_CASE_CODE_POINTS` and
// its predicate — used to be declared HERE, and `case_map.rs` had no arm for it
// at all. Those two files are the only case-mapping code in the crate and they
// split the work by locale: this one handles the root and ordinary locales,
// `case_map.rs` handles `tr`/`az`/`lt`. A constant that lives in one half is a
// fix that half the locales never get, which is exactly what happened —
// `"ꟓ".toUpperCase(Locale.ROOT)` was right and
// `"ꟓ".toUpperCase(Locale.forLanguageTag("tr"))` was wrong. The definition now
// lives next to the table it corrects; see
// [`crate::case_map::JDK_UNMAPPED_CASE_CODE_POINTS`] for the measurement and
// for why a THIRD copy was not the answer.
use crate::case_map::is_jdk_unmapped_case_code_point;

/// [`java_char_is_whitespace`] for a Rust `char` rather than a code unit.
///
/// For callers that already hold a `&str` (`indent`). A supplementary code
/// point is never whitespace in Java, so the `> 0xFFFF` arm is `false` rather
/// than a truncation.
fn char_is_java_whitespace(c: char) -> bool {
    let cp = u32::from(c);
    cp <= 0xFFFF && java_char_is_whitespace(cp as u16)
}

/// `Character.toUpperCase(char)` — Java's **1:1** mapping, not the full one.
///
/// `char::to_uppercase()` yields the FULL (SpecialCasing) mapping, which can
/// be several characters: `ß` → `"SS"`, `ﬀ` → `"FF"`. Java's `char`-taking
/// overload is UnicodeData's *simple* mapping and returns the input unchanged
/// whenever the full mapping does not fit in one `char`. Taking `.next()` off
/// the iterator — the shape W7-95 measured in `native_character_to_upper_case`
/// — turns `ß` into `S`.
///
/// The exception arms are the complete set for the BMP, derived rather than
/// sampled: `Character.toUpperCase((char) c)` was dumped for all 65 536 code
/// units on OpenJDK 25.0.3+9 and diffed against "full mapping, multi-char
/// falls back to identity". Exactly 27 code units differ, all of them Greek
/// vowels with ypogegrammeni whose simple uppercase is the TITLECASE
/// character (`U+1F80` → `U+1F88`) while the full mapping is a two-character
/// expansion. Everywhere else the two rules agree, which is why this is a
/// small `match` and not a ported table.
///
/// A surrogate code unit has no scalar value; Java maps every unmapped char to
/// *itself*, so this returns the input rather than `char::from_u32`'s `None`.
fn java_char_to_upper_case(ch: u16) -> u16 {
    // Rust's tables are newer than the JDK's and pair these; the JDK maps each
    // to itself. See `JDK_UNMAPPED_CASE_CODE_POINTS`.
    if is_jdk_unmapped_case_code_point(u32::from(ch)) {
        return ch;
    }
    match ch {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => return ch + 8,
        0x1FB3 => return 0x1FBC,
        0x1FC3 => return 0x1FCC,
        0x1FF3 => return 0x1FFC,
        _ => {}
    }
    let Some(c) = char::from_u32(u32::from(ch)) else {
        return ch;
    };
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) if u32::from(u) <= 0xFFFF => u as u16,
        _ => ch,
    }
}

/// `Character.toLowerCase(char)` — Java's **1:1** mapping. See
/// [`java_char_to_upper_case`] for the derivation; the same BMP-wide dump
/// finds exactly ONE code unit where the full lowercase mapping differs from
/// Java's simple one: `U+0130` LATIN CAPITAL LETTER I WITH DOT ABOVE, whose
/// SpecialCasing full mapping is the two characters `i` + `U+0307` and whose
/// simple mapping is plain `i`. That one code point is the difference between
/// `"İ".regionMatches(true, 0, "i", 0, 1)` answering `true` (HotSpot) and
/// `false`.
fn java_char_to_lower_case(ch: u16) -> u16 {
    // See the twin above and `JDK_UNMAPPED_CASE_CODE_POINTS`.
    if is_jdk_unmapped_case_code_point(u32::from(ch)) {
        return ch;
    }
    if ch == 0x0130 {
        return 0x0069;
    }
    let Some(c) = char::from_u32(u32::from(ch)) else {
        return ch;
    };
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) if u32::from(l) <= 0xFFFF => l as u16,
        _ => ch,
    }
}

// ---------------------------------------------------------------------------
// String modern methods (Java 11+)
// ---------------------------------------------------------------------------

/// `String.repeat(int)` — "ab".repeat(3) → "ababab".
///
/// W7-95a. The old body was `std::cmp::max(0, n) as usize`, which turned every
/// contract violation into a silent empty string, and then handed the count
/// straight to `str::repeat`, which has no upper bound at all.
///
/// Measured on OpenJDK 25.0.3+9, in the JDK's own check order:
///
/// ```text
/// "ab".repeat(-1)                  IllegalArgumentException: count is negative: -1
/// "ab".repeat(Integer.MIN_VALUE)   IllegalArgumentException: count is negative: -2147483648
/// "".repeat(-1)                    IllegalArgumentException          (count is checked FIRST,
///                                                                    before the empty-string
///                                                                    short circuit)
/// "ab".repeat(0)                   ""
/// "ab".repeat(Integer.MAX_VALUE)   OutOfMemoryError: Required length exceeds implementation limit
/// "abc".repeat(1000000000)         OutOfMemoryError: Required length exceeds implementation limit
/// ```
///
/// The overflow guard is the JDK's `Integer.MAX_VALUE / count < len`, and
/// `len` there is the length of the compact `value` **byte** array — one byte
/// per char for a LATIN-1 string, two for a UTF-16 one — which is why the
/// coder is reconstructed below instead of using the char count. Without it
/// `str::repeat` attempts a multi-gigabyte allocation and aborts the process:
/// a capacity overflow is a Rust panic, and a Rust panic is not a Java
/// throwable. `try_reserve_exact` covers the residual case where the
/// Java-legal length is still more memory than this process can get.
pub(crate) fn native_string_repeat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match args.get(1) {
        Some(Value::Int(n)) => *n,
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
    let units = read_string_chars(ctx, this);
    if units.is_empty() || count == 0 {
        return Ok(Some(Value::Object(Some(ctx.create_string_uninterned("")))));
    }
    if count == 1 {
        // `return this` — `String.repeat(1)` is specified to hand back the
        // receiver, and a String is immutable, so sharing it is correct.
        return Ok(Some(Value::Object(Some(this))));
    }
    // The JDK's guard, on the same quantity the JDK measures: `value.length`,
    // which is 2 bytes per char once any char needs the UTF-16 coder.
    let value_len = if units.iter().all(|&u| u <= 0xFF) {
        units.len() as i64
    } else {
        units.len() as i64 * 2
    };
    if i64::from(i32::MAX) / i64::from(count) < value_len {
        return Err(oome_repeat_limit().into());
    }
    // Repeat the code UNITS, not a Rust `String`. The old body ended in
    // `String::from_utf16_lossy` + `create_string_uninterned(&str)`, so every
    // unpaired surrogate in the receiver came back as U+FFFD — silently, since
    // the unit COUNT is the same either way (U+FFFD is one unit, as is a lone
    // surrogate) and so `length()` agreed with HotSpot while the content did
    // not. Measured on HotSpot 25: `"a\uD800b".repeat(2)` is
    // `0061 D800 0062 0061 D800 0062` and `"\uDC00".repeat(3)` is
    // `DC00 DC00 DC00`.
    //
    // The lossless writer this needs was already in this file
    // ([`sb_string_from_units`], three callers away) and was already reachable
    // through the existing `NativeContext` — no new VM primitive was required.
    let Some(total) = units.len().checked_mul(count as usize) else {
        return Err(oome_repeat_limit().into());
    };
    let mut result: Vec<u16> = Vec::new();
    // Same fallible reservation as before, on the same quantity the JDK's own
    // guard above measures — the `value` array — rather than on a UTF-8 length
    // that no longer exists here.
    if result.try_reserve_exact(total).is_err() {
        return Err(oome_repeat_limit().into());
    }
    for _ in 0..count {
        result.extend_from_slice(&units);
    }
    let str_obj = sb_string_from_units(ctx, &result)?;
    Ok(Some(Value::Object(Some(str_obj))))
}

/// HotSpot's exact `String.repeat` overflow wording, in one place so the three
/// bail-outs above cannot drift apart.
fn oome_repeat_limit() -> cratonvm_types::error::RuntimeError {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: "Required length exceeds implementation limit".to_string(),
    }
}

/// `String.isBlank()` — empty, or every code point is `Character.isWhitespace`.
///
/// W7-95a. The old body was `s.trim().is_empty()`, and `str::trim` is Unicode
/// **White_Space**, which is not Java's table — see [`java_char_is_whitespace`]
/// for the eight code points that disagree and the measurement. A lone
/// `U+00A0` answered `true` here and `false` on HotSpot; `U+001C`..`U+001F`
/// answered `false` here and `true` on HotSpot.
///
/// Tested per code UNIT rather than per code point, which is equivalent: no
/// supplementary code point is whitespace and neither is a surrogate, so a
/// pair and each of its halves all answer `false`. Reading the units directly
/// also keeps the `str` round trip (and its U+FFFD substitution) out of a
/// method whose answer depends on the exact code points present.
pub(crate) fn native_string_is_blank(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let blank = with_string_chars_scratch(ctx, this, |units| {
        units.iter().all(|&u| java_char_is_whitespace(u))
    });
    Ok(Some(Value::Int(if blank { 1 } else { 0 })))
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
    // `read_string_chars`, not `ctx.read_string(..).encode_utf16()`: `chars()`
    // is the code-UNIT view and must reproduce an unpaired surrogate exactly.
    // Through a Rust `String` one becomes U+FFFD -- `"x\uD800y".chars()` gave
    // `[120, 65533, 121]` against HotSpot 25.0.3+9's `[120, 55296, 121]`
    // (W7-95a). Same root cause as `codePointAt`; same one-line fix.
    let char_values: Vec<i32> = read_string_chars(ctx, this)
        .into_iter()
        .map(i32::from)
        .collect();
    int_stream_of(ctx, &char_values)
}

/// `String.codePoints()` — an `IntStream` of **code points**, which is not
/// `chars()`.
///
/// W7-95a. `codePoints` was registered onto `native_string_chars` with the
/// comment "Same as chars for BMP" at two registration sites. It is not the
/// same for any string that contains a surrogate pair: measured on OpenJDK
/// 25.0.3+9, `"a<U+1F600>b".codePoints()` is `[97, 128512, 98]` and this VM
/// answered `[97, 55357, 56832, 98]` — four elements where Java has three, so
/// every `codePoints().count()` over emoji was wrong, not just the values.
///
/// A surrogate that is *not* part of a pair is its own code point and passes
/// through unchanged (`"x\uD800y"` → `[120, 55296, 121]`), which is exactly
/// why this cannot be written over a Rust `str`.
pub(crate) fn native_string_code_points(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let units = read_string_chars(ctx, this);
    let mut code_points: Vec<i32> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let ch = units[i];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < units.len() {
            let low = units[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                code_points
                    .push(0x10000 + ((i32::from(ch) - 0xD800) << 10) + (i32::from(low) - 0xDC00));
                i += 2;
                continue;
            }
        }
        code_points.push(i32::from(ch));
        i += 1;
    }
    int_stream_of(ctx, &code_points)
}

/// Build the synthetic `IntStream` both `chars()` and `codePoints()` return.
///
/// Extracted so the two cannot drift: the layout notes below were paid for
/// twice already (a 1-field allocation and a reference-array allocation), and
/// a second copy of them is a second place to get them wrong.
fn int_stream_of(ctx: &mut dyn NativeContext, char_values: &[i32]) -> MethodCallResult {
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
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 2)?;
    // Must be a primitive `int[]`, not a reference array: this stream's
    // consumers (`IntStream.forEach`/`toArray`/etc.) read field 0 as an
    // int-element array. A `new_ref_array` allocation stored `Value::Int`s
    // into Object-shaped slots, which silently read back as 0 -- every
    // `"...".chars()` consumer therefore saw a correctly-SIZED but all-zero
    // stream (confirmed via a standalone `chars().toArray()` repro).
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, char_values.len());
    for (i, val) in char_values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*val));
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
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let ooffset_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    region_matches_impl(ctx, this, toffset_i, other, ooffset_i, len_i, ignore_case)
}

/// `String.regionMatches`, both overloads.
///
/// W7-95a. Two defects, one shared body now.
///
/// **A negative `len` answers `true`, and did not.** The JDK's bounds test is
///
/// ```text
/// if ((ooffset < 0) || (toffset < 0)
///         || (toffset > (long) length() - len)
///         || (ooffset > (long) other.length() - len)) return false;
/// ```
///
/// with its own comment "toffset, ooffset, or len might be near `-1>>>1`",
/// which is why it widens to `long` rather than adding. When `len` is
/// negative, `length() - len` is *larger* than the length, the test passes,
/// and the comparison loop `while (len-- > 0)` runs zero times — so the answer
/// is `true`. Measured on OpenJDK 25.0.3+9: `"ABC".regionMatches(0, "abc", 0,
/// -1)` and the same call with `Integer.MIN_VALUE` are both `true`; this VM
/// answered `false` from an `if len_i < 0 { return false }` guard that has no
/// counterpart in the JDK. Callers that pass a computed length depend on it.
///
/// **A null `other` is a NullPointerException — but only where the JDK's `||`
/// actually dereferences it.** That expression SHORT-CIRCUITS, and `other` is
/// touched at the FOURTH term, so the null contract is conditional on the first
/// three. Every row measured on OpenJDK 25.0.3+9 with `"abc"` as the receiver:
///
/// ```text
/// regionMatches(  0, null,  0,  1)   NullPointerException   (reaches other.length())
/// regionMatches(  0, null,  0, -1)   NullPointerException   (negative len does NOT save it)
/// regionMatches(  3, null,  0,  0)   NullPointerException   (toffset == length() - len)
/// "".regionMatches(0, null, 0,  0)   NullPointerException   (empty receiver, still reached)
/// regionMatches( -1, null,  0,  1)   false                  (toffset < 0, term two)
/// regionMatches(  0, null, -1,  1)   false                  (ooffset < 0, term one)
/// regionMatches( 99, null,  0,  1)   false                  (term three, other never read)
/// regionMatches(  0, null,  0,  4)   false                  (term three: 0 > 3 - 4)
/// ```
///
/// with the five-argument overload answering identically for
/// `ignoreCase = true` and delegating for `ignoreCase = false`. This VM
/// answered `false` to all eight: `other` was unwrapped at the top of the
/// native and a null took the same exit as a bounds failure, which is the
/// `[default=wrong write]` shape — a defaulting reader turning a contract
/// violation into a plausible answer. The two behaviours cannot be separated by
/// checking null first (three of the eight rows would then be wrong), so the
/// order below is the JDK's expression, term for term.
///
/// **Both sides are read losslessly.** `ctx.read_string` goes through a Rust
/// `String`, where every unpaired surrogate becomes U+FFFD — so two *different*
/// lone surrogates compared equal. `read_string_chars` reads the `value`
/// arrays as UTF-16 code units, which is also the unit `toffset`/`ooffset`/`len`
/// are specified in. `other` is read only after it has survived the first three
/// terms, which is both the JDK's order and the cheaper one.
/// Whether the JDK's `regionMatches` bounds expression already answers `false`
/// from its first THREE terms — the ones that do not touch `other`.
///
/// Split out so the short-circuit point is testable without a receiver, a
/// heap or a `NativeContext`: `true` here means "return false, and `other`
/// must NOT be dereferenced", `false` means "the JDK now evaluates
/// `other.length()`", which is where a null argument becomes a
/// NullPointerException. See [`region_matches_impl`] for the eight measured
/// rows this reproduces.
///
/// `this_len`, `toffset`, `ooffset` and `len` are `i64` for the same reason the
/// JDK widens to `long`: its own comment says "toffset, ooffset, or len might
/// be near `-1>>>1`", so `length() - len` must not wrap.
fn region_matches_short_circuits(this_len: i64, toffset: i64, ooffset: i64, len: i64) -> bool {
    ooffset < 0 || toffset < 0 || toffset > this_len - len
}

#[allow(clippy::too_many_arguments)]
fn region_matches_impl(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    toffset_i: i32,
    other: Option<cratonvm_types::ObjectRef>,
    ooffset_i: i32,
    len_i: i32,
    ignore_case: bool,
) -> MethodCallResult {
    // regionMatches offsets/len are in UTF-16 code UNITS, not Unicode code
    // points. Index over u16 to agree with charAt/length.
    let s_units = read_string_chars(ctx, this);

    // The JDK's test, in `i64` for the same reason the JDK uses `long`, and in
    // the JDK's order so that the `other.length()` dereference happens exactly
    // where the JDK's does.
    let toffset = i64::from(toffset_i);
    let ooffset = i64::from(ooffset_i);
    let len = i64::from(len_i);
    if region_matches_short_circuits(s_units.len() as i64, toffset, ooffset, len) {
        return Ok(Some(Value::Int(0)));
    }
    // Term four. `other.length()` — the JDK's implicit null check.
    let Some(other) = other else {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"String.length()\" because \"other\" is null".to_string(),
            ),
        }
        .into());
    };
    let o_units = read_string_chars(ctx, other);
    if ooffset > o_units.len() as i64 - len {
        return Ok(Some(Value::Int(0)));
    }
    if len <= 0 {
        // `while (len-- > 0)` compared nothing. Not an error, not `false`.
        return Ok(Some(Value::Int(1)));
    }

    let toffset = toffset as usize;
    let ooffset = ooffset as usize;
    for i in 0..len as usize {
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

/// Case-insensitive comparison of two UTF-16 code units, transcribing
/// `StringUTF16.regionMatchesCI`:
///
/// ```text
/// if (c1 == c2) continue;
/// char u1 = Character.toUpperCase(c1);
/// char u2 = Character.toUpperCase(c2);
/// if (u1 == u2) continue;
/// if (Character.toLowerCase(u1) == Character.toLowerCase(u2)) continue;
/// return false;
/// ```
///
/// Note that the JDK lower-cases the **upper-cased** forms, not the originals;
/// that composition is what makes `U+212A` KELVIN SIGN match `k`.
///
/// W7-95a. The old body called `char::to_uppercase()`/`to_lowercase()`, the
/// FULL Unicode mappings, and compared the resulting iterators.
/// `"İ".regionMatches(true, 0, "i", 0, 1)` is `true` on OpenJDK 25.0.3+9 and
/// was `false` here: Java's 1:1 `Character.toLowerCase(U+0130)` is plain `i`,
/// while the full SpecialCasing mapping is two characters. See
/// [`java_char_to_upper_case`] for the BMP-wide derivation of the difference.
///
/// A surrogate code unit has no case mapping and both helpers return it
/// unchanged, so surrogates compare equal only when bit-identical — which is
/// what HotSpot answers.
fn code_unit_eq_ignore_case(a: u16, b: u16) -> bool {
    if a == b {
        return true;
    }
    let ua = java_char_to_upper_case(a);
    let ub = java_char_to_upper_case(b);
    if ua == ub {
        return true;
    }
    java_char_to_lower_case(ua) == java_char_to_lower_case(ub)
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
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let ooffset_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    region_matches_impl(ctx, this, toffset_i, other, ooffset_i, len_i, false)
}

/// formatted(Object[]) — instance method: this.formatted(args) → String.format(this, args)
/// String.format(Locale, String, Object...) — static method with Locale (ignored for now).
/// args[0] = Locale, args[1] = format String, args[2] = Object[]
pub(crate) fn native_string_format_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the Locale. It used to be DISCARDED, which made
    // `String.format(Locale.GERMANY, "%,.2f", 1234.5)` answer the US
    // `1,234.50` where HotSpot 25 answers `1.234,50` — the grouping and
    // decimal separators are swapped in German, and France's grouping
    // separator is not even an ASCII space (U+202F). Both rows are in
    // `docs/known-issues/jdk-only/W7-32-round-2-differential-run.md`.
    let locale = match args.first() {
        Some(Value::Object(l)) => *l,
        _ => None,
    };
    let format_args = [
        args.get(1).cloned().unwrap_or(Value::Object(None)),
        args.get(2).cloned().unwrap_or(Value::Object(None)),
    ];
    // `Given`, not `DefaultFormat`, EVEN WHEN `locale` IS `None`: an explicit
    // `null` `Locale` is the JDK's "no localization is applied", a different
    // request from an overload that has no `Locale` at all. Collapsing the two
    // is precisely the bug `native_string_format` no longer has, and re-making
    // it here would be that bug arriving from the other direction.
    format_impl(ctx, &format_args, FmtLocale::Given(locale))
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
// So this stays registered for images that DO declare the method, where it must
// give the same answer `UnsafeConstants` gives, which it does. A census row
// reading `has_code: false` here means "absent from this image", not "an
// unimplemented native something is waiting on" — the distinction cost a
// paragraph of doubt in the record that filed the UTF-16 hash defect.
//
// **"(JDK 17/21)" used to be an assumption in this sentence. It is now a
// measurement.** MEASURED 2026-08-21 over the nine supported images —
// `javap -p --system <image> java.lang.StringUTF16`:
//
// ```text
//   jdk17-linux  jdk17-windows  jdk17-mac-x64    private static native boolean isBigEndian();
//   jdk21-linux  jdk21-windows  jdk21-mac-x64    private static native boolean isBigEndian();
//   jdk25-linux  jdk25-windows  jdk25-mac-x64    ABSENT
// ```
//
// Six of nine declare it. Re-derivable with
// `scripts/jdk-only-no-image-methods.py`, which reports this row as `PARTIAL`
// and exists because this comment was the standing witness that a
// single-image census cannot adjudicate the population it belongs to.
// See `docs/known-issues/jdk-only/WORKER-3-NOTE-2-*.md`: 62 of the 355 rows a
// JDK-25-only adjudication calls "declared nowhere" are this same shape.
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
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(index).into());
    }
    let mut bytes = vec![0u8; count * 2];
    if ctx.read_byte_array_into(value, src_begin as usize * 2, &mut bytes) != bytes.len() {
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(src_begin).into());
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

    /// The rule `equalsIgnoreCase` now shares with `regionMatches(true, …)`.
    ///
    /// Every row is the verbatim OpenJDK 25.0.3+9 answer to the corresponding
    /// `String.equalsIgnoreCase` call. The A7Dx rows are the ones Rust's
    /// `to_lowercase()` — the implementation this replaced — got wrong.
    #[test]
    fn equals_ignore_case_folds_the_way_the_jdk_does() {
        // "İ".equalsIgnoreCase("i") is TRUE: Java's 1:1 toLowerCase(U+0130) is
        // a plain `i`, while Rust's full mapping is `i` + U+0307.
        assert!(code_unit_eq_ignore_case(0x0130, 0x0069));
        assert!(code_unit_eq_ignore_case(0x0130, 0x0049));
        // U+212A KELVIN SIGN folds onto `k` through the upper-then-lower
        // composition.
        assert!(code_unit_eq_ignore_case(0x212A, 0x006B));
        // The titlecase letter matches its uppercase partner.
        assert!(code_unit_eq_ignore_case(0x01C5, 0x01C4));
        // The six the JDK does not case-pair, measured `false` on HotSpot and
        // `true` under Rust's newer tables.
        assert!(!code_unit_eq_ignore_case(0xA7D3, 0xA7D2));
        assert!(!code_unit_eq_ignore_case(0xA7D5, 0xA7D4));
        assert!(!code_unit_eq_ignore_case(0xA7CF, 0xA7CE));
        // ... and the neighbours that ARE pairs, so this is not a range.
        assert!(code_unit_eq_ignore_case(0xA7D1, 0xA7D0));
        assert!(code_unit_eq_ignore_case(0xA7D7, 0xA7D6));
        // Identity still holds for all six.
        for cp in [0xA7CEu16, 0xA7CF, 0xA7D2, 0xA7D3, 0xA7D4, 0xA7D5] {
            assert!(code_unit_eq_ignore_case(cp, cp));
        }
    }

    /// `compareToIgnoreCase` returns a DIFFERENCE, not a sign.
    ///
    /// The folded-difference rows measured on OpenJDK 25.0.3+9:
    /// `"_".compareToIgnoreCase("a")` is `-2`, `"B".compareToIgnoreCase("a")`
    /// is `1`, `"İ".compareToIgnoreCase("i")` is `0`, and
    /// `"\u{A7D3}".compareToIgnoreCase("\u{A7D2}")` is `1`. The old body
    /// returned `-1`/`0`/`1` from an `Ordering` over Rust-lowercased strings,
    /// which is a wrong magnitude in the first row and a wrong SIGN in the
    /// third and fourth.
    #[test]
    fn compare_to_ignore_case_returns_the_folded_difference() {
        // The fold each row depends on, in the JDK's upper-then-lower order.
        let folded = |c: u16| java_char_to_lower_case(java_char_to_upper_case(c));
        assert_eq!(i32::from(folded(b'_' as u16)) - i32::from(folded(b'a' as u16)), -2);
        assert_eq!(i32::from(folded(b'B' as u16)) - i32::from(folded(b'a' as u16)), 1);
        // "İ" vs "i": the uppercases differ, the lowercases do not, so the
        // loop continues and equal lengths give 0.
        assert_ne!(java_char_to_upper_case(0x0130), java_char_to_upper_case(0x0069));
        assert_eq!(folded(0x0130), folded(0x0069));
        // A7D3 vs A7D2: both fold to themselves, so the difference is 1.
        assert_eq!(i32::from(folded(0xA7D3)) - i32::from(folded(0xA7D2)), 1);
    }

    /// The `regionMatches` short circuit, pinned against the eight rows
    /// measured on OpenJDK 25.0.3+9 with the receiver `"abc"`.
    ///
    /// `true` means the JDK returned `false` without touching `other`; `false`
    /// means it reached `other.length()`, which is a NullPointerException when
    /// `other` is null. All eight used to answer plain `false` here.
    #[test]
    fn region_matches_dereferences_other_exactly_where_the_jdk_does() {
        let abc = 3i64;
        // Reaches other.length() -> NPE for a null `other`.
        assert!(!region_matches_short_circuits(abc, 0, 0, 1));
        assert!(!region_matches_short_circuits(abc, 0, 0, -1)); // negative len does NOT save it
        assert!(!region_matches_short_circuits(abc, 3, 0, 0)); // toffset == length() - len
        assert!(!region_matches_short_circuits(0, 0, 0, 0)); // "" receiver, still reached
        assert!(!region_matches_short_circuits(abc, 0, 0, i64::from(i32::MIN)));
        // Decided by terms one to three -> `false`, `other` never read.
        assert!(region_matches_short_circuits(abc, -1, 0, 1)); // toffset < 0
        assert!(region_matches_short_circuits(abc, 0, -1, 1)); // ooffset < 0
        assert!(region_matches_short_circuits(abc, 99, 0, 1)); // toffset > len - len
        assert!(region_matches_short_circuits(abc, 0, 0, 4)); // 0 > 3 - 4
        assert!(region_matches_short_circuits(abc, i64::from(i32::MAX), 0, 1));
        // The widening is load-bearing: with `i32` arithmetic
        // `length() - Integer.MIN_VALUE` wraps negative and the third term
        // would wrongly short-circuit.
        assert!(!region_matches_short_circuits(abc, 0, 0, i64::from(i32::MIN)));
    }

    // -----------------------------------------------------------------------
    // E18: String.{indexOf,lastIndexOf}(int ch[, int from])
    //
    // Written against a `&[u16]` rather than a heap String on purpose: a
    // `NativeContext` mock cannot even express the interesting receivers,
    // because `create_string(&str)` cannot hold an UNPAIRED surrogate and the
    // lone-surrogate rows are half the contract. Every expectation below is a
    // measured OpenJDK 25.0.3+9 answer (`scratchpad/e18/CharSearch.java`,
    // `Idx3.java`) — not a re-derivation from Rust's `char`, which is the
    // proxy-oracle trap E8 §4.2 warns about and which is exactly what
    // `char::from_u32` was doing here.
    // -----------------------------------------------------------------------

    /// `"xзy𐐷z"` — a Cyrillic з (U+0437, the LOW HALF of U+10437 when masked),
    /// then the real surrogate pair for U+10437 at index 3.
    const MIXED: &[u16] = &[0x0078, 0x0437, 0x0079, 0xD801, 0xDC37, 0x007A];
    /// `"abc"`, whose 'a' is `0x10061 & 0xFFFF`.
    const ABC: &[u16] = &[0x0061, 0x0062, 0x0063];

    fn idx(h: &[u16], ch: i32, from: i32) -> i32 {
        match code_point_needle(ch) {
            Some(n) => index_of_units_from(h, n.units(), from),
            None => -1,
        }
    }

    fn last_idx(h: &[u16], ch: i32, from: i64) -> i32 {
        match code_point_needle(ch) {
            Some(n) => last_index_of_units_from(h, n.units(), from),
            None => -1,
        }
    }

    #[test]
    fn a_supplementary_needle_is_a_surrogate_pair_and_never_its_masked_low_half() {
        // THE row: `(ch & 0xFFFF)` finds 'a' at 0. HotSpot answers -1.
        assert_eq!(idx(ABC, 0x10061, 0), -1);
        assert_eq!(idx(ABC, 0x10061, -5), -1);
        assert_eq!(last_idx(ABC, 0x10061, 2), -1);
        assert_eq!(last_idx(ABC, 0x10061, i64::MAX), -1);
        // Same shape with a receiver that really does contain the low half.
        assert_eq!(idx(&[0x0437], 0x10437, 0), -1);
        assert_eq!(last_idx(&[0x0437], 0x10437, i64::MAX), -1);
        // …and the pair itself IS found, at the index of the HIGH surrogate.
        assert_eq!(idx(MIXED, 0x10437, 0), 3);
        assert_eq!(last_idx(MIXED, 0x10437, i64::MAX), 3);
    }

    #[test]
    fn a_lone_surrogate_needle_is_an_ordinary_code_unit_scan() {
        // `char::from_u32` answers `None` for both of these, which is how
        // `lastIndexOf(int)` came to return -1 where HotSpot returns 3 and 4.
        assert_eq!(idx(MIXED, 0xD801, 0), 3);
        assert_eq!(idx(MIXED, 0xDC37, 0), 4);
        assert_eq!(last_idx(MIXED, 0xD801, i64::MAX), 3);
        assert_eq!(last_idx(MIXED, 0xDC37, i64::MAX), 4);
    }

    #[test]
    fn the_gate_is_is_valid_code_point_and_not_a_narrowing_cast() {
        // `(char) -1` is 0xFFFF and this receiver holds 0xFFFF, yet HotSpot
        // answers -1: `Character.isValidCodePoint` is checked first.
        let ffff: &[u16] = &[0xFFFF, 0x0071];
        assert_eq!(idx(ffff, -1, 0), -1);
        assert_eq!(last_idx(ffff, -1, i64::MAX), -1);
        assert_eq!(idx(ffff, i32::MIN, 0), -1);
        // 0xFFFF is a NONCHARACTER but a valid code point, so it IS found.
        assert_eq!(idx(ffff, 0xFFFF, 0), 0);
        // Above the Unicode range, nothing matches.
        assert_eq!(idx(ABC, 0x110000, 0), -1);
        assert_eq!(idx(ABC, 0x1FFFFFF, 0), -1);
        assert_eq!(last_idx(ABC, 0x110000, i64::MAX), -1);
    }

    #[test]
    fn from_index_clamps_forward_and_floors_backward() {
        assert_eq!(idx(MIXED, 0x10437, 3), 3);
        assert_eq!(idx(MIXED, 0x10437, 4), -1); // starts past the pair
        assert_eq!(idx(MIXED, 0x10437, 99), -1);
        assert_eq!(idx(MIXED, 0x10437, -5), 3); // negative clamps to 0
        assert_eq!(idx(ABC, 0x61, 99), -1);
        // Backward: `min(from, len - width)`, and a NEGATIVE from finds
        // nothing even when the needle is present — the JDK's loop counter
        // starts below zero and never runs.
        assert_eq!(last_idx(MIXED, 0x10437, 99), 3);
        assert_eq!(last_idx(MIXED, 0x10437, 4), 3);
        assert_eq!(last_idx(MIXED, 0x10437, 3), 3);
        assert_eq!(last_idx(MIXED, 0x10437, 2), -1); // width 2: start is 2, pair is at 3
        assert_eq!(last_idx(MIXED, 0x10437, -1), -1);
        assert_eq!(last_idx(ABC, 0x61, -1), -1);
        assert_eq!(last_idx(ABC, 0x61, 99), 0);
        assert_eq!(last_idx(&[], 0x61, 0), -1);
        // A two-unit needle in a two-unit haystack, started past index 0.
        assert_eq!(idx(&[0xD801, 0xDC37], 0x10437, 1), -1);
        assert_eq!(idx(&[0xD801, 0xDC37], 0x10437, 0), 0);
    }

    #[test]
    fn the_empty_string_needle_keeps_its_own_contract() {
        // Not reachable from `code_point_needle` (which is never empty) but
        // shared with the String-needle overloads, whose measured answers are
        // `"abcabc".indexOf("", 99) == 6` and `indexOf("", -5) == 0`.
        let abcabc: &[u16] = &[0x61, 0x62, 0x63, 0x61, 0x62, 0x63];
        assert_eq!(index_of_units_from(abcabc, &[], 99), 6);
        assert_eq!(index_of_units_from(abcabc, &[], -5), 0);
        assert_eq!(index_of_units_from(abcabc, &[], 2), 2);
        assert_eq!(last_index_of_units_from(abcabc, &[], 99), 6);
        assert_eq!(last_index_of_units(abcabc, &[]), 6);
        // …and a needle longer than the haystack is a miss, not a panic.
        assert_eq!(index_of_units_from(&[0x61], &[0x61, 0x62], 0), -1);
        assert_eq!(
            last_index_of_units_from(&[0x61], &[0x61, 0x62], i64::MAX),
            -1
        );
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

    /// E18. The JIT's door and the interpreter's door must answer the same
    /// null `Locale`.
    ///
    /// This is the whole shape of the bug: `jit_string_to_lower_case` called
    /// `string_case_impl` one layer BELOW the null check, so the same source
    /// line threw while it was interpreted and returned the default-locale
    /// string once it tiered up. A tier-dependent answer is invisible to any
    /// test that runs the method only a few times, which is why the assertion
    /// is written against the two ENTRY POINTS rather than against the
    /// behaviour of a warm loop.
    ///
    /// Measured on OpenJDK 25.0.3+9: `"AbC".toLowerCase()` is `"abc"`,
    /// `"AbC".toLowerCase((Locale) null)` throws NullPointerException, and it
    /// still throws on the 200 000th warm iteration — HotSpot's answer does
    /// not depend on its tier either.
    #[test]
    fn the_jit_lower_case_door_enforces_the_same_null_locale_as_the_native() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("AbC");

        // ABSENT Locale slot — the no-argument overload. Answers, does not
        // throw. (The JIT has no such call: its one bound descriptor,
        // `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`,
        // always carries the slot.)
        let absent = native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s))]);
        match absent {
            Ok(Some(Value::Object(Some(o)))) => assert_eq!(ctx.read_string(o).unwrap(), "abc"),
            other => panic!("toLowerCase() must answer \"abc\", got {other:?}"),
        }

        // PRESENT-and-null Locale — both doors must throw.
        let interpreted =
            native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s)), Value::Object(None)]);
        assert!(
            interpreted.is_err(),
            "the interpreted native must throw for toLowerCase((Locale) null)"
        );
        let jit = jit_string_to_lower_case(&mut ctx, s, None);
        assert!(
            jit.is_err(),
            "the JIT direct-call door must throw for the SAME call the interpreter throws for — \
             it took `string_case_impl` directly and answered the default-locale string"
        );
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

    /// The pin this test carries was REVERSED on 2026-08-12.
    ///
    /// It used to assert that `[-1, 100)` over `"abc"` clamps to `[0, 3)` and
    /// appends `"abc"`, which pinned the clamp W7-3 listed as a defect and
    /// declined to reverse. JDK 25's body is `checkRange(start, end,
    /// s.length())` and every out-of-range end of that window throws
    /// `IndexOutOfBoundsException` — the sibling `append(char[], int, int)`
    /// has raised it since W7-3. A clamp is not a lenient success, it is a
    /// SHORT append reported as a complete one, so the builder ends up holding
    /// text the caller never asked for.
    ///
    /// Both polarities are asserted here, because a check that only rejects
    /// would pass on a native that rejects everything.
    /// docs/known-issues/jdk-only/W7-3-format-conversions-and-stringbuilder-bounds.md
    #[test]
    fn sb_append_charsequence_off_len_rejects_an_out_of_range_window() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let s = ctx.create_string("abc");
        for (start, end) in [(-1, 3), (0, 100), (-1, 100), (2, 1)] {
            let e = native_sb_append_charsequence_off_len(
                &mut ctx,
                &[
                    Value::Object(Some(sb)),
                    Value::Object(Some(s)),
                    Value::Int(start),
                    Value::Int(end),
                ],
            )
            .expect_err(&format!("[{start}, {end}) over \"abc\" must be refused"));
            // `checkRange`, not `checkRangeSIOOBE`: the javadoc names the plain
            // IndexOutOfBoundsException for this overload, and a caller
            // catching SIOOBE specifically must NOT see it.
            assert_eq!(err_kind(&ctx, &e), "ioobe", "[{start}, {end})");
        }
        // The builder is untouched by every refusal above — a rejected append
        // must not be a partial one.
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "");
        // The positive half: the in-range boundary window still appends.
        let r = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(Some(s)),
                Value::Int(0),
                Value::Int(3),
            ],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "abc");
    }

    /// `s == null` becomes the four-character `"null"` BEFORE the range check,
    /// so the window is checked against 4 rather than against the caller's
    /// numbers — `append(null, 0, 9)` throws, `append(null, 1, 3)` appends
    /// `"ul"`. Getting the ORDER wrong here reads as a harmless difference and
    /// is not: it decides whether a null sequence can produce an exception at
    /// all.
    #[test]
    fn sb_append_charsequence_off_len_null_is_the_null_literal_then_checked() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        let e = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(None),
                Value::Int(0),
                Value::Int(9),
            ],
        )
        .expect_err("[0, 9) over the \"null\" literal must be refused");
        assert_eq!(err_kind(&ctx, &e), "ioobe");
        let r = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(None),
                Value::Int(1),
                Value::Int(3),
            ],
        );
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), "ul");
    }

    /// `appendCodePoint` admits every BMP code point INCLUDING lone surrogates
    /// (JDK: `isBmpCodePoint` is `cp >>> 16 == 0`, so `Character.toChars` — the
    /// only thrower — is never reached for them), encodes a supplementary one
    /// as a surrogate pair, and REFUSES anything outside `0..=0x10FFFF`.
    ///
    /// The refusal is the half that regressed: the old body truncated an
    /// invalid code point to `cp as u16`, writing a character the caller never
    /// named. The lone-surrogate case is asserted alongside it because that is
    /// the input the truncation was justified by, and a fix that "tightened"
    /// this into `char::from_u32` would break it.
    #[test]
    fn sb_append_code_point_admits_surrogates_and_refuses_non_code_points() {
        let mut ctx = mock_ctx();
        let sb = make_sb(&mut ctx);
        for cp in [0x41, 0x1_F600, 0xD800, 0xDFFF, 0x10_FFFF] {
            native_sb_append_code_point(&mut ctx, &[Value::Object(Some(sb)), Value::Int(cp)])
                .unwrap_or_else(|_| panic!("appendCodePoint(0x{cp:X}) must be admitted"));
        }
        // 1 + 2 + 1 + 1 + 2 code units.
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        let text = ctx.read_string(out_obj).unwrap();
        assert_eq!(text.encode_utf16().count(), 7, "got {text:?}");

        for cp in [-1, 0x11_0000, i32::MIN, i32::MAX] {
            let r = native_sb_append_code_point(&mut ctx, &[Value::Object(Some(sb)), Value::Int(cp)]);
            let refused = matches!(
                r,
                Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::IllegalArgumentException { .. }
                    )
                ))
            );
            assert!(refused, "appendCodePoint(0x{cp:X}) must raise IllegalArgumentException");
        }
    }

    /// The four scalar `insert` overloads that had no native at all. Each
    /// value is chosen so a wrong renderer cannot produce it by accident: a
    /// long past `int` range, a `float`/`double` pair that `Float.toString` and
    /// `Double.toString` spell differently from Rust's `Display` for other
    /// inputs, and `checkOffset` on the far side.
    #[test]
    fn sb_scalar_insert_overloads_render_and_check_the_offset() {
        for (args_tail, expected) in [
            (Value::Int(1), "atruec"),
            (Value::Long(1_234_567_890_123), "a1234567890123c"),
            (Value::Float(1.5), "a1.5c"),
            (Value::Double(2.25), "a2.25c"),
        ] {
            let mut ctx = mock_ctx();
            let sb = make_sb_with(&mut ctx, "ac");
            let args = [Value::Object(Some(sb)), Value::Int(1), args_tail];
            let r = match args_tail {
                Value::Int(_) => native_sb_insert_boolean(&mut ctx, &args),
                Value::Long(_) => native_sb_insert_long(&mut ctx, &args),
                Value::Float(_) => native_sb_insert_float(&mut ctx, &args),
                _ => native_sb_insert_double(&mut ctx, &args),
            };
            assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
            let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
            let Some(Value::Object(Some(out_obj))) = out_r else {
                panic!()
            };
            assert_eq!(ctx.read_string(out_obj).unwrap(), expected);
        }

        // `checkOffset(offset, count)` — the same check the six overloads that
        // already had natives run. An offset past the end must throw, not
        // append at the end.
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "ab");
        let e = native_sb_insert_long(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(99), Value::Long(1)],
        )
        .expect_err("insert(99, 1L) on a 2-char builder must be refused");
        assert_eq!(err_kind(&ctx, &e), "sioobe");
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

    /// The detail message of an `IndexOutOfBoundsException`-family failure.
    ///
    /// The exact wording separates `Preconditions`' `checkFromToIndex` shape
    /// (`Range [a, b) out of bounds for length n`) from the `checkIndex` shape,
    /// which is how a range check that reports the wrong WINDOW is caught even
    /// when it reports the right class.
    fn err_message(e: &cratonvm_types::error::MethodCallFailed) -> Option<String> {
        match e {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(re),
            ) => match re {
                cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { message }
                | cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                    message,
                    ..
                }
                | cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                    message,
                    ..
                } => message.clone(),
                _ => None,
            },
            _ => None,
        }
    }

    // `AbstractStringBuilder.getChars` throws THREE different classes depending
    // on which argument is wrong, and the choice is not the intuitive one. Its
    // two range checks are one line apart in the JDK and pass DIFFERENT
    // formatters — `SIOOBE_FORMATTER` for the source window, `IOOBE_FORMATTER`
    // for the destination — so the destination case is the PLAIN
    // `IndexOutOfBoundsException`, the superclass of both the
    // `ArrayIndexOutOfBoundsException` the element stores suggest and the
    // `StringIndexOutOfBoundsException` its own source-side neighbour throws.
    //
    // These assert through `err_kind`, which discriminates all three, because a
    // `matches!` on `IndexOutOfBoundsException` alone cannot: the two wrong
    // answers are SUBCLASSES, so any assertion phrased on the supertype passes
    // on the broken code. MEASURED against jdk-25.0.3.9; see
    // `docs/known-issues/jdk-only/G53-1-the-exact-exception-class-and-the-rest-of-sbidx-20260817.md`.

    #[test]
    fn sb_get_chars_too_small_dst_throws_the_plain_ioobe_not_a_subclass() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        let dst = ctx.new_array(ArrayElementType::Char, 2);
        // `RJdkBridge1`'s `sbidx-step=getChars into a too-small array`, verbatim.
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(3),
                Value::Object(Some(dst)),
                Value::Int(0),
            ],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "ioobe");
        // The window named is the DESTINATION one, `[dstBegin, dstBegin + n)`
        // against `dst.length` — not the source window `[0, 3)` against 5.
        assert_eq!(
            err_message(&err).as_deref(),
            Some("Range [0, 3) out of bounds for length 2")
        );
    }

    #[test]
    fn sb_get_chars_negative_dst_begin_throws_the_plain_ioobe() {
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
        assert_eq!(err_kind(&ctx, &err), "ioobe");
        assert_eq!(
            err_message(&err).as_deref(),
            Some("Range [-1, 2) out of bounds for length 8")
        );
    }

    #[test]
    fn sb_get_chars_dst_overrun_throws_the_plain_ioobe() {
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
        assert_eq!(err_kind(&ctx, &err), "ioobe");
        assert_eq!(
            err_message(&err).as_deref(),
            Some("Range [2, 7) out of bounds for length 4")
        );
        // The out-of-bounds store must NOT have silently written anything past
        // the array; in-range slots remain at their zero default.
        for i in 0..ctx.array_length(dst) {
            assert_eq!(ctx.get_array_element(dst, i), Value::Int(0));
        }
    }

    #[test]
    fn sb_get_chars_bad_src_range_beats_a_null_destination() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        // The source check runs BEFORE `dst` is dereferenced, so `srcBegin >
        // srcEnd` with a null `dst` is a StringIndexOutOfBoundsException and
        // NOT the NullPointerException that checking the argument first would
        // give. Ordering, not just class choice.
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(3),
                Value::Int(1),
                Value::Object(None),
                Value::Int(0),
            ],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");
        assert_eq!(
            err_message(&err).as_deref(),
            Some("Range [3, 1) out of bounds for length 5")
        );
    }

    #[test]
    fn sb_get_chars_null_dst_throws_npe_even_when_nothing_would_be_copied() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        // `dst.length` is read unconditionally once the source window is
        // accepted, so a zero-length copy into a null array still throws.
        let err = native_sb_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Int(0),
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

    // `codePointCount` is the ONE range check in this class that the JDK gives a
    // null formatter (`Preconditions.checkFromToIndex(begin, end, count,
    // null)`), so it yields the PLAIN `IndexOutOfBoundsException` while its
    // `delete`/`replace`/`substring` neighbours, checked by the same shared
    // helper here, yield `StringIndexOutOfBoundsException`. The paired
    // neighbour assertions below exist so that "fixing" this by changing the
    // shared helper fails loudly instead of silently regressing four methods.

    #[test]
    fn sb_code_point_count_bad_range_throws_the_plain_ioobe() {
        for (begin, end, want_msg) in [
            (0, 6, "Range [0, 6) out of bounds for length 5"),
            (-1, 3, "Range [-1, 3) out of bounds for length 5"),
            (3, 1, "Range [3, 1) out of bounds for length 5"),
        ] {
            let mut ctx = mock_ctx();
            let sb = make_sb_with(&mut ctx, "abcde");
            let err = native_sb_code_point_count(
                &mut ctx,
                &[Value::Object(Some(sb)), Value::Int(begin), Value::Int(end)],
            )
            .unwrap_err();
            assert_eq!(
                err_kind(&ctx, &err),
                "ioobe",
                "codePointCount({begin}, {end})"
            );
            assert_eq!(err_message(&err).as_deref(), Some(want_msg));
        }
    }

    #[test]
    fn sb_substring_and_delete_keep_the_string_subclass_that_code_point_count_drops() {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        let err = native_sb_substring_range(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(3), Value::Int(1)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");

        let sb = make_sb_with(&mut ctx, "abcde");
        let err = native_sb_delete(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(3), Value::Int(1)],
        )
        .unwrap_err();
        assert_eq!(err_kind(&ctx, &err), "sioobe");
    }

    #[test]
    fn sb_code_point_count_valid_window_still_counts_surrogate_pairs() {
        let mut ctx = mock_ctx();
        // "a" + U+10437 (a surrogate PAIR) + "b" = 4 units, 3 code points.
        let sb = make_sb_with(&mut ctx, "a\u{10437}b");
        let r = native_sb_code_point_count(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(0), Value::Int(4)],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(3)));
    }

    #[test]
    fn sb_init_capacity_negative_throws_negative_array_size_not_a_clamp() {
        // `AbstractStringBuilder(int)` is just `new byte[capacity]`, so a
        // negative capacity throws NegativeArraySizeException carrying the raw
        // size. Clamping to zero built a usable empty builder instead.
        //
        // Deliberately a DIFFERENT class from `setLength(-1)`'s
        // StringIndexOutOfBoundsException — the two negative-length paths in
        // this class do not agree, and `RJdkBridge1` asserts both in a row.
        for size in [-1_i32, -7] {
            let mut ctx = mock_ctx();
            let cid = ctx
                .ensure_class_initialized("java/lang/StringBuilder")
                .unwrap();
            let sb = ctx.alloc_object(cid, 4);
            let err =
                native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(size)])
                    .unwrap_err();
            assert!(
                matches!(
                    err,
                    cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(
                            cratonvm_types::error::RuntimeError::NegativeArraySizeException {
                                size: s
                            }
                        )
                    ) if s == size
                ),
                "new StringBuilder({size}) must throw NegativeArraySizeException({size})"
            );
        }
    }

    #[test]
    fn sb_init_capacity_zero_and_positive_still_build_an_empty_builder() {
        for size in [0_i32, 5] {
            let mut ctx = mock_ctx();
            let cid = ctx
                .ensure_class_initialized("java/lang/StringBuilder")
                .unwrap();
            let sb = ctx.alloc_object(cid, 4);
            native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(size)])
                .unwrap();
            // A capacity is not a length: the builder starts empty either way.
            let len = native_sb_length(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
            assert_eq!(
                len,
                Some(Value::Int(0)),
                "new StringBuilder({size}).length()"
            );
        }
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

/// F22 — the UTF-16-unit formatter pipeline and the `%t` zone.
///
/// Every expectation here was measured on HotSpot 25.0.3+9-LTS before it was
/// written, and each test names the wrong implementation it kills. These are
/// PURE functions asserted directly: routing them through a `NativeContext`
/// mock would have measured the mock's name-to-slot fallback rather than the
/// rule.
#[cfg(test)]
mod f22_utf16_formatter_tests {
    use super::*;

    /// `[%5.1s]` of U+1F600 is SEVEN characters on HotSpot 25:
    /// `005B 0020 0020 0020 0020 D83D 005D`. The truncation keeps the lone HIGH
    /// surrogate, and the justifier then pads to 5 - 1 = 4.
    ///
    /// Mutants this kills:
    ///   * a truncator that counts CODE POINTS keeps the whole pair -> 2 units,
    ///     pad 3, total 7 but the WRONG units (`D83D DE00` both present);
    ///   * a truncator that refuses to split a pair returns 0 units, pad 5,
    ///     total 7 again — which is why the assertion is on the UNITS and not
    ///     only on the length. Both wrong answers have length 7.
    #[test]
    fn truncate_then_justify_keeps_a_lone_high_surrogate() {
        let emoji: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        assert_eq!(emoji, vec![0xD83D, 0xDE00]);

        let mut units = emoji.clone();
        fmt_truncate_units(&mut units, 1);
        assert_eq!(
            units,
            vec![0xD83D],
            "%.1s must keep the lone high surrogate"
        );

        let padded = fmt_pad_to_width(units, "", Some(5)).expect("width 5 is small");
        assert_eq!(padded, vec![0x0020, 0x0020, 0x0020, 0x0020, 0xD83D]);

        // The whole `[%5.1s]` row, brackets included.
        let mut row = vec![0x005Bu16];
        row.extend_from_slice(&padded);
        row.push(0x005D);
        assert_eq!(row.len(), 7, "HotSpot 25 measures 7 characters");
        assert_eq!(
            row,
            vec![0x005B, 0x0020, 0x0020, 0x0020, 0x0020, 0xD83D, 0x005D]
        );
    }

    /// `%.3s` of `"a<U+1F600>b"` is three code UNITS — `0061 D83D DE00` — and
    /// `%.2s` of three lone high surrogates keeps two of them.
    ///
    /// Kills a code-POINT truncator (which answers `a<U+1F600>b`, 4 units) and
    /// a byte truncator (`str::len` would cut `a` plus 2 bytes of the emoji).
    #[test]
    fn truncate_counts_code_units_not_points_or_bytes() {
        let mut units: Vec<u16> = "a\u{1F600}b".encode_utf16().collect();
        assert_eq!(units.len(), 4);
        fmt_truncate_units(&mut units, 3);
        assert_eq!(units, vec![0x0061, 0xD83D, 0xDE00]);

        let mut lone = vec![0xD800u16, 0xD800, 0xD800];
        fmt_truncate_units(&mut lone, 2);
        assert_eq!(lone, vec![0xD800, 0xD800]);

        // A precision at or past the length is a no-op, not a pad.
        let mut short = vec![0x0061u16];
        fmt_truncate_units(&mut short, 9);
        assert_eq!(short, vec![0x0061]);
    }

    /// The justifier counts UTF-16 units, pads with U+0020, and honours '-'.
    ///
    /// Kills a `str::len` (byte) justifier: `"é"` is one unit and two UTF-8
    /// bytes, and `String.format("%5s|", "é")` needs FOUR spaces on HotSpot 25.
    /// Also kills a justifier that treats a lone surrogate as zero-width.
    #[test]
    fn justifier_counts_units_and_honours_left_flag() {
        let e: Vec<u16> = "\u{00e9}".encode_utf16().collect();
        assert_eq!(e.len(), 1);
        let padded = fmt_pad_to_width(e.clone(), "", Some(5)).unwrap();
        assert_eq!(padded.len(), 5);
        assert_eq!(padded[..4], [0x0020, 0x0020, 0x0020, 0x0020]);

        let left = fmt_pad_to_width(e, "-", Some(5)).unwrap();
        assert_eq!(left[0], 0x00E9);
        assert_eq!(left[1..], [0x0020, 0x0020, 0x0020, 0x0020]);

        // A lone surrogate is ONE unit to the justifier, like any other char.
        let lone = fmt_pad_to_width(vec![0xD800], "", Some(3)).unwrap();
        assert_eq!(lone, vec![0x0020, 0x0020, 0xD800]);

        // No width, and a width no larger than the content, are both no-ops.
        assert_eq!(
            fmt_pad_to_width(vec![0x0061], "", None).unwrap(),
            vec![0x0061]
        );
        assert_eq!(
            fmt_pad_to_width(vec![0x0061, 0x0062], "", Some(2)).unwrap(),
            vec![0x0061, 0x0062]
        );
    }

    /// `Character.toChars` semantics for the `%c` conversion, as
    /// [`fmt_general_units`] applies them: one unit for any BMP code point
    /// INCLUDING a lone surrogate, a high+low pair for a supplementary one.
    ///
    /// Measured: `String.format(ROOT, "%c", (int) 0xD800)` has `length() == 1`
    /// and unit `D800` on HotSpot 25. Kills the
    /// `char::from_u32(..).unwrap_or('?')` implementation, which answers `003F`.
    #[test]
    fn code_point_to_units_matches_character_to_chars() {
        fn to_units(cp: u32) -> Vec<u16> {
            if cp > 0xFFFF {
                let v = cp - 0x10000;
                vec![(0xD800 + (v >> 10)) as u16, (0xDC00 + (v & 0x3FF)) as u16]
            } else {
                vec![cp as u16]
            }
        }
        assert_eq!(
            to_units(0xD800),
            vec![0xD800],
            "lone HIGH surrogate, not '?'"
        );
        assert_eq!(to_units(0xDFFF), vec![0xDFFF], "lone LOW surrogate, not '?'");
        assert_eq!(to_units(0x1F600), vec![0xD83D, 0xDE00]);
        assert_eq!(to_units(0x41), vec![0x0041]);
        assert_eq!(to_units(0x10FFFF), vec![0xDBFF, 0xDFFF]);
        // Agrees with Rust wherever Rust can answer at all.
        for cp in [0x41u32, 0xE9, 0x20AC, 0x1F600, 0x10FFFF] {
            let expect: Vec<u16> = char::from_u32(cp)
                .unwrap()
                .to_string()
                .encode_utf16()
                .collect();
            assert_eq!(to_units(cp), expect, "cp {cp:#x}");
        }
    }

    /// `TimeZone`'s `GMT±HH:MM` fallback, which is HotSpot's own `Locale.ROOT`
    /// answer for `%tZ`. Measured rows: `America/New_York` in November is
    /// `GMT-05:00` (offset -18000000 ms) and in July `GMT-04:00`;
    /// `Europe/Berlin` is `GMT+01:00` / `GMT+02:00`; `Asia/Kolkata` is
    /// `GMT+05:30`.
    ///
    /// Kills an implementation that (a) drops the sign on a negative offset,
    /// (b) divides by 3_600_000 and loses the half-hour minutes, or
    /// (c) renders the minutes as a fraction of an hour.
    #[test]
    fn gmt_fallback_form_matches_hotspot_rows() {
        assert_eq!(fmt_zone_gmt_form(-18_000_000), "GMT-05:00");
        assert_eq!(fmt_zone_gmt_form(-14_400_000), "GMT-04:00");
        assert_eq!(fmt_zone_gmt_form(3_600_000), "GMT+01:00");
        assert_eq!(fmt_zone_gmt_form(7_200_000), "GMT+02:00");
        assert_eq!(fmt_zone_gmt_form(19_800_000), "GMT+05:30", "half-hour zone");
        assert_eq!(fmt_zone_gmt_form(0), "GMT+00:00");
        // Nepal, the 45-minute zone — the case an hours-only render would lose.
        assert_eq!(fmt_zone_gmt_form(20_700_000), "GMT+05:45");
    }

    /// A zone that was never resolved must keep the pre-existing UTC identity,
    /// so a VM with no reachable `java.util.TimeZone` does not start printing
    /// `GMT+00:00` where it used to print `UTC`, and does not shift any field.
    #[test]
    fn unknown_zone_is_utc_and_zero_offset() {
        assert_eq!(FmtZone::NONE.offset_ms, 0);
        assert!(!FmtZone::NONE.dst);
        assert!(!FmtZone::NONE.known);
        // A `NONE` zone must not claim to be a `java.time` one, or `%tZ` would
        // go looking for a `ZoneId` on a `Date`.
        assert!(!FmtZone::NONE.temporal);
    }

    /// Which `java.time` sources refuse which `%t` fields — the full measured
    /// matrix from HotSpot 25 (`String.format(Locale.ROOT, "%t"+f, src)` for
    /// each of the 31 fields against each source, under
    /// `-Duser.timezone=Asia/Kolkata`). The six original sources are F28-1's;
    /// `OffsetTime`, `YearMonth`, `MonthDay`, `Year` and `Month` are G2-1's,
    /// measured 2026-08-16.
    ///
    /// This is the check the lane exists for: every one of these used to be
    /// ANSWERED here, from a fabricated 0, because `invoke_i32` returns 0 for
    /// a method the class does not have.
    ///
    /// Mutants this kills, each a specific wrong implementation:
    ///  * "only `%tZ` refuses" (the brief's framing) — the `%tH`/`%tY` rows;
    ///  * one flag for all clock fields — the `Instant` `%tL`/`%tN` rows,
    ///    which are ANSWERED while `%tH` is not;
    ///  * composites reporting their OWN character — `%tT` of a `LocalDate`
    ///    reports `H`, `%tc` of an `Instant` reports `a`;
    ///  * `%tF` reporting an inner character — it reports `F`;
    ///  * `%tc` reporting a fixed character — it is `a`, `H` or `Z`
    ///    depending on which group is missing;
    ///  * refusing anything on an epoch-shaped source.
    #[test]
    fn temporal_support_matrix_matches_hotspot() {
        const INSTANT: FmtSupport = FmtSupport {
            year: false,
            month: false,
            day: false,
            time: false,
            sub_second: true,
            instant: true,
            zone: false,
            calendar_printer: false,
        };
        const LOCAL_DATE: FmtSupport = FmtSupport {
            year: true,
            month: true,
            day: true,
            time: false,
            sub_second: false,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const LOCAL_TIME: FmtSupport = FmtSupport {
            year: false,
            month: false,
            day: false,
            time: true,
            sub_second: true,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const LOCAL_DATE_TIME: FmtSupport = FmtSupport {
            year: true,
            month: true,
            day: true,
            time: true,
            sub_second: true,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const ZONED: FmtSupport = FmtSupport {
            year: true,
            month: true,
            day: true,
            time: true,
            sub_second: true,
            instant: true,
            zone: true,
            calendar_printer: false,
        };
        // G2-1's four partial `java.time` sources, plus `OffsetTime`.
        const OFFSET_TIME: FmtSupport = FmtSupport {
            year: false,
            month: false,
            day: false,
            time: true,
            sub_second: true,
            instant: false,
            zone: true,
            calendar_printer: false,
        };
        const YEAR_MONTH: FmtSupport = FmtSupport {
            year: true,
            month: true,
            day: false,
            time: false,
            sub_second: false,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const MONTH_DAY: FmtSupport = FmtSupport {
            year: false,
            month: true,
            day: true,
            time: false,
            sub_second: false,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const YEAR_ONLY: FmtSupport = FmtSupport {
            year: true,
            month: false,
            day: false,
            time: false,
            sub_second: false,
            instant: false,
            zone: false,
            calendar_printer: false,
        };
        const MONTH_ONLY: FmtSupport = FmtSupport {
            year: false,
            month: true,
            day: false,
            time: false,
            sub_second: false,
            instant: false,
            zone: false,
            calendar_printer: false,
        };

        // `calendar_printer` is NOT derivable from the support flags:
        // `ZONED` sets every one of them and still takes the OTHER printer.
        // That is the whole reason it is a stored field, so assert it.
        assert!(FmtSupport::ALL.calendar_printer);
        assert!(!ZONED.calendar_printer);
        assert_eq!(
            (
                ZONED.year,
                ZONED.month,
                ZONED.day,
                ZONED.time,
                ZONED.sub_second,
                ZONED.instant,
                ZONED.zone
            ),
            (
                FmtSupport::ALL.year,
                FmtSupport::ALL.month,
                FmtSupport::ALL.day,
                FmtSupport::ALL.time,
                FmtSupport::ALL.sub_second,
                FmtSupport::ALL.instant,
                FmtSupport::ALL.zone
            ),
            "a ZonedDateTime supports exactly what a Calendar does; only the \
             PRINTER differs, so `calendar_printer` cannot be computed"
        );

        // G2-1, measured 2026-08-16 on HotSpot 25.0.3+9. Each row is the
        // source's complete ANSWERED set out of the 31 fields; everything not
        // listed refuses, with the character given below it. Kills a
        // "one `date` flag" implementation at every one of the five sources.
        for (name, sup, answers) in [
            ("OffsetTime", OFFSET_TIME, "HIklMSLNpzZRTr"),
            ("YearMonth", YEAR_MONTH, "CYymBbh"),
            ("MonthDay", MONTH_DAY, "mdeBbh"),
            ("Year", YEAR_ONLY, "CYy"),
            ("Month", MONTH_ONLY, "mBbh"),
        ] {
            for f in FMT_DATETIME_FIELDS.chars() {
                let got = fmt_temporal_fault_char(f, sup);
                if answers.contains(f) {
                    assert_eq!(got, None, "%t{f} must be ANSWERED on {name}");
                } else {
                    assert!(got.is_some(), "%t{f} must be REFUSED on {name}");
                }
            }
        }
        // The composites' refusal characters, which a per-type table cannot
        // produce and a single `date` flag cannot distinguish.
        for (name, sup, d, ff, c) in [
            ("OffsetTime", OFFSET_TIME, 'm', 'F', 'a'),
            ("YearMonth", YEAR_MONTH, 'd', 'd', 'a'),
            ("MonthDay", MONTH_DAY, 'y', 'F', 'a'),
            ("Year", YEAR_ONLY, 'm', 'm', 'a'),
            ("Month", MONTH_ONLY, 'd', 'F', 'a'),
        ] {
            assert_eq!(fmt_temporal_fault_char('D', sup), Some(d), "%tD on {name}");
            assert_eq!(fmt_temporal_fault_char('F', sup), Some(ff), "%tF on {name}");
            assert_eq!(fmt_temporal_fault_char('c', sup), Some(c), "%tc on {name}");
        }
        // `%tA`/`%ta`/`%tj` need a COMPLETE date, which is the derivation in
        // `FmtSupport::full_date`.
        for f in "Aaj".chars() {
            assert_eq!(fmt_temporal_fault_char(f, LOCAL_DATE), None);
            for sup in [YEAR_MONTH, MONTH_DAY, YEAR_ONLY, MONTH_ONLY, OFFSET_TIME] {
                assert_eq!(fmt_temporal_fault_char(f, sup), Some(f));
            }
        }

        // An epoch-shaped argument takes the `Calendar` printer and refuses
        // NOTHING — all 31 fields.
        for f in FMT_DATETIME_FIELDS.chars() {
            assert_eq!(
                fmt_temporal_fault_char(f, FmtSupport::ALL),
                None,
                "%t{f} must not refuse a Date/Calendar/long"
            );
            // A fully-supported `java.time` source likewise.
            assert_eq!(fmt_temporal_fault_char(f, ZONED), None, "%t{f} on Zoned");
        }

        // Instant: FOUR answers, 27 refusals.
        for f in "LNsQ".chars() {
            assert_eq!(
                fmt_temporal_fault_char(f, INSTANT),
                None,
                "%t{f} on Instant"
            );
        }
        for (f, want) in [
            ('H', 'H'),
            ('I', 'I'),
            ('k', 'k'),
            ('l', 'l'),
            ('M', 'M'),
            ('S', 'S'),
            ('p', 'p'),
            ('z', 'z'),
            ('Z', 'Z'),
            ('B', 'B'),
            ('b', 'b'),
            ('h', 'h'),
            ('A', 'A'),
            ('a', 'a'),
            ('C', 'C'),
            ('Y', 'Y'),
            ('y', 'y'),
            ('j', 'j'),
            ('m', 'm'),
            ('d', 'd'),
            ('e', 'e'),
            ('R', 'H'),
            ('T', 'H'),
            ('r', 'I'),
            ('D', 'm'),
            ('F', 'F'),
            ('c', 'a'),
        ] {
            assert_eq!(
                fmt_temporal_fault_char(f, INSTANT),
                Some(want),
                "%t{f} on Instant"
            );
        }

        // LocalDate: the date group answers, the clock group does not.
        for f in "BbhAaCYyjmdeDF".chars() {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_DATE),
                None,
                "%t{f} on LocalDate"
            );
        }
        for (f, want) in [
            ('H', 'H'),
            ('L', 'L'),
            ('N', 'N'),
            ('s', 's'),
            ('Q', 'Q'),
            ('z', 'z'),
            ('Z', 'Z'),
            ('R', 'H'),
            ('T', 'H'),
            ('r', 'I'),
            ('c', 'H'),
        ] {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_DATE),
                Some(want),
                "%t{f} on LocalDate"
            );
        }

        // LocalTime: the mirror image.
        for f in "HIklMSpLNRTr".chars() {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_TIME),
                None,
                "%t{f} on LocalTime"
            );
        }
        for (f, want) in [
            ('Y', 'Y'),
            ('b', 'b'),
            ('a', 'a'),
            ('s', 's'),
            ('Q', 'Q'),
            ('Z', 'Z'),
            ('D', 'm'),
            ('F', 'F'),
            ('c', 'a'),
        ] {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_TIME),
                Some(want),
                "%t{f} on LocalTime"
            );
        }

        // LocalDateTime: everything but the zone and the epoch.
        for f in "HIklMSpLNBbhAaCYyjmdeRTrDF".chars() {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_DATE_TIME),
                None,
                "%t{f} on LocalDateTime"
            );
        }
        for (f, want) in [('z', 'z'), ('Z', 'Z'), ('s', 's'), ('Q', 'Q'), ('c', 'Z')] {
            assert_eq!(
                fmt_temporal_fault_char(f, LOCAL_DATE_TIME),
                Some(want),
                "%t{f} on LocalDateTime"
            );
        }
    }

    /// The `%t` numbers take the locale's zero digit and NOTHING else takes
    /// anything. Rows measured on HotSpot 25 under `ar-EG`, whose zero digit
    /// is U+0660.
    ///
    /// Kills: (a) an implementation that leaves the digits ASCII, which is
    /// what this file did; (b) one that reuses `fmt_localize` and so rewrites
    /// `'.'` and `','` — `%tD` is `mm/dd/yy` and `ru`'s `%tb` is `нояб.`;
    /// (c) one that shifts the `'+'` of a `%tz` or the `':'` of a `%tT`;
    /// (d) one that shifts non-ASCII digits already present in a name.
    #[test]
    fn t_digits_take_the_locale_zero_and_nothing_else() {
        let ar = FmtSymbols {
            grouping: '\u{066C}',
            decimal: '\u{066B}',
            zero: '\u{0660}',
            // Measured: `ar-EG`'s `getMinusSign()` IS U+002D. It is spelled out
            // rather than defaulted so this row stays a MEASUREMENT of the
            // locale and not an inheritance from `FmtSymbols::default`.
            minus: '-',
        };
        assert_eq!(fmt_localize_digits("03", ar), "\u{0660}\u{0663}");
        assert_eq!(
            fmt_localize_digits("03:43:19", ar),
            "\u{0660}\u{0663}:\u{0664}\u{0663}:\u{0661}\u{0669}",
            "the ':' separators stay ASCII"
        );
        assert_eq!(
            fmt_localize_digits("+0530", ar),
            "+\u{0660}\u{0665}\u{0663}\u{0660}",
            "the %tz sign stays ASCII"
        );
        assert_eq!(
            fmt_localize_digits("11/15/23", ar),
            "\u{0661}\u{0661}/\u{0661}\u{0665}/\u{0662}\u{0663}",
            "'/' is not the grouping separator"
        );
        assert_eq!(
            fmt_localize_digits("-0044-03-15", ar),
            "-\u{0660}\u{0660}\u{0664}\u{0664}-\u{0660}\u{0663}-\u{0661}\u{0665}"
        );
        // A `DateFormatSymbols` name that ends in '.' must survive intact —
        // `fmt_localize` would have rewritten that '.' into U+066B.
        assert_eq!(
            fmt_localize_digits("\u{043D}\u{043E}\u{044F}\u{0431}.", ar),
            "\u{043D}\u{043E}\u{044F}\u{0431}."
        );
        // The overwhelmingly common case is a no-op, and must be exactly one.
        let root = FmtSymbols::default();
        assert_eq!(root.zero, '0');
        for s in ["03:43:19", "+0530", "1699999999", "нояб.", ""] {
            assert_eq!(fmt_localize_digits(s, root), s);
        }
    }

    /// `%tF`'s year: sign OUTSIDE a four-digit zero pad, and a '+' past 9999.
    /// Measured on HotSpot 25 under `Locale.ROOT`.
    ///
    /// Kills Rust's `{:04}`, which counts the sign inside the width
    /// (`-044-03-15` for 45 BC) and never emits a '+'.
    #[test]
    fn iso_year_signs_sit_outside_the_zero_pad() {
        assert_eq!(fmt_iso_year(-44, '-'), "-0044");
        assert_eq!(fmt_iso_year(-1, '-'), "-0001");
        assert_eq!(fmt_iso_year(0, '-'), "0000");
        assert_eq!(fmt_iso_year(1, '-'), "0001");
        assert_eq!(fmt_iso_year(2023, '-'), "2023");
        assert_eq!(fmt_iso_year(9999, '-'), "9999", "no sign at the boundary");
        assert_eq!(fmt_iso_year(10000, '-'), "+10000", "the '+' starts here");
        assert_eq!(fmt_iso_year(12345, '-'), "+12345");
        // Rust's own render is the mutant, and it differs on three of these.
        assert_ne!(fmt_iso_year(-44, '-'), format!("{:04}", -44));
        assert_ne!(fmt_iso_year(12345, '-'), format!("{:04}", 12345));
    }

    /// F38 — `%tF`'s minus is the LOCALE'S, and it is the only localized minus
    /// in `java.util.Formatter`.
    ///
    /// Measured on HotSpot 25 under `lt-LT` (`getMinusSign()` = U+2212), dumped
    /// as UTF-16 code units:
    ///
    /// | conversion | argument | units |
    /// |---|---|---|
    /// | `%tF` | `LocalDate.of(-44,3,15)` | `2212 0030 0030 0034 0034 002D …` |
    /// | `%tF` | `LocalDate.of(12345,3,4)` | `002B 0031 …` — ASCII '+' |
    /// | `%tF` | same, `(Locale) null` | `002D 0030 0030 0034 0034 …` |
    /// | `%d` | `-5` | `002D 0035` |
    ///
    /// Mutants this kills:
    ///  * an ASCII `'-'` — the U+2212 row;
    ///  * localizing the `'+'` too — the 12345 row (the JDK writes that one as
    ///    a literal in the same block);
    ///  * localizing the DATE SEPARATORS as well, which a
    ///    `replace('-', minus)` over the composed field would do — the
    ///    `%tF`-shaped row below;
    ///  * routing the explicit-null locale through a lookup —
    ///    [`FmtSymbols::default`]'s minus must stay U+002D.
    #[test]
    fn iso_year_minus_is_the_locales_and_nothing_else_is() {
        assert_eq!(fmt_iso_year(-44, '\u{2212}'), "\u{2212}0044");
        assert_eq!(fmt_iso_year(-1, '\u{2212}'), "\u{2212}0001");
        // The '+' is a literal even when the minus is not.
        assert_eq!(fmt_iso_year(12345, '\u{2212}'), "+12345");
        // A non-negative year consults no symbol at all.
        assert_eq!(fmt_iso_year(2023, '\u{2212}'), "2023");
        assert_eq!(fmt_iso_year(0, '\u{2212}'), "0000");

        // The whole `%tF` field: exactly ONE unit changes, and the two date
        // separators stay ASCII.
        let field = format!("{}-{:02}-{:02}", fmt_iso_year(-44, '\u{2212}'), 3, 15);
        assert_eq!(field, "\u{2212}0044-03-15");
        assert_eq!(field.matches('-').count(), 2, "separators stay ASCII");

        // An explicit null locale is `getMinusSign(null) == '-'`.
        assert_eq!(FmtSymbols::default().minus, '-');
        // …and the digit substitution pass leaves the sign alone, whichever it
        // is. `ar-EG`'s zero digit with a U+2212 minus is not a real locale
        // pair, which is the point: the two symbols are independent.
        let ar = FmtSymbols {
            zero: '\u{0660}',
            ..FmtSymbols::default()
        };
        assert_eq!(
            fmt_localize_digits("\u{2212}0044-03-15", ar),
            "\u{2212}\u{0660}\u{0660}\u{0664}\u{0664}-\u{0660}\u{0663}-\u{0661}\u{0665}"
        );
    }

    /// F38 — `%tY`/`%ty`/`%tC`, and the year slots of `%tD` and `%tc`, render
    /// the ERA-RELATIVE year; only `%tF`'s `TemporalAccessor` arm renders the
    /// proleptic one.
    ///
    /// Measured on HotSpot 25, `Locale.ROOT`, `-Duser.timezone=Asia/Kolkata`.
    /// The `Calendar` rows are the SAME INSTANT as the `LocalDate` rows
    /// (`-63549548877000L`), which is what makes the `%tF` column the only
    /// disagreement between the two printers.
    ///
    /// Mutants this kills:
    ///  * rendering the proleptic year — `%tY` of 45 BC would be `-044`;
    ///  * `-year` instead of `1 - year` — proleptic 0 is 1 BC, not 0 BC, so
    ///    `LocalDate.of(0,…)` is `0001` and `LocalDate.of(-1,…)` is `0002`;
    ///  * `rem_euclid(100)` on the proleptic year for `%ty`/`%tD` — that gives
    ///    `56`, not `45`;
    ///  * applying the era mapping to `%tF` as well — 45 BC is `-0044-03-15`
    ///    on the `java.time` side;
    ///  * applying `fmt_iso_year` to the `Calendar` printer — that puts a '+'
    ///    on `12345-03-04`, which the JDK does not write there, and a '-' on a
    ///    BC date whose `Calendar.YEAR` is positive.
    #[test]
    fn year_fields_are_era_relative_on_both_printers() {
        // The mapping itself, against `ChronoField.YEAR_OF_ERA` as measured.
        fn year_of_era(year: i64) -> i64 {
            if year <= 0 {
                1 - year
            } else {
                year
            }
        }
        for (proleptic, want) in [(-44i64, 45i64), (-1, 2), (0, 1), (1, 1), (12345, 12345)] {
            assert_eq!(year_of_era(proleptic), want, "YEAR_OF_ERA of {proleptic}");
        }
        assert_eq!(format!("{:04}", year_of_era(-44)), "0045", "%tY of 45 BC");
        assert_eq!(format!("{:02}", year_of_era(-44) % 100), "45", "%ty");
        assert_eq!(format!("{:02}", year_of_era(-44) / 100), "00", "%tC");
        assert_eq!(format!("{:02}", year_of_era(12345) / 100), "123", "%tC AD");
        assert_eq!(format!("{:02}", year_of_era(12345) % 100), "45", "%ty AD");

        // `%tD`'s year slot is `%ty`'s, not the proleptic remainder.
        assert_eq!(
            format!("{:02}/{:02}/{:02}", 3, 15, year_of_era(-44) % 100),
            "03/15/45"
        );
        assert_ne!(
            format!("{:02}", (-44i64).rem_euclid(100)),
            "45",
            "the old proleptic remainder was 56"
        );

        // `%tF`: the two printers, at the SAME instant.
        assert_eq!(
            format!("{}-{:02}-{:02}", fmt_iso_year(-44, '-'), 3, 15),
            "-0044-03-15",
            "TemporalAccessor arm: proleptic, signed"
        );
        assert_eq!(
            format!("{:04}-{:02}-{:02}", year_of_era(-44), 3, 15),
            "0045-03-15",
            "Calendar arm: era-relative, unsigned"
        );
        assert_eq!(
            format!("{:04}-{:02}-{:02}", year_of_era(12345), 3, 4),
            "12345-03-04",
            "Calendar arm writes no '+' past 9999"
        );
        assert_ne!(
            format!("{}-{:02}-{:02}", fmt_iso_year(12345, '-'), 3, 4),
            "12345-03-04",
            "…while the TemporalAccessor arm does"
        );
    }

    /// `%n` is the platform separator, and the two `%n` arms (the bare
    /// short circuit and the decorated one, ~440 lines apart) must agree.
    #[test]
    fn line_separator_units_are_the_platform_separator() {
        let units = fmt_line_separator_units();
        if cfg!(windows) {
            assert_eq!(units, vec![0x000D, 0x000A]);
        } else {
            assert_eq!(units, vec![0x000A]);
        }
    }

    /// The parser's per-UNIT `char` view must be index-aligned with the format
    /// string's units, and must not turn any character the parser tests for
    /// into something else.
    ///
    /// Kills the `str::chars()` collect this replaced: there a surrogate PAIR
    /// is ONE `char`, so every index after it is off by one and the literal
    /// push emits the wrong unit.
    #[test]
    fn parser_char_view_is_index_aligned_with_units() {
        // Rust cannot put a lone surrogate in a literal, so it is appended.
        let units: Vec<u16> = "%5.1s\u{1F600}x"
            .encode_utf16()
            .chain(std::iter::once(0xD800))
            .collect();
        let chars: Vec<char> = units
            .iter()
            .map(|&u| char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'))
            .collect();
        assert_eq!(chars.len(), units.len(), "one char per code UNIT");
        assert_eq!(chars[0], '%');
        assert_eq!(chars[1], '5');
        assert_eq!(chars[2], '.');
        assert_eq!(chars[3], '1');
        assert_eq!(chars[4], 's');
        // The emoji occupies TWO slots now, so 'x' sits at 7, not 6.
        assert_eq!(chars[7], 'x');
        assert_eq!(units[7], 0x0078);
        // A surrogate — half of a pair or lone — is none of the characters the
        // parser tests for, so it falls through to the literal branch.
        for (i, c) in chars.iter().enumerate() {
            if (0xD800..=0xDFFF).contains(&units[i]) {
                assert_eq!(*c, '\u{FFFD}');
                assert!(!c.is_ascii_digit() && !c.is_ascii_alphabetic() && *c != '%');
            }
        }
    }

    /// `has_unpaired_surrogate` is the gate that decides whether a result takes
    /// the lossless units path, so a false negative silently restores the bug.
    #[test]
    fn unpaired_surrogate_gate_is_exact() {
        assert!(!has_unpaired_surrogate(&[0x0061, 0x0062]));
        let pair: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        assert!(!has_unpaired_surrogate(&pair));
        assert!(has_unpaired_surrogate(&[0xD800]));
        assert!(has_unpaired_surrogate(&[0xDC00]));
        assert!(has_unpaired_surrogate(&[0xD800, 0x0061]));
        // A LOW followed by a HIGH is two lone surrogates, not a pair.
        assert!(has_unpaired_surrogate(&[0xDC00, 0xD800]));
        // A trailing high surrogate with nothing after it.
        assert!(has_unpaired_surrogate(&[0x0061, 0xD83D]));
    }
}

/// G26 — the builder family's TEXT-carrying entry points.
///
/// Two defects, one file, measured on HotSpot 25.0.3+9-LTS before the fix
/// (`docs/known-issues/jdk-only/G26-1-four-families-of-RJdkIntrinsics3-20260817.md`):
///
///   * every `String`/`Object`/`CharSequence` argument was read through a Rust
///     `str`, which cannot hold an unpaired surrogate — 28 divergent rows of
///     66 in `scratchpad/g26/G26Builder.java`;
///   * `insert(int, CharSequence[, int, int])` had no native at all, so real
///     bytecode ran against the synthetic layout, truncated silently, and left
///     the receiver in a state where the next `charAt` ABORTED the VM.
///
/// These tests assert the units, not a `read_string` round trip: a test written
/// through `read_string` passes on the broken code, because that is the very
/// conversion that loses the surrogate.
#[cfg(test)]
mod g26_builder_text_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    // `ObjectRef` is NOT re-exported by the parent's `use` list — it is spelled
    // `cratonvm_types::ObjectRef` at every signature there — so `use super::*`
    // does not bring it in.
    use cratonvm_types::{ArrayElementType, ObjectRef};

    /// A `java.lang.String` holding RAW code units, including ones no Rust
    /// `str` can carry. `create_string` takes a `&str` and so cannot build the
    /// input this family is about; the char[] is written directly, which is
    /// the same shape `create_string` produces.
    fn mock_string_of_units(ctx: &mut MockNativeContext, units: &[u16]) -> ObjectRef {
        let s = ctx.create_string("");
        let arr = ctx.new_array(ArrayElementType::Char, units.len());
        for (i, &u) in units.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(i32::from(u)));
        }
        ctx.set_field(s, 0, Value::Object(Some(arr)));
        s
    }

    fn fresh_builder(ctx: &mut MockNativeContext) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        let sb = ctx.alloc_object(cid, 4);
        native_sb_init_default(ctx, &[Value::Object(Some(sb))]).unwrap();
        sb
    }

    fn units_of(ctx: &MockNativeContext, sb: ObjectRef) -> Vec<u16> {
        sb_read_chars(ctx, sb)
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn failure_class(
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
                cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { .. } => "ioobe",
                _ => "other-runtime",
            },
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
                let class_id = ctx.class_id_of_object(*obj);
                match ctx.class_name_of_id(class_id).unwrap_or_default().as_str() {
                    "java/lang/StringIndexOutOfBoundsException" => "sioobe",
                    "java/lang/IndexOutOfBoundsException" => "ioobe",
                    _ => "other-thrown",
                }
            }
            _ => "other-failed",
        }
    }

    fn failure_message(e: &cratonvm_types::error::MethodCallFailed) -> String {
        match e {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(re),
            ) => match re {
                cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                    message,
                    ..
                } => message.clone().unwrap_or_default(),
                cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { message } => {
                    message.clone().unwrap_or_default()
                }
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    /// The whole point of the helper: a lone LOW surrogate survives the read.
    /// `read_string` on the same object answers U+FFFD, and asserting that
    /// difference here is what stops the fix being undone by a "simplification"
    /// back to `read_string`.
    #[test]
    fn read_string_chars_carries_what_read_string_cannot() {
        let mut ctx = mock_ctx();
        let s = mock_string_of_units(&mut ctx, &[0x0070, 0xDC00, 0x0071]);
        assert_eq!(read_string_chars(&ctx, s), vec![0x0070, 0xDC00, 0x0071]);
        // The lossy twin, for contrast: this is what every entry point below
        // used to call.
        assert_eq!(
            utf16(&ctx.read_string(s).unwrap()),
            vec![0x0070, 0xFFFD, 0x0071],
            "read_string is expected to be lossy — that is why it was replaced"
        );
    }

    /// `new StringBuilder(s)` — MEASURED rows c1/c2/c3/c5/c7.
    #[test]
    fn init_from_string_keeps_an_unpaired_surrogate() {
        let mut ctx = mock_ctx();
        let s = mock_string_of_units(&mut ctx, &[0xDC00]);
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        let sb = ctx.alloc_object(cid, 4);
        native_sb_init_string(&mut ctx, &[Value::Object(Some(sb)), Value::Object(Some(s))])
            .unwrap();
        assert_eq!(units_of(&ctx, sb), vec![0xDC00]);

        // A well-formed pair is untouched — the control that would catch a
        // mutant that broke ordinary text on the way to fixing this row.
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        let sb2 = ctx.alloc_object(cid, 4);
        let pair = mock_string_of_units(&mut ctx, &[0xD83D, 0xDE00]);
        native_sb_init_string(
            &mut ctx,
            &[Value::Object(Some(sb2)), Value::Object(Some(pair))],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb2), vec![0xD83D, 0xDE00]);
    }

    /// `sb.append(s)` / `sb.insert(i, s)` / `sb.replace(a, b, s)` — MEASURED
    /// rows a1/a2/a12, i1, r1. One test for the three because they are the same
    /// defect with the same fix, and any one of them passing alone would not
    /// show the other two had been touched.
    #[test]
    fn append_insert_replace_keep_an_unpaired_surrogate() {
        let mut ctx = mock_ctx();
        let lone = mock_string_of_units(&mut ctx, &[0xD800]);

        let sb = fresh_builder(&mut ctx);
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(lone))],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb), vec![0xD800], "append(String)");

        let sb2 = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb2)), Value::Object(Some(xy))],
        )
        .unwrap();
        native_sb_insert_string(
            &mut ctx,
            &[
                Value::Object(Some(sb2)),
                Value::Int(1),
                Value::Object(Some(lone)),
            ],
        )
        .unwrap();
        assert_eq!(
            units_of(&ctx, sb2),
            vec![0x0078, 0xD800, 0x0079],
            "insert(int, String)"
        );

        let sb3 = fresh_builder(&mut ctx);
        let xyz = ctx.create_string("xyz");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb3)), Value::Object(Some(xyz))],
        )
        .unwrap();
        native_sb_replace(
            &mut ctx,
            &[
                Value::Object(Some(sb3)),
                Value::Int(1),
                Value::Int(2),
                Value::Object(Some(lone)),
            ],
        )
        .unwrap();
        assert_eq!(
            units_of(&ctx, sb3),
            vec![0x0078, 0xD800, 0x007A],
            "replace(int, int, String)"
        );
    }

    /// `append(String)` still substitutes the four characters `"null"` for a
    /// null argument (`AbstractStringBuilder.appendNull`). The units rewrite
    /// runs through the same `match`, so the null arm is asserted rather than
    /// assumed.
    #[test]
    fn append_string_null_is_still_the_null_literal() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        native_sb_append_string(&mut ctx, &[Value::Object(Some(sb)), Value::Object(None)]).unwrap();
        assert_eq!(units_of(&ctx, sb), utf16("null"));
    }

    /// `insert(int, Object)` must ASK the object. It read `ctx.read_string`,
    /// which answers `None` for everything that is not a `java.lang.String`,
    /// so every other object was inserted as the four characters `"null"`.
    ///
    /// MEASURED on HotSpot 25.0.3+9-LTS: `new StringBuilder("ab").insert(0,
    /// Integer.valueOf(7))` is `7ab`; this VM answered `nullab`.
    ///
    /// The mock's `Integer` is a one-field object of that class, which is the
    /// shape `invoke_to_string_units_opt`'s wrapper fast path recognises — the
    /// same shape the sibling `append(Object)` has always gone through.
    #[test]
    fn insert_object_asks_the_object_instead_of_answering_null() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        let ab = ctx.create_string("ab");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(ab))],
        )
        .unwrap();

        let int_cid = ctx.ensure_class_initialized("java/lang/Integer").unwrap();
        let boxed = ctx.alloc_object(int_cid, 1);
        ctx.set_field(boxed, 0, Value::Int(7));

        native_sb_insert_object(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Object(Some(boxed)),
            ],
        )
        .unwrap();
        assert_eq!(
            units_of(&ctx, sb),
            utf16("7ab"),
            "insert(0, Integer.valueOf(7)) must be 7ab, not nullab"
        );
    }

    /// A `String` argument to `insert(int, Object)` keeps its raw units —
    /// MEASURED row i2, and the arm the fix above must not have traded away.
    #[test]
    fn insert_object_keeps_an_unpaired_surrogate_from_a_string() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        let lone = mock_string_of_units(&mut ctx, &[0xDC00]);
        native_sb_insert_object(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(0),
                Value::Object(Some(lone)),
            ],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb), vec![0xDC00]);
    }

    /// `insert(int, CharSequence)` had NO native: real bytecode ran against the
    /// synthetic layout and `new StringBuilder("xy").insert(1, (CharSequence)
    /// "AB")` answered `xA` where HotSpot answers `xABy`.
    #[test]
    fn insert_charsequence_inserts_rather_than_overwrites() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(xy))],
        )
        .unwrap();
        let ab = ctx.create_string("AB");
        native_sb_insert_charsequence(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(1),
                Value::Object(Some(ab)),
            ],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb), utf16("xABy"));
    }

    /// A null `CharSequence` becomes the four characters `"null"` — HotSpot
    /// answers `xnully`, this VM answered `xn`.
    #[test]
    fn insert_charsequence_null_is_the_null_literal() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(xy))],
        )
        .unwrap();
        native_sb_insert_charsequence(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(1), Value::Object(None)],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb), utf16("xnully"));
    }

    /// The four-argument form's window, and the empty window that must be a
    /// no-op rather than a refusal — `insert(1, "AB", 1, 1)` answers `xy`.
    #[test]
    fn insert_charsequence_range_takes_the_window() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(xy))],
        )
        .unwrap();
        let abc = ctx.create_string("ABC");
        native_sb_insert_charsequence_range(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(1),
                Value::Object(Some(abc)),
                Value::Int(1),
                Value::Int(3),
            ],
        )
        .unwrap();
        assert_eq!(units_of(&ctx, sb), utf16("xBCy"));

        let sb2 = fresh_builder(&mut ctx);
        let xy2 = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb2)), Value::Object(Some(xy2))],
        )
        .unwrap();
        let ab = ctx.create_string("AB");
        native_sb_insert_charsequence_range(
            &mut ctx,
            &[
                Value::Object(Some(sb2)),
                Value::Int(1),
                Value::Object(Some(ab)),
                Value::Int(1),
                Value::Int(1),
            ],
        )
        .unwrap();
        assert_eq!(
            units_of(&ctx, sb2),
            utf16("xy"),
            "an empty window is legal and changes nothing"
        );
    }

    /// The two refusals are DIFFERENT classes with DIFFERENT messages, and the
    /// offset check runs FIRST. Every string here is TRANSCRIBED from HotSpot
    /// 25.0.3+9-LTS (`scratchpad/g26/G26InsCs.java`), not derived: the javadoc
    /// names only the classes.
    ///
    /// The `insert(9, "AB", 0, 9)` row is the ordering discriminator — BOTH
    /// arguments are out of range and HotSpot answers the offset one. A body
    /// that checked the range first would pass every other row here.
    #[test]
    fn insert_charsequence_range_refusals_are_transcribed() {
        let mut ctx = mock_ctx();
        let ab = ctx.create_string("AB");

        // Offset out of range -> StringIndexOutOfBoundsException.
        for (offset, expected) in [
            (5, "Range [5, 2) out of bounds for length 2"),
            (-1, "Range [-1, 2) out of bounds for length 2"),
        ] {
            let sb = fresh_builder(&mut ctx);
            let xy = ctx.create_string("xy");
            native_sb_append_string(
                &mut ctx,
                &[Value::Object(Some(sb)), Value::Object(Some(xy))],
            )
            .unwrap();
            let e = native_sb_insert_charsequence_range(
                &mut ctx,
                &[
                    Value::Object(Some(sb)),
                    Value::Int(offset),
                    Value::Object(Some(ab)),
                    Value::Int(0),
                    Value::Int(1),
                ],
            )
            .expect_err("an offset out of range must be refused");
            assert_eq!(failure_class(&ctx, &e), "sioobe", "offset {offset}");
            assert_eq!(failure_message(&e), expected, "offset {offset}");
            // A refused insert must not be a partial one.
            assert_eq!(units_of(&ctx, sb), utf16("xy"));
        }

        // Window out of range -> the PLAIN IndexOutOfBoundsException.
        for (start, end, expected) in [
            (0, 9, "Range [0, 9) out of bounds for length 2"),
            (-1, 1, "Range [-1, 1) out of bounds for length 2"),
            (2, 1, "Range [2, 1) out of bounds for length 2"),
        ] {
            let sb = fresh_builder(&mut ctx);
            let xy = ctx.create_string("xy");
            native_sb_append_string(
                &mut ctx,
                &[Value::Object(Some(sb)), Value::Object(Some(xy))],
            )
            .unwrap();
            let e = native_sb_insert_charsequence_range(
                &mut ctx,
                &[
                    Value::Object(Some(sb)),
                    Value::Int(1),
                    Value::Object(Some(ab)),
                    Value::Int(start),
                    Value::Int(end),
                ],
            )
            .expect_err("a window out of range must be refused");
            assert_eq!(failure_class(&ctx, &e), "ioobe", "[{start}, {end})");
            assert_eq!(failure_message(&e), expected, "[{start}, {end})");
        }

        // BOTH wrong: the OFFSET check wins.
        let sb = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(xy))],
        )
        .unwrap();
        let e = native_sb_insert_charsequence_range(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(9),
                Value::Object(Some(ab)),
                Value::Int(0),
                Value::Int(9),
            ],
        )
        .expect_err("both out of range must be refused");
        assert_eq!(failure_class(&ctx, &e), "sioobe");
        assert_eq!(
            failure_message(&e),
            "Range [9, 2) out of bounds for length 2"
        );

        // A null sequence is checked against the LENGTH OF "null", i.e. 4 —
        // so `[0, 9)` is refused and reports 4, while `[0, 4)` succeeds.
        let sb = fresh_builder(&mut ctx);
        let xy = ctx.create_string("xy");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Object(Some(xy))],
        )
        .unwrap();
        let e = native_sb_insert_charsequence_range(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Int(1),
                Value::Object(None),
                Value::Int(0),
                Value::Int(9),
            ],
        )
        .expect_err("a window past the \"null\" literal must be refused");
        assert_eq!(failure_class(&ctx, &e), "ioobe");
        assert_eq!(
            failure_message(&e),
            "Range [0, 9) out of bounds for length 4"
        );
    }

    /// `charAt` on a builder whose slot 0 is not a `char[]` used to be
    /// `buf.unwrap()`, and a Rust panic is not a Java throwable: it terminated
    /// the VM instead of unwinding. MEASURED reproducer: `insert(1,
    /// (CharSequence) s)` corrupted the layout, and the next `charAt` printed
    /// `called Option::unwrap() on a None value` and killed the process.
    ///
    /// The registration gap is closed, so nothing in the suite reaches this
    /// arm any more — which is exactly why it needs a test that reaches it
    /// directly.
    #[test]
    fn char_at_refuses_an_unreadable_buffer_instead_of_aborting() {
        let mut ctx = mock_ctx();
        let sb = fresh_builder(&mut ctx);
        // A count that claims content, with no backing array behind it.
        ctx.set_field(sb, 0, Value::Object(None));
        sb_set_count(&mut ctx, sb, 3);
        let e = native_sb_char_at(&mut ctx, &[Value::Object(Some(sb)), Value::Int(0)])
            .expect_err("an unreadable buffer must throw, not panic");
        assert_eq!(failure_class(&ctx, &e), "sioobe");

        // The ordinary bounds refusal is unchanged, message included — it is
        // TRANSCRIBED from HotSpot and matched before this change.
        let sb2 = fresh_builder(&mut ctx);
        let ab = ctx.create_string("ab");
        native_sb_append_string(
            &mut ctx,
            &[Value::Object(Some(sb2)), Value::Object(Some(ab))],
        )
        .unwrap();
        let e = native_sb_char_at(&mut ctx, &[Value::Object(Some(sb2)), Value::Int(5)])
            .expect_err("index 5 of a 2-char builder must be refused");
        assert_eq!(failure_class(&ctx, &e), "sioobe");
        assert_eq!(failure_message(&e), "Index 5 out of bounds for length 2");
    }

    /// `charsequence_fast_units` is the one place four `append`/`repeat`
    /// overloads read their text, so its `String` arm is the fix for all of
    /// them at once. Asserting through it, rather than through each caller,
    /// is what makes the single-home claim checkable.
    #[test]
    fn charsequence_fast_units_reads_raw_units_for_a_string() {
        let mut ctx = mock_ctx();
        let s = mock_string_of_units(&mut ctx, &[0x0061, 0xD800, 0x0062]);
        let got = charsequence_fast_units(&mut ctx, s)
            .expect("no Java is re-entered for a String")
            .expect("a String is a fast-path shape");
        assert_eq!(got, vec![0x0061, 0xD800, 0x0062]);
    }
}
