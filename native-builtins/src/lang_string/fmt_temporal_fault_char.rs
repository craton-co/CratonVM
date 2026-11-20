#[allow(unused_imports)]
use super::*;

/// The character `IllegalFormatConversionException` reports for `field` on a
/// source with this support, or `None` when the field is answerable.
///
/// The reported character is NOT always the one written in the format string:
/// the composite conversions delegate to nested `print` calls and it is the
/// INNER field that raises, so `%tT` of a `LocalDate` reports `H` and `%tc` of
/// an `Instant` reports `a`. Every row below was measured on HotSpot 25:
///
/// | source | `%tR` `%tT` | `%tr` | `%tD` | `%tF` | `%tc` |
/// |---|---|---|---|---|---|
/// | `Instant` | `H` | `I` | `m` | `F` | `a` |
/// | `LocalDate` | `H` | `I` | ok | ok | `H` |
/// | `LocalTime` | ok | ok | `m` | `F` | `a` |
/// | `LocalDateTime` | ok | ok | ok | ok | `Z` |
/// | `OffsetTime` | ok | ok | `m` | `F` | `a` |
/// | `Year` | `H` | `I` | `m` | `m` | `a` |
/// | `YearMonth` | `H` | `I` | `d` | `d` | `a` |
/// | `MonthDay` | `H` | `I` | `y` | `F` | `a` |
/// | `Month` | `H` | `I` | `d` | `F` | `a` |
///
/// The last four rows are G2-1's, measured 2026-08-16, and they are what
/// forces the three composites to walk their inner fields in ORDER rather than
/// consult a single `date` flag: `%tD` is `mm/dd/yy` and reports `m`, `d` or
/// `y`; `%tF` is `YYYY-mm-dd` and reports its OWN `F` only when the YEAR is
/// the missing one (`ISO_STANDARD_DATE` reads the year with `t.get(yearField)`
/// inline instead of delegating), then `m`, then `d`.
pub fn fmt_temporal_fault_char(field: char, s: FmtSupport) -> Option<char> {
    fn need(ok: bool, c: char) -> Option<char> {
        if ok {
            None
        } else {
            Some(c)
        }
    }
    match field {
        'H' | 'k' | 'I' | 'l' | 'M' | 'S' | 'p' => need(s.time, field),
        'L' | 'N' => need(s.sub_second, field),
        's' => need(s.instant, field),
        // `MILLISECOND_SINCE_EPOCH` reads INSTANT_SECONDS *and*
        // MILLI_OF_SECOND, so it needs both — and every source that has the
        // first here also has the second, which is why the row is not
        // separately observable.
        'Q' => need(s.instant && s.sub_second, field),
        'z' | 'Z' => need(s.zone, field),
        'B' | 'b' | 'h' | 'm' => need(s.month, field),
        'd' | 'e' => need(s.day, field),
        'C' | 'Y' | 'y' => need(s.year, field),
        // DAY_OF_WEEK — see [`FmtSupport::weekday`]: `full_date()`, plus the
        // one measured exception (`java.time.DayOfWeek`).
        'A' | 'a' => need(s.weekday(), field),
        // DAY_OF_YEAR needs a real date; unlike `%tA`/`%ta` it has no source
        // that answers it without one, so it stays tied to `full_date()`.
        'j' => need(s.full_date(), field),
        'R' | 'T' => need(s.time, 'H'),
        'r' => need(s.time, 'I'),
        // `mm/dd/yy`, in that order.
        'D' => {
            if !s.month {
                Some('m')
            } else if !s.day {
                Some('d')
            } else if !s.year {
                Some('y')
            } else {
                None
            }
        }
        // `YYYY-mm-dd`, in that order, and the year slot reports the OUTER
        // character.
        'F' => {
            if !s.year {
                Some(field)
            } else if !s.month {
                Some('m')
            } else if !s.day {
                Some('d')
            } else {
                None
            }
        }
        // `a`(DAY_OF_WEEK) `b`(month) `d`(day) `T`(->`H`, time) `Z`(zone)
        // `Y`(year), in that order, so the FIRST unsupported group names the
        // character.
        'c' => {
            if !s.weekday() {
                Some('a')
            } else if !s.month {
                Some('b')
            } else if !s.day {
                Some('d')
            } else if !s.time {
                Some('H')
            } else if !s.zone {
                Some('Z')
            } else if !s.year {
                Some('Y')
            } else {
                None
            }
        }
        // Not a field this VM implements; `format_temporal_field`'s own
        // catch-all raises `UnknownFormatConversionException` for it, which is
        // a question about the SPECIFIER and outranks a support question.
        _ => None,
    }
}

thread_local! {
    pub static CACHED_DEFAULT_ZONE: std::cell::RefCell<Option<(usize, u64, i64, FmtZone)>> =
        const { std::cell::RefCell::new(None) };
    pub static CACHED_DEFAULT_ZONE_RULES: std::cell::RefCell<
        Option<(usize, u64, std::sync::Arc<crate::tzdb::ZoneRulesData>)>,
    > = const { std::cell::RefCell::new(None) };
}

/// Resolve the `java.util.TimeZone` for an epoch-shaped `%t` argument and read
/// its offset at `millis`.
///
/// `calendar` is `Some(cal)` for a `java.util.Calendar` argument, whose own zone
/// is authoritative; `None` takes `TimeZone.getDefault()`, which is what
/// `Calendar.getInstance()` would have given the JDK.
///
/// Returns [`FmtZone::NONE`] rather than failing if the class is not reachable:
/// a VM without `java.util.TimeZone` keeps the previous UTC behaviour instead of
/// refusing a conversion that used to work.
pub fn fmt_resolve_zone(
    ctx: &mut dyn NativeContext,
    calendar: Option<cratonvm_types::ObjectRef>,
    millis: i64,
) -> FmtZone {
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let minute_bucket = millis.div_euclid(60_000);
    if calendar.is_none() {
        if let Some(zone) = CACHED_DEFAULT_ZONE_RULES.with(|c| {
            let borrow = c.borrow();
            if let Some((c_vm, c_epoch, ref rules)) = *borrow {
                if c_vm == vm && c_epoch == epoch {
                    let (total_ms, dst_ms) = crate::tzdb::legacy_offsets_ms_of(rules, millis);
                    return Some(FmtZone {
                        offset_ms: total_ms,
                        dst: dst_ms != 0,
                        known: true,
                        temporal: false,
                    });
                }
            }
            None
        }) {
            return zone;
        }
        if let Some((c_vm, c_epoch, c_bucket, c_zone)) = CACHED_DEFAULT_ZONE.with(|c| *c.borrow()) {
            if c_vm == vm && c_epoch == epoch && c_bucket == minute_bucket {
                return c_zone;
            }
        }
    }
    let tz = match calendar {
        Some(cal) => match ctx.invoke_virtual(cal, "getTimeZone", "()Ljava/util/TimeZone;", &[]) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return FmtZone::NONE,
        },
        None => match ctx.invoke(
            "java/util/TimeZone",
            "getDefault",
            "()Ljava/util/TimeZone;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return FmtZone::NONE,
        },
    };
    if calendar.is_none() {
        let tz_cid = ctx.class_id_of_object(tz);
        let is_vm_tz = ctx.class_id_by_name("sun/util/calendar/ZoneInfo") == Some(tz_cid)
            || ctx.class_id_by_name("java/util/TimeZone") == Some(tz_cid);
        if is_vm_tz {
            if let Some(id_slot) = ctx
                .class_id_by_name("java/util/TimeZone")
                .and_then(|c| ctx.resolve_field_index_by_class_id(c, "ID"))
            {
                if let Value::Object(Some(id_obj)) = ctx.get_field(tz, id_slot) {
                    if let Some(id_str) = ctx.read_string(id_obj) {
                        if let Some(rules) = crate::tzdb::get_zone_rules(ctx, &id_str) {
                            let (total_ms, dst_ms) =
                                crate::tzdb::legacy_offsets_ms_of(&rules, millis);
                            CACHED_DEFAULT_ZONE_RULES.with(|c| {
                                *c.borrow_mut() = Some((vm, epoch, rules));
                            });
                            return FmtZone {
                                offset_ms: total_ms,
                                dst: dst_ms != 0,
                                known: true,
                                temporal: false,
                            };
                        }
                    }
                }
            }
        }
    }
    // `getOffset(long)` is ZONE_OFFSET + DST_OFFSET together; the raw offset is
    // ZONE_OFFSET alone, so the difference IS DST_OFFSET. Asking the zone rather
    // than reading a Calendar field keeps the `long`/`Date` and `Calendar`
    // sources on one code path.
    // `getOffset()` is real Java and can move `tz` before `getRawOffset()`.
    let tz_pin = ctx.pin_native_root(tz);
    let total = match ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return FmtZone::NONE,
    };
    let tz = ctx.read_native_pin(tz_pin, tz);
    let raw = match ctx.invoke_virtual(tz, "getRawOffset", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        // A zone that answered its offset but not its raw offset is still a
        // usable zone; only the DST *name* selection degrades.
        _ => total,
    };
    let zone = FmtZone {
        offset_ms: total,
        dst: total != raw,
        known: true,
        temporal: false,
    };
    if calendar.is_none() {
        CACHED_DEFAULT_ZONE.with(|c| {
            *c.borrow_mut() = Some((vm, epoch, minute_bucket, zone));
        });
    }
    zone
}

/// The offset a `java.time.ZonedDateTime` / `OffsetDateTime` carries, as a
/// [`FmtZone`].
///
/// `getOffset()` is declared on BOTH and returns a `ZoneOffset`, whose
/// `getTotalSeconds()` is `ChronoField.OFFSET_SECONDS` — which is exactly what
/// `Formatter`'s `ZONE_NUMERIC` arm reads. Measured on HotSpot 25: `%tz` of an
/// `Instant.atZone("Asia/Tokyo")` is `+0900`, of `atZone("Asia/Kolkata")`
/// `+0530`, of `atOffset(-3)` `-0300`, of `atOffset(-3,-30)` `-0330`, and of
/// `atOffset(UTC)` `+0000` — none of which move with `user.timezone`.
///
/// `dst` is left false and is not read for these: the daylight question is
/// asked of the object's own zone in [`fmt_temporal_zone_name`], which is also
/// the only consumer of it here. `known` is true so the `'z'` arm renders the
/// real offset instead of the fixed `+0000`.
pub fn fmt_temporal_zone(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> FmtZone {
    let off = match ctx.invoke_virtual(obj, "getOffset", "()Ljava/time/ZoneOffset;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return FmtZone::NONE,
    };
    let secs = match ctx.invoke_virtual(off, "getTotalSeconds", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return FmtZone::NONE,
    };
    FmtZone {
        offset_ms: secs.saturating_mul(1000),
        dst: false,
        known: true,
        temporal: true,
    }
}

/// `%tZ` — `tz.getDisplayName(DST_OFFSET != 0, TimeZone.SHORT, l)`.
///
/// Re-resolves the `TimeZone` from the argument instead of carrying an
/// `ObjectRef` in [`FmtZone`]: everything between the field extraction and this
/// call runs Java bytecode (`fmt_date_name` alone invokes three methods for
/// `%tc`), and a held reference across that is a moved-object hazard. `val`
/// must be CURRENT on entry: [`format_temporal_field`] pins the argument and
/// re-derives it immediately before each call here (gen r4w3/rooting — being
/// the caller's varargs element keeps it alive, but not at the same address).
///
/// `None` means "no display name available" and the caller falls back to the
/// numeric `GMT±HH:MM` form, which is also what HotSpot itself prints under a
/// locale with no short name for the zone (measured: `Locale.ROOT` gives
/// `GMT-05:00` where `Locale.US` gives `EST`).
pub fn fmt_zone_display_name(
    ctx: &mut dyn NativeContext,
    val: &Value,
    zone: FmtZone,
    locale: FmtLocale,
) -> Option<String> {
    if !zone.known {
        return None;
    }
    // A `java.time` source names its own zone; there is no `TimeZone.getDefault`
    // in that answer at all.
    if zone.temporal {
        return fmt_temporal_zone_name(ctx, val, locale);
    }
    let calendar = fmt_calendar_arg(ctx, val);
    let tz = match calendar {
        Some(cal) => match ctx.invoke_virtual(cal, "getTimeZone", "()Ljava/util/TimeZone;", &[]) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return None,
        },
        None => match ctx.invoke(
            "java/util/TimeZone",
            "getDefault",
            "()Ljava/util/TimeZone;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(z)))) => z,
            _ => return None,
        },
    };
    fmt_zone_name_of(ctx, tz, zone.dst, locale)
}

/// `tz.getDisplayName(daylight, TimeZone.SHORT, l)` with this file's
/// [`FmtLocale`] resolution — the one body both `%tZ` name lookups share so
/// they cannot answer the locale question two ways.
///
/// `TimeZone.SHORT` is 0. `Given(None)` takes `Locale.US`, which is the JDK's
/// own `Objects.requireNonNullElse(l, Locale.US)` — the same rule
/// [`fmt_date_name`] now applies, and the reason it can no longer be reached
/// by falling through to the two-argument overload (whose locale is the
/// DEFAULT). If `Locale.US` cannot be read — an uninitialized
/// `java.util.Locale`, a build with no such field — this degrades to that
/// two-argument overload, i.e. to exactly the answer it gave before, rather
/// than refusing.
pub fn fmt_zone_name_of(
    ctx: &mut dyn NativeContext,
    tz: cratonvm_types::ObjectRef,
    dst: bool,
    locale: FmtLocale,
) -> Option<String> {
    const TZ_SHORT: i32 = 0;
    let explicit = match locale {
        // gen r4w3/rooting: current address through the pin.
        FmtLocale::Given(Some(l)) => Some(l.current(&*ctx)),
        FmtLocale::Given(None) => fmt_locale_us(ctx),
        FmtLocale::DefaultFormat => None,
    };
    let name = match explicit {
        Some(l) => ctx.invoke_virtual(
            tz,
            "getDisplayName",
            "(ZILjava/util/Locale;)Ljava/lang/String;",
            &[
                Value::Int(i32::from(dst)),
                Value::Int(TZ_SHORT),
                Value::Object(Some(l)),
            ],
        ),
        None => ctx.invoke_virtual(
            tz,
            "getDisplayName",
            "(ZI)Ljava/lang/String;",
            &[Value::Int(i32::from(dst)), Value::Int(TZ_SHORT)],
        ),
    };
    match name {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// `Locale.US`, read as a static field.
///
/// The JDK spells the explicit-null locale `Objects.requireNonNullElse(l,
/// Locale.US)` in five places, and the four name-array ones are served by
/// [`fmt_date_name`]'s English tables without needing a `Locale` object at
/// all. `%tZ`'s display name cannot be tabulated, so this is the one site that
/// needs the object itself.
///
/// A static-field READ, not a class load and not `Locale.of("en","US")`: this
/// runs no bytecode, allocates nothing, and answers `None` — degrading to the
/// caller's previous behaviour — on any configuration where `java.util.Locale`
/// is absent, is not yet initialized, or has no such field. Nothing here may
/// fabricate a `Locale`; a wrong one would silently rename every zone.
pub fn fmt_locale_us(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ObjectRef> {
    let cid = ctx.class_id_by_name("java/util/Locale")?;
    let idx = ctx.static_field_index_by_name(cid, "US")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(l)) => Some(l),
        _ => None,
    }
}

/// `%tZ` for a `java.time.ZonedDateTime` / `OffsetDateTime` — the
/// `DateTime.ZONE` arm of `print(Formatter, TemporalAccessor, char, Locale)`,
/// which is a genuinely different lookup from the `TimeZone.getDefault()` one
/// above:
///
/// ```text
/// ZoneId zid = t.query(TemporalQueries.zone());
/// if (zid == null) throw new IllegalFormatConversionException(c, t.getClass());
/// if (!(zid instanceof ZoneOffset) && t.isSupported(INSTANT_SECONDS)) {
///     ... TimeZone.getTimeZone(zid.getId())
///                 .getDisplayName(zid.getRules().isDaylightSavings(instant),
///                                 TimeZone.SHORT, requireNonNullElse(l, US));
/// }
/// sb.append(zid.getId());
/// ```
///
/// So a FIXED-offset zone prints its own id and never a display name, and a
/// REGION zone prints the `java.util.TimeZone` short name. Measured on
/// HotSpot 25, at `1699999999000L`, under `-Duser.timezone=Asia/Kolkata` (the
/// default zone is visible in none of them):
///
/// | argument | `Locale.ROOT` | `Locale.US` | `(Locale) null` |
/// |---|---|---|---|
/// | `atZone("Asia/Tokyo")` | `GMT+09:00` | `JST` | `JST` |
/// | `atZone("Asia/Kolkata")` | `GMT+05:30` | `IST` | |
/// | `atZone("UTC")` | `UTC` | | |
/// | `atZone(ZoneOffset.ofHours(5))` | `+05:00` | | |
/// | `atOffset(ZoneOffset.UTC)` | `Z` | | |
/// | `atOffset(-3)` | `-03:00` | | |
/// | `atOffset(-3,-30)` | `-03:30` | | |
/// | `atZone("America/New_York")` in **July** | | `EDT` | |
/// | `atZone("America/New_York")` in November | | `EST` | |
///
/// The `Z` row is why the id is taken from the object rather than composed
/// from the offset: `ZoneOffset.UTC.getId()` is the single letter `Z`, and
/// `+05:00` / `-03:30` are colon-separated where `%tz` is not.
///
/// The DST flag is computed the way [`fmt_resolve_zone`] computes it —
/// `tz.getOffset(millis) != tz.getRawOffset()` — rather than through
/// `ZoneRules.isDaylightSavings`, so the two `%tZ` routes ask the daylight
/// question exactly once, in one form. The July/November New York rows are the
/// pair that would separate a right answer from a hard-coded one.
///
/// `None` on any failure, and the caller falls back to
/// [`fmt_zone_gmt_form`] over the offset already read — which IS HotSpot's own
/// `Locale.ROOT` answer for a region zone (the `GMT+09:00` cell above).
pub fn fmt_temporal_zone_name(
    ctx: &mut dyn NativeContext,
    val: &Value,
    locale: FmtLocale,
) -> Option<String> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    let obj = *obj;
    // GC: every call below runs bytecode and allocates, so the argument and the
    // `ZoneId` are pinned and re-derived across each one. `pin_base` is the
    // FIRST handle, so one `unpin_native_roots(pin_base)` releases the batch.
    let pin_base = ctx.pin_native_root(obj);
    // `ZonedDateTime.getZone()` is the REGION it was built with, which is what
    // `TemporalQueries.zone()` answers for it; `OffsetDateTime` has NO
    // `getZone()` at all and its `getOffset()` is itself a `ZoneId`. The class
    // is tested rather than the call being tried and allowed to fail, because
    // a speculative `getZone` on an `OffsetDateTime` is a `NoSuchMethodError`
    // raised and discarded on every `%tZ`.
    let is_zdt = match ctx.class_id_by_name("java/time/ZonedDateTime") {
        Some(z) => {
            ctx.class_id_of_object(obj) == z || ctx.is_subclass(ctx.class_id_of_object(obj), z)
        }
        None => false,
    };
    let (method, descriptor) = if is_zdt {
        ("getZone", "()Ljava/time/ZoneId;")
    } else {
        ("getOffset", "()Ljava/time/ZoneOffset;")
    };
    let call = ctx.invoke_virtual(obj, method, descriptor, &[]);
    let zid = match call {
        Ok(Some(Value::Object(Some(z)))) => Some(z),
        _ => None,
    };
    let Some(zid) = zid else {
        ctx.unpin_native_roots(pin_base);
        return None;
    };
    let zid_pin = ctx.pin_native_root(zid);
    let out = fmt_temporal_zone_name_inner(ctx, obj, pin_base, zid, zid_pin, locale);
    ctx.unpin_native_roots(pin_base);
    out
}

/// The pinned body of [`fmt_temporal_zone_name`], split out so that every early
/// return goes through its caller's single `unpin_native_roots`.
pub fn fmt_temporal_zone_name_inner(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    obj_pin: usize,
    zid: cratonvm_types::ObjectRef,
    zid_pin: usize,
    locale: FmtLocale,
) -> Option<String> {
    let zid = ctx.read_native_pin(zid_pin, zid);
    // A class-IDENTITY test, not `is_subclass`: `java.time.ZoneOffset` is
    // final, and the JDK's own screen is `zid instanceof ZoneOffset`.
    let fixed = match ctx.class_id_by_name("java/time/ZoneOffset") {
        Some(off) => ctx.class_id_of_object(zid) == off,
        None => false,
    };
    if fixed {
        // `sb.append(zid.getId())` — `Z`, `+05:00`, `-03:30`.
        return match ctx.invoke_virtual(zid, "getId", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|s| !s.is_empty()),
            _ => None,
        };
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    let inst = match ctx.invoke_virtual(obj, "toInstant", "()Ljava/time/Instant;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => return None,
    };
    let millis = match ctx.invoke_virtual(inst, "toEpochMilli", "()J", &[]) {
        Ok(Some(Value::Long(v))) => v,
        _ => return None,
    };
    let zid = ctx.read_native_pin(zid_pin, zid);
    let tz = match ctx.invoke(
        "java/util/TimeZone",
        "getTimeZone",
        "(Ljava/time/ZoneId;)Ljava/util/TimeZone;",
        &[Value::Object(Some(zid))],
    ) {
        Ok(Some(Value::Object(Some(z)))) => z,
        _ => return None,
    };
    let tz_pin = ctx.pin_native_root(tz);
    let total = match ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]) {
        Ok(Some(Value::Int(v))) => v,
        _ => return None,
    };
    let tz = ctx.read_native_pin(tz_pin, tz);
    let raw = match ctx.invoke_virtual(tz, "getRawOffset", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        _ => total,
    };
    let tz = ctx.read_native_pin(tz_pin, tz);
    fmt_zone_name_of(ctx, tz, total != raw, locale)
}

/// The argument as a `java.util.Calendar`, or `None` for every other `%t`
/// source. One test, used by both zone helpers so they cannot disagree about
/// which arguments carry their own zone.
pub fn fmt_calendar_arg(ctx: &mut dyn NativeContext, val: &Value) -> Option<cratonvm_types::ObjectRef> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    let parent = ctx.class_id_by_name("java/util/Calendar")?;
    let cid = ctx.class_id_of_object(*obj);
    if cid == parent || ctx.is_subclass(cid, parent) {
        Some(*obj)
    } else {
        None
    }
}

/// `TimeZone`'s own `GMT±HH:MM` fallback form, for a zone with no short display
/// name. Measured as HotSpot's `Locale.ROOT` answer for `%tZ`.
pub fn fmt_zone_gmt_form(offset_ms: i32) -> String {
    let neg = offset_ms < 0;
    let mins = (offset_ms / 60_000).abs();
    format!(
        "GMT{}{:02}:{:02}",
        if neg { '-' } else { '+' },
        mins / 60,
        mins % 60
    )
}

/// Decode a `%t` argument into `(year, month, day, hour, minute, second,
/// nanos)` plus its [`FmtZone`] and its [`FmtSupport`].
///
/// The third element is the one that decides whether the fields are ANSWERS or
/// fabrications. `invoke_i32` below returns 0 for a method the argument's class
/// does not have, so a `LocalDate` asked for its hour used to come back with a
/// perfectly plausible `00` — where HotSpot 25 raises
/// `IllegalFormatConversionException: H != java.time.LocalDate`. The caller
/// screens `field` against the support BEFORE it renders anything; see
/// [`fmt_temporal_fault_char`] for the measured matrix.
thread_local! {
    pub static CACHED_LONG_CID: std::cell::Cell<Option<(usize, cratonvm_types::ClassId)>> =
        const { std::cell::Cell::new(None) };
}

pub fn extract_temporal_fields(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
) -> Result<((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport), MethodCallFailed> {
    // Fast path for boxed `java.lang.Long`: reading field 0 and resolving the
    // cached default zone needs no Java upcalls, so no pin is needed.
    if let Value::Object(Some(obj)) = val {
        let vm = ctx.vm_identity();
        let long_cid = match CACHED_LONG_CID.with(std::cell::Cell::get) {
            Some((c_vm, cid)) if c_vm == vm => Some(cid),
            _ => {
                let cid = ctx.class_id_by_name("java/lang/Long");
                if let Some(cid) = cid {
                    CACHED_LONG_CID.with(|c| c.set(Some((vm, cid))));
                }
                cid
            }
        };
        if long_cid == Some(ctx.class_id_of_object(*obj)) {
            let millis = match ctx.get_field(*obj, 0) {
                Value::Long(ms) => ms,
                Value::Int(ms) => i64::from(ms),
                _ => 0,
            };
            let zone = fmt_resolve_zone(ctx, None, millis);
            let shifted = millis.saturating_add(i64::from(zone.offset_ms));
            let day_millis = 86_400_000i64;
            let epoch_day = shifted.div_euclid(day_millis);
            let mut tod = shifted.rem_euclid(day_millis);
            let hour = (tod / 3_600_000) as i32;
            tod %= 3_600_000;
            let minute = (tod / 60_000) as i32;
            tod %= 60_000;
            let second = (tod / 1000) as i32;
            let ms = (tod % 1000) as i32;
            let (y, m, d) = crate::julian_date::hybrid_from_epoch_day(epoch_day);
            return Ok((
                (y as i64, m, d, hour, minute, second, ms * 1_000_000),
                zone,
                FmtSupport::ALL,
            ));
        }
    }
    // gen r4w3/rooting: the `java.time` arms call SEVERAL getters in sequence on
    // the argument (`getYear`, `getMonthValue`, …), and every one is an upcall
    // that can collect and move it. Pin the argument for the whole decode and
    // let the body re-derive it before each call; one `unpin_native_roots`
    // here releases this pin and anything the arms pinned above it.
    match val {
        Value::Object(Some(obj)) => {
            let arg_pin = ctx.pin_native_root(*obj);
            let out = extract_temporal_fields_pinned(ctx, val, field, Some(arg_pin));
            ctx.unpin_native_roots(arg_pin);
            out
        }
        _ => extract_temporal_fields_pinned(ctx, val, field, None),
    }
}

/// The body of [`extract_temporal_fields`]. `arg_pin` is the native pin of an
/// object `val` (always `Some` for one); every use of the argument after a GC
/// point re-reads it through that pin.
pub fn extract_temporal_fields_pinned(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
    arg_pin: Option<usize>,
) -> Result<((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport), MethodCallFailed> {
    fn millis_to_fields(millis: i64) -> (i64, i32, i32, i32, i32, i32, i32) {
        let day_millis = 86_400_000i64;
        let epoch_day = millis.div_euclid(day_millis);
        let mut tod = millis.rem_euclid(day_millis);
        let hour = (tod / 3_600_000) as i32;
        tod %= 3_600_000;
        let minute = (tod / 60_000) as i32;
        tod %= 60_000;
        let second = (tod / 1000) as i32;
        let ms = (tod % 1000) as i32;
        // `Date`/`Long`/`Calendar` sources go through `GregorianCalendar`'s
        // hybrid Julian/Gregorian decode (pre-1582 dates are Julian), unlike
        // `java.time`'s proleptic-Gregorian sources — see `julian_date.rs`.
        let (y, m, d) = crate::julian_date::hybrid_from_epoch_day(epoch_day);
        (y as i64, m, d, hour, minute, second, ms * 1_000_000)
    }

    fn invoke_i32(
        ctx: &mut dyn NativeContext,
        arg_pin: Option<usize>,
        obj: cratonvm_types::ObjectRef,
        name: &str,
    ) -> i32 {
        // gen r4w3/rooting: the previous getter may have moved `obj`; call on
        // the pinned current address.
        let obj = match arg_pin {
            Some(h) => ctx.read_native_pin(h, obj),
            None => obj,
        };
        match ctx.invoke_virtual(obj, name, "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            _ => 0,
        }
    }

    // Shift the instant into the zone's local time before splitting it into
    // fields, which is `cal.setTimeInMillis(ms)` on a zoned `Calendar`. Every
    // field the caller then reads — hour, day, month, year — is local, and the
    // `'s'`/`'Q'` arms undo the shift because an epoch second is not a local
    // quantity. `saturating_add` keeps a `Long.MIN_VALUE` argument from
    // wrapping into a far-future date.
    fn zoned_fields(
        ctx: &mut dyn NativeContext,
        calendar: Option<cratonvm_types::ObjectRef>,
        millis: i64,
    ) -> ((i64, i32, i32, i32, i32, i32, i32), FmtZone) {
        let zone = fmt_resolve_zone(ctx, calendar, millis);
        (
            millis_to_fields(millis.saturating_add(i64::from(zone.offset_ms))),
            zone,
        )
    }

    // The epoch-shaped sources take `printDateTime`'s `Calendar` branch, whose
    // printer has an arm for every field and refuses none — hence
    // [`FmtSupport::ALL`] on all four of them.
    fn epoch(
        pair: ((i64, i32, i32, i32, i32, i32, i32), FmtZone),
    ) -> ((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport) {
        (pair.0, pair.1, FmtSupport::ALL)
    }

    match val {
        Value::Long(ms) => Ok(epoch(zoned_fields(ctx, None, *ms))),
        Value::Object(Some(obj)) => {
            let obj = *obj;
            let cid = ctx.class_id_of_object(obj);
            let is_a = |ctx: &dyn NativeContext, name: &str| {
                ctx.class_id_by_name(name)
                    .is_some_and(|parent| ctx.is_subclass(cid, parent))
            };
            // A `long` argument (e.g. `String.format("%tY", date.getTime())`)
            // arrives here already autoboxed into the varargs Object[] array.
            if is_a(ctx, "java/lang/Long") {
                let millis = match ctx.get_field(obj, 0) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                Ok(epoch(zoned_fields(ctx, None, millis)))
            } else if is_a(ctx, "java/util/Date") {
                let millis = match ctx.invoke_virtual(obj, "getTime", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                Ok(epoch(zoned_fields(ctx, None, millis)))
            } else if is_a(ctx, "java/util/Calendar") {
                // `obj` is passed to `zoned_fields` below, after this call has
                // had its chance to collect and move it.
                let obj_pin = ctx.pin_native_root(obj);
                let millis = match ctx.invoke_virtual(obj, "getTimeInMillis", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                let obj = ctx.read_native_pin(obj_pin, obj);
                // The Calendar's OWN zone, not the default: measured on
                // HotSpot 25, `%tc` of a `Calendar.getInstance(TimeZone
                // .getTimeZone("Asia/Tokyo"))` set to 1699999999000 is
                // `Wed Nov 15 07:13:19 GMT+09:00 2023` whatever
                // `user.timezone` is.
                Ok(epoch(zoned_fields(ctx, Some(obj), millis)))
            } else if is_a(ctx, "java/time/Instant") {
                // Two sequential accessors on `obj` in the SAME arm; the first
                // can move it before the second reads it.
                let obj_pin = ctx.pin_native_root(obj);
                let sec = match ctx.invoke_virtual(obj, "getEpochSecond", "()J", &[])? {
                    Some(Value::Long(v)) => v,
                    _ => 0,
                };
                let obj = ctx.read_native_pin(obj_pin, obj);
                let nano = match ctx.invoke_virtual(obj, "getNano", "()I", &[])? {
                    Some(Value::Int(v)) => v,
                    _ => 0,
                };
                let (y, m, d, h, mi, s, _) = millis_to_fields(sec.saturating_mul(1000));
                // An `Instant` answers FOUR fields and refuses the other 27.
                // The date and clock values computed above are UTC and are
                // never rendered — `fmt_temporal_fault_char` refuses every
                // field that would read them — but they are computed anyway
                // because `%tL`/`%tN` need the nanos and `%ts`/`%tQ` need the
                // epoch, and `epoch_sec()` reconstructs the epoch from these.
                Ok((
                    (y, m, d, h, mi, s, nano),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: false,
                        sub_second: true,
                        instant: true,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/LocalDate") {
                let year = invoke_i32(ctx, arg_pin, obj, "getYear") as i64;
                let month = invoke_i32(ctx, arg_pin, obj, "getMonthValue");
                let day = invoke_i32(ctx, arg_pin, obj, "getDayOfMonth");
                Ok((
                    (year, month, day, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: true,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/LocalTime") {
                let hour = invoke_i32(ctx, arg_pin, obj, "getHour");
                let minute = invoke_i32(ctx, arg_pin, obj, "getMinute");
                let second = invoke_i32(ctx, arg_pin, obj, "getSecond");
                let nano = invoke_i32(ctx, arg_pin, obj, "getNano");
                Ok((
                    (1970, 1, 1, hour, minute, second, nano),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: true,
                        sub_second: true,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/OffsetTime") {
                // G2-1. `OffsetTime` was the one `java.time` type with a
                // printer on HotSpot that this decoder had no arm for, so all
                // 31 fields fell to the `else` below and refused — including
                // the FOURTEEN the JDK answers. Measured on HotSpot 25.0.3+9
                // for `2025-08-16T05:00:45.123+05:30`:
                // `%tH`=`05` `%tI`=`05` `%tk`=`5` `%tl`=`5` `%tM`=`00`
                // `%tS`=`45` `%tL`=`123` `%tN`=`123000000` `%tp`=`am`
                // `%tz`=`+0530` `%tZ`=`+05:30` `%tR`=`05:00` `%tT`=`05:00:45`
                // `%tr`=`05:00:45 AM`.
                //
                // It is `LocalTime` plus a zone and NOT an instant: measured,
                // `%ts` is `s != java.time.OffsetTime` and `%tQ` is
                // `Q != java.time.OffsetTime`, because `INSTANT_SECONDS` needs
                // a date the type does not carry. `getOffset()` is a
                // `ZoneOffset`, which is what [`fmt_temporal_zone`] and
                // [`fmt_temporal_zone_name`]'s non-`ZonedDateTime` arm already
                // read — the latter's `getId()` is where `+05:30` comes from.
                let hour = invoke_i32(ctx, arg_pin, obj, "getHour");
                let minute = invoke_i32(ctx, arg_pin, obj, "getMinute");
                let second = invoke_i32(ctx, arg_pin, obj, "getSecond");
                let nano = invoke_i32(ctx, arg_pin, obj, "getNano");
                // gen r4w3/rooting: the four getters above can have moved it.
                let obj = match arg_pin {
                    Some(h) => ctx.read_native_pin(h, obj),
                    None => obj,
                };
                let zone = fmt_temporal_zone(ctx, obj);
                Ok((
                    (1970, 1, 1, hour, minute, second, nano),
                    zone,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: true,
                        sub_second: true,
                        instant: false,
                        zone: true,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/YearMonth") {
                // G2-1, and the three arms below it. These are the PARTIAL
                // `java.time` types: each supports a proper subset of the date
                // group, which is why [`FmtSupport`] carries `year`/`month`/
                // `day` separately instead of one `date` flag. Measured on
                // HotSpot 25.0.3+9 (`Locale.US`):
                //
                // | source | answers | refuses |
                // |---|---|---|
                // | `YearMonth.of(2020,2)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` `%tm`=`02` `%tB`=`February` `%tb`=`%th`=`Feb` | 24 |
                // | `MonthDay.of(1,2)` | `%tm`=`01` `%td`=`02` `%te`=`2` `%tB`=`January` `%tb`=`%th`=`Jan` | 25 |
                // | `Year.of(2020)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` | 28 |
                // | `Month.JANUARY` | `%tm`=`01` `%tB`=`January` `%tb`=`%th`=`Jan` | 27 |
                //
                // The unread slots are left at the epoch defaults; every field
                // that would read one is refused by
                // [`fmt_temporal_fault_char`], exactly as for `Instant`.
                let year = invoke_i32(ctx, arg_pin, obj, "getYear") as i64;
                let month = invoke_i32(ctx, arg_pin, obj, "getMonthValue");
                Ok((
                    (year, month, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/MonthDay") {
                let month = invoke_i32(ctx, arg_pin, obj, "getMonthValue");
                let day = invoke_i32(ctx, arg_pin, obj, "getDayOfMonth");
                Ok((
                    (1970, month, day, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: true,
                        day: true,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/Year") {
                // `Year.getValue()`, not `getYear()` — the type has no
                // `getYear`.
                let year = invoke_i32(ctx, arg_pin, obj, "getValue") as i64;
                Ok((
                    (year, 1, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: true,
                        month: false,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/Month") {
                // `Month` is an enum and its `getValue()` is 1..=12, which is
                // already this decoder's month convention.
                let month = invoke_i32(ctx, arg_pin, obj, "getValue");
                Ok((
                    (1970, month, 1, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: true,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else if is_a(ctx, "java/time/DayOfWeek") {
                // G2-1 residual §4.1, closed. `DayOfWeek` has no year, month
                // or day at all — MEASURED, `%tA` of `DayOfWeek.MONDAY` is
                // `Monday`, `%ta` is `Mon`, and the other 29 fields refuse,
                // with `%tc` reporting `b`, not `a` (`FmtSupport::weekday`
                // is what makes that composite's first check pass here while
                // `full_date()` alone would not).
                //
                // `getValue()` is ISO-8601, `1..=7` for Monday..Sunday.
                // `day_of_week_sun0` wants a real calendar date, so this picks
                // one KNOWN to land on the requested weekday and read by
                // nothing else: 2001-01-01 was a Monday (a fixed historical
                // fact, not a per-run measurement), so `2001-01-<isoValue>`
                // walks Monday..Sunday for `isoValue` 1..7 in
                // `format_temporal_field`'s `'A'`/`'a'` arms. `year`, `month`
                // and `day` all stay `false` below, so nothing but those two
                // fields can ever read this fabricated triple back out —
                // exactly like `Instant`'s unread epoch defaults.
                let iso = invoke_i32(ctx, arg_pin, obj, "getValue");
                Ok((
                    (2001, 1, iso, 0, 0, 0, 0),
                    FmtZone::NONE,
                    FmtSupport {
                        year: false,
                        month: false,
                        day: false,
                        time: false,
                        sub_second: false,
                        instant: false,
                        zone: false,
                        calendar_printer: false,
                        weekday_only: true,
                    },
                ))
            } else if is_a(ctx, "java/time/LocalDateTime")
                || is_a(ctx, "java/time/ZonedDateTime")
                || is_a(ctx, "java/time/OffsetDateTime")
            {
                let year = invoke_i32(ctx, arg_pin, obj, "getYear") as i64;
                let month = invoke_i32(ctx, arg_pin, obj, "getMonthValue");
                let day = invoke_i32(ctx, arg_pin, obj, "getDayOfMonth");
                let hour = invoke_i32(ctx, arg_pin, obj, "getHour");
                let minute = invoke_i32(ctx, arg_pin, obj, "getMinute");
                let second = invoke_i32(ctx, arg_pin, obj, "getSecond");
                let nano = invoke_i32(ctx, arg_pin, obj, "getNano");
                // All three read LOCAL fields off the object, so no shift
                // applies. The two that carry an offset get it from the object
                // ([`fmt_temporal_zone`]) and are `instant`-capable with it —
                // `%ts` of a `ZonedDateTime` is `1699999999`, which
                // `epoch_sec()` reaches by subtracting the offset back out of
                // these local fields. A `LocalDateTime` has NO zone and no
                // instant: `%tz`, `%tZ`, `%ts` and `%tQ` all refuse
                // (measured), and `%tc` refuses at its zone slot with `Z`.
                let zoned =
                    is_a(ctx, "java/time/ZonedDateTime") || is_a(ctx, "java/time/OffsetDateTime");
                let zone = if zoned {
                    // gen r4w3/rooting: the seven getters above can have moved it.
                    let obj = match arg_pin {
                        Some(h) => ctx.read_native_pin(h, obj),
                        None => obj,
                    };
                    fmt_temporal_zone(ctx, obj)
                } else {
                    FmtZone::NONE
                };
                Ok((
                    (year, month, day, hour, minute, second, nano),
                    zone,
                    FmtSupport {
                        year: true,
                        month: true,
                        day: true,
                        time: true,
                        sub_second: true,
                        instant: zoned,
                        zone: zoned,
                        calendar_printer: false,
                        weekday_only: false,
                    },
                ))
            } else {
                // `printDateTime`'s `else` arm. HotSpot reports
                // "Y != java.lang.String" for `String.format("%tY", "x")`, a
                // typed refusal a caller can catch as
                // `IllegalFormatConversionException`; the base
                // `IllegalArgumentException` and its invented
                // "cannot be formatted as a date" wording were CratonVM's own.
                Err(fmt_raise(ctx, &FmtFault::WrongType(field, cid)))
            }
        }
        // A primitive that is not a `long` cannot arrive here at all: the
        // varargs array boxes everything, and the `Long` case is handled
        // above. Report it the same way `failConversion` would if it could.
        _ => Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Illegal date/time conversion argument".to_string(),
            }
            .into(),
        ),
    }
}

/// `%tF`'s year, which is NOT `%tY`'s.
///
/// `ISO_STANDARD_DATE` is the one composite that renders a year itself instead
/// of delegating, and its rule is its own:
///
/// ```text
/// if (year < 0)      { sb.append(getMinusSign(l)); year = -year; }
/// else if (year > 9999) { sb.append('+'); }
/// sb.append(localizedMagnitude(fmt, null, year, Flags.ZERO_PAD, 4, l));
/// ```
///
/// so the four-digit zero pad is applied to the ABSOLUTE value and the sign
/// sits outside it. Measured on HotSpot 25 under `Locale.ROOT`:
/// `LocalDate.of(-44, 3, 15)` is `-0044-03-15`, `LocalDate.of(-1, 6, 5)` is
/// `-0001-06-05`, `LocalDate.of(12345, 3, 4)` is `+12345-03-04`, and
/// `LocalDate.of(9999, 12, 31)` is `9999-12-31` with no sign at all.
///
/// Rust's `{:04}` counts the sign INSIDE the width (`-044`) and has no '+'
/// rule, which is what this replaces. The digits stay ASCII here and are
/// substituted by [`fmt_localize_digits`] with the rest of the field — the
/// sign is not a digit, so it survives that pass unchanged.
///
/// # This is the ONLY caller of `getMinusSign` in the whole class
///
/// `minus` is [`FmtSymbols::minus`], i.e.
/// `DecimalFormatSymbols.getInstance(l).getMinusSign()`, and F28-1 §5.3's
/// residual is closed here. The JDK's `getMinusSign(Locale)` has exactly one
/// call site and it is this arm: every other negative number in
/// `java.util.Formatter` goes through `leadingSign`, which appends an ASCII
/// `'-'`. Measured under `lt-LT` (`getMinusSign()` = U+2212):
/// `String.format(lt, "%d", -5)` is `002D 0035` while
/// `String.format(lt, "%tF", LocalDate.of(-44,3,15))` is
/// `2212 0030 0030 0034 0034` `002D` `…` — the year's sign localized, the two
/// date separators in the same field left ASCII. See [`FmtSymbols`] for the
/// full measured table and for why the earlier "every negative `%d`/`%f`"
/// framing was wrong.
///
/// The `'+'` past 9999 is NOT symmetric with it and stays ASCII: the JDK writes
/// `sb.append('+')` as a literal in the same block it calls `getMinusSign(l)`
/// in, and `String.format(lt, "%tF", LocalDate.of(12345,3,4))` is measured
/// `+12345-03-04` with an ASCII U+002B.
///
/// **Only the `TemporalAccessor` printer reaches here.** The `Calendar` printer
/// renders `%tF`'s year as a plain era-relative `YEAR_4` with no sign rule at
/// all — see [`FmtSupport::calendar_printer`], which is what selects between
/// the two.
pub fn fmt_iso_year(year: i64, minus: char) -> String {
    let (head, y) = if year < 0 {
        (Some(minus), year.saturating_neg())
    } else if year > 9999 {
        (Some('+'), year)
    } else {
        (None, year)
    };
    match head {
        Some(sign) => format!("{sign}{y:04}"),
        None => format!("{y:04}"),
    }
}

/// Zeller/Sakamoto-style day-of-week for the `%tA`/`%ta` name tables below.
/// Returns 0=Sunday .. 6=Saturday (matching `DAYS_ABBR`/`DAYS_FULL` order) —
/// the same algorithm as the `'E'` pattern letter in `dtf_apply_pattern`
/// (`util_time.rs`), duplicated here in its 0=Sunday form rather than reused
/// since that copy is private and returns the opposite (Java `DayOfWeek`,
/// 1=Monday) convention.
/// `hybrid` selects which calendar `(year, month, day)` are labelled in —
/// `true` for a `calendar_printer` source (`Date`/`Long`/`Calendar`/
/// `Instant`, decoded through `millis_to_fields`'s hybrid Julian/Gregorian
/// calendar), `false` for a `java.time` source (proleptic Gregorian by
/// specification). Weekday itself needs no branch beyond that: it is a
/// property of the physical day, not of which calendar labelled it, and
/// weekday continuity survives the 1582 reform intact (Julian Thursday
/// Oct 4 1582 was followed by Gregorian Friday Oct 15 1582 — the reform
/// deleted ten calendar dates, never a day of the week). `1970-01-01` (epoch
/// day 0) was a Thursday, index 4 in the 0=Sunday order `DAYS_ABBR`/
/// `DAYS_FULL` use.
pub fn day_of_week_sun0(year: i32, month: i32, day: i32, hybrid: bool) -> usize {
    let epoch_day = if hybrid {
        crate::julian_date::hybrid_to_epoch_day(year, month, day)
    } else {
        civil_date::to_epoch_day(year, month, day)
    };
    (epoch_day + 4).rem_euclid(7) as usize
}

pub type ExtractedTemporal = ((i64, i32, i32, i32, i32, i32, i32), FmtZone, FmtSupport);

/// Format a single `%t`/`%T` date/time field conversion (the character
/// following the `t`/`T` prefix — e.g. `b` in `%tb`). See
/// [`extract_temporal_fields`] for how the source value is decoded and
/// `java.util.Formatter`'s own `%t` conversion table for the field meanings.
///
/// The six name-bearing fields — `%tB` `%tb`/`%th` `%tA` `%ta` `%tp`, and
/// `%tc`, which is a composite over two of them — go through
/// [`fmt_date_name`], i.e. through `java.text.DateFormatSymbols`, because that
/// is what `java.util.Formatter.print(TemporalAccessor, char, Locale)` does.
/// They used to be answered from the four English tables below **whatever the
/// locale was**, so every `java.util.logging.SimpleFormatter` line rendered its
/// month in English on a host that is not English — its default pattern opens
/// `%1$tb`. The tables survive as the fallback for the configurations where
/// there is no `DateFormatSymbols` to ask, which is the same value they always
/// produced.
pub const MONTHS_ABBR_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_FULL_EN: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const DAYS_ABBR_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const DAYS_FULL_EN: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

#[allow(clippy::too_many_arguments)]
pub fn format_temporal_field_into(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
    flags: &str,
    width: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
    uppercase: bool,
    pre_extracted: Option<ExtractedTemporal>,
    out: &mut Vec<u16>,
) -> Result<Option<ExtractedTemporal>, MethodCallFailed> {
    if !matches!(val, Value::Object(None))
        && !uppercase
        && width.is_none()
        && sym.zero == '0'
        && matches!(
            field,
            'H' | 'M' | 'S' | 'd' | 'm' | 'Y' | 'y' | 'C' | 'R' | 'T' | 'b' | 'h' | 'B' | 'a' | 'A'
        )
    {
        let ext = match pre_extracted {
            Some(ext) => ext,
            None => extract_temporal_fields(ctx, val, field)?,
        };
        if let (Some(c), Value::Object(Some(obj))) = (fmt_temporal_fault_char(field, ext.2), val) {
            let cid = ctx.class_id_of_object(*obj);
            return Err(fmt_raise(ctx, &FmtFault::WrongType(c, cid)));
        }
        let ((year, month, day, hour, minute, second, _nanos), _zone, support) = ext;
        let year32 = year as i32;
        let year_of_era = if year <= 0 { 1 - year } else { year };
        let month_idx = (month.clamp(1, 12) - 1) as usize;
        let push_2d = |out: &mut Vec<u16>, v: i32| {
            out.push(u16::from(b'0' + ((v / 10) % 10) as u8));
            out.push(u16::from(b'0' + (v % 10) as u8));
        };
        let push_4d = |out: &mut Vec<u16>, v: i64| {
            if (0..10_000).contains(&v) {
                out.push(u16::from(b'0' + ((v / 1000) % 10) as u8));
                out.push(u16::from(b'0' + ((v / 100) % 10) as u8));
                out.push(u16::from(b'0' + ((v / 10) % 10) as u8));
                out.push(u16::from(b'0' + (v % 10) as u8));
            } else {
                for b in format!("{:04}", v).as_bytes() {
                    out.push(u16::from(*b));
                }
            }
        };
        match field {
            'H' => push_2d(out, hour),
            'M' => push_2d(out, minute),
            'S' => push_2d(out, second),
            'd' => push_2d(out, day),
            'm' => push_2d(out, month),
            'Y' => push_4d(out, year_of_era),
            'y' => push_2d(out, (year_of_era % 100) as i32),
            'C' => push_2d(out, (year_of_era / 100) as i32),
            'R' => {
                push_2d(out, hour);
                out.push(u16::from(b':'));
                push_2d(out, minute);
            }
            'T' => {
                push_2d(out, hour);
                out.push(u16::from(b':'));
                push_2d(out, minute);
                out.push(u16::from(b':'));
                push_2d(out, second);
            }
            'b' | 'h' => fmt_date_name_push_u16(
                ctx,
                locale,
                "getShortMonths",
                month_idx,
                MONTHS_ABBR_EN[month_idx],
                out,
            ),
            'B' => fmt_date_name_push_u16(
                ctx,
                locale,
                "getMonths",
                month_idx,
                MONTHS_FULL_EN[month_idx],
                out,
            ),
            'a' => {
                let dow = day_of_week_sun0(year32, month, day, support.calendar_printer);
                fmt_date_name_push_u16(
                    ctx,
                    locale,
                    "getShortWeekdays",
                    dow + 1,
                    DAYS_ABBR_EN[dow],
                    out,
                );
            }
            'A' => {
                let dow = day_of_week_sun0(year32, month, day, support.calendar_printer);
                fmt_date_name_push_u16(
                    ctx,
                    locale,
                    "getWeekdays",
                    dow + 1,
                    DAYS_FULL_EN[dow],
                    out,
                );
            }
            _ => unreachable!(),
        }
        return Ok(if pre_extracted.is_none() {
            Some(ext)
        } else {
            None
        });
    }
    let (text, ext) = format_temporal_field(
        ctx,
        val,
        field,
        flags,
        width,
        sym,
        locale,
        uppercase,
        pre_extracted,
    )?;
    out.extend_from_slice(&text);
    Ok(ext)
}

#[allow(clippy::too_many_arguments)]
pub fn format_temporal_field(
    ctx: &mut dyn NativeContext,
    val: &Value,
    field: char,
    flags: &str,
    width: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
    uppercase: bool,
    pre_extracted: Option<ExtractedTemporal>,
) -> Result<(Vec<u16>, Option<ExtractedTemporal>), MethodCallFailed> {
    // gen r4w3/rooting: the argument is used again AFTER
    // `extract_temporal_fields`' getters and `fmt_date_name`'s
    // `DateFormatSymbols` upcalls (the fault report, `%tZ`, `%tc`'s zone slot),
    // any of which can move it. Pin it for the whole conversion; the body
    // re-derives it through the pin before each such use.
    let val_pin = match val {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    let out = format_temporal_field_pinned(
        ctx, val, val_pin, field, flags, width, sym, locale, uppercase, pre_extracted,
    );
    if let Some(h) = val_pin {
        ctx.unpin_native_roots(h);
    }
    out
}

/// The body of [`format_temporal_field`]. `val` is only guaranteed current on
/// entry; after any upcall it is re-derived through `val_pin`.
#[allow(clippy::too_many_arguments)]
pub fn format_temporal_field_pinned(
    ctx: &mut dyn NativeContext,
    val: &Value,
    val_pin: Option<usize>,
    field: char,
    flags: &str,
    width: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
    uppercase: bool,
    pre_extracted: Option<ExtractedTemporal>,
) -> Result<(Vec<u16>, Option<ExtractedTemporal>), MethodCallFailed> {
    const MONTHS_ABBR: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const MONTHS_FULL: [&str; 12] = [
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
    ];
    const DAYS_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const DAYS_FULL: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    // gen r4w3/rooting: the argument at its CURRENT address.
    let cur_val = |ctx: &dyn NativeContext| -> Value {
        match (val_pin, val) {
            (Some(h), Value::Object(Some(o))) => Value::Object(Some(ctx.read_native_pin(h, *o))),
            _ => val.clone(),
        }
    };

    // "If the argument arg is null, then the result is 'null'" —
    // `printDateTime` returns before it ever looks at the field, so a null
    // date is not a refusal. This used to reach `extract_temporal_fields`'
    // catch-all and come back as an `IllegalArgumentException`.
    if matches!(val, Value::Object(None)) {
        // Same shared printer as every other conversion's null — the word,
        // then the (locale-sensitive) upper-caser, then the justifier.
        let mut s = fmt_null_text('t').to_string();
        if uppercase {
            s = fmt_upper_case(ctx, &s, locale);
        }
        return fmt_pad_str_to_width(&s, flags, width).map(|v| (v, None));
    }
    let arg_now = cur_val(&*ctx);
    let (fields, zone, support) = match pre_extracted {
        Some(ext) => ext,
        None => extract_temporal_fields(ctx, &arg_now, field)?,
    };
    let (year, month, day, hour, minute, second, nanos) = fields;
    // `t.get(ChronoField)` on a field the argument does not support is a
    // `DateTimeException`, which `print(Formatter, TemporalAccessor, char,
    // Locale)` catches and rethrows as `IllegalFormatConversionException(c,
    // t.getClass())`. Screening here rather than at each arm is what keeps the
    // 31 arms from each needing to know which sources can answer them — and
    // the fields decoded above are, for an unsupported field, FABRICATIONS
    // (`invoke_i32` answers 0 for an absent method), so nothing downstream may
    // see them. The character reported is not always `field`; see
    // [`fmt_temporal_fault_char`].
    //
    // A `Value::Long` argument cannot reach the `Some` arm — `FmtSupport::ALL`
    // refuses nothing — which is also why the class needed for the message is
    // always available here.
    // gen r4w3/rooting: re-derived — the getters above can have moved it.
    if let (Some(c), Value::Object(Some(obj))) =
        (fmt_temporal_fault_char(field, support), cur_val(&*ctx))
    {
        let cid = ctx.class_id_of_object(obj);
        return Err(fmt_raise(ctx, &FmtFault::WrongType(c, cid)));
    }
    let year32 = year as i32;
    // `%tY` `%ty` `%tC` — and the year slots of the `%tD` and `%tc` composites
    // — render the ERA-RELATIVE year, never the proleptic one. Both of the
    // JDK's printers do, by two different routes that agree:
    //
    //   TemporalAccessor   int i = t.get(ChronoField.YEAR_OF_ERA);
    //   Calendar           int i = t.get(Calendar.YEAR);   // already era-relative
    //
    // so ONE expression serves every source this VM decodes. F28-1 §5.2 left
    // this open on the premise that "`Calendar.YEAR` is already era-relative, so
    // a single expression cannot serve both". That premise is about the JDK's
    // two printers and it does not transfer, because CratonVM never reads
    // `Calendar.YEAR`: `extract_temporal_fields` decodes EVERY epoch-shaped
    // source through `millis_to_fields` → `julian_date::hybrid_from_epoch_day`,
    // which yields a PROLEPTIC year (Julian-calendar before the 1582 cutover,
    // Gregorian on or after it) exactly like `getYear()` does for the
    // `java.time` sources. Both halves were measured on HotSpot 25
    // (`Locale.ROOT`, `Asia/Kolkata`) at the one instant, 45 BC:
    //
    // | argument | `%tY` | `%ty` | `%tC` | `%tD` | `%tc` tail |
    // |---|---|---|---|---|---|
    // | `LocalDate.of(-44,3,15)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `GregorianCalendar` ERA=BC YEAR=45 | `0045` | `45` | `00` | `03/15/45` | `0045` |
    // | `new Date(-63549548877000L)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `Long.valueOf(-63549548877000L)` | `0045` | `45` | `00` | `03/15/45` | |
    // | `LocalDate.of(-1,6,5)` | `0002` | `02` | `00` | | |
    // | `LocalDate.of(0,2,29)` | `0001` | `01` | `00` | | |
    //
    // The proleptic year is still what `%tF`'s TemporalAccessor arm, the
    // day-of-week and the day-of-year read, so it is not overwritten here.
    //
    // `1 - year` and not `-year`: proleptic 0 is 1 BC, so the mapping is
    // 0 → 1, -1 → 2, -44 → 45. Verified against `ChronoField.YEAR_OF_ERA` on
    // all four of those inputs.
    let year_of_era = if year <= 0 { 1 - year } else { year };
    let millis = nanos / 1_000_000;
    let dow = day_of_week_sun0(year32, month, day, support.calendar_printer);
    let month_idx = (month.clamp(1, 12) - 1) as usize;
    let hour12 = {
        let h = hour % 12;
        if h == 0 {
            12
        } else {
            h
        }
    };
    // `Calendar.AM`/`Calendar.PM` are 0 and 1, which is the index into
    // `getAmPmStrings()`. The English pair is the JDK's own literal
    // `String[] ampm = { "AM", "PM" }` for the null-locale branch, lower-cased
    // the way `printDateTime` lower-cases whatever it read.
    let ampm_idx = if hour < 12 { 0usize } else { 1usize };
    let ampm_en = if hour < 12 { "am" } else { "pm" };
    // `%ts`/`%tQ` are `Calendar.getTimeInMillis()` — an EPOCH quantity, and the
    // one pair of fields that is not local. The fields above have already been
    // shifted into the zone, so the shift is subtracted back out here.
    // Measured on HotSpot 25: `%ts` of `new Date(1699999999000L)` is
    // `1699999999` and `%tQ` is `1699999999000` under UTC, America/New_York,
    // Asia/Kolkata and Europe/Berlin alike — the value does not move with the
    // zone, while `%tH` does.
    let epoch_sec = || {
        // See [`FmtSupport::hybrid_epoch`]: `Date`/`Long`/`Calendar`/`Instant`
        // were decoded through `millis_to_fields`'s hybrid Julian/Gregorian
        // calendar, so re-encoding them must go through the same hybrid
        // calendar or a pre-cutover instant would not round trip back to its
        // own epoch. `ZonedDateTime`/`OffsetDateTime` read their fields
        // straight off the object — genuinely proleptic Gregorian by
        // specification, never Julian — and must stay on the pure
        // civil-date encode.
        let epoch_day = if support.hybrid_epoch() {
            crate::julian_date::hybrid_to_epoch_day(year32, month, day)
        } else {
            civil_date::to_epoch_day(year32, month, day)
        };
        epoch_day * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
            - i64::from(zone.offset_ms) / 1000
    };

    let mut out = match field {
        // Time
        'H' => format!("{:02}", hour),
        'k' => format!("{}", hour),
        'I' => format!("{:02}", hour12),
        'l' => format!("{}", hour12),
        'M' => format!("{:02}", minute),
        'S' => format!("{:02}", second),
        'L' => format!("{:03}", millis),
        'N' => format!("{:09}", nanos),
        // `s.toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))`
        // — the locale's lower-caser, through the same `case_map` the `%S`/`%T`
        // upper-caser goes through. This was `str::to_lowercase`, Rust's
        // locale-independent one; see [`fmt_lower_case`], which also records
        // that the 1158-locale sweep found no row that moves.
        //
        // The `unwrap_or_else` fallback is the JDK's own English literal, which
        // is already lower-case in ASCII and is what the null-locale branch
        // lower-cases. It is not routed through the case mapper because there
        // is nothing to map and this arm is reached exactly when
        // `DateFormatSymbols` could not be asked at all.
        'p' => {
            // Two `ctx` uses in sequence, bound out like the `'r'` arm below
            // does with its upper-caser, rather than chained through a closure.
            let name = fmt_date_name(ctx, locale, "getAmPmStrings", ampm_idx);
            match name {
                Some(s) => fmt_lower_case(ctx, &s, locale),
                None => ampm_en.to_string(),
            }
        }
        // `ZONE_OFFSET + DST_OFFSET`, rendered `{-|+}HHMM`. See [`FmtZone`] for
        // the measured table and for why an unknown zone keeps the old fixed
        // `+0000`/`UTC` pair rather than inventing one.
        'z' => {
            let i = zone.offset_ms;
            let mins = (i / 60_000).abs();
            format!(
                "{}{:02}{:02}",
                if i < 0 { '-' } else { '+' },
                mins / 60,
                mins % 60
            )
        }
        'Z' => {
            // gen r4w3/rooting: current address, re-derived after the getters.
            let arg_now = cur_val(&*ctx);
            fmt_zone_display_name(ctx, &arg_now, zone, locale).unwrap_or_else(|| {
                if zone.known {
                    fmt_zone_gmt_form(zone.offset_ms)
                } else {
                    "UTC".to_string()
                }
            })
        }
        's' => format!("{}", epoch_sec()),
        'Q' => format!("{}", epoch_sec() * 1000 + millis as i64),
        // Date
        'B' => fmt_date_name(ctx, locale, "getMonths", month_idx)
            .unwrap_or_else(|| MONTHS_FULL[month_idx].to_string()),
        'b' | 'h' => fmt_date_name(ctx, locale, "getShortMonths", month_idx)
            .unwrap_or_else(|| MONTHS_ABBR[month_idx].to_string()),
        // `getWeekdays()` is indexed by `Calendar.SUNDAY`..`SATURDAY`, i.e.
        // 1..7 with slot 0 unused, so the 0=Sunday `dow` shifts by one. The
        // JDK reaches the same index from the other direction, as
        // `DAY_OF_WEEK % 7 + 1` over an ISO 1=Monday field.
        'A' => fmt_date_name(ctx, locale, "getWeekdays", dow + 1)
            .unwrap_or_else(|| DAYS_FULL[dow].to_string()),
        'a' => fmt_date_name(ctx, locale, "getShortWeekdays", dow + 1)
            .unwrap_or_else(|| DAYS_ABBR[dow].to_string()),
        // `CENTURY`/`YEAR_2`/`YEAR_4` are one JDK arm over one value:
        // `i = YEAR_OF_ERA; C -> i /= 100; y -> i %= 100; Y -> size = 4`.
        // `year_of_era` is >= 1 by construction, so plain `/` and `%` are the
        // euclidean ones here and no sign can reach the zero pad.
        'C' => format!("{:02}", year_of_era / 100),
        'Y' => format!("{:04}", year_of_era),
        'y' => format!("{:02}", year_of_era % 100),
        'j' => format!(
            "{:03}",
            if support.calendar_printer {
                crate::julian_date::hybrid_day_of_year(year32, month, day)
            } else {
                civil_date::day_of_year(year32, month, day)
            }
        ),
        'm' => format!("{:02}", month),
        'd' => format!("{:02}", day),
        'e' => format!("{}", day),
        // Composites
        'R' => format!("{:02}:{:02}", hour, minute),
        'T' => format!("{:02}:{:02}:{:02}", hour, minute, second),
        'r' => {
            // `sb.append(toUpperCaseWithLocale(tsb.toString(), l))` — %tr
            // upper-cases the AM/PM marker against the call's locale, and that
            // is a separate site from the `%T` prefix's own upper-caser.
            let ampm = fmt_date_name(ctx, locale, "getAmPmStrings", ampm_idx)
                .unwrap_or_else(|| ampm_en.to_string());
            let ampm = fmt_upper_case(ctx, &ampm, locale);
            // The clock is `localizedMagnitude`d, the MARKER is not — it is a
            // `DateFormatSymbols` name. Measured under `ar-EG`: `%tr` is
            // U+0660 U+0663 ':' U+0664 U+0663 ':' U+0661 U+0669 ' ' U+0635.
            let hms = fmt_localize_digits(&format!("{hour12:02}:{minute:02}:{second:02}"), sym);
            format!("{hms} {ampm}")
        }
        // `%tD` is `print(MONTH) '/' print(DAY_OF_MONTH_0) '/' print(YEAR_2)`,
        // so its year slot is `%ty`'s and takes the era-relative year too:
        // measured, `%tD` of `LocalDate.of(-44,3,15)` is `03/15/45`, not
        // `03/15/56` (which is what the proleptic `(-44).rem_euclid(100)` gave).
        'D' => format!("{:02}/{:02}/{:02}", month, day, year_of_era % 100),
        // `ISO_STANDARD_DATE` is the one conversion where the JDK's two
        // printers DISAGREE, so it is the one arm that has to ask which source
        // it has. See [`FmtSupport::calendar_printer`] for the measured table.
        //
        //  * `TemporalAccessor`: reads the PROLEPTIC year inline and renders it
        //    itself — a negative year is `getMinusSign(l)` followed by the
        //    ABSOLUTE value zero-padded to four, and a year past 9999 carries a
        //    literal '+'. `LocalDate.of(-44,3,15)` is `-0044-03-15`,
        //    `LocalDate.of(12345,3,4)` is `+12345-03-04`. Rust's `{:04}` counts
        //    the sign inside the width and answers `-044-03-15`, and it has no
        //    '+' rule at all.
        //  * `Calendar`: delegates to `print(YEAR_4)`, i.e. to `%tY`'s
        //    era-relative value, which is never negative — so no sign rule and
        //    no '+'. The SAME instant as above, as a `GregorianCalendar` with
        //    ERA=BC YEAR=45, is `0045-03-15`, and AD 12345 is `12345-03-04`
        //    with NO leading '+'.
        //
        // Rendering the second one through `fmt_iso_year` was a live
        // divergence: `%tF` of a `Date`/`Calendar`/`long` past year 9999 gained
        // a '+' the JDK does not write there.
        'F' => {
            let y = if support.calendar_printer {
                format!("{year_of_era:04}")
            } else {
                fmt_iso_year(year, sym.minus)
            };
            format!("{y}-{month:02}-{day:02}")
        }
        // `%tc` is the JDK's own composite `%ta %tb %td %tT %tZ %tY` — so its
        // day is `DAY_OF_MONTH_0`, ZERO-padded (`Sat Nov 04 …`, the javadoc's
        // own example). It was space-padded here, which is `%te`'s rule.
        // The zone slot is `print(fmt, sb, t, DateTime.ZONE, l)` — the SAME
        // lookup as `%tZ`, not a hard-coded word. It used to be the literal
        // "UTC", which was consistent with the fields being UTC and wrong with
        // them: `%tc` of `new Date(1699999999000L)` under `America/New_York` is
        // `Tue Nov 14 17:13:19 EST 2023` on HotSpot 25 and was
        // `Tue Nov 14 22:13:19 UTC 2023` here — five hours, the zone name, and
        // under `Asia/Kolkata` the DATE as well.
        'c' => {
            let weekday = fmt_date_name(ctx, locale, "getShortWeekdays", dow + 1)
                .unwrap_or_else(|| DAYS_ABBR[dow].to_string());
            let month_name = fmt_date_name(ctx, locale, "getShortMonths", month_idx)
                .unwrap_or_else(|| MONTHS_ABBR[month_idx].to_string());
            // gen r4w3/rooting: the two `fmt_date_name` upcalls above can have
            // moved the argument; re-derive it through the pin.
            let arg_now = cur_val(&*ctx);
            let zone_name =
                fmt_zone_display_name(ctx, &arg_now, zone, locale).unwrap_or_else(|| {
                    if zone.known {
                        fmt_zone_gmt_form(zone.offset_ms)
                    } else {
                        "UTC".to_string()
                    }
                });
            // Three name slots and two numeric ones. Only the numeric ones are
            // `localizedMagnitude`d — measured under `ar-EG`, `%tc` of a
            // `Date` keeps the Arabic weekday and month as text, keeps the
            // zone name `IST` in ASCII, and renders the day, the clock and the
            // year in Arabic-Indic digits. Localizing the composed string
            // instead would rewrite the digits INSIDE a `GMT+05:30` zone name.
            let dt =
                fmt_localize_digits(&format!("{day:02} {hour:02}:{minute:02}:{second:02}"), sym);
            // `%tc`'s tail is `print(fmt, sb, t, DateTime.YEAR_4, l)` — `%tY`'s
            // arm, so the ERA-RELATIVE year and not the proleptic one.
            // Measured: `%tc` of a `ZonedDateTime` in 45 BC ends `… 0045`, and
            // so does `%tc` of the `GregorianCalendar` for the same instant.
            let y = fmt_localize_digits(&format!("{year_of_era:04}"), sym);
            format!("{weekday} {month_name} {dt} {zone_name} {y}")
        }
        // Unreachable today: the caller already screened `field` against
        // `FMT_DATETIME_FIELDS`, which is `DateTime.isValid`'s own set, and
        // every one of its 31 characters has an arm above. Kept as the arm a
        // future divergence between the two sets would land in, raising the
        // refusal the JDK names rather than the base class.
        _ => {
            return Err(fmt_raise(
                ctx,
                &FmtFault::UnknownConversion(format!("t{field}")),
            ));
        }
    };

    // Every NUMBER a `%t` field renders goes through `localizedMagnitude`,
    // which substitutes the locale's zero digit — this whole family did it in
    // ASCII. Measured on HotSpot 25 under `ar-EG` (zero digit U+0660):
    // `%tH` of `new Date(1699999999000L)` is `U+0660 U+0663`, `%tT` is
    // `U+0660 U+0663 ':' U+0664 U+0663 ':' U+0661 U+0669` (the SEPARATORS stay
    // ASCII), `%tz` is `'+' U+0660 U+0665 U+0663 U+0660` (the SIGN stays
    // ASCII), `%ts`, `%tQ`, `%tY`, `%tj`, `%td`, `%tD`, `%tF`, `%tL`, `%tN`,
    // `%tC`, `%ty`, `%tm`, `%tI`, `%tk`, `%tl` likewise. `java.util.logging`'s
    // default `SimpleFormatter` pattern is `%1$tb %1$td, %1$tY …`, so this is
    // every log line on such a host.
    //
    // The seven NAME fields are excluded because they are
    // `DateFormatSymbols`/`TimeZone` text, not magnitudes — measured, `%tZ`
    // under `ar-EG` is the ASCII `IST` — and the two composites that MIX names
    // with numbers have already localized their own numeric slots above.
    // Width padding is applied after this, in ASCII spaces, which is also
    // HotSpot's order (`[%10tH]` under `ar-EG` is eight U+0020 then two
    // Arabic-Indic digits).
    if !matches!(field, 'B' | 'b' | 'h' | 'A' | 'a' | 'p' | 'Z' | 'c' | 'r') {
        out = fmt_localize_digits(&out, sym);
    }

    // `printDateTime`: `appendJustified(fmt.a, toUpperCaseWithLocale(sb, l))` —
    // the `%T` upper-caser is INSIDE the width and is locale-sensitive.
    if uppercase {
        out = fmt_upper_case(ctx, &out, locale);
    }

    // `_str_` adapter, not a second justifier: a `%t` field is a number, a
    // separator or a `DateFormatSymbols` name, none of which can be an unpaired
    // surrogate, so the `str` shape is not lossy on this path.
    fmt_pad_str_to_width(&out, flags, width).map(|v| (v, Some((fields, zone, support))))
}

// ---------------------------------------------------------------------------
// java.util.Formatter's floating-point conversions: %f %e %E %g %G %a %A
//
// These are NOT Rust's, and delegating to Rust's was the W7-1 divergence
// `probes/ShadowDifferentialProbe` measured against HotSpot 25:
//
//   String.format("%.3f|%e|%g", 1.0/3, 1234.5, 0.0001)
//     HotSpot   0.333|1.234500e+03|0.000100000
//     CratonVM  0.333|1.2345e3|1.0E-4
//
// Three separate reasons, one per conversion:
//
//   * `%e` went to Rust's `{:e}`, which writes the exponent bare (`e3`).
//     Formatter's is always signed and at least two digits (`e+03`), and its
//     default precision is 6 — the no-precision path did not even reach the
//     precision handling, so it printed the shortest round-trip mantissa.
//   * `%g` went to `{:.prec$}`, i.e. fixed notation, and with no precision it
//     fell through to `Double.toString` (hence `1.0E-4`). Formatter's %g is a
//     third algorithm: it picks scientific or fixed by the exponent of the
//     ROUNDED magnitude against the precision, and — unlike C's %g — never
//     strips trailing zeros, which is why 1e-4 prints as `0.000100000`.
//   * `%a` had no arm at all and fell through to `Double.toString`.
//
// Rounding is the fourth reason and it is not visible in that one probe line.
// Rust rounds ties to EVEN; java.util.Formatter specifies HALF_UP for
// %f/%e/%g, so `%.1f` of 0.25 is Java "0.3" against Rust's "0.2". Rather than
// correct that per call site, everything below does its own digit arithmetic
// (see `fmt_decimal_digits`), where HALF_UP is one comparison. %a is the
// exception: the JDK rounds it in BINARY, half to even, inside
// `Formatter.hexDouble`, and `fmt_hex_float` reproduces that rather than the
// decimal rule.
//
// `Double.toString`'s own 10^-3..10^7 scientific threshold (`format_double`,
// `cratonvm_types::java_double_to_string`) is a DIFFERENT set of rules and is
// deliberately not shared with any of this: %g at default precision switches
// to scientific below 10^-4, not below 10^-3. Only the DIGITS are shared — see
// `fmt_shortest_decimal` — never the layout.
// ---------------------------------------------------------------------------

/// The decimal digits a Java number STRING carries — most significant first,
/// no leading and no trailing zeros — plus the base-10 exponent of the leading
/// digit: `value == d0.d1d2… × 10^exp`. A zero of any spelling answers
/// `([0], 0)`; a leading `-` is ignored, since callers track the sign.
///
/// Accepts both spellings the two producers emit: `Double.toString`'s
/// `1.0E-4` / `123.45`, and `BigDecimal.toPlainString`'s never-scientific
/// form.
pub fn fmt_decimal_digits(s: &str) -> (Vec<u8>, i32) {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp10) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let (int_str, frac_str) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: Vec<u8> = int_str
        .bytes()
        .chain(frac_str.bytes())
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    // `exp` starts at the exponent of the first INTEGER-part digit and drops by
    // one for every leading zero skipped.
    let mut exp = int_str.len() as i32 - 1 + exp10;
    match digits.iter().position(|&d| d != 0) {
        None => (vec![0], 0),
        Some(lead) => {
            digits.drain(..lead);
            exp -= lead as i32;
            while digits.len() > 1 && digits[digits.len() - 1] == 0 {
                digits.pop();
            }
            (digits, exp)
        }
    }
}

/// A finite, non-negative `f64` as the digits `java.util.Formatter` rounds
/// from — which are `Double.toString`'s SHORTEST round-trip digits, not the
/// value's exact decimal expansion.
///
/// This is the spec, not an approximation of it. The javadoc for `%f`, `%e`
/// and `%g` all say the same sentence: "If the precision is less than the
/// number of digits which would appear after the decimal point in the string
/// returned by `Double#toString(double)`, then the value will be rounded using
/// the round half up algorithm. Otherwise, zeros may be appended to reach the
/// precision."
///
/// W7-3 rounded the EXACT expansion instead and recorded the difference as a
/// residual where "beyond roughly 20 significant digits this implementation is
/// more exact than HotSpot" — the guess being that `FloatingDecimal`'s
/// `char[20]` digit buffer was the cap. Measured against HotSpot 25 on
/// 2026-08-12, the cap is not 20 digits, it is the shortest representation, and
/// so the divergence starts at the FIRST digit past it rather than in some rare
/// tail:
///
/// | expression | HotSpot 25 | exact-expansion |
/// |---|---|---|
/// | `%.1f` of 0.35 | `0.4` | `0.3` (exact is 0.34999999999999997…) |
/// | `%.2f` of 1.005 | `1.01` | `1.00` (exact is 1.00499999999999989…) |
/// | `%.17f` of 0.1 | `0.10000000000000000` | `0.10000000000000001` |
/// | `%.3f` of 1.2345678901234569e23 | `123456789012345690000000.000` | `…685803008.000` |
///
/// Those first two are `format.floatRoundingHalfUp` and
/// `format.formatterAppendable` in `probes/ShadowDifferentialProbe.java`. The
/// exact expansion is the *better* number and the *wrong* answer: a caller who
/// asked `%.2f` of 1.005 and got 1.00 disagrees with every other Java runtime.
///
/// `format_double` is `Double.toString` (`cratonvm_types::java_double_to_string`),
/// which was measured byte-identical to HotSpot 25's on the values above — so
/// taking the digits from it is also what keeps `%s` and `%f` of the same value
/// telling the same story, which is precisely what the sentence above asks for.
pub fn fmt_shortest_decimal(v: f64) -> (Vec<u8>, i32) {
    fmt_decimal_digits(&format_double(v))
}

/// Round an exact digit string to `n` significant digits, HALF_UP — ties away
/// from zero, which is the rule `java.util.Formatter` names for %e/%f/%g.
///
/// Answers exactly `n` digits plus the exponent the caller must now use: 9.99
/// rounded to two digits carries into 10., i.e. gains a leading digit.
pub fn fmt_round_significant(digits: &[u8], exp: i32, n: usize) -> (Vec<u8>, i32) {
    let n = n.max(1);
    let mut out: Vec<u8> = digits.iter().copied().take(n).collect();
    out.resize(n, 0);
    // The discarded tail is >= half an ulp of the kept part exactly when its
    // first digit is >= 5 — including the "5 followed by nothing" tie, which is
    // the single case where half-even would round the other way.
    if digits.get(n).is_some_and(|&d| d >= 5) {
        let mut i = n;
        loop {
            if i == 0 {
                // Every kept digit was a 9: 999 -> 1000, one digit wider.
                out.insert(0, 1);
                out.pop();
                return (out, exp + 1);
            }
            i -= 1;
            if out[i] == 9 {
                out[i] = 0;
            } else {
                out[i] += 1;
                break;
            }
        }
    }
    (out, exp)
}

/// Round at a FRACTION-digit position rather than a significant-digit one —
/// what %f asks for, where the precision counts digits after the point.
pub fn fmt_round_at_fraction(digits: &[u8], exp: i32, frac: usize) -> (Vec<u8>, i32) {
    // The digit at 10^-frac is significant digit number `exp + 1 + frac`.
    let nsig = exp + 1 + frac as i32;
    if nsig >= 1 {
        fmt_round_significant(digits, exp, nsig as usize)
    } else if nsig == 0 {
        // Nothing survives but a possible carry: the leading digit IS the
        // rounding digit, so the answer is either zero or one unit in the last
        // place (0.06 at %.1f is 0.1).
        if digits.first().copied().unwrap_or(0) >= 5 {
            (vec![1], -(frac as i32))
        } else {
            (vec![0], 0)
        }
    } else {
        // |v| < 0.5 × 10^-frac: it rounds away entirely.
        (vec![0], 0)
    }
}

/// `digits × 10^exp` in plain decimal with exactly `frac` fraction digits.
/// Trailing zeros are WRITTEN, not trimmed — %f and %g both pad to the
/// precision.
pub fn fmt_render_fixed(digits: &[u8], exp: i32, frac: usize) -> String {
    let mut out = String::with_capacity(frac + 8);
    if exp < 0 {
        out.push('0');
    } else {
        for i in 0..=exp {
            out.push(char::from(
                b'0' + digits.get(i as usize).copied().unwrap_or(0),
            ));
        }
    }
    if frac > 0 {
        out.push('.');
        // Fraction digit j (1-based) sits at 10^-j, i.e. index `exp + j` into
        // `digits`; a negative index is one of the zeros after the point.
        for j in 1..=frac as i32 {
            let idx = exp + j;
            let d = if idx < 0 {
                0
            } else {
                digits.get(idx as usize).copied().unwrap_or(0)
            };
            out.push(char::from(b'0' + d));
        }
    }
    out
}

/// `digits × 10^exp` in Formatter's scientific form: one digit before the
/// point, `frac` after, then `e`/`E`, an ALWAYS-explicit sign, and at least two
/// exponent digits. Rust's `{:e}` writes neither the sign nor the padding,
/// which is the whole of the `1.2345e3` vs `1.234500e+03` divergence.
pub fn fmt_render_scientific(digits: &[u8], exp: i32, frac: usize, upper: bool) -> String {
    let mut out = String::with_capacity(frac + 8);
    out.push(char::from(b'0' + digits.first().copied().unwrap_or(0)));
    if frac > 0 {
        out.push('.');
        for j in 1..=frac {
            out.push(char::from(b'0' + digits.get(j).copied().unwrap_or(0)));
        }
    }
    out.push(if upper { 'E' } else { 'e' });
    out.push(if exp < 0 { '-' } else { '+' });
    out.push_str(&format!("{:02}", exp.unsigned_abs()));
    out
}

/// `Double.toHexString`'s digits for a finite, non-negative `v`, WITHOUT the
/// `0x` prefix that `Formatter` writes for itself: `1.0p0`, `0.0p0`,
/// `1.a36e2eb1c432dp-14`, or a subnormal's `0.<digits>p-1022`.
pub fn fmt_hex_digits(v: f64) -> String {
    if v == 0.0 {
        return "0.0p0".to_string();
    }
    let bits = v.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    // 52 significand bits are exactly 13 hex digits. `Double.toHexString` drops
    // trailing zeros but always keeps at least one digit ("0x1.0p0", not
    // "0x1.p0").
    let mut hex = format!("{frac:013x}");
    while hex.len() > 1 && hex.ends_with('0') {
        hex.pop();
    }
    if raw_exp == 0 {
        // Subnormal: no implicit leading 1, and the exponent is pinned.
        format!("0.{hex}p-1022")
    } else {
        format!("1.{hex}p{}", raw_exp - 1023)
    }
}

/// `java.util.Formatter.hexDouble` — %a's magnitude for a finite, non-negative
/// `v`, without the `0x` prefix.
///
/// `prec` is the JDK's already-normalised precision: 0 means "every digit"
/// (Formatter maps a MISSING precision to 0 and an explicit `%.0a` to 1), and
/// >= 13 is likewise every digit, because 13 hex digits is all a double has.
///
/// This is the one member of the family that is not HALF_UP. The JDK rounds
/// the SIGNIFICAND in binary, half to even — the round/sticky/least-significant
/// test below is `hexDouble`'s — and then re-renders the rounded double through
/// `Double.toHexString`. That is why `%.4a` of 1.0 is `0x1.0p0` and not
/// `0x1.0000p0`: nothing pads the digits back out afterwards.
pub fn fmt_hex_float(v: f64, prec: usize) -> String {
    if v == 0.0 || prec == 0 || prec >= 13 {
        return fmt_hex_digits(v);
    }
    // Subnormals carry no implicit leading 1, so normalise by 2^54 first and put
    // the exponent back afterwards — what `hexDouble` does for the same reason.
    let subnormal = (v.to_bits() >> 52) & 0x7ff == 0;
    let scaled = if subnormal { v * 2.0f64.powi(54) } else { v };

    // 1 implicit bit + 4 bits per hex digit kept, out of SIGNIFICAND_WIDTH = 53.
    // prec is 1..=12 here, so `shift` is 4..=48 and `shift - 1` is in range.
    let precision_bits = 1 + prec * 4;
    let shift = 53 - precision_bits as u32;
    let doppel = scaled.to_bits();
    // Exponent and significand together, sign masked off (v >= 0 anyway).
    let mut new_signif = (doppel & 0x7fff_ffff_ffff_ffff) >> shift;
    let rounding_bits = doppel & !(!0u64 << shift);
    let least_zero = new_signif & 1 == 0;
    let round = ((1u64 << (shift - 1)) & rounding_bits) != 0;
    let sticky = shift > 1 && (!(1u64 << (shift - 1)) & rounding_bits) != 0;
    if (least_zero && round && sticky) || (!least_zero && round) {
        new_signif += 1;
    }
    let rounded = f64::from_bits(new_signif << shift);
    if rounded.is_infinite() {
        // The carry ran out of the exponent field; `hexDouble` hard-codes this.
        return "1.0p1024".to_string();
    }
    let res = fmt_hex_digits(rounded);
    if !subnormal {
        return res;
    }
    // Undo the 2^54 normalisation in the printed exponent.
    match res.find('p') {
        Some(idx) => {
            let e = res[idx + 1..].parse::<i32>().unwrap_or(0) - 54;
            format!("{}p{e}", &res[..idx])
        }
        None => res,
    }
}

/// `Formatter.addZeros` over a `digits.digitsPexp` hex float: pad the
/// fractional hex digits out to `prec`, leaving the exponent alone.
///
/// `prec == 0` is the JDK's "all of the digits", where nothing is padded.
/// This applies above 13 too — `hexDouble` stops ROUNDING at 13 hex digits
/// but `addZeros` still pads, so `%.14a` of `Double.MIN_VALUE` is
/// `0x0.00000000000010p-1022`.
pub fn fmt_hex_pad(s: &str, prec: usize) -> String {
    if prec == 0 {
        return s.to_string();
    }
    let idx = match s.find('p') {
        Some(i) => i,
        None => return s.to_string(),
    };
    let (mant, exp) = s.split_at(idx);
    let have = match mant.find('.') {
        Some(dot) => mant.len() - dot - 1,
        // `fmt_hex_digits` always writes a point, but `addZeros` adds one when
        // it has to and this stays faithful to that.
        None => return format!("{mant}.{}{exp}", "0".repeat(prec)),
    };
    if have >= prec {
        return s.to_string();
    }
    format!("{mant}{}{exp}", "0".repeat(prec - have))
}

/// One value through `java.util.Formatter`'s floating-point conversions.
///
/// `precision` is the spec's precision if it carried one. The DEFAULTS live
/// here rather than at the call sites because the no-precision path used to
/// bypass precision handling entirely and answer Rust's `{:e}` /
/// `Double.toString`; a default that only exists on the with-precision branch
/// is not a default.
pub fn java_float_conversion(v: f64, spec: char, precision: Option<usize>) -> String {
    let upper = matches!(spec, 'E' | 'G' | 'A');
    // Formatter tests NaN before it reads the sign, so a NaN never prints one.
    if v.is_nan() {
        return if upper { "NAN" } else { "NaN" }.to_string();
    }
    // The sign test is `Double.compare(value, 0.0) == -1`, so -0.0 DOES print
    // its sign: `%f` of -0.0 is "-0.000000".
    let sign = if v.is_sign_negative() { "-" } else { "" };
    if v.is_infinite() {
        return format!("{sign}{}", if upper { "INFINITY" } else { "Infinity" });
    }
    let mag = v.abs();
    if matches!(spec, 'a' | 'A') {
        // Formatter emits the prefix itself and upper-cases the digits and
        // the 'p' for %A; the exponent is decimal either way.
        //
        // The JDK normalises a MISSING precision to 0 ("assume that we want all
        // of the digits") and an explicit `%.0a` to 1, then pads the mantissa
        // back out to it — `if (prec != 0) addZeros(va, prec)`, outside
        // `hexDouble`. W7-3 argued the opposite from the javadoc, that "nothing
        // pads the digits back out, and `%.4a` of 1.0 is `0x1.0p0`, not
        // `0x1.0000p0`". Measured on HotSpot 25 on 2026-08-12 it is
        // `0x1.0000p0`: a 1763-case sweep of the float family against HotSpot
        // failed on this row and nothing else.
        let prec = precision.map_or(0, |p| p.max(1));
        let digits = fmt_hex_pad(&fmt_hex_float(mag, prec), prec);
        let body = if upper {
            format!("0X{}", digits.to_uppercase())
        } else {
            format!("0x{digits}")
        };
        return format!("{sign}{body}");
    }
    let (digits, exp) = fmt_shortest_decimal(mag);
    format!(
        "{sign}{}",
        fmt_decimal_conversion(&digits, exp, spec, precision)
    )
}

/// A `java.math.BigDecimal`'s `toPlainString()` through %f/%e/%g.
///
/// Formatter formats a BigDecimal from its OWN digits — `print(BigDecimal,
/// Locale)` never converts it to a double — so `%.2f` of `new
/// BigDecimal("2.345")` is `2.35`, the HALF_UP rounding of the exact literal
/// 2.345, and not the 2.34 that the nearest double (2.34499999999999975…)
/// would give. `%a` has no BigDecimal arm at all in the JDK and raises
/// `IllegalFormatConversionException`; the caller rejects it before reaching
/// here.
///
/// The sign follows `signum()`, not the leading character: `new
/// BigDecimal("-0.00")` has signum 0 and prints without a sign.
pub fn java_decimal_conversion(plain: &str, spec: char, precision: Option<usize>) -> String {
    let (digits, exp) = fmt_decimal_digits(plain);
    let negative = plain.starts_with('-') && digits.iter().any(|&d| d != 0);
    let sign = if negative { "-" } else { "" };
    format!(
        "{sign}{}",
        fmt_decimal_conversion(&digits, exp, spec, precision)
    )
}

/// %f / %e / %g over an already-extracted non-negative magnitude
/// `digits × 10^exp`, which is the only part of the family that does not care
/// where the digits came from — a double's `Double.toString` or a
/// BigDecimal's `toPlainString`.
pub fn fmt_decimal_conversion(digits: &[u8], exp: i32, spec: char, precision: Option<usize>) -> String {
    let upper = matches!(spec, 'E' | 'G');
    match spec {
        'e' | 'E' => {
            let prec = precision.unwrap_or(6);
            // One digit before the point plus `prec` after it.
            let (digits, exp) = fmt_round_significant(digits, exp, prec + 1);
            fmt_render_scientific(&digits, exp, prec, upper)
        }
        'g' | 'G' => {
            // Javadoc: the precision defaults to 6, a precision of 0 is taken to
            // be 1, and it counts SIGNIFICANT digits rather than fraction ones.
            let prec = match precision {
                None => 6,
                Some(0) => 1,
                Some(p) => p,
            };
            if digits.iter().all(|&d| d == 0) {
                // Formatter special-cases zero to mantissa "0" with a rounded
                // exponent of 0, which lands in the decimal branch: `%g` of 0.0
                // is "0.00000", never "0.000000e+00".
                fmt_render_fixed(&[0], 0, prec - 1)
            } else {
                // The branch is decided by the magnitude AFTER rounding, which
                // is why 999999.5 at `%g` prints 1.00000e+06 and not 1000000.
                let (digits, exp) = fmt_round_significant(digits, exp, prec);
                if exp >= -4 && exp < prec as i32 {
                    fmt_render_fixed(&digits, exp, (prec as i32 - 1 - exp) as usize)
                } else {
                    fmt_render_scientific(&digits, exp, prec - 1, upper)
                }
            }
        }
        // 'f', and any other spec routed here by a caller that already decided
        // this argument is a float.
        _ => {
            let prec = precision.unwrap_or(6);
            let (digits, exp) = fmt_round_at_fraction(digits, exp, prec);
            fmt_render_fixed(&digits, exp, prec)
        }
    }
}

/// Format a single argument with flags, width, and precision support.
#[allow(clippy::too_many_arguments)]
pub fn format_arg_full(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
) -> Result<Vec<u16>, MethodCallFailed> {
    // gen r4w3/rooting: the argument is used again AFTER upcalls on it —
    // `float_source` and the BigInteger class test run after `format_arg`'s
    // `toString`/`toPlainString`, and the general branch after a declined
    // `Formattable` dispatch — any of which can move it. Pin it for the whole
    // conversion; the body re-derives it through the pin before each use.
    let val_pin = match val {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
        _ => None,
    };
    let out = format_arg_full_pinned(
        ctx, val, val_pin, spec, flags, width, precision, sym, locale,
    );
    if let Some(h) = val_pin {
        ctx.unpin_native_roots(h);
    }
    out
}

/// The body of [`format_arg_full`]. `val` is only guaranteed current on entry;
/// after any upcall it is re-derived through `val_pin`.
#[allow(clippy::too_many_arguments)]
pub fn format_arg_full_pinned(
    ctx: &mut dyn NativeContext,
    val: &Value,
    val_pin: Option<usize>,
    spec: char,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    sym: FmtSymbols,
    locale: FmtLocale,
) -> Result<Vec<u16>, MethodCallFailed> {
    // gen r4w3/rooting: the argument at its CURRENT address.
    let cur_val = |ctx: &dyn NativeContext| -> Value {
        match (val_pin, val) {
            (Some(h), Value::Object(Some(o))) => Value::Object(Some(ctx.read_native_pin(h, *o))),
            _ => val.clone(),
        }
    };
    // Uppercase string-family conversions ('S'/'B'/'C'/'H') format identically
    // to their lowercase form, then the whole result is upper-cased — per
    // java.util.Formatter's "If the conversion is 'S', 'B' or 'C' … the result is
    // converted to upper case". (The numeric uppercase conversions 'X'/'E'/'G'/'A'
    // already emit upper-case digits in `format_arg`, so they are NOT remapped
    // here.) Format with the lowercase spec, then upper-case — BEFORE the width
    // justifier, which is where `print(Formatter, String, Locale)` does it and
    // which matters whenever the mapping changes the length; see that step.
    // Without 'S' support, Groovy's `"COMMERCIAL_%SREPO_URL".formatted(id)` left
    // the specifier literal, so the env-var key never matched (SpringRepos).
    //
    // 'H' was MISSING from this list, and the omission cost three separate
    // answers, because everything downstream keys off the remapped `spec`:
    // `%H` never upper-cased at all, `%H` of null was "null" where HotSpot 25
    // writes "NULL", and `%.2H` never truncated because the precision arm
    // below matches 's'/'b'/'h'. One line, three divergences — which is the
    // shape of a table populated by hand.
    // Whether the conversion AS WRITTEN was an upper-case one. Not the same
    // question as `uppercase_result` below, which asks only whether this
    // function has to case the result itself: `%X`/`%E`/`%G`/`%A` already emit
    // upper-case digits from `format_arg`, so they are not remapped — but they
    // ARE upper-case conversions, and the null path a few lines down is the
    // place where that distinction is load-bearing.
    let uppercase_conversion = spec.is_ascii_uppercase();
    let (spec, uppercase_result) = match spec {
        'S' => ('s', true),
        'B' => ('b', true),
        'C' => ('c', true),
        'H' => ('h', true),
        other => (other, false),
    };

    // `printString`'s FIRST line, ahead of both the '#' refusal and the null
    // test: "If the argument implements Formattable, then its formatTo method
    // is invoked" — and it, not this table, writes the output. The flags,
    // width and precision are handed over and nothing is applied on top: a
    // `Formattable` that writes nothing produces nothing even under `%10s`
    // (measured `"[%10s]"` -> `"[]"`), and one that throws propagates.
    if spec == 's' {
        if let Value::Object(Some(obj)) = val {
            if let Some(out) = fmt_formattable_dispatch(
                ctx,
                *obj,
                flags,
                width,
                precision,
                uppercase_conversion,
                locale,
            )? {
                // The callee's output VERBATIM, in code UNITS. Measured on
                // HotSpot 25: a `formatTo` that writes `"x\uD800y"` answers
                // `length() == 3` / `0078 D800 0079` under `%s`, `%S` AND
                // `%-10s`, so nothing is applied on top and nothing may be
                // lost on the way — which is why the read inside
                // `fmt_formattable_dispatch` is `read_string_chars`.
                return Ok(out);
            }
        }
        // `failMismatch(Flags.ALTERNATE, 's')`, in `printString`'s `else` —
        // so it fires for a non-`Formattable` argument INCLUDING a null one
        // (`%#s` of null is a mismatch on HotSpot 25, not "null"), and after
        // the argument fetch (`%#s` with no argument is
        // `MissingFormatArgumentException`). See `fmt_check_spec`, which used
        // to raise this and now deliberately does not.
        if flags.contains('#') {
            return Err(fmt_raise(
                ctx,
                &FmtFault::FlagsMismatch("#".to_string(), 's'),
            ));
        }
    }

    // A NULL argument converges on ONE printer for every conversion in the
    // table. `printInteger`, `printFloat`, `printCharacter`, `printString`,
    // `printHashCode` and `printDateTime` each open with
    //
    //     if (arg == null) { print(fmt, "null", l); return; }
    //
    // and `printBoolean` reaches the same `print(Formatter, String, Locale)`
    // with "false" instead. That printer does exactly three things — truncate
    // to the precision, upper-case when the conversion is an upper-case one,
    // and SPACE-justify to the width — so none of the numeric decoration below
    // this point applies to a null. Every one of the following was measured on
    // HotSpot 25 and every one of them was wrong here:
    //
    // * `%+d` / `% d` of null is "null", not "+null" / " null".
    // * `%#x` / `%#o` of null is "null", not "0xnull" / "0null".
    // * `%08d` / `%08x` / `%08e` of null is "    null" — padded with SPACES,
    //   because zero padding lives in the numeric printers this never enters.
    // * `%.2f` of null is "nu" and `%,(.2f` is "nu": the precision truncates a
    //   null argument even on the float conversions, whose own precision has
    //   nothing to do with a character count.
    // * `%X`, `%E`, `%G`, `%A` of null are "NULL", because the upper-casing is
    //   this shared printer's, not the digit rendering's.
    //
    // The flag LEGALITY checks are unaffected and have already run in
    // `fmt_check_spec`: `%08s` of null is still a FormatFlagsConversionMismatch
    // on HotSpot. The one exception is the '(' / '+' / ' ' refusal for %o/%x,
    // which is inside `print(long, Locale)` and so is skipped for a null — see
    // the note below it.
    if matches!(val, Value::Object(None)) {
        // `%b`/`%B` is the one conversion whose null is not the word "null" —
        // the rule lives in `fmt_null_text` so that `format_arg`'s own null arm
        // cannot drift from it.
        // `print(Formatter, String, Locale)`'s three steps, IN ITS ORDER:
        // truncate, then upper-case, then justify. The order is observable
        // because the upper-caser can GROW the text past the precision —
        // measured on HotSpot 25, `String.format(ROOT, "%.1S", "ß")` is `"SS"`,
        // length 2, not `"S"`. (It does not bite on the word "null", which is
        // why an ASCII-only row cannot tell the two orders apart.)
        let mut units: Vec<u16> = fmt_null_text(spec).encode_utf16().collect();
        if let Some(p) = precision {
            fmt_truncate_units(&mut units, p);
        }
        if uppercase_conversion {
            units = fmt_upper_case_units(ctx, &units, locale);
        }
        return fmt_pad_to_width(units, flags, width);
    }

    // The GENERAL family ('s', 'b', 'h') and the character conversion ('c')
    // are `print(Formatter, String, Locale)` and nothing else: truncate to the
    // precision, upper-case when the conversion was written upper-case, and
    // justify to the width. Splitting them out here is not a shortcut — every
    // step below this point is inapplicable to them by the JDK's own structure,
    // and each of the four exclusions is enforced somewhere already:
    //
    //   * grouping (','), '#', '(' , '+' and ' ' are refused for these
    //     conversions by `fmt_check_spec` before an argument is even fetched;
    //   * the zero-padding and localization arms below list only the numeric
    //     specs ('d','f','e','E','g','G','x','X','o','a','A'); and
    //   * the BigInteger sign handling is reached only from %o/%x/%X.
    //
    // What the split BUYS is that the three steps that do apply can be done in
    // UTF-16 code UNITS, and all three can produce a LONE SURROGATE that a Rust
    // `String` cannot hold:
    //
    //   * the truncation splits a surrogate pair — `%.1s` of U+1F600 is a lone
    //     high surrogate with `length() == 1` on HotSpot 25, so `[%5.1s]` is
    //     SEVEN characters; see `fmt_truncate_units` for the measured table;
    //   * the argument itself may contain one — `%s` of `"x\uD800y"` is three
    //     units on HotSpot, and was `x` + U+FFFD + `y` here; and
    //   * `%c` of a lone-surrogate code point is that surrogate — HotSpot
    //     answers `length() == 1`, `D800`, where this answered '?'.
    //
    // The order is the JDK's and is observable: the upper-caser runs AFTER the
    // truncation and may grow the result past the precision (`%.1S` of "ß" is
    // "SS"), and BEFORE the justifier, which is what makes `%5S` of "ß" five
    // units rather than six.
    if matches!(spec, 's' | 'b' | 'h' | 'c') {
        // gen r4w3/rooting: a declined `Formattable` dispatch above may have
        // run bytecode first.
        let arg_now = cur_val(&*ctx);
        let mut units = fmt_general_units(ctx, &arg_now, spec)?;
        // `checkCharacter` refuses a precision on %c outright, so only the
        // 's'/'b'/'h' members can arrive here with one — the same set the
        // `String`-shaped arm this replaced matched on.
        if let Some(p) = precision {
            fmt_truncate_units(&mut units, p);
        }
        if uppercase_result {
            units = fmt_upper_case_units(ctx, &units, locale);
        }
        return fmt_pad_to_width(units, flags, width);
    }

    // Get the raw formatted value first
    let raw = {
        // gen r4w3/rooting: current address.
        let arg_now = cur_val(&*ctx);
        format_arg(ctx, &arg_now, spec)?
    };

    // Apply the spec's own precision for the floating-point conversions,
    // overriding the default `format_arg` just applied. Every one of them goes
    // through the same `java_float_conversion` the no-precision path uses — the
    // arm this replaced sent %e to Rust's `{:.prec$e}` (bare exponent) and
    // %g/%a to `{:.prec$}` (fixed notation), which is W7-1's float row.
    let raw = match spec {
        // A null argument never reaches the conversion at all: Formatter's
        // `printFloat` prints "null" before it looks at the spec, and
        // `extract_float_value` would have answered 0.0. The null test is now
        // redundant — the shared-null-printer branch above returns first — and
        // is kept because it is the reason this arm is CORRECT, not merely the
        // mechanism: `float_source` of a null is `None`, which would fall
        // through to `raw` and silently drop the precision.
        'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A'
            if precision.is_some() && !matches!(val, Value::Object(None)) =>
        {
            // The precision is one character of output per unit, and it is
            // caller data with no upper bound but `Integer.MAX_VALUE` —
            // `%.2000000000f` is a legal specifier. See `fmt_reserve_probe`
            // for the HotSpot rows; this is the ONE point at which a precision
            // reaches `fmt_render_fixed`/`fmt_render_scientific`/`fmt_hex_pad`,
            // so asking here keeps all three non-fallible.
            fmt_reserve_probe(precision.unwrap_or(0))?;
            // gen r4w3/rooting: `format_arg` above ran `toPlainString` on a
            // BigDecimal, so the entry-time address may be stale.
            let arg_now = cur_val(&*ctx);
            match float_source(ctx, &arg_now) {
                // `%.Nf` of a BigDecimal must round the BigDecimal's OWN
                // digits, which is why the source is asked for rather than
                // `extract_float_value`d into a double.
                Some(FloatSource::Decimal(plain)) => {
                    java_decimal_conversion(&plain, spec, precision)
                }
                Some(FloatSource::Double(v)) => java_float_conversion(v, spec, precision),
                None => raw,
            }
        }
        // "The precision is the maximum number of characters to be written to
        // the output" — for every GENERAL conversion, not just %s (`%.2b` of
        // true is "tr"). That arm moved UP, into the units-carrying general
        // branch above, and is `fmt_truncate_units` there; 's'/'b'/'h'/'c'
        // cannot reach this point any more.
        _ => raw,
    };

    // `java.math.BigInteger` is the one argument class whose %o/%x/%X goes
    // through a printer that renders a SIGN, so it is the one class for which
    // '(' / '+' / ' ' are legal on those conversions. See the refusal below.
    // gen r4w3/rooting: re-derived — `format_arg`'s `toString(I)` upcall can
    // have moved the argument, and a stale header names the wrong class.
    let big_integer = match cur_val(&*ctx) {
        Value::Object(Some(o)) => {
            ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref()
                == Some("java/math/BigInteger")
        }
        _ => false,
    };

    // `print(long, Locale)` opens its %o and %x arms with
    // `checkBadFlags(PARENTHESES | LEADING_SPACE | PLUS)` — INSIDE the printer,
    // not in `checkInteger`, and that placement is the entire behaviour:
    //
    // * `print(BigInteger, Locale)` has no such check and calls
    //   `leadingSign`/`trailingSign` like the decimal arm does, so `%(x` of
    //   `BigInteger("-255")` is "(ff)", `%+x` of `BigInteger("255")` is "+ff"
    //   and `% o` of `BigInteger("8")` is " 10" on HotSpot 25.
    // * a NULL argument never reaches either printer — `printInteger` writes
    //   "null" first — so `String.format("%(x", (Object) null)` is "null" and
    //   not a refusal. That case has already returned above.
    // * a wrong-typed argument fails `printInteger`'s dispatch first, so
    //   `%(x` of a String is IllegalFormatConversionException, not a flags
    //   mismatch — hence this standing AFTER `format_arg`.
    //
    // Hoisting it into `fmt_check_spec` — where it used to live, next to the
    // ',' refusal that really is argument-independent — refused all three of
    // those cases. The check has to stand HERE, after the argument's class is
    // known, because that is where the JDK put it.
    if matches!(spec, 'o' | 'x' | 'X') && !big_integer {
        let offending: String = fmt_flags_string(flags)
            .chars()
            .filter(|c| "( +".contains(*c))
            .collect();
        if !offending.is_empty() {
            return Err(fmt_raise(
                ctx,
                &FmtFault::FlagsMismatch(offending, spec.to_ascii_lowercase()),
            ));
        }
    }

    // Apply width and flags
    let left_justify = flags.contains('-');
    let zero_pad = flags.contains('0') && !left_justify;
    let numeric_sign = matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A')
        || (big_integer && matches!(spec, 'o' | 'x' | 'X'));

    let mut formatted = raw;

    // ',' grouping flag: insert a thousands separator into the integer part of
    // %d / %f values. The separator inserted here is the ASCII ',' whatever the
    // locale is; `fmt_localize` rewrites it at the very end, once every
    // length-sensitive step is done — see that function for why.
    if flags.contains(',') && matches!(spec, 'd' | 'f' | 'g' | 'G') {
        formatted = group_thousands(&formatted);
    }

    // `Formatter.leadingSign`/`trailingSign`. A negative value with the '('
    // flag is written in accountancy form — "the result will enclose negative
    // numbers in parentheses" — and the '-' disappears rather than being kept
    // alongside; a positive value takes '+' or, failing that, the ' ' flag's
    // leading space. `%x`/`%o` reject all three flags above unless the
    // argument is a BigInteger, whose printer applies them exactly as the
    // decimal one does — `%(x` of -255 is "(ff)", the two's-complement
    // rendering having been replaced by `abs().toString(16)` upstream.
    //
    // A NaN takes none of the three: `print(Formatter, double, Locale)` calls
    // `leadingSign`/`trailingSign` only inside `if (!Double.isNaN(value))`, so
    // `% f` and `%+f` of NaN are "NaN" on HotSpot 25 (this answered " NaN" /
    // "+NaN"; JIT round 20, lane huntIntr, `R20HuntIntrWrappers` fmt.24).
    let is_nan_rendering = formatted == "NaN" || formatted == "NAN";
    if numeric_sign && !is_nan_rendering {
        if let Some(magnitude) = formatted.strip_prefix('-') {
            if flags.contains('(') {
                formatted = format!("({magnitude})");
            }
        } else if flags.contains('+') {
            formatted = format!("+{formatted}");
        } else if flags.contains(' ') {
            formatted = format!(" {formatted}");
        }
    }

    // '#' alternate form. Only the radix conversions have one here: "the output
    // will always begin with the radix indicator '0x'" for %x (and '0X' for
    // %X), and "the output will always begin with a '0'" for %o. It goes on
    // BEFORE the zero padding, so `%#010x` of 255 is `0x000000ff` and not
    // `00000000xff` — hence the prefix-aware split below.
    //
    // The indicator goes INSIDE any leading sign, because the JDK appends it
    // to a StringBuilder that `leadingSign` has already written into: `%#x` of
    // `BigInteger("-255")` is "-0xff" and `%#(x` is "(0xff)". Inserting at
    // byte 0 unconditionally was invisible while %x/%o refused every sign
    // flag; admitting BigInteger above is what makes the position matter.
    if flags.contains('#') {
        let at = usize::from(formatted.starts_with(['-', '+', ' ', '(']));
        match spec {
            'x' => formatted.insert_str(at, "0x"),
            'X' => formatted.insert_str(at, "0X"),
            'o' => formatted.insert(at, '0'),
            _ => {}
        }
    }

    // `print(Formatter, String, Locale)` upper-cases BEFORE it justifies, and
    // the order is load-bearing whenever the mapping changes the LENGTH: a
    // full uppercase mapping can grow a string (U+00DF -> "SS"), and
    // `String.format(ROOT, "%5S", "\u{00DF}")` is `"   SS"` on HotSpot 25 —
    // five units, three spaces. Upper-casing after the pad, which is where
    // this step used to sit, produced SIX. Locale-sensitive since this lane;
    // see `fmt_upper_case`.
    //
    // Only the general family reaches this: 'X'/'E'/'G'/'A' are not remapped
    // (they emit upper-case digits from `format_arg`), so `uppercase_result`
    // is true only for the remapped 'S'/'B'/'C'/'H', none of which takes the
    // grouping, sign, '#' or localization steps above.
    //
    // ...which is exactly why the step now lives in the units-carrying general
    // branch near the top of this function and NOT here: 'S'/'B'/'C'/'H' are
    // the complete set that reached it, and all four return before this point.
    // `uppercase_result` is therefore false for everything that gets this far,
    // and the guard was kept only as a place for a second copy of the rule to
    // grow. The ordering constraint it documents is unchanged and is asserted
    // in the branch above; the `debug_assert` is what keeps a future
    // conversion from being added to the remap table without its upper-caser.
    debug_assert!(
        !uppercase_result,
        "an upper-case-remapped conversion reached the numeric decoration path"
    );

    // `trailingZeros` — the numeric printers' own zero padding, which the JDK
    // does INSIDE the printer, before `appendJustified` ever sees the buffer.
    // Once it has run the buffer is already `width` units long, so the shared
    // justifier below pads nothing; that is why the two are not one step.
    if let Some(w) = width {
        let len = fmt_utf16_len(&formatted);
        // %g/%G were missing from the zero-pad set even though Formatter
        // accepts '0' for them. %a/%A are IN it since 2026-08-12: their
        // zeros go after the "0x" prefix, which the prefix-aware split
        // below now does — `%020a` of 1.0 is `0x00000000000001.0p0`, and
        // with a sign the zeros go after BOTH (`%+020a` is
        // `+0x0000000000001.0p0`). Before this they fell through to the
        // justifier and got leading spaces. W7-3 left this as the family's
        // last flag defect; it needed the `lead` split W7-34 added for
        // `%#010x`, which is why the two waves could not close it together.
        //
        // The non-finite test stands in for Formatter's structure, where
        // zero padding happens only inside the FINITE branch —
        // "Infinity"/"NaN" reach the width justifier and get spaces. It
        // used to be "ends with an ASCII digit", which mistook two finite
        // renderings for infinities the moment this lane gave them
        // non-digit tails: `%#010x` ends in a HEX digit and `%(08d` ends in
        // the closing parenthesis, and both silently reverted to space
        // padding.
        if len < w
            && zero_pad
            && matches!(
                spec,
                'd' | 'f' | 'e' | 'E' | 'g' | 'G' | 'x' | 'X' | 'o' | 'a' | 'A'
            )
            && !formatted.ends_with("Infinity")
            && !formatted.ends_with("INFINITY")
            && !formatted.ends_with("NaN")
            && !formatted.ends_with("NAN")
        {
            // The zeros go INSIDE whatever the value already leads with —
            // a sign, an opening parenthesis, or a radix indicator — never
            // in front of it. The two stack: `%a` puts the sign OUTSIDE the
            // `0x` prefix (`+0x1.0p0`), so a signed hex float leads with
            // three characters, and testing the radix indicator only at
            // byte 0 would have put the zeros in front of the `0x`.
            let mut lead = 0usize;
            if formatted.starts_with(['-', '+', ' ', '(']) {
                lead = 1;
            }
            if formatted[lead..].starts_with("0x") || formatted[lead..].starts_with("0X") {
                lead += 2;
            }
            let zeros = fmt_repeat('0', w - len)?;
            formatted.insert_str(lead, &zeros);
        }
    }

    // Localization comes after the zero padding so that the padding zeros are
    // localized too — `trailingZeros` appends the locale's OWN zero digit, and
    // `fmt_localize` is what supplies it here. Every width decision above was
    // still made on ASCII, and "no localization is applied" to %x, %o and %a,
    // nor to a %s argument's digits, which are the caller's text.
    if matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') {
        formatted = fmt_localize(&formatted, sym);
    }

    // `appendJustified`, shared with `%t`/`%T` and the null path. Spaces are
    // not digits, so standing after `fmt_localize` rather than before it is
    // the same answer.
    //
    // The `_str_` adapter, not a second justifier: everything that reaches here
    // is a NUMERIC conversion whose rendering is ASCII digits, signs, a radix
    // indicator and the locale's own separator characters — none of which can
    // be an unpaired surrogate, so the `str` shape is not lossy on this path.
    fmt_pad_str_to_width(&formatted, flags, width)
}

/// Insert ',' thousands separators into the integer part of a numeric string
/// (for the `%,d` / `%,f` grouping flag). Preserves a leading sign and any
/// fractional part (`.xxx`).
pub fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac_part) = match rest.find('.') {
        Some(p) => (&rest[..p], &rest[p..]),
        None => (rest, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit()) || int_part.len() <= 3 {
        return s.to_string();
    }
    let digits: Vec<char> = int_part.chars().collect();
    let len = digits.len();
    let mut grouped = String::with_capacity(len + len / 3);
    for (idx, ch) in digits.iter().enumerate() {
        // Comma before this digit when the number of digits remaining (incl.
        // this one) is a positive multiple of 3, and it's not the leading digit.
        if idx != 0 && (len - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*ch);
    }
    format!("{sign}{grouped}{frac_part}")
}

/// Where a float conversion's digits come from.
///
/// `java.util.Formatter.print(Object, Locale)` dispatches the float family to
/// two different printers — `print(double, …)` and `print(BigDecimal, …)` —
/// and they are not the same algorithm. Collapsing a BigDecimal into a double
/// first would round twice and lose the very digits the caller chose a
/// BigDecimal to keep.
pub enum FloatSource {
    Double(f64),
    /// A `java.math.BigDecimal`, as its `toPlainString()`.
    Decimal(String),
}

/// Classify a `%f`/`%e`/`%g` argument, unboxing wrappers as needed.
///
/// `None` means the argument is not one of the types the float conversions
/// accept — the caller raises `IllegalFormatConversionException` rather than
/// inventing a value for it.
pub fn float_source(ctx: &mut dyn NativeContext, val: &Value) -> Option<FloatSource> {
    match val {
        Value::Float(v) => Some(FloatSource::Double(*v as f64)),
        Value::Double(v) => Some(FloatSource::Double(*v)),
        Value::Int(v) => Some(FloatSource::Double(*v as f64)),
        Value::Long(v) => Some(FloatSource::Double(*v as f64)),
        Value::Object(Some(obj)) => {
            let cname = ctx
                .class_name_of_id(ctx.class_id_of_object(*obj))
                .unwrap_or_default();
            if cname == "java/math/BigDecimal" {
                // `toPlainString`, not `toString`: the latter switches to
                // scientific notation for some scales, and `fmt_decimal_digits`
                // would then have to trust an exponent this path can avoid
                // producing at all.
                return match ctx.invoke_virtual(*obj, "toPlainString", "()Ljava/lang/String;", &[])
                {
                    Ok(Some(Value::Object(Some(s)))) => {
                        ctx.read_string(s).map(FloatSource::Decimal)
                    }
                    _ => None,
                };
            }
            if !matches!(cname.as_str(), "java/lang/Float" | "java/lang/Double") {
                return None;
            }
            match ctx.get_field(*obj, 0) {
                Value::Float(v) => Some(FloatSource::Double(v as f64)),
                Value::Double(v) => Some(FloatSource::Double(v)),
                Value::Int(v) => Some(FloatSource::Double(v as f64)),
                Value::Long(v) => Some(FloatSource::Double(v as f64)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The raw rendering of a GENERAL-family argument ('s', 'b', 'h') or of the
/// character conversion ('c'), as UTF-16 code **units**.
///
/// Everything here is [`format_arg`]'s answer; the two departures are the two
/// places `format_arg`'s `String` return type destroys a code unit.
///
/// **`%s` of a `java/lang/String`.** `String.valueOf(arg)` on a String is the
/// argument's own characters, and `read_string_chars` hands them over without
/// the UTF-8 round trip that turns each unpaired surrogate into U+FFFD.
/// Measured on HotSpot 25: `String.format(ROOT, "%s", "x\uD800y")` has
/// `length() == 3` and units `0078 D800 0079`.
///
/// The class test is the SAME one `format_arg`'s own `%s` arm makes (`is_string`
/// there) rather than a second copy of it: this function runs first and returns,
/// so on a real String that arm is no longer reached, and on everything else
/// this falls through to it. Note it is a class-IDENTITY test, not
/// `is_subclass` — `java.lang.String` is final, and a subclass test would be
/// both wider and slower.
///
/// Taking this route also means a `java/lang/String` argument never reaches
/// `fmt_formattable_dispatch`'s interface probe — F13's suggested `%s` fast
/// path, obtained by hoisting the comparison that already existed further down
/// this file rather than adding a second one. **The speedup is UNMEASURED**;
/// the change was made because it is the lossless path anyway, and it is
/// behaviour-identical because `java.lang.String` does not implement
/// `java.util.Formattable` and is final, so the probe it skips could only ever
/// have answered "no".
///
/// **`%c` of a lone-surrogate code point.** `Character.isValidCodePoint`
/// admits U+D800..U+DFFF and `Character.toChars` hands one back as a single
/// unpaired `char`; `char::from_u32` refuses it, so the arm in [`format_arg`]
/// substituted '?'. Measured on HotSpot 25:
/// `String.format(ROOT, "%c", (int) 0xD800)` has `length() == 1` and unit
/// `D800`. [`format_arg`] still owns the VALIDATION — it is called first and
/// its `IllegalFormatCodePointException` / `IllegalFormatConversionException`
/// refusals propagate unchanged — and this only RE-RENDERS an argument it has
/// already accepted, so the two cannot drift on which code points are legal.
pub fn fmt_general_units(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
) -> Result<Vec<u16>, MethodCallFailed> {
    if spec == 's' {
        if let Value::Object(Some(obj)) = val {
            // t1 wave 8 lane RL-REFL: a reference ARRAY's class id is its
            // COMPONENT class (`NativeContext::object_is_array`), so a
            // `String[]` argument read as a String; HotSpot formats it as
            // `[Ljava.lang.String;@<hash>` through `toString()` below.
            if !ctx.object_is_array(*obj)
                && ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(*obj))
            {
                return Ok(read_string_chars(ctx, *obj));
            }
            // Every OTHER object: `%s` is `String.valueOf(arg)` ==
            // `arg.toString()`, and that result can hold an unpaired surrogate
            // exactly as a String argument can -- `String.format("%s", x)` where
            // `x.toString()` returns one, and any boxed `Character` holding one.
            // `format_arg` renders it and hands back a Rust `String`, so the
            // unit was already gone by the time this function saw it.
            //
            // These are the SAME two calls `format_arg`'s own `%s` arm makes,
            // in the same order, differing only in reading the result as units
            // -- so the two cannot disagree about anything except the loss.
            return match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => Ok(ctx
                    .read_string_units(s)
                    .unwrap_or_else(|| "null".encode_utf16().collect())),
                Ok(_) => Ok("null".encode_utf16().collect()),
                Err(err) => Err(err),
            };
        }
    }
    let rendered = format_arg(ctx, val, spec)?;
    if spec == 'c' {
        // The range test is a DISAGREEMENT guard, not a second validation:
        // `format_arg` has already refused anything outside it, so this can
        // only fire if the two ever stopped unboxing the same argument the same
        // way — and then the right answer is `format_arg`'s rendering, not a
        // unit fabricated from a value it never saw.
        if let Some(cp) = fmt_char_code_point(ctx, val).filter(|cp| (0..=0x10FFFF).contains(cp)) {
            // `Character.toChars`: one unit for a BMP code point (INCLUDING a
            // lone surrogate), a high+low pair for a supplementary one.
            let cp = cp as u32;
            return Ok(if cp > 0xFFFF {
                let v = cp - 0x10000;
                vec![(0xD800 + (v >> 10)) as u16, (0xDC00 + (v & 0x3FF)) as u16]
            } else {
                vec![cp as u16]
            });
        }
    }
    Ok(rendered.encode_utf16().collect())
}

/// The code point a `%c` argument denotes, once [`format_arg`] has accepted it.
///
/// `None` means "not one of the shapes `%c` takes", which after `format_arg`
/// has returned `Ok` cannot happen — the caller falls back to `format_arg`'s
/// own rendering rather than inventing a character. Deliberately does NOT
/// re-check `isValidCodePoint`: that refusal has exactly one home, in
/// [`format_arg`]'s `'c'` arm, and a second copy here would be a second
/// chance to disagree with it.
///
/// The wrapper set is `printCharacter`'s: `Character`, `Byte`, `Short`,
/// `Integer`. `Byte` and `Short` are NOT masked the way the unsigned integer
/// conversions mask them — `printCharacter` widens them as signed and lets
/// `isValidCodePoint` reject a negative, which is why this reads the slot
/// straight through.
pub fn fmt_char_code_point(ctx: &mut dyn NativeContext, val: &Value) -> Option<i32> {
    match val {
        Value::Int(v) => Some(*v),
        Value::Object(Some(obj)) => {
            let cname = ctx.class_name_of_id(ctx.class_id_of_object(*obj))?;
            if !matches!(
                cname.as_str(),
                "java/lang/Character" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Integer"
            ) {
                return None;
            }
            match ctx.get_field(*obj, 0) {
                Value::Int(v) => Some(v),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Format a single argument for String.format.
///
/// Private: `format_arg_full` is the only entry point `String.format` reaches
/// (W8-F13-1 N3). No caller outside this file exists as of that record, so a
/// stray external use is now a build break instead of a silently stale
/// `pub`.
pub fn format_arg(
    ctx: &mut dyn NativeContext,
    val: &Value,
    spec: char,
) -> Result<String, MethodCallFailed> {
    // Helper: unbox wrapper object to primitive. Arrays are NEVER wrappers —
    // num_slots is the array LENGTH and get_field(0) on packed primitive
    // arrays reads garbage (String.format("%s", int[]) printed a bogus
    // number instead of "[I@hash").
    fn unbox_obj(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> Value {
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
            return Value::Object(Some(obj));
        }
        // ONLY unbox genuine primitive-wrapper classes. Other objects have an
        // int/long at slot 0 too — notably java.math.BigInteger (slot 0 =
        // `signum`), so blindly reading slot 0 made String.format("%x"/"%d"/"%s",
        // bigInteger) format the signum (1) instead of the magnitude. Gate on the
        // wrapper class name so non-wrappers fall through to toString()/radix.
        let cname = ctx
            .class_name_of_id(ctx.class_id_of_object(obj))
            .unwrap_or_default();
        let is_wrapper = matches!(
            cname.as_str(),
            "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Character"
                | "java/lang/Boolean"
                | "java/lang/Float"
                | "java/lang/Double"
        );
        if is_wrapper {
            let nf = ctx.object_num_fields(obj);
            if nf >= 1 {
                let f = ctx.get_field(obj, 0);
                match f {
                    Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                        return f
                    }
                    _ => {}
                }
            }
        }
        Value::Object(Some(obj))
    }

    let formatted = match val {
        // W8-F4-1 N4. Dead on the `String.format` path — `format_arg_full`'s
        // shared null printer returns before this is reached — and ROUTED
        // rather than deleted: the word is now [`fmt_null_text`]'s, the same
        // table that printer reads, so a future caller that makes `format_arg`
        // an entry point again gets the JDK's answer for WHICH word instead of
        // a second copy of the rule that can drift. What this arm still does
        // not do is the decoration (precision, upper-casing, justification);
        // `format_arg` does not decorate anything, for a null or otherwise.
        Value::Object(None) => fmt_null_text(spec).to_string(),
        Value::Object(Some(obj)) => {
            // `printBoolean`, which reads only the CLASS: "If the argument arg
            // is null, then the result is 'false'. If arg is a boolean or
            // Boolean, then the result is the string returned by
            // String.valueOf(arg). Otherwise, the result is 'true'."
            //
            // `unbox_obj` collapses every INTEGRAL wrapper to `Value::Int`, so
            // testing the unboxed shape alone made a zero-valued Integer,
            // Short, Byte or Character answer "false" — `String.format("%b",
            // 0)` was "false" where HotSpot 25 says "true", because an Integer
            // of zero is a non-null object and nothing else. The class is the
            // question, so ask it before unboxing.
            if spec == 'b' {
                let is_boolean = ctx
                    .class_name_of_id(ctx.class_id_of_object(*obj))
                    .is_some_and(|n| n == "java/lang/Boolean");
                if !is_boolean {
                    return Ok("true".to_string());
                }
                return Ok(match unbox_obj(ctx, *obj) {
                    Value::Int(v) => if v != 0 { "true" } else { "false" }.to_string(),
                    _ => "true".to_string(),
                });
            }
            // For %s: real OpenJDK does `String.valueOf(arg)` == `arg.toString()`.
            // A String formats as its characters; a boxed primitive wrapper
            // formats as its value; any other object (enum, record, bean, …)
            // formats via its `toString()`. The previous code only handled
            // String + wrappers and returned "null" for everything else, so e.g.
            // `String.format("%s", Color.GREEN)` printed "null".
            if spec == 's' {
                // String.valueOf(arg) == arg.toString(). Fast-path a *real* String
                // (read its chars directly); every other object — boxed wrappers
                // (incl. Boolean), enums, records, beans — goes through toString().
                // The old code called read_string FIRST on any object, which
                // mis-read a Boolean's value slot and yielded "1"/"0" instead of
                // "true"/"false" (and likewise for other non-String objects whose
                // slot-0 happens to read as text).
                // t1 wave 8 lane RL-REFL: not a `String[]` (an array's class
                // id is its component's), which formats through `toString()`.
                let is_string = !ctx.object_is_array(*obj)
                    && ctx.class_id_by_name("java/lang/String") == Some(ctx.class_id_of_object(*obj));
                if is_string {
                    if let Some(s) = ctx.read_string(*obj) {
                        return Ok(s);
                    }
                }
                return match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => {
                        Ok(ctx.read_string(s).unwrap_or_else(|| "null".to_string()))
                    }
                    Ok(_) => Ok("null".to_string()),
                    Err(err) => Err(err),
                };
            }
            // `printHashCode`, whose whole body is
            //   String s = (arg == null ? "null"
            //                           : Integer.toHexString(arg.hashCode()));
            // so `%h` is the hex of the argument's VIRTUAL hashCode and has
            // nothing to do with the argument's text.
            //
            // This arm used to answer `read_string(arg)`, which made `%h` an
            // alias for `%s` on a String — `String.format("%h", "a")` was "a"
            // where HotSpot 25 says "61" — and "null" on every argument that
            // is not a String, since `read_string` has nothing to read off an
            // Integer, an enum or a bean. Both halves were wrong and the
            // second one is the quieter: a `%h` of a bean printed "null" with
            // no refusal anywhere.
            //
            // `Integer.toHexString` is UNSIGNED, so the `as u32` is the whole
            // of the negative-hash case: `%h` of an object hashing to -1 is
            // "ffffffff", not "-1". `%H` upper-cases the result, and that is
            // done once for the whole general family in `format_arg_full`
            // rather than here.
            if spec == 'h' || spec == 'H' {
                let hash = match ctx.invoke_virtual(*obj, "hashCode", "()I", &[])? {
                    Some(Value::Int(h)) => h,
                    // The descriptor is `()I`, so bytecode cannot reach this;
                    // answering 0 keeps a broken interpreter a wrong answer
                    // rather than a panic.
                    _ => 0,
                };
                return Ok(format!("{:x}", hash as u32));
            }

            // Everything past here is a TYPED conversion, and
            // `java.util.Formatter.print(Object, Locale)` reaches its
            // `failConversion` default for an argument whose class the
            // conversion does not name: "If the argument arg is ... not
            // otherwise applicable to this conversion, then an
            // IllegalFormatConversionException will be thrown."
            //
            // Answering something anyway is how `String.format("%d",
            // "notANumber")` became a no-throw and `%.2f` of a BigDecimal became
            // `0.00` — `unbox_obj` fell through to a slot-0 read that meant
            // nothing on either class. The refusal has to carry the argument's
            // Class, because that is half of the message a caller reads.
            {
                let class_id = ctx.class_id_of_object(*obj);
                let cname = ctx.class_name_of_id(class_id).unwrap_or_default();
                let applicable = match spec {
                    'd' | 'o' | 'x' | 'X' => matches!(
                        cname.as_str(),
                        "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Integer"
                            | "java/lang/Long"
                            | "java/math/BigInteger"
                    ),
                    // %a has no BigDecimal printer in the JDK at all, which is
                    // why it is the one float conversion that refuses one.
                    'f' | 'e' | 'E' | 'g' | 'G' => matches!(
                        cname.as_str(),
                        "java/lang/Float" | "java/lang/Double" | "java/math/BigDecimal"
                    ),
                    'a' | 'A' => matches!(cname.as_str(), "java/lang/Float" | "java/lang/Double"),
                    'c' => matches!(
                        cname.as_str(),
                        "java/lang/Character"
                            | "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Integer"
                    ),
                    _ => true,
                };
                if !applicable {
                    // DIAGNOSTIC (`CRATONVM_DBG_FMT_WRONGTYPE`): this is the
                    // exact site that produces
                    // `IllegalFormatConversionException: d != java.lang.Object`.
                    // The message names only the class, which cannot separate a
                    // reclaimed cell (all-zero header), a stale reference to a
                    // moved object (forwarded header), and an argument that
                    // really is of the wrong class. Dump the header so the run
                    // says which.
                    if None::<std::ffi::OsString>.is_some()
                    {
                        let a = obj.as_ptr() as usize;
                        // SAFETY: diagnostic-only aligned read of the header
                        // words of an argument object the formatter was just
                        // handed, so the address is mapped.
                        let w = unsafe { std::slice::from_raw_parts(a as *const u64, 4) };
                        eprintln!(
                            "[fmt-wrongtype] spec={spec} obj=0x{a:x} cid={} cname={cname:?} \
                             kind={:?} w0=0x{:x} w1=0x{:x} w2=0x{:x} w3=0x{:x}",
                            class_id.as_u32(),
                            ctx.heap_kind_of(*obj),
                            w[0],
                            w[1],
                            w[2],
                            w[3],
                        );
                    }
                    // The conversion character the exception carries is the
                    // LOWER-case one. `FormatSpecifier.conversion(char)` folds
                    // every upper-case conversion down —
                    // `if (Character.isUpperCase(conv)) { f.add(Flags.UPPERCASE);
                    // this.c = Character.toLowerCase(conv); }` — and `c` is
                    // what `failConversion` hands
                    // `IllegalFormatConversionException`. `format_arg_full`
                    // remaps only `S`/`B`/`C`/`H` (the four whose RENDERING
                    // changes), so `X`/`E`/`G`/`A` arrive here as written and
                    // this is the one place the fold has to be repeated.
                    // Measured on HotSpot 25.0.3+9, `Boolean.TRUE` as the
                    // argument, message and `getConversion()` both:
                    //
                    //   %X -> "x != java.lang.Boolean", conv='x'
                    //   %E -> "e != java.lang.Boolean", conv='e'
                    //   %G -> "g != java.lang.Boolean", conv='g'
                    //   %A -> "a != java.lang.Boolean", conv='a'
                    //
                    // `%tY` is NOT folded — `printDateTime` reports the FIELD
                    // character as typed ("Y != java.lang.String", measured) —
                    // which is why this is done here and not in `fmt_raise`.
                    return Err(fmt_raise(
                        ctx,
                        &FmtFault::WrongType(spec.to_ascii_lowercase(), class_id),
                    ));
                }
                // A BigDecimal at its DEFAULT precision still has to come from
                // its own digits — `%f` of `new BigDecimal("2.3")` is
                // "2.300000", six fraction digits of the decimal literal, not of
                // a double it was never turned into.
                if cname == "java/math/BigDecimal" {
                    if let Some(FloatSource::Decimal(plain)) = float_source(ctx, val) {
                        return Ok(java_decimal_conversion(&plain, spec, None));
                    }
                }
            }

            // BigInteger numeric conversions: its slot-0 field is `signum`, not the
            // value, so it must NOT be unboxed. Java's Formatter formats a
            // BigInteger via its real radix toString (e.g. %x => toString(16));
            // BigInteger.toString(radix) works correctly on CratonVM.
            // (new BigInteger(1, sha256).%064x -> Spring Boot buildpack LayerId.)
            // gen r4w3/rooting: the argument's address as of the last upcall.
            // Only the BigInteger `toString(I)` below can move it before the
            // fallback reads under this point, and it refreshes this.
            let mut obj_now = *obj;
            if matches!(spec, 'd' | 'x' | 'X' | 'o') {
                let cname = ctx
                    .class_name_of_id(ctx.class_id_of_object(obj_now))
                    .unwrap_or_default();
                if cname == "java/math/BigInteger" {
                    let radix = match spec {
                        'o' => 8,
                        'd' => 10,
                        _ => 16,
                    };
                    // gen r4w3/rooting: pinned across the upcall so the
                    // `Ok(_)` fall-through below reads the moved object, not
                    // its vacated address.
                    let obj_pin = ctx.pin_native_root(obj_now);
                    let r = ctx.invoke_virtual(
                        obj_now,
                        "toString",
                        "(I)Ljava/lang/String;",
                        &[Value::Int(radix)],
                    );
                    obj_now = ctx.read_native_pin(obj_pin, obj_now);
                    ctx.unpin_native_roots(obj_pin);
                    match r {
                        Ok(Some(Value::Object(Some(s)))) => {
                            let str = ctx.read_string(s).unwrap_or_default();
                            return Ok(if spec == 'X' { str.to_uppercase() } else { str });
                        }
                        Ok(_) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
            // Try to unbox wrapper to primitive and recurse
            let inner = unbox_obj(ctx, obj_now);
            match inner {
                Value::Object(_) => {
                    // Not a wrapper — fallback to string
                    ctx.read_string(obj_now)
                        .unwrap_or_else(|| "null".to_string())
                }
                Value::Int(v) if matches!(spec, 'x' | 'X' | 'o') => {
                    // Java's Formatter masks a Byte argument to 8 bits and a
                    // Short to 16 bits for the unsigned conversions (%x/%X/%o);
                    // an Integer keeps all 32. After unbox_obj all three collapse
                    // to Value::Int, so a negative Byte would sign-extend
                    // (String.format("%02x", (byte)-54) → "ffffffca" instead of
                    // "ca"). Recover the width from the wrapper class.
                    let cname = ctx
                        .class_name_of_id(ctx.class_id_of_object(obj_now))
                        .unwrap_or_default();
                    let masked = match cname.as_str() {
                        "java/lang/Byte" => v & 0xFF,
                        "java/lang/Short" => v & 0xFFFF,
                        _ => v,
                    };
                    return format_arg(ctx, &Value::Int(masked), spec);
                }
                _ => return format_arg(ctx, &inner, spec),
            }
        }
        // The float family is routed through `java_float_conversion` with NO
        // precision, so its Formatter defaults (6 for %f/%e/%g, every digit for
        // %a) apply here. This is the path a bare `%e` takes, and it used to
        // answer Rust's `{:e}` — `1.2345e3` where HotSpot writes
        // `1.234500e+03`. A Float argument widens to double first, as
        // `Formatter.print(float, Locale)` does.
        Value::Int(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u32),
            'X' => format!("{:X}", *v as u32),
            'o' => format!("{:o}", *v as u32),
            // `printCharacter` gates every non-`Character` argument on
            // `Character.isValidCodePoint` and refuses the rest with
            // `IllegalFormatCodePointException` — `String.format("%c",
            // 0x110000)` is a THROW, not the '?' this answered. A `Character`
            // argument is never checked by the JDK and never needs to be: it
            // unboxes into 0..=0xFFFF, which is always valid.
            'c' => {
                if !(0..=0x10FFFF).contains(v) {
                    return Err(fmt_raise(ctx, &FmtFault::IllegalCodePoint(*v)));
                }
                // A lone surrogate (0xD800..=0xDFFF) IS a valid code point by
                // `Character.isValidCodePoint`, and `Character.toChars` hands
                // it back as a single unpaired `char`. Rust's `char` cannot
                // hold one and `create_string` takes a `&str`, so the '?' is
                // kept for exactly that range — a residual of the UTF-8 string
                // representation, not of this check. See
                // W7-41-format-exception-subclasses.md.
                char::from_u32(*v as u32).unwrap_or('?').to_string()
            }
            'b' => ((*v) != 0).to_string(),
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => java_float_conversion(*v as f64, spec, None),
            _ => v.to_string(),
        },
        Value::Long(v) => match spec {
            'd' => v.to_string(),
            'x' => format!("{:x}", *v as u64),
            'X' => format!("{:X}", *v as u64),
            'o' => format!("{:o}", *v as u64),
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => java_float_conversion(*v as f64, spec, None),
            _ => v.to_string(),
        },
        Value::Float(v) => match spec {
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => java_float_conversion(*v as f64, spec, None),
            // `%s`/no-spec of a float -> Java Double.toString form, not raw `{}`.
            _ => format_float(*v),
        },
        Value::Double(v) => match spec {
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => java_float_conversion(*v, spec, None),
            // `%s`/no-spec of a double -> Java Double.toString form, not raw `{}`.
            _ => format_double(*v),
        },
        _ => "?".to_string(),
    };
    Ok(formatted)
}

// ---------------------------------------------------------------------------
// Java's character tables, for the `String` methods that are specified in
// terms of them.
//
// W7-95a. Every one of the three predicates below exists because the Rust
// standard-library method with the plausibly-matching name implements the
// UNICODE definition and Java's is deliberately different. Reaching for
// `char::is_whitespace` / `char::to_uppercase` here is the single root cause
// behind `String.isBlank` and `String.regionMatches(true, ...)` disagreeing
// with HotSpot, and it is the same reflex W7-95 measured across the whole
// `java/lang/Character` family.
// ---------------------------------------------------------------------------

/// `Character.isWhitespace` for one UTF-16 code unit — the JAVADOC rule, not
/// `char::is_whitespace`.
///
/// * Rust's `char::is_whitespace` is the Unicode **White_Space** property:
///   `Zs ∪ Zl ∪ Zp ∪ {U+0009..U+000D, U+0085}`.
/// * Java's `isWhitespace` is "a Unicode space character (`Zs`/`Zl`/`Zp`) that
///   is **not** a non-breaking space (`U+00A0`, `U+2007`, `U+202F`), **or** one
///   of `U+0009..U+000D`, `U+001C..U+001F`".
///
/// Eight code points disagree. Measured on Microsoft OpenJDK 25.0.3+9 —
/// written by NAME here, never as the character itself, because a doc comment
/// holding a literal U+00A0 is one re-encode away from holding a space:
/// `isBlank()` of a lone `U+00A0` NBSP, `U+0085` NEL, `U+2007` FIGURE SPACE
/// or `U+202F` NARROW NBSP is `false` there and was `true` here; of
/// `U+001C`..`U+001F` (file/group/record/unit separator) it is `true` there
/// and was `false` here.
///
/// A surrogate code unit is `Cs`, never whitespace, and no arm below admits
/// one — which is what HotSpot answers.
///
/// **This is an ENUMERATION, deliberately, and not the derivation it replaced.**
/// The first version computed `Zs | Zl | Zp` as
/// `char::is_whitespace(cp) && !matches!(cp, 0x09..=0x0D | 0x85)` and subtracted
/// the three non-breaking spaces. That was correct — verified over all
/// 1,114,112 code points against `Character.isWhitespace` on OpenJDK 25.0.3+9,
/// zero mismatches, and agreeing with the enumeration below at every one of
/// them — but it was correct *by consulting a Rust Unicode table at runtime*,
/// and that is the dependency this file has already been bitten by once: see
/// `case_map::JDK_UNMAPPED_CASE_RUNS`, where Rust's tables being NEWER
/// than the JDK's produced a wrong answer that no amount of care about the Java
/// side could have caught. Whitespace is a small, stable, enumerable set, so the
/// version-skew hazard is removed rather than managed.
///
/// The enumeration itself was swept exhaustively: `WsSweep`, all 1,114,112
/// code points, `proposed mismatches=0`.
///
/// **Twin.** `native_character_is_whitespace` in
/// `native-builtins/src/lang_math.rs` is the same rule for the
/// `java/lang/Character` triples (W7-98a). That lane independently replaced its
/// derivation with this same enumeration; the two now agree character for
/// character AND line for line, which is the point — two enumerations of the
/// same constants cannot silently drift the way two derivations over a moving
/// table can. They should still be hoisted onto one predicate; see N5.
pub fn java_char_is_whitespace(ch: u16) -> bool {
    matches!(u32::from(ch),
        0x0009..=0x000D      // TAB, LF, VT, FF, CR
        | 0x001C..=0x0020    // FILE/GROUP/RECORD/UNIT SEPARATOR, SPACE
        | 0x1680             // OGHAM SPACE MARK
        | 0x2000..=0x2006    // EN QUAD .. SIX-PER-EM SPACE  (2007 FIGURE SPACE excluded)
        | 0x2008..=0x200A    // PUNCTUATION SPACE .. HAIR SPACE
        | 0x2028..=0x2029    // LINE SEPARATOR, PARAGRAPH SEPARATOR
        | 0x205F             // MEDIUM MATHEMATICAL SPACE
        | 0x3000             // IDEOGRAPHIC SPACE
    )
    // Absent on purpose, each measured `false` on HotSpot: 0x00A0 NBSP,
    // 0x2007 FIGURE SPACE, 0x202F NARROW NBSP (non-breaking spaces are
    // excluded by the javadoc), and 0x0085 NEL.
}


/// `Character.toUpperCase(char)` — Java's **1:1** mapping, not the full one.
///
/// `char::to_uppercase()` yields the FULL (SpecialCasing) mapping, which can
/// be several characters: `ß` → `"SS"`, `ﬀ` → `"FF"`. Java's `char`-taking
/// overload is UnicodeData's *simple* mapping and returns the input unchanged
/// whenever the full mapping does not fit in one `char`. Taking `.next()` off
/// the iterator — the shape W7-95 measured in `native_character_to_upper_case`
/// — turns `ß` into `S`.
///
/// The exception arms are the complete set for the BMP, derived rather than
/// sampled: `Character.toUpperCase((char) c)` was dumped for all 65 536 code
/// units on OpenJDK 25.0.3+9 and diffed against "full mapping, multi-char
/// falls back to identity". Exactly 27 code units differ, all of them Greek
/// vowels with ypogegrammeni whose simple uppercase is the TITLECASE
/// character (`U+1F80` → `U+1F88`) while the full mapping is a two-character
/// expansion. Everywhere else the two rules agree, which is why this is a
/// small `match` and not a ported table.
///
/// A surrogate code unit has no scalar value; Java maps every unmapped char to
/// *itself*, so this returns the input rather than `char::from_u32`'s `None`.
pub fn java_char_to_upper_case(ch: u16) -> u16 {
    // Rust's tables are newer than the JDK's and pair these; the JDK maps each
    // to itself. See `JDK_UNMAPPED_CASE_RUNS`.
    if is_jdk_unmapped_case_code_point(u32::from(ch)) {
        return ch;
    }
    match ch {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => return ch + 8,
        0x1FB3 => return 0x1FBC,
        0x1FC3 => return 0x1FCC,
        0x1FF3 => return 0x1FFC,
        _ => {}
    }
    let Some(c) = char::from_u32(u32::from(ch)) else {
        return ch;
    };
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) if u32::from(u) <= 0xFFFF => u as u16,
        _ => ch,
    }
}

/// `Character.toLowerCase(char)` — Java's **1:1** mapping. See
/// [`java_char_to_upper_case`] for the derivation; the same BMP-wide dump
/// finds exactly ONE code unit where the full lowercase mapping differs from
/// Java's simple one: `U+0130` LATIN CAPITAL LETTER I WITH DOT ABOVE, whose
/// SpecialCasing full mapping is the two characters `i` + `U+0307` and whose
/// simple mapping is plain `i`. That one code point is the difference between
/// `"İ".regionMatches(true, 0, "i", 0, 1)` answering `true` (HotSpot) and
/// `false`.
pub fn java_char_to_lower_case(ch: u16) -> u16 {
    // See the twin above and `JDK_UNMAPPED_CASE_RUNS`.
    if is_jdk_unmapped_case_code_point(u32::from(ch)) {
        return ch;
    }
    if ch == 0x0130 {
        return 0x0069;
    }
    let Some(c) = char::from_u32(u32::from(ch)) else {
        return ch;
    };
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) if u32::from(l) <= 0xFFFF => l as u16,
        _ => ch,
    }
}

// ---------------------------------------------------------------------------
// String modern methods (Java 11+)
// ---------------------------------------------------------------------------

/// `String.repeat(int)` — "ab".repeat(3) → "ababab".
///
/// W7-95a. The old body was `std::cmp::max(0, n) as usize`, which turned every
/// contract violation into a silent empty string, and then handed the count
/// straight to `str::repeat`, which has no upper bound at all.
///
/// Measured on OpenJDK 25.0.3+9, in the JDK's own check order:
///
/// ```text
/// "ab".repeat(-1)                  IllegalArgumentException: count is negative: -1
/// "ab".repeat(Integer.MIN_VALUE)   IllegalArgumentException: count is negative: -2147483648
/// "".repeat(-1)                    IllegalArgumentException          (count is checked FIRST,
///                                                                    before the empty-string
///                                                                    short circuit)
/// "ab".repeat(0)                   ""
/// "ab".repeat(Integer.MAX_VALUE)   OutOfMemoryError: Required length exceeds implementation limit
/// "abc".repeat(1000000000)         OutOfMemoryError: Required length exceeds implementation limit
/// ```
///
/// The overflow guard is the JDK's `Integer.MAX_VALUE / count < len`, and
/// `len` there is the length of the compact `value` **byte** array — one byte
/// per char for a LATIN-1 string, two for a UTF-16 one — which is why the
/// coder is reconstructed below instead of using the char count. Without it
/// `str::repeat` attempts a multi-gigabyte allocation and aborts the process:
/// a capacity overflow is a Rust panic, and a Rust panic is not a Java
/// throwable. `try_reserve_exact` covers the residual case where the
/// Java-legal length is still more memory than this process can get.
pub fn native_string_repeat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    if count < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("count is negative: {count}"),
            }
            .into(),
        );
    }
    // JDK 25's order: `count == 1` returns `this` BEFORE the empty test, so
    // `"".repeat(1)` is the receiver; then `len == 0 || count == 0` returns
    // the `""` LITERAL (round 17 wave 4, lane sbempty,
    // `CRATONVM_STRING_LITERAL_IDENTITY`; `=0` restores the old order and the
    // fresh empty String).
    let literal_identity = string_literal_identity_enabled(&*ctx);
    if literal_identity && count == 1 {
        return Ok(Some(Value::Object(Some(this))));
    }
    let units = read_string_chars(ctx, this);
    if units.is_empty() || count == 0 {
        let empty = if literal_identity {
            ctx.intern_string("")
        } else {
            ctx.create_string_uninterned("")
        };
        return Ok(Some(Value::Object(Some(empty))));
    }
    if count == 1 {
        // `return this` — `String.repeat(1)` is specified to hand back the
        // receiver, and a String is immutable, so sharing it is correct.
        return Ok(Some(Value::Object(Some(this))));
    }
    // The JDK's guard, on the same quantity the JDK measures: `value.length`,
    // which is 2 bytes per char once any char needs the UTF-16 coder.
    let value_len = if units.iter().all(|&u| u <= 0xFF) {
        units.len() as i64
    } else {
        units.len() as i64 * 2
    };
    if i64::from(i32::MAX) / i64::from(count) < value_len {
        return Err(oome_repeat_limit().into());
    }
    // Repeat the code UNITS, not a Rust `String`. The old body ended in
    // `String::from_utf16_lossy` + `create_string_uninterned(&str)`, so every
    // unpaired surrogate in the receiver came back as U+FFFD — silently, since
    // the unit COUNT is the same either way (U+FFFD is one unit, as is a lone
    // surrogate) and so `length()` agreed with HotSpot while the content did
    // not. Measured on HotSpot 25: `"a\uD800b".repeat(2)` is
    // `0061 D800 0062 0061 D800 0062` and `"\uDC00".repeat(3)` is
    // `DC00 DC00 DC00`.
    //
    // The lossless writer this needs was already in this file
    // ([`sb_string_from_units`], three callers away) and was already reachable
    // through the existing `NativeContext` — no new VM primitive was required.
    let Some(total) = units.len().checked_mul(count as usize) else {
        return Err(oome_repeat_limit().into());
    };
    let mut result: Vec<u16> = Vec::new();
    // Same fallible reservation as before, on the same quantity the JDK's own
    // guard above measures — the `value` array — rather than on a UTF-8 length
    // that no longer exists here.
    if result.try_reserve_exact(total).is_err() {
        return Err(oome_repeat_limit().into());
    }
    for _ in 0..count {
        result.extend_from_slice(&units);
    }
    // The receiver is no longer needed once its units have been copied.
    // Pin it while the large result is allocated so a GC-safe retry never
    // carries a bare ObjectRef across a moving collection.
    let this_pin = ctx.pin_native_root(this);
    let str_obj = ctx.create_string_from_units_gc_safe(&result);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(str_obj))))
}

/// HotSpot's exact `String.repeat` overflow wording, in one place so the three
/// bail-outs above cannot drift apart.
pub fn oome_repeat_limit() -> cratonvm_types::error::RuntimeError {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: "Required length exceeds implementation limit".to_string(),
    }
}

/// `String.isBlank()` — empty, or every code point is `Character.isWhitespace`.
///
/// W7-95a. The old body was `s.trim().is_empty()`, and `str::trim` is Unicode
/// **White_Space**, which is not Java's table — see [`java_char_is_whitespace`]
/// for the eight code points that disagree and the measurement. A lone
/// `U+00A0` answered `true` here and `false` on HotSpot; `U+001C`..`U+001F`
/// answered `false` here and `true` on HotSpot.
///
/// Tested per code UNIT rather than per code point, which is equivalent: no
/// supplementary code point is whitespace and neither is a surrogate, so a
/// pair and each of its halves all answer `false`. Reading the units directly
/// also keeps the `str` round trip (and its U+FFFD substitution) out of a
/// method whose answer depends on the exact code points present.
pub fn native_string_is_blank(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let blank = with_string_chars_scratch(ctx, this, |units| {
        units.iter().all(|&u| java_char_is_whitespace(u))
    });
    Ok(Some(Value::Int(if blank { 1 } else { 0 })))
}

/// chars() — returns IntStream of char values
///
/// FMT-REGRESSION-ANONOBJ: previously allocated the result via the raw
/// `ctx.alloc_object(ClassId::new(0), 1)` fallback instead of stamping it
/// with the `java/util/stream/IntStream` interface name like every other
/// synthetic IntStream factory (`make_int_stream`, `native_int_stream_range`,
/// etc. in native-collections) does. `alloc_object`'s ClassId(0)
/// defense-in-depth (vm_exec.rs, added to fix a WildFly GC-corruption bug)
/// now redirects any ClassId(0)+num_fields>0 allocation to a shared
/// `cratonvm/synthetic/AnonymousObject$N` class instead of leaving it with a
/// broken/undersized layout -- correct for that fix's own purpose, but it
/// meant this stream's class name was never `java/util/stream/IntStream`, so
/// `forEach`/`allMatch`/etc. (registered in native-collections keyed on that
/// exact class name) resolved to `NoSuchMethodError` on
/// `AnonymousObject$1.forEach(IntConsumer)` /
/// `AnonymousObject$1.allMatch(IntPredicate)` for any caller of
/// `String.chars()`/`codePoints()` (e.g. Spring's
/// `BasicJsonWriter.write` -> `input.chars().forEach(...)`, used by both
/// `BasicJsonWriterTests` and `RuntimeHintsWriterTests` via
/// `RuntimeHintsWriter.write`).
///
/// Fix: allocate via `alloc_concurrent_synthetic` stamped with the
/// `java/util/stream/IntStream` interface name and the standard 2-field
/// synthetic-stream layout (field 0 = elements, field 1 = close handlers --
/// see `STREAM_FIELD_ELEMENTS`/`STREAM_FIELD_CLOSE_HANDLERS` in
/// native-collections/src/lib.rs), matching every other IntStream factory.
pub fn native_string_chars(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `read_string_chars`, not `ctx.read_string(..).encode_utf16()`: `chars()`
    // is the code-UNIT view and must reproduce an unpaired surrogate exactly.
    // Through a Rust `String` one becomes U+FFFD -- `"x\uD800y".chars()` gave
    // `[120, 65533, 121]` against HotSpot 25.0.3+9's `[120, 55296, 121]`
    // (W7-95a). Same root cause as `codePointAt`; same one-line fix.
    let char_values: Vec<i32> = read_string_chars(ctx, this)
        .into_iter()
        .map(i32::from)
        .collect();
    // `CharsSpliterator`: ORDERED | SIZED | SUBSIZED | IMMUTABLE.
    int_stream_of(ctx, &char_values, true)
}

/// `String.codePoints()` — an `IntStream` of **code points**, which is not
/// `chars()`.
///
/// W7-95a. `codePoints` was registered onto `native_string_chars` with the
/// comment "Same as chars for BMP" at two registration sites. It is not the
/// same for any string that contains a surrogate pair: measured on OpenJDK
/// 25.0.3+9, `"a<U+1F600>b".codePoints()` is `[97, 128512, 98]` and this VM
/// answered `[97, 55357, 56832, 98]` — four elements where Java has three, so
/// every `codePoints().count()` over emoji was wrong, not just the values.
///
/// A surrogate that is *not* part of a pair is its own code point and passes
/// through unchanged (`"x\uD800y"` → `[120, 55296, 121]`), which is exactly
/// why this cannot be written over a Rust `str`.
pub fn native_string_code_points(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let units = read_string_chars(ctx, this);
    let mut code_points: Vec<i32> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let ch = units[i];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < units.len() {
            let low = units[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                code_points
                    .push(0x10000 + ((i32::from(ch) - 0xD800) << 10) + (i32::from(low) - 0xDC00));
                i += 2;
                continue;
            }
        }
        code_points.push(i32::from(ch));
        i += 1;
    }
    // JDK 25 `codePoints()` streams a LATIN1 string through the same
    // `CharsSpliterator` as `chars()` (`0x4450`); a UTF16 one through
    // `CodePointsSpliterator` (ORDERED | IMMUTABLE, estimate = char count),
    // which an array spliterator cannot stand for, so that one records
    // nothing.
    let latin1 = units.iter().all(|&u| u <= 0xFF);
    int_stream_of(ctx, &code_points, latin1)
}

/// Build the synthetic `IntStream` both `chars()` and `codePoints()` return.
///
/// Extracted so the two cannot drift: the layout notes below were paid for
/// twice already (a 1-field allocation and a reference-array allocation), and
/// a second copy of them is a second place to get them wrong.
///
/// # JVMS 6.5 census: the class stamped here is an INTERFACE
///
/// `java/util/stream/IntStream` is `ACC_INTERFACE`, so no `new` could have
/// produced the object below, and
/// `cratonvm_native_api::instantiable::observe_uninstantiable_receiver` says so
/// once per boot with `requester=` this function -- on a stock `cratonvm Hello`
/// it is the first of the workspace's seven `IntStream` mints that runs.
/// AUDITED 2026-09-01: real violation, both repairs assessed, neither belongs
/// here, allocation and WARN both left alone on purpose.
///
/// **(a) mint the concrete class the JDK would produce -- rejected.** That
/// class is `java.util.stream.IntPipeline$Head`, and it is not a shape: it is
/// an `AbstractPipeline` stage over a `Spliterator.OfInt`, whose bytecode this
/// VM does not run. Writing slots 0 and 1 of one would alias
/// `AbstractPipeline`'s own fields, and -- decisively -- no native is
/// registered on that name, so every terminal operation would resolve to real
/// pipeline bytecode over a source that was never built.
///
/// **(b) mint a `cratonvm/internal/...` concrete stand-in -- rejected, because
/// the interface name is what makes these streams WORK.** Two exact-match
/// tables in `native-collections/src/lib.rs` are keyed on it: every
/// `IntStream` operation is registered under the literal class name
/// `"java/util/stream/IntStream"`, and `is_synthetic_stream` -- the predicate
/// `int_stream_elements` / `prim_stream_values` use to decide whether to read
/// the backing array in slot 0 -- matches exactly four names, all of them
/// interfaces. A stream stamped anything else answers `forEach` with
/// `NoSuchMethodError` and `average()` with an empty stream, which is precisely
/// the `FMT-REGRESSION-ANONOBJ` failure recorded above `native_string_chars`,
/// replayed by a different route. Repairing it means moving both tables plus an
/// arm in `vm/src/runtime/interpreter/typecheck.rs`'s `synthetic_implements`
/// (so `instanceof IntStream`, which is correct today only because the class IS
/// the interface, stays correct) -- and all three files are outside this
/// audit's write scope.
///
/// **(c) and it cannot be closed from this file regardless.** The census
/// dedupes by class name; the same interface is minted at six further sites
/// (`native-collections/src/lib.rs` `make_int_stream` and `mapToInt`,
/// `native-builtins/src/phases_late/streams.rs`,
/// `native-builtins/src/phases_early.rs`), so a local repair would only
/// re-point `requester=` at one of those.
///
/// **Round 16 wave 14 (lane compat2, CC13-1).** With `array_source`, and
/// unless `CRATONVM_STREAM_SOURCE_FACTS=0`, the stream is allocated at the
/// full synthetic layout (`SYNTHETIC_STREAM_NUM_FIELDS`) so it can record its
/// source spliterator's characteristics, `0x4450`
/// (`record_array_source_spliterator_facts`): `"ab".chars().spliterator()`
/// reported `0x4040` where HotSpot reports `0x4450`. The extra slots read back
/// as zero ("not lazy, no chain, never linked"), as the CORRECTION below
/// records; the 2-field shape is kept under the kill switch.
pub fn int_stream_of(
    ctx: &mut dyn NativeContext,
    char_values: &[i32],
    array_source: bool,
) -> MethodCallResult {
    // 2-field synthetic stream layout: field 0 = elements array, field 1 =
    // close handlers (None -- chars()/codePoints() never register any).
    // Must be 2 fields (not 1) to match STREAM_NUM_FIELDS in
    // native-collections/src/lib.rs -- the shared stream layout every other
    // IntStream factory uses, which reserves field 1 for BaseStream.onClose
    // handlers; a 1-field object would make any downstream onClose()
    // registration on one of these streams an out-of-bounds field write.
    // (A concurrent fix independently found this same root cause -- e.g. it
    // also breaks `StringUtils.containsWhitespace` -> `"...".chars().anyMatch(...)`
    // -- via a 1-field allocation; reconciled to the 2-field layout here.)
    //
    // CORRECTION (audit, 2026-09-01): "match STREAM_NUM_FIELDS" is no longer
    // true of the number. W7-65 grew `STREAM_NUM_FIELDS` to 5 -- slot 2 is the
    // lazy spliterator, 3 the op chain, 4 `AbstractPipeline.linkedOrConsumed`
    // -- and `make_int_stream` allocates that width. The 2 below is left as it
    // is, because what the sentence above is actually about is still right and
    // is the only part that can break something: slot 1 must EXIST or an
    // `onClose` registration writes out of bounds. The three slots this stream
    // lacks are all guarded by `object_num_fields` on the reading side, so a
    // short stream reads as "not lazy, no chain, never linked" rather than
    // faulting -- and the last of those is the one visible consequence:
    // `"..".chars()` cannot report `IllegalStateException: stream has already
    // been operated upon or closed` on reuse. That divergence is already
    // accepted for every primitive stream (`stream_link_or_consume` is
    // reference-carrier-only by design), so widening this to 5 would buy
    // nothing here and is deliberately NOT done as part of an audit that
    // changes no behaviour.
    let record_facts = array_source && cratonvm_native_collections::stream_source_facts_enabled();
    let num_fields = if record_facts {
        cratonvm_native_collections::SYNTHETIC_STREAM_NUM_FIELDS
    } else {
        2
    };
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", num_fields)?;
    // gc-common w15-e: the stream is written AFTER the element array's
    // allocation, so it is rooted across it and re-read.
    let mut scope = NativeHandleScope::new(ctx);
    let stream_h = scope.root(stream);
    // Must be a primitive `int[]`, not a reference array: this stream's
    // consumers (`IntStream.forEach`/`toArray`/etc.) read field 0 as an
    // int-element array. A `new_ref_array` allocation stored `Value::Int`s
    // into Object-shaped slots, which silently read back as 0 -- every
    // `"...".chars()` consumer therefore saw a correctly-SIZED but all-zero
    // stream (confirmed via a standalone `chars().toArray()` repro).
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Int, char_values.len());
    for (i, val) in char_values.iter().enumerate() {
        scope.set_array_element(arr, i, Value::Int(*val));
    }
    let stream = scope.get(&stream_h);
    scope.set_field(stream, 0, Value::Object(Some(arr)));
    scope.set_field(stream, 1, Value::Object(None));
    if record_facts {
        cratonvm_native_collections::record_array_source_spliterator_facts(&mut *scope, stream);
    }
    Ok(Some(Value::Object(Some(stream))))
}

/// regionMatches(boolean ignoreCase, int toffset, String other, int ooffset, int len)
pub fn native_string_region_matches_ic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let ignore_case = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let toffset_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let other = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let ooffset_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    region_matches_impl(ctx, this, toffset_i, other, ooffset_i, len_i, ignore_case)
}

/// `String.regionMatches`, both overloads.
///
/// W7-95a. Two defects, one shared body now.
///
/// **A negative `len` answers `true`, and did not.** The JDK's bounds test is
///
/// ```text
/// if ((ooffset < 0) || (toffset < 0)
///         || (toffset > (long) length() - len)
///         || (ooffset > (long) other.length() - len)) return false;
/// ```
///
/// with its own comment "toffset, ooffset, or len might be near `-1>>>1`",
/// which is why it widens to `long` rather than adding. When `len` is
/// negative, `length() - len` is *larger* than the length, the test passes,
/// and the comparison loop `while (len-- > 0)` runs zero times — so the answer
/// is `true`. Measured on OpenJDK 25.0.3+9: `"ABC".regionMatches(0, "abc", 0,
/// -1)` and the same call with `Integer.MIN_VALUE` are both `true`; this VM
/// answered `false` from an `if len_i < 0 { return false }` guard that has no
/// counterpart in the JDK. Callers that pass a computed length depend on it.
///
/// **A null `other` is a NullPointerException — but only where the JDK's `||`
/// actually dereferences it.** That expression SHORT-CIRCUITS, and `other` is
/// touched at the FOURTH term, so the null contract is conditional on the first
/// three. Every row measured on OpenJDK 25.0.3+9 with `"abc"` as the receiver:
///
/// ```text
/// regionMatches(  0, null,  0,  1)   NullPointerException   (reaches other.length())
/// regionMatches(  0, null,  0, -1)   NullPointerException   (negative len does NOT save it)
/// regionMatches(  3, null,  0,  0)   NullPointerException   (toffset == length() - len)
/// "".regionMatches(0, null, 0,  0)   NullPointerException   (empty receiver, still reached)
/// regionMatches( -1, null,  0,  1)   false                  (toffset < 0, term two)
/// regionMatches(  0, null, -1,  1)   false                  (ooffset < 0, term one)
/// regionMatches( 99, null,  0,  1)   false                  (term three, other never read)
/// regionMatches(  0, null,  0,  4)   false                  (term three: 0 > 3 - 4)
/// ```
///
/// with the five-argument overload answering identically for
/// `ignoreCase = true` and delegating for `ignoreCase = false`. This VM
/// answered `false` to all eight: `other` was unwrapped at the top of the
/// native and a null took the same exit as a bounds failure, which is the
/// `[default=wrong write]` shape — a defaulting reader turning a contract
/// violation into a plausible answer. The two behaviours cannot be separated by
/// checking null first (three of the eight rows would then be wrong), so the
/// order below is the JDK's expression, term for term.
///
/// **Both sides are read losslessly.** `ctx.read_string` goes through a Rust
/// `String`, where every unpaired surrogate becomes U+FFFD — so two *different*
/// lone surrogates compared equal. `read_string_chars` reads the `value`
/// arrays as UTF-16 code units, which is also the unit `toffset`/`ooffset`/`len`
/// are specified in. `other` is read only after it has survived the first three
/// terms, which is both the JDK's order and the cheaper one.
/// Whether the JDK's `regionMatches` bounds expression already answers `false`
/// from its first THREE terms — the ones that do not touch `other`.
///
/// Split out so the short-circuit point is testable without a receiver, a
/// heap or a `NativeContext`: `true` here means "return false, and `other`
/// must NOT be dereferenced", `false` means "the JDK now evaluates
/// `other.length()`", which is where a null argument becomes a
/// NullPointerException. See [`region_matches_impl`] for the eight measured
/// rows this reproduces.
///
/// `this_len`, `toffset`, `ooffset` and `len` are `i64` for the same reason the
/// JDK widens to `long`: its own comment says "toffset, ooffset, or len might
/// be near `-1>>>1`", so `length() - len` must not wrap.
pub fn region_matches_short_circuits(this_len: i64, toffset: i64, ooffset: i64, len: i64) -> bool {
    ooffset < 0 || toffset < 0 || toffset > this_len - len
}

#[allow(clippy::too_many_arguments)]
pub fn region_matches_impl(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    toffset_i: i32,
    other: Option<cratonvm_types::ObjectRef>,
    ooffset_i: i32,
    len_i: i32,
    ignore_case: bool,
) -> MethodCallResult {
    // regionMatches offsets/len are in UTF-16 code UNITS, not Unicode code
    // points. Index over u16 to agree with charAt/length.
    let s_units = read_string_chars(ctx, this);

    // The JDK's test, in `i64` for the same reason the JDK uses `long`, and in
    // the JDK's order so that the `other.length()` dereference happens exactly
    // where the JDK's does.
    let toffset = i64::from(toffset_i);
    let ooffset = i64::from(ooffset_i);
    let len = i64::from(len_i);
    if region_matches_short_circuits(s_units.len() as i64, toffset, ooffset, len) {
        return Ok(Some(Value::Int(0)));
    }
    // Term four. `other.length()` — the JDK's implicit null check.
    let Some(other) = other else {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"String.length()\" because \"other\" is null".to_string(),
            ),
        }
        .into());
    };
    let o_units = read_string_chars(ctx, other);
    if ooffset > o_units.len() as i64 - len {
        return Ok(Some(Value::Int(0)));
    }
    if len <= 0 {
        // `while (len-- > 0)` compared nothing. Not an error, not `false`.
        return Ok(Some(Value::Int(1)));
    }

    let toffset = toffset as usize;
    let ooffset = ooffset as usize;
    if ignore_case {
        // The JDK's coder dispatch: UTF16/UTF16 pairs surrogates by code point.
        // nio21 w9 L5: the coders are read from the Strings (W8L5-2).
        let s_latin1 = string_coder_is_latin1(ctx, this, &s_units);
        let o_latin1 = string_coder_is_latin1(ctx, other, &o_units);
        let eq = region_matches_ci_units(
            &s_units,
            s_latin1,
            toffset,
            &o_units,
            o_latin1,
            ooffset,
            len as usize,
        );
        return Ok(Some(Value::Int(i32::from(eq))));
    }
    for i in 0..len as usize {
        if s_units[toffset + i] != o_units[ooffset + i] {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// Case-insensitive comparison of two UTF-16 code units, transcribing
/// `StringUTF16.regionMatchesCI`:
///
/// ```text
/// if (c1 == c2) continue;
/// char u1 = Character.toUpperCase(c1);
/// char u2 = Character.toUpperCase(c2);
/// if (u1 == u2) continue;
/// if (Character.toLowerCase(u1) == Character.toLowerCase(u2)) continue;
/// return false;
/// ```
///
/// Note that the JDK lower-cases the **upper-cased** forms, not the originals;
/// that composition is what makes `U+212A` KELVIN SIGN match `k`.
///
/// W7-95a. The old body called `char::to_uppercase()`/`to_lowercase()`, the
/// FULL Unicode mappings, and compared the resulting iterators.
/// `"İ".regionMatches(true, 0, "i", 0, 1)` is `true` on OpenJDK 25.0.3+9 and
/// was `false` here: Java's 1:1 `Character.toLowerCase(U+0130)` is plain `i`,
/// while the full SpecialCasing mapping is two characters. See
/// [`java_char_to_upper_case`] for the BMP-wide derivation of the difference.
///
/// A surrogate code unit has no case mapping and both helpers return it
/// unchanged, so surrogates compare equal only when bit-identical — which is
/// what HotSpot answers.
///
/// nio21 w9 L5: a one-shot [`CiUnitFold`]; the CI natives hold one fold per
/// call instead, so this survives for the unit tests that pin its rows.
#[cfg_attr(not(test), allow(dead_code))]
pub fn code_unit_eq_ignore_case(a: u16, b: u16) -> bool {
    CiUnitFold::new().eq(a, b)
}

/// The per-code-unit case fold of the LATIN1 / mixed-coder CI arms
/// (`Character.toUpperCase(char)` / `toLowerCase(char)`), nio21 wave 9 L5.
///
/// It reads the JDK table `lang_math::character_case_map` -- the one the
/// `Character` natives use, checked against HotSpot 25.0.3+9 over every code
/// point (0 differences for `toUpperCase(int)` / `toLowerCase(int)`, and
/// `(char) toUpperCase((int) c) == toUpperCase(c)` for all 65 536 chars) -- in
/// place of [`java_char_to_upper_case`] / [`java_char_to_lower_case`], which
/// rebuild Java's simple mapping from Rust's FULL one plus hand-kept
/// exceptions. ASCII is answered inline (the tables agree there by
/// construction), so the switch `CRATONVM_STRING_CI_JDK_CASE_TABLE` (default
/// ON, `=0` = the Rust-derived pair) is read at most once per call, and only
/// when a non-ASCII unit reaches the fold -- never on an ASCII header name.
pub struct CiUnitFold {
    pub jdk_table: Option<bool>,
}

impl CiUnitFold {
    pub fn new() -> Self {
        Self { jdk_table: None }
    }

    pub fn jdk_table(&mut self) -> bool {
        *self.jdk_table.get_or_insert_with(|| {
            true
        })
    }

    pub fn upper(&mut self, ch: u16) -> u16 {
        if ch < 0x80 {
            return if (0x61..=0x7A).contains(&ch) {
                ch - 0x20
            } else {
                ch
            };
        }
        if self.jdk_table() {
            // Cast: a BMP code point maps inside the BMP (no exception in the
            // JDK table; see the doc above).
            crate::lang_math::character_case_map(u32::from(ch), true) as u16
        } else {
            java_char_to_upper_case(ch)
        }
    }

    pub fn lower(&mut self, ch: u16) -> u16 {
        if ch < 0x80 {
            return if (0x41..=0x5A).contains(&ch) {
                ch + 0x20
            } else {
                ch
            };
        }
        if self.jdk_table() {
            // Cast: as in `upper`.
            crate::lang_math::character_case_map(u32::from(ch), false) as u16
        } else {
            java_char_to_lower_case(ch)
        }
    }

    /// `StringLatin1.regionMatchesCI_UTF16`'s per-unit test.
    pub fn eq(&mut self, a: u16, b: u16) -> bool {
        if a == b {
            return true;
        }
        let ua = self.upper(a);
        let ub = self.upper(b);
        if ua == ub {
            return true;
        }
        self.lower(ua) == self.lower(ub)
    }

    /// `StringLatin1.compareToCI_UTF16`'s per-unit step: `0` to keep going,
    /// else the difference of the folded units.
    pub fn diff(&mut self, a: u16, b: u16) -> i32 {
        if a == b {
            return 0;
        }
        let ua = self.upper(a);
        let ub = self.upper(b);
        if ua == ub {
            return 0;
        }
        i32::from(self.lower(ua)) - i32::from(self.lower(ub))
    }
}

/// Whether `obj` is LATIN1-coded, read from the String itself (nio21 wave 9
/// L5, W8L5-2): a `byte[]` value carries `coder` in field 1 (`0` LATIN1,
/// `1` UTF16), which is what the JDK's `coder()` dispatch reads, so a UTF16
/// String holding only Latin-1 chars (`-XX:-CompactStrings`, or any producer
/// that does not compress) takes the JDK's UTF16 arm. A legacy `char[]` value
/// has no coder; there "every unit <= 0xFF" ([`units_are_latin1`]) stands in,
/// as it does under `CRATONVM_STRING_CI_READ_CODER=0` (default ON; read only
/// when the coder and the content disagree).
pub fn string_coder_is_latin1(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    units: &[u16],
) -> bool {
    let inferred = units_are_latin1(units);
    let coder_latin1 = match string_char_array(ctx, obj) {
        Some((arr, _))
            if matches!(
                ctx.heap_element_type_of(arr),
                cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
            ) =>
        {
            Some(!matches!(ctx.get_field(obj, 1), Value::Int(1)))
        }
        _ => None,
    };
    match coder_latin1 {
        Some(c) if c != inferred => {
            {
                c
            }
        }
        _ => inferred,
    }
}

/// Whether a `String` with these code units is LATIN1-coded under the
/// default `COMPACT_STRINGS`: every JDK constructor, `substring`, and
/// `StringBuilder.toString` compress a UTF-16 buffer whose units all fit in a
/// byte, so "all units <= 0xFF" is the coder. (`-XX:-CompactStrings` is not
/// modelled; only `compareToIgnoreCase`'s magnitude on a surrogate mismatch
/// depends on it.)
pub fn units_are_latin1(units: &[u16]) -> bool {
    units.iter().all(|&u| u <= 0xFF)
}

/// `StringUTF16.compareCodePointCI(int, int)`: upper both with
/// `Character.toUpperCase(int)`, then lower the uppers with
/// `Character.toLowerCase(int)`, and return the difference of the lowers.
pub fn compare_code_point_ci(cp1: i32, cp2: i32) -> i32 {
    use crate::lang_math::character_case_map;
    let u1 = character_case_map(cp1 as u32, true);
    let u2 = character_case_map(cp2 as u32, true);
    if u1 != u2 {
        let l1 = character_case_map(u1, false);
        let l2 = character_case_map(u2, false);
        if l1 != l2 {
            return l1 as i32 - l2 as i32;
        }
    }
    0
}

/// `StringUTF16.codePointIncluding`: a low surrogate pairs BACKWARD with a
/// high surrogate at `index - 1` (only if `index > start`); a high surrogate
/// pairs FORWARD with a low surrogate at `index + 1` (only if
/// `index + 1 < end`), and that forward pair is returned NEGATED so the caller
/// skips the low half. Anything else is returned as is.
pub fn code_point_including(ba: &[u16], cp: i32, index: usize, start: usize, end: usize) -> i32 {
    let c = cp as u16;
    if !(0xD800..=0xDFFF).contains(&c) {
        return cp;
    }
    if (0xDC00..=0xDFFF).contains(&c) {
        if index > start {
            let h = ba[index - 1];
            if (0xD800..=0xDBFF).contains(&h) {
                return to_code_point(h, c);
            }
        }
    } else if index + 1 < end {
        let l = ba[index + 1];
        if (0xDC00..=0xDFFF).contains(&l) {
            return -to_code_point(c, l);
        }
    }
    cp
}

#[inline]
pub fn to_code_point(high: u16, low: u16) -> i32 {
    ((i32::from(high) - 0xD800) << 10) + (i32::from(low) - 0xDC00) + 0x10000
}

/// `StringUTF16.compareToCIImpl`, transcribed statement for statement
/// (OpenJDK 25.0.3+9). Both `StringUTF16.compareToCI` and
/// `StringUTF16.regionMatchesCI` (`== 0`) are this loop, which folds a
/// surrogate PAIR as one code point. The per-code-unit
/// [`code_unit_eq_ignore_case`] it replaces for UTF-16/UTF-16 operands
/// answered `"𐐨".equalsIgnoreCase("𐐀")` (U+10428 / U+10400) `false`: the
/// high halves are identical, and the low halves `U+DC28` / `U+DC00` have no
/// case mapping of their own. HotSpot answers `true`.
///
/// The index skew is the JDK's too: a forward pair advances only its own
/// side, so a pair on one side against a back-paired low surrogate on the
/// other leaves `k1` and `k2` one apart for the rest of the scan.
pub fn utf16_compare_to_ci_impl(
    value: &[u16],
    toffset: usize,
    tlen: usize,
    other: &[u16],
    ooffset: usize,
    olen: usize,
) -> i32 {
    let tlast = toffset + tlen;
    let olast = ooffset + olen;
    let mut k1 = toffset;
    let mut k2 = ooffset;
    while k1 < tlast && k2 < olast {
        let mut cp1 = i32::from(value[k1]);
        let mut cp2 = i32::from(other[k2]);
        if cp1 != cp2 && compare_code_point_ci(cp1, cp2) != 0 {
            cp1 = code_point_including(value, cp1, k1, toffset, tlast);
            if cp1 < 0 {
                k1 += 1;
                cp1 = -cp1;
            }
            cp2 = code_point_including(other, cp2, k2, ooffset, olast);
            if cp2 < 0 {
                k2 += 1;
                cp2 = -cp2;
            }
            let diff = compare_code_point_ci(cp1, cp2);
            if diff != 0 {
                return diff;
            }
        }
        k1 += 1;
        k2 += 1;
    }
    tlen as i32 - olen as i32
}

/// `String.regionMatches(true, ..)` past its bounds test: the JDK's
/// coder dispatch. LATIN1/LATIN1 and the mixed pairs are per code unit
/// (`StringLatin1.regionMatchesCI{,_UTF16}`); UTF16/UTF16 is
/// [`utf16_compare_to_ci_impl`]` == 0`. The caller has already checked that
/// `toffset + len <= a.len()` and `ooffset + len <= b.len()`.
///
/// `a_latin1` / `b_latin1` are the operands' coders
/// ([`string_coder_is_latin1`] where the caller has the String objects).
pub fn region_matches_ci_units(
    a: &[u16],
    a_latin1: bool,
    toffset: usize,
    b: &[u16],
    b_latin1: bool,
    ooffset: usize,
    len: usize,
) -> bool {
    // Order matters for cost: the LATIN1 tests come first so the hot
    // header-name path never reads the switch.
    if a_latin1 || b_latin1 {
        let mut fold = CiUnitFold::new();
        return a[toffset..toffset + len]
            .iter()
            .zip(&b[ooffset..ooffset + len])
            .all(|(&x, &y)| fold.eq(x, y));
    }
    utf16_compare_to_ci_impl(a, toffset, len, b, ooffset, len) == 0
}



/// regionMatches(int toffset, String other, int ooffset, int len) — case-sensitive
pub fn native_string_region_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let toffset_i = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let other = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let ooffset_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    region_matches_impl(ctx, this, toffset_i, other, ooffset_i, len_i, false)
}

/// formatted(Object[]) — instance method: this.formatted(args) → String.format(this, args)
/// String.format(Locale, String, Object...) — static method with Locale (ignored for now).
/// args[0] = Locale, args[1] = format String, args[2] = Object[]
pub fn native_string_format_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the Locale. It used to be DISCARDED, which made
    // `String.format(Locale.GERMANY, "%,.2f", 1234.5)` answer the US
    // `1,234.50` where HotSpot 25 answers `1.234,50` — the grouping and
    // decimal separators are swapped in German, and France's grouping
    // separator is not even an ASCII space (U+202F). Both rows are in
    // `docs/feature-designs/native-dispatch-architecture.md`.
    let locale = match args.first() {
        Some(Value::Object(l)) => *l,
        _ => None,
    };
    let format_args = [
        args.get(1).cloned().unwrap_or(Value::Object(None)),
        args.get(2).cloned().unwrap_or(Value::Object(None)),
    ];
    // `Given`, not `DefaultFormat`, EVEN WHEN `locale` IS `None`: an explicit
    // `null` `Locale` is the JDK's "no localization is applied", a different
    // request from an overload that has no `Locale` at all. Collapsing the two
    // is precisely the bug `native_string_format` no longer has, and re-making
    // it here would be that bug arriving from the other direction.
    // Unpinned here; `format_impl` pins it for the duration of the call
    // (gen r4w3/rooting).
    match format_impl(
        ctx,
        &format_args,
        FmtLocale::Given(locale.map(|obj| FmtLocaleRef { obj, pin: None })),
    ) {
        Ok(v) => Ok(v),
        Err(_) => ctx.invoke_special_bytecode_only(
            "java/lang/String",
            "format",
            "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
            args,
        ),
    }
}

pub fn native_string_formatted(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (the format string), args[1] = Object[]
    let format_args = [
        args.first().cloned().unwrap_or(Value::Object(None)),
        args.get(1).cloned().unwrap_or(Value::Object(None)),
    ];
    match format_impl(ctx, &format_args, FmtLocale::DefaultFormat) {
        Ok(v) => Ok(v),
        Err(_) => ctx.invoke_special_bytecode_only(
            "java/lang/String",
            "formatted",
            "([Ljava/lang/Object;)Ljava/lang/String;",
            args,
        ),
    }
}

// ---------------------------------------------------------------------------
// java.lang.StringUTF16 — static helpers used during <clinit>
// ---------------------------------------------------------------------------
//
// `isBigEndian()Z` is called from `StringUTF16.<clinit>` on JDK 25 to decide
// which byte order the compact-string `byte[]` uses for its `char` pairs —
// it seeds the `HI_BYTE_SHIFT` / `LO_BYTE_SHIFT` constants that `putChar` /
// `getChar` / `compress` / `toBytes` use throughout `StringUTF16`. In the real
// JDK this value is `UNSAFE.isBigEndian()`, i.e. the *host* CPU byte order.
//
// This MUST agree with the byte order CratonVM's own Rust string code uses.
// CratonVM stores every compact UTF-16 string `byte[]` **little-endian** (low
// byte first) regardless of host architecture — see `create_java_string` in
// `vm/src/vm/vm_object.rs` (`lo = unit & 0xFF` written at `i*2`, `hi =
// unit >> 8` at `i*2+1`), and every native String accessor in this file
// decodes the matching `(hi << 8) | lo` from those same little-endian slots
// (see `decode_string_value` above: `lo` at `c*2`, `hi` at `c*2+1`).
// Constant-pool strings, `String.substring`, `String.concat`, interning, etc.
// all go through that little-endian Rust path. On the x86-64 target this VM
// runs on, that also matches the real host endianness.
//
// FIX: this previously returned `Int(1)` (big-endian), justified by a doc
// comment that claimed `create_java_string` wrote big-endian (`hi` at `i*2`).
// That claim was stale/incorrect: the VM has always written little-endian
// (low byte at the even index). Returning `true` made the *real-JDK*
// `StringUTF16` bytecode (e.g. the `String(char[])` constructor, which has no
// native override) build a **big-endian** `byte[]`, while CratonVM's native
// `charAt` / `toCharArray` / `hashCode` / `equals` read it little-endian — so
// the two halves of the VM disagreed on every non-Latin-1 string (byte-swapped
// `char` values). Returning `false` (`Int(0)`) makes the JDK bytecode use the
// same little-endian layout as CratonVM's Rust code, keeping every code path
// consistent. `HI_BYTE_SHIFT == 8` in OpenJDK <=> big-endian; here it is 0.
//
// # On JDK 25 this registration never fires, and that is not a defect
//
// `--dump-native-registry` against Temurin 25.0.3, checked while closing the
// `String.hashCode` record because it asked whether `HI_BYTE_SHIFT` /
// `LO_BYTE_SHIFT` are populated at all:
//
//   java/lang/StringUTF16.isBigEndian()Z   loaded: true  declared: FALSE
//                                          has_code: false  invocations: 0
//
// `declared: false` — the method does not exist on JDK 25's `StringUTF16`. Its
// `<clinit>` reads the byte order from `UNSAFE`, which this VM answers through
// `jdk/internal/misc/UnsafeConstants.BIG_ENDIAN` (the boot log's
// "UnsafeConstants populated (5/5)"). The statics are populated and
// little-endian: `probes/StringUtf16ClassShapeProbe` reads them back as
// `HI_BYTE_SHIFT=0` / `LO_BYTE_SHIFT=8`, identical to HotSpot.
//
// So this stays registered for images that DO declare the method, where it must
// give the same answer `UnsafeConstants` gives, which it does. A census row
// reading `has_code: false` here means "absent from this image", not "an
// unimplemented native something is waiting on" — the distinction cost a
// paragraph of doubt in the record that filed the UTF-16 hash defect.
//
// **"(JDK 17/21)" used to be an assumption in this sentence. It is now a
// measurement.** MEASURED 2026-08-21 over the nine supported images —
// `javap -p --system <image> java.lang.StringUTF16`:
//
// ```text
//   jdk17-linux  jdk17-windows  jdk17-mac-x64    private static native boolean isBigEndian();
//   jdk21-linux  jdk21-windows  jdk21-mac-x64    private static native boolean isBigEndian();
//   jdk25-linux  jdk25-windows  jdk25-mac-x64    ABSENT
// ```
//
// Six of nine declare it. Re-derivable with
// `scripts/jdk-only-no-image-methods.py`, which reports this row as `PARTIAL`
// and exists because this comment was the standing witness that a
// single-image census cannot adjudicate the population it belongs to.
// See `docs/known-issues/jdk-only/WORKER-3-NOTE-2-*.md`: 62 of the 355 rows a
// JDK-25-only adjudication calls "declared nowhere" are this same shape.
//
// Arity is guaranteed by the verifier (`()Z`); we ignore any extra args
// defensively and return the constant unconditionally.
pub fn native_string_utf16_is_big_endian(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // FIX: little-endian to match create_java_string + this file's decoders
    // (was Int(1)). Matches the x86-64 host byte order.
    Ok(Some(Value::Int(0)))
}

/// `java.lang.StringUTF16.getChars(byte[] value, int srcBegin, int srcEnd,
/// char[] dst, int dstBegin)`.
///
/// OpenJDK implements this as a per-code-unit bytecode loop. Hibernate's
/// metamodel bootstrap calls it heavily while constructing annotations and
/// generated proxies, leaving a cold one-shot transaction entirely in the
/// interpreter. Bulk-read the compact UTF-16 source and decode it in Rust,
/// while retaining the JDK's deliberately asymmetric bounds behavior: when
/// `srcBegin >= srcEnd` it performs no source or destination dereference.
pub fn native_string_utf16_get_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if src_begin >= src_end {
        return Ok(None);
    }
    let value = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    // `isub` in the JDK bytecode has wrapping `int` arithmetic.  Keep that
    // behavior before the bounds check so extreme malformed ranges report a
    // StringIndexOutOfBoundsException rather than overflowing Rust arithmetic.
    let count = src_end.wrapping_sub(src_begin);
    let source_len = (ctx.array_length(value) / 2) as i32;
    if bounds_off_count_violation(src_begin, count, source_len).is_some() {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range_size(
            src_begin, count, source_len,
        )
        .into());
    }
    // The bytecode validates the source before its first destination access.
    // Preserve that ordering when both inputs are invalid/null.
    let dst = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = count as usize;
    let dst_len = ctx.array_length(dst) as i64;
    let copy_end = i64::from(dst_begin) + count as i64;
    if dst_begin < 0 || copy_end > dst_len {
        // The JDK's loop `dst[dstBegin++] = getChar(value, i)` (a `castore`):
        // the units that fit are stored, then the first store out of range
        // raises the array access's AIOOBE, "Index n out of bounds for length
        // m" (interpreter round i5 wave 100, lane L7; this said "Array index
        // out of range: <last index>" and stored nothing).
        let length = i32::try_from(dst_len).unwrap_or(i32::MAX);
        if dst_begin < 0 {
            return Err(cratonvm_types::error::RuntimeError::aioobe(dst_begin, length).into());
        }
        let fit = (dst_len - i64::from(dst_begin)).clamp(0, count as i64) as usize;
        for k in 0..fit {
            let i = src_begin as usize + k;
            let mut pair = [0u8; 2];
            if ctx.read_byte_array_into(value, i * 2, &mut pair) == 2 {
                let unit = u16::from(pair[0]) | (u16::from(pair[1]) << 8);
                ctx.set_array_element(dst, dst_begin as usize + k, Value::Int(i32::from(unit)));
            }
        }
        let first_bad = dst_begin.max(length);
        return Err(cratonvm_types::error::RuntimeError::aioobe(first_bad, length).into());
    }
    let mut bytes = vec![0u8; count * 2];
    if ctx.read_byte_array_into(value, src_begin as usize * 2, &mut bytes) != bytes.len() {
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(src_begin).into());
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from(pair[0]) | (u16::from(pair[1]) << 8))
        .collect();
    // The VM implementation performs one checked memcpy into the compact
    // char-array payload. Keep the element fallback for test/mock contexts
    // and unusual heaps that do not expose the bulk hook.
    if !ctx.write_char_array_from(dst, dst_begin as usize, &units) {
        for (index, unit) in units.into_iter().enumerate() {
            ctx.set_array_element(dst, dst_begin as usize + index, Value::Int(unit as i32));
        }
    }
    Ok(None)
}

