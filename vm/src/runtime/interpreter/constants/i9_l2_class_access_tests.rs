//! Interpreter round i1, wave 9, lane L2: JVMS §5.4.3.1 / §5.4.4 class
//! access on every `CONSTANT_Class` resolution, not only `new`
//! (`docs/feature-designs/real-jdk-mode.md`).
use super::{
    check_class_constant_access, class_constant_access_denial_in, class_constant_fill_admitted,
    recorded_resolution_failure,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::error::MethodCallFailed;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

fn jdk_only_vm() -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    SharedVm::new(config)
}

/// Define and register a field-less, method-less class.
fn define(
    shared: &SharedVm,
    name: &str,
    loader: ClassLoaderId,
    access_flags: ClassAccessFlags,
    hidden: bool,
) -> ClassId {
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
        access_flags,
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
        hidden,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    if !hidden {
        cm.register_class_name(loader, name, id);
    }
    id
}

const PKG_PRIVATE: &str = "cratonvm/i9l2/p/Hidden";
const PUBLIC: &str = "cratonvm/i9l2/p/Open";

/// `(accessor in another package, package-private target, public target)`.
fn fixture(shared: &SharedVm) -> (ClassId, ClassId, ClassId) {
    let app = ClassLoaderId::Application;
    let accessor = define(
        shared,
        "cratonvm/i9l2/q/X",
        app,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        false,
    );
    let hidden = define(shared, PKG_PRIVATE, app, ClassAccessFlags::SUPER, false);
    let open = define(
        shared,
        PUBLIC,
        app,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        false,
    );
    (accessor, hidden, open)
}

/// The verdict: a package-private class of another package is refused
/// with HotSpot's text, for the class itself and as the bottom element of
/// an array name at any depth; a public class, a primitive array and a
/// self-reference are admitted.
#[test]
fn the_verdict_checks_the_bottom_element_class() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (accessor, hidden, open) = fixture(&shared);
    let cm = shared.classes.class_manager.read();
    let denial = class_constant_access_denial_in(&shared, &cm, accessor, PKG_PRIVATE, Some(hidden));
    assert_eq!(
        denial.as_deref(),
        Some(
            "failed to access class cratonvm.i9l2.p.Hidden from class cratonvm.i9l2.q.X \
             (cratonvm.i9l2.p.Hidden and cratonvm.i9l2.q.X are in unnamed module of \
             loader 'app')"
        )
    );
    for array in ["[Lcratonvm/i9l2/p/Hidden;", "[[[Lcratonvm/i9l2/p/Hidden;"] {
        assert!(
            class_constant_access_denial_in(&shared, &cm, accessor, array, None).is_some(),
            "{array}"
        );
    }
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, accessor, PUBLIC, Some(open)),
        None
    );
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, accessor, "[Lcratonvm/i9l2/p/Open;", None),
        None
    );
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, accessor, "[[I", None),
        None
    );
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, hidden, PKG_PRIVATE, Some(hidden)),
        None,
        "a class always accesses itself"
    );
    // Not loaded through the referencing loader: no verdict, never a
    // guessed refusal.
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, accessor, "cratonvm/i9l2/p/Gone", None),
        None
    );
}

/// HotSpot admits every class access from a subclass of
/// `jdk.internal.reflect.SerializationConstructorAccessorImpl` (the JDK's
/// generated serialization constructor accessors); so does the verdict.
#[test]
fn a_serialization_constructor_accessor_reaches_any_class() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (_, hidden, _) = fixture(&shared);
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let base = define(
        &shared,
        "jdk/internal/reflect/SerializationConstructorAccessorImpl",
        ClassLoaderId::Bootstrap,
        flags,
        false,
    );
    let generated = define(
        &shared,
        "jdk/internal/reflect/GeneratedSerializationConstructorAccessor1",
        ClassLoaderId::Bootstrap,
        flags,
        false,
    );
    let unrelated = define(
        &shared,
        "jdk/internal/reflect/GeneratedMethodAccessor1",
        ClassLoaderId::Bootstrap,
        flags,
        false,
    );
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(generated, Some(base));
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, generated, PKG_PRIVATE, Some(hidden)),
        None
    );
    assert!(
        class_constant_access_denial_in(&shared, &cm, unrelated, PKG_PRIVATE, Some(hidden))
            .is_some(),
        "control: the exemption is the superclass, not the package"
    );
    drop(cm);

    // Wave 26 (lane L5): the exemption is THE bootstrap class, not its
    // name. A user loader's own class under that binary name buys its
    // subclasses nothing (probe `L5W26SerializationAccessorSpoof`).
    let spoof_loader = ClassLoaderId::UserDefined(5);
    let spoof_base = define(
        &shared,
        "jdk/internal/reflect/SerializationConstructorAccessorImpl",
        spoof_loader,
        flags,
        false,
    );
    let evil = define(&shared, "l5sp/Evil", spoof_loader, flags, false);
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(evil, Some(spoof_base));
    let cm = shared.classes.class_manager.read();
    assert!(
        class_constant_access_denial_in(&shared, &cm, evil, PKG_PRIVATE, Some(hidden)).is_some(),
        "a same-named class of a user loader is not the exempt class"
    );
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, generated, PKG_PRIVATE, Some(hidden)),
        None,
        "the bootstrap class's subclasses stay exempt"
    );
}

/// Interpreter round i1 wave 27 (lane L5): a user loader's OWN class under
/// a JDK-looking name that java.base also has is what that loader answers
/// for the name; another loader with no record of it gets `None` (the
/// global route), then its memoised `loadClass` answer. `java/` names are
/// never a user loader's (probe `L5W27JdkNamedOwnClass`).
#[test]
fn java_name_initiation_survives_memo_eviction() {
    const NAME: &str = "java/util/concurrent/LinkedTransferQueue";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let loader = ClassLoaderId::UserDefined(38);
    let id = define(
        &shared,
        NAME,
        ClassLoaderId::Bootstrap,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        false,
    );
    crate::runtime::resolve::initiating_records::record_initiating_load(&shared, loader, NAME, id);
    super::cache_loader_initiated(&shared, loader, NAME, id);
    // Force the capped cache to lose precisely this row. The dictionary
    // must still prevent another loadClass call and answer compile peeks.
    shared
        .classes
        .initiating_resolution_cache
        .write()
        .get_mut(&loader)
        .unwrap()
        .remove(NAME);
    assert_eq!(
        super::loader_own_record_of_global_name(&shared, loader, NAME),
        Some(id)
    );
    assert_eq!(
        crate::runtime::resolve::initiating_records::loader_initiated_answer(&shared, loader, NAME,),
        Some(id)
    );
}

/// Interpreter round i6 wave 133 (lane L5): a `javax/` name a user loader
/// answered with the JDK's class is in its initiating record
/// (`CRATONVM_GLOBAL_NAME_INITIATING_RECORD`, default on), which the
/// `findLoadedClass` natives read; its own definition of such a name is
/// not a record row (probe `L5W125GlobalNameRefusalRecord`).
#[test]
fn a_jdk_global_name_answer_is_an_initiating_record_row() {
    const NAME: &str = "javax/net/SocketFactory";
    const OWN: &str = "javax/net/ssl/L5W133OwnName";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.config.global_name_initiating_record || !shared.classes.initiating_records_on {
        return; // `=0` in the test environment: nothing to check.
    }
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let jdk = define(&shared, NAME, ClassLoaderId::Bootstrap, flags, false);
    let loader = ClassLoaderId::UserDefined(133);
    let own = define(&shared, OWN, loader, flags, false);
    crate::runtime::resolve::initiating_records::record_initiating_load(&shared, loader, NAME, jdk);
    crate::runtime::resolve::initiating_records::record_initiating_load(&shared, loader, OWN, own);
    assert_eq!(
        crate::runtime::resolve::initiating_records::initiated_class(&shared, loader, NAME),
        Some(jdk)
    );
    assert_eq!(
        crate::runtime::resolve::initiating_records::initiated_class(&shared, loader, OWN),
        None,
        "the loader's own definition is its namespace already"
    );
    assert_eq!(
        crate::runtime::resolve::initiating_records::define_refusal(&shared, loader, NAME),
        if shared.config.global_name_refusal_record {
            Some(jdk)
        } else {
            None
        },
        "the define refusal reads the JDK-global rows under \
         CRATONVM_GLOBAL_NAME_REFUSAL_RECORD only"
    );
}

/// A `--real-jdk` VM with the two loader-record switches this page's
/// readers look at set explicitly (`i38-L5`, 2026-11-02), so the tests do
/// not depend on the environment's values for them.
fn i38_record_vm(refusal: bool, one_door: bool) -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    config.global_name_initiating_record = true;
    config.global_name_refusal_record = refusal;
    config.loader_dictionary_one_door = one_door;
    SharedVm::new(config)
}

/// `i38-L5` (2026-11-02): the define refusal reads a JDK-global row
/// (HotSpot: a loader that initiated `javax.net.SocketFactory` may not
/// define it itself) under `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` only; the
/// loader's own definition of such a name is never a row, so it is never
/// refused; an application name is refused as before.
#[test]
fn i38_l5_define_refusal_reads_global_rows_only_under_the_refusal_switch() {
    const GLOBAL: &str = "jdk/net/SocketFactory";
    const APP: &str = "com/example/I38AppName";
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let loader = ClassLoaderId::UserDefined(381);
    for refusal in [false, true] {
        let shared = i38_record_vm(refusal, false);
        if !shared.classes.initiating_records_on {
            return;
        }
        let jdk = define(&shared, GLOBAL, ClassLoaderId::Bootstrap, flags, false);
        let app = define(&shared, APP, ClassLoaderId::Application, flags, false);
        let records = crate::runtime::resolve::initiating_records::record_initiating_load;
        records(&shared, loader, GLOBAL, jdk);
        records(&shared, loader, APP, app);
        let refused = crate::runtime::resolve::initiating_records::define_refusal;
        assert_eq!(
            refused(&shared, loader, APP),
            Some(app),
            "refusal={refusal}"
        );
        assert_eq!(
            refused(&shared, loader, GLOBAL),
            if refusal { Some(jdk) } else { None },
            "refusal={refusal}"
        );
        assert_eq!(
            refused(&shared, ClassLoaderId::UserDefined(382), GLOBAL),
            None,
            "another loader initiated nothing"
        );
    }
}

/// `i38-L5` (2026-11-02): a JDK-global row, with no memo row behind it (a
/// `Class.forName(name, false, loader)` record, or a row the memo cap
/// evicted), answers `loader_initiated_answer` when records replace the memo,
/// or under the refusal or one-door switch.
#[test]
fn i38_l5_global_row_behind_a_memo_miss_is_read_under_the_record_switches() {
    const GLOBAL: &str = "javax/net/ServerSocketFactory";
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let loader = ClassLoaderId::UserDefined(383);
    for (refusal, one_door) in [(false, false), (true, false), (false, true)] {
        let shared = i38_record_vm(refusal, one_door);
        if !shared.classes.initiating_records_on {
            return;
        }
        let jdk = define(&shared, GLOBAL, ClassLoaderId::Bootstrap, flags, false);
        crate::runtime::resolve::initiating_records::record_initiating_load(
            &shared, loader, GLOBAL, jdk,
        );
        assert_eq!(
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                &shared, loader, GLOBAL,
            ),
            if shared.classes.record_replaces_memo || refusal || one_door {
                Some(jdk)
            } else {
                None
            },
            "refusal={refusal} one_door={one_door}"
        );
    }
}

/// `i38-L5` (2026-11-02): under `CRATONVM_RECORD_REPLACES_MEMO` a `java/`
/// name with a memo row and no record is no answer (the record alone
/// answers); a recorded one is; and `--compatible` still answers the memo.
#[test]
fn i38_l5_java_name_memo_row_without_a_record_is_not_an_answer_when_the_record_replaces_it() {
    const NAME: &str = "java/util/concurrent/Phaser";
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let loader = ClassLoaderId::UserDefined(384);
    let strict = i38_record_vm(false, false);
    if strict.classes.record_replaces_memo {
        let jdk = define(&strict, NAME, ClassLoaderId::Bootstrap, flags, false);
        let answer = crate::runtime::resolve::initiating_records::loader_initiated_answer;
        super::cache_loader_initiated(&strict, loader, NAME, jdk);
        assert_eq!(answer(&strict, loader, NAME), None, "memo row, no record");
        crate::runtime::resolve::initiating_records::record_initiating_load(
            &strict, loader, NAME, jdk,
        );
        assert_eq!(answer(&strict, loader, NAME), Some(jdk), "recorded");
    }
    let compat = SharedVm::new(crate::config::VmConfig::default());
    let jdk = define(&compat, NAME, ClassLoaderId::Bootstrap, flags, false);
    super::cache_loader_initiated(&compat, loader, NAME, jdk);
    assert_eq!(
        crate::runtime::resolve::initiating_records::loader_initiated_answer(&compat, loader, NAME,),
        Some(jdk),
        "--compatible: the memo answers first"
    );
}

#[test]
fn a_user_loaders_own_jdk_named_class_is_its_answer_for_the_name() {
    const NAME: &str = "jdk/security/auth/x500/X500PrivateCredential";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let jdk = define(&shared, NAME, ClassLoaderId::Bootstrap, flags, false);
    let a = ClassLoaderId::UserDefined(31);
    let b = ClassLoaderId::UserDefined(32);
    let own_a = define(&shared, NAME, a, flags, false);
    assert!(super::is_user_loader_global_name(NAME));
    assert!(super::is_user_loader_global_name(
        "jdk/internal/reflect/L5Helper"
    ));
    assert!(super::is_user_loader_global_name("java/lang/String"));
    assert!(!super::is_user_loader_global_name("com/example/Foo"));
    assert_eq!(
        super::loader_own_record_of_global_name(&shared, a, NAME),
        Some(own_a),
        "the loader's own definition, not java.base's"
    );
    assert_eq!(
        super::loader_own_record_of_global_name(&shared, b, NAME),
        None,
        "no record: the caller takes the global route"
    );
    super::cache_loader_initiated(&shared, b, NAME, jdk);
    assert_eq!(
        super::loader_own_record_of_global_name(&shared, b, NAME),
        Some(jdk),
        "a memoised loadClass answer"
    );
}

/// Re-recording the answer the memo already holds moves no epoch (a re-ask
/// of a name the loader answered before must not retire every per-thread
/// site entry); a new row or a different answer does.
#[test]
fn re_recording_a_held_initiating_answer_does_not_move_the_resolution_epoch() {
    const NAME: &str = "com/example/EpochProbe";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let a = define(&shared, NAME, ClassLoaderId::Application, flags, false);
    let b = define(
        &shared,
        "com/example/EpochProbe2",
        ClassLoaderId::Application,
        flags,
        false,
    );
    let loader = ClassLoaderId::UserDefined(41);
    let before = super::resolution_epoch_in(&shared);
    super::cache_loader_initiated(&shared, loader, NAME, a);
    let after_new = super::resolution_epoch_in(&shared);
    assert!(after_new > before, "a new row moves the epoch");
    super::cache_loader_initiated(&shared, loader, NAME, a);
    super::cache_loader_initiated(&shared, loader, NAME, a);
    assert_eq!(
        super::resolution_epoch_in(&shared),
        after_new,
        "the same answer again changes nothing a cached resolution read"
    );
    super::cache_loader_initiated(&shared, loader, NAME, b);
    assert!(
        super::resolution_epoch_in(&shared) > after_new,
        "a different answer moves it"
    );
}

/// A hidden class (stored as `<name>/0x<hex>`) is in the runtime package
/// of its class-file name, so a lambda-proxy-shaped class reaches its
/// host package's package-private classes.
#[test]
fn a_hidden_class_reaches_its_hosts_package() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (_, hidden, _) = fixture(&shared);
    let proxy = define(
        &shared,
        "cratonvm/i9l2/p/Host$$Lambda/0x1f",
        ClassLoaderId::Application,
        ClassAccessFlags::FINAL | ClassAccessFlags::SUPER,
        true,
    );
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        class_constant_access_denial_in(&shared, &cm, proxy, PKG_PRIVATE, Some(hidden)),
        None
    );
}

/// i10-L2: `--compatible` refuses too (the wave-9 census read zero
/// admissions there), and a catch row may no longer fill the entry.
#[test]
fn compatible_mode_refuses_too() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    assert!(
        !shared.config.is_jdk_only(),
        "the default config is --compatible"
    );
    let (accessor, hidden, open) = fixture(&shared);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    assert!(check_class_constant_access(
        &shared,
        &mut thread,
        accessor,
        7,
        PKG_PRIVATE,
        Some(hidden),
        "checkcast",
    )
    .is_err());
    assert!(check_class_constant_access(
        &shared,
        &mut thread,
        accessor,
        8,
        "[Lcratonvm/i9l2/p/Hidden;",
        None,
        "instanceof",
    )
    .is_err());
    assert!(!class_constant_fill_admitted(
        &shared,
        accessor,
        PKG_PRIVATE,
        hidden
    ));
    // Control: an accessible class is still admitted and may fill.
    assert!(check_class_constant_access(
        &shared,
        &mut thread,
        accessor,
        9,
        PUBLIC,
        Some(open),
        "ldc",
    )
    .is_ok());
    assert!(class_constant_fill_admitted(
        &shared, accessor, PUBLIC, open
    ));
}

/// `--real-jdk` refuses, as a Java throwable (never a silent admission),
/// and keeps admitting what is accessible, the array-target form included.
#[test]
fn jdk_only_refuses_an_inaccessible_class_constant() {
    let shared = jdk_only_vm();
    let (accessor, hidden, open) = fixture(&shared);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let refused = check_class_constant_access(
        &shared,
        &mut thread,
        accessor,
        7,
        PKG_PRIVATE,
        Some(hidden),
        "anewarray",
    );
    match refused {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let cid = shared.mem.heap.class_id_of(exc);
            // The stripped unit VM may lack the real hierarchy: name the
            // class only when it is there, and expect the JVMS §5.4.3
            // record only when it is a `LinkageError` by the hierarchy.
            let (name, is_linkage) = {
                let cm = shared.classes.class_manager.read();
                let name = cm.get_class(cid).map(|c| c.name.to_string());
                let is_linkage = cm
                    .get_loaded_class_id("java/lang/LinkageError")
                    .is_some_and(|linkage| cm.is_subclass_of(cid, linkage));
                (name, is_linkage)
            };
            if is_linkage
                && name.as_deref() == Some("java/lang/IllegalAccessError")
                && super::resolution_failure_record_enabled(&shared)
            {
                assert!(recorded_resolution_failure(&shared, &mut thread, accessor, 7).is_some());
            }
        }
        Err(MethodCallFailed::InternalError(_)) => {
            // The stripped unit VM could not build the throwable; the
            // refusal itself is what this test pins.
        }
        Ok(()) => panic!("--real-jdk must refuse a package-private class of another package"),
    }
    assert!(check_class_constant_access(
        &shared,
        &mut thread,
        accessor,
        9,
        "[[Lcratonvm/i9l2/p/Hidden;",
        None,
        "checkcast",
    )
    .is_err());
    assert!(!class_constant_fill_admitted(
        &shared,
        accessor,
        PKG_PRIVATE,
        hidden
    ));
    for _ in 0..2 {
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            10,
            PUBLIC,
            Some(open),
            "ldc",
        )
        .is_ok());
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            11,
            "[Lcratonvm/i9l2/p/Open;",
            None,
            "instanceof",
        )
        .is_ok());
    }
}

/// Register `name` as an array class over `component` with `array_info`,
/// the way `synthesize_array_class_for_loader` records it.
fn define_array(shared: &SharedVm, name: &str, component: ClassId, dimension: u8) -> ClassId {
    let id = define(
        shared,
        name,
        ClassLoaderId::Application,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        false,
    );
    let mut cm = shared.classes.class_manager.write();
    if let Some(class) = cm.class_store.get_mut(id) {
        class.array_info = Some(cratonvm_classloading::ArrayInfo {
            component_class_id: component,
            array_dimension: dimension,
            leaf_component_name: cratonvm_types::intern_arc(PKG_PRIVATE),
        });
    }
    id
}

/// i11-L2: a RESOLVED array class names its bottom element by id
/// (`array_info`), so the verdict reads the class the resolution loaded
/// rather than looking the element name up a second time; a chain that
/// ends at a class of another name is not trusted.
#[test]
fn a_resolved_array_names_its_bottom_element_by_id() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (accessor, hidden, _) = fixture(&shared);
    let one = define_array(&shared, "[Lcratonvm/i9l2/p/Hidden;", hidden, 1);
    let two = define_array(&shared, "[[Lcratonvm/i9l2/p/Hidden;", one, 2);
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        super::array_bottom_element_class(&cm, two, PKG_PRIVATE),
        Some(hidden)
    );
    assert_eq!(
        super::array_bottom_element_class(&cm, two, PUBLIC),
        None,
        "a chain that ends at another class is not the element"
    );
    assert!(class_constant_access_denial_in(
        &shared,
        &cm,
        accessor,
        "[[Lcratonvm/i9l2/p/Hidden;",
        Some(two)
    )
    .is_some());
}
