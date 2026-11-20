// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Zero-fault heap specialization facts (`docs/feature-designs/jit-ir-optimizations.md`):
//! CHA-backed effectively-final instance fields, non-default `@Stable` fields
//! and 1D arrays, and lambda capture chains.
//!
//! The VM builds one [`FrozenFacts`] snapshot on a mutator thread
//! (`vm/src/runtime/frozen_pages.rs`) and hands it to a compile through
//! `CompileRequest::frozen_facts`; the compiler never reads the heap directly.
//! Constant object references and singleton identity guards materialize through
//! GC-relocated JNI global root slots (`Op::IndyRootRef`), and CHA class-load
//! or reflective/JNI/`Unsafe` mutations retire dependent compiled bodies via
//! `JitCache::invalidate_forcing_exits`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Synthetic `CompiledMethod::inlined_methods` class-name prefix used to tie
/// compiled CHA/snapshot field folds to `JitCache::invalidate_forcing_exits`.
pub const CHA_FIELD_DEP_PREFIX: &str = "cratonvm/cha_field/";

/// Synthetic `CompiledMethod::inlined_methods` class-name prefix used to tie
/// CHA leaf-class exact typechecks to `JitCache::invalidate_forcing_exits`.
pub const CHA_LEAF_DEP_PREFIX: &str = "cratonvm/cha_leaf/";

/// The snapshot value of one field or `@Stable` array element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrozenValue {
    /// An `int`-category field (`I`, `Z`, `B`, `C`, `S`).
    Int(i32),
    /// A `long` field (`J`).
    Long(i64),
    /// A `float` field (`F`), stored as raw IEEE-754 bits.
    FloatBits(u32),
    /// A `double` field (`D`), stored as raw IEEE-754 bits.
    DoubleBits(u64),
    /// A non-null reference to another object of the same snapshot.
    Ref(u32),
}

/// A fold a build wanted and could not make yet, by what would have to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrozenMiss {
    /// The trusted `static final` `(class id, static field index)` is not in
    /// the snapshot the build had.
    Root(u32, u32),
    /// The object keyed at `addr` has no live facts in the snapshot yet.
    Object(usize),
}

#[derive(Debug, Default)]
pub struct FrozenObject {
    /// Stable snapshot identity key (`slot_addr` in production, or test token).
    /// Never dereferenced as a raw heap pointer by compiled code.
    pub addr: usize,
    /// Untagged C-heap `Box<ObjectRef>` root slot (`mint_jit_const_ref_slot`),
    /// rewritten in place by every moving collector. `0` when unminted.
    pub slot_addr: usize,
    /// The object's exact class; `0` for an array.
    pub class_id: u32,
    /// By instance field index; `None` where the field is mutable, `volatile`,
    /// `null`, or default-valued `@Stable`.
    pub fields: Vec<Option<FrozenValue>>,
    /// Parallel to `fields` (may be shorter): declaring class internal name
    /// whose CHA/mutation invalidation retires folds of that slot.
    pub field_cha_owners: Vec<Option<Arc<str>>>,
    /// Java `arraylength` when the object is an array (immutable forever).
    pub array_len: Option<u32>,
    /// Non-default elements of bounded `@Stable` 1D arrays; empty otherwise.
    pub elements: Vec<Option<FrozenValue>>,
    /// Declaring class internal name of the `@Stable` field that admitted
    /// `elements`, so dynamic mutation of that holder retires element folds.
    pub elem_cha_owner: Option<Arc<str>>,
}

/// One snapshot. Object ids index [`Self::objects`].
#[derive(Debug, Default)]
pub struct FrozenFacts {
    /// Trusted `static final` reference `(class id, static field index)` ->
    /// object index in [`Self::objects`].
    pub roots: HashMap<(u32, u32), u32>,
    /// Declaring holder class internal name for each `(class id, static field index)`
    /// in [`Self::roots`], so reflective/JNI/`Unsafe` static root writes retire folds.
    pub root_owners: HashMap<(u32, u32), Arc<str>>,
    pub objects: Vec<FrozenObject>,
    pub(crate) addr_index: std::sync::OnceLock<HashMap<usize, u32>>,
}

/// `(class, method, descriptor)`.
pub type MethodName = (Arc<str>, Arc<str>, Arc<str>);

/// Evict an unsatisfiable `wanted` row after this many snapshot-advancing
/// [`FrozenFactsState::take_wanting`] sweeps without a hit so `any_wanted`
/// never stays `true` forever.
const MAX_WANT_SWEEPS: u8 = 4;

/// Minimum JIT compiles after a class is first snapshotted before treating it
/// as unconsumed (gives warm-up methods time to compile and fold its fields).
pub const CONSUMER_GRACE_COMPILES: u64 = 16;

#[derive(Default)]
struct ConsumerState {
    compile_seq: u64,
    consumer_classes: HashSet<u32>,
    failed_exact_classes: HashSet<u32>,
    snapshotted_at_compile: HashMap<u32, u64>,
}

/// Per-VM mutable state for heap-specialization refold requests and counters.
/// Owned by `ClassRealm` so VMs are isolated and teardown drops its rows.
#[derive(Default)]
pub struct FrozenFactsState {
    wanted: Mutex<HashMap<MethodName, (Vec<FrozenMiss>, u8)>>,
    any_wanted: AtomicBool,
    want_epoch: AtomicU64,
    consumer_state: Mutex<ConsumerState>,
    /// Receiver-identity guards planted in installed bodies of this VM.
    pub identity_guards: AtomicU64,
}

/// Whether `method_key` (`"class.name:descriptor"`) names `method`.
pub fn method_matches_key(method: &MethodName, method_key: &str) -> bool {
    method_key
        .strip_prefix(&*method.0)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_prefix(&*method.1))
        .and_then(|rest| rest.strip_prefix(':'))
        == Some(&*method.2)
}

impl FrozenFactsState {
    /// Replace `method`'s row with the misses of the build that just installed
    /// (none removes it).
    pub fn note_misses(&self, method: MethodName, misses: &[FrozenMiss]) {
        let mut rows = self.wanted.lock().unwrap_or_else(|p| p.into_inner());
        if misses.is_empty() {
            rows.remove(&method);
        } else {
            rows.insert(method, (misses.to_vec(), 0));
            self.want_epoch.fetch_add(1, Ordering::Relaxed);
        }
        self.any_wanted.store(!rows.is_empty(), Ordering::Relaxed);
    }

    /// Add `miss` to `method`'s row so the VM recompiles it once `miss` lands.
    pub fn want(&self, method: MethodName, miss: FrozenMiss) {
        let mut rows = self.wanted.lock().unwrap_or_else(|p| p.into_inner());
        let (misses, sweeps) = rows.entry(method).or_insert_with(|| (Vec::new(), 0));
        if !misses.contains(&miss) {
            misses.push(miss);
        }
        *sweeps = 0;
        self.want_epoch.fetch_add(1, Ordering::Relaxed);
        self.any_wanted.store(true, Ordering::Relaxed);
    }

    pub fn any_wanted(&self) -> bool {
        self.any_wanted.load(Ordering::Relaxed)
    }

    pub fn want_epoch(&self) -> u64 {
        self.want_epoch.load(Ordering::Relaxed)
    }

    /// Remove and return the methods with a miss `available` now answers for.
    pub fn take_wanting(
        &self,
        advance_sweeps: bool,
        available: impl Fn(&FrozenMiss) -> bool,
    ) -> Vec<MethodName> {
        let mut rows = self.wanted.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        rows.retain(|method, (misses, sweeps)| {
            if misses.iter().any(&available) {
                out.push(method.clone());
                return false;
            }
            if !advance_sweeps {
                return true;
            }
            *sweeps = sweeps.saturating_add(1);
            *sweeps < MAX_WANT_SWEEPS
        });
        self.any_wanted.store(!rows.is_empty(), Ordering::Relaxed);
        out
    }

    /// Record consumer outcomes from a finished, installed IR compile.
    pub fn record_compile(&self, folded_classes: &[u32], failed_classes: &[u32]) {
        let mut state = self.consumer_state.lock().unwrap_or_else(|p| p.into_inner());
        state.compile_seq = state.compile_seq.saturating_add(1);
        for &cid in folded_classes {
            if cid != 0 {
                state.consumer_classes.insert(cid);
                state.failed_exact_classes.remove(&cid);
            }
        }
        for &cid in failed_classes {
            if cid != 0 && !state.consumer_classes.contains(&cid) {
                state.failed_exact_classes.insert(cid);
            }
        }
    }

    /// Anchor first-snapshotted classes to `compile_seq` for grace tracking.
    pub fn record_snapshotted_classes(&self, class_ids: impl IntoIterator<Item = u32>) {
        let mut state = self.consumer_state.lock().unwrap_or_else(|p| p.into_inner());
        let seq = state.compile_seq;
        for cid in class_ids {
            if cid != 0 {
                state.snapshotted_at_compile.entry(cid).or_insert(seq);
            }
        }
    }

    pub fn has_consumer_class(&self, class_id: u32) -> bool {
        class_id != 0
            && self
                .consumer_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .consumer_classes
                .contains(&class_id)
    }

    /// Clear consumer tracking for redefined or unloaded classes.
    pub fn forget_classes(&self, mut pred: impl FnMut(u32) -> bool) {
        let mut state = self.consumer_state.lock().unwrap_or_else(|p| p.into_inner());
        state.consumer_classes.retain(|&c| !pred(c));
        state.failed_exact_classes.retain(|&c| !pred(c));
        state.snapshotted_at_compile.retain(|&c, _| !pred(c));
    }
}

thread_local! {
    /// Snapshot `addr -> class_id` of the compile on this thread ([`CompileClasses`]).
    static COMPILE_OBJECTS: std::cell::RefCell<HashMap<usize, u32>> =
        std::cell::RefCell::new(HashMap::new());
    /// Classes of snapshot objects handed to the planner as operands.
    static COMPILE_CLASSES: std::cell::RefCell<HashSet<u32>> =
        std::cell::RefCell::new(HashSet::new());
}

/// Re-entrant TLS guard scoping [`compile_has_class`] to the current compile
/// (saves and restores outer state across nested `callee_compiler` invocations).
pub struct CompileClasses {
    prev_objects: HashMap<usize, u32>,
    prev_classes: HashSet<u32>,
}

impl CompileClasses {
    pub fn of_facts(facts: Option<&FrozenFacts>) -> Self {
        let next_objects: HashMap<usize, u32> = facts
            .into_iter()
            .flat_map(|f| {
                f.objects
                    .iter()
                    .filter(|o| o.class_id != 0 && o.addr != 0)
                    .map(|o| (o.addr, o.class_id))
            })
            .collect();
        let prev_objects =
            COMPILE_OBJECTS.with(|c| std::mem::replace(&mut *c.borrow_mut(), next_objects));
        let prev_classes = COMPILE_CLASSES.with(|c| std::mem::take(&mut *c.borrow_mut()));
        Self {
            prev_objects,
            prev_classes,
        }
    }

    /// Classes taken as planned outright (tests).
    pub fn of_classes(classes: impl IntoIterator<Item = u32>) -> Self {
        let next_classes: HashSet<u32> = classes.into_iter().filter(|&id| id != 0).collect();
        let prev_objects = COMPILE_OBJECTS.with(|c| std::mem::take(&mut *c.borrow_mut()));
        let prev_classes =
            COMPILE_CLASSES.with(|c| std::mem::replace(&mut *c.borrow_mut(), next_classes));
        Self {
            prev_objects,
            prev_classes,
        }
    }
}

impl Drop for CompileClasses {
    fn drop(&mut self) {
        let prev_objects = std::mem::take(&mut self.prev_objects);
        let prev_classes = std::mem::take(&mut self.prev_classes);
        COMPILE_OBJECTS.with(|c| *c.borrow_mut() = prev_objects);
        COMPILE_CLASSES.with(|c| *c.borrow_mut() = prev_classes);
    }
}

/// Mark the classes of `objects` as planned operands for this compile.
pub fn note_planned_objects(objects: &[Option<usize>]) {
    if objects.iter().all(Option::is_none) {
        return;
    }
    COMPILE_OBJECTS.with(|by_addr| {
        let by_addr = by_addr.borrow();
        COMPILE_CLASSES.with(|c| {
            c.borrow_mut()
                .extend(objects.iter().flatten().filter_map(|a| by_addr.get(a).copied()));
        });
    });
}

/// True when `class_id` is the class of a snapshot object planned as an operand
/// in this thread's compile.
pub fn compile_has_class(class_id: u32) -> bool {
    class_id != 0 && COMPILE_CLASSES.with(|c| c.borrow().contains(&class_id))
}

impl FrozenFacts {
    /// Index of the object keyed by `addr` in [`Self::objects`], cached lazily.
    pub fn object_id(&self, addr: usize) -> Option<u32> {
        if addr == 0 {
            return None;
        }
        let idx = self.addr_index.get_or_init(|| {
            self.objects
                .iter()
                .enumerate()
                .filter(|(_, o)| o.addr != 0)
                .filter_map(|(id, o)| Some((o.addr, u32::try_from(id).ok()?)))
                .collect()
        });
        idx.get(&addr).copied()
    }

    /// `(class_id, addr)` of the reference `field_index` holds on `addr`.
    pub fn field_target(&self, addr: usize, field_index: usize) -> Option<(u32, usize)> {
        let id = self.object_id(addr)?;
        let object = self.live(id)?;
        let FrozenValue::Ref(target) = (*object.fields.get(field_index)?)? else {
            return None;
        };
        let target = self.objects.get(target as usize)?;
        (target.class_id != 0 && target.addr != 0).then_some((target.class_id, target.addr))
    }

    /// Object `id` when it carries at least one foldable fact.
    pub fn live(&self, id: u32) -> Option<&FrozenObject> {
        self.objects.get(id as usize).filter(|o| {
            o.array_len.is_some()
                || o.fields.iter().any(Option::is_some)
                || o.elements.iter().any(Option::is_some)
        })
    }
}
