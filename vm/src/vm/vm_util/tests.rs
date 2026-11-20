use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

fn test_shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

/// Round 13 wave 10 (lane spin): `finalize_class_init` releases the
/// `class_init_waiters` mutex BEFORE its `ClassManagerWriteGuard` drops.
/// The guard's `Drop` re-takes `class_manager.read()` (deferred-`new`
/// resweep, profile-replay seed), and a claimant in
/// `ensure_class_initialized_shared` holds the write lock while it takes
/// `class_init_waiters`; with the removal as the block's TAIL expression
/// (whose temporary outlives `cm` in edition 2021) the two deadlocked --
/// `R13CodecacheDiscardChurn`'s 1-in-15 `--compatible` hang. A source pin,
/// because the behavioural reproduction needs a held deferred-`new` retry
/// and loses the race most of the time.
#[test]
fn finalize_class_init_releases_the_waiter_mutex_before_the_write_guard_drops() {
    let src = include_str!("../vm_util.rs");
    let start = src
        .find("\nfn finalize_class_init(")
        .unwrap_or_else(|| panic!("finalize_class_init not found"));
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
    // A `let` statement (wave 62 made it conditional: a claim lost to a
    // stub upgrade leaves a later claim's entry in place); its
    // temporaries end at its `;`.
    let removal = body
        .find("let removed = if release_waiter {\n            shared.classes.class_init_waiters.lock().remove(&class_id)\n        } else {\n            None\n        };")
        .unwrap_or_else(|| panic!("the waiter removal must be a `let` statement"));
    let drop_cm = body
        .find("drop(cm);")
        .unwrap_or_else(|| panic!("the write guard must be dropped explicitly"));
    assert!(
        removal < drop_cm,
        "the waiter mutex must be released before the class-manager write guard drops"
    );
}

/// The real bytes the stub-upgrade tests define over a stub named `Foo`.
const UPGRADE_FOO: &[u8] =
    include_bytes!("../../../../classloading/tests/fixtures/wp2_4b_redefine/Foo.v1.class");

/// A stub `Foo` claimed for initialization by `claimant`, the way
/// `ensure_class_initialized_shared` claims it, then upgraded in place to
/// real bytes (`--compatible`): the race of
/// `interpreter-L5-a-stub-upgrade-during-the-stubs-own-initialization-is-lost-FIXED-20261026.md`,
/// interleaved by hand. Returns the class and the claim's waiter.
fn claimed_stub_upgraded_in_place(
    shared: &SharedVm,
    claimant: u64,
) -> (
    ClassId,
    Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
) {
    let id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("Foo", 0)
        .expect("Compatible mode fabricates");
    {
        let mut cm = shared.classes.class_manager_write();
        let class = cm.get_class_mut(id).expect("just fabricated");
        class.initializing_thread = Some(claimant);
        class.state = ClassState::Initializing;
        register_init_waiter(shared, id);
    }
    let waiter = shared
        .classes
        .class_init_waiters
        .lock()
        .get(&id)
        .cloned()
        .expect("the claim registered its waiter");
    let upgraded = shared
        .classes
        .class_manager
        .write()
        .define_class(
            "Foo",
            UPGRADE_FOO,
            cratonvm_types::ClassLoaderId::UserDefined(62),
        )
        .expect("real bytes upgrade the stub");
    assert_eq!(upgraded, id, "the upgrade keeps the stub's ClassId");
    (id, waiter)
}

/// Interpreter round i2 wave 62, lane L5: a claim that initialized the
/// stub and finalizes after the upgrade must not mark the REAL class
/// initialized (its `<clinit>` never ran). Before the fix the finalize
/// wrote `Initialized`, `CLASS_INIT_INITIALIZED` and the bitmap bit, the
/// predicted failure of the first assertion (positive control: the same
/// finalize with the claim held publishes, see
/// `a_held_claim_still_publishes_initialized`).
#[test]
fn a_claim_lost_to_a_stub_upgrade_does_not_publish_initialized() {
    let shared = test_shared();
    let (id, waiter) = claimed_stub_upgraded_in_place(&shared, 7);
    assert!(
        !finalize_class_init(&shared, id, ClassState::Initialized, 7),
        "the claim was lost to the upgrade"
    );
    {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(id).expect("still registered");
        assert_eq!(
            class.state,
            ClassState::Loaded,
            "left as the upgrade set it"
        );
        assert_eq!(class.initializing_thread, None);
        assert!(!is_class_initialized_fast(class));
    }
    assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
    assert!(!is_class_initialized_via_manager(&shared, id));
    assert!(
        *waiter.0.lock(),
        "nobody holds the class: the lost claim releases its waiters"
    );
    assert!(!shared.classes.class_init_waiters.lock().contains_key(&id));
    assert!(
        init_claim_was_lost(&shared, id),
        "the caller retries, for the real <clinit>"
    );
}

/// The positive control of the test above: the same claim, finalized
/// while it still holds the class, publishes `Initialized`.
#[test]
fn a_held_claim_still_publishes_initialized() {
    let shared = test_shared();
    let id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/L5W62HeldClaim", 0)
        .expect("Compatible mode fabricates");
    {
        let mut cm = shared.classes.class_manager_write();
        let class = cm.get_class_mut(id).expect("just fabricated");
        class.initializing_thread = Some(7);
        class.state = ClassState::Initializing;
        register_init_waiter(&shared, id);
    }
    assert!(finalize_class_init(&shared, id, ClassState::Initialized, 7));
    assert!(is_class_initialized_via_manager(&shared, id));
    assert!(!init_claim_was_lost(&shared, id));
}

/// A lost claim whose class another thread has claimed since leaves that
/// claim alone, and its waiters with it: the later claim took over the
/// waiter entry (`register_init_waiter`) and releases them at its own
/// finalize.
#[test]
fn a_claim_lost_to_a_stub_upgrade_leaves_a_later_claim_alone() {
    let shared = test_shared();
    let (id, waiter) = claimed_stub_upgraded_in_place(&shared, 7);
    {
        let mut cm = shared.classes.class_manager_write();
        let class = cm.get_class_mut(id).expect("still registered");
        class.initializing_thread = Some(8);
        register_init_waiter(&shared, id);
    }
    assert!(!finalize_class_init(
        &shared,
        id,
        ClassState::Initialized,
        7
    ));
    {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(id).expect("still registered");
        assert_eq!(class.initializing_thread, Some(8), "the later claim stands");
        assert_eq!(class.state, ClassState::Loaded);
    }
    assert!(
        !*waiter.0.lock(),
        "not released before the later claim ends"
    );
    let shared_entry = shared
        .classes
        .class_init_waiters
        .lock()
        .get(&id)
        .cloned()
        .expect("the later claim's entry stays");
    assert!(
        Arc::ptr_eq(&shared_entry, &waiter),
        "the later claim took over the lost claim's waiter entry"
    );
    assert!(finalize_class_init(&shared, id, ClassState::Initialized, 8));
    assert!(*waiter.0.lock(), "the later claim's finalize releases them");
}

/// A VM with `CRATONVM_CLASS_INIT_BIT_FIRST` set to `on` (the field,
/// whatever the environment says).
fn shared_with_bit_first(on: bool) -> Arc<SharedVm> {
    let mut config = VmConfig::default();
    config.class_init_bit_first = on;
    Arc::new(SharedVm::new(config))
}

/// Interpreter round i3 wave 71, lane L3 (`VmConfig::class_init_bit_first`):
/// with the switch on the bitmap is asked FIRST. A bit set on a class whose
/// `init_state` was never marked (only the bitmap says "initialized") is
/// answered `true` with the switch on and `false` with it off (the memo
/// and the locked read see an uninitialized class): the positive control
/// that the answer came from the bit.
#[test]
fn bit_first_answers_from_the_initialized_bitmap() {
    for on in [false, true] {
        let shared = shared_with_bit_first(on);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/L3W71BitOnly", 0)
            .expect("Compatible mode fabricates");
        let generation = shared.classes.class_init_bits.reset_generation();
        shared
            .classes
            .class_init_bits
            .set_as_of(id.as_u32(), generation);
        assert_eq!(
            is_class_initialized_via_manager(&shared, id),
            on,
            "bit_first={on}: only the switch makes the bit the answer"
        );
    }
}

/// With the switch on, the locked read that finds a class INITIALIZED
/// which no `finalize_class_init` marked fills its bit; with it off the
/// bitmap is left as it was (today's behaviour).
#[test]
fn bit_first_fills_the_bit_from_the_locked_read() {
    for on in [false, true] {
        let shared = shared_with_bit_first(on);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/L3W71Fill", 0)
            .expect("Compatible mode fabricates");
        shared
            .classes
            .class_manager
            .read()
            .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
        assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
        assert!(is_class_initialized_via_manager(&shared, id));
        assert_eq!(shared.classes.class_init_bits.is_set(id.as_u32()), on);
    }
}

/// The premise the switch rests on: a stub upgraded in place clears the
/// bit the first read filled, so the bit-first answer is `false` after
/// the upgrade (the real `<clinit>` has not run). Positive control: the
/// same class answers `true` from the bit before the upgrade.
#[test]
fn bit_first_forgets_a_stub_upgraded_in_place() {
    let shared = shared_with_bit_first(true);
    let id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("Foo", 0)
        .expect("Compatible mode fabricates");
    shared
        .classes
        .class_manager
        .read()
        .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
    assert!(is_class_initialized_via_manager(&shared, id));
    assert!(
        shared.classes.class_init_bits.is_set(id.as_u32()),
        "the read filled the bit"
    );
    let upgraded = shared
        .classes
        .class_manager
        .write()
        .define_class(
            "Foo",
            UPGRADE_FOO,
            cratonvm_types::ClassLoaderId::UserDefined(71),
        )
        .expect("real bytes upgrade the stub");
    assert_eq!(upgraded, id, "the upgrade keeps the stub's ClassId");
    assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
    assert!(
        !is_class_initialized_via_manager(&shared, id),
        "the upgraded class has not run its <clinit>"
    );
}

/// `finalize_class_init` sets the bit itself, so a class initialized the
/// normal way answers from the bit at once with the switch on.
#[test]
fn bit_first_answers_a_finalized_class_from_its_bit() {
    let shared = shared_with_bit_first(true);
    let id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/L3W71Finalized", 0)
        .expect("Compatible mode fabricates");
    {
        let mut cm = shared.classes.class_manager_write();
        let class = cm.get_class_mut(id).expect("just fabricated");
        class.initializing_thread = Some(71);
        class.state = ClassState::Initializing;
        register_init_waiter(&shared, id);
    }
    assert!(finalize_class_init(
        &shared,
        id,
        ClassState::Initialized,
        71
    ));
    assert!(shared.classes.class_init_bits.is_set(id.as_u32()));
    assert!(is_class_initialized_via_manager(&shared, id));
}

/// Interpreter round i3 wave 72, lane L3
/// (`i71-L3-proposal-retire-the-per-thread-class-init-memo`): with the
/// bitmap on, a covered id is answered without the per-thread memo --
/// the locked read fills the bit and leaves the memo empty; with the
/// switch off (`=0`) the memo still serves (the read fills it, and the
/// bitmap is left alone).
#[test]
fn the_memo_serves_only_with_the_bitmap_off() {
    for on in [false, true] {
        let shared = shared_with_bit_first(on);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/L3W72MemoOnlyOff", 0)
            .expect("Compatible mode fabricates");
        let raw = id.as_u32();
        assert!(shared.classes.class_init_bits.covers(raw));
        shared
            .classes
            .class_manager
            .read()
            .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
        let generation = shared.classes.class_init_bits.reset_generation();
        assert!(is_class_initialized_via_manager(&shared, id));
        assert_eq!(
            class_init_memo_hit(shared.vm_identity, raw, generation),
            !on,
            "bit_first={on}: only the switch-off path fills the memo"
        );
        assert_eq!(shared.classes.class_init_bits.is_set(raw), on);
    }
}

/// The bitmap's capacity bounds what it covers: ids past it keep the
/// memo with the switch on (`ClassInitBits::covers`).
#[test]
fn the_bitmap_covers_ids_below_its_capacity_only() {
    let bits = cratonvm_classloading::ClassInitBits::new();
    assert!(bits.covers(0));
    assert!(bits.covers((1 << 20) - 1));
    assert!(!bits.covers(1 << 20));
    assert!(!bits.covers(u32::MAX));
}

/// A claim lost to an upgrade while it links does not move the real
/// class's state: `link_claimed_class` writes nothing, so the class
/// stays `Loaded` for the next claim to link from scratch.
#[test]
fn a_claim_lost_to_a_stub_upgrade_does_not_link_the_real_class() {
    let shared = test_shared();
    let (id, _waiter) = claimed_stub_upgraded_in_place(&shared, 7);
    let mut thread = JvmThread::new(ThreadId(7), "lost-claim");
    link_claimed_class(&shared, &mut thread, id, None).expect("a lost claim is not an error");
    assert_eq!(
        shared
            .classes
            .class_manager
            .read()
            .get_class(id)
            .map(|c| c.state),
        Some(ClassState::Loaded)
    );
}

// -----------------------------------------------------------------------
// default_value_for_descriptor
// -----------------------------------------------------------------------

#[test]
fn default_byte() {
    assert_eq!(default_value_for_descriptor("B"), Value::Int(0));
}

#[test]
fn default_char() {
    assert_eq!(default_value_for_descriptor("C"), Value::Int(0));
}

#[test]
fn default_int() {
    assert_eq!(default_value_for_descriptor("I"), Value::Int(0));
}

#[test]
fn default_short() {
    assert_eq!(default_value_for_descriptor("S"), Value::Int(0));
}

#[test]
fn default_boolean() {
    assert_eq!(default_value_for_descriptor("Z"), Value::Int(0));
}

#[test]
fn default_long() {
    assert_eq!(default_value_for_descriptor("J"), Value::Long(0));
}

#[test]
fn default_float() {
    assert_eq!(default_value_for_descriptor("F"), Value::Float(0.0));
}

#[test]
fn default_double() {
    assert_eq!(default_value_for_descriptor("D"), Value::Double(0.0));
}

#[test]
fn default_object_ref() {
    assert_eq!(
        default_value_for_descriptor("Ljava/lang/String;"),
        Value::Object(None)
    );
}

#[test]
fn default_array_ref() {
    assert_eq!(default_value_for_descriptor("[I"), Value::Object(None));
}

#[test]
fn default_multi_array_ref() {
    assert_eq!(
        default_value_for_descriptor("[[Ljava/lang/Object;"),
        Value::Object(None)
    );
}

#[test]
fn default_empty_descriptor() {
    // Edge case: empty descriptor falls to default match
    assert_eq!(default_value_for_descriptor(""), Value::Int(0));
}

#[test]
fn default_unknown_descriptor() {
    assert_eq!(default_value_for_descriptor("X"), Value::Int(0));
}

// -----------------------------------------------------------------------
// resolve_constant_value
// -----------------------------------------------------------------------

#[test]
fn resolve_integer() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Integer(42)];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Int(42)));
}

#[test]
fn resolve_float() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Float(1.5)];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Float(1.5)));
}

#[test]
fn resolve_long() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Long(9999999.into()),
        ConstantPoolEntry::Tombstone,
    ];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Long(9999999)));
}

#[test]
fn resolve_double() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Double(3.25.into()),
        ConstantPoolEntry::Tombstone,
    ];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Double(3.25)));
}

#[test]
fn resolve_string_ref_returns_none() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("hello".into()),
        ConstantPoolEntry::StringReference { string_index: 1 },
    ];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 2), None);
}

#[test]
fn resolve_out_of_bounds() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![ConstantPoolEntry::Tombstone];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 99), None);
}

#[test]
fn resolve_negative_integer() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Integer(-1)];
    let cp = ConstantPool::new(entries);
    assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Int(-1)));
}

// -----------------------------------------------------------------------
// ensure_class_initialized_shared
// -----------------------------------------------------------------------

#[test]
fn ensure_initialized_nonexistent_class() {
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // ClassId that doesn't exist -- state defaults to Loaded, triggers init
    let result = ensure_class_initialized_shared(&shared, &mut thread, ClassId::new(999));
    // This should fail because class 999 doesn't exist
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// ClassStoreHierarchy
// -----------------------------------------------------------------------

#[test]
fn hierarchy_same_class_is_subclass() {
    let shared = test_shared();
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    assert!(hierarchy.is_subclass("java/lang/Object", "java/lang/Object"));
}

#[test]
fn hierarchy_everything_is_subclass_of_object() {
    let shared = test_shared();
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    assert!(hierarchy.is_subclass("java/lang/String", "java/lang/Object"));
    assert!(hierarchy.is_subclass("java/io/PrintStream", "java/lang/Object"));
}

#[test]
fn hierarchy_direct_superclass_uses_linked_class_edge() {
    let shared = test_shared();
    let mut cm = shared.classes.class_manager_write();
    cm.load_class("java/lang/String").unwrap();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    assert!(hierarchy.is_direct_superclass("java/lang/String", "java/lang/Object"));
    assert!(!hierarchy.is_direct_superclass("java/lang/String", "java/io/Serializable"));
}

#[test]
fn hierarchy_common_superclass_same() {
    let shared = test_shared();
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    assert_eq!(
        hierarchy.common_superclass("java/lang/Object", "java/lang/Object"),
        "java/lang/Object"
    );
}

#[test]
fn hierarchy_common_superclass_unknown_returns_object() {
    let shared = test_shared();
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    // For unknown classes, common superclass should be Object
    assert_eq!(
        hierarchy.common_superclass("com/unknown/A", "com/unknown/B"),
        "java/lang/Object"
    );
}

#[test]
fn hierarchy_common_superclass_xstream_exception_siblings() {
    let shared = test_shared();
    {
        let mut cm = shared.classes.class_manager_write();
        let parent = cm
            .try_ensure_synthetic_class(
                "com/thoughtworks/xstream/converters/ErrorWritingException",
                0,
            )
            .unwrap();
        for name in [
            "com/thoughtworks/xstream/converters/reflection/ObjectAccessException",
            "com/thoughtworks/xstream/converters/ConversionException",
        ] {
            let child = cm.try_ensure_synthetic_class(name, 0).unwrap();
            cm.set_superclass(child, Some(parent));
        }
    }
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    assert_eq!(
        hierarchy.common_superclass(
            "com/thoughtworks/xstream/converters/reflection/ObjectAccessException",
            "com/thoughtworks/xstream/converters/ConversionException",
        ),
        "com/thoughtworks/xstream/converters/ErrorWritingException"
    );
}

#[test]
fn hierarchy_is_interface_unknown() {
    let shared = test_shared();
    let cm = shared.classes.class_manager.read();
    let hierarchy = ClassStoreHierarchy {
        store: &cm.class_store,
        app_workarounds: &[],
    };
    use crate::classloading::vtype::ClassHierarchy;
    // Unknown class should not be considered an interface
    assert!(!hierarchy.is_interface("com/unknown/Foo"));
}

// -----------------------------------------------------------------------
// prepare_class_shared вЂ” ConstantValue initialization (JVMS В§5.5 step 9)
// -----------------------------------------------------------------------

#[test]
fn soft_reference_clock_preparation_respects_loader_and_existing_writes() {
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::field::ClassFileField;
    use cratonvm_types::ClassLoaderId;
    for (name, loader, descriptor, seeded) in [
        ("java/lang/ref/SoftReference", ClassLoaderId::Bootstrap, "J", true),
        ("java/lang/ref/SoftReference", ClassLoaderId::Application, "J", false),
        ("OtherClock", ClassLoaderId::Bootstrap, "J", false),
        ("java/lang/ref/SoftReference", ClassLoaderId::Bootstrap, "I", false),
    ] {
        let shared = test_shared();
        let clock = shared
            .mem
            .ref_processor
            .lock()
            .soft_clock_for_java_initialization();
        assert!(clock > 0);
        let id = {
            let mut cm = shared.classes.class_manager_write();
            let mut class = make_malformed_class(name, loader);
            class.id = cm.class_store.next_id();
            class.state = ClassState::Verified;
            class.access_flags = ClassAccessFlags::PUBLIC;
            class.fields = vec![ClassFileField {
                access_flags: FieldAccessFlags::STATIC,
                name: Arc::from("clock"),
                descriptor: Arc::from(descriptor),
                attributes: Vec::new(),
            }];
            let id = class.id;
            cm.class_store.add(class);
            id
        };
        prepare_class_shared(&shared, id).unwrap();
        let expected = if seeded {
            Value::Long(clock as i64)
        } else {
            default_value_for_descriptor(descriptor)
        };
        assert_eq!(super::super::get_static_shared(&shared, id, 0), expected);
        if seeded {
            for earlier_write in [7, 0] {
                super::super::set_static_shared(&shared, id, 0, Value::Long(earlier_write));
                prepare_class_shared(&shared, id).unwrap();
                assert_eq!(
                    super::super::get_static_shared(&shared, id, 0),
                    Value::Long(earlier_write)
                );
            }
        }
    }
}

#[test]
fn prepare_class_shared_applies_constant_values_for_all_supported_types() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::Attribute;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::field::ClassFileField;

    // Constant pool layout (1-based):
    //   #1 Integer(42)         -> for IConst
    //   #2 Float(2.5)          -> for FConst
    //   #3 Long(0xDEAD_BEEF_CAFE_BABEu64 as i64)
    //   #4 Tombstone           (Long takes 2 slots)
    //   #5 Double(3.141)
    //   #6 Tombstone           (Double takes 2 slots)
    //   #7 Utf8("hello")
    //   #8 StringReference{string_index: 7}
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(42),
        ConstantPoolEntry::Float(2.5),
        ConstantPoolEntry::Long((0xDEAD_BEEF_CAFE_BABEu64 as i64).into()),
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Double(3.141.into()),
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("hello".into()),
        ConstantPoolEntry::StringReference { string_index: 7 },
    ]);

    let make_static_field = |name: &str, descriptor: &str, cv_index: u16| ClassFileField {
        access_flags: FieldAccessFlags::PUBLIC
            | FieldAccessFlags::STATIC
            | FieldAccessFlags::FINAL,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(descriptor),
        attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
            Attribute::ConstantValue {
                constant_value_index: cv_index,
            },
        )],
    };

    let fields = vec![
        make_static_field("I_CONST", "I", 1),
        make_static_field("F_CONST", "F", 2),
        make_static_field("L_CONST", "J", 3),
        make_static_field("D_CONST", "D", 5),
        make_static_field("S_CONST", "Ljava/lang/String;", 8),
        // Plain static field (no ConstantValue) вЂ” must keep its
        // descriptor-derived default after prepare runs.
        ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
            name: Arc::from("PLAIN_LONG"),
            descriptor: Arc::from("J"),
            attributes: Vec::new(),
        },
    ];

    let shared = test_shared();
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("TestCV"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Verified,
            initializing_thread: None,
            constant_pool: cp,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    };

    prepare_class_shared(&shared, class_id).expect("prepare_class_shared");

    // Verify each static slot got the right ConstantValue (slots are
    // indexed by position among static fields, i.e. 0..6).
    let statics = shared.classes.statics.read();
    let slots = statics.get(&class_id).expect("statics for class");

    assert_eq!(slots[0], Value::Int(42), "I_CONST");
    assert_eq!(slots[1], Value::Float(2.5), "F_CONST");
    assert_eq!(
        slots[2],
        Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64),
        "L_CONST"
    );
    assert_eq!(slots[3], Value::Double(3.141), "D_CONST");

    // S_CONST: must be a non-null String reference.
    match slots[4] {
        Value::Object(Some(obj_ref)) => {
            let text = super::super::vm_object::read_java_string(&shared.mem.heap, obj_ref)
                .expect("read string");
            assert_eq!(text, "hello", "S_CONST text");
        }
        other => panic!("expected non-null Object for S_CONST, got {other:?}"),
    }

    // PLAIN_LONG: no ConstantValue attribute, must remain Long(0) (zero
    // default for a J descriptor) вЂ” not Object(None) and not Int(0).
    assert_eq!(slots[5], Value::Long(0), "PLAIN_LONG default");
}

/// A static written before its class is prepared (`set_static_shared`
/// makes the block then) survives preparation, as it survives linking on
/// HotSpot; a `ConstantValue` seed is still assigned. Interpreter round i2
/// wave 56, lane L4 (`ClassSpecializer`'s `BMH_SPECIES` store).
#[test]
fn a_static_written_before_preparation_survives_it() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::Attribute;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::field::ClassFileField;

    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(42),
    ]);
    let fields = vec![
        ClassFileField {
            access_flags: FieldAccessFlags::STATIC,
            name: Arc::from("WRITTEN"),
            descriptor: Arc::from("J"),
            attributes: Vec::new(),
        },
        ClassFileField {
            access_flags: FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
            name: Arc::from("SEEDED"),
            descriptor: Arc::from("I"),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::ConstantValue {
                    constant_value_index: 1,
                },
            )],
        },
        ClassFileField {
            access_flags: FieldAccessFlags::STATIC,
            name: Arc::from("UNTOUCHED"),
            descriptor: Arc::from("J"),
            attributes: Vec::new(),
        },
    ];
    let shared = test_shared();
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("TestEarlyStatic"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Verified,
            initializing_thread: None,
            constant_pool: cp,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    };

    super::super::vm_object::set_static_shared(&shared, class_id, 0, Value::Long(7));
    prepare_class_shared(&shared, class_id).expect("prepare_class_shared");

    let statics = shared.classes.statics.read();
    let slots = statics.get(&class_id).expect("statics for class");
    assert_eq!(slots[0], Value::Long(7), "the early write survives");
    assert_eq!(slots[1], Value::Int(42), "the ConstantValue is assigned");
    assert_eq!(
        slots[2],
        Value::Long(0),
        "an untouched static keeps its default"
    );
}

// -----------------------------------------------------------------------
// verifier_skip_eligible — HIGH-severity security gate
//
// The Pass 2/3 verifier may only be skipped when BOTH (a) the class
// was loaded by the bootstrap loader AND (b) its name is in a
// trusted JDK prefix. A user-classpath class that merely *names*
// itself `java/...` must NOT bypass verification: combined with the
// interpreter's `_unchecked` operand-stack helpers, an unverified
// trusted-name class is a sandbox escape.
// -----------------------------------------------------------------------

/// Build a minimal `Class` with the given name + loader id for predicate
/// and end-to-end tests. Uses FINAL+ABSTRACT (a JVMS Г‚В§4.1
/// "cannot be both" violation) so that if structural verification
/// runs it will surface a `LinkageError::ClassFormatError`.
fn make_malformed_class(name: &str, loader_id: cratonvm_types::ClassLoaderId) -> Class {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::ConstantPool;
    Class {
        id: crate::classloading::ClassId::new(0),
        loader_id,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![]),
        // FINAL + ABSTRACT is rejected by verify_class_access_flags.
        access_flags: ClassAccessFlags::PUBLIC
            | ClassAccessFlags::FINAL
            | ClassAccessFlags::ABSTRACT,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    }
}

#[test]
fn skip_predicate_user_classpath_java_lang_evil_string_is_not_eligible() {
    // User classpath defining a `java/lang/EvilString`: loader is
    // Application, name has a trusted prefix — predicate MUST refuse.
    let c = make_malformed_class(
        "java/lang/EvilString",
        cratonvm_types::ClassLoaderId::Application,
    );
    assert!(
        !verifier_skip_eligible(&c),
        "user-classpath java/lang/EvilString must NOT skip verification"
    );
}

#[test]
fn skip_predicate_bootstrap_java_lang_string_is_eligible() {
    // Real bootstrap-loaded JDK class: skip is the documented
    // perf invariant (HotSpot -Xverify:remote behaviour).
    let c = make_malformed_class("java/lang/String", cratonvm_types::ClassLoaderId::Bootstrap);
    assert!(
        verifier_skip_eligible(&c),
        "bootstrap-loaded java/lang/String must remain eligible for skip"
    );
}

#[test]
fn skip_predicate_bootstrap_application_class_is_not_eligible() {
    // Untrusted prefix even though loaded by bootstrap: skip refuses.
    let c = make_malformed_class("com/example/Foo", cratonvm_types::ClassLoaderId::Bootstrap);
    assert!(!verifier_skip_eligible(&c));
}

#[test]
fn skip_predicate_user_defined_loader_trusted_prefix_is_not_eligible() {
    // Defensive: a UserDefined(_) loader claiming a trusted prefix
    // also must NOT bypass verification.
    let c = make_malformed_class(
        "sun/misc/EvilUnsafe",
        cratonvm_types::ClassLoaderId::UserDefined(7),
    );
    assert!(!verifier_skip_eligible(&c));
}

#[test]
fn end_to_end_user_classpath_trusted_name_triggers_verifier() {
    // End-to-end: install a FINAL+ABSTRACT class named
    // `java/lang/EvilString` with loader=Application into a real
    // ClassStore and run `ensure_class_initialized_shared`.
    // Verification MUST run and surface a Linkage error
    // (`ClassFormatError` from `verify_class_access_flags`).
    use crate::classloading::ClassLoaderId;
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        let mut c = make_malformed_class("java/lang/EvilString", ClassLoaderId::Application);
        c.id = id;
        cm.class_store.add(c);
        id
    };
    let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
    let err = result.expect_err("verifier MUST reject user-classpath java/lang/EvilString");
    match err {
        MethodCallFailed::InternalError(VmError::Linkage(_)) => { /* expected */ }
        other => {
            panic!("expected MethodCallFailed::InternalError(VmError::Linkage), got {other:?}")
        }
    }
}

#[test]
fn clinit_failure_clears_claim_and_removes_waiter() {
    // Regression: an early-return failure inside `initialize_class_shared`
    // (here the bytecode/structural verification failure path) must fully
    // finalize the class via the `InitCleanupGuard` — NOT leak the init
    // claim + waiter. Before the guard, the verify-error path set
    // `state = InitializationError` directly but left `initializing_thread`
    // set and the `class_init_waiters` entry in place, so other threads
    // blocked on the class were never notified and waited out the full
    // timeout.
    use crate::classloading::ClassLoaderId;
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        // FINAL+ABSTRACT user-classpath class -> verification runs and fails.
        let mut c = make_malformed_class("com/example/Boom", ClassLoaderId::Application);
        c.id = id;
        cm.class_store.add(c);
        id
    };

    let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
    assert!(
        result.is_err(),
        "malformed user class must fail initialization; got {result:?}"
    );

    // (1) A LINK failure parks the class unlinked (HotSpot leaves an
    // unlinkable class unlinked, not erroneous), and (2) the init claim is
    // cleared.
    {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id).expect("class still registered");
        assert_eq!(
            class.state, LINK_FAILED_STATE,
            "a link failure must park the class unlinked, not InitializationError"
        );
        assert_eq!(
            class.initializing_thread, None,
            "failed init must clear the initializing_thread claim (leak guard)"
        );
    }

    // (3) The waiter entry is removed so blocked threads are notified
    // immediately rather than waiting out the timeout. This is the core of
    // the leak the guard fixes.
    assert!(
        !shared
            .classes
            .class_init_waiters
            .lock()
            .contains_key(&class_id),
        "failed init must remove the class_init_waiters entry (waiter leak)"
    );

    // (4) A subsequent init attempt links again and fails with the SAME
    // linkage error (not "Could not initialize class"), and cleans up again.
    let again = ensure_class_initialized_shared(&shared, &mut thread, class_id);
    match again {
        Err(MethodCallFailed::InternalError(VmError::Linkage(_))) => {}
        other => {
            panic!("re-init after a link failure must relink and rethrow; got {other:?}")
        }
    }
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id).expect("class still registered");
    assert!(class_link_failed(class));
}

#[test]
fn a_superclass_link_failure_leaves_the_subclass_retryable() {
    // HotSpot links a class's superclass as part of linking the class, so a
    // superclass that fails to link is a link failure of the subclass too:
    // both stay unlinked and a second use rethrows the linkage error.
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let (super_id, sub_id) = {
        let mut cm = shared.classes.class_manager_write();
        let super_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: super_id,
            // ANNOTATION without INTERFACE fails Pass 2 but is not FINAL,
            // so the subclass's own Pass 2 (`cannot extend final class`)
            // passes.
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::ANNOTATION,
            ..make_malformed_class("com/example/BadSuper", ClassLoaderId::Application)
        });
        let sub_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sub_id,
            access_flags: ClassAccessFlags::PUBLIC,
            superclass: Some(super_id),
            ..make_malformed_class("com/example/GoodSub", ClassLoaderId::Application)
        });
        (super_id, sub_id)
    };
    for attempt in 0..2 {
        match ensure_class_initialized_shared(&shared, &mut thread, sub_id) {
            Err(MethodCallFailed::InternalError(VmError::Linkage(_))) => {}
            other => {
                panic!(
                    "attempt {attempt}: expected the superclass's linkage error, got {other:?}"
                )
            }
        }
        let cm = shared.classes.class_manager.read();
        for id in [super_id, sub_id] {
            let class = cm.get_class(id).expect("class still registered");
            assert!(
                class_link_failed(class),
                "attempt {attempt}: {}",
                class.name
            );
        }
    }
}

/// Wave 8, L5: a supertype that failed to link and that another thread
/// has claimed since, to link it again, is still a LINK failure of the
/// subtype (not an initialization failure that would make it erroneous).
#[test]
fn a_supertype_relinked_by_another_thread_still_failed_to_link() {
    use crate::classloading::ClassLoaderId;
    let shared = test_shared();
    let super_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            state: LINK_FAILED_STATE,
            initializing_thread: Some(4242),
            ..make_malformed_class("com/example/Relinking", ClassLoaderId::Application)            });
        id
    };
    assert!(supertype_failed_to_link(&shared, super_id));
    {
        let mut cm = shared.classes.class_manager_write();
        let class = cm.get_class_mut(super_id).expect("registered");
        class.state = ClassState::InitializationError;
        class.initializing_thread = None;
    }
    assert!(!supertype_failed_to_link(&shared, super_id), "erroneous");
}

/// Wave 8, L5: many threads racing to initialize a class whose link keeps
/// failing. A parked link failure is claimable again at once, so a new
/// claim can land between one claimant's release and its waiter-entry
/// removal; before the removal moved under the class-manager write lock,
/// that dropped the old entry un-notified and its waiters slept out the
/// 30 s timeout. Every attempt must fail fast with the linkage error.
#[test]
fn racing_relinks_of_a_link_failing_class_never_strand_a_waiter() {
    use crate::classloading::ClassLoaderId;
    const THREADS: u64 = 8;
    const ATTEMPTS: usize = 200;
    let shared = test_shared();
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        let mut c = make_malformed_class("com/example/RacyBoom", ClassLoaderId::Application);
        c.id = id;
        cm.class_store.add(c);
        id
    };
    let barrier = Arc::new(std::sync::Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let shared = shared.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut thread = JvmThread::new(ThreadId(100 + i), "relink-racer");
                barrier.wait();
                let mut slowest = std::time::Duration::ZERO;
                for _ in 0..ATTEMPTS {
                    let start = std::time::Instant::now();
                    let r = ensure_class_initialized_shared(&shared, &mut thread, class_id);
                    slowest = slowest.max(start.elapsed());
                    assert!(
                        matches!(r, Err(MethodCallFailed::InternalError(VmError::Linkage(_)))),
                        "every attempt relinks and rethrows: {r:?}"
                    );
                }
                slowest
            })
        })
        .collect();
    for h in handles {
        let slowest = h.join().expect("racer thread");
        assert!(
            slowest < std::time::Duration::from_secs(10),
            "a waiter was stranded until the wait timeout: {slowest:?}"
        );
    }
    let cm = shared.classes.class_manager.read();
    assert!(class_link_failed(
        cm.get_class(class_id).expect("registered")
    ));
    assert!(!shared
        .classes
        .class_init_waiters
        .lock()
        .contains_key(&class_id));
}

/// Wave 8, L5: the thread-less link a compile door runs
/// (`link_class_without_java`) only ever moves a class forward to
/// `Verified`, and leaves it untouched when a check fails or another
/// thread holds it.
#[test]
fn link_class_without_java_links_forward_or_leaves_the_class_alone() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let add = |name: &str, flags: ClassAccessFlags, loader: ClassLoaderId| {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            access_flags: flags,
            ..make_malformed_class(name, loader)            });
        id
    };
    let good = add(
        "com/example/LinkGood",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        ClassLoaderId::Application,
    );
    let bad = add(
        "com/example/LinkBad",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT,
        ClassLoaderId::Application,
    );
    let trusted = add(
        "java/lang/LinkTrusted",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT,
        ClassLoaderId::Bootstrap,
    );
    let held = add(
        "com/example/LinkHeld",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        ClassLoaderId::Application,
    );
    shared
        .classes
        .class_manager_write()
        .get_class_mut(held)
        .expect("registered")
        .initializing_thread = Some(4242);
    {
        let cm = shared.classes.class_manager.read();
        let linked = |id| class_linked_or_link_trivial(&shared, &cm, cm.get_class(id).unwrap());
        assert!(!linked(good), "never linked, not trivial");
        assert!(!linked(bad), "never linked, not trivial");
        assert!(linked(trusted), "bootstrap-trusted: nothing to check");
    }
    assert!(link_class_without_java(&shared, good));
    assert!(!link_class_without_java(&shared, bad));
    assert!(!link_class_without_java(&shared, held), "claimed elsewhere");
    let cm = shared.classes.class_manager.read();
    let state = |id| {
        let c = cm.get_class(id).expect("registered");
        (c.state, c.initializing_thread)
    };
    assert_eq!(state(good), (ClassState::Verified, None));
    assert_eq!(state(bad), (ClassState::Loaded, None), "left as it was");
    assert_eq!(state(held), (ClassState::Loaded, Some(4242)), "untouched");
    assert!(class_linked_or_link_trivial(
        &shared,
        &cm,
        cm.get_class(good).unwrap()
    ));
    let waiters = shared.classes.class_init_waiters.lock();
    assert!(!waiters.contains_key(&good) && !waiters.contains_key(&bad));
}

/// Wave 17 (HotSpot `link_class_impl`): `link_class_on_thread` links the
/// superclass and the superinterfaces before the class and leaves all of
/// them `Prepared`, unclaimed and uninitialized; initialization then skips
/// both link steps, and an interface with no default method stays linked
/// but uninitialized.
#[test]
fn link_class_on_thread_links_the_supertypes_first_and_leaves_them_prepared() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let (super_id, iface_id, sub_id) = {
        let mut cm = shared.classes.class_manager_write();
        let super_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: super_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class("com/example/W17LinkSuper", ClassLoaderId::Application)
        });
        let iface_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: iface_id,
            access_flags: ClassAccessFlags::PUBLIC
                | ClassAccessFlags::INTERFACE
                | ClassAccessFlags::ABSTRACT,
            ..make_malformed_class("com/example/W17LinkIface", ClassLoaderId::Application)
        });
        let sub_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sub_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: Some(super_id),
            ..make_malformed_class("com/example/W17LinkSub", ClassLoaderId::Application)
        });
        cm.set_interfaces(sub_id, vec![iface_id]);
        (super_id, iface_id, sub_id)
    };
    let mut thread = JvmThread::new(ThreadId(6), "w17-link");
    let linked = link_class_on_thread(&shared, &mut thread, sub_id);
    assert!(linked.is_ok(), "{linked:?}");
    let state = |id: ClassId| {
        let cm = shared.classes.class_manager.read();
        let c = cm.get_class(id).expect("registered");
        (c.state, c.initializing_thread)
    };
    for id in [super_id, iface_id, sub_id] {
        assert_eq!(state(id), (ClassState::Prepared, None), "{id:?}");
        assert!(shared.classes.statics.read().contains_key(&id), "{id:?}");
        assert!(!shared.classes.class_init_waiters.lock().contains_key(&id));
    }
    let again = link_class_on_thread(&shared, &mut thread, sub_id);
    assert!(again.is_ok(), "{again:?}");
    let init = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
    assert!(init.is_ok(), "{init:?}");
    assert_eq!(state(sub_id), (ClassState::Initialized, None));
    assert_eq!(state(super_id), (ClassState::Initialized, None));
    assert_eq!(
        state(iface_id),
        (ClassState::Prepared, None),
        "no default method: linked, never initialized"
    );
}


/// Wave 17: `link_class_on_thread` leaves a class another thread holds
/// alone, parks a class that fails to link unlinked (retryable), and a
/// superinterface that fails to link stops its implementor before the
/// implementor is claimed or prepared, on both entry points.
#[test]
fn link_class_on_thread_stops_at_a_held_or_failing_class() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let add = |name: &str, flags: ClassAccessFlags| {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            access_flags: flags,
            ..make_malformed_class(name, ClassLoaderId::Application)            });
        id
    };
    let held = add(
        "com/example/W17Held",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    // INTERFACE + FINAL fails Pass 2.
    let bad_iface = add(
        "com/example/W17BadIface",
        ClassAccessFlags::PUBLIC
            | ClassAccessFlags::INTERFACE
            | ClassAccessFlags::ABSTRACT
            | ClassAccessFlags::FINAL,
    );
    let implementor = add(
        "com/example/W17Implementor",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    shared
        .classes
        .class_manager_write()
        .set_interfaces(implementor, vec![bad_iface]);
    shared
        .classes
        .class_manager_write()
        .get_class_mut(held)
        .expect("registered")
        .initializing_thread = Some(4242);
    let mut thread = JvmThread::new(ThreadId(7), "w17-link-fail");
    assert!(link_class_on_thread(&shared, &mut thread, held).is_ok());
    let linkage = |r: Result<(), MethodCallFailed>| {
        matches!(r, Err(MethodCallFailed::InternalError(VmError::Linkage(_))))
    };
    assert!(linkage(link_class_on_thread(
        &shared,
        &mut thread,
        implementor
    )));
    {
        let cm = shared.classes.class_manager.read();
        let state = |id| {
            let c = cm.get_class(id).expect("registered");
            (c.state, c.initializing_thread)
        };
        assert_eq!(state(held), (ClassState::Loaded, Some(4242)), "untouched");
        assert!(class_link_failed(
            cm.get_class(bad_iface).expect("registered")
        ));
        assert_eq!(
            state(implementor),
            (ClassState::Loaded, None),
            "stopped before it was claimed"
        );
    }
    assert!(linkage(ensure_class_initialized_shared(
        &shared,
        &mut thread,
        implementor
    )));
    let cm = shared.classes.class_manager.read();
    assert!(class_link_failed(
        cm.get_class(implementor).expect("registered")
    ));
    assert!(!shared.classes.statics.read().contains_key(&implementor));
    let waiters = shared.classes.class_init_waiters.lock();
    assert!(!waiters.contains_key(&bad_iface) && !waiters.contains_key(&implementor));
}

/// Wave 17 (`interpreter-L1-rust-jvmti-manager-class-prepare-fires-at-define-time-FIXED`):
/// the VM's own Rust-side JVMTI manager hears `ClassPrepare` at the
/// prepare point, on the preparing thread, superclass first, once per
/// class (a second initialization, and a second post of the same class,
/// report nothing).
#[test]
fn the_vm_jvmti_manager_hears_class_prepare_once_on_the_preparing_thread() {
    use crate::classloading::ClassLoaderId;
    use crate::runtime::jvmti::{self as rt, EventCallbacks, EventMode, JvmtiEventKind};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let (super_id, sub_id) = {
        let mut cm = shared.classes.class_manager_write();
        let super_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: super_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class("java/lang/W17RustPrepareSuper", ClassLoaderId::Bootstrap)
        });
        let sub_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sub_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: Some(super_id),
            ..make_malformed_class("java/lang/W17RustPrepareSub", ClassLoaderId::Bootstrap)
        });
        (super_id, sub_id)
    };
    let wire = |id: ClassId| u64::from(id.as_u32());
    let seen: Arc<std::sync::Mutex<Vec<(u64, u64)>>> = Arc::default();
    let manager = rt::manager_for_vm(shared.vm_identity)
        .expect("SharedVm::new installs the VM's manager");
    let sink = Arc::clone(&seen);
    manager
        .set_event_callbacks(EventCallbacks {
            class_prepare: Some(Box::new(move |thread, class| {
                if let Ok(mut seen) = sink.lock() {
                    seen.push((thread, class));
                }
            })),
            ..EventCallbacks::default()
        })
        .expect("callbacks");
    manager
        .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassPrepare, None)
        .expect("enable");
    let mut thread = JvmThread::new(ThreadId(9), "w17-rust-prepare");
    let first = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
    let second = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
    rt::fire_class_prepare_for_vm(shared.vm_identity, 9, wire(sub_id));
    let mut got = seen.lock().map(|s| s.clone()).unwrap_or_default();
    rt::forget_vm_jvmti_state(shared.vm_identity);
    assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
    got.retain(|&(_, class)| class == wire(super_id) || class == wire(sub_id));
    assert_eq!(got, vec![(9, wire(super_id)), (9, wire(sub_id))]);
}

/// Wave 18: reflection links a never-linked class in every mode,
/// `--compatible` included, and leaves it prepared and unclaimed.
#[test]
fn reflection_links_the_class_in_every_mode() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    assert!(!shared.config.is_jdk_only());
    assert!(reflection_links_class_enforced(&shared));
    let class = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class("com/example/W17Reflected", ClassLoaderId::Application)            });
        id
    };
    let mut thread = JvmThread::new(ThreadId(8), "w17-reflect");
    assert!(link_class_for_reflection(&shared, &mut thread, class).is_ok());
    let cm = shared.classes.class_manager.read();
    let c = cm.get_class(class).expect("registered");
    assert_eq!(
        (c.state, c.initializing_thread),
        (ClassState::Prepared, None)
    );
}

/// JVMS §5.5 step 7 / HotSpot `initialize_super_interfaces` (interpreter
/// round i1 wave 9, lane L5): a class initializes, in recursive
/// interfaces-array order, exactly the superinterfaces that DECLARE a
/// default method — not one that only inherits it — and an interface
/// initializes none.
#[test]
fn a_class_initializes_only_the_superinterfaces_that_declare_a_default() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::method::ClassFileMethod;
    let method = |name: &str, flags: MethodAccessFlags| ClassFileMethod {
        access_flags: flags,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: Vec::new(),
    };
    let shared = test_shared();
    let mut cm = shared.classes.class_manager_write();
    let mut add = |name: &str, interface: bool, methods: Vec<ClassFileMethod>| {
        let id = cm.class_store.next_id();
        let access_flags = if interface {
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
        } else {
            ClassAccessFlags::PUBLIC
        };
        cm.class_store.add(Class {
            id,
            access_flags,
            methods,
            ..make_malformed_class(name, ClassLoaderId::Application)            });
        id
    };
    let declares = add(
        "p/Declares",
        true,
        vec![method("d", MethodAccessFlags::PUBLIC)],
    );
    let inherits = add(
        "p/Inherits",
        true,
        vec![method(
            "a",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
        )],
    );
    let statics_only = add(
        "p/StaticsOnly",
        true,
        vec![method(
            "s",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        )],
    );
    let other = add(
        "p/Other",
        true,
        vec![method("o", MethodAccessFlags::PUBLIC)],
    );
    let class = add("p/C", false, Vec::new());
    let twice = add("p/Twice", false, Vec::new());
    cm.set_interfaces(inherits, vec![declares]);
    cm.set_interfaces(class, vec![inherits, statics_only, other]);
    cm.set_interfaces(twice, vec![declares, inherits]);

    assert_eq!(
        superinterfaces_to_initialize(&cm, class),
        vec![declares, other],
        "the inheriting interface is recursed into but not initialized"
    );
    assert_eq!(superinterfaces_to_initialize(&cm, twice), vec![declares]);
    assert!(
        superinterfaces_to_initialize(&cm, inherits).is_empty(),
        "an interface's initialization initializes no superinterface"
    );
}

/// Interpreter round i1 wave 10, lane L4 (JVMS §5.5 step 2 for step 7): a
/// class whose superinterface another thread is initializing waits for
/// that initialization to complete instead of skipping the interface.
#[test]
fn a_class_waits_for_a_superinterface_another_thread_is_initializing() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::method::ClassFileMethod;
    let shared = test_shared();
    let (iface, class) = {
        let mut cm = shared.classes.class_manager_write();
        let iface = cm.class_store.next_id();
        // Bootstrap-trusted names: verification is skipped, so the test
        // exercises only the initialization protocol.
        cm.class_store.add(Class {
            id: iface,
            access_flags: ClassAccessFlags::PUBLIC
                | ClassAccessFlags::INTERFACE
                | ClassAccessFlags::ABSTRACT,
            methods: vec![ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC,
                name: Arc::from("d"),
                descriptor: Arc::from("()V"),
                attributes: Vec::new(),
            }],
            state: ClassState::Initializing,
            initializing_thread: Some(4242),
            ..make_malformed_class("java/lang/InitWaitIface", ClassLoaderId::Bootstrap)
        });
        let class = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: class,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class("java/lang/InitWaitImpl", ClassLoaderId::Bootstrap)
        });
        cm.set_interfaces(class, vec![iface]);
        (iface, class)
    };
    shared.classes.class_init_waiters.lock().insert(
        iface,
        Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new())),
    );
    let initializer = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let mut thread = JvmThread::new(ThreadId(77), "init-waiter");
            ensure_class_initialized_shared(&shared, &mut thread, class)
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        !initializer.is_finished(),
        "the class finished initializing while its superinterface was still initializing"
    );
    assert_ne!(
        shared
            .classes
            .class_manager
            .read()
            .get_class(class)
            .map(|c| c.state),
        Some(ClassState::Initialized)
    );
    assert!(finalize_class_init(
        &shared,
        iface,
        ClassState::Initialized,
        4242
    ));
    let result = initializer.join().expect("initializer thread");
    assert!(result.is_ok(), "{result:?}");
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        cm.get_class(class).map(|c| c.state),
        Some(ClassState::Initialized)
    );
}

#[test]
fn end_to_end_bootstrap_trusted_name_still_skips_verifier() {
    // End-to-end: same malformed shape, but loader=Bootstrap and
    // name in `java/`. The skip is the documented perf invariant вЂ”
    // initialization must succeed (verification is bypassed and
    // structural checks that would have flagged FINAL+ABSTRACT
    // never run).
    use crate::classloading::{ClassLoaderId, ClassState};
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        let mut c = make_malformed_class("java/lang/String", ClassLoaderId::Bootstrap);
        c.id = id;
        cm.class_store.add(c);
        id
    };
    let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
    assert!(
        result.is_ok(),
        "bootstrap-loaded java/lang/String must remain skip-eligible; got {result:?}"
    );
    // Sanity: the class must have reached `Initialized` (no
    // <clinit>, no superclass — full init runs to completion only
    // because verification was skipped).
    let cm = shared.classes.class_manager.read();
    let final_state = cm.get_class(class_id).map(|c| c.state);
    assert_eq!(
        final_state,
        Some(ClassState::Initialized),
        "expected Initialized after skip, got {final_state:?}"
    );
}

/// The wire class ids of the `ClassPrepare` events queued on `rx`, in
/// order (the event's data is tag, class id, signature, status).
#[cfg(feature = "experimental-debug")]
fn class_prepare_events(rx: &std::sync::mpsc::Receiver<crate::debug::DebugEvent>) -> Vec<u64> {
    rx.try_iter()
        .filter(|e| e.kind == crate::debug::events::EventKind::ClassPrepare)
        .map(|e| {
            let mut id = [0u8; 8];
            id.copy_from_slice(&e.extra[1..9]);
            u64::from_be_bytes(id)
        })
        .collect()
}

/// A VM with one JDWP `ClassPrepare` request (no filter, no suspension)
/// and its event channel.
#[cfg(feature = "experimental-debug")]
fn class_prepare_listener(
    shared: &SharedVm,
) -> std::sync::mpsc::Receiver<crate::debug::DebugEvent> {
    use crate::debug::events::{EventKind, SuspendPolicy};
    {
        let mut ds = shared.debug.debug_state.lock();
        ds.events
            .set_event_request(EventKind::ClassPrepare, SuspendPolicy::None, vec![]);
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);
    rx
}

/// Wave 15 (`interpreter-L1-jvmti-class-prepare-is-reported-at-initialization-not-at-linking-FIXED`):
/// a class is prepared, and reported, as part of its linking; wave 17
/// (HotSpot `link_class_impl`) links the superclass first, so the
/// superclass's `ClassPrepare` comes first, and a later initialization
/// request reports nothing again.
#[cfg(feature = "experimental-debug")]
#[test]
fn class_prepare_reports_the_superclass_before_the_subclass() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let (super_id, sub_id) = {
        let mut cm = shared.classes.class_manager_write();
        let super_id = cm.class_store.next_id();
        // Bootstrap-trusted names: verification is skipped.
        cm.class_store.add(Class {
            id: super_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class("java/lang/W15PrepareSuper", ClassLoaderId::Bootstrap)
        });
        let sub_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sub_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: Some(super_id),
            ..make_malformed_class("java/lang/W15PrepareSub", ClassLoaderId::Bootstrap)
        });
        (super_id, sub_id)
    };
    let rx = class_prepare_listener(&shared);
    let mut thread = JvmThread::new(ThreadId(5), "w15-prepare");
    let result = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
    assert!(result.is_ok(), "{result:?}");
    let wire = |id: ClassId| u64::from(id.as_u32());
    assert_eq!(
        class_prepare_events(&rx),
        vec![wire(super_id), wire(sub_id)],
        "the superclass is linked, prepared and reported before its subclass"
    );
    let again = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
    assert!(again.is_ok(), "{again:?}");
    assert!(class_prepare_events(&rx).is_empty(), "once");
}

/// Wave 17 (HotSpot `link_class_impl`): a class whose superclass fails to
/// link is never prepared nor reported, however often its
/// initialization is retried (wave 15 prepared it first, and reported it
/// once).
#[cfg(feature = "experimental-debug")]
#[test]
fn a_class_whose_superclass_fails_to_link_is_never_prepared() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let sub_id = {
        let mut cm = shared.classes.class_manager_write();
        let super_id = cm.class_store.next_id();
        // As in `a_superclass_link_failure_leaves_the_subclass_retryable`:
        // ANNOTATION without INTERFACE fails Pass 2.
        cm.class_store.add(Class {
            id: super_id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::ANNOTATION,
            ..make_malformed_class("com/example/W15BadSuper", ClassLoaderId::Application)
        });
        let sub_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sub_id,
            access_flags: ClassAccessFlags::PUBLIC,
            superclass: Some(super_id),
            ..make_malformed_class("com/example/W15GoodSub", ClassLoaderId::Application)
        });
        sub_id
    };
    let rx = class_prepare_listener(&shared);
    let mut thread = JvmThread::new(ThreadId(5), "w15-prepare-retry");
    for attempt in 0..2 {
        assert!(
            ensure_class_initialized_shared(&shared, &mut thread, sub_id).is_err(),
            "attempt {attempt}: the superclass fails to link"
        );
    }
    assert!(
        class_prepare_events(&rx).is_empty(),
        "neither the superclass nor the subclass is prepared"
    );
    let cm = shared.classes.class_manager.read();
    assert!(class_link_failed(cm.get_class(sub_id).expect("registered")));
    assert!(
        !shared.classes.statics.read().contains_key(&sub_id),
        "no statics block: the subclass was never prepared"
    );
}

/// Wave 15: while `ClassPrepare` is observed, a compile door neither
/// links a class early (`link_class_without_java` never prepares) nor
/// counts a never-linked class as trivially linked, so the class is
/// initialized — prepared and reported — on a Java thread.
#[cfg(feature = "experimental-debug")]
#[test]
fn a_class_is_not_linked_early_while_class_prepare_is_observed() {
    use crate::classloading::ClassLoaderId;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    let shared = test_shared();
    let add = |name: &str, loader: ClassLoaderId| {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ..make_malformed_class(name, loader)            });
        id
    };
    let good = add("com/example/W15EarlyLink", ClassLoaderId::Application);
    let trusted = add("java/lang/W15EarlyTrusted", ClassLoaderId::Bootstrap);
    let gates = &shared.debug.debugger_gates;
    gates.set_native_class_prepare(true);
    assert!(class_prepare_observed(&shared));
    assert!(!link_class_without_java(&shared, good));
    {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(good).expect("registered");
        assert_eq!(
            (class.state, class.initializing_thread),
            (ClassState::Loaded, None)
        );
        assert!(!class_linked_or_link_trivial(
            &shared,
            &cm,
            cm.get_class(trusted).expect("registered")
        ));
    }
    gates.set_native_class_prepare(false);
    assert!(!class_prepare_observed(&shared));
    assert!(link_class_without_java(&shared, good));
    let cm = shared.classes.class_manager.read();
    assert!(class_linked_or_link_trivial(
        &shared,
        &cm,
        cm.get_class(trusted).expect("registered")
    ));
}

// -----------------------------------------------------------------------
// <clinit>-failure policy gate (JVMS §5.5 default-strict inversion)
//
// The lenient swallow + post_clinit_fixup path must be OFF by default so
// a failing `<clinit>` propagates (Erroneous) instead of fabricating
// synthetic statics. The behavior is opt-in via `CRATONVM_LENIENT_CLINIT=1`.
// We can only deterministically assert the default here (the gate is a
// process-lifetime `OnceLock`, so mutating the env mid-test would be
// racy). When the env var is absent, the gate must read `false`.
// -----------------------------------------------------------------------

#[test]
fn lenient_clinit_defaults_off() {
    // Guard against a polluted CI env explicitly setting the opt-in.
    if cratonvm_types::flags::runtime_var("CRATONVM_LENIENT_CLINIT").as_deref() == Ok("1") {
        // Opt-in is honored — gate reads true. Nothing else to assert.
        assert!(lenient_clinit());
    } else {
        // Default / unset / any non-"1" value => strict (JVMS-correct).
        assert!(
            !lenient_clinit(),
            "lenient <clinit> swallow must be OFF unless CRATONVM_LENIENT_CLINIT=1"
        );
    }
}

// -----------------------------------------------------------------------
// Narrowed lenient-swallow allowlist (`clinit_swallow_has_recovery`).
//
// The swallow must be tolerated ONLY for classes with a documented
// recovery path (a `post_clinit_fixup` arm or the SLF4J/logback native
// binder stubs) — not for the old broad framework prefix allowlist that
// hid real init bugs behind a stamped-`Initialized` class.
// -----------------------------------------------------------------------

#[test]
fn clinit_swallow_recovery_admits_documented_cases() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
    // Each of these has an explicit `post_clinit_fixup` match arm.
    for cls in [
        "java/util/logging/LogManager",
        "jdk/internal/icu/text/NormalizerBase$NFCModeImpl",
        "java/lang/invoke/VarHandleInts$Array",
        "java/lang/invoke/VarHandleReferences$Array",
        "io/quarkus/bootstrap/logging/InitialConfigurator",
        "org/jboss/modules/DefaultBootModuleLoaderHolder",
        "java/math/BigInteger",
        "java/math/BigDecimal",
        "java/nio/file/attribute/PosixFilePermission",
        "org/jboss/msc/service/ServiceContainerImpl",
        "org/jboss/msc/service/ServiceLogger",
        "org/wildfly/security/auth/server/_private/ElytronMessages",
    ] {
        assert!(
            clinit_swallow_has_recovery(&shared, cls),
            "{cls} has a post_clinit_fixup recovery arm and must stay swallowable",
        );
    }
    // SLF4J/logback binder packages — recovered by native binder stubs.
    assert!(clinit_swallow_has_recovery(&shared, 
        "org/slf4j/impl/StaticLoggerBinder"
    ));
    assert!(clinit_swallow_has_recovery(&shared, 
        "ch/qos/logback/classic/util/ContextSelectorStaticBinder"
    ));
}

#[test]
fn clinit_swallow_recovery_rejects_unrecovered_classes() {
    // These matched the OLD broad prefix allowlist (java/ jdk/ sun/ javax/
    // org/jboss/ io/quarkus/ org/wildfly/ com/sun/ org/springframework/boot/loader/)
    // but have NO recovery path. They must now propagate per JVMS §5.5
    // instead of being silently stamped Initialized with null statics.
    for cls in [
        "java/util/HashMap",
        "jdk/internal/misc/Unsafe",
        "sun/security/provider/Sun",
        "javax/crypto/Cipher",
        "org/jboss/modules/Module",
        "io/quarkus/runtime/Application",
        "org/wildfly/common/Assert",
        "com/sun/crypto/provider/SunJCE",
        "org/springframework/boot/loader/jar/JarFileArchive",
        // real-cdi-bean-container Step 3: the Spring startup-metrics no-op
        // shim is removed, so ApplicationStartup runs its real `<clinit>` and
        // must NOT be swallowed/backfilled.
        "org/springframework/core/metrics/ApplicationStartup",
        "org/springframework/core/metrics/DefaultApplicationStartup",
        // Arbitrary app classes were never in the old allowlist and must
        // stay rejected.
        "com/example/MyService",
        "org/apache/catalina/startup/Catalina",
    ] {
        assert!(
            !clinit_swallow_has_recovery(&SharedVm::new(crate::config::VmConfig::default()), cls),
            "{cls} has no recovery path and must NOT be swallowed",
        );
    }
}

// -----------------------------------------------------------------------
// Per-OS-thread frame-trace publication (startup-and-diagnostics.md §6.3)
//
// Every test below runs the publication inside its OWN spawned thread. The
// cell is thread-local, so doing otherwise would let cargo's test harness
// (which reuses threads) leak one test's publication into the next.
// -----------------------------------------------------------------------

fn entry(class: &str, method: &str, line: i32) -> cratonvm_native_api::StackTraceEntry {
    cratonvm_native_api::StackTraceEntry {
        class_name: class.into(),
        method_name: method.into(),
        method_descriptor: None,
        source_file: Some("Probe.java".into()),
        line_number: line,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    }
}

fn handle(entries: Vec<cratonvm_native_api::StackTraceEntry>) -> PublishedTraceHandle {
    Arc::new(parking_lot::Mutex::new(entries))
}

#[test]
fn an_unpublished_os_thread_reports_nothing_so_the_primordial_path_still_runs() {
    std::thread::spawn(|| {
        assert!(faulting_thread_java_stack_lines(64).is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn a_published_worker_renders_its_own_frames_with_its_own_identity() {
    std::thread::spawn(|| {
        let _guard = PublishedFrameTrace::publish(
            "pool-1-thread-3",
            77,
            handle(vec![entry("com/example/Svc", "run", 42)]),
        );
        let lines = faulting_thread_java_stack_lines(64).expect("published");
        assert!(
            lines[0].contains("pool-1-thread-3") && lines[0].contains("77"),
            "header must name the faulting thread, got {:?}",
            lines[0]
        );
        assert_eq!(lines[1], "  at com/example/Svc.run(Probe.java:42)");
    })
    .join()
    .unwrap();
}

#[test]
fn dropping_the_guard_unpublishes_so_a_carrier_never_reports_a_stale_mount() {
    std::thread::spawn(|| {
        {
            let _guard =
                PublishedFrameTrace::publish("vt-1", 1, handle(vec![entry("A", "a", 1)]));
            assert!(faulting_thread_java_stack_lines(8).is_some());
        }
        assert!(
            faulting_thread_java_stack_lines(8).is_none(),
            "an unmounted carrier must not still advertise the continuation's stack"
        );
    })
    .join()
    .unwrap();
}

#[test]
fn a_carrier_mounting_one_continuation_after_another_reports_the_current_one() {
    std::thread::spawn(|| {
        for (name, tid) in [("vt-1", 1u64), ("vt-2", 2), ("vt-3", 3)] {
            let _guard = PublishedFrameTrace::publish(
                name,
                tid,
                handle(vec![entry("Task", name, tid as i32)]),
            );
            let lines = faulting_thread_java_stack_lines(8).expect("mounted");
            assert!(lines[0].contains(name), "got {:?}", lines[0]);
            assert!(lines[1].contains(&format!("Task.{name}")));
        }
        assert!(faulting_thread_java_stack_lines(8).is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn a_nested_publication_restores_the_outer_one_rather_than_blanking_the_cell() {
    std::thread::spawn(|| {
        let _outer =
            PublishedFrameTrace::publish("outer", 10, handle(vec![entry("O", "o", 1)]));
        {
            let _inner =
                PublishedFrameTrace::publish("inner", 11, handle(vec![entry("I", "i", 2)]));
            assert!(faulting_thread_java_stack_lines(8).unwrap()[0].contains("inner"));
        }
        let lines = faulting_thread_java_stack_lines(8).expect("outer must come back");
        assert!(lines[0].contains("outer"), "got {:?}", lines[0]);
        assert_eq!(lines[1], "  at O.o(Probe.java:1)");
    })
    .join()
    .unwrap();
}

#[test]
fn a_held_frame_trace_mutex_is_reported_not_waited_on() {
    std::thread::spawn(|| {
        let trace = handle(vec![entry("A", "a", 1)]);
        let _guard = PublishedFrameTrace::publish("worker", 5, trace.clone());
        // Exactly the crash-time situation: the faulting thread was inside
        // a frame-trace republication when it died. Blocking here would
        // turn a diagnosable crash into a hang.
        let _held = trace.lock();
        let lines = faulting_thread_java_stack_lines(8).expect("published");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("not waiting on it"), "got {:?}", lines[0]);
    })
    .join()
    .unwrap();
}

#[test]
fn an_empty_trace_says_so_rather_than_rendering_a_bare_header() {
    std::thread::spawn(|| {
        let _guard = PublishedFrameTrace::publish("worker", 5, handle(Vec::new()));
        let lines = faulting_thread_java_stack_lines(8).expect("published");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("<none published yet>"),
            "got {:?}",
            lines[0]
        );
    })
    .join()
    .unwrap();
}

#[test]
fn a_deep_stack_is_truncated_with_a_count_of_what_was_dropped() {
    std::thread::spawn(|| {
        let frames: Vec<_> = (0..10).map(|i| entry("C", "m", i)).collect();
        let _guard = PublishedFrameTrace::publish("worker", 5, handle(frames));
        let lines = faulting_thread_java_stack_lines(4).expect("published");
        // header + 4 frames + the "... (6 more)" line
        assert_eq!(lines.len(), 6);
        assert!(lines[5].contains("6 more"), "got {:?}", lines[5]);
    })
    .join()
    .unwrap();
}

#[test]
fn publication_is_per_os_thread_and_never_bleeds_across_workers() {
    let a = std::thread::spawn(|| {
        let _guard = PublishedFrameTrace::publish("A", 1, handle(vec![entry("A", "a", 1)]));
        std::thread::sleep(std::time::Duration::from_millis(20));
        faulting_thread_java_stack_lines(8).expect("A")[0].clone()
    });
    let b = std::thread::spawn(|| {
        let _guard = PublishedFrameTrace::publish("B", 2, handle(vec![entry("B", "b", 2)]));
        std::thread::sleep(std::time::Duration::from_millis(20));
        faulting_thread_java_stack_lines(8).expect("B")[0].clone()
    });
    assert!(a.join().unwrap().contains("\"A\""));
    assert!(b.join().unwrap().contains("\"B\""));
}
