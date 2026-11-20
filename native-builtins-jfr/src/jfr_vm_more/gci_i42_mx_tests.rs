use super::{heap_pool_usage_rows, hotspot_gc_threads};

#[test]
fn gci_i42_mx_gc_thread_flags_are_hotspots_per_collector() {
    // Serial: no worker gang, both flags 0, whatever the CPU count.
    assert_eq!(hotspot_gc_threads("SerialOld", 8), Some((0, 0)));
    assert_eq!(hotspot_gc_threads("SerialOld", 64), Some((0, 0)));
    // G1: ParallelGCThreads = ncpus up to 8, then 8 + (n - 8) * 5 / 8;
    // ConcGCThreads = max((P + 2) / 4, 1).
    assert_eq!(hotspot_gc_threads("G1Old", 1), Some((1, 1)));
    assert_eq!(hotspot_gc_threads("G1Old", 4), Some((4, 1)));
    assert_eq!(hotspot_gc_threads("G1Old", 6), Some((6, 2)));
    assert_eq!(hotspot_gc_threads("G1Old", 8), Some((8, 2)));
    assert_eq!(hotspot_gc_threads("G1Old", 16), Some((13, 3)));
    assert_eq!(hotspot_gc_threads("G1Old", 32), Some((23, 6)));
    assert_eq!(hotspot_gc_threads("G1Old", 72), Some((48, 12)));
    // A non-positive count reads as one processor.
    assert_eq!(hotspot_gc_threads("G1Old", 0), Some((1, 1)));
    // Any other collector keeps the caller's figures.
    assert_eq!(hotspot_gc_threads("ZGC Major", 8), None);
    assert_eq!(hotspot_gc_threads("N/A", 8), None);
}

#[test]
fn gci_i42_mx_heap_pool_usage_is_one_row_per_heap_pool() {
    let pool = |name: &'static str, used: i64, committed: i64, max: i64| {
        cratonvm_native_api::GcMemoryPoolInfo {
            name,
            init: -1,
            used,
            committed,
            max,
            collection_used: -1,
            collection_committed: -1,
            manager_names: &[],
        }
    };
    let rows = heap_pool_usage_rows(&[
        pool("G1 Eden Space", 4 << 20, 24 << 20, -1),
        pool("G1 Survivor Space", 1 << 20, 1 << 20, -1),
        pool("G1 Old Gen", 9 << 20, 40 << 20, 256 << 20),
    ]);
    assert_eq!(
        rows,
        vec![
            ("G1 Eden Space", 4 << 20, 24 << 20, -1),
            ("G1 Survivor Space", 1 << 20, 1 << 20, -1),
            ("G1 Old Gen", 9 << 20, 40 << 20, 256 << 20),
        ]
    );
    // No pools described: no rows (the caller writes its historical row).
    assert!(heap_pool_usage_rows(&[]).is_empty());
}

/// The periodic writer reads the switch and the new context answers at
/// each of the four events it changes.
#[test]
fn gci_i42_mx_the_periodic_writer_uses_the_hotspot_answers() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src.find("\npub fn periodic(").expect("periodic");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.matches("true").count() >= 4);
    assert!(body.contains("heap_pool_usage_rows(&ctx.gc_memory_pools())"));
    assert!(body.contains("ctx.explicit_gc_flags()"));
    assert!(body.matches("ctx.hotspot_heap_sizes()").count() >= 2);
    assert!(body.contains("(\"isExplicitGCDisabled\", Val::Bool(explicit_disabled))"));
    assert!(body.contains("(\"usesCompressedOops\", Val::Bool(compressed))"));
    assert!(body.contains("(\"UseCompressedOops\", Val::Bool(compressed_oops))"));
    // Wave 137 L1b: both read the HotSpot flag model when narrow oops are
    // not in force.
    assert_eq!(body.matches("hotspot_model_compressed_oops(&ctx.hotspot_vm_flags(), old)").count(), 2);
}

/// Wave 137 L1b: the flag model's compressed-oops answer, as the property
/// (`vm_properties::hotspot_compressed_oops_mode`) and `getVMOption` give it.
#[test]
fn i137_l1b_the_model_compressed_oops_answer_is_the_propertys() {
    use super::hotspot_model_compressed_oops as model;
    use cratonvm_native_api::hotspot_flags::HotspotVmFlag;
    fn rows(coops: &str, origin: &'static str, max: u64) -> Vec<HotspotVmFlag> {
        vec![
            HotspotVmFlag {
                ty: "size_t",
                name: "MaxHeapSize",
                value: max.to_string(),
                kind: "{product}",
                origin: "{command line}",
            },
            HotspotVmFlag {
                ty: "bool",
                name: "UseCompressedOops",
                value: coops.to_string(),
                kind: "{product lp64_product}",
                origin,
            },
        ]
    }
    const G: u64 = 1 << 30;
    // `-XX:+UseG1GC -Xmx4g`: HotSpot `true`, `Zero based`, ergonomic.
    assert_eq!(
        model(&rows("true", "{ergonomic}", 4 * G), "G1Old"),
        Some((true, "Zero based", "Ergonomic"))
    );
    // `-Xmx2g` G1: unscaled.
    assert_eq!(
        model(&rows("true", "{ergonomic}", 2 * G), "G1Old"),
        Some((true, "32-bit", "Ergonomic"))
    );
    // `-Xmx31g` Serial: disjoint.
    assert_eq!(
        model(&rows("true", "{ergonomic}", 31 * G), "SerialOld"),
        Some((true, "Non-zero disjoint base", "Ergonomic"))
    );
    // `-XX:-UseCompressedOops`, and ZGC: off, base and shift 0.
    assert_eq!(
        model(&rows("false", "{command line}", 4 * G), "G1Old"),
        Some((false, "32-bit", "Command line"))
    );
    assert_eq!(
        model(&rows("false", "{ergonomic}", 4 * G), "ZGC Major"),
        Some((false, "32-bit", "Ergonomic"))
    );
    // Past the limit: `false {default}`.
    assert_eq!(
        model(&rows("false", "{default}", 40 * G), "G1Old"),
        Some((false, "32-bit", "Default"))
    );
    // No model row (`CRATONVM_VM_COMPRESSED_OOPS_MODE_HOTSPOT=0`): none.
    assert_eq!(model(&rows("true", "{ergonomic}", 4 * G)[..1], "G1Old"), None);
}

#[test]
fn gci_i42_mx_narrow_oop_mode_is_hotspots_spelling() {
    use super::hotspot_narrow_oop_mode;
    // Off (this VM's default; HotSpot `-XX:-UseCompressedOops`): base and
    // shift 0, `UnscaledNarrowOop`.
    assert_eq!(hotspot_narrow_oop_mode(false, 0, 0), "32-bit");
    assert_eq!(hotspot_narrow_oop_mode(false, 1 << 35, 3), "32-bit");
    assert_eq!(hotspot_narrow_oop_mode(true, 0, 0), "32-bit");
    assert_eq!(hotspot_narrow_oop_mode(true, 0, 3), "Zero based");
    assert_eq!(hotspot_narrow_oop_mode(true, 1 << 35, 3), "Non-zero disjoint base");
    assert_eq!(hotspot_narrow_oop_mode(true, 0x8_0000_0000 + 0x1000, 3), "Non-zero based");
}
