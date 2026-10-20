// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! UNCONSTRUCTED-CARRIER GATE — a class may not newly become both a class this
//! VM ALLOCATES and a class whose natives are RETIRED onto the JDK's bytecode.
//!
//! # The species
//!
//! Two decisions that are individually correct compose into a defect.
//!
//! 1. A retirement table entry deletes a native so the JDK's own bytecode runs
//!    instead. That is the point of `--jdk-only` and it is usually the right
//!    move: the image's own body is the reference implementation.
//! 2. A native may hand back an instance it allocated itself, through
//!    `try_alloc_concurrent_synthetic`. In real-JDK mode that allocation is
//!    upsized to the real class's layout, so the object has every field the
//!    image declares, at the image's own slot indices.
//!
//! What no step performs is the **constructor**. `try_alloc_concurrent_synthetic`
//! allocates; it does not run `<init>`, so no field initialiser the class
//! declares ever executes. Every field arrives as the zero of its type.
//!
//! While the natives stay, nothing notices: the native bodies keep their state
//! in a side table and never read the fields. Retire one, and the JDK's own
//! bytecode reads them — and for any field whose declared initialiser is not
//! zero, **zero is a legal value that means something else**.
//!
//! # The measured instance
//!
//! `java.net.HttpURLConnection`, 2026-09-12. `URL.openConnection()` allocates
//! the carrier, and the 2026-09-11 wave retired thirteen of its triples onto
//! the image. Five declared initialisers never ran:
//!
//! ```text
//! chunkLength             = -1     arrived 0
//! fixedContentLength      = -1     arrived 0
//! fixedContentLengthLong  = -1L    arrived 0
//! responseCode            = -1     arrived 0
//! method                  = "GET"  arrived null
//! ```
//!
//! `-1` means *unset* to `HttpURLConnection`'s own bytecode, so `0` read as
//! SET. Both streaming setters then refused on a connection nobody had
//! configured, each naming the mode the other had supposedly set, and
//! `getRequestMethod()` disagreed with what went on the wire. One cause, five
//! probe rows across two sweeps, and every one of them looked like a separate
//! defect until the constructor was the answer.
//!
//! `java.util.TreeMap` was the same species, found independently on the same
//! day ("TreeMap's declared reference fields were never written").
//!
//! # What this gate does, and what it deliberately cannot
//!
//! It scores the PRECONDITION: the intersection of the retirement tables'
//! carrier classes with the classes some native allocates. Both halves are
//! source facts, so both are checkable here.
//!
//! It cannot score the third condition — whether the class actually declares a
//! non-zero initialiser, and whether a retired method reads that field —
//! because that is a question about a JDK image and needs `javap`. That
//! narrowing is recorded in
//! `docs/internal/retired/lane-6-carrier-hazard-census-20260912.md`, which is
//! also where the confirmed instances are listed. This file is the tripwire for
//! a SIXTIETH, not a verdict on the fifty-nine.
//!
//! **A new row is not automatically a bug.** It is a request to run the
//! narrowing in that record before landing.
//!
//! # Why a source scan and not the registry dump
//!
//! `--dump-native-registry` is the better oracle for who OWNS a triple, and
//! this lane uses it for exactly that. It cannot answer this question: a
//! retired triple is one that is NOT in the registry, so the dump's silence
//! about a class is indistinguishable from the class never having been
//! registered at all. The tables are the only place retirement is written down.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every crate that can register a native or mint a carrier. Kept in step with
/// `registrar_drift.rs`'s list of the same name: a crate dropped from here
/// silently shrinks the minted half, which turns a real row into a stale one
/// and reads as good news.
const CRATES: &[&str] = &[
    "native-builtins",
    "native-collections",
    "native-io",
    "native-awt",
    "vm",
    "native-builtins-crypto",
    "native-builtins-security",
];

/// The retirement tables, relative to the workspace root.
const RETIREMENT_TABLE: &str = "native-api/src/retired_shadow.rs";

/// Frozen 2026-09-12 (60; 59 at the freeze, plus `java/io/PrintStream`
/// from lane 4 wave 6 the same day -- see the comment on that row), then
/// refrozen 2026-10-09 at 61 after the Currency carrier was audited and repaired.
/// Sorted.
///
/// `java/net/HttpURLConnection` is NOT here because it is not in the
/// intersection: the wave that found this species did not stop retiring its
/// natives, it made the mint site write the declared defaults. A class stays
/// on this list while it meets the precondition, whether or not it has been
/// audited — the audit lives in the record, and the two tests below only ask
/// whether the population has moved.
const BASELINE: &[&str] = &[
    // Added 2026-10-15 (gci i21/fx, after the merge of origin/dev): `java/util/Spliterator`. The
    // retired side is dev's jr20 bridge table `JDK_ONLY_YIELDED_BRIDGE_TRIPLES` in `retired_shadow.rs`
    // (`forEachRemaining` / `getComparator`: under `--jdk-only` the strict registry drops these Bridges
    // so the interface DEFAULT bodies run; `--compatible` and every `NativeKind` are unchanged), which
    // this scan reads as retirement rows; the minted side is the long-standing synthetic spliterators of
    // `phases_late/streams.rs` and `phases_late/nio_file.rs` (`try_alloc_concurrent_synthetic(ctx,
    // "java/util/Spliterator", 3)`). Verdict: precondition met, hazard absent -- `Spliterator` is an
    // INTERFACE and declares no instance field, so no declared initialiser can be skipped; the two
    // default bodies read no field (they call the kept abstract natives `tryAdvance` /
    // `characteristics`), and the three slots are the native's own state, which no JDK body reads.
    "java/util/Spliterator",
    // Added 2026-10-14 (gci i15a, lane nh): `sun/management/MemoryManagerImpl`, whose `<init>(String)` /
    // `getName` / `isValid` are retired while `jmx.rs` `alloc_memory_manager_impl` (the non-heap
    // `CodeCacheManager` / `Metaspace Manager` beans) mints the carrier when the JDK constructor gave no
    // object (synthetic JDK). JDK 25 source read (src.zip, no javap): the class declares `final String name;
    // final boolean isValid; MemoryPoolMXBean[] pools; MBeanNotificationInfo[] notifInfo = null`, the
    // constructor assigning `name` (argument), `isValid = true`, `pools = null`; the superclass
    // `NotificationEmitterSupport` declares `listenerLock = new Object()` and `listenerList =
    // Collections.emptyList()`. Non-zero defaults exist (isValid, listenerLock, listenerList; a null
    // `listenerList` would NPE in `addNotificationListener`), and the mint site writes all of them BY NAME
    // before the bean escapes (`init_notification_emitter_support`, `init_memory_manager_fields`; the
    // list as an empty `ArrayList`). Hazard closed at the mint; the row records the population.
    "sun/management/MemoryManagerImpl",
    // Added 2026-10-11 (jdk-only issues retirement): `java/util/Currency`, whose `getSymbol(Locale)` /
    // `getDisplayName(Locale)` are retired by the `JdkLocaleProviders` row while
    // `phases_early::register_currency_natives` still mints the carrier (`getInstance`, slot 0 the code
    // string, slot 1 the numeric code). `javap -p -c` read first: `Currency` declares only
    // `final String currencyCode; final transient int defaultFractionDigits; final transient int
    // numericCode`, all assigned from the constructor's arguments -- no declared initialiser, so the zero
    // of a field the mint leaves unset is the JDK's own unset value -- and the two retired bodies read
    // `currencyCode` alone, which is slot 0 and which the mint writes.
    "java/util/Currency",
    // Added 2026-10-09 (Quarkus suite): `LogContext` / `Logger` of jboss-logmanager 3.2.2
    // (`RETIRED_SHADOW_QUARKUS_LOG_TRIPLES`). KNOWN HAZARD, unreachable under `--jdk-only`:
    // `javap -p -c` shows `LogContext` declares final fields its constructor builds
    // (`strong`, `pinnedSet`, `levelMapReference`, `closeHandlers`, `treeLock`, `rootLogger`) and
    // `Logger` a `loggerNode` -- not the zero of their type. The only mint sites
    // (`logmanager.rs` `native_get_log_context` / the `LoggerNode` and `Logger` builders, and
    // `wildfly_core.rs`'s covariant `LogManager.getLogger`) are reached through exactly the natives
    // this change retires (plus the `java.util.logging` emulation retired on 2026-08-11), so a refused
    // native mints nothing and the real constructors run; the
    // carriers are minted only under `--compatible`, where the natives still dispatch and nothing
    // reads the retired rows. A new mint site outside those natives must write the declared
    // defaults BY NAME (see `huc_write_declared_field_defaults`) instead of relying on this row.
    "org/jboss/logmanager/LogContext",
    "org/jboss/logmanager/Logger",
    // Added 2026-10-07 (SBR5 integration; lanes avh, ffmlink, walker, jfr): `javap -p -c` read
    // before baselining. `StackTraceElement` declares `byte format` (iconst_0) and strings set by its
    // constructors; `Arena` is an interface (no instance field); `MemorySessionImpl` declares
    // `Thread owner; ResourceList resourceList; int state` with no non-zero initialiser (OPEN is 0).
    "java/lang/StackTraceElement",
    "java/lang/foreign/Arena",
    "jdk/internal/foreign/MemorySessionImpl",
    // Added 2026-10-06 (SBR5 lane walker), `ClassFrameInfo` / `StackFrameInfo`: the stack walk
    // allocates them (`lang_stackwalker::populate_sfi`, `p59_sw_frame`) and fills every field
    // itself, and the walker's accessors are now the JDK's bytecode. Narrowing run before
    // baselining: `ClassFrameInfo` declares `Object classOrMemberName; int flags` and
    // `StackFrameInfo` `String name; Object type; int bci; ContinuationScope contScope;
    // StackTraceElement ste` -- no field has a declared initialiser (the `JLIA` constant is
    // static), so the constructors write only `flags` (the walker's RETAIN_CLASS_REFERENCE bit,
    // which the native writes by name) and the zero-of-type precondition holds.
    "java/lang/ClassFrameInfo",
    "java/lang/StackFrameInfo",
    // Added 2026-09-16 when `origin/dev`'s L1 wave 9 (`887d36ffc`) retired 168
    // HashMap/LinkedHashMap triples onto this carrier. The narrowing this
    // gate's message asks for, run before baselining:
    //
    //  * `javap -p -c` on the constructor chain. `LinkedHashMap.<init>()`
    //    writes `putMode = 0` and `accessOrder = 0` -- both the zero of their
    //    type, so the hazard cannot express itself through them.
    //    `AbstractMap.<init>()` is `invokespecial Object.<init>; return` and
    //    writes nothing. `head` and `tail` are references whose declared
    //    default IS null.
    //  * **Exactly one field on the whole chain has a non-zero declared
    //    initialiser**: `HashMap.<init>()` does `ldc 0.75f; putfield
    //    loadFactor`. It is emphatically read by retired code -- `resize()`
    //    computes `newThr = (int)(newCap * loadFactor)`, so a `loadFactor` of
    //    0.0f gives a threshold of 0, `++size > threshold` is then true on
    //    EVERY put, and the table doubles on every insertion. That is the
    //    hazard, and it would be unbounded rather than merely wrong.
    //  * It is already written BY NAME at the mint site, which is what makes
    //    the precondition unmet rather than the consequence tolerable:
    //    `lhm_init_with_cap_lazy` -> `lhm_init_inner` does
    //    `try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75))`
    //    and the matching `threshold` write, in `native-collections/src/lib.rs`.
    //    `alloc_hs_backing` is the mint site and calls it before the carrier
    //    escapes. `ensure_hashtable_load_factor` is a second, idempotent
    //    by-name repair on the put/evict and serialization paths.
    //
    // By-name matters here for the reason this gate keeps repeating: a slot
    // index depends on the image's field order, which is not this crate's to
    // assume.
    "java/util/LinkedHashMap",
    // Round 15 wave 1 (lane shadow): the nine classes the wrapped-tuple parse
    // newly sees, all pre-existing retirements, frozen here. Step 1: no
    // non-zero declared initialiser (JDK 25 source).
    "java/beans/PropertyChangeEvent",
    "java/io/ByteArrayInputStream",
    "java/io/ByteArrayOutputStream",
    "java/io/File",
    "java/io/FileDescriptor",
    "java/io/FileOutputStream",
    // Round 15 wave 4 (lane jdkonly4): `java/io/InvalidClassException` LEFT
    // this list. Its one mint, `unsafe_natives_ext.rs` `refuse_non_array_class`
    // (round 15 wave 3: `construct_or_mint` with a literal fallback mint),
    // now throws what HotSpot throws, `NoClassDefFoundError:
    // java/lang/InvalidClassException`, through the class-loader funnel
    // (`jboss_module_loader::alloc_single_message_exception`, class in a
    // variable). The kill-switch arm (`CRATONVM_UNSAFE_NON_ARRAY_NCDFE=0`)
    // builds the old `InvalidClassException` through the same funnel.
    // Added by lane 4 wave 6 (2026-09-12), which retires 29 triples onto this
    // class. The narrowing this gate's message asks for, run before landing:
    //
    //  * `javap -p -c` on 17, 21 and 25: the ONLY field with a non-zero
    //    declared initialiser anywhere on the class or its superclasses is
    //    `FilterOutputStream.closeLock = new Object()`. Every other field is
    //    written from a constructor PARAMETER (`out`, `autoFlush`, `charOut`,
    //    `textOut`, `charset`) or initialised to the zero of its type
    //    (`trouble`, `closing`, `closed`, `formatter`).
    //  * **No retired method reads it.** `PrintStream.close()` overrides
    //    `FilterOutputStream.close()` and synchronizes on `this`
    //    (`aload_0; dup; astore_1; monitorenter`), never on `closeLock`, so
    //    the field the hazard would turn into a wrong answer is not on any
    //    retired path.
    //  * It is written BY NAME at the mint site regardless -- see
    //    `install_real_stream_fields` in `native-builtins/src/lang_system.rs`
    //    -- because `System.out.closeLock` read `null` against HotSpot's
    //    `java.lang.Object` and that was a wrong answer whether or not
    //    anything read it.
    //  * The other mint site, `try_alloc_concurrent_synthetic(ctx,
    //    "java/io/PrintStream", 1)` in `t3_impl.rs`, writes a message into
    //    slot 0 and returns `Int(2)`; the object itself never reaches Java.
    //
    // Precondition met, hazard not. Same disposition as the rest of this list.
    "java/io/PrintStream",
    // Added by lane 4 wave 17 (2026-09-19), which retires the eleven Java
    // wrappers of `WinNTFileSystem` (`list`, `getLength`, ...) onto the carrier
    // `file_layout` mints. The narrowing this gate's message asks for:
    //
    //  * `javap -p -c` on JDK 25 (windows): the constructor writes `slash`,
    //    `semicolon`, `altSlash` and `userDir` from system properties, all
    //    non-zero, so the precondition is met.
    //  * **No retired wrapper reads an instance field.** Each is
    //    `getFileForWin32Calls(f)` (which reads only `File.getPath` and the
    //    static `getCWD`) followed by an `invokevirtual` of the same-named
    //    `*0` JNI native, which this VM implements and does not retire.
    //    `createDirectory`, `createFileExclusively` and `setReadOnly` do not
    //    even pass through the helper. `slash`/`userDir` are read by
    //    `normalize`/`resolve`/`slashify`, none of which is retired.
    //
    // Precondition met, hazard not. If a later wave retires `normalize`,
    // `resolve`, `getSeparator` or `getDefaultParent`, this verdict is void:
    // those read the fields the mint site does not write.
    "java/io/WinNTFileSystem",
    // Added by lane 4 wave 18 (2026-09-20), the Linux twin of the entry above:
    // `RETIRED_SHADOW_L4_UNIXFS_TRIPLES` retires the eleven Java wrappers of
    // `UnixFileSystem` onto the carrier `native-builtins/src/lib.rs:40492`
    // mints. The narrowing, run on linux/x86_64 against Temurin 25.0.4+7:
    //
    //  * `javap -p java.io.UnixFileSystem`: three instance fields, `slash`,
    //    `colon` and `userDir`, and the constructor writes all three from
    //    system properties -- non-zero, so the precondition is met.
    //  * **Not one of the eleven retired wrappers contains a `getfield`.**
    //    Measured, not read: `javap -p -c` piped through a per-method
    //    `getfield` count reports NONE for `checkAccess`, `createDirectory`,
    //    `createFileExclusively`, `getBooleanAttributes`,
    //    `getLastModifiedTime`, `getLength`, `getSpace`, `list`,
    //    `setLastModifiedTime`, `setPermission` and `setReadOnly`. Each is an
    //    `invokevirtual`/`invokespecial` of the same-named `*0` JNI native,
    //    which this VM implements and does not retire.
    //
    // Precondition met, hazard not. Void if a later wave retires `normalize`,
    // `resolve`, `getSeparator`, `getDefaultParent` or `canonicalize`: those
    // DO read `slash`/`colon`/`userDir`.
    "java/io/UnixFileSystem",
    "java/lang/Class",
    // Round 15 wave 3 (lane jdkonly2): the one production mint,
    // `spring_startup_bootstrap.rs::build_class_not_found`, runs
    // `ClassNotFoundException(String)` now (`throwable_mint::construct_or_mint`);
    // it used to write the message BY INDEX into slot 0, which is `backtrace`
    // on the real `Throwable` layout. The literal mint is the fallback.
    // Precondition met, hazard repaired.
    "java/lang/ClassNotFoundException",
    // Round 15 wave 1 (lane shadow): `java/lang/Module`, `java/lang/ModuleLayer`,
    // the four `java/lang/management/*MXBean` interfaces and `java/util/Locale`
    // left this list. They were held here only by tuples in
    // `retired_shadow.rs`' TEST module (negative fixtures), which the scan no
    // longer reads; no table retires them.
    "java/lang/Object",
    "java/lang/Package",
    // Round 15 wave 3 (lane jdkonly3; JO2-1, the wrapped-mint parse): the
    // `JavaLangAccess` shim `get_or_build_jla_shim` (`lib.rs`) mints, carrying
    // the four retired `*CarrierThreadLocal` rows. `System$1` is the anonymous
    // `JavaLangAccess` of the static `System.setJavaLangAccess()`: no instance
    // field at all (no outer instance either), and the retired bodies are
    // `((ThreadLocal) local).getCarrierThreadLocal()` and siblings, which read
    // nothing of the receiver. Precondition met, hazard not.
    "java/lang/System$1",
    "java/lang/Thread$State",
    "java/lang/invoke/MethodType",
    "java/lang/management/MemoryUsage",
    "java/lang/module/Configuration",
    "java/lang/module/ModuleDescriptor",
    "java/lang/module/ModuleDescriptor$Provides",
    "java/lang/module/ModuleDescriptor$Requires",
    "java/lang/reflect/Method",
    // Added by gc-common w34 (2026-09-26). Both entered the population with
    // `origin/dev` `19a7a01b6` (2026-09-24, Spring Boot fat jars through the
    // real `LaunchedClassLoader`), which retired `URL.<init>(String)` and
    // seven `URL` accessors (`RETIRED_SHADOW_URL_ACCESSOR_TRIPLES`) and all
    // 26 `URLClassLoader`/`URLClassPath` rows onto classes this VM already
    // minted. The narrowing, on JDK 25.0.3:
    //
    //  * `java/net/URL`: `javap -p -c` -- every field-initialising
    //    constructor opens `iconst_m1; putfield port; iconst_m1; putfield
    //    hashCode`, the class's only two non-zero initialisers. `port` IS read
    //    by a retired body: `getPort()` is `getfield port`, so a minted URL
    //    answered 0 ("port 0") for HotSpot's -1 ("none"). Repaired BY NAME at
    //    every mint site reachable under `--jdk-only`:
    //    `jboss_module_loader::url_write_declared_field_defaults`, called from
    //    `lang_class.rs`, `lib.rs`, `net_phase_e.rs`,
    //    `phases_late/jar_manifest.rs`, `phases_late.rs` (x3) and `servlet.rs`;
    //    `build_synthetic_url` already wrote `port` by name, and the
    //    `jboss_module_loader.rs` 6-slot mint returns early under `--jdk-only`.
    //    `hashCode` stays 0 on purpose: no retired method reads it, and the
    //    JDK's `hashCode()` reads -1 as "ask `handler`", which a minted URL
    //    lacks. Precondition met, hazard repaired.
    //  * `java/net/URLClassLoader`: `javap -p -c` on it, `SecureClassLoader`
    //    and `ClassLoader` -- no literal non-zero initialiser anywhere on the
    //    chain (`defaultAssertionStatus = false`; every other field is a
    //    parameter or a `new`-built reference). The only mint sites are the
    //    two `URLClassLoader.newInstance` natives in `servlet.rs`, which are
    //    not registered under `--jdk-only` (the retirement record says the 26
    //    rows are "every registration either class had" there), so no minted
    //    loader reaches a retired body. Precondition met, hazard not. VOID if
    //    `newInstance` is ever registered under `--jdk-only`: its carrier has
    //    null `ucp`/`closeables`, which every retired body dereferences -- the
    //    larger, reference-initialiser species this gate cannot score.
    "java/net/URL",
    "java/net/URLClassLoader",
    // Step 1: no non-zero declared initialiser (JDK 25 source). Round 15 wave 1.
    "java/nio/ByteOrder",
    "java/nio/ByteBuffer",
    // Added by lane 4 wave 19 (2026-09-20) -- and the class was ALREADY both
    // minted and retired before it. Wave 17 retired fourteen
    // `DirectByteBuffer` rows, but this file's `retired_carriers()` reads only
    // SINGLE-LINE `("class", "name", "desc"),` tuples and `rustfmt` wraps a
    // tuple at `fn_call_width` (60), so every one of wave 17's was invisible
    // here. Wave 19's four (`getDouble`/`getFloat`/`putDouble`/`putFloat`)
    // happen to fit on one line, which is the only reason the class appears.
    // See the note at the end of this list for what that costs the gate.
    //
    // The narrowing, on JDK 25:
    //
    //  * The retired bodies read `address`, `bigEndian`, `nativeByteOrder` and
    //    `segment` (`javap -p -c`, `getfield` census: 28 `segment`, 20
    //    `address`, 18 `bigEndian`, 6 `nativeByteOrder`).
    //  * The only mint site is `native-builtins/src/panama.rs:3816`, the
    //    synthetic-JDK arm of `p67_buffer_for_segment`; the real-JDK arm runs
    //    the constructor. That arm writes `address`, `capacity`, `limit`,
    //    `position`, `mark` (-1, non-zero) , `bigEndian` (1, non-zero) and
    //    `nativeByteOrder` BY NAME before the buffer escapes -- the repair
    //    this gate's step 2 asks for, already in place.
    //  * `segment`'s declared default IS null: an ordinary direct buffer that
    //    no arena backs has no segment, and the JDK's own scoped-access path
    //    treats null as "no session".
    //
    // Precondition met, hazard not.
    "java/nio/DirectByteBuffer",
    "java/nio/CharBuffer",
    // The gate's one long-standing red, closed here rather than inherited:
    // the 2026-09-19 lane-4 handoff lists
    // `unconstructed_carrier_gate::no_new_class_is_both_minted_and_retired`
    // for `java/nio/HeapCharBuffer` under "Known-failing gates (not this
    // lane's)". It is this lane's class, so wave 19 does the narrowing:
    //
    //  * The retired rows read `hb`, `offset` and `segment` (`javap -p -c`
    //    `getfield` census: 21 `hb`, 7 `offset`, 5 `segment`; the class has no
    //    instance fields of its own).
    //  * Every mint site writes `hb` and `offset` BY NAME before the buffer
    //    escapes, together with `position`, `limit`, `capacity` and `mark`
    //    (-1, the one non-zero default): `charset.rs:240` and the four in
    //    `phases_late/charset_buffers.rs`.
    //  * `segment` is null by declaration, as on `DirectByteBuffer` above.
    //
    // Precondition met, hazard not. (Round 15 wave 2, lane jdkonly: not quite
    // -- `charset.rs`'s mint wrote `mark` by name and then its indexed
    // fallback wrote the backing array over slot 0, which IS `mark` on the
    // real layout. The fallback now runs only on the stub layout, as in
    // `alloc_byte_buffer`.)
    "java/nio/HeapCharBuffer",
    // Round 15 wave 1 froze this as a KNOWN HAZARD; round 15 wave 2 (lane
    // jdkonly) repaired the one production mint site,
    // `charset::alloc_byte_buffer`. `ByteBuffer`/`Buffer` declare three
    // non-zero initialisers: `mark = -1` was written by name but then
    // clobbered by the indexed fallback (slot 0 IS `mark` on the real
    // layout; the array went in as a truncated pointer int), and
    // `bigEndian = true` / `nativeByteOrder` were never written (`order()`
    // answered LITTLE_ENDIAN). The fallback now runs only on the stub layout
    // and both order fields are seeded by name
    // (`native_io::seed_buffer_byte_order`); `hb`, `offset`, `position`,
    // `limit`, `capacity` and `address` were already by name, `segment` is
    // null by declaration. Precondition met, hazard repaired
    // (`CRATONVM_CARRIER_MINT_DECLARED_DEFAULTS=0` restores the old mint).
    "java/nio/HeapByteBuffer",
    "java/nio/channels/FileChannel",
    // Step 1: no non-zero declared initialiser (JDK 25 source). Round 15 wave 1.
    "java/nio/charset/CoderResult",
    // Round 15 wave 3 (lane jdkonly2): the seven `java.nio.file` exceptions
    // [`is_throwable_by_name`] newly sees -- each carries `java/lang/Throwable`'s
    // retired rows by inheritance. They were a live hazard (`initCause` threw
    // "Can't overwrite cause", `addSuppressed` dropped, no stack trace under
    // `--jdk-only`); every one is now built by
    // `phases_late/nio_file.rs::p57_path_exception` through its constructor
    // (`throwable_mint::construct_or_mint`), and the literal mint that keeps it
    // in this population is the fallback for when no class can be constructed,
    // which hands the carrier to the shadow `Throwable` constructor before it
    // escapes. Precondition met, hazard repaired
    // (`CRATONVM_CARRIER_MINT_DECLARED_DEFAULTS=0` restores the bare mints).
    "java/nio/file/AccessDeniedException",
    "java/nio/file/DirectoryNotEmptyException",
    "java/nio/file/FileAlreadyExistsException",
    "java/nio/file/FileSystemException",
    "java/nio/file/NoSuchFileException",
    "java/nio/file/NotDirectoryException",
    "java/nio/file/NotLinkException",
    "java/nio/file/attribute/FileTime",
    "java/text/BreakIterator",
    // Step 1: no non-zero declared initialiser (JDK 25 source). Round 15 wave 1.
    "java/time/Duration",
    "java/time/ZoneId",
    "java/util/ArrayList",
    "java/util/ArrayList$Itr",
    "java/util/ArrayList$ListItr",
    // Step 1: no non-zero declared initialiser (JDK 25 source). Round 15 wave 1.
    "java/util/Collections$EmptyEnumeration",
    "java/util/Collections$EmptyIterator",
    // Added 2026-10-09 after the intersection gate found this newly retired
    // carrier. JDK 25 `javap -p -c java.util.Currency` shows exactly three
    // instance fields (`currencyCode`, `defaultFractionDigits`, `numericCode`);
    // its private constructor writes all three from arguments, and a USD
    // instance therefore needs a non-zero fraction digit. The native now
    // allocates all three slots and writes all three fields by name before the
    // object escapes. `getDefaultFractionDigits` and `getNumericCode` also read
    // those named fields, so the synthetic instance agrees with HotSpot's
    // constructor layout rather than relying on field order.
    "java/util/Currency",
    "java/util/Date",
    "java/util/HashMap",
    // Step 1: no non-zero declared initialiser (JDK 25 source). Round 15 wave 1.
    "java/util/HashMap$Node",
    "java/util/HashSet",
    // Round 15 wave 1 froze this as a KNOWN HAZARD (`Throwable`'s `cause =
    // this`, `stackTrace = UNASSIGNED_STACK`, `suppressedExceptions =
    // SUPPRESSED_SENTINEL` never ran); round 15 wave 2 (lane jdkonly)
    // repaired it. Every production throw now goes through
    // `locale_resources::missing_resource_exception`, which RUNS the
    // constructor (`new_object_initialized`: the JDK's own under
    // `--jdk-only`, the registered shadow under `--compatible`). The literal
    // mint that keeps the class in this population is its fallback for when
    // no class can be constructed (a synthetic JDK), and it hands the carrier
    // to the shadow constructor (`lang_misc::native_missing_resource_init`)
    // before it escapes. Precondition met, hazard repaired
    // (`CRATONVM_CARRIER_MINT_DECLARED_DEFAULTS=0` restores the bare mint).
    "java/util/MissingResourceException",
    "java/util/Optional",
    "java/util/OptionalLong",
    "java/util/Properties",
    "java/util/TreeMap",
    "java/util/concurrent/CompletableFuture",
    "java/util/concurrent/ConcurrentHashMap",
    // Round 15 wave 3 (lane jdkonly3; JO2-1, the wrapped-mint parse). Retired:
    // `<init>(ILThreadFactory;RejectedExecutionHandler;)V` and
    // `getCorePoolSize()`. The class DOES carry non-zero initialisers
    // (`executeExistingDelayedTasksAfterShutdown = true`, and through
    // `ThreadPoolExecutor` `ctl`, `mainLock`, `workers`, `termination`), and
    // the five wrapped mints -- the `Executors.newScheduledThreadPool` /
    // `newSingleThreadScheduledExecutor` natives in `phases_early.rs`
    // (`register_scheduled_executor_natives`) and `phases_late/concurrent.rs`
    // (`register_p63_scheduled_executor`, only after its constructing path
    // `stpe_new_executor` failed) -- write two slots BY INDEX. But no real-JDK
    // boot keeps a factory: `NativeMethodRegistry::register` drops every
    // `Executors` pool factory by name under `drop_real_layout_synthetic`
    // (`native-api/src/registry.rs`), so the JDK's own `new
    // ScheduledThreadPoolExecutor(..)` builds every executor. Precondition met
    // in the source text, hazard not reachable in a real-JDK boot. VOID if
    // that drop is narrowed: the mint would then need the constructor.
    "java/util/concurrent/ScheduledThreadPoolExecutor",
    "java/util/concurrent/ThreadPoolExecutor",
    "java/util/concurrent/TimeUnit",
    // Round 15 wave 3 (lane jdkonly3; JO2-1, the wrapped-mint parse): the one
    // mint, `phases_late/jar_manifest.rs` `p59_manifest_new_attributes`, RUNS
    // `Attributes()` (`new_object_initialized`) and mints only when that
    // cannot build it. `map` is the one field and it is reference-initialised
    // by the constructor (`new LinkedHashMap<>(size)`), which every retired
    // body (`get`, `put`, `size`, `entrySet` ...) dereferences -- so the bare
    // fallback WOULD NPE, but it is reached only with no constructible
    // `java.util.jar.Attributes` (a synthetic JDK, where none of these rows is
    // retired). Precondition met, hazard not reachable with a real JDK.
    "java/util/jar/Attributes",
    "java/util/jar/Attributes$Name",
    "java/util/jar/JarEntry",
    "java/util/logging/Level",
    "java/util/logging/LogManager",
    "java/util/logging/Logger",
    "java/util/stream/DoubleStream",
    "java/util/stream/IntStream",
    "java/util/stream/LongStream",
    "java/util/stream/Stream",
    // Round 15 wave 1 froze this as a KNOWN HAZARD; round 15 wave 2 (lane
    // jdkonly) did the narrowing. `ZipEntry` declares six `-1` initialisers
    // (`xdostime`, `crc`, `size`, `csize`, `method`,
    // `externalFileAttributes`), but:
    //
    //  * the only retired triple is `setComment(String)`, which writes
    //    `comment` from its argument and reads none of those six;
    //  * the one live mint (`phases_late/zip_streams.rs`
    //    `register_p58_gzip_streams`, `ZipInputStream.getNextEntry`) is
    //    reached only through `register_phase58_natives` <-
    //    `register_synthetic_overrides`, which is
    //    `#[cfg(feature = "synthetic-jdk")]`: the shipping build never
    //    compiles it in, and where it is compiled in, `register_p62_zip_entry`
    //    reads the same four indexed slots it writes;
    //  * the other text mint, `zip_entry_alloc`, had no caller and was deleted
    //    in round 15 wave 3 (lane jdkonly2).
    //
    // Precondition met in the source text, hazard not reachable in a shipping
    // build. Void if a real-JDK build ever registers the p58/p62 `ZipEntry`
    // natives, or retires a `ZipEntry` accessor that reads `size`/`crc`/...
    "java/util/zip/ZipEntry",
    // Round 15 wave 3 (lane jdkonly3; JO2-1, the wrapped-mint parse): the four
    // group carriers of `phases_late/foreign_ffm.rs` (`MemoryLayout.
    // structLayout` / `sequenceLayout` / `unionLayout` / `paddingLayout`),
    // retired onto the JDK by `RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES` and never
    // narrowed. JDK 25 source: `AbstractLayout` (`byteSize`, `byteAlignment`,
    // `name`), `AbstractGroupLayout` (`kind`, `elements`, `minByteAlignment`)
    // and `SequenceLayoutImpl` (`elemCount`, `elementLayout`) declare no
    // literal initialiser; each is a constructor argument, and `name` must be
    // `Optional.empty()`, not null. Every mint writes all of them by name
    // before the carrier escapes: `p67_layout_slots_for_mint` resolves
    // `byteSize`/`byteAlignment`/`name`/`elements`/`elementLayout`/`elemCount`
    // against the class, `p67_group_set_members` writes an unmodifiable list,
    // `p67_group_set_kind` writes `kind` (the enum's own static) and
    // `minByteAlignment`, `p67_layout_set_name` writes `Optional.empty()`.
    // Precondition met, hazard repaired at the mint sites.
    "jdk/internal/foreign/layout/PaddingLayoutImpl",
    "jdk/internal/foreign/layout/SequenceLayoutImpl",
    "jdk/internal/foreign/layout/StructLayoutImpl",
    "jdk/internal/foreign/layout/UnionLayoutImpl",
    // Added 2026-10-02 (SBR round 2 close; 65 classes the SBR retirements of rounds 1 and 2
    // joined to the minted set). The narrowing, step 1 of this gate's message: `javap -p -c`
    // over every constructor of each class, looking for a `putfield` of a non-zero constant, a
    // `new`/`newarray`/`getstatic` result (a non-null default). 53 classes have none, so the
    // precondition is met and the hazard is not. 12 do (constructor-assigned, NOT necessarily
    // declared initialisers; the scan cannot tell them apart): `java/lang/Thread` (tid, name,
    // scopedValueBindings), `java/net/URI` (port = -1), `javax/net/ssl/SSLEngine` (peerPort =
    // -1), `javax/net/ssl/SSLParameters` (enableRetransmissions, applicationProtocols),
    // `java/security/AccessControlContext`, `java/security/ProtectionDomain`,
    // `java/security/Provider`, `java/util/concurrent/ForkJoinPool` (keepAlive, queues),
    // `javax/crypto/Cipher`, `javax/crypto/spec/SecretKeySpec`, `sun/management/MemoryPoolImpl`
    // and `java/lang/invoke/MemberName`. Their step 2 (does a retired method read the field, and
    // does a mint site write it by name) was DONE 2026-10-02 and the verdicts are in
    // `docs/internal/jdk-only/SBR2-CARRIER-narrowing-audit-20261002.md`: URI and SSLParameters
    // were patched (the `http2.rs` `uri()` placeholder writes `port = -1`; every SSLParameters
    // mint writes `applicationProtocols` and `enableRetransmissions` by name), the rest are
    // safe or unreachable under `--jdk-only`; Provider keeps one latent fallback. They stay
    // baselined: the tripwire records the population. Every vector of
    // the 152-vector suite passes under `--jdk-only` with these retirements on (linux).
    "java/lang/Module",
    "java/lang/ModuleLayer",
    "java/lang/Runtime$Version",
    "java/lang/StackWalker",
    "java/lang/Thread",
    "java/lang/foreign/FunctionDescriptor",
    // SBR5 ffmlink: `Linker` and `SymbolLookup` are INTERFACES (no instance field, so no declared
    // initialiser): the compat natives still mint a carrier of each, and the retired
    // `nativeLinker` / `libraryLookup` / `loaderLookup` rows hand the method to bytecode only
    // where no carrier of them exists (`--jdk-only`).
    "java/lang/foreign/Linker",
    "java/lang/foreign/SymbolLookup",
    "java/lang/invoke/MemberName",
    "java/lang/invoke/MethodHandle",
    "java/lang/invoke/MethodHandles$Lookup",
    "java/lang/invoke/VarHandle",
    "java/net/DatagramSocket",
    "java/net/InetAddress",
    "java/net/URI",
    "java/net/http/HttpClient",
    "java/net/http/HttpHeaders",
    "java/net/http/HttpRequest",
    "java/net/http/HttpRequest$Builder",
    "java/nio/channels/AsynchronousChannelGroup",
    "java/nio/channels/AsynchronousFileChannel",
    "java/nio/channels/AsynchronousServerSocketChannel",
    "java/nio/channels/AsynchronousSocketChannel",
    "java/nio/channels/DatagramChannel",
    "java/nio/channels/SelectionKey",
    "java/nio/channels/Selector",
    "java/nio/channels/ServerSocketChannel",
    "java/nio/channels/SocketChannel",
    "java/security/AccessControlContext",
    "java/security/CodeSource",
    "java/security/KeyFactory",
    "java/security/KeyPair",
    "java/security/KeyPairGenerator",
    "java/security/KeyStore",
    "java/security/MessageDigest",
    "java/security/ProtectionDomain",
    "java/security/Provider",
    "java/security/Provider$Service",
    "java/security/Signature",
    "java/security/cert/CertificateFactory",
    "java/util/Locale",
    "java/util/ResourceBundle",
    "java/util/concurrent/ForkJoinPool",
    "java/util/concurrent/ForkJoinTask",
    "javax/crypto/Cipher",
    "javax/crypto/Mac",
    "javax/crypto/SecretKeyFactory",
    "javax/crypto/spec/SecretKeySpec",
    "javax/management/ObjectName",
    "javax/net/ssl/KeyManagerFactory",
    "javax/net/ssl/SSLContext",
    "javax/net/ssl/SSLEngine",
    "javax/net/ssl/SSLEngineResult",
    "javax/net/ssl/SSLParameters",
    "javax/net/ssl/TrustManagerFactory",
    "javax/security/auth/x500/X500Principal",
    "jdk/internal/module/ModuleReferenceImpl",
    "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
    "jdk/internal/net/http/HttpResponseImpl",
    "jdk/internal/reflect/ReflectionFactory",
    "sun/management/GarbageCollectorImpl",
    "sun/management/MemoryImpl",
    "sun/management/MemoryPoolImpl",
    "sun/security/ssl/SSLEngineImpl",
];

/// **CLOSED in round 15 wave 1 (lane shadow): [`retired_carriers`] now reads
/// wrapped tuples, and the nine minted classes it newly sees are frozen in
/// BASELINE (three of them as KNOWN HAZARD rows, repaired or narrowed in
/// round 15 wave 2).** The record below is kept
/// as the history of the blind spot.
///
/// **This gate saw only the retirement tuples `rustfmt` left on ONE line, and
/// that was 236 of 323 carriers.** Measured 2026-09-20: [`retired_carriers`]
/// keys on `line.trim_start().starts_with("(\"")` and then splits the SAME
/// line on quotes, so a tuple `rustfmt` wrapped --
///
/// ```text
///     (
///         "java/nio/DirectByteBuffer",
///         "getChar",
///         "(I)C",
///     ),
/// ```
///
/// -- contributes nothing. `rustfmt` wraps a tuple at `fn_call_width` (60),
/// not `max_width`, so whether a row is visible here depends on how long its
/// class and descriptor happen to be. Teaching the parser to read wrapped
/// tuples adds NINETEEN classes to the population in one step
/// (`GarbageCollectorMXBean`, `PlatformLoggingMXBean`, `URL`, `ByteOrder`,
/// `HeapByteBuffer`, `CoderResult`, `DateFormat`, `SimpleDateFormat`,
/// `Duration`, `Collections$EmptyEnumeration`, `HashMap$Node`,
/// `MissingResourceException`, `ResourceBundle`, `ZipEntry`, `MBeanServer`
/// and the four `jdk/internal/foreign/layout/*Impl`), spread across five
/// lanes, none of which has been narrowed. Lane 4 records the measurement
/// rather than turning a shared gate red for four other lanes on its way out;
/// the reproduction is in
/// `docs/internal/retired/lane-4-io-nio-foreign-RETIRED-20260920.md` §9.41.

/// Vacuity floors. A scan that stops matching passes every set comparison
/// below while proving nothing, which is a failure mode the source-scanning
/// gates in this directory have each hit at least once.
const MIN_FILES: usize = 250;
const MIN_MINTED: usize = 300;
const MIN_RETIRED_CARRIERS: usize = 150;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-builtins has a parent (the workspace root)")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // `.claude` holds sibling worktrees — whole extra copies of this repo.
        // Descending into one doubles every count and makes the answer depend
        // on which other lanes happen to be checked out.
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            rs_files(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

/// Classes some native ALLOCATES.
///
/// Comment lines are skipped: this file's own prose names
/// `try_alloc_concurrent_synthetic` beside a class, and so does `servlet.rs`,
/// and counting either would put a class in the population on the strength of
/// a sentence.
fn minted_classes() -> (BTreeSet<String>, usize) {
    let ws = workspace();
    let mut paths = Vec::new();
    for c in CRATES {
        let dir = ws.join(c).join("src");
        if dir.is_dir() {
            rs_files(&dir, &mut paths);
        }
    }
    let mut out = BTreeSet::new();
    for p in &paths {
        let Ok(src) = std::fs::read_to_string(p) else {
            continue;
        };
        minted_in_source(&src, &mut out);
    }
    (out, paths.len())
}

/// The mint needle [`minted_classes`] scans for.
const MINT_NEEDLE: &str = "try_alloc_concurrent_synthetic(";

/// [`minted_classes`] for one file's text.
fn minted_in_source(src: &str, out: &mut BTreeSet<String>) {
    let lines: Vec<&str> = src.lines().collect();
    // `#[cfg(test)]` regions are skipped, on the same walk
    // `lock_discipline_ratchet.rs` uses. A carrier minted in a test
    // fixture is not one the runtime ever hands to the JDK's bytecode,
    // and counting them put eight classes in this population on the
    // strength of `try_alloc_concurrent_synthetic(&mut ctx, ..).unwrap()`
    // inside `mod tests` -- among them `java/lang/String` and
    // `java/lang/Throwable`, which no native mints in anger.
    let mut pending_test = false;
    let mut depth: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        let line = *line;
        let t = line.trim_start();
        let is_comment = t.starts_with("//") || t.starts_with('*');
        if !is_comment && depth == 0 && !pending_test && t.starts_with("#[cfg(test)]") {
            pending_test = true;
            continue;
        }
        if pending_test {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            if line.contains('{') && depth <= 0 {
                pending_test = false;
                depth = 0;
            } else if depth > 0 {
                pending_test = false;
            }
            continue;
        }
        if depth > 0 {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            if depth < 0 {
                depth = 0;
            }
            continue;
        }
        if is_comment {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find(MINT_NEEDLE) {
            rest = &rest[at + MINT_NEEDLE.len()..];
            if let Some(class) = class_literal_of_call(rest, &lines[i + 1..]) {
                out.insert(class);
            }
        }
    }
}

/// `(ctx, "java/x/Y", n)` — step over the context argument, then take the
/// first string literal. A call whose class comes from a variable yields
/// nothing, which is the conservative direction: this gate under-reports
/// rather than inventing a row nobody can act on.
///
/// Round 15 wave 3 (lane jdkonly3; proposal JO2-1 of
/// `docs/internal/jit-proposals/jit-r15w3-jdkonly2-proposals-RETIRED-20261003.md`): a call rustfmt
/// WRAPPED -- the needle ends its line, the context argument and the class
/// literal follow on lines of their own -- is read too. `rest` is the text
/// after the needle on its own line; while it holds no argument after a
/// comma, up to three `following` lines are appended. It used to take the
/// needle's line alone, so every wrapped mint contributed nothing
/// (`r15w3-jdkonly2-carrier-gate-blind-to-wrapped-mint-calls-FIXED-20260930.md`).
fn class_literal_of_call(rest: &str, following: &[&str]) -> Option<String> {
    let mut text = rest.to_string();
    for next in following.iter().take(3) {
        if text
            .find(',')
            .is_some_and(|c| !text[c + 1..].trim().is_empty())
        {
            break;
        }
        text.push(' ');
        text.push_str(next.trim());
    }
    let comma = text.find(',')?;
    let after = text[comma + 1..].trim_start().strip_prefix('"')?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// Classes carrying at least one RETIRED triple.
///
/// Only `("class", "name", "descriptor"),` tuples count. A class NAMED in the
/// file's prose is not a retirement, and counting prose is how the first pass
/// of this census reported sixty-six where the tables say fifty-nine.
///
/// Round 15 wave 1 (lane shadow;
/// `r15w1-shadow-unconstructed-carrier-gate-misses-wrapped-rows-FIXED-20260930.md`):
/// two blind spots closed. A tuple too long for one line is written the
/// rustfmt way -- a bare `(` line, then `"class",` on the next -- and was
/// skipped, which hid 60 retired classes; it is read now. And the scan stops
/// at the file's `#[cfg(test)]` / `mod tests {` pair: every table sits above
/// it, and a negative-list fixture below it is not a retirement (seven
/// BASELINE rows were held up by fixtures alone).
fn retired_carriers() -> BTreeSet<String> {
    let path = workspace().join(RETIREMENT_TABLE);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let lines: Vec<&str> = src.lines().collect();
    let mut out = BTreeSet::new();
    let mut after_open_paren = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|n| n.trim() == "mod tests {") {
            break;
        }
        if after_open_paren {
            after_open_paren = false;
            // `"class",` on the line after a bare `(`.
            if let Some(class) = t.strip_prefix('"').and_then(|r| r.strip_suffix("\",")) {
                if class.contains('/') && !class.contains('"') {
                    out.insert(class.to_string());
                }
            }
            continue;
        }
        if t == "(" {
            after_open_paren = true;
            continue;
        }
        if !t.starts_with("(\"") {
            continue;
        }
        // ["(", class, ", ", name, ", ", descriptor, "),"]
        let fields: Vec<&str> = t.split('"').collect();
        if fields.len() < 6 {
            continue;
        }
        let class = fields[1];
        if class.contains('/') {
            out.insert(class.to_string());
        }
    }
    out
}

/// The ancestor whose retired rows every minted throwable inherits.
const THROWABLE: &str = "java/lang/Throwable";

/// Round 15 wave 3 (lane jdkonly2; proposal JO-3 of
/// `docs/internal/jit-proposals/jit-r15w2-jdkonly-proposals-RETIRED-20261001.md`): is `class` a
/// throwable, judged by its name?
///
/// A retirement is written against the class that DECLARES the method, and
/// `Throwable`'s 27 rows (`initCause`, `addSuppressed`, `getStackTrace`, its
/// constructors ...) are the ones every exception runs. A minted
/// `NoSuchFileException` never has a row under its own name, so the plain
/// intersection could not see that it reaches the JDK's `initCause` with
/// `cause` still null -- which is how seven `java.nio.file` mints stayed
/// invisible (`r15w2-jdkonly-native-minted-throwables-skip-constructor-FIXED-20260930.md`).
/// A source scan cannot read the superclass chain; the `Exception` / `Error`
/// suffix is the conservative proxy (a throwable named otherwise, e.g.
/// `ThreadDeath`, is still missed).
fn is_throwable_by_name(class: &str) -> bool {
    class.ends_with("Exception") || class.ends_with("Error")
}

fn population() -> (BTreeSet<String>, usize, usize, usize) {
    let (minted, files) = minted_classes();
    let retired = retired_carriers();
    let throwable_retired = retired.contains(THROWABLE);
    let both: BTreeSet<String> = minted
        .iter()
        .filter(|c| {
            retired.contains(c.as_str()) || (throwable_retired && is_throwable_by_name(c.as_str()))
        })
        .cloned()
        .collect();
    (both, files, minted.len(), retired.len())
}

#[test]
fn the_throwable_proxy_matches_the_rule() {
    let cases = [
        ("java/nio/file/NoSuchFileException", true),
        ("java/lang/NoClassDefFoundError", true),
        ("java/util/zip/ZipEntry", false),
        ("java/lang/ErrorHandler", false),
    ];
    for (class, want) in cases {
        assert_eq!(is_throwable_by_name(class), want, "{class}");
    }
}

/// JO2-1: a mint rustfmt wrapped is read; a class held in a variable, a
/// definition of the funnel and a test-module mint are still not.
#[test]
fn the_mint_parse_reads_wrapped_calls() {
    let fixture = [
        "fn a(ctx: &mut dyn NativeContext) {",
        "    let one = crate::try_alloc_concurrent_synthetic(ctx, \"x/One\", 2)?;",
        "    let two: ObjectRef = match crate::try_alloc_concurrent_synthetic(",
        "        &mut *scope,",
        "        \"x/Two\",",
        "        2,",
        "    ) {",
        "    let three = try_alloc_concurrent_synthetic(",
        "        ctx, \"x/Three\", 4,",
        "    );",
        "    let var = crate::try_alloc_concurrent_synthetic(",
        "        ctx,",
        "        class_name,",
        "        num_fields,",
        "    )?;",
        "}",
        "pub(crate) fn try_alloc_concurrent_synthetic(",
        "    ctx: &mut dyn NativeContext,",
        "    class_name: &str,",
        ") {}",
        "#[cfg(test)]",
        "mod tests {",
        "    let t = try_alloc_concurrent_synthetic(",
        "        &mut ctx,",
        "        \"x/InTest\",",
        "        1,",
        "    );",
        "}",
    ];
    let mut out = BTreeSet::new();
    minted_in_source(&fixture.join("\n"), &mut out);
    let want: BTreeSet<String> = ["x/One", "x/Three", "x/Two"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(out, want);
}

#[test]
fn the_scan_still_finds_its_population() {
    let (both, files, minted, retired) = population();
    println!(
        "unconstructed-carrier census: {files} files, {minted} minted classes, \
         {retired} retired carriers, {} in both (baseline {})",
        both.len(),
        BASELINE.len()
    );
    assert!(
        files >= MIN_FILES,
        "only {files} .rs files across {CRATES:?} (floor {MIN_FILES}). The directory \
         walk broke rather than the workspace shrinking — fix the scan before \
         touching the baseline."
    );
    assert!(
        minted >= MIN_MINTED,
        "only {minted} minted classes found (floor {MIN_MINTED}); the \
         `try_alloc_concurrent_synthetic` needle stopped matching, and an empty \
         population passes both set comparisons below while proving nothing."
    );
    assert!(
        retired >= MIN_RETIRED_CARRIERS,
        "only {retired} retired carriers found (floor {MIN_RETIRED_CARRIERS}); the \
         tuple parse for {RETIREMENT_TABLE} stopped matching."
    );
}

#[test]
fn no_new_class_is_both_minted_and_retired() {
    let (both, ..) = population();
    let baseline: BTreeSet<String> = BASELINE.iter().map(|s| s.to_string()).collect();
    let new: Vec<&String> = both.difference(&baseline).collect();
    assert!(
        new.is_empty(),
        "{} class(es) newly BOTH allocated by a native and carrying retired \
         triples:\n{}\n\n\
         This VM allocates the carrier and no constructor runs, so every field \
         arrives as the zero of its type — and the JDK's own bytecode, which \
         the retirement handed the method to, reads those fields. For any field \
         whose declared initialiser is not zero, zero is a legal value meaning \
         something else. Measured on `java.net.HttpURLConnection`: four `-1` \
         sentinels arrived as 0, `-1` means UNSET, and both streaming setters \
         then refused a connection nobody had configured.\n\n\
         What to do, in order:\n\
         1. `javap -p -c <class>` and read the constructors. If no field has a \
            non-zero declared initialiser, add the class to BASELINE here and \
            put that verdict in the commit message — the precondition is met \
            and the hazard is not.\n\
         2. If any does, write those defaults BY NAME at the mint site before \
            the object escapes. `huc_write_declared_field_defaults` in \
            `native-builtins/src/http_url_connection.rs` is the worked example. \
            BY NAME matters: a slot index depends on the image's field order, \
            which is not this crate's to assume.\n\
         3. Then add the class to BASELINE.\n\n\
         Full narrowing and the confirmed instances: \
         `docs/internal/retired/lane-6-carrier-hazard-census-20260912.md`.",
        new.len(),
        new.iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_baseline_has_no_stale_rows() {
    let (both, ..) = population();
    let baseline: BTreeSet<String> = BASELINE.iter().map(|s| s.to_string()).collect();
    let gone: Vec<&String> = baseline.difference(&both).collect();
    assert!(
        gone.is_empty(),
        "{} baseline class(es) are no longer both minted and retired:\n{}\n\n\
         Either the last mint site went away or the retirement was reverted. \
         Both are good news, and both want the row deleted from BASELINE in the \
         same change, so the list keeps meaning what it says. Do NOT delete a \
         row to quiet the other test in this file.\n\n\
         Read it the other way too: this is also what fires when a scan is \
         accidentally blinded. A `registrar_drift` baseline went stale in \
         exactly this shape when registrations moved into a macro the source \
         scanner cannot expand — the registrations were real the whole time, \
         and regenerating the baseline would have thrown away eleven records.",
        gone.len(),
        gone.iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
