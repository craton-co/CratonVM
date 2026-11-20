use super::*;

#[cfg(unix)]
#[test]
fn publication_owns_its_inode_and_refuses_unsafe_paths() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = std::env::temp_dir().join(format!(
        "cvm-perf-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let dir = root.join("safe");
    let pid = std::process::id();
    let owned = write_perf_data_in_dir(&dir, pid, b"complete").unwrap();
    assert_eq!(std::fs::read(&owned.path).unwrap(), b"complete");
    assert_eq!(
        std::fs::metadata(&owned.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(write_perf_data_in_dir(&dir, pid, b"other VM").is_none());
    // Replacement is a different lease: old teardown cannot remove it.
    std::fs::rename(&owned.path, dir.join("old")).unwrap();
    std::fs::write(&owned.path, b"replacement").unwrap();
    owned.remove();
    assert_eq!(std::fs::read(&owned.path).unwrap(), b"replacement");
    let unsafe_dir = root.join("writable");
    std::fs::create_dir(&unsafe_dir).unwrap();
    std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(write_perf_data_in_dir(&unsafe_dir, pid, b"unsafe").is_none());
    let alias = root.join("alias");
    symlink(&dir, &alias).unwrap();
    assert!(write_perf_data_in_dir(&alias, pid, b"unsafe").is_none());
    std::fs::remove_file(&owned.path).unwrap();
    let victim = root.join("victim");
    std::fs::write(&victim, b"preserve").unwrap();
    symlink(&victim, &owned.path).unwrap();
    assert!(write_perf_data_in_dir(&dir, pid, b"unsafe").is_none());
    assert_eq!(std::fs::read(&victim).unwrap(), b"preserve");
    std::fs::remove_file(&owned.path).unwrap();
    let final_lease = write_perf_data_in_dir(&dir, pid, b"final").unwrap();
    final_lease.remove();
    assert!(!final_lease.path.exists());
    #[cfg(target_os = "linux")]
    {
        std::fs::write(&final_lease.path, b"stale reused PID").unwrap();
        let old = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        std::fs::File::open(&final_lease.path)
            .unwrap()
            .set_times(old)
            .unwrap();
        let replacement = write_perf_data_in_dir(&dir, pid, b"reused PID").unwrap();
        assert_eq!(std::fs::read(&replacement.path).unwrap(), b"reused PID");
        // An old timestamp alone never steals a live embedded VM's lease.
        std::fs::File::open(&replacement.path)
            .unwrap()
            .set_times(old)
            .unwrap();
        assert!(write_perf_data_in_dir(&dir, pid, b"steal").is_none());
        replacement.remove();
    }
    std::fs::remove_dir_all(&root).unwrap();
}

/// Wave 84 (lane L1): a string constant longer than
/// `PerfMaxStringConstLength` is cut at 1024 bytes and still ends in a
/// NUL; `java.rt.vmArgs` is the launcher's option list when it gave one.
#[test]
fn long_string_constants_are_cut_as_hotspots_and_vm_args_are_the_options() {
    let long = "x".repeat(3000);
    let entry = PerfEntry::string_constant("java.rt.vmArgs", &long, true);
    assert_eq!(
        entry.value,
        PerfValue::Str {
            text: long.clone(),
            capacity: PERF_MAX_STRING_CONST_LENGTH + 1
        }
    );
    let bytes = encode_entry(&entry);
    let data_at = i32::from_ne_bytes(bytes[16..20].try_into().unwrap()) as usize;
    assert_eq!(bytes[data_at + 1023], b'x');
    assert_eq!(bytes[data_at + 1024], 0);

    let mut config = crate::config::VmConfig::default();
    config.system_properties = vec![("x".to_string(), "y".to_string())];
    assert_eq!(vm_args_text(&config), "-Dx=y");
    config.jvm_args = vec!["-Xmx64m".to_string(), "-Dx=y".to_string()];
    assert_eq!(vm_args_text(&config), "-Xmx64m -Dx=y");
}


/// zt t30/cv (`zgc-zt12pl4-*` item 3): `sun.threads.cpu_time.vm` is the
/// barrier's VM-thread CPU in nanoseconds, unscaled (HotSpot's
/// `CPUTimeCounters` write `os::thread_cpu_time`), whatever the tick
/// frequency; `sun.threads.total_gc_cpu_time` stays 0 (HotSpot's Linux
/// answer too).
#[test]
fn zt30_cv_cpu_time_vm_is_the_vm_thread_cpu_in_nanoseconds() {
    let runtime = RuntimeCounters {
        vm_thread_cpu_ns: 12_345_678,
        ..RuntimeCounters::default()
    };
    let values = live_values(
        0,
        &HeapCounters::default(),
        &ClassCounters::default(),
        &runtime,
    );
    let at = |name: &str| LIVE_COUNTERS.iter().position(|c| c.0 == name).unwrap();
    assert_eq!(at("sun.threads.cpu_time.vm"), LIVE_VM_THREAD_CPU);
    assert_eq!(values[LIVE_VM_THREAD_CPU], 12_345_678);
    assert_eq!(values[at("sun.threads.total_gc_cpu_time")], 0);
    assert_eq!(values[LIVE_VM_OPERATION_TIME], 0);
}

/// zt t9/pl: the `sun.threads.*` / `sun.rt.*` counters carry HotSpot's
/// kinds (`PerfDataManager::create_counter`: `sun.*` unsupported,
/// monotonic, events or ticks), and the whole live set still fits the
/// 32 KiB file behind the identity entries with a long `java.rt.vmArgs`.
#[test]
fn zt9pl_runtime_and_thread_counters_are_hotspot_kinds_and_fit() {
    let kind = |name: &str| *LIVE_COUNTERS.iter().find(|c| c.0 == name).unwrap();
    assert_eq!(
        kind("sun.rt.safepoints"),
        ("sun.rt.safepoints", UNITS_EVENTS, VARIABILITY_MONOTONIC, false)
    );
    for name in [
        "sun.rt.safepointTime",
        "sun.rt.safepointSyncTime",
        "sun.rt.applicationTime",
        "sun.threads.vmOperationTime",
        "sun.threads.total_gc_cpu_time",
        "sun.threads.cpu_time.vm",
    ] {
        assert_eq!(
            kind(name),
            (name, UNITS_TICKS, VARIABILITY_MONOTONIC, false),
            "{name}"
        );
    }
    let long = "x".repeat(3000);
    let mut entries = vm_entries(1, true, &long, &[("java.vm.name", "cratonvm")], &long);
    let first_live = entries.len() + 1;
    entries.extend(live_entries());
    let (_, offsets) = encode_perf_data_with_offsets(&entries, 1);
    assert_eq!(offsets.len() - first_live, LIVE_COUNT);
    assert!(
        offsets[first_live..].iter().all(|&at| at != usize::MAX),
        "a live counter was left out of the file"
    );
}

/// zt t9/pl: a view handed to `Perf.attach(0)` survives the file's
/// removal (a Java buffer still reads it), and one never attached is
/// unmapped as before. Unix: maps a real file under the test's own
/// directory.
#[cfg(unix)]
#[test]
fn zt9pl_an_attached_view_stays_mapped_after_remove() {
    let root = std::env::temp_dir().join(format!(
        "cvm-perf-attach-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let entries = live_entries();
    let (bytes, _) = encode_perf_data_with_offsets(&entries, 1);
    let dir = root.join("dir");
    let file = write_perf_data_in_dir(&dir, std::process::id(), &bytes).unwrap();
    let (base, len) = file.attach_view().expect("a mapped view");
    assert_eq!(len, PERF_DATA_SIZE);
    file.remove();
    assert!(!file.path.exists());
    // SAFETY: the attached view is never unmapped (the point of the test).
    let magic = unsafe { std::slice::from_raw_parts(base as *const u8, 4) };
    assert_eq!(magic, &[0xca, 0xfe, 0xc0, 0xc0]);
    assert!(file.attach_view().is_none(), "a removed file hands out no view");
    drop(file);
    std::fs::remove_dir_all(&root).unwrap();
}

/// zt t10/pl2: the memory image's `long` at `offset`.
fn read_long(base: usize, offset: usize) -> i64 {
    // SAFETY: the test's own attached image, PERF_DATA_SIZE bytes, never
    // freed once attached; `offset` is a live counter's aligned slot.
    unsafe { std::ptr::read_volatile((base + offset) as *const i64) }
}

/// zt t10/pl2 (item 4): `-XX:+PerfDisableSharedMem` with perf data on
/// keeps the image in memory; the last of each `±` pair wins, as the
/// launcher's own fold reads them; no option at all publishes a file.
#[test]
fn zt10pl2_memory_only_image_follows_the_last_flag_of_each_pair() {
    let args = |list: &[&str]| list.iter().map(|a| (*a).to_string()).collect::<Vec<_>>();
    assert!(!perf_memory_without_file(&args(&[])));
    assert!(perf_memory_without_file(&args(&["-XX:+PerfDisableSharedMem"])));
    let no_perf_data = args(&["-XX:+PerfDisableSharedMem", "-XX:-UsePerfData"]);
    assert!(!perf_memory_without_file(&no_perf_data));
    let shared_again = args(&["-XX:+PerfDisableSharedMem", "-XX:-PerfDisableSharedMem"]);
    assert!(!perf_memory_without_file(&shared_again));
    let mut back_on = args(&["-XX:-UsePerfData", "-XX:+UsePerfData"]);
    back_on.push("-XX:+PerfDisableSharedMem".to_string());
    assert!(perf_memory_without_file(&back_on));
}

/// zt t11/pl3 (t11/x8 P4): `Perf.detach` spares this VM's own view,
/// releases a recorded foreign view once, and ignores other addresses.
#[test]
fn zt11pl3_detach_spares_the_own_view_and_releases_a_foreign_one_once() {
    let mut views = vec![(0x1000, 4096), (0x9000, 32768)];
    assert_eq!(take_foreign_view(&mut views, Some(0x5000), 0x5000), None, "own view");
    assert_eq!(views.len(), 2);
    // The own view is spared even if (by mistake) it was recorded.
    let mut own_recorded = vec![(0x5000, 32768)];
    assert_eq!(take_foreign_view(&mut own_recorded, Some(0x5000), 0x5000), None);
    assert_eq!(own_recorded.len(), 1);
    assert_eq!(take_foreign_view(&mut views, Some(0x5000), 0x9000), Some((0x9000, 32768)));
    assert_eq!(take_foreign_view(&mut views, Some(0x5000), 0x9000), None, "once");
    assert_eq!(take_foreign_view(&mut views, None, 0x2000), None, "unknown");
    assert_eq!(views, vec![(0x1000, 4096)]);
    // Live: a memory image's own base is spared.
    let file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let mut none = Vec::new();
    assert_eq!(take_foreign_view(&mut none, file.own_base(), base), None);
    file.remove();
}

/// zt t11/pl3 (t11/x8 P4): Windows' foreign attach maps a VM this user
/// published (here a fake pid, published by this test) read-only and
/// whole, and finds nothing for an id nobody published.
#[cfg(windows)]
#[test]
fn zt11pl3_windows_foreign_attach_maps_a_published_image() {
    let pid = 2_000_000_000u32 + std::process::id() % 100_000_000;
    let (bytes, _) = encode_perf_data_with_offsets(&live_entries(), 1);
    let Some((_path, mapping)) = windows::publish(pid, &bytes) else {
        // An unsafe or unwritable %TEMP%\hsperfdata_<user>: nothing to map.
        // zt t13/x10 (`zgc-zt12x9-*` item 8): say so, and fail where the
        // run demands its end-to-end tests.
        assert!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_REQUIRE_E2E").is_none(),
            "could not publish a perf-data image under %TEMP%"
        );
        eprintln!("SKIP zt11pl3_windows_foreign_attach_maps_a_published_image: no publish");
        return;
    };
    let lvmid = i32::try_from(pid).unwrap();
    let (base, len) = attach_foreign(lvmid).expect("the published image");
    assert_eq!(len, bytes.len());
    // SAFETY: a live read-only view of `len` bytes, released below.
    let seen = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    assert_eq!(seen, &bytes[..]);
    detach_foreign(base, len);
    drop(mapping);
    assert!(attach_foreign(lvmid + 1).is_none(), "nobody published that id");
}

/// zt t11/pl3 (t11/x8 P3): `CRATONVM_PERF_DATA_FILE=0` wins over
/// `-XX:+PerfDisableSharedMem`, as does `CRATONVM_PERF_DATA_IN_MEMORY=0`.
#[test]
fn zt11pl3_perf_data_file_off_means_no_memory_image() {
    assert!(memory_only_image_allowed(true, true));
    assert!(!memory_only_image_allowed(true, false));
    assert!(!memory_only_image_allowed(false, true));
    assert!(!memory_only_image_allowed(false, false));
}

/// zt t10/pl2 (item 4): the memory image is the same image (magic,
/// live offsets), takes live writes, is handed to `Perf.attach(0)`, and
/// stays readable after `remove` once attached; no file is involved.
#[test]
fn zt10pl2_a_memory_image_is_live_attachable_and_outlives_remove() {
    let file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    assert_eq!(file.live_offsets.len(), LIVE_COUNT);
    assert!(file.path.as_os_str().is_empty());
    let mut values = [0i64; LIVE_COUNT];
    values[LIVE_SAFEPOINTS] = 7;
    values[LIVE_EDEN] = 4096;
    file.write_live(&values);
    let (base, len) = file.attach_view().expect("the memory image");
    assert_eq!(len, PERF_DATA_SIZE);
    assert_eq!(file.own_base(), Some(base));
    // SAFETY: the attached image (never freed from here on).
    let magic = unsafe { std::slice::from_raw_parts(base as *const u8, 4) };
    assert_eq!(magic, &[0xca, 0xfe, 0xc0, 0xc0]);
    assert_eq!(read_long(base, file.live_offsets[LIVE_SAFEPOINTS]), 7);
    assert_eq!(read_long(base, file.live_offsets[LIVE_EDEN]), 4096);
    file.remove();
    assert!(file.attach_view().is_none(), "a removed image hands out nothing");
    assert_eq!(read_long(base, file.live_offsets[LIVE_SAFEPOINTS]), 7);
}

/// zt t10/pl2 (item 2): a monotonic counter keeps the larger of the
/// stored and the written value (a stale sampler pass cannot undo an
/// event-time write); a variable one takes the new value; an index the
/// filter refuses is left alone.
#[test]
fn zt10pl2_monotonic_counters_never_go_back_and_the_filter_holds() {
    // zt t11/pl3: the max rule is the event refresh's (`keeps_max`).
    let file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let at = |i: usize| read_long(base, file.live_offsets[i]);
    let mut values = [0i64; LIVE_COUNT];
    values[LIVE_SAFEPOINTS] = 9;
    values[LIVE_COLLECTOR] = 3;
    values[LIVE_EDEN] = 100;
    file.write_live(&values);
    values[LIVE_SAFEPOINTS] = 5;
    values[LIVE_COLLECTOR] = 2;
    values[LIVE_EDEN] = 50;
    file.write_live(&values);
    assert_eq!(at(LIVE_SAFEPOINTS), 9, "monotonic: kept");
    assert_eq!(at(LIVE_COLLECTOR), 3, "monotonic: kept");
    assert_eq!(at(LIVE_EDEN), 50, "variable: replaced");
    values[LIVE_SAFEPOINTS] = 12;
    values[LIVE_EDEN] = 77;
    file.write_live_where(&values, event_refreshed);
    assert_eq!(at(LIVE_SAFEPOINTS), 12);
    assert_eq!(at(LIVE_EDEN), 50, "not an event-time counter");
    file.remove();
}

/// zt t11/pl3 (t11/x8 P1): the max rule covers the event-refreshed
/// monotonic counters only, and only while the event refresh is on; a
/// sampler-only monotonic counter (`sun.cls.loadedBytes`, which an unload
/// lowers) is stored as written.
#[test]
fn zt11pl3_only_event_refreshed_counters_keep_their_max() {
    assert!(keeps_max(LIVE_SAFEPOINTS, true));
    assert!(keeps_max(LIVE_COLLECTOR, true));
    assert!(!keeps_max(LIVE_SAFEPOINTS, false), "=0: the single writer");
    assert!(!keeps_max(LIVE_LOADED_BYTES, true));
    assert!(!keeps_max(LIVE_LOADED, true));
    assert!(!keeps_max(LIVE_EDEN, true));
    assert_eq!(LIVE_COUNTERS[LIVE_LOADED_BYTES].0, "sun.cls.loadedBytes");
    let file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let mut values = [0i64; LIVE_COUNT];
    values[LIVE_LOADED_BYTES] = 900;
    file.write_live(&values);
    values[LIVE_LOADED_BYTES] = 600;
    file.write_live(&values);
    assert_eq!(
        read_long(base, file.live_offsets[LIVE_LOADED_BYTES]),
        600,
        "an unload lowers it"
    );
    file.remove();
}

/// zt t10/pl2 (item 2): the event-time refresh writes exactly the
/// safepoint block, `vmOperationTime` and the collectors.
#[test]
fn zt10pl2_the_event_refresh_covers_the_safepoint_and_collector_counters() {
    let picked: Vec<&str> = (0..LIVE_COUNT)
        .filter(|&i| event_refreshed(i))
        .map(|i| LIVE_COUNTERS[i].0)
        .collect();
    assert_eq!(
        picked,
        [
            "sun.gc.collector.0.invocations",
            "sun.gc.collector.0.time",
            "sun.gc.collector.1.invocations",
            "sun.gc.collector.1.time",
            "sun.gc.collector.2.invocations",
            "sun.gc.collector.2.time",
            "sun.rt.safepoints",
            "sun.rt.safepointTime",
            "sun.rt.safepointSyncTime",
            "sun.rt.applicationTime",
            "sun.threads.vmOperationTime",
        ]
    );
}

/// zt t11/pl3: on ZGC the perf collectors are HotSpot's pause counters,
/// at ordinals 0 (minor) and 2 (major), whatever order the beans are in;
/// a missing bean reads 0 and slot 1 stays 0.
#[test]
fn zt11pl3_zgc_collector_slots_are_the_pause_beans_at_0_and_2() {
    use cratonvm_gc::gc_metrics::{
        JMX_ZGC_MAJOR_CYCLES_COLLECTOR, JMX_ZGC_MAJOR_PAUSES_COLLECTOR,
        JMX_ZGC_MINOR_CYCLES_COLLECTOR, JMX_ZGC_MINOR_PAUSES_COLLECTOR,
    };
    let beans: [(&str, u64, u64); 4] = [
        (JMX_ZGC_MINOR_CYCLES_COLLECTOR, 4, 40),
        (JMX_ZGC_MINOR_PAUSES_COLLECTOR, 12, 3),
        (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 2, 90),
        (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 6, 1),
    ];
    assert_eq!(zgc_collector_slots(&beans), [(12, 3), (0, 0), (6, 1)]);
    let mut reversed = beans;
    reversed.reverse();
    assert_eq!(zgc_collector_slots(&reversed), [(12, 3), (0, 0), (6, 1)]);
    assert_eq!(zgc_collector_slots(&beans[..1]), [(0, 0); 3]);
    assert_eq!(zgc_collector_slots(&[]), [(0, 0); 3]);
}

/// zt t11/pl3: the pause-end refresh, on every collector since
/// g1gen/c8, and its switch turns it off.
#[test]
fn zt11pl3_the_pause_refresh_is_killable() {
    assert!(pause_refresh_applies(true));
    assert!(!pause_refresh_applies(false));
    // The function itself is a no-op without a live image (a unit-test
    // VM publishes none).
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    refresh_after_pause(&shared);
    assert!(shared.debug.perf_data_file.lock().is_none());
}



/// zt t12/pl4: the counters a ZGC-shaped image leaves out are exactly the
/// seven HotSpot 25's ZGC image lacks (census rows `census-J` / `-C`).
#[test]
fn zt12pl4_the_zgc_shape_leaves_out_hotspots_absent_counters() {
    let absent: Vec<&str> = (0..LIVE_COUNT)
        .filter(|&i| absent_on_zgc(i))
        .map(|i| LIVE_COUNTERS[i].0)
        .collect();
    assert_eq!(
        absent,
        [
            "sun.os.hrt.ticks",
            "sun.gc.collector.1.invocations",
            "sun.gc.collector.1.time",
            "sun.gc.generation.0.space.1.capacity",
            "sun.gc.generation.0.space.1.used",
            "sun.gc.generation.0.space.2.capacity",
            "sun.gc.generation.0.space.2.used",
        ]
    );
    assert!(!zgc_image_shape(false), "never off ZGC");
}

/// zt t12/pl4: the offsets of the kept counters land in counter order,
/// a left-out counter gets `usize::MAX`, and offsets past the kept ones
/// (the ZGC constants after the live block) are not taken.
#[test]
fn zt12pl4_live_offsets_spread_over_the_kept_counters() {
    assert_eq!(
        spread_live_offsets(&[40, 48, 56, 999], &[true, false, true, true]),
        [40, usize::MAX, 48, 56]
    );
    assert_eq!(spread_live_offsets(&[40], &[true, true]), [40, usize::MAX]);
    assert!(spread_live_offsets(&[40], &[]).is_empty());
}

/// zt t12/pl4: a ZGC VM's image (when `CRATONVM_ZGC_PERF_IMAGE_SHAPE` is
/// on) has no `collector.1`, no survivor spaces and no `hrt.ticks`, names
/// its collectors and spaces as HotSpot's ZGC image does, keeps every
/// other live counter at an aligned offset, and takes writes for every
/// index (a left-out one is skipped); a non-ZGC image keeps every
/// counter and no ZGC name. Both fit behind long identity strings.
#[test]
fn zt12pl4_a_zgc_image_has_hotspots_zgc_counter_set() {
    let mut config = crate::config::VmConfig::default();
    let long = "x".repeat(3000);
    config.system_properties.push(("sun.java.command".to_string(), long.clone()));
    config.jvm_args = vec![long];
    let has = |bytes: &[u8], name: &str| {
        let needle = format!("{name}\0");
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    };
    let (other_bytes, other) = image_for_vm(&config, false);
    assert_eq!(other.len(), LIVE_COUNT);
    assert!(other.iter().all(|&at| at != usize::MAX), "off ZGC: every counter");
    assert!(has(&other_bytes, "sun.gc.collector.1.invocations"));
    assert!(!has(&other_bytes, "sun.gc.collector.0.name"));

    let shaped = zgc_image_shape(true);
    let (bytes, offsets) = image_for_vm(&config, true);
    assert_eq!(offsets.len(), LIVE_COUNT);
    for (i, &at) in offsets.iter().enumerate() {
        if shaped && absent_on_zgc(i) {
            assert_eq!(at, usize::MAX, "{} is left out", LIVE_COUNTERS[i].0);
        } else {
            assert!(
                at != usize::MAX && at % 8 == 0 && at + 8 <= PERF_DATA_SIZE,
                "{} is kept",
                LIVE_COUNTERS[i].0
            );
        }
    }
    for name in [
        "sun.gc.collector.1.invocations",
        "sun.gc.generation.0.space.1.used",
        "sun.gc.generation.0.space.2.capacity",
        "sun.os.hrt.ticks",
    ] {
        assert_eq!(has(&bytes, name), !shaped, "{name}");
    }
    for name in [
        "sun.gc.collector.0.name",
        "sun.gc.collector.2.name",
        "sun.gc.generation.0.name",
        "sun.gc.generation.0.spaces",
        "sun.gc.generation.0.space.0.name",
        "sun.gc.generation.1.name",
        "sun.gc.generation.1.spaces",
        "sun.gc.generation.1.space.0.name",
        "ZGC minor collection pauses",
        "ZGC major collection pauses",
    ] {
        assert_eq!(has(&bytes, name), shaped, "{name}");
    }
    assert!(has(&bytes, "sun.gc.collector.2.invocations"));
    assert!(has(&bytes, "sun.gc.generation.0.space.0.used"));

    // Every index may be written; a left-out one is skipped.
    let file = create_in_memory_for_vm(&config, true).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let mut values = [0i64; LIVE_COUNT];
    values[LIVE_COLLECTOR + 2] = 5;
    values[LIVE_COLLECTOR + 4] = 6;
    values[LIVE_S0] = 7;
    values[LIVE_OLD + 1] = 8;
    file.write_live(&values);
    assert_eq!(read_long(base, file.live_offsets[LIVE_COLLECTOR + 4]), 6);
    assert_eq!(read_long(base, file.live_offsets[LIVE_OLD + 1]), 8);
    if !shaped {
        assert_eq!(read_long(base, file.live_offsets[LIVE_COLLECTOR + 2]), 5);
        assert_eq!(read_long(base, file.live_offsets[LIVE_S0]), 7);
    }
    file.remove();
}

/// zt t10/pl2 (item 5): lvmid 0 and negative ids map nothing.
#[test]
fn zt10pl2_foreign_attach_refuses_zero_and_negative_ids() {
    assert!(attach_foreign(0).is_none());
    assert!(attach_foreign(-1).is_none());
    assert!(attach_foreign(i32::MIN).is_none());
}

/// zt t10/pl2 (item 5): another VM's file maps read-only and whole, a
/// link to one or a short file does not, and the view unmaps.
#[cfg(unix)]
#[test]
fn zt10pl2_a_foreign_file_maps_read_only_and_whole() {
    let root = std::env::temp_dir().join(format!(
        "cvm-perf-foreign-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    // zt t11/pl3 (P2): the publisher's modes, whatever the umask.
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    };
    mode(&root, 0o755);
    let (bytes, _) = encode_perf_data_with_offsets(&live_entries(), 1);
    let path = root.join("4242");
    std::fs::write(&path, &bytes).unwrap();
    mode(&path, 0o600);
    let (base, len) = map_foreign_file(&path).expect("a mapped foreign file");
    assert_eq!(len, bytes.len());
    // SAFETY: a live read-only mapping of `len` bytes.
    let seen = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    assert_eq!(seen, &bytes[..]);
    detach_foreign(base, len);
    let link = root.join("4243");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(map_foreign_file(&link).is_none(), "a link is refused");
    let short = root.join("4244");
    std::fs::write(&short, [0u8; 8]).unwrap();
    assert!(map_foreign_file(&short).is_none(), "shorter than a prologue");
    assert!(map_foreign_file(&root.join("4245")).is_none(), "no such file");
    // zt t11/pl3 (P2): a group-writable file, or a file in a
    // group-writable directory, is refused.
    mode(&path, 0o620);
    assert!(map_foreign_file(&path).is_none(), "a group-writable file");
    mode(&path, 0o600);
    mode(&root, 0o775);
    assert!(map_foreign_file(&path).is_none(), "a group-writable directory");
    mode(&root, 0o755);
    assert!(map_foreign_file(&path).is_some_and(|(b, l)| {
        detach_foreign(b, l);
        true
    }));
    // zt t12/x9: a second hard link makes both names refused (HotSpot's
    // `st_nlink > 1` rule); removing it restores the attach.
    let hard = root.join("4246");
    std::fs::hard_link(&path, &hard).unwrap();
    assert!(map_foreign_file(&path).is_none(), "a file with two links");
    assert!(map_foreign_file(&hard).is_none(), "its other name");
    std::fs::remove_file(&hard).unwrap();
    assert!(map_foreign_file(&path).is_some_and(|(b, l)| {
        detach_foreign(b, l);
        true
    }));
    // zt t14/x11 (x10 page item 10): the directory is opened as a
    // descriptor with `O_NOFOLLOW`: a link standing for the directory is
    // refused; the real directory (positive control) maps.
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    mode(&sub, 0o755);
    let inner = sub.join("4247");
    std::fs::write(&inner, &bytes).unwrap();
    mode(&inner, 0o600);
    let sub_link = root.join("sublink");
    std::os::unix::fs::symlink(&sub, &sub_link).unwrap();
    assert!(
        map_foreign_file(&sub_link.join("4247")).is_none(),
        "a link as the directory"
    );
    assert!(map_foreign_file(&inner).is_some_and(|(b, l)| {
        detach_foreign(b, l);
        true
    }));
    std::fs::remove_dir_all(&root).unwrap();
}

/// zt t11/pl3 (t11/x8 P2): the foreign attach's ownership and mode rule.
#[cfg(unix)]
#[test]
fn zt11pl3_a_foreign_entry_must_be_ours_and_not_shared_writable() {
    assert!(foreign_entry_is_safe(true, 1000, 0o100600, 1000));
    assert!(foreign_entry_is_safe(true, 1000, 0o40755, 1000));
    assert!(!foreign_entry_is_safe(false, 1000, 0o100600, 1000), "wrong kind");
    assert!(!foreign_entry_is_safe(true, 1001, 0o100600, 1000), "another owner");
    assert!(!foreign_entry_is_safe(true, 1000, 0o100620, 1000), "group-writable");
    assert!(!foreign_entry_is_safe(true, 1000, 0o41777, 1000), "world-writable");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The prologue HotSpot 25.0.4 wrote (MEASURED, the host oracle of
/// 2026-10-09, default flags: used 12184, time stamp 21849324, 185
/// entries).
#[cfg(target_endian = "little")]
#[test]
fn the_prologue_matches_hotspots_bytes() {
    assert_eq!(
        hex(&encode_prologue(12184, 21_849_324, 185)),
        "cafec0c001020001982f000000000000ec644d010000000020000000b9000000"
    );
}

/// Entry #0 (`sun.rt.createVmBeginTime`, a `long`) and entry #12
/// (`sun.rt.jvmCapabilities`, default and `-XX:+DisableAttachMechanism`)
/// byte for byte as HotSpot 25.0.4 wrote them (MEASURED).
#[cfg(target_endian = "little")]
#[test]
fn entries_match_hotspots_bytes() {
    let begin = vm_entries(1_791_557_291_013, true, "", &[], "")[0].clone();
    assert_eq!(
        hex(&encode_entry(&begin)),
        "3800000014000000000000004a0001033000000073756e2e72742e637265617465566d426567696e54696d650000000005cc2221a1010000"
    );
    let caps = |attachable: bool| vm_entries(0, attachable, "", &[], "")[1].clone();
    let head = "700000001400000041000000420005012b00000073756e2e72742e6a766d4361706162696c697469657300";
    let tail = format!("{}0000000000", "30".repeat(62));
    let default = format!("{head}3131{tail}");
    assert_eq!(default.len(), 224);
    assert_eq!(hex(&encode_entry(&caps(true))), default);
    assert_eq!(
        hex(&encode_entry(&caps(false))),
        format!("{head}3031{tail}")
    );
}

/// `java.property.java.vm.name` lays out as HotSpot's entry #171 did
/// (MEASURED: length 72, vector 25, flags 1, units 5, variability 1,
/// data offset 47), and the file is a walkable v2.0 buffer whose entries
/// end at the first zero length.
#[test]
fn a_string_property_and_the_whole_file_lay_out_as_hotspots() {
    let entry = PerfEntry::string_constant(
        "java.property.java.vm.name",
        "OpenJDK 64-Bit Server VM",
        true,
    );
    let bytes = encode_entry(&entry);
    let int_of = |at: usize| i32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!(bytes.len(), 72);
    assert_eq!(
        (int_of(0), int_of(4), int_of(8), int_of(16)),
        (72, 20, 25, 47)
    );
    assert_eq!(&bytes[12..16], &[b'B', 1, 5, 1]);
    assert_eq!(&bytes[47..47 + 24], b"OpenJDK 64-Bit Server VM");
    assert_eq!(bytes[47 + 24], 0);

    let entries = vm_entries(
        1,
        false,
        "Main a b",
        &[
            ("java.vm.name", "cratonvm"),
            ("java.vm.version", "25.0.1+8"),
        ],
        "-Dx=y",
    );
    let file = encode_perf_data(&entries, 7);
    assert_eq!(file.len(), PERF_DATA_SIZE);
    assert_eq!(&file[0..4], &[0xca, 0xfe, 0xc0, 0xc0]);
    let int_at = |at: usize| i32::from_ne_bytes(file[at..at + 4].try_into().unwrap());
    assert_eq!(int_at(24), 32);
    assert_eq!(int_at(28), entries.len() as i32);
    // Walk it as `PerfDataBuffer.getNextMonitorEntry` does.
    let mut at = 32usize;
    let mut names = Vec::new();
    while at < file.len() && int_at(at) != 0 {
        let length = int_at(at) as usize;
        assert_eq!(at % 4, 0);
        assert_eq!(length % 8, 0);
        let name_at = at + int_at(at + 4) as usize;
        let end = file[name_at..].iter().position(|b| *b == 0).unwrap();
        names.push(String::from_utf8(file[name_at..name_at + end].to_vec()).unwrap());
        at += length;
    }
    assert_eq!(int_at(8) as usize, at, "used");
    assert_eq!(
        names,
        [
            "sun.rt.createVmBeginTime",
            "sun.rt.jvmCapabilities",
            "sun.rt.javaCommand",
            "java.property.java.vm.name",
            "java.property.java.vm.version",
            "java.rt.vmArgs",
            "java.rt.vmFlags",
        ]
    );
}

/// zt t14/pl5: `sun.rt.jvmVersion` is HotSpot's `jvm_version()` layout:
/// major, minor, security and build in one byte each, major first.
#[test]
fn zt14pl5_jvm_version_word_is_hotspots_layout() {
    assert_eq!(jvm_version_word("25.0.1+8"), 0x1900_0108);
    assert_eq!(jvm_version_word("25.0.3+9-LTS"), 0x1900_0309);
    assert_eq!(jvm_version_word("25"), 0x1900_0000);
    assert_eq!(jvm_version_word("21.0.10+7"), 0x1500_0A07);
    assert_eq!(jvm_version_word(""), 0);
}

/// zt t14/pl5: `sun.rt.internalVersion` starts with the VM's name and
/// carries its release in parentheses, as HotSpot's string does.
#[test]
fn zt14pl5_internal_version_has_hotspots_shape() {
    let text = internal_version_with("cratonvm", "25.0.1+8", "25.0.3+9-LTS", None);
    assert!(text.starts_with("cratonvm (25.0.1+8) for "), "{text}");
    assert!(text.ends_with(" JRE (25.0.3+9-LTS)"), "{text}");
    // With the binary's stamp, HotSpot 25's tail.
    let stamp = Some(("2026-10-26T01:02:03Z", "rustc 1.99.1"));
    let text = internal_version_with("cratonvm", "25.0.1+8", "25.0.3+9-LTS", stamp);
    assert!(
        text.ends_with(" JRE (25.0.3+9-LTS), built on 2026-10-26T01:02:03Z with rustc 1.99.1"),
        "{text}"
    );
}

/// zt t14/pl5: a string counter is rewritten in place, cut to its
/// capacity less the NUL and NUL-padded (no tail of a longer old text).
#[test]
fn zt14pl5_a_string_counter_is_rewritten_nul_padded() {
    let file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let at = 30_000;
    let read = |n: usize| {
        // SAFETY: the test's own attached image, PERF_DATA_SIZE bytes.
        unsafe { std::slice::from_raw_parts((base + at) as *const u8, n).to_vec() }
    };
    store_string(base, at, 8, "System.gc()");
    assert_eq!(read(8), b"System.\0");
    store_string(base, at, 8, "No GC");
    assert_eq!(read(8), b"No GC\0\0\0");
    store_string(base, usize::MAX, 8, "x");
    store_string(base, PERF_DATA_SIZE - 4, 8, "x");
    assert_eq!(read(8), b"No GC\0\0\0", "out of range: nothing written");
    file.remove();
}

/// zt t14/pl5: appended entries extend a live image as HotSpot adds
/// counters to its perf memory: `used` and the entry count move, the
/// walk a reader does finds them after the image's own entries, and the
/// writers reach their data.
#[test]
fn zt14pl5_appended_entries_extend_a_live_image() {
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let int_at = |at: usize| {
        // SAFETY: the test's own attached image; `at` is 4-byte aligned
        // inside it.
        unsafe { std::ptr::read_volatile((base + at) as *const i32) }
    };
    let (used, count) = (int_at(8), int_at(28));
    let at = file.append_entries(&[
        long_entry("sun.rt.vmInitDoneTime", 0, UNITS_NONE, VARIABILITY_VARIABLE),
        PerfEntry {
            name: "sun.gc.lastCause".to_string(),
            value: PerfValue::Str {
                text: NO_GC_CAUSE.to_string(),
                capacity: GC_CAUSE_CAPACITY,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_VARIABLE,
        },
    ]);
    let used_now = int_at(8) as usize;
    assert_eq!(int_at(28), count + 2);
    assert!(used_now > used as usize);
    assert!(at.iter().all(|&o| o != usize::MAX && o < used_now));
    assert_eq!(at[0] % 8, 0);
    // A reader's walk ends at `used`, and its last two names are ours.
    let mut walk = 32usize;
    let mut names = Vec::new();
    while walk < used_now {
        let length = int_at(walk) as usize;
        let name_at = walk + int_at(walk + 4) as usize;
        // SAFETY: inside the test's own image (a name is short and the
        // walk stays below `used`, far from the image's end).
        let bytes = unsafe { std::slice::from_raw_parts((base + name_at) as *const u8, 64) };
        let end = bytes.iter().position(|b| *b == 0).unwrap();
        names.push(String::from_utf8(bytes[..end].to_vec()).unwrap());
        walk += length;
    }
    assert_eq!(walk, used_now);
    let ours = ["sun.rt.vmInitDoneTime", "sun.gc.lastCause"];
    assert_eq!(names[names.len() - 2..], ours);
    file.extra.init_times = [at[0], usize::MAX];
    file.extra.gc_cause = [usize::MAX, at[1]];
    file.write_extras(&ExtraValues {
        init_times: Some([1234, 5678]),
        last_cause: Some("System.gc()"),
        ..ExtraValues::default()
    });
    assert_eq!(read_long(base, at[0]), 1234);
    // SAFETY: inside the test's own image.
    let cause = unsafe { std::slice::from_raw_parts((base + at[1]) as *const u8, 12) };
    assert_eq!(cause, b"System.gc()\0");
    file.remove();
}

/// zt t14/pl5 (page item 5): an id nobody published perf data for is
/// HotSpot's `IllegalArgumentException` case.
#[test]
fn zt14pl5_an_id_without_perf_data_is_missing() {
    for id in [-1, i32::MIN, i32::MAX] {
        assert!(foreign_attach_missing_message(id).is_some(), "{id}");
    }
    let message = foreign_attach_missing_message(-1).unwrap();
    let prefix = "Could not map vmid to user ";
    assert!(message.starts_with(prefix), "{message}");
}


/// zt t22/x18: `sun.os.hrt.ticks` counts from the VM's start (the origin
/// of `lastEntryTime` / `lastExitTime` and `Perf.highResCounter`), and from
/// the sampler's start under the `=0` kill.
#[test]
fn zt22x18_hrt_ticks_count_from_the_vm_start() {
    let vm_start = std::time::Instant::now();
    let sampler_start = vm_start + std::time::Duration::from_millis(250);
    assert_eq!(hrt_ticks_origin(vm_start, sampler_start, true), vm_start);
    assert_eq!(
        hrt_ticks_origin(vm_start, sampler_start, false),
        sampler_start,
        "kill: the sampler's start"
    );
}


/// zt t18/pl8: `sun.ci.lastFailedMethod` and the four longs reach their
/// offsets; nothing before they are added.
#[test]
fn zt18pl8_compile_extras_reach_their_data() {
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let var = |name: &str| long_entry(name, 0, UNITS_TICKS, VARIABILITY_MONOTONIC);
    let at = file.append_entries(&[
        var("sun.ci.standardTime"),
        var("sun.ci.osrTime"),
        var("sun.ci.totalBailouts"),
        var("sun.ci.lastFailedType"),
        PerfEntry {
            name: "sun.ci.lastFailedMethod".to_string(),
            value: PerfValue::Str {
                text: String::new(),
                capacity: CI_LAST_METHOD_CAPACITY,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_VARIABLE,
        },
    ]);
    assert!(at.iter().all(|&o| o != usize::MAX));
    file.write_last_failed_method("p/Q bailed");
    file.write_extras(&ExtraValues {
        compile_extras: Some([7, 8, 9, CI_OSR_COMPILE]),
        ..ExtraValues::default()
    });
    assert_eq!(
        read_long(base, at[0]),
        0,
        "nothing before the offsets exist"
    );
    file.extra.compile_extras = [at[0], at[1], at[2], at[3]];
    file.extra.last_failed_method = at[4];
    file.write_extras(&ExtraValues {
        compile_extras: Some([7, 8, 9, CI_OSR_COMPILE]),
        ..ExtraValues::default()
    });
    let read = at[..4]
        .iter()
        .map(|&o| read_long(base, o))
        .collect::<Vec<_>>();
    assert_eq!(read, [7, 8, 9, CI_OSR_COMPILE]);
    file.write_last_failed_method("p/Q bailed");
    // SAFETY: inside the test's own image.
    let text = unsafe {
        std::slice::from_raw_parts((base + at[4]) as *const u8, CI_LAST_METHOD_CAPACITY)
    };
    assert_eq!(&text[..11], b"p/Q bailed\0");
    file.remove();
}

/// zt t15/pl6: `sun.ci.lastMethod` is HotSpot's `"<class> <method>"`
/// with the class's internal name.
#[test]
fn zt15pl6_last_method_text_is_hotspots_shape() {
    let text = last_method_text(
        "jdk/internal/classfile/impl/AbstractPoolEntry$AbstractMemberRefEntry",
        "nameAndType",
    );
    assert_eq!(
        text,
        "jdk/internal/classfile/impl/AbstractPoolEntry$AbstractMemberRefEntry nameAndType"
    );
    assert_eq!(
        last_method_text("java.lang.String", "hashCode"),
        "java/lang/String hashCode"
    );
}

/// zt t15/pl6: the thread, class and compile counters are written at
/// their offsets; `java.threads.started` never goes back (a counter),
/// `live` follows the value written; `sun.ci.lastMethod` is cut to its
/// 160 bytes; nothing is written for a counter not added.
#[test]
fn zt15pl6_thread_class_and_compile_counters_reach_their_data() {
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let var = |name: &str| long_entry(name, 0, UNITS_NONE, VARIABILITY_VARIABLE);
    let at = file.append_entries(&[
        var("java.threads.started"),
        var("java.threads.live"),
        var("java.threads.livePeak"),
        var("java.threads.daemon"),
        var("sun.cls.initializedClasses"),
        var("sun.ci.totalCompiles"),
        PerfEntry {
            name: "sun.ci.lastMethod".to_string(),
            value: PerfValue::Str {
                text: String::new(),
                capacity: CI_LAST_METHOD_CAPACITY,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_VARIABLE,
        },
    ]);
    assert!(at.iter().all(|&o| o != usize::MAX));
    file.write_last_method("p/Q m");
    assert_eq!(
        read_long(base, at[0]),
        0,
        "nothing before the offsets exist"
    );
    file.extra.threads = [at[0], at[1], at[2], at[3]];
    file.extra.class_events = [at[4], usize::MAX, usize::MAX];
    file.extra.compiles = [usize::MAX, at[5], usize::MAX, usize::MAX, usize::MAX];
    file.extra.last_method = at[6];
    file.write_extras(&ExtraValues {
        threads: Some([11, 10, 12, 8]),
        class_events: Some([1184, 1309, 1309]),
        compiles: Some([0, 505, 1, 504, 1]),
        ..ExtraValues::default()
    });
    let read4 = |offsets: &[usize]| {
        offsets
            .iter()
            .map(|&o| read_long(base, o))
            .collect::<Vec<_>>()
    };
    assert_eq!(read4(&at[..4]), [11, 10, 12, 8]);
    assert_eq!((read_long(base, at[4]), read_long(base, at[5])), (1184, 505));
    file.write_extras(&ExtraValues {
        threads: Some([9, 7, 12, 6]),
        ..ExtraValues::default()
    });
    assert_eq!(read4(&at[..4]), [11, 7, 12, 6], "started keeps its maximum");
    let long_name = format!("p/{} m", "C".repeat(200));
    file.write_last_method(&long_name);
    // SAFETY: inside the test's own image.
    let text = unsafe {
        std::slice::from_raw_parts((base + at[6]) as *const u8, CI_LAST_METHOD_CAPACITY)
    };
    assert_eq!(
        &text[..CI_LAST_METHOD_CAPACITY - 1],
        &long_name.as_bytes()[..159]
    );
    assert_eq!(text[CI_LAST_METHOD_CAPACITY - 1], 0);
    file.remove();
}

/// zt t16/pl7: the `sun.gc.tlab.*` values are `ThreadLocalAllocStats::
/// publish`'s: threads, refills and wastes of the refilling threads only,
/// slow allocations of every thread, the handed-out bytes and the wastes
/// in heap words, and nothing published when no thread refilled (HotSpot
/// returns early and the counters keep their values).
#[test]
fn zt16pl7_tlab_values_are_hotspots_publish_in_words() {
    use cratonvm_gc::tlab::{TlabCycleDelta, TlabTotals};
    let mut t = TlabTotals::default();
    t.add_thread(
        TlabCycleDelta {
            refills: 0,
            slow_allocations: 2,
            gc_waste_bytes: 0,
            refill_waste_bytes: 0,
        },
        512,
    );
    assert_eq!(tlab_perf_values(&t, 4096), None, "no refilling thread");
    t.add_thread(
        TlabCycleDelta {
            refills: 6,
            slow_allocations: 1,
            gc_waste_bytes: 1_528_560,
            refill_waste_bytes: 800,
        },
        100_000,
    );
    t.add_thread(
        TlabCycleDelta {
            refills: 2,
            slow_allocations: 0,
            gc_waste_bytes: 64,
            refill_waste_bytes: 8_000,
        },
        50_000,
    );
    let v = tlab_perf_values(&t, 1_572_864).expect("two threads refilled");
    assert_eq!(
        v,
        [2, 8, 6, 196_608, 191_078, 191_070, 1_100, 1_000, 3, 2],
        "allocThreads fills maxFills alloc gcWaste maxGcWaste refillWaste \
         maxRefillWaste slowAlloc maxSlowAlloc"
    );
    let names: Vec<&str> = TLAB_COUNTERS.iter().map(|c| c.0).collect();
    assert_eq!(names[0], "sun.gc.tlab.allocThreads");
    assert_eq!(names[3], "sun.gc.tlab.alloc");
    assert_eq!(names[9], "sun.gc.tlab.maxSlowAlloc");
    let bytes: Vec<&str> = TLAB_COUNTERS
        .iter()
        .filter(|c| c.1 == UNITS_BYTES)
        .map(|c| c.0)
        .collect();
    assert_eq!(
        bytes,
        [
            "sun.gc.tlab.alloc",
            "sun.gc.tlab.gcWaste",
            "sun.gc.tlab.maxGcWaste",
            "sun.gc.tlab.refillWaste",
            "sun.gc.tlab.maxRefillWaste"
        ]
    );
}

/// zt t16/pl7: the published TLAB values reach their ten offsets; a
/// write without them leaves the counters.
#[test]
fn zt16pl7_tlab_counters_reach_their_data() {
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let entries =
        TLAB_COUNTERS.map(|(name, units)| long_entry(name, 0, units, VARIABILITY_VARIABLE));
    let at = file.append_entries(&entries);
    assert!(at.iter().all(|&o| o != usize::MAX && o % 8 == 0));
    file.extra.tlab = std::array::from_fn(|i| at[i]);
    let values = [2, 8, 6, 196_608, 191_078, 191_070, 1_100, 1_000, 3, 2];
    file.write_extras(&ExtraValues {
        tlab: Some(values),
        ..ExtraValues::default()
    });
    let read = || at.iter().map(|&o| read_long(base, o)).collect::<Vec<_>>();
    assert_eq!(read(), values);
    file.write_extras(&ExtraValues::default());
    assert_eq!(read(), values, "a write without TLAB values leaves them");
    file.remove();
}

/// zt t16/pl7: the define counts and the class-trace timers reach their
/// offsets, and `sun.cls.time` is the live counter the sampler fills
/// from the timers' self times.
#[test]
fn zt16pl7_define_counts_and_class_timers_reach_their_data() {
    assert_eq!(LIVE_COUNTERS[LIVE_CLS_TIME].0, "sun.cls.time");
    let classes = ClassCounters {
        time_ticks: 12_345,
        ..ClassCounters::default()
    };
    let v = live_values(
        0,
        &HeapCounters::default(),
        &classes,
        &RuntimeCounters::default(),
    );
    // zt t21/pl10: nanoseconds, written in ticks.
    assert_eq!(v[LIVE_CLS_TIME], ticks_from_nanos(12_345));
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let mut entries = vec![
        long_entry(
            "sun.cls.defineAppClasses",
            0,
            UNITS_TICKS,
            VARIABILITY_MONOTONIC,
        ),
        long_entry(
            "sun.cls.appClassBytes",
            0,
            UNITS_BYTES,
            VARIABILITY_MONOTONIC,
        ),
        long_entry(
            "sun.cls.unsafeDefineClassCalls",
            0,
            UNITS_EVENTS,
            VARIABILITY_MONOTONIC,
        ),
    ];
    entries.extend(
        CLASS_TIMER_COUNTERS.map(|n| long_entry(n, 0, UNITS_TICKS, VARIABILITY_MONOTONIC)),
    );
    let at = file.append_entries(&entries);
    assert!(at.iter().all(|&o| o != usize::MAX));
    file.extra.define_counts = [at[0], at[1], at[2]];
    file.extra.class_times = std::array::from_fn(|i| at[3 + i]);
    let times = [80, 30, 50, 50, 900, 400, 120, 120];
    file.write_extras(&ExtraValues {
        define_counts: Some([7, 4_096, 2]),
        class_times: Some(times),
        ..ExtraValues::default()
    });
    let read = |r: std::ops::Range<usize>| {
        at[r]
            .iter()
            .map(|&o| read_long(base, o))
            .collect::<Vec<_>>()
    };
    assert_eq!(read(0..3), [7, 4_096, 2]);
    assert_eq!(read(3..11), times);
    file.remove();
}

/// zt t29/cb (perf-counter page item 1): `sun.cls.loadedBytes` is the
/// live figure plus the unloaded bytes and `unloadedBytes` the latter, so
/// an unload (live down by what unloaded grows) leaves `loadedBytes`
/// where it was; with no unloaded bytes (the kill) both are as before.
#[test]
fn zt29cb_loaded_bytes_are_live_plus_unloaded() {
    assert_eq!(
        LIVE_COUNTERS[LIVE_UNLOADED_BYTES].0,
        "sun.cls.unloadedBytes"
    );
    let at = |metadata_bytes: u64, unloaded_bytes: u64| {
        let classes = ClassCounters {
            metadata_bytes,
            unloaded_bytes,
            ..ClassCounters::default()
        };
        let v = live_values(
            0,
            &HeapCounters::default(),
            &classes,
            &RuntimeCounters::default(),
        );
        (v[LIVE_LOADED_BYTES], v[LIVE_UNLOADED_BYTES])
    };
    assert_eq!(at(10_000, 0), (10_000, 0));
    // 1 500 bytes of classes unload: live 8 500.
    assert_eq!(at(8_500, 1_500), (10_000, 1_500));
    // More classes load.
    assert_eq!(at(9_000, 1_500), (10_500, 1_500));
    // `metaspace.used` stays the live figure.
    let classes = ClassCounters {
        metadata_bytes: 8_500,
        unloaded_bytes: 1_500,
        ..ClassCounters::default()
    };
    let v = live_values(
        0,
        &HeapCounters::default(),
        &classes,
        &RuntimeCounters::default(),
    );
    assert_eq!(v[LIVE_METASPACE + 1], 8_500);
}

/// zt t29/cb: `sun.cls.sysClassBytes` reaches its offset; a write that
/// leaves it (`None`) does not touch it.
#[test]
fn zt29cb_sys_class_bytes_reach_their_data() {
    let mut file = create_in_memory_for_vm(&crate::config::VmConfig::default(), false).unwrap();
    let (base, _) = file.attach_view().unwrap();
    let at = file.append_entries(&[long_entry(
        "sun.cls.sysClassBytes",
        0,
        UNITS_BYTES,
        VARIABILITY_MONOTONIC,
    )]);
    assert_ne!(at[0], usize::MAX);
    file.extra.sys_class_bytes = at[0];
    file.write_extras(&ExtraValues {
        sys_class_bytes: Some(1_234),
        ..ExtraValues::default()
    });
    assert_eq!(read_long(base, at[0]), 1_234);
    file.write_extras(&ExtraValues::default());
    assert_eq!(read_long(base, at[0]), 1_234);
    file.remove();
}

/// zt t16/pl7: the timers' names are HotSpot's `classLoader.cpp` ones, in
/// `ClassTraceCounters::snapshot`'s order (inclusive, then self, per
/// kind: link, verify, `<clinit>`, define).
#[test]
fn zt16pl7_class_timer_names_follow_the_snapshot_order() {
    use cratonvm_types::perf_class_trace::ClassTraceEvent;
    for (event, stem) in [
        (ClassTraceEvent::Link, "sun.cls.classLinkedTime"),
        (ClassTraceEvent::Verify, "sun.cls.classVerifyTime"),
        (ClassTraceEvent::Clinit, "sun.cls.classInitTime"),
        (ClassTraceEvent::DefineClass, "sun.cls.defineAppClassTime"),
    ] {
        let i = 2 * event as usize;
        assert_eq!(CLASS_TIMER_COUNTERS[i], stem);
        assert_eq!(CLASS_TIMER_COUNTERS[i + 1], format!("{stem}.self"));
    }
}

/// zt t20/pl9 (D5): before the first collection the combined
/// `maxCapacity` is the class space's reservation (HotSpot's
/// `universe_init` figure: 1073741824 with the defaults), after one the
/// class plus non-class reservation; the class space's own figure never
/// changes; the kill keeps the combined figure throughout.
#[test]
fn zt20pl9_metaspace_max_is_the_class_space_until_a_collection() {
    let gib = 1_073_741_824_u64;
    let node = 64 * 1024 * 1024;
    let stats = crate::vm::vm_init::MetaspaceStats::from_triples(
        (6_000_000, 8_000_000, gib + node),
        Some((500_000, 1_000_000, gib)),
    );
    let gib_i = i64::try_from(gib).unwrap();
    let both = i64::try_from(gib + node).unwrap();
    assert_eq!(metaspace_max_values(&stats, 0, true), [gib_i, gib_i]);
    assert_eq!(metaspace_max_values(&stats, 1, true), [both, gib_i]);
    assert_eq!(metaspace_max_values(&stats, 0, false), [both, gib_i]);
    // No class space (`-XX:-UseCompressedClassPointers`): nothing
    // reserved at `universe_init`.
    let flat = crate::vm::vm_init::MetaspaceStats::flat(4_096);
    assert_eq!(metaspace_max_values(&flat, 0, true), [0, 0]);
    assert_eq!(metaspace_max_values(&flat, 3, true), [4_096, 0]);
}

/// zt t21/pl10 (`CRATONVM_PERF_METASPACE_AT_GC`, page item 10): the six
/// metaspace counters read 0 (the reservations D5's) before the first
/// collection, then the figures of the first pass after each collection,
/// held until the count moves; off, the current figures at every pass.
#[test]
fn zt21pl10_metaspace_counters_hold_the_last_collections_figures() {
    let gib = 1_073_741_824_u64;
    let node = 64 * 1024 * 1024;
    let at = |used: u64, ccs_used: u64| {
        crate::vm::vm_init::MetaspaceStats::from_triples(
            (used, used * 2, gib + node),
            Some((ccs_used, ccs_used * 2, gib)),
        )
    };
    let gib_i = i64::try_from(gib).unwrap();
    let both = i64::try_from(gib + node).unwrap();
    let mut held = None;
    // Before any collection: universe_init's figures.
    let first = at(6_000_000, 500_000);
    assert_eq!(
        metaspace_counter_values(&first, 0, true, true, &mut held),
        [0, 0, gib_i, 0, 0, gib_i]
    );
    assert_eq!(held, None);
    // The first pass after collection 1 takes the figures...
    let after_one = metaspace_counter_values(&first, 1, true, true, &mut held);
    assert_eq!(
        after_one,
        [12_000_000, 6_000_000, both, 1_000_000, 500_000, gib_i]
    );
    // ...and holds them while classes load and no collection runs.
    let grown = at(7_000_000, 600_000);
    assert_eq!(
        metaspace_counter_values(&grown, 1, true, true, &mut held),
        after_one
    );
    // The next collection moves them.
    assert_eq!(
        metaspace_counter_values(&grown, 2, true, true, &mut held),
        [14_000_000, 7_000_000, both, 1_200_000, 600_000, gib_i]
    );
    // Kill: the current figures at every pass, and nothing held.
    let mut none = None;
    assert_eq!(
        metaspace_counter_values(&first, 0, true, false, &mut none),
        [12_000_000, 6_000_000, gib_i, 1_000_000, 500_000, gib_i]
    );
    assert_eq!(
        metaspace_counter_values(&grown, 1, true, false, &mut none),
        [14_000_000, 7_000_000, both, 1_200_000, 600_000, gib_i]
    );
    assert_eq!(none, None);
    // Into the live values: the capacities and uses only.
    let mut values = [7i64; LIVE_COUNT];
    put_metaspace_figures(&mut values, &[1, 2, 3, 4, 5, 6]);
    assert_eq!((values[LIVE_METASPACE], values[LIVE_METASPACE + 1]), (1, 2));
    assert_eq!((values[LIVE_CCS], values[LIVE_CCS + 1]), (4, 5));
    assert_eq!(
        values[LIVE_LOADED_BYTES], 7,
        "sun.cls.loadedBytes untouched"
    );
    assert_eq!(LIVE_COUNTERS[LIVE_METASPACE].0, "sun.gc.metaspace.capacity");
    assert_eq!(
        LIVE_COUNTERS[LIVE_CCS + 1].0,
        "sun.gc.compressedclassspace.used"
    );
}

/// zt t20/pl9 (D6): the application loader's class-path defines add to
/// the natives' define count and bytes, never to `unsafeDefineClassCalls`;
/// the kill leaves the natives' counts.
#[test]
fn zt20pl9_define_counts_add_the_class_path_door() {
    assert_eq!(
        define_count_values([2, 866, 1], (3, 13_670), true),
        [5, 14_536, 1]
    );
    assert_eq!(
        define_count_values([2, 866, 1], (3, 13_670), false),
        [2, 866, 1]
    );
    assert_eq!(
        define_count_values([i64::MAX, 0, 0], (1, u64::MAX), true),
        [i64::MAX, i64::MAX, 0]
    );
}

/// zt t20/pl9 (D9): the sampler's park returns at once for a stop or an
/// asked-for pass (clearing the request), and otherwise only once the
/// whole interval has passed, whatever early returns `park_timeout` makes
/// (an unpark with no request stands in for a spurious one).
#[test]
fn zt20pl9_the_sampler_park_holds_the_whole_interval() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let stop = AtomicBool::new(false);
    let wake = AtomicBool::new(true);
    let t = Instant::now();
    park_for_interval(Duration::from_secs(60), &stop, &wake);
    assert!(t.elapsed() < Duration::from_secs(30));
    assert!(!wake.load(Ordering::Acquire), "the request is consumed");
    let stopped = AtomicBool::new(true);
    park_for_interval(Duration::from_secs(60), &stopped, &wake);
    // An unpark with nothing asked: the park goes on to its deadline.
    let parked = std::thread::current();
    let poker = std::thread::spawn(move || {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(10));
            parked.unpark();
        }
    });
    let t = Instant::now();
    park_for_interval(Duration::from_millis(200), &stop, &wake);
    assert!(t.elapsed() >= Duration::from_millis(200));
    poker.join().unwrap();
}

