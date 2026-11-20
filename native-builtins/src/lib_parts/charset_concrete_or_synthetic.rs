#[allow(unused_imports)]
use super::super::*;

/// A **concrete** `Charset` for `name`, preferring the real JDK's own.
///
/// [`charset_alloc`] fabricates `java/nio/charset/Charset` itself, which is
/// ABSTRACT — so `newEncoder()`/`newDecoder()` on the result have no `Code`
/// attribute and every caller that encodes dies with `AbstractMethodError`.
/// That is the same class-is-abstract trap `panama::CRATON_SEGMENT_CLASS` and
/// `CRATON_BUFFER_POOL_CLASS` are named for.
///
/// MEASURED 2026-08-25, `probes/PrintStreamCharset.java`, before this helper:
///
/// ```text
///   System.out.charset()             -> java.nio.charset.Charset   [abstract]
///   System.out.charset().newEncoder()-> AbstractMethodError
///   new PrintStream(baos,true,"UTF-8").charset() -> sun.nio.cs.UTF_8   [ok]
/// ```
///
/// The explicit-charset constructor was already right, which is the tell: the
/// real `Charset.forName` returns a concrete `sun.nio.cs.*` on a real image, so
/// there is a good answer available and the fabrication was reaching for it
/// unnecessarily. Ask the JDK first; fall back to the stand-in only when there
/// is no real class library to ask (a synthetic-JDK image), where an abstract
/// carrier is still better than a null.
pub fn charset_concrete_or_synthetic(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let arg = ctx.create_string(name);
    if let Ok(Some(Value::Object(Some(cs)))) = ctx.invoke(
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(arg))],
    ) {
        // Only accept it if it is NOT the abstract base — `Charset.forName` is
        // itself shimmed by `native_charset_for_name`, which calls
        // `charset_alloc`, so a bare `java/nio/charset/Charset` coming back here
        // means the shim answered and we have gained nothing.
        let cid = ctx.class_id_of_object(cs);
        if !matches!(
            ctx.class_name_of_id(cid).as_deref(),
            Some("java/nio/charset/Charset")
        ) {
            return Ok(cs);
        }
    }
    charset_alloc(ctx, name)
}

pub fn charset_alloc(ctx: &mut dyn NativeContext, name: &str) -> Result<ObjectRef, MethodCallFailed> {
    let charset = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 3)?;
    let charset_pin = ctx.pin_native_root(charset);
    let aliases = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let aliases_pin = ctx.pin_native_root(aliases);
    let name_obj = ctx.create_string(name);
    let charset = ctx.read_native_pin(charset_pin, charset);
    let aliases = ctx.read_native_pin(aliases_pin, aliases);
    ctx.set_field(charset, CHARSET_FIELD_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(charset, 1, Value::Object(Some(aliases)));
    ctx.set_field(charset, 2, Value::Object(None));
    ctx.set_field_by_name(charset, "name", Value::Object(Some(name_obj)));
    ctx.set_field_by_name(charset, "aliases", Value::Object(Some(aliases)));
    ctx.set_field_by_name(charset, "aliasSet", Value::Object(None));
    ctx.unpin_native_roots(aliases_pin);
    ctx.unpin_native_roots(charset_pin);
    Ok(charset)
}

pub fn native_charset_for_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            // `Charset.lookup`'s text (round 15 wave 7, lane drainA; F5 of
            // `r15w3-jdkonly3-throwable-builder-review-findings`; synthetic
            // JDK only -- the row is dropped in every real-JDK boot).
            return Err(RuntimeError::IllegalArgumentException {
                message: "Null charset name".to_string(),
            }
            .into());
        }
    };
    let normalized = normalize_charset_name(&name);
    if normalized.is_empty() {
        // `Charset.lookup2` asks `checkName` for a name no provider knows: an
        // illegal one is `IllegalCharsetNameException(name)`, not unsupported.
        if !charset_name_is_legal(&name) {
            let name_str = ctx.create_string_uninterned(&name);
            return Err(crate::throwable_mint::construct_thrown(
                ctx,
                "java/nio/charset/IllegalCharsetNameException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(name_str))],
                || throw_unsupported_charset_exception_flat(&name),
            ));
        }
        // Real `Charset.forName` throws the specific `UnsupportedCharsetException`
        // subclass (NOT a bare `IllegalArgumentException`) for a syntactically
        // valid but unsupported name. `ResourceBundleMessageSourceTests.
        // resourceBundleMessageSourceWithInvalidDefaultCharsetName` asserts the
        // concrete type via `assertThatExceptionOfType(UnsupportedCharsetException
        // .class)`, which a plain `IllegalArgumentException` fails even though
        // `UnsupportedCharsetException` is itself an `IllegalArgumentException`
        // subclass — the assertion requires the exact/more-specific type.
        return Err(throw_unsupported_charset_exception(ctx, &name));
    }
    if let Some(charset) = cached_real_charset(ctx, &normalized) {
        return Ok(Some(Value::Object(Some(charset))));
    }
    let charset = charset_alloc(ctx, &normalized)?;
    publish_real_charset(ctx, normalized, charset);
    Ok(Some(Value::Object(Some(charset))))
}

/// The calling VM's cached `Charset` for the normalized name `normalized`. The
/// handle is copied out of the table before it is resolved (the old reader held
/// the guard across `resolve_global_root`).
pub fn cached_real_charset(ctx: &dyn NativeContext, normalized: &str) -> Option<ObjectRef> {
    let handle = REAL_CHARSET_CACHE
        .peek(ctx.vm_identity(), |rows| rows.get(normalized).copied())
        .flatten()?;
    ctx.resolve_global_root(handle)
}

/// File `charset` (current) as the calling VM's `Charset` for `normalized`. A
/// racing thread's row it replaces has its root released, in this VM's table.
pub fn publish_real_charset(ctx: &mut dyn NativeContext, normalized: String, charset: ObjectRef) {
    let vm = ctx.vm_identity();
    let handle = ctx.add_global_root(charset);
    if handle == 0 {
        return;
    }
    let replaced = REAL_CHARSET_CACHE.with(vm, |rows| rows.insert(normalized, handle));
    if let Some(replaced) = replaced.filter(|&h| h != handle) {
        ctx.remove_global_root(replaced);
    }
}

/// Construct and throw a real `java.nio.channels.NonWritableChannelException`.
///
/// Round 15 wave 3, lane jdkonly3
/// (`r15w3-jdkonly2-new-object-then-init-throws-pre-constructor-address-FIXED-20260930.md`):
/// `new_object_initialized` keeps the object rooted across `<init>` and
/// returns the forwarded reference. The old `new_object` + `invoke` threw the
/// pre-constructor address (the constructor allocates its stack trace, so a
/// moving collection there made it stale) and ignored a constructor that
/// threw; that now takes the flat fallback.
pub fn throw_non_writable_channel_exception(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    match ctx.new_object_initialized("java/nio/channels/NonWritableChannelException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IOException {
            message: "Channel is not writable".into(),        }
        .into(),
    }
}

/// Construct and throw a real `java.nio.charset.UnsupportedCharsetException`
/// via its public `(String charsetName)` constructor, matching real JDK's
/// `Charset.forName` contract for a syntactically valid but unsupported name.
/// Built as [`throw_non_writable_channel_exception`] is (round 15 wave 3); the
/// name is computed text, so a fresh string, as HotSpot's is.
pub fn throw_unsupported_charset_exception(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> MethodCallFailed {
    let name_str = ctx.create_string_uninterned(name);
    match ctx.new_object_initialized(
        "java/nio/charset/UnsupportedCharsetException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(name_str))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => throw_unsupported_charset_exception_flat(name),
    }
}

pub fn throw_unsupported_charset_exception_flat(name: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: format!("Unsupported charset: {}", name),
    }
    .into()
}

/// `java.nio.charset.Charset.checkName`: non-empty; ASCII letters and digits
/// anywhere, `-` `+` `:` `_` `.` after the first character.
pub fn charset_name_is_legal(name: &str) -> bool {
    !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric() || (i > 0 && matches!(c, '-' | '+' | ':' | '_' | '.'))
        })
}


pub fn native_charset_default(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // CONCRETE, for the same reason `PrintStream.charset()` is: the caller's
    // next move is `newEncoder()`, which is abstract on the fabricated base.
    // JEP 400 pins this to UTF-8 regardless of the platform encoding — it
    // follows `file.encoding`, not `native.encoding`.
    let charset = charset_concrete_or_synthetic(ctx, "UTF-8")?;
    Ok(Some(Value::Object(Some(charset))))
}

/// `Charset.availableCharsets()` — return a real-JDK TreeMap populated
/// with the common charsets. Real-JDK's bytecode walks
/// `CharsetProvider.charsets()` from each provider; the iterator's Charset
/// elements lack a populated `name` slot in our synthetic layout, surfacing
/// as an NPE in `Charset.put(...)` (`name()` returns null) during Tomcat's
/// `B2CConverter.<clinit>` -> `Connector.setURIEncoding`. We bypass that
/// path by constructing a real `java.util.TreeMap` directly and inserting
/// (name -> Charset) pairs via `TreeMap.put`. The `check_override` gate
/// in `vm_exec.rs` ensures dispatch chooses this native over the JDK
/// bytecode for `availableCharsets()`.
pub fn native_charset_available_charsets(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let charsets = [
        "US-ASCII",
        "ISO-8859-1",
        "ISO-8859-2",
        "ISO-8859-3",
        "ISO-8859-4",
        "ISO-8859-5",
        "ISO-8859-15",
        "UTF-8",
        "UTF-16",
        "UTF-16BE",
        "UTF-16LE",
    ];
    let map = match ctx.new_object("java/util/TreeMap")? {
        Some(Value::Object(Some(map))) => map,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke(
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )?;
    let map_pin = ctx.pin_native_root(map);
    for name in charsets {
        let value = charset_concrete_or_synthetic(ctx, name)?;
        let value_pin = ctx.pin_native_root(value);
        let key = ctx.create_string(name);
        let map = ctx.read_native_pin(map_pin, map);
        let value = ctx.read_native_pin(value_pin, value);
        // gc-common w11-c: the reclaiming door (the TreeMap route keeps it).
        // `map` is re-read through its pin after the loop; this native already
        // runs `TreeMap.<init>` bytecode, so its callers treat it as a GC point.
        cratonvm_native_collections::native_map_put_entry_pub(
            ctx,
            &[
                Value::Object(Some(map)),
                Value::Object(Some(key)),
                Value::Object(Some(value)),
            ],
        )?;
        ctx.unpin_native_roots(value_pin);
    }
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    Ok(Some(Value::Object(Some(map))))
}

pub fn native_charset_is_supported(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let supported = !normalize_charset_name(&name).is_empty();
    Ok(Some(Value::Int(if supported { 1 } else { 0 })))
}

pub fn native_charset_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_val = ctx.get_field(this, CHARSET_FIELD_NAME);
    Ok(Some(name_val))
}

pub fn native_charset_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Value::Object(Some(alias_set)) = ctx.get_field_by_name(this, "aliasSet") {
        return Ok(Some(Value::Object(Some(alias_set))));
    }

    let this_pin = ctx.pin_native_root(this);
    let set = build_real_layout_string_hashset(ctx, &[])?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "aliasSet", Value::Object(Some(set)));
    ctx.set_field(this, 2, Value::Object(Some(set)));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(set))))
}

pub fn native_charset_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let name_a = match ctx.get_field(this, CHARSET_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let name_b = match ctx.get_field(other, CHARSET_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if name_a.eq_ignore_ascii_case(&name_b) {
        1
    } else {
        0
    })))
}

pub fn native_charset_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let name = match ctx.get_field(this, CHARSET_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default().to_lowercase(),
        _ => String::new(),
    };
    let mut h: i32 = 0;
    for b in name.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

pub fn native_std_charset_utf8(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx, "UTF-8",
    )?))))
}
fn native_std_charset_utf16(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx, "UTF-16",
    )?))))
}
fn native_std_charset_utf16be(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx, "UTF-16BE",
    )?))))
}
fn native_std_charset_utf16le(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx, "UTF-16LE",
    )?))))
}
fn native_std_charset_ascii(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx, "US-ASCII",
    )?))))
}
fn native_std_charset_latin1(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(charset_concrete_or_synthetic(
        ctx,
        "ISO-8859-1",
    )?))))
}

/// Normalize a charset name to its canonical form, or return empty if unsupported.
pub fn normalize_charset_name(name: &str) -> String {
    // Alias resolution is delegated to the shared table next to the
    // transcoding engine (`cratonvm_native_api::charset::
    // canonical_charset_name`), which the StreamEncoder/StreamDecoder shims
    // in cratonvm-native-io also use — their former private copy went stale
    // (no IBM850, no multibyte families) and silently UTF-8-encoded ibm850
    // OutputStreamWriters in Tomcat's DefaultServlet include conversion.
    if let Some(canon) = cratonvm_native_api::charset::canonical_charset_name(name) {
        return canon.to_string();
    }
    let upper = name.to_uppercase();
    if upper.contains("UTF") && upper.contains("8") {
        "UTF-8".to_string()
    } else {
        String::new()
    }
}

// DEPRECATED (Session 15): BigInteger/BigDecimal - synthetic Rust implementation.
// Only used in synthetic-jdk mode. In real JDK mode, java.math.* loads from JDK bytecode.
// ===========================================================================
// Phase 27: BigInteger — arbitrary precision integer arithmetic
// 2-field synthetic (field 0 = String decimal representation, field 1 = Int signum)
// ===========================================================================

pub const BI_FIELD_VALUE: usize = 0; // Synthetic-mode: String decimal representation
pub const BI_FIELD_SIGNUM: usize = 1; // Synthetic-mode: Int signum (-1, 0, or 1)

/// Convert a big-endian base-2^32 magnitude array to a decimal string.
/// Returns `"0"` for an empty array.  Used by `bi_read` (real-JDK layout).
pub fn mag_words_to_decimal(mag: &[u32]) -> String {
    if mag.is_empty() {
        return "0".to_string();
    }
    // Round 14 wave 2 (lane bigdec): past two words, the limb conversion
    // (`BigInt::to_decimal`: nine digits per division, divide and conquer
    // when wide) instead of one long division of the whole magnitude per
    // DIGIT below. Same string: no leading zeros, "0" for zero.
    // `CRATONVM_BIGINT_MAG_TO_DECIMAL_CHUNKED=0` keeps the loop below.
    if mag.len() > 2
    {
        let le: Vec<u32> = mag.iter().rev().copied().collect();
        return crate::bigint::BigInt::from_le_words(false, le).to_decimal();
    }
    let mut words: Vec<u32> = mag.to_vec();
    let mut digits: Vec<u8> = Vec::new();
    while !words.iter().all(|&w| w == 0) {
        let mut rem: u64 = 0;
        for w in words.iter_mut() {
            let cur = (rem << 32) | (*w as u64);
            *w = (cur / 10) as u32;
            rem = cur % 10;
        }
        digits.push(rem as u8);
    }
    if digits.is_empty() {
        return "0".to_string();
    }
    digits.iter().rev().map(|&d| (d + b'0') as char).collect()
}

/// Convert an unsigned decimal string to a big-endian base-2^32 magnitude
/// vector.  Empty result for `"0"`.  Used by `bi_alloc` (real-JDK layout).
pub fn decimal_to_mag_words(decimal: &str) -> Vec<u32> {
    let s = decimal.trim_start_matches('-');
    if s == "0" || s.is_empty() {
        return Vec::new();
    }
    // Round 14 wave 2 (lane bigdec, BD4-1): a long string goes through
    // `BigInt::from_decimal` (nine digits per step, divide and conquer when
    // wide) instead of one full-magnitude pass per digit plus a front
    // `insert` per new word. Same digits read (`from_decimal` skips every
    // non-ASCII-digit byte, as `to_digit(10)` does here; the leading '-'s are
    // already trimmed), same normalized words, reversed to big-endian.
    // `CRATONVM_BIGINT_FROM_DECIMAL_FAST=0` makes `from_decimal` the
    // digit-at-a-time loop.
    if s.len() > 18 {
        let v = crate::bigint::BigInt::from_decimal(s);
        return v.mag_le().iter().rev().copied().collect();
    }
    // Repeated multiply-and-add over the decimal digits.
    let mut words: Vec<u32> = Vec::new();
    for ch in s.chars() {
        let d = match ch.to_digit(10) {
            Some(v) => v as u64,
            None => continue,
        };
        // multiply existing magnitude by 10
        let mut carry: u64 = d;
        for w in words.iter_mut().rev() {
            let prod = (*w as u64) * 10 + carry;
            *w = prod as u32;
            carry = prod >> 32;
        }
        while carry != 0 {
            words.insert(0, carry as u32);
            carry >>= 32;
        }
    }
    words
}


// ===========================================================================
// BigDecimal — 3-field synthetic (value String, scale Int, precision Int)
// ===========================================================================

pub const BD_FIELD_VALUE: usize = 0; // Synthetic-mode: String decimal representation
pub const BD_FIELD_SCALE: usize = 1; // Synthetic-mode: Int scale
pub const BD_FIELD_PRECISION: usize = 2; // Synthetic-mode: Int precision

/// `INFLATED` sentinel from real-JDK `BigDecimal`: when `intCompact` equals
/// `Long.MIN_VALUE`, the value lives in `intVal` (a `BigInteger`).  Otherwise
/// the compact long is the unscaled value and `intVal` may be null.
pub const BD_INFLATED: i64 = i64::MIN;

/// Format an unscaled integer string + a scale into the canonical
/// `BigDecimal.toString` decimal representation (no exponent — used for
/// arithmetic, not pretty-printing, so we keep it simple and round-trip-able
/// through `f64::parse`).
///
/// # `scale` is an ARGUMENT, and this signature cannot refuse
///
/// `scale` reaches here straight off the object's `scale` field, which
/// `new BigDecimal(BigInteger, int)` lets a caller set to anything, including
/// `Integer.MIN_VALUE`. Two consequences, one of which was a guaranteed VM
/// abort:
///
/// 1. **`-scale` overflowed.** The negative-scale branch used to write
///    `(-scale) as usize`. At `scale == i32::MIN` that is `-i32::MIN`, which
///    **panics in a debug build** ("attempt to negate with overflow") and in
///    release wraps back to `i32::MIN`, which then *sign-extends* through
///    `as usize` to 18446744071562067968 and asks `String::repeat` for it. A
///    Rust panic is not a Java throwable — it takes the VM down and cannot be
///    caught. Now `scale.unsigned_abs()`, which is total.
/// 2. **The remaining size is still `scale`-driven**, and this function
///    returns a `String`, not a `Result`, so the *refusal* has to be at the
///    caller. It is not there yet — see the NOMINATION in
///    `docs/feature-designs/native-dispatch-architecture.md`.
///
/// ## What HotSpot answers at these boundaries — MEASURED
///
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build) on this
/// host, `scratchpad/f20/Plain.java`. The reachable caller is
/// `math_bignum::bd_read` → `BigDecimal.toPlainString()`:
///
/// ```text
/// new BigDecimal(ZERO, MIN).toPlainString() = 0                        [90 ms]
/// new BigDecimal(ONE,  MIN).toPlainString() !! ArithmeticException: Overflow          [0 ms]
/// new BigDecimal(ONE,  MAX).toPlainString() !! OutOfMemoryError: too large to fit in a String [0 ms]
/// ```
///
/// Row 1 is why the `unscaled == "0" && scale < 0` short-circuit below is not
/// an optimisation but the contract: `toPlainString` tests `signum() == 0`
/// *before* it validates the scale, so a zero value takes any scale. Row 2 is
/// `checkScaleNonZero(-(long) scale)` — the STATIC form, which always throws,
/// unlike the instance `checkScale` that `setScale` uses and that exempts a
/// zero value. Row 3 is a length screen, not an attempted allocation: HotSpot
/// answers in 0 ms with a 3 GB heap available.
///
/// So the two divergences left here are: `scale == i32::MIN` with a nonzero
/// value, where HotSpot throws `ArithmeticException("Overflow")` and this
/// returns a ~2 GB string of zeros; and a large positive `scale`, where
/// HotSpot throws `OutOfMemoryError` immediately and this tries. Neither is a
/// VM abort any more, which is the part that could be fixed inside this
/// signature.
pub fn apply_scale(unscaled: &str, scale: i32) -> String {
    // A zero value with a POSITIVE scale still renders its fractional zeros:
    // `new BigDecimal("0").setScale(2)` is "0.00", and H2 DECIMAL(p,2) columns
    // surface 0 as "0.00" (testScript.sql). Only short-circuit `unscaled == "0"`
    // for scale <= 0 (the pad branch below already produces "0.00" for scale>0);
    // `scale == 0` short-circuits for any unscaled value.
    if scale == 0 || (unscaled == "0" && scale < 0) {
        return unscaled.to_string();
    }
    let (neg, abs) = if let Some(stripped) = unscaled.strip_prefix('-') {
        (true, stripped.to_string())
    } else {
        (false, unscaled.to_string())
    };
    let sign = if neg { "-" } else { "" };
    if scale > 0 {
        let s = scale as usize;
        if abs.len() > s {
            let split = abs.len() - s;
            format!("{}{}.{}", sign, &abs[..split], &abs[split..])
        } else {
            let pad = s - abs.len();
            format!("{}0.{}{}", sign, "0".repeat(pad), abs)
        }
    } else {
        // Negative scale = trailing zeros. `unsigned_abs`, NOT `-scale`:
        // `scale` is caller-chosen and `-i32::MIN` is a panic in debug and a
        // sign-extended 1.8e19 in release. See this function's doc comment.
        let zeros = scale.unsigned_abs() as usize;
        let mut out = String::with_capacity(sign.len() + abs.len() + zeros);
        out.push_str(sign);
        out.push_str(&abs);
        for _ in 0..zeros {
            out.push('0');
        }
        out
    }
}

/// `5^n` as a limb `BigInt` via square-and-multiply.
pub fn bigint_pow5(n: u32) -> crate::bigint::BigInt {
    use crate::bigint::BigInt;
    let mut result = BigInt::from_decimal("1");
    let mut sq = BigInt::from_decimal("5");
    let mut e = n;
    while e > 0 {
        if e & 1 == 1 {
            result = result.mul(&sq);
        }
        e >>= 1;
        if e > 0 {
            sq = sq.mul(&sq);
        }
    }
    result
}

/// Read a `BigDecimal` as `(unscaled BigInteger, scale)` for EXACT decimal
/// arithmetic. Replaces the old `f64` round-trip that silently dropped both
/// scale (Rust `{}`-formatting strips trailing zeros: `10.0` → "10") and
/// precision (an `f64` holds ~15-16 significant digits). Prefers the real-JDK
/// `intCompact`/`intVal` layout, falling back to the synthetic decimal string.
/// Build a limb `BigInt` from an `i64` without a decimal round-trip.
pub fn bigint_from_i64(v: i64) -> crate::bigint::BigInt {
    let mag = v.unsigned_abs();
    crate::bigint::BigInt::from_le_words(v < 0, vec![mag as u32, (mag >> 32) as u32])
}

/// Multiply an unscaled `BigInt` by `10^n` (n >= 0) — used to align scales for
/// `add`/`subtract` (BigDecimal rescales the smaller-scale operand up to the
/// larger scale before adding the unscaled integers).
///
/// # `n` comes from an ARGUMENT, and this function cannot refuse
///
/// `n` is a difference of two caller-chosen `BigDecimal` scales, so it is
/// attacker-controlled up to `Integer.MAX_VALUE`. This body used to build the
/// literal decimal string `"1" + "0"*n` and hand it to `BigInt::from_decimal`:
/// an `n`-byte `String` (up to ~2 GB from one ordinary call) followed by an
/// O(n²) digit-at-a-time parse. Both are gone — see below — but the SIZE OF
/// THE ANSWER is still `n`-driven, and this signature returns a `BigInt`, not
/// a `Result`, so **the refusal has to be at the call sites**. Every caller
/// must have run `math_bignum::bd_pow_ten_check(n)?` first. Three had not, as
/// of 2026-08-13 (lane F20 — see
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `native_bd_add`, `native_bd_subtract` and `bd_truncated_bigint`, all in
/// `native-builtins/src/math_bignum.rs`. `bd_set_scale_impl` is guarded and is
/// the model.
///
/// ## The boundary, MEASURED — do not invent a cap
///
/// `BigDecimal`'s own `10^n` factor is `bigTenToThe(n)`, which is
/// `BigInteger.TEN.pow(n)` for anything past its table, so it inherits
/// `BigInteger.pow`'s range check verbatim. `bd_pow_ten_check` is that check
/// specialised to base 10 and nothing more. Its boundary, measured on this
/// host against `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft
/// build), `scratchpad/f20/Pow10.java`, `-Xmx48m`:
///
/// ```text
/// TEN.pow(715827881) !! java.lang.OutOfMemoryError: Java heap space              [61916 ms]
/// TEN.pow(715827882) !! java.lang.OutOfMemoryError: Java heap space              [73069 ms]
/// TEN.pow(715827883) !! ArithmeticException: BigInteger would overflow supported range [1 ms]
/// TEN.pow(715827884) !! ArithmeticException: BigInteger would overflow supported range [0 ms]
/// ```
///
/// So the JDK refuses at `n >= 715_827_883` and NOT below it: at 715827882 it
/// really tries, and a heap big enough would answer. That is exactly
/// `bd_pow_ten_check`'s predicate (`3n >= Integer.MAX_VALUE`, since 10 has one
/// trailing zero bit and `bitLength(5) == 3`). A guessed cap anywhere under
/// 715827883 would refuse where HotSpot succeeds, which is a fresh divergence
/// and worse than the DoS it removes.
///
/// ## Why the string is gone
///
/// `10^n` is now `(5^n) << n`, which is not a rewrite of the JDK's algorithm
/// but *literally* the JDK's algorithm: `BigInteger.pow` factors
/// `2^getLowestSetBit()` out of the base, exponentiates the odd part by
/// repeated squaring and shifts the powers of two back at the end — see
/// [`bi_pow_check_range`]'s doc comment, which already relies on that
/// factorisation to bound the answer's bit length. For base 10 that is
/// `10 = 2·5`, so `TEN.pow(n)` computes `5^n << n`. Reusing [`bigint_pow5`]
/// keeps it to one square-and-multiply loop in this file rather than a second
/// copy of one.
pub fn bigint_mul_pow10(bi: &crate::bigint::BigInt, n: i32) -> crate::bigint::BigInt {
    if n <= 0 {
        return bi.clone();
    }
    // `BigInteger.TEN.pow(n)` == `5^n << n`. No `n`-byte String, no O(n²)
    // decimal parse; the remaining cost is the multiplication itself, which is
    // the cost of the answer and not of the argument's spelling. Round 14
    // wave 2 (lane bigdec, BD4-2): `5^n` from the per-thread memo.
    crate::math_bignum::bigint_mul_pow10_memo(bi, n as u32)
}

/// Low `bits` of a `BigInt`'s two's-complement representation — the
/// narrowing `BigInteger.intValue()`/`longValue()` semantics.
pub fn bigint_low_twos_complement(v: &crate::bigint::BigInt, bits: u32) -> u64 {
    let le = v.mag_le();
    let low = (le.first().copied().unwrap_or(0) as u64)
        | ((le.get(1).copied().unwrap_or(0) as u64) << 32);
    let low = if v.is_neg() { low.wrapping_neg() } else { low };
    if bits >= 64 {
        low
    } else {
        low & ((1u64 << bits) - 1)
    }
}

/// `BigDecimal.setScale` old-style rounding-mode constants (`RoundingMode`
/// shares the same ordinals via its `oldMode` field — see
/// `RoundingMode.setScale(int,RoundingMode)`'s bytecode, which reads
/// `oldMode` and delegates to `setScale(int,int)`).
pub const BD_ROUND_UP: i32 = 0;
pub const BD_ROUND_DOWN: i32 = 1;
pub const BD_ROUND_CEILING: i32 = 2;
pub const BD_ROUND_FLOOR: i32 = 3;
pub const BD_ROUND_HALF_UP: i32 = 4;
pub const BD_ROUND_HALF_DOWN: i32 = 5;
pub const BD_ROUND_HALF_EVEN: i32 = 6;
pub const BD_ROUND_UNNECESSARY: i32 = 7;

// Executor = 2-field synthetic (field 0 = pool size, field 1 = shutdown flag)
pub const EXEC_FIELD_SIZE: usize = 0;
pub const EXEC_FIELD_SHUTDOWN: usize = 1;
// Future = 2-field synthetic (field 0 = result value, field 1 = done flag)
pub const FUT_FIELD_RESULT: usize = 0;
pub const FUT_FIELD_DONE: usize = 1;

/// Executor compatibility paths run work inline. Return a real, completed
/// `CompletableFuture` instead of a two-slot object carrying FutureTask's real
/// class: concrete FutureTask methods execute JDK bytecode and cannot observe
/// the placeholder fields.
pub fn completed_executor_future(
    ctx: &mut dyn NativeContext,
    result: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // Callable.call() can return a heap object. Retain that result across the
    // CompletableFuture allocation and completion dispatch; both can collect.
    // Tomcat's WebappClassLoaderBase.clearReferencesJdbc() reaches this path
    // during DoHead shutdown, where a raw returned value otherwise became stale
    // before CompletableFuture.complete stored it.
    let result_obj = match result {
        Value::Object(Some(result_obj)) => Some(result_obj),
        _ => None,
    };
    let result_pin = result_obj.map(|result_obj| ctx.pin_native_root(result_obj));
    let outcome = (|| -> Result<ObjectRef, MethodCallFailed> {
        let future = match ctx.new_object_initialized(
            "java/util/concurrent/CompletableFuture",
            "()V",
            &[],
        )? {
            Some(Value::Object(Some(future))) => future,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "could not allocate executor completion future".to_string(),
                }
                .into())
            }
        };
        let future_pin = ctx.pin_native_root(future);
        let result = match (result_pin, result_obj) {
            (Some(result_pin), Some(result_obj)) => {
                Value::Object(Some(ctx.read_native_pin(result_pin, result_obj)))
            }
            _ => result,
        };
        let future = ctx.read_native_pin(future_pin, future);
        let complete = ctx.invoke_virtual(future, "complete", "(Ljava/lang/Object;)Z", &[result]);
        let future = ctx.read_native_pin(future_pin, future);
        ctx.unpin_native_roots(future_pin);
        complete?;
        Ok(future)
    })();
    if let Some(result_pin) = result_pin {
        ctx.unpin_native_roots(result_pin);
    }
    outcome
}

pub fn failed_executor_future(
    ctx: &mut dyn NativeContext,
    error: &MethodCallFailed,
) -> Result<ObjectRef, MethodCallFailed> {
    let future =
        match ctx.new_object_initialized("java/util/concurrent/CompletableFuture", "()V", &[])? {
            Some(Value::Object(Some(future))) => future,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "could not allocate executor completion future".to_string(),
                }
                .into())
            }
        };
    let message = ctx.create_string(&format!("{error}"));
    let throwable = match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/RuntimeException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    )? {
        Some(throwable) => throwable,
        None => {
            return Err(RuntimeError::IllegalStateException {
                message: "could not allocate executor failure throwable".to_string(),
            }
            .into())
        }
    };
    ctx.invoke_virtual(
        future,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        &[Value::Object(Some(throwable))],
    )?;
    Ok(future)
}

pub fn native_new_single_thread(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let exec = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
    crate::phases_early::initialize_real_thread_pool_executor(
        ctx,
        exec,
        1,
        1,
        0,
        "MILLISECONDS",
        crate::phases_early::TpeQueueKind::Linked,
        None,
    )
}

pub fn native_new_fixed_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let exec = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
    crate::phases_early::initialize_real_thread_pool_executor(
        ctx,
        exec,
        n,
        n,
        0,
        "MILLISECONDS",
        crate::phases_early::TpeQueueKind::Linked,
        None,
    )
}

pub fn native_new_cached_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let exec = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
    let thread_factory = match args.first() {
        Some(Value::Object(Some(factory))) => Some(*factory),
        _ => None,
    };
    crate::phases_early::initialize_real_thread_pool_executor(
        ctx,
        exec,
        0,
        i32::MAX,
        60,
        "SECONDS",
        crate::phases_early::TpeQueueKind::Synchronous,
        thread_factory,
    )
}

/// Process-wide singleton async worker pool (Bug D). Kept alive across calls +
/// GC by a persistent root (a `pin_native_root` handle is NOT usable here: that
/// is a transient per-thread pin stack cleared after each native call, not a
/// persistent root). Stored as `(global root handle, ObjectRef)`: the GC
/// remaps the root after a move, but it cannot rewrite this raw static copy —
/// so every use must re-read the current address through
/// `resolve_global_root(handle)` (use-after-move otherwise once the pool is
/// relocated by a moving young GC or promotion). The raw copy is only the
/// fallback of a context with no global-root table (a mock).
///
/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// the `var_handle_roots` rank): the root was `register_var_handle_root` and
/// the re-read `read_var_handle_root(identity hash)`. That registry is keyed
/// by the identity hash, so an object that shares the pool's hash (two live
/// objects do once the heap's counter wraps) and registers a var-handle root
/// OVERWRITES the pool's row: the pool loses its root, and this re-read
/// answers the other object -- `execute` then invoked a `ThreadPoolExecutor`
/// method on a VarHandle, a TypeVariable array, or whatever else registered
/// there. A JNI global root is unique per object (the `jfr.rs` /
/// `lucene_es.rs` pattern).
///
/// One slot PER VM (gc-common w9-b,
/// `common-w8a-process-global-object-singletons-in-native-builtins`). It was
/// one slot per process, and the `(identity_key, ObjectRef)` pair did not
/// protect across VMs: `read_var_handle_root` resolves the key in the CALLING
/// VM's registry, so VM B got either an unrelated object of its own that
/// happened to share VM A's pool's identity hash, or (on a miss) the raw
/// address in A's heap. A global root handle likewise names a row of the VM
/// that created it, which is why the slot stays per VM. Dropped by
/// [`forget_vm_native_root_stores`]; the handle itself lives in the VM's own
/// global-root table and goes with it.
pub static ASYNC_POOL: cratonvm_native_api::vm_scoped::VmScoped<Option<(usize, ObjectRef)>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();
pub static LAST_ASYNC_SUBMIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn monotonicish_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub fn note_async_runnable_submitted() {
    LAST_ASYNC_SUBMIT_MS.store(
        monotonicish_epoch_millis(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn async_submit_handoff_grace() -> std::time::Duration {
    static GRACE: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *GRACE.get_or_init(|| {
        let millis = crate::nbflags().async_submit_grace_ms.unwrap_or(20);
        std::time::Duration::from_millis(millis)
    })
}

/// The duration a `Thread.sleep(requested)` sleeps (both sleep bodies,
/// `lang_system::native_thread_sleep` and `native_thread_sleep_nanos`).
///
/// Interpreter round i3 wave 79, lane L6 (page
/// `i75-L6-a-short-sleep-after-an-async-submit-sleeps-3-seconds`): HotSpot
/// sleeps what was asked. The 50..=100 ms arm used to be raised to a 3 s
/// default floor within a second of any async submit, on ANY thread (the
/// submit stamp is process-wide), so `runAsync(..); sleep(100)` slept ~3 s.
/// Both floors are opt-in now: without `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS`
/// / `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` the request is returned as is (the
/// 1..=10 ms arm's old default of 1 ms was already a no-op), and a floor below
/// the request leaves it unchanged (it used to fall back to the 3 s default).
/// The suites that wanted the pacing set the variable.
pub fn async_handoff_sleep_millis(requested: i64) -> i64 {
    let flags = crate::nbflags();
    let handoff_floor = flags.async_handoff_sleep_floor_ms;
    let worker_floor = flags.async_worker_sleep_floor_ms;
    if handoff_floor.is_none() && worker_floor.is_none() {
        return requested;
    }
    let last = LAST_ASYNC_SUBMIT_MS.load(std::sync::atomic::Ordering::Relaxed);
    let since_submit_ms = (last != 0).then(|| monotonicish_epoch_millis().saturating_sub(last));
    handoff_sleep_millis_with(requested, since_submit_ms, handoff_floor, worker_floor)
}

/// [`async_handoff_sleep_millis`]'s rule without the clock or the flags:
/// `since_submit_ms` is the time since the last async submit (`None`: none
/// yet). A request of 1..=10 ms is raised to `handoff_floor`, one of
/// 50..=100 ms to `worker_floor`, only within a second of a submit and only
/// when the floor is set and at least the request.
pub fn handoff_sleep_millis_with(
    requested: i64,
    since_submit_ms: Option<u64>,
    handoff_floor: Option<i64>,
    worker_floor: Option<i64>,
) -> i64 {
    const HANDOFF_WINDOW_MS: u64 = 1_000;
    let floor = if (1..=10).contains(&requested) {
        handoff_floor
    } else if (50..=100).contains(&requested) {
        worker_floor
    } else {
        return requested;
    };
    match since_submit_ms {
        Some(elapsed) if elapsed <= HANDOFF_WINDOW_MS => {
            floor.filter(|v| *v >= requested).unwrap_or(requested)
        }
        _ => requested,
    }
}




/// The async pool's `ThreadFactory`: `Thread.ofPlatform().name(
/// "ForkJoinPool.commonPool-worker-", 1).daemon(true).factory()` -- what a
/// `runAsync` task's `Thread.currentThread()` reports on HotSpot (the common
/// pool's worker names, numbered from 1, daemon), from the JDK's own builder
/// rather than a fabricated class. `None` (the caller keeps the default
/// factory) when the switch is off or any step fails.
pub fn async_pool_thread_factory(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    const OF_PLATFORM: &str = "Ljava/lang/Thread$Builder$OfPlatform;";
    let builder = match ctx
        .invoke_static(
            "java/lang/Thread",
            "ofPlatform",
            "()Ljava/lang/Thread$Builder$OfPlatform;",
            &[],
        )
        .ok()?
    {
        Some(Value::Object(Some(b))) => b,
        _ => return None,
    };
    let pin = ctx.pin_native_root(builder);
    let prefix = ctx.create_string("ForkJoinPool.commonPool-worker-");
    let builder = ctx.read_native_pin(pin, builder);
    ctx.unpin_native_roots(pin);
    let named = match ctx
        .invoke_virtual(
            builder,
            "name",
            &format!("(Ljava/lang/String;J){OF_PLATFORM}"),
            &[Value::Object(Some(prefix)), Value::Long(1)],
        )
        .ok()?
    {
        Some(Value::Object(Some(b))) => b,
        _ => return None,
    };
    let daemon = match ctx
        .invoke_virtual(
            named,
            "daemon",
            &format!("(Z){OF_PLATFORM}"),
            &[Value::Int(1)],
        )
        .ok()?
    {
        Some(Value::Object(Some(b))) => b,
        _ => return None,
    };
    match ctx
        .invoke_virtual(
            daemon,
            "factory",
            "()Ljava/util/concurrent/ThreadFactory;",
            &[],
        )
        .ok()?
    {
        Some(Value::Object(Some(f))) => Some(f),
        _ => None,
    }
}

/// Get-or-create the singleton bounded async worker pool. Returns `None` if the
/// pool could not be constructed (caller then falls back to inline execution).
pub fn async_worker_pool(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    if let Some((handle, cached)) = ASYNC_POOL.peek(vm, |slot| *slot).flatten() {
        // Re-read the CURRENT address: the GC remaps the global root after a
        // move, not this raw static copy. Contexts without a global-root table
        // (mocks) fall back to the cached ref.
        return Some(ctx.resolve_global_root(handle).unwrap_or(cached));
    }
    // Construct OUTSIDE the lock (the re-entrant <init> calls must not hold it):
    //   new ThreadPoolExecutor(0, 256, 10, SECONDS, new SynchronousQueue())
    // Cached-style: grows on demand to 256, reuses idle workers, retires them
    // after 10s idle so no lingering thread keeps the VM alive past main().
    let tu_cid = ctx
        .ensure_class_initialized("java/util/concurrent/TimeUnit")
        .ok()?;
    let seconds_idx = ctx.static_field_index_by_name(tu_cid, "SECONDS")?;
    // JIT round 16 (lane compatdrain, TV2-3): the workers are named as the
    // common pool's (`ForkJoinPool.commonPool-worker-N`, daemon), by the JDK's
    // own `Thread.ofPlatform()` factory, which leaves
    // `Executors.defaultThreadFactory`'s pool counter to the application.
    // Built first and rooted across the queue's construction (Java).
    let factory = async_pool_thread_factory(ctx);
    let factory_pin = factory.map(|f| ctx.pin_native_root(f));
    let queue =
        match ctx.new_object_initialized("java/util/concurrent/SynchronousQueue", "()V", &[]) {
            Ok(Some(Value::Object(Some(q)))) => q,
            _ => {
                if let Some(pin) = factory_pin {
                    ctx.unpin_native_roots(pin);
                }
                return None;
            }
        };
    let factory = match (factory, factory_pin) {
        (Some(f), Some(pin)) => {
            let f = ctx.read_native_pin(pin, f);
            ctx.unpin_native_roots(pin);
            Some(f)        }
        _ => None,
    };
    // gc-common w21-a: `TimeUnit.SECONDS` is read AFTER `SynchronousQueue.<init>`
    // (Java); it was read before it and handed to the pool's constructor at
    // that earlier address. Nothing between here and the call collects.
    let seconds = ctx.get_static_field(tu_cid, seconds_idx);
    let constructed = match factory {
        Some(f) => ctx.new_object_initialized(
            "java/util/concurrent/ThreadPoolExecutor",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;\
             Ljava/util/concurrent/ThreadFactory;)V",
            &[
                Value::Int(0),
                Value::Int(256),
                Value::Long(10),
                seconds,
                Value::Object(Some(queue)),
                Value::Object(Some(f)),
            ],
        ),
        None => ctx.new_object_initialized(
            "java/util/concurrent/ThreadPoolExecutor",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;)V",
            &[
                Value::Int(0),
                Value::Int(256),
                Value::Long(10),
                seconds,
                Value::Object(Some(queue)),
            ],
        ),
    };
    let pool = match constructed.ok()? {
        Some(Value::Object(Some(p))) => p,
        _ => return None,
    };
    // Keep alive + remapped across GC moves; the handle lets every later use
    // re-read the current address (gc-common w29-b: a global root, not a
    // var-handle root re-read by identity hash -- see `ASYNC_POOL`).
    let handle = ctx.add_global_root(pool);
    // Publish under the table lock, resolve a racing winner outside it
    // (`VmScoped` discipline: no `ctx` call while a row is held).
    let winner = ASYNC_POOL.with(vm, |slot| match *slot {
        Some(existing) => Some(existing),
        None => {
            *slot = Some((handle, pool));
            None
        }
    });
    if let Some((ehandle, existing)) = winner {
        // A racing creator already installed one; use it. Ours is orphaned
        // (core=0 means it has no live worker threads yet): release its root
        // so it can be collected instead of staying rooted for the VM's life.
        ctx.remove_global_root(handle);
        return Some(ctx.resolve_global_root(ehandle).unwrap_or(existing));
    }
    Some(pool)
}

/// Drop `vm`'s async worker pool slot (VM teardown). Idempotent.
pub fn forget_vm_async_worker_pool(vm: usize) {
    ASYNC_POOL.forget(vm);
}

/// The async worker pool IF one has already been created — never creates it.
///
/// `ForkJoinPool.awaitQuiescence` must not bring a pool into existence just to
/// observe that it is idle; "no pool" means "nothing was ever submitted", which
/// is quiescent by definition.
pub fn async_worker_pool_existing(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let (handle, cached) = ASYNC_POOL.peek(ctx.vm_identity(), |slot| *slot).flatten()?;
    Some(ctx.resolve_global_root(handle).unwrap_or(cached))
}

/// `ForkJoinPool.awaitQuiescence(long, TimeUnit)` — THE implementation.
///
/// IMPLEMENTED (was an unconditional `true`, i.e. a claim that could be false).
/// `fork`/`invoke`/`submit`/`join` all run inline on the caller, but
/// `execute(Runnable)` hands the task to a real worker thread through
/// `spawn_runnable_on_real_thread`, so work genuinely can be outstanding here.
///
/// This is registered from `native-builtins` rather than from
/// `native-collections` (where the old constant lived) for one reason: the pool
/// to observe is THIS crate's `ASYNC_POOL`, and `native-collections` cannot see
/// it — the dependency runs the other way.
///
/// # Why this is `pub` and registered from TWO places
///
/// The triple has two registrars, and NEITHER is redundant, because they cover
/// disjoint boot paths:
///
///   * `phases_early::register_real_jdk_forkjoin_essentials` (via
///     `register_essential_natives`) is the ONLY one that runs under
///     `--synthetic-jdk`: `vm_init`'s `if config.use_synthetic_jdk` arm calls
///     `register_builtins` and never reaches `register_forkjoin_quiescence`.
///   * [`register_forkjoin_quiescence`] runs on both real-JDK arms.
///
/// They therefore both must exist, and `register` is last-write-wins — so they
/// must install the SAME callback or the answer depends on which arm booted.
/// They used to be two independently-written bodies (one polling `ASYNC_POOL`,
/// one polling the tracked `FutureTask` roots), which is the dup-fix shape:
/// two lanes fixing one bug and neither seeing the other. Both signals are now
/// consulted here, and both registrars point at this function.
///
/// Quiescent means BOTH that every `FutureTask` handed to a worker has
/// completed AND that the executor has no active worker and an empty queue.
/// Either alone can be stale: the roots list is empty before the first
/// `execute`, and the pool can be momentarily idle between hand-off and
/// dequeue. On timeout the answer is `false`, which is what the method is
/// specified to return — and is only reachable now that it can also be true
/// for a reason.
pub fn native_forkjoin_await_quiescence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (this, timeout, unit) — `timeout` is a long, so read the first Long.
    let timeout = args
        .iter()
        .find_map(|v| match v {
            Value::Long(t) => Some(*t),
            _ => None,
        })
        .unwrap_or(0);
    let unit_nanos = match args.iter().rev().find_map(|v| match v {
        Value::Object(Some(o)) => Some(*o),
        _ => None,
    }) {
        Some(unit) => match ctx.invoke_virtual(unit, "toNanos", "(J)J", &[Value::Long(1)]) {
            Ok(Some(Value::Long(n))) if n > 0 => n,
            // An unreadable TimeUnit: treat the timeout as nanoseconds rather
            // than silently collapsing it to zero.
            _ => 1,
        },
        None => 1,
    };
    let budget = std::time::Duration::from_nanos(
        (timeout.max(0) as u128).saturating_mul(unit_nanos.max(1) as u128) as u64,
    );
    let start = std::time::Instant::now();
    loop {
        // SIGNAL 1 — the `FutureTask` roots `spawn_runnable_on_real_thread`
        // records for every `execute(Runnable)` it hands to a real worker.
        // This is what the `FjpMatrixProbe` row `awaitQuiescence.waitsForExecute`
        // measures: a pool that answers "quiescent" immediately loses the
        // task's effect.
        let tasks_idle = crate::util_concurrent_ext::async_tasks_quiescent(ctx);
        // SIGNAL 2 — the executor itself, which also covers work that reached
        // it by any other route. No pool at all means nothing was ever handed
        // to a worker, which is quiescent by definition.
        let existing_pool = async_worker_pool_existing(ctx);
        let pool_idle = match existing_pool {
            None => true,
            Some(pool) => {
                let active = match ctx.invoke_virtual(pool, "getActiveCount", "()I", &[]) {
                    Ok(Some(Value::Int(n))) => n,
                    // Cannot observe the pool — do not manufacture a timeout
                    // callers would spin on; defer to signal 1.
                    _ => 0,
                };
                let queued = match ctx.invoke_virtual(
                    pool,
                    "getQueue",
                    "()Ljava/util/concurrent/BlockingQueue;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(q)))) => {
                        match ctx.invoke_virtual(q, "size", "()I", &[]) {
                            Ok(Some(Value::Int(n))) => n,
                            _ => 0,                        }                    }
                    _ => 0,
                };
                active <= 0 && queued <= 0            }        };
        if tasks_idle && pool_idle {
            return Ok(Some(Value::Int(1)));
        }
        if start.elapsed() >= budget {
            return Ok(Some(Value::Int(0)));
        }
        // Poll rather than wait on a condition: the executor exposes no
        // quiescence signal, and the caller has already told us how long it is
        // willing to wait. Interpreter round i3 wave 81, lane L6: JDK 25's
        // `ForkJoinPool.externalHelpQuiesce` backs off between scans with
        // `LockSupport.parkNanos(this, waits)`: TIMED_WAITING (parking).
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::TimedParked);
        std::thread::sleep(std::time::Duration::from_millis(1));
        ctx.end_blocking_region();
    }
}

/// Install the real `ForkJoinPool.awaitQuiescence` on the real-JDK boot path.
///
/// # `NativeKind::Bridge` is load-bearing here, not decoration
///
/// This registration used to be made on the ambient category. `register`
/// falls back to `SyntheticStub` when no `set_category`/`with_category` scope
/// covers it, and the registry's real-ForkJoinPool arm drops every
/// `java/util/concurrent/ForkJoinPool` native that is not a `Bridge` on the
/// `keep_real_forkjoinpool_bridge` allow-list (native-api/src/registry.rs) —
/// and `real_forkjoinpool` is ON unless `CRATONVM_SYNTHETIC_FORKJOINPOOL` is
/// set, i.e. by DEFAULT. So this call registered nothing at all in the
/// shipping configuration, and under `--real-jdk` it was refused outright and
/// booked a phantom `SyntheticNativeRegistered` violation against a triple
/// that is in fact a bridge.
///
/// Nothing broke, because `register_essential_natives` had already installed
/// the same triple as a `Bridge` — which is exactly what made it invisible.
/// The kind is stated now, and both registrars install the SAME callback (see
/// [`native_forkjoin_await_quiescence`]), so last-write-wins has nothing left
/// to decide.
///
/// Called after `register_concurrent_natives`; both are last-write-wins.
pub fn register_forkjoin_quiescence(registry: &mut NativeMethodRegistry) {
    { let r = &mut *registry;
        r.register(
            "java/util/concurrent/ForkJoinPool",
            "awaitQuiescence",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            native_forkjoin_await_quiescence, cratonvm_native_api::NativeKind::Bridge,
        );
    };
}

/// Interrupt every worker thread of a (possibly delegate-wrapped) REAL
/// `ThreadPoolExecutor` so `shutdownNow()` actually stops idle workers blocked
/// in `getTask()` -> `workQueue.take()`. Returns `true` if a real executor's
/// workers were found (and interrupted), `false` for CratonVM's synthetic
/// 2-field executor model (which has no `workers` field).
///
/// This is what lets a leaked real executor terminate its non-daemon worker so
/// the VM can exit — e.g. JUnit `assertTimeoutPreemptively`, whose
/// `newSingleThreadExecutor(ThreadFactory)` (the non-intercepted overload)
/// builds a real `ThreadPoolExecutor` and calls `shutdownNow()` in `finally`.
/// Without this the synthetic stub only set a flag, the worker was never
/// interrupted, and the non-daemon worker kept the VM alive past `main()`.
/// True if `exec` is a real `ThreadPoolExecutor` (or a delegate wrapper around
/// one) — i.e. it has a real `workers` field — rather than CratonVM's synthetic
/// 2-field executor. Used to keep synthetic lifecycle stubs from writing slot
/// fields of a real executor (which would corrupt real fields).
/// Generalized real-vs-synthetic-receiver check for the `AbstractExecutorService`
/// family (`submit`/`invokeAll`/... registered on `es`/`aes`/`tp` alike).
///
/// `executor_has_real_workers` (below) disambiguates ONLY the `ThreadPoolExecutor`
/// name collision: CratonVM's own synthetic `Executors.newFixedThreadPool()` et
/// al. placeholders are stamped with the real `ThreadPoolExecutor` class name, so
/// a real bytecode-constructed TPE and a synthetic one are indistinguishable by
/// class name alone there -- hence the per-instance `workers` field probe.
///
/// `submit()` is registered on `AbstractExecutorService` too, and its receiver
/// is not necessarily a `ThreadPoolExecutor` at all: any real class that extends
/// `AbstractExecutorService` directly (e.g. Netty's `AbstractEventExecutor` /
/// `SingleThreadEventExecutor`, which override `newTaskFor()` to hand back a
/// Netty `Future`/`Promise` instead of a JDK `FutureTask`) reaches this same
/// native via `invokespecial AbstractExecutorService.submit(...)` from within
/// their own overriding `submit()` -- see
/// CRATONVM-SPRING-GENUINE-BUGLIST 5.2. Those classes have
/// no `workers` field (that's TPE-specific), so the narrower check misreports
/// them as synthetic and CratonVM's single-threaded-immediate-execution model
/// silently replaces the real `newTaskFor()` override, handing back a plain
/// `CompletableFuture` where the caller's real bytecode expects (and later
/// `checkcast`s to) whatever `Future` subtype `newTaskFor()` actually produces.
///
/// CratonVM only ever fabricates synthetic placeholders under a small,
/// enumerable set of class names (the concrete `ThreadPoolExecutor` collision
/// above, plus the bare interface names `ExecutorService`,
/// `ScheduledExecutorService`, and `Executor` used by the wrapper/unconfigurable
/// synthetic executors in `phases_late.rs`) -- no real object's runtime class
/// can ever literally BE an interface. So: resolve the receiver's concrete
/// runtime class name; if it's one of those known synthetic markers, defer to
/// the existing per-instance checks (or treat bare interfaces as always
/// synthetic, since no real object can have that as its own class); any other
/// concrete class name reaching this code path is necessarily real bytecode.
pub fn executor_is_real(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    let target = match ctx.get_field_by_name(exec, "e") {
        Value::Object(Some(inner)) => inner,
        _ => exec,
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(target))
        .unwrap_or_default();
    match class_name.as_str() {
        "java/util/concurrent/ThreadPoolExecutor" => matches!(
            ctx.get_field_by_name(target, "workers"),
            Value::Object(Some(_))
        ),
        "java/util/concurrent/ExecutorService"
        | "java/util/concurrent/ScheduledExecutorService"
        | "java/util/concurrent/Executor" => false,
        _ => true,
    }
}

/// Round 15 wave 7 (lane drainC; `r15w6-sweepC-review-findings` S1): a
/// receiver that has a non-null `e` AND its own `workers` is itself a real
/// pool (a `ThreadPoolExecutor` subclass declaring its own field `e`), so the
/// `e` unwrap below must not replace it with whatever `e` holds.
/// `CRATONVM_TPE_DOORS_SELF_FIRST=0` (read only for such a receiver) restores
/// the unconditional unwrap.
pub fn executor_own_workers_first(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    matches!(
        ctx.get_field_by_name(exec, "workers"),
        Value::Object(Some(_))
    ) && crate::nbflags().tpe_doors_self_first
}

pub fn executor_has_real_workers(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    let tpe = match ctx.get_field_by_name(exec, "e") {
        Value::Object(Some(inner)) if !executor_own_workers_first(ctx, exec) => inner,
        _ => exec,
    };
    matches!(
        ctx.get_field_by_name(tpe, "workers"),
        Value::Object(Some(_))
    )
}

pub fn interrupt_executor_workers(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    interrupt_executor_workers_filtered(ctx, exec, /* only_idle */ false)
}

/// `interrupt_executor_workers` with the `shutdown()` vs `shutdownNow()`
/// distinction the JDK draws.
///
/// `shutdownNow()` interrupts EVERY worker (`interruptWorkers()`), which is what
/// `only_idle == false` does. `shutdown()` must interrupt only the IDLE ones
/// (`interruptIdleWorkers()`): a worker that is currently RUNNING a task holds
/// its own `Worker` lock, so `w.tryLock()` fails for it and the JDK leaves it
/// alone -- that is the whole meaning of "orderly shutdown: previously
/// submitted tasks are executed".
///
/// Interrupting a running task instead is observable, not cosmetic. H2
/// `MVStore.close()` -> `Utils.shutdownExecutor` -> `shutdown()` runs while the
/// buffer-save worker sits inside `FileChannel.write`; an interrupt there makes
/// `AbstractInterruptibleChannel.begin()` take its `me.isInterrupted()` branch
/// and abort the write, which H2 turns into an `MVStoreException` wrapping
/// `NullPointerException: ... Interruptible.interrupt ... this.interruptor is
/// null` and a failed `TestStreamStore` (2026-07-26).
pub fn interrupt_executor_workers_filtered(
    ctx: &mut dyn NativeContext,
    exec: ObjectRef,
    only_idle: bool,
) -> bool {
    // Unwrap Executors$DelegatedExecutorService / AutoShutdownDelegated...
    // (field `e` -> the inner executor) if present.
    let dbg = crate::nbflags().dbg_exec;
    if dbg {
        let has_e = matches!(ctx.get_field_by_name(exec, "e"), Value::Object(Some(_)));
        eprintln!(
            "[EXEC] interrupt_executor_workers called (has 'e' field={})",
            has_e
        );
    }
    let tpe = match ctx.get_field_by_name(exec, "e") {
        Value::Object(Some(inner)) if !executor_own_workers_first(ctx, exec) => inner,
        _ => exec,
    };
    let workers = match ctx.get_field_by_name(tpe, "workers") {
        Value::Object(Some(w)) => w,
        other => {
            if dbg {
                eprintln!("[EXEC] no workers field (got {:?}) -> synthetic", other);
            }
            return false; // synthetic executor (no real worker set)
        }
    };
    let it = match ctx.invoke_virtual(workers, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        other => {
            if dbg {
                eprintln!("[EXEC] workers.iterator failed: {:?}", other);
            }
            return true; // it's a real TPE; we just couldn't iterate
        }
    };
    let mut interrupted = 0;
    // Bound defensively; the worker set is tiny in practice.
    let it_pin = ctx.pin_native_root(it);
    for _ in 0..4096 {
        let it_now = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_now, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        // gc-common w20-c: `hasNext` is Java; re-read the iterator (it was
        // read once per iteration, before it), and pin the worker across
        // `tryLock` / `unlock` (Java) for the `thread` read after them.
        let it_now = ctx.read_native_pin(it_pin, it);
        let worker = match ctx.invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(w)))) => w,
            _ => break,
        };
        let worker_pin = ctx.pin_native_root(worker);
        // `shutdown()`: skip workers that are running a task. `Worker` extends
        // AQS and `runWorker` holds `w.lock()` for the duration of each task, so
        // a successful `tryLock()` means "idle" exactly as it does in
        // `ThreadPoolExecutor.interruptIdleWorkers`. Unlock immediately after,
        // as the JDK does, so the worker can take its next task.
        //
        // Fail OPEN: if `tryLock` cannot be invoked at all (a synthetic worker
        // with no AQS body), interrupt anyway rather than leave a worker blocked
        // in `getTask()` forever -- an unwoken idle worker turns
        // `awaitTermination(1, DAYS)` into a hang, which is worse than an
        // over-eager interrupt.
        if only_idle {
            match ctx.invoke_virtual(worker, "tryLock", "()Z", &[]) {
                Ok(Some(Value::Int(0))) => {
                    if dbg {
                        eprintln!("[EXEC] worker is running a task -> not interrupted");
                    }
                    ctx.unpin_native_roots(worker_pin);
                    continue;
                }
                Ok(Some(Value::Int(_))) => {
                    let worker_now = ctx.read_native_pin(worker_pin, worker);
                    let _ = ctx.invoke_virtual(worker_now, "unlock", "()V", &[]);
                }
                _ => {}
            }
        }
        let worker: ObjectRef = ctx.read_native_pin(worker_pin, worker);
        ctx.unpin_native_roots(worker_pin);
        if let Value::Object(Some(t)) = ctx.get_field_by_name(worker, "thread") {
            // Mirror the foundation interrupt: set the VM atomic AND the Java
            // Thread.interrupted field so getTask()'s Condition.await wakes and
            // the AQS/await loop observes the interrupt.
            ctx.thread_interrupt(t);
            // Round 15 wave 4 (lane threadvm): `Thread`'s own field.
            crate::lang_misc::set_thread_field(ctx, t, "interrupted", Value::Int(1));
            interrupted += 1;
        }
    }
    ctx.unpin_native_roots(it_pin);
    if dbg {
        eprintln!(
            "[EXEC] interrupt_executor_workers: interrupted {} worker(s)",
            interrupted
        );
    }
    true
}

pub fn juli_utc_date_days_ago(days_ago: i64) -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0)
        - days_ago;
    // Civil-date conversion for a Unix day number (proleptic Gregorian).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn cf_capture_throwable(
    ctx: &mut dyn NativeContext,
    err: &cratonvm_types::error::MethodCallFailed,
) -> Value {
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(r),
        ) => {
            let msg = format!("{r}");
            let s = ctx.create_string(&msg);
            Value::Object(Some(s))
        }
        _ => Value::Object(None),
    }
}

// ===========================================================================
// Phase 29: java.text — DecimalFormat, NumberFormat
// ===========================================================================

pub(crate) fn register_number_format_natives(_registry: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java.text.NumberFormat / DecimalFormat / SimpleDateFormat all have real
    // JDK bytecode. The previous 1-field synthetic substitutes are removed so
    // the real .class bytecode runs instead.
}

pub const NF_FIELD_PATTERN: usize = 0;
pub const NF_FIELD_MAX_FRAC: usize = 1;
pub const NF_FIELD_MIN_FRAC: usize = 2;
pub const NF_FIELD_GROUPING: usize = 3;

/// Initialize the optional NumberFormat fields (max/min fraction digits, grouping).
/// Uses object_num_fields to remain backward-compatible with any 1-field allocations.
pub fn nf_init_optional_fields(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if ctx.object_num_fields(this) > NF_FIELD_MAX_FRAC {
        ctx.set_field(this, NF_FIELD_MAX_FRAC, Value::Int(3));
    }
    if ctx.object_num_fields(this) > NF_FIELD_MIN_FRAC {
        ctx.set_field(this, NF_FIELD_MIN_FRAC, Value::Int(0));
    }
    if ctx.object_num_fields(this) > NF_FIELD_GROUPING {
        ctx.set_field(this, NF_FIELD_GROUPING, Value::Int(1));
    }
}

/// Read formatting config from a NumberFormat object, with safe defaults.
pub fn nf_read_config(ctx: &mut dyn NativeContext, this: ObjectRef) -> (i32, i32, bool) {
    let max_frac = if ctx.object_num_fields(this) > NF_FIELD_MAX_FRAC {
        ctx.get_field(this, NF_FIELD_MAX_FRAC).as_int().unwrap_or(3)
    } else {
        3
    };
    let min_frac = if ctx.object_num_fields(this) > NF_FIELD_MIN_FRAC {
        ctx.get_field(this, NF_FIELD_MIN_FRAC).as_int().unwrap_or(0)
    } else {
        0
    };
    let grouping = if ctx.object_num_fields(this) > NF_FIELD_GROUPING {
        ctx.get_field(this, NF_FIELD_GROUPING).as_int().unwrap_or(1) != 0
    } else {
        true
    };
    (max_frac, min_frac, grouping)
}

/// Format a double according to (max_frac, min_frac, grouping) settings.
pub fn nf_format_double_with_config(d: f64, max_frac: i32, min_frac: i32, grouping: bool) -> String {
    let max_f = max_frac.max(0) as usize;
    let min_f = min_frac.max(0) as usize;
    // Round to max_frac digits
    let rounded = if max_f == 0 {
        format!("{:.0}", d)
    } else {
        format!("{:.*}", max_f, d)
    };
    // Strip trailing zeros down to min_frac
    let (int_part, mut frac_part) = match rounded.find('.') {
        Some(dot) => (rounded[..dot].to_string(), rounded[dot + 1..].to_string()),
        None => (rounded.clone(), String::new()),
    };
    // Trim trailing zeros to min_frac length
    while frac_part.len() > min_f && frac_part.ends_with('0') {
        frac_part.pop();
    }
    // Pad with zeros to reach min_frac
    while frac_part.len() < min_f {
        frac_part.push('0');
    }
    // Apply grouping (commas every 3 digits in integer part)
    let int_grouped = if grouping {
        nf_apply_grouping(&int_part)
    } else {
        int_part
    };
    if frac_part.is_empty() {
        int_grouped
    } else {
        format!("{}.{}", int_grouped, frac_part)
    }
}

/// Insert grouping separators (commas) into the integer part.
pub fn nf_apply_grouping(int_part: &str) -> String {
    let negative = int_part.starts_with('-');
    let digits = if negative { &int_part[1..] } else { int_part };
    if digits.len() <= 3 {
        return int_part.to_string();
    }
    let mut result = String::with_capacity(int_part.len() + digits.len() / 3);
    if negative {
        result.push('-');
    }
    let first_group = digits.len() % 3;
    if first_group > 0 {
        result.push_str(&digits[..first_group]);
        if digits.len() > first_group {
            result.push(',');
        }
    }
    for (i, c) in digits[first_group..].chars().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

pub fn native_nf_get_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // gc-common w20-c: the pattern string first, pinned across the format's
    // allocation (allocated second, it could leave `nf` stale).
    let pattern = ctx.create_string("#,##0.###");
    let pattern_pin = ctx.pin_native_root(pattern);
    let nf: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/text/DecimalFormat", 4)?;
    let pattern: ObjectRef = ctx.read_native_pin(pattern_pin, pattern);
    ctx.unpin_native_roots(pattern_pin);
    ctx.set_field(nf, NF_FIELD_PATTERN, Value::Object(Some(pattern)));
    nf_init_optional_fields(ctx, nf);
    Ok(Some(Value::Object(Some(nf))))
}

pub fn native_nf_format_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let d = match args.get(1) {
        Some(Value::Double(v)) => *v,
        Some(Value::Float(v)) => *v as f64,
        _ => 0.0,
    };
    let (max_f, min_f, grouping) = nf_read_config(ctx, this);
    let s = nf_format_double_with_config(d, max_f, min_f, grouping);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_nf_format_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (_max_f, _min_f, grouping) = nf_read_config(ctx, this);
    let int_str = format!("{}", v);
    let s = if grouping {
        nf_apply_grouping(&int_str)
    } else {
        int_str
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_df_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // gc-common w20-c: `this` is written after the string's allocation.
    let this_pin = ctx.pin_native_root(this);
    let pattern = ctx.create_string("#,##0.###");
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, NF_FIELD_PATTERN, Value::Object(Some(pattern)));
    nf_init_optional_fields(ctx, this);
    Ok(None)
}

pub fn native_df_init_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(
        this,
        NF_FIELD_PATTERN,
        args.get(1).copied().unwrap_or(Value::Object(None)),
    );
    nf_init_optional_fields(ctx, this);
    Ok(None)
}

pub fn native_df_format_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Try to read the object as a number string
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_else(|| "0".to_string()),
        Some(Value::Int(v)) => v.to_string(),
        Some(Value::Long(v)) => v.to_string(),
        Some(Value::Double(v)) => v.to_string(),
        Some(Value::Float(v)) => v.to_string(),
        _ => "0".to_string(),
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_df_to_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, NF_FIELD_PATTERN)))}

pub fn native_df_apply_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(
        this,
        NF_FIELD_PATTERN,
        args.get(1).copied().unwrap_or(Value::Object(None)),
    );
    Ok(None)
}

pub const URL_FIELD_PROTOCOL: usize = 0;
pub const URL_FIELD_HOST: usize = 1;
pub const URL_FIELD_PORT: usize = 2;
pub const URL_FIELD_PATH: usize = 3;
pub const URL_FIELD_QUERY: usize = 4;
pub const URL_FIELD_FULL: usize = 5; // Original string for toString

/// Extract the path portion from a URI string
pub fn extract_path(uri: &str) -> &str {
    if let Some(idx) = uri.find("://") {
        let after_scheme = &uri[idx + 3..];
        if let Some(path_start) = after_scheme.find('/') {
            &uri[idx + 3 + path_start..]
        } else {
            "/"
        }
    } else {
        uri
    }
}

pub fn is_ascii_hex_digit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// Percent-quote characters not permitted in a URI query component (RFC 2396
/// `uric` = reserved | unreserved), mirroring `java.net.URI`'s private
/// `quote(String, L_URIC, H_URIC)`. The multi-argument `URI` constructors
/// (`native_uri_init_5`/`native_uri_init_7`) take *raw/decoded* component
/// strings and must escape anything outside that set — including a literal
/// `%`, which is NOT in `L_URIC` (real JDK always escapes a bare `%` supplied
/// this way to `%25`, since these constructors have no "already escaped"
/// concept). Previously these natives spliced the query string in completely
/// unescaped, so `ServletServerHttpRequest.initURI`'s malformed-query
/// fallback (`new URI(null, null, "", query, null).getRawQuery()`, used to
/// turn "foo%%x" into "foo%25%25x") was a no-op.
pub fn quote_uric(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            let allowed = c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '_'
                        | '.'
                        | '!'
                        | '~'
                        | '*'
                        | '\''
                        | '('
                        | ')'
                        | ';'
                        | '/'
                        | '?'
                        | ':'
                        | '@'
                        | '&'
                        | '='
                        | '+'
                        | '$'
                        | ','
                        | '['
                        | ']'
                );
            if allowed {
                out.push(c);
            } else {
                out.push_str(&format!("%{:02X}", c as u32));
            }
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// InetAddress = 2-field synthetic (field 0 = hostname, field 1 = address)

/// Resolve the host's actual hostname. Tries, in order:
///   1. `HOSTNAME` env var (Linux/macOS shells set this).
///   2. `COMPUTERNAME` env var (Windows).
///   3. The `hostname` command (portable across all three target OSes,
///      part of the base install on every supported platform).
/// Returns `"localhost"` only if every method fails.
///
/// This is reused by `getLocalHost` and `NetworkInterface.getNetworkInterfaces`
/// so both surface the same name to Java code.
///
/// Cached: the result is latched on first use. Step 3 forks and execs the
/// `hostname` binary, and on Linux `HOSTNAME` is frequently *not* exported to
/// non-interactive shells, so steps 1 and 2 miss and every single
/// `InetAddress.getLocalHost()` / `NetworkInterface.getNetworkInterfaces()`
/// call paid a full process spawn. A machine does not rename itself
/// mid-process, and HotSpot itself caches `getLocalHost()`, so one resolution
/// per VM is the right granularity. [`resolve_real_hostname_uncached`] keeps
/// the probing logic testable.
pub fn resolve_real_hostname() -> String {
    static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHED.get_or_init(resolve_real_hostname_uncached).clone()
}

/// The uncached probe behind [`resolve_real_hostname`]. Split out so the
/// resolution order stays directly testable without the `OnceLock` latch.
pub fn resolve_real_hostname_uncached() -> String {
    if let Ok(name) = cratonvm_types::flags::runtime_var("HOSTNAME") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(name) = cratonvm_types::flags::runtime_var("COMPUTERNAME") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // Last-resort portable path: invoke `hostname`. Output may include
    // a trailing newline that we strip, and on Windows it may include
    // the FQDN — we keep the short form to match what HotSpot returns.
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                // Strip an FQDN suffix to match HotSpot which returns the
                // short host name from `getLocalHost().getHostName()`.
                let short = trimmed.split('.').next().unwrap_or(trimmed);
                return short.to_string();
            }
        }
    }
    "localhost".to_string()
}

/// Resolve a hostname to its primary IPv4 address. Used by
/// `InetAddress.getLocalHost` and `NetworkInterface` enumeration.
/// Falls back to `127.0.0.1` if every resolution path fails.
pub fn resolve_primary_ipv4(hostname: &str) -> String {
    // Direct DNS resolution first.
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&format!("{hostname}:0")) {
        if let Some(v4) = addrs.into_iter().find(|a| a.is_ipv4()) {
            return v4.ip().to_string();
        }
    }
    // Fallback: connect a UDP socket to a public DNS resolver and read the
    // OS-selected source IP. No packets are sent — `connect` on a `UdpSocket`
    // only sets the default destination. We do not require the public DNS
    // resolver to be reachable; the bind+connect succeeds locally.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:53").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_unspecified() {
                    return addr.ip().to_string();
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

/// Read a Java `byte[]` of length `len` into a `Vec<u8>` (sign-extending each
/// element back to its unsigned form). Used by [`native_inet_get_by_address`]
/// and the byte-array helper in [`native_inet_get_address`].
pub fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| match ctx.get_array_element(arr, i) {
            // Values come back as i32 (sign-extended from the i8 storage).
            // Cast through `i8` so 0xFF round-trips correctly.
            Value::Int(v) => (v as i8) as u8,
            _ => 0,
        })
        .collect()
}

pub fn dmr_model_type_undefined(ctx: &mut dyn NativeContext) -> Value {
    let cid = match ctx.ensure_class_initialized("org/jboss/dmr/ModelType") {
        Ok(cid) => cid,
        Err(_) => return Value::Object(None),
    };
    match ctx.static_field_index_by_name(cid, "UNDEFINED") {
        Some(idx) => ctx.get_static_field(cid, idx),
        None => Value::Object(None),
    }
}

pub fn native_dmr_model_node_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field_by_name(this, "protect", Value::Int(0));
    // gc-common w20-c: the first call runs `ModelValue.<clinit>` (Java, a GC
    // point), and `this` is written after it.
    let this_pin = ctx.pin_native_root(this);
    let undefined = match ctx.ensure_class_initialized("org/jboss/dmr/ModelValue") {
        Ok(cid) => ctx
            .static_field_index_by_name(cid, "UNDEFINED")
            .map(|idx| ctx.get_static_field(cid, idx))
            .unwrap_or(Value::Object(None)),
        Err(_) => Value::Object(None),
    };
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "value", undefined);
    Ok(None)
}

pub fn native_dmr_model_node_get_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match ctx.get_field_by_name(this, "value") {
        Value::Object(Some(value)) => value,
        _ => return Ok(Some(dmr_model_type_undefined(ctx))),
    };
    match ctx.get_field_by_name(value, "type") {
        Value::Object(Some(t)) => Ok(Some(Value::Object(Some(t)))),
        _ => Ok(Some(dmr_model_type_undefined(ctx))),
    }
}

pub fn native_dmr_model_node_is_defined(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match ctx.get_field_by_name(this, "value") {
        Value::Object(Some(value)) => value,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ty = match ctx.get_field_by_name(value, "type") {
        Value::Object(Some(t)) => t,
        _ => return Ok(Some(Value::Int(0))),
    };
    let undefined = match dmr_model_type_undefined(ctx) {
        Value::Object(Some(u)) => u,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if ty.as_ptr() == undefined.as_ptr() {
        0
    } else {
        1
    })))
}

pub fn native_file_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    #[cfg(windows)]
    let fs_class = "java/io/WinNTFileSystem";
    #[cfg(not(windows))]
    let fs_class = "java/io/UnixFileSystem";
    if let Some(Value::Object(Some(fs))) = ctx.new_object_initialized(fs_class, "()V", &[])? {
        ctx.set_static_field_by_name("java/io/File", "FS", Value::Object(Some(fs)));
    }

    let separator = ctx.create_string(std::path::MAIN_SEPARATOR_STR);
    ctx.set_static_field_by_name("java/io/File", "separator", Value::Object(Some(separator)));
    ctx.set_static_field_by_name(
        "java/io/File",
        "separatorChar",
        Value::Int(std::path::MAIN_SEPARATOR as i32),
    );
    #[cfg(windows)]
    let path_separator = ";";
    #[cfg(not(windows))]
    let path_separator = ":";
    let path_separator_obj = ctx.create_string(path_separator);
    ctx.set_static_field_by_name(
        "java/io/File",
        "pathSeparator",
        Value::Object(Some(path_separator_obj)),
    );
    ctx.set_static_field_by_name(
        "java/io/File",
        "pathSeparatorChar",
        Value::Int(path_separator.chars().next().unwrap_or(':') as i32),
    );
    // WSCLOSE-08-07: this native override REPLACES the real <clinit>
    // bytecode wholesale (registered unconditionally below), so the real
    // FS = DefaultFileSystem.getFileSystem() assignment (JVMS: the FIRST
    // static assigned, before separatorChar/pathSeparator) never runs and
    // File.FS was left permanently null. Any real-JDK File method NOT
    // covered by our check_override allow-list (e.g. isInvalid(),
    // length(), lastModified(), delete(), mkdir(), list(), ...)
    // executes real bytecode that dereferences FS directly (e.g.
    // FS.isInvalid(this)), so every such call NPE'd with "Cannot invoke
    // ... because \"java.io.File.FS\" is null" -- observed first via
    // Tomcat's Digester (SAXParserFactory internals stat a File through
    // a non-overridden path while probing mbeans-descriptors.xml),
    // cascading into full StandardContext startup failure. Build and
    // publish a real (non-null) FileSystem instance the same way the
    // JDK's UnixFileSystem/WinNTFileSystem constructors would, so
    // FS.isInvalid/getBooleanAttributes/etc. all resolve to our real
    // native filesystem natives instead of NPEing on a null receiver.
    let user_dir = ctx
        .get_system_property("user.dir")
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());
    let user_dir_str = ctx.create_string(&user_dir);
    #[cfg(windows)]
    {
        let fs_obj = try_alloc_concurrent_synthetic(ctx, "java/io/WinNTFileSystem", 4)?;
        ctx.set_field_by_name(
            fs_obj,
            "slash",
            Value::Int(std::path::MAIN_SEPARATOR as i32),
        );
        ctx.set_field_by_name(fs_obj, "semicolon", Value::Int(';' as i32));
        ctx.set_field_by_name(fs_obj, "altSlash", Value::Int('/' as i32));
        ctx.set_field_by_name(fs_obj, "userDir", Value::Object(Some(user_dir_str)));
        ctx.set_static_field_by_name("java/io/File", "FS", Value::Object(Some(fs_obj)));
    }
    #[cfg(not(windows))]
    {
        let fs_obj = try_alloc_concurrent_synthetic(ctx, "java/io/UnixFileSystem", 3)?;
        ctx.set_field_by_name(
            fs_obj,
            "slash",
            Value::Int(std::path::MAIN_SEPARATOR as i32),
        );
        ctx.set_field_by_name(fs_obj, "colon", Value::Int(':' as i32));
        ctx.set_field_by_name(fs_obj, "userDir", Value::Object(Some(user_dir_str)));
        ctx.set_static_field_by_name("java/io/File", "FS", Value::Object(Some(fs_obj)));
    }
    native_file_clinit_serialization_offsets(ctx)?;
    Ok(None)
}

/// The tail of the real `File.<clinit>`: `UNSAFE`, `PATH_OFFSET` and
/// `PREFIX_LENGTH_OFFSET`, which `File.readObject` uses to store the
/// deserialized `path` and `prefixLength` (the fields are `final`, so the JDK
/// writes them through `Unsafe`). This native replaces the whole initializer,
/// and without these three a `File` read back from an `ObjectInputStream`
/// failed with `NullPointerException: ... "java.io.File.UNSAFE" is null`
/// (Quarkus dev mode hands its `File`s across the class-loader boundary this
/// way).
pub fn native_file_clinit_serialization_offsets(
    ctx: &mut dyn NativeContext,
) -> Result<(), MethodCallFailed> {
    let Some(Value::Object(Some(unsafe_obj))) = ctx.invoke_static(
        "jdk/internal/misc/Unsafe",
        "getUnsafe",
        "()Ljdk/internal/misc/Unsafe;",
        &[],
    )?
    else {
        return Ok(());
    };
    ctx.set_static_field_by_name("java/io/File", "UNSAFE", Value::Object(Some(unsafe_obj)));
    let Some(file_class) = ctx.class_id_by_name("java/io/File") else {
        return Ok(());
    };
    let unsafe_pin = ctx.pin_native_root(unsafe_obj);
    let mirror = ctx.get_class_mirror(file_class);
    let mirror_pin = ctx.pin_native_root(mirror);
    let mut result = Ok(());
    for (static_name, field_name) in [
        ("PATH_OFFSET", "path"),
        ("PREFIX_LENGTH_OFFSET", "prefixLength"),
    ] {
        let name = ctx.create_string_uninterned(field_name);
        let unsafe_obj = ctx.read_native_pin(unsafe_pin, unsafe_obj);
        let mirror = ctx.read_native_pin(mirror_pin, mirror);
        match ctx.invoke_virtual(
            unsafe_obj,
            "objectFieldOffset",
            "(Ljava/lang/Class;Ljava/lang/String;)J",
            &[Value::Object(Some(mirror)), Value::Object(Some(name))],
        ) {
            Ok(Some(offset)) => ctx.set_static_field_by_name("java/io/File", static_name, offset),
            Ok(None) => {}
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(mirror_pin);
    ctx.unpin_native_roots(unsafe_pin);
    result
}

pub fn alloc_heap_bytebuffer(
    ctx: &mut dyn NativeContext,
    capacity: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, capacity);
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 5)?;
    let cap = capacity as i32;
    ctx.set_field(buf, 0, Value::Object(Some(arr)));
    ctx.set_field(buf, 1, Value::Int(0));
    ctx.set_field(buf, 2, Value::Int(cap));
    ctx.set_field(buf, 3, Value::Int(cap));
    ctx.set_field(buf, 4, Value::Int(-1));
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(buf, "offset", Value::Int(0));
    ctx.set_field_by_name(buf, "position", Value::Int(0));
    ctx.set_field_by_name(buf, "limit", Value::Int(cap));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    // Real HeapByteBuffer seeds Buffer.address to ARRAY_BYTE_BASE_OFFSET +
    // offset. Bulk get/put bytecode routes through ScopedMemoryAccess and
    // expects this base-offset-relative value when copying from/to hb.
    // Keep this last so synthetic indexed compatibility writes cannot clobber
    // the inherited real-JDK Buffer.address slot.
    ctx.set_field_by_name(buf, "address", Value::Long(16));
    Ok(buf)
}

pub fn native_heap_bytebuffer_allocate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let capacity = args.first().and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    // Reclaim-and-retry, not one shot. `ByteBuffer.allocate` is shadowed by
    // this native even in real-JDK mode, so the backing array does NOT go
    // through `newarray` — it never sees the try / force-a-GC / try /
    // last-ditch / try ladder that `gc_alloc_array` and `jit_newarray` run.
    // It got one attempt and reported `OutOfMemoryError`.
    //
    // H2 `TestBenchmark` at `-Xmx1g` on ZGC is what that costs: MVStore's
    // background writer grows a `WriteBuffer` to 10,616,832 bytes, the arena
    // has no hole that big *at that instant* (498 KiB largest, 348 MB free and
    // shredded across 30k spans), and the VM threw — with the heap 97% free
    // once the collection that was never asked for finally ran. Repeating the
    // identical `ByteBuffer.allocate` one Java statement later succeeded on the
    // first try; the same class passes under `--nojit`, at `-Xmx2g`, and on the
    // generational collector, which is the shape of a spurious refusal rather
    // than an exhausted heap.
    //
    // Calling `reclaim_before_alloc_retry` is legal HERE specifically: this is
    // the native's first allocation, so it holds no unpinned `ObjectRef` in a
    // Rust local for a collection to dangle or sweep. See the trait method for
    // why that precondition belongs to the caller. Everything allocated after
    // this point is pinned (`bytes_pin` below), so a later failure must not
    // reclaim — and does not.
    let elem = cratonvm_types::ArrayElementType::Byte;
    let bytes = match ctx.try_new_array(elem, capacity) {
        Some(b) => b,
        None => {
            let retry = ctx.reclaim_before_alloc_retry();
            let served = retry.then(|| ctx.try_new_array(elem, capacity)).flatten();
            // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
            let served = served.or_else(|| {
                (retry && ctx.reclaim_after_failed_alloc_retry())
                    .then(|| ctx.try_new_array(elem, capacity))
                    .flatten()
            });
            match served {
                Some(b) => b,
                None => {
                    return Err(RuntimeError::OutOfMemoryError {
                        message: format!("Java heap space (ByteBuffer.allocate {capacity})"),
                    }
                    .into())
                }
            }
        }
    };
    let bytes_pin = ctx.pin_native_root(bytes);
    let bytes = ctx.read_native_pin(bytes_pin, bytes);
    let result = native_byte_buffer_wrap_bytes_offset_len(
        ctx,
        &[
            Value::Object(Some(bytes)),
            Value::Int(0),
            Value::Int(capacity.min(i32::MAX as usize) as i32),
        ],
    );
    // A full JVM dispatches the HeapByteBuffer constructor above.  The
    // native-only allocation entry point is also used by minimal contexts,
    // which cannot run that Java body, so establish the same fields through
    // the constructor's native implementation.
    //
    // gc-common w7-b: re-read `bytes` from its pin first. The constructor
    // above is Java (`new_object_initialized` runs `HeapByteBuffer.<init>`),
    // and interpreted Java can collect; the pin is remapped, the local is
    // not. Passing the pre-constructor local stored a stale address into the
    // new buffer's `hb` whenever that constructor moved the array.
    let bytes = ctx.read_native_pin(bytes_pin, bytes);
    if let Ok(Some(Value::Object(Some(buffer)))) = &result {
        native_heap_byte_buffer_init_array_offset_len(
            ctx,
            &[
                Value::Object(Some(*buffer)),
                Value::Object(Some(bytes)),
                Value::Int(0),
                Value::Int(capacity.min(i32::MAX as usize) as i32),
                Value::Object(None),
            ],
        )?;
    }
    ctx.unpin_native_roots(bytes_pin);
    result
}

pub fn alloc_coding_error_action(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-c: the name first, pinned across the object's allocation
    // (allocated second, it could leave `action` stale).
    let name_obj = ctx.create_string(name);
    let name_pin = ctx.pin_native_root(name_obj);
    let action: ObjectRef =
        try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CodingErrorAction", 1)?;
    let name_obj: ObjectRef = ctx.read_native_pin(name_pin, name_obj);
    ctx.unpin_native_roots(name_pin);
    ctx.set_field_by_name(action, "name", Value::Object(Some(name_obj)));
    ctx.set_field(action, 0, Value::Object(Some(name_obj)));
    Ok(action)
}

pub fn native_coding_error_action_clinit(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    for name in ["IGNORE", "REPLACE", "REPORT"] {
        let action = alloc_coding_error_action(ctx, name);
        ctx.set_static_field_by_name(
            "java/nio/charset/CodingErrorAction",
            name,
            Value::Object(Some(action?)),
        );
    }
    Ok(None)
}

pub fn native_coding_error_action_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "name")))}

pub fn native_level_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    for (name, value) in [
        ("ALL", i32::MIN),
        ("SEVERE", 1000),
        ("WARNING", 900),
        ("INFO", 800),
        ("CONFIG", 700),
        ("FINE", 500),
        ("FINER", 400),
        ("FINEST", 300),
        ("OFF", i32::MAX),
    ] {
        let level = alloc_level(ctx, name, value);
        ctx.set_static_field_by_name("java/util/logging/Level", name, Value::Object(Some(level?)));
    }
    Ok(None)
}

// `java/util/logging/Logger` has exactly ONE slot map in this VM and it is
// `crate::logmanager::LOGGER_FIELD_*` (name 2, parent 8, level 12), matching
// the declaration at `class_manager.rs:14191`. A second, local
// `LOGGER_FIELD_NAME = 0` / `LOGGER_FIELD_LEVEL = 1` used to live here and was
// read by the four bodies below; it named `config: Logger$ConfigurationData`
// and `manager: LogManager`, so a name `String` went into the config slot and
// a `Level` into the manager slot. Nothing ever asserted, because `Logger` IS
// declared and `try_alloc_concurrent_synthetic`'s `num_fields.max(real)`
// clamped every 3-slot ask up to 13 — the clamp protects the heap, not the
// field's identity. Do not reintroduce a local Logger slot constant here —
// every Logger slot access in this file names the `logmanager` constants.
//
// `LEVEL_FIELD_*` below is a DIFFERENT class and stays: those are
// `java/util/logging/Level`'s own `name`/`value`, they match the real class,
// and `native_level_init` is registered by
// `register_essential_natives_with_shims` — live in every mode.
pub const LEVEL_FIELD_NAME: usize = 0;
pub const LEVEL_FIELD_VALUE: usize = 1;

pub fn alloc_level(
    ctx: &mut dyn NativeContext,
    name: &str,
    value: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-c: the name first, pinned across the object's allocation
    // (allocated second, it could leave `lvl` stale).
    let n = ctx.create_string(name);
    let n_pin = ctx.pin_native_root(n);
    let lvl: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/util/logging/Level", 2)?;
    let n: ObjectRef = ctx.read_native_pin(n_pin, n);
    ctx.unpin_native_roots(n_pin);
    ctx.set_field(lvl, LEVEL_FIELD_NAME, Value::Object(Some(n)));
    ctx.set_field(lvl, LEVEL_FIELD_VALUE, Value::Int(value));
    Ok(lvl)
}

pub fn native_level_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Int(0));
    ctx.set_field_by_name(this, "name", name);
    ctx.set_field_by_name(this, "value", value);
    // The synthetic java.util.logging.Level layout keeps inherited name/value
    // in the first two slots; retain a slot fallback for subclass bootstrap.
    ctx.set_field(this, LEVEL_FIELD_NAME, name);
    ctx.set_field(this, LEVEL_FIELD_VALUE, value);
    Ok(None)
}

/// `Logger.getLogger(String)`, delegating to the one JUL Logger producer.
///
/// Registered only from `logging_shims::register_logging_natives`, which is
/// shadowed one call later by `register_slf4j_natives` (`lib.rs:24012`) and
/// again by `logmanager::register_logmanager_natives` (`:24357`) — so this is
/// unreachable through the registry today. It is corrected rather than left
/// alone because "currently shadowed" is a registration-ORDER property, and the
/// map it used to mint (a 3-slot Logger with the name in `config` and an INFO
/// `Level` in `manager`) is one reorder away from being handed to readers that
/// index slots 2/8/12. See NOM F3-2 for the paired removal.
///
/// `get_or_create_logger` is also the only producer that caches by name,
/// links `parent`, and pins across the allocating steps.
pub fn native_logger_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    Ok(Some(Value::Object(Some(
        crate::logmanager::get_or_create_logger(ctx, &name)?,
    ))))
}

/// `Logger.getGlobal()` — `java.util.logging.Logger.GLOBAL_LOGGER_NAME` is
/// `"global"`, and the JDK returns the SAME object as
/// `Logger.getLogger("global")`. Going through `get_or_create_logger` is what
/// makes that identity hold here; minting a fresh Logger per call did not.
pub fn native_logger_get_global(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(
        crate::logmanager::get_or_create_logger(ctx, "global")?,
    ))))
}

/// `Logger.getName()`, third of the three registrations for this triple.
///
/// All three now delegate to `logmanager::jul_logger_name_object`, which is the
/// only code that knows where each of the VM's three JUL Logger layouts keeps
/// the name. Reading one slot here was right for exactly one of them, and which
/// one won was decided by registration order.
pub fn native_logger_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::logmanager::jul_logger_get_name_value(
        ctx, this,
    )))
}

/// `Logger.getLevel()` — reads the ONE level slot,
/// `logmanager::LOGGER_FIELD_LEVEL` (12), which is where `setLevel`
/// (`lib.rs:17613`), `allocate_logger` and `register_slf4j_natives` all write.
/// It used to read slot 1, i.e. `manager: LogManager`.
///
/// The width guard mirrors `logmanager::native_jul_logger_is_loggable`:
/// `Heap::get_field` asserts `index < num_slots`, and a Logger narrower than
/// the declaration can still arrive from a bytecode `new` in a mode that sizes
/// from a stub. Answering `null` there is the JDK's own "no explicit level".
pub fn native_logger_get_level(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ctx.object_num_fields(this) <= crate::logmanager::LOGGER_FIELD_LEVEL {
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(
        ctx.get_field(this, crate::logmanager::LOGGER_FIELD_LEVEL),
    ))
}

pub fn native_logger_log(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Print the log message
    jul_log_msg(ctx, args)
}

/// Log a message only if the logger's current level allows `method_level`.
///
/// Compares the logger's configured level — `logmanager::LOGGER_FIELD_LEVEL`
/// (12), the slot every writer in the tree uses — against the method's
/// inherent level (SEVERE=1000 .. FINEST=300). The doc line here used to say
/// "field 1", which was `manager: LogManager`: this read a `LogManager`
/// reference and asked it for `LEVEL_FIELD_VALUE`, so the threshold silently
/// stayed at the INFO default and `setLevel(SEVERE)` suppressed nothing.
///
/// The slot holds EITHER a `Level` reference or a raw `Int` (an older
/// `setLevel` stored the decoded int); both shapes are decoded, exactly as
/// `logmanager::native_jul_logger_is_loggable` does. `LEVEL_FIELD_VALUE` = 1 is
/// right for both `Level` layouts — the real class is `name` 0, `value` 1
/// (measured with `javap -p java.util.logging.Level` on JDK 25), and the VM's
/// 2-slot synthetic `Level` is a prefix of it.
pub fn native_logger_log_if(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method_level: i32,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `Heap::get_field` asserts `index < num_slots`; a Logger narrower than the
    // declaration keeps the JDK default instead of aborting the VM.
    let current = if ctx.object_num_fields(this) <= crate::logmanager::LOGGER_FIELD_LEVEL {
        800
    } else {
        match ctx.get_field(this, crate::logmanager::LOGGER_FIELD_LEVEL) {
            Value::Int(v) => v,
            Value::Object(Some(l)) if ctx.object_num_fields(l) > LEVEL_FIELD_VALUE => {
                ctx.get_field(l, LEVEL_FIELD_VALUE).as_int().unwrap_or(800)
            }
            _ => 800, // no explicit level — the JDK default is INFO
        }
    };
    if method_level < current {
        return Ok(None); // filter out — level is below logger threshold
    }
    jul_log_msg(ctx, args)
}

pub fn native_logger_log_level(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: this, level, message
    if let Some(Value::Object(Some(msg))) = args.get(2) {
        if let Some(s) = ctx.read_string(*msg) {
            ctx.record_printed_line(s);
        }
    }
    Ok(None)
}

pub fn native_level_all(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(
        ctx,
        "ALL",
        i32::MIN,
    )?))))
}
pub fn native_level_severe(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "SEVERE", 1000)?))))
}
pub fn native_level_warning(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "WARNING", 900)?))))
}
pub fn native_level_info(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "INFO", 800)?))))
}
pub fn native_level_config(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "CONFIG", 700)?))))
}
pub fn native_level_fine(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "FINE", 500)?))))
}
pub fn native_level_finer(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "FINER", 400)?))))
}
pub fn native_level_finest(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(ctx, "FINEST", 300)?))))
}
pub fn native_level_off(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(alloc_level(
        ctx,
        "OFF",
        i32::MAX,
    )?))))
}

pub fn native_level_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, LEVEL_FIELD_NAME)))}

pub fn native_level_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, LEVEL_FIELD_VALUE)))}

// ===========================================================================
// SLF4J Logging Facade — bridges to console output
// ===========================================================================

// SLF4J Logger: 2-field synthetic (name=0, level=1 where 0=TRACE..4=ERROR)
pub const SLF4J_NAME: usize = 0;
pub const SLF4J_LEVEL: usize = 1;

// MDC (Mapped Diagnostic Context) backed by thread-local storage
std::thread_local! {
    pub static MDC_MAP: std::cell::RefCell<std::collections::HashMap<String, String>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn register_locale_natives(_registry: &mut NativeMethodRegistry) {
    // synthetic-stub removed: java.util.Locale has real JDK bytecode.
    // KNOWN GAP: getLanguage()/getCountry() return empty under BOTH the
    // removed fake AND real bytecode (Locale baseLocale fields unpopulated) —
    // a pre-existing VM gap, flagged for follow-up, not a Wave-2 regression.
}

// NOTE: `java/util/Locale` is a real bootstrap class. Its instance slots
// 0/1/… are `baseLocale` / `localeExtensions` (typed objects), NOT
// String language/country/variant fields. The synthetic Locale's slots are
// therefore left at their zero-initialised (null) defaults — that is the
// spec-correct "no extensions" shape and prevents real-JDK Locale bytecode
// (e.g. `getUnicodeLocaleType`) from dispatching `invokevirtual` onto a
// String. All language/country/variant data lives in the `locale_data`
// side table instead. See `locale_data()` for the full rationale.

pub fn locale_alloc(
    ctx: &mut dyn NativeContext,
    lang: &str,
    country: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(locale_alloc_full(ctx, lang, "", country, "")?)
}

/// [`locale_alloc`] with the full subtag set, including the BCP-47 script.
pub fn locale_alloc_full(
    ctx: &mut dyn NativeContext,
    lang: &str,
    script: &str,
    country: &str,
    variant: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let loc = try_alloc_concurrent_synthetic(ctx, "java/util/Locale", 3)?;
    Ok(locale_populate_full(
        ctx, loc, lang, script, country, variant,
    ))
}

/// Record a synthetic Locale's `(language, country, variant)` in the side
/// table **and** populate its real-JDK `baseLocale` instance field with a
/// non-null `sun/util/locale/BaseLocale` object.
///
/// CRITICAL FIX: the side table alone is not enough. Real-JDK `Locale`
/// methods we do *not* natively override (e.g. `Locale.equals`,
/// `Locale.hashCode`) run as bytecode and dereference the private
/// `baseLocale` field — e.g. `Locale.equals` does
/// `baseLocale.equals(otherBase)`. If `baseLocale` is left null (the
/// previous side-table-only behaviour) that bytecode throws a
/// `NullPointerException` (Tomcat's `StringManager.<init>` ->
/// `bundle.getLocale().equals(Locale.ROOT)` tripped exactly this).
///
/// Populating `baseLocale` with a correctly-shaped `BaseLocale` makes
/// real-JDK `Locale.equals`/`hashCode`/`getLanguage`/`toString` and every
/// other un-overridden Locale method work directly. `localeExtensions` is
/// deliberately left null — that is the spec-correct "no extensions" shape
/// and `Locale.equals` handles a null `localeExtensions` on both sides.
pub fn locale_populate(
    ctx: &mut dyn NativeContext,
    loc: ObjectRef,
    lang: &str,
    country: &str,
    variant: &str,
) -> ObjectRef {
    locale_populate_full(ctx, loc, lang, "", country, variant)
}

/// [`locale_populate`] with the BCP-47 script subtag as well.
///
/// Returns `loc`'s CURRENT reference: the `BaseLocale` build below allocates
/// (five objects), so a moving young GC in the middle relocates `loc` out from
/// under the caller's raw `ObjectRef` — the Family-1 stale-native-local shape.
/// `loc` is rooted in a handle scope for the duration and read back at the end.
pub fn locale_populate_full(
    ctx: &mut dyn NativeContext,
    loc: ObjectRef,
    lang: &str,
    script: &str,
    country: &str,
    variant: &str,
) -> ObjectRef {
    locale_data_set_full(ctx.vm_identity(), loc, lang, script, country, variant);
    let mut scope = NativeHandleScope::new(ctx);
    let loc_h = scope.root(loc);
    // Build the real-JDK `sun.util.locale.BaseLocale` backing object.
    // BaseLocale's instance fields are `language`, `script`, `region`,
    // `variant` (all `String`) plus a lazily-computed `int hash`. We set
    // the four String fields by name; `hash` stays 0 (BaseLocale computes
    // it lazily). `BaseLocale.equals` compares the four Strings, so using
    // interned Strings (the default for `create_string`) keeps its
    // identity (`==`) comparisons correct across separately-built Locales.
    match scope.ensure_class_initialized("sun/util/locale/BaseLocale") {
        Ok(base_cid) => {
            let nfields = scope.class_num_total_fields(base_cid).max(5);
            let base = scope.alloc_object(base_cid, nfields);
            let base_h = scope.root(base);
            let lang_s = scope.create_string(lang);
            let lang_h = scope.root(lang_s);
            let script_s = scope.create_string(script);
            let script_h = scope.root(script_s);
            let region_s = scope.create_string(country);
            let region_h = scope.root(region_s);
            let variant_s = scope.create_string(variant);
            let base = scope.get(&base_h);
            let (lang_s, script_s, region_s) = (
                scope.get(&lang_h),
                scope.get(&script_h),
                scope.get(&region_h),
            );
            scope.set_field_by_name(base, "language", Value::Object(Some(lang_s)));
            scope.set_field_by_name(base, "script", Value::Object(Some(script_s)));
            scope.set_field_by_name(base, "region", Value::Object(Some(region_s)));
            scope.set_field_by_name(base, "variant", Value::Object(Some(variant_s)));
            let loc_now = scope.get(&loc_h);
            scope.set_field_by_name(loc_now, "baseLocale", Value::Object(Some(base)));
        }
        Err(e) => {
            // KNOWN GAP (2026-07-14): if `sun/util/locale/BaseLocale`'s own
            // `<clinit>` is touched too early in boot (before
            // `jdk/internal/misc/VM`'s saved-properties snapshot has
            // `java.home` populated), it fails with
            // `InternalError("null property: java.home")` and is
            // permanently marked `InitializationError` (JVMS §5.5) — every
            // later attempt (including this one) gets a cached
            // `NoClassDefFoundError` instead of a fresh error. `baseLocale`
            // is left null; any real-bytecode `Locale` method not natively
            // overridden here (`toString`, `equals`, `hashCode`, …) then
            // NPEs on `this.baseLocale`. See locale-real-jdk-bootstrap-noclassdeffounderror-FIXED.md.
            tracing::warn!(
                "locale_populate: could not initialize sun.util.locale.BaseLocale ({e:?}) — \
                 baseLocale left null on this Locale"
            );
        }
    }
    // `localeExtensions` is intentionally left null (the "no extensions"
    // shape that real-JDK `Locale.equals`/`hashCode` expect).
    scope.get(&loc_h)
}

/// Read a Locale arg's `(language, country, variant)` — side table first,
/// with no instance-field fallback (the slots are not String-typed).
pub fn locale_fields(_ctx: &dyn NativeContext, this: ObjectRef) -> (String, String, String) {
    locale_data_get(this)
}

pub fn native_locale_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let lang = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    locale_populate(ctx, this, &lang, "", "");
    Ok(None)
}

pub fn native_locale_init2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let lang = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let country = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    locale_populate(ctx, this, &lang, &country, "");
    Ok(None)
}

pub fn native_locale_get_default(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let loc = locale_alloc(ctx, "en", "US");
    Ok(Some(Value::Object(Some(loc?))))
}

pub fn native_locale_get_lang(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (lang, _, _) = locale_fields(ctx, this);
    Ok(Some(Value::Object(Some(ctx.create_string(&lang)))))
}

pub fn native_locale_get_country(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, country, _) = locale_fields(ctx, this);
    Ok(Some(Value::Object(Some(ctx.create_string(&country)))))
}

pub fn native_locale_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (lang, country, _) = locale_fields(ctx, this);
    let s = if country.is_empty() {
        lang
    } else {
        format!("{}_{}", lang, country)
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_locale_to_tag(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (lang, country, _) = locale_fields(ctx, this);
    let s = if country.is_empty() {
        lang
    } else {
        format!("{}-{}", lang, country)
    };
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_locale_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (a_lang, a_country, a_variant) = locale_fields(ctx, this);
    let (b_lang, b_country, b_variant) = locale_fields(ctx, other);
    Ok(Some(Value::Int(
        if a_lang == b_lang && a_country == b_country && a_variant == b_variant {
            1
        } else {
            0
        },
    )))
}

pub fn native_locale_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (lang, _, _) = locale_fields(ctx, this);
    let mut h: i32 = 0;
    for b in lang.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

pub fn native_locale_english(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(locale_alloc(ctx, "en", "")?))))
}
fn native_locale_us(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(locale_alloc(ctx, "en", "US")?))))
}
fn native_locale_uk(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(locale_alloc(ctx, "en", "GB")?))))
}
fn native_locale_root(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(locale_alloc(ctx, "", "")?))))
}

pub fn native_locale_for_tag(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let tag = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let parts: Vec<&str> = tag.split('-').collect();
    let lang = parts.first().copied().unwrap_or("");
    let country = parts.get(1).copied().unwrap_or("");
    let loc = locale_alloc(ctx, lang, country);
    Ok(Some(Value::Object(Some(loc?))))
}

// ===========================================================================
// Phase 33: java.security — MessageDigest, SecureRandom
// ===========================================================================

pub(crate) fn register_security_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: MessageDigest wraps host crypto; SecureRandom wraps OS
    // entropy — both are host-resource bridges → Bridge. (Random uses a
    // JDK-identical LCG, also routed through the securerandom module below.)
            let md = "java/security/MessageDigest";
    registry.register(
        md,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/MessageDigest;",
        native_md_get_instance, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(md, "update", "([B)V", native_md_update_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(md, "update", "(B)V", native_md_update_byte, cratonvm_native_api::NativeKind::Bridge);
    registry.register(md, "update", "([BII)V", native_md_update_bytes_off, cratonvm_native_api::NativeKind::Bridge);
    registry.register(md, "digest", "()[B", native_md_digest, cratonvm_native_api::NativeKind::Bridge);
    registry.register(md, "digest", "([B)[B", native_md_digest_input, cratonvm_native_api::NativeKind::Bridge);
    registry.register(md, "digest", "([BII)I", native_md_digest_buf, cratonvm_native_api::NativeKind::Bridge);
    // The real `MessageDigest.digest(byte[],int,int)` has a concrete body that
    // delegates to the SPI's `engineDigest()` / `engineDigest(byte[],int,int)`
    // — abstract on `MessageDigestSpi` (no Code attribute). For our synthetic
    // MessageDigest (which IS-A MessageDigestSpi, so `this` carries the
    // MD_FIELD_ALGO/DATA state), back those SPI hooks directly so the buffer
    // digest path resolves instead of hitting `AbstractMethodError:
    // MessageDigestSpi.engineDigest()[B has no Code attribute` (OffsetMapTest /
    // SkimpyOffsetMap). `native_md_digest`/`native_md_digest_buf` read the same
    // fields and so serve the SPI signatures unchanged.
    let mdspi = "java/security/MessageDigestSpi";
    registry.register(mdspi, "engineDigest", "()[B", native_md_digest, cratonvm_native_api::NativeKind::Bridge);
    registry.register(mdspi, "engineDigest", "([BII)I", native_md_digest_buf, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        mdspi,
        "engineGetDigestLength",
        "()I",
        native_md_get_digest_length, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(md, "reset", "()V", native_md_reset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        md,
        "getAlgorithm",
        "()Ljava/lang/String;",
        native_md_get_algorithm, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(md, "getDigestLength", "()I", native_md_get_digest_length, cratonvm_native_api::NativeKind::Bridge);

    // WP6.7 — `java.util.Random` MUST be deterministic when given a
    // seed (JDK spec).  The previous code aliased the next* methods
    // here to the OS-CSPRNG-backed `native_sr_next_*` helpers, which
    // produced non-deterministic output that broke the seed contract.
    // Both Random and SecureRandom are now registered through the
    // dedicated `securerandom` module, which uses an LCG identical to
    // the JDK's for `Random` and OS entropy for `SecureRandom`.
    crate::securerandom::register_random_and_securerandom_natives(registry);
    }

/// T2.3.5: `Random.nextFloat` — returns a float uniformly distributed
/// in [0.0, 1.0). Matches the JDK formula: take the top 24 bits of a
/// seed step and divide by `2^24`.
pub fn native_random_next_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let seed = sr_next_seed(ctx, this);
    let bits = ((seed >> 16) as u32) >> 8; // top 24 bits
    let f = (bits as f32) / ((1u32 << 24) as f32);
    Ok(Some(Value::Float(f)))
}

/// T2.3.5: `Random.nextGaussian` — returns a double sampled from the
/// standard normal distribution via the Marsaglia polar method, which
/// is what `java.util.Random` uses under the hood. Two uniform samples
/// from [-1, 1) are rejected until their squared norm `s` lies in
/// (0, 1); the two output samples are then `u * sqrt(-2 ln s / s)` and
/// `v * sqrt(-2 ln s / s)`. We generate one and discard the other —
/// the JDK caches the second but our `Random` is stateless-CSPRNG-backed
/// so there's nothing to cache.
pub fn native_random_next_gaussian(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    // Draw pairs until we get one inside the unit circle. Cap the
    // retry budget so a pathological PRNG cannot hang the VM —
    // the probability of rejection is ~(1 - π/4) per pair, so 64
    // retries gives < 2^-26 failure probability.
    for _ in 0..64 {
        // Two uniform draws in [-1, 1).
        let s1 = sr_next_seed(ctx, this);
        let u = (((s1 >> 16) as u32 as f64) / (u32::MAX as f64)) * 2.0 - 1.0;
        let s2 = sr_next_seed(ctx, this);
        let v = (((s2 >> 16) as u32 as f64) / (u32::MAX as f64)) * 2.0 - 1.0;
        let s = u * u + v * v;
        if s > 0.0 && s < 1.0 {
            // fdlibm, matching the other two copies — see the note in
            // `securerandom::native_random_next_gaussian`. This body is not
            // currently reachable from the registry (the `securerandom` module
            // supersedes it, per the comment above `register_random_and_
            // securerandom_natives` in this file), but it is still compiled and
            // still exercised by an in-crate test, and a superseded copy left on
            // `f64::ln` is exactly what a future re-registration would silently
            // reinstate. W7-54-strictmath-fdlibm-family-RETIRED-20260917.md.
            let mult = (-2.0 * cratonvm_types::fdlibm::log(s) / s).sqrt();
            return Ok(Some(Value::Double(u * mult)));
        }
    }
    // Fallback: return 0.0. With the CSPRNG backing, reaching this
    // point is vanishingly unlikely, but the VM stays responsive.
    Ok(Some(Value::Double(0.0)))
}

// MessageDigest = 2-field synthetic (algorithm=0 String, data=1 byte[] accumulator)
pub const MD_FIELD_ALGO: usize = 0;
pub const MD_FIELD_DATA: usize = 1;

pub fn native_md_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let algo = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    // This used to be an `if` with an EMPTY body and a comment explaining
    // that unknown algorithms "fall back to SHA-256 inside `compute_digest`,
    // matching the JDK behaviour of NoSuchAlgorithmException being surfaced
    // lazily". The JDK does not do that: `MessageDigest.getInstance` throws
    // at `getInstance`, measured on HotSpot 25 (`NO-SUCH-DIGEST MessageDigest
    // not available`, probes/JcaAdvertisedVsServedProbe.expected.txt §B). The
    // fallback meant a caller in synthetic mode that asked for a digest this
    // VM does not implement got SHA-256's bytes labelled with its own
    // algorithm name and no error anywhere on the path.
    //
    // Gate on the SAME predicate the real-JDK-mode `md_get_instance` uses
    // rather than a second hand-maintained literal list — that list had
    // already drifted (no SHA-224, no SHA-512/224, no SHA-512/256, no
    // SHA3-224, all of which `compute_digest` implements), so the check it
    // was not performing would have been wrong in the other direction too.
    // W7-63-jca-advertise-vs-serve.md.
    if !crate::jca::message_digest::algorithm_supported_public(&algo) {
        return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
            ctx,
            &format!("{algo} MessageDigest not available"),
        ));
    }
    let md = try_alloc_concurrent_synthetic(ctx, "java/security/MessageDigest", 2)?;
    // GC-safety: `md` is a bare Rust local held across two further
    // allocating calls (`create_string`, `new_array`) below. Either can
    // trigger a moving GC that relocates `md`; without re-reading through a
    // pin, the subsequent `set_field` writes land on `md`'s abandoned
    // from-space copy and are silently lost, leaving the live instance's
    // `algorithm`/`data` fields unset -- same unread-pin anti-pattern fixed
    // in `populate_real_thread_holder` (GCBARRIER-CDLWAIT-FIX, 2026-07-17).
    let md_handle = ctx.pin_native_root(md);
    let algo_str = ctx.create_string(&algo);
    let md = ctx.read_native_pin(md_handle, md);
    ctx.set_field(md, MD_FIELD_ALGO, Value::Object(Some(algo_str)));
    let data = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
    let md = ctx.read_native_pin(md_handle, md);
    ctx.set_field(md, MD_FIELD_DATA, Value::Object(Some(data)));
    ctx.unpin_native_roots(md_handle);
    Ok(Some(Value::Object(Some(md))))
}

pub fn md_append_bytes(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let this_handle = ctx.pin_native_root(this);
    let old_data = match ctx.get_field(this, MD_FIELD_DATA) {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(this_handle);
            return;
        }
    };
    let old_data_handle = ctx.pin_native_root(old_data);
    let old_len = ctx.array_length(old_data);
    let new_len = old_len + bytes.len();
    let new_data = ctx.new_array(cratonvm_types::ArrayElementType::Byte, new_len);
    // GC-safety re-read (mirrors `populate_real_thread_holder`'s
    // GCBARRIER-CDLWAIT-FIX, 2026-07-17): the `new_array` call above can
    // trigger a moving GC, relocating `this`/`old_data`, both captured as
    // bare locals before this allocation. Without this re-read, the copy
    // loop below reads through a stale `old_data` and the final `set_field`
    // writes `this.data` on an abandoned from-space copy -- a silently lost
    // update (this MessageDigest's accumulator never actually grows).
    let old_data = ctx.read_native_pin(old_data_handle, old_data);
    let this = ctx.read_native_pin(this_handle, this);
    for i in 0..old_len {
        let v = ctx.get_array_element(old_data, i);
        ctx.set_array_element(new_data, i, v);
    }
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(new_data, old_len + i, Value::Int(b as i8 as i32));
    }
    ctx.set_field(this, MD_FIELD_DATA, Value::Object(Some(new_data)));
    ctx.unpin_native_roots(this_handle);
}

pub fn native_md_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            bytes.push(b as u8);
        }
    }
    md_append_bytes(ctx, this, &bytes);
    Ok(None)
}

pub fn native_md_update_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    md_append_bytes(ctx, this, &[b]);
    Ok(None)
}

pub fn native_md_update_bytes_off(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
            bytes.push(b as u8);
        }
    }
    md_append_bytes(ctx, this, &bytes);
    Ok(None)
}

/// Compute a cryptographic digest using the specified algorithm.
///
/// Classical digests (MD2, MD5, SHA-1, SHA-2 family) are served by the
/// in-tree constant-time implementations (`real_md2` / `real_md5` /
/// `real_sha1` / `real_sha256` / `real_sha384` / `real_sha512`). The SHA-3
/// family and the two SHAKE XOFs are provided by the RustCrypto `sha3` crate
/// so CratonVM does not have to re-implement the Keccak permutation.
///
/// THE DEFAULT ARM FAILS. It used to be `Ok(real_sha256(data))`, which meant
/// an unrecognised algorithm name was silently served SHA-256's bytes under
/// its own name — the same species as the `Cipher` defect that served
/// ChaCha20 as AES-256-ECB, and the `Mac` defect that served every
/// unimplemented HMAC as HMAC-SHA-256. W7-63-jca-advertise-vs-serve.md.
/// `md_get_instance` gates on `algorithm_supported` first, so in the
/// real-JDK path the arm was unreachable; the synthetic-mode
/// `native_md_get_instance` accepted any name by design, so there it was
/// live. Both doors are now shut, and this one is the structural half: a
/// digest function that cannot name its algorithm must not return bytes.
pub fn compute_digest(algo: &str, data: &[u8]) -> Result<Vec<u8>, MethodCallFailed> {
    use sha3::Digest as _;
    // Strip both `-` and `/` so the truncated SHA-512 spellings
    // ("SHA-512/256", "SHA-512/224") normalise to "SHA512256" / "SHA512224"
    // — matching the alphanumeric-only normalisation used by
    // `algorithm_supported` / `digest_length_bytes`. It also collapses the
    // JDK's `SHAKE128-256` / `SHAKE256-512` to `SHAKE128256` / `SHAKE256512`,
    // which is what `algorithm_supported`'s alphanumeric-only filter gives
    // for the same two names — an agreement asserted by
    // `shake_normalisations_agree_across_the_two_filters` rather than assumed,
    // because it is a coincidence of these names and not a property of the
    // two functions.
    let upper = algo.to_uppercase().replace(['-', '/'], "");
    // `Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256` (and `SHAKE256`).
    // `jca::message_digest::canonical_algorithm` folds these before the
    // real-JDK path calls in; the synthetic-mode `native_md_digest` passes the
    // raw name, and its `getInstance` gate now ADMITS the aliases because it
    // shares `algorithm_supported_public`. Without this fold that door turns an
    // admitted name into an IllegalArgumentException at digest() instead of a
    // NoSuchAlgorithmException at getInstance. W7-63-jca-advertise-vs-serve.md.
    //
    // Written as `if`/`else` rather than `match upper.as_str() { _ => upper }`
    // on purpose: the match form moves `upper` out of an arm while the
    // scrutinee still holds a `&str` borrow of it, which is the E0505 shape.
    let upper = if upper == "SHAKE128" {
        "SHAKE128256".to_string()
    } else if upper == "SHAKE256" {
        "SHAKE256512".to_string()
    } else {
        upper
    };
    match upper.as_str() {
        "MD2" => Ok(real_md2(data)),
        "MD5" => Ok(real_md5(data)),
        "SHA1" | "SHA" => Ok(real_sha1(data)),
        "SHA224" => {
            // SHA-224: the hand-rolled `crypto_impl` SHA-256 code does not
            // cover the 224-bit variant, so use the `sha2` crate.
            let mut h = sha2::Sha224::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA256" => Ok(real_sha256(data)),
        "SHA384" => Ok(real_sha384(data)),
        "SHA512" => Ok(real_sha512(data)),
        "SHA512224" => {
            // SHA-512/224 (FIPS 180-4 §5.3.6): SHA-512 with the distinct
            // alternate IV derived from "SHA-512/224", truncated to 224 bits.
            // Distinct from SHA-224 and not a plain truncation of SHA-512.
            let mut h = sha2::Sha512_224::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA512256" => {
            // SHA-512/256 (FIPS 180-4 §5.3.6): SHA-512 with the distinct
            // alternate IV derived from "SHA-512/256", truncated to 256 bits.
            // Distinct from SHA-256 and not a plain truncation of SHA-512.
            let mut h = sha2::Sha512_256::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA3224" => {
            let mut h = sha3::Sha3_224::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA3256" => {
            let mut h = sha3::Sha3_256::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA3384" => {
            let mut h = sha3::Sha3_384::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        "SHA3512" => {
            let mut h = sha3::Sha3_512::new();
            h.update(data);
            Ok(h.finalize().to_vec())
        }
        // SHAKE128-256 / SHAKE256-512 (SUN, JDK 21+). These are the plain
        // SHAKE128 / SHAKE256 extendable-output functions read out to a FIXED
        // length — 256 and 512 bits, which is exactly what the suffix in the
        // JDK's algorithm name means. Verified against HotSpot 25 in
        // probes/JcaAdvertisedVsServedProbe.expected.txt, whose `""` rows are
        // also the published NIST XOF outputs, so the JDK names are not doing
        // anything exotic. `native-builtins-crypto/src/bc_newhope.rs` already
        // drives `sha3::Shake128` through this same XofReader API.
        //
        // `sha3::Digest` is imported at the top of this function as
        // `Digest as _`; the XOF traits are separate, and `Update` and
        // `Digest` both provide `update`, so importing both unqualified in
        // one scope is ambiguous. Keeping the `use` inside the arm is the
        // smaller change.
        "SHAKE128256" => {
            use sha3::digest::{ExtendableOutput, Update, XofReader};
            let mut xof = sha3::Shake128::default();
            xof.update(data);
            let mut out = vec![0u8; 32];
            xof.finalize_xof().read(&mut out);
            Ok(out)
        }
        "SHAKE256512" => {
            use sha3::digest::{ExtendableOutput, Update, XofReader};
            let mut xof = sha3::Shake256::default();
            xof.update(data);
            let mut out = vec![0u8; 64];
            xof.finalize_xof().read(&mut out);
            Ok(out)
        }
        // NOT `Ok(real_sha256(data))`. See the doc comment: serving SHA-256's
        // bytes under a name this VM does not implement is a wrong-algorithm
        // bug, not graceful degradation, and it is invisible to a caller
        // because the bytes look exactly like a working digest.
        other => Err(RuntimeError::IllegalArgumentException {
            message: format!("unsupported digest algorithm: {other}"),
        }
        .into()),
    }
}

// ===========================================================================
// Real MD2 implementation (RFC 1319)
// ===========================================================================

/// RFC 1319 §3.2 `PI_SUBST` — the 256-byte permutation derived from the
/// digits of pi. There is no shortcut form; the table IS the algorithm.
pub const MD2_PI: [u8; 256] = [
    41, 46, 67, 201, 162, 216, 124, 1, 61, 54, 84, 161, 236, 240, 6, 19, 98, 167, 5, 243, 192, 199,
    115, 140, 152, 147, 43, 217, 188, 76, 130, 202, 30, 155, 87, 60, 253, 212, 224, 22, 103, 66,
    111, 24, 138, 23, 229, 18, 190, 78, 196, 214, 218, 158, 222, 73, 160, 251, 245, 142, 187, 47,
    238, 122, 169, 104, 121, 145, 21, 178, 7, 63, 148, 194, 16, 137, 11, 34, 95, 33, 128, 127, 93,
    154, 90, 144, 50, 39, 53, 62, 204, 231, 191, 247, 151, 3, 255, 25, 48, 179, 72, 165, 181, 209,
    215, 94, 146, 42, 172, 86, 170, 198, 79, 184, 56, 210, 150, 164, 125, 182, 118, 252, 107, 226,
    156, 116, 4, 241, 69, 157, 112, 89, 100, 113, 135, 32, 134, 91, 207, 101, 230, 45, 168, 2, 27,
    96, 37, 173, 174, 176, 185, 246, 28, 70, 97, 105, 52, 64, 126, 15, 85, 71, 163, 35, 221, 81,
    175, 58, 195, 92, 249, 206, 186, 197, 234, 38, 44, 83, 13, 110, 133, 40, 132, 9, 211, 223, 205,
    244, 65, 129, 77, 82, 106, 220, 55, 200, 108, 193, 171, 250, 36, 225, 123, 8, 12, 189, 177, 74,
    120, 136, 149, 139, 227, 99, 232, 109, 233, 203, 213, 254, 59, 0, 29, 57, 242, 239, 183, 14,
    102, 88, 208, 228, 166, 119, 114, 248, 235, 117, 75, 10, 49, 68, 80, 180, 143, 237, 31, 26,
    219, 153, 141, 51, 159, 17, 131, 20,
];

/// MD2 (RFC 1319). HotSpot 25's `SUN` provider carries it — measured
/// `getDigestLength() == 16` — and `SunRsaSign` and `SunMSCAPI` both
/// advertise `MD2withRSA`, which resolves `MessageDigest.getInstance("MD2")`
/// internally. So this closes three advertisements, not one.
///
/// Before this landed, `jca::provider_chain::seed_direct_native_engine_services`
/// listed `MD2` in the `SUN` `MessageDigest` array — three lines above a
/// comment that declined to advertise SHAKE for exactly the reason MD2 was
/// being advertised anyway — and `MessageDigest.getInstance("MD2")` raised
/// `NoSuchAlgorithmException`. Advertise-but-refuse. The choice was implement
/// or de-advertise, and RFC 1319 is short, fully specified and cheap.
///
/// This transcription was adjudicated against HotSpot's own MD2 before it was
/// written, on ten messages covering all three padding cases (short, exactly
/// 16, and 17 bytes — an exact multiple takes a FULL 16-byte pad block, which
/// is the boundary every naive MD2 gets wrong). The vectors are pinned below
/// in `real_md2_matches_hotspot_vectors`; that test is what stands in for a
/// build this lane could not run.
///
/// MD2 is cryptographically broken (preimage and collision attacks are
/// published) and is present for parity with the platform JDK, not because
/// anything should use it. It is not reachable from any TLS or signing path
/// added here — the only new caller is `compute_digest`'s `"MD2"` arm.
pub fn real_md2(data: &[u8]) -> Vec<u8> {
    // §3.1 Padding: append between 1 and 16 bytes, each equal to the number
    // of bytes appended. A message that is already a multiple of 16 gets a
    // full extra block of 0x10 — never zero bytes.
    let pad = 16 - (data.len() % 16);
    let mut m = data.to_vec();
    m.extend(std::iter::repeat(pad as u8).take(pad));

    // §3.2 Checksum, computed over the PADDED message.
    let mut c = [0u8; 16];
    let mut l: u8 = 0;
    for block in m.chunks_exact(16) {
        for (j, &byte) in block.iter().enumerate() {
            c[j] ^= MD2_PI[(byte ^ l) as usize];
            l = c[j];
        }
    }

    // §3.3/§3.4 Compression: a 48-byte state, 18 rounds per block. The
    // checksum is fed through as one additional final block.
    let mut x = [0u8; 48];
    let blocks = m.len() / 16;
    for i in 0..=blocks {
        let block: &[u8] = if i == blocks {
            &c
        } else {
            &m[i * 16..i * 16 + 16]
        };
        for j in 0..16 {
            x[16 + j] = block[j];
            x[32 + j] = x[16 + j] ^ x[j];
        }
        let mut t: u8 = 0;
        for j in 0..18u8 {
            for k in 0..48 {
                x[k] ^= MD2_PI[t as usize];
                t = x[k];
            }
            // `t + j` mod 256. `j` never exceeds 17 so this cannot overflow in
            // practice, but the RFC's arithmetic IS mod 256 and `wrapping_add`
            // says so rather than relying on that.
            t = t.wrapping_add(j);
        }
    }

    x[..16].to_vec()
}

// ===========================================================================
// Real MD5 implementation (RFC 1321)
// ===========================================================================

/// Per-round shift amounts for MD5
pub const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// Pre-computed T[i] = floor(2^32 * abs(sin(i+1))) for MD5
pub const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

pub fn real_md5(data: &[u8]) -> Vec<u8> {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xefcdab89;
    let mut h2: u32 = 0x98badcfe;
    let mut h3: u32 = 0x10325476;

    // Pre-processing: pad message
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    // Process each 512-bit (64-byte) block
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                (a.wrapping_add(f).wrapping_add(MD5_K[i]).wrapping_add(m[g])).rotate_left(MD5_S[i]),
            );
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
    }

    let mut result = Vec::with_capacity(16);
    result.extend_from_slice(&h0.to_le_bytes());
    result.extend_from_slice(&h1.to_le_bytes());
    result.extend_from_slice(&h2.to_le_bytes());
    result.extend_from_slice(&h3.to_le_bytes());
    result
}

// ===========================================================================
// Real SHA-1 implementation (FIPS 180-4)
// ===========================================================================

pub fn real_sha1(data: &[u8]) -> Vec<u8> {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    // Pre-processing: pad message
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) block
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut result = Vec::with_capacity(20);
    result.extend_from_slice(&h0.to_be_bytes());
    result.extend_from_slice(&h1.to_be_bytes());
    result.extend_from_slice(&h2.to_be_bytes());
    result.extend_from_slice(&h3.to_be_bytes());
    result.extend_from_slice(&h4.to_be_bytes());
    result
}

// ===========================================================================
// Real SHA-256 implementation (FIPS 180-4)
// ===========================================================================

/// SHA-256 round constants: first 32 bits of the fractional parts of the
/// cube roots of the first 64 primes (2..311).
pub const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub fn real_sha256(data: &[u8]) -> Vec<u8> {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: pad message
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) block
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = Vec::with_capacity(32);
    for word in &h {
        result.extend_from_slice(&word.to_be_bytes());
    }
    result
}

/// Compute HMAC-SHA-256: HMAC(key, data) = SHA256((key' XOR opad) || SHA256((key' XOR ipad) || data))
/// where key' is the key padded/hashed to 64 bytes, ipad = 0x36, opad = 0x5c
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    const BLOCK_SIZE: usize = 64;

    // Step 1: If key > block size, hash it; if shorter, pad with zeros
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = real_sha256(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    // Step 2: Create inner and outer padded keys
    let mut i_key_pad = [0u8; BLOCK_SIZE];
    let mut o_key_pad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        i_key_pad[i] = key_block[i] ^ 0x36;
        o_key_pad[i] = key_block[i] ^ 0x5c;
    }

    // Step 3: inner hash = SHA256(i_key_pad || data)
    let mut inner_msg = Vec::with_capacity(BLOCK_SIZE + data.len());
    inner_msg.extend_from_slice(&i_key_pad);
    inner_msg.extend_from_slice(data);
    let inner_hash = real_sha256(&inner_msg);

    // Step 4: outer hash = SHA256(o_key_pad || inner_hash)
    let mut outer_msg = Vec::with_capacity(BLOCK_SIZE + 32);
    outer_msg.extend_from_slice(&o_key_pad);
    outer_msg.extend_from_slice(&inner_hash);
    real_sha256(&outer_msg)
}

// ===========================================================================
// Real SHA-512 implementation (FIPS 180-4) — 64-bit words, 80 rounds
// ===========================================================================

/// SHA-512 round constants: first 64 bits of the fractional parts of the
/// cube roots of the first 80 primes (2..409).
pub const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

pub fn real_sha512(data: &[u8]) -> Vec<u8> {
    let mut h: [u64; 8] = [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];

    // Pre-processing: pad message to 1024-bit blocks
    let bit_len = (data.len() as u128).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 1024-bit (128-byte) block
    for chunk in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            w[i] = u64::from_be_bytes([
                chunk[8 * i],
                chunk[8 * i + 1],
                chunk[8 * i + 2],
                chunk[8 * i + 3],
                chunk[8 * i + 4],
                chunk[8 * i + 5],
                chunk[8 * i + 6],
                chunk[8 * i + 7],
            ]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = Vec::with_capacity(64);
    for word in &h {
        result.extend_from_slice(&word.to_be_bytes());
    }
    result
}

/// SHA-384 is SHA-512 with different initial values, truncated to 384 bits (48 bytes).
pub fn real_sha384(data: &[u8]) -> Vec<u8> {
    let mut h: [u64; 8] = [
        0xcbbb9d5dc1059ed8,
        0x629a292a367cd507,
        0x9159015a3070dd17,
        0x152fecd8f70e5939,
        0x67332667ffc00b31,
        0x8eb44a8768581511,
        0xdb0c2e0d64f98fa7,
        0x47b5481dbefa4fa4,
    ];

    // Same padding as SHA-512: 1024-bit blocks
    let bit_len = (data.len() as u128).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            w[i] = u64::from_be_bytes([
                chunk[8 * i],
                chunk[8 * i + 1],
                chunk[8 * i + 2],
                chunk[8 * i + 3],
                chunk[8 * i + 4],
                chunk[8 * i + 5],
                chunk[8 * i + 6],
                chunk[8 * i + 7],
            ]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    // Truncate to 384 bits (6 words = 48 bytes)
    let mut result = Vec::with_capacity(48);
    for word in &h[..6] {
        result.extend_from_slice(&word.to_be_bytes());
    }
    result
}

// ===========================================================================
// Generic HMAC construction (RFC 2104)
// ===========================================================================

/// Generic HMAC: HMAC(K, data) = H((K' ⊕ opad) || H((K' ⊕ ipad) || data))
/// `hash_fn` is the underlying hash, `block_size` is the hash's internal block size.
pub fn hmac_generic(
    key: &[u8],
    data: &[u8],
    hash_fn: fn(&[u8]) -> Vec<u8>,
    block_size: usize,
) -> Vec<u8> {
    // Step 1: If key > block size, hash it; if shorter, pad with zeros
    let mut key_block = vec![0u8; block_size];
    if key.len() > block_size {
        let hashed = hash_fn(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    // Step 2: Create inner and outer padded keys
    let mut i_key_pad = vec![0u8; block_size];
    let mut o_key_pad = vec![0u8; block_size];
    for i in 0..block_size {
        i_key_pad[i] = key_block[i] ^ 0x36;
        o_key_pad[i] = key_block[i] ^ 0x5c;
    }

    // Step 3: inner hash = H(i_key_pad || data)
    let mut inner_msg = Vec::with_capacity(block_size + data.len());
    inner_msg.extend_from_slice(&i_key_pad);
    inner_msg.extend_from_slice(data);
    let inner_hash = hash_fn(&inner_msg);

    // Step 4: outer hash = H(o_key_pad || inner_hash)
    let mut outer_msg = Vec::with_capacity(block_size + inner_hash.len());
    outer_msg.extend_from_slice(&o_key_pad);
    outer_msg.extend_from_slice(&inner_hash);
    hash_fn(&outer_msg)
}

/// HMAC-SHA-384 (128-byte block size, same as SHA-512)
pub fn hmac_sha384(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac_generic(key, data, real_sha384, 128)
}

/// HMAC-SHA-512 (128-byte block size)
pub fn hmac_sha512(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac_generic(key, data, real_sha512, 128)
}

/// HMAC-SHA-1 (64-byte block size)
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac_generic(key, data, real_sha1, 64)
}

/// HMAC-MD5 (64-byte block size)
pub fn hmac_md5(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac_generic(key, data, real_md5, 64)
}

pub fn native_md_digest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let algo = match ctx.get_field(this, MD_FIELD_ALGO) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let data_arr = match ctx.get_field(this, MD_FIELD_DATA) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(data_arr);
    let mut data = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(data_arr, i) {
            data.push(b as u8);
        }
    }
    let digest = compute_digest(&algo, &data)?;
    // GC-safety: `this` is a bare Rust local (from `args.first()`) held
    // across two further allocating calls below (`new_array` x2). Either
    // can trigger a moving GC that relocates `this`; the final "reset"
    // `set_field` must re-read through a pin or it silently lands on an
    // abandoned from-space copy, leaving the live instance's accumulator
    // un-reset -- same unread-pin anti-pattern fixed in
    // `populate_real_thread_holder` (GCBARRIER-CDLWAIT-FIX, 2026-07-17).
    let this_handle = ctx.pin_native_root(this);
    let result = ctx.new_array(cratonvm_types::ArrayElementType::Byte, digest.len());
    let result_handle = ctx.pin_native_root(result);
    for (i, &b) in digest.iter().enumerate() {
        ctx.set_array_element(result, i, Value::Int(b as i8 as i32));
    }
    // Reset
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
    let this = ctx.read_native_pin(this_handle, this);
    let result = ctx.read_native_pin(result_handle, result);
    ctx.set_field(this, MD_FIELD_DATA, Value::Object(Some(empty)));
    ctx.unpin_native_roots(this_handle);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_md_digest_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // update with input first
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let len = ctx.array_length(*arr);
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(*arr, i) {
                bytes.push(b as u8);
            }
        }
        md_append_bytes(ctx, this, &bytes);
    }
    native_md_digest(ctx, args)
}

/// `int MessageDigest.digest(byte[] buf, int offset, int len)` — completes the
/// digest into the caller's buffer and returns the number of bytes written.
/// Without this native the call ran the real JDK bytecode, which dispatches to
/// the abstract `MessageDigestSpi.engineDigest()` (no Code attribute) and threw
/// `AbstractMethodError` — e.g. Kafka's `SkimpyOffsetMap.hashInto` uses this
/// overload (OffsetMapTest), not the `digest()[B` form the other natives cover.
pub fn native_md_digest_buf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let offset = match args.get(2) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let algo = match ctx.get_field(this, MD_FIELD_ALGO) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let data = match ctx.get_field(this, MD_FIELD_DATA) {
        Value::Object(Some(arr)) => {
            let n = ctx.array_length(arr);
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    v.push(b as u8);                }            }
            v        }
        _ => Vec::new(),
    };
    let digest = compute_digest(&algo, &data)?;
    // JDK writes the full digest (callers size `len` to the digest length); we
    // additionally clamp to both `len` and the array bounds to fail safe.
    let cap = ctx.array_length(buf);
    let n = digest.len().min(len).min(cap.saturating_sub(offset));
    for i in 0..n {
        ctx.set_array_element(buf, offset + i, Value::Int(digest[i] as i8 as i32));
    }
    // Reset the running state, matching `digest()`'s post-finalization reset.
    md_clear_data(ctx, this);
    Ok(Some(Value::Int(digest.len() as i32)))
}

/// Replace the digest's buffered input with an empty array.
///
/// gc-common w20-c: the array's allocation can collect and move the digest,
/// which is written after it; re-read it from a pin (both callers wrote the
/// old address).
pub fn md_clear_data(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let this_pin = ctx.pin_native_root(this);
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, MD_FIELD_DATA, Value::Object(Some(empty)));
}

pub fn native_md_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    md_clear_data(ctx, this);
    Ok(None)
}

pub fn native_md_get_algorithm(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, MD_FIELD_ALGO)))}

pub fn native_md_get_digest_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let algo = match ctx.get_field(this, MD_FIELD_ALGO) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // A THIRD hand-maintained digest-length table stood here, and it had
    // drifted furthest of the three: no MD2, no SHA-224, no SHA-512/224, no
    // SHA-512/256, no SHA3-224, and — because it only stripped `-` and not `/`
    // — `SHA-512/256` fell through the `_ => 32` default and reported 32 by
    // accident rather than by arm. Every one of those `compute_digest`
    // implements, so `getDigestLength()` was contradicting `digest()` on five
    // algorithms in synthetic mode.
    //
    // Adding MD2 and the SHAKE digests would have made that worse rather than
    // better: `getInstance` now admits them, so MD2 would have reported 32
    // where the digest is 16 and SHAKE256-512 would have reported 32 where the
    // digest is 64. One predicate, one length table, every door —
    // W7-63-jca-advertise-vs-serve.md.
    //
    // `None` is unreachable through either `getInstance` (both gate on
    // `algorithm_supported` first) and reports 0 rather than a plausible 32,
    // because a corroborating-but-wrong length is what made the SHA-256
    // fallback invisible in the first place.
    let len = crate::jca::message_digest::digest_length_bytes_public(&algo).unwrap_or(0);
    Ok(Some(Value::Int(len as i32)))
}

// SecureRandom — uses OS cryptographic entropy (BCryptGenRandom / /dev/urandom)
// for all random generation. The previous splitmix64 PRNG has been removed from
// all SecureRandom paths to prevent predictable output.

/// Fill a buffer with OS cryptographic entropy.
/// Mirrors crypto_impl::os_random_bytes but inlined here to avoid
/// feature-gate dependency on legacy-synthetic-crypto.
pub fn sr_os_random_bytes(buf: &mut [u8]) -> bool {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "advapi32")]
        extern "system" {
            #[link_name = "SystemFunction036"]
            fn RtlGenRandom(buf: *mut u8, len: u32) -> u8;
        }
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            let ok = unsafe { RtlGenRandom(chunk.as_mut_ptr(), chunk.len() as u32) };
            if ok == 0 {
                return false;
            }
        }
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            return f.read_exact(buf).is_ok();
        }
        false
    }
}

/// Get a cryptographically random u64 from OS entropy.
pub fn sr_os_random_u64() -> u64 {
    let mut buf = [0u8; 8];
    if !sr_os_random_bytes(&mut buf) {
        // Fallback: system_entropy_seed uses RandomState which itself uses OS entropy
        return system_entropy_seed();
    }
    u64::from_le_bytes(buf)
}

/// Gather entropy from multiple system sources and mix them into a seed.
/// Used only as a last-resort fallback when direct OS entropy fails.
pub fn system_entropy_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    h.write_u64(nanos);
    let tid = format!("{:?}", std::thread::current().id());
    h.write(tid.as_bytes());
    let s2 = RandomState::new();
    let h2 = s2.build_hasher();
    h.write_u64(h2.finish());
    h.finish()
}

pub fn native_secure_random_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let seed = sr_os_random_u64() as i64;
    ctx.set_field(this, 0, Value::Long(seed));
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

pub fn sr_next_seed(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    // Use OS entropy directly for each call (CSPRNG).
    // The object's seed field is updated for compatibility but the output
    // comes from the OS, not from a deterministic PRNG.
    let val = sr_os_random_u64();
    ctx.set_field(this, 0, Value::Long(val as i64));
    val as i64
}

// ---------------------------------------------------------------------------
// StampedLock — real implementation backed by `stamped_lock` module.
// ---------------------------------------------------------------------------
//
// WP4.7: The earlier version of this code did `monitor_enter; monitor_wait(5);
// monitor_exit` between observing the state and parking, so a notification
// arriving in that gap was lost — writers blocked for the full 5 ms even
// when contention had already cleared. The fix is to hold a real
// `parking_lot::Mutex` across the check-and-park so `Condvar::wait`
// releases it atomically. All of that lives in `crate::stamped_lock` so
// it can be unit-tested independently of the NativeContext plumbing.
//
// State encoding (see `stamped_lock` module for the full rationale):
//   stamp: i64   — even = no writer, odd = writer active. Starts at 256.
//   readers: i32 — number of active read locks.

pub const STAMPED_ORIGIN: i64 = crate::stamped_lock::STAMPED_ORIGIN;

// ---------------------------------------------------------------------------
// GC-stable lock-identity key (Pattern-A fix, bug nb-lib-gckeys §1)
//
// The StampedLock / ReentrantReadWriteLock state tables in `stamped_lock`
// are keyed by a `usize` derived from the Java lock object. The previous
// derivation — `o.as_ptr() as usize` — is NOT stable under a moving GC:
// a thread that locks before a young-gen collection computes one key, the
// object is relocated, and the matching `unlock` (after the GC) recomputes
// a DIFFERENT key from the new address → it misses the slot it locked,
// leaving the original slot permanently write-/read-held (deadlock) or
// corrupting an unrelated slot that happens to land at the freed address.
//
// Fix: derive the key from a GC-stable identity. `ctx.identity_hash_code`
// survives relocation (the header hash is preserved/remapped by the
// collector), and a generation registry tells apart the objects that share
// one 32-bit identity hash: every object is matched by its CURRENT address
// (step 1 below), and any other address mints its own generation. Such
// collisions are not rare: every backend mints identity hashes from one
// `AtomicI32` counter per heap, which a busy server wraps in minutes
// (gc-common w25-a, `common-w24a-lock-key-registry-merges-live-identity-hash-colliders`)
// — the scheme `native-collections`' `widened_obj_key` uses too (and the one
// `properties_sidetable::key_for` / `phases_early::pbkdf2_key_for` key
// through since gc-common w15-b). Routing both `stamped_addr`
// and `rwl_parent_addr` through this single helper keeps the lock and
// unlock paths agreeing on a key that no GC can shift.
//
// gc-common w9-b (`common-w8a-identity-hash-keyed-registries-are-shared-across-vms`):
// every entry records the VM that minted it, and the lookup considers only
// the CALLING VM's entries. Identity hashes are numbered from the same
// seed in every heap, so the Nth lock hashed in VM A and the Nth in VM B used
// to share a bucket; the "lone occupant moved" rebind then handed B's lock
// A's key outright, and the two VMs' `StampedLock` / RRWL states (and the
// `SSLContext` tables `t27_tls::ctx_obj_key` keys the same way) became one.
// Generations come from a counter that never goes backwards, so a key is
// unique across VMs too (the `stamped_lock` tables are keyed by the packed key
// alone) and is never re-minted after a slot is dropped.
//
// gc-common w14-c (`common-w13a-lock-key-registry-keeps-a-slot-per-object-ever-keyed`):
// the registry is weak. It used to keep a slot for every object it ever keyed
// until VM teardown. Now:
//
// * after every collection [`gc_sweep_and_remap_lock_keys`] (the stop-the-world
//   epilogue) or [`gc_sweep_lock_keys`] / [`gc_drop_lock_keys_in_spans`] (the
//   two out-of-pause reclamation hooks: the in-place verdict, and each
//   concurrent-sweep slice's freed spans) judges each of the collecting VM's
//   slots by its object, the way
//   `gc_sweep_locale_rows` / `gc_sweep_tls_rows` judge their rows, and the
//   epilogue re-addresses the survivors from the pointer map, so step 1 of
//   [`gc_stable_lock_key`] keeps hitting after a move (hash 0 included);
// * a dead object's slot is DROPPED when it was minted by
//   [`gc_stable_weak_lock_key`] (nothing outside the object's lifetime reads
//   its key through [`lock_key_owner_vm`]), and becomes a TOMBSTONE when it
//   was minted by [`gc_stable_lock_key`]. A tombstone (key -> VM, outside the
//   buckets, so no sweep walks it) keeps answering [`lock_key_owner_vm`] --
//   the `t27_tls` `SSLContext` rows outlive their context while an engine
//   still names its key, and an ownerless key reads as every VM's there --
//   but it is never matched or rebound to another object.
//   [`release_lock_key`] drops it once nothing names the key;
// * the generation counter is ONE counter for the whole registry rather than
//   one per hash, so a bucket whose last slot went can be dropped too without
//   ever handing out one of its keys again (identity hashes are numbered
//   sequentially, so a per-hash counter kept one bucket per object ever keyed).
//
// The default, [`gc_stable_lock_key`], is the conservative one: its callers
// that do not need a tombstone (`util_concurrent_ext`'s lock state tables)
// switch to [`gc_stable_weak_lock_key`], and `t27_tls` calls
// [`release_lock_key`] when an `SSLContext`'s rows go, per
// `docs/feature-designs/native-dispatch-architecture.md`.
//
// gc-common w25-a (`common-w24a-lock-key-registry-merges-live-identity-hash-colliders`):
// a lookup matches a slot by `(vm, current address)` ONLY. The old step 2
// ("the lone slot of this hash at another address is the same object, moved")
// answered the occupant's key for ANY other object with its hash, so two live
// colliders shared every lock state and side-table row filed under one key,
// and a newcomer keyed before the sweep took a dead object's key, which that
// sweep then freed under it. The step had no job left: no backend lets a
// native see an object at its new address before the stop-the-world epilogue
// re-addresses its slot (`gc_and_alloc::process_references_after_gc` calls
// [`gc_sweep_and_remap_lock_keys`] before any mutator resumes; ZGC relocates
// only in that pause -- its load barrier is never armed outside unit tests
// --, G1's concurrent phase only marks, and the Generational concurrent
// old-gen cycle is mark-sweep, whose hooks call [`gc_sweep_lock_keys`] /
// [`gc_drop_lock_keys_in_spans`] with nothing moved). So the step served only a caller holding a STALE copy of a
// moved object's address, a bug the stale-handle / stale-receiver audits gate;
// and even there it guessed, because `identity_hash_code` reads the header AT
// the stale address, which by then may be another object's. A stale lookup
// now mints a key of its own at the vacated address (a miss, never another
// object's rows), and the next sweep drops it (a held one: tombstones it).
pub struct LockKeyEntry {
    pub vm: usize,
    /// The object's current address. Re-addressed after every moving
    /// collection by [`gc_sweep_and_remap_lock_keys`].
    pub last_ptr: usize,
    pub generation: u32,
    /// Keep the key as a tombstone when the object dies, until
    /// [`release_lock_key`] or [`forget_vm_lock_keys`]. Set by every
    /// [`gc_stable_lock_key`] call on the slot (sticky).
    pub held: bool,
}

#[derive(Default)]
pub struct LockKeyRegistry {
    /// The next generation to mint, for every hash. Monotonic, so no dropped
    /// slot's key is ever handed out again: `stamped_lock` may still hold
    /// state under it. gcg g3/k: one per SHARD under
    /// `CRATONVM_LOCK_KEY_SHARDS` -- still unique, because a hash lives in
    /// exactly one shard (see `lock_key_shards`).
    pub next_generation: u32,
    /// Identity hash -> the LIVE slots minted for objects with that hash. A
    /// bucket is removed when its last slot goes.
    pub buckets: rustc_hash::FxHashMap<u32, Vec<LockKeyEntry>>,
    /// Packed key -> minting VM, for a held key whose object died.
    pub tombstones: rustc_hash::FxHashMap<u64, usize>,
    /// `(last_ptr, vm, hash)` of every slot in `buckets`, address-ordered:
    /// a freed-span drop reads `range(span)` instead of walking every slot
    /// (`common-w26c-proposal-sub-linear-freed-span-drops`, item 1). Kept in
    /// step by the mint, every removal and the post-collection re-address; the
    /// drop re-checks each hit against its bucket, so a stale entry is never
    /// acted on.
    pub by_addr: std::collections::BTreeSet<(usize, usize, u32)>,
}

#[inline]
pub fn pack_lock_key(hash: u32, generation: u32) -> usize {
    ((hash as usize) << 32) | (generation as usize)
}

/// Map a Java lock object (or the parent RWL of a read/write view) to a
/// The slot of `java.lang.Class.module` in the CALLING VM, or `None` when its
/// `Class` declares no such field. A slot is a fact about one VM's layout, so
/// the memo is keyed by `vm_identity`; it is thread-local because the reader
/// (`Class.getModule`) is hot and a `Cell` costs no lock.
pub fn class_module_slot(ctx: &dyn NativeContext) -> Option<usize> {
    thread_local! {
        static CLASS_MODULE_SLOT: std::cell::Cell<Option<(usize, Option<usize>)>> =
            const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    if let Some((cached_vm, slot)) = CLASS_MODULE_SLOT.with(|c| c.get()) {
        if cached_vm == vm {
            return slot;
        }
    }
    let slot = ctx.resolve_field_index("java/lang/Class", "module");
    CLASS_MODULE_SLOT.with(|c| c.set(Some((vm, slot))));
    slot
}


/// GC-stable `usize` key for the `stamped_lock` state tables. See the
/// module-level note above for the moving-GC deadlock this prevents. The
/// algorithm, over the CALLING VM's LIVE entries of this hash only (a
/// tombstone is never matched or rebound):
///   1. same object seen again at the same address → reuse its generation
///      (since gc-common w14-c the address is re-addressed after every moving
///      collection, so this is the path a moved object takes too);
///   2. otherwise a new object, or another object with the same identity hash
///      → fresh generation. There is no "lone occupant moved" step any more
///      (gc-common w25-a): it merged live colliders, see the module note.
///
/// The slot is HELD: kept as a tombstone after its object dies, so
/// [`lock_key_owner_vm`] still answers, until [`release_lock_key`]. A caller
/// whose state never outlives the object uses [`gc_stable_weak_lock_key`].
pub fn gc_stable_lock_key(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    lock_key_for(ctx, obj, true)
}

/// [`gc_stable_lock_key`] for a caller that reads nothing under the key once
/// its object is dead (the `stamped_lock` / RRWL state tables): the
/// slot is dropped by the first sweep that finds the object dead, and the key
/// is never minted again. A slot [`gc_stable_lock_key`] also keyed stays held.
///
/// Takes `&dyn NativeContext` (it needs only the VM identity and the identity
/// hash), so a `&mut` caller passes its `ctx` unchanged and a `&dyn` one can
/// use it too.
pub fn gc_stable_weak_lock_key(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    lock_key_for(ctx, obj, false)
}

/// The key [`gc_stable_weak_lock_key`] would answer for `obj`, or `None` when
/// the calling VM has no slot it would match -- WITHOUT minting one. Same
/// match as [`lock_key_for`] (the calling VM's slot at `obj`'s current
/// address), and it never touches `held`.
///
/// For the read and remove paths of a side table keyed by weak lock keys
/// (gc-common w20-c, the `lib.rs` identity-hash tables): an object that was
/// never keyed has no row, so a lookup need not leave a slot behind for every
/// `MessageBytes` recycled or every logger asked `getUseParentHandlers()`.
pub fn existing_weak_lock_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let reg = lock_key_shards::lock_key_shard(hash).lock();
    // gc-common w25-a: no "lone occupant" fallback -- another object with
    // this hash has no row of `obj`'s (see the module note).
    reg.buckets
        .get(&hash)?
        .iter()
        .find(|s| s.vm == vm && s.last_ptr == ptr)
        .map(|slot| pack_lock_key(hash, slot.generation))
}

