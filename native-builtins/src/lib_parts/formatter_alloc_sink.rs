#[allow(unused_imports)]
use super::super::*;

// RETIRED 2026-08-21 (H22-1) — `fn register_hex_format_real_jdk_natives`
// stood here (293 lines). It registered 16 natives on `java/util/HexFormat`
// whose bodies built their answer with `{:02x}` and never read the RECEIVER
// — and MEASURED from `--dump-native-registry --explain-jdk-only`, **all 16
// carried `owns_slot: false`**: every one lost its slot to the corrected twin
// this function itself called LAST, so not a single body here could ever run.
// The function's only live effect was that tail call.
//
// That is why the two had to move together. `phases_late::register_p64_hex_format`
// owns all 26 reachable `java/util/HexFormat` slots; retiring it alone would
// have PROMOTED these 16 dead ones and looked like "no effect" (`H14-1` §5's
// trap). Deleting this function is what makes the twin's retirement real, and
// deleting the twin's ONLY shipping call site — which lived here — is what
// leaves `HexFormat` to real JDK bytecode. `of()` also fabricated its receiver
// with `try_alloc_concurrent_synthetic("java/util/HexFormat", 4)`, so this
// removes an untyped-allocation site as well.
//
// MEASURED 2026-08-21: 30 HexFormat assertions byte-identical to HotSpot
// 25.0.3+9 with the class armed; `H14-3` §1 row 5 priced the corpus at a
// net cost of zero vectors. docs/known-issues/jdk-only/H22-1-*.md

/// `java.lang.String.format` / `java.util.Formatter` natives required in
/// real-JDK mode.  The JDK 25 bytecode for `String.format(String, Object...)`
/// and `Formatter.format(String, Object...)` produces ` 0:0:` or empty
/// output in our environment because the `Formatter` static-init chain
/// (`sun.util.locale.provider.LocaleProviderAdapter`,
/// `Locale.ENGLISH`, etc.) isn't fully wired — same root cause family as
/// the recently-fixed `HexFormat.formatHex` and `MessageDigest`.
///
/// Symptom this fixes: `FormatProbe.main` printed `got=[ 0:0:]` (or
/// empty) for `String.format("%02x:%d:%s", 255, 42, "hi")` even though
/// the format string and varargs were correct.  Mirroring
/// `register_hex_format_real_jdk_natives` (called unconditionally from
/// `register_essential_natives`), this delegates to the existing
/// `lang_string::native_string_format` implementation which already
/// supports `%s`, `%d`, `%x`, `%X`, `%02x`, `%-10s`, `%5d`, `%n`, `%%`,
/// `%f`, `%c`, `%b`, `%e`, `%E` with flags, width, and precision.
/// Build the sink a `Formatter` constructed without an `Appendable` writes to.
///
/// The real `Formatter(Locale l, Appendable a)` is
/// `this.a = (a == null) ? new StringBuilder() : a`, so BOTH the no-`Appendable`
/// constructors and an explicit `new Formatter((Appendable) null)` end up with a
/// `StringBuilder` — MEASURED on HotSpot 25.0.3+9, all four of
/// `new Formatter()`, `new Formatter(Locale.ROOT)`, `new Formatter((Locale) null)`
/// and `new Formatter((Appendable) null)` answer
/// `out().getClass() == java.lang.StringBuilder`.
///
/// This used to write `ctx.create_string("")` instead, which is why `out()`
/// answered a `java.lang.String`: not an `Appendable` at all, so
/// `out() instanceof Appendable` was false on a method whose return type is
/// `Appendable`. A `String` also cannot be appended to, which is why `format`
/// below had to carry a read-modify-write branch that replaced slot 0 wholesale.
///
/// Allocating runs `StringBuilder.<init>()V`, so `this` can move: the caller
/// passes its pin handle in and gets the re-derived receiver back with the sink.
pub fn formatter_alloc_sink(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
) -> Result<(ObjectRef, Value), MethodCallFailed> {
    let sink = match ctx.new_object_initialized("java/lang/StringBuilder", "()V", &[])? {
        Some(v @ Value::Object(Some(_))) => v,
        // Allocation refused without reporting a failure. Fall back to a null
        // sink rather than invent one; `formatter_ensure_open` will then report
        // the Formatter as closed, which is a refusal rather than a wrong answer.
        _ => Value::Object(None),
    };
    Ok((ctx.read_native_pin(this_pin, this), sink))
}

/// `Formatter.ensureOpen()` — the guard in front of every public method.
///
/// The JDK's `close()` is `finally { a = null; }` and `ensureOpen()` is
/// `if (a == null) throw new FormatterClosedException()`, so slot 0 being null
/// IS the closed flag; no extra field is needed, and none is available (the
/// synthetic layout this registrar targets is two slots). That identification is
/// only sound because `formatter_alloc_sink` above guarantees a live sink for
/// every constructor including `new Formatter((Appendable) null)` — before that
/// fix a null slot 0 was an ordinary open Formatter and this guard would have
/// mis-reported it as closed.
///
/// MEASURED after `close()` on HotSpot 25.0.3+9: `toString()`, `out()`,
/// `flush()`, `format()` and `locale()` all raise
/// `java.util.FormatterClosedException` with a **null** message. `close()`
/// itself is idempotent, and `ioException()` does NOT throw — it is the one
/// public method with no `ensureOpen`, so it is deliberately left unguarded.
pub fn formatter_ensure_open(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(sink)) = ctx.get_field(this, 0) {
        return Ok(sink);
    }
    Err(formatter_closed(ctx))
}

/// `java.util.FormatterClosedException` — an unchecked `IllegalStateException`
/// subclass with a no-arg constructor and no detail message.
///
/// There is no `RuntimeError` variant for it, so it is built as a real Java
/// object. `ckX` in `RJdkIntrinsics3` compares the EXACT class name, so the
/// nearest available variant (`IllegalStateException`, its superclass) does not
/// substitute even though a `catch` would accept it.
pub fn formatter_closed(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    match ctx.new_object_initialized("java/util/FormatterClosedException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        // Could not build the class. Refuse with the superclass rather than let
        // the call succeed: answering from a closed Formatter is the one
        // outcome that must not happen.
        Ok(_) => RuntimeError::IllegalStateException {
            message: "FormatterClosedException".to_string(),
        }
        .into(),
        Err(e) => e,
    }
}

/// `CRATONVM_JDK_FORMATTER_BODIES` (JIT round 23 wave 3, lane L6): register
/// none of the `String.format` / `String.formatted` / `java.util.Formatter`
/// stand-ins (this registrar's and `lang_math::register_wrapper_natives`'
/// `format` / `formatted` rows), so the JDK's bytecode runs in both modes, as
/// AGENTS.md's "real class bytes beat stand-ins" asks.
///
/// R23-W4-FLIP: default ON since JIT round 23 wave 4 (`=0` restores the
/// stand-ins). A/B on `n23-w3` (`C:\craton\n23p\w3L6\FormatProbe.java`,
/// interleaved, 2 reps): the JDK bodies equal HotSpot on every non-TIME row in
/// both modes (the stand-ins differ on 14: every format error lacked the
/// `Formatter` frames), and are 4-6x faster -- `format-d-s` 2.6-2.8 vs
/// 12.5-15.6 us/op, `format-f` 4.1-4.4 vs 17.8-18.2, `formatter-reuse` 2.6-2.9
/// vs 5.7-6.4. The ` 0:0:` / empty-output failure this registrar's comment
/// records for the JDK chain did not reproduce.
pub fn jdk_formatter_bodies_enabled() -> bool {
    true
}

pub fn register_string_format_real_jdk_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: String.format / Formatter fast-path delegating to the real
    // java.util.Formatter impl in lang_string — replicates real JDK bytecode.
    // String.format(String, Object[]) — the most common form.  Used by
    // `apps/format_probe/FormatProbe.java` and any caller that builds a
    // formatted message without an explicit `Locale`.
    registry.register(
        "java/lang/String",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        lang_string::native_string_format, cratonvm_native_api::NativeKind::Bridge,
    );
    // String.format(Locale, String, Object[]) — `Locale` is ignored for
    // now (we don't have a locale-aware NumberFormat path in real-JDK
    // mode either).  This matches the `synthetic-jdk` registration in
    // `register_wrapper_natives`.
    registry.register(
        "java/lang/String",
        "format",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        lang_string::native_string_format_locale, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // String.formatted(Object[]) — instance method shorthand for
    // `String.format(this, args)`.  Java 15+.
    registry.register(
        "java/lang/String",
        "formatted",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        lang_string::native_string_formatted, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.util.Formatter ---
    //
    // Two-field layout (output=0 String/StringBuilder ref, locale=1).
    // Mirrors `register_formatter_natives` (which is only reachable in
    // synthetic-jdk mode through `register_enterprise_final_natives`).
    let f = "java/util/Formatter";
    registry.register(f, "<init>", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // Slot 1 is the DEFAULT format locale, not null — the real
        // `java.util.Formatter()` is `this(Locale.getDefault(Category.FORMAT),
        // new StringBuilder())`. See [`formatter_default_locale`] for the
        // measured `tr_TR` rows and for the three-rung degradation. Pinned
        // across the resolution because it runs Java code and can move `this`;
        // slot 0 is written first so the new sink is rooted through it.
        let pin = ctx.pin_native_root(this);
        let (this, sink) = formatter_alloc_sink(ctx, this, pin)?;
        ctx.set_field(this, 0, sink);
        let locale = formatter_default_locale(ctx);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        ctx.set_field(this, 1, locale);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "<init>", "(Ljava/util/Locale;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // The locale is the caller's VERBATIM, including an explicit null —
        // MEASURED: `new Formatter((Locale) null).locale()` is null on HotSpot,
        // NOT the host default. Only the no-`Locale` constructors default it.
        // Read it out of `args` BEFORE the allocation below, which can move it,
        // and write it through the re-derived receiver afterwards.
        let pin = ctx.pin_native_root(this);
        let locale = args.get(1).copied().unwrap_or(Value::Object(None));
        let locale_pin = match locale {
            Value::Object(Some(l)) => Some((ctx.pin_native_root(l), l)),
            _ => None,
        };
        let (this, sink) = formatter_alloc_sink(ctx, this, pin)?;
        let locale = match locale_pin {
            Some((h, l)) => Value::Object(Some(ctx.read_native_pin(h, l))),
            None => Value::Object(None),
        };
        ctx.set_field(this, 0, sink);
        ctx.set_field(this, 1, locale);
        ctx.unpin_native_roots(pin);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "<init>", "(Ljava/lang/Appendable;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let appendable = args.get(1).copied().unwrap_or(Value::Object(None));
        let pin = ctx.pin_native_root(this);
        // `Formatter(Locale, Appendable)` is `this.a = (a == null) ? new
        // StringBuilder() : a`, so a NULL Appendable is not stored as null —
        // it becomes a fresh StringBuilder. MEASURED:
        // `new Formatter((Appendable) null).out().getClass()` is
        // `java.lang.StringBuilder` on HotSpot, where this answered `<null>`.
        // Storing the null would also collide with `formatter_ensure_open`'s
        // reading of a null slot 0 as CLOSED.
        let (this, sink) = match appendable {
            Value::Object(Some(a)) => {
                let ah = ctx.pin_native_root(a);
                let this = ctx.read_native_pin(pin, this);
                (this, Value::Object(Some(ctx.read_native_pin(ah, a))))
            }
            _ => formatter_alloc_sink(ctx, this, pin)?,
        };
        ctx.set_field(this, 0, sink);
        // See the `()V` constructor above: slot 1 is the DEFAULT format locale.
        let locale = formatter_default_locale(ctx);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        ctx.set_field(this, 1, locale);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // `Formatter(Appendable, Locale)` is deliberately NOT registered here,
    // and W7-34's proposed patch to add it is not taken. On this registrar's
    // boot path the class is the REAL `java.util.Formatter`, whose own
    // constructor already writes `a` at slot 0 and `l` at slot 1 — the two
    // slots this layout uses — plus `zero`, which no native writes and which
    // real `Formatter` bytecode (the `format(Locale, String, Object[])`
    // overload, which has no native) reads as the digit base. Shadowing that
    // constructor would trade a locale bug for an unwritten `zero`. Once
    // `format` below reads slot 1, the real constructor is all this needs.
    // The synthetic-mode registrar `register_formatter_natives` DOES need the
    // overload, because there is no real constructor there; it has it.
    //
    // F38: that argument was offered as a reason to DELETE the `()V` and
    // `(Appendable)V` registrations too, rather than teach them the default
    // locale. It does not carry, and the reason is slot 0, not slot 1.
    //
    // The real `Formatter()` writes `a = new StringBuilder()`. `read_string`
    // (vm_exec.rs) returns `None` for ANY object whose class is known and is
    // not `java/lang/String` — a deliberate guard, added because the structural
    // reader decoded a `StringBuilder`'s whole char[] CAPACITY. So with the
    // native gone, `toString()` below would read a `StringBuilder` out of slot
    // 0, get `None`, and answer the empty string for every
    // `new Formatter().format(...).toString()`. `format`'s own appending path
    // survives it (it falls through to `invoke_virtual(sb, "append", ...)`),
    // which is what makes the failure silent rather than loud.
    //
    // Deleting is therefore not the smaller change: it is the toString fix plus
    // moving every `new Formatter()` onto real bytecode that resolves
    // `Locale.getDefault(FORMAT)` AND `DecimalFormatSymbols.getInstance(l)
    // .getZeroDigit()` at construction. The `toString()` half is done just
    // below, so whoever wants the deletion now has its precondition; the
    // decision to keep the registrations is a decision about what a lane that
    // cannot build or run should land, not a claim that the deletion is wrong.
    //
    // G32: the slot-0 argument above is now MOOT for the three constructors
    // this registrar owns — they write a real `StringBuilder`, exactly as the
    // JDK does, so `read_string` answering `None` for slot 0 is the expected
    // case rather than the hazard, and `toString()` below routes through
    // `formatter_sink_text`'s `invoke_virtual("toString")` arm. The paragraph is
    // kept because it is still the correct analysis of what deleting the
    // registrations would cost (the locale and `zero` resolution), and that
    // cost is unchanged.
    registry.register(
        f,
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/util/Formatter;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // `format` is one of the five methods guarded by `ensureOpen()`.
            // This has to run BEFORE the formatting work, not just before the
            // append: a post-close `format` that formatted and then refused
            // would still be observable through a `%n`-counting side effect,
            // and the row that caught this defect in the first place is the
            // caller's buffer reading `pre:0051` where HotSpot leaves `pre:005`.
            formatter_ensure_open(ctx, this)?;
            // args[1] = format string, args[2] = Object[] varargs
            let fmt_obj = args.get(1).copied().unwrap_or(Value::Object(None));
            let arr_obj = args.get(2).copied().unwrap_or(Value::Object(None));

            // Field 1 is the `Locale` this Formatter was constructed with —
            // written either by the `(Ljava/util/Locale;)V` ctor above or, for
            // the overloads no native declares, by real `java.util.Formatter`'s
            // own constructor, whose `l` field is slot 1 in the same place.
            // `Formatter.format(String, Object[])` has no locale argument, so
            // the receiver's is the only place one can come from — and
            // discarding it made `new Formatter(sb, Locale.GERMANY)
            // .format("%,.2f", 1234.5)` answer the US `1,234.50` where HotSpot
            // 25 answers `1.234,50`. `native_string_format_locale` takes a null
            // locale as "root defaults", which is what the no-locale ctors
            // store, so the `Locale.ROOT`/`()V` path is byte-for-byte
            // unchanged. W7-34-formatter-family-residuals.md.
            //
            // GC: resolving a non-null locale runs real JDK bytecode (resource
            // bundles, locale providers) and allocates, so the receiver can
            // move across the format call in a way it could not when this
            // delegated to the no-locale path. Pin and re-derive.
            let locale = ctx.get_field(this, 1);
            let this_pin = ctx.pin_native_root(this);
            let format_result =
                lang_string::native_string_format_locale(ctx, &[locale, fmt_obj, arr_obj])?;
            let formatted_str = match format_result {
                Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap_or_default(),
                _ => String::new(),
            };
            // The unpin that used to sit here has moved to the bottom: the
            // append below allocates too, so `this` must stay rooted through it.
            let this = ctx.read_native_pin(this_pin, this);

            // Append to the sink (slot 0), which is ALWAYS an `Appendable`:
            // either the caller's, or the `StringBuilder` the constructors
            // above allocate. The read-modify-write branch that used to sit
            // here — `read_string(slot0)`, concatenate, `set_field` a fresh
            // `String` — existed only because slot 0 held a `String`, and it is
            // gone with that design. It was never merely redundant: replacing
            // slot 0 wholesale means `out()` answers a DIFFERENT object after
            // every `format`, where HotSpot's `out()` is the same buffer for
            // the Formatter's whole life (MEASURED: `f.out() == f.out()` and
            // `f.out().toString().equals(f.toString())` both hold across
            // repeated `format` calls).
            //
            // GC: `create_string` allocates, so the sink must be re-derived
            // AFTER it. Reading slot 0 into a raw `ObjectRef` first and then
            // allocating — what this did — leaves a stale reference to append
            // through. `this` is still pinned by `this_pin` here.
            let text = ctx.create_string(&formatted_str);
            let text_pin = ctx.pin_native_root(text);
            let this = ctx.read_native_pin(this_pin, this);
            let text = ctx.read_native_pin(text_pin, text);
            let sink = match ctx.get_field(this, 0) {
                Value::Object(Some(o)) => o,
                _ => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Object(Some(this))));
                }
            };
            let appended = ctx.invoke_virtual(
                sink,
                "append",
                "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
                &[Value::Object(Some(text))],
            );
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            // Propagate an append failure only AFTER the pin is released.
            let _ = appended?;
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(f, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let sink = formatter_ensure_open(ctx, this)?;
        Ok(Some(formatter_sink_text(ctx, sink)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "out", "()Ljava/lang/Appendable;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Answers slot 0 BY IDENTITY, both for a caller-supplied Appendable and
        // for the constructor-allocated StringBuilder — `f.out() == f.out()` is
        // true on HotSpot, and `new Formatter(sb).out() == sb`.
        let sink = formatter_ensure_open(ctx, this)?;
        Ok(Some(Value::Object(Some(sink))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "close", "()V", |ctx, args| {
        // `Formatter.close()`:
        //
        // ```java
        // if (a == null) return;                       // idempotent
        // try { if (a instanceof Closeable c) c.close(); }
        // catch (IOException ioe) { lastException = ioe; }
        // finally { a = null; }                        // THE closed flag
        // ```
        //
        // Nulling slot 0 is the whole of it, and it is what this body was
        // missing: without it `close()` was a pure no-op, every post-close
        // method answered normally, and a post-close `format()` went on
        // appending to the caller's buffer (`pre:0051` against HotSpot's
        // `pre:005`). Delegating the underlying close is the part that WAS
        // here; it is kept, and its failure is swallowed exactly as the JDK's
        // `catch (IOException)` swallows it, so `close()` stays non-throwing.
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let Value::Object(Some(target)) = ctx.get_field(this, 0) else {
            // Already closed. The JDK's `if (a == null) return` — a second
            // `close()` must NOT throw FormatterClosedException.
            return Ok(None);
        };
        let pin = ctx.pin_native_root(this);
        let _ = ctx.invoke_virtual(target, "close", "()V", &[]);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        ctx.set_field(this, 0, Value::Object(None));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "flush", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let target = formatter_ensure_open(ctx, this)?;
        // StringBuilder implements Appendable but not Flushable. Avoid
        // inventing a StringBuilder.flush() call for the in-memory sink.
        if ctx.read_string(target).is_none() {
            return Ok(None);
        }
        let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(f, "locale", "()Ljava/util/Locale;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Guarded too — `locale()` after `close()` is FormatterClosedException,
        // MEASURED. It is the row most likely to be missed, because unlike the
        // other four it touches slot 1 and has no reason of its own to look at
        // the sink.
        formatter_ensure_open(ctx, this)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
}


#[cfg(feature = "synthetic-jdk")]
pub fn register_builtins(registry: &mut NativeMethodRegistry) {
    // Essential natives first (these are always needed)
    register_essential_natives(registry);
    // Synthetic overrides (only needed when real JDK bytecode unavailable)
    register_synthetic_overrides(registry);
}

/// `String.regionMatches`, both overloads: a reviewed `Intrinsic` registered
/// only in a VM WITHOUT a JIT (`interpreted_intrinsic_registers_row`).
///
/// With a JIT the JDK body wins. Measured on the Azure host (JDK 25.0.4+7, one
/// binary, `regionMatches(false, 0, "jar:", 0, 4)` on a 110-char receiver):
/// the native costs ~320 ns a call -- ~280 ns of it the body's own generic
/// field reads and the two `Vec<u16>` decodes, ~40 ns the crossing, so the
/// leaf claim below recovers only 13 % -- where the compiled bytecode is a few
/// ns. A `jar:nested:` URL parse makes four of these per `new URL(context,
/// spec)`
/// (docs/feature-designs/class-resolution-and-caching.md).
/// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS=string-region-matches` registers
/// the natives in a JIT VM too, marked body-compilable.
///
/// Both registration sites (`lang_math::register_wrapper_natives` and
/// `register_synthetic_overrides`) call this, because `register` captures the
/// ambient leaf claim and a re-registration that did not restate it would
/// demote the triple. LEAF: allocates nothing on the Java heap, never
/// re-enters Java and never blocks; a null `other` is an `Err`.
pub fn register_string_region_matches(registry: &mut NativeMethodRegistry) {
    const ROW: &str = "string-region-matches";
    if !registry.interpreted_intrinsic_registers_row(ROW) {
        return;
    }
    registry.with_leaf(true, |registry| {
        registry.register(
            "java/lang/String",
            "regionMatches",
            "(ZILjava/lang/String;II)Z",
            native_string_region_matches_ic,
            NativeKind::Intrinsic,
        );
        registry.register(
            "java/lang/String",
            "regionMatches",
            "(ILjava/lang/String;II)Z",
            native_string_region_matches,
            NativeKind::Intrinsic,
        );
    });
    registry.mark_body_compilable_intrinsic_row(
        ROW,
        "java/lang/String",
        "regionMatches",
        "(ZILjava/lang/String;II)Z",
    );
    registry.mark_body_compilable_intrinsic_row(
        ROW,
        "java/lang/String",
        "regionMatches",
        "(ILjava/lang/String;II)Z",
    );
}

/// Register synthetic overrides — Rust implementations of methods that have real
/// bytecode in the JDK. These override JDK bytecode and should ONLY be registered
/// in synthetic mode (when `use_synthetic_jdk == true`).
#[cfg(feature = "synthetic-jdk")]
pub fn register_synthetic_overrides(registry: &mut NativeMethodRegistry) {
    let before = registry.len();
    // census-tag: these are Rust fast-paths replicating methods that DO have
    // real JDK bytecode and must match it exactly → Intrinsic. Prior category
    // saved/restored so a wrapping caller is not clobbered.
        
    // W7-93 §7: `StackWalker$Option.<clinit>` is registered ONLY here.
    // `register_essential_natives_with_shims` (called by both the real-JDK
    // and the synthetic arm) registers the rest of the StackWalker boot
    // surface via `register_stack_walker_boot`; the enum initialiser is
    // split out because synthetic mode is the only one that NEEDS it — the
    // fabricated class has three static fields and no `<clinit>` bytecode,
    // and `class_manager.rs` injects an `ACC_NATIVE` `<clinit>` onto that
    // stub for exactly this native to answer. Real-JDK mode loads the actual
    // class, whose own `<clinit>` is ordinary bytecode this VM already runs
    // correctly for every other real JDK enum; leaving this unregistered
    // there lets it run instead of being shadowed.
    stack_walker::register_stack_walker_option_clinit_synthetic(registry);

    // java.lang.Object extras (toString, equals — have bytecode in real JDK)
    registry.register("java/lang/Object", "finalize", "()V", |_ctx, _args| {
        // Object.finalize() is deprecated for removal since JDK 9 and was removed from
        // Object's method table in JDK 18+. Our GC does not invoke finalizers — objects
        // are reclaimed when unreachable, with no opportunity for cleanup hooks. User
        // code should use try-with-resources / Cleaner / PhantomReference instead.
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic); // Deprecated since Java 9
    if identity_hash_leaf_enabled() {
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/Object",
                "hashCode",
                "()I",
                native_object_hash_code_leaf,
                cratonvm_native_api::NativeKind::Intrinsic,
            );
        });
    } else {
        registry.register(
            "java/lang/Object",
            "hashCode",
            "()I",
            native_object_hash_code,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
    registry.register(
        "java/lang/Object",
        "getClass",
        "()Ljava/lang/Class;",
        native_object_get_class, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Object",
        "toString",
        "()Ljava/lang/String;",
        native_object_to_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Object",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_object_equals, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Object",
        "clone",
        "()Ljava/lang/Object;",
        native_object_clone_reclaiming, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/Object", "wait", "()V", native_object_wait, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Object",
        "wait",
        "(J)V",
        native_object_wait_timeout, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // T2.2.2: Object.wait(long millis, int nanos)
    registry.register(
        "java/lang/Object",
        "wait",
        "(JI)V",
        native_object_wait_timeout_nanos, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/Object", "notify", "()V", native_object_notify, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Object",
        "notifyAll",
        "()V",
        native_object_notify_all, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.System ---
    // JNI symbol binding only — nothing to bind, see the essential-natives
    // registration of java/lang/Object.registerNatives. Same for the
    // Class/Thread ones below.
    registry.register("java/lang/System", "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::Intrinsic);
    if identity_hash_leaf_enabled() {
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                native_system_identity_hash_code_leaf,
                cratonvm_native_api::NativeKind::Intrinsic,
            );
        });
    } else {
        registry.register(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            native_system_identity_hash_code,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
    registry.register(
        "java/lang/System",
        "currentTimeMillis",
        "()J",
        native_system_current_time_millis, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        native_system_arraycopy, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.Class ---
    registry.register("java/lang/Class", "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Class",
        "desiredAssertionStatus0",
        "(Ljava/lang/Class;)Z",
        native_assertion_status, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getPrimitiveClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_class_get_primitive_class, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getName0",
        "()Ljava/lang/String;",
        native_class_get_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        native_class_get_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getSimpleName",
        "()Ljava/lang/String;",
        native_class_get_name, cratonvm_native_api::NativeKind::SyntheticStub, // simplified: same as getName for now
    );

    // --- java.lang.Float ---
    registry.register(
        "java/lang/Float",
        "floatToRawIntBits",
        "(F)I",
        native_float_to_raw_int_bits, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.Double ---
    registry.register(
        "java/lang/Double",
        "doubleToRawLongBits",
        "(D)J",
        native_double_to_raw_long_bits, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Double",
        "longBitsToDouble",
        "(J)D",
        native_long_bits_to_double, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.Throwable ---
    registry.register(
        "java/lang/Throwable",
        "fillInStackTrace",
        "(I)Ljava/lang/Throwable;",
        native_throwable_fill_in_stack_trace, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getStackTraceDepth",
        "()I",
        native_throwable_get_stack_trace_depth, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getStackTraceElement",
        "(I)Ljava/lang/StackTraceElement;",
        native_throwable_get_stack_trace_element, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.Throwable (additional) ---
    registry.register(
        "java/lang/Throwable",
        "getMessage",
        "()Ljava/lang/String;",
        native_throwable_get_message, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getCause",
        "()Ljava/lang/Throwable;",
        native_throwable_get_cause, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        native_throwable_init_cause, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "toString",
        "()Ljava/lang/String;",
        native_throwable_to_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
        native_throwable_get_localized_message, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "printStackTrace",
        "()V",
        native_throwable_print_stack_trace, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
        native_throwable_add_suppressed, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
        native_throwable_get_suppressed, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Throwable",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
        native_throwable_get_stack_trace_array, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // (WP8.10.7 wiring moved to register_essential_natives so the Throwable
    // subclass natives are registered in BOTH synthetic-jdk and real-JDK
    // modes — see bench/wildfly-boot/diagnostic.md §WP8.10.7.)

    // --- Exception constructors: DELETED, they are the table's job now ---
    //
    // E34-1 §7 site 3, collapsed by E43 (2026-08-13). This was a `for exc_class
    // in &[30 class names]` loop registering the SAME four descriptors — `()V`,
    // `(String)V`, `(String,Throwable)V`, `(Throwable)V` — on every one of them:
    // 120 registrations, of which 82 restated what
    // `lang_misc::register_throwable_subclass_natives`' per-class JDK-derived
    // table had already registered (with better bodies) and 38 named
    // constructors the real JDK 25 class DOES NOT DECLARE.
    //
    // All 30 names are in that table (MEASURED: set difference is empty), and
    // `register_builtins` calls `register_essential_natives` — which reaches
    // `register_throwable_subclass_natives` — BEFORE it calls this function, so
    // deleting the loop does not leave a single descriptor unregistered. It
    // stops this function from LAST-WRITE-WINNING over the table.
    //
    // Two consequences worth naming, because they are behaviour changes and not
    // just bookkeeping:
    //   * `java/lang/AssertionError.<init>(Ljava/lang/String;)V` — the private
    //     one that `AssertionError(Object)`'s own bytecode calls via
    //     `this(String.valueOf(...))` — now runs `lang_misc`'s body instead of
    //     this loop's identical one, which is the same effect. The (Object)
    //     overload and the six primitive overloads are unaffected either way:
    //     this loop never registered them, and the table already does.
    //   * `java/lang/reflect/InvocationTargetException.<init>()V` and
    //     `(Ljava/lang/Throwable;)V` were being overwritten HERE with the
    //     generic message/cause bodies, so the wrapped throwable landed in
    //     `cause` rather than the JDK's `target` field. The table's
    //     `native_invocation_target_exception_init_target` now wins, and
    //     `target` is a declared field on the synthetic stub
    //     (`class_manager.rs::synthetic_stub_fields`), so the by-name write
    //     lands in both modes.
    //
    // Do not re-add a blanket list here. `the_blanket_four_ctor_loops_stay_
    // collapsed_onto_the_table` (bottom of this file) is a source witness that
    // fails if this loop or its twin in `register_exception_extras_natives`
    // comes back.

    // --- java.lang.Thread ---
    registry.register("java/lang/Thread", "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::Intrinsic);
    // Thread constructors.
    //
    // The no-arg and `(String)` forms used to be registered here as
    // `native_noop_with_this`, i.e. a Thread whose name/priority slots stayed
    // null/0. They were already unreachable: the "store name/target into
    // fields for later start()" block further down this same function
    // re-registers both descriptors, and `NativeMethodRegistry::register` is
    // last-registration-wins. Dropped rather than kept as decoy stubs — the
    // populating versions below are the live implementations.
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            // args[0] = this Thread, args[1] = Runnable target
            // Store the Runnable in field 3 (where Thread.run() looks for it)
            if let (Some(Value::Object(Some(this))), Some(runnable)) = (args.first(), args.get(1)) {
                let num_fields = ctx.object_num_fields(*this);
                if num_fields > 3 {
                    ctx.set_field(*this, 3, runnable.clone());                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            if let (Some(Value::Object(Some(this))), Some(runnable)) = (args.first(), args.get(1)) {
                let num_fields = ctx.object_num_fields(*this);
                if num_fields > 3 {
                    ctx.set_field(*this, 3, runnable.clone());                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        native_thread_current_thread, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "sleep",
        "(J)V",
        crate::lang_system::native_thread_sleep_jfr, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/Thread", "isAlive", "()Z", native_thread_is_alive, cratonvm_native_api::NativeKind::Intrinsic);
    // getPriority/setPriority + isDaemon/setDaemon. These used to be a
    // constant 5, a constant false, and two no-ops, so `setPriority(MAX)`
    // followed by `getPriority()` disagreed with itself and `setDaemon(true)`
    // was unobservable.
    //
    // Three layouts have to be serviced from one native: the real-JDK Thread
    // (values live on `holder`, a `Thread$FieldHolder`), a Thread that
    // declares the field itself, and the synthetic 8-slot Thread whose
    // priority is slot 1 (written as 5 by every constructor above). The
    // synthetic layout has no daemon slot at all — see the note in
    // `setDaemon` below.
    // Round 15 wave 7 (lane drainC; `r15w4-threadvm-review-findings` F2): the
    // fields `java.lang.Thread` declares (`lang_misc::thread_field` /
    // `set_thread_field`), so a subclass's own `holder` / `int priority` is
    // neither read nor clobbered; `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0`
    // restores the by-name reads.
    fn thread_holder(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
        match crate::lang_misc::thread_field(ctx, this, "holder") {
            Value::Object(Some(holder)) => Some(holder),
            _ => None,
        }
    }
    // Read an int attribute, preferring `holder.<name>`, then a same-named
    // field on the Thread itself, then `synthetic_slot` (when the object is
    // the synthetic shape). Returns `default` when none is present.
    fn thread_int_attr(
        ctx: &dyn NativeContext,
        this: ObjectRef,
        name: &str,
        synthetic_slot: Option<usize>,
        default: i32,
    ) -> i32 {
        if let Some(holder) = thread_holder(ctx, this) {
            if let Value::Int(v) = ctx.get_field_by_name(holder, name) {
                return v;
            }
        }
        let class_id = ctx.class_id_of_object(this);
        if ctx
            .resolve_field_index_by_class_id(class_id, name)
            .is_some()
        {
            if let Value::Int(v) = crate::lang_misc::thread_field(ctx, this, name) {
                return v;
            }
        }
        if let Some(slot) = synthetic_slot {
            if ctx.object_num_fields(this) > slot {
                if let Value::Int(v) = ctx.get_field(this, slot) {
                    return v;
                }
            }
        }
        default
    }
    // Mirror of `thread_int_attr`. Writes every location that exists, so a
    // later read finds the value whichever layout the reader assumes.
    fn set_thread_int_attr(
        ctx: &dyn NativeContext,
        this: ObjectRef,
        name: &str,
        synthetic_slot: Option<usize>,
        value: i32,
    ) {
        if let Some(holder) = thread_holder(ctx, this) {
            ctx.set_field_by_name(holder, name, Value::Int(value));
        }
        // No-op when the field is absent, so this is safe to call blind.
        crate::lang_misc::set_thread_field(ctx, this, name, Value::Int(value));
        if let Some(slot) = synthetic_slot {
            if ctx.object_num_fields(this) > slot {
                ctx.set_field(this, slot, Value::Int(value));
            }
        }
    }
    registry.register("java/lang/Thread", "getPriority", "()I", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Int(5)));
        };
        Ok(Some(Value::Int(thread_int_attr(
            ctx,
            this,
            "priority",
            Some(1),
            5,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "setPriority", "(I)V", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(None);
        };
        let requested = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => return Ok(None),
        };
        // `Thread.setPriority` rejects out-of-range values with
        // IllegalArgumentException; we have no bytecode arg-check in front of
        // this native, so clamp to MIN_PRIORITY..=MAX_PRIORITY rather than
        // storing a value `getPriority()` could never legally return.
        set_thread_int_attr(ctx, this, "priority", Some(1), requested.clamp(1, 10));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "isDaemon", "()Z", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Int(0)));
        };
        let daemon = thread_int_attr(ctx, this, "daemon", None, 0);
        Ok(Some(Value::Int(i32::from(daemon != 0))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "setDaemon", "(Z)V", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(None);
        };
        let on = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
        // NOTE (residual): the synthetic Thread layout
        // (class_manager.rs: name/priority/tid/target/virtualFlag + the three
        // JBoss-reflected slots) has no daemon field, and `virtualFlag` is
        // spoken for. So on a pure synthetic Thread this write lands nowhere
        // and `isDaemon()` still reports false. That is also what the VM
        // assumes — `vm_exec.rs::read_thread_daemon_flag` documents "synthetic
        // Thread without a daemon field" as `None` → treat as non-daemon — so
        // no join/shutdown decision changes. Giving synthetic Threads a real
        // daemon slot is a class_manager change, not a native one.
        set_thread_int_attr(ctx, this, "daemon", None, i32::from(on));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // NOTE: do NOT register a `getThreadGroup` shim that returns null.
    // In real-JDK mode the Thread class has a Java implementation
    // (`return holder.group`) and shadowing it with a null-returning
    // native breaks `new Thread(...)`: the constructor calls
    // `parent.getThreadGroup()` and then `g.getMaxPriority()`, which
    // NPEs when `g` is null. The fallback in `vm_exec.rs::invoke_*`
    // already returns null when the method is genuinely missing
    // (e.g. synthetic-JDK / pre-bootstrap call sites).
    registry.register(
        "java/lang/Thread",
        "getName",
        "()Ljava/lang/String;",
        native_thread_get_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "setName",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first() else {
                return Ok(None);
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            // Round 15 wave 4 (lane threadvm): `Thread`'s own `name`, which
            // `getName` (`native_thread_get_name`) now reads.
            crate::lang_misc::set_thread_field(ctx, *this, "name", name.clone());
            if !has_real_jdk_thread_layout(ctx, *this) && ctx.object_num_fields(*this) > 0 {
                ctx.set_field(*this, 0, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // (`setPriority` is registered with `getPriority` above — a second, no-op
    // registration used to sit here and, being later, silently won.)
    registry.register(
        "java/lang/Thread",
        "isInterrupted",
        "(Z)Z",
        native_thread_is_interrupted, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Public no-arg instance method: Thread.isInterrupted() -> doesn't clear
    registry.register("java/lang/Thread", "isInterrupted", "()Z", |ctx, _args| {
        let interrupted = if ctx.is_interrupted(false) { 1 } else { 0 };
        Ok(Some(Value::Int(interrupted)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // Static method: Thread.interrupted() -> clears flag
    registry.register("java/lang/Thread", "interrupted", "()Z", |ctx, _args| {
        let interrupted = if ctx.is_interrupted(true) { 1 } else { 0 };
        Ok(Some(Value::Int(interrupted)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // Thread constructors: store name/target into fields for later start()
    registry.register("java/lang/Thread", "<init>", "()V", |ctx, args| {
        // no-arg constructor: set default name
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let name = thread_default_name(ctx);
        ctx.set_field(this, 0, Value::Object(Some(name)));
        ctx.set_field(this, 1, Value::Int(5)); // default priority
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let target = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name = thread_default_name(ctx);
            ctx.set_field(this, 0, Value::Object(Some(name)));
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let target = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(2).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name_val = match args.get(1).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name = thread_default_name(ctx);
            ctx.set_field(this, 0, Value::Object(Some(name)));
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(2).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = args
                .get(3)
                .cloned()
                .unwrap_or_else(|| Value::Object(Some(thread_default_name(ctx))));
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let target = args.get(2).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(3).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(5));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;ILjava/lang/Runnable;J)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).cloned().unwrap_or(Value::Object(None));
            let name_val = match args.get(2).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            let prio = match args.get(3) {
                Some(Value::Int(p)) => *p,
                _ => 5,
            };
            let target = args.get(4).cloned().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(prio));
            ctx.set_field(this, 2, group);
            ctx.set_field(this, 3, target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/String;IZ)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name_val = match args.get(1).cloned().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            let prio = match args.get(2) {
                Some(Value::Int(p)) => *p,
                _ => 5,
            };
            ctx.set_field(this, 0, name_val);
            ctx.set_field(this, 1, Value::Int(prio));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/Thread", "start0", "()V", native_thread_start0, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "start", "()V", native_thread_start0, cratonvm_native_api::NativeKind::Intrinsic);
    // Real bug fix (2026-07-16, ClientHttpConnectorTests okio.Segment
    // ClassCastException investigation): `getId()` was hardcoded to always
    // return `1` for EVERY thread in the process. Okio's `SegmentPool`
    // (okio-jvm 3.x) shards its free-list into `HASH_BUCKET_COUNT`
    // `AtomicReference<Segment>` buckets, selecting one via
    // `Thread.currentThread().getId() & (HASH_BUCKET_COUNT - 1)` --
    // with every thread hashing to the identical bucket, ALL of the
    // process's segment-pool traffic (every socket read/write across every
    // connection) collapsed onto one shared `AtomicReference`, producing
    // extreme contention that exposed a race and surfaced as
    // `ClassCastException: java.lang.Object cannot be cast to okio.Segment`
    // at `SegmentPool.take()` (a `getAndSet` racing to return a value that
    // was neither the expected `Segment`/`null`/the `LOCK` sentinel).
    // Both `getId()` and `threadId()` must report the id assigned to their
    // receiver. The executing VM context is not the receiver when one thread
    // inspects another, as JULI's ThreadMXBean lookup does below.
    registry.register("java/lang/Thread", "getId", "()J", |ctx, args| {
        // Round 15 wave 4 (lane threadvm): `Thread`'s own `tid`, not a
        // subclass's `long tid` field (`lang_misc::thread_field`).
        let receiver_tid = args.first().and_then(|value| match value {
            Value::Object(Some(thread)) => {
                match crate::lang_misc::thread_field(&*ctx, *thread, "tid") {
                    Value::Long(tid) if tid > 0 => Some(tid),
                    Value::Int(tid) if tid > 0 => Some(tid as i64),
                    _ => None,
                }
            }
            _ => None,
        });
        Ok(Some(Value::Long(
            receiver_tid.unwrap_or_else(|| ctx.thread_id().max(1) as i64),
        )))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // Thread.getState() and Thread.threadState() both return Thread$State.
    // In JDK 25, getState() delegates to threadState() which reads
    // holder.threadStatus and calls VM.toThreadState(int).  We override
    // both to return RUNNABLE directly, avoiding the FieldHolder dependency.
    let thread_state_runnable = |ctx: &mut dyn NativeContext, _args: &[Value]| {
        // gc-common w20-c: the name first, pinned across the object's
        // allocation (allocated second, it could leave `obj` stale).
        let name = ctx.create_string("RUNNABLE");
        let name_pin = ctx.pin_native_root(name);
        let obj: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/lang/Thread$State", 2)?;
        let name: ObjectRef = ctx.read_native_pin(name_pin, name);
        ctx.unpin_native_roots(name_pin);
        ctx.set_field(obj, 0, Value::Object(Some(name)));
        ctx.set_field(obj, 1, Value::Int(1));
        Ok(Some(Value::Object(Some(obj))))
    };
    registry.register(
        "java/lang/Thread",
        "getState",
        "()Ljava/lang/Thread$State;",
        thread_state_runnable, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "threadState",
        "()Ljava/lang/Thread$State;",
        thread_state_runnable, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Thread constructors/run: keep the final essential-native registrations
    // aligned with the real-JDK holder-backed layout. Earlier registrations in
    // this file already do this, but this later essential block used to
    // re-register synthetic slot-3-only variants; last-write-wins made
    // Thread subclasses such as JBossThread start and immediately return
    // without ever invoking their Runnable target.
    registry.register("java/lang/Thread", "<init>", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let name = Value::Object(Some(thread_default_name(ctx)));
        if !has_real_jdk_thread_layout(ctx, this) {
            ctx.set_field(this, 0, name);
            ctx.set_field(this, 1, Value::Int(5));        } else {
            populate_real_thread_holder(ctx, this, Value::Object(None), Value::Object(None), name);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = match args.get(1).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
            } else {
                populate_real_thread_holder(
                    ctx,
                    this,
                    Value::Object(None),
                    Value::Object(None),
                    name,
                );
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let target = args.get(1).copied().unwrap_or(Value::Object(None));
            let name = Value::Object(Some(thread_default_name(ctx)));
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, Value::Object(None), target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let target = args.get(1).copied().unwrap_or(Value::Object(None));
            let name = match args.get(2).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, Value::Object(None), target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            let name = Value::Object(Some(thread_default_name(ctx)));
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 2, group);
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, group, target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let name = match args.get(2).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 2, group);            } else {
                populate_real_thread_holder(ctx, this, group, Value::Object(None), name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            let name = match args.get(3).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 2, group);
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, group, target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            let name = match args.get(3).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 2, group);
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, group, target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            let name = match args.get(3).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(5));
                ctx.set_field(this, 2, group);
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, group, target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;ILjava/lang/Runnable;J)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let group = args.get(1).copied().unwrap_or(Value::Object(None));
            let name = match args.get(2).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => Value::Object(Some(thread_default_name(ctx))),
            };
            let prio = match args.get(3) {
                Some(Value::Int(p)) => *p,
                _ => 5,
            };
            let target = args.get(4).copied().unwrap_or(Value::Object(None));
            if !has_real_jdk_thread_layout(ctx, this) {
                ctx.set_field(this, 0, name);
                ctx.set_field(this, 1, Value::Int(prio));
                ctx.set_field(this, 2, group);
                ctx.set_field(this, 3, target);            } else {
                populate_real_thread_holder(ctx, this, group, target, name);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Thread.run() delegates to the target Runnable stored either in the
    // synthetic slot layout or in real JDK 25's Thread$FieldHolder.task.
    // Exceptions propagate (`?`, not `let _ =`) so `vm_exec.rs::thread_start`
    // sees the failure and can run the uncaught-exception path — see the
    // longer note on the essential-natives registration of this same method.
    registry.register("java/lang/Thread", "run", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let num_fields = ctx.object_num_fields(this);
        if !has_real_jdk_thread_layout(ctx, this) {
            if num_fields >= 4 {
                if let Value::Object(Some(target)) = ctx.get_field(this, 3) {
                    crate::lang_misc::served_thread_run_call(ctx, target)?;
                }
            }
        } else {
            // Round 15 wave 3 (lane trace3): the fields `Thread` declares.
            // Lane trace4 (TR2W2-2): the call is recorded.
            let target_val = crate::lang_misc::served_thread_run_task(ctx, this);
            if let Value::Object(Some(target)) = target_val {
                crate::lang_misc::served_thread_run_call(ctx, target)?;            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "join", "()V", native_thread_join, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Thread", "join", "(J)V", native_thread_join_timed, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Thread",
        "join",
        "(JI)V",
        crate::lang_system::native_thread_join_millis_nanos, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Thread",
        "interrupt",
        "()V",
        native_thread_interrupt, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // KEEP: correct constant. CratonVM has no JNI-attached ("native") threads —
    // every java.lang.Thread it hands out is VM-created.
    registry.register("java/lang/Thread", "isNative", "()Z", |_, _| {
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- cratonvm/VirtualThreadTest native helpers (Session 25) ---
    let vt_test = "cratonvm/VirtualThreadTest";
    // startVirtualThread(Runnable) -> Thread — creates and starts a virtual thread
    registry.register(
        vt_test,
        "startVirtualThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let runnable = args.first().copied().unwrap_or(Value::Object(None));
            vt_test_start_thread(ctx, runnable, "virtual-thread", true)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // threadIsVirtual(Thread) -> boolean — checks virtual flag
    registry.register(
        vt_test,
        "threadIsVirtual",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            let thr = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let num_fields = ctx.object_num_fields(thr);
            if num_fields >= 5 {
                match ctx.get_field(thr, 4) {
                    Value::Int(v) => Ok(Some(Value::Int(if v != 0 { 1 } else { 0 }))),
                    _ => Ok(Some(Value::Int(0))),
                }
            } else {
                Ok(Some(Value::Int(0)))
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // builderOfVirtualStart(Runnable) -> Thread — simulates Thread.ofVirtual().start(r)
    registry.register(
        vt_test,
        "builderOfVirtualStart",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let runnable = args.first().copied().unwrap_or(Value::Object(None));
            vt_test_start_thread(ctx, runnable, "virtual-builder-thread", true)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // builderOfPlatformStart(Runnable) -> Thread — simulates Thread.ofPlatform().start(r)
    registry.register(
        vt_test,
        "builderOfPlatformStart",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let runnable = args.first().copied().unwrap_or(Value::Object(None));
            vt_test_start_thread(ctx, runnable, "platform-builder-thread", false)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.String ---
    // Constructors
    registry.register("java/lang/String", "<init>", "([BII)V", |ctx, args| {
        // new String(byte[], off, len)
        let _this = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => return Ok(None),
        };
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
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
            match ctx.get_array_element(arr, off + i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        let s = String::from_utf8_lossy(&bytes).to_string();
        // gc-common w20-c: `this` is written after the string's allocation.
        let this_pin = ctx.pin_native_root(_this);
        let str_obj = ctx.create_string(&s);
        let _this: ObjectRef = ctx.read_native_pin(this_pin, _this);
        ctx.unpin_native_roots(this_pin);
        // Copy the string content from the new string to `this`
        // Since String is synthetic, the VM treats the ObjectRef itself as the string.
        // We just return the new string as this — but constructors return void.
        // Instead, copy the intern data: field 0 (value array) + field 1 (hash).
        let val = ctx.get_field(str_obj, 0);
        ctx.set_field(_this, 0, val);
        let hash = ctx.get_field(str_obj, 1);
        ctx.set_field(_this, 1, hash);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/String", "<init>", "([B)V", |ctx, args| {
        // new String(byte[])
        let _this = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => return Ok(None),
        };
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(None),
        };
        let len = ctx.array_length(arr);
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            match ctx.get_array_element(arr, i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        let s = String::from_utf8_lossy(&bytes).to_string();
        // gc-common w20-c: `this` is written after the string's allocation.
        let this_pin = ctx.pin_native_root(_this);
        let str_obj = ctx.create_string(&s);
        let _this: ObjectRef = ctx.read_native_pin(this_pin, _this);
        ctx.unpin_native_roots(this_pin);
        let val = ctx.get_field(str_obj, 0);
        ctx.set_field(_this, 0, val);
        let hash = ctx.get_field(str_obj, 1);
        ctx.set_field(_this, 1, hash);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/String",
        "intern",
        "()Ljava/lang/String;",
        native_string_intern, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "hashCode",
        "()I",
        native_string_hash_code, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/String", "length", "()I", native_string_length, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/String", "charAt", "(I)C", native_string_char_at, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/String",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_string_equals, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "indexOf",
        "(I)I",
        native_string_index_of, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // T2.2.6: indexOf(int ch, int fromIndex) / lastIndexOf(int ch, int fromIndex)
    registry.register(
        "java/lang/String",
        "indexOf",
        "(II)I",
        lang_string::native_string_index_of_from, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(II)I",
        lang_string::native_string_last_index_of_from, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // T2.2.7 (`String.codePointAt(int)`) is already registered below
    // with the rest of the lang-math natives; T2.2.8
    // (`String.compareToIgnoreCase`) is already registered further down
    // in this same function. The T2 census captures both as pre-existing.
    registry.register(
        "java/lang/String",
        "substring",
        "(II)Ljava/lang/String;",
        native_string_substring, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(I)Ljava/lang/String;",
        native_string_value_of_int, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.io.PrintStream ---
    // Synthetic PrintStream — all methods captured to thread.printed_lines.
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/String;)V",
        native_println_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/io/PrintStream", "println", "(I)V", native_println_int, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/io/PrintStream",
        "println",
        "(J)V",
        native_println_long, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(D)V",
        native_println_double, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Z)V",
        native_println_boolean, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(C)V",
        native_println_char, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/io/PrintStream", "println", "()V", native_println_void, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/Object;)V",
        native_println_object, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "print",
        "(Ljava/lang/String;)V",
        native_print_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/io/PrintStream", "print", "(I)V", native_print_int, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/io/PrintStream", "print", "(C)V", native_print_char, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/io/PrintStream", "print", "(Z)V", native_print_boolean, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/io/PrintStream", "print", "(J)V", native_print_long, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/io/PrintStream", "print", "(F)V", native_print_float, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/io/PrintStream", "print", "(D)V", native_print_double, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/io/PrintStream",
        "print",
        "(Ljava/lang/Object;)V",
        native_print_object, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(F)V",
        native_println_float, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "printf",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(I)V",
        native_printstream_write_int, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "flush",
        "()V",
        native_printstream_flush, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "close",
        "()V",
        native_printstream_close, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "([BII)V",
        native_printstream_write, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(Ljava/lang/String;)V",
        native_printstream_write_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(Ljava/lang/String;II)V",
        native_printstream_write_string_range, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.io.PrintWriter ---
    registry.register(
        "java/io/PrintWriter",
        "print",
        "(Ljava/lang/String;)V",
        native_print_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintWriter",
        "println",
        "(Ljava/lang/String;)V",
        native_println_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/io/PrintWriter", "println", "()V", native_println_void, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/io/PrintWriter",
        "printf",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;",
        native_printwriter_printf, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintWriter",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;",
        native_printwriter_printf, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintWriter",
        "flush",
        "()V",
        native_printwriter_flush, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintWriter",
        "close",
        "()V",
        native_printwriter_close, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/io/PrintWriter",
        "write",
        "(I)V",
        native_printwriter_write_int, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- the READ side of `trouble`, for BOTH classes ---
    //
    // SYNTHETIC-JDK ONLY, and deliberately so: Compatible mode runs the real
    // `checkError`/`setError`/`clearError` bytecode over the real `trouble`
    // field, which is correct once the absorbing sites record — shadowing it
    // with a native would be a contract-1.4 shadow on working code.
    //
    // These are two DISTINCT classes sharing one body, which is exactly the
    // shape that has been wrong elsewhere in this workspace, so it is stated
    // rather than assumed: `checkError` differs between them ONLY in the type
    // its delegation branch tests (`PrintStream` vs `PrintWriter`/`psOut`) and
    // `printstream_check_error` tests BOTH, in the JDK's order, so one body is
    // correct for both. `setError`/`clearError` are byte-identical two-line
    // bodies in both classes. Every other `PrintStream`/`PrintWriter` pair in
    // this file that looks shared (`native_println_string` and friends) is
    // shared for the different reason that the text is the same; those go
    // through `stream_write`, which branches on the receiver.
    // W7-64-printstream-trouble-and-errormanager.md
    for print_class in ["java/io/PrintStream", "java/io/PrintWriter"] {
        registry.register(print_class, "checkError", "()Z", printstream_check_error, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(print_class, "setError", "()V", printstream_set_error, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(print_class, "clearError", "()V", printstream_clear_error, cratonvm_native_api::NativeKind::Intrinsic);
    }

    // --- java.lang.StringBuilder ---
    register_string_builder_natives(registry, "java/lang/StringBuilder");
    register_string_builder_natives(registry, "java/lang/StringBuffer");

    // --- Additional String natives (Step 3) ---
    registry.register(
        "java/lang/String",
        "toCharArray",
        "()[C",
        native_string_to_char_array, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "contains",
        "(Ljava/lang/CharSequence;)Z",
        native_string_contains, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "startsWith",
        "(Ljava/lang/String;)Z",
        native_string_starts_with, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "startsWith",
        "(Ljava/lang/String;I)Z",
        native_string_starts_with_offset, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "endsWith",
        "(Ljava/lang/String;)Z",
        native_string_ends_with, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "trim",
        "()Ljava/lang/String;",
        native_string_trim, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "replace",
        "(CC)Ljava/lang/String;",
        native_string_replace, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "toLowerCase",
        "()Ljava/lang/String;",
        native_string_to_lower_case, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "toUpperCase",
        "()Ljava/lang/String;",
        native_string_to_upper_case, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/String", "isEmpty", "()Z", native_string_is_empty, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/String",
        "valueOf",
        "(J)Ljava/lang/String;",
        native_string_value_of_long, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(Z)Ljava/lang/String;",
        native_string_value_of_boolean, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(D)Ljava/lang/String;",
        native_string_value_of_double, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(C)Ljava/lang/String;",
        native_string_value_of_char, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(F)Ljava/lang/String;",
        native_string_value_of_float, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        native_string_value_of_object, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "compareTo",
        "(Ljava/lang/String;)I",
        native_string_compare_to, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Erased generic overload: Comparable<String>.compareTo(Object)
    registry.register(
        "java/lang/String",
        "compareTo",
        "(Ljava/lang/Object;)I",
        native_string_compare_to, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "indexOf",
        "(Ljava/lang/String;)I",
        native_string_index_of_str, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "substring",
        "(I)Ljava/lang/String;",
        native_string_substring_one, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "getBytes",
        "()[B",
        native_string_get_bytes, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "concat",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_string_concat, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Additional String methods (Phase 8 Part 7) ---
    registry.register(
        "java/lang/String",
        "split",
        "(Ljava/lang/String;)[Ljava/lang/String;",
        native_string_split, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "split",
        "(Ljava/lang/String;I)[Ljava/lang/String;",
        native_string_split_limit, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "split",
        "(Ljava/lang/String;IZ)[Ljava/lang/String;",
        native_string_split_private, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "join",
        "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
        native_string_join, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "replaceAll",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_string_replace_all, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "replaceFirst",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_string_replace_first, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "matches",
        "(Ljava/lang/String;)Z",
        native_string_matches, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "equalsIgnoreCase",
        "(Ljava/lang/String;)Z",
        native_string_equals_ignore_case, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "compareToIgnoreCase",
        "(Ljava/lang/String;)I",
        native_string_compare_to_ignore_case, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(I)I",
        native_string_last_index_of_char, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(Ljava/lang/String;)I",
        native_string_last_index_of_str, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "getChars",
        "(II[CI)V",
        native_string_get_chars, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "strip",
        "()Ljava/lang/String;",
        native_string_strip, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "stripLeading",
        "()Ljava/lang/String;",
        native_string_strip_leading, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "stripTrailing",
        "()Ljava/lang/String;",
        native_string_strip_trailing, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "copyValueOf",
        "([C)Ljava/lang/String;",
        native_string_copy_value_of, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "([C)Ljava/lang/String;",
        native_string_copy_value_of, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- String modern methods (Java 11+, Phase 49) ---
    registry.register(
        "java/lang/String",
        "repeat",
        "(I)Ljava/lang/String;",
        native_string_repeat, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/String", "isBlank", "()Z", native_string_is_blank, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/String",
        "chars",
        "()Ljava/util/stream/IntStream;",
        native_string_chars, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "lines",
        "()Ljava/util/stream/Stream;",
        native_string_lines, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "codePoints",
        "()Ljava/util/stream/IntStream;",
        // NOT `native_string_chars`, which is what this line said until
        // 2026-08-12 with the comment "Same as chars() for BMP characters" —
        // true for the BMP, and precisely why nobody looked again. `chars()`
        // yields UTF-16 code UNITS, so a supplementary character arrives as its
        // two surrogates and `codePoints()` must pair them back into ONE code
        // point: on HotSpot 25 `"a😀b".codePoints()` is
        // `[97, 128512, 98]` (3 elements), against `[97, 55357, 56832, 98]`
        // (4) for `chars()`. So `codePoints().count()` was wrong too, not only
        // the values.
        //
        // This is the SECOND of exactly two registrations of this triple; the
        // twin is `lang_math.rs`'s, and `register()` is last-write-wins, so
        // whichever registrar runs later silently decides. Both now bind the
        // same body, which is what makes the fix independent of that order.
        // docs/feature-designs/native-dispatch-architecture.md
        crate::lang_string::native_string_code_points, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/String",
        "formatted",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        native_string_formatted, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // `String.indent(int)` — the SAME body `lang_math::register_wrapper_natives`
    // binds (`lib.rs:14679`, inside `register_essential_natives_with_shims`),
    // not a second copy.
    //
    // This slot used to hold an inline closure, and because
    // `register_builtins` runs `register_essential_natives` and only THEN
    // `register_synthetic_overrides`, and `register()` is last-write-wins, that
    // closure WON in synthetic-JDK mode — silently reverting W7-95a's fix for
    // exactly the mode the blocking gate runs in. The closure was the pre-W7-95a
    // body: `let strip = (-n) as usize; if line.len() > strip { &line[strip..] }`.
    // Three defects, measured against `openjdk 25.0.3 2026-04-21 LTS
    // (25.0.3+9-LTS)` (Microsoft build) on this host, `scratchpad/f20/Ind.java`:
    //
    //   HotSpot                                     old closure
    //   "abc".indent(-1)        = "abc\n"           "bc\n"        wrong answer
    //   "  abc".indent(-5)      = "abc\n"           ""            wrong answer
    //   (U+00A0)+"abc".indent(-1) = the same string back  VM ABORT  (see below)
    //   "abc".indent(MIN_VALUE) = "abc\n"           VM ABORT      (debug)
    //
    // Row 3 is the VM-fatal one and it is why this had to move rather than be
    // patched in place: Java strips only leading `Character.isWhitespace`
    // characters, capped at `-n` — `s.substring(Math.min(-n, s.indexOfNonWhitespace()))`
    // - and `Character.isWhitespace(U+00A0)` is **false** (measured). The
    // closure counted BYTES and sliced `&line[1..]`, which lands inside
    // U+00A0's two-byte UTF-8 encoding: `panic!("byte index 1 is not a char
    // boundary")`, and a Rust panic is not a Java throwable — it takes the VM
    // down where HotSpot returns a string. Row 4 is `-i32::MIN`, an overflow
    // panic in debug; the JDK has an explicit `n == Integer.MIN_VALUE` arm
    // (`stripLeading()`) for that same reason, `String.java:4036`.
    //
    // `lang_string::native_string_indent` is the arm that already gets all four
    // right. This is a re-point, not a re-implementation: a third copy of one
    // rule is how the first two drifted.
    registry.register(
        "java/lang/String",
        "indent",
        "(I)Ljava/lang/String;",
        crate::lang_string::native_string_indent, cratonvm_native_api::NativeKind::Intrinsic,
    );
    register_string_region_matches(registry);

    // --- System properties + utilities (Step 4) ---
    // getProperty overloads are registered earlier (T14) with
    // property_key_from_java_string + bootstrap_property_fallback; do not
    // re-register here — native_system_get_property would overwrite them.
    registry.register(
        "java/lang/System",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_system_set_property, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Re-registration of the SAME body, so the leaf claim has to be restated:
    // `register` captures the ambient claim into the slot, and a re-register
    // that does not opt in demotes the triple back to the funnel. That is the
    // deliberate ordering property of `set_leaf` — see its doc comment.
    registry.set_leaf(true);
    registry.register(
        "java/lang/System",
        "nanoTime",
        "()J",
        native_system_nano_time, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.set_leaf(false);
    registry.register("java/lang/System", "exit", "(I)V", native_system_exit, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/lang/System", "gc", "()V", |ctx, _args| {
        crate::hooks::jfr_force_gc_as_system_gc(ctx);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/System",
        "getenv",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_system_getenv, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "getenv",
        "()Ljava/util/Map;",
        native_system_getenv_all, cratonvm_native_api::NativeKind::Bridge,
    );

    registry.register(
        "java/lang/System",
        "lineSeparator",
        "()Ljava/lang/String;",
        native_system_line_separator, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Runtime natives (Phase 13 Step 4) ---
    register_runtime_natives(registry);

    // --- UUID natives (Phase 14 Step 2) ---
    register_uuid_natives(registry);

    // --- Regex natives (Phase 15 Step 1) ---
    register_regex_natives(registry);

    // --- java.time natives (Phase 17) ---
    register_time_natives(registry);

    // --- Math natives (Step 5) ---
    //
    // CLOSED 2026-08-12 — W7-54-strictmath-fdlibm-family-RETIRED-20260917.md. StrictMath used to
    // share the SAME backing transcendentals as Math here, delegating to the
    // host libm, which violated StrictMath's bit-for-bit reproducibility
    // contract. `register_math_natives` now gives StrictMath the fdlibm bodies
    // in `cratonvm_types::fdlibm` (a port of JDK 25's `java.lang.FdLibm`) and
    // leaves Math on libm, whose 1-ULP bound libm meets. See the fuller note at
    // the other Math/StrictMath registration site above for what stays shared
    // (`sqrt` and `IEEEremainder`) and why.
    register_math_natives(registry, "java/lang/Math");
    register_math_natives(registry, "java/lang/StrictMath");

    // --- Wrapper types / boxing (Step 6) ---
    register_wrapper_natives(registry);
    // W7-98 N2: `java.lang.Character`'s Unicode classification/case-mapping
    // natives are registered ONLY here (synthetic mode fabricates
    // `Character` with no `CharacterData` bytecode). See
    // `lang_math::register_character_classification_natives`'s doc comment.
    lang_math::register_character_classification_natives(registry);

    // --- Enhanced Class support (Step 7) ---
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_class_for_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
        native_class_for_name_3, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // T19_H12_FORNAME_MODULE — `Class.forName(Module, String)` native.
    // Bypasses the JDK bytecode path that mis-dispatches
    // `loadClass(Module, String)` to a HashSet receiver because our
    // synthetic `java.lang.Module` slot 2 holds the packages set where
    // real JDK 25 Module declares `loader: ClassLoader`.
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
        native_class_for_name_module, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "isInstance",
        "(Ljava/lang/Object;)Z",
        native_class_is_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "isAssignableFrom",
        "(Ljava/lang/Class;)Z",
        native_class_is_assignable_from, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register("java/lang/Class", "isArray", "()Z", native_class_is_array, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register("java/lang/Class", "isEnum", "()Z", native_class_is_enum, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(
        "java/lang/Class",
        "isInterface",
        "()Z",
        native_class_is_interface, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "isPrimitive",
        "()Z",
        native_class_is_primitive, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.with_leaf(true, |r| {
        r.register(
            "java/lang/Class",
            "getSuperclass",
            "()Ljava/lang/Class;",
            lang_class::native_class_get_superclass_leaf, cratonvm_native_api::NativeKind::Intrinsic,
        );
    });
    registry.register(
        "java/lang/Class",
        "getSimpleName",
        "()Ljava/lang/String;",
        native_class_get_simple_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "newInstance",
        "()Ljava/lang/Object;",
        native_class_new_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.reflect.Field (Phase 8 Part 3) ---
    registry.register(
        "java/lang/reflect/Field",
        "getName",
        "()Ljava/lang/String;",
        native_field_get_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getType",
        "()Ljava/lang/Class;",
        native_field_get_type, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getModifiers",
        "()I",
        native_field_get_modifiers, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_field_get_declaring_class, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setAccessible",
        "(Z)V",
        lang_class::native_field_set_accessible, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_field_get, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "set",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_field_set, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getInt",
        "(Ljava/lang/Object;)I",
        native_field_get_int, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getLong",
        "(Ljava/lang/Object;)J",
        native_field_get_long, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getFloat",
        "(Ljava/lang/Object;)F",
        native_field_get_float, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getDouble",
        "(Ljava/lang/Object;)D",
        native_field_get_double, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getBoolean",
        "(Ljava/lang/Object;)Z",
        native_field_get_boolean, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setInt",
        "(Ljava/lang/Object;I)V",
        native_field_set_int, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setLong",
        "(Ljava/lang/Object;J)V",
        native_field_set_long, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setFloat",
        "(Ljava/lang/Object;F)V",
        native_field_set_float, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setDouble",
        "(Ljava/lang/Object;D)V",
        native_field_set_double, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setBoolean",
        "(Ljava/lang/Object;Z)V",
        native_field_set_boolean, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getByte",
        "(Ljava/lang/Object;)B",
        native_field_get_byte, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getShort",
        "(Ljava/lang/Object;)S",
        native_field_get_short, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getChar",
        "(Ljava/lang/Object;)C",
        native_field_get_char, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setByte",
        "(Ljava/lang/Object;B)V",
        native_field_set_byte, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setShort",
        "(Ljava/lang/Object;S)V",
        native_field_set_short, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "setChar",
        "(Ljava/lang/Object;C)V",
        native_field_set_char, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.reflect.Method (Phase 8 Part 3) ---
    registry.register(
        "java/lang/reflect/Method",
        "getName",
        "()Ljava/lang/String;",
        native_method_get_name, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getReturnType",
        "()Ljava/lang/Class;",
        native_method_get_return_type, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
        native_method_get_parameter_types, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getModifiers",
        "()I",
        native_method_get_modifiers, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_method_get_declaring_class, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "setAccessible",
        "(Z)V",
        lang_class::native_method_set_accessible, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getParameterCount",
        "()I",
        native_method_get_parameter_count, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        native_method_invoke, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.reflect.Constructor (Phase 8 Part 3) ---
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameterTypes",
        "()[Ljava/lang/Class;",
        native_constructor_get_parameter_types, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getModifiers",
        "()I",
        native_constructor_get_modifiers, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        native_constructor_get_declaring_class, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "setAccessible",
        "(Z)V",
        lang_class::native_constructor_set_accessible, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameterCount",
        "()I",
        native_constructor_get_parameter_count, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "newInstance",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        native_constructor_new_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Class reflection: getDeclaredField(s), getDeclaredMethod(s), getDeclaredConstructor(s) ---
    registry.register(
        "java/lang/Class",
        "getDeclaredFields",
        "()[Ljava/lang/reflect/Field;",
        native_class_get_declared_fields, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredField",
        "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        native_class_get_declared_field, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredMethods",
        "()[Ljava/lang/reflect/Method;",
        native_class_get_declared_methods, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        native_class_get_declared_method, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredConstructors",
        "()[Ljava/lang/reflect/Constructor;",
        native_class_get_declared_constructors, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        native_class_get_declared_constructor, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Class: inherited enumeration (public members) ---
    registry.register(
        "java/lang/Class",
        "getFields",
        "()[Ljava/lang/reflect/Field;",
        native_class_get_fields, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getField",
        "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        native_class_get_field, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getMethods",
        "()[Ljava/lang/reflect/Method;",
        native_class_get_methods, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        native_class_get_method, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getConstructors",
        "()[Ljava/lang/reflect/Constructor;",
        native_class_get_constructors, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        native_class_get_constructor, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getInterfaces",
        "()[Ljava/lang/Class;",
        native_class_get_interfaces, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getModifiers",
        "()I",
        native_class_get_modifiers, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Annotation support (Phase 20) ---
    // Registered via shared helper so the natives are also installed in
    // real-JDK mode (register_essential_natives), not only in synthetic mode.
    register_annotation_overrides(registry);

    // --- Session 19: Generics / Type reflection ---
    registry.register(
        "java/lang/Class",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        lang_class::native_class_get_type_parameters, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getGenericSuperclass",
        "()Ljava/lang/reflect/Type;",
        lang_class::native_class_get_generic_superclass, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/Class",
        "getGenericInterfaces",
        "()[Ljava/lang/reflect/Type;",
        lang_class::native_class_get_generic_interfaces, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
        lang_class::native_method_get_generic_param_types, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getGenericReturnType",
        "()Ljava/lang/reflect/Type;",
        lang_class::native_method_get_generic_return_type, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        lang_class::native_method_get_type_parameters, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
        lang_class::native_field_get_generic_type, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.reflect.Modifier ---
    registry.register(
        "java/lang/reflect/Modifier",
        "isPublic",
        "(I)Z",
        native_modifier_is_public, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Modifier",
        "isPrivate",
        "(I)Z",
        native_modifier_is_private, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Modifier",
        "isProtected",
        "(I)Z",
        native_modifier_is_protected, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Modifier",
        "isStatic",
        "(I)Z",
        native_modifier_is_static, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Modifier",
        "isFinal",
        "(I)Z",
        native_modifier_is_final, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Modifier",
        "isAbstract",
        "(I)Z",
        native_modifier_is_abstract, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.reflect.Array ---
    register_reflect_array_natives(registry);

    // --- java.lang.reflect.Proxy ---
    register_reflect_proxy_natives(registry);

    // --- java.lang.reflect.AccessibleObject ---
    // setAccessible — instance setter. CratonVM's reflection layer does not
    // *enforce* the flag (all reflection behaves as `setAccessible(true)`
    // already, matching the JEP 403 relaxation), but the flag is still
    // readable: `isAccessible()` is plain bytecode over the `override` field,
    // and callers do round-trip it (`boolean prev = m.isAccessible();
    // m.setAccessible(true); …; m.setAccessible(prev);`). Dropping the write
    // made that read a lie. Store it — same slot the essential-natives
    // registration for Method/Field/Constructor writes.
    registry.register(
        "java/lang/reflect/AccessibleObject",
        "setAccessible",
        "(Z)V",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(None);
            };
            let flag = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // No-op when the layout has no `override` field, e.g. a synthetic
            // Field/Method built with positional slots only.
            ctx.set_field_by_name(this, "override", Value::Int(flag));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // canAccess(obj) — `true` is the consistent answer, not a stub: the JDK
    // returns true whenever the override flag is set OR the member passes the
    // module/visibility check, and CratonVM performs no access checks at all
    // (every reflective member behaves as `setAccessible(true)`, matching the
    // JEP 403 relaxation this VM adopts). Reporting `false` for anything would
    // contradict what the very next `Method.invoke` / `Field.get` will do.
    registry.register(
        "java/lang/reflect/AccessibleObject",
        "canAccess",
        "(Ljava/lang/Object;)Z",
        native_return_true, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.lang.Enum ---
    register_enum_natives(registry);

    // --- java.util.Objects ---
    register_objects_natives(registry, cratonvm_native_api::NativeKind::Intrinsic);

    // --- sun.misc.Unsafe ---
    register_unsafe_natives(registry);

    // --- java.util.concurrent.locks.LockSupport ---
    register_lock_support_natives(registry);

    // --- java.lang.ref (Phase 21 / T16.8) ---
    reference::register_reference_natives(registry);

    // --- java.util.concurrent (Phase 24) ---
    register_concurrent_natives(registry);

    // --- Stream-terminal / Flow.Subscriber overrides (T16.9) ---
    // Must run AFTER register_concurrent_natives so that it overrides the
    // no-op `Flow.Subscription.request(J)V` default with a saturating-add
    // demand counter.
    streams::register_stream_overrides(registry);

    // --- Phase 25: Utility classes ---
    register_base64_natives(registry);
    register_charset_natives(registry);
    // Real-JDK charset paths must replace the older phase stubs above. In
    // particular, CharsetEncoder must preserve configured REPORT/REPLACE
    // actions rather than unconditionally returning UNDERFLOW.
    charset::register_real_charset_natives(registry);

    // --- Phase 27: java.math ---
    register_biginteger_natives(registry);
    register_bigdecimal_natives(registry);

    // --- Phase 28: java.util.concurrent extended ---
    register_executor_natives(registry);
    register_completable_future_natives(registry);

    // --- Phase 29: java.text ---
    register_number_format_natives(registry);

    // --- Phase 30: java.net ---
    register_net_natives(registry);

    // DEPRECATED (Session 15): java.util.logging - synthetic stubs, only used in synthetic-jdk mode.
    // In real JDK mode, logging classes load from JDK bytecode.
    // --- Phase 31: java.util.logging + Locale ---
    register_logging_natives(registry);
    register_slf4j_natives(registry);
    register_locale_natives(registry);

    // --- Phases 33-37: Enterprise features ---
    register_security_natives(registry);
    register_rwlock_natives(registry);
    register_atomic_extras_natives(registry);
    register_functional_extras_natives(registry);
    register_enterprise_natives(registry);

    // --- Phase 38: java.time completion ---
    register_time_extras_natives(registry);
    // --- T2.5 (session 58): Clock, Year, Month, DayOfWeek, ZoneRules,
    //     Duration.between, LocalDate/LocalDateTime/ZonedDateTime .now(Clock),
    //     DateTimeFormatter.ofPattern(String, Locale), ZoneId.systemDefault
    //     backed by tzdata/env. See util_time.rs:register_t25_natives.
    register_t25_natives(registry);

    // --- Phases 42-46: Enterprise completion ---
    register_exception_extras_natives(registry);
    register_java_lang_extras_natives(registry);
    register_functional_completion_natives(registry);

    // --- Phases 47-50: Enterprise final ---
    register_enterprise_final_natives(registry);

    // --- Phase 50: ThreadLocal, StringTokenizer, BitSet, EnumSet, EnumMap ---
    register_phase50_natives(registry);

    // --- Phase 51: Calendar, TimeZone, Currency, Concurrency extras ---
    register_phase51_natives(registry);

    // --- Phase 52: java.time enhancements, URLEncoder/Decoder, MessageFormat, MathContext ---
    register_phase52_natives(registry);

    // --- Phase 53: javax.crypto, java.security, java.net, ServiceLoader, Record ---
    register_phase53_natives(registry);

    // --- WP6.3: javax.crypto.Cipher class-init shim ---
    // Real-JDK `Cipher.<clinit>` chain transitively reads
    // `${java.home}/conf/security/java.security` via
    // `java.security.Security.<clinit>`, which fails with
    // `IOException("Is a directory")` in our boot-strap.  No-op the
    // <clinit> chain so the existing Cipher.getInstance native
    // dispatch in phases_early.rs reaches the AES/GCM backend.
    crate::jca::cipher::register_cipher_clinit_shim(registry);

    // --- Phase 54: Atomics, MethodHandle, Logging extras, URI/HTTP, Zip ---
    register_phase54_natives(registry);

    // Re-register correct Atomic natives AFTER phase54 (phase54 versions have
    // a bug: void methods return Ok(Some(null)) instead of Ok(None), which
    // pushes a spurious null onto the operand stack).
    register_atomic_integer_natives(registry);
    register_atomic_long_natives(registry);
    register_atomic_reference_natives(registry);
    register_atomic_boolean_natives(registry);

    // --- Phase 55: Charset, Executors, Reflect, Collection extras ---
    register_phase55_natives(registry);

    // --- Phase 56: Stream enhancements, Collectors, Summary Stats, function extras ---
    register_phase56_natives(registry);

    // --- Phase 57: NIO File extras, ProcessBuilder/Process, java.text ---
    register_phase57_natives(registry);

    // --- Phase 58: CompletableFuture expansion, NIO Channels, GZIP/Zip, Pushback, Charset coders ---
    register_phase58_natives(registry);

    // --- Phase 59: Management, JAR, Spliterator, VarHandle, Package, StackWalker, File attrs, Module ---
    register_phase59_natives(registry);

    // --- Phase 60: HTTP Client, Flow, MatchResult, CallSite, Record, ProcessHandle, AbstractMap ---
    register_phase60_natives(registry);

    // --- Phase 61: Text formatting, Logging, Charset, ClassLoader, Reflect, Files/Path, Net ---
    register_phase61_natives(registry);

    // --- Phase 62: CharBuffer, Time expansion, Format factories, NavigableMap/Set, StampedLock, ZipEntry ---
    register_phase62_natives(registry);

    // --- Phase 63: WeakHashMap, ResourceBundle, ServiceLoader, MethodHandles.Lookup, ScheduledExecutor, Formatter, Enumeration ---
    register_phase63_natives(registry);

    // --- Phase 64: SequencedCollection/Map/Set (Java 21), HexFormat, Stream.toList, Collectors.teeing, RandomGenerator, Math.clamp ---
    register_phase64_natives(registry);

    // --- Phase 65: PriorityBlockingQueue, DelayQueue, CompletionService, MethodHandles factories, Stream.mapMulti, Pattern, DTFB ---
    register_phase65_natives(registry);

    // --- Phase 66: Collator, BreakIterator, FileVisitor, WatchService, ConstantDesc, Thread.Builder, PushbackReader ---
    register_phase66_natives(registry);

    // --- Phase 67: StructuredTaskScope, ScopedValue, Gatherer, AsyncChannels, ForeignMemory, StringTemplate ---
    register_phase67_natives(registry);

    // --- Phase 9.1 / NEW-13 (pre-p68): Baseline javax.net.ssl stubs from tls.rs.
    // Registered BEFORE phase68 so that register_p68_ssl's real native-tls-backed
    // implementations (SSLSocketFactory.createSocket, SSLContext.init, etc.) take
    // precedence via last-writer-wins in the native method registry. tls.rs
    // therefore provides the fallback coverage for SSLEngine/SSLParameters/
    // TrustManagerFactory methods that phase68 doesn't override.
    register_tls_natives(registry);

    // --- Phase 68: javax.crypto.Mac, SSL, security.cert, JDBC, XML, invoke extras ---
    register_phase68_natives(registry);

    // --- Phase 69: Cleaner, Spliterator, WebSocket, SwitchBootstraps, CompactNumberFormat, SubmissionPublisher ---
    register_phase69_natives(registry);

    // --- Phase 70: FileTime, GZIP, ObjectStreams, ConstantBootstraps, AtomicAccumulators, EnumMap/EnumSet ---
    register_phase70_natives(registry);

    // --- Phase 71: Wrapper extras, BigInteger ext, Files bridge, Thread extras, Zip, Logging ---
    register_phase71_natives(registry);

    // --- Phase 72: Preferences, Beans, JNDI, Datagram, HttpServer, ServerSocket extras ---
    register_phase72_natives(registry);
    // Phase 72's raw-slot DatagramSocket overlay is synthetic-only. Reassert
    // the moving-GC-safe side-table implementation afterwards so it is the
    // final owner of every overlapping native key.
    net_phase_e::register_re7_datagram_socket(registry);

    // --- NEW-15: Virtual threads / Loom (JEP 444 / 491) ---
    // Continuation, ContinuationScope, ForkJoinPool.commonPool.
    crate::phases_late::register_new15_loom(registry);

    // --- T16.7 `concurrent_extras` (ForkJoinPool `execute` x2,
    // `externalSubmit`, `getRunningThreadCount`, `getStealCount`,
    // `hasQueuedSubmissions`): DELETED in interpreter round i4 wave 84, lane
    // L6. Synthetic-only (this arm), it overwrote the shipping
    // `register_essential_natives_with_shims` /
    // `register_real_jdk_forkjoin_essentials` bodies of the first three with
    // copies that skipped `fjp_reject_submission` (no NPE / rejection) and
    // the pool frame, and the last three answered from a process-global
    // four-thread pool no task ever ran on -- and the registry drops them
    // anyway unless `CRATONVM_SYNTHETIC_FORKJOINPOOL` is set.

    // --- T19.3: Quarkus static-init replay (RuntimeValue, StartupContext,
    // ApplicationConfig, DataSourceRuntimeConfig, Timing). Drives
    // io.quarkus.runtime.generated.Main.main's lazy-materialization +
    // shutdown-task bookkeeping during KC26 boot.
    crate::quarkus_staticinit::register_quarkus_staticinit_natives(registry);

    // --- T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater /
    // AtomicLongFieldUpdater.newUpdater native overrides (unblocks
    // org.jboss.logmanager.ExtHandler.<clinit>).
    crate::atomic_updater::register_atomic_updater_natives(registry);

    // --- T19.4: Quarkus ArC CDI container (Arc.initialize/container/shutdown,
    // ArcContainer.instance/beanManager/isRunning/select/requestContext,
    // InstanceHandle.get/isAvailable/destroy, BeanManager.createInstance/
    // getBeans/getReference, InjectableBean.get/getScope/getBeanClass).
    crate::quarkus_arc::register_quarkus_arc_natives(registry);

    // --- Phase D: Java 25 — Stream gatherers, Scoped values, Structured concurrency ---
    register_phase_d_natives(registry);

    // --- Phase E: Panama FFI (JEP 454) ---
    register_pe_panama(registry);

    // --- R3: Resource Loading — Class.getResourceAsStream, InputStreamReader, BufferedReader ---
    register_r3_resource_loading(registry);
    servlet::register_spring_mock_response_natives(registry);
    servlet::register_script_engine_manager_natives(registry);
    servlet::register_jython_thread_state_natives(registry);
    servlet::register_jython_pyobject_natives(registry);
    servlet::register_jython_imp_natives(registry);

    // --- S1: URLClassLoader + ServiceLoader (real META-INF/services discovery) ---
    register_s1_classloading(registry);

    // --- S2: Real ByteBuffer (backed byte[]) + NIO Socket I/O (TcpStream/TcpListener) ---
    register_s2_nio(registry);

    // --- S3: Real HttpClient (TCP-backed HTTP/1.1 GET/POST) ---
    register_s3_http_client(registry);

    // --- T4: MethodHandle invoke/invokeExact/invokeWithArguments dispatch ---
    register_t4_method_handle_invoke(registry);

    // --- T2.8: MethodHandle completeness (unreflect, permuteArguments, guardWithTest) ---
    crate::lang_invoke::register_t28_method_handle_completeness(registry);

    // --- Phase 8.4: JMX (Java Management Extensions) ---
    #[cfg(feature = "management")]
    {
        if let Some(h) = crate::hooks::JMX_HOOKS.get() { (h.register_jmx_natives)(registry); }
        // Pure-synthetic-JDK mode has no real `java.management` module, so the
        // in-process synthetic MBeanServer (createMBeanServer / getAttribute /
        // registerMBean / queryNames …) is required here. Real-JDK boot paths
        // (vm_init.rs) deliberately call `register_jmx_natives` WITHOUT this so
        // the concrete `com.sun.jmx.mbeanserver.JmxMBeanServer` bytecode runs.
        if let Some(h) = crate::hooks::JMX_HOOKS.get() { (h.register_mbean_server)(registry); }
    }

    // --- Phase 9.2: HTTP/2 Client (java.net.http.*) ---
    register_http2_natives(registry);

    // --- Phase J: Jackson ObjectMapper + Gson (M16) ---
    register_jackson_gson_natives(registry);

    // --- Phase 10: ClassLoader hierarchy ---
    classloader::register_classloader_natives(registry);

    // --- WP2.3-B: real defineClass surface for Unsafe + Lookup. Must
    // come AFTER classloader::register_classloader_natives so the
    // WP2.3-B Lookup overrides replace the simpler implementations.
    // Re-routes Unsafe.defineClass / defineClass0 / defineAnonymousClass
    // and Lookup.defineClass / defineHiddenClass / defineHiddenClassWithClassData
    // through `define_class_full` with skip_verification + nest-host +
    // CodeSource inheritance from the calling context.
    unsafe_natives::register_unsafe_define_class(registry);
    lookup_define::register_lookup_define_class(registry);

    // --- Phase 11: Class-File API (JEP 484) ---
    classfile_api::register_classfile_api_natives(registry);

    // --- Phase 13.1: Java Serialization ---
    //
    // NOT gated on `experimental-serialization` any more, and the gate was not
    // a policy choice — it was a hole. This call site is already inside
    // `register_synthetic_overrides`, which is `#[cfg(feature =
    // "synthetic-jdk")]`, so it can only run in a synthetic-library build; and
    // in that build `java/io/ObjectOutputStream` / `ObjectInputStream` are
    // fabricated stub classes with NO bytecode behind them. Registering
    // nothing therefore did not "disable an experiment", it left the two
    // classes present-but-inert: `writeObject` succeeded, wrote nothing,
    // `readObject` handed back a blank instance, and every field read back
    // null (`NullPointerException: Cannot read field 'intValue' because the
    // object is null`), while `writeObject(new NotSerializable())` raised no
    // `NotSerializableException` at all.
    //
    // That is exactly what
    // synthetic-jdk-class-library-gaps-FIXED-20260811.md
    // filed as four "serialization gaps" in the synthetic class library. The
    // implementation they were said to be missing is `serialization.rs`, all
    // 7.6k lines of it, already compiled into this build (the module's own
    // `#[cfg]` is `any(experimental-serialization, synthetic-jdk)`) — the
    // registrations were the only thing the feature flag still withheld. The
    // real-JDK path is unaffected: it never calls this function, and its own
    // entry point (`register_reflection_factory_serialization`, from
    // `register_essential_natives_with_shims`) keeps its
    // `experimental-serialization` gate.
    serialization::register_serialization_natives(registry);

    // `ByteArrayOutputStream` intrinsic. The interpreter mis-resolves inherited
    // `getfield`/`putfield` slots for `ByteArrayOutputStream` *subclasses*
    // (e.g. `sun.security.util.DerOutputStream`), so real `write`/`toByteArray`
    // bytecode silently drops bytes — which produced empty DER output and broke
    // ECDSA signature encoding under real JCA. The intrinsic addresses `buf`/
    // `count` by name (consistent with reflection), so it is correct for any
    // subclass.
    //
    // G54-1 NOMINATION 5: `serialization::register_byte_array_output_stream`
    // used to be called here. It registers nothing — TOMBSTONE-verdicted
    // (`serialization.rs`'s own doc on the function measures why: every one
    // of the twelve registrations it used to make was already overwritten by
    // `cratonvm_native_io::register_io_natives`'s superset before any
    // bytecode ran, in every build that ever compiled them) — so removing the
    // call changes nothing about what this registry contains.
    // `java/io/ByteArrayOutputStream` is served in every mode by
    // `register_io_natives`, called immediately after `register_synthetic_overrides`
    // returns in `vm_init.rs`'s `config.use_synthetic_jdk` arm.

    // --- Phase 14.1: CDS/AppCDS ---
    #[cfg(feature = "experimental-aot")]
    cds::register_cds_natives(registry);

    // --- Phase 14.2: AOT / Project Leyden ---
    #[cfg(feature = "experimental-aot")]
    aot::register_aot_natives(registry);

    // --- Phase 15: JDK 25 Language & API Features ---
    jdk25_concurrency::register_jdk25_concurrency_natives(registry);
    jdk25_patterns::register_jdk25_patterns_natives(registry);
    jdk25_language::register_jdk25_language_natives(registry);
    vector_api::register_vector_api_natives(registry);

    // --- Phase 17.2: GraalVM Native Image Compatibility ---
    graalvm_compat::register_graalvm_compat_natives(registry);
    // LETSGO_S2: real-JDK bootstrap compatibility fallbacks used by
    // letsgo-main and related Spring/SLF4J startup paths.
    letsgo_compat::register_letsgo_compat_natives(registry);

    // Spring ApplicationStartup / StartupStep: registered once in
    // `spring_startup_bootstrap::register` inside `register_essential_natives`.
    // (A duplicate registration here used to overwrite those natives with an
    // ArrayList layout that mismatched real-JDK field order — post-CCE AV on
    // letsgo nojit.)

    // --- Phase 19.2: Real Crypto Primitives ---
    // Always registered (real RustCrypto primitives), independent of the
    // synthetic-shim feature.
    crate::crypto_impl::register_crypto_impl_natives(registry);

    // --- M18: ConcurrentHashMap atomic ops, LinkedBlockingQueue, ArrayBlockingQueue, Phaser improvements ---
    register_m18_concurrent_fixes(registry);
    // --- T3.1: ConcurrentSkipListMap, LinkedTransferQueue, Flow ---
    register_t31_concurrent_extras(registry);

    // --- T3.8: JNDI (javax.naming) ---
    t3_impl::register_t38_jndi(registry);
    // --- T3.9: StAX XMLEventReader + SchemaFactory ---
    t3_impl::register_t39_stax(registry);
    // --- T3.10: ScriptEngineManager (javax.script) ---
    t3_impl::register_t310_scripting(registry);
    // --- T3.11: Internationalization extras ---
    t3_impl::register_t311_i18n(registry);
    // --- T3.12-T3.15: Tooling natives ---
    t3_impl::register_t312_tooling(registry);
    // --- T3.1.16-T3.1.18: Structured concurrency ---
    t3_impl::register_t31_structured_concurrency(registry);

    // Note: SecurityManager and deprecated API natives (T8) are now registered in
    // register_essential_natives so they are always available regardless of feature flag.
    // so they are always available regardless of synthetic-jdk feature flag.

    // --- Quarkus bootstrap: RunnerClassLoader.close() ---
    // KEEP: correct no-op *for this mode only*, and NOT a duplicate of the
    // real implementation. This registrar is `#[cfg(feature = "synthetic-jdk")]`
    // and is reached only through `register_builtins`, which `vm_init.rs`
    // calls under `if config.use_synthetic_jdk`. In that mode
    // `RunnerClassLoader` is a ONE-slot synthetic placeholder holding just
    // `parent` (`classloading/src/class_manager.rs`, the
    // "RunnerClassLoader = 1-slot placeholder" arm) — there is no
    // `resourceDirectoryMap` and no jar handle, so there is genuinely nothing
    // to close.
    // The real-JDK branch is the sibling `else`, and it registers the real
    // `quarkus_runner_class_loader_close` (see `vm/src/vm/vm_init.rs`), which
    // walks `resourceDirectoryMap` and closes the jar resources while skipping
    // the null holes that used to NPE and mask `doRun`'s real exception.
    // The two never coexist; do not "deduplicate" them.
    registry.register(
        "io/quarkus/bootstrap/runner/RunnerClassLoader",
        "close",
        "()V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- T19.H3 (final override after all phase-* registrations): re-register
    // LogManager singleton natives so the per-phase fallback LogManager.getLogManager
    // stubs (register_logging_natives, register_p61_logging) don't silently
    // win. The singleton path is the ONLY path that returns a stable
    // ObjectRef for the KC26 ClassCastException fix.
    logmanager::register_logmanager_natives(registry);

    // WP7.1 — re-register the classpath-walking ServiceLoader natives in
    // synthetic-JDK mode so `register_phase53_service_loader` /
    // `register_p63_service_loader` (which install empty-iterator stubs)
    // do not silently win over the proper SPI implementation. Mirrors
    // the unconditional registration in `register_essential_natives`.
    jdbc::register_jdbc_driver_natives(registry);

    let after = registry.len();
        tracing::info!(count = after - before, "Registered synthetic overrides");
}

// ---------------------------------------------------------------------------
// Registration helpers (to avoid repetition for StringBuilder/Buffer, Math/StrictMath)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// No-op natives (registerNatives, etc.)
// ---------------------------------------------------------------------------

pub fn native_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn native_platform_filesystem_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let file_sep = ctx
        .get_system_property("file.separator")
        .or_else(|| system_property_fallback(ctx, "file.separator"))
        .unwrap_or_else(|| std::path::MAIN_SEPARATOR.to_string());
    let path_sep = ctx
        .get_system_property("path.separator")
        .or_else(|| system_property_fallback(ctx, "path.separator"))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                ";".to_string()
            } else {
                ":".to_string()
            }
        });
    let user_dir = ctx
        .get_system_property("user.dir")
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());

    let slash = file_sep.chars().next().unwrap_or(std::path::MAIN_SEPARATOR) as i32;
    let semicolon = path_sep
        .chars()
        .next()
        .unwrap_or(if cfg!(windows) { ';' } else { ':' }) as i32;
    let alt_slash = if slash == ('\\' as i32) {
        '/' as i32
    } else {
        '\\' as i32
    };

    // Works for both WinNTFileSystem and UnixFileSystem (fields absent in one
    // class are ignored by the field-by-name setter implementation).
    // WinNTFileSystem names its path-separator field `semicolon`;
    // UnixFileSystem names the same concept `colon` (tomcat-08-07 follow-up:
    // this used to only set `semicolon`, which silently no-ops on
    // UnixFileSystem, so `colon` stayed at its zero-init default and any
    // pure-Java FileSystem method that reads it, e.g. path joining, saw a
    // NUL separator on Linux). Set both by the same value; each is a no-op
    // on the class that doesn't declare it.
    //
    // gc-common w20-c: the `userDir` string is allocated before the stores,
    // with `this` pinned across it (it was written after the allocation).
    let this_pin = ctx.pin_native_root(this);
    let user_dir_str = ctx.create_string(&user_dir);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "slash", Value::Int(slash));
    ctx.set_field_by_name(this, "semicolon", Value::Int(semicolon));
    ctx.set_field_by_name(this, "colon", Value::Int(semicolon));
    ctx.set_field_by_name(this, "altSlash", Value::Int(alt_slash));
    ctx.set_field_by_name(this, "userDir", Value::Object(Some(user_dir_str)));
    Ok(None)
}

pub fn native_lazy_launcher_discover(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    const LAZY: &str = "org/apache/maven/surefire/junitplatform/LazyLauncher";
    const FACTORY: &str = "org/junit/platform/launcher/core/LauncherFactory";
    const DESC_GET: &str = "()Lorg/junit/platform/launcher/Launcher;";
    const DESC_DISCOVER: &str =
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;";

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("LazyLauncher.discover: null receiver".into()),
            }
            .into());
        }
    };
    let req = match args.get(1).cloned() {
        Some(v) => v,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "LazyLauncher.discover: missing LauncherDiscoveryRequest".into(),
            }
            .into());
        }
    };

    let materialize_from_factory = |ctx: &mut dyn NativeContext| -> Result<
        cratonvm_types::ObjectRef,
        cratonvm_types::error::MethodCallFailed,
    > {
        match ctx.invoke(FACTORY, "create", DESC_GET, &[]) {
            Ok(Some(Value::Object(Some(d)))) => Ok(d),
            Ok(other) => Err(RuntimeError::IllegalStateException {
                message: format!(
                    "LazyLauncher.discover: LauncherFactory.create() expected Launcher, got {other:?}"
                ),
            }
            .into()),
            Err(e) => Err(e),
        }
    };

    // Pin the request across the launcher() / LauncherFactory.create() calls
    // below — a moving young GC there would relocate it out from under this
    // raw Rust local (native stale-local family). The dispatcher truncates
    // the pin stack when this native returns.
    let req_pin = match req {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    let delegate =
        match ctx.invoke_special(LAZY, "launcher", DESC_GET, &[Value::Object(Some(this))]) {
            Ok(Some(Value::Object(Some(d)))) => d,
            Ok(Some(Value::Object(None))) | Ok(None) => materialize_from_factory(ctx)?,
            Ok(Some(_)) => materialize_from_factory(ctx)?,
            Err(_) => materialize_from_factory(ctx)?,
        };

    let req_cur = match req_pin {
        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        None => req,
    };
    ctx.invoke_virtual(delegate, "discover", DESC_DISCOVER, &[req_cur])
}

pub fn eprint_java_throwable(ctx: &mut dyn NativeContext, label: &str, exc: ObjectRef) {
    // gc-common w20-c: `toString` is Java, and `exc` is used again after it.
    let exc_pin = ctx.pin_native_root(exc);
    let text = ctx.invoke_virtual(exc, "toString", "()Ljava/lang/String;", &[]);
    let exc: ObjectRef = ctx.read_native_pin(exc_pin, exc);
    ctx.unpin_native_roots(exc_pin);
    match text {
        Ok(Some(Value::Object(Some(s)))) => {
            let msg = ctx.read_string(s).unwrap_or_default();
            eprintln!("[SUREFIRE-RUN] {label}: {msg}");
        }
        Ok(other) => {
            eprintln!("[SUREFIRE-RUN] {label}: toString unexpected {other:?}");
        }
        Err(e) => {
            eprintln!("[SUREFIRE-RUN] {label}: toString invoke failed: {e:?}");
        }
    }
    let _ = ctx.invoke_virtual(exc, "printStackTrace", "()V", &[]);
}

/// Exception <init>(Ljava/lang/String;)V — sets detailMessage (field 0)
/// Exception <init>(Ljava/lang/String;Ljava/lang/Throwable;)V
/// Exception <init>(Ljava/lang/Throwable;)V — sets cause (field 1)
pub fn native_return_false(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0))) // false
}

/// Default JVM-wide assertion status reported by `Class.desiredAssertionStatus`
/// / `desiredAssertionStatus0`. Real HotSpot returns `false` unless `-ea` is on;
/// CratonVM mirrors that, but honours `CRATONVM_ENABLE_ASSERTIONS=1` so test
/// harnesses can reproduce Surefire's default (Surefire forks the test JVM with
/// `-ea`). When enabled, `<clinit>` stores `$assertionsDisabled = false`, so
/// `assert` statements in real bytecode (e.g. keycloak JWKUtil.toIntegerBytes)
/// actually throw `AssertionError` on violation instead of being no-ops.
pub fn assertion_status_default() -> i32 {
    if crate::nbflags().enable_assertions {
        1
    } else {
        0
    }
}

/// `Class.desiredAssertionStatus0(Class)` / `desiredAssertionStatus()` honouring
/// the `CRATONVM_ENABLE_ASSERTIONS` flag — see [`assertion_status_default`].
/// `ClassLoader.setDefaultAssertionStatus(boolean)`: the field write of the
/// JDK body, without its `synchronized (assertionLock)` (see the registration).
pub fn native_cl_set_default_assertion_status_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first() else {
        return Ok(None);
    };
    let enabled = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field_by_name(*this, "defaultAssertionStatus", Value::Int(enabled));
    Ok(None)
}

/// [`native_cl_set_default_assertion_status_field`] after the JDK body's
/// `if (classAssertionStatus == null) initializeJavaAssertionMaps();`
/// (interpreter round i1 wave 51, lane L3; `lang_class::reflection_jdk_bodies`).
/// The private initializer is JDK bytecode (it reads the `-ea` directives
/// through the `retrieveDirectives` native); `this` is pinned across it.
pub fn native_cl_set_default_assertion_status_with_maps(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    if matches!(
        ctx.get_field_by_name(this, "classAssertionStatus"),
        Value::Object(None) | Value::Int(0)
    ) {
        let pin = ctx.pin_native_root(this);
        let result = ctx.invoke_special(
            "java/lang/ClassLoader",
            "initializeJavaAssertionMaps",
            "()V",
            &[Value::Object(Some(this))],
        );
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        result?;
        let mut rest = vec![Value::Object(Some(this))];
        rest.extend_from_slice(args.get(1..).unwrap_or(&[]));
        return native_cl_set_default_assertion_status_field(ctx, &rest);
    }
    native_cl_set_default_assertion_status_field(ctx, args)
}

/// `JVM_DesiredAssertionStatus` for the class `mirror` reflects: `false` for
/// a primitive or array class, else `JavaAssertions::enabled(name, loader ==
/// null)` over the owning VM's directives (`NativeContext::assertion_directives`;
/// interpreter round i5 wave 107, lane L5,
/// `interpreter-i106-L5-assertion-switches-are-one-global-FIXED-20261016.md`). A boot-loader
/// class follows `-esa` / `-dsa` only; every other loader (the platform loader
/// included, as in HotSpot) follows `-ea` / `-da`, whose fallback is
/// [`assertion_status_default`] (`CRATONVM_ENABLE_ASSERTIONS`).
pub fn desired_assertion_status_of_mirror(
    ctx: &dyn NativeContext,
    mirror: Option<ObjectRef>,
) -> bool {
    let Some(mirror) = mirror else {
        return false;
    };
    let Some(class_id) = ctx.class_id_from_mirror(mirror) else {
        return false;
    };
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return false;
    };
    if name.starts_with('[') {
        return false;
    }
    let system_class = ctx.loader_id_of_class(class_id) == 0;
    let user_fallback = assertion_status_default() != 0;
    match ctx.assertion_directives() {
        Some(directives) => directives.enabled(&name, system_class, user_fallback),
        None => !system_class && user_fallback,
    }
}

/// `Class.desiredAssertionStatus0(Class)` (the class is the argument) and the
/// synthetic `Class.desiredAssertionStatus()` stand-in (the class is the
/// receiver): see [`desired_assertion_status_of_mirror`].
pub fn native_assertion_status(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mirror = match args.first() {
        Some(Value::Object(m)) => *m,
        _ => None,
    };
    Ok(Some(Value::Int(i32::from(desired_assertion_status_of_mirror(
        &*ctx, mirror,
    )))))
}

pub fn native_return_zero(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// Shared body for `java.util.logging.Logger`'s level-named convenience
/// methods (`info`/`warning`/`severe`/`fine`/`finer`/`finest`/`config`, in
/// both the `(String)V` and `(Supplier<String>)V` forms).
///
/// The real bodies are one-liners — `log(Level.INFO, msg)` — so that is
/// exactly what this does, delegating to the `log(Level, String)` /
/// `log(Level, Supplier)` natives `logmanager::register_logmanager_natives`
/// installs. Those carry the real level filtering, LogRecord construction and
/// handler delivery, so routing through them keeps one implementation of the
/// JUL pipeline instead of a second, divergent copy.
///
/// A closure passed to `NativeMethodRegistry::register` must coerce to a bare
/// `fn` pointer and therefore cannot capture, which is why the level name
/// arrives as a parameter from fourteen non-capturing call sites rather than
/// from a loop variable.
///
/// GC: `resolve_standard_level` allocates (it can mint the `Level` constant),
/// so the receiver and the message/supplier argument are pinned across it and
/// re-read through their pins before being handed to `invoke_virtual`.
pub fn jul_convenience_log(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level_name: &str,
    supplier: bool,
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let payload = args.get(1).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let payload_pin = match payload {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let level = crate::logmanager::resolve_standard_level(ctx, level_name);
    let this = ctx.read_native_pin(this_pin, this);
    let payload = match payload_pin {
        Some((pin, obj)) => Value::Object(Some(ctx.read_native_pin(pin, obj))),
        None => payload,
    };
    ctx.unpin_native_roots(this_pin);
    let Some(level) = level else {
        // No Level constant available: drop rather than fabricate a record
        // with a null level, which `Handler.isLoggable` would reject anyway.
        return Ok(None);
    };
    let desc = if supplier {
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V"
    } else {
        "(Ljava/util/logging/Level;Ljava/lang/String;)V"
    };
    // Nothing allocates between the reads above and this call, so `level`,
    // `this` and `payload` are all still current.
    ctx.invoke_virtual(this, "log", desc, &[Value::Object(Some(level)), payload])?;
    Ok(None)
}

/// `java.security.ProtectionDomain.implies(Permission)`.
///
/// Evaluated against the same parsed-policy core that backs
/// `SecurityManager.checkPermission` and the `Policy.implies(pd, perm)`
/// native (`security_manager::policy_allows_full_generic`), using the
/// currently-active privileged frame's codeBase + signer digests so
/// `grant codeBase "…"` / `grant signedBy "…"` clauses match the same way in
/// all three places. When no policy file is installed the core is allow-all,
/// so the common case still answers `true`.
///
/// Permission layout convention (shared with `security_manager.rs`):
/// field 0 = name/target, field 1 = actions.
pub fn native_protection_domain_implies(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Instance method: [this(ProtectionDomain), permission]. A call shape with
    // no permission argument has nothing to evaluate — keep the historical
    // permissive answer rather than inventing a denial.
    if args.len() < 2 {
        return Ok(Some(Value::Int(1)));
    }
    // A STATIC domain answers from its OWN permission collection and never
    // consults the policy. `ProtectionDomain(CodeSource, PermissionCollection)`
    // -- the two-argument constructor -- sets `staticPermissions = true`, and
    // the JDK's `implies` is then
    //
    //     if (hasAllPerm) return true;
    //     if (permissions != null) return permissions.implies(perm);
    //     return false;
    //
    // Falling through to the policy core instead meant a domain constructed
    // with NO permissions answered TRUE for `AllPermission`, because the core
    // is allow-all with no `java.policy` loaded. **That is a security answer
    // given the permissive way for a reason that has nothing to do with the
    // domain being asked about**, and it is the one shape of wrong answer this
    // predicate must not produce. MEASURED by
    // `SecuritySurfaceSweep.java`:
    // `new ProtectionDomain(null, null).implies(new AllPermission())` is
    // `false` on HotSpot and was `true` here.
    if let Some(Value::Object(Some(pd))) = args.first() {
        if matches!(
            ctx.get_field_by_name(*pd, "staticPermissions"),
            Value::Int(1)
        ) {
            if matches!(ctx.get_field_by_name(*pd, "hasAllPerm"), Value::Int(1)) {
                return Ok(Some(Value::Int(1)));
            }
            let Value::Object(Some(perms)) = ctx.get_field_by_name(*pd, "permissions") else {
                return Ok(Some(Value::Int(0)));
            };
            let perm = args.get(1).copied().unwrap_or(Value::Object(None));
            return match ctx.invoke_virtual(
                perms,
                "implies",
                "(Ljava/security/Permission;)Z",
                &[perm],
            ) {
                Ok(Some(v @ Value::Int(_))) => Ok(Some(v)),
                // The collection could not be asked: deny rather than fall
                // through to the allow-all core, which is the failure mode
                // this arm exists to remove.
                _ => Ok(Some(Value::Int(0))),
            };
        }
    }
    // A null permission carries no grant to satisfy: deny, matching the
    // `Policy.implies` native's null handling (security_manager.rs).
    let perm = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(perm))
        .unwrap_or_default();
    let target = match ctx.get_field(perm, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let actions = match ctx.get_field(perm, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let code_base = crate::security_manager::current_privileged_code_base_arc();
    let cert_digests = crate::security_manager::current_privileged_cert_digests_arc();
    let allowed = crate::security_manager::policy_allows_full_generic(
        &class_name,
        &target,
        &actions,
        code_base.as_deref(),
        &cert_digests,
    );
    Ok(Some(Value::Int(allowed as i32)))
}

/// Whether the given **OS** descriptor carries `O_APPEND`.
///
/// Only ever called for 0/1/2 — see `native_file_descriptor_get_append`.
#[cfg(unix)]
pub fn os_fd_is_append(fd: i32) -> bool {
    // SAFETY: `F_GETFL` only reads the descriptor's status flags; it touches
    // no memory and returns -1 (with errno set) for a closed/invalid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    flags >= 0 && (flags & libc::O_APPEND) != 0
}

#[cfg(not(unix))]
pub fn os_fd_is_append(_fd: i32) -> bool {
    // Windows has no `O_APPEND` status flag on a HANDLE; the JDK's own Windows
    // build tracks append mode on the stream, not on the descriptor.
    false
}

/// `java.io.FileDescriptor.getAppend(int fd)` — static.
///
/// W4: was a hard `false` justified as "CratonVM never opens an O_APPEND
/// descriptor". That was wrong — `FileDescriptorTable::open_write(path, true)`
/// passes `.append(true)` straight to the OS. What is true is that the `fd`
/// value handed to this native is a `fd_table` id, NOT an OS descriptor, for
/// everything except 0/1/2 (see the `getHandle` native above, which maps
/// exactly that range through and answers -1 otherwise). Handing a table id to
/// `fcntl` would report the flags of an unrelated real descriptor, so only the
/// three standard streams are queried for real — and they are the only ones
/// the JDK asks about anyway: `FileDescriptor(int)` is the sole caller and it
/// runs for in/out/err, where a shell's `>>` genuinely does hand us O_APPEND.
///
/// ESCALATION (reported, not worked around here): a faithful answer for the
/// remaining descriptors needs `FileDescriptorTable` to record the `append`
/// flag `open_write` already receives and expose it as
/// `fd_table().is_append(fd) -> Option<bool>` (`native-api/src/fd_table.rs`).
pub fn native_file_descriptor_get_append(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let fd = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let append = (0..=2).contains(&fd) && os_fd_is_append(fd);
    Ok(Some(Value::Int(append as i32)))
}

/// `Field/Method/Constructor.getRoot()` — the receiver's own `root` field.
///
/// W4: was a hard `null` for all three. Null is still the normal answer (our
/// `copy()` natives hand back the SAME object rather than minting a linked
/// copy, so nothing sets `root`), but reading the field means a reflective
/// object that *did* acquire a root — e.g. real `AccessibleObject.copy()`
/// bytecode running in a configuration where the `copy` native is not the
/// winner — reports it instead of lying. `get_field_by_name` answers
/// `Object(None)` for a layout with no such field, so synthetic Field/Method
/// objects keep the old behaviour exactly.
///
/// `AccessibleObject.getRoot()` deliberately keeps its literal `null`: that IS
/// the JDK body for the base class.
pub fn native_reflect_get_root(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.get_field_by_name(this, "root") {
        root @ Value::Object(Some(_)) => Ok(Some(root)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `java.lang.reflect.Executable.getParameters0()`.
///
/// W4: was a hard `null`. Now returns a real `Parameter[]` whenever the
/// declaring class file actually carries a `MethodParameters` attribute
/// (JVMS 4.7.24), and `null` otherwise.
///
/// The null half is load-bearing rather than a leftover stub:
/// `Executable.privateGetParameters` sets `hasRealParameterData = true` for
/// ANY non-null result, and that flag is what `Parameter.isNamePresent()`
/// reports. Synthesizing `arg0`/`arg1`/… here would make `isNamePresent()`
/// lie; returning null makes the JDK synthesize the very same names itself
/// and correctly report them as absent.
pub fn native_executable_get_parameters0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match ctx.get_field_by_name(this, "clazz") {
        Value::Object(Some(mirror)) => crate::lang_class::mirror_class_id(ctx, mirror),
        _ => None,
    };
    let Some(class_id) = class_id else {
        return Ok(Some(Value::Object(None)));
    };
    // Constructor reflection objects carry no `name` field — the JDK uses
    // `<init>` implicitly (same convention as `lang_reflect`).
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "<init>".to_string()),
        _ => "<init>".to_string(),
    };
    // Method-style descriptor first, then Constructor-style (same order as
    // `lang_reflect::native_executable_get_parameters`).
    let descriptor = crate::lang_reflect::executable_descriptor(ctx, this);
    if descriptor.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    // No attribute = `null` (the JDK synthesizes `argN` and reports
    // `isNamePresent()` false).
    //
    // Interpreter round i4 wave 84, lane L3: an attribute whose entries all
    // have `name_index == 0` is NOT "no real parameter data". javac 21+ emits
    // exactly that, without `-parameters`, for a member with a MANDATED or
    // SYNTHETIC parameter (an inner class constructor's outer instance, an
    // enum's `valueOf(String)`, a local class's captured values), and HotSpot
    // answers it: a `Parameter` per entry with a NULL name and the entry's
    // flags, so `isImplicit()` / `isSynthetic()` / `getModifiers()` are real
    // while `getName()` is still `argN` and `isNamePresent()` false. This used
    // to answer `null` (every flag lost), and an entry without a name among
    // named ones was named `argN` (`isNamePresent()` true).
    // `L3W84ParameterFlags`.
    //
    // Interpreter round i5 wave 101, lane L3 (`L3W101MalformedMethodParameters`):
    // HotSpot's `JVM_GetMethodParameters` answers the attribute's OWN entries,
    // so a zero-entry attribute is an empty array (not "absent"), the array
    // is sized by the attribute, not the descriptor, and an empty name stays
    // empty -- the JDK's `verifyParameters` then throws its
    // `MalformedParametersException`s ("Wrong number of parameters in
    // MethodParameters attribute", "Invalid parameter name \"\""). Before the
    // array it checks every `name_index`: out of the pool, or not a
    // `CONSTANT_Utf8`, is an `IllegalArgumentException`.
    let Some(entries) = ctx.method_parameters_attribute(class_id, &name, &descriptor) else {
        return Ok(Some(Value::Object(None)));
    };
    let mut rows: Vec<(Option<String>, u16)> = Vec::with_capacity(entries.len());
    for (entry_name, flags) in entries {
        use cratonvm_native_api::MethodParameterName as N;
        let message = match entry_name {
            N::Unnamed => {
                rows.push((None, flags));
                continue;
            }
            N::Name(n) => {
                rows.push((Some(n), flags));
                continue;
            }
            N::OutOfBounds => "Constant pool index out of bounds",
            N::NotUtf8 => "Wrong type at constant pool index",
        };
        return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: message.to_string(),
        }
        .into());
    }
    let arr =
        crate::lang_reflect::executable_parameters0_array_from_entries(ctx, this, &descriptor, &rows);
    Ok(Some(Value::Object(Some(arr))))
}

/// `java.lang.StackTraceElement.computeFormat()`.
///
/// W4: was a null-safe no-op (ES-FAIL-06 — the JDK body dereferences
/// `declaringClassObject` with no null guard, and elements built by the public
/// 4-arg constructor leave it null). Keep the null guard, but compute the real
/// bits when the class object IS present:
///
/// * `0x1 BUILTIN_CLASS_LOADER` — `toString()` drops the class-loader name.
///   Set for the bootstrap/platform/application loaders, which are exactly
///   `loader_id_of_class` 0/1/2 (`ClassLoaderId::to_native_id`); a user-defined
///   loader gets an id >= 3 and keeps its name in the rendering.
/// * `0x2 JDK_NON_UPGRADEABLE_MODULE` — `toString()` drops the module version.
///   Set for the `java.*` / `jdk.*` boot-layer modules.
///
/// Both bits only ever SUPPRESS a prefix, so an element whose
/// `classLoaderName`/`moduleName` are null renders identically either way —
/// this cannot regress the ES-FAIL-06 case it replaces.
pub fn native_stack_trace_element_compute_format(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let class_id = match ctx.get_field_by_name(this, "declaringClassObject") {
        Value::Object(Some(mirror)) => crate::lang_class::mirror_class_id(ctx, mirror),
        _ => None,
    };
    // Interpreter round i3 wave 79, lane L2: `BUILTIN_CLASS_LOADER` for the
    // platform and application loaders only (`null instanceof
    // BuiltinClassLoader` is false for the bootstrap loader, id 0), and
    // `JDK_NON_UPGRADEABLE_MODULE` for a boot-layer module hashed in
    // `java.base` (`lang_misc::boot_module_hashed_in_java_base`), not every
    // `java.*` / `jdk.*` name: an upgradeable module keeps its `@version`.
    let mut format = 0i32;
    // `frame_loader_id` may run Java (the module table's first ask): `this`
    // is pinned across it and re-read.
    let pin = ctx.pin_native_root(this);
    if let Some(class_id) = class_id {
        let boot_module = match ctx.module_identity_of_class(class_id) {
            cratonvm_native_api::ClassModuleIdentity::Named(module) if !module.is_empty() => {
                Some(module)            }
            _ => None,
        };
        // The REPORTED loader (`lang_misc::frame_loader_id`): a platform
        // module's image class is the platform loader's, as on HotSpot.
        if matches!(
            crate::lang_misc::frame_loader_id(ctx, class_id, boot_module.as_deref()),
            1 | 2
        ) {
            format |= 0x1;
        }
        if let Some(module) = &boot_module {
            if crate::lang_misc::boot_module_hashed_in_java_base(&*ctx, module) {
                format |= 0x2;
            }
        }
    }
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    // No-op when the layout has no `format` field (synthetic elements).
    ctx.set_field_by_name(this, "format", Value::Int(format));
    Ok(None)
}

/// Which of the four POSIX process credentials
/// `jdk.internal.misc.VM.get{u,eu,g,eg}id()` is asking for.
#[derive(Clone, Copy)]
pub enum ProcessCredential {
    Uid,
    Euid,
    Gid,
    Egid,
}

/// Real process credential for `jdk.internal.misc.VM.get{u,eu,g,eg}id()`.
///
/// These four `getXid` calls are Unix-only in HotSpot (`jdk.internal.misc.VM`
/// declares them, and the JVM only registers them on POSIX builds). Returning
/// a fabricated 0 on every platform claimed the process runs as root, which
/// is a value real callers branch on. `libc` is already an unconditional
/// dependency of this crate (see the NIO Selector `poll` path), so the true
/// values are one syscall away; the Windows arm keeps 0 purely so the
/// registration compiles there — nothing calls it.
#[cfg(unix)]
pub fn process_credential(which: ProcessCredential) -> i64 {
    // SAFETY: getuid/geteuid/getgid/getegid are always-succeeding POSIX
    // syscalls with no arguments and no error return.
    unsafe {
        match which {
            ProcessCredential::Uid => libc::getuid() as i64,
            ProcessCredential::Euid => libc::geteuid() as i64,
            ProcessCredential::Gid => libc::getgid() as i64,
            ProcessCredential::Egid => libc::getegid() as i64,
        }
    }
}

/// Windows has no POSIX uid/gid; HotSpot does not expose these natives there.
#[cfg(not(unix))]
pub fn process_credential(_which: ProcessCredential) -> i64 {
    0
}

/// `jdk.internal.misc.Signal.findSignal0(name)` — signal name (no "SIG"
/// prefix) to signal number.
///
/// The numbers are the Linux/glibc `signal(7)` values, which is the platform
/// CratonVM emulates for the rest of the `jdk.net`/`sun.nio.ch` surface. The
/// only property callers actually depend on is that distinct names get
/// distinct numbers (`Signal.handlers` is keyed by the `Signal`, whose
/// `hashCode`/`equals` fold in the number), plus the JDK contract that an
/// unrecognised name yields a negative value so `Signal(String)` throws
/// `IllegalArgumentException` rather than manufacturing a bogus signal.
///
/// `BREAK` is the Windows-only console signal HotSpot exposes; keep it
/// mapped so `sun.misc.Signal("BREAK")` users (JLine, Tomcat's Windows
/// service wrapper) resolve rather than throw.
///
/// Windows (interpreter round i5 wave 100, lane L1; page
/// `interpreter-i100-L1-windows-signal-names-and-numbers-are-linuxs-FIXED-20261015.md`): HotSpot
/// answers from `os_windows.cpp`'s `siglabels` -- the seven MSVC `signal.h`
/// names and numbers, nothing else -- so on Windows `new Signal("HUP")`
/// throws `IllegalArgumentException("Unknown signal: HUP")` and
/// `new Signal("ABRT").getNumber()` is 22. This answered the Linux table on
/// every host.
pub fn posix_signal_number(name: &str) -> i32 {
    if cfg!(windows) {
        return match name {
            "ABRT" => 22,
            "FPE" => 8,
            "SEGV" => 11,
            "INT" => 2,
            "TERM" => 15,
            "BREAK" => 21,
            "ILL" => 4,
            _ => -1,
        };
    }
    match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "TRAP" => 5,
        "ABRT" | "IOT" => 6,
        "BUS" => 7,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        "STKFLT" => 16,
        "CHLD" | "CLD" => 17,
        "CONT" => 18,
        "STOP" => 19,
        "TSTP" => 20,
        "TTIN" => 21,
        "TTOU" => 22,
        "URG" => 23,
        "XCPU" => 24,
        "XFSZ" => 25,
        "VTALRM" => 26,
        "PROF" => 27,
        "WINCH" => 28,
        "IO" | "POLL" => 29,
        "PWR" => 30,
        "SYS" => 31,
        // Windows console break (`SIGBREAK` == 21 in MSVC's signal.h). That
        // number is `TTIN` on Unix, so only answer it on Windows; elsewhere
        // the name is genuinely unknown, exactly as HotSpot reports.
        "BREAK" if cfg!(windows) => 21,
        _ => -1,
    }
}

/// Extract ObjectRef from args at given index, or return NullPointerException
/// Map a bare library name (e.g. "foo") to the platform-specific filename
/// (e.g. "foo.dll" on Windows, "libfoo.so" on Linux).
/// If the name already contains a path separator or an extension, it is returned as-is.
pub fn platform_lib_name(name: &str) -> String {
    if name.contains('/') || name.contains('\\') || name.contains('.') {
        return name.to_string();
    }
    if cfg!(windows) {
        format!("{}.dll", name)
    } else if cfg!(target_os = "macos") {
        format!("lib{}.dylib", name)
    } else {
        format!("lib{}.so", name)
    }
}

/// The inverse of [`platform_lib_name`]: recover the bare library name from the
/// absolute path the JDK's own loader hands to `NativeLibraries.load`.
///
/// (See also [`VM_PROVIDED_LIBRARY_HANDLE`], the handle such a library gets.)
///
/// `NativeLibraries.load` never receives `"zip"`. `NativeLibraries.loadLibrary(
/// Class, File)` passes `file.getCanonicalPath()`, so the native sees
/// `<java.home>\bin\zip.dll` on Windows and `<java.home>/lib/libzip.so` on
/// Linux. Every policy question about a library — the
/// `lang_system::is_vm_provided_jdk_library` allowlist above all — is phrased in
/// bare names, so the path has to be decoded before it can be asked.
///
/// Decoding is by SPELLING, not by host platform: a path can be handed to this
/// VM from a JDK image for another OS (cross-checked corpora, a copied
/// `java.library.path`), and the answer must depend only on the string. A file
/// name with no recognised platform suffix comes back unchanged, which is the
/// conservative answer — it simply misses the allowlist and is reported as the
/// failure it is.
/// The `NativeLibraryImpl.handle` of a JDK-image library whose natives this VM
/// supplies in-process (`NativeLibraries.load`'s allowlist arm), SBR wave 5
/// FJP2. Nonzero, because the JDK reads 0 as "not loaded" (`Unloader` throws on
/// it). Decoded (`- 1`) it is no library-table index, so `findEntry0` takes the
/// whole-process lookup -- the shape of HotSpot's handle for a builtin library,
/// the process itself -- and `unload` refuses it.
pub const VM_PROVIDED_LIBRARY_HANDLE: i64 = i64::MAX;

pub fn bare_native_library_name(path: &str) -> String {
    let file = path
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(path);
    // Windows: `zip.dll`. `System.mapLibraryName` applies no `lib` prefix on
    // this platform, so a leading `lib` there is part of the name and stays.
    let ext_at = file.rfind('.').unwrap_or(file.len());
    if file[ext_at..].eq_ignore_ascii_case(".dll") {
        return file[..ext_at].to_string();
    }
    // Unix / macOS: `libzip.so`, `libzip.dylib`.
    for ext in [".so", ".dylib"] {
        if let Some(stem) = file.strip_suffix(ext) {
            return stem.strip_prefix("lib").unwrap_or(stem).to_string();
        }
    }
    file.to_string()
}


pub fn native_springboot_mongo_reactive_customizer_destroy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Spring Boot 4.0's customizer blocks here in
    // `shutdownGracefully().awaitUninterruptibly()`. In this runtime Netty's
    // worker threads can already have exited while its promise remains
    // incomplete, leaving application-context destruction stuck forever.
    // Request the ordinary zero-quiet-period graceful shutdown, but do not
    // await its stale termination promise. The matching `customize` bridge
    // creates this one lifecycle-owned group with daemon workers, so those
    // workers cannot retain the VM after the Spring context has returned.
    if let Value::Object(Some(event_loop_group)) = ctx.get_field_by_name(this, "eventLoopGroup") {
        // gc-common w20-c: `this` and the group are used after the Java
        // calls below (the shutdown request, every `isShutdown()` poll); read
        // each back from its pin. The dispatcher truncates the pin stack on
        // the error returns.
        let this_pin = ctx.pin_native_root(this);
        let group_pin = ctx.pin_native_root(event_loop_group);
        native_netty_event_executor_group_shutdown_gracefully(
            ctx,
            &[Value::Object(Some(event_loop_group))],
        )?;
        // `shutdownGracefully` only CASes the group into ST_SHUTTING_DOWN
        // synchronously; the ST_SHUTDOWN transition `isShutdown()` observes
        // happens later, on the event-loop thread's own run loop noticing
        // the zero quiet period has already elapsed. That's asynchronous
        // relative to this (Spring's context-close) thread, so a caller
        // that checks `isShutdown()` immediately after `destroy()` returns
        // (e.g. `MongoReactiveAutoConfigurationTests
        // .nettyTransportSettingsAreConfiguredAutomatically`) can observe
        // `false` even though shutdown was correctly requested. Poll for
        // the real transition with a bound instead of the stale
        // `awaitUninterruptibly()` this bridge exists to avoid — bounded so
        // a pathological group that never confirms still can't hang context
        // destruction. Use `ctx.park` (the VM-cooperative wait also used by
        // e.g. `LockSupport.park`), not `std::thread::sleep`: a raw OS sleep
        // here starves the event-loop thread of whatever this thread holds
        // while blocked, so the poll always loses the race and hits the
        // deadline instead of observing the transition. Traced empirically:
        // a freshly-created, never-used group's `isShutdown()` flips true in
        // ~200ms, but a group that actually attempted Mongo connections
        // takes ~1.6-2.5s — its child event loops have live/pending channel
        // state from those attempts to unwind first, not just an idle
        // selector to notice the zero quiet period. Bound generously above
        // that observed range so the common case still converges well
        // inside it and only a truly pathological group hits the cap.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(4000);
        loop {
            let group_now = ctx.read_native_pin(group_pin, event_loop_group);
            let is_shutdown = matches!(
                ctx.invoke_virtual(group_now, "isShutdown", "()Z", &[])?,
                Some(Value::Int(v)) if v != 0
            );
            if is_shutdown || std::time::Instant::now() >= deadline {
                break;
            }
            ctx.park(Some(std::time::Duration::from_millis(5)));
        }
        let this_now = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field_by_name(this_now, "eventLoopGroup", Value::Object(None));
    }
    Ok(None)
}

pub fn native_springboot_mongo_reactive_customizer_customize(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let builder = match args.get(1).copied() {
        Some(Value::Object(Some(builder))) => builder,
        _ => return Ok(None),
    };
    // gc-common w20-c: `this` and `builder` are used after many Java calls
    // below; pinned here (every later pin is above these, so the existing
    // `unpin_native_roots` calls leave them), released at the end. The
    // dispatcher truncates the pin stack on the early returns.
    let this_pin = ctx.pin_native_root(this);
    let builder_pin = ctx.pin_native_root(builder);

    // Preserve Spring Boot's opt-out for user-provided transport settings.
    if let Value::Object(Some(provider)) = ctx.get_field_by_name(this, "settings") {
        if let Some(Value::Object(Some(settings))) =
            ctx.invoke_virtual(provider, "getIfAvailable", "()Ljava/lang/Object;", &[])?
        {
            if matches!(
                ctx.invoke_virtual(
                    settings,
                    "getTransportSettings",
                    "()Lcom/mongodb/connection/TransportSettings;",
                    &[],
                )?,
                Some(Value::Object(Some(_)))
            ) {
                return Ok(None);
            }
        }
    }

    // A TLS-enabled Mongo client can still be completing its asynchronous
    // bootstrap when Spring destroys this bean. Netty's termination promise is
    // then left incomplete by CratonVM even after all useful work is done. Use
    // a daemon factory only for this Spring-owned client group, preserving the
    // normal non-daemon policy for application-managed Netty groups.
    let thread_factory = match ctx.new_object("io/netty/util/concurrent/DefaultThreadFactory")? {
        Some(Value::Object(Some(factory))) => factory,
        _ => return Ok(None),
    };
    let factory_pin = ctx.pin_native_root(thread_factory);
    let pool_name = ctx.create_string("cratonvm-mongo-reactive");
    let pool_name_pin = ctx.pin_native_root(pool_name);
    let factory = ctx.read_native_pin(factory_pin, thread_factory);
    let pool_name = ctx.read_native_pin(pool_name_pin, pool_name);
    ctx.invoke(
        "io/netty/util/concurrent/DefaultThreadFactory",
        "<init>",
        "(Ljava/lang/String;ZI)V",
        &[
            Value::Object(Some(factory)),
            Value::Object(Some(pool_name)),
            Value::Int(1),
            Value::Int(5),
        ],
    )?;
    ctx.unpin_native_roots(pool_name_pin);

    let handler_factory = match ctx.invoke(
        "io/netty/channel/nio/NioIoHandler",
        "newFactory",
        "()Lio/netty/channel/IoHandlerFactory;",
        &[],
    )? {
        Some(Value::Object(Some(handler_factory))) => handler_factory,
        _ => {
            ctx.unpin_native_roots(factory_pin);
            return Ok(None);
        }
    };
    let handler_pin = ctx.pin_native_root(handler_factory);
    let event_loop_group = match ctx.new_object("io/netty/channel/MultiThreadIoEventLoopGroup")? {
        Some(Value::Object(Some(group))) => group,
        _ => {
            ctx.unpin_native_roots(handler_pin);
            ctx.unpin_native_roots(factory_pin);
            return Ok(None);
        }
    };
    let group_pin = ctx.pin_native_root(event_loop_group);
    let event_loop_group = ctx.read_native_pin(group_pin, event_loop_group);
    let thread_factory = ctx.read_native_pin(factory_pin, thread_factory);
    let handler_factory = ctx.read_native_pin(handler_pin, handler_factory);
    ctx.invoke(
        "io/netty/channel/MultiThreadIoEventLoopGroup",
        "<init>",
        "(Ljava/util/concurrent/ThreadFactory;Lio/netty/channel/IoHandlerFactory;)V",
        &[
            Value::Object(Some(event_loop_group)),
            Value::Object(Some(thread_factory)),
            Value::Object(Some(handler_factory)),
        ],
    )?;
    let event_loop_group = ctx.read_native_pin(group_pin, event_loop_group);
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(
        this_now,
        "eventLoopGroup",
        Value::Object(Some(event_loop_group)),
    );
    ctx.unpin_native_roots(group_pin);
    ctx.unpin_native_roots(handler_pin);
    ctx.unpin_native_roots(factory_pin);

    let transport_builder = match ctx.invoke(
        "com/mongodb/connection/TransportSettings",
        "nettyBuilder",
        "()Lcom/mongodb/connection/NettyTransportSettings$Builder;",
        &[],
    )? {
        Some(Value::Object(Some(transport_builder))) => transport_builder,
        _ => return Ok(None),
    };
    let transport_builder_pin = ctx.pin_native_root(transport_builder);
    let this_now = ctx.read_native_pin(this_pin, this);
    let event_loop_group = match ctx.get_field_by_name(this_now, "eventLoopGroup") {
        Value::Object(Some(group)) => group,
        _ => {
            ctx.unpin_native_roots(transport_builder_pin);
            return Ok(None);
        }
    };
    let transport_builder = ctx.read_native_pin(transport_builder_pin, transport_builder);
    ctx.invoke_virtual(
        transport_builder,
        "eventLoopGroup",
        "(Lio/netty/channel/EventLoopGroup;)Lcom/mongodb/connection/NettyTransportSettings$Builder;",
        &[Value::Object(Some(event_loop_group))],
    )?;
    let transport_builder = ctx.read_native_pin(transport_builder_pin, transport_builder);
    let transport_settings = match ctx.invoke_virtual(
        transport_builder,
        "build",
        "()Lcom/mongodb/connection/NettyTransportSettings;",
        &[],
    )? {
        Some(Value::Object(Some(transport_settings))) => transport_settings,
        _ => {
            ctx.unpin_native_roots(transport_builder_pin);
            return Ok(None);
        }
    };
    let transport_settings_pin = ctx.pin_native_root(transport_settings);
    let transport_settings = ctx.read_native_pin(transport_settings_pin, transport_settings);
    let builder_now = ctx.read_native_pin(builder_pin, builder);
    ctx.invoke_virtual(
        builder_now,
        "transportSettings",
        "(Lcom/mongodb/connection/TransportSettings;)Lcom/mongodb/MongoClientSettings$Builder;",
        &[Value::Object(Some(transport_settings))],
    )?;
    ctx.unpin_native_roots(transport_settings_pin);
    ctx.unpin_native_roots(transport_builder_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub fn system_ephemeral_port_range() -> (i32, i32) {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|range| {
            let mut ports = range
                .split_whitespace()
                .filter_map(|port| port.parse::<i32>().ok());
            Some((ports.next()?, ports.next()?))
        })
        .filter(|(lower, upper)| (0..=*upper).contains(lower) && *upper <= 65_535)
        // IANA's dynamic/private range is the portable fallback when the host
        // has no Linux procfs port-range setting (for example on Windows).
        .unwrap_or((49_152, 65_535))
}

/// Copy `len` elements `src[src_off..]` into `dst[0..]` for the `Arrays.copyOf*`
/// shadows (strings round, `str16`): in one bulk copy when the context has one
/// (`bulk_array_copy` for a primitive array; for a `reference` array,
/// `copy_reference_array_elements`, which the callers may use because they
/// allocate `dst` with `src`'s own component class, so no element needs a
/// store check), else element by element as before. The per-element loop
/// validated and forwarded both arrays on every element; under `--compatible`
/// it was most of `StringBuilder.toString()` (`Arrays.copyOfRange([BII)`).
pub fn arrays_copy_elements(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    len: usize,
    reference: bool,
) {
    if len == 0 {
        return;
    }
    let bulk = if reference {
        ctx.copy_reference_array_elements(src, src_off, dst, 0, len)
    } else {
        ctx.bulk_array_copy(src, src_off, dst, 0, len)
    };
    if !bulk {
        for i in 0..len {
            ctx.set_array_element(dst, i, ctx.get_array_element(src, src_off + i));
        }
    }
}

/// The source array for an `Arrays.copyOf`/`copyOfRange`/`fill` native: a null
/// one is a `NullPointerException`, not a null RESULT.
///
/// MEASURED 2026-08-28 in BOTH modes (`probes/ArraysHashSetShadowSweep.java`),
/// against HotSpot 25.0.3+9. Every one of these natives opened with
///
/// ```text
/// let src = match args.first() {
///     Some(Value::Object(Some(a))) => *a,
///     _ => return Ok(Some(Value::Object(None))),   // <- a null array answers null
/// };
/// ```
///
/// so `Arrays.copyOf(null, 1)` handed the caller a null array where the JDK
/// throws. That is the worst shape a refusal can take: the caller does not
/// learn it passed a null until the null it got back is dereferenced somewhere
/// else, by which time the array is no longer in the frame.
pub fn arrays_src_or_npe(args: &[Value], what: &str) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(a))) => Ok(*a),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("Arrays.{what}: array is null")),
        }
        .into()),
    }
}

/// A new-array length: negative is `NegativeArraySizeException`.
///
/// MEASURED: `Arrays.copyOf(src, -1)` answered **OutOfMemoryError** on this VM
/// against HotSpot's `NegativeArraySizeException`, because the length was read
/// as `*n as usize` -- and `-1 as usize` on a 64-bit host is 18 446 744 073 709
/// 551 615, which is a perfectly sincere request for eighteen exabytes. The
/// allocator did the only thing it could.
///
/// The wrong TYPE matters more than the wrong message here: an application that
/// catches `NegativeArraySizeException` around a computed length recovers, and
/// `OutOfMemoryError` is an `Error` that most catch blocks deliberately do not
/// take.
pub fn arrays_len_or_negative(n: i32) -> Result<usize, MethodCallFailed> {
    if n < 0 {
        return Err(RuntimeError::NegativeArraySizeException { size: n }.into());
    }
    Ok(n as usize)
}

