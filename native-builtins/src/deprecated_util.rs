// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T8.2 — Deprecated java.util / java.lang / java.io API native implementations.
//!
//! Implements all deprecated API natives called out in T8.2.  Each sub-item
//! follows the same registration pattern as deprecated_lang.rs.
//!
//! Skipped (already done elsewhere):
//!   T8.2.6  — Class.newInstance()         (deprecated_io_util.rs)
//!   T8.2.7  — Number.byteValue/shortValue (deprecated_io_util.rs)
//!   T8.2.13 — URLDecoder.decode(String)   (deprecated_io_util.rs)
//!   T8.2.14 — URLEncoder.encode(String)   (deprecated_io_util.rs)

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
#[cfg(test)]
use cratonvm_types::ArrayElementType;
use cratonvm_types::ObjectRef;
use cratonvm_types::Value;

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Calendar math helpers
// ---------------------------------------------------------------------------

/// Structured date/time components extracted from an epoch-millis value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateParts {
    /// Full Gregorian year (e.g. 2000).
    pub year: i32,
    /// Month, 0-based (0 = January … 11 = December).
    pub month: i32,
    /// Day of month, 1-based.
    pub date: i32,
    /// Hour of day, 0–23.
    pub hrs: i32,
    /// Minute, 0–59.
    pub min: i32,
    /// Second, 0–59.
    pub sec: i32,
    /// Day of week, 0 = Sunday … 6 = Saturday.
    pub day_of_week: i32,
}

/// Returns `true` when `year` is a Gregorian leap year.
#[inline]
fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in `month` (0-based) for the given `year` (full Gregorian).
#[inline]
fn days_in_month(year: i32, month: i32) -> i32 {
    const TABLE: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 1 && is_leap(year) {
        29
    } else {
        TABLE[month as usize]
    }
}

/// Julian Day Number of the default `GregorianCalendar` cutover instant
/// (1582-10-15, proleptic Gregorian) — the earliest date `Date`'s deprecated
/// field-based constructors (and `GregorianCalendar` itself, by default)
/// interpret as Gregorian. Anything earlier is interpreted as a Julian
/// calendar date, per `java.util.GregorianCalendar`'s documented default
/// cutover (`Date(Long.MIN_VALUE)` disables this; that path is unaffected
/// here since it never calls these helpers).
const CUTOVER_JDN: i64 = 2_299_161;
/// Julian Day Number of the Unix epoch (1970-01-01, proleptic Gregorian).
const JDN_UNIX_EPOCH: i64 = 2_440_588;

/// Proleptic-Gregorian (year, 1-based month, day) -> Julian Day Number.
/// Fliegel & Van Flandern (1968); all divisions truncate toward zero,
/// matching the formula's original (C-style) integer-division semantics.
#[inline]
fn gregorian_to_jdn(year: i64, month: i64, day: i64) -> i64 {
    let a = (month - 14) / 12;
    let term1 = (1461 * (year + 4800 + a)) / 4;
    let term2 = (367 * (month - 2 - 12 * a)) / 12;
    let term3 = (3 * ((year + 4900 + a) / 100)) / 4;
    term1 + term2 - term3 + day - 32075
}

/// Proleptic-Julian (year, 1-based month, day) -> Julian Day Number.
/// Fliegel & Van Flandern (1968), Julian-calendar variant.
#[inline]
fn julian_to_jdn(year: i64, month: i64, day: i64) -> i64 {
    let inner = (month - 9) / 7;
    367 * year - (7 * (year + 5001 + inner)) / 4 + (275 * month) / 9 + day + 1_729_777
}

/// Julian Day Number -> proleptic-Gregorian (year, 1-based month, day).
/// Fliegel & Van Flandern inverse formula.
#[inline]
fn jdn_to_gregorian(jdn: i64) -> (i64, i64, i64) {
    let mut l = jdn + 68_569;
    let n = (4 * l) / 146_097;
    l -= (146_097 * n + 3) / 4;
    let mut y = (4000 * (l + 1)) / 1_461_001;
    l = l - (1461 * y) / 4 + 31;
    let m = (80 * l) / 2447;
    let d = l - (2447 * m) / 80;
    let l2 = m / 11;
    let month = m + 2 - 12 * l2;
    y = 100 * (n - 49) + y + l2;
    (y, month, d)
}

/// Julian Day Number -> proleptic-Julian (year, 1-based month, day).
/// Fliegel & Van Flandern inverse formula, Julian-calendar variant.
#[inline]
fn jdn_to_julian(jdn: i64) -> (i64, i64, i64) {
    let c = jdn + 32_082;
    let d2 = (4 * c + 3) / 1461;
    let e = c - (1461 * d2) / 4;
    let m2 = (5 * e + 2) / 153;
    let d = e - (153 * m2 + 2) / 5 + 1;
    let m = m2 + 3 - 12 * (m2 / 10);
    let y = d2 - 4800 + (m2 / 10);
    (y, m, d)
}

/// Convert (year, month, date, hrs, min, sec) to Unix epoch milliseconds.
///
/// - `year`  — full Gregorian year (not the +1900 deprecated form; callers
///   must add 1900 before calling this function).
/// - `month` — 0-based (0 = January).
/// - `date`  — 1-based day of month.
///
/// Matches `java.util.Date`'s deprecated multi-arg constructors, which are
/// specified in terms of `GregorianCalendar`'s default Julian/Gregorian
/// hybrid calendar: dates before 1582-10-15 (Gregorian) are interpreted as
/// Julian calendar dates, not proleptic-Gregorian ones — see
/// `bug-h2-suite-residual-fail-triage-FIXED.md`'s
/// `TestPreparedStatement.testDate8` writeup for the ~10-day discrepancy
/// this produces if the cutover is ignored.
pub fn date_fields_to_millis(
    year: i32,
    month: i32,
    date: i32,
    hrs: i32,
    min: i32,
    sec: i32,
) -> i64 {
    let (y, m, d) = (year as i64, month as i64 + 1, date as i64);
    let greg_jdn = gregorian_to_jdn(y, m, d);
    let jdn = if greg_jdn >= CUTOVER_JDN {
        greg_jdn
    } else {
        julian_to_jdn(y, m, d)
    };
    let days = jdn - JDN_UNIX_EPOCH;

    days * 86_400_000 + hrs as i64 * 3_600_000 + min as i64 * 60_000 + sec as i64 * 1_000
}

/// Decompose Unix epoch milliseconds into a [`DateParts`] struct.
///
/// Inverse of [`date_fields_to_millis`] — same Julian/Gregorian hybrid
/// cutover applies (dates before 1582-10-15 Gregorian decode as Julian
/// calendar fields).
pub fn millis_to_date_parts(millis: i64) -> DateParts {
    // Seconds / minutes / hours
    let sec_total = millis.div_euclid(1_000);
    let sec = sec_total.rem_euclid(60) as i32;
    let min_total = sec_total.div_euclid(60);
    let min = min_total.rem_euclid(60) as i32;
    let hr_total = min_total.div_euclid(60);
    let hrs = hr_total.rem_euclid(24) as i32;
    let day_count = hr_total.div_euclid(24); // days since Unix epoch (may be negative)

    // Day of week: 1970-01-01 was a Thursday (= 4, with Sunday = 0).
    let day_of_week = ((day_count.rem_euclid(7) + 4) % 7) as i32;

    let jdn = day_count + JDN_UNIX_EPOCH;
    let (year, month_1based, date) = if jdn >= CUTOVER_JDN {
        jdn_to_gregorian(jdn)
    } else {
        jdn_to_julian(jdn)
    };

    DateParts {
        year: year as i32,
        month: (month_1based - 1) as i32,
        date: date as i32,
        hrs,
        min,
        sec,
        day_of_week,
    }
}

fn default_time_zone_ref(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name("java/util/TimeZone")?;
    let field_index = ctx.static_field_index_by_name(class_id, "defaultTimeZone")?;
    match ctx.get_static_field(class_id, field_index) {
        Value::Object(Some(tz)) => Some(tz),
        _ => None,
    }
}

fn timezone_offset_at_millis(ctx: &mut dyn NativeContext, tz: ObjectRef, millis: i64) -> i32 {
    match ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]) {
        Ok(Some(Value::Int(offset))) => offset,
        _ => 0,
    }
}

pub fn millis_to_default_date_parts(ctx: &mut dyn NativeContext, millis: i64) -> DateParts {
    let offset = default_time_zone_ref(ctx)
        .map(|tz| timezone_offset_at_millis(ctx, tz, millis) as i64)
        .unwrap_or(0);
    millis_to_date_parts(millis.saturating_add(offset))
}

pub fn date_fields_to_default_millis(
    ctx: &mut dyn NativeContext,
    year: i32,
    month: i32,
    date: i32,
    hrs: i32,
    min: i32,
    sec: i32,
) -> i64 {
    let local_millis = date_fields_to_millis(year, month, date, hrs, min, sec);
    let Some(tz) = default_time_zone_ref(ctx) else {
        return local_millis;
    };
    let mut candidate =
        local_millis.saturating_sub(timezone_offset_at_millis(ctx, tz, local_millis) as i64);
    for _ in 0..4 {
        let next =
            local_millis.saturating_sub(timezone_offset_at_millis(ctx, tz, candidate) as i64);
        if next == candidate {
            break;
        }
        candidate = next;
    }
    candidate
}

// ---------------------------------------------------------------------------
// Field accessors for java.util.Date (field 0 = epoch millis as Long)
// ---------------------------------------------------------------------------

fn get_date_millis(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> i64 {
    match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

fn set_date_millis(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef, millis: i64) {
    ctx.set_field(this, 0, Value::Long(millis));
}

// ---------------------------------------------------------------------------
// T8.2.1 — Date multi-arg constructors
// ---------------------------------------------------------------------------

fn native_date_init_iii(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 70,
    };
    let month = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let date = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let millis = date_fields_to_default_millis(ctx, year + 1900, month, date, 0, 0, 0);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_init_iiiii(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 70,
    };
    let month = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let date = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let hrs = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let min = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let millis = date_fields_to_default_millis(ctx, year + 1900, month, date, hrs, min, 0);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_init_iiiiii(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 70,
    };
    let month = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let date = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let hrs = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let min = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let sec = match args.get(6) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let millis = date_fields_to_default_millis(ctx, year + 1900, month, date, hrs, min, sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

/// Date(String) — deprecated; throws UnsupportedOperationException.
/// `java.util.Date(String)` — deprecated, but still reachable and still
/// expected to work: real JDK defines it as `this(parse(s))`, and Spring's
/// `ObjectToObjectConverter` picks it up as the String -> Date conversion for
/// anything the formatting conversion service does not handle itself. Throwing
/// `UnsupportedOperationException` here made `@RequestHeader java.util.Date`
/// binding fail for an ordinary RFC-1123 header value
/// (`web.method.annotation.RequestHeaderMethodArgumentResolverTests
/// .dateConversion` and its `web.reactive` twin).
///
/// `Date.parse(String)` has no native override, so this delegates to the real
/// JDK bytecode for the parsing itself rather than re-implementing the
/// (surprisingly permissive) legacy grammar.
fn native_date_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let text = args.get(1).copied().unwrap_or(Value::Object(None));
    let millis = match ctx.invoke("java/util/Date", "parse", "(Ljava/lang/String;)J", &[text])? {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as i64,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Date(String): unparseable date".to_string(),
            }
            .into())
        }
    };
    set_date_millis(ctx, this, millis);
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.2.2 — Date getters / setters
// ---------------------------------------------------------------------------

fn native_date_get_year(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.year - 1900)))
}

fn native_date_get_month(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.month)))
}

fn native_date_get_date(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.date)))
}

fn native_date_get_day(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.day_of_week)))
}

fn native_date_get_hours(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.hrs)))
}

fn native_date_get_minutes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.min)))
}

fn native_date_get_seconds(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    Ok(Some(Value::Int(p.sec)))
}

fn native_date_get_timezone_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let millis = get_date_millis(ctx, this);
    let offset = default_time_zone_ref(ctx)
        .map(|tz| timezone_offset_at_millis(ctx, tz, millis))
        .unwrap_or(0);
    Ok(Some(Value::Int(-offset / 60_000)))
}

fn native_date_set_year(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 70,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis =
        date_fields_to_default_millis(ctx, new_year + 1900, p.month, p.date, p.hrs, p.min, p.sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_set_month(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_month = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis = date_fields_to_default_millis(ctx, p.year, new_month, p.date, p.hrs, p.min, p.sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_set_date(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_date = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis = date_fields_to_default_millis(ctx, p.year, p.month, new_date, p.hrs, p.min, p.sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_set_hours(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_hours = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis =
        date_fields_to_default_millis(ctx, p.year, p.month, p.date, new_hours, p.min, p.sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_set_minutes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_min = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis = date_fields_to_default_millis(ctx, p.year, p.month, p.date, p.hrs, new_min, p.sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

fn native_date_set_seconds(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_sec = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let current_millis = get_date_millis(ctx, this);
    let p = millis_to_default_date_parts(ctx, current_millis);
    let millis = date_fields_to_default_millis(ctx, p.year, p.month, p.date, p.hrs, p.min, new_sec);
    set_date_millis(ctx, this, millis);
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.2.3 — String(byte[], int hibyte, int offset, int count)
// ---------------------------------------------------------------------------

/// `<init>([BIII)V` — deprecated String constructor with hibyte argument.
/// Each resulting char = (hibyte << 8) | (byte[offset+i] & 0xFF).
fn native_string_init_hibyte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte_arr = obj_arg(args, 1)?;
    let hibyte = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let offset = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let count = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };

    let arr_len = ctx.array_length(byte_arr);
    // `checked_add`: a negative int argument arrives as a huge `usize`, and the
    // plain sum overflowed (a panic in debug builds) instead of throwing.
    if offset.checked_add(count).map_or(true, |end| end > arr_len) {
        // `String.checkBoundsOffCount(offset, count, length)`: the SIOOBE
        // "Range [o, o + c) out of bounds for length n" (wave 100 L7; this was
        // an AIOOBE "Array index out of range: n"). The `as` casts restore the
        // `int` arguments the `usize` reads above widened.
        return Err(RuntimeError::sioobe_range_size(
            offset as i32,
            count as i32,
            i32::try_from(arr_len).unwrap_or(i32::MAX),
        )
        .into());
    }

    // One bulk read (gc-common w20-b, see `read_byte_range`). A short read
    // -- not possible for the `byte[]` the descriptor admits -- keeps the old
    // loop's zero fill for the missing tail.
    let mut bytes = read_byte_range(ctx, byte_arr, offset, count);
    bytes.resize(count, 0);
    let chars: Vec<u16> = bytes
        .iter()
        .map(|&b| ((hibyte as u16) << 8) | (b as u16))
        .collect();

    // gc-common w19-g handoff: write `this` in place through the lossless
    // String writer, which decides the coder (the old copy of a second
    // String's `value` left `this` LATIN1 for UTF16 bytes, so every non-zero
    // `hibyte` read back as garbage, and held `this` raw across that String's
    // allocation). Its VM allocation does not collect, so a full young space
    // gets one reclaim-and-retry, with `this` pinned, before the OOME --
    // the shared `lang_string::init_string_from_units_or_oom` (gc-common
    // w20-b; this spelled the same ladder out by hand).
    // (`byte_arr` is not used after `chars` is built.)
    crate::lang_string::init_string_from_units_or_oom(
        ctx,
        this,
        &chars,
        "String(byte[],int,int,int)",
    )?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Cipher-probe support: byte[]→String constructors.
//
// The real-JDK private constructor `String(Charset, byte[], int, int)` does:
//   1. `Charset.defaultCharset()` (intercepted, returns a Charset stub).
//   2. Compares stub against `sun.nio.cs.UTF_8.INSTANCE` (a static the
//      bootstrap chain never populates because we no-op
//      `sun/nio/cs/UTF_8.<clinit>`).
//   3. Falls through every COMPACT_STRINGS branch with `value=new byte[0]`
//      and `coder=0` because the comparison is always false.
//
// Net result: `new String(byteArray)` returns an empty string regardless of
// input, breaking any byte[]-round-trip including CipherProbe's
// `"hello aes-gcm".equals(new String(pt, "UTF-8"))` check. These intercepts
// short-circuit all three paths (`(byte[])`, `(byte[],String)`,
// `(byte[],Charset)`) and their offset-aware variants, decoding the input
// bytes lossily as UTF-8 and writing the text into `this` in place through
// `init_string_from_units` (gc-common w20-b; it used to copy the fields of
// a fresh, INTERNED `create_string` object) — preserving the JDK 25
// compact-string invariant `coder == LATIN1 ⟺ value.length == codepoint count`.
// ---------------------------------------------------------------------------

/// Build a JVM String from `bytes` decoded as UTF-8 (lossy on invalid
/// sequences) in place into `this` (see [`string_init_from_text`]).
fn string_from_bytes_utf8(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    // UTF-8 lossy decode: invalid sequences become U+FFFD. This matches
    // `String(byte[], "UTF-8")` semantics in default-replacement mode (the
    // spec allows a CharsetDecoder to throw, but the public ctor uses
    // `CodingErrorAction.REPLACE` by default so this branch is correct for
    // the unguarded probe path).
    let text = std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned());
    string_init_from_text(ctx, this, &text, "String(byte[])")
}

/// Write `text` into the already-allocated String `this`.
///
/// gc-common w20-b. This used to build a SECOND String with the pooled
/// `create_string` and copy its `value` / `coder` into `this`:
/// - `create_string` INTERNS, so every distinct text ever decoded by
///   `new String(byte[] ..)` (a file's lines, every request body) became a
///   permanent string-pool row -- a strong root for the life of the VM;
/// - `this` was held raw across that String's allocation.
///
/// Now the text's UTF-16 units go through the lossless in-place writer, which
/// lays down the same compact layout (`populate_java_string_fields` decides
/// the coder exactly as the String constructors do: LATIN1 iff every unit
/// fits a byte), on the collecting retry ladder, with `this` pinned. The
/// hash / hashIsZero slots stay at their alloc-zero defaults as before.
fn string_init_from_text(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    text: &str,
    what: &str,
) -> MethodCallResult {
    let units: Vec<u16> = text.encode_utf16().collect();
    crate::lang_string::init_string_from_units_or_oom(ctx, this, &units, what)?;
    Ok(None)
}

/// Build a JVM String from `bytes` decoded with the named charset and copy
/// its layout onto `this`.
///
/// Routes through the same `engine::decode_bytes_lossy` path that
/// `CharsetDecoder.decode` uses, so ISO-8859-1 / Latin-1 / windows-12xx etc.
/// produce the spec-correct chars instead of UTF-8-substituted U+FFFD.
///
/// Empty/unrecognised charset names fall back to UTF-8 (matching the
/// `normalize_charset_name` → `decode_str_named` empty-name fallback).
fn string_from_bytes_with_charset(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    bytes: &[u8],
    charset_name: &str,
) -> MethodCallResult {
    let text = crate::charset::decode_str_named(charset_name, bytes);
    string_init_from_text(ctx, this, &text, "String(byte[],Charset)")
}

/// `count` bytes of the `byte[]` `arr` from `offset` (the caller has checked
/// the range), in one bulk copy.
///
/// gc-common w20-b (perf): the six `String(byte[] ..)` constructors read
/// their input one boxed `get_array_element` per byte -- a virtual call, a
/// bounds check and a `Value` match per byte of every decoded request body.
/// `read_byte_array_into` is one `copy_nonoverlapping` on the VM (it falls
/// back to the per-element read itself for a G1 humongous array); a short
/// read -- only possible for a non-`byte[]` argument, which the descriptor
/// excludes -- is truncated as the old loop skipped non-`Int` elements.
fn read_byte_range(
    ctx: &dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    offset: usize,
    count: usize,
) -> Vec<u8> {
    let mut bytes = vec![0u8; count];
    let n = ctx.read_byte_array_into(arr, offset, &mut bytes);
    bytes.truncate(n);
    bytes
}

/// `<init>([B)V` — `new String(byte[])`.
fn native_string_init_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let len = ctx.array_length(arr);
    let bytes = read_byte_range(ctx, arr, 0, len);
    string_from_bytes_utf8(ctx, this, &bytes)
}

/// `<init>(Ljava/lang/StringBuilder;)V` — `new String(StringBuilder)`.
///
/// Reads the builder's characters — through `sb_read_chars`, which understands
/// WHICHEVER of the two layouts the receiver holds — and initialises `this` from
/// them. This bypasses the real JDK `String(AbstractStringBuilder, Void)` ctor,
/// which `Arrays.copyOfRange`s `getValue()`; that was a `char[]` -> `byte[]` type
/// mismatch against a synthetic builder (Tomcat DF05).
///
/// The hand-rolled copy that used to stand in the body read slot 0 as a `char[]`
/// and answered an EMPTY vector for a compact `byte[]` payload — which is a
/// FABRICATION, not a refusal: the caller cannot tell an empty builder from one
/// whose content this constructor declined to look at. MEASURED, `--real-jdk`
/// with the builder registrations skipped so real bytecode owned the object: a
/// builder holding "abcdefgh" reported `length() == 8` and
/// `toString().length() == 0`, `rc = 0`, no exception — the "every append
/// silently discarded" behaviour `H22` recorded against
/// `register_string_builder_natives`, produced here, one class away from
/// anything that registrar names.
///
/// `WORKER-3-NOTE-6` (2026-08-22) measured a SEPARATE dispatch defect where a
/// `(AbstractStringBuilder,Void)V` call landed on this `(StringBuilder)V` body
/// instead of `native_string_init_abstract_string_builder`. RE-MEASURED
/// 2026-09-20 (`--dump-native-registry`, `--nojit
/// CRATONVM_DISABLE_INTRINSICS=1`, builder registrations skipped): the
/// `(AbstractStringBuilder,Void)V` triple now shows `invocations: 2` and this
/// `(StringBuilder)V` triple shows `invocations: 0` for the same
/// `sb.toString()` call — the dispatch now lands on the RIGHT body,
/// whatever fixed it did so incidentally. This function is kept correct for
/// either overload regardless, since nothing here depends on which one runs.
fn native_string_init_from_string_builder(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A null builder -> NPE, matching the JDK ctor — with the JDK ctor's own
    // helpful-NPE text (JIT round 16, lane strings, STR16-4): `obj_arg`'s
    // generic "null object argument" is not what HotSpot prints.
    if matches!(args.get(1), Some(Value::Object(None)))
        && crate::lang_string::string_builder_ctor_null_npe_enabled()
    {
        return Err(crate::lang_string::string_ctor_null_builder_npe());
    }
    let sb = obj_arg(args, 1)?;
    let units: Vec<u16> = crate::lang_string::sb_read_chars(ctx, sb);
    // Write the UTF-16 units STRAIGHT into `this`.
    //
    // This used to be `String::from_utf16_lossy(&chars).into_bytes()` fed to
    // `string_from_bytes_utf8`, and the round trip through a Rust `String` is
    // the bug: a `str` cannot hold an unpaired surrogate at all, so every lone
    // surrogate in the builder became U+FFFD. MEASURED on HotSpot 25.0.3+9 —
    // `sb.append((char) 0xDC00); new String(sb).charAt(0)` is 56320, where this
    // answered 65533. The builder itself was never lossy: `sb.charAt(0)` and
    // `sb.toString().charAt(0)` both already answered 56320, so this
    // constructor disagreed with the very object it was copying.
    //
    // `init_string_from_units` is the units-preserving primitive and the only
    // one that applies here. Note it is NOT interchangeable with
    // `lang_string::sb_string_from_units`, which ALLOCATES a fresh String and
    // returns it — correct for a String-returning native, wrong for a
    // constructor, which must populate the receiver the caller already has a
    // reference to. Both end in the same `populate_java_string_fields`.
    //
    // gc-common w20-b: on the collecting retry ladder (the primitive does
    // not collect, so a full young space raised a spurious OOME here).
    crate::lang_string::init_string_from_units_or_oom(
        ctx,
        this,
        &units,
        "String from StringBuilder",
    )?;
    Ok(None)
}

/// `<init>([BII)V` — `new String(byte[], int offset, int length)`.
fn native_string_init_bytes_off_len(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let count = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr);
    // `checked_add`: a negative int argument arrives as a huge `usize`, and the
    // plain sum overflowed (a panic in debug builds) instead of throwing.
    if offset.checked_add(count).map_or(true, |end| end > arr_len) {
        // `String.checkBoundsOffCount(offset, count, length)`: the SIOOBE
        // "Range [o, o + c) out of bounds for length n" (wave 100 L7; this was
        // an AIOOBE "Array index out of range: n"). The `as` casts restore the
        // `int` arguments the `usize` reads above widened.
        return Err(RuntimeError::sioobe_range_size(
            offset as i32,
            count as i32,
            i32::try_from(arr_len).unwrap_or(i32::MAX),
        )
        .into());
    }
    let bytes = read_byte_range(ctx, arr, offset, count);
    string_from_bytes_utf8(ctx, this, &bytes)
}

/// `<init>([BLjava/lang/String;)V` — `new String(byte[], String charsetName)`.
/// The charset name is currently ignored (UTF-8 lossy is used) because the
/// probe-relevant charsets (UTF-8, US-ASCII, ISO-8859-1) all agree on the
/// ASCII subset that the round-trip exercises.  Out-of-scope charsets
/// (Shift_JIS, EUC-KR, etc.) would silently produce mojibake here — fine
/// for the probe path, not for full correctness.  An
/// `UnsupportedEncodingException` thrower keyed on a real charset table is
/// a follow-up for production use.
fn native_string_init_bytes_charset_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let charset_obj = obj_arg(args, 2)?;
    let charset_name = ctx.read_string(charset_obj).unwrap_or_default();
    let len = ctx.array_length(arr);
    let bytes = read_byte_range(ctx, arr, 0, len);
    string_from_bytes_with_charset(ctx, this, &bytes, &charset_name)
}

/// `<init>([BIILjava/lang/String;)V` — `new String(byte[], int offset,
/// int length, String charsetName)`.
fn native_string_init_bytes_off_len_charset_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let count = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let charset_obj = obj_arg(args, 4)?;
    let charset_name = ctx.read_string(charset_obj).unwrap_or_default();
    let arr_len = ctx.array_length(arr);
    // `checked_add`: a negative int argument arrives as a huge `usize`, and the
    // plain sum overflowed (a panic in debug builds) instead of throwing.
    if offset.checked_add(count).map_or(true, |end| end > arr_len) {
        // `String.checkBoundsOffCount(offset, count, length)`: the SIOOBE
        // "Range [o, o + c) out of bounds for length n" (wave 100 L7; this was
        // an AIOOBE "Array index out of range: n"). The `as` casts restore the
        // `int` arguments the `usize` reads above widened.
        return Err(RuntimeError::sioobe_range_size(
            offset as i32,
            count as i32,
            i32::try_from(arr_len).unwrap_or(i32::MAX),
        )
        .into());
    }
    let bytes = read_byte_range(ctx, arr, offset, count);
    string_from_bytes_with_charset(ctx, this, &bytes, &charset_name)
}

/// `<init>([BLjava/nio/charset/Charset;)V` — `new String(byte[], Charset)`.
fn native_string_init_bytes_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let cs_obj = obj_arg(args, 2)?;
    let charset_name = match ctx.get_field(cs_obj, crate::CHARSET_FIELD_NAME) {
        Value::Object(Some(name_obj)) => ctx.read_string(name_obj).unwrap_or_default(),
        _ => String::new(),
    };
    let len = ctx.array_length(arr);
    let bytes = read_byte_range(ctx, arr, 0, len);
    string_from_bytes_with_charset(ctx, this, &bytes, &charset_name)
}

/// `<init>([BIILjava/nio/charset/Charset;)V` — `new String(byte[], int
/// offset, int length, Charset)`.
fn native_string_init_bytes_off_len_charset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arr = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let count = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let cs_obj = obj_arg(args, 4)?;
    let charset_name = match ctx.get_field(cs_obj, crate::CHARSET_FIELD_NAME) {
        Value::Object(Some(name_obj)) => ctx.read_string(name_obj).unwrap_or_default(),
        _ => String::new(),
    };
    let arr_len = ctx.array_length(arr);
    // `checked_add`: a negative int argument arrives as a huge `usize`, and the
    // plain sum overflowed (a panic in debug builds) instead of throwing.
    if offset.checked_add(count).map_or(true, |end| end > arr_len) {
        // `String.checkBoundsOffCount(offset, count, length)`: the SIOOBE
        // "Range [o, o + c) out of bounds for length n" (wave 100 L7; this was
        // an AIOOBE "Array index out of range: n"). The `as` casts restore the
        // `int` arguments the `usize` reads above widened.
        return Err(RuntimeError::sioobe_range_size(
            offset as i32,
            count as i32,
            i32::try_from(arr_len).unwrap_or(i32::MAX),
        )
        .into());
    }
    let bytes = read_byte_range(ctx, arr, offset, count);
    string_from_bytes_with_charset(ctx, this, &bytes, &charset_name)
}

// ---------------------------------------------------------------------------
// T8.2.4 — String.getBytes(int srcBegin, int srcEnd, byte[] dst, int dstBegin)
// ---------------------------------------------------------------------------

/// `getBytes(II[BI)V` — deprecated; copies the low byte of each char into dst.
fn native_string_get_bytes_deprecated(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let dst = obj_arg(args, 3)?;
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };

    let text = ctx.read_string(this).unwrap_or_default();
    let chars: Vec<u16> = text.encode_utf16().collect();

    // JDK 25 `getBytes(int, int, byte[], int)`: `checkBoundsBeginEnd(srcBegin,
    // srcEnd, length())`, then `checkBoundsOffCount(dstBegin, srcEnd -
    // srcBegin, dst.length)`, both SIOOBEs with `Preconditions`' range text
    // (wave 100 L7; this raised an AIOOBE "Array index out of range: n" for the
    // source only and wrote past a short destination).
    let length = i32::try_from(chars.len()).unwrap_or(i32::MAX);
    let (begin_i, end_i) = (src_begin as i32, src_end as i32);
    if begin_i < 0 || begin_i > end_i || end_i > length {
        return Err(RuntimeError::sioobe_range(begin_i, end_i, length).into());
    }
    let dst_len = i32::try_from(ctx.array_length(dst)).unwrap_or(i32::MAX);
    let (dst_begin_i, count_i) = (dst_begin as i32, end_i - begin_i);
    if (dst_len | dst_begin_i | count_i) < 0 || count_i > dst_len - dst_begin_i {
        return Err(RuntimeError::sioobe_range_size(dst_begin_i, count_i, dst_len).into());
    }

    for i in src_begin..src_end {
        let low_byte = (chars[i] & 0xFF) as i32;
        ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(low_byte));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.2.5 — Character deprecated static methods
// ---------------------------------------------------------------------------

/// `isJavaLetter(C)Z` — deprecated alias for `Character.isJavaIdentifierStart`.
fn native_char_is_java_letter(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let ch = char::from_u32(c).unwrap_or('\0');
    // isJavaIdentifierStart: letter, _, $, or letter-number (Lu, Ll, Lt, Lm, Lo, Nl)
    let ok = ch.is_alphabetic() || ch == '_' || ch == '$';
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// `isJavaLetterOrDigit(C)Z` — deprecated alias for `Character.isJavaIdentifierPart`.
fn native_char_is_java_letter_or_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let ch = char::from_u32(c).unwrap_or('\0');
    let ok = ch.is_alphanumeric() || ch == '_' || ch == '$';
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// `isSpace(C)Z` — deprecated; true for ASCII whitespace: SP HT LF FF CR.
fn native_char_is_space(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = match args.first() {
        Some(Value::Int(v)) => *v as u16,
        _ => 0,
    };
    let ok = matches!(c, 0x20 | 0x09 | 0x0A | 0x0C | 0x0D);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// T8.2.8 — Properties.save(OutputStream, String)
// ---------------------------------------------------------------------------

/// `save(Ljava/io/OutputStream;Ljava/lang/String;)V` — delegates to `store()`.
fn native_properties_save(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut store_args: Vec<Value> = Vec::with_capacity(2);
    if args.len() > 1 {
        store_args.push(args[1]);
    }
    if args.len() > 2 {
        store_args.push(args[2]);
    }
    ctx.invoke_virtual(
        this,
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        &store_args,
    )
}

// ---------------------------------------------------------------------------
// T8.2.9 — Hashtable.elements() / keys()
// ---------------------------------------------------------------------------
//
// Hashtable in our impl is backed by the native HashMap layout from
// native-collections (field 0 = Object[] buckets, field 1 = size, field 2 =
// capacity; each bucket node has fields key=0, value=1, hash=2, next=3).
//
// We snapshot keys/values into a fresh Object[] and return a `java/util/
// Enumeration` synthetic (field 0 = array, field 1 = cursor) — that synthetic
// has its `hasMoreElements`/`nextElement` methods registered by
// `phases_late::register_phase56_enumeration`, so callers (BouncyCastle,
// Provider hierarchy, etc.) get a working enumeration without a separate
// `HashtableEnumerator` synthetic class needing its own native methods.

/// Walk the HashMap-backed Hashtable buckets and emit (key, value) pairs.
/// Mirrors `native-collections::map_collect_*` since those are private.
///
/// Layout from native-collections: slot 0 = buckets (Object[]).
///
/// Historical bug: this used to read slot 2 as "capacity" to bound the walk,
/// but slot 2 is `threshold:int` on a real-JDK Hashtable. When
/// `native_map_init` writes buckets to the real-JDK `table` slot (via
/// `resolve_field_index("java/util/HashMap", "table")`) it sometimes lands
/// on Hashtable's slot 2 (because HashMap's table-field slot index doesn't
/// match Hashtable's layout — Hashtable's `table` is at slot 0, not at
/// HashMap's table slot). Slot 2 then holds a pointer that this function
/// would read as `Value::Object`, mismatch the `Value::Int(c)` pattern, and
/// return an empty Vec. That collapsed `Hashtable.keys()` / `elements()` to
/// empty Enumerations, which cascaded into BC's
/// `AbstractX500NameStyle.copyHashTable(BCStyle.DefaultLookUp)` producing an
/// empty per-instance `defaultLookUp` and surfaced as every
/// `BCStyle.attrNameToOID("cn"/"o"/"CN"/...)` throwing "Unknown object id".
///
/// Fix: derive the cap from `buckets.length` directly. That's authoritative
/// (the bucket array IS the capacity) and immune to any slot-2 corruption.
fn collect_hashtable(ctx: &dyn NativeContext, this: ObjectRef, want_keys: bool) -> Vec<Value> {
    let buckets = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return Vec::new(),
    };
    let cap = ctx.array_length(buckets);
    let mut out = Vec::new();
    for i in 0..cap {
        let mut node_val = ctx.get_array_element(buckets, i);
        while let Value::Object(Some(node)) = node_val {
            // Two possible bucket-node layouts — `next` is slot 3 in BOTH:
            //   * real java.util.Hashtable$Entry: hash(0,int) key(1) value(2) next(3)
            //   * native HashMap node:            key(0)      value(1) hash(2,int) next(3)
            // In real-JDK mode `Hashtable.<init>`/`put` run real bytecode, so the
            // receiver is a genuine Hashtable with real Entry nodes; reading slot 0
            // as the key then yields the primitive `hash` (Value::Int) and the
            // caller's `checkcast String` blows up with "not an object reference"
            // (and BC's copyHashTable produces a corrupt per-instance defaultLookUp
            // → every attrNameToOID("CN") throws "Unknown object id"). Discriminate
            // on slot 0: a real Entry's `hash` is a primitive int; a native node's
            // key is always an object reference.
            let real_layout = matches!(ctx.get_field(node, 0), Value::Int(_));
            let v = if real_layout {
                if want_keys {
                    ctx.get_field(node, 1)
                } else {
                    ctx.get_field(node, 2)
                }
            } else if want_keys {
                ctx.get_field(node, 0)
            } else {
                ctx.get_field(node, 1)
            };
            out.push(v);
            node_val = ctx.get_field(node, 3);
        }
    }
    out
}

/// Walk the native-backed Hashtable buckets and emit `(key, value)` pairs in
/// bucket/chain order. Same discrimination as [`collect_hashtable`] (real
/// `Hashtable$Entry` layout vs native HashMap-node layout) but returns both
/// halves of each entry together, so a single pass yields self-consistent
/// pairs (two separate `collect_hashtable` calls would each re-walk and could
/// diverge if the table mutated in between).
///
/// Reads fields only — performs no allocation, so the returned `ObjectRef`s are
/// valid at return but go stale on the first allocating call afterwards; the
/// caller must pin them before allocating.
fn collect_hashtable_pairs(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let buckets = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return Vec::new(),
    };
    let cap = ctx.array_length(buckets);
    let mut out = Vec::new();
    for i in 0..cap {
        let mut node_val = ctx.get_array_element(buckets, i);
        while let Value::Object(Some(node)) = node_val {
            // `next` is slot 3 in both layouts; discriminate on slot 0 (a real
            // Entry's `hash` is a primitive int, a native node's key is always
            // an object reference) — see `collect_hashtable`.
            let real_layout = matches!(ctx.get_field(node, 0), Value::Int(_));
            let (k, v) = if real_layout {
                (ctx.get_field(node, 1), ctx.get_field(node, 2))
            } else {
                (ctx.get_field(node, 0), ctx.get_field(node, 1))
            };
            out.push((k, v));
            node_val = ctx.get_field(node, 3);
        }
    }
    out
}

/// `java/util/Hashtable.clone()Ljava/lang/Object;`
///
/// CratonVM models `Hashtable`/`Properties` natively: `put` stores synthetic
/// bucket-node objects (`cratonvm/synthetic/AnonymousObject$N`) in the slot-0
/// `table[]`, not genuine `java/util/Hashtable$Entry` instances. The real-JDK
/// `Hashtable.clone()` body does `t.table[i] = (Hashtable$Entry) table[i].clone()`
/// — that `checkcast` throws `ClassCastException` against our synthetic node.
/// (`InitialContext.<init>` clones its environment Hashtable, so the very first
/// `new InitialDirContext(env)` blew up before any LDAP socket; bug TC0622.)
///
/// Shadow the broken real-JDK body: build a fresh natively-backed map of the
/// receiver's concrete class and re-`put` each `(key, value)` pair through the
/// native `put`. This matches `java.util.Hashtable.clone()` semantics — a fresh
/// entry chain (deep) over the *same* key/value references (shallow) — without
/// ever materialising a `Hashtable$Entry`. Purely additive: the real-JDK path is
/// 100% broken for any natively-populated Hashtable, so there is nothing to
/// regress. (`Properties` is side-table backed and handled separately by the
/// generic `Object.clone` native.)
fn native_hashtable_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Concrete class (a user `Hashtable` subclass should clone as itself). Fall
    // back to plain Hashtable if the lookup can't resolve a name. `Properties`
    // never reaches here — it is side-table backed and handled by the generic
    // `Object.clone` native (see the registration note).
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/util/Hashtable".to_string());

    // Snapshot the entries first (field reads only, no allocation). The refs
    // they hold go stale on the first allocating call below, so pin every
    // object-typed key/value before allocating anything. `base` is the first
    // pin handle; `unpin_native_roots(base)` later releases the whole batch
    // (pin handles are contiguous and increasing).
    let pairs = collect_hashtable_pairs(ctx, this);
    let mut pinned: Vec<(Value, Option<usize>, Value, Option<usize>)> =
        Vec::with_capacity(pairs.len());
    let mut base: Option<usize> = None;

    for (k, v) in pairs {
        let kh = if let Value::Object(Some(o)) = k {
            let h = ctx.pin_native_root(o);
            base.get_or_insert(h);
            Some(h)
        } else {
            None
        };
        let vh = if let Value::Object(Some(o)) = v {
            let h = ctx.pin_native_root(o);
            base.get_or_insert(h);
            Some(h)
        } else {
            None
        };
        pinned.push((k, kh, v, vh));    }

    // Allocate the fresh map and run its native `<init>` (installs the empty
    // bucket store; `Properties` also clears its defaults slot). Pin it too so
    // the per-entry `put` calls (which allocate nodes) don't leave it stale.
    let clone = match ctx.new_object_initialized(&cls, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => {
            if let Some(b) = base {
                ctx.unpin_native_roots(b);
            }
            return match other {
                Ok(v) => Ok(v),
                Err(e) => Err(e),
            };
        }
    };
    let clone_pin = ctx.pin_native_root(clone);
    base.get_or_insert(clone_pin);

    // Re-put each pair through the native `put` (registered on Hashtable; works
    // regardless of the concrete subclass since it operates on the bucket store
    // generically). Read each ref back through its pin in case the heap moved.
    for (k, kh, v, vh) in &pinned {
        let key = match kh {
            Some(h) => match k {
                Value::Object(Some(o)) => Value::Object(Some(ctx.read_native_pin(*h, *o))),
                _ => *k,
            },
            None => *k,
        };
        let val = match vh {
            Some(h) => match v {
                Value::Object(Some(o)) => Value::Object(Some(ctx.read_native_pin(*h, *o))),
                _ => *v,
            },
            None => *v,
        };
        let cl = ctx.read_native_pin(clone_pin, clone);
        if let Err(e) = ctx.invoke(
            "java/util/Hashtable",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(cl)), key, val],
        ) {
            if let Some(b) = base {
                ctx.unpin_native_roots(b);
            }
            return Err(e);
        }
    }

    let clone = ctx.read_native_pin(clone_pin, clone);

    if let Some(b) = base {
        ctx.unpin_native_roots(b);
    }
    Ok(Some(Value::Object(Some(clone))))
}

/// `java.util.Hashtable$Enumerator` — java.base's OWN enumerator over a
/// `Hashtable`, and the class `Hashtable.getEnumeration(int)` instantiates.
const HASHTABLE_ENUMERATOR_CLASS: &str = "java/util/Hashtable$Enumerator";

/// `Enumerator(Hashtable this$0, int type, boolean iterator)` — the inner
/// class's only constructor, verified with `javap -p -s` against JDK 25.
const HASHTABLE_ENUMERATOR_CTOR: &str = "(Ljava/util/Hashtable;IZ)V";

/// `java.util.Hashtable.KEYS` — the `getEnumeration(int)` selector for keys.
/// Measured, not assumed: driving a real `Enumerator` with 0 yields the keys
/// and with 1 the values, on HotSpot 25 and on CratonVM alike.
const HASHTABLE_TYPE_KEYS: i32 = 0;

/// `java.util.Hashtable.VALUES` — the selector for `elements()`.
const HASHTABLE_TYPE_VALUES: i32 = 1;

/// True iff the receiver's slot-0 bucket array holds at least one node.
///
/// This is the gate on the real-enumerator landing below, and it is phrased as
/// a question about the STORE rather than about the receiver's class on
/// purpose. It answers "no" for exactly the two receivers that must not take
/// that landing, and for no others:
///
///   * an EMPTY `Hashtable`, where java.base itself does not build an
///     `Enumerator` (`getEnumeration` short-circuits to
///     `Collections.emptyEnumeration()` when `count == 0`, and that carrier is
///     deliberately not an `Iterator`); and
///   * `Properties`, whose entries live in an identity-keyed side table
///     (`properties_sidetable`) and NOT in the slot-0 buckets — a real
///     `Enumerator` over a `Properties` enumerates nothing at all. Measured
///     2026-08-12: `[]` against a `keySet()` of `[pk]` on CratonVM, and a
///     `NullPointerException` in the `Enumerator` constructor on HotSpot,
///     whose `Properties.table` is null outright. (In real-JDK mode a
///     `Properties` receiver never reaches here anyway — JDK 9+ `Properties`
///     declares its own `keys()`/`elements()` over its `map` field, so
///     resolution stops there. The gate is what keeps that true when
///     `Properties` does NOT declare them, which is the synthetic-JDK shape.)
///
/// Reads fields only — allocates nothing, retains nothing, and returns on the
/// first node rather than walking the whole table.
fn hashtable_has_entry(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let buckets = match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return false,
    };
    let cap = ctx.array_length(buckets);
    for i in 0..cap {
        if let Value::Object(Some(_)) = ctx.get_array_element(buckets, i) {
            return true;
        }
    }
    false
}

/// A real `java.util.Hashtable$Enumerator` over `this`, or `None` when this
/// image cannot build one.
///
/// WHY THIS EXISTS. `Hashtable.keys()`/`elements()` are shadowed by natives
/// because CratonVM populates the table with its own bucket nodes
/// (`cratonvm/synthetic/AnonymousObject$N`) rather than
/// `java.util.Hashtable$Entry`. The shadow then had to produce an
/// `Enumeration` from a snapshot, and every carrier it could reach was the
/// wrong TYPE: the fabricated `java/util/Enumeration$Impl` (refused under
/// `--real-jdk`, since no JDK declares it), then `Collections.enumeration(...)`
/// — which is a real class, but an `Enumeration` and nothing else.
///
/// The real `Hashtable$Enumerator` implements `Enumeration` AND `Iterator`
/// over ONE cursor, and code in the wild takes one element through the
/// `Enumeration` face and the rest through the `Iterator` face. A
/// `Collections$3` breaks that pattern while passing every value assertion.
///
/// WHY IT WORKS ANYWAY. The premise behind the snapshot — "the real backing
/// store is not usable" — was measured and is FALSE for `Hashtable`. The
/// receiver's real `table` field IS populated (6 entries → `usedSlots=6`,
/// `count=6`, `threshold=8`), so java.base's own `Enumerator` reads the real
/// array and the real `modCount`, and its `Entry.key`/`.value`/`.next` field
/// accesses land correctly on our nodes. Measured 2026-08-12 against JDK 25:
/// keys, values, keys≠values, `NoSuchElementException` past the end, the
/// dual-face shared cursor, two independent cursors, `Collections.list(...)`,
/// and a `Hashtable(Map)`-built receiver all agree line-for-line with HotSpot.
///
/// This is the same rule as `classloader::real_snapshot_enumeration`, taken
/// one step further: prefer a real class the JDK builds ITSELF — here not just
/// the carrier but the whole cursor, so the type is right by construction and
/// nothing has to know what the carrier is called.
///
/// `None` (never a fabrication, and never a half-built object) when the class
/// is absent or would be stood in for — the synthetic-JDK shape, where the
/// caller keeps the snapshot carrier it has always had.
///
/// Returns the receiver alongside the answer, read back through the pin. That
/// is not decoration: this function runs Java code (`<clinit>`, the
/// constructor) and the heap can move under it, so the caller's own `this` is
/// stale on return — and `args[0]` is a COPY made before the move, so
/// re-reading the argument slice would hand back the same stale ref rather
/// than a fresh one.
fn real_hashtable_enumerator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    enum_type: i32,
) -> Result<(Option<ObjectRef>, ObjectRef), cratonvm_types::error::MethodCallFailed> {
    // Never accept a stand-in: a fabricated `Hashtable$Enumerator` would be
    // exactly the class-fabrication this landing exists to stop minting, and
    // its methods would be unbound. Ask before a stub can be created, and
    // again after initialization in case one already existed.
    if ctx.would_fabricate_synthetic_stub(HASHTABLE_ENUMERATOR_CLASS)
        || ctx.is_class_synthetic_stub(HASHTABLE_ENUMERATOR_CLASS)
    {
        // Nothing ran, so the caller's `this` is still good.
        return Ok((None, this));
    }
    // GC-SAFETY: `ensure_class_initialized` and `new_object_initialized` both
    // run Java code and can move the heap, and `this` is a bare Rust local the
    // collector cannot see. Root it across both and read it back through the
    // pin — the contract `make_snapshot_enumeration` documents.
    let pin = ctx.pin_native_root(this);
    // Unpin on EVERY exit path, the error one included — hence the split: the
    // inner half may use `?`, this half may not.
    let out = real_hashtable_enumerator_pinned(ctx, this, pin, enum_type);
    ctx.unpin_native_roots(pin);
    out
}

/// [`real_hashtable_enumerator`] with `this` already rooted at `pin`.
fn real_hashtable_enumerator_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pin: usize,
    enum_type: i32,
) -> Result<(Option<ObjectRef>, ObjectRef), cratonvm_types::error::MethodCallFailed> {
    // A class this image does not have is not an error here — it is the
    // synthetic-JDK shape, and the caller has a landing for it. Discard the
    // failure rather than surfacing it at the application's `keys()` call site,
    // which is the mistake the fabricated carrier made in the other direction.
    if ctx
        .ensure_class_initialized(HASHTABLE_ENUMERATOR_CLASS)
        .is_err()
    {
        return Ok((None, ctx.read_native_pin(pin, this)));
    }
    if ctx.is_class_synthetic_stub(HASHTABLE_ENUMERATOR_CLASS)
        || !ctx.method_exists(
            HASHTABLE_ENUMERATOR_CLASS,
            "<init>",
            HASHTABLE_ENUMERATOR_CTOR,
        )
    {
        return Ok((None, ctx.read_native_pin(pin, this)));
    }
    let this = ctx.read_native_pin(pin, this);
    // `iterator = false` is what `getEnumeration(int)` passes: it selects the
    // Enumeration-face contract (no modCount check in `nextElement`). The
    // Iterator face stays available either way — that is the whole point of
    // the class — and `next()` checks `modCount` regardless of this flag.
    let built = ctx.new_object_initialized(
        HASHTABLE_ENUMERATOR_CLASS,
        HASHTABLE_ENUMERATOR_CTOR,
        &[
            Value::Object(Some(this)),
            Value::Int(enum_type),
            Value::Int(0),
        ],
    )?;
    let this = ctx.read_native_pin(pin, this);
    match built {
        Some(Value::Object(Some(enm))) => Ok((Some(enm), this)),
        _ => Ok((None, this)),
    }
}

// FIX: added `type_marker` param. The synthetic Enumeration carries a
// discriminator in field 2 (1 = keys snapshot, 0 = values/elements snapshot)
// so callers can tell a keys()-enumeration from an elements()-enumeration.
// The previous signature only wrote fields 0 (array) and 1 (cursor) and never
// populated field 2, so keys() produced an enumeration whose marker silently
// defaulted to Int(0) — indistinguishable from an elements() enumeration.
fn make_hashtable_enumeration(
    ctx: &mut dyn NativeContext,
    snapshot: Vec<Value>,
    type_marker: i32,
) -> MethodCallResult {
    // Allocate the concrete synthetic `java/util/Enumeration$Impl` helper
    // class (field 0 = Object[] array, field 1 = Int cursor). It is
    // pre-registered in `vm_init` with `Object` as superclass and the
    // `Enumeration`/`Iterator` interfaces, and its `hasMoreElements` /
    // `nextElement` natives are bound by `register_enumeration_impl_natives`.
    //
    // The previous code allocated the bare `java/util/Enumeration`
    // *interface*: an interface has no instantiable concrete class, so
    // `ensure_class_initialized` could not produce a usable ClassId and the
    // object degraded to `java/lang/Object` (ClassId 0). A subsequent
    // `invokeinterface Enumeration.hasMoreElements` then failed to retarget
    // to any class with that method (Hazelcast `NoSuchMethodError
    // hasMoreElements()Z`).
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, snapshot.len());
    for (i, v) in snapshot.into_iter().enumerate() {
        ctx.set_array_element(arr, i, v);
    }
    // FIX: allocate the FULL `Enumeration$Impl` width so the type marker in
    // field 2 has a backing slot; fields 0/1 (array, cursor) keep the layout
    // that `register_enumeration_impl_natives` reads.
    //
    // The width is 5, not the 3 this site used to ask for. `getResources` grew
    // slots 3 and 4 (element-decoding mode / lazy-scan cursor — see
    // `classloader::ENUM_ELEMENTS_AS_IS` and the `nextElement` body), and
    // `classloader::make_snapshot_enumeration` — the other producer of exactly
    // this shape — allocates 5. Asking for 3 here would leave `nextElement`'s
    // `get_field(this, 3)` reading off the end of the object. Slot 3 is left
    // unwritten on purpose: `ENUM_ELEMENTS_AS_IS` is 0, which is what a
    // zero-initialized instance already holds.
    //
    // `--real-jdk` refuses `Enumeration$Impl` — no JDK image declares it — and
    // the bare `?` here turned that correct refusal into a
    // `NoClassDefFoundError` at the application's `Hashtable.keys()` call site.
    // Measured 2026-08-12; HotSpot 25 does the same program cleanly.
    //
    // This registration is the WINNER of a double registration: `lib.rs` calls
    // `deprecated_io_util::register_*` then `deprecated_util::register_*`, and
    // `register()` is last-write-wins. The loser (`deprecated_io_util.rs:1201`)
    // routes through `classloader::make_snapshot_enumeration`, which HAS the
    // fallback and would have recovered — so the run died precisely because the
    // winning copy is the one without it. Two copies of one rule, drifted.
    //
    // Land it the way that funnel already does: a real `Arrays$ArrayList`
    // wrapped by real `Collections.enumeration` bytecode. Compatible mode is
    // byte-identical — the `Ok` arm below is the old body unchanged.
    //
    // SINCE 2026-08-12 THIS IS THE FALLBACK, NOT THE PRIMARY PATH. That
    // landing fixed the crash and the values but not the TYPE: the carrier is
    // `java.util.Collections$3`, an `Enumeration` and nothing else, where the
    // JDK's own `Hashtable$Enumerator` is an `Enumeration` AND an `Iterator`
    // over one cursor. `RJdkEnumerations` asserts that dual role and was RED
    // on exactly this. `native_hashtable_keys`/`elements` now build the real
    // `Hashtable$Enumerator` over the live table first (see
    // [`real_hashtable_enumerator`]), and only fall through to here when the
    // receiver has no entries in its slot-0 buckets or the image has no such
    // class. Both remaining arms are correct where they are reached, so
    // nothing below changed.
    //
    // The `type_marker` is dropped on the real carrier, which is safe because
    // nothing reads field 2: `register_enumeration_impl_natives` reads slots
    // 0, 1, 3 and 4, and never 2. It is a write-only slot — which is also why
    // the narrower boot declaration never lost a write.
    let en = match try_alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 5) {
        Ok(en) => {
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            // FIX: stamp the keys/values discriminator into field 2.
            ctx.set_field(en, 2, Value::Int(type_marker));
            en
        }
        Err(refusal) => match crate::classloader::real_snapshot_enumeration(ctx, arr)? {
            Some(en) => en,
            // Nothing real to stand in — an image with no `Arrays$ArrayList` —
            // so the refusal stands rather than silently re-fabricating.
            None => return Err(refusal),
        },
    };
    Ok(Some(Value::Object(Some(en))))
}

/// The JDK's own `Collections.emptyEnumeration()` carrier, or `None` when this
/// image cannot produce one.
///
/// This is the carrier HotSpot hands back from `Hashtable.keys()`/`elements()`
/// on an EMPTY table: `Hashtable.getEnumeration(int)` short-circuits to
/// `Collections.emptyEnumeration()` when `count == 0` rather than building an
/// `Enumerator`, and that carrier is deliberately NOT an `Iterator`. It is the
/// premise [`hashtable_has_entry`] already states in its doc block and the one
/// this VM had no way to honour: the empty case fell through to
/// [`make_hashtable_enumeration`], whose primary arm fabricates
/// `java/util/Enumeration$Impl`.
///
/// MEASURED 2026-08-21, `new Hashtable<>().keys()`, one probe, three arms:
///
/// | | class | `instanceof Iterator` | `nextElement()` past the end |
/// |---|---|---|---|
/// | HotSpot 25.0.3+9 | `Collections$EmptyEnumeration` | false | `NoSuchElementException` |
/// | CratonVM `--real-jdk` | `Collections$3` | false | `NoSuchElementException` |
/// | CratonVM Compatible | `Enumeration$Impl` | **true** | **returns** |
///
/// Three divergences at one site, not one — and the populated table was already
/// correct in both arms (`Hashtable$Enumerator`), so the defect is
/// EMPTY-CONDITIONED and a probe that only fills the table cannot see it.
///
/// Same rule as [`real_hashtable_enumerator`] and
/// `classloader::real_snapshot_enumeration`: prefer a real class the JDK builds
/// itself over a name no image declares. `None` — never a fabrication — when
/// the method is absent, which is the synthetic-JDK shape and also what
/// `MockNativeContext`/the in-tree test registry answer, so both keep the
/// snapshot carrier they have always had.
///
/// docs/feature-designs/native-dispatch-architecture.md
fn real_empty_enumeration(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    if !ctx.method_exists(
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
    ) {
        return Ok(None);
    }
    // `?`, not a swallow. `classloader::real_snapshot_enumeration` — the sibling
    // this mirrors, reached from the very same two call sites on the strict arm
    // today — propagates, and swallowing an `Err(ExceptionThrown)` here would
    // DISCARD a live `Throwable` rather than deliver it. A shape that is merely
    // unexpected (a void or null return) still falls back to the snapshot
    // carrier, which is the case worth being lenient about.
    match ctx.invoke(
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        &[],
    )? {
        Some(Value::Object(Some(e))) => Ok(Some(e)),
        _ => Ok(None),
    }
}

/// `elements()Ljava/util/Enumeration;` — java.base's own `Enumerator` over the
/// live table when one can be built, else a snapshot of values.
fn native_hashtable_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    if hashtable_has_entry(ctx, this) {
        let (enm, refreshed) = real_hashtable_enumerator(ctx, this, HASHTABLE_TYPE_VALUES)?;
        if let Some(enm) = enm {
            return Ok(Some(Value::Object(Some(enm))));
        }
        this = refreshed;
    }
    let values = collect_hashtable(ctx, this, false);
    // An EMPTY snapshot must yield the JDK's own `Collections$EmptyEnumeration`
    // — see [`real_empty_enumeration`] for the three-arm measurement and for
    // why `hashtable_has_entry` is the WRONG key here (it also answers false
    // for a NON-empty `Properties`, which must keep the snapshot arm below).
    // GC: nothing to lose. This branch runs Java code and can move the heap,
    // but `values` is empty by construction and `this` is not read again.
    if values.is_empty() {
        if let Some(e) = real_empty_enumeration(ctx)? {
            return Ok(Some(Value::Object(Some(e))));
        }
    }
    // FIX: type marker 0 = values/elements snapshot.
    make_hashtable_enumeration(ctx, values, 0)
}

/// `keys()Ljava/util/Enumeration;` — java.base's own `Enumerator` over the live
/// table when one can be built, else a snapshot of keys.
///
/// The snapshot arm is still reached in two cases: an image with no real
/// `java.util.Hashtable$Enumerator` (the synthetic-JDK shape), and a receiver
/// whose slot-0 buckets hold nothing — see [`hashtable_has_entry`] for why
/// those two are the same question.
///
/// Since 2026-08-21 (H19) an EMPTY snapshot is diverted BEFORE that arm to the
/// JDK's own `Collections.emptyEnumeration()` carrier, which is what a real JVM
/// returns there; see [`real_empty_enumeration`]. That is the only case where
/// the snapshot arm was reached with nothing to enumerate, and it is the case
/// `Compatible` mode got wrong in three ways at once.
fn native_hashtable_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    if hashtable_has_entry(ctx, this) {
        let (enm, refreshed) = real_hashtable_enumerator(ctx, this, HASHTABLE_TYPE_KEYS)?;
        if let Some(enm) = enm {
            return Ok(Some(Value::Object(Some(enm))));
        }
        // The attempt ran Java code and may have moved the heap: take the
        // receiver it read back through its pin, not the stale `args[0]` copy.
        this = refreshed;
    }
    let keys = collect_hashtable(ctx, this, true);
    // See the twin in `native_hashtable_elements` and
    // [`real_empty_enumeration`]. Keyed on the SNAPSHOT being empty, not on
    // `hashtable_has_entry`.
    if keys.is_empty() {
        if let Some(e) = real_empty_enumeration(ctx)? {
            return Ok(Some(Value::Object(Some(e))));
        }
    }
    // FIX: type marker 1 = keys snapshot.
    make_hashtable_enumeration(ctx, keys, 1)
}

// ---------------------------------------------------------------------------
// T8.2.10 — StringBufferInputStream
// ---------------------------------------------------------------------------

/// `<init>(Ljava/lang/String;)V`
///   field 0 = String ref, field 1 = position (Int), field 2 = count (Int)
// THIS FILE OWNS THE SLOTS. `deprecated_io_util.rs` registers the SAME
// `StringBufferInputStream` and `LineNumberInputStream` triples and loses every
// one of them -- measured with `--dump-native-registry`: its rows come back
// `owns_slot=false, invocations=0` while these come back `owns_slot=true` with
// real counts. The first pass of this fix went into that file, built green, and
// changed nothing. Both copies are corrected so a future change to which
// registrar wins cannot resurrect the defect, but THIS is the live one.
//
// `StringBufferInputStream` IS A LOW-BYTE STREAM: `count` is the CHARACTER
// count and each read hands back `(byte) s.charAt(pos)`. `text.len()` is Rust's
// UTF-8 byte length, a different number the moment any char is non-ASCII:
//
//   new StringBufferInputStream("ab\u00e9\u4e2d")
//     HotSpot   available 4, bytes [97, 98, 233, 45]
//     this VM   available 7, bytes [97, 98, 195, 169, 228, 184, 173]
//
// The JDK is deliberately lossy here -- that is WHY the class is deprecated.
// Encoding as UTF-8 instead is not a fix: it changes the byte VALUES and the
// LENGTH together, so a caller that sized a buffer from `available()` reads a
// different number of different bytes. MEASURED with
// `L4CensusTail.java`.
fn native_sbis_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let str_obj = obj_arg(args, 1)?;
    let text = ctx.read_string(str_obj).unwrap_or_default();
    let len = text.chars().count() as i32;
    ctx.set_field(this, 0, Value::Object(Some(str_obj)));
    ctx.set_field(this, 1, Value::Int(0)); // position
    ctx.set_field(this, 2, Value::Int(len)); // count
    Ok(None)
}

/// `read()I`
fn native_sbis_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let str_ref = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let pos = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let count = match ctx.get_field(this, 2) {
        Value::Int(v) => v as usize,
        _ => 0,
    };

    if pos >= count {
        return Ok(Some(Value::Int(-1)));
    }

    let text = ctx.read_string(str_ref).unwrap_or_default();
    // The low byte of the char at `pos` — see the note on `native_sbis_init`.
    let b = text
        .chars()
        .nth(pos)
        .map(|c| (c as u32 & 0xFF) as i32)
        .unwrap_or(0);
    ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
    Ok(Some(Value::Int(b)))
}

/// `read([BII)I`
fn native_sbis_read_buf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buf = obj_arg(args, 1)?;
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };

    let str_ref = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let pos = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let count = match ctx.get_field(this, 2) {
        Value::Int(v) => v as usize,
        _ => 0,
    };

    if pos >= count {
        return Ok(Some(Value::Int(-1)));
    }

    let text = ctx.read_string(str_ref).unwrap_or_default();
    // Char-indexed and low-byte, exactly as the single-byte read.
    let chars: Vec<char> = text.chars().collect();
    let avail = count - pos;
    let to_read = len.min(avail);

    for i in 0..to_read {
        let b = chars
            .get(pos + i)
            .map(|c| (*c as u32 & 0xFF) as i32)
            .unwrap_or(0);
        ctx.set_array_element(buf, off + i, Value::Int(b));
    }
    ctx.set_field(this, 1, Value::Int((pos + to_read) as i32));
    Ok(Some(Value::Int(to_read as i32)))
}

/// `available()I`
fn native_sbis_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let count = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int((count - pos).max(0))))}

/// `reset()V`
fn native_sbis_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.2.11 — LineNumberInputStream
// ---------------------------------------------------------------------------

/// `<init>(Ljava/io/InputStream;)V`
///   field 0 = wrapped InputStream ref
///   field 1 = current line number (Int)
///   field 2 = saved line number for mark/reset (Int)
///   field 3 = pushback byte (Int, -1 = none)
fn native_lnis_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = obj_arg(args, 1)?;
    ctx.set_field(this, 0, Value::Object(Some(inner)));
    ctx.set_field(this, 1, Value::Int(0)); // line number
    ctx.set_field(this, 2, Value::Int(0)); // saved line number
    ctx.set_field(this, 3, Value::Int(-1)); // pushback byte
    Ok(None)
}

/// Read one byte from the wrapped stream, honouring the pushback slot and
/// normalising `\r` and `\r\n` to `\n` while incrementing the line counter.
///
/// gc-common w20-b: `this` is `&mut` (the `stale-receiver-audit` shape's
/// fix). The wrapped stream's `read()` is Java; this used to read the line
/// counter off, and store the pushback into, `this` at its entry address
/// after one or two of those calls, and `read([BII)` stored every byte into
/// its buffer at the entry address too.
fn lnis_read_one(
    ctx: &mut dyn NativeContext,
    this: &mut cratonvm_types::ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let this_pin = ctx.pin_native_root(*this);
    let result = lnis_read_one_pinned(ctx, this_pin, *this);
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    result
}

/// [`lnis_read_one`]'s body; `this` is pinned at `this_pin` by the caller,
/// and `inner` is pinned just above it (released with it).
fn lnis_read_one_pinned(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: cratonvm_types::ObjectRef,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let inner = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(-1),
    };
    let inner_pin = ctx.pin_native_root(inner);

    // Drain pushback first.
    let pushback = match ctx.get_field(this, 3) {
        Value::Int(v) => v,
        _ => -1,
    };
    let b = if pushback >= 0 {
        ctx.set_field(this, 3, Value::Int(-1));
        pushback
    } else {
        match ctx.invoke_virtual(inner, "read", "()I", &[])? {
            Some(Value::Int(v)) => v,
            _ => -1,
        }
    };

    if b == -1 {
        return Ok(-1);
    }

    if b == b'\r' as i32 {
        // Peek at the next byte to handle \r\n.
        let inner = ctx.read_native_pin(inner_pin, inner);
        let next = match ctx.invoke_virtual(inner, "read", "()I", &[])? {
            Some(Value::Int(v)) => v,
            _ => -1,
        };
        let this = ctx.read_native_pin(this_pin, this);
        let ln = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(ln + 1));
        if next != b'\n' as i32 && next != -1 {
            ctx.set_field(this, 3, Value::Int(next)); // push back non-LF
        }
        return Ok(b'\n' as i32);
    }

    if b == b'\n' as i32 {
        let this = ctx.read_native_pin(this_pin, this);
        let ln = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(ln + 1));    }
    Ok(b)}

/// `read()I`
fn native_lnis_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(lnis_read_one(ctx, &mut this)?)))
}

/// `read([BII)I`
fn native_lnis_read_buf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let buf = obj_arg(args, 1)?;
    // BOUNDS FIRST, and as SIGNED ints. `*v as usize` turns -1 into
    // 18446744073709551615, so a negative offset sailed past the (absent) check
    // and then silently read nothing instead of raising
    // `IndexOutOfBoundsException`. MEASURED with
    // `L4CensusTail.java` (`read(buf, -1, 1)`).
    let off_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let cap = ctx.array_length(buf) as i64;
    if off_i < 0 || len_i < 0 || (len_i as i64) > cap - (off_i as i64) {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!("off {off_i}, len {len_i}, buffer length {cap}")),
        }
        .into());
    }
    let off = off_i as usize;
    let len = len_i as usize;

    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }

    let mut count = 0i32;
    let buf_pin = ctx.pin_native_root(buf);
    for i in 0..len {
        let b = match lnis_read_one(ctx, &mut this) {
            Ok(b) => b,
            Err(e) => {
                ctx.unpin_native_roots(buf_pin);
                return Err(e);
            }
        };
        if b == -1 {
            break;
        }
        let buf = ctx.read_native_pin(buf_pin, buf);
        ctx.set_array_element(buf, off + i, Value::Int(b));
        count += 1;
    }
    ctx.unpin_native_roots(buf_pin);
    if count == 0 {
        Ok(Some(Value::Int(-1)))
    } else {
        Ok(Some(Value::Int(count)))
    }
}

/// `getLineNumber()I`
fn native_lnis_get_line_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(match ctx.get_field(this, 1) {
        Value::Int(v) => Value::Int(v),
        _ => Value::Int(0),
    }))
}

/// `setLineNumber(I)V`
fn native_lnis_set_line_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, 1, Value::Int(n));
    Ok(None)
}

/// `available()I` — delegates to the wrapped stream.
fn native_lnis_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // HALVED, because this stream collapses CRLF to one '\n'. The JDK is
    //
    //     (pushBack == -1) ? (in.available() + 1) / 2
    //                      : (in.available() + 1) / 2 + 1
    //
    // a conservative floor: in the worst case every remaining pair of bytes is
    // a CRLF that will be delivered as a single byte. Delegating raw
    // over-reports —
    //
    //   "a\nb\r\nc\rd"   HotSpot available 4, this VM 8
    //
    // — and `available()` is the number callers size a buffer from and loop on,
    // so over-reporting hands them a short read they did not plan for. MEASURED
    // with `L4CensusTail.java`.
    //
    // gc-common w20-b: the pushback is read BEFORE the wrapped stream's
    // `available()` (Java); it was read off `this` at its entry address
    // after it (the `deprecated_io_util` twin had the same fix in w19-g).
    let pushback = match ctx.get_field(this, 3) {
        Value::Int(v) if v >= 0 => 1,
        _ => 0,
    };
    let raw = ctx
        .invoke_virtual(inner, "available", "()I", &[])?
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    Ok(Some(Value::Int((raw + 1) / 2 + pushback)))
}

/// `reset()V` — restores saved line number and delegates to wrapped stream.
fn native_lnis_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let saved = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, 1, Value::Int(saved));
    let inner = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.invoke_virtual(inner, "reset", "()V", &[])?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// T8.2.12 — Locale.getISO3Language()  (deprecated alias with 2→3 letter map)
// ---------------------------------------------------------------------------

/// Map a 2-letter ISO 639-1 language code to its ISO 639-2/T (terminological)
/// 3-letter equivalent.  Returns the input unchanged when not found.
fn iso2_to_iso3(lang2: &str) -> &'static str {
    // Covers all 184 two-letter codes from ISO 639-1 → ISO 639-2/T.
    match lang2 {
        "aa" => "aar",
        "ab" => "abk",
        "ae" => "ave",
        "af" => "afr",
        "ak" => "aka",
        "am" => "amh",
        "an" => "arg",
        "ar" => "ara",
        "as" => "asm",
        "av" => "ava",
        "ay" => "aym",
        "az" => "aze",
        "ba" => "bak",
        "be" => "bel",
        "bg" => "bul",
        "bh" => "bih",
        "bi" => "bis",
        "bm" => "bam",
        "bn" => "ben",
        "bo" => "bod",
        "br" => "bre",
        "bs" => "bos",
        "ca" => "cat",
        "ce" => "che",
        "ch" => "cha",
        "co" => "cos",
        "cr" => "cre",
        "cs" => "ces",
        "cu" => "chu",
        "cv" => "chv",
        "cy" => "cym",
        "da" => "dan",
        "de" => "deu",
        "dv" => "div",
        "dz" => "dzo",
        "ee" => "ewe",
        "el" => "ell",
        "en" => "eng",
        "eo" => "epo",
        "es" => "spa",
        "et" => "est",
        "eu" => "eus",
        "fa" => "fas",
        "ff" => "ful",
        "fi" => "fin",
        "fj" => "fij",
        "fo" => "fao",
        "fr" => "fra",
        "fy" => "fry",
        "ga" => "gle",
        "gd" => "gla",
        "gl" => "glg",
        "gn" => "grn",
        "gu" => "guj",
        "gv" => "glv",
        "ha" => "hau",
        "he" => "heb",
        "hi" => "hin",
        "ho" => "hmo",
        "hr" => "hrv",
        "ht" => "hat",
        "hu" => "hun",
        "hy" => "hye",
        "hz" => "her",
        "ia" => "ina",
        "id" => "ind",
        "ie" => "ile",
        "ig" => "ibo",
        "ii" => "iii",
        "ik" => "ipk",
        "io" => "ido",
        "is" => "isl",
        "it" => "ita",
        "iu" => "iku",
        "ja" => "jpn",
        "jv" => "jav",
        "ka" => "kat",
        "kg" => "kon",
        "ki" => "kik",
        "kj" => "kua",
        "kk" => "kaz",
        "kl" => "kal",
        "km" => "khm",
        "kn" => "kan",
        "ko" => "kor",
        "kr" => "kau",
        "ks" => "kas",
        "ku" => "kur",
        "kv" => "kom",
        "kw" => "cor",
        "ky" => "kir",
        "la" => "lat",
        "lb" => "ltz",
        "lg" => "lug",
        "li" => "lim",
        "ln" => "lin",
        "lo" => "lao",
        "lt" => "lit",
        "lu" => "lub",
        "lv" => "lav",
        "mg" => "mlg",
        "mh" => "mah",
        "mi" => "mri",
        "mk" => "mkd",
        "ml" => "mal",
        "mn" => "mon",
        "mr" => "mar",
        "ms" => "msa",
        "mt" => "mlt",
        "my" => "mya",
        "na" => "nau",
        "nb" => "nob",
        "nd" => "nde",
        "ne" => "nep",
        "ng" => "ndo",
        "nl" => "nld",
        "nn" => "nno",
        "no" => "nor",
        "nr" => "nbl",
        "nv" => "nav",
        "ny" => "nya",
        "oc" => "oci",
        "oj" => "oji",
        "om" => "orm",
        "or" => "ori",
        "os" => "oss",
        "pa" => "pan",
        "pi" => "pli",
        "pl" => "pol",
        "ps" => "pus",
        "pt" => "por",
        "qu" => "que",
        "rm" => "roh",
        "rn" => "run",
        "ro" => "ron",
        "ru" => "rus",
        "rw" => "kin",
        "sa" => "san",
        "sc" => "srd",
        "sd" => "snd",
        "se" => "sme",
        "sg" => "sag",
        "si" => "sin",
        "sk" => "slk",
        "sl" => "slv",
        "sm" => "smo",
        "sn" => "sna",
        "so" => "som",
        "sq" => "sqi",
        "sr" => "srp",
        "ss" => "ssw",
        "st" => "sot",
        "su" => "sun",
        "sv" => "swe",
        "sw" => "swa",
        "ta" => "tam",
        "te" => "tel",
        "tg" => "tgk",
        "th" => "tha",
        "ti" => "tir",
        "tk" => "tuk",
        "tl" => "tgl",
        "tn" => "tsn",
        "to" => "ton",
        "tr" => "tur",
        "ts" => "tso",
        "tt" => "tat",
        "tw" => "twi",
        "ty" => "tah",
        "ug" => "uig",
        "uk" => "ukr",
        "ur" => "urd",
        "uz" => "uzb",
        "va" => "vol",
        "ve" => "ven",
        "vi" => "vie",
        "vo" => "vol",
        "wa" => "wln",
        "wo" => "wol",
        "xh" => "xho",
        "yi" => "yid",
        "yo" => "yor",
        "za" => "zha",
        "zh" => "zho",
        "zu" => "zul",
        _ => "",
    }
}

/// `getISO3Language()Ljava/lang/String;`
///
/// Reads the language tag from field 0 of the Locale object (stored as a Java
/// String reference), maps it through the 2→3 letter table, and returns the
/// 3-letter code. With no mapping it throws `MissingResourceException`, as
/// the JDK does (it used to hand back the 2-letter code; see the body).
fn native_locale_get_iso3_language(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `java.util.Locale`'s real-JDK instance slots are `baseLocale` /
    // `localeExtensions` (typed objects), NOT String language fields — so
    // the language is read from the ObjectRef-keyed side table populated by
    // `locale_alloc` / the Locale `<init>` natives, falling back to the
    // synthetic-stub slot 0 only for test contexts that write it directly.
    let (side_lang, _, _) = crate::locale_data_get(this);
    let lang2 = if !side_lang.is_empty() {
        side_lang
    } else {
        match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    };
    // THIRD FALLBACK, and the one that covers `Locale.of(..)`. A real `Locale`
    // keeps its language in `baseLocale.language`, and fills neither the side
    // table (which `locale_alloc` and the `<init>` natives write) nor the
    // synthetic slot 0 -- so `Locale.of("en","US").getISO3Language()` answered
    // `""` against HotSpot's `"eng"` (apps/probes/LocaleDateTzShadowSweep 10). Ask
    // our own `getLanguage()`, which is registered and already right for every
    // shape, rather than adding a third copy of the lookup: the `iso2_to_iso3`
    // table below was complete all along and simply never got a code to map.
    // `this` is pinned across the call: the refusal below reads it again.
    let (lang2, this) = if lang2.is_empty() {
        let this_pin = ctx.pin_native_root(this);
        let lang2 = match ctx.invoke_virtual(this, "getLanguage", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        (lang2, this)
    } else {
        (lang2, this)
    };
    // `java.util.Locale.getISO3Language`, reproduced end to end. The tail used
    // to be `if the table has no row, hand back the 2-letter code`, and that is
    // the one shape the real method REFUSES:
    //
    // ```java
    //   String lang = baseLocale.getLanguage();
    //   if (lang.length() == 3) return lang;          // already ISO 639-2
    //   else if (lang.isEmpty()) return "";           // Locale.ROOT
    //   String language3 = getISO3Code(lang, LocaleISOData.isoLanguageTable);
    //   if (language3 == null) throw new MissingResourceException(...);
    // ```
    //
    // MEASURED, HotSpot 25.0.4+7 (`apps/probes/UulrLocaleDisplaySweep`):
    //
    // ```text
    //   Locale.of("qq","ZZ").getISO3Language()   MissingResourceException
    //                                            "Couldn't find 3-letter
    //                                             language code for qq"
    //   CratonVM                                 "qq"
    // ```
    //
    // The old answer is the quiet direction: a caller that writes the result
    // into an ISO 639-2 field gets a two-letter string that every consumer of
    // that field will mis-parse, with nothing to say where it came from. The
    // 3-letter pass-through above it is the other arm the old tail was missing
    // -- `Locale.of("eng")` is a legal locale and its ISO3 language is itself,
    // which the 2->3 table cannot express.
    if lang2.is_empty() {
        let s = ctx.create_string("");
        return Ok(Some(Value::Object(Some(s))));
    }
    if lang2.len() == 3 {
        let s = ctx.create_string(&lang2);
        return Ok(Some(Value::Object(Some(s))));
    }
    let iso3 = iso2_to_iso3(&lang2);
    if iso3.is_empty() {
        // Round 15 wave 2, lane jdkonly: the constructor runs
        // (`locale_resources::missing_resource_exception`); a bare mint left
        // `Throwable`'s declared initialisers (`cause = this`, the suppressed
        // sentinel) at zero. Round 15 wave 3, lane jdkonly2: with the JDK's
        // `className` / `key`, `("FormatData_" + toString(), "ShortLanguage")`
        // (`r15w2-jdkonly-missing-resource-fields-FIXED-20260930.md`).
        let locale_text = match ctx.invoke_virtual(this, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => lang2.clone(),
        };
        return Err(crate::locale_resources::missing_resource_exception_text(
            ctx,
            &format!("Couldn't find 3-letter language code for {lang2}"),
            &format!("FormatData_{locale_text}"),
            "ShortLanguage",
            false,
        ));
    }
    let s = ctx.create_string(iso3);
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_deprecated_util_natives(r: &mut NativeMethodRegistry) {
    let date = "java/util/Date";

    // T8.2.1 — Date constructors
    r.register(date, "<init>", "(III)V", native_date_init_iii, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "<init>", "(IIIII)V", native_date_init_iiiii, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "<init>", "(IIIIII)V", native_date_init_iiiiii, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        date,
        "<init>",
        "(Ljava/lang/String;)V",
        native_date_init_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // T8.2.2 — Date getters
    r.register(date, "getYear", "()I", native_date_get_year, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getMonth", "()I", native_date_get_month, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getDate", "()I", native_date_get_date, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getDay", "()I", native_date_get_day, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getHours", "()I", native_date_get_hours, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getMinutes", "()I", native_date_get_minutes, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "getSeconds", "()I", native_date_get_seconds, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        date,
        "getTimezoneOffset",
        "()I",
        native_date_get_timezone_offset, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // T8.2.2 — Date setters
    r.register(date, "setYear", "(I)V", native_date_set_year, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "setMonth", "(I)V", native_date_set_month, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "setDate", "(I)V", native_date_set_date, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "setHours", "(I)V", native_date_set_hours, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "setMinutes", "(I)V", native_date_set_minutes, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(date, "setSeconds", "(I)V", native_date_set_seconds, cratonvm_native_api::NativeKind::SyntheticStub);

    // T8.2.3 — String(byte[], hibyte, offset, count)
    r.register(
        "java/lang/String",
        "<init>",
        "([BIII)V",
        native_string_init_hibyte, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Cipher-probe support: byte[]→String constructors. The real-JDK bytecode
    // walks `Charset.defaultCharset()` → `sun/nio/cs/UTF_8.INSTANCE` and
    // `StringCoding.countPositives` which require a working
    // `sun.nio.cs` provider chain we don't fully bootstrap. Without these
    // intercepts the constructors silently complete with zero `value` bytes
    // and surface as empty strings — exactly the symptom that breaks the
    // CipherProbe round-trip equality check on the decrypted plaintext.
    //
    // Each intercept decodes the input bytes as UTF-8 (the only charset the
    // probe needs; "ISO-8859-1" / "US-ASCII" are also routed through this
    // because we accept any name and default to UTF-8 lossy decoding which
    // is byte-equivalent for the ASCII subset all of these charsets agree on)
    // and copies the resulting String's instance fields onto `this`. We
    // borrow `create_string`'s value/coder/hash/hashIsZero so the layout
    // matches the JDK 25 compact-string contract.
    r.register(
        "java/lang/String",
        "<init>",
        "([B)V",
        native_string_init_bytes, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "java/lang/String",
        "<init>",
        "([BII)V",
        native_string_init_bytes_off_len, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // DF05: `new String(StringBuilder)`. The real JDK `String(AbstractStringBuilder,
    // Void)` ctor reads `byte[] val = asb.getValue()` and `Arrays.copyOfRange(val,...)`,
    // assuming the compact-string byte[] layout. CratonVM's synthetic StringBuilder
    // backs its content with a char[], so that copy hits a char[]->byte[] mismatch
    // (ArrayStoreException directly, or "Object cannot be cast to [B" via Xerces'
    // StringToken char-merge in XSD xs:pattern validation = Tomcat DF05). Build the
    // String from the builder's chars instead. (StringBuffer's ctor already works and
    // is intentionally not intercepted.)
    //
    // `register_with_kind(.., Intrinsic)` is LOAD-BEARING. Every other
    // `java/lang/String` `Bridge` is dropped in real-JDK mode by
    // `NativeMethodRegistry::register` (contract §1.4). This one is `Bridge`
    // by the ambient category here, so that drop took it — and DF05 came
    // straight back, in its quieter form: `new String(sb)` for a builder
    // holding "abcd42Σ" returned `"a\0b\0c\0d"`. The real
    // ctor's `Arrays.copyOfRange` over the builder's `byte[]` reads
    // CratonVM's `char[]` one byte at a time, so every second byte is the
    // high half of a Latin-1 char — zero. Silent content corruption on an
    // ordinary call, with no exception anywhere.
    //
    // It IS a §1.4-reviewed intrinsic: not a faster stand-in for the
    // bytecode, but the correct answer where the bytecode's premise (the
    // builder is `byte[]`-backed) does not hold on this VM. It goes away when
    // `StringBuilder` stops being char[]-backed, not before. Covered by
    // `probes/StringDroppedNativesProbe`.
    //
    // JIT round 16 (lane strings, STR16-2): under `--real-jdk` on a real
    // image it HAS stopped — every builder there is the JDK's compact one —
    // so `CRATONVM_STRING_BUILDER_CTOR_BYTECODE=1` leaves this triple (and
    // its `AbstractStringBuilder` sibling in `lib.rs`) to the real bytecode.
    if !crate::lang_string::string_builder_ctor_yields_to_bytecode(
        r.compatibility_mode(),
        r.real_jdk(),
        crate::lang_string::sb_bodies_aligned(r),
        crate::lang_string::string_builder_ctor_bytecode_flag(),
    ) {
        r.register(
            "java/lang/String",
            "<init>",
            "(Ljava/lang/StringBuilder;)V",
            native_string_init_from_string_builder,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
    if !r.real_jdk() {
        r.register(
            "java/lang/String",
            "<init>",
            "([BLjava/lang/String;)V",
            native_string_init_bytes_charset_name,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            "java/lang/String",
            "<init>",
            "([BIILjava/lang/String;)V",
            native_string_init_bytes_off_len_charset_name,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    r.register(
        "java/lang/String",
        "<init>",
        "([BLjava/nio/charset/Charset;)V",
        native_string_init_bytes_charset, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "java/lang/String",
        "<init>",
        "([BIILjava/nio/charset/Charset;)V",
        native_string_init_bytes_off_len_charset, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // T8.2.4 — String.getBytes(srcBegin, srcEnd, dst, dstBegin)
    r.register(
        "java/lang/String",
        "getBytes",
        "(II[BI)V",
        native_string_get_bytes_deprecated, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // T8.2.5 — Character deprecated statics.
    //
    // These three own their registry slots. A second, byte-identical copy used
    // to be registered from `native-builtins/src/deprecated_io_util.rs`; it lost
    // the slot every time (`owns_slot: false, overwrote: bridge` in the registry
    // dump) and was deleted 2026-08-13 (lane E23).
    //
    // MEASURED, HotSpot 25, all 65,536 `char` values
    // (`.../scratchpad/e23/Witness23.java`,
    // `docs/feature-designs/native-dispatch-architecture.md` §6):
    //
    //   isSpace(C)Z                0 mismatches — exact, keep
    //   isJavaLetter(C)Z         949 mismatches — WRONG
    //   isJavaLetterOrDigit(C)Z 1010 mismatches — WRONG
    //
    // The two wrong ones model `isJavaIdentifierStart`/`Part` as
    // `is_alphabetic()/is_alphanumeric() || '_' || '$'`. The JDK's predicate also
    // admits `Sc` (currency: `¢ £ ¤ ¥`, first miss U+00A2) and `Pc`, and excludes
    // the `Other_Alphabetic` combining marks Rust's `Alphabetic` includes (first
    // over-accept U+0345). `java/lang/Character` is `has_code: true` in real-JDK
    // mode, so both natives SHADOW correct image bytecode with a wrong answer.
    //
    // Do NOT "fix" them by widening the Rust predicate: an exact body needs a
    // Unicode general-category table, and Rust's is a NEWER Unicode version than
    // the image's — the exact trap E17-1 §5 documents for `getType`. The closure
    // is to DELETE both registrations so real-JDK mode falls through to bytecode;
    // that is a mode-visible change and is nominated, not taken here.
    let ch = "java/lang/Character";
    r.register(ch, "isJavaLetter", "(C)Z", native_char_is_java_letter, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        ch,
        "isJavaLetterOrDigit",
        "(C)Z",
        native_char_is_java_letter_or_digit, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(ch, "isSpace", "(C)Z", native_char_is_space, cratonvm_native_api::NativeKind::SyntheticStub);

    // T8.2.8 — Properties.save
    r.register(
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_properties_save, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // T8.2.9 — Hashtable enumeration
    let ht = "java/util/Hashtable";
    r.register(
        ht,
        "elements",
        "()Ljava/util/Enumeration;",
        native_hashtable_elements, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ht,
        "keys",
        "()Ljava/util/Enumeration;",
        native_hashtable_keys, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // TC0622 — `Hashtable.clone()`. CratonVM stores Hashtable entries as
    // synthetic bucket nodes in the slot-0 `table[]`; the real-JDK clone body
    // casts them to `Hashtable$Entry` and throws CCE. Shadow it with a native
    // that rebuilds a fresh natively-backed map. Paired with the force-native
    // overrides in `interpreter.rs` / `vm_exec.rs` so it wins over the real-JDK
    // bytecode.
    //
    // NOTE: `Properties` (which extends Hashtable) is deliberately NOT routed
    // here — it stores entries in an identity-keyed side-table
    // (`properties_sidetable`), not the slot-0 buckets, so this slot-0 re-put
    // would produce an empty clone. `Properties.clone()` is handled by the
    // generic `Object.clone` native (`native_object_clone`), which shallow-
    // copies the heap fields (incl. `defaults`) and copies the side-table.
    r.register(ht, "clone", "()Ljava/lang/Object;", native_hashtable_clone, cratonvm_native_api::NativeKind::SyntheticStub);

    // T8.2.10 — StringBufferInputStream
    let sbis = "java/io/StringBufferInputStream";
    r.register(sbis, "<init>", "(Ljava/lang/String;)V", native_sbis_init, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sbis, "read", "()I", native_sbis_read, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sbis, "read", "([BII)I", native_sbis_read_buf, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sbis, "available", "()I", native_sbis_available, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sbis, "reset", "()V", native_sbis_reset, cratonvm_native_api::NativeKind::SyntheticStub);

    // T8.2.11 — LineNumberInputStream
    let lnis = "java/io/LineNumberInputStream";
    r.register(lnis, "<init>", "(Ljava/io/InputStream;)V", native_lnis_init, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "read", "()I", native_lnis_read, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "read", "([BII)I", native_lnis_read_buf, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "getLineNumber", "()I", native_lnis_get_line_number, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "setLineNumber", "(I)V", native_lnis_set_line_number, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "available", "()I", native_lnis_available, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lnis, "reset", "()V", native_lnis_reset, cratonvm_native_api::NativeKind::SyntheticStub);

    // T8.2.12 — Locale.getISO3Language
    r.register(
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        native_locale_get_iso3_language, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// gc-common w20-b: the LIVE `LineNumberInputStream` natives (this file owns
/// the slots; `deprecated_io_util` carries the w17-f fix in its dead twin)
/// used the receiver and the caller's buffer through addresses from before
/// the wrapped stream's `read` / `available` (Java). The move is simulated
/// inside that `invoke_virtual`: the hook remaps the native pins from each
/// old address to a second object that plays the moved copy, and clobbers
/// the vacated one.
#[cfg(test)]
mod w20b_lnis_moved_receiver_tests;
