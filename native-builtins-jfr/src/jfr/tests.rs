use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn registers_the_recordingstream_bootstrap_boundary() {
    let mut registry = NativeMethodRegistry::new();
    register_jfr_natives(&mut registry);
    assert!(registry
        .find("jdk/jfr/internal/JVM", "isAvailable", "()Z")
        .is_some());
    assert!(registry
        .find("jdk/jfr/internal/JVM", "createJFR", "(Z)Z")
        .is_some());
    assert!(registry
        .find("jdk/jfr/internal/JVM", "beginRecording", "()V")
        .is_some());
    assert!(registry
        .find("jdk/jfr/internal/JDKEvents", "initialize", "()V")
        .is_none());
    // The streaming surface is a SET: a stream that starts a recording but
    // never ends it leaves `Event.isEnabled()` true for the rest of the
    // process, and a stream whose consumers are never captured delivers
    // nothing however well the recording state behaves. Each of these had
    // to be added together for `JfrEventsTest` to pass, so each is pinned.
    for (name, descriptor) in [
        ("startAsync", "()V"),
        ("start", "()V"),
        ("close", "()V"),
        (
            "onEvent",
            "(Ljava/lang/String;Ljava/util/function/Consumer;)V",
        ),
        ("onEvent", "(Ljava/util/function/Consumer;)V"),
    ] {
        assert!(
            registry
                .find("jdk/jfr/consumer/RecordingStream", name, descriptor)
                .is_none(),
            "jdk/jfr/consumer/RecordingStream.{name}{descriptor} must run as the JDK's own bytecode (SBR5 jfr)"
        );
    }
    // `begin`/`end` carry the event's start time and duration, which no
    // field on the object can hold because CratonVM does not weave event
    // classes. As no-ops every event was an instant event at commit time.
    for (name, descriptor) in [("begin", "()V"), ("end", "()V"), ("commit", "()V")] {
        assert!(
            registry.find("jdk/jfr/Event", name, descriptor).is_none(),
            "jdk/jfr/Event.{name}{descriptor} must run as the JDK's own bytecode (SBR5 jfr)"
        );
    }
    // The thread filter and the event-configuration round trip are
    // stateful: the guard/reader must exist alongside its mutator, or the
    // pair silently disagrees again (wave-2 stub removal).
    for (name, descriptor) in [
        ("exclude", "(Ljava/lang/Thread;)V"),
        ("include", "(Ljava/lang/Thread;)V"),
        ("isExcluded", "(Ljava/lang/Thread;)Z"),
        (
            "setConfiguration",
            "(Ljava/lang/Class;Ljdk/jfr/internal/event/EventConfiguration;)Z",
        ),
        ("getConfiguration", "(Ljava/lang/Class;)Ljava/lang/Object;"),
        ("getThreadId", "(Ljava/lang/Thread;)J"),
    ] {
        assert!(
            registry
                .find("jdk/jfr/internal/JVM", name, descriptor)
                .is_some(),
            "jdk/jfr/internal/JVM.{name}{descriptor} must stay registered"
        );
    }
}

#[test]
fn jfr_type_ids_are_stable_and_distinct() {
    let string = type_id("java.lang.String");
    assert_eq!(string, type_id("java.lang.String"));
    assert_ne!(string, type_id("java.lang.Thread"));
    assert_ne!(string, 0);
}

/// Every id this bridge hands out must clear
/// `JVM.RESERVED_CLASS_ID_LIMIT` (500). Below it the JDK's
/// `TypeLibrary.types` map — keyed by id — already holds the ~200 built-in
/// types whose ids are baked into `metadata.bin`, so a low id makes
/// `EventType.getEventType(myEventClass)` answer with somebody else's event
/// type instead of failing.
#[test]
fn jfr_type_ids_clear_the_jdks_reserved_class_id_range() {
    assert_eq!(FIRST_JFR_TYPE_ID, 500);
    for name in [
        "com.example.First",
        "com.example.Second",
        "com.example.Third",
    ] {
        assert!(
            type_id(name) >= FIRST_JFR_TYPE_ID,
            "{name} was assigned an id inside the JDK's reserved range"
        );
    }
}

/// The descriptor set the payload capture admits has to be exactly the set
/// the chunk writer can declare in metadata — a field written into a
/// payload but absent from the metadata mis-frames every field after it.
#[test]
fn capturable_field_descriptors_match_the_writable_jfr_types() {
    for descriptor in ["Z", "B", "C", "S", "I", "J", "F", "D", "Ljava/lang/String;"] {
        assert!(
            JFR_FIELD_DESCRIPTORS.contains(&descriptor),
            "{descriptor} must be capturable"
        );
    }
    for descriptor in [
        "Ljava/lang/Class;",
        "Ljava/lang/Object;",
        "[I",
        "[Ljava/lang/String;",
    ] {
        assert!(
            !JFR_FIELD_DESCRIPTORS.contains(&descriptor),
            "{descriptor} has no JFR field type this writer can declare and must be skipped"
        );
    }
}

/// `JVMSupport.nanosToTicks` is `(long)(nanos * getTimeConversionFactor())`
/// and `counterTime()` counts nanoseconds, so the factor and the advertised
/// tick frequency have to stay in step with `counter_time`'s unit.
#[test]
fn tick_frequency_and_conversion_factor_agree() {
    assert_eq!(TICKS_PER_SECOND, NANOS_PER_SECOND);
    assert_eq!(TICKS_PER_SECOND as f64 / NANOS_PER_SECOND as f64, 1.0);
}

/// The host really can answer `hostTotalMemory`; on the two platforms with
/// a probe it must not fall back to the `0` "unknown" sentinel.
#[test]
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn host_memory_totals_are_probed_not_zero() {
    let (total, swap) = host_memory_totals().expect("host RAM probe");
    assert!(total > 0, "hostTotalMemory must be a real total");
    assert!(swap >= 0, "hostTotalSwapMemory must not be negative");
}

/// `withoutThreshold()` writes `0 ns`, `withThreshold(Duration)` writes the
/// unit-suffixed form. A value this parser cannot read must answer `None`
/// rather than a guess: a wrong threshold silently drops events.
#[test]
fn jfr_timespan_settings_parse_to_nanoseconds() {
    assert_eq!(parse_jfr_timespan_nanos("0 ns"), Some(0));
    assert_eq!(parse_jfr_timespan_nanos("20 ms"), Some(20_000_000));
    assert_eq!(parse_jfr_timespan_nanos("20ms"), Some(20_000_000));
    assert_eq!(parse_jfr_timespan_nanos("1 s"), Some(1_000_000_000));
    assert_eq!(parse_jfr_timespan_nanos("500 us"), Some(500_000));
    assert_eq!(parse_jfr_timespan_nanos("2 m"), Some(120_000_000_000));
    assert_eq!(parse_jfr_timespan_nanos("1 h"), Some(3_600_000_000_000));
    assert_eq!(parse_jfr_timespan_nanos("1 d"), Some(86_400_000_000_000));
    // A bare number is nanoseconds, which is what the JDK's own parser does.
    assert_eq!(parse_jfr_timespan_nanos("750"), Some(750));
    assert_eq!(parse_jfr_timespan_nanos("infinity"), Some(u64::MAX));
    assert_eq!(parse_jfr_timespan_nanos("everything"), None);
    assert_eq!(parse_jfr_timespan_nanos("20 fortnights"), None);
    assert_eq!(parse_jfr_timespan_nanos(""), None);
}

#[test]
fn primitive_class_descriptors_match_openjdk_jfr_type_names() {
    assert_eq!(canonical_jfr_type_name("Z"), "boolean");
    assert_eq!(canonical_jfr_type_name("I"), "int");
    assert_eq!(
        canonical_jfr_type_name("java/lang/String"),
        "java.lang.String"
    );
}
