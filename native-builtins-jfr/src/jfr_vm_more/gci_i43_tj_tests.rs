use super::{
    gc_command_line_flags, hotspot_gc_time_ratio, hotspot_size_value,
    hotspot_young_generation_configuration, GcCommandLineFlags,
};

const M: i64 = 1024 * 1024;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn gci_i43_tj_size_values_parse_as_hotspots_atojulong() {
    assert_eq!(hotspot_size_value("64m"), Some(64 << 20));
    assert_eq!(hotspot_size_value("64M"), Some(64 << 20));
    assert_eq!(hotspot_size_value("1G"), Some(1 << 30));
    assert_eq!(hotspot_size_value("2k"), Some(2048));
    assert_eq!(hotspot_size_value("5"), Some(5));
    assert_eq!(hotspot_size_value("0x10"), Some(16));
    assert_eq!(hotspot_size_value("18446744073709551615"), Some(u64::MAX));
    assert_eq!(hotspot_size_value("20000000t"), None, "overflow");
    assert_eq!(hotspot_size_value(""), None);
    assert_eq!(hotspot_size_value("m"), None);
    assert_eq!(hotspot_size_value("12x"), None);
    assert_eq!(hotspot_size_value("12mb"), None);
    assert_eq!(hotspot_size_value("+5"), None);
}

/// Last occurrence wins; `-Xmn` sets both young bounds at its position;
/// text that does not parse leaves the flag at its default.
#[test]
fn gci_i43_tj_command_line_flags_are_read_in_order() {
    let f = gc_command_line_flags(&args(&[
        "-Xmx256m",
        "-Xmn32m",
        "-XX:NewSize=16m",
        "-XX:MaxTenuringThreshold=5",
        "-XX:+UseG1GC",
        "-XX:GCTimeRatio=bad",
        "-XX:NewRatio=3",
        "-Xmn",
        "-Dx=-XX:InitialTenuringThreshold=3",
    ]));
    assert_eq!(
        f,
        GcCommandLineFlags {
            new_size: Some(16 << 20),
            max_new_size: Some(32 << 20),
            new_ratio: Some(3),
            max_tenuring_threshold: Some(5),
            initial_tenuring_threshold: None,
            gc_time_ratio: None,
            g1_heap_region_size: None,
        }
    );
    assert_eq!(gc_command_line_flags(&[]), GcCommandLineFlags::default());
}

/// HotSpot Serial's `GenArguments` arithmetic: `-Xms256m -Xmx256m` fixes
/// the young generation at `align_down(256M / 3, 64K)` = 89 456 640 (the
/// i42/mx page's figure); a smaller `-Xms` starts it at a third of that;
/// `-Xmn` and `NewRatio` move both.
#[test]
fn gci_i43_tj_serial_young_configuration_is_genarguments() {
    let none = GcCommandLineFlags::default();
    let fixed = [256 * M, 256 * M, 256 * M];
    assert_eq!(
        hotspot_young_generation_configuration("SerialOld", fixed, &none),
        Some((89_456_640, 89_456_640, 2))
    );
    assert_eq!(
        hotspot_young_generation_configuration("SerialOld", [8 * M, 64 * M, 256 * M], &none),
        Some((22_347_776, 89_456_640, 2))
    );
    let xmn = gc_command_line_flags(&args(&["-Xmn64m"]));
    assert_eq!(
        hotspot_young_generation_configuration("SerialOld", fixed, &xmn),
        Some((64 * M, 64 * M, 2))
    );
    let ratio = gc_command_line_flags(&args(&["-XX:NewRatio=3"]));
    assert_eq!(
        hotspot_young_generation_configuration("SerialOld", fixed, &ratio),
        Some((64 * M, 64 * M, 3))
    );
    // A `MaxNewSize` at or above the heap leaves room for one granule of
    // old generation.
    let huge = gc_command_line_flags(&args(&["-Xmn256m"]));
    assert_eq!(
        hotspot_young_generation_configuration("SerialOld", fixed, &huge),
        Some((256 * M - 65_536, 256 * M - 65_536, 2))
    );
}

/// HotSpot G1 writes `NewSize` as given (default
/// `ScaleForWordSize(1M)`) and the `MaxNewSize` `G1YoungGenSizer` set
/// (at the defaults 60 % of the regions: 153 MiB at `-Xmx256m`, HotSpot
/// 25.0.3 row `tj43_c_g1`; never `Long.MIN_VALUE`), a command-line
/// `MaxNewSize` raised to a larger `NewSize`. ZGC keeps the caller's row.
#[test]
fn gci_i43_tj_g1_young_configuration_is_the_flags() {
    let heap = [256 * M, 256 * M, 256 * M];
    assert_eq!(
        hotspot_young_generation_configuration("G1Old", heap, &GcCommandLineFlags::default()),
        Some((1_363_144, 160_432_128, 2))
    );
    let xmn = gc_command_line_flags(&args(&["-Xmn64m"]));
    assert_eq!(
        hotspot_young_generation_configuration("G1Old", heap, &xmn),
        Some((64 * M, 64 * M, 2))
    );
    let crossed = gc_command_line_flags(&args(&["-XX:NewSize=32m", "-XX:MaxNewSize=16m"]));
    assert_eq!(
        hotspot_young_generation_configuration("G1Old", heap, &crossed),
        Some((32 * M, 32 * M, 2))
    );
    assert_eq!(
        hotspot_young_generation_configuration("ZGC Major", heap, &xmn),
        None
    );
}

/// gci i47w/j43: G1's `maxSize` is `G1YoungGenSizer`'s ergonomic
/// `MaxNewSize` (`adjust_max_new_size`) for every sizer kind, a whole
/// number of regions; HotSpot 25.0.3 answered 160 432 128 at
/// `-XX:+UseG1GC -Xms256m -Xmx256m` where this wrote `Long.MIN_VALUE`.
#[test]
fn gci_i47w_j43_g1_young_max_is_the_sizers_max_new_size() {
    use super::hotspot_g1_max_new_size as max_new;
    const U: u64 = 1024 * 1024;
    let f = |list: &[&str]| gc_command_line_flags(&args(list));
    // Defaults: 60 % of 256 one-MiB regions, rounded down.
    assert_eq!(max_new(256 * U, 1_363_144, &f(&[])), 153 * U);
    // A -Xmx that is not a multiple of the 2 MiB heap alignment.
    assert_eq!(max_new(255 * U, 1_363_144, &f(&[])), 153 * U);
    // NewRatio alone fixes the length: 256 / 4.
    assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:NewRatio=3"])), 64 * U);
    // NewSize with NewRatio: NewSize wins (the sizer's warning case);
    // NewSize alone keeps the 60 % maximum unless it is larger.
    let ns16 = f(&["-XX:NewRatio=3", "-XX:NewSize=16m"]);
    assert_eq!(max_new(256 * U, 16 * U, &ns16), 153 * U);
    assert_eq!(max_new(256 * U, 200 * U, &f(&["-XX:NewSize=200m"])), 200 * U);
    // A command-line MaxNewSize is whole regions, at least one, and
    // raised to a larger (even default) NewSize first.
    assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:MaxNewSize=35127296"])), 33 * U);
    assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:MaxNewSize=1m"])), U);
    let gt = f(&["-XX:NewSize=64m", "-XX:MaxNewSize=32m"]);
    assert_eq!(max_new(256 * U, 64 * U, &gt), 64 * U);
    // Region size: clamp(MaxHeapSize / 2048, 1M, 32M), or the flag.
    assert_eq!(max_new(1024 * U, 1_363_144, &f(&[])), 614 * U);
    assert_eq!(max_new(16384 * U, 1_363_144, &f(&[])), 1228 * 8 * U);
    let r4 = f(&["-XX:G1HeapRegionSize=4m"]);
    assert_eq!(max_new(256 * U, 1_363_144, &r4), 38 * 4 * U);
    // Through the event's answer.
    assert_eq!(
        hotspot_young_generation_configuration("G1Old", [256 * M, 256 * M, 256 * M], &ns16),
        Some((16 * M, 153 * M, 3))
    );
}

/// `GCTimeRatio`: Serial keeps the flag (99 by default); G1 replaces a
/// default or zero value by 12 (`G1Arguments::initialize`).
#[test]
fn gci_i43_tj_gc_time_ratio_is_hotspots_per_collector() {
    assert_eq!(hotspot_gc_time_ratio("SerialOld", None), Some(99));
    assert_eq!(hotspot_gc_time_ratio("SerialOld", Some(4)), Some(4));
    assert_eq!(hotspot_gc_time_ratio("G1Old", None), Some(12));
    assert_eq!(hotspot_gc_time_ratio("G1Old", Some(0)), Some(12));
    assert_eq!(hotspot_gc_time_ratio("G1Old", Some(19)), Some(19));
    assert_eq!(hotspot_gc_time_ratio("ZGC Major", None), None);
}

/// The periodic writer reads the switch and the new answers at each of
/// the four events it changes.
#[test]
fn gci_i43_tj_the_periodic_writer_uses_the_flag_answers() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src.find("\npub fn periodic(").expect("periodic");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.matches("true").count() >= 4);
    assert!(body.contains("hotspot_gc_time_ratio(old, flags.gc_time_ratio)"));
    assert!(body.contains("(\"gcTimeRatio\", Val::Int(gc_time_ratio))"));
    assert!(body.contains("ctx.hotspot_tlab_configuration()"));
    assert!(body.contains("flags.max_tenuring_threshold.map_or(15, as_int)"));
    assert!(body.contains("flags.initial_tenuring_threshold.map_or(7, as_int)"));
    assert!(body.contains(
        "hotspot_young_generation_configuration(old, ctx.hotspot_heap_sizes(), &flags)"
    ));
    assert!(body.contains("resolved_flag_origin("));
}

#[test]
fn periodic_origin_uses_resolved_ergo_and_management_before_argument_spelling() {
    use cratonvm_native_api::hotspot_flags::HotspotVmFlag;
    let mut row = HotspotVmFlag { ty: "uint", name: "ConcGCThreads", value: "2".into(),
        kind: "{product}", origin: "{command line, ergonomic}" };
    let command = args(&["-XX:ConcGCThreads=0"]);
    assert_eq!(super::resolved_flag_origin(row.name, &[row.clone()], None,
        Some(&command), "G1Old", 256 * M), "Ergonomic");
    row.name = "MinHeapFreeRatio";
    row.ty = "uintx";
    row.kind = "{manageable}";
    row.value = "45".into();
    row.origin = "{management}";
    assert_eq!(super::resolved_flag_origin(row.name, &[row.clone()], Some("Default"),
        Some(&command), "G1Old", 256 * M), "Management");
}

#[test]
fn periodic_origin_preserves_the_compression_model_and_legacy_provider_fallback() {
    let command = args(&["-XX:-UseTLAB"]);
    assert_eq!(super::resolved_flag_origin("UseCompressedOops", &[], Some("Command line"),
        None, "SerialOld", 256 * M), "Command line");
    assert_eq!(super::resolved_flag_origin("UseTLAB", &[], Some("Default"),
        Some(&command), "SerialOld", 256 * M), "Command line");
    assert_eq!(super::resolved_flag_origin("UseTLAB", &[], None,
        None, "SerialOld", 256 * M), "Ergonomic");
}

/// Item 8's `origin`: `Command line` for what the options set,
/// `Ergonomic` for what HotSpot's argument processing sets, `Default`
/// otherwise.
#[test]
fn gci_i43_tj_flag_origins_are_hotspots() {
    use super::hotspot_flag_origin as origin;
    let g1 = args(&["-XX:+UseG1GC", "-Xms256m", "-Xmx256m", "-XX:-UseCompressedOops"]);
    assert_eq!(origin("UseG1GC", &g1, "G1Old", 256 * M), "Command line");
    assert_eq!(origin("UseSerialGC", &g1, "G1Old", 256 * M), "Default");
    assert_eq!(origin("MaxHeapSize", &g1, "G1Old", 256 * M), "Command line");
    assert_eq!(origin("InitialHeapSize", &g1, "G1Old", 256 * M), "Command line");
    assert_eq!(origin("UseCompressedOops", &g1, "G1Old", 256 * M), "Command line");
    // gci i47w/j43: `FLAG_SET_DEFAULT` in `G1Arguments::initialize`
    // (HotSpot 25.0.3 row `tj43_c_g1`).
    assert_eq!(origin("ParallelGCThreads", &g1, "G1Old", 256 * M), "Default");
    assert_eq!(origin("CICompilerCount", &g1, "G1Old", 256 * M), "Ergonomic");
    assert_eq!(origin("UseTLAB", &g1, "G1Old", 256 * M), "Default");
    assert_eq!(origin("MaxJavaStackTraceDepth", &g1, "G1Old", 256 * M), "Default");

    let gen = args(&["-XX:+UseGenerationalGC", "-Xmx64m", "-XX:-UseTLAB"]);
    assert_eq!(origin("UseSerialGC", &gen, "SerialOld", 64 * M), "Command line");
    assert_eq!(origin("InitialHeapSize", &gen, "SerialOld", 64 * M), "Ergonomic");
    assert_eq!(origin("ParallelGCThreads", &gen, "SerialOld", 64 * M), "Default");
    assert_eq!(origin("UseTLAB", &gen, "SerialOld", 64 * M), "Command line");
    assert_eq!(origin("UseCompressedOops", &gen, "SerialOld", 64 * M), "Ergonomic");
    assert_eq!(origin("UseCompressedOops", &gen, "SerialOld", 32 << 30), "Default");
    // Wave 137 L1b: `CICompilerCount`'s tiered ergonomics (MEASURED 15
    // on 32 CPUs), and a given value wins.
    use super::hotspot_ci_compiler_count as ci;
    assert_eq!(ci(&[], 1), 2);
    assert_eq!(ci(&[], 4), 3);
    assert_eq!(ci(&[], 8), 4);
    assert_eq!(ci(&[], 16), 12);
    assert_eq!(ci(&[], 32), 15);
    assert_eq!(ci(&[], 64), 18);
    assert_eq!(ci(&args(&["-XX:CICompilerCount=3", "-XX:CICompilerCount=6"]), 32), 6);
    assert_eq!(ci(&args(&["-XX:CICompilerCount=6", "-Xint"]), 32), 0);
    assert_eq!(ci(&args(&["-Xint", "-Xmixed"]), 32), 15);
    let xint = args(&["-XX:CICompilerCount=6", "-Xint"]);
    assert_eq!(origin("CICompilerCount", &xint, "G1Old", 256 * M), "Ergonomic");
    // Wave 137 L1b: ZGC's `FLAG_SET_ERGO(UseCompressedOops, false)`.
    let z = args(&["-XX:+UseZGC", "-Xmx256m"]);
    assert_eq!(origin("UseCompressedOops", &z, "ZGC Major", 256 * M), "Ergonomic");
    assert_eq!(origin("UseCompressedOops", &z, "ZGC Major", 64 << 30), "Ergonomic");

    // Nothing given: the selected collector is ergonomic; a flag whose
    // name only starts like another's is not it.
    let none = args(&["-XX:UseG1GCFoo=1"]);
    assert_eq!(origin("UseSerialGC", &none, "SerialOld", 64 * M), "Ergonomic");
    assert_eq!(origin("UseG1GC", &none, "SerialOld", 64 * M), "Default");
    assert_eq!(origin("MaxHeapSize", &none, "SerialOld", 64 * M), "Ergonomic");
}
