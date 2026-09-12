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
fn put_arr(ctx: &mut dyn NativeContext, map_pin: usize, map: ObjectRef, key: &str, items: &[&str]) {
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

/// Pins `map` across [`populate_format_data_en_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `map` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn populate_format_data_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_format_data_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

fn populate_format_data_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
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
    put_arr(
        ctx,
        map_pin,
        map,
        "DayNarrows",
        &["S", "M", "T", "W", "T", "F", "S"],
    );
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
    put_str(
        ctx,
        map_pin,
        map,
        "DateTimePatternChars",
        "GyMdkHmsSEDFwWahKzZYuXL",
    );
    put_arr(
        ctx,
        map_pin,
        map,
        "NumberPatterns",
        // Slot 1 must NOT carry a `;(¤#,##0.00)` negative subpattern: that is
        // the CLDR *accounting* form, no locale uses it as the standard
        // currency pattern, and its presence stops `DecimalFormat` prefixing a
        // minus sign. Kept byte-identical to the `getNumberPatterns()` override
        // below — this is the FormatData bundle copy of the same table, and the
        // accounting pattern was wrong in both. Grep the pattern STRING, not
        // the function name, before deciding a table like this has one home.
        &["#,##0.###", "\u{00A4}#,##0.00", "#,##0%", "#E0"],
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
fn populate_locale_names_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_locale_names_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

fn populate_locale_names_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
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

/// Pins `map` across [`populate_calendar_data_en_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `map` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn populate_calendar_data_en(ctx: &mut dyn NativeContext, map: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*map);
    let w5_out = populate_calendar_data_en_body(ctx, *map);
    *map = ctx.read_native_pin(w5_pin, *map);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

fn populate_calendar_data_en_body(ctx: &mut dyn NativeContext, map: ObjectRef) {
    // cceres5: every put below allocates; one entry pin, read per call.
    let map_pin = ctx.pin_native_root(map);
    put_str(ctx, map_pin, map, "firstDayOfWeek", "1"); // Sunday
    put_str(ctx, map_pin, map, "minimalDaysInFirstWeek", "1");
    ctx.unpin_native_roots(map_pin);
}

/// Copy the JDK image's OWN `CalendarData` rows into the synthetic bundle,
/// returning whether anything was read.
///
/// `firstDayOfWeek` / `minimalDaysInFirstWeek` are NOT per-locale integers in
/// CLDR. They are one region-keyed TABLE shared by every locale --
/// `"1: AG AS BD ... US ...;2: 001 AD ... DE ...;6: MV;7: AE AF ..."` -- which
/// `CLDRCalendarDataProviderImpl` parses and then selects from by the locale's
/// COUNTRY. `populate_calendar_data_en` writes a bare `"1"` in its place, and a
/// bare `"1"` is not a region table: the real provider finds no region in it,
/// answers 0, and `CalendarDataUtility`'s "not in 1..7" guard substitutes its
/// own default of 1. Every locale then reports Sunday / 1 minimal day, and
/// `en-US` looks healthy only because 1/1 happens to be the right answer for
/// the US -- which is what made the defect survive its own control.
///
/// This reads the real bundle class out of the image by the same mechanism
/// `cldr_collation_rule` uses for `CollationData`, so the region table arrives
/// intact rather than being re-derived from a curated table in this file (which
/// would be one more copy of CLDR to keep in step with the image). Returning
/// false leaves the caller on that curated fallback, which is still the right
/// answer for an image that has no `sun/util/resources/cldr/CalendarData.class`
/// at all.
fn populate_calendar_data_from_cldr(
    ctx: &mut dyn NativeContext,
    map_pin: usize,
    map: ObjectRef,
    lang: &str,
    country: &str,
) -> bool {
    let Some(table) = load_cldr_table(ctx, "sun.util.resources.cldr.CalendarData", lang, country)
    else {
        return false;
    };
    // GC: `put_str`/`put_arr` re-read `map` through `map_pin` on every call, so
    // the raw `map` handed in here may already be a pre-move address -- the
    // same contract the `CollationData` arm below relies on.
    let mut wrote = false;
    for (key, value) in table.iter() {
        match value {
            CldrValue::Str(s) => {
                crate::phases_late::text_intl::put_str(ctx, map_pin, map, key, s);
                wrote = true;
            }
            CldrValue::Arr(items) => {
                let refs: Vec<&str> = items.iter().map(String::as_str).collect();
                crate::phases_late::text_intl::put_arr(ctx, map_pin, map, key, &refs);
                wrote = true;
            }
        }
    }
    wrote
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

// ---------------------------------------------------------------------------
// W7-80 — the JDK image's own CLDR locale data
//
// Every "CratonVM doesn't surface jdk.localedata" comment in this file was
// STALE. Measured on `--jdk-only` with the 2026-08-12 dev binary, before a
// line of this was written:
//
//   LOAD ok sun.text.resources.cldr.ext.FormatData_ru  super=java.util.ListResourceBundle
//     NEW ok / CONTENTS rows=415
//     MonthAbbreviations=[янв., февр., мар., апр., мая, …]
//
// The classes load out of the jimage, instantiate through their public no-arg
// constructor, and `getContents()` evaluates. The only thing that ever failed
// was `setAccessible` from an unnamed module — a JPMS check on the REFLECTIVE
// caller, which a Rust native does not go through. So there is no need for a
// curated table: the data is in the image this VM already boots from, all 1158
// locales of it, and the job is to read it.
//
// The values are decoded into RUST strings and cached, rather than caching Java
// refs: a cached `ObjectRef` is a stale pointer one moving collection later.
// ---------------------------------------------------------------------------

/// One value out of a CLDR bundle's `getContents()` row. The generated tables
/// carry a `String` or a `String[]` for every key `java.text` reads; the
/// `String[][]` rows (`TimeZoneNames`) are deliberately not modelled — see
/// `decode_cldr_value`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CldrValue {
    Str(String),
    Arr(Vec<String>),
}

/// A whole bundle family flattened for one locale: the CLDR parent chain
/// merged least-specific-first, so an inherited key still resolves.
type CldrTable = std::sync::Arc<std::collections::BTreeMap<String, CldrValue>>;

/// `(simple-name, language, country)` → the merged table, or `None` when not a
/// single candidate class loaded. The `None` is cached too: a miss costs a
/// `find_resource` probe per candidate and there is no point repeating it.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two acquisition sites, both in
/// `cldr_table_for`: the hit check, whose innermost body is a bare `return`,
/// and the store. The `ctx.find_resource` probing that builds the table runs
/// between them, after the read guard has been dropped.
fn cldr_cache() -> &'static cratonvm_types::lock_order::OrderedMutex<
    std::collections::HashMap<String, Option<CldrTable>>,
> {
    static INSTANCE: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<
            std::collections::HashMap<String, Option<CldrTable>>,
        >,
    > = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The two packages that hold a `LocaleData` base name's CLDR classes: the
/// base one in `java.base` (ROOT + `_en` + `_en_US_POSIX`) and the `ext` one in
/// `jdk.localedata` (every other locale). Enumerated from the live jimage, not
/// guessed.
///
/// **Any** `sun.text.resources.*` / `sun.util.resources.*` base name maps here,
/// including the legacy JRE-package ones. That is deliberate: CratonVM's
/// `LocaleProviderAdapter` selection asks for `sun.text.resources.FormatData`
/// (the JRE package) where HotSpot's CLDR default asks for
/// `sun.text.resources.cldr.FormatData` — measured with
/// `CRATONVM_DBG_CATALINA=1`, six calls, all the JRE name. HotSpot's answer is
/// the oracle and HotSpot answers from CLDR, so both names resolve to the CLDR
/// classes rather than to `jdk.localedata`'s older JRE-package copies.
fn cldr_packages(base_name: &str) -> Option<(&'static str, &'static str)> {
    if base_name.starts_with("sun.text.resources.") {
        Some(("sun/text/resources/cldr", "sun/text/resources/cldr/ext"))
    } else if base_name.starts_with("sun.util.resources.") {
        Some(("sun/util/resources/cldr", "sun/util/resources/cldr/ext"))
    } else {
        None
    }
}

/// The bundle families that genuinely live OUTSIDE the `cldr` packages.
///
/// [`cldr_packages`] maps EVERY `sun.text.resources.*` base name onto the CLDR
/// package pair on purpose, and its own doc comment says why: this VM's
/// adapter selection asks for the legacy JRE name where HotSpot's CLDR default
/// asks for the `cldr` one, and HotSpot's answer is the oracle. That is right
/// for the families CLDR re-generated.
///
/// A few were never re-generated, because their data is not CLDR's, and for
/// those the blanket mapping makes every candidate miss and the family read as
/// absent. Verified against a JDK 25.0.3 image with `jimage list`, not assumed:
///
/// ```text
///   java.base       sun/text/resources/BreakIteratorInfo.class
///                   sun/text/resources/{Word,Line,Sentence}BreakIteratorData
///   jdk.localedata  sun/text/resources/ext/BreakIteratorInfo_th.class
///                   sun/text/resources/ext/{Word,Line}BreakIteratorData_th
/// ```
///
/// and no `sun/text/resources/cldr/BreakIteratorInfo` of either. `CollationData`
/// is the same shape and is the reason [`cldr_collation_rule`] had to hand-roll
/// its own probe rather than call [`load_cldr_table`]; it is left alone here so
/// that working reader keeps its cache and its most-specific-first order, which
/// differ from this one's merge.
fn non_cldr_packages(simple: &str) -> Option<(&'static str, &'static str)> {
    match simple {
        "BreakIteratorInfo" => Some(("sun/text/resources", "sun/text/resources/ext")),
        _ => None,
    }
}

/// Class-name suffixes for a locale, LEAST specific first, so a later merge
/// overrides an earlier one — the JDK parent chain flattened into one map, the
/// same shape `build_locale_chain` uses for `.properties`.
///
/// **Not modelled:** CLDR's non-truncating parent locales (`en_GB` → `en_001` →
/// `en`, `es_MX` → `es_419` → `es`), which live in
/// `CLDRBaseLocaleDataMetaInfo`, and the script subtag (`sr_Latn`, `zh_Hans`).
/// Both degrade to the language bundle, which is the right *language* with the
/// wrong regional variant — visibly closer than en, and named here so the next
/// reader does not have to rediscover it.
fn cldr_suffixes(lang: &str, country: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    if !lang.is_empty() {
        out.push(format!("_{lang}"));
        if !country.is_empty() {
            out.push(format!("_{lang}_{country}"));
        }
    }
    out
}

/// Decode one `getContents()` value. `None` means "a shape this table does not
/// model", and the caller then drops the whole row rather than storing a row of
/// empty strings that would read downstream as real data.
fn decode_cldr_value(ctx: &mut dyn NativeContext, val: ObjectRef) -> Option<CldrValue> {
    if ctx.heap_kind_of(val) != cratonvm_types::ObjectKind::Array {
        return ctx.read_string(val).map(CldrValue::Str);
    }
    if ctx.heap_element_type_of(val) != cratonvm_types::ArrayElementType::Reference {
        return None;
    }
    let len = ctx.array_length(val);
    let mut items = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(val, i) {
            Value::Object(Some(e)) => {
                // A nested array is `TimeZoneNames`' `String[][]`. Those bundles
                // take the real-class path (`needs_concrete_bundle_class`), so
                // flattening them here would be inventing data.
                if ctx.heap_kind_of(e) == cratonvm_types::ObjectKind::Array {
                    return None;
                }
                items.push(ctx.read_string(e).unwrap_or_default());
            }
            // A null slot is a real CLDR value (the unused day-period entries of
            // `AmPmMarkers`), and the JDK stores it as an empty string.
            _ => items.push(String::new()),
        }
    }
    Some(CldrValue::Arr(items))
}

/// Instantiate one CLDR bundle class and merge its rows into `out`.
/// Returns whether anything was read.
fn read_cldr_contents(
    ctx: &mut dyn NativeContext,
    class_internal_name: &str,
    out: &mut std::collections::BTreeMap<String, CldrValue>,
) -> bool {
    let bundle = match ctx.new_object_initialized(class_internal_name, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        // A candidate whose `.class` resource exists but which will not
        // instantiate is not fatal: the chain simply keeps whatever the
        // less-specific candidates already merged.
        _ => return false,
    };
    let contents = match ctx.invoke_virtual(bundle, "getContents", "()[[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return false,
    };
    // GC: from here to the end of the loop nothing allocates on the Java heap —
    // `array_length`, `get_array_element`, `heap_kind_of` and `read_string` are
    // all reads — so the raw `contents` ref cannot go stale under a moving
    // collection. The `new_object_initialized` for the NEXT candidate happens
    // after this function has already copied everything into Rust strings.
    let rows = ctx.array_length(contents);
    let mut added = 0usize;
    for i in 0..rows {
        let Value::Object(Some(row)) = ctx.get_array_element(contents, i) else {
            continue;
        };
        if ctx.array_length(row) < 2 {
            continue;
        }
        let key = match ctx.get_array_element(row, 0) {
            Value::Object(Some(k)) => match ctx.read_string(k) {
                Some(s) => s,
                None => continue,
            },
            _ => continue,
        };
        let Value::Object(Some(val)) = ctx.get_array_element(row, 1) else {
            continue;
        };
        if let Some(decoded) = decode_cldr_value(ctx, val) {
            out.insert(key, decoded);
            added += 1;
        }
    }
    added > 0
}

/// The merged CLDR table for `base_name` at `(lang, country)`, or `None` when
/// the image has no candidate class at all.
///
/// A `None` is the ONLY path that silently degrades to the curated en tables,
/// so it announces itself once per (family, locale) rather than leaving a
/// reader to infer an en fallback from English output on a Russian host.
fn load_cldr_table(
    ctx: &mut dyn NativeContext,
    base_name: &str,
    lang: &str,
    country: &str,
) -> Option<CldrTable> {
    let simple = base_name.rsplit('.').next().unwrap_or_default();
    if simple.is_empty() {
        return None;
    }
    // The non-CLDR families first: for them `cldr_packages`' blanket mapping
    // names two packages the image does not have.
    let (root_pkg, ext_pkg) = match non_cldr_packages(simple) {
        Some(pair) => pair,
        None => cldr_packages(base_name)?,
    };
    let cache_key = format!("{simple}|{lang}|{country}");
    if let Ok(cache) = cldr_cache().lock() {
        if let Some(hit) = cache.get(&cache_key) {
            return hit.clone();
        }
    }

    let mut merged: std::collections::BTreeMap<String, CldrValue> =
        std::collections::BTreeMap::new();
    let mut loaded_any = false;
    for suffix in cldr_suffixes(lang, country) {
        // Exactly one of the two packages holds any given candidate (ROOT/_en
        // in java.base, everything else in jdk.localedata), so the order of
        // this inner probe does not matter.
        let mut found: Option<String> = None;
        for pkg in [root_pkg, ext_pkg] {
            let cand = format!("{pkg}/{simple}{suffix}");
            if ctx.find_resource(&format!("{cand}.class")).is_some() {
                found = Some(cand);
                break;
            }
        }
        let Some(cand) = found else { continue };
        if read_cldr_contents(ctx, &cand, &mut merged) {
            loaded_any = true;
        }
    }

    let table: Option<CldrTable> = if loaded_any {
        Some(std::sync::Arc::new(merged))
    } else {
        // Warn only for the families that are expected to answer. The other
        // `sun.*.resources.*` base names (`CollationData`, …) legitimately have
        // no `cldr` package at all, and a warning on those would be crying
        // wolf — which is how a real fallback notice gets filtered out of a
        // log.
        //
        // `BreakIteratorInfo` USED to be the first example in that sentence and
        // is now in the list below instead, because `non_cldr_packages` routes
        // it to the packages the image actually has. The sentence was right
        // about the old behaviour and would now suppress the warning for the
        // one family whose miss means the probe is broken.
        // `BreakIteratorInfo` is in the list because its ROOT bundle is in
        // `java.base`: unlike the `cldr` families, a jlinked image that dropped
        // `jdk.localedata` still has it, so a total miss is not the ordinary
        // degradation this warning exists to announce — it means the probe is
        // wrong.
        if matches!(
            simple,
            "FormatData" | "CurrencyNames" | "LocaleNames" | "BreakIteratorInfo"
        ) {
            tracing::warn!(
                family = simple,
                language = lang,
                country = country,
                "W7-80: no CLDR bundle class in the JDK image for this locale family; \
                 falling back to CratonVM's curated en locale data. Formatting will be \
                 English regardless of the reported locale."
            );
        }
        None
    };
    if let Ok(mut cache) = cldr_cache().lock() {
        cache.insert(cache_key, table.clone());
    }
    table
}

/// `sun.util.locale.provider.LocaleResources.getNumberStrings`, reproduced:
/// the number tables are keyed by numbering system, so read
/// `<DefaultNumberingSystem>.<kind>` first, then `latn.<kind>`, then the bare
/// `<kind>`. Getting this wrong is silent — every locale would fall through to
/// the bare key, which CLDR does not define, and the whole number surface would
/// stay en while looking like it had been made locale-aware.
fn cldr_number_strings(table: &CldrTable, kind: &str) -> Option<Vec<String>> {
    if let Some(CldrValue::Str(ns)) = table.get("DefaultNumberingSystem") {
        if let Some(CldrValue::Arr(v)) = table.get(&format!("{ns}.{kind}")) {
            return Some(v.clone());
        }
    }
    if let Some(CldrValue::Arr(v)) = table.get(&format!("latn.{kind}")) {
        return Some(v.clone());
    }
    match table.get(kind) {
        Some(CldrValue::Arr(v)) => Some(v.clone()),
        _ => None,
    }
}

/// One slot of a CLDR `NumberElements` array, or `None` when the array is
/// absent or the slot is the empty string CLDR uses for "not set here".
fn number_element(elems: &Option<Vec<String>>, index: usize) -> Option<&str> {
    elems
        .as_ref()
        .and_then(|v| v.get(index))
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

/// The same slot as a single `char`, with the caller's fallback. CLDR stores
/// these as strings because a few are multi-character (`minusSignText`,
/// `percentText`); `DecimalFormatSymbols`' char-valued setters take the first
/// character, which is what the JDK's own `findNonFormatChar` reduces to for
/// every locale in the image.
fn number_element_char(elems: &Option<Vec<String>>, index: usize, fallback: char) -> char {
    number_element(elems, index)
        .and_then(|s| s.chars().next())
        .unwrap_or(fallback)
}

/// A `String[]` value straight out of the table.
fn cldr_arr<'t>(table: &'t CldrTable, key: &str) -> Option<&'t Vec<String>> {
    match table.get(key) {
        Some(CldrValue::Arr(v)) => Some(v),
        _ => None,
    }
}

/// `(language, country)` of the `locale` field on a receiver that carries one —
/// `sun.util.locale.provider.LocaleResources` for the `getNumberPatterns` /
/// `getDecimalFormatSymbolsData` / `getDateTimePattern` overrides. Empty
/// strings when the field or the accessors yield nothing, which resolves to the
/// CLDR ROOT bundle.
fn receiver_locale(ctx: &mut dyn NativeContext, this: Option<&Value>) -> (String, String) {
    let Some(Value::Object(Some(this))) = this else {
        return (String::new(), String::new());
    };
    let Value::Object(Some(loc)) = ctx.get_field_by_name(*this, "locale") else {
        return (String::new(), String::new());
    };
    let (lang, country, _variant) = decompose_locale(ctx, loc);
    (lang, country)
}

/// `(language, country)` of a `Locale` argument.
fn arg_locale(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> (String, String) {
    match arg {
        Some(Value::Object(Some(loc))) => {
            let (lang, country, _variant) = decompose_locale(ctx, *loc);
            (lang, country)
        }
        _ => (String::new(), String::new()),
    }
}

/// A locale display NAME out of the JDK image's own CLDR `LocaleNames` bundles.
///
/// `code` is the thing being named — a language subtag (`"tr"`) or a region
/// subtag (`"US"`) — and `display_*` is the locale to name it IN. CLDR keys
/// both kinds into one bundle, languages lowercase and regions uppercase, so
/// one lookup serves `getDisplayLanguage` and `getDisplayCountry` alike.
///
/// This is the table H2's `CompareMode.getName` round-trips through: it asks
/// every collation locale for its ENGLISH display name and matches the answer
/// against the requested collation. Returning the subtag instead — which is
/// what the display-name overrides in `locale_bootstrap.rs` used to do, on the
/// reasoning that any caller "just wants a non-null human-readable string" —
/// makes `SET COLLATION TURKISH` unresolvable, because nothing in the table
/// ever answers `TURKISH`.
pub(crate) fn cldr_locale_display_name(
    ctx: &mut dyn NativeContext,
    code: &str,
    display_lang: &str,
    display_country: &str,
) -> Option<String> {
    if code.is_empty() {
        return None;
    }
    let table = load_cldr_table(
        ctx,
        "sun.util.resources.cldr.LocaleNames",
        display_lang,
        display_country,
    )?;
    match table.get(code) {
        Some(CldrValue::Str(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
    }
}

/// The collation TAILORING for a locale — the rule fragment the JDK's
/// `CollatorProviderImpl` concatenates onto `CollationRules.DEFAULTRULES`.
///
/// This family does NOT live under a `cldr` package, so `load_cldr_table`
/// cannot reach it: the real classes are `sun/text/resources/ext/CollationData_XX`
/// (47 of them in a JDK 25 image, `_tr` and `_da` and `_sv` among them). Same
/// reader, different package, and no ROOT candidate — an absent tailoring means
/// "the default rules are already right for this locale", which is true for
/// English and most others, so returning `None` is the correct answer rather
/// than a fallback.
///
/// Without this the synthetic `CollationData` bundle is EMPTY, the provider
/// reads `""` for its rules, and every locale gets a collator built from the
/// default rules alone — `Collator.getInstance(new Locale("tr"))` really is a
/// `java.text.RuleBasedCollator` on this VM, it just is not a Turkish one.
pub(crate) fn cldr_collation_rule(
    ctx: &mut dyn NativeContext,
    lang: &str,
    country: &str,
) -> Option<String> {
    if lang.is_empty() {
        return None;
    }
    // Cache like `load_cldr_table` does, and for the same reason: the rule
    // string is read by instantiating the bundle class and walking its
    // `getContents()` array, which is far too expensive to repeat. A collation
    // rule is asked for once per `Collator.getInstance`, and callers that build
    // one per comparison are common.
    let cache_key = format!("CollationRule|{lang}|{country}");
    if let Ok(cache) = collation_rule_cache().lock() {
        if let Some(hit) = cache.get(&cache_key) {
            return hit.clone();
        }
    }
    let found = cldr_collation_rule_uncached(ctx, lang, country);
    if let Ok(mut cache) = collation_rule_cache().lock() {
        cache.insert(cache_key, found.clone());
    }
    found
}

fn collation_rule_cache() -> &'static cratonvm_types::lock_order::OrderedMutex<
    std::collections::HashMap<String, Option<String>>,
> {
    static INSTANCE: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn cldr_collation_rule_uncached(
    ctx: &mut dyn NativeContext,
    lang: &str,
    country: &str,
) -> Option<String> {
    // Most specific first: a `_tr_TR` tailoring wins over `_tr`. Unlike the
    // CLDR chain there is nothing to merge — a bundle either carries the whole
    // `Rule` for that locale or does not exist.
    let mut candidates = Vec::new();
    if !country.is_empty() {
        candidates.push(format!(
            "sun/text/resources/ext/CollationData_{lang}_{country}"
        ));
    }
    candidates.push(format!("sun/text/resources/ext/CollationData_{lang}"));
    for cand in candidates {
        if ctx.find_resource(&format!("{cand}.class")).is_none() {
            continue;
        }
        let mut merged: std::collections::BTreeMap<String, CldrValue> =
            std::collections::BTreeMap::new();
        if !read_cldr_contents(ctx, &cand, &mut merged) {
            continue;
        }
        if let Some(CldrValue::Str(rule)) = merged.get("Rule") {
            if !rule.is_empty() {
                return Some(rule.clone());
            }
        }
    }
    None
}

/// The FormatData table for a locale. One name for the base string so the six
/// call sites cannot drift apart on it.
/// The merged `BreakIteratorInfo` bundle for a locale.
///
/// Root + `_<lang>` + `_<lang>_<country>`, least specific first, so `_th`'s
/// `WordData = WordBreakIteratorData_th` overrides the root's while the root's
/// `BreakIteratorClasses` still resolves for a locale that has no override.
/// That merge is [`load_cldr_table`]'s, and it is the right one here for the
/// same reason it is right for `FormatData`: the JDK's own parent chain.
fn break_iterator_info(
    ctx: &mut dyn NativeContext,
    lang: &str,
    country: &str,
) -> Option<CldrTable> {
    load_cldr_table(ctx, "sun.text.resources.BreakIteratorInfo", lang, country)
}

fn cldr_format_data(ctx: &mut dyn NativeContext, lang: &str, country: &str) -> Option<CldrTable> {
    load_cldr_table(ctx, "sun.text.resources.cldr.FormatData", lang, country)
}

/// The display symbol for an ISO 4217 code in a locale.
///
/// CLDR keys `CurrencyNames` with the UPPERCASE code for the SYMBOL and the
/// lowercase code for the display NAME — the opposite of what
/// `populate_currency_names_en` in this file assumed. Measured against the live
/// bundles rather than inferred: `CurrencyNames_ru` carries `RUB=₽` and
/// `rub=российский рубль`, and root `CurrencyNames` carries only `EUR`/`JPY`/
/// `USD`, so an unlisted code correctly falls through.
///
/// Order: real CLDR ▸ this file's curated table ▸ the JDK's documented last
/// resort (the code itself). The curated middle step is what keeps
/// `getCurrencyInstance(Locale.US)` rendering `$` on an image that shipped
/// without `jdk.localedata`.
pub(crate) fn cldr_currency_symbol(
    ctx: &mut dyn NativeContext,
    code: &str,
    lang: &str,
    country: &str,
) -> String {
    if !code.is_empty() {
        if let Some(table) =
            load_cldr_table(ctx, "sun.util.resources.cldr.CurrencyNames", lang, country)
        {
            if let Some(CldrValue::Str(sym)) = table.get(code) {
                if !sym.is_empty() {
                    return sym.clone();
                }
            }
        }
    }
    match code {
        "USD" => "$".to_string(),
        "EUR" => "\u{20AC}".to_string(),
        "GBP" => "\u{00A3}".to_string(),
        "JPY" => "\u{00A5}".to_string(),
        "CNY" => "\u{00A5}".to_string(),
        "CHF" => "CHF".to_string(),
        "CAD" => "$".to_string(),
        "AUD" => "$".to_string(),
        _ => code.to_string(),
    }
}

/// The display NAME for an ISO 4217 code in a locale — `cldr_currency_symbol`'s
/// twin, and deliberately written against the same table so the two cannot
/// drift apart the way the curated copies did.
///
/// CLDR keys `CurrencyNames` with the UPPERCASE code for the SYMBOL and the
/// lowercase code for the display NAME. That convention is already recorded on
/// `cldr_currency_symbol`; this is the other half of it, and until now nothing
/// read it — `Currency.getDisplayName` ran real bytecode, which reaches the
/// name through `LocaleServiceProviderPool` + `CurrencyNameProvider`, an SPI
/// this VM does not serve. The pool answered null and the JDK's documented last
/// resort took over, so every currency answered its own CODE.
///
/// MEASURED against HotSpot 25.0.4+7 (`apps/probes/CurrencyNameProbe`):
///
/// ```text
/// Currency.getInstance("USD").getDisplayName(Locale.ENGLISH)
///   HotSpot   US Dollar
///   was       USD
/// ```
///
/// The synthetic bundle already HELD the answer — `getString("usd")` returns
/// "US Dollar" on this VM today — which is what makes this a plumbing gap
/// rather than a data one, and what makes the bridge cheap.
///
/// DEBT, and worth naming as such: the right end state is the provider pool
/// working, so `Currency` needs no bridge at all. This adds a shadow while the
/// campaign is retiring them. It is the same trade `getSymbol(Locale)` and the
/// whole `Locale.getDisplay*` family already made in this file, and it buys a
/// right answer where real bytecode gives a wrong one. Whoever makes
/// `LocaleServiceProviderPool` serve `CurrencyNameProvider` should delete both
/// halves together.
///
/// The curated fallback is measured, not guessed — the eight entries are
/// HotSpot's own answers for `Locale.ENGLISH`, and they are what an image
/// shipped without `jdk.localedata` falls back to. The last resort is the JDK's
/// own: the currency code.
pub(crate) fn cldr_currency_display_name(
    ctx: &mut dyn NativeContext,
    code: &str,
    lang: &str,
    country: &str,
) -> String {
    if !code.is_empty() {
        if let Some(table) =
            load_cldr_table(ctx, "sun.util.resources.cldr.CurrencyNames", lang, country)
        {
            if let Some(CldrValue::Str(name)) = table.get(&code.to_lowercase()) {
                if !name.is_empty() {
                    return name.clone();
                }
            }
        }
    }
    match code {
        "USD" => "US Dollar",
        "EUR" => "Euro",
        "GBP" => "British Pound",
        "JPY" => "Japanese Yen",
        "CNY" => "Chinese Yuan",
        "CHF" => "Swiss Franc",
        "CAD" => "Canadian Dollar",
        "AUD" => "Australian Dollar",
        _ => code,
    }
    .to_string()
}

/// `Currency.getSymbol()`'s locale: the real no-arg body is
/// `getSymbol(Locale.getDefault(Locale.Category.DISPLAY))`, so the symbol a
/// bare `getSymbol()` returns is a function of the host locale — `RUB` renders
/// `₽` on a ru host and the bare code `RUB` on an en one.
///
/// `getDefault()` rather than `getDefault(DISPLAY)` on purpose: W7-67 §2 records
/// that the base property is seeded FROM the display value and that
/// `user.language.display` is never created from platform values (the JDK's own
/// condition for writing it is dead), so the two are the same locale on every
/// host — and the no-arg form is one call instead of two plus a `Category`
/// mirror lookup.
pub(crate) fn currency_symbol_for_default_locale(
    ctx: &mut dyn NativeContext,
    code: &str,
) -> String {
    let display = match ctx.invoke(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(loc)))) => Some(loc),
        _ => None,
    };
    let (lang, country) = match display {
        Some(loc) => {
            let (lang, country, _variant) = decompose_locale(ctx, loc);
            (lang, country)
        }
        None => (String::new(), String::new()),
    };
    cldr_currency_symbol(ctx, code, &lang, &country)
}

/// `Currency.getDisplayName()`'s locale, on the same reasoning (and the same
/// `getDefault()`-not-`getDefault(DISPLAY)` shortcut) as
/// [`currency_symbol_for_default_locale`], which see.
pub(crate) fn currency_display_name_for_default_locale(
    ctx: &mut dyn NativeContext,
    code: &str,
) -> String {
    let display = match ctx.invoke(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(loc)))) => Some(loc),
        _ => None,
    };
    let (lang, country) = match display {
        Some(loc) => {
            let (lang, country, _variant) = decompose_locale(ctx, loc);
            (lang, country)
        }
        None => (String::new(), String::new()),
    };
    cldr_currency_display_name(ctx, code, &lang, &country)
}

/// Overlay the JDK image's own CLDR data for `(lang, country)` onto a synthetic
/// bundle's backing map, on TOP of whatever curated en baseline was written
/// first. Overlay rather than replace, so a key CLDR does not carry keeps the
/// value this file has been serving — this change can add correctness but it
/// cannot take a key away.
///
/// Returns whether any CLDR data was found.
fn overlay_cldr_bundle(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    base_name: &str,
    lang: &str,
    country: &str,
) -> bool {
    // WHITELIST, not "every family CLDR has". `CalendarData` is the reason:
    // its CLDR `firstDayOfWeek` is not the plain `"1"` this file's curated
    // table writes, it is a country-list string —
    //   "1: AG AS BD BR …;2: 001 AD AE …;6: MV;7: AE AF BH …"
    // — which the real `CLDRCalendarDataProviderImpl` parses per region.
    // Overlaying it would replace a value a consumer reads as an integer with
    // one it cannot parse, and the failure would surface far from here. The
    // same caution covers `TimeZoneNames`, whose values are `String[][]` and
    // which already has its own real-class path.
    if !matches!(
        base_name.rsplit('.').next().unwrap_or_default(),
        "FormatData" | "CurrencyNames" | "LocaleNames"
    ) {
        return false;
    }
    let Some(table) = load_cldr_table(ctx, base_name, lang, country) else {
        return false;
    };
    let map_pin = ctx.pin_native_root(map);
    for (key, value) in table.iter() {
        match value {
            CldrValue::Str(s) => put_str(ctx, map_pin, map, key, s),
            CldrValue::Arr(items) => {
                let refs: Vec<&str> = items.iter().map(String::as_str).collect();
                put_arr(ctx, map_pin, map, key, &refs);
            }
        }
    }

    // The legacy 9-slot `DateTimePatterns` (4 time + 4 date + 1 combiner) is
    // the shape `populate_format_data_en` writes and the shape this file's
    // consumers read. CLDR splits it into `TimePatterns` / `DatePatterns` /
    // a 4-slot `DateTimePatterns` combiner, and the overlay above has just
    // replaced the 9-slot array with the 4-slot combiner. Rebuild the legacy
    // shape from the real per-locale patterns so the key keeps its meaning
    // instead of silently shrinking to four entries.
    let time = cldr_arr(&table, "TimePatterns").cloned();
    let date = cldr_arr(&table, "DatePatterns").cloned();
    if let (Some(time), Some(date)) = (time, date) {
        if time.len() >= 4 && date.len() >= 4 {
            let combiner = cldr_arr(&table, "DateTimePatterns")
                .and_then(|c| c.first().cloned())
                .unwrap_or_else(|| "{1} {0}".to_string());
            let nine: Vec<&str> = time[..4]
                .iter()
                .chain(date[..4].iter())
                .map(String::as_str)
                .chain(std::iter::once(combiner.as_str()))
                .collect();
            put_arr(ctx, map_pin, map, "DateTimePatterns", &nine);
            put_arr(ctx, map_pin, map, "gregorian.DateTimePatterns", &nine);
        }
    }
    ctx.unpin_native_roots(map_pin);
    true
}

/// Build a synthetic `ResourceBundle` for `bundle_name`. Always returns
/// non-null. For user `.properties` resources on the classpath we
/// populate from the file. For known JDK locale-data base names we
/// pre-populate English/US defaults. Unknown names get an empty bundle.
fn build_bundle(
    ctx: &mut dyn NativeContext,
    bundle_name: &str,
    lang: &str,
    country: &str,
) -> Result<ObjectRef, MethodCallFailed> {
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
            let mut map_now = ctx.read_native_pin(map_pin, map);
            populate_format_data_en(ctx, &mut map_now);
        } else if bundle_name.starts_with("sun.util.resources.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.cldr.LocaleNames")
            || bundle_name.starts_with("sun.util.resources.ext.LocaleNames")
        {
            let mut map_now = ctx.read_native_pin(map_pin, map);
            populate_locale_names_en(ctx, &mut map_now);
        } else if bundle_name.starts_with("sun.util.resources.CalendarData")
            || bundle_name.starts_with("sun.util.resources.cldr.CalendarData")
        {
            // The image's own CalendarData FIRST. Its week rules are a
            // region-keyed table that the curated fallback cannot express --
            // see `populate_calendar_data_from_cldr`.
            if !populate_calendar_data_from_cldr(ctx, map_pin, map, lang, country) {
                let mut map_now = ctx.read_native_pin(map_pin, map);
                populate_calendar_data_en(ctx, &mut map_now);
            }
        } else if bundle_name.starts_with("sun.util.resources.CurrencyNames")
            || bundle_name.starts_with("sun.util.resources.cldr.CurrencyNames")
        {
            let map_now = ctx.read_native_pin(map_pin, map);
            populate_currency_names_en(ctx, map_now);
        } else if bundle_name.starts_with("sun.text.resources.CollationData")
            || bundle_name.starts_with("sun.text.resources.ext.CollationData")
        {
            // `LocaleResources.getCollationData()` reads exactly one key from
            // this bundle and hands it to `new RuleBasedCollator(DEFAULTRULES +
            // rule)`. An empty bundle is not a degraded collator, it is the
            // wrong language's collator with no way for the caller to tell.
            if let Some(rule) = cldr_collation_rule(ctx, lang, country) {
                crate::phases_late::text_intl::put_str(ctx, map_pin, map, "Rule", &rule);
            }
        }

        // W7-80: overlay the JDK image's own CLDR data for the REQUESTED
        // locale on top of the curated en baseline just written. This is the
        // whole of stage 2 for the `DateFormatSymbols` surface —
        // `DateFormatSymbols.initializeData` reads `MonthNames`,
        // `MonthAbbreviations`, `AmPmMarkers`, `DayNames`, `DayAbbreviations`,
        // `Eras` and `DateTimePatternChars` out of exactly this bundle, and
        // `java.util.Formatter`'s `%tb` (the first conversion in
        // `SimpleFormatter`'s default pattern) reads `getShortMonths()` from
        // the result.
        //
        // A curated fallback stays underneath rather than being deleted: it
        // costs one map fill per bundle and it means an image without
        // `jdk.localedata` (a jlinked runtime that dropped it) degrades to
        // today's behaviour instead of to an empty bundle.
        let map_now = ctx.read_native_pin(map_pin, map);
        overlay_cldr_bundle(ctx, map_now, bundle_name, lang, country);
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
    name.ends_with(".TimeZoneNames") || name.ends_with(".LocaleNames")
}

/// `.LocaleNames` was added 2026-09-11 (wave 6) and the paragraph above said
/// why it was not there: "nothing in the failing test exercises their cast".
/// Something does now.
///
/// `apps/probes/L1LocaleProviderWorkload` asks for
/// `Locale.getDisplayVariant` and `getDisplayScript`, and both land in
/// `LocaleData.getLocaleNames`, whose `checkcast` to
/// `sun/util/resources/OpenListResourceBundle` a curated
/// `java/util/ResourceBundle` cannot satisfy:
///
/// ```text
///   N.displayVariant  Valencian -> THREW java.lang.ClassCastException
///       class java.util.ResourceBundle cannot be cast to class
///       sun.util.resources.OpenListResourceBundle
/// ```
///
/// The other half of that paragraph -- "the real CLDR display names live in
/// `sun.util.resources.cldr.ext.*`, which the simple locale-candidate chain
/// does not reach" -- was true of the chain and is what
/// [`concrete_bundle_chain`] fixes. `jimage list` on the JDK 25.0.3 image:
///
/// ```text
///   sun/util/resources/LocaleNames.class            (legacy root)
///   sun/util/resources/cldr/LocaleNames.class
///   sun/util/resources/cldr/LocaleNames_en.class
///   sun/util/resources/cldr/ext/LocaleNames_ca.class
/// ```
///
/// `CurrencyNames` has the same shape and is deliberately NOT added: it
/// measured `+38` armed on the same probe, so its curated path is doing work
/// the class bundles would have to replace, and that is its own measurement.
fn concrete_bundle_chain(
    bundle_name: &str,
    chain: &[(String, String, String)],
) -> Vec<(String, String, String)> {
    let Some((root_pkg, ext_pkg)) = cldr_packages(bundle_name) else {
        return chain.to_vec();
    };
    let root_pkg = root_pkg.replace('/', ".");
    let ext_pkg = ext_pkg.replace('/', ".");
    let mut out: Vec<(String, String, String)> = chain.to_vec();
    // LEAST specific first, and appended AFTER the plain candidates on
    // purpose: `try_class_bundle` links each existing candidate as the child
    // of the previous one and returns the last, so the CLDR bundle ends up
    // the most specific and the legacy root becomes its parent rather than
    // its replacement.
    for (cand, lang, country) in chain {
        let Some(simple) = cand.rsplit('.').next() else {
            continue;
        };
        for pkg in [root_pkg.as_str(), ext_pkg.as_str()] {
            out.push((format!("{pkg}.{simple}"), lang.clone(), country.clone()));
        }
    }
    out
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
/// `true` when the code that called `ResourceBundle.getBundle` is application
/// code rather than the JDK's own.
///
/// `getBundle` is CALLER-SENSITIVE: it resolves against the caller's module.
/// `sun.util.resources.*` and friends live in `java.base` and are not exported,
/// so an unnamed-module caller cannot see them and HotSpot answers
/// `MissingResourceException` -- while `java.base`'s own code loads them fine.
/// This VM had no such distinction and handed its synthesized locale bundle to
/// everyone.
///
/// MEASURED (`apps/probes/CurrencyNameProbe`, HotSpot 25.0.4+7, from an
/// ordinary classpath class):
///
/// ```text
/// getBundle sun.util.resources.CurrencyNames        MissingResourceException
/// getBundle sun.util.resources.LocaleNames          MissingResourceException
/// getBundle sun.text.resources.FormatData           MissingResourceException
/// getBundle sun.util.resources.CalendarData         MissingResourceException
/// getBundle sun.util.resources.cldr.CurrencyNames   MissingResourceException
/// getBundle com.example.NoSuchBundleAtAll           MissingResourceException  <- this one already agreed
/// ```
///
/// The last row is the tell: the refusal path already existed and was reached
/// only by names `is_jdk_internal_bundle` does not claim. All this adds is that
/// a JDK-internal name is JDK-internal to APPLICATION code too.
///
/// The synthesized bundles stay for a `java.*`/`sun.*`/`jdk.*` caller, which is
/// what keeps this VM's own locale shims working -- `populate_format_data_en`
/// exists because real `DateFormatSymbols` bytecode needs it.
///
/// `frames.last()` is the IMMEDIATE caller: `capture_stack_trace` is
/// outermost-first (see `frame_class_ids`, "innermost-first, matching
/// `capture_stack_trace`'s `.iter().rev()` convention"), which is the same
/// reading `caller_bundle_class_loader` below relies on.
fn bundle_caller_is_application(ctx: &mut dyn NativeContext) -> bool {
    let frames = ctx.capture_stack_trace(0);
    let Some(frame) = frames.last() else {
        return false;
    };
    let Some(cid) = frame.class_id else {
        return false;
    };
    let Some(name) = ctx.class_name_arc_of_id(cid) else {
        return false;
    };
    !(name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("sun/")
        || name.starts_with("jdk/")
        || name.starts_with("com/sun/"))
}

fn caller_bundle_class_loader(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let frames = ctx.capture_stack_trace(0);
    let Some(frame) = frames.last() else {
        return Ok(None);
    };
    let Some(cid) = frame.class_id else {
        return Ok(None);
    };
    let mirror = ctx.get_class_mirror(cid);
    let loader =
        match crate::lang_class::native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))])
        {
            Ok(Some(Value::Object(Some(loader)))) => loader,
            _ => return Ok(None),
        };
    // Anything other than the application-loader singleton. NOT
    // `is_user_defined_loader`: that predicate excludes a bare
    // `java.net.URLClassLoader` BY CLASS NAME for its other call sites, yet
    // `new URLClassLoader(urls, parent)` is the most ordinary way an
    // application builds an isolated loader, and HotSpot resolves a bundle
    // through it. Classes owned by the built-in loaders report `null` here or
    // the app singleton, and both keep the established `-cp` path.
    // The PLATFORM loader is built-in too, and since `Class.getClassLoader()`
    // began reporting it for platform-module classes (`java.sql`,
    // `java.scripting`, ...) a JDK caller reaches here holding it. It has no
    // application resource path of its own, so treating it as a custom loader
    // would route a bundle lookup through a loader that can only answer for the
    // image -- the opposite of what the comment above intends by "built-in
    // loaders keep the established `-cp` path".
    if crate::classloader::is_platform_loader_object(ctx, loader) {
        return Ok(None);
    }
    let app = crate::classloader::get_or_create_app_loader(ctx)?;
    if loader.as_ptr() == app.as_ptr() {
        return Ok(None);
    }
    Ok(Some(loader))
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
    // Each accessor allocates its result String, so the raw `loc` could be
    // stale by the second call. W7-80 made this the hot path for every locale
    // lookup in the file, so pin it rather than keep relying on the three
    // calls landing between collections.
    let loc_pin = ctx.pin_native_root(loc);
    let loc_now = ctx.read_native_pin(loc_pin, loc);
    let lang = match ctx.invoke_virtual(loc_now, "getLanguage", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let loc_now = ctx.read_native_pin(loc_pin, loc);
    let country = match ctx.invoke_virtual(loc_now, "getCountry", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let loc_now = ctx.read_native_pin(loc_pin, loc);
    let variant = match ctx.invoke_virtual(loc_now, "getVariant", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    ctx.unpin_native_roots(loc_pin);
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
    // Read the caller BEFORE anything allocates: this walks the live frame
    // stack, and every later use of it is past a dozen allocations.
    let caller_is_app = bundle_caller_is_application(ctx);

    // Resolve the requested locale from a Locale argument (getBundle(String,
    // Locale[, ClassLoader|Control])). Other shapes (or a Control in slot 1)
    // fall back to the ROOT chain. Keep the Locale ObjectRef too (not just its
    // decomposed strings) — `resolve_fallback_locale` below needs it to
    // consult a caller-supplied Control's `getFallbackLocale` override.
    let mut requested_locale_obj: Option<ObjectRef> = None;
    let (lang, country, variant) = match args.get(1) {
        Some(Value::Object(Some(loc))) => {
            let cid = ctx.class_id_of_object(*loc);
            if ctx.class_name_arc_of_id(cid).as_deref() == Some("java/util/Locale") {
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
    let loader = match bundle_class_loader(ctx, args) {
        Some(loader) => Some(loader),
        None => caller_bundle_class_loader(ctx)?,
    };

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
        let chain = if needs_concrete_bundle_class(&bundle_name) {
            concrete_bundle_chain(&bundle_name, &chain)
        } else {
            chain.clone()
        };
        if let Some(real) = try_class_bundle(ctx, &chain) {
            ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
            return Ok(Some(Value::Object(Some(real))));
        }
    }

    // JDK-internal base names keep the synthesized / empty-bundle fallback; a
    // genuinely-absent app bundle gets MissingResourceException
    // (java.util.ResourceBundle.getBundle's contract, which callers such as
    // Tomcat's StringManager rely on).
    // ...and JDK-internal to APPLICATION code as well, which is why the
    // caller matters. See `bundle_caller_is_application`.
    if is_jdk_internal_bundle(&bundle_name) && !caller_is_app {
        ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
        // W7-80: the requested locale reaches `build_bundle` now. It used to
        // be dropped here, which is why every locale got the same en bundle —
        // the data was never asked for a language, so no table could have been
        // locale-aware however good it was.
        let obj = build_bundle(ctx, &bundle_name, &lang, &country);
        return Ok(Some(Value::Object(Some(obj?))));
    }
    ctx.unpin_native_roots(loader_pin.unwrap_or(obj_pin));
    // 3, not 8. All four sites here write exactly one field --
    // `detailMessage`, which the throwable name map puts in slot 0 -- and
    // the synthetic table gives every unclaimed `*Exception` three slots
    // (`detailMessage`, `cause`, `suppressedExceptions`). Asking for 8 did
    // not widen the table; it only made the factory and the table disagree,
    // which is what `t9c_synthetic_field_tables_cover_their_factories`
    // reports. The JDK's own `className`/`key` fields are never populated
    // by any of these sites, so there is nothing for the extra five to hold.
    let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 3)?;
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
            let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 3)?;
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
        let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 3)?;
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
            let exc = try_alloc_concurrent_synthetic(ctx, "java/util/MissingResourceException", 3)?;
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
// in a Map, `retrieveJavaTimeFieldValueName` (singular). Both walk the JDK's
// own `LocaleServiceProviderPool` → `CalendarNameProviderImpl` chain, which
// NPEs in CratonVM's partial bootstrap, so without an override they return
// null/empty and the formatter falls back to the raw NUMERIC value — e.g.
// Spring's RFC-1123 `HttpHeaders` date formatter (built with `Locale.US`)
// renders "4, 18 12 2008 …" instead of "Thu, 18 Dec 2008 …".
//
// We answer these two statics ourselves, out of the JDK image's own CLDR
// `FormatData` for the requested locale — the same [`load_cldr_table`] reader
// W7-80 built for `DateFormatSymbols`. The curated English tables below are the
// fallback for an image that has no CLDR data at all (synthetic-JDK mode, a
// jlinked image without `jdk.localedata`), and only for English/root locales.
//
// This was English-only until 2026-08-16, and the gap it left was NOT cosmetic:
// `java.text.DateFormatSymbols.getInstance(Locale.GERMAN).getMonths()[1]`
// answered "Februar" (W7-80's path) while `MMMM` under the same locale rendered
// and parsed the NUMBER `2`, because `java.time` never reaches
// `DateFormatSymbols` — it reaches the two statics below. H2's
// `PARSEDATETIME('3. FEBRUAR 2001', 'd. MMMM yyyy', 'de')` was the witness.

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

/// True for English or the root/empty language — the only families the
/// hardcoded en/US CLDR names below are valid for, and therefore the only ones
/// allowed to reach them when the image carries no CLDR data. A missing locale
/// is treated as English (the JDK default the Spring suite runs under).
fn language_is_english(lang: &str) -> bool {
    lang.is_empty() || lang == "en"
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

/// The CLDR `FormatData` key a Calendar `(field, style)` pair resolves to,
/// mirroring `sun.util.locale.provider.CalendarNameProviderImpl.getResourceKeyFor`
/// for the CLDR adapter and the `gregory` calendar (the only one these two
/// statics answer — see [`calendar_id_is_gregorian`]). `None` for the fields and
/// styles the JDK itself does not key (`ALL_STYLES`, `YEAR`).
///
/// Read out of JDK 25's own source rather than inferred: ERA ignores the
/// standalone bit and spells its styles `long.Eras` / `Eras` / `narrow.Eras`;
/// MONTH and DAY_OF_WEEK take a `standalone.` prefix and a
/// `Names` / `Abbreviations` / `Narrows` suffix; AM_PM takes `narrow.` for
/// narrow and nothing otherwise — NOT `abbreviated.`, which the JDK reaches
/// only from `DateFormatSymbols`, never from here.
fn calendar_resource_key(field: i32, style: i32) -> Option<String> {
    let base = style & !CAL_STANDALONE_MASK;
    let standalone = if style != base { "standalone." } else { "" };
    let styled = match base {
        CAL_STYLE_SHORT => "Abbreviations",
        CAL_STYLE_NARROW => "Narrows",
        CAL_STYLE_LONG => "Names",
        _ => return None,
    };
    Some(match field {
        CAL_ERA => match base {
            CAL_STYLE_NARROW => "narrow.Eras".to_string(),
            CAL_STYLE_LONG => "long.Eras".to_string(),
            _ => "Eras".to_string(),
        },
        CAL_MONTH => format!("{standalone}Month{styled}"),
        CAL_DAY_OF_WEEK => format!("{standalone}Day{styled}"),
        CAL_AM_PM => match base {
            CAL_STYLE_NARROW => "narrow.AmPmMarkers".to_string(),
            _ => "AmPmMarkers".to_string(),
        },
        _ => return None,
    })
}

/// The localized name array for a Calendar `(field, style)` in `(lang, country)`,
/// straight out of the JDK image's CLDR `FormatData`.
///
/// Four candidate keys, in the JDK's own order. `retrieveJavaTimeFieldValueName(s)`
/// asks `CalendarNameProviderImpl` with `javatime = true` first, which prefixes
/// the key with `java.time.`, and falls back to the plain (non-javatime) call
/// when that answers null; `getDisplayNameImpl` inside each pass retries once
/// with the `standalone.` prefix stripped. The `java.time.`-prefixed keys are
/// real rows in these bundles — root `FormatData` carries `java.time.long.Eras`
/// = `[BCE, CE]` beside `long.Eras`, which is exactly the pair the two passes
/// exist to tell apart — so the prefixed lookup is not dead code, and dropping
/// it would silently serve `java.util.Calendar`'s era spelling to `java.time`.
fn cldr_calendar_name_array(
    ctx: &mut dyn NativeContext,
    lang: &str,
    country: &str,
    field: i32,
    style: i32,
) -> Option<Vec<String>> {
    let key = calendar_resource_key(field, style)?;
    let table = cldr_format_data(ctx, lang, country)?;
    let bare = key.strip_prefix("standalone.");
    let mut candidates: Vec<String> = vec![format!("java.time.{key}")];
    if let Some(bare) = bare {
        candidates.push(format!("java.time.{bare}"));
    }
    candidates.push(key.clone());
    if let Some(bare) = bare {
        candidates.push(bare.to_string());
    }
    for cand in candidates {
        if let Some(v) = cldr_arr(&table, &cand) {
            return Some(v.clone());
        }
    }
    None
}

/// True when any two entries of `names` are equal — the JDK's own
/// `CalendarNameProviderImpl.hasDuplicates`, empty slots included. Narrow month
/// and day arrays are the reason it exists: `[J, F, M, A, M, J, …]` cannot be
/// inverted into a `Map<name, value>`, so the JDK returns no map at all and the
/// caller falls back to the singular per-value lookup.
fn calendar_names_have_duplicates(names: &[String]) -> bool {
    names
        .iter()
        .enumerate()
        .any(|(i, a)| names[i + 1..].iter().any(|b| b == a))
}

/// `(name, Calendar field value)` pairs for a localized name array, mirroring
/// `CalendarNameProviderImpl.getDisplayNamesImpl`: empty slots are skipped (CLDR
/// leaves the 13th month and the unused day-period slots blank), the value base
/// is 1 for `DAY_OF_WEEK` and 0 elsewhere, and a duplicated array yields nothing
/// — except for `AM_PM`, whose flexible day-period slots legitimately repeat and
/// which the JDK exempts by name.
fn calendar_name_entries(names: &[String], field: i32) -> Vec<(String, i32)> {
    if field != CAL_AM_PM && calendar_names_have_duplicates(names) {
        return Vec::new();
    }
    let base = if field == CAL_DAY_OF_WEEK { 1 } else { 0 };
    names
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, name)| !name.is_empty())
        .map(|(i, name)| (name.clone(), base + i as i32))
        .collect()
}

/// `(name, Calendar value)` pairs for a `(field, style)` in the `Locale`
/// argument's language.
///
/// CLDR first. The curated English tables are reached only when the image
/// carries no CLDR data for the family at all *and* the language is English or
/// root — a non-English locale on such an image gets nothing, which leaves
/// `DateTimeTextProvider` on its numeric fallback rather than printing English
/// month names for German. When CLDR *does* answer, its answer stands even if
/// it is empty: an empty result there is the duplicate-narrow-names case, and
/// falling through to the English table would answer a question the JDK
/// deliberately declines.
fn locale_calendar_entries(
    ctx: &mut dyn NativeContext,
    locale_arg: Option<&Value>,
    field: i32,
    style: i32,
) -> Vec<(String, i32)> {
    let (lang, country) = arg_locale(ctx, locale_arg);
    if let Some(names) = cldr_calendar_name_array(ctx, &lang, &country, field, style) {
        return calendar_name_entries(&names, field);
    }
    if !language_is_english(&lang) {
        return Vec::new();
    }
    en_calendar_field_names(field, style)
        .map(|e| {
            e.into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect()
        })
        .unwrap_or_default()
}

/// The single localized name for one Calendar field VALUE — the singular
/// `retrieveJavaTimeFieldValueName` path, which the JDK indexes directly and
/// therefore does NOT subject to the duplicate check (it is how narrow month and
/// day names are served at all).
///
/// `DAY_OF_WEEK` is 1-based (SUNDAY=1) where the array is 0-based; every other
/// field indexes straight. A standalone style whose slot CLDR leaves empty
/// retries once at the format style, as `getDisplayNameImpl` does.
fn locale_calendar_name(
    ctx: &mut dyn NativeContext,
    locale_arg: Option<&Value>,
    field: i32,
    value: i32,
    style: i32,
) -> Option<String> {
    let (lang, country) = arg_locale(ctx, locale_arg);
    if let Some(names) = cldr_calendar_name_array(ctx, &lang, &country, field, style) {
        let index = if field == CAL_DAY_OF_WEEK {
            value - 1
        } else {
            value
        };
        if index >= 0 {
            if let Some(name) = names.get(index as usize) {
                if !name.is_empty() {
                    return Some(name.clone());
                }
            }
        }
        let base = style & !CAL_STANDALONE_MASK;
        if base != style {
            return locale_calendar_name(ctx, locale_arg, field, value, base);
        }
        return None;
    }
    if !language_is_english(&lang) {
        return None;
    }
    en_calendar_field_names(field, style)?
        .into_iter()
        .find(|(_, v)| *v == value)
        .map(|(name, _)| name.to_string())
}


/// `java.text.Normalizer`'s two null contracts, measured rather than guessed.
///
/// Both public entry points take `(CharSequence src, Normalizer.Form form)`
/// and neither declares a check; the NPEs come out of the first dereference
/// each one performs, so the ORDER is observable and `src` wins:
///
/// ```text
///   HotSpot 25.0.4+7, java.text.Normalizer
///     normalize(null, NFC)    NPE: Cannot invoke "java.lang.CharSequence.toString()" because "src" is null
///     normalize("a", null)    NPE: Cannot invoke "java.text.Normalizer$Form.ordinal()" because "form" is null
///     normalize(null, null)   NPE: ... "src" is null          <- src first
///     isNormalized(null, NFC) NPE: ... "src" is null
///     isNormalized("a", null) NPE: ... "form" is null
/// ```
///
/// Measured 2026-09-10 with `apps/probes/L1TailSweep.java`; this VM answered
/// `null`, `"a"`, `null`, `true`, `true` for those five. `java/text/` is a
/// HELD family for L1 — arming it moves `L1TailSweep` eleven rows AWAY from
/// HotSpot — so the remedy §1.4 would prefer (yield to the bytecode) is not
/// available here and the native has to carry the contract itself.
///
/// Registered TWICE (`locale_resources.rs` for the real-JDK boot,
/// `phases_late/text_intl.rs` for synthetic mode), so both call this: a
/// duplicate pair that sits half-fixed is the shape `owns_slot` exists to
/// catch, and only one of the two wins any given boot.
pub(crate) fn normalizer_reject_nulls(
    src: Option<&Value>,
    form: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    let null = |v: Option<&Value>| !matches!(v, Some(Value::Object(Some(_))));
    if null(src) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.lang.CharSequence.toString()\" because \"src\" is null"
                    .to_string(),
            ),
        }
        .into());
    }
    if null(form) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.text.Normalizer$Form.ordinal()\" because \"form\" is null"
                    .to_string(),
            ),
        }
        .into());
    }
    Ok(())
}

pub fn register(registry: &mut NativeMethodRegistry) {
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
        Ok(Some(Value::Object(Some(crate::locale_alloc(ctx, "", "")?))))
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
        |ctx, args| {
            // 13-element layout: indices 0-10 are mandatory (decimal,
            // grouping, pattern sep, percent, zero, digit, minus,
            // exponent, perMille, infinity, NaN), 11=monetary decimal
            // separator, 12=monetary grouping separator (both optional —
            // DFS falls back to the regular separators when those slots
            // are empty / the array is short).
            //
            // W7-80: answered from the JDK image's own CLDR `NumberElements`
            // for this `LocaleResources`' locale, which is where the real
            // `getDecimalFormatSymbolsData` reads it from — via
            // `getNumberStrings`, so the numbering-system prefix
            // (`latn.NumberElements`) has to be honoured or every locale falls
            // through to a key CLDR does not define. The en constants stay as
            // the fallback for an image with no `jdk.localedata`.
            let (lang, country) = receiver_locale(ctx, args.first());
            // An EMPTY language is always wrong here and was previously
            // SILENT in the worst possible way: `cldr_format_data(ctx, "", "")`
            // resolves to the ROOT bundle, which loads fine and carries a full
            // 13-element `NumberElements` -- so the lookup SUCCEEDS, no
            // fallback arm is taken, no warning fires, and every locale is
            // formatted with root (English) number symbols. Neither the W7-80
            // "no bundle" warning nor the "no NumberElements" one below can see
            // it, because nothing is missing; the wrong LOCALE was asked for.
            //
            // This is the tell to check first when a locale asked for BY NAME
            // still formats as English.
            if lang.is_empty() {
                // Two different faults produce an empty language here and they
                // need different fixes, so name which one it was rather than
                // guessing. The field SHAPE is identical on JDK 21 and 25
                // (`private final java.util.Locale locale;`, same position,
                // measured with javap), so a missing field means the receiver
                // is not the real `LocaleResources` -- not that the JDK differs.
                let field = match args.first() {
                    Some(Value::Object(Some(this))) => {
                        match ctx.get_field_by_name(*this, "locale") {
                            Value::Object(Some(loc)) => {
                                // The Locale is there. Ask it directly and
                                // report the RAW shape of the answer, because
                                // the three cases need different fixes and all
                                // three read as an empty string upstream:
                                //   * a real "" -- the Locale genuinely has no
                                //     language;
                                //   * Ok(None) -- the call returned VOID, which
                                //     is what a REFUSED native hands back to a
                                //     native caller, and is the tell that this
                                //     dispatch route did not reach bytecode;
                                //   * Err -- it threw.
                                // Bytecode callers of Locale.getLanguage() are
                                // known-correct in every mode (measured), so a
                                // gap here is a DISPATCH-ROUTE difference, not a
                                // broken accessor.
                                match ctx.invoke_virtual(
                                    loc,
                                    "getLanguage",
                                    "()Ljava/lang/String;",
                                    &[],
                                ) {
                                    Ok(Some(Value::Object(Some(sref)))) => {
                                        match ctx.read_string(sref) {
                                            Some(v) if v.is_empty() => {
                                                "getLanguage-returned-empty-string"
                                            }
                                            Some(_) => "getLanguage-OK-but-lang-empty-upstream",
                                            None => "getLanguage-unreadable-string",
                                        }
                                    }
                                    Ok(Some(Value::Object(None))) => "getLanguage-returned-null",
                                    Ok(Some(_)) => "getLanguage-returned-non-reference",
                                    Ok(None) => "getLanguage-returned-VOID-refused-native",
                                    Err(_) => "getLanguage-threw",
                                }
                            }
                            Value::Object(None) => "locale-field-null",
                            _ => "locale-field-not-a-reference",
                        }
                    }
                    _ => "no-receiver",
                };
                tracing::warn!(
                    locale_field = field,
                    "W7-80: LocaleResources receiver yielded no language, so number                      symbols resolve against the ROOT bundle and EVERY locale formats                      as English. Nothing is missing from the JDK image and this bridge                      is not the culprit -- the wrong LocaleResources was handed to it.                      `getLanguage-returned-empty-string` (the measured JDK 21                      --jdk-only case) means the receiver IS a real LocaleResources but                      for the ROOT locale: something upstream answered a non-root                      request with root resources, so look at adapter/resource                      SELECTION, not at this bridge or at the CLDR bundles.                      `getLanguage-returned-VOID-refused-native` would instead mean the                      call never reached bytecode."
                );
            }
            let cldr = cldr_format_data(ctx, &lang, &country)
                .and_then(|t| cldr_number_strings(&t, "NumberElements"));
            let outer = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 3);
            // Every allocation below this point can move `outer`; the raw ref
            // was carried across three of them before. Pin it and re-read.
            let outer_pin = ctx.pin_native_root(outer);
            let elems = match cldr {
                // `DecimalFormatSymbols.initialize` indexes 0..=10
                // unconditionally, so a short array is an AIOOBE with no
                // message — refuse to hand one over.
                Some(v) if v.len() >= 11 => {
                    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
                    make_string_array(ctx, &refs)
                }
                _ => {
                    // Substituting en here is a SILENT wrong answer: the caller
                    // gets a fully-formed 13-element array and formats every
                    // locale as English with no error at all. That is how the
                    // strict-mode `textformat` divergence stayed unexplained --
                    // the W7-80 warning in `load_cldr_table` never fires for
                    // it, because a locale that resolves to the ROOT bundle
                    // LOADS fine and only its CONTENT is English.
                    //
                    // Say it here, where the substitution actually happens, and
                    // name the locale that was asked for: an EMPTY language is
                    // the tell that the receiver's `locale` field could not be
                    // read at all, rather than that the image lacks the data.
                    tracing::warn!(
                        language = %lang,
                        country = %country,
                        "W7-80: no CLDR NumberElements for this locale; substituting \
                         CratonVM's en number symbols, so every locale will format as \
                         English. An EMPTY language here means the LocaleResources \
                         receiver's locale could not be read, NOT that the JDK image \
                         lacks the data."
                    );
                    make_string_array(
                        ctx,
                        &[
                            ".", ",", ";", "%", "0", "#", "-", "E", "\u{2030}", "\u{221E}", "NaN",
                            ".", ",",
                        ],
                    )
                }
            };
            let outer_now = ctx.read_native_pin(outer_pin, outer);
            ctx.set_array_element(outer_now, 0, Value::Object(Some(elems)));
            // Use empty strings rather than null for indices 1 and 2 so a
            // checkcast-then-getfield path can't run into a null-pointer
            // surprise via a downstream method call (DFS treats an empty
            // currency symbol as "unset" and resolves it lazily through
            // initializeCurrency()).
            let empty1 = ctx.create_string("");
            let outer_now = ctx.read_native_pin(outer_pin, outer);
            ctx.set_array_element(outer_now, 1, Value::Object(Some(empty1)));
            let empty2 = ctx.create_string("");
            let outer_now = ctx.read_native_pin(outer_pin, outer);
            ctx.set_array_element(outer_now, 2, Value::Object(Some(empty2)));
            ctx.unpin_native_roots(outer_pin);
            Ok(Some(Value::Object(Some(outer_now))))
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
    // W7-80: answered from the JDK image's own CLDR `CurrencyNames` bundle for
    // the ARGUMENT locale, which is what makes this locale-sensitive at all —
    // the previous curated table returned "$" for USD on every locale and "€"
    // for EUR on every locale, so the symbol could not disagree between en and
    // ja the way HotSpot's does (`JPY` is `¥` for ru/de and the fullwidth `￥`
    // for ja; `RUB` is `₽` for ru and the bare code `RUB` for en).
    //
    // The key convention is the opposite way round from the one this file's own
    // `populate_currency_names_en` used: in CLDR the UPPERCASE ISO code maps to
    // the SYMBOL and the lowercase code maps to the display NAME. Verified
    // against the live bundles — `CurrencyNames_ru` has `RUB=₽` and
    // `rub=российский рубль`.
    //
    // The final fallback is the JDK's own: "use the currency code as symbol of
    // last resort". The curated table is kept ahead of it so an image without
    // `jdk.localedata` still renders `$` rather than `USD`, which is the
    // behaviour `CurrencyStyleFormatterTests` / `NumberFormattingTests` need.
    // `getSymbol()` (no-arg) delegates to `getSymbol(Locale.getDefault(...))`
    // in real bytecode, so overriding only the 1-arg overload fixes both call
    // forms.
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
            let (lang, country) = arg_locale(ctx, args.get(1));
            let sym = cldr_currency_symbol(ctx, &code, &lang, &country);
            let s = ctx.create_string(&sym);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // `Currency.getDisplayName(Locale)` — see `cldr_currency_display_name` for
    // why real bytecode cannot answer this and why the bridge is debt.
    // `getDisplayName()` (no-arg) delegates to
    // `getDisplayName(Locale.getDefault(DISPLAY))` in real bytecode, so
    // overriding only the 1-arg overload fixes both call forms — the same
    // property `getSymbol` above relies on.
    registry.register(
        "java/util/Currency",
        "getDisplayName",
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
            let (lang, country) = arg_locale(ctx, args.get(1));
            let name = cldr_currency_display_name(ctx, &code, &lang, &country);
            let s = ctx.create_string(&name);
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
            // `this` was carried raw across ~15 allocating calls below (a
            // `create_string` each, plus every `invoke_virtual` into real
            // bytecode). W7-80 adds CLDR loads — which instantiate bundle
            // classes and build their whole `Object[][]` — to the same
            // sequence, so pin it and re-read before each use.
            let this_pin = ctx.pin_native_root(this);
            // The `Locale` argument goes the same way: it is read again, far
            // below, to derive the currency, by which point several bundle
            // classes have been instantiated. `unpin_native_roots(this_pin)`
            // releases both (pins release from an index onward).
            let locale_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            let locale_pin = locale_obj.map(|o| ctx.pin_native_root(o));
            // W7-80: every symbol below now comes from the JDK image's own CLDR
            // `NumberElements` for this locale — the same array the real
            // `DecimalFormatSymbols.initialize` reads, in the same index order
            // (0 decimal, 1 grouping, 2 pattern separator, 3 percent, 4 zero
            // digit, 5 digit, 6 minus, 7 exponent separator, 8 per-mille,
            // 9 infinity, 10 NaN, 11 monetary decimal, 12 monetary grouping).
            //
            // What that replaces is the CURATED 12-language separator list
            // below, which survives only as the fallback. The list was
            // introduced for a real defect (German `NumberFormat.parse("1,1")`
            // returned 11.0 — Spring DLBF customEditor/converter) and it is a
            // strict subset of what CLDR says: ru's grouping separator is
            // U+00A0 and its NaN is "не число", neither of which a list that
            // only ever chose between '.', ',' and U+202F could reach.
            //
            // Original note kept for the fallback's provenance:
            // derive the two separators that actually vary by
            // language from the locale's language tag (curated, like the
            // `getDateTimePattern` override). Without this, every locale got the
            // en/US separators and e.g. German `NumberFormat.parse("1,1")`
            // returned 11.0 instead of 1.1 (Spring DLBF customEditor/converter).
            // (dec, grp): en='.'/',' ; de=','/'.' ;
            // fr=','/' ' (narrow no-break space).
            let (lang, country) = arg_locale(ctx, args.get(1));
            let elems = cldr_format_data(ctx, &lang, &country)
                .and_then(|t| cldr_number_strings(&t, "NumberElements"))
                // Short array ⇒ ignore it entirely. Taking the first few slots
                // from CLDR and the rest from the curated list would build a
                // symbol set no locale has, which is the exact chimera
                // W7-44 declined to ship.
                .filter(|v| v.len() >= 11);
            let (curated_dec, curated_grp): (char, char) = match lang.as_str() {
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
            // `number_element` / `number_element_char` are the one accessor for
            // a CLDR slot, so an absent array cannot leave half the symbol set
            // CLDR and half curated.
            let dec_sep = number_element_char(&elems, 0, curated_dec);
            let grp_sep = number_element_char(&elems, 1, curated_grp);
            // Slots 11/12 are optional in CLDR; the JDK's own rule is "absent
            // or empty means use the plain separator", reproduced here.
            let mon_dec_sep = number_element_char(&elems, 11, dec_sep);
            let mon_grp_sep = number_element_char(&elems, 12, grp_sep);
            let pattern_sep = number_element_char(&elems, 2, ';');
            let percent_ch = number_element_char(&elems, 3, '%');
            let zero_digit = number_element_char(&elems, 4, '0');
            let digit_ch = number_element_char(&elems, 5, '#');
            let minus_ch = number_element_char(&elems, 6, '-');
            let permill_ch = number_element_char(&elems, 8, '\u{2030}');
            let exponent_text = number_element(&elems, 7).unwrap_or("E").to_string();
            let infinity_text = number_element(&elems, 9).unwrap_or("\u{221E}").to_string();
            let nan_text = number_element(&elems, 10).unwrap_or("NaN").to_string();
            // Set every public char/string field via the corresponding
            // setter so we don't depend on instance-field layout.
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setDecimalSeparator",
                "(C)V",
                &[Value::Int(dec_sep as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setGroupingSeparator",
                "(C)V",
                &[Value::Int(grp_sep as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setPatternSeparator",
                "(C)V",
                &[Value::Int(pattern_sep as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setPercent",
                "(C)V",
                &[Value::Int(percent_ch as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setZeroDigit",
                "(C)V",
                &[Value::Int(zero_digit as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ =
                ctx.invoke_virtual(this_now, "setDigit", "(C)V", &[Value::Int(digit_ch as i32)]);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setMinusSign",
                "(C)V",
                &[Value::Int(minus_ch as i32)],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setPerMill",
                "(C)V",
                &[Value::Int(permill_ch as i32)],
            );
            let exp = ctx.create_string(&exponent_text);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setExponentSeparator",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(exp))],
            );
            let inf = ctx.create_string(&infinity_text);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setInfinity",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(inf))],
            );
            let nan = ctx.create_string(&nan_text);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setNaN",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(nan))],
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setMonetaryDecimalSeparator",
                "(C)V",
                &[Value::Int(mon_dec_sep as i32)],
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
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setMonetaryGroupingSeparator",
                "(C)V",
                &[Value::Int(mon_grp_sep as i32)],
            );

            // W7-80: the currency. `$`/`USD` were hardcoded here for every
            // locale, which is why `NumberFormat.getCurrencyInstance(ru_RU)
            // .getCurrency().getCurrencyCode()` answered USD where HotSpot
            // answers RUB.
            //
            // The CODE comes from `Currency.getInstance(Locale)` — java.base's
            // own `currency.data` table, which was ALREADY correct here
            // (measured RUB / EUR / TRY / JPY before this change; the probe row
            // is `currency.ofLocale`). Nothing had to be curated for it; the
            // value was simply never asked for. `getInstance` raises
            // `IllegalArgumentException` for a locale with no ISO 3166 country
            // (a bare `new Locale("ru")`), and the `_` arm below absorbs that
            // `Err` — the established idiom in this file for a call whose
            // failure is a legitimate outcome.
            //
            // The SYMBOL goes through `cldr_currency_symbol`, the same helper
            // the `Currency.getSymbol(Locale)` override uses.
            //
            // ORDER IS LOAD-BEARING and is the reverse of what stood here.
            // `setInternationalCurrencySymbol`'s real body re-derives
            // `currencySymbol = currency.getSymbol(locale)`, so whichever of
            // the two runs LAST wins the symbol. The old code set the symbol
            // first and the code second, and relied on the `getSymbol`
            // override to put the symbol back; setting the code first and the
            // symbol second makes that independent of `getSymbol`'s behaviour.
            let locale_for_currency = match (locale_obj, locale_pin) {
                (Some(o), Some(p)) => Value::Object(Some(ctx.read_native_pin(p, o))),
                _ => Value::Object(None),
            };
            let currency = match ctx.invoke(
                "java/util/Currency",
                "getInstance",
                "(Ljava/util/Locale;)Ljava/util/Currency;",
                &[locale_for_currency],
            ) {
                Ok(Some(Value::Object(Some(c)))) => Some(c),
                _ => None,
            };
            let code = currency
                .and_then(|c| match ctx.get_field_by_name(c, "currencyCode") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                })
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".to_string());
            let symbol = cldr_currency_symbol(ctx, &code, &lang, &country);
            let ics = ctx.create_string(&code);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setInternationalCurrencySymbol",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(ics))],
            );
            let cs = ctx.create_string(&symbol);
            let this_now = ctx.read_native_pin(this_pin, this);
            let _ = ctx.invoke_virtual(
                this_now,
                "setCurrencySymbol",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(cs))],
            );
            ctx.unpin_native_roots(this_pin);
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
    //
    // Slot 1 (currency) used to read `¤#,##0.00;(¤#,##0.00)`. The `;` half is a
    // NEGATIVE SUBPATTERN, and the parenthesised form it names is the CLDR
    // *accounting* pattern, which no locale uses as its standard currency
    // format. With it present, `DecimalFormat` never prefixes a minus sign, so
    // `NumberFormat.getCurrencyInstance(Locale.US).format(-1234.5)` produced
    // `($1,234.50)` where HotSpot gives `-$1,234.50`. Dropping the subpattern
    // restores the JDK rule "negative prefix = minus sign + positive prefix".
    //
    // Measured rather than assumed (probes/NumberPatternCensusProbe.java on
    // Temurin 25.0.3+9): of the 1158 installed locales, the parenthesised
    // string matched **0**, so this was wrong for every locale, not just en-US
    // — and the replacement `¤#,##0.00` is the exact CLDR value for **361** of
    // them (the whole en family included) and the single most common form.
    // The remaining 797 differ only in symbol PLACEMENT (`#,##0.00 ¤` for the
    // de/fr/es family, 333 locales; `¤ #,##0.00`, 215) — a pre-existing
    // en-shaped approximation this table shares with the hardcoded
    // `getDecimalFormatSymbolsData` above, which likewise returns en's `.`/`,`
    // separators for every locale. Making the pattern locale-aware WITHOUT
    // making the symbols locale-aware would render a hybrid (`1,234.50 €`),
    // which is further from HotSpot than the uniform en shape; the two must
    // move together, and that is a larger change than this parity fix.
    //
    // Slot 0 is right for 1154/1158, slot 2 (percent) for 924/1158 (the 234
    // misses are the `#,##0 %` no-break-space family — the same en/locale
    // split), and slot 3 is not reachable from outside `java.text`, so it is
    // uncounted. See W7-44-numberformat-enum-and-double-tostring.md.
    //
    // W7-80 is that larger change, and it lands the two halves TOGETHER as
    // W7-44 required: this table and `getDecimalFormatSymbolsData` above now
    // read the same locale's CLDR bundle, so `#,##0.00 ¤` for de arrives beside
    // de's `,`/`.` separators and its `€`, never one without the others.
    //
    // Slot 3 is the ACCOUNTING pattern, not the scientific one. Read from
    // `NumberFormatProviderImpl`: `ACCOUNTINGSTYLE = 3`, and slot 3 is consulted
    // only when the locale carries `-u-cf-account`. The `#E0` that stood there
    // was mislabelled "scientific"; it was harmless because nothing could reach
    // it (`NumberFormat.getScientificInstance` is package-private), and CLDR's
    // real slot 3 is strictly better.
    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getNumberPatterns",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let (lang, country) = receiver_locale(ctx, args.first());
            let cldr = cldr_format_data(ctx, &lang, &country)
                .and_then(|t| cldr_number_strings(&t, "NumberPatterns"));
            let arr = match cldr {
                // `NumberFormatProviderImpl.getInstance` indexes 0..2 directly
                // and tests `length > 3` before touching the accounting slot,
                // so a 3-slot array is safe but a shorter one is not.
                Some(v) if v.len() >= 3 => {
                    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
                    make_string_array(ctx, &refs)
                }
                _ => make_string_array(ctx, &["#,##0.###", "\u{00A4}#,##0.00", "#,##0%", "#E0"]),
            };
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
        // W7-80: the patterns come from the JDK image's own CLDR `TimePatterns`
        // / `DatePatterns` / `DateTimePatterns` for this `LocaleResources`'
        // locale. The curated en/de arrays below survive as the fallback.
        //
        // The java.time overload takes the same table on purpose: the real
        // `getJavaTimeDateTimePattern` looks for a `java.time.`-prefixed key
        // first and falls back to the unprefixed one, and JDK 25's CLDR
        // `FormatData` carries `java.time.` variants only for the non-Gregorian
        // calendars (`java.time.japanese.DatePatterns`, …), never for gregory.
        let (lang, country) = receiver_locale(ctx, args.first());
        let table = cldr_format_data(ctx, &lang, &country);
        // (time FULL/LONG/MEDIUM/SHORT, date FULL/LONG/MEDIUM/SHORT)
        let (fallback_time, fallback_date): ([&str; 4], [&str; 4]) = if lang == "de" {
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
        let time: Vec<String> = match table.as_ref().and_then(|t| cldr_arr(t, "TimePatterns")) {
            Some(v) if v.len() >= 4 => v.clone(),
            _ => fallback_time.iter().map(|s| (*s).to_string()).collect(),
        };
        let date: Vec<String> = match table.as_ref().and_then(|t| cldr_arr(t, "DatePatterns")) {
            Some(v) if v.len() >= 4 => v.clone(),
            _ => fallback_date.iter().map(|s| (*s).to_string()).collect(),
        };
        let combiners: Vec<String> =
            match table.as_ref().and_then(|t| cldr_arr(t, "DateTimePatterns")) {
                Some(v) if !v.is_empty() => v.clone(),
                _ => vec!["{1}, {0}".to_string()],
            };
        let style = |i: usize| -> i32 {
            match args.get(i) {
                Some(Value::Int(n)) => *n,
                _ => -1,
            }
        };
        let time_style = style(1);
        let date_style = style(2);
        // clamp out-of-range styles to MEDIUM rather than panic
        let clamp = |s: i32| -> usize {
            if (0..=3).contains(&s) {
                s as usize
            } else {
                2
            }
        };
        let pattern = match (date_style >= 0, time_style >= 0) {
            (true, true) => {
                // `LocaleResources.getDateTimePattern` picks the combiner at
                // `max(dateStyle, timeStyle)` and substitutes {1}=date, {0}=time.
                // A literal replace matches the JDK's `MessageFormat.format` on
                // every combiner in the image: the JDK doubles the quotes first,
                // so a quoted literal like `{1} 'at' {0}` round-trips to the same
                // `'at'` a replace leaves in place.
                let combiner = combiners
                    .get(clamp(date_style.max(time_style)))
                    .or_else(|| combiners.first())
                    .cloned()
                    .unwrap_or_else(|| "{1}, {0}".to_string());
                combiner
                    .replace("{1}", &date[clamp(date_style)])
                    .replace("{0}", &time[clamp(time_style)])
            }
            (true, false) => date[clamp(date_style)].clone(),
            (false, true) => time[clamp(time_style)].clone(),
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

    // ---------------------------------------------------------------------
    // BREAKITER, the real data. `java.text.BreakIterator`'s three bundle-fed
    // factories walk
    //
    //   BreakIteratorProviderImpl.getBreakInstance(locale, idx, dataKey, dictKey)
    //     LocaleResources.getBreakIteratorInfo("BreakIteratorClasses")  -> String[]
    //     LocaleResources.getBreakIteratorInfo(dataKey)                 -> String
    //     LocaleResources.getBreakIteratorResources(dataKey)            -> byte[]
    //     new sun.text.RuleBasedBreakIterator(name, bytes)
    //
    // (`getCharacterInstance` is not here: JDK 25 answers it with an inline
    // `GraphemeBreakIterator` and asks no bundle at all. Read from the image's
    // own bytecode, not assumed — the other three pass indices 0/1/2 and the
    // root bundle's `BreakIteratorClasses` has exactly three entries.)
    //
    // Both readers returned null, so `classNames[type]` threw
    // `NullPointerException: Cannot load from null array` and this VM had to
    // pin a synthetic iterator over the whole family (the BREAKITER allow-list
    // in `vm/src/vm/vm_exec.rs`). The comments there and in
    // `reflect_annotations.rs` blame "jdk.localedata's class-based resource
    // bundles are not surfaced through our jimage path" — which W7-80 showed
    // was STALE for the CLDR families and is stale here too. The classes and
    // the binary data are both in the image this VM already boots from; the
    // reason the lookup missed is that `cldr_packages` sent it to a `cldr`
    // package that does not exist for this family. See `non_cldr_packages`.
    //
    // Answering these two from the image is what lets `java/text/BreakIterator`'s
    // 17 registrations be retired rather than pinned — lane 1 §10 item 5.
    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getBreakIteratorInfo",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let Some(Value::Object(Some(k))) = args.get(1).copied() else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(key) = ctx.read_string(k) else {
                return Ok(Some(Value::Object(None)));
            };
            let (lang, country) = receiver_locale(ctx, args.first());
            let Some(table) = break_iterator_info(ctx, &lang, &country) else {
                return Ok(Some(Value::Object(None)));
            };
            match table.get(&key) {
                Some(CldrValue::Arr(v)) => {
                    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
                    let arr = make_string_array(ctx, &refs);
                    Ok(Some(Value::Object(Some(arr))))
                }
                Some(CldrValue::Str(sv)) => {
                    let sv = sv.clone();
                    let o = ctx.create_string(&sv);
                    Ok(Some(Value::Object(Some(o))))
                }
                // ABSENT KEY. HotSpot's body ends in `ResourceBundle.getObject`,
                // which throws `MissingResourceException` rather than answering
                // null. Answering null is a deliberate, narrower divergence: it
                // is what this method did for EVERY key before this native, and
                // the three keys `getBreakInstance` asks for are all present in
                // the root bundle, so no caller reaches this arm. Throwing here
                // would turn a silent wrong answer into a new abort on a path
                // that has never been exercised.
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    registry.register(
        "sun/util/locale/provider/LocaleResources",
        "getBreakIteratorResources",
        "(Ljava/lang/String;)[B",
        |ctx, args| {
            let Some(Value::Object(Some(k))) = args.get(1).copied() else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(key) = ctx.read_string(k) else {
                return Ok(Some(Value::Object(None)));
            };
            let (lang, country) = receiver_locale(ctx, args.first());
            // The data file's NAME is a value in the same bundle, exactly as
            // `BreakIteratorResourceBundle.handleGetObject` reads it:
            // `getPackageName().replace('.','/') + "/" + info.getString(key)`.
            // So `WordData` -> `WordBreakIteratorData` in the root bundle and
            // `WordBreakIteratorData_th` in `_th`'s, and the package is the
            // bundle's own — which is why both are probed below rather than
            // the root one only.
            let Some(table) = break_iterator_info(ctx, &lang, &country) else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(CldrValue::Str(file)) = table.get(&key).cloned() else {
                return Ok(Some(Value::Object(None)));
            };
            let mut bytes = None;
            for pkg in ["sun/text/resources/ext", "sun/text/resources"] {
                if let Some(b) = ctx.find_resource(&format!("{pkg}/{file}")) {
                    bytes = Some(b);
                    break;
                }
            }
            let Some(bytes) = bytes else {
                tracing::warn!(
                    key = %key,
                    file = %file,
                    "BREAKITER: the bundle names a data file the image does not                      carry; the real RuleBasedBreakIterator cannot be built."
                );
                return Ok(Some(Value::Object(None)));
            };
            // No pin/re-read pair: `new_array` is the only allocation and
            // `write_byte_array_from` is a bulk write into it, so nothing can
            // move between them.
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            if !ctx.write_byte_array_from(arr, 0, &bytes) {
                // A refused bulk write leaves a zero-filled array, which the
                // real `RuleBasedBreakIterator` would parse as a corrupt table
                // rather than reject. Null is the answer this method already
                // gave for everything, and it keeps the failure at the caller
                // that can see it.
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueNames(
    //     String id, int field, int style, Locale locale) -> Map<String,Integer>
    // The java.time text-name entry point (see the module note above). Build a
    // real java.util.HashMap of name -> Calendar-value so the downstream
    // real-JDK `DateTimeTextProvider.createStore` bytecode can iterate its
    // `entrySet()` unchanged. Serve only the `gregory` calendar; the names come
    // from the requested locale's CLDR data, English only as a fallback for an
    // image that carries none.
    registry.register(
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueNames",
        "(Ljava/lang/String;IILjava/util/Locale;)Ljava/util/Map;",
        |ctx, args| {
            if !calendar_id_is_gregorian(ctx, args.first()) {
                return Ok(Some(Value::Object(None)));
            }
            let field = int_arg(args, 1);
            let style = int_arg(args, 2);
            // Narrow month/day names have duplicates ("J"/"J"/"J", "S"/"S")
            // that a Map<String,Integer> would collapse; DateTimeTextProvider
            // handles those via the singular per-value lookup below, so return
            // null here to steer it there. `calendar_name_entries` enforces the
            // same rule for any other array CLDR happens to duplicate; this
            // early exit keeps the common case from loading a table to find out.
            let base = style & !CAL_STANDALONE_MASK;
            if base == CAL_STYLE_NARROW && (field == CAL_MONTH || field == CAL_DAY_OF_WEEK) {
                return Ok(Some(Value::Object(None)));
            }
            let entries = locale_calendar_entries(ctx, args.get(3), field, style);
            if entries.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
                Some(Value::Object(Some(m))) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Every `create_string` / `valueOf` / `put` below allocates, so the
            // map can move under any of them. Pin once and re-read per entry,
            // the same discipline `put_arr` uses a few hundred lines up.
            let map_pin = ctx.pin_native_root(map);
            let mut map = map;
            for (name, val) in entries {
                let k = ctx.create_string(&name);
                let k_pin = ctx.pin_native_root(k);
                let boxed = ctx
                    .invoke(
                        "java/lang/Integer",
                        "valueOf",
                        "(I)Ljava/lang/Integer;",
                        &[Value::Int(val)],
                    )?
                    .unwrap_or(Value::Object(None));
                map = ctx.read_native_pin(map_pin, map);
                let k = ctx.read_native_pin(k_pin, k);
                ctx.invoke_virtual(
                    map,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(k)), boxed],
                )?;
                map = ctx.read_native_pin(map_pin, map);
                ctx.unpin_native_roots(k_pin);
            }
            ctx.unpin_native_roots(map_pin);
            Ok(Some(Value::Object(Some(map))))
        },
    );

    // sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueName(
    //     String id, int field, int value, int style, Locale locale) -> String
    // Singular sibling of the above — used by `DateTimeTextProvider` for narrow
    // month/day (and as a fallback for any other style whose plural map came
    // back null). Returns the requested locale's CLDR name for one Calendar
    // field value.
    registry.register(
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueName",
        "(Ljava/lang/String;IIILjava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            if !calendar_id_is_gregorian(ctx, args.first()) {
                return Ok(Some(Value::Object(None)));
            }
            let field = int_arg(args, 1);
            let value = int_arg(args, 2);
            let style = int_arg(args, 3);
            match locale_calendar_name(ctx, args.get(4), field, value, style) {
                Some(name) => {
                    let s = ctx.create_string(&name);
                    Ok(Some(Value::Object(Some(s))))
                }
                None => Ok(Some(Value::Object(None))),
            }
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
            normalizer_reject_nulls(args.first(), args.get(1))?;
            let input = match args.first() {
                Some(Value::Object(Some(s))) => normalizer_read_char_sequence(ctx, *s),
                _ => return Ok(Some(Value::Object(None))),
            };
            // java.text.Normalizer.Form ordinals: NFD=0, NFC=1, NFKD=2, NFKC=3
            // (declaration order in the JDK enum — NOT alphabetical).
            let form_ordinal = normalizer_form_ordinal(ctx, args.get(1));
            let one = |run: &str| -> String {
                match form_ordinal {
                    0 => run.nfd().collect::<String>(),  // NFD
                    1 => run.nfc().collect::<String>(),  // NFC
                    2 => run.nfkd().collect::<String>(), // NFKD
                    3 => run.nfkc().collect::<String>(), // NFKC
                    _ => run.to_string(),
                }
            };
            // `unicode_normalization` takes `&str`, which cannot hold an
            // unpaired surrogate, so the whole input used to be decoded and
            // every lone unit became U+FFFD. MEASURED on both VMs:
            //
            //   Normalizer.normalize("a<U+D800>b", NFC)
            //     HotSpot   61,d800,62      CratonVM   61,fffd,62
            //
            // Splitting at each unpaired surrogate is CORRECT rather than
            // approximate: an unpaired surrogate is an unassigned code point
            // with combining class 0 that composes with nothing, so it is a
            // normalization boundary — no composition or reordering can cross
            // it, and the runs either side normalize independently.
            //
            // Input with no unpaired surrogate is a single run, so the common
            // path is exactly what it was.
            let out = normalize_units_by_run(&input, one);
            let s = crate::lang_string::sb_string_from_units(ctx, &out)?;
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
            normalizer_reject_nulls(args.first(), args.get(1))?;
            let input = match args.first() {
                Some(Value::Object(Some(s))) => normalizer_read_char_sequence(ctx, *s),
                _ => return Ok(Some(Value::Int(1))),
            };
            // Form ordinals: NFD=0, NFC=1, NFKD=2, NFKC=3 (JDK enum order).
            let form_ordinal = normalizer_form_ordinal(ctx, args.get(1));
            // Run-wise, for the same reason `normalize` is: an unpaired
            // surrogate is a normalization boundary, and it is itself
            // normalized under every form (unassigned, combining class 0), so
            // the answer is "every representable run is normalized".
            let normalized = normalizer_runs(&input)
                .into_iter()
                .all(|run| match form_ordinal {
                    0 => is_nfd_quick(run.chars()) == IsNormalized::Yes,
                    1 => is_nfc_quick(run.chars()) == IsNormalized::Yes,
                    2 => is_nfkd_quick(run.chars()) == IsNormalized::Yes,
                    3 => is_nfkc_quick(run.chars()) == IsNormalized::Yes,
                    _ => true,
                });
            Ok(Some(Value::Int(if normalized { 1 } else { 0 })))
        },
    );

    registry.set_category(__prev_cat);
}

/// Read an arbitrary `CharSequence` argument, not just `String`. `read_string`
/// only understands the compact-string `String` layout; called on any other
/// `CharSequence` (e.g. pgjdbc's SCRAM stringprep wraps a `char[]` in
/// `java.nio.CharBuffer` before calling `Normalizer.normalize`) it silently
/// returns `None`, and blindly defaulting that to `""` turns real content
/// into an empty string — surfacing downstream as an
/// `ArrayIndexOutOfBoundsException` in `Character.codePointAt` on the
/// now-empty array. Real `String` is read directly; everything else goes
/// through its own `toString()`, which every `CharSequence` must provide.
/// A `CharSequence` argument's raw UTF-16 code units.
///
/// Units rather than a `String` because a Rust `str` cannot hold an unpaired
/// surrogate and this is the reader every `Normalizer` entry point uses; see
/// the `normalize` registration for the measured rows.
fn normalizer_read_char_sequence(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<u16> {
    if ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(obj)) {
        return crate::lang_string::read_string_chars(&*ctx, obj);
    }
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => crate::lang_string::read_string_chars(&*ctx, s),
        _ => Vec::new(),
    }
}

/// Normalize `units` by applying `one` to each maximal run of representable
/// text, passing every unpaired surrogate through untouched.
///
/// See the `normalize` registration for why an unpaired surrogate is a
/// normalization boundary and this is exact rather than a best effort. A run
/// contains no unpaired surrogate by construction, so `from_utf16_lossy` over
/// it is lossless.
/// The maximal representable runs of `units`, with every unpaired surrogate
/// acting as a separator. Shared by `normalize` and `isNormalized` so the two
/// cannot disagree about where a run ends.
fn normalizer_runs(units: &[u16]) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut run: Vec<u16> = Vec::new();
    let mut i = 0usize;
    while i < units.len() {
        let u = units[i];
        let high = (0xD800..=0xDBFF).contains(&u);
        let low = (0xDC00..=0xDFFF).contains(&u);
        if high && i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
            run.push(u);
            run.push(units[i + 1]);
            i += 2;
            continue;
        }
        if high || low {
            if !run.is_empty() {
                runs.push(String::from_utf16_lossy(&run));
                run.clear();
            }
            i += 1;
            continue;
        }
        run.push(u);
        i += 1;
    }
    if !run.is_empty() {
        runs.push(String::from_utf16_lossy(&run));
    }
    runs
}

fn normalize_units_by_run(units: &[u16], one: impl Fn(&str) -> String) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut run: Vec<u16> = Vec::new();
    let mut flush = |run: &mut Vec<u16>, out: &mut Vec<u16>| {
        if !run.is_empty() {
            out.extend(one(&String::from_utf16_lossy(run)).encode_utf16());
            run.clear();
        }
    };
    let mut i = 0usize;
    while i < units.len() {
        let u = units[i];
        let high = (0xD800..=0xDBFF).contains(&u);
        let low = (0xDC00..=0xDFFF).contains(&u);
        if high && i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
            run.push(u);
            run.push(units[i + 1]);
            i += 2;
            continue;
        }
        if high || low {
            flush(&mut run, &mut out);
            out.push(u);
            i += 1;
            continue;
        }
        run.push(u);
        i += 1;
    }
    flush(&mut run, &mut out);
    out
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

#[cfg(test)]
mod w6_concrete_bundle_chain_tests {
    use super::*;

    /// The CLDR candidates are APPENDED, least-specific first, and the plain
    /// ones are kept.
    ///
    /// Both halves matter. Keeping the plain chain means `TimeZoneNames`,
    /// whose legacy root is the one `try_class_bundle` has always found,
    /// still finds it. Appending means the CLDR bundle is instantiated LAST
    /// and therefore returned, with the legacy root as its parent rather than
    /// its replacement.
    #[test]
    fn the_cldr_packages_are_appended_after_the_plain_candidates() {
        let chain = vec![
            ("sun.util.resources.LocaleNames".to_string(), String::new(), String::new()),
            (
                "sun.util.resources.LocaleNames_en".to_string(),
                "en".to_string(),
                String::new(),
            ),
        ];
        let out = concrete_bundle_chain("sun.util.resources.LocaleNames", &chain);
        let names: Vec<&str> = out.iter().map(|(c, _, _)| c.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "sun.util.resources.LocaleNames",
                "sun.util.resources.LocaleNames_en",
                "sun.util.resources.cldr.LocaleNames",
                "sun.util.resources.cldr.ext.LocaleNames",
                "sun.util.resources.cldr.LocaleNames_en",
                "sun.util.resources.cldr.ext.LocaleNames_en",
            ]
        );
        // The locale each candidate stands for travels with it: `rb_get_bundle`
        // reads that pair back to stamp the bundle's `locale` field, and a
        // candidate that lost its language would stamp the root.
        assert_eq!(out[4].1, "en");
    }

    /// A base name with no CLDR package pair is handed back untouched rather
    /// than given invented candidates.
    #[test]
    fn a_non_locale_base_name_keeps_its_own_chain() {
        let chain = vec![(
            "com.example.Messages".to_string(),
            String::new(),
            String::new(),
        )];
        let out = concrete_bundle_chain("com.example.Messages", &chain);
        assert_eq!(out, chain);
    }

    /// The two families this routing is for, and the one it is deliberately
    /// not for.
    #[test]
    fn currency_names_is_not_routed_to_the_class_bundles() {
        assert!(needs_concrete_bundle_class("sun.util.resources.TimeZoneNames"));
        assert!(needs_concrete_bundle_class("sun.util.resources.LocaleNames"));
        // +38 armed on `L1LocaleProviderWorkload`: its curated path is doing
        // work the class bundles would have to replace, and that is its own
        // measurement rather than this one's corollary.
        assert!(!needs_concrete_bundle_class(
            "sun.util.resources.CurrencyNames"
        ));
    }
}
