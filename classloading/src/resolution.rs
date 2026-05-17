//! Resolution cache for symbolic references.
//!
//! Caches resolved constant pool references (fields, methods, and call sites) to avoid
//! re-resolving on every instruction. The cache key is `(referring class, cp index)`.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::fx_hash::{fx_hashmap_with_capacity, FxHashMap};

use super::ClassId;
use rustjvm_types::Value;

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
        recipe: Arc<str>,
        constant_args: Vec<Arc<str>>,
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
}

// ---------------------------------------------------------------------------
// Resolution cache
// ---------------------------------------------------------------------------

/// Cache key: (referring class, constant pool index).
type ResolutionKey = (ClassId, u16);

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
}

impl ResolutionCache {
    /// Create an empty resolution cache.
    pub fn new() -> Self {
        Self {
            fields: fx_hashmap_with_capacity(64),
            methods: fx_hashmap_with_capacity(64),
            call_sites: fx_hashmap_with_capacity(16),
            condy: fx_hashmap_with_capacity(16),
        }
    }

    /// Look up a cached field resolution.
    pub fn get_field(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedField> {
        self.fields.get(&(class_id, cp_index))
    }

    /// Cache a resolved field.
    pub fn put_field(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedField) {
        self.fields.insert((class_id, cp_index), resolved);
    }

    /// Look up a cached method resolution.
    pub fn get_method(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedMethod> {
        self.methods.get(&(class_id, cp_index))
    }

    /// Cache a resolved method.
    pub fn put_method(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedMethod) {
        self.methods.insert((class_id, cp_index), resolved);
    }

    /// Look up a cached call site (invokedynamic).
    pub fn get_call_site(&self, class_id: ClassId, cp_index: u16) -> Option<&ResolvedCallSite> {
        self.call_sites.get(&(class_id, cp_index))
    }

    /// Cache a resolved call site.
    pub fn put_call_site(&mut self, class_id: ClassId, cp_index: u16, resolved: ResolvedCallSite) {
        self.call_sites.insert((class_id, cp_index), resolved);
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
        self.condy.insert((class_id, cp_index), value);
    }

    /// Scan cached CONSTANT_Dynamic values for GC roots.
    pub fn scan_condy_roots(&self, roots: &mut Vec<rustjvm_types::ObjectRef>) {
        for val in self.condy.values() {
            if let Value::Object(Some(obj_ref)) = val {
                roots.push(*obj_ref);
            }
        }
    }

    /// Update cached CONSTANT_Dynamic ObjectRefs after GC relocation.
    pub fn update_condy_refs(&mut self, pointer_map: &std::collections::HashMap<usize, usize>) {
        for val in self.condy.values_mut() {
            if let Value::Object(Some(ref mut obj_ref)) = val {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { rustjvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
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
    }
}

impl Default for ResolutionCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Invoke cache — stores everything needed to create a Frame directly
// ---------------------------------------------------------------------------

use rustjvm_native_api::NativeCallback;

// Re-export from jit-api crate — the canonical definition lives there now.
pub use rustjvm_jit_api::CachedBytecodeMethod;

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
        Self { generation, counter }
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
pub enum CachedInvokeTarget {
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
        num_params: u16,
        /// WP2.4-F1 — redefine staleness gate.
        gate: RedefineGate,
    },
    /// JIT-compiled method: call native code directly, no frame push needed.
    Jit {
        compiled: Arc<rustjvm_jit::CompiledMethod>,
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
    },
}

impl CachedInvokeTarget {
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
            | CachedInvokeTarget::Jit { gate, .. } => gate.is_stale(),
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

/// Cache that maps invoke sites to fully-resolved invoke targets.
/// On cache hit, no locks, string allocations, or method resolution needed.
pub struct InvokeCache {
    entries: FxHashMap<InvokeCacheKey, CachedInvokeTarget>,
}

impl InvokeCache {
    pub fn new() -> Self {
        Self {
            entries: fx_hashmap_with_capacity(64),
        }
    }

    /// Look up an invoke-cache entry. Stale entries (whose declaring class
    /// has been redefined since the entry was populated) are *not* returned
    /// from this getter — they're auto-evicted and the call site falls
    /// through to the slow path which re-resolves against the freshly
    /// installed bytecode.  See [`RedefineGate`] for the underlying check.
    #[inline]
    pub fn get(&mut self, caller_class: ClassId, cp_index: u16, is_special: bool) -> Option<&CachedInvokeTarget> {
        let key = (caller_class, cp_index, is_special);
        // WP2.4-F1: O(1) generation check on hit. If the entry is stale,
        // remove it and pretend we never had it; the caller will repopulate.
        let stale = self.entries.get(&key).map(|t| t.is_stale()).unwrap_or(false);
        if stale {
            self.entries.remove(&key);
            return None;
        }
        self.entries.get(&key)
    }

    pub fn put(&mut self, caller_class: ClassId, cp_index: u16, is_special: bool, target: CachedInvokeTarget) {
        self.entries.insert((caller_class, cp_index, is_special), target);
    }

    /// Clear all cached invoke targets. Used to invalidate stale entries after
    /// a partially-failed class initialization (e.g. System.initPhase1).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Evict a single entry. Used by call sites that detect staleness via
    /// other means (e.g. JIT downgrade) to force the next lookup down the
    /// slow path.
    #[inline]
    pub fn evict(&mut self, caller_class: ClassId, cp_index: u16, is_special: bool) {
        self.entries.remove(&(caller_class, cp_index, is_special));
    }
}

impl Default for InvokeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for InvokeCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvokeCache({} entries)", self.entries.len())
    }
}

impl fmt::Debug for CachedInvokeTarget {
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
            },
        );

        let resolved = cache.get_method(key_class, cp_index).unwrap();
        assert_eq!(resolved.declaring_class_id, ClassId::new(3));
        assert_eq!(&*resolved.method_name, "toString");
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
        let mut cache = InvokeCache::new();
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
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        let mut cache = InvokeCache::new();
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
        assert!(cache.get(caller, cp_index, false).is_none(),
                "stale entry must be evicted after redefine bump");

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
        });
        cache.put(caller, cp_index, false, CachedInvokeTarget::Bytecode {
            cached: new_cached,
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
        });
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
                recipe: Arc::from("Hello, \u{0001}!"),
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
}
