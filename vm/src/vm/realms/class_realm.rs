// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class loading, linking, resolution and per-class caches. Owns the L10 lock.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.classes.<field>`.

use crate::classloading::resolution::{LambdaCallSite, LinkResolver, ResolutionCache};
use crate::classloading::{ClassId, ClassManager};
use crate::runtime::lock_order::{
    LockLevel, OrderedPlMutex, OrderedPlRwLock, OrderedPlRwLockWriteGuard,
};
use crate::types::{ObjectRef, Value};
use crate::vm::vm_init::ANON_CLASS_CACHE_LEN;
use parking_lot::RwLock;

/// First `ClassId` the lambda-proxy allocator ([`ClassRealm::next_lambda_id`])
/// hands out. Synthetic proxy ids are minted from a counter seeded here so they
/// can never collide with a real `ClassStore` id, which starts at 0 and is
/// bumped once per loaded class -- 2^31 real classes is not a reachable state.
///
/// The range is therefore a **sound necessary precondition**: a class id below
/// this base is not a lambda proxy, and no lookup is needed to say so. That
/// matters because `lambda_proxies` is behind an `RwLock` and was probed on
/// every non-`invokespecial` virtual invoke in the interpreter -- see
/// [`ClassRealm::is_lambda_proxy_class`].
///
/// Declared here so the seed and the predicate cannot drift apart; `vm_init`
/// seeds the counter from this constant.
pub const LAMBDA_PROXY_ID_BASE: u32 = 0x8000_0000;

/// The VM-internal class every annotation proxy instance shares.
///
/// No JDK declares this name -- it is minted by `ensure_vm_internal_class` and
/// lives in the VM's own reserved namespace -- so exactly one class can ever
/// carry it, under the bootstrap loader. That uniqueness is what lets
/// [`ClassRealm::is_annotation_proxy_class`] answer from a single `ClassId`
/// instead of a name comparison under the class-manager lock.
pub const ANNOTATION_PROXY_CLASS: &str = "java/lang/annotation/AnnotationProxy";
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// One class's static-field storage, at a **stable, never-freed address**.
///
/// Static reads were the most expensive field access in compiled code (~35 ns
/// against HotSpot's ~1 — see
/// `jit-getstatic-costs-a-helper-call-FIXED-20260803.md`). What
/// was left after trimming the helper is the `RwLock` + hash probe that reaching
/// a `Vec<Value>` inside an `FxHashMap` requires — and, once this block had a
/// stable address, the prerequisite for deleting the helper CALL outright:
/// compiled `getstatic` now bakes [`StaticsIndex::base_cell_addr`] and loads
/// from the block directly.
///
/// A `Vec` cannot be read without that lock, because a `resize` would move the
/// buffer out from under a concurrent reader. This block therefore **leaks**
/// its allocation: created once, never freed, and growth allocates a *new*
/// block while leaving the old one mapped. A raw pointer handed out here stays
/// valid for the life of the VM, which is what lets [`StaticsIndex`] serve
/// reads with no lock at all.
///
/// Deliberately `Deref<Target = [Value]>` so call sites that index, iterate or
/// take `.len()` keep working unchanged.
pub struct StaticsBlock {
    ptr: *mut Value,
    len: usize,
}

// SAFETY: a plain owned allocation of `Value` (itself `Send + Sync`). All
// mutation still happens under the `statics` write lock, exactly as when this
// was a `Vec<Value>`; the raw pointer changes nothing about which thread may
// touch it.
unsafe impl Send for StaticsBlock {}
unsafe impl Sync for StaticsBlock {}

impl StaticsBlock {
    /// Allocate `len` zero-initialized slots and leak them.
    pub fn new(len: usize) -> Self {
        Self::from_values(vec![Value::Int(0); len])
    }

    pub fn from_values(values: Vec<Value>) -> Self {
        let leaked: &'static mut [Value] = Box::leak(values.into_boxed_slice());
        Self {
            ptr: leaked.as_mut_ptr(),
            len: leaked.len(),
        }
    }

    /// Base address of slot 0. Stable for the life of the VM.
    #[inline]
    pub fn base_ptr(&self) -> *mut Value {
        self.ptr
    }

    #[inline]
    pub fn slot_len(&self) -> usize {
        self.len
    }

    /// Grow to at least `new_len`, preserving contents.
    ///
    /// The OLD block is intentionally left allocated: a lock-free reader may
    /// still hold a pointer into it. Growth only fires when a write targets an
    /// index past the class's declared field count — already the unexpected
    /// path — so the leak is bounded by that rarity.
    pub fn grow_to(&mut self, new_len: usize) {
        if new_len <= self.len {
            return;
        }
        let mut values = vec![Value::Int(0); new_len];
        values[..self.len].copy_from_slice(self.as_slice());
        let grown = Self::from_values(values);
        self.ptr = grown.ptr;
        self.len = grown.len;
    }

    #[inline]
    pub fn as_slice(&self) -> &[Value] {
        // SAFETY: `ptr`/`len` describe a leaked, never-freed allocation.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [Value] {
        // SAFETY: as above; `&mut self` proves exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl std::ops::Deref for StaticsBlock {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for StaticsBlock {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        self.as_mut_slice()
    }
}

pub struct StaticsIndexSlot {
    base: std::sync::atomic::AtomicPtr<Value>,
    len: AtomicUsize,
}

/// Lock-free `ClassId -> statics base pointer` index.
///
/// Mirrors `ClassRealm::statics`, so a reader that needs one slot can skip the
/// `RwLock` and the hash probe entirely. Entries are published when a class's
/// [`StaticsBlock`] is created or grown; because blocks are never freed, a
/// pointer read from here stays valid.
///
/// Ids at or beyond [`StaticsIndex::CAPACITY`] are simply not indexed and fall
/// back to the map, making the capacity a performance bound, not a correctness
/// one.
pub struct StaticsIndex {
    slots: OnceLock<Box<[StaticsIndexSlot]>>,
}

impl Default for StaticsIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticsIndex {
    /// Covers class ids `[0, 1 << 17)`; ~1.5 MiB, allocated on first publish.
    pub const CAPACITY: usize = 1 << 17;

    pub const fn new() -> Self {
        Self {
            slots: OnceLock::new(),
        }
    }

    fn slots(&self) -> &[StaticsIndexSlot] {
        self.slots.get_or_init(|| {
            (0..Self::CAPACITY)
                .map(|_| StaticsIndexSlot {
                    base: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
                    len: AtomicUsize::new(0),
                })
                .collect()
        })
    }

    /// Publish (or re-publish, after a grow) a class's statics base.
    ///
    /// `len` is stored BEFORE `base`, and `base` with `Release`: a reader loads
    /// `base` with `Acquire` and only then consults `len`, so a non-null base
    /// can never be paired with a length that overstates its block.
    pub fn publish(&self, class_id: ClassId, block: &StaticsBlock) {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return;
        }
        let slot = &self.slots()[idx];
        slot.len.store(block.slot_len(), Ordering::Relaxed);
        slot.base.store(block.base_ptr(), Ordering::Release);
    }

    /// Read one static slot without taking any lock.
    ///
    /// `None` means "not indexed / out of range" — the caller falls back to the
    /// locked map path.
    #[inline]
    pub fn get(&self, class_id: ClassId, field_index: usize) -> Option<Value> {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return None;
        }
        // Never force the 1.5 MiB allocation for a VM that has not published.
        let slots = self.slots.get()?;
        let slot = &slots[idx];
        let base = slot.base.load(Ordering::Acquire);
        if base.is_null() || field_index >= slot.len.load(Ordering::Relaxed) {
            return None;
        }
        // Read the cell as two independent 8-byte words instead of copying
        // the 16-byte `Value` in one go.
        //
        // `*base.add(i)` on a 16-byte type is lowered as a wide (SSE) copy, and
        // a 16-byte access is NOT single-copy atomic on x86-64. Against a
        // concurrent `putstatic` this reader observed **halves of two different
        // writes**: `probes/StaticRaceProbe.java` reproduces it in seconds, a
        // `static long` flipped between `0x0123456789ABCDEF` and
        // `0x7EDCBA9876543210` reading back as `0x7EDCBA9889ABCDEF`. For a
        // plain `long`/`double` static JLS §17.7 permits that; for a
        // **`volatile`** one it does not, and the same probe tore a
        // `static volatile long` too — on the interpreter path (`--nojit`) as
        // well, since `get_static_shared` routes through here.
        //
        // Two aligned 8-byte volatile loads fix it: the discriminant word and a
        // 4-byte payload share the low word, an 8-byte payload IS the high
        // word, so no payload ever spans a load. `read_volatile` is what stops
        // the optimizer from merging them back into one wide move.
        //
        // The STORE side needs no matching change, and that is a measurement,
        // not an assumption: with the read narrowed, 15 s x 4 reader threads x
        // {inline JIT, helper JIT, interpreter} observed no tear at all, which
        // is only possible if the payload qword is already stored atomically.
        // (Which is what x86-64 gives: whatever a 16-byte struct store lowers
        // to, it never splits an 8-byte-aligned qword.)
        //
        // Transmuting the pair back into a `Value` needs no knowledge of the
        // enum's discriminant encoding, only its size — and every bit pattern
        // that can be assembled here is a valid `Value`, including a tag from
        // one write paired with a payload from another: the `Object` variant is
        // `Option<ObjectRef>`, whose niche makes a zero pointer word `None`
        // rather than an invalid `NonNull`.
        const _: () = assert!(std::mem::size_of::<Value>() == 16);
        // SAFETY: `base` points into a leaked, never-freed `StaticsBlock`, and
        // `field_index` was bounds-checked against the length published for
        // that same block. `base` is 8-aligned (it is a `Value` pointer), so
        // both words are aligned. The read stays unsynchronized against a
        // concurrent `putstatic` — as the JIT's inline `getfield` is for
        // instance-field cells — it is only no longer TORN.
        let cell = unsafe { base.add(field_index) } as *const u64;
        let lo = unsafe { cell.read_volatile() };
        let hi = unsafe { cell.add(1).read_volatile() };
        // SAFETY: `Value` is 16 bytes (asserted above) and these are its own
        // bytes, read from a live cell.
        Some(unsafe { std::mem::transmute::<[u64; 2], Value>([lo, hi]) })
    }

    /// Address of the `AtomicPtr` **cell** that names a class's statics base —
    /// what compiled `getstatic` code bakes as an immediate.
    ///
    /// Deliberately NOT the block address. A `StaticsBlock` is stable for the
    /// life of the VM in the normal case, but two paths can still publish a
    /// different one for the same class: `StaticsBlock::grow_to` (a write past
    /// the published length) and a re-`prepare_class_shared`. Baking the block
    /// address would leave compiled code reading the abandoned copy — writes
    /// would land in the new block and never be observed. Baking the address of
    /// the pointer cell costs one extra dependent load and makes every
    /// republication visible to already-compiled code with no patching, no
    /// invalidation protocol and no new invariant to maintain: the slot array
    /// is allocated once (`OnceLock`) and never freed, so this address is valid
    /// forever.
    ///
    /// `None` = not indexable, nothing published yet, or `field_index` past the
    /// published length — the caller keeps the `jit_getstatic` helper path.
    pub fn base_cell_addr(&self, class_id: ClassId, field_index: usize) -> Option<usize> {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return None;
        }
        let slots = self.slots.get()?;
        let slot = &slots[idx];
        if slot.base.load(Ordering::Acquire).is_null()
            || field_index >= slot.len.load(Ordering::Relaxed)
        {
            return None;
        }
        Some(&slot.base as *const std::sync::atomic::AtomicPtr<Value> as usize)
    }
}

/// Class loading, linking, resolution and per-class caches. Owns the L10 lock.
pub struct ClassRealm {
    /// Class loader and cache, protected by an RwLock.
    ///
    /// L10 in the global lock hierarchy — the coarsest lock, acquired first.
    /// See [`crate::runtime::lock_order`]. [`OrderedPlRwLock`] is a drop-in for
    /// `parking_lot::RwLock` that asserts the descending acquisition order on
    /// every `.read()` / `.write()` / `.read_recursive()`, so holding any
    /// lower-level lock (a monitor, `ref_processor`, …) across a class-manager
    /// acquisition is caught instead of deadlocking.
    pub class_manager: OrderedPlRwLock<ClassManager>,

    /// Lock-free cache of the shared `cratonvm/synthetic/AnonymousObject$N`
    /// ClassId, indexed by field count `N` (slot 0 is unused — `num_fields > 0`
    /// always on this path). `0` means "not yet resolved" — a valid sentinel
    /// because these synthetic classes are minted with non-zero ClassIds
    /// (`ClassId(0)` is `java/lang/Object`).
    ///
    /// `vm_exec::alloc_object` rewrites every `ClassId(0)`-with-fields
    /// allocation (e.g. a `HashMap` node) to one of these synthetic classes.
    /// The first allocation for a given `N` resolves it through
    /// `ClassManager::ensure_synthetic_class` (a write-lock + name `format!` +
    /// hash probe) and stores the id here; every later allocation reads the id
    /// with a single relaxed atomic load and skips the class-manager locks
    /// entirely. The synthetic stub declares exactly `N` fields, so the
    /// field-count clamp is a provable no-op and is skipped on the fast path.
    pub anon_class_cache: [std::sync::atomic::AtomicU32; ANON_CLASS_CACHE_LEN],

    /// Static fields: class_id -> field_index -> Value.
    /// T10.9.B: FxHashMap — keys are internal ClassId, hot path accessed
    /// on every getstatic/putstatic bytecode.
    pub statics: RwLock<FxHashMap<ClassId, StaticsBlock>>,

    /// Lock-free mirror of [`Self::statics`] for single-slot reads. Kept in
    /// step by `set_static_shared` / the class-init path, which publish every
    /// block they create or grow.
    pub statics_index: StaticsIndex,

    /// `java/lang/System`'s `ClassId` in THIS VM, or `u32::MAX` while unknown.
    ///
    /// Recorded by `prepare_class_shared`, which already has the class name in
    /// hand. Compiled `getstatic` uses it as a lock-free "does this read need
    /// the `System.out`/`err`/`in` bootstrap intercept?" test before deciding
    /// to emit a direct load: the intercept lives in `jit_getstatic`, so a
    /// static of this one class must never bypass the helper. Answering by
    /// name would need a `class_manager` acquisition on the compiler thread,
    /// which is the one thing the resolver must not do.
    ///
    /// Per-`ClassRealm`, so unlike the deleted process-global `system_class_id`
    /// atomic it cannot leak one VM's id into another's decisions.
    pub system_class_id: AtomicU32,

    /// Cache of resolved symbolic references (fields and methods).
    pub resolution_cache: RwLock<ResolutionCache>,

    /// Round 8 audit fix (CRIT #2): the reflective `(class, name,
    /// descriptor)` cache. Previously built (`LinkResolver::new()`) but
    /// never wired into any caller, so the entire dedupe win was dead
    /// code. Now reachable from native reflective callers
    /// (`Class.getDeclaredMethod`, `Class.getMethod`, JNI
    /// `GetMethodID`/`GetFieldID`) via `vm.link_resolver()`. The
    /// cache is invalidated on `redefine_class` through the same
    /// hook that drops `ResolutionCache` entries (see
    /// `link_resolver_invalidate_adapter`).
    pub link_resolver: LinkResolver,

    /// T10 — vtable manager for virtual/interface dispatch.
    ///
    /// Shared as an `Arc` so the class-loader install hook (a plain
    /// `fn` pointer with no captured state) can reach the same manager
    /// via the `crate::runtime::vtable::global_vtable_manager()` cell.
    pub vtable_manager: std::sync::Arc<parking_lot::RwLock<crate::runtime::vtable::VtableManager>>,

    /// T10 — read-optimized shared resolution cache (uses RwLock internally).
    pub shared_resolution: crate::runtime::lockfree_resolve::SharedResolutionState,

    /// Per-class synthetic lock objects for static synchronized methods.
    /// When a static synchronized method is called, we need an object to use
    /// as the monitor (since there's no `this`). We allocate a dummy object
    /// per class and store it here.
    /// T10.9.B: FxHashMap — ClassId-keyed internal cache.
    pub class_locks: RwLock<FxHashMap<ClassId, ObjectRef>>,

    /// Class mirror cache: maps ClassId to java/lang/Class ObjectRef.
    /// Used by `Object.getClass()`.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    pub class_mirrors: RwLock<FxHashMap<ClassId, ObjectRef>>,

    /// Reverse of `class_mirrors`: maps a Class mirror back to its ClassId.
    /// Populated whenever `get_or_create_class_mirror` allocates a new mirror.
    /// This lets `mirror_class_id` recover the ClassId without storing it in
    /// the mirror's Java-visible fields (which would clash with the real-JDK
    /// `java/lang/Class` layout).
    /// T10.9.B: FxHashMap — ObjectRef pointer keys.
    pub class_mirrors_reverse: RwLock<FxHashMap<ObjectRef, ClassId>>,

    /// Loader-faithful resolution cache (gated by
    /// `CRATONVM_LOADER_AWARE_RESOLUTION`). Maps an *initiating* loader and an
    /// internal class name to the `ClassId` that loader resolves it to —
    /// CratonVM's analogue of the JVMS §5.4.3 *initiating loader* table. Only
    /// populated for references reached from bytecode defined by a user-defined
    /// loader: once `resolve_class_loader_aware` has driven that loader's
    /// `loadClass` (or proved the name is bootstrap/global) for a given name,
    /// the answer is memoised here so subsequent references skip the re-entrant
    /// `loadClass` invocation. Grow-only — CratonVM does not unload classes, and
    /// in-place `redefine_class` keeps the `ClassId` stable. Empty (and never
    /// read) when the gate is off.
    ///
    /// Nested `loader -> (name -> id)` so a hot-path read can probe by borrowed
    /// `&str` (`Arc<str>: Borrow<str>`) without allocating an `Arc` per lookup.
    pub initiating_resolution_cache:
        RwLock<FxHashMap<cratonvm_types::ClassLoaderId, FxHashMap<Arc<str>, ClassId>>>,

    /// Lambda proxy registry: maps synthetic proxy ClassId → LambdaCallSite metadata.
    /// Used by the interpreter to dispatch method calls on lambda proxy objects.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    ///
    /// Held behind an `Arc` because `try_lambda_dispatch` must take an owned
    /// copy out from under the `RwLock` before it can run the lambda body (the
    /// body can itself register proxies, so the read guard cannot be held
    /// across it). With a bare `LambdaCallSite` that copy was a DEEP CLONE on
    /// every lambda call — six `Arc<str>` refcount pairs plus a fresh
    /// `Vec<char>` — and the clone/drop/`mi_free`/`memmove` around it measured
    /// ~27% of a lambda-only `perf` profile. As an `Arc` the same line is one
    /// refcount bump.
    pub lambda_proxies: RwLock<FxHashMap<ClassId, Arc<LambdaCallSite>>>,

    /// Cache of the synthesized `java.lang.reflect.Method` (with its
    /// `parameterTypes`/`exceptionTypes` arrays and name/signature strings
    /// already populated) that `proxy_invoke_handler_shared` builds for every
    /// `InvocationHandler.invoke(proxy, method, args)` dispatch. Keyed by
    /// (proxy's ClassId, method name, descriptor) — real JDK dynamic-proxy
    /// classes build this Method object ONCE per interface method in their
    /// static initializer, not per call; before this cache, CratonVM rebuilt
    /// it (a fresh object + two arrays + two strings) on every single
    /// reflective dispatch, which on allocation-heavy reflection-driven
    /// workloads (e.g. ByteBuddy's `JavaDispatcher.INVOKER`) generated enough
    /// short-lived garbage to fragment the non-compacting old-gen arena and
    /// OOM even though most of the heap was nominally free.
    /// Grow-only, bounded by the number of distinct (proxy class, method)
    /// pairs an application actually exercises — not user-input-sized.
    /// Rooted unconditionally in `memory::roots` §6b alongside `class_mirrors`
    /// since these Method objects, like class mirrors, are meant to outlive
    /// any single call and be shared across every future dispatch to the
    /// same proxy method — **and remapped** in
    /// `memory::gc::update_all_roots` §6a via `remap_proxy_method_cache`.
    ///
    /// Both halves are load-bearing, and for a long time only the scan half
    /// shipped. Rooting alone makes a MOVING young collection *evacuate* the
    /// Method (it is reachable) and leave this map holding the from-space
    /// address, so the next dispatch on the same key hands running Java an
    /// object whose body has been reset: `Method.getName()` returns `null`,
    /// and any `InvocationHandler` that switches on the method name (javac
    /// lowers a String switch to `String.hashCode()`) throws
    /// `NullPointerException: Cannot invoke "String.hashCode()"`. That was
    /// 18 + 11 Spring Boot failures under `-XX:+UseGenerationalGC` with ZGC
    /// and G1 green on the same binary. Witness:
    /// `probes/ProxyMethodNameProbe.java`.
    pub proxy_method_cache: RwLock<FxHashMap<(ClassId, String, String), ObjectRef>>,

    /// Resolved implementation-owner `ClassId` for each lambda proxy id, on the
    /// **globally-resolved** path only.
    ///
    /// `try_lambda_dispatch` used to re-resolve its target *by name* on every
    /// single SAM call — `invoke_shared`'s `load_class_concurrent` for the
    /// `InvokeStatic` arm, and a `class_manager.write()` + `load_class` for the
    /// `InvokeSpecial` / `NewInvokeSpecial` arms. `CRATONVM_DBG=lambda-prof`
    /// priced it: of ~3,000-3,900 ns per dispatch of an **empty** lambda,
    /// **2,300-3,080 ns was the target invoke** — and with a no-op body, that is
    /// all resolution — against ~130 ns for the proxy-table lookup and ~230-300
    /// ns for argument coercion. It is what made a lambda's interface call cost
    /// ~4,200 ns against ~18 ns for the identical call on a named class, and why
    /// the JIT was worth only 17% on Spring context startup: the cost sits in
    /// Rust dispatch machinery that compiled Java bodies never enter.
    ///
    /// Only the *global* answer is memoised. The loader-faithful override
    /// (`lambda_impl_dispatch_override_driven`) is still consulted first on
    /// every call and is not cached here, so a user-loader-local copy keeps
    /// winning exactly as before; this table is only ever reached once that
    /// override has declined, which means the by-name answer is the stable
    /// global copy. Grow-only, for the same reason `initiating_resolution_cache`
    /// is: CratonVM does not unload classes, and in-place `redefine_class` keeps
    /// the `ClassId`.
    pub lambda_impl_owner_memo: RwLock<FxHashMap<ClassId, ClassId>>,

    /// Defining class (the class whose `invokedynamic` created this lambda /
    /// method-ref) for each lambda proxy id — what HotSpot names the proxy after
    /// and reports as its nest host. Kept as a side table so the reflection name
    /// natives report `<host>$$Lambda/0x<id>` and a correct `getNestHost()` even
    /// for a cross-class method reference, whose implementation method lives in a
    /// different class than the one that defines the reference. bug-06 fam5 #1.
    pub lambda_proxy_hosts: RwLock<FxHashMap<ClassId, ClassId>>,

    /// Counter for generating unique synthetic lambda proxy ClassIds.
    /// Seeded from [`LAMBDA_PROXY_ID_BASE`] to avoid collisions with real
    /// ClassIds from the ClassStore, and to keep the seed and the range test
    /// in [`ClassRealm::is_lambda_proxy_class`] spelled once.
    pub next_lambda_id: AtomicU32,

    /// Resolved `ClassId` of [`ANNOTATION_PROXY_CLASS`], or `u32::MAX` while it
    /// has not been observed. Written once, by
    /// [`ClassRealm::resolve_annotation_proxy_cid`].
    ///
    /// Per-realm rather than process-global on purpose: two VMs in one process
    /// mint the class independently and need not agree on its id, and this
    /// value gates a **correctness** decision (annotation-proxy dispatch must
    /// reach `execute_invoke`'s interception layer), not a fast-path hint.
    ///
    /// Safe to hold without an invalidation key: CratonVM does not unload
    /// classes and in-place `redefine_class` keeps the `ClassId`, so a
    /// `ClassId`'s class identity is immutable for the life of the VM. That is
    /// the same standing property `lambda_impl_owner_memo` above relies on.
    pub annotation_proxy_cid: AtomicU32,

    /// `class_definition_epoch()` at which [`ANNOTATION_PROXY_CLASS`] was last
    /// confirmed **not** to be defined, or `u64::MAX` if it has never been
    /// looked up.
    ///
    /// The negative half needs a key where the positive half does not: "no
    /// class carries this name" is only true until the next class definition,
    /// and the epoch moves on every one of them (`loaded_classes_insert` is its
    /// sole writer). Holding the answer for one epoch turns an unbounded
    /// per-invoke lookup into one lookup per class defined.
    pub annotation_proxy_absent_epoch: std::sync::atomic::AtomicU64,

    /// Primitive type Class mirrors: "int" → ObjectRef, "boolean" → ObjectRef, etc.
    /// T10.9.B: FxHashMap — keys are fixed primitive type names, not user input.
    pub primitive_mirrors: RwLock<FxHashMap<String, ObjectRef>>,

    /// Canonical `java.lang.Module` mirrors keyed by module name (e.g.
    /// "java.base"), with `""` reserved for the unnamed module. `Class.getModule()`
    /// MUST hand back the SAME `Module` instance for every class in a module,
    /// because the JDK compares modules by identity (`Module` does not override
    /// `equals`). Real bytecode relies on this — e.g.
    /// `Throwable.validateSuppressedExceptionsList` deserializes a Throwable's
    /// suppressed-exceptions list and throws `StreamCorruptedException("List
    /// implementation not in base module.")` unless
    /// `Object.class.getModule() == deserializedList.getClass().getModule()`.
    /// Allocating a fresh Module per `getModule()` call (the old behaviour) made
    /// that comparison always false and broke ObjectInputStream round-trips of any
    /// object holding a `java.util` List (HIB-CV-29).
    pub module_mirrors: RwLock<FxHashMap<String, ObjectRef>>,

    /// Cached field count for java/lang/String (0 = not yet resolved).
    /// Once resolved, this is the actual `num_total_fields` from the loaded
    /// String class, avoiding lock contention in `create_java_string()`.
    pub cached_string_num_fields: AtomicUsize,

    /// True when the loaded java/lang/String uses JDK 9+ compact strings
    /// (byte[] value + byte coder) instead of pre-JDK-9 char[] layout.
    /// Set once during bootstrap; read by create_java_string / read_java_string.
    pub compact_strings: std::sync::atomic::AtomicBool,

    /// Cached field count for java/lang/Class mirror objects (0 = not yet resolved).
    pub cached_class_mirror_num_fields: AtomicUsize,

    /// Per-class initialization condition variables (JVM spec §5.5).
    ///
    /// When a thread starts initializing a class, it inserts an entry here.
    /// Other threads that try to initialize the same class will wait on the
    /// condvar until the initializing thread completes (success or error).
    /// Entries are removed once initialization finishes.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    /// Round-9 HIGH-4: inner `std::sync::Mutex<bool>` + `std::sync::Condvar`
    /// migrated to `parking_lot` equivalents — removes poison handling and
    /// yields a smaller, faster condvar with the same wait/notify API.
    pub class_init_waiters: parking_lot::Mutex<
        FxHashMap<ClassId, Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>>,
    >,

    /// Per-class-name loading locks (Session 30: Thread-Safe Class Loading).
    ///
    /// When a thread starts loading a class, it acquires the per-name lock.
    /// Other threads that try to load the *same* class will block on this lock
    /// instead of blocking the global ClassManager write lock, which allows
    /// concurrent loading of *different* classes to proceed without contention.
    /// The bool inside the Mutex indicates whether loading is in-progress.
    /// T10.9.B: FxHashMap — keys are internal class names.
    pub class_loading_locks:
        parking_lot::Mutex<FxHashMap<String, Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>>>,

    /// T10.9.E — Per-(class, slot-index) cache of the field's declared
    /// descriptor byte (the first byte of the JVM type descriptor:
    /// `b'J'` for `long`, `b'D'` for `double`, `b'L'` or `b'['` for
    /// reference types, etc.).
    ///
    /// Populated lazily on the first native-side field access that
    /// resolves the descriptor via the class manager, then consulted by
    /// `NativeContextImpl::get_field`/`set_field` on every subsequent
    /// access to the same slot so the hot path stays O(1) after the
    /// first resolution. Missing entries (never-seen class or index)
    /// fall back to the legacy descriptor-unaware `get_field`/`set_field`
    /// paths so the cache is strictly a correctness normalizer +
    /// performance acceleration, never a correctness hazard.
    ///
    /// Sizing: unbounded by design — total entries are bounded by
    /// `(loaded_classes * avg_fields)`, typically ~O(10k) for large
    /// applications like WildFly/Keycloak. The hottest access pattern
    /// (Unsafe-style concurrent data structures re-reading a handful
    /// of offsets) produces at most a few dozen entries per class.
    /// No eviction: entries are stable for the lifetime of the VM
    /// because class bytecode is immutable after loading.
    pub field_descriptor_cache:
        parking_lot::RwLock<crate::runtime::fx_collections::FxHashMap<(ClassId, usize), u8>>,

    /// WP0.2 — per-class cache of `ObjectStreamClass` descriptor
    /// mirrors. Populated by the first call to
    /// `ObjectStreamClass.lookup(cls)` for any given class; every
    /// subsequent lookup returns the same `ObjectRef`, matching the
    /// real-JDK singleton-per-class semantics (`ObjectStreamClass.
    /// Caches.localDescs`).
    ///
    /// See `crate::runtime::serialization::oscache` for the cache
    /// implementation and rationale.
    pub osc_cache: crate::runtime::serialization::OscCache,
}

impl ClassRealm {
    /// Is `class_id` one of the synthetic proxy classes spun for a lambda or
    /// method reference?
    ///
    /// Lambda proxies have no bytecode implementation of their
    /// functional-interface method, so every dispatch path that would install
    /// or use a direct target has to recognise them and cede to the slow SAM
    /// route. That check ran as `lambda_proxies.read().contains_key(&cid)` on
    /// **every** non-`invokespecial` virtual invoke in the interpreter -- an
    /// `RwLock` acquisition plus a hash probe to answer "no" for every ordinary
    /// class in the program.
    ///
    /// The range test in front of it is exact rather than heuristic: proxy ids
    /// come only from `alloc_lambda_proxy_id`, whose counter is seeded at
    /// [`LAMBDA_PROXY_ID_BASE`], so an id below the base cannot be in the map.
    /// A hit still pays the full lookup, which is what keeps the answer
    /// identical -- the map, not the range, remains the authority on membership
    /// (`MAX_LAMBDA_PROXIES` means an allocated id can be refused a table slot).
    #[inline]
    pub fn is_lambda_proxy_class(&self, class_id: ClassId) -> bool {
        class_id.as_u32() >= LAMBDA_PROXY_ID_BASE
            && self.lambda_proxies.read().contains_key(&class_id)
    }

    /// Is `class_id` the VM-internal [`ANNOTATION_PROXY_CLASS`]?
    ///
    /// Annotation proxies have no real bytecode for the `Annotation` contract
    /// (nor for `Object.equals`/`hashCode`/`toString`), so a cached virtual
    /// target must never serve them -- the dispatch has to fall through to
    /// `execute_invoke`'s interception layer. The gate is consumed immediately
    /// and decides correctness, so unlike an optional tier-up predicate it
    /// cannot be deferred; it can only be made cheaper.
    ///
    /// Steady state is one relaxed load and one `u32` compare. The name
    /// comparison it replaces took the class-manager read lock, resolved the
    /// class, and compared an `Arc<str>` against a literal, once per virtual
    /// invoke.
    ///
    /// # The `u32::MAX` state is the common one, and it used to be the slow one
    ///
    /// "Steady state" above meant *once the class exists*. `AnnotationProxy` is
    /// a VM-internal synthetic class that most programs never mint, so the hint
    /// stayed `u32::MAX` for the whole process and every virtual invoke reached
    /// [`Self::resolve_annotation_proxy_cid`] — whose negative cache is keyed
    /// on `class_definition_epoch()`, bumped by *every* class definition.
    /// During class loading (Tomcat's annotation scan, Spring cold start — the
    /// phase that runs interpreted) the epoch never holds still, so the
    /// negative never held and each virtual invoke took the class-manager read
    /// lock and probed the name across the builtin loader delegation chain.
    ///
    /// `any_annotation_proxy_defined()` closes that: a process-global one-way
    /// latch raised by `loaded_classes_insert` the first time the name is
    /// defined *anywhere*, so it is immune to the epoch. False ⇒ no realm has
    /// the class ⇒ no `class_id` can be it. It gates only the *lookup*, never
    /// the answer — the per-realm `annotation_proxy_cid` still decides
    /// identity, because two VMs in one process mint the class independently
    /// and must not share an id.
    #[inline]
    pub fn is_annotation_proxy_class(&self, class_id: ClassId) -> bool {
        let hint = self.annotation_proxy_cid.load(Ordering::Relaxed);
        if hint != u32::MAX {
            return class_id.as_u32() == hint;
        }
        if !crate::classloading::any_annotation_proxy_defined() {
            return false;
        }
        self.resolve_annotation_proxy_cid() == Some(class_id)
    }

    /// The cold half of [`Self::is_annotation_proxy_class`]: look the class up
    /// by name, at most once per class-definition epoch until it exists.
    ///
    /// Answering by NAME rather than per-receiver is what makes the negative
    /// cacheable at all. "Is receiver X the proxy" depends on X and cannot be
    /// held across receivers; "is the proxy class defined yet" does not depend
    /// on X, and is fixed for as long as the class-definition epoch is.
    #[cold]
    fn resolve_annotation_proxy_cid(&self) -> Option<ClassId> {
        let epoch = crate::classloading::class_definition_epoch();
        if self.annotation_proxy_absent_epoch.load(Ordering::Relaxed) == epoch {
            return None;
        }
        let found = self
            .class_manager
            .read()
            .get_loaded_class_id(ANNOTATION_PROXY_CLASS);
        match found {
            Some(cid) => {
                self.annotation_proxy_cid
                    .store(cid.as_u32(), Ordering::Relaxed);
                Some(cid)
            }
            None => {
                self.annotation_proxy_absent_epoch
                    .store(epoch, Ordering::Relaxed);
                None
            }
        }
    }

    /// Acquire the L10 `class_manager` write lock through a guard that
    /// drains any JVMTI ClassLoad/ClassPrepare events queued during the
    /// critical section once the underlying lock is released.
    ///
    /// obsaudit D1 (2026-07-26): this is now the *only* sanctioned way to
    /// take this write lock — `cratonvm_classloading::fire_class_load_hook`/
    /// `fire_class_prepare_hook` no longer invoke an installed JVMTI hook
    /// synchronously; they queue the event on a thread-local and rely on
    /// this guard's `Drop` to fire it after the lock is gone. Acquiring the
    /// write lock any other way (e.g. reaching into `.class_manager_write()`
    /// directly) would silently strand queued events until the next time
    /// this method happens to run on the same OS thread — every former
    /// direct-`.write()` call site in the workspace was mechanically
    /// switched to this method for exactly that reason. See the DEFERRED
    /// FIRING notes in `classloading/src/class_manager.rs` near
    /// `install_class_load_hook`.
    pub fn class_manager_write(&self) -> ClassManagerWriteGuard<'_> {
        ClassManagerWriteGuard {
            guard: Some(self.class_manager.write()),
        }
    }
}

/// RAII write guard for [`ClassRealm::class_manager`] that fires queued
/// JVMTI class-lifecycle events after releasing the lock. See
/// [`ClassRealm::class_manager_write`].
pub struct ClassManagerWriteGuard<'a> {
    // `Option` so `Drop` can explicitly drop the inner guard (releasing the
    // lock) before draining hooks, rather than relying on field-drop order
    // (which Rust does guarantee top-to-bottom for a single-field struct,
    // but the `Option` makes the ordering an explicit, checkable step
    // instead of an implicit language rule future edits could disturb).
    guard: Option<OrderedPlRwLockWriteGuard<'a, ClassManager>>,
}

impl std::ops::Deref for ClassManagerWriteGuard<'_> {
    type Target = ClassManager;
    fn deref(&self) -> &ClassManager {
        self.guard.as_ref().expect("guard taken before drop")
    }
}

impl std::ops::DerefMut for ClassManagerWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut ClassManager {
        self.guard.as_mut().expect("guard taken before drop")
    }
}

impl Drop for ClassManagerWriteGuard<'_> {
    fn drop(&mut self) {
        // Release the class-manager write lock first...
        self.guard = None;
        // ...then fire whatever ClassLoad/ClassPrepare events this critical
        // section queued. A hook invoked from here may freely take a fresh
        // `class_manager_write()`/`.read()` itself: the lock this guard held
        // is already gone by this point.
        cratonvm_classloading::drain_pending_class_hooks();
    }
}

#[cfg(test)]
mod receiver_shape_gate_tests {
    use super::*;
    use crate::vm::SharedVm;
    use crate::VmConfig;
    use std::sync::Arc;

    /// The range test in [`ClassRealm::is_lambda_proxy_class`] must never
    /// answer "no" for an id that IS in the table, and must answer "no"
    /// without touching the table for every ordinary class id. Both halves are
    /// asserted: a range test that were merely conservative would be silently
    /// wrong here, because the caller uses `true` to cede to the slow SAM route.
    #[test]
    fn is_lambda_proxy_class_agrees_with_the_table() {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        // Every id the allocator hands out is inside the reserved range.
        let proxy = shared.alloc_lambda_proxy_id();
        assert!(
            proxy.as_u32() >= LAMBDA_PROXY_ID_BASE,
            "lambda proxy ids must come from the range the predicate tests"
        );

        // In range but not registered: the table stays the authority.
        assert!(!shared.classes.is_lambda_proxy_class(proxy));

        shared.classes.lambda_proxies.write().insert(
            proxy,
            Arc::new(LambdaCallSite {
                functional_interface_id: None,
                functional_interface: "java/lang/Runnable".into(),
                sam_method_name: "run".into(),
                sam_descriptor: "()V".into(),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: "test/A".into(),
                    member_name: "lambda$0".into(),
                    descriptor: "()V".into(),
                },
                instantiated_descriptor: "()V".into(),
                capture_types: vec![],
                proxy_class_id: proxy,
                serializable_flag: false,
            }),
        );
        assert!(shared.classes.is_lambda_proxy_class(proxy));

        // Ordinary class ids are below the base and answer "no".
        for raw in [0u32, 1, 42, 65_535, LAMBDA_PROXY_ID_BASE - 1] {
            assert!(
                !shared.classes.is_lambda_proxy_class(ClassId::new(raw)),
                "class id {raw} is not a lambda proxy"
            );
        }
    }

    /// [`ClassRealm::is_annotation_proxy_class`] replaced a name comparison
    /// under the class-manager lock with a `ClassId` identity test. The failure
    /// mode that matters is the NEGATIVE one: the memo must not latch "absent"
    /// from before the class was minted, because a stale `false` sends an
    /// annotation proxy down a cached-target path that has no bytecode for it.
    #[test]
    fn is_annotation_proxy_class_learns_the_class_after_it_is_minted() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        // Before the class exists every id answers "no" -- and asking
        // repeatedly is exactly what would latch a wrong answer if the negative
        // memo were not keyed on the class-definition epoch.
        for _ in 0..4 {
            for raw in [0u32, 1, 7, 1234] {
                assert!(!shared.classes.is_annotation_proxy_class(ClassId::new(raw)));
            }
        }

        // Mint it the way `ensure_vm_internal_class` does.
        let proxy_cid = shared.classes.class_manager_write().ensure_generated_class(
            ANNOTATION_PROXY_CLASS,
            4,
            cratonvm_classloading::ClassOrigin::VmInternal,
        );

        assert!(
            shared.classes.is_annotation_proxy_class(proxy_cid),
            "the proxy class must be recognised once defined, even though every \
             earlier query answered `no`"
        );
        // Repeat reads take the memoized identity path and stay stable.
        assert!(shared.classes.is_annotation_proxy_class(proxy_cid));
        // ...and nothing else is the proxy.
        for raw in [0u32, 1, 7, 1234] {
            let other = ClassId::new(raw);
            if other != proxy_cid {
                assert!(!shared.classes.is_annotation_proxy_class(other));
            }
        }
    }
}

#[cfg(test)]
mod statics_index_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// A lock-free static read must never observe halves of two different
    /// writes.
    ///
    /// [`StaticsIndex::get`] used to copy the whole 16-byte `Value` cell in one
    /// go, and a 16-byte access is not single-copy atomic on x86-64: a reader
    /// racing a `putstatic` of a `long` saw the high half of one value with the
    /// low half of another. `probes/StaticRaceProbe.java` reproduces it from
    /// Java in seconds — on the interpreter too, since `get_static_shared`
    /// routes through here — and for a `volatile` long that is a JLS §17.7
    /// violation, not merely the permitted non-atomic plain-long read.
    ///
    /// This is the same race in miniature: one writer flipping a slot between
    /// two patterns, four readers asserting they only ever see those two.
    ///
    /// **It does not, on its own, reproduce the historical tear.** Whether
    /// rustc emits one wide copy or two 8-byte loads depends on the inlining
    /// context, and in this test's context it already chose the narrow form:
    /// verified by reverting the fix, at which point the Java probe still tore
    /// within seconds and this test still passed. So read it as a cheap CI pin
    /// on the invariant, not as the reproduction — that is
    /// `probes/StaticRaceProbe.java`, and a future rewrite of `get` has to be
    /// re-checked against the probe, not against this.
    #[test]
    fn get_never_returns_a_torn_long() {
        const A: i64 = 0x0123_4567_89AB_CDEF;
        const B: i64 = 0x7EDC_BA98_7654_3210;

        let block = StaticsBlock::new(4);
        let index = Arc::new(StaticsIndex::new());
        index.publish(ClassId::new(3), &block);
        // The block is leaked by construction, so a raw base pointer outlives
        // both threads; `usize` to carry it across the spawn boundary.
        let base = block.base_ptr() as usize;

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let slot = base as *mut Value;
            let mut flip = false;
            while !writer_stop.load(AtomicOrdering::Relaxed) {
                flip = !flip;
                // Exactly what `set_static_shared` does to the cell, minus the
                // lock the lock-free reader does not take either.
                // SAFETY: `slot` is the leaked block's first cell.
                unsafe { *slot = Value::Long(if flip { B } else { A }) };
            }
        });

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let index = Arc::clone(&index);
                std::thread::spawn(move || {
                    for _ in 0..200_000 {
                        match index.get(ClassId::new(3), 0) {
                            Some(Value::Long(v)) => assert!(
                                v == A || v == B,
                                "torn long static read: {v:#x} is neither {A:#x} nor {B:#x}"
                            ),
                            // The writer only ever stores `Value::Long`; the
                            // initial `Value::Int(0)` is legitimate until its
                            // first store lands.
                            Some(Value::Int(0)) => {}
                            other => panic!("unexpected static cell: {other:?}"),
                        }
                    }
                })
            })
            .collect();

        for r in readers {
            r.join().expect("reader thread");
        }
        stop.store(true, AtomicOrdering::Relaxed);
        writer.join().expect("writer thread");
    }

    /// The address handed to compiled code is the POINTER CELL, so a
    /// republication (a grown or re-prepared block) is followed with no
    /// patching — that is the whole reason the backend pays an extra load.
    #[test]
    fn base_cell_addr_is_stable_across_republication() {
        let index = StaticsIndex::new();
        let first = StaticsBlock::new(2);
        index.publish(ClassId::new(9), &first);
        let cell = index
            .base_cell_addr(ClassId::new(9), 1)
            .expect("published block must resolve");

        let second = StaticsBlock::new(8);
        assert_ne!(
            first.base_ptr(),
            second.base_ptr(),
            "the two blocks must be distinct allocations for this test to mean anything"
        );
        index.publish(ClassId::new(9), &second);

        assert_eq!(
            index.base_cell_addr(ClassId::new(9), 1),
            Some(cell),
            "the pointer cell address must not move when the block does"
        );
        // And what compiled code loads through it is the NEW block.
        // SAFETY: `cell` is the address of this index's `AtomicPtr` slot.
        let observed = unsafe {
            (*(cell as *const std::sync::atomic::AtomicPtr<Value>)).load(Ordering::Acquire)
        };
        assert_eq!(observed, second.base_ptr());
    }

    /// Out of range in either direction means "keep the helper", never a baked
    /// address that would read past the block.
    #[test]
    fn base_cell_addr_refuses_unpublished_and_out_of_bounds() {
        let index = StaticsIndex::new();
        assert_eq!(index.base_cell_addr(ClassId::new(4), 0), None);
        let block = StaticsBlock::new(2);
        index.publish(ClassId::new(4), &block);
        assert!(index.base_cell_addr(ClassId::new(4), 1).is_some());
        assert_eq!(index.base_cell_addr(ClassId::new(4), 2), None);
        assert_eq!(
            index.base_cell_addr(ClassId::new(StaticsIndex::CAPACITY as u32), 0),
            None
        );
    }
}
