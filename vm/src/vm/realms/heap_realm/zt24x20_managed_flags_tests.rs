use super::ManagedHotspotFlags;
use cratonvm_native_api::hotspot_flags::HotspotVmFlag;

fn row(name: &'static str, value: &str) -> HotspotVmFlag {
    HotspotVmFlag {
        ty: "uintx",
        name,
        value: value.to_string(),
        kind: "{manageable}",
        origin: "{default}",
    }
}

/// zt t27/lg1: the manageable booleans and `HeapDumpPath` -- unwritten
/// reads `None`, a write lays value and `{management}` over the row, an
/// unmodelled name is refused.
#[test]
fn zt27lg1_written_booleans_and_path_override_their_rows() {
    let managed = ManagedHotspotFlags::default();
    let flag = |ty: &'static str, name: &'static str, value: &str| HotspotVmFlag {
        ty,
        name,
        value: value.to_string(),
        kind: "{manageable}",
        origin: "{default}",
    };
    let mut rows = vec![
        flag("bool", "HeapDumpOnOutOfMemoryError", "false"),
        flag("ccstr", "HeapDumpPath", ""),
        flag("bool", "ShowCodeDetailsInExceptionMessages", "true"),
    ];
    assert_eq!(managed.get_bool("HeapDumpOnOutOfMemoryError"), None);
    managed.apply(&mut rows);
    assert_eq!(rows[0].origin, "{default}");
    assert!(managed.set_bool("HeapDumpOnOutOfMemoryError", true));
    assert!(managed.set_bool("ShowCodeDetailsInExceptionMessages", false));
    assert!(!managed.set_bool("PrintFlagsFinal", true), "not modelled");
    managed.set_heap_dump_path("/tmp/x.hprof");
    managed.apply(&mut rows);
    assert_eq!(managed.get_bool("HeapDumpOnOutOfMemoryError"), Some(true));
    assert_eq!((rows[0].value.as_str(), rows[0].origin), ("true", "{management}"));
    assert_eq!((rows[1].value.as_str(), rows[1].origin), ("/tmp/x.hprof", "{management}"));
    assert_eq!((rows[2].value.as_str(), rows[2].origin), ("false", "{management}"));
    assert_eq!(managed.get("HeapDumpOnOutOfMemoryError"), None, "not a ratio cell");
}

/// zt t28/mf: `PrintConcurrentLocks` is a modelled boolean, and the
/// numeric rows keep their written text (last write wins) and refuse an
/// unmodelled name.
#[test]
fn zt28mf_written_numeric_rows_override_their_rows() {
    let managed = ManagedHotspotFlags::default();
    let flag = |ty: &'static str, name: &'static str, value: &str| HotspotVmFlag {
        ty,
        name,
        value: value.to_string(),
        kind: "{manageable}",
        origin: "{default}",
    };
    let mut rows = vec![
        flag("int", "HeapDumpGzipLevel", "0"),
        flag("double", "G1PeriodicGCSystemLoadThreshold", "0.000000"),
        flag("bool", "PrintConcurrentLocks", "false"),
    ];
    assert_eq!(managed.number_text("HeapDumpGzipLevel"), None);
    assert!(managed.set_number_text("HeapDumpGzipLevel", "3".to_string()));
    assert!(managed.set_number_text("HeapDumpGzipLevel", "6".to_string()));
    assert!(managed.set_number_text("G1PeriodicGCSystemLoadThreshold", "0.500000".to_string()));
    assert!(!managed.set_number_text("SurvivorRatio", "3".to_string()), "not modelled");
    assert!(managed.set_bool("PrintConcurrentLocks", true));
    managed.apply(&mut rows);
    assert_eq!((rows[0].value.as_str(), rows[0].origin), ("6", "{management}"));
    assert_eq!((rows[1].value.as_str(), rows[1].origin), ("0.500000", "{management}"));
    assert_eq!((rows[2].value.as_str(), rows[2].origin), ("true", "{management}"));
}

/// Nothing written: the rows are untouched. `0` is a value (stored as 1),
/// not "unset".
#[test]
fn a_written_ratio_overrides_value_and_origin() {
    let managed = ManagedHotspotFlags::default();
    let mut rows = vec![row("MaxHeapFreeRatio", "70"), row("MinHeapFreeRatio", "40")];
    managed.apply(&mut rows);
    assert_eq!(rows[1].value, "40");
    assert_eq!(rows[1].origin, "{default}");
    assert!(managed.set("MinHeapFreeRatio", 0));
    assert!(!managed.set("SurvivorRatio", 3), "not modelled");
    managed.apply(&mut rows);
    assert_eq!(managed.get("MinHeapFreeRatio"), Some(0));
    assert_eq!(rows[1].value, "0");
    assert_eq!(rows[1].origin, "{management}");
    assert_eq!(rows[1].jmx_origin(), "MANAGEMENT");
    assert_eq!(rows[1].jmm_origin(), 3);
    assert!(rows[1].is_manageable());
    assert_eq!(rows[0].value, "70", "the other ratio is not written");
    assert_eq!(rows[0].origin, "{default}");
}

/// The door: HotSpot's range first, then the constraint against the
/// other ratio's CURRENT value (defaults 40 / 70), then the write, which
/// `jcmd VM.flags` (non-default rows) shows. An unmodelled name is `None`.
#[test]
fn set_vm_option_checks_range_then_constraint() {
    use crate::config::VmConfig;
    use crate::runtime::serviceability::VmDiagnosticState as _;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use cratonvm_native_api::NativeSystemAccess as _;
    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "zt24x20");
    let ctx = crate::vm::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let range = |name: &str| {
        Some(Err(format!(
            "{name} error: must have value in range [0...100]"
        )))
    };
    let constraint = |name: &str| {
        Some(Err(format!(
            "{name} error: value violates its flag's constraint."
        )))
    };
    assert_eq!(ctx.set_manageable_hotspot_flag("SurvivorRatio", 4), None);
    assert_eq!(
        ctx.set_manageable_hotspot_flag("MinHeapFreeRatio", 101),
        range("MinHeapFreeRatio")
    );
    // A negative `long` is a huge `uintx`: the range, not the constraint.
    assert_eq!(
        ctx.set_manageable_hotspot_flag("MaxHeapFreeRatio", u64::MAX),
        range("MaxHeapFreeRatio")
    );
    // 71 > MaxHeapFreeRatio (70, the default).
    assert_eq!(
        ctx.set_manageable_hotspot_flag("MinHeapFreeRatio", 71),
        constraint("MinHeapFreeRatio")
    );
    assert_eq!(
        ctx.set_manageable_hotspot_flag("MinHeapFreeRatio", 70),
        Some(Ok(()))
    );
    // 69 < MinHeapFreeRatio, now 70.
    assert_eq!(
        ctx.set_manageable_hotspot_flag("MaxHeapFreeRatio", 69),
        constraint("MaxHeapFreeRatio")
    );
    let min = ctx
        .hotspot_vm_flags()
        .into_iter()
        .find(|row| row.name == "MinHeapFreeRatio")
        .expect("the modelled row");
    assert_eq!((min.value.as_str(), min.origin), ("70", "{management}"));
    assert!(shared
        .vm_flags()
        .iter()
        .any(|f| f == "-XX:MinHeapFreeRatio=70"));
}
