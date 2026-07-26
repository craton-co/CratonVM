// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.nio.charset` and buffer natives: Charset, CharsetEncoder/Decoder, CharBuffer.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
        let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
        let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
            let obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1);
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
pub(crate) fn seed_coder_error_actions(ctx: &mut dyn NativeContext, coder: ObjectRef) {
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
            let result = alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1);
            ctx.set_field(result, 0, Value::Int(0)); // 0 = UNDERFLOW (success)
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        enc,
        "flush",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1);
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
            let result = alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1);
            ctx.set_field(result, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.register(
        dec,
        "flush",
        "(Ljava/nio/CharBuffer;)Ljava/nio/charset/CoderResult;",
        |ctx, _args| {
            let result = alloc_concurrent_synthetic(ctx, "java/nio/charset/CoderResult", 1);
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
            let enc_obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetEncoder", 6);
            ctx.set_field(enc_obj, 0, Value::Object(Some(this)));
            ctx.set_field(enc_obj, 1, Value::Float(avg));
            ctx.set_field(enc_obj, 2, Value::Float(max));
            seed_coder_error_actions(ctx, enc_obj);
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
            let dec_obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetDecoder", 6);
            ctx.set_field(dec_obj, 0, Value::Object(Some(this)));
            ctx.set_field(dec_obj, 1, Value::Float(avg));
            ctx.set_field(dec_obj, 2, Value::Float(max));
            let replacement = ctx.create_string("\u{fffd}");
            ctx.set_field_by_name(dec_obj, "replacement", Value::Object(Some(replacement)));
            seed_coder_error_actions(ctx, dec_obj);
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
        let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3);
        ctx.set_field(set, 0, Value::Object(None));
        ctx.set_field(set, 1, Value::Int(0));
        ctx.set_field(set, 2, Value::Int(16));
        Ok(Some(Value::Object(Some(set))))
    });
    r.register(cs, "displayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cs, "isRegistered", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(cs, "canEncode", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(
        cs,
        "contains",
        "(Ljava/nio/charset/Charset;)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
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
            let tm = alloc_concurrent_synthetic(ctx, "java/util/TreeMap", 3);
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
                let cs_obj = alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 2);
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
    // Synthetic-mode indexed fallback (older callers).
    ctx.set_field(buf, CB_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(buf, CB_FIELD_POS, Value::Int(0));
    ctx.set_field(buf, CB_FIELD_LIMIT, Value::Int(len_chars));
    ctx.set_field(buf, CB_FIELD_CAPACITY, Value::Int(len_chars));
    ctx.set_field(buf, CB_FIELD_MARK, Value::Int(-1));
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
        let buf = p62_alloc_char_buffer(ctx, cap.max(0) as usize);
        Ok(Some(Value::Object(Some(buf))))
    });
    r.register(cb, "wrap", "([C)Ljava/nio/CharBuffer;", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let len = ctx.array_length(arr);
        // Pin across the buffer alloc below — a moving young GC there would
        // relocate the backing array (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        let buf = alloc_concurrent_synthetic(ctx, "java/nio/CharBuffer", 5);
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
            // 144 Tomcat MessageBytes-conversion failures.
            let s = match args.first() {
                Some(Value::Object(Some(s))) => {
                    let direct = ctx.read_string(*s).unwrap_or_default();
                    if !direct.is_empty() {
                        direct
                    } else {
                        match ctx.invoke_virtual(*s, "toString", "()Ljava/lang/String;", &[]) {
                            Ok(Some(Value::Object(Some(strref)))) => {
                                ctx.read_string(strref).unwrap_or_default()
                            }
                            _ => direct,
                        }
                    }
                }
                _ => String::new(),
            };
            let chars: Vec<u16> = s.encode_utf16().collect();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, chars.len());
            for (i, &ch) in chars.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(ch as i32));
            }
            // Pin across the buffer alloc below — a moving young GC there
            // would relocate the backing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let buf = alloc_concurrent_synthetic(ctx, "java/nio/CharBuffer", 5);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            cb_write_hb(ctx, buf, arr, chars.len() as i32);
            Ok(Some(Value::Object(Some(buf))))
        },
    );
    // hasArray()Z — JDK bytecode reads `hb != null && !isReadOnly`.
    // Mirror that with a robust slot/name lookup so synthetic-mode
    // CharBuffers (where slot 0 may have been overwritten by
    // descriptor-coerced int writes against Buffer.mark) still
    // report true. Without this, icu4j's `b16BitUnits.subSequence(
    // start, end).toString()` reads a null `hb` and throws
    // `IllegalStateException("CharBuffer has no backing array")`.
    r.register(cb, "hasArray", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if cb_read_hb(ctx, this).is_some() {
            1
        } else {
            0
        })))
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
    r.register(cb, "isReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            match ctx.get_field_by_name(this, "isReadOnly") {
                Value::Int(v) => v,
                _ => 0,
            },
        )))
    });
    r.register(cb, "isDirect", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
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
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)))))
    });
    fn cb_order_big(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        use crate::servlet::s2_byte_order_object;
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 0)))))
    }
    fn cb_order_little(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        use crate::servlet::s2_byte_order_object;
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, 1)))))
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
    r.register(cb, "arrayOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field_by_name(this, "offset") {
            Value::Int(v) => Ok(Some(Value::Int(v))),
            _ => Ok(Some(Value::Int(0))),
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
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let units: Vec<u16> = text.encode_utf16().collect();
            // StringCharBuffer.toString(int,int) receives absolute buffer
            // indexes (CharBuffer.toString() calls it with position..limit),
            // and StringCharBuffer then adds only `offset` before slicing
            // the wrapped CharSequence. Do not add current position again.
            let abs_start = cur_off + start;
            let abs_end = cur_off + end;
            let len = units.len() as i32;
            let s_lo = abs_start.max(0).min(len);
            let s_hi = abs_end.max(s_lo).min(len);
            let out = String::from_utf16_lossy(&units[s_lo as usize..s_hi as usize]);
            return Ok(Some(Value::Object(Some(ctx.create_string(&out)))));
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
        let s: String = String::from_utf16_lossy(&chars);
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
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
        let args2 = [
            Value::Object(Some(this)),
            Value::Int(0),
            Value::Int(lim - pos),
        ];
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
            let new_pos = cur_pos + start;
            let new_lim = cur_pos + end;
            // Allocate a fresh HeapCharBuffer pointing at the same char[]
            // — JDK's HeapCharBuffer.subSequence does the same.
            // Pin across the buffer alloc below — a moving young GC there
            // would relocate the backing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let buf = alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
            ctx.set_field_by_name(buf, "offset", Value::Int(cur_off));
            ctx.set_field_by_name(buf, "isReadOnly", Value::Int(0));
            ctx.set_field_by_name(buf, "position", Value::Int(new_pos));
            ctx.set_field_by_name(buf, "limit", Value::Int(new_lim));
            ctx.set_field_by_name(buf, "capacity", Value::Int(new_lim));
            ctx.set_field_by_name(buf, "mark", Value::Int(-1));
            ctx.set_field(buf, CB_FIELD_ARRAY, Value::Object(Some(arr)));
            ctx.set_field(buf, CB_FIELD_POS, Value::Int(new_pos));
            ctx.set_field(buf, CB_FIELD_LIMIT, Value::Int(new_lim));
            ctx.set_field(buf, CB_FIELD_CAPACITY, Value::Int(new_lim));
            ctx.set_field(buf, CB_FIELD_MARK, Value::Int(-1));
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
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("index: {idx}"),
            }
            .into());
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
        ctx.set_field(this, CB_FIELD_MARK, Value::Int(-1));
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
        ctx.set_field(this, CB_FIELD_MARK, Value::Int(-1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "rewind", "()Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, CB_FIELD_POS, Value::Int(0));
        ctx.set_field(this, CB_FIELD_MARK, Value::Int(-1));
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
        ctx.set_field(this, CB_FIELD_POS, Value::Int(new_pos));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "limit", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, CB_FIELD_LIMIT)))
    });
    r.register(cb, "limit", "(I)Ljava/nio/CharBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_lim = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, CB_FIELD_LIMIT, Value::Int(new_lim));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(cb, "capacity", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, CB_FIELD_CAPACITY)))
    });
    r.register(cb, "array", "()[C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match cb_read_hb(ctx, this) {
            Some(a) => Ok(Some(Value::Object(Some(a)))),
            None => Err(RuntimeError::IllegalStateException {
                message: "CharBuffer.array: no backing array".into(),
            }
            .into()),
        }
    });
    r.register(cb, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field_by_name(this, "position") {
            Value::Int(v) => v as usize,
            _ => match ctx.get_field(this, CB_FIELD_POS) {
                Value::Int(v) => v as usize,
                _ => 0,
            },
        };
        let lim = match ctx.get_field_by_name(this, "limit") {
            Value::Int(v) => v as usize,
            _ => match ctx.get_field(this, CB_FIELD_LIMIT) {
                Value::Int(v) => v as usize,
                _ => 0,
            },
        };
        let off = match ctx.get_field_by_name(this, "offset") {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let arr = match cb_read_hb(ctx, this) {
            Some(a) => a,
            None => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        let mut chars = Vec::new();
        for i in pos..lim {
            if let Value::Int(ch) = ctx.get_array_element(arr, i + off) {
                chars.push(ch as u16);
            }
        }
        let text = String::from_utf16_lossy(&chars);
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    });

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
}

pub(crate) fn p62_alloc_char_buffer(ctx: &mut dyn NativeContext, cap: usize) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, cap);
    // Use HeapCharBuffer (concrete) not CharBuffer (abstract) so real-JDK
    // bytecode methods like compact() dispatch correctly.
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate the backing array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    let buf = alloc_concurrent_synthetic(ctx, "java/nio/HeapCharBuffer", 5);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    cb_write_hb(ctx, buf, arr, cap as i32);
    buf
}
