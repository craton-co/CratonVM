// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Resolution cache for symbolic references.
//!
//! Caches resolved constant pool references (fields, methods, and call sites) to avoid
//! re-resolving on every instruction. The cache key is `(referring class, cp index)`.
//!
//! # Round 7 audit fix (LOW #13): `ClassFile` parse is parallelizable
//!
//! `cratonvm_reader::read_class` is pure and stateless — it takes a
//! `&[u8]` and returns a fully-owned `ClassFile`. The cold-start JDK
//! bootstrap parses ~6 000 classes serially and `read_class` is
//! ~40-60 µs per class on a modern x86 core, so the serial cost adds
//! up to ~300 ms of wall time on a 16-core host that could be ~25 ms
//! parallel.
//!
//! **Why this isn't done here.** Adding `rayon` to
//! `classloading/Cargo.toml` brings in a thread-pool + a large
//! transitive graph (crossbeam, num_cpus) for one optimisation that
//! only fires once at startup. The `class_manager` registration path
//! also holds `&mut self` while inserting the parsed `ClassFile`s,
//! which serialises the back-end of the pipeline regardless. The
//! win is real but small relative to the dependency surface area.
//!
//! **TODO:** if `rayon` becomes a workspace dep for another reason,
//! revisit: add `pub fn parallel_parse(inputs: &[(Arc<str>, &[u8])])
//! -> Vec<Result<ClassFile, ClassFileError>>` in `cratonvm_reader`
//! and call it from the bootstrap-scan path in `class_manager`.
//!
//! # Round 7 audit fix (LOW #14): JFR `StringPool` vs `intern_arc` are intentionally distinct
//!
//! `cratonvm_jfr::dump::StringPool` assigns `u16` IDs for the JFR
//! binary wire format (one ID per unique string per JFR chunk; the
//! pool resets between chunks). `cratonvm_types::intern_arc` returns
//! a process-lifetime `Arc<str>` for runtime sharing across the
//! VM. Different lifetimes (chunk-scoped vs process-scoped),
//! different keying (`u16` wire ID vs identity-by-arc), different
//! consumers (JFR file writer vs constant-pool tables). No
//! unification opportunity — flagged for closure.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use crate::fx_hash::{fx_hashmap_with_capacity, FxHashMap};

use super::ClassId;
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// Resolved field / method references
// ---------------------------------------------------------------------------

/// A resolved field reference.
#[derive(Debug, Clone)]
pub struct ResolvedField {
    /// The class that declares the field.
    pub declaring_class_id: ClassId,
    /// Index into the static fields array (for static) or absolute field index (for instance).
    pub field_index: usize,
    /// Whether this is a static field.
    pub is_static: bool,
    /// Whether this field has ACC_VOLATILE, requiring memory fences on access.
    pub is_volatile: bool,
    /// Whether this field's type is a reference (descriptor starts with `L` or `[`).
    /// Used to convert zero-initialized heap slots (`Int(0)`) to `Object(None)`.
    pub is_reference: bool,
    /// First byte of the field's type descriptor (`I`/`J`/`D`/`L`/`[`/…), cached
    /// here so the getfield/getstatic/putfield/putstatic opcode handlers pick the
    /// correct category-2 (`J`/`D`) CompactValue push path WITHOUT a second
    /// `class_manager` RwLock + constant-pool walk per access (the old
    /// `resolve_field_descriptor_byte`). `0` if the descriptor is empty.
    pub desc_byte: u8,
}

/// A resolved method reference.
///
/// The string fields use `Arc<str>` so that cloning a cached entry (on every cache
/// hit in `resolve_method_ref`) is a cheap refcount bump rather than a heap
/// allocation.  All existing callers read these fields as `&str` via `Deref`, so
/// the type change is transparent outside this module.
#[derive(Debug, Clone)]
pub struct ResolvedMethod {
    /// The class that declares the method.
    pub declaring_class_id: ClassId,
    /// The resolved class name (for dispatch purposes).
    pub class_name: Arc<str>,
    /// The method name.
    pub method_name: Arc<str>,
    /// The method descriptor.
    pub method_descriptor: Arc<str>,
    /// Cached parameter count (number of JVM stack slots consumed, excluding `this`).
    pub num_params: u16,
    /// Exact native callback resolved for the symbolic owner, if one exists.
    ///
    /// This is method-resolution metadata rather than a VM-global string-keyed
    /// cache: a constant-pool method reference hashes the native registry once,
    /// then every call site sharing that resolved reference reuses the target.
    /// Resolution-cache invalidation on class redefinition/unloading drops the
    /// callback with the rest of the method metadata.
    ///
    /// **MEMOIZED NEGATIVE — read before adding lazy native registration.**
    /// `None` here does not mean "not looked up yet"; it means "the registry
    /// said no native exists", memoized for the lifetime of the enclosing
    /// [`ResolutionCache`] entry. The producer collapses the absent case with
    /// `.unwrap_or((None, None))` after a single `NativeMethodRegistry`
    /// probe, and nothing re-tries it: [`ResolutionCache::invalidate_class`]
    /// only fires on redefine / unload / synthetic upgrade, none of which are
    /// triggered by registering a native.
    ///
    /// That is sound **only** because the registry is fully populated during
    /// `vm_init` and never grows afterwards (JNI `RegisterNatives` writes a
    /// separate table that is consulted independently). The moment any lazy or
    /// deferred native-registration pass is added — the exact bug already found
    /// in the native registry's own memo — every method resolved before that
    /// pass permanently believes it has no native implementation. If you add
    /// one, either pair this field with a registry epoch that
    /// [`ResolutionCache`] hits compare against, or invalidate the resolution
    /// cache from the registration path.
    pub native_target: Option<cratonvm_native_api::NativeCallback>,
    /// Category paired with [`Self::native_target`], cached from the same
    /// registry probe so synthetic-stub selection never re-hashes the triple.
    pub native_kind: Option<cratonvm_native_api::NativeKind>,
}

// ---------------------------------------------------------------------------
// Method handle types (Phase 9)
// ---------------------------------------------------------------------------

/// JVM MethodHandle reference_kind values (JVMS 5.4.3.5, Table 5.4.3.5-A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodHandleKind {
    GetField,         // 1
    GetStatic,        // 2
    PutField,         // 3
    PutStatic,        // 4
    InvokeVirtual,    // 5
    InvokeStatic,     // 6
    InvokeSpecial,    // 7
    NewInvokeSpecial, // 8
    InvokeInterface,  // 9
}

impl MethodHandleKind {
    /// Convert a raw `reference_kind` byte (1..=9) to a `MethodHandleKind`.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::GetField),
            2 => Some(Self::GetStatic),
            3 => Some(Self::PutField),
            4 => Some(Self::PutStatic),
            5 => Some(Self::InvokeVirtual),
            6 => Some(Self::InvokeStatic),
            7 => Some(Self::InvokeSpecial),
            8 => Some(Self::NewInvokeSpecial),
            9 => Some(Self::InvokeInterface),
            _ => None,
        }
    }

    /// Convert back to the JVMS `reference_kind` byte (1..=9). Inverse of
    /// [`Self::from_tag`]. Used when serializing a lambda's implementation
    /// method handle into a `SerializedLambda`-equivalent record.
    pub fn as_tag(self) -> u8 {
        match self {
            Self::GetField => 1,
            Self::GetStatic => 2,
            Self::PutField => 3,
            Self::PutStatic => 4,
            Self::InvokeVirtual => 5,
            Self::InvokeStatic => 6,
            Self::InvokeSpecial => 7,
            Self::NewInvokeSpecial => 8,
            Self::InvokeInterface => 9,
        }
    }
}

impl fmt::Display for MethodHandleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GetField => write!(f, "REF_getField"),
            Self::GetStatic => write!(f, "REF_getStatic"),
            Self::PutField => write!(f, "REF_putField"),
            Self::PutStatic => write!(f, "REF_putStatic"),
            Self::InvokeVirtual => write!(f, "REF_invokeVirtual"),
            Self::InvokeStatic => write!(f, "REF_invokeStatic"),
            Self::InvokeSpecial => write!(f, "REF_invokeSpecial"),
            Self::NewInvokeSpecial => write!(f, "REF_newInvokeSpecial"),
            Self::InvokeInterface => write!(f, "REF_invokeInterface"),
        }
    }
}

/// A resolved method handle — lightweight VM-internal representation.
///
/// Does not correspond to a real `java.lang.invoke.MethodHandle` object on the heap.
/// Instead, the VM uses this to dispatch calls from invokedynamic / lambda proxies.
///
/// The string fields use `Arc<str>` so that cloning a `LambdaCallSite` (which
/// happens on every lambda invocation via `Arc<CachedInvokeTarget>` clone)
/// is a cheap refcount bump rather than three heap allocations. Source data
/// (`Class.name`, `Method.name`, descriptor pool) is already `Arc<str>`, so
/// producers pass refcount-bumped clones directly.
#[derive(Debug, Clone)]
pub struct MethodHandle {
    /// The kind of reference (invoke virtual, static, etc.).
    pub kind: MethodHandleKind,
    /// Class that owns the target member.
    pub class_name: Arc<str>,
    /// Member name (method name or field name).
    pub member_name: Arc<str>,
    /// Method descriptor or field descriptor.
    pub descriptor: Arc<str>,
}

// ---------------------------------------------------------------------------
// Call site types (Phase 9)
// ---------------------------------------------------------------------------

/// A label in a pattern-matching switch (SwitchBootstraps.typeSwitch).
///
/// String fields use `Arc<str>` so cloning a `ResolvedCallSite::TypeSwitch`
/// (which happens on every `CachedInvokeTarget` populate path) is a cheap
/// refcount bump rather than a heap allocation per label.
#[derive(Debug, Clone)]
pub enum SwitchLabel {
    /// Match by type (`instanceof` check). ClassId pre-resolved at bootstrap.
    Type {
        class_name: Arc<str>,
        class_id: ClassId,
    },
    /// Match by exact integer value (for constant case labels).
    Int(i32),
    /// Match by exact long value.
    Long(i64),
    /// Match by exact float value.
    Float(f32),
    /// Match by exact double value.
    Double(f64),
    /// Match by string equality.
    Str(Arc<str>),
    /// Match by primitive type class (JDK 25 primitive patterns, JEP 507).
    /// Descriptor is the JVM type descriptor: "I", "J", "F", "D", "Z", "B", "S", "C".
    PrimitiveClass(Arc<str>),
}

/// A resolved invokedynamic call site, cached after first bootstrap.
///
/// String fields use `Arc<str>` so cloning a cached entry (which happens on
/// every lambda/condy/typeswitch invocation) is a refcount bump rather than
/// a fan-out of `String` allocations. The pool of source strings
/// (`Class.name`, descriptor strings, constant-pool UTF-8 entries) is
/// already `Arc<str>` upstream.
#[derive(Debug, Clone)]
pub enum ResolvedCallSite {
    /// String concatenation (StringConcatFactory.makeConcatWithConstants).
    StringConcat {
        /// UTF-16 code units, deliberately not `Arc<str>`: a recipe can embed a
        /// lone surrogate from a folded string literal, and a Rust `str` cannot
        /// hold one -- it becomes U+FFFD. Same for the `TAG_CONST` constants.
        recipe: Arc<[u16]>,
        constant_args: Vec<Arc<[u16]>>,
        target_descriptor: Arc<str>,
    },
    /// Lambda / method reference (LambdaMetafactory.metafactory).
    Lambda(LambdaCallSite),
    /// Pattern-matching type switch (SwitchBootstraps.typeSwitch, JEP 441).
    TypeSwitch { labels: Vec<SwitchLabel> },
    /// Pattern-matching enum switch (SwitchBootstraps.enumSwitch, JEP 441).
    EnumSwitch { labels: Vec<Arc<str>> },
    /// Record ObjectMethods bootstrap (equals/hashCode/toString, JEP 395).
    RecordObjectMethod {
        /// Which method: "equals", "hashCode", or "toString"
        method: RecordMethodKind,
        /// Component names (e.g. ["x", "y"]).
        component_names: Vec<Arc<str>>,
        /// Field indices for each component.
        field_indices: Vec<usize>,
        /// Field descriptors for each component (e.g. ["I", "Ljava/lang/String;"]).
        field_descriptors: Vec<Arc<str>>,
    },
}

/// Which record ObjectMethods bootstrap method is being called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordMethodKind {
    Equals,
    HashCode,
    ToString,
}

/// Lambda-specific call site data produced by LambdaMetafactory bootstrap.
///
/// String fields use `Arc<str>` so a `LambdaCallSite` clone — which happens
/// every time the cached `Arc<CachedInvokeTarget>` is rebound or the lambda
/// proxy is reconstructed — costs four refcount bumps rather than four heap
/// allocations.
#[derive(Debug, Clone)]
pub struct LambdaCallSite {
    /// Functional interface class name (e.g. "java/util/function/Consumer").
    pub functional_interface: Arc<str>,
    /// SAM (Single Abstract Method) name (e.g. "accept").
    pub sam_method_name: Arc<str>,
    /// SAM erased descriptor (e.g. "(Ljava/lang/Object;)V").
    pub sam_descriptor: Arc<str>,
    /// The actual implementation method handle.
    pub impl_handle: MethodHandle,
    /// Instantiated method type descriptor (concrete types after generics are resolved).
    pub instantiated_descriptor: Arc<str>,
    /// Type chars for captured values (from the invokedynamic factory descriptor params).
    pub capture_types: Vec<char>,
    /// Synthetic proxy ClassId allocated for this lambda form.
    pub proxy_class_id: ClassId,
    /// `true` when this lambda's `invokedynamic` bootstrap went through
    /// `LambdaMetafactory.altMetafactory` with `FLAG_SERIALIZABLE` (0x1) set --
    /// i.e. the source target type was `Serializable`-intersected, as in
    /// `(Comparator<T> & Serializable)` (which is how every `Comparator.comparing*`
    /// factory in the JDK is written).
    ///
    /// This is ONLY the explicit flag. The JDK's full rule
    /// (`AbstractValidatingLambdaMetafactory`) is `FLAG_SERIALIZABLE ||
    /// Serializable.isAssignableFrom(functionalInterface)`; the inheritance half
    /// is evaluated lazily because the functional interface is not necessarily
    /// loaded yet at bootstrap time. `SharedVm::lambda_proxy_serializability` is
    /// the single owner of the combined rule -- do not re-derive it elsewhere.
    ///
    /// `false` for every proxy built outside the `invokedynamic` bootstrap that
    /// has no flags to read (unit fixtures, deserialization); those still come
    /// out serializable through the inheritance half when they should.
    pub serializable_flag: bool,
    /// Loader-resolved `ClassId` of `functional_interface`, captured at
    /// bootstrap time through the HOST class's defining loader. Two loaders
    /// can define the same interface name (e.g. Spring's
    /// `@CompileWithForkedClassLoader` fork re-defines the whole framework);
    /// resolving the name globally at dispatch time picks an arbitrary copy
    /// and runs default methods in the wrong loader's context. `None` only
    /// for construction sites with no loader context (reflective
    /// metafactory, deserialization); consumers must then fall back to
    /// name-based resolution.
    pub functional_interface_id: Option<ClassId>,
}

// ---------------------------------------------------------------------------
// Resolution cache
// ---------------------------------------------------------------------------

/// Cache key: (referring class, constant pool index).
type ResolutionKey = (ClassId, u16);

/// Per-map FIFO cap for [`ResolutionCache`].
///
/// The cache is keyed on `(referring class, cp index)` so its natural
/// size is bounded by the total CP-reference count of the loaded
/// program — but a long-running JVM that loads and unloads many
/// classes (agents, redefine churn, fileless classloaders) can grow
/// each map without bound. Mirrors the FIFO cap on `class_bytes_cache`
/// (`class_manager.rs`) and `CanonicalizeCache` (`class_path.rs`).
/// Generous enough that real programs never evict — eviction only
/// kicks in for pathological reference sets.
const RESOLUTION_CACHE_CAP: usize = 1 << 16;

/// Insert `(key, value)` into `map` honouring the FIFO `order` tracker
/// and `RESOLUTION_CACHE_CAP`. If `key` already exists the value is
/// overwritten in place and the FIFO position is left unchanged
/// (avoiding a linear `VecDeque` scan — resolution is idempotent, so a
/// repeat insert writes an identical value). Mirrors
/// [`CanonicalizeCache::insert`] in `class_path.rs`.
fn resolution_cache_insert<V>(
    map: &mut FxHashMap<ResolutionKey, V>,
    order: &mut VecDeque<ResolutionKey>,
    key: ResolutionKey,
    value: V,
) {
    if map.contains_key(&key) {
        map.insert(key, value);
        return;
    }
    if map.len() >= RESOLUTION_CACHE_CAP {
        if let Some(oldest) = order.pop_front() {
            map.remove(&oldest);
        }
    }
    order.push_back(key);
    map.insert(key, value);
}

/// Caches resolved symbolic references from the constant pool.
///
/// Avoids re-resolving the same field/method/call-site reference every time the
/// instruction is executed. Per JVM spec, resolution is idempotent, so
/// caching the result is safe.
#[derive(Debug)]
pub struct ResolutionCache {
    fields: FxHashMap<ResolutionKey, ResolvedField>,
    methods: FxHashMap<ResolutionKey, ResolvedMethod>,
    call_sites: FxHashMap<ResolutionKey, ResolvedCallSite>,
    /// Cached CONSTANT_Dynamic values (condy, JEP 309).
    condy: FxHashMap<ResolutionKey, Value>,
    /// Insertion-order trackers for FIFO eviction, one per map. The
    /// front is the oldest entry; the back is the most recent. Kept in
    /// sync with each map: every fresh insert pushes to the back; every
    /// eviction pops from the front. Bounded by `RESOLUTION_CACHE_CAP`.
    fields_order: VecDeque<ResolutionKey>,
    methods_order: VecDeque<ResolutionKey>,
    call_sites_order: VecDeque<ResolutionKey>,
    condy_order: VecDeque<ResolutionKey>,
}

impl ResolutionCache {
    /// Create an empty resolution cache.
    pub fn new() -> Self {
        Self {
            fields: fx_hashmap_with_capacity(64),
            methods: fx_hashmap_with_capacity(64),
            call_sites: fx_hashmap_with_capacity(16),
            condy: fx_hashmap_with_capacity(16),
            fields_order: VecDeque::with_capacity(64),
            methods_order: VecDeque::with_capacity(64),
            call_sites_order: VecDeque::with_capacity(16),
            condy_order: VecDeque::with_capacity(16),
        }
    }

    /// Look up a cached field resolution.
    pub fn get_field(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedField> {
        self.fields.get(&(class_id, cp_index))
    }

    /// Cache a resolved field.
    pub fn put_field(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedField) {
        resolution_cache_insert(
            &mut self.fields,
            &mut self.fields_order,
            (class_id, cp_index),
            resolved,
        );
    }

    /// Look up a cached method resolution.
    pub fn get_method(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedMethod> {
        self.methods.get(&(class_id, cp_index))
    }

    /// Cache a resolved method.
    pub fn put_method(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedMethod) {
        resolution_cache_insert(
            &mut self.methods,
            &mut self.methods_order,
            (class_id, cp_index),
            resolved,
        );
    }

    /// Look up a cached call site (invokedynamic).
    pub fn get_call_site(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedCallSite> {
        self.call_sites.get(&(class_id, cp_index))
    }

    /// Cache a resolved call site.
    pub fn put_call_site(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedCallSite) {
        resolution_cache_insert(
            &mut self.call_sites,
            &mut self.call_sites_order,
            (class_id, cp_index),
            resolved,
        );
    }

    /// The number of cached field resolutions.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The number of cached method resolutions.
    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    /// The number of cached call site resolutions.
    pub fn call_site_count(&self) -> usize {
        self.call_sites.len()
    }

    /// Look up a cached CONSTANT_Dynamic value.
    pub fn get_condy(&self, class_id: ClassId, cp_index: u16) -> Option<&Value> {
        self.condy.get(&(class_id, cp_index))
    }

    /// Cache a resolved CONSTANT_Dynamic value.
    pub fn put_condy(&mut self, class_id: ClassId, cp_index: u16, value: Value) {
        resolution_cache_insert(
            &mut self.condy,
            &mut self.condy_order,
            (class_id, cp_index),
            value,
        );
    }

    /// Scan cached CONSTANT_Dynamic values for GC roots.
    pub fn scan_condy_roots(&self, roots: &mut Vec<cratonvm_types::ObjectRef>) {
        for val in self.condy.values() {
            if let Value::Object(Some(obj_ref)) = val {
                roots.push(*obj_ref);
            }
        }
    }

    /// Visit condy roots together with the referring class that owns the
    /// constant-pool cache entry. Loader unloading uses this to make the root
    /// conditional on defining-loader liveness.
    pub fn for_each_condy_root(&self, mut visit: impl FnMut(ClassId, cratonvm_types::ObjectRef)) {
        for (&(class_id, _), value) in &self.condy {
            if let Value::Object(Some(object)) = value {
                visit(class_id, *object);
            }
        }
    }

    /// Update cached CONSTANT_Dynamic ObjectRefs after GC relocation.
    pub fn update_condy_refs(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        for val in self.condy.values_mut() {
            if let Value::Object(Some(ref mut obj_ref)) = val {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }

    /// Clear all cached resolutions. Used to invalidate stale entries after
    /// a partially-failed class initialization (e.g. System.initPhase1).
    pub fn clear(&mut self) {
        self.fields.clear();
        self.methods.clear();
        self.call_sites.clear();
        self.condy.clear();
        // Keep the FIFO trackers in sync with their maps.
        self.fields_order.clear();
        self.methods_order.clear();
        self.call_sites_order.clear();
        self.condy_order.clear();
    }

    /// Round 4 audit fix (CRIT): drop every cached resolution that
    /// references `class_id`, in any of the four caches.
    ///
    /// Called from the JVMTI `RedefineClasses` path (via the
    /// `ResolutionInvalidateHook` installed at VM init) when the
    /// bytecode + constant pool of a class are replaced in place. The
    /// (referring-class, cp-index) keys captured by these caches were
    /// resolved against the **old** constant pool — after a redefine
    /// the same cp index in the new pool may refer to a different
    /// field/method/call-site, and the cached `ResolvedField` /
    /// `ResolvedMethod` / `ResolvedCallSite` / condy value would
    /// silently return a stale resolution otherwise.
    ///
    /// Two-pronged invalidation:
    ///   1. drop every entry whose **key** matches `class_id` (the
    ///      caller side — every cp-index that was looked up from
    ///      bytecode that just got swapped out);
    ///   2. drop every entry whose **resolved declaring class** matches
    ///      `class_id` (the callee side — cached resolutions held by
    ///      other classes that pointed at a now-redefined method/field
    ///      body; needed so redefine sees through proxy/intermediate
    ///      classes that resolved methods *into* the redefined class).
    ///
    /// `ResolvedCallSite` and condy values are evicted on the key match
    /// only — their inner contents don't expose a `declaring_class_id`
    /// reachable from the cache.
    ///
    /// AUDIT (2026-07-26): prong 2 is **live for `fields` but dead for
    /// `methods`.** `ResolvedField`'s producer stores the true declaring class
    /// (found by the superclass walk), so a field resolved *into* a redefined
    /// superclass is correctly evicted. `ResolvedMethod`'s only producer
    /// (`interpreter.rs`, `resolve_method_metadata`) sets
    /// `declaring_class_id: current_class_id` — i.e. the **referring** class,
    /// identical to `key_class` — so the second conjunct can never eliminate an
    /// entry the first did not. The "sees through proxy/intermediate classes"
    /// behaviour described above therefore does not occur for methods. Fixing
    /// it requires a change in the producer (not this crate); see
    /// `arch-2026-07-26/classloading-verify-and-resolve.md`,
    /// "cross-owner requests". Left as-is deliberately rather than papered
    /// over here, because a same-crate change cannot make the field carry
    /// information the producer never wrote.
    pub fn invalidate_class(&mut self, class_id: ClassId) {
        // Fields: drop by key OR by resolved declaring class.
        self.fields.retain(|(key_class, _), resolved| {
            *key_class != class_id && resolved.declaring_class_id != class_id
        });
        // Methods: same two-pronged check. NB: prong 2 is currently a no-op —
        // see the doc comment above.
        self.methods.retain(|(key_class, _), resolved| {
            *key_class != class_id && resolved.declaring_class_id != class_id
        });
        // Call sites + condy: key match only. (Their resolved values
        // carry no reachable declaring-class link.)
        self.call_sites
            .retain(|(key_class, _), _| *key_class != class_id);
        self.condy
            .retain(|(key_class, _), _| *key_class != class_id);
        // Rebuild the FIFO trackers so they stay in sync with the maps:
        // keep only keys still present, preserving insertion order.
        self.fields_order
            .retain(|key| self.fields.contains_key(key));
        self.methods_order
            .retain(|key| self.methods.contains_key(key));
        self.call_sites_order
            .retain(|key| self.call_sites.contains_key(key));
        self.condy_order.retain(|key| self.condy.contains_key(key));
    }
}

impl Default for ResolutionCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// LinkResolver — reflective (class, name, descriptor) lookup cache
// ---------------------------------------------------------------------------

/// A resolved member identifier produced by [`LinkResolver`].
///
/// Reflective lookups (`Class.getDeclaredMethod`,
/// `Class.getDeclaredField`, `Class.getMethod`, `Class.getField`, JNI
/// `GetMethodID`/`GetFieldID`, MethodHandles.Lookup, JVMTI agents,
/// `Unsafe.objectFieldOffset`, Spring's massive reflection batches) all
/// walk the class hierarchy in a linear `find_method` / `find_field`
/// scan that the per-CP-index [`ResolutionCache`] never observes — the
/// CP cache is keyed on `(referring class, cp index)` whereas
/// reflection passes raw `&str` and the same `(declaring class, name,
/// descriptor)` triple recurs thousands of times across the Spring
/// startup. This entry is the deduplicated answer for one such triple.
#[derive(Debug, Clone)]
pub enum ResolvedMember {
    /// A method was found at `declaring_class_id`, position `index`
    /// inside its `methods` vec. Callers re-fetch the
    /// [`cratonvm_reader::ClassFileMethod`] via the class store so the
    /// cache stays small (no full snapshot).
    Method {
        declaring_class_id: ClassId,
        index: u32,
    },
    /// A field was found at `declaring_class_id`. `absolute_index` is
    /// the value [`crate::Class::find_own_field`] returned (i.e. the
    /// index into the static slot array for static fields, or
    /// `first_field_index + offset` for instance fields).
    Field {
        declaring_class_id: ClassId,
        absolute_index: u32,
        is_static: bool,
    },
    /// The lookup walked the hierarchy and turned up nothing — cached
    /// so a tight loop of "does class X declare method Y?" probes
    /// (Spring's `AnnotationUtils.findAnnotation` is the canonical hot
    /// spot) doesn't re-walk every time.
    NotFound,
}

/// Shared cache for reflective `(class, name, descriptor)` lookups.
///
/// Round 7 audit fix (HIGH #11) / round-5 carryover: the per-CP-index
/// [`ResolutionCache`] only covers bytecode references that go through
/// a constant pool. Reflective callers (`Class.getDeclaredMethod`,
/// JNI `GetMethodID`, Spring's `ReflectionUtils.findMethod`, Hibernate
/// proxy-class scanners, ByteBuddy's `getDeclaredMethods()` walk) pay
/// the linear `find_method_recursive` / `find_field_recursive` cost
/// on every probe. On a Spring Boot cold start the same
/// `(Iterable.class, "iterator", "()Ljava/util/Iterator;")` triple is
/// resolved 20-50k times. This cache keys on
/// `(ClassId, Arc<str>, Arc<str>)` and de-dupes the answer.
///
/// Concurrency: writes are rare (population happens at most once per
/// distinct triple) and reads dominate every-time, so a
/// `parking_lot::RwLock<FxHashMap>` is the right fit: concurrent
/// readers never block each other, and the populate path takes the
/// write lock for a single insert. (`FxHashMap` is safe here because
/// the keys are method-name / descriptor `Arc<str>`s already interned
/// upstream by the constant pool — not attacker-controlled.)
///
/// `Arc<str>` keys allow producers to clone the constant-pool-interned
/// strings (refcount bump, no allocation) instead of copying the bytes
/// — matches the round-3 `Arc<str>` CP work.
/// Maximum number of distinct `(ClassId, name, descriptor)` triples the
/// [`LinkResolver`] cache retains before the CLOCK sweep evicts cold
/// entries (PERF: bound the cache).
///
/// Sizing rationale: the cache only holds *reflective* resolutions —
/// the (class, name, descriptor) triples that go through
/// `Class.getDeclaredMethod`, JNI `Get{Method,Field}ID`, Spring's
/// `ReflectionUtils.findMethod`, ByteBuddy/Hibernate proxy scans, etc.
/// A large Spring Boot / Wildfly cold start touches on the order of a
/// few-thousand distinct hot triples (the same ~20-50k probes collapse
/// onto a much smaller working set). 16 384 entries comfortably covers
/// that working set so the steady-state hit rate is unchanged, while
/// capping worst-case memory at ~128k * (tuple + value) ≈ ~16 MB even
/// for pathological apps that reflect over tens of thousands of
/// distinct members. Each entry is small (two `Arc<str>` clones — shared
/// refcount bumps, not byte copies — plus a `ClassId` and a tagged enum).
///
/// PERF (2026-06, Tomcat DoHead profiling): a large server resolves **tens
/// of thousands** of distinct (class, member, descriptor) sites in a single
/// start/stop — well over the old 16k cap. The cache then sat pinned AT the
/// cap, so every insert ran the O(n) CLOCK eviction sweep: `LinkResolver::
/// insert` (the sweep) measured **72% of total CPU** across a repeated
/// Tomcat start/stop test, because stable JDK/Tomcat resolutions were
/// evicted and re-resolved on every iteration. Sizing the cap to hold that
/// working set lets it reach steady state (no eviction), turning each later
/// iteration's resolutions into cache hits.
const CACHE_CAP: usize = 128 * 1024;

/// Number of entries to *try* to reclaim each time the cap is hit. We
/// sweep in a batch rather than evicting a single entry per insert so
/// the amortised cost of the CLOCK pass is spread across many inserts
/// (one O(n) sweep buys ~`CACHE_EVICT_BATCH` cheap inserts before the
/// next sweep). 1/8 of the cap keeps the table comfortably below the cap
/// without thrashing.
const CACHE_EVICT_BATCH: usize = CACHE_CAP / 8;

/// One stored cache entry: the resolved member plus a CLOCK
/// "recently used" reference bit.
///
/// PERF: the bit is an `AtomicBool` so a cache **hit** can mark the
/// entry as recently-used through the shared read guard (`Relaxed`
/// store) without upgrading to the write lock — the read-lock fast path
/// stays a hash + key compare + one relaxed store. The CLOCK eviction
/// sweep (write side, runs only when the cap is reached) gives every
/// marked entry a second chance: it clears set bits and evicts entries
/// whose bit is already clear. This approximates LRU at O(1) amortised
/// cost without per-hit LRU bookkeeping.
struct CachedEntry {
    value: ResolvedMember,
    /// CLOCK reference bit. Set on every hit (relaxed), cleared by the
    /// eviction sweep. `Relaxed` is sufficient: the bit is a pure
    /// eviction heuristic and never gates correctness — a missed or
    /// stale read at worst evicts a still-warm entry, which only forces
    /// a recompute on the next probe (semantically identical to a miss).
    used: AtomicBool,
}

impl CachedEntry {
    #[inline]
    fn new(value: ResolvedMember) -> Self {
        // Born with the bit SET so a freshly-inserted entry survives the
        // immediately-following sweep (it is by definition the most
        // recently used).
        Self {
            value,
            used: AtomicBool::new(true),
        }
    }
}

pub struct LinkResolver {
    /// hashbrown `HashMap` (not std) so we get stable `raw_entry_mut`
    /// for borrow-free probes. Round 8 audit fix (CRIT): the old
    /// `FxHashMap<(ClassId, Arc<str>, Arc<str>), _>` keyed by owned
    /// `Arc<str>` forced every probe (even a cache **hit**) to bump
    /// two Arc refcounts to build the lookup tuple — defeating the
    /// dedupe win on the hot Spring `findMethod` loop.
    ///
    /// PERF (bounded cache): the value is a [`CachedEntry`] carrying a
    /// CLOCK reference bit so the table can be capped at [`CACHE_CAP`]
    /// without per-hit LRU bookkeeping. See [`Self::evict_clock`].
    cache: parking_lot::RwLock<
        hashbrown::HashMap<
            (ClassId, Arc<str>, Arc<str>),
            CachedEntry,
            crate::fx_hash::FxBuildHasher,
        >,
    >,
}

impl LinkResolver {
    /// Build an empty resolver.
    pub fn new() -> Self {
        Self {
            cache: parking_lot::RwLock::new(hashbrown::HashMap::with_capacity_and_hasher(
                256,
                Default::default(),
            )),
        }
    }

    /// Compute the hash of a `(ClassId, &str, &str)` triple against the
    /// cache's `BuildHasher`. The hash MUST agree with the hash of the
    /// owned `(ClassId, Arc<str>, Arc<str>)` tuple — which it does
    /// because `Arc<str>` derefs to `str` and `Hash` for `Arc<T>` /
    /// `str` walks the bytes the same way the tuple impl does.
    #[inline]
    fn hash_key(
        hasher: &crate::fx_hash::FxBuildHasher,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> u64 {
        use std::hash::{BuildHasher, Hash, Hasher};
        let mut h = hasher.build_hasher();
        // Tuple `Hash` impl walks each field in order; replicate that
        // here so the borrowed and owned forms produce the same hash.
        class_id.hash(&mut h);
        name.hash(&mut h);
        descriptor.hash(&mut h);
        h.finish()
    }

    /// Probe the cache. Returns `None` on cold miss; callers must then
    /// do the full hierarchy walk and call [`Self::insert`] with the
    /// result (including `ResolvedMember::NotFound` so the next probe
    /// short-circuits).
    ///
    /// Round 8 audit fix (CRIT): probes via `(ClassId, &str, &str)` using
    /// hashbrown's `raw_entry` API so a cache hit costs a hash + a key
    /// comparison — no Arc clones until we know we'll write into the
    /// cache.
    pub fn get(&self, class_id: ClassId, name: &str, descriptor: &str) -> Option<ResolvedMember> {
        let guard = self.cache.read();
        let hash = Self::hash_key(guard.hasher(), class_id, name, descriptor);
        guard
            .raw_entry()
            .from_hash(hash, |(k_cid, k_name, k_desc)| {
                *k_cid == class_id && k_name.as_ref() == name && k_desc.as_ref() == descriptor
            })
            .map(|(_, entry)| {
                // PERF (CLOCK): mark recently-used through the shared read
                // guard with a relaxed store — no write-lock upgrade, so
                // the hit path stays a hash + key compare + one atomic
                // store. Skip the store if already set to avoid needless
                // cache-line dirtying under read-heavy contention.
                if !entry.used.load(Ordering::Relaxed) {
                    entry.used.store(true, Ordering::Relaxed);
                }
                entry.value.clone()
            })
    }

    /// Populate (or overwrite) a cache entry. Takes the write lock for
    /// a single insert. Callers should hold no other locks while
    /// calling — the read side runs without yielding.
    pub fn insert(
        &self,
        class_id: ClassId,
        name: Arc<str>,
        descriptor: Arc<str>,
        resolved: ResolvedMember,
    ) {
        let mut guard = self.cache.write();
        // PERF (bounded cache): keep the table under the cap. We sweep
        // *before* inserting so the fresh entry (which `CachedEntry::new`
        // marks used) is never the one evicted, and a single insert that
        // crosses the threshold can't leave us permanently over-cap.
        if guard.len() >= CACHE_CAP {
            Self::evict_clock(&mut guard);
        }
        guard.insert((class_id, name, descriptor), CachedEntry::new(resolved));
    }

    /// CLOCK / second-chance eviction sweep (PERF: bound the cache).
    ///
    /// Runs under the write lock when the table hits [`CACHE_CAP`].
    /// First pass: drop entries whose reference bit is clear (cold since
    /// the last sweep) and clear the bit on the survivors (their "second
    /// chance"). This approximates LRU without per-hit ordering
    /// bookkeeping. If a single pass didn't reclaim enough — possible
    /// when almost everything was touched since the last sweep — a
    /// second `retain` drops now-clear entries until we're back under
    /// the target. Correctness is unaffected: eviction only removes
    /// cached answers, and a later probe simply re-walks and re-inserts
    /// (identical to a cold miss).
    fn evict_clock(
        map: &mut hashbrown::HashMap<
            (ClassId, Arc<str>, Arc<str>),
            CachedEntry,
            crate::fx_hash::FxBuildHasher,
        >,
    ) {
        let target = CACHE_CAP.saturating_sub(CACHE_EVICT_BATCH);
        // Pass 1: second-chance. Evict clear-bit entries; clear the bit
        // on the rest.
        map.retain(|_, entry| {
            // `get_mut`-style access: we hold `&mut entry`, so a plain
            // load/store is fine (no other thread can race us under the
            // write lock).
            if *entry.used.get_mut() {
                *entry.used.get_mut() = false;
                true
            } else {
                false
            }
        });
        // Pass 2 (rare): everything survived pass 1 because all bits were
        // set. Their bits are now clear, so a second sweep reclaims down
        // to the target.
        if map.len() > target {
            let mut to_drop = map.len() - target;
            map.retain(|_, entry| {
                if to_drop > 0 && !*entry.used.get_mut() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Round 8 audit fix (CRIT #2): caller-friendly "get-or-compute"
    /// wrapper. Caller passes `(class_id, &str, &str)` plus a closure
    /// that performs the hierarchy walk on cold miss. The closure
    /// returns the `(declaring class, name_arc, descriptor_arc,
    /// member)` tuple so the cache can store the canonical interned
    /// strings (the closure typically already has them via the
    /// reader's constant-pool path).
    ///
    /// Bug 2 wiring guide for native reflective callers: replace the
    /// raw `find_method_recursive(...)` / `find_field_recursive(...)`
    /// call with `vm.link_resolver().resolve_or_compute(class_id,
    /// name, descriptor, || { /* existing walk; return
    /// (name_arc, desc_arc, ResolvedMember::...) */ })`. Subsequent
    /// hits return immediately with a single hash + key compare and
    /// zero Arc clones.
    ///
    /// Round 9 audit fix (vm LOW #11): under contention two threads
    /// that both miss the cache, both run the (potentially expensive)
    /// hierarchy walk, and both arrive at `insert()` — the loser's
    /// walk result is wasted *and* its `insert()` clobbers the
    /// winner's entry with a freshly-allocated duplicate. After the
    /// expensive compute, we re-acquire the write lock and use a
    /// raw-entry probe to detect a race-winner; if one is already
    /// present we discard our computation and clone the existing
    /// value. Sub-microsecond extra work on the hit path; eliminates
    /// the duplicate-insert + wasted-walk on the loser path.
    pub fn resolve_or_compute<F>(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        compute: F,
    ) -> ResolvedMember
    where
        F: FnOnce() -> (Arc<str>, Arc<str>, ResolvedMember),
    {
        if let Some(hit) = self.get(class_id, name, descriptor) {
            return hit;
        }
        let (name_arc, desc_arc, resolved) = compute();
        // Race-loser check: re-probe under the write lock. If another
        // thread populated the same key while we were computing,
        // discard our result and return theirs.
        let mut guard = self.cache.write();
        let hash = Self::hash_key(guard.hasher(), class_id, &name_arc, &desc_arc);
        let race_winner = guard
            .raw_entry()
            .from_hash(hash, |(k_cid, k_name, k_desc)| {
                *k_cid == class_id
                    && k_name.as_ref() == name_arc.as_ref()
                    && k_desc.as_ref() == desc_arc.as_ref()
            })
            .map(|(_, entry)| {
                // Mark the race-winner used too (we just "hit" it).
                entry.used.store(true, Ordering::Relaxed);
                entry.value.clone()
            });
        if let Some(existing) = race_winner {
            return existing;
        }
        // PERF (bounded cache): same cap enforcement as `insert`.
        if guard.len() >= CACHE_CAP {
            Self::evict_clock(&mut guard);
        }
        guard.insert(
            (class_id, name_arc, desc_arc),
            CachedEntry::new(resolved.clone()),
        );
        resolved
    }

    /// Round 8 wave-3 (HIGH from round-8 classloading-reader review):
    /// one-shot reflective resolution helper that takes a `ClassStore`
    /// and does the cache probe + hierarchy walk + cache insert in a
    /// single call. Lets non-VM callers (`classloading` unit tests,
    /// `verifier`, future `classloading`-internal reflective paths)
    /// participate in the LinkResolver dedupe without re-implementing
    /// the `find_method_recursive` / `find_field_recursive` orchestration.
    ///
    /// The JNI path in `vm::native::jni` still uses `resolve_or_compute`
    /// directly because it needs to interleave with `with_shared_vm` and
    /// `find_method_recursive` returns method-index (not in
    /// `ResolvedMember::Method`'s shape) so the closure does a slightly
    /// different post-walk to compute the index. This helper covers the
    /// common case where the caller wants the canonical resolved member.
    ///
    /// Returns `ResolvedMember::NotFound` (cached) on miss so a tight
    /// loop of "does class X declare method Y?" probes doesn't re-walk.
    pub fn resolve_method_in_store(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        store: &crate::class::ClassStore,
    ) -> ResolvedMember {
        self.resolve_or_compute(class_id, name, descriptor, || {
            let resolved =
                match crate::class::find_method_recursive(class_id, name, descriptor, store) {
                    Some((_, declaring)) => {
                        // Locate the position inside the declaring class's
                        // methods vec so callers can re-fetch the
                        // `ClassFileMethod` via the store.
                        match store.get(declaring) {
                            Some(decl) => match decl
                                .methods
                                .iter()
                                .position(|m| &*m.name == name && &*m.descriptor == descriptor)
                            {
                                Some(idx) => ResolvedMember::Method {
                                    declaring_class_id: declaring,
                                    index: idx as u32,
                                },
                                // Defensive: walk succeeded but position
                                // lookup failed (would only happen if the
                                // store mutated between the two calls,
                                // which a single `&store` borrow prevents).
                                // Treat as NotFound so we don't return a
                                // bogus index.
                                None => ResolvedMember::NotFound,
                            },
                            None => ResolvedMember::NotFound,
                        }
                    }
                    None => ResolvedMember::NotFound,
                };
            (
                cratonvm_types::intern_arc(name),
                cratonvm_types::intern_arc(descriptor),
                resolved,
            )
        })
    }

    /// Field-resolution sibling of [`Self::resolve_method_in_store`].
    ///
    /// # The empty descriptor in the cache key is a NAME-ONLY key
    ///
    /// This used to justify itself with "JVMS `(class, name)` is already unique
    /// within a class (multiple fields with the same name are illegal)". That is
    /// not what JVMS says. §4.5 forbids two fields in one class file sharing a
    /// name **and** descriptor; sharing a name alone is legal, and javac is not
    /// the only thing that emits class files. §5.4.3.2 accordingly resolves a
    /// fieldref by name and descriptor.
    ///
    /// That sentence is the belief the whole gap grew from: it is why
    /// `find_own_field` and `find_field_recursive` key on the name, and why a
    /// compiled field access could take its slot index from one field and its
    /// type tag from another.
    ///
    /// This entry point still keys on the name alone, and its callers pass
    /// names they own. A caller that has a descriptor — the `getfield` /
    /// `putfield` resolution path, and JNI `GetFieldID` — must use
    /// `find_field_recursive_by_descriptor` (or `MemberResolver::locate_field`
    /// with `Some(descriptor)`) instead.
    pub fn resolve_field_in_store(
        &self,
        class_id: ClassId,
        name: &str,
        store: &crate::class::ClassStore,
    ) -> ResolvedMember {
        self.resolve_or_compute(class_id, name, "", || {
            let resolved = match crate::class::find_field_recursive(class_id, name, store) {
                Some((field_index, field, declaring)) => ResolvedMember::Field {
                    declaring_class_id: declaring,
                    absolute_index: field_index as u32,
                    is_static: field
                        .access_flags
                        .contains(cratonvm_reader::class_access_flags::FieldAccessFlags::STATIC),
                },
                None => ResolvedMember::NotFound,
            };
            (
                cratonvm_types::intern_arc(name),
                cratonvm_types::intern_arc(""),
                resolved,
            )
        })
    }

    /// Drop every cached entry whose key class matches `class_id`, or
    /// whose resolved declaring class matches, **or that is a negative**.
    /// Mirrors [`ResolutionCache::invalidate_class`] so a JEP 109 redefine
    /// invalidates this cache in lockstep.
    ///
    /// BUGFIX (memoized-negative, 2026-07-26): [`ResolvedMember::NotFound`]
    /// used to be *retained* here unless it happened to be keyed on the
    /// changed class. That is unsound, because a negative is a statement about
    /// a whole hierarchy **walk**, not about one class. `find_method_recursive`
    /// / `find_field_recursive` climb the superclass and superinterface chain,
    /// so a `NotFound` cached under `(Sub, "m", "()V")` also asserts that
    /// `Base` does not declare `m` — and nothing in the key records that the
    /// walk passed through `Base`.
    ///
    /// The live failure that produces: a JNI `GetMethodID(Sub, "m", "()V")`
    /// against a synthetic-stub `Base` caches `NotFound`; `ClassManager::
    /// upgrade_synthetic_class` later swaps in the real `Base` that *does*
    /// declare `m` and fires the invalidate hook for `Base`; the entry is keyed
    /// on `Sub`, so the old `retain` kept it; every later `GetMethodID` returns
    /// a null `jmethodID` and the caller sees `NoSuchMethodError` for a method
    /// that now exists. There was no escape short of `clear()` (no production
    /// caller) or the CLOCK sweep at 128 K entries. The same shape applies to
    /// `RedefineClasses` adding a member to a superclass.
    ///
    /// Since `invalidate_class` has no `ClassStore` it cannot ask whether
    /// `class_id` is in the key class's ancestry, so it drops **every**
    /// negative. That is the conservative direction: a dropped-but-still-valid
    /// negative costs one re-walk and is semantically identical to a miss,
    /// whereas a retained-but-stale negative is a permanently wrong answer.
    /// Invalidation is rare (redefine / unload / synthetic upgrade) and the
    /// pass is already an O(n) `retain`, so this adds no new asymptotic cost.
    pub fn invalidate_class(&self, class_id: ClassId) {
        let mut guard = self.cache.write();
        guard.retain(|(key_class, _, _), entry| {
            if *key_class == class_id {
                return false;
            }
            match &entry.value {
                ResolvedMember::Method {
                    declaring_class_id, ..
                }
                | ResolvedMember::Field {
                    declaring_class_id, ..
                } => *declaring_class_id != class_id,
                // A negative describes a hierarchy walk whose path this key
                // does not record. Re-derive it rather than trust it.
                ResolvedMember::NotFound => false,
            }
        });
    }

    /// Drop every cached entry (e.g. on JVM shutdown / test teardown).
    pub fn clear(&self) {
        self.cache.write().clear();
    }

    /// Current cache size — exposed for diagnostics / tests.
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// True if the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }
}

impl Default for LinkResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LinkResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid taking the read lock under `Debug` (cheap diagnostic
        // path; the size is the only useful datum without dumping every
        // key, which would be enormous).
        f.debug_struct("LinkResolver")
            .field("entries", &self.cache.read().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Invoke cache — stores everything needed to create a Frame directly
// ---------------------------------------------------------------------------

use cratonvm_native_api::{NativeCallback, NativeKind, NativeMethodId};

// Re-export from jit-api crate — the canonical definition lives there now.
pub use cratonvm_jit_api::CachedBytecodeMethod;

/// WP2.4-F1 — JEP 109 redefinition staleness gate for an invoke-cache entry.
///
/// Each [`CachedInvokeTarget`] carries one of these so the interpreter can
/// detect, in O(1) on every cache hit, that the bound class has been
/// redefined since the entry was populated. The fix shape:
///
/// 1. At populate time we call
///    [`crate::ClassManager::class_redefine_generation_handle`] for the
///    *declaring* class (where the bytecode actually lives) and snapshot
///    the current value into `generation`.
/// 2. At every cache hit the dispatcher compares
///    `counter.load(Acquire) == generation`. A mismatch means
///    `redefine_class` ran and bumped `redefine_generations` with `Release`
///    (see `class_manager::redefine_class` step 7) — the cached
///    `Arc<CachedBytecodeMethod>` still points at the *old* code/exception
///    table and must be evicted before dispatch.
///
/// The `Arc<AtomicU32>` is shared with [`crate::ClassManager`] so we don't
/// need to reborrow the manager on the hot path — the counter outlives any
/// borrow.  Cloning the gate is two refcount bumps (the `Arc` is the only
/// allocation; `generation` is a `u32`).
#[derive(Clone)]
pub struct RedefineGate {
    /// Snapshot of the redefine generation at populate time.
    pub generation: u32,
    /// Live handle to the [`ClassManager::redefine_generations`] counter for
    /// the declaring class.  Loaded with `Acquire` ordering on each hit;
    /// pairs with the `Release` `fetch_add` in `redefine_class`.
    pub counter: Arc<AtomicU32>,
}

impl RedefineGate {
    /// Build a gate that snapshots the counter's *current* value.
    #[inline]
    pub fn snapshot(counter: Arc<AtomicU32>) -> Self {
        let generation = counter.load(Ordering::Acquire);
        Self {
            generation,
            counter,
        }
    }

    /// Build a gate that always reports fresh — used for entries that don't
    /// bind to a specific class (e.g. test fixtures, JIT entries during VM
    /// boot before the manager has assigned a `ClassId`).
    #[inline]
    pub fn never_stale() -> Self {
        Self {
            generation: 0,
            counter: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Returns `true` if the live counter has advanced past the snapshot —
    /// i.e. the declaring class has been redefined since this entry was
    /// populated.  Uses `Acquire` ordering so a reader that observes the
    /// new generation is also guaranteed to observe the new
    /// `Class.methods` table that `redefine_class` wrote before the
    /// `Release` `fetch_add`.
    #[inline]
    pub fn is_stale(&self) -> bool {
        self.counter.load(Ordering::Acquire) != self.generation
    }
}

impl fmt::Debug for RedefineGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RedefineGate(gen={})", self.generation)
    }
}

/// Fully-resolved invoke target. Eliminates all intermediate string lookups,
/// class loading, and superclass walks on subsequent calls.
#[derive(Clone)]
pub enum CachedInvokeTarget<JitMethod = ()> {
    /// Bytecode method: all data needed for Frame creation (invokestatic/invokespecial).
    Bytecode {
        cached: Arc<CachedBytecodeMethod>,
        /// WP2.4-F1 — redefine staleness gate; bound to
        /// `cached.declaring_class_id`.
        gate: RedefineGate,
    },
    /// Native method: direct function pointer (invokestatic/invokespecial).
    Native {
        callback: NativeCallback,
        /// Stable registry slot. Makes census and live kind/callback redemption
        /// O(1) on a warmed call site, without re-hashing the name triple.
        native_id: NativeMethodId,
        /// Registration category captured when this entry was populated.
        /// Strict-mode hits revalidate it from `native_id` before dispatch.
        native_kind: NativeKind,
        num_params: u16,
        /// WP2.4-F1 — redefine staleness gate; bound to the resolved
        /// declaring class. A redefine that swaps a native method body for
        /// bytecode (or vice versa) must evict this entry.
        gate: RedefineGate,
    },
    /// Monomorphic virtual bytecode: checks receiver class, dispatches if match.
    VirtualBytecode {
        receiver_class_id: ClassId,
        cached: Arc<CachedBytecodeMethod>,
        /// WP2.4-F1 — redefine staleness gate; bound to
        /// `cached.declaring_class_id` (the class whose method body we hold).
        gate: RedefineGate,
    },
    /// Monomorphic virtual native: checks receiver class, dispatches if match.
    VirtualNative {
        receiver_class_id: ClassId,
        callback: NativeCallback,
        /// Stable registry slot. Makes census and live kind/callback redemption
        /// O(1) on a warmed call site, without re-hashing the name triple.
        native_id: NativeMethodId,
        /// Registration category captured when this entry was populated.
        /// Strict-mode hits revalidate it from `native_id` before dispatch.
        native_kind: NativeKind,
        num_params: u16,
        /// WP2.4-F1 — redefine staleness gate.
        gate: RedefineGate,
    },
    /// JIT-compiled method: call native code directly, no frame push needed.
    Jit {
        /// Backend-owned compiled artifact. Generic by design: class loading
        /// owns invoke-cache semantics, not a concrete compiler backend.
        compiled: JitMethod,
        num_params: u16,
        return_type: u8, // b'I', b'J', b'V'
        needs_heap: bool,
        /// The bytecode method being JIT-executed — retained so that when a
        /// callee throws, the interpreter can route the exception through
        /// this method's exception table (the JIT itself has no exception
        /// handling; without this, a `try { ... } catch { ... }` in a
        /// JIT'd method is bypassed and the exception escapes). C1 fix.
        cached: Arc<CachedBytecodeMethod>,
        /// WP2.4-F1 — redefine staleness gate; on stale hit the JIT entry
        /// is evicted and the slow path re-resolves against the freshly
        /// installed bytecode (the JIT cache itself is invalidated by
        /// `fire_jit_invalidate_hook` in `redefine_class` step 8).
        gate: RedefineGate,
        /// C1→C2 supersede epoch snapshot (`jit_supersede_epoch()` at
        /// IC-fill time). When the background worker publishes an optimizing
        /// recompile that replaces a C1 body, it bumps the global epoch;
        /// `is_stale` then reports this entry stale on its next hit, the
        /// cache self-evicts it, and re-resolution picks up the C2 artifact
        /// from the jit cache. The superseded artifact is retained forever
        /// (code is never freed), so a not-yet-evicted entry is merely the
        /// slower C1 body — never a dangling pointer.
        supersede_epoch: u32,
    },
    /// Interpreter intrinsic: a hot JDK method resolved once at IC-fill time
    /// to a direct intrinsic handler. Steady-state dispatch pays no native
    /// registry probe, no class-manager `RwLock`, and no descriptor parse.
    /// See `gaps/feature_roadmap_interpreter_intrinsic_table.md`.
    Intrinsic {
        /// The resolved intrinsic identity — kept for the hit counter, the
        /// on/off debug flag, and `Debug` formatting.
        kind: cratonvm_native_api::InterpIntrinsic,
        /// Directly-callable handler trampoline (same shape as a
        /// `NativeCallback`); forwards into the `native-builtins` intrinsics.
        callback: NativeCallback,
        /// Parameter slot count (receiver excluded).
        num_params: u16,
        /// Phase 3 — parameter descriptors, split ONCE at IC-fill time. The
        /// steady-state dispatch path coerces args against these without
        /// re-resolving the method ref (no resolution-cache `RwLock`, no
        /// `HashMap` probe) and without re-parsing the descriptor string.
        param_descs: Arc<[Arc<str>]>,
        /// Phase 3 — return-type byte (`b'I'`/`b'J'`/`b'V'`/…), precomputed
        /// at IC-fill time so the steady-state path does no descriptor scan.
        return_type: u8,
        /// `Some(class)` for `invokevirtual`/`invokeinterface` — the IC must
        /// verify the receiver's actual class matches before dispatching
        /// (roadmap §3.4 virtual-dispatch soundness). `None` for
        /// `invokestatic`, which needs no receiver guard.
        receiver_class_id: Option<ClassId>,
        /// WP2.4-F1 — redefine staleness gate; bound to the resolved
        /// declaring class.
        gate: RedefineGate,
    },
}

impl<JitMethod> CachedInvokeTarget<JitMethod> {
    /// WP2.4-F1 — fast O(1) staleness check. Returns `true` when the bound
    /// class's redefine generation has advanced since this entry was
    /// populated. The caller is expected to evict the entry and fall
    /// through to the slow path on `true`.
    #[inline]
    pub fn is_stale(&self) -> bool {
        match self {
            CachedInvokeTarget::Bytecode { gate, .. }
            | CachedInvokeTarget::Native { gate, .. }
            | CachedInvokeTarget::VirtualBytecode { gate, .. }
            | CachedInvokeTarget::VirtualNative { gate, .. }
            | CachedInvokeTarget::Intrinsic { gate, .. } => gate.is_stale(),
            // A Jit entry is additionally stale once a C1→C2 supersede has
            // been published anywhere (global epoch advanced): the cached
            // `Arc<CompiledMethod>` may be the replaced C1 body, so evict
            // and re-resolve from the jit cache. `InvokeCache::get` performs
            // this check on every hit and self-evicts, so no dispatch arm
            // needs supersede-specific handling.
            CachedInvokeTarget::Jit {
                gate,
                supersede_epoch,
                ..
            } => gate.is_stale() || *supersede_epoch != crate::class_manager::jit_supersede_epoch(),
        }
    }
}

/// Cache key: (caller class, constant pool index, is_special).
///
/// The `is_special` bit distinguishes `invokespecial` from
/// `invokevirtual`/`invokeinterface`. JVMS §5.4.3.3/§5.4.3.4 resolve the two
/// opcodes through different rules: invokespecial statically binds to the
/// method ref's declared class, while invokevirtual/invokeinterface dispatch
/// on the receiver's actual class. The **same** `cp_index` can legitimately
/// be referenced by both an invokespecial and an invokevirtual in different
/// methods of the same class (e.g. Scala trait bridges where the 4-arg
/// `mkString$` does `invokespecial #666` while the 1-arg `mkString` default
/// does `invokeinterface #666` — both pointing at the same InterfaceMethodref
/// entry). Without the opcode bit, the virtual cache entry (with receiver
/// class = concrete subclass) would hijack the invokespecial call and cause
/// infinite recursion through the subclass override.
type InvokeCacheKey = (ClassId, u16, bool);

/// Per-call-site cap on the polymorphic overflow cache (see `poly_entries`
/// below). A handful of distinct receiver classes at one call site (the
/// common "a few concrete implementations of one interface" shape, e.g.
/// H2's `org.h2.bnf.Rule` family) all fit comfortably; a genuinely
/// megamorphic site (hundreds+ of distinct receiver classes) simply stops
/// gaining new poly entries past the cap and keeps missing for classes
/// beyond it, exactly as it did before this cache existed -- never
/// unbounded growth.
const POLY_CACHE_CAP_PER_SITE: usize = 8;

/// Cache that maps invoke sites to fully-resolved invoke targets.
/// On cache hit, no locks, string allocations, or method resolution needed.
pub struct InvokeCache<JitMethod = ()> {
    entries: FxHashMap<InvokeCacheKey, CachedInvokeTarget<JitMethod>>,
    /// Small overflow cache for call sites that see MORE than one distinct
    /// receiver class. `entries` above is monomorphic: a second receiver
    /// class simply overwrites the first, so an alternating/megamorphic call
    /// site (e.g. a `for (Rule r : list) r.autoComplete(...)` loop rotating
    /// through several concrete `Rule` implementations at the SAME bytecode
    /// call site) misses `entries` on nearly every call, falling all the way
    /// through to the expensive vtable/slow-path resolution each time. Keyed
    /// by the same call-site identity PLUS the receiver's concrete class, so
    /// distinct receiver classes coexist here instead of evicting each
    /// other. Purely an additive fast-path optimization: `entries` and its
    /// `get`/`put`/`evict` semantics below are completely unchanged, and any
    /// call site that only ever sees one receiver class never populates this
    /// map at all (see `put_poly`'s callers, which are the SAME call sites
    /// that already populate `entries`).
    poly_entries: FxHashMap<InvokeCacheKey, Vec<(ClassId, CachedInvokeTarget<JitMethod>)>>,
}

impl<JitMethod: Clone> InvokeCache<JitMethod> {
    pub fn new() -> Self {
        Self {
            entries: fx_hashmap_with_capacity(64),
            poly_entries: fx_hashmap_with_capacity(0),
        }
    }

    /// Look up an invoke-cache entry. Stale entries (whose declaring class
    /// has been redefined since the entry was populated) are *not* returned
    /// from this getter — the call site falls through to the slow path, which
    /// re-resolves against the freshly installed bytecode and overwrites this
    /// slot. See [`RedefineGate`] for the underlying check.
    ///
    /// # One hash probe, not two
    ///
    /// This is the interpreter's most frequent map lookup — every
    /// `invokevirtual` / `invokespecial` / `invokestatic` that a call site has
    /// warmed comes through it. It used to hash `key` TWICE per hit: once to
    /// test staleness through a borrow it immediately dropped, and once more to
    /// return the value. `InvokeCache::get` was the single hottest symbol in
    /// every profile taken of `WebClientIntegrationTests` and of the isolated
    /// WebClient exchange it is built from (2.65-2.95% of all samples, ahead of
    /// `execute_frame_from_index` itself), and half of that was the duplicate
    /// probe.
    ///
    /// # Why dropping the eviction is safe
    ///
    /// The `remove` this replaces was an optimisation, not a correctness term.
    /// A stale entry that stays in the map is still *reported* as a miss by the
    /// `filter` below, so no caller can ever dispatch through it — and the miss
    /// sends that call site down the slow path, which ends in
    /// `populate_invoke_cache` -> [`Self::put`], and `put` overwrites the SAME
    /// key. The slot therefore heals on the very next call rather than being
    /// vacated on this one, and the table cannot grow: a key that is present
    /// stays present, whether it holds the stale entry or its replacement.
    ///
    /// A call site that goes stale and is then never executed again keeps one
    /// entry alive until the cache is dropped with its thread. That is the same
    /// footprint the entry had before it went stale, and staleness only arises
    /// from a JVMTI redefine or a C1->C2 supersede, both rare by construction.
    #[inline]
    pub fn get(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
    ) -> Option<&CachedInvokeTarget<JitMethod>> {
        // WP2.4-F1: O(1) generation check on hit, folded into the same probe.
        self.entries
            .get(&(caller_class, cp_index, is_special))
            .filter(|t| !t.is_stale())
    }

    pub fn put(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        self.entries
            .insert((caller_class, cp_index, is_special), target);
    }

    /// Second-chance lookup for a call site that just missed the primary
    /// `get` above, keyed additionally by the receiver's concrete class.
    /// Only ever consulted AFTER a primary miss (see
    /// `execute_invokevirtual_cached`'s cache-miss branch) -- a primary hit
    /// never reaches this. Same staleness handling as `get`: a stale hit is
    /// auto-evicted and treated as a miss.
    #[inline]
    pub fn get_poly(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        receiver_class: ClassId,
    ) -> Option<CachedInvokeTarget<JitMethod>> {
        let key = (caller_class, cp_index, is_special);
        let entries = self.poly_entries.get_mut(&key)?;
        let idx = entries.iter().position(|(cid, _)| *cid == receiver_class)?;
        if entries[idx].1.is_stale() {
            entries.remove(idx);
            return None;
        }
        Some(entries[idx].1.clone())
    }

    /// Remember a resolved target for a specific receiver class at this call
    /// site, in ADDITION to (never instead of) the primary monomorphic slot
    /// `put` above still maintains for the same call. Capped at
    /// `POLY_CACHE_CAP_PER_SITE` distinct receiver classes per call site;
    /// beyond the cap, further distinct classes are simply not remembered
    /// here (they keep working correctly via the normal slow path -- this
    /// cache is purely a fast-path optimization, never a correctness
    /// requirement). An existing entry for the same receiver class is
    /// refreshed in place.
    pub fn put_poly(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        receiver_class: ClassId,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        let key = (caller_class, cp_index, is_special);
        let entries = self.poly_entries.entry(key).or_default();
        if let Some(slot) = entries.iter_mut().find(|(cid, _)| *cid == receiver_class) {
            slot.1 = target;
            return;
        }
        if entries.len() < POLY_CACHE_CAP_PER_SITE {
            entries.push((receiver_class, target));
        }
    }

    /// Clear all cached invoke targets. Used to invalidate stale entries after
    /// a partially-failed class initialization (e.g. System.initPhase1).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.poly_entries.clear();
    }

    /// Evict a single entry. Used by call sites that detect staleness via
    /// other means (e.g. JIT downgrade) to force the next lookup down the
    /// slow path. Also drops any polymorphic overflow entries for the same
    /// call site (across all receiver classes) -- an evict typically fires
    /// because something about the call site's resolution changed (e.g. a
    /// redefine-driven shadow suppression), which is not receiver-specific,
    /// so distrust everything cached at this call site, matching the
    /// pre-existing `entries` eviction's own scope.
    #[inline]
    pub fn evict(&mut self, caller_class: ClassId, cp_index: u16, is_special: bool) {
        let key = (caller_class, cp_index, is_special);
        self.entries.remove(&key);
        self.poly_entries.remove(&key);
    }
}

impl<JitMethod: Clone> Default for InvokeCache<JitMethod> {
    fn default() -> Self {
        Self::new()
    }
}

impl<JitMethod> fmt::Debug for InvokeCache<JitMethod> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvokeCache({} entries)", self.entries.len())
    }
}

impl<JitMethod> fmt::Debug for CachedInvokeTarget<JitMethod> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CachedInvokeTarget::Bytecode { cached: m, .. } => {
                write!(
                    f,
                    "Bytecode({}.{}{})",
                    m.class_name, m.method_name, m.method_descriptor
                )
            }
            CachedInvokeTarget::Native { .. } => write!(f, "Native(...)"),
            CachedInvokeTarget::VirtualBytecode { cached, .. } => {
                write!(
                    f,
                    "VirtualBytecode({}.{}{})",
                    cached.class_name, cached.method_name, cached.method_descriptor
                )
            }
            CachedInvokeTarget::VirtualNative { .. } => write!(f, "VirtualNative(...)"),
            CachedInvokeTarget::Jit { .. } => write!(f, "Jit(...)"),
            CachedInvokeTarget::Intrinsic { kind, .. } => write!(f, "Intrinsic({kind:?})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_field_put_and_get() {
        let mut cache = ResolutionCache::new();
        let key_class = ClassId::new(1);
        let cp_index = 5;

        assert!(cache.get_field(key_class, cp_index).is_none());

        cache.put_field(
            key_class,
            cp_index,
            ResolvedField {
                declaring_class_id: ClassId::new(2),
                field_index: 3,
                is_static: true,
                is_volatile: false,
                is_reference: false,
                desc_byte: 0,
            },
        );

        let resolved = cache.get_field(key_class, cp_index).unwrap();
        assert_eq!(resolved.declaring_class_id, ClassId::new(2));
        assert_eq!(resolved.field_index, 3);
        assert!(resolved.is_static);
        assert_eq!(cache.field_count(), 1);
    }

    #[test]
    fn cache_method_put_and_get() {
        fn cached_native(
            _ctx: &mut dyn cratonvm_native_api::NativeContext,
            _args: &[cratonvm_types::Value],
        ) -> cratonvm_types::error::MethodCallResult {
            Ok(None)
        }

        let mut cache = ResolutionCache::new();
        let key_class = ClassId::new(0);
        let cp_index = 10;

        assert!(cache.get_method(key_class, cp_index).is_none());

        cache.put_method(
            key_class,
            cp_index,
            ResolvedMethod {
                declaring_class_id: ClassId::new(3),
                class_name: Arc::from("java/lang/Object"),
                method_name: Arc::from("toString"),
                method_descriptor: Arc::from("()Ljava/lang/String;"),
                num_params: 0,
                native_target: Some(cached_native),
                native_kind: Some(cratonvm_native_api::NativeKind::Bridge),
            },
        );

        let resolved = cache.get_method(key_class, cp_index).unwrap();
        assert_eq!(resolved.declaring_class_id, ClassId::new(3));
        assert_eq!(&*resolved.method_name, "toString");
        assert_eq!(
            resolved.native_target.map(|target| target as usize),
            Some(cached_native as usize)
        );
        assert_eq!(
            resolved.native_kind,
            Some(cratonvm_native_api::NativeKind::Bridge)
        );
        assert_eq!(cache.method_count(), 1);
    }

    #[test]
    fn cache_different_classes_same_index() {
        let mut cache = ResolutionCache::new();
        let class_a = ClassId::new(1);
        let class_b = ClassId::new(2);

        cache.put_field(
            class_a,
            5,
            ResolvedField {
                declaring_class_id: ClassId::new(10),
                field_index: 0,
                is_static: false,
                is_volatile: false,
                is_reference: false,
                desc_byte: 0,
            },
        );
        cache.put_field(
            class_b,
            5,
            ResolvedField {
                declaring_class_id: ClassId::new(20),
                field_index: 1,
                is_static: true,
                is_volatile: false,
                is_reference: false,
                desc_byte: 0,
            },
        );

        assert_eq!(
            cache.get_field(class_a, 5).unwrap().declaring_class_id,
            ClassId::new(10),
        );
        assert_eq!(
            cache.get_field(class_b, 5).unwrap().declaring_class_id,
            ClassId::new(20),
        );
        assert_eq!(cache.field_count(), 2);
    }

    #[test]
    fn cache_overwrites_same_key() {
        let mut cache = ResolutionCache::new();
        let class_id = ClassId::new(1);

        cache.put_field(
            class_id,
            3,
            ResolvedField {
                declaring_class_id: ClassId::new(5),
                field_index: 0,
                is_static: false,
                is_volatile: false,
                is_reference: false,
                desc_byte: 0,
            },
        );
        cache.put_field(
            class_id,
            3,
            ResolvedField {
                declaring_class_id: ClassId::new(6),
                field_index: 1,
                is_static: true,
                is_volatile: false,
                is_reference: false,
                desc_byte: 0,
            },
        );

        let resolved = cache.get_field(class_id, 3).unwrap();
        assert_eq!(resolved.declaring_class_id, ClassId::new(6));
        assert_eq!(cache.field_count(), 1);
    }

    /// C12 regression: the invoke cache MUST distinguish `invokespecial`
    /// from `invokevirtual`/`invokeinterface` on the same (caller_class,
    /// cp_index) pair. Scala trait bridges like `IterableOnceOps.mkString$`
    /// share a single InterfaceMethodref cp_index with the virtual call from
    /// the trait's own default method. Without the `is_special` key bit, the
    /// virtual cache entry (receiver = concrete subclass override) hijacks
    /// the invokespecial call and recurses through the subclass override,
    /// blowing the stack.
    #[test]
    fn invoke_cache_distinguishes_special_vs_virtual() {
        use std::sync::Arc;
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(7);
        let cp_index = 666u16;

        let make_cached = |name: &str| {
            Arc::new(CachedBytecodeMethod {
                declaring_class_id: ClassId::new(99),
                class_name: Arc::from(name),
                method_name: Arc::from("mkString"),
                method_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/String;"),
                source_file: None,
                code: Arc::from(&[0xb1u8][..]),
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 1,
                num_params: 1,
                is_synchronized: false,
                is_static: false,
                force_native_cache: std::sync::OnceLock::new(),
                intercept_shape_cache: std::sync::OnceLock::new(),
                native_callback_cache: std::sync::OnceLock::new(),
                invoc_key: std::sync::OnceLock::new(),
                jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
                quickened: std::sync::OnceLock::new(),
            })
        };

        // invokespecial entry: points at the interface default method itself.
        cache.put(
            caller,
            cp_index,
            true,
            CachedInvokeTarget::Bytecode {
                cached: make_cached("scala/collection/IterableOnceOps"),
                gate: RedefineGate::never_stale(),
            },
        );
        // invokevirtual/interface entry at the same cp_index: points at the
        // concrete subclass's override.
        cache.put(
            caller,
            cp_index,
            false,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(42),
                cached: make_cached("scala/collection/AbstractIterable"),
                gate: RedefineGate::never_stale(),
            },
        );

        // The two entries must NOT alias each other.
        let special = cache.get(caller, cp_index, true).expect("special entry");
        match special {
            CachedInvokeTarget::Bytecode { cached: m, .. } => {
                assert_eq!(&*m.class_name, "scala/collection/IterableOnceOps");
            }
            other => panic!("expected Bytecode for invokespecial, got {other:?}"),
        }
        let virt = cache.get(caller, cp_index, false).expect("virtual entry");
        match virt {
            CachedInvokeTarget::VirtualBytecode { cached, .. } => {
                assert_eq!(&*cached.class_name, "scala/collection/AbstractIterable");
            }
            other => panic!("expected VirtualBytecode for invokevirtual, got {other:?}"),
        }
    }

    /// WP2.4-F1: redefining a class must invalidate the cache entry on the
    /// next hit. Bumping the live counter (which is what `redefine_class`
    /// does) renders the snapshotted entry stale.
    #[test]
    fn invoke_cache_evicts_stale_entry_after_redefine_bump() {
        use std::sync::atomic::Ordering;
        use std::sync::Arc;
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(7);
        let cp_index = 11u16;

        let counter = Arc::new(AtomicU32::new(0));
        let cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: ClassId::new(99),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("answer"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]), // bipush 42; ireturn
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        });
        let entry = CachedInvokeTarget::Bytecode {
            cached,
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
        };
        cache.put(caller, cp_index, false, entry);

        // First hit — counter unchanged, must return Some.
        assert!(cache.get(caller, cp_index, false).is_some());

        // Simulate `redefine_class` step 7 — bump the live counter.
        counter.fetch_add(1, Ordering::Release);

        // Next hit — entry is stale, getter must auto-evict and return None.
        assert!(
            cache.get(caller, cp_index, false).is_none(),
            "stale entry must be evicted after redefine bump"
        );

        // Subsequent populate with a fresh snapshot succeeds.
        let new_cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: ClassId::new(99),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("answer"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x63, 0xACu8][..]), // bipush 99; ireturn
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        });
        cache.put(
            caller,
            cp_index,
            false,
            CachedInvokeTarget::Bytecode {
                cached: new_cached,
                gate: RedefineGate::snapshot(Arc::clone(&counter)),
            },
        );
        let hit = cache.get(caller, cp_index, false).expect("fresh entry hit");
        match hit {
            CachedInvokeTarget::Bytecode { cached, .. } => {
                // The bipush byte for 99 (0x63) confirms we got the new body.
                assert_eq!(cached.code[1], 0x63);
            }
            _ => panic!("expected Bytecode entry"),
        }
    }

    #[test]
    fn method_handle_kind_from_tag() {
        assert_eq!(
            MethodHandleKind::from_tag(1),
            Some(MethodHandleKind::GetField)
        );
        assert_eq!(
            MethodHandleKind::from_tag(5),
            Some(MethodHandleKind::InvokeVirtual)
        );
        assert_eq!(
            MethodHandleKind::from_tag(6),
            Some(MethodHandleKind::InvokeStatic)
        );
        assert_eq!(
            MethodHandleKind::from_tag(8),
            Some(MethodHandleKind::NewInvokeSpecial)
        );
        assert_eq!(
            MethodHandleKind::from_tag(9),
            Some(MethodHandleKind::InvokeInterface)
        );
        assert_eq!(MethodHandleKind::from_tag(0), None);
        assert_eq!(MethodHandleKind::from_tag(10), None);
    }

    #[test]
    fn method_handle_kind_display() {
        assert_eq!(
            MethodHandleKind::InvokeStatic.to_string(),
            "REF_invokeStatic"
        );
        assert_eq!(
            MethodHandleKind::InvokeVirtual.to_string(),
            "REF_invokeVirtual"
        );
    }

    #[test]
    fn method_handle_construction() {
        let mh = MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("com/example/Foo"),
            member_name: Arc::from("bar"),
            descriptor: Arc::from("(I)V"),
        };
        assert_eq!(mh.kind, MethodHandleKind::InvokeStatic);
        assert_eq!(&*mh.class_name, "com/example/Foo");
        assert_eq!(&*mh.member_name, "bar");
        assert_eq!(&*mh.descriptor, "(I)V");
    }

    #[test]
    fn cache_call_site_put_and_get() {
        let mut cache = ResolutionCache::new();
        let class_id = ClassId::new(1);
        let cp_index = 42;

        assert!(cache.get_call_site(class_id, cp_index).is_none());

        cache.put_call_site(
            class_id,
            cp_index,
            ResolvedCallSite::StringConcat {
                recipe: Arc::from(
                    "Hello, \u{0001}!"
                        .encode_utf16()
                        .collect::<Vec<u16>>()
                        .as_slice(),
                ),
                constant_args: vec![],
                target_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/String;"),
            },
        );

        assert!(cache.get_call_site(class_id, cp_index).is_some());
        assert_eq!(cache.call_site_count(), 1);
    }

    #[test]
    fn cache_lambda_call_site() {
        let mut cache = ResolutionCache::new();
        let class_id = ClassId::new(1);
        let proxy_id = ClassId::new(0x8000_0000);

        cache.put_call_site(
            class_id,
            10,
            ResolvedCallSite::Lambda(LambdaCallSite {
                functional_interface_id: None,
                functional_interface: Arc::from("java/lang/Runnable"),
                sam_method_name: Arc::from("run"),
                sam_descriptor: Arc::from("()V"),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: Arc::from("com/example/App"),
                    member_name: Arc::from("lambda$main$0"),
                    descriptor: Arc::from("()V"),
                },
                instantiated_descriptor: Arc::from("()V"),
                capture_types: vec![],
                proxy_class_id: proxy_id,
                serializable_flag: false,
            }),
        );

        let site = cache.get_call_site(class_id, 10).unwrap();
        match site {
            ResolvedCallSite::Lambda(lcs) => {
                assert_eq!(&*lcs.functional_interface, "java/lang/Runnable");
                assert_eq!(&*lcs.sam_method_name, "run");
                assert_eq!(lcs.proxy_class_id, proxy_id);
            }
            _ => panic!("Expected Lambda call site"),
        }
    }

    /// Round 5 audit fix (MED #10) / Round 7 audit fix (MED #11):
    /// `LinkResolver` round-trip — populate a triple, read it back,
    /// confirm `NotFound` is also cached so the next probe
    /// short-circuits, and verify `invalidate_class` drops every entry
    /// whose key or resolved declaring class matches — plus every
    /// negative, whose provenance the key does not record.
    #[test]
    fn link_resolver_caches_and_invalidates() {
        let resolver = LinkResolver::new();
        let class_a = ClassId::new(1);
        let class_b = ClassId::new(2);
        let name: Arc<str> = Arc::from("iterator");
        let desc: Arc<str> = Arc::from("()Ljava/util/Iterator;");

        // Cold miss.
        assert!(resolver.get(class_a, &name, &desc).is_none());

        // Populate with a Method resolution whose declaring class is
        // class_b — both the key class and the resolved declaring
        // class participate in invalidation, so we exercise both
        // axes.
        resolver.insert(
            class_a,
            Arc::clone(&name),
            Arc::clone(&desc),
            ResolvedMember::Method {
                declaring_class_id: class_b,
                index: 7,
            },
        );

        // Hit.
        match resolver.get(class_a, &name, &desc) {
            Some(ResolvedMember::Method {
                declaring_class_id,
                index,
            }) => {
                assert_eq!(declaring_class_id, class_b);
                assert_eq!(index, 7);
            }
            other => panic!("expected cached Method, got {other:?}"),
        }
        assert_eq!(resolver.len(), 1);

        // NotFound is also cached.
        let missing_name: Arc<str> = Arc::from("totallyNotAMethod");
        resolver.insert(
            class_a,
            Arc::clone(&missing_name),
            Arc::clone(&desc),
            ResolvedMember::NotFound,
        );
        assert!(matches!(
            resolver.get(class_a, &missing_name, &desc),
            Some(ResolvedMember::NotFound)
        ));
        assert_eq!(resolver.len(), 2);

        // Invalidating class_b drops the Method entry (resolved declaring
        // class match) AND the NotFound entry. The latter changed in the
        // 2026-07-26 memoized-negative fix: a negative asserts something about
        // the whole hierarchy walk, and the key does not record which classes
        // that walk passed through, so it cannot be proven unaffected.
        resolver.invalidate_class(class_b);
        assert!(resolver.get(class_a, &name, &desc).is_none());
        assert!(
            resolver.get(class_a, &missing_name, &desc).is_none(),
            "a negative must not survive an unrelated-looking invalidation — \
             the changed class may be an ancestor the walk crossed"
        );
        assert!(resolver.is_empty());
    }

    /// REGRESSION (memoized-negative, 2026-07-26): a `NotFound` cached under a
    /// *subclass* key must be dropped when an **ancestor** is invalidated.
    ///
    /// Concrete failure this reproduces: `GetMethodID(Sub, "m", "()V")` runs
    /// while `Base` is still a synthetic stub that lacks `m`, so `NotFound` is
    /// cached at key `(Sub, "m", "()V")`. `upgrade_synthetic_class` then swaps
    /// in the real `Base`, which declares `m`, and fires the invalidate hook
    /// for `Base`. Under the old `retain`, the entry's key class was `Sub`, not
    /// `Base`, and its value carried no `declaring_class_id`, so it was kept —
    /// and every later `GetMethodID` returned a null `jmethodID`, surfacing as
    /// `NoSuchMethodError` for a method that now exists. The only escapes were
    /// `clear()` (no production caller) and the CLOCK sweep at 128 K entries.
    #[test]
    fn negative_cached_under_subclass_is_dropped_when_ancestor_changes() {
        let resolver = LinkResolver::new();
        let base = ClassId::new(10);
        let sub = ClassId::new(11);
        let name: Arc<str> = Arc::from("m");
        let desc: Arc<str> = Arc::from("()V");

        // The walk from `Sub` climbed through `Base` and found nothing.
        resolver.insert(
            sub,
            Arc::clone(&name),
            Arc::clone(&desc),
            ResolvedMember::NotFound,
        );
        assert!(matches!(
            resolver.get(sub, &name, &desc),
            Some(ResolvedMember::NotFound)
        ));

        // `Base` gains the method and fires the invalidate hook for itself.
        // Nothing in the key mentions `Base`, so only the negative-dropping
        // rule can save us here.
        resolver.invalidate_class(base);

        assert!(
            resolver.get(sub, &name, &desc).is_none(),
            "the stale negative must be gone so the next probe re-walks the \
             hierarchy and finds the newly-declared method"
        );
    }

    /// The fix must not throw away positives it has no reason to distrust: a
    /// resolution whose declaring class is unrelated to the invalidated class
    /// still survives.
    #[test]
    fn unrelated_positive_survives_invalidation() {
        let resolver = LinkResolver::new();
        let keeper = ClassId::new(20);
        let declaring = ClassId::new(21);
        let unrelated = ClassId::new(22);
        let name: Arc<str> = Arc::from("size");
        let desc: Arc<str> = Arc::from("()I");

        resolver.insert(
            keeper,
            Arc::clone(&name),
            Arc::clone(&desc),
            ResolvedMember::Method {
                declaring_class_id: declaring,
                index: 3,
            },
        );
        resolver.invalidate_class(unrelated);
        assert!(
            matches!(
                resolver.get(keeper, &name, &desc),
                Some(ResolvedMember::Method { index: 3, .. })
            ),
            "positives keyed and declared elsewhere must not be collateral damage"
        );
    }

    /// Round 9 audit fix (vm LOW #11): `resolve_or_compute` must detect
    /// a race-winner that populated the same key while we were
    /// computing, and return the winner's entry instead of clobbering
    /// it. Simulate the race by populating the cache from outside
    /// inside the closure (which models another thread winning).
    #[test]
    fn resolve_or_compute_handles_race_loser() {
        let resolver = LinkResolver::new();
        let class_a = ClassId::new(1);
        let class_winner = ClassId::new(99);
        let class_loser = ClassId::new(42);

        // Loser computes a result, but the closure simulates a winner
        // by inserting into the cache mid-compute. The race-loser
        // check must spot the winner and return it.
        let got = resolver.resolve_or_compute(class_a, "m", "()V", || {
            // Simulate a different thread winning: insert under the
            // same key with `class_winner` as the declaring class.
            resolver.insert(
                class_a,
                Arc::from("m"),
                Arc::from("()V"),
                ResolvedMember::Method {
                    declaring_class_id: class_winner,
                    index: 7,
                },
            );
            // The "loser" returns a different declaring class.
            (
                Arc::from("m"),
                Arc::from("()V"),
                ResolvedMember::Method {
                    declaring_class_id: class_loser,
                    index: 99,
                },
            )
        });
        match got {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => {
                assert_eq!(
                    declaring_class_id, class_winner,
                    "race loser should have returned the winner's entry"
                );
                assert_eq!(index, 7);
            }
            other => panic!("expected Method, got {other:?}"),
        }
        // Cache must still hold the winner's entry, not the loser's.
        match resolver.get(class_a, "m", "()V").expect("entry present") {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => {
                assert_eq!(declaring_class_id, class_winner);
                assert_eq!(index, 7);
            }
            other => panic!("expected Method, got {other:?}"),
        }
        assert_eq!(resolver.len(), 1);
    }

    /// PERF (bounded cache): the cache must stay at or under [`CACHE_CAP`]
    /// no matter how many distinct triples are inserted, and a hot entry
    /// (kept warm via `get`) must survive the CLOCK eviction sweep.
    #[test]
    fn link_resolver_cache_is_bounded() {
        let resolver = LinkResolver::new();
        let desc: Arc<str> = Arc::from("()V");

        // Insert a "hot" entry and keep touching it so its CLOCK bit
        // stays set across sweeps.
        let hot_class = ClassId::new(1);
        let hot_name: Arc<str> = Arc::from("hot");
        resolver.insert(
            hot_class,
            hot_name.clone(),
            desc.clone(),
            ResolvedMember::Method {
                declaring_class_id: hot_class,
                index: 0,
            },
        );

        // Flood the cache with far more than CACHE_CAP distinct triples,
        // re-touching the hot entry between batches.
        for i in 0..(CACHE_CAP * 3) {
            // Use ClassId values that never collide with `hot_class`.
            let cid = ClassId::new((i as u32) + 2);
            let name: Arc<str> = Arc::from(format!("m{i}"));
            resolver.insert(
                cid,
                name,
                desc.clone(),
                ResolvedMember::Method {
                    declaring_class_id: cid,
                    index: i as u32,
                },
            );
            if i % 64 == 0 {
                // Keep the hot entry's reference bit set.
                let _ = resolver.get(hot_class, &hot_name, &desc);
            }
        }

        // Hard bound: never exceeds the cap.
        assert!(
            resolver.len() <= CACHE_CAP,
            "cache size {} exceeded cap {}",
            resolver.len(),
            CACHE_CAP
        );

        // The continuously-touched hot entry survived all the sweeps.
        assert!(
            resolver.get(hot_class, &hot_name, &desc).is_some(),
            "hot entry was evicted despite being kept warm"
        );
    }
}
