// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.nio.charset` and buffer natives: Charset, CharsetEncoder/Decoder, CharBuffer.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

/// Canonical charset name of a synthetic `java/nio/charset/Charset` receiver.
/// Slot 0 holds the name string (see the `name()` / `displayName()` natives in
/// `register_p61_charset`); it is run through `canonical_charset_name` so
/// aliases (`UTF8`, `latin1`, `ASCII`, …) compare equal to their canonical
/// spelling. Unknown names are returned verbatim.
pub(crate) fn charset_name_field(ctx: &dyn NativeContext, cs: ObjectRef) -> String {
    let raw = match ctx.get_field(cs, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    cratonvm_native_api::charset::canonical_charset_name(&raw)
        .map(str::to_string)
        .unwrap_or(raw)
}

/// `Charset.contains(Charset)` — "can `this` represent every character
/// `other` can?". Abstract in the real JDK; these are the per-family answers
/// the concrete `sun.nio.cs` classes give.
///
/// The default arm is deliberately `false` rather than "assume ASCII
/// superset": CratonVM's charset table includes the EBCDIC code pages
/// (`IBM500`, `IBM1047`), which are NOT ASCII supersets, so a blanket
/// assumption would be wrong for them. A false negative only makes a caller
/// transcode when it could have aliased; a false positive corrupts data.
pub(crate) fn charset_contains(this: &str, other: &str) -> bool {
    if this.eq_ignore_ascii_case(other) {
        return true;
    }
    match this {
        // sun.nio.cs.UTF_8 / UTF_16* / UTF_32* all `return true`: the Unicode
        // charsets can encode every character of any other charset.
        "UTF-8" | "UTF-16" | "UTF-16BE" | "UTF-16LE" | "UTF-32" | "UTF-32BE" | "UTF-32LE" => true,
        // sun.nio.cs.ISO_8859_1.contains: US-ASCII and itself.
        "ISO-8859-1" => matches!(other, "US-ASCII" | "ISO-8859-1"),
        // sun.nio.cs.US_ASCII.contains: itself only.
        "US-ASCII" => other == "US-ASCII",
        // The remaining single-byte and CJK charsets in our table are ASCII
        // supersets (ISO-8859-x, windows-125x, KOI8-*, Shift_JIS, EUC-*,
        // Big5, GB*, IBM850) — but NOT the EBCDIC pages, which are listed
        // explicitly below so they fall through to `false`.
        "IBM500" | "IBM1047" => false,
        "ISO-8859-2" | "ISO-8859-3" | "ISO-8859-4" | "ISO-8859-5" | "ISO-8859-15"
        | "windows-1250" | "windows-1251" | "windows-1252" | "KOI8-R" | "KOI8-U" | "Shift_JIS"
        | "EUC-JP" | "ISO-2022-JP" | "Big5" | "EUC-KR" | "GB2312" | "GBK" | "GB18030"
        | "IBM850" => other == "US-ASCII",
        _ => false,
    }
}

/// Register the one Charset bridge that a real JDK cannot supply itself:
/// `Charset.contains` is abstract, while the concrete `sun.nio.cs.*` classes
/// do not expose Code attributes to CratonVM.  Keep this separate from the
/// synthetic Charset registrar: its slot-based object constructors are not
/// valid for a real JDK receiver.
pub fn register_real_jdk_charset_contains(r: &mut NativeMethodRegistry) {
    r.register(
        "java/nio/charset/Charset",
        "contains",
        "(Ljava/nio/charset/Charset;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Charset.contains: null charset".to_string()),
                    }
                    .into())
                }
            };
            // `name()` is ordinary real-JDK bytecode and reads the real
            // Charset.name field, unlike the synthetic-only slot helper.
            let read_name = |ctx: &mut dyn NativeContext, value: ObjectRef| -> String {
                match ctx.invoke_virtual(value, "name", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(name)))) => {
                        ctx.read_string(name).unwrap_or_default()
                    }
                    _ => String::new(),
                }
            };
            let this_name = read_name(ctx, this);
            let other_name = read_name(ctx, other);
            let canonical = |name: String| {
                cratonvm_native_api::charset::canonical_charset_name(&name)
                    .map(str::to_string)
                    .unwrap_or(name)
            };
            Ok(Some(Value::Int(i32::from(charset_contains(
                &canonical(this_name),
                &canonical(other_name),
            )))))
        },
    );
}

// ---------------------------------------------------------------------------
// java.nio.charset — Charset, StandardCharsets
// ---------------------------------------------------------------------------
pub(crate) fn register_phase55_charset(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Charset = 1-field synthetic (name=0)
    let cs = "java/nio/charset/Charset";
    r.register(
        cs,
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        |ctx, args| {
            let name_ref = obj_arg(args, 0)?;
            let raw_name = ctx.read_string(name_ref).unwrap_or_default();
            // B7: fail-closed for unknown charset names. `normalize_charset_name`
            // returns "" for any name it does not recognise, so an unsupported
            // name throws here instead of silently yielding a fake Charset.
            // The real JDK throws java.nio.charset.UnsupportedCharsetException,
            // which *extends* IllegalArgumentException; the VM has no dedicated
            // UnsupportedCharsetException RuntimeError variant, so we raise the
            // IllegalArgumentException supertype with the JDK message text —
            // callers catching IllegalArgumentException behave correctly. (A
            // precise UnsupportedCharsetException type would require a new
            // RuntimeError variant in types/src/error.rs.)
            let normalized = normalize_charset_name(&raw_name);
            if normalized.is_empty() {
                return Err(RuntimeError::IllegalArgumentException { message: raw_name }.into());
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let canon = ctx.create_string(&normalized);
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(canon)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(cs, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cs, "displayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        cs,
        "defaultCharset",
        "()Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string("UTF-8");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(cs, "isSupported", "(Ljava/lang/String;)Z", |ctx, args| {
        let name_ref = obj_arg(args, 0)?;
        let name = ctx.read_string(name_ref).unwrap_or_default();
        let supported = !normalize_charset_name(&name).is_empty();
        Ok(Some(Value::Int(if supported { 1 } else { 0 })))
    });
    r.register(cs, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cs, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = args[1] {
            let n1 = if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
                ctx.read_string(s).unwrap_or_default()
            } else {
                String::new()
            };
            let n2 = if let Value::Object(Some(s)) = ctx.get_field(other, 0) {
                ctx.read_string(s).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(Some(Value::Int(if n1.eq_ignore_ascii_case(&n2) {
                1
            } else {
                0
            })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(cs, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
            let name = ctx.read_string(s).unwrap_or_default().to_lowercase();
            let mut h: i32 = 0;
            for b in name.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as i32);
            }
            Ok(Some(Value::Int(h)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });

    // StandardCharsets — static fields returning Charset objects
    let sc = "java/nio/charset/StandardCharsets";
    r.register(sc, "UTF_8", "Ljava/nio/charset/Charset;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh Charset (native stale-local family).
        let obj_pin = ctx.pin_native_root(obj);
        let name = ctx.create_string("UTF-8");
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field(obj, 0, Value::Object(Some(name)));
        ctx.unpin_native_roots(obj_pin);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        sc,
        "US_ASCII",
        "Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string("US-ASCII");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sc,
        "ISO_8859_1",
        "Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string("ISO-8859-1");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(sc, "UTF_16", "Ljava/nio/charset/Charset;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh Charset (native stale-local family).
        let obj_pin = ctx.pin_native_root(obj);
        let name = ctx.create_string("UTF-16");
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field(obj, 0, Value::Object(Some(name)));
        ctx.unpin_native_roots(obj_pin);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        sc,
        "UTF_16BE",
        "Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string("UTF-16BE");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sc,
        "UTF_16LE",
        "Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh Charset (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let name = ctx.create_string("UTF-16LE");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// CharsetEncoder / CharsetDecoder basics
// CharsetEncoder = 3-field (charset=0, avgBytesPerChar=1 Float, maxBytesPerChar=2 Float)
// CharsetDecoder = 3-field (charset=0, avgCharsPerByte=1 Float, maxCharsPerByte=2 Float)
// =============================================================================

/// Pins `coder` across [`seed_coder_error_actions_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `coder` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn seed_coder_error_actions(ctx: &mut dyn NativeContext, coder: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*coder);
    let w5_out = seed_coder_error_actions_body(ctx, *coder);
    *coder = ctx.read_native_pin(w5_pin, *coder);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Seed a freshly-built synthetic `CharsetEncoder`/`CharsetDecoder` with the
/// JDK-default coding-error actions (`CodingErrorAction.REPORT`) for the
/// `malformedInputAction` / `unmappableCharacterAction` fields.
///
/// The real `java.nio.charset.CharsetEncoder`/`CharsetDecoder` constructors
/// initialise both fields to `CodingErrorAction.REPORT`. Our `newEncoder` /
/// `newDecoder` shims allocate the coder object directly and only wrote the
/// charset / avg / max slots, leaving these two fields `null`. When real-JDK
/// bytecode runs on such a coder — e.g. `CharsetEncoder.canEncode` (Spring's
/// `HttpHeaders.encodeBasicAuth` → `encoder.canEncode(username)`) — its
/// `finally` block calls `onMalformedInput(malformedInputAction())` with a
/// `null` argument, throwing `IllegalArgumentException("Null action")`. Seed
/// the fields so the JDK bytecode sees the same defaults HotSpot would.
/// `set_field_by_name` is a no-op when the field is absent (synthetic-JDK
/// mode), so this is safe in both modes.
pub(crate) fn seed_coder_error_actions_body(ctx: &mut dyn NativeContext, coder: ObjectRef) {
    let report = match ctx.ensure_class_initialized("java/nio/charset/CodingErrorAction") {
        Ok(cid) => match ctx.static_field_index_by_name(cid, "REPORT") {
            Some(idx) => ctx.get_static_field(cid, idx),
            None => return,
        },
        Err(_) => return,
    };
    ctx.set_field_by_name(coder, "malformedInputAction", report);
    ctx.set_field_by_name(coder, "unmappableCharacterAction", report);
}

pub fn register_p58_charset_coder(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let enc = "java/nio/charset/CharsetEncoder";
    r.register(
        enc,
        "charset",
        "()Ljava/nio/charset/Charset;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(enc, "averageBytesPerChar", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(enc, "maxBytesPerChar", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        enc,
        "encode",
        "(Ljava/nio/CharBuffer;Ljava/nio/ByteBuffer;Z)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1)?;
            ctx.set_field(result, 0, Value::Int(0)); // 0 = UNDERFLOW (success)
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        enc,
        "flush",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1)?;
            ctx.set_field(result, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        enc,
        "reset",
        "()Ljava/nio/charset/CharsetEncoder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    let dec = "java/nio/charset/CharsetDecoder";
    r.register(
        dec,
        "charset",
        "()Ljava/nio/charset/Charset;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(dec, "averageCharsPerByte", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(dec, "maxCharsPerByte", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        dec,
        "decode",
        "(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;Z)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1)?;
            ctx.set_field(result, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        dec,
        "flush",
        "(Ljava/nio/CharBuffer;)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1)?;
            ctx.set_field(result, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        dec,
        "reset",
        "()Ljava/nio/charset/CharsetDecoder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dec,
        "onMalformedInput",
        "(Ljava/nio/charset/CodingErrorAction;)Ljava/nio/charset/CharsetDecoder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let action = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "malformedInputAction", action);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        dec,
        "onUnmappableCharacter",
        "(Ljava/nio/charset/CodingErrorAction;)Ljava/nio/charset/CharsetDecoder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let action = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "unmappableCharacterAction", action);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        dec,
        "replaceWith",
        "(Ljava/lang/String;)Ljava/nio/charset/CharsetDecoder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let replacement = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "replacement", replacement);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(dec, "replacement", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "replacement")))
    });

    // CoderResult constants
    let cr = "java/nio/charset/CoderResult";
    r.register(cr, "isUnderflow", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if tag == 0 { 1 } else { 0 })))
    });
    r.register(cr, "isOverflow", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if tag == 1 { 1 } else { 0 })))
    });
    r.register(cr, "isError", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if tag >= 2 { 1 } else { 0 })))
    });
    r.register(cr, "isMalformed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if tag == 2 { 1 } else { 0 })))
    });
    r.register(cr, "isUnmappable", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if tag == 3 { 1 } else { 0 })))
    });

    // Charset.newEncoder / newDecoder methods
    let cs = "java/nio/charset/Charset";
    r.register(
        cs,
        "newEncoder",
        "()Ljava/nio/charset/CharsetEncoder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, CHARSET_FIELD_NAME) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
                _ => "UTF-8".to_string(),
            };
            let avg = cratonvm_native_api::charset::average_bytes_per_char(&name);
            let max = cratonvm_native_api::charset::max_bytes_per_char(&name);
            let mut enc_obj =
                try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetEncoder", 6)?;
            ctx.set_field(enc_obj, 0, Value::Object(Some(this)));
            ctx.set_field(enc_obj, 1, Value::Float(avg));
            ctx.set_field(enc_obj, 2, Value::Float(max));
            seed_coder_error_actions(ctx, &mut enc_obj);
            Ok(Some(Value::Object(Some(enc_obj))))
        },
    );
    r.register(
        cs,
        "newDecoder",
        "()Ljava/nio/charset/CharsetDecoder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, CHARSET_FIELD_NAME) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "UTF-8".to_string()),
                _ => "UTF-8".to_string(),
            };
            let avg = cratonvm_native_api::charset::average_chars_per_byte(&name);
            let max = cratonvm_native_api::charset::max_chars_per_byte(&name);
            let mut dec_obj =
                try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetDecoder", 6)?;
            ctx.set_field(dec_obj, 0, Value::Object(Some(this)));
            ctx.set_field(dec_obj, 1, Value::Float(avg));
            ctx.set_field(dec_obj, 2, Value::Float(max));
            let replacement = ctx.create_string("\u{fffd}");
            ctx.set_field_by_name(dec_obj, "replacement", Value::Object(Some(replacement)));
            seed_coder_error_actions(ctx, &mut dec_obj);
            Ok(Some(Value::Object(Some(dec_obj))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.nio.charset — Charset expansion, availableCharsets, aliases
// =============================================================================

pub(crate) fn register_p61_charset(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cs = "java/nio/charset/Charset";
    r.register(cs, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0))) // field 0 = name string
    });
    r.register(cs, "aliases", "()Ljava/util/Set;", |ctx, _args| {
        // Return empty HashSet
        let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
        ctx.set_field(set, 0, Value::Object(None));
        ctx.set_field(set, 1, Value::Int(0));
        ctx.set_field(set, 2, Value::Int(16));
        Ok(Some(Value::Object(Some(set))))
    });
    r.register(cs, "displayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    // isRegistered() — STUB-REMOVAL (wave 3): was a flat `true`. The real body
    // is one line, and it is decidable from the name we already store in slot
    // 0: `return !name.startsWith("X-") && !name.startsWith("x-")` — an
    // experimental/private charset is by definition NOT in the IANA registry.
    r.register(cs, "isRegistered", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = charset_name_field(ctx, this);
        let experimental = name.starts_with("X-") || name.starts_with("x-");
        Ok(Some(Value::Int(i32::from(!experimental))))
    });
    // KEEP: spec-correct, not a stub. `java.nio.charset.Charset.canEncode()`
    // IS `return true` in the real JDK; only decode-only charsets (e.g.
    // JIS_AUTODETECT) override it to false, and CratonVM's charset table
    // (`cratonvm_native_api::charset::canonical_charset_name`) contains none —
    // every entry has a working `encode_chars` path.
    // REACHABILITY: `register_p61_charset` is reached only via
    // `register_phase61_natives` -> `register_synthetic_overrides`, which is
    // `#[cfg(feature = "synthetic-jdk")]` — so in the default real-JDK build
    // this registration does not exist and `Charset.canEncode()` runs its own
    // (identical) bytecode.
    r.register(cs, "canEncode", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    // contains(Charset) — STUB-REMOVAL (wave 3): was a flat `false`, which said
    // "UTF-8 cannot represent US-ASCII". The method is ABSTRACT in the real JDK
    // (each concrete charset answers for itself); decide it from the two
    // canonical names. See `charset_contains` for the per-family rules and for
    // why the default arm is deliberately conservative.
    r.register(
        cs,
        "contains",
        "(Ljava/nio/charset/Charset;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                // Real `contains(null)` NPEs on the instanceof-free impls.
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Charset.contains: null charset".to_string()),
                    }
                    .into())
                }
            };
            let a = charset_name_field(ctx, this);
            let b = charset_name_field(ctx, other);
            Ok(Some(Value::Int(i32::from(charset_contains(&a, &b)))))
        },
    );
    r.register(
        cs,
        "compareTo",
        "(Ljava/nio/charset/Charset;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name1 = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if let Some(Value::Object(Some(other))) = args.get(1) {
                let name2 = match ctx.get_field(*other, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(Some(Value::Int(
                    name1.to_lowercase().cmp(&name2.to_lowercase()) as i32,
                )))
            } else {
                Ok(Some(Value::Int(0)))
            }
        },
    );
    r.register(cs, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // availableCharsets returns sorted map of name -> Charset
    r.register(
        cs,
        "availableCharsets",
        "()Ljava/util/SortedMap;",
        |ctx, _args| {
            // Return a TreeMap with standard charsets
            let tm = try_alloc_concurrent_synthetic(ctx, "java/util/TreeMap", 3)?;
            let charsets = [
                "ISO-8859-1",
                "US-ASCII",
                "UTF-16",
                "UTF-16BE",
                "UTF-16LE",
                "UTF-8",
            ];
            let data_arr = ctx.new_array(
                cratonvm_types::ArrayElementType::Reference,
                charsets.len() * 2,
            );
            for (i, &name) in charsets.iter().enumerate() {
                let key = ctx.create_string(name);
                ctx.set_array_element(data_arr, i * 2, Value::Object(Some(key)));
                // Create a Charset object for each
                let cs_obj = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 2)?;
                let n = ctx.create_string(name);
                ctx.set_field(cs_obj, 0, Value::Object(Some(n)));
                ctx.set_field(cs_obj, 1, Value::Int(0)); // flags
                ctx.set_array_element(data_arr, i * 2 + 1, Value::Object(Some(cs_obj)));
            }
            ctx.set_field(tm, 0, Value::Object(Some(data_arr)));
            ctx.set_field(tm, 1, Value::Int(charsets.len() as i32));
            ctx.set_field(tm, 2, Value::Object(None)); // no comparator
            Ok(Some(Value::Object(Some(tm))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.nio.CharBuffer = 5-field (array=0 char[], pos=1, limit=2, capacity=3, mark=4)
// =============================================================================

pub(crate) const CB_FIELD_ARRAY: usize = 0;

pub(crate) const CB_FIELD_POS: usize = 1;

pub(crate) const CB_FIELD_LIMIT: usize = 2;

pub(crate) const CB_FIELD_CAPACITY: usize = 3;

pub(crate) const CB_FIELD_MARK: usize = 4;

/// Initialise a freshly-allocated synthetic CharBuffer so BOTH the
/// indexed-slot layout (used by our own natives) AND the real-JDK
/// `hb` / `position` / `limit` / `capacity` / `mark` / `offset` /
/// `isReadOnly` fields (used by JDK bytecode that wasn't overridden)
/// reference the same backing char[]. Without the by-name writes the
/// JDK bytecode for `hasArray` / `array` / `subSequence` reads a null
/// `hb` (uninitialised real-JDK field) and throws "no backing array"
/// — observed on icu4j-68.2 / icu4j-70.1 going through
/// `CharBuffer.subSequence(...).toString()` in `ICUResourceBundleReader.
/// getStringV2`.
/// Is this a bare synthetic CharBuffer — five slots and none of the real
/// `java.nio.Buffer` field metadata — rather than a real-JDK-shaped one?
///
/// The indexed `CB_FIELD_*` writes are only meaningful on the former. On the
/// latter the same indices alias real fields (`mark`@0, `address`@4) that the
/// by-name writes already set correctly. Mirrors
/// `servlet.rs::s2_bb_synthetic_layout`, which guards the ByteBuffer side of
/// the identical hazard.
fn cb_synthetic_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    // Slot count, exactly as `s2_bb_synthetic_layout` decides it for
    // ByteBuffer: the bare synthetic carrier is five slots
    // (array/pos/limit/capacity/mark), and `alloc_concurrent_synthetic`
    // widens the allocation to the real field count — strictly more than
    // five — whenever the real class is loaded.
    ctx.object_num_fields(buf) == CB_SYNTHETIC_FIELD_COUNT
}

/// The bare synthetic CharBuffer carrier's slot count — the `5` every
/// `try_alloc_concurrent_synthetic(_, "java/nio/*CharBuffer", 5)?` call site here
/// passes, named so [`cb_synthetic_layout`] and those sites cannot drift apart.
pub(crate) const CB_SYNTHETIC_FIELD_COUNT: usize = 5;

pub(crate) fn cb_write_hb(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    arr: ObjectRef,
    len_chars: i32,
) {
    // Real-JDK field names: `hb` (char[]), `offset` (int),
    // `isReadOnly` (boolean) on CharBuffer; `mark` / `position` /
    // `limit` / `capacity` (int) on Buffer.
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(buf, "offset", Value::Int(0));
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(0));
    ctx.set_field_by_name(buf, "position", Value::Int(0));
    ctx.set_field_by_name(buf, "limit", Value::Int(len_chars));
    ctx.set_field_by_name(buf, "capacity", Value::Int(len_chars));
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    // Synthetic-mode indexed fallback — ONLY for the bare synthetic layout,
    // exactly as `servlet.rs::bb_write_hb` already guards the ByteBuffer side.
    //
    // The two layouts alias. On a real-JDK `java/nio/CharBuffer` the
    // hierarchy-wide field order is Buffer's `mark`(0) `position`(1)
    // `limit`(2) `capacity`(3) `address`(4), then CharBuffer's
    // `hb`/`offset`/`isReadOnly` — so of the five indexed slots only 1/2/3
    // mean the same thing in both. `CB_FIELD_ARRAY`(0) lands on **`mark`** and
    // `CB_FIELD_MARK`(4) lands on **`address`**.
    //
    // Redoing them unconditionally clobbered both. `address` was noticed and
    // is re-asserted below; `mark` was not, so a real-layout CharBuffer
    // carried the backing `char[]`'s reference — a large positive number — in
    // its `mark`. Nothing read it until `Buffer.<init>` began validating
    // `mark > position`, at which point `CharBuffer.allocate(12).duplicate()`
    // threw `IllegalArgumentException: mark > position: (52784352 > 0)`:
    // `duplicate()` passes `markValue()` straight into the constructor.
    //
    // Re-asserting the one aliased field somebody remembered is what left the
    // other corrupt. Not writing them at all on a real layout cannot rot the
    // same way.
    if cb_synthetic_layout(ctx, buf) {
        ctx.set_field(buf, CB_FIELD_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, CB_FIELD_POS, Value::Int(0));
        ctx.set_field(buf, CB_FIELD_LIMIT, Value::Int(len_chars));
        ctx.set_field(buf, CB_FIELD_CAPACITY, Value::Int(len_chars));
        ctx.set_field(buf, CB_FIELD_MARK, Value::Int(-1));
    }
    // `java.nio.Buffer.address` (long), and it MUST be written last, by name:
    // the indexed CB_FIELD_* writes above can alias the real `address` slot,
    // and CB_FIELD_MARK's -1 is exactly what used to land in it.
    //
    // A real `HeapCharBuffer` sets `address` to
    // `ARRAY_CHAR_BASE_OFFSET + (offset << 1)` (= 16 + 0). Left at -1, the
    // inherited bulk-put bytecode `CharBuffer.put(CharBuffer)` ->
    // `putBuffer` -> `ScopedMemoryAccess.copyMemory` computes a source
    // offset of `address(-1) + (pos << 1)`, which is below
    // `arrayBaseOffset` (16), so `unsafe_array_read_bytes`'s
    // `byte_off.checked_sub(ABASE)` underflows and the copy reports
    // ArrayIndexOutOfBoundsException.
    //
    // This is the identical defect, and the identical fix, that
    // `charset.rs`'s ByteBuffer allocator already carries with the same
    // warning about aliasing. Found 2026-08-05: it is what fails
    // `BeanRegistrationsAotContributionTests
    // #applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` on
    // current dev - javac's `BaseFileManager.decode` grows its CharBuffer and
    // copies the old one in, so EVERY source file it reads hits this.
    // 16 == `Unsafe.arrayBaseOffset(char[])` here.
    ctx.set_field_by_name(buf, "address", Value::Long(16));
}

/// Write `java.nio.Buffer.address` for a freshly-allocated HEAP CharBuffer.
///
/// `CB_FIELD_MARK` is slot 4, and on a real-JDK `java/nio/CharBuffer` the
/// hierarchy-wide field order is Buffer's `mark`(0) `position`(1) `limit`(2)
/// `capacity`(3) `address`(4) followed by CharBuffer's `hb`/`offset`/
/// `isReadOnly`. So the indexed mark write lands on **`address`**, not on
/// `mark` — every `flip()`/`clear()`/`rewind()` reset it to -1.
///
/// With `address` at -1, `CharBuffer.put(CharBuffer)` -> `putBuffer` computes
/// a source offset below `arrayBaseOffset` and the copy reports
/// ArrayIndexOutOfBoundsException. The indexed writes have to stay for
/// synthetic mode (where the by-name fields do not exist), so re-assert the
/// real field afterwards, exactly as `cb_write_hb` does at allocation.
///
/// Measured 2026-08-05: `flip()` alone took a freshly allocated CharBuffer
/// from address=16 to address=-1, which is why `put(char[])`, `get(char[])`
/// and `put(String)` all worked while only `put(CharBuffer)` threw.
/// AUDIT 2026-08-05, second pass: the flat `address = 16` this used to write
/// is only right for a buffer whose `offset` is 0. A real `HeapCharBuffer` sets
/// `address = ARRAY_CHAR_BASE_OFFSET + offset * 2`, so a SLICE carries a larger
/// address, and a direct buffer carries a real native pointer that must never
/// be synthesised at all. Freshly-allocated heap buffers therefore go through
/// [`cb_write_heap_address`] (which honours `offset`), and mutators go through
/// [`cb_set_mark`] (which preserves whatever the object already carries).
#[inline]
pub(crate) fn cb_write_heap_address(ctx: &mut dyn NativeContext, buf: ObjectRef, offset: i32) {
    ctx.set_field_by_name(buf, "mark", Value::Int(-1));
    ctx.set_field_by_name(buf, "address", Value::Long(16 + (offset as i64) * 2));
}

/// Write `mark` on a CharBuffer without destroying `address`.
///
/// The indexed slot has to stay for synthetic mode (where the by-name fields do
/// not exist), so save the real `address` across it and put it back. Save and
/// restore rather than recompute: this is called on buffers CratonVM did not
/// allocate, including slices (`address = base + offset * 2`) and direct
/// buffers (`address` is a genuine native pointer). On a synthetic buffer the
/// read yields a non-`Long` and nothing is restored.
#[inline]
pub(crate) fn cb_set_mark(ctx: &mut dyn NativeContext, buf: ObjectRef, v: i32) {
    let saved_address = ctx.get_field_by_name(buf, "address");
    ctx.set_field(buf, CB_FIELD_MARK, Value::Int(v));
    ctx.set_field_by_name(buf, "mark", Value::Int(v));
    if let Value::Long(_) = saved_address {
        ctx.set_field_by_name(buf, "address", saved_address);
    }
}

/// Read the backing char[] from a CharBuffer, honouring both the
/// real-JDK `hb` field and the synthetic indexed slot.
pub(crate) fn cb_read_hb(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(buf, "hb") {
        Value::Object(Some(a)) => Some(a),
        _ => match ctx.get_field(buf, CB_FIELD_ARRAY) {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        },
    }
}

/// Is this buffer READ-ONLY, by the field the JDK's own bodies read?
///
/// Byte-identical in shape to `servlet.rs::s2_bb_is_read_only`, which is the
/// ByteBuffer twin of this whole family — the two must not drift, so this is
/// transcribed from it rather than written afresh.
///
/// The FIELD, not the `isReadOnly()` method, because that is what
/// `CharBuffer.hasArray`/`array`/`arrayOffset` read:
///
/// ```java
/// public final boolean hasArray() { return (hb != null) && !isReadOnly; }
/// ```
///
/// The two can legitimately disagree. `ByteBufferAsCharBufferRB` overrides the
/// METHOD to return `true` and never writes the field (JDK 25
/// `java.base/java/nio/ByteBufferAsCharBufferRB.java:214` — the only
/// `isReadOnly` in the file is the method). That is not a hazard here: those
/// view classes carry no `hb` either, so the "no backing array" arm of
/// [`buffer_array_access`] answers first and the field is never consulted.
/// MEASURED on jdk-25.0.3+9 (`probes/BufferAccessibleArrayProbe`):
/// `ByteBuffer.allocate(8).asReadOnlyBuffer().asIntBuffer()` answers
/// `isReadOnly() == true` and `array()` throws **UnsupportedOperationException**,
/// not `ReadOnlyBufferException`.
///
/// A synthetic carrier with no such field answers a non-`Int`, or `Int(0)`, and
/// lands on `false` — which is right: every CharBuffer this VM mints goes
/// through [`cb_write_hb`], which writes `isReadOnly = 0` by name.
/// CONVERGED (F14-1 N1): a delegation to `cratonvm_native_io`'s copy, which is
/// the only one of the three this crate can import (the dependency edge runs
/// `cratonvm-native-builtins` → `cratonvm-native-io`). `servlet.rs`'s
/// `s2_bb_is_read_only` is the same delegation.
fn cb_is_read_only(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    cratonvm_native_io::buffer_is_read_only(ctx, buf)
}

/// The ONE three-way decision that `hasArray()`, `array()` and `arrayOffset()`
/// share — and the reason none of them can be written as a single
/// "is the array accessible?" branch.
///
/// JDK 25, `java.base/java/nio/CharBuffer.java` L1490/L1513/L1541 (and the
/// byte-for-byte identical bodies in `ByteBuffer.java` at the same line
/// numbers — one contract, generated twice from `X-Buffer.java.template`):
///
/// ```java
/// public final boolean hasArray() { return (hb != null) && !isReadOnly; }
///
/// public final char[] array() {
///     if (hb == null)  throw new UnsupportedOperationException();
///     if (isReadOnly)  throw new ReadOnlyBufferException();
///     return hb;
/// }
///
/// public final int arrayOffset() {          // identical split
///     if (hb == null)  throw new UnsupportedOperationException();
///     if (isReadOnly)  throw new ReadOnlyBufferException();
///     return offset;
/// }
/// ```
///
/// So `hasArray() == false` covers **two** states that raise **different**
/// classes, and the classes are not interchangeable to a `catch`. MEASURED on
/// jdk-25.0.3+9 (`probes/BufferAccessibleArrayProbe`):
///
/// | receiver | `hasArray()` | `array()` / `arrayOffset()` |
/// |---|---|---|
/// | `HeapCharBuffer` (`CharBuffer.allocate(4)`) | `true` | the array / `0` |
/// | `HeapCharBufferR` (`.asReadOnlyBuffer()`) | `false` | `ReadOnlyBufferException` |
/// | `StringCharBuffer` (`CharBuffer.wrap("abcd")`) | `false` | `UnsupportedOperationException` |
/// | `ByteBufferAsCharBufferB` (a view) | `false` | `UnsupportedOperationException` |
///
/// The **order** is load-bearing and is why this is a function rather than two
/// booleans at each site: a `StringCharBuffer` is read-only AND array-less, and
/// the JDK checks array-less FIRST, so it answers `UnsupportedOperation`. A
/// read-only-first implementation gets that one cell wrong and every other cell
/// right, which is exactly the shape a single-branch "not accessible" fix
/// produces.
/// CONVERGED (F14-1 N1). The `CbArrayAccess` enum and its `cb_array_access`
/// constructor used to live here, byte-identical to
/// `cratonvm_native_io::BufferArrayAccess` / `buffer_array_access` and to the
/// conjunction open-coded in `servlet.rs`. Three copies of a rule whose whole
/// point is an ORDER that is wrong in exactly one cell is three chances to
/// drift, and this family had already drifted once — the ByteBuffer half
/// carried `&& !isReadOnly` while this half did not.
///
/// They are DELETED rather than aliased: an enum alias would leave two type
/// names for one set of states, and the three natives below now name the
/// imported type directly. `cratonvm-native-builtins` depends on
/// `cratonvm-native-io` and not the reverse, so the `native-io` copy is the
/// only one this file could have converged onto.
use cratonvm_native_io::{buffer_array_access, BufferArrayAccess};

/// `throw new UnsupportedOperationException()` — the NO-ARGUMENT constructor,
/// so `getMessage()` is null exactly as HotSpot's is. The empty string is the
/// documented spelling for that in `types/src/error.rs`
/// (`RuntimeError::UnsupportedOperationException` maps `""` to `None` so the
/// `()V` ctor is used); `Some("")` would set a non-null empty detail message.
///
/// CONVERGED (F14-1 N1): a delegation to the one remaining body. The local name
/// stays because the two call sites below read better with it and because the
/// empty-string spelling — the detail a fourth transcription would get wrong —
/// now exists once.
#[inline]
fn cb_no_backing_array() -> MethodCallFailed {
    cratonvm_native_io::buffer_no_backing_array()
}

/// Plain CharBuffer/HeapCharBuffer/StringCharBuffer instances have no
/// independent byte-order concept of their own (real `HeapCharBuffer.order()`
/// just returns the platform's native order); this mirrors the same
/// convention already used by `ByteOrder.nativeOrder()`
/// (`servlet.rs::register_s2_byteorder`) — 0 = BIG_ENDIAN, 1 = LITTLE_ENDIAN.
///
/// Note: `ByteBuffer.asCharBuffer()` (`servlet.rs::s2_bb_as_char_buffer`)
/// allocates its returned view under this same plain `java/nio/CharBuffer`
/// class name (not one of the `ByteBufferAsCharBuffer{B,L,RB,RL}` subclasses)
/// and separately stashes the source ByteBuffer's order in an indexed slot —
/// that slot is not read here, so an `asCharBuffer()` view built over a
/// non-native-order source ByteBuffer will report native order rather than
/// the source's actual order from this native. That's a narrower, pre-existing
/// gap in `s2_bb_as_char_buffer`'s view-class choice, independent of the
/// AbstractMethodError this registration fixes (real HeapCharBuffer/wrap/
/// allocate paths, which is what the affected Spring Boot classes exercise).
#[inline]
pub(crate) fn cb_native_order(_ctx: &dyn NativeContext, _buf: ObjectRef) -> i32 {
    if cfg!(target_endian = "big") {
        0
    } else {
        1
    }
}

/// Read one of `java.nio.Buffer`'s `int` fields, preferring the real-JDK named
/// slot and falling back to the synthetic indexed one.
///
/// These natives are registered on the ABSTRACT `java/nio/CharBuffer`, so a
/// receiver can be either a synthetic five-field object or a real
/// `StringCharBuffer` / `HeapCharBuffer` the JDK's own factory built. Reading
/// (or writing) the indexed slot on a real one lands wherever that index
/// happens to fall — for `StringCharBuffer` that is not `position` at all.
fn cb_read_int_field(ctx: &dyn NativeContext, buf: ObjectRef, name: &str, slot: usize) -> i32 {
    match ctx.get_field_by_name(buf, name) {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, slot) {
            Value::Int(v) => v,
            _ => 0,
        },
    }
}

/// Companion to [`cb_read_int_field`]: write both spellings, so whichever the
/// reader picks agrees.
fn cb_write_int_field(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    name: &str,
    slot: usize,
    value: i32,
) {
    ctx.set_field_by_name(buf, name, Value::Int(value));
    ctx.set_field(buf, slot, Value::Int(value));
}

/// `Buffer.limit()`, tolerant of both layouts.
fn cb_read_limit(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    cb_read_int_field(ctx, buf, "limit", CB_FIELD_LIMIT)
}

/// Read the `CharSequence` a `java.nio.StringCharBuffer` wraps, as text.
///
/// `NativeContext::read_string` only understands a real `java.lang.String`,
/// and `CharBuffer.wrap(CharSequence)` accepts anything — Tomcat's `CharChunk`
/// (`MessageBytes.toBytes` is `encoder.encode(CharBuffer.wrap(charChunk))`), a
/// `StringBuilder`, any application type. A native that stood in front of
/// `wrap` used to do this fallback at construction time and store the result
/// in a `char[]`; with `wrap` handed back to its own bytecode there is no
/// `char[]`, so the fallback belongs here, where the sequence is read.
///
/// Without it every non-`String` `CharSequence` reads back as empty, which is
/// silent: `encode(CharBuffer.wrap(charChunk))` produces zero bytes and
/// reports success.
pub(crate) fn read_wrapped_char_sequence(ctx: &mut dyn NativeContext, seq: ObjectRef) -> String {
    String::from_utf16_lossy(&read_wrapped_char_sequence_units(ctx, seq))
}

/// [`read_wrapped_char_sequence`] without the loss.
///
/// `CharBuffer.wrap(CharSequence)` STORES what this reads, so a `String`
/// return type here made `wrap(s).toString()` unable to give back the string it
/// was handed -- `G63-1` section 3 measured the `char[]` overload and section 6
/// N2 left this one open as "not native-collections, unowned".
///
/// The `String` spelling above is kept, and is still right for its other
/// callers: they hand the text to a charset encoder or a comparison, where a
/// Rust `String` is what is wanted and an unpaired surrogate is not
/// representable in the output anyway.
pub(crate) fn read_wrapped_char_sequence_units(
    ctx: &mut dyn NativeContext,
    seq: ObjectRef,
) -> Vec<u16> {
    let direct = ctx.read_string_units(seq).unwrap_or_default();
    if !direct.is_empty() {
        return direct;
    }
    match ctx.invoke_virtual(seq, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(text)))) => ctx.read_string_units(text).unwrap_or_default(),
        _ => direct,
    }
}

/// The text a CharBuffer currently exposes (`position..limit`), read straight
/// off the receiver rather than obtained by calling `toString()` on it.
///
/// Introduced as a workaround: a real `java.nio.StringCharBuffer` answered an
/// EMPTY string through `ctx.invoke_virtual` / `Method.invoke` while a direct
/// bytecode `cb.toString()` was correct. That defect is FIXED — a duplicate
/// `java/nio/CharBuffer.toString()` registration was shadowing the correct one
/// (stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md)
/// — and the `invoke_to_string_opt` caller that needed the workaround is gone.
///
/// It stays because it is still the right primitive for a caller that must read
/// a buffer's characters WITHOUT calling back into Java: `charsequence_chars`
/// uses it as the `AbstractStringBuilder.append(CharSequence)` fast path,
/// where the JDK is `charAt`-driven and re-entering for one virtual call per
/// character would be the only alternative.
///
/// Returns `None` for a receiver with neither a `str` nor an `hb`, so the
/// caller can fall back to ordinary dispatch.
pub(crate) fn cb_read_text(ctx: &mut dyn NativeContext, buf: ObjectRef) -> Option<String> {
    // A by-name read answers `Int(0)` for a field the receiver does not have,
    // indistinguishable from a real zero, so a named 0 means "ask the indexed
    // slot too". Safe for position/limit specifically: both layouts agree on 0
    // when it is genuinely 0, so they cannot disagree in the direction that
    // matters. NOT safe for `mark`, whose unset value is -1.
    fn coord(ctx: &dyn NativeContext, buf: ObjectRef, name: &str, slot: usize) -> i32 {
        if let Value::Int(v) = ctx.get_field_by_name(buf, name) {
            if v != 0 {
                return v;
            }
        }
        match ctx.get_field(buf, slot) {
            Value::Int(v) => v,
            _ => 0,
        }
    }
    let pos = coord(ctx, buf, "position", CB_FIELD_POS);
    let lim = coord(ctx, buf, "limit", CB_FIELD_LIMIT);
    let off = match ctx.get_field_by_name(buf, "offset") {
        Value::Int(v) => v.max(0),
        _ => 0,
    };
    if let Value::Object(Some(seq)) = ctx.get_field_by_name(buf, "str") {
        let units: Vec<u16> = read_wrapped_char_sequence_units(ctx, seq);
        let n = units.len() as i32;
        let lo = (off + pos).clamp(0, n) as usize;
        let hi = (off + lim).clamp(lo as i32, n) as usize;
        return Some(String::from_utf16_lossy(&units[lo..hi]));
    }
    let arr = cb_read_hb(ctx, buf)?;
    let n = ctx.array_length(arr) as i32;
    let lo = (off + pos).clamp(0, n);
    let hi = (off + lim).clamp(lo, n);
    let mut units: Vec<u16> = Vec::with_capacity((hi - lo).max(0) as usize);
    for i in lo..hi {
        if let Value::Int(v) = ctx.get_array_element(arr, i as usize) {
            units.push(v as u16);
        }
    }
    Some(String::from_utf16_lossy(&units))
}

/// `Objects.checkFromToIndex(from, to, length)`, the range check
/// `HeapCharBuffer.subSequence` opens with.
///
/// The class is `IndexOutOfBoundsException` and the message is
/// `Preconditions.outOfBoundsMessage`'s `checkFromToIndex` shape — verified
/// against HotSpot 25 in `probes/CharBufferWrapProbe`, which also records the
/// case this does NOT cover: `StringCharBuffer.subSequence` reaches
/// `Buffer.checkIndex` instead, whose formatter builds the exception with no
/// detail message at all. Both are the same class, so a `catch` cannot tell
/// them apart; only a message-exact differential can.
fn cb_check_from_to_index(from: i32, to: i32, length: i32) -> Result<(), MethodCallFailed> {
    if from >= 0 && from <= to && to <= length {
        return Ok(());
    }
    Err(RuntimeError::ioobe(
        cratonvm_types::error::out_of_bounds_message::check_from_to_index(
            i64::from(from),
            i64::from(to),
            i64::from(length),
        ),
    )
    .into())
}

pub(crate) fn register_p62_char_buffer(r: &mut NativeMethodRegistry) {
    use crate::servlet::s2_byte_order_object;
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cb = "java/nio/CharBuffer";
    r.register(cb, "allocate", "(I)Ljava/nio/CharBuffer;", |ctx, args| {
        let cap = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 16,
        };
        // `CharBuffer.allocate` opens with
        // `if (capacity < 0) throw createCapacityException(capacity)`.
        // Clamping to 0 instead handed back an empty buffer and reported
        // success, so a negative capacity — an arithmetic slip upstream —
        // surfaced later as an unexplained `BufferOverflowException`, or not
        // at all.
        if cap < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("capacity < 0: ({cap} < 0)"),
            }
            .into());
        }
        let buf = p62_alloc_char_buffer(ctx, cap as usize)?;
        Ok(Some(Value::Object(Some(buf))))
    });
    // `CharBuffer.wrap(char[])` and `CharBuffer.wrap(CharSequence)` are
    // DELIBERATELY not registered in real-JDK mode.
    //
    // These stamped the ABSTRACT `java/nio/CharBuffer`, so every `CharBuffer`
    // method without a native override dispatched to an abstract declaration
    // and threw `AbstractMethodError` (`slice()`, `duplicate()`,
    // `asReadOnlyBuffer()`), and `subSequence(int,int)` fell to the native
    // below, which bounds-checked nothing:
    // `CharBuffer.wrap("Hello, World").subSequence(0, 99)` handed back a
    // 99-character buffer, 87 code units of it read past the end of the
    // wrapped sequence. An earlier fix changed the stamp to the concrete
    // `java/nio/HeapCharBuffer`, which closes the `AbstractMethodError` half.
    //
    // Not registering them at all closes the rest. Their bytecode is
    // `wrap(array, 0, array.length)` / `wrap(csq, 0, csq.length())`, and the
    // THREE-argument forms have never been intercepted here — so they already
    // build a real `HeapCharBuffer` / `StringCharBuffer` and already match
    // HotSpot exactly, `capacity` and null message included
    // (`probes/CharBufferWrapProbe` rows 17-18). Letting the JDK's own factory
    // run additionally gets `wrap(String)` its real `StringCharBuffer`:
    // read-only, `hasArray() == false`, `put` refused, and the wrapped
    // sequence held rather than copied.
    //
    // Contrast `allocate` just above, which stamps the concrete class and is
    // correct on every one of those shapes for exactly that reason. The class
    // to stamp is the one the JDK builds, and the cheapest way to stamp it
    // right is not to stamp it at all.
    //
    // Synthetic-JDK mode keeps them: there is no `CharBuffer` bytecode there
    // to fall back to.
    #[cfg(feature = "synthetic-jdk")]
    {
        r.register(cb, "wrap", "([C)Ljava/nio/CharBuffer;", |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            // Pin across the buffer alloc below — a moving young GC there would
            // relocate the backing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            // HeapCharBuffer, NOT the abstract `java/nio/CharBuffer` — see the
            // comment above. Repro:
            // docs/known-issues/repros/charbuffer-address/CBSLICE.java
            let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            cb_write_hb(ctx, buf, arr, len as i32);
            Ok(Some(Value::Object(Some(buf))))
        });
        r.register(
            cb,
            "wrap",
            "(Ljava/lang/CharSequence;)Ljava/nio/CharBuffer;",
            |ctx, args| {
                // The arg is any CharSequence, not necessarily a String. For a
                // real String `read_string` works; for other implementations
                // (e.g. Tomcat's `CharChunk`, StringBuilder, CharBuffer) it
                // returns None/empty, so fall back to a virtual `toString()`.
                // Without this fallback `CharBuffer.wrap(charChunk)` produced an
                // empty buffer, so `MessageBytes.toBytes` (which does
                // `encoder.encode(CharBuffer.wrap(charC))`) encoded nothing —
                // 144 Tomcat MessageBytes-conversion failures. Real-JDK mode
                // needs none of this: `StringCharBuffer` holds the
                // `CharSequence` itself and indexes it directly.
                // UNITS, and through the shared reader rather than a fourth
                // open-coded copy of it. `wrap` STORES what it reads here, so a
                // Rust `String` in the middle meant `wrap(seq).toString()` could
                // not give back the sequence it was handed. The comment above
                // describes exactly this read; it is now one function.
                let chars: Vec<u16> = match args.first() {
                    Some(Value::Object(Some(s))) => read_wrapped_char_sequence_units(ctx, *s),
                    _ => Vec::new(),
                };
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, chars.len());
                for (i, &ch) in chars.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(ch as i32));
                }
                // Pin across the buffer alloc below — a moving young GC there
                // would relocate the backing array (native stale-local family).
                let arr_pin = ctx.pin_native_root(arr);
                // HeapCharBuffer for the same reason as `wrap([C)` above.
                let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5)?;
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                cb_write_hb(ctx, buf, arr, chars.len() as i32);
                Ok(Some(Value::Object(Some(buf))))
            },
        );
    }
    // hasArray()Z — JDK bytecode reads `hb != null && !isReadOnly`.
    // Mirror that with a robust slot/name lookup so synthetic-mode
    // CharBuffers (where slot 0 may have been overwritten by
    // descriptor-coerced int writes against Buffer.mark) still
    // report true. Without this, icu4j's `b16BitUnits.subSequence(
    // start, end).toString()` reads a null `hb` and throws
    // `IllegalStateException("CharBuffer has no backing array")`.
    //
    // The `&& !isReadOnly` half was MISSING: this answered purely "is there an
    // hb?", so `CharBuffer.allocate(4).asReadOnlyBuffer()` — a real
    // `HeapCharBufferR`, which does have an `hb` — reported `hasArray() == true`
    // where HotSpot 25.0.3+9 reports `false`
    // (`probes/BufferAccessibleArrayProbe`, row `rcb.hasArray`). A caller that
    // trusts `hasArray()` then reaches for `array()` and mutates the backing
    // store of a buffer the JDK says is read-only. The ByteBuffer twin
    // (`servlet.rs::register_s2_bytebuffer`, `hasArray()Z`) has carried the
    // second half all along; this is the drifted copy.
    r.register(cb, "hasArray", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let access =
            buffer_array_access(cb_read_hb(ctx, this).is_some(), cb_is_read_only(ctx, this));
        Ok(Some(Value::Int(i32::from(
            access == BufferArrayAccess::Accessible,
        ))))
    });
    // CharBuffer.isReadOnly()/isDirect() are abstract in the real JDK (each
    // concrete Heap/Direct/View subclass overrides them) — every OTHER typed
    // view buffer (Int/Long/Short/Float/DoubleBuffer) gets these registered
    // directly on its own class, but CharBuffer was missing from that list.
    // Objects allocated straight against the literal `java/nio/CharBuffer`
    // class (this file's `wrap`/`allocate` above use it directly rather than
    // a Heap-prefixed subclass) then hit the abstract declaration on
    // `isReadOnly()` — "AbstractMethodError: java/nio/Buffer.isReadOnly()Z
    // has no Code attribute" (CratonVM resolves the abstract method all the
    // way up to `Buffer` because neither `CharBuffer` nor `Buffer` had a
    // native registered) — killing any caller whose real-JDK bytecode reads
    // `hasArray()`/`isReadOnly()` on a CharBuffer (e.g. Lucene's vector codec
    // tests decoding index metadata strings).
    //
    // The body used to open-code the field read a FOURTH time, and not
    // identically: `match … { Value::Int(v) => v, _ => 0 }` returns the raw
    // field value where [`cb_is_read_only`] — which `hasArray`/`array`/
    // `arrayOffset` on this same receiver consult — normalises it to 0/1. Any
    // `isReadOnly` other than 0 or 1 made `isReadOnly()` and `hasArray()`
    // disagree about one receiver. It is the same read now.
    r.register(cb, "isReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(cb_is_read_only(ctx, this)))))
    });
    // isDirect()Z — STUB-REMOVAL (wave 3): was a flat `false`. Derive it, the
    // same way `hasArray` above derives from the backing array, so a genuinely
    // direct buffer is never mis-reported: a CharBuffer with an `hb` char[] is
    // heap-backed by definition. Everything this VM produces today lands in
    // that branch — `allocate`/`wrap` above build an `hb`, and
    // `ByteBuffer.asCharBuffer()` (`servlet.rs::s2_bb_as_char_buffer`)
    // TRANSCODES into a fresh char[] instead of aliasing direct memory even
    // when its source ByteBuffer is direct — so `false` stays the answer; only
    // a future `Direct*`-classed CharBuffer flips it.
    r.register(cb, "isDirect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if cb_read_hb(ctx, this).is_some() {
            return Ok(Some(Value::Int(0)));
        }
        let cid = ctx.class_id_of_object(this);
        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
        let direct = cname
            .rsplit('/')
            .next()
            .is_some_and(|leaf| leaf.starts_with("Direct"));
        Ok(Some(Value::Int(i32::from(direct))))
    });
    // order()Ljava/nio/ByteOrder; — abstract on CharBuffer, like isReadOnly/
    // isDirect above; every concrete leaf subclass overrides it in real
    // OpenJDK. CratonVM never registered a native anywhere in the CharBuffer
    // hierarchy, so any first-use call (real-JDK bytecode resolves the
    // abstract declaration with no override) throws
    // "AbstractMethodError: java/nio/CharBuffer.order()Ljava/nio/ByteOrder;
    // has no Code attribute" — hit by ICU4X's `UCharacterProperty.<clinit>`
    // reading Unicode property data via `CharBuffer.get(int[])`, which
    // transitively poisons `java.net.IDN.<clinit>` (the first caller to ever
    // exercise this path) for the rest of the process. Plain `CharBuffer`/
    // `HeapCharBuffer`/`StringCharBuffer` report the platform's native byte
    // order, matching real `HeapCharBuffer.order()`; the four
    // `ByteBufferAsCharBuffer{B,L,RB,RL}` view classes encode their
    // endianness in the class-name suffix (B/RB = big, L/RL = little),
    // matching real JDK's per-view-class `order()` overrides.
    r.register(cb, "order", "()Ljava/nio/ByteOrder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ord = cb_native_order(ctx, this);
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
    });
    fn cb_order_big(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        use crate::servlet::s2_byte_order_object;
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 0)?))))
    }
    fn cb_order_little(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        use crate::servlet::s2_byte_order_object;
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 1)?))))
    }
    for subclass in [
        "java/nio/ByteBufferAsCharBufferB",
        "java/nio/ByteBufferAsCharBufferRB",
    ] {
        r.register(subclass, "order", "()Ljava/nio/ByteOrder;", cb_order_big);
    }
    for subclass in [
        "java/nio/ByteBufferAsCharBufferL",
        "java/nio/ByteBufferAsCharBufferRL",
    ] {
        r.register(subclass, "order", "()Ljava/nio/ByteOrder;", cb_order_little);
    }
    // `arrayOffset()` has the SAME three-way split as `array()` below and had
    // NEITHER refusal — a `StringCharBuffer` and a read-only heap buffer both
    // answered a number, and `0` is a perfectly ordinary offset, so
    // `hasArray()`-less code that reached for the offset got a plausible answer
    // instead of the exception that tells it to take the other path.
    //
    // Transcribed from the `array()` arm below through the shared
    // [`buffer_array_access`] rather than written afresh, so the two cannot drift.
    // This is the same repair `servlet.rs`'s `arrayOffset()I` already carries on
    // the ByteBuffer side (record: W7-83 §7.1).
    r.register(cb, "arrayOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match buffer_array_access(cb_read_hb(ctx, this).is_some(), cb_is_read_only(ctx, this)) {
            BufferArrayAccess::Absent => Err(cb_no_backing_array()),
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            BufferArrayAccess::Accessible => match ctx.get_field_by_name(this, "offset") {
                Value::Int(v) => Ok(Some(Value::Int(v))),
                _ => Ok(Some(Value::Int(0))),
            },
        }
    });
    // toString(II) / toString() — abstract on CharBuffer; subSequence and
    // wrap return synthetic instances with class `java/nio/CharBuffer`
    // itself, so the default `toString()` body (which calls
    // `toString(position(), limit())` on this) hits an abstract method
    // and throws AbstractMethodError. ICUBinary.getString uses exactly
    // this pattern (`bytes.asCharBuffer().subSequence(0,len).toString()`)
    // during ICU normalization data load, which blocks ICU clinit and
    // cascades into Jetty `Main.processCommandLine` NPE.
    fn cb_to_string_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let start = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let end = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let cur_pos = match ctx.get_field_by_name(this, "position") {
            Value::Int(v) => v,
            _ => match ctx.get_field(this, CB_FIELD_POS) {
                Value::Int(v) => v,
                _ => 0,
            },
        };
        let cur_off = match ctx.get_field_by_name(this, "offset") {
            Value::Int(v) => v,
            _ => 0,
        };
        // StringCharBuffer has no `hb`; it stores the wrapped CharSequence in
        // `str` and uses the same position/limit/offset coordinates as the
        // JDK bytecode. This is reached by Netty's
        // `CharBuffer.wrap(String,start,end).toString()` cookie path.
        let cid = ctx.class_id_of_object(this);
        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
        if cname == "java/nio/StringCharBuffer" {
            let str_obj = match ctx.get_field_by_name(this, "str") {
                Value::Object(Some(o)) => o,
                _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
            };
            // Units on the way in AND on the way out (below): this branch lost
            // them twice, once decoding the wrapped sequence to host text and
            // once re-encoding the slice with `from_utf16_lossy`.
            let units: Vec<u16> = read_wrapped_char_sequence_units(ctx, str_obj);
            // StringCharBuffer.toString(int,int) receives absolute buffer
            // indexes (CharBuffer.toString() calls it with position..limit),
            // and StringCharBuffer then adds only `offset` before slicing
            // the wrapped CharSequence. Do not add current position again.
            let abs_start = cur_off + start;
            let abs_end = cur_off + end;
            let len = units.len() as i32;
            let s_lo = abs_start.max(0).min(len);
            let s_hi = abs_end.max(s_lo).min(len);
            let out = &units[s_lo as usize..s_hi as usize];
            return Ok(Some(Value::Object(Some(ctx.create_string_from_units(out)))));
        }
        let arr = match cb_read_hb(ctx, this) {
            Some(a) => a,
            None => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
        };
        // CharBuffer.toString(int start, int end) reads start..end (exclusive)
        // RELATIVE to the current position — see HeapCharBuffer.toString.
        let abs_start = cur_off + cur_pos + start;
        let abs_end = cur_off + cur_pos + end;
        let arr_len = ctx.array_length(arr) as i32;
        let s_lo = abs_start.max(0).min(arr_len);
        let s_hi = abs_end.max(s_lo).min(arr_len);
        let mut chars: Vec<u16> = Vec::with_capacity((s_hi - s_lo) as usize);
        for i in s_lo..s_hi {
            if let Value::Int(v) = ctx.get_array_element(arr, i as usize) {
                chars.push(v as u16);
            }
        }
        // The units are already in hand; `from_utf16_lossy` + `create_string`
        // was the only thing losing them. MEASURED on both VMs at
        // `c77fc76a1`, `CharBuffer.wrap(new char[]{'a', 0xD800, 'b'})`:
        //
        //   toString()   HotSpot 61,d800,62    CratonVM 61,fffd,62
        //
        // `sb_string_from_units` takes the plain-text path when the units are
        // representable, so every buffer that renders correctly today is
        // unchanged. G63-1 N2.
        let out = crate::lang_string::sb_string_from_units(ctx, &chars)?;
        Ok(Some(Value::Object(Some(out))))
    }
    r.register(cb, "toString", "(II)Ljava/lang/String;", cb_to_string_range);
    // Concrete CharBuffer subclasses inherit our toString(II) only when the
    // VM's vtable-lookup walks the super-class native registry. The
    // ByteBufferAs*CharBuffer family and HeapCharBuffer are what
    // `ByteBuffer.asCharBuffer()` returns in real-JDK mode, so registering
    // directly on each ensures dispatch hits us regardless of how vtable
    // resolution handles abstract-in-base + native-on-base.
    for subclass in [
        "java/nio/ByteBufferAsCharBufferB",
        "java/nio/ByteBufferAsCharBufferL",
        "java/nio/ByteBufferAsCharBufferRB",
        "java/nio/ByteBufferAsCharBufferRL",
        "java/nio/HeapCharBuffer",
        "java/nio/HeapCharBufferR",
        "java/nio/StringCharBuffer",
    ] {
        r.register(
            subclass,
            "toString",
            "(II)Ljava/lang/String;",
            cb_to_string_range,
        );
    }
    r.register(cb, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let pos = match ctx.get_field_by_name(this, "position") {
            Value::Int(v) => v,
            _ => match ctx.get_field(this, CB_FIELD_POS) {
                Value::Int(v) => v,
                _ => 0,
            },
        };
        let lim = match ctx.get_field_by_name(this, "limit") {
            Value::Int(v) => v,
            _ => match ctx.get_field(this, CB_FIELD_LIMIT) {
                Value::Int(v) => v,
                _ => pos,
            },
        };
        // toString() is documented as toString(position(), limit()). For
        // synthetic/heap buffers our `cb_to_string_range` historically treats
        // the range as relative, so keep the old 0..remaining call here.
        //
        // BUT the range convention differs by receiver, and this registration
        // is reached by BOTH routes while the `StringCharBuffer` one below is
        // reached by only one. `StringCharBuffer` does not declare
        // `toString()` — the JDK inherits it from `CharBuffer` — so
        // `Class.getMethod("toString")` resolves to the DECLARING class and
        // reflection / `Method.invoke` / `ctx.invoke_virtual` land HERE, while
        // a bytecode `invokevirtual` dispatches on the receiver and lands
        // there. Measured 2026-08-13 on `CharBuffer.wrap("x00ff0a80y", 1, 9)`:
        // bytecode gave `00ff0a80` (correct) and the reflective route gave
        // `x00ff0a8` — the right LENGTH from the wrong ORIGIN, because this
        // arm passed a relative `0` for a receiver whose window lives in
        // `position`. `HexFormat.parseHex(CharSequence)` is specified as
        // `parseHex(CharBuffer.wrap(...))`, so it read the unsliced text and
        // threw `NumberFormatException: not a hexadecimal digit: "x"`.
        //
        // Dispatch on the receiver's real class so ONE implementation serves
        // both routes; the twin below stays as the direct-dispatch entry.
        let is_string_cb = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .map(|n| n == "java/nio/StringCharBuffer")
            .unwrap_or(false);
        let args2 = if is_string_cb {
            // Absolute: JDK `StringCharBuffer.toString(start, end)` is
            // `str.subSequence(start + offset, end + offset)`.
            [Value::Object(Some(this)), Value::Int(pos), Value::Int(lim)]
        } else {
            [
                Value::Object(Some(this)),
                Value::Int(0),
                Value::Int(lim - pos),
            ]
        };
        cb_to_string_range(ctx, &args2)
    });
    r.register(
        "java/nio/StringCharBuffer",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let pos = match ctx.get_field_by_name(this, "position") {
                Value::Int(v) => v,
                _ => 0,
            };
            let lim = match ctx.get_field_by_name(this, "limit") {
                Value::Int(v) => v,
                _ => pos,
            };
            let args2 = [Value::Object(Some(this)), Value::Int(pos), Value::Int(lim)];
            cb_to_string_range(ctx, &args2)
        },
    );
    // subSequence(II)Ljava/nio/CharBuffer; — abstract on CharBuffer, so
    // an unbacked synthetic instance would AbstractMethodError. Allocate
    // a fresh CharBuffer with the same backing array and adjusted
    // position/limit (start..end relative to current position).
    //
    // Only reachable for receivers stamped with the ABSTRACT
    // `java/nio/CharBuffer` — anything concrete (`HeapCharBuffer` from
    // `allocate`, `StringCharBuffer` / `HeapCharBuffer` from the real `wrap`)
    // runs its own bytecode instead and gets these checks from
    // `Objects.checkFromToIndex` for free. `ByteBufferAsCharBuffer
    // .subSequence` still produces abstract-stamped receivers, so this stays.
    r.register(
        cb,
        "subSequence",
        "(II)Ljava/nio/CharBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let start = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let end = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let arr = match cb_read_hb(ctx, this) {
                Some(a) => a,
                None => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CharBuffer.subSequence: no backing array".into(),
                    }
                    .into())
                }
            };
            let cur_pos = cb_read_int_field(ctx, this, "position", CB_FIELD_POS);
            let cur_lim = cb_read_limit(ctx, this);
            let cur_cap = cb_read_int_field(ctx, this, "capacity", CB_FIELD_CAPACITY);
            // `HeapCharBuffer.subSequence` opens with
            // `Objects.checkFromToIndex(start, end, limit() - position())`,
            // and this checked NOTHING: `wrap("Hello, World").subSequence(0,
            // 99)` returned a 99-character buffer, 87 code units of it read
            // past the end of the backing array and handed back as content.
            // A negative `start` was worse still — it produced a buffer whose
            // own `position()` was negative.
            cb_check_from_to_index(start, end, cur_lim.saturating_sub(cur_pos))?;
            let cur_off = match ctx.get_field_by_name(this, "offset") {
                Value::Int(v) => v,
                _ => 0,
            };
            // Read BEFORE the allocation below: `this` is what the flag comes
            // from, and it must be captured while `ctx` is only borrowed
            // immutably.
            let src_read_only = cb_is_read_only(ctx, this);
            let new_pos = cur_pos + start;
            let new_lim = cur_pos + end;
            // Allocate a fresh HeapCharBuffer pointing at the same char[]
            // — JDK's HeapCharBuffer.subSequence does the same.
            // Pin across the buffer alloc below — a moving young GC there
            // would relocate the backing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
            ctx.set_field_by_name(buf, "offset", Value::Int(cur_off));
            // A view of a read-only buffer is READ-ONLY. This wrote a flat 0,
            // so `subSequence` was a documented laundering route out of the
            // read-only state — the result was array-backed and writable.
            // MEASURED, jdk-25.0.3+9: `CharBuffer.allocate(4).asReadOnlyBuffer()
            // .subSequence(0, 2).isReadOnly()` is `true`, as are `.slice()` and
            // `.duplicate()` of the same receiver, and
            // `CharBuffer.wrap("abcd").slice().isReadOnly()`.
            ctx.set_field_by_name(buf, "isReadOnly", Value::Int(i32::from(src_read_only)));
            ctx.set_field_by_name(buf, "position", Value::Int(new_pos));
            ctx.set_field_by_name(buf, "limit", Value::Int(new_lim));
            // The slice keeps the PARENT's capacity, not its own limit — the
            // JDK passes `capacity()` straight through, and `capacity()` is
            // observable (`CharBuffer.clear()` widens the limit back out to
            // it). Writing `new_lim` here made every subsequence look like a
            // buffer that had been allocated at exactly its own length.
            ctx.set_field_by_name(buf, "capacity", Value::Int(cur_cap));
            ctx.set_field_by_name(buf, "mark", Value::Int(-1));
            // Guarded for the same reason as `cb_write_hb`: on a real layout
            // slot 0 is `mark` and slot 4 is `address`.
            if cb_synthetic_layout(ctx, buf) {
                ctx.set_field(buf, CB_FIELD_ARRAY, Value::Object(Some(arr)));
                ctx.set_field(buf, CB_FIELD_POS, Value::Int(new_pos));
                ctx.set_field(buf, CB_FIELD_LIMIT, Value::Int(new_lim));
                ctx.set_field(buf, CB_FIELD_CAPACITY, Value::Int(cur_cap));
                ctx.set_field(buf, CB_FIELD_MARK, Value::Int(-1));
            }
            cb_write_heap_address(ctx, buf, cur_off);
            Ok(Some(Value::Object(Some(buf))))
        },
    );
    r.register(cb, "get", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, CB_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        let lim = match ctx.get_field(this, CB_FIELD_LIMIT) {
            Value::Int(v) => v,
            _ => 0,
        };
        if pos >= lim {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let arr = match ctx.get_field(this, CB_FIELD_ARRAY) {
            Value::Object(Some(a)) => a,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "no backing array".into(),
                }
                .into())
            }
        };
        let ch = ctx.get_array_element(arr, pos as usize);
        ctx.set_field(this, CB_FIELD_POS, Value::Int(pos + 1));
        Ok(Some(ch))
    });
    r.register(cb, "get", "(I)C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let lim = match ctx.get_field(this, CB_FIELD_LIMIT) {
            Value::Int(v) => v,
            _ => 0,
        };
        if idx < 0 || idx >= lim {
            // `CharBuffer.get(int)` lands in `Buffer.checkIndex`, whose
            // formatter builds an `IndexOutOfBoundsException` with no detail
            // message. `IllegalArgumentException("index: N")` — what this
            // raised — is not in that hierarchy at all, so a
            // `catch (IndexOutOfBoundsException)` around a buffer read did not
            // see it and the throw escaped as an unrelated failure.
            return Err(RuntimeError::ioobe_no_message().into());
        }
        let arr = match ctx.get_field(this, CB_FIELD_ARRAY) {
            Value::Object(Some(a)) => a,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "no backing array".into(),
                }
                .into())
            }
        };
        Ok(Some(ctx.get_array_element(arr, idx as usize)))
    });
    r.register(cb, "put", "(C)Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // READ-ONLY FIRST, before the overflow check. Every read-only
        // `CharBuffer` subclass overrides the mutators with a bare
        // `throw new ReadOnlyBufferException()` and never consults
        // position/limit, so the refusal outranks both of the bounds classes.
        // MEASURED, jdk-25.0.3+9: a buffer that is read-only AND full answers
        // `ReadOnlyBufferException`, where the same buffer writable answers
        // `BufferOverflowException`; an absolute `put(9, 'z')` past the end of a
        // read-only buffer answers `ReadOnlyBufferException`, not
        // `IndexOutOfBoundsException`.
        //
        // Without this a read-only receiver reaching this native (any receiver
        // whose resolved `put` has no `Code` — the synthetic-JDK build, and
        // CratonVM-minted abstract-stamped `java/nio/CharBuffer` instances)
        // silently WROTE, which is worse than a wrong exception class: it
        // mutates storage the JDK guarantees is immutable through this handle.
        if cb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let ch = args.get(1).copied().unwrap_or(Value::Int(0));
        let pos = match ctx.get_field(this, CB_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        let lim = match ctx.get_field(this, CB_FIELD_LIMIT) {
            Value::Int(v) => v,
            _ => 0,
        };
        if pos >= lim {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let arr = match ctx.get_field(this, CB_FIELD_ARRAY) {
            Value::Object(Some(a)) => a,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "no backing array".into(),
                }
                .into())
            }
        };
        ctx.set_array_element(arr, pos as usize, ch);
        ctx.set_field(this, CB_FIELD_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "flip", "()Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, CB_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, CB_FIELD_LIMIT, Value::Int(pos));
        ctx.set_field(this, CB_FIELD_POS, Value::Int(0));
        cb_set_mark(ctx, this, -1);
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "clear", "()Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cap = match ctx.get_field(this, CB_FIELD_CAPACITY) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, CB_FIELD_POS, Value::Int(0));
        ctx.set_field(this, CB_FIELD_LIMIT, Value::Int(cap));
        cb_set_mark(ctx, this, -1);
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "rewind", "()Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, CB_FIELD_POS, Value::Int(0));
        cb_set_mark(ctx, this, -1);
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "remaining", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, CB_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        let lim = match ctx.get_field(this, CB_FIELD_LIMIT) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int((lim - pos).max(0))))
    });
    r.register(cb, "hasRemaining", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, CB_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        let lim = match ctx.get_field(this, CB_FIELD_LIMIT) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if pos < lim { 1 } else { 0 })))
    });
    r.register(cb, "position", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, CB_FIELD_POS)))
    });
    r.register(cb, "position", "(I)Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_pos = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        // `Buffer.position(int)` is `if (newPosition > limit | newPosition < 0)
        // throw createPositionException(...)`, an IllegalArgumentException.
        // Validating is not pedantry here: `StringCharBuffer.subSequence`
        // builds its result through the `Buffer` constructor and CATCHES that
        // IAE to raise `IndexOutOfBoundsException`, so an unchecked write is
        // one reason `wrap("Hello, World").subSequence(3, 2)` returned a
        // buffer with position 3 and limit 2 instead of throwing.
        let lim = cb_read_limit(ctx, this);
        if new_pos > lim {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("newPosition > limit: ({new_pos} > {lim})"),
            }
            .into());
        }
        if new_pos < 0 {
            // `createPositionException` distinguishes the two causes; a
            // negative index reported as "> limit" reads as nonsense
            // (`newPosition > limit: (-1 > 5)`).
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("newPosition < 0: ({new_pos} < 0)"),
            }
            .into());
        }
        cb_write_int_field(ctx, this, "position", CB_FIELD_POS, new_pos);
        // Same closing line as `limit(int)`: `if (mark > newPosition) mark =
        // -1;`. Fixed on the argument, not on a measured row -- the probe's
        // ordering happens not to leave a mark above a lowered position -- but
        // it is the same two-line contract and leaving one half of it undone is
        // how the `limit` half survived this long.
        if cb_read_int_field(ctx, this, "mark", CB_FIELD_MARK) > new_pos {
            cb_set_mark(ctx, this, -1);
        }
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "limit", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(cb_read_limit(ctx, this))))
    });
    r.register(cb, "limit", "(I)Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_lim = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        // `Buffer.limit(int)`: `if (newLimit > capacity | newLimit < 0) throw
        // createLimitException(...)`, and the position follows the limit down.
        let cap = cb_read_int_field(ctx, this, "capacity", CB_FIELD_CAPACITY);
        if new_lim > cap {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("newLimit > capacity: ({new_lim} > {cap})"),
            }
            .into());
        }
        if new_lim < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("newLimit < 0: ({new_lim} < 0)"),
            }
            .into());
        }
        cb_write_int_field(ctx, this, "limit", CB_FIELD_LIMIT, new_lim);
        if cb_read_int_field(ctx, this, "position", CB_FIELD_POS) > new_lim {
            cb_write_int_field(ctx, this, "position", CB_FIELD_POS, new_lim);
        }
        // `Buffer.limit(int)` ends `if (mark > newLimit) mark = -1;`. Without
        // it a mark left ABOVE the new limit survives, and the next `reset()`
        // sets the position past the limit instead of raising
        // `InvalidMarkException` -- a buffer whose position exceeds its limit,
        // which every `remaining()`-driven loop then reads as empty.
        //
        // MEASURED with `apps/probes/L4BridgeSweep.java` (`position(3);
        // mark(); limit(2); reset()`), heap, view and `wrap(CharSequence)`.
        if cb_read_int_field(ctx, this, "mark", CB_FIELD_MARK) > new_lim {
            cb_set_mark(ctx, this, -1);
        }
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "capacity", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, CB_FIELD_CAPACITY)))
    });
    // The three-way split, in the JDK's order — see [`buffer_array_access`].
    //
    // This used to be ONE branch: no `hb` raised
    // `IllegalStateException("CharBuffer.array: no backing array")`, a class
    // that is in neither of the two hierarchies the JDK uses here, so neither a
    // `catch (UnsupportedOperationException)` nor a
    // `catch (ReadOnlyBufferException)` around a buffer access saw it. And the
    // read-only cell was missing entirely: an array-backed READ-ONLY buffer
    // handed its backing `char[]` straight out, which is a mutable alias to
    // storage the JDK refuses to expose — a wrong ANSWER on the two throwing
    // cells and a wrong CAPABILITY on the third.
    //
    // MEASURED, jdk-25.0.3+9, `probes/BufferAccessibleArrayProbe`:
    //   scb.array  java.lang.UnsupportedOperationException   (StringCharBuffer)
    //   rcb.array  java.nio.ReadOnlyBufferException          (HeapCharBufferR)
    //   hcb.array  OK 4                                      (HeapCharBuffer)
    r.register(cb, "array", "()[C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let hb = cb_read_hb(ctx, this);
        match buffer_array_access(hb.is_some(), cb_is_read_only(ctx, this)) {
            BufferArrayAccess::Absent => Err(cb_no_backing_array()),
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            // `hb` is `Some` on this arm by construction of `buffer_array_access`.
            BufferArrayAccess::Accessible => Ok(Some(Value::Object(hb))),
        }
    });
    // `java/nio/CharBuffer.toString()` is registered ONCE, above, delegating to
    // `cb_to_string_range`. A second registration of the same triple used to sit
    // here, reading only the backing `char[]` through `cb_read_hb` and returning
    // `""` when there was none. Being later, it WON -- and a real
    // `java.nio.StringCharBuffer` has no `hb` (it holds the wrapped CharSequence
    // in `str`), so every route that resolved `toString()` against the DECLARING
    // class got an empty string.
    //
    // A direct bytecode `cb.toString()` was unaffected because it resolves the
    // exact class and finds `StringCharBuffer.toString()` (registered separately,
    // and correct). Reflection and `NativeContext::invoke_virtual` both land in
    // `invoke_on_class_shared`, which resolves the inherited declaration on
    // `java/nio/CharBuffer` -- so `Method.invoke(CharBuffer.toString(), scb)`
    // answered `""` while `scb.toString()` answered the text.
    // See stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md.

    // Also register under Buffer parent
    let buf = "java/nio/Buffer";
    r.register(buf, "position", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1))) // pos field
    });
    r.register(buf, "limit", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(buf, "capacity", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.set_category(__prev_cat);
    ()
}

pub(crate) fn p62_alloc_char_buffer(
    ctx: &mut dyn NativeContext,
    cap: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, cap);
    // Use HeapCharBuffer (concrete) not CharBuffer (abstract) so real-JDK
    // bytecode methods like compact() dispatch correctly.
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate the backing array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    cb_write_hb(ctx, buf, arr, cap as i32);
    Ok(buf)
}

#[cfg(test)]
mod cb_layout_tests {
    use super::*;
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::ClassId;

    /// On a real-JDK-shaped CharBuffer the indexed `CB_FIELD_*` writes must
    /// not happen at all: slot 0 is `mark` and slot 4 is `address`.
    ///
    /// The `mark` half is what `Buffer.<init>`'s `mark > position` check
    /// caught. Before the guard, `cb_write_hb` left the backing `char[]`'s
    /// reference sitting in `mark`, and `CharBuffer.allocate(12).duplicate()`
    /// — which passes `markValue()` straight into the constructor — threw
    /// `IllegalArgumentException: mark > position: (52784352 > 0)`.
    ///
    /// Mirrors `servlet.rs::bb_write_hb_real_layout_preserves_address_and_mark`,
    /// which pins the identical hazard on the ByteBuffer side.
    #[test]
    fn cb_write_hb_real_layout_preserves_address_and_mark() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ctx
            .ensure_class_initialized("java/nio/CharBuffer")
            .expect("class init");
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, 12);
        let buf = ctx.alloc_object(class_id, 10);

        cb_write_hb(&mut ctx, buf, arr, 12);

        // Slot 0 is the real `mark`. The indexed fallback would put the
        // backing `char[]`'s reference there — the exact corruption
        // `Buffer.<init>`'s `mark > position` check surfaced. Assert on the
        // SLOTS rather than the names: this is what the guard controls, and
        // it is observable without the field-name metadata a mock context
        // does not carry for this class.
        assert_ne!(
            ctx.get_field(buf, CB_FIELD_ARRAY),
            Value::Object(Some(arr)),
            "slot 0 is the real Buffer.mark — the indexed fallback must be suppressed here"
        );
        // Slot 4 is the real `address`; the indexed fallback would put -1
        // there, which is the defect `cb_write_heap_address` exists to undo.
        assert_ne!(
            ctx.get_field(buf, CB_FIELD_MARK),
            Value::Int(-1),
            "slot 4 is the real Buffer.address — the indexed fallback must be suppressed here"
        );
    }

    /// The full `hasArray`/`array`/`arrayOffset` truth table, as measured on
    /// HotSpot 25.0.3+9 (`probes/BufferAccessibleArrayProbe`).
    ///
    /// Asserted on the pure decision function rather than through a
    /// `MockNativeContext`: the mock resolves field NAMES through a slot
    /// fallback, so a receiver-shaped test here would be measuring the mock's
    /// name-to-slot table and not the ordering this function exists to fix.
    /// The two inputs are exactly the two fields the JDK bodies read.
    ///
    /// The function is now `cratonvm_native_io::buffer_array_access` and the
    /// local `cb_array_access` is gone (F14-1 N1). These three tests are KEPT
    /// rather than deleted as duplicates of `native-io`'s own: they assert
    /// from the CONSUMER side, so a future lane that re-introduces a local
    /// copy has to delete an assertion to do it, and the CharBuffer-specific
    /// receivers each cell names (`StringCharBuffer`, `ByteBufferAsCharBufferB`)
    /// are witnesses `native-io`'s ByteBuffer-side rows do not carry.
    #[test]
    fn charbuffer_natives_use_the_shared_jdk_three_way_table() {
        // HeapCharBuffer — `CharBuffer.allocate(4)`.
        assert_eq!(
            buffer_array_access(true, false),
            BufferArrayAccess::Accessible
        );
        // HeapCharBufferR — `.asReadOnlyBuffer()`. HotSpot: hasArray() == false,
        // array() throws ReadOnlyBufferException.
        assert_eq!(buffer_array_access(true, true), BufferArrayAccess::ReadOnly);
        // ByteBufferAsCharBufferB — a writable VIEW with no `hb`. HotSpot:
        // array() throws UnsupportedOperationException even though it is not
        // read-only, which is why "not accessible" cannot be one branch.
        assert_eq!(buffer_array_access(false, false), BufferArrayAccess::Absent);
    }

    /// The ORDER, pinned on its own because it is the single cell a
    /// read-only-first implementation gets wrong.
    ///
    /// `CharBuffer.wrap("abcd")` is a `StringCharBuffer`: read-only AND
    /// array-less at the same time. The JDK checks `hb == null` FIRST, so it
    /// answers `UnsupportedOperationException`, not `ReadOnlyBufferException` —
    /// and the two are unrelated classes, so a `catch` cannot absorb the
    /// difference. This is the row `RJdkIntrinsics2 --only=bounds` asserts.
    #[test]
    fn shared_table_checks_absent_before_read_only_for_stringcharbuffer() {
        assert_eq!(buffer_array_access(false, true), BufferArrayAccess::Absent);
    }

    /// Only ONE of the three states admits the array, and it is the one where
    /// `hasArray()` is true. A mutation that made `hasArray()` derive from
    /// anything but this function would let a read-only buffer hand out a
    /// mutable alias to its backing store.
    #[test]
    fn exactly_one_state_hands_the_array_over() {
        let states = [
            buffer_array_access(false, false),
            buffer_array_access(false, true),
            buffer_array_access(true, false),
            buffer_array_access(true, true),
        ];
        assert_eq!(
            states
                .iter()
                .filter(|s| **s == BufferArrayAccess::Accessible)
                .count(),
            1
        );
    }

    /// A genuinely synthetic carrier has no field-name metadata, so the
    /// indexed fallback is the only way these natives round-trip state. Guard
    /// against the fix above suppressing it for the case it exists to serve.
    #[test]
    fn cb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let class_id = ClassId::new(9999); // never registered by name
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, 8);
        let buf = ctx.alloc_object(class_id, 5);

        cb_write_hb(&mut ctx, buf, arr, 8);

        assert_eq!(ctx.get_field(buf, CB_FIELD_ARRAY), Value::Object(Some(arr)));
        assert_eq!(ctx.get_field(buf, CB_FIELD_POS), Value::Int(0));
        assert_eq!(ctx.get_field(buf, CB_FIELD_LIMIT), Value::Int(8));
        assert_eq!(ctx.get_field(buf, CB_FIELD_CAPACITY), Value::Int(8));
        assert_eq!(ctx.get_field(buf, CB_FIELD_MARK), Value::Int(-1));
    }
}
