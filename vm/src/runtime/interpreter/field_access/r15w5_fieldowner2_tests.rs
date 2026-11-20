//! Round 15 wave 5, lane fieldowner2: the compile doors' threadless field
//! peek keeps loader-specific guesses out of the per-site resolution
//! cache, which an isolated URL loader's site now takes as its answer
//! (`r15w4-fieldowner-isolated-loader-field-sites-reask-the-loader-FIXED-20260930.md`).
use super::{loader_specific_peek, threadless_fill_withheld};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// Define and register a field-less, method-less class (the
/// `constants.rs` `i9_l2_class_access_tests` helper).
fn define(shared: &SharedVm, name: &str, loader: ClassLoaderId) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: loader,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
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
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(loader, name, id);
    id
}

#[test]
fn a_peek_fills_only_what_the_interpreter_would_answer() {
    if !super::isolated_field_site_sticky_enabled() {
        // The kill switch restores the historical fill everywhere.
        return;
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let user = ClassLoaderId::UserDefined(4051);
    let app = ClassLoaderId::Application;
    let app_referencing = define(&shared, "fo2/AppCaller", app);
    let referencing = define(&shared, "fo2/Caller", user);
    let app_owner = define(&shared, "fo2/Shared", app);
    let own_owner = define(&shared, "fo2/Own", user);
    let jdk_owner = define(&shared, "jdk/fo2/Named", ClassLoaderId::Bootstrap);

    // A built-in loader's class: every answer is the interpreter's too.
    assert!(!threadless_fill_withheld(
        &shared,
        app_referencing,
        "fo2/Shared",
        app_owner,
        false
    ));
    assert!(!loader_specific_peek(
        &shared,
        app_referencing,
        "fo2/Shared",
        true
    ));
    // A user loader's loader-sensitive name answered by another loader's
    // class (the initiating memo): withheld.
    assert!(threadless_fill_withheld(
        &shared,
        referencing,
        "fo2/Shared",
        app_owner,
        true
    ));
    assert!(loader_specific_peek(
        &shared,
        referencing,
        "fo2/Shared",
        true
    ));
    // Its own definition: the answer every path gives; filled.
    assert!(!threadless_fill_withheld(
        &shared,
        referencing,
        "fo2/Own",
        own_owner,
        true
    ));
    // A `jdk/`-style name the loader may define, answered by the JDK's
    // class: withheld (the interpreter asks the loader under --real-jdk).
    assert!(threadless_fill_withheld(
        &shared,
        referencing,
        "jdk/fo2/Named",
        jdk_owner,
        false
    ));
    assert!(loader_specific_peek(
        &shared,
        referencing,
        "jdk/fo2/Named",
        false
    ));
    // A `java/` name: the user loader cannot define it, but while the
    // interpreter asks that loader's `loadClass` for it (JavaNameAsk, on
    // by default since dev i47; JVMS 5.3.2) the call is observable, so a
    // peek's global answer is withheld; with the ask off the interpreter
    // answers globally too and the fill is the answer every path gives.
    // (Until nio21 wave 13 this asserted "filled" unconditionally, stale
    // since i47 put `java/` in `is_user_loader_global_name`.)
    let asks = super::super::constants::loader_global_name_ask_enabled(&shared, "java/lang/Object");
    assert_eq!(
        threadless_fill_withheld(&shared, referencing, "java/lang/Object", jdk_owner, false),
        asks
    );
    assert_eq!(
        loader_specific_peek(&shared, referencing, "java/lang/Object", false),
        asks
    );
}

/// The compile doors' side-effect-free JVMS §5.3.4 question: two loaders
/// that each defined `fo2c/T` see a field of that type differently.
#[test]
fn a_peek_sees_a_field_type_two_loaders_both_defined() {
    if !crate::runtime::env_cache::loader_aware_resolution() {
        // The user-loader view below is the loader-initiated one.
        return;
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let a = ClassLoaderId::UserDefined(4061);
    let b = ClassLoaderId::UserDefined(4062);
    let holder = define(&shared, "fo2c/Holder", a);
    let reader = define(&shared, "fo2c/Reader", b);
    let same_loader_reader = define(&shared, "fo2c/Peer", a);
    define(&shared, "fo2c/T", a);
    define(&shared, "fo2c/T", b);
    let cm = shared.classes.class_manager.read();
    assert!(super::peek_field_constraint_violated(
        &shared, &cm, reader, holder, "Lfo2c/T;"
    ));
    assert!(super::peek_field_constraint_violated(
        &shared,
        &cm,
        reader,
        holder,
        "[[Lfo2c/T;"
    ));
    assert!(
        !super::peek_field_constraint_violated(
            &shared,
            &cm,
            same_loader_reader,
            holder,
            "Lfo2c/T;"
        ),
        "one loader: no constraint"
    );
    assert!(!super::peek_field_constraint_violated(
        &shared, &cm, reader, holder, "[I"
    ));
    assert!(!super::peek_field_constraint_violated(
        &shared,
        &cm,
        reader,
        holder,
        "Ljava/lang/String;"
    ));
    assert!(
        !super::peek_field_constraint_violated(&shared, &cm, reader, holder, "Lfo2c/Unknown;"),
        "a name one side has not seen is not a direct violation"
    );
}

/// Round 15 wave 6 (lane sweepA, L2): the peek withholds its fill only for
/// a field type a user loader may define.
#[test]
fn only_a_definable_reference_type_is_constrainable() {
    use super::field_type_constrainable as c;
    assert!(c("Lfo2c/Shared;"));
    assert!(c("[[Lfo2c/Shared;"));
    assert!(c("Ljavax/sql/DataSource;"), "javax/ is definable");
    assert!(!c("Ljava/io/PrintStream;"), "System.out keeps its fill");
    assert!(!c("[Ljava/lang/String;"));
    assert!(!c("I"));
    assert!(!c("[J"));
    assert!(!c("Lmalformed"));
}
