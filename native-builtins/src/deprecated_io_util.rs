//! Deprecated APIs from java.io, java.util, java.text, and related packages.
//!
//! This module provides native implementations for deprecated methods that
//! legacy code may still call. Every method is registered via NativeMethodRegistry.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

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

fn url_decode(input: &str) -> String {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                hex_digit(bytes[i + 1]),
                hex_digit(bytes[i + 2]),
            ) {
                result.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
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

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_deprecated_io_util_natives(r: &mut NativeMethodRegistry) {
    register_date_constructors(r);
    register_date_getters_setters(r);
    register_string_deprecated(r);
    register_character_deprecated(r);
    register_class_new_instance(r);
    register_number_defaults(r);
    register_properties_save(r);
    register_hashtable_enumerations(r);
    register_string_buffer_input_stream(r);
    register_line_number_input_stream(r);
    register_locale_noop(r);
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
        let year = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let month = match args.get(2) { Some(Value::Int(v)) => *v, _ => 0 };
        let day = match args.get(3) { Some(Value::Int(v)) => *v, _ => 1 };
        let millis = to_epoch_millis(year + 1900, month, day, 0, 0, 0);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(IIIII)V — year, month, date, hrs, min
    r.register(date, "<init>", "(IIIII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let month = match args.get(2) { Some(Value::Int(v)) => *v, _ => 0 };
        let day = match args.get(3) { Some(Value::Int(v)) => *v, _ => 1 };
        let hrs = match args.get(4) { Some(Value::Int(v)) => *v, _ => 0 };
        let min = match args.get(5) { Some(Value::Int(v)) => *v, _ => 0 };
        let millis = to_epoch_millis(year + 1900, month, day, hrs, min, 0);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(IIIIII)V — year, month, date, hrs, min, sec
    r.register(date, "<init>", "(IIIIII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let month = match args.get(2) { Some(Value::Int(v)) => *v, _ => 0 };
        let day = match args.get(3) { Some(Value::Int(v)) => *v, _ => 1 };
        let hrs = match args.get(4) { Some(Value::Int(v)) => *v, _ => 0 };
        let min = match args.get(5) { Some(Value::Int(v)) => *v, _ => 0 };
        let sec = match args.get(6) { Some(Value::Int(v)) => *v, _ => 0 };
        let millis = to_epoch_millis(year + 1900, month, day, hrs, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // <init>(Ljava/lang/String;)V — Date.parse(String) form
    // Simplified: attempt to parse a date string, default to epoch 0 on failure.
    r.register(date, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let str_obj = obj_arg(args, 1)?;
        let text = ctx.read_string(str_obj).unwrap_or_default();
        // Attempt a simple parse: "Mon DD HH:MM:SS YYYY" or numeric forms.
        // Full Date.parse is extremely complex; we handle common patterns.
        let millis = parse_date_string(&text);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });
}

/// Simple date string parser for the deprecated Date(String) constructor.
/// Handles "yyyy/mm/dd", "yyyy-mm-dd", and falls back to epoch 0.
fn parse_date_string(s: &str) -> i64 {
    let s = s.trim();
    // Try "YYYY/MM/DD" or "YYYY-MM-DD" optionally with "HH:MM:SS"
    let parts: Vec<&str> = s.splitn(2, |c: char| c == ' ' || c == 'T').collect();
    let date_part = parts.first().unwrap_or(&"");
    let time_part = parts.get(1).unwrap_or(&"");

    let date_nums: Vec<i32> = date_part
        .split(|c: char| c == '/' || c == '-')
        .filter_map(|p| p.parse::<i32>().ok())
        .collect();

    if date_nums.len() >= 3 {
        let (year, month_1, day) = (date_nums[0], date_nums[1], date_nums[2]);
        let (hour, min, sec) = parse_time_part(time_part);
        return to_epoch_millis(year, month_1 - 1, day, hour, min, sec);
    }

    0 // fallback to epoch
}

fn parse_time_part(s: &str) -> (i32, i32, i32) {
    let nums: Vec<i32> = s
        .split(':')
        .filter_map(|p| p.trim().parse::<i32>().ok())
        .collect();
    (
        *nums.first().unwrap_or(&0),
        *nums.get(1).unwrap_or(&0),
        *nums.get(2).unwrap_or(&0),
    )
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
        let new_year = match args.get(1) { Some(Value::Int(v)) => *v, _ => 70 };
        let ms = get_date_millis(ctx, this);
        let (_, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(new_year + 1900, month, day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setMonth(I)V
    r.register(date, "setMonth", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_month = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let ms = get_date_millis(ctx, this);
        let (year, _, day, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, new_month, day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setDate(I)V
    r.register(date, "setDate", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_day = match args.get(1) { Some(Value::Int(v)) => *v, _ => 1 };
        let ms = get_date_millis(ctx, this);
        let (year, month, _, hour, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, new_day, hour, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setHours(I)V
    r.register(date, "setHours", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_hours = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, _, min, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, new_hours, min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setMinutes(I)V
    r.register(date, "setMinutes", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_min = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, _, sec, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, hour, new_min, sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // setSeconds(I)V
    r.register(date, "setSeconds", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_sec = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, min, _, _) = from_epoch_millis(ms);
        let millis = to_epoch_millis(year, month, day, hour, min, new_sec);
        set_date_millis(ctx, this, millis);
        Ok(None)
    });

    // toLocaleString()Ljava/lang/String;
    r.register(date, "toLocaleString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        let formatted = format!(
            "{:02}/{:02}/{:04} {:02}:{:02}:{:02}",
            month + 1, day, year, hour, min, sec
        );
        let s = ctx.create_string(&formatted);
        Ok(Some(Value::Object(Some(s))))
    });

    // toGMTString()Ljava/lang/String;
    r.register(date, "toGMTString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = get_date_millis(ctx, this);
        let (year, month, day, hour, min, sec, _) = from_epoch_millis(ms);
        let month_name = match month {
            0 => "Jan", 1 => "Feb", 2 => "Mar", 3 => "Apr",
            4 => "May", 5 => "Jun", 6 => "Jul", 7 => "Aug",
            8 => "Sep", 9 => "Oct", 10 => "Nov", 11 => "Dec",
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
        let hibyte = match args.get(2) { Some(Value::Int(v)) => *v, _ => 0 };
        let offset = match args.get(3) { Some(Value::Int(v)) => *v as usize, _ => 0 };
        let count = match args.get(4) { Some(Value::Int(v)) => *v as usize, _ => 0 };

        let arr_len = ctx.array_length(byte_arr);
        if offset + count > arr_len {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException {
                index: (offset + count) as i32,
            }.into());
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
        let src_begin = match args.get(1) { Some(Value::Int(v)) => *v as usize, _ => 0 };
        let src_end = match args.get(2) { Some(Value::Int(v)) => *v as usize, _ => 0 };
        let dst = obj_arg(args, 3)?;
        let dst_begin = match args.get(4) { Some(Value::Int(v)) => *v as usize, _ => 0 };

        let text = ctx.read_string(this).unwrap_or_default();
        let chars: Vec<u16> = text.encode_utf16().collect();

        if src_end > chars.len() {
            return Err(RuntimeError::StringIndexOutOfBoundsException {
                index: src_end as i32,
            }.into());
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
// ---------------------------------------------------------------------------

fn register_character_deprecated(r: &mut NativeMethodRegistry) {
    let ch = "java/lang/Character";

    // isJavaLetter(C)Z — delegates to isJavaIdentifierStart
    r.register(ch, "isJavaLetter", "(C)Z", |_ctx, args| {
        let c = match args.get(0) { Some(Value::Int(v)) => *v as u16, _ => 0 };
        let ch = char::from_u32(c as u32).unwrap_or('\0');
        let result = ch.is_alphabetic() || ch == '_' || ch == '$';
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    });

    // isJavaLetterOrDigit(C)Z — delegates to isJavaIdentifierPart
    r.register(ch, "isJavaLetterOrDigit", "(C)Z", |_ctx, args| {
        let c = match args.get(0) { Some(Value::Int(v)) => *v as u16, _ => 0 };
        let ch = char::from_u32(c as u32).unwrap_or('\0');
        let result = ch.is_alphanumeric() || ch == '_' || ch == '$';
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    });

    // isSpace(C)Z — true for ' ', '\t', '\n', '\r', '\f'
    r.register(ch, "isSpace", "(C)Z", |_ctx, args| {
        let c = match args.get(0) { Some(Value::Int(v)) => *v as u16, _ => 0 };
        let result = matches!(c, 0x20 | 0x09 | 0x0A | 0x0D | 0x0C);
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    });
}

// ---------------------------------------------------------------------------
// T8.2.6 — Class.newInstance()
// ---------------------------------------------------------------------------

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
                None => return Err(RuntimeError::UnsupportedOperationException {
                    message: "Class.newInstance: receiver is not a Class mirror".to_string(),
                }.into()),
            };
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if class_name.is_empty() {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: "Class.newInstance: receiver mirror has no class name".to_string(),
                }.into());
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
                    return Err(RuntimeError::UnsupportedOperationException {
                        message: format!(
                            "InstantiationException: cannot instantiate abstract/interface type {class_name}"
                        ),
                    }.into());
                }
            }

            // Check if the no-arg constructor exists
            if !ctx.method_exists(&class_name, "<init>", "()V") {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!("InstantiationException: no no-arg constructor in {}", class_name),
                }.into());
            }

            // Create a new instance and invoke the constructor
            let result = ctx.new_object(&class_name)?;
            match result {
                Some(Value::Object(Some(obj))) => {
                    ctx.invoke(
                        &class_name,
                        "<init>",
                        "()V",
                        &[Value::Object(Some(obj))],
                    )?;
                    Ok(Some(Value::Object(Some(obj))))
                }
                _ => Err(RuntimeError::UnsupportedOperationException {
                    message: format!("InstantiationException: failed to instantiate {}", class_name),
                }.into()),
            }
        },
    );
}

// ---------------------------------------------------------------------------
// T8.2.7 — Number.byteValue() / shortValue()
// ---------------------------------------------------------------------------

fn register_number_defaults(r: &mut NativeMethodRegistry) {
    let num = "java/lang/Number";

    // byteValue()B — returns (byte) intValue()
    r.register(num, "byteValue", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // intValue is stored in field 0 for boxed numbers
        let int_val = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            Value::Long(v) => v as i32,
            Value::Float(f) => f as i32,
            Value::Double(d) => d as i32,
            _ => 0,
        };
        Ok(Some(Value::Int((int_val as i8) as i32)))
    });

    // shortValue()S — returns (short) intValue()
    r.register(num, "shortValue", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let int_val = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            Value::Long(v) => v as i32,
            Value::Float(f) => f as i32,
            Value::Double(d) => d as i32,
            _ => 0,
        };
        Ok(Some(Value::Int((int_val as i16) as i32)))
    });
}

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
            if args.len() > 1 { store_args.push(args[1]); }
            if args.len() > 2 { store_args.push(args[2]); }
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
        let buckets = match ctx.get_field(this, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return Vec::new(),
        };
        let cap = match ctx.get_field(this, 2) {
            Value::Int(c) if c > 0 => c as usize,
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for i in 0..cap {
            let mut node_val = ctx.get_array_element(buckets, i);
            while let Value::Object(Some(node)) = node_val {
                let v = if want_keys {
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
        let en = alloc_concurrent_synthetic(ctx, "java/util/Enumeration", 2);
        ctx.set_field(en, 0, Value::Object(Some(arr)));
        ctx.set_field(en, 1, Value::Int(0));
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
        ctx.set_field(this, 1, Value::Int(0));    // position
        ctx.set_field(this, 2, Value::Int(len));   // count
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
        let off = match args.get(2) { Some(Value::Int(v)) => *v as usize, _ => 0 };
        let len = match args.get(3) { Some(Value::Int(v)) => *v as usize, _ => 0 };

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
        ctx.set_field(this, 1, Value::Int(0));    // line number
        ctx.set_field(this, 2, Value::Int(0));    // saved line number
        ctx.set_field(this, 3, Value::Int(-1));   // pushback byte
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
        let off = match args.get(2) { Some(Value::Int(v)) => *v as usize, _ => 0 };
        let len = match args.get(3) { Some(Value::Int(v)) => *v as usize, _ => 0 };

        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }

        // Read one byte at a time through the single-byte read to get line tracking
        // This is what the JDK implementation does
        let inner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(-1))),
        };

        let mut count = 0i32;
        for i in 0..len {
            let pushback = match ctx.get_field(this, 3) {
                Value::Int(v) => v,
                _ => -1,
            };

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
        let num = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
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
        let readlimit = match args.get(1) { Some(Value::Int(v)) => *v, _ => 0 };
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

fn register_locale_noop(r: &mut NativeMethodRegistry) {
    // No-op marker: getISO3Language deprecated alias is Java-level delegation.
    r.register(
        "java/util/Locale",
        "__deprecated_marker__",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

// ---------------------------------------------------------------------------
// T8.2.13 / T8.2.14 — URLDecoder.decode(String) / URLEncoder.encode(String)
// ---------------------------------------------------------------------------

fn register_url_codec(r: &mut NativeMethodRegistry) {
    // URLDecoder.decode(String)String — deprecated single-arg form, uses UTF-8
    r.register(
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let decoded = url_decode(&text);
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
            // Charset name is in args[1] — we always use UTF-8 for simplicity
            let decoded = url_decode(&text);
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
            // Charset object is in args[1] — we always use UTF-8 for simplicity
            let decoded = url_decode(&text);
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
            let encoded = url_encode(&text);
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
            let encoded = url_encode(&text);
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

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
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "<init>", "(III)V",
            &[Value::Object(Some(date_obj)), Value::Int(70), Value::Int(0), Value::Int(1)],
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
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "<init>", "(IIIII)V",
            &[Value::Object(Some(date_obj)), Value::Int(70), Value::Int(0), Value::Int(1),
              Value::Int(1), Value::Int(0)],
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
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "<init>", "(IIIIII)V",
            &[Value::Object(Some(date_obj)), Value::Int(70), Value::Int(0), Value::Int(1),
              Value::Int(0), Value::Int(0), Value::Int(30)],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        assert_eq!(ms, 30_000);
    }

    #[test]
    fn test_date_init_string() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let str_obj = ctx.create_string("2000/01/01");
        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "<init>", "(Ljava/lang/String;)V",
            &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
        );
        assert!(result.is_ok());
        let ms = get_date_millis(&mut ctx, date_obj);
        // 2000-01-01 00:00:00 UTC
        assert_eq!(ms, 946684800000);
    }

    // --- T8.2.2 Date getters/setters ---

    #[test]
    fn test_date_get_year() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        // Set to 2000-06-15 12:30:45
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "getYear", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(100))); // 2000 - 1900 = 100
    }

    #[test]
    fn test_date_get_month_and_date() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let month = call_native(
            &reg, &mut ctx, "java/util/Date", "getMonth", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(month.unwrap(), Some(Value::Int(5))); // June = 5

        let day = call_native(
            &reg, &mut ctx, "java/util/Date", "getDate", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(day.unwrap(), Some(Value::Int(15)));
    }

    #[test]
    fn test_date_get_hours_min_sec() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        let hours = call_native(
            &reg, &mut ctx, "java/util/Date", "getHours", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(hours.unwrap(), Some(Value::Int(12)));

        let mins = call_native(
            &reg, &mut ctx, "java/util/Date", "getMinutes", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(mins.unwrap(), Some(Value::Int(30)));

        let secs = call_native(
            &reg, &mut ctx, "java/util/Date", "getSeconds", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(secs.unwrap(), Some(Value::Int(45)));
    }

    #[test]
    fn test_date_get_day_of_week() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        // 1970-01-01 = Thursday = 4
        set_date_millis(&mut ctx, date_obj, 0);
        let dow = call_native(
            &reg, &mut ctx, "java/util/Date", "getDay", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(dow.unwrap(), Some(Value::Int(4))); // Thursday
    }

    #[test]
    fn test_date_set_year() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        // Set year to 120 (= 2020)
        call_native(
            &reg, &mut ctx, "java/util/Date", "setYear", "(I)V",
            &[Value::Object(Some(date_obj)), Value::Int(120)],
        ).unwrap();

        let year = call_native(
            &reg, &mut ctx, "java/util/Date", "getYear", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(year.unwrap(), Some(Value::Int(120)));
    }

    #[test]
    fn test_date_set_month() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
        set_date_millis(&mut ctx, date_obj, ms);

        call_native(
            &reg, &mut ctx, "java/util/Date", "setMonth", "(I)V",
            &[Value::Object(Some(date_obj)), Value::Int(11)],
        ).unwrap();

        let month = call_native(
            &reg, &mut ctx, "java/util/Date", "getMonth", "()I",
            &[Value::Object(Some(date_obj))],
        );
        assert_eq!(month.unwrap(), Some(Value::Int(11)));
    }

    #[test]
    fn test_date_to_locale_string() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        // Epoch 0 = 1970-01-01 00:00:00
        set_date_millis(&mut ctx, date_obj, 0);

        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "toLocaleString", "()Ljava/lang/String;",
            &[Value::Object(Some(date_obj))],
        );
        let val = result.unwrap().unwrap();
        if let Value::Object(Some(s)) = val {
            let text = ctx.read_string(s).unwrap();
            assert_eq!(text, "01/01/1970 00:00:00");
        } else {
            panic!("Expected string object");
        }
    }

    #[test]
    fn test_date_to_gmt_string() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let date_obj = alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 4);
        set_date_millis(&mut ctx, date_obj, 0);

        let result = call_native(
            &reg, &mut ctx, "java/util/Date", "toGMTString", "()Ljava/lang/String;",
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
        let this = alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4);
        // Create byte array [65, 66, 67] = "ABC" with hibyte=0
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);
        ctx.set_array_element(arr, 0, Value::Int(65));
        ctx.set_array_element(arr, 1, Value::Int(66));
        ctx.set_array_element(arr, 2, Value::Int(67));

        let result = call_native(
            &reg, &mut ctx, "java/lang/String", "<init>", "([BIII)V",
            &[Value::Object(Some(this)), Value::Object(Some(arr)),
              Value::Int(0), Value::Int(0), Value::Int(3)],
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
            &reg, &mut ctx, "java/lang/String", "getBytes", "(II[BI)V",
            &[Value::Object(Some(str_obj)), Value::Int(0), Value::Int(5),
              Value::Object(Some(dst)), Value::Int(0)],
        );
        assert!(result.is_ok());
        // Check first byte = 'H' = 72
        assert_eq!(ctx.get_array_element(dst, 0), Value::Int(72));
        // 'e' = 101
        assert_eq!(ctx.get_array_element(dst, 1), Value::Int(101));
    }

    // --- T8.2.5 Character deprecated ---

    #[test]
    fn test_character_is_java_letter() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg, &mut ctx, "java/lang/Character", "isJavaLetter", "(C)Z",
            &[Value::Int('A' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));

        let result = call_native(
            &reg, &mut ctx, "java/lang/Character", "isJavaLetter", "(C)Z",
            &[Value::Int('$' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));

        let result = call_native(
            &reg, &mut ctx, "java/lang/Character", "isJavaLetter", "(C)Z",
            &[Value::Int('3' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_character_is_java_letter_or_digit() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg, &mut ctx, "java/lang/Character", "isJavaLetterOrDigit", "(C)Z",
            &[Value::Int('3' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn test_character_is_space() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();

        for &ch in &[' ', '\t', '\n', '\r', '\x0c'] {
            let result = call_native(
                &reg, &mut ctx, "java/lang/Character", "isSpace", "(C)Z",
                &[Value::Int(ch as i32)],
            );
            assert_eq!(result.unwrap(), Some(Value::Int(1)), "Expected true for {:?}", ch);
        }

        let result = call_native(
            &reg, &mut ctx, "java/lang/Character", "isSpace", "(C)Z",
            &[Value::Int('x' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    // --- T8.2.7 Number.byteValue / shortValue ---

    #[test]
    fn test_number_byte_value() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let num = alloc_concurrent_synthetic(&mut ctx, "java/lang/Number", 4);
        ctx.set_field(num, 0, Value::Int(300));

        let result = call_native(
            &reg, &mut ctx, "java/lang/Number", "byteValue", "()B",
            &[Value::Object(Some(num))],
        );
        // 300 as byte = 44 (300 & 0xFF = 44, then as i8 = 44)
        assert_eq!(result.unwrap(), Some(Value::Int(44)));
    }

    #[test]
    fn test_number_short_value() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let num = alloc_concurrent_synthetic(&mut ctx, "java/lang/Number", 4);
        ctx.set_field(num, 0, Value::Int(70000));

        let result = call_native(
            &reg, &mut ctx, "java/lang/Number", "shortValue", "()S",
            &[Value::Object(Some(num))],
        );
        // 70000 as i16 = 4464
        assert_eq!(result.unwrap(), Some(Value::Int(70000i32 as i16 as i32)));
    }

    // --- T8.2.8 Properties.save ---

    #[test]
    fn test_properties_save_delegates() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let props = alloc_concurrent_synthetic(&mut ctx, "java/util/Properties", 4);
        let out = alloc_concurrent_synthetic(&mut ctx, "java/io/OutputStream", 0);
        let comment = ctx.create_string("test");

        // Pre-arm invoke_virtual to return Ok(None) for store()
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(None));
        }

        let result = call_native(
            &reg, &mut ctx, "java/util/Properties", "save",
            "(Ljava/io/OutputStream;Ljava/lang/String;)V",
            &[Value::Object(Some(props)), Value::Object(Some(out)),
              Value::Object(Some(comment))],
        );
        assert!(result.is_ok());
    }

    // --- T8.2.9 Hashtable enumerations ---

    #[test]
    fn test_hashtable_elements_and_keys() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let ht = alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 4);

        let elements = call_native(
            &reg, &mut ctx, "java/util/Hashtable", "elements",
            "()Ljava/util/Enumeration;",
            &[Value::Object(Some(ht))],
        );
        assert!(elements.is_ok());
        match elements.unwrap() {
            Some(Value::Object(Some(_))) => {}
            other => panic!("Expected Object(Some(_)), got {:?}", other),
        }

        let keys = call_native(
            &reg, &mut ctx, "java/util/Hashtable", "keys",
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
        let sbis = alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4);
        let str_obj = ctx.create_string("AB");

        // init
        call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
        ).unwrap();

        // available should be 2
        let avail = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "available", "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(avail.unwrap(), Some(Value::Int(2)));

        // read() first byte
        let b1 = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "read", "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b1.unwrap(), Some(Value::Int(65))); // 'A'

        // read() second byte
        let b2 = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "read", "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b2.unwrap(), Some(Value::Int(66))); // 'B'

        // read() EOF
        let b3 = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "read", "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b3.unwrap(), Some(Value::Int(-1)));

        // reset and read again
        call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "reset", "()V",
            &[Value::Object(Some(sbis))],
        ).unwrap();

        let b1_again = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "read", "()I",
            &[Value::Object(Some(sbis))],
        );
        assert_eq!(b1_again.unwrap(), Some(Value::Int(65)));
    }

    #[test]
    fn test_string_buffer_input_stream_read_array() {
        let reg = setup();
        let mut ctx = MockNativeContext::new();
        let sbis = alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4);
        let str_obj = ctx.create_string("Hello");

        call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
        ).unwrap();

        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 10);
        let n = call_native(
            &reg, &mut ctx, "java/io/StringBufferInputStream", "read", "([BII)I",
            &[Value::Object(Some(sbis)), Value::Object(Some(buf)),
              Value::Int(0), Value::Int(3)],
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
            &reg, &mut ctx, "java/net/URLDecoder", "decode",
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
            &reg, &mut ctx, "java/net/URLEncoder", "encode",
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
        let decoded = url_decode(&encoded);
        assert_eq!(decoded, original);
    }
}
