#[allow(unused_imports)]
use super::*;

/// The 2026-09-11 lane-L6 wave: `java/net/HttpURLConnection` and
/// `ProxySelector.getDefault` -- 15 rows of a lane of 966, after the corpus
/// refused 109 that the probe tree had cleared.
///
/// A fifth table rather than rows merged into a sibling, for the reason the
/// second gives for existing: these were adjudicated by a different METHOD --
/// eight differential probes written for this lane, and then six builds of
/// paired corpus runs -- and the method is the part worth being able to see at
/// a glance.
///
/// # The probe tree cleared 124 rows. The corpus refused 109 of them.
///
/// This is the finding of the wave and it goes first, because it re-states one
/// of the four preconditions and every future lane will meet it.
///
/// Armed on the candidate prefixes, **all 125 probes in the tree got no worse**
/// and five got dramatically better. Built, and run as a two-binary A/B
/// against a control from the same merged tree, the same five improved --
/// `L6UriSweep` 80 diff lines to 0, `L6X500Sweep` 276 to 0,
/// `L6HttpLogicSweep` 64 to 18, `L6InetSweep` 380 to 368, `L6SocketSweep` 42
/// to 38 -- and **zero probes regressed**. Both instruments said 124 rows.
///
/// The `--real-jdk` corpus, paired against that control and run alone, said
/// 15. The ladder, one build per row:
///
/// ```text
///   #  table                          corpus (paired, alone)   measured out
///   1  124 rows, 10 classes           trial 126/132            URL, DatagramSocket,
///                                     ctrl  132/132            MulticastSocket, Inet*  (70)
///   2   54 rows,  4 classes           trial 130/132            URI (29) -- RJdkBridge1
///   3   25 rows,  3 classes           RSslLiveSession 3/3 red  nothing: a guess
///   4   11 rows,  2 classes           RSslLiveSession 3/3 red  nothing: the same guess
///   5   10 rows, X500Principal alone  RSslLiveSession 3/3 red  X500Principal (10)
///   6   15 rows                       trial 132/132, ctrl 132/132   --
/// ```
///
/// Builds 3 and 4 are listed because they are the cost of guessing:
/// `HttpURLConnection` was the plausible culprit for an HTTPS vector, was
/// dropped twice, was innocent, and is one of the two classes this table
/// carries.
///
/// # Precondition 3 is checked one frame too high
///
/// Every refusal is the same species:
///
/// ```text
///   RJdkServices              NPE  URLStreamHandler.openConnection, "this.handler" is null
///   RServiceLoaderDoubleSource   (same)
///   RJdkDefineClass           NPE  URLStreamHandler.getDefaultPort,  "this.handler" is null
///   RJdkNet                   ULE  sun/nio/ch/DatagramChannelImpl.receive0
///   RJdkNet  (InetAddress)    ULE  java/net/Inet6AddressImpl.lookupAllHostAddr
///   RNetIfaceScope            every scoped IPv6 address must round-trip, 2 did not
///   RJdkBridge1               URL.toURI().getPath() lost a lone surrogate to U+FFFD
///   RSslLiveSession           CK client.responseCode = 200 unclassified
/// ```
///
/// Precondition 3 asks whether the IMAGE METHOD carries `Code` to yield to.
/// All 124 rows passed it. It does not ask what that code then CALLS:
///
///  * `java.net.URL`'s methods are one line each through `this.handler`, a
///    field only the real constructor writes -- and this VM MINTS `URL`
///    objects in `classloader.rs` without running it;
///  * `X500Principal` has one declared instance field, `transient X500Name
///    thisX500Name`, and `native-builtins/src/jca/x500.rs` documents in its own
///    header that this VM repurposes that slot to hold a **String**. Any JDK
///    bytecode on the class dereferences a String as an `X500Name`;
///  * `java.net.URI` is the same shape one level out: `URL.toURI()` allocates a
///    real `java.net.URI` and publishes fields into it rather than running its
///    constructor;
///  * `InetAddress.getByName` and `DatagramSocket` yield into
///    `Inet6AddressImpl.lookupAllHostAddr` and
///    `sun/nio/ch/DatagramChannelImpl.receive0`, both `ACC_NATIVE` and neither
///    implemented here -- so the retirement trades a shadow for an
///    `UnsatisfiedLinkError`, one frame deeper than precondition 3 looks.
///
/// **So precondition 3 is really: the image method carries `Code`, AND that
/// code's own callees are satisfiable in this VM.** Four of the six are a
/// field only a real constructor writes, which is `Class.getModule`'s
/// situation from lane-0 §7. A VM that allocates a JDK carrier without
/// constructing it has, for every such class, a shadow that cannot be retired
/// until the carrier is built properly.
///
/// # The dial is a lead in both directions, and a hand-run vector is not a run
///
/// Two instrument traps, both paid for here:
///
///  * the dial (`CRATONVM_ENFORCE_NATIVE_SHADOW`) was **optimistic** on
///    `URL`/`DatagramSocket`/`Inet*`, **pessimistic on the wrong vector** for
///    `X500Principal` (it failed `RJdkX509Intercept`, which passes on every
///    binary built here), and **silent** about `RSslLiveSession`, the vector
///    that actually refuses it. It DECLINES at dispatch and arms a PREFIX;
///    this table re-tags at REGISTRATION and is per-triple;
///  * `cratonvm --real-jdk -cp build:... RJdkBridge1` passes on a binary whose
///    HARNESS run of the same vector fails. `run.sh` builds the classpath,
///    sets the flags and applies the cross-VM diff. The isolation that settles
///    a vector is `ONLY=<Vector> bash regression-suite/run.sh`, three runs on
///    each binary.
///
/// # Why the lane's other prefixes carry no table
///
/// Each was armed ALONE on the whole 125-probe tree:
///
/// ```text
///   javax/net/                                    2 probes worse   L6TlsParamSweep 66 -> 94
///   java/security/,sun/security/,
///     javax/crypto/,javax/security/               8 probes worse   SecuritySurfaceSweep 0 -> 2594
///   sun/net/                                      0               123 of 125 VACUOUS
///   jdk/net/,jdk/internal/net/                    0               123 of 125 VACUOUS
/// ```
///
/// A VACUOUS arm is not a pass: `sun/net/` and the two `jdk` prefixes reached
/// the dial in 2 probes of 125, which is precondition 1 and the trap 146 of
/// Phase 2's 236 candidates fell into.
///
/// Three structural reasons sit behind the rest, each recorded in full in
/// `docs/feature-designs/native-dispatch-architecture.md`:
///
///  1. **`javax/net/ssl/` and `sun/security/ssl/` are an IMPLEMENTATION.**
///     This VM's TLS is rustls (`native-builtins/src/t27_tls.rs`, 20,630
///     lines). Yielding does not restore a JDK behaviour this VM approximates
///     -- it removes TLS. That is `StrictMath`'s situation: a 0-diff probe is
///     evidence the family WORKS, never on its own a reason to retire it.
///  2. **The 67 `HttpsURLConnectionImpl` rows are a null-`delegate`
///     workaround.** `register_https_delegate_forwarders` exists because this
///     VM ALLOCATES that carrier rather than constructing it, and each
///     forwarder runs the SUPERCLASS body the Impl overrides -- the
///     retirement's own remedy, one level up.
///  3. **A base-class row loses the dispatch to its own subclass.** 15 of
///     `InetAddress`'s 18 bucket-A rows are never dispatched, because every
///     instance is an `Inet4Address` or an `Inet6Address` and the door asks
///     the registry about the DECLARING class.
///
/// # The refusals are not inert
///
/// A refusal is a retirement only when nothing already owns the triple:
/// `JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor`, and a
/// non-null one means an earlier registration is still serving, so strict mode
/// runs that older native and every probe reads exactly as before. Measured on
/// the trial binary over a `--real-jdk-report` of the probe tree: every row of
/// this table appears in the refusal set and ZERO refusals carry a survivor.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L6_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/HttpURLConnection", "<init>", "(Ljava/net/URL;)V"),
    (
        "java/net/HttpURLConnection",
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/net/HttpURLConnection",
        "getHeaderFieldDate",
        "(Ljava/lang/String;J)J",
    ),
    (
        "java/net/HttpURLConnection",
        "getInstanceFollowRedirects",
        "()Z",
    ),
    (
        "java/net/HttpURLConnection",
        "getRequestMethod",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/HttpURLConnection",
        "getRequestProperties",
        "()Ljava/util/Map;",
    ),
    (
        "java/net/HttpURLConnection",
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/net/HttpURLConnection",
        "setChunkedStreamingMode",
        "(I)V",
    ),
    ("java/net/HttpURLConnection", "setConnectTimeout", "(I)V"),
    ("java/net/HttpURLConnection", "setDoOutput", "(Z)V"),
    (
        "java/net/HttpURLConnection",
        "setFixedLengthStreamingMode",
        "(I)V",
    ),
    ("java/net/HttpURLConnection", "setReadTimeout", "(I)V"),
    (
        "java/net/HttpURLConnection",
        "setRequestMethod",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/net/HttpURLConnection",
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/net/ProxySelector",
        "getDefault",
        "()Ljava/net/ProxySelector;",
    ),
];

/// Is this exact triple a retired §1.4 shadow?
///
/// The class-name prefix test is a cheap discriminator: every entry is under
/// `java/util/`, and almost no registration is, so the common case costs one
/// prefix compare and nothing else.
///
/// The prefix was `java/util/logging/` until the 2026-08-12 collections wave;
/// widening it to `java/util/` is not a widening of what is retired — the
/// binary search still decides that — only of what gets asked. An entry under
/// a prefix this test rejects answers `false`, which reads as "not retired" and
/// is invisible; `every_entry_is_reachable_through_the_predicate` is the guard.
/// `java/util/` now admits `java/util/concurrent/`, `java/util/stream/` and the
/// rest of the package tree to one extra binary search each, which is the whole
/// cost, and `the_held_collection_families_are_not_retired` is the test that
/// says admitting them changes no answer.
/// The 2026-09-11 lane-5 RESIDUAL wave: 67 `sun/misc/Unsafe` rows, and the
/// reason they were held until now was a missing INSTRUMENT, not a blocker.
///
/// The retired lane page held all 82 `sun/misc/Unsafe` registrations with
/// "precondition 1 fails by measurement" — 121 probes reported the dial VACUOUS
/// on that scope and the 132 `--real-jdk` corpus reports reached 18 of the 82.
/// Its own §9 said what that was worth: **until a workload exists, the count is
/// not evidence of anything.** It is not evidence the rows are right, not
/// evidence they are retirable, and not evidence they are dead.
///
/// `L5SunMiscUnsafe.java` is that workload, and with it the four
/// preconditions read:
///
/// ```text
///   1. the dial was asked              y/r = 76/76 on the workload
///   2. whole probe tree no worse       134 measured: 0 toward, 1 away, and
///                                      that row is y/r=0/0 VACUOUS with a
///                                      line count that moved — not the dial
///   3. the image target carries Code   outcome=bytecode-won on all 67, and
///                                      `javap -p sun.misc.Unsafe` reports
///                                      ZERO native methods on the class —
///                                      all 99 carry Code and delegate to
///                                      `theInternalUnsafe`
///   4. a per-triple dispatch observed  67 distinct triples, one row each
/// ```
///
/// **The wave was 48 rows for an afternoon.** The first workload reached 48 of
/// them; widening it to the volatile twins, the long atomics and the bulk
/// memory trio reached 19 more, and this page's own account had said that is
/// what takes them — *a probe edit, not a build*. It cost one probe edit and
/// two runs.
///
/// Precondition 2 was measured on the 36-row workload and is NOT re-run for
/// the 19: the battery's question is whether arming this prefix disturbs the
/// OTHER 133 probes, and widening one probe cannot change their answer. The
/// row that did change is this probe's own, and it was re-measured directly —
/// `d(base,armed) = 0` on 50 rows, so arming the prefix changes nothing about
/// its output at all.
///
/// Precondition 3 is not read off a `javap` here: `outcome=bytecode-won` in the
/// `--real-jdk-report` IS the image's bytecode having run and produced the
/// answer. And the answer is the same one: the probe is **byte-identical armed
/// and unarmed**, 36 rows, including the two lines that differ from HotSpot for
/// an unrelated reason (see below). A retirement that changes no answer while
/// refusing 48 natives is the definition of a shadow.
///
/// # What `sun.misc.Unsafe` actually is on JDK 25, measured
///
/// Fully functional. Every field accessor and its volatile twin, all three
/// `compareAndSwap*`, `getAndAdd`/`getAndSet`, the static-field pair,
/// `allocateMemory`/`setMemory`/`freeMemory`, `allocateInstance`,
/// `getLoadAverage`, `park`/`unpark` and `throwException` answer on HotSpot 25
/// and on CratonVM alike — only a terminal-deprecation WARNING is printed, on
/// stderr, which the A/B harness drops. The class being deprecated for removal
/// says nothing about whether it works today.
///
/// # The 34 rows NOT here, and why each is out
///
///   * **`ensureClassInitialized(Ljava/lang/Class;)V` and
///     `shouldBeInitialized(Ljava/lang/Class;)Z` are ABSENT from the JDK 25
///     image** — `javap -p sun.misc.Unsafe` declares neither, and the workload
///     gets `NoSuchMethodException` for both on HotSpot. Nothing can dispatch
///     them on a supported image, so they are bucket-F DELETIONS rather than
///     retirements, the same verdict `AbstractExecutorService`'s four rows got.
///     Left for a deletion commit with its own census: a retirement table entry
///     would claim a dispatch nobody has observed, which is the rule this table
///     is under.
///   * **`getUnsafe()Lsun/misc/Unsafe;` is NOT absent, and the first version of
///     this note said it was.** `javap -p sun.misc.Unsafe` prints
///     `public static sun.misc.Unsafe getUnsafe();` on the 17, 21 AND 25
///     images. The `NoSuchMethodException` that put it in the list above came
///     from `getMethod`, and it is the JDK's core-reflection METHOD FILTER
///     working: `jdk.internal.reflect.Reflection.methodFilterMap` hides this
///     one method from the reflective surface, which is the door that stops a
///     library from acquiring `Unsafe` reflectively.
///
///     So it is not a deletion candidate, and CratonVM's `getUnsafe` native is
///     correct — it throws `SecurityException` for a caller off the boot path,
///     measured against HotSpot. What diverges is that this VM implements no
///     member filter AT ALL, so the method is reflectively visible here and
///     invisible there. That is cross-cutting rather than lane 5's, and it has
///     its own page (fixed 2026-09-17):
///     `core-reflection-has-no-member-filter-20260911.md`.
///     It stays out of this table because the divergence is in the reflective
///     surface, not in the native, and retiring the native would not move it.
///
///     **The lesson is worth more than the row: the image is not the authority
///     on what reflection answers.** A census built from class files cannot see
///     this defect, and a `javap` check would have prevented the wrong claim —
///     which is what eventually caught it.
///   * **the rest were not dispatched by this workload.** Precondition 4 is
///     per-triple and this table honours that: a row with no observed dispatch
///     stays out however obvious its sibling looks. The registered surface on
///     this class is larger than the workload reaches, and closing the gap is
///     more probe rows rather than a weaker rule.
///
/// # Why this is a separate table from `RETIRED_SHADOW_L5_TRIPLES`
///
/// Same reason the Phase 2 table gives for existing: these were adjudicated by
/// a different instrument, on a different day, against a measurement the
/// earlier wave explicitly did not have. Merging them would make the earlier
/// table's account cover rows it never saw.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L5R_TRIPLES: &[(&str, &str, &str)] = &[
    ("sun/misc/Unsafe", "addressSize", "()I"),
    (
        "sun/misc/Unsafe",
        "allocateInstance",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
    ),
    ("sun/misc/Unsafe", "allocateMemory", "(J)J"),
    ("sun/misc/Unsafe", "arrayBaseOffset", "(Ljava/lang/Class;)I"),
    ("sun/misc/Unsafe", "arrayIndexScale", "(Ljava/lang/Class;)I"),
    (
        "sun/misc/Unsafe",
        "compareAndSwapInt",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "sun/misc/Unsafe",
        "compareAndSwapLong",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "sun/misc/Unsafe",
        "compareAndSwapObject",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "sun/misc/Unsafe",
        "copyMemory",
        "(Ljava/lang/Object;JLjava/lang/Object;JJ)V",
    ),
    ("sun/misc/Unsafe", "freeMemory", "(J)V"),
    ("sun/misc/Unsafe", "fullFence", "()V"),
    ("sun/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
    (
        "sun/misc/Unsafe",
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
    ),
    ("sun/misc/Unsafe", "getAndSetInt", "(Ljava/lang/Object;JI)I"),
    (
        "sun/misc/Unsafe",
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
    ),
    (
        "sun/misc/Unsafe",
        "getAndSetObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
    ),
    ("sun/misc/Unsafe", "getBoolean", "(Ljava/lang/Object;J)Z"),
    (
        "sun/misc/Unsafe",
        "getBooleanVolatile",
        "(Ljava/lang/Object;J)Z",
    ),
    ("sun/misc/Unsafe", "getByte", "(J)B"),
    ("sun/misc/Unsafe", "getByte", "(Ljava/lang/Object;J)B"),
    (
        "sun/misc/Unsafe",
        "getByteVolatile",
        "(Ljava/lang/Object;J)B",
    ),
    ("sun/misc/Unsafe", "getChar", "(Ljava/lang/Object;J)C"),
    (
        "sun/misc/Unsafe",
        "getCharVolatile",
        "(Ljava/lang/Object;J)C",
    ),
    ("sun/misc/Unsafe", "getDouble", "(Ljava/lang/Object;J)D"),
    (
        "sun/misc/Unsafe",
        "getDoubleVolatile",
        "(Ljava/lang/Object;J)D",
    ),
    ("sun/misc/Unsafe", "getFloat", "(Ljava/lang/Object;J)F"),
    (
        "sun/misc/Unsafe",
        "getFloatVolatile",
        "(Ljava/lang/Object;J)F",
    ),
    ("sun/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I"),
    (
        "sun/misc/Unsafe",
        "getIntVolatile",
        "(Ljava/lang/Object;J)I",
    ),
    ("sun/misc/Unsafe", "getLoadAverage", "([DI)I"),
    ("sun/misc/Unsafe", "getLong", "(J)J"),
    ("sun/misc/Unsafe", "getLong", "(Ljava/lang/Object;J)J"),
    (
        "sun/misc/Unsafe",
        "getLongVolatile",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "sun/misc/Unsafe",
        "getObject",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "sun/misc/Unsafe",
        "getObjectVolatile",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    ("sun/misc/Unsafe", "getShort", "(Ljava/lang/Object;J)S"),
    (
        "sun/misc/Unsafe",
        "getShortVolatile",
        "(Ljava/lang/Object;J)S",
    ),
    ("sun/misc/Unsafe", "loadFence", "()V"),
    (
        "sun/misc/Unsafe",
        "objectFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
    ),
    ("sun/misc/Unsafe", "pageSize", "()I"),
    ("sun/misc/Unsafe", "park", "(ZJ)V"),
    ("sun/misc/Unsafe", "putBoolean", "(Ljava/lang/Object;JZ)V"),
    (
        "sun/misc/Unsafe",
        "putBooleanVolatile",
        "(Ljava/lang/Object;JZ)V",
    ),
    ("sun/misc/Unsafe", "putByte", "(Ljava/lang/Object;JB)V"),
    (
        "sun/misc/Unsafe",
        "putByteVolatile",
        "(Ljava/lang/Object;JB)V",
    ),
    ("sun/misc/Unsafe", "putChar", "(Ljava/lang/Object;JC)V"),
    (
        "sun/misc/Unsafe",
        "putCharVolatile",
        "(Ljava/lang/Object;JC)V",
    ),
    ("sun/misc/Unsafe", "putDouble", "(Ljava/lang/Object;JD)V"),
    (
        "sun/misc/Unsafe",
        "putDoubleVolatile",
        "(Ljava/lang/Object;JD)V",
    ),
    ("sun/misc/Unsafe", "putFloat", "(Ljava/lang/Object;JF)V"),
    (
        "sun/misc/Unsafe",
        "putFloatVolatile",
        "(Ljava/lang/Object;JF)V",
    ),
    ("sun/misc/Unsafe", "putInt", "(Ljava/lang/Object;JI)V"),
    (
        "sun/misc/Unsafe",
        "putIntVolatile",
        "(Ljava/lang/Object;JI)V",
    ),
    ("sun/misc/Unsafe", "putLong", "(JJ)V"),
    ("sun/misc/Unsafe", "putLong", "(Ljava/lang/Object;JJ)V"),
    (
        "sun/misc/Unsafe",
        "putLongVolatile",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "sun/misc/Unsafe",
        "putObject",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    (
        "sun/misc/Unsafe",
        "putObjectVolatile",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    ("sun/misc/Unsafe", "putShort", "(Ljava/lang/Object;JS)V"),
    (
        "sun/misc/Unsafe",
        "putShortVolatile",
        "(Ljava/lang/Object;JS)V",
    ),
    ("sun/misc/Unsafe", "reallocateMemory", "(JJ)J"),
    ("sun/misc/Unsafe", "setMemory", "(Ljava/lang/Object;JJB)V"),
    (
        "sun/misc/Unsafe",
        "staticFieldBase",
        "(Ljava/lang/reflect/Field;)Ljava/lang/Object;",
    ),
    (
        "sun/misc/Unsafe",
        "staticFieldOffset",
        "(Ljava/lang/reflect/Field;)J",
    ),
    ("sun/misc/Unsafe", "storeFence", "()V"),
    (
        "sun/misc/Unsafe",
        "throwException",
        "(Ljava/lang/Throwable;)V",
    ),
    ("sun/misc/Unsafe", "unpark", "(Ljava/lang/Object;)V"),
];

/// Lane 4 wave 4, 2026-09-12: `java/nio/CharBuffer`, and the blocker was gone.
///
/// The seventeen §1.4 shadows on `java/nio/CharBuffer` whose image method
/// declares or inherits `Code`. Ten declare it, seven inherit it from
/// `java/nio/Buffer`. `java/nio/` was admitted as a prefix by wave 1, which
/// retires this class's 34-row `java/nio/ByteBuffer` twin under it -- same
/// accessors, same registrar, same shape -- so there is no prefix decision
/// here, only a table.
///
/// # This family was BACKED OUT once, and the interesting part is why it came back
///
/// §9.2 of the lane page lost `java/nio/CharBuffer` on the first build of wave
/// 1: armed, `subSequence(1, 3)` answered `cd` where the oracle says `bc`,
/// diagnosed as a carrier whose `position`/`limit` the real accessors could not
/// read. Re-measured on 2026-09-12 the defect does not reproduce, and the
/// registrar says why in its own comment: `p62_alloc_char_buffer` and every
/// `wrap`/`subSequence`/`slice` producer in
/// `native-builtins/src/phases_late/charset_buffers.rs` mint
/// `java/nio/HeapCharBuffer` -- the real CONCRETE class -- so the real bodies
/// find real state in real slots. `4ba4f312b` (2026-08-06, "CharBuffer.wrap
/// stamped the abstract class, so subSequence checked nothing") is that fix,
/// and it predates the §9.2 measurement: what §9.2 armed was a five-family
/// scope, and the wrong answer it attributed to this family was not this
/// family's.
///
/// **So the row that reopened this wave is a re-measurement, not a repair.**
/// That is worth being explicit about, because three of this lane's four waves
/// so far have been "fix the carrier, then take the rows" and it would be easy
/// to file this one the same way. Nothing in this wave fixes anything; the
/// fixing was done five weeks earlier by somebody else.
///
/// # `charAt` is carved out, and by the re-tag's own condition
///
/// Eighteen registrations on this class clear the §1.4 bucket test, not
/// seventeen. `charAt(I)C` is the eighteenth and it is NOT here, because
/// [`crate::registry::NativeMethodRegistry::register`] re-tags a retired triple
/// only when `effective_category() == NativeKind::Bridge`, and `charAt` is
/// registered as an `intrinsic` from `native-builtins/src/lib.rs`. A row for it
/// would be INERT BY CONSTRUCTION -- the predicate would answer `true` and
/// nothing would ever ask. The shape
/// [`the_charbuffer_wave_is_seventeen_bridges_without_the_intrinsic`] guards.
///
/// # The funnel
///
/// Seventeen triples, every one of them reached by
/// `L4CharBufferSweep.java` under `--nojit
/// CRATONVM_DISABLE_INTRINSICS=1`, which is what makes the census's
/// `invocations_complete: true` mean what it says. `session()` and
/// `checkSession()` are package-private `java.nio.Buffer` internals no probe
/// can call by name; they are reached 5 and 149 times respectively as callees
/// of the buffer operations above them, which is the only way anything reaches
/// them and is how the JDK reaches them too. Both are no-ops here for a reason
/// the retirement makes real rather than emulated: `Buffer.session()` returns
/// null when `segment` is null, and this VM never writes `segment`.
///
/// # What the retirement does, and the defect it found
///
/// One binary against itself, plus a third carrying the table without the fix
/// the commit before this one makes. `L4CharBufferSweep.java`, 261
/// rows, oracle stable over three captures:
///
/// ```text
///   L4CharBufferSweep, --real-jdk, 261 rows        rows differing
///     A  control (origin/dev bdb02d94e)                  0
///     B  this table, WITHOUT the carrier-side fix        12
///     C  the fix + this table                             0
///     D  the fix, this table un-retired (same binary)     0
/// ```
///
/// Arm B is the finding. `CharBuffer.toString()` is `toString(position(),
/// limit())` in the JDK and this VM held `toString(int, int)` to TWO index
/// conventions -- absolute for a `StringCharBuffer`, relative to the position
/// for everything else -- with its own `toString()` passing `0, limit -
/// position` to cancel the second one out. Retiring `toString()` put a real
/// body on the calling end and the window moved by exactly `position` on
/// twelve rows. Bisected to that single row in one pass with
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW`: seventeen runs, sixteen still at 12 and
/// `toString()Ljava/lang/String;` alone at 0.
///
/// **A native that owns both ends of a convention agrees with itself whatever
/// the convention is.** Three earlier waves in this lane learned that about a
/// carrier's FIELDS; this is the same sentence about a method's ARGUMENTS, and
/// the retirement is what asked for the second opinion.
///
/// Compatible mode is 0 on the control and 0 on the wave. The re-tag is
/// mode-blind and `SyntheticStub` is allowed in compatible mode, so the native
/// still wins there and nothing moves — which is the whole of why a retirement
/// is a `--real-jdk`-only behaviour change.
///
/// **17 refusals, 0 survivors**, every one `synthetic-native-registered` over
/// the seventeen distinct triples. The control reports ONE of them as
/// `native-won` rather than seventeen, which is a fact about what the report
/// records rather than about the other sixteen: the census beside it has all
/// seventeen at `invocations > 0` with `invocations_complete: true`.
///
/// Corpus: `134 passed, 0 failed` on the control and on the wave under
/// `CRATONVM_ARGS=--real-jdk`, compared VECTOR BY VECTOR and not by totals;
/// `SUITE=all` 134/134 and `SUITE=core` 93/93 on the wave. Kind map: seventeen
/// rows amended `bridge -> synthetic-stub` in the 25/linux baseline, after
/// which the wave fires 870 flips -- exactly the control's count, so the gate
/// is as red as dev and no redder.
///
/// Measured on **linux/x86_64 against JDK 25**.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/CharBuffer",
        "allocate",
        "(I)Ljava/nio/CharBuffer;",
    ),
    ("java/nio/CharBuffer", "array", "()[C"),
    ("java/nio/CharBuffer", "arrayOffset", "()I"),
    ("java/nio/CharBuffer", "capacity", "()I"),
    ("java/nio/CharBuffer", "checkSession", "()V"),
    ("java/nio/CharBuffer", "clear", "()Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "flip", "()Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "hasArray", "()Z"),
    ("java/nio/CharBuffer", "hasRemaining", "()Z"),
    ("java/nio/CharBuffer", "limit", "()I"),
    ("java/nio/CharBuffer", "limit", "(I)Ljava/nio/CharBuffer;"),
    ("java/nio/CharBuffer", "position", "()I"),
    (
        "java/nio/CharBuffer",
        "position",
        "(I)Ljava/nio/CharBuffer;",
    ),
    ("java/nio/CharBuffer", "remaining", "()I"),
    ("java/nio/CharBuffer", "rewind", "()Ljava/nio/CharBuffer;"),
    (
        "java/nio/CharBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/CharBuffer", "toString", "()Ljava/lang/String;"),
];

/// Lane 5's SECOND residual wave, 2026-09-11 — 33 triples over the two
/// `Unsafe` spellings, and the two halves earned their rows by different
/// instruments.
///
/// # The 13 `sun/misc/Unsafe` rows: the ADDRESS-form accessors
///
/// `RETIRED_SHADOW_L5R_TRIPLES` retired 67 rows and left 32 out with a reason
/// that was honest and narrow: *precondition 4 is per triple however obvious a
/// sibling looks*. `getByte(J)B`, `getLong(J)J` and `putLong(JJ)V` were in that
/// wave; `getInt(J)I` and its eleven siblings were not, because nothing in the
/// tree had ever called them. The page said the remedy was **a probe edit, not
/// a build**, and this is the edit: `L5SunMiscUnsafe.java` grew
/// seven `addrRow` cases that allocate, round-trip one type through the
/// `(long)` accessor pair, and free.
///
/// ```text
///   dial armed on sun/misc/Unsafe   reached 13   yielded 13   declined_no_bytecode 0
///   per triple                      outcome = bytecode-won on all 13, one row each
///   the workload                    d(base, armed) = 0 over 57 rows
///   vs HotSpot                      d(hs, base) = d(hs, armed) = 2  (the ONE
///                                   `getUnsafe` row, which is the member-filter
///                                   defect and not this wave's)
/// ```
///
/// 13 dispatches for 14 calls is not a miscount: `getByte(J)B` is already
/// retired, so it is no longer registered and the dial cannot reach it.
///
/// # The 20 `jdk/internal/misc/Unsafe` rows: the delegating accessors
///
/// These are §10.4's list, and they are admitted by the rule §4 used for the
/// sixteen already retired: **a Java method that delegates to an `ACC_NATIVE`
/// primitive at the SAME offset.** `javap -p jdk.internal.misc.Unsafe` on the
/// 25 image separates the two populations for you — 68 of the class's methods
/// are `ACC_NATIVE` and can never be retired (contract §1.5), and these 20 are
/// all in the other group, `declared` with `has_code`.
///
/// # What the class-wide dial says, and why it is not a reason to stop
///
/// Arming `jdk/internal/misc/Unsafe` WHOLE takes `UnsafeShadowSweep` from 472
/// rows to 292 and `rc=0` to `rc=1`. That is the measurement §10.4 predicted
/// ("the class-wide arm is where `L4BridgeSweep` goes from 499 rows to zero"),
/// and it is a statement about the CLASS, not about these 20 triples: the same
/// armed run yields 46 distinct triples to bytecode, and the 26 this table does
/// not take are exactly the ones that cannot survive it.
///
/// **Three families are excluded, each for a reason that is not "we did not get
/// to it":**
///
///   * every `*Unaligned` row — `getIntUnaligned`, `putLongUnaligned` and their
///     six siblings do byte-offset arithmetic on the offset they are handed.
///     This VM answers `objectFieldOffset` with a SLOT INDEX, so the JDK's
///     arithmetic lands nowhere. Permanently blocked, not pending.
///   * the sub-word atomics — `compareAndExchange{Byte,Short}`,
///     `compareAndSet{Byte,Short}`, `getAndAdd{Byte,Short}`. Same cause: the
///     JDK emulates these by masking within an enclosing word at a computed
///     byte offset.
///   * the four that report the VM's own numbering — `objectFieldOffset`,
///     `staticFieldOffset`, `staticFieldBase`, `arrayIndexScale` — plus
///     `getUnsafe` and `ensureClassInitialized`. Yielding these hands the rest
///     of the class a number from the other model, which is how one arm turns
///     into 180 changed lines.
///
/// `weakCompareAndSetIntPlain` is absent from this table and its seven siblings
/// are present. That is not an oversight: the sweep never dispatched it, and a
/// row whose only evidence is that its siblings passed is exactly what
/// precondition 4 refuses.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L5S_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeIntAcquire",
        "(Ljava/lang/Object;JII)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeIntRelease",
        "(Ljava/lang/Object;JII)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeLongAcquire",
        "(Ljava/lang/Object;JJJ)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeLongRelease",
        "(Ljava/lang/Object;JJJ)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndExchangeReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntAcquire",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntOpaque",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongAcquire",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getReferenceOpaque",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntRelease",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongRelease",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetIntAcquire",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetIntRelease",
        "(Ljava/lang/Object;JII)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongAcquire",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongPlain",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetLongRelease",
        "(Ljava/lang/Object;JJJ)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferencePlain",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "weakCompareAndSetReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("sun/misc/Unsafe", "getAddress", "(J)J"),
    ("sun/misc/Unsafe", "getChar", "(J)C"),
    ("sun/misc/Unsafe", "getDouble", "(J)D"),
    ("sun/misc/Unsafe", "getFloat", "(J)F"),
    ("sun/misc/Unsafe", "getInt", "(J)I"),
    ("sun/misc/Unsafe", "getShort", "(J)S"),
    ("sun/misc/Unsafe", "putAddress", "(JJ)V"),
    ("sun/misc/Unsafe", "putByte", "(JB)V"),
    ("sun/misc/Unsafe", "putChar", "(JC)V"),
    ("sun/misc/Unsafe", "putDouble", "(JD)V"),
    ("sun/misc/Unsafe", "putFloat", "(JF)V"),
    ("sun/misc/Unsafe", "putInt", "(JI)V"),
    ("sun/misc/Unsafe", "putShort", "(JS)V"),
];

/// Lane 5's THIRD residual wave, 2026-09-12 -- 25 triples, and the first of the
/// three to be measured on more than one image.
///
/// # The 23 `jdk/internal/misc/Unsafe` rows: the off-heap surface
///
/// `RETIRED_SHADOW_L5S_TRIPLES` took that class's 20 delegating ATOMICS and
/// said the rest "needs a dispatch each". This is that dispatch. The instrument
/// is new -- `L5JdkInternalUnsafe.java` -- and it exists because
/// L1's 472-row `UnsafeShadowSweep` reaches the off-heap surface through the
/// `sun.misc` spelling nearly everywhere: of the 26 registered triples that are
/// declared-with-code on 17, 21 and 25 and are outside the three permanently
/// blocked families, its `off-heap` section dispatches SEVEN, four of them only
/// on a throwing edge. Nineteen had never been dispatched by anything, which is
/// precondition 4 failing for want of a caller rather than for a reason.
///
/// ```text
///   dial armed on jdk/internal/misc/Unsafe   reached 1396  yielded 99
///   per triple                               outcome = bytecode-won on all 28
///                                            the armed run dispatched
///   the workload                             d(base, armed) = 2 rows, and both
///                                            are `invokeCleaner` -- see below
///   vs HotSpot 25                            d(hs, base) = d(hs, strict) = 2,
///                                            the same pre-existing row
/// ```
///
/// These are admitted by the rule §4 used for the sixteen retired first: **a
/// Java method that delegates to an `ACC_NATIVE` primitive at the same
/// offset.** `javap -p` on all three images separates the two populations, and
/// the separation is exact -- 68 of this class's methods are `ACC_NATIVE` on
/// 21 and 25 (70 on 17) and can never be retired under contract §1.5; every row
/// here is in the other group, `declared` with `has_code` on all three.
///
/// Two rows the dial passed are NOT here: `allocateMemory(J)J` and
/// `reallocateMemory(JJ)J`. They were in this table until the two-binary arm
/// ran, and what threw them out is recorded on
/// `the_l5t_wave_refuses_the_two_allocator_rows_the_dial_passed`. The short
/// version is that an armed dial yields one dispatch and leaves the native
/// registered; a table entry removes it, and this VM's off-heap surface cannot
/// resolve an address the JDK's own allocator path returned.
///
/// # `invokeCleaner` was dispatched, yielded, and is NOT in this table
///
/// It is the one row the armed arm made WORSE, and it is worth a sentence
/// because the dial's own verdict says the opposite. `outcome = bytecode-won`,
/// so precondition 4 passes; but `invokeCleaner` on a *direct* buffer goes from
/// `ok=returned` to `EX:IllegalArgumentException` the moment the class is
/// armed. The JDK's bytecode reaches the buffer's `Cleaner` through a field
/// this VM's direct buffers do not carry, so yielding replaces a working call
/// with a throw. Precondition 2 is not a formality that precondition 4 can
/// overrule.
///
/// The same probe found the defect that observation sits on: this VM's
/// `invokeCleaner` accepts a HEAP buffer and returns, where every supported
/// image throws `IllegalArgumentException("buffer is non-direct")`. That is a
/// native to fix, not a shadow to retire, and it is recorded in §9d of the
/// lane page rather than fixed here.
///
/// # The two `sun/misc/Unsafe` rows: a version boundary, measured at last
///
/// `ensureClassInitialized` and `shouldBeInitialized` are declared `public`,
/// with code, on 17 and 21, and REMOVED on 25. The second residual wave left
/// them out with a precise blocker -- its workload runs on 25, where nothing
/// can dispatch them, and precondition 4 is a dispatch observed by the citing
/// instrument. It said they stay out "until someone measures them on a 17 or 21
/// run". This is that run:
///
/// ```text
///   CratonVM --java-home <jdk-21.0.12+8> --real-jdk, L5SunMiscUnsafe
///   dial armed on sun/misc/Unsafe   reached 2   yielded 2   declined_no_bytecode 0
///   per triple                      outcome = bytecode-won on both
///   the workload                    d(base, armed) = 0 over 57 rows
///   vs HotSpot 21                   d(hs, base) = d(hs, armed) = 1, and that
///                                   row is `getUnsafe` -- the member-filter
///                                   defect, which is not this wave's and is
///                                   now confirmed on a second image
///   image columns on the 21 run     declared, !acc_native, has_code on both
/// ```
///
/// Note what yielding them lands on. The `sun.misc` bytecode delegates to
/// `theInternalUnsafe.ensureClassInitialized(c)`, and the `jdk.internal`
/// spelling of that method is one of the six rows
/// [`RETIRED_SHADOW_L5S_TRIPLES`] names as permanently blocked -- so it is
/// still a native here, and the delegation lands on it. The two spellings are
/// not one decision, and retiring the outer one while the inner one stays is
/// the §4 rule working exactly as written rather than an inconsistency.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L5T_TRIPLES: &[(&str, &str, &str)] = &[
    ("jdk/internal/misc/Unsafe", "addressSize", "()I"),
    ("jdk/internal/misc/Unsafe", "copyMemory", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V"),
    ("jdk/internal/misc/Unsafe", "defineClass", "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
    ("jdk/internal/misc/Unsafe", "freeMemory", "(J)V"),
    ("jdk/internal/misc/Unsafe", "getAddress", "(J)J"),
    ("jdk/internal/misc/Unsafe", "getByte", "(J)B"),
    ("jdk/internal/misc/Unsafe", "getChar", "(J)C"),
    ("jdk/internal/misc/Unsafe", "getDouble", "(J)D"),
    ("jdk/internal/misc/Unsafe", "getFloat", "(J)F"),
    ("jdk/internal/misc/Unsafe", "getInt", "(J)I"),
    ("jdk/internal/misc/Unsafe", "getLong", "(J)J"),
    ("jdk/internal/misc/Unsafe", "getShort", "(J)S"),
    ("jdk/internal/misc/Unsafe", "loadLoadFence", "()V"),
    ("jdk/internal/misc/Unsafe", "pageSize", "()I"),
    ("jdk/internal/misc/Unsafe", "putAddress", "(JJ)V"),
    ("jdk/internal/misc/Unsafe", "putByte", "(JB)V"),
    ("jdk/internal/misc/Unsafe", "putChar", "(JC)V"),
    ("jdk/internal/misc/Unsafe", "putDouble", "(JD)V"),
    ("jdk/internal/misc/Unsafe", "putFloat", "(JF)V"),
    ("jdk/internal/misc/Unsafe", "putInt", "(JI)V"),
    ("jdk/internal/misc/Unsafe", "putLong", "(JJ)V"),
    ("jdk/internal/misc/Unsafe", "putShort", "(JS)V"),
    ("jdk/internal/misc/Unsafe", "setMemory", "(Ljava/lang/Object;JJB)V"),
    ("sun/misc/Unsafe", "ensureClassInitialized", "(Ljava/lang/Class;)V"),
    ("sun/misc/Unsafe", "shouldBeInitialized", "(Ljava/lang/Class;)Z"),
];

/// `--real-jdk` throughput wave, 2026-09-19 -- the three `Preconditions` bounds checks, 3 rows.
///
/// # What was measured
///
/// `docs/feature-designs/native-dispatch-architecture.md`. One release
/// binary, one probe (`NativeCrossingPerf.java`), only the mode differing:
///
/// ```text
///                                   default      --real-jdk    --real-jdk, this table armed
///   Objects.checkIndex                 4.5 ns        278 ns        7.7 ns
///   heap ByteBuffer.get(int)          20.7 ns        341 ns       27.3 ns
/// ```
///
/// `Preconditions.checkIndex` is the whole of the first row. Under `--real-jdk` the JIT's thin
/// direct helper for it is refused (`direct_native_helper` admits a helper only for a reviewed
/// `Intrinsic`, and this is a `Bridge`), so a compiled caller pays the generic dispatch and the
/// registered native RUNS in front of real bytecode: the census carries the same triple as both
/// `[bytecode-won]` and `[native-won]`, depending on the door. Arming the dispatch-time dial
/// (`CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/util/Preconditions`) does NOT measure this
/// retirement -- it made `Objects.checkIndex` 3x SLOWER (485 -> 1576 ns), because a method with a
/// registered native is never compiled (`native_skip`), so the yielded bytecode ran interpreted.
/// Only a refused registration takes the native out of the JIT's sight.
///
/// # Why each row is safe to retire
///
/// The three `int` overloads that take a `BiFunction`. The real bodies are `if (index < 0 || index >=
/// length) throw outOfBounds(oobef, ...); return index;` and are HotSpot intrinsic candidates. The
/// native's reason to exist -- honouring the exception formatter -- is `Preconditions.outOfBounds`'s
/// own job once the real class is the one running (`native-builtins/src/preconditions.rs`). The
/// non-JDK `(II)I` / `(III)I` spellings registered beside them have NO bytecode to yield to and are
/// deliberately not here.
///
/// # What this wave REFUSED, measured -- `Buffer.session()` / `checkSession()`
///
/// The first cut of this wave also retired those two on the ten buffer classes that register them
/// (`CharBuffer` took the same pair in wave 4), on the argument that the real `session()` is
/// `segment != null ? segment.sessionImpl() : null` and `segment` is `null` for a buffer that is not
/// a `MemorySegment` view. **That was wrong, and the corpus said so in seconds.** On the same
/// release binary, only this pair's rows differing:
///
/// ```text
///                                    session/checkSession retired    kept as shims
///   PcapWriteHandlerTest                     8 ok / 17 failed          25 ok / 0 failed
///   AdaptiveBigEndianDirectByteBufTest     339 ok / 76 failed         415 ok / 0 failed
///   every failure:  NoSuchMethodError: 'void jdk.internal.foreign.ArenaImpl.checkValidStateRaw()'
/// ```
///
/// A buffer whose `segment` is NOT null -- a direct buffer over a `MemorySegment`, which netty's
/// allocators make -- gets its real `sessionImpl()` run against this VM's own arena carrier
/// (`docs/feature-designs/native-dispatch-architecture.md`: the segment, arena
/// and session carriers are laid out deliberately unlike the JDK's, so their real bodies must never
/// run), and the invoke resolves against a class that never declares the method. The constant-`null`
/// shim is not a shortcut over correct bytecode there; it is what stops that bytecode running.
/// `CharBuffer` is not a counter-example: nothing in the corpus makes a segment-backed `CharBuffer`.
///
/// The cost of keeping them is small: a compiled `session()` site binds the thin `buffer_session`
/// helper regardless of policy, so the pair was never what made the accessors slow.
/// (Superseded: SBR3 lane NIOBUF and JIT round 17 wave 2, lane nio2, re-measured the carrier and
/// retired the family whole; `the_buffer_session_family_is_retired_whole` pins it IN, by name.)
///
/// Compatible mode is untouched: a `SyntheticStub` registers and dispatches normally there.
pub(in crate::retired_shadow) static RETIRED_SHADOW_JOTP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/util/Preconditions",
        "checkFromIndexSize",
        "(IIILjava/util/function/BiFunction;)I",
    ),
    (
        "jdk/internal/util/Preconditions",
        "checkFromToIndex",
        "(IIILjava/util/function/BiFunction;)I",
    ),
    (
        "jdk/internal/util/Preconditions",
        "checkIndex",
        "(IILjava/util/function/BiFunction;)I",
    ),
];

/// `--real-jdk` throughput wave, part two, 2026-09-19 -- the sixteen `Unsafe.*Unaligned`
/// accessors, which `RETIRED_SHADOW_L5S_TRIPLES`'s doc and
/// `the_l5s_wave_refuses_the_three_families_that_cannot_be_retired` recorded as "permanently
/// blocked, not pending". They are not, and the reason that record gives does not reach them.
///
/// # What the record said, and what it was about
///
/// "Every `*Unaligned` row does byte-offset arithmetic on the offset it is handed, and this VM
/// answers `objectFieldOffset` with a SLOT INDEX, so the JDK's arithmetic lands nowhere." That is
/// true of an offset taken from `objectFieldOffset` -- and no `*Unaligned` accessor is ever handed
/// one: they exist for byte-addressed memory, i.e. arrays and off-heap addresses. Both of those ARE
/// byte-offset models here, and the model is already implemented by the natives the real bytecode
/// composes:
///
/// ```text
///   getIntUnaligned(o, off)      real:  (off & 3) == 0  ->  getInt(o, off)
///                                       (off & 1) == 0  ->  makeInt(getShort(o, off), getShort(o, off + 2))
///                                       otherwise       ->  makeInt(getByte(o, off) .. getByte(o, off + 3))
///   getIntUnaligned(o, off, be)  real:  convEndian(be, getIntUnaligned(o, off))
/// ```
///
/// and `getInt`/`getShort`/`getLong(Object,long)` are `ACC_NATIVE` -- which §1.5 says can never be
/// retired -- bound to `native_unsafe_get_int_mb` and its siblings in
/// `native-builtins/src/unsafe_natives_ext.rs`. Those are the SAME handlers the sixteen shadows
/// here are registered to (the source says so: "the `*_mb` handlers do exactly that"). The shadows
/// therefore add nothing the composition does not already reach through byte-addressed natives; they
/// only stand in front of it.
///
/// # What they cost
///
/// A shadowed method is never compiled, and the census carries these as both `[bytecode-won]` and
/// `[native-won]`. Priced in isolation on a release binary
/// (`UnsafeCrossingPerf.java`), array target, one call:
///
/// ```text
///                                       default     --real-jdk
///   Unsafe.getInt(arr, off)  [ACC_NATIVE]  267 ns       268 ns
///   Unsafe.getIntUnaligned(arr, off, be)   277 ns       849 ns
///   Unsafe.putIntUnaligned(arr, off, v, be) 302 ns      875 ns
/// ```
///
/// -- the shadowed 3-arg method under `--real-jdk` costs three times the native it stands in front
/// of. They sit under every heap `ByteBuffer.get{Short,Char,Int,Long}` / `put*`, through the
/// already-retired `ScopedMemoryAccess` layer.
///
/// # The instrument that decides it
///
/// `UnsafeUnalignedMatrixProbe.java`, which this record's predecessor wrote for
/// exactly this family: every width and both byte orders over `byte[]`-backed heap AND direct
/// buffers at every alignment within a word, then reads that SPAN several elements of
/// `char[]`/`int[]`/`long[]` (the shape `ArraysSupport.vectorizedMismatch` produces, and the one a
/// truncating read gets wrong), then `Arrays.mismatch`. Diffed against HotSpot's transcript, under
/// `--real-jdk` with this table armed.
///
/// The sub-word CAS/atomic rows and the four that report the VM's own numbering
/// (`objectFieldOffset`, `staticFieldOffset`, `staticFieldBase`, `arrayIndexScale`) stay exactly
/// as `the_l5s_wave_refuses_the_three_families_that_cannot_be_retired` left them: their offsets
/// really are slot indices, and the reasoning for them is unchanged.
///
/// A separate table, not rows in `RETIRED_SHADOW_JOTP_TRIPLES`, so `CRATONVM_UNRETIRE_NATIVE_SHADOW`
/// can disarm one half of the throughput wave without the other.
pub(in crate::retired_shadow) static RETIRED_SHADOW_JOTP_UNALIGNED_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/misc/Unsafe",
        "getCharUnaligned",
        "(Ljava/lang/Object;J)C",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getCharUnaligned",
        "(Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntUnaligned",
        "(Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getIntUnaligned",
        "(Ljava/lang/Object;JZ)I",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongUnaligned",
        "(Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getLongUnaligned",
        "(Ljava/lang/Object;JZ)J",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getShortUnaligned",
        "(Ljava/lang/Object;J)S",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "getShortUnaligned",
        "(Ljava/lang/Object;JZ)S",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putCharUnaligned",
        "(Ljava/lang/Object;JC)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putCharUnaligned",
        "(Ljava/lang/Object;JCZ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntUnaligned",
        "(Ljava/lang/Object;JI)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putIntUnaligned",
        "(Ljava/lang/Object;JIZ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongUnaligned",
        "(Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putLongUnaligned",
        "(Ljava/lang/Object;JJZ)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putShortUnaligned",
        "(Ljava/lang/Object;JS)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "putShortUnaligned",
        "(Ljava/lang/Object;JSZ)V",
    ),
];

/// SBR round 2, wave 7, lane AWT: the CONCRETE `sun.management` rows.
///
/// Every row here is a `Bridge` that shadows a method with a `Code` attribute
/// in `java.management` (22 retire; 26 of the 111 registered `sun/management` +
/// `ManagementFactory` bridges; the other 85 are `ACC_NATIVE` in JDK 25 and are
/// genuine natives, correctly `Bridge`). Under the dial
/// (`CRATONVM_ENFORCE_NATIVE_SHADOW=sun/management/,java/lang/management/`) the
/// `Sbr2W7AwtMgmt` probe is line-identical to the control in both modes.
/// Record: `docs/feature-designs/native-dispatch-architecture.md`;
/// what stays native (AWT, ImageIO, JFR): `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_AWT_TRIPLES: &[(&str, &str, &str)] = &[
    // `ManagementFactory.<init>()V` is NOT here (lane 2 held it: the image declares
    // no dispatchable constructor; `the_l2_table_holds_only_what_lane_2_measured`).
    // `loadNativeLib()V` is NOT here either: its real body is `System.loadLibrary("management")`,
    // and the VM links no libmanagement -- measured `UnsatisfiedLinkError: no
    // management in system library path` then `NoClassDefFoundError` on every
    // `ManagementFactory` use. The bridge is a genuine stand-in for the missing
    // library; see the HELD page.
    (
        "sun/management/ClassLoadingImpl",
        "<init>",
        "(Lsun/management/VMManagement;)V",
    ),
    (
        "sun/management/GarbageCollectorImpl",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "sun/management/GarbageCollectorImpl",
        "getName",
        "()Ljava/lang/String;",
    ),
    ("sun/management/MemoryImpl", "isVerbose", "()Z"),
    (
        "sun/management/MemoryManagerImpl",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "sun/management/MemoryManagerImpl",
        "getName",
        "()Ljava/lang/String;",
    ),
    ("sun/management/MemoryManagerImpl", "isValid", "()Z"),
    (
        "sun/management/MemoryPoolImpl",
        "<init>",
        "(Ljava/lang/String;ZJJ)V",
    ),
    (
        "sun/management/MemoryPoolImpl",
        "getName",
        "()Ljava/lang/String;",
    ),
    (
        "sun/management/MemoryPoolImpl",
        "getType",
        "()Ljava/lang/management/MemoryType;",
    ),
    ("sun/management/MemoryPoolImpl", "isValid", "()Z"),
    (
        "sun/management/ThreadImpl",
        "setThreadContentionMonitoringEnabled",
        "(Z)V",
    ),
    (
        "sun/management/VMManagementImpl",
        "isBootClassPathSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isCompilationTimeMonitoringSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isCurrentThreadCpuTimeSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isGcNotificationSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isObjectMonitorUsageSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isOtherThreadCpuTimeSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isRemoteDiagnosticCommandsSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isSynchronizerUsageSupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isThreadAllocatedMemorySupported",
        "()Z",
    ),
    (
        "sun/management/VMManagementImpl",
        "isThreadContentionMonitoringSupported",
        "()Z",
    ),
];

/// SBR round 3, lane awt: the AWT/management rows whose real bytecode runs
/// correctly under the shadow dial on Linux headless with the real JDK image.
/// Sorted (binary-searched). What stays native, with the measured blocker:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_AWT_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/awt/GraphicsEnvironment",
        "getLocalGraphicsEnvironment",
        "()Ljava/awt/GraphicsEnvironment;",
    ),
    ("java/awt/GraphicsEnvironment", "isHeadless", "()Z"),
    (
        "java/awt/Toolkit",
        "getDefaultToolkit",
        "()Ljava/awt/Toolkit;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "loadNativeLib",
        "()V",
    ),
    (
        "jdk/jfr/internal/Type",
        "getKnownType",
        "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;",
    ),
    (
        "jdk/jfr/internal/util/Utils",
        "getValidType",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljdk/jfr/internal/Type;",
    ),
];

/// SBR round 3 wave 2, lane UNSAFECAS (2026-10-05): the rows that lane retired.
/// Sorted (binary-searched). Record:
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// * `sun/awt/PlatformGraphicsInfo.createToolkit()`: the seam `Toolkit.getDefaultToolkit`
///   (retired by lane awt) reaches. The native answered a `HeadlessToolkit` built by
///   hand; the real Linux body is `new XToolkit()`, which constructs headlessly under
///   the image's own bytecode (`Sbr3AwtGe` under the dial: `toolkit = HeadlessToolkit`,
///   `screensize` `HeadlessException`, `beep` ok, every line HotSpot's). Unix only:
///   the Windows body is `new WToolkit()`, which the native's own note says cannot be
///   constructed headlessly here, and nothing was measured on Windows.
#[cfg(unix)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_UNSAFECAS_TRIPLES: &[(&str, &str, &str)] = &[(
    "sun/awt/PlatformGraphicsInfo",
    "createToolkit",
    "()Ljava/awt/Toolkit;",
)];
#[cfg(not(unix))]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_UNSAFECAS_TRIPLES: &[(&str, &str, &str)] = &[];

/// SBR5 lane awtjfr: the JDK's own Java2D / ImageIO / font bytecode now runs on
/// CratonVM (`native-awt/src/java2d`, `native-awt/src/jpeg`), so the stand-ins
/// that answered for it retire:
///
/// * `java.awt.image.BufferedImage` (the constructor and the accessors) and
///   `sun.java2d.SunGraphics2D` (the 30 drawing / state rows): the real
///   classes run over the Rust loops behind `GraphicsPrimitiveMgr`.
/// * `javax.imageio.ImageIO.read` / `write` and the PNG / JPEG plugin
///   `read` / `write` bodies: the JDK's readers and writers run, with
///   `libjavajpeg`'s natives implemented in Rust (a libjpeg-6b-exact codec).
/// * `java.awt.Font` / `FontMetrics` accessors and
///   `HeadlessGraphicsEnvironment.getAvailableFontFamilyNames`: the real
///   `sun.font` stack runs over the font natives in `native-awt/src/java2d/fonts.rs`.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_AWTJFR_TRIPLES: &[(&str, &str, &str)] = &[
    ("com/sun/imageio/plugins/jpeg/JPEGImageReader", "dispose", "()V"),
    ("com/sun/imageio/plugins/jpeg/JPEGImageReader", "read", "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;"),
    ("com/sun/imageio/plugins/png/PNGImageWriter", "write", "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V"),
    ("java/awt/Font", "getFamily", "()Ljava/lang/String;"),
    ("java/awt/Font", "getFontName", "()Ljava/lang/String;"),
    ("java/awt/Font", "getFontName", "(Ljava/util/Locale;)Ljava/lang/String;"),
    ("java/awt/Font", "getSize", "()I"),
    ("java/awt/Font", "getStyle", "()I"),
    ("java/awt/FontMetrics", "charWidth", "(C)I"),
    ("java/awt/FontMetrics", "getAscent", "()I"),
    ("java/awt/FontMetrics", "getDescent", "()I"),
    ("java/awt/FontMetrics", "getHeight", "()I"),
    ("java/awt/FontMetrics", "getLeading", "()I"),
    ("java/awt/FontMetrics", "getMaxAdvance", "()I"),
    ("java/awt/FontMetrics", "stringWidth", "(Ljava/lang/String;)I"),
    ("java/awt/image/BufferedImage", "<init>", "(III)V"),
    ("java/awt/image/BufferedImage", "createGraphics", "()Ljava/awt/Graphics2D;"),
    ("java/awt/image/BufferedImage", "getHeight", "()I"),
    ("java/awt/image/BufferedImage", "getRGB", "(II)I"),
    ("java/awt/image/BufferedImage", "getRGB", "(IIII[III)[I"),
    ("java/awt/image/BufferedImage", "getRaster", "()Ljava/awt/image/WritableRaster;"),
    ("java/awt/image/BufferedImage", "getType", "()I"),
    ("java/awt/image/BufferedImage", "getWidth", "()I"),
    ("java/awt/image/BufferedImage", "setRGB", "(III)V"),
    ("javax/imageio/ImageIO", "read", "(Ljava/io/File;)Ljava/awt/image/BufferedImage;"),
    ("javax/imageio/ImageIO", "read", "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;"),
    ("javax/imageio/ImageIO", "write", "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z"),
    ("javax/imageio/ImageIO", "write", "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z"),
    ("javax/imageio/ImageIO", "write", "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z"),
    ("sun/java2d/HeadlessGraphicsEnvironment", "getAvailableFontFamilyNames", "()[Ljava/lang/String;"),
    ("sun/java2d/HeadlessGraphicsEnvironment", "getAvailableFontFamilyNames", "(Ljava/util/Locale;)[Ljava/lang/String;"),
    ("sun/java2d/SunGraphics2D", "clearRect", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "copyArea", "(IIIIII)V"),
    ("sun/java2d/SunGraphics2D", "create", "()Ljava/awt/Graphics;"),
    ("sun/java2d/SunGraphics2D", "dispose", "()V"),
    ("sun/java2d/SunGraphics2D", "drawArc", "(IIIIII)V"),
    ("sun/java2d/SunGraphics2D", "drawImage", "(Ljava/awt/Image;IILjava/awt/image/ImageObserver;)Z"),
    ("sun/java2d/SunGraphics2D", "drawLine", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "drawOval", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "drawPolygon", "([I[II)V"),
    ("sun/java2d/SunGraphics2D", "drawPolyline", "([I[II)V"),
    ("sun/java2d/SunGraphics2D", "drawRect", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "drawString", "(Ljava/lang/String;II)V"),
    ("sun/java2d/SunGraphics2D", "fillArc", "(IIIIII)V"),
    ("sun/java2d/SunGraphics2D", "fillOval", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "fillPolygon", "([I[II)V"),
    ("sun/java2d/SunGraphics2D", "fillRect", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "getBackground", "()Ljava/awt/Color;"),
    ("sun/java2d/SunGraphics2D", "getClipBounds", "()Ljava/awt/Rectangle;"),
    ("sun/java2d/SunGraphics2D", "getColor", "()Ljava/awt/Color;"),
    ("sun/java2d/SunGraphics2D", "getTransform", "()Ljava/awt/geom/AffineTransform;"),
    ("sun/java2d/SunGraphics2D", "rotate", "(D)V"),
    ("sun/java2d/SunGraphics2D", "scale", "(DD)V"),
    ("sun/java2d/SunGraphics2D", "setBackground", "(Ljava/awt/Color;)V"),
    ("sun/java2d/SunGraphics2D", "setClip", "(IIII)V"),
    ("sun/java2d/SunGraphics2D", "setColor", "(Ljava/awt/Color;)V"),
    ("sun/java2d/SunGraphics2D", "setFont", "(Ljava/awt/Font;)V"),
    ("sun/java2d/SunGraphics2D", "setRenderingHint", "(Ljava/awt/RenderingHints$Key;Ljava/lang/Object;)V"),
    ("sun/java2d/SunGraphics2D", "setStroke", "(Ljava/awt/Stroke;)V"),
    ("sun/java2d/SunGraphics2D", "setTransform", "(Ljava/awt/geom/AffineTransform;)V"),
    ("sun/java2d/SunGraphics2D", "translate", "(II)V"),
];

/// Retired triples that OLDER supported images still declare `native`, with
/// the first feature version whose image gives them a body. Every retirement
/// here was measured against JDK 25, where the triple has bytecode to yield
/// to; over an older image there is none, so retiring it there leaves an
/// `ACC_NATIVE` method with no implementation (`UnsatisfiedLinkError` under
/// `--real-jdk`). The registry keeps such a row a `Bridge` on those images
/// ([`retired_triple_is_native_before`]). Found by the vm's own
/// missing-native check on a JDK 21 `java.home` (2026-10-05).
pub(in crate::retired_shadow) static RETIRED_BUT_NATIVE_BEFORE: &[(&str, &str, &str, u32)] =
    &[("java/lang/Class", "getSigners", "()[Ljava/lang/Object;", 25)];

/// The first feature version whose image gives this retired triple a body,
/// when an older supported image still declares it `native` (see
/// [`RETIRED_BUT_NATIVE_BEFORE`]).
pub fn retired_triple_is_native_before(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<u32> {
    RETIRED_BUT_NATIVE_BEFORE
        .iter()
        .find(|(c, m, d, _)| *c == class_name && *m == method_name && *d == descriptor)
        .map(|row| row.3)
}

/// SBR3 ffm-fix: the opt-in pure lever (`CRATONVM_PURE_BYTECODE=1`) keeps the
/// layout rows native (its `java.lang.foreign` / `jdk.internal.foreign`
/// exemption). The JDK's own `ValueLayout.varHandle()` ends in
/// `MethodHandles.collectArguments`, whose `rebind().editor()` this VM's
/// `MethodHandle` carrier has no `LambdaFormEditor` for (`NoSuchMethodError`,
/// RJdkForeign under `CRATONVM_PURE_BYTECODE=1`); the default mode runs it fine
/// (the filterValue/IndirectVarHandle serve). Providing `editor()` is the MH
/// lane's carrier project.
///
/// SBR3 ffmperf: the same exemption for the arena / session / segment /
/// splitter rows, for the same reason (`lever_keeps_platform_native` keeps the
/// whole `java.lang.foreign` / `jdk.internal.foreign` layer native under the
/// lever, so the carriers must stay minted by their natives). Held in a helper
/// because the predicate's own body must name no table.
pub(in crate::retired_shadow) fn pure_lever_keeps_ffm_row_native(key: &(&str, &str, &str)) -> bool {
    crate::unretire::pure_bytecode()
        && (RETIRED_SHADOW_SBR3_FFM_TRIPLES.binary_search(key).is_ok()
            || RETIRED_SHADOW_SBR4_SEGACC_TRIPLES.binary_search(key).is_ok()
            || RETIRED_SHADOW_SBR5_AVH_TRIPLES.binary_search(key).is_ok()
            // SBR5 ffmlink: the `java.lang.foreign` rows of its table (the lookups).
            || (key.0.starts_with("java/lang/foreign/")
                && RETIRED_SHADOW_SBR5_FFMLINK_TRIPLES.binary_search(key).is_ok()))
}

/// Every row that some table of `tables` (always [`RETIRED_SHADOW_TABLES`])
/// answers to a binary search, in ONE hash set, built once per process from the
/// constants.
///
/// SBR round 5, lane warmup. [`triple_is_retired_shadow`] used to binary-search
/// each of the ~140 tables for every call, so a call for a class under a
/// retired prefix cost ~140 searches of ~10 three-string compares each (a
/// `memcmp` per compare). Startup asks it once per registered native -- thousands
/// of times, ~9% of `Hello` CPU in a `perf` profile -- and the retired-ancestor
/// check asks it again per resolved call. The set is a pure function of the
/// build's constants (the tables are `const`; the per-run diagnostic switch
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` is applied AFTER this lookup, in
/// [`triple_is_retired_shadow`]), so it is not compatibility state and is
/// identical for every VM in the process.
///
/// # It answers what the searches answered, not what the tables say
///
/// A row enters the set only when the binary search of ITS OWN table finds it.
/// Five tables are not sorted (`RETIRED_SHADOW_PHASE2_TRIPLES`,
/// `..._SBR_NIOCH_TRIPLES`, `..._SBR2_CTORPLAN_TRIPLES`,
/// `..._SBR2_SMALL2_TRIPLES`, and the unlisted `..._SBR3_NIOBUF_HELD_TRIPLES`),
/// and a binary search over an unsorted slice can miss a row that is present,
/// which would silently leave that row un-retired. Today the searches find
/// every row (`the_row_set_answers_what_the_binary_searches_answered` lists any
/// that do not), so building the set from the rows themselves would answer the
/// same; building it from what the searches find keeps this a pure speed
/// change even on the day a table is misordered further. Sorting those five
/// tables is the table owners' call, not this change's.
///
/// The table list is a parameter only so the predicate's body keeps naming the
/// one const it must read (`the_predicate_names_no_table_directly`).
pub(in crate::retired_shadow) fn retired_shadow_row_set(
    tables: &'static [&'static [(&'static str, &'static str, &'static str)]],
) -> &'static std::collections::HashSet<
    (&'static str, &'static str, &'static str),
    std::hash::BuildHasherDefault<rustc_hash::FxHasher>,
> {
    use std::sync::OnceLock;
    type RowSet = std::collections::HashSet<
        (&'static str, &'static str, &'static str),
        std::hash::BuildHasherDefault<rustc_hash::FxHasher>,
    >;
    static SET: OnceLock<RowSet> = OnceLock::new();
    SET.get_or_init(|| {
        let total: usize = tables.iter().map(|t| t.len()).sum();
        let mut set = RowSet::with_capacity_and_hasher(total, Default::default());
        for table in tables {
            for row in table.iter() {
                if table.binary_search(row).is_ok() {
                    set.insert(*row);
                }
            }
        }
        // Windows: the channel family is retired only where the Windows channel layer is switched on
        // (see `RETIRED_SHADOW_SBR_NIOREAL_WINDOWS_TRIPLES`).
        #[cfg(windows)]
        if windows_channel_layer() {
            for row in RETIRED_SHADOW_SBR_NIOREAL_WINDOWS_TRIPLES
                .iter()
                .chain(RETIRED_SHADOW_NIO_COVARIANT_BRIDGE_WINDOWS_TRIPLES)
                .chain(RETIRED_SHADOW_SBR_ASYNCNET_WINDOWS_TRIPLES)
            {
                set.insert(*row);
            }
        }
        set
    })
}

/// Is `class_name` one of the 62 throwable-family classes lane T retired whole
/// ([`RETIRED_SHADOW_LT_TRIPLES`])? The retirement re-tags their bridges
/// `SyntheticStub`; under `--compatible` such a stub still wins over the real
/// bytecode unless the class is real-protected
/// (`vm` `native_override::real_protected_stub_class`), which asks this
/// (`jr21-caught-throw-cost-split`). The table is sorted by triple, so one
/// class's rows are contiguous and a `partition_point` finds them.
pub fn is_lane_t_throwable_class(class_name: &str) -> bool {
    let i = RETIRED_SHADOW_LT_TRIPLES.partition_point(|&(c, _, _)| c < class_name);
    RETIRED_SHADOW_LT_TRIPLES
        .get(i)
        .is_some_and(|&(c, _, _)| c == class_name)
}

pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    triple_is_retired_shadow_given_prefix(
        || class_is_under_retired_prefix(class_name),
        class_name,
        method_name,
        descriptor,
    )
}

/// Is `class_name` under one of the [`RETIRED_SHADOW_PREFIXES`]? A pure
/// function of the class name and a constant, so
/// `NativeMethodRegistry::register` computes it once per class
/// (`ClassFacts`) rather than scanning ~200 prefixes per registration
/// (Phase 5 fast boot, `p5-natreg`).
pub(crate) fn class_is_under_retired_prefix(class_name: &str) -> bool {
    static INDEX: std::sync::OnceLock<PrefixIndex> = std::sync::OnceLock::new();
    let index =
        INDEX.get_or_init(|| PrefixIndex::new(RETIRED_SHADOW_PREFIXES.iter().map(|p| (*p, 0))));
    let mut hit = false;
    index.for_each_match(class_name, |_| hit = true);
    hit
}

/// [`triple_is_retired_shadow`] with the class-only prefix test supplied by
/// the caller (a memo of [`class_is_under_retired_prefix`] for this class).
/// The same conjunction evaluated in the same order; `prefix_hit` is called
/// exactly where the scan used to run.
pub(crate) fn triple_is_retired_shadow_given_prefix(
    prefix_hit: impl FnOnce() -> bool,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // The set-first switch ([`retired_shadow_set_first`]; performance, default
    // OFF; t1 wave 5, lane L71): ask the one row set before the ~200-entry
    // prefix scan. Both
    // are pure tests of the same triple and the answer is their conjunction,
    // so only the order changes; the scan (`starts_with` over every prefix)
    // was most of this function's cost on a miss, and the by-name dispatch
    // walk (`retired_row_masks_ancestor_bridge`) asks it per ancestor per
    // call (i71 page, t1 wave 5).
    if retired_shadow_set_first()
        && !retired_shadow_row_set(RETIRED_SHADOW_TABLES).contains(&(
            class_name,
            method_name,
            descriptor,
        ))
    {
        return false;
    }
    if !prefix_hit() {
        return false;
    }
    let key = (class_name, method_name, descriptor);
    // Driven off `RETIRED_SHADOW_TABLES` rather than one `||` arm per table.
    // `any` short-circuits exactly as the chain did and each table is still
    // binary-searched, so this is the same work in the same order of magnitude
    // — what changes is that a table which is not in the const is not
    // consulted, instead of being consulted while the const says otherwise.
    // Both lane 1 and lane 7 shipped a table missing from that const; the
    // arrangement below cannot reproduce it.
    //
    // The const's order differs from the chain's it replaces. That is
    // immaterial and `no_triple_is_claimed_by_two_tables` is why: no triple is
    // in two tables, so no input can reach a second table that would answer
    // differently, and which table answers first is unobservable.
    retired_shadow_row_set(RETIRED_SHADOW_TABLES).contains(&key)
        // The opt-in pure lever keeps the FFM rows native; see
        // [`pure_lever_keeps_ffm_row_native`] (a helper, so this body names no
        // table: `the_predicate_names_no_table_directly`).
        && !pure_lever_keeps_ffm_row_native(&key)
        // `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns named rows back off, so a
        // wave can be bisected in RUNS rather than one build per hypothesis.
        // Unset -- every shipping configuration -- this is `false` without
        // consulting anything; see [`crate::unretire`], whose
        // `the_default_is_inert_across_every_retired_row` asserts it against
        // every row of every table above.
        && !crate::unretire::is_excluded(class_name, method_name, descriptor)
}

/// `CRATONVM_RETIRED_SHADOW_SET_FIRST` (see [`triple_is_retired_shadow`]), read
/// once per process: a pure evaluation-order switch, it changes no answer, so
/// one value for every VM in the process is not compatibility state.
pub(in crate::retired_shadow) fn retired_shadow_set_first() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| true)
}

/// Lane 4 wave 5 -- three of the four families the wave-1 BUILDS backed out,
/// 27 rows over eight classes of `java/io/`, `java/nio/channels/` and
/// `java/nio/file/attribute/`.
///
/// No new prefix. `java/io/` and `java/nio/` were both admitted by wave 1 and
/// every class here is under one of them, so the table is the whole of the
/// decision.
///
/// # A backed-out family's recorded CAUSE is a hypothesis, and these were three
///
/// §9.2 of the lane page lost five families on wave 1's builds and wrote one
/// cause for all of them: "a fabricated carrier whose real fields this VM never
/// writes". Wave 4 took `java/nio/CharBuffer` and found that false for it.
/// This wave took the rest and found the record wrong in a THIRD way -- not
/// stale, and not mistaken about which vector, but mistaken about the
/// MECHANISM in one case and about the kind of thing it was in another:
///
/// * **The file-handle group (9)** -- `FileOutputStream`, `FileCleanable`,
///   `FileDescriptor` and the abstract-receiver `java/nio/channels/FileChannel`
///   -- went out as an UN-ATTRIBUTED group because `RJdkSecurity` reproduced
///   under no dial scope. §9.4 then found that vector's cause and fixed it: a
///   `java.io.File` carrier with an unwritten `prefixLength`. **Nobody re-tried
///   the group after the fix.** It is green here in every arm, and the whole of
///   its blocker was a defect in a different class that had been closed for two
///   days.
/// * **`java/nio/file/attribute/` (12)** was attributed to `RFileTimes` by a
///   dial sweep, and that attribution is CORRECT -- the vector fails on this
///   table alone, reproduced one row at a time. The recorded mechanism is not:
///   it is not a fabricated carrier, it is a READER. See below.
/// * **`java/io/ByteArrayInputStream` (9)** was attributed to `RSslLiveSession`,
///   also correctly, and is not a defect at all. Six rows retire; three are
///   carved out because they carry an OBSERVATION rather than an answer. See
///   `the_backed_out_wave_is_twenty_seven_rows_over_eight_classes`.
///
/// The fourth, `java/nio/file/spi/FileSystemProvider`, is not here and not
/// because it was forgotten: it was blocked on a concrete `Path`/
/// `FileSystemProvider`/`FileSystem` carrier until 2026-09-17, when the last
/// of those landed; see
/// `the_provider_family_retirement_is_four_of_nine_bucket_ab_rows` for the
/// wave that closed the blockers and what stayed excluded regardless.
///
/// # `filetime_read_millis` read a value whose UNIT it did not check
///
/// `FileTime` stores a PAIR. `FileTime.from(Instant)` compiles to
/// `new FileTime(0L, null, instant)` on 17, 21 and 25 alike -- `value` is a
/// literal `lconst_0` and the time is in `instant`. This VM's reader took
/// `value` for milliseconds, which held only because `filetime_alloc`, the
/// other end of the same VM, converts to millis before storing. Retire
/// `from`/`fromMillis` and real bytecode becomes the producer; the setter then
/// read **0** and `BasicFileAttributeView.setTimes` stamped every file
/// 1970-01-01. That is `RFileTimes`, and it is wave 4's finding in a new place:
/// **a native that owns both ends of a convention agrees with itself whatever
/// the convention is, and a retirement is how you ask for a second opinion.**
///
/// Fixed in the commit this table lands with, not carved out -- the rows are
/// correct and the reader was not.
///
/// # Four arms, and the wave MOVES the probe toward HotSpot
///
/// `L4W5Sweep.java`, 183 rows over all four families, diffed
/// against a HotSpot 25 oracle rather than against the control alone:
///
/// ```text
///   A  control (origin/dev f99c2e748), --real-jdk        8 rows differ
///   B  this wave, --real-jdk                             4 rows differ
///   C  this wave, compatible mode          0 differ FROM THE CONTROL
///   D  this wave + UNRETIRE, same binary   0 differ FROM THE CONTROL
/// ```
///
/// Arm D names `27 table row(s)` across its eight rules at arm time, which is
/// the receipt that B's movement is THIS table's and not a neighbour's under a
/// prefix that admits the whole of `java/io/`.
///
/// The four rows it moves are four silent wrong answers, all toward HotSpot:
///
/// ```text
///   FileTime.from(..., NANOSECONDS).toString()   .123Z          -> .123456789Z
///   FileTime.from(..., NANOSECONDS).to(NANOS)    ...123000000   -> ...123456789
///   PosixFilePermissions.fromString("rwx")       a Set          -> IAE
///   FileChannel.isOpen() after the stream closed true           -> false
/// ```
///
/// The last is §4's species exactly. The receiver is a REAL
/// `sun.nio.ch.FileChannelImpl` in all three arms -- a native registered on
/// `java/nio/channels/FileChannel` won the door for it and answered from this
/// VM's fd table instead of from the `closed` field real `close()` had just
/// set. Not a crash; a confident wrong answer to "is this channel open".
///
/// The 4 that remain are all carrier-IDENTITY rows this table does not touch
/// (`BasicFileAttributeView` and `FileAttribute` minted as their INTERFACES,
/// `Files.newInputStream` answering `FileInputStream` where HotSpot answers
/// `sun.nio.ch.ChannelInputStream`).
///
/// # The corpus is what found both defects, and no probe did
///
/// The probe tree was clean on the first build of this table. The corpus was
/// not: `RFileTimes` and `RSslLiveSession`, the two vectors §9.2 named, failed
/// on it and were attributed to a family in six runs and to a row in nine more
/// -- `CRATONVM_UNRETIRE_NATIVE_SHADOW`, no rebuild. §9.3's rule, paid for
/// again.
///
/// ```text
///   CRATONVM_ARGS=--real-jdk   control  136 passed, 0 failed
///                              wave     136 passed, 0 failed
///   SUITE=all                  wave     136 passed, 0 failed
///   SUITE=core                 wave      95 passed, 0 failed
/// ```
///
/// The two `--real-jdk` arms were compared VECTOR BY VECTOR and not by their
/// totals: 136 lines each, zero differing. `TIMEOUT=600`; no timing claim is
/// made from any arm.
///
/// # 27 of 27 through the funnel, 33 registrations, 0 survivors
///
/// Every row is invoked by `L4W5Sweep` under `--nojit
/// CRATONVM_DISABLE_INTRINSICS=1`, which is what makes the census's
/// `invocations_complete: true` mean what it says. One row --
/// `FileOutputStream.write([BII)V` -- reports the count as INCOMPLETE, so
/// precondition 4 rests on its `outcome` instead, and the control's report has
/// it `native-won`.
///
/// The strict registry drops 33 registrations over the 27 triples and the
/// wave's report carries no `native-won` among them; the control's report has
/// 22. The gap between 27 and 33 is six double-registered triples, and it is
/// the same six the ratchet counts.
///
/// # +33 in three arms, on one binary
///
/// ```text
///   arm             OFF             ON              delta
///   (default)   3979 / 13590    4012 / 13590        +33 / 0
///   management  4006 / 13958    4039 / 13958        +33 / 0
///   synthetic   3979 / 13625    4012 / 13625        +33 / 0
/// ```
///
/// The OFF column is the same binary with this wave's rows disarmed, and it
/// lands exactly on LANE 5's landed figures -- the third residual wave of that
/// lane re-froze these same three constants between this wave's first
/// measurement and its merge. That is what says the two compose rather than
/// overlap, and it is a stronger statement than the pre-merge sitting could
/// make: the same +33 in all three arms, measured twice, on trees 35
/// registrations apart.
///
/// Totals identical in both columns: every stub added is an existing `Bridge`
/// relabelled.
///
/// # Kind map: 32 rows amended, and the gate is no redder than dev
///
/// `bridge -> synthetic-stub` in the 25/linux baseline, written rather than
/// regenerated so dev's own 1 050 flips are not absorbed. **32, not 33**:
/// `PosixFilePermission.valueOf` is absent from the baseline altogether, and
/// this gate passes and REPORTS a new row rather than failing on it, so
/// freezing a row whose history this wave has not measured would be the wrong
/// repair. After the amendment the wave fires 1 050 -- exactly the control's
/// count -- and names no row of this table.
///
/// Measured on **linux/x86_64 against JDK 25**.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/ByteArrayInputStream", "<init>", "([B)V"),
    ("java/io/ByteArrayInputStream", "<init>", "([BII)V"),
    ("java/io/ByteArrayInputStream", "available", "()I"),
    ("java/io/ByteArrayInputStream", "read", "([B)I"),
    ("java/io/ByteArrayInputStream", "reset", "()V"),
    ("java/io/ByteArrayInputStream", "skip", "(J)J"),
    (
        "java/io/FileCleanable",
        "register",
        "(Ljava/io/FileDescriptor;)V",
    ),
    ("java/io/FileDescriptor", "<init>", "()V"),
    ("java/io/FileOutputStream", "close", "()V"),
    ("java/io/FileOutputStream", "flush", "()V"),
    ("java/io/FileOutputStream", "write", "(I)V"),
    ("java/io/FileOutputStream", "write", "([B)V"),
    ("java/io/FileOutputStream", "write", "([BII)V"),
    ("java/nio/channels/FileChannel", "close", "()V"),
    ("java/nio/channels/FileChannel", "isOpen", "()Z"),
    (
        "java/nio/file/attribute/FileTime",
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
    ),
    (
        "java/nio/file/attribute/FileTime",
        "from",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "java/nio/file/attribute/FileTime",
        "from",
        "(Ljava/time/Instant;)Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "java/nio/file/attribute/FileTime",
        "toInstant",
        "()Ljava/time/Instant;",
    ),
    ("java/nio/file/attribute/FileTime", "toMillis", "()J"),
    (
        "java/nio/file/attribute/FileTime",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/nio/file/attribute/PosixFilePermission",
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/attribute/PosixFilePermission;",
    ),
    (
        "java/nio/file/attribute/PosixFilePermission",
        "values",
        "()[Ljava/nio/file/attribute/PosixFilePermission;",
    ),
    (
        "java/nio/file/attribute/PosixFilePermissions",
        "asFileAttribute",
        "(Ljava/util/Set;)Ljava/nio/file/attribute/FileAttribute;",
    ),
    (
        "java/nio/file/attribute/PosixFilePermissions",
        "fromString",
        "(Ljava/lang/String;)Ljava/util/Set;",
    ),
    (
        "java/nio/file/attribute/PosixFilePermissions",
        "toString",
        "(Ljava/util/Set;)Ljava/lang/String;",
    ),
];

/// Lane 4 wave 6, the `java/io/PrintWriter` half: seven rows, one registrar,
/// and a verdict that was measured a month before it could be landed.
///
/// # The record this table is cashing
///
/// `register_printstream_fallback_natives` in
/// `native-builtins/src/logging_shims.rs` holds BOTH `java/io/PrintWriter` and
/// `java/io/PrintStream`, and on 2026-08-11 the two halves were measured
/// triple by triple with `CRATONVM_ENFORCE_NATIVE_SHADOW` scoped to one class
/// at a time against HotSpot 25. `PrintWriter` came out retirable in four
/// arms, `PrintStream` blocked. The write-up is
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// which carries the table entries below verbatim and a landing recipe -- and
/// says why it was never taken: all three frozen artefacts are keyed
/// `<jdk-feature>/<os>`, the measuring lane was on Windows, and the gates
/// there exit 2 rather than pass or fail. **The blocker was a platform, not a
/// question.** This wave is on linux/x86_64 with the JDK 25 image, so the
/// blocker is simply absent.
///
/// A recorded verdict is still a hypothesis -- lane 4 has now been wrong about
/// a backed-out family's cause three times in three different ways -- so every
/// row below was re-censused and re-measured here rather than copied.
///
/// # Why the receiver makes this half retirable and the other half not
///
/// `native_printwriter_init_outputstream` ends in an `invoke_special` chain
/// into the real `PrintWriter(OutputStream, boolean)`, which is registered
/// nowhere and so always runs as bytecode: `lock`, `out`, `charOut` and
/// `textOut` are populated by the JDK whichever path built the receiver. A
/// `PrintWriter` over a `Writer` never enters a native at all. Both
/// construction paths therefore leave a receiver real bytecode can use, which
/// is what the four arms show and what the `PrintStream` half does not have.
///
/// # `println(String)` also carries an observation, and that was checked
///
/// Four of these seven route to `native_println_*`, which call
/// `ctx.record_printed_line` -- the side table `vm/tests/differential.rs` and
/// twenty-two assertions in `vm/src/vm/tests.rs` read. That is the shape §9.19
/// of the lane page found on `ByteArrayInputStream`, where a correct row was
/// unretirable because its native also carried the VM's only observation of an
/// event. It does not bite here, and the reason is mechanical rather than
/// lucky: a retirement re-tags `Bridge` -> `SyntheticStub`, and a
/// `SyntheticStub` still DISPATCHES in compatible mode. Only strict mode drops
/// it. Every reader of `printed_lines` runs compatible.
///
/// Measured on **linux/x86_64 against JDK 25**.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/PrintWriter", "<init>", "(Ljava/io/OutputStream;)V"),
    ("java/io/PrintWriter", "println", "()V"),
    ("java/io/PrintWriter", "println", "(I)V"),
    ("java/io/PrintWriter", "println", "(Ljava/lang/Object;)V"),
    ("java/io/PrintWriter", "println", "(Ljava/lang/String;)V"),
    ("java/io/PrintWriter", "write", "(Ljava/lang/String;)V"),
    ("java/io/PrintWriter", "write", "(Ljava/lang/String;II)V"),
];

/// Lane 4 wave 6, the `java/io/PrintStream` half: twenty-nine rows, and the
/// blast radius §5 of the lane page requires this commit to name.
///
/// **`System.out` and `System.err` are how every lane reads its probes.** A
/// regression in this table does not read as a `PrintStream` bug; it reads as
/// all nine lanes' probes failing at once, and the first instinct will be to
/// blame the harness. Worse, the documented failure mode here is SILENT:
/// `writeln` catches its own `IOException` and sets `trouble = true`, so a
/// broken stream discards rather than throws, and a suite that asserts only on
/// exit codes reads green while the VM prints nothing. That is why the probe
/// for this wave reports through a `FileOutputStream` as well as through
/// `System.out`, and why this half is a table of its own that
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` can disarm at run time without a rebuild.
///
/// # The 2026-08-11 blocked list, re-read
///
/// W7-22 §3 blocked this class on five things, the first of which it called
/// "the whole job": `System.out`/`System.err` are fabricated rather than
/// constructed, with `FilterOutputStream.out`, `charOut`, `textOut`,
/// `closeLock` all null and `charset` an instance of the ABSTRACT
/// `java.nio.charset.Charset`. Re-measured by reflection on this wave's
/// control binary, that is no longer the state:
///
/// ```text
///   System.out field   2026-08-11        2026-09-12 (control)   HotSpot 25
///   out                null              FileOutputStream       BufferedOutputStream
///   charOut            null              OutputStreamWriter     OutputStreamWriter
///   textOut            null              BufferedWriter         BufferedWriter
///   charset            abstract Charset  sun.nio.cs.UTF_8       sun.nio.cs.UTF_8
///   closeLock          null              null                   Object
/// ```
///
/// `install_real_stream_fields` in `native-builtins/src/lang_system.rs` now
/// constructs a real `FileOutputStream`/`OutputStreamWriter`/`BufferedWriter`
/// chain and publishes all three at once, with a comment saying exactly why a
/// half-wired stream would be worse than the discard. Items 1 and 2 of the
/// blocked list are therefore spent. **This is the third time in this lane
/// that a family's recorded blocker turned out to have been fixed somewhere
/// else in the meantime** (§9.4, §9.17), and the first two were found by
/// re-reading rather than by re-measuring.
///
/// What the re-read did NOT clear:
///
/// * `closeLock` is still null on every receiver in compatible mode, including
///   one built by the real three-argument constructor. It is a `final` field
///   with an initialiser, so something is skipping instance initialisers;
///   strict mode gets it right, which is what says the native path is the one
///   dropping it.
/// * `native_printstream_init_outputstream` still does NOT chain to a real
///   constructor -- blocked-list item 3, and the doc comment above it claims
///   the chain in a paragraph that belongs to the PrintWriter function it sits
///   next to (a code-move artefact of the `lib.rs` -> `logging_shims.rs`
///   split). A receiver it builds has `charOut` and `textOut` null, which is
///   the `close()` NPE W7-22 measured. This table retires BOTH constructors
///   with the methods, so in strict mode nothing of this class is native and
///   the question does not arise; in compatible mode the native still runs and
///   the defect is untouched by this wave.
///
/// # What is deliberately NOT here
///
/// * `write(Ljava/lang/String;II)V` -- registered, but **no supported image
///   declares it** (`javap -p` on 17, 21 and 25: absent from all three).
///   Retiring it replaces a working native with a `NoSuchMethodError`. It is
///   the E/F row of the class and is held back by name.
/// * `charset()Ljava/nio/charset/Charset;` -- registered as `Intrinsic`, so
///   the `Bridge` -> `SyntheticStub` re-tag does not fire on it and a table
///   row would be inert. Left to whoever adjudicates that kind.
/// * `write(Ljava/lang/String;)V` IS here: `private` in all three images, so
///   only the class's own bytecode can reach it, which is precisely the caller
///   that exists in strict mode.
///
/// Measured on **linux/x86_64 against JDK 25**.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_PRINTSTREAM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V"),
    (
        "java/io/PrintStream",
        "<init>",
        "(Ljava/io/OutputStream;Z)V",
    ),
    (
        "java/io/PrintStream",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/io/PrintStream;",
    ),
    ("java/io/PrintStream", "close", "()V"),
    ("java/io/PrintStream", "flush", "()V"),
    (
        "java/io/PrintStream",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
    ),
    (
        "java/io/PrintStream",
        "format",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
    ),
    ("java/io/PrintStream", "print", "(C)V"),
    ("java/io/PrintStream", "print", "(D)V"),
    ("java/io/PrintStream", "print", "(F)V"),
    ("java/io/PrintStream", "print", "(I)V"),
    ("java/io/PrintStream", "print", "(J)V"),
    ("java/io/PrintStream", "print", "(Ljava/lang/Object;)V"),
    ("java/io/PrintStream", "print", "(Ljava/lang/String;)V"),
    ("java/io/PrintStream", "print", "(Z)V"),
    (
        "java/io/PrintStream",
        "printf",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
    ),
    (
        "java/io/PrintStream",
        "printf",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
    ),
    ("java/io/PrintStream", "println", "()V"),
    ("java/io/PrintStream", "println", "(C)V"),
    ("java/io/PrintStream", "println", "(D)V"),
    ("java/io/PrintStream", "println", "(F)V"),
    ("java/io/PrintStream", "println", "(I)V"),
    ("java/io/PrintStream", "println", "(J)V"),
    ("java/io/PrintStream", "println", "(Ljava/lang/Object;)V"),
    ("java/io/PrintStream", "println", "(Ljava/lang/String;)V"),
    ("java/io/PrintStream", "println", "(Z)V"),
    ("java/io/PrintStream", "write", "(I)V"),
    ("java/io/PrintStream", "write", "(Ljava/lang/String;)V"),
    ("java/io/PrintStream", "write", "([BII)V"),
];

/// Lane 4 wave 7, the `java/io/PrintStream` wave's `java/nio/file/Path` cousin:
/// nine rows (a tenth candidate the census named is measured unsafe -- see
/// below) that `636ef84ed` ("fix(nio): stamp java.nio.file.Path receivers as
/// the real platform class") made retirable for the first time.
///
/// # Why now and not earlier
///
/// `java.nio.file.Path` is an interface, so every native-built `Path` this VM
/// ever handed out had to be a `new` on SOME class. Until `636ef84ed`,
/// `p57_alloc_path`/`p57_alloc_path_raw` in
/// `native-builtins/src/phases_late/nio_file.rs` minted that receiver under the
/// literal interface name `java/nio/file/Path` -- illegal per JVMS §6.5 and,
/// more to the point for this table, a receiver whose runtime class was never
/// going to run real bytecode for anything, retired or not: an
/// `InstantiationError`-shaped object has no image method to fall through to.
/// `636ef84ed` mints through `concrete_receiver::alloc_concrete` against
/// `path_layout::impl_class()` instead (`sun/nio/fs/WindowsPath` on this
/// platform, `sun/nio/fs/UnixPath` on Unix) and mirrors every
/// `register_phase57_nio_file` registration made under the interface name onto
/// that concrete class, which is what first gives these rows a real
/// receiver to land on.
///
/// # Why these nine (and one excluded tenth) and not the other ~21 `java/nio/file/Path` rows
///
/// `javap -p java.nio.file.Path` on this machine's JDK 25 (Microsoft build
/// 25.0.3.9) draws the line directly: these ten (nine retired, one excluded
/// below) are the DEFAULT or STATIC methods the interface declares WITH a
/// body --
///
/// ```text
///   public static java.nio.file.Path of(java.lang.String, java.lang.String...);
///   public static java.nio.file.Path of(java.net.URI);
///   public default boolean startsWith(java.lang.String);
///   public default boolean endsWith(java.lang.String);
///   public default java.nio.file.Path resolve(java.lang.String);
///   public default java.nio.file.Path resolveSibling(java.nio.file.Path);
///   public default java.nio.file.Path resolveSibling(java.lang.String);
///   public default java.io.File toFile();
///   public default java.nio.file.WatchKey register(java.nio.file.WatchService, java.nio.file.WatchEvent$Kind<?>...) throws java.io.IOException;
///   public default java.util.Iterator<java.nio.file.Path> iterator();
/// ```
///
/// -- while the other ~21 (`compareTo`, `equals`, `getFileName`,
/// `getFileSystem`, `getName`, `getNameCount`, `getParent`, `getRoot`,
/// `hashCode`, `isAbsolute`, `normalize`, `register` with the 3-arg
/// `Modifier...` descriptor, `relativize`, `resolve(Path)`, `startsWith(Path)`,
/// `subpath`, `toAbsolutePath`, `toRealPath`, `toString`, `toUri`,
/// `endsWith(Path)`) are `public abstract` on `Path` itself -- no `Code`, so
/// retiring their `("java/nio/file/Path", ...)` registration would make the
/// fallback an unimplemented interface method instead of a working body. A
/// second `javap -p sun.nio.fs.WindowsPath` confirms the receiver side of the
/// same fact: `WindowsPath` overrides every one of those 21 (directly or via a
/// covariant-return bridge) and overrides NONE of these ten, so a `WindowsPath`
/// receiver's `resolve(String)`, `toFile()`, `iterator()`, etc. were always
/// going to resolve to `Path`'s own default body regardless of which concrete
/// class the receiver wears -- retiring the interface-keyed row is retiring
/// the ONLY declaration that exists for these ten, on any receiver. That this
/// column is necessary but not SUFFICIENT is exactly what the excluded tenth
/// row below found: having a body is not the same as the body working against
/// what this VM's allocator actually wrote.
///
/// The census this wave is built from ran `L4FilesSweep.java`
/// under `--real-jdk --explain-jdk-only --dump-native-registry <file>` against
/// this same real JDK 25, filtered to `class == "java/nio/file/Path"`,
/// `owns_slot == true`, `kind == "bridge"` and
/// `image_declaring_method.has_code == true`; it named exactly these ten
/// candidate rows, independently agreeing with the two `javap` reads above --
/// nine of the ten are in the table; see below for the tenth.
///
/// # Where the nine are registered
///
/// All nine are registered in `register_phase57_nio_file` itself and so fall
/// inside that function's `__path_rows_before` .. `mirror_class_registrations`
/// window -- see that function's own comment for how the mirror onto the
/// concrete class works.
///
/// # The tenth row, `register(WatchService, Kind...)`, is excluded on a MEASURED NPE
///
/// The census named a tenth candidate,
/// `("java/nio/file/Path", "register", "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;)Ljava/nio/file/WatchKey;")`
/// -- registered by `register_watch_service` in `native-io/src/lib.rs`, a
/// different function in a different crate, outside `register_phase57_nio_file`'s
/// mirror window, and `javap -p java.nio.file.Path` confirms it is a default
/// method with real `Code` exactly like the nine that are here. It looked safe
/// on the same reasoning as the other nine. **It is not**, and only a probe
/// run caught it: `L4PathRetireProbe.java`'s `dir.register(ws,
/// ENTRY_CREATE)`, with this row added to the table and the binary rebuilt,
/// throws `NullPointerException: Cannot invoke
/// "sun.nio.fs.WindowsWatchService$Poller.register(...)" because
/// "this.poller" is null` -- HotSpot returns a valid `WatchKey`. `Path`'s real
/// default `register` body reads `((WindowsWatchService)
/// watcher).poller`, and the `WatchService` this VM hands back from
/// `FileSystem.newWatchService()` is never wired up as a real
/// `WindowsWatchService` (its `poller` field is never populated) the same way
/// `path_layout` populates `Path`'s `string`/`bytes` slots but never `fs` --
/// a SIBLING gap in a different carrier, not the same bug, but the same
/// shape: a receiver stamped with the right CLASS whose declared fields the
/// allocator never filled in. Left out until `WatchService`'s own carrier gets
/// the same treatment `path_layout.rs` gave `Path`.
///
/// # `NativeKind` re-checked by hand, not only by the census
///
/// All nine sit under `r.set_category(NativeKind::Bridge)` with no
/// `set_category(...Intrinsic)` between it and their registration --
/// `register_phase57_nio_file` sets `Bridge` once at its top and does not
/// change category before line 7974's mirror call. Unlike `PrintStream.charset()`
/// (`RETIRED_SHADOW_L4_PRINTSTREAM_TRIPLES`'s own note), nothing here is
/// `Intrinsic`, so no row in this table is inert.
///
/// # Verification
///
/// `L4FilesSweep` (395 rows) against a real JDK 25 oracle, this table armed
/// under `--real-jdk`: same four pre-existing divergences as the unpatched
/// tree, no new ones, exit 0. `L4PathRetireProbe.java`, purpose-built for
/// these nine rows plus the excluded tenth, matches HotSpot exactly on all
/// nine and is what caught the tenth.
///
/// Measured on **Windows** against **JDK 25** (Microsoft build 25.0.3.9).
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_PATH_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/file/Path", "endsWith", "(Ljava/lang/String;)Z"),
    ("java/nio/file/Path", "iterator", "()Ljava/util/Iterator;"),
    (
        "java/nio/file/Path",
        "of",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Path",
        "of",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Path",
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Path",
        "resolveSibling",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Path",
        "resolveSibling",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
    ("java/nio/file/Path", "startsWith", "(Ljava/lang/String;)Z"),
    ("java/nio/file/Path", "toFile", "()Ljava/io/File;"),
];

/// Lane 4 wave 9, `java/nio/file/spi/FileSystemProvider`: four of the nine
/// rows `the_provider_family_is_retired_by_no_table` used to block, all at
/// once, retired together.
///
/// # The blockers that closed
///
/// `p57_alloc_provider` minted every provider this VM builds as the LITERAL
/// abstract `FileSystemProvider` class until 2026-09-16, so dispatch on it
/// (`H11-1`) never had a concrete class to retarget onto.
/// `FILE_SYSTEM_PROVIDER_IMPLS` records what the concrete mint cost:
/// `L4FilesSweep` went from 0 to 172 differing lines the first time it was
/// tried, because
/// `sun/nio/fs/UnixPath.toUnixPath`'s `instanceof UnixPath` fails against this
/// VM's interface-stamped `Path`. That is the companion fix `java/nio/file/Path`
/// needed first (2026-09-16, "stamp java.nio.file.Path receivers as the real
/// platform class"); this wave is the provider half that fix unblocked,
/// mirrored onto each `FILE_SYSTEM_PROVIDER_IMPLS` class the same way
/// `RETIRED_SHADOW_L4_PRINTSTREAM_TRIPLES` needs no per-platform duplicate
/// rows -- the mirror copies the `Bridge` -> `SyntheticStub` re-tag from the
/// one row keyed on the abstract name onto every concrete provider class
/// `register_phase57_nio_file`'s own `for target in FILE_SYSTEM_PROVIDER_IMPLS`
/// loop reaches.
///
/// A SECOND blocker closed the same week, found by the SAME attempt that hit
/// this table TWICE: `path_layout.rs` resolved `PathSlots::fs` but
/// `p57_write_path_fields` never wrote it, so a native-built `Path`'s `fs`
/// field was always null (2026-09-16/17 fix: `p57_write_path_fields` now
/// resolves `fs` from the Path's own encoded text). `WindowsFileSystemProvider`'s
/// real overrides of `createLink`/`createSymbolicLink`/`newFileChannel`/
/// `readSymbolicLink` all call `Path.getFileSystem()` internally, so the
/// FIRST attempt at this table made `L4FilesSweep` crash outright with an
/// uncaught `NullPointerException`.
///
/// A THIRD blocker closed immediately after: once `fs` was no longer null,
/// `WindowsPath.getAbsolutePath()`'s real body started reading
/// `getFileSystem().defaultDirectory()`/`.defaultRoot()` -- package-private,
/// non-native methods declared only on the CONCRETE `sun.nio.fs.WindowsFileSystem`,
/// never on the abstract interface `p57_alloc_default_filesystem` stamped
/// every default FileSystem with. `NoSuchMethodError`, the SECOND attempt at
/// this table. Fixed by minting the default FileSystem singleton concrete too
/// (`cratonvm_native_api::filesystem_layout`, 2026-09-17) -- Windows-verified
/// only, same caveat as every carrier fix in this family.
///
/// All three are the SAME species of defect, one carrier layer apart each
/// time: a receiver stamped with an ABSTRACT/INTERFACE name has no real
/// method table for real bytecode to dispatch into, and each layer was
/// invisible until the layer above it started handing out receivers real
/// bytecode could actually reach.
///
/// # The census named nine; javap on the concrete class only clears four
///
/// The nine are `owns_slot`, `Bridge`-kind, bucket-A/B rows whose
/// `FileSystemProvider`-declared body `javap -c` shows as non-abstract (real
/// `Code`, not `throw new UnsupportedOperationException()` as the class's
/// only statement) -- `createLink`, `createSymbolicLink`,
/// `installedProviders`, `newAsynchronousFileChannel`, `newFileChannel`,
/// `newFileSystem(Path,Map)`, `newInputStream`, `newOutputStream`,
/// `readSymbolicLink`. That column answers "does the ABSTRACT class have a
/// body", not "is the receiver's real answer correct" -- the receiver is now
/// a CONCRETE class, so what matters is whether
/// `sun.nio.fs.WindowsFileSystemProvider` (the one real image on this
/// machine; Windows/x86_64, JDK 25 --
/// `sun.nio.fs.Unix*`/`Linux*`/`Mac*`/`Bsd*`/`Solaris*FileSystemProvider` are
/// NOT installed here and were not checked) overrides the method with real
/// work, or inherits the abstract default and what that default actually
/// does once a receiver reaches it. `javap -p` on the concrete class clears
/// only four:
///
/// ```text
///   method                       overridden on WindowsFileSystemProvider?
///   createLink                   yes
///   createSymbolicLink           yes
///   newFileChannel                yes
///   readSymbolicLink             yes
///   newAsynchronousFileChannel   yes (but see "held out anyway" below)
///   newFileSystem(Path,Map)      NO -- inherits the abstract default
///   newInputStream               NO -- inherits the abstract default
///   newOutputStream               NO -- inherits the abstract default
///   installedProviders           n/a -- `static`, never overridden by anything
/// ```
///
/// # The four retired here, and why "overridden" is enough
///
/// `createLink`, `createSymbolicLink`, `newFileChannel` and `readSymbolicLink`
/// are all registered directly in `register_phase57_nio_file`'s own `fsp`
/// block (so the mirror loop above reaches them), `WindowsFileSystemProvider`
/// overrides every one with real `Code`, and each one's own native
/// (`p57_create_hard_link`, `p57_create_symbolic_link`, the fd_table-backed
/// channel open, `p57_read_symbolic_link`) does the same real OS-level
/// operation the real override does -- no synthetic-only behaviour that
/// falling through to bytecode would lose. `newFileChannel`'s own
/// registration comment says as much already: "Real JDK delegates to
/// WindowsFileSystemProvider.newFileChannel which overrides this abstract
/// method. We provide a synthetic FileChannel ... so the existing
/// j.n.c.FileChannel native methods can drive it" -- a workaround for the
/// exact carrier bugs this wave's own doc closed.
///
/// # The three excluded on `javap -c` evidence, not a guess
///
/// `newFileSystem(Path,Map)`, `newInputStream` and `newOutputStream` all
/// inherit `FileSystemProvider`'s own default body rather than an override,
/// and `javap -c java.nio.file.spi.FileSystemProvider` disassembles what that
/// default actually does -- decisive rather than javadoc-quoting:
///
/// * `newFileSystem(Path,Map)` -- three bytecodes, `new
///   UnsupportedOperationException; dup; invokespecial <init>; athrow`,
///   unconditionally. `register_phase57_nio_file`'s own registration
///   comment on this exact triple says so before this wave existed: "abstract
///   by default and the base method throws UnsupportedOperationException,"
///   and the native it registers instead mounts a real jar-backed
///   `FileSystem` so smallrye's `ClassPathUtils.processAsJarPath` (Quarkus
///   boot) does not die on it. Retiring swaps a working mount for the
///   unconditional throw its own neighbouring comment already named.
///   (Retired since by the 2026-09-23 archive-provider wave: smallrye asks
///   the `jar` provider of `installedProviders()`, which is now the real
///   `jdk.nio.zipfs` one, so the throw only ever reaches callers that offer
///   an archive to the `file` provider -- as on HotSpot.)
/// * `newOutputStream` -- inherits the default, which does
///   `this.newByteChannel(path, optsWithWRITE, NO_ATTRS)` (an `invokevirtual`
///   on the RECEIVER, not a call to this table's own `newFileChannel` row) and
///   wraps the result via `Channels.newOutputStream`. The registration this
///   table does NOT retire says why that matters: "The fix: register
///   `newOutputStream` directly so we never fall through to the JDK
///   default... hand back a real `java.io.FileOutputStream` instance with
///   the fd wired onto its `FileDescriptor` -- that way every downstream
///   `write`/`flush`/`close` native (already registered against
///   `java/io/FileOutputStream`) just works without any new shim type," fixing
///   a measured Felix `BundleException: Unable to cache bundle` on every
///   `installBundle`. `Channels.newOutputStream`'s anonymous wrapper is not a
///   `FileOutputStream`, so any downstream cast/`instanceof`/`getFD()` this
///   VM has wired specifically to that class would stop matching.
/// * `newInputStream` -- inherits the default, which calls the STATIC
///   `Files.newByteChannel(path, opts)` (resolved through the path's own
///   provider, not `this`) and wraps it the same way. This table's own
///   `fsp_new_input_stream` additionally serves VFS/jar entries with no file
///   descriptor at all (`vfs_read`, an in-memory `ByteArrayInputStream`),
///   runs `capability_gate::open_read_gated` so a denial surfaces as
///   `SecurityException` rather than an I/O condition, and constructs a real
///   `java.io.FileInputStream` with `closeLock` backfilled (missing it NPEs
///   `close()` under try-with-resources, per `fis_backfill_constructor_fields`
///   in `native-io`) -- none of which the generic `Channels`-wrapped
///   `SeekableByteChannel` path is known to reproduce.
/// * `installedProviders` -- RETIRED since, by the 2026-09-23 archive-provider
///   wave (`RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES`), which MEASURED the
///   premise below false: the real `ServiceLoader` discovery answers
///   `jdk.nio.zipfs` and `jdk.internal.jrtfs`. The original reasoning:
///   `static`, so there is no receiver to retarget
///   onto; the JVMS resolves an `invokestatic` to the literal declaring
///   class regardless of what `p57_alloc_provider` mints. Its real body does
///   REAL `ServiceLoader` discovery via `loadInstalledProviders()` (`javap
///   -c`: `getstatic installedProviders; ifnonnull; ...; invokestatic
///   FileSystems.getDefault; invokevirtual .provider; ...; invokestatic
///   loadInstalledProviders`) -- and this VM ships no `META-INF/services`
///   entries for a `jar:`/`jrt:` provider, so that discovery would answer a
///   one-element list. This table's own native answers three
///   (`file`/`jar`/`jrt`, `file` at element 0 to match
///   `FileSystems.getDefault().provider()` identity, per the class's own
///   javadoc contract HotSpot honours) specifically so
///   `FileSystems.getFileSystem(URI.create("jrt:/"))` resolves for the
///   in-process compiler reading the runtime image (HIB-CV-27). Retiring
///   trades a synthetic list this VM depends on for a real but incomplete
///   one.
///
/// # `newAsynchronousFileChannel` is held out on a structural gap, not a behavioural one
///
/// `WindowsFileSystemProvider` DOES override it with real `Code`, and the
/// native itself (`native-io/src/lib.rs`, `native_afc_provider_open`) reads as
/// exactly this wave's story one carrier early: its own comment says Craton
/// was "executing the abstract provider default, which throws
/// UnsupportedOperationException, instead of the Unix provider implementation"
/// before this row's synthetic AFC backend existed. What is different about
/// this one row is WHERE it is registered: every other row in this table is
/// inside `register_phase57_nio_file`'s own `fsp` block, so the `for target in
/// FILE_SYSTEM_PROVIDER_IMPLS { mirror_class_registrations(...) }` loop a few
/// lines above copies its `Bridge`/`SyntheticStub` kind onto every concrete
/// provider class. `native_afc_provider_open` is registered in
/// `native-io/src/lib.rs`'s `register_async_file_channel`, a completely
/// separate registrar (called from `register_phase92_io_completeness`, not
/// `register_phase57_nio_file`) -- confirmed by reading it, not assumed: that
/// function DOES run its own `mirror_class_registrations` loop (`for target
/// in AFC_IMPLS.iter().chain([AFC_ABSTRACT_IMPL])`), but the source class it
/// mirrors is `afc` (`java/nio/channels/AsynchronousFileChannel`), not `fsp`,
/// so a row registered under `java/nio/file/spi/FileSystemProvider` in that
/// same function is never selected by either mirror loop in the codebase. No
/// mirrored copy of this one triple exists on
/// `sun/nio/fs/WindowsFileSystemProvider` regardless of what this table says.
/// Whether a retired-but-unmirrored abstract-class row is even consulted for
/// a concrete receiver depends on registry-lookup semantics (an exact match
/// at the C25-retargeted concrete class, versus a hierarchy walk back up to
/// the abstract declaration) this file's own comments do not settle and this
/// wave did not verify against a build. Held out rather than asserted; the
/// fix, if wanted, is either mirroring this triple too or moving its
/// registration into the `fsp` block above.
///
/// Measured on **Windows/x86_64 against JDK 25** -- the only real-JDK image on
/// this machine. `sun.nio.fs.Unix/Linux/Mac/Bsd/SolarisFileSystemProvider`
/// were not directly `javap`'d; `FILE_SYSTEM_PROVIDER_IMPLS` picks whichever
/// one the running image actually has, and this table retires the row keyed
/// on the abstract class name, which the mirror then copies onto whichever
/// concrete class wins on THAT platform -- so an unverified platform inherits
/// this wave's verdict rather than getting its own.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FILESYSTEMPROVIDER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/file/spi/FileSystemProvider",
        "createLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "newFileChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
];

/// Lane 4 wave 8, 2026-09-18. `java/io/` was admitted by wave 1; no new
/// prefix.
///
/// # A whole-class retirement, not a per-method one
///
/// `L4CensusTail.java` was written (an earlier session, never
/// tabled) to reach exactly two families this lane had never measured:
/// `java/io/LineNumberInputStream` (8 registrations) and
/// `java/io/StringBufferInputStream` (5). Both are minted by
/// `try_alloc_concurrent_synthetic` (`native-builtins/src/deprecated_util.rs`)
/// and their accessors read/write FOUR (LNIS) or THREE (SBIS) fixed slot
/// indices -- `native_lnis_init`'s own doc comment: `field 0 = wrapped
/// InputStream, 1 = line number, 2 = saved line number, 3 = pushback byte`
/// -- a private convention that does not correspond to either class's real
/// declared layout (`javap -p`: LNIS is `pushBack, lineNumber,
/// markLineNumber, markPushBack`, four fields with no slot for the inherited
/// `FilterInputStream.in`; SBIS is `buffer, pos, count`, three). This is the
/// lane's five-times-repeated carrier shape (`File`'s `prefixLength`,
/// `AbstractLayout.name`, `AbstractGroupLayout.elements`,
/// `CharBuffer.toString(int,int)`'s convention, `FileTime`'s unit) IF any row
/// is retired while a sibling stays native: real bytecode reading a field
/// this VM never wrote at that offset, or a native reading a field real
/// bytecode never populated the way the native's own convention assumes.
///
/// Both tables here are their WHOLE native surface, `<init>` included --
/// mirroring wave 6's `PrintStream`/`PrintWriter` ("retiring the
/// constructors hands construction back to the JDK and all four fields
/// arrive"). With `<init>` retired too, no native ever mints or touches
/// either object again: the real constructor's own `putfield`s populate the
/// real declared fields at the real offsets the JVM resolves for them, and
/// every accessor -- also retired -- reads them back the same way. There is
/// no partially-native state for the carrier question to apply to.
///
/// # Measured
///
/// `L4CensusTail`, 126 rows, oracle JDK 25 on windows/x86_64. The LNIS/SBIS
/// section is rows 1-38 of 129 in the probe's own numbering. Retired,
/// `--real-jdk`: **zero rows differ from the oracle** -- confirmed by running
/// the exact same rows the pre-fix binary had already produced correctly in
/// COMPATIBLE mode (this table's own precondition: unblocked only once
/// `native_vm_save_properties` let `System.initPhase1()`'s real bytecode
/// complete under `--real-jdk` at all, see the commit this table landed
/// with). `compatible` mode: unaffected, `SyntheticStub` is
/// `allowed_in(Compatible)`, so the native still wins there.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_DEPRECATED_STREAMS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/LineNumberInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
    ),
    ("java/io/LineNumberInputStream", "available", "()I"),
    ("java/io/LineNumberInputStream", "getLineNumber", "()I"),
    ("java/io/LineNumberInputStream", "mark", "(I)V"),
    ("java/io/LineNumberInputStream", "read", "()I"),
    ("java/io/LineNumberInputStream", "read", "([BII)I"),
    ("java/io/LineNumberInputStream", "reset", "()V"),
    ("java/io/LineNumberInputStream", "setLineNumber", "(I)V"),
    (
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    ("java/io/StringBufferInputStream", "available", "()I"),
    ("java/io/StringBufferInputStream", "read", "()I"),
    ("java/io/StringBufferInputStream", "read", "([BII)I"),
    ("java/io/StringBufferInputStream", "reset", "()V"),
];

/// Lane 4 wave 8, 2026-09-18, `java/nio/file/SimpleFileVisitor`'s four erased
/// `FileVisitor` bridge rows. `java/nio/` was admitted by wave 1.
///
/// `SimpleFileVisitor` (`register_p66_file_visitor`,
/// `native-builtins/src/phases_late/nio_file.rs`) declares NO instance
/// fields (`javap -p`) -- none of this wave's carrier question applies here;
/// the class is pure dispatch. The native bodies replicate documented
/// javadoc behaviour (`preVisitDirectory`/`visitFile` answer `CONTINUE`,
/// `visitFileFailed` rethrows, `postVisitDirectory` rethrows iff its
/// exception argument is non-null) that the real class's own bytecode
/// already implements identically -- `register_p66_file_visitor`'s own
/// comment records the one defect this surface had (an argument-index bug
/// that threw the wrong value; fixed before this table).
///
/// Measured with the same `L4CensusTail` run as the table above (its rows
/// 88-95: `bridge`/`direct` `visitFile`/`preVisitDirectory`/
/// `postVisitDirectory`, both the null- and exception-carrying calls,
/// reached through a reference typed as the raw `FileVisitor` interface AND
/// through `SimpleFileVisitor<Path>` directly). Zero rows differ from the
/// JDK 25 oracle in either shape.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FILEVISITOR_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/file/SimpleFileVisitor",
        "postVisitDirectory",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
    ),
    (
        "java/nio/file/SimpleFileVisitor",
        "preVisitDirectory",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
    ),
    (
        "java/nio/file/SimpleFileVisitor",
        "visitFile",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
    ),
    (
        "java/nio/file/SimpleFileVisitor",
        "visitFileFailed",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
    ),
];

/// Lane 4 wave 8, 2026-09-18, the `ByteBufferAsCharBuffer{B,L,RB,RL}` half of
/// wave 4's `toString(int,int)` fix that wave 4 itself never tabled.
/// `java/nio/` was admitted by wave 1.
///
/// `cb_to_string_range` (`native-builtins/src/phases_late/charset_buffers.rs`)
/// is registered on `java/nio/CharBuffer` itself AND, separately, on seven
/// concrete subclasses -- the comment at its own registration loop: "the
/// `ByteBufferAs*CharBuffer` family and `HeapCharBuffer` are what
/// `ByteBuffer.asCharBuffer()` returns in real-JDK mode... registering
/// directly on each ensures dispatch hits us regardless of how vtable
/// resolution handles abstract-in-base + native-on-base." Wave 4
/// (`RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES`) retired the base-class row only;
/// the four `ByteBufferAsCharBuffer*` subclass rows -- the ones a real
/// `ByteBuffer.asCharBuffer()` actually returns -- were never tabled, so the
/// SAME defect §9.15 fixed in the shared function (`toString()` taking its
/// window from an absolute buffer position rather than the position-relative
/// one every real body except `StringCharBuffer`'s uses) was fixed but never
/// asked to prove itself on the classes that matter. `javap -p`: all four
/// declare `public java.lang.String toString(int, int)` with real `Code`.
///
/// `HeapCharBuffer`/`HeapCharBufferR`/`StringCharBuffer` are the other three
/// classes under the same registration loop, added here rather than left for
/// later: `L4CensusTail`'s own `HeapCharBuffer toString`/`HeapCharBufferR
/// toString`/`StringCharBuffer toString`/`StringCharBuffer sliced` rows
/// (measured alongside the four above, same run, same zero-diff result)
/// already exercise `toString()`'s delegation into exactly these three
/// classes' `toString(int,int)` -- `CharBuffer.allocate(6)` is a
/// `HeapCharBuffer`, `.asReadOnlyBuffer()` of it a `HeapCharBufferR`,
/// `CharBuffer.wrap(String)` a `StringCharBuffer`. `javap -p`: all three
/// declare `toString(int, int)` with real `Code` (`HeapCharBufferR`
/// overrides it itself, not inherited from `HeapCharBuffer`).
///
/// Measured with the same `L4CensusTail` run as the four rows above: its
/// `view BE/LE toString`, `... RO toString` and `... sliced toString` rows
/// (11-15 depending on endianness) construct exactly a
/// `ByteBufferAsCharBufferB` or `ByteBufferAsCharBufferL` via
/// `ByteBuffer.allocate(8).order(order).asCharBuffer()` and reach
/// `toString(int,int)` through `toString()`'s one-line delegation
/// (`toString(position(), limit())`, §9.15); `HeapCharBuffer toString`,
/// `HeapCharBufferR toString`, `StringCharBuffer toString` and
/// `StringCharBuffer sliced` do the same for the three rows below. Zero rows
/// differ from the JDK 25 oracle, for any of the seven. `RB`/`RL` (the
/// read-only `ByteBufferAsCharBuffer` views) are reached by the same rows'
/// `.asReadOnlyBuffer()` half and are not independently exercised by this
/// probe beyond that -- included here on the class's own `javap -p` evidence
/// (identical declared method, same shared native, same real `Code`) rather
/// than a separate probe row, and named here rather than left silent.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_CHARBUFFER_VIEWS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/ByteBufferAsCharBufferB",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferL",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRB",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRL",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/HeapCharBuffer",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/HeapCharBufferR",
        "toString",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/nio/StringCharBuffer",
        "toString",
        "(II)Ljava/lang/String;",
    ),
];

/// Lane 4 wave 9, 2026-09-18, one row: `java/nio/file/Paths.get(String,
/// String...)`. `java/nio/` was admitted by wave 1.
///
/// Found by the funnel step run for the first time in this lane's history
/// against a WORKING `--real-jdk` (86 invocations across
/// `L4FilesSweep`/`L4Reach`/`TailFamilySweep`). The native
/// (`native-builtins/src/phases_late/nio_file.rs`) is pure string-joining
/// into `p57_alloc_path_checked` -- no filesystem call, no
/// `sun/nio/fs/WindowsNativeDispatcher` reach, unlike the wave this same
/// funnel run also surfaced and did NOT land (see the "ATTEMPTED, not yet
/// accepted" note this section replaces in git history: `Files`'s 32 rows
/// cascade into `WindowsNativeDispatcher.GetFileAttributesEx0`, a Win32
/// native this VM has never implemented -- measured via `L4FilesSweep`
/// crashing on its first call, `UnsatisfiedLinkError`, not a soft diff).
/// Real bytecode's `Paths.get` delegates to `FileSystems.getDefault()
/// .getPath(...)`, which reaches the ALREADY-NATIVE, already-invoked
/// (47 times in the same probe run) `WindowsFileSystem.getPath` -- this row
/// does not retire anything downstream of it, only the thin static wrapper.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_PATHS_TRIPLES: &[(&str, &str, &str)] = &[(
    "java/nio/file/Paths",
    "get",
    "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
)];

/// Lane 4 wave 10/13/14/15, 21 rows: `sun/nio/fs/WindowsPath`'s
/// `Path`-interface methods, named by wave 9's funnel step
/// (`docs/feature-designs/native-dispatch-architecture.md` §9 Wave 9).
///
/// Started as all 19 candidate rows. `getFileName`/`getParent` were
/// excluded through wave 14 and retired by wave 15:
///
/// - `getFileName`/`getParent` both scan `this.path` directly for a
///   literal `'\\'` via `String.lastIndexOf(92)`, completely independent
///   of the `offsets` cache (confirmed via `javap`: exactly 2
///   `lastIndexOf`/`indexOf` call sites in the whole class, one in each).
///   This VM used to store `path` with `/`, so that scan failed for any
///   multi-component path and no auxiliary field could fix it. Wave 15
///   changed what the slot HOLDS to the JDK's own `\` form and the string
///   the real `WindowsPathParser` normalises to
///   (`cratonvm_native_api::path_layout::stored_form`), with every native
///   READER converting back to the VM-internal `/` spelling
///   (`canonical_form`) -- which also repaired the mixed `C:/U/x\q` the
///   bytecode `resolve` built next to a parsed `C:/U/x/q`. The 2 rows
///   (3 descriptors: `getParent` has a covariant `WindowsPath` bridge) then
///   matched HotSpot on a 27-shape probe (`L4PathShapes`).
///
/// `toRealPath` (wave 14, both the real
/// `([Ljava/nio/file/LinkOption;)Lsun/nio/fs/WindowsPath;` and the bridge
/// `([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;`) was blocked the
/// same way through wave 13 -- `WindowsLinkSupport.getRealPath` →
/// `WindowsNativeDispatcher.GetFullPathName` → `GetFullPathName0`, a
/// Win32 native this VM had never implemented -- and broke `L4TrustPath`
/// (`UnsatisfiedLinkError`, not a silent wrong answer -- loading a
/// `KeyStore` reaches `Security$SecPropLoader.loadFromPath` →
/// `WindowsPath.toRealPath`) when tried anyway. Wave 14 implemented
/// `GetFullPathName0` (same FFI pattern as wave 10's
/// `GetFileAttributesEx0`/`CreateFile0`) and retried: `javap -c` on
/// `WindowsLinkSupport.getRealPath`/`resolveAllLinks` shows the
/// non-symlink path needs only `GetFullPathName0` plus
/// `WindowsFileAttributes.get` (→ `GetFileAttributesEx0`, wave 10) --
/// both now implemented. The symlink-follow branch
/// (`WindowsLinkSupport.readLinkImpl`) additionally needs
/// `DeviceIoControlGetReparsePoint`, still unimplemented; a 10-row
/// oracle-diff probe (`L4ToRealPath`, no symlinks: absolute/dotted/
/// dotdot/uppercase-cased/directory/missing-file/`NOFOLLOW_LINKS`
/// shapes) matched HotSpot exactly in both `--real-jdk` and default
/// mode, so this table retires `toRealPath` for the covered,
/// non-symlink case -- a real symlink would still throw
/// `UnsatisfiedLinkError` naming the missing native, same as any other
/// row in this lane blocked on a missing `WindowsNativeDispatcher`
/// primitive.
///
/// The other 18 (`compareTo`/`endsWith`/`equals`/`getFileSystem`/
/// `getName`/`getNameCount`/`getRoot`/`hashCode`/`isAbsolute`/
/// `normalize`/`relativize`/`resolve`/`startsWith`/`subpath`/
/// `toAbsolutePath`/`toRealPath`(x2)/`toUri`) are pure path-string
/// manipulation with no direct `\\`-scan -- `javap -c` confirms no other
/// `lastIndexOf`/`indexOf` call site in the class. `toAbsolutePath()` is
/// pure bytecode (string concatenation against
/// `WindowsFileSystem.defaultDirectory()`).
///
/// Landed after three real bugs, found and fixed across as many attempts
/// (§9.36 and its two follow-ups): (1) `getFileName()` [before it was
/// excluded] NPE'd on a `null this.root` -- fixed via a safe `root`/`type`
/// fallback in `p57_write_path_fields`. (2) `getName(0)` on a
/// multi-component path returned the whole string -- real
/// `WindowsPath.initOffsets()` (a null-guarded lazy cache) scans for
/// `'\\'`, which never appeared in the `/`-stored `path` of the time; fixed
/// by pre-populating the real `offsets` field (wave 15 later moved the
/// storage itself to `\`). (3) `"a//b"` (a legitimate path with a redundant
/// separator) came back `nameCount=3` instead of `2` -- real `path` is
/// always pre-normalized (single separators only) by construction, so a
/// literal translation of `initOffsets()`'s algorithm never needed to
/// handle a double separator; this VM's OWN allocator does not collapse
/// one, so the offsets computation was fixed to skip empty spans between
/// consecutive separators. All three fixes are general and landed
/// independent of this table.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/nio/fs/WindowsPath",
        "compareTo",
        "(Ljava/nio/file/Path;)I",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "endsWith",
        "(Ljava/nio/file/Path;)Z",
    ),
    ("sun/nio/fs/WindowsPath", "equals", "(Ljava/lang/Object;)Z"),
    (
        "sun/nio/fs/WindowsPath",
        "getFileName",
        "()Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "getFileSystem",
        "()Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "getName",
        "(I)Ljava/nio/file/Path;",
    ),
    ("sun/nio/fs/WindowsPath", "getNameCount", "()I"),
    (
        "sun/nio/fs/WindowsPath",
        "getParent",
        "()Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "getParent",
        "()Lsun/nio/fs/WindowsPath;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "getRoot",
        "()Ljava/nio/file/Path;",
    ),
    ("sun/nio/fs/WindowsPath", "hashCode", "()I"),
    ("sun/nio/fs/WindowsPath", "isAbsolute", "()Z"),
    (
        "sun/nio/fs/WindowsPath",
        "normalize",
        "()Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "relativize",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "startsWith",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "subpath",
        "(II)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsPath",
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Lsun/nio/fs/WindowsPath;",
    ),
    ("sun/nio/fs/WindowsPath", "toUri", "()Ljava/net/URI;"),
];

/// Lane 4 wave 16, 23 rows: the Windows `sun/nio/fs` filesystem classes
/// -- `WindowsFileSystem` (9 of 10), `WindowsFileSystemProvider` (9 of 10),
/// `WindowsDirectoryStream` (2), `WindowsFileStore` (2) and
/// `WindowsPath.toString`.
///
/// Wave 9's funnel named these (`WindowsFileSystem` 10, `WindowsFileSystemProvider`
/// 11) and §9.34 named what stood between them and retirement: the
/// `WindowsNativeDispatcher` natives this VM's `sun/nio/fs/` support never
/// covered. Waves 10, 14, 15 and 16 implemented the ones these rows reach
/// (`CreateFile0`, `GetFileAttributesEx0`, `FindFirstFile0/1`,
/// `FindNextFile0`, `GetFileInformationByHandle0`, the volume and drive
/// queries, `SetFileTime0`, `SetEndOfFile`, `DeviceIoControl*`,
/// `GetFinalPathNameByHandle`, ...).
///
/// NOT in the table, and why (each is a named blocker, not a deferral):
///
/// - `WindowsFileSystem.newWatchService` and everything under it: the real
///   `WindowsWatchService` drives a `ReadDirectoryChangesW` overlapped-I/O
///   loop over a completion port (`CreateIoCompletionPort`,
///   `GetQueuedCompletionStatus0`, `PostQueuedCompletionStatus`,
///   `CancelIo`, `GetOverlappedResult`, `CreateEvent`) and a poller thread.
///   That is an async-I/O runtime, not a filesystem primitive.
/// - `WindowsFileSystemProvider.checkAccess`: for `WRITE`/`EXECUTE` the real
///   body asks the access-check machinery (`WindowsSecurity`: thread/process
///   token duplication, `GetFileSecurity0`, `AccessCheck`), the security
///   family this VM has none of.
/// - `WindowsFileAttributes.fileKey`: held on purpose since the 2026-08-19
///   census (`the_held_windows_attribute_triple_is_not_retired`). The real
///   body is `return null;`; the native answers a `(volume, index)` key that
///   lets `FileTreeWalker.wouldLoop` see a symlink cycle. HotSpot-identical
///   and strictly worse behaviour, so it stays a native by decision.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_WINFS_TRIPLES: &[(&str, &str, &str)] = &[
    // `WindowsDirectoryStream.close`/`.iterator` held OUT (2026-09-22): see
    // the note after this table's closing bracket
    // (`newByteChannel`/`newDirectoryStream`/`readAttributes`'s doc) — the
    // fourth and fifth blockers found bisecting the SAME `javac` module-path
    // scan. `newDirectoryStream` mints its `*DirectoryStream` via
    // `alloc_concrete` directly rather than running the real constructor, so
    // `closed`/`closeLock` are never set and real `iterator()`/`close()`
    // NPE/throw the instant they run against it.
    ("sun/nio/fs/WindowsFileStore", "getBlockSize", "()J"),
    ("sun/nio/fs/WindowsFileStore", "getTotalSpace", "()J"),
    ("sun/nio/fs/WindowsFileSystem", "close", "()V"),
    (
        "sun/nio/fs/WindowsFileSystem",
        "getPath",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/WindowsFileSystem",
        "getPathMatcher",
        "(Ljava/lang/String;)Ljava/nio/file/PathMatcher;",
    ),
    (
        "sun/nio/fs/WindowsFileSystem",
        "getRootDirectories",
        "()Ljava/lang/Iterable;",
    ),
    (
        "sun/nio/fs/WindowsFileSystem",
        "getSeparator",
        "()Ljava/lang/String;",
    ),
    ("sun/nio/fs/WindowsFileSystem", "isOpen", "()Z"),
    ("sun/nio/fs/WindowsFileSystem", "isReadOnly", "()Z"),
    (
        "sun/nio/fs/WindowsFileSystem",
        "provider",
        "()Ljava/nio/file/spi/FileSystemProvider;",
    ),
    (
        "sun/nio/fs/WindowsFileSystem",
        "supportedFileAttributeViews",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "getFileAttributeView",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "getFileStore",
        "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "getPath",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
    ),
    // getScheme() held OUT (2026-09-22): see `newFileSystem`'s doc in the
    // wave-17 `FILES2` table below -- the two rows are the read half and the
    // dispatch half of the SAME carrier-sharing defect (every logical
    // provider is this one concrete class; a caller iterating
    // `installedProviders()` by scheme must see each one's OWN scheme, not
    // real bytecode's single hard-coded "file" answer).
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "isHidden",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
    ),
    ("sun/nio/fs/WindowsPath", "toString", "()Ljava/lang/String;"),
];
// `newByteChannel`, `newDirectoryStream` and `readAttributes(Class)` held OUT
// (2026-09-22, same wave, same defect as `getScheme`/`newFileSystem` above):
// each has its own jarfs/jrtfs-aware body on `fsp` (`newByteChannel` reads a
// mounted archive's `vfs_read` snapshot; `newDirectoryStream` lists via
// `jarfs_list_dir`/`jrtfs_list_dir_classified`; `readAttributes(Class)` is
// exactly what `Files.isDirectory` reaches through `readAttributesIfExists`
// while walking a listed directory). MEASURED bisecting `TestCompilerTests`
// with `CRATONVM_UNRETIRE_NATIVE_SHADOW`, one row at a time: each was its own
// blocker, in this order, before `javac`'s in-process module-path scan
// (`Locations$SystemModulesLocationHandler.initSystemModules`) got past the
// runtime-image listing. `WindowsDirectoryStream.close`/`.iterator` (wave 16's
// OWN table, retired above `RETIRED_SHADOW_L4_WINFS_TRIPLES`) are the fourth
// and fifth: my `newDirectoryStream` mints the concrete `*DirectoryStream`
// directly rather than running its real `<init>`, so `closed`/`closeLock`
// are never set and real `iterator()`/`close()` NPE/throw
// `IllegalStateException: Directory stream is closed` the instant they run
// against it.

/// Lane 4 wave 17: the eleven Java wrappers of `java.io.WinNTFileSystem`.
///
/// The JDK 25 class declares each operation as a Java method
/// (`list(File)`, `getLength(File)`, ...) that calls the JNI entry point of
/// the same name suffixed `0`. This VM shadowed the wrapper AND registered the
/// `*0` native (kind `Bridge`, `nio_file.rs` `fs_reg` / the `WinNTFileSystem`
/// loop); the wrapper is what the funnel counted. Retiring it hands the
/// wrapper to bytecode, which then calls the `*0` native that already exists.
/// The table names `WinNTFileSystem` only, so `UnixFileSystem` (Linux image)
/// is untouched and stays a separate measurement on a Linux host.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_WINNTFS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/WinNTFileSystem",
        "checkAccess",
        "(Ljava/io/File;I)Z",
    ),
    (
        "java/io/WinNTFileSystem",
        "createDirectory",
        "(Ljava/io/File;)Z",
    ),
    (
        "java/io/WinNTFileSystem",
        "createFileExclusively",
        "(Ljava/lang/String;)Z",
    ),
    (
        "java/io/WinNTFileSystem",
        "getBooleanAttributes",
        "(Ljava/io/File;)I",
    ),
    (
        "java/io/WinNTFileSystem",
        "getLastModifiedTime",
        "(Ljava/io/File;)J",
    ),
    ("java/io/WinNTFileSystem", "getLength", "(Ljava/io/File;)J"),
    ("java/io/WinNTFileSystem", "getSpace", "(Ljava/io/File;I)J"),
    (
        "java/io/WinNTFileSystem",
        "list",
        "(Ljava/io/File;)[Ljava/lang/String;",
    ),
    (
        "java/io/WinNTFileSystem",
        "setLastModifiedTime",
        "(Ljava/io/File;J)Z",
    ),
    (
        "java/io/WinNTFileSystem",
        "setPermission",
        "(Ljava/io/File;IZZ)Z",
    ),
    (
        "java/io/WinNTFileSystem",
        "setReadOnly",
        "(Ljava/io/File;)Z",
    ),
];

/// Lane 4 wave 19: the rows the wave-18 ledger (§9.37) could not dispose of --
/// the ones with no named blocker, measured on linux/x86_64 and retired here.
///
/// # What was left, and why these are the ones
///
/// Wave 18 walked the whole prefix set with `l4ledger.py` and
/// joined `l4blockers.json` onto it. 618 open bucket-A/B rows
/// matched a named blocker; 115 matched none, and "no probe in this tree
/// invokes it" is explicitly NOT one (§9.1). This table is what was left after
/// the 52 of those 115 that a blocker DOES cover were given one:
///
/// * the five typed buffers' `array`/`capacity`/`flip`/`hasRemaining`/`limit`/
///   `position`/`remaining` (35). `java/nio/Buffer`'s own `position`, `limit`
///   and `capacity` were retired by wave 17 and measured then; these are the
///   same three plus their siblings, on the five subclasses that inherit the
///   bodies.
/// * `ByteBufferAsCharBuffer{B,L}`'s five accessors (10), which `L4CharBufferSweep`
///   and `L4TypedBufferSweep` read back over both byte orders.
/// * `DirectByteBuffer.{getDouble,getFloat,putDouble,putFloat}` (4) -- the four
///   widths wave 17's `DirectByteBuffer` group left out.
/// * `ByteBuffer.toString`, `ByteOrder.equals`, `CharBuffer.charAt`,
///   `StringCharBuffer.toString` (4).
/// * `java.nio.file.Files`' remaining seven, which the wave-18 Unix dispatcher
///   natives (`stat0`/`lstat0`/`unlink0`/...) made reachable as bytecode for
///   the first time on this platform.
/// * `ObjectInputStream.resolveProxyClass` and
///   `ObjectStreamClass$RecordSupport.deserializationCtr` (2). These two had
///   **no caller anywhere in the probe tree**; `L4Serial.java` was
///   written for them (a record round-trip and a serialized dynamic proxy,
///   both read back) and reaches them 2 and 1 times.
///
/// # Not in the table
///
/// `session()`/`checkSession()` on all ten buffer classes stay out: the JOTP
/// wave refuses them on `dev` and the measurement is netty's, not this repo's.
/// So do `Bits$1`'s four pool-MXBean accessors and `Buffer$2`'s
/// `reserveMemory`/`unreserveMemory` -- those carry the VM's own direct-memory
/// accounting, which is the `Bits.reserveMemory` finding of wave 17 one class
/// along.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_W19_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/ObjectInputStream",
        "resolveProxyClass",
        "([Ljava/lang/String;)Ljava/lang/Class;",
    ),
    (
        "java/io/ObjectStreamClass$RecordSupport",
        "deserializationCtr",
        "(Ljava/io/ObjectStreamClass;)Ljava/lang/invoke/MethodHandle;",
    ),
    (
        "java/nio/ByteBuffer",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/nio/ByteBufferAsCharBufferB", "charAt", "(I)C"),
    ("java/nio/ByteBufferAsCharBufferB", "get", "()C"),
    ("java/nio/ByteBufferAsCharBufferB", "get", "(I)C"),
    ("java/nio/ByteBufferAsCharBufferB", "hasArray", "()Z"),
    (
        "java/nio/ByteBufferAsCharBufferB",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/nio/ByteBufferAsCharBufferL", "charAt", "(I)C"),
    ("java/nio/ByteBufferAsCharBufferL", "get", "()C"),
    ("java/nio/ByteBufferAsCharBufferL", "get", "(I)C"),
    ("java/nio/ByteBufferAsCharBufferL", "hasArray", "()Z"),
    (
        "java/nio/ByteBufferAsCharBufferL",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/nio/ByteOrder",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/nio/CharBuffer", "charAt", "(I)C"),
    ("java/nio/DirectByteBuffer", "getDouble", "(I)D"),
    ("java/nio/DirectByteBuffer", "getFloat", "(I)F"),
    (
        "java/nio/DirectByteBuffer",
        "putDouble",
        "(ID)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "putFloat",
        "(IF)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/DoubleBuffer", "array", "()[D"),
    ("java/nio/DoubleBuffer", "capacity", "()I"),
    (
        "java/nio/DoubleBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
    ),
    ("java/nio/DoubleBuffer", "hasRemaining", "()Z"),
    ("java/nio/DoubleBuffer", "limit", "()I"),
    ("java/nio/DoubleBuffer", "position", "()I"),
    ("java/nio/DoubleBuffer", "remaining", "()I"),
    ("java/nio/FloatBuffer", "array", "()[F"),
    ("java/nio/FloatBuffer", "capacity", "()I"),
    ("java/nio/FloatBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/FloatBuffer", "hasRemaining", "()Z"),
    ("java/nio/FloatBuffer", "limit", "()I"),
    ("java/nio/FloatBuffer", "position", "()I"),
    ("java/nio/FloatBuffer", "remaining", "()I"),
    ("java/nio/IntBuffer", "array", "()[I"),
    ("java/nio/IntBuffer", "capacity", "()I"),
    ("java/nio/IntBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/IntBuffer", "hasRemaining", "()Z"),
    ("java/nio/IntBuffer", "limit", "()I"),
    ("java/nio/IntBuffer", "position", "()I"),
    ("java/nio/IntBuffer", "remaining", "()I"),
    ("java/nio/LongBuffer", "array", "()[J"),
    ("java/nio/LongBuffer", "capacity", "()I"),
    ("java/nio/LongBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/LongBuffer", "hasRemaining", "()Z"),
    ("java/nio/LongBuffer", "limit", "()I"),
    ("java/nio/LongBuffer", "position", "()I"),
    ("java/nio/LongBuffer", "remaining", "()I"),
    ("java/nio/ShortBuffer", "array", "()[S"),
    ("java/nio/ShortBuffer", "capacity", "()I"),
    ("java/nio/ShortBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/ShortBuffer", "hasRemaining", "()Z"),
    ("java/nio/ShortBuffer", "limit", "()I"),
    ("java/nio/ShortBuffer", "position", "()I"),
    ("java/nio/ShortBuffer", "remaining", "()I"),
    (
        "java/nio/StringCharBuffer",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/nio/file/Files",
        "getOwner",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;",
    ),
    (
        "java/nio/file/Files",
        "readAllLines",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/util/List;",
    ),
    (
        "java/nio/file/Files",
        "readString",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/lang/String;",
    ),
    (
        "java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/util/Set;ILjava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;Ljava/nio/charset/Charset;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
    ),
];

/// Lane 4 wave 18: the eleven Java wrappers of `java.io.UnixFileSystem`, the
/// Linux twin of wave 17's `WinNTFileSystem` table.
///
/// `RETIRED_SHADOW_L4_WINNTFS_TRIPLES`' doc comment ends "the table names
/// `WinNTFileSystem` only, so `UnixFileSystem` (Linux image) is untouched and
/// stays a separate measurement on a Linux host". This is that measurement,
/// taken 2026-09-20 on linux/x86_64 against Temurin 25.0.4+7 -- the first time
/// this lane has run on the platform whose rows these are.
///
/// The shape is identical to the Windows one: JDK 25's `UnixFileSystem`
/// declares each operation as a Java method that calls the JNI entry point of
/// the same name suffixed `0`, and this VM shadows BOTH. Measured in the
/// lane-4 registry census on Linux, every `*0` the eleven wrappers need is
/// registered and answering (`getBooleanAttributes0` 73 invocations,
/// `delete0` 19, `canonicalize0` 12, `rename0` 4) -- so retiring the wrapper
/// hands it to bytecode that calls a native which already exists.
///
/// NOT in the table, and each for a measured reason:
///
/// * `<init>()V` -- the carrier is VM-minted and the constructor is what
///   writes it. Wave 17 kept `WinNTFileSystem.<init>` native for the same
///   reason and `unconstructed_carrier_gate` is the guard that says so.
/// * the `*0` JNI natives -- bucket D (`ACC_NATIVE` in the image). §1.5
///   *requires* them to stay `Bridge`; they are the surface the retired
///   wrappers call.
/// * `getNameMax0`, `delete0(File,boolean)`, `getDriveDirectory`,
///   `getFinalPath0`, `listRoots0` -- registered under descriptors JDK 25's
///   `UnixFileSystem` does not declare (bucket F), so a table row would match
///   nothing.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_UNIXFS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/UnixFileSystem",
        "checkAccess",
        "(Ljava/io/File;I)Z",
    ),
    (
        "java/io/UnixFileSystem",
        "createDirectory",
        "(Ljava/io/File;)Z",
    ),
    (
        "java/io/UnixFileSystem",
        "createFileExclusively",
        "(Ljava/lang/String;)Z",
    ),
    (
        "java/io/UnixFileSystem",
        "getBooleanAttributes",
        "(Ljava/io/File;)I",
    ),
    (
        "java/io/UnixFileSystem",
        "getLastModifiedTime",
        "(Ljava/io/File;)J",
    ),
    ("java/io/UnixFileSystem", "getLength", "(Ljava/io/File;)J"),
    ("java/io/UnixFileSystem", "getSpace", "(Ljava/io/File;I)J"),
    (
        "java/io/UnixFileSystem",
        "list",
        "(Ljava/io/File;)[Ljava/lang/String;",
    ),
    (
        "java/io/UnixFileSystem",
        "setLastModifiedTime",
        "(Ljava/io/File;J)Z",
    ),
    (
        "java/io/UnixFileSystem",
        "setPermission",
        "(Ljava/io/File;IZZ)Z",
    ),
    ("java/io/UnixFileSystem", "setReadOnly", "(Ljava/io/File;)Z"),
];

/// Lane 4 wave 17: `java.io.File.createTempFile(String,String)`, which
/// delegates to the three-argument form (retired earlier) -- and, since wave
/// 18, `File.deleteOnExit()`.
///
/// # `deleteOnExit` was blocked on a VM gap, and wave 18 closed the gap
///
/// Wave 17 measured the row as correct and un-retirable: its bytecode registers
/// `DeleteOnExitHook` through `JavaLangAccess.registerShutdownHook`, which ends
/// in `java.lang.Shutdown.add(2, true, hook)`, and this VM ran none of
/// `Shutdown`'s hook SLOTS at exit (`shutdown hooks: ran=0`). With the row
/// retired, HotSpot left `L4FileTempExit`'s temp directory empty and this VM
/// left the file, the directory and its child on disk.
///
/// `lang_system::run_jdk_shutdown_slots` now walks that array on every exit
/// path, so the JDK's own hook runs where HotSpot runs it -- after the
/// application hooks, which ARE slot 1. Re-measured 2026-09-20 on
/// linux/x86_64: `L4FileTempExit`'s directory is empty after the run on both
/// VMs, and `L4ShutdownSlots` reports the same non-null `slot[2]` on both.
/// The row is in the table because the blocker is gone, not because the
/// verdict changed.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FILETMP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/File",
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/File;",
    ),
    ("java/io/File", "deleteOnExit", "()V"),
];

/// Lane 4 wave 17: the `java.nio.file` static surface and the Windows provider
/// rows behind it -- `Files` (all 32 registered rows), `FileVisitResult`'s two
/// enum synthetics, `FileSystemProvider.newInputStream`/`newOutputStream`, and
/// the `WindowsFileSystemProvider` / `WindowsFileStore` /
/// `WindowsFileSystem.getFileStores` / `WindowsDirectoryStream.spliterator`
/// rows that had never been invoked (so the funnel's `invocations > 0` filter
/// never showed them) and that the retired `Files` surface now reaches.
///
/// # Why the four provider mutators are in the same table as `Files`
///
/// `WindowsFileSystemProvider.{copy,createDirectory,delete,move}` were
/// registered by `concrete_receiver.rs` as natives that DELEGATE to the
/// matching `Files` native (`nio_file.rs`, "one body per operation"). With the
/// `Files` rows retired, `Files.delete` is bytecode that calls
/// `provider.delete`, which is that native, which calls `Files.delete`:
/// unbounded native recursion (`EXCEPTION_STACK_OVERFLOW` at the first
/// `Files.createDirectory` of the run). Found by retiring each `Files` method
/// alone: exactly `copy`, `createDirectory`, `delete` and `move` overflowed.
/// Retiring the provider halves with them is what turns the loop into the JDK
/// body, `WindowsNativeDispatcher.CreateDirectory0`/`DeleteFile0`/
/// `RemoveDirectory0`/`MoveFileEx0`/`CopyFileEx0` -- the wave-15 natives that
/// nothing had reached until now.
///
/// NOT in the table, each with its named blocker:
///
/// - `FileSystems.getDefault`, `FileSystems.newFileSystem` (both overloads;
///   retired since by the 2026-09-23 archive-provider wave, see
///   `RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES`),
///   `WindowsFileSystemProvider.getFileSystem(URI)`: `getDefault` is the
///   identity anchor for the native `Path`/`FileSystem` carrier. Retiring
///   `getFileSystem(URI)` alone measured a second, distinct default
///   filesystem (`FileSystems.getFileSystem(file:///) == getDefault()` turns
///   `false`); the `newFileSystem` rows measured a regression
///   (`ProviderNotFoundException` for a non-archive became "no throw")
///   because they lean on `installedProviders`, below.
/// - `FileSystemProvider.installedProviders` (see the wave-9 table): the
///   native answers `file`/`jar`/`jrt`; the real body's `ServiceLoader`
///   discovery would answer one. (Measured false 2026-09-23; retired by
///   `RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES`.)
/// - `WindowsFileSystemProvider.checkAccess` and `newWatchService` /
///   `WindowsPath.register`: the security-token and IOCP families
///   (wave 16's table names them).
/// - `WindowsFileSystemProvider.newAsynchronousFileChannel`: IOCP again.
/// - `WindowsFileSystemProvider.newFileSystem(Path,Map)`: the class inherits
///   the abstract default, which throws; the native answers an archive
///   filesystem. `newFileSystem(URI,Map)` and `getScheme()` (wave 16's table)
///   are the same species, missed in the original wave and held out
///   2026-09-22: every logical provider (`file`/`jar`/`jrt`) this VM mints
///   shares this one concrete class, so retiring either row hands EVERY
///   provider instance real bytecode's single hard-coded "file"-provider
///   answer, and an in-process `javac` can never mount a classpath jar or the
///   runtime image (see the row's own comment).
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FILES2_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/file/FileVisitResult",
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/FileVisitResult;",
    ),
    (
        "java/nio/file/FileVisitResult",
        "values",
        "()[Ljava/nio/file/FileVisitResult;",
    ),
    (
        "java/nio/file/Files",
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "createDirectories",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "createFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "createTempDirectory",
        "(Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "delete",
        "(Ljava/nio/file/Path;)V",
    ),
    (
        "java/nio/file/Files",
        "deleteIfExists",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "java/nio/file/Files",
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
    ),
    (
        "java/nio/file/Files",
        "find",
        "(Ljava/nio/file/Path;ILjava/util/function/BiPredicate;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
    ),
    (
        "java/nio/file/Files",
        "getLastModifiedTime",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "java/nio/file/Files",
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
    ),
    (
        "java/nio/file/Files",
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "java/nio/file/Files",
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
    ),
    (
        "java/nio/file/Files",
        "isSymbolicLink",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "java/nio/file/Files",
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "java/nio/file/Files",
        "lines",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
    ),
    (
        "java/nio/file/Files",
        "list",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
    ),
    (
        "java/nio/file/Files",
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
    ),
    (
        "java/nio/file/Files",
        "notExists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
    ),
    (
        "java/nio/file/Files",
        "probeContentType",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
    ),
    (
        "java/nio/file/Files",
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
    ),
    (
        "java/nio/file/Files",
        "readAllLines",
        "(Ljava/nio/file/Path;)Ljava/util/List;",
    ),
    (
        "java/nio/file/Files",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
    ),
    (
        "java/nio/file/Files",
        "readString",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
    ),
    (
        "java/nio/file/Files",
        "size",
        "(Ljava/nio/file/Path;)J",
    ),
    (
        "java/nio/file/Files",
        "walk",
        "(Ljava/nio/file/Path;I[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
    ),
    (
        "java/nio/file/Files",
        "walk",
        "(Ljava/nio/file/Path;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
    ),
    (
        "java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/Files",
        "writeString",
        "(Ljava/nio/file/Path;Ljava/lang/CharSequence;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "newOutputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/OutputStream;",
    ),
    (
        "sun/nio/fs/WindowsDirectoryStream",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "getAttribute",
        "(Ljava/lang/String;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "getFileStoreAttributeView",
        "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "getUnallocatedSpace",
        "()J",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "getUsableSpace",
        "()J",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "isReadOnly",
        "()Z",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "name",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "supportsFileAttributeView",
        "(Ljava/lang/Class;)Z",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "supportsFileAttributeView",
        "(Ljava/lang/String;)Z",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/fs/WindowsFileStore",
        "type",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/fs/WindowsFileSystem",
        "getFileStores",
        "()Ljava/lang/Iterable;",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "delete",
        "(Ljava/nio/file/Path;)V",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    // newFileSystem(URI, Map) held OUT (2026-09-22, Spring in-process javac):
    // `p57_alloc_provider` mints EVERY logical provider (file/jar/jrt) as
    // this SAME concrete platform class, distinguished only by a "scheme"
    // field this native reads (see its own registration, `java/nio/file/spi/
    // FileSystemProvider` in `nio_file.rs`, category Bridge). Retiring the
    // mirrored copy on `WindowsFileSystemProvider` hands the call to REAL
    // bytecode, which has no idea about the scheme field and always behaves
    // as the FILE-scheme provider -- so a real `javac` classpath jar (or the
    // runtime image) can never be mounted: `Locations.<*>` gets
    // `ProviderNotFoundException`/`UnsupportedOperationException` from every
    // provider it tries, reported as "no file system provider is available
    // to handle this file". MEASURED: `TestCompilerTests`/
    // `CandidateComponentsIndexerTests` (`org.springframework.core.test.tools
    // .TestCompiler` / `org.springframework.context.index.test.TestCompiler`,
    // both backed by `ToolProvider.getSystemJavaCompiler()`) fail every
    // method under `--real-jdk` with this error; `CRATONVM_UNRETIRE_NATIVE_
    // SHADOW=sun/nio/fs/WindowsFileSystemProvider.newFileSystem` alone does
    // not clear it (see `getScheme`, held out the same day in the wave-16
    // table below, for the companion half of the SAME defect).
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "newOutputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/OutputStream;",
    ),
    // readAttributes(Path,String,LinkOption[]) held OUT (2026-09-22): the
    // name-keyed sibling of the Class-keyed overload above/in the wave-16
    // table -- same jarfs/jrtfs-aware `fsp` body (`read_named_attributes`),
    // same carrier-sharing defect.
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
    ),
];

/// Lane 4 wave 17: originally 65 rows over 20 classes -- the buffer,
/// `CoderResult`, `ByteOrder`, `FileDescriptor$1`, `NativeThreadSet`,
/// `FileChannelImpl` and stream-constructor rows the funnel still counted
/// after waves 1-16. Now 63 rows over 19 classes: `sun/nio/ch/Util`'s two rows
/// were removed 2026-09-22 (see below the table's closing bracket).
///
/// Every row is an `owns_slot`, `Bridge`, image-`Code` row that a lane-4 probe
/// invoked (`invocations > 0`), so each hands a method the JDK has real
/// bytecode for to that bytecode. `L4ByteBufferSweep`, `L4TypedBufferSweep`,
/// `L4CharBufferSweep`, `L4StreamTailSweep`, `L4PrintStreamSweep`,
/// `L4W6PrintCarrier`, `L4FileSweep` and `L4Reach` all stay at zero strict diff
/// lines against HotSpot with the whole table retired (debug binary).
///
/// `Buffer.session()` / `checkSession()` were in this table's first cut and are
/// OUT: `RETIRED_SHADOW_JOTP_TRIPLES`'s doc comment measured, on real netty
/// tests, that a segment-backed buffer's real `session()` runs against this
/// VM's own arena carrier (`NoSuchMethodError` on
/// `ArenaImpl.checkValidStateRaw`), and `the_jotp_wave_refuses_the_buffer_
/// session_pair` pins them out. The regression suite and every lane-4 probe
/// were green with them retired, which is the point: none of them builds a
/// segment-backed buffer. The rows that remain (`Buffer$2.acquireSession` /
/// `releaseSession`, the `DirectByteBuffer` accessors) reach the session only
/// through `session()`, which stays the native shim; that is an argument from
/// the bytecode, NOT a measurement, and the two netty tests above are the
/// verification still owed.
///
/// Not in the table, on purpose: `java/io/ByteArrayInputStream`'s three
/// funnel rows (wave 5 carved them out: they carry an OBSERVATION),
/// `java/nio/file/FileSystems`'s three, `FileSystemProvider.installedProviders`,
/// `WindowsFileAttributes.fileKey` and the two Windows watch/access rows (each
/// named in the table above this one).
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_BUFFERS2_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/BufferedOutputStream",
        "flush",
        "()V",
    ),
    (
        "java/io/BufferedOutputStream",
        "write",
        "(I)V",
    ),
    (
        "java/io/BufferedOutputStream",
        "write",
        "([BII)V",
    ),
    (
        "java/io/FileDescriptor$1",
        "get",
        "(Ljava/io/FileDescriptor;)I",
    ),
    (
        "java/io/FileDescriptor$1",
        "getAppend",
        "(Ljava/io/FileDescriptor;)Z",
    ),
    (
        "java/io/FileDescriptor$1",
        "getHandle",
        "(Ljava/io/FileDescriptor;)J",
    ),
    (
        "java/io/FileDescriptor$1",
        "registerCleanup",
        "(Ljava/io/FileDescriptor;)V",
    ),
    (
        "java/io/FileDescriptor$1",
        "setAppend",
        "(Ljava/io/FileDescriptor;Z)V",
    ),
    (
        "java/io/FileDescriptor$1",
        "setHandle",
        "(Ljava/io/FileDescriptor;J)V",
    ),
    (
        "java/io/FilterInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
    ),
    (
        "java/nio/Buffer",
        "<init>",
        "(IIIILjava/lang/foreign/MemorySegment;)V",
    ),
    (
        "java/nio/Buffer",
        "capacity",
        "()I",
    ),
    (
        "java/nio/Buffer",
        "limit",
        "()I",
    ),
    (
        "java/nio/Buffer",
        "position",
        "()I",
    ),
    (
        "java/nio/Buffer$2",
        "acquireSession",
        "(Ljava/nio/Buffer;)V",
    ),
    (
        "java/nio/Buffer$2",
        "getBufferAddress",
        "(Ljava/nio/Buffer;)J",
    ),
    (
        "java/nio/Buffer$2",
        "releaseSession",
        "(Ljava/nio/Buffer;)V",
    ),
    (
        "java/nio/ByteBuffer",
        "get",
        "([BII)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteOrder",
        "nativeOrder",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteOrder",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "get",
        "()B",
    ),
    (
        "java/nio/DirectByteBuffer",
        "get",
        "(I)B",
    ),
    (
        "java/nio/DirectByteBuffer",
        "getChar",
        "(I)C",
    ),
    (
        "java/nio/DirectByteBuffer",
        "getInt",
        "(I)I",
    ),
    (
        "java/nio/DirectByteBuffer",
        "getLong",
        "(I)J",
    ),
    (
        "java/nio/DirectByteBuffer",
        "getShort",
        "(I)S",
    ),
    (
        "java/nio/DirectByteBuffer",
        "isDirect",
        "()Z",
    ),
    (
        "java/nio/DirectByteBuffer",
        "isReadOnly",
        "()Z",
    ),
    (
        "java/nio/DirectByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "put",
        "(IB)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "putChar",
        "(IC)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "putInt",
        "(II)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "putLong",
        "(IJ)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DirectByteBuffer",
        "putShort",
        "(IS)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "get",
        "([DII)Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "put",
        "([DII)Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "get",
        "([FII)Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "put",
        "([FII)Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/HeapByteBuffer",
        "<init>",
        "([BIILjava/lang/foreign/MemorySegment;)V",
    ),
    (
        "java/nio/HeapByteBuffer",
        "isDirect",
        "()Z",
    ),
    (
        "java/nio/HeapByteBuffer",
        "isReadOnly",
        "()Z",
    ),
    (
        "java/nio/IntBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
    ),
    (
        "java/nio/IntBuffer",
        "get",
        "([III)Ljava/nio/IntBuffer;",
    ),
    (
        "java/nio/IntBuffer",
        "put",
        "([III)Ljava/nio/IntBuffer;",
    ),
    (
        "java/nio/LongBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
    ),
    (
        "java/nio/LongBuffer",
        "get",
        "([JII)Ljava/nio/LongBuffer;",
    ),
    (
        "java/nio/LongBuffer",
        "put",
        "([JII)Ljava/nio/LongBuffer;",
    ),
    (
        "java/nio/MappedByteBuffer",
        "isLoaded",
        "()Z",
    ),
    (
        "java/nio/ShortBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
    ),
    (
        "java/nio/ShortBuffer",
        "get",
        "([SII)Ljava/nio/ShortBuffer;",
    ),
    (
        "java/nio/ShortBuffer",
        "put",
        "([SII)Ljava/nio/ShortBuffer;",
    ),
    (
        "java/nio/charset/CoderResult",
        "isError",
        "()Z",
    ),
    (
        "java/nio/charset/CoderResult",
        "isMalformed",
        "()Z",
    ),
    (
        "java/nio/charset/CoderResult",
        "isOverflow",
        "()Z",
    ),
    (
        "java/nio/charset/CoderResult",
        "isUnderflow",
        "()Z",
    ),
    (
        "java/nio/charset/CoderResult",
        "isUnmappable",
        "()Z",
    ),
    (
        "sun/nio/ch/FileChannelImpl",
        "open",
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;",
    ),
    (
        "sun/nio/ch/FileChannelImpl$Closer",
        "run",
        "()V",
    ),
    (
        "sun/nio/ch/NativeThreadSet",
        "add",
        "()I",
    ),
    (
        "sun/nio/ch/NativeThreadSet",
        "remove",
        "(I)V",
    ),
    (
        "sun/nio/ch/NativeThreadSet",
        "signalAndWait",
        "()V",
    ),
];

