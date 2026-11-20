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
mod tests;

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
