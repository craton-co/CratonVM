// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
//! Per-VM native-to-Java entry metadata. This is a view used by `execute`,
//! never a second compiled-entry door. Live policy gates still run there.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct ExecuteEntryRecord {
    pub(super) index: usize,
    pub(super) name: Arc<str>,
    pub(super) descriptor: Arc<str>,
    pub(super) code: ExecuteCode,
    pub(super) source_file: Option<Arc<str>>,
    flags: MethodAccessFlags,
    // Constructors deliberately use the ordinary gate memo: a finalizable
    // subclass definition can withdraw their positive verdict without a redefine.
    gate: AtomicU64,
    seal: AtomicU64,
    // RetainedCode is required even for a memo: eviction must retire executable
    // memory through the quiescence protocol when this is its final owner.
    body: parking_lot::RwLock<(u64, Option<cratonvm_jit::RetainedCode>)>,
}

#[cfg(test)]
mod tests {
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
}

impl ExecuteEntryRecord {
    pub(super) fn matches(
        &self,
        method: &cratonvm_reader::method::ClassFileMethod,
        source_file: Option<&str>,
    ) -> bool {
        method.code().is_some_and(|code| {
            let old: &[u8] = &self.code.code;
            let new: &[u8] = &code.code;
            std::ptr::eq(old.as_ptr(), new.as_ptr())
                && old.len() == new.len()
                && self.code.max_stack == code.max_stack
                && self.code.max_locals == code.max_locals
                && &*self.code.exception_table == code.exception_table.as_slice()
                && self.name == method.name
                && self.descriptor == method.descriptor
                && self.flags == method.access_flags
                && self.source_file.as_deref() == source_file
        })
    }

    pub(super) fn new(
        index: usize,
        method: &cratonvm_reader::method::ClassFileMethod,
        source_file: Option<&str>,
    ) -> Option<Self> {
        let code = method.code()?;
        Some(Self {
            index,
            name: method.name.clone(),
            descriptor: method.descriptor.clone(),
            code: ExecuteCode {
                max_stack: code.max_stack,
                max_locals: code.max_locals,
                code: code.code.clone(),
                exception_table: if code.exception_table.is_empty() {
                    Arc::default()
                } else {
                    execute_exception_table(&code.exception_table)
                },
            },
            source_file: source_file.map(Arc::from),
            flags: method.access_flags,
            gate: AtomicU64::new(0),
            seal: AtomicU64::new(0),
            body: parking_lot::RwLock::new((0, None)),
        })
    }

    #[inline]
    pub(super) fn gate(&self, epoch: u32) -> Option<bool> {
        if &*self.name == "<init>" {
            return None;
        }
        let saved = self.gate.load(Ordering::Acquire);
        (saved >> 1 == u64::from(epoch) + 1).then_some(saved & 1 != 0)
    }

    #[inline]
    pub(super) fn note_gate(&self, epoch: u32, iface: bool) {
        if &*self.name != "<init>" {
            self.gate.store(
                ((u64::from(epoch) + 1) << 1) | u64::from(iface),
                Ordering::Release,
            );
        }
    }

    /// Every skip-set writer changes the version before touching the set;
    /// an odd/in-flight version can never authorize a memo hit.
    #[inline]
    pub(super) fn skipped(&self, shared: &SharedVm, key: &(Arc<str>, Arc<str>, Arc<str>)) -> bool {
        let set = &shared.jit.jit_skip_set;
        let epoch = set.epoch();
        let saved = self.seal.load(Ordering::Acquire);
        if epoch != 0 && epoch & 1 == 0 && saved & !1 == epoch {
            return saved & 1 != 0;
        }
        let seals = set.read();
        let epoch = set.epoch();
        let skipped = seals.contains(key);
        drop(seals);
        self.seal
            .store(epoch | u64::from(skipped), Ordering::Release);
        skipped
    }

    pub(super) fn compiled(
        &self,
        shared: &SharedVm,
        class_id: ClassId,
        class_name: &str,
    ) -> Option<Arc<cratonvm_jit::CompiledMethod>> {
        let cache = shared.jit.jit_cache.read();
        let generation = cache.generation();
        {
            let body = self.body.read();
            if body.0 == generation {
                return body.1.as_ref().map(|body| body.arc().clone());
            }
        }
        let body = cache.get_for_reentry(class_name, &self.name, &self.descriptor, class_id);
        *self.body.write() = (
            generation,
            body.clone().map(cratonvm_jit::RetainedCode::new),
        );
        body
    }
}
