#[allow(unused_imports)]
use super::*;

/// Where one accepted registration came from, index-parallel with
/// `registrations` / `categories`.
///
/// `&'static Location` is two words and is produced by the compiler from the
/// `#[track_caller]` shim on `register()` — no allocation, no `format!`, no
/// string at all until [`NativeMethodRegistry::census`] asks. That matters
/// because boot runs ~3,100 registrations and the census is requested by
/// roughly nobody: the same reasoning that moved the native-ring name map to
/// `name_index` + `flush_native_ring_names` (see that field doc).
pub(in crate::registry) struct RegistrationProvenance {
    /// `file()`/`line()` of the `register(...)` call. Formatted only in
    /// `census()`.
    pub(in crate::registry) site: &'static core::panic::Location<'static>,
    /// Kind of the slot this registration displaced, captured BEFORE the
    /// in-place slot update rewrites it. That history is otherwise
    /// unrecoverable — `register()` is documented last-registration-wins and
    /// the slot keeps no predecessor — and the Real-JDK plan needs it to tell
    /// "this triple was always a bridge" from "a stub was promoted to a bridge
    /// after the fact" when auditing the census.
    pub(in crate::registry) overwrote: Option<NativeKind>,
}

/// log2 of [`INVOCATION_SHARDS`].
pub(in crate::registry) const INVOCATION_SHARD_BITS: u32 = 3;

/// How many per-thread shards `NativeMethodRegistry::slot_invocations` has.
/// Each shard costs 8 bytes per slot, about 100 KiB at the ~12,000 slots a
/// full boot registers.
pub(in crate::registry) const INVOCATION_SHARDS: usize = 1 << INVOCATION_SHARD_BITS;

thread_local! {
    /// This thread's invocation-counter shard, or `u8::MAX` until first use.
    /// A plain byte with no destructor, so reading it never fails, even while
    /// the thread is being torn down.
    pub(in crate::registry) static INVOCATION_SHARD: std::cell::Cell<u8> = const { std::cell::Cell::new(u8::MAX) };
}

/// The calling thread's shard of the invocation counters, always
/// `< INVOCATION_SHARDS`.
///
/// It is derived from the address of the thread's own `INVOCATION_SHARD`
/// cell. That address is distinct for every live thread, and reading it needs
/// no `std::thread::current()`, which can panic during thread teardown. It is
/// a multiplicative (Fibonacci) hash, and the top bits are kept, so threads
/// whose TLS blocks differ only in high address bits (stacks a fixed
/// power-of-two apart) still spread across the shards. A collision costs only
/// shared-line traffic, never a lost count. This mixes no randomness into
/// anything observable.
#[inline]
pub(in crate::registry) fn invocation_shard() -> usize {
    INVOCATION_SHARD
        .try_with(|cell| {
            let cached = cell.get();
            if cached != u8::MAX {
                return usize::from(cached);
            }
            let addr = cell as *const std::cell::Cell<u8> as usize as u64;
            let mixed = (addr ^ (addr >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let shard = (mixed >> (64 - INVOCATION_SHARD_BITS)) as u8;
            cell.set(shard);
            usize::from(shard)
        })
        .unwrap_or(0)
}

/// One shard of a [`ShardedCounter`], alone on its cache line pair (the
/// adjacent-line prefetcher pulls lines in pairs).
#[repr(align(128))]
#[derive(Debug, Default)]
pub(in crate::registry) struct CounterShard(pub(in crate::registry) std::sync::atomic::AtomicU64);

/// gci i20/jt (`gci-i19jt-jni-calls-serialise-on-the-barrier-lock-FIXED-20261017`
/// item 3): a counter bumped on a hot path by many threads, sharded like the
/// slot invocation counters ([`invocation_shard`]) so the threads' `fetch_add`s
/// do not fight over one cache line. Read by summing the shards: exact once
/// the counting threads are quiescent, a relaxed snapshot while they run --
/// what a single relaxed `AtomicU64` gave.
///
/// Shaped like the `AtomicU64` it replaces (`fetch_add` / `load`), so a field
/// changes type without touching its call sites. For the VM's per-call JNI
/// census (`NativeRealm::jni_bridge_invocations`), which every JNI native
/// dispatch bumped on one VM-wide line.
#[derive(Debug, Default)]
pub struct ShardedCounter {
    pub(in crate::registry) shards: [CounterShard; INVOCATION_SHARDS],
}

impl ShardedCounter {
    /// A counter at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `n` on the calling thread's shard. Returns that SHARD's previous
    /// value (not the total; no caller reads it).
    #[inline]
    pub fn fetch_add(&self, n: u64, order: std::sync::atomic::Ordering) -> u64 {
        self.shards[invocation_shard()].0.fetch_add(n, order)
    }

    /// The sum over every shard (each read with `order`), saturating.
    pub fn load(&self, order: std::sync::atomic::Ordering) -> u64 {
        self.shards
            .iter()
            .fold(0u64, |sum, shard| sum.saturating_add(shard.0.load(order)))
    }
}

/// The generic-signature natives `--compatible` forced over the JDK's own
/// concrete bodies (`vm_exec.rs` `check_override`'s GENS-1 rows,
/// `native_override.rs`' `Class` arm), which
/// [`NativeMethodRegistry::compatible_jdk_reifier`] leaves unregistered so
/// those bodies run (interpreter round i2 wave 57, lane L3).
pub const GENERIC_REIFIER_NATIVES: [(&str, &str, &str); 10] = [
    (
        "java/lang/Class",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
    ),
    (
        "java/lang/Class",
        "getGenericInterfaces",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/Class",
        "getGenericSuperclass",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/reflect/Method",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/reflect/Method",
        "getGenericReturnType",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/reflect/Method",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getGenericParameterTypes",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
    ),
    (
        "java/lang/reflect/Field",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/reflect/RecordComponent",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
    ),
];

/// Is `class.method` a declaration-annotation getter of `Class` or a
/// reflection member, any descriptor -- one of the natives
/// [`NativeMethodRegistry::jdk_declaration_annotations`] leaves unregistered
/// so the JDK's bodies (`Class.annotationData`, `Executable` /
/// `Field.declaredAnnotations`, `sharedGetParameterAnnotations`,
/// `Method.getDefaultValue`) parse the class file's bytes? Interpreter round
/// i2 wave 58, lane L3. Not `Class.isAnnotation`, nor
/// `Annotation.annotationType` (a VM-built annotation proxy still needs it),
/// nor the type-variable carrier's rows.
pub fn is_declaration_annotation_native(class_name: &str, method_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/Class"
            | "java/lang/reflect/AccessibleObject"
            | "java/lang/reflect/Executable"
            | "java/lang/reflect/Method"
            | "java/lang/reflect/Constructor"
            | "java/lang/reflect/Field"
            | "java/lang/reflect/Parameter"
    ) && matches!(
        method_name,
        "getAnnotation"
            | "getDeclaredAnnotation"
            | "getAnnotations"
            | "getDeclaredAnnotations"
            | "getAnnotationsByType"
            | "getDeclaredAnnotationsByType"
            | "isAnnotationPresent"
            | "getParameterAnnotations"
            | "getDefaultValue"
    )
}

/// The natives that build the VM's own `AnnotatedType`s, or read them, which
/// [`NativeMethodRegistry::jdk_type_annotations`] leaves unregistered so the
/// JDK's `TypeAnnotationParser` and `AnnotatedTypeFactory` bodies run
/// (interpreter round i2 wave 57, lane L3).
///
/// Not `TypeVariable.getAnnotatedBounds`: it is registered on the interface,
/// for the VM's own type-variable carrier, which a VM that answers the bytes
/// does not build (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`); a JDK
/// `TypeVariableImpl` selects its own body.
pub const TYPE_ANNOTATION_NATIVES: [(&str, &str, &str); 14] = [
    (
        "java/lang/Class",
        "getAnnotatedSuperclass",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/Class",
        "getAnnotatedInterfaces",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotatedReturnType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Parameter",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Field",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotatedOwnerType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl",
        "getAnnotatedActualTypeArguments",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    // The two stand-ins on the JDK's path to a METHOD's type-parameter
    // annotations (`TypeAnnotationParser.parseAllTypeAnnotations` ->
    // `JavaLangAccess.getRawExecutableTypeAnnotations`), which answered no
    // bytes.
    (
        "jdk/internal/reflect/ReflectionFactory",
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
    ),
    (
        "java/lang/reflect/ReflectAccess",
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
    ),
];

/// The class-only questions [`NativeMethodRegistry::register`] asks of every
/// registration, answered once per class name (Phase 5 fast boot,
/// `p5-natreg`).
///
/// Every field is a pure function of the class name and of process-wide
/// constants (the retired-shadow tables and prefixes, `COMPAT_FAMILIES`, the
/// no-image receiver tables, the once-read `CRATONVM_WINDOWS_CHANNEL_LAYER`),
/// never of this registry's mode, alignment rows or ambient category -- those
/// are still read per registration, where they always were. So a memoized
/// answer is the answer the predicate would have given at that call, and the
/// registry built is byte-identical (`native-builtins/tests/registration_pass_bench.rs`
/// diffs the census across the change). What it removes is the cost: under
/// `--compatible` the `COMPAT_FAMILIES` prefix walk (`compat_family_of_class`)
/// alone was ~40% of `register`, and the ~200-prefix retired scan another
/// ~10%, for ~10,000 registrations over ~1,300 classes. The first three
/// fields come from one walk of a combined prefix index
/// (`retired_shadow::class_shadow_facts`), so even the once-per-class
/// computation no longer scans the prefix lists.
#[derive(Clone, Copy)]
pub(in crate::registry) struct ClassFacts {
    /// `retired_shadow::compat_family_of_class` (via `class_shadow_facts`).
    pub(in crate::registry) family: Option<&'static crate::retired_shadow::CompatFamily>,
    /// `retired_shadow::compat_row_of_stub_class` (via `class_shadow_facts`).
    pub(in crate::registry) stub_row: Option<cratonvm_types::HotSpotAlignment>,
    /// `retired_shadow::class_is_under_retired_prefix` (via
    /// `class_shadow_facts`).
    pub(in crate::registry) retired_prefix: bool,
    /// `no_image_receiver::receiver_declared_by_no_supported_image`.
    pub(in crate::registry) no_image: bool,
    /// `native_class_hash` (the registration key's class half and the
    /// `classes_with_natives` prefilter entry).
    pub(in crate::registry) class_hash: (u64, u64),
}

impl ClassFacts {
    pub(in crate::registry) fn of(class_name: &str) -> Self {
        // `compat_family_of_class`, `compat_row_of_stub_class` and
        // `class_is_under_retired_prefix`, from one prefix walk.
        let shadow = crate::retired_shadow::class_shadow_facts(class_name);
        ClassFacts {
            family: shadow.family,
            stub_row: shadow.stub_row,
            retired_prefix: shadow.retired_prefix,
            no_image: crate::no_image_receiver::receiver_declared_by_no_supported_image(class_name),
            class_hash: native_class_hash(class_name),
        }
    }
}

/// The registration log's storage: every `(class, method, descriptor)` triple
/// as three ids into one interned text buffer.
///
/// Boot registers ~12 000 triples over a few hundred distinct class names and
/// descriptors, and the log used to box each of the three strings per row
/// (`Vec<(Box<str>, Box<str>, Box<str>)>`: 48 bytes of row plus three
/// allocations), about 1.5-2 MB of the boot RSS
/// (`docs/feature-designs/real-jdk-mode.md`).
/// Interned, a row is 12 bytes and each distinct string is stored once.
///
/// Dedup is by a 64-bit hash of the string, verified against the stored text;
/// a string whose hash collides with a different one is stored again (never
/// merged), so a collision costs a few bytes and never a wrong name.
#[derive(Default)]
pub(in crate::registry) struct RegistrationLog {
    pub(in crate::registry) text: String,
    /// `(offset, len)` into `text`, per interned id.
    pub(in crate::registry) spans: Vec<(u32, u32)>,
    pub(in crate::registry) by_hash: FxHashMap<u64, u32>,
    pub(in crate::registry) triples: Vec<[u32; 3]>,
}

impl RegistrationLog {
    pub(in crate::registry) fn with_capacity(rows: usize) -> Self {
        Self {
            triples: Vec::with_capacity(rows),
            ..Self::default()
        }
    }

    pub(in crate::registry) fn len(&self) -> usize {
        self.triples.len()
    }

    pub(in crate::registry) fn name(&self, id: u32) -> &str {
        let (off, len) = self.spans[id as usize];
        &self.text[off as usize..(off + len) as usize]
    }

    pub(in crate::registry) fn intern(&mut self, s: &str) -> u32 {
        use std::hash::BuildHasher;
        let hash = self.by_hash.hasher().hash_one(s);
        if let Some(&id) = self.by_hash.get(&hash) {
            if self.name(id) == s {
                return id;
            }
        }
        let id = u32::try_from(self.spans.len()).expect("registration log: id overflow");
        let off = u32::try_from(self.text.len()).expect("registration log: text overflow");
        self.text.push_str(s);
        self.spans.push((off, s.len() as u32));
        self.by_hash.entry(hash).or_insert(id);
        id
    }

    pub(in crate::registry) fn push(&mut self, class: &str, method: &str, descriptor: &str) {
        let row = [
            self.intern(class),
            self.intern(method),
            self.intern(descriptor),
        ];
        self.triples.push(row);
    }

    pub(in crate::registry) fn get(&self, index: usize) -> Option<(&str, &str, &str)> {
        let [c, m, d] = *self.triples.get(index)?;
        Some((self.name(c), self.name(m), self.name(d)))
    }

    pub(in crate::registry) fn iter(&self) -> impl Iterator<Item = (&str, &str, &str)> + '_ {
        self.triples
            .iter()
            .map(|&[c, m, d]| (self.name(c), self.name(m), self.name(d)))
    }
}

pub struct NativeMethodRegistry {
    /// Dense slot table. `slots[i]` is the resolved native for
    /// `NativeMethodId(i)`. Append-only: entries are updated in place on
    /// re-registration and never removed, which is what makes handles stable.
    pub(in crate::registry) slots: Vec<NativeSlot>,
    /// Per-slot dispatch counter, **index-parallel with `slots`** and grown in
    /// the single `None` arm of `register()` that pushes a slot.
    ///
    /// Deliberately NOT a field on [`NativeSlot`]. `NativeSlot` is
    /// `#[derive(Copy, Clone)]` and is copied by value out of the table on the
    /// `find` / `find_with_kind` hot path (`slot_for_exact` hands back a `&`,
    /// both callers immediately destructure `slot.callback`/`slot.kind`); an
    /// `AtomicU64` is not `Copy`, so putting the counter there would force
    /// every dispatch lookup to carry the registry borrow's lifetime instead of
    /// two plain machine words. A parallel `Vec` keeps the counter off the
    /// dispatch struct entirely — the counter is touched only by
    /// [`record_invocation`](Self::record_invocation), which is a separate
    /// bounds-checked index.
    ///
    /// A superseded registration does not get its own counter: re-registering a
    /// triple updates the slot in place (that is what keeps handles valid), so
    /// the accumulated count carries over to the new owner. See
    /// [`invocations_of_kind`](Self::invocations_of_kind).
    ///
    /// **Sharded by thread** (JIT round 11 wave 12, lane rt; page
    /// `r11-rt-helper-hot-paths-repeat-work-per-call` item 3). There are
    /// [`INVOCATION_SHARDS`] parallel vectors, and every one is index-parallel
    /// with `slots`. A dispatch bumps only its own thread's shard
    /// ([`invocation_shard`]), and a reader sums the shards
    /// ([`invocation_total`](Self::invocation_total)). With a single vector,
    /// the natives every thread calls (`AtomicInteger.get`, `ArrayList.get`)
    /// made all threads' `fetch_add`s fight over one cache line per slot. A
    /// shard is its own allocation, so the same slot in two shards never shares
    /// a line. The sum is exact: every dispatch is still one `fetch_add` on
    /// exactly one counter.
    pub(in crate::registry) slot_invocations: [Vec<std::sync::atomic::AtomicU64>; INVOCATION_SHARDS],
    /// Per-slot "this counter is a **lower bound**" flag, index-parallel with
    /// `slots` and grown in the same single arm as `slot_invocations`.
    ///
    /// # Why the census needs a second bit per slot
    ///
    /// [`record_invocation`](Self::record_invocation) is exact for every
    /// dispatch that reaches it. What it cannot see is a dispatch that never
    /// consults the registry at all, because some caller resolved this
    /// triple's callback ONCE and then called the resulting function pointer
    /// directly. Two such families were measured on 2026-08-17
    /// (`docs/feature-designs/native-dispatch-architecture.md`):
    /// the interpreter's intrinsic table and the JIT's thin direct-call
    /// helpers. Both are deliberate optimisations, both are correct, and both
    /// silently removed this counter along with the name resolution it was
    /// attached to.
    ///
    /// Rather than pay a `fetch_add` on those paths — measured at **+9.2 ns
    /// per call**, which is the whole margin a thin direct-call helper exists
    /// to buy — a bypassing path sets this bit ONCE, cold, when it binds the
    /// call site. The census then reports `invocations_complete: false` for
    /// that slot, and a reader knows the number is a floor rather than a
    /// count. An instrument that says "at least N" is usable; one that says
    /// "N" and means "at least N" is not.
    ///
    /// `false` (the default) is the *claim*, not the absence of one: it says
    /// nothing bypassed this slot as far as the registry was told. It is only
    /// as true as the bypassing paths are honest about calling
    /// [`mark_invocations_incomplete`](Self::mark_invocations_incomplete) —
    /// which is why that method's doc carries the list of sites that must.
    pub(in crate::registry) slot_invocations_incomplete: Vec<std::sync::atomic::AtomicBool>,
    /// 128-bit `(class, method, descriptor)` digest -> slot index. A hit here
    /// is a *candidate*, not an answer: `slot_index_for_key` re-checks the full
    /// triple before returning the slot.
    pub(in crate::registry) slot_by_key: FxHashMap<(u64, u64), u32>,
    /// [`native_class_hash`] of every class name that has ever been passed to
    /// [`register`](Self::register) — the negative-lookup prefilter for
    /// [`slot_for_exact`](Self::slot_for_exact). See `native_class_hash` for
    /// why this exists and what it is worth.
    ///
    /// Soundness is one-directional and cheap to keep: a class absent from
    /// this set provably has no registration (the single production writer is
    /// `register`, which inserts here in the same statement sequence that
    /// pushes to `registrations`), so answering `None` for it is exact. A hash
    /// collision can only add a FALSE POSITIVE, which falls through to the
    /// full digest + name verification below and is therefore harmless.
    /// Registrations are never removed, so the set never needs to shrink.
    pub(in crate::registry) classes_with_natives: FxHashSet<(u64, u64)>,
    /// Process-unique base for [`generation`](Self::generation).
    ///
    /// Without this, `generation()` would just be `slots.len()`, and two
    /// *different* registries with the same number of registrations would
    /// report the same generation — so a `NativeCallSite` that outlives one
    /// registry (a `static` cell in a test binary that builds several `SharedVm`s,
    /// say) could accept a memo taken against a different registry and redeem a
    /// slot index that means something else. Banding each registry into its own
    /// `REGISTRY_EPOCH_STRIDE`-wide range makes that a re-resolve instead of a
    /// wrong answer.
    pub(in crate::registry) registry_epoch: u32,
    pub(in crate::registry) mutation_epoch: u32,
    /// Set once this VM has loaded Netty's real `netty_tcnative` library and run
    /// its `JNI_OnLoad`; from then on every lookup under
    /// `io/netty/internal/tcnative/` misses, so dispatch falls through to the
    /// `RegisterNatives` pointers the library published.
    ///
    /// This lives on the registry rather than at a dispatch site because the
    /// registry is the only choke point all the routes share. `vm_exec`'s
    /// general `is_native` arm, `try_stackless_invoke`'s `resolve_step1_native`,
    /// and `dispatch_static`'s skip-clinit probe each reach the table
    /// independently, and a stub retired at one of them is still live at the
    /// others — which is not a cosmetic difference: it left
    /// `Library.initialize0()Z` (a plain zero-arg static, so the stackless path
    /// claims it) answering `true` without ever calling `apr_initialize`, while
    /// the rest of the package ran against the real library. The first real
    /// `SSLContext.make` then dereferenced the never-created `tcn_global_pool`
    /// and took a SIGSEGV.
    ///
    /// The registry is per-`SharedVm`, so muting is per-VM: a sibling VM that
    /// never loaded the library keeps its stubs.
    ///
    /// Only ever set, never cleared — a `dlclose` cannot un-publish the
    /// function pointers `find_jni_native` already holds.
    pub(in crate::registry) netty_tcnative_muted: std::sync::atomic::AtomicBool,
    /// Append-only registration log: the original `(class, method, descriptor)`
    /// triples, kept as `Box<str>` rather than `String` to minimize per-entry
    /// overhead. This replaces the previous `FxHashMap<u64, String>` reverse map
    /// which was inserted into on every `register()` purely to enable collision
    /// detection.
    ///
    /// Two consumers:
    ///
    ///  1. `alias_class` (a rare, slow-path operation called a handful of times
    ///     at boot to copy interface-method registrations down to subinterface
    ///     class names).
    ///  2. **Full-name verification on every digest hit.** Each `NativeSlot`
    ///     carries the `reg_index` of the triple it was registered under, and
    ///     `slot_index_for_key` compares all three strings before returning the slot.
    ///     Without this, an FNV-1a collision would silently hand back the wrong
    ///     callback — the exact defect the `class_manager::name_to_id` shadow map
    ///     had (see `classloading/src/class_manager.rs`, `loaded_classes` field
    ///     doc, "Round 4 audit fix (CRIT)") before it was deleted.
    ///
    /// Index-parallel with `categories` and `provenance`. Only *accepted*
    /// registrations are pushed — every drop arm in `register()` (including the
    /// Real-JDK refusal) returns before this point.
    pub(in crate::registry) registrations: RegistrationLog,
    /// Registration site + displaced kind, index-parallel with `registrations`
    /// and `categories`. See [`RegistrationProvenance`]; materialized into
    /// strings only by [`census`](Self::census).
    pub(in crate::registry) provenance: Vec<RegistrationProvenance>,
    /// AUDIT 2026-05-17 (Fix 5): O(1) index keyed by the 128-bit hash
    /// of `(method_name, descriptor)` (class portion omitted). Used by
    /// `find_by_method_descriptor` to avoid the O(N) linear scan over
    /// `registrations`. Built incrementally on every `register()`.
    pub(in crate::registry) by_method_desc: FxHashMap<(u64, u64), NativeCallback>,
    /// Memo for [`owner_classes_for_method`](Self::owner_classes_for_method):
    /// `(registrations.len() when built, (method, descriptor) -> owning classes)`.
    /// Interior-mutable because every caller holds the registry by `&self`.
    pub(in crate::registry) method_owner_index: std::sync::RwLock<
        Option<(
            usize,
            std::collections::HashMap<(Box<str>, Box<str>), Vec<Box<str>>>,
        )>,
    >,
    /// Category aligned with `registrations` (index-parallel), for
    /// `dump_registrations` / census output.
    pub(in crate::registry) categories: Vec<NativeKind>,
    /// The category applied to subsequent `register()` calls, or `None` when
    /// no scope is in effect.
    ///
    /// `None` is what makes "nobody chose this" a *scoped* fact rather than a
    /// sticky one. It used to be a plain `NativeKind` initialised to
    /// `SyntheticStub`, with a separate `category_chosen: bool` alongside — and
    /// that boolean was set by `set_category` and **never cleared**, because
    /// the restore half of the ubiquitous
    /// `let prev = registry.current_category(); … registry.set_category(prev);`
    /// idiom put the kind back and left the flag true. So after the first
    /// `set_category` anywhere in boot, every subsequent registration reported
    /// "chosen" whether or not any scope covered it: measured on JDK 25 /
    /// linux, 11,875 of 11,876 registrations, which is not a measurement of
    /// anything.
    ///
    /// Carrying the "chosen" bit *inside* the value fixes that for free. The
    /// idiom already saves and restores; an `Option` makes it restore the
    /// absence too, so a registrar entered with no category leaves none behind.
    /// `set_category` takes `impl Into<Option<NativeKind>>`, so both halves of
    /// the idiom — and all 567 `set_category(NativeKind::…)` sites — compile
    /// unchanged.
    ///
    /// Reads that need a kind rather than a question go through
    /// [`Self::effective_category`], which is the one place the conservative
    /// `SyntheticStub` default now lives.
    /// The leaf claim applied to subsequent `register()` calls. Scoped via
    /// [`Self::set_leaf`] / [`Self::with_leaf`]. Defaults to `false`
    /// (conservative — every native pays the full funnel until it opts out).
    pub(in crate::registry) current_leaf: bool,
    /// Whether `current_category` was **stated at the registration site**
    /// ([`NativeMethodRegistry::register_with_kind`]) rather than inherited
    /// from an ambient `set_category` / `with_category` in an ancestor frame.
    ///
    /// Index-parallel copy lands in `kind_stated`. Only ever `true` for the
    /// duration of one `register_with_kind` call, which sets and restores it
    /// around the inner `register`.
    ///
    /// This is the discriminator the 157-entry reclassification needs: today
    /// the census can say a registration is a `SyntheticStub` and where it was
    /// written, but not whether anyone *decided* that. See
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
    /// `true` only for the duration of the retired-shadow re-tag in
    /// [`Self::register`] (a `Bridge` over a triple in
    /// `crate::retired_shadow`'s tables, being registered as `SyntheticStub`).
    ///
    /// Read by the real-JDK ForkJoin keep arms in `register_inner`, which keep
    /// a `Bridge` and drop everything else on those classes in EVERY real-JDK
    /// mode. Without it a retired `ForkJoinPool`/`ForkJoinTask` triple would
    /// lose its native in `--compatible` too (the re-tag has already made it a
    /// `SyntheticStub` when the keep arm asks), which is not what a §1.4
    /// retirement is for. With it the re-tagged registration is kept, so
    /// `--compatible` still dispatches it and `--real-jdk` drops it as a stub.
    /// SBR wave 4 lane FJP.
    pub(in crate::registry) retagging_retired_shadow: bool,
    /// `true` only inside a
    /// [`NativeMethodRegistry::set_admitting_family_intrinsic`] scope: the
    /// `Compatible` family drop at the top of [`Self::register`] keeps that
    /// one stated `Intrinsic`, as `JdkOnly` keeps it (`resolve_dispatch` step
    /// 2). Interpreter round i5 wave 109, lane L4.
    pub(in crate::registry) admitting_family_intrinsic: bool,
    /// Per-registration copy of [`Self::next_kind_stated`], index-parallel with
    /// `registrations` and `categories`.
    /// Per-registration answer to *"was a category scope in effect for this
    /// call?"* — i.e. `current_category.is_some()` at the moment of the
    /// `register`. Index-parallel with `registrations` and `categories`.
    ///
    /// Load-bearing for the downgrade rule in `register`. A first cut keyed
    /// that rule on `kind_stated` alone and promptly preserved a stated
    /// `Bridge` over a `with_category(Intrinsic)` re-registration of
    /// `java/lang/Float.intBitsToFloat` — a downgrade of exactly the kind the
    /// rule exists to prevent, in the opposite direction. `with_category` is an
    /// opinion; `kind_stated` deliberately does not record it, and this does.
    /// Caught by diffing `--dump-native-registry` across the change; it was the
    /// only kind that moved.
    /// Strict "no synthetic stubs" mode. When true, `register()` DROPS any
    /// registration whose `current_category` is `SyntheticStub` — it is never
    /// inserted, so a call to that method falls through to real JDK bytecode
    /// (if present) or a clear `NoSuchMethodError`/unimplemented diagnostic,
    /// never to a fake. This is the comprehensive "remove synthetic stubs
    /// completely" switch: it makes the entire SyntheticStub bucket vanish from
    /// the build at once. Enabled via the `CRATONVM_NO_STUBS` env var (read
    /// once in `new()`). Off by default — opt-in, because some apps currently
    /// limp on these fakes and dropping them surfaces real gaps as clear
    /// errors. `Intrinsic` and `Bridge` registrations are never affected.
    /// See docs/feature-designs/native-dispatch-architecture.md.
    pub(in crate::registry) drop_synthetic_stubs: bool,
    /// Opt out of the `java/net/Socket` / `ServerSocket` drop below.
    ///
    /// That filter exists because ONE registered native shadows a class's real
    /// bytecode at every dispatch site, so under `CRATONVM_REAL_NET_SOCKETS`
    /// the synthetic socket surface has to be absent from the registry, not
    /// merely un-called. That is right for a running VM and wrong for a
    /// registry built purely to unit-test the synthetic natives themselves:
    /// there is no real socket bytecode in that process to shadow, so the
    /// filter just leaves the tests with nothing to call. Default `false`; only
    /// `cratonvm-vm`'s inline test registry sets it.
    pub(in crate::registry) allow_synthetic_net_sockets: bool,
    /// VM-scoped strict policy (`docs/book/src/user-guide/real-jdk-mode.md` §4,
    /// 2026-07-31). Set once at VM init by
    /// [`set_compatibility_mode`](Self::set_compatibility_mode), before the
    /// `register_*` population pass.
    ///
    /// A **field**, not a process global, on purpose: §2 forbids process
    /// globals for this feature, and this repo has already shipped the failure
    /// it is protecting against — process-global native caches leaking state
    /// across two `SharedVm`s in one test process.
    ///
    /// Relationship to `drop_synthetic_stubs`: `JdkOnly` is a strict superset.
    /// `CRATONVM_NO_STUBS` silently drops the same bucket; `JdkOnly`
    /// additionally records a structured, attributed
    /// `JdkOnlyViolation::SyntheticNativeRegistered` in
    /// [`refused`](Self::refused_registrations). Both arms are live and
    /// independent — the env var keeps working with `Compatible` set.
    ///
    /// In `Compatible` (the default) the only cost anywhere is one field load
    /// and a discriminant compare at the top of `register()`; nothing in
    /// dispatch reads it.
    pub(in crate::registry) compatibility_mode: CompatibilityMode,
    /// Is a REAL JDK class library in use (`--java-home`) rather than the
    /// synthetic one?
    ///
    /// Distinct from [`CompatibilityMode`], which is the strict-vs-compatible
    /// policy — a run can be `Compatible` on either class library. Registrars
    /// consult this to skip a native whose only job is to stand in for a class
    /// the synthetic library fakes. See `native-collections`'
    /// `CompletableFuture` dependent-stage block: over a real JDK those
    /// natives detect the real object and delegate straight back to the method
    /// they shadow, so registering them buys a native dispatch and a by-name
    /// `invoke_special` and nothing else.
    ///
    /// Set **once at VM init, before the `register_*` population pass**, for
    /// the same reason [`Self::set_compatibility_mode`] must be: it gates
    /// registration, so flipping it later leaves whatever was already accepted
    /// in place. Defaults to `false`, so a registry nobody configures behaves
    /// exactly as it did before this field existed.
    pub(in crate::registry) real_jdk: bool,
    /// The feature version of the real JDK image (`java.home/release`'s
    /// `JAVA_VERSION`), when one is in use and its `release` file says. Set
    /// at VM init beside [`Self::real_jdk`], before the `register_*` pass.
    /// A registrar that yields a method to the image's bytecode must not do
    /// so where the image still declares that method `native` (JDK 17/21
    /// declare `Class.getModifiers` / `getSigners` native; JDK 25 gives them
    /// bodies), or the method has no implementation at all under
    /// `--real-jdk`. `None` (unknown or synthetic) answers as the newest.
    pub(in crate::registry) jdk_feature: Option<u32>,
    /// Does this VM run a JIT (`VmConfig::jit_enabled`)? Set at VM init beside
    /// [`Self::real_jdk`], before the `register_*` pass, for registrars whose
    /// `Intrinsic` pays only while the method it covers runs interpreted (a
    /// method backed by a registered native is never compiled, so under the
    /// JIT such an intrinsic turns a compiled body into a native crossing plus
    /// an interpreted re-entry on every miss; interpreter round i3 wave 75,
    /// lane L4: `MethodType.makeImpl`). Defaults to `true`, the conservative
    /// answer: a registry nobody configures registers no such intrinsic.
    pub(in crate::registry) jit_enabled: bool,
    /// Does the JDK drive virtual threads (`VmConfig::jdk_continuations` over a
    /// real JDK 21 or newer)? Set at VM init beside [`Self::real_jdk`], before
    /// the `register_*` pass: the continuation registrar
    /// (`phases_late::concurrent::register_new15_continuation`) then serves the
    /// JDK's `Continuation` natives and stands nothing in front of its bytecode,
    /// and `ContinuationSupport.isSupported0()` answers `true`. Defaults to
    /// `false`: a registry nobody configures keeps the bound shape.
    pub(in crate::registry) jdk_continuations: bool,
    /// `CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES` (default on), read once
    /// at construction like `drop_synthetic_stubs`. Consulted only through
    /// [`Self::retired_row_masks_ancestor_bridges`], which answers `false` in
    /// `Compatible` mode before reading it. Round 14 wave 2, lane shadow.
    pub(in crate::registry) retired_rows_mask_ancestor_bridges: bool,
    /// `CRATONVM_ANCESTOR_WALK_HELPER` (default on), read once at
    /// construction like the field above. See [`Self::ancestor_walk_helper`].
    /// Round 15 wave 2, lane jdkonly (SH14-1).
    pub(in crate::registry) ancestor_walk_helper: bool,
    /// `CRATONVM_ANCESTOR_REDEFINE_RULE` (default on), read once at
    /// construction like the field above. See [`Self::ancestor_redefine_rule`].
    /// Round 15 wave 3, lane walk.
    pub(in crate::registry) ancestor_redefine_rule: bool,
    /// `CRATONVM_ANCESTOR_WALK_STUB_YIELD` (default on), read once at
    /// construction like the field above. See
    /// [`Self::ancestor_walk_stub_yield`]. Round 15 wave 3, lane walk.
    pub(in crate::registry) ancestor_walk_stub_yield: bool,
    /// `CRATONVM_REDEFINE_OWNER_SCOPED_EVICT` (default on), read once at
    /// construction like the field above. See
    /// [`Self::redefine_owner_scoped_eviction`]. Round 15 wave 4, lane walk2.
    pub(in crate::registry) redefine_owner_scoped_eviction: bool,
    /// `CRATONVM_ANCESTOR_REDEFINE_BODYLESS_KEEP` (default on), read once at
    /// construction like the field above. See
    /// [`Self::ancestor_redefine_bodyless_keep`]. Round 15 wave 4, lane walk2.
    pub(in crate::registry) ancestor_redefine_bodyless_keep: bool,
    /// `CRATONVM_JIT_SITE_CACHE_OWNER_REDEFINE` (default ON since round 16,
    /// lane redefine; `=0` is the kill switch), read once at construction like
    /// the field above. See [`Self::jit_site_cache_owner_redefine`]. Round 15
    /// wave 4, lane walk3.
    pub(in crate::registry) jit_site_cache_owner_redefine: bool,
    /// `CRATONVM_REDEFINE_INHERITED_OWNER` (default on), read once at
    /// construction like the field above. See
    /// [`Self::redefine_inherited_owner`]. Round 16, lane redefine.
    pub(in crate::registry) redefine_inherited_owner: bool,
    /// `CRATONVM_JIT_NATIVE_LEAN_DOOR` (default OFF), read once at
    /// construction like the field above. See [`Self::jit_native_lean_door`].
    /// JIT round 17 wave 2, lane funnel.
    pub(in crate::registry) jit_native_lean_door: bool,
    /// `CRATONVM_NATIVE_CLASS_HASH_MEMO` (default ON), read once at
    /// construction: [`Self::class_prefilter`] answers the class-name prefix
    /// hash from the per-thread memo ([`native_class_hash_memoized`]). nio21
    /// wave 17, lane L1.
    pub(in crate::registry) class_hash_memo: bool,
    /// `CRATONVM_DBG_NATIVE_DOOR_SPLIT` (default OFF), read once at
    /// construction. See [`Self::dbg_native_door_split`]. JIT round 17 wave 2,
    /// lane funnel.
    pub(in crate::registry) dbg_native_door_split: bool,
    /// The census [`Self::note_native_door_split`] keeps while
    /// `dbg_native_door_split` is on: `[crossings, lean crossings, front
    /// cycles, funnel cycles, tail cycles]`. Per VM, like the invocation
    /// counters; untouched (all zero) while the switch is off.
    pub(in crate::registry) native_door_split: [std::sync::atomic::AtomicU64; 5],
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL` (default OFF), read once at
    /// construction. See [`Self::jit_native_site_door_lean_funnel`]. JIT round
    /// 19 wave 2, lane funnel2 (FUNNEL17-5 residual).
    pub(in crate::registry) jit_native_site_door_lean_funnel: bool,
    /// `CRATONVM_JIT_NATIVE_WALK_ONCE` (default OFF), read once at
    /// construction. See [`Self::jit_native_walk_once`]. JIT round 19 wave 2,
    /// lane funnel2 (FUNNEL17-2).
    pub(in crate::registry) jit_native_walk_once: bool,
    /// `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS` (default OFF), read once at
    /// construction. See [`Self::dbg_native_site_door_census`]. JIT round 19
    /// wave 2, lane funnel2.
    pub(in crate::registry) dbg_native_site_door_census: bool,
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS` (default OFF), read once at
    /// construction. See [`Self::jit_native_site_door_leaf_kinds`]. JIT round
    /// 19 wave 3, lane funnel3 (FUNNEL2-19-2).
    pub(in crate::registry) jit_native_site_door_leaf_kinds: bool,
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY` (opt-in, default OFF: flipped ON in the round-19 proposal round, back OFF at its close -- after the dev merge the one-way door measured faster on every R19Funnel3Crossing row), read once at
    /// construction. See [`Self::jit_native_site_door_two_way`]. JIT round 19
    /// wave 3, lane funnel3 (FUNNEL2-19-1).
    pub(in crate::registry) jit_native_site_door_two_way: bool,
    /// `CRATONVM_JIT_NATIVE_FRESH_RESULT` (default OFF), read once at
    /// construction. See [`Self::jit_native_fresh_result`]. JIT round 19 wave
    /// 3, lane funnel3 (FUNNEL2-19-3).
    pub(in crate::registry) jit_native_fresh_result: bool,
    /// `CRATONVM_JIT_NATIVE_SITE_REFUSAL_BY_RECEIVER` (default OFF), read once
    /// at construction. See [`Self::jit_native_site_refusal_by_receiver`]. JIT
    /// round 19 wave 6, lane mega6 (FUNNEL3-19-1).
    pub(in crate::registry) jit_native_site_refusal_by_receiver: bool,
    /// `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS` (default OFF), read once at
    /// construction. See [`Self::jit_leaf_probe_validated_args`]. JIT round 19
    /// wave 6, lane mega6 (FUNNEL3-19-2).
    pub(in crate::registry) jit_leaf_probe_validated_args: bool,
    /// The census [`Self::note_native_site_door`] keeps while
    /// `dbg_native_site_door_census` is on, indexed by
    /// [`NativeSiteDoorEvent`]. Per VM; all zero while the switch is off.
    pub(in crate::registry) native_site_door_census: [std::sync::atomic::AtomicU64; NativeSiteDoorEvent::COUNT],
    /// The `--compatible` rows aligned with HotSpot
    /// (`cratonvm_types::hotspot_alignment`), read once at construction
    /// (`HotSpotAlignments::from_env`: the defaults, each row's legacy
    /// variable (`HotSpotAlignment::legacy_key`), then `CRATONVM_COMPAT_ALIGN`) and handed
    /// to the class manager and the class realm by `vm_init`. Asked through
    /// [`Self::aligned`]; three rows are this registry's own:
    ///
    /// * `ParallelLoaders` (`CRATONVM_COMPAT_PARALLEL_LOADERS`): under
    ///   `Compatible` over a real JDK, the JDK's parallel-capable loader model
    ///   is in force, as under `JdkOnly` -- the two shadows that kept
    ///   `ClassLoader$ParallelLoaders` empty (`RETIRED_SHADOW_L7_TRIPLES`:
    ///   `ClassLoader.registerAsParallelCapable()Z`, answering `true` without
    ///   registering, and the no-op `SecureClassLoader.<clinit>`) are not
    ///   registered, so the real bytecode runs, and the loader-lock doors that
    ///   read [`Self::compat_parallel_capable_loaders`] (through
    ///   `NativeContext::faithful_loader_locks`) behave as under `JdkOnly`.
    ///   Interpreter round i1 wave 48, lane L5
    ///   (`interpreter-L5-a-non-parallel-capable-loader-gets-a-per-name-lock-FIXED-20261015`).
    /// * `UrlAppHandler` (`CRATONVM_COMPAT_URL_APP_HANDLER`): under
    ///   `Compatible`, a `java.net.URL` that carries an APPLICATION
    ///   `URLStreamHandler` is served by that handler, as under `JdkOnly` --
    ///   `URL.openStream` yields to the JDK's body and Spring's
    ///   `UrlResource.getInputStream` shadow is not registered. Read through
    ///   [`Self::url_app_handler_is_authoritative`]. Interpreter round i1 wave
    ///   49, lane L6
    ///   (`interpreter-L6-compatible-fat-jar-reads-boot-inf-classes-as-a-nested-jar-FIXED-20261013.md`).
    /// * `ModuleMessages` (`CRATONVM_COMPAT_MODULE_MESSAGES`): under
    ///   `Compatible`, a VM-raised `ClassCastException` names each operand's
    ///   module and loader from the raise site's class ids, a user-defined
    ///   loader by its `nameAndId`, as under `JdkOnly` (HotSpot's
    ///   `SharedRuntime::generate_class_cast_message`). Read through
    ///   [`Self::messages_name_user_loaders`]. Interpreter round i1 wave 49,
    ///   lane L6
    ///   (`interpreter-L6-compatible-cast-and-duplicate-define-messages-omit-module-and-loader-FIXED-20261013.md`).
    pub(in crate::registry) hotspot_alignments: cratonvm_types::HotSpotAlignments,
    /// Is any `retired_shadow::COMPAT_FAMILIES` row in force (and a real JDK in
    /// use)? Kept beside [`Self::hotspot_alignments`] so the dispatch walks that
    /// ask [`Self::retired_row_masks_ancestor_bridges`] pay one field compare
    /// under `Compatible` when no family is adopted. SBR round 4, lane compat.
    pub(in crate::registry) compat_family_in_force: bool,
    /// `CRATONVM_IMAGE_APP_MODULE_LOADERS`, read once at construction: a
    /// runtime-image class of a module the JDK's `ModuleLoaderMap` gives to
    /// the APPLICATION loader (`jdk.compiler`, `jdk.jshell`, ...) reports
    /// that loader from `Class.getClassLoader()`, and every non-boot image
    /// class a `jrt:/<module>` `CodeSource`, as on HotSpot (both modes). Read
    /// through [`Self::image_app_module_loaders`]. Interpreter round i1 wave
    /// 50, lane L5
    /// (`interpreter-L5-runtime-image-module-classes-answer-a-synthetic-code-source-FIXED-20261014`).
    pub(in crate::registry) image_app_module_loaders: bool,
    /// `CRATONVM_LOADER_PER_NAME_LOCK`, read once at construction: the base
    /// `ClassLoader.loadClass` native holds a parallel-capable user loader's
    /// per-name lock (`getClassLoadingLock(name)`) around the load, as the
    /// JDK's own `loadClass` does, wherever the JDK's parallel-capable loader
    /// model is in force ([`Self::compat_parallel_capable_loaders`]). Read
    /// through [`Self::per_name_loader_locks`]. Interpreter round i1 wave 51,
    /// lane L5 (`interpreter-L5-a-non-parallel-capable-loader-gets-a-per-name-lock-FIXED-20261015`).
    pub(in crate::registry) per_name_loader_locks: bool,
    /// `CRATONVM_CL_DELEGATES_TO_URL_PARENT`, read once at construction: the
    /// base `ClassLoader.loadClass` native's parent-first step asks a bare
    /// `java.net.URLClassLoader` parent's own `loadClass` (whose recorded URLs
    /// the global store never answers for), as it asks a user-defined
    /// parent's. Read through [`Self::delegates_to_url_parent`]. Interpreter
    /// round i2 wave 60, lane L5 (probe `L5W60UrlParentDelegation`); default
    /// on since the lane's flip commit (`=0` restores the old rule).
    pub(in crate::registry) delegates_to_url_parent: bool,
    /// `CRATONVM_RESOLVER_ABSENCE_SHAPE`, read once at construction: a served
    /// loader rung reads only the resolver's explicit absence
    /// (`ClassFileError::ClassNotFound` of the name asked for) as "not found";
    /// every other linkage refusal of a class file the resolver FOUND is its
    /// throwable, and an internal error is never absorbed as absence. Read
    /// through [`Self::resolver_absence_shape`]. Interpreter round i2 wave 61,
    /// lane L5 (`i60-L5-proposal-the-resolver-reports-absence-as-its-own-shape`;
    /// probe `L5W61AppLoaderRefusals`); default on since the lane's flip
    /// commit (`=0` restores the old absorption).
    pub(in crate::registry) resolver_absence_shape: bool,
    /// `CRATONVM_BAIS_JDK_BODIES`, read once at construction: over a real
    /// JDK, in either mode, `register()` drops every registration of a
    /// `java/io/ByteArrayInputStream` method other than a constructor and
    /// `close()V` (kept since wave 52: its `BaisEvent::Close` recycles an
    /// `https` response's connection, `RSslLiveSession`), from
    /// every registrar and of every kind, so the JDK's own bodies run. They
    /// were `Bridge`s (and `--compatible` `SyntheticStub`s) over concrete
    /// bytecode (AGENTS.md: real bytes win; the `i50-L5-bridges-still-run-
    /// over-jdk-bytecode-under-jdk-only` page's I/O rows), and every stream
    /// this VM mints writes the real `buf` / `pos` / `mark` / `count` fields
    /// by name, which is all the JDK's bodies read. Interpreter round i1 wave
    /// 51, lane L5.
    pub(in crate::registry) bais_jdk_bodies: bool,
    /// `CRATONVM_JDK_INET_SOCKET_ADDRESS_BODIES`, read once at construction
    /// (default ON, `=0` keeps the natives): under `JdkOnly` over a real JDK,
    /// `register()` drops every registration on `java/net/InetSocketAddress`,
    /// and over a real JDK in either mode the `java/net/SocketAddress.toString()`
    /// one (a class that declares no `toString`), so the JDK's own bodies run. They were reviewed `Intrinsic`s (`phases_early.rs`
    /// `register_phase52_inet_socket_address`), so they won over the JDK's
    /// bytecode in both modes (`resolve_dispatch` step 2), and they were not
    /// HotSpot's: `hashCode()` hashed the address TEXT xor the port (HotSpot:
    /// `addr.hashCode() + port`), the `(String, int)` constructor kept the
    /// caller's literal as the holder's `hostname` (HotSpot: null once
    /// resolved, so `getHostName()` is the reverse lookup) and resolved
    /// `localhost` without the resolver; and each call was a native door the
    /// JIT cannot inline (`Nio20HttpHot isa` 4.6 us against HotSpot's 49 ns;
    /// `HttpClient` builds and hashes one per request). The JDK bodies need
    /// nothing from the VM: the constructor's `InetAddress.getByName` is the
    /// JDK's own under `JdkOnly` (`RETIRED_SHADOW_SBR_NETADDR_TRIPLES`), and
    /// the socket natives read either shape (`inet_addr_resolve`). JIT round
    /// 20 wave 19, lane yHttp (WALKCOMPAT20-3).
    pub(in crate::registry) jdk_inet_socket_address_bodies: bool,
    /// `CRATONVM_STRICT_FAMILY_BRIDGES`, read once at construction (default
    /// ON since interpreter round i7 wave 144, A/B w142lto; `=0` off;
    /// performance): under `JdkOnly` over a real JDK, `register()` drops
    /// a non-`Intrinsic` registration of every `CompatFamily::bridges` triple
    /// (`retired_shadow::is_compat_family_bridge`), the triples `--compatible`
    /// already drops with the family row. Such a `Bridge` never answers under
    /// `JdkOnly` (concrete bytecode beats it, `resolve_dispatch` step 3), but
    /// its registration makes the compile gates (`registered_native_will_run`)
    /// keep the JDK body interpreted: `SoftReference` / `PhantomReference`
    /// `.refersTo` ran ~4.3 us a call under `--real-jdk` against ~0.1 us
    /// under `--compatible`, whose registry lacks the three `refersTo`
    /// Bridges (interpreter round i7 wave 138, lane L2,
    /// `i137-orch-jdk-only-soft-and-phantom-refersto-cost-4-us`). With the
    /// switch on the two modes' registries agree on these triples.
    pub(in crate::registry) strict_family_bridges: bool,
    /// `CRATONVM_NESTED_JAR_WALK_INDEX`, read once at construction: where the
    /// `URLClassLoader` family runs the JDK's bodies
    /// ([`Self::jdk_url_class_loaders`]), the generic
    /// `URLClassPath$Loader.getResource` / `findResource` carry a reviewed
    /// `Intrinsic` that answers `null` without running the body when the
    /// loader's base URL is Spring Boot's `jar:nested:` form and the VM's index
    /// of that nested jar lacks the name -- the one case whose body ends in
    /// Spring's `JarUrlConnection` "not found" -- and runs the JDK body
    /// otherwise. Read through [`Self::nested_jar_walk_index`]. Interpreter
    /// round i2 wave 54, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) nested_jar_walk_index: bool,
    /// `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`, read once at construction
    /// (default ON since the wave-65 triage, `=0` turns it off; the cost was inside host noise): where the walk index is registered
    /// ([`Self::nested_jar_walk_index`]), its per-VM rows carry the outer fat
    /// jar's length and modification time and every ask re-stats the file,
    /// rebuilding a row whose jar was rewritten at the same path since, so a
    /// class added by the rewrite is found as on HotSpot. Read through
    /// [`Self::nested_jar_walk_index_stamp`]. Interpreter round i2 wave 65,
    /// lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) nested_jar_walk_index_stamp: bool,
    /// `CRATONVM_NESTED_JAR_SHARED_OUTER`, read once at construction
    /// (default OFF): where the nested-jar walk index or bytes index is
    /// registered, both build their per-spec rows from ONE per-VM open of the
    /// fat jar's outer archive (its mapping -- read into memory on Windows --
    /// and its parsed central directory), re-stat'ed before every reuse and
    /// reopened when the file changed, instead of mapping the file and
    /// parsing its whole central directory once per nested jar. Read through
    /// [`Self::nested_jar_shared_outer`]. Interpreter round i2 wave 66, lane
    /// L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) nested_jar_shared_outer: bool,
    /// `CRATONVM_NESTED_JAR_OUTER_INDEX`, read once at construction (default
    /// ON since the wave-67 triage, row build -42% with no mapping held; `=0` off): where the nested-jar walk index or bytes index is registered,
    /// both build their per-spec rows from ONE per-VM parse of the fat jar's
    /// outer central directory held WITHOUT the outer bytes
    /// (`cratonvm_classloading::OuterArchiveIndex`): each nested jar is read
    /// at the ask with a bounded seek + read of its one entry, the stamp
    /// re-checked before and after, and the file closed again -- no mapping
    /// and no resident copy across asks, so a concurrent truncation is an
    /// `io::Error` (the body runs), never a `SIGBUS`. The successor of
    /// [`Self::nested_jar_shared_outer`], and preferred over it when both are
    /// set. Read through [`Self::nested_jar_outer_index`]. Interpreter round
    /// i2 wave 67, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_outer_index: bool,
    /// `CRATONVM_NESTED_JAR_REFUSAL_STAMP`, read once at construction
    /// (default ON since the wave-68 triage; `=0` turns it off): a refused row of the nested-jar walk index or of either
    /// bytes index keeps the outer file's stamp (length + mtime, read BEFORE
    /// the refused build) and is rebuilt by a later ask that finds the file
    /// changed (at most once per 100 ms per row), and a bytes-index row whose
    /// file changed is rebuilt instead of turned into a refusal. Without it a
    /// refusal is kept for the VM's life. Read through
    /// [`Self::nested_jar_refusal_stamp`]. Interpreter round i2 wave 68, lane
    /// L5 (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_refusal_stamp: bool,
    /// `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`, read once at construction
    /// (default OFF): with the nested-jar bytes index and the outer index
    /// both on, a bytes-index row over a nested jar STORED in its fat jar
    /// (Spring Boot's layout) keeps only the nested jar's parsed central
    /// directory and reads each served entry from the outer file at the ask
    /// (open, stamp check, one positioned read, CRC check, close) instead of
    /// owning a copy of the whole nested jar; a deflated nested jar keeps
    /// the copy. Read through [`Self::nested_jar_bytes_directory`].
    /// Interpreter round i2 wave 68, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_bytes_directory: bool,
    /// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`, read once at construction
    /// (default OFF; builds on `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`, itself
    /// default off): a read of a directory-only bytes-index row that is part
    /// of a burst (the same fat jar read less than 50 ms before) keeps the
    /// outer handle open for the next read instead of closing it; a small
    /// reaper thread closes it 50 ms after the burst's last read. The handle
    /// is reused only after a `stat` of the path finds the same length,
    /// modification time, device and inode. Unix only (on Windows the pool
    /// keeps nothing). Read through [`Self::nested_jar_outer_handle_reuse`].
    /// Interpreter round i2 wave 69, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_outer_handle_reuse: bool,
    /// `CRATONVM_NESTED_JAR_POOLED_STAT`, read once at construction (default
    /// OFF; builds on `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`): a hit on a
    /// directory-only nested bytes row whose fat jar has a kept handle in the
    /// VM's pool skips the row's own `stat` (length + mtime), because the
    /// pooled read right after it `stat`s the path for the handle's identity
    /// anyway (`NestedArchiveEntries::pooled_read_checks_outer`): one `stat`
    /// per class inside a burst instead of two. A changed file is answered by
    /// the body once (the read refuses it) and the row is rebuilt on the next
    /// ask. Read through [`Self::nested_jar_pooled_stat`]. Interpreter round
    /// i3 wave 71, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_pooled_stat: bool,
    /// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`, read once at construction
    /// (default OFF): where the walk index or the nested-jar bytes index is
    /// registered, a row for a `jar:nested:` spec whose outer fat jar is an
    /// archive of the VM's application class path (`java -jar app.jar`, or
    /// the fat jar on `-cp`) is built from that class path entry's own bytes
    /// and a clone of its parsed central directory
    /// (`ClassPath::outer_archive_for`) -- no open, no positioned read, no
    /// parse -- when the file still has the stamp it had when the class path
    /// loaded it; otherwise the row is built as without the switch. A bytes
    /// row over a stored nested jar is then a range of the class path's
    /// mapping (the mapping the class path holds for the VM's life anyway).
    /// Read through [`Self::nested_jar_class_path_outer`]. Interpreter round
    /// i2 wave 69, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) nested_jar_class_path_outer: bool,
    /// `CRATONVM_NESTED_JAR_BYTES_INDEX`, read once at construction (default
    /// OFF): where the `URLClassLoader` family runs the JDK's bodies
    /// ([`Self::jdk_url_class_loaders`]), `jdk/internal/loader/Resource.getBytes()`
    /// carries a reviewed `Intrinsic` that, for the generic
    /// `URLClassPath$Loader`'s anonymous `Resource` over a Spring Boot
    /// `jar:nested:` URL whose nested jar has a plain view (not Multi-Release,
    /// unsigned), answers the entry's bytes from the VM's own copy of the
    /// nested jar instead of reading Spring's Java stream stack, and runs the
    /// JDK body for every other receiver. Read through
    /// [`Self::nested_jar_bytes_index`]. Interpreter round i2 wave 63, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) nested_jar_bytes_index: bool,
    /// `CRATONVM_JAR_LOADER_BYTES_INDEX`, read once at construction (default
    /// OFF): where the `URLClassLoader` family runs the JDK's bodies
    /// ([`Self::jdk_url_class_loaders`]), `jdk/internal/loader/Resource.getBytes()`
    /// carries a reviewed `Intrinsic` that, for a `URLClassPath$JarLoader`
    /// `Resource` over a plain `java.util.jar.JarFile` (not Multi-Release,
    /// unsigned) whose entry's central-directory CRC and size match the VM's
    /// own mapping of that file, answers the entry's bytes from the mapping
    /// instead of the body's interpreted read loop over the `ZipFile` stream,
    /// and runs the JDK body for every other receiver. Read through
    /// [`Self::jar_loader_bytes_index`]. Interpreter round i2 wave 64, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) jar_loader_bytes_index: bool,
    /// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`, read once at construction
    /// (default OFF): where the jar-loader bytes index is registered
    /// ([`Self::jar_loader_bytes_index`]), its row for a jar that is an
    /// archive of the VM's application class path (a `-cp` jar the JDK's
    /// `AppClassLoader` reads through `URLClassPath$JarLoader`) is built from
    /// that class path entry's own bytes and a clone of its parsed central
    /// directory (`ClassPath::outer_archive_for`,
    /// `ClassPath::open_jar_archive_lent`) -- no second open, mapping or
    /// parse -- when the file still has the stamp it had when the class path
    /// loaded it; otherwise the row is opened as without the switch. The
    /// same lend `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` gives the nested-jar
    /// rows (wave 69), independent of it. Read through
    /// [`Self::jar_loader_class_path_archive`]. Interpreter round i2 wave
    /// 70, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) jar_loader_class_path_archive: bool,
    /// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`, read once at construction
    /// (default OFF): where the jar-loader bytes index is registered
    /// ([`Self::jar_loader_bytes_index`]), a `URLClassPath$JarLoader`
    /// resource of a jar that is an archive of the VM's application class
    /// path is answered straight from that class path entry (its bytes, its
    /// parse, its entry index, its load-time stamp compared with the file by
    /// one `stat`; `ClassPath::jar_entry_bytes_matching`), with no row in the
    /// jar cache at all; a jar the class path does not hold unchanged falls
    /// back to the rows (and to `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`
    /// when that is on). Read through [`Self::jar_loader_class_path_direct`].
    /// Interpreter round i3 wave 71, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) jar_loader_class_path_direct: bool,
    /// `CRATONVM_DEFINE_ONE_PARSE`, read once at construction (default OFF):
    /// `ClassLoader.defineClass1` / `defineClass2` hand the class file their
    /// supertype pre-pass already parsed to the define
    /// ([`DefineClassFull::preparsed`]) instead of parsing the same bytes a
    /// second time. Read through [`Self::define_one_parse`]. Interpreter
    /// round i3 wave 72, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) define_one_parse: bool,
    /// `CRATONVM_REAL_BUILTIN_LOADERS`, read once at construction (default
    /// ON; exactly `0` turns it off, any other value or none keeps it on --
    /// the reader's rule before it moved here): under `--real-jdk` the VM's
    /// platform and application loaders are the JDK's own `ClassLoaders`
    /// objects. Read through [`Self::real_builtin_loaders`]. It was a process
    /// `OnceLock` in `native-builtins/src/classloader.rs`
    /// (`real_builtin_loaders_enabled`) until interpreter round i4 wave 95,
    /// lane L5 (AGENTS.md: no process globals for compatibility state).
    pub(in crate::registry) real_builtin_loaders: bool,
    /// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`, read once at construction
    /// (default OFF): the define natives' supertype pre-pass asks the
    /// defining loader's `loadClass` for a supertype name once per (loader,
    /// name) -- HotSpot's dictionary (`resolve_super_or_fail` finds the name
    /// the loader already answered) -- instead of once per define. Read
    /// through [`Self::define_supertype_dictionary`]. Interpreter round i3
    /// wave 72, lane L5 (same page).
    pub(in crate::registry) define_supertype_dictionary: bool,
    /// `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION`, read once at
    /// construction (default OFF): the define natives' supertype pre-pass
    /// records the defining user loader as an initiating loader of each
    /// supertype it resolves (`NativeContext::note_initiating_load`), so
    /// `findLoadedClass` answers it and a later define of the name by that
    /// loader is a duplicate definition, as on HotSpot. Only where the
    /// initiating records are kept (`--real-jdk`). Interpreter round i3 wave
    /// 79, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) define_records_supertype_initiation: bool,
    /// `CRATONVM_LOADER_KIND_MEMO`, read once at construction (default OFF):
    /// `classloader::is_user_defined_loader` and `is_bare_url_class_loader`
    /// answer a `ClassLoader` subclass from a per-VM memo keyed by the
    /// receiver's class id (the class-loader natives' per-VM caches) instead
    /// of recomputing the verdict on every `findLoadedClass0`, `defineClass1`
    /// and `getResource`. Read through [`Self::loader_kind_memo`].
    /// Interpreter round i3 wave 81, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) loader_kind_memo: bool,
    /// `CRATONVM_CLV_SUB_FAST_KEY`, read once at construction (default OFF,
    /// performance): see [`NativeSystemAccess::clv_sub_fast_key`]. Per
    /// registry, so two VMs in one process each keep their own setting (t1
    /// wave 6 lane Y3; it was a process `OnceLock` in
    /// `classloader_value_sidetable.rs`, first VM wins).
    pub(in crate::registry) clv_sub_fast_key: bool,
    /// `CRATONVM_LOADER_NAMESPACE_INDEX`, read once at construction (default
    /// OFF): `classloader::peek_loader_namespace_id` and
    /// `loader_namespace_id` answer a user loader whose namespace they already
    /// found from a per-VM index keyed by the loader's address, valid while
    /// the VM's collection count has not moved (no object moves or dies
    /// between two collections), and `cl_has_synthetic_layout` answers a class
    /// it already found on the real `ClassLoader` layout from a per-VM memo
    /// keyed by class id. Read through [`Self::loader_namespace_index`].
    /// Interpreter round i3 wave 83, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) loader_namespace_index: bool,
    /// `CRATONVM_PLATFORM_RESOURCE_JRT_WALK`, read once at construction
    /// (default OFF): the platform loader's and a null-parent loader's
    /// `ClassLoader.getResource` native asks only the runtime-image class-path
    /// entries for its `jrt:` URL instead of listing every entry's URLs and
    /// keeping the first `jrt:` one -- the same answer without a directory
    /// entry's `exists()` or a jar's index probes. Read through
    /// [`Self::platform_resource_jrt_walk`]. Interpreter round i3 wave 73,
    /// lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) platform_resource_jrt_walk: bool,
    /// `CRATONVM_INFLATER_REUSE_BUFFERS`, read once at construction (default
    /// OFF): the `Inflater.inflateBytesBytes` native reads its input and
    /// inflates into scratch buffers kept with the stream's zlib state
    /// instead of allocating and zeroing two buffers per call. Read through
    /// [`Self::inflater_reuse_buffers`]. Interpreter round i3 wave 73, lane L5
    /// (same page).
    pub(in crate::registry) inflater_reuse_buffers: bool,
    /// `CRATONVM_INFLATER_INPUT_WINDOW`, read once at construction (default
    /// ON since the wave-75 triage, `on_unless_zero`): `Inflater.inflateBytesBytes` copies the caller's input to zlib
    /// in bounded windows -- the next window only while a step consumed the
    /// whole previous one and did not finish -- instead of the whole
    /// remaining input per call, so a small-output read over a large input
    /// buffer (`InflaterInputStream.read()`, `DataInputStream` over a
    /// `GZIPInputStream`) is O(output), not O(buffered input). Read through
    /// [`Self::inflater_input_window`]. Interpreter round i3 wave 75, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) inflater_input_window: bool,
    /// `CRATONVM_INFLATER_RAW_ADLER_ZERO`, read once at construction (default
    /// ON since the wave-74 triage, `on_unless_zero`): a raw (`nowrap`) `Inflater` keeps no ADLER-32 and answers
    /// `getAdler()` = 0 (zlib's `strm->adler` for a raw stream under HotSpot)
    /// instead of checksumming every inflated byte. Read through
    /// [`Self::inflater_raw_adler_zero`]. Interpreter round i3 wave 74, lane
    /// L5 (`docs/feature-designs/native-dispatch-architecture.md`,
    /// case 1).
    pub(in crate::registry) inflater_raw_adler_zero: bool,
    /// `CRATONVM_INFLATER_SHARDED_TABLE`, read once at construction (default
    /// OFF, `truthy_word`): every `Inflater.init` of this VM mints a handle
    /// for one of the inflater handle table's lock shards
    /// (`native-builtins` `zip_real::inflater_table`) instead of the one
    /// shared table, so threads inflating different streams no longer
    /// serialise on one process-wide lock held across the zlib step. Read
    /// through [`Self::inflater_sharded_table`]. Interpreter round i5 wave
    /// 100, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) inflater_sharded_table: bool,
    /// `CRATONVM_ARRAYS_MISMATCH_BULK`, read once at construction (default
    /// ON since the wave-74 triage, `on_unless_zero`): the `ArraysSupport.mismatch` byte/char intrinsics and
    /// `vectorizedMismatch`'s element-index path compare bulk copies of the
    /// two payloads instead of reading two `Value`s per element. Read through
    /// [`Self::arrays_mismatch_bulk`]. Interpreter round i3 wave 74, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`,
    /// "Progress (wave 74)").
    pub(in crate::registry) arrays_mismatch_bulk: bool,
    /// `CRATONVM_ZIP_UTILS_INTRINSICS`, read once at construction (default
    /// ON since the wave-74 triage, `on_unless_zero`): `java.util.zip.ZipUtils.get16` / `get32` -- every little-endian
    /// CEN / LOC field read of `ZipFile` -- are registered as `Intrinsic`
    /// natives that read the two / four bytes in one call, instead of the JDK
    /// body's two `Preconditions.checkIndex` calls and its `Unsafe`
    /// `getShortUnaligned` / `getIntUnaligned` chain. Registration-time only
    /// (`zip_real::register_zip_real_natives`). Interpreter round i3 wave 74,
    /// lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`,
    /// "Progress (wave 74)").
    pub(in crate::registry) zip_utils_intrinsics: bool,
    /// `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS`, read once at construction
    /// (default OFF): the `java.util.zip.ZipUtils` CEN / LOC header accessors
    /// (`CENFLG`, `CENNAM`, ..., `LOCEXT`; 21 rows) are registered as
    /// `Intrinsic` natives answering the field in range and running the JDK
    /// body otherwise, so a `ZipFile.getEntry` hit's ~10 field reads run no
    /// interpreted accessor frame. Registration-time only
    /// (`zip_real::register_zip_real_natives`). Interpreter round i3 wave 75,
    /// lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) zip_cen_accessor_intrinsics: bool,
    /// `CRATONVM_ZIP_CODER_COMPARE_INTRINSIC`, read once at construction
    /// (default ON since the wave-75 triage, `on_unless_zero`):
    /// `ZipCoder$UTF8ZipCoder.compare` -- the entry-name
    /// compare of every CEN hash hit -- is an `Intrinsic` comparing an
    /// all-ASCII name's chars against the CEN bytes in place (no encode, no
    /// `byte[]`), the JDK body otherwise. Registration-time only. Same wave
    /// and page.
    pub(in crate::registry) zip_coder_compare_intrinsic: bool,
    /// `CRATONVM_JIT_ZIP_HEADER_LOADS`, read once at construction (default
    /// ON since the JIT round 19 proposal round's close, `=0` kill; JIT round
    /// 19 proposal wave 10, lane zip10, i75-L5; host A/B: zip-get-entry
    /// 2714 -> 1431 ns, jar-get-entry 3451 -> 1706, zip-read-sized 21.9 ->
    /// 14.3 us, every armed run under every default run on the get rows): in a VM WITH a JIT, the `ZipUtils.get16` / `get32` intrinsics
    /// (`CRATONVM_ZIP_UTILS_INTRINSICS`) and the CEN / LOC accessor
    /// intrinsics (`CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS`) are NOT registered,
    /// so the JDK's own `ZipUtils` bytecode runs: a registered native on the
    /// triple makes the JIT's inline resolver refuse every site that reaches
    /// it (`native-shadow`), so compiled `getZipEntry` / `getEntryPos` paid a
    /// native-door crossing per CEN field, where HotSpot compiles the same
    /// bytecode to a bounds check and a load (`Preconditions.checkIndex` and
    /// `Unsafe.getShortUnaligned` / `getIntUnaligned`, both answered inline
    /// by this JIT's optimizing tier). Without a JIT the intrinsics register
    /// as before. Registration-time only (`zip_real::register_zip_real_natives`).
    pub(in crate::registry) jit_zip_header_loads: bool,
    /// `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC`, read once at construction
    /// (default OFF, `truthy_word`): `ZipFile$Source.getEntryPos(String,
    /// boolean, ZipCoder)` is an `Intrinsic` that answers the hash walk's
    /// no-hash-match miss (`null`) natively and runs the JDK body for every
    /// other case. Registration-time only
    /// (`zip_real::register_zip_real_natives`). Interpreter round i3 wave 76,
    /// lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) zip_entry_miss_intrinsic: bool,
    /// `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC`, read once at construction (default
    /// OFF, `truthy_word`): the `ZipFile$Source.getEntryPos` intrinsic (also
    /// registered by this switch alone) answers a hash match too -- a
    /// `UTF8ZipCoder` exact match builds the `ZipFile$EntryPos` record
    /// natively, a collision continues the walk natively -- so a hit no longer
    /// walks the chain twice (the native walk, then the JDK body's). A
    /// directory match, a non-ASCII name and every odd layout still run the
    /// JDK body. Registration-time only (`zip_real::register_zip_real_natives`).
    /// Interpreter round i3 wave 77, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) zip_entry_hit_intrinsic: bool,
    /// `CRATONVM_INTRINSIC_BODY_COMPILES`, read once at construction (default
    /// OFF, `truthy_word`): the memo / fast-path `Intrinsic`s that are
    /// registered only in a VM without a JIT register in a JIT VM too, marked
    /// [`Self::mark_body_compilable_intrinsic`], and the interpreter's compile
    /// gates (`registered_native_will_run`) let the JIT compile the JDK body
    /// behind a marked row and bind compiled callers to it: an interpreted
    /// call takes the intrinsic, a decline (`invoke_special_bytecode_only`)
    /// reaches the compiled body instead of the interpreter, and compiled code
    /// may skip the native door. Interpreter round i3 wave 79, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) intrinsic_body_compiles: bool,
    /// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`, read once at construction
    /// (unset: [`INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`], `finder-mint` since
    /// interpreter round i3 wave 81; `=0` / `=none` / `=` empty: no row;
    /// see [`read_intrinsic_body_compiles_rows`]): a comma-separated list of row tokens
    /// (`finder-mint`, `method-type-intern`, `ste-to-string`, ...) that adopt
    /// [`Self::intrinsic_body_compiles`] one by one, so a row that wins under
    /// the JIT can take that path without the rows that lose
    /// ([`Self::intrinsic_body_compiles_row`]). Interpreter round i3 wave 80,
    /// lane L4.
    pub(in crate::registry) intrinsic_body_compiles_rows: Vec<String>,
    /// The `Intrinsic` triples marked body-compilable
    /// ([`Self::mark_body_compilable_intrinsic`]); a handful at most, scanned
    /// only by the compile gates.
    pub(in crate::registry) body_compilable_intrinsics: Vec<(&'static str, &'static str, &'static str)>,
    /// The `Intrinsic` triples whose native re-enters the JDK's own bytecode
    /// (`invoke_*_bytecode_only`), named by their registrar
    /// ([`Self::mark_reentry_only_body`]) so the VM can publish those bodies
    /// re-entry-only (`JitCache::mark_reentry_only`, which alone decides,
    /// under `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`). Recorded always: a
    /// handful of rows, read once at VM init; no dispatch decision reads it.
    /// Interpreter round i6 wave 121, lane L2 (page
    /// `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`).
    pub(in crate::registry) reentry_only_bodies: Vec<(&'static str, &'static str, &'static str)>,
    /// The subset of [`Self::reentry_only_bodies`] whose re-entry-only
    /// publication is decided by its OWN switch rather than the cache-wide
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`
    /// ([`Self::mark_reentry_only_body_on_its_own_switch`]; the registrar
    /// asks that switch). JIT round 23 wave 8h, lane W8-L1:
    /// `BuiltinClassLoader.loadClassOrNull` under
    /// `CRATONVM_JIT_LOADER_REENTRY_BODIES`.
    pub(in crate::registry) reentry_only_bodies_own_switch: Vec<(&'static str, &'static str, &'static str)>,
    /// Per-VM storage for the entry-miss intrinsic's resolved
    /// `ZipFile$Source` field slots ([`Self::zip_source_slot_cache`]): set on
    /// first use by `native-builtins` (`zip_real::zip_source_slots`), never by
    /// this crate. Per VM, not a process global (AGENTS.md): another VM in the
    /// same process may lay the class out differently.
    pub(in crate::registry) zip_source_slots: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// Per-VM storage for the record intrinsics' `boolean`-component memo
    /// ([`Self::record_boolean_components_slot`]): set on first use by
    /// `native-builtins` (`intrinsics::record`), never by this crate. It was a
    /// process-wide `static` keyed by `(vm_identity, ClassId)` (AGENTS.md: no
    /// process globals); per VM it needs no VM key and no forget hook, and it
    /// is dropped with the VM. Interpreter round i6 wave 117, lane L4
    /// (`i39-L4-proposal-one-record-layout-table-per-class`).
    pub(in crate::registry) record_boolean_components: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// Per-VM storage for the `CRATONVM_VH_REAL_VARFORM` record
    /// ([`Self::vh_real_varform_slot`]): set on first use by `native-builtins`
    /// (`lang_invoke::VhRealVarFormRecord`), never by this crate. Interpreter
    /// round i7 wave 136, lane L4: the forms were public statics of
    /// application-visible classes before.
    pub(in crate::registry) vh_real_varform: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// Set once a `SHOW_HIDDEN_FRAMES` stack walk ran in this VM
    /// ([`Self::note_hidden_frames_observed`]); never cleared. Read by the
    /// "hidden frames never observed" latch of the method-handle lanes that
    /// skip hidden `LambdaForm` activations (interpreter round i6 wave 117,
    /// lane L4, `CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`). Per VM (AGENTS.md).
    pub(in crate::registry) hidden_frames_observed: std::sync::atomic::AtomicBool,
    /// `CRATONVM_REFERENCE_LAYOUT_MEMO`, read once at construction (default
    /// OFF, `truthy_word`): `Reference.clear0()` (and `PhantomReference`'s
    /// redeclaration of it) clears `java.lang.ref.Reference.referent` by the
    /// slot resolved once per VM on `Reference` itself, and decides the GC
    /// registry retirement from `Reference.queue` and the two
    /// `ReferenceQueue` sentinels located once per VM, instead of two
    /// by-name hierarchy walks, a by-name read and two class-plus-static
    /// lookups per call. Registration-time only (`lib.rs`, the two `clear0`
    /// rows). Interpreter round i3 wave 77, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`,
    /// "Progress (wave 77)").
    pub(in crate::registry) reference_layout_memo: bool,
    /// Per-VM storage for [`Self::reference_layout_memo`]'s resolved layout
    /// ([`Self::reference_layout_slot`]): set on the first successful
    /// resolution by `native-builtins` (`reference::reference_layout`), never
    /// by this crate. Per VM, not a process global (AGENTS.md).
    pub(in crate::registry) reference_layout: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// Per-VM storage for `native-io`'s process tables
    /// ([`Self::process_tables_slot`]): set on first use by `native-io`
    /// (`process::tables`), never by this crate. Per VM, not a process global
    /// (AGENTS.md); dropped with the VM. Interpreter round i5 wave 101, lane L1.
    pub(in crate::registry) process_tables: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// Per-VM storage for the `Resource.getBytes` intrinsics' archive caches
    /// ([`Self::archive_bytes_cache_slot`]): set on first use by
    /// `native-builtins` (`classloader::archive_bytes_caches`), never by this
    /// crate. A byte cache of files on disk, kept per VM rather than in a
    /// process global (AGENTS.md); interpreter round i2 wave 64, lane L5.
    /// Wave 65 added the nested-jar walk index and the `URLClassLoader`
    /// local class-path cache to it.
    pub(in crate::registry) archive_bytes_cache: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// `CRATONVM_COMPAT_URL_LOADER_DEFINED_PACKAGES`, read once at
    /// construction (default ON, `=0` restores the old answer): under
    /// `--compatible`, `ClassLoader.getDefinedPackage`'s class-file probe for
    /// a user loader whose own URLs hold class files of the package answers
    /// "defined" only once that loader has defined a class of it -- the rule
    /// `--real-jdk` already had. The eager answer fabricated an unsealed
    /// `Package` before `URLClassLoader.defineClass` could define the
    /// package from a `Sealed: true` manifest, so the JDK's
    /// `getAndVerifyPackage` threw "sealing violation: can't seal package
    /// <p>: already loaded" where HotSpot defines the class. Read through
    /// [`Self::compat_url_loader_defined_packages`]. Interpreter round i2
    /// wave 64, lane L5 (probe `L5W64JarLoaderBytesIndex`, `--compatible`).
    pub(in crate::registry) compat_url_loader_defined_packages: bool,
    /// `CRATONVM_STRING_LITERAL_IDENTITY`, read once at construction (default
    /// ON, `=0` kill): where a JDK 25 body answers a string LITERAL, the
    /// native standing in for it answers the pooled literal, not a fresh
    /// equal copy (`native-builtins` `lang_string::string_literal_identity_enabled`).
    /// Per VM since j27 wave 7 (lane T1LOOPSTATIC, backlog row SBEMPTY17-4):
    /// it was a process `OnceLock`, against the AGENTS.md per-VM rule. Read
    /// through [`Self::string_literal_identity`].
    pub(in crate::registry) string_literal_identity: bool,
    /// `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC`, read once at construction
    /// (default on): over a real JDK, in either mode, the platform and
    /// application loaders' package listings (`ClassLoader.packages()`, a
    /// reviewed `Intrinsic` that then runs the JDK body, and the
    /// `getDefinedPackages` native) first put a `Package` for every package of
    /// a class the VM defined for that loader into the loader's own `packages`
    /// map, as HotSpot's loaders have one there; and `--compatible`'s
    /// `Module.getPackages` answers an unnamed module through the JDK body.
    /// Read through [`Self::builtin_loader_package_sync`]. Interpreter round
    /// i2 wave 55, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) builtin_loader_package_sync: bool,
    /// `CRATONVM_RESOURCE_STREAMS_OPEN_URL`, read once at construction (default
    /// on since lane L5's last wave-56 commit; `=0` reverts): where
    /// the VM mints JDK-constructed resource URLs ([`Self::jdk_url_class_loaders`]),
    /// the static `ClassLoader.getSystemResourceAsStream` answers as its JDK
    /// body does, `getSystemResource(name)` then `url.openStream()` (an
    /// `IOException` from the open is `null`), and `Class.getResourceAsStream`
    /// of an unnamed-module class whose loader is a built-in one asks that
    /// loader's `getResourceAsStream`, as the JDK body does; so the stream is
    /// the URL connection's (`BufferedInputStream` for a `file:` class-path
    /// entry), not a `ByteArrayInputStream` over bytes the VM read. Read
    /// through [`Self::resource_streams_open_url`]. Interpreter round i2 wave
    /// 56, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) resource_streams_open_url: bool,
    /// `CRATONVM_DEFINED_CLASS_RESOURCES_DEFINING_LOADER`, read once at
    /// construction (default on since lane L5's wave-57 switch-on commit;
    /// `=0` reverts): a
    /// `"<internal/name>.class"` resource request answers a run-time DEFINED
    /// class's own bytes (`classloader.rs` `defined_class_resource_bytes`, a
    /// VM extension HotSpot does not have) only through the receiver-less
    /// `ClassLoader.getSystemResourceAsStream`, only for a VM CGLIB name
    /// (`$$SpringCGLIB$$`) and only when the application loader DEFINED that
    /// class, instead of from any loaded class of the name, loader-blind,
    /// through that door and `Module.getResourceAsStream`'s fallback. So a
    /// class from `Lookup.defineClass` or a user loader's `defineClass` has no
    /// resource, as on HotSpot. Read through
    /// [`Self::defined_class_resources_defining_loader`]. Interpreter round
    /// i2 wave 57, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) defined_class_resources_defining_loader: bool,
    /// `CRATONVM_BOOT_APPEND_PACKAGE_TABLE`, read once at construction
    /// (default on since lane L5's wave-57 switch-on commit; `=0` reverts):
    /// the boot loader's
    /// `getSystemPackageNames` / `getSystemPackageLocation` stand-ins
    /// (`boot_loader.rs`) answer a package of classes appended to the boot
    /// search from the table `ClassManager::extend_bootstrap_classpath`
    /// records (package -> the appended entry's path), instead of asking every
    /// loaded class whether it was appended and parsing the first resource
    /// URL of one back into a path (undecoded, and shadowable by a same-named
    /// resource earlier on the search). Read through
    /// [`Self::boot_append_package_table`]. Interpreter round i2 wave 57,
    /// lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) boot_append_package_table: bool,
    /// `CRATONVM_JDK_BUILT_PACKAGES`, read once at construction: over a real
    /// JDK, in either mode, the application loader's `Package` for a class it
    /// defined from a `file:` class-path entry is built by the JDK's own
    /// `BuiltinClassLoader.definePackage(pn, man, url)` -- the JAR's
    /// `java.util.jar.Manifest` (per-entry section, then main section,
    /// `Sealed` and the seal base the JDK's way), `null` for a directory --
    /// and put into the loader's `packages` map the first time a door asks,
    /// instead of an uninitialised `Package` whose fields the VM writes from
    /// a Rust parse of `MANIFEST.MF`; and that loader's `getDefinedPackage`
    /// answers a package none of its classes is in with `null`, as the JDK's
    /// map read does. Read through [`Self::jdk_built_packages`]. Interpreter
    /// round i2 wave 58, lane L5
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) jdk_built_packages: bool,
    /// `CRATONVM_IMPL_JARS_ES_LOADER_ONLY`, read once at construction (default
    /// on): the `IMPL-JARS/<module>/` archive fallback after a failed
    /// `loadClass` (the VM-driven resolution door, the base `loadClass` /
    /// `findClass(String, String)` natives) runs only for Elasticsearch's
    /// `EmbeddedImplClassLoader`, the loader it was written for. Any other
    /// loader's miss is the resolution's answer, as on HotSpot, and no longer
    /// pays a whole-class-path `LISTING.TXT` search per Jackson / SnakeYAML
    /// name. Read through [`Self::impl_jars_es_loader_only`]. Interpreter
    /// round i2 wave 54, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::registry) impl_jars_es_loader_only: bool,
    /// `CRATONVM_RETIRE_WALK_CARRIER`, read once at construction (default on
    /// since the lane's last commit; `=0` keeps the carrier): over a real
    /// JDK, the native `StackWalker.walk` / `forEach` always hand out the
    /// JDK's frame classes, and the VM's walk carrier (an instance of the
    /// `StackWalker$StackFrame` INTERFACE) is retired with its eleven
    /// interface natives and the two it forced over the interface's default
    /// `getMethodType` / `getDescriptor` bodies. Read through
    /// [`Self::walk_carrier_retired`]. Interpreter round i2 wave 56, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) walk_carrier_retired: bool,
    /// `CRATONVM_COMPATIBLE_JDK_REIFIER`, read once at construction: under
    /// `Compatible` over a real JDK, the generic-signature natives
    /// ([`GENERIC_REIFIER_NATIVES`]) and the natives over the JDK's own type
    /// classes (`sun/reflect/generics/reflectiveObjects/`) are not
    /// registered, so the JDK's own
    /// `Class` / `Method` / `Constructor` / `Field` / `RecordComponent`
    /// bodies run their `ClassRepository` / `MethodRepository` /
    /// `CoreReflectionFactory` path, as under `JdkOnly` and on HotSpot (a
    /// missing type raises `TypeNotPresentException` through the holder's
    /// loader; a type variable's bounds stay lazy). Read through
    /// [`Self::compatible_jdk_reifier`]. Interpreter round i2 wave 57, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) compatible_jdk_reifier: bool,
    /// `CRATONVM_JDK_TYPE_ANNOTATIONS`, read once at construction: over a real
    /// JDK whose `getTypeAnnotationBytes0` / `Class.getRawTypeAnnotations`
    /// answer the class file's bytes ([`Self::set_type_annotation_bytes_real`]),
    /// the `AnnotatedType` natives ([`TYPE_ANNOTATION_NATIVES`]) are not
    /// registered, so the JDK's `TypeAnnotationParser` builds every
    /// `AnnotatedType` and its own accessors read it, in either mode. Read
    /// through [`Self::jdk_type_annotations`]. Interpreter round i2 wave 57,
    /// lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) jdk_type_annotations: bool,
    /// `CRATONVM_JDK_DECLARATION_ANNOTATIONS`, read once at construction:
    /// under `Compatible`, the natives answering a `Class`'s or a member's
    /// declaration annotations ([`is_declaration_annotation_native`]) are not
    /// registered, and the member mirrors carry their `annotations` /
    /// `parameterAnnotations` / `annotationDefault` bytes as under `JdkOnly`,
    /// so the JDK's `AnnotationParser` answers them -- one parser, the one
    /// the type annotations already use. Read through
    /// [`Self::jdk_declaration_annotations`]. Interpreter round i2 wave 58,
    /// lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) jdk_declaration_annotations: bool,
    /// `CRATONVM_REFLECT_CLASS_ACCESS`, read once at construction: the served
    /// reflective doors ask `Reflection.verifyMemberAccess`'s class half -- a
    /// member of a NON-public class is reachable only from that class's own
    /// RUNTIME package (package name and defining loader) -- after the module
    /// test and before the member modifiers, skipped by the override. Read
    /// through [`Self::reflect_class_access`]. Interpreter round i2 wave 59,
    /// lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_class_access: bool,
    /// `CRATONVM_INNER_CLASSES_AGREEMENT`, read once at construction: the
    /// served `Class` natives that answer a member class's declaring class
    /// (`getDeclaringClass0`, `getEnclosingClass`, `getSimpleName`,
    /// `getCanonicalName`) raise HotSpot's `IncompatibleClassChangeError`
    /// when the declaring class's `InnerClasses`, resolved by ITS loader, does
    /// not name this very class. Read through
    /// [`Self::inner_classes_agreement`]. Interpreter round i2 wave 59, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) inner_classes_agreement: bool,
    /// `CRATONVM_JDK_REFLECTIVE_INVOKE`, read once at construction: over a
    /// real JDK, either mode, default on: `Method.invoke` and `Constructor.newInstance`
    /// are not registered by any registrar, so their JDK bodies run --
    /// `checkAccess`, the accessor factory, and a `DirectMethodHandleAccessor`
    /// / `DirectConstructorHandleAccessor` over a direct method handle (the
    /// native accessors' `invoke0` / `newInstance0` before `java.lang.invoke`
    /// is up). Read through [`Self::jdk_reflective_invoke`]. Interpreter round
    /// i2 wave 59, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) jdk_reflective_invoke: bool,
    /// `CRATONVM_FIELD_DOORS_INTRINSIC`, read once at construction: under
    /// `--real-jdk` over a real JDK, the `Field` value doors (`get` / `set`
    /// and their typed twins) are registered as reviewed `Intrinsic`s rather
    /// than `Bridge`s, so `resolve_dispatch` step 2 serves them with the same
    /// native `--compatible` runs, instead of the JDK's `Field` bytecode
    /// (`Reflection.getCallerClass`, `checkAccess`, `refersTo`, the accessor
    /// and its method-handle door, each a separate door crossing). The
    /// natives already raise HotSpot's exceptions with the JDK accessor's
    /// frames (the served-raise chains) and initialize the declaring class
    /// where the accessor factory does. Read through
    /// [`Self::jdk_only_field_doors`]. Interpreter round i3 wave 73, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`,
    /// cut (b)). Default on since wave 78, which also took the 18 doors out
    /// of `RETIRED_SHADOW_SBR2_CALLER_TRIPLES`: the switch, not the table,
    /// now decides whether `--real-jdk` runs the JDK's `Field` bytecode
    /// (`=0`: the doors register as `SyntheticStub` and are refused, as the
    /// retirement did; see [`Self::field_door_kind`]).
    pub(in crate::registry) jdk_only_field_doors: bool,
    /// `CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC`, read once at
    /// construction: under `--real-jdk` over a real JDK,
    /// `StackTraceElement.of(Object, int)` (the array `Throwable.getOurStackTrace()`
    /// builds) is a reviewed `Intrinsic` that allocates and fills the
    /// elements and stores each `computeFormat()` answer natively, instead of
    /// the JDK body's interpreted allocation loop and per-element
    /// `computeFormat()`. Read through [`Self::stack_trace_element_of_intrinsic`].
    /// Interpreter round i3 wave 74, lane L2. Default off.
    pub(in crate::registry) stack_trace_element_of_intrinsic: bool,
    /// `CRATONVM_REFLECT_INVOKE_PLAN`, read once at construction (default
    /// OFF, `truthy_word`): every `Method` mirror the VM mints carries, in
    /// its `slot` field, the id of a per-VM invocation plan (declaring class
    /// id and name, method name, descriptor, parsed parameters and return,
    /// modifiers), and the served `Method.invoke` reads those facts from the
    /// plan instead of re-deriving them by name on every call. Read through
    /// [`Self::reflect_invoke_plan`]; the plans live in
    /// [`Self::reflect_door_cache_slot`]. Interpreter round i3 wave 74, lane
    /// L3 (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_invoke_plan: bool,
    /// Per-VM storage for the served reflective doors' caches (the
    /// `Method.invoke` plans and the `Field` identity memo;
    /// [`Self::reflect_door_cache_slot`]): set on first use by
    /// `native-builtins` (`lang_class::reflect_door_caches`), never by this
    /// crate. Kept per VM rather than in a process global (AGENTS.md).
    pub(in crate::registry) reflect_door_caches: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// `CRATONVM_FIELD_DOOR_IDENT_MEMO`, read once at construction (default
    /// OFF, `truthy_word`): the served `Field` value doors take their refusal
    /// identity's text (`Decl.name` and the type name -- five `String`s per
    /// call that only a refusal reads) from a per-VM memo keyed by the
    /// field's declaring class, slot and staticness, instead of building it
    /// before the first check of every call. Read through
    /// [`Self::field_door_ident_memo`]. Interpreter round i3 wave 74, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) field_door_ident_memo: bool,
    /// `CRATONVM_STACK_TRACE_ORIGIN_RECORD`, read once at construction
    /// (default OFF, `truthy_word`), either mode: a `StackTraceElement` the
    /// VM fills for a frame of a class it has filled one for before, in ANY
    /// array, copies that class's origin fields (`moduleName`,
    /// `moduleVersion`, `classLoaderName`, `format`) and its interned class
    /// name from a per-VM record ([`Self::stack_trace_origin_record_slot`])
    /// instead of deriving them again, and `--real-jdk`'s
    /// `StackTraceElement.of` intrinsic reuses the class's `computeFormat()`
    /// answer once the module system is up. Read through
    /// [`Self::stack_trace_origin_record`]. Interpreter round i3 wave 75,
    /// lane L2
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) stack_trace_origin_record: bool,
    /// Per-VM storage for the frame-origin records
    /// ([`Self::stack_trace_origin_record`]): set on first use by
    /// `native-builtins` (`lang_misc::frame_origin_records`), never by this
    /// crate. Kept per VM rather than in a process global (AGENTS.md).
    pub(in crate::registry) stack_trace_origin_records: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// `CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT`, read once at construction
    /// (default OFF, `truthy_word`), either mode: the `StackTraceElement[]`
    /// fills (`initStackTraceElements`, the `StackTraceElement.of`
    /// intrinsic, `--compatible`'s `Throwable.getStackTrace()` native) take
    /// each frame's dotted class name straight from its captured internal
    /// name (`lang_class::dotted_binary_name`), where they resolved the class
    /// by name (the class manager's lock and a name hash) and probed the
    /// process-wide dotted-name cache only to compute the same text. A hidden
    /// class's name (the one text the two paths can disagree on, under
    /// `CRATONVM_STACK_TRACE_BINARY_NAMES=0`) keeps the lookup. Read through
    /// [`Self::stack_trace_fill_name_from_text`]. Interpreter round i3 wave
    /// 76, lane L2
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) stack_trace_fill_name_from_text: bool,
    /// `CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC`, read once at
    /// construction (default OFF, `truthy_word`): under `--real-jdk` over a
    /// real JDK, `StackTraceElement.toString()` (a `final` class reading only
    /// its own fields) is a reviewed `Intrinsic` that renders the JDK body's
    /// text in Rust, instead of its interpreted `length()` / `appendTo` /
    /// `StringBuilder` chain. Read through
    /// [`Self::stack_trace_element_to_string_intrinsic`]. Interpreter round i3
    /// wave 76, lane L2
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    pub(in crate::registry) stack_trace_element_to_string_intrinsic: bool,
    /// `CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC`, read once at
    /// construction (default OFF, `truthy_word`): under `--real-jdk` over a
    /// real JDK, `StackTraceElement.of(StackFrameInfo)` (behind every
    /// `StackWalker.StackFrame.toStackTraceElement()`) is a reviewed
    /// `Intrinsic` that allocates the element, fills it as
    /// `initStackTraceElement` does and stores the `computeFormat()` answer
    /// natively, instead of the JDK body's interpreted `computeFormat()`.
    /// Read through [`Self::stack_trace_element_of_frame_intrinsic`].
    /// Interpreter round i3 wave 77, lane L2
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) stack_trace_element_of_frame_intrinsic: bool,
    /// `CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`, read once at construction
    /// (default OFF, `truthy_word`): where the native serves
    /// `Throwable.getStackTrace()` (`--compatible`), the first call on a
    /// throwable whose `stackTrace` is unassigned stores the array it built
    /// there, as the JDK's `getOurStackTrace()` does, and every call answers
    /// a copy of the stored array (the explicitly-set branch answered the
    /// stored array itself). Read through
    /// [`Self::compat_stack_trace_keeps_array`]. Interpreter round i3 wave
    /// 77, lane L2
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) compat_stack_trace_keeps_array: bool,
    /// `CRATONVM_COMPAT_SFI_ELEMENT_JDK_BODY`, read once at construction
    /// (default OFF, `truthy_word`): under `--compatible` over a real JDK,
    /// `StackFrameInfo.toStackTraceElement()` is NOT served by the
    /// `lang_stackwalker` `Bridge`; its JDK body runs (its `synchronized`
    /// memo included), and `StackTraceElement.of(StackFrameInfo)` behind it
    /// is the reviewed `Intrinsic` `--real-jdk` uses
    /// ([`Self::stack_trace_element_of_frame_intrinsic`]). Read through
    /// [`Self::compat_sfi_element_jdk_body`]. Interpreter round i3 wave 79,
    /// lane L2 (`i77-L2-proposal-compatible-walker-element-cost-20261110.md`).
    pub(in crate::registry) compat_sfi_element_jdk_body: bool,
    /// The `StackTraceElement` fill's five behaviour switches
    /// (`CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS`, `_MODULE_PREFIX`,
    /// `_ELEMENT_FULL_ORIGIN`, `_ELEMENT_ORIGIN_MEMO`, `_BINARY_NAMES`), read
    /// once at construction with their unchanged default-on rule. They were
    /// process `OnceLock`s and per-array env reads (interpreter round i3 wave
    /// 77, lane L2:
    /// `docs/feature-designs/native-dispatch-architecture.md`).
    /// Read through [`Self::stack_trace_fill_switches`].
    pub(in crate::registry) stack_trace_fill_switches: StackTraceFillSwitches,
    /// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`, read once at construction
    /// (default OFF, `truthy_word`), either mode, over a real JDK 17+ with the
    /// real AQS: `ReentrantLock.lock()V` / `unlock()V` are reviewed
    /// `Intrinsic`s that serve the uncontended and reentrant shapes on the
    /// JDK's own `state` / `exclusiveOwnerThread` fields, in the JDK bodies'
    /// order, and re-enter the JDK bodies for every other shape. Read through
    /// [`Self::reentrant_lock_uncontended_fast`]. Interpreter round i3 wave 77,
    /// lane L1
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reentrant_lock_uncontended_fast: bool,
    /// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL`, read once at construction
    /// (default OFF, `truthy_word`; DIAGNOSTIC, meaningful only with
    /// [`Self::reentrant_lock_uncontended_fast`]): the two `Intrinsic`s are
    /// registered but decline every call, so a bench prices the fallback (a
    /// native call plus the re-entry into the JDK body) against the plain
    /// JDK body. Read through [`Self::reentrant_lock_fast_decline_all`].
    pub(in crate::registry) reentrant_lock_fast_decline_all: bool,
    /// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE`, read once at
    /// construction (default OFF, `truthy_word`; meaningful only with
    /// [`Self::reentrant_lock_uncontended_fast`]): `ReentrantLock`'s
    /// `lockInterruptibly()V` and `tryLock()Z` join `lock()` / `unlock()` as
    /// reviewed `Intrinsic`s on the same fields (an interrupted caller and
    /// every unserved shape run the JDK bodies). Read through
    /// [`Self::reentrant_lock_fast_wide`]. Interpreter round i3 wave 80, lane
    /// L1 (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reentrant_lock_fast_wide: bool,
    /// `CRATONVM_REFLECT_INVOKE_SETTLED`, read once at construction (default
    /// OFF, `truthy_word`; meaningful with [`Self::reflect_invoke_plan`]): a
    /// planned static `Method.invoke` target outside `java/` and `jdk/`
    /// dispatches through the settling static door
    /// (`NativeInvokeAccess::invoke_static_settling`) with the owner its plan
    /// remembers, instead of the by-name `invoke_by_class_id` tail on every
    /// call. Read through [`Self::reflect_invoke_settled`]. Interpreter round
    /// i3 wave 75, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_invoke_settled: bool,
    /// `CRATONVM_REFLECT_CTOR_PLAN_PER_VM`, read once at construction
    /// (default OFF, `truthy_word`): `Constructor.newInstance`'s plans live in
    /// this VM's reflect-door caches ([`Self::reflect_door_cache_slot`]) under
    /// a read lock, checked against the class name and modifiers on a hit,
    /// instead of the process-wide mutex table; and a planned public
    /// constructor skips the `exports` caller walk when it cannot refuse.
    /// Read through [`Self::reflect_ctor_plan_per_vm`]. Interpreter round i3
    /// wave 75, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_ctor_plan_per_vm: bool,
    /// `CRATONVM_REFLECT_DOOR_WORD_BATCH`, read once at construction (default
    /// OFF, `truthy_word`; meaningful with [`Self::reflect_invoke_plan`]): a
    /// planned `Method.invoke` reads `slot`, `clazz` and `override` in one
    /// batch, builds one argument vector, and the boxing wrapper takes the
    /// return descriptor from the plan instead of a per-call descriptor
    /// `String`; a `Constructor.newInstance` reads `constructorAccessor`,
    /// `slot` and `override` in one batch. Read through
    /// [`Self::reflect_door_word_batch`]. Interpreter round i3 wave 76, lane
    /// L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_door_word_batch: bool,
    /// `CRATONVM_REFLECT_DOOR_PHASE_STATS`, read once at construction (default
    /// OFF, `truthy_word`): the served `Method.invoke` /
    /// `Constructor.newInstance` doors time their phases and print a
    /// per-phase average to stderr every window of calls (a measurement
    /// instrument; nothing a program sees changes). Read through
    /// [`Self::reflect_door_phase_stats`]. Interpreter round i3 wave 77, lane
    /// L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_door_phase_stats: bool,
    /// `CRATONVM_JIT_REFLECT_DOOR_DIRECT`, read once at construction (default
    /// OFF, `truthy_word`): the JIT's virtual inline-cache helper
    /// (`jit_invoke_virtual_mic`) calls the forced `Method.invoke` /
    /// `Constructor.newInstance` native directly -- the callback the
    /// interpreter's cached reflect door takes -- instead of the MIC
    /// resolution, the compile probe and the by-name `invoke_or_native`
    /// cascade it otherwise runs on every call (the native site cache refuses
    /// the `invoke` name). Read through [`Self::jit_reflect_door_direct`].
    /// Interpreter round i3 wave 81, lane L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`,
    /// "Progress (wave 81)").
    pub(in crate::registry) jit_reflect_door_direct: bool,
    /// `CRATONVM_REFLECT_INVOKE_PLAN_FACTS`, read once at construction
    /// (default OFF, `truthy_word`; meaningful with
    /// [`Self::reflect_invoke_plan`]): a `Method` plan keeps its declaring
    /// class's access flags, an exact-class receiver of a planned virtual
    /// call skips the two selection reads, `crosses_package` compares the
    /// interned names, and the boxing wrapper skips the `Class`-mirror check
    /// of a return the body boxed itself. Read through
    /// [`Self::reflect_invoke_plan_facts`]. Interpreter round i3 wave 77, lane
    /// L3
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub(in crate::registry) reflect_invoke_plan_facts: bool,
    /// `CRATONVM_REFLECT_NO_EXPORT_GATE` (presence disables the reflective
    /// `exports` gate), read once at construction instead of through
    /// `runtime_var` on every reflective member access. The variable is read
    /// from the latched snapshot either way, so this is a pure refactor.
    /// Read through [`Self::reflect_export_gate_disabled`]. Interpreter round
    /// i3 wave 75, lane L3.
    pub(in crate::registry) reflect_export_gate_disabled: bool,
    /// Do the type-annotation byte natives answer the class file's bytes
    /// (`--real-jdk`, or `--compatible` under
    /// `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`)? Set from the VM's config
    /// before any `register_*`.
    pub(in crate::registry) type_annotation_bytes_real: bool,
    /// Registrations refused by the `JdkOnly` arm of `register()`, in
    /// registration order. Empty in `Compatible` mode. This is the wave-1
    /// deliverable: §10 is "measurement, not deletion" everywhere except class
    /// fabrication (§5) and stub registration (§4) — this vector is the §4 half
    /// of the evidence, and the CLI report renders it.
    pub(in crate::registry) refused: Vec<JdkOnlyViolation>,
    /// Real-JDK mode: drop synthetic natives whose hardcoded field-slot layout
    /// corrupts the *real* JDK object. Currently `java/util/StringJoiner`,
    /// `java/io/StringReader`, `java/io/InputStreamReader`, `java/util/EnumSet`,
    /// `LinkedBlockingDeque`, and `ScheduledThreadPoolExecutor`.
    /// `StringJoiner` is registered by
    /// `native-collections::register_string_joiner_natives` with a fake
    /// 5-field layout (delim/prefix/suffix/elements-ArrayList/emptyValue) but
    /// bundles into `register_collections_natives` — a function real-JDK mode
    /// calls for the side-table collection natives. On a real StringJoiner (7
    /// fields: prefix/delimiter/suffix/elts[]/size/len/emptyValue) the synthetic
    /// `add` reads slot 3 (real `elts`, null) and no-ops, so `size` never moves
    /// and `toString` renders just prefix+suffix. The real bytecode is
    /// self-contained and correct, so we drop the synthetic surface and let it
    /// run. `EnumSet` has the same problem in a more dangerous form: the
    /// fallback native surface manufactures an abstract `java/util/EnumSet`
    /// receiver with a two-field synthetic layout. In real-JDK mode that
    /// receiver then bypasses or misroutes `add`/`size`/`iterator`, so
    /// `EnumSet.of(...)` and `allOf(...)` on app enums return an empty object
    /// with `iterator() == null`. Dropping the native surface lets the JDK
    /// factories allocate the concrete `RegularEnumSet`/`JumboEnumSet` classes,
    /// which CratonVM's real collection paths already handle. `LinkedBlockingDeque`
    /// is registered as a SyntheticStub fallback by native-collections with the
    /// four-slot fake blocking-queue layout; on a real JDK deque, that constructor
    /// leaves real final fields such as `lock`/`notEmpty` null, and Tomcat's
    /// `WriteBuffer.clear()` then fails in `LinkedBlockingDeque.clear()`.
    /// `StringReader` has the same drift on modern JDKs: the real class wraps a
    /// final `Reader r`, while the synthetic native constructor writes the old
    /// `(content,pos,length)` slots, leaving `r` null before `mark()` delegates.
    /// `Pattern`/`Matcher` have the same drift: the legacy regex natives allocate
    /// real-layout objects but write the old synthetic slots, leaving fields such
    /// as `Matcher.locals` uninitialized.
    /// Same mechanism as the `CRATONVM_REAL_NET_SOCKETS` Socket drop above,
    /// but set by `vm_init`'s real-JDK arm (not env-gated). Off in synthetic mode
    /// (there the fake layout *is* the object layout). Surfaced via Spring
    /// `UriComponentsBuilder.pathSegment`, which dropped the URL path segment
    /// (`ReleaseScheduleTests`).
    pub(in crate::registry) drop_real_layout_synthetic: bool,
    /// PERF (native-ring lazy name map): deferred `cb_ptr -> registrations[idx]`
    /// index for the native-call ring's `fn-ptr → "class.method desc"` map.
    ///
    /// At boot ~3,100 natives are registered. The ring is **off by default**
    /// (`native_ring::is_enabled()` == false), so eagerly building the name
    /// string was pure waste: every `register()` paid a `format!` heap
    /// allocation **plus** a cross-module `name_map()` `Mutex` round-trip for a
    /// diagnostic that is almost never requested.
    ///
    /// We can't simply skip-while-disabled and re-populate later from nothing —
    /// boot registration happens *before* the watchdog arms recording, so the
    /// names would be lost (the WF32-fix rationale). But we DON'T need a second
    /// copy of the parts: the full triple is already persisted in
    /// `registrations` by the unconditional `registrations.push(...)` below.
    /// So we only record a cheap `cb_ptr -> index` here (one `FxHashMap` insert,
    /// no allocation, no cross-module lock), and materialize the actual name
    /// strings lazily via [`flush_native_ring_names`](Self) when — and only
    /// when — a diagnostic dump is actually requested (the ring is enabled).
    ///
    /// First-write-wins (`entry().or_insert(idx)`) to match the eager path's
    /// `register_name`/`or_insert_with` semantics: if the same `fn` pointer is
    /// registered for multiple triples (e.g. compiler function merging), the
    /// first-seen triple is the resolved name, exactly as before.
    pub(in crate::registry) name_index: FxHashMap<usize, usize>,
    /// Per-class memo of the class-only questions [`register`](Self::register)
    /// asks of every registration ([`ClassFacts`]). See that type.
    pub(in crate::registry) class_facts: FxHashMap<Box<str>, ClassFacts>,
    /// This VM's capability policy, or `None` when the embedder installed none.
    ///
    /// A **field**, not a process global, for exactly the reason
    /// [`compatibility_mode`](Self) is: a policy shared between two `SharedVm`s
    /// in one process is the cross-VM interference
    /// `crate::capability` exists to remove. `Arc` because the same policy
    /// object is also reachable from every native through
    /// [`CapabilityCheck`](crate::capability::CapabilityCheck), and the audit
    /// log must be one log, not two.
    ///
    /// `None` is the default and costs one null check in `register()` and one
    /// in [`check_dispatch_capability`](Self::check_dispatch_capability).
    pub(in crate::registry) capabilities: Option<Arc<crate::capability::CapabilitySet>>,
    /// `slot index -> capability kind` for the handful of registered natives
    /// that definitionally exercise a capability
    /// ([`classify_native`](crate::capability::classify_native)).
    ///
    /// Sparse on purpose: of ~3,100 registrations only a few dozen classify,
    /// so this is a small map rather than a byte on the `Copy` `NativeSlot`
    /// that the `find`/`find_with_kind` hot path copies by value. Populated in
    /// `register()` only when a policy is installed — with no policy there is
    /// nothing to check and the map stays empty.
    pub(in crate::registry) sensitive_slots: FxHashMap<u32, crate::capability::CapabilityKind>,
    pub(in crate::registry) app_workarounds: Vec<AppWorkaroundEntry>,
    /// Synthetic stub field layouts for third-party classes registered by
    /// `native-builtins` and copied into `ClassManager` at VM init.
    pub(in crate::registry) app_stub_layouts: FxHashMap<&'static str, &'static [SyntheticStubField]>,
}

/// Kind of application/library workaround registered in [`NativeMethodRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppWorkaroundKind {
    /// ClassLoader subclass whose `CONSTANT_Class` resolution must use the
    /// defining loader's isolated namespace.
    LoaderIsolatedConstantResolution,
    /// ClassLoader subclass that inherits `ClassLoader.loadClass` without
    /// overriding it and must route through `java/lang/ClassLoader.loadClass`.
    InheritBaseClassLoaderLoadClass,
    /// Instrumentation transformer class whose instances must be deduplicated
    /// per VM (keep first copy, ignore later copies).
    SingletonInstrumentationTransformer,
    /// `invokedynamic` bootstrap class whose call sites must not be folded
    /// into constant CallSites.
    NonConstantIndyBootstrap,
    /// Virtual method dispatch where receiver subtype has a registered native
    /// shadow that should be preferred over a base class's method.
    ReceiverSubtypeNativeRetarget,
    /// Package/class prefix excluded from JNI dispatch/site memos.
    SkipJniMemo,
    /// Package/class prefix whose host JNI pointers are ABI-incompatible
    /// (always skip host `dlsym` lookup).
    SkipIncompatibleHostJni,
    /// Package/class prefix whose host JNI pointers are ABI-incompatible
    /// only until real native library load (`netty_tcnative_real` etc.).
    SkipIncompatibleHostJniUntilRealLoad,
    /// Verifier exception superclass hint for pre-Java-7 bytecode lacking
    /// `StackMapTable` frames (`aux` is the superclass internal name).
    VerifierSuperclassHint,
    /// Package prefix that may fall back to a synthetic stub class when absent
    /// from the classpath (non-reflective bytecode linkage).
    EnterpriseStubPackagePrefix,
    /// Package prefix for nested-JAR `IMPL-JARS/<module>/LISTING.TXT` lookup
    /// (`aux` is the module name, or `""` for suffix-derived module names).
    ImplJarsPackagePrefix,
    /// Class or package prefix registered for post-`<clinit>` fixup.
    PostClinitFixup,
    /// Virtual/special dispatch fast-path bridge for loader-aware enum `isIn`.
    LoaderAwareEnumIsInBridge,
    /// Fallback map class name for annotation proxy `asMap`.
    AnnotationMapFallbackClass,
    /// `invokedynamic` boolean cast bootstrap redirect (`aux` is target class
    /// whose `castToBoolean(Object)Z` helper is invoked).
    IndyBooleanCastRedirect,
    /// Alias from a third-party class name (`class_pattern`) to a JDK class
    /// (`aux`) for synthetic stub field layouts.
    StubLayoutAlias,
}

/// Field specification for third-party synthetic stub classes registered by
/// `native-builtins` and consumed by `classloading`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticStubField {
    pub name: &'static str,
    pub descriptor: &'static str,
    pub is_public_static_final: bool,
}

/// Optional predicate on the library version string (when known).
pub type LibraryVersionPredicate = fn(&str) -> bool;

/// One entry in the registry's application-workaround table:
/// `(class_pattern, method_name, descriptor, library_version, reason, kind, aux)`.
#[derive(Clone)]
pub struct AppWorkaroundEntry {
    /// Internal JVM class name or prefix (e.g. `"org/python/core/PyModule"` or `"org/jboss/"`).
    pub class_pattern: &'static str,
    /// Optional method name (empty `""` for class-level or package-level entries).
    pub method_name: &'static str,
    /// Optional method descriptor (empty `""` when not method-specific).
    pub descriptor: &'static str,
    /// Optional library version predicate (`None` matches any version).
    pub library_version: Option<LibraryVersionPredicate>,
    /// Human-readable reason explaining why this workaround exists.
    pub reason: &'static str,
    /// Generic capability / dispatch kind queried by core crates.
    pub kind: AppWorkaroundKind,
    /// Optional auxiliary string (e.g. superclass name or module name).
    pub aux: &'static str,
}


/// The `native` methods of `java.lang.Thread` in the supported JDK images
/// (21 and 25): what stays registered on `java/lang/Thread` in `Compatible`
/// mode when the JDK drives virtual threads
/// ([`NativeMethodRegistry::jdk_continuations`]); every other registration on
/// the class stands in front of a JDK body and is not made.
pub(in crate::registry) const JDK_THREAD_NATIVE_METHODS: &[&str] = &[
    "registerNatives",
    "findScopedValueBindings",
    "currentCarrierThread",
    "currentThread",
    "setCurrentThread",
    "scopedValueCache",
    "setScopedValueCache",
    "ensureMaterializedForStackWalk",
    "yield0",
    "sleep0",
    "sleepNanos0",
    "start0",
    "holdsLock",
    "getStackTrace0",
    "dumpThreads",
    "getThreads",
    "setPriority0",
    "interrupt0",
    "clearInterruptEvent",
    "setNativeName",
    "getNextThreadIdOffset",
    "stop0",
    "suspend0",
    "resume0",
];

