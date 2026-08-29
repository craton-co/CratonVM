//! A faithful native `java.text.DateFormat.format(Date)` for
//! `java.text.SimpleDateFormat` receivers.
//!
//! # Why this exists
//!
//! `org.apache.juli.TestOneLineFormatterPerformance` is a RATIO test: it
//! asserts Tomcat's `DateFormatCache` beats
//! `String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", …)` over a million
//! iterations. A uniformly slow VM still passes it. CratonVM failed it because
//! the two sides are not uniformly slow — **we accelerate one of them and not
//! the other**:
//!
//! | | HotSpot | CratonVM | ratio |
//! |---|---|---|---|
//! | `String.format("%t…")` | 1277 ns | 2082 ns | 1.6x — served by our own intrinsic |
//! | `SimpleDateFormat.format(Date)` | 248 ns | 138,472 ns | **557x** — interpreted JDK bytecode |
//!
//! Measured by `probes/DateFormatCostProbe.java` / `DateFormatPathProbe.java`
//! on a fixed timestamp. Both VMs take the same branches (`useDateFormatSymbols
//! = false`, `locale = en_US`, `gregory`, `zeroDigit = '0'`), the native-call
//! census is ~5 natives per format (so this is not the native funnel), and
//! `--nojit` costs the same as the default (so the JIT is not engaged here).
//! It is simply this VM executing a few thousand JDK bytecodes.
//!
//! This module closes the asymmetry the same way `String.format`,
//! `String.replaceAll` and the collection natives already do: a faithful Rust
//! implementation of the common case, with the real bytecode as the fallback.
//!
//! # Faithfulness is CHECKED, not argued
//!
//! An alternate implementation of a JDK formatter has a large surface to get
//! subtly wrong (era, DST, month-name provenance, `zeroPaddingNumber`'s
//! truncation rules). Rather than reason about it, this module
//! **cross-checks itself against the real bytecode at run time**, once per
//! distinct *output shape*: the first time a given `(plan, month, weekday, era,
//! am/pm, dst-flag, negative-offset)` tuple is formatted, the native result is
//! compared against `format(Date, StringBuffer, FieldPosition)` run as
//! bytecode. A mismatch returns the BYTECODE answer and disables the fast path
//! for that plan permanently. So a bug here costs performance, never
//! correctness, and `CRATONVM_DBG=dateformat` names it.
//!
//! The tuple space is what actually varies the text (12 months x 7 weekdays x
//! 2 eras x 2 am/pm x 2 dst x 2 sign = 1344 worst case, one or two in a hot
//! loop), so the check is amortised to nothing while still covering every
//! branch that produces a different string.
//!
//! # What is NOT accelerated
//!
//! Anything outside the checked subset falls through to the real bytecode with
//! no behaviour change at all:
//!
//! * a receiver that is not exactly `java.text.SimpleDateFormat` (a subclass
//!   may override the virtual `format(Date, StringBuffer, FieldPosition)` that
//!   the `final` `format(Date)` dispatches to);
//! * a calendar that is not exactly `java.util.GregorianCalendar`, or one whose
//!   Gregorian cutover has been moved;
//! * instants before the default Gregorian cutover (1582-10-15), where
//!   `GregorianCalendar` switches to Julian rules;
//! * a `zeroDigit` other than `'0'` (Arabic-Indic digit locales);
//! * `forceStandaloneForm` (locales whose month names differ standalone);
//! * the pattern letters `z L F w W Y u` — zone display names, standalone
//!   months, and the week-number family, whose rules depend on
//!   `TimeZone`-provided strings or the calendar's `firstDayOfWeek` /
//!   `minimalDaysInFirstWeek`.
//!
//! # Not a contract §1.4 shadow, and it should stop being re-argued
//!
//! This module's single registration —
//! `java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;`, the only
//! row a census attributes to this file — is a native in front of image
//! bytecode, which is the shape §1.4 calls a shadow. It is nonetheless
//! `NativeKind::Intrinsic` rather than `Bridge`, deliberately, and so is
//! outside the `bridge_shadows_bytecode` population and exempt from the
//! `--jdk-only` yield.
//!
//! The reason is the "faithfulness is CHECKED" section above, restated as the
//! property a retirement wave needs: **this native cannot give an answer the
//! bytecode would not.** Every unsupported shape falls through to the bytecode
//! and every supported one is cross-checked against it, with a mismatch
//! returning the BYTECODE answer and disabling the fast path for good. So
//! retiring it changes no observable and costs the 557x it was written to
//! close. Confirmed against a 2026-08-11 census (`kind: intrinsic`, one row);
//! recorded so the next wave does not re-derive it from the "native in front
//! of `Code`" predicate alone. See
//! docs/known-issues/jdk-only/W7-22-shadow-retirement-logging-and-time.md §5.

use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::OnceLock;

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::obj_arg;

type MethodCallResult = Result<Option<Value>, MethodCallFailed>;

/// Milliseconds in a day.
const MS_PER_DAY: i64 = 86_400_000;

/// The default `GregorianCalendar` Gregorian cutover, 1582-10-15T00:00:00Z.
/// A calendar whose cutover has been moved is declined outright.
const DEFAULT_CUTOVER_MILLIS: i64 = -12_219_292_800_000;

/// 1583-01-01T00:00:00Z. The whole of 1582 is declined, not just the instants
/// before the cutover: that year is TEN DAYS SHORT (1582-10-04 Julian is
/// followed by 1582-10-15 Gregorian), so `GregorianCalendar`'s DAY_OF_MONTH and
/// DAY_OF_YEAR both diverge from proleptic-Gregorian arithmetic across it.
/// Caught by the run-time cross-check first — 196 cells of the parity battery
/// — and turned into an up-front decline so no formatter gets poisoned for it.
const FIRST_FULLY_GREGORIAN_MILLIS: i64 = -12_212_553_600_000;

/// One element of a compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Literal text, already unquoted.
    Literal(String),
    /// A run of `count` copies of pattern letter `letter`.
    Field { letter: char, count: usize },
}

/// A compiled, accelerable pattern plus the receiver state it was compiled
/// against. Rebuilt whenever any of those inputs changes.
#[derive(Debug, Clone)]
struct Plan {
    pattern: String,
    /// Identity of the `String` object the `pattern` field pointed at when the
    /// plan was built. `applyPattern` installs a different object, so an
    /// identity mismatch is the cheap staleness test.
    pattern_id: i32,
    /// Identity of the `DateFormatSymbols` the plan was verified against.
    /// `identity_hash_code` is a 32-bit value and CAN collide across objects,
    /// so a cache entry may be reached by a DIFFERENT formatter. Two formatters
    /// sharing a hash and a pattern but not their symbols (same pattern,
    /// different locale) would otherwise inherit each other's `verified` set
    /// and skip the cross-check. Comparing the symbols identity too makes that
    /// a plan rebuild instead.
    symbols_id: i32,
    pieces: std::sync::Arc<Vec<Piece>>,
    symbols: std::sync::Arc<Symbols>,
}

/// Per-`SimpleDateFormat` cached state, keyed by the receiver's identity hash.
struct Entry {
    plan: Option<Plan>,
    /// Set once a cross-check has disagreed: the fast path is off for good.
    poisoned: bool,
    /// Output shapes already cross-checked against the bytecode.
    verified: FxHashSet<ShapeKey>,
}

/// The `DateFormatSymbols` strings a plan can need, decoded once when the plan
/// is built rather than on every format.
///
/// Reading one month name per format cost two heap field reads plus a
/// `read_string` — which allocates a Rust `String` and decodes UTF-16 — and
/// `GenerationalHeap::is_object_address` (reached through every `get_field`)
/// was 8.9% of the native in a perf profile. These are safe to hold across
/// calls because the only public route to different symbols is
/// `setDateFormatSymbols`, which installs a CLONE — a new object, so a new
/// identity, so a plan rebuild. `probes/DateFormatParityProbe.java` covers
/// that case explicitly.
#[derive(Debug, Default, Clone)]
struct Symbols {
    eras: Vec<String>,
    months: Vec<String>,
    short_months: Vec<String>,
    weekdays: Vec<String>,
    short_weekdays: Vec<String>,
    ampms: Vec<String>,
}

fn read_string_array(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Vec<String> {
    let Some(arr) = obj_slot(ctx, obj, slot) else {
        return Vec::new();
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => out.push(ctx.read_string(s).unwrap_or_default()),
            _ => out.push(String::new()),
        }
    }
    out
}

fn read_symbols(ctx: &mut dyn NativeContext, sl: &Slots, symbols: ObjectRef) -> Symbols {
    Symbols {
        eras: read_string_array(ctx, symbols, sl.dfs_eras),
        months: read_string_array(ctx, symbols, sl.dfs_months),
        short_months: read_string_array(ctx, symbols, sl.dfs_short_months),
        weekdays: read_string_array(ctx, symbols, sl.dfs_weekdays),
        short_weekdays: read_string_array(ctx, symbols, sl.dfs_short_weekdays),
        ampms: read_string_array(ctx, symbols, sl.dfs_ampms),
    }
}

/// The tuple that determines which *text* a plan emits. See the module doc.
type ShapeKey = (u8, u8, u8, u8, bool, bool);

fn plan_cache() -> &'static parking_lot::Mutex<FxHashMap<i32, Entry>> {
    static C: OnceLock<parking_lot::Mutex<FxHashMap<i32, Entry>>> = OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// One-entry per-thread cache in front of [`plan_cache`].
///
/// The shared map costs two `Mutex` acquisitions and two hashes per format,
/// which is real money once a format is ~1.3 us. A formatter is almost always
/// used from one thread in a loop, so a single-entry thread-local keyed on the
/// same three identities skips both locks entirely on the steady-state path.
/// It caches only what has ALREADY been proven against the bytecode (`verified`
/// is a copy, so a thread that has not yet seen a shape still takes the slow
/// path and cross-checks it) and is invalidated by any identity change.
struct LastPlan {
    this_id: i32,
    pattern_id: i32,
    symbols_id: i32,
    pieces: std::sync::Arc<Vec<Piece>>,
    symbols: std::sync::Arc<Symbols>,
    verified: FxHashSet<ShapeKey>,
}

thread_local! {
    static LAST_PLAN: std::cell::RefCell<Option<LastPlan>> =
        const { std::cell::RefCell::new(None) };
    /// Reused render buffer — one allocation per thread rather than per format.
    static SCRATCH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn dbg_on() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG")
            .map(|v| v.split(',').any(|t| t.trim() == "dateformat"))
            .unwrap_or(false)
    })
}

/// Field slot indices and `ClassId`s, resolved once.
///
/// The first cut of this module used `get_field_by_name`/`set_field_by_name`
/// and `class_name_of_id` throughout. A perf profile of the hot loop put
/// `get_field_by_name` at 8.0%, `set_field_by_name` at 5.8%, `memcmp` at 4.1%
/// and `slot_for_exact` at 3.3% — i.e. a fifth of the native was re-resolving
/// the same twenty names, under the class-manager read lock, on every single
/// format. These are stable for the lifetime of the VM (a field's slot cannot
/// move; a class redefinition would give a NEW `ClassId`, which the
/// `class_id_of_object` comparison below catches), so resolve them once.
struct Slots {
    sdf_class: cratonvm_types::ClassId,
    gcal_class: cratonvm_types::ClassId,
    date_class: cratonvm_types::ClassId,
    /// The `TimeZone` classes whose `getOffset(long)` this VM itself
    /// implements (`register_tzdb_offset_natives_for`). For those — and ONLY
    /// those — the offset can be read straight from the tzdb helper instead of
    /// dispatching into Java and paying a native-funnel entry. Any other
    /// receiver may be an application subclass with its own override, or a
    /// `java.util.SimpleTimeZone` whose id is an opaque LABEL rather than a
    /// zone, so it keeps the virtual call. C12-1 removed
    /// `java/util/SimpleTimeZone` from this set; do not restore it without
    /// reading `docs/known-issues/jdk-only/D3-1-simpledateformat-format-zone-arm.md`.
    zoneinfo_class: Option<cratonvm_types::ClassId>,
    timezone_class: Option<cratonvm_types::ClassId>,
    tz_id: usize,

    sdf_pattern: usize,
    sdf_format_data: usize,
    sdf_zero_digit: usize,
    sdf_force_standalone: usize,
    df_calendar: usize,

    gcal_cutover: usize,
    cal_zone: usize,
    cal_time: usize,
    cal_is_time_set: usize,
    cal_are_fields_set: usize,
    cal_are_all_fields_set: usize,

    date_fast_time: usize,
    date_cdate: usize,

    dfs_eras: usize,
    dfs_months: usize,
    dfs_short_months: usize,
    dfs_weekdays: usize,
    dfs_short_weekdays: usize,
    dfs_ampms: usize,
}

fn slots(ctx: &mut dyn NativeContext) -> Option<&'static Slots> {
    static S: OnceLock<Option<Slots>> = OnceLock::new();
    // `get_or_init` may run the closure on more than one thread only if the
    // slot is unset, and both would compute the same answer.
    S.get_or_init(|| {
        let sdf_class = ctx.class_id_by_name("java/text/SimpleDateFormat")?;
        let gcal_class = ctx.class_id_by_name("java/util/GregorianCalendar")?;
        let date_class = ctx.class_id_by_name("java/util/Date")?;
        let dfs_class = ctx.class_id_by_name("java/text/DateFormatSymbols")?;
        let f = |cid: cratonvm_types::ClassId, name: &str| {
            ctx.resolve_field_index_by_class_id(cid, name)
        };
        let timezone_class = ctx.class_id_by_name("java/util/TimeZone");
        Some(Slots {
            sdf_class,
            gcal_class,
            date_class,
            zoneinfo_class: ctx.class_id_by_name("sun/util/calendar/ZoneInfo"),
            timezone_class,
            tz_id: timezone_class.and_then(|c| ctx.resolve_field_index_by_class_id(c, "ID"))?,
            sdf_pattern: f(sdf_class, "pattern")?,
            sdf_format_data: f(sdf_class, "formatData")?,
            sdf_zero_digit: f(sdf_class, "zeroDigit")?,
            sdf_force_standalone: f(sdf_class, "forceStandaloneForm")?,
            df_calendar: f(sdf_class, "calendar")?,
            gcal_cutover: f(gcal_class, "gregorianCutover")?,
            cal_zone: f(gcal_class, "zone")?,
            cal_time: f(gcal_class, "time")?,
            cal_is_time_set: f(gcal_class, "isTimeSet")?,
            cal_are_fields_set: f(gcal_class, "areFieldsSet")?,
            cal_are_all_fields_set: f(gcal_class, "areAllFieldsSet")?,
            date_fast_time: f(date_class, "fastTime")?,
            date_cdate: f(date_class, "cdate")?,
            dfs_eras: f(dfs_class, "eras")?,
            dfs_months: f(dfs_class, "months")?,
            dfs_short_months: f(dfs_class, "shortMonths")?,
            dfs_weekdays: f(dfs_class, "weekdays")?,
            dfs_short_weekdays: f(dfs_class, "shortWeekdays")?,
            dfs_ampms: f(dfs_class, "ampms")?,
        })
    })
    .as_ref()
}

// ---------------------------------------------------------------------------
// Pattern compilation
// ---------------------------------------------------------------------------

/// The JDK's pattern letters, in `DateFormatSymbols.patternChars` order.
/// Anything alphabetic and NOT in here is an `IllegalArgumentException` in
/// `SimpleDateFormat.compile()` — we decline instead and let the bytecode
/// raise it, so the exception's type, message and stack all stay the JDK's.
const PATTERN_CHARS: &str = "GyMdkHmsSEDFwWahKzZYuXL";

/// Pattern letters this module implements. The rest fall back — see the module
/// doc for why each is excluded.
fn letter_supported(c: char) -> bool {
    matches!(
        c,
        'G' | 'y'
            | 'M'
            | 'd'
            | 'k'
            | 'H'
            | 'm'
            | 's'
            | 'S'
            | 'E'
            | 'D'
            | 'a'
            | 'h'
            | 'K'
            | 'Z'
            | 'X'
    )
}

/// Compile a pattern to `Piece`s, mirroring `SimpleDateFormat.compile()`'s
/// quoting rules. Returns `None` when the pattern is malformed or uses a letter
/// outside [`letter_supported`] — both mean "run the bytecode".
fn compile_pattern(pattern: &str) -> Option<Vec<Piece>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut literal = String::new();
    let mut i = 0usize;
    let mut in_quote = false;

    let flush = |literal: &mut String, pieces: &mut Vec<Piece>| {
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(literal)));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // `''` is an escaped quote in BOTH modes; a lone `'` toggles.
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                literal.push('\'');
                i += 2;
                continue;
            }
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if in_quote {
            literal.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            if !PATTERN_CHARS.contains(c) {
                // The JDK throws IllegalArgumentException here. Decline so the
                // bytecode throws it, with its own message.
                return None;
            }
            if !letter_supported(c) {
                return None;
            }
            let mut count = 0usize;
            while i < chars.len() && chars[i] == c {
                count += 1;
                i += 1;
            }
            flush(&mut literal, &mut pieces);
            pieces.push(Piece::Field { letter: c, count });
            continue;
        }
        literal.push(c);
        i += 1;
    }
    if in_quote {
        // Unterminated quote — the JDK throws. Decline.
        return None;
    }
    flush(&mut literal, &mut pieces);
    Some(pieces)
}

// ---------------------------------------------------------------------------
// Civil date arithmetic (proleptic Gregorian, matching GregorianCalendar
// at and after the default 1582-10-15 cutover)
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 -> (year, month 1..=12, day 1..=31). Howard Hinnant's
/// `civil_from_days`, which is exact for the whole `i64` range and agrees with
/// `GregorianCalendar` wherever `GregorianCalendar` is Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn day_of_year(y: i64, m: u32, d: u32) -> u32 {
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut doy = CUM[(m - 1) as usize] + d;
    if m > 2 && is_leap(y) {
        doy += 1;
    }
    doy
}

/// Broken-down local time, in the same field terms `Calendar` uses.
struct Fields {
    year: i64,
    month0: u32,      // 0-based, as Calendar.MONTH
    day: u32,         // DAY_OF_MONTH
    day_of_week: u32, // 1 = SUNDAY, as Calendar.DAY_OF_WEEK
    day_of_year: u32,
    hour_of_day: u32,
    minute: u32,
    second: u32,
    millis: u32,
}

fn fields_from_local_millis(local: i64) -> Fields {
    let days = local.div_euclid(MS_PER_DAY);
    let ms_of_day = local.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Fields {
        year,
        month0: month - 1,
        day,
        // 1970-01-01 was a Thursday; Calendar spells SUNDAY as 1, so
        // THURSDAY is 5 and day 0 must map to 5.
        day_of_week: ((days + 4).rem_euclid(7) + 1) as u32,
        day_of_year: day_of_year(year, month, day),
        hour_of_day: (ms_of_day / 3_600_000) as u32,
        minute: (ms_of_day / 60_000 % 60) as u32,
        second: (ms_of_day / 1_000 % 60) as u32,
        millis: (ms_of_day % 1_000) as u32,
    }
}

// ---------------------------------------------------------------------------
// zeroPaddingNumber
// ---------------------------------------------------------------------------

/// `SimpleDateFormat.zeroPaddingNumber(value, min_digits, max_digits, buf)`
/// with `zeroDigit == '0'`.
///
/// Both of the JDK's arms reduce to the same rule for a non-negative value:
/// take the decimal digits, keep the RIGHTMOST `max_digits` of them, then
/// left-pad with zeros to `min_digits`. The fast arm handles 1-2 and 4 digit
/// cases inline; the slow arm reaches `DecimalFormat` with
/// `setMinimumIntegerDigits`/`setMaximumIntegerDigits`, whose integer-digit
/// maximum truncates from the left. `SimpleDateFormat` only ever passes
/// `max_digits` of 2 or `Integer.MAX_VALUE`.
fn zero_padding_number(out: &mut String, value: i64, min_digits: usize, max_digits: usize) {
    let digits = value.unsigned_abs().to_string();
    let kept = if digits.len() > max_digits {
        &digits[digits.len() - max_digits..]
    } else {
        &digits[..]
    };
    if value < 0 {
        out.push('-');
    }
    for _ in kept.len()..min_digits {
        out.push('0');
    }
    out.push_str(kept);
}

// ---------------------------------------------------------------------------
// Reading the receiver
// ---------------------------------------------------------------------------

/// Everything the fast path needs off the Java objects, read once per call.
struct Inputs {
    millis: i64,
    offset_ms: i32,
    fields: Fields,
    calendar: ObjectRef,
    symbols: ObjectRef,
}

fn int_field(ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str) -> Option<i32> {
    ctx.get_field_by_name(obj, name).as_int()
}

fn obj_field(ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str) -> Option<ObjectRef> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn obj_slot(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Option<ObjectRef> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

fn class_name_is(ctx: &dyn NativeContext, obj: ObjectRef, want: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(want)
}

/// Read a `String[]` element as a Rust `String`.
fn string_array_elem(ctx: &mut dyn NativeContext, arr: ObjectRef, index: usize) -> Option<String> {
    if index >= ctx.array_length(arr) {
        return None;
    }
    match ctx.get_array_element(arr, index) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The fast path
// ---------------------------------------------------------------------------

/// Render `pieces` for the given instant. `None` means "something in here is
/// outside the supported subset after all" — run the bytecode.
#[allow(clippy::too_many_arguments)]
fn render(pieces: &[Piece], inputs: &Inputs, syms: &Symbols, out: &mut String) -> Option<()> {
    let f = &inputs.fields;
    out.clear();
    for piece in pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Field { letter, count } => {
                let count = *count;
                match letter {
                    // 'G' — era. Guarded to AD by the cutover check, so index 1.
                    'G' => {
                        out.push_str(syms.eras.get(1)?);
                    }
                    'y' => {
                        if count == 2 {
                            zero_padding_number(out, f.year, 2, 2);
                        } else {
                            zero_padding_number(out, f.year, count, usize::MAX);
                        }
                    }
                    'M' => {
                        if count >= 4 {
                            out.push_str(syms.months.get(f.month0 as usize)?);
                        } else if count == 3 {
                            out.push_str(syms.short_months.get(f.month0 as usize)?);
                        } else {
                            zero_padding_number(out, f.month0 as i64 + 1, count, usize::MAX);
                        }
                    }
                    'd' => zero_padding_number(out, f.day as i64, count, usize::MAX),
                    'D' => zero_padding_number(out, f.day_of_year as i64, count, usize::MAX),
                    // 'k' — 1..24. GregorianCalendar's getMaximum(HOUR_OF_DAY) is 23.
                    'k' => {
                        let v = if f.hour_of_day == 0 {
                            24
                        } else {
                            f.hour_of_day
                        };
                        zero_padding_number(out, v as i64, count, usize::MAX);
                    }
                    'H' => zero_padding_number(out, f.hour_of_day as i64, count, usize::MAX),
                    // 'h' — 1..12. getLeastMaximum(HOUR) is 11.
                    'h' => {
                        let v = f.hour_of_day % 12;
                        let v = if v == 0 { 12 } else { v };
                        zero_padding_number(out, v as i64, count, usize::MAX);
                    }
                    // 'K' — 0..11, no wrap.
                    'K' => zero_padding_number(out, (f.hour_of_day % 12) as i64, count, usize::MAX),
                    'm' => zero_padding_number(out, f.minute as i64, count, usize::MAX),
                    's' => zero_padding_number(out, f.second as i64, count, usize::MAX),
                    'S' => zero_padding_number(out, f.millis as i64, count, usize::MAX),
                    'E' => {
                        let table = if count >= 4 {
                            &syms.weekdays
                        } else {
                            &syms.short_weekdays
                        };
                        out.push_str(table.get(f.day_of_week as usize)?);
                    }
                    'a' => {
                        out.push_str(syms.ampms.get(usize::from(f.hour_of_day >= 12))?);
                    }
                    // 'Z' — RFC 822, always +hhmm / -hhmm (5 chars with sign).
                    'Z' => {
                        let mins = inputs.offset_ms / 60_000;
                        let hhmm = (mins / 60) * 100 + (mins % 60);
                        if mins >= 0 {
                            out.push('+');
                            zero_padding_number(out, hhmm as i64, 4, 4);
                        } else {
                            // The JDK hands a NEGATIVE value to
                            // zeroPaddingNumber with width 5, so DecimalFormat
                            // emits the sign and pads the magnitude to 4.
                            out.push('-');
                            zero_padding_number(out, (-hhmm) as i64, 4, 4);
                        }
                    }
                    // 'X' — ISO 8601. The JDK throws for count > 3.
                    'X' => {
                        if count > 3 {
                            return None;
                        }
                        if inputs.offset_ms == 0 {
                            out.push('Z');
                        } else {
                            let mut mins = inputs.offset_ms / 60_000;
                            if mins >= 0 {
                                out.push('+');
                            } else {
                                out.push('-');
                                mins = -mins;
                            }
                            zero_padding_number(out, (mins / 60) as i64, 2, 2);
                            if count != 1 {
                                if count == 3 {
                                    out.push(':');
                                }
                                zero_padding_number(out, (mins % 60) as i64, 2, 2);
                            }
                        }
                    }
                    _ => return None,
                }
            }
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Fallback: run the real bytecode
// ---------------------------------------------------------------------------

/// `DateFormat.format(Date)`'s own non-`java.text` branch, run as bytecode:
/// `format(date, new StringBuffer(), DontCareFieldPosition.INSTANCE).toString()`.
/// Both branches of the real method produce the same string; the `StringBuf`
/// one is only an allocation optimisation.
fn format_via_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    date: Option<ObjectRef>,
) -> Result<Option<Value>, MethodCallFailed> {
    // Two allocations and two calls follow, any of which can move these
    // objects — pin and re-read (family-1 shape).
    let this_pin = ctx.pin_native_root(this);
    let date_pin = date.map(|d| ctx.pin_native_root(d));
    let sb = match ctx.new_object_initialized("java/lang/StringBuffer", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::NullPointerException {
                message: Some("StringBuffer <init> produced no object".to_string()),
            }
            .into());
        }
    };
    let sb_pin = ctx.pin_native_root(sb);
    let fp =
        match ctx.new_object_initialized("java/text/FieldPosition", "(I)V", &[Value::Int(0)])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Err(RuntimeError::NullPointerException {
                    message: Some("FieldPosition <init> produced no object".to_string()),
                }
                .into());
            }
        };
    let this_now = ctx.read_native_pin(this_pin, this);
    let date_now = match (date, date_pin) {
        (Some(d), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, d))),
        // A null Date: hand it on so the JDK raises its own NPE, from its own
        // frame, with its own message.
        _ => Value::Object(None),
    };
    let sb_now = ctx.read_native_pin(sb_pin, sb);
    let result = ctx.invoke_virtual_bytecode_only(
        this_now,
        "format",
        "(Ljava/util/Date;Ljava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
        &[date_now, Value::Object(Some(sb_now)), Value::Object(Some(fp))],
    );
    ctx.unpin_native_roots(this_pin);
    let buf = match result? {
        Some(Value::Object(Some(o))) => o,
        other => {
            return Ok(other);
        }
    };
    ctx.invoke_virtual(buf, "toString", "()Ljava/lang/String;", &[])
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Gather the receiver state the fast path needs, or `None` to decline.
fn gather(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    this: ObjectRef,
    date: ObjectRef,
) -> Option<Inputs> {
    // The receiver must be exactly SimpleDateFormat: `format(Date)` is final on
    // DateFormat but dispatches to a VIRTUAL 3-arg format a subclass may own.
    // ClassId equality, not a name compare — a redefined class gets a new id,
    // which is exactly the invalidation the cached slots need.
    if ctx.class_id_of_object(this) != sl.sdf_class {
        return None;
    }
    if ctx.class_id_of_object(date) != sl.date_class {
        return None;
    }
    // A locale whose digits are not ASCII '0'..'9' reaches DecimalFormat.
    match ctx.get_field(this, sl.sdf_zero_digit).as_int() {
        // 0 means "not yet cached"; the bytecode caches it on first use, so
        // decline until it has.
        Some(z) if z == '0' as i32 => {}
        _ => return None,
    }
    if ctx
        .get_field(this, sl.sdf_force_standalone)
        .as_int()
        .unwrap_or(1)
        != 0
    {
        return None;
    }

    let calendar = obj_slot(ctx, this, sl.df_calendar)?;
    if ctx.class_id_of_object(calendar) != sl.gcal_class {
        return None;
    }
    // A moved cutover changes which rules apply where; only the default is
    // modelled.
    match ctx.get_field(calendar, sl.gcal_cutover) {
        Value::Long(v) if v == DEFAULT_CUTOVER_MILLIS => {}
        _ => return None,
    }
    let symbols = obj_slot(ctx, this, sl.sdf_format_data)?;

    // `Date.getTime()` is `getTimeImpl()`, which normalises first when a
    // deprecated setter left `cdate` dirty. With no `cdate` there is nothing to
    // normalise and `fastTime` is the answer.
    if obj_slot(ctx, date, sl.date_cdate).is_some() {
        return None;
    }
    let millis = match ctx.get_field(date, sl.date_fast_time) {
        Value::Long(v) => v,
        _ => return None,
    };
    if millis < FIRST_FULLY_GREGORIAN_MILLIS {
        return None;
    }

    // The UTC offset — raw + DST, exactly what `GregorianCalendar.computeFields`
    // adds. For the two `TimeZone` classes this VM implements natively itself
    // the answer comes straight out of `tzdb`, saving a Java dispatch and a
    // native-funnel entry per format (~400-760 ns, measured). Anything else may
    // be an application subclass overriding `getOffset`, so it gets the real
    // virtual call.
    let zone = obj_slot(ctx, calendar, sl.cal_zone)?;
    let zone_class = ctx.class_id_of_object(zone);
    // `java/util/SimpleTimeZone` is deliberately NOT here — see
    // `docs/known-issues/jdk-only/D3-1-simpledateformat-format-zone-arm.md` and
    // `E1-1-simpledateformat-format-zone-arm-landed.md`. The fast arm below
    // answers from the zone's `ID` FIELD via tzdb. That is right for a
    // `ZoneInfo` (whose id IS the zone) and for the abstract `TimeZone` itself
    // (an instance of that exact class can only be one this VM fabricated). It
    // is wrong for a `java.util.SimpleTimeZone`, whose id is by contract an
    // opaque LABEL and whose offset is the `rawOffset` its constructor stored:
    // `new SimpleTimeZone(0, "America/Sao_Paulo")` formatted an instant at
    // -03:00 here and at +00:00 on HotSpot. Nothing in this VM fabricates a
    // `SimpleTimeZone` any more (`alloc_synth_timezone`) and nothing implements
    // its `getOffset` (`register_tzdb_offset_natives_for`), so every such
    // receiver is one the APPLICATION built. The `else` arm's virtual call
    // reaches its real bytecode, which is correct for it: `SimpleTimeZone`
    // DECLARES `getOffset(J)I` with code, so `invoke_or_native`'s
    // `has_own_bytecode` gate skips the superclass climb and
    // `java/util/TimeZone`'s surviving tzdb native does not capture it either.
    let vm_implemented =
        Some(zone_class) == sl.zoneinfo_class || Some(zone_class) == sl.timezone_class;
    let offset_ms = if vm_implemented {
        let (rules, id) = zone_rules_cached(ctx, sl, zone)?;
        match rules {
            // The same rule `tzdb::legacy_offsets_ms` applies, against rules
            // already resolved for this zone object — no string-keyed catalog
            // lookup per format.
            Some(r) if millis.div_euclid(1000) >= crate::tzdb::ZONEINFO_LEGACY_FLOOR_EPOCH_SEC => {
                crate::tzdb::offset_at_instant(&r, millis.div_euclid(1000)).saturating_mul(1000)
            }
            _ => crate::tzdb::legacy_offsets_ms(ctx, &id, millis).0,
        }
    } else {
        match ctx.invoke_virtual(zone, "getOffset", "(J)I", &[Value::Long(millis)]) {
            Ok(Some(v)) => v.as_int()?,
            _ => return None,
        }
    };

    let local = millis.checked_add(offset_ms as i64)?;
    let fields = fields_from_local_millis(local);
    // The zone offset can pull a UTC instant just after the boundary back into
    // 1582 locally, so re-check on the LOCAL year. This also covers the BC era
    // (`YEAR = 1 - y` with `ERA = 0`), which is out of scope.
    if fields.year <= 1582 {
        return None;
    }
    Some(Inputs {
        millis,
        offset_ms,
        fields,
        calendar,
        symbols,
    })
}

/// Reproduce `calendar.setTime(date)`'s observable effect without paying for
/// its eager `computeFields()`.
///
/// `Calendar.setTimeInMillis` stores the instant, marks the fields stale and
/// then recomputes them EAGERLY — that recompute is 20-40 us on this VM and is
/// the bulk of what this native exists to avoid. Leaving the fields marked
/// stale is observationally identical: `Calendar.get`/`complete()` recompute
/// on demand, and `getTimeInMillis()` reads `time` either way.
fn stamp_calendar(ctx: &mut dyn NativeContext, sl: &Slots, calendar: ObjectRef, millis: i64) {
    // Write only what actually changes. `set_field` re-derives the slot's field
    // descriptor to coerce the value, and that showed up at 15.4% of the native
    // in a perf profile — four writes per format, four different slots, so the
    // VM's one-entry descriptor cache missed every time. Three of the four
    // fields are already at the value we want after the first format (the flags
    // only move again when `Calendar.complete()` runs), so a read-and-skip
    // turns three writes into three much cheaper reads.
    if ctx.get_field(calendar, sl.cal_time) != Value::Long(millis) {
        ctx.set_field(calendar, sl.cal_time, Value::Long(millis));
    }
    if ctx.get_field(calendar, sl.cal_is_time_set).as_int() != Some(1) {
        ctx.set_field(calendar, sl.cal_is_time_set, Value::Int(1));
    }
    if ctx.get_field(calendar, sl.cal_are_fields_set).as_int() != Some(0) {
        ctx.set_field(calendar, sl.cal_are_fields_set, Value::Int(0));
    }
    if ctx.get_field(calendar, sl.cal_are_all_fields_set).as_int() != Some(0) {
        ctx.set_field(calendar, sl.cal_are_all_fields_set, Value::Int(0));
    }
}

/// The zone's tzdb id, cached by the zone object's identity so the Java
/// `String` is decoded once per zone rather than once per format.
///
/// A `TimeZone`'s `ID` is effectively immutable in practice, but `setID` exists
/// — so the cache stores the identity of the ID STRING alongside, and a
/// different string object re-reads. That check is two integer compares.
#[allow(clippy::type_complexity)]
fn zone_rules_cached(
    ctx: &mut dyn NativeContext,
    sl: &Slots,
    zone: ObjectRef,
) -> Option<(
    Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
    std::sync::Arc<str>,
)> {
    let id_obj = obj_slot(ctx, zone, sl.tz_id)?;
    let id_identity = ctx.identity_hash_code(id_obj);
    let zone_identity = ctx.identity_hash_code(zone);
    ZONE_RULES.with(|c| {
        let mut slot = c.borrow_mut();
        if let Some((z, i, rules, id)) = slot.as_ref() {
            if *z == zone_identity && *i == id_identity {
                return Some((rules.clone(), id.clone()));
            }
        }
        let text: std::sync::Arc<str> = std::sync::Arc::from(ctx.read_string(id_obj)?.as_str());
        let rules = crate::tzdb::get_zone_rules(ctx, &text);
        *slot = Some((zone_identity, id_identity, rules.clone(), text.clone()));
        Some((rules, text))
    })
}

#[allow(clippy::type_complexity)]
mod zone_rules_tls {
    pub(super) type Slot = Option<(
        i32,
        i32,
        Option<std::sync::Arc<crate::tzdb::ZoneRulesData>>,
        std::sync::Arc<str>,
    )>;
}

thread_local! {
    /// Last (`TimeZone` object, its `ID` string object) -> resolved tzdb rules.
    /// A `TimeZone`'s `ID` can in principle be reassigned (`setID`), so the ID
    /// string's identity is part of the key; that check is two integer compares
    /// against a per-format string decode plus a catalog lookup.
    static ZONE_RULES: std::cell::RefCell<zone_rules_tls::Slot> =
        const { std::cell::RefCell::new(None) };
}

fn shape_key(inputs: &Inputs) -> ShapeKey {
    (
        inputs.fields.month0 as u8,
        inputs.fields.day_of_week as u8,
        u8::from(inputs.fields.year <= 0),
        u8::from(inputs.fields.hour_of_day >= 12),
        inputs.offset_ms != 0,
        inputs.offset_ms < 0,
    )
}

pub(crate) fn register_date_format_fast(r: &mut NativeMethodRegistry) {
    let prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "java/text/DateFormat",
        "format",
        "(Ljava/util/Date;)Ljava/lang/String;",
        |ctx, args| -> MethodCallResult {
            let this = obj_arg(args, 0)?;
            let date = match args.get(1) {
                Some(Value::Object(Some(d))) => *d,
                // null Date -> the bytecode's own NullPointerException.
                _ => return format_via_bytecode(ctx, this, None),
            };

            let Some(sl) = slots(ctx) else {
                return format_via_bytecode(ctx, this, Some(date));
            };
            let Some(inputs) = gather(ctx, sl, this, date) else {
                return format_via_bytecode(ctx, this, Some(date));
            };

            // Three identities decide whether a cached plan still applies:
            // the formatter, the `String` its `pattern` field points at
            // (`applyPattern` installs a different object) and its
            // `DateFormatSymbols` (`setDateFormatSymbols` likewise).
            let this_id = ctx.identity_hash_code(this);
            let Some(pattern_obj) = obj_slot(ctx, this, sl.sdf_pattern) else {
                return format_via_bytecode(ctx, this, Some(date));
            };
            let pattern_id = ctx.identity_hash_code(pattern_obj);
            let symbols_id = ctx.identity_hash_code(inputs.symbols);
            let shape = shape_key(&inputs);

            // ---- Steady state: no locks, no hashing, no re-parse ----------
            let hit = LAST_PLAN.with(|c| {
                let last = c.borrow();
                match last.as_ref() {
                    Some(l)
                        if l.this_id == this_id
                            && l.pattern_id == pattern_id
                            && l.symbols_id == symbols_id
                            && l.verified.contains(&shape) =>
                    {
                        Some((l.pieces.clone(), l.symbols.clone()))
                    }
                    _ => None,
                }
            });
            if let Some((pieces, syms)) = hit {
                let rendered = SCRATCH.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    render(&pieces, &inputs, &syms, &mut buf).map(|()| buf.clone())
                });
                if let Some(text) = rendered {
                    stamp_calendar(ctx, sl, inputs.calendar, inputs.millis);
                    let s = ctx.create_string(&text);
                    return Ok(Some(Value::Object(Some(s))));
                }
                return format_via_bytecode(ctx, this, Some(date));
            }

            // ---- Slow path: consult (and repair) the shared plan cache ----
            if plan_cache().lock().get(&this_id).is_some_and(|e| e.poisoned) {
                return format_via_bytecode(ctx, this, Some(date));
            }
            let (pieces, pattern, syms) = {
                {
                let mut cache = plan_cache().lock();
                let entry = cache.entry(this_id).or_insert_with(|| Entry {
                    plan: None,
                    poisoned: false,
                    verified: Default::default(),
                });
                let stale = entry
                    .plan
                    .as_ref()
                    .map(|p| p.pattern_id != pattern_id || p.symbols_id != symbols_id)
                    .unwrap_or(true);
                if stale {
                    entry.verified.clear();
                    entry.plan = None;
                    // Drop the lock before decoding the String: `read_string`
                    // allocates, and an allocation under this lock would put a
                    // GC on the wrong side of it.
                    drop(cache);
                    let Some(text) = ctx.read_string(pattern_obj) else {
                        return format_via_bytecode(ctx, this, Some(date));
                    };
                    let compiled = compile_pattern(&text);
                    let syms = std::sync::Arc::new(read_symbols(ctx, sl, inputs.symbols));
                    let mut cache = plan_cache().lock();
                    let entry = cache.get_mut(&this_id).expect("entry inserted above");
                    entry.plan = compiled.map(|pieces| Plan {
                        pattern: text,
                        pattern_id,
                        symbols_id,
                        pieces: std::sync::Arc::new(pieces),
                        symbols: syms,
                    });
                }
                }
                let cache = plan_cache().lock();
                match cache.get(&this_id).and_then(|e| e.plan.as_ref()) {
                    Some(p) => (p.pieces.clone(), p.pattern.clone(), p.symbols.clone()),
                    None => return format_via_bytecode(ctx, this, Some(date)),
                }
            };

            let mut fast = String::new();
            if render(&pieces, &inputs, &syms, &mut fast).is_none() {
                return format_via_bytecode(ctx, this, Some(date));
            }

            // First time we produce this OUTPUT SHAPE, prove it against the
            // bytecode. See the module doc.
            let needs_check = {
                let mut cache = plan_cache().lock();
                let entry = cache.get_mut(&this_id).expect("entry inserted above");
                entry.verified.insert(shape)
            };
            if needs_check {
                let reference = format_via_bytecode(ctx, this, Some(date))?;
                let reference_str = match reference {
                    Some(Value::Object(Some(s))) => ctx.read_string(s),
                    _ => None,
                };
                if reference_str.as_deref() != Some(fast.as_str()) {
                    {
                        let mut cache = plan_cache().lock();
                        if let Some(entry) = cache.get_mut(&this_id) {
                            entry.poisoned = true;
                        }
                    }
                    LAST_PLAN.with(|c| *c.borrow_mut() = None);
                    // Unconditionally, not behind the debug flag: a divergence
                    // here is a real defect in this module and must not be
                    // discoverable only by someone who already suspected it.
                    eprintln!(
                        "[cratonvm] date-format fast path DISABLED for pattern {pattern:?}:                          produced {fast:?} but the JDK produced {reference_str:?};                          falling back to bytecode for this formatter"
                    );
                    return Ok(reference);
                }
                if dbg_on() {
                    eprintln!(
                        "[dbg-dateformat] verified pattern {pattern:?} shape {shape:?} -> {fast:?}"
                    );
                }
            } else {
                stamp_calendar(ctx, sl, inputs.calendar, inputs.millis);
            }

            // Publish to the thread-local now that this shape is proven.
            {
                let verified = plan_cache()
                    .lock()
                    .get(&this_id)
                    .map(|e| e.verified.clone())
                    .unwrap_or_default();
                LAST_PLAN.with(|c| {
                    *c.borrow_mut() = Some(LastPlan {
                        this_id,
                        pattern_id,
                        symbols_id,
                        pieces: pieces.clone(),
                        symbols: syms.clone(),
                        verified,
                    })
                });
            }
            let s = ctx.create_string(&fast);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.set_category(prev);

    // G28-1: the `sun.util.calendar.ZoneInfo` daylight-saving family
    // (`getDSTSavings`, `useDaylightTime`, `observesDaylightTime`,
    // `inDaylightTime(Date)` and the six-argument `getOffset`). It is a strange
    // address for it and this comment exists to say why it is here anyway.
    //
    // The obvious home is `util_time.rs`, next to the other `java.time` doors.
    // That module is `#[cfg(feature = "synthetic-jdk")]` and IS NOT COMPILED
    // INTO THE DEFAULT BUILD (`lib.rs`, the `mod util_time` declaration): a
    // registration added there would be dead code that reads as a fix, which
    // `HANDOFF-20260814` §5 records as a trap this campaign has fallen into
    // twice. MEASURED, not assumed: `--dump-native-registry` under `--jdk-only`
    // carries `java/text/DateFormat format ... owns_slot=true
    // by=date_format_fast.rs`, and names `util_time.rs` in no row at all. So
    // `register_date_format_fast` demonstrably runs on the real-JDK path and
    // this one does not.
    //
    // Relocating the call next to `register_tzdb_offset_natives_for` in
    // `lib.rs` -- which is not this lane's file -- is NOMINATED in
    // `docs/known-issues/jdk-only/G28-1-the-dst-rule-layer-rebuilt-20260817.md`.
    // Whoever moves it: register on `ZoneInfo` ONLY. The reason the base class
    // is excluded is on `register_zoneinfo_dst_natives` itself, and following
    // the neighbouring call's two-class shape would silently undo it.
    crate::tzdb::register_zoneinfo_dst_natives(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_the_juli_pattern() {
        let p = compile_pattern("dd-MMM-yyyy HH:mm:ss").expect("supported");
        assert_eq!(
            p,
            vec![
                Piece::Field {
                    letter: 'd',
                    count: 2
                },
                Piece::Literal("-".into()),
                Piece::Field {
                    letter: 'M',
                    count: 3
                },
                Piece::Literal("-".into()),
                Piece::Field {
                    letter: 'y',
                    count: 4
                },
                Piece::Literal(" ".into()),
                Piece::Field {
                    letter: 'H',
                    count: 2
                },
                Piece::Literal(":".into()),
                Piece::Field {
                    letter: 'm',
                    count: 2
                },
                Piece::Literal(":".into()),
                Piece::Field {
                    letter: 's',
                    count: 2
                },
            ]
        );
    }

    #[test]
    fn quoting_matches_the_jdk_rules() {
        assert_eq!(
            compile_pattern("'at' HH''mm").unwrap(),
            vec![
                Piece::Literal("at ".into()),
                Piece::Field {
                    letter: 'H',
                    count: 2
                },
                Piece::Literal("'".into()),
                Piece::Field {
                    letter: 'm',
                    count: 2
                },
            ]
        );
        // An unterminated quote is an IllegalArgumentException in the JDK — we
        // decline so the bytecode raises it.
        assert!(compile_pattern("HH'mm").is_none());
    }

    #[test]
    fn declines_unsupported_and_unknown_letters() {
        for p in ["yyyy-MM-dd z", "yyyy 'w' w", "LLLL", "YYYY", "u", "F"] {
            assert!(compile_pattern(p).is_none(), "{p} must decline");
        }
        // Not a pattern letter at all: the JDK throws, so we decline too.
        assert!(compile_pattern("qqq").is_none());
    }

    #[test]
    fn zero_padding_matches_both_jdk_arms() {
        let mut s = String::new();
        zero_padding_number(&mut s, 5, 2, usize::MAX);
        assert_eq!(s, "05");
        s.clear();
        zero_padding_number(&mut s, 5, 1, usize::MAX);
        assert_eq!(s, "5");
        s.clear();
        zero_padding_number(&mut s, 2023, 4, usize::MAX);
        assert_eq!(s, "2023");
        s.clear();
        // `yy` clips 2023 to 23 — min 2, max 2.
        zero_padding_number(&mut s, 2023, 2, 2);
        assert_eq!(s, "23");
        s.clear();
        // A year wider than the count is NOT truncated when max is unbounded.
        zero_padding_number(&mut s, 12345, 4, usize::MAX);
        assert_eq!(s, "12345");
        s.clear();
        zero_padding_number(&mut s, 7, 3, usize::MAX);
        assert_eq!(s, "007");
    }

    #[test]
    fn civil_dates_match_known_points() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 2023-11-14, the fixed instant the probes use.
        assert_eq!(civil_from_days(19675), (2023, 11, 14));
        // Leap day.
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
    }

    #[test]
    fn day_of_week_anchors_on_a_known_thursday() {
        // 1970-01-01 was a Thursday; Calendar.THURSDAY == 5.
        assert_eq!(fields_from_local_millis(0).day_of_week, 5);
        // …so the following Sunday is 1.
        assert_eq!(fields_from_local_millis(3 * MS_PER_DAY).day_of_week, 1);
    }

    #[test]
    fn day_of_year_handles_leap_years() {
        assert_eq!(day_of_year(2023, 1, 1), 1);
        assert_eq!(day_of_year(2023, 12, 31), 365);
        assert_eq!(day_of_year(2024, 12, 31), 366);
        assert_eq!(day_of_year(2024, 3, 1), 61);
    }

    #[test]
    fn fields_split_the_day_correctly() {
        let f = fields_from_local_millis(1_700_000_000_000 - 8 * 3_600_000);
        assert_eq!((f.year, f.month0 + 1, f.day), (2023, 11, 14));
        assert_eq!((f.hour_of_day, f.minute, f.second), (14, 13, 20));
        assert_eq!(f.millis, 0);
    }
}
