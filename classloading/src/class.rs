// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime class representation.
//!
//! After a `.class` file is parsed by the reader, the VM wraps it into a `Class`
//! that holds resolved superclass/interface references as `ClassId`s and supports
//! field/method lookup and subclass checking.

use std::fmt;
use std::sync::Arc;

use crate::class_origin::ClassOrigin;
use crate::loader_flags;
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

    /// Provenance of this class — where its definition actually came from.
    ///
    /// **Authoritative, and now the only answer.** Ask
    /// `origin.is_compatibility_stub()` for the census / `--jdk-only` policy
    /// question, and [`Class::dispatch_lacks_class_file`] for the dispatch one.
    /// The derived `is_synthetic_stub` bool that used to mirror this field —
    /// and conflate those two questions — was deleted 2026-08-06 (contract §5:
    /// "Convert it to a pure derived mirror now, delete it in a later wave").
    /// Written only through [`Class::set_origin`].
    ///
    /// `--jdk-only` rejects exactly one variant of this enum
    /// ([`ClassOrigin::CompatibilityStub`]); every other way a class can come
    /// into existence — boot image, classpath, user loader, array synthesis,
    /// hidden class, lambda, proxy, reflection accessor — is legitimate in both
    /// modes. See `docs/book/src/user-guide/jdk-only-mode.md` §1 and §5.
    pub origin: ClassOrigin,

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
    /// `Some(_)` for an array type whose component is a **reference** type
    /// (`[Lp/X;`, `[[I`, …); `None` for an ordinary class or interface **and**
    /// for a primitive-component array (`[I`, `[Z`, …), whose component has no
    /// `ClassId` in the store — primitive pseudo-classes are surfaced lazily by
    /// `Class.getPrimitiveClass` and never registered here. The VM
    /// **synthesises** array classes from the resolved component class; JVMS
    /// §5.3.3 forbids any classpath I/O for them.
    ///
    /// The recorded [`ArrayInfo::component_class_id`] is the array class's
    /// *identity witness*: per JVMS §5.3.3 the array's defining loader is the
    /// defining loader of that exact component, so two array classes with the
    /// same name and different components are genuinely different classes.
    /// `ClassManager::load_array_class_for_loader` reads it to decide whether a
    /// pre-existing bootstrap-keyed array class may be re-keyed or must be left
    /// alone — see `array-class-defining-loader.md`.
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

    /// Lazily-computed cache for [`Class::generated_record_object_methods`].
    ///
    /// `0` means "not computed yet"; a computed value always has
    /// [`RECORD_OBJ_COMPUTED`] set, so a record with none of the three
    /// generated bodies still caches a non-zero answer. Computing the answer
    /// needs a linear method scan plus a bytecode-shape check — far too
    /// expensive to repeat on the record `hashCode`/`equals` hot path, where
    /// a single `HashMap` probe keyed by a record runs it once per component.
    pub record_object_methods: std::sync::atomic::AtomicU8,
}

/// Bit set in [`Class::generated_record_object_methods`] for a javac-generated
/// `hashCode()I` body (`aload_0; invokedynamic ObjectMethods; ireturn`).
pub const RECORD_OBJ_HASH_CODE: u8 = 1 << 0;
/// Bit set in [`Class::generated_record_object_methods`] for a javac-generated
/// `equals(Ljava/lang/Object;)Z` body.
pub const RECORD_OBJ_EQUALS: u8 = 1 << 1;
/// Bit set in [`Class::generated_record_object_methods`] for a javac-generated
/// `toString()Ljava/lang/String;` body.
pub const RECORD_OBJ_TO_STRING: u8 = 1 << 2;
/// Marker bit distinguishing "computed, none generated" from "not computed".
pub const RECORD_OBJ_COMPUTED: u8 = 1 << 7;

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
    /// the inner array class `[I`.
    ///
    /// Primitive-component arrays (`[I`, `[Z`, …) carry no `ArrayInfo` at all
    /// (the enclosing `Class::array_info` is `None`), because a primitive
    /// pseudo-class has no `ClassId` in the `ClassStore` and inventing one here
    /// would make this field lie about class identity.
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

/// Counts in-place changes to any class's provenance — see [`class_origin_epoch`].
static CLASS_ORIGIN_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times any class's [`ClassOrigin`] has been rewritten in place.
///
/// Consumers that memoize something derived from a class's provenance —
/// specifically whether it is a synthetic stub — can hold their entry against
/// this value and drop it when the number moves. That is what makes such a
/// memo safe: a stub's field descriptors are placeholders that must not be
/// trusted, but a stub can be *promoted* to the real class at any time
/// ([`ClassManager::upgrade_synthetic_class`]), and a cached "this is a stub"
/// answer would then be wrong forever.
///
/// Only IN-PLACE changes need to bump this. A brand-new class gets a fresh
/// `ClassId`, so no entry keyed by that id can exist to go stale.
/// [`Class::set_origin`] is the documented single writer of the provenance
/// pair, which is why the bump lives there.
#[inline]
pub fn class_origin_epoch() -> u64 {
    CLASS_ORIGIN_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// The dedupe set for [`Class::is_subclass_of_inner`]'s DAG walk.
///
/// # Why not a hash set
///
/// It was `FxHashSet<ClassId>`, pre-sized to 16 — one heap allocation plus a
/// hash and a probe per node visited, for a set that holds a handful of `u32`s.
/// `perf record -F 999` over `probes/KeySetBench hoisted` (the
/// `native-collections` per-element floor written up in
/// `springboot-configurationpropertysources-native-collections-floor-20260821-RETIRED.md`,
/// now under `docs/internal/fixed-suite-bugs/springboot/`)
/// attributed **9.91 %** of the whole process
/// to `hashbrown::HashMap<ClassId, ()>::insert`, plus a further 4.59 % to
/// `is_subclass_of_inner` itself: the largest single leaf in that profile,
/// above every GC and dispatch frame, and reached from the collection natives'
/// receiver-shape tests, which run once per element.
///
/// A linear scan over a stack array of `u32`s has no allocation, no hashing and
/// no indirection, and at these sizes it is not close. The set is small by
/// construction — one entry per node of the (superclass, interface) DAG
/// actually walked — and the widest real-JDK shapes here (the `Collection`
/// family, `CompletableFuture`, `Function`) are a couple of dozen nodes.
///
/// The `Vec` spill exists so that a pathological hierarchy degrades to a longer
/// linear scan rather than losing the dedupe and going exponential, which is
/// the failure the set was introduced to prevent in the first place.
struct VisitedClasses {
    inline: [u32; VISITED_INLINE],
    len: usize,
    spill: Vec<u32>,
}

/// Inline capacity of [`VisitedClasses`], in `ClassId`s. 16 `u32`s is one cache
/// line; 32 is two, and covers every real-JDK interface DAG measured here.
const VISITED_INLINE: usize = 32;

impl VisitedClasses {
    #[inline]
    fn new() -> Self {
        Self {
            inline: [0; VISITED_INLINE],
            len: 0,
            spill: Vec::new(),
        }
    }

    /// `true` when `id` was NOT already present — the same sense as
    /// `HashSet::insert`, so the caller's `if !visited.insert(..) { return }`
    /// reads unchanged.
    #[inline]
    fn insert(&mut self, id: ClassId) -> bool {
        let v = id.as_u32();
        if self.inline[..self.len].contains(&v) {
            return false;
        }
        if !self.spill.is_empty() && self.spill.contains(&v) {
            return false;
        }
        if self.len < VISITED_INLINE {
            self.inline[self.len] = v;
            self.len += 1;
        } else {
            self.spill.push(v);
        }
        true
    }
}

impl Class {
    // ----- Provenance ------------------------------------------------------

    /// **Question (2), and only question (2):** does this class have no class
    /// file behind it, so dispatch must look for a native registered under its
    /// **own exact name** instead of walking the hierarchy to an inherited
    /// `java/lang/Object` body?
    ///
    /// The `is_synthetic_stub` bool this replaced (deleted 2026-08-06) answered two
    /// unrelated questions at once:
    ///
    /// 1. *is this a compatibility substitution?* — the census and `--jdk-only`
    ///    policy question. [`Class::origin`] is authoritative for that one, and
    ///    `origin.is_compatibility_stub()` is how you ask it.
    /// 2. *does this class have no bytecode of its own?* — this one, which the
    ///    dispatch sites were actually asking.
    ///
    /// The two coincided only because every class the VM fabricated was tagged
    /// `CompatibilityStub`, including ones that are not compatibility
    /// substitutions at all. `java/lang/reflect/Proxy$Instance` — the shared
    /// synthetic supertype of every generated `$ProxyN` — is
    /// [`ClassOrigin::VmInternal`]: a generation artefact, not a stand-in for
    /// absent bytes. Correcting its census row must not silently move it off
    /// the dispatch branches it needs.
    ///
    /// # Why not `!origin.has_real_bytes()`
    ///
    /// Because [`ClassOrigin::VmInternal`] and [`ClassOrigin::VmArray`] both
    /// answer "no real bytes", so that spelling would ALSO move every
    /// `cratonvm/synthetic/AnonymousObject$N` (the allocation shape behind every
    /// `HashMap` node in the VM) and every `cratonvm/synthetic/AmbiguousName$…`
    /// stand-in onto branches they take today's non-stub arm of. That is a
    /// `Compatible`-mode behaviour change on the busiest allocation shape there
    /// is.
    ///
    /// # Why the method table is the discriminator
    ///
    /// A fabricated class's *only* callable surface is the native registered
    /// under its own name; `synthetic_stub_ctor_methods` declares NATIVE-flagged
    /// entries for exactly the fabricated names that have one. An
    /// `AnonymousObject$N` has an empty method table and no registration —
    /// nothing to find under its own name, so it belongs on the non-stub arm,
    /// which is where it already is. `Proxy$Instance` has a NATIVE-flagged
    /// `<init>` and native registrations in `reflect_annotations.rs`, so it
    /// belongs on the stub arm, which is also where it already is.
    ///
    /// This predicate is therefore **equal to `is_synthetic_stub` for every
    /// class in the store today**, before and after the `Proxy$Instance` flip —
    /// which is the property `jdk_only_class_origin.rs`'s
    /// `dispatch_predicate_matches_the_stub_bit` pins.
    ///
    /// Cost: one enum discriminant test for every real class (the common case
    /// returns on the `_` arm without touching `methods`). Only the fabricated
    /// and VM-internal classes, whose method tables hold 0–12 entries, pay the
    /// scan.
    pub fn dispatch_lacks_class_file(&self) -> bool {
        match &self.origin {
            // Fabricated: no class file was ever found, by definition.
            ClassOrigin::CompatibilityStub { .. } => true,
            // VM-invented: only the ones that carry native-backed methods under
            // their own name need the exact-name lookup.
            ClassOrigin::VmInternal => self.methods.iter().any(|m| m.is_native()),
            _ => false,
        }
    }

    /// Record where this class came from.
    ///
    /// **Every write to `origin` must go through here**, because it also bumps
    /// [`class_origin_epoch`] — the invalidation signal for every memo derived
    /// from provenance. Assigning the field directly leaves those memos serving
    /// a pre-upgrade answer.
    ///
    /// The in-place "a real `.class` turned up, upgrade the stub" path in
    /// `ClassManager::upgrade_synthetic_class` is the case that matters most —
    /// it installs the real origin here, and until 2026-08-06 it also had to
    /// clear a derived `is_synthetic_stub` mirror in the same breath or real
    /// bytes kept losing to a fabricated stand-in. There is no mirror left to
    /// forget.
    pub fn set_origin(&mut self, origin: ClassOrigin) {
        self.origin = origin;
        // Invalidate every memo derived from provenance — see
        // [`class_origin_epoch`]. Unconditional rather than gated on the
        // stub bit actually changing: this runs once per class definition or
        // upgrade, never on a hot path, and a bump that was not strictly
        // required only costs a re-resolve.
        CLASS_ORIGIN_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

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

    /// Which of `hashCode`/`equals`/`toString` this record declares with the
    /// **javac-generated** body — a bare `invokedynamic` against
    /// `java.lang.runtime.ObjectMethods.bootstrap` (JEP 395).
    ///
    /// The result is `RECORD_OBJ_COMPUTED | <bits>`; a non-record, or a record
    /// that hand-writes all three, yields just `RECORD_OBJ_COMPUTED`. The
    /// answer is memoised in [`Class::record_object_methods`].
    ///
    /// Callers use this to run those three methods through the VM's direct
    /// record implementation instead of interpreting the `invokedynamic`. A
    /// hand-written override must NOT be diverted, hence the exact body-shape
    /// check rather than a name match: javac emits precisely
    ///
    /// * `hashCode()I`                    — `aload_0; invokedynamic; ireturn`
    /// * `equals(Ljava/lang/Object;)Z`    — `aload_0; aload_1; invokedynamic; ireturn`
    /// * `toString()Ljava/lang/String;`   — `aload_0; invokedynamic; areturn`
    ///
    /// and the `invokedynamic`'s bootstrap method is verified to be
    /// `ObjectMethods.bootstrap` before the bit is set.
    pub fn generated_record_object_methods(&self) -> u8 {
        use std::sync::atomic::Ordering;
        let cached = self.record_object_methods.load(Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }
        let mut bits = RECORD_OBJ_COMPUTED;
        if self.is_record() {
            if self.record_object_body_is_generated("hashCode", "()I", &[0x2a], 0xac) {
                bits |= RECORD_OBJ_HASH_CODE;
            }
            if self.record_object_body_is_generated(
                "equals",
                "(Ljava/lang/Object;)Z",
                &[0x2a, 0x2b],
                0xac,
            ) {
                bits |= RECORD_OBJ_EQUALS;
            }
            if self.record_object_body_is_generated(
                "toString",
                "()Ljava/lang/String;",
                &[0x2a],
                0xb0,
            ) {
                bits |= RECORD_OBJ_TO_STRING;
            }
        }
        // Racing threads compute the same value, so a plain store is fine.
        self.record_object_methods.store(bits, Ordering::Relaxed);
        bits
    }

    /// `true` when `name`/`descriptor` is declared by this class with exactly
    /// `prologue` (the `aload_*` receiver/argument pushes), one
    /// `invokedynamic` bound to `ObjectMethods.bootstrap`, and `ret_opcode`.
    fn record_object_body_is_generated(
        &self,
        name: &str,
        descriptor: &str,
        prologue: &[u8],
        ret_opcode: u8,
    ) -> bool {
        let Some(method) = self.find_method(name, descriptor) else {
            return false;
        };
        // `ClassFileMethod::code()` only sees an ALREADY-decoded Code
        // attribute, and the caller runs at inline-cache fill time — before
        // this body has ever executed, so the attribute is still `Raw` and
        // `code()` reads as absent. (Relying on `code()` here silently
        // disabled the whole fast path: every record cached "not generated"
        // on its first call and kept it forever.) Decode on demand instead;
        // the answer is memoised by the caller, so this runs once per class.
        for attribute in &method.attributes {
            let Some(decoded) = attribute.decoded_or_decode(&self.constant_pool) else {
                continue;
            };
            if let cratonvm_reader::attribute::Attribute::Code(code) = &*decoded {
                return self.code_is_generated_object_method(&code.code, prologue, ret_opcode);
            }
        }
        false
    }

    /// The bytecode-shape half of [`Class::record_object_body_is_generated`].
    fn code_is_generated_object_method(
        &self,
        body: &[u8],
        prologue: &[u8],
        ret_opcode: u8,
    ) -> bool {
        if body.len() != prologue.len() + 6 {
            return false;
        }
        if !body.starts_with(prologue) {
            return false;
        }
        let indy_at = prologue.len();
        if body[indy_at] != 0xba
            || body[indy_at + 3] != 0
            || body[indy_at + 4] != 0
            || body[indy_at + 5] != ret_opcode
        {
            return false;
        }
        let cp_index = ((body[indy_at + 1] as u16) << 8) | body[indy_at + 2] as u16;
        self.invokedynamic_uses_object_methods_bootstrap(cp_index)
    }

    /// Resolve an `InvokeDynamic` constant-pool entry through the
    /// `BootstrapMethods` attribute and report whether its bootstrap method is
    /// `java.lang.runtime.ObjectMethods.bootstrap`.
    fn invokedynamic_uses_object_methods_bootstrap(&self, cp_index: u16) -> bool {
        use cratonvm_reader::constant_pool::ConstantPoolEntry;
        let bsm_attr_index = match self.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::InvokeDynamic {
                bootstrap_method_attr_index,
                ..
            }) => *bootstrap_method_attr_index as usize,
            _ => return false,
        };
        let Some(bsm) = self.bootstrap_methods.get(bsm_attr_index) else {
            return false;
        };
        let method_ref_index = match self.constant_pool.get(bsm.bootstrap_method_ref) {
            Some(ConstantPoolEntry::MethodHandle {
                reference_index, ..
            }) => *reference_index,
            _ => return false,
        };
        let (class_index, name_and_type_index) = match self.constant_pool.get(method_ref_index) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return false,
        };
        if self.constant_pool.get_class_name(class_index) != Some("java/lang/runtime/ObjectMethods")
        {
            return false;
        }
        matches!(
            self.constant_pool.get_name_and_type(name_and_type_index),
            Some(("bootstrap", _))
        ) && self.object_methods_args_are_canonical(&bsm.bootstrap_arguments)
    }

    /// `true` when `args`, the static arguments of an `ObjectMethods.bootstrap`
    /// site in this class, are javac's shape for this record: the record class
    /// itself, a name list, then one `REF_getField` of THIS class per record
    /// component, in component order, each naming the instance field the
    /// component is stored in (instance field `i` of this class, which is slot
    /// `first_field_index + i`). Only such a site is answered exactly by the
    /// VM's whole-record walk (`intrinsics::record`); any other getter list
    /// (a subset, another order, another class's fields, an accessor method)
    /// is not. Interpreter round i1 wave 39, lane L4
    /// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`). The name
    /// list is not checked: the JDK ignores it for `equals`/`hashCode`, and
    /// the native `toString` renders the site's own list.
    ///
    /// Only for a `final` class (interpreter round i1 wave 40, lane L4): the
    /// whole-record walk reads the RECEIVER's class, which for a hand-assembled
    /// non-`final` record can be a subclass (`makeToString` names the linked
    /// record class, `makeEquals` tests `isInstance`, and the subclass declares
    /// no components). Such a record's sites, its intrinsics and its
    /// component fast path then take the getter-driven route. javac records
    /// are always `final`.
    pub fn object_methods_args_are_canonical(&self, args: &[u16]) -> bool {
        use cratonvm_reader::constant_pool::ConstantPoolEntry;
        let cp = &self.constant_pool;
        let components = &self.record_components;
        if !self.is_final() {
            return false;
        }
        if args.len() != components.len() + 2 || cp.get_class_name(args[0]) != Some(&*self.name) {
            return false;
        }
        let mut instance_fields = self.fields.iter().filter(|f| !f.is_static());
        for (component, &arg) in components.iter().zip(&args[2..]) {
            let Some(ConstantPoolEntry::MethodHandle {
                reference_kind: 1,
                reference_index,
            }) = cp.get(arg)
            else {
                return false;
            };
            let Some(ConstantPoolEntry::FieldReference {
                class_index,
                name_and_type_index,
            }) = cp.get(*reference_index)
            else {
                return false;
            };
            if cp.get_class_name(*class_index) != Some(&*self.name) {
                return false;
            }
            let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) else {
                return false;
            };
            if name != component.name || descriptor != component.descriptor {
                return false;
            }
            match instance_fields.next() {
                Some(field) if &*field.name == name && &*field.descriptor == descriptor => {}
                _ => return false,
            }
        }
        true
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

    /// Find a field declared in **this** class by name, ignoring its descriptor.
    ///
    /// Returns `(absolute_index, &field)` where `absolute_index` accounts for
    /// inherited fields (i.e. it equals `first_field_index + local_offset`).
    ///
    /// To search inherited fields, call this on the superclass via `ClassStore`.
    ///
    /// # A name is not a field's identity
    ///
    /// JVMS §4.5 forbids two fields in one class file sharing a name **and**
    /// descriptor — it permits them to share a name alone. So this function's
    /// key is not unique, and when it is not, it returns whichever such field
    /// comes first in declaration order.
    ///
    /// That is fine for the callers that pass a name they own and know to be
    /// unique (`"value"`, `"hash"`, `"coder"`, `"size"`). It is NOT fine for
    /// resolving a `CONSTANT_Fieldref`, which JVMS §5.4.3.2 resolves by name
    /// **and** descriptor: use [`Self::find_own_field_by_descriptor`] there.
    /// Resolving a fieldref through this function selects a field of the wrong
    /// type and hands the caller its slot index, which is how a compiled
    /// `putfield` comes to write one field's tag at another field's slot.
    pub fn find_own_field(&self, name: &str) -> Option<(usize, &ClassFileField)> {
        self.find_own_field_matching(|f| &*f.name == name)
    }

    /// [`Self::find_own_field`] with the JVMS §5.4.3.2 key: name **and**
    /// descriptor. This is the correct lookup for a `CONSTANT_Fieldref`.
    pub fn find_own_field_by_descriptor(
        &self,
        name: &str,
        descriptor: &str,
    ) -> Option<(usize, &ClassFileField)> {
        self.find_own_field_matching(|f| &*f.name == name && &*f.descriptor == descriptor)
    }

    /// The index arithmetic both lookups share.
    ///
    /// A static field's index is its position among the static fields; an
    /// instance field's is `first_field_index + position among instance
    /// fields`. `self.fields` holds both kinds interleaved in declaration
    /// order, so neither counter can be derived from the loop index — see
    /// [`Self::field_at_index`], which is the inverse of this mapping.
    fn find_own_field_matching(
        &self,
        matches: impl Fn(&ClassFileField) -> bool,
    ) -> Option<(usize, &ClassFileField)> {
        let mut static_idx = 0usize;
        let mut instance_idx = 0usize;
        for field in &self.fields {
            if matches(field) {
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
    /// **Visited set:** the recursion uses a [`VisitedClasses`] stack array to
    /// dedupe
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
        // The primary-supers display answers a class target in one compare
        // (see `ClassStore::primary_display_verdict`); the secondary-supers
        // closure answers the rest by binary search
        // (`ClassStore::secondary_supers_verdict`). Only for the class the
        // store actually holds under `self.id` — a `Class` value that is not
        // (yet) stored has no display of its own.
        if store
            .get(self.id)
            .is_some_and(|stored| std::ptr::eq(stored, self))
        {
            let verdict = match store.primary_display_verdict(self.id, other_id) {
                // Answered without the closure; publish it anyway, so the next
                // question about this class is answered without the lock
                // (`PublishedSupers::verdict`).
                Some(verdict) => {
                    store.ensure_secondary_supers_published(self.id);
                    Some(verdict)
                }
                None => store.secondary_supers_verdict(self.id, other_id),
            };
            if let Some(verdict) = verdict {
                debug_assert_eq!(
                    verdict,
                    self.is_subclass_of_walk(other_id, store),
                    "subtype display disagrees with the DAG walk: {} <: {}",
                    self.id,
                    other_id
                );
                return verdict;
            }
        }
        self.is_subclass_of_walk(other_id, store)
    }

    /// The DAG walk behind [`Self::is_subclass_of`] (the display's fallback and
    /// its debug-build cross-check).
    fn is_subclass_of_walk(&self, other_id: ClassId, store: &ClassStore) -> bool {
        // NOT a hash set. Pre-sizing an `FxHashSet` to 16 removed the rehash
        // but kept the allocation and the hashing, and `perf record` still put
        // `hashbrown::HashMap<ClassId, ()>::insert` at 9.91 % of the whole
        // process on `probes/KeySetBench hoisted`. See [`VisitedClasses`].
        let mut visited = VisitedClasses::new();
        self.is_subclass_of_inner(other_id, store, 0, &mut visited)
    }

    fn is_subclass_of_inner(
        &self,
        other_id: ClassId,
        store: &ClassStore,
        depth: usize,
        visited: &mut VisitedClasses,
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

    /// Loader-identity-blind fallback for [`is_subclass_of`]: walks this
    /// class's superclass chain (exception `catch_type`s are always
    /// classes, never interfaces, per JVMS SS4.7.3 / SS6.5.athrow, so the
    /// interface walk that `is_subclass_of` performs is intentionally
    /// skipped here) comparing each ancestor's OWN name to `target_name`
    /// textually, ignoring `ClassId` identity entirely.
    ///
    /// Exists because exception-handler `catch_type` resolution
    /// (`find_exception_handler*`/`route_jit_exception_through_method` in
    /// `vm/src/runtime/interpreter.rs`) resolves the catch class via a flat,
    /// global, name-only lookup (`ClassManager::find_class_by_name`) rather
    /// than the loader-faithful `resolve_class_loader_aware` used for
    /// `new`/`checkcast`/`instanceof`/`ldc X.class`. When a library class
    /// (e.g. a third-party jar's exception type) ends up loaded under two
    /// different `ClassId`s by two different loaders -- which legitimately
    /// happens when a `ClassLoader` subclass has a `findClass` fallback that
    /// (incorrectly, or as an artifact of CratonVM's simplified loader
    /// model) re-defines a class its parent chain could already serve, e.g.
    /// Spring's `CompileWithForkedClassLoaderClassLoader` forking a new
    /// loader per test method -- a `throw` from code loaded by loader A can
    /// carry an exception whose `ClassId` never equals the `ClassId` that
    /// `find_class_by_name` deterministically returns for the same simple
    /// name (typically whichever copy was registered first). The primary,
    /// `ClassId`-based `is_subclass_of` check then wrongly reports "no
    /// match" and the exception incorrectly escapes a `catch` block that
    /// should have caught it (observed as AssertJ's `IntrospectionError`
    /// escaping `PropertyOrFieldSupport.getSimpleValue`'s own
    /// `catch (IntrospectionError e)`).
    ///
    /// This is an intentional simplification: two *genuinely* different
    /// classes sharing the same fully-qualified name under different
    /// loaders (deliberate sandboxing) would also match here. That mirrors
    /// the same accepted tradeoff already made elsewhere in this codebase
    /// (e.g. `native_class_get_declaring_class`'s loader-aware fallback) --
    /// given CratonVM's flat global class store, treating identically-named
    /// classes as "the same" for control-flow purposes is far less harmful
    /// than silently letting a same-bytecode exception go uncaught.
    pub fn is_subclass_of_by_name(&self, target_name: &str, store: &ClassStore) -> bool {
        if &*self.name == target_name {
            return true;
        }
        // Same stack-array dedupe set as `is_subclass_of`; see [`VisitedClasses`].
        let mut visited = VisitedClasses::new();
        self.is_subclass_of_by_name_inner(target_name, store, 0, &mut visited)
    }

    /// Loader-identity-blind assignability: like [`Self::is_subclass_of`] but
    /// comparing each node's fully-qualified NAME to `target_name` instead of
    /// `ClassId` identity, walking BOTH the superclass chain and the interface
    /// DAG (unlike [`Self::is_subclass_of_by_name`], whose supers-only walk is
    /// specific to exception `catch_type`s, which are never interfaces).
    ///
    /// Exists for JIT `checkcast`/`instanceof` (`jit_typecheck_resolve` in
    /// `vm/src/jit/helpers.rs`): the compiled artifact carries only the target
    /// class NAME, and resolving that name at run time can land on a
    /// *different* `ClassId` than the receiver's when the same class got
    /// defined twice by two loaders (e.g. Spring's
    /// AOT-processing/`CompileWithForkedClassLoader` child loaders re-defining
    /// app classes — the exact shape behind `SpringBootContextLoaderAotTests`'
    /// Residual 6, where a JIT-compiled `checkcast
    /// org/codehaus/groovy/reflection/ClassInfo` refused the cast between two
    /// same-named `ClassInfo` copies and silently nulled Groovy's registry
    /// lookups).
    ///
    /// **This is now the LAST resort, not the first.** The justification used
    /// to be "CratonVM has a flat global class store, so identically-named
    /// classes may as well be the same class". That premise is gone: the class
    /// dictionary is keyed by `(ClassLoaderId, name)`, and the JIT compiler
    /// resolves each type-check site's `CONSTANT_Class` entry through the
    /// compiling class's own loader and interns the site under the resulting
    /// `ClassId` (`cratonvm_jit::intern_typecheck_target`). A site with a
    /// recorded target answers by identity and never reaches this function —
    /// so a same-named class from a different loader is refused, which is what
    /// loader isolation means.
    ///
    /// What still reaches here is a site whose target was NOT loaded when the
    /// method was compiled, so the compiler had no id to record. For those the
    /// old tradeoff stands: accepting an identically-named class is less
    /// harmful than failing a cast the interpreter's loader-faithful CP
    /// resolution would have passed.
    pub fn is_assignable_to_name(&self, target_name: &str, store: &ClassStore) -> bool {
        // A stack-array dedupe set, not an `FxHashSet`: this runs on every
        // by-name (unrecorded-site) negative JIT type check and on the first
        // negative per epoch at recorded sites, and the hash set cost one heap
        // allocation plus a hash per node. See [`VisitedClasses`].
        let mut visited = VisitedClasses::new();
        self.is_assignable_to_name_inner(target_name, store, 0, &mut visited)
    }

    fn is_assignable_to_name_inner(
        &self,
        target_name: &str,
        store: &ClassStore,
        depth: usize,
        visited: &mut VisitedClasses,
    ) -> bool {
        if depth > MAX_HIERARCHY_DEPTH {
            return false;
        }
        if &*self.name == target_name {
            return true;
        }
        if !visited.insert(self.id) {
            return false;
        }
        if let Some(super_id) = self.superclass {
            if let Some(super_class) = store.get(super_id) {
                if super_class.is_assignable_to_name_inner(target_name, store, depth + 1, visited) {
                    return true;
                }
            }
        }
        for &iface_id in &self.interfaces {
            if let Some(iface_class) = store.get(iface_id) {
                if iface_class.is_assignable_to_name_inner(target_name, store, depth + 1, visited) {
                    return true;
                }
            }
        }
        false
    }

    fn is_subclass_of_by_name_inner(
        &self,
        target_name: &str,
        store: &ClassStore,
        depth: usize,
        visited: &mut VisitedClasses,
    ) -> bool {
        if depth > MAX_HIERARCHY_DEPTH {
            return false;
        }
        if !visited.insert(self.id) {
            return false;
        }
        let Some(super_id) = self.superclass else {
            return false;
        };
        let Some(super_class) = store.get(super_id) else {
            return false;
        };
        if &*super_class.name == target_name {
            return true;
        }
        super_class.is_subclass_of_by_name_inner(target_name, store, depth + 1, visited)
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
    /// This store's compact-layout domain. `class_id` is a per-store INDEX, so
    /// it does not identify a class in the process-global layout registry; the
    /// domain is what does. Allocated once per store, in `ClassStore::new`, so
    /// it is in force before the first class is added.
    layout_domain: u32,
    /// Monotonic ClassId slots. An unloaded class leaves a tombstone so a
    /// stale ClassId can never alias a subsequently loaded class.
    classes: Vec<Option<Class>>,
    live_count: usize,
    /// Direct-subclass adjacency: `superclass id -> direct subclass ids`, in
    /// ascending id (= load) order.
    ///
    /// Maintained by [`ClassStore::add`], [`ClassStore::set_superclass`] and
    /// [`ClassStore::remove`] — the only three places a superclass edge is
    /// created, moved, or destroyed. See [`ClassStore::descendants_of`] for
    /// why this index exists.
    subclasses: rustc_hash::FxHashMap<u32, Vec<u32>>,
    /// Direct-implementor adjacency: `interface id -> direct implementor ids`,
    /// in ascending id order. The mirror of [`Self::subclasses`] for the
    /// `implements` / `extends-interface` relation, built from
    /// [`Class::interfaces`] rather than [`Class::superclass`].
    ///
    /// A key here is whatever some class NAMED in its `interfaces` list; the
    /// store does not verify that the named class carries `ACC_INTERFACE`, so
    /// this index describes the edges the loaded metadata actually declares
    /// rather than a well-formedness claim about them.
    ///
    /// Maintained by [`ClassStore::add`], [`ClassStore::set_interfaces`] and
    /// [`ClassStore::remove`] — the only three places an interface edge is
    /// created, moved, or destroyed. The middle maintainer is
    /// `set_interfaces`, NOT `set_superclass`: re-parenting a class moves a
    /// superclass edge and leaves `Class::interfaces` untouched, so it has
    /// nothing to mirror here. `set_interfaces` exists precisely because the
    /// production paths that populate `interfaces` *after* `add` — deferred
    /// interface resolution in the two synthetic-stub minting paths, the
    /// stub-to-real-bytecode upgrade, and the bootstrap compat-class wiring in
    /// `vm_init` — used to write the field through `get_mut`, which would
    /// leave this index blind to every one of them.
    ///
    /// See [`ClassStore::subtypes_of`] for why this index exists and why it is
    /// deliberately NOT folded into [`ClassStore::descendants_of`].
    implementors: rustc_hash::FxHashMap<u32, Vec<u32>>,
    /// Primary-supers display, parallel to [`Self::classes`] — see
    /// [`PrimaryDisplay`]. Maintained by the same four edge mutators as the two
    /// adjacency indices (`add`, `set_superclass`, `set_interfaces`, `remove`).
    displays: Vec<PrimaryDisplay>,
    /// The `implementors` keys (classes some class names in its `interfaces`)
    /// whose display is invalid or whose depth is 2 or more — i.e. the ones
    /// from which a superclass edge could lead to a class of depth >= 1. While
    /// this set is non-empty the display answers only POSITIVE subtype
    /// questions; see [`Self::primary_display_verdict`] for the argument.
    /// Empty on every well-formed hierarchy (an interface's superclass is
    /// `java/lang/Object`, so its depth is 1).
    deep_interface_keys: FxHashSet<u32>,
    /// Secondary supers: per class, the sorted set of every id its DAG walk
    /// can reach — HotSpot's `_secondary_supers`, widened to the whole
    /// closure so it answers interface targets and every question the primary
    /// display declines. Filled lazily under `&self` by
    /// [`Self::secondary_supers_verdict`] (a class nobody asks about costs
    /// nothing), and emptied under `&mut self` by the same four edge mutators,
    /// for every class that can reach the changed class
    /// ([`Self::forget_secondary_supers_reaching`]).
    ///
    /// Shared (`Arc`) because it is also the LOCK-FREE side: the VM holds a
    /// clone outside the `class_manager` lock and answers a subtype question
    /// from a published closure without taking it ([`PublishedSupers`]).
    secondaries: Arc<PublishedSupers>,
    /// `false` under `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY=1`: the display is
    /// still maintained but never consulted. Read once per store.
    display_enabled: bool,
    /// JVMS §5.4.5 overriding compares run-time packages by defining loader as
    /// well as by package name — see `method_override::same_runtime_package_for_override`.
    /// `false` (the default of an unconfigured store) is the historical
    /// name-only rule. Per store, hence per VM, never a process global:
    /// `ClassManager::set_compatibility_mode` turns it on in both modes
    /// (owner decision 2026-09-27), unless `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`.
    override_packages_by_loader: bool,
    /// This store's verifier type maps, keyed by the ids this store mints —
    /// one per store because a `ClassId` means nothing outside it (see
    /// [`crate::type_maps::TypeMapStore`]). Shared (`Arc`) for the same reason
    /// as [`Self::secondaries`]: the VM reads it without the `class_manager`
    /// lock (`ClassRealm::type_maps`).
    type_maps: Arc<crate::type_maps::TypeMapStore>,
}

/// Slots in a [`PrimaryDisplay`] — HotSpot's `primary_super_limit()`.
const PRIMARY_DISPLAY_LEN: usize = 8;

/// An unused display slot / the depth of an invalid display. `u32::MAX` is
/// never a `ClassId` a store hands out.
const DISPLAY_NONE: u32 = u32::MAX;

/// A class's superclass chain by depth — HotSpot's `_primary_supers`.
///
/// `supers[d]` is the ancestor at depth `d` (`java/lang/Object`, or whatever
/// root the chain ends at, is depth 0; the class itself is at `depth`), for
/// `d < PRIMARY_DISPLAY_LEN`. A class `T` of depth `d` is a superclass of `S`
/// exactly when `S.depth >= d && S.supers[d] == T` — one load and a compare,
/// where [`Class::is_subclass_of`] walks the chain and the whole interface DAG.
///
/// A display is VALID only when the class's whole superclass chain is present
/// in the store, acyclic, and at most [`MAX_HIERARCHY_DEPTH`] deep — the
/// conditions under which the walk itself reaches every ancestor. Anything
/// else is `depth == DISPLAY_NONE`, and the caller walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrimaryDisplay {
    depth: u32,
    supers: [u32; PRIMARY_DISPLAY_LEN],
}

impl PrimaryDisplay {
    const INVALID: Self = Self {
        depth: DISPLAY_NONE,
        supers: [DISPLAY_NONE; PRIMARY_DISPLAY_LEN],
    };

    #[inline]
    fn is_valid(&self) -> bool {
        self.depth != DISPLAY_NONE
    }
}

/// `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY=1` turns the display off (the DAG walk
/// answers every subtype question, the pre-2026-09-23 behaviour).
fn subtype_display_enabled() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_LOADER_NO_SUBTYPE_DISPLAY").is_none()
}

/// A superclass / interface edge changed (or a forward edge came into
/// existence). Moves `class_definition_epoch`, which every per-thread and JIT
/// memo of a subtype verdict is tagged with; see
/// `class_manager::bump_class_definition_epoch`. Called by the four edge
/// mutators at their END, after the closures are emptied, so a memo tagged
/// with the new epoch cannot have been computed from a closure published
/// before the change.
#[inline]
fn note_hierarchy_edge_change(layout_domain: u32) {
    crate::class_manager::bump_class_definition_epoch(layout_domain);
}

/// Class ids per slot chunk of [`PublishedSupers`].
const SUPERS_SLOT_CHUNK: usize = 1 << 12;
/// Slot chunks: ids below `SUPERS_SLOT_CHUNK * SUPERS_SLOT_CHUNKS` (4M) can
/// be published; a higher id is answered under the lock, by the walk.
const SUPERS_SLOT_CHUNKS: usize = 1 << 10;
/// Words per arena chunk (256 KiB). A closure never straddles two chunks and
/// holds at most [`MAX_HIERARCHY_DEPTH`] words.
const SUPERS_ARENA_CHUNK: usize = 1 << 16;
/// Arena chunks per store: 16 MiB of closure words. Past that nothing new is
/// published (the lock-free side misses and the locked side walks) — the
/// bound on what invalidated closures can leave behind, see
/// [`PublishedSupers`].
const SUPERS_ARENA_CHUNKS: usize = 64;
/// Slot value: nothing published.
const SUPERS_EMPTY: u64 = 0;
/// Slot value: the closure is larger than [`MAX_HIERARCHY_DEPTH`] ids, so the
/// walk answers (its depth bound could bite).
const SUPERS_TOO_LARGE: u64 = u64::MAX;

/// The secondary-supers closures of one [`ClassStore`], readable WITHOUT the
/// `class_manager` lock — stage 3 of
/// `docs/internal/fixed-bugs/interpreter-L2-proposal-subtype-check-display-FIXED-20261003.md`.
///
/// # Shape
///
/// A slot per class id (`AtomicU64`, chunked directory, chunks allocated on
/// first fill) holding `(arena offset << 16) | length` of the class's sorted
/// closure, [`SUPERS_EMPTY`] or [`SUPERS_TOO_LARGE`]. The closures live in an
/// append-only arena of `AtomicU32` words. A reader loads the slot (`Acquire`)
/// and binary-searches the words; the filler wrote them before its `Release`
/// compare-exchange on the slot, so a reader that sees the slot sees them.
///
/// # Why a reader needs no lock
///
/// * Fills happen only in [`ClassStore::secondary_supers_verdict`], under
///   `&ClassStore` (the read lock); emptying only under `&mut ClassStore` (the
///   write lock). So a published closure always describes the hierarchy as of
///   some moment no writer was inside a mutation, and a writer empties every
///   closure its change can alter before it releases the lock.
/// * Arena words are NEVER reused or freed while the store lives: an emptied
///   slot's words become garbage, so a reader that loaded the old slot value an
///   instant before a writer emptied it still reads valid, immutable words.
///   That is the whole reclamation story, and the reason for
///   [`SUPERS_ARENA_CHUNKS`]'s cap: churn (synthetic-stub upgrades, unloading)
///   leaves garbage behind, and once the arena is full publication stops rather
///   than growing without bound.
/// * A reader racing a writer can observe the pre-change closure — the same
///   answer it would have got had it taken the read lock an instant earlier.
///   A memo computed from such an answer is still discarded: the mutators move
///   `class_definition_epoch` after emptying ([`note_hierarchy_edge_change`]).
pub struct PublishedSupers {
    slots: Box<[std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>]>,
    arena: Box<[std::sync::OnceLock<Box<[std::sync::atomic::AtomicU32]>>]>,
    /// Next free arena word. Taken only by a fill (once per class per edge
    /// change), never by a reader.
    cursor: parking_lot::Mutex<usize>,
    /// Non-empty slots. Exact whenever the caller holds `&mut ClassStore`.
    filled: std::sync::atomic::AtomicUsize,
    /// Set once the arena is full; nothing new is published after that.
    exhausted: std::sync::atomic::AtomicBool,
}

impl fmt::Debug for PublishedSupers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublishedSupers({} published)", self.published_count())
    }
}

impl PublishedSupers {
    fn new() -> Self {
        Self {
            slots: (0..SUPERS_SLOT_CHUNKS)
                .map(|_| std::sync::OnceLock::new())
                .collect(),
            arena: (0..SUPERS_ARENA_CHUNKS)
                .map(|_| std::sync::OnceLock::new())
                .collect(),
            cursor: parking_lot::Mutex::new(0),
            filled: std::sync::atomic::AtomicUsize::new(0),
            exhausted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Can a closure for `id` still be published? `false` past the slot
    /// directory and once the arena is full — the caller then walks instead of
    /// computing a closure it could not keep.
    #[inline]
    fn can_publish(&self, id: u32) -> bool {
        (id as usize) < SUPERS_SLOT_CHUNK * SUPERS_SLOT_CHUNKS
            && !self.exhausted.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    fn slot(&self, id: u32) -> Option<&std::sync::atomic::AtomicU64> {
        let i = id as usize;
        self.slots
            .get(i / SUPERS_SLOT_CHUNK)?
            .get()?
            .get(i % SUPERS_SLOT_CHUNK)
    }

    /// The slot value for `id` ([`SUPERS_EMPTY`] when nothing is published).
    #[inline]
    fn slot_value(&self, id: u32) -> u64 {
        self.slot(id).map_or(SUPERS_EMPTY, |s| {
            s.load(std::sync::atomic::Ordering::Acquire)
        })
    }

    /// Lock-free: `Some(answer)` to "is `sub` `sup` or a subtype of it?" when
    /// `sub`'s closure is published — exactly [`Class::is_subclass_of`]'s
    /// answer (see [`ClassStore::secondary_supers_verdict`] for why the
    /// closure equals the walk). `None` when nothing is published for `sub`
    /// (never asked, emptied by an edge change, too large, the display is
    /// off, or `sub` is not a stored class): ask under the lock, which fills.
    #[inline]
    pub fn verdict(&self, sub: ClassId, sup: ClassId) -> Option<bool> {
        self.answer(self.slot_value(sub.as_u32()), sup.as_u32())
    }

    /// Answer membership of `sup` in the closure a slot value names.
    #[inline]
    fn answer(&self, enc: u64, sup: u32) -> Option<bool> {
        if enc == SUPERS_EMPTY || enc == SUPERS_TOO_LARGE {
            return None;
        }
        let len = (enc & 0xFFFF) as usize;
        let off = (enc >> 16) as usize;
        let chunk = self.arena.get(off / SUPERS_ARENA_CHUNK)?.get()?;
        let start = off % SUPERS_ARENA_CHUNK;
        let words = chunk.get(start..start + len)?;
        let (mut lo, mut hi) = (0usize, words.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match words[mid]
                .load(std::sync::atomic::Ordering::Relaxed)
                .cmp(&sup)
            {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(true),
            }
        }
        Some(false)
    }

    /// Publish `closure` (sorted; EMPTY means too large) for `id` and return
    /// the slot value now in force — this one, or a racing filler's, which
    /// describes the same hierarchy (both ran under the read lock). `None`
    /// when `id` is past the slot directory or the arena is full.
    fn publish(&self, id: u32, closure: &[u32]) -> Option<u64> {
        let i = id as usize;
        let slot = &self.slots.get(i / SUPERS_SLOT_CHUNK)?.get_or_init(|| {
            (0..SUPERS_SLOT_CHUNK)
                .map(|_| std::sync::atomic::AtomicU64::new(SUPERS_EMPTY))
                .collect()
        })[i % SUPERS_SLOT_CHUNK];
        let enc = if closure.is_empty() {
            SUPERS_TOO_LARGE
        } else {
            self.store_words(closure)?
        };
        match slot.compare_exchange(
            SUPERS_EMPTY,
            enc,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                self.filled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(enc)
            }
            Err(current) => Some(current),
        }
    }

    /// Append `words` to the arena; the slot value that names them.
    fn store_words(&self, words: &[u32]) -> Option<u64> {
        debug_assert!(!words.is_empty() && words.len() <= MAX_HIERARCHY_DEPTH);
        let mut cursor = self.cursor.lock();
        let mut off = *cursor;
        if off % SUPERS_ARENA_CHUNK + words.len() > SUPERS_ARENA_CHUNK {
            off = (off / SUPERS_ARENA_CHUNK + 1) * SUPERS_ARENA_CHUNK;
        }
        let Some(slot) = self.arena.get(off / SUPERS_ARENA_CHUNK) else {
            self.exhausted
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return None;
        };
        let chunk = slot.get_or_init(|| {
            (0..SUPERS_ARENA_CHUNK)
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect()
        });
        let start = off % SUPERS_ARENA_CHUNK;
        for (cell, &w) in chunk[start..start + words.len()].iter().zip(words) {
            cell.store(w, std::sync::atomic::Ordering::Relaxed);
        }
        *cursor = off + words.len();
        Some(((off as u64) << 16) | words.len() as u64)
    }

    /// Empty `id`'s slot. Only under `&mut ClassStore`.
    fn forget(&self, id: u32) {
        if let Some(slot) = self.slot(id) {
            if slot.swap(SUPERS_EMPTY, std::sync::atomic::Ordering::AcqRel) != SUPERS_EMPTY {
                // Saturating: `fetch_update` never fails with `Some`.
                let _ = self.filled.fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| Some(n.saturating_sub(1)),
                );
            }
        }
    }

    /// Non-empty slots (diagnostics, and the mutators' early-out).
    pub fn published_count(&self) -> usize {
        self.filled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Insert `child` into `map[parent]`, keeping the list sorted and duplicate
/// free.
///
/// Both adjacency lists are kept in ascending id order and both are walked by
/// a breadth-first search, so a repeated entry costs a wasted visit. A class
/// file may legally name the same interface twice (see `proxy_gen`'s
/// duplicate-interface handling) and the deferred-resolution paths may re-run
/// over a list they already applied, so the de-duplication has to happen on
/// insert rather than being assumed of the input.
fn link_subtype_edge(map: &mut rustc_hash::FxHashMap<u32, Vec<u32>>, parent: u32, child: u32) {
    let kids = map.entry(parent).or_default();
    if let Err(pos) = kids.binary_search(&child) {
        kids.insert(pos, child);
    }
}

/// Remove `child` from `map[parent]`, dropping the entry once it empties.
///
/// Dropping the emptied vector matters on the unload path: a loader that
/// defines and unloads thousands of classes would otherwise leave one empty
/// `Vec` behind per interface it ever named.
fn unlink_subtype_edge(map: &mut rustc_hash::FxHashMap<u32, Vec<u32>>, parent: u32, child: u32) {
    if let Some(kids) = map.get_mut(&parent) {
        kids.retain(|&k| k != child);
        if kids.is_empty() {
            map.remove(&parent);
        }
    }
}

impl ClassStore {
    /// Create an empty class store.
    pub fn new() -> Self {
        Self {
            layout_domain: cratonvm_types::next_layout_domain(),
            classes: Vec::new(),
            live_count: 0,
            subclasses: rustc_hash::FxHashMap::default(),
            implementors: rustc_hash::FxHashMap::default(),
            displays: Vec::new(),
            deep_interface_keys: FxHashSet::default(),
            secondaries: Arc::new(PublishedSupers::new()),
            display_enabled: subtype_display_enabled(),
            override_packages_by_loader: false,
            type_maps: Arc::new(crate::type_maps::TypeMapStore::new()),
        }
    }

    /// Whether §5.4.5 overriding in this store compares defining loaders (see
    /// the field doc). Read by `method_override::can_override`.
    #[inline]
    pub fn override_packages_by_loader(&self) -> bool {
        self.override_packages_by_loader
    }

    /// Install the §5.4.5 run-time package policy for this store. Meant for VM
    /// init, before any class is defined (vtables built earlier keep the rule
    /// they were built with). A change moves this store's
    /// `class_definition_epoch`, which retires every memoized selection
    /// (`vm::runtime::resolve::selection`'s `SELECT_MEMO`) and subtype verdict
    /// computed under the other rule; setting the value it already has does
    /// nothing, so a `--compatible` VM never bumps anything.
    pub fn set_override_packages_by_loader(&mut self, on: bool) {
        if self.override_packages_by_loader != on {
            self.override_packages_by_loader = on;
            note_hierarchy_edge_change(self.layout_domain);
        }
    }

    /// Allocate the next `ClassId` (does not yet store a class).
    ///
    /// Useful when you need the id before constructing the `Class` (e.g.
    /// to fill in `class.id`).
    /// This store's compact-layout domain. The VM's heap must be told the same
    /// value or it will refuse every compact allocation — see
    /// `Heap::set_layout_domain` and `compact_object_body_size`.
    pub fn layout_domain(&self) -> u32 {
        self.layout_domain
    }

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
        let superclass = class.superclass;
        let interfaces = class.interfaces.clone();
        self.classes.push(Some(class));
        self.live_count += 1;
        // A new class can be the ANCESTOR a descendant's descriptor lookup was
        // previously unable to resolve, so it retires provenance-derived memos
        // for the same reason an in-place origin change does — see
        // [`class_origin_epoch`]. Not every `add` needs this (most classes are
        // nobody's missing ancestor), but the alternative is reasoning about
        // which ones do, and a class definition is not a hot path.
        CLASS_ORIGIN_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Ids are monotonic, so appending keeps each child list sorted.
        if let Some(sid) = superclass {
            self.subclasses
                .entry(sid.as_u32())
                .or_default()
                .push(id.as_u32());
        }
        // The interface mirror. Sorted-insert rather than `push` because a
        // class file may name the same interface twice, which would otherwise
        // put the same id in the list twice. Several production paths hand
        // `add` an EMPTY interface list and fill it in afterwards to avoid a
        // load cycle; those go through `set_interfaces`, which is what keeps
        // this index in step with them.
        for iface in &interfaces {
            link_subtype_edge(&mut self.implementors, iface.as_u32(), id.as_u32());
        }
        // The primary display. `id == next_id()`, so the slot is the next one.
        let display = self.compute_display(id);
        self.displays.push(display);
        // A class re-parented onto this id before it existed (`set_superclass`
        // to a not-yet-added id) got an invalid display; it can be valid now.
        if self.subclasses.contains_key(&id.as_u32()) {
            self.recompute_displays_below(id);
        }
        // Likewise a closure that reached this id through such a forward edge
        // stopped here (a missing class has no edges of its own); it grows now,
        // and so can a memoised verdict about any class that reached it.
        let key = id.as_u32();
        if self.subclasses.contains_key(&key) || self.implementors.contains_key(&key) {
            self.forget_secondary_supers_reaching(key);
            note_hierarchy_edge_change(self.layout_domain);
        }
        self.refresh_deep_interface_key(id.as_u32());
        for iface in &interfaces {
            self.refresh_deep_interface_key(iface.as_u32());
        }
        // Compact reference-field layout: register this class's oop-map / offset
        // table so the heap + GC can place and scan its reference fields as
        // 8-byte pointers. No-op when `CRATONVM_COMPACT_REF_FIELDS=0` opts out.
        self.register_compact_layout_if_enabled(id);
        id
    }

    /// Re-parent an already-stored class, keeping the direct-subclass index in
    /// step.
    ///
    /// The one caller that needs this is the synthetic-stub → real-bytecode
    /// upgrade: a stub is minted with whatever superclass its name implies
    /// (often `None`/`java/lang/Object`), and the real class file may name a
    /// different one. Writing `class.superclass` through `get_mut` instead
    /// would silently desynchronise [`ClassStore::descendants_of`].
    pub fn set_superclass(&mut self, id: ClassId, new_super: Option<ClassId>) {
        let Some(class) = self
            .classes
            .get_mut(id.as_u32() as usize)
            .and_then(Option::as_mut)
        else {
            return;
        };
        let old_super = class.superclass;
        if old_super == new_super {
            return;
        }
        class.superclass = new_super;
        // Every closure that can reach `id` passes over the moved edge.
        self.forget_secondary_supers_reaching(id.as_u32());
        if let Some(old) = old_super {
            if let Some(kids) = self.subclasses.get_mut(&old.as_u32()) {
                kids.retain(|&k| k != id.as_u32());
            }
        }
        if let Some(new) = new_super {
            let kids = self.subclasses.entry(new.as_u32()).or_default();
            // Keep ascending order: the list is a topological order of a
            // single hierarchy level, which `descendants_of` relies on.
            match kids.binary_search(&id.as_u32()) {
                Ok(_) => {}
                Err(pos) => kids.insert(pos, id.as_u32()),
            }
        }
        // Every display below `id` embeds `id`'s chain: recompute this class
        // and all its descendants, parents first.
        self.recompute_display_at(id);
        self.recompute_displays_below(id);
        note_hierarchy_edge_change(self.layout_domain);
    }

    /// Replace an already-stored class's `interfaces` list, keeping the
    /// direct-implementor index in step.
    ///
    /// This is to [`Self::implementors`] what [`Self::set_superclass`] is to
    /// [`Self::subclasses`], and it exists for the same reason: several
    /// production paths cannot fill `interfaces` in at `add` time and have to
    /// come back for it.
    ///
    ///  * both synthetic-stub minting paths register the stub FIRST and resolve
    ///    its interface names afterwards, because loading an interface can
    ///    recursively demand the very class being minted;
    ///  * the synthetic-stub -> real-bytecode upgrade replaces the whole
    ///    metadata record in place, and the real class file routinely names
    ///    interfaces the stub did not;
    ///  * `vm_init` wires `java/util/Iterator` and friends onto the bootstrap
    ///    compatibility classes once both ends are loaded.
    ///
    /// Writing `class.interfaces` through [`Self::get_mut`] instead would
    /// silently desynchronise [`Self::subtypes_of`] — and because the only
    /// consumer of that walk treats a missing edge as "nothing to
    /// devirtualize", the desynchronisation would cost throughput without ever
    /// failing a test.
    pub fn set_interfaces(&mut self, id: ClassId, new_interfaces: Vec<ClassId>) {
        let Some(class) = self
            .classes
            .get_mut(id.as_u32() as usize)
            .and_then(Option::as_mut)
        else {
            return;
        };
        if class.interfaces == new_interfaces {
            return;
        }
        let old = std::mem::replace(&mut class.interfaces, new_interfaces);
        // Cloned so the borrow of `self.classes` ends before `self.implementors`
        // is touched; both lists are a handful of entries.
        let new = class.interfaces.clone();
        // Every closure that can reach `id` passes over the changed edges.
        self.forget_secondary_supers_reaching(id.as_u32());
        for iface in &old {
            if !new.contains(iface) {
                unlink_subtype_edge(&mut self.implementors, iface.as_u32(), id.as_u32());
            }
        }
        for iface in &new {
            link_subtype_edge(&mut self.implementors, iface.as_u32(), id.as_u32());
        }
        for iface in old.iter().chain(new.iter()) {
            self.refresh_deep_interface_key(iface.as_u32());
        }
        note_hierarchy_edge_change(self.layout_domain);
    }

    // ----- Primary-supers display ------------------------------------------

    /// The display of the class in slot `id`, derived from its superclass's
    /// (already current) display. See [`PrimaryDisplay`] for when it is valid.
    fn compute_display(&self, id: ClassId) -> PrimaryDisplay {
        let Some(class) = self.get(id) else {
            return PrimaryDisplay::INVALID;
        };
        let me = id.as_u32();
        let Some(super_id) = class.superclass else {
            // A root (`java/lang/Object`, or a fabricated class with no
            // superclass): depth 0.
            let mut d = PrimaryDisplay::INVALID;
            d.depth = 0;
            d.supers[0] = me;
            return d;
        };
        if super_id == id || self.get(super_id).is_none() {
            return PrimaryDisplay::INVALID;
        }
        let parent = match self.displays.get(super_id.as_u32() as usize) {
            Some(p) if p.is_valid() => *p,
            _ => return PrimaryDisplay::INVALID,
        };
        let depth = parent.depth + 1;
        // Cast: MAX_HIERARCHY_DEPTH is a small constant.
        if depth as usize > MAX_HIERARCHY_DEPTH {
            return PrimaryDisplay::INVALID;
        }
        // A cycle through the recorded prefix (the full chain is checked by
        // `superclass_chain_reaches` where a cycle can be created).
        let known = (parent.depth as usize + 1).min(PRIMARY_DISPLAY_LEN);
        if parent.supers[..known].contains(&me) {
            return PrimaryDisplay::INVALID;
        }
        let mut d = parent;
        d.depth = depth;
        if (depth as usize) < PRIMARY_DISPLAY_LEN {
            d.supers[depth as usize] = me;
        }
        d
    }

    /// `true` when following superclass edges up from `from` (inclusive)
    /// reaches `target` — the cycle test for re-parenting.
    fn superclass_chain_reaches(&self, from: ClassId, target: ClassId) -> bool {
        let mut cur = Some(from);
        let mut steps = 0usize;
        while let Some(c) = cur {
            if c == target {
                return true;
            }
            steps += 1;
            if steps > MAX_HIERARCHY_DEPTH + 1 {
                // Too deep to be a well-formed chain; treat as a cycle so the
                // display is invalidated rather than trusted.
                return true;
            }
            cur = self.get(c).and_then(|k| k.superclass);
        }
        false
    }

    /// Recompute `id`'s display in place and refresh its interface-key status.
    fn recompute_display_at(&mut self, id: ClassId) {
        let cycle = self
            .get(id)
            .and_then(|c| c.superclass)
            .is_some_and(|s| self.superclass_chain_reaches(s, id));
        let display = if cycle {
            PrimaryDisplay::INVALID
        } else {
            self.compute_display(id)
        };
        if let Some(slot) = self.displays.get_mut(id.as_u32() as usize) {
            *slot = display;
        }
        self.refresh_deep_interface_key(id.as_u32());
    }

    /// Recompute the display of every transitive subclass of `id`, parents
    /// before children ([`Self::descendants_of`]'s order).
    fn recompute_displays_below(&mut self, id: ClassId) {
        for d in self.descendants_of(id) {
            self.recompute_display_at(d);
        }
    }

    /// Keep [`Self::deep_interface_keys`] exact for `key`.
    fn refresh_deep_interface_key(&mut self, key: u32) {
        let named_as_interface = self.implementors.contains_key(&key);
        let deep = named_as_interface
            && self
                .displays
                .get(key as usize)
                .is_none_or(|d| !d.is_valid() || d.depth >= 2);
        if deep {
            self.deep_interface_keys.insert(key);
        } else {
            self.deep_interface_keys.remove(&key);
        }
    }

    /// `Some(answer)` when the primary display decides "`sub` is `sup` or a
    /// subtype of it" EXACTLY as [`Class::is_subclass_of`]'s DAG walk would;
    /// `None` when the caller must walk.
    ///
    /// Answered only for a `sup` whose display is valid with depth in
    /// `1..PRIMARY_DISPLAY_LEN` (a root, a deep class or an unknown one walks).
    ///
    /// * POSITIVE — `sub.supers[sup.depth] == sup`: `sup` is on `sub`'s
    ///   superclass chain, which the walk explores first (it recurses into the
    ///   superclass before any interface), within its depth bound.
    /// * NEGATIVE — only while no class names `sup` in its `interfaces` and
    ///   [`Self::deep_interface_keys`] is empty. Proof: the walk reaches `sup`
    ///   over a last edge `X -> sup` that is either an interface edge (then `sup`
    ///   is an `implementors` key — excluded) or a superclass edge. If every
    ///   edge on the path is a superclass edge, `sup` is on `sub`'s chain and
    ///   the positive test would have fired. Otherwise take the LAST interface
    ///   edge `Y -> J` on the path: `J` is an `implementors` key and `sup` is a
    ///   strict ancestor of `J` of depth >= 1, so `J`'s depth is >= 2 (or its
    ///   display is invalid) and `J` is in the deep set — excluded.
    ///
    /// Depends on `Class::superclass` / `Class::interfaces` changing only
    /// through the four edge mutators; both are gated by source scans
    /// (`superclass_is_only_ever_written_through_set_superclass`,
    /// `no_raw_write_to_a_stored_class_interfaces_bypasses_set_interfaces`).
    /// Access flags are deliberately NOT consulted: they change in place
    /// through `get_mut`, and the walk does not read them either.
    #[inline]
    fn primary_display_verdict(&self, sub: ClassId, sup: ClassId) -> Option<bool> {
        if !self.display_enabled {
            return None;
        }
        let t = self.displays.get(sup.as_u32() as usize)?;
        let t_depth = t.depth as usize;
        if !t.is_valid() || t_depth == 0 || t_depth >= PRIMARY_DISPLAY_LEN {
            return None;
        }
        let s = self.displays.get(sub.as_u32() as usize)?;
        if !s.is_valid() {
            return None;
        }
        if s.depth as usize >= t_depth && s.supers[t_depth] == sup.as_u32() {
            return Some(true);
        }
        if !self.deep_interface_keys.is_empty() || self.implementors.contains_key(&sup.as_u32()) {
            return None;
        }
        Some(false)
    }

    // ----- Secondary supers ------------------------------------------------

    /// `Some(answer)` when `sub`'s secondary-supers closure decides "`sub` is
    /// `sup` or a subtype of it" exactly as [`Class::is_subclass_of`]'s DAG
    /// walk would; `None` when the caller must walk. Asked after
    /// [`Self::primary_display_verdict`] declines — interface targets, roots,
    /// deep targets, and class targets while a deep interface key exists.
    ///
    /// The closure is every id reachable from `sub` over superclass and
    /// interface edges, expanding only classes present in the store — which is
    /// the walk's own reachability: it recurses only into `store.get(..)` hits
    /// but matches an edge's target id before looking it up. Its depth bound
    /// cannot bite either: the walk's recursion stack holds distinct present
    /// reachable classes, so a closure of at most [`MAX_HIERARCHY_DEPTH`] ids
    /// never reaches depth `MAX_HIERARCHY_DEPTH + 1`; a larger closure is
    /// stored empty and walks.
    ///
    /// Filled on first use, a sorted slice, answered by binary search.
    /// Correct only while every edge change goes through the four mutators,
    /// which empty the closures of every class that can reach the changed one
    /// ([`Self::forget_secondary_supers_reaching`]) — see the two source-scan
    /// gates named on [`Self::primary_display_verdict`].
    ///
    /// Fills happen ONLY here, i.e. under `&self` (the caller's
    /// `class_manager` read lock), and emptying only under `&mut self` (the
    /// write lock), so a closure is never published from a hierarchy a writer
    /// is halfway through changing. That is what lets
    /// [`PublishedSupers::verdict`] answer without the lock.
    #[inline]
    fn secondary_supers_verdict(&self, sub: ClassId, sup: ClassId) -> Option<bool> {
        if !self.display_enabled {
            return None;
        }
        let published = self.secondaries.slot_value(sub.as_u32());
        let enc = if published != SUPERS_EMPTY {
            published
        } else {
            // Nothing to publish into: walk rather than recompute the closure
            // on every question.
            if !self.secondaries.can_publish(sub.as_u32()) || self.get(sub).is_none() {
                return None;
            }
            let closure = self.compute_secondary_supers(sub);
            self.secondaries.publish(sub.as_u32(), &closure)?
        };
        self.secondaries.answer(enc, sup.as_u32())
    }

    /// Publish `id`'s closure when a question about it was answered WITHOUT
    /// it (by the primary display), so the next question about `id` from the
    /// lock-free side ([`PublishedSupers::verdict`]) finds it. One slot load
    /// once published; the closure is computed once per class per edge change.
    #[inline]
    fn ensure_secondary_supers_published(&self, id: ClassId) {
        if self.display_enabled
            && self.secondaries.slot_value(id.as_u32()) == SUPERS_EMPTY
            && self.secondaries.can_publish(id.as_u32())
            && self.get(id).is_some()
        {
            let closure = self.compute_secondary_supers(id);
            let _ = self.secondaries.publish(id.as_u32(), &closure);
        }
    }

    /// The lock-free view of this store's secondary-supers closures, for a
    /// caller that keeps it outside the `class_manager` lock (the VM's
    /// `ClassRealm::published_supers`). One per store, for the store's life.
    pub fn published_supers(&self) -> Arc<PublishedSupers> {
        Arc::clone(&self.secondaries)
    }

    /// This store's verifier type maps (see [`Self::type_map_store`]).
    pub fn type_maps(&self) -> &crate::type_maps::TypeMapStore {
        &self.type_maps
    }

    /// The type-map store as an `Arc`, for a caller that keeps it outside the
    /// `class_manager` lock (the VM's `ClassRealm::type_maps`). One per store,
    /// for the store's life.
    pub fn type_map_store(&self) -> Arc<crate::type_maps::TypeMapStore> {
        Arc::clone(&self.type_maps)
    }

    /// The closure [`Self::secondary_supers_verdict`] stores for `id`: `id`
    /// and every id reachable from it, sorted — or empty when that is more
    /// than [`MAX_HIERARCHY_DEPTH`] ids.
    fn compute_secondary_supers(&self, id: ClassId) -> Box<[u32]> {
        let root = id.as_u32();
        let mut seen: FxHashSet<u32> = FxHashSet::default();
        seen.insert(root);
        let mut out: Vec<u32> = vec![root];
        let mut stack: Vec<u32> = vec![root];
        while let Some(n) = stack.pop() {
            // A missing class (forward reference, tombstone) is matched but
            // not expanded — exactly what the walk does.
            let Some(class) = self.get(ClassId::new(n)) else {
                continue;
            };
            for next in class.superclass.iter().chain(class.interfaces.iter()) {
                let v = next.as_u32();
                if seen.insert(v) {
                    if out.len() >= MAX_HIERARCHY_DEPTH {
                        return Box::default();
                    }
                    out.push(v);
                    stack.push(v);
                }
            }
        }
        out.sort_unstable();
        out.into_boxed_slice()
    }

    /// Empty the secondary-supers closure of `id` and of every class that can
    /// reach `id` — the only closures an edge change at `id` can alter (a
    /// path to anything past `id` reaches `id` first). Walks the subclass and
    /// implementor indices backwards; costs nothing while no closure is filled.
    fn forget_secondary_supers_reaching(&mut self, id: u32) {
        // Exact under `&mut self`: nothing fills without `&self`.
        if self.secondaries.published_count() == 0 {
            return;
        }
        let mut seen: FxHashSet<u32> = FxHashSet::default();
        let mut stack: Vec<u32> = vec![id];
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            self.secondaries.forget(n);
            if let Some(kids) = self.subclasses.get(&n) {
                stack.extend_from_slice(kids);
            }
            if let Some(kids) = self.implementors.get(&n) {
                stack.extend_from_slice(kids);
            }
        }
    }

    /// Number of classes whose display is valid, and whether negative answers
    /// are currently served (for tests and diagnostics).
    pub fn subtype_display_coverage(&self) -> (usize, bool) {
        let valid = self
            .classes
            .iter()
            .zip(&self.displays)
            .filter(|(c, d)| c.is_some() && d.is_valid())
            .count();
        (valid, self.deep_interface_keys.is_empty())
    }

    /// Every transitive subclass of `id`, parents before children.
    ///
    /// PERF (general-bugs TODO, "layout registry lookup hotspot"): the caller
    /// this exists for — `ClassManager::recompute_subclass_layouts` — used to
    /// answer the same question by scanning **every** class in the store and
    /// walking each one's superclass chain with `is_subclass_of`, i.e.
    /// `O(classes x depth)` per call. It is called once per synthetic-stub
    /// upgrade whose layout shifted, and a Spring-Boot-scale run performs
    /// thousands of those against tens of thousands of classes, so the scan is
    /// quadratic in the class count. Walking the adjacency index instead costs
    /// `O(descendants)`.
    ///
    /// The returned order is a valid topological order for the superclass
    /// relation (breadth-first from `id`, children in ascending id within a
    /// level), so a consumer may recompute each entry using its parent's
    /// already-updated values — which is exactly what the layout recompute
    /// requires and what the old ascending-id scan provided.
    ///
    /// Depth is bounded by [`MAX_HIERARCHY_DEPTH`] levels, so a corrupt index
    /// containing a cycle terminates instead of hanging. A class is never its
    /// own descendant.
    ///
    /// # Superclass edges ONLY, deliberately
    ///
    /// This walks [`Self::subclasses`] and never [`Self::implementors`], and
    /// widening it was considered and rejected. Its caller
    /// (`ClassManager::recompute_subclass_layouts`) consumes the returned
    /// order as a topological order of the SUPERCLASS relation — each entry
    /// recomputes `first_field_index` / `num_total_fields` by reading its
    /// parent's already-updated values. Adding interface edges would break
    /// that property outright: a class can be reached as an implementor of an
    /// interface at level 1 while its own superclass is only reached at level
    /// 3, so it would be recomputed against a stale parent. It would also pull
    /// in classes that are not in the changed class's field-layout lineage at
    /// all, since an interface contributes no instance fields.
    ///
    /// Class-hierarchy analysis wants the other question and asks
    /// [`Self::subtypes_of`], which walks both indices and makes no ordering
    /// promise.
    pub fn descendants_of(&self, id: ClassId) -> Vec<ClassId> {
        let mut out: Vec<ClassId> = Vec::new();
        let mut frontier: Vec<u32> = match self.subclasses.get(&id.as_u32()) {
            Some(kids) => kids.clone(),
            None => return out,
        };
        let mut seen: FxHashSet<u32> = FxHashSet::default();
        seen.insert(id.as_u32());
        let mut level = 0usize;
        while !frontier.is_empty() && level < MAX_HIERARCHY_DEPTH {
            let mut next: Vec<u32> = Vec::new();
            for cid in frontier {
                if !seen.insert(cid) {
                    continue;
                }
                out.push(ClassId::new(cid));
                if let Some(kids) = self.subclasses.get(&cid) {
                    next.extend_from_slice(kids);
                }
            }
            frontier = next;
            level += 1;
        }
        out
    }

    /// Every transitive SUBTYPE of `id` — subclasses, implementors and
    /// sub-interfaces alike — or `None` when there are more than `limit` of
    /// them.
    ///
    /// The union of both adjacency indices, walked breadth-first with a `seen`
    /// set. `id` itself is never included. This is the question
    /// class-hierarchy analysis asks (`ClassRealm::unique_implementor`), and it
    /// is a different question from [`Self::descendants_of`] — see that
    /// method's doc for why the two are separate rather than one widened walk.
    ///
    /// # `None` means "cannot answer", never "none exist"
    ///
    /// The caller is a bounded analysis, and every way this walk can come back
    /// incomplete has to be distinguishable from an empty answer — an
    /// incomplete list read as complete is how "one implementor" gets reported
    /// for a hierarchy that has two. So both bail-outs return `None`:
    ///
    ///  * more than `limit` distinct subtypes, which is the cap the caller
    ///    imposes so that a JDK root or a widely implemented interface costs a
    ///    constant instead of O(loaded classes) — reported either by the
    ///    direct-children pre-check or by the walk itself;
    ///  * a frontier still non-empty after [`MAX_HIERARCHY_DEPTH`] levels,
    ///    which a well-formed hierarchy cannot reach and a corrupt index can.
    ///
    /// `Some(v)` therefore carries the guarantee that `v` is the COMPLETE set
    /// of loaded subtypes, which is what lets the caller conclude "exactly
    /// one".
    ///
    /// # Both edge kinds compose
    ///
    /// An implementor's own subclasses are implementors too, and an interface
    /// extending an interface is an edge like any other, so the two relations
    /// have to be walked together rather than one after the other: reaching
    /// `class C implements J` from `interface I` requires following the
    /// interface edge `I -> J`, then `J -> C`, then class edges below `C`. The
    /// `seen` set is what keeps a diamond (`C implements I` and
    /// `C implements J extends I`) from reporting `C` twice.
    ///
    /// # No ordering promise
    ///
    /// The result is breadth-first over the union, which is NOT a topological
    /// order of the superclass relation — a class can be reached through an
    /// interface edge before its own superclass is reached. A caller that
    /// needs parents-before-children must use [`Self::descendants_of`].
    pub fn subtypes_of(&self, id: ClassId, limit: usize) -> Option<Vec<ClassId>> {
        let root = id.as_u32();
        let direct_subclasses = self.subclasses.get(&root).map_or(0, Vec::len);
        let direct_implementors = self.implementors.get(&root).map_or(0, Vec::len);
        // Cheap early-out before anything is copied or allocated. A widely
        // implemented JDK interface has thousands of direct implementors, and
        // the loop below would otherwise memcpy that whole list into the
        // frontier only to give up on the 65th visit.
        //
        // The two counts are summed without de-duplicating them, which can
        // only over-count — and only for metadata where one class is both a
        // direct subclass and a direct implementor of the same id, which no
        // well-formed class file produces (a class cannot `extends` an
        // interface). An over-count can turn a `Some` into a `None`, never the
        // reverse, and `None` is the answer that costs nothing.
        if direct_subclasses + direct_implementors > limit {
            return None;
        }
        let mut out: Vec<ClassId> = Vec::new();
        let mut seen: FxHashSet<u32> = FxHashSet::default();
        seen.insert(root);
        let mut frontier: Vec<u32> = Vec::with_capacity(direct_subclasses + direct_implementors);
        if let Some(kids) = self.subclasses.get(&root) {
            frontier.extend_from_slice(kids);
        }
        if let Some(impls) = self.implementors.get(&root) {
            frontier.extend_from_slice(impls);
        }
        let mut level = 0usize;
        while !frontier.is_empty() {
            if level >= MAX_HIERARCHY_DEPTH {
                return None;
            }
            let mut next: Vec<u32> = Vec::new();
            for cid in frontier {
                if !seen.insert(cid) {
                    continue;
                }
                if out.len() >= limit {
                    return None;
                }
                out.push(ClassId::new(cid));
                if let Some(kids) = self.subclasses.get(&cid) {
                    next.extend_from_slice(kids);
                }
                if let Some(impls) = self.implementors.get(&cid) {
                    next.extend_from_slice(impls);
                }
            }
            frontier = next;
            level += 1;
        }
        Some(out)
    }

    /// Rebuild and re-register the compact field layout of **every** loaded
    /// class.
    ///
    /// Needed exactly once, when compressed oops are switched on at VM init:
    /// the bootstrap class set is already loaded and laid out by then, with
    /// 8-byte reference fields, while the field accessors are about to start
    /// reading 4-byte narrow slots. Since the heap has just been created, no
    /// instance of any of those classes exists yet, so re-laying them out is
    /// free of the "read back under a different layout than it was written"
    /// hazard that makes layout changes unsafe at any later point.
    pub fn recompute_all_compact_layouts(&self) -> usize {
        let mut n = 0;
        for slot in &self.classes {
            if let Some(class) = slot {
                self.register_compact_layout_if_enabled(class.id);
                n += 1;
            }
        }
        n
    }

    /// Build and register the compact field layout for class `id`, if the
    /// compact reference-field layout is enabled. Idempotent (overwrites on
    /// redefine / subclass-layout recompute). Safe no-op when the flag is off.
    pub fn register_compact_layout_if_enabled(&self, id: ClassId) {
        let compact = cratonvm_types::compact_ref_fields_enabled();
        // `CRATONVM_DBG_LAYOUT=1`'s FIRST job is the class-id -> NAME mapping,
        // and that mapping exists in every configuration. It used to sit past
        // the `compact_ref_fields_enabled()` early return, so with compact
        // layouts off — the default — the flag printed NOTHING, while three
        // separate doc comments told readers to use it for exactly this:
        //
        //   gc/src/autobox.rs        "run with CRATONVM_DBG_LAYOUT=1, which
        //                             prints `[layout] <name> cid=<N> ...`"
        //   gc/src/heap.rs           "Run with CRATONVM_DBG_LAYOUT=1 to resolve
        //                             a class_id to a name"
        //   types/src/compact_value.rs  the same sentence again
        //
        // MEASURED (`G45-1`): `CRATONVM_DBG_COERCION=1 CRATONVM_DBG_LAYOUT=1`
        // over `RJdkSecurity` produced 13,135 lines and not one `[layout]` row,
        // so none of the six class ids in that transcript could be resolved.
        // A cheap gate for a DIFFERENT feature was standing in front of an
        // informative one and hiding its zero — and `autobox.rs`'s own comment
        // says it best about the flag it replaced: "a diagnostic that names the
        // wrong instrument costs more than no diagnostic, because it is
        // trusted."
        if !compact {
            if loader_flags().dbg_layout {
                let name = self.get(id).map(|c| c.name.to_string()).unwrap_or_default();
                eprintln!(
                    "[layout] {name} cid={} (no compact layout: compact reference \
                     fields are disabled in this build/configuration)",
                    id.as_u32()
                );
            }
            return;
        }
        let built = self.build_compact_layout(id);
        // Diagnostic for compressed-oops / layout work: shows whether a class
        // got the compact tagless layout at all, and how wide its body is.
        if loader_flags().dbg_layout {
            let name = self.get(id).map(|c| c.name.to_string()).unwrap_or_default();
            match &built {
                Some(l) => eprintln!(
                    "[layout] {name} cid={} total={} refs={} fields={}",
                    id.as_u32(),
                    l.total_size,
                    l.ref_disps.len(),
                    l.field_disps.len()
                ),
                // A `None` here is almost always the `padded` refusal below,
                // and the two numbers that say so are the ones the message
                // used to omit: a class whose `num_total_fields` exceeds the
                // instance fields its chain actually DECLARES was padded to a
                // synthetic-stub floor, and pays the legacy 16-byte tagged
                // cell for every slot -- not just for the padding. Printing
                // them turns "no compact layout" from a fact into a cause.
                None => {
                    let (total, declared) = self.declared_vs_total(id);
                    eprintln!(
                        "[layout] {name} cid={} LEGACY, no compact layout (num_total_fields={total} declared_instance_fields={declared}{})",
                        id.as_u32(),
                        if total > declared {
                            format!(" PADDED by {}", total - declared)
                        } else {
                            String::new()
                        }
                    );
                }
            }
        }
        if let Some(layout) = built {
            cratonvm_types::register_class_layout(
                self.layout_domain,
                id.as_u32(),
                Arc::new(layout),
            );
        }
    }

    /// `(num_total_fields, instance fields actually DECLARED by the chain)`
    /// for `id` -- the pair whose difference is the synthetic-stub padding.
    ///
    /// Read only by the `CRATONVM_DBG_LAYOUT` message above. Kept beside
    /// [`Self::build_compact_layout`] because it walks the same chain and must
    /// count the same way the padding loop there does: instance fields only,
    /// every ancestor, `java/lang/Object` included.
    fn declared_vs_total(&self, id: ClassId) -> (usize, usize) {
        let total = self.get(id).map_or(0, |c| c.num_total_fields);
        let mut declared = 0usize;
        let mut cur = Some(id);
        while let Some(cid) = cur {
            let Some(class) = self.get(cid) else { break };
            declared += class
                .fields
                .iter()
                .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
                .count();
            cur = class.superclass;
        }
        (total, declared)
    }

    /// Build the per-class compact instance-field layout: a naturally aligned
    /// tagless payload table (1/2/4/8 bytes), superclasses first, plus the
    /// reference-field oop-map for the GC.
    ///
    /// Handles synthetic-stub **padding**: a class's `num_total_fields` may
    /// exceed its declared instance fields (native `<init>` writes to synthetic
    /// indices). Each ancestor contributes its declared fields followed by any
    /// padding up to *its own* `num_total_fields`, so absolute indices line up
    /// even when an ancestor is padded. Padded / unknown-descriptor slots are
    /// treated as references (8-byte), matching the heap default-init rule
    /// (`Value::Object(None)` for uncovered slots).
    ///
    /// # Assignment order vs declaration order
    ///
    /// Within **one ancestor's own contribution**, offsets are assigned
    /// widest-field-first (see
    /// [`pack_fields_by_width_enabled`](cratonvm_types::pack_fields_by_width_enabled)),
    /// not in declaration order — declaration order leaves alignment gaps that
    /// cost real bytes (`String` bodies 24 instead of 16). The *recorded*
    /// mapping is unchanged in meaning: `field_offsets[i]` is still the offset
    /// of absolute field index `i`, and every accessor reaches a field through
    /// that table, so nothing outside this function can observe the order in
    /// which the offsets were handed out.
    ///
    /// The reorder is deliberately scoped to a single ancestor's own fields so
    /// the **parent-prefix property** survives: a parent's contribution is laid
    /// out from the same starting offset, over the same field set, in the same
    /// sorted order, whether it is being built for the parent's own layout or
    /// as the prefix of a child's. Inherited absolute indices therefore still
    /// map to identical offsets across the whole hierarchy — which is what lets
    /// a `get_field` on a supertype-typed reference use one offset table.
    fn build_compact_layout(&self, id: ClassId) -> Option<CompactLayout> {
        self.build_compact_layout_ordered(id, cratonvm_types::pack_fields_by_width_enabled())
    }

    /// [`Self::build_compact_layout`] with the assignment order passed in
    /// rather than read from the process-wide flag.
    ///
    /// The flag is a `OnceLock`, so a process can only ever observe one of its
    /// two values — which would make the declaration-order control arm
    /// untestable in the same run as the packed arm, and an A/B where one side
    /// cannot be exercised is not an A/B.
    fn build_compact_layout_ordered(
        &self,
        id: ClassId,
        pack_by_width: bool,
    ) -> Option<CompactLayout> {
        // Superclass chain, root (java/lang/Object) first.
        let mut chain: Vec<ClassId> = Vec::new();
        let mut cur = Some(id);
        while let Some(cid) = cur {
            chain.push(cid);
            cur = self.get(cid)?.superclass;
        }
        chain.reverse();

        // A compact instance has no shape word: its field count is read from
        // its class's CURRENT layout (`ObjectHeader::num_slots`). That is only
        // sound for a class whose layout cannot change while instances exist,
        // and a compatibility stub's can -- `upgrade_synthetic_class` grows it
        // (and re-registers every descendant) once real bytes turn up. So a
        // stub, and anything that inherits from one, stays on the legacy
        // self-describing layout, whose long header records the count.
        if chain.iter().any(|&cid| {
            self.get(cid)
                .is_some_and(|c| c.origin.is_compatibility_stub())
        }) {
            return None;
        }

        let total = self.get(id)?.num_total_fields;
        let mut field_offsets: Vec<u32> = Vec::with_capacity(total);
        let mut is_ref: Vec<bool> = Vec::with_capacity(total);
        let mut field_kinds: Vec<cratonvm_types::FieldStorageKind> = Vec::with_capacity(total);
        let mut ref_offsets: Vec<u32> = Vec::new();
        let mut off: u32 = 0;
        let mut count: usize = 0;
        let mut padded = false;

        for cid in chain {
            let class = self.get(cid)?;

            // This ancestor's own declared instance fields, in declaration
            // order — which is the order their absolute indices are in.
            let mut declared: Vec<cratonvm_types::FieldStorageKind> = Vec::new();
            for f in &class.fields {
                if f.access_flags.contains(FieldAccessFlags::STATIC) {
                    continue;
                }
                let b = f.descriptor.as_bytes().first().copied().unwrap_or(0);
                declared.push(cratonvm_types::FieldStorageKind::from_descriptor_byte(b)?);
            }

            // The order offsets are *assigned* in.
            //
            // Plain width-descending is NOT enough, and `java.util.LinkedList`
            // is the counterexample that proves it: `AbstractList` leaves the
            // running offset at 4 (`int modCount`), and declaration order then
            // happens to fill that 4-byte hole with `int size` before the two
            // `Node` references, landing on 24. Sorting widest-first instead
            // puts an 8-byte reference there, wastes the hole, and ends at 28 —
            // which rounds to **32**. Width-first made it bigger.
            //
            // So the assignment is width-descending *with the current hole
            // preferred*: at each step, take the widest remaining field that
            // needs no padding where the cursor already is, and only pay for
            // padding when nothing fits. That places `size` into the hole and
            // gets LinkedList back to 24, while still packing `String` to 16.
            let order: Vec<usize> = if pack_by_width {
                let mut by_width: Vec<usize> = (0..declared.len()).collect();
                // Widest first; references before same-width primitives so the
                // oop-map the GC scan walks stays contiguous; declaration index
                // as the final tiebreak, which makes the order total and
                // deterministic — a redefine over the same field set must
                // produce the same layout, because the version registry keys on
                // it.
                by_width.sort_by_key(|&i| {
                    let storage = declared[i];
                    (
                        std::cmp::Reverse(storage.size_runtime()),
                        !storage.is_reference(),
                        i,
                    )
                });
                let mut placed = vec![false; declared.len()];
                let mut cursor = off;
                let mut chosen: Vec<usize> = Vec::with_capacity(declared.len());
                for _ in 0..declared.len() {
                    // First choice: the widest unplaced field already aligned
                    // at the cursor. Second: the widest unplaced field at all.
                    let pick = by_width
                        .iter()
                        .copied()
                        .find(|&i| !placed[i] && cursor % declared[i].alignment_runtime() == 0)
                        .or_else(|| by_width.iter().copied().find(|&i| !placed[i]));
                    let Some(i) = pick else { break };
                    placed[i] = true;
                    let alignment = declared[i].alignment_runtime();
                    cursor = (cursor + alignment - 1) & !(alignment - 1);
                    cursor += declared[i].size_runtime();
                    chosen.push(i);
                }
                // Insurance, not decoration: this makes "never larger than
                // declaration order" a property of the output rather than a
                // hope about the heuristic. The comparison is scoped to one
                // ancestor's own contribution and uses only that ancestor's
                // incoming offset, so both a parent's own layout and the same
                // parent's prefix inside a child reach the identical verdict —
                // which is what keeps inherited fields on identical offsets.
                let declaration_end = declared.iter().fold(off, |acc, storage| {
                    let alignment = storage.alignment_runtime();
                    ((acc + alignment - 1) & !(alignment - 1)) + storage.size_runtime()
                });
                if cursor <= declaration_end {
                    chosen
                } else {
                    (0..declared.len()).collect()
                }
            } else {
                (0..declared.len()).collect()
            };

            // Recorded per absolute index, not per assignment position: the
            // tables stay indexed the way every accessor indexes them.
            let base = field_offsets.len();
            field_offsets.resize(base + declared.len(), 0);
            is_ref.resize(base + declared.len(), false);
            field_kinds.resize(
                base + declared.len(),
                cratonvm_types::FieldStorageKind::Reference,
            );
            for &i in &order {
                let storage = declared[i];
                let alignment = storage.alignment_runtime();
                off = (off + alignment - 1) & !(alignment - 1);
                field_offsets[base + i] = off;
                let r = storage.is_reference();
                is_ref[base + i] = r;
                field_kinds[base + i] = storage;
                if r {
                    ref_offsets.push(off);
                }
                off += storage.size_runtime();
            }
            count += declared.len();

            // Pad up to this ancestor's own total so absolute indices stay
            // aligned. Padding is never reordered: it has no descriptor, so
            // there is no width to sort on — and a padded class is refused
            // outright below anyway.
            let target = class.num_total_fields;
            while count < target {
                let storage = cratonvm_types::FieldStorageKind::Reference;
                let alignment = storage.alignment_runtime();
                off = (off + alignment - 1) & !(alignment - 1);
                field_offsets.push(off);
                is_ref.push(true);
                field_kinds.push(storage);
                ref_offsets.push(off);
                off += storage.size_runtime();
                count += 1;
                padded = true;
            }
        }

        // `CompactLayout::ref_offsets` is documented as ascending, and the GC
        // scan loops read it in order. Assignment order is width-descending, so
        // the pushes above are not sorted by construction any more.
        ref_offsets.sort_unstable();

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

        // Every object begins on an 8-byte boundary. Rounding the tail keeps
        // the next header aligned without expanding individual fields.
        off = (off + 7) & !7;

        // The layout above is relative to the field area; the registry hands
        // out ABSOLUTE displacements, so every consumer addresses a field as
        // `base + disp` with no header size of its own to get wrong. A compact
        // instance's header is only the 8-byte header word, and the object is
        // padded to `MIN_OBJECT_SIZE` so it always has a second word (the
        // forwarding target's home during a relocating collection).
        let header = cratonvm_types::COMPACT_HEADER_SIZE as u32;
        let field_disps: Vec<u32> = field_offsets.iter().map(|&o| o + header).collect();
        let ref_disps: Vec<u32> = ref_offsets.iter().map(|&o| o + header).collect();
        let total_size = (header + off).max(cratonvm_types::MIN_OBJECT_SIZE as u32);

        Some(CompactLayout {
            field_disps,
            is_ref,
            field_kinds,
            ref_disps,
            total_size,
        })
    }

    /// Look up a class by id.
    pub fn get(&self, id: ClassId) -> Option<&Class> {
        self.classes.get(id.as_u32() as usize)?.as_ref()
    }

    /// Look up a class by id (mutable).
    pub fn get_mut(&mut self, id: ClassId) -> Option<&mut Class> {
        self.classes.get_mut(id.as_u32() as usize)?.as_mut()
    }

    /// Replace a live class slot with a tombstone and return its metadata.
    ///
    /// ClassIds are deliberately never reused: [`Self::next_id`] remains based
    /// on the slot vector length, not `live_count`.
    pub fn remove(&mut self, id: ClassId) -> Option<Class> {
        let class = self.classes.get_mut(id.as_u32() as usize)?.take()?;
        self.live_count = self.live_count.saturating_sub(1);
        // The closures: this slot and every class that reached this one. Walked
        // backwards over the two indices, so before they lose this id's lists.
        self.forget_secondary_supers_reaching(id.as_u32());
        // The display: this slot, and (defensively — see below: none should
        // exist) every descendant, whose chains ran through it. Done before
        // the subclass index loses this id's child list.
        if let Some(slot) = self.displays.get_mut(id.as_u32() as usize) {
            *slot = PrimaryDisplay::INVALID;
        }
        for d in self.descendants_of(id) {
            if let Some(slot) = self.displays.get_mut(d.as_u32() as usize) {
                *slot = PrimaryDisplay::INVALID;
            }
            self.refresh_deep_interface_key(d.as_u32());
        }
        // Drop the unloaded class's edges from the adjacency index. Its own
        // child list goes too: a live subclass of an unloaded class cannot
        // exist (the subclass keeps its superclass reachable), so any entry
        // left here would only ever name tombstones.
        if let Some(sid) = class.superclass {
            if let Some(kids) = self.subclasses.get_mut(&sid.as_u32()) {
                kids.retain(|&k| k != id.as_u32());
            }
        }
        self.subclasses.remove(&id.as_u32());
        // Same two directions for the interface mirror, on the same argument:
        // this class's edges UP to the interfaces it implemented go, and its
        // own implementor list goes too — a live implementor of an unloaded
        // interface cannot exist, because the implementor keeps its interfaces
        // reachable, so any entry left under this id would only ever name
        // tombstones. Leaving either behind would let `subtypes_of` report a
        // tombstoned class as a subtype, which `get` then drops silently while
        // it still counted against the caller's cap.
        for iface in &class.interfaces {
            unlink_subtype_edge(&mut self.implementors, iface.as_u32(), id.as_u32());
        }
        self.implementors.remove(&id.as_u32());
        self.refresh_deep_interface_key(id.as_u32());
        for iface in &class.interfaces {
            self.refresh_deep_interface_key(iface.as_u32());
        }
        // gcd d1/c: only a slot this store's domain owns
        // (`gengc-r5w4-conc8-class-store-remove-unregisters-another-domains-layout`).
        if self.owns_layout_slot(id.as_u32()) {
            cratonvm_types::unregister_class_layout(id.as_u32());
        }
        note_hierarchy_edge_change(self.layout_domain);
        Some(class)
    }

    /// gcd d1/c (2026-09-27) — may this store clear `class_id`'s slot of the
    /// process-wide compact-layout registry?
    ///
    /// The registry is indexed by `class_id`, which every `ClassStore` restarts
    /// from 0, and a slot belongs to the first layout domain that published
    /// into it (`register_class_layout` refuses every other). But
    /// `unregister_class_layout` takes no domain, so [`Self::remove`] used to
    /// clear the slot whoever owned it: a VM unloading its class X, whose
    /// registration had been refused because another VM owns slot X, cleared
    /// that VM's LIVE layout, after which its compact instances of its own
    /// class X could no longer be sized or walked
    /// (`docs/internal/gc/gengc-r5w4-conc8-class-store-remove-unregisters-another-domains-layout-FIXED-20260928.md`).
    ///
    /// `true` in a process that has only ever had one `ClassStore` (every
    /// production embedding; the answer before this check, byte for byte).
    /// Otherwise `true` only when the domain screen admits this store to the
    /// slot's registered layout (`compact_field_disp_storage_in`, the
    /// registry's own ownership predicate; it needs a layout with at least one
    /// field to answer). A zero-field layout of this store's own class then
    /// stays registered in a multi-store process: a few bytes kept, never
    /// another store's layout cleared.
    pub fn owns_layout_slot(&self, class_id: u32) -> bool {
        cratonvm_types::single_layout_domain()
            || cratonvm_types::field_layout::compact_field_disp_storage_in(
                self.layout_domain,
                class_id,
                0,
            )
            .is_some()
    }

    /// The number of loaded classes.
    pub fn len(&self) -> usize {
        self.live_count
    }

    /// The number of `ClassId` **slots** ever allocated, including the
    /// tombstones left behind by [`Self::remove`].
    ///
    /// This — not [`Self::len`] — is the exclusive upper bound on a valid
    /// `ClassId`, because ids are never reused: after an unload the slot
    /// vector still holds the (now empty) entry. The generational-handle
    /// resolver in [`crate::metadata_handle`] uses it to tell "index past the
    /// end of the table" apart from "in range, but the class was unloaded",
    /// which are different bugs.
    pub fn slot_count(&self) -> usize {
        self.classes.len()
    }

    /// Returns true if no classes have been loaded.
    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }

    /// Iterate over all loaded classes.
    pub fn iter(&self) -> impl Iterator<Item = &Class> {
        self.classes.iter().filter_map(Option::as_ref)
    }

    /// Find a class by name. O(n) scan — the class manager maintains a
    /// `HashMap` for fast name-based lookup; this is a fallback.
    pub fn find_by_name(&self, name: &str) -> Option<&Class> {
        self.iter().find(|c| &*c.name == name)
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

/// Find a field by name in the given class or any of its superclasses,
/// **ignoring its descriptor**.
///
/// Returns `(absolute_index, &ClassFileField, ClassId)` where `ClassId` is the
/// class that actually declares the field.
///
/// # This is not the fieldref key
///
/// JVMS §5.4.3.2 resolves a `CONSTANT_Fieldref` by name **and** descriptor, and
/// JVMS §4.5 permits one class to declare several fields sharing a name. This
/// function keys on the name alone, so where a hierarchy has more than one
/// field of that name it returns whichever the search order meets first — which
/// may be a field of an entirely different type from the one the constant pool
/// named.
///
/// Keep using it for a name the caller owns and knows to be unique. For a
/// fieldref, call [`find_field_recursive_by_descriptor`].
pub fn find_field_recursive<'a>(
    class_id: ClassId,
    field_name: &str,
    store: &'a ClassStore,
) -> Option<(usize, &'a ClassFileField, ClassId)> {
    find_field_recursive_matching(class_id, store, |c| c.find_own_field(field_name))
}

/// [`find_field_recursive`] with the JVMS §5.4.3.2 key: name **and**
/// descriptor.
///
/// Same search order — the class itself, then its superinterfaces, then its
/// superclass, repeating — with the descriptor required to match at every step.
/// `None` means no field of that exact name and descriptor exists anywhere in
/// the hierarchy, which JVMS makes a `NoSuchFieldError`; callers that cannot
/// afford to fail closed fall back to the name-only search and count it.
pub fn find_field_recursive_by_descriptor<'a>(
    class_id: ClassId,
    field_name: &str,
    descriptor: &str,
    store: &'a ClassStore,
) -> Option<(usize, &'a ClassFileField, ClassId)> {
    find_field_recursive_matching(class_id, store, |c| {
        c.find_own_field_by_descriptor(field_name, descriptor)
    })
}

/// The JVMS §5.4.3.2 search ORDER, shared by both keys above.
///
///   1. Look in C itself.
///   2. Otherwise, recursively search the direct superinterfaces of C.
///   3. Otherwise, recursively search the superclass of C.
///
/// Perf: the per-level interface BFS reuses two scratch buffers (`queue`,
/// `visited`) across superclass levels instead of allocating a fresh `Vec` +
/// default-hasher `HashSet` per level. `visited` is an `FxHashSet` — the fx
/// hasher is faster than the SipHash default and the `ClassId` keys are not
/// attacker-keyed.
fn find_field_recursive_matching<'a>(
    class_id: ClassId,
    store: &'a ClassStore,
    own: impl Fn(&'a Class) -> Option<(usize, &'a ClassFileField)>,
) -> Option<(usize, &'a ClassFileField, ClassId)> {
    let mut current_id = class_id;
    let mut queue: Vec<ClassId> = Vec::new();
    let mut visited: FxHashSet<ClassId> = FxHashSet::default();
    loop {
        let class = store.get(current_id)?;
        if let Some((idx, field)) = own(class) {
            return Some((idx, field, current_id));
        }
        // Phase 2: at each level, also search the superinterfaces,
        // depth-first in declaration order (JVMS §5.4.3.2 is recursive:
        // each direct superinterface, then ITS superinterfaces, before the
        // next direct one — HotSpot's `InstanceKlass::find_interface_field`).
        // `queue` is used as a stack, each level pushed in reverse so the
        // first-declared interface pops first (interpreter round i1 wave 29,
        // `i29-L4-field-resolution-searches-superinterfaces-breadth-first`).
        // Per the spec, only static fields can be inherited from interfaces;
        // we still return whatever matches and let the caller distinguish
        // static vs. instance via the field flags.
        queue.clear();
        visited.clear();
        queue.extend(class.interfaces.iter().rev());
        while let Some(iface_id) = queue.pop() {
            if !visited.insert(iface_id) {
                continue;
            }
            if let Some(iface) = store.get(iface_id) {
                if let Some((idx, field)) = own(iface) {
                    return Some((idx, field, iface_id));
                }
                queue.extend(iface.interfaces.iter().rev());
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

/// JVMS §6.5 `invokespecial` — the "actual method selection" step. Resolving
/// an `invokespecial` constant-pool entry (JVMS §5.4.3.3, what
/// [`find_method_recursive`] performs when handed the CP-referenced class)
/// is NOT the same as selecting the method to invoke: for a genuine
/// `super.m(...)` call, selection restarts the search at a DIFFERENT class
/// than the one named in the constant pool.
///
/// Per spec: if the resolved method is not an instance-initialization
/// method, the symbolic reference names a class (not an interface) that is
/// a superclass of the CALLING class (the JVM treats every class file as
/// `ACC_SUPER` since Java SE 8), the
/// search for the method to invoke begins at the calling class's own DIRECT
/// SUPERCLASS — not at the class named by the constant-pool reference.
///
/// This matters whenever the constant pool reference names an ANCESTOR
/// further up the hierarchy than the caller's immediate superclass (common
/// for a compiler-generated bridge, or any multi-level `super` chain): a
/// class sitting BETWEEN the caller and that ancestor may override the
/// method, and only starting the walk at the caller's own superclass will
/// find it. Starting the walk at the CP-referenced class directly (what a
/// plain `find_method_recursive(cp_class_id, ...)` call does) walks straight
/// past that override and can land on a much-less-specific declaration
/// higher up the chain (in the worst case, `java/lang/Object`, if the
/// erased name/descriptor happens to coincide there too).
///
/// Returns `cp_class_id` unchanged (the plain, non-redirected resolution
/// start) for `<init>`, for interface references, or whenever the redirect
/// condition does not hold — i.e. it is always safe to feed the result
/// straight into [`find_method_recursive`] in place of `cp_class_id`.
/// JVMS §5.4.6 `invokevirtual` — does this site name a **private** method?
///
/// The `invokevirtual` counterpart of [`invokespecial_selection_start`], and
/// here for the same reason: resolving the constant-pool entry is not the
/// same as selecting the method to invoke, and the difference is a rule of
/// the spec rather than a policy of any one caller.
///
/// JVMS §5.4.6 selects a private method as the one the constant pool
/// resolved, with **no override lookup at all**. javac has emitted
/// `invokevirtual` for a call to a private instance method since Java 11
/// (JEP 181 nestmates), where it used to emit `invokespecial` — the opcode
/// changed, the semantics did not. Every compiled dispatcher resolves an
/// ordinary `invokevirtual` by walking up from the RECEIVER's class, so on
/// such a site it finds the most-derived same-named private method and calls
/// THAT. The shape is ordinary — a class whose constructor calls its own
/// `private void init()`, subclassed by a class that does the same — and
/// `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
/// `HttpClientOptions` are three such levels in one chain.
///
/// Returns the **declaring class** of the resolved method when the site
/// names a private one, and `None` otherwise (not private, or not
/// resolvable). A `Some` means the site must be classified as a direct,
/// non-dispatching bind rather than as virtual dispatch. No class-name
/// substitution is needed on top: JVM access control makes a private method
/// invocable only from the class that declares it, so the constant pool's
/// owner already IS the declaring class.
///
/// Had four copies until 2026-08-28 — one per compile door plus the
/// interpreter's own — which is what the resolve guard's bypass budget
/// noticed. One rule, one implementation, in the module that owns selection.
pub fn invokevirtual_private_declaring_class(
    cp_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    store: &ClassStore,
) -> Option<ClassId> {
    // The RESOLVED method is the first declaration on the superclass chain,
    // whatever its flags (JVMS §5.4.3.3) -- not `find_method_recursive`'s
    // answer, which steps past an abstract re-declaration to a private
    // ancestor and bound the ancestor's body (interpreter round i1 wave 29,
    // lane L4; `tools/probes/interp/L4/L4W29PrivateAncestorShadow.java`).
    let mut cur = Some(cp_class_id);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > 4096 {
            return None;
        }
        let class = store.get(id)?;
        if let Some(method) = class.find_method(method_name, method_descriptor) {
            return method
                .access_flags
                .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE)
                .then_some(id);
        }
        cur = class.superclass;
    }
    None
}

/// JVMS §5.4.6 `invokevirtual` — is this site's target UNOVERRIDABLE, and if
/// so where is it declared?
///
/// The third of this module's `invokevirtual`/`invokespecial` selection
/// rules, beside [`invokevirtual_private_declaring_class`] and
/// [`invokespecial_selection_start`]. All three answer the same shape of
/// question — *what does dispatch at this site actually select?* — and all
/// three are wanted by more than one compile door, which is why they live
/// here rather than in whichever door needed them first.
///
/// A method is unoverridable when it is `final`, or when the class the
/// constant pool NAMES is final, or when its declaring class is. The middle
/// one is not redundant: the receiver must be an instance of the CP class,
/// so if that class is final the receiver's class IS it, and selection
/// cannot reach anywhere the resolution walk did not just look.
///
/// Deliberately narrower than the letter of the rule, and for stated
/// reasons rather than by omission:
///
/// * `native` is excluded — a registered native has no compiled body to
///   bind to, and the doors route natives through their own machinery.
/// * `abstract` and `static` are excluded as impossible-by-construction
///   (a `final abstract` method is illegal, and `invokevirtual` never names
///   a `static` one), so a malformed classfile falls back to dispatch
///   rather than binding.
/// * `private` is excluded because it is the OTHER rule's answer — see
///   [`invokevirtual_private_declaring_class`].
///
/// Returns the DECLARING class, which a caller must substitute for the
/// constant pool's class name before any direct bind: the CP entry commonly
/// names a subclass (`PooledHeapByteBuf.checkIndex`) while the body lives on
/// the ancestor that declares it (`AbstractByteBuf`), and binding under the
/// subclass name would key the compiled callee under a method that class
/// does not declare.
pub fn invokevirtual_final_declaring_class(
    cp_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    store: &ClassStore,
) -> Option<ClassId> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    // The first declaration on the chain decides (interpreter round i1 wave
    // 35, item 6 of `i29-L4-invokespecial-and-invokeinterface-selection-…`):
    // `find_method_recursive` steps past an abstract re-declaration to a
    // concrete ancestor, so `final C extends abstract B { abstract m }
    // extends A { m }` bound A.m in compiled code, where selection finds B's
    // abstract `m` and raises `AbstractMethodError`. Such a site is not
    // devirtualised; dispatch raises the error.
    let mut cur = Some(cp_class_id);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > 4096 {
            return None;
        }
        let class = store.get(id)?;
        if let Some(m) = class.find_method(method_name, method_descriptor) {
            if m.access_flags.contains(MethodAccessFlags::ABSTRACT) {
                return None;
            }
            break;
        }
        cur = class.superclass;
    }
    let (method, declaring_id) =
        find_method_recursive(cp_class_id, method_name, method_descriptor, store)?;
    if method.access_flags.intersects(
        MethodAccessFlags::ABSTRACT
            | MethodAccessFlags::STATIC
            | MethodAccessFlags::NATIVE
            | MethodAccessFlags::PRIVATE,
    ) {
        return None;
    }
    let class_is_final = |id| {
        store
            .get(id)
            .is_some_and(|c| c.access_flags.contains(ClassAccessFlags::FINAL))
    };
    if !method.access_flags.contains(MethodAccessFlags::FINAL)
        && !class_is_final(cp_class_id)
        && !class_is_final(declaring_id)
    {
        return None;
    }
    Some(declaring_id)
}

pub fn invokespecial_selection_start(
    caller_class_id: ClassId,
    cp_class_id: ClassId,
    cp_reference_is_interface: bool,
    method_name: &str,
    store: &ClassStore,
) -> ClassId {
    if cp_reference_is_interface || method_name == "<init>" {
        return cp_class_id;
    }
    let Some(caller) = store.get(caller_class_id) else {
        return cp_class_id;
    };
    // No `ACC_SUPER` test: "In Java SE 8 and above, the Java Virtual Machine
    // considers the ACC_SUPER flag to be set in every class file" (JVMS
    // section 4.1), and HotSpot 25 selects from the superclass for a caller without
    // it (interpreter round i1 wave 29, `i29-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot`
    // item 2).
    // `cp_class_id` must be a genuine (proper) superclass of the caller —
    // walk the caller's own superclass chain looking for it.
    let mut cur = caller.superclass;
    let mut is_superclass = false;
    while let Some(id) = cur {
        if id == cp_class_id {
            is_superclass = true;
            break;
        }
        cur = store.get(id).and_then(|c| c.superclass);
    }
    if !is_superclass {
        return cp_class_id;
    }
    // `caller.superclass` is guaranteed `Some` here: the loop above only
    // sets `is_superclass` after having entered it at least once.
    caller.superclass.unwrap_or(cp_class_id)
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
            origin: ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
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

    fn make_field_desc(name: &str, descriptor: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
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

    /// Interpreter round i1 wave 35: `final C extends abstract B { abstract
    /// m } extends A { m }` is not devirtualised to `A.m`, since selection
    /// finds B's abstract `m`; without the re-declaration it binds `A.m`.
    #[test]
    fn a_final_class_over_an_abstract_redeclaration_is_not_devirtualised() {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let mut store = ClassStore::new();
        let a = store.next_id();
        let mut concrete = make_method("m", "()V");
        concrete.access_flags = MethodAccessFlags::PUBLIC;
        store.add(make_class(
            a,
            "A",
            None,
            vec![],
            vec![],
            vec![concrete],
            0,
            0,
        ));
        let b = store.next_id();
        let mut abstract_m = make_method("m", "()V");
        abstract_m.access_flags = MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT;
        let mut b_class = make_class(b, "B", Some(a), vec![], vec![], vec![abstract_m], 0, 0);
        b_class.access_flags |= ClassAccessFlags::ABSTRACT;
        store.add(b_class);
        let c = store.next_id();
        let mut c_class = make_class(c, "C", Some(b), vec![], vec![], vec![], 0, 0);
        c_class.access_flags |= ClassAccessFlags::FINAL;
        store.add(c_class);
        assert_eq!(
            invokevirtual_final_declaring_class(c, "m", "()V", &store),
            None
        );

        let d = store.next_id();
        let mut d_class = make_class(d, "D", Some(a), vec![], vec![], vec![], 0, 0);
        d_class.access_flags |= ClassAccessFlags::FINAL;
        store.add(d_class);
        assert_eq!(
            invokevirtual_final_declaring_class(d, "m", "()V", &store),
            Some(a)
        );
    }

    #[test]
    fn class_id_display() {
        let id = ClassId::new(42);
        assert_eq!(format!("{id}"), "42");
        assert_eq!(id.as_u32(), 42);
    }

    // --- Compact layout: width-descending field assignment -------------------

    fn typed_field(name: &str, descriptor: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        }
    }

    /// `java.lang.String`'s exact field set and declaration order.
    fn string_shaped_fields() -> Vec<ClassFileField> {
        vec![
            typed_field("value", "[B"),
            typed_field("coder", "B"),
            typed_field("hash", "I"),
            typed_field("hashIsZero", "Z"),
        ]
    }

    /// The whole point of the reorder: `String` is 14 bytes of field data, and
    /// declaration order spends 24 on it (`coder` at 8 forces `hash` to skip to
    /// 12, and `hashIsZero` lands at 16, rounding the body to 24). Widest-first
    /// packs the same four fields into 14, which rounds to 16.
    ///
    /// Asserted as *both* arms in one test, against the same class, so a
    /// regression that quietly stops reordering cannot read as a pass.
    #[test]
    fn width_packing_saves_eight_bytes_on_a_string_shaped_class() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(make_class(
            id,
            "java/lang/String",
            None,
            vec![],
            string_shaped_fields(),
            vec![],
            0,
            4,
        ));

        let packed = store.build_compact_layout_ordered(id, true).unwrap();
        assert_eq!(
            packed.total_size,
            8 + 16,
            "widest-first must pack String's 14 bytes of fields into 16"
        );
        // value(8) @0, hash(4) @8, coder(1) @12, hashIsZero(1) @13.
        assert_eq!(packed.field_disps, vec![8, 20, 16, 21]);
        assert_eq!(packed.ref_disps, vec![8]);

        let declared = store.build_compact_layout_ordered(id, false).unwrap();
        assert_eq!(
            declared.total_size,
            8 + 24,
            "declaration order is the control: it must still cost 24"
        );
        assert_eq!(declared.field_disps, vec![8, 16, 20, 24]);
    }

    /// Characterises the split-loader shape these two predicates disagree on,
    /// using the `AotIntegrationTests` case as the concrete example: a proxy
    /// implementing an interface whose `ClassId` is the child loader's copy,
    /// tested against the parent loader's copy of the same name.
    ///
    /// `is_subclass_of` must answer `false` — the ids genuinely differ, and
    /// that is the honest answer to the question it was asked.
    /// `is_assignable_to_name` must answer `true`, because it is asked the
    /// question a flat class store can actually answer. Both arms are asserted
    /// against the same pair so a regression that collapses one into the other
    /// cannot read as a pass, and a negative pins that the name walk has not
    /// rotted into "everything is assignable".
    ///
    /// The interface leg is the part `is_subclass_of_by_name` cannot do: an
    /// annotation type is an interface, so a proxy reaches it through
    /// `interfaces`, never through `superclass`. Five callers depend on this
    /// distinction — exception `catch_type` matching, JIT
    /// `checkcast`/`instanceof`, the recovered-mirror receiver check,
    /// `VarHandle` return coercion and the `Serializable` probe — and none of
    /// them had a test that built the two-copy hierarchy explicitly.
    #[test]
    fn a_proxy_is_assignable_to_the_other_loaders_copy_of_its_interface_by_name() {
        let mut store = ClassStore::new();

        // The parent loader's copy — the one the `ContextConfiguration[]` array
        // was created with.
        let parent_iface = store.next_id();
        store.add(make_class(
            parent_iface,
            "org/springframework/test/context/ContextConfiguration",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));

        // The forked loader's copy: same name, different id. This is the whole
        // defect in one line.
        let forked_iface = store.next_id();
        store.add(make_class(
            forked_iface,
            "org/springframework/test/context/ContextConfiguration",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        assert_ne!(parent_iface, forked_iface);

        // `jdk/proxy3/$Proxy27`, synthesized against the FORKED copy.
        let proxy = store.next_id();
        store.add(make_class(
            proxy,
            "jdk/proxy3/$Proxy27",
            None,
            vec![forked_iface],
            vec![],
            vec![],
            0,
            0,
        ));

        let proxy_class = store.get(proxy).expect("proxy present");

        // Exact-identity: correctly false against the parent's copy, true
        // against its own. Neither is a bug; the bug is stopping there.
        assert!(
            !proxy_class.is_subclass_of(parent_iface, &store),
            "the two copies have different ids, so identity must not match — if \
             this starts passing, the loader split itself was fixed and the \
             fallback below is no longer the thing under test",
        );
        assert!(proxy_class.is_subclass_of(forked_iface, &store));

        // Name-based: reaches the interface through the DAG and answers the
        // question HotSpot would have, where there is only one copy.
        assert!(
            proxy_class.is_assignable_to_name(
                "org/springframework/test/context/ContextConfiguration",
                &store,
            ),
            "the name walk must cross the loader split — a proxy created FROM \
             an annotation type must not read as unrelated to it just because \
             a forked loader owns the copy it was built against",
        );

        // ...and it is still a real check: an unrelated name is refused, so
        // `ArrayStoreException` fidelity survives for the case the check exists
        // for (a String into a Runnable[]).
        assert!(
            !proxy_class.is_assignable_to_name("java/lang/Runnable", &store),
            "the fallback must not degrade into 'everything is assignable'",
        );
    }

    /// r9w4 vm4: `is_assignable_to_name` now dedupes with [`VisitedClasses`]
    /// (stack array + `Vec` spill) instead of an `FxHashSet`. Pin the two
    /// behaviours the set exists for on a DAG wider than the inline capacity:
    /// a shared super-interface reached through 40 branches is still found,
    /// and an unrelated name is still refused (the walk terminates instead of
    /// going exponential, and the spill keeps deduping past 32 entries).
    #[test]
    fn r9w4_assignable_to_name_dedupes_a_wide_interface_diamond() {
        let mut store = ClassStore::new();
        let root = store.next_id();
        store.add(make_class(
            root,
            "p/Root",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        let mut branches = Vec::new();
        for i in 0..40 {
            let id = store.next_id();
            store.add(make_class(
                id,
                &format!("p/Branch{i}"),
                None,
                vec![root],
                vec![],
                vec![],
                0,
                0,
            ));
            branches.push(id);
        }
        let leaf = store.next_id();
        store.add(make_class(
            leaf,
            "p/Leaf",
            None,
            branches,
            vec![],
            vec![],
            0,
            0,
        ));
        let leaf_class = store.get(leaf).expect("leaf present");
        assert!(leaf_class.is_assignable_to_name("p/Root", &store));
        assert!(leaf_class.is_assignable_to_name("p/Branch39", &store));
        assert!(!leaf_class.is_assignable_to_name("p/Unrelated", &store));
        // The supers-only walk shares the set type; a class with no superclass
        // chain to the target is still a plain negative.
        assert!(!leaf_class.is_subclass_of_by_name("p/Root", &store));
    }

    /// The two name-based walks are NOT interchangeable, and the pair of
    /// asserts below is the whole reason to prefer one at a call site.
    ///
    /// `is_subclass_of_by_name` exists for exception `catch_type` matching,
    /// where the target is never an interface, so it walks only the superclass
    /// chain. Its name does not say so, and a caller asking about an INTERFACE
    /// gets a silent, permanent `false` rather than an error — which is
    /// indistinguishable from a correct negative and so cannot be noticed
    /// without a test like this one.
    ///
    /// It has been read the wrong way twice. The `Path.toString()` branch in
    /// `vm/src/runtime/invokedynamic.rs` carries the first write-up (that
    /// version "never actually fires"); the second was
    /// `interpreter::typecheck::unmod_backing_reaches`, which decides between
    /// `Collections$UnmodifiableList` and `Collections$UnmodifiableRandomAccessList`
    /// for the class the `instanceof`/`checkcast` opcodes consult. It asked
    /// about `java/util/RandomAccess` — an interface — so it answered `false`
    /// for every list, and `x instanceof RandomAccess` disagreed with both
    /// `RandomAccess.class.isInstance(x)` and `x.getClass()`.
    ///
    /// The hierarchy below is `ArrayList`'s, shortened to the part that
    /// matters: the marker is reached through `interfaces`, and there is no
    /// route to it through `superclass` at all.
    #[test]
    fn the_supers_only_name_walk_cannot_see_an_interface_the_dag_walk_finds() {
        let mut store = ClassStore::new();

        let random_access = store.next_id();
        store.add(make_class(
            random_access,
            "java/util/RandomAccess",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        let abstract_list = store.next_id();
        store.add(make_class(
            abstract_list,
            "java/util/AbstractList",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        let array_list = store.next_id();
        store.add(make_class(
            array_list,
            "java/util/ArrayList",
            Some(abstract_list),
            vec![random_access],
            vec![],
            vec![],
            0,
            0,
        ));

        let al = store.get(array_list).expect("ArrayList present");

        // The superclass leg: both walks agree, because a superclass IS on the
        // supers-only path. Asserted so the negative below reads as "interface"
        // and not as "this walk is broken".
        assert!(al.is_subclass_of_by_name("java/util/AbstractList", &store));
        assert!(al.is_assignable_to_name("java/util/AbstractList", &store));

        // The interface leg: they diverge. This is the trap.
        assert!(
            !al.is_subclass_of_by_name("java/util/RandomAccess", &store),
            "if this starts passing, `is_subclass_of_by_name` grew an interface \
             walk — which is a behaviour change for every exception-`catch_type` \
             caller and must be justified there, not discovered here",
        );
        assert!(
            al.is_assignable_to_name("java/util/RandomAccess", &store),
            "the DAG walk is the one a caller asking about an interface must use",
        );

        // Still a real check in both directions.
        assert!(!al.is_assignable_to_name("java/util/Deque", &store));
    }

    /// Reordering must not disturb which *index* names which field: the storage
    /// kind recorded at index `i` is still field `i`'s, whatever offset it got.
    #[test]
    fn width_packing_keeps_field_kinds_on_their_own_indices() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(make_class(
            id,
            "java/lang/String",
            None,
            vec![],
            string_shaped_fields(),
            vec![],
            0,
            4,
        ));

        let layout = store.build_compact_layout_ordered(id, true).unwrap();
        use cratonvm_types::FieldStorageKind::*;
        assert_eq!(layout.field_kinds, vec![Reference, Byte, Int, Boolean]);
        assert_eq!(layout.is_ref, vec![true, false, false, false]);
    }

    /// The parent-prefix property is what lets a `getfield` through a
    /// supertype-typed reference use one offset table, and it is exactly what a
    /// reorder could silently break. A parent's own fields must land on the
    /// same offsets whether the layout being built is the parent's or a
    /// child's — which holds only because the sort is scoped to one ancestor's
    /// contribution rather than applied across the flattened field list.
    #[test]
    fn width_packing_preserves_the_parent_prefix_property() {
        let mut store = ClassStore::new();

        let parent = store.next_id();
        store.add(make_class(
            parent,
            "Parent",
            None,
            vec![],
            vec![
                typed_field("flag", "Z"),
                typed_field("ref", "Ljava/lang/Object;"),
                typed_field("n", "I"),
            ],
            vec![],
            0,
            3,
        ));

        let child = store.next_id();
        store.add(make_class(
            child,
            "Child",
            Some(parent),
            vec![],
            vec![typed_field("extra", "J"), typed_field("small", "S")],
            vec![],
            3,
            5,
        ));

        let parent_layout = store.build_compact_layout_ordered(parent, true).unwrap();
        let child_layout = store.build_compact_layout_ordered(child, true).unwrap();

        assert_eq!(
            child_layout.field_disps[..parent_layout.field_disps.len()],
            parent_layout.field_disps[..],
            "the parent's fields must keep their offsets inside the child"
        );
        assert_eq!(
            child_layout.field_kinds[..parent_layout.field_kinds.len()],
            parent_layout.field_kinds[..],
        );
        // ref(8) @0, n(4) @8, flag(1) @12 -> parent body rounds to 16; the
        // child's own fields continue from the parent's *unrounded* 13.
        assert_eq!(parent_layout.field_disps, vec![20, 8, 16]);
        assert_eq!(parent_layout.total_size, 8 + 16);
    }

    /// Two fields of equal width put the reference first, so the oop-map the GC
    /// walks stays contiguous. `ref_offsets` must also come back ascending —
    /// assignment order is width-descending, so it is no longer sorted by
    /// construction, and the scan loops read it in order.
    #[test]
    fn width_packing_groups_references_and_sorts_the_oop_map() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(make_class(
            id,
            "Mixed",
            None,
            vec![],
            vec![
                typed_field("a", "J"),
                typed_field("r1", "Ljava/lang/Object;"),
                typed_field("b", "D"),
                typed_field("r2", "[I"),
            ],
            vec![],
            0,
            4,
        ));

        let layout = store.build_compact_layout_ordered(id, true).unwrap();
        assert_eq!(layout.ref_disps, vec![8, 16], "refs first, ascending");
        assert!(
            layout.ref_disps.windows(2).all(|w| w[0] < w[1]),
            "ref_disps must be ascending: {:?}",
            layout.ref_disps
        );
        assert_eq!(layout.total_size, 8 + 32);
    }

    /// `java.util.LinkedList`, the shape that caught plain width-descending
    /// making an object **bigger**.
    ///
    /// `AbstractList` contributes `int modCount` and leaves the running offset
    /// at 4. Declaration order then fills that 4-byte hole with `int size`
    /// before the two `Node` references and lands on 24. Naive widest-first put
    /// a reference there instead, wasted the hole, ended at 28 and rounded to
    /// **32** — a measured regression, found by censusing both arms of a real
    /// run rather than by reading the sort. Hole-first placement must get it
    /// back to 24.
    #[test]
    fn width_packing_fills_an_inherited_hole_instead_of_wasting_it() {
        let mut store = ClassStore::new();

        let parent = store.next_id();
        store.add(make_class(
            parent,
            "java/util/AbstractList",
            None,
            vec![],
            vec![typed_field("modCount", "I")],
            vec![],
            0,
            1,
        ));

        let child = store.next_id();
        store.add(make_class(
            child,
            "java/util/LinkedList",
            Some(parent),
            vec![],
            vec![
                typed_field("size", "I"),
                typed_field("first", "Ljava/util/LinkedList$Node;"),
                typed_field("last", "Ljava/util/LinkedList$Node;"),
            ],
            vec![],
            1,
            4,
        ));

        let packed = store.build_compact_layout_ordered(child, true).unwrap();
        let declared = store.build_compact_layout_ordered(child, false).unwrap();
        assert_eq!(
            declared.total_size,
            8 + 24,
            "declaration order is already 24 here"
        );
        assert_eq!(
            packed.total_size,
            8 + 24,
            "hole-first must match declaration order, not regress to 32"
        );
        // modCount @0, size @4 (into the hole), first @8, last @16.
        assert_eq!(packed.field_disps, vec![8, 12, 16, 24]);
    }

    /// The "never larger than declaration order" guarantee, asserted over every
    /// arrangement of a field set chosen to expose the bad cases: an odd
    /// leading primitive, mixed widths, and a reference that has to align.
    ///
    /// This is the property the fallback exists for. A heuristic that is
    /// usually better is not the same as one that is never worse, and only the
    /// second is safe to turn on by default across a whole heap.
    #[test]
    fn width_packing_is_never_larger_than_declaration_order() {
        let descriptors = ["B", "Ljava/lang/Object;", "I", "J", "S", "Z", "[I"];
        // Every rotation of the declaration order, so the incoming alignment
        // and the tail both vary.
        for rot in 0..descriptors.len() {
            let mut store = ClassStore::new();
            let id = store.next_id();
            let fields: Vec<ClassFileField> = (0..descriptors.len())
                .map(|i| {
                    let d = descriptors[(i + rot) % descriptors.len()];
                    typed_field(&format!("f{i}"), d)
                })
                .collect();
            store.add(make_class(
                id,
                "Rotated",
                None,
                vec![],
                fields,
                vec![],
                0,
                descriptors.len(),
            ));

            let packed = store.build_compact_layout_ordered(id, true).unwrap();
            let declared = store.build_compact_layout_ordered(id, false).unwrap();
            assert!(
                packed.total_size <= declared.total_size,
                "rotation {rot}: packed {} > declaration order {}",
                packed.total_size,
                declared.total_size
            );
            // And every field must still be within the body and aligned.
            for (i, (&offset, &kind)) in packed
                .field_disps
                .iter()
                .zip(packed.field_kinds.iter())
                .enumerate()
            {
                assert_eq!(
                    offset % kind.alignment_runtime(),
                    0,
                    "rotation {rot} field {i} at {offset} is misaligned for {kind:?}"
                );
                assert!(offset + kind.size_runtime() <= packed.total_size);
            }
            // No two fields may overlap.
            let mut spans: Vec<(u32, u32)> = packed
                .field_disps
                .iter()
                .zip(packed.field_kinds.iter())
                .map(|(&o, &k)| (o, o + k.size_runtime()))
                .collect();
            spans.sort_unstable();
            for w in spans.windows(2) {
                assert!(
                    w[0].1 <= w[1].0,
                    "rotation {rot}: fields overlap {:?} {:?}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    /// A padded class is still refused outright, reorder or not — the reorder
    /// must not have turned an untyped slot into something the oop-map claims
    /// to know the type of.
    #[test]
    fn width_packing_still_refuses_a_padded_class() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(make_class(
            id,
            "Padded",
            None,
            vec![],
            vec![typed_field("n", "I")],
            vec![],
            0,
            4, // three synthetic slots with no descriptor
        ));

        assert!(store.build_compact_layout_ordered(id, true).is_none());
        assert!(store.build_compact_layout_ordered(id, false).is_none());
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

    /// JVMS §4.5 forbids two fields in one class file sharing a name **and**
    /// descriptor. Sharing a NAME alone is legal, and this is the case the
    /// name-only lookup answered wrongly: it returns whichever comes first in
    /// declaration order, so a fieldref naming the OTHER one got that field's
    /// slot index.
    ///
    /// Asserted as a DIFFERENCE between the two keys on the same class, not as
    /// "the descriptor form works": a test that only checked the new function
    /// would still pass if it silently ignored the descriptor.
    #[test]
    fn two_fields_may_share_a_name_and_only_the_descriptor_tells_them_apart() {
        let id = ClassId::new(0);
        let fields = vec![
            make_field_desc("x", "Ljava/lang/Object;"),
            make_field_desc("x", "I"),
        ];
        let class = make_class(id, "Shadow", None, vec![], fields, vec![], 0, 2);

        // The name-only key cannot distinguish them and takes the first.
        let (idx, field) = class.find_own_field("x").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(&*field.descriptor, "Ljava/lang/Object;");

        // The JVMS key reaches either one.
        let (idx0, f0) = class
            .find_own_field_by_descriptor("x", "Ljava/lang/Object;")
            .unwrap();
        assert_eq!((idx0, &*f0.descriptor), (0, "Ljava/lang/Object;"));

        let (idx1, f1) = class.find_own_field_by_descriptor("x", "I").unwrap();
        assert_eq!(
            (idx1, &*f1.descriptor),
            (1, "I"),
            "the second `x` is reachable only by descriptor, and it is the one a \
             fieldref with descriptor `I` names"
        );

        // A descriptor no field has matches nothing, rather than falling back
        // to a same-named field of another type.
        assert!(class.find_own_field_by_descriptor("x", "J").is_none());
    }

    /// The same distinction across a hierarchy: a subclass shadowing an
    /// inherited field name with a different TYPE. `find_field_recursive` stops
    /// at the subclass's field whatever the fieldref asked for; the descriptor
    /// form walks past it to the one that matches.
    #[test]
    fn a_shadowing_subclass_field_does_not_capture_a_supertype_fieldref() {
        let mut store = ClassStore::new();
        let base_id = store.next_id();
        store.add(make_class(
            base_id,
            "Base",
            None,
            vec![],
            vec![make_field_desc("x", "Ljava/lang/String;")],
            vec![],
            0,
            1,
        ));
        let sub_id = store.next_id();
        store.add(make_class(
            sub_id,
            "Sub",
            Some(base_id),
            vec![],
            vec![make_field_desc("x", "I")],
            vec![],
            1,
            2,
        ));

        // Name only: the subclass's `int x` shadows, whatever was asked for.
        let (idx, field, declaring) = find_field_recursive(sub_id, "x", &store).unwrap();
        assert_eq!((idx, &*field.descriptor, declaring), (1, "I", sub_id));

        // With the descriptor, a fieldref for `Base.x:Ljava/lang/String;`
        // resolved from `Sub` reaches Base's field and Base's slot.
        let (idx, field, declaring) =
            find_field_recursive_by_descriptor(sub_id, "x", "Ljava/lang/String;", &store).unwrap();
        assert_eq!(
            (idx, &*field.descriptor, declaring),
            (0, "Ljava/lang/String;", base_id),
            "the descriptor key must walk PAST the shadowing field, not stop at it"
        );

        // And the subclass's own field is still reachable by its descriptor.
        let (idx, _, declaring) =
            find_field_recursive_by_descriptor(sub_id, "x", "I", &store).unwrap();
        assert_eq!((idx, declaring), (1, sub_id));

        // No field of that name and descriptor anywhere: `None`, not a
        // same-named field of another type.
        assert!(find_field_recursive_by_descriptor(sub_id, "x", "J", &store).is_none());
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

    /// JVMS §5.4.3.2 searches superinterfaces depth-first:
    /// `C implements I1, I2`, `I1 extends I3`, `I3` and `I2` both declare `F`
    /// — HotSpot answers `I3.F` (interpreter round i1 wave 29,
    /// `i29-L4-field-resolution-searches-superinterfaces-breadth-first`).
    #[test]
    fn interface_fields_are_found_depth_first() {
        let mut store = ClassStore::new();
        let i3 = store.next_id();
        store.add(make_class(i3, "I3", None, vec![], vec![make_field("F")], vec![], 0, 1));
        let i2 = store.next_id();
        store.add(make_class(i2, "I2", None, vec![], vec![make_field("F")], vec![], 0, 1));
        let i1 = store.next_id();
        store.add(make_class(i1, "I1", None, vec![i3], vec![], vec![], 0, 0));
        let c = store.next_id();
        store.add(make_class(c, "C", None, vec![i1, i2], vec![], vec![], 0, 0));
        let (_, field, declaring) = find_field_recursive(c, "F", &store).unwrap();
        assert_eq!(&*field.name, "F");
        assert_eq!(declaring, i3, "I1's superinterface I3 before the second direct one, I2");
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

    /// `invokespecial_selection_start` — the JVMS §6.5 super-call redirect.
    /// `Grandparent` <- `Parent` (overrides `m`) <- `Child`. `Child`'s
    /// bytecode does `invokespecial Grandparent.m()V` (the CP entry names
    /// the ancestor, not the direct superclass — the shape a synthetic
    /// bridge or a multi-level `super` chain produces). Naive resolution
    /// starting at `Grandparent` would find `Grandparent.m`, skipping
    /// `Parent`'s override entirely; the JVMS-correct answer starts the
    /// search at `Child`'s own direct superclass (`Parent`) and finds
    /// `Parent.m`.
    #[test]
    fn invokespecial_selection_start_redirects_to_callers_direct_superclass() {
        let mut store = ClassStore::new();

        let grandparent_id = store.next_id();
        store.add(make_class(
            grandparent_id,
            "Grandparent",
            None,
            vec![],
            vec![],
            vec![make_method("m", "()V")],
            0,
            0,
        ));

        let parent_id = store.next_id();
        store.add(make_class(
            parent_id,
            "Parent",
            Some(grandparent_id),
            vec![],
            vec![],
            vec![make_method("m", "()V")],
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
            vec![],
            0,
            0,
        ));

        // The CP entry names Grandparent directly.
        let start = invokespecial_selection_start(child_id, grandparent_id, false, "m", &store);
        assert_eq!(
            start, parent_id,
            "must redirect to the caller's direct superclass"
        );

        let (method, declaring) = find_method_recursive(start, "m", "()V", &store).unwrap();
        assert_eq!(&*method.name, "m");
        assert_eq!(
            declaring, parent_id,
            "must land on Parent's override, not Grandparent's"
        );
    }

    /// No redirect for `<init>` — constructors are never subject to the
    /// super-call selection rule.
    #[test]
    fn invokespecial_selection_start_never_redirects_init() {
        let mut store = ClassStore::new();
        let object_id = store.next_id();
        store.add(make_class(
            object_id,
            "java/lang/Object",
            None,
            vec![],
            vec![],
            vec![make_method("<init>", "()V")],
            0,
            0,
        ));
        let child_id = store.next_id();
        store.add(make_class(
            child_id,
            "Child",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("<init>", "()V")],
            0,
            0,
        ));
        let start = invokespecial_selection_start(child_id, object_id, false, "<init>", &store);
        assert_eq!(start, object_id);
    }

    /// No redirect for a genuinely ordinary (non-super, e.g. private-method
    /// or same-class) `invokespecial`: when the CP-referenced class is NOT
    /// an ancestor of the caller, the plain CP-referenced class is returned
    /// unchanged.
    #[test]
    fn invokespecial_selection_start_no_redirect_when_not_a_superclass() {
        let mut store = ClassStore::new();
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
        let unrelated_id = store.next_id();
        store.add(make_class(
            unrelated_id,
            "Unrelated",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("m", "()V")],
            0,
            0,
        ));
        let caller_id = store.next_id();
        store.add(make_class(
            caller_id,
            "Caller",
            Some(object_id),
            vec![],
            vec![],
            vec![make_method("m", "()V")],
            0,
            0,
        ));
        // `Unrelated` is not a superclass of `Caller` — no redirect.
        let start = invokespecial_selection_start(caller_id, unrelated_id, false, "m", &store);
        assert_eq!(start, unrelated_id);
    }

    /// No redirect for an interface-method reference — JVMS §6.5's redirect
    /// only applies when the symbolic reference names a class.
    #[test]
    fn invokespecial_selection_start_no_redirect_for_interface_reference() {
        let mut store = ClassStore::new();
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
        let iface_id = store.next_id();
        store.add(make_class(
            iface_id,
            "Iface",
            None,
            vec![],
            vec![],
            vec![make_method("m", "()V")],
            0,
            0,
        ));
        let child_id = store.next_id();
        store.add(make_class(
            child_id,
            "Child",
            Some(object_id),
            vec![iface_id],
            vec![],
            vec![],
            0,
            0,
        ));
        let start = invokespecial_selection_start(child_id, iface_id, true, "m", &store);
        assert_eq!(start, iface_id);
    }

    /// Regression test for the Infinispan `GlobalConfiguration` /
    /// `GlobalConfigurationBuilder` `isClustered()` `NoSuchMethodError` bug
    /// (keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md).
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

    #[test]
    fn unloaded_slots_are_tombstoned_and_never_reused() {
        let mut store = ClassStore::new();
        store.add(make_class(
            ClassId::new(0),
            "dead/C",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        store.add(make_class(
            ClassId::new(1),
            "live/C",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        assert!(store.remove(ClassId::new(0)).is_some());
        assert!(store.get(ClassId::new(0)).is_none());
        assert_eq!(store.len(), 1);
        assert_eq!(store.next_id(), ClassId::new(2));

        let new_id = store.add(make_class(
            ClassId::new(2),
            "new/C",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        assert_eq!(new_id, ClassId::new(2));
        assert!(store.get(ClassId::new(0)).is_none());
    }

    // -------------------------------------------------------------------
    // The direct-implementor adjacency index and `subtypes_of`
    // (`NOTES-cha.md` C4). These live here rather than beside the
    // `unique_implementor` tests because the index is the store's, and
    // `vm/src/vm/tests.rs` is `#[cfg(all(test, feature = "synthetic-jdk"))]`
    // — a test of the index itself should not need a feature build.
    // -------------------------------------------------------------------

    /// Shorthand for the CHA fixtures below: a class with no fields, no
    /// methods and no layout, named only by its position in a hierarchy.
    fn add_subtype_fixture(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        interfaces: Vec<ClassId>,
    ) -> ClassId {
        let id = store.next_id();
        store.add(make_class(
            id,
            name,
            superclass,
            interfaces,
            vec![],
            vec![],
            0,
            0,
        ));
        id
    }

    /// `is_subclass_of` (display + walk) against the bare walk for every
    /// ordered pair of stored classes.
    fn assert_display_agrees_with_walk(store: &ClassStore, ids: &[ClassId]) {
        for &s in ids {
            let class = store.get(s).expect("stored");
            for &t in ids {
                assert_eq!(
                    class.is_subclass_of(t, store),
                    class.is_subclass_of_walk(t, store),
                    "{s} <: {t}"
                );
            }
        }
    }

    /// The primary display answers class targets in one compare — positive
    /// and negative — and defers interface targets and roots to the walk.
    #[test]
    fn primary_display_answers_class_targets_and_agrees_with_the_walk() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
        let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
        let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
        let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
        let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
        let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![i]);
        let ids = [object, i, j, a, b, c, d];

        assert_eq!(store.primary_display_verdict(c, a), Some(true));
        assert_eq!(store.primary_display_verdict(c, b), Some(true));
        assert_eq!(store.primary_display_verdict(d, a), Some(false));
        assert_eq!(store.primary_display_verdict(a, c), Some(false));
        // Interface targets (implementors keys) and the root walk.
        assert_eq!(store.primary_display_verdict(c, i), None);
        assert_eq!(store.primary_display_verdict(c, j), None);
        assert_eq!(store.primary_display_verdict(c, object), None);
        assert_eq!(store.subtype_display_coverage(), (ids.len(), true));
        assert_display_agrees_with_walk(&store, &ids);
    }

    /// A class named as an interface whose own chain is deep (malformed, or a
    /// fabricated stub) makes the walk reach a class target through an
    /// interface edge; the display must stop answering negatives.
    #[test]
    fn a_deep_interface_key_turns_off_negative_display_answers() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
        let k = add_subtype_fixture(&mut store, "p/K", Some(a), vec![]);
        let e = add_subtype_fixture(&mut store, "p/E", Some(object), vec![k]);
        assert!(store.get(e).unwrap().is_subclass_of(a, &store));
        assert_eq!(store.primary_display_verdict(e, a), None);
        assert!(!store.subtype_display_coverage().1);
        assert_display_agrees_with_walk(&store, &[object, a, k, e]);
        // Dropping the edge restores negative answers.
        store.set_interfaces(e, vec![]);
        assert_eq!(store.primary_display_verdict(e, a), Some(false));
    }

    /// Re-parenting recomputes the class and its descendants; a parent added
    /// after the child named it validates the child; a cycle and a removal
    /// invalidate.
    #[test]
    fn primary_display_follows_reparenting_forward_parents_cycles_and_removal() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
        let x = add_subtype_fixture(&mut store, "p/X", Some(object), vec![]);
        let y = add_subtype_fixture(&mut store, "p/Y", Some(x), vec![]);
        assert_eq!(store.primary_display_verdict(y, a), Some(false));
        store.set_superclass(x, Some(a));
        assert_eq!(store.primary_display_verdict(y, a), Some(true));
        assert_display_agrees_with_walk(&store, &[object, a, x, y]);

        // Forward parent: `p` names an id that does not exist yet.
        let p = add_subtype_fixture(&mut store, "p/P", Some(object), vec![]);
        let future = ClassId::new(store.next_id().as_u32() + 1);
        store.set_superclass(p, Some(future));
        assert_eq!(store.primary_display_verdict(p, object), None);
        let filler = add_subtype_fixture(&mut store, "p/Filler", Some(a), vec![]);
        let q = add_subtype_fixture(&mut store, "p/Q", Some(a), vec![]);
        assert_eq!(q, future);
        assert_eq!(store.primary_display_verdict(p, a), Some(true));
        assert_display_agrees_with_walk(&store, &[object, a, x, y, p, filler, q]);

        // Cycle: a -> y -> x -> a.
        store.set_superclass(a, Some(y));
        assert_eq!(store.primary_display_verdict(y, x), None);
        assert_eq!(store.primary_display_verdict(q, a), None);
        store.set_superclass(a, Some(object));
        assert_eq!(store.primary_display_verdict(y, x), Some(true));

        // Removal invalidates the slot.
        store.remove(y);
        assert_eq!(store.primary_display_verdict(y, x), None);
    }

    /// Interface targets are answered from the secondary-supers closure, and
    /// each of the four edge mutators empties every closure that could see
    /// its change — including a closure that stopped at a forward reference.
    #[test]
    fn secondary_supers_answer_interface_targets_and_follow_every_edge_change() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
        let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
        let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
        let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
        let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
        let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![]);
        let all = [object, i, j, a, b, c, d];

        assert_eq!(store.primary_display_verdict(c, i), None);
        assert_eq!(store.secondary_supers_verdict(c, i), Some(true));
        assert_eq!(store.secondary_supers_verdict(c, j), Some(true));
        assert_eq!(store.secondary_supers_verdict(d, i), Some(false));
        assert_display_agrees_with_walk(&store, &all);

        // `set_interfaces` high in the DAG reaches C's closure two hops down.
        store.set_interfaces(j, vec![]);
        assert_eq!(store.secondary_supers_verdict(c, i), Some(false));
        assert_display_agrees_with_walk(&store, &all);

        // `set_superclass` moves D under B, which implements J.
        assert_eq!(store.secondary_supers_verdict(d, j), Some(false));
        store.set_superclass(d, Some(b));
        assert_eq!(store.secondary_supers_verdict(d, j), Some(true));
        assert_display_agrees_with_walk(&store, &all);

        // `add` of a forward-referenced id grows the closure that stopped there.
        let e = add_subtype_fixture(&mut store, "p/E", Some(object), vec![]);
        let future = store.next_id();
        store.set_interfaces(e, vec![future]);
        assert_eq!(store.secondary_supers_verdict(e, future), Some(true));
        assert_eq!(store.secondary_supers_verdict(e, i), Some(false));
        let f = add_subtype_fixture(&mut store, "p/F", Some(object), vec![i]);
        assert_eq!(f, future);
        assert_eq!(store.secondary_supers_verdict(e, i), Some(true));
        assert_display_agrees_with_walk(&store, &[object, i, j, a, b, c, d, e, f]);

        // `remove` shrinks it again; the removed id stays a direct edge target,
        // which the walk still matches.
        store.remove(f);
        assert_eq!(store.secondary_supers_verdict(e, i), Some(false));
        assert_eq!(store.secondary_supers_verdict(e, f), Some(true));
        assert_display_agrees_with_walk(&store, &[object, i, j, a, b, c, d, e]);
    }

    /// A closure larger than the walk's depth bound could disagree with the
    /// bounded walk, so it is stored empty and the question walks.
    #[test]
    fn a_closure_past_the_walk_bound_is_not_answered() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let ifaces: Vec<ClassId> = (0..MAX_HIERARCHY_DEPTH)
            .map(|k| add_subtype_fixture(&mut store, &format!("p/I{k}"), Some(object), vec![]))
            .collect();
        let wide = add_subtype_fixture(&mut store, "p/Wide", Some(object), ifaces.clone());
        assert_eq!(store.secondary_supers_verdict(wide, ifaces[0]), None);
        assert!(store.get(wide).unwrap().is_subclass_of(ifaces[7], &store));
        let narrow =
            add_subtype_fixture(&mut store, "p/Narrow", Some(object), ifaces[..8].to_vec());
        assert_eq!(
            store.secondary_supers_verdict(narrow, ifaces[3]),
            Some(true)
        );
        assert_eq!(
            store.secondary_supers_verdict(narrow, ifaces[9]),
            Some(false)
        );
        // The lock-free view refuses a too-large closure too.
        assert_eq!(store.published_supers().verdict(wide, ifaces[0]), None);
        assert_eq!(
            store.published_supers().verdict(narrow, ifaces[3]),
            Some(true)
        );
    }

    /// Stage 3: the published view answers with no `&ClassStore` at all, only
    /// for closures a locked question already filled, and every edge mutator
    /// empties what its change can alter — and then moves
    /// `class_definition_epoch`, which the per-thread and JIT subtype memos
    /// are tagged with.
    #[test]
    fn published_supers_answer_without_the_store_and_follow_every_edge_change() {
        let mut store = ClassStore::new();
        if !store.display_enabled {
            return; // switched off by the environment
        }
        let published = store.published_supers();
        let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
        let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
        let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
        let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
        let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
        let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
        let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![]);

        // Nothing is published until a locked question fills it.
        assert_eq!(published.verdict(c, i), None);
        assert_eq!(store.secondary_supers_verdict(c, i), Some(true));
        assert_eq!(published.verdict(c, i), Some(true));
        assert_eq!(published.verdict(c, a), Some(true));
        assert_eq!(published.verdict(c, c), Some(true));
        assert_eq!(published.verdict(c, d), Some(false));
        assert_eq!(published.published_count(), 1);
        // A class id that is not stored is never published.
        assert_eq!(
            store.secondary_supers_verdict(ClassId::new(9_999), object),
            None
        );
        assert_eq!(published.verdict(ClassId::new(9_999), object), None);

        // `set_interfaces` two hops up empties C's closure, then moves the epoch.
        let e0 = crate::class_definition_epoch();
        store.set_interfaces(j, vec![]);
        assert!(crate::class_definition_epoch() > e0);
        assert_eq!(published.verdict(c, i), None);
        assert_eq!(store.secondary_supers_verdict(c, i), Some(false));
        assert_eq!(published.verdict(c, i), Some(false));

        // `set_superclass` below it.
        assert_eq!(store.secondary_supers_verdict(d, a), Some(false));
        let e1 = crate::class_definition_epoch();
        store.set_superclass(d, Some(b));
        assert!(crate::class_definition_epoch() > e1);
        assert_eq!(published.verdict(d, a), None);
        assert_eq!(store.secondary_supers_verdict(d, a), Some(true));
        assert_eq!(published.verdict(d, a), Some(true));

        // `remove` empties the removed class's own slot (nothing reaches C);
        // a closure that never passed through it stays published.
        let e2 = crate::class_definition_epoch();
        store.remove(c);
        assert!(crate::class_definition_epoch() > e2);
        assert_eq!(published.verdict(c, a), None);
        assert_eq!(published.verdict(d, a), Some(true));
        // Removing an ancestor empties its dependants' closures.
        store.remove(b);
        assert_eq!(published.verdict(d, a), None);
        assert_eq!(store.secondary_supers_verdict(d, a), Some(false));

        // Every published answer equals the walk.
        for &s in &[object, i, j, a, d] {
            let class = store.get(s).expect("stored");
            for &t in &[object, i, j, a, b, c, d] {
                let _ = store.secondary_supers_verdict(s, t);
                if let Some(v) = published.verdict(s, t) {
                    assert_eq!(v, class.is_subclass_of_walk(t, &store), "{s} <: {t}");
                }
            }
        }
    }

    /// The arena: a closure never straddles two chunks, the directory bounds
    /// are refusals rather than panics, a full arena stops publishing, and a
    /// racing second fill keeps the first.
    #[test]
    fn published_supers_arena_packs_bounds_and_first_fill_wins() {
        let p = PublishedSupers::new();
        // Straddle: two words left in chunk 0, a four-word closure moves on.
        *p.cursor.lock() = SUPERS_ARENA_CHUNK - 2;
        let enc = p.publish(5, &[1, 2, 3, 5]).expect("room in chunk 1");
        assert_eq!((enc >> 16) as usize, SUPERS_ARENA_CHUNK);
        assert_eq!(p.answer(enc, 3), Some(true));
        assert_eq!(p.answer(enc, 4), Some(false));
        assert_eq!(p.verdict(ClassId::new(5), ClassId::new(5)), Some(true));
        // First fill wins; the loser is handed the winner's value.
        assert_eq!(p.publish(5, &[5, 9]), Some(enc));
        assert_eq!(p.verdict(ClassId::new(5), ClassId::new(9)), Some(false));
        assert_eq!(p.published_count(), 1);
        // Too large: remembered, never answered.
        assert_eq!(p.publish(6, &[]), Some(SUPERS_TOO_LARGE));
        assert_eq!(p.verdict(ClassId::new(6), ClassId::new(6)), None);
        // Past the slot directory: refused.
        assert_eq!(p.publish(u32::MAX - 1, &[1]), None);
        assert_eq!(p.verdict(ClassId::new(u32::MAX - 1), ClassId::new(1)), None);
        // A full arena publishes nothing new.
        assert!(p.can_publish(7));
        *p.cursor.lock() = SUPERS_ARENA_CHUNK * SUPERS_ARENA_CHUNKS - 1;
        assert_eq!(p.publish(7, &[1, 7]), None);
        assert_eq!(p.verdict(ClassId::new(7), ClassId::new(7)), None);
        assert!(
            !p.can_publish(8),
            "a full arena stops the callers computing closures"
        );
        // Forgetting keeps the count exact.
        p.forget(5);
        p.forget(6);
        p.forget(5);
        assert_eq!(p.published_count(), 0);
        assert_eq!(p.verdict(ClassId::new(5), ClassId::new(5)), None);
    }

    /// The base case: a class naming an interface in its `interfaces` list is
    /// a subtype of that interface.
    ///
    /// The control matters as much as the assertion. `descendants_of` must
    /// still answer "nothing" for the same interface, because it walks the
    /// SUPERCLASS index alone and its caller
    /// (`ClassManager::recompute_subclass_layouts`) consumes its result as a
    /// topological order of that one relation. The two walks are separate
    /// methods for exactly this reason.
    #[test]
    fn subtypes_of_sees_a_direct_implementor_and_descendants_of_does_not() {
        let mut store = ClassStore::new();
        let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
        let implementor = add_subtype_fixture(&mut store, "cha/Impl", None, vec![iface]);

        assert_eq!(store.subtypes_of(iface, 64), Some(vec![implementor]));
        assert!(
            store.descendants_of(iface).is_empty(),
            "descendants_of stays superclass-only: widening it would hand \
             recompute_subclass_layouts entries in the wrong order"
        );
        // Nothing is its own subtype, in either direction.
        assert_eq!(store.subtypes_of(implementor, 64), Some(vec![]));
    }

    /// A subclass of an implementor is an implementor too, which is why the
    /// two relations have to be walked TOGETHER rather than one after the
    /// other: reaching `Sub` from `Iface` needs an interface edge followed by
    /// a superclass edge.
    #[test]
    fn subtypes_of_follows_a_class_edge_below_an_implementor() {
        let mut store = ClassStore::new();
        let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
        let implementor = add_subtype_fixture(&mut store, "cha/Impl", None, vec![iface]);
        let sub = add_subtype_fixture(&mut store, "cha/Sub", Some(implementor), vec![]);
        let deeper = add_subtype_fixture(&mut store, "cha/Deeper", Some(sub), vec![]);

        assert_eq!(
            store.subtypes_of(iface, 64),
            Some(vec![implementor, sub, deeper]),
            "an implementor's whole subclass tree is below the interface"
        );
        // And the class half still answers the class question on its own.
        assert_eq!(store.descendants_of(implementor), vec![sub, deeper]);
    }

    /// `interface Derived extends Base` is an edge like any other — it is
    /// recorded in `Class::interfaces` exactly as `implements` is — so a class
    /// implementing `Derived` is a subtype of `Base` two hops away.
    #[test]
    fn subtypes_of_follows_an_interface_extending_an_interface() {
        let mut store = ClassStore::new();
        let base = add_subtype_fixture(&mut store, "cha/Base", None, vec![]);
        let derived = add_subtype_fixture(&mut store, "cha/Derived", None, vec![base]);
        let concrete = add_subtype_fixture(&mut store, "cha/Concrete", None, vec![derived]);

        assert_eq!(store.subtypes_of(base, 64), Some(vec![derived, concrete]));
        assert_eq!(store.subtypes_of(derived, 64), Some(vec![concrete]));
    }

    /// A diamond reaches the same class twice — once directly, once through
    /// the sub-interface — and it must be reported once. A duplicate would
    /// both waste a slot against the caller's cap and make
    /// `unique_implementor` read "two providers" for a hierarchy that has one.
    #[test]
    fn subtypes_of_reports_a_diamond_implementor_once() {
        let mut store = ClassStore::new();
        let base = add_subtype_fixture(&mut store, "cha/Base", None, vec![]);
        let derived = add_subtype_fixture(&mut store, "cha/Derived", None, vec![base]);
        // Names BOTH, which a real class file routinely does.
        let concrete = add_subtype_fixture(&mut store, "cha/Concrete", None, vec![base, derived]);

        assert_eq!(store.subtypes_of(base, 64), Some(vec![derived, concrete]));
    }

    /// `Class::interfaces` may only be mutated by [`ClassStore::set_interfaces`]
    /// (and read everywhere else).
    ///
    /// Nothing in the type system stops a caller from mutating
    /// `Class::interfaces` through a `get_mut` and leaving the store's mirrors
    /// stale — the field is `pub`, and six production sites did exactly that
    /// until the CHA lane routed them through [`ClassStore::set_interfaces`].
    /// Three whole-VM answers depend on the mirror being exact:
    ///
    ///  * class-hierarchy analysis (`subtypes_of`, `unique_implementor`) — a
    ///    missed edge silently loses a subtype, so CHA quietly binds nothing;
    ///  * the primary display's NEGATIVE answers
    ///    ([`ClassStore::primary_display_verdict`]), whose proof needs every
    ///    interface edge to be an `implementors` key;
    ///  * the secondary-supers closures
    ///    ([`ClassStore::secondary_supers_verdict`]), which are invalidated by
    ///    walking the same index backwards.
    ///
    /// A stale mirror does not crash and does not fail at the point of the
    /// mistake; the display's `debug_assert_eq!` against the walk catches it
    /// only if some debug test happens to ask about that edge. That is the
    /// shape of defect a source scan is for.
    ///
    /// The scan is [`raw_edge_field_mutations`], shared with the `superclass`
    /// gate below: every workspace crate's `src` and `tests` directories,
    /// INCLUDING test modules (a fixture that pushes an edge raw is exactly
    /// what the display cannot tolerate), every mutation spelling. The one
    /// legitimate writer is the `mem::replace` in
    /// [`ClassStore::set_interfaces`], exempted by its exact text.
    #[test]
    fn no_raw_write_to_a_stored_class_interfaces_bypasses_set_interfaces() {
        // Assembled so this scanner does not match its own source.
        let field = format!(".{}", "interfaces");
        let the_writer = format!("let old = std::mem::replace(&mut class{field}, new_interfaces);");
        let (writer_seen, offenders) = raw_edge_field_mutations("interfaces", &the_writer, &[]);
        // The scanner must still recognise the one writer it exempts, or a
        // rewrite of `set_interfaces` has turned it blind to that spelling.
        assert!(
            writer_seen,
            "the scan no longer sees `ClassStore::set_interfaces`' own write — \
             update `the_writer` together with that function"
        );
        assert!(
            offenders.is_empty(),
            "`Class::interfaces` mutated directly, bypassing \
             `ClassStore::set_interfaces` and desynchronising the implementor \
             index (class-hierarchy analysis) and the subtype display. Call \
             `ClassManager::set_interfaces` / `ClassStore::set_interfaces` \
             instead. If the receiver is not a stored `Class`, say so on the \
             line with a `// gate-ok(not-a-Class): <reason>` comment:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// `Class::superclass` may only be written by [`ClassStore::set_superclass`].
    ///
    /// The store keeps a direct-subclass adjacency index that
    /// `recompute_subclass_layouts` walks, the primary display, and the
    /// secondary-supers closures (published lock-free, [`PublishedSupers`]),
    /// all maintained by that one mutator. A raw write through `get_mut`
    /// compiles, runs, and looks right — and silently removes the class from
    /// the layout walk (its fields then overlap the parent's on the next
    /// superclass growth) and leaves a published closure answering the OLD
    /// hierarchy without any lock. There is no failure at the point of the
    /// mistake, which is exactly why this is a gate and not a comment.
    ///
    /// Moved here from `class_manager.rs` (round i1 wave 5) onto the matcher
    /// the `interfaces` gate uses: the old scan read `src` only and matched the
    /// one spelling `.superclass = `, so `Option::take` / `replace` /
    /// `insert` / `get_or_insert*`, `&mut <x>.superclass` (`mem::replace` /
    /// `mem::swap`), a chain split across lines by `rustfmt`, and any write in
    /// a `tests` directory all went unseen. `native-builtins/src/cds.rs` stays
    /// exempt as a whole file, as before: its `superclass` is an archive
    /// record's superclass NAME (`String`), with no index behind it. Anything
    /// else that is not a `Class` opts a line out with a
    /// `gate-ok(not-a-Class): <reason>` comment on it or the line above.
    #[test]
    fn superclass_is_only_ever_written_through_set_superclass() {
        // Assembled so this scanner does not match its own source.
        let field = format!(".{}", "superclass");
        let the_writer = format!("class{field} = new_super;");
        let (writer_seen, offenders) =
            raw_edge_field_mutations("superclass", &the_writer, &["native-builtins/src/cds.rs"]);
        assert!(
            writer_seen,
            "the scan no longer sees `ClassStore::set_superclass`' own write — \
             update `the_writer` together with that function"
        );
        assert!(
            offenders.is_empty(),
            "`Class::superclass` written directly, bypassing \
             `ClassStore::set_superclass` and desynchronising the \
             direct-subclass index that `recompute_subclass_layouts` walks and \
             the subtype display. Call `ClassManager::set_superclass` / \
             `ClassStore::set_superclass` instead. If the receiver is not a \
             `Class` at all — a different struct that happens to have a \
             `superclass` field — say so on the line with a \
             `// gate-ok(not-a-Class): <reason>` comment:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// The workspace source scan behind the two edge gates above: every
    /// mutation of `.<field_name>` in every workspace crate's `src` and
    /// `tests` directories, as `(writer_seen, offenders)`.
    ///
    /// It matches every mutation spelling — assignment (also `[i] = `), the
    /// mutating `Vec` and `Option` methods, and `&mut <x>.<field>` (which
    /// covers `mem::take` / `mem::replace` / `mem::swap`) — across line
    /// breaks, since `rustfmt` splits a long `get_mut(..).expect(..).<field>
    /// .push(..)` chain one call per line. String literals and comments are
    /// ignored.
    ///
    /// `the_writer` is the one legitimate writer, exempted by its exact
    /// (trimmed) text in `classloading/src/class.rs`; `writer_seen` says the
    /// scan still recognised it. `exempt_files` are path suffixes skipped
    /// whole. A line whose receiver is not a stored `Class` opts out with a
    /// `gate-ok(not-a-Class): <reason>` comment on it or on the line directly
    /// above (the reason is mandatory).
    fn raw_edge_field_mutations(
        field_name: &str,
        the_writer: &str,
        exempt_files: &[&str],
    ) -> (bool, Vec<String>) {
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        /// The line with string-literal contents blanked and any `//`
        /// comment cut. Per line: a string spanning lines is scanned as code
        /// on its inner lines, which can only over-report.
        fn code_of(line: &str) -> String {
            // Char literals that would otherwise open a string.
            let line = line.replace("'\"'", "' '").replace("'\\\"'", "' '");
            let mut out = String::with_capacity(line.len());
            let mut in_str = false;
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if in_str {
                    if c == '\\' {
                        chars.next();
                    } else if c == '"' {
                        in_str = false;
                        out.push('"');
                    }
                    continue;
                }
                if c == '"' {
                    in_str = true;
                    out.push('"');
                } else if c == '/' && chars.peek() == Some(&'/') {
                    break;
                } else {
                    out.push(c);
                }
            }
            out
        }

        /// Is the `.<field>` occurrence between `before` and `after` a
        /// mutation? The `Vec` mutators serve `interfaces`, the `Option` ones
        /// (`take`, `replace`, `insert`, `get_or_insert*`, `as_mut`,
        /// `as_deref_mut`, `take_if`) serve `superclass`; every name listed
        /// mutates whichever of the two types has it, so one list serves both.
        fn is_mutation(before: &str, after: &str) -> bool {
            const MUTATORS: &[&str] = &[
                "take",
                "take_if",
                "replace",
                "get_or_insert",
                "get_or_insert_with",
                "get_or_insert_default",
                "as_deref_mut",
                "push",
                "pop",
                "insert",
                "remove",
                "swap_remove",
                "retain",
                "retain_mut",
                "clear",
                "truncate",
                "drain",
                "extend",
                "extend_from_slice",
                "extend_from_within",
                "append",
                "splice",
                "split_off",
                "resize",
                "resize_with",
                "dedup",
                "dedup_by",
                "dedup_by_key",
                "sort",
                "sort_by",
                "sort_by_key",
                "sort_unstable",
                "sort_unstable_by",
                "sort_unstable_by_key",
                "reverse",
                "swap",
                "fill",
                "rotate_left",
                "rotate_right",
                "iter_mut",
                "get_mut",
                "first_mut",
                "last_mut",
                "as_mut",
                "as_mut_slice",
                "as_mut_ptr",
            ];
            fn is_assignment(rest: &str) -> bool {
                rest.strip_prefix('=').is_some_and(|r| !r.starts_with('='))
            }
            let rest = after.trim_start();
            if is_assignment(rest) {
                return true;
            }
            if rest.starts_with('[') {
                if let Some(close) = rest.find(']') {
                    if is_assignment(rest[close + 1..].trim_start()) {
                        return true;
                    }
                }
            }
            if let Some(call) = rest.strip_prefix('.') {
                let name: String = call
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if MUTATORS.contains(&name.as_str()) {
                    return true;
                }
            }
            // `&mut <receiver>.interfaces`: strip the receiver expression.
            let head = before
                .trim_end_matches(|c: char| c.is_alphanumeric() || "_.()[]:?".contains(c))
                .trim_end();
            head.ends_with("&mut")
        }

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the classloading crate directory must have a parent")
            .to_path_buf();
        assert!(
            workspace.join("Cargo.toml").is_file(),
            "workspace root not found at {} — failing rather than scanning nothing",
            workspace.display()
        );
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&workspace).expect("workspace root is readable") {
            let crate_dir = entry.expect("readable directory entry").path();
            for sub in ["src", "tests"] {
                let dir = crate_dir.join(sub);
                if dir.is_dir() {
                    collect_rs(&dir, &mut files);
                }
            }
        }
        assert!(
            files.len() > 100,
            "only {} source files found under {} — the directory walk is broken",
            files.len(),
            workspace.display()
        );

        let field = format!(".{field_name}");
        let marker = "gate-ok(not-a-Class):";

        let mut writer_seen = false;
        let mut offenders: Vec<String> = Vec::new();
        for file in &files {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            if exempt_files.iter().any(|e| rel.ends_with(e)) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let code: Vec<String> = lines.iter().map(|l| code_of(l)).collect();
            let joined = code.join("\n");
            for (pos, _) in joined.match_indices(field.as_str()) {
                let after = &joined[pos + field.len()..];
                // `.interfaces_foo` is another identifier.
                if after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                if !is_mutation(&joined[..pos], after) {
                    continue;
                }
                let n = joined[..pos].matches('\n').count();
                if rel.ends_with("classloading/src/class.rs") && code[n].trim() == the_writer {
                    writer_seen = true;
                    continue;
                }
                let excused = [Some(lines[n]), n.checked_sub(1).map(|p| lines[p])]
                    .into_iter()
                    .flatten()
                    .any(|l| {
                        l.split(marker)
                            .nth(1)
                            .is_some_and(|reason| !reason.trim().is_empty())
                    });
                if excused {
                    continue;
                }
                offenders.push(format!("{rel}:{}: {}", n + 1, lines[n].trim()));
            }
        }
        (writer_seen, offenders)
    }

    /// The deferred-resolution shape: several production paths register a
    /// class with an EMPTY interface list (loading an interface can recursively
    /// demand the very class being minted) and fill it in afterwards, and the
    /// synthetic-stub upgrade replaces the list wholesale. `set_interfaces` is
    /// the only call that keeps the index in step with either.
    #[test]
    fn set_interfaces_creates_and_destroys_implementor_edges() {
        let mut store = ClassStore::new();
        let first = add_subtype_fixture(&mut store, "cha/First", None, vec![]);
        let second = add_subtype_fixture(&mut store, "cha/Second", None, vec![]);
        let impl_id = add_subtype_fixture(&mut store, "cha/Impl", None, vec![]);

        assert_eq!(store.subtypes_of(first, 64), Some(vec![]));

        store.set_interfaces(impl_id, vec![first]);
        assert_eq!(store.subtypes_of(first, 64), Some(vec![impl_id]));
        assert_eq!(
            store.get(impl_id).map(|c| c.interfaces.clone()),
            Some(vec![first]),
            "the field and the index are written together or not at all"
        );

        // The upgrade case: the real bytecode names a different interface.
        store.set_interfaces(impl_id, vec![second]);
        assert_eq!(
            store.subtypes_of(first, 64),
            Some(vec![]),
            "the old edge must be withdrawn, not merely shadowed"
        );
        assert_eq!(store.subtypes_of(second, 64), Some(vec![impl_id]));

        // A list naming the same interface twice must still produce one edge.
        store.set_interfaces(impl_id, vec![second, second]);
        assert_eq!(store.subtypes_of(second, 64), Some(vec![impl_id]));
    }

    /// Unloading has to clear BOTH directions, or `subtypes_of` reports
    /// tombstones as subtypes: they survive `get` as `None` — so
    /// `unique_implementor` skips them — but they still count against its cap,
    /// which turns an answerable hierarchy into a refusal.
    #[test]
    fn remove_leaves_no_dangling_interface_edges() {
        let mut store = ClassStore::new();
        let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
        let one = add_subtype_fixture(&mut store, "cha/One", None, vec![iface]);
        let two = add_subtype_fixture(&mut store, "cha/Two", None, vec![iface]);
        assert_eq!(store.subtypes_of(iface, 64), Some(vec![one, two]));

        // Up-edge: the unloaded implementor is no longer under its interface.
        assert!(store.remove(one).is_some());
        assert_eq!(store.subtypes_of(iface, 64), Some(vec![two]));

        // Down-edge: the unloaded interface's own implementor list goes too.
        assert!(store.remove(iface).is_some());
        assert_eq!(
            store.subtypes_of(iface, 64),
            Some(vec![]),
            "a tombstoned interface must not keep answering for its implementors"
        );
    }

    /// Over the cap the walk answers `None` — "cannot answer" — and never a
    /// truncated list. The caller concludes "exactly one provider" from a
    /// `Some`, so a silently short list is the one way this index can produce
    /// a wrong CHA answer rather than merely a missed one.
    #[test]
    fn subtypes_of_reports_the_cap_as_cannot_answer() {
        let mut store = ClassStore::new();
        let iface = add_subtype_fixture(&mut store, "cha/Wide", None, vec![]);
        for i in 0..5 {
            add_subtype_fixture(&mut store, &format!("cha/Impl{i}"), None, vec![iface]);
        }

        assert_eq!(store.subtypes_of(iface, 4), None);
        assert_eq!(
            store.subtypes_of(iface, 5).map(|v| v.len()),
            Some(5),
            "exactly at the cap is still an answer"
        );
        // The cap is on the COMBINED walk: three implementors plus two
        // subclasses of one of them is five subtypes, not two indices of three.
        let mut store = ClassStore::new();
        let iface = add_subtype_fixture(&mut store, "cha/Mixed", None, vec![]);
        let impl_id = add_subtype_fixture(&mut store, "cha/MixedImpl", None, vec![iface]);
        add_subtype_fixture(&mut store, "cha/MixedSubA", Some(impl_id), vec![]);
        add_subtype_fixture(&mut store, "cha/MixedSubB", Some(impl_id), vec![]);
        assert_eq!(store.subtypes_of(iface, 2), None);
        assert_eq!(store.subtypes_of(iface, 3).map(|v| v.len()), Some(3));
    }

    /// gcd d1/c (`gengc-r5w4-conc8-class-store-remove-unregisters-another-domains-layout`):
    /// two stores share class id N; the first owns N's layout slot. The second
    /// store removing its own class N must leave the first store's layout
    /// registered. (Staged at a high id so no other test's store is likely to
    /// have claimed it; if one did, there is nothing to stage and the test
    /// says so by returning.)
    #[test]
    fn gcd_d1c_removing_a_class_leaves_another_stores_layout_at_its_id() {
        use cratonvm_types::{class_layout, register_class_layout, CompactLayout, FieldStorageKind};
        const N: u32 = 700;
        let owner = ClassStore::new();
        let mut other = ClassStore::new();
        let layout = std::sync::Arc::new(CompactLayout {
            field_disps: vec![8],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_disps: vec![8],
            total_size: 16,
        });
        register_class_layout(owner.layout_domain(), N, std::sync::Arc::clone(&layout));
        let ours = || class_layout(N).is_some_and(|l| std::sync::Arc::ptr_eq(&l, &layout));
        if !ours() {
            eprintln!("slot {N} already belongs to another store in this binary; nothing to stage");
            return;
        }
        for i in 0..=N {
            other.add(make_class(
                ClassId::new(i),
                "gcd/Shared",
                None,
                vec![],
                vec![],
                vec![],
                0,
                0,
            ));
        }
        assert!(ours(), "the second store's registration at N was refused");
        assert!(owner.owns_layout_slot(N));
        assert!(!other.owns_layout_slot(N));
        assert!(other.remove(ClassId::new(N)).is_some());
        assert!(ours(), "removing the second store's class N cleared the first store's layout");
        cratonvm_types::unregister_class_layout(N);
        assert!(class_layout(N).is_none());
    }
}
