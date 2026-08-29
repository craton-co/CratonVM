// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deprecated APIs from java.io, java.util, java.text, and related packages.
//!
//! This module provides native implementations for deprecated methods that
//! legacy code may still call. Every method is registered via NativeMethodRegistry.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Date math utilities
// ---------------------------------------------------------------------------

/// Returns true if `year` is a leap year (Gregorian calendar).
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in each month (non-leap). Index 0 = January.
const DAYS_IN_MONTH: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn days_in_month(year: i32, month: i32) -> i32 {
    if month == 1 && is_leap_year(year) {
        29
    } else {
        DAYS_IN_MONTH[month as usize]
    }
}

/// Convert year/month/day/hour/min/sec to milliseconds since Unix epoch.
/// year = actual year (already +1900 from deprecated convention before calling).
/// month = 0-based. day = 1-based.
fn to_epoch_millis(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: i32) -> i64 {
    // Days from epoch (1970-01-01) to start of `year`
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if is_leap_year(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if is_leap_year(y) { 366 } else { 365 };
        }
    }
    // Add days for months
    for m in 0..month {
        days += days_in_month(year, m) as i64;
    }
    // Add remaining days (day is 1-based)
    days += (day - 1) as i64;

    days * 86_400_000 + hour as i64 * 3_600_000 + min as i64 * 60_000 + sec as i64 * 1000
}

/// Break epoch millis into (year, month_0based, day_1based, hour, min, sec, day_of_week_0sun).
fn from_epoch_millis(millis: i64) -> (i32, i32, i32, i32, i32, i32, i32) {
    let mut remaining_ms = millis;
    let sec_total = remaining_ms.div_euclid(1000);
    let sec = (sec_total.rem_euclid(60)) as i32;
    let min_total = sec_total.div_euclid(60);
    let min = (min_total.rem_euclid(60)) as i32;
    let hour_total = min_total.div_euclid(60);
    let hour = (hour_total.rem_euclid(24)) as i32;
    let mut day_count = hour_total.div_euclid(24); // days since epoch (can be negative)

    // Day of week: 1970-01-01 was Thursday (day 4, with Sunday=0)
    let dow = ((day_count.rem_euclid(7) + 4) % 7) as i32;

    // Find year
    let mut year = 1970;
    if day_count >= 0 {
        loop {
            let days_in_year: i64 = if is_leap_year(year) { 366 } else { 365 };
            if day_count < days_in_year {
                break;
            }
            day_count -= days_in_year;
            year += 1;
        }
    } else {
        loop {
            year -= 1;
            let days_in_year: i64 = if is_leap_year(year) { 366 } else { 365 };
            day_count += days_in_year;
            if day_count >= 0 {
                break;
            }
        }
    }

    // Find month
    let mut month = 0i32;
    loop {
        let dim = days_in_month(year, month) as i64;
        if day_count < dim {
            break;
        }
        day_count -= dim;
        month += 1;
        if month >= 12 {
            break;
        }
    }

    let day = day_count as i32 + 1; // 1-based

    (year, month, day, hour, min, sec, dow)
}

/// Read the millis timestamp stored in field 0 of a Date object.
fn get_date_millis(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    match ctx.get_field(this, 0) {
        Value::Long(ms) => ms,
        Value::Int(ms) => ms as i64,
        _ => 0,
    }
}

/// Store millis timestamp in field 0 of a Date object.
fn set_date_millis(ctx: &mut dyn NativeContext, this: ObjectRef, millis: i64) {
    ctx.set_field(this, 0, Value::Long(millis));
}

// ---------------------------------------------------------------------------
// URL encoding/decoding utilities
// ---------------------------------------------------------------------------

/// Percent/`+`-decode into the raw byte sequence, without assuming any
/// particular charset for the final bytes-to-`String` step (the caller picks
/// that — see `url_decode`/`url_decode_named`). Real JDK's `URLDecoder.decode`
/// throws `IllegalArgumentException` on a malformed escape rather than
/// passing it through unchanged.
fn url_decode_bytes(input: &str) -> Result<Vec<u8>, &'static str> {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("URLDecoder: Incomplete trailing escape (%) pattern");
            }
            match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    result.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                _ => return Err("URLDecoder: Illegal hex characters in escape (%) pattern"),
            }
        } else if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    Ok(result)
}

fn url_decode(input: &str) -> Result<String, &'static str> {
    Ok(String::from_utf8_lossy(&url_decode_bytes(input)?).into_owned())
}

/// `URLDecoder.decode(String, String)` / `(String, Charset)` — decode the
/// percent-escaped bytes using the CALLER'S requested charset, not always
/// UTF-8. Previously both overloads ignored their charset argument entirely
/// (see the removed "we always use UTF-8 for simplicity" comments), so e.g.
/// `URLDecoder.decode("%e0%e0%e0", "windows-1251")` produced UTF-8-lossy
/// replacement characters (U+FFFD) instead of the correct Cyrillic text —
/// breaking `ServletServerHttpRequestTests.getFormBodyWithNotUtf8Charset`
/// and any non-UTF-8 form-urlencoded request body.
fn url_decode_named(input: &str, charset_name: &str) -> Result<String, &'static str> {
    Ok(crate::charset::decode_str_named(
        charset_name,
        &url_decode_bytes(input)?,
    ))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn url_encode(input: &str) -> String {
    let mut result = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                result.push(b as char);
            }
            b' ' => {
                result.push('+');
            }
            _ => {
                result.push('%');
                result.push(to_hex_char(b >> 4));
                result.push(to_hex_char(b & 0x0f));
            }
        }
    }
    result
}

fn to_hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => '0',
    }
}

/// `URLEncoder.encode(String, String)` / `(String, Charset)` — same charset
/// gap as `url_decode_named`, mirrored on the encode side: previously always
/// percent-escaped the input's raw UTF-8 bytes regardless of the requested
/// charset, so e.g. `URLEncoder.encode("а", "windows-1251")` produced
/// `%D0%B0` (UTF-8) instead of the correct single-byte `%E0`. Safe
/// (unescaped) characters are ASCII and identical across every charset this
/// VM supports, so only non-safe characters need the charset's own byte
/// encoding.
fn url_encode_named(input: &str, charset_name: &str) -> String {
    let mut result = String::new();
    for ch in input.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '*' => {
                result.push(ch);
            }
            ' ' => {
                result.push('+');
            }
            _ => {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                for b in crate::charset::encode_str_named(charset_name, s) {
                    result.push('%');
                    result.push(to_hex_char(b >> 4));
                    result.push(to_hex_char(b & 0x0f));
                }
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_deprecated_io_util_natives(r: &mut NativeMethodRegistry) {
    register_date_constructors(r);
    register_date_getters_setters(r);
    register_string_deprecated(r);
    // T8.2.5 (`register_character_deprecated`) removed 2026-08-13, lane E23.
    // `Character.isJavaLetter(C)Z`, `isJavaLetterOrDigit(C)Z` and `isSpace(C)Z`
    // were registered from HERE *and* from
    // `native-builtins/src/deprecated_util.rs:2076/2078/2083`.  The registry
    // dump recorded all three of this file's copies as `owns_slot: false,
    // overwrote: bridge` — they lost the slot to `deprecated_util.rs` and never
    // ran.  A duplicate registration that loses today wins tomorrow if either
    // file's registration order shifts, which is exactly how four `Character`
    // bridge copies came to beat the real intrinsics (§5.4 of
    // `docs/known-issues/jdk-only/`
    // `E17-1-character-digit-int-and-the-fifteen-unregistered.md`), so the dead
    // copies are gone rather than left as a latent coin-flip.
    //
    // Deleting them is behaviour-neutral: the two bodies were transliterated
    // and diffed over all 65,536 `char` values and disagree **0** times
    // (`E23-1-synthetic-jdk-nosuchmethoderror-census.md` §6).  The surviving
    // copy is NOT correct — it is wrong on 949 / 1,010 chars vs HotSpot 25 —
    // but that is one defect in one place now instead of two.
    register_class_new_instance(r);
    register_number_defaults(r);
    register_properties_save(r);
    register_hashtable_enumerations(r);
    register_string_buffer_input_stream(r);
    register_line_number_input_stream(r);
    // T8.2.12 (`register_locale_noop`) removed — see the note where it stood.
    register_url_codec(r);
}

// ---------------------------------------------------------------------------
// T8.2.1 — Date(int, int, int, ...) constructors
// ---------------------------------------------------------------------------

fn register_date_constructors(r: &mut NativeMethodRegistry) {
    let date = "java/util/Date";

    // <init>(III)V — year, month, date
    r.register(date, "<init>", "(III)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let month = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let day = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let millis = to_epoch_millis(year + 1900, month, day, 0, 0, 0);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(IIIII)V — year, month, date, hrs, min
    r.register(date, "<init>", "(IIIII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let month = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let day = match args.get(3) {
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
        let millis = to_epoch_millis(year + 1900, month, day, hrs, min, 0);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(IIIIII)V — year, month, date, hrs, min, sec
    r.register(date, "<init>", "(IIIIII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let month = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let day = match args.get(3) {
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
        let millis = to_epoch_millis(year + 1900, month, day, hrs, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(Ljava/lang/String;)V — Date.parse(String) form
    //
    // Parses the formats documented for the deprecated `Date(String)` ctor /
    // `Date.parse`: the `Date.toString` form ("EEE MMM dd HH:mm:ss zzz yyyy"),
    // RFC-1123 / RFC-822 ("EEE, dd MMM yyyy HH:mm:ss zzz"), and the ISO-ish
    // numeric forms ("YYYY-MM-DD" / "YYYY/MM/DD" with optional time).
    //
    // Like the real `Date.parse`, a genuinely unparseable string throws
    // `IllegalArgumentException` — it must NOT silently fall back to epoch 0,
    // which would hand callers a wrong-but-valid date and mask the error.
    r.register(date, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let str_obj = obj_arg(args, 1)?;
        let text = ctx.read_string(str_obj).unwrap_or_default();
        match parse_date_string(&text) {
            Some(millis) => {
                set_date_millis(ctx, this, millis);
                Ok(None)
            }
            None => Err(RuntimeError::IllegalArgumentException { message: text }.into()),
        }
    });
}

/// Map a 3-letter (or longer) English month name to a 0-based month index.
/// Case-insensitive; only the first three letters are significant, matching
/// `java.util.Date.parse`.
fn month_name_to_index(name: &str) -> Option<i32> {
    let key: String = name
        .chars()
        .take(3)
        .flat_map(|c| c.to_lowercase())
        .collect();
    match key.as_str() {
        "jan" => Some(0),
        "feb" => Some(1),
        "mar" => Some(2),
        "apr" => Some(3),
        "may" => Some(4),
        "jun" => Some(5),
        "jul" => Some(6),
        "aug" => Some(7),
        "sep" => Some(8),
        "oct" => Some(9),
        "nov" => Some(10),
        "dec" => Some(11),
        _ => None,
    }
}

/// True if `tok` looks like a day-of-week name (Sun..Sat, prefix match) — these
/// are ignored by `Date.parse` but appear in the toString / RFC-1123 forms.
fn is_weekday_token(tok: &str) -> bool {
    let key: String = tok.chars().take(3).flat_map(|c| c.to_lowercase()).collect();
    matches!(
        key.as_str(),
        "sun" | "mon" | "tue" | "wed" | "thu" | "fri" | "sat"
    )
}

/// True if `tok` is a recognised time-zone token (named zone or numeric
/// offset). Used to keep an otherwise-unknown alphabetic/offset token from
/// failing the parse when it's a zone we accept but don't apply an offset for.
fn is_timezone_token(tok: &str) -> bool {
    timezone_offset_minutes(tok).is_some() || tok.starts_with("GMT") || tok.starts_with("UTC")
}

/// Minutes to ADD to the parsed local time to get UTC, for the time-zone tokens
/// `Date.parse` understands. Returns `None` for tokens that aren't time zones.
/// Numeric offsets like "+0100" / "GMT-0530" are also handled.
fn timezone_offset_minutes(tok: &str) -> Option<i32> {
    // Named zones (subset of what java.util.Date.parse recognises).
    let named = match tok {
        "UT" | "UTC" | "GMT" | "Z" => Some(0),
        "EST" => Some(5 * 60),
        "EDT" => Some(4 * 60),
        "CST" => Some(6 * 60),
        "CDT" => Some(5 * 60),
        "MST" => Some(7 * 60),
        "MDT" => Some(6 * 60),
        "PST" => Some(8 * 60),
        "PDT" => Some(7 * 60),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    // JDK `Date.parse` matches zone words by PREFIX — its scan compares the zone
    // table against `regionMatches(true, 0, s, start, len)` where `len` is the
    // *scanned token's* length — so an abbreviated zone like "GM" is accepted as
    // GMT (this is exactly what `new Date("... 13:30:00 GM")` relies on). Mirror
    // that for multi-character all-alphabetic tokens, walking the zone table in
    // JDK order so ambiguous prefixes resolve identically. Single-letter tokens
    // are skipped (they'd collide with month / AM-PM prefixes and never occur in
    // the forms we parse); numeric offsets ("+0100", "GMT-5") fall through below.
    if tok.len() >= 2 && tok.bytes().all(|b| b.is_ascii_alphabetic()) {
        const ZONES: &[(&str, i32)] = &[
            ("GMT", 0),
            ("UT", 0),
            ("UTC", 0),
            ("EST", 5 * 60),
            ("EDT", 4 * 60),
            ("CST", 6 * 60),
            ("CDT", 5 * 60),
            ("MST", 7 * 60),
            ("MDT", 6 * 60),
            ("PST", 8 * 60),
            ("PDT", 7 * 60),
        ];
        let upper = tok.to_ascii_uppercase();
        for (name, off) in ZONES {
            if name.starts_with(upper.as_str()) {
                return Some(*off);
            }
        }
    }
    // Numeric offset, optionally prefixed by GMT/UTC: "+HH:MM", "+HHMM", "-HHMM".
    let body = tok
        .strip_prefix("GMT")
        .or_else(|| tok.strip_prefix("UTC"))
        .unwrap_or(tok);
    let (sign, digits) = if let Some(rest) = body.strip_prefix('+') {
        (1, rest)
    } else if let Some(rest) = body.strip_prefix('-') {
        (-1, rest)
    } else {
        return None;
    };
    let digits: String = digits.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hh: i32 = digits[0..2].parse().ok()?;
    let mm: i32 = digits[2..4].parse().ok()?;
    // East-of-UTC ("+0100") means local is AHEAD of UTC, so SUBTRACT to reach
    // UTC → the offset-to-add is negative.
    Some(-sign * (hh * 60 + mm))
}

/// Parse a date string for the deprecated `Date(String)` constructor.
///
/// Recognised formats:
///   * `Date.toString`:  "EEE MMM dd HH:mm:ss zzz yyyy"  (e.g. "Wed Jun 17 14:30:45 GMT 2026")
///   * RFC-1123/RFC-822: "EEE, dd MMM yyyy HH:mm:ss zzz" (e.g. "Wed, 17 Jun 2026 14:30:45 GMT")
///   * ISO numeric:      "YYYY-MM-DD" / "YYYY/MM/DD" with an optional "HH:MM:SS" time.
///
/// Returns `Some(epoch_millis)` on success, or `None` when the string cannot be
/// parsed (the caller throws `IllegalArgumentException`).
fn parse_date_string(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Try the purely-numeric ISO forms first (no alphabetic month name).
    if let Some(ms) = parse_iso_numeric(s) {
        return Some(ms);
    }

    // Otherwise treat it as a token-based textual date (toString / RFC-1123).
    parse_textual(s)
}

/// Handle "YYYY-MM-DD" / "YYYY/MM/DD" optionally followed by " HH:MM:SS" or
/// "THH:MM:SS". Returns `None` if the leading date part isn't three integers.
fn parse_iso_numeric(s: &str) -> Option<i64> {
    let (date_part, time_part) = match s.split_once(|c: char| c == ' ' || c == 'T') {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };

    // The date part must be exactly three integer components separated by '/' or '-'.
    let nums: Vec<&str> = date_part
        .split(|c: char| c == '/' || c == '-')
        .filter(|p| !p.is_empty())
        .collect();
    if nums.len() != 3 {
        return None;
    }
    let year: i32 = nums[0].parse().ok()?;
    let month_1: i32 = nums[1].parse().ok()?;
    let day: i32 = nums[2].parse().ok()?;
    if !(1..=12).contains(&month_1) || !(1..=31).contains(&day) {
        return None;
    }

    let (hour, min, sec) = parse_time_part(time_part)?;
    Some(to_epoch_millis(year, month_1 - 1, day, hour, min, sec))
}

/// Parse the textual `Date.toString` / RFC-1123 forms by scanning whitespace-
/// and comma-separated tokens for: a month name, a 4-digit year, a 1-2 digit
/// day-of-month, an "HH:MM[:SS]" time, and an optional time-zone token. Order is
/// tolerant so both layouts ("Wed Jun 17 14:30:45 GMT 2026" and
/// "Wed, 17 Jun 2026 14:30:45 GMT") parse. Returns `None` if month, day, or
/// year cannot be determined.
fn parse_textual(s: &str) -> Option<i64> {
    let mut month: Option<i32> = None;
    let mut year: Option<i32> = None;
    let mut day: Option<i32> = None;
    let mut time = (0i32, 0i32, 0i32);
    let mut tz_offset_min = 0i32;

    for raw in s.split(|c: char| c.is_whitespace() || c == ',') {
        let tok = raw.trim();
        if tok.is_empty() {
            continue;
        }
        if is_weekday_token(tok) {
            continue;
        }
        if let Some(m) = month_name_to_index(tok) {
            month.get_or_insert(m);
            continue;
        }
        if tok.contains(':') {
            // "HH:MM" or "HH:MM:SS"
            match parse_time_part(tok) {
                Some(t) => time = t,
                None => return None,
            }
            continue;
        }
        if let Some(off) = timezone_offset_minutes(tok) {
            tz_offset_min = off;
            continue;
        }
        // A bare integer is either the year (4 digits / >31) or day-of-month.
        if let Ok(n) = tok.parse::<i32>() {
            if tok.len() >= 4 || n > 31 {
                year.get_or_insert(n);
            } else if (1..=31).contains(&n) {
                // Prefer filling day first; a later 4-digit number is the year.
                if day.is_none() {
                    day = Some(n);
                } else {
                    year.get_or_insert(n);
                }
            } else {
                return None;
            }
            continue;
        }
        // Recognised-but-ignored zone tokens (e.g. "GMT+1" variants already
        // handled above) shouldn't fail the parse; any genuinely unknown
        // alphabetic token should.
        if is_timezone_token(tok) {
            continue;
        }
        return None;
    }

    let month = month?;
    let day = day?;
    let year = year?;
    let (hour, min, sec) = time;
    let local = to_epoch_millis(year, month, day, hour, min, sec);
    Some(local + tz_offset_min as i64 * 60_000)
}

/// Parse an "HH", "HH:MM", or "HH:MM:SS" time component. Returns `None` if any
/// present field isn't an integer (so the caller can reject the whole string).
fn parse_time_part(s: &str) -> Option<(i32, i32, i32)> {
    let s = s.trim();
    if s.is_empty() {
        return Some((0, 0, 0));
    }
    let mut h = 0;
    let mut m = 0;
    let mut sec = 0;
    for (i, field) in s.split(':').enumerate() {
        let v: i32 = field.trim().parse().ok()?;
        match i {
            0 => h = v,
            1 => m = v,
            2 => sec = v,
            _ => return None,
        }
    }
    Some((h, m, sec))
}

// ---------------------------------------------------------------------------
// T8.2.2 — Date.getYear/getMonth/getDate/getHours/etc.
// ---------------------------------------------------------------------------

fn register_date_getters_setters(r: &mut NativeMethodRegistry) {
    let date = "java/util/Date";

    // getYear()I — returns year - 1900
    r.register(date, "getYear", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (year, _, _, _, _, _, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(year - 1900)))
    });

    // getMonth()I — returns 0-based month
    r.register(date, "getMonth", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, month, _, _, _, _, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(month)))
    });

    // getDate()I — returns day of month
    r.register(date, "getDate", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, _, day, _, _, _, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(day)))
    });

    // getDay()I — returns day of week (0=Sunday)
    r.register(date, "getDay", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, _, _, _, _, _, dow) = from_epoch_millis(ms);
        Ok(Some(Value::Int(dow)))
    });

    // getHours()I
    r.register(date, "getHours", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, _, _, hours, _, _, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(hours)))
    });

    // getMinutes()I
    r.register(date, "getMinutes", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, _, _, _, min, _, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(min)))
    });

    // getSeconds()I
    r.register(date, "getSeconds", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (_, _, _, _, _, sec, _) = from_epoch_millis(ms);
        Ok(Some(Value::Int(sec)))
    });

    // setYear(I)V
    r.register(date, "setYear", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_year = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 70,
        };
        let ms = get_date_millis(ctx, this);
        let (_, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(new_year + 1900, month, day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setMonth(I)V
    r.register(date, "setMonth", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_month = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let ms = get_date_millis(ctx, this);
        let (year, _, day, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, new_month, day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setDate(I)V
    r.register(date, "setDate", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_day = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        let ms = get_date_millis(ctx, this);
        let (year, month, _, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, new_day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setHours(I)V
    r.register(date, "setHours", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_hours = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, _, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, new_hours, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setMinutes(I)V
    r.register(date, "setMinutes", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_min = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, _, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, hour, new_min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setSeconds(I)V
    r.register(date, "setSeconds", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_sec = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, min, _, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, hour, min, new_sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // `toLocaleString()Ljava/lang/String;` WAS REGISTERED HERE AND IS RETIRED
    // (2026-08-21, WORKER 4). It is a section 1.4 shadow that answered the wrong
    // string, and the JDK's own body answers the right one on this VM.
    //
    // MEASURED, `regression-suite/probes/W4Deprecated.java`, Linux/JDK 25.0.4,
    // `new Date(946684800000L)`:
    //
    //     HotSpot                                        Jan 1, 2000, 12:00:00 AM
    //     CratonVM, this native                          01/01/2000 00:00:00      <- WRONG
    //     CratonVM, DateFormat.getDateTimeInstance(...)  Jan 1, 2000, 12:00:00 AM <- the JDK body
    //
    // `Date.toLocaleString()`'s body IS that third line — `DateFormat
    // .getDateTimeInstance(DEFAULT, DEFAULT).format(this)` — so the probe's
    // third row is not an approximation of the JDK path, it is the JDK path,
    // run on this VM, agreeing with the oracle exactly. Retiring the native is
    // a fix and not a trade.
    //
    // Trap 4 checked, not assumed: `--dump-native-registry` unioned over 105
    // corpus vectors shows ONE registration of this triple, this one,
    // `owns_slot=True`, `invocations=0`. The deletion removes a row and
    // promotes nothing.
    //
    // The unit test that asserted the wrong string went with it — see
    // `test_date_to_locale_string`'s replacement in the test module. It froze
    // `"01/01/1970 00:00:00"` as expected output, which is how a divergence
    // survives a green suite for as long as this one did.

    // toGMTString()Ljava/lang/String;
    r.register(date, "toGMTString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        let month_name = match month {
            0 => "Jan",
            1 => "Feb",
            2 => "Mar",
            3 => "Apr",
            4 => "May",
            5 => "Jun",
            6 => "Jul",
            7 => "Aug",
            8 => "Sep",
            9 => "Oct",
            10 => "Nov",
            11 => "Dec",
            _ => "???",
        };
        let formatted = format!(
            "{} {} {} {:02}:{:02}:{:02} GMT",
            day, month_name, year, hour, min, sec
        );
        let s = ctx.create_string(&formatted);
        Ok(Some(Value::Object(Some(s))))
    });
}

// ---------------------------------------------------------------------------
// T8.2.3 — String(byte[], int hibyte, int offset, int count)
// ---------------------------------------------------------------------------

fn register_string_deprecated(r: &mut NativeMethodRegistry) {
    let string = "java/lang/String";

    // <init>([BIII)V — deprecated hibyte constructor
    // For each byte, create a char as (hibyte << 8) | (byte & 0xFF)
    r.register(string, "<init>", "([BIII)V", |ctx, args| {
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
        if offset + count > arr_len {
            return Err(RuntimeError::aioobe_index_only((offset + count) as i32).into());
        }

        // Build chars: (hibyte << 8) | (byte & 0xFF)
        let mut chars: Vec<u16> = Vec::with_capacity(count);
        for i in 0..count {
            let b = match ctx.get_array_element(byte_arr, offset + i) {
                Value::Int(v) => v as u8,
                _ => 0u8,
            };
            chars.push(((hibyte as u16) << 8) | (b as u16));
        }

        let text = String::from_utf16_lossy(&chars);
        let str_obj = ctx.create_string(&text);
        // Copy the char array from newly created string to `this`
        let char_arr = ctx.get_field(str_obj, 0);
        ctx.set_field(this, 0, char_arr);
        Ok(None)
    });

    // T8.2.4 — getBytes(II[BI)V — deprecated form
    // Copy low bytes of chars from srcBegin to srcEnd into dst byte array at dstBegin
    r.register(string, "getBytes", "(II[BI)V", |ctx, args| {
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

        if src_end > chars.len() {
            return Err(RuntimeError::sioobe_range(
                src_begin as i32,
                src_end as i32,
                chars.len() as i32,
            )
            .into());
        }

        for i in src_begin..src_end {
            let low_byte = (chars[i] & 0xFF) as i32;
            ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(low_byte));
        }
        Ok(None)
    });
}

// ---------------------------------------------------------------------------
// T8.2.5 — Character.isJavaLetter / isJavaLetterOrDigit / isSpace
//
// REMOVED 2026-08-13 (lane E23).  These three were dead duplicate
// registrations of triples owned by `native-builtins/src/deprecated_util.rs`
// (`native_char_is_java_letter` / `_or_digit` / `native_char_is_space`, lines
// 865/877/891, registered at 2076/2078/2083).  See the tombstone in
// `register_deprecated_io_util_natives` for the measurement and the reason the
// losers were deleted rather than left in place.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// T8.2.6 — Class.newInstance()
// ---------------------------------------------------------------------------

/// Build the real `java.lang.InstantiationException` that `Class.newInstance`
/// owes its caller, with `None` meaning the JDK's null-message form.
///
/// A real throwable rather than a `RuntimeError` carrying the type's NAME in
/// its text: only the real class makes `catch (InstantiationException)` match.
/// Falls back to the previous shape only if the exception class itself cannot
/// be constructed, which would mean the image is unusable anyway.
fn instantiation_exception(
    ctx: &mut dyn NativeContext,
    message: Option<&str>,
) -> cratonvm_types::error::MethodCallFailed {
    let built = match message {
        Some(m) => {
            let dotted = m.replace('/', ".");
            let msg = ctx.create_string(&dotted);
            ctx.new_object_initialized(
                "java/lang/InstantiationException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg))],
            )
        }
        None => ctx.new_object_initialized("java/lang/InstantiationException", "()V", &[]),
    };
    if let Ok(Some(Value::Object(Some(exc)))) = built {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::UnsupportedOperationException {
        message: format!("InstantiationException could not be constructed (message={message:?})"),
    }
    .into()
}

fn register_class_new_instance(r: &mut NativeMethodRegistry) {
    // newInstance()Ljava/lang/Object; — invoke the no-arg constructor reflectively
    //
    // WP6.5: This native is registered in BOTH synthetic and real-JDK mode
    // (real-JDK path: `vm/src/vm/vm_init.rs:768` calls
    // `register_deprecated_io_util_natives` unconditionally for the URLDecoder
    // and Date deprecated overloads).  The receiver `this` is a Class
    // mirror, so we MUST resolve the *encoded* class_id of the mirror via
    // `mirror_class_id` (which consults `class_id_from_mirror` and the
    // legacy slot-0 fallback).  The previous implementation called
    // `ctx.class_id_of_object(this_class)`, which returns the heap class_id
    // of the receiver — that's `java/lang/Class` itself for any mirror, so
    // every `Class.newInstance()` call ended up allocating a new
    // `java/lang/Class` instance and the subsequent `checkcast` to the
    // intended target type failed with `ClassCastException`.  This blocked
    // BouncyCastleProvider startup at `loadServiceClass` (which uses
    // deprecated `Class.newInstance()` to instantiate every `$Mappings`
    // helper) and any other code path that still uses the JDK 1.0
    // deprecated entry point.
    r.register(
        "java/lang/Class",
        "newInstance",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this_class = obj_arg(args, 0)?;
            let class_id = match crate::lang_class::mirror_class_id(ctx, this_class) {
                Some(cid) => cid,
                // G69-1 closes `G68-1` section 5's residue. A PRIMITIVE mirror
                // carries no class id -- there is no `int` class to resolve --
                // so `mirror_class_id` answers `None` and this arm used to
                // report the receiver as "not a Class mirror". It IS one:
                // `int.class` is a `Class`, and HotSpot answers
                // `InstantiationException` with the primitive's name (`int`),
                // exactly as it does for an interface or an array. The
                // no-class-id case and the not-a-mirror case had been collapsed
                // into one message, and only the second of them was true.
                None if crate::lang_class::mirror_is_primitive(ctx, this_class) => {
                    let name =
                        crate::lang_class::mirror_class_name(ctx, this_class).unwrap_or_default();
                    return Err(instantiation_exception(ctx, Some(&name)));
                }
                None => {
                    return Err(RuntimeError::UnsupportedOperationException {
                        message: "Class.newInstance: receiver is not a Class mirror".to_string(),
                    }
                    .into())
                }
            };
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if class_name.is_empty() {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: "Class.newInstance: receiver mirror has no class name".to_string(),
                }
                .into());
            }

            // Reject abstract classes / interfaces / array / primitive types
            // explicitly — matches `Class.newInstance` JDK semantics
            // (InstantiationException for these).  Without this, attempting
            // to instantiate an interface or abstract type would silently
            // allocate an under-initialised object and hand it to bytecode
            // that performs a checkcast or invokevirtual on a method the
            // target abstract/interface type does not actually have.
            if let Some(cid_check) = Some(class_id) {
                let flags = ctx.class_access_flags(cid_check);
                let abstract_bit = cratonvm_types::access_flags::ACC_ABSTRACT;
                let iface_bit = cratonvm_types::access_flags::ACC_INTERFACE;
                if flags & (abstract_bit | iface_bit) != 0 {
                    // The comment above says "InstantiationException for
                    // these" and the code threw UnsupportedOperationException
                    // with the words "InstantiationException:" in the MESSAGE
                    // — a stringly-typed exception. A `catch
                    // (InstantiationException)` does not match it, which is the
                    // whole point of the type, and this file's own note about
                    // Spring's `beanDefinitionWithAbstractClass` depends on the
                    // catch matching.
                    //
                    // MEASURED on both VMs; the message rule is not derivable
                    // and is transcribed:
                    //
                    //   abstract class   InstantiationException | null
                    //   interface        InstantiationException | java.lang.Runnable
                    //   no no-arg ctor   InstantiationException | PC$NoNoArg
                    //   int[]            InstantiationException | [I
                    //   private ctor     SUCCEEDS — not an error at all
                    //
                    // So it is `getName()` everywhere EXCEPT an abstract
                    // non-interface class, where the message is null.
                    let is_iface = flags & iface_bit != 0;
                    return Err(instantiation_exception(
                        ctx,
                        if is_iface { Some(&class_name) } else { None },
                    ));
                }
            }

            // Check the receiver mirror's exact class id. Name-only lookup can
            // collapse a Lookup.defineClass helper back to an application-loader
            // class with the same binary name.
            if !ctx.class_declares_method(class_id, "<init>", "()V") {
                return Err(instantiation_exception(ctx, Some(&class_name)));
            }

            // Create a new instance of the exact mirror class and invoke its
            // constructor without re-resolving the name through the global map.
            let result = ctx.new_object_initialized_with_class_id(class_id, "()V", &[])?;
            match result {
                Some(Value::Object(Some(obj))) => Ok(Some(Value::Object(Some(obj)))),
                _ => Err(RuntimeError::UnsupportedOperationException {
                    message: format!(
                        "InstantiationException: failed to instantiate {}",
                        class_name
                    ),
                }
                .into()),
            }
        },
    );
}

// ---------------------------------------------------------------------------
// T8.2.7 — Number.byteValue() / shortValue()
// ---------------------------------------------------------------------------

/// RETIRED 2026-08-21 (WORKER 4). This function registered
/// `java.lang.Number.byteValue()B` and `shortValue()S`, and both were section
/// 1.4 shadows of a real `java.base` body that is one line long:
///
/// ```java
/// public byte  byteValue()  { return (byte)  intValue(); }
/// public short shortValue() { return (short) intValue(); }
/// ```
///
/// The natives were a faithful transcription of exactly that — `invoke_virtual`
/// on `intValue()`, then the narrowing cast — which is what makes them a
/// re-implementation rather than a bridge. Nothing here crosses a VM boundary.
///
/// **The evidence for retiring rather than keeping.** MEASURED,
/// `regression-suite/probes/W4Deprecated.java`, 116 cases, Linux/JDK 25.0.4:
/// `byteValue()`/`shortValue()` over 15 truncating and sign-flipping inputs,
/// through a USER `Number` subclass that declares only the four abstract
/// primitives (the shape a `java.lang.Number` row exists to serve, via the
/// superclass walk), through `Integer`/`Long`/`Double`/`Float`, through
/// `BigInteger`/`BigDecimal` — the two the comment below specifically called
/// out — and through a `Number`-typed reference. **Zero diffs against the
/// oracle in both modes, before and after.**
///
/// `--dump-native-registry` unioned over 105 corpus vectors: ONE registration
/// each, `owns_slot=True`, `invocations=0`. `dupX = 0`, so the deletion removes
/// two rows and promotes nothing.
///
/// The `BigDecimal` regression the old comment records — *"silently returned 0
/// for every value under the old field-read shortcut, which broke JSON-B /
/// Yasson's untyped numeric binding"* — was caused by a native READING FIELD 0,
/// and was fixed by making the native call `intValue()` virtually. The JDK's
/// own body has always called `intValue()` virtually. The second fix for that
/// defect is to stop standing in front of the body that never had it.
///
/// (The class is `java.lang.Number`, which is WORKER 3's subject; the file is
/// WORKER 4's. Recorded because the split is not obvious from either side.)
fn register_number_defaults(_r: &mut NativeMethodRegistry) {}

// ---------------------------------------------------------------------------
// T8.2.8 — Properties.save(...)
// ---------------------------------------------------------------------------

fn register_properties_save(r: &mut NativeMethodRegistry) {
    // save(OutputStream, String)V — delegates to store(OutputStream, String)
    r.register(
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Delegate to store() via invoke_virtual
            let mut store_args = Vec::with_capacity(2);
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
        },
    );
}

// ---------------------------------------------------------------------------
// T8.2.9 — Hashtable.elements() / keys() Enumeration
// ---------------------------------------------------------------------------

fn register_hashtable_enumerations(r: &mut NativeMethodRegistry) {
    let ht = "java/util/Hashtable";

    // Hashtable in our impl shares the native HashMap layout from
    // native-collections: field 0 = Object[] buckets, field 1 = size,
    // field 2 = capacity. Each bucket node has fields key=0, value=1,
    // hash=2, next=3.
    //
    // We snapshot the entries into a flat Object[] and return a
    // `java/util/Enumeration` synthetic — its `hasMoreElements` /
    // `nextElement` methods are registered in `phases_late`, so callers
    // (BouncyCastle Provider hierarchy etc.) get a fully working
    // enumeration without needing per-class natives on a
    // `HashtableEnumerator`.
    fn snapshot(ctx: &dyn NativeContext, this: ObjectRef, want_keys: bool) -> Vec<Value> {
        // Layout from native-collections: slot 0 = buckets (Object[]).
        //
        // Historical bug: this used to read slot 2 as "capacity" to bound the
        // walk, but slot 2 is `threshold:int` on a real-JDK Hashtable, and
        // `native_map_init`'s S111r29 path (which writes buckets to the JDK
        // `table` slot) ends up clobbering Hashtable's slot 2 with a pointer
        // (HashMap's table slot index doesn't match Hashtable's layout —
        // Hashtable's `table` is at slot 0). Slot 2 then holds a `Value::Object`
        // that mismatches the `Value::Int(c)` pattern below, so this function
        // returned an empty Vec. `keys()` / `elements()` collapsed to empty
        // Enumerations, BC's `AbstractX500NameStyle.copyHashTable(
        // BCStyle.DefaultLookUp)` produced an empty per-instance
        // `defaultLookUp`, and every `BCStyle.attrNameToOID("cn"/"o"/"CN"/...)`
        // threw "Unknown object id".
        //
        // Fix: derive the cap from `buckets.length` directly — that's
        // authoritative (the bucket array IS the capacity) and immune to any
        // slot-2 corruption. Matches the parallel fix in
        // `deprecated_util::collect_hashtable`.
        let buckets = match ctx.get_field(this, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return Vec::new(),
        };
        let cap = ctx.array_length(buckets);
        let mut out = Vec::new();
        for i in 0..cap {
            let mut node_val = ctx.get_array_element(buckets, i);
            while let Value::Object(Some(node)) = node_val {
                // Layout-aware read — `next` is slot 3 in both layouts; only
                // key/value slots differ. Real java.util.Hashtable$Entry is
                // hash(0,int) key(1) value(2) next(3); the native HashMap node is
                // key(0) value(1) hash(2,int) next(3). In real-JDK mode the table
                // is a genuine Hashtable, so reading slot 0 as the key returns the
                // primitive `hash` and breaks the caller's checkcast. Discriminate
                // on slot 0 being a primitive int. (Mirrors deprecated_util::collect_hashtable.)
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
    fn build_enum(ctx: &mut dyn NativeContext, items: Vec<Value>) -> MethodCallResult {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, items.len());
        for (i, v) in items.into_iter().enumerate() {
            ctx.set_array_element(arr, i, v);
        }
        // Allocate the concrete synthetic `Enumeration$Impl` (pre-registered
        // with `Object` superclass + `Enumeration`/`Iterator` interfaces and
        // working `hasMoreElements`/`nextElement` natives) — NOT the bare
        // `java/util/Enumeration` interface, which has no instantiable
        // concrete class and degrades the object to `java/lang/Object`.
        let en = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
        Ok(Some(Value::Object(Some(en))))
    }

    // elements()Ljava/util/Enumeration; — snapshot of values.
    r.register(ht, "elements", "()Ljava/util/Enumeration;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let values = snapshot(ctx, this, false);
        build_enum(ctx, values)
    });

    // keys()Ljava/util/Enumeration; — snapshot of keys.
    r.register(ht, "keys", "()Ljava/util/Enumeration;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let keys = snapshot(ctx, this, true);
        build_enum(ctx, keys)
    });
}

// ---------------------------------------------------------------------------
// T8.2.10 — StringBufferInputStream
// ---------------------------------------------------------------------------

fn register_string_buffer_input_stream(r: &mut NativeMethodRegistry) {
    let sbis = "java/io/StringBufferInputStream";

    // <init>(String)V — store the string
    // field 0 = string ref, field 1 = position (Int), field 2 = count/length (Int)
    r.register(sbis, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let str_obj = obj_arg(args, 1)?;
        let text = ctx.read_string(str_obj).unwrap_or_default();
        let len = text.len() as i32;
        ctx.set_field(this, 0, Value::Object(Some(str_obj)));
        ctx.set_field(this, 1, Value::Int(0)); // position
        ctx.set_field(this, 2, Value::Int(len)); // count
        Ok(None)
    });

    // read()I — read one byte
    r.register(sbis, "read", "()I", |ctx, args| {
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
        let byte_val = text.as_bytes().get(pos).copied().unwrap_or(0) as i32;
        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
        Ok(Some(Value::Int(byte_val)))
    });

    // read([BII)I — read bytes from string into buffer
    r.register(sbis, "read", "([BII)I", |ctx, args| {
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
        let bytes = text.as_bytes();
        let avail = count - pos;
        let to_read = len.min(avail);

        for i in 0..to_read {
            let b = bytes.get(pos + i).copied().unwrap_or(0) as i32;
            ctx.set_array_element(buf, off + i, Value::Int(b));
        }

        ctx.set_field(this, 1, Value::Int((pos + to_read) as i32));
        Ok(Some(Value::Int(to_read as i32)))
    });

    // available()I — bytes remaining
    r.register(sbis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        let count = match ctx.get_field(this, 2) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int((count - pos).max(0))))
    });

    // reset()V — reset position to 0
    r.register(sbis, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
}

// ---------------------------------------------------------------------------
// T8.2.11 — LineNumberInputStream
// ---------------------------------------------------------------------------

fn register_line_number_input_stream(r: &mut NativeMethodRegistry) {
    let lnis = "java/io/LineNumberInputStream";

    // <init>(InputStream)V — wrap an input stream
    // field 0 = wrapped stream, field 1 = line number (Int), field 2 = saved line number (Int)
    // field 3 = pushback byte (Int, -1 = none)
    r.register(lnis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let inner = obj_arg(args, 1)?;
        ctx.set_field(this, 0, Value::Object(Some(inner)));
        ctx.set_field(this, 1, Value::Int(0)); // line number
        ctx.set_field(this, 2, Value::Int(0)); // saved line number
        ctx.set_field(this, 3, Value::Int(-1)); // pushback byte
        Ok(None)
    });

    // read()I — read one byte, track line numbers (\r\n -> \n conversion)
    r.register(lnis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(-1))),
        };

        // Check pushback
        let pushback = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => -1,
        };

        // `inner` is read twice below (the wrapped read, then the CR peek);
        // the first can collect and move it.
        let inner_pin = ctx.pin_native_root(inner);
        let byte_val = if pushback >= 0 {
            ctx.set_field(this, 3, Value::Int(-1));
            pushback
        } else {
            // Read from the wrapped stream
            match ctx.invoke_virtual(inner, "read", "()I", &[])? {
                Some(Value::Int(v)) => v,
                _ => -1,
            }
        };

        if byte_val == -1 {
            return Ok(Some(Value::Int(-1)));
        }

        if byte_val == b'\r' as i32 {
            // Peek at next byte
            let inner = ctx.read_native_pin(inner_pin, inner);
            let next = match ctx.invoke_virtual(inner, "read", "()I", &[])? {
                Some(Value::Int(v)) => v,
                _ => -1,
            };
            // Increment line number for \r (or \r\n)
            let line_no = match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(line_no + 1));

            if next != b'\n' as i32 && next != -1 {
                // Push back the byte that isn't \n
                ctx.set_field(this, 3, Value::Int(next));
            }
            // Convert \r to \n
            return Ok(Some(Value::Int(b'\n' as i32)));
        }

        if byte_val == b'\n' as i32 {
            let line_no = match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(line_no + 1));
        }

        Ok(Some(Value::Int(byte_val)))
    });

    // read([BII)I — read bytes with line number tracking
    r.register(lnis, "read", "([BII)I", |ctx, args| {
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

        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }

        // Read one byte at a time through the single-byte read to get line tracking
        // This is what the JDK implementation does
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(-1))),
        };

        // `inner` is read on every iteration below and each read can collect.
        // Pinned ONCE here, not per iteration -- a pin inside the loop would
        // accumulate roots for the whole read.
        let inner_pin = ctx.pin_native_root(inner);
        let mut count = 0i32;
        for i in 0..len {
            let pushback = match ctx.get_field(this, 3) {
                Value::Int(v) => v,
                _ => -1,
            };

            let inner = ctx.read_native_pin(inner_pin, inner);
            let byte_val = if pushback >= 0 {
                ctx.set_field(this, 3, Value::Int(-1));
                pushback
            } else {
                match ctx.invoke_virtual(inner, "read", "()I", &[])? {
                    Some(Value::Int(v)) => v,
                    _ => -1,
                }
            };

            if byte_val == -1 {
                break;
            }

            let actual_byte = if byte_val == b'\r' as i32 {
                let inner = ctx.read_native_pin(inner_pin, inner);
                let next = match ctx.invoke_virtual(inner, "read", "()I", &[])? {
                    Some(Value::Int(v)) => v,
                    _ => -1,
                };
                let line_no = match ctx.get_field(this, 1) {
                    Value::Int(v) => v,
                    _ => 0,
                };
                ctx.set_field(this, 1, Value::Int(line_no + 1));
                if next != b'\n' as i32 && next != -1 {
                    ctx.set_field(this, 3, Value::Int(next));
                }
                b'\n' as i32
            } else {
                if byte_val == b'\n' as i32 {
                    let line_no = match ctx.get_field(this, 1) {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    ctx.set_field(this, 1, Value::Int(line_no + 1));
                }
                byte_val
            };

            ctx.set_array_element(buf, off + i, Value::Int(actual_byte));
            count += 1;
        }

        if count == 0 {
            Ok(Some(Value::Int(-1)))
        } else {
            Ok(Some(Value::Int(count)))
        }
    });

    // getLineNumber()I
    r.register(lnis, "getLineNumber", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let line_no = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(line_no)))
    });

    // setLineNumber(I)V
    r.register(lnis, "setLineNumber", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let num = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(num));
        Ok(None)
    });

    // available()I — delegate to wrapped stream
    r.register(lnis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(0))),
        };
        ctx.invoke_virtual(inner, "available", "()I", &[])
    });

    // mark(I)V — delegate with line number save
    r.register(lnis, "mark", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let readlimit = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        // Save current line number
        let line_no = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(line_no)); // save to field 2
                                                     // Delegate mark to wrapped stream
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(inner, "mark", "(I)V", &[Value::Int(readlimit)])?;
        Ok(None)
    });

    // reset()V — delegate with line number restore
    r.register(lnis, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Restore saved line number
        let saved = match ctx.get_field(this, 2) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(saved));
        // Delegate reset to wrapped stream
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(inner, "reset", "()V", &[])?;
        Ok(None)
    });
}

// ---------------------------------------------------------------------------
// T8.2.12 — Locale no-op marker
// ---------------------------------------------------------------------------

// REMOVED (wave-2 inline-constant stub removal): this registered
// `java/util/Locale.__deprecated_marker__()V`, a method name that exists in no
// class file and that no bytecode can reference — the registry entry was
// unreachable in BOTH run modes and is named by neither `deprecated_verify`'s
// manifest nor its JDK-25 checklist. `Locale.getISO3Language` is, as the old
// comment said, plain Java-level delegation and needs no native at all.

// ---------------------------------------------------------------------------
// T8.2.13 / T8.2.14 — URLDecoder.decode(String) / URLEncoder.encode(String)
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — stated for the whole registrar, not adjudicated
// per row. Every one of these was among the 200 registrations the real boot
// made with NO category scope over them, which `--dump-native-registry`
// could not report until `current_category` became an `Option`: the old
// `category_chosen` flag was set by the first `set_category` in boot and
// never cleared, so everything after it claimed to have been chosen.
// `SyntheticStub` is the kind these carried before and after — verified by
// a census A/B — and it is the right one on the merits: `java.net.URLEncoder` / `URLDecoder` are ordinary
// bytecode in `java.base` and JDK 25 declares no `ACC_NATIVE` method on
// either, so contract §1.5 cannot call these bridges.
pub fn register_url_codec(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // URLDecoder.decode(String)String — deprecated single-arg form, uses UTF-8
    r.register(
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let decoded = match url_decode(&text) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),
                    }
                    .into())
                }
            };
            let result = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // URLDecoder.decode(String, String)String — two-arg form with charset name
    r.register(
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let decoded = match url_decode_named(&text, &charset_name) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),
                    }
                    .into())
                }
            };
            let result = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // URLDecoder.decode(String, Charset)String — two-arg form with Charset object
    r.register(
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(v @ Value::Object(Some(_))) => crate::charset::charset_name_of(ctx, *v),
                _ => String::new(),
            };
            let decoded = match url_decode_named(&text, &charset_name) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),
                    }
                    .into())
                }
            };
            let result = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // URLEncoder.encode(String, String)String — two-arg form with charset name
    r.register(
        "java/net/URLEncoder",
        "encode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let encoded = url_encode_named(&text, &charset_name);
            let result = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // URLEncoder.encode(String, Charset)String — two-arg form with Charset object
    r.register(
        "java/net/URLEncoder",
        "encode",
        "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(v @ Value::Object(Some(_))) => crate::charset::charset_name_of(ctx, *v),
                _ => String::new(),
            };
            let encoded = url_encode_named(&text, &charset_name);
            let result = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // URLEncoder.encode(String)String — deprecated single-arg form
    r.register(
        "java/net/URLEncoder",
        "encode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let encoded = url_encode(&text);
            let result = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(result))))
        },
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Helper: look up a native and call it through the registry.
    fn call_native(
        registry: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        class: &str,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let cb = registry
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
        cb(ctx, args)
    }

    fn setup() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_deprecated_io_util_natives(&mut r);
        r
    }

    // --- T8.2.1 Date constructors ---

    #[test]
    fn test_date_init_ymd() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        // year=70 (1970), month=0 (Jan), day=1 => epoch 0
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(III)V",
            &[
                Value::Object(Some(date_obj)),
                Value::Int(70),
                Value::Int(0),
                Value::Int(1),
            ],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        assert_eq!(ms, 0, "1970-01-01 should be epoch 0");
    }

    #[test]
    fn test_date_init_ymdhm() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        // year=70, month=0, day=1, hrs=1, min=0 => 3_600_000 ms
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(IIIII)V",
            &[
                Value::Object(Some(date_obj)),
                Value::Int(70),
                Value::Int(0),
                Value::Int(1),
                Value::Int(1),
                Value::Int(0),
            ],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        assert_eq!(ms, 3_600_000);
    }

    #[test]
    fn test_date_init_ymdhms() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        // year=70, month=0, day=1, hrs=0, min=0, sec=30 => 30_000 ms
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(IIIIII)V",
            &[
                Value::Object(Some(date_obj)),
                Value::Int(70),
                Value::Int(0),
                Value::Int(1),
                Value::Int(0),
                Value::Int(0),
                Value::Int(30),
            ],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        assert_eq!(ms, 30_000);
    }

    #[test]
    fn test_date_init_string() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let str_obj = ctx.create_string("2000/01/01");
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        // 2000-01-01 00:00:00 UTC
        assert_eq!(ms, 946684800000);
    }

    #[test]
    fn test_date_init_string_tostring_form() {
        // Date.toString form: "EEE MMM dd HH:mm:ss zzz yyyy"
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let str_obj = ctx.create_string("Sat Jan 01 00:00:00 GMT 2000");
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(result.is_ok());
        assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
    }

    #[test]
    fn test_date_init_string_rfc1123_form() {
        // RFC-1123 form: "EEE, dd MMM yyyy HH:mm:ss zzz"
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let str_obj = ctx.create_string("Sat, 01 Jan 2000 00:00:00 GMT");
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(result.is_ok());
        assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
    }

    #[test]
    fn test_date_init_string_numeric_tz_offset() {
        // "+0100" means local is one hour ahead of UTC → 01:00 local == 00:00 UTC.
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let str_obj = ctx.create_string("Sat, 01 Jan 2000 01:00:00 +0100");
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(result.is_ok());
        assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
    }

    #[test]
    fn test_date_init_string_unparseable_throws() {
        // A genuinely unparseable string must throw IllegalArgumentException,
        // NOT silently fall back to epoch 0.
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let str_obj = ctx.create_string("not a date at all");
        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(
            result.is_err(),
            "unparseable string should throw, got {result:?}"
        );
    }

    #[test]
    fn test_parse_date_string_helper() {
        // ISO numeric still works.
        assert_eq!(parse_date_string("1970-01-01"), Some(0));
        assert_eq!(parse_date_string("1970/01/01 00:00:00"), Some(0));
        // toString + RFC-1123 textual forms agree on the same instant.
        assert_eq!(parse_date_string("Thu Jan 01 00:00:00 GMT 1970"), Some(0));
        assert_eq!(parse_date_string("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // Unparseable / empty → None.
        assert_eq!(parse_date_string(""), None);
        assert_eq!(parse_date_string("garbage"), None);
        assert_eq!(parse_date_string("Jan 1970"), None); // no day-of-month
    }

    // --- T8.2.2 Date getters/setters ---

    #[test]
    fn test_date_get_year() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        // Set to 2000-06-15 12:30:45
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getYear",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(100))); // 2000 - 1900 = 100
    }

    #[test]
    fn test_date_get_month_and_date() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let month = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getMonth",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(month.unwrap(), Some(Value::Int(5))); // June = 5

        let day = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getDate",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(day.unwrap(), Some(Value::Int(15)));
    }

    #[test]
    fn test_date_get_hours_min_sec() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let hours = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getHours",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(hours.unwrap(), Some(Value::Int(12)));

        let mins = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getMinutes",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(mins.unwrap(), Some(Value::Int(30)));

        let secs = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getSeconds",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(secs.unwrap(), Some(Value::Int(45)));
    }

    #[test]
    fn test_date_get_day_of_week() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        // 1970-01-01 = Thursday = 4
        set_date_millis(&mut ctx, date_obj, 0);
        let dow = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getDay",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(dow.unwrap(), Some(Value::Int(4))); // Thursday
    }

    #[test]
    fn test_date_set_year() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        // Set year to 120 (= 2020)
        call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "setYear",
            "(I)V",
            &[Value::Object(Some(date_obj)), Value::Int(120)],
        )
        .unwrap();

        let year = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getYear",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(year.unwrap(), Some(Value::Int(120)));
    }

    #[test]
    fn test_date_set_month() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "setMonth",
            "(I)V",
            &[Value::Object(Some(date_obj)), Value::Int(11)],
        )
        .unwrap();

        let month = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "getMonth",
            "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(month.unwrap(), Some(Value::Int(11)));
    }

    // `test_date_to_locale_string` was REMOVED 2026-08-21 (WORKER 4) with the
    // registration it pinned. It asserted
    //
    //     assert_eq!(text, "01/01/1970 00:00:00");
    //
    // which is not what `Date.toLocaleString()` returns on any JDK — HotSpot
    // 25.0.4 answers `Jan 1, 1970, 12:00:00 AM` — so the test's function was to
    // hold a divergence in place. `[a test that freezes VM output locks in the
    // divergence]`. See the retirement note beside the old registration.

    #[test]
    fn test_date_to_gmt_string() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4).unwrap();
        set_date_millis(&mut ctx, date_obj, 0);

        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Date",
            "toGMTString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(date_obj))],
        );
        let val = result.unwrap().unwrap();
        if let Value::Object(Some(s)) = val {
            let text = ctx.read_string(s).unwrap();
            assert_eq!(text, "1 Jan 1970 00:00:00 GMT");
        } else {
            panic!("Expected string object");
        }
    }

    // --- T8.2.3 String hibyte constructor ---

    #[test]
    fn test_string_hibyte_constructor() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();
        // Create byte array [65, 66, 67] = "ABC" with hibyte=0
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);
        ctx.set_array_element(arr, 0, Value::Int(65));
        ctx.set_array_element(arr, 1, Value::Int(66));
        ctx.set_array_element(arr, 2, Value::Int(67));

        let result = call_native(
            &reg,
            &mut ctx,
            "java/lang/String",
            "<init>",
            "([BIII)V",
            &[
                Value::Object(Some(this)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(0),
                Value::Int(3),
            ],
        );
        assert!(result.is_ok());
    }

    // --- T8.2.4 String.getBytes deprecated ---

    #[test]
    fn test_string_get_bytes_deprecated() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let str_obj = ctx.create_string("Hello");
        let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 10);

        let result = call_native(
            &reg,
            &mut ctx,
            "java/lang/String",
            "getBytes",
            "(II[BI)V",
            &[
                Value::Object(Some(str_obj)),
                Value::Int(0),
                Value::Int(5),
                Value::Object(Some(dst)),
                Value::Int(0),
            ],
        );
        assert!(result.is_ok());
        // Check first byte = 'H' = 72
        assert_eq!(ctx.get_array_element(dst, 0), Value::Int(72));
        // 'e' = 101
        assert_eq!(ctx.get_array_element(dst, 1), Value::Int(101));
    }

    // --- T8.2.5 Character deprecated ---
    //
    // `test_character_is_java_letter`, `test_character_is_java_letter_or_digit`
    // and `test_character_is_space` were deleted with the registrations they
    // covered (see the tombstone in `register_deprecated_io_util_natives`).
    // They asserted against `setup()`, which registers ONLY this file, so they
    // exercised the copy that loses the registry slot at VM start-up and never
    // runs — three green tests over a dead body.  The live copies are covered
    // by `deprecated_util.rs`'s own tests (`isJavaLetter` at 2863/2874/2885,
    // `isJavaLetterOrDigit` at 2901/2911, `isSpace` at 2928/2943).
    //
    // Neither test set can see the real defect: both bodies answer 949
    // (`isJavaLetter`) and 1,010 (`isJavaLetterOrDigit`) of the 65,536 `char`
    // values differently from HotSpot 25, and every character these tests probe
    // (`A`, `$`, `3`, the five ASCII spaces) is in the agreeing majority.

    // --- T8.2.7 Number.byteValue / shortValue ---

    // `test_number_byte_value` and `test_number_short_value` were REMOVED
    // 2026-08-21 (WORKER 4) with the two registrations they pinned. Both drove
    // a `MockNativeContext` whose `invoke_virtual` was scripted to return the
    // value the test then asserted the narrowing of — so they tested the cast,
    // which is a Rust `as`, and could not have failed while the registration
    // existed. `regression-suite/probes/W4Deprecated.java` asks the same
    // question of a real VM against a real oracle, over 116 cases including
    // the `BigDecimal`/`BigInteger` subclasses the retired comment was about.
    // See `register_number_defaults` for the retirement note.

    // --- T8.2.8 Properties.save ---

    #[test]
    fn test_properties_save_delegates() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let props = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Properties", 4).unwrap();
        let out = try_alloc_concurrent_synthetic(&mut ctx, "java/io/OutputStream", 0).unwrap();
        let comment = ctx.create_string("test");

        // Pre-arm invoke_virtual to return Ok(None) for store()
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(None));
        }

        let result = call_native(
            &reg,
            &mut ctx,
            "java/util/Properties",
            "save",
            "(Ljava/io/OutputStream;Ljava/lang/String;)V",
            &[
                Value::Object(Some(props)),
                Value::Object(Some(out)),
                Value::Object(Some(comment)),
            ],
        );
        assert!(result.is_ok());
    }

    // --- T8.2.9 Hashtable enumerations ---

    #[test]
    fn test_hashtable_elements_and_keys() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let ht = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 4).unwrap();

        let elements = call_native(
            &reg,
            &mut ctx,
            "java/util/Hashtable",
            "elements",
            "()Ljava/util/Enumeration;",
            &[Value::Object(Some(ht))],
        );
        assert!(elements.is_ok());
        match elements.unwrap() {
            Some(Value::Object(Some(_))) => {}
            other => panic!("Expected Object(Some(_)), got {:?}", other),
        }

        let keys = call_native(
            &reg,
            &mut ctx,
            "java/util/Hashtable",
            "keys",
            "()Ljava/util/Enumeration;",
            &[Value::Object(Some(ht))],
        );
        assert!(keys.is_ok());
        match keys.unwrap() {
            Some(Value::Object(Some(_))) => {}
            other => panic!("Expected Object(Some(_)), got {:?}", other),
        }
    }

    // --- T8.2.10 StringBufferInputStream ---

    #[test]
    fn test_string_buffer_input_stream() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let sbis =
            try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
        let str_obj = ctx.create_string("AB");

        // init
        call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
        )
        .unwrap();

        // available should be 2
        let avail = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "available",
            "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(avail.unwrap(), Some(Value::Int(2)));

        // read() first byte
        let b1 = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "read",
            "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b1.unwrap(), Some(Value::Int(65))); // 'A'

        // read() second byte
        let b2 = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "read",
            "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b2.unwrap(), Some(Value::Int(66))); // 'B'

        // read() EOF
        let b3 = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "read",
            "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b3.unwrap(), Some(Value::Int(-1)));

        // reset and read again
        call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "reset",
            "()V",
            &[Value::Object(Some(sbis))],
        )
        .unwrap();

        let b1_again = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "read",
            "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b1_again.unwrap(), Some(Value::Int(65)));
    }

    #[test]
    fn test_string_buffer_input_stream_read_array() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let sbis =
            try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
        let str_obj = ctx.create_string("Hello");

        call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
        )
        .unwrap();

        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 10);
        let n = call_native(
            &reg,
            &mut ctx,
            "java/io/StringBufferInputStream",
            "read",
            "([BII)I",
            &[
                Value::Object(Some(sbis)),
                Value::Object(Some(buf)),
                Value::Int(0),
                Value::Int(3),
            ],
        );
        assert_eq!(n.unwrap(), Some(Value::Int(3)));
        assert_eq!(ctx.get_array_element(buf, 0), Value::Int(72)); // 'H'
        assert_eq!(ctx.get_array_element(buf, 1), Value::Int(101)); // 'e'
        assert_eq!(ctx.get_array_element(buf, 2), Value::Int(108)); // 'l'
    }

    // --- T8.2.13 / T8.2.14 URL encode/decode ---

    #[test]
    fn test_url_decode() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let input = ctx.create_string("Hello+World%21");

        let result = call_native(
            &reg,
            &mut ctx,
            "java/net/URLDecoder",
            "decode",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(input))],
        );
        let val = result.unwrap().unwrap();
        if let Value::Object(Some(s)) = val {
            let text = ctx.read_string(s).unwrap();
            assert_eq!(text, "Hello World!");
        } else {
            panic!("Expected string object");
        }
    }

    #[test]
    fn test_url_encode() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let input = ctx.create_string("Hello World!");

        let result = call_native(
            &reg,
            &mut ctx,
            "java/net/URLEncoder",
            "encode",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(input))],
        );
        let val = result.unwrap().unwrap();
        if let Value::Object(Some(s)) = val {
            let text = ctx.read_string(s).unwrap();
            assert_eq!(text, "Hello+World%21");
        } else {
            panic!("Expected string object");
        }
    }

    // --- Registration count ---

    #[test]
    fn test_registration_count() {
        let reg = setup();
        // Count all registrations: should be well over 40
        assert!(
            reg.len() >= 40,
            "Expected at least 40 registered deprecated natives, got {}",
            reg.len()
        );
    }

    // --- Date math edge cases ---

    #[test]
    fn test_epoch_millis_roundtrip() {
        // Verify roundtrip for a known date: 2024-03-15 10:30:00
        let ms = to_epoch_millis(2024, 2, 15, 10, 30, 0);
        let (year, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        assert_eq!(year, 2024);
        assert_eq!(month, 2); // March = 2
        assert_eq!(day, 15);
        assert_eq!(hour, 10);
        assert_eq!(min, 30);
        assert_eq!(sec, 0);
    }

    #[test]
    fn test_leap_year() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn test_url_encode_decode_roundtrip() {
        let original = "key=value&foo=bar baz";
        let encoded = url_encode(original);
        let decoded = url_decode(&encoded).expect("well-formed escape sequence");
        assert_eq!(decoded, original);
    }
}
