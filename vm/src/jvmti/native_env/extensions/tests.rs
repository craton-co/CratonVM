use super::*;

/// HotSpot 25's extension list, ids and order (MEASURED,
/// `L1W63JvmtiAgentFunctions`).
#[test]
fn the_extensions_are_hotspots_three_and_three() {
    let ids: Vec<&str> = functions().iter().map(|f| f.1).collect();
    assert_eq!(
        ids,
        [
            "com.sun.hotspot.functions.IsClassUnloadingEnabled",
            "com.sun.hotspot.functions.GetVirtualThread",
            "com.sun.hotspot.functions.GetCarrierThread",
        ]
    );
    let events: Vec<(JInt, &str)> = events().iter().map(|e| (e.0, e.1)).collect();
    assert_eq!(
        events,
        [
            (49, "com.sun.hotspot.events.ClassUnload"),
            (48, "com.sun.hotspot.events.VirtualThreadMount"),
            (47, "com.sun.hotspot.events.VirtualThreadUnmount"),
        ]
    );
}

/// gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
/// finding 3): both thread extension functions take ONE JNI entry before
/// they decode their operand, so every raw `ObjectRef` they read
/// afterwards (`thread_operand`'s answer, the registry's mirrors, the
/// carrier's global) is read out of native. (Source pin: an in-native
/// caller cannot be staged in a unit test of this module.)
#[test]
fn gci_i15rb_thread_extensions_hold_one_entry_across_the_operand() {
    let src = include_str!("../extensions.rs").replace("\r\n", "\n");
    for (f, operand) in [
        ("extern \"C\" fn get_virtual_thread(", "thread_operand(&shared, carrier)"),
        ("extern \"C\" fn get_carrier_thread(", "thread_operand(&shared, vthread)"),
    ] {
        let start = src.find(f).expect(f);
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let entry = body.find("let _fx = jni::ForeignJniEntry::enter();").expect(f);
        let decode = body.find(operand).expect(operand);
        assert!(entry < decode, "{f}: the entry comes first");
    }
}

