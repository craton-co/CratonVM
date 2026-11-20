#[allow(unused_imports)]
use super::*;


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
    pub static STRING_CHARS_SCRATCH_A: std::cell::RefCell<Vec<u16>> =
        std::cell::RefCell::new(Vec::with_capacity(64));
    pub static STRING_CHARS_SCRATCH_B: std::cell::RefCell<Vec<u16>> =
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
pub fn decode_string_chars(
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
        // Bulk first: one `memcpy` per 512 bytes where the loop below costs two
        // validated accessor calls (~40 ns each) per char. Every String native that
        // reads a whole string comes through here (a 150-char URL decoded on each
        // of the four `regionMatches` of a `new URL(context, spec)`: ~9 us).
        let mut buf = [0u8; 512];
        let mut c = 0usize;
        while c < char_count {
            let want = (char_count - c).min(buf.len() / 2);
            if ctx.read_byte_array_into(arr, c * 2, &mut buf[..want * 2]) != want * 2 {
                break;
            }
            dst.extend(
                buf[..want * 2]
                    .chunks_exact(2)
                    .map(|p| (u16::from(p[1]) << 8) | u16::from(p[0])),
            );
            c += want;
        }
        // What the bulk read could not copy (an array it cannot read flat).
        for c in c..char_count {
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
        // Bulk first, as for UTF-16 above.
        let mut buf = [0u8; 512];
        let mut i = 0usize;
        while i < raw_len {
            let want = (raw_len - i).min(buf.len());
            if ctx.read_byte_array_into(arr, i, &mut buf[..want]) != want {
                break;
            }
            dst.extend(buf[..want].iter().map(|&b| u16::from(b)));
            i += want;
        }
        for i in i..raw_len {
            let ch = match ctx.get_array_element(arr, i) {
                Value::Int(v) => (v & 0xff) as u16,
                _ => 0,
            };
            dst.push(ch);
        }
    }
}

/// O(1) access to one UTF-16 code unit of a String, by index — the per-unit
/// rule of [`decode_string_chars`], for a caller that needs one or two units
/// and must not pay for decoding the rest.
pub struct StringCodeUnits {
    pub arr: Option<cratonvm_types::ObjectRef>,
    pub layout: StringUnitLayout,
    pub len: usize,
}

#[derive(Clone, Copy)]
pub enum StringUnitLayout {
    /// Legacy `char[]` value.
    Chars,
    /// Compact `byte[]`, coder UTF-16: two bytes per unit, low byte first.
    Utf16,
    /// Compact `byte[]`, coder LATIN-1: one byte per unit, zero-extended.
    Latin1,
}

impl StringCodeUnits {
    pub fn of(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> Self {
        let Some((arr, raw_len)) = string_char_array(ctx, obj) else {
            return Self {
                arr: None,
                layout: StringUnitLayout::Chars,
                len: 0,
            };
        };
        let is_byte_array = matches!(
            ctx.heap_element_type_of(arr),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean,
        );
        let (layout, len) = if !is_byte_array {
            (StringUnitLayout::Chars, raw_len)
        } else if matches!(ctx.get_field(obj, 1), Value::Int(1)) {
            (StringUnitLayout::Utf16, raw_len / 2)
        } else {
            (StringUnitLayout::Latin1, raw_len)
        };
        Self {
            arr: Some(arr),
            layout,
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The unit at `i`; `i < self.len()` is the caller's to check.
    pub fn at(&self, ctx: &dyn NativeContext, i: usize) -> u16 {
        let Some(arr) = self.arr else {
            return 0;
        };
        let byte = |j: usize| match ctx.get_array_element(arr, j) {
            Value::Int(v) => (v as u8) as u16,
            _ => 0,
        };
        match self.layout {
            StringUnitLayout::Utf16 => (byte(i * 2 + 1) << 8) | byte(i * 2),
            StringUnitLayout::Latin1 => byte(i),
            StringUnitLayout::Chars => {
                let mut one = [0u16; 1];
                if ctx.read_char_array_into(arr, i, &mut one) == 1 {
                    one[0]
                } else {
                    0
                }
            }
        }
    }
}

/// Fill the given `Vec<u16>` with the characters of the String object's
/// underlying value (char[] or compact-string byte[]). The vector is
/// cleared first.
pub fn fill_string_chars(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef, dst: &mut Vec<u16>) {
    decode_string_chars(ctx, obj, dst);
}

/// Run `f` with the thread-local scratch buffer A populated from `obj`.
/// The buffer is reused across calls — no allocation on the hot path
/// once it has grown to the steady-state size.
pub fn with_string_chars_scratch<R>(
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
pub fn with_two_string_chars_scratches<R>(
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

pub fn register_string_builder_natives(registry: &mut NativeMethodRegistry, class: &str) {
    registry.register(class, "<init>", "()V", native_sb_init_default, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "<init>",
        "(Ljava/lang/String;)V",
        native_sb_init_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sb_init_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "<init>", "(I)V", native_sb_init_capacity, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/String;)L{class};"),
        native_sb_append_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/StringBuffer;)L{class};"),
        native_sb_append_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;",
        native_sb_append_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "appendNull",
        "()Ljava/lang/AbstractStringBuilder;",
        native_sb_append_null, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(I)L{class};"),
        native_sb_append_int, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(C)L{class};"),
        native_sb_append_char, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        "(C)Ljava/lang/Appendable;",
        native_sb_append_char, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `appendCodePoint(I)` is registered ONCE, below, next to the code-point
    // family it belongs with. A second registration used to stand here, 113
    // lines earlier in this same function, pointing at
    // `native_sb_append_codepoint`; `register()` is last-write-wins, so it
    // never owned the slot. MEASURED from
    // `--dump-native-registry --explain-jdk-only --real-jdk`:
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
        native_sb_repeat_codepoint, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "repeat",
        "(CI)Ljava/lang/AbstractStringBuilder;",
        native_sb_repeat_codepoint, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("([CII)L{class};"),
        native_sb_append_char_array_off_len, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("([C)L{class};"),
        native_sb_append_char_array, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(Z)L{class};"),
        native_sb_append_boolean, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(J)L{class};"),
        native_sb_append_long, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(D)L{class};"),
        native_sb_append_double, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(F)L{class};"),
        native_sb_append_float, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/Object;)L{class};"),
        native_sb_append_object, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // C36: intercept the (CharSequence, int, int) variants used by
    // Formatter internals. Real JDK bytecode of this method reads slot
    // 2 (count) from our synthetic StringBuilder layout and blows up
    // with a bogus capacity request.
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/CharSequence;II)L{class};"),
        native_sb_append_charsequence_off_len, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
        native_sb_append_charsequence_off_len, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        &format!("(Ljava/lang/CharSequence;)L{class};"),
        native_sb_append_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
        native_sb_append_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
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
        native_sb_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "length", "()I", native_sb_length, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "charAt", "(I)C", native_sb_char_at, cratonvm_native_api::NativeKind::SyntheticStub);
    // codePointAt/codePointBefore/codePointCount/appendCodePoint: MUST be
    // native for the same reason as getValue/getCoder below — unregistered,
    // real JDK `AbstractStringBuilder` bytecode operates on the compact
    // `byte[] value` + `byte coder` layout, which CratonVM's synthetic
    // `char[]`-backed StringBuilder does not have. See the doc comments on
    // `native_sb_code_point_at` / `native_sb_append_code_point`.
    registry.register(class, "codePointAt", "(I)I", native_sb_code_point_at, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "codePointBefore",
        "(I)I",
        native_sb_code_point_before, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "codePointCount", "(II)I", native_sb_code_point_count, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "appendCodePoint",
        &format!("(I)L{class};"),
        native_sb_append_code_point, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // BUG-TC0622: real-JDK bytecode (String.nonSyncContentEquals, reached via
    // String.contentEquals(CharSequence)) reads the builder's value/coder
    // directly. Our synthetic char[]+count layout has no compact byte[]/coder,
    // so without these natives getCoder() returns `count` and getValue() hands
    // back the char[] mis-typed as a byte[], crashing in StringUTF16.contentEquals.
    registry.register(class, "getValue", "()[B", native_sb_get_value, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "getCoder", "()B", native_sb_get_coder, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "getChars", "(II[CI)V", native_sb_get_chars, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "reverse", &format!("()L{class};"), native_sb_reverse, cratonvm_native_api::NativeKind::SyntheticStub);

    // --- Mutation methods ---
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/String;)L{class};"),
        native_sb_insert_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(IC)L{class};"),
        native_sb_insert_char, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(II)L{class};"),
        native_sb_insert_int, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/Object;)L{class};"),
        native_sb_insert_object, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(I[CII)L{class};"),
        native_sb_insert_char_array_off_len, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(I[C)L{class};"),
        native_sb_insert_char_array, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // The four scalar `insert` overloads that had NO native. Without them real
    // `AbstractStringBuilder.insert` bytecode ran against the synthetic
    // char[]/count layout — the layout mismatch every neighbour above exists to
    // prevent. See `native_sb_insert_boolean`'s doc comment and
    // docs/feature-designs/native-dispatch-architecture.md.
    registry.register(
        class,
        "insert",
        &format!("(IZ)L{class};"),
        native_sb_insert_boolean, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(IJ)L{class};"),
        native_sb_insert_long, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(IF)L{class};"),
        native_sb_insert_float, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(ID)L{class};"),
        native_sb_insert_double, cratonvm_native_api::NativeKind::SyntheticStub,
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
        native_sb_insert_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "insert",
        &format!("(ILjava/lang/CharSequence;II)L{class};"),
        native_sb_insert_charsequence_range, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "delete", &format!("(II)L{class};"), native_sb_delete, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "deleteCharAt",
        &format!("(I)L{class};"),
        native_sb_delete_char_at, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "replace",
        &format!("(IILjava/lang/String;)L{class};"),
        native_sb_replace, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "setCharAt", "(IC)V", native_sb_set_char_at, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "setLength", "(I)V", native_sb_set_length, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        class,
        "indexOf",
        "(Ljava/lang/String;)I",
        native_sb_index_of, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "indexOf",
        "(Ljava/lang/String;I)I",
        native_sb_index_of_from, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "lastIndexOf",
        "(Ljava/lang/String;)I",
        native_sb_last_index_of, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "lastIndexOf",
        "(Ljava/lang/String;I)I",
        native_sb_last_index_of_from, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "substring",
        "(I)Ljava/lang/String;",
        native_sb_substring, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        class,
        "substring",
        "(II)Ljava/lang/String;",
        native_sb_substring_range, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(class, "capacity", "()I", native_sb_capacity, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "ensureCapacity", "(I)V", native_sb_ensure_cap, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(class, "trimToSize", "()V", native_sb_trim_to_size, cratonvm_native_api::NativeKind::SyntheticStub);
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
        native_sb_repeat_charsequence, cratonvm_native_api::NativeKind::SyntheticStub,
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
pub fn string_char_array(
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
pub fn string_char_count(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> usize {
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

pub fn native_string_intern(
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

    // `ctx.intern_string` deduplicates against the VM's `shared.string_pool`
    // (returns the existing `ObjectRef` on a hit), so for already-interned
    // content this is a single hashmap lookup + the unavoidable `read_string`
    // decode. gcg g2/w: the content used to be run through the process-wide
    // `cratonvm_types::intern_arc` pool first, "to save an allocation per cold
    // call"; it saved none (`read_string` has already allocated the text) and
    // kept every interned text in Rust memory for the life of the PROCESS
    // (`gcg-g2w-string-table-review-findings-FIXED-20261012.md` item 1). Removed.
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
    //
    // nio20 wave 6, L5 (the `--compatible` locale census: 109 061 `intern()`
    // calls for 10 000 provider ops, `BaseLocale` / `LocaleUtils` interning
    // every subtag): the surrogate screen used to run FIRST, on every call,
    // through `read_string_chars` -- a fresh `Vec<u16>` filled by one checked
    // `get_array_element` per byte (two per UTF-16 unit) -- and then
    // `read_string` decoded the same value array a second time, in bulk. Now
    // the bulk decode runs first and the unit read runs only when its text
    // holds U+FFFD. Same answers: the VM's decode is `from_utf16_lossy` (LATIN-1
    // has no surrogates), so an unpaired surrogate ALWAYS shows up as U+FFFD,
    // and a text without one had none; a receiver whose text did not decode
    // (`None`) takes the unit read too, as every receiver did before.
    let decoded = ctx.read_string(this);
    let screen = match decoded.as_deref() {
        Some(text) => text.contains('\u{FFFD}'),
        None => true,
    };
    if screen {
        let units = read_string_chars(&*ctx, this);
        if has_unpaired_surrogate(&units) {
            return Ok(Some(Value::Object(Some(intern_unrepresentable(
                ctx, this, units,
            )))));
        }
    }
    // gcg g2/w: a VM that pools the RECEIVER on a miss (JLS; HotSpot answers
    // `s.intern() == s` for a fresh `s`) answers here.
    // Only for a receiver whose text decoded: pooling an undecodable receiver
    // under the empty key would hand it to every later `"".intern()`.
    // `None` (a context without the rule, or the VM under `=0`) keeps the path below.
    if let Some(text) = decoded.as_deref() {
        if let Some(interned) = ctx.intern_string_receiver(this, text) {
            return Ok(Some(Value::Object(Some(interned))));
        }
    }
    let text = decoded.unwrap_or_default();
    let interned = ctx.intern_string(&text);
    Ok(Some(Value::Object(Some(interned))))
}

/// The intern pool for strings Rust text cannot hold, keyed by UTF-16 units.
///
/// Values are global-root HANDLES, never `ObjectRef`s: the collector owns the
/// reference and hands back its current, possibly relocated address. Same
/// shape, and the same reason, as `net_phase_e::HttpsCarrierSession`'s cached
/// session. A row `String.intern()` made holds a WEAK handle (the String dies
/// with its last reference, as on HotSpot). A row `ldc` made or read is weak
/// too since gci i32/sl: the literal is held by its class's `ldc` record
/// (HotSpot's resolved references), so it dies with its class; under
/// `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0` (and for a condy static
/// argument) `ldc` makes the row strong instead (`vm` crate, `constants.rs`,
/// `intern_wide_string_literal_or_oom_as`). See [`surrogate_intern_replace`].
///
/// Unbounded in principle, exactly as the real intern pool is — and in
/// practice bounded by how many distinct lone-surrogate strings a program
/// interns, which is a set every measurement in this tree has found empty
/// outside a test.
///
/// **One table per VM, two callers.** `String.intern()` reaches it through
/// [`intern_unrepresentable`] below; the interpreter's `ldc` of a
/// surrogate-bearing *literal* reaches it through
/// [`surrogate_intern_probe`] / [`surrogate_intern_claim`], which exist
/// because that caller lives in the `vm` crate and holds a `SharedVm` rather
/// than a `NativeContext`. It has to be the SAME table: JVMS §5.1 interns
/// string literals, so `"\uD800" == "\uD800"` and
/// `LITERAL == LITERAL.intern()` are both required to hold, and two tables
/// would answer the second one `false`.
///
/// Keyed by VM first (gc-common w9-c): the values are JNI global-ref handles,
/// which mean something only in the minting VM's table. One process-wide
/// table made a second VM's literals non-canonical and, once the first VM was
/// gone, could resolve a recycled handle address to an unrelated object.
/// [`surrogate_intern_forget_vm`] drops a VM's rows at teardown.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions —
/// [`surrogate_intern_probe`]'s read and [`surrogate_intern_claim`]'s
/// `entry().or_insert()` — copy a `usize` handle out and drop the guard in the
/// same statement, so neither can hold it across a call back into the VM.
#[allow(clippy::type_complexity)]
pub fn surrogate_intern_pool() -> &'static cratonvm_types::lock_order::OrderedMutex<
    // addr-keyed: not a heap address -- VM identity -> literal -> global-root handle; dropped by surrogate_intern_forget_vm.
    std::collections::HashMap<usize, std::collections::HashMap<Vec<u16>, usize>>,
> {
    static P: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<
            // addr-keyed: the accessor's table (see above).
            std::collections::HashMap<usize, std::collections::HashMap<Vec<u16>, usize>>,
        >,
    > = std::sync::OnceLock::new();
    P.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The global-root handle already canonical for `units`, if any.
///
/// Read half of the pool, for a caller that owns a different root API. A hit
/// lets `ldc` skip allocating the `String` at all.
pub fn surrogate_intern_probe(vm: usize, units: &[u16]) -> Option<usize> {
    let pool = surrogate_intern_pool().lock().ok()?;
    pool.get(&vm)?.get(units).copied()
}

/// Publish `handle` as the canonical instance for `units`, returning whichever
/// handle won. A caller whose handle lost must release it.
///
/// Write half of [`surrogate_intern_probe`]. Takes the handle rather than the
/// `ObjectRef` for the reason the module comment gives: the collector owns the
/// reference, and a raw `ObjectRef` parked in a `static` would be a stale
/// address after the next relocating cycle.
pub fn surrogate_intern_claim(vm: usize, units: Vec<u16>, handle: usize) -> usize {
    match surrogate_intern_pool().lock() {
        Ok(mut pool) => *pool.entry(vm).or_default().entry(units).or_insert(handle),
        // A poisoned pool must not silently de-intern: returning the caller's
        // own handle keeps this call's identity self-consistent, which is the
        // same direction `intern_unrepresentable` takes on the same failure.
        Err(_) => handle,
    }
}

/// Replace `dead` -- a handle for `units` that no longer resolves (a WEAK
/// global whose String was collected) -- with `handle`, and answer whichever
/// handle is canonical now: `handle` when the swap happened (the caller then
/// releases `dead`), the current entry when another thread replaced it first
/// (the caller releases `handle`). Without it a cleared weak handle would win
/// every later [`surrogate_intern_claim`] (insert-if-absent), and each
/// `intern()` of the text would answer a fresh String. gci i4/wr
/// (`gcg-g2w-string-table-review-findings` item 2). Same lock discipline as
/// the claim: the guard is dropped before the handle is returned.
pub fn surrogate_intern_replace_dead(
    vm: usize,
    units: Vec<u16>,
    dead: usize,
    handle: usize,
) -> usize {
    match surrogate_intern_pool().lock() {
        Ok(mut pool) => {
            let slot = pool.entry(vm).or_default().entry(units).or_insert(handle);
            if *slot == dead {
                *slot = handle;
            }
            *slot
        }
        Err(_) => handle,
    }
}

/// Replace the canonical handle for `units` with `handle`, but only while it
/// is still `expected` (a compare-and-swap under the pool lock); answers the
/// handle that is canonical afterwards. With no row for `units`, `handle` is
/// claimed. A caller whose `handle` did not win must release it; the caller
/// whose swap succeeded releases `expected`.
///
/// gcg g2/w item 2 (GC wave 9, lane gcD): `String.intern()` rows hold WEAK
/// handles, which clear when the String dies (HotSpot's `StringTable` keeps
/// no interned String alive). A cleared row must be replaceable, or the
/// insert-if-absent [`surrogate_intern_claim`] would hand every later caller
/// the dead handle and `intern()` / `ldc` would stop answering one instance.
/// The `ldc` side also uses it to upgrade a live weak row to a strong one (a
/// literal lives as long as its class, HotSpot's resolved references).
pub fn surrogate_intern_replace(vm: usize, units: &[u16], expected: usize, handle: usize) -> usize {
    match surrogate_intern_pool().lock() {
        Ok(mut pool) => {
            let row = pool.entry(vm).or_default();
            match row.get_mut(units) {
                Some(cur) if *cur == expected => {
                    *cur = handle;
                    handle
                }
                Some(cur) => *cur,
                None => {
                    row.insert(units.to_vec(), handle);
                    handle
                }
            }
        }
        // Same direction as `surrogate_intern_claim` on a poisoned pool.
        Err(_) => handle,
    }
}

/// Drop `vm`'s rows: its handles mean nothing once its global-ref table is
/// gone, and a later VM must never resolve them. Called from
/// `release_vm_native_state` (gc-common w9-c).
pub fn surrogate_intern_forget_vm(vm: usize) {
    if let Ok(mut pool) = surrogate_intern_pool().lock() {
        pool.remove(&vm);
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
pub fn intern_unrepresentable(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: Vec<u16>,
) -> cratonvm_types::ObjectRef {
    // Probe before rooting: an already-interned literal (the common case once
    // `ldc` populates this table) costs one lock and no root traffic.
    let vm = ctx.vm_identity();
    // gci i4/wr (`gcg-g2w-string-table-review-findings` item 2) and gcg g2/w
    // item 2 (GC wave 9, lane gcD): the surrogate rows hold JNI WEAK globals,
    // so an interned lone-surrogate String nothing else references is
    // collected as in HotSpot (its `StringTable` is weak; a `WeakReference` to
    // it clears); it used to be a strong JNI global, immortal until VM
    // teardown. The rows are weak on EVERY collector, dev's gcD rule: HotSpot
    // 25's `StringTable` is weak under Serial, G1 and ZGC alike, and the rows
    // are JNI weak globals, which every collector clears on its own (the main
    // pool's sweep is not involved). gci i20/mr finding 4: the 2026-10-15
    // merge had kept gci i4/wr's narrower rule (weak only while the main pool
    // is, i.e. on Generational only), which silently made G1's and
    // ZGC's rows strong again -- an interned lone-surrogate String immortal on
    // G1 where HotSpot collects it. A literal's row stays weak (gci i32/sl:
    // its class's `ldc` record holds the literal; strong only under the kill
    // switch, `constants.rs`, `pooled_wide_string_literal`). A context without weak
    // globals (a mock: handle 0) keeps the strong root. A row whose handle no
    // longer resolves is replaced, not kept. A row's String handed back to
    // Java takes the keep-alive the `add_weak_global_root` contract asks for
    // (a concurrent mark may not have reached it; harmless on a strong row).
    let mut probed_dead = None;
    if let Some(winner) = surrogate_intern_probe(vm, &units) {
        if let Some(obj) = ctx.resolve_global_root(winner) {
            ctx.gc_reference_keep_alive(obj);
            return obj;
        }
        // A cleared weak row (its String died): take it over below.
        probed_dead = Some(winner);
    }
    let mut handle = ctx.add_weak_global_root(this);
    if handle == 0 {
        handle = ctx.add_global_root(this);
    }
    let mut winner = match probed_dead {
        Some(d) => surrogate_intern_replace(vm, &units, d, handle),
        None => surrogate_intern_claim(vm, units.clone(), handle),
    };
    if winner == handle {
        if let Some(d) = probed_dead {
            // Every weak handle is released through `remove_global_root`,
            // a cleared one included, or its slot leaks.
            ctx.remove_global_root(d);
        }
        return this;
    }
    // A row another thread filed whose String has died since: replace it.
    if ctx.resolve_global_root(winner).is_none() {
        let dead = winner;
        winner = surrogate_intern_replace_dead(vm, units, dead, handle);
        if winner == handle {
            ctx.remove_global_root(dead);
            return this;
        }
    }
    ctx.remove_global_root(handle);
    // `resolve_global_root` is the only correct way back: the winning object
    // may have been relocated since it was rooted.
    let canonical = ctx.resolve_global_root(winner).unwrap_or(this);
    // Another thread's row: not this call's receiver, so keep it alive.
    ctx.gc_reference_keep_alive(canonical);
    canonical
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
///
/// gc-common w20-b: this used to hand-write the compact layout -- a
/// `new_array` plus one `set_array_element` per BYTE of every
/// `StringBuilder.toString()` result, and the compact `byte[]` layout even
/// when the VM runs the legacy `char[]` String layout. It now ends in the
/// shared in-place writer ([`init_string_from_units_or_oom`]): the same
/// fields (value, coder, hash = 0, hashIsZero = false) laid down by
/// `populate_java_string_fields` with one bulk copy, on the collecting retry
/// ladder, with `this` pinned.
pub fn native_string_init_abstract_string_builder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let chars = match args.get(1) {
        Some(Value::Object(Some(builder))) => sb_read_chars(ctx, *builder),
        // A null builder is the real constructor's first dereference
        // (`asb.getValue()`), not an empty String. This arm answered `""` —
        // a fabricated success: MEASURED against HotSpot 25.0.3+9 with
        // `R16StringsIdentity` (reflective `String(AbstractStringBuilder,
        // Void)` with a null builder throws the NPE below there).
        Some(Value::Object(None)) if string_builder_ctor_null_npe_enabled() => {
            return Err(string_ctor_null_builder_npe());
        }
        _ => Vec::new(),
    };
    init_string_from_units_or_oom(ctx, this, &chars, "String(AbstractStringBuilder,Void)")?;
    Ok(None)
}

/// The `NullPointerException` the real `String(AbstractStringBuilder, Void)`
/// raises for a null builder — its first instruction after `Object.<init>` is
/// `asb.getValue()` — with HotSpot's helpful-NPE text, MEASURED on 25.0.3+9
/// for both `new String((StringBuilder) null)` (which delegates to that
/// constructor) and the constructor itself (`R16StringsIdentity`).
///
/// JIT round 16, lane strings (STR16-4). Shared by the two shadows of that
/// constructor: this file's `(AbstractStringBuilder,Void)` one and
/// `deprecated_util`'s `(StringBuilder)` one, which answered
/// `"null object argument"`.
pub fn string_ctor_null_builder_npe() -> MethodCallFailed {
    sb_npe("Cannot invoke \"java.lang.AbstractStringBuilder.getValue()\" because \"asb\" is null")
}

/// `CRATONVM_STRING_BUILDER_CTOR_NULL_NPE` (JIT round 16, lane strings,
/// STR16-4; **default ON**, `=0` is the kill switch): the two native String
/// constructors over a builder throw HotSpot's NPE for a null builder. See
/// [`string_ctor_null_builder_npe`].
pub fn string_builder_ctor_null_npe_enabled() -> bool {
    true
}

/// `CRATONVM_STRING_BUILDER_CTOR_BYTECODE` (JIT round 16, lane strings,
/// STR16-2; default ON since round 16: R16Bench strings 1992->1377 ms, toString 549->54 ns). Read once,
/// at registration.
pub fn string_builder_ctor_bytecode_flag() -> bool {
    true
}

/// Whether the two `Intrinsic` shadows of the real String-from-builder
/// constructors — `String(AbstractStringBuilder, Void)` and
/// `String(StringBuilder)` — stay UNREGISTERED, so the JDK's own bytecode
/// runs.
///
/// Both exist for DF05: on the synthetic image a builder is `char[]`-backed,
/// and the real constructor's `Arrays.copyOfRange(asb.getValue(), ...)` would
/// read it one byte at a time. That premise is false under `--real-jdk` on a
/// real image: no builder registration survives there (every
/// `AbstractStringBuilder` / `StringBuilder` / `StringBuffer` native is a
/// `SyntheticStub` the strict registry refuses), so every builder is the JDK's
/// own compact `byte[]` object, `coder` and `maybeLatin1` maintained by JDK
/// bytecode and by the JIT's in-place `append` intrinsics, which write the
/// same layout. `--compatible` kept the natives while its builders were
/// mutated by natives that never write `maybeLatin1`, which the real
/// constructor reads to decide whether a UTF16 payload may compress; since
/// the strings round (`str16`) its real-image builders run the JDK's bodies
/// too (`HotSpotAlignment::JdkStringBuilderBodies`, `sb_bodies_aligned`), and
/// with that row the constructor yields there as well.
///
/// Why it matters: the native is the dominant term of
/// `new StringBuilder().append(..)...toString()` — MEASURED (Windows box,
/// `R16StringsSplit`) `toString()` of a 14-character builder at ~2 200-3 200
/// ns/op through the native against ~340 ns/op for a user-code replica of the
/// constructor's LATIN1 arm (`Arrays.copyOfRange` + one allocation), and the
/// replica's work is all the real constructor does.
pub fn string_builder_ctor_yields_to_bytecode(
    mode: cratonvm_types::compat::CompatibilityMode,
    real_jdk: bool,
    sb_bodies_aligned: bool,
    flag_on: bool,
) -> bool {
    flag_on && real_jdk && (mode.is_jdk_only() || sb_bodies_aligned)
}

/// `sb_bodies_aligned` for [`string_builder_ctor_yields_to_bytecode`]: does
/// this registry's `--compatible` drop the builder natives
/// (`HotSpotAlignment::JdkStringBuilderBodies`)?
pub fn sb_bodies_aligned(registry: &cratonvm_native_api::NativeMethodRegistry) -> bool {
    registry
        .hotspot_alignments()
        .contains(cratonvm_types::HotSpotAlignment::JdkStringBuilderBodies)
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
pub fn hash_slot_for(element_type: Option<cratonvm_types::ArrayElementType>) -> Option<usize> {
    // (The latch it feeds is `STRING_HASH_SLOT`, per VM: gc-common w15-e.)
    match element_type {
        Some(cratonvm_types::ArrayElementType::Byte)
        | Some(cratonvm_types::ArrayElementType::Boolean) => Some(2),
        Some(_) => Some(1),
        None => None,
    }
}

thread_local! {
    /// `(vm_identity, String.hash slot)` for the VM this thread last hashed a
    /// `String` in (gc-common w15-e). One TLS read and one compare on the hot
    /// path; a thread that moves to another VM re-derives the slot once.
    pub static STRING_HASH_SLOT: std::cell::Cell<Option<(usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

pub fn native_string_hash_code(
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
    // value-array element type, which is constant for a VM — so resolve it
    // ONCE per VM (from a real string's value array) and cache it.
    // Then read the cached hash FIRST, before touching the value array, so a
    // cache hit is a single field read like HotSpot. (Resolving the slot per
    // call — either by class+field name or by re-reading the value array —
    // was itself the bottleneck that kept cache hits ~20x slower than a plain
    // field read, dwarfing the hashing win.)
    //
    // The latch must only ever be set from a receiver that actually carries
    // the evidence — see `hash_slot_for`.
    //
    // gc-common w15-e: the latch is per VM (`STRING_HASH_SLOT`, a thread-local
    // keyed by the VM identity), not process-wide. The layout is a property of
    // a VM's `String` class, and one process can host VMs with different
    // layouts (a synthetic `char[]` one beside a JDK 9+ `byte[]` one, as a
    // test binary does): the process-wide latch handed the first VM's slot to
    // every other VM, where slot 1 is `coder` (read as the cached hash, and
    // overwritten with the computed one) and slot 2 lies past the end of a
    // two-field synthetic `String`.
    let vm = ctx.vm_identity();
    let hash_field_index: usize = match STRING_HASH_SLOT.with(std::cell::Cell::get) {
        Some((owner, slot)) if owner == vm => slot,
        _ => {
            let elem = string_char_array(ctx, this).map(|(arr, _)| ctx.heap_element_type_of(arr));
            match hash_slot_for(elem) {
                Some(slot) => {
                    STRING_HASH_SLOT.with(|c| c.set(Some((vm, slot))));
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
    } else if hash_field_index == 2 && ctx.object_num_fields(this) > 3 {
        // JDK 25 `String.hashCode`: `if (h == 0) hashIsZero = true; else hash
        // = h;`. The JDK 9+ compact layout is `{value, coder, hash,
        // hashIsZero}` (the slot the latch above already assumes for `hash`),
        // so a zero hash marks slot 3. Reflection / `Unsafe` visible only: the
        // value answered is 0 either way. STR20 review.
        ctx.set_field(this, 3, Value::Int(1));
    }

    Ok(Some(Value::Int(hash)))
}

pub fn native_string_length(
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

pub fn native_string_char_at(
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
        None => return Err(cratonvm_types::error::RuntimeError::sioobe_no_length(index).into()),
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
        //
        // JIT round 16 wave 8 (lane trace; `CRATONVM_BOUNDS_RAISE_BYTECODE`,
        // default ON): and its FRAMES. The raise runs the real `String.charAt`
        // body, so the exception lists HotSpot's ten JDK frames
        // (`Preconditions$1.apply` .. `String.charAt`) above the caller; this
        // native has no frame, so building the exception here listed none of
        // them -- every cached interpreted call site, and every compiled one
        // (its out-of-range arm deopts into this cache). Only for the JDK's
        // compact `byte[]` layout, the one that bytecode reads; the screen
        // keeps this raise for anything that does not come back as a
        // `StringIndexOutOfBoundsException`.
        if is_byte_array {
            if let Some(raised) = crate::preconditions::raise_through_bytecode(
                ctx,
                "java/lang/String",
                "charAt",
                "(I)C",
                &[Value::Object(Some(this)), Value::Int(index)],
                Some("java/lang/StringIndexOutOfBoundsException"),
            ) {
                return Err(raised);
            }
        }
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

pub fn native_string_equals(
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

pub fn native_string_index_of(
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
pub fn native_string_index_of_from(
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
pub fn native_string_last_index_of_from(
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

pub fn native_string_substring(
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
        return Err(cratonvm_types::error::RuntimeError::sioobe_range(
            begin,
            end,
            char_count as i32,
        )
        .into());
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
    // `_gc_safe`: `sub_utf16` is already Rust-owned; `this`/`arr` are not
    // dereferenced again below, so a moving young GC here is safe. Without
    // this, String.substring() -- unconditionally forced native, hot path
    // for Response.toAbsolute()-style URI manipulation -- hard-aborts the
    // whole process on young-gen exhaustion instead of collecting and
    // continuing. See docs/feature-designs/gpu-offload.md.
    // nio21 w9 L5: lossless when the range splits a surrogate pair.
    let result = string_native_result_from_units(ctx, &sub_utf16, true)?;
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = int value
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = val.to_string();
    let result = ctx.create_string_uninterned_gc_safe(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// Format a double like Java does (no trailing zeros for integers, etc.)
pub fn format_double(v: f64) -> String {
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
///
/// **Memoised per class.** `resolve_field_index_by_class_id` takes a
/// class-manager READ LOCK and walks the hierarchy comparing field NAMES, and
/// `sb_view` + `sb_set_count` between them ask for one twice on every single
/// `append`. A name→slot answer is a property of the CLASS, not of the
/// receiver, so [`SB_SLOT_MEMO`] caches it keyed by `ClassId` and the hot path
/// pays one `class_id_of_object` and an array scan instead. Measured on the
/// `String/Regex` row's builder loop, `resolve_field_index_by_class_id` +
/// `set_field_by_name` + the `memcmp` they drive were 5.3% of the phase.
pub fn sb_field_slot(
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
    let class_id = ctx.class_id_of_object(this);
    let slot = sb_class_slot(ctx, class_id, name)?;
    if slot < fields {
        Some(slot)
    } else {
        None
    }
}

/// One memoised `(class, field name) -> slot` answer.
///
/// `slot` is `None` for a name the class does not declare at all — the
/// NEGATIVE answer is worth caching too, because `sb_set_count`'s
/// `toStringCache` probe misses on every `StringBuilder` ever appended to and
/// a miss is the most expensive lookup there is: it walks the whole hierarchy
/// before concluding.
///
/// Keyed by `ClassId`, which makes the answer permanent: a `ClassId` names one
/// loaded class in one loader, and JVMTI `RedefineClasses` is forbidden from
/// changing a class's field layout, so a name that resolved to slot `k` under
/// this class resolves to slot `k` for the rest of the run. (The
/// `java.util.regex` fast path next door caches the same kind of answer in a
/// process-global `OnceLock` keyed only by class NAME; this is the stricter
/// key, not a looser one.)
///
/// gc-common w15-e: a `ClassId` is minted per VM, from zero, so the key also
/// carries the VM identity. A thread that serves two VMs (an embedder's host
/// thread, a test that builds two VMs in turn) used to take one VM's slot for
/// the other VM's unrelated class with the same id.
#[derive(Clone, Copy)]
pub struct SbSlotMemoEntry {
    pub vm: usize,
    pub class_id: cratonvm_types::ClassId,
    /// Index into [`SB_MEMO_NAMES`].
    pub name: u8,
    pub slot: Option<u32>,
}

/// The field names this file resolves on a hot path. Anything not listed here
/// goes straight to the class model, uncached — the memo is a fixed-size array
/// scan, so it stays fast only while it stays small.
pub const SB_MEMO_NAMES: [&str; 3] = ["count", "coder", "toStringCache"];

thread_local! {
    /// Direct-mapped `(ClassId, name) -> slot` memo.
    ///
    /// Sized for the receivers this path actually sees: `StringBuilder` and
    /// `StringBuffer`, three names each. Thread-local so it needs no lock of
    /// its own — the thing it exists to avoid IS a lock.
    pub static SB_SLOT_MEMO: std::cell::RefCell<Vec<SbSlotMemoEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`sb_field_slot`]'s class-model half, memoised. See [`SbSlotMemoEntry`].
pub fn sb_class_slot(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    name: &str,
) -> Option<usize> {
    let Some(name_idx) = SB_MEMO_NAMES.iter().position(|&n| n == name) else {
        return ctx.resolve_field_index_by_class_id(class_id, name);
    };
    let name_idx = name_idx as u8;
    let vm = ctx.vm_identity();
    let hit = SB_SLOT_MEMO.with(|memo| {
        memo.borrow()
            .iter()
            .find(|e| e.vm == vm && e.class_id == class_id && e.name == name_idx)
            .map(|e| e.slot)
    });
    if let Some(slot) = hit {
        return slot.map(|s| s as usize);
    }
    let slot = ctx.resolve_field_index_by_class_id(class_id, name);
    SB_SLOT_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        // A runaway memo would turn the scan into the cost it removes. Real
        // programs load two builder classes; a pathological one that somehow
        // produced more just stops being memoised past the cap. Entries of a
        // VM this thread no longer serves are dropped first, so a long-lived
        // thread that outlives several VMs does not fill the cap with them.
        if memo.len() >= 32 {
            memo.retain(|e| e.vm == vm);
        }
        if memo.len() < 32 {
            memo.push(SbSlotMemoEntry {
                vm,
                class_id,
                name: name_idx,
                slot: slot.map(|s| s as u32),
            });
        }
    });
    slot
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
pub fn sb_value_units(
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

// ---------------------------------------------------------------------------
// The two layouts, and the WRITER half of `sb_value_units`
// ---------------------------------------------------------------------------

/// Which representation a builder's `value` slot holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SbLayout {
    /// CratonVM's own synthetic class: `value: char[] @0`, `count: int @1`.
    /// One array element per character; there is no `coder`.
    Synthetic,
    /// The real JDK's compact layout: `value: byte[]`, `coder: byte`,
    /// `count: int` — every slot resolved BY NAME, because a slot COUNT cannot
    /// separate the nine images (see [`sb_field_slot`]).
    Compact { coder_slot: usize },
}

/// Everything a native needs to know about a builder's payload, resolved in ONE
/// pass.
///
/// The first version of this migration asked the receiver the same questions in
/// every helper: `sb_append_units` alone reached `sb_layout` four times — once
/// directly, once through `sb_count_units`, once through `sb_capacity_units`,
/// once more for the coder — and each of those re-read `value`, re-asked its
/// element type, and for a compact receiver resolved `coder` by NAME under the
/// class-manager read lock.
///
/// MEASURED, ABBA-interleaved over six rounds against the pre-migration control:
/// **2.0x on `append(String)`, 2.6x on `append(int)`, 3.1x on `charAt`, 3.5x on
/// an inflating build.** That is the cost of asking the same question five
/// times on the hottest path in the VM, and this struct is the answer: one pass,
/// handed down.
#[derive(Clone, Copy)]
pub struct SbView {
    pub layout: SbLayout,
    pub buf: Option<cratonvm_types::ObjectRef>,
    /// Array length in ELEMENTS — bytes for the compact layout, characters for
    /// the synthetic one.
    pub raw_len: usize,
    pub utf16: bool,
    /// Characters, clamped to what the payload can actually hold.
    pub count: usize,
    /// Characters — `value.length >> coder`, which is `capacity()`.
    pub capacity: usize,
    /// The receiver's class, and where THIS image puts `count` in it —
    /// resolved once while building the view so the write side does not have
    /// to resolve it a second time. `None` for the synthetic 2-slot layout and
    /// for a receiver whose class model cannot answer, which is exactly when
    /// [`sb_set_count`] falls back to its historical index heuristic.
    pub count_slot: Option<(cratonvm_types::ClassId, usize)>,
}

/// Where `count` lives for THIS receiver, with the class it was resolved
/// against.
///
/// The class id is handed back so the write side can reuse it for its own
/// (memoised) `toStringCache` lookup instead of asking the heap for the
/// receiver's class a second time.
pub fn sb_count_slot(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Option<(cratonvm_types::ClassId, usize)> {
    let fields = ctx.object_num_fields(this);
    if fields <= 2 {
        return None;
    }
    let class_id = ctx.class_id_of_object(this);
    let slot = sb_class_slot(ctx, class_id, "count")?;
    if slot < fields {
        Some((class_id, slot))
    } else {
        None
    }
}

/// No class model (the mock) — the historical index heuristic, unchanged.
pub fn sb_raw_count_heuristic(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> i32 {
    let slot = if ctx.object_num_fields(this) >= 3 {
        2
    } else {
        1
    };
    match ctx.get_field(this, slot) {
        Value::Int(v) => v,
        _ => 0,
    }
}

/// Resolve the receiver's payload once.
///
/// `WORKER-3-NOTE-3` §3 named the residual precisely: CratonVM keeps two
/// incompatible representations of one class, the natives wrote the first and
/// real `AbstractStringBuilder` bytecode writes the second, and **any run in
/// which some calls take one path and some the other holds a torn object**.
/// [`sb_value_units`] taught the READER both; every writer then still converted
/// whatever it found into a `char[]`, which is what PRODUCED the tear rather
/// than tolerating it.
///
/// The payload that is already there decides. A builder real bytecode
/// constructed is written back compact; one allocated under the synthetic
/// 2-slot class is written back as a `char[]`. Only a builder with no payload at
/// all asks the class, and there the presence of a field named `coder` is the
/// whole test: the synthetic class declares two fields and neither is called
/// that (`classloading/src/class_manager.rs`, `instance_fields(2)`).
pub fn sb_view(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> SbView {
    use cratonvm_types::ArrayElementType;
    // One `array_shape` where this used to ask `object_is_array`,
    // `array_length` and `heap_element_type_of` about the same payload — three
    // validated heap round-trips, each re-walking the same header, on every
    // `append`.
    let (buf, raw_len, elem) = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => match ctx.array_shape(a) {
            Some((elem, len)) => (Some(a), len, Some(elem)),
            None => (None, 0, None),
        },
        _ => (None, 0, None),
    };
    // `coder` is read at slot 1 FIRST, and only resolved by name when that read
    // cannot be a coder.
    //
    // `AbstractStringBuilder` declares `value` then `coder` before anything
    // else on every compact-strings image — MEASURED with `javap -p --system`
    // on 17.0.20.1+1, 21.0.12+8 and 25.0.4+7, where the variation is entirely
    // in what comes AFTER (`maybeLatin1` arrives at 2 in 21 and 25, which is
    // why `count` still has to be resolved by name; see `sb_field_slot`).
    //
    // The guess is self-checking rather than trusted: a `coder` is 0 or 1 and
    // nothing else, so any other value — or a non-int, which is what slot 1
    // holds under the synthetic 2-slot layout — falls through to the name
    // lookup. That matters because the lookup is a class-manager READ LOCK on
    // the hottest path in the VM: paying it per operation, on top of the one
    // `count` already costs, is the whole of `charAt`'s remaining 1.25x
    // against the pre-migration control.
    let compact_payload = matches!(
        elem,
        Some(ArrayElementType::Byte) | Some(ArrayElementType::Boolean)
    );
    let (layout, utf16) = if compact_payload {
        match ctx.get_field(this, 1) {
            Value::Int(c @ (0 | 1)) => (SbLayout::Compact { coder_slot: 1 }, c == 1),
            _ => {
                let coder_slot = sb_field_slot(ctx, this, "coder").unwrap_or(1);
                let utf16 = matches!(ctx.get_field(this, coder_slot), Value::Int(1));
                (SbLayout::Compact { coder_slot }, utf16)
            }
        }
    } else if matches!(elem, Some(ArrayElementType::Char)) {
        (SbLayout::Synthetic, false)
    } else {
        // No payload to ask, so the CLASS decides — the constructor case.
        match sb_field_slot(ctx, this, "coder") {
            Some(coder_slot) => (
                SbLayout::Compact { coder_slot },
                matches!(ctx.get_field(this, coder_slot), Value::Int(1)),
            ),
            None => (SbLayout::Synthetic, false),
        }
    };
    let capacity = if utf16 { raw_len / 2 } else { raw_len };
    let count_slot = sb_count_slot(ctx, this);
    let raw_count = match count_slot {
        Some((_, slot)) => match ctx.get_field(this, slot) {
            Value::Int(v) => v,
            _ => 0,
        },
        None => sb_raw_count_heuristic(ctx, this),
    };
    let count = (raw_count.max(0) as usize).min(capacity);
    SbView {
        layout,
        buf,
        raw_len,
        utf16,
        count,
        capacity,
        count_slot,
    }
}

/// Which representation the receiver holds. Prefer [`sb_view`] where more than
/// one of its answers is wanted; this exists for the few callers that want only
/// this one.
pub fn sb_layout(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> SbLayout {
    sb_view(ctx, this).layout
}

/// Capacity in CHARACTERS — `value.length >> coder`, which is exactly what
/// `AbstractStringBuilder.capacity()` answers, and is observable and specified.
pub fn sb_capacity_units(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> usize {
    sb_view(ctx, this).capacity
}

/// The builder's length, clamped to what its payload can actually hold.
pub fn sb_count_units(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> usize {
    sb_view(ctx, this).count
}

/// `AbstractStringBuilder.ensureCapacityInternal` + `newCapacity`, reproduced
/// rather than approximated: `capacity()` is observable, so the growth rule is
/// part of the contract and `probes/StringBuilderShadowSweep.java` asserts it at
/// six points.
pub fn sb_grow_units(old: usize, needed: usize) -> usize {
    if needed <= old {
        old
    } else {
        std::cmp::max(old.saturating_mul(2).saturating_add(2), needed)
    }
}

/// [`sb_grow_units`] for a compact LATIN1 payload that INFLATES to UTF16 on the
/// same write: `ensureCapacityNewCoder` asks `newCapacity` with the NEW coder
/// over the OLD byte array, so the preferred growth is `oldLength + (2 << 1)`
/// bytes over an `oldLength` of one byte per character: `max(needed, old + 2)`
/// characters, not the same-coder `2 * old + 2` (strings round 2, `str17`:
/// `new StringBuilder(1).append('n').append('Ď')` reported capacity 4
/// against HotSpot's 3, in every mode, interpreted too). No growth requested
/// keeps the capacity.
pub fn sb_inflate_grow_units(old: usize, needed: usize) -> usize {
    if needed <= old {
        old
    } else {
        std::cmp::max(old.saturating_add(2), needed)
    }
}

thread_local! {
    /// Scratch for the compact payload's BYTES, so the bulk read/write pair can
    /// be used without an allocation per call.
    pub static SB_BYTE_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The characters in `[from, to)` of a resolved view.
///
/// One bulk call per read: `read_char_array_into` for the synthetic payload,
/// `read_byte_array_into` for the compact one, decoded in Rust afterwards. The
/// per-element `get_array_element` loop this replaced was half of `charAt`'s
/// 3.1x.
pub fn sb_units_range_view(
    ctx: &dyn NativeContext,
    v: &SbView,
    from: usize,
    to: usize,
) -> Vec<u16> {
    let mut out = Vec::new();
    let Some(arr) = v.buf else {
        return out;
    };
    if to <= from {
        return out;
    }
    match v.layout {
        SbLayout::Synthetic => {
            let to = to.min(v.raw_len);
            if to <= from {
                return out;
            }
            out.resize(to - from, 0);
            let written = ctx.read_char_array_into(arr, from, &mut out[..]);
            out.truncate(written);
        }
        SbLayout::Compact { .. } if v.utf16 => {
            let to = to.min(v.raw_len / 2);
            if to <= from {
                return out;
            }
            let n = to - from;
            SB_BYTE_SCRATCH.with(|cell| {
                let mut bytes = cell.borrow_mut();
                bytes.clear();
                bytes.resize(n * 2, 0);
                let read = ctx.read_byte_array_into(arr, from * 2, &mut bytes[..]);
                // Little-endian, the same order `sb_put_units_at` writes and
                // `vm_object::create_java_string` uses for Strings.
                out.reserve(read / 2);
                for pair in bytes[..read - (read % 2)].chunks_exact(2) {
                    out.push(u16::from(pair[0]) | (u16::from(pair[1]) << 8));
                }
            });
        }
        SbLayout::Compact { .. } => {
            let to = to.min(v.raw_len);
            if to <= from {
                return out;
            }
            let n = to - from;
            SB_BYTE_SCRATCH.with(|cell| {
                let mut bytes = cell.borrow_mut();
                bytes.clear();
                bytes.resize(n, 0);
                let read = ctx.read_byte_array_into(arr, from, &mut bytes[..]);
                out.reserve(read);
                out.extend(bytes[..read].iter().map(|&b| u16::from(b)));
            });
        }
    }
    out
}

/// The characters in `[from, to)`, whatever layout the builder holds.
pub fn sb_units_range(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    from: usize,
    to: usize,
) -> Vec<u16> {
    sb_units_range_view(ctx, &sb_view(ctx, this), from, to)
}

/// One character of a resolved view, without decoding the whole payload.
pub fn sb_unit_at_view(ctx: &dyn NativeContext, v: &SbView, index: usize) -> Option<u16> {
    let arr = v.buf?;
    match v.layout {
        SbLayout::Synthetic => {
            if index >= v.raw_len {
                return None;
            }
            match ctx.get_array_element(arr, index) {
                Value::Int(x) => Some(x as u16),
                _ => None,
            }
        }
        SbLayout::Compact { .. } if v.utf16 => {
            if index * 2 + 1 >= v.raw_len {
                return None;
            }
            let lo = match ctx.get_array_element(arr, index * 2) {
                Value::Int(x) => u16::from(x as u8),
                _ => 0,
            };
            let hi = match ctx.get_array_element(arr, index * 2 + 1) {
                Value::Int(x) => u16::from(x as u8),
                _ => 0,
            };
            Some((hi << 8) | lo)
        }
        SbLayout::Compact { .. } => {
            if index >= v.raw_len {
                return None;
            }
            match ctx.get_array_element(arr, index) {
                Value::Int(x) => Some((x & 0xff) as u16),
                _ => None,
            }
        }
    }
}

/// One character, without decoding the whole payload.
pub fn sb_unit_at(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    index: usize,
) -> Option<u16> {
    sb_unit_at_view(ctx, &sb_view(ctx, this), index)
}

/// Write `units` into a payload starting at CHARACTER `at`.
///
/// ONE bulk call, always: `write_char_array_from` for the synthetic payload and
/// `write_byte_array_from` for the compact one, the latter encoded into the
/// thread-local scratch first. The per-element store loop this replaced turned
/// `append("abcdefgh")` into eight VM dispatches, and sixteen once the builder
/// had inflated.
///
/// UTF16 is little-endian — the same order [`sb_value_units`] reads and
/// `vm_object::create_java_string` writes, so `StringUTF16.isBigEndian()` stays
/// false for builders exactly as it is for Strings.
pub fn sb_put_units_at(
    ctx: &mut dyn NativeContext,
    buf: cratonvm_types::ObjectRef,
    at: usize,
    units: &[u16],
    layout: SbLayout,
    utf16: bool,
) {
    if units.is_empty() {
        return;
    }
    if let SbLayout::Synthetic = layout {
        if !ctx.write_char_array_from(buf, at, units) {
            for (i, &u) in units.iter().enumerate() {
                ctx.set_array_element(buf, at + i, Value::Int(i32::from(u)));
            }
        }
        return;
    }
    // The encode and the store both happen INSIDE the borrow. Cloning the
    // scratch out of its cell first — which is what this did — allocates a
    // fresh `Vec` on every append and gives the scratch nothing to do.
    SB_BYTE_SCRATCH.with(|cell| {
        let mut bytes = cell.borrow_mut();
        bytes.clear();
        let off = if utf16 {
            bytes.reserve(units.len() * 2);
            for &u in units {
                bytes.push((u & 0xFF) as u8);
                bytes.push((u >> 8) as u8);
            }
            at * 2
        } else {
            bytes.extend(units.iter().map(|&u| (u & 0xFF) as u8));
            at
        };
        if !ctx.write_byte_array_from(buf, off, &bytes) {
            // A heap that declines the bulk path (the unit-test mocks do).
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(buf, off + i, Value::Int(i32::from(b)));
            }
        }
    });
}

/// Store `units` as the builder's ENTIRE content, in the receiver's OWN layout.
///
/// `exact_capacity` is `None` for an ordinary write — the payload array is
/// reused when the units fit and otherwise grown by [`sb_grow_units`] — and
/// `Some(c)` for the two callers that own the capacity themselves,
/// `ensureCapacity` and `trimToSize`, which always reallocate.
///
/// Returns the post-GC `this`: this can allocate, so every caller MUST use the
/// returned reference for anything afterwards.
///
/// This is the writer half `WORKER-3-NOTE-3` N1 named. Before it, every write
/// path installed a fresh `char[]` over whatever it found, so a builder that
/// real `AbstractStringBuilder` bytecode had constructed was CONVERTED to the
/// synthetic layout by its first native mutation — and every later
/// real-bytecode read of `value`/`coder` then read a `char[]` as though it were
/// a compact `byte[]`. MEASURED against jdk-25.0.4+7:
/// `new StringBuilder("a\u{20ac}b").chars().toArray()` answered
/// `[97, 172, 98]` where HotSpot answers `[97, 8364, 98]` — 0x20AC truncated to
/// its low byte, which is exactly a LATIN1 read of a UTF-16 unit.
/// `codePoints()`, `compareTo` and `writeObject` are the same read and were the
/// same wrong.
pub fn sb_store_units(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: &[u16],
    exact_capacity: Option<usize>,
) -> cratonvm_types::ObjectRef {
    use cratonvm_types::ArrayElementType;
    let v = sb_view(&*ctx, this);
    // The JDK INFLATES and never deflates: once a builder has held a non-LATIN1
    // character its `coder` stays UTF16 for the life of the value array.
    // Deflating on the way back down would DOUBLE the reported capacity, which
    // `probes/StringBuilderShadowSweep.java` asserts it does not
    // (`delete back to all-latin1 keeps capacity`).
    let compact = matches!(v.layout, SbLayout::Compact { .. });
    let utf16 = compact && (v.utf16 || units.iter().any(|&u| u > 0xFF));
    let target = match exact_capacity {
        Some(c) => c.max(units.len()),
        // A LATIN1 payload inflating on this write grows by
        // `ensureCapacityNewCoder`'s rule, not the same-coder one.
        None if compact && utf16 && !v.utf16 => sb_inflate_grow_units(v.capacity, units.len()),
        None => sb_grow_units(v.capacity, units.len()),
    };
    let need_elems = if compact {
        target << usize::from(utf16)
    } else {
        target
    };
    let reuse = if exact_capacity.is_some() || utf16 != v.utf16 {
        None
    } else {
        v.buf.filter(|_| {
            v.raw_len
                >= if compact {
                    units.len() << usize::from(utf16)
                } else {
                    units.len()
                }
        })
    };
    let (this, buf) = match reuse {
        Some(a) => (this, a),
        None => {
            let elem = if compact {
                ArrayElementType::Byte
            } else {
                ArrayElementType::Char
            };
            let mut scope = NativeHandleScope::new(ctx);
            let handle = scope.root(this);
            // gc-common w8-b (`common-w3c-native-callback-allocations-cannot-collect`,
            // route 1, "StringBuilder expansion"): reclaim and retry before
            // the `OutOfMemoryError`, as `newarray` in `AbstractStringBuilder`
            // would. Legal because `this` is rooted by the scope, and every
            // caller of the growth helpers (`sb_store_units`, `sb_write_units`,
            // `sb_append_units`, `sb_reserve_units`, `sb_append_chars`,
            // `sb_append_str`, `sb_write_chars`; all in this file) holds no
            // other heap reference across the call: each returns the
            // refreshed `this` (or `Ok(None)`) straight away, and the ones
            // that hold more keep it in their own `NativeHandleScope`. A new
            // caller that keeps a bare `ObjectRef` across these helpers breaks
            // that and must root it. The last arm is the historical
            // single-attempt `new_array`, so exhaustion still surfaces
            // exactly as before (the native funnel's `OutOfMemoryError`).
            let fresh = match scope.try_new_array(elem, need_elems) {
                Some(arr) => arr,
                None => {
                    let retried = if scope.reclaim_before_alloc_retry() {
                        scope.try_new_array(elem, need_elems).or_else(|| {
                            // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
                            if scope.reclaim_after_failed_alloc_retry() {
                                scope.try_new_array(elem, need_elems)
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    };
                    match retried {
                        Some(arr) => arr,
                        None => scope.new_array(elem, need_elems),
                    }
                }
            };
            let this = scope.get(&handle);
            scope.set_field(this, 0, Value::Object(Some(fresh)));
            (this, fresh)
        }
    };
    sb_put_units_at(ctx, buf, 0, units, v.layout, utf16);
    if let SbLayout::Compact { coder_slot } = v.layout {
        ctx.set_field(this, coder_slot, Value::Int(i32::from(utf16)));
    }
    sb_set_count(ctx, this, units.len() as i32);
    this
}

/// Replace the whole payload, growing by the JDK's rule when it does not fit.
pub fn sb_write_units(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: &[u16],
) -> cratonvm_types::ObjectRef {
    sb_store_units(ctx, this, units, None)
}

/// Append `units` in the receiver's own layout.
///
/// The in-place arm is the hot one: one resolved view, one bulk store, one count
/// write, no allocation and no whole-payload read. The slow arm — a grow, or a
/// LATIN1 payload asked for a character it cannot hold — reads the content once
/// and hands it to [`sb_store_units`], which is amortised O(1) because the
/// growth rule doubles.
pub fn sb_append_units(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: &[u16],
) -> cratonvm_types::ObjectRef {
    if units.is_empty() {
        return this;
    }
    let v = sb_view(&*ctx, this);
    if v.count + units.len() <= v.capacity {
        let representable = match v.layout {
            SbLayout::Synthetic => true,
            SbLayout::Compact { .. } => v.utf16 || units.iter().all(|&u| u <= 0xFF),
        };
        if let (true, Some(buf)) = (representable, v.buf) {
            sb_put_units_at(ctx, buf, v.count, units, v.layout, v.utf16);
            sb_set_count_resolved(ctx, this, (v.count + units.len()) as i32, v.count_slot);
            return this;
        }
    }
    let mut all = sb_units_range_view(&*ctx, &v, 0, v.count);
    all.extend_from_slice(units);
    sb_store_units(ctx, this, &all, None)
}

/// Grow the payload so it can hold at least `min_capacity` characters, keeping
/// the content and the coder. `ensureCapacity`'s whole body.
pub fn sb_reserve_units(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    min_capacity: usize,
) -> cratonvm_types::ObjectRef {
    let v = sb_view(&*ctx, this);
    if min_capacity <= v.capacity {
        return this;
    }
    let units = sb_units_range_view(&*ctx, &v, 0, v.count);
    let target = sb_grow_units(v.capacity, min_capacity);
    sb_store_units(ctx, this, &units, Some(target))
}

/// Overwrite ONE character in place. `false` when the payload cannot hold it —
/// a LATIN1 compact array asked for a non-LATIN1 character — and the caller must
/// take the whole-payload path so the array inflates.
pub fn sb_put_unit(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    index: usize,
    unit: u16,
) -> bool {
    let v = sb_view(&*ctx, this);
    if matches!(v.layout, SbLayout::Compact { .. }) && !v.utf16 && unit > 0xFF {
        return false;
    }
    let Some(buf) = v.buf else {
        return false;
    };
    sb_put_units_at(ctx, buf, index, &[unit], v.layout, v.utf16);
    true
}

/// The catchable `OutOfMemoryError` HotSpot raises for an over-large value
/// array. The panicking allocator would abort the whole VM instead of unwinding
/// to the `catch` the caller wrote.
pub fn sb_oom() -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: "Requested array size exceeds VM limit".to_string(),
    }
    .into()
}

/// Read `len` characters of a Java `char[]` starting at `from`, bulk where the
/// heap offers it and element by element where it does not.
pub fn sb_read_char_array(
    ctx: &dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: usize,
    len: usize,
) -> Vec<u16> {
    let mut out = vec![0u16; len];
    let read = ctx.read_char_array_into(arr, from, &mut out[..]);
    if read < len {
        for (i, slot) in out.iter_mut().enumerate().skip(read) {
            *slot = match ctx.get_array_element(arr, from + i) {
                Value::Int(c) => c as u16,
                _ => 0,
            };
        }
    }
    out
}

/// Install a fresh, empty payload of `capacity` characters in the receiver's OWN
/// layout, and return the post-GC `this`. `None` is the catchable
/// `OutOfMemoryError` HotSpot raises for an over-large value array.
///
/// A constructor is the one place where the payload cannot decide the layout, so
/// the CLASS decides: an object allocated under the real
/// `java.lang.StringBuilder` declares `value: byte[]`, and writing a `char[]`
/// into it — which is what these constructors did — is the tear itself, minted
/// at birth.
pub fn sb_alloc_payload(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    capacity: usize,
) -> Option<cratonvm_types::ObjectRef> {
    use cratonvm_types::ArrayElementType;
    let layout = sb_view(&*ctx, this).layout;
    let elem = match layout {
        SbLayout::Compact { .. } => ArrayElementType::Byte,
        SbLayout::Synthetic => ArrayElementType::Char,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let handle = scope.root(this);
    // gc-common w9-d (`common-w8b-native-factories-still-single-attempt`): a
    // refused payload reclaims and retries before the `OutOfMemoryError`, as
    // `new byte[capacity]` in `AbstractStringBuilder(int)` would -- the shape
    // `sb_store_units` has had since w8-b. Legal because `this` is rooted by
    // the scope and read back from it below, and the four callers
    // (`native_sb_init_{default,string,charsequence,capacity}`, all
    // registered-only) hold nothing else across the call but a `Vec<u16>`.
    let buf = match scope.try_new_array(elem, capacity) {
        Some(buf) => buf,
        None => {
            // Past HotSpot's `SOFT_MAX_ARRAY_LENGTH` no collection can help;
            // HotSpot throws "Requested array size exceeds VM limit" at once.
            if capacity > (i32::MAX as usize) - 8 || !scope.reclaim_before_alloc_retry() {
                return None;
            }
            match scope.try_new_array(elem, capacity) {
                Some(buf) => buf,
                // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
                None if scope.reclaim_after_failed_alloc_retry() => {
                    scope.try_new_array(elem, capacity)?
                }
                None => return None,
            }
        }
    };
    let this = scope.get(&handle);
    scope.set_field(this, 0, Value::Object(Some(buf)));
    if let SbLayout::Compact { coder_slot } = layout {
        scope.set_field(this, coder_slot, Value::Int(0));
    }
    Some(this)
}

pub fn sb_state(
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
pub fn sb_set_count(ctx: &mut dyn NativeContext, this: cratonvm_types::ObjectRef, count: i32) {
    let resolved = sb_count_slot(&*ctx, this);
    sb_set_count_resolved(ctx, this, count, resolved);
}

/// [`sb_set_count`] with the `count` slot already resolved.
///
/// The append path builds an [`SbView`] immediately before the write and that
/// view already knows where `count` is, so re-resolving it here was a second
/// `object_num_fields` + `class_id_of_object` + memo probe per append for an
/// answer we were holding.
pub fn sb_set_count_resolved(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    count: i32,
    resolved: Option<(cratonvm_types::ClassId, usize)>,
) {
    // Ask the class model where `count` is, exactly as `sb_state` now does —
    // the two must not disagree, and for two years they did. See
    // `sb_field_slot` for the three layouts and the measurement.
    if let Some((class_id, slot)) = resolved {
        // The payload these natives maintain is a `char[]`, and
        // `native_sb_get_coder` reports LATIN1 for it, so keep the real
        // `coder` field agreeing with that answer. Resolve it by name too:
        // the previous unconditional `set_field(this, 1, 0)` was a `coder`
        // write on the JDK 17 layout and a `count` write on the 2-slot
        // synthetic one, and the unconditional `set_field(this, 2, count)`
        // wrote an int over `maybeLatin1` on every JDK 21+ image.
        // `coder` is NOT written here any more. It used to be forced to
        // LATIN1 on every count update, on the premise stated just above —
        // "the payload these natives maintain is a `char[]`". Since the layout
        // migration that premise is false: `sb_store_units` writes the
        // receiver's OWN payload and owns the `coder` that describes it, and a
        // blind zero here would tell every real-bytecode reader that a UTF16
        // payload was LATIN1 — the same wrong answer by a shorter route. The
        // count is all this helper knows, and all it now writes.
        ctx.set_field(this, slot, Value::Int(count));
        // …and `StringBuffer.toStringCache`, when the receiver has one.
        //
        // Every mutating native funnels through here, which makes this the
        // chokepoint the cache needs. `StringBuffer` caches its last
        // `toString()` and every one of its own bodies nulls the field before
        // delegating — but SIX of them do not, because they do not need to:
        // `insert(int,int)`, `(int,long)`, `(int,float)`, `(int,double)`,
        // `(int,boolean)` and `(int,CharSequence)` are
        // `invokespecial AbstractStringBuilder.insert(...)`, and the JDK's own
        // `AbstractStringBuilder.insert` then re-dispatches VIRTUALLY to
        // `this.insert(offset, String.valueOf(i))`, landing back in the
        // synchronized `StringBuffer` override that does null it. A native
        // registered on the abstract base renders the value itself and never
        // re-enters, so that hook never fires. MEASURED: with the 62
        // `StringBuffer` shadows retired, `sb.toString(); sb.insert(0, 7);
        // sb.toString()` answered the STALE "abc" for exactly those six and
        // the right "7abc" for the other thirty.
        //
        // The guard used to be `count_slot + 1 < num_fields` — "this receiver
        // has state after `count`" — followed by an UNCONDITIONAL by-name
        // write. That is a whole-hierarchy name walk on every append of every
        // builder whose layout happens to satisfy the count heuristic, and for
        // a `StringBuilder` the walk can only ever MISS, because that class
        // declares no `toStringCache` at all. A miss is also the most
        // expensive shape of that lookup: it compares every field name in the
        // hierarchy before concluding.
        //
        // Ask the class directly instead, through the same memo the `count`
        // and `coder` lookups use, so the negative answer is computed once per
        // class and the write is skipped outright from then on. This is also
        // strictly closer to what `StringBuffer` specifies: it clears the
        // cache whenever the receiver DECLARES one, rather than whenever a
        // field-count heuristic happens to hold.
        if let Some(cache_slot) = sb_class_slot(&*ctx, class_id, "toStringCache") {
            if cache_slot < ctx.object_num_fields(this) {
                ctx.set_field(this, cache_slot, Value::Object(None));
            }
        }
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

/// Ensure the builder can hold `additional` more characters, in its OWN layout.
///
/// This used to hand back a `char[]` for the caller to write into, which is why
/// every write path converted the payload: there is no `char[]` to hand back for
/// a builder real `AbstractStringBuilder` bytecode constructed, so the old body
/// ALLOCATED one and installed it over the compact `byte[]`. Callers now go
/// through [`sb_append_units`] / [`sb_store_units`], which write the receiver's
/// own representation, and this is the capacity half alone.
///
/// Returns the post-GC `this`: growing allocates, and `ctx.new_array` can move
/// the receiver.
pub fn sb_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    additional: usize,
) -> cratonvm_types::ObjectRef {
    let count = sb_count_units(&*ctx, this);
    sb_reserve_units(ctx, this, count.saturating_add(additional))
}

/// Helper: append a slice of UTF-16 units to a builder.
///
/// Returns the CURRENT (pin-refreshed) `this`: the grow path allocates, and
/// callers that return `this` to Java (every `append` overload — chained
/// `.append(...)` dispatches on that return value) must hand back the post-move
/// address, not their raw pre-call copy (cceres5, live-captured at
/// `JndiName.getAbsoluteName`'s chained appends).
pub fn sb_append_chars(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    chars: &[u16],
) -> cratonvm_types::ObjectRef {
    sb_append_units(ctx, this, chars)
}

/// Helper: append a Rust string to a StringBuilder. Returns the CURRENT
/// `this` (see `sb_append_chars`).
pub fn sb_append_str(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    text: &str,
) -> cratonvm_types::ObjectRef {
    // `encode_utf16().collect()` is a heap allocation on EVERY append, and the
    // hottest appends there are — `append(int)`, `append(long)`,
    // `append(char)`, `append(boolean)` — render at most twenty characters and
    // so never need one. Encode short text into the stack instead and keep the
    // `Vec` for the long tail.
    //
    // A UTF-16 encoding is never LONGER in units than the UTF-8 is in bytes
    // (1-, 2- and 3-byte scalars are one unit each, 4-byte scalars two), so
    // `text.len() <= SB_STACK_UNITS` is a sound bound on the unit count and
    // the buffer cannot overflow.
    if text.len() <= SB_STACK_UNITS {
        let mut buf = [0u16; SB_STACK_UNITS];
        let mut units = 0usize;
        for unit in text.encode_utf16() {
            buf[units] = unit;
            units += 1;
        }
        return sb_append_chars(ctx, this, &buf[..units]);
    }
    let chars: Vec<u16> = text.encode_utf16().collect();
    sb_append_chars(ctx, this, &chars)
}

/// Longest text [`sb_append_str`] encodes without touching the allocator.
pub const SB_STACK_UNITS: usize = 64;

/// The widest `i64` rendering: nineteen digits plus a sign.
pub const SB_DIGITS: usize = 20;

/// `value` rendered into a caller-owned stack buffer — what `Integer.toString`
/// / `Long.toString` produce, without the `String` that `i64::to_string`
/// allocates and frees for every single `append`.
///
/// The `String/Regex` benchmark row makes two builder appends per loop
/// iteration, one of them an `append(int)`, so this is one malloc/free pair per
/// element of the input.
pub fn sb_render_signed(value: i64, buf: &mut [u8; SB_DIGITS]) -> &str {
    // `-i64::MIN` overflows, so accumulate the magnitude unsigned.
    let mut magnitude = value.unsigned_abs();
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        at -= 1;
        buf[at] = b'-';
    }
    // Every byte written above is ASCII, so this cannot fail; the fallback
    // keeps the function total rather than panicking on an impossible state.
    core::str::from_utf8(&buf[at..]).unwrap_or("0")
}

/// The `NullPointerException` a real `AbstractStringBuilder` body raises when it
/// dereferences a null argument, with HotSpot's helpful-NPE wording.
///
/// Ten arms of this registrar used to answer a plausible NON-answer instead — an
/// empty builder, an unchanged builder, `-1` — which is the fabricated-success
/// shape `W2-7-fabricated-success-where-the-spec-mandates-failure.md`
/// inventories: a caller that guards a loop with the exception never leaves it,
/// and nothing anywhere throws. Every one of them is a `native-won` triple of
/// the L2 lane and was MEASURED against jdk-25.0.4+7 by
/// `probes/StringBuilderShadowSweep.java`.
pub fn sb_npe(message: &str) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::NullPointerException {
        message: Some(message.to_string()),
    }
    .into()
}

pub fn native_sb_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // `AbstractStringBuilder()` is `value = new byte[16]` on a real image and
    // `char[16]` on the synthetic one; `sb_alloc_payload` picks by the CLASS,
    // which is the only thing that can decide it for a fresh object.
    let Some(this) = sb_alloc_payload(ctx, this, 16) else {
        return Err(sb_oom());
    };
    sb_set_count(ctx, this, 0);
    Ok(None)
}

pub fn native_sb_init_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    //
    // `AbstractStringBuilder(String)` is `this(str.length() + 16); append(str);`
    // so a null argument fails in `str.length()` — a NullPointerException, not
    // a usable builder of length 0. MEASURED: `new StringBuilder((String) null)`
    // throws on jdk-25.0.4+7 and answered `0` here.
    let chars: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(s))) => read_string_chars(&*ctx, *s),
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"String.length()\" because \"str\" is null",
            ))
        }
        _ => Vec::new(),
    };
    // Capacity first, content second — exactly the JDK's two steps, so an
    // inflating write keeps the `len + 16` capacity the constructor established
    // instead of recomputing one.
    let Some(this) = sb_alloc_payload(ctx, this, chars.len() + 16) else {
        return Err(sb_oom());
    };
    sb_write_units(ctx, this, &chars);
    Ok(None)
}

/// `StringBuilder(CharSequence)` / `StringBuffer(CharSequence)`.
///
/// Without this native the real JDK ctor delegates to
/// `AbstractStringBuilder.<init>(CharSequence)`, which populates the real field
/// layout in a way the SYNTHETIC arm cannot read — picocli's
/// `Help.Ansi.Text` copy-ctor (`new StringBuilder(other.plain)`) came out blank.
/// Intercept it so the receiver stays consistent in either layout, mirroring
/// `native_sb_init_string` but coercing any CharSequence to text.
pub fn native_sb_init_charsequence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // `charsequence_chars` runs arbitrary Java `charAt`, which can allocate and
    // move `this`, so it is rooted across that call and the answer is taken from
    // the handle afterwards.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    // `charsequence_chars`, not `invoke_to_string`: `AbstractStringBuilder`'s own
    // `(CharSequence)` constructor is `this(seq.length() + 16); append(seq);`, so
    // the characters it stores are the ones `append` reads — by `charAt` for
    // anything but the fast-path shapes, never `toString()`. The units form also
    // carries an unpaired surrogate, which the `str` this replaces could not:
    // MEASURED rows c6/c8/c9 of `scratchpad/g26/G26Builder.java`.
    //
    // The doc comment on this function has always said the rest: real
    // `AbstractStringBuilder(CharSequence)` calls `seq.length()`, so a null
    // sequence throws NPE. The code did not — this arm answered an empty
    // builder. MEASURED.
    let chars: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(o))) => charsequence_chars(&mut *scope, *o, None)?,
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"java.lang.CharSequence.length()\" because \"seq\" is null",
            ))
        }
        _ => Vec::new(),
    };
    let this = scope.get(&this_h);
    drop(scope);
    let Some(this) = sb_alloc_payload(ctx, this, chars.len() + 16) else {
        return Err(sb_oom());
    };
    sb_write_units(ctx, this, &chars);
    Ok(None)
}

pub fn native_sb_init_capacity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // `StringIndexOutOfBoundsException`, and `RJdkBridge1`'s `sbidx` rows assert
    // both, one right after the other, precisely because the two negative-length
    // paths in this class do not agree. MEASURED on jdk-25.0.3.9:
    // `new StringBuilder(-1)` -> `NegativeArraySizeException: -1`,
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
    let Some(this) = sb_alloc_payload(ctx, this, cap) else {
        return Err(sb_oom());
    };
    sb_set_count(ctx, this, 0);
    Ok(None)
}

pub fn native_sb_append_string(
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

pub fn native_sb_append_int(
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
    let mut digits = [0u8; SB_DIGITS];
    let this = sb_append_str(ctx, this, sb_render_signed(i64::from(val), &mut digits));
    Ok(Some(Value::Object(Some(this))))
}

pub fn native_sb_append_char(
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

pub fn native_sb_append_null(
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

pub fn native_sb_append_codepoint(
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
pub fn native_sb_repeat_codepoint(
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
pub fn native_sb_repeat_charsequence(
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
pub fn native_sb_append_char_array_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // `append(char[] str, int offset, int len)` opens with
    // `checkRange(offset, offset + len, str.length)`, so the array is
    // dereferenced BEFORE the window is judged: a null `str` is a
    // NullPointerException whatever the offsets are, including the
    // `(-1, -1)` pair that would otherwise be a range failure. This arm
    // returned the builder unchanged. MEASURED.
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot read the array length because \"str\" is null",
            ))
        }
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
    // The source is a Java `char[]`; the destination is whichever layout the
    // receiver holds. The `bulk_array_copy` straight into the builder's buffer
    // that used to stand here cannot: for a compact receiver there is no
    // `char[]` to copy into, and the `sb_ensure_capacity` that produced one
    // CONVERTED the payload. Read the window once, then append through the
    // layout-aware writer.
    let src = sb_read_char_array(ctx, arr, start, copy_len);
    let this = sb_append_units(ctx, this, &src);
    Ok(Some(Value::Object(Some(this))))
}

/// `StringBuilder.append(char[])` — full-array variant.
pub fn native_sb_append_char_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // `append(char[] str)` is `int len = str.length;` — a null array throws,
    // and this is the overload of the three null-append shapes that does NOT
    // append the text "null" (`append(String)` and `append(CharSequence)` do).
    // MEASURED: the three really do differ.
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot read the array length because \"str\" is null",
            ))
        }
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let n = ctx.array_length(arr);
    let src = sb_read_char_array(ctx, arr, 0, n);
    let this = sb_append_units(ctx, this, &src);
    Ok(Some(Value::Object(Some(this))))
}

pub fn native_sb_append_boolean(
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

pub fn native_sb_append_long(
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
    let mut digits = [0u8; SB_DIGITS];
    let this = sb_append_str(ctx, this, sb_render_signed(val, &mut digits));
    Ok(Some(Value::Object(Some(this))))
}

pub fn native_sb_append_double(
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

pub fn native_sb_append_float(
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
pub fn invoke_to_string(
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
pub fn invoke_to_string_opt(
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
/// Is `obj` a real `java.lang.String` INSTANCE - not merely something whose
/// class id says `java/lang/String`?
///
/// A reference array has no class of its own in this VM's class store, so it
/// carries its COMPONENT's class id: `class_id_of_object(new String[2])` answers
/// `java/lang/String`. Every `class_name == "java/lang/String"` test in the
/// rendering path therefore matched a `String[]`, and each one then read slot 0
/// as `String.value` - which is the array's FIRST ELEMENT POINTER, decoded as
/// the `(tag, payload)` pair of a `Value` cell.
///
/// MEASURED (`probes/AppendArrayProbe`), and no GC is involved - the collector's
/// corrupt-cell guard fires at `collections_now=0`:
///
/// ```text
///   sb.append(new String[]{"y","n"})
///   String.valueOf((Object) new String[]{"y","n"})
///   CratonVM   ""  + `gc::guard: corrupt Value cell` + `NullPointerException:
///                    Cannot read the array length because "this.value" is null`
///   HotSpot    "[Ljava.lang.String;@<hash>"
/// ```
///
/// This is the producer `corrupt-value-cell-is-fatal-on-three-of-four-collectors`
/// left open (retired as
/// `corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`). That record reasoned the reference "is already stale when the
/// native is entered" because the String fast path has no allocation between
/// entry and the read. The reference was never stale; the READ was never of a
/// String. Its Spring Boot witness reaches here through
/// `ItemMetadata.newProperty(.., new String[]{"y","n"}, ..)` and AssertJ's
/// `createDescription`, which is a `sb.append(Object)` over that array.
///
/// The kind check is the same one the boxed-wrapper fast path in
/// [`invoke_to_string_units_opt`] has always carried, in its own words: "MUST
/// exclude arrays: a heap array's num_slots is its LENGTH". Three of the four
/// String doors did not.
///
/// `vm_exec::is_string_object` is the same predicate on the VM side and has
/// always screened the kind; this is that rule, brought to the natives.
pub fn is_plain_string(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> bool {
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return false;
    }
    let class_id = ctx.class_id_of_object(obj);
    // JIT round 19 wave 2, lane funnel2 (NIO2-17-5,
    // `CRATONVM_CLASS_FACTS_MEMO_STRING=1`, default OFF): the same name test,
    // answered from the per-thread class-facts memo instead of an allocated
    // copy of the name per call.
    if crate::class_facts_memo::string_fact_enabled() {
        return crate::class_facts_memo::name_facts(ctx, class_id).is_some_and(|f| f.is_string());
    }
    ctx.class_name_of_id(class_id).as_deref() == Some("java/lang/String")
}

pub fn invoke_to_string_units_opt(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Option<Vec<u16>>, cratonvm_types::error::MethodCallFailed> {
    if let Some(units) = to_string_fast_units(ctx, obj) {
        return Ok(Some(units));
    }
    java_to_string_units_opt(ctx, obj)
}

/// The answers of [`invoke_to_string_units_opt`] that run no Java: a `String`,
/// a boxed primitive (`None` for everything else, whose `toString()` has to
/// be called). Split out (JIT round 16 wave 9, lane sbtrace) so the
/// `String.valueOf(Object)` / builder `append(Object)` natives can tell the
/// upcall case apart and run it through the JDK bodies
/// ([`tostring_upcall_frames_enabled`]).
pub fn to_string_fast_units(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<Vec<u16>> {
    // MUST exclude arrays, for the reason the wrapper fast path below says in
    // its own words -- and this is the site that was missing it.
    //
    // A reference array has no class of its own in the class store, so it
    // carries its COMPONENT's class id: `class_id_of_object(new String[2])`
    // answers `java/lang/String`, and the name test below matched. This native
    // then read slot 0 as `String.value`, i.e. it decoded the array's FIRST
    // ELEMENT POINTER as the `(tag, payload)` pair of a `Value` cell.
    //
    // MEASURED (`probes/AppendArrayProbe`, no GC involved -- the guard fires at
    // `collections_now=0`):
    //
    // ```text
    //   sb.append(new String[]{"y","n"})
    //   CratonVM   ""            + `gc::guard: corrupt Value cell` + an NPE
    //                              downstream on the String it minted with a
    //                              null `value`
    //   HotSpot    "[Ljava.lang.String;@<hash>"
    // ```
    //
    // That is the whole of the producer
    // `corrupt-value-cell-is-fatal-on-three-of-four-collectors` was looking for.
    // Its own reasoning was that the reference must already be stale on entry
    // because the String fast path has no allocation between entry and the
    // read; the reference was never stale, and the read was never of a String.
    // The Spring Boot witness reaches it through
    // `ItemMetadata.newProperty(.., new String[]{"y","n"}, ..)` and AssertJ's
    // `createDescription`, which is a `sb.append(Object)` over that array.
    //
    // `read_string`'s structural decode is gated with it: it reads the same two
    // slots, so it has the same hazard and none of the class test's excuse.
    //
    // An array falls through to the `invoke_virtual("toString")` at the bottom,
    // which is `Object.toString()` -- the identity rendering HotSpot gives.
    let is_array = ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array;

    // Fast path: if it's already a String object, read its code units.
    //
    // The class test comes FIRST and `read_string` stays as the fallback: on a
    // class the VM cannot name, `read_string`'s structural decode is still the
    // best answer available, and keeping it means this refactor cannot lose a
    // route it used to serve. `read_string` is gated by the array test too: it
    // reads the same two slots, so it has the same hazard and none of the class
    // test's excuse.
    if !is_array {
        if is_plain_string(&*ctx, obj) {
            return Some(read_string_chars(&*ctx, obj));
        }
        if let Some(s) = ctx.read_string(obj) {
            return Some(s.encode_utf16().collect());
        }
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
        // JIT round 19 wave 3, lane funnel3 (NIO2-17-5 rung 2,
        // `CRATONVM_CLASS_FACTS_MEMO_WRAPPER=1`, default OFF): the boxed kind
        // from the per-thread class-facts memo instead of an allocated copy
        // of the class name per call. Off: the same classification of the
        // name.
        use crate::class_facts_memo::BoxKind;
        let box_kind = match crate::class_facts_memo::rung_facts(
            &*ctx,
            crate::class_facts_memo::Rung::Wrapper,
            class_id,
        ) {
            Some(facts) => facts.and_then(crate::class_facts_memo::NameFacts::box_kind),
            None => BoxKind::from_name(&ctx.class_name_of_id(class_id).unwrap_or_default()),
        };
        if let Some(kind) = box_kind {
            match ctx.get_field(obj, 0) {
                Value::Int(v) => {
                    let formatted = if kind == BoxKind::Boolean {
                        if v != 0 { "true" } else { "false" }.to_string()
                    } else if kind == BoxKind::Character {
                        // A boxed `Character` can hold a lone surrogate --
                        // `Character.valueOf('\ud800')` is legal -- and
                        // `char::from_u32` rejects exactly those, so this
                        // printed '?' for a value it could have passed through
                        // untouched. Emit the raw unit.
                        return Some(vec![v as u16]);
                    } else if kind == BoxKind::Byte {
                        (v as i8).to_string()
                    } else if kind == BoxKind::Short {
                        (v as i16).to_string()
                    } else {
                        // Integer
                        v.to_string()
                    };
                    return Some(formatted.encode_utf16().collect());
                }
                Value::Long(v) => return Some(v.to_string().encode_utf16().collect()),
                // Use the Java-spec formatters (NOT raw `{}`), so a boxed Double/Float
                // rendered via String.valueOf(Object) / StringBuilder.append(Object) /
                // object string-concat matches `Double.toString` — incl. the
                // 10^-3..10^7 scientific-notation threshold, "Infinity", and "-0.0".
                // Raw `format!("{}")` dropped the ".0", printed "inf"/"-0", and never
                // used E-notation (e.g. boxed 1e7 -> "10000000.0", -0.0 -> "-0").
                Value::Float(v) => return Some(format_float(v).encode_utf16().collect()),
                Value::Double(v) => return Some(format_double(v).encode_utf16().collect()),
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
    // stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md
    // are byte-identical to HotSpot with
    // this gone.

    None
}

/// The `obj.toString()` upcall of [`invoke_to_string_units_opt`], for an object
/// [`to_string_fast_units`] does not answer.
pub fn java_to_string_units_opt(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Option<Vec<u16>>, cratonvm_types::error::MethodCallFailed> {
    // Call obj.toString() via virtual dispatch. A Java exception from the
    // override is observable and must reach the caller; only an absent or
    // malformed return value uses the historical identity fallback.
    //
    // gen r4w3/rooting: the `Ok(_)` fallback below reads `obj` again
    // (`heap_kind_of`, `array_descriptor_for`, `identity_hash_code`) after the
    // upcall, which can have moved it. Pin across the call and re-derive; the
    // reads after the unpin run no bytecode.
    let obj_pin = ctx.pin_native_root(obj);
    let result = ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
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
                crate::lang_class::array_descriptor_for(ctx, obj).class_text(&*ctx)
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
pub fn invoke_to_string_units(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Result<Vec<u16>, cratonvm_types::error::MethodCallFailed> {
    Ok(invoke_to_string_units_opt(ctx, obj)?.unwrap_or_else(|| "null".encode_utf16().collect()))
}

pub fn native_sb_append_object(
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
        Some(Value::Object(Some(obj))) => match to_string_fast_units(&mut *scope, *obj) {
            Some(units) => units,
            None => {
                // The upcall case: wave 9 (lane sbtrace) runs the builder's own
                // JDK `append(Object)` body, so a throwing `toString()` lists
                // `String.valueOf` and `StringBuilder.append` as HotSpot does.
                let this_now = scope.get(&this_handle);
                if let Some(result) = sb_append_object_through_bytecode(&mut *scope, this_now, *obj)
                {
                    return result;
                }
                java_to_string_units_opt(&mut *scope, *obj)?
                    .unwrap_or_else(|| "null".encode_utf16().collect())
            }
        },
        _ => "null".encode_utf16().collect(),
    };
    let this = scope.get(&this_handle);
    let this = sb_append_chars(&mut *scope, this, &units);
    Ok(Some(Value::Object(Some(this))))
}

/// `CRATONVM_TOSTRING_UPCALL_FRAMES` (JIT round 16 wave 9, lane sbtrace; a
/// correctness fix, default ON, `0` is the kill switch): when
/// `String.valueOf(Object)` or a builder's `append(Object)` is served by a
/// native and has to call the argument's `toString()`, the call goes through
/// the JDK bodies of those methods (`invoke_special_bytecode_only`), so an
/// exception the `toString()` throws lists HotSpot's `String.valueOf` and
/// `StringBuilder.append` frames (`R16StringsInterpSb`;
/// `r16-strings-jdk-only-interpreter-serves-refused-stringbuilder-natives-FIXED-20261005.md`,
/// its `--compatible` half). A `String` or boxed-primitive argument, which runs
/// no Java, is untouched. Read once (the native-builtins gate idiom of
/// `preconditions::bounds_raise_bytecode_enabled`).
pub fn tostring_upcall_frames_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `sb.append(obj)` for an `obj` whose `toString()` must run, through the
/// receiver class's own JDK body (`return append(String.valueOf(obj))`;
/// `StringBuffer`'s also clears `toStringCache`). Its `String.valueOf` call is
/// [`native_string_value_of_object`] where that is registered, which runs the
/// JDK body in turn ([`string_value_of_through_bytecode`]); its
/// `append(String)` is the ordinary native.
///
/// `None` -- the caller calls `toString()` itself, as before -- when the switch
/// is off, the receiver is not a `StringBuilder` / `StringBuffer` in the JDK's
/// compact layout, or the class declares no such body (the synthetic image,
/// mock contexts). Once the body runs, its answer is final: `toString()` has
/// been called, so a failure must not be followed by a second call.
pub fn sb_append_object_through_bytecode(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    obj: cratonvm_types::ObjectRef,
) -> Option<MethodCallResult> {
    if !tostring_upcall_frames_enabled() {
        return None;
    }
    let receiver = ctx.class_id_of_object(this);
    let class = ctx.class_name_of_id(receiver)?;
    let desc = match class.as_str() {
        "java/lang/StringBuilder" => "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
        "java/lang/StringBuffer" => "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
        _ => return None,
    };
    if !matches!(sb_view(&*ctx, this).layout, SbLayout::Compact { .. }) {
        return None;
    }
    if !ctx.class_declares_method(receiver, "append", desc) {
        return None;
    }
    Some(ctx.invoke_special_bytecode_only(
        &class,
        "append",
        desc,
        &[Value::Object(Some(this)), Value::Object(Some(obj))],
    ))
}

/// `String.valueOf(obj)` for an `obj` whose `toString()` must run, through the
/// JDK body (`obj == null ? "null" : obj.toString()`), so its frame is on the
/// stack of the upcall. `None` -- the caller calls `toString()` itself -- when
/// `CRATONVM_TOSTRING_UPCALL_FRAMES` is off or `String` declares no such body
/// (the synthetic image, mock contexts). As for the builders, once the body
/// runs its answer is final.
pub fn string_value_of_through_bytecode(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<MethodCallResult> {
    const DESC: &str = "(Ljava/lang/Object;)Ljava/lang/String;";
    if !tostring_upcall_frames_enabled() {
        return None;
    }
    let string = ctx.class_id_by_name("java/lang/String")?;
    if !ctx.class_declares_method(string, "valueOf", DESC) {
        return None;
    }
    Some(ctx.invoke_special_bytecode_only(
        "java/lang/String",
        "valueOf",
        DESC,
        &[Value::Object(Some(obj))],
    ))
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
pub fn charsequence_chars(
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
pub fn charsequence_length(
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
pub fn charsequence_fast_units(
    ctx: &mut dyn NativeContext,
    cs: cratonvm_types::ObjectRef,
) -> Result<Option<Vec<u16>>, MethodCallFailed> {
    let cid = ctx.class_id_of_object(cs);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    // The array exclusion `invoke_to_string_units_opt` carries, for the same
    // reason: a reference array answers its COMPONENT's class id, so
    // `String[]` passes a `name == "java/lang/String"` test and this would read
    // its first element pointer as `String.value`. Nothing routes an array here
    // today -- an array is not a `CharSequence` -- but this is the second of
    // the two doors that read String slots off a class-name test, and the first
    // one was reached.
    if is_plain_string(&*ctx, cs) {
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
        // `docs/feature-designs/native-dispatch-architecture.md`
        // rather than fixed from here.
        return Ok(crate::phases_late::charset_buffers::cb_read_text(ctx, cs)
            .map(|t| t.encode_utf16().collect()));
    }
    Ok(None)
}

/// C36: `AbstractStringBuilder.append(CharSequence)` — same JDK-layout
/// mismatch as the 3-arg variant below; intercept to append the whole
/// sequence natively.
pub fn native_sb_append_charsequence(
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
pub fn native_sb_append_charsequence_off_len(
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
pub fn has_unpaired_surrogate(units: &[u16]) -> bool {
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
pub fn sb_string_from_units(
    ctx: &mut dyn NativeContext,
    units: &[u16],
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if units.iter().all(|&u| u < 0x80) {
        if units.len() <= 256 {
            let mut ascii = [0u8; 256];
            for (dst, &u) in ascii[..units.len()].iter_mut().zip(units) {
                *dst = u as u8;
            }
            if let Ok(text) = std::str::from_utf8(&ascii[..units.len()]) {
                return Ok(ctx.create_string_uninterned_gc_safe(text));
            }
        } else {
            let ascii: Vec<u8> = units.iter().map(|&u| u as u8).collect();
            if let Ok(text) = std::str::from_utf8(&ascii) {
                return Ok(ctx.create_string_uninterned_gc_safe(text));
            }
        }
    }
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
    init_string_from_units_or_oom(ctx, obj, units, "String from UTF-16 units")
}

/// Apply a text mapping (`String.toUpperCase`'s, say) to UTF-16 `units` that
/// may hold UNPAIRED surrogates: every maximal well-formed run is decoded,
/// mapped by `f` and re-encoded; every lone surrogate is copied through as
/// itself, which is what the JDK's `toUpperCase` / `toLowerCase` do with one
/// (it has no case mapping). nio21 wave 9 L5.
pub fn map_units_keeping_lone_surrogates(
    units: &[u16],
    mut f: impl FnMut(&str) -> String,
) -> Vec<u16> {
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
                out.extend(f(&String::from_utf16_lossy(&run)).encode_utf16());
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
        out.extend(f(&String::from_utf16_lossy(&run)).encode_utf16());
    }
    out
}

/// A fresh `String` of `units` for the `java/lang/String` natives that used
/// to end in `String::from_utf16_lossy` + `create_string_uninterned*`
/// (`substring`, `concat`, `join`; nio21 wave 9 L5). Well-formed units keep
/// that exact path (`gc_safe` picks which `create_string_uninterned*` it
/// was); only units holding an UNPAIRED surrogate take
/// [`sb_string_from_units`], so `"𐐀x".substring(1)` is `"\uDC00x"` as on
/// HotSpot 25 rather than `"�x"`. The switch
/// (`CRATONVM_STRING_NATIVE_LOSSLESS_UNITS`, default ON) is read only on
/// that rare arm.
pub fn string_native_result_from_units(
    ctx: &mut dyn NativeContext,
    units: &[u16],
    gc_safe: bool,
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if units.iter().all(|&u| u < 0x80) {
        if units.len() <= 256 {
            let mut ascii = [0u8; 256];
            for (dst, &u) in ascii[..units.len()].iter_mut().zip(units) {
                *dst = u as u8;
            }
            if let Ok(text) = std::str::from_utf8(&ascii[..units.len()]) {
                return Ok(if gc_safe {
                    ctx.create_string_uninterned_gc_safe(text)
                } else {
                    ctx.create_string_uninterned(text)
                });
            }
        } else {
            let ascii: Vec<u8> = units.iter().map(|&u| u as u8).collect();
            if let Ok(text) = std::str::from_utf8(&ascii) {
                return Ok(if gc_safe {
                    ctx.create_string_uninterned_gc_safe(text)
                } else {
                    ctx.create_string_uninterned(text)
                });
            }
        }
    }
    if has_unpaired_surrogate(units)
    {
        return sb_string_from_units(ctx, units);
    }
    let text = String::from_utf16_lossy(units);
    Ok(if gc_safe {
        ctx.create_string_uninterned_gc_safe(&text)
    } else {
        ctx.create_string_uninterned(&text)
    })
}

/// `CRATONVM_STRING_LITERAL_IDENTITY` (JIT round 17 wave 4, lane sbempty; a
/// correctness fix, default ON, `=0` kill switch): where a JDK 25 body answers
/// a string LITERAL, the native standing in for it answers that literal (the
/// pooled object), not a fresh equal copy -- the empty builder / empty
/// substring (`""`, [`sb_new_string_from_units`]) and `String.valueOf(Object)`
/// of `null` (`"null"`) and of a `Boolean` (`"true"` / `"false"`).
/// Per VM (j27 wave 7, lane T1LOOPSTATIC, backlog row SBEMPTY17-4): the
/// switch is read once when the VM's native registry is built
/// (`NativeMethodRegistry::string_literal_identity`) and answered through
/// `ctx`; it was a process `OnceLock`, so every VM of a process got the first
/// VM's answer (AGENTS.md: no process globals for compatibility state). Asked
/// only on the arms that may answer a literal.
pub fn string_literal_identity_enabled(ctx: &dyn NativeContext) -> bool {
    ctx.string_literal_identity()
}

/// The String a JDK 25 `newString`-shaped body answers for `units`: the
/// canonical `""` literal (the object `ldc ""` and `"".intern()` answer) when
/// `units` is empty, else a FRESH String through [`sb_string_from_units`].
///
/// JDK 25 spells the empty arm out in every body this serves:
/// `StringBuilder.toString()` and `StringBuffer.toString()` open with
/// `if (length() == 0) return "";`, and `AbstractStringBuilder.substring`
/// ends in `StringLatin1.newString` / `StringUTF16.newString`, whose first
/// statement is `if (len == 0) return "";`. So on HotSpot
/// `new StringBuilder().toString() == ""` holds, and the natives answered a
/// fresh empty String instead (`R17StringsSb` `empty ... isLit=false`;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// Only for those bodies. A `String` CONSTRUCTOR (`new String(char[0])`,
/// `String.valueOf(char[])`, which is `new String(data)`) must answer a new
/// object, and so must `String.join` / `StringJoiner` (`new String(value,
/// coder)`), so [`sb_string_from_units`] itself stays fresh for every caller.
///
/// `intern_string("")` is a pool hit after the first `ldc ""` and allocates
/// nothing; nothing the callers hold is dereferenced after it.
pub fn sb_new_string_from_units(
    ctx: &mut dyn NativeContext,
    units: &[u16],
) -> Result<cratonvm_types::ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if units.is_empty() && string_literal_identity_enabled(&*ctx) {
        return Ok(ctx.intern_string(""));
    }
    sb_string_from_units(ctx, units)
}

/// Fill the already-allocated `java.lang.String` `this` from `units` through
/// [`NativeContext::init_string_from_units`], on the shared GC-and-retry
/// ladder, and answer `this`'s current address.
///
/// gc-common w20-b (`common-w19g-init-string-from-units-never-collects`):
/// the VM's `init_string_from_units` allocates the backing array with the
/// NON-collecting `try_alloc_array_full`, so a full young space made it
/// answer `false` with most of the heap reclaimable. The two
/// `String(char[])` constructors ignored that `false` and returned normally
/// -- a published String with a null `value`, whose first `length()` threw
/// `NullPointerException` from inside `java.lang.String` -- and the other
/// callers raised a spurious `OutOfMemoryError`. Here a refusal gets one
/// `reclaim_before_alloc_retry()` (the ladder every other native allocation
/// retries on: a collection, the overhead-limit check, the soft-reference
/// clearing) and one retry, with `this` pinned across it -- `units` are Rust
/// memory, so `this` is the only reference at risk -- and only then the
/// catchable `OutOfMemoryError` ("Java heap space (`what`)"). Every caller
/// that fills a String in place goes through here; the primitive keeps its
/// non-collecting contract.
pub fn init_string_from_units_or_oom(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    units: &[u16],
    what: &str,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let mut ok = ctx.init_string_from_units(this, units);
    if !ok && ctx.reclaim_before_alloc_retry() {
        let this_now = ctx.read_native_pin(this_pin, this);
        ok = ctx.init_string_from_units(this_now, units);
        // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
        if !ok && ctx.reclaim_after_failed_alloc_retry() {
            let this_now = ctx.read_native_pin(this_pin, this);
            ok = ctx.init_string_from_units(this_now, units);
        }
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if !ok {
        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
            message: format!("Java heap space ({what})"),
        }
        .into());
    }
    Ok(this_now)
}

pub fn native_sb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `sb_read_chars` reads whichever layout the receiver holds. The version of
    // this that recognised only a `char[]` answered "" for a compact receiver —
    // a FABRICATION, not a refusal: the caller cannot tell an empty builder from
    // one whose content the native declined to look at, and that is the visible
    // half of the "every append silently discarded, `toString()` empty, `rc=0`"
    // behaviour `H22` measured on this registrar.
    let chars = sb_read_chars(&*ctx, this);
    // `StringBuilder.toString()` must return a *fresh* String distinct from any
    // equal literal — the JVM spec only pools literals and `intern()`. Routing
    // it through the interned pool made `==` wrongly report identity (e.g.
    // `sb.toString() == "literal"`), breaking identity-based symbol comparisons
    // such as xerces' `NamespaceSupport`.
    //
    // `_gc_safe` (inside `sb_string_from_units`): the units are already
    // Rust-owned and `this` is not dereferenced again below, so a moving young
    // GC here is safe. Without it a `toString()`-heavy hot loop (e.g.
    // `Response.toAbsolute()`) hard-aborts the process on young-gen exhaustion
    // instead of collecting and continuing — see
    // docs/feature-designs/native-dispatch-architecture.md.
    //
    // The ONE exception is the empty builder: JDK 25's `toString()` (both
    // `StringBuilder`'s and `StringBuffer`'s) opens with `if (length() == 0)
    // return "";`, so that answer IS the literal (round 17 wave 4, lane
    // sbempty, `CRATONVM_STRING_LITERAL_IDENTITY`).
    let result = sb_new_string_from_units(ctx, &chars)?;
    Ok(Some(Value::Object(Some(result))))
}

