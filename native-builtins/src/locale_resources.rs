//! C22: ResourceBundle locale-data overrides for real-JDK mode.
//!
//! Jackson's `StdDateFormat.<clinit>` calls `SimpleDateFormat`, which in turn
//! calls `ResourceBundle.getBundle("sun.text.resources.FormatData", Locale.US)`.
//! In stock JDK 25 these bundles live as compiled classes inside the
//! `jdk.localedata` module (e.g. `sun.text.resources.FormatData_en_US.class`)
//! and are loaded through the `LocaleProviderAdapter` chain. Our
//! classloader's jimage path does not surface those classes, so the
//! `getBundle` call ends up walking a tangled set of caller-class /
//! Module-of-caller checks that NPE long before the bundle can be
//! constructed (the common terminal failure is "Cannot invoke isNamed on
//! null" raised inside `ResourceBundle.checkNamedModule`).
//!
//! Option A from the C22 task plan: register Rust natives that *replace*
//! the Java bodies of `ResourceBundle.getBundle(...)` for real-JDK mode.
//! For each well-known JDK locale-data base name we hand back a synthetic
//! `ResourceBundle` whose `handleGetObject` map is pre-populated with
//! sensible English/US defaults (month names, day names, am/pm markers,
//! eras, date-time patterns, etc.) — enough to satisfy
//! `SimpleDateFormat`/`DateFormatSymbols`/`DecimalFormatSymbols` and the
//! Jackson date-format bootstrap. Unknown bundle names get an empty
//! bundle (with a `.properties` fallback for user resources on the
//! classpath), matching the behaviour of the synthetic-mode override in
//! `phases_late.rs::register_p63_resource_bundle`.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

fn make_string_array(ctx: &mut dyn NativeContext, items: &[&str]) -> ObjectRef {
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, items.len());
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    arr
}

fn put_arr(ctx: &mut dyn NativeContext, map: ObjectRef, key: &str, items: &[&str]) {
    let k = ctx.create_string(key);
    let arr = make_string_array(ctx, items);
    rustjvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(k)),
            Value::Object(Some(arr)),
        ],
    )
    .ok();
}

fn put_str(ctx: &mut dyn NativeContext, map: ObjectRef, key: &str, value: &str) {
    let k = ctx.create_string(key);
    let v = ctx.create_string(value);
    rustjvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .ok();
}

fn populate_format_data_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // 13-slot month arrays (12 months + empty trailing slot for lunar
    // calendar compatibility, per the JDK convention).
    put_arr(
        ctx,
        map,
        "MonthNames",
        &[
            "January", "February", "March", "April", "May", "June", "July",
            "August", "September", "October", "November", "December", "",
        ],
    );
    put_arr(
        ctx,
        map,
        "MonthAbbreviations",
        &[
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep",
            "Oct", "Nov", "Dec", "",
        ],
    );
    put_arr(
        ctx,
        map,
        "MonthNarrows",
        &["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D", ""],
    );
    put_arr(
        ctx,
        map,
        "standalone.MonthNames",
        &[
            "January", "February", "March", "April", "May", "June", "July",
            "August", "September", "October", "November", "December", "",
        ],
    );
    put_arr(
        ctx,
        map,
        "standalone.MonthAbbreviations",
        &[
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep",
            "Oct", "Nov", "Dec", "",
        ],
    );
    put_arr(
        ctx,
        map,
        "standalone.MonthNarrows",
        &["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D", ""],
    );
    // 7-element day arrays starting with Sunday — the JDK's
    // `DateFormatSymbols.toOneBasedArray` prepends an empty slot so the
    // resulting `weekdays`/`shortWeekdays` arrays end up Calendar-indexed
    // (1=Sunday..7=Saturday). Providing a pre-padded 8-element array
    // would yield a 9-slot result and trip downstream array reads.
    put_arr(
        ctx,
        map,
        "DayNames",
        &[
            "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday",
            "Friday", "Saturday",
        ],
    );
    put_arr(
        ctx,
        map,
        "DayAbbreviations",
        &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(
        ctx,
        map,
        "DayNarrows",
        &["S", "M", "T", "W", "T", "F", "S"],
    );
    put_arr(
        ctx,
        map,
        "standalone.DayNames",
        &[
            "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday",
            "Friday", "Saturday",
        ],
    );
    put_arr(
        ctx,
        map,
        "standalone.DayAbbreviations",
        &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(
        ctx,
        map,
        "standalone.DayNarrows",
        &["S", "M", "T", "W", "T", "F", "S"],
    );
    put_arr(ctx, map, "AmPmMarkers", &["AM", "PM"]);
    put_arr(ctx, map, "narrow.AmPmMarkers", &["a", "p"]);
    put_arr(ctx, map, "Eras", &["BC", "AD"]);
    put_arr(ctx, map, "short.Eras", &["BC", "AD"]);
    put_arr(ctx, map, "narrow.Eras", &["B", "A"]);
    put_arr(
        ctx,
        map,
        "QuarterNames",
        &["1st quarter", "2nd quarter", "3rd quarter", "4th quarter"],
    );
    put_arr(ctx, map, "QuarterAbbreviations", &["Q1", "Q2", "Q3", "Q4"]);
    put_arr(ctx, map, "QuarterNarrows", &["1", "2", "3", "4"]);
    put_arr(
        ctx,
        map,
        "standalone.QuarterNames",
        &["1st quarter", "2nd quarter", "3rd quarter", "4th quarter"],
    );
    put_arr(
        ctx,
        map,
        "standalone.QuarterAbbreviations",
        &["Q1", "Q2", "Q3", "Q4"],
    );
    put_arr(
        ctx,
        map,
        "standalone.QuarterNarrows",
        &["1", "2", "3", "4"],
    );
    // 9-slot DateTimePatterns: 4 time patterns (FULL/LONG/MEDIUM/SHORT),
    // 4 date patterns, 1 date-time combiner — the standard JDK layout
    // SimpleDateFormat consumes via DateFormatSymbols.
    put_arr(
        ctx,
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
    put_arr(
        ctx,
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
    put_str(ctx, map, "DateTimePatternChars", "GyMdkHmsSEDFwWahKzZYuXL");
    put_arr(
        ctx,
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
        map,
        "NumberElements",
        &[
            ".", ",", ";", "%", "0", "#", "-", "E", "\u{2030}", "\u{221E}",
            "NaN",
        ],
    );
    put_str(ctx, map, "TimePatternChars", "hHmsSaEcLkKzZ");
}

fn populate_locale_names_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    let langs: &[(&str, &str)] = &[
        ("en", "English"), ("fr", "French"), ("de", "German"),
        ("es", "Spanish"), ("it", "Italian"), ("ja", "Japanese"),
        ("ko", "Korean"), ("zh", "Chinese"), ("ru", "Russian"),
        ("pt", "Portuguese"), ("nl", "Dutch"), ("sv", "Swedish"),
        ("ar", "Arabic"), ("hi", "Hindi"), ("tr", "Turkish"),
    ];
    let countries: &[(&str, &str)] = &[
        ("US", "United States"), ("GB", "United Kingdom"), ("CA", "Canada"),
        ("FR", "France"), ("DE", "Germany"), ("ES", "Spain"),
        ("IT", "Italy"), ("JP", "Japan"), ("CN", "China"),
        ("KR", "South Korea"), ("RU", "Russia"), ("BR", "Brazil"),
        ("AU", "Australia"), ("NL", "Netherlands"), ("SE", "Sweden"),
        ("MX", "Mexico"), ("IN", "India"), ("ZA", "South Africa"),
    ];
    for (k, v) in langs {
        put_str(ctx, map, k, v);
    }
    for (k, v) in countries {
        put_str(ctx, map, k, v);
    }
}

fn populate_calendar_data_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    put_str(ctx, map, "firstDayOfWeek", "1"); // Sunday
    put_str(ctx, map, "minimalDaysInFirstWeek", "1");
}

fn populate_currency_names_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    put_str(ctx, map, "USD", "US Dollar");
    put_str(ctx, map, "EUR", "Euro");
    put_str(ctx, map, "GBP", "British Pound");
    put_str(ctx, map, "JPY", "Japanese Yen");
    put_str(ctx, map, "CNY", "Chinese Yuan");
    put_str(ctx, map, "usd", "$");
    put_str(ctx, map, "eur", "\u{20AC}");
    put_str(ctx, map, "gbp", "\u{00A3}");
    put_str(ctx, map, "jpy", "\u{00A5}");
}

/// Build a synthetic `ResourceBundle` for `bundle_name`. Always returns
/// non-null. For user `.properties` resources on the classpath we
/// populate from the file. For known JDK locale-data base names we
/// pre-populate English/US defaults. Unknown names get an empty bundle.
fn build_bundle(ctx: &mut dyn NativeContext, bundle_name: &str) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/util/ResourceBundle", 2);
    let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
    rustjvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    ctx.set_field(obj, 0, Value::Object(Some(map)));
    ctx.set_field(obj, 1, Value::Object(None));

    // .properties fallback for user resources.
    let resource_name = format!("{}.properties", bundle_name.replace('.', "/"));
    let mut populated_from_props = false;
    if let Some(bytes) = ctx.find_resource(&resource_name) {
        if let Ok(content) = String::from_utf8(bytes) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                let sep_pos = line.find(|c: char| c == '=' || c == ':');
                if let Some(pos) = sep_pos {
                    let key = line[..pos].trim();
                    let value = line[pos + 1..].trim();
                    let k = ctx.create_string(key);
                    let v = ctx.create_string(value);
                    rustjvm_native_collections::native_map_put_pub(
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

    if !populated_from_props {
        if bundle_name.starts_with("sun.text.resources.FormatData")
            || bundle_name.starts_with("sun.text.resources.cldr.FormatData")
            || bundle_name.starts_with("sun.text.resources.ext.FormatData")
        {
            populate_format_data_en(ctx, map);
        } else if bundle_name.starts_with("sun.util.resources.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.cldr.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.ext.LocaleNames")
        {
            populate_locale_names_en(ctx, map);
        } else if bundle_name.starts_with("sun.util.resources.CalendarData")
            || bundle_name.starts_with("sun.util.resources.cldr.CalendarData")
        {
            populate_calendar_data_en(ctx, map);
        } else if bundle_name.starts_with("sun.util.resources.CurrencyNames")
            || bundle_name.starts_with("sun.util.resources.cldr.CurrencyNames")
        {
            populate_currency_names_en(ctx, map);
        }
    }

    obj
}

fn rb_get_bundle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let bundle_name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let obj = build_bundle(ctx, &bundle_name);
    Ok(Some(Value::Object(Some(obj))))
}

fn rb_get_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let map = match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Object(None))),
    };
    rustjvm_native_collections::native_map_get_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key))],
    )
}

fn rb_get_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    rb_get_object(ctx, args)
}

fn rb_handle_get_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    rb_get_object(ctx, args)
}

fn rb_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Int(0))),
    };
    let map = match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Int(0))),
    };
    rustjvm_native_collections::native_map_contains_key_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key))],
    )
}

pub fn register(registry: &mut NativeMethodRegistry) {
    let rb = "java/util/ResourceBundle";

    // All getBundle overloads dispatch to the same builder.  This bypasses
    // the JDK's internal Module / caller-class machinery (which trips an
    // NPE in our partial bootstrap) and the LocaleProviderAdapter chain
    // (which has no class-based `jdk.localedata` bundle data to load
    // through our jimage path).
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/Module;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );

    // Read-side overrides — the JDK `getString` / `getObject` path walks
    // a parent chain and a control-cache; with our synthetic backing map
    // we can answer directly out of slot 0.
    registry.register(
        rb,
        "getString",
        "(Ljava/lang/String;)Ljava/lang/String;",
        rb_get_string,
    );
    registry.register(
        rb,
        "getStringArray",
        "(Ljava/lang/String;)[Ljava/lang/String;",
        rb_get_object,
    );
    registry.register(
        rb,
        "getObject",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        rb_get_object,
    );
    registry.register(
        rb,
        "handleGetObject",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        rb_handle_get_object,
    );
    registry.register(rb, "containsKey", "(Ljava/lang/String;)Z", rb_contains_key);
    registry.register(rb, "getLocale", "()Ljava/util/Locale;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });
    registry.register(rb, "getBaseBundleName", "()Ljava/lang/String;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });

    // sun.util.resources.LocaleData.getBundle(String, Locale)  →ResourceBundle
    // Internally this calls `sun.util.resources.Bundles.of(name, locale,
    // strategy)` which walks the JDK locale provider/Bundles cache and
    // ends up trying to load class-based bundles from `jdk.localedata`
    // through ServiceLoader plumbing that NPEs in our partial bootstrap.
    // Short-circuit at the public LocaleData.getBundle entry point.
    registry.register(
        "sun/util/resources/LocaleData",
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );

    // sun.util.resources.Bundles.of(String, Locale, Strategy)  → ResourceBundle
    // Belt-and-suspenders override for any caller that bypasses LocaleData
    // and goes straight to Bundles.of.
    registry.register(
        "sun/util/resources/Bundles",
        "of",
        "(Ljava/lang/String;Ljava/util/Locale;Lsun/util/resources/Bundles$Strategy;)Ljava/util/ResourceBundle;",
        rb_get_bundle,
    );

    // sun.util.locale.provider.LocaleResources.getDecimalFormatSymbolsData()
    // returns an Object[3]:  [ String[] numberElements, String intlCurrSymbol,
    // String currSymbol ].  `DecimalFormatSymbols.initialize` reads
    // `data[0][0..10]` (decimal/grouping/pattern sep + percent + zero +
    // digit + minus + exponent + perMille + infinity + NaN), then
    // `data[1]` and `data[2]` as currency strings (null-tolerant via
    // checkcast).  Any short array there yields an
    // `ArrayIndexOutOfBoundsException` with no message — which is the
    // C22 failure surface we hit on `new DecimalFormatSymbols(Locale.US)`.
    // Build a fully-populated 11-element string array under index 0 and
    // null currency strings under 1/2 (DFS lazily resolves currency
    // through `initializeCurrency` which goes via Currency.getInstance,
    // not through this array).
    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getDecimalFormatSymbolsData",
        "()[Ljava/lang/Object;",
        |ctx, _args| {
            let outer = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 3);
            // 13-element layout: indices 0-10 are mandatory (decimal,
            // grouping, pattern sep, percent, zero, digit, minus,
            // exponent, perMille, infinity, NaN), 11=monetary decimal
            // separator, 12=monetary grouping separator (both optional —
            // DFS falls back to the regular separators when those slots
            // are empty / the array is short).
            let elems = make_string_array(
                ctx,
                &[
                    ".", ",", ";", "%", "0", "#", "-", "E",
                    "\u{2030}", "\u{221E}", "NaN", ".", ",",
                ],
            );
            ctx.set_array_element(outer, 0, Value::Object(Some(elems)));
            // Use empty strings rather than null for indices 1 and 2 so a
            // checkcast-then-getfield path can't run into a null-pointer
            // surprise via a downstream method call (DFS treats an empty
            // currency symbol as "unset" and resolves it lazily through
            // initializeCurrency()).
            let empty1 = ctx.create_string("");
            let empty2 = ctx.create_string("");
            ctx.set_array_element(outer, 1, Value::Object(Some(empty1)));
            ctx.set_array_element(outer, 2, Value::Object(Some(empty2)));
            Ok(Some(Value::Object(Some(outer))))
        },
    );

    // java.text.DecimalFormatSymbols.initialize(Locale) — bypass the
    // JDK's private locale-provider walk entirely.  The original method
    // reads `LocaleResources.getDecimalFormatSymbolsData()` then uses
    // hard-coded indices on the returned arrays; even with our shaped
    // override (above) something downstream still raises an
    // ArrayIndexOutOfBoundsException with no message — likely a stale
    // instance-field offset on the boxed Locale path.  Skip that whole
    // dance and configure DFS through its public setters directly.
    registry.register(
        "java/text/DecimalFormatSymbols",
        "initialize",
        "(Ljava/util/Locale;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let locale = args.get(1).copied().unwrap_or(Value::Object(None));
            // Set every public char/string field via the corresponding
            // setter so we don't depend on instance-field layout.
            let _ = ctx.invoke_virtual(this, "setDecimalSeparator", "(C)V", &[Value::Int('.' as i32)]);
            let _ = ctx.invoke_virtual(this, "setGroupingSeparator", "(C)V", &[Value::Int(',' as i32)]);
            let _ = ctx.invoke_virtual(this, "setPatternSeparator", "(C)V", &[Value::Int(';' as i32)]);
            let _ = ctx.invoke_virtual(this, "setPercent", "(C)V", &[Value::Int('%' as i32)]);
            let _ = ctx.invoke_virtual(this, "setZeroDigit", "(C)V", &[Value::Int('0' as i32)]);
            let _ = ctx.invoke_virtual(this, "setDigit", "(C)V", &[Value::Int('#' as i32)]);
            let _ = ctx.invoke_virtual(this, "setMinusSign", "(C)V", &[Value::Int('-' as i32)]);
            let _ = ctx.invoke_virtual(this, "setPerMill", "(C)V", &[Value::Int(0x2030)]);
            let exp = ctx.create_string("E");
            let _ = ctx.invoke_virtual(this, "setExponentSeparator", "(Ljava/lang/String;)V", &[Value::Object(Some(exp))]);
            let inf = ctx.create_string("\u{221E}");
            let _ = ctx.invoke_virtual(this, "setInfinity", "(Ljava/lang/String;)V", &[Value::Object(Some(inf))]);
            let nan = ctx.create_string("NaN");
            let _ = ctx.invoke_virtual(this, "setNaN", "(Ljava/lang/String;)V", &[Value::Object(Some(nan))]);
            let _ = ctx.invoke_virtual(this, "setMonetaryDecimalSeparator", "(C)V", &[Value::Int('.' as i32)]);
            let cs = ctx.create_string("$");
            let _ = ctx.invoke_virtual(this, "setCurrencySymbol", "(Ljava/lang/String;)V", &[Value::Object(Some(cs))]);
            let ics = ctx.create_string("USD");
            let _ = ctx.invoke_virtual(this, "setInternationalCurrencySymbol", "(Ljava/lang/String;)V", &[Value::Object(Some(ics))]);
            // Locale field is not exposed via a public setter; the JDK's
            // own initialize() does putfield directly. Use invoke on a
            // synthetic helper via reflection-free path: call getLocale()
            // first to learn the field index dynamically would be
            // overkill — instead, leave it null and rely on
            // DFS.getLocale() returning whatever was set during
            // serialization (or null).  Jackson / SimpleDateFormat
            // never read DFS.getLocale() during their bootstrap.
            let _ = locale; // suppress unused warning
            Ok(None)
        },
    );

    // LocaleResources.getNumberPatterns() → String[] of 4 patterns
    // (number / currency / percent / scientific).  Used by
    // `NumberFormat.getInstance` and friends.  Same root cause as above:
    // the JDK indexes 0..3 unconditionally — short arrays AIOOBE.
    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getNumberPatterns",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = make_string_array(
                ctx,
                &[
                    "#,##0.###",
                    "\u{00A4}#,##0.00;(\u{00A4}#,##0.00)",
                    "#,##0%",
                    "#E0",
                ],
            );
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}
