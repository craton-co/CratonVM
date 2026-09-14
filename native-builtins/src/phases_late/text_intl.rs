// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.text` / `java.time` / i18n natives: formats, Collator, BreakIterator, ResourceBundle, DateTimeFormatterBuilder.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// ---------------------------------------------------------------------------
// java.text — DecimalFormat, SimpleDateFormat, MessageFormat, NumberFormat
// DecimalFormat = 4-field synthetic (pattern=0, groupingUsed=1, maxFracDigits=2, minFracDigits=3)
// SimpleDateFormat = 1-field synthetic (pattern=0)
// MessageFormat = 1-field synthetic (pattern=0)
// ---------------------------------------------------------------------------
pub(crate) const DF_FIELD_PATTERN: usize = 0;

pub(crate) const DF_FIELD_GROUPING: usize = 1;

pub(crate) const DF_FIELD_MAX_FRAC: usize = 2;

pub(crate) const DF_FIELD_MIN_FRAC: usize = 3;

/// Insert grouping separators (commas) into the integer part of a formatted number.
pub(crate) fn df_add_grouping(s: &str) -> String {
    let (int_part, frac_part) = match s.find('.') {
        Some(dot) => (&s[..dot], Some(&s[dot..])),
        None => (s, None),
    };
    let negative = int_part.starts_with('-');
    let digits = if negative { &int_part[1..] } else { int_part };
    if digits.len() <= 3 {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len() + digits.len() / 3);
    if negative {
        result.push('-');
    }
    let remainder = digits.len() % 3;
    if remainder > 0 {
        result.push_str(&digits[..remainder]);
        if digits.len() > remainder {
            result.push(',');
        }
    }
    let groups: Vec<&str> = digits[remainder..]
        .as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    result.push_str(&groups.join(","));
    if let Some(frac) = frac_part {
        result.push_str(frac);
    }
    result
}

/// Parse a DecimalFormat pattern string to extract formatting parameters.
/// Supports patterns like "#,##0.###", "0.00", "#,##0%", "$#,##0.00".
/// Returns (grouping_used, max_fraction_digits, min_fraction_digits).
pub(crate) fn df_parse_pattern(pattern: &str) -> (bool, usize, usize) {
    // Strip positive/negative subpattern (split on ';')
    let pos_pattern = pattern.split(';').next().unwrap_or(pattern);

    // Strip prefix (non-digit characters before first # or 0)
    let digit_start = pos_pattern
        .find(|c: char| c == '#' || c == '0' || c == ',')
        .unwrap_or(0);
    let core = &pos_pattern[digit_start..];

    // Strip suffix (non-digit characters after last # or 0)
    let digit_end = core
        .rfind(|c: char| c == '#' || c == '0')
        .map(|i| i + 1)
        .unwrap_or(core.len());
    let core = &core[..digit_end];

    // Check for grouping separator
    let has_grouping = core.contains(',');

    // Count fraction digits (after '.')
    let (max_frac, min_frac) = if let Some(dot_pos) = core.find('.') {
        let frac_part = &core[dot_pos + 1..];
        let max = frac_part.chars().filter(|&c| c == '#' || c == '0').count();
        let min = frac_part.chars().filter(|&c| c == '0').count();
        (max, min)
    } else {
        (0, 0)
    };

    (has_grouping, max_frac, min_frac)
}

pub(crate) fn register_phase57_text(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let df = "java/text/DecimalFormat";
    let nf = "java/text/NumberFormat";
    let mf = "java/text/MessageFormat";

    // --- DecimalFormat ---
    r.register(df, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Pin across the create_string below — a moving young GC there would
        // relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let pat = ctx.create_string("#,##0.###");
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, DF_FIELD_PATTERN, Value::Object(Some(pat)));
        ctx.set_field(this, DF_FIELD_GROUPING, Value::Int(1));
        ctx.set_field(this, DF_FIELD_MAX_FRAC, Value::Int(3));
        ctx.set_field(this, DF_FIELD_MIN_FRAC, Value::Int(0));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });

    r.register(df, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, DF_FIELD_PATTERN, args[1]);
        // Parse pattern to extract grouping and fraction digit settings
        let pattern = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let (grouping, max_frac, min_frac) = df_parse_pattern(&pattern);
        ctx.set_field(
            this,
            DF_FIELD_GROUPING,
            Value::Int(if grouping { 1 } else { 0 }),
        );
        ctx.set_field(this, DF_FIELD_MAX_FRAC, Value::Int(max_frac as i32));
        ctx.set_field(this, DF_FIELD_MIN_FRAC, Value::Int(min_frac as i32));
        Ok(None)
    });

    r.register(df, "format", "(D)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args[1] {
            Value::Double(d) => d,
            Value::Float(f) => f as f64,
            Value::Int(i) => i as f64,
            Value::Long(l) => l as f64,
            _ => 0.0,
        };
        let max_frac = match ctx.get_field(this, DF_FIELD_MAX_FRAC) {
            Value::Int(i) => i as usize,
            _ => 3,
        };
        let min_frac = match ctx.get_field(this, DF_FIELD_MIN_FRAC) {
            Value::Int(i) => i as usize,
            _ => 0,
        };
        let grouping = match ctx.get_field(this, DF_FIELD_GROUPING) {
            Value::Int(i) => i != 0,
            _ => false,
        };
        let formatted = format!("{:.*}", max_frac, val);
        // Trim trailing zeros only down to min_frac decimal places
        let trimmed = if formatted.contains('.') && min_frac < max_frac {
            let dot_pos = formatted.find('.').unwrap();
            let frac = &formatted[dot_pos + 1..];
            // Trim from the end, but keep at least min_frac digits
            let mut keep = frac.len();
            while keep > min_frac && frac.as_bytes()[keep - 1] == b'0' {
                keep -= 1;
            }
            if keep == 0 {
                formatted[..dot_pos].to_string()
            } else {
                format!("{}.{}", &formatted[..dot_pos], &frac[..keep])
            }
        } else {
            formatted
        };
        // Apply grouping separators to integer part
        let result = if grouping {
            df_add_grouping(&trimmed)
        } else {
            trimmed
        };
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    });

    r.register(df, "format", "(J)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args[1] {
            Value::Long(l) => l,
            Value::Int(i) => i as i64,
            _ => 0,
        };
        let grouping = match ctx.get_field(this, DF_FIELD_GROUPING) {
            Value::Int(i) => i != 0,
            _ => false,
        };
        let formatted = val.to_string();
        let result = if grouping {
            df_add_grouping(&formatted)
        } else {
            formatted
        };
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    });

    r.register(df, "getPattern", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DF_FIELD_PATTERN)))
    });

    r.register(df, "setMaximumFractionDigits", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, DF_FIELD_MAX_FRAC, args[1]);
        Ok(None)
    });

    r.register(df, "setGroupingUsed", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, DF_FIELD_GROUPING, args[1]);
        Ok(None)
    });

    r.register(df, "isGroupingUsed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DF_FIELD_GROUPING)))
    });

    // DecimalFormat.parse(String) → Number
    r.register(
        df,
        "parse",
        "(Ljava/lang/String;)Ljava/lang/Number;",
        |ctx, args| {
            let text_ref = obj_arg(args, 1)?;
            let text = ctx.read_string(text_ref).unwrap_or_default();
            // Strip grouping separators and currency symbols, then parse
            let cleaned: String = text
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
                .collect();
            if cleaned.contains('.') {
                let val: f64 = cleaned.parse().unwrap_or(0.0);
                let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
                ctx.set_field(obj, 0, Value::Double(val));
                Ok(Some(Value::Object(Some(obj))))
            } else {
                let val: i64 = cleaned.parse().unwrap_or(0);
                let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Long", 1)?;
                ctx.set_field(obj, 0, Value::Long(val));
                Ok(Some(Value::Object(Some(obj))))
            }
        },
    );

    // NumberFormat.parse(String) → Number (same as DecimalFormat.parse)
    r.register(
        nf,
        "parse",
        "(Ljava/lang/String;)Ljava/lang/Number;",
        |ctx, args| {
            let text_ref = obj_arg(args, 1)?;
            let text = ctx.read_string(text_ref).unwrap_or_default();
            let cleaned: String = text
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
                .collect();
            if cleaned.contains('.') {
                let val: f64 = cleaned.parse().unwrap_or(0.0);
                let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
                ctx.set_field(obj, 0, Value::Double(val));
                Ok(Some(Value::Object(Some(obj))))
            } else {
                let val: i64 = cleaned.parse().unwrap_or(0);
                let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Long", 1)?;
                ctx.set_field(obj, 0, Value::Long(val));
                Ok(Some(Value::Object(Some(obj))))
            }
        },
    );

    // --- NumberFormat static factories ---
    r.register(
        nf,
        "getInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DecimalFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("#,##0.###");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, DF_FIELD_PATTERN, Value::Object(Some(pat)));
            ctx.set_field(obj, DF_FIELD_GROUPING, Value::Int(1));
            ctx.set_field(obj, DF_FIELD_MAX_FRAC, Value::Int(3));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        nf,
        "getIntegerInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DecimalFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("#,##0");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, DF_FIELD_PATTERN, Value::Object(Some(pat)));
            ctx.set_field(obj, DF_FIELD_GROUPING, Value::Int(1));
            ctx.set_field(obj, DF_FIELD_MAX_FRAC, Value::Int(0));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        nf,
        "getCurrencyInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DecimalFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("$#,##0.00");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, DF_FIELD_PATTERN, Value::Object(Some(pat)));
            ctx.set_field(obj, DF_FIELD_GROUPING, Value::Int(1));
            ctx.set_field(obj, DF_FIELD_MAX_FRAC, Value::Int(2));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        nf,
        "getPercentInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DecimalFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("#,##0%");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(obj, DF_FIELD_PATTERN, Value::Object(Some(pat)));
            ctx.set_field(obj, DF_FIELD_GROUPING, Value::Int(1));
            ctx.set_field(obj, DF_FIELD_MAX_FRAC, Value::Int(0));
            ctx.unpin_native_roots(obj_pin);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // --- SimpleDateFormat / DateFormat ---
    // No synthetic natives: real `java.text.SimpleDateFormat` bytecode runs.
    // The previous synthetic stub stored the pattern in slot 0 — which is
    // `DateFormat.calendar` in the real class layout — and left
    // `DateFormat.numberFormat` null, so the JDK's `final DateFormat.format`
    // wrappers NPE'd on `numberFormat.getClass()`. The real implementation's
    // dependencies (`Calendar`, `DateFormatSymbols`, `NumberFormat`) all work
    // on CratonVM, so the real classes are used directly.

    // --- MessageFormat ---
    r.register(mf, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(None)
    });

    r.register(mf, "toPattern", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // `DecimalFormat.toPattern()` — the pattern the constructors above already
    // store in `DF_FIELD_PATTERN`. Only `MessageFormat` had one, so a
    // `DecimalFormat("#0.00")` could be built and applied but never asked what
    // pattern it was using.
    r.register(df, "toPattern", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DF_FIELD_PATTERN)))
    });

    // MessageFormat.format(String, Object...) — static
    r.register(
        mf,
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        p57_message_format,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p57_message_format(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pattern_ref = obj_arg(args, 0)?;
    let pattern = ctx.read_string(pattern_ref).unwrap_or_default();
    // Read the Object[] args
    let arr_values: Vec<String> = if let Value::Object(Some(arr)) = args[1] {
        let len = ctx.array_length(arr);
        (0..len)
            .map(|i| {
                let v = ctx.get_array_element(arr, i);
                match v {
                    Value::Int(i) => i.to_string(),
                    Value::Long(l) => l.to_string(),
                    Value::Float(f) => f.to_string(),
                    Value::Double(d) => d.to_string(),
                    Value::Object(Some(o)) => {
                        if let Some(s) = ctx.read_string(o) {
                            s
                        } else {
                            // Try to unbox wrapper type (Integer, Long, etc.)
                            let nf = ctx.object_num_fields(o);
                            if nf >= 1 {
                                match ctx.get_field(o, 0) {
                                    Value::Int(iv) => iv.to_string(),
                                    Value::Long(lv) => lv.to_string(),
                                    Value::Float(fv) => fv.to_string(),
                                    Value::Double(dv) => dv.to_string(),
                                    _ => "null".to_string(),
                                }
                            } else {
                                "null".to_string()
                            }
                        }
                    }
                    _ => "null".to_string(),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    // Replace {0}, {1}, etc. with arguments
    let mut result = pattern;
    for (i, val) in arr_values.iter().enumerate() {
        result = result.replace(&format!("{{{}}}", i), val);
    }
    let s = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(s))))
}

// =============================================================================
// java.text — ChoiceFormat, DecimalFormatSymbols, DateFormatSymbols,
//             ParsePosition, FieldPosition, Normalizer
// =============================================================================

pub(crate) fn register_p61_text_formatting(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- ParsePosition = 1-field (index=0) ---
    let pp = "java/text/ParsePosition";
    r.register(pp, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Int(idx));
        Ok(None)
    });
    r.register(pp, "getIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(pp, "setIndex", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Int(idx));
        Ok(None)
    });
    r.register(pp, "getErrorIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // errorIndex is field 1 if the object has ≥2 fields; otherwise default -1
        if ctx.object_num_fields(this) > 1 {
            Ok(Some(ctx.get_field(this, 1)))
        } else {
            Ok(Some(Value::Int(-1)))
        }
    });
    r.register(pp, "setErrorIndex", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(idx));
        }
        Ok(None)
    });

    // --- FieldPosition = 2-field (field=0, endIndex=1) ---
    let fp = "java/text/FieldPosition";
    r.register(fp, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let field = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Int(field));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(fp, "getField", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    // `java.text.FieldPosition` is a CONCRETE class, so this native also
    // intercepts REAL FieldPosition receivers — and the real JDK declares
    // `int field; int endIndex; int beginIndex; Format.Field attribute;`
    // in that order. That is exactly why the synthetic layout above is
    // (field=0, endIndex=1) and why `getEndIndex` below reads slot 1
    // unguarded: the first two slots already line up with the real class.
    // beginIndex is then slot 2 on a real receiver, and the flat `0` this
    // returned until 2026-07-28 discarded it — after a real
    // `format(obj, buf, pos)` wrote both indices,
    // `out.substring(fp.getBeginIndex(), fp.getEndIndex())` sliced from 0
    // instead of from the field start.
    //
    // Guarded exactly like `ParsePosition.getErrorIndex` above. On the 2-field
    // synthetic there is no beginIndex slot and 0 stays correct: it is the
    // JDK's "requested field not found" state, and it agrees with the slot-1
    // endIndex that `<init>` zeroes and no formatter in this tree updates.
    r.register(fp, "getBeginIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Slot 2 is only read when it really is an int — a receiver whose
        // third slot is a reference (a layout that does not match the one
        // described above) falls back to the "field not found" 0 rather than
        // returning an object from an `()I` native.
        if ctx.object_num_fields(this) > 2 {
            if let Value::Int(begin) = ctx.get_field(this, 2) {
                return Ok(Some(Value::Int(begin)));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(fp, "getEndIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // --- DecimalFormatSymbols = 6-field ---
    // (decimalSep=0 Int(char), groupSep=1, percent=2, currency=3 String, intlCurrency=4 String, minus=5 Int(char))
    let dfs = "java/text/DecimalFormatSymbols";
    r.register(dfs, "<init>", "()V", p61_dfs_init_default);
    r.register(dfs, "<init>", "(Ljava/util/Locale;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        p61_dfs_init(ctx, this);
        Ok(None)
    });
    r.register(dfs, "getDecimalSeparator", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(dfs, "getGroupingSeparator", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(dfs, "getPercent", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        dfs,
        "getCurrencySymbol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(
        dfs,
        "getInternationalCurrencySymbol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        },
    );
    r.register(dfs, "getMinusSign", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 5)))
    });
    r.register(dfs, "setDecimalSeparator", "(C)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(
            this,
            0,
            args.get(1).copied().unwrap_or(Value::Int('.' as i32)),
        );
        Ok(None)
    });
    r.register(dfs, "setGroupingSeparator", "(C)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(
            this,
            1,
            args.get(1).copied().unwrap_or(Value::Int(',' as i32)),
        );
        Ok(None)
    });

    // --- DateFormatSymbols = 5-field ---
    // (weekdays=0 String[], shortWeekdays=1, months=2, shortMonths=3, amPm=4)
    let dfsy = "java/text/DateFormatSymbols";
    r.register(dfsy, "<init>", "()V", p61_dfsy_init);
    r.register(dfsy, "<init>", "(Ljava/util/Locale;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        p61_dfsy_fill(ctx, this);
        Ok(None)
    });
    r.register(dfsy, "getWeekdays", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        dfsy,
        "getShortWeekdays",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(dfsy, "getMonths", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        dfsy,
        "getShortMonths",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(
        dfsy,
        "getAmPmStrings",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        },
    );

    // --- ChoiceFormat = 3-field: limits=0 (array of doubles), formats=1 (array of strings), count=2 ---
    // ChoiceFormat pattern syntax: "limit#string|limit#string|..."
    // or "limit<string|limit#string|..." where # means >= and < means >
    let cf = "java/text/ChoiceFormat";
    r.register(cf, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pattern_str = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        // Parse pattern: segments separated by '|', each is "limit#format" or "limit<format"
        let segments: Vec<&str> = pattern_str.split('|').collect();
        let count = segments.len();
        // Two arrays, then a box and a string per segment, and finally three
        // stores into the receiver: everything here outlives an allocation.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let limits_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, count);
        let limits_h = scope.root(limits_obj);
        let formats_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, count);
        let formats_h = scope.root(formats_obj);
        for (i, seg) in segments.iter().enumerate() {
            let (limit, format_str) = if let Some(pos) = seg.find('#') {
                let limit: f64 = seg[..pos].trim().parse().unwrap_or(0.0);
                (limit, &seg[pos + 1..])
            } else if let Some(pos) = seg.find('<') {
                let limit: f64 = seg[..pos].trim().parse().unwrap_or(0.0);
                // '<' means strictly greater than; we use nextUp semantics
                (limit + f64::EPSILON, &seg[pos + 1..])
            } else {
                (0.0, seg.trim())
            };
            // Store limit as a boxed Double synthetic
            let limit_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/lang/Double", 1)?;
            scope.set_field(limit_obj, 0, Value::Double(limit));
            let limits_arr = scope.get(&limits_h);
            scope.set_array_element(limits_arr, i, Value::Object(Some(limit_obj)));
            let fmt_s = scope.create_string(format_str);
            let formats_arr = scope.get(&formats_h);
            scope.set_array_element(formats_arr, i, Value::Object(Some(fmt_s)));
        }
        let this = scope.get(&this_h);
        let limits_arr = scope.get(&limits_h);
        let formats_arr = scope.get(&formats_h);
        scope.set_field(this, 0, Value::Object(Some(limits_arr)));
        scope.set_field(this, 1, Value::Object(Some(formats_arr)));
        scope.set_field(this, 2, Value::Int(count as i32));
        Ok(None)
    });
    r.register(cf, "<init>", "([D[Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let limits = args.get(1).copied().unwrap_or(Value::Object(None));
        let formats = args.get(2).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, limits);
        ctx.set_field(this, 1, formats);
        let count = match limits {
            Value::Object(Some(a)) => ctx.array_length(a) as i32,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(count));
        Ok(None)
    });
    r.register(cf, "format", "(D)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Double(v)) => *v,
            Some(Value::Long(v)) => *v as f64,
            Some(Value::Int(v)) => *v as f64,
            _ => 0.0,
        };
        let count = match ctx.get_field(this, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let limits_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => {
                let s = ctx.create_string(&format!("{d}"));
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        let formats_arr = match ctx.get_field(this, 1) {
            Value::Object(Some(a)) => a,
            _ => {
                let s = ctx.create_string(&format!("{d}"));
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        // Find the largest limit <= d
        let mut chosen = 0;
        for i in 0..count {
            let limit = match ctx.get_array_element(limits_arr, i) {
                Value::Object(Some(obj)) => match ctx.get_field(obj, 0) {
                    Value::Double(v) => v,
                    _ => 0.0,
                },
                Value::Double(v) => v,
                _ => 0.0,
            };
            if d >= limit {
                chosen = i;
            } else {
                break;
            }
        }
        if count > 0 {
            let fmt = ctx.get_array_element(formats_arr, chosen);
            Ok(Some(fmt))
        } else {
            let s = ctx.create_string(&format!("{d}"));
            Ok(Some(Value::Object(Some(s))))
        }
    });

    // Read an arbitrary `CharSequence` argument, not just `String`.
    // `ctx.read_string` only understands the compact-string `String` layout;
    // called on any other `CharSequence` (e.g. pgjdbc's SCRAM stringprep
    // wraps a `char[]` in `java.nio.CharBuffer` before calling
    // `Normalizer.normalize`) it silently returns `None`, and the old
    // `.unwrap_or_default()` at the two call sites below turned that into an
    // empty string instead of the real content — surfacing downstream as an
    // `ArrayIndexOutOfBoundsException` in `Character.codePointAt` on the
    // now-empty array. Mirrors the `String.format("%s", ...)` fast-path in
    // `lang_string.rs` (real `String` read directly; everything else via its
    // own `toString()`, which every `CharSequence` must provide).
    fn read_char_sequence(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> String {
        if ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(obj)) {
            if let Some(s) = ctx.read_string(obj) {
                return s;
            }
        }
        match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    }

    // --- Normalizer — real Unicode normalization via unicode-normalization crate ---
    let norm = "java/text/Normalizer";
    r.register(
        norm,
        "normalize",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Ljava/lang/String;",
        |ctx, args| {
            use unicode_normalization::UnicodeNormalization;
            // The same contract as the real-JDK twin in `locale_resources.rs`;
            // one helper so the pair cannot sit half-fixed.
            crate::locale_resources::normalizer_reject_nulls(args.first(), args.get(1))?;
            let input = match args.first() {
                Some(Value::Object(Some(s))) => read_char_sequence(ctx, *s),
                _ => return Ok(Some(Value::Object(None))),
            };
            // Read the Form's ordinal via `Enum.ordinal()` so this works for
            // BOTH the real-JDK `Normalizer.Form` enum (real Enum layout: name
            // at field 0, ordinal at field 1) and the synthetic one — reading
            // field 0 as an int would pick up the `name` String reference and
            // always yield 0 (NFC). NFC=0, NFD=1, NFKC=2, NFKD=3.
            let form_ordinal = match args.get(1) {
                Some(Value::Object(Some(f))) => ctx
                    .invoke_virtual(*f, "ordinal", "()I", &[])
                    .ok()
                    .flatten()
                    .and_then(|v| v.as_int())
                    .unwrap_or(0),
                _ => 0,
            };
            let normalized = match form_ordinal {
                0 => input.nfc().collect::<String>(),  // NFC
                1 => input.nfd().collect::<String>(),  // NFD
                2 => input.nfkc().collect::<String>(), // NFKC
                3 => input.nfkd().collect::<String>(), // NFKD
                _ => input,
            };
            let s = ctx.create_string(&normalized);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        norm,
        "isNormalized",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z",
        |ctx, args| {
            use unicode_normalization::{
                is_nfc_quick, is_nfd_quick, is_nfkc_quick, is_nfkd_quick, IsNormalized,
            };
            crate::locale_resources::normalizer_reject_nulls(args.first(), args.get(1))?;
            let input = match args.first() {
                Some(Value::Object(Some(s))) => read_char_sequence(ctx, *s),
                _ => return Ok(Some(Value::Int(1))),
            };
            let form_ordinal = match args.get(1) {
                Some(Value::Object(Some(f))) => ctx.get_field(*f, 0).as_int().unwrap_or(0),
                _ => 0,
            };
            let result = match form_ordinal {
                0 => is_nfc_quick(input.chars()) == IsNormalized::Yes,
                1 => is_nfd_quick(input.chars()) == IsNormalized::Yes,
                2 => is_nfkc_quick(input.chars()) == IsNormalized::Yes,
                3 => is_nfkd_quick(input.chars()) == IsNormalized::Yes,
                _ => true,
            };
            Ok(Some(Value::Int(if result { 1 } else { 0 })))
        },
    );
    // Normalizer.Form enum values
    let nf = "java/text/Normalizer$Form";
    r.register(nf, "NFC", "Ljava/text/Normalizer$Form;", |ctx, _args| {
        p57_alloc_enum(ctx, "java/text/Normalizer$Form", "NFC", 0)
    });
    r.register(nf, "NFD", "Ljava/text/Normalizer$Form;", |ctx, _args| {
        p57_alloc_enum(ctx, "java/text/Normalizer$Form", "NFD", 1)
    });
    r.register(nf, "NFKC", "Ljava/text/Normalizer$Form;", |ctx, _args| {
        p57_alloc_enum(ctx, "java/text/Normalizer$Form", "NFKC", 2)
    });
    r.register(nf, "NFKD", "Ljava/text/Normalizer$Form;", |ctx, _args| {
        p57_alloc_enum(ctx, "java/text/Normalizer$Form", "NFKD", 3)
    });
    r.set_category(__prev_cat);
}

pub(crate) fn p61_dfs_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    p61_dfs_init(ctx, this);
    Ok(None)
}

pub(crate) fn p61_dfs_init(ctx: &mut dyn NativeContext, this: ObjectRef) {
    ctx.set_field(this, 0, Value::Int('.' as i32)); // decimal separator
    ctx.set_field(this, 1, Value::Int(',' as i32)); // grouping separator
    ctx.set_field(this, 2, Value::Int('%' as i32)); // percent
    let cur = ctx.create_string("$");
    ctx.set_field(this, 3, Value::Object(Some(cur))); // currency symbol
    let intl = ctx.create_string("USD");
    ctx.set_field(this, 4, Value::Object(Some(intl))); // international currency
    ctx.set_field(this, 5, Value::Int('-' as i32)); // minus sign
}

pub(crate) fn p61_dfsy_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    p61_dfsy_fill(ctx, this);
    Ok(None)
}

pub(crate) fn p61_dfsy_fill(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // Weekdays: index 0 = empty, 1=Sunday..7=Saturday (Java convention)
    let weekdays = [
        "",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let wd_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, weekdays.len());
    for (i, &w) in weekdays.iter().enumerate() {
        let s = ctx.create_string(w);
        ctx.set_array_element(wd_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, 0, Value::Object(Some(wd_arr)));

    let short_weekdays = ["", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let swd_arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        short_weekdays.len(),
    );
    for (i, &w) in short_weekdays.iter().enumerate() {
        let s = ctx.create_string(w);
        ctx.set_array_element(swd_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, 1, Value::Object(Some(swd_arr)));

    let months = [
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
        "",
    ];
    let m_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, months.len());
    for (i, &m) in months.iter().enumerate() {
        let s = ctx.create_string(m);
        ctx.set_array_element(m_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, 2, Value::Object(Some(m_arr)));

    let short_months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "",
    ];
    let sm_arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        short_months.len(),
    );
    for (i, &m) in short_months.iter().enumerate() {
        let s = ctx.create_string(m);
        ctx.set_array_element(sm_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, 3, Value::Object(Some(sm_arr)));

    let ampm = ["AM", "PM"];
    let ap_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, ampm.len());
    for (i, &a) in ampm.iter().enumerate() {
        let s = ctx.create_string(a);
        ctx.set_array_element(ap_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, 4, Value::Object(Some(ap_arr)));
}

// =============================================================================
// java.time expansion — MonthDay, YearMonth, Year
// =============================================================================

pub(crate) fn register_p62_time_expansion(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- Year = 1-field (year=0 Int) ---
    let yr = "java/time/Year";
    r.register(yr, "of", "(I)Ljava/time/Year;", |ctx, args| {
        let y = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 2000,
        };
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Year", 1)?;
        ctx.set_field(obj, 0, Value::Int(y));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(yr, "now", "()Ljava/time/Year;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Year", 1)?;
        ctx.set_field(obj, 0, Value::Int(2026));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(yr, "getValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(yr, "isLeap", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        Ok(Some(Value::Int(if leap { 1 } else { 0 })))
    });
    r.register(yr, "isLeap", "(J)Z", |_ctx, args| {
        let y = match args.first() {
            Some(Value::Long(v)) => *v as i32,
            _ => 0,
        };
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        Ok(Some(Value::Int(if leap { 1 } else { 0 })))
    });
    r.register(yr, "length", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        Ok(Some(Value::Int(if leap { 366 } else { 365 })))
    });
    r.register(yr, "plusYears", "(J)Ljava/time/Year;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let add = match args.get(1) {
            Some(Value::Long(v)) => *v as i32,
            _ => 0,
        };
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Year", 1)?;
        ctx.set_field(obj, 0, Value::Int(y + add));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(yr, "minusYears", "(J)Ljava/time/Year;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let sub = match args.get(1) {
            Some(Value::Long(v)) => *v as i32,
            _ => 0,
        };
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Year", 1)?;
        ctx.set_field(obj, 0, Value::Int(y - sub));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(yr, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let s = ctx.create_string(&format!("{y}"));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(yr, "compareTo", "(Ljava/time/Year;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y1 = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let y2 = if let Some(Value::Object(Some(o))) = args.get(1) {
            match ctx.get_field(*o, 0) {
                Value::Int(v) => v,
                _ => 0,
            }
        } else {
            0
        };
        Ok(Some(Value::Int(y1.cmp(&y2) as i32)))
    });

    // --- YearMonth = 2-field (year=0, month=1 Int 1-12) ---
    let ym = "java/time/YearMonth";
    r.register(ym, "of", "(II)Ljava/time/YearMonth;", |ctx, args| {
        let y = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 2000,
        };
        let m = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/YearMonth", 2)?;
        ctx.set_field(obj, 0, Value::Int(y));
        ctx.set_field(obj, 1, Value::Int(m));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(ym, "now", "()Ljava/time/YearMonth;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/YearMonth", 2)?;
        ctx.set_field(obj, 0, Value::Int(2026));
        ctx.set_field(obj, 1, Value::Int(2));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(ym, "getYear", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ym, "getMonthValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(ym, "lengthOfMonth", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 2000,
        };
        let m = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 1,
        };
        let days = p62_days_in_month(y, m);
        Ok(Some(Value::Int(days)))
    });
    r.register(ym, "plusMonths", "(J)Ljava/time/YearMonth;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let m = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 1,
        };
        let add = match args.get(1) {
            Some(Value::Long(v)) => *v as i32,
            _ => 0,
        };
        let total = (y * 12 + (m - 1)) + add;
        let new_y = total.div_euclid(12);
        let new_m = total.rem_euclid(12) + 1;
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/YearMonth", 2)?;
        ctx.set_field(obj, 0, Value::Int(new_y));
        ctx.set_field(obj, 1, Value::Int(new_m));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(ym, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let y = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let m = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 1,
        };
        let s = ctx.create_string(&format!("{y:04}-{m:02}"));
        Ok(Some(Value::Object(Some(s))))
    });

    // --- MonthDay = 2-field (month=0, day=1) ---
    let md = "java/time/MonthDay";
    r.register(md, "of", "(II)Ljava/time/MonthDay;", |ctx, args| {
        let m = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let d = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/MonthDay", 2)?;
        ctx.set_field(obj, 0, Value::Int(m));
        ctx.set_field(obj, 1, Value::Int(d));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(md, "getMonthValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(md, "getDayOfMonth", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(md, "isValidYear", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 1,
        };
        let d = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 1,
        };
        let y = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 2000,
        };
        let max = p62_days_in_month(y, m);
        Ok(Some(Value::Int(if d <= max { 1 } else { 0 })))
    });
    r.register(md, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 1,
        };
        let d = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 1,
        };
        let s = ctx.create_string(&format!("--{m:02}-{d:02}"));
        Ok(Some(Value::Object(Some(s))))
    });
    r.set_category(__prev_cat);
}

pub(crate) fn p62_days_in_month(y: i32, m: i32) -> i32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

// =============================================================================
// java.text — NumberFormat/DateFormat static factories
// =============================================================================

pub(crate) fn register_p62_format_factories(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let nf = "java/text/NumberFormat";
    // NumberFormat = 3-field synthetic (like DecimalFormat: pattern=0, groupingUsed=1, maxFrac=2)
    r.register(
        nf,
        "getInstance",
        "()Ljava/text/NumberFormat;",
        p62_new_number_format,
    );
    r.register(
        nf,
        "getNumberInstance",
        "()Ljava/text/NumberFormat;",
        p62_new_number_format,
    );
    r.register(
        nf,
        "getIntegerInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/NumberFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("#,##0");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            ctx.set_field(obj, 1, Value::Int(1)); // grouping used
            ctx.set_field(obj, 2, Value::Int(0)); // 0 fraction digits
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        nf,
        "getCurrencyInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/NumberFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("$#,##0.00");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            ctx.set_field(obj, 1, Value::Int(1));
            ctx.set_field(obj, 2, Value::Int(2));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        nf,
        "getPercentInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/NumberFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("#,##0%");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            ctx.set_field(obj, 1, Value::Int(1));
            ctx.set_field(obj, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // Locale-specific variants (ignore locale, return default)
    r.register(
        nf,
        "getInstance",
        "(Ljava/util/Locale;)Ljava/text/NumberFormat;",
        p62_new_number_format,
    );
    r.register(
        nf,
        "getNumberInstance",
        "(Ljava/util/Locale;)Ljava/text/NumberFormat;",
        p62_new_number_format,
    );

    let df = "java/text/DateFormat";
    r.register(
        df,
        "getDateInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("MMM d, yyyy");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        df,
        "getTimeInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("h:mm:ss a");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        df,
        "getDateTimeInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("MMM d, yyyy h:mm:ss a");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        df,
        "getDateInstance",
        "(I)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("MMM d, yyyy");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        df,
        "getTimeInstance",
        "(I)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("h:mm:ss a");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        df,
        "getDateTimeInstance",
        "(II)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/DateFormat", 3)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh format (native stale-local family).
            let obj_pin = ctx.pin_native_root(obj);
            let pat = ctx.create_string("MMM d, yyyy h:mm:ss a");
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            ctx.set_field(obj, 0, Value::Object(Some(pat)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // Format base class
    let fmt = "java/text/Format";
    r.register(
        fmt,
        "format",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            // Default: call toString on the object
            if let Some(Value::Object(Some(obj))) = args.get(1) {
                let s = ctx.create_string(&format!("{:?}", obj.as_ptr()));
                Ok(Some(Value::Object(Some(s))))
            } else {
                let s = ctx.create_string("null");
                Ok(Some(Value::Object(Some(s))))
            }
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p62_new_number_format(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/text/NumberFormat", 3)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh format (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let pat = ctx.create_string("#,##0.###");
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    ctx.set_field(obj, 0, Value::Object(Some(pat)));
    ctx.set_field(obj, 1, Value::Int(1));
    ctx.set_field(obj, 2, Value::Int(3));
    Ok(Some(Value::Object(Some(obj))))
}

// =============================================================================
// java.util.ResourceBundle — simple key-value store
// ResourceBundle = 2-field (entries=0 HashMap, parent=1)
// =============================================================================

pub(crate) fn register_p63_resource_bundle(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let rb = "java/util/ResourceBundle";
    r.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
        resource_bundle_get_bundle,
    );
    r.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
        resource_bundle_get_bundle,
    );
    r.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;)Ljava/util/ResourceBundle;",
        resource_bundle_get_bundle,
    );
    r.register(
        rb,
        "getString",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = match args.get(1) {
                Some(Value::Object(Some(k))) => *k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let map = match ctx.get_field(this, 0) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), Value::Object(Some(key))],
            )
        },
    );
    r.register(
        rb,
        "getObject",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = match args.get(1) {
                Some(Value::Object(Some(k))) => *k,
                _ => return Ok(Some(Value::Object(None))),
            };
            let map = match ctx.get_field(this, 0) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), Value::Object(Some(key))],
            )
        },
    );
    r.register(rb, "containsKey", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = match args.get(1) {
            Some(Value::Object(Some(k))) => *k,
            _ => return Ok(Some(Value::Int(0))),
        };
        let map = match ctx.get_field(this, 0) {
            Value::Object(Some(m)) => m,
            _ => return Ok(Some(Value::Int(0))),
        };
        cratonvm_native_collections::native_map_contains_key_pub(
            ctx,
            &[Value::Object(Some(map)), Value::Object(Some(key))],
        )
    });
    // W2: this returned null, and `ResourceBundle.getLocale()` never returns
    // null in the real JDK — so the idiomatic `bundle.getLocale().getLanguage()`
    // (and every `new MessageFormat(pattern, bundle.getLocale())`) NPE'd on a
    // bundle that had loaded perfectly well.
    //
    // Answer `Locale.ROOT`, which is not a guess but this implementation's
    // actual state: `resource_bundle_get_bundle` above ignores the Locale
    // argument entirely and loads one flat properties bundle with no
    // candidate-locale negotiation, so what the caller is holding IS the base
    // bundle, and the JDK gives a base bundle `Locale.ROOT`.
    r.register(rb, "getLocale", "()Ljava/util/Locale;", |ctx, _args| {
        // Same "empty language, empty country" ROOT locale
        // `locale_resources.rs` builds for its own base-bundle path.
        let root = crate::locale_alloc(ctx, "", "")?;
        Ok(Some(Value::Object(Some(root))))
    });
    r.register(rb, "getKeys", "()Ljava/util/Enumeration;", |ctx, _args| {
        let e = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)?;
        Ok(Some(Value::Object(Some(e))))
    });
    r.register(rb, "keySet", "()Ljava/util/Set;", |ctx, _args| {
        // Through the real `HashSet.<init>`: the three slots this used to
        // write are the MAP layout on a class whose one real field is `map`.
        // See `phases_late::collections`'s `Collections.singleton`.
        let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 1)?;
        let set_pin = ctx.pin_native_root(set);
        let _ = ctx.invoke(
            "java/util/HashSet",
            "<init>",
            "()V",
            &[Value::Object(Some(set))],
        );
        let set = ctx.read_native_pin(set_pin, set);
        ctx.unpin_native_roots(set_pin);
        Ok(Some(Value::Object(Some(set))))
    });
    r.set_category(__prev_cat);
}

/// Build a Java String[] from a Rust slice of &str.
pub(crate) fn make_string_array(ctx: &mut dyn NativeContext, items: &[&str]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, items.len());
    // Pin across the create_strings below — a moving young GC there would
    // relocate the fresh array (native stale-local family).
    let arr_pin = ctx.pin_native_root(arr);
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    arr
}

/// Put a String[] under a key into a HashMap. `map_pin` is the caller's pin
/// handle for `map` — the string/array allocations below can trigger a moving
/// young GC, so the current map ref is re-read from the pin before the put
/// (native stale-local family).
pub(crate) fn put_arr(
    ctx: &mut dyn NativeContext,
    map_pin: usize,
    map: ObjectRef,
    key: &str,
    items: &[&str],
) {
    let k = ctx.create_string(key);
    let k_pin = ctx.pin_native_root(k);
    let arr = make_string_array(ctx, items);
    let map = ctx.read_native_pin(map_pin, map);
    let k = ctx.read_native_pin(k_pin, k);
    ctx.unpin_native_roots(k_pin);
    cratonvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(k)),
            Value::Object(Some(arr)),
        ],
    )
    .ok();
}

/// Put a String value under a key into a HashMap. `map_pin` as in [`put_arr`].
pub(crate) fn put_str(
    ctx: &mut dyn NativeContext,
    map_pin: usize,
    map: ObjectRef,
    key: &str,
    value: &str,
) {
    let k = ctx.create_string(key);
    let k_pin = ctx.pin_native_root(k);
    let v = ctx.create_string(value);
    let map = ctx.read_native_pin(map_pin, map);
    let k = ctx.read_native_pin(k_pin, k);
    ctx.unpin_native_roots(k_pin);
    cratonvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .ok();
}

/// Pins `map` across [`populate_format_data_en_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `map` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn populate_format_data_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_format_data_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Populate a synthetic English-US `sun.text.resources.FormatData` bundle map.
pub(crate) fn populate_format_data_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // Pin across the per-entry allocations below — a moving young GC there
    // would relocate `map` (native stale-local family); `put_arr`/`put_str`
    // re-read the current ref from this pin before each put.
    let map_pin = ctx.pin_native_root(map);
    // 13 entries (12 months + empty 13th for lunar calendar slot, JDK convention).
    put_arr(
        ctx,
        map_pin,
        map,
        "MonthNames",
        &[
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
            "",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "MonthAbbreviations",
        &[
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "MonthNarrows",
        &[
            "J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D", "",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.MonthNames",
        &[
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
            "",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.MonthAbbreviations",
        &[
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.MonthNarrows",
        &[
            "J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D", "",
        ],
    );
    // 8 slots: index 0 unused, 1=Sunday..7=Saturday.
    put_arr(
        ctx,
        map_pin,
        map,
        "DayNames",
        &[
            "",
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "DayAbbreviations",
        &["", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "DayNarrows",
        &["", "S", "M", "T", "W", "T", "F", "S"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.DayNames",
        &[
            "",
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.DayAbbreviations",
        &["", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.DayNarrows",
        &["", "S", "M", "T", "W", "T", "F", "S"],
    );
    put_arr(ctx, map_pin, map, "AmPmMarkers", &["AM", "PM"]);
    put_arr(ctx, map_pin, map, "narrow.AmPmMarkers", &["a", "p"]);
    put_arr(ctx, map_pin, map, "Eras", &["BC", "AD"]);
    put_arr(ctx, map_pin, map, "short.Eras", &["BC", "AD"]);
    put_arr(ctx, map_pin, map, "narrow.Eras", &["B", "A"]);
    put_arr(
        ctx,
        map_pin,
        map,
        "QuarterNames",
        &["1st quarter", "2nd quarter", "3rd quarter", "4th quarter"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "QuarterAbbreviations",
        &["Q1", "Q2", "Q3", "Q4"],
    );
    put_arr(ctx, map_pin, map, "QuarterNarrows", &["1", "2", "3", "4"]);
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.QuarterNames",
        &["1st quarter", "2nd quarter", "3rd quarter", "4th quarter"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.QuarterAbbreviations",
        &["Q1", "Q2", "Q3", "Q4"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.QuarterNarrows",
        &["1", "2", "3", "4"],
    );
    // Standard 9-element layout used by SimpleDateFormat:
    // 4 time patterns (FULL/LONG/MEDIUM/SHORT), 4 date patterns, 1 date-time combiner.
    put_arr(
        ctx,
        map_pin,
        map,
        "DateTimePatterns",
        &[
            "h:mm:ss a zzzz",
            "h:mm:ss a z",
            "h:mm:ss a",
            "h:mm a",
            "EEEE, MMMM d, yyyy",
            "MMMM d, yyyy",
            "MMM d, yyyy",
            "M/d/yy",
            "{1} {0}",
        ],
    );
    // Calendar-keyed variant of DateTimePatterns used by some JDK paths.
    put_arr(
        ctx,
        map_pin,
        map,
        "gregorian.DateTimePatterns",
        &[
            "h:mm:ss a zzzz",
            "h:mm:ss a z",
            "h:mm:ss a",
            "h:mm a",
            "EEEE, MMMM d, yyyy",
            "MMMM d, yyyy",
            "MMM d, yyyy",
            "M/d/yy",
            "{1} {0}",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "DateTimePatternChars",
        &["GyMdkHmsSEDFwWahKzZYuXL"],
    );
    // Number format patterns: number / currency / percent / scientific.
    put_arr(
        ctx,
        map_pin,
        map,
        "NumberPatterns",
        &[
            "#,##0.###",
            "\u{00A4}#,##0.00;(\u{00A4}#,##0.00)",
            "#,##0%",
            "#E0",
        ],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "NumberElements",
        &[
            ".", ",", ";", "%", "0", "#", "-", "E", "\u{2030}", "\u{221E}", "NaN",
        ],
    );
    put_str(ctx, map_pin, map, "TimePatternChars", "hHmsSaEcLkKzZ");
    ctx.unpin_native_roots(map_pin);
}

/// Pins `map` across [`populate_locale_names_en_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `map` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn populate_locale_names_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_locale_names_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Populate a synthetic `sun.util.resources.LocaleNames` (English) bundle map.
pub(crate) fn populate_locale_names_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // Minimal language/country names that cover the common queries; for any
    // missing key callers fall back to the locale code via getDisplayName
    // overrides we register elsewhere.
    // Pin across the per-entry allocations below — a moving young GC there
    // would relocate `map` (native stale-local family).
    let map_pin = ctx.pin_native_root(map);
    let langs: &[(&str, &str)] = &[
        ("en", "English"),
        ("fr", "French"),
        ("de", "German"),
        ("es", "Spanish"),
        ("it", "Italian"),
        ("ja", "Japanese"),
        ("ko", "Korean"),
        ("zh", "Chinese"),
        ("ru", "Russian"),
        ("pt", "Portuguese"),
        ("nl", "Dutch"),
        ("sv", "Swedish"),
        ("ar", "Arabic"),
        ("hi", "Hindi"),
        ("tr", "Turkish"),
    ];
    let countries: &[(&str, &str)] = &[
        ("US", "United States"),
        ("GB", "United Kingdom"),
        ("CA", "Canada"),
        ("FR", "France"),
        ("DE", "Germany"),
        ("ES", "Spain"),
        ("IT", "Italy"),
        ("JP", "Japan"),
        ("CN", "China"),
        ("KR", "South Korea"),
        ("RU", "Russia"),
        ("BR", "Brazil"),
        ("AU", "Australia"),
        ("NL", "Netherlands"),
        ("SE", "Sweden"),
        ("MX", "Mexico"),
        ("IN", "India"),
        ("ZA", "South Africa"),
    ];
    for (k, v) in langs {
        put_str(ctx, map_pin, map, k, v);
    }
    for (k, v) in countries {
        put_str(ctx, map_pin, map, k, v);
    }
    ctx.unpin_native_roots(map_pin);
}

/// Pins `map` across [`populate_calendar_data_en_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `map` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn populate_calendar_data_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_calendar_data_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Populate a synthetic `sun.util.resources.CalendarData` (English) bundle map.
pub(crate) fn populate_calendar_data_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // Pin across the per-entry allocations below — a moving young GC there
    // would relocate `map` (native stale-local family).
    let map_pin = ctx.pin_native_root(map);
    put_str(ctx, map_pin, map, "firstDayOfWeek", "1"); // Sunday
    put_str(ctx, map_pin, map, "minimalDaysInFirstWeek", "1");
    ctx.unpin_native_roots(map_pin);
}

/// Populate a synthetic `sun.util.resources.CurrencyNames` bundle map.
pub(crate) fn populate_currency_names_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // Pin across the per-entry allocations below — a moving young GC there
    // would relocate `map` (native stale-local family).
    let map_pin = ctx.pin_native_root(map);
    put_str(ctx, map_pin, map, "USD", "US Dollar");
    put_str(ctx, map_pin, map, "EUR", "Euro");
    put_str(ctx, map_pin, map, "GBP", "British Pound");
    put_str(ctx, map_pin, map, "JPY", "Japanese Yen");
    put_str(ctx, map_pin, map, "CNY", "Chinese Yuan");
    put_str(ctx, map_pin, map, "usd", "$");
    put_str(ctx, map_pin, map, "eur", "\u{20AC}");
    put_str(ctx, map_pin, map, "gbp", "\u{00A3}");
    put_str(ctx, map_pin, map, "jpy", "\u{00A5}");
    ctx.unpin_native_roots(map_pin);
}

/// Load a ResourceBundle from a .properties file on the classpath, with a
/// fallback synthetic English-US population for well-known JDK locale
/// bundles whose real class-based implementations live in the
/// `jdk.localedata` module that our jimage path does not surface.
pub(crate) fn resource_bundle_get_bundle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let bundle_name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ResourceBundle", 2)?;
    // Pin across the map alloc / per-entry puts below — a moving young GC
    // there would relocate the fresh bundle and map (native stale-local
    // family).
    let obj_pin = ctx.pin_native_root(obj);
    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
    let map_pin = ctx.pin_native_root(map);
    cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    let obj = ctx.read_native_pin(obj_pin, obj);
    let map = ctx.read_native_pin(map_pin, map);
    ctx.set_field(obj, 0, Value::Object(Some(map)));
    ctx.set_field(obj, 1, Value::Object(None));

    // Try to load the .properties file from classpath
    let resource_name = format!("{}.properties", bundle_name.replace('.', "/"));
    let mut populated_from_props = false;
    if let Some(bytes) = ctx.find_resource(&resource_name) {
        if let Ok(content) = String::from_utf8(bytes) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                // Split on '=' or ':'
                let sep_pos = line.find(|c: char| c == '=' || c == ':');
                if let Some(pos) = sep_pos {
                    let key = line[..pos].trim();
                    let value = line[pos + 1..].trim();
                    let k = ctx.create_string(key);
                    let k_pin = ctx.pin_native_root(k);
                    let v = ctx.create_string(value);
                    let map = ctx.read_native_pin(map_pin, map);
                    let k = ctx.read_native_pin(k_pin, k);
                    ctx.unpin_native_roots(k_pin);
                    cratonvm_native_collections::native_map_put_pub(
                        ctx,
                        &[
                            Value::Object(Some(map)),
                            Value::Object(Some(k)),
                            Value::Object(Some(v)),
                        ],
                    )
                    .ok();
                    populated_from_props = true;
                }
            }
        }
    }

    // Synthetic fallback for locale-data bundles that the JDK loads from
    // class files inside `jdk.localedata` (not surfaced by our jimage path).
    // We always merge these in for the matching base names so callers get
    // sensible English-US defaults — Jackson's StdDateFormat needs
    // MonthNames / DayNames / AmPmMarkers / Eras / DateTimePatterns at
    // minimum.
    if !populated_from_props {
        let bn = bundle_name.as_str();
        let mut map = ctx.read_native_pin(map_pin, map);
        if bn.starts_with("sun.text.resources.FormatData")
            || bn.starts_with("sun.text.resources.cldr.FormatData")
            || bn.starts_with("sun.text.resources.ext.FormatData")
        {
            populate_format_data_en(ctx, &mut map);
        } else if bn.starts_with("sun.util.resources.LocaleNames")
            || bn.starts_with("sun.util.resources.cldr.LocaleNames")
            || bn.starts_with("sun.util.resources.ext.LocaleNames")
        {
            populate_locale_names_en(ctx, &mut map);
        } else if bn.starts_with("sun.util.resources.CalendarData")
            || bn.starts_with("sun.util.resources.cldr.CalendarData")
        {
            populate_calendar_data_en(ctx, &mut map);
        } else if bn.starts_with("sun.util.resources.CurrencyNames")
            || bn.starts_with("sun.util.resources.cldr.CurrencyNames")
        {
            populate_currency_names_en(ctx, map);
        }
    }

    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// =============================================================================
// DateTimeFormatterBuilder = 2-field (parts=0 ArrayList, locale=1)
// =============================================================================

pub(crate) fn register_p65_datetime_builder(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let dtfb = "java/time/format/DateTimeFormatterBuilder";
    r.register(dtfb, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        ctx.set_field(al, 0, Value::Object(None));
        ctx.set_field(al, 1, Value::Int(0));
        ctx.set_field(this, 0, Value::Object(Some(al)));
        ctx.set_field(this, 1, Value::Object(None));
        Ok(None)
    });
    // appendLiteral returns this
    r.register(
        dtfb,
        "appendLiteral",
        "(Ljava/lang/String;)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendLiteral",
        "(C)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendValue",
        "(Ljava/time/temporal/TemporalField;)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendValue",
        "(Ljava/time/temporal/TemporalField;I)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendText",
        "(Ljava/time/temporal/TemporalField;)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendPattern",
        "(Ljava/lang/String;)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "appendOptional",
        "(Ljava/time/format/DateTimeFormatter;)Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "optionalStart",
        "()Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "optionalEnd",
        "()Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "toFormatter",
        "()Ljava/time/format/DateTimeFormatter;",
        |ctx, _args| {
            // Return a basic DateTimeFormatter stub
            let dtf = try_alloc_concurrent_synthetic(ctx, "java/time/format/DateTimeFormatter", 2)?;
            let pat = ctx.create_string("");
            ctx.set_field(dtf, 0, Value::Object(Some(pat)));
            ctx.set_field(dtf, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(dtf))))
        },
    );
    r.register(
        dtfb,
        "toFormatter",
        "(Ljava/util/Locale;)Ljava/time/format/DateTimeFormatter;",
        |ctx, _args| {
            let dtf = try_alloc_concurrent_synthetic(ctx, "java/time/format/DateTimeFormatter", 2)?;
            let pat = ctx.create_string("");
            ctx.set_field(dtf, 0, Value::Object(Some(pat)));
            ctx.set_field(dtf, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(dtf))))
        },
    );
    r.register(
        dtfb,
        "parseCaseInsensitive",
        "()Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        dtfb,
        "parseCaseSensitive",
        "()Ljava/time/format/DateTimeFormatterBuilder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // FormatStyle enum
    let fs = "java/time/format/FormatStyle";
    r.register(
        fs,
        "values",
        "()[Ljava/time/format/FormatStyle;",
        |ctx, _args| {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
            let names = ["FULL", "LONG", "MEDIUM", "SHORT"];
            for (i, name) in names.iter().enumerate() {
                if let Ok(Some(e)) =
                    p57_alloc_enum(ctx, "java/time/format/FormatStyle", name, i as i32)
                {
                    ctx.set_array_element(arr, i, e);
                }
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        fs,
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/format/FormatStyle;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => "MEDIUM".to_string(),
            };
            let ordinal = match name.as_str() {
                "FULL" => 0,
                "LONG" => 1,
                "MEDIUM" => 2,
                "SHORT" => 3,
                _ => 2,
            };
            p57_alloc_enum(ctx, "java/time/format/FormatStyle", &name, ordinal)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.text.Collator = 1-field (strength=0 Int)
// =============================================================================

/// Compare two strings according to collation strength.
pub(crate) fn collator_compare(s1: &str, s2: &str, strength: i32) -> i32 {
    let n1 = collator_normalize(s1, strength);
    let n2 = collator_normalize(s2, strength);
    n1.cmp(&n2) as i32
}

/// Normalize a string for collation comparison at the given strength level.
pub(crate) fn collator_normalize(s: &str, strength: i32) -> String {
    match strength {
        0 => {
            // PRIMARY: fold case and strip accents
            s.chars()
                .map(|c| {
                    let base = strip_accent(c);
                    base.to_lowercase().next().unwrap_or(base)
                })
                .collect()
        }
        1 => {
            // SECONDARY: fold case, keep accents
            s.to_lowercase()
        }
        _ => {
            // TERTIARY/IDENTICAL: exact ordering
            s.to_string()
        }
    }
}

/// Strip accent from a character (common Latin characters)
pub(crate) fn strip_accent(c: char) -> char {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' => 'e',
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' => 'i',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' => 'u',
        'Ñ' | 'ñ' => 'n',
        'Ç' | 'ç' => 'c',
        'Ý' | 'ý' | 'ÿ' => 'y',
        _ => c,
    }
}

pub(crate) fn register_p66_collator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/text/Collator";
    r.register(c, "getInstance", "()Ljava/text/Collator;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/text/Collator", 1)?;
        ctx.set_field(obj, 0, Value::Int(2)); // SECONDARY strength
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        c,
        "getInstance",
        "(Ljava/util/Locale;)Ljava/text/Collator;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/Collator", 1)?;
            ctx.set_field(obj, 0, Value::Int(2));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        c,
        "compare",
        "(Ljava/lang/String;Ljava/lang/String;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let strength = ctx.get_field(this, 0).as_int().unwrap_or(2);
            let s1 = match args.get(1) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            let s2 = match args.get(2) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Int(collator_compare(&s1, &s2, strength))))
        },
    );
    r.register(
        c,
        "compare",
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let strength = ctx.get_field(this, 0).as_int().unwrap_or(2);
            let s1 = match args.get(1) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            let s2 = match args.get(2) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Int(collator_compare(&s1, &s2, strength))))
        },
    );
    r.register(
        c,
        "equals",
        "(Ljava/lang/String;Ljava/lang/String;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let strength = ctx.get_field(this, 0).as_int().unwrap_or(2);
            let s1 = match args.get(1) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            let s2 = match args.get(2) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Int(
                if collator_compare(&s1, &s2, strength) == 0 {
                    1
                } else {
                    0
                },
            )))
        },
    );
    r.register(c, "getStrength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(c, "setStrength", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let strength = args.get(1).copied().unwrap_or(Value::Int(2));
        ctx.set_field(this, 0, strength);
        Ok(None)
    });
    r.register(
        c,
        "getCollationKey",
        "(Ljava/lang/String;)Ljava/text/CollationKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let strength = ctx.get_field(this, 0).as_int().unwrap_or(2);
            let s = match args.get(1) {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => String::new(),
            };
            // Normalize string based on strength for comparison
            let normalized = collator_normalize(&s, strength);
            // CollationKey = 2-field (source=0, key_bytes=1)
            let ck = try_alloc_concurrent_synthetic(ctx, "java/text/CollationKey", 2)?;
            ctx.set_field(ck, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            let key_s = ctx.create_string(&normalized);
            ctx.set_field(ck, 1, Value::Object(Some(key_s)));
            Ok(Some(Value::Object(Some(ck))))
        },
    );
    // CollationKey methods
    let ck_cls = "java/text/CollationKey";
    r.register(
        ck_cls,
        "getSourceString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        ck_cls,
        "compareTo",
        "(Ljava/text/CollationKey;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let k1 = match ctx.get_field(this, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let k2 = match ctx.get_field(other, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(Value::Int(k1.cmp(&k2) as i32)))
        },
    );
    r.register(ck_cls, "toByteArray", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = match ctx.get_field(this, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let bytes = key.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });

    // Collator constants — KEEP. `static final int` FIELD reads (the "I" field
    // descriptor, not a method), and 0/1/2/3 are the literal strength values
    // `java.text.Collator` declares.
    r.register(c, "PRIMARY", "I", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(c, "SECONDARY", "I", |_ctx, _args| Ok(Some(Value::Int(1))));
    r.register(c, "TERTIARY", "I", |_ctx, _args| Ok(Some(Value::Int(2))));
    r.register(c, "IDENTICAL", "I", |_ctx, _args| Ok(Some(Value::Int(3))));
    r.set_category(__prev_cat);
}

// =============================================================================
// java.text.BreakIterator = 3-field (text=0 String, pos=1 Int, kind=2 Int)
// kind: 0=word, 1=sentence, 2=character, 3=line
// =============================================================================

pub(crate) const BI_WORD: i32 = 0;

pub(crate) const BI_SENTENCE: i32 = 1;

pub(crate) const BI_CHARACTER: i32 = 2;

pub(crate) const BI_LINE: i32 = 3;

/// True when the receiver is this VM's FABRICATED `java/text/BreakIterator`
/// carrier, and not a real subclass instance.
///
/// `java.text.BreakIterator` is ABSTRACT, so no real instance can carry that
/// exact class name — only [`bi_alloc_kind`]'s fabrication does, and the
/// three-field layout below belongs to it alone. The discriminator is
/// therefore exact rather than a heuristic about widths.
///
/// # Why anything needs to ask
///
/// Of the seventeen registrations on `java/text/BreakIterator`, exactly TWO
/// name a method that is CONCRETE on the abstract class — `setText(String)`
/// and `preceding(int)` (`javap -p` on the JDK 25 image: everything else this
/// registrar claims is `abstract`). A dispatch door asks the registry about
/// the DECLARING class of the resolved method, so for the abstract ones a real
/// `sun.text.RuleBasedBreakIterator` receiver resolves to its own body and the
/// natives never see it. For those two, the declaring class IS
/// `java.text.BreakIterator`, so the natives claimed every BreakIterator in
/// the VM — including the real ones the provider chain builds.
///
/// MEASURED 2026-09-11 with `apps/probes/L1BreakIterRealProbe`, whose `P.*`
/// rows reach `BreakIteratorProviderImpl` without going through the pinned
/// static factories. The chain already worked: `P.wordClass` answered
/// `sun.text.RuleBasedBreakIterator` and `P.wordClass.th`
/// `sun.text.DictionaryBasedBreakIterator`, so wave 5's two `LocaleResources`
/// readers find the bundle, find the blob, and the constructor validates the
/// rule data. And then every walk over that correct iterator answered `[0]`,
/// because `setText(String)` had written the text into slot 0 of an object
/// whose slot 0 is `charCategoryTable`, and the real `first()`/`next()` ran
/// against a `text` field nobody had set. The same one cause took
/// `GraphemeBreakIterator` — pure JDK code that reads no bundle at all — to
/// `NullPointerException: "this.boundaries" is null`.
///
/// One hijacked setter, three symptoms, and none of them in the family the
/// symptoms pointed at.
fn bi_is_fabricated_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("java/text/BreakIterator")
}

/// `BreakIterator.setText(String)`'s REAL body, for a receiver this VM did not
/// fabricate: `setText(new StringCharacterIterator(newText))`.
///
/// Read out of `javap -c java.text.BreakIterator` rather than from memory —
/// the whole method is those two steps, and `setText(CharacterIterator)` is
/// abstract, so the virtual call lands on the receiver's own implementation
/// and no native can claim it.
fn bi_delegate_set_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    text: Value,
) -> MethodCallResult {
    let s = match text {
        Value::Object(Some(s)) => s,
        // A null argument is `new StringCharacterIterator(null)` on HotSpot,
        // i.e. a NullPointerException out of the constructor — so throw one
        // here rather than inventing a quieter answer.
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("BreakIterator.setText: text is null".to_string()),
            }
            .into())
        }
    };
    // GC-SAFETY: `new_object_initialized` runs `<init>` and can move the heap,
    // and `this` is a bare Rust local the collector cannot see.
    let this_pin = ctx.pin_native_root(this);
    let built = ctx.new_object_initialized(
        "java/text/StringCharacterIterator",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    );
    let this = ctx.read_native_pin(this_pin, this);
    let built = match built {
        Ok(b) => b,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let r = match built {
        Some(Value::Object(Some(it))) => ctx.invoke_virtual(
            this,
            "setText",
            "(Ljava/text/CharacterIterator;)V",
            &[Value::Object(Some(it))],
        ),
        // No `StringCharacterIterator` in this image is the synthetic-JDK
        // shape; leaving the receiver untouched is what happened before this
        // delegation existed.
        _ => Ok(None),
    };
    ctx.unpin_native_roots(this_pin);
    r?;
    Ok(None)
}

/// `BreakIterator.preceding(int)`'s REAL body, for a receiver this VM did not
/// fabricate.
///
/// ```text
///   int pos = following(offset);
///   while (pos >= offset && pos != DONE) pos = previous();
///   return pos;
/// ```
///
/// `javap -c`, not paraphrase. Both calls are virtual and both targets are
/// abstract on `BreakIterator`, so they land on the real implementation. The
/// loop has no iteration guard for the same reason the JDK's has none: it
/// terminates on `previous()` reaching `DONE`, and an implementation where it
/// does not is one that hangs on HotSpot too.
fn bi_delegate_preceding(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: i32,
) -> MethodCallResult {
    const DONE: i32 = -1;
    let this_pin = ctx.pin_native_root(this);
    // One exit path for the pin: compute into a Result and release once.
    let out = (|| -> Result<i32, MethodCallFailed> {
        let this_now = ctx.read_native_pin(this_pin, this);
        let mut pos =
            match ctx.invoke_virtual(this_now, "following", "(I)I", &[Value::Int(offset)])? {
                Some(Value::Int(p)) => p,
                _ => DONE,
            };
        while pos >= offset && pos != DONE {
            let this_now = ctx.read_native_pin(this_pin, this);
            pos = match ctx.invoke_virtual(this_now, "previous", "()I", &[])? {
                Some(Value::Int(p)) => p,
                _ => DONE,
            };
        }
        Ok(pos)
    })();
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(out?)))
}

pub(crate) fn bi_alloc_kind(
    ctx: &mut dyn NativeContext,
    kind: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/text/BreakIterator", 3)?;
    ctx.set_field(obj, 0, Value::Object(None));
    ctx.set_field(obj, 1, Value::Int(0));
    ctx.set_field(obj, 2, Value::Int(kind));
    Ok(obj)
}

pub(crate) fn java_text_len(text: &str) -> usize {
    text.encode_utf16().count()
}

pub(crate) fn java_text_pos_to_byte(text: &str, pos: usize) -> usize {
    let mut units = 0;
    for (byte_idx, ch) in text.char_indices() {
        if units >= pos {
            return byte_idx;
        }
        units += ch.len_utf16();
        if units > pos {
            return byte_idx;
        }
    }
    text.len()
}

pub(crate) fn byte_pos_to_java_text_pos(text: &str, pos: usize) -> usize {
    let mut units = 0;
    for (byte_idx, ch) in text.char_indices() {
        if byte_idx >= pos {
            return units;
        }
        units += ch.len_utf16();
    }
    units
}

/// A word character for word-break purposes, in ANY script.
///
/// This used to be `bytes[i].is_ascii_alphanumeric()` applied to the UTF-8
/// bytes, which makes every non-ASCII letter a NON-word character. That is not
/// simply "less accurate": it puts a word boundary at every ASCII/non-ASCII
/// transition, so `"AΣ"` broke between the two letters while `"ΑΣ"` did not —
/// the all-Greek string was right only because BOTH chars were misclassified
/// the same way, and the bug showed up exactly where the classification changed.
///
/// Found through `G9-1`'s final-sigma rows. `ConditionalSpecialCasing`'s
/// `isFinalCased` walks backwards from the sigma only while
/// `!wordBoundary.isBoundary(i)`, so a spurious boundary meant it never reached
/// the preceding cased letter and `"AΣ".toLowerCase(ROOT)` produced the medial
/// `σ` instead of the final `ς`.
fn bi_is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// UAX #29 `MidLetter` / `MidNumLetQ`: punctuation that does NOT break a word
/// when it sits between two word characters (rules WB6 and WB7), so `"A'Σ"`
/// and `"can't"` are each one word.
///
/// This is the documented subset of those classes, not the whole of UAX #29 —
/// the full algorithm also has extend/format, regional-indicator, numeric and
/// Katakana rules that this iterator has never implemented. What is claimed
/// here is only that a letter is a letter in every script, and that these
/// connectors join.
fn bi_is_mid_word(c: char) -> bool {
    matches!(
        c,
        '\u{0027}'
            | '\u{002E}'
            | '\u{003A}'
            | '\u{00B7}'
            | '\u{0387}'
            | '\u{05F4}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2024}'
            | '\u{2027}'
            | '\u{FE13}'
            | '\u{FE52}'
            | '\u{FE55}'
            | '\u{FF07}'
            | '\u{FF0E}'
            | '\u{FF1A}'
    )
}

/// `text` as `(byte offset, char)` pairs, so the scans below can look at
/// neighbours without decoding twice.
fn bi_chars(text: &str) -> Vec<(usize, char)> {
    text.char_indices().collect()
}

/// Whether index `k` counts as part of a word run, with WB6/WB7 applied: a
/// connector is in-word only when a word character sits on BOTH sides.
fn bi_in_word(chars: &[(usize, char)], k: usize) -> bool {
    let c = chars[k].1;
    if bi_is_word_char(c) {
        return true;
    }
    if !bi_is_mid_word(c) {
        return false;
    }
    k > 0
        && k + 1 < chars.len()
        && bi_is_word_char(chars[k - 1].1)
        && bi_is_word_char(chars[k + 1].1)
}

/// Find the next break boundary after `pos` in `text` for the given iterator kind.
pub(crate) fn bi_find_next(text: &str, pos: usize, kind: i32) -> Option<usize> {
    let text_len = java_text_len(text);
    if pos >= text_len {
        return None;
    }
    let bytes = text.as_bytes();
    let start = java_text_pos_to_byte(text, pos);
    match kind {
        BI_WORD => {
            // Word boundary: the transition between a word run and a non-word
            // run, classified per CHARACTER (see `bi_is_word_char`) rather than
            // per UTF-8 byte.
            let chars = bi_chars(text);
            let mut k = chars.partition_point(|(b, _)| *b < start);
            let at_word = k < chars.len() && bi_in_word(&chars, k);
            while k < chars.len() && bi_in_word(&chars, k) == at_word {
                k += 1;
            }
            let i = chars.get(k).map_or(bytes.len(), |(b, _)| *b);
            Some(byte_pos_to_java_text_pos(text, i))
        }
        BI_SENTENCE => {
            // Sentence boundary: after .!? followed by whitespace
            let mut i = start;
            while i < bytes.len() {
                if bytes[i] == b'.' || bytes[i] == b'!' || bytes[i] == b'?' {
                    let after_punctuation = i + 1;
                    // A dot inside an identifier/domain-like token is not a sentence
                    // boundary. In particular, Spring Boot metadata reasons commonly
                    // contain names such as `spring.server`; returning at the dot
                    // silently truncates the generated short reason to `spring.`.
                    // Keep the deliberately small ASCII implementation, but preserve
                    // the essential BreakIterator contract: sentence terminators break
                    // only at end-of-text or before whitespace.
                    if after_punctuation < bytes.len()
                        && !bytes[after_punctuation].is_ascii_whitespace()
                    {
                        i = after_punctuation;
                        continue;
                    }
                    i = after_punctuation;
                    // Skip trailing whitespace
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    return Some(byte_pos_to_java_text_pos(text, i));
                }
                i += 1;
            }
            Some(text_len)
        }
        BI_CHARACTER => {
            // Character boundary: next Unicode codepoint
            let ch = text[start..].chars().next()?;
            Some(pos + ch.len_utf16())
        }
        BI_LINE => {
            // Line break opportunity: after whitespace or hyphens. HotSpot's
            // line iterator reports the boundary after the separating
            // whitespace/hyphen run, not inside it; Picocli relies on that to
            // avoid hard-wrapping option names and env-var tokens.
            let mut i = start;
            while i < bytes.len() {
                if bytes[i].is_ascii_whitespace() {
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    return Some(byte_pos_to_java_text_pos(text, i));
                }
                if bytes[i] == b'-' {
                    while i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                    }
                    return Some(byte_pos_to_java_text_pos(text, i));
                }
                i += 1;
            }
            Some(text_len)
        }
        _ => Some(text_len),
    }
}

/// Find the previous break boundary before `pos`.
pub(crate) fn bi_find_prev(text: &str, pos: usize, kind: i32) -> Option<usize> {
    if pos == 0 {
        return None;
    }
    let bytes = text.as_bytes();
    match kind {
        BI_WORD => {
            // The mirror of the `next` arm, same per-character classification.
            let chars = bi_chars(text);
            let byte_pos = java_text_pos_to_byte(text, pos);
            let mut k = chars.partition_point(|(b, _)| *b < byte_pos);
            if k > 0 {
                k -= 1;
            }
            let at_word = k < chars.len() && bi_in_word(&chars, k);
            while k > 0 && bi_in_word(&chars, k - 1) == at_word {
                k -= 1;
            }
            let i = chars.get(k).map_or(0, |(b, _)| *b);
            Some(byte_pos_to_java_text_pos(text, i))
        }
        BI_SENTENCE => {
            let mut i = java_text_pos_to_byte(text, pos);
            if i > 0 {
                i -= 1;
            }
            // Find the previous sentence-ending punctuation
            while i > 0 {
                if bytes[i - 1] == b'.' || bytes[i - 1] == b'!' || bytes[i - 1] == b'?' {
                    return Some(byte_pos_to_java_text_pos(text, i));
                }
                i -= 1;
            }
            Some(0)
        }
        BI_CHARACTER => {
            // Previous codepoint boundary
            let mut i = pos;
            if i > 0 {
                i -= 1;
                while i > 0 && !text.is_char_boundary(i) {
                    i -= 1;
                }
            }
            Some(i)
        }
        BI_LINE => {
            let mut i = java_text_pos_to_byte(text, pos);
            while i > 0 {
                let idx = i - 1;
                if bytes[idx].is_ascii_whitespace() {
                    let mut start = idx;
                    while start > 0 && bytes[start - 1].is_ascii_whitespace() {
                        start -= 1;
                    }
                    let boundary = byte_pos_to_java_text_pos(text, idx + 1);
                    if boundary < pos {
                        return Some(boundary);
                    }
                    i = start;
                    continue;
                }
                if bytes[idx] == b'-' {
                    let mut start = idx;
                    while start > 0 && bytes[start - 1] == b'-' {
                        start -= 1;
                    }
                    let boundary = byte_pos_to_java_text_pos(text, idx + 1);
                    if boundary < pos {
                        return Some(boundary);
                    }
                    i = start;
                    continue;
                }
                i -= 1;
            }
            Some(0)
        }
        _ => Some(0),
    }
}

#[cfg(test)]
pub(crate) mod break_iterator_line_boundary_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const HELP_TEXT: &str =
        "specified, --user is used, and the env variable KC_CLI_PASSWORD is not defined";
    const PICOCLI_BREAK_TEXT: &str =
        "specified, \u{00ff}\u{00ff}user is used, and the env variable KC_CLI_PASSWORD is not defined";

    fn collect_line_boundaries(text: &str) -> Vec<usize> {
        let text_len = java_text_len(text);
        let mut pos = 0;
        let mut out = Vec::new();
        while let Some(next) = bi_find_next(text, pos, BI_LINE) {
            assert!(next > pos, "line boundary did not advance from {pos}");
            out.push(next);
            if next == text_len {
                break;
            }
            pos = next;
        }
        out
    }

    #[test]
    fn line_next_boundaries_match_hotspot_for_cli_help_text() {
        assert_eq!(
            collect_line_boundaries(HELP_TEXT),
            vec![11, 13, 18, 21, 27, 31, 35, 39, 48, 64, 67, 71, 78]
        );
    }

    #[test]
    fn line_boundaries_do_not_split_env_var_after_whitespace() {
        assert_eq!(bi_find_next(HELP_TEXT, 47, BI_LINE), Some(48));
        assert_eq!(bi_find_next(HELP_TEXT, 48, BI_LINE), Some(64));
        assert_eq!(bi_find_prev(HELP_TEXT, 48, BI_LINE), Some(39));
        assert_eq!(bi_find_prev(HELP_TEXT, 49, BI_LINE), Some(48));
    }

    #[test]
    fn line_boundaries_treat_hyphen_runs_as_one_separator() {
        assert_eq!(bi_find_next(HELP_TEXT, 11, BI_LINE), Some(13));
        assert_eq!(bi_find_prev(HELP_TEXT, 13, BI_LINE), Some(11));
        assert_eq!(bi_find_prev(HELP_TEXT, 14, BI_LINE), Some(13));
    }

    #[test]
    fn line_boundaries_return_java_offsets_for_non_ascii_replacement_text() {
        assert_eq!(
            collect_line_boundaries(PICOCLI_BREAK_TEXT),
            vec![11, 18, 21, 27, 31, 35, 39, 48, 64, 67, 71, 78]
        );
        assert_eq!(bi_find_next(PICOCLI_BREAK_TEXT, 39, BI_LINE), Some(48));
        assert_eq!(bi_find_next(PICOCLI_BREAK_TEXT, 48, BI_LINE), Some(64));
        assert_eq!(bi_find_prev(PICOCLI_BREAK_TEXT, 48, BI_LINE), Some(39));
        assert_eq!(bi_find_prev(PICOCLI_BREAK_TEXT, 49, BI_LINE), Some(48));
    }
}

#[cfg(test)]
pub(crate) mod break_iterator_sentence_boundary_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn sentence_does_not_end_inside_dotted_identifier() {
        let text = "Server namespace has moved to spring.server";
        assert_eq!(
            bi_find_next(text, 0, BI_SENTENCE),
            Some(java_text_len(text))
        );
    }

    #[test]
    fn sentence_ends_before_whitespace_separated_successor() {
        let text = "First sentence. Second sentence.";
        assert_eq!(bi_find_next(text, 0, BI_SENTENCE), Some(16));
    }
}

pub(crate) fn register_p66_break_iterator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let bi = "java/text/BreakIterator";
    r.register(
        bi,
        "getWordInstance",
        "()Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_WORD)?)))),
    );
    r.register(
        bi,
        "getWordInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_WORD)?)))),
    );
    r.register(
        bi,
        "getSentenceInstance",
        "()Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_SENTENCE)?)))),
    );
    r.register(
        bi,
        "getSentenceInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_SENTENCE)?)))),
    );
    r.register(
        bi,
        "getCharacterInstance",
        "()Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_CHARACTER)?)))),
    );
    r.register(
        bi,
        "getCharacterInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_CHARACTER)?)))),
    );
    r.register(
        bi,
        "getLineInstance",
        "()Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_LINE)?)))),
    );
    r.register(
        bi,
        "getLineInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
        |ctx, _args| Ok(Some(Value::Object(Some(bi_alloc_kind(ctx, BI_LINE)?)))),
    );
    // CONCRETE on the abstract class, so this native claims EVERY
    // BreakIterator in the VM and not just the fabricated carrier — see
    // `bi_is_fabricated_carrier` for the measurement and the three symptoms
    // that came of it. A real subclass gets the real two-line body.
    r.register(bi, "setText", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = args.get(1).copied().unwrap_or(Value::Object(None));
        if !bi_is_fabricated_carrier(ctx, this) {
            return bi_delegate_set_text(ctx, this, text);
        }
        ctx.set_field(this, 0, text);
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(bi, "first", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(0));
        Ok(Some(Value::Int(0)))
    });
    r.register(bi, "last", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).map(|t| t.len() as i32).unwrap_or(0),
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(len));
        Ok(Some(Value::Int(len)))
    });
    r.register(bi, "next", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(-1))),
        };
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let kind = ctx.get_field(this, 2).as_int().unwrap_or(0);
        match bi_find_next(&text, pos, kind) {
            Some(next_pos) if next_pos > pos => {
                ctx.set_field(this, 1, Value::Int(next_pos as i32));
                Ok(Some(Value::Int(next_pos as i32)))
            }
            _ => Ok(Some(Value::Int(-1))), // DONE
        }
    });
    r.register(bi, "previous", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(-1))),
        };
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let kind = ctx.get_field(this, 2).as_int().unwrap_or(0);
        match bi_find_prev(&text, pos, kind) {
            Some(prev_pos) if prev_pos < pos => {
                ctx.set_field(this, 1, Value::Int(prev_pos as i32));
                Ok(Some(Value::Int(prev_pos as i32)))
            }
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(bi, "following", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let offset = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let text = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(-1))),
        };
        let kind = ctx.get_field(this, 2).as_int().unwrap_or(0);
        match bi_find_next(&text, offset, kind) {
            Some(next_pos) => {
                ctx.set_field(this, 1, Value::Int(next_pos as i32));
                Ok(Some(Value::Int(next_pos as i32)))
            }
            None => Ok(Some(Value::Int(-1))),
        }
    });
    // The OTHER concrete one. Same hijack, same remedy: a real subclass gets
    // `BreakIterator.preceding`'s own loop over its own `following`/
    // `previous`, both of which are abstract and therefore unclaimable.
    r.register(bi, "preceding", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !bi_is_fabricated_carrier(ctx, this) {
            let offset = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            return bi_delegate_preceding(ctx, this, offset);
        }
        let offset = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let text = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(-1))),
        };
        let kind = ctx.get_field(this, 2).as_int().unwrap_or(0);
        match bi_find_prev(&text, offset, kind) {
            Some(prev_pos) => {
                ctx.set_field(this, 1, Value::Int(prev_pos as i32));
                Ok(Some(Value::Int(prev_pos as i32)))
            }
            None => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(bi, "current", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        bi,
        "getText",
        "()Ljava/text/CharacterIterator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Return a StringCharacterIterator wrapping the text
            let ci = try_alloc_concurrent_synthetic(ctx, "java/text/StringCharacterIterator", 2)?;
            ctx.set_field(ci, 0, ctx.get_field(this, 0));
            ctx.set_field(ci, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(ci))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.text.CompactNumberFormat — Java 12
// =============================================================================

pub(crate) fn register_p69_compact_number_format(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cnf = "java/text/CompactNumberFormat";
    // CompactNumberFormat = 2-field (locale=0, style=1 Int: 0=SHORT, 1=LONG)
    r.register(
        cnf,
        "<init>",
        "(Ljava/lang/String;Ljava/text/DecimalFormatSymbols;[Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, Value::Object(None));
            ctx.set_field(this, 1, Value::Int(0));
            Ok(None)
        },
    );
    r.register(
        cnf,
        "format",
        "(DLjava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
        |ctx, args| {
            let val = match args.get(1) {
                Some(Value::Double(v)) => *v,
                _ => 0.0,
            };
            let formatted = if val.abs() >= 1_000_000_000.0 {
                format!("{:.0}B", val / 1_000_000_000.0)
            } else if val.abs() >= 1_000_000.0 {
                format!("{:.0}M", val / 1_000_000.0)
            } else if val.abs() >= 1000.0 {
                format!("{:.0}K", val / 1000.0)
            } else {
                format!("{:.0}", val)
            };
            let s = ctx.create_string(&formatted);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        cnf,
        "format",
        "(JLjava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
        |ctx, args| {
            let val = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let formatted = if val.unsigned_abs() >= 1_000_000_000 {
                format!("{}B", val / 1_000_000_000)
            } else if val.unsigned_abs() >= 1_000_000 {
                format!("{}M", val / 1_000_000)
            } else if val.unsigned_abs() >= 1000 {
                format!("{}K", val / 1000)
            } else {
                format!("{}", val)
            };
            let s = ctx.create_string(&formatted);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // NumberFormat.getCompactNumberInstance — Java 12
    r.register(
        "java/text/NumberFormat",
        "getCompactNumberInstance",
        "()Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/CompactNumberFormat", 2)?;
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        "java/text/NumberFormat",
        "getCompactNumberInstance",
        "(Ljava/util/Locale;Ljava/text/NumberFormat$Style;)Ljava/text/NumberFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/CompactNumberFormat", 2)?;
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // NumberFormat.Style enum
    let style = "java/text/NumberFormat$Style";
    r.register(
        style,
        "SHORT",
        "Ljava/text/NumberFormat$Style;",
        |ctx, _args| p57_alloc_enum(ctx, "java/text/NumberFormat$Style", "SHORT", 0),
    );
    r.register(
        style,
        "LONG",
        "Ljava/text/NumberFormat$Style;",
        |ctx, _args| p57_alloc_enum(ctx, "java/text/NumberFormat$Style", "LONG", 1),
    );
    r.set_category(__prev_cat);
}
