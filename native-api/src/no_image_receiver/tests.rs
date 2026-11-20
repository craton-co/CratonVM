use super::*;

/// [`receiver_declared_by_no_supported_image`] binary-searches both tables,
/// so an unsorted entry is not a style question: it makes the predicate
/// answer `false` for a name that is in the list, silently.
#[test]
fn table_is_sorted_and_unique() {
    for (label, table) in [
        ("NO_IMAGE_JDK_RECEIVERS", NO_IMAGE_JDK_RECEIVERS),
        ("VM_MINTED_STAND_IN_RECEIVERS", VM_MINTED_STAND_IN_RECEIVERS),
        ("VM_SERVICE_RECEIVERS", VM_SERVICE_RECEIVERS),
        ("STRICT_STILL_FABRICATES", STRICT_STILL_FABRICATES),
    ] {
        let mut sorted = table.to_vec();
        sorted.sort_unstable();
        assert_eq!(table, sorted.as_slice(), "{label} is not sorted");
        sorted.dedup();
        assert_eq!(table.len(), sorted.len(), "{label} has duplicates");
    }
}

/// The two `cratonvm/` tables answer opposite questions about the same
/// prefix. An overlap would make the answer depend on which one a future
/// reader edited.
#[test]
fn stand_ins_and_vm_services_are_disjoint() {
    for name in VM_SERVICE_RECEIVERS {
        assert!(
            !VM_MINTED_STAND_IN_RECEIVERS.contains(name),
            "{name} is in both the stand-in and the VM-service table"
        );
        assert!(
            !receiver_declared_by_no_supported_image(name),
            "{name} is a reviewed VM service and must keep NativeKind::Bridge"
        );
    }
}

/// The named cases, in both directions. The positives are the families the
/// 2026-08-10 sweep found on the deletion list; the negatives are the
/// nearest real classes, because a prefix or substring match here would
/// demote live bridges.
#[test]
fn named_receivers_answer_correctly() {
    for yes in [
        "java/util/HashMap$KeyItr",
        "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl",
        "java/util/TreeSet$Itr",
        "java/util/Enumeration$Impl",
        "java/util/function/Predicate$$Lambda$And",
        "sun/nio/ch/WindowsFileDispatcherImpl",
        "cratonvm/internal/ArrayListSubList",
    ] {
        assert!(receiver_declared_by_no_supported_image(yes), "{yes}");
    }
    for no in [
        // the VM-service exclusion, asserted from the other side by
        // `strict_fabricated_receivers_are_listed_but_not_retagged`
        "java/lang/reflect/Proxy$Instance",
        // the real classes whose names the entries above are built from
        "java/util/HashMap",
        "java/util/concurrent/atomic/AtomicIntegerFieldUpdater",
        "java/lang/reflect/Proxy",
        "sun/nio/ch/UnixFileDispatcherImpl",
        "sun/nio/ch/FileDispatcherImpl",
        // a real ACC_NATIVE bridge, and the platform-variant class the
        // two-platform sweep got wrong
        "sun/nio/ch/Net",
        "jdk/net/WindowsSocketOptions",
        "sun/nio/ch/KQueuePort",
        "sun/nio/fs/PollingWatchService",
        // an unlisted cratonvm/ receiver must not be caught by the prefix
        "cratonvm/internal/UnmodifiableList",
        "cratonvm/Instrument",
    ] {
        assert!(!receiver_declared_by_no_supported_image(no), "{no}");
    }
}

/// The four blocked receivers are in [`NO_IMAGE_JDK_RECEIVERS`] on purpose
/// — the image fact about them is true and the gate script should keep
/// checking it — and the predicate must still answer `false`. An exclusion
/// that worked by *omission* would be indistinguishable from an oversight,
/// and the next person to re-derive the table from a census would add them
/// back.
#[test]
fn strict_fabricated_receivers_are_listed_but_not_retagged() {
    for name in STRICT_STILL_FABRICATES {
        assert!(
            NO_IMAGE_JDK_RECEIVERS.contains(name)
                || VM_MINTED_STAND_IN_RECEIVERS.contains(name),
            "{name} is excluded by omission rather than by rule"
        );
        assert!(
            !receiver_declared_by_no_supported_image(name),
            "{name} is still fabricated under --real-jdk; re-tagging it \
             replaces a silent §5 violation with an UnsatisfiedLinkError"
        );
    }
    // Same shape for the proxy machinery, which is excluded as a VM
    // service rather than as a blocked stand-in.
    for name in [
        "java/lang/reflect/Proxy$Dispatch",
        "java/lang/reflect/Proxy$Instance",
    ] {
        assert!(NO_IMAGE_JDK_RECEIVERS.contains(&name), "{name}");
        assert!(VM_SERVICE_RECEIVERS.contains(&name), "{name}");
        assert!(
            is_reviewed_vm_service_receiver(name),
            "{name} must be recognized by native dispatch as a VM service"
        );
        assert!(!receiver_declared_by_no_supported_image(name), "{name}");
    }
}

/// `sun/nio/ch/KQueuePort` and `sun/nio/fs/PollingWatchService` are the two
/// names a linux+windows sweep called "in no image" and a macOS image has.
/// They are the reason the table is measured against six images and not
/// four, and their absence from it is the assertion that keeps that true.
#[test]
fn macos_only_classes_are_not_in_the_table() {
    for name in ["sun/nio/ch/KQueuePort", "sun/nio/fs/PollingWatchService"] {
        assert!(
            !NO_IMAGE_JDK_RECEIVERS.contains(&name),
            "{name} is declared by the macOS images and must not be demoted"
        );
    }
}
