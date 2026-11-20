use super::*;
use crate::classloading::{ClassLoaderId, ClassState};
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;

pub(crate) fn named_method(
    name: &str,
    descriptor: &str,
    lines: Vec<LineNumberEntry>,
) -> ClassFileMethod {
    // j27 wave 5 (lane REFSP): with rows, a body covering every bci the
    // fixtures resolve (255 `nop`s, then `return`): the line lookup
    // answers none at or past the code's end, as HotSpot does, and a
    // parsed class's rows all start inside its code. A lone `return`
    // without rows, as before.
    let body = if lines.is_empty() {
        vec![0xb1]
    } else {
        let mut body = vec![0x00; 255];
        body.push(0xb1);
        body
    };
    let code = CodeAttribute {
        max_stack: 1,
        max_locals: 1,
        code: cratonvm_reader::ByteView::from_vec(body),
        exception_table: Vec::new(),
        attributes: vec![Attribute::LineNumberTable(lines)],
    };
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
            Attribute::Code(Box::new(code)),
        )],
    }
}

pub(crate) fn store_with(methods: Vec<ClassFileMethod>) -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let id = store.next_id();
    let class = Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from("probe/Target"),
        source_file: Some("Target.java".to_string()),
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: Vec::new(),
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
    };
    let id = store.add(class);
    (store, id)
}
