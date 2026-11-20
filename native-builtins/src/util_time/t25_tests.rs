use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- Clock ---

#[test]
fn t25_clock_system_utc_returns_clock() {
    let mut ctx = mock_ctx();
    let v = native_clock_system_utc(&mut ctx, &[]).unwrap();
    assert!(matches!(v, Some(Value::Object(Some(_)))));
}

#[test]
fn t25_clock_instant_returns_instant() {
    let mut ctx = mock_ctx();
    let v = native_clock_instant(&mut ctx, &[]).unwrap();
    match v {
        Some(Value::Object(Some(inst))) => {
            // Field 0 (epoch seconds) must be positive since 1970.
            let sec = match ctx.get_field(inst, INST_FIELD_EPOCH_SEC) {
                Value::Long(v) => v,
                _ => 0,
            };
            assert!(sec > 0, "epoch seconds should be > 0, got {sec}");
        }
        other => panic!("expected Instant object, got {other:?}"),
    }
}

#[test]
fn t25_clock_millis_returns_long() {
    let mut ctx = mock_ctx();
    let v = native_clock_millis(&mut ctx, &[]).unwrap();
    match v {
        Some(Value::Long(m)) => assert!(m > 0),
        other => panic!("expected Long, got {other:?}"),
    }
}

// --- LocalDate.now(Clock) / LocalDateTime.now(Clock) ---

#[test]
fn t25_local_date_now_clock_round_trips() {
    let mut ctx = mock_ctx();
    let clock = match native_clock_system_utc(&mut ctx, &[]).unwrap() {
        Some(Value::Object(Some(c))) => c,
        _ => panic!("clock alloc failed"),
    };
    let v = native_ld_now_clock(&mut ctx, &[Value::Object(Some(clock))]).unwrap();
    match v {
        Some(Value::Object(Some(ld))) => {
            let y = match ctx.get_field(ld, LD_FIELD_YEAR) {
                Value::Int(v) => v,
                _ => 0,
            };
            // Proleptic Gregorian year — must be >= 1970 for any
            // wall clock time after the Unix epoch.
            assert!(y >= 1970, "year should be >= 1970, got {y}");
        }
        other => panic!("expected LocalDate, got {other:?}"),
    }
}

// --- Duration.between ---

#[test]
fn t25_duration_between_same_instant_is_zero() {
    let mut ctx = mock_ctx();
    let i = alloc_instant(&mut ctx, 1_000, 500);
    let v = native_dur_between(&mut ctx, &[Value::Object(Some(i)), Value::Object(Some(i))])
        .unwrap();
    match v {
        Some(Value::Object(Some(d))) => {
            let s = match ctx.get_field(d, DUR_FIELD_SECONDS) {
                Value::Long(v) => v,
                _ => -1,
            };
            let n = match ctx.get_field(d, DUR_FIELD_NANOS) {
                Value::Int(v) => v,
                _ => -1,
            };
            assert_eq!(s, 0);
            assert_eq!(n, 0);
        }
        other => panic!("expected Duration, got {other:?}"),
    }
}

#[test]
fn t25_duration_between_positive_difference() {
    let mut ctx = mock_ctx();
    let start = alloc_instant(&mut ctx, 100, 0);
    let end = alloc_instant(&mut ctx, 142, 500_000_000);
    let v = native_dur_between(
        &mut ctx,
        &[Value::Object(Some(start)), Value::Object(Some(end))],
    )
    .unwrap();
    match v {
        Some(Value::Object(Some(d))) => {
            let s = match ctx.get_field(d, DUR_FIELD_SECONDS) {
                Value::Long(v) => v,
                _ => -1,
            };
            let n = match ctx.get_field(d, DUR_FIELD_NANOS) {
                Value::Int(v) => v,
                _ => -1,
            };
            assert_eq!(s, 42);
            assert_eq!(n, 500_000_000);
        }
        other => panic!("expected Duration, got {other:?}"),
    }
}

#[test]
fn t25_duration_between_negative_difference_normalizes() {
    let mut ctx = mock_ctx();
    let start = alloc_instant(&mut ctx, 200, 0);
    let end = alloc_instant(&mut ctx, 100, 0);
    let v = native_dur_between(
        &mut ctx,
        &[Value::Object(Some(start)), Value::Object(Some(end))],
    )
    .unwrap();
    match v {
        Some(Value::Object(Some(d))) => {
            let s = match ctx.get_field(d, DUR_FIELD_SECONDS) {
                Value::Long(v) => v,
                _ => 0,
            };
            let n = match ctx.get_field(d, DUR_FIELD_NANOS) {
                Value::Int(v) => v,
                _ => 0,
            };
            // -100 seconds — nanos normalized to [0, 999_999_999].
            // The JDK normalizes (-100, 0) to (-100, 0) — no
            // normalization needed here.
            assert_eq!(s, -100);
            assert_eq!(n, 0);
        }
        other => panic!("expected Duration, got {other:?}"),
    }
}

#[test]
fn t25_duration_between_null_start_throws_npe() {
    let mut ctx = mock_ctx();
    let i = alloc_instant(&mut ctx, 0, 0);
    let r = native_dur_between(&mut ctx, &[Value::Object(None), Value::Object(Some(i))]);
    assert!(r.is_err());
}

// --- Year ---

#[test]
fn t25_year_is_leap_static_2000() {
    let mut ctx = mock_ctx();
    let v = native_year_is_leap_static(&mut ctx, &[Value::Long(2000)]).unwrap();
    assert_eq!(v, Some(Value::Int(1)));
}

#[test]
fn t25_year_is_leap_static_1900() {
    let mut ctx = mock_ctx();
    let v = native_year_is_leap_static(&mut ctx, &[Value::Long(1900)]).unwrap();
    assert_eq!(v, Some(Value::Int(0)));
}

#[test]
fn t25_year_is_leap_static_2024() {
    let mut ctx = mock_ctx();
    let v = native_year_is_leap_static(&mut ctx, &[Value::Long(2024)]).unwrap();
    assert_eq!(v, Some(Value::Int(1)));
}

#[test]
fn t25_year_is_leap_static_2023() {
    let mut ctx = mock_ctx();
    let v = native_year_is_leap_static(&mut ctx, &[Value::Long(2023)]).unwrap();
    assert_eq!(v, Some(Value::Int(0)));
}

#[test]
fn t25_year_is_leap_instance() {
    let mut ctx = mock_ctx();
    let y = alloc_year(&mut ctx, 2024);
    let v = native_year_is_leap_instance(&mut ctx, &[Value::Object(Some(y))]).unwrap();
    assert_eq!(v, Some(Value::Int(1)));
}

#[test]
fn t25_year_length_leap_year_is_366() {
    let mut ctx = mock_ctx();
    let y = alloc_year(&mut ctx, 2024);
    let v = native_year_length(&mut ctx, &[Value::Object(Some(y))]).unwrap();
    assert_eq!(v, Some(Value::Int(366)));
}

#[test]
fn t25_year_length_non_leap_is_365() {
    let mut ctx = mock_ctx();
    let y = alloc_year(&mut ctx, 2023);
    let v = native_year_length(&mut ctx, &[Value::Object(Some(y))]).unwrap();
    assert_eq!(v, Some(Value::Int(365)));
}

// --- Month ---

#[test]
fn t25_month_of_valid_returns_month() {
    let mut ctx = mock_ctx();
    let v = native_month_of(&mut ctx, &[Value::Int(6)]).unwrap();
    match v {
        Some(Value::Object(Some(m))) => match ctx.get_field(m, MONTH_FIELD_VALUE) {
            Value::Int(val) => assert_eq!(val, 6),
            other => panic!("unexpected value field {other:?}"),
        },
        other => panic!("expected Month, got {other:?}"),
    }
}

#[test]
fn t25_month_of_out_of_range_throws() {
    let mut ctx = mock_ctx();
    assert!(native_month_of(&mut ctx, &[Value::Int(0)]).is_err());
    assert!(native_month_of(&mut ctx, &[Value::Int(13)]).is_err());
}

#[test]
fn t25_month_length_february_leap_is_29() {
    let mut ctx = mock_ctx();
    let feb = alloc_month(&mut ctx, 2);
    let v = native_month_length(&mut ctx, &[Value::Object(Some(feb)), Value::Int(1)]).unwrap();
    assert_eq!(v, Some(Value::Int(29)));
}

#[test]
fn t25_month_length_february_non_leap_is_28() {
    let mut ctx = mock_ctx();
    let feb = alloc_month(&mut ctx, 2);
    let v = native_month_length(&mut ctx, &[Value::Object(Some(feb)), Value::Int(0)]).unwrap();
    assert_eq!(v, Some(Value::Int(28)));
}

#[test]
fn t25_month_length_january_is_31_regardless_of_leap() {
    let mut ctx = mock_ctx();
    let jan = alloc_month(&mut ctx, 1);
    let v1 = native_month_length(&mut ctx, &[Value::Object(Some(jan)), Value::Int(0)]).unwrap();
    let v2 = native_month_length(&mut ctx, &[Value::Object(Some(jan)), Value::Int(1)]).unwrap();
    assert_eq!(v1, Some(Value::Int(31)));
    assert_eq!(v2, Some(Value::Int(31)));
}

#[test]
fn t25_month_length_june_is_30() {
    let mut ctx = mock_ctx();
    let jun = alloc_month(&mut ctx, 6);
    let v = native_month_length(&mut ctx, &[Value::Object(Some(jun)), Value::Int(0)]).unwrap();
    assert_eq!(v, Some(Value::Int(30)));
}

#[test]
fn t25_month_max_length_february_is_29() {
    let mut ctx = mock_ctx();
    let feb = alloc_month(&mut ctx, 2);
    let v = native_month_max_length(&mut ctx, &[Value::Object(Some(feb))]).unwrap();
    assert_eq!(v, Some(Value::Int(29)));
}

#[test]
fn t25_month_min_length_february_is_28() {
    let mut ctx = mock_ctx();
    let feb = alloc_month(&mut ctx, 2);
    let v = native_month_min_length(&mut ctx, &[Value::Object(Some(feb))]).unwrap();
    assert_eq!(v, Some(Value::Int(28)));
}

// --- DayOfWeek ---

#[test]
fn t25_dow_of_valid_returns_day() {
    let mut ctx = mock_ctx();
    let v = native_dow_of(&mut ctx, &[Value::Int(3)]).unwrap();
    match v {
        Some(Value::Object(Some(d))) => match ctx.get_field(d, DOW_FIELD_VALUE) {
            Value::Int(val) => assert_eq!(val, 3),
            other => panic!("unexpected value field {other:?}"),
        },
        other => panic!("expected DayOfWeek, got {other:?}"),
    }
}

#[test]
fn t25_dow_of_out_of_range_throws() {
    let mut ctx = mock_ctx();
    assert!(native_dow_of(&mut ctx, &[Value::Int(0)]).is_err());
    assert!(native_dow_of(&mut ctx, &[Value::Int(8)]).is_err());
}

#[test]
fn t25_dow_from_local_date() {
    // 1970-01-01 is a Thursday → DayOfWeek value 4.
    let mut ctx = mock_ctx();
    let date = alloc_local_date(&mut ctx, 1970, 1, 1);
    let v = native_dow_from(&mut ctx, &[Value::Object(Some(date))]).unwrap();
    match v {
        Some(Value::Object(Some(d))) => match ctx.get_field(d, DOW_FIELD_VALUE) {
            Value::Int(val) => assert_eq!(val, 4, "1970-01-01 is Thursday"),
            other => panic!("unexpected value field {other:?}"),
        },
        other => panic!("expected DayOfWeek, got {other:?}"),
    }
}

#[test]
fn t25_dow_from_local_date_known_sunday() {
    // 2024-01-07 is a Sunday → DayOfWeek value 7.
    let mut ctx = mock_ctx();
    let date = alloc_local_date(&mut ctx, 2024, 1, 7);
    let v = native_dow_from(&mut ctx, &[Value::Object(Some(date))]).unwrap();
    match v {
        Some(Value::Object(Some(d))) => match ctx.get_field(d, DOW_FIELD_VALUE) {
            Value::Int(val) => assert_eq!(val, 7, "2024-01-07 is Sunday"),
            other => panic!("unexpected value field {other:?}"),
        },
        other => panic!("expected DayOfWeek, got {other:?}"),
    }
}

#[test]
fn t25_dow_from_instant_epoch() {
    // Epoch 0 corresponds to 1970-01-01 (Thursday).
    let mut ctx = mock_ctx();
    let inst = alloc_instant(&mut ctx, 0, 0);
    let v = native_dow_from(&mut ctx, &[Value::Object(Some(inst))]).unwrap();
    match v {
        Some(Value::Object(Some(d))) => match ctx.get_field(d, DOW_FIELD_VALUE) {
            Value::Int(val) => assert_eq!(val, 4),
            other => panic!("unexpected value field {other:?}"),
        },
        other => panic!("expected DayOfWeek, got {other:?}"),
    }
}

#[test]
fn t25_dow_from_null_throws_npe() {
    let mut ctx = mock_ctx();
    assert!(native_dow_from(&mut ctx, &[Value::Object(None)]).is_err());
}

// --- ZoneRules.getOffset ---

#[test]
fn t25_zone_rules_get_offset_utc_returns_zero() {
    let mut ctx = mock_ctx();
    let zr = alloc_zone_rules(&mut ctx, "UTC");
    let inst = alloc_instant(&mut ctx, 0, 0);
    let v = native_zone_rules_get_offset(
        &mut ctx,
        &[Value::Object(Some(zr)), Value::Object(Some(inst))],
    )
    .unwrap();
    match v {
        Some(Value::Object(Some(zo))) => match ctx.get_field(zo, 0) {
            Value::Int(secs) => assert_eq!(secs, 0),
            other => panic!("unexpected total-seconds field {other:?}"),
        },
        other => panic!("expected ZoneOffset, got {other:?}"),
    }
}

#[test]
fn t25_zone_rules_get_offset_los_angeles() {
    let mut ctx = mock_ctx();
    let zr = alloc_zone_rules(&mut ctx, "America/Los_Angeles");
    let inst = alloc_instant(&mut ctx, 0, 0);
    let v = native_zone_rules_get_offset(
        &mut ctx,
        &[Value::Object(Some(zr)), Value::Object(Some(inst))],
    )
    .unwrap();
    match v {
        Some(Value::Object(Some(zo))) => match ctx.get_field(zo, 0) {
            Value::Int(secs) => assert_eq!(secs, -8 * 3600),
            other => panic!("unexpected total-seconds field {other:?}"),
        },
        other => panic!("expected ZoneOffset, got {other:?}"),
    }
}

// --- ZoneId.systemDefault tzdata path ---

#[test]
fn t25_zone_id_system_default_returns_zone() {
    let mut ctx = mock_ctx();
    let v = native_zone_id_system_default_tzdata(&mut ctx, &[]).unwrap();
    match v {
        Some(Value::Object(Some(z))) => match ctx.get_field(z, ZID_FIELD_ID) {
            Value::Object(Some(s)) => {
                let id = ctx.read_string(s).unwrap_or_default();
                assert!(!id.is_empty(), "zone id must not be empty");
            }
            other => panic!("unexpected zone id field {other:?}"),
        },
        other => panic!("expected ZoneId, got {other:?}"),
    }
}

// --- DateTimeFormatter.ofPattern(String, Locale) ---

#[test]
fn t25_dtf_of_pattern_locale_delegates() {
    let mut ctx = mock_ctx();
    let pat = ctx.create_string("yyyy-MM-dd");
    let locale = ctx.create_string("en_US"); // locale treated as opaque
    let v = native_dtf_of_pattern_locale(
        &mut ctx,
        &[Value::Object(Some(pat)), Value::Object(Some(locale))],
    )
    .unwrap();
    assert!(matches!(v, Some(Value::Object(Some(_)))));
}
