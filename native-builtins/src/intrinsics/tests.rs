use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn lookup_resolves_known_methods() {
    assert_eq!(
        lookup("java/lang/String", "length", "()I"),
        Some(InterpIntrinsic::StringLength)
    );
    assert_eq!(
        lookup(
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V"
        ),
        Some(InterpIntrinsic::SystemArraycopy)
    );
    assert_eq!(
        lookup("java/lang/Math", "max", "(JJ)J"),
        Some(InterpIntrinsic::MathMaxLong)
    );
}

#[test]
fn lookup_is_descriptor_exact() {
    // Wrong descriptor for an otherwise-known (class,name) must miss.
    assert_eq!(lookup("java/lang/String", "length", "()J"), None);
    // append overloads must resolve independently.
    assert_eq!(
        lookup(
            "java/lang/StringBuilder",
            "append",
            "(I)Ljava/lang/StringBuilder;"
        ),
        Some(InterpIntrinsic::StringBuilderAppendInt)
    );
    assert_eq!(
        lookup(
            "java/lang/StringBuilder",
            "append",
            "(C)Ljava/lang/StringBuilder;"
        ),
        Some(InterpIntrinsic::StringBuilderAppendChar)
    );
}

#[test]
fn lookup_misses_unknown() {
    assert_eq!(
        lookup(
            "java/lang/String",
            "concat",
            "(Ljava/lang/String;)Ljava/lang/String;"
        ),
        None
    );
    assert_eq!(lookup("com/example/Foo", "bar", "()V"), None);
}

#[test]
fn signature_prefilter_is_conservative() {
    assert!(might_have_method_descriptor("length", "()I"));
    assert!(might_have_method_descriptor("max", "(JJ)J"));
    assert!(might_have_method_descriptor(
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;"
    ));
    assert!(!might_have_method_descriptor(
        "concat",
        "(Ljava/lang/String;)Ljava/lang/String;"
    ));
    assert!(!might_have_method_descriptor("length", "()J"));
}

// `TABLE` (the module's, through `use super::*`) is the second copy of
// `lookup`'s arms the tests below cross-check; since j27 CB17-6 it is also
// what the native census publishes.

/// `is_static` and [`lookup`] must agree for every entry: the interpreter
/// picks its argument-pop helper from `is_static`, so a wrong answer reads
/// the receiver as `param0` or drops it.
#[test]
fn staticness_agrees_with_the_table() {
    for &(class, name, desc, expect_static) in TABLE {
        let kind = lookup(class, name, desc)
            .unwrap_or_else(|| panic!("{class} {name} {desc} must resolve"));
        assert_eq!(
            is_static(kind),
            expect_static,
            "{class} {name} {desc} staticness"
        );
    }
}

/// The prefilter's real invariant, G9-1: it may drop a STATIC entry (see
/// its doc), but it must never drop an INSTANCE one — the virtual dispatch
/// path consults it before `lookup` and a `false` there is final.
#[test]
fn every_instance_entry_is_admitted() {
    for &(class, name, desc, is_stat) in TABLE {
        if is_stat {
            continue;
        }
        assert!(
            might_have_method_descriptor(name, desc),
            "{class} {name} {desc} resolves through lookup() but the virtual \
             prefilter rejects it, so the fast path is unreachable"
        );
    }
}

/// The two Thread entries really are the whole of the prefilter's blind
/// spot. If this starts failing, a new static intrinsic was added and the
/// doc on `might_have_method_descriptor` needs re-reading, not deleting.
#[test]
fn the_prefilter_blind_spot_is_exactly_the_two_thread_statics() {
    let blind: Vec<&str> = TABLE
        .iter()
        .filter(|(_, name, desc, _)| !might_have_method_descriptor(name, desc))
        .map(|(_, name, _, _)| *name)
        .collect();
    assert_eq!(blind, vec!["onSpinWait", "currentThread"]);
}

/// Guards the table above against drifting behind `lookup`.
///
/// `dispatch` has one arm per `InterpIntrinsic`; the two record kinds are
/// not in `lookup` at all (they are produced per-class by
/// `vm::runtime::interpreter::dispatch_static::record_object_intrinsic`),
/// so the expected count is the enum's size minus those two.
#[test]
fn the_table_is_complete() {
    assert_eq!(TABLE.len(), 30, "lookup() arm count");
    assert_eq!(
        lookup("java/lang/Thread", "onSpinWait", "()V"),
        Some(InterpIntrinsic::ThreadOnSpinWait)
    );
    assert_eq!(lookup("java/lang/Record", "hashCode", "()I"), None);
    assert_eq!(
        lookup("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
        None
    );
}

/// STR16-5: the strict-policy filter names exactly the `StringBuilder`
/// rows of the table (whose `native_sb_*` bodies the `--real-jdk`
/// registry refuses), and never declines anything under `--compatible`.
#[test]
fn strict_policy_filter_is_exactly_the_string_builder_rows() {
    for &(class, name, desc, _) in TABLE {
        let kind = lookup(class, name, desc).expect("table row resolves");
        assert_eq!(
            is_string_builder_entry(kind),
            class == "java/lang/StringBuilder",
            "{class}.{name}{desc}"
        );
        assert!(
            !strict_policy_declines(kind, false),
            "--compatible never declines {class}.{name}{desc}"
        );
        if !is_string_builder_entry(kind) && !is_reviewed_string_entry(kind) {
            assert!(
                !strict_policy_declines(kind, true),
                "only builder and String rows are declined: {class}.{name}{desc}"
            );
        }
    }
    // With the switch armed in this environment, every builder row is
    // declined under `--real-jdk`; unarmed, none is.
    let armed = jdk_only_sb_bytecode_enabled();
    for &(class, name, desc, _) in TABLE {
        let kind = lookup(class, name, desc).expect("table row resolves");
        if is_string_builder_entry(kind) {
            assert_eq!(
                strict_policy_declines(kind, true),
                armed,
                "{class}.{name}{desc}"
            );
        }
    }
}

/// Round 17 wave 3 (lane compatbridge): the reviewed `String` rows are
/// exactly the three `java/lang/String` members of the table, never
/// declined under `--compatible`, and declined under `--real-jdk` exactly
/// when `CRATONVM_STRICT_STRING_TABLE_BYTECODE` is armed.
#[test]
fn reviewed_string_rows_are_exactly_the_string_members() {
    let armed = jdk_only_string_table_bytecode_enabled();
    for &(class, name, desc, _) in TABLE {
        let kind = lookup(class, name, desc).expect("table row resolves");
        assert_eq!(
            is_reviewed_string_entry(kind),
            class == "java/lang/String",
            "{class}.{name}{desc}"
        );
        if is_reviewed_string_entry(kind) {
            assert!(!strict_policy_declines(kind, false), "{class}.{name}{desc}");
            assert_eq!(
                strict_policy_declines(kind, true),
                armed,
                "{class}.{name}{desc}"
            );
        }
    }
}
