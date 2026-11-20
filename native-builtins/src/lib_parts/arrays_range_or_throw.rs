#[allow(unused_imports)]
use super::super::*;

/// `copyOfRange`'s three-way bound check, in the JDK's own order.
///
/// `Arrays.copyOfRange` is specified as: `from < 0 || from > original.length`
/// is `ArrayIndexOutOfBoundsException`, `from > to` is
/// `IllegalArgumentException`, and **`to` beyond the end is LEGAL** -- the
/// result is zero/null-padded. MEASURED, all four rows silently returned an
/// array on this VM:
///
/// ```text
/// copyOfRange(src, 3, 1)   HotSpot IllegalArgumentException        CratonVM no-throw
/// copyOfRange(src, -1, 2)  HotSpot ArrayIndexOutOfBoundsException  CratonVM no-throw
/// copyOfRange(src, 9, 9)   HotSpot ArrayIndexOutOfBoundsException  CratonVM no-throw
/// copyOfRange(null, 0, 1)  HotSpot NullPointerException            CratonVM no-throw
/// ```
///
/// The old code read both ends with `(*n).max(0)`, which turns a negative
/// `from` into 0 and makes the out-of-range case indistinguishable from a
/// legal one. Clamping an argument is not validating it.
pub fn arrays_range_or_throw(
    from: i32,
    to: i32,
    src_len: usize,
) -> Result<(usize, usize), MethodCallFailed> {
    if from < 0 || (from as usize) > src_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: from,
            message: Some(format!(
                "copyOfRange: from {from} out of bounds for length {src_len}"
            )),
        }
        .into());
    }
    if from > to {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("{from} > {to}"),
        }
        .into());
    }
    Ok((from as usize, (to - from) as usize))
}

pub fn obj_arg(
    args: &[Value],
    idx: usize,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => {
            // RKC16N.9 diagnostic. The "null object argument" NPE is
            // generic across ~3900 obj_arg call sites; without a Java
            // frame at this point we cannot tell *which* native helper
            // is being invoked with a null.
            //
            // When `CRATONVM_DBG_NULL_NATIVE` is set, dump a Rust
            // backtrace + the offending arg index/length to stderr.
            // The topmost non-`obj_arg` frame names the native helper.
            // Gated so production NPEs stay quiet — Java code legally
            // throws NPE in many places (`HashMap.get(null)` etc.) and
            // we don't want to drown those in backtraces.
            if crate::nbflags().dbg_null_native {
                eprintln!(
                    "[obj_arg] null at idx={} args.len={} arg={:?}\n{}",
                    idx,
                    args.len(),
                    args.get(idx),
                    std::backtrace::Backtrace::force_capture()
                );
            }
            Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("null object argument".to_string()),
            }
            .into())
        }
    }
}

pub fn native_not_implemented(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Err(cratonvm_types::error::MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::NotImplemented {
                feature: "native method not yet implemented".to_string(),
            },
        ),
    ))
}

// ---------------------------------------------------------------------------
// java.lang.Object natives
// ---------------------------------------------------------------------------

/// hash21: does `Object.hashCode` owe this receiver the STRUCTURAL hash of the
/// synthetic generic-reflection stubs (`GenericArrayType` / `ParameterizedType`
/// / `WildcardType`) instead of its identity hash? Only an instance of one of
/// those classes: `is_array` receivers are excluded because an array's header
/// class id is its component's (`NativeHeapAccess::object_is_array`), so an
/// array OF those interface types carries the stub's name without being one.
pub fn reflect_stub_hash_is_structural(
    is_array: bool,
    facts: Option<crate::class_facts_memo::NameFacts>,
) -> bool {
    !is_array && facts.is_some_and(crate::class_facts_memo::NameFacts::is_reflect_type_stub)
}

pub fn native_object_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (the object)
    let this = match args.first() {
        Some(Value::Object(Some(obj_ref))) => *obj_ref,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("hashCode on null".to_string()),
            }
            .into());
        }
    };
    // Synthetic generic-reflection stubs (GenericArrayType/ParameterizedType/
    // WildcardType) need a value-based hashCode consistent with their
    // structural `equals` (see the `Object.equals` override) — an identity hash
    // would violate the equals/hashCode contract for an equal-but-distinct
    // deserialized type (spring-bug-08 SerializableTypeWrapperTests). Hash the
    // canonical `getTypeName()` rendering with FNV-1a (stable, deterministic).
    //
    // JIT round 17 wave 2, lane nio2 (FUNNEL17-3): the "is it one of the three"
    // question is answered from a per-thread, per-VM memo of the class id's
    // name facts (`class_facts_memo`), so the common arm takes no class-manager
    // lock and touches no shared refcount. Same answer: the facts are a pure
    // function of the name, which a class id never changes.
    //
    // hash21: an ARRAY is never one of them. A reference array's header (and so
    // `class_id_of_object`) names its COMPONENT class, so `ParameterizedType[]`,
    // `WildcardType[]` and `GenericArrayType[]` read as the stub class here and
    // were hashed structurally (`render_type_name` then read "fields" of the
    // array); JLS 10.7 / `Object.hashCode` give every array its identity hash.
    {
        let cid = ctx.class_id_of_object(this);
        if reflect_stub_hash_is_structural(
            ctx.object_is_array(this),
            crate::class_facts_memo::name_facts(&*ctx, cid),
        ) {
            let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
            let mut h: u32 = 2166136261;
            for b in s.as_bytes() {
                h ^= *b as u32;
                h = h.wrapping_mul(16777619);
            }
            return Ok(Some(Value::Int(h as i32)));
        }
    }
    let hash = ctx.identity_hash_code(this);
    if dbg_vdisp_cached() {
        eprintln!("[vdisp] native_object_hash_code (IDENTITY) called -> {hash}");
    }
    Ok(Some(Value::Int(hash)))
}

/// JIT round 21, lane native (RD16-3; `CRATONVM_JIT_IDENTITY_HASH_LEAF`,
/// default ON): `Object.hashCode` claimed LEAF
/// ([`NativeMethodRegistry::set_leaf`]). A receiver of an ordinary class whose
/// header holds (or mints in place) its identity hash answers on the leaf
/// path: the class-facts memo read and `identity_hash_code_installed` (no
/// allocation, no lock, no GC point). Everything else -- a null receiver (the
/// NPE), a generic-reflection type stub (its structural hash renders the type
/// name), a hash displaced into a monitor, `CRATONVM_DBG_VDISP` -- declines
/// BEFORE any effect and runs [`native_object_hash_code`] on the full funnel.
#[inline]
pub fn object_hash_code_fast(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let cid = ctx.class_id_of_object(this);
    let stub = reflect_stub_hash_is_structural(
        ctx.object_is_array(this),
        crate::class_facts_memo::name_facts(ctx, cid),
    );
    if !stub && !dbg_vdisp_cached() {
        return ctx.identity_hash_code_installed(this);
    }
    None
}

pub fn native_object_hash_code_leaf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(hash) = object_hash_code_fast(&*ctx, *this) {
            return Ok(Some(Value::Int(hash)));
        }
    }
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    native_object_hash_code(ctx, args)
}

/// `Thread.holdsLock(Object)` (T1.6.7), on the full funnel: the monitor table's
/// answer for the calling thread, and HotSpot's `JVM_HoldsLock` NPE for `null`.
pub fn native_thread_holds_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        Some(Value::Object(None)) => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                // HotSpot's `JVM_HoldsLock` throws with no message: `getMessage()`
                // is null (probe L7W29TWaitNotify.java).
                message: None,            }
            .into());        }
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if ctx.current_thread_holds_lock(obj) {
        1
    } else {
        0
    })))
}

/// JIT tier2 wave 5, lane nat21 (NATIVE21-2): `Thread.holdsLock` claimed LEAF
/// ([`NativeMethodRegistry::set_leaf`]). A non-null receiver whose answer the
/// mark word and the calling thread's lock-slot / inflated-monitor caches give
/// (`current_thread_holds_lock_lock_free`: no lock, no allocation, no GC point)
/// is answered here. Everything else declines BEFORE any effect and runs
/// [`native_thread_holds_lock`] on the full funnel: a `null` receiver (so the
/// NPE is raised with the native's frame, as `JVM_HoldsLock`'s is), a cache
/// miss, an inflated monitor outside the cache, a forwarded word.
pub fn native_thread_holds_lock_leaf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(obj))) = args.first() {
        if let Some(held) = ctx.current_thread_holds_lock_lock_free(*obj) {
            return Ok(Some(Value::Int(i32::from(held))));
        }
    }
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    native_thread_holds_lock(ctx, args)
}

/// Cached `CRATONVM_DBG_VDISP` check for `native_object_hash_code` --
/// `Object.hashCode()`'s native is one of the hottest paths in the entire
/// VM (every `HashMap`/`HashSet` operation on a default-hashCode key calls
/// it), so a raw `cratonvm_types::flags::runtime_var_os(...)` here -- a global-lock-guarded
/// syscall -- on every single call is a severe, silently-pervasive
/// performance bug: any hashCode()-heavy workload (Hibernate Validator's
/// reflective constraint-metadata caching was the one that surfaced it,
/// 100+ seconds and climbing for a single-bean `Validator.validate()` call
/// that completes in low tens of milliseconds on HotSpot -- see
/// `vm/src/runtime/env_cache.rs`'s `dbg_vdisp` for the SAME diagnostic
/// already correctly cached for the vm crate's own call sites; this crate
/// cannot depend on `vm`, so it needs its own cache) rather than a
/// per-object cost. Cache the lookup once, like every other `CRATONVM_DBG_*`
/// hot-path check in this file (see `route_ec_to_real` above for the same
/// pattern).
pub fn dbg_vdisp_cached() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| crate::nbflags().dbg_vdisp)
}

/// Map a synthetic object's stamped class — an interface, an abstract class, or
/// a craton-internal backing class — to the concrete, JDK-plausible class that a
/// real HotSpot JVM reports from `Object.getClass()`.
///
/// Several CratonVM synthetic factories stamp their result with the *interface*
/// or *abstract* type it stands in for (e.g. `IntStream.rangeClosed(..)` →
/// the `java/util/stream/IntStream` interface, `FileSystems.getDefault()` →
/// the abstract `java/nio/file/FileSystem`) or with a private craton-internal
/// name (`List.of(..)` → `cratonvm/internal/UnmodifiableList`). On a real JVM an
/// instance's runtime class is always concrete, so `getClass()` must never
/// surface an interface/abstract/internal type. Native method *dispatch* keys on
/// the resolved declaring class, not the receiver's stamp (see
/// `interpreter.rs`), so re-reporting the class here is decoupled from method
/// resolution — it changes only the Java-visible `Class` mirror, not storage,
/// dispatch, or GC layout.
///
/// Returns the internal (slash-separated) name of the concrete substitute for a
/// stamp whose concrete class is fixed (does not depend on the instance), or
/// `None`. Size-dependent immutable-collection stamps are handled separately by
/// [`immutable_collection_kind`] because their concrete class varies with the
/// element count.
pub fn jdk_concrete_getclass_alias(stamp: &str) -> Option<&'static [&'static str]> {
    Some(match stamp {
        // java.util.stream — synthetic streams are stamped with the *interface*.
        // (`IntStream.of(..)` already produces a real `IntPipeline$Head`, so its
        // objects are not stamped with the interface and never reach here.)
        "java/util/stream/Stream" => &["java/util/stream/ReferencePipeline$Head"],
        "java/util/stream/IntStream" => &["java/util/stream/IntPipeline$Head"],
        "java/util/stream/LongStream" => &["java/util/stream/LongPipeline$Head"],
        "java/util/stream/DoubleStream" => &["java/util/stream/DoublePipeline$Head"],
        // java.net — abstract URLConnection subclasses.
        "java/net/JarURLConnection" => &["sun/net/www/protocol/jar/JarURLConnection"],
        "java/net/HttpURLConnection" => &["sun/net/www/protocol/http/HttpURLConnection"],
        // nio21 wave 3 L1: the `--compatible`-only fixed-length body-stream
        // carrier (`http_url_connection::HUC_BODY_STREAM_CLASS`, never minted
        // under `--real-jdk`). HotSpot's `getOutputStream()` after
        // `setFixedLengthStreamingMode` is a `StreamingOutputStream`, so that
        // is the name `getClass()` reports. Display only: the carrier still
        // IS a `ByteArrayOutputStream` (`instanceof` answers true where HotSpot
        // answers false) and a `Method` taken from the reported class does not
        // accept it as a receiver -- the residual on
        // `r21-huc-body-stream-carrier-needs-display-and-baos-family-FIXED-20261012.md`.
        // `CRATONVM_HUC_BODY_STREAM_DISPLAY=0`: the carrier's own name.
        "cratonvm/internal/HucBodyStream"
            if true =>
        {
            &["sun/net/www/protocol/http/HttpURLConnection$StreamingOutputStream"]
        }
        // JSSE carriers (`--compatible`; `--real-jdk` runs the JDK's own and never
        // mints these stamps). The rustls-backed socket / factory / session
        // carriers are stamped with the PUBLIC javax.net.ssl type, which HotSpot
        // can never hand out; it hands out the `sun.security.ssl` classes below
        // (`Nio21CarrierClasses.java`). Since 2026-10-10 the carriers are
        // minted AS those classes (`tls_carriers`, `jsse-socket-impl` / `jsse-session-impl`),
        // so these arms only fire for a carrier minted as the abstract class: a switch
        // set to `0`, or an image without the Impl. Display only there. Record:
        // `compatible-tls-and-http-carriers-are-instances-of-abstract-classes-FIXED-20261010.md`.
        "javax/net/ssl/SSLSocket" => &["sun/security/ssl/SSLSocketImpl"],
        "javax/net/ssl/SSLServerSocket" => &["sun/security/ssl/SSLServerSocketImpl"],
        "javax/net/ssl/SSLSocketFactory" => &["sun/security/ssl/SSLSocketFactoryImpl"],
        "javax/net/ssl/SSLServerSocketFactory" => &["sun/security/ssl/SSLServerSocketFactoryImpl"],
        "javax/net/ssl/SSLSession" => &["sun/security/ssl/SSLSessionImpl"],
        // java.nio.file — abstract/interface stamps; the concrete impl is
        // platform specific. Resolution is best-effort: an unavailable class
        // simply falls back to the stamped mirror (see caller).
        //
        // TWO OF THESE NAMED AN ABSTRACT CLASS until 2026-08-21 (WORKER 4).
        // `sun.nio.fs.UnixFileSystem` and `sun.nio.fs.UnixFileSystemProvider`
        // are the abstract PARENTS of the per-platform classes, so on Linux
        // `FileSystems.getDefault().getClass()` answered
        // `sun.nio.fs.UnixFileSystem` with `Modifier.isAbstract == true`, where
        // HotSpot 25.0.4 answers `sun.nio.fs.LinuxFileSystem`. MEASURED,
        // `probes/W4Abstract.java`, both modes. An alias that names a class
        // `new` cannot produce (JVMS 6.5) is the same defect the mint sites had
        // — it is just being told rather than built.
        //
        // The lists are ordered concrete-first and the resolver takes the first
        // entry that is present AND instantiable, so the abstract parents are
        // kept as LAST-RESORT entries: on a platform whose concrete class is not
        // in this list the answer degrades to today's, never to nothing. The
        // `instantiable` filter is what makes keeping them safe.
        "java/nio/file/FileSystem" => {
            if cfg!(windows) {
                &["sun/nio/fs/WindowsFileSystem"]
            } else {
                &[
                    "sun/nio/fs/LinuxFileSystem",
                    "sun/nio/fs/MacOSXFileSystem",
                    "sun/nio/fs/BsdFileSystem",
                    "sun/nio/fs/AixFileSystem",
                    "sun/nio/fs/SolarisFileSystem",
                    "sun/nio/fs/UnixFileSystem",
                ]
            }
        }
        "java/nio/file/spi/FileSystemProvider" => {
            if cfg!(windows) {
                &["sun/nio/fs/WindowsFileSystemProvider"]
            } else {
                &[
                    "sun/nio/fs/LinuxFileSystemProvider",
                    "sun/nio/fs/MacOSXFileSystemProvider",
                    "sun/nio/fs/BsdFileSystemProvider",
                    "sun/nio/fs/AixFileSystemProvider",
                    "sun/nio/fs/SolarisFileSystemProvider",
                    "sun/nio/fs/UnixFileSystemProvider",
                ]
            }
        }
        "java/nio/file/Path" => {
            if cfg!(windows) {
                &["sun/nio/fs/WindowsPath"]
            } else {
                &["sun/nio/fs/UnixPath"]
            }
        }
        _ => return None,
    })
}

/// The resolved display strategy for a stamped class, memoised per stamp.
#[derive(Clone, Copy)]
pub enum GetClassDisplay {
    /// No remap — report the stamped class unchanged (the common case).
    Stamp,
    /// A fixed concrete alias, already resolved to its `ClassId`.
    Fixed(cratonvm_types::ClassId),
    /// A `cratonvm/internal/Unmodifiable{List,Set,Map}` stamp shared by the
    /// immutable `*.of`/`copyOf` factories and the `Collections.unmodifiable*`
    /// wrappers. Resolved per object: the immutable marker (slot 1) selects the
    /// size-discriminated `ImmutableCollections$*` family (`List12`/`ListN`,
    /// `Set12`/`SetN`, `Map1`/`MapN`); otherwise the `Collections$Unmodifiable*`
    /// family (with a `RandomAccess` check for lists).
    CollList,
    CollSet,
    /// `cratonvm/internal/UnmodifiableSortedSet` ? backs
    /// `Collections.unmodifiableSortedSet` (distinct from `CollSet`, which
    /// backs plain `unmodifiableSet` and does NOT implement SortedSet).
    CollSortedSet,
    /// `cratonvm/internal/UnmodifiableNavigableSet` ? backs
    /// `Collections.unmodifiableNavigableSet` and keeps NavigableSet casts
    /// separate from plain unmodifiable set wrappers.
    CollNavigableSet,
    CollMap,
    /// `cratonvm/internal/UnmodifiableCollection` — only ever produced by
    /// `Collections.unmodifiableCollection` (no immutable factory), so it always
    /// maps to `Collections$UnmodifiableCollection`.
    CollUnmod,
    /// `cratonvm/internal/UnmodifiableEntrySet` — split out from `CollSet`
    /// because an `entrySet()` VIEW of an immutable map does not follow the
    /// `Set12`/`SetN` split its keySet/values siblings use: HotSpot answers
    /// `ImmutableCollections$Set12` at size 1 and `ImmutableCollections$MapN$1`
    /// at every other size (0 included), for `Map.of(...)`'s `entrySet()`.
    /// `l3-followups-the-carrier-identity-and-the-dead-registrations-20260830.md`
    /// §1a.
    CollEntrySet,
    /// A `java/util/Spliterator`-stamped carrier this VM minted. Reporting the
    /// bare interface name from `getClass()` is not merely wrong, it is
    /// IMPOSSIBLE (JVMS 6.5: `new` on an interface cannot produce an
    /// instance) — same doc, §1b. The default alias
    /// (`Spliterators$IteratorSpliterator`) is right for the common
    /// iterator-backed case; a producer that knows its exact shape writes a
    /// hint into slot 4 (`SPL_FIELD_DISPLAY_HINT` in native-collections;
    /// CONTRACT: keep the index and the three codes — 0 default, 1
    /// `Collections$2`, 2 `AbstractList$RandomAccessSpliterator` — in sync)
    /// and this reads it back.
    Spliterator,
    /// `cratonvm/internal/UnmodifiableListItr` — the shared carrier
    /// `native-collections::alloc_unmod_list_itr` mints for `iterator()` and
    /// `listIterator()` over both `List.of`/`copyOf` and
    /// `Collections.unmodifiableList`. HotSpot hands out three different
    /// classes from that one call shape; slot 2
    /// (`UNMOD_LIST_ITR_IMMUTABLE`) and slot 3
    /// (`UNMOD_LIST_ITR_VIA_LIST_ITERATOR`) are what this carrier lost at
    /// mint time and cannot re-derive from the snapshot alone. CONTRACT: keep
    /// both indices in sync with that constant pair.
    CollListItr,
    /// A real `java/util/ArrayList$Itr` this VM minted for a receiver whose
    /// declared type is not `ArrayList` — `native-collections::
    /// alloc_arraylist_iterator_as` hands this carrier to `Vector`/`Stack` too,
    /// because both extend `AbstractList` and carry a real `modCount`, but
    /// HotSpot's own `Vector.iterator()` returns a `Vector$Itr` instead
    /// (`apps/probes/ItrCarrierCensus`, `phase-2s-last-lead...20260908.md`
    /// §2). Display-only: dispatch still resolves on the actual
    /// `ArrayList$Itr` class, so `native_al_itr_*` is unaffected by what
    /// `getClass()` reports here.
    ArrayListItr,
    /// A synthetic stream carrier stamped with one of the four stream
    /// INTERFACES (`java/util/stream/Stream`, `IntStream`, ...). Resolved per
    /// object (JIT round 18 wave 10, lane streams5): a stage an intermediate op
    /// built names its own HotSpot class (`ReferencePipeline$2` for `filter`,
    /// `SortedOps$OfInt`, ...; `cratonvm_native_collections::
    /// synthetic_stream_stage_display_name`), a source stage keeps the
    /// `*Pipeline$Head` alias resolved here. `CRATONVM_STREAM_STAGE_DISPLAY_CLASS=0`
    /// answers the alias for every stage, the pre-wave-10 `Fixed` answer.
    StreamStage(cratonvm_types::ClassId),
    /// The synthetic `java/nio/file/FileSystem` stamp (an ABSTRACT class), which
    /// this VM mints for the platform default filesystem where no concrete one
    /// resolves AND for every mounted jar / runtime-image filesystem
    /// (`nio_file::p57_alloc_jar_filesystem` / `p57_alloc_jrt_filesystem`).
    /// Resolved per object (nio21 wave 14, lane L3): a jrt mount answers
    /// `jdk.internal.jrtfs.JrtFileSystem` and a jar mount
    /// `jdk.nio.zipfs.ZipFileSystem`, as HotSpot's `FileSystems.getFileSystem(
    /// URI.create("jrt:/"))` / `newFileSystem(zip)` do; anything else keeps the
    /// platform alias resolved here (`LinuxFileSystem` / `WindowsFileSystem`).
    /// It used to be `Fixed` to the platform class for all three
    /// (`Nio21ConcreteReceivers` rows `fs-jrt` / `fs-zip`). Display only: the
    /// object is still the synthetic carrier, so the JVMS 6.5 census still
    /// counts it (`r21-natives-hand-back-uninstantiable-receivers-FIXED-20261009.md`).
    /// The `java/nio/file/spi/FileSystemProvider` stamp takes the same arm: a
    /// minted `jar` / `jrt` provider answers `jdk.nio.zipfs.ZipFileSystemProvider`
    /// / `jdk.internal.jrtfs.JrtFileSystemProvider` (`nio_file::
    /// p57_minted_provider_scheme`).
    /// `CRATONVM_FS_MOUNT_DISPLAY_CLASS=0` restores the `Fixed` answer.
    FileSystemMount(cratonvm_types::ClassId),
}



/// The HotSpot class of a mounted synthetic FileSystem `this`
/// ([`GetClassDisplay::FileSystemMount`]), or `None` for the platform default
/// (and for an image that lacks the class, or whose class `new` could not
/// produce: the alias must never name an abstract class, JVMS 6.5).
pub fn getclass_fs_mount_display(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<cratonvm_types::ClassId> {
    let is_fs = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("java/nio/file/FileSystem");
    let name = if is_fs {
        match crate::phases_late::nio_file::p57_fs_scheme(&*ctx, this) {
            "jrt" => "jdk/internal/jrtfs/JrtFileSystem",
            "jar" => "jdk/nio/zipfs/ZipFileSystem",
            _ => return None,
        }
    } else {
        match crate::phases_late::nio_file::p57_minted_provider_scheme(&*ctx, this).as_deref() {
            Some("jrt") => "jdk/internal/jrtfs/JrtFileSystemProvider",
            Some("jar") => "jdk/nio/zipfs/ZipFileSystemProvider",
            _ => return None,
        }
    };
    let cid = getclass_resolve_display(ctx, name)?;
    cratonvm_native_api::instantiable::class_is_instantiable(&*ctx, cid).then_some(cid)
}

/// Classify the craton-internal immutable/unmodifiable collection stamps.
pub fn collection_display_kind(stamp: &str) -> Option<GetClassDisplay> {
    match stamp {
        "cratonvm/internal/UnmodifiableList" => Some(GetClassDisplay::CollList),
        "cratonvm/internal/UnmodifiableSet" => Some(GetClassDisplay::CollSet),
        // entrySet() view — its own family, not unified with CollSet: an
        // immutable map's entrySet() does not follow the Set12/SetN split
        // (see `GetClassDisplay::CollEntrySet`).
        "cratonvm/internal/UnmodifiableEntrySet" => Some(GetClassDisplay::CollEntrySet),
        "cratonvm/internal/UnmodifiableSortedSet" => Some(GetClassDisplay::CollSortedSet),
        "cratonvm/internal/UnmodifiableNavigableSet" => Some(GetClassDisplay::CollNavigableSet),
        "cratonvm/internal/UnmodifiableMap" => Some(GetClassDisplay::CollMap),
        "cratonvm/internal/UnmodifiableCollection" => Some(GetClassDisplay::CollUnmod),
        "java/util/Spliterator" => Some(GetClassDisplay::Spliterator),
        "cratonvm/internal/UnmodifiableListItr" => Some(GetClassDisplay::CollListItr),
        // JIT round 16 (lane compatdrain): the live `iterator()` carrier of
        // `Collections.unmodifiable{Collection,Set,List}` is JDK 25's
        // `Collections$UnmodifiableCollection$1` (two fields, so the
        // `CollListItr` arm's slot-2 / slot-3 tests answer exactly that name).
        // `CRATONVM_COMPAT_UNMOD_ITERATOR_JDK=0`: the stamp, as before.
        "cratonvm/internal/UnmodifiableItr"
            if true =>
        {
            Some(GetClassDisplay::CollListItr)
        }
        "java/util/ArrayList$Itr" => Some(GetClassDisplay::ArrayListItr),
        _ => None,
    }
}

thread_local! {
    // Per-stamp memo for the display strategy. `getClass()`/`toString()` are
    // hot; resolving the stamped name (a `String` clone) and loading the alias
    // on every call would allocate each time. `ClassId`s are stable for the VM
    // lifetime, so this never goes stale -- for ONE VM. Keyed by
    // `(vm_identity, class_id)` since JIT round 17 wave 1 (lane nio), as
    // `object_to_string_dotted_name` is: class ids restart at zero in every VM,
    // and a thread that serves two VMs (an embedder, a test binary) read the
    // first VM's verdict for the second VM's class.
    //
    // `FxHashMap` (JIT round 19, nio19): the JIT's thin `getClass()` helper
    // (`jit_object_get_class_direct`) asks this memo on EVERY call, and std's
    // SipHash was 8% of `R17NioRows getclass` (`hash_one` + `Sip13Rounds::
    // write`). The key is a VM identity and a dense class id, not a packed
    // object word, so Fx spreads it.
    pub static GETCLASS_ALIAS_CACHE: std::cell::RefCell<
        // addr-keyed: not a heap address -- the key's `usize` is the VM's identity.
        rustc_hash::FxHashMap<(usize, cratonvm_types::ClassId), GetClassDisplay>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    // Secondary memo: concrete display name -> resolved `ClassId` (loading on
    // first use). Shared by the fixed and size-dependent paths. The resolved id
    // is a per-VM fact, so the key carries the VM too (round 17, as above).
    pub static GETCLASS_NAME_TO_ID: std::cell::RefCell<
        // addr-keyed: not a heap address -- the key's `usize` is the VM's identity.
        rustc_hash::FxHashMap<(usize, &'static str), Option<cratonvm_types::ClassId>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Is `callback` the registered body of `Object.getClass()`? The JIT's
/// site-cached native arm (`LeafNativeKind::ObjectGetClass`) keys on this, as
/// its `Integer.hashCode` twin does (SBR round 2, lane CALLER).
pub fn is_object_hash_code_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let f1: cratonvm_native_api::NativeCallback = native_object_hash_code;
    let f2: cratonvm_native_api::NativeCallback = native_object_hash_code_leaf;
    let addr = callback as usize;
    addr == f1 as usize || addr == f2 as usize
}

pub fn is_object_get_class_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let own: cratonvm_native_api::NativeCallback = native_object_get_class;
    callback as usize == own as usize
}

/// Is `callback` the registered body of `Class.isInstance(Object)`? The JIT's
/// site-cached native arm (`LeafNativeKind::ClassIsInstance`, JIT round 20
/// wave 17, lane pAtom2, `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE`) keys on
/// this, as the `Object.getClass` arm keys on [`is_object_get_class_native`].
pub fn is_class_is_instance_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let own: cratonvm_native_api::NativeCallback = lang_class::native_class_is_instance;
    callback as usize == own as usize
}

/// Does `callback` return, when it returns an object at all, a FRESH live
/// base: an allocation made after the callback's last GC point, with nothing
/// after it that can collect? JIT round 19 wave 3, lane funnel3 (FUNNEL2-19-3,
/// `CRATONVM_JIT_NATIVE_FRESH_RESULT`): the JIT native doors then heal the
/// result with the forwarding read alone (`safe_native_call_jit_fresh_result`)
/// instead of the funnel's entry heap walk.
///
/// The claim rides on the CALLBACK, as the leaf claim does, so a triple
/// re-registered to another body loses it. Each member is listed with the
/// proof, read off its body; add one only with the same reading:
///
/// * `native_string_value_of_int` (`Integer.toString(I)`, `String.valueOf(I)`)
///   and `native_string_value_of_long` (`Long.toString(J)`,
///   `String.valueOf(J)`): `create_string_uninterned_gc_safe` is the last
///   call. Its only GC point (the young-headroom `maybe_gc`, or the
///   collect-and-retry ladder on exhaustion) runs BEFORE the allocation it
///   returns, and what follows the allocation (`note_native_string_alloc`:
///   two header reads and a TLAB counter) cannot collect.
pub fn is_fresh_object_result_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let cb = callback as usize; // Cast: fn address identity
    let int: cratonvm_native_api::NativeCallback = lang_string::native_string_value_of_int;
    let long: cratonvm_native_api::NativeCallback = lang_string::native_string_value_of_long;
    cb == int as usize || cb == long as usize
}


/// `true` when `class_id`'s `getClass()` display strategy is already memoised
/// as "report the stamped class unchanged" -- the common case, and the only one
/// a no-allocation `getClass()` probe may answer (a miss classifies by name and
/// may load a class, which the native does and a leaf must not).
///
/// `vm_identity` is the asking VM's (`SharedVm::vm_identity`, the value
/// `NativeContext::vm_identity` returns): the memo is per thread AND per VM.
pub fn getclass_display_is_cached_stamp(
    vm_identity: usize,
    class_id: cratonvm_types::ClassId,
) -> bool {
    GETCLASS_ALIAS_CACHE.with(|c| {
        matches!(
            c.borrow().get(&(vm_identity, class_id)),
            Some(GetClassDisplay::Stamp)
        )
    })
}

/// Resolve (and cache) a concrete display class name to its `ClassId`, loading
/// it on first use. `None` if the class is unavailable (caller falls back).
pub fn getclass_resolve_name(
    ctx: &mut dyn NativeContext,
    name: &'static str,
) -> Option<cratonvm_types::ClassId> {
    let key = (ctx.vm_identity(), name);
    if let Some(cached) = GETCLASS_NAME_TO_ID.with(|c| c.borrow().get(&key).copied()) {
        return cached;
    }
    let resolved = ctx.class_id_by_name(name).or_else(|| {
        let _ = ctx.load_class(name);
        ctx.class_id_by_name(name)
    });
    GETCLASS_NAME_TO_ID.with(|c| c.borrow_mut().insert(key, resolved));
    resolved
}

/// [`getclass_resolve_name`] for a class a remap arm of
/// [`getclass_display_class_id`] REPORTS. Every such arm resolves its answer
/// here, and the name must be one [`getclass_display_target_listed`] knows:
/// that list is what [`getclass_reports_only_itself`] (the compiled
/// `x.getClass() == C.class` fold's predicate) reads, so a new arm that
/// forgets to list its answer fails every debug run that reaches it instead
/// of letting the fold answer `false` for an object that reports `C`.
/// JIT round 20 wave 12, lane qMisc (NIO20-L3-1).
pub fn getclass_resolve_display(
    ctx: &mut dyn NativeContext,
    name: &'static str,
) -> Option<cratonvm_types::ClassId> {
    debug_assert!(
        getclass_display_target_listed(name),
        "getClass() display target {name} is not listed in GETCLASS_DISPLAY_TARGETS"
    );
    getclass_resolve_name(ctx, name)
}

/// Every class name a remap arm of [`getclass_display_class_id`] can report
/// for an object stamped with ANOTHER class, besides the alias table's
/// answers ([`GETCLASS_ALIAS_SOURCES`] -> [`jdk_concrete_getclass_alias`])
/// and the stream stage names (`synthetic_stream_stage_display_names`), which
/// are read from their own tables. Listed whatever the switches gating an
/// arm say (`CRATONVM_HUC_BODY_STREAM_DISPLAY`, `CRATONVM_FS_MOUNT_DISPLAY_CLASS`,
/// ...): the predicate must hold for every configuration. JIT round 20 wave
/// 12, lane qMisc (NIO20-L3-1).
pub const GETCLASS_DISPLAY_TARGETS: &[&str] = &[
    // `CollList`
    "java/util/ImmutableCollections$List12",
    "java/util/ImmutableCollections$ListN",
    "java/util/Collections$UnmodifiableRandomAccessList",
    "java/util/Collections$UnmodifiableList",
    // `CollSet`
    "java/util/AbstractMap$1",
    "java/util/ImmutableCollections$Set12",
    "java/util/ImmutableCollections$SetN",
    "java/util/Collections$UnmodifiableSet",
    // `CollEntrySet`
    "java/util/ImmutableCollections$MapN$1",
    "java/util/Collections$UnmodifiableMap$UnmodifiableEntrySet",
    // `CollSortedSet` / `CollNavigableSet`
    "java/util/Collections$UnmodifiableSortedSet",
    "java/util/Collections$UnmodifiableNavigableSet",
    // `CollMap`
    "java/util/ImmutableCollections$Map1",
    "java/util/ImmutableCollections$MapN",
    "java/util/Collections$UnmodifiableMap",
    // `CollUnmod`
    "java/util/AbstractMap$2",
    "java/util/Collections$UnmodifiableCollection",
    // `Spliterator`
    "java/util/Collections$2",
    "java/util/AbstractList$RandomAccessSpliterator",
    "java/util/Spliterators$IteratorSpliterator",
    // `ArrayListItr`
    "java/util/Vector$Itr",
    // `CollListItr`
    "java/util/ImmutableCollections$ListItr",
    "java/util/Collections$UnmodifiableList$1",
    "java/util/Collections$UnmodifiableCollection$1",
    // `FileSystemMount` (`getclass_fs_mount_display`)
    "jdk/internal/jrtfs/JrtFileSystem",
    "jdk/nio/zipfs/ZipFileSystem",
    "jdk/internal/jrtfs/JrtFileSystemProvider",
    "jdk/nio/zipfs/ZipFileSystemProvider",
    // The alias table's switch-gated row (`cratonvm/internal/HucBodyStream`),
    // listed here too so a run with the switch off at the fold's compile and
    // on later (it is read per classification) cannot slip past.
    "sun/net/www/protocol/http/HttpURLConnection$StreamingOutputStream",
];

/// Every stamp [`jdk_concrete_getclass_alias`] has an arm for (its keys, the
/// switch-gated one included). Checked against the table on every
/// classification in debug builds ([`getclass_display_class_id`]). JIT round
/// 20 wave 12, lane qMisc (NIO20-L3-1).
pub const GETCLASS_ALIAS_SOURCES: &[&str] = &[
    "java/util/stream/Stream",
    "java/util/stream/IntStream",
    "java/util/stream/LongStream",
    "java/util/stream/DoubleStream",
    "java/net/JarURLConnection",
    "java/net/HttpURLConnection",
    "cratonvm/internal/HucBodyStream",
    "javax/net/ssl/SSLSocket",
    "javax/net/ssl/SSLServerSocket",
    "javax/net/ssl/SSLSocketFactory",
    "javax/net/ssl/SSLServerSocketFactory",
    "javax/net/ssl/SSLSession",
    "java/nio/file/FileSystem",
    "java/nio/file/spi/FileSystemProvider",
    "java/nio/file/Path",
];

/// May `getClass()` report the class named `name` for an object whose
/// header names another class? The listed targets, every alias of every
/// alias row, and every stream stage name.
pub fn getclass_display_target_listed(name: &str) -> bool {
    GETCLASS_DISPLAY_TARGETS.contains(&name)
        || GETCLASS_ALIAS_SOURCES
            .iter()
            .filter_map(|stamp| jdk_concrete_getclass_alias(stamp))
            .any(|aliases| aliases.contains(&name))
        || cratonvm_native_collections::synthetic_stream_stage_display_names()
            .iter()
            .any(|stage| !stage.is_empty() && *stage == name)
}

/// May `getClass()` report a class OTHER than the one named `name` for an
/// object whose header names it -- is `name` a stamp some remap arm of
/// [`getclass_display_class_id`] classifies? Asked through the arms' own
/// classifiers, so it cannot drift from them; the switch-gated arms answer
/// `true` whatever their switch says, and every `cratonvm/internal/` stamp
/// does too.
pub fn getclass_display_source(name: &str) -> bool {
    name.starts_with("cratonvm/internal/")
        || name == "java/util/ArrayList$Itr"
        || name == "java/util/Spliterator"
        || collection_display_kind(name).is_some()
        || GETCLASS_ALIAS_SOURCES.contains(&name)
        || jdk_concrete_getclass_alias(name).is_some()
        || cratonvm_native_collections::is_synthetic_stream_class_name(name)
}

/// JIT round 20 wave 12, lane qMisc (NIO20-L3-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`, the sound
/// route): does `getClass()` of a non-array object report the class named
/// `name` EXACTLY when the object's header names that class? Holds when no
/// remap arm reports `name` for another stamp
/// ([`getclass_display_target_listed`]) and none remaps `name` itself
/// ([`getclass_display_source`]). Then a compiled `x.getClass() == C.class`
/// is `x != null` (else the NPE) and an exact header class test, with no
/// mirror at all. By NAME, so a same-named class of any loader is refused
/// with it: conservative, never wrong. An array class name (`[`) is refused
/// (its header names the component). Pure: static tables only.
pub fn getclass_reports_only_itself(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('[')
        && !getclass_display_source(name)
        && !getclass_display_target_listed(name)
}


/// The element count of a synthetic immutable collection, via its own `size()`.
/// Best-effort: any failure yields `-1`, which selects the `…N` (multi) form.
pub fn getclass_collection_size(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    match ctx.invoke_virtual(this, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n as i64,
        _ => -1,
    }
}

/// Whether a craton-internal collection wrapper was produced by an *immutable*
/// factory (`List.of`/`copyOf` …) rather than `Collections.unmodifiable*`. Reads
/// the marker slot set by `freeze_result` in native-collections. CONTRACT: this
/// index must match `UNMOD_FIELD_IMMUTABLE` (= 1) there.
pub fn getclass_immutable_marker(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, 1), Value::Int(1))
}

/// Whether `this` is a view (keySet/values/entrySet) of a map that came from
/// `Map.of`/`Map.copyOf` — mirrors
/// `native-collections::UNMOD_FIELD_IMMUTABLE_VIEW` (slot 2). CONTRACT: keep
/// the index in sync; see that constant's own doc comment for why it is a
/// separate slot from `UNMOD_FIELD_IMMUTABLE`.
pub fn getclass_immutable_view_marker(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) > 2 && matches!(ctx.get_field(this, 2), Value::Int(1))
}

/// The display-name hint a `java/util/Spliterator`-stamped carrier's producer
/// left in slot 4 — mirrors `native-collections::SPL_FIELD_DISPLAY_HINT`.
/// CONTRACT: keep the index and the codes in sync. Absent (narrower object, or
/// a producer that never set one) means "no opinion": the generic
/// `Spliterators$IteratorSpliterator` default.
pub fn getclass_spliterator_display_hint(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if ctx.object_num_fields(this) > 4 {
        if let Value::Int(n) = ctx.get_field(this, 4) {
            return n;
        }
    }
    0
}

/// Whether the backing collection of an unmodifiable-list wrapper implements
/// `java.util.RandomAccess` — HotSpot reports `Collections$UnmodifiableRandom`
/// `AccessList` for those and `Collections$UnmodifiableList` otherwise. The
/// backing lives in slot 0 (`UNMOD_FIELD_BACKING`).
pub fn getclass_backing_is_random_access(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let backing = match ctx.get_field(this, 0) {
        Value::Object(Some(b)) => b,
        _ => return false,
    };
    let ra = match getclass_resolve_name(ctx, "java/util/RandomAccess") {
        Some(cid) => cid,
        None => return false,
    };
    getclass_object_reaches(ctx, backing, ra, 0)
}

/// Does `obj` reach `target` as the object it STANDS FOR?
///
/// The plain `is_subclass` above is the whole answer for an ordinary receiver.
/// It is not the answer for a wrapper of a wrapper —
/// `Collections.unmodifiableList(List.of("a", "b"))` backs one
/// `cratonvm/internal/UnmodifiableList` stamp with another, and every stamp
/// declares only its FAMILY-level interfaces (`List`, `Collection`,
/// `Serializable`), never `RandomAccess`, because that marker is a property of
/// what the instance wraps. Asking the inner stamp's own hierarchy answers
/// `false` for `List.of`, whose HotSpot display class
/// (`ImmutableCollections$ListN`, via `AbstractImmutableList`) implements it.
///
/// So descend slot 0 (`UNMOD_FIELD_BACKING`): an unmodifiable view carries the
/// marker exactly when the thing it wraps does.
///
/// This function is one half of a PAIR and must not move alone. The interpreter
/// runs the identical rule in `interpreter::typecheck::object_reaches`, because
/// the `instanceof`/`checkcast` opcodes cannot call in here — the receiver is a
/// bare Rust local at that point and this side may load classes and safepoint.
/// The two answer the same question about the same object through different
/// machinery, and when they disagree the VM reports one class from `getClass()`
/// and admits a different one at a cast. Change either and change both;
/// `apps/probes/RandomAccessProbe` prints all three doors per row and an
/// `agree=` column that goes false the moment they part.
pub fn getclass_object_reaches(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    target: cratonvm_types::ClassId,
    depth: usize,
) -> bool {
    // A wrapper chain is a handful of links; the bound is a cycle guard, not a
    // policy. (`alloc_unmod_wrapper` stores an object that already existed, so
    // slot-0 nesting is acyclic by construction — this is insurance.)
    if depth > 8 {
        return false;
    }
    let cid = ctx.class_id_of_object(obj);
    if ctx.is_subclass(cid, target) {
        return true;
    }
    let is_stamp = ctx
        .class_name_of_id(cid)
        .is_some_and(|n| n.starts_with("cratonvm/internal/Unmodifiable"));
    if !is_stamp {
        return false;
    }
    match ctx.get_field(obj, 0) {
        Value::Object(Some(inner)) => getclass_object_reaches(ctx, inner, target, depth + 1),
        _ => false,
    }
}

/// Resolve the concrete display `ClassId` for an object stamped with `class_id`,
/// or `None` to use the stamp unchanged. The stamp→strategy classification is
/// memoised; the size-dependent collection forms are resolved per-object.
pub fn getclass_display_class_id(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    this: ObjectRef,
) -> Option<cratonvm_types::ClassId> {
    let memo_key = (ctx.vm_identity(), class_id);
    let display = match GETCLASS_ALIAS_CACHE.with(|c| c.borrow().get(&memo_key).copied()) {
        Some(d) => d,
        None => {
            // Cache miss: classify once. `Stamp` is cached too (negative cache)
            // so the common non-synthetic class never re-clones its name.
            let d = match ctx.class_name_of_id(class_id) {
                Some(name) => {
                    if let Some(kind) = collection_display_kind(&name) {
                        kind
                    } else if let Some(aliases) = jdk_concrete_getclass_alias(&name) {
                        // JIT r20 wave 12 (lane qMisc, NIO20-L3-1): the
                        // compiled fold's predicate reads the alias rows
                        // through this list of their keys.
                        debug_assert!(
                            GETCLASS_ALIAS_SOURCES.contains(&name.as_str()),
                            "getClass() alias stamp {name} is not listed in GETCLASS_ALIAS_SOURCES"
                        );
                        // The first candidate the image has AND that `new`
                        // could legally produce. Trusting the NAME is what put
                        // an ABSTRACT class in two of these rows; see the
                        // table's own comment and
                        // `cratonvm_native_api::instantiable`.
                        match cratonvm_native_api::instantiable::first_instantiable(ctx, aliases) {
                            // Round 18 wave 10 (lane streams5): the stream
                            // stamps are per object (`StreamStage`).
                            Some((_, cid))
                                if cratonvm_native_collections::is_synthetic_stream_class_name(
                                    &name,
                                ) =>
                            {
                                GetClassDisplay::StreamStage(cid)
                            }
                            // nio21 wave 14 (lane L3): the synthetic
                            // `FileSystem` is the default AND the jar / jrt
                            // mounts; per object (`FileSystemMount`).
                            Some((_, cid))
                                if (name == "java/nio/file/FileSystem"
                                    || name == "java/nio/file/spi/FileSystemProvider") =>
                            {
                                GetClassDisplay::FileSystemMount(cid)
                            }
                            Some((_, cid)) => GetClassDisplay::Fixed(cid),
                            None => GetClassDisplay::Stamp,
                        }
                    } else {
                        GetClassDisplay::Stamp
                    }
                }
                None => GetClassDisplay::Stamp,
            };
            GETCLASS_ALIAS_CACHE.with(|c| c.borrow_mut().insert(memo_key, d));
            d
        }
    };
    match display {
        GetClassDisplay::Stamp => None,
        GetClassDisplay::Fixed(cid) => Some(cid),
        GetClassDisplay::FileSystemMount(platform) => {
            Some(getclass_fs_mount_display(ctx, this).unwrap_or(platform))
        }
        GetClassDisplay::StreamStage(head) => {
            match cratonvm_native_collections::synthetic_stream_stage_display_name(&*ctx, this) {
                // An image without the stage class keeps the `Head` alias.
                Some(name) => getclass_resolve_display(ctx, name).or(Some(head)),
                None => Some(head),
            }
        }
        // Immutable (marker set): HotSpot picks by size — `List.of()`→ListN,
        // size 1–2→List12, ≥3→ListN; Set mirrors List; Map uses Map1 only for a
        // single entry. Unmodifiable (no marker): the `Collections$Unmodifiable*`
        // family — lists split on `RandomAccess`.
        GetClassDisplay::CollList => {
            let name = if getclass_immutable_marker(ctx, this) {
                let n = getclass_collection_size(ctx, this);
                if (1..=2).contains(&n) {
                    "java/util/ImmutableCollections$List12"
                } else {
                    "java/util/ImmutableCollections$ListN"
                }
            } else if getclass_backing_is_random_access(ctx, this) {
                "java/util/Collections$UnmodifiableRandomAccessList"
            } else {
                "java/util/Collections$UnmodifiableList"
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::CollSet => {
            let name = if getclass_immutable_view_marker(ctx, this) {
                // `Map.of(...).keySet()` — AbstractMap's own default view, at
                // every size. Display-only fix; H0-2/H4-1's carrier-retag
                // remedy is about `instanceof`, not `getClass()`, and does not
                // apply here (see `UNMOD_FIELD_IMMUTABLE_VIEW`'s own doc
                // comment).
                "java/util/AbstractMap$1"
            } else if getclass_immutable_marker(ctx, this) {
                let n = getclass_collection_size(ctx, this);
                if (1..=2).contains(&n) {
                    "java/util/ImmutableCollections$Set12"
                } else {
                    "java/util/ImmutableCollections$SetN"
                }
            } else {
                "java/util/Collections$UnmodifiableSet"
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::CollEntrySet => {
            let name = if getclass_immutable_view_marker(ctx, this) {
                let n = getclass_collection_size(ctx, this);
                if n == 1 {
                    "java/util/ImmutableCollections$Set12"
                } else {
                    "java/util/ImmutableCollections$MapN$1"
                }
            } else {
                // `Collections.unmodifiableMap(m).entrySet()` — HotSpot's
                // `entrySet()` view is its own nested class, distinct from the
                // plain `UnmodifiableSet` that `keySet()`/`values()` share.
                // MEASURED against the oracle, `H18-2` §3: was
                // `Collections$UnmodifiableSet` here, which is wrong in a way
                // `keySet()` and `values()` are not (they really do share
                // HotSpot's `UnmodifiableSet`).
                "java/util/Collections$UnmodifiableMap$UnmodifiableEntrySet"
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::CollSortedSet => {
            getclass_resolve_display(ctx, "java/util/Collections$UnmodifiableSortedSet")
        }
        GetClassDisplay::CollNavigableSet => {
            getclass_resolve_display(ctx, "java/util/Collections$UnmodifiableNavigableSet")
        }
        GetClassDisplay::CollMap => {
            let name = if getclass_immutable_marker(ctx, this) {
                let n = getclass_collection_size(ctx, this);
                if n == 1 {
                    "java/util/ImmutableCollections$Map1"
                } else {
                    "java/util/ImmutableCollections$MapN"
                }
            } else {
                "java/util/Collections$UnmodifiableMap"
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::CollUnmod => {
            let name = if getclass_immutable_view_marker(ctx, this) {
                // `Map.of(...).values()` — AbstractMap's own default view.
                "java/util/AbstractMap$2"
            } else {
                "java/util/Collections$UnmodifiableCollection"
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::Spliterator => {
            let name = match getclass_spliterator_display_hint(ctx, this) {
                1 => "java/util/Collections$2",
                2 => "java/util/AbstractList$RandomAccessSpliterator",
                _ => "java/util/Spliterators$IteratorSpliterator",
            };
            getclass_resolve_display(ctx, name)
        }
        GetClassDisplay::ArrayListItr => {
            if getclass_al_itr_backing_is_vector(ctx, this) {
                getclass_resolve_display(ctx, "java/util/Vector$Itr")
            } else {
                Some(class_id)
            }
        }
        GetClassDisplay::CollListItr => {
            // Slot indices mirror `native-collections::UNMOD_LIST_ITR_IMMUTABLE`
            // / `UNMOD_LIST_ITR_VIA_LIST_ITERATOR` — see that pair's own doc
            // comment for why the carrier needs them at all.
            let immutable =
                ctx.object_num_fields(this) > 2 && matches!(ctx.get_field(this, 2), Value::Int(1));
            let name = if immutable {
                "java/util/ImmutableCollections$ListItr"
            } else if ctx.object_num_fields(this) > 3
                && matches!(ctx.get_field(this, 3), Value::Int(1))
            {
                "java/util/Collections$UnmodifiableList$1"
            } else {
                "java/util/Collections$UnmodifiableCollection$1"
            };
            getclass_resolve_display(ctx, name)
        }
    }
}

/// Whether an `ArrayList$Itr` carrier's backing list (`this$0`) is a
/// `java.util.Vector` (or its `Stack` subclass) rather than a plain
/// `ArrayList` — see [`GetClassDisplay::ArrayListItr`].
pub fn getclass_al_itr_backing_is_vector(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let Some(slot) = ctx.resolve_field_index("java/util/ArrayList$Itr", "this$0") else {
        return false;
    };
    let backing = match ctx.get_field(this, slot) {
        Value::Object(Some(b)) => b,
        _ => return false,
    };
    let Some(vec_cid) = getclass_resolve_name(ctx, "java/util/Vector") else {
        return false;
    };
    ctx.is_subclass(ctx.class_id_of_object(backing), vec_cid)}

pub fn native_object_get_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("getClass on null".to_string()),
            }
            .into());
        }
    };

    if dbg_toarray_enabled() {
        eprintln!(
            "[DBG_TOARRAY] native_object_get_class ENTER kind={:?} cid={:?} len={}",
            ctx.heap_kind_of(this),
            ctx.class_id_of_object(this),
            if ctx.heap_kind_of(this) == cratonvm_types::ObjectKind::Array {
                ctx.array_length(this) as i64
            } else {
                -1
            }
        );
    }
    // For array objects, build a class mirror with the correct array type name
    // (e.g., "[I" for int[], "[Ljava/lang/String;" for String[]).
    // Primitive arrays use ClassId(0) + element_type; reference arrays store
    // the component ClassId in the header.
    if ctx.heap_kind_of(this) == cratonvm_types::ObjectKind::Array {
        let class_id = ctx.class_id_of_object(this);
        let element_type = ctx.heap_element_type_of(this);
        // The name computation moved to `lang_class::array_class_internal_name`
        // (G69-1) so the reflection messages can ask the same question and get
        // the same answer -- they were asking `class_name_of_id` instead, which
        // on an array answers the COMPONENT's name. Still `Cow`, so the eight
        // primitive branches still do not allocate.
        let _ = element_type;
        // An array of a lambda proxy: the proxy is virtual (no class file, no store entry), so
        // its array class can only be synthesised once the class manager knows the proxy's name
        // (SBR5 walker). A no-op, and a one-compare reject, for every other component.
        if element_type == cratonvm_types::ArrayElementType::Reference
            && class_id.as_u32() >= 0x8000_0000
        {
            let _ = crate::lang_class::register_lambda_array_component(ctx, class_id);
        }
        let array_class_name = crate::lang_class::array_class_internal_name(ctx, this);
        // Create a class mirror with the array type name.  We need a real
        // (non-primitive) class mirror so that `Class.isPrimitive()` returns
        // false and the identity matches the `Foo[].class` literal — both
        // are required by Spring's `ClassUtils.resolvePrimitiveIfNecessary`
        // (called via `TypeDescriptor.getObjectType`) which otherwise reads
        // `isPrimitive()==true` for an `Object[]` mirror produced here and
        // returns `null`, surfacing as `NPE: Cannot invoke isInstance on
        // null` inside `GenericConversionService.convert`.
        if dbg_toarray_enabled() {
            eprintln!(
                "[DBG_TOARRAY] getClass array_class_name={:?} comp_cid={:?} arr_len={}",
                array_class_name,
                class_id,
                ctx.array_length(this)
            );
        }
        // Array ClassIds are deliberately interned globally, but an array
        // object's header preserves its *actual* reference-component ClassId.
        // Do not collapse that identity when exposing `getClass()` for an array
        // whose component was defined by an isolated loader: the subsequent
        // `getComponentType()` / `Array.newInstance()` path must retain the
        // same component, or Java's perfectly valid arraycopy covariance check
        // observes two same-named classes as incompatible.
        if element_type == cratonvm_types::ArrayElementType::Reference {
            // Strict mode: the class manager's own per-loader array class (the
            // one `X[].class`, `arrayType()` and a member's declared type
            // resolve to), not a descriptor-backed stand-in.
            if let Some(arr_cid) =
                crate::lang_class::strict_array_class_id(ctx, class_id, &array_class_name)
            {
                let mirror = ctx.get_class_mirror(arr_cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
        if element_type == cratonvm_types::ArrayElementType::Reference
            && ctx.loader_id_of_class(class_id) >= 3
        {
            if let Some(loader) =
                crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())
            {
                let mirror = crate::lang_class::synthetic_class_mirror_for_loader(
                    ctx,
                    &array_class_name,
                    loader,
                );
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
        if let Some(arr_cid) = ctx.class_id_by_name(&array_class_name) {
            let mirror = ctx.get_class_mirror(arr_cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
        let _ = ctx.load_class(&array_class_name);
        if let Some(arr_cid) = ctx.class_id_by_name(&array_class_name) {
            let mirror = ctx.get_class_mirror(arr_cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // Last-resort fallback (e.g. degenerate names that fail to synthesise):
        // keep the old behaviour so we never return a null mirror.
        let mirror = ctx.primitive_class_mirror(&array_class_name);
        return Ok(Some(Value::Object(Some(mirror))));
    }

    let class_id = ctx.class_id_of_object(this);
    // Synthetic objects are sometimes stamped with the interface/abstract/
    // internal type they stand in for; a real JVM always reports a concrete
    // runtime class. Re-map to a JDK-plausible concrete class for the mirror
    // (memoised; no-op for ordinary classes). Storage/dispatch are unaffected.
    let display_id = getclass_display_class_id(ctx, class_id, this).unwrap_or(class_id);
    // zr z4/ab: a mirror minted lazily on a full heap collects first, as an
    // allocation does. `this` is dead from here on, so the native holds no
    // heap reference across the collection (`reclaim_before_alloc_retry`'s
    // precondition). Only a context that overrides `try_get_class_mirror`
    // can answer `None` (the VM: ZGC since zr z4/ab, Generational and G1
    // since gci i22/oo, `CRATONVM_GC_NATIVE_MIRROR_RECLAIM`).
    let mirror = match ctx.try_get_class_mirror(display_id) {
        Some(mirror) => mirror,
        None => {
            let _ = ctx.reclaim_before_alloc_retry();
            match ctx.try_get_class_mirror(display_id) {
                Some(mirror) => mirror,
                None => {
                    // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
                    let _ = ctx.reclaim_after_failed_alloc_retry();
                    ctx.get_class_mirror(display_id)
                }
            }
        }
    };
    Ok(Some(Value::Object(Some(mirror))))
}

// ---------------------------------------------------------------------------
// `Object.toString` per-class dotted-name cache (perf)
//
// `Object.toString` is one of the hottest natives in the VM (every implicit
// string concatenation against a non-overriding class hits it). The naive
// implementation allocated four times per call: `class_name_of_id`, the
// `replace('/', ".")` of the slashed name, the `format!` of the
// `"dotted@hash"` text, and finally `create_string`.
//
// We mirror the cache strategy in `lang_class.rs::dotted_class_name`: the
// slashed → dotted derivation is pure (class names don't change once the
// class is loaded), so we memoise the `Arc<str>` keyed by `ClassId`. After
// the first call per class only one allocation remains (the `format!` for
// the per-object `dotted@hash` text — unavoidable because the hash varies).
// ---------------------------------------------------------------------------
pub fn object_to_string_dotted_name(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> std::sync::Arc<str> {
    use parking_lot::RwLock;
    use rustc_hash::FxHashMap;
    use std::sync::{Arc, OnceLock};

    // Keyed by `(vm_identity, class_id)`: class ids restart at zero in every
    // VM, and a test binary has several live at once — see the
    // `ClassNameCache` note in `lang_class`.
    static CACHE: OnceLock<RwLock<FxHashMap<(usize, u32), Arc<str>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(FxHashMap::default()));
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some(arc) = cache.read().get(&key).cloned() {
        return arc;
    }
    // Miss: derive the dotted form (single `replace` or `Arc::from` clone if
    // the name has no `/`) and insert under the write lock.
    // Lambda proxies first, for the reason spelled out on the `Object.toString`
    // closure above: they are absent from the class store, so the by-id lookup
    // below cannot name them. `java/lang/Object.toString` is registered TWICE in
    // this crate (here via `native_object_to_string`, and as a closure earlier in
    // the file); whichever registrar runs last wins, and the two used to render an
    // unknown class differently -- "?" here against "" there. Keeping the lambda
    // case identical in both means the answer no longer depends on that order.
    if let Some(lambda_name) = crate::lang_class::lambda_proxy_class_name(ctx, class_id) {
        // Already dotted, and its `/0x<id>` tail must survive verbatim.
        let arc: Arc<str> = Arc::from(lambda_name);
        cache.write().insert(key, Arc::clone(&arc));
        return arc;
    }
    let slashed = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "?".to_string());
    let dotted: Arc<str> = if slashed.contains('/') {
        // Keeps a hidden class's `/0x<hex>` tail (see the twin closure above).
        Arc::from(crate::lang_class::dotted_binary_name(&slashed))
    } else {
        Arc::from(slashed)
    };
    cache.write().insert(key, Arc::clone(&dotted));
    dotted
}

pub fn native_object_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::fmt::Write as _;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("toString on null".to_string()),
            }
            .into());
        }
    };
    let class_id = ctx.class_id_of_object(this);
    // JIT round 19 wave 3, lane funnel3 (NIO2-17-5 rung 5,
    // `CRATONVM_CLASS_FACTS_MEMO_CARRIERS=1`, default OFF): the Tomcat carrier
    // test from the per-thread class-facts memo instead of the class-manager
    // lock and an `Arc` bump per call.
    let message_bytes = match crate::class_facts_memo::rung_facts(
        ctx,
        crate::class_facts_memo::Rung::Carriers,
        class_id,
    ) {
        Some(facts) => facts.is_some_and(crate::class_facts_memo::NameFacts::is_message_bytes),
        None => {
            ctx.class_name_arc_of_id(class_id).as_deref()
                == Some("org/apache/tomcat/util/buf/MessageBytes")
        }
    };
    if message_bytes {
        return native_message_bytes_to_string(ctx, args);
    }
    // Keep the default `toString` class name consistent with `getClass()`:
    // synthetic objects stamped with an interface/abstract/internal class
    // report their concrete JDK display class here too (memoised; no-op for
    // ordinary classes).
    let class_id = getclass_display_class_id(ctx, class_id, this).unwrap_or(class_id);
    let dotted = object_to_string_dotted_name(ctx, class_id);
    // JDK `Object.toString` renders `Integer.toHexString(hashCode())`, a VIRTUAL
    // call — a bytecode `hashCode()` override must show through here (see the twin
    // fix in the `Object.toString` registration near the top of this file for the
    // CGLIB proxy-cache rationale). Non-overriding receivers dispatch to the
    // identity native, so their rendering is unchanged.
    let hash = match ctx.invoke_virtual(this, "hashCode", "()I", &[]) {
        Ok(Some(Value::Int(h))) => h,
        _ => ctx.identity_hash_code(this),
    };
    // Build "dotted@hash" with a single pre-sized buffer (saves the realloc
    // that `format!` would do, but more importantly limits us to one heap
    // alloc for this call — the per-object hash means we cannot cache this).
    // `hash` is i32 → up to 8 hex chars; plus '@' separator.
    let mut text = String::with_capacity(dotted.len() + 9);
    text.push_str(&dotted);
    let _ = write!(text, "@{hash:x}");
    let str_ref = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(str_ref))))
}

pub fn native_object_equals(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Default equals: reference identity
    let this = args.first().copied().unwrap_or(Value::Object(None));
    let other = args.get(1).copied().unwrap_or(Value::Object(None));
    let equal = match (this, other) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => a.as_ptr() == b.as_ptr(),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    };
    if dbg_obj_equals_cached() {
        let aid = match this {
            Value::Object(Some(o)) => o.as_ptr() as usize,
            _ => 0,
        };
        let bid = match other {
            Value::Object(Some(o)) => o.as_ptr() as usize,
            _ => 0,
        };
        eprintln!("[obj-eq] a={aid:x} b={bid:x} -> {equal}");    }
    Ok(Some(Value::Int(if equal { 1 } else { 0 })))}

/// Cached `CRATONVM_DBG_OBJ_EQUALS` check -- `Object.equals()`'s default
/// (identity) implementation is exactly as hot as `Object.hashCode()`
/// (see `dbg_vdisp_cached` above, both fire together on every default-
/// identity `HashMap`/`HashSet` operation); same fix, same reasoning.
pub fn dbg_obj_equals_cached() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| crate::nbflags().dbg_obj_equals)
}

/// Cached `CRATONVM_DBG_CLONE` check -- `Object.clone()` is far less hot
/// than hashCode()/equals() but gets the same treatment for consistency
/// (this whole `CRATONVM_DBG_*`-in-a-frequently-called-native family was
/// worth a single pass once the hashCode()/equals() cases were found to be
/// a real, severe performance bug).
pub fn dbg_clone_cached() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| crate::nbflags().dbg_clone)
}

/// Does `class_id` -- or any of its superclasses, or any interface either of
/// them implements, transitively -- implement `java.lang.Cloneable`?
///
/// Written as an explicit walk rather than routed through an assignability
/// helper because the two facts it needs are the two the `NativeContext` trait
/// actually exposes: the superclass chain and the DECLARED interfaces at each
/// level. `Cloneable` is a marker with no methods, so there is nothing else
/// about it to ask. The worklist is transitive because an interface may extend
/// another, and `Cloneable` is often reached that way rather than declared.
pub fn object_class_is_cloneable(ctx: &dyn NativeContext, class_id: cratonvm_types::ClassId) -> bool {
    let Some(cloneable) = ctx.class_id_by_name("java/lang/Cloneable") else {
        // No `Cloneable` in this image at all -- refusing every clone would be
        // far worse than allowing them, so keep the pre-2026-08-29 behaviour.
        return true;
    };
    let mut seen: Vec<cratonvm_types::ClassId> = Vec::new();
    let mut work: Vec<cratonvm_types::ClassId> = Vec::new();
    let mut cls = Some(class_id);
    while let Some(c) = cls {
        work.push(c);
        if work.len() > 256 {
            break;
        }
        cls = ctx.superclass_of(c);
    }
    while let Some(c) = work.pop() {
        if c == cloneable {
            return true;
        }
        if seen.contains(&c) {
            continue;
        }
        seen.push(c);
        if seen.len() > 512 {
            break;
        }
        for i in ctx.class_interfaces(c) {
            if i == cloneable {
                return true;
            }
            if !seen.contains(&i) {
                work.push(i);
            }
        }
    }
    false
}

/// gcf f7/o: `Object.clone()` as the funnel calls it. The array copy's
/// allocation, refused on a full heap, collects once
/// (`reclaim_before_alloc_retry`) and retries, as HotSpot's `JVM_Clone`
/// allocation does: `Throwable.getStackTrace()`'s `clone()` was the first
/// allocation after a program dropped its data, and with one non-collecting
/// attempt it threw an `OutOfMemoryError` outside every `catch`
/// (`gcf-f5o-a-vm-raised-oome-whose-construction-fails-escapes-every-catch`).
/// Only this registered entry may collect: it holds nothing but the pinned
/// receiver. Nested Rust callers use [`native_object_clone`] (one attempt).
pub fn native_object_clone_reclaiming(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cloned = object_clone_body(ctx, args, true);
    vm_object_alloc_event(ctx, cloned)
}

/// JVMTI `VMObjectAlloc` of the object a VM-allocating native answers
/// (`NativeSystemAccess::jvmti_vm_object_alloc`; interpreter round i2 wave
/// 63, lane L1): `Object.clone`, `Array.newInstance`, a reflective
/// constructor, `Unsafe.allocateInstance`, where HotSpot posts it. The answer
/// carries the object where it is after an agent's callback.
pub fn vm_object_alloc_event(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
) -> MethodCallResult {
    match result {
        Ok(Some(Value::Object(Some(obj)))) => {
            Ok(Some(Value::Object(Some(ctx.jvmti_vm_object_alloc(obj)))))
        }
        other => other,
    }
}

/// [`vm_object_alloc_event`] of a multi-dimensional array and, depth first,
/// of every array it holds, as HotSpot posts `Array.newInstance(int.class, 2,
/// 2)`'s three (MEASURED: `[[I`, `[I`, `[I`).
pub fn vm_object_alloc_event_nested(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
) -> MethodCallResult {
    let Ok(Some(Value::Object(Some(outer)))) = result else {
        return result;
    };
    Ok(Some(Value::Object(Some(post_nested_arrays(ctx, outer)))))
}

pub fn post_nested_arrays(ctx: &mut dyn NativeContext, arr: ObjectRef) -> ObjectRef {
    let arr = ctx.jvmti_vm_object_alloc(arr);
    if ctx.heap_element_type_of(arr) != cratonvm_types::ArrayElementType::Reference {
        return arr;
    }
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    for i in 0..len {
        let current = ctx.read_native_pin(pin, arr);
        if let Value::Object(Some(inner)) = ctx.get_array_element(current, i) {
            if ctx.object_is_array(inner) {
                post_nested_arrays(ctx, inner);
            }
        }
    }
    let arr = ctx.read_native_pin(pin, arr);
    ctx.unpin_native_roots(pin);
    arr
}

/// The clone body for Rust callers that may hold unpinned references (no
/// collection inside).
pub fn native_object_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    object_clone_body(ctx, args, false)
}

/// One fallible attempt at the array copy, then (only when the caller may
/// collect) the shared native reclaim and one more attempt. `None` = refused.
pub fn clone_array_with_reclaim(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    et: cratonvm_types::ArrayElementType,
    len: usize,
) -> Option<ObjectRef> {
    let attempt = |ctx: &mut dyn NativeContext| {
        if et == cratonvm_types::ArrayElementType::Reference {
            ctx.try_new_ref_array(class_id, len)
        } else {
            ctx.try_new_array(et, len)
        }
    };
    if let Some(clone) = attempt(ctx) {
        return Some(clone);
    }
    if !ctx.reclaim_before_alloc_retry() {
        return None;
    }
    if let Some(clone) = attempt(ctx) {
        return Some(clone);
    }
    // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung, then retries once more.
    if !ctx.reclaim_after_failed_alloc_retry() {
        return None;
    }
    attempt(ctx)
}

/// The side-table overlay a clone of a class must replicate (the
/// `LinkedHashMap` / `Properties` arms of [`object_clone_body`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloneOverlay {
    None,
    LinkedHashMap,
    Properties,
}

/// The first of `java/util/LinkedHashMap`, `java/util/Properties` on the
/// superclass chain of `class_id` (stopping at `java/lang/Object` or an
/// unnamed class). Pure reads; the walk [`object_clone_body`] has always made.
pub fn object_clone_overlay_of(ctx: &dyn NativeContext, class_id: ClassId) -> CloneOverlay {
    let mut cur = class_id;
    loop {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/util/LinkedHashMap" => return CloneOverlay::LinkedHashMap,
            Some(n) if n == "java/util/Properties" => return CloneOverlay::Properties,
            Some(n) if n == "java/lang/Object" => return CloneOverlay::None,
            None => return CloneOverlay::None,
            _ => match ctx.superclass_of(cur) {
                Some(p) => cur = p,
                None => return CloneOverlay::None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 75, lane L4: `Object.clone` answers its per-class
// questions once per class (`CRATONVM_OBJECT_CLONE_CLASS_FACTS`, default off)
// ---------------------------------------------------------------------------
//
// Every plain-object `clone()` asked two questions of its CLASS before and
// after the copy: is it `Cloneable` (`object_class_is_cloneable`: a by-name
// lookup of `Cloneable`, a superclass walk into a `Vec`, a worklist over every
// level's interfaces) and does it carry a side-table overlay
// (`object_clone_overlay_of`: an owned `String` per class up to `Object`). Both
// are facts of the class, fixed once it is loaded (redefinition cannot change
// the hierarchy). Behind the switch a per-thread, direct-mapped memo keyed by
// (VM identity, collection epoch, `ClassId`) answers them; a miss runs the
// same two walks and files their answers. A `ClassId` is reused only after an
// unload, which happens in a collection and so ends the epoch. Every write of
// the copy, the `CloneNotSupportedException` and its text, the finalizer
// registration and the overlay copies are unchanged; only the walks are
// skipped on a hit. Under `--real-jdk` every `Lookup.find*` clones one
// `MemberName` (`MemberName.Factory.resolve`). Proposal
// `i74-L4-proposal-object-clone-answers-its-class-questions-once-20261107`;
// bench `L4W75CloneClassFactsBench.java`.

/// Lines of [`CLONE_CLASS_FACTS_MEMO`] (a power of two).
pub const CLONE_CLASS_FACTS_SLOTS: usize = 64;

/// `CRATONVM_OBJECT_CLONE_CLASS_FACTS` -- performance, default OFF,
/// `truthy_word` (see the section comment).
pub fn object_clone_class_facts_enabled() -> bool {
    nbflags().object_clone_class_facts
}

/// What a plain-object clone asks of its class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CloneClassFacts {
    pub cloneable: bool,
    pub overlay: CloneOverlay,
}

#[derive(Clone, Copy)]
pub struct CloneClassFactsLine {
    pub vm: usize,
    pub epoch: u64,
    pub class_id: ClassId,
    pub facts: CloneClassFacts,
}

thread_local! {
    /// Per-thread memo of [`CloneClassFacts`] (see the section comment).
    pub static CLONE_CLASS_FACTS_MEMO: std::cell::RefCell<[Option<CloneClassFactsLine>; CLONE_CLASS_FACTS_SLOTS]> =
        const { std::cell::RefCell::new([None; CLONE_CLASS_FACTS_SLOTS]) };
}

pub fn clone_class_facts_slot(class_id: ClassId) -> usize {
    let h = (class_id.as_u32() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (h >> 58) as usize & (CLONE_CLASS_FACTS_SLOTS - 1)
}

/// The filed facts of `class_id` in VM `vm` and epoch `epoch`.
pub fn clone_class_facts_get(vm: usize, epoch: u64, class_id: ClassId) -> Option<CloneClassFacts> {
    if vm == 0 {
        return None;
    }
    CLONE_CLASS_FACTS_MEMO.with(|m| match m.borrow()[clone_class_facts_slot(class_id)] {
        Some(l) if l.vm == vm && l.epoch == epoch && l.class_id == class_id => Some(l.facts),
        _ => None,
    })
}

pub fn clone_class_facts_put(vm: usize, epoch: u64, class_id: ClassId, facts: CloneClassFacts) {
    if vm == 0 {
        return;
    }
    CLONE_CLASS_FACTS_MEMO.with(|m| {
        m.borrow_mut()[clone_class_facts_slot(class_id)] = Some(CloneClassFactsLine {
            vm,
            epoch,
            class_id,
            facts,
        });
    });
}

/// The facts of `class_id`, from the memo or the two walks. Pure reads; no GC
/// point. A class whose image has no `Cloneable` (the walk's permissive
/// answer) is not filed.
pub fn object_clone_class_facts(ctx: &dyn NativeContext, class_id: ClassId) -> CloneClassFacts {
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    if let Some(facts) = clone_class_facts_get(vm, epoch, class_id) {
        return facts;
    }
    let facts = CloneClassFacts {
        cloneable: object_class_is_cloneable(ctx, class_id),
        overlay: object_clone_overlay_of(ctx, class_id),
    };
    if ctx.class_id_by_name("java/lang/Cloneable").is_some() {
        clone_class_facts_put(vm, epoch, class_id, facts);
    }
    facts
}


pub fn object_clone_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    may_reclaim: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("clone on null".to_string()),
            }
            .into());
        }
    };
    let class_id = ctx.class_id_of_object(this);
    let kind = ctx.heap_kind_of(this);
    // `CRATONVM_OBJECT_CLONE_CLASS_FACTS` (wave 75, L4): both class questions
    // below from a per-thread memo; `None` = asked by their walks, as before.
    let facts = if kind == cratonvm_types::ObjectKind::Object && object_clone_class_facts_enabled()
    {
        Some(object_clone_class_facts(&*ctx, class_id))
    } else {
        None
    };
    // `Object.clone()` is `throw new CloneNotSupportedException()` for a
    // receiver whose class does not implement `Cloneable`. Arrays are always
    // cloneable and are handled below; every other kind has to be asked.
    //
    // Without this, a subclass of `Object` that reaches `super.clone()` got a
    // COPY where the JDK gives an exception -- a fabricated success, and the
    // kind that only shows up much later as two objects where the program
    // expects one. MEASURED by `SystemRuntimeObjectSweep.java`.
    if kind == cratonvm_types::ObjectKind::Object
        && !facts.map_or_else(
            || object_class_is_cloneable(&*ctx, class_id),
            |f| f.cloneable,
        )
    {
        let name = ctx
            .class_name_of_id(class_id)
            .unwrap_or_default()
            .class_text(&*ctx);
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/CloneNotSupportedException",
            &name,
        ));
    }
    if dbg_clone_cached() {
        let name = ctx
            .class_name_of_id(class_id)
            .unwrap_or_else(|| "<unknown>".to_string());
        eprintln!("[DBG_CLONE] kind={:?} class={}", kind, name);
    }
    match kind {
        cratonvm_types::ObjectKind::Object => {
            let num_fields = ctx.object_num_fields(this);
            // gc-common w20-c: the clone's allocation can collect and move
            // the original, which is read (every field, then the LHM /
            // Properties side state) after it; re-read it from a pin.
            let this_pin = ctx.pin_native_root(this);
            let clone_ref = ctx.alloc_object(class_id, num_fields);
            let this: ObjectRef = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            // G52: THIS LOOP COERCES EVERY FIELD TWICE, AND THAT IS NOT A
            // CHOICE THIS SITE MADE. Read this before "optimising" it.
            //
            // `NativeContextImpl::get_field` and `::set_field`
            // (`vm/src/vm/vm_exec.rs`) both resolve the slot's declared
            // descriptor and route through `get_field_as`/`set_field_as`,
            // i.e. through `gc/src/heap.rs::coerce_field_value_for_slot`
            // (T10.9.E, added after this loop was written). So a slot whose
            // raw contents disagree with the class's declared type is
            // coerced on the way out of the original AND on the way into the
            // clone.
            //
            // `Object.clone()` is specified as a verbatim field copy, so the
            // obvious question is why this is not one. The answer is that a
            // verbatim slot copy IS NOT EXPRESSIBLE from a native today:
            // `NativeContext` exposes no raw slot-INDEXED accessor. The only
            // raw pair it has is `get_field_by_name`/`set_field_by_name`,
            // which cannot be driven from a slot index and would resolve a
            // shadowed field name to the wrong slot. NOMINATED in
            // `docs/feature-designs/native-dispatch-architecture.md`.
            //
            // Why the double coercion is nonetheless harmless: the coercion
            // is IDEMPOTENT, pinned by
            // `gc/src/heap.rs::the_descriptor_coercion_is_idempotent` over
            // every (variant, descriptor) pair. The clone therefore holds
            // exactly the original's coerced contents; the second pass can
            // never compound the first.
            //
            // MEASURED 2026-08-17, `--real-jdk`, `CRATONVM_DBG_COERCION=1`
            // + `CRATONVM_DBG_CLONE=1` on `9ae371468`, and it CONTRADICTS the
            // standing "clone amplifies whatever the original got wrong"
            // reading of `G45-1` §3's 54-event cluster:
            //
            // * `RJdkSecurity`: 4,534 clones, of which 4,533 are ARRAYS and
            //   take the `ObjectKind::Array` arm below — which uses
            //   `get_array_element`/`set_array_element` and never touches
            //   this path at all. ONE object clone, 3 events.
            // * `RSerial`: 29 clones, 12 objects, all
            //   `java/lang/invoke/MemberName`, 21 events.
            // * ALL 24 events are the same thing: `species=
            //   primitive-into-reference descriptor=L value=Int(0)` — a READ
            //   of a slot that was never descriptor-initialised, which under
            //   `heap.rs`'s R-niche decode rule reads back as `Int(0)`. The
            //   store half fires nothing, because the read already answered
            //   `Object(None)`.
            // * `CRATONVM_DBG_OVERLAY=1` names the slots: `MemberName` 4 and
            //   5, which `javap -p` on HotSpot 25.0.3+9 gives as
            //   `method:ResolvedMethodName` and `resolution:Object` — both
            //   legitimately null on an unresolved `MemberName`.
            //
            // So on 100% of the measured population this loop does not
            // amplify a defect, it NORMALISES one: the original keeps a
            // mis-tagged raw zero, the clone gets a properly tagged null.
            //
            // The hazard that WOULD be amplification, for whoever measures a
            // non-zero count of it: a slot declared primitive that holds a
            // live `Object(Some(_))` — the `pointer-into-primitive` species,
            // 207 events in `RJdkSecurity` from
            // `jca/provider_chain.rs`. Cloning such an object writes
            // `Int(ptr as i32)` into the clone: a truncated, stale address
            // that no collector will remap, and a reference dropped. It has
            // never fired here because `Provider.clone()` dispatches to the
            // `java/util/Hashtable.clone` bridge rather than to this native.
            // G56-1 NOMINATION 3, landed after NOMINATION 2 (the
            // `NativeContext::set_field_raw` override) as that record
            // requires: `Object.clone()` is specified as a verbatim field
            // copy, and now that both halves of the raw slot-indexed pair are
            // genuinely raw (`get_field_raw`/`set_field_raw`,
            // `native-api/src/registry.rs`), this loop no longer has to
            // coerce twice to get one. The hazard the double coercion was
            // silently correcting for a `pointer-into-primitive` receiver
            // (`Int(ptr as i32)` copied byte-for-byte instead of dropped) is
            // now handled correctly by construction -- a verbatim copy
            // carries the same bits either way -- rather than by an accident
            // of the old loop happening to coerce it away.
            //
            // The raw pair now runs inside `copy_fields_into_clone`, whose
            // production override validates both objects once for the whole
            // body (a 14-field clone was ~4.5 us slot by slot).
            ctx.copy_fields_into_clone(this, clone_ref, num_fields);
            // A throwable's clone keeps the trace it was cloned with, also
            // after the original's refill: the VM gives it its own copy of the
            // original's retained trace (page
            // `i82-L2-a-cloned-throwable-does-not-keep-its-trace`). No
            // allocation on the Java heap, so `this` and `clone_ref` stay current.
            ctx.clone_throwable_backtrace(this, clone_ref);
            // JLS §12.6 / HotSpot `JVM_Clone`: the clone of a finalizable
            // object is registered for finalization, exactly once — nothing
            // else registers an object minted by `alloc_object` (gc-common
            // w5-d, `handoff-w5d-clone-registers-finalizable`). A no-op for a
            // class without `finalize()`; arrays never reach this arm.
            ctx.register_if_finalizable(clone_ref);
            // KC26 LinkedHashMap.clone fix: LHM (and subclasses like
            // AnnotationAttributes) keeps most of its state in a side-table
            // overlay keyed by the receiver's identity hash code (C21:
            // GC-stable across moving collections), not in the heap fields
            // the loop above copies. Without this, the cloned LHM has all
            // state missing (size, table, head/tail) and a subsequent
            // `HashMap.clone()` → `reinitialize()` → `putMapEntries()` chain
            // walks an empty receiver, producing OOM via division-by-zero
            // load-factor or silent data loss.  Detect LHM ancestry on the
            // receiver and replicate the overlay onto the clone.
            let overlay =
                facts.map_or_else(|| object_clone_overlay_of(&*ctx, class_id), |f| f.overlay);
            match overlay {
                CloneOverlay::LinkedHashMap => {
                    // gc-common w13-c: the slot-keyed copy. The ctx-less
                    // form (deleted) cached addresses that went stale,
                    // wrote over moved live maps and leaked a rooted row
                    // per clone.
                    cratonvm_native_collections::clone_lhm_overlay_ctx(&*ctx, this, clone_ref);
                }
                // TC0622: `Properties` keeps its entries in an identity-keyed
                // side-table (`properties_sidetable`), NOT the heap fields the
                // shallow loop above copies. A fresh clone has a new identity
                // and therefore an empty side-table, so `getProperty` /
                // `propertyNames` / `stringPropertyNames` would see nothing
                // (the inherited Hashtable `defaults` field IS copied above, so
                // the defaults fallback still works). Snapshot the receiver's
                // side-table onto the clone — same pattern as the LinkedHashMap
                // overlay just above. This is what makes
                // `(Properties) props.clone()` round-trip its keys, e.g. the
                // env Hashtable JNDI clones in `InitialContext.<init>` and any
                // app that clones a Properties.
                CloneOverlay::Properties => {
                    let snap = crate::properties_sidetable::snapshot_sidetable(ctx, this);
                    if !snap.is_empty() {
                        crate::properties_sidetable::replace_sidetable(ctx, clone_ref, &snap);
                    }
                }
                CloneOverlay::None => {}
            }
            Ok(Some(Value::Object(Some(clone_ref))))
        }
        cratonvm_types::ObjectKind::Array => {
            let len = ctx.array_length(this);
            let et = ctx.heap_element_type_of(this);
            // gc-common w20-c: the copy's allocation can collect and move the
            // source array, whose elements are read after it (they were read
            // from its old address).
            let this_pin = ctx.pin_native_root(this);
            let clone = if may_reclaim {
                match clone_array_with_reclaim(ctx, class_id, et, len) {
                    Some(clone) => clone,
                    None => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                            message: "Java heap space".to_string(),
                        }
                        .into());
                    }
                }
            } else if et == cratonvm_types::ArrayElementType::Reference {
                ctx.new_ref_array(class_id, len)
            } else {
                ctx.new_array(et, len)
            };
            let this: ObjectRef = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            // Strings round (`str16`): one bulk copy, as `System.arraycopy`
            // makes it -- `copy_within`-class for a primitive array, the
            // context's validated-once reference copy for a reference array
            // (same component class, so no store check). JDK 25
            // `Arrays.copyOf`/`copyOfRange` return `original.clone()` for a
            // whole-array copy, so every `StringBuilder.toString()` of a full
            // builder came here, and the element loop below (two validated
            // heap trips per element) was its cost. `false` = nothing
            // written; the loop then runs as before.
            let bulk = len == 0
                || if et == cratonvm_types::ArrayElementType::Reference {
                    ctx.copy_reference_array_elements(this, 0, clone, 0, len)
                } else {
                    ctx.bulk_array_copy(this, 0, clone, 0, len)
                };
            if !bulk {
                for i in 0..len {
                    let val = ctx.get_array_element(this, i);
                    ctx.set_array_element(clone, i, val);
                }
            }
            Ok(Some(Value::Object(Some(clone))))
        }
        // Round-9 gc CRIT-1: HumongousFiller is a GC-internal walker
        // sentinel that should never reach the Object.clone() path.
        // If it ever does, treat it as an NPE rather than silently
        // returning a clone of garbage.
        cratonvm_types::ObjectKind::HumongousFiller => {
            Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("clone on humongous-continuation filler (GC sentinel)".to_string()),
            }
            .into())
        }
    }
}

/// No-op native that accepts `this` but does nothing.
// ---------------------------------------------------------------------------
// System.in/out/err override storage (used by setIn0/setOut0/setErr0)
// ---------------------------------------------------------------------------

/// One System stream override entry. `handle` is a persistent global GC
/// root (see [`NativeContext::add_global_root`]) so the installed stream
/// survives — and is remapped by — moving collections; `raw` is only the
/// registration-time address, kept as a last-resort fallback for contexts
/// with no global-root support (mocks).
///
/// GC-SAFETY (2026-07-21): this table previously stored a bare `ObjectRef`
/// in a process-global static — invisible to the collector. The first
/// moving GC after `System.setOut` left the entry pointing at recycled
/// memory (header reads as `ClassId(0)`/`java/lang/Object`, `num_slots=0`),
/// which silently dropped every native-originated framework log line under
/// WildFly (JBoss LogManager installs a delegating stdio stream very early
/// in boot) and produced the `[RESID-DIAG READ] class=java/lang/Object`
/// spam. Persistent-singleton native caches MUST hold a global root.
pub struct StreamOverride {
    pub handle: usize,
    pub raw: ObjectRef,
}

/// Table of System stream overrides set via System.setIn/setOut/setErr, keyed
/// by "in" / "out" / "err". Stores the most recently installed stream.
///
/// gc-common w21-a (`common-w20f-process-wide-caches-of-vm-local-global-root-handles-FIXED-20260923`,
/// found by the page's confirmation grep): one table per VM, dropped by
/// `forget_vm_native_root_stores`. It was ONE process-wide table keyed by the
/// stream name, while `handle` indexes the installing VM's own global-ref
/// table. So VM B's `System.setOut` called `remove_global_root` with VM A's
/// handle on VM B's table -- releasing an unrelated root of B's, while A's
/// stream stayed rooted in A for good -- and VM B's native logging
/// (`logging_shims`) resolved A's handle in B's table and wrote to whatever
/// object sat there (or, on a miss, to A's raw address). The raw-ref accessor
/// `system_overridden_stream(name)`, which had no caller, is removed. Nothing
/// under the lock calls `ctx`.
pub static SYSTEM_STREAM_OVERRIDES: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<&'static str, StreamOverride>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Per-VM teardown: drop `vm`'s stream overrides. The handles index `vm`'s own
/// global-ref table, which goes with it, so they are dropped, never released.
pub fn forget_vm_system_stream_overrides(vm: usize) {
    SYSTEM_STREAM_OVERRIDES.forget(vm);
}

/// Set the override for the given stream name ("in", "out", or "err").
/// Registers the stream as a persistent global GC root; releases the root
/// of any previously-installed override for the same name (in this VM).
pub fn system_set_overridden_stream(
    ctx: &mut dyn NativeContext,
    name: &'static str,
    stream: ObjectRef,
) {
    system_clear_overridden_stream(ctx, name);
    let vm = ctx.vm_identity();
    let handle = ctx.add_global_root(stream);
    let replaced = SYSTEM_STREAM_OVERRIDES.with(vm, |rows| {
        rows.insert(
            name,
            StreamOverride {
                handle,
                raw: stream,
            },
        )
    });
    // A racing `setOut` on another thread of this VM filed its own row in
    // between; its root is released rather than leaked.
    if let Some(replaced) = replaced.filter(|r| r.handle != 0 && r.handle != handle) {
        ctx.remove_global_root(replaced.handle);
    }
}

/// Remove the override for the given stream name (back to default).
pub fn system_clear_overridden_stream(ctx: &mut dyn NativeContext, name: &'static str) {
    let vm = ctx.vm_identity();
    let prev = SYSTEM_STREAM_OVERRIDES.with(vm, |rows| rows.remove(name));
    if let Some(prev) = prev {
        if prev.handle != 0 {
            ctx.remove_global_root(prev.handle);
        }
    }
}

/// `System.setIn0/setOut0/setErr0` (JVM spec: they write `System.in/out/err`
/// directly): set the static field AND update the override table.
///
/// One function per stream serves the `*0` native AND its public wrapper
/// (JDK 25 `setIn(in)` is `setIn0(in);`), so the two rows cannot drift and
/// the thin-wrapper census (`native-builtins/tests/r15_shadow_thin_wrapper_census.rs`)
/// proves the pair `same-fn`. `--real-jdk` retires the three wrappers onto
/// their bytecode (`RETIRED_SHADOW_SYSTEM_STREAM_SETTER_TRIPLES`). Round 15
/// wave 1, lane shadow; the bodies are the six closures they replace.
pub fn native_system_set_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &'static str,
) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_static_field_by_name("java/lang/System", field, val);
    // `CRATONVM_SYSTEM_STREAM_NULL_IS_NULL` (default ON, both modes; round 15
    // wave 2, lane jdkonly): the field is now written, so a null in it reads
    // back as null (`System.setOut(null); System.out == null`, as on HotSpot)
    // instead of the VM's canonical stream. `0` never marks it: null keeps
    // meaning "not installed yet".
    {
        ctx.mark_system_stream_written(field);
    }
    if let Value::Object(Some(s)) = val {
        system_set_overridden_stream(ctx, field, s);
    } else {
        system_clear_overridden_stream(ctx, field);
    }
    Ok(None)
}

pub fn native_system_set_in(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_system_set_stream(ctx, args, "in")
}

pub fn native_system_set_out(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_system_set_stream(ctx, args, "out")
}

pub fn native_system_set_err(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_system_set_stream(ctx, args, "err")
}

/// Read the current override for the given stream name ("in"/"out"/"err"),
/// resolved through its persistent global root so the returned ref is
/// valid after any number of moving collections. Falls back to the
/// registration-time raw ref only for contexts without global-root support
/// (`add_global_root` answered 0); a handle that no longer resolves answers
/// `None` rather than its stale registration-time address (gc-common w21-a).
pub fn system_overridden_stream_resolved(ctx: &dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let (handle, raw) = SYSTEM_STREAM_OVERRIDES
        .peek(ctx.vm_identity(), |rows| {
            rows.get(name).map(|e| (e.handle, e.raw))
        })
        .flatten()?;
    if handle != 0 {
        ctx.resolve_global_root(handle)
    } else {
        Some(raw)
    }
}

/// Side-table mapping a CratonVM-synthesised `java/util/Locale` ObjectRef to
/// its `(owning VM, language, script, country, variant)`.
///
/// The key is the Locale's address, unique among live objects of every live
/// VM, so a reader needs no VM. The VM stamp is what the `locale` root row
/// scans and remaps by, and what [`forget_vm_locale_state`] drops at VM
/// teardown (gc-common w10-a; this table used to be scanned and rebuilt whole
/// by every VM's collection).
///
/// The `script` slot is what makes `Locale.forLanguageTag("zh-hant-CN")`
/// round-trip: `Locale` keeps the script as a first-class subtag (its
/// `toString()` renders it as the `_#Hant` suffix), and a table that only held
/// language/country/variant had nowhere to put it, so every script-bearing
/// tag collapsed onto the script-less locale — Tomcat's
/// `TestAcceptLanguage.bug56848`, "expected:<zh_CN_#Hant> but was:<zh_CN>".
///
/// CRITICAL: `java.util.Locale` is a real bootstrap class whose instance
/// fields are `baseLocale` (a `sun.util.locale.BaseLocale`) and
/// `localeExtensions` (a `sun.util.locale.LocaleExtensions`) — NOT plain
/// `String` language/country/variant fields. Earlier code wrote the
/// language/country Strings straight into instance slots 0/1/2. When a
/// real-JDK `Locale` method that we do NOT natively override runs against
/// such an object (e.g. `getUnicodeLocaleType`), its bytecode does
/// `getfield localeExtensions; invokevirtual LocaleExtensions.getXxx()` —
/// but slot 1 holds a `String`, so the `invokevirtual` retargets onto
/// `java/lang/String` and raises a bogus
/// `NoSuchMethodError java/lang/String.getUnicodeLocaleType(...)`.
///
/// To avoid poisoning the real-JDK field layout, the synthetic Locale's
/// instance slots are left untouched (null / zero-initialised, which is the
/// spec-correct "no extensions" shape). The language/country/variant data
/// lives here instead, keyed by ObjectRef, and every Locale accessor native
/// reads from this table.
///
/// WEAK (gc-common w11-d, `common-w10a-locale-side-tables-root-every-locale-
/// ever-built`). A row does NOT keep its Locale alive: [`gc_scan_locale_roots`]
/// no longer pushes the keys, [`gc_sweep_locale_rows`] drops the row of a
/// Locale that did not survive a collection, and [`gc_update_locale_refs`]
/// re-keys a moved one. The keys used to be strong roots and nothing removed a
/// row before VM teardown, so every Locale a native ever built (one or two per
/// `ResourceBundle.getBundle`, one per `Locale.ROOT` / `Locale.US` read, ...)
/// was immortal, with its `BaseLocale` and four strings.
pub fn locale_data() -> &'static parking_lot::Mutex<
    std::collections::HashMap<ObjectRef, (usize, String, String, String, String)>,
> {
    use std::sync::OnceLock;
    static DATA: OnceLock<
        parking_lot::Mutex<
            std::collections::HashMap<ObjectRef, (usize, String, String, String, String)>,
        >,
    > = OnceLock::new();
    DATA.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record a synthetic Locale's `(language, country, variant)` in the side
/// table, with no script subtag. See [`locale_data`] for why instance fields
/// must not be used, and [`locale_data_set_full`] for the script-bearing form.
/// `vm` is the VM whose heap `obj` lives in.
pub fn locale_data_set(vm: usize, obj: ObjectRef, lang: &str, country: &str, variant: &str) {
    locale_data_set_full(vm, obj, lang, "", country, variant);
}

/// Record a synthetic Locale's full `(language, script, country, variant)`,
/// owned by `vm`.
pub fn locale_data_set_full(
    vm: usize,
    obj: ObjectRef,
    lang: &str,
    script: &str,
    country: &str,
    variant: &str,
) {
    locale_data().lock().insert(
        obj,
        (
            vm,
            lang.to_string(),
            script.to_string(),
            country.to_string(),
            variant.to_string(),
        ),
    );
}

/// Read a synthetic Locale's `(language, country, variant)` from the side
/// table. Returns empty strings for a Locale we never recorded (e.g. a
/// real-JDK-constructed Locale) — callers treat that as the root locale.
pub fn locale_data_get(obj: ObjectRef) -> (String, String, String) {
    let (l, _, c, v) = locale_data_get_full(obj);
    (l, c, v)
}

/// Read a synthetic Locale's full `(language, script, country, variant)`.
pub fn locale_data_get_full(obj: ObjectRef) -> (String, String, String, String) {
    locale_data()
        .lock()
        .get(&obj)
        .map(|(_, l, s, c, v)| (l.clone(), s.clone(), c.clone(), v.clone()))
        .unwrap_or_default()
}

/// The ISO-639 language of `locale` (or of the default locale when `None`),
/// normalised for [`case_map`].
///
/// A `java.util.Locale` reaches the String natives in one of three shapes, and
/// all three have to be understood or `toLowerCase(Locale.forLanguageTag("tr"))`
/// silently falls back to the root rules:
///
/// 1. one of *our* synthetic Locales — the language lives in a Rust side table
///    (`locale_data`, or `locale_bootstrap`'s own for the cached default),
///    because writing Strings into a real `Locale`'s instance slots would
///    poison its real-JDK field layout (see [`locale_data`]);
/// 2. a real-JDK `Locale` — the language is `baseLocale.language`;
/// 3. absent (the no-arg `toLowerCase()`), which the JDK defines as
///    `Locale.getDefault()`. Rather than *calling* `getDefault()` from inside a
///    String native — it can allocate and run `<clinit>` — we read the calling
///    VM's cached default Locale, or the `user.language` that seeds it when
///    none is cached yet, so `-Duser.language=tr` and `Locale.setDefault`
///    behave as they do on HotSpot. See
///    [`locale_bootstrap::default_case_mapping_language`], which answers only to
///    the precision case mapping distinguishes (`tr` / `az` / `lt`, or `""` for
///    the root rules every other language shares).
pub fn locale_language_for_case_mapping(
    ctx: &mut dyn NativeContext,
    locale: Option<ObjectRef>,
) -> std::borrow::Cow<'static, str> {
    let Some(obj) = locale else {
        return std::borrow::Cow::Borrowed(locale_bootstrap::default_case_mapping_language(ctx));
    };
    let (lang, _, _) = locale_data_get(obj);
    let raw = if !lang.is_empty() {
        lang
    } else if let Some(l) = locale_bootstrap::synthetic_language(obj) {
        l
    } else {
        real_locale_language(ctx, obj)
    };
    std::borrow::Cow::Owned(case_map::normalize_language(&raw))
}

/// `baseLocale.language` of a real-JDK `Locale`, or the empty string.
///
/// Allocates nothing and runs no bytecode, so `locale` may be a raw reference
/// read from a side table just before the call.
pub fn real_locale_language(ctx: &mut dyn NativeContext, locale: ObjectRef) -> String {
    if let Value::Object(Some(base)) = ctx.get_field_by_name(locale, "baseLocale") {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(base, "language") {
            return ctx.read_string(s).unwrap_or_default();
        }
    }
    String::new()
}

/// GC root scan for `vm`'s Locale caches: the cached default Locales
/// (`locale_bootstrap`'s `CACHED_LOCALE`), which `Locale.getDefault()` hands
/// back and which nothing on the Java side need reference. Without rooting and
/// remapping they go stale after a moving young GC and a later `Locale` method
/// dispatch SIGSEGVs ("Stale pointer … java/util/Locale").
///
/// Only `vm`'s rows (gc-common w10-a): the `locale` row of
/// `vm/src/memory/native_roots.rs` used to root and remap every VM's Locales
/// from every VM's collection.
///
/// The two subtag side tables (`locale_data` here and `locale_bootstrap`'s
/// synthetic-Locale table) are NOT roots since gc-common w11-d: they are weak,
/// swept by [`gc_sweep_locale_rows`] and re-keyed by [`gc_update_locale_refs`].
/// A row rooting its own key made every recorded Locale immortal.
pub fn gc_scan_locale_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    crate::locale_bootstrap::gc_scan_locale_roots(vm, out);
}

/// Drop `vm`'s subtag rows (in both Locale side tables) whose Locale did not
/// survive a collection, and return how many were dropped. Other VMs' rows are
/// never judged.
///
/// `is_live(addr)` answers for a key's address as the table holds it: the
/// PRE-remap address inside a stop-the-world epilogue (the caller folds the
/// collection's pointer map in: a moved key is live, and
/// [`gc_update_locale_refs`] re-keys it afterwards), or a pure "not in a span
/// this reclamation freed" range test.
///
/// Called by the VM after every stop-the-world collection
/// (`gc_and_alloc::process_references_after_gc`) and, per
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// from the two out-of-pause reclamation hooks in `vm/src/memory/addr_keyed.rs`.
/// Without the sweep a dead Locale's row would outlive it, and a later object
/// allocated at the same address would read the dead Locale's subtags.
///
/// `is_live` is evaluated under each table's lock. The locks are leaves: no
/// path takes a heap lock and then one of these, except the range-test hook,
/// whose predicate takes no lock (the same discipline
/// `addr_keyed::drop_address_keyed_rows_in` documents).
pub fn gc_sweep_locale_rows(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    let dropped = {
        let mut map = locale_data().lock();
        let before = map.len();
        map.retain(|k, row| row.0 != vm || is_live(k.as_ptr() as usize));
        before - map.len()
    };
    dropped + crate::locale_bootstrap::gc_sweep_locale_rows(vm, is_live)
}

/// The TLS twin of [`gc_sweep_locale_rows`] (gc-common w12-a): drop `vm`'s TLS
/// side-table rows whose owning `SSLEngine` / `SSLSession` / `SSLParameters` /
/// real `HttpURLConnection` did not survive a collection, and unrooted ALPN
/// selectors that died. Same `is_live` contract, same three call sites. See
/// `t27_tls::gc_sweep_tls_rows`.
pub fn gc_sweep_tls_rows(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    crate::t27_tls::gc_sweep_tls_rows(vm, is_live)
}

/// gci i30/rp (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`,
/// residual "side-table owners not offered"): append to `out` the owner
/// address of every row of VM `vm` in the weak side tables of this crate's
/// lib that the stop-the-world epilogue sweeps with the pause's in-place
/// verdict -- the Locale subtag tables ([`gc_sweep_locale_rows`]), the TLS
/// weak owners ([`gc_sweep_tls_rows`]) and the lock-key registry
/// ([`gc_sweep_and_remap_lock_keys`]). Read-only: each table lock is a leaf,
/// taken and released on its own, no heap lock is taken. The addresses are
/// the tables' own (PRE-remap inside a pause). Duplicates and nulls are left
/// for the caller's screen.
///
/// The VM (`gc_and_alloc::g1_pinned_young_referents_unheld`) offers them to
/// G1's per-object verdict for the young regions a pause kept in place, so an
/// owner that died there is refused by `InPlaceVerdict::survived` and its row
/// goes at this pause, as for an evacuated region, instead of outliving it
/// until the region is evacuated.
pub fn gc_weak_row_owners(vm: usize, out: &mut Vec<usize>) {
    {
        let map = locale_data().lock();
        out.extend(
            map.iter()
                .filter(|(_, row)| row.0 == vm)
                .map(|(k, _)| k.as_ptr() as usize),
        );
    }
    crate::locale_bootstrap::gc_weak_row_owners(vm, out);
    crate::t27_tls::gc_weak_row_owners(vm, out);
    lock_key_owner_addrs(vm, out);
}


/// Post-GC remap companion to [`gc_sweep_locale_rows`] and
/// [`gc_scan_locale_roots`]: `vm`'s rows and cached defaults only.
pub fn gc_update_locale_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    {
        let mut map = locale_data().lock();
        crate::locale_bootstrap::rekey_vm_rows(&mut *map, vm, |row| row.0, pointer_map);
    }
    crate::locale_bootstrap::gc_update_locale_refs(vm, pointer_map);
}

/// Drop `vm`'s Locale rows and cached default Locales (VM teardown, via
/// [`forget_vm_native_object_singletons`]). Idempotent.
pub fn forget_vm_locale_state(vm: usize) {
    locale_data().lock().retain(|_, row| row.0 != vm);
    crate::locale_bootstrap::forget_vm_locale_state(vm);
}

/// Per-VM teardown for the native object singletons gc-common w10-a made
/// VM-scoped (`common-w8a-process-global-object-singletons-in-native-builtins`
/// and the `locale` root row): the Locale tables, the `Signal` handlers and
/// `java.beans.Beans` state, the JEP-290 serial filter, the Spring fallback
/// `Environment` and generated-subclass name caches, and the
/// `ThreadLocal.withInitial` / `InheritableThreadLocal` tables. Runs from
/// [`forget_vm_native_root_stores`]. Idempotent.
pub fn forget_vm_native_object_singletons(vm: usize) {
    forget_vm_locale_state(vm);
    crate::deprecated_internal::forget_vm_signal_handlers(vm);
    crate::deprecated_internal::forget_vm_beans_state(vm);
    #[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
    crate::serialization::forget_vm_serial_filter(vm);
    crate::spring_startup_bootstrap::forget_vm_noop_environment(vm);
    crate::spring_startup_bootstrap::forget_vm_cglib_override_subclasses(vm);
    crate::phases_early::forget_vm_thread_local_side_tables(vm);
}

pub fn native_noop_with_this(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Object.wait / notify / notifyAll
// ---------------------------------------------------------------------------

pub fn native_object_wait(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Interpreter round i4 wave 89, lane L6 (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`):
    // a mounted virtual thread's wait unmounts instead (JEP 491).
    if let Some(result) = ctx.vt_monitor_wait_unmount(this, None) {
        return result;
    }
    if ctx.blocks_jdk_carrier() {
        return monitor_wait_compensated(ctx, this, None, false);
    }
    ctx.monitor_wait(this, None)
}

/// `Object.wait` of a mounted JDK virtual thread, which blocks its carrier:
/// the scheduler compensates for the time of the wait
/// (`NativeContext::begin_carrier_compensation`). The monitor's object and a
/// thrown `InterruptedException` are pinned across the two Java calls.
pub fn monitor_wait_compensated(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    timeout_ms: Option<u64>,
    reporting_timeout: bool,
) -> MethodCallResult {
    let pin = ctx.pin_native_root(this);
    let attempted = ctx.begin_carrier_compensation();
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    let result = if reporting_timeout {
        ctx.monitor_wait_reporting_timeout(this, timeout_ms)
    } else {
        ctx.monitor_wait(this, timeout_ms)
    };
    if !attempted {
        return result;
    }
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let pin = ctx.pin_native_root(exc);
            ctx.end_carrier_compensation(true);
            let exc = ctx.read_native_pin(pin, exc);
            ctx.unpin_native_roots(pin);
            Err(MethodCallFailed::ExceptionThrown(exc))
        }
        other => {
            ctx.end_carrier_compensation(true);
            other
        }
    }
}

pub fn native_object_wait_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        // WP4.5 tag erasure, as in `native_thread_sleep`: a long popped
        // generically comes back tagged Double with the bits intact.
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    // G72-1: a NEGATIVE timeout is an error, not a long wait.
    //
    // This arm used to fold `ms <= 0` into `None`, and `monitor_wait` reads
    // `None` as "wait forever" -- so `o.wait(-5)` on a held monitor parked the
    // thread with nobody to notify it and the VM never exited. A hang is worse
    // than a wrong value: it presents as "the application stopped", far from
    // the call, and the ordinary way to reach it is `deadline - now()` on a
    // deadline that has already passed.
    //
    // ZERO still means forever (JLS 17.2), which is why the test is `< 0` and
    // not `<= 0`. The rule was already written down in the doc comment of
    // `native_object_wait_timeout_nanos` directly below -- stated correctly and
    // implemented in that function only.
    if millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    let timeout_ms = if millis > 0 {
        Some(millis as u64)
    } else {
        None
    };
    // Wave 89 (lane L6): see `native_object_wait`.
    if let Some(result) = ctx.vt_monitor_wait_unmount(this, timeout_ms) {
        return result;
    }
    if ctx.blocks_jdk_carrier() {
        return monitor_wait_compensated(ctx, this, timeout_ms, true);
    }
    ctx.monitor_wait_reporting_timeout(this, timeout_ms)
}

/// T2.2.2: `Object.wait(long millis, int nanos)` — per `Object.wait(J)`V
/// semantics in JLS §17.2, the nanos parameter rounds the timeout up by
/// one millisecond if any of the nanos are non-zero (sub-millisecond
/// resolution is not guaranteed and HotSpot rounds the same way).
///
/// Validates argument ranges per the JDK spec:
///   * `millis < 0` → IllegalArgumentException
///   * `nanos` outside `[0, 999_999]` → IllegalArgumentException
///
/// Delegates to [`ctx.monitor_wait`] with the effective millisecond
/// timeout.
pub fn native_object_wait_timeout_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Interpreter round i5 wave 108, lane L6: decoded as
    // `native_object_wait_timeout` decodes it (WP4.5 tag erasure: a long may
    // come back tagged `Double` with its bits intact); read as 0 here, a
    // tag-lost `wait(-1, 0)` waited forever instead of throwing and a
    // `wait(50, 0)` waited for a notify that might never come.
    let millis = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let nanos = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                // NOT "timeout value is negative" -- that is `wait(long)`'s
                // wording. The two-arg overload says `timeoutMillis`.
                // Measured on Temurin 25.0.3; the pair is in `scratchpad/g74/Z.java`.
                message: "timeoutMillis value is negative".to_string(),
            }
            .into(),
        );
    }
    if !(0..=999_999).contains(&nanos) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    // HotSpot rounds sub-millisecond nanos up to one extra millisecond so
    // the caller never gets back *before* its requested interval.
    let effective_ms = if nanos > 0 {
        millis.saturating_add(1)
    } else {
        millis
    };
    let timeout_ms = if effective_ms > 0 {
        Some(effective_ms as u64)
    } else {
        None
    };
    // Interpreter round i5 wave 108, lane L6: as `wait(J)` (the JDK's
    // `wait(long, int)` IS `wait(long)` after its checks): a virtual thread
    // the VM schedules unmounts here too, where this stand-in kept its
    // carrier for the whole wait (wave 89's `vt_monitor_wait_unmount` was
    // wired into `native_object_wait_timeout` only).
    if let Some(result) = ctx.vt_monitor_wait_unmount(this, timeout_ms) {
        return result;
    }
    if ctx.blocks_jdk_carrier() {
        return monitor_wait_compensated(ctx, this, timeout_ms, true);
    }
    ctx.monitor_wait_reporting_timeout(this, timeout_ms)
}

pub fn native_object_notify(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.monitor_notify(this)}

pub fn native_object_notify_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.monitor_notify_all(this)}

// ---------------------------------------------------------------------------
// java.lang.Class natives
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// java.lang.System natives
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// java.lang.Float / Double natives
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// java.lang.Throwable natives
// ---------------------------------------------------------------------------

/// getStackTraceElement(int index) — return a StackTraceElement for the given frame index.
///
/// Creates a StackTraceElement object with declaringClass, methodName, fileName, lineNumber.
// --- Throwable additional methods ---

/// getCause() — read field 1 (cause)
/// initCause(Throwable) — set field 1, return this
/// toString() — build "ClassName: message" or just "ClassName"
/// printStackTrace() — print class name + message to output
/// getSuppressed() — return empty Throwable[]
/// getStackTrace() — return empty StackTraceElement[] for now
// ---------------------------------------------------------------------------
// java.lang.String natives
// ---------------------------------------------------------------------------

/// Helper: get the char[] value array from a String object's field 0.
// ---------------------------------------------------------------------------
// java.io.PrintStream natives
// ---------------------------------------------------------------------------

pub fn stdio_print_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

thread_local! {
    pub static STDIO_PRINT_LOCK_HELD: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

pub fn with_stdio_print_lock<R>(f: impl FnOnce() -> R) -> R {
    if STDIO_PRINT_LOCK_HELD.with(|held| held.get()) {
        return f();
    }
    let _guard = stdio_print_lock().lock();
    STDIO_PRINT_LOCK_HELD.with(|held| held.set(true));
    let result = f();
    STDIO_PRINT_LOCK_HELD.with(|held| held.set(false));
    result
}

pub const SUREFIRE_FORWARDING_PRINT_STREAM: &str =
    "org/apache/maven/surefire/api/report/ConsoleOutputCapture$ForwardingPrintStream";

/// zt t7/cl2: `CRATONVM_DBG_PRINT_LOSS=1` (default off, diagnostics only).
/// When a print native lets a line go nowhere, it says so on the host's
/// stderr, bypassing every Java stream. That covers: a closed stream, a sink
/// that threw (an absorbed `IOException`, or a refused `Error` /
/// `RuntimeException`), a failed autoflush, or no sink and no console fd.
/// Without it a lost line and a line never printed look the same: an empty
/// stdout and rc 0. Read only on those paths, so a delivered write costs
/// nothing.
pub fn print_loss_dbg_on() -> bool {
    false
}

/// The `[cratonvm] print-loss:` line of [`print_loss_dbg_on`]: `why`, the
/// receiver's class, the thrown object's class when there is one, and the
/// line's length in bytes (not its text: the line may be the program's
/// output, which this must not reproduce out of band).
pub fn note_print_loss(
    ctx: &dyn NativeContext,
    why: &str,
    receiver: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
    len: usize,
) {
    if !print_loss_dbg_on() {
        return;
    }
    let name_of = |obj: ObjectRef| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let stream = receiver.map(name_of).unwrap_or_else(|| "-".to_string());
    let thrown = thrown.map(name_of).unwrap_or_else(|| "-".to_string());
    eprintln!("[cratonvm] print-loss: {why} stream={stream} thrown={thrown} bytes={len}");
}


/// Detect if a PrintStream / PrintWriter is (or wraps) a system stream
/// (stdout fd=1, stderr fd=2).  Returns the fd_id if so, None otherwise.
///
/// The receiver may be:
///   * `System.out` / `System.err` itself, in which case ptr-equality wins
///     immediately; OR
///   * a `PrintWriter` (or similar wrapper) whose synthetic layout stores
///     the underlying stream in `field 0` — e.g. `new PrintWriter(System.out)`
///     constructed via `native_printwriter_init_outputstream`.  We follow
///     that chain a few hops to handle BufferedWriter / OutputStreamWriter
///     pass-throughs.
pub fn stream_fd(ctx: &dyn NativeContext, args: &[Value]) -> Option<u32> {
    let Some(Value::Object(Some(stream))) = args.first() else {
        return None;
    };
    let out = ctx.get_system_stream("out");
    let err = ctx.get_system_stream("err");
    let matches_fd = |obj: &ObjectRef| -> Option<u32> {
        if let Some(o) = &out {
            if std::ptr::eq(obj.as_ptr(), o.as_ptr()) {
                return Some(1);
            }
        }
        if let Some(e) = &err {
            if std::ptr::eq(obj.as_ptr(), e.as_ptr()) {
                return Some(2);
            }
        }
        None
    };

    // Direct match (PrintStream is System.out/err) or wrapped match
    // (PrintWriter.field0 == System.out/err).  Walk a small chain to
    // tolerate wrapper-on-wrapper layouts.
    let mut cur = *stream;
    for _ in 0..4 {
        if let Some(fd) = matches_fd(&cur) {
            return Some(fd);
        }
        match ctx.get_field(cur, 0) {
            Value::Object(Some(next)) => cur = next,
            _ => break,        }    }
    None
}

/// Classify a print-sink object (`PrintStream.out` / `PrintWriter` backing)
/// as a CHAR `java/io/Writer` vs a byte `java/io/OutputStream`.
///
/// This is the crux of the picocli/JUnit-console `NoSuchMethodError:
/// java/io/BufferedWriter.write([BII)V` fix.  A `PrintWriter`'s `out` field
/// (and a `PrintWriter`'s own backing) is a CHAR `Writer` — the JDK
/// `PrintWriter(OutputStream)` ctor wraps the sink as
/// `new BufferedWriter(new OutputStreamWriter(out))`.  `BufferedWriter`
/// only has `write([CII)V` / `write(Ljava/lang/String;)V`, NOT the byte
/// `write([BII)V` that our native print intercept previously assumed.  A
/// `PrintStream`'s `out`, by contrast, is a byte `OutputStream`.
///
/// Returns `Some(true)` when `out` is (a subclass of) `java/io/Writer`,
/// `Some(false)` when it is (a subclass of) `java/io/OutputStream`, and
/// `None` when it is neither / unresolvable (caller keeps prior behaviour).
pub fn sink_is_writer(ctx: &mut dyn NativeContext, out: ObjectRef) -> Option<bool> {
    let cid = ctx.class_id_of_object(out);
    if let Some(writer_cid) = ctx.class_id_by_name("java/io/Writer") {
        if cid == writer_cid || ctx.is_subclass(cid, writer_cid) {
            return Some(true);
        }
    }
    if let Some(os_cid) = ctx.class_id_by_name("java/io/OutputStream") {
        if cid == os_cid || ctx.is_subclass(cid, os_cid) {
            return Some(false);        }    }
    None
}

/// Write `text` to a CHAR `java/io/Writer` sink via the real JDK
/// `Writer.write(Ljava/lang/String;)V` method.  In real-JDK mode no
/// synthetic Writer/BufferedWriter natives are registered, so this
/// correctly dispatches to the JDK's own
/// `BufferedWriter`/`OutputStreamWriter`/`StreamEncoder` bytecode — no
/// stub is added.  Returns `true` on a successful invoke.
/// `this` is the `PrintStream`/`PrintWriter` whose `trouble` flag records a
/// failure here — see `route_write_through_out`. It is NOT the write target;
/// `out` is.
///
/// Returns whether the call was ROUTED, in `route_write_through_out`'s sense —
/// not whether the sink took the bytes. Those two used to be the same `bool`
/// here, which is what made an absorbed `IOException` fall through to the
/// console fast path and print text HotSpot writes nowhere.
/// W7-81-write-route-three-way.md
pub fn write_string_to_writer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out: ObjectRef,
    text: &str,
) -> bool {
    // `create_string` and the sink's `write` (bytecode) can both move `this`
    // and `out`: root them and read them back after each (gc-common w12-b
    // handoff).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let out_h = scope.root(out);
    let s = scope.create_string(text);
    let out = scope.get(&out_h);
    let written = scope.invoke_virtual(
        out,
        "write",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    );
    let this = scope.get(&this_h);
    // RECORDED since W7-64: `PrintWriter.write(String,int,int)` and
    // `PrintStream`'s private `write(String)` both end
    // `catch (IOException x) { trouble = true; }`.
    // W7-64-printstream-trouble-and-errormanager.md
    cratonvm_native_api::print_error_state::classify_write_failure(&mut *scope, this, written)
        .routed()
}

/// Write text to real stdout/stderr if this is a system stream (dual-mode).
/// When the receiver `PrintStream` wraps a real underlying `OutputStream`
/// (its `FilterOutputStream.out` field is non-null — i.e. a user
/// `new PrintStream(fos/...)` or DaCapo's `TeePrintStream`/`TeeOutputStream`,
/// typically installed via `System.setOut`), write `bytes` through
/// `out.write([B,0,len)` so the real stream chain (tee → screen + digest →
/// file) observes the output, exactly as the JDK `PrintStream` bytecode would.
///
/// Returns `true` when the call was ROUTED — the sink took the bytes, or the
/// sink raised the `IOException` the JDK's own `catch` names and HotSpot
/// therefore wrote them nowhere. Returns `false` when there is no Java sink to
/// route to (`out` is null — the canonical synthetic `System.out`/`System.err`,
/// fd-backed) **or when the sink refused the call outright** (an `Error`, a
/// `RuntimeException`, an internal VM failure), so the caller falls back to the
/// fd fast-path. See the three-way table at the byte write below; both branches
/// answer from it. W7-81-write-route-three-way.md
///
/// This closes the `System.setOut`/`System.setErr` redirection gap: the
/// blanket `println`/`print` natives previously resolved every PrintStream to
/// a fixed fd (1/2) and wrote straight to the console, leaving a redirected
/// file empty (DaCapo luindex's `stdout.log` digest validation saw the
/// empty-input SHA-1).
pub fn route_write_through_out(ctx: &mut dyn NativeContext, args: &[Value], bytes: &[u8]) -> bool {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return false,
    };
    // Cheap null-probe (no allocation): canonical synthetic out/err have a
    // null `out` and must take the fd fast-path. The result ObjectRef is
    // intentionally discarded — see the GC note below.
    if !matches!(ctx.get_field_by_name(this, "out"), Value::Object(Some(_))) {
        return false;
    }
    // Allocate + fill the byte[] FIRST — `new_array` can trigger a compacting
    // GC. A `Value::Object` resolved from `this.out` BEFORE the allocation
    // would dangle if the collector moved it (the avrora SEGV regression), so
    // `out` is resolved AFTER it. The funnel roots the receiver object, but no
    // collection rewrites the `this` local, so `this`, the array and `out` are
    // rooted here and read back after every GC point (gc-common w12-b
    // handoff).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    let arr_h = scope.root(arr);
    scope.write_byte_array_from(arr, 0, bytes);
    let this = scope.get(&this_h);
    let out = match scope.get_field_by_name(this, "out") {
        Value::Object(Some(o)) => o,
        _ => return false,
    };
    let out_h = scope.root(out);
    // Branch on the runtime class of `out`.  A `PrintWriter` wraps its sink
    // as a CHAR `BufferedWriter`/`OutputStreamWriter` (only `write([CII)V` /
    // `write(String)`), so the byte `write([BII)V` below would raise
    // `NoSuchMethodError: java/io/BufferedWriter.write([BII)V` (the picocli /
    // JUnit-console help-text crash).  Write chars for a `Writer`, bytes for
    // an `OutputStream`.
    if let Some(true) = sink_is_writer(&mut *scope, out) {
        // Reconstruct the text from the UTF-8 bytes the caller built (callers
        // pass UTF-8 of the original String / println buffer).
        let text = String::from_utf8_lossy(bytes);
        let this = scope.get(&this_h);
        let out = scope.get(&out_h);
        return write_string_to_writer(&mut *scope, this, out, &text);
    }
    let out = scope.get(&out_h);
    let arr = scope.get(&arr_h);
    // RECORDED since W7-64. `PrintStream.write(byte[],int,int)` is
    // `try { …; out.write(buf, off, len); … }
    //  catch (InterruptedIOException x) { Thread.currentThread().interrupt(); }
    //  catch (IOException x) { trouble = true; }`, and every `print`/`println`
    // overload funnels through a private `write`/`writeln` with the same two
    // clauses. This helper is that delegation for both `PrintStream` and
    // `PrintWriter` receivers, and the `trouble` field is on both classes, so
    // the record is by field name on `this`.
    //
    // ROUTED, not DELIVERED — the two used to be one `bool` and the branches
    // disagreed about which one it was. `classify_write_failure` splits the
    // three outcomes the JDK bodies above actually have:
    //
    //   clean                 -> Delivered -> routed. The sink has the bytes.
    //   IOException           -> Absorbed  -> routed. The JDK's `catch` ran,
    //                            `trouble` is set, and HotSpot wrote the bytes
    //                            NOWHERE — so neither may we. Falling back here
    //                            would print to a console HotSpot never touched.
    //   Error / RuntimeException / InternalError
    //                         -> Refused   -> NOT routed. HotSpot propagates
    //                            these out of `println`; this helper cannot, so
    //                            the caller's console fast path is what stops
    //                            the text vanishing. That fallback is the
    //                            picocli / JUnit-console `NoSuchMethodError`
    //                            survival path named above, and it is the one
    //                            thing a `?` here would delete.
    //
    // W7-70 left this undone for a stated reason — the absorption and the
    // fallback are ONE mechanism — and the split above is how they come apart:
    // not by changing signatures (nothing's arity moved; the 23 `stream_write`
    // / `stream_writeln` call sites are untouched) but by deciding what "the
    // sink refused the call" should do differently from "the sink handled the
    // failure".
    //
    // Which branch became authoritative, per outcome: the BYTE branch on an
    // absorbed `IOException` (it already declined to echo, and HotSpot writes
    // nowhere), and the WRITER branch on a refusal (it already fell back, and
    // silence is the one answer HotSpot never gives). Neither branch was right
    // on both.
    // W7-64-printstream-trouble-and-errormanager.md,
    // W7-70-printstream-close-noop.md, W7-81-write-route-three-way.md
    let written = scope.invoke_virtual(
        out,
        "write",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(bytes.len() as i32),
        ],
    );
    let this = scope.get(&this_h);
    // zt t7/cl2: an absorbed `IOException` is a lost line (HotSpot's too); a
    // refused one falls back to the console fd below if the stream is one.
    if let Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(thrown)) = &written {
        note_print_loss(&*scope, "sink-threw", Some(this), Some(*thrown), bytes.len());
    }
    cratonvm_native_api::print_error_state::classify_write_failure(&mut *scope, this, written)
        .routed()
}

/// Encode `text` with the receiver's own charset.
///
/// **`PrintStream` has a charset and this VM was ignoring it.** Every text
/// write funnelled `text.as_bytes()` — UTF-8, unconditionally — into the sink.
/// MEASURED:
///
/// ```text
///   new PrintStream(sink, true, ISO_8859_1).print("é中")
///       HotSpot   e9 3f            (latin1: e-acute, '?' for the unmappable)
///       CratonVM  c3 a9 e4 b8 ad   (UTF-8)
///   new PrintStream(sink, true, US_ASCII).print("a中b")
///       HotSpot   61 3f 62
///       CratonVM  61 e4 b8 ad 62
/// ```
///
/// The second row is the one that matters: a stream declared `US_ASCII`
/// emitted a three-byte sequence with the high bit set. Anything downstream
/// that trusts the declared encoding — a fixed-width record writer, a protocol
/// framer, a terminal — is handed bytes it cannot represent, and the length in
/// bytes no longer matches the length in characters.
///
/// `None` means "UTF-8, use `as_bytes()`" and is the fast path: the charset
/// field is absent (a `PrintWriter` receiver, or a console stream), null, or
/// resolves to UTF-8. The discriminator is the charset object's CLASS NAME,
/// which `class_name_arc_of_id` answers without allocating — the standard
/// charsets each have their own final class. Anything else takes the general
/// path through `String.getBytes(Charset)`, which is the JDK's own encoder and
/// therefore right for every charset this VM can load, at the cost of one
/// String allocation on a path that is by construction rare.
/// The charset name a `PrintStream` with no usable `charset` field should be
/// repaired to.
///
/// `System.out` / `System.err` answer their OWN stream encoding; every other
/// `PrintStream` answers `Charset.defaultCharset()`, which JEP 400 pins to
/// UTF-8. MEASURED on Temurin 25.0.3+9 under `LANG=C`, in ONE run:
/// `System.out.charset()` is `US-ASCII` and `new PrintStream(baos).charset()`
/// is `UTF-8`. A single constant here cannot be right for both.
pub fn printstream_repair_encoding(ctx: &dyn NativeContext, this: Option<ObjectRef>) -> String {
    if let Some(this) = this {
        for (stream, key) in [("out", "stdout.encoding"), ("err", "stderr.encoding")] {
            if let Some(canonical) = ctx.get_system_stream(stream) {
                if std::ptr::eq(canonical.as_ptr(), this.as_ptr()) {
                    return ctx
                        .get_system_property(key)
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| "UTF-8".to_string());
                }
            }
        }
    }
    "UTF-8".to_string()
}

pub fn printstream_encode(ctx: &mut dyn NativeContext, args: &[Value], text: &str) -> Option<Vec<u8>> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let cs = match ctx.get_field_by_name(this, "charset") {
        Value::Object(Some(c)) => c,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(cs);
    // NIO2-17-5 rung 5 (`CRATONVM_CLASS_FACTS_MEMO_CARRIERS`): the arm every
    // UTF-8 stream takes -- no class, or `sun/nio/cs/UTF_8` -- answered from
    // the class-facts memo; any other charset class keeps the name match.
    if let Some(facts) =
        crate::class_facts_memo::rung_facts(ctx, crate::class_facts_memo::Rung::Carriers, cid)
    {
        match facts {
            None => return None,
            Some(f) if f.is_utf8_charset() => return None,
            Some(_) => {}
        }
    }
    match ctx.class_name_arc_of_id(cid).as_deref() {
        None | Some("sun/nio/cs/UTF_8") => None,
        // The abstract stand-in a synthetic-JDK image falls back to. It used
        // to be read as "UTF-8" unconditionally, which was true only while
        // `install_charset` stamped a hard-coded UTF-8; now that the stamp
        // carries the HOST's encoding, a stand-in named `US-ASCII` that still
        // emitted UTF-8 bytes would be the one outcome worse than either
        // honest answer — a stream lying about its own wire format. Its `name`
        // slot is the only thing it has, so encode through the VM's own engine
        // by that name.
        Some("java/nio/charset/Charset") => {
            let name = match ctx.get_field(cs, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            match cratonvm_native_api::charset::canonical_charset_name(&name) {
                None | Some("UTF-8") => None,
                Some(canon) => {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    Some(cratonvm_native_api::charset::encode_chars_lossy(
                        canon, &units,
                    ))
                }
            }
        }
        Some("sun/nio/cs/ISO_8859_1") => Some(
            text.chars()
                .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
                .collect(),
        ),
        Some("sun/nio/cs/US_ASCII") => Some(
            text.chars()
                .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
                .collect(),
        ),
        Some(_) => {
            let s = ctx.create_string(text);
            let bytes = ctx
                .invoke_virtual(
                    s,
                    "getBytes",
                    "(Ljava/nio/charset/Charset;)[B",
                    &[Value::Object(Some(cs))],
                )
                .ok()
                .flatten();
            match bytes {
                Some(Value::Object(Some(arr))) => {
                    let len = ctx.array_length(arr);
                    let mut out = vec![0u8; len];
                    ctx.read_byte_array_into(arr, 0, &mut out);
                    Some(out)
                }
                _ => None,
            }
        }
    }
}

/// `PrintStream.ensureOpen()`: a write to a CLOSED stream sets the error flag.
///
/// `close()` nulls `out` in the JDK, and every write opens with
/// `if (out == null) throw new IOException("Stream closed")` — caught by the
/// same body into `trouble = true`. MEASURED: after `close()`, this VM's
/// `print("z")` reached the sink (a `ByteArrayOutputStream.close()` is a
/// no-op, so the write succeeded) and `checkError()` stayed FALSE, where
/// HotSpot answers true. A `PrintStream` whose `checkError()` cannot become
/// true is a `PrintStream` with no error reporting at all — the flag is the
/// class's ONLY channel for failure.
pub fn printstream_refuse_if_closed(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return false;
    };
    if cratonvm_native_api::print_error_state::is_closing(&*ctx, this) {
        cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
        note_print_loss(&*ctx, "closed", Some(this), None, 0);
        return true;
    }
    false
}

/// `PrintStream`'s autoflush, which fires on more than a newline.
///
/// `PrintStream.write(byte[], int, int)` ends with `if (autoFlush)
/// out.flush();` UNCONDITIONALLY, and every `print`/`println` overload reaches
/// it through the internal `OutputStreamWriter`. `write(int b)` flushes only
/// for `'\n'`. MEASURED on an autoflush stream over a counting sink:
/// HotSpot's flush count was 1 after a single `print("a")` and this VM's was
/// 0, so a `new PrintStream(sock.getOutputStream(), true)` — the ordinary way
/// to write a line protocol — buffered indefinitely.
pub fn printstream_autoflush(ctx: &mut dyn NativeContext, args: &[Value]) {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return;
    };
    if !matches!(ctx.get_field_by_name(this, "autoFlush"), Value::Int(v) if v != 0) {
        return;
    }
    let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") else {
        return;
    };
    // KEPT SWALLOW, at JDK parity: this flush lives inside `write`'s own
    // `catch (IOException x) { trouble = true; }`. The flush is bytecode, so
    // `this` is read back from a pin before `trouble` is set on it (gc-common
    // w12-b handoff).
    let this_pin = ctx.pin_native_root(this);
    let flushed = ctx.invoke_virtual(out, "flush", "()V", &[]);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Err(e) = flushed {
        // zt t7/cl2: the bytes stay in the Java sink's buffer.
        let thrown = match e {
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(t) => Some(t),
            cratonvm_types::error::MethodCallFailed::InternalError(_) => None,
        };
        note_print_loss(&*ctx, "autoflush-threw", Some(this), thrown, 0);
        cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
    }
}

pub fn stream_write(ctx: &mut dyn NativeContext, args: &[Value], text: &str) {
    // THE RECEIVER STAYS ROOTED, exactly as in `stream_writeln_inner` and for
    // exactly the reason the LOCK-SCOPE note below already gives: these
    // helpers "recursively interpret Java bytecode". That bytecode allocates,
    // so it can collect, so it can RELOCATE this stream — and `args` is a
    // pre-call snapshot that no collection rewrites. See
    // [`args_with_live_receiver`].
    //
    // This is the `print` twin of the `println` defect that made
    // `bench-gpu/residency-gc.sh` red on 2026-09-06. It is fixed here on the
    // strength of being the same code shape rather than a second reproducer:
    // `stream_fd` at the bottom dereferences the snapshot after two
    // Java-interpreting helpers have run, which is the crashing sequence.
    let mut scope = NativeHandleScope::new(ctx);
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => Some(scope.root(*o)),
        _ => None,
    };
    // LOCK-SCOPE (2026-07-21): `surefire_forwarding_write` and
    // `route_write_through_out` recursively interpret Java bytecode
    // (`invoke_virtual`); running them under the global stdio print mutex
    // deadlocks against Java monitors / `class_manager` whose holders may
    // themselves be blocked on this mutex (live gdb capture: WildFly boot
    // wedge at parallel-extension-add, 2026-07-20). Only the raw fd write
    // is serialized.
    if printstream_refuse_if_closed(&mut *scope, args) {
        return;
    }
    if surefire_forwarding_write(&mut *scope, args, text, false) {
        return;
    }
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    let encoded = printstream_encode(&mut *scope, args, text);
    let bytes: &[u8] = encoded.as_deref().unwrap_or_else(|| text.as_bytes());
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if route_write_through_out(&mut *scope, args, bytes) {
        let args = &args_with_live_receiver(&scope, args, recv.as_ref());
        printstream_autoflush(&mut *scope, args);
        return;
    }
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if let Some(fd) = stream_fd(&mut *scope, args) {
        let ok = with_stdio_print_lock(|| scope.fd_table().write_bytes(fd, bytes));
        // RECORDED since W7-64. The fd IS this stream's `out` — a console
        // `PrintStream` has no Java sink object — so a host write failure here
        // is exactly the `IOException` `PrintStream`'s private `write(String)`
        // catches with `trouble = true`.
        // W7-64-printstream-trouble-and-errormanager.md
        if let Some(Value::Object(Some(this))) = args.first().copied() {
            cratonvm_native_api::print_error_state::record_host_io_failure(&*scope, this, ok);
        }
        return;
    }
    // NOWHERE TO WRITE IS AN ERROR, not a no-op. `PrintWriter.close()` and
    // `PrintStream.close()` both null `out`, and every write method then
    // reaches `ensureOpen()`'s `IOException("Stream closed")`, which the same
    // body catches into `trouble = true`. Falling out of both routes here
    // means exactly that state — no Java sink and no file descriptor — and
    // returning quietly left `checkError()` FALSE forever. MEASURED on a
    // `PrintWriter` after `close()`: HotSpot true, this VM false.
    // ONLY that state, though. Two very different situations reach this line,
    // and until now both set `trouble`:
    //
    //  * no Java sink and no fd — the closed stream above. HotSpot sets it.
    //  * a sink that REFUSED the write — an `Error` or `RuntimeException`,
    //    which `catch (IOException x)` does not name. HotSpot PROPAGATES that
    //    out of `print` and never runs the `catch`, so `trouble` stays clear.
    //    This VM cannot propagate it (W7-70 established that making these
    //    natives propagate deletes the console fallback), so it falls through
    //    to here — but inventing a `trouble` HotSpot would not set turns a
    //    KEPT divergence into a second, louder one.
    //
    // The sink FIELD separates them, which is why both `close()` natives now
    // null it. MEASURED — `CloseFlushSwallowProbe`'s
    // `psRouteErrorWriteDidNotRecordTrouble` and its `pw` twin.
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let has_sink = matches!(scope.get_field_by_name(this, "out"), Value::Object(Some(_)));
        // zt t7/cl2: the line reached neither a sink nor a console fd.
        let why = if has_sink {
            "refused-no-console-fd"
        } else {
            "no-sink-no-console-fd"
        };
        note_print_loss(&*scope, why, Some(this), None, 0);
        if !has_sink {
            cratonvm_native_api::print_error_state::set_trouble(&*scope, this);
        }
    }
}

/// Return the current JVM line separator, respecting any
/// user-overridden `line.separator` system property.  Falls back to
/// the host-platform default (`\r\n` on Windows, `\n` elsewhere).
///
/// The doc comment above used to say "`\n` on Windows, `\n` elsewhere", and the
/// code said the same thing twice in a `cfg!(windows)` that decided nothing.
/// `PrintStream.println` and `PrintWriter.println` are SPECIFIED to write the
/// platform separator, so every `println` in the VM went through this fallback
/// whenever the property was unseeded — during bootstrap, or through a
/// `Properties` object that had lost the key.
pub fn host_line_separator(ctx: &dyn NativeContext) -> String {
    // The CAPTURED static, not the live property -- see
    // `NativeContext::captured_line_separator` for the measurement.
    ctx.captured_line_separator()
}

/// Write `text` followed by the configured line separator — RB.7 requires
/// `println` to honour `line.separator` and emit NO leading BOM (the
/// underlying `stream_write` writes UTF-8 bytes raw, which is what Java
/// specifies for println).
/// Real `PrintWriter.println` ends with `if (autoFlush) out.flush();`. The
/// shared `println` natives below wrote straight through without it, so a
/// `new PrintWriter(stream, true)` left its `BufferedWriter` unflushed until
/// something else happened to flush -- `MockMvcTester.debug(out)`'s report
/// came back truncated mid-way through
/// (`test.web.servlet.assertj.MockMvcTesterIntegrationTests`). `print` must
/// NOT flush (the JDK only auto-flushes on a newline), so this hook lives on
/// the writeln path alone.
pub fn printwriter_autoflush_if_needed(ctx: &mut dyn NativeContext, args: &[Value]) {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return;
    };
    // Only the plain `java.io.PrintWriter` carrier: a subclass runs its own
    // bytecode (which already honours `autoFlush`), and the fd-backed
    // `PrintStream` path is unbuffered, so flushing it every line would be
    // pure overhead on a very hot path.
    //
    // NIO2-17-5 rung 5 (`CRATONVM_CLASS_FACTS_MEMO_CARRIERS`): the exact-class
    // test from the class-facts memo.
    let class_id = ctx.class_id_of_object(this);
    let plain_print_writer = match crate::class_facts_memo::rung_facts(
        ctx,
        crate::class_facts_memo::Rung::Carriers,
        class_id,
    ) {
        Some(facts) => facts.is_some_and(crate::class_facts_memo::NameFacts::is_print_writer),
        None => ctx.class_name_arc_of_id(class_id).as_deref() == Some("java/io/PrintWriter"),
    };
    if !plain_print_writer {
        return;
    }
    if !matches!(ctx.get_field_by_name(this, "autoFlush"), Value::Int(v) if v != 0) {
        return;
    }
    // KEPT SWALLOW, at JDK parity. This is `PrintWriter.println`'s autoflush,
    // and it dispatches `PrintWriter.flush()` — whose own body is
    // `try { ensureOpen(); out.flush(); } catch (IOException x)
    // { trouble = true; }`. HotSpot's autoflush therefore cannot raise an
    // `IOException` either; `println` declares nothing and never throws one.
    //
    // Residual, and NOT the same residual `route_write_through_out` closed: an
    // `Error` from that dispatch is absorbed here where HotSpot would let it
    // out. W7-81's three-way answer does not reach this site, because this is
    // not a write and there is nothing to route — an autoflush has no fallback
    // destination, so "refused" and "absorbed" have the same consequence here
    // (nothing) and the only remaining question is propagation. That is a
    // signature change on `stream_writeln` (11 call sites) and its sibling
    // `stream_write` (12) — all 23 in native bodies that already return
    // `MethodCallResult`, "ten" in an earlier note having been a sample — and
    // it is genuinely just arity here, unlike at the write path, where the
    // arity was never the blocker. Left for a lane that can measure it.
    // W7-57-close-flush-swallow-sweep.md, W7-70-printstream-close-noop.md,
    // W7-81-write-route-three-way.md
    let _ = ctx.invoke_virtual(this, "flush", "()V", &[]);
}

pub fn stream_writeln(ctx: &mut dyn NativeContext, args: &[Value], text: &str) {
    // The same hazard one level up: `stream_writeln_inner` re-enters Java, so
    // `args` can name the receiver's pre-call address by the time the
    // autoflush dereferences it. See [`args_with_live_receiver`].
    let mut scope = NativeHandleScope::new(ctx);
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => Some(scope.root(*o)),
        _ => None,
    };
    stream_writeln_inner(&mut *scope, args, text);
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    printwriter_autoflush_if_needed(&mut *scope, args);
}

/// Rebuild a native argument snapshot with its RECEIVER at the address it has
/// now, reading it back out of the handle that kept it rooted.
///
/// `args` is a Rust-side snapshot taken before the native was entered.
/// `vm_exec::safe_native_call_impl` pins every argument — so nothing is ever
/// collected out from under a native — but it rebuilds the snapshot from those
/// pins only for a collection it ran ITSELF, at one of the three hooks BEFORE
/// the callback. Its own comment says why they live there: the native
/// allocation wrappers "must stay GC-free mid-callback, since their callers
/// hold unrooted local `ObjectRef`s".
///
/// A native that re-enters Java breaks that premise. The bytecode it invokes
/// allocates and can drive a young collection, and a Generational young
/// collection RELOCATES — by Cheney copy on the moving path, and by selective
/// promotion even on the non-moving one. The object survives at its new
/// address; the caller's snapshot keeps naming the old one.
///
/// Reading a field through that stale address lands in the inactive semi-space
/// or, with `CRATONVM_GEN_UNCOMMIT` on (the default since 2026-09-05), in a
/// DECOMMITTED page — `EXCEPTION_ACCESS_VIOLATION` inside
/// `gen_heap::get_field`. With uncommit off the same read finds a zeroed
/// header, decodes as `ClassId(0)` / `java.lang.Object` with no slots, and the
/// field guard drops it with a warning: the same defect, surviving only
/// because the page happened to still be mapped.
///
/// See `known-issues/gc/native-arg-snapshot-stale-across-java-reentry-20260906.md`.
pub fn args_with_live_receiver(
    scope: &NativeHandleScope<'_>,
    args: &[Value],
    recv: Option<&cratonvm_native_api::registry::NativeHandle>,
) -> Vec<Value> {
    let mut out = args.to_vec();
    if let (Some(h), Some(slot)) = (recv, out.first_mut()) {
        *slot = Value::Object(Some(scope.get(h)));
    }
    out
}

pub fn stream_writeln_inner(ctx: &mut dyn NativeContext, args: &[Value], text: &str) {
    // THE RECEIVER STAYS ROOTED FOR THE WHOLE BODY, and the argument snapshot
    // is rebuilt after every step that can re-enter Java. `printstream_encode`
    // runs the JDK charset encoder and `route_write_through_out` runs
    // `Writer.write`; both are bytecode, both allocate, and either can drive a
    // young collection that relocates this stream. Before this scope existed,
    // `stream_fd` below dereferenced the PRE-CALL address and took a SIGSEGV
    // in `gen_heap::get_field` — the reproducer was `GpuResidencyGc` under
    // `-XX:+UseGenerationalGC` with the JIT on, which is what made
    // `bench-gpu/residency-gc.sh` red on 2026-09-06. See
    // [`args_with_live_receiver`].
    let mut scope = NativeHandleScope::new(ctx);
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => Some(scope.root(*o)),
        _ => None,
    };
    // LOCK-SCOPE (2026-07-21): see `stream_write` — the Java-interpreting
    // helpers must not run under the stdio print mutex; only the fd write
    // pair (text + separator) stays atomic.
    let sep = host_line_separator(&mut *scope);
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if printstream_refuse_if_closed(&mut *scope, args) {
        return;
    }
    if surefire_forwarding_write(&mut *scope, args, text, true) {
        return;
    }
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    // User/Tee streams: write text+separator as one buffer through `out`.
    // The SEPARATOR is encoded with the same charset as the text — it is
    // ASCII in every charset this reaches, but encoding the pair together is
    // what keeps a stateful encoder (a UTF-16 stream's BOM, say) consistent.
    let line = format!("{text}{sep}");
    let encoded = printstream_encode(&mut *scope, args, &line);
    let buf: Vec<u8> = match encoded {
        Some(b) => b,
        None => line.as_bytes().to_vec(),
    };
    // The JDK encoder just ran.
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if route_write_through_out(&mut *scope, args, &buf) {
        let args = &args_with_live_receiver(&scope, args, recv.as_ref());
        printstream_autoflush(&mut *scope, args);
        return;
    }
    // `route_write_through_out` runs `Writer.write` on the path that returns
    // FALSE too — it can attempt a sink and be refused — so the snapshot is
    // rebuilt here as well. This is the read that crashed.
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if let Some(fd) = stream_fd(&mut *scope, args) {
        let ok = with_stdio_print_lock(|| {
            // Both writes are attempted regardless, as before — `and` keeps
            // the first failure without turning the pair into a short-circuit
            // that would drop the separator after a partial text write.
            let text_written = scope.fd_table().write_bytes(fd, &buf);
            let sep_written = Ok(());
            text_written.and(sep_written)
        });
        // RECORDED since W7-64 — see `stream_write`.
        if let Some(Value::Object(Some(this))) = args.first().copied() {
            cratonvm_native_api::print_error_state::record_host_io_failure(&*scope, this, ok);
        }
        return;
    }
    // NOWHERE TO WRITE IS AN ERROR, not a no-op. `PrintWriter.close()` and
    // `PrintStream.close()` both null `out`, and every write method then
    // reaches `ensureOpen()`'s `IOException("Stream closed")`, which the same
    // body catches into `trouble = true`. Falling out of both routes here
    // means exactly that state — no Java sink and no file descriptor — and
    // returning quietly left `checkError()` FALSE forever. MEASURED on a
    // `PrintWriter` after `close()`: HotSpot true, this VM false.
    // ONLY that state, though. Two very different situations reach this line,
    // and until now both set `trouble`:
    //
    //  * no Java sink and no fd — the closed stream above. HotSpot sets it.
    //  * a sink that REFUSED the write — an `Error` or `RuntimeException`,
    //    which `catch (IOException x)` does not name. HotSpot PROPAGATES that
    //    out of `print` and never runs the `catch`, so `trouble` stays clear.
    //    This VM cannot propagate it (W7-70 established that making these
    //    natives propagate deletes the console fallback), so it falls through
    //    to here — but inventing a `trouble` HotSpot would not set turns a
    //    KEPT divergence into a second, louder one.
    //
    // The sink FIELD separates them, which is why both `close()` natives now
    // null it. MEASURED — `CloseFlushSwallowProbe`'s
    // `psRouteErrorWriteDidNotRecordTrouble` and its `pw` twin.
    let args = &args_with_live_receiver(&scope, args, recv.as_ref());
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let has_sink = matches!(scope.get_field_by_name(this, "out"), Value::Object(Some(_)));
        // zt t7/cl2: the line reached neither a sink nor a console fd.
        let why = if has_sink {
            "refused-no-console-fd"
        } else {
            "no-sink-no-console-fd"
        };
        note_print_loss(&*scope, why, Some(this), None, 0);
        if !has_sink {
            cratonvm_native_api::print_error_state::set_trouble(&*scope, this);
        }
    }
}

/// Emit a framework log record through the live Java-level console stream.
///
/// Native logging fallbacks must not merely add entries to `printed_lines`:
/// Spring Boot's `OutputCaptureExtension` observes the `PrintStream` installed
/// by `System.setOut`, so bypassing that stream loses the record to tests (and
/// to any other Java-level redirection).  Routing through `stream_writeln`
/// preserves the canonical fd fast path for the original stream while calling
/// a capture stream's real `OutputStream.write` override after redirection.
thread_local! {
    /// Recursion depth for `emit_framework_log`'s real-dispatch path. A
    /// delegating override stream's `println` bytecode may funnel back into
    /// a print native that logs (or the delegate chain may loop); depth > 2
    /// falls back to the direct native writeln, which cannot recurse.
    pub static EMIT_FRAMEWORK_LOG_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Include the Throwable's Java representation for overloads whose contract
/// carries an exception.  Calling `printStackTrace` here would bypass the
/// capture stream on some JDK paths; `toString` is sufficient for framework
/// diagnostics and remains part of the same captured record.
// ---------------------------------------------------------------------------
// java.lang.System.Logger — the receiver CratonVM's `System.getLogger` mints
// ---------------------------------------------------------------------------
//
// W5 STUB REMOVAL. `System.getLogger(String)`, its `ResourceBundle` overload
// and `jdk.internal.logger.LazyLoggers.getLogger(String, Module)` used to mint
// a receiver whose CLASS was the `java/lang/System$Logger` INTERFACE itself
// (`alloc_object(<interface ClassId>, 1)`). That object was inert in both
// directions:
//
//   * the interface declares no instance fields, so the `set_field(logger, 0,
//     name)` meant to record the logger name wrote past the class's declared
//     layout (the "undersized object layout" hazard `alloc_concurrent_synthetic`
//     documents), and
//   * both instance-dispatch sites — `vm_exec::invoke_on_class_shared`'s
//     `override_cb` and `interpreter::try_stackless_invoke` step 6 — DROP a
//     registered native when `declaring_is_interface && !is_static` unless the
//     triple is force-listed, and `java/lang/System$Logger` is in no force list
//     (those lists live in `vm/`, not here). So the natives registered on the
//     interface could not reliably serve it either.
//
// Measured against HotSpot 25: `System.getLogger("x").isLoggable(Level.ERROR)`
// answered false (HotSpot: true), while `getName()` and the 4-arg `log`
// overload resolved to the ABSTRACT interface declarations, so the JDK's
// default `log(Level,String)` body — which delegates to
// `log(level, (ResourceBundle) null, msg, (Object[]) null)` — threw
// `AbstractMethodError`.
//
// The receiver is now stamped with a CONCRETE class of our own,
// [`CRATON_SYSTEM_LOGGER_CLASS`]. It is never present on any classpath, so it
// is always a plain synthetic stub — in real-JDK mode as well as synthetic-jdk
// mode — which means:
//
//   * nothing drops a native registered on it (it is not an interface), and
//   * `invoke_on_class_shared` checks the exact-class native FIRST for the
//     `cratonvm/internal/*` namespace, and its method-not-found arm walks the
//     receiver's class chain through the native registry, so every dispatch
//     route lands on the natives below.
//
// The same method set stays registered on the `java/lang/System$Logger`
// interface name: when a call site resolves to the abstract interface
// declaration, `invoke_on_class_shared` looks the native up under the DECLARING
// class before it tries the receiver's own class, and any receiver still
// stamped with the interface (e.g. one minted by an older cached path) keeps
// working. Registering there can never shadow a genuine `System.Logger`
// implementation: a concrete implementation declares these methods itself, so
// dispatch resolves to its own class and never consults the interface entry —
// and for the one method with a body in the interface (the default
// `log(Level,String)`) the interface-native drop rule above still applies, so
// real bytecode wins.
//
// NOT fixed by deleting the shadow and letting the real JDK chain run:
// `System.getLogger` first needs `Reflection.getCallerClass()` (which returns
// null on CratonVM boot frames — the resulting `IllegalCallerException` /
// `DefaultLoggerFinder.isSystem` NPE is precisely why this shim exists), then
// the `LoggerFinderLoader` → `ServiceLoader` → `BootstrapLogger` chain, of
// which this crate has not one native or shim, i.e. it has never been
// exercised under CratonVM.

/// Binary name of the concrete class CratonVM stamps onto the `System.Logger`
/// instances `System.getLogger` / `LazyLoggers.getLogger` hand out.
pub const CRATON_SYSTEM_LOGGER_CLASS: &str = "cratonvm/internal/SystemLogger";
/// Slot 0 — the requested logger name (`getName()`).
pub const SYSTEM_LOGGER_SLOT_NAME: usize = 0;
/// Slot 1 — the severity threshold this logger publishes at, as an int.
pub const SYSTEM_LOGGER_SLOT_THRESHOLD: usize = 1;
pub const SYSTEM_LOGGER_SLOTS: usize = 2;
/// `System.Logger.Level.INFO`'s severity, and the default threshold — matching
/// a stock JDK, whose `DefaultLoggerFinder`/`SimpleConsoleLogger` publishes at
/// INFO with no logging configuration. `jdk.internal.event.EventHelper`'s
/// `isLoggable(DEBUG)` readout (see `jca/provider_chain.rs`) documents the same
/// default from the other side: DEBUG is NOT loggable on a stock JDK.
pub const SYSTEM_LOGGER_SEVERITY_INFO: i32 = 800;

/// `System.Logger.Level` severities, by enum constant name.
pub fn system_logger_severity_of(name: &str) -> Option<i32> {
    Some(match name {
        "ALL" => i32::MIN,
        "TRACE" => 400,
        "DEBUG" => 500,
        "INFO" => SYSTEM_LOGGER_SEVERITY_INFO,
        "WARNING" => 900,
        "ERROR" => 1000,
        "OFF" => i32::MAX,
        _ => return None,
    })
}

/// Bounds-checked slot read: a receiver minted by an older path (or the
/// interface-stamped shape this file used to produce) can have fewer slots
/// than we expect.
pub fn system_logger_slot(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize) -> Value {
    if ctx.object_num_fields(obj) > slot {
        ctx.get_field(obj, slot)
    } else {
        Value::Object(None)
    }
}

pub fn system_logger_this(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

/// Read a `System.Logger.Level`'s constant name: the `name` field on a real
/// JDK enum, slot 0 on the synthetic one `p57_alloc_enum` builds.
pub fn system_logger_level_name(ctx: &mut dyn NativeContext, level: Option<&Value>) -> Option<String> {
    let level = match level {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return None,
    };
    let by_name = match ctx.get_field_by_name(level, "name") {
        Value::Object(Some(text)) => ctx.read_string(text),
        _ => None,
    };
    by_name.or_else(|| match system_logger_slot(ctx, level, 0) {
        Value::Object(Some(text)) => ctx.read_string(text),
        _ => None,
    })
}

pub fn system_logger_threshold(ctx: &mut dyn NativeContext, this: Option<ObjectRef>) -> i32 {
    let this = match this {
        Some(this) => this,
        None => return SYSTEM_LOGGER_SEVERITY_INFO,
    };
    match system_logger_slot(ctx, this, SYSTEM_LOGGER_SLOT_THRESHOLD) {
        Value::Int(severity) => severity,
        _ => SYSTEM_LOGGER_SEVERITY_INFO,
    }
}

/// Is the `Level` argument itself `null`?
///
/// Distinct from "the level's name could not be read": a minted or
/// short receiver can present a `Level` whose `name` field is unreadable, and
/// that is a shape problem to be absorbed, not a caller error to be thrown at.
/// Only an actually-absent reference is the `null` the JDK raises on.
pub fn system_logger_level_arg_is_null(level: Option<&Value>) -> bool {
    !matches!(level, Some(Value::Object(Some(_))))
}

/// `System.Logger`'s null-`Level` contract.
///
/// Every `System.Logger` method that takes a `Level` dereferences it, and BOTH
/// implementations a JDK can hand out do so before anything else — before the
/// message is looked at and before a `Supplier` is evaluated:
///
/// * `sun.util.logging.internal.LoggingProviderImpl$JULWrapper` (what
///   `System.getLogger` returns whenever `java.logging` is resolved, i.e. the
///   stock case) is `julLogger.isLoggable(toJUL(level))`; `toJUL(null)` is
///   `null` and `java.util.logging.Logger.isLoggable` then calls
///   `level.intValue()`.
/// * `jdk.internal.logger.SimpleConsoleLogger` (the no-`java.logging` fallback,
///   and the class this VM's own strict fallback constructs) is
///   `isLoggable(PlatformLogger.toPlatformLevel(level))`;
///   `toPlatformLevel(null)` returns `null` (`PlatformLogger.java:511`) and the
///   next line is `level.ordinal()`.
///
/// So the two disagree about `OFF` (see [`system_logger_is_loggable`]) but
/// agree unanimously about `null`. MEASURED on this host, `openjdk 25.0.3
/// 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build), `scratchpad/f20/Slog.java`,
/// against `System.getLogger("f20.probe")` (which reported itself as
/// `sun.util.logging.internal.LoggingProviderImpl$JULWrapper`):
///
/// ```text
/// isLoggable(null) !! java.lang.NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null
/// log(null, "m")   !! java.lang.NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null
/// ```
///
/// Before this, a null `Level` fell through `system_logger_level_name` to
/// `None`, then through `system_logger_severity_of` to the
/// `.unwrap_or(SYSTEM_LOGGER_SEVERITY_INFO)` default, and `isLoggable(null)`
/// answered **true** — a fabricated permission, and the `log` family silently
/// published at INFO instead of throwing. That is the shape a caller cannot
/// see: the wrong answer is the permissive one.
///
/// The message is the `JULWrapper` road's, because that is the road a stock
/// JDK takes and therefore the string an oracle diff will hold. The
/// `SimpleConsoleLogger` road NPEs at a different expression and so would
/// carry different text; the TYPE is what both roads agree on and what any
/// `catch` sees.
/// Does this image ship `java.logging` -- i.e. is `System.getLogger` backed by
/// `LoggingProviderImpl$JULWrapper` rather than by `SimpleConsoleLogger`?
///
/// The two implementations disagree about `isLoggable(OFF)` and about which
/// method a null `Level` is dereferenced through, so several rows of the
/// `System.Logger` surface hang on this one fact. See
/// [`system_logger_is_loggable`] for the disagreement itself.
///
/// **Asked as a RESOURCE, not as a loaded class and not by loading one.**
/// `class_id_by_name` answers only for classes already loaded, and a caller
/// asking `isLoggable` before anything has touched logging gets `None` from it
/// -- which is exactly how the first attempt at this predicate came out wrong.
/// `load_class` would answer correctly and would also run
/// `java.util.logging.Logger`'s static initialiser, bringing up the whole
/// `LogManager`: far too large a side effect for a predicate. `find_resource`
/// asks the image and touches nothing.
///
/// Memoised because the answer is a property of the image and cannot change
/// within a process, and because the later arms are not free.
///
/// THREE ARMS, and the first two are not enough on their own -- both were tried
/// and both answered "absent" for a class the image plainly ships:
///
/// 1. `class_id_by_name` sees only classes ALREADY LOADED, and nothing has
///    touched logging when the first `isLoggable` arrives;
/// 2. `find_resource` searches the application's resource path and does not
///    reach the boot image;
/// 3. `load_class` asks the loader properly. It LOADS but does not INITIALISE,
///    so `java.util.logging.Logger`'s static initialiser -- which brings up the
///    whole `LogManager` -- does not run. Once, behind the memo.
///
/// Worth writing down because arms 1 and 2 are the two an author reaches for
/// first, they are the cheap ones, and they are both wrong here for reasons
/// that have nothing to do with logging.
pub fn system_logger_provider_is_jul(ctx: &mut dyn NativeContext) -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    const UNKNOWN: u8 = 0;
    const JUL: u8 = 1;
    const SIMPLE: u8 = 2;
    static CACHED: AtomicU8 = AtomicU8::new(UNKNOWN);
    match CACHED.load(Ordering::Relaxed) {
        JUL => return true,
        SIMPLE => return false,
        _ => {}
    }
    let present = ctx.class_id_by_name("java/util/logging/Logger").is_some()
        || ctx
            .find_resource("java/util/logging/Logger.class")
            .is_some()
        || ctx.load_class("java/util/logging/Logger").is_ok();
    CACHED.store(if present { JUL } else { SIMPLE }, Ordering::Relaxed);
    present
}

/// The NPE a null `Level` argument earns -- naming the method the provider in
/// force would have dereferenced it through.
///
/// `java.util.logging.Level.intValue()` on the JUL-backed provider,
/// `PlatformLogger$Level.ordinal()` on `SimpleConsoleLogger`. Same question as
/// [`system_logger_provider_is_jul`], second row. MEASURED by
/// `SystemRuntimeObjectSweep.java`.
pub fn system_logger_require_level(
    ctx: &mut dyn NativeContext,
    level: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if system_logger_level_arg_is_null(level) {
        let message = if system_logger_provider_is_jul(ctx) {
            "Cannot invoke \"java.util.logging.Level.intValue()\" because \"level\" is null"
        } else {
            "Cannot invoke \"sun.util.logging.PlatformLogger$Level.ordinal()\" because \"level\" is null"
        };
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    Ok(())
}

/// `System.Logger.isLoggable(Level)`: true iff the level is at least as severe
/// as this logger's threshold.
///
/// # `OFF` — the two JDK implementations DISAGREE, and this is not a guess
///
/// This body used to short-circuit `OFF` to `false`, with a comment claiming
/// that as the contract. It is the contract of exactly one of the two
/// implementations. MEASURED on this host (`scratchpad/f20/Slog.java`,
/// OpenJDK 25.0.3+9), against the logger `System.getLogger` actually returns:
///
/// ```text
/// impl class = sun.util.logging.internal.LoggingProviderImpl$JULWrapper
/// isLoggable(ALL)=false  isLoggable(TRACE)=false  isLoggable(DEBUG)=false
/// isLoggable(INFO)=true  isLoggable(WARNING)=true isLoggable(ERROR)=true
/// isLoggable(OFF)=true
/// ```
///
/// `OFF` is **true** there, because JUL's rule is
/// `level.intValue() >= levelValue && levelValue != offValue` — the `OFF`
/// exclusion is on the LOGGER's level, not on the argument's, and
/// `Level.OFF.intValue()` is `Integer.MAX_VALUE`, which clears any threshold.
/// `SimpleConsoleLogger.isLoggable` (`SimpleConsoleLogger.java:127-131`) is
/// `level != PlatformLogger.Level.OFF && level.ordinal() >= effectiveLevel.ordinal()`
/// and answers **false**.
///
/// Which one is the oracle for a given CratonVM run depends on whether
/// `java.logging` is resolved. The lane that wrote the paragraph above could
/// not determine that without running the VM, and left the arm at `false` and
/// recorded it rather than flipping a coin -- which was the right call on the
/// evidence it had.
///
/// **ANSWERED 2026-08-29 by running the VM.** The question was never which
/// answer is nicer; it is which provider `System.getLogger` resolves to, and
/// that is a property of the image the run is using, decidable at the moment
/// the question is asked. `LoggingProviderImpl` is the JUL-backed provider and
/// exists exactly when `java.util.logging.Logger` does, so:
///
/// * `java.logging` present -> `JULWrapper` -> `isLoggable(OFF)` is **true**
///   (`level.intValue() >= levelValue && levelValue != offValue` excludes the
///   LOGGER's level, not the argument's, and `Level.OFF.intValue()` is
///   `Integer.MAX_VALUE`);
/// * absent -> `SimpleConsoleLogger` -> **false**
///   (`level != Level.OFF && level.ordinal() >= effectiveLevel.ordinal()`).
///
/// So the arm is now conditional rather than constant, which is what the
/// original paragraph was actually asking for. MEASURED by
/// `SystemRuntimeObjectSweep.java`: HotSpot answers `true` on this
/// image, and so does this body now.
/// See
/// `docs/feature-designs/native-dispatch-architecture.md`.
/// The remaining severity rows above all agree with this body.
pub fn system_logger_is_loggable(
    ctx: &mut dyn NativeContext,
    this: Option<ObjectRef>,
    level: Option<&Value>,
) -> bool {
    let name = system_logger_level_name(ctx, level);
    if name.as_deref() == Some("OFF") {
        return system_logger_provider_is_jul(ctx);
    }
    let severity = name
        .as_deref()
        .and_then(system_logger_severity_of)
        .unwrap_or(SYSTEM_LOGGER_SEVERITY_INFO);
    severity >= system_logger_threshold(ctx, this)
}

pub fn system_logger_name_of(ctx: &mut dyn NativeContext, this: Option<ObjectRef>) -> String {
    let this = match this {
        Some(this) => this,
        None => return String::new(),
    };
    match system_logger_slot(ctx, this, SYSTEM_LOGGER_SLOT_NAME) {
        Value::Object(Some(name)) => ctx.read_string(name).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Render a message argument: a `String` directly, anything else through its
/// own `toString()`.
pub fn system_logger_text(ctx: &mut dyn NativeContext, value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Object(Some(obj))) => {
            let obj = *obj;
            ctx.read_string(obj).or_else(|| {
                match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(text)))) => ctx.read_string(text),
                    _ => None,
                }
            })
        }
        Some(Value::Object(None)) => Some("null".to_string()),
        _ => None,
    }
}

/// `Supplier<String>.get()`, evaluated only once the record is known loggable
/// (the JDK's default `log(Level, Supplier)` bodies make the same promise).
pub fn system_logger_supplier_text(
    ctx: &mut dyn NativeContext,
    supplier: Option<&Value>,
) -> Option<String> {
    let supplier = match supplier {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return None,
    };
    match ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]) {
        Ok(Some(value)) => system_logger_text(ctx, Some(&value)),
        _ => None,
    }
}

/// `{0}`-style parameter substitution for the `Object...` overloads.
pub fn system_logger_format(
    ctx: &mut dyn NativeContext,
    message: &str,
    params: Option<&Value>,
) -> String {
    let params = match params {
        Some(Value::Object(Some(array))) => *array,
        _ => return message.to_string(),
    };
    let len = ctx.array_length(params);
    let mut rendered = message.to_string();
    for index in 0..len {
        let element = ctx.get_array_element(params, index);
        let text = system_logger_text(ctx, Some(&element)).unwrap_or_default();
        rendered = rendered.replace(&format!("{{{index}}}"), &text);
    }
    rendered
}

/// Publish one record through the same console sink every other logging shim
/// in this crate uses (`record_printed_line` + the `System.out` override).
pub fn system_logger_emit(
    ctx: &mut dyn NativeContext,
    this: Option<ObjectRef>,
    level: Option<&Value>,
    message: Option<String>,
    throwable: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    // The null-`Level` NPE is raised BEFORE the message is consulted, because
    // that is the order both JDK implementations use: the level is
    // dereferenced by `isLoggable`, which every `log` overload calls first.
    // Ordering is observable — `log(null, (String) null)` throws on HotSpot and
    // used to return silently here, since the `message == None` early-return
    // below came first. See `system_logger_require_level`.
    system_logger_require_level(ctx, level)?;
    let message = match message {
        Some(message) => message,
        // MEASURED residual, NOT fixed here: HotSpot publishes a null message
        // as the text "null" (`log(INFO, (String) null)` printed
        // `INFO: null`), where this returns without emitting. That is an
        // output-text divergence with no permission component, and it is
        // recorded rather than changed because `message: Option<String>` is
        // this helper's "nothing to say" channel for several callers.
        None => return Ok(()),
    };
    if !system_logger_is_loggable(ctx, this, level) {
        return Ok(());
    }
    let level_name = system_logger_level_name(ctx, level).unwrap_or_else(|| "INFO".to_string());
    let logger_name = system_logger_name_of(ctx, this);
    emit_framework_log_with_throwable(
        ctx,
        &level_name,
        &format!("[{logger_name}] {message}"),
        throwable,
    );
    Ok(())
}

/// The real `java.base` class the strict-mode fallback constructs instead of
/// fabricating [`CRATON_SYSTEM_LOGGER_CLASS`].
///
/// Chosen by the rule the iterator/collector fallbacks established: a real
/// class is only usable as a stand-in when **its fields are declared and
/// writable, so filling it is construction rather than fabrication**.
/// `SimpleConsoleLogger(String name, boolean usePlatformLevel)` is a public
/// constructor's worth of state — `name`, `usePlatformLevel`, and a `level`
/// that defaults to INFO — and it is the class `DefaultLoggerFinder` itself
/// hands out when no `LoggerFinder` service and no `java.logging` module are
/// available, which is exactly the situation a strict CratonVM boot is in.
///
/// Measured under `--real-jdk` before it was wired in (`probes/LoggerRoutes`,
/// Azure Linux, JDK 25): constructing it reflectively already worked, and
/// `getName()`, `isLoggable(INFO)=true` and `isLoggable(DEBUG)=false` matched
/// both HotSpot and the shim this replaces. So the object runs its OWN
/// bytecode for the whole `System.Logger` surface — the interface-name
/// registrations cannot shadow a concrete implementation, which is the same
/// property the block comment above `register_system_logger_methods` relies on.
///
/// It is NOT what HotSpot's `System.getLogger` returns when `java.logging` is
/// resolved (that is `LoggingProviderImpl$JULWrapper`). Reaching THAT needs the
/// `LoggerFinderLoader` → `ServiceLoader` chain, which
/// `Reflection.getCallerClass()` returning null on CratonVM boot frames is
/// still enough to break — see the block comment above.
pub const REAL_SYSTEM_LOGGER_CLASS: &str = "jdk/internal/logger/SimpleConsoleLogger";

/// Build a real `System.Logger` for `name`, or `None` if this image has no
/// [`REAL_SYSTEM_LOGGER_CLASS`].
///
/// A construction that FAILS is propagated rather than folded into `None`: a
/// half-run `<init>` can leave a pending Java exception, and replacing it with
/// the original fabrication refusal would report the wrong cause.
pub fn real_jdk_system_logger(
    ctx: &mut dyn NativeContext,
    name: Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if ctx.class_id_by_name(REAL_SYSTEM_LOGGER_CLASS).is_none()
        && ctx
            .ensure_class_initialized(REAL_SYSTEM_LOGGER_CLASS)
            .is_err()
    {
        return Ok(None);
    }
    // `usePlatformLevel = false`: the CratonVM shim published at INFO with no
    // logging configuration, `SimpleConsoleLogger`'s `System.Logger` default is
    // INFO too, and `true` would instead read `sun.util.logging`'s platform
    // level. Keeping INFO is what makes this a drop-in.
    match ctx.new_object_initialized(
        REAL_SYSTEM_LOGGER_CLASS,
        "(Ljava/lang/String;Z)V",
        &[name, Value::Int(0)],
    )? {
        Some(Value::Object(Some(logger))) => Ok(Some(logger)),
        _ => Ok(None),
    }
}

/// Allocate a `System.Logger` receiver named `name`.
///
/// Fallible since 2026-08-05 (Real-JDK wave 2, lane L7): under `--real-jdk` the
/// fabrication of [`CRATON_SYSTEM_LOGGER_CLASS`] is refused rather than being
/// recorded as a violation and then performed anyway.
///
/// **The refusal now has somewhere to land (2026-08-06).** Refusing alone was
/// measured, and it took out `java.io.ObjectInputFilter$Config.<clinit>` — which
/// calls `System.getLogger("java.io.serialization")` unconditionally — and with
/// it *every* `ObjectInputStream` construction, i.e. all of deserialization.
/// That was the last of the five classes in
/// `strict-boot-refuses-five-classes-the-corpus-needs-20260805.md`, the
/// `serialization` row, and it is the reason `RSerial` and
/// `JdkOnlyBreadthProbe/strict/serialization` stayed red after the other four
/// were fixed. §1.1 forbids substituting a FAKE for the refused class; it does
/// not forbid — it requires — handing back the real thing, so the fallback
/// constructs [`REAL_SYSTEM_LOGGER_CLASS`].
pub fn craton_alloc_system_logger(
    ctx: &mut dyn NativeContext,
    name: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // Native stale-local family: `name` must survive the class registration
    // and the allocation below, both of which can move the young generation.
    let name_pin = match name {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let class_id = match ctx.class_id_by_name(CRATON_SYSTEM_LOGGER_CLASS) {
        Some(id) => id,
        None => match ctx
            .try_ensure_synthetic_class(CRATON_SYSTEM_LOGGER_CLASS, SYSTEM_LOGGER_SLOTS)
        {
            Ok(id) => id,
            Err(err) => {
                // Strict mode refused the fabrication. Build the real class
                // instead — see this function's doc comment for what refusing
                // alone cost. The pin is still held here on purpose: `name` is
                // about to be handed to a real `<init>` that allocates, and the
                // young generation can move underneath it.
                let name_now = match name_pin {
                    Some((handle, obj)) => Value::Object(Some(ctx.read_native_pin(handle, obj))),
                    None => Value::Object(None),
                };
                let real = real_jdk_system_logger(ctx, name_now);
                // Release the pin taken above before unwinding: an early
                // return past `unpin_native_roots` leaks the frame.
                if let Some((handle, _)) = name_pin {
                    ctx.unpin_native_roots(handle);
                }
                return match real? {
                    Some(logger) => Ok(logger),
                    // No real class to stand in, so the refusal stands. A
                    // catchable `NoClassDefFoundError`, not the uncatchable
                    // `InternalError` the `?` conversion would give.
                    None => Err(cratonvm_native_api::refusal_to_java_failure(ctx, err)),
                };
            }
        },
    };
    let slots = SYSTEM_LOGGER_SLOTS.max(ctx.class_num_total_fields(class_id));
    let logger: ObjectRef = ctx
        .try_alloc_object_gc_safe(class_id, slots)
        .unwrap_or_else(|| ctx.alloc_object(class_id, slots));
    let name = match name_pin {
        Some((handle, obj)) => Value::Object(Some(ctx.read_native_pin(handle, obj))),
        None => Value::Object(None),
    };
    ctx.set_field(logger, SYSTEM_LOGGER_SLOT_NAME, name);
    ctx.set_field(
        logger,
        SYSTEM_LOGGER_SLOT_THRESHOLD,
        Value::Int(SYSTEM_LOGGER_SEVERITY_INFO),
    );
    if let Some((handle, _)) = name_pin {
        ctx.unpin_native_roots(handle);
    }
    Ok(logger)
}

/// `System.getLogger(String)`, `System.getLogger(String, ResourceBundle)` and
/// `LazyLoggers.getLogger(String, Module)` — all static, all with the requested
/// name in `args[0]`.
pub fn native_system_get_logger(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `System.getLogger` is `Objects.requireNonNull(name)` first. The NPE
    // carries no message. Answering a logger for a null name turned a caller's
    // bug into a logger nobody can find again by name. MEASURED by
    // `SystemRuntimeObjectSweep.java`.
    let name = match args.first() {
        Some(v @ Value::Object(Some(_))) => *v,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let logger = craton_alloc_system_logger(ctx, name)?;
    Ok(Some(Value::Object(Some(logger))))
}

/// `System.getLogger(String, ResourceBundle)` -- the one overload whose SECOND
/// argument may not be null either (`Objects.requireNonNull(bundle)`).
///
/// A separate body rather than an arity test inside
/// [`native_system_get_logger`], because
/// `jdk/internal/logger/LazyLoggers.getLogger(String, Module)` is registered
/// against that same body and ITS second argument is allowed to be null. Two
/// call sites with two different contracts need two functions; deciding at run
/// time would mean asking the frame which descriptor it is serving, which is a
/// question this boundary cannot answer.
pub fn native_system_get_logger_with_bundle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    native_system_get_logger(ctx, args)
}

pub fn native_system_logger_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match system_logger_this(args) {
        Some(this) => this,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(system_logger_slot(ctx, this, SYSTEM_LOGGER_SLOT_NAME)))}

pub fn native_system_logger_is_loggable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    system_logger_require_level(ctx, args.get(1))?;
    let loggable = system_logger_is_loggable(ctx, system_logger_this(args), args.get(1));
    Ok(Some(Value::Int(i32::from(loggable))))
}

/// `log(Level, String)`
pub fn native_system_logger_log_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    let message = system_logger_text(ctx, args.get(2));
    system_logger_emit(ctx, this, args.get(1), message, None)?;
    Ok(None)
}

/// `log(Level, Object)`
pub fn native_system_logger_log_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_system_logger_log_string(ctx, args)
}

/// `log(Level, String, Throwable)`
pub fn native_system_logger_log_string_throwable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    let message = system_logger_text(ctx, args.get(2));
    system_logger_emit(ctx, this, args.get(1), message, args.get(3))?;
    Ok(None)
}

/// `log(Level, String, Object...)`
pub fn native_system_logger_log_params(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    let message = system_logger_text(ctx, args.get(2))
        .map(|message| system_logger_format(ctx, &message, args.get(3)));
    system_logger_emit(ctx, this, args.get(1), message, None)?;
    Ok(None)
}

/// `log(Level, Supplier<String>)`
pub fn native_system_logger_log_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    // Before the Supplier is invoked: HotSpot dereferences the level in
    // `isLoggable` and never reaches `Supplier.get()` for a null level.
    system_logger_require_level(ctx, args.get(1))?;
    if !system_logger_is_loggable(ctx, this, args.get(1)) {
        return Ok(None);
    }
    let message = system_logger_supplier_text(ctx, args.get(2));
    system_logger_emit(ctx, this, args.get(1), message, None)?;
    Ok(None)
}

/// `log(Level, Supplier<String>, Throwable)`
pub fn native_system_logger_log_supplier_throwable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    // Before the Supplier is invoked: HotSpot dereferences the level in
    // `isLoggable` and never reaches `Supplier.get()` for a null level.
    system_logger_require_level(ctx, args.get(1))?;
    if !system_logger_is_loggable(ctx, this, args.get(1)) {
        return Ok(None);
    }
    let message = system_logger_supplier_text(ctx, args.get(2));
    system_logger_emit(ctx, this, args.get(1), message, args.get(3))?;
    Ok(None)
}

/// `log(Level, ResourceBundle, String, Throwable)` — the resource bundle is
/// ignored (CratonVM has no logger-scoped bundle lookup); the key is published
/// as the message, which is what the JDK does for an absent bundle.
pub fn native_system_logger_log_bundle_throwable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    let message = system_logger_text(ctx, args.get(3));
    system_logger_emit(ctx, this, args.get(1), message, args.get(4))?;
    Ok(None)
}

/// `log(Level, ResourceBundle, String, Object...)` — this is the overload the
/// JDK's own default `log(Level, String)` body delegates to, so it is the one
/// that must work for `System.getLogger("x").log(ERROR, "msg")` to emit.
pub fn native_system_logger_log_bundle_params(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = system_logger_this(args);
    let message = system_logger_text(ctx, args.get(3))
        .map(|message| system_logger_format(ctx, &message, args.get(4)));
    system_logger_emit(ctx, this, args.get(1), message, None)?;
    Ok(None)
}

/// Register the full `System.Logger` instance surface under `class_name`.
///
/// Called for [`CRATON_SYSTEM_LOGGER_CLASS`] (the receiver's real class, which
/// every dispatch route reaches) and for `java/lang/System$Logger` (the
/// declaring class a call site resolves to when it lands on the abstract
/// interface declaration). See the block comment above for why both are needed
/// and why the interface registration cannot shadow a real implementation.
pub fn register_system_logger_methods(
    registry: &mut NativeMethodRegistry,
    class_name: &str,
) {
    registry.register(
        class_name,
        "getName",
        "()Ljava/lang/String;",
        native_system_logger_get_name, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "isLoggable",
        "(Ljava/lang/System$Logger$Level;)Z",
        native_system_logger_is_loggable, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/lang/String;)V",
        native_system_logger_log_string, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/util/function/Supplier;)V",
        native_system_logger_log_supplier, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/lang/Object;)V",
        native_system_logger_log_object, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_system_logger_log_string_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V",
        native_system_logger_log_supplier_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/lang/String;[Ljava/lang/Object;)V",
        native_system_logger_log_params, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/util/ResourceBundle;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_system_logger_log_bundle_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        class_name,
        "log",
        "(Ljava/lang/System$Logger$Level;Ljava/util/ResourceBundle;Ljava/lang/String;[Ljava/lang/Object;)V",
        native_system_logger_log_bundle_params, NativeKind::SyntheticStub,
    );
}

pub fn emit_framework_log_with_throwable(
    ctx: &mut dyn NativeContext,
    level: &str,
    message: &str,
    throwable: Option<&Value>,
) {
    let throwable = match throwable {
        Some(Value::Object(Some(throwable))) => {
            match native_throwable_to_string(ctx, &[Value::Object(Some(*throwable))]) {
                Ok(Some(Value::Object(Some(text)))) => ctx.read_string(text),
                _ => None,            }        }
        _ => None,
    };
    let text = match throwable {
        Some(throwable) if !throwable.is_empty() => format!("{level} {message}\n{throwable}"),
        _ => format!("{level} {message}"),
    };
    emit_framework_log(ctx, &text);
}

/// Commons Logging accepts arbitrary objects. Spring uses `LogMessage` for
/// lazy formatting, so only reading `String` receivers loses those records.
pub fn emit_framework_log_object(
    ctx: &mut dyn NativeContext,
    level: &str,
    message: ObjectRef,
    throwable: Option<&Value>,
) {
    // gc-common w20-c: `toString` is Java, and the `format` fallback read the
    // message object after it; read it back from a pin.
    let message_pin = ctx.pin_native_root(message);
    let text = ctx.read_string(message).or_else(|| {
        match ctx.invoke_virtual(message, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(text)))) => ctx.read_string(text),
            _ => None,
        }
    });
    let message: ObjectRef = ctx.read_native_pin(message_pin, message);
    ctx.unpin_native_roots(message_pin);
    let message = text.or_else(|| match ctx.get_field_by_name(message, "format") {
        Value::Object(Some(format)) => ctx.read_string(format),
        _ => None,
    });
    if let Some(message) = message {
        emit_framework_log_with_throwable(ctx, level, &message, throwable);
    }
}

pub fn native_println_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (PrintStream), args[1] = String
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Same CompactValue tag-erasure quirk as `native_print_long` — a long
    // pushed via `CompactValue::long` decodes as `Value::Double(<denormal>)`
    // when popped through invokevirtual's untyped `pop_unchecked()`. Pull
    // the raw bits back out so e.g. `System.out.println(System.currentTimeMillis())`
    // prints the actual epoch millis rather than `0`.
    let val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Float(f)) => f.to_bits() as i64,
        _ => 0,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let text = format_double(val);
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ch = match args.get(1) {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\0'),
        _ => '\0',
    };
    let text = ch.to_string();
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_void(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ctx.record_printed_line(String::new());
    stream_writeln(ctx, args, "");
    Ok(None)
}

pub fn native_println_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => invoke_to_string(ctx, *obj)?,
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

pub fn native_println_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let text = format_float(val);
    ctx.record_printed_line(text.clone());
    stream_writeln(ctx, args, &text);
    Ok(None)
}

/// printf(String, Object[]) — format and print, return this (PrintStream)
pub fn native_printf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_opt = match args.first() {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    // Delegate to String.format logic: args[1] = format string, args[2] = Object[]
    let fmt_args = &args[1..]; // [format_string, Object[]]
    let result = native_string_format(ctx, fmt_args)?;
    if let Some(Value::Object(Some(str_ref))) = result {
        let text = ctx.read_string(str_ref).unwrap_or_default();
        ctx.record_printed_line(text.clone());
        stream_write(ctx, args, &text);
    }
    Ok(Some(Value::Object(this_opt)))
}

/// `printf`/`format(Locale, String, Object[])` — the locale-taking overloads.
///
/// Without these the call reached real `PrintStream.format(Locale, …)`
/// bytecode, which builds a `java.util.Formatter` over `this` and writes
/// through `Appendable.append`. That route printed **nothing at all** — not a
/// wrong string, not an exception, no output — while the no-locale overload
/// registered beside it worked, so `System.out.printf(Locale.ROOT, …)` silently
/// dropped the line. `probes/JitEntryFloorProbe` prints its one measurement
/// exactly that way, and deliberately (`%.2f` under a comma-decimal locale
/// turns its CSV row into four columns), so the probe ran green to completion
/// and reported no number.
///
/// The `Locale` is HONOURED. It used to be dropped, under a comment claiming
/// that matched `String.format(Locale, …)` — a claim W7-34's locale patch had
/// already made false, and which became actively wrong once the no-`Locale`
/// overload started following `Locale.getDefault(Locale.Category.FORMAT)`
/// (W7-91 §5): delegating to the no-locale entry renders
/// `printf(Locale.ROOT, …)` in the HOST's locale.
/// `regression-suite/src/RJdkHello.java` pins that call's output character for
/// character, so it is a red vector on any non-en-US host — green on en-US CI,
/// which is the same hiding place the original defect used.
pub fn native_printf_locale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_opt = match args.first() {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    // args = [this, Locale, format String, Object[]]
    let with_locale = [
        args.get(1).copied().unwrap_or(Value::Object(None)),
        args.get(2).copied().unwrap_or(Value::Object(None)),
        args.get(3).copied().unwrap_or(Value::Object(None)),
    ];
    // GC: resolving a non-null locale runs real JDK bytecode (resource bundles,
    // locale providers) and allocates, so the receiver can move across the
    // format call in a way it could not when this delegated to the no-locale
    // path. Pin and re-derive — the same argument the `java/util/Formatter`
    // `format` registration makes.
    let this_pin = this_opt.map(|t| ctx.pin_native_root(t));
    let result = lang_string::native_string_format_locale(ctx, &with_locale)?;
    let this_cur = match (this_pin, this_opt) {
        (Some(p), Some(t)) => Some(ctx.read_native_pin(p, t)),
        _ => this_opt,
    };
    if let Some(p) = this_pin {
        ctx.unpin_native_roots(p);
    }
    if let Some(Value::Object(Some(str_ref))) = result {
        let text = ctx.read_string(str_ref).unwrap_or_default();
        ctx.record_printed_line(text.clone());
        // `stream_write` and all three of its helpers read only `args[0]`
        // (`stream_fd`, `route_write_through_out`, `surefire_forwarding_write`),
        // so the re-derived receiver alone is a complete argument list.
        stream_write(ctx, &[Value::Object(this_cur)], &text);
    }
    Ok(Some(Value::Object(this_cur)))
}

/// Return the underlying `Writer` from a `PrintWriter` object when it is a
/// non-fd-backed Writer (e.g. `StringWriter` in `ModelNode.toString()`).
///
/// Strategy: try `get_field_by_name("out")` first (set by real JDK bytecode in
/// `PrintWriter(Writer,boolean)` ctor), then slot-0 (synthetic convention from
/// `native_printwriter_init_outputstream`).  Returns `None` when the sink is a
/// `BufferedWriter` (the JDK-wrapping chain for `PrintWriter(OutputStream)` →
/// those still belong to the fd-table path) or when no Writer backing is found.
/// The sink a `PrintWriter` writes through, whatever kind it is: the real-JDK
/// `out` field when the layout has one, else the synthetic `field 0 = stream`
/// slot. Unlike [`printwriter_get_backing_writer`] this does NOT filter to
/// non-`BufferedWriter` `Writer`s — flush/close must reach an `OutputStream`
/// or a `BufferedWriter` sink too.
pub fn printwriter_sink(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") {
        return Some(out);
    }
    match ctx.get_field(this, 0) {
        Value::Object(Some(out)) => Some(out),
        _ => None,
    }
}

