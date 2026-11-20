use super::*;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::method::ClassFileMethod;

fn method(name: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::STATIC,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

#[test]
fn metadata_is_bound_to_the_exact_method_version() {
    let method = method("call");
    let row = ExecuteEntryRecord::new(3, &method, Some("Old.java")).unwrap();
    assert!(row.matches(&method.clone(), Some("Old.java")));
    assert!(
        !row.matches(&self::method("call"), Some("Old.java")),
        "EMCP reload"
    );
    assert!(
        !row.matches(&method, Some("New.java")),
        "source metadata upgrade"
    );
    let mut changed = method.clone();
    changed.access_flags |= MethodAccessFlags::SYNCHRONIZED;
    assert!(!row.matches(&changed, Some("Old.java")));
    changed = method.clone();
    if let LazyAttribute::Decoded(Attribute::Code(code)) = &mut changed.attributes[0] {
        code.max_stack = 1;
    }
    assert!(!row.matches(&changed, Some("Old.java")), "metadata-only code update");
    changed = method.clone();
    changed.name = Arc::from("other");
    assert!(
        !row.matches(&changed, Some("Old.java")),
        "index/hash collision"
    );
    assert!(Arc::ptr_eq(
        &row.code.exception_table,
        &row.code.clone().exception_table
    ));
}

#[test]
fn gate_verdicts_expire_and_constructors_use_the_live_finalizer_memo() {
    let row = ExecuteEntryRecord::new(0, &method("call"), None).unwrap();
    assert_eq!(row.gate(2), None);
    row.note_gate(2, true);
    assert_eq!(row.gate(2), Some(true));
    assert_eq!(row.gate(3), None);
    let ctor = ExecuteEntryRecord::new(0, &method("<init>"), None).unwrap();
    ctor.note_gate(2, false);
    assert_eq!(ctor.gate(2), None);
}

#[test]
fn seals_and_unseals_expire_the_entry_verdict() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let row = ExecuteEntryRecord::new(0, &method("call"), None).unwrap();
    let key = (Arc::from("Probe"), Arc::from("call"), Arc::from("()V"));
    assert!(!row.skipped(&vm, &key));
    vm.jit.jit_skip_set.write().insert(key.clone());
    assert!(row.skipped(&vm, &key));
    vm.jit.jit_skip_set.write().retain(|_| false);
    assert!(!row.skipped(&vm, &key));
}

#[test]
fn compiled_misses_are_stamped_by_this_vms_cache_generation() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let row = ExecuteEntryRecord::new(0, &method("call"), None).unwrap();
    assert!(row.compiled(&vm, ClassId::new(7), "Probe").is_none());
    assert_eq!(row.body.read().0, vm.jit.jit_cache.read().generation());
    let publish = || {
        let body = cratonvm_jit::CompiledMethod::new(
            cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer"),
        );
        vm.jit.jit_cache.read().put(
            "Probe".into(),
            "call".into(),
            "()V".into(),
            ClassId::new(7),
            body,
        );
    };
    publish();
    let first =
        cratonvm_jit::RetainedCode::new(row.compiled(&vm, ClassId::new(7), "Probe").unwrap());
    let hit =
        cratonvm_jit::RetainedCode::new(row.compiled(&vm, ClassId::new(7), "Probe").unwrap());
    assert_eq!(first.artifact_id, hit.artifact_id);
    vm.jit.jit_cache.read().clear_all();
    assert!(
        row.compiled(&vm, ClassId::new(7), "Probe").is_none(),
        "withdrawn body"
    );
    publish();
    let next =
        cratonvm_jit::RetainedCode::new(row.compiled(&vm, ClassId::new(7), "Probe").unwrap());
    assert_ne!(
        first.artifact_id, next.artifact_id,
        "replacement publication"
    );
    let other_vm = SharedVm::new(crate::config::VmConfig::default());
    assert!(other_vm.jit.execute_entry_records.read().is_empty());
}
