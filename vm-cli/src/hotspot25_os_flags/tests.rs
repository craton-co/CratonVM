use super::*;

#[test]
fn the_os_lists_do_not_overlap_the_generated_tables() {
    for f in LINUX_ONLY {
        assert!(hotspot25_flags::lookup(f.name).is_none(), "{}", f.name);
    }
    for n in WINDOWS_ONLY {
        assert!(hotspot25_flags::lookup(n).is_some(), "{n}");
    }
    for (n, _) in LINUX_ONLY_DEVELOP {
        assert!(crate::hotspot25_develop_flags::develop(n).is_none(), "{n}");
    }
}

#[test]
fn lookup_answers_for_this_os() {
    assert_eq!(lookup("UseTransparentHugePages").is_some(), ON_LINUX);
    assert_eq!(lookup("UseOSErrorReporting").is_some(), ON_WINDOWS);
    assert!(lookup("UseSerialGC").is_some());
    assert_eq!(develop("DelayThreadStartALot"), ON_LINUX.then_some(true));
    assert_eq!(develop("CountBytecodes"), Some(true));
}
