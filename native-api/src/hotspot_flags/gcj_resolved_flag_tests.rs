use super::HotspotVmFlag;

#[test]
fn each_resolved_type_and_origin_selects_its_jfr_representation() {
    let mut flag = HotspotVmFlag { ty: "int", name: "MaxJavaStackTraceDepth",
        value: "23".into(), kind: "{product}", origin: "{command line}" };
    for (ty, event) in [("bool", "jdk.BooleanFlag"), ("int", "jdk.IntFlag"),
        ("uint", "jdk.UnsignedIntFlag"), ("intx", "jdk.LongFlag"),
        ("uintx", "jdk.UnsignedLongFlag"), ("size_t", "jdk.UnsignedLongFlag"),
        ("uint64_t", "jdk.UnsignedLongFlag"), ("double", "jdk.DoubleFlag"),
        ("ccstr", "jdk.StringFlag"), ("ccstrlist", "jdk.StringFlag")] {
        flag.ty = ty;
        assert_eq!(flag.jfr_event_name(), Some(event));
    }
    flag.ty = "unsupported";
    assert_eq!(flag.jfr_event_name(), None);
    for (origin, jfr) in [("{default}", "Default"), ("{command line}", "Command line"),
        ("{ergonomic}", "Ergonomic"), ("{management}", "Management")] {
        flag.origin = origin;
        assert_eq!(flag.jfr_origin(), jfr);
    }
    flag.origin = "{command line, ergonomic}";
    assert_eq!(flag.jfr_origin(), "Ergonomic");
    assert_eq!(flag.jmm_origin(), 6);
    assert_eq!(flag.jmx_origin(), "ERGONOMIC");
}
