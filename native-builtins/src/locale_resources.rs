// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
use cratonvm_types::error::MethodCallFailed;

// cceres5 (WildFly metrics `getResourceDescription` stale-ResourceBundle,
// live-captured 2026-07-22 via CRATONVM_DBG_STALE_RECV): every helper below
// interleaves `create_string`/array allocations with stores into `map`/`arr`
// while holding the raw refs — a moving GC mid-sequence baked pre-move
// addresses into the freshly built bundle (and `rb_get_bundle` then RETURNED
// the pre-move bundle itself). Pin + re-read throughout, per the Family-1
// idiom.
fn make_string_array(ctx: &mut dyn NativeContext, items: &[&str]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, items.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        let arr_now = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr_now, i, Value::Object(Some(js)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    arr
}

// `map_pin` is the CALLER's pin for `map` (taken before its first
// allocation); reading through it here yields the current address even after
// the internal string/array allocations of earlier calls in the caller's
// sequence.
fn put_arr(
    ctx: &mut dyn NativeContext,
    map_pin: usize,
    map: ObjectRef,
    key: &str,
    items: &[&str],
) {
    let k = ctx.create_string(key);
    let k_pin = ctx.pin_native_root(k);
    let arr = make_string_array(ctx, items);
    let map_now = ctx.read_native_pin(map_pin, map);
    let k_now = ctx.read_native_pin(k_pin, k);
    cratonvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map_now)),
            Value::Object(Some(k_now)),
            Value::Object(Some(arr)),
        ],
    )
    .ok();
    ctx.unpin_native_roots(k_pin);
}

fn put_str(ctx: &mut dyn NativeContext, map_pin: usize, map: ObjectRef, key: &str, value: &str) {
    let k = ctx.create_string(key);
    let k_pin = ctx.pin_native_root(k);
    let v = ctx.create_string(value);
    let map_now = ctx.read_native_pin(map_pin, map);
    let k_now = ctx.read_native_pin(k_pin, k);
    cratonvm_native_collections::native_map_put_pub(
        ctx,
        &[
            Value::Object(Some(map_now)),
            Value::Object(Some(k_now)),
            Value::Object(Some(v)),
        ],
    )
    .ok();
    ctx.unpin_native_roots(k_pin);
}

fn populate_format_data_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // cceres5: every put below allocates; one entry pin, read per call.
    let map_pin = ctx.pin_native_root(map);
    // 13-slot month arrays (12 months + empty trailing slot for lunar
    // calendar compatibility, per the JDK convention).
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
    // 7-element day arrays starting with Sunday — the JDK's
    // `DateFormatSymbols.toOneBasedArray` prepends an empty slot so the
    // resulting `weekdays`/`shortWeekdays` arrays end up Calendar-indexed
    // (1=Sunday..7=Saturday). Providing a pre-padded 8-element array
    // would yield a 9-slot result and trip downstream array reads.
    put_arr(
        ctx,
        map_pin,
        map,
        "DayNames",
        &[
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
        &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(ctx, map_pin, map, "DayNarrows", &["S", "M", "T", "W", "T", "F", "S"]);
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.DayNames",
        &[
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
        &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "standalone.DayNarrows",
        &["S", "M", "T", "W", "T", "F", "S"],
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
    put_arr(ctx, map_pin, map, "QuarterAbbreviations", &["Q1", "Q2", "Q3", "Q4"]);
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
    put_arr(ctx, map_pin, map, "standalone.QuarterNarrows", &["1", "2", "3", "4"]);
    // 9-slot DateTimePatterns: 4 time patterns (FULL/LONG/MEDIUM/SHORT),
    // 4 date patterns, 1 date-time combiner — the standard JDK layout
    // SimpleDateFormat consumes via DateFormatSymbols.
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
    put_str(ctx, map_pin, map, "DateTimePatternChars", "GyMdkHmsSEDFwWahKzZYuXL");
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

fn populate_locale_names_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // cceres5: every put below allocates; one entry pin, read per call.
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

fn populate_calendar_data_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // cceres5: every put below allocates; one entry pin, read per call.
    let map_pin = ctx.pin_native_root(map);
    put_str(ctx, map_pin, map, "firstDayOfWeek", "1"); // Sunday
    put_str(ctx, map_pin, map, "minimalDaysInFirstWeek", "1");
    ctx.unpin_native_roots(map_pin);
}

fn populate_currency_names_en(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // cceres5: every put below allocates; one entry pin, read per call.
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

/// Build a synthetic `ResourceBundle` for `bundle_name`. Always returns
/// non-null. For user `.properties` resources on the classpath we
/// populate from the file. For known JDK locale-data base names we
/// pre-populate English/US defaults. Unknown names get an empty bundle.
fn build_bundle(ctx: &mut dyn NativeContext, bundle_name: &str) -> Result<ObjectRef, MethodCallFailed> {
    // cceres5: pin the bundle + backing map across every allocation below
    // (map init, root-locale alloc, per-property string allocations, the
    // populate tables) and return the pin-refreshed address — the raw `obj`
    // return was one producer of the stale-ResourceBundle family (see
    // rb_get_bundle).
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ResourceBundle", 2)?;
    let obj_pin = ctx.pin_native_root(obj);
    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
    let map_pin = ctx.pin_native_root(map);
    cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    let obj_now = ctx.read_native_pin(obj_pin, obj);
    let map_now = ctx.read_native_pin(map_pin, map);
    ctx.set_field(obj_now, 0, Value::Object(Some(map_now)));
    ctx.set_field(obj_now, 1, Value::Object(None));

    // CRITICAL (Tomcat boot): `java.util.ResourceBundle.getLocale()` is a
    // `final` JDK method whose body is `return this.locale;`. Our synthetic
    // bundle carries the *real* `java/util/ResourceBundle` class, so in
    // real-JDK mode a caller that bypasses the `getLocale` native override
    // (e.g. JIT-compiled callsite, or a resolution path that runs the
    // bytecode `getfield locale` directly) reads slot `locale` — which the
    // two `set_field` calls above leave NULL. Tomcat's
    // `StringManager.<init>` then does `bundle.getLocale().equals(Locale.ROOT)`
    // and NPEs ("Cannot invoke equals on null"), which converts to an
    // `ExceptionInInitializerError` on `org/apache/catalina/startup/Catalina`
    // and aborts Tomcat's `Bootstrap.init`.
    //
    // Fix: resolve the real `locale` field by NAME (layout-independent) and
    // store a non-null `Locale.ROOT`-equivalent (empty language/country)
    // there. The `getLocale` native override still returns the same value;
    // this just guarantees correctness when the override is bypassed.
    let root_locale = crate::locale_alloc(ctx, "", "");
    let obj_now = ctx.read_native_pin(obj_pin, obj);
    if let Some(loc_idx) = ctx.resolve_field_index("java/util/ResourceBundle", "locale") {
        ctx.set_field(obj_now, loc_idx, Value::Object(Some(root_locale?)));
    } else {
        // Synthetic-JDK mode (class not loaded with real layout): slot 1 is
        // the conventional `locale` placement used by the synthetic layout.
        ctx.set_field(obj_now, 1, Value::Object(Some(root_locale?)));
    }

    // Record the base name so `getBaseBundleName()` has something true to
    // report. By name, so it lands on the real JDK's private `name` field when
    // that layout is present and is a harmless no-op when it is not. `obj` is
    // re-read through the pin AFTER `create_string`, which can move it.
    let base_name = ctx.create_string(bundle_name);
    let obj_now = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj_now, "name", Value::Object(Some(base_name)));

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
                    let k_pin = ctx.pin_native_root(k);
                    let v = ctx.create_string(value);
                    let map_now = ctx.read_native_pin(map_pin, map);
                    let k_now = ctx.read_native_pin(k_pin, k);
                    cratonvm_native_collections::native_map_put_pub(
                        ctx,
                        &[
                            Value::Object(Some(map_now)),
                            Value::Object(Some(k_now)),
                            Value::Object(Some(v)),
                        ],
                    )
                    .ok();
                    ctx.unpin_native_roots(k_pin);
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
            let map_now = ctx.read_native_pin(map_pin, map);
            populate_format_data_en(ctx, map_now);
        } else if bundle_name.starts_with("sun.util.resources.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.cldr.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.ext.LocaleNames")
        {
            let map_now = ctx.read_native_pin(map_pin, map);
            populate_locale_names_en(ctx, map_now);
        } else if bundle_name.starts_with("sun.util.resources.CalendarData")
            || bundle_name.starts_with("sun.util.resources.cldr.CalendarData")
        {
            let map_now = ctx.read_native_pin(map_pin, map);
            populate_calendar_data_en(ctx, map_now);
        } else if bundle_name.starts_with("sun.util.resources.CurrencyNames")
            || bundle_name.starts_with("sun.util.resources.cldr.CurrencyNames")
        {
            let map_now = ctx.read_native_pin(map_pin, map);
            populate_currency_names_en(ctx, map_now);
        }
    }

    let obj_now = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj_now)
}

/// Decode `java.util.Properties` escapes in a key or value: `\uXXXX`,
/// `\t`/`\n`/`\r`/`\f`, and `\<char>` (`\\`, `\=`, `\:`, `\ `, `\#`, …). Needed
/// so e.g. the Spanish `Número` decodes to `Número` (one char, not six) —
/// otherwise downstream width/alignment math is wrong.
fn unescape_props(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('u') => {
                let hex: String = (0..4).filter_map(|_| it.next()).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{0C}'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Parse a `.properties` byte slice and put each `key=value` into `map`,
/// overwriting earlier entries (so a more-specific locale variant wins).
/// Handles trailing-backslash line continuation and `\u`/escape decoding;
/// the value keeps its significant internal/trailing whitespace (only leading
/// whitespace after the separator is stripped).
fn parse_props_into_map(ctx: &mut dyn NativeContext, map: ObjectRef, bytes: &[u8]) {
    let content = match std::str::from_utf8(bytes) {
        Ok(c) => c,
        Err(_) => return,
    };
    // cceres5: one entry pin for the whole parse — each line's two
    // `create_string` calls can move `map`, and pinning the raw param
    // per-line would just pin an already-stale address on later lines.
    let entry_pin = ctx.pin_native_root(map);
    let mut pending = String::new();
    let mut continuing = false;
    for raw in content.lines() {
        // Leading whitespace of every physical line is insignificant.
        let seg = raw.trim_start();
        if !continuing && (seg.is_empty() || seg.starts_with('#') || seg.starts_with('!')) {
            continue;
        }
        // A line continues when it ends with an odd number of backslashes.
        let trailing_bs = seg.chars().rev().take_while(|&c| c == '\\').count();
        if trailing_bs % 2 == 1 {
            pending.push_str(&seg[..seg.len() - 1]);
            continuing = true;
            continue;
        }
        pending.push_str(seg);
        continuing = false;

        // Separator: first unescaped '=' or ':'; else first unescaped whitespace.
        let bytes_p = pending.as_bytes();
        let mut sep: Option<usize> = None;
        let mut esc = false;
        for (i, &b) in bytes_p.iter().enumerate() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' => esc = true,
                b'=' | b':' => {
                    sep = Some(i);
                    break;
                }
                b' ' | b'\t' | 0x0C if sep.is_none() => sep = Some(i),
                _ => {}
            }
        }
        if let Some(pos) = sep {
            let key = unescape_props(pending[..pos].trim_end());
            let val = unescape_props(pending[pos + 1..].trim_start());
            let k = ctx.create_string(&key);
            let k_pin = ctx.pin_native_root(k);
            let v = ctx.create_string(&val);
            let map_now = ctx.read_native_pin(entry_pin, map);
            let k_now = ctx.read_native_pin(k_pin, k);
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[
                    Value::Object(Some(map_now)),
                    Value::Object(Some(k_now)),
                    Value::Object(Some(v)),
                ],
            )
            .ok();
            ctx.unpin_native_roots(k_pin);
        }
        pending.clear();
    }
    ctx.unpin_native_roots(entry_pin);
}

/// `true` for JDK-internal bundle base names that CratonVM synthesizes (locale
/// data) or must not fail on during partial bootstrap — these keep the
/// empty-bundle fallback. App bundles that are genuinely absent throw
/// MissingResourceException instead (the spec-correct behaviour).
fn is_jdk_internal_bundle(name: &str) -> bool {
    name.starts_with("sun.")
        || name.starts_with("jdk.")
        || name.starts_with("com.sun.")
        || name.starts_with("java.")
        || name.starts_with("javax.")
}

/// `true` for the locale-data base-name families that `build_bundle`
/// hand-synthesizes (FormatData / LocaleNames / CalendarData / CurrencyNames).
/// Those live in the `jdk.localedata` module, which our partial-bootstrap
/// class path does not reliably surface, so we keep the curated English/US
/// fallback rather than instantiating the real (potentially absent) class
/// bundle. Every OTHER base name is eligible for the real class-based
/// `ListResourceBundle` path below.
fn is_synthesized_locale_base(name: &str) -> bool {
    name.starts_with("sun.text.resources.") || name.starts_with("sun.util.resources.")
}

/// `true` for the `sun.util.resources.*` bundle family whose *caller* in
/// `sun.util.resources.LocaleData` immediately `checkcast`s the result to a
/// concrete JDK bundle type — so a bare synthetic `java/util/ResourceBundle`
/// is rejected with a `ClassCastException`. From `javap -c LocaleData`,
/// `getTimeZoneNames` does `checkcast sun/util/resources/TimeZoneNamesBundle`.
///
/// `TimeZoneNamesBundle` is abstract, but the jimage ships concrete leaf data
/// classes (`sun.util.resources.cldr.TimeZoneNames`/`_en`, plus the non-CLDR
/// `sun.util.resources.TimeZoneNames` root) that subclass it. So this base name
/// MUST take the real class-based bundle path (`try_class_bundle`) — which
/// instantiates the concrete leaf class — rather than the curated synthetic
/// `build_bundle` that returns a raw `java/util/ResourceBundle`. The root data
/// class always exists, so the cast always finds a real instance. Surfaced by
/// Tomcat `TestExpiresFilter`, whose HTTP `Expires`/`Date` header formatting
/// resolves time-zone display names via `TimeZoneNameUtility` →
/// `LocaleData.getTimeZoneNames`.
///
/// `getCurrencyNames` / `getLocaleNames` `checkcast` to
/// `sun/util/resources/OpenListResourceBundle` and share the same latent
/// pattern, but are deliberately LEFT on the curated `build_bundle` path: the
/// real CLDR display names for those live in the `sun.util.resources.cldr.ext.*`
/// package that the simple locale-candidate chain below does not reach, so
/// routing them to `try_class_bundle` would hand back a partial bundle without
/// changing their (already code-only) display-name output — and nothing in the
/// failing test exercises their cast. Restricting to `TimeZoneNames` keeps this
/// fix's blast radius to the one bundle the reported bug needs.
///
/// `FormatData` / `CalendarData` are likewise NOT included: their `LocaleData`
/// accessors return a plain `ResourceBundle` (no `checkcast`), and
/// `DateFormatSymbols`/`DecimalFormatSymbols` consume them through the curated
/// English/US map populated by `build_bundle`.
fn needs_concrete_bundle_class(name: &str) -> bool {
    name.ends_with(".TimeZoneNames")
}

/// Real-JDK "java.class" bundle format: the bundle is a compiled
/// `ListResourceBundle` subclass rather than a `.properties` file. The JDK
/// ships javac/launcher diagnostics (and some app resources) this way — e.g.
/// `com.sun.tools.javac.resources.compiler` is a generated `ListResourceBundle`
/// whose `getContents()` returns the message table; there is NO
/// `compiler.properties` at runtime. Our `getBundle` override only knew how to
/// load `.properties`, so it handed javac an empty bundle and every diagnostic
/// degraded to "compiler message file broken: key=…".
///
/// Walk the candidate chain (ROOT/least-specific first, mirroring the
/// `.properties` merge order), instantiate every candidate class that exists,
/// link the less-specific ones as the parent chain, and return the
/// most-specific bundle. The read-side natives (`rb_get_object`) resolve keys
/// through the overridden `getContents()` and walk that parent chain on a miss.
/// Returns `None` when no candidate class is on the class path.
fn try_class_bundle(
    ctx: &mut dyn NativeContext,
    chain: &[(String, String, String)],
) -> Option<ObjectRef> {
    // Candidate class internal names (dots → slashes) whose `.class` resource
    // exists, least-specific first.
    let existing: Vec<String> = chain
        .iter()
        .map(|(cand, _, _)| cand.replace('.', "/"))
        .filter(|p| ctx.find_resource(&format!("{p}.class")).is_some())
        .collect();
    if existing.is_empty() {
        return None;
    }

    // Instantiate least-specific → most-specific and link parents. Every live
    // bundle is pinned across the subsequent `new_object_initialized`
    // (instantiating the generated `getContents()` array can trigger a moving
    // GC that relocates earlier bundles); `first_pin` anchors the batch for a
    // single release at the end.
    let mut first_pin: Option<usize> = None;
    let mut parent_pin: Option<usize> = None;
    let mut most_specific: Option<ObjectRef> = None;
    for cname in &existing {
        let cur = match ctx.new_object_initialized(cname, "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => continue,
        };
        let cur_pin = ctx.pin_native_root(cur);
        if first_pin.is_none() {
            first_pin = Some(cur_pin);
        }
        if let Some(pp) = parent_pin {
            // Re-read both refs through their pins before the re-entrant call.
            let parent = ctx.read_native_pin(pp, cur);
            let cur_now = ctx.read_native_pin(cur_pin, cur);
            ctx.invoke_virtual(
                cur_now,
                "setParent",
                "(Ljava/util/ResourceBundle;)V",
                &[Value::Object(Some(parent))],
            )
            .ok();
        }
        parent_pin = Some(cur_pin);
        most_specific = Some(cur);
    }

    let result = match (parent_pin, most_specific) {
        (Some(pp), Some(ms)) => Some(ctx.read_native_pin(pp, ms)),
        _ => None,
    };
    if let Some(base) = first_pin {
        ctx.unpin_native_roots(base);
    }
    result
}

/// Return the explicit `ClassLoader` passed to a `ResourceBundle.getBundle`
/// overload, when there is one.  It is important not to replace this with the
/// VM-wide class path: callers such as Spring's test-resource extension use a
/// short-lived thread-context loader whose resources deliberately do not live
/// on that class path.
fn bundle_class_loader(ctx: &dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    let class_loader = ctx.class_id_by_name("java/lang/ClassLoader")?;
    args.iter().skip(1).find_map(|arg| match arg {
        Value::Object(Some(obj)) if ctx.is_subclass(ctx.class_id_of_object(*obj), class_loader) => {
            Some(*obj)
        }
        _ => None,
    })
}

/// The `ClassLoader` of the class that CALLED `ResourceBundle.getBundle`.
///
/// `getBundle(String)` and `getBundle(String, Locale)` are `@CallerSensitive`:
/// the JDK resolves the search loader from the caller's own module
/// (`getBundleImpl` -> `getLoader(caller.getModule())`), NOT from the
/// application class path. Our natives replace every `getBundle` overload, and
/// the no-loader ones fell back to CratonVM's process-wide `-cp` scan -- so a
/// class defined by a custom loader could not find a bundle that only ITS
/// loader can serve. Keycloak 26.6.1: Liquibase's
/// `ResourceBundle.getBundle("liquibase/i18n/liquibase-core")` runs from
/// classes defined by Quarkus's `RunnerClassLoader`, whose jars are not on the
/// CratonVM process class path at all, so the boot died with
/// `MissingResourceException` where HotSpot resolves the bundle fine.
///
/// Only USER-DEFINED loaders are reported: for the built-in loaders the `-cp`
/// scan is the established, well-tested path and answers the same thing.
/// Because our native IS the `getBundle` frame (no Java frame is pushed for
/// it), the innermost captured Java frame is the caller.
fn caller_bundle_class_loader(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let frames = ctx.capture_stack_trace(0);
    let cid = frames.last()?.class_id?;
    let mirror = ctx.get_class_mirror(cid);
    let loader = match crate::lang_class::native_class_get_class_loader(
        ctx,
        &[Value::Object(Some(mirror))],
    ) {
        Ok(Some(Value::Object(Some(loader)))) => loader,
        _ => return None,
    };
    // Anything other than the application-loader singleton. NOT
    // `is_user_defined_loader`: that predicate excludes a bare
    // `java.net.URLClassLoader` BY CLASS NAME for its other call sites, yet
    // `new URLClassLoader(urls, parent)` is the most ordinary way an
    // application builds an isolated loader, and HotSpot resolves a bundle
    // through it. Classes owned by the built-in loaders report `null` here or
    // the app singleton, and both keep the established `-cp` path.
    let app = crate::classloader::get_or_create_app_loader(ctx);
    if loader.as_ptr() == app.as_ptr() {
        return None;
    }
    Some(loader)
}

/// Locate a `.properties` candidate through the loader supplied to
/// `ResourceBundle.getBundle`, falling back to the application class path only
/// for overloads that supplied no loader.  `ClassLoader.getResourceAsStream`
/// already preserves parent-first delegation and user-defined loader overrides
/// (including Spring Boot's `ResourcesClassLoader`).
fn find_bundle_resource(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
    path: &str,
) -> Option<Vec<u8>> {
    let Some(loader) = loader else {
        return ctx.find_resource(path);
    };
    let loader_pin = ctx.pin_native_root(loader);
    let name = Value::Object(Some(ctx.create_string(path)));
    let loader = ctx.read_native_pin(loader_pin, loader);
    let stream = match ctx.invoke_virtual(
        loader,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        &[name],
    ) {
        Ok(Some(Value::Object(Some(stream)))) => stream,
        _ => {
            ctx.unpin_native_roots(loader_pin);
            // A loader that cannot serve the candidate must not LOSE one the
            // process class path can: every no-loader overload searched `-cp`
            // before the caller-sensitive loader was wired in above, so this
            // keeps that reach rather than narrowing it.
            return ctx.find_resource(path);
        }
    };
    ctx.unpin_native_roots(loader_pin);

    let stream_pin = ctx.pin_native_root(stream);
    let stream = ctx.read_native_pin(stream_pin, stream);
    let bytes = match ctx.invoke_virtual(stream, "readAllBytes", "()[B", &[]) {
        Ok(Some(Value::Object(Some(bytes)))) => bytes,
        _ => {
            ctx.unpin_native_roots(stream_pin);
            return None;
        }
    };
    ctx.unpin_native_roots(stream_pin);
    let mut out = vec![0; ctx.array_length(bytes)];
    let copied = ctx.read_byte_array_into(bytes, 0, &mut out);
    out.truncate(copied);
    Some(out)
}

/// Read a `Locale`'s language/country/variant via its public accessors, so it
/// works for both our synthetic Locales and the JDK's predefined constants
/// (`Locale.FRENCH`, …) whose codes live in `BaseLocale`, not the synthetic
/// side table.
fn decompose_locale(ctx: &mut dyn NativeContext, loc: ObjectRef) -> (String, String, String) {
    let lang = match ctx.invoke_virtual(loc, "getLanguage", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let country = match ctx.invoke_virtual(loc, "getCountry", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let variant = match ctx.invoke_virtual(loc, "getVariant", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    (lang, country, variant)
}

/// Candidate chain, LEAST specific (ROOT) first, merged in order so a
/// more-specific locale variant overrides — and inherited keys present only
/// in a parent bundle still resolve (the JDK parent-chain fallback, flattened
/// into one map). The most-specific variant that actually exists tags the
/// bundle's locale (so getLocale() reports it).
fn build_locale_chain(
    bundle_name: &str,
    lang: &str,
    country: &str,
    variant: &str,
) -> Vec<(String, String, String)> {
    let mut chain: Vec<(String, String, String)> = Vec::new();
    chain.push((bundle_name.to_string(), String::new(), String::new()));
    if !lang.is_empty() {
        chain.push((
            format!("{bundle_name}_{lang}"),
            lang.to_string(),
            String::new(),
        ));
    }
    if !lang.is_empty() && !country.is_empty() {
        chain.push((
            format!("{bundle_name}_{lang}_{country}"),
            lang.to_string(),
            country.to_string(),
        ));
    }
    // Locale variant (e.g. `en_GB_GLASGOW` -> `..._en_GB_GLASGOW.properties`).
    // The JDK's candidate chain includes the variant as its most-specific
    // entry; without it Spring's `ResourceBundleEditor` (which round-trips
    // `name_en_GB_GLASGOW` through `parseLocaleString`) misses the bundle.
    if !lang.is_empty() && !country.is_empty() && !variant.is_empty() {
        chain.push((
            format!("{bundle_name}_{lang}_{country}_{variant}"),
            lang.to_string(),
            country.to_string(),
        ));
    }
    chain
}

/// Determine the JDK fallback locale for a `ResourceBundle.getBundle` call
/// whose requested-locale candidate chain didn't resolve anything more
/// specific than the root bundle. Mirrors
/// `ResourceBundle.Control.getFallbackLocale`: consult a caller-supplied
/// `Control` argument (if any) via virtual dispatch so subclass overrides
/// (e.g. Spring's `ResourceBundleMessageSource.MessageSourceControl`, which
/// returns `null` when `fallbackToSystemLocale=false`) are honored; otherwise
/// replicate the JDK default `Control`'s behavior of falling back to
/// `Locale.getDefault()` when it differs from the requested locale.
fn resolve_fallback_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    bundle_name: &str,
    requested_locale_obj: Option<ObjectRef>,
    lang: &str,
    country: &str,
    variant: &str,
) -> Option<(String, String, String)> {
    let control_obj = ctx
        .class_id_by_name("java/util/ResourceBundle$Control")
        .and_then(|control_cid| {
            args.iter().skip(1).find_map(|a| match a {
                Value::Object(Some(o))
                    if ctx.is_subclass(ctx.class_id_of_object(*o), control_cid) =>
                {
                    Some(*o)
                }
                _ => None,
            })
        });

    if let Some(control) = control_obj {
        // Need a Locale object to pass to getFallbackLocale; use the one that
        // was actually requested, or Locale.getDefault() when the overload
        // took no explicit Locale (e.g. `getBundle(String, Control)`).
        let locale_obj = match requested_locale_obj {
            Some(o) => o,
            None => match ctx.invoke(
                "java/util/Locale",
                "getDefault",
                "()Ljava/util/Locale;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            },
        };
        let name_obj = ctx.create_string(bundle_name);
        let fallback = match ctx.invoke_virtual(
            control,
            "getFallbackLocale",
            "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/Locale;",
            &[
                Value::Object(Some(name_obj)),
                Value::Object(Some(locale_obj)),
            ],
        ) {
            Ok(Some(Value::Object(Some(loc)))) => loc,
            _ => return None,
        };
        return Some(decompose_locale(ctx, fallback));
    }

    // No Control argument: standard JDK default-Control behavior — fall back
    // to Locale.getDefault() when it differs from the requested locale.
    let default_obj = match ctx.invoke(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let (d_lang, d_country, d_variant) = decompose_locale(ctx, default_obj);
    if d_lang == lang && d_country == country && d_variant == variant {
        None
    } else {
        Some((d_lang, d_country, d_variant))
    }
}

fn rb_get_bundle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let bundle_name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if crate::nbflags().dbg_catalina {
        eprintln!("CATALINA-DBG: ResourceBundle.getBundle native — name={bundle_name:?}");
    }

    // Resolve the requested locale from a Locale argument (getBundle(String,
    // Locale[, ClassLoader|Control])). Other shapes (or a Control in slot 1)
    // fall back to the ROOT chain. Keep the Locale ObjectRef too (not just its
    // decomposed strings) — `resolve_fallback_locale` below needs it to
    // consult a caller-supplied Control's `getFallbackLocale` override.
    let mut requested_locale_obj: Option<ObjectRef> = None;
    let (lang, country, variant) = match args.get(1) {
        Some(Value::Object(Some(loc))) => {
            let cid = ctx.class_id_of_object(*loc);
            if ctx.class_name_of_id(cid).as_deref() == Some("java/util/Locale") {
                requested_locale_obj = Some(*loc);
                // Read via getLanguage()/getCountry()/getVariant() so it works
                // for both our synthetic Locales and the JDK's predefined
                // constants (Locale.FRENCH, …) whose codes live in BaseLocale,
                // not the synthetic side table.
                decompose_locale(ctx, *loc)
            } else {
                (String::new(), String::new(), String::new())
            }
        }
        _ => (String::new(), String::new(), String::new()),
    };

    let mut chain = build_locale_chain(&bundle_name, &lang, &country, &variant);
    // An explicit `ClassLoader` argument wins; otherwise honour the JDK's
    // caller-sensitive resolution -- see `caller_bundle_class_loader`.
    let loader = bundle_class_loader(ctx, args).or_else(|| caller_bundle_class_loader(ctx));

    // cceres5 (WildFly metrics stale-ResourceBundle, live-captured via
    // CRATONVM_DBG_STALE_RECV): `obj`/`map`/`loader` were carried raw across
    // map-init, every per-candidate parse (hundreds of string allocations),
    // and `locale_alloc` — and the PRE-MOVE `obj` was then returned to Java.
    // Pin all three; read through the pins at every later use.
    let loader_pin = loader.map(|l| ctx.pin_native_root(l));
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/ResourceBundle", 2)?;
    let obj_pin = ctx.pin_native_root(obj);
    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
    let map_pin = ctx.pin_native_root(map);
    cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    let obj_now = ctx.read_native_pin(obj_pin, obj);
    let map_now = ctx.read_native_pin(map_pin, map);
    ctx.set_field(obj_now, 0, Value::Object(Some(map_now)));
    ctx.set_field(obj_now, 1, Value::Object(None));

    // Base name for `getBaseBundleName()` — see the matching write in
    // `build_bundle`. Re-read `obj` through its pin after `create_string`.
    let base_name = ctx.create_string(&bundle_name);
    let obj_now = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj_now, "name", Value::Object(Some(base_name)));

    let mut matched: Option<(String, String)> = None;
    for (cand, m_lang, m_country) in &chain {
        let path = format!("{}.properties", cand.replace('.', "/"));
        let loader_now = loader_pin.map(|p| ctx.read_native_pin(p, loader.unwrap()));
        if let Some(bytes) = find_bundle_resource(ctx, loader_now, &path) {
            let map_now = ctx.read_native_pin(map_pin, map);
            parse_props_into_map(ctx, map_now, &bytes);
            matched = Some((m_lang.clone(), m_country.clone()));
        }
    }

    // JDK fallback-locale semantics: when the requested locale's own
    // candidate chain doesn't resolve anything more specific than the root
    // bundle, the JDK retries with the `Control`'s fallback locale — the
    // caller-supplied Control's override if one was passed (e.g. Spring's
    // `ResourceBundleMessageSource`, which disables this via
    // `fallbackToSystemLocale=false`), or `Locale.getDefault()` for the
    // standard no-Control overloads. Verified against real JDK 25: requesting
    // "en" with no `messages_en.properties` on the classpath but a
    // `messages_de.properties` present resolves to the "de" bundle when the
    // JVM default locale is German — but a requested locale with its own
    // matching file always wins over the fallback (so this only kicks in
    // when the primary chain found nothing beyond root).
    let beyond_root = matched.as_ref().is_some_and(|(l, _)| !l.is_empty());
    if !beyond_root {
        if let Some((f_lang, f_country, f_variant)) = resolve_fallback_locale(
            ctx,
            args,
            &bundle_name,
            requested_locale_obj,
            &lang,
            &country,
            &variant,
        ) {
            let fallback_chain = build_locale_chain(&bundle_name, &f_lang, &f_country, &f_variant);
            for (cand, m_lang, m_country) in &fallback_chain {
                let path = format!("{}.properties", cand.replace('.', "/"));
                let loader_now = loader_pin.map(|p| ctx.read_native_pin(p, loader.unwrap()));
                if let Some(bytes) = find_bundle_resource(ctx, loader_now, &path) {
                    let map_now = ctx.read_native_pin(map_pin, map);
                    parse_props_into_map(ctx, map_now, &bytes);
                    matched = Some((m_lang.clone(), m_country.clone()));
                }
            }
            chain.extend(fallback_chain);
        }
    }

    if let Some((m_lang, m_country)) = matched {
        let locale = crate::locale_alloc(ctx, &m_lang, &m_country);
        let obj_now = ctx.read_native_pin(obj_pin, obj);
        if let Some(loc_idx) = ctx.resolve_field_index("java/util/ResourceBundle", "locale") {
            ctx.set_field(obj_now, loc_idx, Value::Object(Some(locale?)));
        } else {
            ctx.set_field(obj_now, 1, Value::Object(Some(locale?)));
        }
        let first_pin = loader_pin.unwrap_or(obj_pin);
        ctx.unpin_native_roots(first_pin);
        return Ok(Some(Value::Object(Some(obj_now))));
    }

    // No .properties on the classpath. Before the synthetic / empty-bundle
    // fallback, try a real class-based `ListResourceBundle` (the JDK ships
    // javac/launcher messages — and some apps ship resources — as compiled
    // bundle classes, e.g. `com.sun.tools.javac.resources.compiler`). Skip the
    // hand-synthesized locale-data families, which stay on their curated path.
    if !is_synthesized_locale_base(&bundle_name) || needs_concrete_bundle_class(&bundle_name) {
        if let Some(real) = try_class_bundle(ctx, &chain) {
            ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
            return Ok(Some(Value::Object(Some(real))));
        }
    }

    // JDK-internal base names keep the synthesized / empty-bundle fallback; a
    // genuinely-absent app bundle gets MissingResourceException
    // (java.util.ResourceBundle.getBundle's contract, which callers such as
    // Tomcat's StringManager rely on).
    if is_jdk_internal_bundle(&bundle_name) {
        ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
        let obj = build_bundle(ctx, &bundle_name);
        return Ok(Some(Value::Object(Some(obj?))));
    }
    ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
    let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 8)?;
    let msg = ctx.create_string(&format!(
        "Can't find bundle for base name {bundle_name}, locale {lang}"
    ));
    ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg)));
    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
        exc,
    ))
}

/// Re-apply `sun.util.resources.TimeZoneNamesBundle.handleGetObject`'s
/// transform when we resolve a value out of a real bundle's `getContents()`
/// directly. That override does NOT return the raw `getContents()` value: when
/// the value is a `String[]`, it returns a NEW `String[]` of length+1 with the
/// lookup key (the zone id) copied into slot 0 and the original entries shifted
/// to slots 1..n (see `javap -c TimeZoneNamesBundle`). The JDK's
/// `TimeZoneNameUtility.getZoneStrings` depends on this layout — each row is
/// `[id, longStd, shortStd, longDst, shortDst, longGen, shortGen]` (7 columns).
/// Reading `getContents()` ourselves bypassed the override and produced a
/// 6-column row (zone id dropped), so `SimpleDateFormat`'s `z` field could not
/// find/format the GMT zone and fell back to "UTC" — corrupting every Tomcat
/// `Date`/`Expires`/`Last-Modified` HTTP header (surfaced by `TestExpiresFilter`,
/// whose `validate` parses the `Expires` header back with `FastHttpDateFormat`).
/// Only `String[]` values are transformed; single-`String` entries (e.g. the
/// `timezone.excity.*` keys) pass through unchanged, exactly like the JDK's
/// `instanceof String[]` guard.
fn maybe_prepend_tz_zone_id(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: ObjectRef,
    val: Value,
) -> Value {
    let arr = match val {
        Value::Object(Some(a)) => a,
        _ => return val,
    };
    // Only TimeZoneNamesBundle subclasses get the id-prepend.
    let is_tz = match ctx.class_id_by_name("sun/util/resources/TimeZoneNamesBundle") {
        Some(tz) => ctx.is_subclass(ctx.class_id_of_object(this), tz),
        None => false,
    };
    if !is_tz {
        return val;
    }
    // ...and only when the value is a reference array (a `String[]`, mirroring
    // the JDK's `instanceof String[]` guard). NOTE: `class_name_of_id` on an
    // array object here reports the *element* class (e.g. "java/lang/String"),
    // not the JVM array descriptor, so detect array-ness via `heap_kind_of`
    // rather than the class name. The `timezone.excity.*` keys carry a single
    // `String` value (kind == Object) and correctly fall through unchanged.
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array
        || ctx.heap_element_type_of(arr) != cratonvm_types::ArrayElementType::Reference
    {
        return val;
    }
    let len = ctx.array_length(arr);
    let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len + 1);
    ctx.set_array_element(new_arr, 0, Value::Object(Some(key)));
    for i in 0..len {
        let e = ctx.get_array_element(arr, i);
        ctx.set_array_element(new_arr, i + 1, e);
    }
    Value::Object(Some(new_arr))
}

/// The parent of a **synthetic** bundle, or `None` when it has none — which is
/// every synthetic bundle today.
///
/// `rb_get_bundle` allocates a synthetic bundle as a bare
/// `java/util/ResourceBundle` with TWO fields under its own convention:
/// field 0 is the backing map, field 1 the locale. The REAL
/// `java.util.ResourceBundle` declares `parent` FIRST, so
/// `get_field_by_name(this, "parent")` resolves to index 0 and hands back the
/// **backing map**.
///
/// Recursing on that re-entered this native with a `java/util/HashMap`
/// receiver. A HashMap is not `is_synthetic`, so the real-subclass arm probed
/// it with `getContents()` and then `handleGetObject(String)` — two
/// `NoSuchMethodError`s per missing key, both swallowed by the `if let Ok(..)`
/// guards, both logged. Every Tomcat suite log carries the pair, attributed to
/// `org/apache/tomcat/util/res/StringManager.getString`; that noise is what
/// `tomcatreactivewebserverfactorytests-graceful-shutdown-timeout-RESOLVED-20260806.md`
/// flagged. The final answer was never wrong — the map has no `parent` field
/// either, so the walk ended in the same `MissingResourceException` the caller
/// catches — which is why it survived this long.
///
/// So: refuse a "parent" that is the backing map, and refuse one that is not a
/// `ResourceBundle` at all. The second half is the load-bearing one — it keeps
/// this correct if the synthetic layout ever gains a slot, instead of trading
/// one index coincidence for another.
///
/// NOTE for whoever gives synthetic bundles a real parent chain: they cannot
/// simply start honouring `setParent`. `java.util.ResourceBundle.setParent` has
/// no native, so real bytecode would write the real layout's field 0 — the
/// backing map's slot — and silently empty the bundle. Give the synthetic shape
/// its own parent slot (or move it to a real `PropertyResourceBundle` with a
/// `lookup` map) before wiring the chain up.
fn synthetic_bundle_parent(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    backing_map: ObjectRef,
) -> Option<ObjectRef> {
    let parent = match ctx.get_field_by_name(this, "parent") {
        Value::Object(Some(p)) => p,
        _ => return None,
    };
    if parent == backing_map {
        return None;
    }
    let rb = ctx.class_id_by_name("java/util/ResourceBundle")?;
    if ctx.is_subclass(ctx.class_id_of_object(parent), rb) {
        Some(parent)
    } else {
        None
    }
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
    // CratonVM's synthetic bundles (built by the `getBundle` native) are raw
    // `java/util/ResourceBundle` instances whose field 0 holds the backing map.
    // A *real* ResourceBundle subclass instantiated from bytecode (e.g. a user
    // `ListResourceBundle`) has the genuine field layout, so the map shortcut
    // would read the wrong slot and return null. Resolve those via their real
    // data instead.
    let this_cid = ctx.class_id_of_object(this);
    let this_cname = ctx.class_name_of_id(this_cid);
    let is_synthetic = this_cname.as_deref() == Some("java/util/ResourceBundle");
    if !is_synthetic {
        // Real PropertyResourceBundle keeps its entries in the `lookup` Map
        // field (populated by its real constructor from the .properties stream),
        // NOT in field 0 and NOT via getContents() (which it doesn't override).
        // Reading field 0 returned a wrong/empty slot → getString gave null, and
        // the getContents() probe raised NoSuchMethodError (keycloak
        // PropertiesUtilTest). Resolve via `lookup`.
        if this_cname.as_deref() == Some("java/util/PropertyResourceBundle") {
            return match ctx.get_field_by_name(this, "lookup") {
                Value::Object(Some(lookup)) => cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(lookup)), Value::Object(Some(key))],
                ),
                _ => Ok(Some(Value::Object(None))),
            };
        }
        // ListResourceBundle subclasses expose their entries via the overridden
        // `getContents()` (an `Object[][]` of `{key, value}` rows). Look the key
        // up there. `getContents` has no native, so this cannot recurse back
        // into this override. Other subclass shapes fall through to the map
        // shortcut (behaviour no worse than before).
        let key_str = ctx.read_string(key);
        if let Ok(Some(Value::Object(Some(contents)))) =
            ctx.invoke_virtual(this, "getContents", "()[[Ljava/lang/Object;", &[])
        {
            let rows = ctx.array_length(contents);
            for i in 0..rows {
                if let Value::Object(Some(row)) = ctx.get_array_element(contents, i) {
                    if ctx.array_length(row) >= 2 {
                        let rk = match ctx.get_array_element(row, 0) {
                            Value::Object(Some(k)) => ctx.read_string(k),
                            _ => None,
                        };
                        if rk.is_some() && rk == key_str {
                            let val = ctx.get_array_element(row, 1);
                            return Ok(Some(maybe_prepend_tz_zone_id(ctx, this, key, val)));
                        }
                    }
                }
            }
            // Key absent in this bundle: walk the parent chain
            // (ResourceBundle.getObject's contract) before failing, so a
            // locale variant inherits keys present only in a less-specific
            // parent — e.g. `compiler_de` falling back to the ROOT
            // `compiler` bundle for untranslated messages.
            if let Value::Object(Some(parent)) = ctx.get_field_by_name(this, "parent") {
                return rb_get_object(
                    ctx,
                    &[Value::Object(Some(parent)), Value::Object(Some(key))],
                );
            }
            // Key absent: throw MissingResourceException — ResourceBundle.getObject's
            // contract, and jakarta.el.ResourceBundleELResolver.getValue catches it
            // to produce the "???key???" sentinel.
            let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 8)?;
            let msg = ctx.create_string(&format!(
                "Can't find resource for key {}",
                key_str.unwrap_or_default()
            ));
            ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg)));
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc,
            ));
        }

        // Not a ListResourceBundle-style class (getContents() unavailable) —
        // fall back to the REAL `getObject()` contract: call the receiver's own
        // `handleGetObject` override virtually. Covers general custom
        // ResourceBundle subclasses like Spring's `MessageSourceResourceBundle`
        // (overrides handleGetObject directly — delegates to a MessageSource —
        // and has neither `getContents()` nor a `lookup` field), which
        // previously fell through to the synthetic-bundle "field 0 as map" hack
        // below and silently misread its `messageSource` field as a Map,
        // returning null for every key
        // (ResourceBundleMessageSourceTests.messageSourceResourceBundle).
        // `handleGetObject` is abstract on `ResourceBundle`, so any concrete
        // subclass reaching this branch necessarily declares its own override —
        // virtual dispatch resolves there, never recursing back into this
        // native (which is registered on the more general `ResourceBundle`).
        if let Ok(Some(val)) = ctx.invoke_virtual(
            this,
            "handleGetObject",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[Value::Object(Some(key))],
        ) {
            if !matches!(val, Value::Object(None)) {
                return Ok(Some(val));
            }
        }
        if let Value::Object(Some(parent)) = ctx.get_field_by_name(this, "parent") {
            return rb_get_object(
                ctx,
                &[Value::Object(Some(parent)), Value::Object(Some(key))],
            );
        }
        let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 8)?;
        let msg = ctx.create_string(&format!(
            "Can't find resource for key {}",
            key_str.unwrap_or_default()
        ));
        ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg)));
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
        ));
    }

    let map = match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = cratonvm_native_collections::native_map_get_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key))],
    );
    // Key absent from the synthetic bundle's backing map: same contract as the
    // real-subclass branches above — walk the parent chain, then throw
    // `MissingResourceException` rather than silently returning null.
    // Hibernate Validator's `AbstractMessageInterpolator.resolveParameter`
    // depends on this: it calls `bundle.getString(key)` inside a
    // `try { ... } catch (MissingResourceException e) { keep original
    // "{param}" text }` — a null return (instead of the exception) let the
    // literal Java string "null" leak into interpolated messages
    // (`MessageSourceMessageInterpolatorIntegrationTests.unknown`,
    // `JksSslStoreBundleTests`'s provider-not-found messages).
    match result {
        Ok(Some(Value::Object(None))) | Ok(None) => {
            if let Some(parent) = synthetic_bundle_parent(ctx, this, map) {
                return rb_get_object(
                    ctx,
                    &[Value::Object(Some(parent)), Value::Object(Some(key))],
                );
            }
            let key_str = ctx.read_string(key);
            let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 8)?;
            let msg = ctx.create_string(&format!(
                "Can't find resource for key {}",
                key_str.unwrap_or_default()
            ));
            ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg)));
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc,
            ))
        }
        other => other,
    }
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
    // Real PropertyResourceBundle: keys live in the `lookup` Map, not field 0.
    let this_cid = ctx.class_id_of_object(this);
    let this_cname = ctx.class_name_of_id(this_cid);
    if this_cname.as_deref() == Some("java/util/PropertyResourceBundle") {
        return match ctx.get_field_by_name(this, "lookup") {
            Value::Object(Some(lookup)) => {
                cratonvm_native_collections::native_map_contains_key_pub(
                    ctx,
                    &[Value::Object(Some(lookup)), Value::Object(Some(key))],
                )
            }
            _ => Ok(Some(Value::Int(0))),
        };
    }
    // Real ListResourceBundle / OpenListResourceBundle subclass (e.g. the CLDR
    // `TimeZoneNamesBundle` the JDK loads through our `getBundle` override):
    // its keys come from the overridden `getContents()`, NOT a field-0 map —
    // field 0 on a real bundle is the inherited `parent` slot, so the map
    // shortcut below would test the wrong object and answer `false`. The JDK's
    // `LocaleResources.getTimeZoneNames` gates its zone-name lookup on
    // `tzb.containsKey(key)`; a wrong `false` there sends it down the metazone
    // fallback and yields "UTC" instead of "GMT" for the GMT zone (breaking
    // every Tomcat `Date`/`Expires`/`Last-Modified` HTTP header, which formats
    // with a `z` field and the GMT TimeZone). Resolve real subclasses through
    // `getContents()` + the parent chain, mirroring `rb_get_object`.
    let is_synthetic = this_cname.as_deref() == Some("java/util/ResourceBundle");
    if !is_synthetic {
        let key_str = ctx.read_string(key);
        if let Ok(Some(Value::Object(Some(contents)))) =
            ctx.invoke_virtual(this, "getContents", "()[[Ljava/lang/Object;", &[])
        {
            let rows = ctx.array_length(contents);
            for i in 0..rows {
                if let Value::Object(Some(row)) = ctx.get_array_element(contents, i) {
                    if ctx.array_length(row) >= 1 {
                        let rk = match ctx.get_array_element(row, 0) {
                            Value::Object(Some(k)) => ctx.read_string(k),
                            _ => None,
                        };
                        if rk.is_some() && rk == key_str {
                            return Ok(Some(Value::Int(1)));
                        }
                    }
                }
            }
            // Key absent here: walk the parent chain (a locale variant inherits
            // keys present only in a less-specific parent bundle).
            if let Value::Object(Some(parent)) = ctx.get_field_by_name(this, "parent") {
                return rb_contains_key(
                    ctx,
                    &[Value::Object(Some(parent)), Value::Object(Some(key))],
                );
            }
            return Ok(Some(Value::Int(0)));
        }
    }
    let map = match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Int(0))),
    };
    cratonvm_native_collections::native_map_contains_key_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key))],
    )
}

// java.time text-name support (DateTimeTextProvider path).
//
// `java.time.format.DateTimeFormatterBuilder` text fields (`EEE`/`MMM`/`a`/`G`)
// resolve their localized strings through
// `java.time.format.DateTimeTextProvider.createStore`, which asks
// `sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueNames`
// (plural → `Map<name,value>`) and, for narrow month/day whose names collapse
// in a Map, `retrieveJavaTimeFieldValueName` (singular). Both walk the
// `jdk.localedata` class-based CLDR bundles CratonVM doesn't surface, so they
// return null/empty and the formatter falls back to the raw NUMERIC value —
// e.g. Spring's RFC-1123 `HttpHeaders` date formatter (built with `Locale.US`)
// renders "4, 18 12 2008 …" instead of "Thu, 18 Dec 2008 …". We answer these
// two statics directly from the en/US CLDR name tables, mirroring the
// `FormatData` / `getDateTimePattern` overrides in this file. Only English
// (and the root locale) is served; other languages return null so the
// formatter keeps its existing numeric fallback rather than showing English.

// java.util.Calendar field constants.
const CAL_ERA: i32 = 0;
const CAL_MONTH: i32 = 2;
const CAL_DAY_OF_WEEK: i32 = 7;
const CAL_AM_PM: i32 = 9;
// java.util.Calendar.STANDALONE_MASK — English standalone names equal the
// format names, so we fold it away before selecting a style.
const CAL_STANDALONE_MASK: i32 = 0x8000;
// Calendar text-style bases (after masking off STANDALONE): SHORT_FORMAT=1,
// LONG_FORMAT=2, NARROW_FORMAT=4. ALL_STYLES=0 is not modelled.
const CAL_STYLE_SHORT: i32 = 1;
const CAL_STYLE_LONG: i32 = 2;
const CAL_STYLE_NARROW: i32 = 4;

fn int_arg(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    }
}

/// True when the `Locale` argument is English or the root/empty locale — the
/// only families our hardcoded en/US CLDR names are valid for. A missing
/// locale is treated as English (the JDK default the Spring suite runs under).
fn locale_is_english(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> bool {
    match arg {
        Some(Value::Object(Some(loc))) => {
            match ctx.invoke_virtual(*loc, "getLanguage", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => {
                    let lang = ctx.read_string(s).unwrap_or_default();
                    lang.is_empty() || lang == "en"
                }
                _ => true,
            }
        }
        _ => true,
    }
}

/// English CLDR display names for a `gregory` Calendar field at a given style,
/// returned as `(name, Calendar field value)` pairs in field-value order.
/// `None` for fields/styles we don't model (e.g. `ALL_STYLES`). The Calendar
/// values follow the JDK convention: `MONTH` JANUARY=0..DECEMBER=11,
/// `DAY_OF_WEEK` SUNDAY=1..SATURDAY=7, `ERA` BC=0/AD=1, `AM_PM` AM=0/PM=1 —
/// exactly what `DateTimeTextProvider.createStore` re-keys.
fn en_calendar_field_names(field: i32, style: i32) -> Option<Vec<(&'static str, i32)>> {
    let base = style & !CAL_STANDALONE_MASK;
    match field {
        CAL_MONTH => {
            let names: &[&str; 12] = match base {
                CAL_STYLE_LONG => &[
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
                ],
                CAL_STYLE_SHORT => &[
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ],
                CAL_STYLE_NARROW => &["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D"],
                _ => return None,
            };
            Some((0..12).map(|i| (names[i as usize], i)).collect())
        }
        CAL_DAY_OF_WEEK => {
            // Index 0 = Sunday; Calendar value = index + 1 (SUNDAY=1).
            let names: &[&str; 7] = match base {
                CAL_STYLE_LONG => &[
                    "Sunday",
                    "Monday",
                    "Tuesday",
                    "Wednesday",
                    "Thursday",
                    "Friday",
                    "Saturday",
                ],
                CAL_STYLE_SHORT => &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
                CAL_STYLE_NARROW => &["S", "M", "T", "W", "T", "F", "S"],
                _ => return None,
            };
            Some((0..7).map(|i| (names[i as usize], i + 1)).collect())
        }
        CAL_ERA => {
            let names: &[&str; 2] = match base {
                CAL_STYLE_LONG => &["Before Christ", "Anno Domini"],
                CAL_STYLE_SHORT => &["BC", "AD"],
                CAL_STYLE_NARROW => &["B", "A"],
                _ => return None,
            };
            Some(vec![(names[0], 0), (names[1], 1)])
        }
        CAL_AM_PM => {
            let names: &[&str; 2] = match base {
                CAL_STYLE_NARROW => &["a", "p"],
                CAL_STYLE_SHORT | CAL_STYLE_LONG => &["AM", "PM"],
                _ => return None,
            };
            Some(vec![(names[0], 0), (names[1], 1)])
        }
        _ => None,
    }
}

pub fn register(registry: &mut NativeMethodRegistry) -> Result<(), MethodCallFailed> {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    // (String, Locale, ClassLoader, Control) — used by WildFly's
    // `StandardResourceDescriptionResolver.getResourceBundle`
    // (`ResourceBundle.getBundle(base, locale, bundleLoader.get(),
    // Control.getNoFallbackControl(FORMAT_PROPERTIES))`). This was the ONE
    // `getBundle` overload left unintercepted, so it ran the real-JDK bytecode
    // whose Module/caller-class machinery trips in our partial bootstrap and
    // surfaced as `MissingResourceException: Can't find bundle for base name
    // org.wildfly.extension.health.LocalDescriptions` even though the
    // `.properties` is on the classpath. Route it through the same builder as
    // the other overloads (which load the resource via `find_resource`).
    registry.register(
        rb,
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;",
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

    // getKeys() / keySet() — REQUIRED for any caller that *iterates* the
    // bundle rather than looking keys up individually. `getKeys()` is an
    // ABSTRACT method on `java.util.ResourceBundle`: our synthetic bundle
    // object carries the abstract class `java/util/ResourceBundle`, so
    // without an explicit native override an `invokevirtual getKeys()`
    // resolves the abstract declaration (no Code) and throws
    // `AbstractMethodError`. Concrete tripwire: the JDK XML serializer's
    // `com.sun.org.apache.xml.internal.serializer.CharInfo.<init>` loads
    // `XMLEntities` via `ResourceBundle.getBundle(...)` then walks it with
    // `getKeys()` — the AbstractMethodError aborted `ToXMLStream.<clinit>`,
    // left `m_xmlcharInfo` null, and surfaced downstream as a
    // `TransformerException` that broke Hazelcast 5.4.0's XSLT schema
    // validation at boot.
    //
    // We return a synthetic `java/util/Enumeration$Impl` (field 0 =
    // `Object[]` of keys, field 1 = `int` cursor) — its
    // `hasMoreElements`/`nextElement` natives are registered
    // unconditionally by `register_enumeration_impl_natives`, so this works
    // in both real-JDK and synthetic-JDK modes.
    registry.register(rb, "getKeys", "()Ljava/util/Enumeration;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let map = match ctx.get_field(this, 0) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        };
        let arr = cratonvm_native_collections::native_map_keys_as_array(ctx, map);
        let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
        Ok(Some(Value::Object(Some(enm))))
    });
    // keySet() — newer (Java 9+) accessor with the same role as getKeys().
    // Delegate to the backing map's keySet so callers get a real Set view.
    registry.register(rb, "keySet", "()Ljava/util/Set;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let map = match ctx.get_field(this, 0) {
            Value::Object(Some(m)) => m,
            _ => return Ok(Some(Value::Object(None))),
        };
        cratonvm_native_collections::native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
    });
    // Per JLS / java.util.ResourceBundle Javadoc: getLocale() must NEVER
    // return null on a successfully loaded bundle. Stock JDK returns the
    // bundle's locale (or Locale.ROOT for a base bundle). Returning null
    // here causes Tomcat's StringManager.<init> to NPE on
    // `bundle.getLocale().equals(Locale.ROOT)`, which converts to
    // ExceptionInInitializerError on Tomcat.<clinit>. That in turn makes
    // Spring's `OnClassCondition` evaluate `@ConditionalOnClass(Tomcat.class)`
    // to false, and `ServletWebServerFactoryConfiguration$EmbeddedTomcat`
    // never registers a ServletWebServerFactory bean →
    // MissingWebServerFactoryBeanException at app startup.
    registry.register(rb, "getLocale", "()Ljava/util/Locale;", |ctx, args| {
        // Return the bundle's own `locale` field (set when it was built, to the
        // locale it resolved for). Per the ResourceBundle Javadoc this must
        // never be null on a loaded bundle, so fall back to an empty/ROOT
        // Locale when the field is somehow unset.
        if let Some(Value::Object(Some(this))) = args.first() {
            if let Value::Object(Some(loc)) = ctx.get_field_by_name(*this, "locale") {
                return Ok(Some(Value::Object(Some(loc))));
            }
        }
        Ok(Some(Value::Object(Some(crate::locale_alloc(ctx, "", "")))))
    });
    // getBaseBundleName() — the real `ResourceBundle` returns its private
    // `name` field. The Javadoc permits null ("or null if unknown"), but we
    // are never in that position: every bundle this file builds was built FOR
    // a specific base name, so answering null threw away information we hold.
    // Spring's `ResourceBundleMessageSource` and JDK `Bundles` caches key on
    // it. Read the named field (populated by `build_bundle` /
    // `rb_get_bundle`, and by the real JDK factory in real-JDK mode) and fall
    // back to null only when it genuinely is not set.
    registry.register(
        rb,
        "getBaseBundleName",
        "()Ljava/lang/String;",
        |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.first() {
                if let v @ Value::Object(Some(_)) = ctx.get_field_by_name(*this, "name") {
                    return Ok(Some(v));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

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
            let outer = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 3);
            // 13-element layout: indices 0-10 are mandatory (decimal,
            // grouping, pattern sep, percent, zero, digit, minus,
            // exponent, perMille, infinity, NaN), 11=monetary decimal
            // separator, 12=monetary grouping separator (both optional —
            // DFS falls back to the regular separators when those slots
            // are empty / the array is short).
            let elems = make_string_array(
                ctx,
                &[
                    ".", ",", ";", "%", "0", "#", "-", "E", "\u{2030}", "\u{221E}", "NaN", ".", ",",
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

    // java.util.Currency.getSymbol(Locale) — real bytecode resolves the
    // display symbol via `LocaleServiceProviderPool.getPool(CurrencyNameProvider.class)
    // .getLocalizedObject(...)`, which walks the `jdk.localedata` class-based
    // resource chain CratonVM doesn't surface (same gap as the
    // ResourceBundle/LocaleResources overrides above). With no provider found,
    // the JDK's own fallback kicks in: "use currency code as symbol of last
    // resort" — i.e. it returns the bare ISO 4217 code ("USD") instead of "$".
    //
    // This silently poisons `DecimalFormatSymbols.initialize` below:
    // `setCurrencySymbol("$")` runs first, but `setInternationalCurrencySymbol
    // ("USD")` runs right after and its real bytecode body re-derives
    // `currencySymbol = currency.getSymbol(locale)` — which, without this
    // override, clobbers the correct "$" back to "USD". Surfaced as
    // `CurrencyStyleFormatterTests`/`NumberFormattingTests` formatting
    // `new BigDecimal("23")` as "USD23.00" instead of "$23.00", and parsing
    // "$23.56" failing with `ParseException` because the computed
    // `positivePrefix` affix ("USD") never matches the literal "$" prefix in
    // the input text.
    //
    // Answer directly from a small curated ISO-code → symbol table (same
    // curation level as the existing `populate_currency_names_en` map and the
    // `Currency.getSymbol()` no-arg override that already existed for
    // synthetic-JDK mode). `getSymbol()` (no-arg) delegates to
    // `getSymbol(Locale.getDefault(...))` in real bytecode, so overriding only
    // the 1-arg overload fixes both call forms.
    registry.register(
        "java/util/Currency",
        "getSymbol",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let code = match ctx.get_field_by_name(this, "currencyCode") {
                Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
                _ => String::new(),
            };
            let sym = match code.as_str() {
                "USD" => "$",
                "EUR" => "\u{20AC}",
                "GBP" => "\u{00A3}",
                "JPY" => "\u{00A5}",
                "CNY" => "\u{00A5}",
                "CHF" => "CHF",
                "CAD" => "$",
                "AUD" => "$",
                _ => &code,
            };
            let s = ctx.create_string(sym);
            Ok(Some(Value::Object(Some(s))))
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
            // Persist the `locale` field on the DFS instance so that the
            // public setCurrencySymbol/setInternationalCurrencySymbol setters
            // — which re-enter the JDK's `initializeCurrency(this.locale)` —
            // do not NPE on `locale.getCountry()`. Also pre-set
            // `currencyInitialized=true` to short-circuit the JDK's
            // re-entrant currency initialization (Kafka 4.2.0 boot path).
            ctx.set_field_by_name(this, "locale", locale);
            ctx.set_field_by_name(this, "currencyInitialized", Value::Int(1));
            // Locale-aware decimal/grouping separators. The JDK's per-locale
            // CLDR data lives in `jdk.localedata`, which CratonVM doesn't
            // surface, so derive the two separators that actually vary by
            // language from the locale's language tag (curated, like the
            // `getDateTimePattern` override). Without this, every locale got the
            // en/US separators and e.g. German `NumberFormat.parse("1,1")`
            // returned 11.0 instead of 1.1 (Spring DLBF customEditor/converter).
            // All other symbols are locale-invariant across the locales the
            // suite exercises. (dec, grp): en='.'/',' ; de=','/'.' ;
            // fr=','/' ' (narrow no-break space).
            let lang = match locale {
                Value::Object(Some(loc)) => {
                    match ctx.invoke_virtual(loc, "getLanguage", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            };
            let (dec_sep, grp_sep): (char, char) = match lang.as_str() {
                // Comma-decimal / dot-grouping family (German, Spanish, Italian,
                // Dutch, Portuguese, Danish, Polish, …). Verified against JDK 25
                // for de; the others share the CLDR ','/'.' convention.
                "de" | "es" | "it" | "nl" | "pt" | "da" | "pl" | "ro" | "el" | "tr" | "id" => {
                    (',', '.')
                }
                // French uses a narrow no-break space (U+202F) for grouping.
                "fr" => (',', '\u{202F}'),
                // en and everything else: US/root separators.
                _ => ('.', ','),
            };
            // Set every public char/string field via the corresponding
            // setter so we don't depend on instance-field layout.
            let _ = ctx.invoke_virtual(
                this,
                "setDecimalSeparator",
                "(C)V",
                &[Value::Int(dec_sep as i32)],
            );
            let _ = ctx.invoke_virtual(
                this,
                "setGroupingSeparator",
                "(C)V",
                &[Value::Int(grp_sep as i32)],
            );
            let _ = ctx.invoke_virtual(
                this,
                "setPatternSeparator",
                "(C)V",
                &[Value::Int(';' as i32)],
            );
            let _ = ctx.invoke_virtual(this, "setPercent", "(C)V", &[Value::Int('%' as i32)]);
            let _ = ctx.invoke_virtual(this, "setZeroDigit", "(C)V", &[Value::Int('0' as i32)]);
            let _ = ctx.invoke_virtual(this, "setDigit", "(C)V", &[Value::Int('#' as i32)]);
            let _ = ctx.invoke_virtual(this, "setMinusSign", "(C)V", &[Value::Int('-' as i32)]);
            let _ = ctx.invoke_virtual(this, "setPerMill", "(C)V", &[Value::Int(0x2030)]);
            let exp = ctx.create_string("E");
            let _ = ctx.invoke_virtual(
                this,
                "setExponentSeparator",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(exp))],
            );
            let inf = ctx.create_string("\u{221E}");
            let _ = ctx.invoke_virtual(
                this,
                "setInfinity",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(inf))],
            );
            let nan = ctx.create_string("NaN");
            let _ = ctx.invoke_virtual(
                this,
                "setNaN",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(nan))],
            );
            let _ = ctx.invoke_virtual(
                this,
                "setMonetaryDecimalSeparator",
                "(C)V",
                &[Value::Int(dec_sep as i32)],
            );
            // setMonetaryGroupingSeparator — without this, `monetaryGroupingSeparator`
            // keeps its zero-value default. Real `DecimalFormat.subparse` (the
            // number-parsing engine) uses `symbols.getMonetaryGroupingSeparator()`
            // instead of `symbols.getGroupingSeparator()` whenever `isCurrencyFormat`
            // is true (i.e. any format built via `NumberFormat.getCurrencyInstance`),
            // so a grouped currency string like "$3,339.12" failed to recognize the
            // "," as a grouping separator and parsing stopped after the first digit
            // group — surfaced as `NumberFormattingTests.currencyFormatting()`
            // binding "$3,339.12" with a ParseException (getErrorCount()==1).
            let _ = ctx.invoke_virtual(
                this,
                "setMonetaryGroupingSeparator",
                "(C)V",
                &[Value::Int(grp_sep as i32)],
            );
            let cs = ctx.create_string("$");
            let _ = ctx.invoke_virtual(
                this,
                "setCurrencySymbol",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(cs))],
            );
            let ics = ctx.create_string("USD");
            let _ = ctx.invoke_virtual(
                this,
                "setInternationalCurrencySymbol",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(ics))],
            );
            // Locale field is not exposed via a public setter; the JDK's
            // own initialize() does putfield directly. Use invoke on a
            // synthetic helper via reflection-free path: call getLocale()
            // first to learn the field index dynamically would be
            // overkill — instead, leave it null and rely on
            // DFS.getLocale() returning whatever was set during
            // serialization (or null).  Jackson / SimpleDateFormat
            // never read DFS.getLocale() during their bootstrap.
            Ok(None)
        },
    );
    registry.register(
        "java/text/DecimalFormatSymbols",
        "getInstance",
        "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;",
        |ctx, args| {
            let locale = args.first().copied().unwrap_or(Value::Object(None));
            ctx.new_object_initialized(
                "java/text/DecimalFormatSymbols",
                "(Ljava/util/Locale;)V",
                &[locale],
            )
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

    // sun.util.locale.provider.LocaleResources.getDateTimePattern(int timeStyle,
    // int dateStyle, Calendar cal) → String.  The real-JDK body fetches the
    // date/time pattern arrays through `LocaleData.getDateFormatData(locale)` →
    // `sun.util.resources.Bundles.of(...)`, the jdk.localedata class-based
    // resource path CratonVM does not surface (same gap as BreakIterator /
    // getDecimalFormatSymbolsData), so it returns a **null** pattern. The caller
    // `DateFormatProviderImpl.getInstance` then does `new SimpleDateFormat(null,
    // locale)` → `applyPatternImpl(null)` → `compile(null)` → NPE
    // ("Cannot invoke String.length() because pattern is null"). This is the
    // root of BUG-15: it breaks MessageFormat's `{n,date}` / `{n,time}` typed
    // elements (Spring StaticMessageSource → MessageFormat) and the
    // `DateFormat.get{Date,Time,DateTime}Instance` style factories (Spring
    // DateFormatter style/ISO-less path, property/date editors).
    //
    // Answer directly from the same 9-slot en-US DateTimePatterns table
    // `populate_format_data_en` uses (4 time FULL/LONG/MEDIUM/SHORT, 4 date
    // FULL/LONG/MEDIUM/SHORT, combiner "{1} {0}"). Styles are
    // FULL=0/LONG=1/MEDIUM=2/SHORT=3; a negative style means "no date" or "no
    // time" (DateFormat.getDateInstance passes timeStyle=-1,
    // getTimeInstance passes dateStyle=-1). With the real pattern in hand the
    // downstream real-JDK `SimpleDateFormat` ctor + `format` run unchanged and
    // match HotSpot. Locale-aware for the en / de families (the locales the
    // Spring suite exercises); any other language falls back to en. Patterns
    // are the exact JDK 25 CLDR forms (`\u{202f}` = the narrow no-break space
    // CLDR puts before the am/pm marker); the date-time combiner is "{1}, {0}"
    // (date + ", " + time). Consistent with the existing hardcoded
    // `getNumberPatterns` / `getDecimalFormatSymbolsData` overrides.
    // Shared body for the java.text `getDateTimePattern` and the java.time
    // `getJavaTimeDateTimePattern`. Both take (this, timeStyle, dateStyle,
    // <cal-or-calType>): style indices FULL=0/LONG=1/MEDIUM=2/SHORT=3, a
    // negative style meaning "no date"/"no time". The 4th arg (a Calendar or a
    // calendar-type String) is ignored — only the ISO/Gregorian family is
    // modelled — and the en/de java.time patterns are identical to the java.text
    // ones for the locales the Spring suite exercises, so one table serves both.
    fn locale_datetime_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> Value {
        // Read this LocaleResources' `locale` field → language tag so we
        // pick the right CLDR pattern family. Falls back to en when the
        // field/getLanguage path yields nothing.
        let lang = match args.first() {
            Some(Value::Object(Some(this))) => match ctx.get_field_by_name(*this, "locale") {
                Value::Object(Some(loc)) => {
                    match ctx.invoke_virtual(loc, "getLanguage", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            },
            _ => String::new(),
        };
        // (time FULL/LONG/MEDIUM/SHORT, date FULL/LONG/MEDIUM/SHORT)
        let (time, date): ([&str; 4], [&str; 4]) = if lang == "de" {
            (
                ["HH:mm:ss zzzz", "HH:mm:ss z", "HH:mm:ss", "HH:mm"],
                ["EEEE, d. MMMM y", "d. MMMM y", "dd.MM.y", "dd.MM.yy"],
            )
        } else {
            (
                [
                    "h:mm:ss\u{202f}a zzzz",
                    "h:mm:ss\u{202f}a z",
                    "h:mm:ss\u{202f}a",
                    "h:mm\u{202f}a",
                ],
                ["EEEE, MMMM d, y", "MMMM d, y", "MMM d, y", "M/d/yy"],
            )
        };
        let style = |i: usize| -> i32 {
            match args.get(i) {
                Some(Value::Int(n)) => *n,
                _ => -1,
            }
        };
        let time_style = style(1);
        let date_style = style(2);
        let pick = |arr: &[&str; 4], s: i32| -> String {
            // clamp out-of-range styles to MEDIUM rather than panic
            let idx = if (0..=3).contains(&s) { s as usize } else { 2 };
            arr[idx].to_string()
        };
        let pattern = match (date_style >= 0, time_style >= 0) {
            // combiner "{1}, {0}" = datePattern + ", " + timePattern
            (true, true) => format!("{}, {}", pick(&date, date_style), pick(&time, time_style)),
            (true, false) => pick(&date, date_style),
            (false, true) => pick(&time, time_style),
            (false, false) => String::new(),
        };
        Value::Object(Some(ctx.create_string(&pattern)))
    }

    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getDateTimePattern",
        "(IILjava/util/Calendar;)Ljava/lang/String;",
        |ctx, args| Ok(Some(locale_datetime_pattern(ctx, args))),
    );

    // The java.time localized-formatting path
    // (DateTimeFormatter.ofLocalizedDate/Time/DateTime →
    // DateTimeFormatterBuilder$LocalizedPrinterParser.formatter →
    // DateTimeFormatterBuilder.getLocalizedDateTimePattern →
    // LocaleResources.getJavaTimeDateTimePattern) reads its pattern from the
    // SAME jdk.localedata bundle CratonVM does not surface. Without this native
    // it returns null, and the caller's `appendPattern(null)` throws NPE
    // ("pattern") — the root of the Spring format.datetime.standard failures
    // (bindLocalDate*, bindLocalDateTime*, DateTimeFormatterFactory style
    // formatting). The signature differs from getDateTimePattern only in its 3rd
    // arg (calendar-type String, e.g. "iso8601", vs Calendar); the pattern table
    // is shared. Must be force-listed in the same two dispatch gates as
    // getDateTimePattern (concrete JDK-library methods consult the native
    // registry only when force-listed).
    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getJavaTimeDateTimePattern",
        "(IILjava/lang/String;)Ljava/lang/String;",
        |ctx, args| Ok(Some(locale_datetime_pattern(ctx, args))),
    );

    // sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueNames(
    //     String id, int field, int style, Locale locale) -> Map<String,Integer>
    // The java.time text-name entry point (see the module note above). Build a
    // real java.util.HashMap of name -> Calendar-value so the downstream
    // real-JDK `DateTimeTextProvider.createStore` bytecode can iterate its
    // `entrySet()` unchanged. Serve only the `gregory` calendar in English;
    // everything else returns null (numeric fallback), matching HotSpot for
    // the locales the suite exercises.
    registry.register(
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueNames",
        "(Ljava/lang/String;IILjava/util/Locale;)Ljava/util/Map;",
        |ctx, args| {
            if !calendar_id_is_gregorian(ctx, args.first()) || !locale_is_english(ctx, args.get(3))
            {
                return Ok(Some(Value::Object(None)));
            }
            let field = int_arg(args, 1);
            let style = int_arg(args, 2);
            // Narrow month/day names have duplicates ("J"/"J"/"J", "S"/"S")
            // that a Map<String,Integer> would collapse; DateTimeTextProvider
            // handles those via the singular per-value lookup below, so return
            // null here to steer it there.
            let base = style & !CAL_STANDALONE_MASK;
            if base == CAL_STYLE_NARROW && (field == CAL_MONTH || field == CAL_DAY_OF_WEEK) {
                return Ok(Some(Value::Object(None)));
            }
            let entries = match en_calendar_field_names(field, style) {
                Some(e) => e,
                None => return Ok(Some(Value::Object(None))),
            };
            let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
                Some(Value::Object(Some(m))) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            for (name, val) in entries {
                let k = ctx.create_string(name);
                let boxed = ctx
                    .invoke(
                        "java/lang/Integer",
                        "valueOf",
                        "(I)Ljava/lang/Integer;",
                        &[Value::Int(val)],
                    )?
                    .unwrap_or(Value::Object(None));
                ctx.invoke_virtual(
                    map,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(k)), boxed],
                )?;
            }
            Ok(Some(Value::Object(Some(map))))
        },
    );

    // sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueName(
    //     String id, int field, int value, int style, Locale locale) -> String
    // Singular sibling of the above — used by `DateTimeTextProvider` for narrow
    // month/day (and as a fallback for any other style whose plural map came
    // back null). Returns the single English CLDR name for the requested
    // Calendar field value.
    registry.register(
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueName",
        "(Ljava/lang/String;IIILjava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            if !calendar_id_is_gregorian(ctx, args.first()) || !locale_is_english(ctx, args.get(4))
            {
                return Ok(Some(Value::Object(None)));
            }
            let field = int_arg(args, 1);
            let value = int_arg(args, 2);
            let style = int_arg(args, 3);
            let entries = match en_calendar_field_names(field, style) {
                Some(e) => e,
                None => return Ok(Some(Value::Object(None))),
            };
            for (name, val) in entries {
                if val == value {
                    let s = ctx.create_string(name);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // java.text.Normalizer.normalize / isNormalized — real Unicode normalization
    // via the `unicode-normalization` crate.
    //
    // The real-JDK bodies drive `sun.text.normalizer` off ICU normalization
    // tables that CratonVM's jimage path does not surface, so in real-JDK mode
    // they return garbage (e.g. `normalize("ï", NFD)` yields six U+0226 chars),
    // breaking Spring `ContentDisposition.transliterateToAscii` (which NFD-
    // decomposes accented filename chars). These natives are ALSO registered by
    // `phases_late::register_p61_text_formatting`, but that runs only in
    // synthetic-JDK mode (`register_synthetic_overrides`); the real-JDK boot
    // path registers only `register_essential_natives` (this function), so the
    // Normalizer natives must be surfaced here too. Companion force-native gate:
    // `interpreter.rs::force_native_over_real_jdk_bytecode` /
    // `vm_exec.rs` `check_override` pin these over the broken JDK bytecode.
    registry.register(
        "java/text/Normalizer",
        "normalize",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Ljava/lang/String;",
        |ctx, args| {
            use unicode_normalization::UnicodeNormalization;
            let input = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            // java.text.Normalizer.Form ordinals: NFD=0, NFC=1, NFKD=2, NFKC=3
            // (declaration order in the JDK enum — NOT alphabetical).
            let form_ordinal = normalizer_form_ordinal(ctx, args.get(1));
            let normalized = match form_ordinal {
                0 => input.nfd().collect::<String>(),  // NFD
                1 => input.nfc().collect::<String>(),  // NFC
                2 => input.nfkd().collect::<String>(), // NFKD
                3 => input.nfkc().collect::<String>(), // NFKC
                _ => input,
            };
            let s = ctx.create_string(&normalized);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    registry.register(
        "java/text/Normalizer",
        "isNormalized",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z",
        |ctx, args| {
            use unicode_normalization::{
                is_nfc_quick, is_nfd_quick, is_nfkc_quick, is_nfkd_quick, IsNormalized,
            };
            let input = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(1))),
            };
            // Form ordinals: NFD=0, NFC=1, NFKD=2, NFKC=3 (JDK enum order).
            let form_ordinal = normalizer_form_ordinal(ctx, args.get(1));
            let normalized = match form_ordinal {
                0 => is_nfd_quick(input.chars()) == IsNormalized::Yes,
                1 => is_nfc_quick(input.chars()) == IsNormalized::Yes,
                2 => is_nfkd_quick(input.chars()) == IsNormalized::Yes,
                3 => is_nfkc_quick(input.chars()) == IsNormalized::Yes,
                _ => true,
            };
            Ok(Some(Value::Int(if normalized { 1 } else { 0 })))
        },
    );

    registry.set_category(__prev_cat);
    Ok(())
}

/// Read a `java.text.Normalizer.Form` enum argument's ordinal (NFC=0, NFD=1,
/// NFKC=2, NFKD=3) via `Enum.ordinal()`. The virtual call works for both the
/// real-JDK `Form` enum (Enum layout: `name` at field 0, `ordinal` at field 1)
/// and the synthetic one — reading field 0 as an int would pick up the `name`
/// String reference and always yield 0 (NFC). Defaults to 0 (NFC) when the
/// argument is null or the call fails.
fn normalizer_form_ordinal(ctx: &mut dyn NativeContext, form: Option<&Value>) -> i32 {
    match form {
        Some(Value::Object(Some(f))) => ctx
            .invoke_virtual(*f, "ordinal", "()I", &[])
            .ok()
            .flatten()
            .and_then(|v| v.as_int())
            .unwrap_or(0),
        _ => 0,
    }
}

/// True when the calendar-type id argument denotes the Gregorian/ISO calendar
/// (`DateTimeTextProvider` always passes `"gregory"`). Non-Gregorian ids
/// (`japanese`, `buddhist`, …) return false so their own providers keep
/// running — we must not answer them with Gregorian names.
fn calendar_id_is_gregorian(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> bool {
    match arg {
        Some(Value::Object(Some(s))) => {
            let id = ctx.read_string(*s).unwrap_or_default();
            id.eq_ignore_ascii_case("gregory") || id.eq_ignore_ascii_case("iso8601")
        }
        _ => false,
    }
}
