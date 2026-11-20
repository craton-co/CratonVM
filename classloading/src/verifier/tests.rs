use super::*;
use crate::class::{ClassId, ClassLoaderId, ClassState};

use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;

fn empty_cp() -> ConstantPool {
    ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
}

fn make_class_in_store(
    store: &mut ClassStore,
    name: &str,
    superclass: Option<ClassId>,
    flags: ClassAccessFlags,
    methods: Vec<ClassFileMethod>,
) -> ClassId {
    let id = store.next_id();
    store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_cp(),
        access_flags: flags,
        superclass,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: crate::class_origin::ClassOrigin::VmInternal,
        has_finalizer: false,
        signature: None,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    });
    id
}

fn make_method(
    name: &str,
    desc: &str,
    flags: MethodAccessFlags,
    has_code: bool,
) -> ClassFileMethod {
    let attributes = if has_code {
        vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                exception_table: vec![],
                attributes: vec![],
            },
        )))]
    } else {
        vec![]
    };

    ClassFileMethod {
        access_flags: flags,
        name: Arc::from(name),
        descriptor: Arc::from(desc),
        attributes,
    }
}

// --- Access flag validity ---

#[test]
fn final_and_abstract_class_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn interface_without_abstract_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::INTERFACE, // missing ABSTRACT
        vec![],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn interface_with_final_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT | ClassAccessFlags::FINAL,
        vec![],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn valid_class_passes() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Good",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_ok());
}

#[test]
fn valid_interface_passes() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "GoodInterface",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT,
        vec![],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_ok());
}

// --- Abstract method flag checks ---

#[test]
fn abstract_private_method_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "foo",
            "()V",
            MethodAccessFlags::ABSTRACT | MethodAccessFlags::PRIVATE,
            false,
        )],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn abstract_static_method_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "foo",
            "()V",
            MethodAccessFlags::ABSTRACT | MethodAccessFlags::STATIC,
            false,
        )],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

// --- Final class constraint ---

#[test]
fn extend_final_class_rejected() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "FinalParent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::SUPER,
        vec![],
    );
    let child_id = make_class_in_store(
        &mut store,
        "Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_err());
}

// --- Final method constraint ---

#[test]
fn override_final_method_rejected() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "Parent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "foo",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL,
            true,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_err());
}

#[test]
fn override_non_final_method_ok() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "Parent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    let child_id = make_class_in_store(
        &mut store,
        "Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_ok());
}

#[test]
fn private_method_shadowing_final_method_ok() {
    // HotSpot links a class declaring its own private
    // getClass()Ljava/lang/Class; although Object.getClass is final.
    let mut store = ClassStore::new();
    let object_id = make_class_in_store(
        &mut store,
        "java/lang/Object",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "getClass",
            "()Ljava/lang/Class;",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
            false,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "GetClassPrivate",
        Some(object_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "getClass",
            "()Ljava/lang/Class;",
            MethodAccessFlags::PRIVATE,
            true,
        )],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_ok());
}

#[test]
fn public_method_overriding_object_get_class_still_rejected() {
    let mut store = ClassStore::new();
    let object_id = make_class_in_store(
        &mut store,
        "java/lang/Object",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "getClass",
            "()Ljava/lang/Class;",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
            false,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "p/GetClassPublic",
        Some(object_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "getClass",
            "()Ljava/lang/Class;",
            MethodAccessFlags::PUBLIC,
            true,
        )],
    );
    let child = store.get(child_id).unwrap();
    match verify_class_structure(child, &store) {
        Err(LinkageError::IncompatibleClassChangeError { message }) => assert_eq!(
            message,
            "class p.GetClassPublic overrides final method \
             java.lang.Object.getClass()Ljava/lang/Class;"
        ),
        other => panic!("expected IncompatibleClassChangeError, got {other:?}"),
    }
}

#[test]
fn final_method_found_behind_non_final_intermediate_rejected() {
    // HotSpot continues the search above a non-final same-signature
    // declaration; the final one further up is still an illegal override.
    let mut store = ClassStore::new();
    let top_id = make_class_in_store(
        &mut store,
        "p/Top",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "foo",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL,
            true,
        )],
    );
    let mid_id = make_class_in_store(
        &mut store,
        "p/Mid",
        Some(top_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PRIVATE, true)],
    );
    let child_id = make_class_in_store(
        &mut store,
        "q/Child",
        Some(mid_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_final_method_constraint(child, &store).is_err());
}

#[test]
fn package_private_final_across_packages_ok() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "p/Parent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::FINAL, true)],
    );
    let other_pkg = make_class_in_store(
        &mut store,
        "q/Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    let same_pkg = make_class_in_store(
        &mut store,
        "p/Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
    );
    assert!(verify_final_method_constraint(store.get(other_pkg).unwrap(), &store).is_ok());
    assert!(verify_final_method_constraint(store.get(same_pkg).unwrap(), &store).is_err());
}

// --- Abstract method implementation ---

#[test]
fn concrete_class_missing_abstract_impl_rejected() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "AbstractParent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "doThing",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            false,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "ConcreteChild",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![], // no implementation of doThing
    );
    let child = store.get(child_id).unwrap();
    // The FOCUSED checker still rejects the shape...
    assert!(verify_inherited_abstract_methods_implemented(child, &store).is_err());
    // ...but ordinary class loading deliberately does NOT enforce it
    // (HotSpot parity: linkage proceeds and AbstractMethodError is
    // raised only if the missing method is actually invoked — see the
    // `let _ = verify_inherited_abstract_methods_implemented;` note in
    // `verify_class_structure`; JAXB's `JAXBContextImpl`, which leaves
    // the deprecated abstract `createValidator()` unimplemented, is a
    // real-world class that must keep loading). This assertion pins the
    // deliberate non-enforcement so a future re-enable is a conscious
    // choice.
    assert!(verify_class_structure(child, &store).is_ok());
}

#[test]
fn concrete_class_with_abstract_impl_ok() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "AbstractParent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "doThing",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            false,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "ConcreteChild",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "doThing",
            "()V",
            MethodAccessFlags::PUBLIC,
            true,
        )],
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_ok());
}

#[test]
fn abstract_class_may_skip_abstract_impl() {
    let mut store = ClassStore::new();
    let parent_id = make_class_in_store(
        &mut store,
        "AbstractParent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "doThing",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            false,
        )],
    );
    let child_id = make_class_in_store(
        &mut store,
        "AbstractChild",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![], // abstract child need not implement
    );
    let child = store.get(child_id).unwrap();
    assert!(verify_class_structure(child, &store).is_ok());
}

// --- Code attribute presence ---

#[test]
fn concrete_method_without_code_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, false)],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn abstract_method_with_code_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
        vec![make_method(
            "foo",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            true, // should not have Code
        )],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn native_method_with_code_rejected() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Bad",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "nativeMethod",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            true, // should not have Code
        )],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_err());
}

#[test]
fn native_method_without_code_ok() {
    let mut store = ClassStore::new();
    let id = make_class_in_store(
        &mut store,
        "Good",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        vec![make_method(
            "nativeMethod",
            "()V",
            MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            false,
        )],
    );
    let class = store.get(id).unwrap();
    assert!(verify_class_structure(class, &store).is_ok());
}

// -----------------------------------------------------------------
// F3 — JSR/RET subroutine relaxation (Pass 3, JVMS §4.10.2.5)
// -----------------------------------------------------------------

/// Mock hierarchy used by the JSR/RET tests below — every class is a
/// subclass of every other, common superclass is always Object,
/// nothing is an interface. Sufficient because the JSR-relaxation
/// path does no type-state checking that would consult the
/// hierarchy.
struct PermissiveHierarchy;

impl crate::vtype::ClassHierarchy for PermissiveHierarchy {
    fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
        true
    }
    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }
    fn is_interface(&self, _name: &str) -> bool {
        false
    }
}

fn make_pre_java7_jsr_class(
    method_name: &str,
    descriptor: &str,
    max_stack: u16,
    max_locals: u16,
    code: Vec<u8>,
    exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
) -> Class {
    Class {
        id: ClassId::new(0),
        loader_id: ClassLoaderId::Application,
        name: Arc::from("Hierarchical"),
        source_file: None,
        // Java 5 (major 49) — pre-Java-7, no StackMapTable
        // requirement; legitimately may emit jsr/ret.
        version: ClassFileVersion::JAVA_5,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_cp(),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from(method_name),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table,
                    attributes: vec![],
                },
            )))],
        }],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: crate::class_origin::ClassOrigin::VmInternal,
        has_finalizer: false,
        signature: None,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    }
}

/// A synthetic method that mirrors the
/// `TypePool$AbstractBase$Hierarchical.clear` shape from
/// ByteBuddy 1.12.12 — two distinct `jsr` call sites that target
/// the same subroutine. Our worklist verifier cannot type-check it
/// (it merges `ReturnAddress(3)` and `ReturnAddress(6)` into `Top`).
///
/// Version-gated policy: at a **legal pre-Java-7 version (major 49)** the
/// opcodes are permitted (JVMS §4.9.1) and HotSpot loads the class, so
/// CratonVM accepts it after the structural scan — by default, with no
/// escape hatch. The escape hatch is exercised here only to confirm it is
/// also a path to acceptance.
///
/// Bytecode:
/// ```text
///   0: jsr +9      → push returnAddress(3),  branch to 9
///   3: jsr +6      → push returnAddress(6),  branch to 9
///   6: return
///   7: nop          (alignment padding)
///   8: nop
///   9: astore_0     ← subroutine entry: store the merged returnAddress
///  10: ret 0        ← return via the stored address
/// ```
#[test]
fn jsr_double_call_site_pre_java7_accepted_by_default() {
    let class = make_pre_java7_jsr_class(
        "clear",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x09, // 0: jsr +9
            0xa8, 0x00, 0x06, // 3: jsr +6
            0xb1, // 6: return
            0x00, 0x00, // 7..8: nop padding (unreachable)
            0x4b, // 9: astore_0
            0xa9, 0x00, // 10: ret 0
        ],
        vec![],
    );
    // Default policy (escape hatch off): legal pre-Java-7 version, so the
    // structurally well-formed subroutine method is accepted — this is the
    // regression fix (ByteBuddy `JavaDispatcher$DynamicClassLoader.proxy`).
    let accepted_default =
        verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(
        accepted_default.is_ok(),
        "legal pre-Java-7 (major 49) subroutine method must be accepted by \
         default (HotSpot loads it), got {accepted_default:?}"
    );
    // Escape hatch on: also accepted (structural-only).
    let accepted_hatch = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        accepted_hatch.is_ok(),
        "with CRATONVM_ALLOW_JSR_RET the structurally-valid subroutine method \
         must be accepted (structural-only), got {accepted_hatch:?}"
    );
}

// -----------------------------------------------------------------------
// TYPE MAPS on the subroutine path (arch-2026-07-26)
// -----------------------------------------------------------------------

/// Build a pre-Java-7 class carrying an explicit `ClassId` and an arbitrary
/// method list.
///
/// The type-map store is a process-wide side table with first-writer-wins
/// install semantics, so every test that publishes must own a distinct
/// `ClassId` or it will read another test's maps under `cargo test`'s
/// parallel harness. The 90_00x range is reserved for these tests.
fn make_pre_java7_class_with_id(raw_id: u32, methods: Vec<ClassFileMethod>) -> Class {
    let mut class = make_pre_java7_jsr_class("placeholder", "()V", 1, 1, vec![0xb1], vec![]);
    class.id = ClassId::new(raw_id);
    class.methods = methods;
    class
}

/// A method with a `Code` attribute and no subroutine opcodes.
fn plain_method(name: &str, code: Vec<u8>, max_stack: u16, max_locals: u16) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// The double-`jsr` body used by the acceptance tests above.
fn double_jsr_method(name: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![
                    0xa8, 0x00, 0x09, // 0: jsr +9
                    0xa8, 0x00, 0x06, // 3: jsr +6
                    0xb1, // 6: return
                    0x00, 0x00, // 7..8: nop padding
                    0x4b, // 9: astore_0
                    0xa9, 0x00, // 10: ret 0
                ]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// REGRESSION: a single legacy `finally` used to cost the **whole class**
/// its type maps.
///
/// `verify_class_bytecode_inner` routes a subroutine-bearing class down its
/// own per-method path, which never calls
/// `bytecode_verifier::verify_bytecode` — the only place that used to
/// publish. So `verification_status` answered `Unknown` and *every* method,
/// including the ordinary ones that were fully type-state verified, fell
/// back to conservative scanning. It must now publish.
#[test]
fn subroutine_bearing_class_publishes_maps_for_its_non_jsr_methods() {
    let class = make_pre_java7_class_with_id(
        90_001,
        vec![
            // aload_0; astore_0; return — no branches, no handlers, no jsr.
            plain_method("plain", vec![0x2a, 0x4b, 0xb1], 1, 1),
            double_jsr_method("legacyFinally"),
        ],
    );
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(
        res.is_ok(),
        "legal pre-Java-7 class must verify, got {res:?}"
    );

    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Verified,
        "a subroutine-bearing class must no longer answer Unknown"
    );

    let plain = crate::type_maps::type_maps_for(class.id, 0)
        .expect("the non-jsr method must have maps");
    // pc 0 is an instruction start with a proven layout.
    let locals = plain.local_oops_at(0).expect("pc 0 must be described");
    assert!(
        locals.get(0),
        "local 0 of an instance method is `this`, a reference"
    );
    assert_eq!(
        plain.stack_depth_at(0),
        Some(0),
        "the operand stack is empty on entry"
    );
    // pc 1 is after `aload_0`: one reference on the stack.
    assert_eq!(plain.stack_depth_at(1), Some(1));
    assert!(
        plain
            .stack_oops_at(1)
            .expect("pc 1 must be described")
            .get(0),
        "the value `aload_0` pushed is a reference"
    );
    assert!(
        plain.safe_for_fast_path(),
        "a straight-line method with in-range local operands is fast-path safe; \
         veto was {:?}",
        plain.fast_path_veto()
    );
}

/// The subroutine method itself stays conservative — but *explicitly*.
///
/// The contract is that `None` from `oop_map_at` means "unproven", never
/// "no references live here". A fabricated all-clear bitmap would be a
/// missed-root bug (JVMS §4.10.2.5 subroutine inlining exists precisely
/// because a naive merge collapses a caller's live reference to `Top` for
/// the duration of the subroutine, and that slot is read as a reference
/// again after `ret`). So the method publishes a *row-less* map whose veto
/// names the reason.
#[test]
fn jsr_method_map_is_explicitly_unproven_not_fabricated_empty() {
    let class = make_pre_java7_class_with_id(
        90_002,
        vec![
            double_jsr_method("legacyFinally"),
            plain_method("plain", vec![0xb1], 1, 1),
        ],
    );
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(
        res.is_ok(),
        "legal pre-Java-7 class must verify, got {res:?}"
    );

    let jsr = crate::type_maps::type_maps_for(class.id, 0)
        .expect("the jsr method must have an explicit entry, not silence");
    assert_eq!(
        jsr.entry_count(),
        0,
        "no row may be fabricated for a method whose type-state is unproven"
    );
    for pc in 0..12u32 {
        assert!(
            jsr.oop_map_at(pc).is_none(),
            "pc {pc} must answer `None` (unproven → scan conservatively)"
        );
        assert!(jsr.local_oops_at(pc).is_none());
        assert!(jsr.stack_oops_at(pc).is_none());
    }
    assert!(
        !jsr.safe_for_fast_path(),
        "a subroutine method must never take the unchecked interpreter path"
    );
    assert_eq!(
        jsr.fast_path_veto(),
        Some(FastPathVeto::Subroutine),
        "the veto reason is what makes the conservative fallback diagnosable \
         rather than indistinguishable from an unverified class"
    );
}

/// Abstract and native methods must still occupy their position so
/// `ClassTypeMaps` indices line up with `Class::methods`.
#[test]
fn subroutine_class_type_map_indices_track_class_methods() {
    let class = make_pre_java7_class_with_id(
        90_003,
        vec![
            make_method("abs", "()V", MethodAccessFlags::ABSTRACT, false),
            double_jsr_method("legacyFinally"),
            make_method("nat", "()V", MethodAccessFlags::NATIVE, false),
            plain_method("plain", vec![0x2a, 0x4b, 0xb1], 1, 1),
        ],
    );
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(res.is_ok(), "class must verify, got {res:?}");

    let maps = crate::type_maps::class_type_maps(class.id).expect("maps must be published");
    assert_eq!(maps.method_count(), 4, "one entry per Class::methods slot");
    assert!(maps.method(0).is_none(), "abstract method has no body");
    assert_eq!(
        maps.method(1).and_then(|m| m.fast_path_veto()),
        Some(FastPathVeto::Subroutine)
    );
    assert!(maps.method(2).is_none(), "native method has no body");
    assert!(
        maps.method(3).is_some_and(|m| m.entry_count() > 0),
        "the plain method at index 3 must carry real rows"
    );
    // The by-name path must agree with the by-index path.
    assert_eq!(
        crate::type_maps::type_maps_for_named(class.id, "plain", "()V")
            .map(|m| m.entry_count()),
        maps.method(3).map(|m| m.entry_count())
    );
}

/// A class that fails verification must publish nothing: a half-built map
/// for a class that never loads would be visible to the GC forever.
#[test]
fn failed_subroutine_class_publishes_no_maps() {
    let mut class =
        make_pre_java7_class_with_id(90_004, vec![double_jsr_method("legacyFinally")]);
    // Major ≥ 51 makes the subroutine opcodes illegal (JVMS §4.9.1).
    class.version = ClassFileVersion::JAVA_7;
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(res.is_err(), "subroutine at major 51 must be rejected");
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Unknown,
        "a class that failed verification must leave no maps behind"
    );
}

/// The same double-`jsr` shape, but stamped with a **Java 7+ class-file
/// version (major 51)** where JVMS §4.9.1 *forbids* `jsr`/`jsr_w`/`ret`.
/// Such a class is malformed; HotSpot rejects it and so must CratonVM by
/// default. The `CRATONVM_ALLOW_JSR_RET` escape hatch still forces
/// structural-only acceptance for deployments that must load such jars.
#[test]
fn jsr_double_call_site_java7plus_rejected_by_default_accepted_with_escape_hatch() {
    let mut class = make_pre_java7_jsr_class(
        "clear",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x09, // 0: jsr +9
            0xa8, 0x00, 0x06, // 3: jsr +6
            0xb1, // 6: return
            0x00, 0x00, // 7..8: nop padding (unreachable)
            0x4b, // 9: astore_0
            0xa9, 0x00, // 10: ret 0
        ],
        vec![],
    );
    // Forbidden at major ≥ 51 — mark the class as Java 7.
    class.version = ClassFileVersion::JAVA_7;
    // Default policy: malformed (subroutine opcode at version ≥ 51) → reject.
    let rejected = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(
        rejected.is_err(),
        "subroutine opcode at class-file version 51+ is forbidden (JVMS §4.9.1) \
         and must be rejected by default, got {rejected:?}"
    );
    // Escape hatch on: structural-only acceptance regardless of version.
    let accepted = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        accepted.is_ok(),
        "with CRATONVM_ALLOW_JSR_RET the structurally-valid subroutine method \
         must be accepted even at version 51+, got {accepted:?}"
    );
}

/// F3 closer match to the real `clear()` shape: jsr inside a
/// `try-finally` whose `catch` re-jsr's to the same subroutine.
/// The exception table makes offset 15 (`astore_1`) reachable from
/// the protected range as a handler entry — this mirrors the
/// `Exception table: 0 12 15 any; 15 19 15 any` layout from the
/// ByteBuddy class file.
///
/// Bytecode:
/// ```text
///   0: aconst_null  (placeholder for the parent.clear() effect)
///   1: pop
///   2: jsr +19      → push returnAddress(5),  branch to 21
///   5: goto 28
///   8: nop padding
///   9: nop
///  10: nop
///  11: nop
///  12: nop
///  13: nop
///  14: nop
///  15: astore_1     ← handler entry: store the throwable
///  16: jsr +5       → push returnAddress(19),  branch to 21
///  19: aload_1
///  20: athrow
///  21: astore_2     ← subroutine entry
///  22: aload_0      (filler)
///  23: pop
///  24: nop
///  25: nop
///  26: ret 2
///  28: return
/// ```
/// The exception table protects [0..15) with handler at 15, which
/// in real bytecode would be the `try { parent.clear() } catch (any)`
/// shape that the Java 5 javac emits for `try-finally`. At this legal
/// pre-Java-7 version the class is accepted by default (HotSpot loads it).
#[test]
fn jsr_finally_handler_double_call_site_pre_java7_accepted_by_default() {
    // Build the bytecode array first so we can reference precise
    // offsets in the exception table without juggling magic numbers.
    let code: Vec<u8> = vec![
        0x01, // 0: aconst_null
        0x57, // 1: pop
        0xa8, 0x00, 0x13, // 2: jsr +19  → 21
        0xa7, 0x00, 0x17, // 5: goto +23 → 28
        0x00, 0x00, 0x00, // 8..10: nop
        0x00, 0x00, 0x00, // 11..13: nop
        0x00, // 14: nop
        0x4c, // 15: astore_1
        0xa8, 0x00, 0x05, // 16: jsr +5 → 21
        0x2b, // 19: aload_1
        0xbf, // 20: athrow
        0x4d, // 21: astore_2
        0x2a, // 22: aload_0
        0x57, // 23: pop
        0x00, 0x00, // 24..25: nop
        0xa9, 0x02, // 26: ret 2
        0xb1, // 28: return
    ];
    let exc = vec![
        cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc: 0,
            end_pc: 15,
            handler_pc: 15,
            catch_type: 0, // any
        },
        cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc: 15,
            end_pc: 19,
            handler_pc: 15,
            catch_type: 0,
        },
    ];
    let class = make_pre_java7_jsr_class("clear", "()V", 1, 3, code, exc);
    // Legal pre-Java-7 (major 49) version: HotSpot loads this try-finally
    // double-jsr shape, so CratonVM accepts it after the structural scan —
    // by default, no escape hatch required.
    let accepted_default =
        verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
    assert!(
        accepted_default.is_ok(),
        "ByteBuddy clear()-shape (try-finally with double-jsr to one \
         subroutine) at a legal pre-Java-7 version must be accepted by \
         default, got {accepted_default:?}"
    );
    // Escape hatch: also accepted (structural-only).
    let accepted_hatch = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        accepted_hatch.is_ok(),
        "with CRATONVM_ALLOW_JSR_RET the structurally-valid clear()-shape \
         must be accepted (structural-only), got {accepted_hatch:?}"
    );
}

/// Sanity: structural validation still rejects malformed bytecode
/// in JSR-using methods. A `jsr` whose 16-bit offset points past
/// the end of the bytecode array is rejected by the structural
/// fallback. Exercised with the escape hatch ON so the failure is
/// attributable to the structural pass and not to the default
/// subroutine rejection — the structural scan must still run (and
/// reject) even when JSR methods are tolerated.
#[test]
fn jsr_with_out_of_range_target_is_rejected() {
    let class = make_pre_java7_jsr_class(
        "bad",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x7f, 0xff, // jsr +32767 (way past end of method)
            0xb1, // return
        ],
        vec![],
    );
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        res.is_err(),
        "out-of-range jsr target must still be rejected by structural pass \
         even with the escape hatch on, got {res:?}"
    );
}

/// Sanity: a class whose pre-Java-7 method does NOT use jsr/ret
/// continues to be type-state-verified. Here the method has a stack
/// underflow (`ireturn` on an empty stack) — the standard verifier
/// must catch it because we never enter the JSR-relaxation branch.
#[test]
fn non_jsr_pre_java7_method_still_strictly_verified() {
    let class = make_pre_java7_jsr_class(
        "bad",
        "()I",
        0,
        0,
        vec![0xac], // ireturn on empty stack
        vec![],
    );
    let res = verify_class_bytecode(&class, &PermissiveHierarchy);
    assert!(
        res.is_err(),
        "non-JSR method must continue to be strictly type-state-verified, \
         got {res:?}"
    );
}

/// Sanity: when a JSR method (tolerated under the escape hatch)
/// appears BEFORE a non-JSR method with a type-state bug, the
/// non-JSR failure must still be re-detected — tolerating the JSR
/// method must not short-circuit verification of the rest of the
/// class. Exercised with the escape hatch ON so the JSR method is
/// accepted (structural-only) rather than triggering the default
/// rejection, isolating the non-JSR bug as the cause of failure.
#[test]
fn failing_jsr_method_does_not_mask_later_real_bug() {
    // Build the ByteBuddy clear() shape (which fails the worklist
    // verifier without our fix) followed by a non-JSR method with
    // a stack-underflow type-state bug.
    let mut class = make_pre_java7_jsr_class(
        "clear",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x09, // 0: jsr +9
            0xa8, 0x00, 0x06, // 3: jsr +6
            0xb1, // 6: return
            0x00, 0x00, // 7..8: pad
            0x4b, // 9: astore_0
            0xa9, 0x00, // 10: ret 0
        ],
        vec![],
    );
    class.methods.push(ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("typeStateBug"),
        descriptor: Arc::from("()I"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    });
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        res.is_err(),
        "non-JSR type-state bug must be detected even when a tolerated \
         JSR method precedes it, got {res:?}"
    );
}

/// Sanity: when a JSR method (tolerated under the escape hatch)
/// appears BEFORE a non-JSR method with a type-state bug (stack
/// underflow), the non-JSR method's bug must still be caught.
/// Exercised with the escape hatch ON: the JSR method is accepted
/// structurally, so the failure can only come from the non-JSR bug —
/// proving the per-method routing still type-state-verifies siblings.
#[test]
fn type_state_bug_after_jsr_method_still_detected() {
    let mut class = make_pre_java7_jsr_class(
        "subroutineMethod",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x06, // 0: jsr +6 → 6
            0xb1, // 3: return
            0x00, 0x00, // 4..5: pad
            0x4b, // 6: astore_0
            0xa9, 0x00, // 7: ret 0
        ],
        vec![],
    );
    // Append a non-JSR method with a type-state error (ireturn on
    // empty stack — passes structural check but fails type-state).
    class.methods.push(ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("typeStateBug"),
        descriptor: Arc::from("()I"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    });
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        res.is_err(),
        "type-state bug in non-JSR method must be caught even when a \
         JSR method precedes it in class.methods, got {res:?}"
    );
}

/// Sanity: a class with a mix of JSR-using and non-JSR methods
/// applies the structural-only tolerance only to the JSR ones (under
/// the escape hatch). The non-JSR method here is malformed (truncated
/// bytecode for `getstatic`) and must be rejected even though another
/// method in the same class uses `jsr`. Exercised with the escape
/// hatch ON so the failure is attributable to the malformed non-JSR
/// method, not the default subroutine rejection.
#[test]
fn mixed_jsr_and_non_jsr_methods_isolate_relaxation() {
    let mut class = make_pre_java7_jsr_class(
        "subroutineMethod",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x06, // 0: jsr +6 → 6
            0xb1, // 3: return
            0x00, 0x00, // 4..5: pad
            0x4b, // 6: astore_0
            0xa9, 0x00, // 7: ret 0
        ],
        vec![],
    );
    // Add a second method that does NOT use jsr/ret but is
    // structurally malformed (truncated `getstatic` — opcode 0xb2
    // wants 2 operand bytes; we only supply 1).
    class.methods.push(ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("malformed"),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb2, 0x00]), // getstatic with truncated index
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    });
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        res.is_err(),
        "malformed non-JSR method in a mixed class must still be rejected, \
         got {res:?}"
    );
}

/// SECURITY FIX (HIGH): the default-policy rejection of a
/// subroutine-using method at a *forbidden* class-file version
/// (major ≥ 51) must surface as a `VerifyError` whose message names
/// the `jsr/jsr_w/ret` cause and the `CRATONVM_ALLOW_JSR_RET` escape
/// hatch, so the refusal is diagnosable rather than an opaque failure.
#[test]
fn jsr_default_rejection_is_a_verify_error_with_diagnostic() {
    let mut class = make_pre_java7_jsr_class(
        "clear",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x06, // 0: jsr +6 → 6
            0xb1, // 3: return
            0x00, 0x00, // 4..5: pad
            0x4b, // 6: astore_0
            0xa9, 0x00, // 7: ret 0
        ],
        vec![],
    );
    // Java 7+ (major 51): subroutine opcodes are forbidden → rejected by
    // default with a diagnostic message.
    class.version = ClassFileVersion::JAVA_7;
    match verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false) {
        Err(LinkageError::VerifyError {
            method_name,
            message,
            ..
        }) => {
            assert_eq!(method_name, "clear");
            assert!(
                message.contains("jsr") && message.contains("CRATONVM_ALLOW_JSR_RET"),
                "rejection message must name the jsr/ret cause and the escape \
                 hatch, got: {message}"
            );
        }
        other => panic!("expected VerifyError for default jsr rejection, got {other:?}"),
    }
}

fn assert_legacy_jsr_structural_rejects(code: Vec<u8>, max_locals: u16, needle: &str) {
    let class = make_pre_java7_jsr_class("legacy", "()V", 2, max_locals, code, vec![]);
    match verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false) {
        Err(LinkageError::VerifyError { message, .. }) => {
            assert!(
                message.contains(needle),
                "expected VerifyError containing '{needle}', got: {message}"
            );
        }
        other => panic!("expected structural VerifyError, got {other:?}"),
    }
}

#[test]
fn legacy_jsr_structural_rejects_negative_target() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xa8, 0xff, 0xff, // 0: jsr -1 -> negative target
            0xb1, // 3: return
        ],
        1,
        "Illegal target of jump or branch",
    );
}

#[test]
fn legacy_jsr_structural_rejects_code_len_target() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xa8, 0x00, 0x04, // 0: jsr +4 -> code_len
            0xb1, // 3: return
        ],
        1,
        "Illegal target of jump or branch",
    );
}

#[test]
fn legacy_jsr_structural_rejects_huge_wide_target() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xc9, 0x7f, 0xff, 0xff, 0xff, // 0: jsr_w i32::MAX
            0xb1, // 5: return
        ],
        1,
        "Illegal target of jump or branch",
    );
}

#[test]
fn legacy_jsr_structural_rejects_non_instruction_target() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xa8, 0x00, 0x01, // 0: jsr +1 -> operand byte, not opcode
            0xb1, // 3: return
        ],
        1,
        "Illegal target of jump or branch",
    );
}

#[test]
fn legacy_ret_structural_rejects_uncovered_local() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xa9, 0x00, // 0: ret 0 without a reachable jsr/astore
        ],
        1,
        // Wave 104 hand-back: the subroutine walk answers the uncovered
        // `ret` first, with libverify's register check (register 0 holds
        // `this`, not a return address).
        "(class: Hierarchical, method: legacy signature: ()V) Register 0 contains wrong type",
    );
}

/// Wave 105 hand-back (lane L7, probe `L7W105JsrPcKeyedMerge` R5): a
/// subroutine B called from top level AND from inside A has the
/// intersected mask list [B]; B's `ret` returns with the RET state's list
/// cut below B (libverify's `new_mask_count = i`), i.e. [], so A's `ret`
/// finds A missing, as HotSpot 25 answers.
#[test]
fn shared_subroutine_orphans_the_outer_ret() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0x03, 0x3c, // 0: iconst_0; 1: istore_1
            0xa8, 0x00, 0x0f, // 2: jsr 17 (B)
            0xa8, 0x00, 0x04, // 5: jsr 9 (A)
            0xb1, // 8: return
            0x4d, 0x01, 0x4c, // A 9: astore_2; 10: aconst_null; 11: astore_1
            0xa8, 0x00, 0x05, // 12: jsr 17 (B, from inside A)
            0xa9, 0x02, // 15: ret 2
            0x4e, 0x00, 0x00, // B 17: astore_3; 18: nop; 19: nop
            0xa9, 0x03, // 20: ret 3
        ],
        4,
        "(class: Hierarchical, method: legacy signature: ()V) Illegal return from subroutine",
    );
}

/// Control for the test above: javac's nested `finally` (B called only
/// from inside A) verifies -- B's `ret` returns into A with [A]. The
/// structural scan's coverage rule misses A's `ret 1` at 8 (it sits after
/// the inner `jsr`); the walk checks the whole method and accepts, which
/// overrules that coverage-only error (second wave-105 hand-back).
#[test]
fn nested_subroutine_returns_into_its_caller() {
    let class = make_pre_java7_jsr_class(
        "legacy",
        "()V",
        2,
        3,
        vec![
            0xa8, 0x00, 0x04, // 0: jsr 4 (A)
            0xb1, // 3: return
            0x4c, // A 4: astore_1
            0xa8, 0x00, 0x05, // 5: jsr 10 (B)
            0xa9, 0x01, // 8: ret 1
            0x4d, // B 10: astore_2
            0xa9, 0x02, // 11: ret 2
        ],
        vec![],
    );
    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(res.is_ok(), "javac's nested finally must verify, got {res:?}");
}

#[test]
fn legacy_ret_structural_rejects_local_out_of_range() {
    assert_legacy_jsr_structural_rejects(
        vec![
            0xa9, 0x01, // 0: ret 1 with max_locals = 1
        ],
        1,
        "Illegal local variable number",
    );
}

/// Sanity: the JSR scanner must not be fooled by a `tableswitch`
/// whose jump-table contents happen to contain the byte `0xa8`
/// (the `jsr` opcode). The instruction-level walk steps over the
/// switch's variable-length operand correctly.
#[test]
fn jsr_scanner_handles_tableswitch_padding() {
    // Layout: aligned `tableswitch` whose default and offsets
    // include byte values that would be misread as jsr if the
    // scanner walked one byte at a time. We only assert that the
    // method classifies as non-JSR (so it should reach the strict
    // verifier and fail there if at all).
    //
    //   0: iconst_0      (0x03)
    //   1: tableswitch   (0xaa)
    //   2..3: pad (to 4-byte align after opcode at offset 1: pad to offset 4)
    //   4..7: default = 16
    //   8..11: low = 0
    //  12..15: high = 0
    //  16..19: offset[0] = 12  ← the byte 0x0c, no 0xa8
    //  20: ireturn (0xac)
    let mut code: Vec<u8> = Vec::new();
    code.push(0x03); // 0: iconst_0
    code.push(0xaa); // 1: tableswitch
                     // Pad so default starts at offset (1+1+pad) ≡ 0 mod 4 → next offset must be 4
                     // so we need 2 pad bytes after offset 1
    code.push(0x00);
    code.push(0x00);
    // 4..7: default
    code.extend(&12i32.to_be_bytes());
    // 8..11: low
    code.extend(&0i32.to_be_bytes());
    // 12..15: high
    code.extend(&0i32.to_be_bytes());
    // 16..19: offset[0]
    code.extend(&12i32.to_be_bytes());
    // 20: ireturn (0xac)
    code.push(0xac);

    // Wrap it in the same Class scaffolding.
    let method = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("withSwitch"),
        descriptor: Arc::from("()I"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    };
    // Scanner-level assertion only — we don't care whether the full
    // verifier accepts the synthetic switch.
    assert!(
        !method_uses_jsr_or_ret(&method),
        "tableswitch padding must not be misclassified as jsr"
    );
}

/// Targeted unit test for the `is_pre_java7` helper.
#[test]
fn pre_java7_classification() {
    assert!(is_pre_java7(&ClassFileVersion::JAVA_5));
    assert!(is_pre_java7(&ClassFileVersion::JAVA_6));
    assert!(!is_pre_java7(&ClassFileVersion::JAVA_7));
    assert!(!is_pre_java7(&ClassFileVersion::JAVA_8));
    assert!(!is_pre_java7(&ClassFileVersion::JAVA_25));
}

// -----------------------------------------------------------------
// MED — invokespecial <init> new-site owner-match (JVMS §4.10.1.9)
// -----------------------------------------------------------------

/// Constant pool whose entry #6 is a `Methodref` to `D.<init>()V`.
fn init_owner_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                        // 0
        ConstantPoolEntry::Utf8("D".into()),                 // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2 (owner D)
        ConstantPoolEntry::Utf8("<init>".into()),            // 3
        ConstantPoolEntry::Utf8("()V".into()),               // 4
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6 (D.<init>()V)
    ])
}

fn owner_match_frame() -> VerificationFrame {
    // A frame with a single `Uninitialized(0)` receiver on the stack.
    let mut frame = VerificationFrame::initial_frame("Test", "test", "()V", true, 1, 4);
    frame.push(VType::Uninitialized(0)).unwrap();
    frame
}

#[test]
fn invokespecial_init_owner_mismatch_rejected() {
    // `new C` (recorded at offset 0) followed by `invokespecial D.<init>`
    // is a JVMS §4.10.1.9 violation: the new-site type C must equal the
    // constructor owner D.
    let cp = init_owner_cp();
    let frame = owner_match_frame();
    let mut new_site_classes = std::collections::HashMap::new();
    new_site_classes.insert(0u16, std::sync::Arc::<str>::from("C")); // new C, not D

    let res = check_new_init_owner_match(
        &Instruction::Invokespecial(6),
        &frame,
        &cp,
        &new_site_classes,
        "Test",
        "test",
    );
    assert!(
        res.is_err(),
        "invokespecial D.<init> on an object created by `new C` must be rejected, \
         got {res:?}"
    );
}

#[test]
fn invokespecial_init_owner_match_accepted() {
    // `new D` followed by `invokespecial D.<init>` is well-formed.
    let cp = init_owner_cp();
    let frame = owner_match_frame();
    let mut new_site_classes = std::collections::HashMap::new();
    new_site_classes.insert(0u16, std::sync::Arc::<str>::from("D")); // new D == owner

    let res = check_new_init_owner_match(
        &Instruction::Invokespecial(6),
        &frame,
        &cp,
        &new_site_classes,
        "Test",
        "test",
    );
    assert!(
        res.is_ok(),
        "invokespecial D.<init> on an object created by `new D` must be accepted, \
         got {res:?}"
    );
}

#[test]
fn invokespecial_init_unknown_new_site_is_skipped() {
    // If the `new` site was never recorded (e.g. the uninitialized value
    // arrived via a declared StackMapTable frame), we cannot compare —
    // the check must be a no-op rather than a false rejection.
    let cp = init_owner_cp();
    let frame = owner_match_frame();
    let new_site_classes: std::collections::HashMap<u16, std::sync::Arc<str>> =
        std::collections::HashMap::new(); // offset 0 not present

    let res = check_new_init_owner_match(
        &Instruction::Invokespecial(6),
        &frame,
        &cp,
        &new_site_classes,
        "Test",
        "test",
    );
    assert!(
        res.is_ok(),
        "an unrecorded new-site must skip the owner-match (no false reject), \
         got {res:?}"
    );
}

#[test]
fn invokespecial_init_owner_match_end_to_end_rejected() {
    // End-to-end through the pre-Java-7 linear walk: a Java-5 method that
    // does `new C; dup; invokespecial D.<init>()V`. The class also carries
    // a jsr method so the per-method routing engages (escape hatch on so
    // the jsr method is tolerated and the owner mismatch is the only cause
    // of failure).
    //
    // Bytecode of the offending method:
    //   0: new #2  (creates D per cp, but we point new at a *different*
    //               class via a dedicated cp below)
    //   3: dup
    //   4: invokespecial #? D.<init>()V
    //   7: return
    //
    // We build a bespoke cp where the `new` index names C and the
    // constructor names D.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                        // 0
        ConstantPoolEntry::Utf8("C".into()),                 // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2 (new C)
        ConstantPoolEntry::Utf8("D".into()),                 // 3
        ConstantPoolEntry::ClassReference { name_index: 3 }, // 4 (owner D)
        ConstantPoolEntry::Utf8("<init>".into()),            // 5
        ConstantPoolEntry::Utf8("()V".into()),               // 6
        ConstantPoolEntry::NameAndType {
            name_index: 5,
            descriptor_index: 6,
        }, // 7
        ConstantPoolEntry::MethodReference {
            class_index: 4,
            name_and_type_index: 7,
        }, // 8 (D.<init>()V)
    ]);

    // new #2 (C); dup; invokespecial #8 (D.<init>); return
    let code = vec![
        0xbb, 0x00, 0x02, // 0: new C
        0x59, // 3: dup
        0xb7, 0x00, 0x08, // 4: invokespecial D.<init>
        0xb1, // 7: return
    ];

    let mut class = make_pre_java7_jsr_class(
        "subroutine",
        "()V",
        1,
        1,
        vec![
            0xa8, 0x00, 0x06, // 0: jsr +6 → 6
            0xb1, // 3: return
            0x00, 0x00, // 4..5: pad
            0x4b, // 6: astore_0
            0xa9, 0x00, // 7: ret 0
        ],
        vec![],
    );
    class.constant_pool = cp;
    class.methods.push(ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from("makeMismatch"),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 2,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    });

    let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
    assert!(
        res.is_err(),
        "new C; invokespecial D.<init> must be rejected by the owner-match, got {res:?}"
    );
}

// =======================================================================
// Map collection without a load decision — the user-defined-loader
// coverage gap (arch-2026-07-26/access-control-and-map-coverage)
// =======================================================================

/// A method whose bytecode the walk cannot get through: `pop` on an empty
/// operand stack.
fn underflow_method(name: &str) -> ClassFileMethod {
    plain_method(name, vec![0x57, 0xb1], 1, 1) // pop; return
}

/// The property the whole coverage fix rests on: a class whose Pass-3
/// *verdict* is deferred still gets real maps, and one unverifiable method
/// does not cost its siblings theirs.
#[test]
fn collect_class_type_maps_never_rejects_and_keeps_good_methods() {
    let class = make_pre_java7_class_with_id(
        91_001,
        vec![
            make_method("abs", "()V", MethodAccessFlags::ABSTRACT, false),
            underflow_method("cannotWalk"),
            // aconst_null; astore_0; return
            plain_method("fine", vec![0x01, 0x4b, 0xb1], 1, 1),
            double_jsr_method("legacyFinally"),
        ],
    );

    // Control: the *verifying* entry point rejects this class outright, so
    // before the fix a deferred-Pass-3 class published nothing at all.
    assert!(
        verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false).is_err(),
        "fixture must be one the verifying path rejects"
    );

    let maps = collect_class_type_maps(&class, &PermissiveHierarchy);
    assert!(maps.verified(), "collection is not a `skipped` marker");
    assert_eq!(maps.method_count(), 4, "one entry per Class::methods slot");

    assert!(maps.method(0).is_none(), "abstract method has no body");

    let bad = maps
        .method(1)
        .expect("unwalkable method still gets an entry");
    assert_eq!(bad.entry_count(), 0, "no rows may be invented for it");
    assert_eq!(
        bad.fast_path_veto(),
        Some(FastPathVeto::IncompleteWalk),
        "the reason must be recorded, not merely the absence of rows"
    );
    assert!(!bad.safe_for_fast_path());

    let good = maps.method(2).expect("the sibling keeps its maps");
    assert!(
        good.entry_count() > 0,
        "a walkable method must not be punished for its sibling"
    );
    assert!(
        good.local_oops_at(2).expect("row at pc 2").get(0),
        "local 0 holds the stored reference from pc 2 onward"
    );
    assert!(good.safe_for_fast_path());

    assert_eq!(
        maps.method(3).and_then(|m| m.fast_path_veto()),
        Some(FastPathVeto::Subroutine),
        "the jsr/ret method keeps its own, more informative veto"
    );
}

/// `publish_deferred_class_type_maps` must make the class visible to
/// `type_maps_for` / `verification_status` — the deferred path's whole
/// point. Before the fix these answered `Unknown` for every
/// user-defined-loader class in the process.
#[test]
fn deferred_publish_makes_maps_visible() {
    let class = make_pre_java7_class_with_id(
        91_002,
        vec![plain_method("fine", vec![0x01, 0x4b, 0xb1], 1, 1)],
    );
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Unknown,
        "nothing published yet"
    );
    assert!(publish_deferred_class_type_maps(
        &class,
        &PermissiveHierarchy
    ));
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Verified
    );
    assert!(crate::type_maps::type_maps_for(class.id, 0)
        .is_some_and(|m| m.entry_count() > 0 && m.safe_for_fast_path()));
}

/// JIT round 23 wave 8f (`CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY`): the
/// loads-only walk visits the same instructions (so the same hierarchy
/// questions, and the same verifier loads) as the collecting walk, builds
/// no row and publishes nothing.
#[test]
fn a_loads_only_walk_asks_what_the_collecting_walk_asks_and_builds_no_row() {
    /// Counts every hierarchy question the walk asks.
    struct Counting(std::cell::Cell<usize>);
    impl crate::vtype::ClassHierarchy for Counting {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            self.0.set(self.0.get() + 1);
            true
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            self.0.set(self.0.get() + 1);
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            self.0.set(self.0.get() + 1);
            false
        }
        fn records_verifier_loads(&self) -> bool {
            true
        }
        fn note_verifier_load(&self, _name: &str) {
            self.0.set(self.0.get() + 1);
        }
    }
    let class = make_pre_java7_class_with_id(
        91_008,
        vec![
            // aconst_null; astore_0; return
            plain_method("fine", vec![0x01, 0x4b, 0xb1], 1, 1),
            underflow_method("cannotWalk"),
        ],
    );
    let collecting = Counting(std::cell::Cell::new(0));
    let maps = collect_class_type_maps(&class, &collecting);
    assert!(maps.method(0).is_some_and(|m| m.entry_count() > 0));
    let loads_only = Counting(std::cell::Cell::new(0));
    walk_class_for_verifier_loads(&class, &loads_only);
    assert_eq!(
        loads_only.0.get(),
        collecting.0.get(),
        "the same questions, in the same walk"
    );
    assert_eq!(
        collect_class_type_maps_with(&class, &PermissiveHierarchy, false)
            .method(0)
            .map(|m| m.entry_count()),
        Some(0),
        "a loads-only walk records no row"
    );
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Unknown,
        "and publishes nothing"
    );
}

/// REDEFINE: `publish_class_type_maps` is first-writer-wins, so after a
/// class's bytecode is replaced the old maps would keep describing the old
/// bodies unless something *replaces* them. A stale oop map is a wrong oop
/// map. `refresh_class_type_maps` is what closes that.
#[test]
fn refresh_replaces_stale_maps_after_a_redefine() {
    // v1: one 3-instruction method.
    let mut class = make_pre_java7_class_with_id(
        91_003,
        vec![plain_method("m", vec![0x01, 0x4b, 0xb1], 1, 1)],
    );
    assert!(publish_deferred_class_type_maps(
        &class,
        &PermissiveHierarchy
    ));
    let v1_rows = crate::type_maps::type_maps_for(class.id, 0)
        .expect("v1 maps")
        .entry_count();
    assert_eq!(v1_rows, 3);

    // v2: the redefined body is longer. A no-replace publish is a no-op...
    class.methods = vec![plain_method("m", vec![0x01, 0x4b, 0x01, 0x4b, 0xb1], 1, 1)];
    assert!(
        !crate::type_maps::publish_class_type_maps(
            class.id,
            collect_class_type_maps(&class, &PermissiveHierarchy)
        ),
        "first-writer-wins: this is exactly why redefine needs `replace`"
    );
    assert_eq!(
        crate::type_maps::type_maps_for(class.id, 0)
            .expect("maps")
            .entry_count(),
        v1_rows,
        "the stale v1 rows are still what a consumer would read"
    );

    // ...and `refresh` is what actually installs the new bodies' maps.
    assert!(refresh_class_type_maps(&class, &PermissiveHierarchy));
    assert_eq!(
        crate::type_maps::type_maps_for(class.id, 0)
            .expect("v2 maps")
            .entry_count(),
        5,
        "the maps must describe the bytecode that is actually installed"
    );
}

/// `refresh_class_type_maps` must also overwrite a
/// `mark_class_verification_skipped` marker — a class defined with
/// `skip_verification` and later redefined into ordinary bytecode should
/// stop answering `Skipped`.
#[test]
fn refresh_overwrites_a_verification_skipped_marker() {
    let class = make_pre_java7_class_with_id(
        91_004,
        vec![plain_method("m", vec![0x01, 0x4b, 0xb1], 1, 1)],
    );
    assert!(crate::type_maps::mark_class_verification_skipped(class.id));
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Skipped
    );
    assert!(refresh_class_type_maps(&class, &PermissiveHierarchy));
    assert_eq!(
        crate::type_maps::verification_status(class.id),
        crate::type_maps::VerificationStatus::Verified
    );
}

// =====================================================================
// JVMS §4.9.1 structural bytecode verification
// =====================================================================

/// A method with a `Code` attribute, a custom descriptor and a custom
/// exception table — the shapes the structural scan is about.
fn structural_method(
    name: &str,
    descriptor: &str,
    max_stack: u16,
    max_locals: u16,
    code: Vec<u8>,
    exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table,
                attributes: vec![],
            },
        )))],
    }
}

fn handler(
    start_pc: u16,
    end_pc: u16,
    handler_pc: u16,
) -> cratonvm_reader::attribute::ExceptionTableEntry {
    cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type: 0, // catch-all (finally)
    }
}

/// `iconst_0; istore_0; return` with a valid catch-all handler — the valid
/// counterpart every rejection test below is measured against.
fn well_formed_body() -> Vec<u8> {
    vec![0x03, 0x3b, 0xb1]
}

#[test]
fn structural_accepts_a_well_formed_method() {
    let class = make_pre_java7_class_with_id(
        92_001,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            well_formed_body(),
            vec![handler(0, 2, 2)],
        )],
    );
    verify_class_structural_bytecode(&class).expect("a well-formed method must pass");
}

#[test]
fn structural_rejects_handler_pc_past_the_code_array() {
    let class = make_pre_java7_class_with_id(
        92_002,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            well_formed_body(),
            vec![handler(0, 2, 99)],
        )],
    );
    let err = verify_class_structural_bytecode(&class)
        .expect_err("a handler_pc past the end of the code array must be rejected");
    assert!(err.to_string().contains("handler_pc"), "{err}");
}

#[test]
fn structural_rejects_handler_pc_inside_an_instruction() {
    // `sipush` is 3 bytes (0x11 hi lo); handler_pc = 1 lands on its operand.
    let class = make_pre_java7_class_with_id(
        92_003,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            vec![0x11, 0x00, 0x01, 0x57, 0xb1], // sipush 1; pop; return
            vec![handler(0, 3, 1)],
        )],
    );
    let err = verify_class_structural_bytecode(&class)
        .expect_err("a handler entry inside an instruction must be rejected");
    assert!(err.to_string().contains("instruction boundary"), "{err}");
}

#[test]
fn structural_rejects_inverted_handler_range() {
    let class = make_pre_java7_class_with_id(
        92_004,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            well_formed_body(),
            vec![handler(2, 0, 2)],
        )],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
}

#[test]
fn structural_rejects_empty_handler_range() {
    let class = make_pre_java7_class_with_id(
        92_005,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            well_formed_body(),
            vec![handler(1, 1, 2)],
        )],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
}

// ---------------------------------------------------------------------
// JVMS §4.7.3 `catch_type`, enforced by `bytecode_verifier::catch_type_of`
//
// Not part of the structural scan: the rule is a constant-pool
// cross-check, so it runs on the type-state pass rather than the
// hierarchy-independent one. `cratonvm_reader` enforces the same rule
// while it decodes the `Code` attribute and therefore rejects malformed
// *bytes* first — which is why the corpus case for this shape
// (`handler_with_a_non_class_catch_type_rejected`) asserts at the reader.
// These are the verifier's own half, reached the way any in-memory
// `CodeAttribute` producer reaches it — synthetic stubs, cached class data,
// tests — where no reader ever saw the bytes.
// ---------------------------------------------------------------------

/// `sipush 1; pop; return; astore_1; return` — a region guarded over
/// `0..4` whose handler entry at 5 is reached only along the exception
/// edge. Instruction boundaries at 0, 3, 4, 5, 6.
fn catch_type_body() -> Vec<u8> {
    vec![0x11, 0x00, 0x01, 0x57, 0xb1, 0x4c, 0xb1]
}

/// [`catch_type_body`] guarded by a single handler with the given
/// `catch_type`, over a caller-supplied constant pool.
fn catch_type_class(raw_id: u32, cp: ConstantPool, catch_type: u16) -> Class {
    let mut class = make_pre_java7_class_with_id(
        raw_id,
        vec![structural_method(
            "m",
            "()V",
            1,
            2,
            catch_type_body(),
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 0,
                end_pc: 4,
                handler_pc: 5,
                catch_type,
            }],
        )],
    );
    class.constant_pool = cp;
    class
}

#[test]
fn rejects_a_handler_whose_catch_type_is_not_a_class() {
    // Entry #1 is a Utf8, not a CONSTANT_Class.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                  // 0
        ConstantPoolEntry::Utf8("not a class".into()), // 1
    ]);
    let class = catch_type_class(92_101, cp, 1);
    let err = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
        .expect_err("a catch_type that is not a CONSTANT_Class must be rejected");
    assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
}

#[test]
fn rejects_a_handler_whose_catch_type_is_out_of_range() {
    // Nothing at index 7 at all — the pre-fix fallback silently typed the
    // handler entry as `java/lang/Throwable` and verified clean.
    let class = catch_type_class(92_102, empty_cp(), 7);
    let err = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
        .expect_err("an out-of-range catch_type must be rejected");
    assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
}

/// The valid counterparts: without these, the two rejections above are
/// satisfied by a verifier that refuses every handler.
#[test]
fn accepts_a_handler_whose_catch_type_is_a_class() {
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                          // 0
        ConstantPoolEntry::Utf8("java/lang/Exception".into()), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 },   // 2
    ]);
    let class = catch_type_class(92_103, cp, 2);
    verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
        .expect("a CONSTANT_Class catch_type must verify");
}

#[test]
fn accepts_a_catch_all_handler() {
    // `catch_type == 0` is the `finally` form and names no pool entry.
    let class = catch_type_class(92_104, empty_cp(), 0);
    verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
        .expect("a catch-all handler must verify");
}

#[test]
fn structural_rejects_local_index_past_max_locals() {
    // `istore_3` with max_locals = 1.
    let class = make_pre_java7_class_with_id(
        92_006,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            vec![0x03, 0x3e, 0xb1], // iconst_0; istore_3; return
            vec![],
        )],
    );
    let err = verify_class_structural_bytecode(&class)
        .expect_err("a local operand past max_locals must be rejected");
    assert!(err.to_string().contains("max_locals"), "{err}");
}

#[test]
fn structural_rejects_cat2_local_straddling_max_locals() {
    // `lstore_1` needs slots 1 and 2, but max_locals = 2 stops at slot 1.
    let class = make_pre_java7_class_with_id(
        92_007,
        vec![structural_method(
            "m",
            "()V",
            2,
            2,
            vec![0x09, 0x40, 0xb1], // lconst_0; lstore_1; return
            vec![],
        )],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
    // The same body with one more local slot is fine.
    let ok = make_pre_java7_class_with_id(
        92_008,
        vec![structural_method(
            "m",
            "()V",
            2,
            3,
            vec![0x09, 0x40, 0xb1],
            vec![],
        )],
    );
    verify_class_structural_bytecode(&ok).expect("lstore_1 fits when max_locals is 3");
}

#[test]
fn structural_rejects_max_locals_smaller_than_the_argument_slots() {
    // `static m(J)V` needs 2 local slots for its single `long` argument.
    let class = make_pre_java7_class_with_id(
        92_009,
        vec![structural_method("m", "(J)V", 1, 1, vec![0xb1], vec![])],
    );
    let err = verify_class_structural_bytecode(&class)
        .expect_err("max_locals must cover the method's own arguments");
    assert!(err.to_string().contains("max_locals"), "{err}");

    let ok = make_pre_java7_class_with_id(
        92_010,
        vec![structural_method("m", "(J)V", 1, 2, vec![0xb1], vec![])],
    );
    verify_class_structural_bytecode(&ok).expect("max_locals = 2 covers a long argument");
}

#[test]
fn structural_rejects_out_of_range_branch_target() {
    // `goto +100` in a 4-byte method.
    let class = make_pre_java7_class_with_id(
        92_011,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            vec![0xa7, 0x00, 0x64, 0xb1],
            vec![],
        )],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
}

#[test]
fn structural_rejects_branch_into_the_middle_of_an_instruction() {
    // `goto +2` lands on the second byte of the following `sipush`.
    let class = make_pre_java7_class_with_id(
        92_012,
        vec![structural_method(
            "m",
            "()V",
            1,
            1,
            vec![
                0xa7, 0x00, 0x02, // 0: goto 2   (mid-`goto` operand)
                0x11, 0x00, 0x01, // 3: sipush 1
                0x57, // 6: pop
                0xb1, // 7: return
            ],
            vec![],
        )],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
}

#[test]
fn structural_rejects_empty_code_array() {
    let class = make_pre_java7_class_with_id(
        92_013,
        vec![structural_method("m", "()V", 1, 1, vec![], vec![])],
    );
    assert!(verify_class_structural_bytecode(&class).is_err());
}

#[test]
fn structural_skips_abstract_and_native_methods() {
    // They have no `Code`, so there is nothing to scan and nothing to
    // reject — the presence rules live in Pass 2.
    let mut class = make_pre_java7_class_with_id(92_014, vec![]);
    class.methods = vec![
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            name: Arc::from("a"),
            descriptor: Arc::from("()V"),
            attributes: vec![],
        },
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: Arc::from("n"),
            descriptor: Arc::from("(J)V"),
            attributes: vec![],
        },
    ];
    verify_class_structural_bytecode(&class).expect("no Code, nothing to verify");
}
