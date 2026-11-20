// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Zero-fault CHA + `@Stable` + lambda-capture heap specialization
//! (`docs/feature-designs/jit-ir-optimizations.md`).
//!
//! Snapshots `static final` root graphs and monomorphic receiver singletons on
//! mutator threads without OS page protections (`mprotect` / `VirtualProtect`),
//! signal/VEH fault handlers, or non-moving GC slabs:
//! - Constant object references and singleton identity guards materialize via
//!   GC-relocated JNI global root slots (`Op::IndyRootRef`).
//! - Instance fields are folded when Class Hierarchy Analysis (CHA) proves no
//!   loaded class writes the field outside its declaring `<init>` on `this`,
//!   or when the field is `@jdk.internal.vm.annotation.Stable` with a
//!   non-default value (`!= 0` / non-null).
//! - Dynamic class definitions, redefinitions, reflection, `Unsafe`,
//!   `VarHandle`, and JNI field writes invalidate dependent compiled bodies
//!   before mutation takes effect (`JitCache::invalidate_forcing_exits`).

use crate::classloading::{Class, ClassId, ClassStore};
use crate::vm::SharedVm;
use cratonvm_jit::frozen_facts::{
    CHA_FIELD_DEP_PREFIX, FrozenFacts, FrozenFactsState, FrozenMiss, FrozenObject, FrozenValue,
};
use cratonvm_reader::constant_pool::ConstantPoolEntry;
use cratonvm_types::{ClassLoaderId, ObjectKind, ObjectRef, Value};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const OBJECT_CAP: usize = 4096;
const SLOT_CAP: usize = 4096;
const DEMAND_CAP: usize = 512;
const MAX_DEPTH: u8 = 6;
const ARRAY_ELEM_CAP: usize = 256;
const RECEIVER_SAMPLE_MASK: u32 = 15;
const RECEIVER_MIN_SAMPLES: u32 = 8;
const REFRESH_BACKOFF: u64 = 8;

/// `(declaring_class_id, method_name, descriptor, site_pc)`.
pub type ReceiverSite = (u32, Arc<str>, Arc<str>, usize);

#[derive(Default)]
struct ChaState {
    scanned_up_to: u32,
    /// `(declaring_class_name, field_name, field_desc)` written outside `<init>`
    /// or inside `<init>` on a non-`this` receiver.
    mutable_fields: FxHashSet<(Arc<str>, Arc<str>, Arc<str>)>,
    /// `(declaring_class_name, field_name, field_desc)` mutated dynamically via
    /// reflection, `Unsafe`, `VarHandle`, or JNI (overrides `@Stable` as well).
    dynamically_mutated: FxHashSet<(Arc<str>, Arc<str>, Arc<str>)>,
    /// Symbolic `putfield` owners not yet loaded when their caller was scanned:
    /// `owner_class_name -> Vec<(field_name, field_desc)>`.
    pending_putfields: FxHashMap<Arc<str>, Vec<(Arc<str>, Arc<str>)>>,
    /// Declaring class internal names that contributed at least one folded
    /// field/array fact to a snapshot in this VM.
    active_dep_owners: FxHashSet<Arc<str>>,
}

#[derive(Default)]
struct ReceiverTracker {
    /// `(address_epoch, raw_addr, samples, others)`.
    raw_classes: FxHashMap<u32, (u64, usize, u32, u32)>,
    raw_sites: FxHashMap<ReceiverSite, (u64, usize, u32, u32)>,
    /// Confirmed monomorphic singletons: `key -> (slot_addr, samples, others)`.
    confirmed_classes: FxHashMap<u32, (usize, u32, u32)>,
    confirmed_sites: FxHashMap<ReceiverSite, (usize, u32, u32)>,
    poisoned_classes: FxHashSet<u32>,
    poisoned_sites: FxHashSet<ReceiverSite>,
    cached_table: Option<Arc<HashMap<String, usize>>>,
}

/// Per-VM mutable state for heap specialization, owned by `ClassRealm`.
pub struct FrozenVmState {
    pub facts: parking_lot::RwLock<Option<Arc<FrozenFacts>>>,
    pub facts_state: Arc<FrozenFactsState>,
    pub dirty: AtomicBool,
    pub has_snapshots: AtomicBool,
    pub cha_initialized: AtomicBool,
    pub cha_leaf_watched_nonempty: AtomicBool,
    pub cha_leaf_watched: parking_lot::Mutex<rustc_hash::FxHashSet<Arc<str>>>,
    cha: parking_lot::Mutex<ChaState>,
    minted_slots: parking_lot::Mutex<Vec<usize>>,
    receivers: parking_lot::Mutex<ReceiverTracker>,
    refresh_lock: parking_lot::Mutex<()>,
    receiver_sample: AtomicU32,
    clock_start: Instant,
    last_want_epoch: AtomicU64,
    last_refresh_end_us: AtomicU64,
    last_refresh_us: AtomicU64,
    snapshots: AtomicU64,
    last_fact_objects: AtomicU64,
    refolds: AtomicU64,
    demanded: AtomicU64,
    reported_exit: AtomicBool,
}

impl Default for FrozenVmState {
    fn default() -> Self {
        Self {
            facts: parking_lot::RwLock::new(None),
            facts_state: Arc::new(FrozenFactsState::default()),
            dirty: AtomicBool::new(false),
            has_snapshots: AtomicBool::new(false),
            cha_initialized: AtomicBool::new(false),
            cha_leaf_watched_nonempty: AtomicBool::new(false),
            cha_leaf_watched: parking_lot::Mutex::new(rustc_hash::FxHashSet::default()),
            cha: parking_lot::Mutex::new(ChaState::default()),
            minted_slots: parking_lot::Mutex::new(Vec::new()),
            receivers: parking_lot::Mutex::new(ReceiverTracker::default()),
            refresh_lock: parking_lot::Mutex::new(()),
            receiver_sample: AtomicU32::new(0),
            clock_start: Instant::now(),
            last_want_epoch: AtomicU64::new(0),
            last_refresh_end_us: AtomicU64::new(0),
            last_refresh_us: AtomicU64::new(0),
            snapshots: AtomicU64::new(0),
            last_fact_objects: AtomicU64::new(0),
            refolds: AtomicU64::new(0),
            demanded: AtomicU64::new(0),
            reported_exit: AtomicBool::new(false),
        }
    }
}

pub fn enabled() -> bool {
    !cratonvm_types::image_layout::capturing()
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_FROZEN_PAGES")
}

/// Mark snapshot dirty when a class finishes `<clinit>` with `static final`
/// reference candidates (`note_class_initialized_final_ref_classes`).
pub fn freeze_graphs(vm: &SharedVm, roots: &[ObjectRef]) {
    if !enabled() || roots.is_empty() {
        return;
    }
    vm.classes.frozen.dirty.store(true, Ordering::Release);
}

fn resolve_declaring_field_owner(
    store: &ClassStore,
    start_name: &str,
    field_name: &str,
    field_desc: &str,
) -> Option<Arc<str>> {
    // Prefer exact hierarchy walk from any loaded class named `start_name`.
    let mut found_any_start = false;
    for cid_raw in 0..store.next_id().as_u32() {
        let cid = ClassId::new(cid_raw);
        let Some(start_cls) = store.get(cid) else {
            continue;
        };
        if &*start_cls.name != start_name {
            continue;
        }
        found_any_start = true;
        let mut cur = Some(cid);
        while let Some(c_id) = cur {
            let Some(c) = store.get(c_id) else {
                break;
            };
            if c.fields
                .iter()
                .any(|f| !f.is_static() && &*f.name == field_name && &*f.descriptor == field_desc)
            {
                return Some(Arc::clone(&c.name));
            }
            cur = c.superclass;
        }
    }
    if found_any_start {
        Some(Arc::from(start_name))
    } else {
        None
    }
}

fn descriptor_call_effect(desc: &str, is_static: bool) -> Option<(usize, usize)> {
    let bytes = desc.as_bytes();
    if bytes.first().copied() != Some(b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut args = usize::from(!is_static);
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                args += 1;
                i += 1;
            }
            b'L' => {
                args += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i = i.checked_add(1)?;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                }
                args += 1;
                i = i.checked_add(1)?;
            }
            _ => return None,
        }
    }
    let ret = *bytes.get(i + 1)?;
    Some((args, usize::from(ret != b'V')))
}

#[derive(Clone)]
struct PutfieldFinding {
    owner_name: Arc<str>,
    field_name: Arc<str>,
    field_desc: Arc<str>,
    resolved_declarer: Option<Arc<str>>,
}

/// Extract all `putfield` mutations in `cls` that are NOT proven to initialize
/// `this` inside `cls.<init>`.
fn extract_non_init_putfields(store: &ClassStore, cls: &Class) -> Vec<PutfieldFinding> {
    let mut out = Vec::new();
    for m in &cls.methods {
        let Some(code_attr) = m.code() else {
            continue;
        };
        let code: &[u8] = &code_attr.code;
        if code.is_empty() {
            continue;
        }
        let is_ctor = &*m.name == "<init>" && !m.is_static();
        let ctor_keeps_this = is_ctor
            && cratonvm_jit::ir::body_writes_local(code, code.len(), 0) == Some(false);
        let handler_pcs: Vec<usize> = if is_ctor {
            code_attr
                .exception_table
                .iter()
                .map(|e| e.handler_pc as usize)
                .collect()
        } else {
            Vec::new()
        };
        let call_effect = |call_pc: usize| -> Option<(usize, usize)> {
            let op = *code.get(call_pc)?;
            if !matches!(op, 0xb6 | 0xb7 | 0xb8 | 0xb9) || call_pc + 2 >= code.len() {
                return None;
            }
            let cp = u16::from_be_bytes([code[call_pc + 1], code[call_pc + 2]]);
            let nat = match cls.constant_pool.get(cp)? {
                ConstantPoolEntry::MethodReference {
                    name_and_type_index,
                    ..
                }
                | ConstantPoolEntry::InterfaceMethodReference {
                    name_and_type_index,
                    ..
                } => *name_and_type_index,
                _ => return None,
            };
            let (_, desc) = cls.constant_pool.get_name_and_type(nat)?;
            descriptor_call_effect(desc, op == 0xb8)
        };

        let mut pc = 0usize;
        while pc < code.len() {
            let op = code[pc];
            let Some(len) = cratonvm_jit::bytecode_analysis::insn_len(code, pc) else {
                break;
            };
            if op == 0xb5 && pc + 2 < code.len() {
                let cp_idx = u16::from_be_bytes([code[pc + 1], code[pc + 2]]);
                if let Some(ConstantPoolEntry::FieldReference {
                    class_index,
                    name_and_type_index,
                }) = cls.constant_pool.get(cp_idx)
                {
                    if let (Some(owner), Some((fname, fdesc))) = (
                        cls.constant_pool.get_class_name(*class_index),
                        cls.constant_pool.get_name_and_type(*name_and_type_index),
                    ) {
                        let resolved = resolve_declaring_field_owner(store, owner, fname, fdesc);
                        let is_own_field = resolved
                            .as_deref()
                            .is_some_and(|decl| decl == &*cls.name);
                        let is_verified_final_ctor_init = is_ctor
                            && is_own_field
                            && cls.version.major >= 53
                            && cls.fields.iter().any(|f| {
                                !f.is_static()
                                    && f.is_final()
                                    && &*f.name == fname
                                    && &*f.descriptor == fdesc
                            });
                        let is_proven_this_ctor_init = is_ctor
                            && is_own_field
                            && ctor_keeps_this
                            && cratonvm_jit::ir::splice_call_operand_sources_with_calls(
                                code,
                                code.len(),
                                pc,
                                2,
                                &handler_pcs,
                                &call_effect,
                            )
                            .is_some_and(|srcs| {
                                srcs.first()
                                    == Some(&cratonvm_jit::ir::SpliceOperandSource::Local(0))
                            });
                        if !is_verified_final_ctor_init && !is_proven_this_ctor_init {
                            out.push(PutfieldFinding {
                                owner_name: Arc::from(owner),
                                field_name: Arc::from(fname),
                                field_desc: Arc::from(fdesc),
                                resolved_declarer: resolved,
                            });
                        }
                    }
                }
            }
            pc += len;
        }
    }
    out
}

fn apply_cha_findings(
    cha: &mut ChaState,
    findings: Vec<PutfieldFinding>,
    resolved_pending: Vec<(Arc<str>, Arc<str>, Arc<str>)>,
) -> Vec<Arc<str>> {
    let mut to_invalidate: Vec<Arc<str>> = Vec::new();
    for f in findings {
        match f.resolved_declarer {
            Some(decl) => {
                if cha
                    .mutable_fields
                    .insert((Arc::clone(&decl), f.field_name, f.field_desc))
                    && cha.active_dep_owners.contains(&decl)
                    && !to_invalidate.contains(&decl)
                {
                    to_invalidate.push(decl);
                }
            }
            None => {
                let entry = cha.pending_putfields.entry(f.owner_name).or_default();
                let pair = (f.field_name, f.field_desc);
                if !entry.contains(&pair) {
                    entry.push(pair);
                }
            }
        }
    }
    for (decl, fname, fdesc) in resolved_pending {
        if cha
            .mutable_fields
            .insert((Arc::clone(&decl), fname, fdesc))
            && cha.active_dep_owners.contains(&decl)
            && !to_invalidate.contains(&decl)
        {
            to_invalidate.push(decl);
        }
    }
    to_invalidate
}

fn ensure_cha_caught_up(vm: &SharedVm) {
    let start_id = vm.classes.frozen.cha.lock().scanned_up_to;
    let (next_id, findings, resolved_pending) = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let end_id = store.next_id().as_u32();
        if start_id >= end_id {
            vm.classes
                .frozen
                .cha_initialized
                .store(true, Ordering::Release);
            return;
        }
        let pending_keys: FxHashMap<Arc<str>, Vec<(Arc<str>, Arc<str>)>> =
            vm.classes.frozen.cha.lock().pending_putfields.clone();
        let mut findings = Vec::new();
        let mut resolved_pending = Vec::new();
        for raw in start_id..end_id {
            if let Some(cls) = store.get(ClassId::new(raw)) {
                findings.extend(extract_non_init_putfields(store, cls));
                if let Some(waiters) = pending_keys.get(&cls.name) {
                    for (fname, fdesc) in waiters {
                        if let Some(decl) =
                            resolve_declaring_field_owner(store, &cls.name, fname, fdesc)
                        {
                            resolved_pending.push((decl, Arc::clone(fname), Arc::clone(fdesc)));
                        }
                    }
                }
            }
        }
        (end_id, findings, resolved_pending)
    };
    let to_invalidate = {
        let mut cha = vm.classes.frozen.cha.lock();
        if next_id > cha.scanned_up_to {
            cha.scanned_up_to = next_id;
        }
        apply_cha_findings(&mut cha, findings, resolved_pending)
    };
    vm.classes
        .frozen
        .cha_initialized
        .store(true, Ordering::Release);
    retire_cha_owners(vm, &to_invalidate);
}

fn retire_cha_owners(vm: &SharedVm, owners: &[Arc<str>]) {
    if owners.is_empty() {
        return;
    }
    *vm.classes.frozen.facts.write() = None;
    vm.classes.frozen.dirty.store(true, Ordering::Release);
    let jit = vm.jit.jit_cache.write();
    for owner in owners {
        let dep = format!("{CHA_FIELD_DEP_PREFIX}{owner}");
        let _ = jit.invalidate_forcing_exits(&dep);
    }
}

/// Hook called on every class definition (`note_finalizable_define` and hidden
/// class defines) with no class-manager lock held.
#[inline]
pub fn note_class_defined(vm: &SharedVm, class_id: ClassId) {
    if vm
        .classes
        .frozen
        .cha_leaf_watched_nonempty
        .load(Ordering::Acquire)
    {
        note_class_defined_cha_leaf(vm, class_id);
    }
    if !vm.classes.frozen.cha_initialized.load(Ordering::Acquire) {
        return;
    }
    note_class_defined_slow(vm, class_id);
}

#[cold]
fn note_class_defined_cha_leaf(vm: &SharedVm, class_id: ClassId) {
    let mut to_invalidate: Vec<Arc<str>> = Vec::new();
    {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let Some(cls) = store.get(class_id) else {
            return;
        };
        let watched = vm.classes.frozen.cha_leaf_watched.lock();
        let mut cur = cls.superclass;
        while let Some(sid) = cur {
            let Some(sc) = store.get(sid) else {
                break;
            };
            if watched.contains(&sc.name) {
                to_invalidate.push(Arc::clone(&sc.name));
            }
            cur = sc.superclass;
        }
    }
    if !to_invalidate.is_empty() {
        let jit = vm.jit.jit_cache.write();
        for name in to_invalidate {
            let dep = format!("{}{name}", cratonvm_jit::frozen_facts::CHA_LEAF_DEP_PREFIX);
            let _ = jit.invalidate_forcing_exits(&dep);
        }
    }
}

#[cold]
fn note_class_defined_slow(vm: &SharedVm, class_id: ClassId) {
    let pending_for_name = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let Some(cls) = store.get(class_id) else {
            return;
        };
        let waiters = vm
            .classes
            .frozen
            .cha
            .lock()
            .pending_putfields
            .remove(&cls.name);
        let findings = extract_non_init_putfields(store, cls);
        let mut resolved_pending = Vec::new();
        if let Some(waiters) = waiters {
            for (fname, fdesc) in waiters {
                if let Some(decl) = resolve_declaring_field_owner(store, &cls.name, &fname, &fdesc)
                {
                    resolved_pending.push((decl, fname, fdesc));
                }
            }
        }
        (findings, resolved_pending)
    };
    let to_invalidate = {
        let mut cha = vm.classes.frozen.cha.lock();
        let next = class_id.as_u32().saturating_add(1);
        if cha.scanned_up_to + 1 == next {
            cha.scanned_up_to = next;
        }
        apply_cha_findings(&mut cha, pending_for_name.0, pending_for_name.1)
    };
    retire_cha_owners(vm, &to_invalidate);
}

/// Hook called BEFORE reflective, `Unsafe`, `VarHandle`, or JNI instance field
/// mutations. Fast path is a single relaxed atomic byte load when inactive.
#[inline(always)]
pub fn note_slot_write(vm: &SharedVm, obj: ObjectRef, slot: usize, force_all_instances: bool) {
    if !vm.classes.frozen.has_snapshots.load(Ordering::Relaxed) {
        return;
    }
    note_slot_write_slow(vm, obj, slot, force_all_instances);
}

#[cold]
fn note_slot_write_slow(vm: &SharedVm, obj: ObjectRef, slot: usize, force_all_instances: bool) {
    let (obj, ok) = vm.mem.heap.load_and_forward_checked(obj);
    if !ok || vm.mem.heap.kind_of(obj) != ObjectKind::Object {
        return;
    }
    let cid = vm.mem.heap.class_id_of_validated(obj);
    if cid.as_u32() == 0 {
        return;
    }
    let field_meta: Option<(Arc<str>, Arc<str>, Arc<str>, bool)> = {
        let cm = vm.classes.class_manager.read();
        let mut cur = Some(cid);
        let mut found = None;
        while let Some(c_id) = cur {
            let Some(c) = cm.get_class(c_id) else {
                break;
            };
            let base = c.first_field_index as usize;
            if slot >= base {
                if let Some(f) = c.fields.iter().filter(|f| !f.is_static()).nth(slot - base) {
                    found = Some((
                        Arc::clone(&c.name),
                        Arc::clone(&f.name),
                        Arc::clone(&f.descriptor),
                        f.is_final(),
                    ));
                    break;
                }
            }
            cur = c.superclass;
        }
        found
    };
    let Some((decl_name, fname, fdesc, is_final)) = field_meta else {
        return;
    };
    {
        let cha = vm.classes.frozen.cha.lock();
        if cha
            .dynamically_mutated
            .contains(&(Arc::clone(&decl_name), Arc::clone(&fname), Arc::clone(&fdesc)))
        {
            return;
        }
    }
    // Native helper construction of brand-new `final` JDK objects (`Method`,
    // `String`) initializes an un-snapshotted instance; only treat `final`
    // native writes as mutations when `force_all_instances` (`Unsafe` inline
    // cell publication / JNI / `VarHandle`) or `obj` itself was snapshotted.
    if is_final && !force_all_instances && !is_snapshotted_object(vm, obj) {
        return;
    }
    let should_retire = {
        let mut cha = vm.classes.frozen.cha.lock();
        let key = (Arc::clone(&decl_name), fname, fdesc);
        cha.mutable_fields.insert(key.clone());
        cha.dynamically_mutated.insert(key) && cha.active_dep_owners.contains(&decl_name)
    };
    if should_retire {
        retire_cha_owners(vm, &[decl_name]);
    } else {
        *vm.classes.frozen.facts.write() = None;
        vm.classes.frozen.dirty.store(true, Ordering::Release);
    }
}

fn is_snapshotted_object(vm: &SharedVm, obj: ObjectRef) -> bool {
    let want = obj.as_ptr() as usize;
    let slots = vm.classes.frozen.minted_slots.lock().clone();
    if slots.is_empty() {
        return false;
    }
    let _refs = vm.natives.jni_global_refs.lock();
    slots.into_iter().any(|slot| {
        slot != 0 && unsafe { (*(slot as *const ObjectRef)).as_ptr() as usize == want }
    })
}

fn current_slot_map(vm: &SharedVm) -> FxHashMap<usize, usize> {
    let slots = vm.classes.frozen.minted_slots.lock().clone();
    let mut map = FxHashMap::with_capacity_and_hasher(slots.len(), Default::default());
    if slots.is_empty() {
        return map;
    }
    let _refs = vm.natives.jni_global_refs.lock();
    for slot in slots {
        if slot != 0 {
            let cur = unsafe { (*(slot as *const ObjectRef)).as_ptr() as usize };
            if cur != 0 {
                map.insert(cur, slot);
            }
        }
    }
    map
}

fn get_or_mint_slot(
    vm: &SharedVm,
    slot_map: &mut FxHashMap<usize, usize>,
    obj: ObjectRef,
) -> usize {
    let raw = obj.as_ptr() as usize;
    if raw == 0 {
        return 0;
    }
    if let Some(&slot) = slot_map.get(&raw) {
        return slot;
    }
    if vm.classes.frozen.minted_slots.lock().len() >= SLOT_CAP {
        return 0;
    }
    let slot = crate::jit::helpers::mint_jit_const_ref_slot(vm, obj);
    if slot != 0 {
        let mut minted = vm.classes.frozen.minted_slots.lock();
        if !minted.contains(&slot) {
            minted.push(slot);
        }
        slot_map.insert(raw, slot);
    }
    slot
}

/// Sampled interpreter virtual-call receiver observation (`dispatch_virtual`).
pub fn note_receiver(
    vm: &SharedVm,
    site: (u32, &str, &str, usize),
    receiver_class_id: u32,
    receiver_addr: usize,
) {
    if !enabled() || receiver_addr == 0 || receiver_class_id == 0 {
        return;
    }
    let tick = vm
        .classes
        .frozen
        .receiver_sample
        .fetch_add(1, Ordering::Relaxed);
    if (tick & RECEIVER_SAMPLE_MASK) != 0 {
        return;
    }
    let epoch = vm.mem.heap.address_epoch();
    let site_key: ReceiverSite = (site.0, site.1.into(), site.2.into(), site.3);

    let (promote_class, promote_site) = {
        let mut tr = vm.classes.frozen.receivers.lock();
        let mut p_cls = false;
        let mut p_site = false;

        if !tr.poisoned_classes.contains(&receiver_class_id) {
            if let Some(&(slot, calls, others)) = tr.confirmed_classes.get(&receiver_class_id) {
                let same = unsafe { (*(slot as *const ObjectRef)).as_ptr() as usize == receiver_addr };
                let next_calls = calls.saturating_add(1);
                let next_others = others.saturating_add(u32::from(!same));
                if next_others.saturating_mul(32) > next_calls {
                    tr.confirmed_classes.remove(&receiver_class_id);
                    tr.poisoned_classes.insert(receiver_class_id);
                    tr.cached_table = None;
                } else {
                    tr.confirmed_classes
                        .insert(receiver_class_id, (slot, next_calls, next_others));
                }
            } else {
                let row = tr
                    .raw_classes
                    .entry(receiver_class_id)
                    .or_insert((epoch, receiver_addr, 0, 0));
                if row.0 != epoch {
                    *row = (epoch, receiver_addr, 1, 0);
                } else if row.1 == receiver_addr {
                    row.2 = row.2.saturating_add(1);
                } else {
                    row.3 = row.3.saturating_add(1);
                    if row.3 > row.2 {
                        tr.raw_classes.remove(&receiver_class_id);
                        tr.poisoned_classes.insert(receiver_class_id);
                    }
                }
                if let Some(&(_, _, calls, others)) = tr.raw_classes.get(&receiver_class_id) {
                    if calls >= RECEIVER_MIN_SAMPLES
                        && others.saturating_mul(64) <= calls
                        && tr.confirmed_classes.len() + tr.confirmed_sites.len() < DEMAND_CAP
                    {
                        p_cls = true;
                    }
                }
            }
        }

        if !tr.poisoned_sites.contains(&site_key) {
            if let Some(&(slot, calls, others)) = tr.confirmed_sites.get(&site_key) {
                let same = unsafe { (*(slot as *const ObjectRef)).as_ptr() as usize == receiver_addr };
                let next_calls = calls.saturating_add(1);
                let next_others = others.saturating_add(u32::from(!same));
                if next_others.saturating_mul(32) > next_calls {
                    tr.confirmed_sites.remove(&site_key);
                    tr.poisoned_sites.insert(site_key.clone());
                    tr.cached_table = None;
                } else {
                    tr.confirmed_sites
                        .insert(site_key.clone(), (slot, next_calls, next_others));
                }
            } else {
                let row = tr
                    .raw_sites
                    .entry(site_key.clone())
                    .or_insert((epoch, receiver_addr, 0, 0));
                if row.0 != epoch {
                    *row = (epoch, receiver_addr, 1, 0);
                } else if row.1 == receiver_addr {
                    row.2 = row.2.saturating_add(1);
                } else {
                    row.3 = row.3.saturating_add(1);
                    if row.3 > row.2 {
                        tr.raw_sites.remove(&site_key);
                        tr.poisoned_sites.insert(site_key.clone());
                    }
                }
                if let Some(&(_, _, calls, others)) = tr.raw_sites.get(&site_key) {
                    if calls >= RECEIVER_MIN_SAMPLES
                        && others.saturating_mul(64) <= calls
                        && tr.confirmed_classes.len() + tr.confirmed_sites.len() < DEMAND_CAP
                    {
                        p_site = true;
                    }
                }
            }
        }
        (p_cls, p_site)
    };

    if !promote_class && !promote_site {
        return;
    }
    if vm.mem.gc_barrier.stw_requested.load(Ordering::Acquire)
        || vm.mem.heap.concurrent_relocation_active()
        || vm.mem.heap.address_epoch() != epoch
    {
        return;
    }
    // Exclude custom user-loader instances so strong root slots never block
    // `URLClassLoader` / webapp class unloading.
    let user_loader = {
        let cm = vm.classes.class_manager.read();
        cm.get_class(ClassId::new(receiver_class_id))
            .is_none_or(|c| matches!(c.loader_id, ClassLoaderId::UserDefined(_)))
    };
    if user_loader {
        return;
    }
    let obj = unsafe { ObjectRef::from_raw(receiver_addr as *mut u8) };
    let (obj, ok) = vm.mem.heap.load_and_forward_checked(obj);
    if !ok || vm.mem.heap.kind_of(obj) != ObjectKind::Object {
        return;
    }
    let mut slot_map = current_slot_map(vm);
    let slot = get_or_mint_slot(vm, &mut slot_map, obj);
    if slot == 0 {
        return;
    }
    let mut changed = false;
    {
        let mut tr = vm.classes.frozen.receivers.lock();
        if promote_class
            && !tr.poisoned_classes.contains(&receiver_class_id)
            && tr.confirmed_classes.len() + tr.confirmed_sites.len() < DEMAND_CAP
        {
            tr.raw_classes.remove(&receiver_class_id);
            tr.confirmed_classes
                .entry(receiver_class_id)
                .or_insert((slot, RECEIVER_MIN_SAMPLES, 0));
            tr.cached_table = None;
            changed = true;
        }
        if promote_site
            && !tr.poisoned_sites.contains(&site_key)
            && tr.confirmed_classes.len() + tr.confirmed_sites.len() < DEMAND_CAP
        {
            tr.raw_sites.remove(&site_key);
            tr.confirmed_sites
                .entry(site_key)
                .or_insert((slot, RECEIVER_MIN_SAMPLES, 0));
            tr.cached_table = None;
            changed = true;
        }
    }
    if changed {
        vm.classes.frozen.demanded.fetch_add(1, Ordering::Relaxed);
        vm.classes.frozen.dirty.store(true, Ordering::Release);
    }
}

/// Retire disproven receiver-identity speculations when a compiled guard traps,
/// preventing repeated 4xN per-field deopt cycles on polymorphic classes/sites.
#[inline]
pub fn note_method_trapped(vm: &SharedVm, method_key: &str, bci: u32) {
    if !vm.classes.frozen.has_snapshots.load(Ordering::Relaxed) {
        return;
    }
    note_method_trapped_slow(vm, method_key, bci);
}

#[cold]
fn note_method_trapped_slow(vm: &SharedVm, method_key: &str, bci: u32) {
    let Some((cls_name, rest)) = method_key.split_once('.') else {
        return;
    };
    let Some((m_name, m_desc)) = rest.split_once(':') else {
        return;
    };
    let class_ids: Vec<u32> = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        (0..store.next_id().as_u32())
            .filter(|&raw| {
                store
                    .get(ClassId::new(raw))
                    .is_some_and(|c| &*c.name == cls_name)
            })
            .collect()
    };
    if class_ids.is_empty() {
        return;
    }
    let mut tr = vm.classes.frozen.receivers.lock();
    let mut changed = false;
    for cid in class_ids {
        if tr.confirmed_classes.remove(&cid).is_some() {
            tr.poisoned_classes.insert(cid);
            changed = true;
        }
        let site: ReceiverSite = (cid, m_name.into(), m_desc.into(), bci as usize);
        if tr.confirmed_sites.remove(&site).is_some() {
            tr.poisoned_sites.insert(site);
            changed = true;
        }
    }
    if changed {
        tr.cached_table = None;
    }
}

pub fn dominant_receiver(vm: &SharedVm, class_id: u32) -> Option<usize> {
    if !enabled() {
        return None;
    }
    vm.classes
        .frozen
        .receivers
        .lock()
        .confirmed_classes
        .get(&class_id)
        .map(|&(slot, _, _)| slot)
}

pub fn site_receiver(
    vm: &SharedVm,
    class_id: u32,
    method: &str,
    descriptor: &str,
    bci: usize,
) -> Option<usize> {
    if !enabled() {
        return None;
    }
    let key: ReceiverSite = (class_id, method.into(), descriptor.into(), bci);
    vm.classes
        .frozen
        .receivers
        .lock()
        .confirmed_sites
        .get(&key)
        .map(|&(slot, _, _)| slot)
}

pub fn dominant_receivers(vm: &SharedVm) -> Option<Arc<HashMap<String, usize>>> {
    if !enabled() {
        return None;
    }
    if let Some(cached) = vm.classes.frozen.receivers.lock().cached_table.clone() {
        return Some(cached);
    }
    let (classes, sites): (Vec<(u32, usize)>, Vec<(ReceiverSite, usize)>) = {
        let tr = vm.classes.frozen.receivers.lock();
        (
            tr.confirmed_classes
                .iter()
                .map(|(&cid, &(slot, _, _))| (cid, slot))
                .collect(),
            tr.confirmed_sites
                .iter()
                .map(|(k, &(slot, _, _))| (k.clone(), slot))
                .collect(),
        )
    };
    let mut out = HashMap::with_capacity(classes.len() + sites.len());
    {
        let cm = vm.classes.class_manager.read();
        for (cid, slot) in classes {
            if let Some(cls) = cm.get_class(ClassId::new(cid)) {
                out.insert(cls.name.to_string(), slot);
            }
        }
        for ((cid, m, d, bci), slot) in sites {
            if let Some(cls) = cm.get_class(ClassId::new(cid)) {
                out.insert(format!("{}.{m}:{d}@{bci}", cls.name), slot);
            }
        }
    }
    let arc = Arc::new(out);
    vm.classes.frozen.receivers.lock().cached_table = Some(Arc::clone(&arc));
    Some(arc)
}

pub fn unregister_vm(vm: &SharedVm) {
    report_vm_at_exit(vm);
}

pub fn invalidate_class(vm: &SharedVm, class_id: u32) {
    let mut set = FxHashSet::default();
    set.insert(class_id);
    invalidate_classes(vm, &set);
}

pub fn invalidate_classes(vm: &SharedVm, class_ids: &FxHashSet<u32>) {
    if class_ids.is_empty() {
        return;
    }
    vm.classes
        .static_final_ref_classes
        .write()
        .retain(|&(cid, _), _| !class_ids.contains(&cid));
    vm.classes
        .frozen
        .facts_state
        .forget_classes(|cid| class_ids.contains(&cid));
    {
        let mut tr = vm.classes.frozen.receivers.lock();
        tr.raw_classes.retain(|cid, _| !class_ids.contains(cid));
        tr.confirmed_classes
            .retain(|cid, _| !class_ids.contains(cid));
        tr.raw_sites
            .retain(|(cid, _, _, _), _| !class_ids.contains(cid));
        tr.confirmed_sites
            .retain(|(cid, _, _, _), _| !class_ids.contains(cid));
        tr.cached_table = None;
    }
    if !vm.classes.frozen.cha_initialized.load(Ordering::Acquire) {
        if enabled() {
            vm.classes.frozen.dirty.store(true, Ordering::Release);
        }
        return;
    }
    let (owners_to_retire, findings): (Vec<Arc<str>>, Vec<PutfieldFinding>) = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let mut owners = Vec::new();
        let mut findings = Vec::new();
        for &raw in class_ids {
            if let Some(cls) = store.get(ClassId::new(raw)) {
                owners.push(Arc::clone(&cls.name));
                findings.extend(extract_non_init_putfields(store, cls));
            }
        }
        (owners, findings)
    };
    let mut extra = {
        let mut cha = vm.classes.frozen.cha.lock();
        apply_cha_findings(&mut cha, findings, Vec::new())
    };
    for o in owners_to_retire {
        if !extra.contains(&o) {
            extra.push(o);
        }
    }
    retire_cha_owners(vm, &extra);
}

pub fn refresh_facts_if_due(vm: &SharedVm) {
    if !enabled() {
        return;
    }
    let dirty = vm.classes.frozen.dirty.load(Ordering::Acquire);
    let want_epoch = vm.classes.frozen.facts_state.want_epoch();
    let want_dirty = vm.classes.frozen.facts_state.any_wanted()
        && vm.classes.frozen.last_want_epoch.load(Ordering::Relaxed) != want_epoch;
    if (!dirty && !want_dirty)
        || vm.mem.gc_barrier.stw_requested.load(Ordering::Acquire)
        || vm.mem.heap.concurrent_relocation_active()
    {
        return;
    }
    let started = vm.classes.frozen.clock_start.elapsed().as_micros() as u64;
    let wait = (vm.classes.frozen.last_refresh_us.load(Ordering::Relaxed) * REFRESH_BACKOFF)
        .clamp(2_000, 100_000);
    let last_end = vm
        .classes
        .frozen
        .last_refresh_end_us
        .load(Ordering::Relaxed);
    if last_end != 0 && started.saturating_sub(last_end) < wait {
        return;
    }
    let Some(_guard) = vm.classes.frozen.refresh_lock.try_lock() else {
        return;
    };
    let published = if dirty {
        vm.classes.frozen.dirty.store(false, Ordering::Relaxed);
        match build_snapshot(vm) {
            Some(facts) => {
                *vm.classes.frozen.facts.write() = Some(facts);
                vm.classes
                    .frozen
                    .has_snapshots
                    .store(true, Ordering::Release);
                true
            }
            None => {
                vm.classes.frozen.dirty.store(true, Ordering::Release);
                false
            }
        }
    } else {
        false
    };

    if let Some(facts) = vm.classes.frozen.facts.read().clone() {
        let methods = vm
            .classes
            .frozen
            .facts_state
            .take_wanting(published, |miss| match *miss {
                FrozenMiss::Object(addr) => facts.object_id(addr).is_some(),
                FrozenMiss::Root(cls, idx) => facts.roots.contains_key(&(cls, idx)),
            });
        if !methods.is_empty() {
            let evicted = vm.jit.jit_cache.invalidate_for_frozen_facts(&methods);
            vm.classes
                .frozen
                .refolds
                .fetch_add(methods.len() as u64, Ordering::Relaxed);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[frozen-pages] {} method(s) to refold, {evicted} bodies withdrawn",
                    methods.len()
                );
            }
        }
    }

    vm.classes
        .frozen
        .last_want_epoch
        .store(want_epoch, Ordering::Release);
    let ended = (vm.classes.frozen.clock_start.elapsed().as_micros() as u64).max(1);
    vm.classes
        .frozen
        .last_refresh_us
        .store(ended.saturating_sub(started), Ordering::Relaxed);
    vm.classes
        .frozen
        .last_refresh_end_us
        .store(ended, Ordering::Relaxed);
}

#[derive(Clone)]
struct InstanceFieldPlan {
    slot: usize,
    tag: u8,
    declaring_owner: Arc<str>,
    cha_final: bool,
    is_stable: bool,
    is_1d_array: bool,
}

fn is_1d_array_descriptor(desc: &str) -> bool {
    let b = desc.as_bytes();
    b.first().copied() == Some(b'[') && b.get(1).copied() != Some(b'[')
}

fn build_snapshot(vm: &SharedVm) -> Option<Arc<FrozenFacts>> {
    if vm.mem.gc_barrier.stw_requested.load(Ordering::Acquire)
        || vm.mem.heap.concurrent_relocation_active()
    {
        return None;
    }
    ensure_cha_caught_up(vm);

    let root_keys: Vec<(u32, u32, Arc<str>)> = {
        let cm = vm.classes.class_manager.read();
        vm.classes
            .static_final_ref_classes
            .read()
            .iter()
            .filter_map(|(&(cid, idx), &val_cid)| {
                if val_cid == 0 {
                    return None;
                }
                let cls = cm.get_class(ClassId::new(cid))?;
                if matches!(cls.loader_id, ClassLoaderId::UserDefined(_)) {
                    return None;
                }
                Some((cid, idx, Arc::clone(&cls.name)))
            })
            .collect()
    };

    let demand_slots: Vec<usize> = {
        let tr = vm.classes.frozen.receivers.lock();
        let mut s: Vec<usize> = tr
            .confirmed_classes
            .values()
            .chain(tr.confirmed_sites.values())
            .map(|&(slot, _, _)| slot)
            .collect();
        s.sort_unstable();
        s.dedup();
        s
    };

    let mut slot_map = current_slot_map(vm);
    let mut facts = FrozenFacts::default();
    let mut visited_by_slot: FxHashMap<usize, u32> = FxHashMap::default();
    let mut class_plans: FxHashMap<u32, (usize, Vec<InstanceFieldPlan>)> = FxHashMap::default();
    let mut recorded_owners: FxHashSet<Arc<str>> = FxHashSet::default();
    let mut snapshotted_cids: Vec<u32> = Vec::new();

    struct WorkItem {
        obj: ObjectRef,
        slot_addr: usize,
        id: u32,
        depth: u8,
        stable_array_owner: Option<Arc<str>>,
    }
    let mut queue: std::collections::VecDeque<WorkItem> = std::collections::VecDeque::new();

    let enqueue_obj = |vm: &SharedVm,
                       slot_map: &mut FxHashMap<usize, usize>,
                       facts: &mut FrozenFacts,
                       visited_by_slot: &mut FxHashMap<usize, u32>,
                       queue: &mut std::collections::VecDeque<WorkItem>,
                       raw_obj: ObjectRef,
                       depth: u8,
                       stable_array_owner: Option<Arc<str>>|
     -> Option<u32> {
        let (obj, ok) = vm.mem.heap.load_and_forward_checked(raw_obj);
        if !ok {
            return None;
        }
        let kind = vm.mem.heap.kind_of(obj);
        if !matches!(kind, ObjectKind::Object | ObjectKind::Array) {
            return None;
        }
        let cid = if kind == ObjectKind::Object {
            let c = vm.mem.heap.class_id_of_validated(obj).as_u32();
            if c == 0 {
                return None;
            }
            c
        } else {
            0
        };
        let slot_addr = get_or_mint_slot(vm, slot_map, obj);
        if slot_addr == 0 {
            return None;
        }
        if let Some(&existing_id) = visited_by_slot.get(&slot_addr) {
            return Some(existing_id);
        }
        if facts.objects.len() >= OBJECT_CAP {
            return None;
        }
        let id = u32::try_from(facts.objects.len()).ok()?;
        facts.objects.push(FrozenObject {
            addr: slot_addr,
            slot_addr,
            class_id: cid,
            ..Default::default()
        });
        visited_by_slot.insert(slot_addr, id);
        queue.push_back(WorkItem {
            obj,
            slot_addr,
            id,
            depth,
            stable_array_owner,
        });
        Some(id)
    };

    for (holder_cid, field_idx, holder_name) in root_keys {
        if let Value::Object(Some(obj)) =
            crate::vm::get_static_shared(vm, ClassId::new(holder_cid), field_idx as usize)
        {
            if let Some(id) = enqueue_obj(
                vm,
                &mut slot_map,
                &mut facts,
                &mut visited_by_slot,
                &mut queue,
                obj,
                0,
                None,
            ) {
                facts.roots.insert((holder_cid, field_idx), id);
                facts
                    .root_owners
                    .insert((holder_cid, field_idx), Arc::clone(&holder_name));
                recorded_owners.insert(holder_name);
            }
        }
    }

    for slot in demand_slots {
        let obj_opt = {
            let _refs = vm.natives.jni_global_refs.lock();
            (slot != 0).then(|| unsafe { *(slot as *const ObjectRef) })
        };
        if let Some(obj) = obj_opt.filter(|o| !o.as_ptr().is_null()) {
            let _ = enqueue_obj(
                vm,
                &mut slot_map,
                &mut facts,
                &mut visited_by_slot,
                &mut queue,
                obj,
                0,
                None,
            );
        }
    }

    while let Some(item) = queue.pop_front() {
        if vm.mem.gc_barrier.stw_requested.load(Ordering::Acquire)
            || vm.mem.heap.concurrent_relocation_active()
        {
            return None;
        }
        let (obj, ok) = vm.mem.heap.load_and_forward_checked(item.obj);
        if !ok {
            continue;
        }
        let _ = item.slot_addr;
        let kind = vm.mem.heap.kind_of(obj);
        if kind == ObjectKind::Array {
            let len = vm.mem.heap.array_length(obj);
            let mut elems = Vec::new();
            if let Some(ref owner) = item.stable_array_owner {
                if len <= ARRAY_ELEM_CAP && item.depth < MAX_DEPTH {
                    elems.resize(len, None);
                    let mut any_elem = false;
                    for (idx, dst) in elems.iter_mut().enumerate() {
                        let Ok(val) = vm.mem.heap.get_array_element_weak(obj, idx) else {
                            continue;
                        };
                        let fv = match val {
                            Value::Int(v) if v != 0 => Some(FrozenValue::Int(v)),
                            Value::Long(v) if v != 0 => Some(FrozenValue::Long(v)),
                            Value::Float(v) if v.to_bits() != 0 => {
                                Some(FrozenValue::FloatBits(v.to_bits()))
                            }
                            Value::Double(v) if v.to_bits() != 0 => {
                                Some(FrozenValue::DoubleBits(v.to_bits()))
                            }
                            Value::Object(Some(child)) => enqueue_obj(
                                vm,
                                &mut slot_map,
                                &mut facts,
                                &mut visited_by_slot,
                                &mut queue,
                                child,
                                item.depth + 1,
                                None,
                            )
                            .map(FrozenValue::Ref),
                            _ => None,
                        };
                        if fv.is_some() {
                            any_elem = true;
                            *dst = fv;
                        }
                    }
                    if any_elem {
                        recorded_owners.insert(Arc::clone(owner));
                    } else {
                        elems.clear();
                    }
                }
            }
            let entry = &mut facts.objects[item.id as usize];
            entry.array_len = u32::try_from(len).ok();
            entry.elem_cha_owner = (!elems.is_empty()).then_some(item.stable_array_owner).flatten();
            entry.elements = elems;
            continue;
        }

        let cid = facts.objects[item.id as usize].class_id;
        if cid == 0 {
            continue;
        }
        if !snapshotted_cids.contains(&cid) {
            snapshotted_cids.push(cid);
        }
        let (num_fields, plans) = class_plans
            .entry(cid)
            .or_insert_with(|| compute_class_field_plans(vm, ClassId::new(cid)))
            .clone();
        if plans.is_empty() || num_fields == 0 {
            continue;
        }

        let mut fields = vec![None; num_fields];
        let mut owners = vec![None; num_fields];
        for plan in &plans {
            if plan.slot >= num_fields {
                continue;
            }
            let read_tag = if plan.tag == b'[' { b'L' } else { plan.tag };
            let val = vm.mem.heap.get_field_as(obj, plan.slot, read_tag);
            let folded = match val {
                Value::Int(v)
                    if matches!(plan.tag, b'I' | b'Z' | b'B' | b'C' | b'S')
                        && (plan.cha_final || (plan.is_stable && v != 0)) =>
                {
                    Some(FrozenValue::Int(v))
                }
                Value::Long(v)
                    if plan.tag == b'J' && (plan.cha_final || (plan.is_stable && v != 0)) =>
                {
                    Some(FrozenValue::Long(v))
                }
                Value::Float(v)
                    if plan.tag == b'F'
                        && (plan.cha_final || (plan.is_stable && v.to_bits() != 0)) =>
                {
                    Some(FrozenValue::FloatBits(v.to_bits()))
                }
                Value::Double(v)
                    if plan.tag == b'D'
                        && (plan.cha_final || (plan.is_stable && v.to_bits() != 0)) =>
                {
                    Some(FrozenValue::DoubleBits(v.to_bits()))
                }
                Value::Object(Some(child))
                    if matches!(plan.tag, b'L' | b'[') && item.depth < MAX_DEPTH =>
                {
                    let stable_arr = (plan.is_stable && plan.is_1d_array)
                        .then(|| Arc::clone(&plan.declaring_owner));
                    enqueue_obj(
                        vm,
                        &mut slot_map,
                        &mut facts,
                        &mut visited_by_slot,
                        &mut queue,
                        child,
                        item.depth + 1,
                        stable_arr,
                    )
                    .map(FrozenValue::Ref)
                }
                _ => None,
            };
            if folded.is_some() {
                fields[plan.slot] = folded;
                owners[plan.slot] = Some(Arc::clone(&plan.declaring_owner));
                recorded_owners.insert(Arc::clone(&plan.declaring_owner));
            }
        }
        let entry = &mut facts.objects[item.id as usize];
        entry.fields = fields;
        entry.field_cha_owners = owners;
    }

    {
        let mut cha = vm.classes.frozen.cha.lock();
        cha.active_dep_owners.extend(recorded_owners);
    }
    vm.classes
        .frozen
        .facts_state
        .record_snapshotted_classes(snapshotted_cids);
    let live_count = (0..facts.objects.len() as u32)
        .filter(|&id| facts.live(id).is_some())
        .count() as u64;
    vm.classes.frozen.snapshots.fetch_add(1, Ordering::Relaxed);
    vm.classes
        .frozen
        .last_fact_objects
        .store(live_count, Ordering::Relaxed);
    Some(Arc::new(facts))
}

fn compute_class_field_plans(vm: &SharedVm, exact_cid: ClassId) -> (usize, Vec<InstanceFieldPlan>) {
    let raw_fields: Vec<(usize, u8, Arc<str>, Arc<str>, Arc<str>, bool)> = {
        let cm = vm.classes.class_manager.read();
        let Some(top) = cm.get_class(exact_cid) else {
            return (0, Vec::new());
        };
        if top.name.starts_with('[') || matches!(top.loader_id, ClassLoaderId::UserDefined(_)) {
            return (0, Vec::new());
        }
        let num_total = top.num_total_fields;
        let mut list = Vec::new();
        let mut cur = Some(exact_cid);
        while let Some(cid) = cur {
            let Some(c) = cm.get_class(cid) else {
                break;
            };
            for (inst_idx, f) in c.fields.iter().filter(|f| !f.is_static()).enumerate() {
                if f.is_volatile() {
                    continue;
                }
                let is_stable = cratonvm_classloading::annotations::field_annotations(f)
                    .find_by_type_descriptor("Ljdk/internal/vm/annotation/Stable;", |i| {
                        c.constant_pool.get_utf8(i)
                    })
                    .is_some();
                let tag = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                list.push((
                    c.first_field_index as usize + inst_idx,
                    tag,
                    Arc::clone(&c.name),
                    Arc::clone(&f.name),
                    Arc::clone(&f.descriptor),
                    is_stable,
                ));
            }
            cur = c.superclass;
        }
        let cha = vm.classes.frozen.cha.lock();
        let plans = list
            .into_iter()
            .filter_map(|(slot, tag, decl, fname, fdesc, is_stable)| {
                let key = (Arc::clone(&decl), Arc::clone(&fname), Arc::clone(&fdesc));
                if cha.dynamically_mutated.contains(&key) {
                    return None;
                }
                let cha_final = !cha.mutable_fields.contains(&key);
                if !cha_final && !is_stable {
                    return None;
                }
                Some(InstanceFieldPlan {
                    slot,
                    tag,
                    declaring_owner: decl,
                    cha_final,
                    is_stable,
                    is_1d_array: is_1d_array_descriptor(&fdesc),
                })
            })
            .collect();
        return (num_total, plans);
    };
    #[allow(unreachable_code)]
    (0, raw_fields.into_iter().map(|_| unreachable!()).collect())
}

fn report_vm_at_exit(vm: &SharedVm) {
    if !enabled() || !cratonvm_types::exit_report::exit_report_on() {
        return;
    }
    let snaps = vm.classes.frozen.snapshots.load(Ordering::Relaxed);
    if snaps == 0
        || vm
            .classes
            .frozen
            .reported_exit
            .swap(true, Ordering::Relaxed)
    {
        return;
    }
    cratonvm_types::exit_report!(
        "[frozen-pages] fact snapshots={snaps} objects with facts at the last one={} methods sent back to refold={}",
        vm.classes.frozen.last_fact_objects.load(Ordering::Relaxed),
        vm.classes.frozen.refolds.load(Ordering::Relaxed),
    );
    cratonvm_types::exit_report!(
        "[frozen-pages] receiver-identity guards planted={} receivers specialized on demand={}",
        vm.classes
            .frozen
            .facts_state
            .identity_guards
            .load(Ordering::Relaxed),
        vm.classes.frozen.demanded.load(Ordering::Relaxed),
    );
}

pub fn report_at_exit() {
    if !enabled() || !cratonvm_types::exit_report::exit_report_on() {
        return;
    }
    for vm in crate::vm::vm_init::live_hook_vms_for_jit() {
        report_vm_at_exit(&vm);
    }
}

#[cfg(test)]
mod tests;
