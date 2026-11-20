use super::{resolved_flag_event_value, resolved_uint_flag_value, Val};
use cratonvm_native_api::hotspot_flags::HotspotVmFlag;

#[test]
fn jfr_uses_the_resolved_value_and_type_including_unsigned_bits() {
    let mut flag = HotspotVmFlag { ty: "int", name: "MaxJavaStackTraceDepth",
        value: "23".into(), kind: "{product}", origin: "{command line}" };
    assert!(matches!(resolved_flag_event_value("jdk.IntFlag", &flag), Some(Val::Int(23))));
    assert!(resolved_flag_event_value("jdk.LongFlag", &flag).is_none());
    flag.ty = "uint"; flag.value = u32::MAX.to_string();
    assert!(matches!(resolved_flag_event_value("jdk.UnsignedIntFlag", &flag), Some(Val::Int(-1))));
    flag.ty = "size_t"; flag.value = u64::MAX.to_string();
    assert!(matches!(resolved_flag_event_value("jdk.UnsignedLongFlag", &flag), Some(Val::Long(-1))));
    flag.ty = "bool"; flag.value = "false".into();
    assert!(matches!(resolved_flag_event_value("jdk.BooleanFlag", &flag), Some(Val::Bool(false))));
    flag.value = "invalid".into();
    assert!(resolved_flag_event_value("jdk.BooleanFlag", &flag).is_none());
}
#[test]
fn resolved_string_null_and_double_encodings_match_hotspot() {
    let mut row = HotspotVmFlag { ty: "ccstr", name: "HeapDumpPath", value: String::new(),
        kind: "{manageable}", origin: "{default}" };
    assert!(matches!(resolved_flag_event_value("jdk.StringFlag", &row), Some(Val::Str(None))));
    row.origin = "{command line}";
    assert!(matches!(resolved_flag_event_value("jdk.StringFlag", &row), Some(Val::Str(None))));
    row.value = "gcj output path".into();
    assert!(matches!(resolved_flag_event_value("jdk.StringFlag", &row),
        Some(Val::Str(Some(value))) if value == "gcj output path"));
    assert!(resolved_flag_event_value("jdk.DoubleFlag", &row).is_none());
    row.ty = "double"; row.value = "1.500000".into();
    assert!(matches!(resolved_flag_event_value("jdk.DoubleFlag", &row), Some(Val::Double(value)) if value == 1.5));
    row.ty = "uint64_t"; row.value = u64::MAX.to_string();
    assert!(matches!(resolved_flag_event_value("jdk.UnsignedLongFlag", &row), Some(Val::Long(-1))));
}
#[test]
fn configuration_fields_use_the_resolved_collector_and_command_line_values() {
    let row = |name, value: u32| HotspotVmFlag { ty: "uint", name,
        value: value.to_string(), kind: "{product}", origin: "{default}" };
    let zgc = [row("MaxTenuringThreshold", 6), row("InitialTenuringThreshold", 7),
        row("ParallelGCThreads", 2), row("ConcGCThreads", 2)];
    assert_eq!(resolved_uint_flag_value(&zgc, "MaxTenuringThreshold", 15), 6);
    assert_eq!(resolved_uint_flag_value(&zgc, "ConcGCThreads", 8), 2);
    let explicit = [row("MaxTenuringThreshold", 4), row("InitialTenuringThreshold", 4),
        row("ParallelGCThreads", 3), row("ConcGCThreads", 1)];
    assert_eq!(resolved_uint_flag_value(&explicit, "InitialTenuringThreshold", 7), 4);
    assert_eq!(resolved_uint_flag_value(&explicit, "ParallelGCThreads", 23), 3);
    assert_eq!(resolved_uint_flag_value(&[], "MaxTenuringThreshold", 15), 15);
    let unsigned = [row("ParallelGCThreads", u32::MAX)];
    assert_eq!(resolved_uint_flag_value(&unsigned, "ParallelGCThreads", 0), -1);
}

