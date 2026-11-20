use super::*;

/// zr z4/jf: the six types are registered after `jdk.GCPhaseConcurrent`
/// (earlier ids unchanged) with HotSpot's field names in HotSpot's order.
#[test]
fn zr4jf_zgc_types_have_hotspots_field_names() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let names = |n: &str| -> Vec<String> {
        let id = reg
            .find_by_name(n)
            .unwrap_or_else(|| panic!("{n} registered"));
        reg.get(id)
            .unwrap()
            .fields
            .iter()
            .map(|f| f.name.to_string())
            .collect()
    };
    assert_eq!(names("jdk.GCPhasePauseLevel1"), ["gcId", "name"]);
    assert_eq!(names("jdk.ZAllocationStall"), ["type", "size"]);
    assert_eq!(names("jdk.ZRelocationSet"), ["total", "empty", "relocate"]);
    assert_eq!(
        names("jdk.ZRelocationSetGroup"),
        [
            "type",
            "candidatePages",
            "total",
            "empty",
            "selectedPages",
            "relocate"
        ]
    );
    assert_eq!(names("jdk.ZUncommit"), ["uncommitted"]);
    assert_eq!(names("jdk.ZOldGarbageCollection"), ["gcId"]);
    let conc = reg.find_by_name("jdk.GCPhaseConcurrent").unwrap();
    let level1 = reg.find_by_name("jdk.GCPhasePauseLevel1").unwrap();
    assert!(level1.0 > conc.0, "new types are registered last");
}
