use super::JitIntrinsic;

/// One variant from EVERY family region, first and last of the enum
/// included. Each must fall inside the sentinel range, and `LAST` must
/// have the largest discriminant of them — the property
/// `is_sentinel_entry` rests on.
#[test]
fn sentinel_range_covers_every_family() {
    let one_per_family = [
        JitIntrinsic::MathSqrt,
        JitIntrinsic::MathMaxDouble,
        JitIntrinsic::IntRotateRight,
        JitIntrinsic::LongRotateRight,
        JitIntrinsic::AtomicIntCompareAndSet,
        JitIntrinsic::AtomicLongCompareAndSet,
        JitIntrinsic::IntegerIntValue,
        JitIntrinsic::ArraycopyPrimitive,
        JitIntrinsic::StringHashCode,
        JitIntrinsic::StringBuilderAppendInt,
        JitIntrinsic::StringIndexOfStr,
        JitIntrinsic::ArraysEquals8,
        JitIntrinsic::ArraysSortByte,
        JitIntrinsic::Crc32cUpdateBytes,
        JitIntrinsic::LongBitsToDouble,
        JitIntrinsic::FfmSegmentGetAtIndex,
        JitIntrinsic::FfmSegmentSetAtIndex,
        JitIntrinsic::ArrayListGet,
        JitIntrinsic::ArrayListSize,
        JitIntrinsic::MathRoundDouble,
        JitIntrinsic::CharIsValidCodePoint,
        JitIntrinsic::ReachabilityFence,
    ];
    for v in one_per_family {
        assert!(
            JitIntrinsic::is_sentinel_entry(v.as_entry()),
            "{v:?}'s sentinel lies below the range `is_sentinel_entry` accepts; \
             `JitIntrinsic::LAST` no longer names the last variant"
        );
        assert!(
            (v as usize) <= (JitIntrinsic::LAST as usize),
            "{v:?} has a larger discriminant than `JitIntrinsic::LAST`"
        );
    }
}

/// JIT round 17 wave 1, lane nio: the fence sentinel is its own value, the
/// lowest one (it is `LAST`), and the two site matchers the doors share
/// accept exactly the JDK triples and nothing adjacent.
#[test]
fn r17_nio_reachability_fence_sentinel_and_site_matchers() {
    let fence = JitIntrinsic::ReachabilityFence.as_entry();
    assert!(JitIntrinsic::is_sentinel_entry(fence));
    assert_ne!(fence, JitIntrinsic::ArrayListSize.as_entry());
    assert_eq!(fence, JitIntrinsic::LAST.as_entry());
    assert!(crate::is_reachability_fence_site(
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V"
    ));
    assert!(!crate::is_reachability_fence_site(
        "java/lang/ref/WeakReference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V"
    ));
    assert!(!crate::is_reachability_fence_site(
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/String;)V"
    ));
    assert!(crate::is_object_get_class_site(
        "getClass",
        "()Ljava/lang/Class;"
    ));
    assert!(!crate::is_object_get_class_site(
        "getClass",
        "()Ljava/lang/Object;"
    ));
    assert!(!crate::is_object_get_class_site(
        "getClasses",
        "()Ljava/lang/Class;"
    ));
}

#[test]
fn an_ordinary_code_address_is_not_a_sentinel() {
    for addr in [0usize, 0x1000, 0x7FFF_FFFF_FFFF, usize::MAX / 2] {
        assert!(!JitIntrinsic::is_sentinel_entry(addr), "{addr:#x}");
    }
}

/// `from_entry` still classifies the CRC32 pair, and only them.
#[test]
fn from_entry_still_classifies_only_the_crc32_family() {
    assert_eq!(
        JitIntrinsic::from_entry(JitIntrinsic::Crc32cUpdateByte.as_entry()),
        Some(JitIntrinsic::Crc32cUpdateByte)
    );
    assert_eq!(
        JitIntrinsic::from_entry(JitIntrinsic::FfmSegmentSetAtIndex.as_entry()),
        None
    );
    assert_eq!(JitIntrinsic::from_entry(0x1000), None);
}
