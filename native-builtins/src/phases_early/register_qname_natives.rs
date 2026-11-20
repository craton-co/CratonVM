#[allow(unused_imports)]
use super::*;

pub fn register_qname_natives(r: &mut NativeMethodRegistry) {
    let c = "javax/xml/namespace/QName";
    r.register(c, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        qname_init(ctx, args, None, args.get(1).copied(), None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        c,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| qname_init(ctx, args, args.get(1).copied(), args.get(2).copied(), None), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        c,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let ns = args.get(1).copied();
            let local = args.get(2).copied();
            let prefix = args.get(3).copied();
            qname_init(ctx, args, ns, local, prefix)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(c, "getNamespaceURI", "()Ljava/lang/String;", |ctx, args| {
        Ok(Some(qname_part(
            ctx,
            obj_arg(args, 0)?,
            "namespaceURI",
            0,
            0,
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getLocalPart", "()Ljava/lang/String;", |ctx, args| {
        Ok(Some(qname_part(ctx, obj_arg(args, 0)?, "localPart", 1, 1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getPrefix", "()Ljava/lang/String;", |ctx, args| {
        Ok(Some(qname_part(ctx, obj_arg(args, 0)?, "prefix", 2, 2)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ns = qname_part_text(ctx, this, "namespaceURI", 0, 0);
        let local = qname_part_text(ctx, this, "localPart", 1, 1);
        let text = if ns.is_empty() {
            local
        } else {
            format!("{{{ns}}}{local}")
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---------------------------------------------------------------------------
// Calendar / GregorianCalendar — 8-field synthetic
// ---------------------------------------------------------------------------
pub const CAL_FIELD_YEAR: usize = 0;
pub const CAL_FIELD_MONTH: usize = 1;
pub const CAL_FIELD_DAY: usize = 2;
pub const CAL_FIELD_HOUR: usize = 3;
pub const CAL_FIELD_MINUTE: usize = 4;
pub const CAL_FIELD_SECOND: usize = 5;
pub const CAL_FIELD_MILLIS: usize = 6;
pub const CAL_FIELD_TIMEZONE: usize = 7;
/// W2: `setLenient`/`setFirstDayOfWeek` used to be no-ops. Both settings now
/// live in slots appended after the original 8, so `alloc_calendar` does not
/// have to initialise them: 0 means "never set" for both (see the two
/// accessors' comments), which is exactly the JDK default in each case.
pub const CAL_FIELD_STRICT: usize = 8;
pub const CAL_FIELD_FIRST_DAY_OF_WEEK: usize = 9;
pub const CAL_NUM_FIELDS: usize = 10;

/// Width-guarded read of one of the two appended Calendar setting slots.
///
/// A receiver can be narrower than `CAL_NUM_FIELDS` — `vm.rs`'s
/// `gregorian_calendar_leap_year` test allocates an 8-field carrier directly,
/// and in real-JDK mode the receiver is a genuine `GregorianCalendar` whose
/// layout is not ours at all — so a narrow object reads 0, which both callers
/// treat as "unset / JDK default" rather than running off the end.
pub fn cal_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> i32 {
    if ctx.object_num_fields(this) > slot {
        ctx.get_field(this, slot).as_int().unwrap_or(0)
    } else {
        0
    }
}

pub fn cal_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: i32) {
    if ctx.object_num_fields(this) > slot {
        ctx.set_field(this, slot, Value::Int(v));
    }
}

pub fn alloc_calendar(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let cal = try_alloc_concurrent_synthetic(ctx, "java/util/GregorianCalendar", CAL_NUM_FIELDS)?;
    ctx.set_field(cal, CAL_FIELD_YEAR, Value::Int(1970));
    ctx.set_field(cal, CAL_FIELD_MONTH, Value::Int(0));
    ctx.set_field(cal, CAL_FIELD_DAY, Value::Int(1));
    ctx.set_field(cal, CAL_FIELD_HOUR, Value::Int(0));
    ctx.set_field(cal, CAL_FIELD_MINUTE, Value::Int(0));
    ctx.set_field(cal, CAL_FIELD_SECOND, Value::Int(0));
    ctx.set_field(cal, CAL_FIELD_MILLIS, Value::Int(0));
    ctx.set_field(cal, CAL_FIELD_TIMEZONE, Value::Object(None));
    Ok(cal)
}

pub fn cal_get_field_index(java_field: i32) -> Option<usize> {
    match java_field {
        1 => Some(CAL_FIELD_YEAR),
        2 => Some(CAL_FIELD_MONTH),
        5 => Some(CAL_FIELD_DAY),
        11 => Some(CAL_FIELD_HOUR),
        12 => Some(CAL_FIELD_MINUTE),
        13 => Some(CAL_FIELD_SECOND),
        14 => Some(CAL_FIELD_MILLIS),
        _ => None,
    }
}

pub fn cal_to_epoch_millis(ctx: &mut dyn NativeContext, cal: ObjectRef) -> i64 {
    let y = match ctx.get_field(cal, CAL_FIELD_YEAR) {
        Value::Int(v) => v,
        _ => 1970,
    };
    let m = match ctx.get_field(cal, CAL_FIELD_MONTH) {
        Value::Int(v) => v,
        _ => 0,
    };
    let d = match ctx.get_field(cal, CAL_FIELD_DAY) {
        Value::Int(v) => v,
        _ => 1,
    };
    let h = match ctx.get_field(cal, CAL_FIELD_HOUR) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let min = match ctx.get_field(cal, CAL_FIELD_MINUTE) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let s = match ctx.get_field(cal, CAL_FIELD_SECOND) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let ms = match ctx.get_field(cal, CAL_FIELD_MILLIS) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let month_1based = m + 1;
    // `GregorianCalendar` is a hybrid Julian/Gregorian calendar: before the
    // default cutover (1582-10-15) its fields are Julian, not proleptic
    // Gregorian. See `julian_date.rs`.
    let epoch_day = crate::julian_date::hybrid_to_epoch_day(y, month_1based, d);
    epoch_day * 86_400_000 + h * 3_600_000 + min * 60_000 + s * 1000 + ms
}

pub fn cal_from_epoch_millis(ctx: &mut dyn NativeContext, cal: ObjectRef, millis: i64) {
    let day_millis = 86_400_000i64;
    let epoch_day = millis.div_euclid(day_millis);
    let mut time_of_day = millis.rem_euclid(day_millis);
    let h = (time_of_day / 3_600_000) as i32;
    time_of_day %= 3_600_000;
    let min = (time_of_day / 60_000) as i32;
    time_of_day %= 60_000;
    let s = (time_of_day / 1000) as i32;
    let ms = (time_of_day % 1000) as i32;
    // Same hybrid decode as the encode above, and for the same reason.
    let (year, m, d) = crate::julian_date::hybrid_from_epoch_day(epoch_day);
    ctx.set_field(cal, CAL_FIELD_YEAR, Value::Int(year));
    ctx.set_field(cal, CAL_FIELD_MONTH, Value::Int(m - 1));
    ctx.set_field(cal, CAL_FIELD_DAY, Value::Int(d));
    ctx.set_field(cal, CAL_FIELD_HOUR, Value::Int(h));
    ctx.set_field(cal, CAL_FIELD_MINUTE, Value::Int(min));
    ctx.set_field(cal, CAL_FIELD_SECOND, Value::Int(s));
    ctx.set_field(cal, CAL_FIELD_MILLIS, Value::Int(ms));
}

pub fn native_cal_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(1970));
    ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_DAY, Value::Int(1));
    ctx.set_field(this, CAL_FIELD_HOUR, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_MINUTE, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_SECOND, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_MILLIS, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_TIMEZONE, Value::Object(None));
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_get_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let cal = alloc_calendar(ctx);
    Ok(Some(Value::Object(Some(cal?))))
}

pub fn native_cal_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let field = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    if field == 0 {
        return Ok(Some(Value::Int(1)));
    }
    if let Some(idx) = cal_get_field_index(field) {
        Ok(Some(ctx.get_field(this, idx)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn native_cal_set_field(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let field = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if let Some(idx) = cal_get_field_index(field) {
        ctx.set_field(this, idx, Value::Int(value));
    }
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_set_ymd(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1970,
    };
    let month = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let day = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(year));
    ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(month));
    ctx.set_field(this, CAL_FIELD_DAY, Value::Int(day));
    Ok(Some(Value::Object(None)))
}

#[allow(clippy::too_many_arguments)]
pub fn native_cal_set_ymdhms(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1970,
    };
    let month = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let day = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let hour = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let minute = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let second = match args.get(6) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(year));
    ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(month));
    ctx.set_field(this, CAL_FIELD_DAY, Value::Int(day));
    ctx.set_field(this, CAL_FIELD_HOUR, Value::Int(hour));
    ctx.set_field(this, CAL_FIELD_MINUTE, Value::Int(minute));
    ctx.set_field(this, CAL_FIELD_SECOND, Value::Int(second));
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_get_time_millis(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let millis = cal_to_epoch_millis(ctx, this);
    Ok(Some(Value::Long(millis)))
}

pub fn native_cal_set_time_millis(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let millis = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    cal_from_epoch_millis(ctx, this, millis);
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let field = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Object(None))),
    };
    let amount = match args.get(2) {
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let millis = cal_to_epoch_millis(ctx, this);
    let delta = match field {
        1 => {
            let y = match ctx.get_field(this, CAL_FIELD_YEAR) {
                Value::Int(v) => v,
                _ => 1970,
            };
            ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(y + amount as i32));
            return Ok(Some(Value::Object(None)));
        }
        2 => {
            let m = match ctx.get_field(this, CAL_FIELD_MONTH) {
                Value::Int(v) => v,
                _ => 0,
            };
            let total = m as i64 + amount;
            let new_year_add = if total >= 0 {
                total / 12
            } else {
                (total - 11) / 12
            };
            let new_month = ((total % 12) + 12) % 12;
            let y = match ctx.get_field(this, CAL_FIELD_YEAR) {
                Value::Int(v) => v,
                _ => 1970,
            };
            ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(y + new_year_add as i32));
            ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(new_month as i32));
            return Ok(Some(Value::Object(None)));
        }
        5 => amount * 86_400_000,
        11 => amount * 3_600_000,
        12 => amount * 60_000,
        13 => amount * 1000,
        14 => amount,
        _ => 0,
    };
    cal_from_epoch_millis(ctx, this, millis + delta);
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_get_time(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let millis = cal_to_epoch_millis(ctx, this);
    let date = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
    ctx.set_field(date, 0, Value::Long(millis));
    Ok(Some(Value::Object(Some(date))))
}

pub fn native_cal_set_time_from_date(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let date = obj_arg(args, 1)?;
    let millis = match ctx.get_field(date, 0) {
        Value::Long(v) => v,
        _ => 0,
    };
    cal_from_epoch_millis(ctx, this, millis);
    Ok(Some(Value::Object(None)))
}

pub fn native_cal_before(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let a = cal_to_epoch_millis(ctx, this);
    let b = cal_to_epoch_millis(ctx, other);
    Ok(Some(Value::Int(if a < b { 1 } else { 0 })))
}

pub fn native_cal_after(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let a = cal_to_epoch_millis(ctx, this);
    let b = cal_to_epoch_millis(ctx, other);
    Ok(Some(Value::Int(if a > b { 1 } else { 0 })))
}

pub fn native_cal_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let a = cal_to_epoch_millis(ctx, this);
    let b = cal_to_epoch_millis(ctx, other);
    Ok(Some(Value::Int(a.cmp(&b) as i32)))
}

pub fn native_cal_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Pin across the allocation — a moving young GC there would relocate
    // `this` before the copy loop below (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let clone = try_alloc_concurrent_synthetic(ctx, "java/util/GregorianCalendar", CAL_NUM_FIELDS)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    // W2: `CAL_NUM_FIELDS` grew from 8 to 10 (leniency + first-day-of-week),
    // but the source may still be a narrower carrier, so bound the copy by the
    // receiver's actual width rather than reading off its end.
    let n = CAL_NUM_FIELDS.min(ctx.object_num_fields(this));
    for i in 0..n {
        let v = ctx.get_field(this, i);
        ctx.set_field(clone, i, v);
    }
    Ok(Some(Value::Object(Some(clone))))
}

pub fn native_cal_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(1970));
    ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_DAY, Value::Int(1));
    ctx.set_field(this, CAL_FIELD_HOUR, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_MINUTE, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_SECOND, Value::Int(0));
    ctx.set_field(this, CAL_FIELD_MILLIS, Value::Int(0));
    Ok(Some(Value::Object(None)))
}

/// `GregorianCalendar.isLeapYear(int)` — cutover-aware, matching the real
/// implementation's `year > 1582 ? Gregorian rule : Julian rule` split
/// (default cutover, year 1582 itself goes to the Julian arm because the
/// cutover date, October, is not before March — see `julian_date.rs`).
pub fn native_cal_is_leap_year(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let year = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let leap = if year > 1582 {
        crate::civil_date::is_leap_year(year)
    } else {
        crate::julian_date::is_leap_year(year)
    };
    Ok(Some(Value::Int(if leap { 1 } else { 0 })))
}

/// `Calendar.getActualMaximum(DAY_OF_MONTH)` — only February's length is
/// leap-rule-sensitive, and a year at or before the default cutover (1582)
/// is governed by the Julian rule (see `native_cal_is_leap_year` above).
pub fn days_in_month_cal(year: i32, month: i32) -> i32 {
    match month {
        0 | 2 | 4 | 6 | 7 | 9 | 11 => 31,
        3 | 5 | 8 | 10 => 30,
        1 => {
            let leap = if year > 1582 {
                crate::civil_date::is_leap_year(year)
            } else {
                crate::julian_date::is_leap_year(year)
            };
            if leap {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

pub fn native_cal_get_actual_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let field = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    match field {
        1 => Ok(Some(Value::Int(292_278_994))),
        2 => Ok(Some(Value::Int(11))),
        5 => {
            let year = match ctx.get_field(this, CAL_FIELD_YEAR) {
                Value::Int(v) => v,
                _ => 1970,
            };
            let month = match ctx.get_field(this, CAL_FIELD_MONTH) {
                Value::Int(v) => v,
                _ => 0,
            };
            Ok(Some(Value::Int(days_in_month_cal(year, month))))        }
        11 => Ok(Some(Value::Int(23))),
        12 | 13 => Ok(Some(Value::Int(59))),
        14 => Ok(Some(Value::Int(999))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// `Calendar.getActualMinimum(int)`.
///
/// W2: this used to answer a constant `1` for every field, which is wrong for
/// most of them — `MONTH`'s minimum is 0 (JANUARY), every time-of-day field's
/// is 0, and `DAY_OF_WEEK_IN_MONTH`'s is -1. A caller normalising a field to
/// its legal range (`Math.max(v, cal.getActualMinimum(f))`) silently clamped
/// January to February and midnight to 01:00.
///
/// The minima below are all field-independent for a Gregorian calendar, so
/// unlike `getActualMaximum` this needs no receiver state. Field ids are the
/// `java.util.Calendar` constants, matching `native_cal_get_actual_max`.
pub fn native_cal_get_actual_min(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let field = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let min = match field {
        1 => 1,  // YEAR
        3 => 1,  // WEEK_OF_YEAR
        5 => 1,  // DAY_OF_MONTH
        6 => 1,  // DAY_OF_YEAR
        7 => 1,  // DAY_OF_WEEK (SUNDAY)
        8 => -1, // DAY_OF_WEEK_IN_MONTH
        // ERA(0), MONTH(2), WEEK_OF_MONTH(4), AM_PM(9), HOUR(10),
        // HOUR_OF_DAY(11), MINUTE(12), SECOND(13), MILLISECOND(14) and, for
        // this UTC-based synthetic calendar, ZONE_OFFSET(15)/DST_OFFSET(16).
        _ => 0,
    };
    Ok(Some(Value::Int(min)))}

pub fn register_calendar_natives(r: &mut NativeMethodRegistry) {
            let cal = "java/util/Calendar";
    r.register(
        cal,
        "getInstance",
        "()Ljava/util/Calendar;",
        native_cal_get_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cal,
        "getInstance",
        "(Ljava/util/TimeZone;)Ljava/util/Calendar;",
        native_cal_get_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cal,
        "getInstance",
        "(Ljava/util/Locale;)Ljava/util/Calendar;",
        native_cal_get_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(cal, "get", "(I)I", native_cal_get, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "set", "(II)V", native_cal_set_field, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "set", "(III)V", native_cal_set_ymd, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "set", "(IIIII)V", native_cal_set_ymdhms, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "set", "(IIIIII)V", native_cal_set_ymdhms, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "add", "(II)V", native_cal_add, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "getTime", "()Ljava/util/Date;", native_cal_get_time, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        cal,
        "setTime",
        "(Ljava/util/Date;)V",
        native_cal_set_time_from_date, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(cal, "getTimeInMillis", "()J", native_cal_get_time_millis, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "setTimeInMillis", "(J)V", native_cal_set_time_millis, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "before", "(Ljava/lang/Object;)Z", native_cal_before, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "after", "(Ljava/lang/Object;)Z", native_cal_after, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        cal,
        "compareTo",
        "(Ljava/util/Calendar;)I",
        native_cal_compare_to, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(cal, "clone", "()Ljava/lang/Object;", native_cal_clone, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "clear", "()V", native_cal_clear, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "getActualMaximum", "(I)I", native_cal_get_actual_max, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "getActualMinimum", "(I)I", native_cal_get_actual_min, cratonvm_native_api::NativeKind::Intrinsic);
    // W2: `setLenient`/`setFirstDayOfWeek` were no-ops while their getters
    // returned the hardcoded JDK defaults, so a caller that configured the
    // calendar and read the setting back was told its own change had not
    // happened. Both now round-trip through the two slots added at the end of
    // the synthetic layout.
    r.register(cal, "isLenient", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Slot stores "strict" so a zeroed/narrow carrier reads as the JDK
        // default (lenient).
        let strict = cal_get_slot(ctx, this, CAL_FIELD_STRICT);
        Ok(Some(Value::Int(i32::from(strict == 0))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "setLenient", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lenient = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        cal_set_slot(ctx, this, CAL_FIELD_STRICT, i32::from(lenient == 0));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "getFirstDayOfWeek", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // SUNDAY..SATURDAY are 1..7, so an unwritten slot (0) means "never
        // set" and answers the JDK/US default, SUNDAY.
        let d = cal_get_slot(ctx, this, CAL_FIELD_FIRST_DAY_OF_WEEK);
        Ok(Some(Value::Int(if (1..=7).contains(&d) { d } else { 1 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cal, "setFirstDayOfWeek", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        cal_set_slot(ctx, this, CAL_FIELD_FIRST_DAY_OF_WEEK, d);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    let gc = "java/util/GregorianCalendar";
    r.register(gc, "<init>", "()V", native_cal_init_default, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "<init>", "(III)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let year = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1970,
        };
        let month = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let day = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        ctx.set_field(this, CAL_FIELD_YEAR, Value::Int(year));
        ctx.set_field(this, CAL_FIELD_MONTH, Value::Int(month));
        ctx.set_field(this, CAL_FIELD_DAY, Value::Int(day));
        ctx.set_field(this, CAL_FIELD_HOUR, Value::Int(0));
        ctx.set_field(this, CAL_FIELD_MINUTE, Value::Int(0));
        ctx.set_field(this, CAL_FIELD_SECOND, Value::Int(0));
        ctx.set_field(this, CAL_FIELD_MILLIS, Value::Int(0));
        ctx.set_field(this, CAL_FIELD_TIMEZONE, Value::Object(None));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "<init>", "(IIIII)V", native_cal_set_ymdhms, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "<init>", "(IIIIII)V", native_cal_set_ymdhms, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "get", "(I)I", native_cal_get, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "set", "(II)V", native_cal_set_field, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "set", "(III)V", native_cal_set_ymd, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "add", "(II)V", native_cal_add, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "getTime", "()Ljava/util/Date;", native_cal_get_time, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        gc,
        "setTime",
        "(Ljava/util/Date;)V",
        native_cal_set_time_from_date, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(gc, "getTimeInMillis", "()J", native_cal_get_time_millis, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "setTimeInMillis", "(J)V", native_cal_set_time_millis, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "before", "(Ljava/lang/Object;)Z", native_cal_before, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "after", "(Ljava/lang/Object;)Z", native_cal_after, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        gc,
        "compareTo",
        "(Ljava/util/Calendar;)I",
        native_cal_compare_to, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(gc, "clone", "()Ljava/lang/Object;", native_cal_clone, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "clear", "()V", native_cal_clear, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "getActualMaximum", "(I)I", native_cal_get_actual_max, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(gc, "isLeapYear", "(I)Z", native_cal_is_leap_year, cratonvm_native_api::NativeKind::Intrinsic);

    // java.util.Date — 1-field synthetic (millis=0)
    let date = "java/util/Date";
    r.register(date, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "<init>", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Long(millis));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "getTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "setTime", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Long(millis));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "before", "(Ljava/util/Date;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let a = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let b = match ctx.get_field(other, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if a < b { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "after", "(Ljava/util/Date;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let a = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let b = match ctx.get_field(other, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if a > b { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "compareTo", "(Ljava/util/Date;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let a = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let b = match ctx.get_field(other, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(a.cmp(&b) as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let a = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let b = match ctx.get_field(other, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if a == b { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(
            (millis ^ ((millis as u64 >> 32) as i64)) as i32,
        )))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let s = ctx.create_string(&format!("Date({})", millis));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(date, "clone", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let clone = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
        ctx.set_field(clone, 0, Value::Long(millis));
        Ok(Some(Value::Object(Some(clone))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// TimeZone — 2-field synthetic (id=0 String, offset=1 Int millis)
// ---------------------------------------------------------------------------
// synthetic-stub removed: TimeZone now defers to real JDK bytecode; these
// field-index consts are retained but unused.
#[allow(dead_code)]
pub const TZ_FIELD_ID: usize = 0;
#[allow(dead_code)]
pub const TZ_FIELD_OFFSET: usize = 1;
#[allow(dead_code)]
pub const TZ_NUM_FIELDS: usize = 2;

pub fn register_timezone_natives(_r: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java/util/TimeZone has real JDK bytecode (getDefault always-UTC,
    // getTimeZone fixed-offset parser, getAvailableIDs fixed 6-ID list,
    // useDaylightTime/inDaylightTime hardcoded-false were all fakes).
    let _ = _r;
}

// ---------------------------------------------------------------------------
// Currency — fabricated instances mirror all three JDK fields by name.
// ---------------------------------------------------------------------------
pub fn register_currency_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/Currency";
    r.register(
        c,
        "getInstance",
        "(Ljava/lang/String;)Ljava/util/Currency;",
        |ctx, args| {
            let code_str = match args.first() {
                Some(Value::Object(Some(o))) => {
                    ctx.read_string(*o).unwrap_or_else(|| "USD".to_string())
                }
                _ => "USD".to_string(),
            };
            let numeric = match code_str.as_str() {
                "USD" => 840,
                "EUR" => 978,
                "GBP" => 826,
                "JPY" => 392,
                "CHF" => 756,
                "CAD" => 124,
                "AUD" => 36,
                "CNY" => 156,
                _ => 0,
            };
            let cur = try_alloc_concurrent_synthetic(ctx, "java/util/Currency", 3)?;
            // gc-common w20-b: `cur` was held raw across its code string.
            let cur = alloc_string_into_named_field(ctx, cur, "currencyCode", &code_str);
            let digits = match code_str.as_str() {
                "JPY" => 0,
                "BHD" | "KWD" | "OMR" => 3,
                _ => 2,
            };
            ctx.set_field_by_name(cur, "defaultFractionDigits", Value::Int(digits));
            ctx.set_field_by_name(cur, "numericCode", Value::Int(numeric));
            Ok(Some(Value::Object(Some(cur))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "getCurrencyCode", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "currencyCode")))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "getNumericCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "numericCode")))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "getDefaultFractionDigits", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "defaultFractionDigits")))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // W7-80: the real body is
    // `getSymbol(Locale.getDefault(Locale.Category.DISPLAY))`, so this answer is
    // locale-sensitive — `RUB` renders `₽` on a ru host and the bare code `RUB`
    // on an en one. It used to be a locale-INDEPENDENT four-entry table: right
    // for USD/EUR/GBP/JPY in every locale, and the bare ISO code for everything
    // else in every locale. Shares `cldr_currency_symbol` with the 1-arg
    // overload in `locale_resources`, so the two call forms cannot drift.
    r.register(c, "getSymbol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = match ctx.get_field_by_name(this, "currencyCode") {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let sym = crate::locale_resources::currency_symbol_for_default_locale(ctx, &code);
        let s = ctx.create_string(&sym);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "getDisplayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = match ctx.get_field_by_name(this, "currencyCode") {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        // Through the SHARED helper, so this copy and the `(Locale)` overload
        // in `locale_resources` cannot drift — which they already had:
        // this table answered `British Pound Sterling` where HotSpot 25.0.4+7
        // answers `British Pound` (MEASURED, `apps/probes/CurrencyNameProbe`),
        // and it had no entry for CNY/CHF/CAD/AUD at all. Same reasoning the
        // `getSymbol` pair above records for `cldr_currency_symbol`.
        let name = crate::locale_resources::currency_display_name_for_default_locale(ctx, &code);
        let s = ctx.create_string(&name);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "currencyCode")))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// Exchanger — single-slot rendezvous over the REAL field layout.
//
// The real `java.util.concurrent.Exchanger` instance layout is
// arena(0, `[`-descriptor), ncpu(1, I), bound(2, I). The synthetic protocol
// keeps the deposited value in slot 0 (an Object into a `[`/`L` slot passes
// the descriptor-aware coercion) and the state Int in slot 1 (an `I` slot —
// Int lands intact). The previous 3-field protocol ALSO wrote the reply
// Object into slot 2 — but slot 2 is the real `int bound`, so the
// descriptor-aware path mangled the reference into `Int(ptr)` and the
// exchange returned garbage in real-JDK builds. The reply now REUSES slot 0:
//   state 0 (empty):    first thread writes slot0 = its value, state = 1
//   state 1 (waiting):  partner reads slot0, writes slot0 = ITS value, state = 2
//   state 2 (exchanged): first thread reads slot0 as the reply, resets to 0
// ---------------------------------------------------------------------------
pub const EXCH_FIELD_SLOT: usize = 0;
pub const EXCH_FIELD_STATE: usize = 1;

pub fn register_exchanger_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/Exchanger";
    r.register(c, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, EXCH_FIELD_SLOT, Value::Object(None));
        ctx.set_field(this, EXCH_FIELD_STATE, Value::Int(0)); // empty
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // exchange(Object) — blocking, no timeout
    r.register(
        c,
        "exchange",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            exchanger_do_exchange(ctx, this, val, None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // exchange(Object, long, TimeUnit) — with timeout
    r.register(
        c,
        "exchange",
        "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            let timeout_val = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let unit_ordinal = match args.get(3) {
                Some(Value::Object(Some(u))) => crate::time_unit_ordinal(ctx, *u),
                _ => 2,
            };
            let timeout_ms = crate::convert_time_unit_to_millis(timeout_val, unit_ordinal);
            exchanger_do_exchange(ctx, this, val, Some(timeout_ms))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn exchanger_do_exchange(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    my_val: Value,
    timeout_ms: Option<i64>,
) -> MethodCallResult {
    let deadline = timeout_ms
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms.max(0) as u64));

    // GC: this loop BLOCKS in `monitor_wait`, and a blocked thread is exactly
    // where a PEER thread's collection runs. Both the receiver and the value
    // being exchanged are Rust locals that no collection rewrites, and the
    // loop dereferences both on the next turn. That makes this the widest
    // window in the tranche — every other site needs a collection to land in a
    // short call, this one waits for one. Pin both and re-read at the top of
    // each turn. See `internal/audits/wide-tranche-triage-20260907.md`.
    let this_pin = ctx.pin_native_root(this);
    let my_val_pin = match my_val {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };

    loop {
        let this = ctx.read_native_pin(this_pin, this);
        let my_val = match my_val_pin {
            Some((pin, obj)) => Value::Object(Some(ctx.read_native_pin(pin, obj))),
            None => my_val,
        };
        ctx.monitor_enter(this);
        let state = ctx.get_field(this, EXCH_FIELD_STATE).as_int().unwrap_or(0);

        if state == 1 {
            // Another thread is waiting — swap: take its value out of the
            // slot, leave ours as the reply, mark exchanged, wake it.
            let other_val = ctx.get_field(this, EXCH_FIELD_SLOT);
            ctx.set_field(this, EXCH_FIELD_SLOT, my_val);
            ctx.set_field(this, EXCH_FIELD_STATE, Value::Int(2)); // exchanged
            let notify_result = ctx.monitor_notify_all(this);
            ctx.monitor_exit(this);
            notify_result?;
            return Ok(Some(other_val));
        }

        if state == 2 {
            // A previous exchange is still completing (the first thread has
            // not yet collected its reply) — wait for the reset, then retry.
            let wait_result = ctx.monitor_wait(this, Some(5));
            // The wait PARKS, so the exit that pairs with it has to use the
            // post-wait address: `MonitorTable::exit` dereferences the header,
            // and a pre-wait address is a fault (reclaimed) or a permanently
            // leaked monitor (merely moved).
            let this = ctx.read_native_pin(this_pin, this);
            ctx.monitor_exit(this);
            wait_result?;
            continue;
        }

        // state == 0: no partner yet — deposit our value, mark as waiting.
        ctx.set_field(this, EXCH_FIELD_SLOT, my_val);
        ctx.set_field(this, EXCH_FIELD_STATE, Value::Int(1)); // waiting
        ctx.monitor_exit(this);

        // Wait until a partner completes the exchange (state == 2).
        //
        // The re-read at the top of the OUTER loop does not reach here: this
        // inner loop is where the thread actually parks, and it can spin for
        // the whole timeout without the outer body running again. Re-read
        // `this` on every inner turn too -- `monitor_wait` at the foot of the
        // body is the widest collection window in this function.
        loop {
            let this = ctx.read_native_pin(this_pin, this);
            ctx.monitor_enter(this);
            let cur_state = ctx.get_field(this, EXCH_FIELD_STATE).as_int().unwrap_or(0);
            if cur_state == 2 {
                // Partner swapped: the slot now holds ITS value (our reply).
                let other_val = ctx.get_field(this, EXCH_FIELD_SLOT);
                ctx.set_field(this, EXCH_FIELD_STATE, Value::Int(0));
                ctx.set_field(this, EXCH_FIELD_SLOT, Value::Object(None));
                // Wake any third thread parked in the state==2 retry branch.
                let notify_result = ctx.monitor_notify_all(this);
                ctx.monitor_exit(this);
                notify_result?;
                return Ok(Some(other_val));
            }
            if let Some(dl) = deadline {
                if std::time::Instant::now() >= dl {
                    // Timeout — withdraw our deposit and throw.
                    ctx.set_field(this, EXCH_FIELD_STATE, Value::Int(0));
                    ctx.set_field(this, EXCH_FIELD_SLOT, Value::Object(None));
                    ctx.monitor_exit(this);
                    return Err(RuntimeError::IllegalStateException {
                        message: "TimeoutException: Exchanger.exchange timed out".to_string(),
                    }
                    .into());
                }
            }
            let wait_result = ctx.monitor_wait(this, Some(5));
            // Post-wait address, as in the state==2 branch above.
            let this = ctx.read_native_pin(this_pin, this);
            ctx.monitor_exit(this);
            wait_result?;
        }
    }
}

// ---------------------------------------------------------------------------
// Phaser — int[3] holder ([0]=parties, [1]=arrivals, [2]=phase) stored in
// object slot 1.
//
// The real `java.util.concurrent.Phaser` instance layout is state(0, J),
// parent(1, L), root(2, L), evenQ(3, L), oddQ(4, L). The old synthetic Int
// writes to slots 0/1/2 were destroyed by the descriptor-aware coercion:
// slot 0 (J) turned Int(parties) into Long(parties) — which the Int-matching
// reads then saw as 0 — and slots 1/2 (L) nulled the Ints outright. The
// holder lives in slot 1 (`parent`, a reference slot, so the array object
// passes the coercion); slot 0 must NOT be used — an object written to a J
// slot is reinterpreted as Long(ptr). Legacy raw-Int reads (and the
// Long-coerced parties at slot 0) cover synthetic allocations.
// ---------------------------------------------------------------------------
pub const PH_HOLDER_SLOT: usize = 1;
pub const PH_H_PARTIES: usize = 0;
pub const PH_H_ARRIVALS: usize = 1;
pub const PH_H_PHASE: usize = 2;

pub fn ph_holder(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, ObjectRef) {
    if let Value::Object(Some(h)) = ctx.get_field(this, PH_HOLDER_SLOT) {
        return (this, h);
    }
    // Migrate legacy values written before the holder existed: parties may
    // survive at slot 0 as Int (synthetic layout) or Long (real layout — the
    // J-descriptor coercion widened it); arrivals/phase only as raw Ints.
    let legacy_parties = match ctx.get_field(this, 0) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    };
    let legacy_arrivals = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let legacy_phase = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Pin across the allocation (moving-GC receiver-relocation hazard).
    let this_pin = ctx.pin_native_root(this);
    let h = ctx.new_array(cratonvm_types::ArrayElementType::Int, 3);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_array_element(h, PH_H_PARTIES, Value::Int(legacy_parties));
    ctx.set_array_element(h, PH_H_ARRIVALS, Value::Int(legacy_arrivals));
    ctx.set_array_element(h, PH_H_PHASE, Value::Int(legacy_phase));
    ctx.set_field(this, PH_HOLDER_SLOT, Value::Object(Some(h)));
    (this, h)
}

pub fn ph_get(ctx: &mut dyn NativeContext, this: ObjectRef, idx: usize) -> i32 {
    let (_, h) = ph_holder(ctx, this);
    match ctx.get_array_element(h, idx) {
        Value::Int(v) => v,
        _ => 0,
    }
}

pub fn ph_set(ctx: &mut dyn NativeContext, this: ObjectRef, idx: usize, v: i32) {
    let (_, h) = ph_holder(ctx, this);
    ctx.set_array_element(h, idx, Value::Int(v));
}

/// The message JDK 25's `Phaser.badArrive` builds, in its measured shape.
///
/// Measured on OpenJDK 25.0.3:
///
/// ```text
/// Attempted arrival of unregistered party for
/// java.util.concurrent.Phaser@44c03695[phase = 0 parties = 0 arrived = 0]
/// ```
///
/// The real one is `"Attempted arrival of unregistered party for " + this`,
/// i.e. it goes through `Phaser.toString()`. This model registers no
/// `toString`, so the tail is reconstructed from the holder rather than left
/// off — a caller that string-matches the message sees the same text.
pub fn ph_bad_arrive_message(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let phase = ph_get(ctx, this, PH_H_PHASE);
    let parties = ph_get(ctx, this, PH_H_PARTIES);
    let arrived = ph_get(ctx, this, PH_H_ARRIVALS);
    let id = ctx.identity_hash_code(this);
    format!(
        "Attempted arrival of unregistered party for \
         java.util.concurrent.Phaser@{id:x}[phase = {phase} parties = {parties} arrived = {arrived}]"
    )
}

/// SYNTHETIC-MODE ONLY, and tagged accordingly.
///
/// Every triple below is dead under `--fast-boot` and `--real-jdk`: no real
/// `Phaser` method is `ACC_NATIVE`, real `Phaser` declares `Code` for all of
/// them, none is in `force_native_over_real_jdk_bytecode`, and the only call
/// chain that reaches this registrar is
/// `register_phase51_natives` <- `register_synthetic_overrides` <-
/// `register_builtins`, which `vm_init` runs only inside
/// `if config.use_synthetic_jdk`.
///
/// The category was `Intrinsic`, which is a claim this model cannot support —
/// `cratonvm_native_api::NativeKind::Intrinsic` means "returns the same answer the real bytecode
/// would, just faster" (native-api/src/registry.rs). It does not: the
/// int\[3\] holder has no queues, no parent/root tree, no `onAdvance` hook, and
/// eight `Phaser` methods are missing outright (`bulkRegister`, `onAdvance`,
/// `getParent`, `getRoot`, `awaitAdvance`, `awaitAdvanceInterruptibly`,
/// `<init>(Phaser,int)`, `toString`). `SyntheticStub` is byte-identical under
/// `CompatibilityMode::Compatible` — which is what BOTH `--synthetic-jdk` and
/// `--fast-boot` run, since `cratonvm_native_api::NativeKind::allowed_in` returns `true` for every
/// kind there — and it makes `--real-jdk` refuse these at registration instead
/// of letting an approximation win silently.
pub fn register_phaser_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/concurrent/Phaser";
    r.register(c, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, h) = ph_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, PH_H_PARTIES, Value::Int(0));
        ctx.set_array_element(h, PH_H_ARRIVALS, Value::Int(0));
        ctx.set_array_element(h, PH_H_PHASE, Value::Int(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let parties = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let (this, h) = ph_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, PH_H_PARTIES, Value::Int(parties));
        ctx.set_array_element(h, PH_H_ARRIVALS, Value::Int(0));
        ctx.set_array_element(h, PH_H_PHASE, Value::Int(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "register", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, _) = ph_holder(ctx, this);
        ctx.monitor_enter(this);
        let phase = ph_get(ctx, this, PH_H_PHASE);
        // A TERMINATED phaser answers its terminal phase and registers
        // NOTHING. JDK 25's `doRegister` breaks out before it touches the
        // counts (`if ((phase = (int)(s >>> PHASE_SHIFT)) < 0) break;`).
        // Measured on OpenJDK 25.0.3: `new Phaser(1); forceTermination();
        // register()` returns 0x80000000 and `getRegisteredParties()` is
        // still 1. Incrementing here handed a terminated phaser a party that
        // could never arrive.
        if phase < 0 {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(phase)));
        }
        let p = ph_get(ctx, this, PH_H_PARTIES);
        ph_set(ctx, this, PH_H_PARTIES, p + 1);
        ctx.monitor_exit(this);
        Ok(Some(Value::Int(phase)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "arrive", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, _) = ph_holder(ctx, this);
        ctx.monitor_enter(this);
        let arrivals = ph_get(ctx, this, PH_H_ARRIVALS);
        let parties = ph_get(ctx, this, PH_H_PARTIES);
        let phase = ph_get(ctx, this, PH_H_PHASE);
        // JDK 25's `doArrive` asks these two questions, in this order, BEFORE
        // it touches any count.
        //
        // The first is not a nicety: without it, `forceTermination()` then
        // `arrive()` read phase -1, wrote `phase + 1 == 0`, and
        // `isTerminated()` flipped back to FALSE. A terminated phaser must
        // stay terminated.
        if phase < 0 {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(phase)));
        }
        // The second turns a silent no-op into the specified failure.
        // Measured on OpenJDK 25.0.3: `new Phaser(0).arrive()` throws
        // IllegalStateException("Attempted arrival of unregistered party
        // for ...").
        if parties <= 0 {
            let message = ph_bad_arrive_message(ctx, this);
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalStateException { message }.into());
        }
        let new_arrivals = arrivals + 1;
        if new_arrivals >= parties {
            ph_set(ctx, this, PH_H_ARRIVALS, 0);
            ph_set(ctx, this, PH_H_PHASE, phase + 1);
            // Notify all waiting threads that phase has advanced
            ctx.monitor_notify_all(this)?;
        } else {
            ph_set(ctx, this, PH_H_ARRIVALS, new_arrivals);
        }
        ctx.monitor_exit(this);
        Ok(Some(Value::Int(phase)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "arriveAndAwaitAdvance", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, _) = ph_holder(ctx, this);
        ctx.monitor_enter(this);
        let arrivals = ph_get(ctx, this, PH_H_ARRIVALS);
        let parties = ph_get(ctx, this, PH_H_PARTIES);
        let phase = ph_get(ctx, this, PH_H_PHASE);
        // Same two `doArrive` preconditions as `arrive()` above — this method
        // is an arrival too, and without the `phase < 0` guard the `phase + 1`
        // below resurrects a terminated phaser. Measured on OpenJDK 25.0.3:
        // `new Phaser(1); forceTermination(); arriveAndAwaitAdvance()` returns
        // 0x80000000 without blocking.
        if phase < 0 {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(phase)));
        }
        if parties <= 0 {
            let message = ph_bad_arrive_message(ctx, this);
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalStateException { message }.into());
        }
        let new_arrivals = arrivals + 1;
        if new_arrivals >= parties {
            // Last party to arrive — advance phase and notify all waiters
            ph_set(ctx, this, PH_H_ARRIVALS, 0);
            let np = phase + 1;
            ph_set(ctx, this, PH_H_PHASE, np);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(np)))
        } else {
            // Not all parties arrived yet — record arrival and wait for phase to advance
            ph_set(ctx, this, PH_H_ARRIVALS, new_arrivals);
            let target_phase = phase + 1;
            // `monitor_wait` PARKS, which is exactly where a peer thread's
            // collection runs — and `this` is the holder array every `ph_get`
            // and the `monitor_exit` below dereference. Pin it and re-read
            // through the pin AFTER each wait; without that the loop kept
            // reading (and released the monitor at) the pre-wait address.
            //
            // MERGE NOTE (2026-09-08): `origin/dev` fixed this site in the same
            // hour with the re-read at the TOP of the loop instead. That closes
            // the loop-carried half — turn N+1 no longer reads turn N's address
            // — but not the within-turn half: the `monitor_exit` on the error
            // path and the `ph_get` that follows the wait both still ran on the
            // PRE-wait value, which is the address `MonitorTable::exit`
            // dereferences. Resolved to this side because the park window ends
            // at the wait, not at the top of the loop, and because
            // `stale-receiver-audit.py`'s RULE 2 reports the other shape as a
            // site.
            let this_pin = ctx.pin_native_root(this);
            let mut cur = this;
            // Bounded monitor-waits until the phase advances
            loop {
                let wr = ctx.monitor_wait(cur, Some(10));
                cur = ctx.read_native_pin(this_pin, cur);
                if let Err(e) = wr {
                    ctx.monitor_exit(cur);
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
                let current_phase = ph_get(ctx, cur, PH_H_PHASE);
                if current_phase >= target_phase || current_phase < 0 {
                    ctx.monitor_exit(cur);
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Int(current_phase)));
                }
            }
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "arriveAndDeregister", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, _) = ph_holder(ctx, this);
        ctx.monitor_enter(this);
        let parties = ph_get(ctx, this, PH_H_PARTIES);
        let arrivals = ph_get(ctx, this, PH_H_ARRIVALS);
        let phase = ph_get(ctx, this, PH_H_PHASE);
        // `doArrive(ONE_DEREGISTER)` — the same two preconditions.
        if phase < 0 {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(phase)));
        }
        if parties <= 0 {
            let message = ph_bad_arrive_message(ctx, this);
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalStateException { message }.into());
        }
        let new_parties = (parties - 1).max(0);
        ph_set(ctx, this, PH_H_PARTIES, new_parties);
        if new_parties == 0 {
            // The only remaining registered party just arrived, so the phase
            // COMPLETES and only then does the phaser terminate. The terminal
            // phase is `Integer.MIN_VALUE | phase` with that advance already
            // applied — not -1. Measured on OpenJDK 25.0.3:
            // `new Phaser(1).arriveAndDeregister()` leaves `getPhase()` at
            // 0x80000001, and so does `new Phaser(2)` after two of them.
            ph_set(ctx, this, PH_H_ARRIVALS, 0);
            ph_set(ctx, this, PH_H_PHASE, (phase + 1) | i32::MIN);
            ctx.monitor_notify_all(this)?;
        } else if arrivals >= new_parties {
            // `arrivals`, NOT `arrivals + 1`. Deregistering removes the
            // party's registration AND its arrival together: `unarrived` drops
            // by one because `parties` did, so the ARRIVED count does not
            // move. Comparing `arrivals + 1` advanced the phase one arrival
            // early — measured on OpenJDK 25.0.3,
            // `new Phaser(2).arriveAndDeregister()` leaves phase 0 /
            // registered 1 / arrived 0, where this model reported phase 1.
            ph_set(ctx, this, PH_H_ARRIVALS, 0);
            ph_set(ctx, this, PH_H_PHASE, phase + 1);
            ctx.monitor_notify_all(this)?;
        }
        ctx.monitor_exit(this);
        Ok(Some(Value::Int(phase)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getPhase", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ph_get(ctx, this, PH_H_PHASE))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getRegisteredParties", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ph_get(ctx, this, PH_H_PARTIES))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getArrivedParties", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ph_get(ctx, this, PH_H_ARRIVALS))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "getUnarrivedParties", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let p = ph_get(ctx, this, PH_H_PARTIES);
        let a = ph_get(ctx, this, PH_H_ARRIVALS);
        Ok(Some(Value::Int((p - a).max(0))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let phase = ph_get(ctx, this, PH_H_PHASE);
        Ok(Some(Value::Int(if phase < 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "forceTermination", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (this, _) = ph_holder(ctx, this);
        ctx.monitor_enter(this);
        // `Integer.MIN_VALUE | phase`, NOT -1: the phase NUMBER survives
        // termination and `getPhase()` must still report it. Measured on
        // OpenJDK 25.0.3 — forcing at phase 0 gives 0x80000000, at phase 2
        // gives 0x80000002, and the party counts are PRESERVED (a
        // `new Phaser(3)` still reports 3 registered / 3 unarrived after).
        // -1 is 0xFFFFFFFF, which reads as "terminated at phase 0x7FFFFFFF".
        // The `>= 0` test keeps a second call idempotent.
        let phase = ph_get(ctx, this, PH_H_PHASE);
        if phase >= 0 {
            ph_set(ctx, this, PH_H_PHASE, phase | i32::MIN);
        }
        let notify_result = ctx.monitor_notify_all(this);
        ctx.monitor_exit(this);
        notify_result?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

// ---------------------------------------------------------------------------
// Timer / TimerTask — stubs
// ---------------------------------------------------------------------------
pub fn register_timer_natives(_r: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java/util/Timer + java/util/TimerTask have real JDK bytecode. The fake
    // schedule/scheduleAtFixedRate were NO-OPS (tasks never ran) and the
    // ctors/cancel/purge only maintained synthetic fields for those no-ops.
    // Real Timer spins a TimerThread off the real ThreadPrimitives, so tasks
    // actually fire.
    let _ = _r;
}

// ---------------------------------------------------------------------------
// ForkJoinPool, ForkJoinTask, RecursiveTask, RecursiveAction — stubs
// ---------------------------------------------------------------------------

// WP4.3 fix — side-table for ForkJoinTask "done + result" state, keyed by
// the task's ObjectRef pointer.
//
// Why this exists: in synthetic-JDK mode, ForkJoinTask/RecursiveTask were
// allocated with two synthetic fields (idx 0 = result, idx 1 = done). The
// fork()/invoke()/join() natives below used those slots to memoise the
// result so that a subsequent join() would not re-run compute(). In
// real-JDK mode, however, the actual loaded ForkJoinTask class has its
// own field layout (volatile int status, Object aux, etc.) and field
// index 1 is NOT "done" — `ctx.get_field(this, 1).as_int()` returns
// `None`/0 every time, causing fork()/join()/invoke() to ALL re-invoke
// compute() on the same task.
//
// For FjpProbe (1M long[]→sum, threshold 1000, depth 10) this re-invocation
// makes total compute() calls grow as O(2^depth × constant) instead of
// O(N/threshold + 2^depth), and the probe never terminates.
//
// The side-table sidesteps the problem by storing the done flag and the
// computed result keyed by the task's `ObjectRef.as_ptr()`, independent of
// the class's field layout. Both synthetic-JDK and real-JDK modes hit the
// same path. Memory is bounded by the number of live ForkJoinTask
// instances; tasks are reaped on a best-effort basis when their entry has
// been observed `done==true` and not consulted for >256 calls (see
// `fjp_state_reap`).
/// Parallelism of `ForkJoinPool.commonPool()`.
///
/// Not a placeholder: the common pool handed out below starts no workers and
/// runs every task inline on the submitting thread, so it has exactly one.
/// Kept as a single constant so the pool's parallelism slot and
/// `getCommonPoolParallelism()` cannot drift apart.
pub const FJP_COMMON_PARALLELISM: i32 = 1;

pub fn fjp_state() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<usize, FjpEntry>> {
    static STATE: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<usize, FjpEntry>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

#[derive(Clone, Copy, Debug)]
pub struct FjpEntry {
    pub done: bool,
    /// Set for the duration of one thread's claim on this task's body —
    /// between [`fjp_state_claim`] handing out [`FjpClaim::Claimed`] and the
    /// matching completion write (`fjp_state_set_done` /
    /// `fjp_state_set_thrown` / `fjp_state_set_done_preserving_thrown` /
    /// `fjp_state_release_claim`). Closes `lane-concurrency-forkjoin
    /// -scheduler-20260917.md` item 1: every entry point used to read `done`
    /// under its own lock acquisition, release the lock, run the body, and
    /// only THEN acquire the lock again to record completion — a
    /// check-then-act window with nothing claimed in it. Two threads that
    /// both read `done == false` in that window both ran the body
    /// (`L5FjDouble`: `threads=2 maxDepth=1 CONCURRENT-two-threads`, 1-3 of
    /// 20 shapes under an armed `ForkJoinPool`). `running` is never read by
    /// bytecode — it lives only in this side table — so closing the race
    /// this way needs no change to `ForkJoinTask.status` or to what
    /// `isDone()` promises.
    pub running: bool,
    /// Which thread holds the claim while `running` is set
    /// ([`fjp_current_thread_tag`]); meaningless otherwise. Lets
    /// [`fjp_state_claim`] tell a SECOND thread's collision (block, like a real
    /// `awaitDone()`) from the SAME thread re-entering a task it is already
    /// running (never block: nothing else can release the claim).
    pub owner: u64,
    /// W2: real cancellation state. `cancel(Z)Z` used to be a constant
    /// `false` on all six of ForkJoinTask/RecursiveTask/RecursiveAction's
    /// registrations, so a caller was told cancellation had failed while
    /// `isCancelled()` also said `false` and the task went on to run —
    /// callers that poll `isCancelled()` after a `cancel()` spun forever.
    /// A cancelled task is `done && cancelled`, which is exactly the real
    /// `ForkJoinTask` invariant (`isDone()` is true for a cancelled task and
    /// `isCompletedNormally()` is false).
    pub cancelled: bool,
    pub result: Value,
    /// The `Throwable` this task completed exceptionally with, or
    /// `Value::Object(None)` for a task that has not completed abnormally.
    ///
    /// Every task this bridge runs executes INLINE on the submitting thread,
    /// so a `Callable.call()` / `compute()` that throws surfaces here as an
    /// `Err(MethodCallFailed::ExceptionThrown)` from `invoke_virtual` — with
    /// nowhere to put it, the eager-inline natives used to discard it and
    /// report a null result, so `Future.get()` handed back null instead of
    /// throwing and `isCompletedNormally()` claimed success for a task that
    /// had blown up. Weld's `AbstractExecutorServices.checkForExceptions`
    /// (which calls `Future.get()` on every `invokeAll` result and rethrows
    /// the `ExecutionException`'s cause) is the reason this has to be
    /// modelled rather than approximated: without it a failed CDI bean
    /// deployment reports success and the container boots half-built.
    ///
    /// Rooted and remapped by `gc_scan_forkjoin_roots` /
    /// `gc_update_forkjoin_refs` exactly like `result`.
    pub thrown: Value,
    /// The VM whose task (or pool) this is: `ctx.vm_identity()` of the native
    /// that created or last touched the entry through [`fjp_entry_in`].
    /// [`FJP_UNSCOPED_VM`] (0, also the mock contexts' identity) for an entry
    /// only a VM-less writer has touched so far -- such an entry is scanned
    /// and remapped by every VM, as every entry used to be.
    ///
    /// gc-common w9-b (`common-w8a-forkjoin-and-tls-root-rows-stay-process-global`):
    /// the `forkjoin` root row walks only the collecting VM's entries (plus
    /// unscoped ones), so VM B's collector is no longer handed VM A's tasks,
    /// results and throwables, and VM B's pointer map no longer rekeys them.
    pub vm: usize,
}

/// [`FjpEntry::vm`] of an entry no VM-bearing writer has touched yet.
pub const FJP_UNSCOPED_VM: usize = 0;

impl FjpEntry {
    pub fn new(vm: usize) -> Self {
        Self {
            done: false,
            running: false,
            owner: 0,
            cancelled: false,
            result: Value::Object(None),
            thrown: Value::Object(None),
            vm,
        }
    }

    /// Whether VM `vm`'s collection owns this entry: its own, or unscoped.
    #[inline]
    pub fn owned_by(&self, vm: usize) -> bool {
        self.vm == vm || self.vm == FJP_UNSCOPED_VM
    }
}

/// `o`'s entry, created for `vm` if absent -- the ONE way an entry is
/// created, so every entry carries an owner (gc-common w9-b).
///
/// * An UNSCOPED entry (created by a VM-less writer, see
///   [`fjp_state_mark_queued_in_vm`] given [`FJP_UNSCOPED_VM`]) is adopted by the first VM-bearing touch.
/// * An entry stamped with ANOTHER VM is stale: the heaps of two live VMs are
///   disjoint, so the address was reused after that VM's teardown without
///   [`forget_vm_forkjoin_state`]. It is replaced by a fresh entry rather than
///   re-stamped, or the new task would inherit the dead VM's done/result.
/// * A VM-less caller (`vm == FJP_UNSCOPED_VM`) never changes the stamp.
pub fn fjp_entry_in(
    m: &mut rustc_hash::FxHashMap<usize, FjpEntry>,
    vm: usize,
    o: ObjectRef,
) -> &mut FjpEntry {
    let e = m.entry(fjp_key(o)).or_insert_with(|| FjpEntry::new(vm));
    if vm != FJP_UNSCOPED_VM && e.vm != vm {
        if e.vm == FJP_UNSCOPED_VM {
            e.vm = vm;
        } else {
            *e = FjpEntry::new(vm);
        }
    }
    e
}

/// Signalled whenever a claim is released — either by a completion write
/// ([`fjp_state_set_done`] and siblings) or by [`fjp_state_release_claim`]
/// after an internal VM error left the task unfinished. One condvar shared
/// by every task: waiters recheck their own key on each wakeup, so a
/// spurious cross-task wakeup just costs a re-lock, not a wrong answer.
pub fn fjp_done_condvar() -> &'static parking_lot::Condvar {
    static CV: std::sync::OnceLock<parking_lot::Condvar> = std::sync::OnceLock::new();
    CV.get_or_init(parking_lot::Condvar::new)
}

/// What [`fjp_state_claim`] found.
pub enum FjpClaim {
    /// The task was already done — by a normal completion, an abnormal one,
    /// or a `cancel()` — before this call ever got a chance to run the body.
    /// The `Result` is exactly what a fresh `fjp_compute_and_complete` would
    /// have returned for the same state, so every caller's existing
    /// post-processing (raw rethrow for `join()`, `ExecutionException`
    /// wrapping for `get()`, swallow for `quietly*`) applies unchanged.
    Done(Result<Value, MethodCallFailed>),
    /// This call won the race and must run the body, then call one of the
    /// completion functions (or, on an internal VM error that is not a task
    /// outcome, [`fjp_state_release_claim`]) exactly once.
    Claimed,
    /// The CALLING thread already holds this task's claim: it is being re-entered
    /// from inside its own `compute()`. The caller must run the body without
    /// claiming or completing -- the outer frame still owns both. See
    /// [`fjp_state_claim`].
    Reentrant,
}

/// A small per-thread tag for [`FjpEntry::owner`]. Never 0 (0 = unowned).
pub fn fjp_current_thread_tag() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    thread_local! {
        static TAG: u64 = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    TAG.with(|t| *t)
}

/// Build the `Result` a caller would see for an already-`done` entry —
/// `fjp_complete_from_outcome`'s three outcomes, read back from the table
/// instead of freshly produced.
pub fn fjp_outcome_from_entry(e: &FjpEntry) -> Result<Value, MethodCallFailed> {
    if e.cancelled {
        return Err(RuntimeError::CancellationException {
            message: String::new(),
        }
        .into());
    }
    if let Value::Object(Some(t)) = e.thrown {
        return Err(MethodCallFailed::ExceptionThrown(t));
    }
    Ok(e.result)
}

/// Atomically claim `task` for execution, or report that it is already done.
///
/// Returns the (possibly GC-relocated) task alongside the claim outcome.
/// Every caller of [`fjp_compute_and_complete`] / [`fjp_compute_void`] used
/// to make this decision itself with a lock acquisition that had already
/// been released by the time it acted on the answer — see the comment on
/// [`FjpEntry::running`]. This is the one place that check now happens, and
/// it happens under a single critical section per attempt.
///
/// The fast, uncontended path (shipped `--real-jdk`: 0/20 unarmed, always)
/// takes one lock and returns — no pin, no blocking-region bracket, the same
/// cost the old racy check already had. Only a genuine collision — another
/// thread's claim still in flight, which needs a real multi-worker
/// `ForkJoinPool` to happen at all — pins the task and blocks like a real
/// `awaitDone()` would, bracketed by `begin_blocking_region`/
/// `end_blocking_region` so a concurrent STW pause does not wait on a thread
/// that cannot reach a safepoint (the same hazard `rw_read_lock`'s callers in
/// `util_concurrent_ext.rs` are bracketed against).
///
/// # Re-entry by the claiming thread
///
/// A task can fork ITSELF from inside its own `compute()`.
/// `java.util.stream.ForEachOps$ForEachTask.compute` does it on its second
/// split (`taskToFork = task` while `task` is still `this`), and every real
/// parallel `forEach` reaches it. Under the eager `fork()` (`fjt_fork_run_now`)
/// that is a `fork()` of a task the same thread has claimed and is running, so
/// waiting for the claim to be released waits for a release only this thread
/// can perform: a self-deadlock at the very first parallel `forEach` of more
/// than a leaf's worth of elements. MEASURED 2026-09-18 under `--real-jdk`,
/// where real stream bytecode runs (`IntStream.range(0, 1000).parallel()
/// .forEach(..)`): all 1000 elements were processed, then the main thread
/// parked in `fork()` for good. It is what turned `Groovy` class initialisation
/// (`DgmMethodRecord.loadDgmInfo`'s parallel `forEach`) into a hang for every
/// spring-test class that reached `ClassUtils.isPresent("groovy.lang.Closure")`.
/// That case answers [`FjpClaim::Reentrant`]; a genuinely concurrent collision
/// still blocks.
pub fn fjp_state_claim(ctx: &mut dyn NativeContext, o: ObjectRef) -> (ObjectRef, FjpClaim) {
    let me = fjp_current_thread_tag();
    let vm = ctx.vm_identity();
    {
        let mut m = fjp_state().lock();
        let e = fjp_entry_in(&mut m, vm, o);
        if e.done {
            return (o, FjpClaim::Done(fjp_outcome_from_entry(e)));
        }
        if !e.running {
            e.running = true;
            e.owner = me;
            return (o, FjpClaim::Claimed);
        }
        if e.owner == me {
            return (o, FjpClaim::Reentrant);
        }
    }
    let pin = ctx.pin_native_root(o);
    let mut live = ctx.read_native_pin(pin, o);
    // Interpreter round i2 wave 66, lane L1: the JDK body this stands for, a
    // `join()` / `get()` / `invoke()` of a task another thread runs, waits in
    // `ForkJoinTask.awaitDone`, which parks: `Parked` (WAITING and JVMTI
    // `PARKED`, as the unrecorded region already read; now it says so).
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
    let result = loop {
        let mut m = fjp_state().lock();
        {
            let e = fjp_entry_in(&mut m, vm, live);
            if e.done {
                break FjpClaim::Done(fjp_outcome_from_entry(e));
            }
            if !e.running {
                e.running = true;
                e.owner = me;
                break FjpClaim::Claimed;
            }
            if e.owner == me {
                break FjpClaim::Reentrant;
            }
        }
        fjp_done_condvar().wait(&mut m);
        drop(m);
        live = ctx.read_native_pin(pin, live);
    };
    ctx.end_blocking_region();
    ctx.unpin_native_roots(pin);
    (live, result)
}

/// Release a claim on `o` WITHOUT marking it done — the exit for an internal
/// VM error, which [`fjp_complete_from_outcome`]'s `Err(other)` arm cannot
/// treat as a task outcome. Leaves the task exactly as re-triable as it was
/// before claims existed: `done` stays `false`, so the next `join()`/`get()`/
/// `invoke()` claims and runs it again.
pub fn fjp_state_release_claim(o: ObjectRef) {
    {
        let mut m = fjp_state().lock();
        if let Some(e) = m.get_mut(&fjp_key(o)) {
            e.running = false;
        }
    }
    fjp_done_condvar().notify_all();
}

#[inline]
pub fn fjp_key(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// The bits `ForkJoinTask`'s write-once `status` word carries, verbatim from
/// `javap -p java.util.concurrent.ForkJoinTask` on 17, 21 and 25 — the three
/// images agree on all three values.
///
/// `DONE` is `1 << 31`, which is why every reader in the JDK spells the test
/// `status < 0` rather than a mask.
pub const FJT_STATUS_DONE: i32 = 1 << 31;
pub const FJT_STATUS_ABNORMAL: i32 = 1 << 16;
pub const FJT_STATUS_THROWN: i32 = 1 << 17;

/// Stamp the REAL `ForkJoinTask.status` word to agree with what the side table
/// has just recorded.
///
/// # Why a side table needed this and why the field is reachable now
///
/// The comment on [`fjp_state`] explains the side table: under a real layout
/// `ctx.get_field(this, 1)` is not `done`, so fork/join/invoke all re-invoked
/// `compute()`. That is an argument against reaching the field BY INDEX and it
/// was silent about reaching it by NAME — `get_field_by_name` resolves `status`
/// on the real class, and `L5FjStatus.java` reads the same field
/// back through core reflection to prove the write landed rather than assuming
/// it did.
///
/// # What this does and does not fix
///
/// It makes a COMPLETED task look completed to the JDK's own model, so the
/// real `doExec()` — whose first act is `if ((s = status) >= 0)` — declines to
/// run a body this VM has already run, and so bytecode that reads `isDone()` /
/// `isCompletedAbnormally()` / `getException()` without passing through a
/// native agrees with the natives that do.
///
/// It is NOT a claim, and was not meant to be one — this function only
/// stamps a *completed* task's real status word after the fact, so a
/// concurrent `doExec()` that has not yet checked `status` still declines to
/// re-run a task this VM already finished (the check races the write, not
/// the run). §3a's race — two runners that both start before either
/// finishes — needed a claim BEFORE the body runs, and stamping `DONE` early
/// would have been a change to `isDone()`'s contract (true for a task still
/// executing) rather than a fix. `FjpEntry::running` /
/// [`fjp_state_claim`] is that claim instead: it lives only in this
/// bytecode-invisible side table, so it closes the race in
/// `lane-concurrency-forkjoin-scheduler-20260917.md` item 1 without touching
/// what `isDone()` promises at all.
///
/// Never clears a bit. The real word is write-once per bit (`setDone()` is a
/// bitwise OR, `trySetCancelled` stamps `DONE|ABNORMAL`), and a VM that could
/// clear `ABNORMAL` would manufacture exactly the fabricated success W6-9
/// removed from `fjp_state_set_done`.
///
/// Silent on a receiver with no such field: the synthetic-JDK carrier does not
/// declare `status`, and `set_field_by_name` drops a store whose field the
/// class does not have. There is nothing to agree with there — the side table
/// is the only model in that mode.
pub fn fjp_stamp_real_status(ctx: &mut dyn NativeContext, task: ObjectRef, bits: i32) {
    let current = match ctx.get_field_by_name(task, "status") {
        Value::Int(v) => v,
        // No `status` field, or one that is not an int: the synthetic carrier.
        _ => return,
    };
    if current & bits == bits {
        return;
    }
    ctx.set_field_by_name(task, "status", Value::Int(current | bits));
}

/// `fjp_state_cancel` plus the matching stamp, for the six `cancel(Z)Z`
/// registrations that all do the same two things.
///
/// The real `cancel` is `trySetCancelled()`, which ORs `DONE | ABNORMAL`.
/// `THROWN` is deliberately absent: a cancelled task has no throwable, and
/// `getException()` on one answers a `CancellationException` the JDK
/// manufactures from `ABNORMAL` without `THROWN`.
pub fn fjp_cancel_and_stamp(ctx: &mut dyn NativeContext, task: ObjectRef) -> bool {
    let cancelled = fjp_state_cancel(ctx.vm_identity(), task);
    if cancelled {
        fjp_stamp_real_status(ctx, task, FJT_STATUS_DONE | FJT_STATUS_ABNORMAL);
    }
    cancelled
}

/// Returns `(done, cached_result)` for the given task. If the task has
/// never been seen, returns `(false, Value::Object(None))`.
pub fn fjp_state_get(o: ObjectRef) -> (bool, Value) {
    let m = fjp_state().lock();
    match m.get(&fjp_key(o)) {
        Some(e) => (e.done, e.result),
        None => (false, Value::Object(None)),
    }
}

/// Record a raw result against the task and mark it done — i.e. the real
/// `complete(V)`, which is `setRawResult(v); setDone();` and nothing else.
///
/// W6-9: this used to ALSO do `e.thrown = Value::Object(None)`, on the
/// reasoning that "the task produced a value" and "the task threw" are mutually
/// exclusive in the real status word. They are not, and the status word is the
/// proof. Verified against `javap -c java.util.concurrent.ForkJoinTask` on JDK
/// 25:
///
/// ```text
/// private void setDone() { getAndBitwiseOrStatus(DONE); signalWaiters(); }
/// ```
///
/// `setDone()` is a pure bitwise OR into a write-once status word (`DONE` =
/// `1<<31`), so it cannot clear `ABNORMAL` (`1<<16`) or `THROWN` (`1<<17`) —
/// the bits `trySetCancelled` (`|= DONE|ABNORMAL`) and `trySetThrown`
/// (`|= DONE|ABNORMAL|THROWN`) stamp. So on a real JDK,
/// `t.completeExceptionally(ex); t.complete(v);` leaves `t` ABNORMAL with `ex`
/// still readable through `getException()`.
///
/// Clearing `thrown` here flipped exactly that task to "completed normally":
/// `isCompletedAbnormally()` answered `false`, `getException()` answered null,
/// and `join()`/`get()` handed back `v` instead of raising — a fabricated
/// success for a task the caller had explicitly failed. Nothing clears `thrown`
/// any more. The one real API that DOES reset a task's status is
/// `reinitialize()` (`aux = null; status &= 1<<24`), which as of W6-9 §7.2 IS
/// registered — by `register_forkjointask_w6_9_residual_bridge`, and it drops
/// the side-table entry rather than reaching through this function. No caller
/// here wants the reset.
///
/// A cancelled task stays cancelled and keeps its null result: `cancel()`
/// already completed it, and the real `setDone()` likewise cannot clear
/// `CANCELLED` once `trySetCancelled` has stamped it. The real `setRawResult`
/// inside `complete` IS still executed for a cancelled task; that half is
/// [`fjp_state_set_raw_result`], which `fjp_complete_body` now calls first, so
/// a bare `getRawResult()` no longer diverges (W6-9 §7.4).
/// The `setRawResult(v)` half of `complete(V)`, for a receiver whose
/// raw-result slot IS the side table.
///
/// Split out from [`fjp_state_set_done`] because the two halves have different
/// write-once rules: `setDone()` ORs into a status word that `trySetCancelled`
/// has already stamped, so it is suppressed on a cancelled task — but
/// `setRawResult` is an ordinary virtual call that the real `complete(V)` runs
/// before it, cancelled or not.
pub fn fjp_state_set_raw_result(vm: usize, o: ObjectRef, result: Value) {
    let mut m = fjp_state().lock();
    fjp_entry_in(&mut m, vm, o).result = result;
}

/// Bounded reap for the completion paths: once the table passes 4096
/// entries, drop up to 2048 DONE entries that `vm`'s collection owns. It used
/// to drop any VM's done entries, so VM B's completions could forget VM A's
/// memoised results (gc-common w9-b). A VM-less caller
/// (`FJP_UNSCOPED_VM`) still reaps any done entry, as before: it cannot tell
/// whose entries it is completing, and a narrower reap would let a
/// `quietlyComplete()`-only workload grow the table without bound.
pub fn fjp_reap_done(m: &mut rustc_hash::FxHashMap<usize, FjpEntry>, vm: usize) {
    if m.len() > 4096 {
        let drained: Vec<usize> = m
            .iter()
            .filter_map(|(k, v)| {
                if v.done && (vm == FJP_UNSCOPED_VM || v.owned_by(vm)) {
                    Some(*k)
                } else {
                    None
                }
            })
            .take(2048)
            .collect();
        for k in drained {
            m.remove(&k);
        }
    }
}

pub fn fjp_state_set_done(vm: usize, o: ObjectRef, result: Value) {
    {
        let mut m = fjp_state().lock();
        {
            let e = fjp_entry_in(&mut m, vm, o);
            if !e.cancelled {
                e.done = true;
                e.result = result;
            }
            // Release the claim regardless of `cancelled`: a claimant that
            // raced a concurrent `cancel()` still finished its body and must
            // still let the next `fjp_state_claim` proceed. See
            // `FjpEntry::running`.
            e.running = false;
        }
        // Best-effort reap: if the table has grown beyond 4096 entries, drop
        // the oldest "done" half. With 1M-element FjpProbe at threshold 1000
        // we expect <2048 live tasks so this is rarely hit; it just bounds
        // memory under pathological recursion.
        fjp_reap_done(&mut m, vm);
    }
    // Outside the lock: a waiter woken here re-locks immediately to recheck
    // its own key, so notifying while still holding `m` would just wake it
    // into contention on the lock this thread has not released yet.
    fjp_done_condvar().notify_all();
}

/// `ForkJoinTask.setDone()` — OR `DONE` into the status and touch NOTHING ELSE.
///
/// This is deliberately NOT [`fjp_state_set_done`], and the two are not
/// interchangeable. The real `setDone()` ORs one bit into a write-once status
/// word: an already-`ABNORMAL` task stays abnormal, and the raw-result slot is
/// not written (`complete(T)` writes it separately, through `setRawResult`).
/// `fjp_state_set_done` models the OTHER completion — `complete(V)`, "the task
/// produced this value" — so it also overwrites `result`.
///
/// Calling `fjp_state_set_done(this, Value::Object(None))` from
/// `quietlyComplete()` would therefore null out a raw result a
/// `CountedCompleter` had already stashed via `setRawResult` before calling
/// `tryComplete()` — `java.util.stream.AbstractTask.compute()` is exactly
/// `setLocalResult(doLeaf()); tryComplete();`, and `tryComplete()` ends in
/// `quietlyComplete()`.
///
/// (W6-9: it would ALSO have erased the abnormal record, because
/// `fjp_state_set_done` used to clear `thrown`. It no longer does — that was
/// its own defect, reached through the four `complete(Ljava/lang/Object;)V`
/// registrations — so the erasure is no longer a reason to prefer this
/// function. The raw-result clobber still is.)
///
/// A cancelled entry is already `done`, so this is a no-op there — matching the
/// write-once status word that `trySetCancelled` has already stamped.
///
/// Takes the VM (gc-common w9-b): the entry it creates or touches is stamped
/// with `vm`, so only that VM's collection roots it.
pub fn fjp_state_set_done_preserving_thrown_in_vm(vm: usize, o: ObjectRef) {
    {
        let mut m = fjp_state().lock();
        {
            let e = fjp_entry_in(&mut m, vm, o);
            e.done = true;
            e.running = false;
        }
        // Same bounded reap as `fjp_state_set_done`: this is a completion path too,
        // so a `quietlyComplete()`-only workload must not grow the table forever.
        fjp_reap_done(&mut m, vm);
    }
    fjp_done_condvar().notify_all();
}

/// Complete the task ABNORMALLY with `thrown`.
///
/// Mirrors the real `ForkJoinTask.trySetThrown`: the task becomes DONE, has no
/// result, and every later `join()`/`get()`/`invoke()` replays the throwable
/// instead of handing back a value. A task that was already cancelled keeps
/// its cancellation (the real status word is likewise write-once).
pub fn fjp_state_set_thrown(vm: usize, o: ObjectRef, thrown: ObjectRef) {
    {
        let mut m = fjp_state().lock();
        let e = fjp_entry_in(&mut m, vm, o);
        if !e.cancelled {
            e.done = true;
            e.result = Value::Object(None);
            e.thrown = Value::Object(Some(thrown));
        }
        e.running = false;
    }
    fjp_done_condvar().notify_all();
}

/// The `Throwable` a task completed abnormally with, if any.
pub fn fjp_state_thrown(o: ObjectRef) -> Option<ObjectRef> {
    match fjp_state().lock().get(&fjp_key(o)) {
        Some(FjpEntry {
            thrown: Value::Object(Some(t)),
            ..
        }) => Some(*t),
        _ => None,
    }
}

/// `ForkJoinTask.cancel(mayInterruptIfRunning)`.
///
/// Mirrors the real `ForkJoinTask.cancel` = `trySetCancelled()`: a task that
/// has not completed becomes DONE+ABNORMAL and the call reports `true`; a task
/// that already completed normally cannot be cancelled and reports `false`; a
/// second `cancel()` on an already-cancelled task still reports `true`.
///
/// `mayInterruptIfRunning` is ignored, exactly as the real implementation does
/// — ForkJoinTask never interrupts a running worker.
pub fn fjp_state_cancel(vm: usize, o: ObjectRef) -> bool {
    let cancelled_now = {
        let mut m = fjp_state().lock();
        let e = fjp_entry_in(&mut m, vm, o);
        if e.done {
            return e.cancelled;
        }
        e.done = true;
        e.cancelled = true;
        e.result = Value::Object(None);
        true
    };
    // `cancel()` does not stop an in-flight `running` claim — real
    // ForkJoinTask.cancel() never interrupts a running worker either — but a
    // thread blocked in `fjp_state_claim` waiting on THIS task must see
    // `done` right away rather than only once the runner it is shadowing
    // eventually completes and notifies on its own account.
    fjp_done_condvar().notify_all();
    cancelled_now
}

/// Record a `fork()`ed task as submitted-but-not-yet-run.
///
/// `fork()` here is lazy: it queues the task and returns, and the work happens
/// in the matching `join()`/`get()`/`invoke()`. Entering the task in the side
/// table with `done == false` is what makes `getQueuedTaskCount()` answerable.
/// It changes nothing else — every reader already treats a missing key and a
/// `done == false` entry identically — and it is GC-safe, because
/// `gc_scan_forkjoin_roots`/`gc_update_forkjoin_refs` root and rekey every
/// entry the collecting VM owns (and every unscoped one).
pub fn fjp_state_mark_queued_in_vm(vm: usize, o: ObjectRef) {
    let mut m = fjp_state().lock();
    fjp_entry_in(&mut m, vm, o);
}

/// Tasks forked but not yet completed — the pool's real queue depth.
///
/// The count is over all tasks rather than per-pool: `commonPool()` mints a
/// fresh carrier on every call and a task is never associated with a pool in
/// this model, so there is no per-pool partition to report. `getQueuedTask
/// Count()` is documented as an estimate.
///
/// Over THIS VM's tasks (and unscoped ones) since gc-common w9-b: it counted
/// every VM's tasks in the process.
pub fn fjp_queued_task_count(vm: usize) -> i64 {
    fjp_state()
        .lock()
        .values()
        .filter(|e| !e.done && e.owned_by(vm))
        .count() as i64
}

/// `(done, cancelled)` for the given task; `(false, false)` if never seen.
pub fn fjp_state_flags(o: ObjectRef) -> (bool, bool) {
    let m = fjp_state().lock();
    match m.get(&fjp_key(o)) {
        Some(e) => (e.done, e.cancelled),
        None => (false, false),
    }
}

/// `fjp_state_get` plus the spec'd `CancellationException` on a cancelled
/// task. Used by `join()` / `get()` / `invoke()`, which the real JDK makes
/// throw rather than hand back a value; `isDone()`/`isCancelled()`/
/// `getRawResult()` deliberately keep the plain `fjp_state_get`.
///
/// It raises a REAL `java.util.concurrent.CancellationException`. It used to
/// raise `IllegalStateException` with the spec'd type name in its message,
/// because `RuntimeError` had no variant for the real class — and a proxy is
/// only as good as its message: `catch (CancellationException)` is the one way
/// a caller tells a cancelled task from a failed one, and it did not fire.
/// MEASURED, both modes, three rows (`join`, `get`, `invoke` on a cancelled
/// task). The variant exists now; see `RuntimeError::CancellationException`.
pub fn fjp_state_get_checked(o: ObjectRef) -> Result<(bool, Value), MethodCallFailed> {
    let m = fjp_state().lock();
    match m.get(&fjp_key(o)) {
        Some(e) if e.cancelled => Err(RuntimeError::CancellationException {
            message: String::new(),
        }
        .into()),
        Some(e) => Ok((e.done, e.result)),
        None => Ok((false, Value::Object(None))),
    }
}

pub fn fjp_remap_value_ref(value: &mut Value, pointer_map: &cratonvm_types::PointerMap) {
    if let Value::Object(Some(obj)) = value {
        let old_addr = obj.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}

/// The one `ForkJoinPool.commonPool()` carrier, as a raw address.
///
/// `commonPool()` minted a FRESH object on every call. The registration's own
/// comment had said so since it was written -- "TODO: this allocates a FRESH
/// pool object on every call instead of caching a true singleton (unlike the
/// real JDK, where commonPool() always returns the same instance)" -- and the
/// differential finally asked: `ForkJoinPool.commonPool() ==
/// ForkJoinPool.commonPool()` is `true` on HotSpot 25.0.4+7 and was `false`
/// here, in both modes. Any code that keys a map, a registry or a
/// shutdown-once flag on the pool identity saw a different pool every time it
/// looked.
///
/// ONE PER VM since gc-common w9-b
/// (`common-w8a-forkjoin-and-tls-root-rows-stay-process-global`). It was one
/// `AtomicUsize` for the process, so VM B's `commonPool()` answered the first
/// pool minted in VM A's heap: B read and wrote A's fields, B's collector was
/// handed it as a root, and after A's teardown it dangled. The `commonPool()`
/// native reads and publishes through [`fjp_common_pool_get_in_vm`] /
/// [`fjp_common_pool_set_in_vm`]; there is no process-wide row. A row holds a
/// raw address (0 = none); this file's GC hooks own its rooting and remap.
pub static FJP_COMMON_POOL: cratonvm_native_api::vm_scoped::VmScoped<usize> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

thread_local! {
    /// The `ForkJoinPool` carriers whose submission natives are on THIS
    /// thread's stack right now, innermost last, each with the pin handle that
    /// keeps it addressable.
    ///
    /// This pool runs every task INLINE on the submitting thread, so the JDK's
    /// own answer to "am I in a pool?" — `Thread.currentThread() instanceof
    /// ForkJoinWorkerThread` — is false even in the middle of
    /// `pool.invoke(task)`. MEASURED against HotSpot 25.0.4+7, both modes:
    ///
    /// ```text
    ///   pool.invoke(task calling ForkJoinTask.inForkJoinPool())
    ///     HotSpot  true       CratonVM  false
    ///   pool.invoke(task calling ForkJoinTask.getPool())
    ///     HotSpot  non-null   CratonVM  null
    /// ```
    ///
    /// It is not a cosmetic pair. `inForkJoinPool()` is how library code
    /// decides whether it may `fork()` or must run inline, and how
    /// `ManagedBlocker`-shaped code decides whether to compensate — a
    /// permanent `false` sends every such caller down the "I am on an ordinary
    /// thread" branch even while it is executing inside a pool.
    ///
    /// A PIN HANDLE, not a bare address: an inline task is arbitrary Java and
    /// can complete a moving collection, and a thread-local cannot be reached
    /// by the GC hooks (which run on whichever thread triggered the
    /// collection). The handle is taken and released by the same native, so
    /// the stack discipline `unpin_native_roots` needs is preserved.
    pub static FJP_POOL_STACK: std::cell::RefCell<Vec<(usize, ObjectRef)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Push `args[0]` as the pool this thread is now inside; returns the pin
/// handle to hand back to [`fjp_pool_frame_leave`], or `None` when the
/// receiver is not an object (a static call, or a dispatch defect).
pub fn fjp_pool_frame_enter(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<usize> {
    let pool = match args.first().copied() {
        Some(Value::Object(Some(p))) => p,
        _ => return None,
    };
    let base = ctx.pin_native_root(pool);
    FJP_POOL_STACK.with(|s| s.borrow_mut().push((base, pool)));
    Some(base)
}

/// Pop the frame [`fjp_pool_frame_enter`] pushed. Call on EVERY exit path —
/// the callers wrap their body in a closure so there is exactly one.
pub fn fjp_pool_frame_leave(ctx: &mut dyn NativeContext, base: Option<usize>) {
    if let Some(base) = base {
        FJP_POOL_STACK.with(|s| {
            s.borrow_mut().pop();
        });
        ctx.unpin_native_roots(base);
    }
}

/// `ForkJoinTask.inForkJoinPool()`.
pub fn fjp_in_pool() -> bool {
    FJP_POOL_STACK.with(|s| !s.borrow().is_empty())
}

/// `ForkJoinTask.getPool()` — the innermost pool, re-read through its pin so
/// a collection during the task cannot hand back a stale address.
pub fn fjp_current_pool(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let top = FJP_POOL_STACK.with(|s| s.borrow().last().copied());
    top.map(|(base, pool)| ctx.read_native_pin(base, pool))
}

pub fn fjp_common_pool_row(vm: usize) -> Option<ObjectRef> {
    match FJP_COMMON_POOL.peek(vm, |addr| *addr).unwrap_or(0) {
        0 => None,
        addr => Some(unsafe { ObjectRef::from_raw(addr as *mut u8) }),
    }
}

/// VM `vm`'s cached common pool, or `None` before its first `commonPool()`
/// call (gc-common w9-b).
pub fn fjp_common_pool_get_in_vm(vm: usize) -> Option<ObjectRef> {
    fjp_common_pool_row(vm)
}

/// Publish VM `vm`'s common pool carrier. Idempotent by construction -- the
/// only caller mints one when [`fjp_common_pool_get_in_vm`] answered `None`.
pub fn fjp_common_pool_set_in_vm(vm: usize, o: ObjectRef) {
    FJP_COMMON_POOL.with(vm, |addr| *addr = o.as_ptr() as usize);
}

/// Is this VM `vm`'s common pool?
/// `ForkJoinPool.commonPool().shutdown()` is specified as a NO-OP, so every
/// refusal keyed on shutdown state has to let the common pool through --
/// otherwise one stray `shutdown()` anywhere in a process takes out every
/// later user of it.
pub fn fjp_is_common_pool(vm: usize, o: ObjectRef) -> bool {
    let is = |p: Option<ObjectRef>| matches!(p, Some(p) if std::ptr::eq(p.as_ptr(), o.as_ptr()));
    is(fjp_common_pool_row(vm))
}

/// Drop VM `vm`'s fork-join entries and its common-pool row (VM teardown,
/// from `lib.rs::forget_vm_native_root_stores`). Unscoped entries are left
/// alone: they may belong to a VM that is still running.
/// Idempotent (gc-common w9-b).
pub fn forget_vm_forkjoin_state(vm: usize) {
    if vm == FJP_UNSCOPED_VM {
        return;
    }
    fjp_state().lock().retain(|_, e| e.vm != vm);
    FJP_COMMON_POOL.forget(vm);
}

/// GC root scan hook for ForkJoinTask's native done/result side-table.
///
/// The table owns both the task key and any cached Object result until the
/// bounded reaper drops the entry. Rooting the key is important: otherwise a
/// young GC can reclaim the task, reuse its address for a different task, and
/// the new task inherits the old cached result.
///
/// Walks only VM `vm`'s entries and common pool, plus the unscoped entries
/// (gc-common w9-b): it used to root every VM's tasks for every collection.
pub fn gc_scan_forkjoin_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    // The cached common pool is reachable ONLY from `FJP_COMMON_POOL` between
    // two `commonPool()` calls, so without this push a young collection
    // reclaims it and the next call hands back a dangling reference -- the
    // exact failure the side table's key push above this one exists to stop.
    if let Some(p) = fjp_common_pool_row(vm) {
        out.push(p);
    }
    let m = fjp_state().lock();
    for (&key, entry) in m.iter() {
        if !entry.owned_by(vm) {
            continue;
        }
        if key != 0 {
            out.push(unsafe { ObjectRef::from_raw(key as *mut u8) });
        }
        if let Value::Object(Some(obj)) = entry.result {
            out.push(obj);
        }
        // The recorded throwable is reachable ONLY from this table until a
        // later `get()`/`join()` replays it — without this push a young GC
        // reclaims the exception object and the replay hands back a dangling
        // reference.
        if let Value::Object(Some(obj)) = entry.thrown {
            out.push(obj);
        }
    }
}

/// Post-GC remap companion to [`gc_scan_forkjoin_roots`].
///
/// Remaps cached Object results and rekeys task entries when a moving
/// collection relocates the rooted task object.
///
/// Only VM `vm`'s entries and common pool, plus the unscoped entries, are
/// touched: another VM's map describes another heap (gc-common w9-b). The
/// owned entries are all taken out before any is re-inserted, so an entry
/// moving onto a key another owned entry is moving off cannot be clobbered.
pub fn gc_update_forkjoin_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let remap_pool = |row: usize| {
        if FJP_COMMON_POOL.has_row(row) {
            FJP_COMMON_POOL.with(row, |addr| {
                if let Some(&moved) = pointer_map.get(&*addr) {
                    *addr = moved;
                }
            });
        }
    };
    remap_pool(vm);
    let mut m = fjp_state().lock();
    let owned: Vec<usize> = m
        .iter()
        .filter(|(_, e)| e.owned_by(vm))
        .map(|(&k, _)| k)
        .collect();
    let mut moved = Vec::with_capacity(owned.len());
    for key in owned {
        if let Some(mut entry) = m.remove(&key) {
            let new_key = pointer_map.get(&key).copied().unwrap_or(key);
            fjp_remap_value_ref(&mut entry.result, pointer_map);
            fjp_remap_value_ref(&mut entry.thrown, pointer_map);
            moved.push((new_key, entry));
        }
    }
    for (new_key, entry) in moved {
        m.insert(new_key, entry);
    }
}

/// Which method actually carries a `ForkJoinTask`'s work.
///
/// The eager-inline Bridge policy used to invoke `compute()` blind, trying
/// `()Ljava/lang/Object;` and then `()V`. That covers exactly the two JDK
/// convenience subclasses -- `RecursiveTask` (`compute()Ljava/lang/Object;`)
/// and `RecursiveAction` (`compute()V`) -- and nothing else. The method every
/// concrete `ForkJoinTask` must implement is the protocol method `exec()Z`;
/// subclasses that extend `ForkJoinTask` DIRECTLY have no `compute()` at all.
///
/// JUnit Platform's `ForkJoinPoolHierarchicalTestExecutorService$ExclusiveTask`
/// is one such class, and it is the task type the whole parallel test executor
/// is built on: its `submit(TestTask)` ends in `forkJoinPool.submit(task)`.
/// The blind `compute()` invoke raised `NoSuchMethodError` twice, swallowed
/// both, and returned null -- so a nested JUnit engine running in CONCURRENT
/// mode executed ZERO tests (`ParallelApplicationEventsIntegrationTests` 0/2,
/// `started ==> expected: 13 but was: 0`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FjtEntry {
    ComputeObject,
    ComputeVoid,
    Exec,
    /// No `compute()`/`exec()` at all — the shape is unrecognised. Kept as its
    /// own variant so the historical "try both `compute()` overloads, then
    /// give up with null" guess stays confined to the case it was written for.
    /// The three real variants above propagate a throwing body instead (see
    /// `fjp_compute_and_complete`); replaying that blind retry for them ran a
    /// throwing `compute()` a SECOND time.
    Unknown,
}

/// Pick the entry point from the receiver's RUNTIME class rather than inferring
/// it from whether a speculative invoke happened to fail: an exception thrown
/// from inside a perfectly good `compute()` is indistinguishable from "no such
/// method" at the `invoke_virtual` result, and used to trigger a bogus second
/// invoke.
pub fn fjt_entry_point(ctx: &mut dyn NativeContext, task: ObjectRef) -> FjtEntry {
    let Some(cls) = ctx.class_name_of_id(ctx.class_id_of_object(task)) else {
        return FjtEntry::ComputeObject;
    };
    if ctx.method_exists(&cls, "compute", "()Ljava/lang/Object;") {
        FjtEntry::ComputeObject
    } else if ctx.method_exists(&cls, "compute", "()V") {
        FjtEntry::ComputeVoid
    } else if ctx.method_exists(&cls, "exec", "()Z") {
        FjtEntry::Exec
    } else {
        // Unknown shape -- keep the historical behaviour.
        FjtEntry::Unknown
    }
}

/// Ask a task that has just run for its RAW RESULT, the way the real
/// `ForkJoinTask.invoke()` does (`doInvoke(); return getRawResult();`).
///
/// `getRawResult()` is VIRTUAL and abstract on `ForkJoinTask`, so every
/// concrete task overrides it over a DIFFERENT field: `RecursiveTask` keeps
/// `result`, `java.util.stream.AbstractTask` keeps `localResult`, a user
/// subclass keeps whatever it likes. So this INVOKES the receiver's method
/// rather than reading a field by name -- a by-name field read is not
/// descriptor-aware in this VM and silently answers `Int(0)`/null for a name it
/// fails to match, which is the same class of silent wrong answer this function
/// exists to remove.
///
/// A missing or failing `getRawResult()` degrades to null -- the value the
/// caller returned unconditionally before -- so no task shape can regress.
pub fn fjt_raw_result(ctx: &mut dyn NativeContext, task: ObjectRef) -> Value {
    let Some(cls) = ctx.class_name_of_id(ctx.class_id_of_object(task)) else {
        return Value::Object(None);
    };
    if !ctx.method_exists(&cls, "getRawResult", "()Ljava/lang/Object;") {
        return Value::Object(None);
    }
    match ctx.invoke_virtual(task, "getRawResult", "()Ljava/lang/Object;", &[]) {
        Ok(value) => value.unwrap_or(Value::Object(None)),
        Err(_) => Value::Object(None),
    }
}

/// Threads currently inside a task body, i.e. `ForkJoinPool.getActiveThread
/// Count()`'s "threads stealing or executing tasks". This pool runs every task
/// inline on the submitting thread, so a thread is an active pool thread for
/// exactly as long as it is inside `fjp_compute_*`. Nested fork/join on one
/// thread is still one thread, hence the depth guard.
pub static FJP_ACTIVE_THREADS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

thread_local! {
    pub static FJP_COMPUTE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII guard: counts the calling thread as an active pool thread for the
/// duration of one task body, and only on the outermost entry.
pub struct FjpActiveGuard(pub bool);

impl FjpActiveGuard {
    pub fn enter() -> Self {
        let outermost = FJP_COMPUTE_DEPTH.with(|d| {
            let n = d.get();
            d.set(n + 1);
            n == 0
        });
        if outermost {
            FJP_ACTIVE_THREADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Self(outermost)
    }
}

impl Drop for FjpActiveGuard {
    fn drop(&mut self) {
        FJP_COMPUTE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        if self.0 {
            FJP_ACTIVE_THREADS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Run `task`'s body inline and COMPLETE it in the `fjp_state` side table.
///
/// Returns the (possibly relocated) task plus the outcome. A task body that
/// throws is recorded as an abnormal completion via [`fjp_state_set_thrown`]
/// and the exception is propagated to the caller — it used to be discarded,
/// which made `join()` hand back null and `isCompletedNormally()` report
/// success for a task that had blown up.
///
/// The completion happens while `task_pin` is still held and before any
/// further allocation, so no GC can move the throwable between catching it
/// and rooting it in the table.
///
/// Claims `task` via [`fjp_state_claim`] before running anything. Every
/// caller already has its own fast-path `done` check before calling this
/// function; that check stays as a cheap skip, but it is no longer what
/// makes running the body exactly-once safe — this claim is. See
/// `FjpEntry::running`.
pub fn fjp_compute_and_complete(
    ctx: &mut dyn NativeContext,
    task: ObjectRef,
) -> (ObjectRef, Result<Value, MethodCallFailed>) {
    let (task, claim) = fjp_state_claim(ctx, task);
    if let FjpClaim::Done(outcome) = claim {
        return (task, outcome);
    }
    let reentrant = matches!(claim, FjpClaim::Reentrant);
    let _active = FjpActiveGuard::enter();
    let task_pin = ctx.pin_native_root(task);
    let mut live_task = task;
    let outcome = match fjt_entry_point(ctx, live_task) {
        FjtEntry::ComputeObject => ctx
            .invoke_virtual(live_task, "compute", "()Ljava/lang/Object;", &[])
            .map(|v| v.unwrap_or(Value::Object(None))),
        FjtEntry::ComputeVoid => match ctx.invoke_virtual(live_task, "compute", "()V", &[]) {
            // `compute()` is VOID here, so its own value is always null. Right
            // for `RecursiveAction`; WRONG for every `CountedCompleter`, whose
            // value goes to the raw-result slot (`AbstractTask.compute()` ends
            // `setLocalResult(doLeaf()); tryComplete();`). The real
            // `ForkJoinTask.invoke()` is `doInvoke(); return getRawResult();`.
            // Returning null instead handed every real parallel-stream terminal
            // a null `Node`/sink -- `Nodes.collect` NPE'd at
            // `node.getChildCount()`. Re-read the pin first: `compute()`
            // allocates, so it can move the task.
            Ok(_) => {
                live_task = ctx.read_native_pin(task_pin, live_task);
                Ok(fjt_raw_result(ctx, live_task))
            }
            Err(e) => Err(e),
        },
        FjtEntry::Exec => {
            // `exec()` runs the task and leaves any value in the task's own
            // raw-result slot; `ForkJoinTask<Void>` subclasses answer null.
            match ctx.invoke_virtual(live_task, "exec", "()Z", &[]) {
                Ok(_) => {
                    live_task = ctx.read_native_pin(task_pin, live_task);
                    ctx.invoke_virtual(live_task, "getRawResult", "()Ljava/lang/Object;", &[])
                        .map(|v| v.unwrap_or(Value::Object(None)))
                }
                Err(e) => Err(e),
            }
        }
        FjtEntry::Unknown => {
            // Historical guess for a task class with no recognised entry
            // point: try both `compute()` shapes and settle for null. Kept
            // byte-for-byte because "which method carries the work" is
            // genuinely unknown here, so a propagated NoSuchMethodError
            // would be noise rather than the task's own failure.
            match ctx.invoke_virtual(live_task, "compute", "()Ljava/lang/Object;", &[]) {
                Ok(Some(val)) => Ok(val),
                _ => {
                    live_task = ctx.read_native_pin(task_pin, live_task);
                    let _ = ctx.invoke_virtual(live_task, "compute", "()V", &[]);
                    Ok(Value::Object(None))
                }
            }
        }
    };
    live_task = ctx.read_native_pin(task_pin, live_task);
    // A re-entrant run leaves the claim and the completion to the frame that
    // owns them; recording DONE here would mark the task finished while that
    // frame is still computing it.
    let outcome = if reentrant {
        outcome
    } else {
        fjp_complete_from_outcome(ctx, live_task, outcome)
    };
    ctx.unpin_native_roots(task_pin);
    (live_task, outcome)
}

/// Complete `task` inline for a SUBMIT-shaped entry point.
///
/// `submit`/`execute` hand the task back and let a failure surface at the
/// matching `join()`/`get()` — the real JDK does not raise the task's
/// exception at the submitter. So a throwing body is RECORDED (via
/// `fjp_compute_and_complete`) but not re-raised here. An internal VM error
/// still propagates.
pub fn fjp_compute_for_submit(
    ctx: &mut dyn NativeContext,
    task: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let (live_task, outcome) = fjp_compute_and_complete(ctx, task);
    match outcome {
        Ok(_) | Err(MethodCallFailed::ExceptionThrown(_)) => Ok(live_task),
        Err(internal) => Err(internal),
    }
}

/// Record a computed outcome against `task` and hand it back.
///
/// `Err(InternalError)` is a VM-level failure rather than something the task
/// "completed with", so it is passed through without touching `done` — but
/// the claim [`fjp_state_claim`] handed the caller still has to be released,
/// via [`fjp_state_release_claim`], or a task whose body raised an internal
/// error would stay `running` forever and wedge every future claimant.
pub fn fjp_complete_from_outcome(
    ctx: &mut dyn NativeContext,
    task: ObjectRef,
    outcome: Result<Value, MethodCallFailed>,
) -> Result<Value, MethodCallFailed> {
    match outcome {
        Ok(value) => {
            fjp_state_set_done(ctx.vm_identity(), task, value);
            fjp_stamp_real_status(ctx, task, FJT_STATUS_DONE);
            Ok(value)
        }
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            fjp_state_set_thrown(ctx.vm_identity(), task, exc);
            fjp_stamp_real_status(
                ctx,
                task,
                FJT_STATUS_DONE | FJT_STATUS_ABNORMAL | FJT_STATUS_THROWN,
            );
            Err(MethodCallFailed::ExceptionThrown(exc))
        }
        Err(other) => {
            fjp_state_release_claim(task);
            Err(other)
        }
    }
}

pub fn fjp_compute_void(
    ctx: &mut dyn NativeContext,
    task: ObjectRef,
) -> (ObjectRef, Result<(), MethodCallFailed>) {
    let (task, claim) = fjp_state_claim(ctx, task);
    if let FjpClaim::Done(outcome) = claim {
        return (task, outcome.map(|_| ()));
    }
    let reentrant = matches!(claim, FjpClaim::Reentrant);
    let _active = FjpActiveGuard::enter();
    let task_pin = ctx.pin_native_root(task);
    let (method, desc) = if fjt_entry_point(ctx, task) == FjtEntry::Exec {
        ("exec", "()Z")
    } else {
        ("compute", "()V")
    };
    let outcome = ctx.invoke_virtual(task, method, desc, &[]).map(|_| ());
    let live_task = ctx.read_native_pin(task_pin, task);
    let outcome = if reentrant {
        outcome.map(|()| Value::Object(None))
    } else {
        fjp_complete_from_outcome(ctx, live_task, outcome.map(|()| Value::Object(None)))
    };
    ctx.unpin_native_roots(task_pin);
    (live_task, outcome.map(|_| ()))
}

// ---------------------------------------------------------------------------
// Bulk submission — invokeAll / invokeAny
// ---------------------------------------------------------------------------
//
// `ForkJoinPool.commonPool()` is a VM Bridge shortcut: it hands back an object
// that never ran the real pool constructor, so its `queues`/`runState`/`mode`
// fields are unset. Every pool method therefore has to be either implemented
// here or kept off the real-JDK path — anything that falls through to real
// bytecode reaches `poolSubmit()`/`submissionQueue()` against that
// under-initialized instance and throws `RejectedExecutionException`.
//
// `invokeAll(Collection)` was the overload Weld's `ConcurrentBeanDeployer` →
// `AbstractExecutorServices.invokeAllAndCheckForExceptions` calls, and it was
// never covered: that is what failed the entire `org.hibernate.orm.test.cdi.*`
// cluster. `invokeAny` was uncovered too and was WORSE than a crash — real
// bytecode ran the callables and then returned null, a silent wrong answer.

/// Wrap `cause` in a `java.util.concurrent.ExecutionException` — the exception
/// `Future.get()` is specified to raise for a task that completed abnormally.
///
/// The wrapper is load-bearing, not cosmetic: Weld's
/// `AbstractExecutorServices.checkForExceptions` catches `ExecutionException`
/// specifically and rethrows its `getCause()`. Handing back the bare cause
/// would sail straight through that catch.
pub fn fjp_execution_exception(ctx: &mut dyn NativeContext, cause: ObjectRef) -> MethodCallFailed {
    let cause_pin = ctx.pin_native_root(cause);
    let live_cause = ctx.read_native_pin(cause_pin, cause);
    let built = cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/util/concurrent/ExecutionException",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(live_cause))],
    );
    let live_cause = ctx.read_native_pin(cause_pin, live_cause);
    ctx.unpin_native_roots(cause_pin);
    match built {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        // Constructor unavailable: the raw cause still REPORTS the failure,
        // which beats reporting success with a null result.
        Ok(None) => MethodCallFailed::ExceptionThrown(live_cause),
    }
}

/// What `join()` / `invoke()` should observe for a task.
///
/// `ForkJoinTask.join()` rethrows the task's own throwable unchecked; the
/// `ExecutionException` wrapping is `get()`'s contract, not `join()`'s.
pub fn fjp_state_get_for_join(o: ObjectRef) -> Result<(bool, Value), MethodCallFailed> {
    match fjp_state_thrown(o) {
        Some(thrown) => Err(MethodCallFailed::ExceptionThrown(thrown)),
        None => fjp_state_get_checked(o),
    }
}

/// What `Future.get()` should observe for a task.
pub fn fjp_state_get_for_future(
    ctx: &mut dyn NativeContext,
    o: ObjectRef,
) -> Result<(bool, Value), MethodCallFailed> {
    match fjp_state_thrown(o) {
        Some(thrown) => Err(fjp_execution_exception(ctx, thrown)),
        None => fjp_state_get_checked(o),
    }
}

/// Shared body of every `join()` / `invoke()` registration.
///
/// Computes the task if it has not run yet, and replays an abnormal
/// completion as the task's OWN throwable — `ForkJoinTask.join()` rethrows
/// unchecked rather than wrapping.
pub fn fjp_join_body(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let (done, cached) = fjp_state_get_for_join(this)?;
    if done {
        tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(this)), cached = ?cached, "fjt.join cached");
        return Ok(Some(cached));
    }
    let (this, outcome) = fjp_compute_and_complete(ctx, this);
    tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(this)), ok = outcome.is_ok(), "fjt.join computed");
    Ok(Some(outcome?))
}

/// Shared body of every `Future.get()` registration.
///
/// Same as [`fjp_join_body`] except an abnormal completion is reported as
/// `ExecutionException(cause)` — `get()`'s contract, and the type Weld's
/// `checkForExceptions` catches.
pub fn fjp_future_get_body(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let (done, cached) = fjp_state_get_for_future(ctx, this)?;
    if done {
        return Ok(Some(cached));
    }
    let (_this, outcome) = fjp_compute_and_complete(ctx, this);
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err(MethodCallFailed::ExceptionThrown(cause)) => Err(fjp_execution_exception(ctx, cause)),
        Err(internal) => Err(internal),
    }
}

/// Shared body of `RecursiveAction`'s `join()` / `invoke()` — void result.
pub fn fjp_join_void_body(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let (done, _) = fjp_state_get_for_join(this)?;
    if !done {
        let (_this, outcome) = fjp_compute_void(ctx, this);
        outcome?;
    }
    Ok(Some(Value::Object(None)))
}

/// Shared body of the whole `quietly*` family — drive the task to completion
/// and SWALLOW its outcome.
///
/// The defining property of `quietlyJoin()` / `quietlyInvoke()` is that they
/// neither hand back a value nor throw: the caller is expected to ask
/// `isCompletedAbnormally()` / `getException()` afterwards. So the throwable is
/// still RECORDED — `fjp_compute_and_complete` routes it through
/// `fjp_state_set_thrown` before this sees it — it is simply not raised here.
/// "Swallow" means "do not rethrow", NOT "do not record"; discarding it would
/// make `getException()` answer null for a task that had just blown up, which
/// is the same class of silent-success bug `fjp_complete_from_outcome` exists
/// to prevent. An internal VM error is not a task outcome and still propagates.
///
/// The done-check reads [`fjp_state_flags`] rather than
/// [`fjp_state_get_checked`] on purpose: the checked variant raises the
/// `CancellationException` proxy for a cancelled task, and that proxy is a
/// `MethodCallFailed::InternalError`, not an `ExceptionThrown` — it would sail
/// straight through the swallow below and abort the caller. The real
/// `quietlyJoin()` on a cancelled task returns silently (its `status` is
/// already negative, so it never even reaches `awaitDone`).
pub fn fjp_quietly_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let (done, _cancelled) = fjp_state_flags(this);
    if done {
        return Ok(());
    }
    let (_this, outcome) = fjp_compute_and_complete(ctx, this);
    match outcome {
        Ok(_) | Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),
        Err(internal) => Err(internal),
    }
}

/// Does this receiver keep its raw result in a field of its OWN, rather than in
/// the `fjp_state` side table?
///
/// `ForkJoinTask` / `RecursiveTask` / `RecursiveAction` are modelled entirely in
/// the side table — their `getRawResult`/`setRawResult` registrations right
/// below read and write [`FjpEntry::result`] — so for those three the table
/// write IS the raw-result slot and invoking the virtual would only re-enter
/// this module. Every OTHER receiver (a user subclass of `ForkJoinTask`,
/// `java.util.stream.AbstractTask`) declares its own field, and only its own
/// `setRawResult` can write it.
///
/// The `method_exists` half is what makes this safe in synthetic-JDK mode,
/// where the task classes are fabricated stubs with no `setRawResult` to call:
/// it answers `false` there and the caller takes the side-table-only path.
///
/// **How narrow the `false` arm actually is, measured (W7-48).** All three
/// names in the `matches!` below are ABSTRACT classes, so that arm can only
/// fire on a receiver whose RUNTIME class is literally one of them — which no
/// Java program can produce. A user subclass of `RecursiveTask` does not
/// name-match (its class is `Foo$1`), and `NativeContext::method_exists` walks
/// the superclass chain (`vm/src/vm/vm_exec.rs`), so it finds the inherited
/// `protected final setRawResult` and this function answers `true`. The caller
/// then invokes the virtual — which resolves straight back to the registered
/// `RecursiveTask.setRawResult` native, i.e. to the same side-table write, by a
/// longer route. The predicate is therefore behaviourally right in every
/// reachable case, but NOT for the reason the paragraph above gives: a
/// `RecursiveTask` / `RecursiveAction` subclass has no field of its own either,
/// and its `setRawResult` is `final` so it cannot acquire one.
///
/// The consequence worth knowing: the side-table-only arm of
/// `fjp_complete_body` — and therefore [`fjp_state_set_raw_result`] — is
/// defence in depth, not a live path, and cannot be falsified from Java. Do
/// not write a corpus assertion claiming to cover it; it would be green before
/// and after.
pub fn fjt_has_own_raw_result_slot(ctx: &mut dyn NativeContext, task: ObjectRef) -> bool {
    let Some(cls) = ctx.class_name_of_id(ctx.class_id_of_object(task)) else {
        return false;
    };
    !matches!(
        cls.as_str(),
        "java/util/concurrent/ForkJoinTask"
            | "java/util/concurrent/RecursiveTask"
            | "java/util/concurrent/RecursiveAction"
    ) && ctx.method_exists(&cls, "setRawResult", "(Ljava/lang/Object;)V")
}

/// Shared body of every `complete(Ljava/lang/Object;)V` registration.
///
/// Real body (`javap -c java.util.concurrent.ForkJoinTask`, JDK 25):
///
/// ```text
/// public void complete(V v) {
///     try { setRawResult(v); }
///     catch (Throwable rex) { trySetException(rex); return; }
///     setDone();
/// }
/// ```
///
/// All four registrations used to be a bare `fjp_state_set_done(this, val)`,
/// which got two things wrong:
///
/// * `fjp_state_set_done` cleared `thrown`, so `t.complete(v)` on a task that
///   had already completed exceptionally ERASED the abnormal record. The real
///   `setDone()` is a bitwise OR into a write-once status word and cannot clear
///   `ABNORMAL` — see [`fjp_state_set_done`], which no longer clears it either.
/// * `setRawResult` is VIRTUAL, and for a receiver that keeps its raw result in
///   its own field (see [`fjt_has_own_raw_result_slot`]) the side-table write
///   left that field untouched: a later real `getRawResult()` answered null for
///   the very value the caller had just handed to `complete()`.
///
/// A `setRawResult` that throws is RECORDED as the task's abnormal completion,
/// exactly like the real `catch (Throwable rex) { trySetException(rex); }`, and
/// the task is NOT marked normally done. Reporting success there would be the
/// same fabricated success this family keeps producing.
pub fn fjp_complete_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    val: Value,
) -> Result<(), MethodCallFailed> {
    if !fjt_has_own_raw_result_slot(ctx, this) {
        // `setRawResult(v); setDone();` — as two calls, because only the
        // second is write-once. The real `complete(V)` runs `setRawResult`
        // BEFORE `setDone()`, cancelled or not; suppressing both made
        // `t.cancel(false); t.complete(v); t.getRawResult()` answer null where
        // the real one answers `v` (W6-9 §7.4). `fjp_state_set_done` re-stores
        // `result` for the ordinary path, which is the same value.
        fjp_state_set_raw_result(ctx.vm_identity(), this, val);
        fjp_state_set_done(ctx.vm_identity(), this, val);
        fjp_stamp_real_status(ctx, this, FJT_STATUS_DONE);
        return Ok(());
    }
    // `setRawResult` runs arbitrary bytecode and can allocate, so both the
    // receiver and the value have to survive it behind pins.
    let this_pin = ctx.pin_native_root(this);
    let val_pin = pinned_object_value(ctx, val);
    let outcome = ctx.invoke_virtual(this, "setRawResult", "(Ljava/lang/Object;)V", &[val]);
    let live_this = ctx.read_native_pin(this_pin, this);
    let live_val = read_pinned_object_value(ctx, val_pin, val);
    // Complete while the pins are still held and before any further
    // allocation, the same ordering `fjp_compute_and_complete` relies on.
    let recorded = match outcome {
        Ok(_) => {
            fjp_state_set_done(ctx.vm_identity(), live_this, live_val);
            fjp_stamp_real_status(ctx, live_this, FJT_STATUS_DONE);
            Ok(())
        }
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            fjp_state_set_thrown(ctx.vm_identity(), live_this, exc);
            fjp_stamp_real_status(
                ctx,
                live_this,
                FJT_STATUS_DONE | FJT_STATUS_ABNORMAL | FJT_STATUS_THROWN,
            );
            Ok(())
        }
        // A VM-level failure is not something the task "completed with".
        Err(internal) => Err(internal),
    };
    ctx.unpin_native_roots(this_pin);
    recorded
}

/// `ex instanceof RuntimeException || ex instanceof Error` — the test the real
/// `completeExceptionally` uses to decide whether to wrap.
///
/// Walks the superclass chain by NAME rather than using
/// `is_subclass(ClassId, ClassId)`, because that needs both reference classes
/// loaded and loading one from inside a completion path would allocate. The
/// walk stops at `Exception`/`Throwable`/`Object` — anything reaching those
/// without passing `RuntimeException`/`Error` is checked — and is hop-bounded
/// so a malformed hierarchy cannot spin.
pub fn fjt_is_unchecked_throwable(ctx: &mut dyn NativeContext, ex: ObjectRef) -> bool {
    let mut cur = Some(ctx.class_id_of_object(ex));
    for _ in 0..64 {
        let Some(cid) = cur else { return false };
        match ctx.class_name_arc_of_id(cid).as_deref() {
            Some("java/lang/RuntimeException") | Some("java/lang/Error") => return true,
            Some("java/lang/Exception")
            | Some("java/lang/Throwable")
            | Some("java/lang/Object") => return false,
            _ => {}
        }
        cur = ctx.superclass_of(cid);
    }
    false
}

/// Shared body of every `completeExceptionally(Ljava/lang/Throwable;)V`
/// registration.
///
/// Real body (`javap -c java.util.concurrent.ForkJoinTask`, JDK 25):
///
/// ```text
/// public void completeExceptionally(Throwable ex) {
///     trySetException((ex instanceof RuntimeException) || (ex instanceof Error)
///                     ? ex : new RuntimeException(ex));
/// }
/// ```
///
/// W6-9: this was registered NOWHERE and named in NEITHER allow-list, so in
/// real-JDK mode it ran real bytecode — `trySetThrown` CASes the real
/// `aux`/`status` fields, which nothing in this VM ever reads, because every
/// completion this model publishes lives in the `fjp_state` side table. A task
/// the caller had explicitly failed therefore stayed `done == false`, and the
/// next `join()`/`get()` RAN ITS BODY and handed back a value, with
/// `isCompletedAbnormally()` answering `false` and `getException()` answering
/// null. That is the fabricated success the `complete()` fix above is measured
/// against, and without this registration the measurement cannot be taken.
///
/// The done-check is `trySetThrown`'s own `status >= 0` guard: the real status
/// word is write-once and the FIRST completion wins, so this is a no-op on an
/// already-complete task.
pub fn fjp_complete_exceptionally_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    ex: Value,
) -> Result<(), MethodCallFailed> {
    let (done, _cancelled) = fjp_state_flags(this);
    if done {
        return Ok(());
    }
    let this_pin = ctx.pin_native_root(this);
    let ex_pin = pinned_object_value(ctx, ex);
    let unchecked = match ex {
        Value::Object(Some(t)) => fjt_is_unchecked_throwable(ctx, t),
        // The real `null instanceof RuntimeException` is false, so a null cause
        // is wrapped rather than passed through.
        _ => false,
    };
    let recorded = if unchecked {
        read_pinned_object_value(ctx, ex_pin, ex)
    } else {
        let cause = read_pinned_object_value(ctx, ex_pin, ex);
        match cratonvm_native_api::throwable_builder::construct_new(
            ctx,
            "java/lang/RuntimeException",
            "(Ljava/lang/Throwable;)V",
            &[cause],
        ) {
            Ok(Some(wrapped)) => Value::Object(Some(wrapped)),
            // Constructor unavailable: the bare throwable still RECORDS the
            // failure, which beats recording nothing — the same degrade
            // `fjp_execution_exception` takes. (For a null `ex` there is then
            // nothing to record at all and the task stays incomplete; that is
            // the one input this cannot model without the wrapper class.)
            _ => read_pinned_object_value(ctx, ex_pin, ex),
        }
    };
    let live_this = ctx.read_native_pin(this_pin, this);
    if let Value::Object(Some(t)) = recorded {
        fjp_state_set_thrown(ctx.vm_identity(), live_this, t);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(())
}

/// Run one `Callable.call()` inline and complete a fresh task key with the
/// outcome. The task object is the `Future` handed back to the caller.
///
/// A throwing callable is recorded as that task's abnormal completion and is
/// NOT propagated: `invokeAll` must run every callable and report failures
/// through the individual futures. Only an internal VM error aborts.
pub fn fjp_run_callable_as_task(
    ctx: &mut dyn NativeContext,
    callable: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // Pin the callable BEFORE allocating: `alloc_concurrent_synthetic` can
    // trigger a collection that relocates it.
    let callable_pin = ctx.pin_native_root(callable);
    let task = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ForkJoinTask", 0)?;
    let task_pin = ctx.pin_native_root(task);
    let live_callable = ctx.read_native_pin(callable_pin, callable);
    let outcome = {
        let _active = FjpActiveGuard::enter();
        ctx.invoke_virtual(live_callable, "call", "()Ljava/lang/Object;", &[])
            .map(|v| v.unwrap_or(Value::Object(None)))
    };
    let live_task = ctx.read_native_pin(task_pin, task);
    let recorded = fjp_complete_from_outcome(ctx, live_task, outcome);
    ctx.unpin_native_roots(callable_pin);
    match recorded {
        Ok(_) | Err(MethodCallFailed::ExceptionThrown(_)) => Ok(live_task),
        Err(internal) => Err(internal),
    }
}

/// A batch of `Collection` elements that stays GC-rooted while it is processed.
///
/// The elements MUST remain pinned for the whole of `invokeAll`/`invokeAny`:
/// running one callable allocates freely, so any element ref handed out as a
/// bare address would be stale by the next iteration. Callers re-read each
/// element through its own handle via [`FjpBatch::element`] and release the
/// whole batch once — with [`FjpBatch::release`].
pub struct FjpBatch {
    /// Pin-stack watermark to truncate back to; releases every handle below.
    pub base: usize,
    pub handles: Vec<(usize, ObjectRef)>,
}

impl FjpBatch {
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// The current address of element `i`, re-read through its pin.
    pub fn element(&self, ctx: &dyn NativeContext, i: usize) -> ObjectRef {
        let (handle, obj) = self.handles[i];
        ctx.read_native_pin(handle, obj)
    }

    pub fn release(self, ctx: &mut dyn NativeContext) {
        ctx.unpin_native_roots(self.base);
    }
}

/// `submit(Runnable)` / `submit(Runnable, T)` counterpart of
/// [`fjp_run_callable_as_task`]: run the runnable inline and complete a fresh
/// task key with `fixed_result` (null for the no-result overload).
pub fn fjp_run_runnable_as_task(
    ctx: &mut dyn NativeContext,
    runnable: ObjectRef,
    fixed_result: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let runnable_pin = ctx.pin_native_root(runnable);
    let result_pin = pinned_object_value(ctx, fixed_result);
    let task = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ForkJoinTask", 0)?;
    let task_pin = ctx.pin_native_root(task);
    let live_runnable = ctx.read_native_pin(runnable_pin, runnable);
    let outcome = {
        let _active = FjpActiveGuard::enter();
        ctx.invoke_virtual(live_runnable, "run", "()V", &[])
    };
    let live_task = ctx.read_native_pin(task_pin, task);
    let result = read_pinned_object_value(ctx, result_pin, fixed_result);
    let recorded = fjp_complete_from_outcome(ctx, live_task, outcome.map(|_| result));
    ctx.unpin_native_roots(runnable_pin);
    match recorded {
        Ok(_) | Err(MethodCallFailed::ExceptionThrown(_)) => Ok(live_task),
        Err(internal) => Err(internal),
    }
}

/// Drain a `Collection` receiver into a pinned [`FjpBatch`].
///
/// Iterates through the real `Iterator` bytecode rather than assuming a
/// concrete container: Weld passes an `ArrayList`, but `invokeAll` accepts any
/// `Collection` and a `Set`/unmodifiable view has to work identically.
pub fn fjp_collect_elements(
    ctx: &mut dyn NativeContext,
    collection: ObjectRef,
) -> Result<FjpBatch, MethodCallFailed> {
    let coll_pin = ctx.pin_native_root(collection);
    let live_coll = ctx.read_native_pin(coll_pin, collection);
    let iter = match ctx.invoke_virtual(live_coll, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        Ok(_) => {
            ctx.unpin_native_roots(coll_pin);
            return Ok(FjpBatch {
                base: coll_pin,
                handles: Vec::new(),
            });
        }
        Err(e) => {
            ctx.unpin_native_roots(coll_pin);
            return Err(e);
        }
    };
    let iter_pin = ctx.pin_native_root(iter);
    let mut live_iter = iter;
    // Every element collected so far must stay rooted across the next
    // `hasNext`/`next` (both can allocate), so each one is pinned as it is
    // seen and the whole batch is re-read through its own handle at the end.
    // The handle is carried explicitly rather than derived from `iter_pin`
    // arithmetic: the pin stack being densely indexed is an implementation
    // detail of `pin_native_root`, not a contract this code should rely on.
    let mut out: Vec<(usize, ObjectRef)> = Vec::new();
    loop {
        live_iter = ctx.read_native_pin(iter_pin, live_iter);
        match ctx.invoke_virtual(live_iter, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(v))) if v != 0 => {}
            Ok(_) => break,
            Err(e) => {
                ctx.unpin_native_roots(coll_pin);
                return Err(e);
            }
        }
        live_iter = ctx.read_native_pin(iter_pin, live_iter);
        match ctx.invoke_virtual(live_iter, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(element)))) => {
                let handle = ctx.pin_native_root(element);
                out.push((handle, element));
            }
            // `invokeAll`/`invokeAny` are specified to throw NPE when the
            // collection or any element is null (confirmed against the host
            // JDK). Skipping the element instead would hand back a list
            // SHORTER than the caller submitted — a silent wrong answer of
            // exactly the kind this bridge exists to stop producing.
            Ok(_) => {
                ctx.unpin_native_roots(coll_pin);
                return Err(RuntimeError::NullPointerException {
                    message: Some("null task in the submitted collection".to_string()),
                }
                .into());
            }
            Err(e) => {
                ctx.unpin_native_roots(coll_pin);
                return Err(e);
            }
        }
    }
    // Deliberately NOT unpinned here — the caller runs user code between
    // elements, and the batch has to survive it.
    Ok(FjpBatch {
        base: coll_pin,
        handles: out,
    })
}

/// `ForkJoinPool.invokeAll(Collection)` (and the timed / uninterruptible
/// variants, which differ only in a timeout this inline model never needs).
///
/// Runs every callable inline in submission order and returns a real
/// `java.util.ArrayList` of completed task objects. Matches the JDK contract:
/// the call returns only once EVERY task has completed, and a task that threw
/// is reported through its own future rather than aborting the batch.
pub fn fjp_invoke_all(ctx: &mut dyn NativeContext, tasks: ObjectRef) -> MethodCallResult {
    let batch = fjp_collect_elements(ctx, tasks)?;
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
        Ok(Some(Value::Object(Some(l)))) => l,
        Ok(_) => {
            batch.release(ctx);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            batch.release(ctx);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let mut live_list = list;
    for i in 0..batch.len() {
        let callable = batch.element(ctx, i);
        let task = match fjp_run_callable_as_task(ctx, callable) {
            Ok(t) => t,
            Err(e) => {
                batch.release(ctx);
                return Err(e);
            }
        };
        let task_pin = ctx.pin_native_root(task);
        live_list = ctx.read_native_pin(list_pin, live_list);
        let live_task = ctx.read_native_pin(task_pin, task);
        let added = ctx.invoke_virtual(
            live_list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(live_task))],
        );
        // The list roots the task from here on, so the task pin can go.
        ctx.unpin_native_roots(task_pin);
        if let Err(e) = added {
            batch.release(ctx);
            return Err(e);
        }
    }
    live_list = ctx.read_native_pin(list_pin, live_list);
    batch.release(ctx);
    Ok(Some(Value::Object(Some(live_list))))
}

/// `ForkJoinPool.invokeAny(Collection)` (and its timed variant).
///
/// Returns the result of the first callable that completes normally. Per the
/// `ExecutorService` contract an empty collection raises
/// `IllegalArgumentException`, and a collection in which EVERY callable throws
/// raises `ExecutionException` wrapping the last failure — the previous
/// fall-through to real bytecode returned null in both cases, which reads as a
/// successful "the task produced null" to every caller.
pub fn fjp_invoke_any(ctx: &mut dyn NativeContext, tasks: ObjectRef) -> MethodCallResult {
    let batch = fjp_collect_elements(ctx, tasks)?;
    if batch.len() == 0 {
        batch.release(ctx);
        return Err(RuntimeError::IllegalArgumentException {
            message: "invokeAny: empty task collection".to_string(),        }
        .into());
    }
    // The last failure is kept pinned so it survives the remaining callables'
    // allocations and is still valid when it becomes the ExecutionException's
    // cause below.
    let mut last_failure: Option<(usize, ObjectRef)> = None;
    for i in 0..batch.len() {
        let callable = batch.element(ctx, i);
        let task = match fjp_run_callable_as_task(ctx, callable) {
            Ok(t) => t,
            Err(e) => {
                batch.release(ctx);
                return Err(e);
            }
        };
        match fjp_state_thrown(task) {
            None => {
                let (_, result) = fjp_state_get(task);
                batch.release(ctx);
                return Ok(Some(result));
            }
            Some(thrown) => {
                let handle = ctx.pin_native_root(thrown);
                last_failure = Some((handle, thrown));
            }
        }
    }
    let failure = last_failure.map(|(handle, obj)| ctx.read_native_pin(handle, obj));
    let result = match failure {
        Some(cause) => Err(fjp_execution_exception(ctx, cause)),
        // Unreachable: the collection was non-empty, so some callable ran.
        None => Ok(Some(Value::Object(None))),
    };
    batch.release(ctx);
    result
}


pub fn register_forkjoin_natives(r: &mut NativeMethodRegistry) {
    let pool = "java/util/concurrent/ForkJoinPool";
    r.register(pool, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(pool, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let par = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        ctx.set_field(this, 0, Value::Int(par));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        pool,
        "commonPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
        |ctx, _args| {
            let p = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ForkJoinPool", 1)?;
            ctx.set_field(p, 0, Value::Int(FJP_COMMON_PARALLELISM));
            Ok(Some(Value::Object(Some(p))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(pool, "getParallelism", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // KEEP: reads the same constant `commonPool()` right above stamps into the
    // pool's parallelism slot, so this is a real readout of this VM's common
    // pool rather than an independent guess. (It is also the value HotSpot
    // reports on a single-core host.)
    r.register(pool, "getCommonPoolParallelism", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(FJP_COMMON_PARALLELISM)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W2: `shutdown()` was a no-op while `isShutdown()`/`isTerminated()` were
    // constant `false`, so the standard drain loop
    // `pool.shutdown(); while (!pool.isTerminated()) pool.awaitTermination(...)`
    // never made progress — and `awaitTermination` right below it already
    // returned `true`, so the two answers actively contradicted each other.
    //
    // This pool executes every task INLINE (see `invoke`/`submit` below), so
    // there is never anything still running: "shut down" and "terminated"
    // coincide, and one flag captures both. It lives in the `fjp_state` side
    // table rather than an instance slot because the carrier has a single field
    // (parallelism) and `commonPool` allocates it one field wide; the side
    // table is also the only one of the two that the GC hook
    // (`gc_update_forkjoin_refs`) already remaps. A pool is never also a task,
    // so the key spaces cannot collide.
    r.register(pool, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_state_set_done(ctx.vm_identity(), this, Value::Object(None));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(pool, "shutdownNow", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_state_set_done(ctx.vm_identity(), this, Value::Object(None));
        // Nothing is ever queued — tasks run inline — so the "tasks that never
        // started" list is genuinely empty.
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        // gc-common w20-b: the list was held raw across its array.
        let list: ObjectRef = alloc_array_into_field(
            ctx,
            list,
            0,
            cratonvm_types::ArrayElementType::Reference,
            10,
        );
        ctx.set_field(list, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(pool, "isShutdown", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (shut, _) = fjp_state_flags(this);
        Ok(Some(Value::Int(i32::from(shut))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(pool, "isTerminated", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (shut, _) = fjp_state_flags(this);
        Ok(Some(Value::Int(i32::from(shut))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // invoke(ForkJoinTask) — call compute() on the task, return result.
    // WP4.3 fix: use the side-table for done/result so we don't redundantly
    // re-invoke compute() on each fork/join/invoke (in real-JDK mode the
    // field-index path was reading 0 every time → exponential blow-up).
    r.register(
        pool,
        "invoke",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;",
        |ctx, args| {
            let task = obj_arg(args, 1)?;
            let (done, cached) = fjp_state_get(task);
            if done {
                return Ok(Some(cached));
            }
            // `ForkJoinPool.invoke(task)` DOES rethrow the task's exception —
            // unlike submit/execute, it is a "run it and give me the answer"
            // call with nowhere else to report a failure. Same COPY as the
            // real-JDK twin in `register_real_jdk_forkjoin_essentials`: two
            // registrations of one method that disagree about what they raise
            // is the drifted-twin shape this campaign keeps finding.
            let (_task, outcome) = fjp_compute_and_complete(ctx, task);
            match outcome {
                Ok(v) => Ok(Some(v)),
                Err(MethodCallFailed::ExceptionThrown(ex)) => {
                    Err(fjp_pool_invoke_exception(ctx, ex))
                }
                Err(internal) => Err(internal),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // submit(ForkJoinTask) — eagerly compute inline
    r.register(
        pool,
        "submit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let mut task = obj_arg(args, 1)?;
            let (done, _) = fjp_state_get(task);
            if !done {
                task = fjp_compute_for_submit(ctx, task)?;
            }
            Ok(Some(Value::Object(Some(task))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // submit(Runnable) — run it and wrap as a ForkJoinTask
    r.register(
        pool,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let runnable = obj_arg(args, 1)?;
            let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
            Ok(Some(Value::Object(Some(runnable))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: this pool starts no worker threads — it borrows whichever thread
    // called `invoke`/`submit` and runs the task there — so the number of
    // threads it owns is the one borrowed carrier. HotSpot reports the same 1
    // for a parallelism-1 pool.
    r.register(pool, "getPoolSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // "Threads stealing or executing tasks", read from the live counter that
    // `fjp_compute_object_result`/`fjp_compute_void` maintain. The previous
    // constant 0 was right only between tasks; a `compute()` that asks the
    // pool how busy it is was told "idle" while it was itself running.
    r.register(pool, "getActiveThreadCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(
            FJP_ACTIVE_THREADS.load(std::sync::atomic::Ordering::Relaxed),
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Real queue depth. The constant 0 assumed nothing is ever outstanding,
    // but `fork()` is deliberately LAZY here: it queues the task and returns,
    // and the work only happens at the matching `join()`. Between those two
    // points the task genuinely is queued.
    r.register(pool, "getQueuedTaskCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(fjp_queued_task_count(ctx.vm_identity()))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W2: this returned a constant `true` ("the pool terminated within the
    // timeout") even for a pool nobody had shut down, which now that
    // `isTerminated()` is real would have made the two contradict each other.
    // Answer the same flag: because every task runs inline, a shut-down pool
    // is terminated the instant it is shut down, and a live pool never
    // terminates no matter how long the caller waits.
    r.register(
        pool,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            let (shut, _) = fjp_state_flags(this);
            Ok(Some(Value::Int(i32::from(shut))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinTask — done+result tracked in `fjp_state` side-table.
    // WP4.3 fix: switched off field-index access (broken in real-JDK mode).
    let fjt = "java/util/concurrent/ForkJoinTask";
    // fork() — LAZY: just queue the task. join() / get() / invoke() will
    // drive compute() if not yet done. Eager fork was overflowing the
    // host stack on deeply-recursive RecursiveTask probes (FjpProbe @ 1M
    // elements / depth 10).
    r.register(
        fjt,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(fjt, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_future_get_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "isCancelled", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (_, cancelled) = fjp_state_flags(this);
        Ok(Some(Value::Int(i32::from(cancelled))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "isCompletedNormally", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        // A cancelled task, and a task whose body threw, are both DONE but
        // did NOT complete normally.
        let (done, cancelled) = fjp_state_flags(this);
        let threw = fjp_state_thrown(this).is_some();
        Ok(Some(Value::Int(i32::from(done && !cancelled && !threw))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // The exact complement of `isCompletedNormally` over a DONE task, and NOT
    // `!isCompletedNormally()`: the real `isCompletedAbnormally()` is
    // `(status & ABNORMAL) != 0`, and `ABNORMAL` is only ever set together with
    // `DONE` (`trySetCancelled` -> DONE|ABNORMAL|CANCELLED, `trySetThrown` ->
    // DONE|ABNORMAL|THROWN). A task that has not run yet is therefore neither
    // normally nor abnormally completed, which the naive negation gets wrong.
    r.register(fjt, "isCompletedAbnormally", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, cancelled) = fjp_state_flags(this);
        let threw = fjp_state_thrown(this).is_some();
        Ok(Some(Value::Int(i32::from(done && (cancelled || threw)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "complete", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        fjp_complete_body(ctx, this, val)?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W6-9: `completeExceptionally(Throwable)` — see
    // `fjp_complete_exceptionally_body` for why leaving it unregistered is a
    // silent success rather than a hang. Registered on `ForkJoinTask`,
    // `RecursiveTask` and `RecursiveAction` (the three classes both allow-lists
    // name), same as the real-JDK boot path in
    // `register_real_jdk_forkjoin_essentials`.
    r.register(
        fjt,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ex = args.get(1).copied().unwrap_or(Value::Object(None));
            fjp_complete_exceptionally_body(ctx, this, ex)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // RecursiveTask — done+result tracked in `fjp_state` side-table.
    // WP4.3 fix: switched off field-index access (broken in real-JDK mode).
    let rt = "java/util/concurrent/RecursiveTask";
    // KEEP: genuinely empty, same as the `RecursiveAction` ctor below. The real
    // `RecursiveTask()` constructor has an empty body, and this model keeps all
    // task state in the `fjp_state` side table, which starts empty for an
    // unseen key — so there is nothing to initialise here.
    r.register(rt, "<init>", "()V", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Lazy fork — see ForkJoinTask.fork above for rationale.
    r.register(
        rt,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(rt, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_future_get_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "getRawResult", "()Ljava/lang/Object;", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (_, cached) = fjp_state_get(this);
        Ok(Some(cached))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "setRawResult", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        // setRawResult does NOT mark the task done — it's just a setter
        // called by the framework during task execution.
        let mut m = fjp_state().lock();
        let entry = fjp_entry_in(&mut m, ctx.vm_identity(), this);
        entry.result = val;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "complete", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        fjp_complete_body(ctx, this, val)?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        rt,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ex = args.get(1).copied().unwrap_or(Value::Object(None));
            fjp_complete_exceptionally_body(ctx, this, ex)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // RecursiveAction — done tracked in `fjp_state` side-table; result
    // always Value::Object(None) since compute() returns void.
    // WP4.3 fix: switched off field-index access (broken in real-JDK mode).
    let ra = "java/util/concurrent/RecursiveAction";
    // KEEP: genuinely empty. The real `RecursiveAction()` constructor has an
    // empty body, and this model keeps all task state in the `fjp_state` side
    // table, which starts empty for an unseen key — so there is nothing to
    // initialise here.
    r.register(ra, "<init>", "()V", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Lazy fork — see ForkJoinTask.fork above for rationale.
    r.register(
        ra,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ra, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_void_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_void_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // KEEP: `RecursiveAction.getRawResult()` returns null in the real JDK too —
    // the class exists precisely for tasks whose `compute()` is void, so there
    // is no result to hand back. This constant is the spec.
    r.register(ra, "getRawResult", "()Ljava/lang/Object;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ra,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ex = args.get(1).copied().unwrap_or(Value::Object(None));
            fjp_complete_exceptionally_body(ctx, this, ex)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// WP4.3: real-JDK essentials registration of ForkJoinPool /
/// ForkJoinTask / RecursiveTask / RecursiveAction natives. Called from
/// `lib::register_essential_natives` (the real-JDK boot path).
///
/// Only registers fork/join/invoke/get/isDone/etc. — does NOT register
/// `<init>` or `commonPool` because the real JDK has its own bytecode for
/// those that uses Unsafe machinery we already support through
/// `register_t12_unsafe_natives`. The hot-path fork/join/invoke is what
/// loops infinitely without an override (Unsafe CAS on `status` retries
/// forever in real-JDK mode), so just those are overridden here.
/// The two guards every `ForkJoinPool` submission runs before it does
/// anything, neither of which any of the six submission natives had.
///
/// MEASURED against HotSpot 25.0.4+7, both modes, nine rows
/// (`probes/ForkJoinShadowSweep.java`):
///
/// ```text
///   pool.submit((Callable) null)   HotSpot NullPointerException          CratonVM no-throw
///   pool.execute((Runnable) null)  HotSpot NullPointerException          CratonVM no-throw
///   pool.invoke(null)              HotSpot NullPointerException          CratonVM no-throw
///   pool.shutdown(); pool.submit(..)
///                                  HotSpot RejectedExecutionException    CratonVM no-throw
/// ```
///
/// The second is the one that matters. These natives run the task INLINE on
/// the calling thread, so "the pool is shut down" had no path to the code that
/// decides whether to run it — a caller that had shut its pool down and was
/// draining it still had new work executed, silently, on its own thread.
///
/// The shutdown state is read by INVOKING the real `isShutdown()` rather than
/// by consulting the side table: `ForkJoinPool.shutdown()` is not registered on
/// the real-JDK path (confirmed against `--dump-native-registry`), so real
/// bytecode owns `runState` and the side table never learns about it. That is
/// also why this cannot recurse — there is no native on `isShutdown` to come
/// back here. Anything other than a definite `true` is treated as "not shut
/// down", so a pool this VM cannot interrogate keeps the old behaviour rather
/// than refusing work it would have run.
///
/// `commonPool()` is exempt and must be: `ForkJoinPool.commonPool().shutdown()`
/// is specified as a NO-OP, so the common pool can never be in this state, and
/// a stray `shutdown()` on it must not take out every later user of it.
/// `ForkJoinTask.getThrowableException()` — the same-class COPY a POOL hands
/// back for a task that failed, with the original as its cause.
///
/// The JDK does this so a caller on another thread gets a stack trace that
/// reaches its own frame; the copy's message is `ex.toString()`, which is what
/// `Throwable(Throwable cause)` sets. MEASURED against HotSpot 25.0.4+7,
/// EIGHT runs, and the three shapes do NOT agree with each other:
///
/// ```text
///   pool.invoke(throwing task)         cause present   8/8   <- deterministic
///   task.invoke()                      no cause        8/8   <- deterministic
///   ForkJoinTask.invokeAll(t1, t2)     cause present   3/8   <- NOT deterministic
/// ```
///
/// So this is applied HERE and nowhere else. `ForkJoinTask.invoke()` must NOT
/// copy — HotSpot hands back the task's own throwable there, and this VM
/// already matched. `invokeAll` is a coin HotSpot flips (whether the failing
/// task ran on a worker or was helped inline decides it), so its probe row
/// asserts the exception TYPE and nothing else; pinning either face of that
/// would be a probe reporting the host's load.
///
/// A class with no `(Throwable)` constructor, or a constructor that throws,
/// falls back to the original exception — reporting the failure with a
/// slightly poorer trace beats reporting a different failure.
pub fn fjp_pool_invoke_exception(ctx: &mut dyn NativeContext, ex: ObjectRef) -> MethodCallFailed {
    let ex_pin = ctx.pin_native_root(ex);
    let live = ctx.read_native_pin(ex_pin, ex);
    let class_name = ctx.class_name_arc_of_id(ctx.class_id_of_object(live));
    let built = match class_name {
        Some(name) => ctx.new_object_initialized(
            &name,
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(live))],
        ),
        None => Ok(None),
    };
    let live = ctx.read_native_pin(ex_pin, live);
    ctx.unpin_native_roots(ex_pin);
    match built {
        Ok(Some(Value::Object(Some(copy)))) => MethodCallFailed::ExceptionThrown(copy),
        _ => MethodCallFailed::ExceptionThrown(live),
    }
}

pub fn fjp_reject_submission(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    task_index: usize,
) -> Result<(), MethodCallFailed> {
    if matches!(args.get(task_index), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    if let Some(Value::Object(Some(pool))) = args.first().copied() {
        if fjp_is_common_pool(ctx.vm_identity(), pool) {
            return Ok(());
        }
        if matches!(
            ctx.invoke_virtual(pool, "isShutdown", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        ) {
            return Err(RuntimeError::RejectedExecutionException {
                message: String::new(),
            }
            .into());
        }
    }
    Ok(())
}

pub fn register_real_jdk_forkjoin_essentials(r: &mut NativeMethodRegistry) {
            // ForkJoinPool.invoke(ForkJoinTask) — call task.compute() once and
    // memoise the result in the side-table.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "invoke",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;",
        |ctx, args| {
            fjp_reject_submission(ctx, args, 1)?;
            let task = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let (done, cached) = fjp_state_get(task);
            if done {
                tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(task)), cached = ?cached, "pool.invoke done");
                return Ok(Some(cached));
            }
            // The body runs INSIDE this pool, so it is bracketed by the pool
            // frame `inForkJoinPool()`/`getPool()` read. The closure is what
            // gives the frame exactly one exit path, `?` included.
            let frame = fjp_pool_frame_enter(ctx, args);
            let out = {
                // `ForkJoinPool.invoke(task)` DOES rethrow the task's
                // exception, as a same-class copy — see
                // `fjp_pool_invoke_exception`.
                let (task, outcome) = fjp_compute_and_complete(ctx, task);
                tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(task)), ok = outcome.is_ok(), "pool.invoke result");
                match outcome {
                    Ok(v) => Ok(Some(v)),
                    Err(MethodCallFailed::ExceptionThrown(ex)) => {
                        Err(fjp_pool_invoke_exception(ctx, ex))
                    }
                    Err(internal) => Err(internal),
                }
            };
            fjp_pool_frame_leave(ctx, frame);
            out
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinPool.submit(ForkJoinTask) / externalSubmit(ForkJoinTask) - keep the
    // public real-JDK submit path on the same side-table-backed semantics as
    // invoke(). Letting the concrete JDK submit bytecode enqueue into the real
    // pool exposes WorkQueue/status machinery that CratonVM only partially
    // models on the default real-ForkJoinPool path, and Fork6Hard observes
    // stale task/result objects there under GC stress.
    for submit_name in ["submit", "externalSubmit"] {
        r.register(
            "java/util/concurrent/ForkJoinPool",
            submit_name,
            "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
            |ctx, args| {
                fjp_reject_submission(ctx, args, 1)?;
                let mut task = match args.get(1).copied() {
                    Some(Value::Object(Some(r))) => r,
                    _ => return Ok(args.get(1).copied()),
                };
                let (done, _) = fjp_state_get(task);
                if !done {
                    let frame = fjp_pool_frame_enter(ctx, args);
                    let out = fjp_compute_for_submit(ctx, task);
                    fjp_pool_frame_leave(ctx, frame);
                    task = out?;
                }
                Ok(Some(Value::Object(Some(task))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // ForkJoinPool.execute(ForkJoinTask) — the SAME side-table policy as
    // `submit`/`externalSubmit` directly above, and it was missing.
    //
    // 2026-09-11, lane 5 residual §9.1. `execute` had no registration ANYWHERE
    // and was not on `keep_real_forkjoinpool_bridge`'s allow-list, so
    // `pool.execute(task)` ran the concrete JDK bytecode: the task went into a
    // real `WorkQueue` and a real worker thread ran its body through the JDK's
    // own `doExec()`, which never touches this side table. The caller's
    // following `join()`/`get()` is a Bridge that reads the side table, saw
    // `done == false`, and RAN THE BODY AGAIN — two threads, one task,
    // overlapping. `L5FjDouble.java` scores it per shape and says
    // which of the three double-execution shapes it is.
    //
    // The retired lane page read the symptom as the JDK's own WorkQueue being
    // claimed twice and held 70 `ForkJoinPool`/`ForkJoinTask` rows behind it.
    // It is not the JDK's: it is one half of this pool's surface on the
    // side-table model and the other half on the real one, with no agreement
    // about what "done" means.
    //
    // Why inline rather than teaching `join()` to wait for the real worker:
    // that is the policy `submit` already states three lines up — letting the
    // concrete submit bytecode enqueue into the real pool exposes
    // WorkQueue/status machinery CratonVM only partially models, and Fork6Hard
    // observes stale task/result objects there under GC stress. `execute` is
    // the same call with the return value dropped, so it gets the same answer;
    // a lane that moves `submit` off this model moves `execute` with it.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "execute",
        "(Ljava/util/concurrent/ForkJoinTask;)V",
        |ctx, args| {
            fjp_reject_submission(ctx, args, 1)?;
            let task = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                // `execute(null)` throws NullPointerException on a real pool,
                // and `fjp_reject_submission` above is what raises it — by the
                // time control is here the argument is a non-null task or the
                // call has already failed. Returning void for anything else
                // keeps this arm total without inventing a second policy.
                _ => return Ok(None),
            };
            let (done, _) = fjp_state_get(task);
            if !done {
                let frame = fjp_pool_frame_enter(ctx, args);
                let out = fjp_compute_for_submit(ctx, task);
                fjp_pool_frame_leave(ctx, frame);
                out?;
            }
            // void: `execute` hands nothing back, and a failure surfaces at the
            // matching `join()`/`get()` exactly as `fjp_compute_for_submit`'s
            // own doc comment describes for `submit`.
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinPool.submit(Callable) / submit(Runnable) / submit(Runnable, T) —
    // same Bridge policy as submit(ForkJoinTask) above: these overloads were
    // previously left off the allow-list, so real JDK bytecode ran them
    // against a pool whose `commonPool()` shortcut never populates the real
    // `queues`/`runState`/`mode` fields — `poolSubmit`/`submissionQueue`
    // throws RejectedExecutionException (RealFjp.java repro). Adapt eagerly
    // inline like submit(ForkJoinTask): allocate a bare `ForkJoinTask`
    // instance (no JDK `<init>`/AdaptedCallable machinery needed — nothing
    // ever calls its `compute()`) as the side-table key, run the user
    // callable/runnable synchronously, and mark it done with the result.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            fjp_reject_submission(ctx, args, 1)?;
            let callable = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Shares `fjp_run_callable_as_task` with `invokeAll` so both have
            // one exception model. The old inline version discarded a
            // throwing `call()` (`_ => Value::Object(None)`) and marked the
            // task done with a null result, so `Future.get()` reported
            // success for a callable that had blown up.
            let frame = fjp_pool_frame_enter(ctx, args);
            let out = fjp_run_callable_as_task(ctx, callable);
            fjp_pool_frame_leave(ctx, frame);
            Ok(Some(Value::Object(Some(out?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            fjp_reject_submission(ctx, args, 1)?;
            let runnable = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let frame = fjp_pool_frame_enter(ctx, args);
            let out = fjp_run_runnable_as_task(ctx, runnable, Value::Object(None));
            fjp_pool_frame_leave(ctx, frame);
            Ok(Some(Value::Object(Some(out?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            fjp_reject_submission(ctx, args, 1)?;
            let runnable = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(args.get(2).copied()),
            };
            let fixed_result = args.get(2).copied().unwrap_or(Value::Object(None));
            let frame = fjp_pool_frame_enter(ctx, args);
            let out = fjp_run_runnable_as_task(ctx, runnable, fixed_result);
            fjp_pool_frame_leave(ctx, frame);
            Ok(Some(Value::Object(Some(out?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // ForkJoinPool.invokeAll(Collection) / invokeAll(Collection, long, TimeUnit)
    // / invokeAllUninterruptibly(Collection).
    //
    // `invokeAll(Collection)` is the overload Weld's `ConcurrentBeanDeployer`
    // -> `AbstractExecutorServices.invokeAllAndCheckForExceptions` calls, and
    // it was never on the allow-list — so it fell through to real JDK bytecode
    // against the under-initialized `commonPool()` bridge object and threw
    // `RejectedExecutionException` at `submissionQueue()`, failing the entire
    // `org.hibernate.orm.test.cdi.*` / `jpa.cdi.*` cluster plus
    // `filter.FilterParameterTests`. Same eager-inline model as
    // `submit(Callable)`: run each callable synchronously, collect the
    // completed tasks as the returned `List<Future<T>>`.
    //
    // The timed and uninterruptible variants share the implementation: every
    // task has already completed by the time this returns, so a timeout can
    // never elapse and there is no interruptible wait to differ over.
    for invoke_all_desc in [
        "(Ljava/util/Collection;)Ljava/util/List;",
        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/util/List;",
    ] {
        r.register(
            "java/util/concurrent/ForkJoinPool",
            "invokeAll",
            invoke_all_desc,
            |ctx, args| {
                let tasks = obj_arg(args, 1)?;
                fjp_invoke_all(ctx, tasks)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "invokeAllUninterruptibly",
        "(Ljava/util/Collection;)Ljava/util/List;",
        |ctx, args| {
            let tasks = obj_arg(args, 1)?;
            fjp_invoke_all(ctx, tasks)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinPool.invokeAny(Collection) / invokeAny(Collection, long, TimeUnit).
    //
    // Uncovered for the same reason as invokeAll, but it failed WORSE: real
    // bytecode ran the callables and then returned null instead of throwing,
    // so callers saw a silent wrong answer rather than an error. (The
    // `invokeAll` doc flagged invokeAny as "not checked"; a matrix probe
    // diffed against the host JDK is what caught it.)
    for invoke_any_desc in [
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
    ] {
        r.register(
            "java/util/concurrent/ForkJoinPool",
            "invokeAny",
            invoke_any_desc,
            |ctx, args| {
                let tasks = obj_arg(args, 1)?;
                fjp_invoke_any(ctx, tasks)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // ForkJoinPool.lazySubmit(ForkJoinTask) — same uncovered-overload story;
    // it threw RejectedExecutionException. The JDK's lazySubmit differs from
    // submit only in NOT signalling a worker to pick the task up, which is
    // meaningless for a pool that runs everything inline.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "lazySubmit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let mut task = obj_arg(args, 1)?;
            let (done, _) = fjp_state_get(task);
            if !done {
                task = fjp_compute_for_submit(ctx, task)?;
            }
            Ok(Some(Value::Object(Some(task))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinTask.getException() — reads the same recorded throwable that
    // `join()`/`get()` replay, so the three cannot disagree about whether a
    // task failed.
    for task_class in [
        "java/util/concurrent/ForkJoinTask",
        "java/util/concurrent/RecursiveTask",
        "java/util/concurrent/RecursiveAction",
    ] {
        // `getException()` is registered by
        // `phases_late::concurrent::register_forkjointask_w6_9_residual_bridge`,
        // which runs later on both boot paths — the body that used to be here
        // answered null for a cancelled task, contradicting the
        // `isCompletedAbnormally()` that reads the same side table (W6-9 §7.1).
        // Do not re-add one: registration is last-write-wins, so a copy here
        // would look live and be dead, and would also cost the duplicate-
        // registration gate three shadowed rows per task class.
        //
        // W6-9: `completeExceptionally(Throwable)` — the WRITER for the record
        // `getException()` reads. It was registered nowhere and named in
        // neither allow-list, so real bytecode CASed the real `aux`/`status`
        // fields that nothing here reads: the task stayed `done == false`, the
        // next `join()` ran its body, and the trio `getException()` /
        // `isCompletedAbnormally()` / `isCompletedNormally()` all reported a
        // clean task the caller had explicitly failed. See
        // `fjp_complete_exceptionally_body`. Must stay in step with
        // `keep_real_forkjointask_bridge` (native-api/src/registry.rs) and
        // `is_forkjoin_native_override`
        // (vm/src/runtime/interpreter/native_override.rs), entry for entry.
        r.register(
            task_class,
            "completeExceptionally",
            "(Ljava/lang/Throwable;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let ex = args.get(1).copied().unwrap_or(Value::Object(None));
                fjp_complete_exceptionally_body(ctx, this, ex)?;
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // `awaitQuiescence` must observe the FutureTask roots created by the
    // real-worker `execute(Runnable)` bridge. The old constant true could say
    // the pool was idle while a worker was blocked inside user code.
    //
    // THIS REGISTRAR IS THE ONLY ONE THAT RUNS UNDER `--synthetic-jdk`:
    // `vm_init`'s `if config.use_synthetic_jdk` arm calls `register_builtins`
    // (which reaches here through `register_essential_natives`) and never
    // calls `lib::register_forkjoin_quiescence`. On the real-JDK arms BOTH
    // run, and `register` is last-write-wins — so the two must install the
    // same callback or the answer would depend on which arm booted. They used
    // to be two independently-written bodies. `native_forkjoin_await_quiescence`
    // is now the single implementation, and it consults BOTH the tracked
    // `FutureTask` roots this body used to poll and the `ASYNC_POOL` executor
    // the other one did.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "awaitQuiescence",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        crate::native_forkjoin_await_quiescence, cratonvm_native_api::NativeKind::Bridge,
    );

    // `ForkJoinTask.inForkJoinPool()` / `getPool()`. Both are STATIC and both
    // were unregistered, so real bytecode answered them from
    // `Thread.currentThread() instanceof ForkJoinWorkerThread` — which is
    // false on this VM even inside `pool.invoke`, because the pool runs its
    // tasks inline on the submitting thread. See `FJP_POOL_STACK` for the
    // measurement and for why the pair matters.
    //
    // Registered on all three task classes for the same
    // belt-and-braces reason `register_forkjointask_w6_9_residual_bridge`
    // gives: both are `public static` on `ForkJoinTask`, so real-JDK
    // resolution always lands there, but synthetic-mode lookup is per class
    // name.
    for task_class in [
        "java/util/concurrent/ForkJoinTask",
        "java/util/concurrent/RecursiveTask",
        "java/util/concurrent/RecursiveAction",
    ] {
        r.register(task_class, "inForkJoinPool", "()Z", |_ctx, _args| {
            Ok(Some(Value::Int(i32::from(fjp_in_pool()))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(
            task_class,
            "getPool",
            "()Ljava/util/concurrent/ForkJoinPool;",
            |ctx, _args| Ok(Some(Value::Object(fjp_current_pool(&*ctx)))), cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // ForkJoinTask.fork / join / invoke / get / isDone / isCompletedNormally /
    // isCancelled / cancel / complete — all routed through the side-table.
    //
    // FjpProbe fix: lazy fork (see RecursiveTask above) — fork() is a
    // marker only; join()/get()/invoke() drive compute() when not done.
    let fjt = "java/util/concurrent/ForkJoinTask";
    r.register(
        fjt,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(this)), "fjt.fork (lazy)");
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(fjt, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_future_get_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    // get(long, TimeUnit) — `final` in the real JDK (declaring class is always
    // ForkJoinTask, even for RecursiveTask/RecursiveAction receivers), needed
    // by `Future<T>.get(timeout, unit)` callers (e.g. RealFjp.java). Every
    // task this Bridge produces is already eagerly computed inline, so the
    // timeout never actually applies — same semantics as the no-arg get().
    r.register(
        fjt,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            fjp_future_get_body(ctx, this)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(fjt, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "isCompletedNormally", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        // A cancelled task, and a task whose body threw, are both DONE but
        // did NOT complete normally.
        let (done, cancelled) = fjp_state_flags(this);
        let threw = fjp_state_thrown(this).is_some();
        Ok(Some(Value::Int(i32::from(done && !cancelled && !threw))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `isCompletedAbnormally()` — was registered NOWHERE and named in NEITHER
    // allow-list, so it ran real bytecode: `(status & ABNORMAL) != 0` over the
    // real `status` field, which no Bridge here ever writes. Every task this VM
    // completes is completed in the `fjp_state` side table instead, so the real
    // field stays 0 and the method answered `false` for a task that had just
    // handed its caller an `ExecutionException`
    // (`RJdkForkJoin.java:259`/`:294`).
    //
    // `ABNORMAL` is only ever set together with `DONE` in the real status word
    // (`trySetCancelled` -> DONE|ABNORMAL|CANCELLED, `trySetThrown` ->
    // DONE|ABNORMAL|THROWN), which is why this is the complement of
    // `isCompletedNormally` *over a done task* rather than its plain negation:
    // a task that has not run yet is neither.
    //
    // Registered on `ForkJoinTask` only, exactly like the sibling
    // `isCompletedNormally`/`isCancelled` above: all three are `public final`
    // in the real `ForkJoinTask`, so an `invokevirtual` on a `RecursiveTask` /
    // `RecursiveAction` / user receiver still resolves its declaring class to
    // `java/util/concurrent/ForkJoinTask`.
    r.register(fjt, "isCompletedAbnormally", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, cancelled) = fjp_state_flags(this);
        let threw = fjp_state_thrown(this).is_some();
        Ok(Some(Value::Int(i32::from(done && (cancelled || threw)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "isCancelled", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (_, cancelled) = fjp_state_flags(this);
        Ok(Some(Value::Int(i32::from(cancelled))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(fjt, "complete", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        fjp_complete_body(ctx, this, val)?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // RecursiveTask — same as ForkJoinTask plus getRawResult / setRawResult.
    //
    // FjpProbe fix: switch to LAZY fork. Eager fork (running compute()
    // inline at fork time) causes O(depth)-style Rust call stack growth on
    // deeply recursive RecursiveTask hierarchies — for the 1M-element
    // FjpProbe (depth 10) this overflowed the worker thread's stack.
    //
    // Lazy fork records the task in the side-table marker `done=false`
    // (untouched), and `join()` computes it if not yet done. Because the
    // user's typical pattern is `l.fork(); r.compute(); l.join();`, lazy
    // fork still produces correct results — `l.compute()` runs at
    // `l.join()` time after `r.compute()` has finished and freed its
    // recursion stack.
    let rt = "java/util/concurrent/RecursiveTask";
    r.register(
        rt,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            tracing::debug!(target: "fjp", task = format!("0x{:x}", fjp_key(this)), "rt.fork (lazy)");
            // Lazy: do not eagerly compute. The next `join()` / `get()` /
            // `invoke()` on this task will run `compute()` if not done.
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(rt, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_future_get_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "getRawResult", "()Ljava/lang/Object;", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (_, cached) = fjp_state_get(this);
        Ok(Some(cached))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "setRawResult", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        let mut m = fjp_state().lock();
        let entry = fjp_entry_in(&mut m, ctx.vm_identity(), this);
        entry.result = val;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(rt, "complete", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).copied().unwrap_or(Value::Object(None));
        fjp_complete_body(ctx, this, val)?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // RecursiveAction (compute returns void).
    // Lazy fork — see ForkJoinTask.fork above for rationale.
    let ra = "java/util/concurrent/RecursiveAction";
    r.register(
        ra,
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            fjp_state_mark_queued_in_vm(ctx.vm_identity(), this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ra, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_void_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "invoke", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fjp_join_void_body(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "isDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let (done, _) = fjp_state_get(this);
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ra, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(fjp_cancel_and_stamp(ctx, this)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // KEEP: `RecursiveAction.getRawResult()` returns null in the real JDK too —
    // the class exists precisely for tasks whose `compute()` is void, so there
    // is no result to hand back. This constant is the spec.
    r.register(ra, "getRawResult", "()Ljava/lang/Object;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ForkJoinPool.commonPool() — return a real ForkJoinPool object.
    // The real JDK bytecode for commonPool() goes through Unsafe-backed
    // singleton initialization that is fragile in our environment; we
    // just allocate a synthetic ForkJoinPool with parallelism=N and let
    // the user code call our intercepted invoke().
    // TODO: this allocates a FRESH pool object on every call instead of
    // caching a true singleton (unlike the real JDK, where commonPool()
    // always returns the same instance). Harmless for the Bridge-covered
    // methods above (state lives in the side-table, keyed by task/adapter
    // identity, not by pool identity), but `pool1 == pool2` / identity-based
    // pool bookkeeping in user code would observe distinct objects.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "commonPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
        |ctx, _args| {
            let p = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ForkJoinPool", 1)?;
            // Field 0 is "parallelism" in synthetic mode; in real-JDK
            // mode the real fields are populated by the real JDK
            // <clinit>. The native invoke() above uses the side-table
            // exclusively, so this field has no functional effect.
            ctx.set_field(p, 0, Value::Int(FJP_COMMON_PARALLELISM));
            Ok(Some(Value::Object(Some(p))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ForkJoinPool.getParallelism — simple stub for callers that read the
    // pool's worker count without going through the real JDK initialisation.
    r.register(
        "java/util/concurrent/ForkJoinPool",
        "getParallelism",
        "()I",
        |_ctx, _args| {
            // Match HotSpot defaults: max(1, cpus - 1).
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4) as i32;
            Ok(Some(Value::Int(cpus.saturating_sub(1).max(1))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// ScheduledExecutorService — stubs
// ---------------------------------------------------------------------------

pub fn stpe_new_delayed_work_queue(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    match ctx.new_object_initialized(
        "java/util/concurrent/ScheduledThreadPoolExecutor$DelayedWorkQueue",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(q)))) => Some(q),
        _ => match ctx.new_object_initialized(
            "java/util/concurrent/PriorityBlockingQueue",
            "()V",
            &[],
        ) {
            Ok(Some(Value::Object(Some(q)))) => Some(q),
            _ => None,
        },
    }
}

pub fn stpe_time_unit_millis(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/TimeUnit")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, "MILLISECONDS")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(unit)) => Some(unit),
        _ => None,
    }
}

pub fn stpe_legacy_slot_init(ctx: &mut dyn NativeContext, this: ObjectRef, cores: i32) {
    let cores = cores.max(1);
    if ctx.object_num_fields(this) > 0 {
        ctx.set_field(this, 0, Value::Int(cores));
    }
    if ctx.object_num_fields(this) > 1 {
        ctx.set_field(this, 1, Value::Int(0));
    }
}

/// Which real `BlockingQueue` a plain `ThreadPoolExecutor` built by an
/// `Executors.*` factory shortcut should use — mirrors the queue type real
/// JDK bytecode picks for each factory (see `java.util.concurrent.Executors`).
pub enum TpeQueueKind {
    /// `newFixedThreadPool`/`newSingleThreadExecutor`: unbounded `LinkedBlockingQueue`.
    Linked,
    /// `newCachedThreadPool`: zero-capacity rendezvous `SynchronousQueue`.
    Synchronous,
}

pub fn tpe_new_queue(ctx: &mut dyn NativeContext, kind: TpeQueueKind) -> Option<ObjectRef> {
    let class_name = match kind {
        TpeQueueKind::Linked => "java/util/concurrent/LinkedBlockingQueue",
        TpeQueueKind::Synchronous => "java/util/concurrent/SynchronousQueue",
    };
    match ctx.new_object_initialized(class_name, "()V", &[]) {
        Ok(Some(Value::Object(Some(q)))) => Some(q),
        _ => None,
    }
}

pub fn tpe_time_unit_by_name(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/TimeUnit")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, name)?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(unit)) => Some(unit),
        _ => None,
    }
}

