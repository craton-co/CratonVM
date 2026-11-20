use super::initialize_bootstrap_declaring_class;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::config::VmConfig;
use crate::error::MethodCallResult;
use crate::native::registry::{NativeCallback, NativeContext};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const BSM_DESC: &str =
    "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;";

static PARENT_CLINIT: AtomicU32 = AtomicU32::new(0);
static CHILD_CLINIT: AtomicU32 = AtomicU32::new(0);

fn parent_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    PARENT_CLINIT.fetch_add(1, Ordering::SeqCst);
    Ok(None)
}

fn child_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    CHILD_CLINIT.fetch_add(1, Ordering::SeqCst);
    Ok(None)
}

fn method(name: &str, descriptor: &str, flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        access_flags: flags,
        attributes: vec![],
    }
}

/// Define `name` as a LOADED (not initialized) application class.
fn define(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    methods: Vec<ClassFileMethod>,
) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
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
        origin: cratonvm_classloading::ClassOrigin::ApplicationClassPath {
            source: Arc::from("i15-l4-test"),
        },
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    if superclass.is_some() {
        cm.set_superclass(id, superclass);
    }
    id
}

/// `C.bsm` for a static `bsm` declared in `P` runs `P.<clinit>` only; a
/// bootstrap `C` declares itself initializes `C`.
#[test]
fn an_inherited_bootstrap_initializes_only_its_declaring_class() {
    const PARENT: &str = "cratonvm/i15l4/BsmParent";
    const CHILD: &str = "cratonvm/i15l4/BsmChild";
    let mut vm = SharedVm::new(VmConfig::default());
    let clinits: [(&str, NativeCallback); 2] = [(PARENT, parent_clinit), (CHILD, child_clinit)];
    for (owner, callback) in clinits {
        vm.natives.native_methods.register(
            owner,
            "<clinit>",
            "()V",
            callback,
            cratonvm_native_api::NativeKind::Bridge,
        );
    }
    let shared = Arc::new(vm);
    let clinit = || {
        method(
            "<clinit>",
            "()V",
            MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
        )
    };
    // Native, so the link step's verifier (which wave 15 runs before the
    // superclass is initialized) accepts a method with no Code attribute;
    // the test never calls them.
    let public_static =
        MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE;
    let parent = define(
        &shared,
        PARENT,
        None,
        vec![clinit(), method("bsm", BSM_DESC, public_static)],
    );
    let child = define(
        &shared,
        CHILD,
        Some(parent),
        vec![clinit(), method("own", BSM_DESC, public_static)],
    );
    let initialized = |id| crate::vm::is_class_initialized_via_manager(&shared, id);
    let mut thread = JvmThread::new(ThreadId(0), "i15-l4-bsm");

    let inherited =
        initialize_bootstrap_declaring_class(&shared, &mut thread, child, "bsm", BSM_DESC);
    assert!(inherited.is_ok(), "{inherited:?}");
    assert_eq!(PARENT_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(
        CHILD_CLINIT.load(Ordering::SeqCst),
        0,
        "an inherited bootstrap never initializes the class it is named through"
    );
    assert!(initialized(parent));
    assert!(!initialized(child));

    let own =
        initialize_bootstrap_declaring_class(&shared, &mut thread, child, "own", BSM_DESC);
    assert!(own.is_ok(), "{own:?}");
    assert_eq!(
        CHILD_CLINIT.load(Ordering::SeqCst),
        1,
        "a bootstrap C declares"
    );
    assert!(initialized(child));
}

/// Both bootstrap routes use the rule: the condy route through
/// `initialize_bootstrap_declaring_class`, the generic indy route through
/// `crate::vm::invoke_static_shared` for a `REF_invokeStatic` bootstrap. A
/// text pin, because either route needs `MethodHandles.lookup()`, which the
/// unit-test VM cannot run.
#[test]
fn both_bootstrap_routes_initialize_the_declaring_class() {
    fn production_flat(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        src.split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect()
    }
    let condy = production_flat(include_str!("../constants.rs"));
    assert!(condy.contains("initialize_bootstrap_declaring_class(shared,thread,owner,"));
    assert!(
        !condy.contains("ensure_class_initialized_shared(shared,thread,owner)?;"),
        "the condy bootstrap must not initialize the named class"
    );
    let indy = production_flat(include_str!(
        "../../invokedynamic/object_methods_getter_refusal.rs"
    ));
    let start = indy
        .find("fnlink_generic_call_site(")
        .expect("the generic indy linker");
    let linker = &indy[start..];
    assert!(linker.contains("h.kind==MethodHandleKind::InvokeStatic"));
    assert!(linker.contains("ifbsm_is_static{crate::vm::invoke_static_shared_on_class("));
}
