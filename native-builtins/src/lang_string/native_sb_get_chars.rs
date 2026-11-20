#[allow(unused_imports)]
use super::*;

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
/// `docs/feature-designs/native-dispatch-architecture.md`.
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
pub fn native_sb_get_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

    let view = sb_view(&*ctx, this);
    let count = view.count as i32;
    // (1) Source-range check FIRST, with `SIOOBE_FORMATTER`. This precedes the
    // null-`dst` dereference, which is why a bad source window beats a null
    // destination rather than the other way round.
    if src_begin < 0 || src_end > count || src_begin > src_end {
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_range(src_begin, src_end, count).into(),
        );
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
    // Decoded through the receiver's own layout: reading the payload array
    // element by element is a LATIN1 read of a UTF-16 unit whenever the two
    // disagree, which is the truncation `chars()` measured.
    let src = sb_units_range_view(&*ctx, &view, src_begin as usize, src_begin as usize + n);
    // `set_array_element` is infallible (returns no Result), so every store is
    // guarded by the explicit destination-range check above rather than relying
    // on a downstream bounds error.
    for (i, &u) in src.iter().enumerate() {
        ctx.set_array_element(dst, dst_begin as usize + i, Value::Int(i32::from(u)));
    }
    Ok(None)
}

pub fn native_sb_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, count) = sb_state(ctx, this);
    Ok(Some(Value::Int(count)))
}

/// `AbstractStringBuilder.getCoder()B`.
///
/// For a compact receiver this is the `coder` FIELD, not a value derived from
/// the content, and the difference is load-bearing: the JDK inflates and never
/// deflates, so a builder that has held a non-LATIN1 character keeps
/// `coder == UTF16` even while its current content is all LATIN1. Deriving the
/// answer from the content would then contradict `getValue()`'s array, and
/// `String.nonSyncContentEquals` — the one caller that matters — compares the
/// two only when `coder() == sb.getCoder()`, so a disagreement is a wrong
/// answer, not a slow one.
pub fn native_sb_get_coder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let v = sb_view(&*ctx, this);
    match v.layout {
        SbLayout::Compact { .. } => Ok(Some(Value::Int(i32::from(v.utf16)))),
        // The synthetic `char[]` payload is not compacted at all, so the coder
        // is whatever `native_sb_get_value`'s synthesised array will use — and
        // the two must agree by construction.
        SbLayout::Synthetic => {
            let chars = sb_read_chars(&*ctx, this);
            Ok(Some(Value::Int(i32::from(chars.iter().any(|&u| u > 0xFF)))))
        }
    }
}

/// `AbstractStringBuilder.getValue()[B`.
///
/// Companion to [`native_sb_get_coder`]: the compact `byte[]` the real JDK's
/// `String.nonSyncContentEquals` reads, paired with the coder that describes it.
///
/// For a compact receiver that array IS the payload and is handed back
/// directly — the JDK's own accessor returns the field, callers bound their read
/// by `length()`, and copying it would be pure waste on a path
/// `String.contentEquals` takes. For a synthetic receiver there is no such array
/// and one is built, in the SAME layout CratonVM's own Strings use
/// (`vm_object::create_java_string`): LATIN1 one byte per char, UTF16 two
/// little-endian bytes per char, low byte first.
pub fn native_sb_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = sb_view(&*ctx, this);
    if let SbLayout::Compact { .. } = v.layout {
        if let Some(arr) = v.buf {
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // Snapshot the chars into a Rust-local Vec BEFORE allocating, so the moving
    // GC that `new_array` may trigger cannot leave us with a stale `this`.
    let chars = sb_units_range_view(&*ctx, &v, 0, v.count);
    let latin1 = chars.iter().all(|&u| u <= 0xFF);
    let byte_len = if latin1 { chars.len() } else { chars.len() * 2 };
    let arr = ctx.new_array(ArrayElementType::Byte, byte_len);
    sb_put_units_at(
        ctx,
        arr,
        0,
        &chars,
        SbLayout::Compact { coder_slot: 1 },
        !latin1,
    );
    Ok(Some(Value::Object(Some(arr))))
}

/// The class every builder bounds failure raises.
pub const SIOOBE: &str = "java/lang/StringIndexOutOfBoundsException";

/// The JDK descriptor of builder method `params` on `class`: `ret`, or, for
/// `None`, the class itself (the covariant `L<class>;` every chaining builder
/// method returns).
pub fn sb_body_descriptor(class: &str, params: &str, ret: Option<&str>) -> String {
    match ret {
        Some(r) => format!("{params}{r}"),
        None => format!("{params}L{class};"),
    }
}

/// Raise a builder bounds failure the native has ALREADY decided on through
/// the JDK bodies of the method it serves, so the exception lists HotSpot's
/// frames: `AbstractStringBuilder.<name>` and, when the receiver's class
/// overrides it (`StringBuilder.charAt` is a visibility bridge calling `super.charAt`), that
/// override too (JIT round 16 wave 9, lane sbtrace;
/// `r16-trace-compatible-sb-charat-sioobe-has-no-jdk-frames-FIXED-20261001.md`).
///
/// How the two frames arise. The receiver class's own body runs first
/// (`invoke_special_bytecode_only`, never the native). Its `super.<name>` is an
/// ordinary `invokespecial`, which under `--compatible` reaches this native
/// AGAIN; that nested call finds the receiver's class (or
/// `AbstractStringBuilder`) as its innermost frame and runs
/// `AbstractStringBuilder`'s body instead, which raises. A false "nested"
/// (some other method of those classes calling `<name>`) costs only the
/// override's frame, never the exception, and the nested call never runs the
/// override again, so the nesting is bounded at two levels.
///
/// Screens (each `None`: the caller raises as before, fail-closed):
/// `CRATONVM_BOUNDS_RAISE_BYTECODE` off; a builder not in the JDK's compact
/// layout (the synthetic `char[]` class has no such bodies); a `count` field
/// that disagrees with the native's view (then the body might not raise, and
/// for a mutator it would MUTATE before the native raises); a body the class
/// does not declare; a body that did not throw `expected`.
pub fn sb_raise_through_builder_body(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    name: &str,
    params: &str,
    ret: Option<&str>,
    args: &[Value],
    expected: &str,
) -> Option<MethodCallFailed> {
    if !crate::preconditions::bounds_raise_bytecode_enabled() {
        return None;
    }
    let v = sb_view(&*ctx, this);
    if !matches!(v.layout, SbLayout::Compact { .. }) {
        return None;
    }
    let (_, count_slot) = v.count_slot?;
    if ctx.get_field(this, count_slot) != Value::Int(v.count as i32) {
        return None;
    }
    const ASB: &str = "java/lang/AbstractStringBuilder";
    let receiver = ctx.class_id_of_object(this);
    let receiver_name = ctx.class_name_of_id(receiver)?;
    // Nested: the innermost frame is the receiver's class (the override's
    // `super.<name>`), or `AbstractStringBuilder` itself. The second test is
    // the recursion guard: were a frame ever pushed for the served native,
    // the nested call must still not run the override again.
    let asb_id = ctx.class_id_by_name(ASB);
    let nested = match ctx.frame_class_ids().first() {
        Some(innermost) => *innermost == receiver || Some(*innermost) == asb_id,
        None => false,
    };
    let own_desc = sb_body_descriptor(&receiver_name, params, ret);
    // `declared_method_access_flags`, NOT `class_declares_method`: the
    // receiver's "override" is often javac's VISIBILITY BRIDGE (a public class
    // re-publishing a public method of its package-private superclass:
    // `StringBuilder.charAt(I)C` / `setCharAt(IC)V` are `ACC_BRIDGE |
    // ACC_SYNTHETIC`, line 91 = the class declaration), which
    // `class_declares_method` deliberately excludes. HotSpot runs the bridge
    // and lists its frame, so it must run here too (wave 9b: `--compatible`
    // `sb-charAt-*` read depth 11, `StringBuilder.charAt:91` missing).
    let (class, desc) = if receiver_name != ASB
        && !nested
        && ctx
            .declared_method_access_flags(receiver, name, &own_desc)
            .is_some()
    {
        (receiver_name, own_desc)
    } else {
        let asb_desc = sb_body_descriptor(ASB, params, ret);
        let asb = asb_id?;
        if !ctx.class_declares_method(asb, name, &asb_desc) {
            return None;
        }
        (ASB.to_string(), asb_desc)
    };
    let result = ctx.invoke_special_bytecode_only(&class, name, &desc, args);
    crate::preconditions::accept_bytecode_raise(ctx, result, Some(expected))
}

pub fn native_sb_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let v = sb_view(&*ctx, this);
    let count = v.count as i32;
    if index < 0 || index >= count {
        // Message as well as class: `AbstractStringBuilder.charAt` delegates to
        // `String.checkIndex` → `Preconditions.checkIndex(index, count,
        // SIOOBE_FORMATTER)`. Its wording is TRANSCRIBED from HotSpot and
        // asserted by the probe rows n21/n22/n23 (`Index 5 out of bounds for
        // length 2`), which match today and must keep matching.
        //
        // JIT round 16 wave 8 (lane trace; `CRATONVM_BOUNDS_RAISE_BYTECODE`):
        // raised by the real `String.checkIndex(index, count)` body (static,
        // layout-free, unlike the builder's own `charAt`, whose body reads the
        // compact layout this builder may not have), so the trace carries the
        // eight JDK frames from `Preconditions$1.apply` to `String.checkIndex`.
        // HotSpot also lists `AbstractStringBuilder.charAt` and
        // `StringBuilder.charAt`, which this native stands for: wave 9 (lane
        // sbtrace) runs those two bodies first on a compact builder
        // (`sb_raise_through_builder_body`, whose screens say when it cannot),
        // and keeps the `String.checkIndex` raise as the fallback.
        // `r16-trace-compatible-sb-charat-sioobe-has-no-jdk-frames-FIXED-20261001.md`.
        if let Some(raised) =
            sb_raise_through_builder_body(ctx, this, "charAt", "(I)", Some("C"), args, SIOOBE)
        {
            return Err(raised);
        }
        if let Some(raised) = crate::preconditions::raise_through_bytecode(
            ctx,
            "java/lang/String",
            "checkIndex",
            "(II)V",
            &[Value::Int(index), Value::Int(count)],
            Some("java/lang/StringIndexOutOfBoundsException"),
        ) {
            return Err(raised);
        }
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    }
    // `buf.unwrap()` here once ABORTED THE VM: a builder whose slot 0 was not a
    // `char[]` reached this with `count > 0` and no buffer, and a Rust panic is
    // not a Java throwable — it terminates the process instead of unwinding to
    // the `catch` the caller wrote. `sb_unit_at` reads either layout and yields
    // `None` only when there is no readable payload at all, where the same
    // refusal the bounds arm raises is the honest answer.
    match sb_unit_at_view(&*ctx, &v, index as usize) {
        Some(u) => Ok(Some(Value::Int(i32::from(u)))),
        None => Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into()),
    }
}



/// The builder's length in units (what `sb_read_chars(..).len()` answers) and
/// its units `[from, to)` with `from = max(0, anchor - before)`, `to =
/// min(len, anchor + after)` (empty when `to <= from`), as `(len, from, units)`.
/// `None` -- the caller takes the whole-builder path -- when the switch is off
/// or the payload yielded fewer units than the window asked for.
pub fn sb_code_point_window(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    anchor: i32,
    before: i64,
    after: i64,
) -> Option<(usize, usize, Vec<u16>)> {
    let v = sb_view(ctx, this);
    let len = if v.buf.is_some() { v.count } else { 0 };
    let len_i = len as i64; // Cast: an array-bounded count
    let from = (i64::from(anchor) - before).clamp(0, len_i);
    let to = (i64::from(anchor) + after).clamp(0, len_i);
    if to <= from {
        return Some((len, from as usize, Vec::new())); // Cast: in [0, len]
    }
    let (from, to) = (from as usize, to as usize); // Cast: in [0, len]
    let units = sb_units_range_view(ctx, &v, from, to);
    (units.len() == to - from).then_some((len, from, units))
}

pub fn native_sb_code_point_at(
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
    // JIT r23 wave 2, lane L5 (`CRATONVM_SB_CODE_POINT_WINDOW`): the one or
    // two units the answer needs, not the whole builder (a code-point loop
    // over a builder was quadratic).
    if let Some((len, _, w)) = sb_code_point_window(ctx, this, index_i32, 0, 2) {
        if index_i32 < 0 || (index_i32 as usize) >= len {
            // j27 wave 7 (lane T1LOOPSTATIC, backlog row R16B-139): HotSpot's
            // frames (`String.checkIndex` .. `AbstractStringBuilder.codePointAt`
            // / `StringBuffer.codePointAt`), as `charAt` raises.
            if let Some(raised) = sb_raise_through_builder_body(
                ctx,
                this,
                "codePointAt",
                "(I)",
                Some("I"),
                args,
                SIOOBE,
            ) {
                return Err(raised);
            }
            return Err(
                cratonvm_types::error::RuntimeError::sioobe_index(index_i32, len as i32).into(),
            );
        }
        let ch = w[0];
        if (0xD800..=0xDBFF).contains(&ch)
            && w.len() > 1
            && (0xDC00..=0xDFFF).contains(&w[1])
        {
            let cp = 0x10000 + ((ch as i32 - 0xD800) << 10) + (w[1] as i32 - 0xDC00);
            return Ok(Some(Value::Int(cp)));
        }
        return Ok(Some(Value::Int(ch as i32)));
    }
    let chars = sb_read_chars(ctx, this);
    if index_i32 < 0 || (index_i32 as usize) >= chars.len() {
        // `AbstractStringBuilder.codePointAt` also delegates to
        // `String.checkIndex`, so it carries the same text.
        // j27 wave 7 (lane T1LOOPSTATIC, R16B-139): and its frames.
        if let Some(raised) =
            sb_raise_through_builder_body(ctx, this, "codePointAt", "(I)", Some("I"), args, SIOOBE)
        {
            return Err(raised);
        }
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
pub fn native_sb_code_point_before(
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
    // JIT r23 wave 2, lane L5 (`CRATONVM_SB_CODE_POINT_WINDOW`): units
    // `[index - 2, index)` only; `base` is the window's first index.
    let (len, base, chars) = match sb_code_point_window(ctx, this, index_i32, 2, 0) {
        Some(w) => w,
        None => {
            let all = sb_read_chars(ctx, this);
            (all.len(), 0, all)
        }
    };
    if index_i32 <= 0 || (index_i32 as usize) > len {
        // The JDK body is `checkIndex(index - 1, count)`: the message names
        // `index - 1` (JIT round 17, orchestrator; `R17StringsSb` "raise
        // codePointBefore zero" under `--compatible`).
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(
            index_i32.wrapping_sub(1),
            len as i32,
        )
        .into());
    }
    let index = index_i32 as usize;
    let ch = chars[index - 1 - base];
    if (0xDC00..=0xDFFF).contains(&ch) && index >= 2 {
        let high = chars[index - 2 - base];
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
pub fn native_sb_code_point_count(
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
    // JIT r23 wave 2, lane L5 (`CRATONVM_SB_CODE_POINT_WINDOW`): units
    // `[begin, end)` only; `base` is the window's first index.
    let (len, base, chars) = match sb_code_point_window(
        ctx,
        this,
        begin_i32,
        0,
        i64::from(end_i32) - i64::from(begin_i32),
    ) {
        Some(w) => w,
        None => {
            let all = sb_read_chars(ctx, this);
            (all.len(), 0, all)
        }
    };
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
    let count_i32 = len as i32;
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
        let ch = chars[i - base];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < end {
            let low = chars[i + 1 - base];
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
pub fn native_sb_append_code_point(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_sb_append_codepoint(ctx, args)
}

/// `AbstractStringBuilder.reverse()` (JDK 25
/// `AbstractStringBuilder.java:1696-1733`, delegating to `StringUTF16.reverse`).
///
/// The javadoc is explicit that this is NOT a plain code-unit reversal, which is
/// what this used to be:
///
/// > Causes this character sequence to be replaced by the reverse of the
/// > sequence. **If there are any surrogate pairs included in the sequence,
/// > these are treated as single characters for the reverse operation. Thus,
/// > the order of the high-low surrogates is never reversed.**
///
/// > Note that the reverse operation may result in producing surrogate pairs
/// > that were unpaired low-surrogates and high-surrogates before the operation.
/// > For example, reversing `"\uDC00\uD800"` produces `"𐀀"` which is a valid
/// > surrogate pair.
///
/// The JDK does it in two passes and so does this: reverse every code unit, then
/// walk the result and swap back any `(low, high)` neighbour — that neighbour is
/// exactly a pair the first pass inverted. The second pass runs only when the
/// first saw a surrogate, and it advances TWO units after a swap
/// (`putChar(val, i++, c1)`), so `"\uDC00\uD800"` in the *input* is left as the
/// valid pair the javadoc promises rather than being swapped a second time.
pub fn native_sb_reverse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = sb_view(&*ctx, this);
    let mut units = sb_units_range_view(&*ctx, &v, 0, v.count);
    let had_surrogate = units.iter().any(|&u| (0xD800..=0xDFFF).contains(&u));
    units.reverse();
    if had_surrogate {
        let mut i = 0usize;
        while i + 1 < units.len() {
            // A LOW surrogate followed by a HIGH one is a pair the reversal
            // inverted; put it back.
            if (0xDC00..=0xDFFF).contains(&units[i]) && (0xD800..=0xDBFF).contains(&units[i + 1]) {
                units.swap(i, i + 1);
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    // The write is in place — same length, so `sb_store_units` reuses the
    // payload array — and through the layout-aware writer, because reversing a
    // compact `byte[]` by writing a `char[]` over it is the conversion this
    // migration removed.
    let this = sb_write_units(ctx, this, &units);
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// StringBuilder mutation methods
// ---------------------------------------------------------------------------

/// Helper: read the current content of a builder as a `Vec<u16>`.
///
/// Reads whichever layout the receiver holds. Returning an empty vector for a
/// compact `byte[]` payload — which this did before `sb_value_units` landed —
/// was NOT a refusal: it was a wrong answer with a plausible shape, and it is
/// the visible half of the "every append silently discarded, `toString()`
/// empty, `rc=0`" behaviour `H22` measured on this registrar.
pub fn sb_read_chars(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> Vec<u16> {
    let v = sb_view(ctx, this);
    sb_units_range_view(ctx, &v, 0, v.count)
}

/// Helper: write a slice of UTF-16 units into a builder, replacing all content.
///
/// Returns the post-GC `this`.
pub fn sb_write_chars(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    chars: &[u16],
) -> cratonvm_types::ObjectRef {
    sb_write_units(ctx, this, chars)
}

/// `insert(int, String)` — insert a string at `offset`.
///
/// `checkOffset(offset, count)` first (see [`sb_check_offset`]); the null
/// substitution is second, and unlike `replace` this overload really does
/// substitute "null" rather than throwing.
pub fn native_sb_insert_string(
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
pub fn native_sb_insert_charsequence_range(
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
    // 5. The splice — and the ORDER matters when the source is the receiver.
    //
    // `AbstractStringBuilder.insert(int, CharSequence, int, int)` is
    //
    //     ensureCapacityInternal(count + len);
    //     shift(dstOffset, len);      // value[dstOffset..count) moves right
    //     count += len;
    //     putCharsAt(dstOffset, s, start, end);   // reads s.charAt(...) NOW
    //
    // so every character is read from the array AFTER the shift and after the
    // characters written before it. That is unobservable for any other
    // sequence, and fully observable when `s == this`: MEASURED on
    // jdk-25.0.4+7, `new StringBuilder("abc").insert(1, b)` is `aaaabc`, not
    // the `aabcbc` a snapshot-first implementation produces, and
    // `new StringBuilder("abcdef").insert(2, b, 1, 3)` is `abbbcdef`, not
    // `abbccdef`.
    //
    // It is a sequential dependency, not an artefact of leftover capacity: the
    // reads are all at indices below the pre-insert length, so the region the
    // shift left stale is written before it could ever be read. Four probe
    // rows pin all four shapes.
    let self_source = cs_handle
        .as_ref()
        .is_some_and(|handle| scope.get(handle) == this_now);
    let result = if self_source {
        let len = insert_chars.len();
        let n = chars.len();
        let mut buf: Vec<u16> = Vec::with_capacity(n + len);
        // The array as `shift` leaves it: the head, then whatever the shift did
        // not overwrite, then the tail moved right by `len`.
        buf.extend_from_slice(&chars[..offset]);
        buf.extend_from_slice(&chars[offset..(offset + len).min(n)]);
        buf.resize(offset + len, 0);
        buf.extend_from_slice(&chars[offset..]);
        for i in 0..len {
            buf[offset + i] = buf[start as usize + i];
        }
        buf
    } else {
        let mut result = Vec::with_capacity(chars.len() + insert_chars.len());
        result.extend_from_slice(&chars[..offset]);
        result.extend_from_slice(&insert_chars);
        result.extend_from_slice(&chars[offset..]);
        result
    };
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
pub fn native_sb_insert_charsequence(
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
    // Third door, same array exclusion -- see `is_plain_string`.
    if is_plain_string(&*ctx, cs) {
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
pub fn native_sb_insert_char(
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
pub fn native_sb_insert_int(
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
pub fn native_sb_insert_object(
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
pub fn sb_insert_text(
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
pub fn sb_insert_offset(args: &[Value]) -> i32 {
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
pub fn native_sb_insert_boolean(
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
pub fn native_sb_insert_long(
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
pub fn native_sb_insert_float(
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
pub fn native_sb_insert_double(
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
pub fn native_sb_insert_char_array_off_len(
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
    // Deferred, not read here: `insert(int index, char[] str, int offset,
    // int len)` is `checkOffset(index, count); checkRangeSIOOBE(offset,
    // offset + len, str.length);`, so a bad DESTINATION offset beats a null
    // array. `insert(-1, (char[]) null, 0, 1)` is a
    // StringIndexOutOfBoundsException and `insert(1, (char[]) null, 0, 1)` is a
    // NullPointerException; both were a silent no-op. MEASURED, both orders.
    let arr = match args.get(2) {
        Some(Value::Object(Some(a))) => Some(*a),
        Some(Value::Object(None)) => None,
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
    let Some(arr) = arr else {
        return Err(sb_npe(
            "Cannot read the array length because \"str\" is null",
        ));
    };

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
pub fn native_sb_insert_char_array(
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
    // `checkOffset(offset, count)` is the FIRST line of the real body and
    // `int len = str.length;` the second, so the destination check precedes the
    // dereference: `insert(9, (char[]) null)` on a 3-character builder is a
    // StringIndexOutOfBoundsException, `insert(1, (char[]) null)` a
    // NullPointerException. Reading the array up front — as this did — made
    // both a silent no-op, so the source read now happens after the check.
    // MEASURED, both orders.
    let arr = match args.get(2) {
        Some(Value::Object(Some(a))) => Some(*a),
        Some(Value::Object(None)) => None,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };

    let chars = sb_read_chars(ctx, this);
    if let Some(failure) = sb_check_offset(offset, chars.len() as i32) {
        return Err(failure);
    }
    let offset = offset as usize;
    let Some(arr) = arr else {
        return Err(sb_npe(
            "Cannot read the array length because \"str\" is null",
        ));
    };
    let arr_len = ctx.array_length(arr);
    let mut insert_chars = Vec::with_capacity(arr_len);
    for i in 0..arr_len {
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

/// `delete(int, int)` — remove the range `[start, end)`.
///
/// `AbstractStringBuilder.delete` clamps `end` DOWN to the length and only then
/// runs `checkRangeSIOOBE(start, end, count)`, so an over-long `end` alone is
/// legal (`delete(0, 100)` empties the builder) while a `start` past the length
/// is not: `new StringBuilder("ab").delete(5, 6)` is a
/// `StringIndexOutOfBoundsException`, because after the clamp `start 5 > end 2`.
/// Clamping BOTH ends — what this did — turned that into a silent no-op, the
/// fabricated-success shape `docs/feature-designs/native-dispatch-architecture.md`
/// inventories. A caller that guards a loop with the exception never leaves it.
pub fn native_sb_delete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub fn native_sb_delete_char_at(
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
        // Wave 9 (lane sbtrace): HotSpot's frames (`String.checkIndex` ..
        // `AbstractStringBuilder.deleteCharAt` / the builder's override).
        if let Some(raised) =
            sb_raise_through_builder_body(ctx, this, "deleteCharAt", "(I)", None, args, SIOOBE)
        {
            return Err(raised);
        }
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
pub fn native_sb_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub fn native_sb_set_char_at(
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
    let v = sb_view(&*ctx, this);
    let count = v.count as i32;
    if index < 0 || index >= count {
        // Wave 9 (lane sbtrace): HotSpot's frames, as `deleteCharAt`.
        if let Some(raised) =
            sb_raise_through_builder_body(ctx, this, "setCharAt", "(IC)", Some("V"), args, SIOOBE)
        {
            return Err(raised);
        }
        return Err(cratonvm_types::error::RuntimeError::sioobe_index(index, count).into());
    }
    // In place where the payload can hold the character; otherwise a LATIN1
    // compact array has been asked for a non-LATIN1 one and has to inflate,
    // which is a whole-payload rewrite — exactly `AbstractStringBuilder`'s own
    // `inflate()`.
    if !sb_put_unit(ctx, this, index as usize, ch) {
        let mut units = sb_units_range_view(&*ctx, &v, 0, count as usize);
        units[index as usize] = ch;
        sb_write_units(ctx, this, &units);
    }
    Ok(None)
}

/// `setLength(int)` — truncate, or extend with NUL characters.
///
/// A negative length is the one thing `AbstractStringBuilder.setLength` rejects,
/// and it does so before touching the buffer; clamping it to 0 (what this did)
/// silently emptied the builder instead.
///
/// The message is `StringIndexOutOfBoundsException(int)`'s own wording rather
/// than one of the `Preconditions` shapes, because this check is hand-rolled in
/// the JDK and not routed through a formatter. UNVERIFIED against JDK 25: only
/// the exception CLASS is pinned by the javadoc.
pub fn native_sb_set_length(
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
        // j27 wave 7 (lane T1LOOPSTATIC, R16B-139): HotSpot's frame
        // (`AbstractStringBuilder.setLength` / `StringBuffer.setLength`). The
        // JDK body raises before it writes anything (`newLength < 0` is its
        // first test), so the run mutates nothing.
        if let Some(raised) =
            sb_raise_through_builder_body(ctx, this, "setLength", "(I)", Some("V"), args, SIOOBE)
        {
            return Err(raised);
        }
        return Err(
            cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                index: new_len,
                message: Some(format!("String index out of range: {new_len}")),
            }
            .into(),
        );
    }
    let new_len = new_len as usize;
    let v = sb_view(&*ctx, this);
    let count = v.count;
    if new_len <= count {
        // The JDK only moves `count`; the characters past it are unreachable and
        // are overwritten by whatever is appended next.
        sb_set_count(ctx, this, new_len as i32);
        return Ok(None);
    }
    // Extending pads with U+0000, and does so through the layout-aware writer so
    // the padding lands in the receiver's own representation.
    let mut units = sb_units_range_view(&*ctx, &v, 0, count);
    units.resize(new_len, 0);
    sb_write_units(ctx, this, &units);
    Ok(None)
}

/// Return a lane mask for 32 UTF-16 units equal to `needle`.
///
/// This out-of-line helper keeps the baseline binary free of AVX-512
/// instructions outside runtime-dispatched calls.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
pub unsafe fn avx512_u16_equal_mask(input: *const u16, needle: u16) -> u32 {
    let mask: u32;
    // SAFETY: the caller guarantees a readable 64-byte range and AVX-512F/BW.
    unsafe {
        std::arch::asm!(
            "vmovdqu16 zmm0, [{input}]",
            "vmovd xmm2, {needle:e}",
            "vpbroadcastw zmm1, xmm2",
            "vpcmpeqw k1, zmm0, zmm1",
            "kmovd {mask:e}, k1",
            "vzeroupper",
            input = in(reg) input,
            needle = in(reg) u32::from(needle),
            mask = lateout(reg) mask,
            out("zmm0") _,
            out("zmm1") _,
            out("xmm2") _,
            out("k1") _,
            options(nostack, preserves_flags),
        );
    }
    mask
}

/// indexOf(String) — find substring, return -1 if not found
pub fn native_sb_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // `read_string_chars`, not `read_string`: the needle may itself hold an
    // unpaired surrogate.
    //
    // A null needle is a NullPointerException, not `-1`: `indexOf` reaches
    // `String.indexOf(byte[], byte, int, String, int)`, whose first act is
    // `tgt.length()`. `-1` is the answer for "not present", and a caller
    // cannot tell it from "you passed null". MEASURED.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"String.length()\" because \"str\" is null",
            ))
        }
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(index_of_units_from(&chars, &target, 0))))
}

/// indexOf(String, int) — find substring from offset
pub fn native_sb_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // The null needle beats the `fromIndex`, which is only clamped and never
    // rejected: `indexOf(null, -1)` and `indexOf(null, 99)` are both NPE.
    // MEASURED.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"String.length()\" because \"str\" is null",
            ))
        }
        _ => return Ok(Some(Value::Int(-1))),
    };
    let from = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let chars = sb_read_chars(ctx, this);
    Ok(Some(Value::Int(index_of_units_from(&chars, &target, from))))
}

/// Last occurrence of `needle` in `haystack` at a start index <= `from`
/// (UTF-16 code-unit indices), or -1. Matches String/AbstractStringBuilder
/// `lastIndexOf` semantics, including the empty-needle case.
pub fn u16_last_index_of(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    let n = haystack.len() as i32;
    let m = needle.len() as i32;
    if m == 0 {
        // JDK `String.lastIndexOf(byte[], .., tgtStr, fromIndex)`:
        // `fromIndex = min(fromIndex, srcCount - tgtCount)`, then a NEGATIVE
        // result is `-1` before the empty-needle answer (`fromIndex`) is
        // reached. `"abc".lastIndexOf("", -1)` is `-1`, not `0` (STR20 review).
        return if from < 0 { -1 } else { from.min(n) };
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
pub fn native_sb_last_index_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // `read_string_chars`: an unpaired surrogate in the needle cannot survive
    // `read_string().encode_utf16()`, which routes through a Rust `str`.
    //
    // A null needle throws, exactly as in `indexOf`. MEASURED.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"String.length()\" because \"str\" is null",
            ))
        }
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
pub fn native_sb_last_index_of_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // `read_string_chars`: an unpaired surrogate in the needle cannot survive
    // `read_string().encode_utf16()`, which routes through a Rust `str`.
    //
    // A null needle throws, exactly as in `indexOf`. MEASURED.
    let target: Vec<u16> = match args.get(1) {
        Some(Value::Object(Some(obj))) => read_string_chars(ctx, *obj),
        Some(Value::Object(None)) => {
            return Err(sb_npe(
                "Cannot invoke \"String.length()\" because \"str\" is null",
            ))
        }
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
pub fn native_sb_substring(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        // j27 wave 7 (lane T1LOOPSTATIC, R16B-139): HotSpot's frames
        // (`AbstractStringBuilder.substring(I)` -> `substring(II)` ->
        // `Preconditions.checkFromToIndex`).
        let raised = sb_raise_through_builder_body(
            ctx,
            this,
            "substring",
            "(I)",
            Some("Ljava/lang/String;"),
            args,
            SIOOBE,
        );
        return Err(raised.unwrap_or(failure));
    }
    // Units, not `from_utf16_lossy`: `new StringBuilder("a\uD800b").substring(1, 2)`
    // is a one-char String holding an unpaired surrogate. See
    // [`sb_string_from_units`]. An empty range is the `""` literal, as
    // `StringLatin1.newString` / `StringUTF16.newString` answer it
    // ([`sb_new_string_from_units`]).
    let str_obj = sb_new_string_from_units(ctx, &chars[start as usize..])?;
    Ok(Some(Value::Object(Some(str_obj))))
}

/// `Preconditions.checkFromToIndex(start, end, count, SIOOBE_FORMATTER)` — the
/// range check `AbstractStringBuilder.substring`/`subSequence` open with, and
/// the body of `String.checkRangeSIOOBE`, which `delete`/`replace`/
/// `codePointCount` open with in turn.
///
/// Returns the failure to raise, or `None` when the range is valid.
pub fn sb_check_from_to_index(
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
pub fn sb_check_offset(offset: i32, count: i32) -> Option<cratonvm_types::error::MethodCallFailed> {
    sb_check_from_to_index(offset, count, count)
}

/// substring(int, int) — substring [start, end)
pub fn native_sb_substring_range(
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
        // j27 wave 7 (lane T1LOOPSTATIC, R16B-139): HotSpot's frames.
        let raised = sb_raise_through_builder_body(
            ctx,
            this,
            "substring",
            "(II)",
            Some("Ljava/lang/String;"),
            args,
            SIOOBE,
        );
        return Err(raised.unwrap_or(failure));
    }
    // `start == end` is the `""` literal (`newString`'s `len == 0` arm).
    let str_obj = sb_new_string_from_units(ctx, &chars[start as usize..end as usize])?;
    Ok(Some(Value::Object(Some(str_obj))))
}

/// `capacity()` — `value.length >> coder`, the JDK's own expression.
pub fn native_sb_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(sb_capacity_units(&*ctx, this) as i32)))
}

/// `ensureCapacity(int)` — grow if needed, by the JDK's `newCapacity` rule.
///
/// A request at or below the current capacity is a no-op, INCLUDING a negative
/// one: `AbstractStringBuilder.ensureCapacity` tests `minimumCapacity > 0` first
/// and returns.
pub fn native_sb_ensure_cap(
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
    sb_reserve_units(ctx, this, min_cap);
    Ok(None)
}

/// `trimToSize()` — shrink the payload to exactly `count << coder` bytes.
///
/// This was `native_noop_with_this` on the assumption that capacity is not
/// observable. It IS observable: `native_sb_capacity` answers
/// `value.length >> coder`, so after a no-op trim `capacity()` still reported
/// the pre-trim size — a directly visible divergence from
/// `AbstractStringBuilder.trimToSize`, which reallocates `value` to exactly the
/// live length. The payload is also the only thing holding the surplus memory
/// alive, so the no-op defeated the method's entire purpose.
///
/// The `coder` is preserved, not recomputed: the JDK's `Arrays.copyOf(value,
/// count << coder)` cannot deflate, so a trimmed UTF16 builder stays UTF16 and
/// `capacity()` stays `count`.
pub fn native_sb_trim_to_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let v = sb_view(&*ctx, this);
    let count = v.count;
    if count == v.capacity {
        // Already exact — `trimToSize` is defined to be a no-op in that case,
        // and skipping the copy keeps the common path allocation-free.
        return Ok(None);
    }
    let units = sb_units_range_view(&*ctx, &v, 0, count);
    sb_store_units(ctx, this, &units, Some(count));
    Ok(None)
}

/// Format a float like Java does.
pub fn format_float(v: f32) -> String {
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
pub fn read_string_chars(
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

pub fn native_string_to_char_array(
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
    // gc-common w9-d (`common-w8b-native-factories-still-single-attempt`):
    // caller-sized, so a refusal reclaims and retries before the catchable
    // `OutOfMemoryError`. `live` is empty because nothing read before this
    // line is used after it -- the characters are in a Rust `Vec` -- and this
    // native is registered-only (`String.toCharArray()`).
    let arr = crate::new_array_reclaiming(
        ctx,
        ArrayElementType::Char,
        chars.len(),
        &mut [],
        "char[] (String.toCharArray)",
    )?;
    for (i, &ch) in chars.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(ch as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_string_contains(
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

pub fn native_string_starts_with(
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
pub const STARTS_WITH_NPE: &str = "Cannot invoke \"String.length()\" because \"prefix\" is null";

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
pub fn native_string_starts_with_offset(
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

pub fn native_string_ends_with(
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
pub fn native_string_trim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    // nio21 w9 L5: lossless for a lone surrogate (`(" " + LO).trim()`).
    let result = string_native_result_from_units(ctx, &units[start..end], false)?;
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_replace(
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
    if old_char == new_char {
        return Ok(Some(Value::Object(Some(this))));
    }
    let out = with_string_chars_scratch(ctx, this, |chars| {
        let first = chars.iter().position(|&ch| ch == old_char)?;
        let mut out: Vec<u16> = Vec::with_capacity(chars.len());
        out.extend_from_slice(&chars[..first]);
        out.push(new_char);
        for &ch in &chars[first + 1..] {
            out.push(if ch == old_char { new_char } else { ch });
        }
        Some(out)
    });
    let Some(out) = out else {
        return Ok(Some(Value::Object(Some(this))));
    };
    let result = string_native_result_from_units(ctx, &out, false)?;
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
pub fn native_string_replace_charseq(
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
        // HotSpot's text is the JDK body's JEP 358 message (`target.toString()`;
        // the JDK classes carry a LocalVariableTable). Interpreter round i4 wave
        // 98, lane L9 (`L9W98JdkApiTexts` row `replace-target`).
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"java.lang.CharSequence.toString()\" because \"target\" is null",
            ))
        }
    };
    let replacement_units = match args.get(2) {
        Some(Value::Object(Some(o))) => {
            invoke_to_string_units_opt(&mut *scope, *o)?.unwrap_or_default()
        }
        _ => {
            return Err(string_arg_npe(
                "Cannot invoke \"java.lang.CharSequence.toString()\" because \"replacement\" is null",
            ))
        }
    };
    let this = scope.get(&this_handle);
    if target_units.len() == 1
        && replacement_units.len() == 1
        && target_units[0] == replacement_units[0]
    {
        return Ok(Some(Value::Object(Some(this))));
    }
    let s_units = read_string_chars(&*scope, this);
    if !target_units.is_empty() && find_units(&s_units, &target_units).is_none() {
        return Ok(Some(Value::Object(Some(this))));
    }
    let out = replace_units(&s_units, &target_units, &replacement_units);
    let result = sb_string_from_units(&mut *scope, &out)?;
    Ok(Some(Value::Object(Some(result))))
}

#[inline]
fn find_units(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    if rest.is_empty() {
        return haystack.iter().position(|&u| u == first);
    }
    let limit = haystack.len().checked_sub(needle.len())?;
    let mut i = 0usize;
    while i <= limit {
        let rel = haystack[i..=limit].iter().position(|&u| u == first)?;
        let pos = i + rel;
        if &haystack[pos + 1..pos + needle.len()] == rest {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

/// Literal find-and-replace over UTF-16 code units — `str::replace`'s contract,
/// in the space Java strings actually live in.
///
/// The empty-target case is the one worth stating: both Java and Rust insert
/// the replacement before every unit and once at the end, so `"abc"` with
/// `("", "-")` is `-a-b-c-`. It is handled explicitly because the obvious loop
/// silently produces `a-b-c` instead, and no test in this tree would have
/// caught the difference.
pub fn replace_units(haystack: &[u16], target: &[u16], replacement: &[u16]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(haystack.len());
    if target.is_empty() {
        out.extend_from_slice(replacement);
        for &u in haystack {
            out.push(u);
            out.extend_from_slice(replacement);
        }
        return out;
    }
    if target.len() == 1 && replacement.len() == 1 {
        let (t, r) = (target[0], replacement[0]);
        for &u in haystack {
            out.push(if u == t { r } else { u });
        }
        return out;
    }
    let mut i = 0usize;
    while let Some(rel) = find_units(&haystack[i..], target) {
        let pos = i + rel;
        out.extend_from_slice(&haystack[i..pos]);
        out.extend_from_slice(replacement);
        i = pos + target.len();
    }
    out.extend_from_slice(&haystack[i..]);
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
pub fn string_case_impl(
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
    //
    // gc-common w11-d: the no-Locale default can change (`setDefault`), and
    // the memo key does not carry it. A locale-dependent default never
    // memoises (the branch below returns first), so skipping the lookup then
    // is exact. One TLS read when the default is cached.
    let default_is_locale_dependent = locale.is_none()
        && crate::case_map::is_locale_dependent(
            crate::locale_bootstrap::default_case_mapping_language(ctx),
        );
    if memoize && !default_is_locale_dependent {
        if let Some(result) = ctx.get_ascii_case_string_cached(this, locale, !lowercase) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    let lang = crate::locale_language_for_case_mapping(ctx, locale);
    // One read serves both paths below. Nothing above allocates or runs
    // bytecode, so `this` is still the address the caller passed.
    let src = ctx.read_string(this).unwrap_or_default();
    // nio21 w9 L5: an UNPAIRED surrogate cannot survive `read_string` (it
    // decodes to U+FFFD), and the mapped text then differs from `src`, so
    // `"\uD801a".toUpperCase()` answered `"�A"` where HotSpot 25 answers
    // `"\uD801A"`. A U+FFFD in the decode is the only way one can be there;
    // only then are the units read, and each well-formed run is mapped by the
    // same rule as below while each lone surrogate is carried through.
    if src.contains('\u{FFFD}')
    {
        let units = read_string_chars(ctx, this);
        if has_unpaired_surrogate(&units) {
            let locale_dependent = crate::case_map::is_locale_dependent(&lang);
            let mapped = map_units_keeping_lone_surrogates(&units, |text| {
                match (locale_dependent, lowercase) {
                    (true, true) => crate::case_map::to_lower_case(text, &lang),
                    (true, false) => crate::case_map::to_upper_case(text, &lang),
                    (false, true) => crate::case_map::jdk_to_lowercase(text),
                    (false, false) => crate::case_map::jdk_to_uppercase(text),
                }
            });
            if mapped == units {
                return Ok(Some(Value::Object(Some(this))));
            }
            let obj = sb_string_from_units(ctx, &mapped)?;
            return Ok(Some(Value::Object(Some(obj))));
        }
    }
    if crate::case_map::is_locale_dependent(&lang) {
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

    let mut folded = src;
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
pub fn locale_arg(args: &[Value], index: usize) -> Option<cratonvm_types::ObjectRef> {
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
pub fn locale_arg_checked(
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

pub fn string_case_native(
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
pub fn native_string_to_lower_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, true, true)
}

/// `String.toUpperCase()` / `toUpperCase(Locale)` **with** the per-receiver memo.
pub fn native_string_to_upper_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, false, true)
}

/// `String.toLowerCase()` / `toLowerCase(Locale)` **without** the memo — the
/// real-JDK essential set and the `phases_early` overloads, which each allocated
/// a fresh String per call before they shared this implementation. Keeping them
/// uncached means unifying the four sites changes only the locale handling.
pub fn native_string_to_lower_case_uncached(
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
pub fn native_string_to_upper_case_uncached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    string_case_native(ctx, args, false, false)
}

pub fn native_string_is_empty(
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

pub fn native_string_value_of_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let result = ctx.create_string_uninterned_gc_safe(&val.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    // j27 wave 7 (lane T1LOOPSTATIC, backlog row SBEMPTY17-2): JDK 25's body
    // is `return b ? "true" : "false";`, two literals, so
    // `String.valueOf(true) == "true"` on HotSpot; the pooled literal under
    // `CRATONVM_STRING_LITERAL_IDENTITY`, as `valueOf(Object)`'s `Boolean`
    // arm already answers. `=0`: the fresh copy, as before.
    let text = if val { "true" } else { "false" };
    let result = if string_literal_identity_enabled(&*ctx) {
        ctx.intern_string(text)
    } else {
        ctx.create_string_uninterned_gc_safe(text)
    };
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let text = format_double(val);
    // `Double.toString` answers the five special values with pooled literals
    // (round 13 wave 9; see `lang_math::fp_special_literal`).
    if let Some(s) = crate::lang_math::fp_special_literal(ctx, &text) {
        return Ok(Some(Value::Object(Some(s))));
    }
    let result = ctx.create_string_uninterned_gc_safe(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\0'),
        _ => '\0',
    };
    let result = ctx.create_string_uninterned_gc_safe(&ch.to_string());
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let text = format_float(val);
    // `Float.toString`'s special values are pooled literals (see
    // `lang_math::fp_special_literal`).
    if let Some(s) = crate::lang_math::fp_special_literal(ctx, &text) {
        return Ok(Some(Value::Object(Some(s))));
    }
    let result = ctx.create_string_uninterned_gc_safe(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_value_of_object(
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
            //
            // `is_plain_string`, not a bare class-name test: this arm returns
            // the RECEIVER as the answer, so a `String[]` admitted here is
            // handed to the caller AS a String and every later `.value` read of
            // it decodes the array's first element pointer. That is the loudest
            // of the four doors, because the bad object escapes the native.
            if is_plain_string(&*ctx, *obj) {
                return Ok(Some(Value::Object(Some(*obj))));
            }
            // `Boolean.toString()` is `value ? "true" : "false"`: literals, so
            // `String.valueOf((Object) Boolean.TRUE) == "true"` on HotSpot. The
            // wrapper fast path below renders text and would copy it (round 17
            // wave 4, lane sbempty, `CRATONVM_STRING_LITERAL_IDENTITY`).
            if string_literal_identity_enabled(&*ctx) {
                if let Some(text) = boolean_to_string_literal_text(&*ctx, *obj) {
                    let lit = ctx.intern_string(text);
                    return Ok(Some(Value::Object(Some(lit))));
                }
            }
            // Wave 9 (lane sbtrace): an argument whose `toString()` must run
            // runs it from the JDK body, so the `String.valueOf` frame is on
            // the upcall's stack (`CRATONVM_TOSTRING_UPCALL_FRAMES`).
            let units = match to_string_fast_units(ctx, *obj) {
                Some(units) => Some(units),
                None => {
                    if let Some(result) = string_value_of_through_bytecode(ctx, *obj) {
                        return result;
                    }
                    java_to_string_units_opt(ctx, *obj)?
                }
            };
            match units {
                Some(units) => {
                    let result = sb_string_from_units(ctx, &units)?;
                    Ok(Some(Value::Object(Some(result))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        }
        _ => {
            // JDK: `(obj == null) ? "null" : ...` -- the literal, so
            // `String.valueOf((Object) null) == "null"` on HotSpot.
            let result = if string_literal_identity_enabled(&*ctx) {
                ctx.intern_string("null")
            } else {
                ctx.create_string_uninterned("null")
            };
            Ok(Some(Value::Object(Some(result))))
        }
    }
}

/// `"true"` / `"false"` when `obj` is a `java.lang.Boolean` (its one field the
/// `value`), else `None`. The same shape test as the wrapper arm of
/// [`to_string_fast_units`]: never an array, exactly one field, the class
/// named `java/lang/Boolean` (which is final, so no override of `toString`).
pub fn boolean_to_string_literal_text(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<&'static str> {
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array || ctx.object_num_fields(obj) != 1
    {
        return None;
    }
    let class_id = ctx.class_id_of_object(obj);
    // NIO2-17-5 rung 2 (`CRATONVM_CLASS_FACTS_MEMO_WRAPPER`), as in
    // `to_string_fast_units`.
    match crate::class_facts_memo::rung_facts(ctx, crate::class_facts_memo::Rung::Wrapper, class_id)
    {
        Some(facts) => {
            if facts.and_then(crate::class_facts_memo::NameFacts::box_kind)
                != Some(crate::class_facts_memo::BoxKind::Boolean)
            {
                return None;
            }
        }
        None => {
            let name = ctx.class_name_of_id(class_id)?;
            if name != "java/lang/Boolean" {
                return None;
            }
        }
    }
    match ctx.get_field(obj, 0) {
        Value::Int(v) => Some(if v != 0 { "true" } else { "false" }),
        _ => None,
    }
}

pub fn native_string_compare_to(
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
                // HotSpot: `byte[] v2 = anotherString.value;` is the first
                // dereference of the argument (the same helpful-NPE shape as
                // `compareToIgnoreCase`'s "s2" below). STR20 review.
                message: Some(
                    "Cannot read field \"value\" because \"anotherString\" is null".to_string(),
                ),
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

pub fn native_string_index_of_str(
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

pub fn native_string_substring_one(
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
    // nio21 w9 L5: lossless when `begin` splits a surrogate pair.
    let result = string_native_result_from_units(ctx, &sub_utf16, false)?;
    Ok(Some(Value::Object(Some(result))))
}

/// UTF-8 bytes of UTF-16 `units` the way `String.getBytes(UTF_8)` makes them:
/// a pair is one 4-byte sequence, an UNPAIRED surrogate is the encoder's
/// replacement `?` (0x3F). nio21 wave 9 L5.
pub fn utf8_bytes_jdk_replacing(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Err(_) => out.push(b'?'),
        }
    }
    out
}

pub fn native_string_get_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // nio21 w9 L5: from the code units, with the JDK UTF-8 encoder's
    // replacement for an unpaired surrogate (`?`, one byte; HotSpot 25:
    // `"\uD801".getBytes()` is `[63]`). Through `read_string` the surrogate
    // was U+FFFD and encoded as the three bytes EF BF BD.
    let units = read_string_chars(ctx, this);
    let encoded = utf8_bytes_jdk_replacing(&units);
    let bytes = encoded.as_slice();
    // gc-common w9-d: as `native_string_to_char_array` -- the bytes are in a
    // Rust `Vec`, nothing else is held, registered-only (`getBytes()`).
    let arr = crate::new_array_reclaiming(
        ctx,
        ArrayElementType::Byte,
        bytes.len(),
        &mut [],
        "byte[] (String.getBytes)",
    )?;
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_string_concat(
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
                // HotSpot 25's helpful NPE for `str.isEmpty()` in `concat`.
                message: Some(
                    "Cannot invoke \"String.isEmpty()\" because \"str\" is null".to_string(),
                ),
            }
            .into());
        }
    };
    // nio21 w9 L5: code units, not two lossy `String`s: `"\uD801".concat("\uDC00")`
    // is the PAIR U+10400 on HotSpot 25 (and equals "𐐀"); through `read_string`
    // each half decoded to U+FFFD and the answer was "��".
    let tail = read_string_chars(ctx, other);
    if tail.is_empty() {
        // `if (str.isEmpty()) return this;` -- the receiver itself.
        return Ok(Some(Value::Object(Some(this))));
    }
    let mut units = read_string_chars(ctx, this);
    units.extend_from_slice(&tail);
    let result = string_native_result_from_units(ctx, &units, false)?;
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Phase 8 Part 7: Additional String methods
// ---------------------------------------------------------------------------

pub fn native_string_split(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // nio21 w10 L4: exact or declined (see `string_regex_exact`).
    if let Some(r) = string_regex_front(
        ctx,
        args,
        StringRegexOp::Split(0),
        "split",
        "(Ljava/lang/String;)[Ljava/lang/String;",
    ) {
        return r;
    }
    native_string_split_impl(ctx, args, 0)
}

pub fn native_string_split_limit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    if let Some(r) = string_regex_front(
        ctx,
        args,
        StringRegexOp::Split(limit),
        "split",
        "(Ljava/lang/String;I)[Ljava/lang/String;",
    ) {
        return r;
    }
    native_string_split_impl(ctx, args, limit)
}

pub fn native_string_split_private(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let with_delimiters = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    // nio21 w10 L4: `splitWithDelimiters` has no exact path; it declines.
    let op = if with_delimiters {
        None
    } else {
        Some(StringRegexOp::Split(limit))
    };
    if string_regex_exact_enabled() {
        if let Some(op) = op {
            if let Some(r) = string_regex_exact(ctx, args, op) {
                return r;
            }
        }
        if let Some(r) = string_regex_decline(
            ctx,
            "split",
            "(Ljava/lang/String;IZ)[Ljava/lang/String;",
            args,
        ) {
            return r;
        }
    }
    if !with_delimiters {
        return native_string_split_impl(ctx, args, limit);
    }
    native_string_split_with_delimiters(ctx, args, limit)
}

pub fn string_array_from_parts(ctx: &mut dyn NativeContext, parts: &[String]) -> MethodCallResult {
    let string_class_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(id) => id,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3). `java.lang.String`
        // is in every image, so this arm is unreachable on a working one; a run
        // that reaches it has no `java.base`, and fabricating a `String`
        // stand-in there is a second failure wearing the first one's name.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    // gc-common w15-e: every element is a fresh String allocated AFTER the
    // array, so the array is rooted and re-read for each store (a collecting
    // allocation would otherwise leave the stores in the vacated copy and the
    // live array full of nulls).
    let mut scope = NativeHandleScope::new(ctx);
    let arr_obj = scope.new_ref_array(string_class_id, parts.len());
    let arr_h = scope.root(arr_obj);
    for (i, part) in parts.iter().enumerate() {
        let str_ref = scope.create_string_uninterned(part);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(str_ref)));
    }
    let arr = scope.get(&arr_h);
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
pub const SPLIT_REGEX_NPE: &str = "Cannot invoke \"String.length()\" because \"regex\" is null";

pub fn native_string_split_with_delimiters(
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

pub fn is_java_ascii_regex_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | b'\x0c' | b'\r')
}

pub fn split_on_whitespace_comma(s: &str, limit: i32) -> Vec<String> {
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

pub fn native_string_split_impl(
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
pub fn native_string_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args[0] = delimiter, args[1] = CharSequence[]
    // nio21 w9 L5: code units throughout (delimiter, String elements, result),
    // so an unpaired surrogate -- or two halves joined into a pair -- survives
    // as on HotSpot 25. Non-String elements still go through `toString()`.
    // nio21 w10 L4: `delimiter.toString()` -- a `StringBuilder` delimiter is
    // its text, not its value array (and was `""` before wave 9).
    let delim: Vec<u16> = match args.first() {
        Some(Value::Object(Some(obj))) => invoke_to_string_units_opt(ctx, *obj)?
            .unwrap_or_else(|| "null".encode_utf16().collect()),
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
    let mut joined: Vec<u16> = Vec::new();
    for i in 0..len {
        if i > 0 {
            joined.extend_from_slice(&delim);
        }
        let arr = scope.get(&arr_handle);
        if let Value::Object(Some(elem)) = scope.get_array_element(arr, i) {
            let mut element_scope = NativeHandleScope::new(&mut *scope);
            let elem_handle = element_scope.root(elem);
            let elem = element_scope.get(&elem_handle);
            // Preserve the String fast path, but use real polymorphic
            // dispatch for StringBuilder, custom CharSequences, and
            // application classes such as Spring Boot's Regex.
            match element_scope.read_string_units(elem) {
                Some(units) => joined.extend_from_slice(&units),
                None => joined.extend(invoke_to_string(&mut *element_scope, elem)?.encode_utf16()),
            }
        } else {
            joined.extend("null".encode_utf16());
        }
    }
    let result = string_native_result_from_units(&mut *scope, &joined, false)?;
    Ok(Some(Value::Object(Some(result))))
}

/// HotSpot's helpful-NPE text for a null `delimiter`, shared by both `join`
/// overloads.
pub const JOIN_DELIMITER_NPE: &str =
    "Cannot invoke \"java.lang.CharSequence.toString()\" because \"delimiter\" is null";

/// `String.join(CharSequence delimiter, Iterable<? extends CharSequence>)`.
///
/// Same three-nulls table as [`native_string_join`], with one difference the
/// message records: this overload opens with two explicit
/// `Objects.requireNonNull` calls rather than implicit dereferences, so HotSpot
/// raises a NullPointerException with a NULL message for both. Measured:
/// `String.join(",", (Iterable) null)` and `String.join(null, List.of())` both
/// throw with no message. The delimiter is still checked first.
pub fn native_string_join_iterable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // nio21 w10 L4: the JDK 25 body, losslessly --
    //
    //   Objects.requireNonNull(delimiter); Objects.requireNonNull(elements);
    //   var delim = delimiter.toString();
    //   for (CharSequence cs : elements) elems[size++] = String.valueOf(cs);
    //   return join("", "", delim, elems, size);
    //
    // Code units throughout (an unpaired surrogate survives, two halves can
    // join into a pair), `delimiter.toString()` rather than `read_string` (a
    // `StringBuilder` delimiter used to join with `""`), no element cap (the
    // old loop stopped silently after 1 000 000 elements and returned a
    // truncated join), and a null `iterator()` is the for-each's
    // `NullPointerException`, not `""`.
    let delim_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let iterable = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let mut scope = NativeHandleScope::new(ctx);
    let iterable_handle = scope.root(iterable);
    let delim: Vec<u16> = invoke_to_string_units_opt(&mut *scope, delim_obj)?
        .unwrap_or_else(|| "null".encode_utf16().collect());
    let iterable = scope.get(&iterable_handle);
    let iterator =
        match scope.invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])? {
            Some(Value::Object(Some(obj))) => obj,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into())
            }
        };
    let iterator_handle = scope.root(iterator);
    let mut joined: Vec<u16> = Vec::new();
    let mut first = true;
    loop {
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
        if !first {
            joined.extend_from_slice(&delim);
        }
        first = false;
        match elem {
            Some(Value::Object(Some(obj))) => match invoke_to_string_units_opt(&mut *scope, obj)? {
                Some(units) => joined.extend_from_slice(&units),
                None => joined.extend("null".encode_utf16()),
            },
            _ => joined.extend("null".encode_utf16()),
        }
    }
    let result = string_native_result_from_units(&mut *scope, &joined, false)?;
    Ok(Some(Value::Object(Some(result))))
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
pub fn regex_arg_npe(parameter: &str) -> cratonvm_types::error::RuntimeError {
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
pub fn string_arg_npe(message: &str) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::NullPointerException {
        message: Some(message.to_string()),
    }
    .into()
}

// ---------------------------------------------------------------------------
// nio21 wave 10 L4: the regex-backed String natives, HotSpot-exact or declined
// ---------------------------------------------------------------------------
//
// `matches`, `replaceAll`, `replaceFirst` and `split` used to translate the
// Java pattern for the Rust `regex` / `fancy-regex` engines and run them over
// `read_string` text. That is not `java.util.regex`: Rust's `.` admits `\r`
// and U+0085/U+2028/U+2029, `$` does not match before a final line
// terminator, `\d \w \s \b` are Unicode, `fancy-regex` lookaround and
// possessive/atomic forms are a different backtracker, byte offsets are not
// UTF-16 indices, an unpaired surrogate is U+FFFD, the replacement
// iteration's empty-match rule is the crate's, `matches` fell back to an
// UNANCHORED search for any pattern the anchored std engine refused, and
// `split` dropped the JDK's "no leading empty string for a zero-width match
// at 0" rule and the `[this]` identity of a no-match split.
//
// Now each of them first tries `string_regex_exact`, which answers only
// inside a subset where the answer is provably the JDK's:
//
// * the subject is ASCII (so byte offsets are UTF-16 indices, and no
//   Unicode-vs-ASCII class difference or non-ASCII line terminator can
//   matter), and the pattern is printable ASCII;
// * the pattern is in the grammar `java_regex_exact_subset` accepts, and is
//   RE-EMITTED (never passed through) with every construct spelled in its
//   Java meaning: literals as `\x{..}`, `\d \w \s` as their ASCII classes,
//   `.` as `[^\n\r\x{85}\x{2028}\x{2029}]`, `^` as `\A`, `$` as `\z` (and
//   only when the subject holds no `\n` / `\r`); no backreferences, no
//   lookaround, no possessive quantifiers or atomic groups, no inline flags,
//   no named groups, no `\p{..}`, no nested classes or `&&`;
// * the std `regex` engine's leftmost-first semantics are the backtracking
//   ones for that subset, and the iteration (`Matcher.find`'s
//   `if (last == first) next++`), the replacement parser
//   (`Matcher.appendExpandedReplacement`) and `Pattern.split` are
//   transcribed from the JDK 25 sources;
// * a replacement that would throw, a `$` group ref with a quantified group
//   in the pattern, and every null argument are declined too.
//
// Everything else DECLINES to the JDK's own body
// (`invoke_special_bytecode_only`): real class bytes are authoritative. If
// no body can run (a context without bytecode), the pre-wave-10 Rust path
// answers as before. `CRATONVM_STRING_REGEX_EXACT=0` restores the old natives.
//
// Perf: the exact path costs what the old one did (one cached compile, one
// scan). A declined call pays the JDK regex path (`Pattern.compile` and a
// `Matcher` per call, interpreted until compiled): non-ASCII subjects,
// lookaround / backreference / `\p{..}` patterns, `${name}` replacements.

/// The `java.util.regex` subset [`string_regex_exact`] answers, re-emitted in
/// the Rust `regex` crate's syntax with Java's meaning for every construct.
///
/// Shared with the real-layout `Matcher.find` fast path
/// (`regex_matcher::matcher_realjdk_cached`, nio21 wave 11 L2), which runs it
/// over any subject [`java_regex_exact_subject_ok`] admits (not only ASCII:
/// its offsets go through UTF-16 tables).
pub struct ExactRegex {
    /// The re-emitted pattern (std `regex` syntax).
    pub rust: String,
    /// The pattern has a `$` (emitted as `\z`; exact only on a subject
    /// without a line terminator).
    pub has_dollar: bool,
    /// A quantifier applies to a group: overall spans are exact, but group
    /// captures under an empty-iteration loop are where backtracking engines
    /// differ, so a replacement with a group ref declines.
    pub quantified_group: bool,
}

/// Whether `units` is a subject on which an [`ExactRegex`]'s answers are the
/// JDK's (nio21 wave 11 L2): no UNPAIRED surrogate (the Rust engine sees
/// U+FFFD there), and, for a pattern with `$`, no line terminator (`\n`,
/// `\r`, U+0085, U+2028, U+2029: Java's `$` also matches before a final one).
/// Valid surrogate pairs are fine: every construct of the subset consumes a
/// supplementary code point the way `java.util.regex` does (one char), and
/// an attempt Java starts at the LOW half of a pair answers what the attempt
/// at its HIGH half did (no ASCII class contains a surrogate, a negated one
/// contains both, and `(?-u:\b)` is false inside a pair). A search that would
/// START inside a pair is the caller's to decline.
pub fn java_regex_exact_subject_ok(x: &ExactRegex, units: &[u16]) -> bool {
    if has_unpaired_surrogate(units) {
        return false;
    }
    !(x.has_dollar
        && units
            .iter()
            .any(|&u| matches!(u, 0x0A | 0x0D | 0x85 | 0x2028 | 0x2029)))
}

#[inline]
pub fn java_regex_exact_subject_ascii_ok(x: &ExactRegex, ascii: &str) -> bool {
    debug_assert!(ascii.is_ascii());
    !(x.has_dollar && ascii.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r'))
}

/// Push one literal byte for a Rust pattern: alphanumerics raw, everything
/// else as `\x{HH}` (so no Java literal can become Rust syntax, e.g. `\<`).
pub fn exact_regex_push_lit(out: &mut String, c: u8) {
    if c.is_ascii_alphanumeric() {
        out.push(c as char);
    } else {
        out.push_str(&format!("\\x{{{c:02X}}}"));
    }
}

/// One single-character class item at `b[i]`: a literal, or an escaped
/// punctuation / `\t \n \r \f`. Returns `(char, bytes consumed)`.
pub fn exact_regex_class_single(b: &[u8], i: usize) -> Option<(u8, usize)> {
    let c = *b.get(i)?;
    match c {
        b']' | b'[' => None,
        b'\\' => {
            let e = *b.get(i + 1)?;
            let v = match e {
                b't' => b'\t',
                b'n' => b'\n',
                b'r' => b'\r',
                b'f' => 0x0C,
                e if !e.is_ascii_alphanumeric() => e,
                _ => return None,
            };
            Some((v, 2))
        }
        _ => Some((c, 1)),
    }
}

/// Accept a Java pattern in the exact subset and re-emit it; `None` = not in
/// the subset (decline). Every rejection below is either a construct whose
/// Java meaning the std engine does not have, or a Java syntax error (which
/// the JDK body must raise with its own message).
///
/// `\B` declines (nio21 w11 L2): Java's `\B` is TRUE inside a surrogate pair
/// (both sides non-word), a position the Rust engine never tries, so
/// `"a\uD801\uDC00"` finds `\B` at 2 on HotSpot; and the regex crate refuses
/// the ASCII `(?-u:\B)` in a UTF-8 regex anyway.
pub fn java_regex_exact_subset(p: &[u16]) -> Option<ExactRegex> {
    if p.iter().any(|&u| !(0x20..=0x7E).contains(&u)) {
        return None;
    }
    // Cast: every unit is printable ASCII (checked above).
    let b: Vec<u8> = p.iter().map(|&u| u as u8).collect();
    let n = b.len();
    let mut out = String::with_capacity(n * 4 + 8);
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut quantifiable = false;
    let mut after_group = false;
    let mut has_dollar = false;
    let mut quantified_group = false;
    while i < n {
        let c = b[i];
        match c {
            b'\\' => {
                let e = *b.get(i + 1)?;
                i += 2;
                match e {
                    b't' => out.push_str("\\t"),
                    b'n' => out.push_str("\\n"),
                    b'r' => out.push_str("\\r"),
                    b'f' => out.push_str("\\x0C"),
                    b'd' => out.push_str("[0-9]"),
                    b'D' => out.push_str("[^0-9]"),
                    b'w' => out.push_str("[0-9A-Za-z_]"),
                    b'W' => out.push_str("[^0-9A-Za-z_]"),
                    b's' => out.push_str("[\\t\\n\\x0B\\x0C\\r ]"),
                    b'S' => out.push_str("[^\\t\\n\\x0B\\x0C\\r ]"),
                    b'b' => {
                        // Zero-width. JDK 19+ `\b` is the ASCII `\w`
                        // boundary (measured on 25.0.3+9: `\b` in "\u00e9a b"
                        // is at 1,2,3,4), so the ASCII assertion, on any
                        // subject. nio21 w11 L2: was the Unicode `\b`, exact
                        // only because the String natives took ASCII text.
                        out.push_str("(?-u:\\b)");
                        quantifiable = false;
                        after_group = false;
                        continue;
                    }
                    e if !e.is_ascii_alphanumeric() => exact_regex_push_lit(&mut out, e),
                    // Backreferences, octal, \x \u \p \Q \G \Z \z \A \R \h \X \k ...
                    _ => return None,
                }
                quantifiable = true;
                after_group = false;
            }
            b'[' => {
                i += 1;
                out.push('[');
                if b.get(i) == Some(&b'^') {
                    out.push('^');
                    i += 1;
                }
                if b.get(i) == Some(&b']') {
                    return None;
                }
                let mut prev_single: Option<u8> = None;
                let mut any = false;
                loop {
                    let c = *b.get(i)?;
                    if c == b']' {
                        i += 1;
                        break;
                    }
                    if c == b'[' || (c == b'&' && b.get(i + 1) == Some(&b'&')) {
                        return None;
                    }
                    if c == b'-' && any && b.get(i + 1) != Some(&b']') {
                        let lo = prev_single.take()?;
                        let (hi, adv) = exact_regex_class_single(&b, i + 1)?;
                        if hi < lo {
                            return None;
                        }
                        out.push('-');
                        exact_regex_push_lit(&mut out, hi);
                        i += 1 + adv;
                        continue;
                    }
                    if c == b'\\' {
                        match b.get(i + 1) {
                            Some(b'd') => {
                                out.push_str("0-9");
                                i += 2;
                                prev_single = None;
                                any = true;
                                continue;
                            }
                            Some(b'w') => {
                                out.push_str("0-9A-Za-z_");
                                i += 2;
                                prev_single = None;
                                any = true;
                                continue;
                            }
                            Some(b's') => {
                                out.push_str("\\t\\n\\x0B\\x0C\\r ");
                                i += 2;
                                prev_single = None;
                                any = true;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    let (v, adv) = exact_regex_class_single(&b, i)?;
                    exact_regex_push_lit(&mut out, v);
                    prev_single = Some(v);
                    any = true;
                    i += adv;
                }
                out.push(']');
                quantifiable = true;
                after_group = false;
            }
            b'(' => {
                if b.get(i + 1) == Some(&b'?') {
                    if b.get(i + 2) != Some(&b':') {
                        return None;
                    }
                    out.push_str("(?:");
                    i += 3;
                } else {
                    out.push('(');
                    i += 1;
                }
                depth += 1;
                quantifiable = false;
                after_group = false;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                out.push(')');
                i += 1;
                quantifiable = true;
                after_group = true;
            }
            b'*' | b'+' | b'?' | b'{' => {
                if !quantifiable {
                    return None;
                }
                if c == b'{' {
                    let close = b[i..].iter().position(|&x| x == b'}')? + i;
                    let body = std::str::from_utf8(&b[i + 1..close]).ok()?;
                    let (lo, hi) = match body.split_once(',') {
                        Some((lo, hi)) => (lo, Some(hi)),
                        None => (body, None),
                    };
                    let digits = |s: &str| -> Option<u32> {
                        if s.is_empty() || s.len() > 4 || !s.bytes().all(|d| d.is_ascii_digit()) {
                            None
                        } else {
                            s.parse().ok()
                        }
                    };
                    let lo_v = digits(lo)?;
                    match hi {
                        None => out.push_str(&format!("{{{lo_v}}}")),
                        Some("") => out.push_str(&format!("{{{lo_v},}}")),
                        Some(h) => {
                            let hi_v = digits(h)?;
                            if hi_v < lo_v || hi_v > 1000 {
                                return None;
                            }
                            out.push_str(&format!("{{{lo_v},{hi_v}}}"));
                        }
                    }
                    if lo_v > 1000 {
                        return None;
                    }
                    i = close + 1;
                } else {
                    out.push(c as char);
                    i += 1;
                }
                match b.get(i) {
                    Some(b'?') => {
                        out.push('?');
                        i += 1;
                    }
                    Some(b'+') => return None, // possessive
                    _ => {}
                }
                // nio21 w13 L4: only a LOOP over a group (`*`, `+`, `{..}`)
                // is the empty-iteration case; `(x)?` runs its group at most
                // once, greedy first in both engines, so its captures are
                // exact (`"x1y22".replaceAll("(\\d)(\\d)?", "<$2$1>")` is
                // `x<1>y<22>` on HotSpot; it used to decline).
                if after_group && c != b'?' {
                    quantified_group = true;
                }
                quantifiable = false;
                after_group = false;
            }
            b'}' | b']' => return None,
            b'.' => {
                out.push_str("[^\\n\\r\\x{85}\\x{2028}\\x{2029}]");
                i += 1;
                quantifiable = true;
                after_group = false;
            }
            b'^' => {
                out.push_str("\\A");
                i += 1;
                quantifiable = false;
                after_group = false;
            }
            b'$' => {
                out.push_str("\\z");
                has_dollar = true;
                i += 1;
                quantifiable = false;
                after_group = false;
            }
            b'|' => {
                out.push('|');
                i += 1;
                quantifiable = false;
                after_group = false;
            }
            _ => {
                exact_regex_push_lit(&mut out, c);
                i += 1;
                quantifiable = true;
                after_group = false;
            }
        }
    }
    if depth != 0 {
        return None;
    }
    Some(ExactRegex {
        rust: out,
        has_dollar,
        quantified_group,
    })
}

/// A compiled std-engine regex for an [`ExactRegex`] (`None` when only
/// `fancy-regex` takes it or it does not compile: decline).
pub fn exact_std_regex(rust: &str) -> Option<regex::Regex> {
    match crate::compile_anchored_cached(rust)? {
        crate::regex_matcher::JavaRegex::Std(r) => Some(r),
        _ => None,
    }
}

/// One piece of a parsed Java replacement string.
pub enum JavaReplPart {
    Lit(Vec<u16>),
    Group(usize),
}

/// `Matcher.appendExpandedReplacement` (JDK 25) as a parse against
/// `group_count`; `None` wherever the JDK would throw (trailing `\`, `$` at
/// the end, `$` + non-digit, a group number above `group_count`) and for the
/// named form `${..}` -- the JDK body answers those.
pub fn parse_java_replacement(r: &[u16], group_count: usize) -> Option<Vec<JavaReplPart>> {
    let mut parts = Vec::new();
    let mut lit: Vec<u16> = Vec::new();
    let mut i = 0usize;
    while i < r.len() {
        let c = r[i];
        if c == u16::from(b'\\') {
            i += 1;
            lit.push(*r.get(i)?);
            i += 1;
        } else if c == u16::from(b'$') {
            i += 1;
            let first = *r.get(i)?;
            if !(u16::from(b'0')..=u16::from(b'9')).contains(&first) {
                return None; // `${name}` or an illegal reference
            }
            let mut num = usize::from(first - u16::from(b'0'));
            i += 1;
            while let Some(&d) = r.get(i) {
                if !(u16::from(b'0')..=u16::from(b'9')).contains(&d) {
                    break;
                }
                let next = num * 10 + usize::from(d - u16::from(b'0'));
                if next > group_count {
                    break;
                }
                num = next;
                i += 1;
            }
            if num > group_count {
                return None; // IndexOutOfBoundsException: No group N
            }
            if !lit.is_empty() {
                parts.push(JavaReplPart::Lit(std::mem::take(&mut lit)));
            }
            parts.push(JavaReplPart::Group(num));
        } else {
            lit.push(c);
            i += 1;
        }
    }
    if !lit.is_empty() {
        parts.push(JavaReplPart::Lit(lit));
    }
    Some(parts)
}

/// `Matcher.find()`'s iteration over `s` (ASCII): each match's
/// `(start, end)` and its captures, with the JDK's empty-match rule
/// (`if (last == first) next++`, nothing past the end).
pub fn java_find_iter<'h>(
    re: &'h regex::Regex,
    s: &'h str,
) -> impl Iterator<Item = regex::Captures<'h>> + 'h {
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    std::iter::from_fn(move || {
        let mut next = last;
        if first == Some(last) {
            next += 1;
        }
        if next > s.len() {
            return None;
        }
        let caps = re.captures_at(s, next)?;
        let m = caps.get(0)?;
        first = Some(m.start());
        last = m.end();
        Some(caps)
    })
}

#[derive(Clone, Copy)]
pub enum StringRegexOp {
    Matches,
    ReplaceAll,
    ReplaceFirst,
    Split(i32),
}

/// `CRATONVM_STRING_REGEX_EXACT` (nio21 wave 10 L4, default ON).
///
/// JIT r23 wave 4, lane L5: memoised in a `MemoSlot` (1 off, 2 on; re-derived
/// on a test override). It was read through `runtime_flag_default_on` on every
/// `String.matches` / `replaceAll` / `replaceFirst` / `split` call -- a hash of
/// the name against the declared-flag set plus an `OsString` copy, 3.2 M reads
/// in a 6 s `matches` spin on n23-w3 (`CRATONVM_DBG_FLAGREADS=1`), together with
/// [`string_regex_memo_enabled`] the largest named leaf of the row.
pub fn string_regex_exact_enabled() -> bool {
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

/// `CRATONVM_STRING_REGEX_SCRATCH` (JIT r23 wave 4, lane L5; perf, default ON,
/// `=0` restores the allocating reads): the memoised exact path
/// ([`string_regex_memo_enabled`]) reads the receiver and the pattern into
/// per-thread scratch buffers and builds the ASCII subject `&str` in a reused
/// `String`, instead of two fresh `Vec<u16>`s and a `from_utf16_lossy` `String`
/// per call (the allocator was the largest leaf of the `String.matches` row on
/// n23-w3: `RtlAllocateHeap` + `RtlFreeHeap` + `GetProcessHeap` ~11 % of the
/// samples). The buffers are taken out of the slot for the call and put back
/// after, so a nested call finds empty ones; the answers are the same functions
/// over the same units. Memoised like [`string_regex_exact_enabled`].
pub fn string_regex_scratch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_STRING_REGEX_KNOWN_STRING_READ` (JIT r23 wave 5, lane L5; perf,
/// default ON, `=0` restores [`decode_string_chars`]): the scratch path reads
/// its receiver and pattern -- both typed `String` by the method's own
/// signature, and `String` is final -- through
/// `NativeHeapAccess::append_known_string_units` (the VM's in-place payload
/// decode) instead of [`decode_string_chars`]'s per-field context reads
/// (`get_field` with its layout resolution, twice per string: on the n23-w4
/// `String.matches` row those layers were ~20 % of the samples). A `false`
/// from the context falls back to [`decode_string_chars`]; both readers are
/// lossless, so the units are the same. Memoised like
/// [`string_regex_exact_enabled`].
pub fn string_regex_known_string_read_enabled() -> bool {
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

/// `obj`'s units into `out` (cleared first) for the scratch path; see
/// [`string_regex_known_string_read_enabled`].
pub fn read_regex_string_units(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    out: &mut Vec<u16>,
) {
    out.clear();
    if string_regex_known_string_read_enabled() && ctx.append_known_string_units(obj, out) {
        return;
    }
    decode_string_chars(ctx, obj, out);
}

/// [`string_regex_scratch_enabled`]'s per-thread buffers.
#[derive(Default)]
pub struct StringRegexScratch {
    pub text: Vec<u16>,
    pub pattern: Vec<u16>,
    pub subject: String,
}

/// A buffer grown past this many elements is not kept for the next call.
pub const STRING_REGEX_SCRATCH_KEEP: usize = 1 << 16;

thread_local! {
    pub static STRING_REGEX_SCRATCH: std::cell::Cell<StringRegexScratch> =
        std::cell::Cell::new(StringRegexScratch {
            text: Vec::new(),
            pattern: Vec::new(),
            subject: String::new(),
        });
}

/// The memoised exact path of [`string_regex_exact`] over the scratch buffers;
/// the same answers as its allocating twin below.
pub fn string_regex_exact_scratch(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    regex_obj: cratonvm_types::ObjectRef,
    repl_obj: Option<cratonvm_types::ObjectRef>,
    op: StringRegexOp,
) -> Option<MethodCallResult> {
    let mut buf = STRING_REGEX_SCRATCH.with(|c| c.take());
    let answer = string_regex_exact_scratch_with(ctx, this, regex_obj, repl_obj, op, &mut buf);
    if buf.text.capacity() <= STRING_REGEX_SCRATCH_KEEP
        && buf.pattern.capacity() <= STRING_REGEX_SCRATCH_KEEP
        && buf.subject.capacity() <= STRING_REGEX_SCRATCH_KEEP
    {
        STRING_REGEX_SCRATCH.with(|c| c.set(buf));
    }
    answer
}

pub fn string_regex_exact_scratch_with(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    regex_obj: cratonvm_types::ObjectRef,
    repl_obj: Option<cratonvm_types::ObjectRef>,
    op: StringRegexOp,
    buf: &mut StringRegexScratch,
) -> Option<MethodCallResult> {
    read_regex_string_units(&*ctx, this, &mut buf.text);
    let text = &buf.text;
    if text.iter().any(|&u| u > 0x7F) {
        return None;
    }
    read_regex_string_units(&*ctx, regex_obj, &mut buf.pattern);
    let row = exact_regex_memo(&buf.pattern);
    let x = row.exact.as_ref()?;
    if x.has_dollar && text.iter().any(|&u| u == 0x0A || u == 0x0D) {
        return None;
    }
    // ASCII, so byte offsets are UTF-16 indices.
    buf.subject.clear();
    buf.subject.extend(text.iter().map(|&u| char::from(u as u8)));
    let s = buf.subject.as_str();
    match op {
        StringRegexOp::Matches => {
            let m = row.anchored.as_ref()?.is_match(s);
            Some(Ok(Some(Value::Int(i32::from(m)))))
        }
        StringRegexOp::ReplaceAll | StringRegexOp::ReplaceFirst => {
            let repl = read_string_chars(&*ctx, repl_obj?);
            let first_only = matches!(op, StringRegexOp::ReplaceFirst);
            let re = row.plain.as_ref()?;
            match exact_regex_replace_units(x, re, text, s, &repl, first_only)? {
                None => Some(Ok(Some(Value::Object(Some(this))))),
                Some(out) => Some(
                    string_native_result_from_units(ctx, &out, false)
                        .map(|o| Some(Value::Object(Some(o)))),
                ),
            }
        }
        StringRegexOp::Split(limit) => {
            let ranges = exact_regex_split_with(row.plain.as_ref()?, s, limit)?;
            Some(string_array_from_ranges(ctx, this, text, &ranges))
        }
    }
}

/// The three `String` regex `Intrinsic`s a compiled call site may bind to a
/// thin helper (JIT r23 wave 4, lane L5, `CRATONVM_JIT_STRING_REGEX_DIRECT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringRegexThinOp {
    /// `String.matches(String)`.
    Matches,
    /// `String.replaceAll(String, String)`.
    ReplaceAll,
    /// `String.replaceFirst(String, String)`.
    ReplaceFirst,
}

/// The thin helper's door into the registered natives
/// (`native_string_matches` / `native_string_replace_all` /
/// `native_string_replace_first`): their FIRST step, the exact path, and only
/// its successful answers. `args` is the native's own argument vector
/// (receiver, regex[, replacement]). `None` is every other outcome -- the exact
/// path disabled or declining, a non-ASCII receiver, a pattern outside the
/// exact subset, an exception -- and the caller then dispatches the
/// registered native, which runs the same exact path first and so answers
/// identically. The exact path writes nothing before it answers (its only
/// side effect is the result `String`), so running it twice on a decline is
/// unobservable.
pub fn string_regex_thin(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: StringRegexThinOp,
) -> Option<Value> {
    if !string_regex_exact_enabled() {
        return None;
    }
    let op = match op {
        StringRegexThinOp::Matches => StringRegexOp::Matches,
        StringRegexThinOp::ReplaceAll => StringRegexOp::ReplaceAll,
        StringRegexThinOp::ReplaceFirst => StringRegexOp::ReplaceFirst,
    };
    match string_regex_exact(ctx, args, op)? {
        Ok(Some(v)) => Some(v),
        _ => None,
    }
}

/// The exact answer, or `None` to decline (see the section comment).
pub fn string_regex_exact(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: StringRegexOp,
) -> Option<MethodCallResult> {
    let obj = |n: usize| match args.get(n) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj(0)?;
    let regex_obj = obj(1)?;
    let repl_obj = match op {
        StringRegexOp::ReplaceAll | StringRegexOp::ReplaceFirst => Some(obj(2)?),
        _ => None,
    };
    if string_regex_memo_enabled() && string_regex_scratch_enabled() {
        return string_regex_exact_scratch(ctx, this, regex_obj, repl_obj, op);
    }
    let text = read_string_chars(ctx, this);
    if text.iter().any(|&u| u > 0x7F) {
        return None;
    }
    let pattern = read_string_chars(ctx, regex_obj);
    // JIT r23 wave 3, lane L5 (`CRATONVM_STRING_REGEX_MEMO`): the memoised
    // parse and the compiled regexes, not re-derived per call.
    if string_regex_memo_enabled() {
        let row = exact_regex_memo(&pattern);
        let x = row.exact.as_ref()?;
        if x.has_dollar && text.iter().any(|&u| u == 0x0A || u == 0x0D) {
            return None;
        }
        // ASCII, so byte offsets are UTF-16 indices.
        let s = String::from_utf16_lossy(&text);
        return match op {
            StringRegexOp::Matches => {
                let m = row.anchored.as_ref()?.is_match(&s);
                Some(Ok(Some(Value::Int(i32::from(m)))))
            }
            StringRegexOp::ReplaceAll | StringRegexOp::ReplaceFirst => {
                let repl = read_string_chars(ctx, repl_obj?);
                let first_only = matches!(op, StringRegexOp::ReplaceFirst);
                let re = row.plain.as_ref()?;
                match exact_regex_replace_with(x, re, &s, &repl, first_only)? {
                    None => Some(Ok(Some(Value::Object(Some(this))))),
                    Some(out) => Some(
                        string_native_result_from_units(ctx, &out, false)
                            .map(|o| Some(Value::Object(Some(o)))),
                    ),
                }
            }
            StringRegexOp::Split(limit) => {
                let ranges = exact_regex_split_with(row.plain.as_ref()?, &s, limit)?;
                Some(string_array_from_ranges(ctx, this, &text, &ranges))
            }
        };
    }
    let x = java_regex_exact_subset(&pattern)?;
    if x.has_dollar && text.iter().any(|&u| u == 0x0A || u == 0x0D) {
        return None;
    }
    // ASCII, so byte offsets are UTF-16 indices.
    let s = String::from_utf16_lossy(&text);
    match op {
        StringRegexOp::Matches => {
            let m = exact_regex_matches(&x, &s)?;
            Some(Ok(Some(Value::Int(i32::from(m)))))
        }
        StringRegexOp::ReplaceAll | StringRegexOp::ReplaceFirst => {
            let repl = read_string_chars(ctx, repl_obj?);
            let first_only = matches!(op, StringRegexOp::ReplaceFirst);
            match exact_regex_replace(&x, &s, &repl, first_only)? {
                // `return text.toString();` -- the receiver itself.
                None => Some(Ok(Some(Value::Object(Some(this))))),
                Some(out) => Some(
                    string_native_result_from_units(ctx, &out, false)
                        .map(|o| Some(Value::Object(Some(o)))),
                ),
            }
        }
        StringRegexOp::Split(limit) => {
            let ranges = exact_regex_split(&x, &s, limit)?;
            Some(string_array_from_ranges(ctx, this, &text, &ranges))
        }
    }
}

/// `Pattern.matches(regex, s)` for an [`ExactRegex`] over ASCII `s`.
pub fn exact_regex_matches(x: &ExactRegex, s: &str) -> Option<bool> {
    let re = exact_std_regex(&format!("\\A(?:{})\\z", x.rust))?;
    Some(re.is_match(s))
}

/// `CRATONVM_STRING_REGEX_MEMO` (JIT r23 wave 3, lane L5; perf, default ON,
/// `=0` restores the per-call path): the `String` regex natives keep, per
/// thread, the last [`EXACT_REGEX_MEMO_CAP`] patterns' parse
/// ([`java_regex_exact_subset`], including a "not exact" verdict) and their
/// two compiled `regex::Regex`es, NOT cloned per call. The per-call path
/// re-parsed the pattern, re-formatted the anchored form, took the global
/// cache's lock and CLONED the `Regex` -- and a clone carries a fresh cache
/// pool, so every search also built a new `regex_automata` cache
/// (`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`: allocation was the
/// largest leaf of the `String.matches` row). The answers are the same
/// functions over the same compiled patterns. Keyed by the pattern's UTF-16
/// units only: a pattern is VM-independent text, so the memo holds no VM
/// state.
///
/// JIT r23 wave 4: memoised in a `MemoSlot`, as [`string_regex_exact_enabled`].
pub fn string_regex_memo_enabled() -> bool {
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

/// How many patterns [`exact_regex_memo`] keeps per thread.
pub const EXACT_REGEX_MEMO_CAP: usize = 16;

/// One [`exact_regex_memo`] row.
pub struct ExactRegexMemoRow {
    /// The pattern's UTF-16 units (the key).
    pub pattern: Vec<u16>,
    /// `java_regex_exact_subset(pattern)`.
    pub exact: Option<ExactRegex>,
    /// `\A(?:rust)\z`, for `matches`; `None` when not exact or not a std regex.
    pub anchored: Option<regex::Regex>,
    /// `rust` itself, for the find-based ops.
    pub plain: Option<regex::Regex>,
}

thread_local! {
    /// [`exact_regex_memo`]'s rows, most recently used last.
    pub static EXACT_REGEX_MEMO: std::cell::RefCell<Vec<std::rc::Rc<ExactRegexMemoRow>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The memoised parse and compiled regexes of `pattern` (see
/// [`string_regex_memo_enabled`]).
pub fn exact_regex_memo(pattern: &[u16]) -> std::rc::Rc<ExactRegexMemoRow> {
    let hit = EXACT_REGEX_MEMO.with(|m| {
        let mut rows = m.borrow_mut();
        let pos = rows.iter().rposition(|r| r.pattern == pattern)?;
        let row = rows.remove(pos);
        rows.push(row.clone());
        Some(row)
    });
    if let Some(row) = hit {
        return row;
    }
    let exact = java_regex_exact_subset(pattern);
    let (anchored, plain) = match &exact {
        Some(x) => (
            regex::Regex::new(&format!("\\A(?:{})\\z", x.rust)).ok(),
            regex::Regex::new(&x.rust).ok(),
        ),
        None => (None, None),
    };
    let row = std::rc::Rc::new(ExactRegexMemoRow {
        pattern: pattern.to_vec(),
        exact,
        anchored,
        plain,
    });
    EXACT_REGEX_MEMO.with(|m| {
        let mut rows = m.borrow_mut();
        if rows.len() >= EXACT_REGEX_MEMO_CAP {
            rows.remove(0);
        }
        rows.push(row.clone());
    });
    row
}

/// `Matcher.replaceAll` / `replaceFirst` for an [`ExactRegex`] over ASCII
/// `s`: `Some(None)` = no match (the JDK returns the receiver), `Some(Some(units))`
/// = the result, `None` = decline (a replacement the JDK would throw on, or a
/// group ref against a quantified group).
pub fn exact_regex_replace(
    x: &ExactRegex,
    s: &str,
    repl: &[u16],
    first_only: bool,
) -> Option<Option<Vec<u16>>> {
    let re = exact_std_regex(&x.rust)?;
    exact_regex_replace_with(x, &re, s, repl, first_only)
}

/// [`exact_regex_replace`] over an already compiled `x.rust` (`re`).
pub fn exact_regex_replace_with(
    x: &ExactRegex,
    re: &regex::Regex,
    s: &str,
    repl: &[u16],
    first_only: bool,
) -> Option<Option<Vec<u16>>> {
    let group_count = re.captures_len().checked_sub(1)?;
    let parts = parse_java_replacement(repl, group_count)?;
    if x.quantified_group
        && parts
            .iter()
            .any(|p| matches!(p, JavaReplPart::Group(g) if *g > 0))
    {
        return None;
    }
    let text: Vec<u16> = s.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::new();
    let mut append_pos = 0usize;
    let mut found = false;
    for caps in java_find_iter(re, s) {
        let m = caps.get(0)?;
        found = true;
        out.extend_from_slice(&text[append_pos..m.start()]);
        for part in &parts {
            match part {
                JavaReplPart::Lit(u) => out.extend_from_slice(u),
                JavaReplPart::Group(g) => {
                    if let Some(gm) = caps.get(*g) {
                        out.extend_from_slice(&text[gm.start()..gm.end()]);
                    }
                }
            }
        }
        append_pos = m.end();
        if first_only {
            break;
        }
    }
    if !found {
        return Some(None);
    }
    out.extend_from_slice(&text[append_pos..]);
    Some(Some(out))
}

