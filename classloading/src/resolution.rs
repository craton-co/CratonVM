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
    /// Whether this field has ACC_FINAL. Read only by the `putfield` /
    /// `putstatic` linking check (JVMS §6.5: a final field may be written only
    /// by its declaring class, and from class file version 53 only by that
    /// class's `<init>` / `<clinit>`); reflection and `Unsafe` never consult it.
    pub is_final: bool,
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
    /// A `String` label of `SwitchBootstraps.enumSwitch`: the target enum
    /// constant's name equals this. Only an `enumSwitch` that also carries
    /// `Class` labels is linked as a `TypeSwitch` with these; an all-name one
    /// stays [`ResolvedCallSite::EnumSwitch`].
    EnumConstant(Arc<str>),
    /// An `EnumDesc` label of `SwitchBootstraps.typeSwitch` (a qualified
    /// `case E.A` in a pattern switch, JEP 441): the target is the enum
    /// constant `name` of the enum class `class_id` — its class is `class_id`
    /// or a constant-body subclass of it (`Enum.getDeclaringClass()`).
    EnumDesc { class_id: ClassId, name: Arc<str> },
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
        /// `None` for javac's shape (one `REF_getField` per record component,
        /// in component order, from the record itself), which the VM's
        /// whole-record walk answers. `Some(record class)` when the site's
        /// getters are another list (a subset, another order): then
        /// `field_indices` / `field_descriptors` hold one entry per GETTER,
        /// `equals` / `hashCode` fold over them as the JDK's `ObjectMethods`
        /// does, and `equals` tests the other operand against the record
        /// class (`Class.isInstance`). Interpreter round i1 wave 39, lane L4.
        getter_driven: Option<ClassId>,
        /// Empty unless a getter-driven `toString` site has METHOD getters
        /// (`--jdk-only`): then one entry per getter, `Some(getter)` for a
        /// method getter (whose `field_indices` entry is unused and whose
        /// `field_descriptors` entry is its return type), `None` for a field
        /// getter. The JDK's `makeToString` route needs
        /// `MethodHandle.copyWith`, which CratonVM lacks. Interpreter round i1
        /// wave 41, lane L4 (accessor methods); wave 42 (static and special
        /// getters).
        accessor_getters: Vec<Option<RecordAccessorGetter>>,
    },
}

/// A METHOD getter of a getter-driven `ObjectMethods` `toString` site, whose
/// handle type is `(R)T` for the record class `R` (the only type the JDK's
/// `makeToString` accepts). Interpreter round i1 wave 41 (`Virtual`) and wave
/// 42 (`Static`, `Special`), lane L4.
#[derive(Debug, Clone)]
pub enum RecordAccessorGetter {
    /// `REF_invokeVirtual R.m()T`: selected from the receiver's class.
    Virtual {
        name: Arc<str>,
        descriptor: Arc<str>,
    },
    /// `REF_invokeStatic X.m(R)T` of any class `X` (already resolved through
    /// the calling class's loader): the receiver is its one argument.
    Static {
        owner: ClassId,
        owner_name: Arc<str>,
        name: Arc<str>,
        descriptor: Arc<str>,
    },
    /// `REF_invokeSpecial R.m()T` from `R` itself: `R`'s method (or the one it
    /// inherits), never a subclass's override.
    Special {
        owner: ClassId,
        owner_name: Arc<str>,
        name: Arc<str>,
        descriptor: Arc<str>,
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

/// A recorded failed resolution of one constant-pool entry (JVMS §5.4.3).
///
/// *"If an attempt by the Java Virtual Machine to resolve a symbolic reference
/// fails because an error is thrown that is an instance of `LinkageError` (or a
/// subclass), then subsequent attempts to resolve the reference always fail with
/// the same error that was thrown as a result of the initial resolution
/// attempt."* HotSpot keeps the error class and message per entry
/// (`SystemDictionary::add_resolution_error`) and throws a NEW instance of that
/// class with that message on every later attempt; this is the same record.
/// It holds no heap reference, so it needs no GC rooting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionFailure {
    /// Internal name of the `LinkageError` subclass that was thrown.
    pub error_class: Arc<str>,
    /// Its detail message, if it had one.
    pub message: Option<Arc<str>>,
    /// The thrown error's cause, as `(internal class name, detail message)`,
    /// if it had one: HotSpot's resolution-error table keeps the cause's class
    /// and message too and rethrows a new error with a new cause of that
    /// class and message (`ConstantPool::throw_resolution_error`), so a
    /// `NoClassDefFoundError` caused by a `ClassNotFoundException` stays so on
    /// every later attempt. Interpreter round i1 wave 23, lane L5: until then
    /// the rethrow dropped the cause, so the second attempt's error differed
    /// from the first's.
    pub cause: Option<(Arc<str>, Option<Arc<str>>)>,
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
    /// `Arc`-wrapped so the interpreter's fast path can take a site out of the
    /// read guard with one refcount bump, whatever its shape — cloning a
    /// `TypeSwitch` / `RecordObjectMethod` site cost one `Vec` per execution.
    call_sites: FxHashMap<ResolutionKey, Arc<ResolvedCallSite>>,
    /// Recorded `ldc` constants that re-resolution reproduces identically
    /// (Integer, Float, String, Class, MethodType) — FIFO-capped, since
    /// evicting one only costs a re-resolution.
    condy: FxHashMap<ResolutionKey, Value>,
    /// Recorded constants whose re-resolution would NOT yield the same object:
    /// `CONSTANT_Dynamic` (the bootstrap runs once, JVMS §5.4.3.6) and
    /// `CONSTANT_MethodHandle` (`Lookup.find*` answers a fresh handle).
    /// Uncapped and insert-if-absent — see [`Self::put_permanent_constant`].
    /// Bounded by the number of such CP entries actually executed, and purged
    /// with their class ([`Self::forget_classes`], [`Self::invalidate_class`]).
    permanent_constants: FxHashMap<ResolutionKey, Value>,
    /// Recorded `LinkageError` outcomes for constant-pool entries whose
    /// resolution failed (`CONSTANT_Class`, `CONSTANT_Dynamic`). See
    /// [`ResolutionFailure`].
    ///
    /// Uncapped, like [`Self::permanent_constants`] and for the same reason:
    /// an evicted failure record lets the next attempt resolve again, and
    /// possibly SUCCEED, which JVMS §5.4.3 forbids. Bounded by the number of
    /// entries that actually failed; purged with the class on unload.
    failures: FxHashMap<ResolutionKey, ResolutionFailure>,
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
            permanent_constants: fx_hashmap_with_capacity(0),
            fields_order: VecDeque::with_capacity(64),
            methods_order: VecDeque::with_capacity(64),
            call_sites_order: VecDeque::with_capacity(16),
            condy_order: VecDeque::with_capacity(16),
            failures: fx_hashmap_with_capacity(0),
        }
    }

    /// Look up a recorded resolution failure for this constant-pool entry.
    pub fn get_failure(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolutionFailure> {
        self.failures.get(&(class_id, cp_index))
    }

    /// Record that resolving this constant-pool entry failed with a
    /// `LinkageError`. The first record wins: a racing second resolver that
    /// failed differently must not replace the error the first attempt fixed.
    pub fn put_failure(&mut self, class_id: ClassId, cp_index: u16, failure: ResolutionFailure) {
        self.failures.entry((class_id, cp_index)).or_insert(failure);
    }

    /// The number of recorded resolution failures.
    pub fn failure_count(&self) -> usize {
        self.failures.len()
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

    /// The snapshot a `(class, cp index)` resolution takes BEFORE it reads the
    /// referencing class's constant pool, for [`Self::put_field_as_of`] /
    /// [`Self::put_method_as_of`]: the process redefinition count
    /// ([`crate::class_redefinition_count`], one `Acquire` load).
    ///
    /// Why the fill needs it (interpreter round i1 wave 22, lane L5): a
    /// resolver reads the constant pool under the class-manager read lock and
    /// fills this cache after dropping it (the lock order forbids holding the
    /// class manager while taking this cache's lock). A redefinition of the
    /// referencing class that runs in between replaces the pool and sweeps
    /// this cache (`ClassManager::redefine_class` step 9) before the fill
    /// lands, so the fill published the OLD pool's member under a key the new
    /// pool may give another meaning, and nothing swept it again. A field
    /// resolution loads the owner class in that window, which can run a
    /// Java agent's transformer on the same thread.
    ///
    /// In this crate's unit tests the count is the per-thread stand-in
    /// [`InvokeCache`] reads too (`invoke_cache_redefinition_count`).
    #[inline]
    pub fn fill_snapshot() -> u64 {
        invoke_cache_redefinition_count()
    }

    /// Whether a fill whose resolution took [`Self::fill_snapshot`] `as_of`
    /// may still be published under a key whose sweep ([`Self::invalidate_class`])
    /// reaches it through `class_id` (and, for a field or method, through
    /// `also`, its resolved declaring class). `redefine_class` advances the
    /// count (recording the class, [`crate::for_each_redefined_class_between`])
    /// before it replaces the pool and sweeps this cache under its write lock
    /// after that, so under this cache's write lock either the sweep has not
    /// run yet (and will remove the fill) or the count has moved past a record
    /// naming the class.
    ///
    /// Class-scoped since interpreter round i1 wave 23 (lane L5): wave 22
    /// skipped a fill on ANY redefinition in the process, which was harmless
    /// for a method or field (one re-resolution) but not for the records whose
    /// re-resolution is observable — a permanent constant re-runs its bootstrap
    /// (JVMS §5.4.3.6 allows one result) and a skipped failure lets a later
    /// attempt succeed (§5.4.3 forbids it). Now only a redefinition of the
    /// class the sweep keys on, or a span the ring cannot answer (lapped, or an
    /// unattributed bump), skips. A `ClassId` in the ring may be another VM's
    /// class with the same id, which skips one fill too many there.
    #[inline]
    fn fill_is_current(class_id: ClassId, also: Option<ClassId>, as_of: u64) -> bool {
        let now = invoke_cache_redefinition_count();
        if now == as_of {
            return true;
        }
        let mut redefined = false;
        let answered = invoke_cache_redefined_classes(as_of, now, |c| {
            redefined |= c == class_id || Some(c) == also;
        });
        answered && !redefined
    }

    /// [`Self::put_field`] for a resolution that took [`Self::fill_snapshot`]
    /// `as_of`; skipped (`false`) when a redefinition may have replaced the
    /// constant pool it read.
    pub fn put_field_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        resolved: ResolvedField,
        as_of: u64,
    ) -> bool {
        if !Self::fill_is_current(class_id, Some(resolved.declaring_class_id), as_of) {
            return false;
        }
        self.put_field(class_id, cp_index, resolved);
        true
    }

    /// [`Self::put_method`] for a resolution that took
    /// [`Self::fill_snapshot`] `as_of`; skipped (`false`) when a redefinition
    /// may have replaced the constant pool it read.
    pub fn put_method_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        resolved: ResolvedMethod,
        as_of: u64,
    ) -> bool {
        if !Self::fill_is_current(class_id, Some(resolved.declaring_class_id), as_of) {
            return false;
        }
        self.put_method(class_id, cp_index, resolved);
        true
    }

    /// [`Self::put_call_site`] for a bootstrap that took
    /// [`Self::fill_snapshot`] `as_of` before reading the caller's constant
    /// pool; skipped (`false`) when a redefinition of the caller may have
    /// replaced that pool (the next execution bootstraps the new pool's
    /// entry). Interpreter round i1 wave 23, lane L5.
    pub fn put_call_site_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        resolved: ResolvedCallSite,
        as_of: u64,
    ) -> bool {
        if !Self::fill_is_current(class_id, None, as_of) {
            return false;
        }
        self.put_call_site(class_id, cp_index, resolved);
        true
    }

    /// [`Self::put_condy`] for a resolution that took [`Self::fill_snapshot`]
    /// `as_of` before reading the constant pool; skipped (`false`) when a
    /// redefinition of `class_id` may have replaced that pool.
    pub fn put_condy_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        value: Value,
        as_of: u64,
    ) -> bool {
        if !Self::fill_is_current(class_id, None, as_of) {
            return false;
        }
        self.put_condy(class_id, cp_index, value);
        true
    }

    /// [`Self::put_permanent_constant`] for a resolution that took
    /// [`Self::fill_snapshot`] `as_of` before reading the constant pool.
    /// `None` (nothing recorded) when a redefinition of `class_id` may have
    /// replaced that pool: the caller hands out its own value to the execution
    /// that resolved the OLD pool's entry, and the new pool's entry is resolved
    /// afresh (HotSpot writes such a late result into the old pool's cache,
    /// which only obsolete frames read). Otherwise the value every resolver
    /// must hand out, as `put_permanent_constant` answers.
    pub fn put_permanent_constant_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        value: Value,
        as_of: u64,
    ) -> Option<Value> {
        if !Self::fill_is_current(class_id, None, as_of) {
            return None;
        }
        Some(self.put_permanent_constant(class_id, cp_index, value))
    }

    /// [`Self::put_failure`] for a resolution that took
    /// [`Self::fill_snapshot`] `as_of` before reading the constant pool;
    /// skipped (`false`) when a redefinition of `class_id` may have replaced
    /// that pool. JVMS §5.4.3 makes a failure permanent for the entry that
    /// failed; the new pool's entry at the same index is another entry, which
    /// a stale record would fail with an error about a class it never named.
    pub fn put_failure_as_of(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        failure: ResolutionFailure,
        as_of: u64,
    ) -> bool {
        if !Self::fill_is_current(class_id, None, as_of) {
            return false;
        }
        self.put_failure(class_id, cp_index, failure);
        true
    }

    /// Look up a cached call site (invokedynamic).
    pub fn get_call_site(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedCallSite> {
        self.call_sites
            .get(&(class_id, cp_index))
            .map(|site| &**site)
    }

    /// [`Self::get_call_site`] as the shared `Arc`, for a caller that must
    /// drop the cache guard before using the site.
    pub fn get_call_site_arc(
        &self,
        class_id: ClassId,
        cp_index: u16,
    ) -> Option<&Arc<ResolvedCallSite>> {
        self.call_sites.get(&(class_id, cp_index))
    }

    /// Cache a resolved call site.
    pub fn put_call_site(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedCallSite) {
        resolution_cache_insert(
            &mut self.call_sites,
            &mut self.call_sites_order,
            (class_id, cp_index),
            Arc::new(resolved),
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

    /// Look up a recorded constant-pool constant (either store: a CP index has
    /// exactly one tag, so the two cannot both hold a key).
    pub fn get_condy(&self, class_id: ClassId, cp_index: u16) -> Option<&Value> {
        let key = (class_id, cp_index);
        self.condy
            .get(&key)
            .or_else(|| self.permanent_constants.get(&key))
    }

    /// Record a constant that re-resolution reproduces identically. FIFO-capped
    /// (see [`RESOLUTION_CACHE_CAP`]); an entry already recorded in the
    /// permanent store is left alone.
    pub fn put_condy(&mut self, class_id: ClassId, cp_index: u16, value: Value) {
        if self.permanent_constants.contains_key(&(class_id, cp_index)) {
            return;
        }
        resolution_cache_insert(
            &mut self.condy,
            &mut self.condy_order,
            (class_id, cp_index),
            value,
        );
    }

    /// Record a constant whose re-resolution would not reproduce the same
    /// object (`CONSTANT_Dynamic`, `CONSTANT_MethodHandle`), and answer the
    /// value every resolver must hand out.
    ///
    /// Never evicted (the FIFO cap of [`Self::put_condy`] would re-run a
    /// bootstrap after eviction and hand out a second, non-identical object),
    /// and insert-if-absent: JVMS §5.4.3.6 makes the FIRST recorded result the
    /// one every racing resolver observes, so a loser gets the winner back
    /// instead of replacing it.
    pub fn put_permanent_constant(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        value: Value,
    ) -> Value {
        let key = (class_id, cp_index);
        // A recomputable record for the same key cannot exist (one tag per CP
        // index), but if one did the permanent record must be the only answer.
        if self.condy.remove(&key).is_some() {
            self.condy_order.retain(|k| *k != key);
        }
        *self.permanent_constants.entry(key).or_insert(value)
    }

    /// The number of permanent (never-evicted) constant records.
    pub fn permanent_constant_count(&self) -> usize {
        self.permanent_constants.len()
    }

    /// Scan cached CONSTANT_Dynamic values for GC roots.
    pub fn scan_condy_roots(&self, roots: &mut Vec<cratonvm_types::ObjectRef>) {
        for val in self.condy.values().chain(self.permanent_constants.values()) {
            if let Value::Object(Some(obj_ref)) = val {
                roots.push(*obj_ref);
            }
        }
    }

    /// Visit condy roots together with the referring class that owns the
    /// constant-pool cache entry. Loader unloading uses this to make the root
    /// conditional on defining-loader liveness.
    pub fn for_each_condy_root(&self, mut visit: impl FnMut(ClassId, cratonvm_types::ObjectRef)) {
        for (&(class_id, _), value) in self.condy.iter().chain(self.permanent_constants.iter()) {
            if let Value::Object(Some(object)) = value {
                visit(class_id, *object);
            }
        }
    }

    /// Update cached CONSTANT_Dynamic ObjectRefs after GC relocation.
    pub fn update_condy_refs(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        for val in self
            .condy
            .values_mut()
            .chain(self.permanent_constants.values_mut())
        {
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
        self.permanent_constants.clear();
        self.failures.clear();
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
    /// `classloading-verify-and-resolve.md`,
    /// "cross-owner requests". Left as-is deliberately rather than papered
    /// over here, because a same-crate change cannot make the field carry
    /// information the producer never wrote.
    pub fn invalidate_class(&mut self, class_id: ClassId) {
        self.forget_where(|id| id == class_id);
    }

    /// Drop only what a shifted instance-field layout of `class_id` makes
    /// stale: the field resolutions keyed on it or resolved into it (a
    /// `ResolvedField` carries a field index). Its constant pool is unchanged,
    /// so everything else keyed on it stays — in particular its recorded
    /// failures (JVMS §5.4.3: a failed entry fails the same way forever) and
    /// its permanent constants (a `CONSTANT_Dynamic` bootstrap must not run
    /// twice, a `CONSTANT_MethodHandle` must answer one handle; §5.4.3.6).
    /// Fired for each descendant `recompute_subclass_layouts` moved
    /// (`ResolutionInvalidation::FieldLayoutOnly`; interpreter round i1 wave
    /// 24, lane L5), which used to take [`Self::invalidate_class`] and lose
    /// both.
    ///
    /// Also dropped: a record `ObjectMethods` call site keyed on the class,
    /// which carries the record's field indices. Every other call site stays
    /// (a lambda site bootstrapped again would hand out a second non-capturing
    /// instance).
    pub fn invalidate_class_field_layout(&mut self, class_id: ClassId) {
        self.fields.retain(|(key_class, _), resolved| {
            *key_class != class_id && resolved.declaring_class_id != class_id
        });
        // A getter-driven site keyed on ANOTHER class holds the field slots of
        // its record class (`getter_driven`), so it goes when that record's
        // layout moves too (interpreter round i1 wave 44, lane L4; row 5 of
        // `i42-L4-objectmethods-getters-the-native-linkage-still-hands-to-the-jdk`).
        self.call_sites.retain(|(key_class, _), site| match **site {
            ResolvedCallSite::RecordObjectMethod { getter_driven, .. } => {
                *key_class != class_id && getter_driven != Some(class_id)
            }
            _ => true,
        });
        self.fields_order
            .retain(|key| self.fields.contains_key(key));
        self.call_sites_order
            .retain(|key| self.call_sites.contains_key(key));
    }

    /// Drop every record keyed on (or, for fields/methods, resolved into) a
    /// class of `is_gone`, with [`Self::invalidate_class`]'s rules. Called
    /// when those classes are UNLOADED.
    ///
    /// Load-bearing for GC safety, not only for memory: the constant records
    /// are loader-conditional roots ([`Self::for_each_condy_root`]). Once a
    /// class is unloaded its loader pin is removed, so a record left behind
    /// would be reported as an UNCONDITIONAL root for an object the unloading
    /// collection already reclaimed (the class's own `ldc X.class` mirror, a
    /// condy value only it referenced).
    pub fn forget_classes(&mut self, is_gone: impl Fn(ClassId) -> bool) {
        self.forget_where(is_gone);
    }

    fn forget_where(&mut self, is_gone: impl Fn(ClassId) -> bool) {
        // Fields: drop by key OR by resolved declaring class.
        self.fields.retain(|(key_class, _), resolved| {
            !is_gone(*key_class) && !is_gone(resolved.declaring_class_id)
        });
        // Methods: same two-pronged check. NB: prong 2 is currently a no-op —
        // see the doc comment above.
        self.methods.retain(|(key_class, _), resolved| {
            !is_gone(*key_class) && !is_gone(resolved.declaring_class_id)
        });
        // Call sites + condy: key match only. (Their resolved values
        // carry no reachable declaring-class link.)
        self.call_sites
            .retain(|(key_class, _), _| !is_gone(*key_class));
        self.condy.retain(|(key_class, _), _| !is_gone(*key_class));
        self.permanent_constants
            .retain(|(key_class, _), _| !is_gone(*key_class));
        // Recorded failures: key match only (the new constant pool may name a
        // different, resolvable class at the same index).
        self.failures
            .retain(|(key_class, _), _| !is_gone(*key_class));
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

    /// [`Self::invalidate_class`] for a whole set of classes, in ONE pass over
    /// the cache instead of one pass per class (gc-common w9-e,
    /// `docs/internal/gc-common-round-20260923/common-w8e-class-unload-sweeps-whole-tables-once-per-class-FIXED-20260926.md`).
    ///
    /// Evicts exactly what a loop of `invalidate_class` over `ids` evicts: an
    /// entry keyed on a class of the set, an entry resolved into one, and
    /// every negative. An empty set evicts nothing, as the empty loop does
    /// (the negatives go only because SOME class changed). Proven by
    /// `tests::invalidate_classes_matches_a_loop_of_invalidate_class`.
    ///
    /// The class-unload transaction (`vm::memory::gc::unload_dead_class_metadata`)
    /// is the caller: it unloads a whole loader's classes at once, inside the
    /// collection pause, and the per-class loop there cost O(classes x cache).
    pub fn invalidate_classes<S: std::hash::BuildHasher>(
        &self,
        ids: &std::collections::HashSet<ClassId, S>,
    ) {
        if ids.is_empty() {
            return;
        }
        let mut guard = self.cache.write();
        guard.retain(|(key_class, _, _), entry| {
            if ids.contains(key_class) {
                return false;
            }
            match &entry.value {
                ResolvedMember::Method {
                    declaring_class_id, ..
                }
                | ResolvedMember::Field {
                    declaring_class_id, ..
                } => !ids.contains(declaring_class_id),
                // Same rule as `invalidate_class`: a negative describes a walk
                // whose path this key does not record.
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

/// The C1→C2 supersede staleness gate of a [`CachedInvokeTarget::Jit`] entry:
/// a snapshot of the filling VM's supersede epoch (`JitRealm::supersede_epoch`
/// in the `vm` crate) and a live handle to it.
///
/// Per VM (interpreter round i1 wave 19). The epoch was a process static
/// until then, so one VM's C2 publish evicted every `Jit` entry of every
/// thread of every VM; `ClassId`s and compiled bodies are per VM, so only the
/// publishing VM's entries can hold the replaced body.
///
/// The handle is a `&'static` (the VM leaks its four-byte cell), not an
/// `Arc` like [`RedefineGate`]'s: every `Jit` entry of every thread of a VM
/// points at the same cell, and the invoke-cache hit arms clone the entry, so
/// an `Arc` would put one shared refcount line under every compiled call from
/// the interpreter. `Copy`, and one load to check.
#[derive(Clone, Copy)]
pub struct SupersedeGate {
    /// The epoch when the entry's artifact was read from the jit cache.
    pub epoch: u32,
    /// The filling VM's supersede epoch. Advanced with `AcqRel` after a
    /// replacing C2 body is published.
    pub counter: &'static AtomicU32,
}

/// The counter behind [`SupersedeGate::never_stale`]; nothing advances it.
static NEVER_SUPERSEDED: AtomicU32 = AtomicU32::new(0);

impl SupersedeGate {
    /// Snapshot `counter`'s current value. Take it BEFORE reading the
    /// artifact the entry will hold: a supersede racing in between then
    /// reads as stale (one extra re-resolve), never as a current C1 body.
    #[inline]
    pub fn snapshot(counter: &'static AtomicU32) -> Self {
        let epoch = counter.load(Ordering::Acquire);
        Self { epoch, counter }
    }

    /// A gate on a counter nothing advances (tests, fixtures).
    #[inline]
    pub fn never_stale() -> Self {
        Self::snapshot(&NEVER_SUPERSEDED)
    }

    /// Whether the VM published a replacing C2 body since the snapshot.
    #[inline]
    pub fn is_stale(&self) -> bool {
        self.counter.load(Ordering::Acquire) != self.epoch
    }
}

impl fmt::Debug for SupersedeGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SupersedeGate(epoch={})", self.epoch)
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
        /// The CALL SITE's descriptor, tokenised once at fill time.
        ///
        /// The dispatch arm used to recover the same two facts — each
        /// parameter's tag, and the return tag — by calling
        /// `resolve_method_ref(caller_class_id, cp_index)` on **every** call,
        /// which is a resolution-cache `RwLock` read, a hash probe and three
        /// `Arc<str>` clone/drop pairs, and then scanning the string it
        /// returned twice (`ParamTags::of`, `jit::return_type`). An inline
        /// cache entry is keyed by `(caller class, cp index)` and the promoted
        /// cross-thread entry by the same triple, so the call site's
        /// descriptor is a constant of the entry; this is that constant,
        /// built from the identical `resolve_method_metadata` result the fill
        /// already holds.
        facts: cratonvm_jit_api::DescriptorFacts,
        /// Whether the target is a static method (filled by `invokestatic`)
        /// or an instance one (filled by `invokespecial` or a private
        /// `invokevirtual`). An `invokestatic` and an `invokevirtual` naming
        /// the same `Methodref` share one cache key, so each hit arm refuses
        /// the other kind's entry — JVMS §6.5 makes one of them an
        /// `IncompatibleClassChangeError`, which only the slow path raises.
        is_static: bool,
        /// JVMS §2.11.10: the monitor the native must hold when the method it
        /// answers is `ACC_SYNCHRONIZED` (and the VM's native-sync switch was
        /// on at fill time) — `(is_static, declaring class)`: the receiver for
        /// an instance method, the declaring class's mirror for a static one.
        /// `None` for every other method. The hit arms take it around the call
        /// exactly as the slow path's native doors do.
        sync: Option<(bool, ClassId)>,
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
        /// The CALL SITE's descriptor, tokenised once at fill time. See the
        /// same field on [`CachedInvokeTarget::Native`].
        facts: cratonvm_jit_api::DescriptorFacts,
        /// The `ACC_SYNCHRONIZED` monitor fact — see the same field on
        /// [`CachedInvokeTarget::Native`] (always an instance method here, so
        /// the monitor is the receiver).
        sync: Option<(bool, ClassId)>,
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
        /// C1→C2 supersede gate: the filling VM's supersede epoch, snapshot
        /// before the artifact was read (see [`SupersedeGate`]). When that
        /// VM's background worker publishes an optimizing recompile that
        /// replaces a C1 body, it bumps its epoch; `is_stale` then reports
        /// this entry stale on its next hit, the cache self-evicts it, and
        /// re-resolution picks up the C2 artifact from the jit cache. The
        /// superseded artifact is retained (the entry owns it), so a
        /// not-yet-evicted entry is merely the slower C1 body — never a
        /// dangling pointer.
        supersede_epoch: SupersedeGate,
    },
    /// Interpreter intrinsic: a hot JDK method resolved once at IC-fill time
    /// to a direct intrinsic handler. Steady-state dispatch pays no native
    /// registry probe, no class-manager `RwLock`, and no descriptor parse.
    /// See `feature_roadmap_interpreter_intrinsic_table.md`.
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
            // A Jit entry is additionally stale once its VM has published a
            // C1→C2 supersede (that VM's epoch advanced): the cached
            // `Arc<CompiledMethod>` may be the replaced C1 body, so evict
            // and re-resolve from the jit cache. `InvokeCache::get` performs
            // this check on every hit and self-evicts, so no dispatch arm
            // needs supersede-specific handling.
            CachedInvokeTarget::Jit {
                gate,
                supersede_epoch,
                ..
            } => {
                if gate.is_stale() {
                    return true;
                }
                if supersede_epoch.is_stale() {
                    // Counted, not merely returned: the epoch is per VM, but
                    // one C2 publish still evicts every `Jit` entry at every
                    // call site in every thread of that VM. Whether that is
                    // worth avoiding is a question about how many entries it
                    // actually throws away, and nothing was measuring that.
                    // Relaxed, and only on the stale path.
                    EPOCH_STALE_EVICTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return true;
                }
                false
            }
        }
    }
}

/// Invoke-cache entries reported stale because the C1→C2 supersede epoch moved.
static EPOCH_STALE_EVICTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many `Jit` invoke-cache entries the supersede epoch has invalidated.
pub fn epoch_stale_evictions() -> u64 {
    EPOCH_STALE_EVICTIONS.load(std::sync::atomic::Ordering::Relaxed)
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
type InvokeCacheKey = (ClassId, u16, bool, u32);

/// `CRATONVM_INVOKE_CACHE_PC_KEY=1` adds the call site's bytecode offset to
/// the invoke-cache key.
///
/// # Why the cp index alone is not a call site
///
/// The key is `(caller class, cp index, is_special)`, which names the
/// METHOD REFERENCE, not the place that calls it. Two call sites in one
/// class that invoke the same method share a constant-pool entry and
/// therefore share one cache entry. For invokestatic that is harmless --
/// the target is the same either way -- but it makes two distinct sites
/// indistinguishable to anything that wants to reason about a SITE:
///
/// * the GPU offload hook keeps an eligible site out of this cache so a
///   later call with bigger arrays can still offload. Giving up on one site
///   that always passes small arrays therefore silently disables offload
///   for a sibling site calling the same kernel with big ones -- measured
///   at an 18x regression on `GpuHookOverheadBench` before this existed;
/// * `poly_entries` is a per-site inline cache. Two virtual sites sharing a
///   cp index share one 8-entry list, so a site seeing one receiver class
///   can be evicted by a sibling seeing eight others.
///
/// Off by default until measured: this is the interpreter's hottest map,
/// and adding a field to its key costs hash and space on every invoke.
/// `evict` stays reference-scoped in both modes (it drops every pc for the
/// reference), because eviction answers "this target is stale", which is a
/// property of the target and not of the site that found it.
pub fn pc_key_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_INVOKE_CACHE_PC_KEY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// The pc component of a key: the real offset when pc-keying is on, and a
/// constant `0` when it is off, which reproduces the pre-2026-09-03 key
/// exactly rather than approximating it.
#[inline]
fn site_pc(pc: u32) -> u32 {
    if pc_key_enabled() {
        pc
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Hit census — the only way to know the pc threading is RIGHT
// ---------------------------------------------------------------------------
//
// Adding a component to this key has a failure mode that does not look like a
// bug: if a site's `put` and its `get` are handed different pcs -- the invoke's
// own offset in one and the already-advanced `pc` in the other -- the entry is
// stored under one key and looked for under another, every lookup misses, and
// the only symptom is that the interpreter got slower. A correctness test
// cannot see it, because missing the cache is always SAFE: the slow path
// recomputes the same answer.
//
// So the hit rate is the acceptance criterion for the change, not a nicety.
// Turning pc-keying on must leave it essentially unchanged.

static INVOKE_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INVOKE_CACHE_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `CRATONVM_INVOKE_CACHE_STATS=1` counts invoke-cache hits and misses.
///
/// Off by default and read once: this is the hottest map in the interpreter
/// and two relaxed increments per invoke are not free.
fn invoke_cache_stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_INVOKE_CACHE_STATS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

#[inline]
fn note_invoke_lookup(hit: bool) {
    if !invoke_cache_stats_enabled() {
        return;
    }
    let c = if hit {
        &INVOKE_CACHE_HITS
    } else {
        &INVOKE_CACHE_MISSES
    };
    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// One line at exit when `CRATONVM_INVOKE_CACHE_STATS=1` asked for it.
pub fn invoke_cache_stats_summary() {
    use std::sync::atomic::Ordering;
    let hits = INVOKE_CACHE_HITS.load(Ordering::Relaxed);
    let misses = INVOKE_CACHE_MISSES.load(Ordering::Relaxed);
    if hits + misses == 0 {
        return;
    }
    eprintln!(
        "[cratonvm] invoke cache: lookups={} hits={hits} misses={misses} \
         ({:.2}% hit); pc-keyed={}",
        hits + misses,
        100.0 * hits as f64 / (hits + misses) as f64,
        pc_key_enabled(),
    );
    let (scoped, full, dropped) = invoke_cache_redefinition_retirements();
    if scoped + full > 0 {
        eprintln!(
            "[cratonvm] invoke cache redefinition retirements: \
             class-scoped={scoped} (entries dropped={dropped}) full={full}"
        );
    }
}

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
    /// [`crate::class_redefinition_count`] as of this cache's last lookup:
    /// every entry in both maps was filled while the count still read this.
    ///
    /// The target side of an entry has its [`RedefineGate`]; this is the
    /// CALLER side. A redefinition replaces the caller's constant pool under
    /// the same `ClassId`, so the key `(caller, cp index, ..)` may name another
    /// member afterwards, and nothing about the TARGET changed to say so
    /// (interpreter round i1 wave 18, lane L4: the general dispatchers served
    /// such an entry; the fast doors were kept off it only by switching
    /// themselves off for the process on the first redefinition anywhere).
    /// A lookup that sees the count moved drops both maps once, and a fill
    /// that straddles a move is not stored. See
    /// [`INVOKE_CACHE_RETIRES_REDEFINED_CALLERS`].
    redefinitions_seen: u64,
}

/// Kill switch for the caller-side retirement of [`InvokeCache`] entries
/// (interpreter round i1 wave 18, lane L4). `true`: a lookup after any class
/// redefinition drops this thread's entries once and a fill that straddles a
/// redefinition is dropped. `false`: the pre-wave-18 behaviour, where only
/// the target's [`RedefineGate`] retired an entry.
pub const INVOKE_CACHE_RETIRES_REDEFINED_CALLERS: bool = true;

/// Kill switch for the class-scoped arm of that retirement (interpreter round
/// i1 wave 20, lane L2, stage 2 of
/// `interpreter-L4-proposal-class-scoped-invoke-cache-retirement-FIXED-20260926`). `true`: a lookup
/// after redefinitions drops only the entries whose CALLER is a redefined
/// class, read from the redefinition ring
/// ([`crate::for_each_redefined_class_between`]); when the ring cannot
/// answer, everything, as before. `false`: every redefinition drops every
/// entry. The target side is each entry's [`RedefineGate`] either way.
///
/// Why the caller alone: a redefinition may change method bodies, the
/// constant pool and the bytecode offsets, never the method set, the
/// modifiers or the hierarchy (`redefine_class` refuses those). So a key
/// `(D, cp index, ..)` of an unredefined `D` still resolves to the same
/// declaring class and method, whose body change its gate reports. The one
/// target that holds more than that method's body is a `Jit` entry (its
/// compiled code may have inlined any class), so those all go.
pub const INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS: bool = true;

/// Redefinition retirements an [`InvokeCache`] made: class-scoped, full, and
/// the entries the class-scoped ones dropped. Cold path; reported by
/// [`invoke_cache_stats_summary`].
static INVOKE_CACHE_SCOPED_RETIREMENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static INVOKE_CACHE_FULL_RETIREMENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static INVOKE_CACHE_SCOPED_DROPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(class-scoped, full, entries dropped by the class-scoped)` retirements
/// so far, process-wide.
pub fn invoke_cache_redefinition_retirements() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        INVOKE_CACHE_SCOPED_RETIREMENTS.load(Relaxed),
        INVOKE_CACHE_FULL_RETIREMENTS.load(Relaxed),
        INVOKE_CACHE_SCOPED_DROPS.load(Relaxed),
    )
}

/// The redefinition count an [`InvokeCache`] compares against. In this
/// crate's own unit tests it is a per-thread stand-in, so that the
/// redefinitions other tests of this crate perform (`redefine_class`
/// advances the real counter) can neither drop a fill nor empty a cache a
/// test is asserting about; every other build reads
/// [`crate::class_redefinition_count`].
#[cfg(not(test))]
#[inline(always)]
fn invoke_cache_redefinition_count() -> u64 {
    crate::class_manager::class_redefinition_count()
}

#[cfg(test)]
#[inline(always)]
fn invoke_cache_redefinition_count() -> u64 {
    tests_redefinition_count::get()
}

/// The classes of the redefinitions counted in `(after, upto]`, from the
/// ring [`invoke_cache_redefinition_count`] pairs with; `false` when it
/// cannot answer. See [`crate::for_each_redefined_class_between`].
#[cfg(not(test))]
fn invoke_cache_redefined_classes(after: u64, upto: u64, visit: impl FnMut(ClassId)) -> bool {
    crate::class_manager::for_each_redefined_class_between(after, upto, visit)
}

#[cfg(test)]
fn invoke_cache_redefined_classes(after: u64, upto: u64, visit: impl FnMut(ClassId)) -> bool {
    tests_redefinition_count::for_each_between(after, upto, visit)
}

#[cfg(test)]
mod tests_redefinition_count {
    use super::ClassId;
    use std::cell::{Cell, RefCell};
    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
        /// `(count, class)` per bump; `None` for a bump that named no class.
        static RECORDS: RefCell<Vec<(u64, Option<ClassId>)>> = const { RefCell::new(Vec::new()) };
    }
    pub(super) fn get() -> u64 {
        COUNT.with(Cell::get)
    }
    /// What an unattributed bump does to the real counter, on this thread
    /// only: its record retires everything.
    pub(super) fn bump() {
        record(None);
    }
    /// What `redefine_class` of `class` does to the real counter and ring,
    /// on this thread only.
    pub(super) fn bump_of(class: ClassId) {
        record(Some(class));
    }
    fn record(class: Option<ClassId>) {
        let next = get() + 1;
        RECORDS.with(|r| r.borrow_mut().push((next, class)));
        COUNT.with(|c| c.set(next));
    }
    /// Forget the records older than the last `keep` counts: what the real
    /// ring does once it laps a reader.
    pub(super) fn lap(keep: u64) {
        let floor = get().saturating_sub(keep);
        RECORDS.with(|r| r.borrow_mut().retain(|(count, _)| *count > floor));
    }
    pub(super) fn for_each_between(after: u64, upto: u64, mut visit: impl FnMut(ClassId)) -> bool {
        RECORDS.with(|r| {
            let records = r.borrow();
            for count in after + 1..=upto {
                match records.iter().find(|(c, _)| *c == count) {
                    Some((_, Some(class))) => visit(*class),
                    _ => return false,
                }
            }
            true
        })
    }
}

impl<JitMethod: Clone> InvokeCache<JitMethod> {
    pub fn new() -> Self {
        Self {
            entries: fx_hashmap_with_capacity(64),
            poly_entries: fx_hashmap_with_capacity(0),
            redefinitions_seen: invoke_cache_redefinition_count(),
        }
    }

    /// The redefinition count every entry was filled under (see the field).
    /// A cache shared with other threads that is filled from this thread's
    /// resolution (the promoted-invoke map) stamps its entry with this, a
    /// snapshot taken before that resolution began.
    #[inline]
    pub fn redefinitions_seen(&self) -> u64 {
        self.redefinitions_seen
    }

    /// Drop every entry once the redefinition count has moved since the last
    /// lookup. One `Acquire` load and a compare when nothing was redefined.
    #[inline(always)]
    fn retire_after_redefinition(&mut self) {
        if !INVOKE_CACHE_RETIRES_REDEFINED_CALLERS {
            return;
        }
        let now = invoke_cache_redefinition_count();
        if now != self.redefinitions_seen {
            self.flush_for_redefinition(now);
        }
    }

    /// Retire what the redefinitions since the last lookup may have made
    /// wrong: the entries whose caller is a redefined class, or every entry
    /// when the ring cannot name the classes (see
    /// [`INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS`]).
    #[cold]
    #[inline(never)]
    fn flush_for_redefinition(&mut self, now: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        if INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS {
            let mut callers: Vec<ClassId> = Vec::new();
            let named = invoke_cache_redefined_classes(self.redefinitions_seen, now, |class| {
                if !callers.contains(&class) {
                    callers.push(class);
                }
            });
            if named {
                // A `Jit` entry goes whatever its caller: its compiled body
                // may have inlined a redefined class's method, which no gate
                // of the entry watches, and the JIT withdraws such bodies
                // under the same redefinition (the full flush dropped them).
                let is_jit = |target: &CachedInvokeTarget<JitMethod>| {
                    matches!(target, CachedInvokeTarget::Jit { .. })
                };
                let before = self.entries.len() + self.poly_entries.len();
                self.entries
                    .retain(|key, target| !callers.contains(&key.0) && !is_jit(target));
                self.poly_entries.retain(|key, site| {
                    site.retain(|(_, target)| !is_jit(target));
                    !callers.contains(&key.0) && !site.is_empty()
                });
                let dropped = before.saturating_sub(self.entries.len() + self.poly_entries.len());
                INVOKE_CACHE_SCOPED_RETIREMENTS.fetch_add(1, Relaxed);
                INVOKE_CACHE_SCOPED_DROPS.fetch_add(dropped as u64, Relaxed);
                self.redefinitions_seen = now;
                return;
            }
        }
        INVOKE_CACHE_FULL_RETIREMENTS.fetch_add(1, Relaxed);
        self.entries.clear();
        self.poly_entries.clear();
        self.redefinitions_seen = now;
    }

    /// Whether a fill may be stored: no redefinition has run since the last
    /// lookup, which preceded the resolution the fill carries. A fill that
    /// straddles one is dropped; the next lookup misses and re-resolves.
    ///
    /// `as_of` is the count the resolution began under (a
    /// [`Self::redefinitions_seen`] snapshot taken before it read the
    /// caller's constant pool; [`Self::put`] passes the field itself). The
    /// count must still read `as_of` AND this cache's last lookup must have
    /// seen it: a lookup
    /// made by Java the resolution ran on this thread (a class loader, a
    /// `<clinit>`) after a redefinition moves `redefinitions_seen` up to the
    /// new count, which alone would admit a fill resolved against the old
    /// pool (interpreter round i1 wave 19, lane L4).
    #[inline]
    fn fill_admitted_as_of(&self, as_of: u64) -> bool {
        if !INVOKE_CACHE_RETIRES_REDEFINED_CALLERS {
            return true;
        }
        let now = invoke_cache_redefinition_count();
        now == self.redefinitions_seen && now == as_of
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
        pc: u32,
    ) -> Option<&CachedInvokeTarget<JitMethod>> {
        // Caller side first: a redefinition anywhere drops the maps once.
        self.retire_after_redefinition();
        // WP2.4-F1: O(1) generation check on hit, folded into the same probe.
        let found = self
            .entries
            .get(&(caller_class, cp_index, is_special, site_pc(pc)))
            .filter(|t| !t.is_stale());
        note_invoke_lookup(found.is_some());
        found
    }

    pub fn put(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        pc: u32,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        let as_of = self.redefinitions_seen;
        self.put_as_of(caller_class, cp_index, is_special, pc, as_of, target);
    }

    /// [`Self::put`] for a fill whose resolution may run Java on this thread
    /// between the lookup that missed and this store: `as_of` is
    /// [`Self::redefinitions_seen`] read before the resolution began, and the
    /// fill is dropped unless the redefinition count still reads it. See
    /// [`Self::fill_admitted_as_of`].
    pub fn put_as_of(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        pc: u32,
        as_of: u64,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        if !self.fill_admitted_as_of(as_of) {
            return;
        }
        self.entries
            .insert((caller_class, cp_index, is_special, site_pc(pc)), target);
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
        pc: u32,
        receiver_class: ClassId,
    ) -> Option<CachedInvokeTarget<JitMethod>> {
        self.retire_after_redefinition();
        let key = (caller_class, cp_index, is_special, site_pc(pc));
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
        pc: u32,
        receiver_class: ClassId,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        let as_of = self.redefinitions_seen;
        self.put_poly_as_of(
            caller_class,
            cp_index,
            is_special,
            pc,
            receiver_class,
            as_of,
            target,
        );
    }

    /// [`Self::put_poly`] with a fill-time snapshot; see [`Self::put_as_of`].
    #[allow(clippy::too_many_arguments)]
    pub fn put_poly_as_of(
        &mut self,
        caller_class: ClassId,
        cp_index: u16,
        is_special: bool,
        pc: u32,
        receiver_class: ClassId,
        as_of: u64,
        target: CachedInvokeTarget<JitMethod>,
    ) {
        if !self.fill_admitted_as_of(as_of) {
            return;
        }
        let key = (caller_class, cp_index, is_special, site_pc(pc));
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

    /// Drop every `Jit` entry whose artifact `keep` rejects, in both maps, and
    /// return how many went. Every other target kind is kept.
    ///
    /// For releasing compiled code the cache would otherwise pin. A `Jit`
    /// entry OWNS its artifact, and it is dropped only when its own call site
    /// runs again (a stale hit heals by `put`). A site that never runs again
    /// therefore keeps a withdrawn body mapped for the life of the thread, and
    /// the code-cache sweeper exists precisely for code that does not run
    /// again. The RT-8 soak (2026-09-18) measured it: a sweep withdrew
    /// 1,040,800 bytes and not one came back, because the interpreted callers
    /// of every withdrawn body still held it here.
    pub fn retain_jit(&mut self, mut keep: impl FnMut(&JitMethod) -> bool) -> usize {
        let mut removed = 0;
        let mut keep_target = |target: &CachedInvokeTarget<JitMethod>| match target {
            CachedInvokeTarget::Jit { compiled, .. } => {
                let kept = keep(compiled);
                if !kept {
                    removed += 1;
                }
                kept
            }
            _ => true,
        };
        self.entries.retain(|_, target| keep_target(target));
        self.poly_entries.retain(|_, site| {
            site.retain(|(_, target)| keep_target(target));
            !site.is_empty()
        });
        removed
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
    /// Drop every cached target for this method REFERENCE.
    ///
    /// Reference-scoped, not site-scoped, in both key modes: eviction says
    /// "this target is stale" (a redefined class, a failed initialization),
    /// which is a property of the target rather than of whichever site
    /// noticed. With pc-keying on there may be several sites holding it, so
    /// the map is scanned; eviction is a cold path (six callers, all on
    /// staleness or error) and correctness here outranks its cost.
    pub fn evict(&mut self, caller_class: ClassId, cp_index: u16, is_special: bool) {
        if !pc_key_enabled() {
            let key = (caller_class, cp_index, is_special, 0);
            self.entries.remove(&key);
            self.poly_entries.remove(&key);
            return;
        }
        let matches =
            |k: &InvokeCacheKey| k.0 == caller_class && k.1 == cp_index && k.2 == is_special;
        self.entries.retain(|k, _| !matches(k));
        self.poly_entries.retain(|k, _| !matches(k));
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

    /// JVMS §5.4.3 failure record: first record wins, keyed per
    /// (class, cp index), dropped by `invalidate_class` and `clear`.
    #[test]
    fn resolution_failure_first_record_wins_and_is_invalidated() {
        let mut cache = ResolutionCache::new();
        let a = ClassId::new(7);
        let b = ClassId::new(8);
        let ncdfe = ResolutionFailure {
            error_class: Arc::from("java/lang/NoClassDefFoundError"),
            message: Some(Arc::from("p/Opt")),
            cause: None,
        };
        assert!(cache.get_failure(a, 3).is_none());
        cache.put_failure(a, 3, ncdfe.clone());
        cache.put_failure(
            a,
            3,
            ResolutionFailure {
                error_class: Arc::from("java/lang/IllegalAccessError"),
                message: None,
                cause: None,
            },
        );
        cache.put_failure(b, 3, ncdfe.clone());
        assert_eq!(cache.get_failure(a, 3), Some(&ncdfe));
        assert!(cache.get_failure(a, 4).is_none());
        assert_eq!(cache.failure_count(), 2);
        cache.invalidate_class(a);
        assert!(cache.get_failure(a, 3).is_none());
        assert_eq!(cache.get_failure(b, 3), Some(&ncdfe));
        cache.clear();
        assert_eq!(cache.failure_count(), 0);
    }

    /// Interpreter round i1 wave 24, lane L5: a field-layout shift of a class
    /// whose constant pool did not change drops its field resolutions (keyed
    /// on it, or resolved into it from another class) and its record
    /// `ObjectMethods` site, and keeps its recorded failure, its permanent
    /// constant and its other call sites.
    #[test]
    fn a_field_layout_shift_keeps_failures_and_permanent_constants() {
        let mut cache = ResolutionCache::new();
        let moved = ClassId::new(7);
        let other = ClassId::new(8);
        let field_into = |declaring: ClassId| ResolvedField {
            declaring_class_id: declaring,
            field_index: 3,
            is_static: false,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        };
        let ncdfe = ResolutionFailure {
            error_class: Arc::from("java/lang/NoClassDefFoundError"),
            message: Some(Arc::from("p/Opt")),
            cause: None,
        };
        cache.put_field(moved, 1, field_into(other));
        cache.put_field(other, 1, field_into(moved));
        cache.put_field(other, 2, field_into(other));
        cache.put_failure(moved, 3, ncdfe.clone());
        cache.put_permanent_constant(moved, 4, Value::Int(44));
        cache.put_call_site(
            moved,
            5,
            ResolvedCallSite::RecordObjectMethod {
                method: RecordMethodKind::HashCode,
                component_names: vec![Arc::from("x")],
                field_indices: vec![0],
                field_descriptors: vec![Arc::from("I")],
                getter_driven: None,
                accessor_getters: Vec::new(),
            },
        );
        cache.put_call_site(moved, 6, ResolvedCallSite::EnumSwitch { labels: Vec::new() });

        cache.invalidate_class_field_layout(moved);

        assert!(cache.get_field(moved, 1).is_none(), "keyed on the moved class");
        assert!(cache.get_field(other, 1).is_none(), "resolved into the moved class");
        assert!(cache.get_field(other, 2).is_some(), "unrelated field kept");
        assert_eq!(cache.get_failure(moved, 3), Some(&ncdfe), "JVMS 5.4.3");
        assert!(
            matches!(cache.get_condy(moved, 4), Some(Value::Int(44))),
            "JVMS 5.4.3.6"
        );
        assert!(cache.get_call_site(moved, 5).is_none(), "record site carries field indices");
        assert!(cache.get_call_site(moved, 6).is_some(), "other call sites kept");
        assert_eq!(cache.field_count(), 1);
        assert_eq!(cache.call_site_count(), 1);
    }

    /// Interpreter round i1 wave 44, lane L4: a getter-driven record
    /// `ObjectMethods` site keyed on ANOTHER class (the class holding the
    /// `invokedynamic`) holds the record's field slots, so a layout shift of
    /// the record drops it; a site driven by another record stays.
    #[test]
    fn a_record_layout_shift_drops_getter_driven_sites_keyed_elsewhere() {
        let mut cache = ResolutionCache::new();
        let record = ClassId::new(7);
        let holder = ClassId::new(8);
        let other_record = ClassId::new(9);
        let site = |driven: ClassId| ResolvedCallSite::RecordObjectMethod {
            method: RecordMethodKind::HashCode,
            component_names: vec![Arc::from("x")],
            field_indices: vec![0],
            field_descriptors: vec![Arc::from("I")],
            getter_driven: Some(driven),
            accessor_getters: Vec::new(),
        };
        cache.put_call_site(holder, 1, site(record));
        cache.put_call_site(holder, 2, site(other_record));

        cache.invalidate_class_field_layout(record);

        assert!(cache.get_call_site(holder, 1).is_none(), "holds the moved record's slots");
        assert!(cache.get_call_site(holder, 2).is_some(), "another record's site kept");
        assert_eq!(cache.call_site_count(), 1);
    }

    /// A permanent constant record (condy / MethodHandle) survives the FIFO
    /// eviction of the recomputable records, and the first record wins.
    #[test]
    fn permanent_constant_survives_fifo_eviction_and_first_record_wins() {
        let mut cache = ResolutionCache::new();
        let condy_owner = ClassId::new(3);
        assert_eq!(
            cache.put_permanent_constant(condy_owner, 9, Value::Int(1)),
            Value::Int(1)
        );
        // A losing racer gets the winner back and does not replace it.
        assert_eq!(
            cache.put_permanent_constant(condy_owner, 9, Value::Int(2)),
            Value::Int(1)
        );
        // Push the recomputable store past its cap.
        for i in 0..=RESOLUTION_CACHE_CAP {
            // Cast: test-only; `i` is at most 2^16, so both halves fit.
            let (class, cp) = (ClassId::new(100 + (i >> 16) as u32), (i & 0xFFFF) as u16);
            cache.put_condy(class, cp, Value::Int(i as i32));
        }
        assert!(cache.condy.len() <= RESOLUTION_CACHE_CAP);
        assert_eq!(cache.get_condy(condy_owner, 9), Some(&Value::Int(1)));
        assert_eq!(cache.permanent_constant_count(), 1);
        // A recomputable record cannot shadow a permanent one.
        cache.put_condy(condy_owner, 9, Value::Int(7));
        assert_eq!(cache.get_condy(condy_owner, 9), Some(&Value::Int(1)));
    }

    /// Unloading drops every record keyed on the dead classes — the constant
    /// records are loader-conditional GC roots and must not outlive the pin.
    #[test]
    fn forget_classes_drops_constant_and_failure_records() {
        let mut cache = ResolutionCache::new();
        let dead = ClassId::new(11);
        let live = ClassId::new(12);
        cache.put_condy(dead, 1, Value::Int(1));
        cache.put_permanent_constant(dead, 2, Value::Int(2));
        cache.put_permanent_constant(live, 2, Value::Int(3));
        cache.put_failure(
            dead,
            5,
            ResolutionFailure {
                error_class: Arc::from("java/lang/NoClassDefFoundError"),
                message: None,
                cause: None,
            },
        );
        cache.forget_classes(|id| id == dead);
        assert!(cache.get_condy(dead, 1).is_none());
        assert!(cache.get_condy(dead, 2).is_none());
        assert!(cache.get_failure(dead, 5).is_none());
        assert_eq!(cache.get_condy(live, 2), Some(&Value::Int(3)));
        let mut visited = Vec::new();
        cache.for_each_condy_root(|id, _| visited.push(id));
        assert!(visited.is_empty(), "no object values were recorded");
        // `invalidate_class` shares the rules.
        cache.invalidate_class(live);
        assert_eq!(cache.permanent_constant_count(), 0);
    }

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
                is_final: false,
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
                is_final: false,
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
                is_final: false,
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
                is_final: false,
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
                is_final: false,
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
            Arc::new(CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
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
                },
            ))
        };

        // invokespecial entry: points at the interface default method itself.
        // One `pc` for both entries on purpose: this test is about `is_special`
        // disambiguating two entries at the SAME site, and `pc` became a key
        // dimension in `6e392453f`. Varying it here would make them differ for a
        // reason the test is not about, and the assertions below would then hold
        // whether or not `is_special` was doing anything.
        const SITE_PC: u32 = 0;
        cache.put(
            caller,
            cp_index,
            true,
            SITE_PC,
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
            SITE_PC,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(42),
                cached: make_cached("scala/collection/AbstractIterable"),
                gate: RedefineGate::never_stale(),
            },
        );

        // The two entries must NOT alias each other.
        let special = cache
            .get(caller, cp_index, true, SITE_PC)
            .expect("special entry");
        match special {
            CachedInvokeTarget::Bytecode { cached: m, .. } => {
                assert_eq!(&*m.class_name, "scala/collection/IterableOnceOps");
            }
            other => panic!("expected Bytecode for invokespecial, got {other:?}"),
        }
        let virt = cache
            .get(caller, cp_index, false, SITE_PC)
            .expect("virtual entry");
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
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ));
        let entry = CachedInvokeTarget::Bytecode {
            cached,
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
        };
        cache.put(caller, cp_index, false, 0, entry);

        // First hit — counter unchanged, must return Some.
        assert!(cache.get(caller, cp_index, false, 0).is_some());

        // Simulate `redefine_class` step 7 — bump the live counter.
        counter.fetch_add(1, Ordering::Release);

        // Next hit — entry is stale, getter must auto-evict and return None.
        assert!(
            cache.get(caller, cp_index, false, 0).is_none(),
            "stale entry must be evicted after redefine bump"
        );

        // Subsequent populate with a fresh snapshot succeeds.
        let new_cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ));
        cache.put(
            caller,
            cp_index,
            false,
            0,
            CachedInvokeTarget::Bytecode {
                cached: new_cached,
                gate: RedefineGate::snapshot(Arc::clone(&counter)),
            },
        );
        let hit = cache
            .get(caller, cp_index, false, 0)
            .expect("fresh entry hit");
        match hit {
            CachedInvokeTarget::Bytecode { cached, .. } => {
                // The bipush byte for 99 (0x63) confirms we got the new body.
                assert_eq!(cached.code[1], 0x63);
            }
            _ => panic!("expected Bytecode entry"),
        }
    }

    /// `retain_jit` drops exactly the `Jit` entries its predicate rejects, in
    /// the monomorphic map AND the polymorphic overflow, and nothing else.
    /// It exists so the code-cache sweeper can release bodies that a call
    /// site which never runs again would otherwise pin for good; a version
    /// that skipped the poly map would leave every megamorphic site pinning.
    #[test]
    fn retain_jit_drops_only_rejected_jit_entries_in_both_maps() {
        use std::sync::Arc;
        let counter = Arc::new(AtomicU32::new(0));
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(99),
                class_name: Arc::from("p/Target"),
                method_name: Arc::from("answer"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]),
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        // The artifact is a plain id here: `retain_jit` is generic over it.
        let jit = |id: u32| CachedInvokeTarget::Jit {
            compiled: id,
            num_params: 0,
            return_type: b'I',
            needs_heap: false,
            cached: Arc::clone(&cached),
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
            supersede_epoch: SupersedeGate::never_stale(),
        };
        let bytecode = CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
        };
        let caller = ClassId::new(7);
        let mut cache = InvokeCache::<u32>::new();
        cache.put(caller, 1, false, 0, jit(1)); // withdrawn
        cache.put(caller, 2, false, 0, jit(2)); // live
        cache.put(caller, 3, false, 0, bytecode);
        cache.put_poly(caller, 4, false, 0, ClassId::new(40), jit(1)); // withdrawn
        cache.put_poly(caller, 4, false, 0, ClassId::new(41), jit(2)); // live
        cache.put_poly(caller, 5, false, 0, ClassId::new(50), jit(1)); // withdrawn, alone

        let removed = cache.retain_jit(|id| *id != 1);

        assert_eq!(
            removed, 3,
            "the three entries naming artifact 1, and no others"
        );
        assert!(cache.get(caller, 1, false, 0).is_none());
        assert!(
            cache.get(caller, 2, false, 0).is_some(),
            "a live body stays cached"
        );
        assert!(
            cache.get(caller, 3, false, 0).is_some(),
            "non-JIT targets are untouched"
        );
        assert!(cache
            .get_poly(caller, 4, false, 0, ClassId::new(40))
            .is_none());
        assert!(cache
            .get_poly(caller, 4, false, 0, ClassId::new(41))
            .is_some());
        assert!(cache
            .get_poly(caller, 5, false, 0, ClassId::new(50))
            .is_none());
    }

    /// Interpreter round i1 wave 19, lane L5: a `Jit` entry goes stale on its
    /// own VM's supersede epoch only. Two counters stand for two VMs; the one
    /// that advances evicts its entry and leaves the other's hit.
    #[test]
    fn a_jit_entry_goes_stale_on_its_own_vms_supersede_epoch_only() {
        use std::sync::Arc;
        let redefine = Arc::new(AtomicU32::new(0));
        // Each VM leaks its cell; so does this test.
        let epoch_a: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(5)));
        let epoch_b: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(5)));
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(19),
                class_name: Arc::from("p/Hot"),
                method_name: Arc::from("run"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]),
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let jit = |epoch: &'static AtomicU32| CachedInvokeTarget::Jit {
            compiled: 1u32,
            num_params: 0,
            return_type: b'I',
            needs_heap: false,
            cached: Arc::clone(&cached),
            gate: RedefineGate::snapshot(Arc::clone(&redefine)),
            supersede_epoch: SupersedeGate::snapshot(epoch),
        };
        let caller = ClassId::new(7);
        let mut in_a = InvokeCache::<u32>::new();
        let mut in_b = InvokeCache::<u32>::new();
        in_a.put(caller, 1, false, 0, jit(epoch_a));
        in_b.put(caller, 1, false, 0, jit(epoch_b));
        let evictions = epoch_stale_evictions();

        // VM A publishes a replacing C2 body.
        epoch_a.fetch_add(1, Ordering::AcqRel);

        assert!(
            in_b.get(caller, 1, false, 0).is_some(),
            "another VM's C2 publish leaves this VM's entry current"
        );
        assert!(
            in_a.get(caller, 1, false, 0).is_none(),
            "the publishing VM's entry may hold the replaced body"
        );
        assert!(epoch_stale_evictions() > evictions, "counted");
        assert!(!SupersedeGate::never_stale().is_stale());
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

    /// gc-common w9-e: the batch form evicts exactly what a loop of the
    /// per-class form evicts, for every subset of a mixed cache (keyed on a
    /// gone class, resolved into one, both, neither, and negatives), and an
    /// empty set evicts nothing.
    #[test]
    fn invalidate_classes_matches_a_loop_of_invalidate_class() {
        fn fill(resolver: &LinkResolver) {
            let desc: Arc<str> = Arc::from("()V");
            for key in 1..=6u32 {
                for decl in 1..=6u32 {
                    resolver.insert(
                        ClassId::new(key),
                        Arc::from(format!("m{decl}")),
                        Arc::clone(&desc),
                        ResolvedMember::Method {
                            declaring_class_id: ClassId::new(decl),
                            index: decl,
                        },
                    );
                    resolver.insert(
                        ClassId::new(key),
                        Arc::from(format!("f{decl}")),
                        Arc::from(""),
                        ResolvedMember::Field {
                            declaring_class_id: ClassId::new(decl),
                            absolute_index: decl,
                            is_static: decl % 2 == 0,
                        },
                    );
                }
                resolver.insert(
                    ClassId::new(key),
                    Arc::from("missing"),
                    Arc::clone(&desc),
                    ResolvedMember::NotFound,
                );
            }
        }
        fn keys(resolver: &LinkResolver) -> Vec<(u32, String, String)> {
            let mut out: Vec<(u32, String, String)> = resolver
                .cache
                .read()
                .keys()
                .map(|(c, n, d)| (c.as_u32(), n.to_string(), d.to_string()))
                .collect();
            out.sort();
            out
        }

        let sets: [&[u32]; 5] = [&[], &[3], &[2, 5], &[1, 2, 3, 4, 5, 6], &[99]];
        for set in sets {
            let looped = LinkResolver::new();
            fill(&looped);
            for &id in set {
                looped.invalidate_class(ClassId::new(id));
            }
            let batched = LinkResolver::new();
            fill(&batched);
            let ids: std::collections::HashSet<ClassId> =
                set.iter().map(|&id| ClassId::new(id)).collect();
            batched.invalidate_classes(&ids);
            assert_eq!(
                keys(&batched),
                keys(&looped),
                "batch and loop disagree for the set {set:?}"
            );
            if set.is_empty() {
                assert_eq!(batched.len(), 6 * 13, "an empty set evicts nothing");
            }
        }
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

    fn i18_l4_static_target(declaring: u32) -> CachedInvokeTarget<()> {
        use std::sync::Arc;
        CachedInvokeTarget::Bytecode {
            cached: Arc::new(CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(declaring),
                    class_name: Arc::from("p/Helper"),
                    method_name: Arc::from("pickA"),
                    method_descriptor: Arc::from("()I"),
                    source_file: None,
                    code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                    exception_table: Arc::from(&[][..]),
                    max_stack: 1,
                    max_locals: 0,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            )),
            // The TARGET never changes in these tests: what retires the
            // entry is the caller side.
            gate: RedefineGate::never_stale(),
        }
    }

    /// Interpreter round i1 wave 18, lane L4: a redefinition replaces the
    /// CALLER's constant pool under the same `ClassId`, so an entry keyed on
    /// `(caller, cp index)` may name another member afterwards although its
    /// target's gate is unchanged. The next lookup after a redefinition drops
    /// the maps (primary and poly), and the cache serves what is filled after
    /// it.
    #[test]
    fn a_redefinition_retires_the_callers_entries_and_the_cache_keeps_serving() {
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(18_401);
        let receiver = ClassId::new(18_402);
        cache.put(caller, 5, false, 0, i18_l4_static_target(18_403));
        cache.put_poly(caller, 6, false, 0, receiver, i18_l4_static_target(18_403));
        assert!(cache.get(caller, 5, false, 0).is_some());
        assert!(cache.get_poly(caller, 6, false, 0, receiver).is_some());

        tests_redefinition_count::bump();
        assert!(
            cache.get(caller, 5, false, 0).is_none(),
            "an entry filled before the redefinition may name the old pool's member"
        );
        assert!(
            cache.get_poly(caller, 6, false, 0, receiver).is_none(),
            "the poly entries are keyed on the caller too"
        );

        cache.put(caller, 5, false, 0, i18_l4_static_target(18_404));
        match cache.get(caller, 5, false, 0) {
            Some(CachedInvokeTarget::Bytecode { cached, .. }) => {
                assert_eq!(cached.declaring_class_id, ClassId::new(18_404));
            }
            other => panic!("a fill after the redefinition must be served, got {other:?}"),
        }
    }

    /// A fill whose resolution straddled a redefinition (the count moved after
    /// the lookup that missed) is not stored: it may have read the old pool.
    #[test]
    fn a_fill_that_straddles_a_redefinition_is_not_stored() {
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(18_411);
        let receiver = ClassId::new(18_412);
        assert!(
            cache.get(caller, 5, false, 0).is_none(),
            "the missing lookup"
        );
        tests_redefinition_count::bump();
        cache.put(caller, 5, false, 0, i18_l4_static_target(18_413));
        cache.put_poly(caller, 5, false, 0, receiver, i18_l4_static_target(18_413));
        assert!(cache.get(caller, 5, false, 0).is_none());
        assert!(cache.get_poly(caller, 5, false, 0, receiver).is_none());
        // The lookup above caught up with the count: the next fill is kept.
        cache.put(caller, 5, false, 0, i18_l4_static_target(18_413));
        assert!(cache.get(caller, 5, false, 0).is_some());
        assert_eq!(cache.redefinitions_seen(), tests_redefinition_count::get());
    }

    /// Interpreter round i1 wave 19, lane L4: a fill whose resolution ran
    /// Java that looked the cache up AFTER a redefinition (a user loader's
    /// `loadClass`, a `<clinit>`) is not stored. The nested lookup moved
    /// `redefinitions_seen` up to the new count, so `put` alone would admit
    /// the fill; the snapshot taken before the resolution refuses it.
    #[test]
    fn a_fill_whose_resolution_ran_a_lookup_after_a_redefinition_is_not_stored() {
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(19_401);
        let receiver = ClassId::new(19_402);
        assert!(
            cache.get(caller, 5, false, 0).is_none(),
            "the missing lookup"
        );
        let as_of = cache.redefinitions_seen();
        // The resolution runs Java; another thread redefines the caller; the
        // Java performs an invoke on this thread.
        tests_redefinition_count::bump();
        assert!(
            cache.get(caller, 9, false, 0).is_none(),
            "the nested lookup"
        );
        assert_ne!(cache.redefinitions_seen(), as_of);

        cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_403));
        cache.put_poly_as_of(
            caller,
            5,
            false,
            0,
            receiver,
            as_of,
            i18_l4_static_target(19_403),
        );
        assert!(
            cache.get(caller, 5, false, 0).is_none(),
            "resolved against the old pool"
        );
        assert!(cache.get_poly(caller, 5, false, 0, receiver).is_none());

        // A fill resolved after the nested lookup carries the new snapshot.
        let as_of = cache.redefinitions_seen();
        cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_404));
        match cache.get(caller, 5, false, 0) {
            Some(CachedInvokeTarget::Bytecode { cached, .. }) => {
                assert_eq!(cached.declaring_class_id, ClassId::new(19_404));
            }
            other => panic!("a fill resolved under the new count is served, got {other:?}"),
        }
    }

    /// With no redefinition in between, `put_as_of` stores exactly as `put`
    /// does, and a snapshot from before an unrelated earlier move that no
    /// lookup has caught up with is refused like a plain `put`.
    #[test]
    fn put_as_of_with_a_current_snapshot_stores_like_put() {
        let mut cache = InvokeCache::<()>::new();
        let caller = ClassId::new(19_411);
        let receiver = ClassId::new(19_412);
        assert!(cache.get(caller, 5, false, 0).is_none());
        let as_of = cache.redefinitions_seen();
        cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_413));
        cache.put_poly_as_of(
            caller,
            6,
            false,
            0,
            receiver,
            as_of,
            i18_l4_static_target(19_413),
        );
        assert!(cache.get(caller, 5, false, 0).is_some());
        assert!(cache.get_poly(caller, 6, false, 0, receiver).is_some());

        tests_redefinition_count::bump();
        cache.put_as_of(caller, 7, false, 0, as_of, i18_l4_static_target(19_414));
        assert!(cache.get(caller, 7, false, 0).is_none());
    }
}

/// Interpreter round i1 wave 20, lane L2 (stage 2 of
/// `interpreter-L4-proposal-class-scoped-invoke-cache-retirement-FIXED-20260926`): a redefinition
/// the ring names retires only the redefined callers' entries.
#[cfg(test)]
mod w20_l2_class_scoped_retirement_tests {
    use super::tests_redefinition_count;
    use super::{
        CachedBytecodeMethod, CachedInvokeTarget, ClassId, InvokeCache, RedefineGate,
        INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS,
    };
    use std::sync::Arc;

    fn target(declaring: u32) -> CachedInvokeTarget<()> {
        CachedInvokeTarget::Bytecode {
            cached: Arc::new(CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(declaring),
                    class_name: Arc::from("p/Helper"),
                    method_name: Arc::from("pick"),
                    method_descriptor: Arc::from("()I"),
                    source_file: None,
                    code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                    exception_table: Arc::from(&[][..]),
                    max_stack: 1,
                    max_locals: 0,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            )),
            gate: RedefineGate::never_stale(),
        }
    }

    /// A redefinition of `C` (three counts, as `redefine_class` and the VM
    /// record it) drops `(C, ..)` entries in both maps and keeps `(D, ..)`.
    #[test]
    fn a_redefinition_of_one_class_keeps_the_other_callers_entries() {
        assert!(INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS);
        let mut cache = InvokeCache::<()>::new();
        let (c, d, receiver) = (
            ClassId::new(20_201),
            ClassId::new(20_202),
            ClassId::new(20_203),
        );
        cache.put(c, 5, false, 0, target(20_204));
        cache.put(d, 5, false, 0, target(20_204));
        cache.put_poly(c, 6, false, 0, receiver, target(20_204));
        cache.put_poly(d, 6, false, 0, receiver, target(20_204));

        for _ in 0..3 {
            tests_redefinition_count::bump_of(c);
        }
        assert!(cache.get(c, 5, false, 0).is_none(), "the redefined caller");
        assert!(cache.get_poly(c, 6, false, 0, receiver).is_none());
        assert!(cache.get(d, 5, false, 0).is_some(), "an unredefined caller");
        assert!(cache.get_poly(d, 6, false, 0, receiver).is_some());
        assert_eq!(cache.redefinitions_seen(), tests_redefinition_count::get());

        // The redefined caller refills and is served.
        cache.put(c, 5, false, 0, target(20_205));
        assert!(cache.get(c, 5, false, 0).is_some());
    }

    /// A compiled (`Jit`) entry is dropped whatever its caller: its body may
    /// have inlined the redefined class.
    #[test]
    fn a_class_scoped_retirement_drops_every_compiled_entry() {
        let mut cache = InvokeCache::<()>::new();
        let (c, d) = (ClassId::new(20_241), ClassId::new(20_242));
        let compiled = CachedInvokeTarget::Jit {
            compiled: (),
            num_params: 0,
            return_type: b'I',
            needs_heap: false,
            cached: match target(20_243) {
                CachedInvokeTarget::Bytecode { cached, .. } => cached,
                _ => return,
            },
            gate: RedefineGate::never_stale(),
            supersede_epoch: super::SupersedeGate::never_stale(),
        };
        cache.put(d, 5, false, 0, compiled);
        cache.put(d, 6, false, 0, target(20_243));
        assert!(cache.get(d, 5, false, 0).is_some());
        tests_redefinition_count::bump_of(c);
        assert!(cache.get(d, 5, false, 0).is_none(), "compiled: dropped");
        assert!(cache.get(d, 6, false, 0).is_some(), "bytecode: kept");
    }

    /// A reader the ring has lapped cannot know which classes moved: it
    /// retires everything.
    #[test]
    fn a_reader_behind_the_ring_retires_every_entry() {
        let mut cache = InvokeCache::<()>::new();
        let (c, d) = (ClassId::new(20_211), ClassId::new(20_212));
        cache.put(d, 5, false, 0, target(20_213));
        assert!(cache.get(d, 5, false, 0).is_some());
        tests_redefinition_count::bump_of(c);
        tests_redefinition_count::bump_of(c);
        tests_redefinition_count::lap(1);
        assert!(cache.get(d, 5, false, 0).is_none());
    }

    /// A bump that names no class retires everything, and a class-scoped
    /// one in the same span does not rescue the other callers.
    #[test]
    fn an_unattributed_redefinition_retires_every_entry() {
        let mut cache = InvokeCache::<()>::new();
        let (c, d) = (ClassId::new(20_221), ClassId::new(20_222));
        cache.put(d, 5, false, 0, target(20_223));
        tests_redefinition_count::bump_of(c);
        tests_redefinition_count::bump();
        assert!(cache.get(d, 5, false, 0).is_none());
    }

    /// The fill admission is unchanged: a fill that straddles a class-scoped
    /// redefinition of ANOTHER class is still dropped (its snapshot is old).
    #[test]
    fn a_fill_straddling_an_unrelated_redefinition_is_still_dropped() {
        let mut cache = InvokeCache::<()>::new();
        let (c, d) = (ClassId::new(20_231), ClassId::new(20_232));
        assert!(cache.get(d, 5, false, 0).is_none());
        let as_of = cache.redefinitions_seen();
        tests_redefinition_count::bump_of(c);
        cache.put_as_of(d, 5, false, 0, as_of, target(20_233));
        assert!(cache.get(d, 5, false, 0).is_none());
    }
}

#[cfg(test)]
mod i22_l5_fill_snapshot_tests {
    use super::*;

    fn method(name: &str) -> ResolvedMethod {
        ResolvedMethod {
            declaring_class_id: ClassId::new(22_502),
            class_name: Arc::from("i22l5/Owner"),
            method_name: Arc::from(name),
            method_descriptor: Arc::from("()V"),
            num_params: 0,
            native_target: None,
            native_kind: None,
        }
    }

    /// Interpreter round i1 wave 22, lane L5: a `(class, cp index)` fill whose
    /// resolution read the constant pool before a redefinition is not
    /// published — the redefinition's sweep already ran, and nothing would
    /// sweep the old pool's member again. A fill that saw no redefinition is.
    #[test]
    fn a_fill_that_straddles_a_redefinition_is_not_published() {
        let mut cache = ResolutionCache::new();
        let caller = ClassId::new(22_501);

        let before = ResolutionCache::fill_snapshot();
        // What `redefine_class` does between the resolver's pool read and
        // its fill: advance the count, sweep, advance it again.
        tests_redefinition_count::bump_of(caller);
        cache.invalidate_class(caller);
        tests_redefinition_count::bump_of(caller);
        assert!(!cache.put_method_as_of(caller, 7, method("oldPoolMember"), before));
        assert!(
            cache.get_method(caller, 7).is_none(),
            "the old pool's member is not cached under the new pool's index"
        );
        let field = ResolvedField {
            declaring_class_id: ClassId::new(22_502),
            field_index: 0,
            is_static: false,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: b'I',
        };
        assert!(!cache.put_field_as_of(caller, 8, field.clone(), before));
        assert!(cache.get_field(caller, 8).is_none());

        let now = ResolutionCache::fill_snapshot();
        assert!(cache.put_method_as_of(caller, 7, method("newPoolMember"), now));
        assert_eq!(
            cache.get_method(caller, 7).map(|m| &*m.method_name),
            Some("newPoolMember")
        );
        assert!(cache.put_field_as_of(caller, 8, field, now));
        assert!(cache.get_field(caller, 8).is_some());
    }

    /// Interpreter round i1 wave 23, lane L5
    /// (`i22-L5-resolution-cache-fills-can-outlive-a-concurrent-redefinition`):
    /// the call-site, constant and failure fills have `_as_of` twins, and
    /// every twin is class-scoped — a redefinition of ANOTHER class does not
    /// skip the fill (a skipped permanent constant would re-run its
    /// bootstrap, a skipped failure would let a later attempt succeed), while
    /// one of the caller, or a span the ring cannot name, does.
    #[test]
    fn every_fill_twin_skips_exactly_when_the_callers_pool_may_have_been_replaced() {
        let mut cache = ResolutionCache::new();
        let caller = ClassId::new(23_501);
        let unrelated = ClassId::new(23_502);
        let failure = || ResolutionFailure {
            error_class: Arc::from("java/lang/NoClassDefFoundError"),
            message: Some(Arc::from("i23l5/Missing")),
            cause: None,
        };
        let site = || ResolvedCallSite::StringConcat {
            recipe: Arc::from(&[1u16][..]),
            constant_args: Vec::new(),
            target_descriptor: Arc::from("(I)Ljava/lang/String;"),
        };

        // Another class's redefinition: every fill is published.
        let before = ResolutionCache::fill_snapshot();
        tests_redefinition_count::bump_of(unrelated);
        assert!(cache.put_call_site_as_of(caller, 1, site(), before));
        assert!(cache.put_condy_as_of(caller, 2, Value::Int(7), before));
        assert_eq!(
            cache.put_permanent_constant_as_of(caller, 3, Value::Int(8), before),
            Some(Value::Int(8))
        );
        assert!(cache.put_failure_as_of(caller, 4, failure(), before));
        assert!(cache.put_method_as_of(caller, 5, method("kept"), before));
        assert!(cache.get_call_site(caller, 1).is_some());
        assert_eq!(cache.get_condy(caller, 2), Some(&Value::Int(7)));
        assert_eq!(cache.get_condy(caller, 3), Some(&Value::Int(8)));
        assert!(cache.get_failure(caller, 4).is_some());

        // The caller's redefinition between the pool read and the fill: the
        // sweep already ran, so nothing is published and nothing replaced.
        let before = ResolutionCache::fill_snapshot();
        tests_redefinition_count::bump_of(caller);
        cache.invalidate_class(caller);
        tests_redefinition_count::bump_of(caller);
        assert!(!cache.put_call_site_as_of(caller, 11, site(), before));
        assert!(!cache.put_condy_as_of(caller, 12, Value::Int(1), before));
        assert_eq!(
            cache.put_permanent_constant_as_of(caller, 13, Value::Int(2), before),
            None,
            "the old pool's constant is handed to its own execution only"
        );
        assert!(!cache.put_failure_as_of(caller, 14, failure(), before));
        assert!(cache.get_call_site(caller, 11).is_none());
        assert!(cache.get_condy(caller, 12).is_none());
        assert!(cache.get_condy(caller, 13).is_none());
        assert!(
            cache.get_failure(caller, 14).is_none(),
            "the new pool's entry at 14 is not failed with the old entry's error"
        );

        // A span the ring cannot name (an unattributed bump): skipped.
        let before = ResolutionCache::fill_snapshot();
        tests_redefinition_count::bump();
        assert!(!cache.put_failure_as_of(caller, 24, failure(), before));
        assert!(cache.get_failure(caller, 24).is_none());

        // No redefinition at all: published.
        let now = ResolutionCache::fill_snapshot();
        assert!(cache.put_failure_as_of(caller, 34, failure(), now));
        assert!(cache.get_failure(caller, 34).is_some());
    }
}
