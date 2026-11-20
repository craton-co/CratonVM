#[allow(unused_imports)]
use super::*;


// ===== Phase 17: java.time — LocalDate, LocalTime, Instant, Duration =====

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(3), Value::Int(15)],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let y = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "getYear",
        "()I",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(y, Value::Int(2024));

    let m = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "getMonthValue",
        "()I",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(m, Value::Int(3));

    let d = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "getDayOfMonth",
        "()I",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(d, Value::Int(15));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_parse_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("2023-12-25")
    };
    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "parse",
        "(Ljava/lang/CharSequence;)Ljava/time/LocalDate;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let ts = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(r)) => r,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("2023-12-25".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_plus_days() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(1), Value::Int(30)],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    // Jan 30 + 3 days = Feb 2
    let result = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "plusDays",
        "(J)Ljava/time/LocalDate;",
        &[Value::Object(Some(date_obj)), Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    let result_obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let ts = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(result_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(r)) => r,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("2024-02-02".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_plus_months_clamp() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Jan 31 + 1 month = Feb 29 (2024 is leap year)
    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(1), Value::Int(31)],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "plusMonths",
        "(J)Ljava/time/LocalDate;",
        &[Value::Object(Some(date_obj)), Value::Long(1)],
    )
    .unwrap()
    .unwrap();
    let result_obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let d = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "getDayOfMonth",
        "()I",
        &[Value::Object(Some(result_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(d, Value::Int(29)); // Clamped to Feb 29 (leap year)
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_is_before_after_equal() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let d1 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let d2 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(6), Value::Int(15)],
    )
    .unwrap()
    .unwrap();
    let d1o = match d1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    let d2o = match d2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };

    let before = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "isBefore",
        "(Ljava/time/chrono/ChronoLocalDate;)Z",
        &[Value::Object(Some(d1o)), Value::Object(Some(d2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(before, Value::Int(1));

    let after = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "isAfter",
        "(Ljava/time/chrono/ChronoLocalDate;)Z",
        &[Value::Object(Some(d1o)), Value::Object(Some(d2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after, Value::Int(0));

    let equal = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "isEqual",
        "(Ljava/time/chrono/ChronoLocalDate;)Z",
        &[Value::Object(Some(d1o)), Value::Object(Some(d1o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(equal, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_leap_year() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let d2024 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let d2024o = match d2024 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let leap = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "isLeapYear",
        "()Z",
        &[Value::Object(Some(d2024o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap, Value::Int(1));

    let d2023 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2023), Value::Int(1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let d2023o = match d2023 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let not_leap = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "isLeapYear",
        "()Z",
        &[Value::Object(Some(d2023o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_leap, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_epoch_day_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(1970), Value::Int(1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let epoch = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "toEpochDay",
        "()J",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(epoch, Value::Long(0));

    // Roundtrip
    let back = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "ofEpochDay",
        "(J)Ljava/time/LocalDate;",
        &[Value::Long(0)],
    )
    .unwrap()
    .unwrap();
    let back_obj = match back {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let ts = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(back_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("1970-01-01".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_date_day_of_year() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2024), Value::Int(3), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let date_obj = match date {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let doy = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "getDayOfYear",
        "()I",
        &[Value::Object(Some(date_obj))],
    )
    .unwrap()
    .unwrap();
    // Jan(31) + Feb(29 in 2024) + 1 = 61
    assert_eq!(doy, Value::Int(61));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_time_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let time = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(III)Ljava/time/LocalTime;",
        &[Value::Int(14), Value::Int(30), Value::Int(45)],
    )
    .unwrap()
    .unwrap();
    let time_obj = match time {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let h = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "getHour",
        "()I",
        &[Value::Object(Some(time_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h, Value::Int(14));

    let m = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "getMinute",
        "()I",
        &[Value::Object(Some(time_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(m, Value::Int(30));

    let s = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "getSecond",
        "()I",
        &[Value::Object(Some(time_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(s, Value::Int(45));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_time_parse_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("09:15:30")
    };
    let time = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "parse",
        "(Ljava/lang/CharSequence;)Ljava/time/LocalTime;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    let time_obj = match time {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let ts = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(time_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("09:15:30".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_time_plus_hours_wrap() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let time = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(II)Ljava/time/LocalTime;",
        &[Value::Int(22), Value::Int(30)],
    )
    .unwrap()
    .unwrap();
    let time_obj = match time {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    // 22:30 + 5 hours = 03:30 (wraps around)
    let result = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "plusHours",
        "(J)Ljava/time/LocalTime;",
        &[Value::Object(Some(time_obj)), Value::Long(5)],
    )
    .unwrap()
    .unwrap();
    let result_obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let h = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "getHour",
        "()I",
        &[Value::Object(Some(result_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h, Value::Int(3));

    let m = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "getMinute",
        "()I",
        &[Value::Object(Some(result_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(m, Value::Int(30));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_time_is_before_after() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let t1 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(II)Ljava/time/LocalTime;",
        &[Value::Int(9), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    let t2 = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(II)Ljava/time/LocalTime;",
        &[Value::Int(17), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    let t1o = match t1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let t2o = match t2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let before = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "isBefore",
        "(Ljava/time/LocalTime;)Z",
        &[Value::Object(Some(t1o)), Value::Object(Some(t2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(before, Value::Int(1));

    let after = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "isAfter",
        "(Ljava/time/LocalTime;)Z",
        &[Value::Object(Some(t1o)), Value::Object(Some(t2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn local_time_second_of_day() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let time = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(III)Ljava/time/LocalTime;",
        &[Value::Int(1), Value::Int(30), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    let time_obj = match time {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sod = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "toSecondOfDay",
        "()I",
        &[Value::Object(Some(time_obj))],
    )
    .unwrap()
    .unwrap();
    // 1*3600 + 30*60 = 5400
    assert_eq!(sod, Value::Int(5400));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn instant_of_epoch_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let inst = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "ofEpochSecond",
        "(J)Ljava/time/Instant;",
        &[Value::Long(1700000000)],
    )
    .unwrap()
    .unwrap();
    let inst_obj = match inst {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "getEpochSecond",
        "()J",
        &[Value::Object(Some(inst_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(1700000000));

    let nano = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "getNano",
        "()I",
        &[Value::Object(Some(inst_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nano, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn instant_of_epoch_milli_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let inst = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "ofEpochMilli",
        "(J)Ljava/time/Instant;",
        &[Value::Long(1700000500)],
    )
    .unwrap()
    .unwrap();
    let inst_obj = match inst {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let millis = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "toEpochMilli",
        "()J",
        &[Value::Object(Some(inst_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(1700000500));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn instant_plus_minus_seconds() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let inst = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "ofEpochSecond",
        "(J)Ljava/time/Instant;",
        &[Value::Long(1000)],
    )
    .unwrap()
    .unwrap();
    let inst_obj = match inst {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let plus = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "plusSeconds",
        "(J)Ljava/time/Instant;",
        &[Value::Object(Some(inst_obj)), Value::Long(500)],
    )
    .unwrap()
    .unwrap();
    let plus_obj = match plus {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "getEpochSecond",
        "()J",
        &[Value::Object(Some(plus_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(1500));

    let minus = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "minusSeconds",
        "(J)Ljava/time/Instant;",
        &[Value::Object(Some(plus_obj)), Value::Long(200)],
    )
    .unwrap()
    .unwrap();
    let minus_obj = match minus {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec2 = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "getEpochSecond",
        "()J",
        &[Value::Object(Some(minus_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec2, Value::Long(1300));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn instant_is_before_after() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let i1 = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "ofEpochSecond",
        "(J)Ljava/time/Instant;",
        &[Value::Long(100)],
    )
    .unwrap()
    .unwrap();
    let i2 = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "ofEpochSecond",
        "(J)Ljava/time/Instant;",
        &[Value::Long(200)],
    )
    .unwrap()
    .unwrap();
    let i1o = match i1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let i2o = match i2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let before = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "isBefore",
        "(Ljava/time/Instant;)Z",
        &[Value::Object(Some(i1o)), Value::Object(Some(i2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(before, Value::Int(1));

    let after = call_native(
        &shared,
        &mut thread,
        "java/time/Instant",
        "isAfter",
        "(Ljava/time/Instant;)Z",
        &[Value::Object(Some(i1o)), Value::Object(Some(i2o))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let dur = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofHours",
        "(J)Ljava/time/Duration;",
        &[Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    let dur_obj = match dur {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(7200));

    let hours = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "toHours",
        "()J",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hours, Value::Long(2));

    let minutes = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "toMinutes",
        "()J",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(minutes, Value::Long(120));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_plus_minus() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let d1 = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(100)],
    )
    .unwrap()
    .unwrap();
    let d2 = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(50)],
    )
    .unwrap()
    .unwrap();
    let d1o = match d1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let d2o = match d2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sum = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "plus",
        "(Ljava/time/Duration;)Ljava/time/Duration;",
        &[Value::Object(Some(d1o)), Value::Object(Some(d2o))],
    )
    .unwrap()
    .unwrap();
    let sum_obj = match sum {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(sum_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(150));

    let diff = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "minus",
        "(Ljava/time/Duration;)Ljava/time/Duration;",
        &[Value::Object(Some(d1o)), Value::Object(Some(d2o))],
    )
    .unwrap()
    .unwrap();
    let diff_obj = match diff {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec2 = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(diff_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec2, Value::Long(50));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_multiplied_by() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let dur = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(30)],
    )
    .unwrap()
    .unwrap();
    let dur_obj = match dur {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "multipliedBy",
        "(J)Ljava/time/Duration;",
        &[Value::Object(Some(dur_obj)), Value::Long(4)],
    )
    .unwrap()
    .unwrap();
    let result_obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(result_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(120));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_is_zero_negative() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let zero = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(0)],
    )
    .unwrap()
    .unwrap();
    let zero_obj = match zero {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let is_zero = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "isZero",
        "()Z",
        &[Value::Object(Some(zero_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_zero, Value::Int(1));

    let neg = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(-5)],
    )
    .unwrap()
    .unwrap();
    let neg_obj = match neg {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let is_neg = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "isNegative",
        "()Z",
        &[Value::Object(Some(neg_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_neg, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_negated_and_abs() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let dur = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofSeconds",
        "(J)Ljava/time/Duration;",
        &[Value::Long(60)],
    )
    .unwrap()
    .unwrap();
    let dur_obj = match dur {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let negated = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "negated",
        "()Ljava/time/Duration;",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    let negated_obj = match negated {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(negated_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(-60));

    let abs = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "abs",
        "()Ljava/time/Duration;",
        &[Value::Object(Some(negated_obj))],
    )
    .unwrap()
    .unwrap();
    let abs_obj = match abs {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let sec2 = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(abs_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec2, Value::Long(60));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn duration_of_millis() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let dur = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "ofMillis",
        "(J)Ljava/time/Duration;",
        &[Value::Long(2500)],
    )
    .unwrap()
    .unwrap();
    let dur_obj = match dur {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let millis = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "toMillis",
        "()J",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(2500));

    let sec = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getSeconds",
        "()J",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sec, Value::Long(2));

    let nano = call_native(
        &shared,
        &mut thread,
        "java/time/Duration",
        "getNano",
        "()I",
        &[Value::Object(Some(dur_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nano, Value::Int(500_000_000));
}

// ===== Phase 18: TreeMap + TreeSet tests =====

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_put_get_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("apple")
    };
    let va = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("red")
    };
    let kb = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("banana")
    };
    let vb = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("yellow")
    };
    let kc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("cherry")
    };
    let vc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("pink")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(kb)),
            Value::Object(Some(vb)),
        ],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(va)),
        ],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(kc)),
            Value::Object(Some(vc)),
        ],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    let got = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(kb))],
    )
    .unwrap()
    .unwrap();
    let got_str = match got {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(got_str, Some("yellow".to_string()));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_sorted_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for (k, v) in [("cherry", "3"), ("apple", "1"), ("banana", "2")] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        let vs = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(v)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        )
        .unwrap();
    }

    let first = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "firstKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let first_s = match first {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(first_s, Some("apple".to_string()));

    let last = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "lastKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let last_s = match last {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(last_s, Some("cherry".to_string()));

    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let ts_s = match ts {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ts_s, Some("{apple=1, banana=2, cherry=3}".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_remove() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        let vs = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(v)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        )
        .unwrap();
    }

    let bk = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let removed = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(bk))],
    )
    .unwrap()
    .unwrap();
    let removed_s = match removed {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(removed_s, Some("2".to_string()));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));

    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let ts_s = match ts {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ts_s, Some("{a=1, c=3}".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_ceiling_floor_higher_lower() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for k in ["b", "d", "f"] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(ks)),
            ],
        )
        .unwrap();
    }

    let c_key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let ceiling = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(c_key))],
    )
    .unwrap()
    .unwrap();
    let cs = match ceiling {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(cs, Some("d".to_string()));

    let c_key2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let floor = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(c_key2))],
    )
    .unwrap()
    .unwrap();
    let fs = match floor {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(fs, Some("b".to_string()));

    let d_key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let higher = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(d_key))],
    )
    .unwrap()
    .unwrap();
    let hs = match higher {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(hs, Some("f".to_string()));

    let d_key2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let lower = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(tm)), Value::Object(Some(d_key2))],
    )
    .unwrap()
    .unwrap();
    let ls = match lower {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ls, Some("b".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_first_last_entry() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for (k, v) in [("x", "24"), ("a", "1"), ("m", "13")] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        let vs = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(v)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        )
        .unwrap();
    }

    let first_entry = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let entry_obj = match first_entry {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let key_val = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry_obj))],
    )
    .unwrap()
    .unwrap();
    let ks = match key_val {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ks, Some("a".to_string()));

    let last_entry = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let lentry_obj = match last_entry {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let lkey = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(lentry_obj))],
    )
    .unwrap()
    .unwrap();
    let lks = match lkey {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(lks, Some("x".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_poll_first_last_entry() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for k in ["c", "a", "b"] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(ks)),
            ],
        )
        .unwrap();
    }

    let polled = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let pe = match polled {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let pk = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pe))],
    )
    .unwrap()
    .unwrap();
    let pks = match pk {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(pks, Some("a".to_string()));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_contains_key_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
    let va = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("world")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(va)),
        ],
    )
    .unwrap();

    let check_k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
    let ck = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(tm)), Value::Object(Some(check_k))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ck, Value::Int(1));

    let check_v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("world")
    };
    let cv = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(tm)), Value::Object(Some(check_v))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cv, Value::Int(1));

    let miss_k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("nope")
    };
    let mk = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(tm)), Value::Object(Some(miss_k))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(mk, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_head_tail_sub_map() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4"), ("e", "5")] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        let vs = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(v)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        )
        .unwrap();
    }

    let c_key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let head = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        &[Value::Object(Some(tm)), Value::Object(Some(c_key))],
    )
    .unwrap()
    .unwrap();
    let head_obj = match head {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let head_size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(head_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(head_size, Value::Int(2));

    let c_key2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let tail = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        &[Value::Object(Some(tm)), Value::Object(Some(c_key2))],
    )
    .unwrap()
    .unwrap();
    let tail_obj = match tail {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let tail_size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tail_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(tail_size, Value::Int(3));

    let b_key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let d_key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let sub = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(b_key)),
            Value::Object(Some(d_key)),
        ],
    )
    .unwrap()
    .unwrap();
    let sub_obj = match sub {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let sub_size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(sub_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sub_size, Value::Int(2));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_put_replace() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("old")
    };
    let old = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Object(None));

    let ka2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("new")
    };
    let prev = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka2)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    let prev_s = match prev {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(prev_s, Some("old".to_string()));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_key_set_sorted() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    for k in ["cherry", "apple", "banana"] {
        let ks = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(k)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(tm)),
                Value::Object(Some(ks)),
                Value::Object(Some(ks)),
            ],
        )
        .unwrap();
    }

    let keyset = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "keySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    let ks_obj = match keyset {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let ts_str = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ks_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_s = match ts_str {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ts_s, Some("[apple, banana, cherry]".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_add_contains_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["cherry", "apple", "banana", "apple"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "size",
        "()I",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    let check = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("banana")
    };
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ts)), Value::Object(Some(check))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    let miss = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("grape")
    };
    let not_found = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ts)), Value::Object(Some(miss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_found, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_sorted_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["dog", "cat", "ant", "bat"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let first = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "first",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let fs = match first {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(fs, Some("ant".to_string()));

    let last = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "last",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let ls = match last {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ls, Some("dog".to_string()));

    let str_val = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let ss = match str_val {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ss, Some("[ant, bat, cat, dog]".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_remove() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["a", "b", "c"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let bk = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let removed = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "remove",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ts)), Value::Object(Some(bk))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(removed, Value::Int(1));

    let str_val = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let ss = match str_val {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ss, Some("[a, c]".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_ceiling_floor_higher_lower() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["b", "d", "f"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let c = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let ceiling = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(ts)), Value::Object(Some(c))],
    )
    .unwrap()
    .unwrap();
    let cs = match ceiling {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(cs, Some("d".to_string()));

    let c2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let floor = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(ts)), Value::Object(Some(c2))],
    )
    .unwrap()
    .unwrap();
    let fs = match floor {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(fs, Some("b".to_string()));

    let d = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let higher = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(ts)), Value::Object(Some(d))],
    )
    .unwrap()
    .unwrap();
    let hs = match higher {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(hs, Some("f".to_string()));

    let d2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let lower = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(ts)), Value::Object(Some(d2))],
    )
    .unwrap()
    .unwrap();
    let ls = match lower {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ls, Some("b".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_head_tail_sub_set() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["a", "b", "c", "d", "e"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let c_k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let head = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        &[Value::Object(Some(ts)), Value::Object(Some(c_k))],
    )
    .unwrap()
    .unwrap();
    let ho = match head {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let hs = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "size",
        "()I",
        &[Value::Object(Some(ho))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hs, Value::Int(2));

    let c_k2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let tail = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        &[Value::Object(Some(ts)), Value::Object(Some(c_k2))],
    )
    .unwrap()
    .unwrap();
    let to = match tail {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let tsz = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "size",
        "()I",
        &[Value::Object(Some(to))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(tsz, Value::Int(3));

    let b_k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let d_k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };
    let sub = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
        &[
            Value::Object(Some(ts)),
            Value::Object(Some(b_k)),
            Value::Object(Some(d_k)),
        ],
    )
    .unwrap()
    .unwrap();
    let so = match sub {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };
    let ssz = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "size",
        "()I",
        &[Value::Object(Some(so))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ssz, Value::Int(2));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_iterator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    for name in ["c", "a", "b"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ts)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let itr_obj = match itr {
        Value::Object(Some(r)) => r,
        _ => panic!("expected"),
    };

    let mut collected = Vec::new();
    loop {
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet$Itr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(itr_obj))],
        )
        .unwrap()
        .unwrap();
        if has == Value::Int(0) {
            break;
        }
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/TreeSet$Itr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(itr_obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(r)) = val {
            if let Some(s) = read_java_string(&shared.mem.heap, r) {
                collected.push(s);
            }
        }
    }
    assert_eq!(collected, vec!["a", "b", "c"]);
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_add_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let al = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    for name in ["z", "m", "a"] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(name)
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "addAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(ts)), Value::Object(Some(al))],
    )
    .unwrap();

    let str_val = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    let ss = match str_val {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(ss, Some("[a, m, z]".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_get_or_default() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let va = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(va)),
        ],
    )
    .unwrap();

    let check = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let def = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("fallback")
    };
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(check)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap()
    .unwrap();
    let gs = match got {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(gs, Some("val".to_string()));

    let miss = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("other")
    };
    let def2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("fallback")
    };
    let got2 = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(miss)),
            Value::Object(Some(def2)),
        ],
    )
    .unwrap()
    .unwrap();
    let gs2 = match got2 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(gs2, Some("fallback".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_put_if_absent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let va = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("first")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(va)),
        ],
    )
    .unwrap();

    let ka2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let vb = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("second")
    };
    let existing = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka2)),
            Value::Object(Some(vb)),
        ],
    )
    .unwrap()
    .unwrap();
    let es = match existing {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(es, Some("first".to_string()));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "size",
        "()I",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_map_clear_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "<init>",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let ka = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("x")
    };
    let va = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("y")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(tm)),
            Value::Object(Some(ka)),
            Value::Object(Some(va)),
        ],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(0));

    call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "clear",
        "()V",
        &[Value::Object(Some(tm))],
    )
    .unwrap();

    let empty2 = call_native(
        &shared,
        &mut thread,
        "java/util/TreeMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(tm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn tree_set_clear_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ts = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "<init>",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ts)), Value::Object(Some(s))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "clear",
        "()V",
        &[Value::Object(Some(ts))],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/TreeSet",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(ts))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
}

// ---------------------------------------------------------------
// Phase 19: ByteArrayInputStream, ByteArrayOutputStream, Scanner
// ---------------------------------------------------------------

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn bais_read_single_bytes() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create byte[] with values [65, 66, 67] = A, B, C
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        3,
    );
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(65));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(66));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(67));

    // BAIS requires 4 slots: buf/pos/mark/count (Session 83 layout).
    let bais = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayInputStream", 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(arr))],
    )
    .unwrap();

    // Read 3 bytes
    let b1 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b1, Value::Int(65));

    let b2 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b2, Value::Int(66));

    let b3 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b3, Value::Int(67));

    // EOF
    let eof = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eof, Value::Int(-1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn bais_available_and_reset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        5,
    );
    for i in 0..5 {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(i as i32));
    }

    // BAIS requires 4 slots: buf/pos/mark/count (Session 83 layout).
    let bais = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayInputStream", 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(arr))],
    )
    .unwrap();

    let avail = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "available",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avail, Value::Int(5));

    // Read 2 bytes
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap();

    let avail2 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "available",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avail2, Value::Int(3));

    // Reset
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "reset",
        "()V",
        &[Value::Object(Some(bais))],
    )
    .unwrap();

    let avail3 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "available",
        "()I",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avail3, Value::Int(5));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn bais_bulk_read() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        4,
    );
    for i in 0..4u8 {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i as usize, Value::Int(10 + i as i32));
    }

    // BAIS requires 4 slots: buf/pos/mark/count (Session 83 layout).
    let bais = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayInputStream", 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(arr))],
    )
    .unwrap();

    let buf = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        10,
    );
    let count = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "read",
        "([BII)I",
        &[
            Value::Object(Some(bais)),
            Value::Object(Some(buf)),
            Value::Int(0),
            Value::Int(10),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(4));
    assert_eq!(
        shared.mem.heap.get_array_element(buf, 0).unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(buf, 3).unwrap(),
        Value::Int(13)
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn baos_write_and_to_byte_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let baos = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    // Write 3 bytes
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "write",
        "(I)V",
        &[Value::Object(Some(baos)), Value::Int(72)],
    )
    .unwrap(); // H
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "write",
        "(I)V",
        &[Value::Object(Some(baos)), Value::Int(105)],
    )
    .unwrap(); // i
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "write",
        "(I)V",
        &[Value::Object(Some(baos)), Value::Int(33)],
    )
    .unwrap(); // !

    let size = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    let result = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let result_arr = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(result_arr), 3);
    assert_eq!(
        shared.mem.heap.get_array_element(result_arr, 0).unwrap(),
        Value::Int(72)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(result_arr, 1).unwrap(),
        Value::Int(105)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(result_arr, 2).unwrap(),
        Value::Int(33)
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn baos_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let baos = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    for &b in b"Hello" {
        call_native(
            &shared,
            &mut thread,
            "java/io/ByteArrayOutputStream",
            "write",
            "(I)V",
            &[Value::Object(Some(baos)), Value::Int(b as i32)],
        )
        .unwrap();
    }

    let s = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let text = match s {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(text, Some("Hello".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn baos_reset_and_grow() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let baos = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "(I)V",
        &[Value::Object(Some(baos)), Value::Int(4)],
    )
    .unwrap();

    // Write 6 bytes (exceeds initial capacity of 4)
    for i in 0..6 {
        call_native(
            &shared,
            &mut thread,
            "java/io/ByteArrayOutputStream",
            "write",
            "(I)V",
            &[Value::Object(Some(baos)), Value::Int(i)],
        )
        .unwrap();
    }
    let size = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(6));

    // Reset
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "reset",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();
    let size2 = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size2, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_next_has_next() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello world  foo")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    // hasNext
    let hn = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNext",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hn, Value::Int(1));

    // next -> "hello"
    let t1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s1 = match t1 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s1, Some("hello".to_string()));

    // next -> "world"
    let t2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s2 = match t2 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s2, Some("world".to_string()));

    // next -> "foo"
    let t3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s3 = match t3 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s3, Some("foo".to_string()));

    // hasNext -> false
    let hn2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNext",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hn2, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_next_int_long_double() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("42 1000000000000 2.75")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(42));

    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLong",
        "()J",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Long(1_000_000_000_000));

    let v3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextDouble",
        "()D",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v3, Value::Double(2.75));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_next_line() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("line one\nline two\nline three")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let l1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLine",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s1 = match l1 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s1, Some("line one".to_string()));

    let l2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLine",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s2 = match l2 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s2, Some("line two".to_string()));

    let hl = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextLine",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hl, Value::Int(1));

    let l3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLine",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s3 = match l3 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s3, Some("line three".to_string()));

    let hl2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextLine",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hl2, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_custom_delimiter() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a,b,,c")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(",+")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "useDelimiter",
        "(Ljava/lang/String;)Ljava/util/Scanner;",
        &[Value::Object(Some(sc)), Value::Object(Some(delim))],
    )
    .unwrap();

    let t1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s1 = match t1 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s1, Some("a".to_string()));

    let t2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s2 = match t2 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s2, Some("b".to_string()));

    let t3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s3 = match t3 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s3, Some("c".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_has_next_int_double() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("42 hello 3.14")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let hi = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextInt",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hi, Value::Int(1));

    // Consume 42
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap();

    // "hello" is not int
    let hi2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextInt",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hi2, Value::Int(0));

    // But has next double? No — "hello" is not a double either
    let hd = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextDouble",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hd, Value::Int(0));

    // Skip "hello"
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap();

    // Now "3.14" — hasNextDouble true
    let hd2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextDouble",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hd2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_next_boolean() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("true false TRUE")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let b1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextBoolean",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b1, Value::Int(1));

    let b2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextBoolean",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b2, Value::Int(0));

    let b3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextBoolean",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b3, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_empty_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let hn = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNext",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hn, Value::Int(0));

    let hl = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNextLine",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hl, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_mixed_tokens() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("name 25 3.5 true")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    // Read string
    let t1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    let s1 = match t1 {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(s1, Some("name".to_string()));

    // Read int
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(25));

    // Read float
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextFloat",
        "()F",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Float(3.5));

    // Read boolean
    let v3 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextBoolean",
        "()Z",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v3, Value::Int(1));
}

