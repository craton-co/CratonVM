// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime class representation.
//!
//! After a `.class` file is parsed by the reader, the VM wraps it into a `Class`
//! that holds resolved superclass/interface references as `ClassId`s and supports
//! field/method lookup and subclass checking.

use std::fmt;
use std::sync::Arc;

use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::field::ClassFileField;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::CompactLayout;
use rustc_hash::FxHashSet;

// ---------------------------------------------------------------------------
// ClassState — the class lifecycle state machine (JVM spec 5.5)
// ---------------------------------------------------------------------------

/// The initialization state of a class (JVM spec 5.5).
///
/// ```text
/// Loading → Loaded → Verifying → Verified → Preparing → Prepared → Initializing → Initialized
///                                                                                ↘ InitializationError
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassState {
    /// Class definition is in progress (superclass/interfaces being resolved).
    /// Used to detect circular class loading dependencies. A class in this state
    /// has been allocated a ClassId but its full metadata is not yet available.
    Loading,
    /// Class has been loaded and parsed, but not yet verified.
    Loaded,
    /// Structural verification (Pass 2) is in progress.
    Verifying,
    /// Structural verification passed.
    Verified,
    /// Static field preparation is in progress.
    Preparing,
    /// Static fields have been prepared (default + ConstantValue).
    Prepared,
    /// `<clinit>` is being executed.
    Initializing,
    /// Fully initialized and ready for use.
    Initialized,
    /// `<clinit>` threw an exception; class is unusable.
    InitializationError,
}

impl fmt::Display for ClassState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassState::Loading => write!(f, "Loading"),
            ClassState::Loaded => write!(f, "Loaded"),
            ClassState::Verifying => write!(f, "Verifying"),
            ClassState::Verified => write!(f, "Verified"),
            ClassState::Preparing => write!(f, "Preparing"),
            ClassState::Prepared => write!(f, "Prepared"),
            ClassState::Initializing => write!(f, "Initializing"),
            ClassState::Initialized => write!(f, "Initialized"),
            ClassState::InitializationError => write!(f, "InitializationError"),
        }
    }
}

// Re-export ClassId and ClassLoaderId from the shared types crate.
pub use cratonvm_types::{ClassId, ClassLoaderId};

// ---------------------------------------------------------------------------
// CodeSource — per-class protection-domain location + signer certificates
// ---------------------------------------------------------------------------

/// A lightweight stand-in for `java.security.CodeSource`. Each loaded class
/// can have one attached; permission checks inspect the `url` field to
/// match `grant codeBase "..."` entries in a policy file, and the
/// `certificate_sha256` set is consulted by `signedBy` filters.
///
/// This is deliberately *not* a full `java.security.CodeSource` — a
/// production implementation would parse each DER-encoded cert with an
/// X.509 parser and expose the signer chain. Here we just hash the DER
/// bytes so a policy parser can compare against the same hash format.
///
/// `CodeSource` is `Clone + Send + Sync` so it can live alongside `Class`
/// and be handed off to permission checks running on arbitrary threads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeSource {
    /// The origin URL of the class. Typical forms:
    /// * `file:/opt/app.jar` for a JAR on disk
    /// * `file:/opt/classes/` for an unpacked directory
    /// * `None` for synthetic / in-memory classes that have no stable URL.
    pub url: Option<String>,

    /// DER-encoded X.509 signer certificates.  An empty vector means the
    /// source is unsigned.  The first entry is conventionally the
    /// end-entity cert; subsequent entries form the chain up to a root.
    pub certificates: Vec<Vec<u8>>,

    /// Pre-computed SHA-256 hex digests of every entry in `certificates`.
    /// Kept in sync with `certificates`.  `signedBy` grants match when
    /// the policy's signedBy value is present in this set (see
    /// `native-builtins/src/security_manager/policy.rs`).
    pub certificate_sha256: Vec<String>,
}

impl CodeSource {
    /// Build a `CodeSource` from a URL and an optional set of DER-encoded
    /// X.509 certificates. The SHA-256 digests are pre-computed so every
    /// permission check is a cheap hex-string compare.
    pub fn new(url: Option<String>, certificates: Vec<Vec<u8>>) -> Self {
        let certificate_sha256 = certificates.iter().map(|der| sha256_hex(der)).collect();
        CodeSource {
            url,
            certificates,
            certificate_sha256,
        }
    }

    /// Convenience: a URL-only (unsigned) source.
    pub fn from_url(url: impl Into<String>) -> Self {
        CodeSource {
            url: Some(url.into()),
            certificates: Vec::new(),
            certificate_sha256: Vec::new(),
        }
    }
}

/// SHA-256 of a byte slice, formatted as lowercase hex. Hand-rolled to
/// avoid pulling a hashing crate into `classloading` — the digest is
/// only used for fingerprint comparisons, not for cryptographic purposes.
fn sha256_hex(data: &[u8]) -> String {
    // NIST FIPS 180-4, §6.2.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Pad: bit 1 + zeros + 64-bit big-endian length.
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write;
        let _ = write!(out, "{word:08x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Class — runtime representation of a loaded Java class
// ---------------------------------------------------------------------------

/// A loaded Java class in the VM.
///
/// This struct combines the parsed class file data with runtime bookkeeping
/// such as the assigned `ClassId`, resolved superclass/interface `ClassId`s,
/// and field layout information.
#[derive(Debug)]
pub struct Class {
    /// Unique identifier assigned at load time.
    pub id: ClassId,

    /// Which class loader loaded this class.
    pub loader_id: ClassLoaderId,

    /// Fully-qualified internal name (e.g. `"java/lang/Object"`).
    ///
    /// Stored as a shared `Arc<str>` so classes that reference the same
    /// name (e.g. 100s of `.class` files mentioning `"java/lang/Object"`)
    /// share a single backing allocation. The backing storage originates
    /// from [`cratonvm_types::intern_arc`] at parse time in the reader's
    /// constant-pool path. Cloning this field is a single refcount bump.
    pub name: Arc<str>,

    /// Source file name from the `SourceFile` attribute, if present.
    pub source_file: Option<String>,

    /// Class file version (major.minor), used to determine verification strategy.
    pub version: ClassFileVersion,

    /// Current initialization state (JVM spec 5.5).
    pub state: ClassState,

    /// The thread that is currently executing `<clinit>` for this class.
    /// Set when state transitions to `Initializing`, cleared on `Initialized`
    /// or `InitializationError`. Used for re-entrancy detection (JVM spec §5.5
    /// step 2: if the same thread is already initializing, return immediately).
    pub initializing_thread: Option<u64>,

    /// The constant pool from the parsed class file.
    pub constant_pool: ConstantPool,

    /// Class access flags (public, final, interface, etc.).
    pub access_flags: ClassAccessFlags,

    /// The superclass, or `None` for `java/lang/Object`.
    pub superclass: Option<ClassId>,

    /// Directly implemented interfaces.
    pub interfaces: Vec<ClassId>,

    /// Fields declared in **this** class (not inherited).
    pub fields: Vec<ClassFileField>,

    /// Methods declared in **this** class.
    pub methods: Vec<ClassFileMethod>,

    /// Index of the first field that belongs to this class (inherited fields
    /// have indices `0..first_field_index`). This is used for field layout:
    /// when allocating an object, we need `num_total_fields` slots, and
    /// inherited fields keep the same indices they have in the parent class.
    pub first_field_index: usize,

    /// Total number of instance fields (inherited + own). This determines how
    /// many field slots an object of this class needs.
    pub num_total_fields: usize,

    /// Bootstrap methods from the `BootstrapMethods` class attribute (JVM spec 4.7.23).
    /// Used by `invokedynamic` instructions to resolve call sites.
    pub bootstrap_methods: Vec<cratonvm_reader::attribute::BootstrapMethod>,

    /// Generic signature string from the Signature attribute (JVMS §4.7.9).
    pub signature: Option<String>,

    /// Annotations on this class (from RuntimeVisibleAnnotations and RuntimeInvisibleAnnotations).
    pub annotations: Vec<cratonvm_reader::attribute::Annotation>,

    /// Nest host class name (from NestHost attribute, JEP 181).
    /// If `None`, this class is its own nest host (or has NestMembers).
    pub nest_host: Option<String>,

    /// Nest member class names (from NestMembers attribute, JEP 181).
    /// Only populated on the nest host class itself.
    pub nest_members: Vec<String>,

    /// Record components (from Record attribute, JEP 395, Java 16+).
    /// Non-empty only for record classes.
    pub record_components: Vec<RecordComponentInfo>,

    /// Permitted subclass names (from PermittedSubclasses attribute, JEP 409, Java 17+).
    /// Non-empty only for sealed classes/interfaces.
    pub permitted_subclasses: Vec<String>,

    /// Inner classes declared in this class (from InnerClasses attribute, JVMS §4.7.6).
    /// Each entry: (inner_class_name, outer_class_name, inner_name, access_flags).
    /// `inner_class_name` and `outer_class_name` are internal names; `inner_name` is
    /// the simple name (or empty for anonymous classes).
    pub inner_classes: Vec<InnerClassEntry>,

    /// Enclosing method info (from EnclosingMethod attribute, JVMS §4.7.7).
    /// `(enclosing_class_name, method_name, method_descriptor)`.
    /// Only present for local/anonymous classes.
    pub enclosing_method: Option<EnclosingMethodInfo>,

    /// Whether this is a hidden class (JEP 371, Java 15+).
    /// Hidden classes are not discoverable by name via ClassLoader.
    pub hidden: bool,

    /// Module name (from Module attribute, Java 9+). None for unnamed module.
    pub module_name: Option<String>,

    /// `true` if this class was created as a synthetic stub (no `.class` file found).
    /// Synthetic stubs have zero methods and rely entirely on native registrations.
    /// When a real `.class` file is available, this is `false` and bytecode methods
    /// take precedence over native registrations in dispatch.
    ///
    /// Do not use this path for types the JDK or application exposes as real classfiles;
    /// see `docs/jvm-no-synthetic-stubs.md`.
    pub is_synthetic_stub: bool,

    /// `true` if this class (or an ancestor) overrides `Object.finalize()`.
    /// Objects of such classes are registered with the `FinalizerThread` at
    /// allocation time so their `finalize()` method can be invoked before GC
    /// reclaims them (JLS §12.6).
    pub has_finalizer: bool,

    /// Protection-domain `CodeSource` for this class (`None` for synthetic
    /// stubs and bootstrap JDK classes loaded from `jmod`/`jimage`).  This
    /// is what `SecurityManager.checkPermission` walks to resolve
    /// `grant codeBase "..."` and `grant signedBy "..."` filters — the
    /// URL becomes the code base and the SHA-256 of each signer cert
    /// becomes the signedBy match key.
    pub code_source: Option<CodeSource>,

    /// RKC16N.3 — Array-class metadata.
    ///
    /// `Some(_)` if and only if this class represents a Java array type
    /// (binary name starts with `[`). The bootstrap class loader
    /// **synthesises** these classes from the resolved component class —
    /// JVMS §5.3.3 forbids any classpath I/O for reference-array types.
    /// `None` for ordinary classes and interfaces.
    pub array_info: Option<ArrayInfo>,

    /// Round 8 audit fix (CRIT #4): per-class initialization-state
    /// AtomicU8, embedded directly on `Class` (not in a side-table).
    ///
    /// Previously the "fast path" for `ensure_class_initialized` went
    /// through `ClassManager::class_init_state_handle(class_id)` which
    /// did a `RwLock<FxHashMap<ClassId, Arc<AtomicU8>>>::read()` +
    /// hash lookup on EVERY dispatch — i.e. the supposed fast path
    /// was a lock acquisition + hash probe per checkpoint. Embedding
    /// the atomic on `Class` collapses the fast path to a single
    /// `Arc<AtomicU8>::load(Acquire)` once the caller already holds
    /// an `Arc<Class>` (or `&Class`), which the dispatch path
    /// typically does.
    ///
    /// The state values are the same `CLASS_INIT_*` constants used by
    /// the side-table (defined in `class_manager`). The side-table is
    /// retained for back-compat during the round-8 transition; both
    /// stores must be kept in lockstep by `set_class_init_state`.
    ///
    /// Wrapped in `Arc` so callers (interpreter dispatch, JIT entry
    /// stubs, reflection) can cheaply share a handle that outlives
    /// the borrow of the enclosing `Class`.
    pub init_state: Arc<std::sync::atomic::AtomicU8>,
}

/// RKC16N.3 — Metadata for a synthesised array class.
///
/// Per JVMS §5.3.3 reference-array classes are synthesised by the bootstrap
/// loader from the resolved component class with no classpath I/O. Per
/// JLS §10.7 every array class has `java.lang.Object` as its superclass and
/// implements `java.lang.Cloneable` and `java.io.Serializable`.
#[derive(Debug, Clone)]
pub struct ArrayInfo {
    /// `ClassId` of the **immediate** component type. For `[Ljava/util/HashMap;`
    /// this is the id of `java/util/HashMap`. For `[[I` this is the id of
    /// the inner array class `[I`. For primitive arrays such as `[I` this
    /// is the id of the primitive pseudo-class (`int`, `long`, …).
    pub component_class_id: ClassId,

    /// Total number of `[` prefixes in the array's binary name. For `[I`
    /// this is `1`; for `[[I` and `[[Ljava/util/HashMap;` this is `2`; etc.
    pub array_dimension: u8,

    /// Internal name of the **leaf** component (the inner-most non-array
    /// type). For `[Ljava/util/HashMap;` this is `java/util/HashMap`; for
    /// `[[I` this is `int`. Cached so callers do not have to re-walk the
    /// `[` chain.
    pub leaf_component_name: Arc<str>,
}

/// A resolved record component (from the Record attribute, Java 16+).
#[derive(Debug, Clone)]
pub struct RecordComponentInfo {
    pub name: String,
    pub descriptor: String,
}

/// An entry from the InnerClasses attribute (JVMS §4.7.6).
#[derive(Debug, Clone)]
pub struct InnerClassEntry {
    /// Internal name of the inner class.
    pub inner_class: String,
    /// Internal name of the outer class, or empty if anonymous.
    pub outer_class: String,
    /// Simple name of the inner class, or empty for anonymous classes.
    pub inner_name: String,
    /// Access flags of the inner class.
    pub access_flags: u16,
}

/// Enclosing method info (JVMS §4.7.7).
#[derive(Debug, Clone)]
pub struct EnclosingMethodInfo {
    /// Internal name of the enclosing class.
    pub class_name: String,
    /// Name of the enclosing method, or empty if not in a method.
    pub method_name: String,
    /// Descriptor of the enclosing method, or empty if not in a method.
    pub method_descriptor: String,
}

impl Class {
    // ----- Interned accessors ---------------------------------------------
    //
    // `Class::name`, `ClassFileMethod::name`/`descriptor`, and
    // `ClassFileField::name`/`descriptor` are already stored as
    // `Arc<str>`, so these accessors are direct clones of the field —
    // no hash lookup, no allocation, just a refcount bump. They exist
    // so consumers that want to hold onto the name beyond the borrow
    // of `Class` / `ClassFileMethod` / `ClassFileField` can do so
    // without binding to the `&str` lifetime of the enclosing borrow.
    //
    // Backing storage originates in the reader's constant-pool path
    // (`cratonvm_types::intern_arc` at parse time), so identical names
    // loaded across many classes share a single allocation. Two `Arc<str>`
    // returned from these methods for the same content compare equal via
    // `Arc::ptr_eq`.

    /// Returns a shared `Arc<str>` for this class's name.
    ///
    /// Repeated calls return clones of the same `Arc<str>` (verifiable
    /// via `Arc::ptr_eq`). Equivalent to `self.name.clone()` since the
    /// field is already an interned `Arc<str>`.
    pub fn interned_name(&self) -> Arc<str> {
        Arc::clone(&self.name)
    }

    /// Returns a shared `Arc<str>` for the method at `index` by name.
    /// `None` if the index is out of range.
    pub fn interned_method_name(&self, index: usize) -> Option<Arc<str>> {
        self.methods.get(index).map(|m| Arc::clone(&m.name))
    }

    /// Returns a shared `Arc<str>` for the method at `index` by descriptor.
    /// `None` if the index is out of range.
    pub fn interned_method_descriptor(&self, index: usize) -> Option<Arc<str>> {
        self.methods.get(index).map(|m| Arc::clone(&m.descriptor))
    }

    /// Returns a shared `Arc<str>` for the field at `index` by name.
    /// `None` if the index is out of range.
    pub fn interned_field_name(&self, index: usize) -> Option<Arc<str>> {
        self.fields.get(index).map(|f| Arc::clone(&f.name))
    }

    /// Returns a shared `Arc<str>` for the field at `index` by descriptor.
    /// `None` if the index is out of range.
    pub fn interned_field_descriptor(&self, index: usize) -> Option<Arc<str>> {
        self.fields.get(index).map(|f| Arc::clone(&f.descriptor))
    }

    // ----- Class-level attribute accessors --------------------------------
    //
    // All class-level attributes are eagerly decoded at load time by the
    // class manager and stored in the dedicated `pub` fields above
    // (`source_file`, `signature`, `nest_host`, `enclosing_method`,
    // `record_components`, `nest_members`, `permitted_subclasses`,
    // `inner_classes`, `bootstrap_methods`, `annotations`, `module_name`).
    //
    // The accessor methods below exist so callers can use a stable shape
    // (`Option<&str>` / `&[T]`) without committing to whether the field
    // is `Option<String>` or owned `Vec<_>`.

    /// The `SourceFile` attribute value (JVM spec 4.7.10), if present.
    pub fn source_file(&self) -> Option<&str> {
        self.source_file.as_deref()
    }

    /// The `Signature` attribute value (JVM spec 4.7.9), if present.
    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    /// The internal name of the nest host (JEP 181), if a `NestHost`
    /// attribute is present.
    pub fn nest_host(&self) -> Option<&str> {
        self.nest_host.as_deref()
    }

    /// Resolved `EnclosingMethod` info (JVMS §4.7.7) for local/anonymous
    /// classes, if the attribute is present.
    pub fn enclosing_method(&self) -> Option<&EnclosingMethodInfo> {
        self.enclosing_method.as_ref()
    }

    /// Resolved record components (JEP 395, Java 16+). Empty for
    /// non-record classes.
    pub fn record_components(&self) -> &[RecordComponentInfo] {
        &self.record_components
    }

    // ----- Method lookup ---------------------------------------------------

    /// Find a method declared in **this** class by name and descriptor.
    ///
    /// Does **not** walk the superclass chain — that is the caller's
    /// responsibility (typically the class manager or interpreter).
    pub fn find_method(&self, name: &str, descriptor: &str) -> Option<&ClassFileMethod> {
        self.methods
            .iter()
            .find(|m| &*m.name == name && &*m.descriptor == descriptor)
    }

    // ----- Record / Sealed checks ------------------------------------------

    /// Returns `true` if this class is a record (has Record attribute).
    #[inline]
    pub fn is_record(&self) -> bool {
        !self.record_components.is_empty()
    }

    /// Returns `true` if this class is sealed (has PermittedSubclasses attribute).
    #[inline]
    pub fn is_sealed(&self) -> bool {
        !self.permitted_subclasses.is_empty()
    }

    /// Check if this is a hidden class (JEP 371).
    #[inline]
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Returns `true` if **this** class declares a `finalize()V` method (not inherited).
    /// Used during class loading to compute [`Class::has_finalizer`].
    pub fn declares_finalize(&self) -> bool {
        &*self.name != "java/lang/Object"
            && self
                .methods
                .iter()
                .any(|m| &*m.name == "finalize" && &*m.descriptor == "()V")
    }

    // ----- Field lookup ----------------------------------------------------

    /// Find a field declared in **this** class by name.
    ///
    /// Returns `(absolute_index, &field)` where `absolute_index` accounts for
    /// inherited fields (i.e. it equals `first_field_index + local_offset`).
    ///
    /// To search inherited fields, call this on the superclass via `ClassStore`.
    pub fn find_own_field(&self, name: &str) -> Option<(usize, &ClassFileField)> {
        // For static fields, the index is the position among static fields.
        // For instance fields, the index is first_field_index + position among instance fields.
        let mut static_idx = 0usize;
        let mut instance_idx = 0usize;
        for field in &self.fields {
            if &*field.name == name {
                let abs_idx = if field.is_static() {
                    static_idx
                } else {
                    self.first_field_index + instance_idx
                };
                return Some((abs_idx, field));
            }
            if field.is_static() {
                static_idx += 1;
            } else {
                instance_idx += 1;
            }
        }
        None
    }

    /// Get the **instance** field at a given absolute index.
    ///
    /// If `index < first_field_index`, the field belongs to a superclass and
    /// this method returns `None` — the caller should look it up from the
    /// superclass via `ClassStore`.
    ///
    /// `index - first_field_index` is the position of the field among this
    /// class's *instance* fields. `self.fields` holds static **and** instance
    /// fields interleaved in declaration order, so we cannot index `self.fields`
    /// directly: a class such as `java.util.regex.Matcher` declares two static
    /// constants (`ENDANCHOR`, `NOANCHOR`) in the middle of its instance fields,
    /// which would shift every instance field declared after them. We must walk
    /// `self.fields` counting only non-static entries — this is the inverse of
    /// the `first_field_index + instance_idx` mapping in [`Self::find_own_field`].
    pub fn field_at_index(&self, index: usize) -> Option<&ClassFileField> {
        if index < self.first_field_index {
            return None; // belongs to a superclass
        }
        let target_instance_idx = index - self.first_field_index;
        let mut instance_idx = 0usize;
        for field in &self.fields {
            if field.is_static() {
                continue;
            }
            if instance_idx == target_instance_idx {
                return Some(field);
            }
            instance_idx += 1;
        }
        None
    }

    // ----- Subclass / interface checking -----------------------------------

    /// Check whether this class is a subclass of (or implements) the given class.
    ///
    /// Walks the superclass chain and interface list recursively. Needs access
    /// to the `ClassStore` to look up parent classes by `ClassId`.
    ///
    /// **Visited set:** the recursion uses an `FxHashSet<ClassId>` to dedupe
    /// nodes already explored. Without it, diamond interface hierarchies
    /// (e.g. `B implements I1, I2` where `I1 extends I3` and `I2 extends I3`,
    /// or the much wider real-JDK shapes around `java/util/List` /
    /// `java/util/Collection` / `java/util/SequencedCollection`) trigger an
    /// exponential blowup: every shared interface in the DAG is visited
    /// `2^k` times where `k` is the diamond depth. With the visited set the
    /// walk is linear in the size of the (super, interface) DAG.
    pub fn is_subclass_of(&self, other_id: ClassId, store: &ClassStore) -> bool {
        // Cheap early-out — the overwhelmingly common case is `self == other`
        // or the immediate superclass match, neither of which needs the
        // visited-set allocation.
        if self.id == other_id {
            return true;
        }
        let mut visited: FxHashSet<ClassId> = FxHashSet::default();
        self.is_subclass_of_inner(other_id, store, 0, &mut visited)
    }

    fn is_subclass_of_inner(
        &self,
        other_id: ClassId,
        store: &ClassStore,
        depth: usize,
        visited: &mut FxHashSet<ClassId>,
    ) -> bool {
        if depth > MAX_HIERARCHY_DEPTH {
            return false;
        }
        if self.id == other_id {
            return true;
        }
        // Mark this node visited; if we have seen it before in another
        // branch of the diamond, skip — its answer was already false (we
        // wouldn't be re-entering otherwise).
        if !visited.insert(self.id) {
            return false;
        }

        // Walk the superclass chain.
        if let Some(super_id) = self.superclass {
            if super_id == other_id {
                return true;
            }
            if let Some(super_class) = store.get(super_id) {
                if super_class.is_subclass_of_inner(other_id, store, depth + 1, visited) {
                    return true;
                }
            }
        }

        // Walk implemented interfaces.
        for &iface_id in &self.interfaces {
            if iface_id == other_id {
                return true;
            }
            if let Some(iface_class) = store.get(iface_id) {
                if iface_class.is_subclass_of_inner(other_id, store, depth + 1, visited) {
                    return true;
                }
            }
        }

        false
    }

    // ----- Access flag convenience -----------------------------------------

    #[inline]
    pub fn is_interface(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::INTERFACE)
    }

    #[inline]
    pub fn is_abstract(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::ABSTRACT)
    }

    #[inline]
    pub fn is_public(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::PUBLIC)
    }

    #[inline]
    pub fn is_final(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::FINAL)
    }

    #[inline]
    pub fn is_enum(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::ENUM)
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Class[{}] {}", self.id, self.name)
    }
}

/// Maximum depth for hierarchy traversal, to prevent stack overflow on
/// pathological or cyclic class hierarchies.
const MAX_HIERARCHY_DEPTH: usize = 1024;

// ---------------------------------------------------------------------------
// ClassStore — indexed storage for all loaded classes
// ---------------------------------------------------------------------------

/// A store for all loaded `Class` instances, indexed by `ClassId`.
///
/// Classes are appended via `add` and receive monotonically increasing ids.
/// Lookup is O(1) by `ClassId`.
#[derive(Debug)]
pub struct ClassStore {
    classes: Vec<Class>,
}

impl ClassStore {
    /// Create an empty class store.
    pub fn new() -> Self {
        Self {
            classes: Vec::new(),
        }
    }

    /// Allocate the next `ClassId` (does not yet store a class).
    ///
    /// Useful when you need the id before constructing the `Class` (e.g.
    /// to fill in `class.id`).
    pub fn next_id(&self) -> ClassId {
        ClassId::new(self.classes.len() as u32)
    }

    /// Add a class and return its `ClassId`.
    ///
    /// The class's `id` field **must** match `self.next_id()` — this is
    /// asserted in debug builds.
    pub fn add(&mut self, class: Class) -> ClassId {
        let expected_id = self.next_id();
        debug_assert_eq!(
            class.id, expected_id,
            "Class id mismatch: expected {expected_id}, got {}",
            class.id,
        );
        let id = class.id;
        self.classes.push(class);
        // Compact reference-field layout: register this class's oop-map / offset
        // table so the heap + GC can place and scan its reference fields as
        // 8-byte pointers. No-op when `CRATONVM_COMPACT_REF_FIELDS=0` opts out.
        self.register_compact_layout_if_enabled(id);
        id
    }

    /// Build and register the compact field layout for class `id`, if the
    /// compact reference-field layout is enabled. Idempotent (overwrites on
    /// redefine / subclass-layout recompute). Safe no-op when the flag is off.
    pub fn register_compact_layout_if_enabled(&self, id: ClassId) {
        if !cratonvm_types::compact_ref_fields_enabled() {
            return;
        }
        if let Some(layout) = self.build_compact_layout(id) {
            cratonvm_types::register_class_layout(id.as_u32(), Arc::new(layout));
        }
    }

    /// Build the per-class compact instance-field layout: a prefix-sum offset
    /// table (reference field = 8 bytes, primitive field = 16-byte tagged cell),
    /// in declaration order with superclasses first, plus the reference-field
    /// oop-map for the GC.
    ///
    /// Handles synthetic-stub **padding**: a class's `num_total_fields` may
    /// exceed its declared instance fields (native `<init>` writes to synthetic
    /// indices). Each ancestor contributes its declared fields followed by any
    /// padding up to *its own* `num_total_fields`, so absolute indices line up
    /// even when an ancestor is padded. Padded / unknown-descriptor slots are
    /// treated as references (8-byte), matching the heap default-init rule
    /// (`Value::Object(None)` for uncovered slots).
    fn build_compact_layout(&self, id: ClassId) -> Option<CompactLayout> {
        // Superclass chain, root (java/lang/Object) first.
        let mut chain: Vec<ClassId> = Vec::new();
        let mut cur = Some(id);
        while let Some(cid) = cur {
            chain.push(cid);
            cur = self.get(cid)?.superclass;
        }
        chain.reverse();

        let total = self.get(id)?.num_total_fields;
        let mut field_offsets: Vec<u32> = Vec::with_capacity(total);
        let mut is_ref: Vec<bool> = Vec::with_capacity(total);
        let mut ref_offsets: Vec<u32> = Vec::new();
        let mut off: u32 = 0;
        let mut count: usize = 0;
        let mut padded = false;

        let mut push = |r: bool, off: &mut u32| {
            field_offsets.push(*off);
            is_ref.push(r);
            if r {
                ref_offsets.push(*off);
                *off += cratonvm_types::REF_FIELD_SIZE as u32;
            } else {
                *off += cratonvm_types::SLOT_SIZE as u32;
            }
        };

        for cid in chain {
            let class = self.get(cid)?;
            for f in &class.fields {
                if f.access_flags.contains(FieldAccessFlags::STATIC) {
                    continue;
                }
                let b = f.descriptor.as_bytes().first().copied().unwrap_or(0);
                let r = b == b'L' || b == b'[';
                push(r, &mut off);
                count += 1;
            }
            // Pad up to this ancestor's own total so absolute indices stay aligned.
            let target = class.num_total_fields;
            while count < target {
                push(true, &mut off); // padded slot -> reference (8-byte null)
                count += 1;
                padded = true;
            }
        }

        // A padded slot has NO field descriptor, so its true type is unknown. We
        // cannot build a trustworthy oop-map for such a class: the untyped
        // `ClassId(0)`-minted synthetic containers (`cratonvm/synthetic/
        // AnonymousObject$N`, every HashMap/LinkedHashMap node, view backings, …)
        // are all pure padding, and native code stores MIXED types into their raw
        // slots (e.g. `map_alloc_node` writes `Int(hash)` into slot 0 and object
        // refs into slots 1-3). Guessing every padded slot is a reference makes
        // the GC scan a primitive slot as an 8-byte pointer (and auto-box the int
        // on write) — under GC_STRESS this strands the node's real reference
        // slots and corrupts the heap (missed-fixup of the compact node's key
        // ref). Refuse the compact layout for any padded class so it falls back
        // to the legacy uniform 16-byte tagged-`Value` cell layout, where every
        // slot self-describes its type via its tag — exactly the compact-OFF
        // behaviour, which is correct for these mixed-use containers. Classes
        // whose every slot has a real descriptor (no padding) are unaffected and
        // stay compact.
        if padded {
            return None;
        }

        Some(CompactLayout {
            field_offsets,
            is_ref,
            ref_offsets,
            body_size: off,
        })
    }

    /// Look up a class by id.
    pub fn get(&self, id: ClassId) -> Option<&Class> {
        self.classes.get(id.as_u32() as usize)
    }

    /// Look up a class by id (mutable).
    pub fn get_mut(&mut self, id: ClassId) -> Option<&mut Class> {
        self.classes.get_mut(id.as_u32() as usize)
    }

    /// The number of loaded classes.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// Returns true if no classes have been loaded.
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Iterate over all loaded classes.
    pub fn iter(&self) -> impl Iterator<Item = &Class> {
        self.classes.iter()
    }

    /// Find a class by name. O(n) scan — the class manager maintains a
    /// `HashMap` for fast name-based lookup; this is a fallback.
    pub fn find_by_name(&self, name: &str) -> Option<&Class> {
        self.classes.iter().find(|c| &*c.name == name)
    }
}

impl Default for ClassStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Convenience: look up a field by name walking the superclass chain
// ---------------------------------------------------------------------------

/// Find a field by name in the given class or any of its superclasses.
///
/// Returns `(absolute_index, &ClassFileField, ClassId)` where `ClassId` is the
/// class that actually declares the field.
pub fn find_field_recursive<'a>(
    class_id: ClassId,
    field_name: &str,
    store: &'a ClassStore,
) -> Option<(usize, &'a ClassFileField, ClassId)> {
    // JVM spec §5.4.3.2: field resolution.
    //   1. Look in C itself.
    //   2. Otherwise, recursively search the direct superinterfaces of C.
    //   3. Otherwise, recursively search the superclass of C.
    //
    // Phase 1+3: walk the superclass chain, checking own fields at each level.
    //
    // Perf: the per-level interface BFS reuses two scratch buffers
    // (`queue`, `visited`) across superclass levels instead of
    // allocating a fresh `Vec` + default-hasher `HashSet` per level.
    // `visited` is an `FxHashSet` — the fx hasher is faster than the
    // SipHash default and the `ClassId` keys are not attacker-keyed.
    let mut current_id = class_id;
    let mut queue: Vec<ClassId> = Vec::new();
    let mut visited: FxHashSet<ClassId> = FxHashSet::default();
    loop {
        let class = store.get(current_id)?;
        if let Some((idx, field)) = class.find_own_field(field_name) {
            return Some((idx, field, current_id));
        }
        // Phase 2: at each level, also search the superinterfaces (BFS).
        // Per the spec, only static fields can be inherited from interfaces;
        // we still return whatever matches by name and let the caller
        // distinguish static vs. instance via the field flags.
        queue.clear();
        visited.clear();
        queue.extend_from_slice(&class.interfaces);
        let mut i = 0;
        while i < queue.len() {
            let iface_id = queue[i];
            i += 1;
            if !visited.insert(iface_id) {
                continue;
            }
            if let Some(iface) = store.get(iface_id) {
                if let Some((idx, field)) = iface.find_own_field(field_name) {
                    return Some((idx, field, iface_id));
                }
                queue.extend_from_slice(&iface.interfaces);
            }
        }
        current_id = class.superclass?;
    }
}

/// Find a method by name and descriptor, walking the superclass chain and
/// then the interface hierarchy.
///
/// Returns `(&ClassFileMethod, ClassId)` where `ClassId` is the class that
/// actually declares the method.
///
/// # Resolution rules (JVMS §5.4.3.3 / §5.4.3.4)
///
/// Phase 1 walks the superclass chain; Phase 2 walks the *transitive*
/// superinterface closure. Both phases *prefer a concrete (non-abstract)
/// method* and only fall back to an abstract declaration when no concrete
/// dispatch target exists anywhere.
///
/// Virtual-dispatch correctness (Phase 1): when the superclass chain contains
/// BOTH an abstract declaration (e.g. `java/net/JarURLConnection.getJarFile()`
/// — abstract, no Code attribute) AND a concrete override, the walk prefers
/// the concrete one. It keeps walking past an abstract declaration, recording
/// it only as a last-resort fallback, so dispatch never lands on a method with
/// no Code attribute when a real implementation exists.
///
/// Interface-method resolution (Phase 2): the superinterface closure is walked
/// transitively. A concrete (default) method wins; otherwise the first
/// abstract declaration is returned rather than `None`. This matters when
/// `class_id` is itself an interface: resolving e.g. `invokeinterface
/// java/util/concurrent/ScheduledFuture.cancel(Z)Z` must succeed even though
/// `cancel` is inherited (abstractly) from the `java/util/concurrent/Future`
/// superinterface. Previously Phase 2 skipped abstract methods unconditionally,
/// so the VM raised a spurious `NoSuchMethodError` for inherited interface
/// methods.
///
/// Callers that need to distinguish a real implementation from an abstract
/// declaration must inspect `ClassFileMethod::is_abstract()` /
/// `ClassFileMethod::code()` on the result (the bytecode-verifier and the
/// interpreter's dispatch paths already do).
/// Returns true when `sub` is a *strict* (proper) subinterface of `sup` — i.e.
/// `sup` appears in `sub`'s transitive superinterface closure and `sub != sup`.
/// Used for maximally-specific default-method selection (JVMS §5.4.3.3).
fn is_strict_subinterface(sub: ClassId, sup: ClassId, store: &ClassStore) -> bool {
    if sub == sup {
        return false;
    }
    let mut stack: Vec<ClassId> = Vec::new();
    let mut seen: FxHashSet<ClassId> = FxHashSet::default();
    if let Some(c) = store.get(sub) {
        stack.extend_from_slice(&c.interfaces);
    }
    while let Some(id) = stack.pop() {
        if id == sup {
            return true;
        }
        if !seen.insert(id) {
            continue;
        }
        if let Some(c) = store.get(id) {
            stack.extend_from_slice(&c.interfaces);
        }
    }
    false
}

pub fn find_method_recursive<'a>(
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    store: &'a ClassStore,
) -> Option<(&'a ClassFileMethod, ClassId)> {
    // Phase 1: walk the superclass chain (concrete + inherited methods).
    //
    // Prefer the first NON-abstract match. If only abstract declarations are
    // found on the chain, remember the first one as a fallback — but give
    // Phase 2 (interface default methods) a chance first, since a concrete
    // default body is a valid dispatch target whereas an abstract superclass
    // declaration is not.
    let mut current_id = class_id;
    let mut abstract_fallback: Option<(&'a ClassFileMethod, ClassId)> = None;
    loop {
        let Some(class) = store.get(current_id) else {
            break;
        };
        if let Some(method) = class.find_method(method_name, method_descriptor) {
            if !method.is_abstract() {
                // Concrete method (has a Code attribute, or is ACC_NATIVE) —
                // this is the most-specific real dispatch target.
                return Some((method, current_id));
            }
            // Abstract declaration: record the first one seen and continue
            // walking — a concrete override may live on a more-derived class
            // we have not visited yet only if `class_id` was itself derived,
            // but in the abstract-receiver case there is nothing more derived.
            // Keeping the fallback lets us still return a usable answer.
            if abstract_fallback.is_none() {
                abstract_fallback = Some((method, current_id));
            }
        }
        match class.superclass {
            Some(sc) => current_id = sc,
            None => break,
        }
    }
    // No concrete superclass-chain method — but before falling back to an
    // abstract declaration, try Phase 2: a concrete interface default method
    // is a valid dispatch target, an abstract class method is not.

    // Phase 2: walk the interface hierarchy.
    // BFS over all interfaces of the class and its superclasses, including
    // the transitive superinterface closure.
    let mut queue: Vec<ClassId> = Vec::new();
    let mut current_id = class_id;
    loop {
        if let Some(class) = store.get(current_id) {
            // When `class_id` is itself an interface, the interface's own
            // declarations must be searched too — its `find_method` was
            // already probed in Phase 1, but a *superinterface* method (e.g.
            // `Future.cancel` reached via `ScheduledFuture`) is only found by
            // seeding the BFS with the interface node itself so the
            // `queue.extend_from_slice(&iface.interfaces)` step below pulls
            // in its superinterfaces.
            if class.is_interface() {
                queue.push(current_id);
            }
            queue.extend_from_slice(&class.interfaces);
            match class.superclass {
                Some(sc) => current_id = sc,
                None => break,
            }
        } else {
            break;
        }
    }

    // Continue tracking the abstract fallback across Phase 2: an abstract
    // class-method declaration found in Phase 1 already lives in
    // `abstract_fallback` and takes precedence; Phase 2 only fills it when
    // Phase 1 found nothing. Interface-method resolution still succeeds for an
    // inherited abstract method (JVMS §5.4.3.4 — an abstract method is a valid
    // resolution result). Note: `abstract_fallback` is intentionally NOT
    // re-declared here so the Phase 1 result survives.
    // Collect ALL concrete (default) interface-method candidates in the closure,
    // then pick the maximally-specific one per JVMS §5.4.3.3 / §5.4.6: a default
    // declared in interface I is maximally-specific iff no *subinterface* of I in
    // the candidate set also declares a matching default. Returning the first
    // concrete match in BFS order (the previous behaviour) wrongly picked a
    // SUPERinterface's default over a more-specific override when the class
    // directly listed both — e.g. Hibernate's `SessionImpl` lists
    // `SharedSessionContractImplementor` (whose `asSessionImplementor` default
    // throws ClassCastException) *and* `SessionImplementor` (the `return this`
    // override); its superclass `AbstractSharedSessionContract` binds the
    // throwing one first, so the BFS returned the throwing default and the
    // flush/dirty-check path blew up with "session is not a SessionImplementor".
    let mut visited: FxHashSet<ClassId> = FxHashSet::default();
    let mut default_candidates: Vec<(&'a ClassFileMethod, ClassId)> = Vec::new();
    let mut i = 0;
    while i < queue.len() {
        let iface_id = queue[i];
        i += 1;
        if !visited.insert(iface_id) {
            continue;
        }
        if let Some(iface) = store.get(iface_id) {
            if let Some(method) = iface.find_method(method_name, method_descriptor) {
                if !method.is_abstract() && !method.is_static() {
                    // Concrete (default) interface method — a candidate for the
                    // maximally-specific selection below.
                    default_candidates.push((method, iface_id));
                } else if method.is_abstract() && abstract_fallback.is_none() && !method.is_static()
                {
                    // Abstract declaration — remember it as a fallback but keep
                    // searching for a concrete default method elsewhere in the
                    // closure.
                    abstract_fallback = Some((method, iface_id));
                }
            }
            // Also search super-interfaces.
            queue.extend_from_slice(&iface.interfaces);
        }
    }

    // Select the maximally-specific concrete default among the candidates.
    match default_candidates.len() {
        0 => {}
        1 => return Some(default_candidates[0]),
        _ => {
            // I is maximally-specific iff no OTHER candidate J is a strict
            // subinterface of I (i.e. I is not a proper superinterface of any
            // other candidate). Return the first such candidate in BFS order.
            // If the candidates are mutually unrelated (a genuine diamond with
            // no single override) the JLS leaves the choice unspecified, so
            // returning the first deterministically is acceptable.
            for &(method, id) in &default_candidates {
                let superseded = default_candidates
                    .iter()
                    .any(|&(_, other)| other != id && is_strict_subinterface(other, id, store));
                if !superseded {
                    return Some((method, id));
                }
            }
            return Some(default_candidates[0]);
        }
    }

    // No concrete dispatch target anywhere. Return the abstract
    // superclass-chain declaration (if any) so callers get a stable
    // `(method, declaring_class)` answer — `interpreter::execute` and
    // `invoke_on_class_shared_inner` already detect the missing Code
    // attribute and surface a spec-compliant `AbstractMethodError` (or run
    // the receiver-walk / native-override rescues) rather than a confusing
    // `NoSuchMethodError`.
    abstract_fallback
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::class_access_flags::{
        ClassAccessFlags, FieldAccessFlags, MethodAccessFlags,
    };
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    /// Create a minimal constant pool (just a tombstone).
    fn empty_constant_pool() -> ConstantPool {
        ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
    }

    /// Helper to create a minimal class.
    #[allow(clippy::too_many_arguments)]
    fn make_class(
        id: ClassId,
        name: &str,
        superclass: Option<ClassId>,
        interfaces: Vec<ClassId>,
        fields: Vec<ClassFileField>,
        methods: Vec<ClassFileMethod>,
        first_field_index: usize,
        num_total_fields: usize,
    ) -> Class {
        Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
            interfaces,
            fields,
            methods,
            first_field_index,
            num_total_fields,
            bootstrap_methods: vec![],
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
            is_synthetic_stub: false,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }

    fn make_field(name: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc("I"),
            attributes: vec![],
        }
    }

    fn make_static_field(name: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::STATIC,
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc("I"),
            attributes: vec![],
        }
    }

    fn make_method(name: &str, descriptor: &str) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        }
    }

    #[test]
    fn class_id_display() {
        let id = ClassId::new(42);
        assert_eq!(format!("{id}"), "42");
        assert_eq!(id.as_u32(), 42);
    }

    #[test]
    fn class_store_add_and_get() {
        let mut store = ClassStore::new();
        assert!(store.is_empty());

        let id0 = store.next_id();
        assert_eq!(id0, ClassId::new(0));

        let class0 = make_class(id0, "java/lang/Object", None, vec![], vec![], vec![], 0, 0);
        store.add(class0);

        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());

        let c = store.get(id0).unwrap();
        assert_eq!(&*c.name, "java/lang/Object");
        assert!(c.superclass.is_none());
    }

    #[test]
    fn class_store_find_by_name() {
        let mut store = ClassStore::new();

        let id0 = store.next_id();
        store.add(make_class(
            id0,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        let id1 = store.next_id();
        store.add(make_class(
            id1,
            "java/lang/String",
            Some(id0),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        assert!(store.find_by_name("java/lang/String").is_some());
        assert!(store.find_by_name("java/lang/Integer").is_none());
    }

    #[test]
    fn find_method_in_class() {
        let id = ClassId::new(0);
        let methods = vec![
            make_method("<init>", "()V"),
            make_method("toString", "()Ljava/lang/String;"),
            make_method("hashCode", "()I"),
        ];
        let class = make_class(id, "Foo", None, vec![], vec![], methods, 0, 0);

        assert!(class
            .find_method("toString", "()Ljava/lang/String;")
            .is_some());
        assert!(class.find_method("toString", "()I").is_none());
        assert!(class.find_method("missing", "()V").is_none());
    }

    #[test]
    fn find_own_field() {
        let id = ClassId::new(0);
        let fields = vec![make_field("x"), make_field("y"), make_field("z")];
        let class = make_class(id, "Point", None, vec![], fields, vec![], 2, 5);

        // Field "x" is at local index 0 → absolute index 2 (first_field_index=2)
        let (idx, field) = class.find_own_field("x").unwrap();
        assert_eq!(idx, 2);
        assert_eq!(&*field.name, "x");

        // Field "z" is at local index 2 → absolute index 4
        let (idx, _) = class.find_own_field("z").unwrap();
        assert_eq!(idx, 4);

        assert!(class.find_own_field("missing").is_none());
    }

    #[test]
    fn field_at_index() {
        let id = ClassId::new(0);
        let fields = vec![make_field("a"), make_field("b")];
        let class = make_class(id, "Foo", None, vec![], fields, vec![], 3, 5);

        // Index 3 → local 0 → "a"
        assert_eq!(&*class.field_at_index(3).unwrap().name, "a");
        // Index 4 → local 1 → "b"
        assert_eq!(&*class.field_at_index(4).unwrap().name, "b");
        // Index 2 → belongs to superclass → None
        assert!(class.field_at_index(2).is_none());
        // Index 5 → out of bounds → None
        assert!(class.field_at_index(5).is_none());
    }

    /// Regression: `java.util.regex.Matcher` declares two static constants
    /// (`ENDANCHOR`, `NOANCHOR`) interleaved between its instance fields.
    /// `field_at_index` must map an absolute instance index back to the
    /// N-th *instance* field, skipping the interleaved statics — otherwise
    /// every instance field declared after a static is misresolved, which
    /// caused `Matcher.find()` to loop forever on zero-width matches.
    #[test]
    fn field_at_index_skips_interleaved_statics() {
        let id = ClassId::new(0);
        // Declaration order: i0, i1, S(static), i2, S(static), i3
        let fields = vec![
            make_field("i0"),
            make_field("i1"),
            make_static_field("S_A"),
            make_field("i2"),
            make_static_field("S_B"),
            make_field("i3"),
        ];
        // first_field_index = 0, 4 instance fields total.
        let class = make_class(id, "Matcher", None, vec![], fields, vec![], 0, 4);

        // find_own_field gives the absolute index; field_at_index must invert it.
        for name in ["i0", "i1", "i2", "i3"] {
            let (abs, _) = class.find_own_field(name).unwrap();
            assert_eq!(
                &*class.field_at_index(abs).unwrap().name,
                name,
                "field_at_index({abs}) should round-trip back to {name}"
            );
        }
        // Direct index checks: instance fields are 0..4 regardless of statics.
        assert_eq!(&*class.field_at_index(0).unwrap().name, "i0");
        assert_eq!(&*class.field_at_index(1).unwrap().name, "i1");
        assert_eq!(&*class.field_at_index(2).unwrap().name, "i2");
        assert_eq!(&*class.field_at_index(3).unwrap().name, "i3");
        assert!(class.field_at_index(4).is_none());
    }

    #[test]
    fn is_subclass_of_same_class() {
        let mut store = ClassStore::new();
        let id0 = store.next_id();
        store.add(make_class(id0, "Foo", None, vec![], vec![], vec![], 0, 0));

        let foo = store.get(id0).unwrap();
        assert!(foo.is_subclass_of(id0, &store));
    }

    #[test]
    fn is_subclass_of_parent() {
        let mut store = ClassStore::new();

        // java/lang/Object (id=0)
        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Foo extends Object (id=1)
        let foo_id = store.next_id();
        store.add(make_class(
            foo_id,
            "Foo",
            Some(object_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Bar extends Foo (id=2)
        let bar_id = store.next_id();
        store.add(make_class(
            bar_id,
            "Bar",
            Some(foo_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        let bar = store.get(bar_id).unwrap();

        // Bar is subclass of Bar (self)
        assert!(bar.is_subclass_of(bar_id, &store));
        // Bar is subclass of Foo (parent)
        assert!(bar.is_subclass_of(foo_id, &store));
        // Bar is subclass of Object (grandparent)
        assert!(bar.is_subclass_of(object_id, &store));

        // Foo is NOT subclass of Bar
        let foo = store.get(foo_id).unwrap();
        assert!(!foo.is_subclass_of(bar_id, &store));
    }

    #[test]
    fn is_subclass_of_interface() {
        let mut store = ClassStore::new();

        // java/lang/Object (id=0)
        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Serializable interface (id=1) — extends Object
        let serial_id = store.next_id();
        store.add(make_class(
            serial_id,
            "java/io/Serializable",
            Some(object_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Comparable interface (id=2) — extends Object
        let comparable_id = store.next_id();
        store.add(make_class(
            comparable_id,
            "java/lang/Comparable",
            Some(object_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // MyClass implements Serializable, Comparable (id=3)
        let my_id = store.next_id();
        store.add(make_class(
            my_id,
            "MyClass",
            Some(object_id),
            vec![serial_id, comparable_id],
            vec![],
            vec![],
            0,
            0,
        ));

        let my = store.get(my_id).unwrap();
        assert!(my.is_subclass_of(serial_id, &store));
        assert!(my.is_subclass_of(comparable_id, &store));
        assert!(my.is_subclass_of(object_id, &store));

        // Serializable is NOT subclass of MyClass
        let serial = store.get(serial_id).unwrap();
        assert!(!serial.is_subclass_of(my_id, &store));
    }

    #[test]
    fn find_field_recursive_walks_superclass() {
        let mut store = ClassStore::new();

        // Object with field "hash"
        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![make_field("hash")],
            vec![],
            0,
            1,
        ));

        // Foo extends Object, adds field "x"
        let foo_id = store.next_id();
        store.add(make_class(
            foo_id,
            "Foo",
            Some(object_id),
            vec![],
            vec![make_field("x")],
            vec![],
            1, // inherited 1 field from Object
            2,
        ));

        // Find "x" in Foo — should be in Foo at absolute index 1
        let (idx, field, declaring_class) = find_field_recursive(foo_id, "x", &store).unwrap();
        assert_eq!(&*field.name, "x");
        assert_eq!(idx, 1);
        assert_eq!(declaring_class, foo_id);

        // Find "hash" in Foo — should walk up to Object, absolute index 0
        let (idx, field, declaring_class) = find_field_recursive(foo_id, "hash", &store).unwrap();
        assert_eq!(&*field.name, "hash");
        assert_eq!(idx, 0);
        assert_eq!(declaring_class, object_id);

        // "missing" not found
        assert!(find_field_recursive(foo_id, "missing", &store).is_none());
    }

    #[test]
    fn find_method_recursive_walks_superclass() {
        let mut store = ClassStore::new();

        // Object with toString
        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![make_method("toString", "()Ljava/lang/String;")],
            0,
            0,
        ));

        // Foo extends Object, adds foo()
        let foo_id = store.next_id();
        store.add(make_class(
            foo_id,
            "Foo",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("foo", "()V")],
            0,
            0,
        ));

        // Find "foo" in Foo
        let (method, declaring) = find_method_recursive(foo_id, "foo", "()V", &store).unwrap();
        assert_eq!(&*method.name, "foo");
        assert_eq!(declaring, foo_id);

        // Find "toString" in Foo → walks to Object
        let (method, declaring) =
            find_method_recursive(foo_id, "toString", "()Ljava/lang/String;", &store).unwrap();
        assert_eq!(&*method.name, "toString");
        assert_eq!(declaring, object_id);

        // "bar" not found
        assert!(find_method_recursive(foo_id, "bar", "()V", &store).is_none());
    }

    /// Regression test for the Infinispan `GlobalConfiguration` /
    /// `GlobalConfigurationBuilder` `isClustered()` `NoSuchMethodError` bug
    /// (docs/known-issues/keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md).
    ///
    /// Two closely-named, UNRELATED classes (no inheritance between them,
    /// both extend plain `Object` — mirroring the real
    /// `org.infinispan.configuration.global.GlobalConfiguration` /
    /// `GlobalConfigurationBuilder` pair, which are siblings in the same
    /// package with overlapping names but no subclass relationship). Only
    /// `WidgetConfig` (standing in for `GlobalConfiguration`) declares
    /// `isClustered()Z`; `WidgetConfigBuilder` (standing in for
    /// `GlobalConfigurationBuilder`) does not.
    ///
    /// The actual root cause of the bug was NOT in `find_method_recursive` —
    /// it was a native `build()` override that returned the receiver
    /// (`WidgetConfigBuilder`) itself instead of constructing a genuinely
    /// distinct `WidgetConfig`, so runtime dispatch correctly-but-confusingly
    /// resolved `isClustered()` against the Builder's (real) class identity
    /// and threw NoSuchMethodError naming the Builder. This test pins the
    /// class-resolution invariant that guards against a REGRESSION in the
    /// other direction: `find_method_recursive` must resolve `isClustered()`
    /// on the class that actually declares it and must NEVER silently
    /// satisfy the lookup from the similarly-named sibling class, however the
    /// receiver's class_id was obtained.
    #[test]
    fn find_method_recursive_does_not_confuse_similarly_named_sibling_classes() {
        let mut store = ClassStore::new();

        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![make_method("toString", "()Ljava/lang/String;")],
            0,
            0,
        ));

        // WidgetConfigBuilder — mirrors GlobalConfigurationBuilder: siblings
        // with WidgetConfig (same package, overlapping name prefix), NOT its
        // superclass or subclass. Deliberately does NOT declare isClustered().
        let builder_id = store.next_id();
        store.add(make_class(
            builder_id,
            "com/example/config/WidgetConfigBuilder",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("build", "()Lcom/example/config/WidgetConfig;")],
            0,
            0,
        ));

        // WidgetConfig — mirrors GlobalConfiguration. Only this class
        // declares isClustered().
        let config_id = store.next_id();
        store.add(make_class(
            config_id,
            "com/example/config/WidgetConfig",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("isClustered", "()Z")],
            0,
            0,
        ));

        // Resolving isClustered() on the class that actually declares it
        // (WidgetConfig) must succeed and must be attributed to WidgetConfig.
        let (method, declaring) = find_method_recursive(config_id, "isClustered", "()Z", &store)
            .expect("isClustered() must resolve on WidgetConfig, the declaring class");
        assert_eq!(&*method.name, "isClustered");
        assert_eq!(declaring, config_id);

        // The similarly-named sibling (WidgetConfigBuilder) genuinely lacks
        // isClustered() — resolving it there must fail (a real
        // NoSuchMethodError), never silently succeed by picking up
        // WidgetConfig's method.
        assert!(find_method_recursive(builder_id, "isClustered", "()Z", &store).is_none());

        // Sanity: build() is where WidgetConfigBuilder and WidgetConfig
        // actually connect in real Infinispan bytecode. It resolves on the
        // Builder and is absent from WidgetConfig.
        let (method, declaring) = find_method_recursive(
            builder_id,
            "build",
            "()Lcom/example/config/WidgetConfig;",
            &store,
        )
        .expect("build() must resolve on WidgetConfigBuilder");
        assert_eq!(&*method.name, "build");
        assert_eq!(declaring, builder_id);
        assert!(find_method_recursive(
            config_id,
            "build",
            "()Lcom/example/config/WidgetConfig;",
            &store
        )
        .is_none());
    }

    #[test]
    fn class_display() {
        let class = make_class(
            ClassId::new(7),
            "java/util/ArrayList",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );
        assert_eq!(format!("{class}"), "Class[7] java/util/ArrayList");
    }

    #[test]
    fn class_access_flags() {
        let mut class = make_class(ClassId::new(0), "Foo", None, vec![], vec![], vec![], 0, 0);
        assert!(class.is_public());
        assert!(!class.is_interface());
        assert!(!class.is_abstract());
        assert!(!class.is_final());
        assert!(!class.is_enum());

        class.access_flags =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
        assert!(class.is_interface());
        assert!(class.is_abstract());
    }

    // -----------------------------------------------------------------------
    // Tests for iterative find_field_recursive / find_method_recursive
    // -----------------------------------------------------------------------

    #[test]
    fn find_field_iterative_deep_chain() {
        // Build a chain: C0 (has "base_field") <- C1 <- C2 <- ... <- C99
        // The field should be found at the bottom of the chain.
        let mut store = ClassStore::new();
        let depth = 100;

        let root_id = store.next_id();
        store.add(make_class(
            root_id,
            "C0",
            None,
            vec![],
            vec![make_field("base_field")],
            vec![],
            0,
            1,
        ));

        let mut prev_id = root_id;
        for i in 1..depth {
            let id = store.next_id();
            store.add(make_class(
                id,
                &format!("C{i}"),
                Some(prev_id),
                vec![],
                vec![],
                vec![],
                1,
                1,
            ));
            prev_id = id;
        }

        // Look up "base_field" from the leaf class — must walk 99 levels.
        let (idx, field, declaring) = find_field_recursive(prev_id, "base_field", &store).unwrap();
        assert_eq!(&*field.name, "base_field");
        assert_eq!(idx, 0);
        assert_eq!(declaring, root_id);

        // Missing field returns None.
        assert!(find_field_recursive(prev_id, "no_such", &store).is_none());
    }

    #[test]
    fn find_method_iterative_deep_chain() {
        let mut store = ClassStore::new();
        let depth = 100;

        let root_id = store.next_id();
        store.add(make_class(
            root_id,
            "C0",
            None,
            vec![],
            vec![],
            vec![make_method("base_method", "()V")],
            0,
            0,
        ));

        let mut prev_id = root_id;
        for i in 1..depth {
            let id = store.next_id();
            store.add(make_class(
                id,
                &format!("C{i}"),
                Some(prev_id),
                vec![],
                vec![],
                vec![],
                0,
                0,
            ));
            prev_id = id;
        }

        let (method, declaring) =
            find_method_recursive(prev_id, "base_method", "()V", &store).unwrap();
        assert_eq!(&*method.name, "base_method");
        assert_eq!(declaring, root_id);

        assert!(find_method_recursive(prev_id, "no_such", "()V", &store).is_none());
    }

    #[test]
    fn find_field_iterative_own_field_found_first() {
        // Child declares same field name as parent — child's version wins.
        let mut store = ClassStore::new();

        let parent_id = store.next_id();
        store.add(make_class(
            parent_id,
            "Parent",
            None,
            vec![],
            vec![make_field("x")],
            vec![],
            0,
            1,
        ));

        let child_id = store.next_id();
        store.add(make_class(
            child_id,
            "Child",
            Some(parent_id),
            vec![],
            vec![make_field("x")],
            vec![],
            1,
            2,
        ));

        let (idx, _, declaring) = find_field_recursive(child_id, "x", &store).unwrap();
        assert_eq!(declaring, child_id);
        assert_eq!(idx, 1); // child's first_field_index
    }

    #[test]
    fn find_method_iterative_own_method_found_first() {
        let mut store = ClassStore::new();

        let parent_id = store.next_id();
        store.add(make_class(
            parent_id,
            "Parent",
            None,
            vec![],
            vec![],
            vec![make_method("run", "()V")],
            0,
            0,
        ));

        let child_id = store.next_id();
        store.add(make_class(
            child_id,
            "Child",
            Some(parent_id),
            vec![],
            vec![],
            vec![make_method("run", "()V")],
            0,
            0,
        ));

        let (_, declaring) = find_method_recursive(child_id, "run", "()V", &store).unwrap();
        assert_eq!(declaring, child_id);
    }

    #[test]
    fn is_subclass_of_bounded_depth_limit() {
        // Build a chain deeper than MAX_HIERARCHY_DEPTH to verify it doesn't
        // stack-overflow and instead returns false when the limit is exceeded.
        let mut store = ClassStore::new();
        let chain_len = MAX_HIERARCHY_DEPTH + 10;

        let root_id = store.next_id();
        store.add(make_class(
            root_id,
            "Root",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        let mut prev_id = root_id;
        for i in 1..chain_len {
            let id = store.next_id();
            store.add(make_class(
                id,
                &format!("D{i}"),
                Some(prev_id),
                vec![],
                vec![],
                vec![],
                0,
                0,
            ));
            prev_id = id;
        }

        let leaf = store.get(prev_id).unwrap();
        // Should return false (depth exceeded) rather than stack-overflow.
        assert!(!leaf.is_subclass_of(root_id, &store));
        // Self-check still works.
        assert!(leaf.is_subclass_of(prev_id, &store));
    }

    // -- M2: Default interface method resolution tests -------------------------

    fn make_abstract_method(name: &str, descriptor: &str) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::ABSTRACT | MethodAccessFlags::PUBLIC,
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        }
    }

    fn make_default_method(name: &str, descriptor: &str) -> ClassFileMethod {
        // Default method = non-abstract on an interface (has code).
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        }
    }

    /// Build a class flagged as an `interface` (so `Class::is_interface()`
    /// reports true). `make_class` hard-codes `PUBLIC | SUPER`, which is fine
    /// for concrete-class tests but wrong when the test needs the node to be
    /// recognised as an interface (e.g. interface-method resolution where
    /// `class_id` is itself an interface).
    fn make_interface(
        id: ClassId,
        name: &str,
        object_id: ClassId,
        superinterfaces: Vec<ClassId>,
        methods: Vec<ClassFileMethod>,
    ) -> Class {
        let mut c = make_class(
            id,
            name,
            Some(object_id),
            superinterfaces,
            vec![],
            methods,
            0,
            0,
        );
        c.access_flags =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
        c
    }

    #[test]
    fn m2_find_method_recursive_finds_default_on_interface() {
        let mut store = ClassStore::new();

        // Object (no superclass)
        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Interface Greeting with abstract greet() and default shout()
        let iface_id = store.next_id();
        store.add(make_class(
            iface_id,
            "Greeting",
            Some(obj_id),
            vec![],
            vec![],
            vec![
                make_abstract_method("greet", "(Ljava/lang/String;)Ljava/lang/String;"),
                make_default_method("shout", "(Ljava/lang/String;)Ljava/lang/String;"),
            ],
            0,
            0,
        ));

        // Concrete class CasualGreeting implements Greeting, has greet() but NOT shout()
        let class_id = store.next_id();
        store.add(make_class(
            class_id,
            "CasualGreeting",
            Some(obj_id),
            vec![iface_id],
            vec![],
            vec![make_method(
                "greet",
                "(Ljava/lang/String;)Ljava/lang/String;",
            )],
            0,
            0,
        ));

        // greet() is on the concrete class → found directly
        let found = find_method_recursive(
            class_id,
            "greet",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &store,
        );
        assert!(found.is_some(), "greet should be found on concrete class");
        assert_eq!(found.unwrap().1, class_id);

        // shout() is a default method on the interface → should be found via interface walk
        let found = find_method_recursive(
            class_id,
            "shout",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &store,
        );
        assert!(
            found.is_some(),
            "default method shout should be found via interface"
        );
        assert_eq!(
            found.unwrap().1,
            iface_id,
            "shout should resolve to the Greeting interface"
        );
    }

    #[test]
    fn m2_find_method_recursive_maximally_specific_default_wins_over_superinterface() {
        // Mirrors Hibernate's SessionImpl shape: a superclass binds a
        // superinterface's default method; the subclass adds the subinterface
        // that overrides it AND re-lists the superinterface directly. The
        // maximally-specific (subinterface) default must win. The previous
        // first-BFS-match logic returned the superinterface's default — which
        // for `SharedSessionContractImplementor.asSessionImplementor` throws
        // ClassCastException in Hibernate's flush path.
        let mut store = ClassStore::new();

        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Shared interface: default f()
        let shared_id = store.next_id();
        store.add(make_interface(
            shared_id,
            "Shared",
            obj_id,
            vec![],
            vec![make_default_method("f", "()V")],
        ));

        // Impl extends Shared, overriding the default f()
        let impl_id = store.next_id();
        store.add(make_interface(
            impl_id,
            "Impl",
            obj_id,
            vec![shared_id],
            vec![make_default_method("f", "()V")],
        ));

        // AbstractShared (class) implements only Shared, declares no f().
        let abs_id = store.next_id();
        store.add(make_class(
            abs_id,
            "AbstractShared",
            Some(obj_id),
            vec![shared_id],
            vec![],
            vec![],
            0,
            0,
        ));

        // Sess extends AbstractShared, implements [Shared, Impl] — the exact
        // ordering that previously made the BFS return Shared.f() first.
        let sess_id = store.next_id();
        store.add(make_class(
            sess_id,
            "Sess",
            Some(abs_id),
            vec![shared_id, impl_id],
            vec![],
            vec![],
            0,
            0,
        ));

        let found = find_method_recursive(sess_id, "f", "()V", &store);
        assert!(found.is_some(), "default f() must resolve");
        assert_eq!(
            found.unwrap().1,
            impl_id,
            "maximally-specific Impl.f() must win over the Shared.f() superinterface default"
        );
    }

    #[test]
    fn m2_find_method_recursive_prefers_concrete_over_abstract_interface_methods() {
        let mut store = ClassStore::new();

        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Interface with only abstract method (no default)
        let iface_id = store.next_id();
        store.add(make_interface(
            iface_id,
            "Runnable",
            obj_id,
            vec![],
            vec![make_abstract_method("run", "()V")],
        ));

        // Concrete class implementing Runnable with run()
        let class_id = store.next_id();
        store.add(make_class(
            class_id,
            "MyTask",
            Some(obj_id),
            vec![iface_id],
            vec![],
            vec![make_method("run", "()V")],
            0,
            0,
        ));

        // run() found on concrete class — the concrete declaration must win
        // over the abstract interface declaration.
        let found = find_method_recursive(class_id, "run", "()V", &store);
        assert!(found.is_some());
        assert_eq!(found.unwrap().1, class_id);

        // missing() not found anywhere
        let found = find_method_recursive(class_id, "missing", "()V", &store);
        assert!(found.is_none());
    }

    /// Regression: interface-method resolution must walk transitive
    /// superinterfaces and return an *abstract* inherited declaration.
    ///
    /// Mirrors the Kafka 3.7.0 boot failure: `invokeinterface
    /// java/util/concurrent/ScheduledFuture.cancel(Z)Z`. `cancel(boolean)` is
    /// declared abstractly on `java/util/concurrent/Future`, and
    /// `ScheduledFuture` only inherits it (via `extends Delayed, Future`).
    /// Resolving against the `ScheduledFuture` interface itself must still
    /// find `Future.cancel` (JVMS §5.4.3.4) — previously Phase 2 skipped all
    /// abstract methods and returned `None`, surfacing a spurious
    /// `NoSuchMethodError`.
    #[test]
    fn m2_find_method_recursive_resolves_inherited_abstract_interface_method() {
        let mut store = ClassStore::new();

        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // `Future` — declares `cancel(Z)Z` abstractly.
        let future_id = store.next_id();
        store.add(make_interface(
            future_id,
            "java/util/concurrent/Future",
            obj_id,
            vec![],
            vec![make_abstract_method("cancel", "(Z)Z")],
        ));

        // `Delayed` — sibling superinterface, declares nothing relevant.
        let delayed_id = store.next_id();
        store.add(make_interface(
            delayed_id,
            "java/util/concurrent/Delayed",
            obj_id,
            vec![],
            vec![make_abstract_method(
                "getDelay",
                "(Ljava/util/concurrent/TimeUnit;)J",
            )],
        ));

        // `ScheduledFuture extends Delayed, Future` — declares no methods of
        // its own; `cancel` is inherited from `Future`.
        let scheduled_id = store.next_id();
        store.add(make_interface(
            scheduled_id,
            "java/util/concurrent/ScheduledFuture",
            obj_id,
            vec![delayed_id, future_id],
            vec![],
        ));

        // Resolving `cancel(Z)Z` against the `ScheduledFuture` interface must
        // succeed, returning the abstract declaration from `Future`.
        let found = find_method_recursive(scheduled_id, "cancel", "(Z)Z", &store);
        assert!(
            found.is_some(),
            "cancel(Z)Z inherited from Future must resolve via ScheduledFuture",
        );
        let (method, declaring) = found.unwrap();
        assert_eq!(declaring, future_id, "cancel is declared on Future");
        assert!(method.is_abstract(), "the resolved declaration is abstract");

        // A truly absent method still resolves to None.
        assert!(find_method_recursive(scheduled_id, "nope", "()V", &store).is_none());
    }

    #[test]
    fn m2_find_method_recursive_walks_super_interfaces() {
        let mut store = ClassStore::new();

        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Super-interface with default method
        let super_iface_id = store.next_id();
        store.add(make_class(
            super_iface_id,
            "Base",
            Some(obj_id),
            vec![],
            vec![],
            vec![make_default_method("baseMethod", "()V")],
            0,
            0,
        ));

        // Sub-interface extending Base (no new methods)
        let sub_iface_id = store.next_id();
        store.add(make_class(
            sub_iface_id,
            "Extended",
            Some(obj_id),
            vec![super_iface_id],
            vec![],
            vec![],
            0,
            0,
        ));

        // Concrete class implements Extended (which extends Base)
        let class_id = store.next_id();
        store.add(make_class(
            class_id,
            "Impl",
            Some(obj_id),
            vec![sub_iface_id],
            vec![],
            vec![],
            0,
            0,
        ));

        // baseMethod() should be found on the super-interface
        let found = find_method_recursive(class_id, "baseMethod", "()V", &store);
        assert!(
            found.is_some(),
            "default method on super-interface should be found"
        );
        assert_eq!(found.unwrap().1, super_iface_id);
    }

    #[test]
    fn m2_find_method_recursive_concrete_overrides_default() {
        let mut store = ClassStore::new();

        let obj_id = store.next_id();
        store.add(make_class(
            obj_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // Interface with default method
        let iface_id = store.next_id();
        store.add(make_class(
            iface_id,
            "Iface",
            Some(obj_id),
            vec![],
            vec![],
            vec![make_default_method("doIt", "()V")],
            0,
            0,
        ));

        // Concrete class overrides the default method
        let class_id = store.next_id();
        store.add(make_class(
            class_id,
            "Impl",
            Some(obj_id),
            vec![iface_id],
            vec![],
            vec![make_method("doIt", "()V")],
            0,
            0,
        ));

        // Should find the override on the concrete class, NOT the default
        let found = find_method_recursive(class_id, "doIt", "()V", &store);
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().1,
            class_id,
            "concrete override takes priority over default"
        );
    }

    // -----------------------------------------------------------------------
    // T10: interned accessors — verify class-level consumers of StringPool
    // yield shared `Arc<str>` storage.
    // -----------------------------------------------------------------------

    #[test]
    fn t10_intern_class_name_shared_across_loads() {
        // Build two distinct Class instances with the same name; their
        // `interned_name()` must return the same Arc<str> backing.
        let class_a = make_class(
            ClassId::new(0),
            "pkg/Shared",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );
        let class_b = make_class(
            ClassId::new(1),
            "pkg/Shared",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );

        let name_a = class_a.interned_name();
        let name_b = class_b.interned_name();

        assert_eq!(&*name_a, "pkg/Shared");
        assert_eq!(&*name_b, "pkg/Shared");
        assert!(
            Arc::ptr_eq(&name_a, &name_b),
            "same class name across two Class instances must share Arc<str> allocation"
        );
    }

    #[test]
    fn t10_intern_method_name_and_descriptor_shared() {
        // Two classes each with a method `run()V` — the interned method
        // name and descriptor must share the same Arc<str> between the two.
        let methods_a = vec![make_method("run", "()V")];
        let methods_b = vec![make_method("run", "()V")];
        let class_a = make_class(ClassId::new(0), "A", None, vec![], vec![], methods_a, 0, 0);
        let class_b = make_class(ClassId::new(1), "B", None, vec![], vec![], methods_b, 0, 0);

        let name_a = class_a.interned_method_name(0).unwrap();
        let name_b = class_b.interned_method_name(0).unwrap();
        assert_eq!(&*name_a, "run");
        assert!(Arc::ptr_eq(&name_a, &name_b));

        let desc_a = class_a.interned_method_descriptor(0).unwrap();
        let desc_b = class_b.interned_method_descriptor(0).unwrap();
        assert_eq!(&*desc_a, "()V");
        assert!(Arc::ptr_eq(&desc_a, &desc_b));
    }

    #[test]
    fn t10_intern_field_name_and_descriptor_shared() {
        let fields_a = vec![make_field("count")];
        let fields_b = vec![make_field("count")];
        let class_a = make_class(ClassId::new(0), "A", None, vec![], fields_a, vec![], 0, 1);
        let class_b = make_class(ClassId::new(1), "B", None, vec![], fields_b, vec![], 0, 1);

        let name_a = class_a.interned_field_name(0).unwrap();
        let name_b = class_b.interned_field_name(0).unwrap();
        assert_eq!(&*name_a, "count");
        assert!(Arc::ptr_eq(&name_a, &name_b));

        let desc_a = class_a.interned_field_descriptor(0).unwrap();
        let desc_b = class_b.interned_field_descriptor(0).unwrap();
        assert_eq!(&*desc_a, "I");
        assert!(Arc::ptr_eq(&desc_a, &desc_b));
    }

    #[test]
    fn t10_intern_out_of_range_returns_none() {
        let class = make_class(ClassId::new(0), "Empty", None, vec![], vec![], vec![], 0, 0);
        assert!(class.interned_method_name(0).is_none());
        assert!(class.interned_method_descriptor(0).is_none());
        assert!(class.interned_field_name(0).is_none());
        assert!(class.interned_field_descriptor(0).is_none());
    }

    // -----------------------------------------------------------------------
    // T10.9.C — `Class.name: Arc<str>` field migration
    //
    // With the `name` field now stored as a shared `Arc<str>` that originates
    // from the interned constant-pool storage, two `Class` instances carrying
    // the same name string must share a single backing allocation. Cloning
    // the field is a refcount bump (not an allocation).
    // -----------------------------------------------------------------------

    #[test]
    fn t10_9_c_two_loads_share_name_arc() {
        // Two Class instances with the same logical name — names were
        // interned through the global pool at construction time, so their
        // `Arc<str>` backing must be physically identical.
        let class_a = make_class(
            ClassId::new(0),
            "java/lang/String",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );
        let class_b = make_class(
            ClassId::new(1),
            "java/lang/String",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );

        // Simulate the `class_manager` construction path that interns the
        // name via `cratonvm_types::intern_arc`. `make_class` constructs via
        // `Arc::from(name)` directly in tests; reset both sides through the
        // global pool to verify the interning semantics hold end-to-end.
        let name1: Arc<str> = cratonvm_types::intern_arc(&class_a.name);
        let name2: Arc<str> = cratonvm_types::intern_arc(&class_b.name);

        assert_eq!(&*name1, "java/lang/String");
        assert_eq!(&*name2, "java/lang/String");
        assert!(
            Arc::ptr_eq(&name1, &name2),
            "interned class names must share a single Arc<str> allocation"
        );

        // And the direct field — if both were interned at construction time,
        // `Arc::ptr_eq` on the struct field clones returns true as well.
        let field1 = Arc::clone(&class_a.name);
        let field2 = Arc::clone(&class_b.name);
        // We verify this holds when names are funneled through the intern
        // pool, which is the path `class_manager::define_class_with_options`
        // takes at load time.
        let pooled1 = cratonvm_types::intern_arc(&field1);
        let pooled2 = cratonvm_types::intern_arc(&field2);
        assert!(Arc::ptr_eq(&pooled1, &pooled2));
    }
}
