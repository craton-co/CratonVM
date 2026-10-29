// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One table of the `--compatible` behaviours that are aligned with HotSpot.
//!
//! Both modes target HotSpot 25 (AGENTS.md). `--jdk-only` has every row of
//! this table by construction; `--compatible` has a row when its bit is set.
//! Each row used to be its own `--compatible` variable, its own `bool`
//! field, its own `JdkOnly || field` accessor and, when a native asked, its
//! own `NativeContext` method. A new alignment is now one variant here and
//! one call site (`registry.aligned(X)`, `ctx.aligned(X)`,
//! `ClassManager::aligned(X)`), and reverting one on a host is still one
//! variable:
//!
//! ```text
//! CRATONVM_COMPAT_ALIGN=all            every row on
//! CRATONVM_COMPAT_ALIGN=none           every row off (the old --compatible)
//! CRATONVM_COMPAT_ALIGN=-catch-types-at-link,+component-type-from-array-class
//! ```
//!
//! Items are applied left to right over the defaults ([`HotSpotAlignment::
//! default_on`]); a bare name means `+name`. A row that had its own variable
//! before this table (its [`HotSpotAlignment::legacy_key`]) still answers
//! that variable, read before the list, so `CRATONVM_COMPAT_LOADER_THROWS=0`
//! keeps working for one wave.
//!
//! The set is a `Copy` value read once per VM by `vm_init` and handed to the
//! class manager, the native registry and the class realm: no process
//! global (AGENTS.md). (A registry built outside `vm_init`, as unit tests
//! build one, reads its own at construction.) Interpreter round i1 wave 50,
//! lane L5 (`i49-L6-proposal-one-hotspot-alignment-table-for-compatible-mode`,
//! first stage).

use crate::compat::CompatibilityMode;

/// One `--compatible` behaviour aligned with HotSpot (see the module doc).
/// The discriminant is the bit index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HotSpotAlignment {
    /// The JDK's parallel-capable loader model (wave 48, lane L5).
    ParallelLoaders = 0,
    /// JVMS §5.3 initiating-loader records (wave 48, lane L5).
    InitiatingRecords = 1,
    /// JVMS §5.3.4 loader constraints, recorded and enforced (wave 49, lane L5).
    LoaderConstraints = 2,
    /// A user loader's throwable from a VM-driven `loadClass` is the
    /// resolution's error (wave 49, lane L5).
    LoaderThrows = 3,
    /// A user loader that may answer a JDK-global name is asked first
    /// (wave 49, lane L5).
    GlobalNameAsk = 4,
    /// A `URL`'s application `URLStreamHandler` is authoritative (wave 49,
    /// lane L6).
    UrlAppHandler = 5,
    /// VM-raised messages name modules and loaders as HotSpot does (wave 49,
    /// lane L6).
    ModuleMessages = 6,
    /// The verifier's catch-type loads run at link time (wave 50, lane L5;
    /// probe `CatchTypeLinkProbe`).
    CatchTypesAtLink = 7,
    /// JVMS §5.3.5 supertype shape checks at define (wave 50, lane L5; probe
    /// `L5W29SupertypeShapeChecks`).
    SupertypeShapeChecks = 8,
    /// A user loader's class under a JDK package name is in that loader's
    /// unnamed module (wave 50, lane L5; probe `L5W28JdkNamedModule`).
    UserLoaderUnnamedModule = 9,
    /// `ClassLoader.findLoadedClass` refuses a name `checkName` refuses
    /// (wave 50, lane L5; probe `L5W42LoaderSurface`, `find-loaded-own`).
    FindLoadedNameCheck = 10,
    /// `Class.getComponentType` answers the array class's own component,
    /// whatever its loader, and initializes nothing (wave 50, lane L5; probe
    /// `L5W44LoaderReview`, `forname-array-noinit`).
    ComponentTypeFromArrayClass = 11,
    /// An array type check compares array CLASSES, not component names
    /// (wave 50, lane L5; probe `L5W43NamespacedArrayCast`).
    NamespacedArrayTypes = 12,
    /// Is a name the runtime image's? Asked of the image's own index, not a
    /// package-prefix heuristic (`classloader.rs` `is_runtime_image_class_name`;
    /// wave 50, lane L5; probe `L5W40BootParentGlobalName`).
    RuntimeImageNames = 13,
    /// The stub pre-pass of `resolve_class_loader_aware` asks the referencing
    /// loader once, through the checked door (JVMS §5.3.2: one request, its
    /// throwable the resolution's), before the global route may fabricate a
    /// stub (wave 51, lane L5; probe `L5W41StubPrepassAsk` with
    /// `CRATONVM_LOADER_AWARE_RESOLUTION=0`).
    StubPrepassOneAsk = 14,
    /// An isolated `URLClassLoader`'s refusal of an `invokestatic` owner is
    /// final: the flat application store does not answer it (wave 51, lane
    /// L5; probe `L5W51CompatUrlLoaderRows`, `iso-static`).
    IsolatedStaticOwner = 15,
    /// `Collections.unmodifiable*` and the immutable collections'
    /// serialization carrier run the JDK's own bodies over a real JDK, not
    /// the `cratonvm/internal/Unmodifiable*` stand-ins (wave 51, lane L5;
    /// probe `L5W51UnmodifiableViewsAreTheJdks`).
    JdkUnmodifiableViews = 16,
    /// A NON-array type check against a resolved class (`checkcast`,
    /// `instanceof`, a condy / static-argument cast, the JIT's compiled
    /// twins, `Class.isInstance` / `isAssignableFrom`, a proxy's interface
    /// match) compares classes, not names: another loader's same-named class
    /// is refused (wave 51, lane L4; probes `L4W32CastLoaderText`,
    /// `L4W31CrossLoaderArrayStore`'s `isAssignableFrom` rows,
    /// `L4W51NamespacedClassTypes`). Default on since wave 52 (lane L4):
    /// `interpreter-L4-compatible-type-checks-keep-the-by-name-loader-rule-FIXED`.
    NamespacedClassTypes = 17,
    /// `aastore` judges the element by subtyping against the array's
    /// component class, not by name (wave 51, lane L4; probe
    /// `L4W31CrossLoaderArrayStore`). Default on since wave 52 (lane L4); the
    /// row-off by-name walk (which admits a store into an array created with
    /// a split loader's other copy of the component) is pinned by
    /// `aastore_fails_open_across_a_split_loaders_two_copies_of_one_name`.
    NamespacedArrayStores = 18,
    /// Over a real JDK, the `URLClassLoader` / `URLClassPath` family runs the
    /// JDK's own bodies, as under `--jdk-only` since the wave-24 fat-jar
    /// retirement: the 26 `RETIRED_SHADOW_URLCLASSLOADER_TRIPLES` stand-ins
    /// are not registered, `ucl_try_define_local_class` refuses, an isolated
    /// or bare `URLClassLoader` runs its real `findClass` and keeps its own
    /// namespace, a user loader's `getResource*` runs `ClassLoader`'s body,
    /// `BootLoader.findResource(s)` answer the image's boot modules, and the
    /// URLs the VM builds come from the JDK's `URL(String)` constructor
    /// (handler included). So a URL's own `URLStreamHandler` serves the
    /// loader's classes and resources (wave 52, lane L5; probe
    /// `L5W51CompatUrlLoaderRows`, `handler` rows; positive control
    /// `L5W52CompatUrlClassLoaderBodies`). Since the i50-L5 page's last item
    /// the eight `URL` accessor stand-ins go with it too
    /// (`retired_shadow::is_url_accessor_shadow`). Needs a real JDK: the synthetic
    /// class library has no `URLClassPath` bytecode to fall to
    /// (`NativeMethodRegistry::jdk_url_class_loaders`). Inert on Windows
    /// until `--compatible`'s `java.io.File` normalises `\C:\dir` as the
    /// JDK does (`i52-L5-compatible-file-keeps-a-slash-before-a-windows-drive`).
    JdkUrlClassLoaders = 19,
    /// Over a real JDK, the application class path is HotSpot's: no fat-JAR /
    /// web-archive flattening (`ClassPath::with_fat_jar_emulation(.., false)`)
    /// and absolute entries (`vm_init` `jdk_only_app_class_path_entry`), so a
    /// Spring Boot jar's `JarLauncher` loads the application through its
    /// real `LaunchedClassLoader`. Read before the class manager is built.
    /// Separate from [`Self::JdkUrlClassLoaders`] (which it needs: without it
    /// nothing can read a `jar:nested:` URL) so a host can keep the real
    /// loaders and revert the class-path change alone, the change that costs
    /// a fat jar's start ~2.7x (wave 52, lane L5). In force only with
    /// [`Self::JdkUrlClassLoaders`] in force (so not on Windows yet).
    UnflattenedAppClassPath = 20,
    /// A user loader's initiating RECORD (JVMS §5.3, uncapped) answers a
    /// name the capped per-loader memo has no row for (evicted past its cap,
    /// or never memoised: `Class.forName`), so the loader is not asked again
    /// as HotSpot does not ask it; the memo still answers first, including
    /// the names only it holds (`--compatible`'s global-fallback answers).
    /// Needs [`Self::InitiatingRecords`]. `--jdk-only` has the
    /// stricter form (`CRATONVM_RECORD_REPLACES_MEMO`: the record alone).
    /// Wave 52, lane L5 (the i38-L5 record-replaces-memo proposal's
    /// `--compatible` stage; probe `L5W49InitiatingRecordEviction`,
    /// `evicted`).
    RecordBehindMemo = 21,
    /// Over a real JDK, `java.io.File` and the platform `FileSystem`'s Java
    /// wrappers (`WinNTFileSystem` / `UnixFileSystem` `getLength`, `list`,
    /// ...) run the JDK's own bodies, as under `--jdk-only` since lane 4's
    /// waves 1-18: their `RETIRED_SHADOW_*` rows are not registered
    /// (`retired_shadow::is_file_family_shadow`), so a `File` is built by its
    /// real constructor (`fs.normalize`, `prefixLength`), caches its `Path`,
    /// and a subclass's `getPath` override is what `toString` / `getName`
    /// read. The `*0` JNI entry points stay (`ACC_NATIVE`), and the VM's own
    /// `File` producers already write the real fields (`file_layout`).
    /// Registration-time (no answer site to count). Wave 53, lane L5 (the
    /// i52-L5 proposal's first family; probe `L5W53CompatFileBodies`).
    JdkFileBodies = 22,
    /// Over a real JDK, a `jar:file:` URL that carries the JDK's own `jar`
    /// handler is opened by that handler: `URL.openConnection` answers the
    /// JDK's `sun.net.www.protocol.jar.JarURLConnection` and `URL.openStream`
    /// its body, `openConnection().getInputStream()`, so a jar entry's stream
    /// is `JarURLConnection$JarURLInputStream` (as a `file:` entry's already
    /// is the JDK's `FileURLConnection`'s) and its `JarFile` the JDK's
    /// `URLJarFile`, not the VM's carrier and a `ByteArrayInputStream` over
    /// bytes it read itself. Only a single-level `jar:file:` URL to a
    /// non-`.jmod` archive (the VM's nested-jar and jmod spellings are its own
    /// extensions, which the JDK handler refuses). Needs
    /// [`Self::JdkUrlClassLoaders`] (the URLs the VM mints carry the handler).
    /// `--jdk-only` already runs the JDK bodies (the `URL` bridges lose to
    /// bytecode there). Default on since lane L5's wave-57 switch-on commit.
    /// Wave 57, lane L5 (the JAR half of
    /// `docs/feature-designs/jdk-only-mode.md`; probe
    /// `L5W57JarResourceStreams`).
    JdkJarUrlConnections = 23,
    /// Over a real JDK, the `java.util.jar` / `java.util.zip` archive family
    /// runs the JDK's own bodies, as under `--jdk-only` since the 2026-09-24
    /// `JarFile` retirement: the `RETIRED_SHADOW_JARFILE_TRIPLES` stand-ins
    /// (every `JarFile` constructor and accessor, `ZipFile$1.getManifestName`),
    /// the `ZipFile` / `ZipEntry.setComment` and `JarEntry` / `Manifest` /
    /// `Attributes` rows `--jdk-only` retired with them
    /// (`retired_shadow::is_jar_file_family_shadow`), the `ZipFile.close`
    /// `Intrinsic`, and the Spring Boot nested-jar and launcher natives that
    /// only existed to stand in front of the compact `JarFile`
    /// (`NativeMethodRegistry::jdk_jar_file_bodies`) are not registered. So a
    /// `JarFile` is built by its real constructor, a closed one throws
    /// `IllegalStateException("zip file closed")` from `ensureOpen`, and a
    /// subclass (Spring Boot's `NestedJarFile`, the JDK's `URLJarFile`) reads
    /// the state its `super(...)` really set. Registration-time (no answer
    /// site to count). Wave 58, lane L5 (the i52-L5 proposal's per-family row
    /// for the jar family; probe `L5W58CompatJarFileBodies`; the defect
    /// `docs/feature-designs/jdk-only-mode.md`).
    JdkJarFileBodies = 24,
    /// Over a real JDK, `ForkJoinPool` / `ForkJoinTask` / `RecursiveTask` /
    /// `RecursiveAction` run the JDK's own bodies, as under `--jdk-only` since
    /// SBR wave 4 lane FJP: the family's retired rows (`RETIRED_SHADOW_SBR_FJP_TRIPLES`,
    /// `RETIRED_SHADOW_SBR_FJP2_TRIPLES`) are not registered, so a task submitted
    /// to a pool runs on one of the pool's own daemon workers, not inline on the
    /// caller. Registration-time (SBR round 4, lane compat).
    JdkForkJoinPool = 25,
    /// Over a real JDK, the JCA provider family (`Security`, `Provider`,
    /// `Provider$Service`, `sun.security.jca.*`) runs the JDK's own bodies, as
    /// under `--jdk-only` since SBR wave 1 lane JCA / wave 2 lane JCA2:
    /// `Security.getProvider("SUN")` is the installed `sun.security.provider.Sun`,
    /// the same object on every call. Registration-time, plus the two readers
    /// in `jca/provider_chain.rs` that key on the family (SBR round 4, lane
    /// compat).
    JdkProviders = 26,
    /// Over a real JDK, `ModuleLayer.boot()` and the boot `Configuration` are
    /// the JDK's own objects (`finish_boot_layer`), as under `--jdk-only`, so a
    /// `--module-path` module is in the boot layer (SBR round 4, lane compat).
    JdkModuleGraph = 27,
    /// Over a real JDK, `java.util.HashMap`, `LinkedHashMap`, `HashSet` and
    /// `LinkedHashSet` (with their views, iterators and nodes) run the JDK's
    /// own bodies, as under `--jdk-only` since lane 1's waves 3 and 9 and round
    /// 13 wave 7: the family's retired rows are not registered (a
    /// `retired_shadow::COMPAT_FAMILIES` entry), and the integer overlay
    /// (`native-collections` `try_hm_int_fast_put`), a side table the real
    /// bodies cannot read, stays empty under the row. So a compiled caller binds
    /// and inlines `HashMap.get` / `put` and a user key's `hashCode` / `equals`
    /// as the default mode does, instead of a native door that calls back into
    /// Java per probe. The VM's own map producers already build the real layout
    /// (`--jdk-only` reads them). JIT round 16 (hcb; the osrdoor `--compatible`
    /// user-key page; probes `R16OsrdoorParts`, `HcbCompatHashBodies`).
    JdkHashCollections = 28,
    /// Over a real JDK, `StringBuilder` / `AbstractStringBuilder` run the
    /// JDK's own bodies, as under `--jdk-only`: their `SyntheticStub` rows
    /// (`lang_string::register_string_builder_natives` and every other
    /// registrar's) are not registered, and `String(AbstractStringBuilder,
    /// Void)` / `String(StringBuilder)` yield to bytecode (STR16-2's decision,
    /// `lang_string::string_builder_ctor_yields_to_bytecode`). On a real image
    /// those natives already worked on the JDK's compact `byte[]` builder, so
    /// the objects do not change; what changes is that `maybeLatin1` and
    /// every other field is maintained by the JDK's code, which is what let
    /// the constructor stay bytecode in strict mode. Measured: every builder
    /// op was a 1.2-2.4 us native call in `--compatible` against 50-200 ns in
    /// `--jdk-only` (`R16StringsSplit`), `R16Bench strings` 7.2-8.1 s against
    /// ~1.1 s. The int -> decimal trio (`Integer.toString(I)`,
    /// `Long.toString(J)`, `String.valueOf(I)`) follows the row too: STR16-7's
    /// strict decision (`lang_math::strict_int_to_string_bytecode`) is taken
    /// here as under `--jdk-only`, since `AbstractStringBuilder.append(int)`'s
    /// JDK body already formats through the same `DecimalDigits` code.
    /// Registration-time. Strings round (`str16`, 2026-10-06;
    /// `docs/feature-designs/jdk-only-mode.md`).
    JdkStringBuilderBodies = 29,
    /// Over a real JDK, `java.util.Arrays`' retired rows (`copyOf`,
    /// `copyOfRange`, `fill`, `equals`, `hashCode`, `sort`, `binarySearch`,
    /// `stream`, `toString`; `retired_shadow::COMPAT_FAMILIES`) run the JDK's
    /// bodies, as under `--jdk-only`. `Arrays$ArrayList` is not in the family.
    /// Measured: `StringBuilder.toString()`'s `Arrays.copyOfRange([BII)` was a
    /// generic JIT->native door call in `--compatible` (`toStr` 485-713 ns
    /// against 89-152 `--jdk-only`). Registration-time. Strings round
    /// (`str16`, 2026-10-06).
    JdkArraysBodies = 30,
    /// Over a real JDK, `java.awt.image.BufferedImage`, `sun.java2d.SunGraphics2D`,
    /// `javax.imageio.ImageIO`, the JPEG / PNG plugins and the `java.awt.Font`
    /// accessors run the JDK's own bodies, as under `--jdk-only` since SBR round 5
    /// lane awtjfr: the Java2D loops, the JPEG codec and the font scaler are
    /// native-side Rust (`native-awt/src/java2d`, `native-awt/src/jpeg`), so the
    /// image bytes and pixels are HotSpot's. Registration-time (SBR round 5,
    /// lane awtjfr).
    JdkJava2d = 31,
    /// Over a real JDK, `java.lang.StackWalker`'s retired rows (`getInstance`
    /// x4) run the JDK's bodies, as under `--jdk-only`: the walker is the JDK's
    /// own object and its frames are the JDK's `StackFrameInfo`. SBR round 5,
    /// lane compat2.
    JdkStackWalker = 32,
    /// Over a real JDK, `jdk.internal.misc.Signal` and `sun.misc.Signal` run the
    /// JDK's bodies (`findSignal0` / `handle0` / `raise0` stay the VM's
    /// ACC_NATIVE bridges), as under `--jdk-only`. SBR round 5, lane compat2.
    JdkSignal = 33,
    /// Over a real JDK, `java.lang.Runtime`'s retired rows (`getRuntime`,
    /// `version`, the shutdown hooks, `load0` / `loadLibrary0`) run the JDK's
    /// bodies, and every `ClassLoader` carries a real `NativeLibraries` (what
    /// the real `loadLibrary` reads), as under `--jdk-only`. SBR round 5, lane
    /// compat2.
    JdkRuntime = 34,
    /// Over a real JDK, `java.util.stream` pipelines are the JDK's own
    /// (`ReferencePipeline`, `IntPipeline`, ...), so a parallel stream's work is
    /// split over the common `ForkJoinPool` as on HotSpot. SBR round 5, lane
    /// compat2.
    JdkStreams = 35,
    /// Over a real JDK, `java.lang.Module`'s access rows (`addExports`,
    /// `addOpens`, `addUses`, `isOpen`, `isExported`, `implAdd*`, ...) run the
    /// JDK's bodies, as under `--jdk-only`. SBR round 5, lane compat2.
    JdkModuleAccess = 36,
    /// Over a real JDK, the JSSE family (`SSLContext`, `SSLEngine`,
    /// `SSLSocket`, the `sun.security.ssl` managers, the keystore SPIs) runs the
    /// JDK's bytecode over real keystores, as under `--jdk-only` since SBR wave
    /// 5 lane JSSE. SBR round 5, lane compat2.
    JdkJsse = 37,
    /// Over a real JDK, `java.nio.channels.Pipe` is the JDK's own `PipeImpl`
    /// over `IOUtil.makePipe` (`pipe(2)`), as under `--jdk-only`: the VM's
    /// fabricated pipe (`RETIRED_SHADOW_R19_NIO_PIPE_TRIPLES`) is not
    /// registered, so a non-blocking `source().read` answers `0` on an empty
    /// pipe instead of parking, and a pipe end can be registered with a
    /// `Selector`. JIT round 19 (nio19); probe `NioConf19` (`pipe`).
    JdkPipe = 38,
    /// Over a real JDK, the `java.nio` buffers (`Buffer`, the eight `X`-Buffer
    /// families with their heap / direct / view / read-only classes,
    /// `ByteOrder`, `Bits`), `java.nio.charset` (coders, `CoderResult`) and
    /// `sun.nio.cs` (the charsets and the `StreamDecoder` / `StreamEncoder`
    /// behind `InputStreamReader` / `OutputStreamWriter`) run the JDK's own
    /// bodies, as under `--jdk-only`: their retired rows and `SyntheticStub`
    /// stand-ins are not registered (`ScopedMemoryAccess`'s too, the buffers'
    /// memory door). The stand-ins answered real buffers through the superclass
    /// walk (several native crossings per element of a charset loop) and the
    /// VM's `StreamDecoder` split a surrogate pair across two `read(char[])`
    /// calls into two U+FFFD (`NioConf19` row `coder-isr`). JIT round 19
    /// (nio19).
    JdkNioBuffers = 39,
    /// Over a real JDK, `jdk.internal.util.Preconditions`' `checkIndex` /
    /// `checkFromToIndex` / `checkFromIndexSize` `(.., BiFunction)` overloads
    /// run the JDK's bodies, as under `--jdk-only` (JOTP retired them): every
    /// `CharBuffer.put(String,int,int)` / `subSequence`, `String.getChars`,
    /// `Objects.check*` crossed a stand-in, 160 000 times in one `R17NioRows
    /// heapcb` run. JIT round 19 (nio19).
    JdkPreconditions = 40,
    /// The verifier's reference assignability loads run at link time: the
    /// link step loads, through the class's defining loader, every name
    /// HotSpot's split verifier loads for the class
    /// (`VerificationType::resolve_and_check_assignability`), so that loader
    /// initiates them (`Instrumentation.getInitiatedClasses`, a user loader's
    /// `loadClass`) and a missing one fails the link with its
    /// `NoClassDefFoundError` (interpreter round i2, the i47 page; probes
    /// `L5W55VerifierInitiatedLoads`, `I47VerifierLoadsAtLink`).
    VerifierLoadsAtLink = 41,
    /// Over a real JDK, `java.util.concurrent.locks.LockSupport`'s retired rows
    /// (`park()`, `park(Object)`, `parkNanos(long)`, `parkNanos(Object, long)`,
    /// `parkUntil(Object, long)`, `unpark`, `getBlocker`; the
    /// `RETIRED_SHADOW_*` LockSupport block) run the JDK's bodies, as under
    /// `--jdk-only` (where they are refused stubs): `setBlocker(t, blocker)`
    /// around `U.park(..)`, `getBlocker` reading the `parkBlocker` field the
    /// bodies write, and a virtual thread's park through
    /// `JavaLangAccess.parkVirtualThread` with its blocker set as on HotSpot.
    /// Only `Unsafe.park` / `unpark` (`ACC_NATIVE`) stay the VM's. Registration-
    /// time. Default on (a Java-visible thread-state / blocker answer moved to
    /// HotSpot's); `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0` (its
    /// [`Self::legacy_key`]) or `CRATONVM_COMPAT_ALIGN=-jdk-lock-support`
    /// reverts it. Interpreter round i2 wave 69, lane L1
    /// (`i68-L1-proposal-let-lock-support-blocker-overloads-run-their-bytecode`;
    /// probe `L1W69LockSupportBodies`).
    JdkLockSupport = 42,
    /// Over a real JDK, `java.util.concurrent.locks.StampedLock` and its lock
    /// views run the JDK's bodies, as under `--jdk-only` (where the 31
    /// stand-ins `register_stamped_lock_natives` registers are refused
    /// `SyntheticStub`s): the lock word is the real `state` field, CAS'd
    /// through `Unsafe`, and a contended acquire parks through
    /// `LockSupport.park(this)` / `parkNanos(this, ..)` on the JDK's wait
    /// queue, so `getBlocker`, `ThreadInfo.getLockName()` and a stack walk
    /// (`Unsafe.park` under `LockSupport.park` under `StampedLock.acquire*`)
    /// answer as on HotSpot. The stand-ins kept the lock in a side table keyed
    /// by a GC-stable weak key and mirrored `state` through a receiver read
    /// before a blocking region
    /// (`i68-L1-stamped-lock-stand-ins-reuse-a-pre-region-receiver-20261101`).
    /// A `retired_shadow::COMPAT_FAMILIES` entry naming the class in
    /// `stub_classes`. Registration-time. Default on (a Java-visible
    /// lock / blocker answer moved to HotSpot's);
    /// `CRATONVM_COMPAT_JDK_STAMPED_LOCK=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-stamped-lock` reverts it. Interpreter round
    /// i2 wave 70, lane L1
    /// (`i69-L1-proposal-compatible-stampedlock-runs-its-jdk-bodies`; probe
    /// `L1W70StampedLockBodies`).
    JdkStampedLock = 43,
    /// Over a real JDK, the `java.lang.ref` reference model is `--jdk-only`'s:
    /// the retired constructors, `get`, `Reference.clear`, `WeakReference.
    /// refersTo`, `Reference.reachabilityFence` and `ReferenceQueue.poll` /
    /// `remove` x2 run the JDK's bodies, the processor hears of a `Reference`
    /// at its ALLOCATION (`memory::ref_discovery`, which the native
    /// constructors used to stand in for), and a GC enqueue wakes the real
    /// `ReferenceQueue.remove`'s `lock.wait()` (`queue_wake_ups_recorded`).
    /// Registration-time plus those two VM gates. JIT round 20 (nio20, lane L4;
    /// NIO19-6, `R16Fieldsite2Layout weakref`, `R17NioRows fence`).
    JdkReferences = 44,
    /// Over a real JDK, four of `java.lang.Object`'s retired rows (`<init>`,
    /// `finalize`, `wait()`, `wait(J)`) run the JDK's bodies, as under
    /// `--jdk-only`: `<init>` is an empty body there and here (finalizable
    /// objects are registered at allocation from the class bit, not by the
    /// constructor), `wait` reaches the `wait0` bridge. Every constructor
    /// chain crossed the `<init>` stand-in. `equals` / `toString` stay (they
    /// answer for the VM's generic-reflection carriers;
    /// `retired_shadow::is_object_body_row`). JIT round 20 (nio20, lane L4;
    /// NIO19-6).
    JdkObjectBodies = 45,
    /// Over a real JDK, `jdk.internal.misc.Unsafe.copyMemory(Object,long,
    /// Object,long,long)` and `setMemory(Object,long,long,byte)` run the
    /// JDK's bodies (`copyMemoryChecks` / `setMemoryChecks`, then the
    /// `copyMemory0` / `setMemory0` bridges the stand-ins already shared):
    /// HotSpot's `IllegalArgumentException` for a negative size or a
    /// non-primitive-array base. Only those two rows; the rest of the
    /// `Unsafe` table is not adopted. JIT round 20 (nio20, lane L4; NIO19-6,
    /// `R17NioRows utfencode`).
    JdkUnsafeBulkBodies = 46,
    /// Over a real JDK, `java.util.Optional` and `OptionalInt` / `Long` /
    /// `Double` run the JDK's bodies, as under `--jdk-only`: the natives
    /// already wrote the real layout (`value`; `isPresent`, `value`), so the
    /// objects do not change; what changes is `Optional.empty()` answering the
    /// JDK's `EMPTY` singleton. JIT round 20 (nio20, lane L4; NIO19-6,
    /// `R16Fieldsite2Layout optional`).
    JdkOptional = 47,
    /// Over a real JDK on Linux, the socket / server-socket / datagram
    /// channels, the selector and the `java.net.DatagramSocket` /
    /// `MulticastSocket` family run the JDK's `sun.nio.ch` code over the
    /// VM's real `ACC_NATIVE` layer (`native-io/src/net_nio_real.rs`), as
    /// under `--jdk-only` since SBR wave 5 lane NIOREAL: the
    /// `RETIRED_SHADOW_SBR_NIOREAL_TRIPLES` rows (the factories among them)
    /// are not registered, `System$1.blockedOn` is the JDK's, and the
    /// interruptible waits honour `Thread.nioBlocker`. Inert on Windows (the
    /// table is empty there). JIT round 20 (nio20, lane L4; NIO19-7, the r19
    /// `--compatible` carriers page).
    JdkNioChannels = 48,
    /// Over a real JDK, `java.util.ArrayList` with its `SubList`, `SubList$1`,
    /// `ListItr` and spliterator runs the JDK's own bodies, as under
    /// `--jdk-only`: every retired row and `SyntheticStub` of those classes is
    /// not registered, so `subList` is the JDK's `ArrayList$SubList` (no VM
    /// sublist state past its fields) and every view operation is bytecode.
    /// `ArrayList$Itr` is excepted (the VM mints it for `Vector` backings
    /// too). `R16Fieldsite2Layout sublist` was 5.6x the `--jdk-only` time.
    /// Registration-time. JIT round 20 (nio20 wave 2, lane L5;
    /// `retired_shadow::COMPAT_FAMILIES`).
    JdkArrayList = 49,
    /// Over a real JDK, `java.util.Locale` and its nested classes run the
    /// JDK's own bodies, as under `--jdk-only`: `getDefault` is the JDK's
    /// `defaultLocale` static (set by `Locale.<clinit>`'s `initDefault()`, so
    /// its identity is the JDK's locale cache's, as on HotSpot), and the
    /// accessors read `baseLocale`, not the VM's subtag side tables.
    /// `R16Fieldsite2Layout locale` was 16x the `--jdk-only` time.
    /// Registration-time. JIT round 20 (nio20 wave 2, lane L5).
    JdkLocale = 50,
    /// Over a real JDK, `java.util.Date`'s deprecated field constructors,
    /// getters and setters run the JDK's bodies, as under `--jdk-only`: the
    /// natives computed in UTC and left `cdate` stale after a setter (a later
    /// `toString` printed the old date). Registration-time. JIT round 20
    /// (nio20 wave 2, lane L5).
    JdkDate = 51,
    /// Over a real JDK, `Object.equals` / `Object.toString` run the JDK's
    /// bodies, as under `--jdk-only`. In force only where `--compatible`
    /// builds the JDK's generic-reflection `*Impl` classes
    /// (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`, default on): the stand-ins
    /// answered structurally for the VM's interface-classed carriers, which
    /// that switch retires. Registration-time. JIT round 20 (nio20 wave 2,
    /// lane L5; NIO20-L4-4).
    JdkObjectIdentityBodies = 52,
    /// Over a real JDK on Linux, the asynchronous socket channels and their
    /// groups (`AsynchronousSocketChannel` / `AsynchronousServerSocketChannel`
    /// / `AsynchronousChannelGroup`, `sun.nio.ch.Unix*Asynchronous*Impl`,
    /// `EPollPort`) run the JDK's `sun.nio.ch` code over the VM's real
    /// `ACC_NATIVE` layer, as under `--jdk-only` since SBR wave 6 lane
    /// `asyncnet`: their `RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES` rows (the
    /// factories among them) are not registered, so the Future form of
    /// `accept()` completes, a timed read fails with
    /// `InterruptedByTimeoutException`, and so on (`NioConf19b` `asock`). The
    /// `AsynchronousFileChannel` half of that table is not this row's. Inert on
    /// Windows. JIT round 20 (nio20 wave 3, lane L1).
    JdkAsyncChannels = 53,
    /// Over a real JDK on Linux, `java.net.http` (`HttpClient`, the request /
    /// response / header classes, the body publishers and handlers, and
    /// `WebSocket`) runs the JDK's own `jdk.internal.net.http` client, as under
    /// `--jdk-only`: the `RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES` client rows and
    /// every `SyntheticStub` on `java/net/http/` and `jdk/internal/net/http/`
    /// are not registered, so `HttpClient.newHttpClient()` is the JDK's
    /// `HttpClientFacade` over a real `Selector` (`jdk-nio-channels`, a
    /// prerequisite) and HTTPS goes through `SSLContext.createSSLEngine` (the
    /// JSSE carrier engine unless `jdk-jsse`). The three native clients
    /// (`net_phase_e`, `http2`, `http_client`) mint no carrier and their wire
    /// defaults (`User-Agent: cratonvm-*`, no `Upgrade: h2c`) and their missing
    /// `WebSocket` (`NioConf19 ws` threw `UnsupportedOperationException`) go
    /// with them. Inert on Windows, like `JdkNioChannels`. JIT round 20 (nio20
    /// wave 4, lane L4; the r20 `--compatible` HttpClient carriers page).
    JdkHttpClient = 54,
    /// Over a real JDK, the locale PROVIDERS run the JDK's own bodies, as
    /// under `--jdk-only`: `ResourceBundle` (`getBundle` and the readers,
    /// `$1`, `$Control`), `sun.util.locale.provider` (`LocaleResources`,
    /// `CalendarDataUtility`, `JRELocaleProviderAdapter`), `sun.util.resources`
    /// (`LocaleData`, `Bundles`), `DecimalFormatSymbols.getInstance` /
    /// `initialize`, `BreakIterator`, and the two `Currency` display bridges.
    /// No native-minted bundle or iterator carrier is left (their only
    /// producers are rows of the family), and the JDK's per-`LocaleResources`
    /// caches replace stand-ins that rebuilt a bundle or a symbol array per
    /// call (`R16Fieldsite2Layout locale`, ~12x the `--jdk-only` time with
    /// `jdk-locale` on). Registration-time. JIT round 20 (nio20 wave 5, lane
    /// L4; `retired_shadow::COMPAT_FAMILIES`).
    JdkLocaleProviders = 55,
    /// Over a real JDK on Linux, `com.sun.net.httpserver.HttpServer` is the
    /// JDK's own `sun.net.httpserver.ServerImpl` over the real `Selector` /
    /// `ServerSocketChannel` (`jdk-nio-channels`, a prerequisite), as under
    /// `--jdk-only` since SBR wave 6 lane HTTPSTACK: the server rows of
    /// `RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES` (the `create` factories among
    /// them, so no carrier server is minted) and the carrier's
    /// `SyntheticStub`s are not registered. Without it `--compatible` serves
    /// every `HttpServer` from the VM's Rust carrier (`net_phase_e`
    /// `re10_*`), whose threads, header set and framing are its own, so a
    /// `--compatible` client measured against an in-process server measures a
    /// different server than `--jdk-only` (`SbrW5HttpBench`). Inert on
    /// Windows, like `JdkNioChannels`. Default ON since the nio21 wave 13
    /// host A/B (the JDK server is ~9x the carrier per request under
    /// `--jdk-only`, `huc.get`: a JIT item, not a reason for a second
    /// server). JIT round 20 (nio20 wave 5, lane L1).
    JdkHttpServer = 56,
    /// Over a real JDK, `java.io.PrintWriter` runs the JDK's own bodies, as
    /// under `--jdk-only`: its seven retired rows
    /// (`RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES`: `<init>(OutputStream)`, the
    /// four `println`s, the two `write(String..)`s) are not registered. The
    /// natives rebuilt each line as a Rust `String` (an unpaired surrogate
    /// came out as U+FFFD), pushed every line onto the thread's
    /// `printed_lines` (never drained), and cost a native door per call on
    /// every `PrintWriter` over a `StringWriter` / `OutputStreamWriter`.
    /// `PrintStream` is not in it. Registration-time. JIT round 20 (nio20
    /// wave 6, lane L5).
    JdkPrintWriter = 57,
    /// Ask a user loader for java.* references, once per initiating name (i47).
    JavaNameAsk = 58,
    /// Over a real JDK, `java.util.concurrent.CountDownLatch` runs the JDK's
    /// bodies, as under `--jdk-only` (where the eleven stand-in registrations
    /// -- `register_concurrent_natives`' six and the `surefire_cdl` early
    /// copy of five -- are refused `SyntheticStub`s): the count is the real
    /// `CountDownLatch$Sync` AQS state, and `await` parks through
    /// `AbstractQueuedSynchronizer.acquireSharedInterruptibly` /
    /// `LockSupport.park(this)` with the `Sync` as the blocker, so
    /// `LockSupport.getBlocker`, `ThreadInfo.getLockName()` / `getLockInfo()`,
    /// a stack walk and `toString` answer as on HotSpot, and a virtual thread
    /// unmounts through the JDK's park instead of a VM-local wait key. The
    /// stand-ins kept the count in an `int[]` holder in the `sync` slot,
    /// waited on the latch's own monitor, and set no park blocker
    /// (`i68-L1-countdownlatch-stand-in-sets-no-park-blocker-20261101`).
    /// A `retired_shadow::COMPAT_FAMILIES` entry naming the class in
    /// `stub_classes`. Registration-time. Default on (a Java-visible blocker
    /// / thread-state answer moved to HotSpot's, the `JdkStampedLock` shape);
    /// `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-count-down-latch` reverts it. Interpreter
    /// round i3 wave 71, lane L1 (`i70-L1-proposal-a-countdownlatch-family-row`;
    /// probe `L1W71CountDownLatchBodies`).
    JdkCountDownLatch = 59,
    /// Over a real JDK, `java.util.concurrent.locks.AbstractQueuedLongSynchronizer`
    /// keeps its state in its own `state` field, as under `--jdk-only` (where
    /// the three rows are retired shadows): `getState` / `setState` /
    /// `compareAndSetState` run the JDK's bodies (a `getfield`, a `putfield`,
    /// `Unsafe.compareAndSetLong` on `STATE`, the CAS `AtomicLong` already
    /// runs). The stand-ins (`native-builtins` `register_aqls_state_natives`)
    /// kept the word in a process-global side table keyed by object identity
    /// and never wrote the field, so a deserialized synchronizer read 0, a
    /// reflective or debugger read of `state` disagreed with `getState()`,
    /// and the table was a process global for compatibility state
    /// (`i71-L1-compatible-aqls-state-lives-in-a-process-global-side-table-20261104`).
    /// No `java.base` class extends it, so what moves is application code
    /// that subclasses it. A `retired_shadow::COMPAT_FAMILIES` entry naming
    /// the class in `classes`. Registration-time. Default on. Kill switch
    /// `CRATONVM_COMPAT_JDK_AQLS=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-aqls`. Interpreter round i3 wave 72,
    /// lane L1 (probe `L1W72AqlsBodies`). Since wave 73 the `=0` arm's
    /// stand-ins keep the word in that same field too (the side table is
    /// gone), so the two arms differ only in who runs the three accessors.
    JdkAqls = 60,
    /// Over a real JDK, `java.util.concurrent.CopyOnWriteArrayList` runs the
    /// JDK's bodies, as under `--jdk-only` (where its 16 rows are retired
    /// shadows): every mutator serialises on the list's private `lock`
    /// object, as the JDK's bulk methods (`addAll(int, ..)`, `removeAll`,
    /// `retainAll`, `replaceAll`, `sort`, the sub-list mutators) already did
    /// under `--compatible`. The stand-ins (the `Bridge`s of
    /// `register_concurrent_natives` and `native-collections`'
    /// `register_concurrent_completeness_natives`) locked the LIST's monitor,
    /// so a stand-in `add` and a JDK `removeAll` could interleave and lose an
    /// update, and a program holding `synchronized (list)` blocked every
    /// stand-in mutator
    /// (`i71-L1-compatible-copyonwritearraylist-bridges-lock-the-list-not-its-lock-20261104`).
    /// `CopyOnWriteArraySet` is not in it (its rows are routed by
    /// `native-collections`' `cow_set_route`). A `COMPAT_FAMILIES` entry
    /// naming the class in `classes`. Registration-time. Default ON since the
    /// wave-74 triage (it started OFF: the 2026-07-14 bridges were added
    /// around a `this.lock` NPE on Spring's COWAL-backed lists, which
    /// `L1W74CopyOnWriteLockShapes` shows does not recur).
    /// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-copy-on-write` turns it off. Interpreter
    /// round i3 wave 72, lane L1 (probe
    /// `L1W72CopyOnWriteBodies`). Since wave 73 the row (read at run time
    /// through `NativeContext::aligned`) also has `native-collections`'
    /// interface `iterator()` native answer a COWAL receiver with the JDK's
    /// own `CopyOnWriteArrayList$COWIterator` over the same snapshot instead
    /// of an `ArrayList$Itr` wrapper
    /// (`i72-L1-interface-iterator-native-hands-a-cowal-an-arraylist-itr-20261105`).
    ///
    /// Interpreter round i3 wave 76, lane L1: GRADUATED over a real JDK. The
    /// stand-ins are registered on the synthetic library only and the
    /// iterator arm answers `COWIterator` whatever the row says, so turning
    /// the row off no longer changes a real-JDK `--compatible` VM (it prints
    /// one stderr note at registration;
    /// `i74-L1-proposal-retire-the-cowal-stand-ins-from-the-real-jdk-boot-20261107`).
    /// The row, its token and its variable stay one wave as that no-op, then
    /// go (freeing bit 61).
    JdkCopyOnWrite = 61,
    /// Over a real JDK, `java.util.concurrent.CopyOnWriteArraySet` runs the
    /// JDK's bodies with no native door in front of them, as under
    /// `--jdk-only`: none of the `HashSet` surface `native-collections`'
    /// `register_hashset_natives` mirrors onto the class (`SET_CLASSES`, 24
    /// `Bridge`s, three of them retired shadows) is registered. Row off, each
    /// of those natives asks `cow_set_route` whether the receiver is a real
    /// set and, for every real one, re-dispatches to the receiver's own
    /// bytecode, so the answers are already the JDK's; the row removes the
    /// native door and the route decision from every call. The fabricated
    /// set of the synthetic library keeps the map surface (the row needs a
    /// real JDK). A `COMPAT_FAMILIES` entry naming the class in `classes` (the
    /// three retired rows) with the other 21 in `bridges`. Registration-time.
    /// Default ON since the wave-74 triage, with `JdkCopyOnWrite` (the set's
    /// elements live in a `CopyOnWriteArrayList`).
    /// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-copy-on-write-set` turns it off.
    /// Interpreter round i3 wave 74, lane L1 (probe
    /// `L1W74CopyOnWriteSetBodies`, bench `L1W74CopyOnWriteSetBench`;
    /// `i72-L1-proposal-a-copyonwritearrayset-family-row-20261105`).
    JdkCopyOnWriteArraySet = 62,
    /// Over a real JDK, `java.util.concurrent.PriorityBlockingQueue` runs the
    /// JDK's bodies, as under `--jdk-only` (where its nine rows are retired
    /// shadows, `RETIRED_SHADOW_L5_TRIPLES`): `<init>()V`, `offer`, `put`,
    /// both `poll`s, `take`, `peek`, `size`, `isEmpty`. The stand-ins
    /// (`native-collections` `register_priority_blocking_queue_natives`, nine
    /// `Bridge`s on the real-JDK boot path) keep the queue as a SORTED array
    /// in the real `queue` / `size` fields, by natural order whatever the
    /// queue's `comparator`, and wait on the queue's own monitor in 50 ms
    /// slices; the JDK's other bodies (`toString`, `iterator`, `toArray`,
    /// `remove(Object)`, `drainTo`, the `Collection` constructor) read and
    /// write the same two fields as a binary HEAP. So with the stand-ins a
    /// `reverseOrder()` queue polls smallest first, a queue built from a
    /// collection or after a `remove(Object)` polls out of order, `toString`
    /// prints sorted where HotSpot prints heap order, and a `take()` waiter
    /// is not parked on the queue's `notEmpty` condition. A
    /// `COMPAT_FAMILIES` entry naming the class in `classes` (all nine are
    /// retired rows; no `bridges`, no `stub_classes`). Registration-time.
    /// Default ON since the wave-75 triage ([`Self::default_on`]).
    /// `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE=0` (its
    /// [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-priority-blocking-queue` turns it off.
    /// Interpreter round i3 wave 75, lane L1 (probe
    /// `L1W75PriorityBlockingQueueBodies`, bench
    /// `L1W75PriorityBlockingQueueBench`;
    /// `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`,
    /// candidate 3).
    ///
    /// Bit 63 was the last bit of the `u64` the set used to be; interpreter
    /// round i3 wave 76 (lane L1) widened the set to a `u128`
    /// (`i75-L1-proposal-widen-the-hotspot-alignment-set-past-64-rows-20261108`).
    JdkPriorityBlockingQueue = 63,
    /// Over a real JDK, `java.util.concurrent.ConcurrentHashMap` (and its
    /// views, iterators and entries) runs the JDK's bodies over the JDK's own
    /// `table`, as under `--jdk-only` (where its rows are retired shadows,
    /// `RETIRED_SHADOW_PHASE3_TRIPLES`). The stand-ins
    /// (`native-collections` `register_concurrent_hashmap_natives` and the
    /// view / iterator carrier registrars, `native-builtins`
    /// `register_m18_concurrent_fixes`' six) keep the entries in a VM-side
    /// SEGMENTED store hung off `AbstractMap.keySet` (slot 0) that the JDK's
    /// `table` never sees, so `toString` / iteration order, recursive-update
    /// detection, serialization and every reflective read of `table` /
    /// `sizeCtl` / `baseCount` differ from HotSpot. With the row: none of
    /// them is registered (a `COMPAT_FAMILIES` entry naming the class prefix
    /// takes the retired rows and stubs of any registrar; `native-collections`'
    /// CHM registrar and carrier loops skip all of theirs), the `java.util.Map`
    /// interface doors hand a CHM receiver to its own
    /// bytecode and read its `table` when they collect, the five
    /// `ConcurrentMap` interface rows do the same, and the JIT's direct
    /// `ConcurrentMap.get` helper declines. Registration-time, plus those
    /// door checks (a per-VM registry read on a CHM receiver only).
    /// Interpreter round i3 wave 76, lane L1 (probe
    /// `L1W76ConcurrentHashMapBodies`, bench `L1W76ConcurrentHashMapBench`;
    /// `i75-L1-proposal-a-concurrenthashmap-family-row-20261108`). The first
    /// row past bit 63 (the set is a `u128` since this wave).
    ///
    /// JIT round 21 (nio21 wave 1, lane L4) built the same row independently
    /// (same token); the two were merged here at the nio21 dev merge. What
    /// that side adds: `java.util.Properties` goes with the map (the
    /// `RETIRED_SHADOW_PHASE3_TRIPLES` pair: its retired rows and stubs are
    /// not registered either), and the system-properties arms follow the row
    /// since nio21 wave 2 (gates A-E of
    /// `r21-compatible-chm-row-needs-the-system-properties-arms`: `initPhase1`
    /// publishes `System.props` / `VM.savedProps`, `sysprops_merge` reads
    /// through, `setProperties` re-publishes), and the `native-collections`
    /// readers follow `chm_jdk_model` (the JDK table, in both modes).
    /// Default ON (both sides turned it on: the wave-76 triage, and nio21
    /// wave 4 lane L1). `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP=0` (its
    /// [`Self::legacy_key`]) or `CRATONVM_COMPAT_ALIGN=-jdk-concurrent-hash-map`
    /// turns it off.
    JdkConcurrentHashMap = 64,
    /// Over a real JDK, a child process is the JDK's own: `ProcessBuilder`
    /// runs its JDK 25 bodies (`start()` builds a `java.lang.ProcessImpl`
    /// through the `forkAndExec` / `create` bridges), as under `--jdk-only`.
    /// Without the row `--compatible`'s `ProcessBuilder.start()` stand-in
    /// (`native-io` `native_process_builder_start`) returns the VM's
    /// `cratonvm/synthetic/Process`, whose `waitFor`, streams and `onExit`
    /// are natives standing in for whole Java methods: a thread in
    /// `waitFor()` or reading the child's stdout lists no JDK frame, and
    /// `getClass()` / stack traces of the process and its streams differ from
    /// HotSpot (`i59-L6-compatible-synthetic-process-lists-no-jdk-frames`).
    /// With the row: every `SyntheticStub` on `java/lang/Process*` (the
    /// `ProcessBuilder` surface; `ProcessHandle*`, `ProcessEnvironment` and
    /// `ProcessImpl` excepted) and on the minted `cratonvm/synthetic/Process*`
    /// classes is not registered, nor are the 13 `java/lang/Process` rows that
    /// served only the minted receiver (`COMPAT_FAMILIES` `bridges`).
    /// Registration-time. Default ON since the wave-81 triage
    /// ([`Self::default_on`]; the 265-probe battery read one change, a fix).
    /// `CRATONVM_COMPAT_JDK_PROCESS=0` (its [`Self::legacy_key`]) or
    /// `CRATONVM_COMPAT_ALIGN=-jdk-process` turns it off. Interpreter round i3
    /// wave 80, lane L6 (probe `L6W59BlockedNativeStates`, rows `pipe` and
    /// `waitFor`; `L6W80CompatJdkProcess`).
    JdkProcess = 65,
    /// Over a real JDK, `java.util.concurrent.CompletableFuture` runs the
    /// JDK's bodies, as under `--jdk-only` (where its eleven rows are retired
    /// shadows, `RETIRED_SHADOW_L5_TRIPLES`): `complete`, `completeValue`,
    /// `completeExceptionally`, `isCompletedExceptionally`, `thenCombine`,
    /// `allOf`, `anyOf` and the four one-argument `*Async` stages. The
    /// stand-ins on the real-JDK boot path (`native-collections`
    /// `register_concurrent_completeness_natives`, `native-builtins`
    /// `register_essential_natives_with_shims`' `completeValue`) differ from
    /// HotSpot where they do not delegate: `completeValue` was a plain read
    /// then store of `result` (two racing completers could both answer
    /// `true`; a CAS since this wave, row or not), `thenCombine` throws
    /// its own `NullPointerException` texts, and `allOf()` / `anyOf()` of
    /// no element or of only synthetic futures answer from the VM's two-slot
    /// model. With the row none of them is registered (a `COMPAT_FAMILIES`
    /// entry naming the class in `classes` and `stub_classes`, plus the seven
    /// delegating stages `CRATONVM_CF_DELEGATING_YIELD=0` would register, in
    /// `bridges`). `complete` was also a measured fast path
    /// (`CRATONVM_NATIVE_CF_COMPLETE`, 2026-09-02); the row's bench prices it
    /// again. Registration-time.
    /// `CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=0` (its [`Self::legacy_key`])
    /// or `CRATONVM_COMPAT_ALIGN=-jdk-completable-future` turns it off.
    /// Interpreter round i3 wave 80, lane L1 (probe
    /// `L1W80CompletableFutureBodies`, bench `L1W80CompletableFutureBench`;
    /// `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`,
    /// candidate 4).
    ///
    /// JIT round 21 (nio21 wave 2, lane L4) built the same row independently
    /// (same token; the nio21 wave-1 census priced the stand-ins at nothing:
    /// `CRATONVM_NATIVE_CF_COMPLETE=0` moved no `Nio20HttpHot` row); merged
    /// here at the nio21 dev merge, which adds the class's `SyntheticStub`s to
    /// the family. Default ON (both sides turned it on: the wave-80 triage,
    /// and nio21 wave 2 lane L4).
    JdkCompletableFuture = 66,
    /// Over a real JDK, the four scalar atomics (`AtomicInteger`,
    /// `AtomicLong`, `AtomicReference`, `AtomicBoolean`) run the JDK's own
    /// bodies for their retired rows (`RETIRED_SHADOW_STATELESS_TRIPLES`: the
    /// constructors, `get` / `set` / `lazySet`, the CAS and the
    /// `getAndAdd` family), as under `--jdk-only`. The stand-ins hold no
    /// state of their own: they read and write the class's one `value`
    /// field, which is all the JDK bodies touch (through `Unsafe` /
    /// `VarHandle`), so the two can serve one object in turn. Every
    /// `jdk.internal.net.http` request crosses dozens of these (the
    /// `SequentialScheduler` / `SocketTube` / `Http1Exchange` state words).
    /// The non-retired registrations on the classes stay (`VMSupportsCS8`
    /// among them, a JDK `native`). Default ON since nio21 wave 3 (lane L1;
    /// the `Nio20HttpHot -Dnio20.rows=hc,conc` census showed it cost-neutral
    /// and conformant). JIT round 21 (nio21 wave 1, lane L4).
    JdkScalarAtomics = 67,
    /// Over a real JDK, `java.util.TreeMap` and `java.util.TreeSet` (with
    /// their `$` views, iterators and entries) run the JDK's own bodies, as
    /// under `--jdk-only` since 2026-09-12 (`RETIRED_SHADOW_L1_TM_TRIPLES`):
    /// every retired row and every `SyntheticStub` of the two class prefixes
    /// is not registered. The stand-ins kept a map's pairs in a Rust side
    /// table and called the comparator back through `invoke_virtual` per
    /// compare; every `HttpHeaders` read is a case-insensitive `TreeMap`
    /// lookup (`Nio20HttpHot tmap`: 75-114 us `--compatible` against 13 us
    /// `--jdk-only`). The VM's own readers already arbitrate the side table
    /// against the real `root` by the real `size` (`tm_collect_pairs`), which
    /// is the `--jdk-only` route. Registration-time. JIT round 21 (nio21 wave
    /// 2, lane L4).
    JdkTreeMap = 68,
    /// Over a real JDK, `java.util.LinkedList` and `java.util.ArrayDeque`
    /// (with their nested iterators) run the JDK's own bodies, as under
    /// `--jdk-only` (`RETIRED_SHADOW_L1_TRIPLES`; L1 wave 3's "authority"
    /// table: both families keep their state in the real fields). Every
    /// registration on the two prefixes is a retired row or a
    /// `SyntheticStub` (25/linux kind map: 48 + 37), so `--compatible` then
    /// registers none of them. Registration-time. JIT round 21 (nio21 wave 2,
    /// lane L4).
    JdkLinkedCollections = 69,
    /// Over a real JDK, `java.util.concurrent.TimeUnit`'s nine conversion
    /// rows (`RETIRED_SHADOW_L5_TRIPLES`: `toNanos` ... `toDays`, `convert`,
    /// `sleep`) run the JDK's own bodies, as under `--jdk-only`. The
    /// stand-ins were a Spring partial-boot workaround registered from
    /// `reflect_annotations.rs` and hold no state. Registration-time. JIT
    /// round 21 (nio21 wave 2, lane L4).
    JdkTimeUnit = 70,
    /// Over a real JDK, `java.util.concurrent.atomic.LongAdder` and
    /// `DoubleAdder` run the JDK's own `Striped64` bodies, as under
    /// `--jdk-only` (`RETIRED_SHADOW_STATELESS_TRIPLES`, 10 + 5 rows). The
    /// stand-ins kept contended adds in a Rust cell strip the JDK bodies do
    /// not see, so the WHOLE surface goes at once (every registration on the
    /// two classes is a retired row). Registration-time. JIT round 21 (nio21
    /// wave 2, lane L4).
    JdkAdders = 71,
    /// Over a real JDK, the carrier-thread-local accessors run the JDK's own
    /// per-thread bodies, as under `--jdk-only`: the eight `System$1` /
    /// `System$2` rows of `RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES` and the four
    /// `sun.nio.ch.Util` temporary-buffer rows of
    /// `RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES` (one decision, the table's
    /// own note) are not registered, and the `JavaLangAccess` interface
    /// bridges call the `ThreadLocal`'s own accessor. `--compatible` kept ONE
    /// value per `CarrierThreadLocal` for the whole VM, so
    /// `sun.nio.ch.IOVecWrapper` and `sun.nio.fs.NativeBuffers` (each a
    /// `TerminatingThreadLocal`) were shared by every thread
    /// (`UtilShareProbe`: 7,883 shared buffers and 8,014 NPEs over that map,
    /// 0 and 0 with the rows retired). A correctness row. Registration-time,
    /// plus the four bridges' gate. JIT round 21 (nio21 wave 2, lane L4).
    JdkCarrierThreadLocals = 72,
    /// Over a real JDK, `Thread.interrupt()`, `isInterrupted()` and the static
    /// `interrupted()` run the JDK's own bodies, as under `--jdk-only` since
    /// SBR wave 6 (`RETIRED_SHADOW_SBR_THREAD_TRIPLES`): the bodies read and
    /// write the `interrupted` FIELD and call the `interrupt0` /
    /// `clearInterruptEvent` bridges, which stay. The VM writes that field
    /// wherever it raises or consumes its own interrupt flag, in both modes
    /// (`vm_exec::set_thread_interrupted_field`: `thread_interrupt`, every
    /// consume site), so the field and the flag the blocking natives poll
    /// agree. Registration-time. JIT round 21 (nio21 wave 3, lane L1).
    JdkThreadInterrupt = 73,
    /// Over a real JDK, the `jdk.internal.misc.Unsafe` rows whose JDK body is
    /// ONE call of a bridge both modes keep (`weakCompareAndSet*` ->
    /// `compareAndSet*`, `compareAndExchange*Acquire/Release` ->
    /// `compareAndExchange*`, `get*Acquire/Opaque` -> `get*Volatile`,
    /// `put*Release/Opaque` -> `put*Volatile`, `loadFence` / `storeFence` ->
    /// `fullFence`, `storeStoreFence` -> `storeFence`; checked with `javap -c`
    /// on 25.0.3) run that body, as under `--jdk-only`. The off-heap
    /// accessors, the `getAndAdd` / `getAndSet` loops and the allocator stay
    /// (`retired_shadow::is_unsafe_delegation_row`). Registration-time. JIT
    /// round 21 (nio21 wave 3, lane L1).
    JdkUnsafeDelegations = 74,
    /// Over a real JDK, `java.net.URI` runs the JDK's own bodies
    /// (`RETIRED_SHADOW_SBR_URI_TRIPLES`, 30 rows), as under `--jdk-only`
    /// since SBR wave 1, and every VM producer of a `URI` (`make_uri`,
    /// `Path.toUri`, `File.toURI`, `URL.toURI`, `URI.create`, the HTTP
    /// request URIs) gives its object the JDK parser's fields
    /// (`net_phase_e::jdk_uri_model`), and the one reader outside that file,
    /// `nio_file::p57_uri_full_text`, follows the row (nio21 wave 4 L1).
    /// Default ON since nio21 wave 4 (lane L1). Registration-time plus the
    /// producer gate. JIT round 21 (nio21 wave 3, lane L1).
    JdkUri = 75,
    /// Over a real JDK, `java.io.ByteArrayOutputStream` runs the JDK's own
    /// bodies (`RETIRED_SHADOW_L4_TRIPLES`, 13 rows), as under `--jdk-only`.
    /// The stand-ins are layout code over `buf` / `count` since the
    /// `HttpURLConnection` body stream got its own carrier (nio21 wave 2,
    /// `CRATONVM_HUC_BODY_STREAM_CARRIER`); with that switch off the plain
    /// stream's hook would be gone, so a fixed-length upload is then sent
    /// buffered. Default ON since nio21 wave 4 (lane L1). Registration-time.
    /// JIT round 21 (nio21 wave 3, lane L1).
    JdkByteArrayStreams = 76,
    /// Over a real JDK, `jdk.internal.access.SharedSecrets.getJavaLangAccess()`
    /// runs the JDK's own body (`return javaLangAccess;`), and the
    /// `--compatible` `System.initPhase1` publishes that static (the VM's one
    /// `JavaLangAccess` carrier, before and after the body), as HotSpot's
    /// `setJavaLangAccess` does (`retired_shadow::is_shared_secrets_jla_getter_row`).
    /// The getter only: the `System$1` methods behind it are not this row's.
    /// Registration-time plus the `initPhase1` publish. JIT round 21 (nio21
    /// wave 4, lane L1).
    JdkSharedSecrets = 77,
    /// Over a real JDK, `java.lang.invoke` builds its own handles: the
    /// `RETIRED_SHADOW_*` rows on `java/lang/invoke/` (the `Lookup.find*` /
    /// `unreflect*` finders, the `MethodHandle` combinators, `MethodType`,
    /// `LambdaMetafactory`; the family `--jdk-only` retired as one unit,
    /// `RETIRED_SHADOW_SBR4_MHCONS_TRIPLES`) and the companion stubs there
    /// are not registered, so `findVarHandle` answers a
    /// `VarHandleInts$FieldInstanceReadWrite` and `findVirtual` a
    /// `DirectMethodHandle`, as under `--jdk-only`, instead of instances of
    /// the ABSTRACT `VarHandle` / `MethodHandle` (JVMS 6.5; the VM's
    /// uninstantiable-receiver census). The handles' own natives
    /// (`invokeExact`, the `VarHandle` access modes, `MethodHandleNatives`)
    /// stay, as they do under `--jdk-only`. The natives' `--jdk-only` arms
    /// (`lang_invoke`'s `is_jdk_only()` gates) follow the row. A call
    /// through a real handle was measured 7-60x a carrier's (SBR3 lane mh); the
    /// 2026-10-09 census found no cost, and the row is ON. Registration-time. JIT round 21 (nio21 wave 12,
    /// lane L3).
    ///
    /// The nio21 rows were renumbered 67..78 at the nio21 dev merge (they had
    /// been 63..75, beside the two rows both sides had built).
    JdkInvoke = 78,
    /// `--compatible` hands `jar:` and `jrt:` to the JDK's own providers
    /// (`jdk.nio.zipfs`, `jdk.internal.jrtfs`), as `--jdk-only` does: a mounted
    /// archive is a `ZipFileSystem` of `ZipPath`s, not an instance of the
    /// ABSTRACT `java.nio.file.FileSystem` this VM stamps (JVMS 6.5; the
    /// uninstantiable-receiver census). Registration-time: the archive-provider
    /// rows of `RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES` are dropped, plus the
    /// run-time gates that make the path natives yield to a `Path` this VM did
    /// not mint. JIT round 21 (nio21 close).
    JdkArchiveFs = 79,
    /// Over a real JDK, `java.util.concurrent.ThreadPoolExecutor` (and
    /// `ScheduledThreadPoolExecutor`) carry no registered native at all, as
    /// under `--jdk-only`, where the fifteen `ThreadPoolExecutor` rows and the
    /// two `ScheduledThreadPoolExecutor` rows are retired shadows
    /// (`RETIRED_SHADOW_L5_TRIPLES`) and `execute` is a stated
    /// `SyntheticStub`. Without the row `--compatible` registers them all
    /// (`native-builtins` `register_executor_natives`, `native-collections`
    /// `register_concurrent_completeness_natives`' stat methods), each a
    /// router that, for a real pool, re-dispatches to the JDK's own body
    /// (`tp_real_call`, `es_real_pool_door`, `executor_is_real`); dispatch
    /// already prefers that body (the class is on the real-protected-stub
    /// list), so the answer is the JDK's either way and what the row removes
    /// is a door, an arbitration and a router per call, plus the by-name
    /// force-native arm of the native-context call path. With the row: a
    /// `COMPAT_FAMILIES` entry naming both classes in `classes` and
    /// `stub_classes`. The `ExecutorService` interface rows stay (abstract
    /// methods: `--jdk-only` keeps them too). Registration-time. Default OFF
    /// (a new row starts off; the 2026 Elasticsearch / Gradle worker-leak
    /// history in `register_concurrent_natives` is why a real workload must
    /// hold first). `CRATONVM_COMPAT_JDK_THREAD_POOLS=1` (its
    /// [`Self::legacy_key`]) or `CRATONVM_COMPAT_ALIGN=+jdk-thread-pools`
    /// turns it on. Interpreter round i3 wave 82, lane L1 (probe
    /// `L1W82ThreadPoolsBodies`, bench `L1W82ThreadPoolsBench`;
    /// `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`,
    /// candidate 5, and the router-family shape of
    /// `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107`).
    JdkThreadPools = 80,
    /// `sun.misc.Unsafe`: the retired rows of the legacy facade run the JDK's
    /// bodies, each of which calls `beforeMemoryAccess()` (JEP 498), so the
    /// first legacy memory access prints HotSpot 25's four-line
    /// "terminally deprecated" warning on stderr, as it does under
    /// `--jdk-only`. The stand-ins skipped it. The bodies are one call of
    /// `jdk.internal.misc.Unsafe`, whose natives stay. Registration-time. JIT
    /// round 21 (nio21 wave 13, lane L4; `r21-plain-run-stderr-parity`).
    JdkSunMiscUnsafe = 81,
    /// `java.lang.foreign` / `jdk.internal.foreign`: the rows `--jdk-only` retires
    /// (`RETIRED_SHADOW_SBR*_FFM*` and `RETIRED_SHADOW_SBR5_AVH_TRIPLES`: the layout
    /// factories, `Arena` / `ArenaImpl`, the `MemorySegment` statics, the layout
    /// `varHandle` rows and the 26 `arrayElementVarHandle` rows) are not registered
    /// under `--compatible`, so segments, arenas and layout `VarHandle`s are the
    /// JDK's own real classes there too, not the VM's `MemorySegmentImpl` carrier
    /// and shape-table handle. Registration-time. Default ON: the 17-class FFM
    /// battery (`Sbr2W8FfmBase`, `Sbr3Ffm*`, `Sbr5Ffmlink*`, `RJdkForeign`, ...)
    /// answers as `--jdk-only` does line for line, where the carrier differed
    /// from HotSpot on 117 of `Sbr2W8FfmBase`'s rows. The layout `VarHandle`
    /// get+set costs 0.13x of the shape-table handle and `arrayElementVarHandle`
    /// 0.52x; direct `MemorySegment.get/set` 1.3x. `CRATONVM_COMPAT_ALIGN=
    /// -jdk-foreign` restores the carrier.
    JdkForeign = 82,
    /// `java.util.concurrent.atomic.AtomicIntegerArray` and `AtomicLongArray`
    /// run the JDK's own bodies over a real JDK, as under `--jdk-only`: the
    /// 26 + 26 `Bridge` stand-ins of `register_atomic_extras_natives` (the
    /// whole surface of both classes, `<init>` included) are not registered.
    /// The stand-ins kept the backing array in the one field the real classes
    /// have, so nothing but speed differs, and speed differed by 18x: with a
    /// registration in front of them the JDK bodies were never compiled
    /// (`promote-refuse door_registered_native`), and netty's `MpscIntQueue` --
    /// an `AtomicIntegerArray`, and the free list of every
    /// `AdaptivePoolingAllocator` size class -- cost 15 us per allocator cycle
    /// against 0.8 us in `--jdk-only`, which put
    /// `AdaptiveByteBufAllocatorGrowthTest` at 160-175 s against a 180 s cap.
    /// Registration-time. `CRATONVM_COMPAT_ALIGN=-jdk-atomic-arrays` restores
    /// the stand-ins. JIT work on the netty throughput page
    /// (`docs/feature-designs/jdk-only-mode.md`).
    JdkAtomicArrays = 83,
    /// `jdk.compiler`'s file manager: the `--compatible` Bridges over
    /// `JavacFileManager.list` / `inferBinaryName` /
    /// `checkNotModuleOrientedLocation`, `JavacFileManager$PathAndContainer
    /// .compareTo` and `RelativePath.hashCode` / `equals` / `compareTo` /
    /// `getPath` (`native-builtins` `test_frameworks.rs`, force-native in
    /// `native_override.rs`) are not registered, so in-process `javac` runs
    /// the JDK's own bodies, as under `--jdk-only` (where those Bridges never
    /// win over the bytecode). The stand-ins answered differently from
    /// HotSpot: `list` on a `SYSTEM_MODULES[...]` location minted
    /// `PathFileObject$SimpleFileObject`s through `getJavaFileForInput`
    /// (HotSpot: `DirectoryFileObject`s), `list(CLASS_PATH, "java...")`
    /// answered `[]` whatever the class path holds, `inferBinaryName`
    /// answered a name where the location has no paths (HotSpot: `null`),
    /// `RelativePath.compareTo` answered only the sign, and a compile through
    /// a `ForwardingJavaFileManager` at `-Xmx32m` died in `ClassFinder.fillIn`
    /// on a `null` binary name (`R20JimgSmallHeap --compatible`). JIT round 20
    /// wave 14, lane cjavac
    /// (`docs/feature-designs/jdk-only-mode.md`;
    /// probe `R20CjavacInferName`). Registration-time. Default ON (a
    /// correctness row); `CRATONVM_COMPAT_ALIGN=-jdk-javac-file-manager`
    /// restores the stand-ins.
    JdkJavacFileManager = 84,
    /// `java.lang.foreign` segments the VM mints for a POINTER that comes back
    /// from C: a downcall's `ADDRESS` return (the general and the memoised
    /// paths), an upcall's address argument, and `get(AddressLayout, long)`
    /// where a native answers it (`native-builtins` `panama.rs`
    /// `address_return_segment` / `pe_jdk_native_segment`, `panama_upcall.rs`
    /// `new_arg_segment`). With the row (and [`Self::JdkForeign`], whose real
    /// segment model these segments join) they are the JDK's own
    /// `NativeMemorySegmentImpl` in `MemorySessionImpl.GLOBAL_SESSION`, as JDK 25's
    /// `Utils.longToAddress` makes them and as `--jdk-only` has answered since
    /// round 19; without it they are the VM's `MemorySegmentImpl` carrier,
    /// whose class, `toString`, `scope()` identity
    /// (`ptr.scope().equals(Arena.global().scope())` was `false`) and
    /// funnelled `address()` (~230 ns a call, HotSpot 3 ns) differed from
    /// HotSpot. `JdkForeign` alone left them carriers: the minting natives are
    /// the linker's `Bridge`s, which that family does not touch. Counted where
    /// `--compatible` mints the JDK segment in place of the carrier.
    /// Call-time (read per mint). Default ON. JIT round 20 wave 19, lane
    /// yCompatSeg (MEM20-3 of `docs/feature-designs/jit-ir-optimizations.md`;
    /// probe `R20YCompatSegRows`). `CRATONVM_COMPAT_ALIGN=-jdk-foreign-segments`
    /// restores the carrier mints (and `-jdk-foreign` implies it).
    JdkForeignSegments = 85,
    /// The `invokedynamic` linkage checks HotSpot makes at link time (the
    /// bootstrap's static-argument and constant resolution, unloadable
    /// constant classes, the JDK-shape refusals; `ClassRealm::indy_link_checks`,
    /// interpreter round i1 wave 49, lane L4). It was its own variable,
    /// `CRATONVM_INDY_CHECKS_EVERY_MODE` (the legacy key). Default ON.
    /// Interpreter round i6 wave 118, lane L4 (`i50-L4-proposal-a-per-vm-table-of-hotspot-checks`).
    IndyLinkChecks = 86,
    /// JVMS §5.4.6 method selection checks (an `invokeinterface` receiver's
    /// `IncompatibleClassChangeError`, private / static / abstract selected
    /// methods; `ClassRealm::selection_checks`, wave 50, lane L4: the
    /// interpreter, the compile doors, the inliner and the compiled helpers).
    /// Legacy key `CRATONVM_SELECTION_CHECKS_EVERY_MODE`. Default ON. Wave 118.
    SelectionChecks = 87,
    /// The `MethodHandles` combinator argument checks HotSpot's bodies make
    /// (`ClassRealm::mh_combinator_checks`, wave 50, lane L4). Legacy key
    /// `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE`. Default ON. Wave 118.
    MhCombinatorChecks = 88,
    /// The access check of an `ldc` of a `CONSTANT_MethodHandle`
    /// (`ClassRealm::ldc_handle_access_checks`, wave 50, lane L4). Legacy key
    /// `CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE`. Default ON. Wave 118.
    LdcHandleAccessChecks = 89,
    /// `--compatible` fills a loader's `packages` map in
    /// `ClassLoader.getNamedPackage` as `--jdk-only` does (the JDK's
    /// `NamedPackage` objects, which `getDefinedPackage(s)` answer). Was the
    /// registration-time switch `CRATONVM_COMPAT_NAMED_PACKAGES` (interpreter
    /// round i1 wave 49, lane L6), still honoured as the legacy key. Default
    /// ON. Registration-time. Interpreter round i6 wave 118, lane L5
    /// (`i49-L6-proposal-one-hotspot-alignment-table-for-compatible-mode`).
    CompatNamedPackages = 90,
    /// `--compatible` hands out the JDK's own immutable collections
    /// (`ImmutableCollections.ListN` / `SetN` / `MapN`, ...) from
    /// `Stream.toList`, `List/Set/Map.copyOf` and the unmodifiable
    /// collectors instead of this VM's stand-ins. Was
    /// `CRATONVM_COMPAT_JDK_IMMUTABLES` (wave 49, lane L6), still the legacy
    /// key. Default ON. Registration-time (and one collector arm read per
    /// call). Interpreter round i6 wave 118, lane L5.
    CompatJdkImmutables = 91,
    /// "Does this class's module export its package to every module"
    /// (`Module.isExported(pn)`) is asked of the VM's module registry by the
    /// module's IDENTITY before the name-keyed answer (`Lookup.in` within a
    /// module, `Proxy`'s `nonExported`). Was
    /// `CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY` (wave 49, lane L6), still the
    /// legacy key. Default ON. Read per question (not counted). Interpreter
    /// round i6 wave 118, lane L5.
    ModuleExportAllByIdentity = 92,
    /// `--compatible` runs the JDK's own `java.util.Hashtable` (and its `$`
    /// views, entries, enumerators) as `--jdk-only` does: the family's retired
    /// rows (`RETIRED_SHADOW_L1_HT_TRIPLES`, 79) and its `SyntheticStub`s are
    /// not registered (`retired_shadow::COMPAT_FAMILIES`). The force-native
    /// gates that name `Hashtable` (`native_override.rs`
    /// `core_map_family_forced_member` and the view-carrier lists,
    /// `dispatch_virtual.rs`, `vm_exec.rs`'s `check_override`) force only where
    /// a registration exists, so they fall to the JDK bodies with the row on.
    /// `Properties` (a `Hashtable` subclass) is the `JdkConcurrentHashMap`
    /// row's: keep that row on with this one, or a VM-layout `Properties`
    /// would inherit the JDK's `Hashtable` bodies. Default ON since
    /// interpreter round i7 wave 144 (A/B w142lto; built in round i6 wave 119,
    /// lane L5, `i52-L5-proposal-a-compatible-row-per-retired-shadow-family`);
    /// `CRATONVM_COMPAT_JDK_HASHTABLE=0` or `CRATONVM_COMPAT_ALIGN=-jdk-hashtable`
    /// turns it off.
    /// Registration-time.
    JdkHashtable = 93,
    /// `--compatible` runs the JDK's own `java.util.logging` (`Logger`,
    /// `LogManager`, `Level`, `Handler` / `FileHandler`, `LogRecord`) as
    /// `--jdk-only` does: the family's retired rows (`RETIRED_SHADOW_TRIPLES`'
    /// logging half, 88 triples, retired for `--jdk-only` on 2026-08-11 and
    /// 2026-08-12) and its `SyntheticStub`s are not registered
    /// (`retired_shadow::COMPAT_FAMILIES`). `LoggingPermission` is excepted:
    /// it is one of the five permission classes one closure registers over
    /// (`native_override.rs`' permission list), and dropping its rows alone
    /// would mix a JDK-built permission with `BasicPermission`'s natives. The
    /// force gates that name the family (`Level.parse`, `FileHandler`'s
    /// lifecycle) force only where a registration exists. Default OFF
    /// (interpreter round i6 wave 120, lane L5,
    /// `i52-L5-proposal-a-compatible-row-per-retired-shadow-family`;
    /// `CRATONVM_COMPAT_JDK_LOGGING=1` or `CRATONVM_COMPAT_ALIGN=+jdk-logging`).
    /// Registration-time.
    JdkLogging = 94,
    /// `--compatible` runs the JDK's own `String.getBytes(Charset)` and
    /// `String.getBytes(String)` bodies, as `--jdk-only` does: their two
    /// `Bridge`s (an exact transcoding front that declines to the JDK's body)
    /// are not registered (`retired_shadow::COMPAT_FAMILIES`, `bridges`). The
    /// router census's first base-class JDK class (interpreter round i6 wave
    /// 122, lane L1; step 2 of
    /// `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`).
    /// `String`'s reviewed `Intrinsic`s (`split`, the regex trio,
    /// `valueOf(Object)`) are not in it. Default OFF
    /// (`CRATONVM_COMPAT_JDK_STRING_GET_BYTES=1` or
    /// `CRATONVM_COMPAT_ALIGN=+jdk-string-get-bytes`). Registration-time.
    JdkStringGetBytes = 95,
    /// `--compatible` runs the JDK's own `java.lang.management.MemoryUsage`
    /// bodies, as `--jdk-only` does: the class's retired rows (the four `()J`
    /// getters and `<init>(JJJJ)V`, which routes to the JDK's constructor
    /// whenever the class has bytes) and its `toString` shim `Bridge` are not
    /// registered (`retired_shadow::COMPAT_FAMILIES`). The VM's own usages
    /// write the four fields by name, so the getters read what the natives
    /// answered. The router census's next base-class JDK class needing a row
    /// after `String` (interpreter round i6 wave 123, lane L1; step 2 of
    /// `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`).
    /// Default OFF (`CRATONVM_COMPAT_JDK_MEMORY_USAGE=1` or
    /// `CRATONVM_COMPAT_ALIGN=+jdk-memory-usage`). Registration-time.
    JdkMemoryUsage = 96,
    /// `--compatible` runs the JDK's own bodies of the three
    /// `sun.management` memory beans, `GarbageCollectorImpl`,
    /// `MemoryManagerImpl` and `MemoryPoolImpl`, as `--jdk-only` does: their
    /// retired rows (each class's `<init>(String..)`, which routes to the
    /// JDK's constructor whenever the class has bytes, and the `getName`,
    /// `isValid` and `getType` field readers) are not registered
    /// (`retired_shadow::COMPAT_FAMILIES`). The VM's own beans write those
    /// fields by name, so the JDK's readers answer what the natives did. The
    /// classes' `ACC_NATIVE` methods (`getUsage0`, `getMemoryPools0`,
    /// `getCollectionCount`, ...) are genuine natives and stay. The router
    /// census's next base-class JDK classes after `MemoryUsage` (interpreter
    /// round i6 wave 124, lane L1; step 2 of
    /// `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`).
    /// Default OFF (`CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS=1` or
    /// `CRATONVM_COMPAT_ALIGN=+jdk-management-beans`). Registration-time.
    JdkManagementBeans = 97,
    /// `--compatible` runs the JDK's own bodies of
    /// `sun.reflect.generics.reflectiveObjects.TypeVariableImpl`, as
    /// `--jdk-only` does: its eleven retired rows (`getName`, `getBounds`,
    /// `toString`, `getTypeName`, `getAnnotatedBounds` and the six
    /// annotation getters; `RETIRED_SHADOW_SBR_REFLECT_TRIPLES`,
    /// `RETIRED_SHADOW_REFLECT_GENERICS_DELEGATE_TRIPLES`,
    /// `RETIRED_SHADOW_SBR_REFLMETA_TRIPLES`) are not registered
    /// (`retired_shadow::COMPAT_FAMILIES`). Seven of them already routed a
    /// JDK-built receiver to the JDK's body; the other four read the fields
    /// the JDK's bodies read. The VM's own type-variable carrier (an instance
    /// of the `java.lang.reflect.TypeVariable` interface) keeps its natives.
    /// The router census's next base-class JDK class after the
    /// `sun.management` beans (interpreter round i6 wave 125, lane L1; step 2
    /// of `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`).
    /// Default OFF (`CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL=1` or
    /// `CRATONVM_COMPAT_ALIGN=+jdk-type-variable-impl`). Registration-time.
    JdkTypeVariableImpl = 98,
    /// `--compatible` runs the JDK's own bodies of `java.util.PriorityQueue`
    /// and its `$Itr`, as `--jdk-only` does: the nineteen retired rows of
    /// `RETIRED_SHADOW_L1_TRIPLES` on those two classes (the four
    /// constructors, `add` / `offer` / `peek` / `poll` / `remove` / `contains`
    /// / `clear` / `size` / `isEmpty` / `toArray` / `iterator` / `toString`,
    /// and `$Itr.hasNext` / `next` / `remove`) are not registered
    /// (`retired_shadow::COMPAT_FAMILIES`). The stand-ins keep the real
    /// `queue` / `size` / `comparator` fields but never write `modCount`, so
    /// the JDK's fail-fast iterator cannot see their changes; the row hands
    /// the whole class to the JDK. Interpreter round i6 wave 133, lane L6
    /// (`i52-L5-proposal-a-compatible-row-per-retired-shadow-family`, the
    /// wave-121 census's next candidate). Default OFF
    /// (`CRATONVM_COMPAT_JDK_PRIORITY_QUEUE=1` or
    /// `CRATONVM_COMPAT_ALIGN=+jdk-priority-queue`). Registration-time.
    JdkPriorityQueue = 99,
    /// Over a real JDK, `jdk.internal.misc.VM.initLevel()` and `isBooted()`
    /// run the JDK's own bodies (`getstatic initLevel`, compared with
    /// `SYSTEM_BOOTED` for `isBooted`), as under `--jdk-only` and on HotSpot:
    /// their rows of `RETIRED_SHADOW_SBR_SYSTEM_TRIPLES` /
    /// `RETIRED_SHADOW_L5_TRIPLES` are not registered
    /// (`retired_shadow::is_vm_init_level_row`). The stand-ins answered from
    /// the PROCESS-wide boot level (`initLevel` with a floor of 2, `isBooted`
    /// a constant `true`), not the per-VM static the JDK's `initLevel(int)`,
    /// `isModuleSystemInited()` and the boot's level writes use. `isBooted`
    /// rides the row only while the fast boot raises the static to 4
    /// (`CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED`, default ON). The other retired
    /// `VM` getters stay native (the review on the predicate). Default ON;
    /// `CRATONVM_COMPAT_ALIGN=-jdk-vm-init-level` reverts it. Registration-time.
    /// JIT round j27 wave 5, lane VMALIGN.
    JdkVmInitLevel = 100,
}

/// How many rows the set can hold: the width of the `u128` behind
/// [`HotSpotAlignments`]. (Interpreter round i3 wave 76, lane L1: it was a
/// `u64`, full at 64 rows with `JdkPriorityBlockingQueue`.)
pub const ALIGNMENT_SET_CAPACITY: usize = u128::BITS as usize;

// The set is a `u128` (`HotSpotAlignments`) and the census has one counter
// per row (`AlignmentCensus`, sized by `ALL.len()`): a 129th row must widen
// the set again (or graduate a row and free its bit) before it lands, and
// fails to compile here rather than wrapping its shift.
const _: () = assert!(HotSpotAlignment::ALL.len() <= ALIGNMENT_SET_CAPACITY);

impl HotSpotAlignment {
    /// Every row, in bit order.
    pub const ALL: [HotSpotAlignment; 101] = [
        HotSpotAlignment::ParallelLoaders,
        HotSpotAlignment::InitiatingRecords,
        HotSpotAlignment::LoaderConstraints,
        HotSpotAlignment::LoaderThrows,
        HotSpotAlignment::GlobalNameAsk,
        HotSpotAlignment::UrlAppHandler,
        HotSpotAlignment::ModuleMessages,
        HotSpotAlignment::CatchTypesAtLink,
        HotSpotAlignment::SupertypeShapeChecks,
        HotSpotAlignment::UserLoaderUnnamedModule,
        HotSpotAlignment::FindLoadedNameCheck,
        HotSpotAlignment::ComponentTypeFromArrayClass,
        HotSpotAlignment::NamespacedArrayTypes,
        HotSpotAlignment::RuntimeImageNames,
        HotSpotAlignment::StubPrepassOneAsk,
        HotSpotAlignment::IsolatedStaticOwner,
        HotSpotAlignment::JdkUnmodifiableViews,
        HotSpotAlignment::NamespacedClassTypes,
        HotSpotAlignment::NamespacedArrayStores,
        HotSpotAlignment::JdkUrlClassLoaders,
        HotSpotAlignment::UnflattenedAppClassPath,
        HotSpotAlignment::RecordBehindMemo,
        HotSpotAlignment::JdkFileBodies,
        HotSpotAlignment::JdkJarUrlConnections,
        HotSpotAlignment::JdkJarFileBodies,
        HotSpotAlignment::JdkForkJoinPool,
        HotSpotAlignment::JdkProviders,
        HotSpotAlignment::JdkModuleGraph,
        HotSpotAlignment::JdkHashCollections,
        HotSpotAlignment::JdkStringBuilderBodies,
        HotSpotAlignment::JdkArraysBodies,
        HotSpotAlignment::JdkJava2d,
        HotSpotAlignment::JdkStackWalker,
        HotSpotAlignment::JdkSignal,
        HotSpotAlignment::JdkRuntime,
        HotSpotAlignment::JdkStreams,
        HotSpotAlignment::JdkModuleAccess,
        HotSpotAlignment::JdkJsse,
        HotSpotAlignment::JdkPipe,
        HotSpotAlignment::JdkNioBuffers,
        HotSpotAlignment::JdkPreconditions,
        HotSpotAlignment::VerifierLoadsAtLink,
        HotSpotAlignment::JdkLockSupport,
        HotSpotAlignment::JdkStampedLock,
        HotSpotAlignment::JdkReferences,
        HotSpotAlignment::JdkObjectBodies,
        HotSpotAlignment::JdkUnsafeBulkBodies,
        HotSpotAlignment::JdkOptional,
        HotSpotAlignment::JdkNioChannels,
        HotSpotAlignment::JdkArrayList,
        HotSpotAlignment::JdkLocale,
        HotSpotAlignment::JdkDate,
        HotSpotAlignment::JdkObjectIdentityBodies,
        HotSpotAlignment::JdkAsyncChannels,
        HotSpotAlignment::JdkHttpClient,
        HotSpotAlignment::JdkLocaleProviders,
        HotSpotAlignment::JdkHttpServer,
        HotSpotAlignment::JdkPrintWriter,
        HotSpotAlignment::JavaNameAsk,
        HotSpotAlignment::JdkCountDownLatch,
        HotSpotAlignment::JdkAqls,
        HotSpotAlignment::JdkCopyOnWrite,
        HotSpotAlignment::JdkCopyOnWriteArraySet,
        HotSpotAlignment::JdkPriorityBlockingQueue,
        HotSpotAlignment::JdkConcurrentHashMap,
        HotSpotAlignment::JdkProcess,
        HotSpotAlignment::JdkCompletableFuture,
        HotSpotAlignment::JdkScalarAtomics,
        HotSpotAlignment::JdkTreeMap,
        HotSpotAlignment::JdkLinkedCollections,
        HotSpotAlignment::JdkTimeUnit,
        HotSpotAlignment::JdkAdders,
        HotSpotAlignment::JdkCarrierThreadLocals,
        HotSpotAlignment::JdkThreadInterrupt,
        HotSpotAlignment::JdkUnsafeDelegations,
        HotSpotAlignment::JdkUri,
        HotSpotAlignment::JdkByteArrayStreams,
        HotSpotAlignment::JdkSharedSecrets,
        HotSpotAlignment::JdkInvoke,
        HotSpotAlignment::JdkArchiveFs,
        HotSpotAlignment::JdkThreadPools,
        HotSpotAlignment::JdkSunMiscUnsafe,
        HotSpotAlignment::JdkForeign,
        HotSpotAlignment::JdkAtomicArrays,
        HotSpotAlignment::JdkJavacFileManager,
        HotSpotAlignment::JdkForeignSegments,
        HotSpotAlignment::IndyLinkChecks,
        HotSpotAlignment::SelectionChecks,
        HotSpotAlignment::MhCombinatorChecks,
        HotSpotAlignment::LdcHandleAccessChecks,
        HotSpotAlignment::CompatNamedPackages,
        HotSpotAlignment::CompatJdkImmutables,
        HotSpotAlignment::ModuleExportAllByIdentity,
        HotSpotAlignment::JdkHashtable,
        HotSpotAlignment::JdkLogging,
        HotSpotAlignment::JdkStringGetBytes,
        HotSpotAlignment::JdkMemoryUsage,
        HotSpotAlignment::JdkManagementBeans,
        HotSpotAlignment::JdkTypeVariableImpl,
        HotSpotAlignment::JdkPriorityQueue,
        HotSpotAlignment::JdkVmInitLevel,
    ];

    /// The kebab-case name `CRATONVM_COMPAT_ALIGN` items use.
    pub const fn token(self) -> &'static str {
        match self {
            HotSpotAlignment::ParallelLoaders => "parallel-loaders",
            HotSpotAlignment::InitiatingRecords => "initiating-records",
            HotSpotAlignment::LoaderConstraints => "loader-constraints",
            HotSpotAlignment::LoaderThrows => "loader-throws",
            HotSpotAlignment::GlobalNameAsk => "global-name-ask",
            HotSpotAlignment::UrlAppHandler => "url-app-handler",
            HotSpotAlignment::ModuleMessages => "module-messages",
            HotSpotAlignment::CatchTypesAtLink => "catch-types-at-link",
            HotSpotAlignment::SupertypeShapeChecks => "supertype-shape-checks",
            HotSpotAlignment::UserLoaderUnnamedModule => "user-loader-unnamed-module",
            HotSpotAlignment::FindLoadedNameCheck => "find-loaded-name-check",
            HotSpotAlignment::ComponentTypeFromArrayClass => "component-type-from-array-class",
            HotSpotAlignment::NamespacedArrayTypes => "namespaced-array-types",
            HotSpotAlignment::RuntimeImageNames => "runtime-image-names",
            HotSpotAlignment::StubPrepassOneAsk => "stub-prepass-one-ask",
            HotSpotAlignment::IsolatedStaticOwner => "isolated-static-owner",
            HotSpotAlignment::JdkUnmodifiableViews => "jdk-unmodifiable-views",
            HotSpotAlignment::NamespacedClassTypes => "namespaced-class-types",
            HotSpotAlignment::NamespacedArrayStores => "namespaced-array-stores",
            HotSpotAlignment::JdkUrlClassLoaders => "jdk-url-class-loaders",
            HotSpotAlignment::UnflattenedAppClassPath => "unflattened-app-class-path",
            HotSpotAlignment::RecordBehindMemo => "record-behind-memo",
            HotSpotAlignment::JdkFileBodies => "jdk-file-bodies",
            HotSpotAlignment::JdkJarUrlConnections => "jdk-jar-url-connections",
            HotSpotAlignment::JdkJarFileBodies => "jdk-jar-file-bodies",
            HotSpotAlignment::JdkForkJoinPool => "jdk-fork-join-pool",
            HotSpotAlignment::JdkProviders => "jdk-providers",
            HotSpotAlignment::JdkModuleGraph => "jdk-module-graph",
            HotSpotAlignment::JdkHashCollections => "jdk-hash-collections",
            HotSpotAlignment::JdkStringBuilderBodies => "jdk-string-builder-bodies",
            HotSpotAlignment::JdkArraysBodies => "jdk-arrays-bodies",
            HotSpotAlignment::JdkJava2d => "jdk-java2d",
            HotSpotAlignment::JdkStackWalker => "jdk-stack-walker",
            HotSpotAlignment::JdkSignal => "jdk-signal",
            HotSpotAlignment::JdkRuntime => "jdk-runtime",
            HotSpotAlignment::JdkStreams => "jdk-streams",
            HotSpotAlignment::JdkModuleAccess => "jdk-module-access",
            HotSpotAlignment::JdkJsse => "jdk-jsse",
            HotSpotAlignment::JdkPipe => "jdk-pipe",
            HotSpotAlignment::JdkNioBuffers => "jdk-nio-buffers",
            HotSpotAlignment::JdkPreconditions => "jdk-preconditions",
            HotSpotAlignment::VerifierLoadsAtLink => "verifier-loads-at-link",
            HotSpotAlignment::JdkLockSupport => "jdk-lock-support",
            HotSpotAlignment::JdkStampedLock => "jdk-stamped-lock",
            HotSpotAlignment::JdkReferences => "jdk-references",
            HotSpotAlignment::JdkObjectBodies => "jdk-object-bodies",
            HotSpotAlignment::JdkUnsafeBulkBodies => "jdk-unsafe-bulk-bodies",
            HotSpotAlignment::JdkOptional => "jdk-optional",
            HotSpotAlignment::JdkNioChannels => "jdk-nio-channels",
            HotSpotAlignment::JdkArrayList => "jdk-array-list",
            HotSpotAlignment::JdkLocale => "jdk-locale",
            HotSpotAlignment::JdkDate => "jdk-date",
            HotSpotAlignment::JdkObjectIdentityBodies => "jdk-object-identity-bodies",
            HotSpotAlignment::JdkAsyncChannels => "jdk-async-channels",
            HotSpotAlignment::JdkHttpClient => "jdk-http-client",
            HotSpotAlignment::JdkLocaleProviders => "jdk-locale-providers",
            HotSpotAlignment::JdkHttpServer => "jdk-http-server",
            HotSpotAlignment::JdkPrintWriter => "jdk-print-writer",
            HotSpotAlignment::JavaNameAsk => "java-name-ask",
            HotSpotAlignment::JdkCountDownLatch => "jdk-count-down-latch",
            HotSpotAlignment::JdkAqls => "jdk-aqls",
            HotSpotAlignment::JdkCopyOnWrite => "jdk-copy-on-write",
            HotSpotAlignment::JdkCopyOnWriteArraySet => "jdk-copy-on-write-set",
            HotSpotAlignment::JdkPriorityBlockingQueue => "jdk-priority-blocking-queue",
            HotSpotAlignment::JdkConcurrentHashMap => "jdk-concurrent-hash-map",
            HotSpotAlignment::JdkProcess => "jdk-process",
            HotSpotAlignment::JdkCompletableFuture => "jdk-completable-future",
            HotSpotAlignment::JdkScalarAtomics => "jdk-scalar-atomics",
            HotSpotAlignment::JdkTreeMap => "jdk-tree-map",
            HotSpotAlignment::JdkLinkedCollections => "jdk-linked-collections",
            HotSpotAlignment::JdkTimeUnit => "jdk-time-unit",
            HotSpotAlignment::JdkAdders => "jdk-adders",
            HotSpotAlignment::JdkCarrierThreadLocals => "jdk-carrier-thread-locals",
            HotSpotAlignment::JdkThreadInterrupt => "jdk-thread-interrupt",
            HotSpotAlignment::JdkUnsafeDelegations => "jdk-unsafe-delegations",
            HotSpotAlignment::JdkUri => "jdk-uri",
            HotSpotAlignment::JdkByteArrayStreams => "jdk-byte-array-streams",
            HotSpotAlignment::JdkSharedSecrets => "jdk-shared-secrets",
            HotSpotAlignment::JdkInvoke => "jdk-invoke",
            HotSpotAlignment::JdkArchiveFs => "jdk-archive-fs",
            HotSpotAlignment::JdkThreadPools => "jdk-thread-pools",
            HotSpotAlignment::JdkSunMiscUnsafe => "jdk-sun-misc-unsafe",
            HotSpotAlignment::JdkForeign => "jdk-foreign",
            HotSpotAlignment::JdkAtomicArrays => "jdk-atomic-arrays",
            HotSpotAlignment::JdkJavacFileManager => "jdk-javac-file-manager",
            HotSpotAlignment::JdkForeignSegments => "jdk-foreign-segments",
            HotSpotAlignment::IndyLinkChecks => "indy-link-checks",
            HotSpotAlignment::SelectionChecks => "selection-checks",
            HotSpotAlignment::MhCombinatorChecks => "mh-combinator-checks",
            HotSpotAlignment::LdcHandleAccessChecks => "ldc-handle-access-checks",
            HotSpotAlignment::CompatNamedPackages => "named-packages",
            HotSpotAlignment::CompatJdkImmutables => "jdk-immutables",
            HotSpotAlignment::ModuleExportAllByIdentity => "module-export-all-by-identity",
            HotSpotAlignment::JdkHashtable => "jdk-hashtable",
            HotSpotAlignment::JdkLogging => "jdk-logging",
            HotSpotAlignment::JdkStringGetBytes => "jdk-string-get-bytes",
            HotSpotAlignment::JdkMemoryUsage => "jdk-memory-usage",
            HotSpotAlignment::JdkManagementBeans => "jdk-management-beans",
            HotSpotAlignment::JdkTypeVariableImpl => "jdk-type-variable-impl",
            HotSpotAlignment::JdkPriorityQueue => "jdk-priority-queue",
            HotSpotAlignment::JdkVmInitLevel => "jdk-vm-init-level",
        }
    }

    /// The row's own variable from before this table, still honoured.
    pub const fn legacy_key(self) -> Option<&'static str> {
        match self {
            HotSpotAlignment::ParallelLoaders => Some("CRATONVM_COMPAT_PARALLEL_LOADERS"),
            HotSpotAlignment::InitiatingRecords => Some("CRATONVM_COMPAT_INITIATING_RECORDS"),
            HotSpotAlignment::LoaderConstraints => Some("CRATONVM_COMPAT_LOADER_CONSTRAINTS"),
            HotSpotAlignment::LoaderThrows => Some("CRATONVM_COMPAT_LOADER_THROWS"),
            HotSpotAlignment::GlobalNameAsk => Some("CRATONVM_COMPAT_GLOBAL_NAME_ASK"),
            HotSpotAlignment::UrlAppHandler => Some("CRATONVM_COMPAT_URL_APP_HANDLER"),
            HotSpotAlignment::ModuleMessages => Some("CRATONVM_COMPAT_MODULE_MESSAGES"),
            // Not from before the table: the row's own `=0` kill switch, which
            // interpreter round i2 wave 69 (lane L1) gives a row that is on by
            // default from its first commit (a thread-state correctness fix).
            HotSpotAlignment::JdkLockSupport => Some("CRATONVM_COMPAT_JDK_LOCK_SUPPORT"),
            // The same shape, interpreter round i2 wave 70 (lane L1).
            HotSpotAlignment::JdkStampedLock => Some("CRATONVM_COMPAT_JDK_STAMPED_LOCK"),
            // The same shape, interpreter round i3 wave 71 (lane L1).
            HotSpotAlignment::JdkCountDownLatch => Some("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH"),
            // The same shape, interpreter round i3 wave 72 (lane L1).
            HotSpotAlignment::JdkAqls => Some("CRATONVM_COMPAT_JDK_AQLS"),
            HotSpotAlignment::JdkCopyOnWrite => Some("CRATONVM_COMPAT_JDK_COPY_ON_WRITE"),
            // The same shape, interpreter round i3 wave 74 (lane L1).
            HotSpotAlignment::JdkCopyOnWriteArraySet => {
                Some("CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET")
            }
            // The same shape, interpreter round i3 wave 75 (lane L1).
            HotSpotAlignment::JdkPriorityBlockingQueue => {
                Some("CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE")
            }
            // The same shape, interpreter round i3 wave 76 (lane L1).
            HotSpotAlignment::JdkConcurrentHashMap => {
                Some("CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP")
            }
            // The same shape, default OFF (interpreter round i3 wave 80, lane L6).
            HotSpotAlignment::JdkProcess => Some("CRATONVM_COMPAT_JDK_PROCESS"),
            // The same shape, interpreter round i3 wave 80 (lane L1).
            HotSpotAlignment::JdkCompletableFuture => {
                Some("CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE")
            }
            // The same shape, default OFF (interpreter round i3 wave 82, lane L1).
            HotSpotAlignment::JdkThreadPools => Some("CRATONVM_COMPAT_JDK_THREAD_POOLS"),
            // Interpreter round i6 wave 118, lane L4: the four L4 check
            // families' own variables from before they became rows.
            HotSpotAlignment::IndyLinkChecks => Some("CRATONVM_INDY_CHECKS_EVERY_MODE"),
            HotSpotAlignment::SelectionChecks => Some("CRATONVM_SELECTION_CHECKS_EVERY_MODE"),
            HotSpotAlignment::MhCombinatorChecks => {
                Some("CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE")
            }
            HotSpotAlignment::LdcHandleAccessChecks => {
                Some("CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE")
            }
            // Interpreter round i6 wave 118, lane L5: the three wave-49
            // registration-time switches' own variables.
            HotSpotAlignment::CompatNamedPackages => Some("CRATONVM_COMPAT_NAMED_PACKAGES"),
            HotSpotAlignment::CompatJdkImmutables => Some("CRATONVM_COMPAT_JDK_IMMUTABLES"),
            HotSpotAlignment::ModuleExportAllByIdentity => {
                Some("CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY")
            }
            // The family rows' shape, default OFF (interpreter round i6 wave
            // 119, lane L5).
            HotSpotAlignment::JdkHashtable => Some("CRATONVM_COMPAT_JDK_HASHTABLE"),
            HotSpotAlignment::JdkLogging => Some("CRATONVM_COMPAT_JDK_LOGGING"),
            HotSpotAlignment::JdkStringGetBytes => Some("CRATONVM_COMPAT_JDK_STRING_GET_BYTES"),
            HotSpotAlignment::JdkMemoryUsage => Some("CRATONVM_COMPAT_JDK_MEMORY_USAGE"),
            HotSpotAlignment::JdkManagementBeans => Some("CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS"),
            HotSpotAlignment::JdkTypeVariableImpl => Some("CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL"),
            HotSpotAlignment::JdkPriorityQueue => Some("CRATONVM_COMPAT_JDK_PRIORITY_QUEUE"),
            _ => None,
        }
    }

    /// Is the row on in `--compatible` when nothing says otherwise? The rows
    /// that held through a host census are; a row added this wave starts off
    /// and is turned on by its lane's last commit (so that one commit is the
    /// revert).
    pub const fn default_on(self) -> bool {
        match self {
            HotSpotAlignment::ParallelLoaders
            | HotSpotAlignment::InitiatingRecords
            | HotSpotAlignment::LoaderConstraints
            | HotSpotAlignment::LoaderThrows
            | HotSpotAlignment::GlobalNameAsk
            | HotSpotAlignment::UrlAppHandler
            | HotSpotAlignment::ModuleMessages
            // Wave 50, lane L5: on from the lane's last commit.
            | HotSpotAlignment::CatchTypesAtLink
            | HotSpotAlignment::SupertypeShapeChecks
            | HotSpotAlignment::UserLoaderUnnamedModule
            | HotSpotAlignment::FindLoadedNameCheck
            | HotSpotAlignment::ComponentTypeFromArrayClass
            | HotSpotAlignment::NamespacedArrayTypes
            | HotSpotAlignment::RuntimeImageNames
            // Wave 51, lane L5: on from the lane's switch-on commits, one per
            // row.
            | HotSpotAlignment::StubPrepassOneAsk
            | HotSpotAlignment::IsolatedStaticOwner
            | HotSpotAlignment::JdkUnmodifiableViews
            // Wave 52, lane L5: on from the lane's last commit (that commit
            // is the revert; each row also reverts alone through
            // `CRATONVM_COMPAT_ALIGN=-<token>`).
            | HotSpotAlignment::JdkUrlClassLoaders
            | HotSpotAlignment::RecordBehindMemo
            // Wave 52, lane L4: on from the lane's last commit (that one
            // commit is the revert). Resolution is loader-faithful in both
            // modes (`CRATONVM_LOADER_AWARE_RESOLUTION`, default on, and the
            // JIT's `class_resolved_without_loading`), so a fork's checks name
            // the fork's own classes; probe `L4W52ForkedLoaderTypes` (a
            // child-first fork's casts, stores, annotations, proxies and
            // lambdas) matches HotSpot with both rows on, and the census
            // counts every answer the rows change.
            | HotSpotAlignment::NamespacedClassTypes
            | HotSpotAlignment::NamespacedArrayStores
            // Wave 53, lane L5: on from the lane's switch-on commit (that
            // commit is the revert; `CRATONVM_COMPAT_ALIGN=-jdk-file-bodies`
            // on a host).
            | HotSpotAlignment::JdkFileBodies
            // Wave 57, lane L5: on from the lane's switch-on commit (that
            // commit is the revert; `CRATONVM_COMPAT_ALIGN=
            // -jdk-jar-url-connections` on a host).
            | HotSpotAlignment::JdkJarUrlConnections
            // Wave 58, lane L5: on from the lane's switch-on commit (that
            // commit is the revert; `CRATONVM_COMPAT_ALIGN=
            // -jdk-jar-file-bodies` on a host).
            | HotSpotAlignment::JdkJarFileBodies
            // SBR round 4, lane compat: on from the lane's switch-on commit
            // (that commit is the revert; `CRATONVM_COMPAT_ALIGN=-<token>` on a
            // host). Each row is in force only over a real JDK
            // (`needs_real_jdk`). Probes `Sbr4CompatFamilies` / `Sbr4CompatModule`
            // / `Sbr3LatentProbe`: `--compatible` equals HotSpot on every line
            // with the rows on (one held line), where it differed on 15-56.
            | HotSpotAlignment::JdkForkJoinPool
            | HotSpotAlignment::JdkProviders
            | HotSpotAlignment::JdkModuleGraph
            // JIT round 16, hcb: on from the switch-on commit (that commit is
            // the revert; `CRATONVM_COMPAT_ALIGN=-jdk-hash-collections` on a
            // host).
            | HotSpotAlignment::JdkHashCollections
            // Strings round (`str16`): on from its switch-on commit (the
            // revert; `CRATONVM_COMPAT_ALIGN=-jdk-string-builder-bodies`,
            // `-jdk-arrays-bodies`).
            | HotSpotAlignment::JdkStringBuilderBodies
            | HotSpotAlignment::JdkArraysBodies
            // SBR round 5, lane awtjfr: on from the switch-on commit (that commit
            // is the revert; `CRATONVM_COMPAT_ALIGN=-jdk-java2d` on a host).
            | HotSpotAlignment::JdkJava2d
            // SBR round 5, lane compat2: on from the lane's switch-on commit
            // (that commit is the revert; `CRATONVM_COMPAT_ALIGN=-<token>` on a
            // host). Probes `Sbr5Compat2Streams` / `Module` / `Walker`,
            // `SbrW4Fjp`, `Sbr2W7ModuleGraph`: `--compatible` equals HotSpot on
            // every line with the rows on, where it differed on 6-48.
            | HotSpotAlignment::JdkStackWalker
            | HotSpotAlignment::JdkSignal
            | HotSpotAlignment::JdkRuntime
            | HotSpotAlignment::JdkStreams
            | HotSpotAlignment::JdkModuleAccess
            // JIT round 19 (nio19): on from the switch-on commit (that commit
            // is the revert; `CRATONVM_COMPAT_ALIGN=-jdk-pipe` on a host). The
            // fabricated pipe hung a non-blocking read in both modes.
            | HotSpotAlignment::JdkPipe
            // JIT round 19 (nio19): on from the switch-on commit
            // (`CRATONVM_COMPAT_ALIGN=-jdk-nio-buffers` reverts on a host).
            | HotSpotAlignment::JdkNioBuffers
            | HotSpotAlignment::JdkPreconditions
            // Interpreter round i2 wave 69, lane L1: on from its first commit
            // (a Java-visible blocker / thread-state answer moved to
            // HotSpot's; `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0` reverts).
            | HotSpotAlignment::JdkLockSupport
            // Interpreter round i2 wave 70, lane L1: the same shape
            // (`CRATONVM_COMPAT_JDK_STAMPED_LOCK=0` reverts).
            | HotSpotAlignment::JdkStampedLock
            // Interpreter round i3 wave 71, lane L1: the same shape
            // (`CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH=0` reverts). `--jdk-only`
            // (the launcher default) runs these bodies, and real AQS has been
            // `--compatible`'s `ReentrantLock` / `Semaphore` / `CyclicBarrier`
            // since before round i2.
            | HotSpotAlignment::JdkCountDownLatch => true,
            // Interpreter round i2, the i47 page: on from its switch-on
            // commit (`CRATONVM_COMPAT_ALIGN=-verifier-loads-at-link` on a
            // host).
            HotSpotAlignment::VerifierLoadsAtLink | HotSpotAlignment::JavaNameAsk => true,
            // JIT round 20 (nio20, lane L4): the four families whose
            // `--compatible` natives carry no VM state the JDK bodies lack
            // (the reference model's state moves with its row: discovery at
            // allocation and the queue wake-ups). On from the lane's commit
            // (`CRATONVM_COMPAT_ALIGN=-<token>` reverts one on a host).
            HotSpotAlignment::JdkReferences
            | HotSpotAlignment::JdkObjectBodies
            | HotSpotAlignment::JdkUnsafeBulkBodies
            | HotSpotAlignment::JdkOptional => true,
            // JIT round 20 (nio20 wave 2, lane L5): the families the wave-1 page
            // left unadopted, each read native by native for VM state the JDK
            // bodies lack (`retired_shadow::COMPAT_FAMILIES`, the comment on
            // each entry). On from the lane's commit
            // (`CRATONVM_COMPAT_ALIGN=-<token>` reverts one on a host).
            HotSpotAlignment::JdkArrayList
            | HotSpotAlignment::JdkLocale
            | HotSpotAlignment::JdkDate
            | HotSpotAlignment::JdkObjectIdentityBodies => true,
            // `jdk-nio-channels` (nio20, lane L4): the whole socket / selector /
            // datagram family over the real `sun.nio.ch` layer. ON since the
            // nio20 host census (2026-10-07): the registry diff drops exactly the
            // 322 channel/selector synthetic stubs and adds nothing; every
            // `NioConf19` socket/selector/datagram row and `Nio20EchoStress`
            // (selector echo, interrupt, async close, selector churn, no fd leak)
            // match HotSpot; the Spring Boot sample starts. Netty/Tomcat fixtures
            // were not available on the host (`CRATONVM_COMPAT_ALIGN=-jdk-nio-channels`
            // reverts it there).
            HotSpotAlignment::JdkNioChannels => true,
            // `jdk-async-channels` (nio20 wave 3, lane L1): the async socket
            // family over the real `sun.nio.ch` layer, the same shape as
            // `jdk-nio-channels` (the factories are rows of the table it
            // adopts). ON since the nio20 host census (2026-10-07,
            // `cratonvm-nio20-w3b`): the registry diff off/on drops exactly the
            // 118 async-channel stand-ins (116 `SyntheticStub`s, the two
            // `shutdown*` bridges) and adds nothing; `NioConf19b` `asock`,
            // `Nio20EchoStress`, `Nio20CompatCarriers` and the Spring Boot sample
            // match with the row on and off. No NIO2 server fixture was on the
            // host (`CRATONVM_COMPAT_ALIGN=-jdk-async-channels` reverts it).
            HotSpotAlignment::JdkAsyncChannels => true,
            // `jdk-http-client` (nio20 wave 4, lane L4): the JDK's
            // `jdk.internal.net.http` client in place of the three native
            // carriers. ON: the carriers hold no state the JDK client lacks
            // (`retired_shadow::COMPAT_FAMILIES`, the review on the entry), it
            // rides `jdk-nio-channels` (a prerequisite: with that row off this
            // one is not in force either), and the carriers had no WebSocket
            // at all. Its cost is the JDK client's (`SbrW5HttpBench`, measured
            // ~10x the carrier per request under `--jdk-only` in SBR wave 6);
            // `CRATONVM_COMPAT_ALIGN=-jdk-http-client` reverts it on a host.
            HotSpotAlignment::JdkHttpClient => true,
            // `jdk-locale-providers` (nio20 wave 5, lane L4): the provider
            // natives carry no VM state the JDK bodies lack (the review on the
            // `retired_shadow::COMPAT_FAMILIES` entry: the bundle and
            // break-iterator carriers are minted only by the family's own
            // rows), and `--jdk-only` has run these bodies since SBR wave 4
            // (`SbrW4Locale2`, 0 lines from HotSpot). ON; the A/B is
            // `CRATONVM_COMPAT_ALIGN=-jdk-locale-providers` on a host.
            HotSpotAlignment::JdkLocaleProviders => true,
            // `jdk-print-writer` (nio20 wave 6, lane L5): the seven retired
            // `PrintWriter` rows carry no VM state the JDK bodies lack (the
            // review on the `retired_shadow::COMPAT_FAMILIES` entry: the
            // constructor native already chained into the JDK's two-argument
            // one, and `printed_lines` is a test observation that only grew).
            // `--jdk-only` has run these bodies since lane 4 wave 6. ON; the
            // A/B is `CRATONVM_COMPAT_ALIGN=-jdk-print-writer` on a host.
            HotSpotAlignment::JdkPrintWriter => true,
            // `jdk-jsse`: correct but 20-50x the native carrier on a TLS
            // handshake and blocked by a C1 miscompile
            // (`docs/feature-designs/jdk-only-mode.md`):
            // stays OFF.
            HotSpotAlignment::JdkJsse => false,
            // `jdk-http-server` (nio20 wave 5, lane L1): the JDK's own
            // `sun.net.httpserver` in place of the Rust carrier server. Correct
            // by the same review as `jdk-http-client` (the family entry), and
            // `--jdk-only` has served every `HttpServer` this way since SBR
            // wave 6 (152/152 suite). ON since the nio21 wave 13 host A/B
            // (linux, `cratonvm-jdkret-w4`): with the row on, the five
            // `HttpServer*` / `HttpContext*` / `HttpExchange*` probes, `NioConf19`
            // and `NioConf19b`, `Nio20CompatCarriers` and `Nio21W3Doors` equal
            // `--jdk-only` line for line; with it off, four of the probes differ
            // from HotSpot (`HttpContextImpl` / `HttpExchangeImpl` class names,
            // `getAddress()` text). The carrier is ~9x faster per request
            // (`SbrW5HttpBench huc.get`, 270 vs 2 411 us); that is the JIT page
            // `r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot`, not a
            // reason to keep a second server. `CRATONVM_COMPAT_ALIGN=
            // -jdk-http-server` reverts it.
            HotSpotAlignment::JdkHttpServer => true,
            // (`jdk-concurrent-hash-map` and `jdk-completable-future`, which
            // nio21 built too, are with the interpreter round i3 rows below.)
            //
            // `jdk-scalar-atomics` (nio21 wave 1, lane L4): correct by the
            // family review (one shared `value` field). ON since nio21 wave 3
            // (lane L1): the wave-2 host census priced it neutral on every
            // `Nio20HttpHot` row (`hc` 1.43-3.07M vs 1.41-3.70M ns, `conc`
            // 4.0-5.3k vs 3.9-6.0k) and `Nio21ChmPair atomics` matched
            // HotSpot with it on. `CRATONVM_COMPAT_ALIGN=-jdk-scalar-atomics`
            // reverts it on a host.
            HotSpotAlignment::JdkScalarAtomics => true,
            // nio21 wave 2, lane L4: the `HttpClient` request path's
            // stand-in families. Each entry of
            // `retired_shadow::COMPAT_FAMILIES` (or the carrier-thread-local
            // predicate) drops every registration `--jdk-only` drops on its
            // classes (the 25/linux kind map lists only retired rows and
            // `SyntheticStub`s there, plus `TreeMap`'s seven `Bridge`s on
            // triples the image does not declare), so `--compatible` runs the
            // bodies `--jdk-only` has run since September; the review is on
            // each entry. ON from the lane's commit; `CRATONVM_COMPAT_ALIGN=
            // -<token>` reverts one on a host. `jdk-carrier-thread-locals` is a
            // correctness fix (one value per `CarrierThreadLocal` for the
            // whole VM before it).
            HotSpotAlignment::JdkTreeMap
            | HotSpotAlignment::JdkLinkedCollections
            | HotSpotAlignment::JdkTimeUnit
            | HotSpotAlignment::JdkAdders
            | HotSpotAlignment::JdkCarrierThreadLocals => true,
            // nio21 wave 3, lane L1: two more `HttpClient`-path doors, each a
            // registration-time row with no VM state the JDK bodies lack (the
            // review on the `retired_shadow` predicate). ON from the lane's
            // commit; `CRATONVM_COMPAT_ALIGN=-<token>` reverts one on a host.
            HotSpotAlignment::JdkThreadInterrupt | HotSpotAlignment::JdkUnsafeDelegations => true,
            // nio21 wave 3, lane L1, staged OFF there; ON since nio21 wave 4
            // (lane L1). `jdk-uri`: the `nio_file` reader follows the row, and
            // the w3 host run with `+jdk-uri,+jdk-byte-array-streams` matched
            // HotSpot on every `Nio21W3Doors` `uri` / `baos` row but the one
            // that reader broke (`uri resolved-path`, whose throw also cut
            // off `uri tourl`); with the row off,
            // `--compatible` answered two `uri` rows wrong (`equals` of a
            // case-folded escape, `resolve(null)` with no NPE).
            // `jdk-byte-array-streams`: the fixed-length upload's stream is the
            // JDK's `StreamingOutputStream` over its own carrier, so nothing
            // HTTP rides the `ByteArrayOutputStream` natives any more.
            // `CRATONVM_COMPAT_ALIGN=-<token>` reverts either on a host.
            HotSpotAlignment::JdkUri | HotSpotAlignment::JdkByteArrayStreams => true,
            // `jdk-shared-secrets` (nio21 wave 4, lane L1): the
            // `getJavaLangAccess` getter over the static `initPhase1` now
            // publishes, as `--jdk-only` has done since SBR wave 6. ON;
            // `CRATONVM_COMPAT_ALIGN=-jdk-shared-secrets` reverts it on a host.
            HotSpotAlignment::JdkSharedSecrets => true,
            // `jdk-invoke` (nio21 wave 12, lane L3): ON since the census of
            // 2026-10-09 (Linux, JDK 25): `--compatible` `Nio21ConcreteReceivers`
            // equals HotSpot on every `vh-*` / `mh-*` row, the regression suite
            // passes 109/109 with and without the row, a `VarHandle` /
            // `invokeExact` / `AtomicInteger` / `ConcurrentHashMap.merge` loop
            // costs the same (the handle call is faster), and the Spring Boot
            // sample starts in the same time. `CRATONVM_COMPAT_ALIGN=-jdk-invoke`
            // reverts it.
            HotSpotAlignment::JdkInvoke => true,
            // `jdk-archive-fs` (nio21 close): ON since the census of 2026-10-09
            // (Linux, JDK 25): `--compatible` `Nio21ConcreteReceivers` and
            // `RealFsProbe` equal HotSpot with the row (the `jrt:` listing was
            // empty without it), the `FileSystem` census row is gone, the
            // regression suite passes 109/109 and the Spring Boot sample starts
            // exploded and as `-jar` in the same time. `CRATONVM_COMPAT_ALIGN=
            // -jdk-archive-fs` reverts it.
            HotSpotAlignment::JdkArchiveFs => true,
            // Wave 52, lane L5's row, turned back OFF by the orchestrator
            // after the host run: the Spring Boot fat jar
            // (`/data/sbjar-fat/simple-fat.jar`, `--compatible`, JIT on)
            // started in 10.1 s with it on and 3.96 s with
            // `CRATONVM_COMPAT_ALIGN=-unflattened-app-class-path` (3.68 s with
            // `-jdk-url-class-loaders`, which disables both), all four runs
            // started and none raised a `ClassCastException` /
            // `ArrayStoreException`. The real `URLClassLoader` bodies
            // (`JdkUrlClassLoaders`) stay on; the unflattened class path
            // waits for that cost to be found
            // (`interpreter-L5-compatible-gates-that-still-differ-after-the-alignment-table-FIXED-20261007`).
            //
            // Interpreter round i2 wave 63: back ON. The host's wave-63 run
            // (dev `4657c937e1` and the i50 / i51 branch, three interleaved
            // rounds under load): with the row the jar starts in 13.8 /
            // 14.9 / 13.8 s; without it, it no longer starts at all
            // (`Unable to load factories from location
            // [META-INF/spring.factories]` after 1.2 s, the same on dev).
            // The layout is HotSpot's; its start-up cost is
            // `docs/feature-designs/class-resolution-and-caching.md`.
            HotSpotAlignment::UnflattenedAppClassPath => true,
            // `jdk-aqls` (interpreter round i3 wave 72, lane L1): the JDK's
            // three `state` accessors over the real field. ON from the lane's
            // switch-on commit (that commit is the revert;
            // `CRATONVM_COMPAT_JDK_AQLS=0` on a host), a pure HotSpot
            // alignment: the JDK bodies are a `getfield`, a `putfield` and
            // `Unsafe.compareAndSetLong`, the CAS `AtomicLong` runs in both
            // modes; `--jdk-only` (the launcher default) has run these three
            // bodies since they were retired; the rest of AQLS (acquire,
            // release, `ConditionObject`) was the JDK's bytecode in
            // `--compatible` already; the stand-ins' side table is gone (wave
            // 73: they use the field too); and no `java.base` class extends AQLS, so the
            // row reaches application subclasses only.
            HotSpotAlignment::JdkAqls => true,
            // `jdk-copy-on-write` (interpreter round i3 wave 72, lane L1) and
            // `jdk-copy-on-write-set` (wave 74, lane L1): ON since the wave-74
            // triage. Probe `L1W74CopyOnWriteLockShapes` reads the same with
            // the row on as off (the 2026-07-14 `this.lock` NPE the bridges
            // were added around does not recur), `L1W72CopyOnWriteBodies` and
            // `L1W74CopyOnWriteSetBodies` match HotSpot in `--compatible` only
            // with the rows on, and the set bench is 3-90x faster.
            // `CRATONVM_COMPAT_JDK_COPY_ON_WRITE=0` / `_SET=0` revert.
            HotSpotAlignment::JdkCopyOnWrite => true,
            HotSpotAlignment::JdkCopyOnWriteArraySet => true,
            // `jdk-priority-blocking-queue` (interpreter round i3 wave 75,
            // lane L1): ON since the wave-75 triage. `L1W75PriorityBlockingQueueBodies
            // --compatible` reads DIFF(16) with the row off (heap order,
            // comparator, waiters) and SAME with it on; the JDK bodies cost
            // what `--jdk-only` pays (peek / size ~0.8 us, drain256 8.6 -> 4.5 us).
            // `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE=0` reverts.
            HotSpotAlignment::JdkPriorityBlockingQueue => true,
            // `jdk-concurrent-hash-map` (interpreter round i3 wave 76, lane
            // L1): ON since the wave-76 triage. `L1W76ConcurrentHashMapBodies
            // --compatible` reads DIFF(2) with the row off and SAME with it on;
            // `L1W76ConcurrentHashMapBench --compatible` get 411 -> 197 ns,
            // computeIfAbsent 7221 -> 260, size 280 -> 12 (`--nojit` get
            // 1275 -> 2764: the JDK body interpreted).
            // `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP=0` reverts. nio21 (wave
            // 1, lane L4; ON since wave 4, lane L1) built the same row with
            // `Properties` and the system-properties arms (gates A-E of
            // `r21-compatible-chm-row-needs-the-system-properties-arms`); its
            // host runs with the row on (binaries w2 and w3) started the Spring
            // Boot sample in both modes and matched HotSpot on `Nio21ChmPair`,
            // `Nio20CompatRows` and `NioConf19`.
            HotSpotAlignment::JdkConcurrentHashMap => true,
            // `jdk-process` (interpreter round i3 wave 80, lane L6): ON since
            // the wave-81 triage. The 265-probe battery with the row on reads
            // one change against the wave-80 build, `L6W59BlockedNativeStates
            // --compatible` DIFF(8) -> SAME (the blocked `pipe` / `waitFor`
            // threads list the JDK's frames); every process probe is SAME.
            // `CRATONVM_COMPAT_JDK_PROCESS=0` reverts.
            HotSpotAlignment::JdkProcess => true,
            // `jdk-completable-future` (interpreter round i3 wave 80, lane
            // L1): ON since the wave-80 triage. `L1W80CompletableFutureBodies
            // --compatible` reads DIFF(9) with the row off (the stand-in
            // `anyOf` fails outright) and SAME with it on;
            // `L1W80CompletableFutureBench --compatible` all-of-4 3274 -> 468 ns,
            // complete-exc 3789 -> 353, then-combine 6655 -> 3371, complete
            // 629 -> 324. `CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=0` reverts.
            // nio21 (wave 2, lane L4) built the same row, ON from its commit
            // (the wave-1 census priced the stand-ins at nothing).
            HotSpotAlignment::JdkCompletableFuture => true,
            // `jdk-thread-pools` (interpreter round i3 wave 82, lane L1): a new
            // row starts off. `L1W82ThreadPoolsBodies` with it on and off, the
            // battery and a pool-heavy workload (Tomcat fixture, Spring Boot
            // sample) decide the flip; `L1W82ThreadPoolsBench` prices it.
            HotSpotAlignment::JdkThreadPools => false,
            // `jdk-sun-misc-unsafe` (nio21 wave 13, lane L4): the legacy facade's
            // JDK bodies in place of the stand-ins, so `--compatible` prints
            // HotSpot 25's `sun.misc.Unsafe` deprecation warning
            // (`Nio21CasRace`, `Nio21UnsafeOffheap`: stderr equal to HotSpot).
            // Windows A/B on three Unsafe probes: output identical, wall time
            // inside the run-to-run noise. ON; `CRATONVM_COMPAT_ALIGN=
            // -jdk-sun-misc-unsafe` reverts it.
            HotSpotAlignment::JdkSunMiscUnsafe => true,
            // `jdk-foreign`: ON (see the variant's doc for the measurements);
            // `CRATONVM_COMPAT_ALIGN=-jdk-foreign` reverts it.
            HotSpotAlignment::JdkForeign => true,
            // `jdk-atomic-arrays`: ON (see the variant's doc for the
            // measurements); `CRATONVM_COMPAT_ALIGN=-jdk-atomic-arrays`
            // reverts it.
            HotSpotAlignment::JdkAtomicArrays => true,
            // `jdk-javac-file-manager` (JIT round 20 wave 14, lane cjavac): ON
            // from its commit, a correctness row (see the variant's doc);
            // `CRATONVM_COMPAT_ALIGN=-jdk-javac-file-manager` reverts it.
            HotSpotAlignment::JdkJavacFileManager => true,
            // `jdk-foreign-segments` (JIT round 20 wave 19, lane yCompatSeg): ON
            // from its commit (see the variant's doc);
            // `CRATONVM_COMPAT_ALIGN=-jdk-foreign-segments` reverts it.
            HotSpotAlignment::JdkForeignSegments => true,
            // Interpreter round i6 wave 118, lane L4: the four L4 check
            // families, on by default since waves 49-50 under their own
            // variables (now the rows' legacy keys).
            HotSpotAlignment::IndyLinkChecks
            | HotSpotAlignment::SelectionChecks
            | HotSpotAlignment::MhCombinatorChecks
            | HotSpotAlignment::LdcHandleAccessChecks => true,
            // Interpreter round i6 wave 118, lane L5: on by default since wave
            // 49 under their own variables (now the rows' legacy keys).
            HotSpotAlignment::CompatNamedPackages
            | HotSpotAlignment::CompatJdkImmutables
            | HotSpotAlignment::ModuleExportAllByIdentity => true,
            // `jdk-hashtable` (interpreter round i6 wave 119, lane L5): default
            // ON since interpreter round i7 wave 144 (A/B w142lto);
            // `CRATONVM_COMPAT_JDK_HASHTABLE=0` or `-jdk-hashtable` reverts it.
            HotSpotAlignment::JdkHashtable => true,
            // `jdk-logging` (interpreter round i6 wave 120, lane L5): OFF until
            // its probe and the `--compatible` suite say otherwise.
            HotSpotAlignment::JdkLogging => false,
            // `jdk-string-get-bytes` (interpreter round i6 wave 122, lane L1):
            // OFF until its probe and bench say otherwise.
            HotSpotAlignment::JdkStringGetBytes => false,
            // `jdk-memory-usage` (interpreter round i6 wave 123, lane L1): OFF
            // until its probe and bench say otherwise.
            HotSpotAlignment::JdkMemoryUsage => false,
            // `jdk-management-beans` (interpreter round i6 wave 124, lane L1):
            // OFF until its probe and bench say otherwise.
            HotSpotAlignment::JdkManagementBeans => false,
            // `jdk-type-variable-impl` (interpreter round i6 wave 125, lane
            // L1): OFF until its probe and bench say otherwise.
            HotSpotAlignment::JdkTypeVariableImpl => false,
            // `jdk-priority-queue` (interpreter round i6 wave 133, lane L6):
            // OFF until its probe, bench and the `--compatible` suite say
            // otherwise.
            HotSpotAlignment::JdkPriorityQueue => false,
            // `jdk-vm-init-level` (JIT round j27 wave 5, lane VMALIGN): ON from
            // its first commit, a HotSpot-parity fix (two bare static reads;
            // lane FBTAIL's `initLevel` native already answered the static).
            // `CRATONVM_COMPAT_ALIGN=-jdk-vm-init-level` reverts it on a host.
            HotSpotAlignment::JdkVmInitLevel => true,
        }
    }

    /// Does the row move a `--compatible` registration to the JDK's own
    /// bytecode, so that it can only be in force over a real JDK class library?
    /// (`NativeMethodRegistry::aligned` answers `false` for such a row on the
    /// synthetic library. The older registration-time rows carry their own
    /// `real_jdk` test at their site.)
    pub const fn needs_real_jdk(self) -> bool {
        matches!(
            self,
            HotSpotAlignment::JdkForkJoinPool
                | HotSpotAlignment::JdkProviders
                | HotSpotAlignment::JdkModuleGraph
                | HotSpotAlignment::JdkHashCollections
                | HotSpotAlignment::JdkStringBuilderBodies
                | HotSpotAlignment::JdkArraysBodies
                | HotSpotAlignment::JdkJava2d
                | HotSpotAlignment::JdkStackWalker
                | HotSpotAlignment::JdkSignal
                | HotSpotAlignment::JdkRuntime
                | HotSpotAlignment::JdkStreams
                | HotSpotAlignment::JdkModuleAccess
                | HotSpotAlignment::JdkJsse
                | HotSpotAlignment::JdkPipe
                | HotSpotAlignment::JdkNioBuffers
                | HotSpotAlignment::JdkPreconditions
                | HotSpotAlignment::JdkLockSupport
                | HotSpotAlignment::JdkStampedLock
                | HotSpotAlignment::JdkReferences
                | HotSpotAlignment::JdkObjectBodies
                | HotSpotAlignment::JdkUnsafeBulkBodies
                | HotSpotAlignment::JdkOptional
                | HotSpotAlignment::JdkNioChannels
                | HotSpotAlignment::JdkArrayList
                | HotSpotAlignment::JdkLocale
                | HotSpotAlignment::JdkDate
                | HotSpotAlignment::JdkObjectIdentityBodies
                | HotSpotAlignment::JdkAsyncChannels
                | HotSpotAlignment::JdkHttpClient
                | HotSpotAlignment::JdkLocaleProviders
                | HotSpotAlignment::JdkHttpServer
                | HotSpotAlignment::JdkPrintWriter
                | HotSpotAlignment::JdkCountDownLatch
                | HotSpotAlignment::JdkAqls
                | HotSpotAlignment::JdkCopyOnWrite
                | HotSpotAlignment::JdkCopyOnWriteArraySet
                | HotSpotAlignment::JdkPriorityBlockingQueue
                | HotSpotAlignment::JdkConcurrentHashMap
                | HotSpotAlignment::JdkProcess
                | HotSpotAlignment::JdkCompletableFuture
                | HotSpotAlignment::JdkScalarAtomics
                | HotSpotAlignment::JdkTreeMap
                | HotSpotAlignment::JdkLinkedCollections
                | HotSpotAlignment::JdkTimeUnit
                | HotSpotAlignment::JdkAdders
                | HotSpotAlignment::JdkCarrierThreadLocals
                | HotSpotAlignment::JdkThreadInterrupt
                | HotSpotAlignment::JdkUnsafeDelegations
                | HotSpotAlignment::JdkUri
                | HotSpotAlignment::JdkByteArrayStreams
                | HotSpotAlignment::JdkSharedSecrets
                | HotSpotAlignment::JdkInvoke
                | HotSpotAlignment::JdkArchiveFs
                | HotSpotAlignment::JdkThreadPools
                | HotSpotAlignment::JdkSunMiscUnsafe
                | HotSpotAlignment::JdkForeign
                | HotSpotAlignment::JdkAtomicArrays
                | HotSpotAlignment::JdkJavacFileManager
                | HotSpotAlignment::JdkForeignSegments
                | HotSpotAlignment::JdkHashtable
                | HotSpotAlignment::JdkLogging
                | HotSpotAlignment::JdkStringGetBytes
                | HotSpotAlignment::JdkMemoryUsage
                | HotSpotAlignment::JdkManagementBeans
                | HotSpotAlignment::JdkTypeVariableImpl
                | HotSpotAlignment::JdkPriorityQueue
                | HotSpotAlignment::JdkVmInitLevel
        )
    }

    /// Has this row GRADUATED: over a real JDK it is in force whatever the
    /// set says, so neither its legacy variable nor `CRATONVM_COMPAT_ALIGN`
    /// (`-<token>`, `none`) brings the VM's stand-ins back
    /// ([`HotSpotAlignments::with_graduated_rows`], applied once by
    /// `vm_init`). The synthetic class library is not touched: a family row is
    /// never in force there ([`Self::needs_real_jdk`]).
    ///
    /// 2026-10-11 (`docs/feature-designs/jdk-only-mode.md`):
    /// * `JdkForkJoinPool`: with it off, a real JDK parallel pipeline ran
    ///   over the VM's `ForkJoinPool` / `ForkJoinTask` stand-ins and lost
    ///   results (`List.of(..).parallelStream().takeWhile(..)` answered `[]`,
    ///   `dropWhile` / `mapMulti` threw `NullPointerException` in
    ///   `Node.getChildCount()`), and hung with `-jdk-streams` added.
    /// * `JdkStreams`: with it off (and `-jdk-stack-walker`), the
    ///   `StackWalker.walk` stream was the VM's carrier again and
    ///   `takeWhile` on it answered `ReferencePipeline$Head` (HotSpot
    ///   `WhileOps$1`).
    pub const fn graduated(self) -> bool {
        matches!(
            self,
            HotSpotAlignment::JdkForkJoinPool | HotSpotAlignment::JdkStreams
        )
    }

    /// The row a token names.
    pub fn from_token(token: &str) -> Option<HotSpotAlignment> {
        HotSpotAlignment::ALL
            .into_iter()
            .find(|a| a.token() == token)
    }

    /// The row's bit. The set is a `u128` since nio21 wave 2 (lane L4): bit
    /// 59 was the last a `u64` had room for after the four rows that wave
    /// needed, so the width doubled rather than rows being merged.
    #[inline]
    const fn bit(self) -> u128 {
        1u128 << (self as u8)
    }
}

/// The `--compatible` rows in force for one VM (see the module doc). Under
/// `--jdk-only` every row is in force whatever the set says: ask through
/// [`Self::aligned`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotSpotAlignments(u128);

/// The variable that edits the set (see the module doc).
pub const COMPAT_ALIGN_KEY: &str = "CRATONVM_COMPAT_ALIGN";

impl HotSpotAlignments {
    /// No row.
    pub const NONE: HotSpotAlignments = HotSpotAlignments(0);

    /// Every row.
    pub const fn all() -> HotSpotAlignments {
        let mut bits = 0u128;
        let mut i = 0;
        while i < HotSpotAlignment::ALL.len() {
            bits |= HotSpotAlignment::ALL[i].bit();
            i += 1;
        }
        HotSpotAlignments(bits)
    }

    /// The defaults ([`HotSpotAlignment::default_on`]), no variable read.
    pub const fn defaults() -> HotSpotAlignments {
        let mut bits = 0u128;
        let mut i = 0;
        while i < HotSpotAlignment::ALL.len() {
            if HotSpotAlignment::ALL[i].default_on() {
                bits |= HotSpotAlignment::ALL[i].bit();
            }
            i += 1;
        }
        HotSpotAlignments(bits)
    }

    /// The defaults, then each row's legacy variable, then
    /// `CRATONVM_COMPAT_ALIGN`, read through the runtime flag boundary
    /// (per-thread test overrides included). Called once per VM.
    pub fn from_env() -> HotSpotAlignments {
        HotSpotAlignments::from_lookup(|key| {
            crate::flags::runtime_var_os(key).map(|v| v.to_string_lossy().into_owned())
        })
    }

    /// [`Self::from_env`] over any variable lookup.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> HotSpotAlignments {
        let mut set = HotSpotAlignments::defaults();
        for a in HotSpotAlignment::ALL {
            if let Some(key) = a.legacy_key() {
                if let Some(value) = get(key) {
                    set = set.with(a, !crate::flags::value_is_off(&value));
                }
            }
        }
        if let Some(list) = get(COMPAT_ALIGN_KEY) {
            set = set.edited(&list);
        }
        set
    }

    /// `self` edited by a `CRATONVM_COMPAT_ALIGN` list. An unknown name is
    /// ignored (and reported on stderr: a misspelt revert must not look like
    /// a revert that changed nothing).
    pub fn edited(self, list: &str) -> HotSpotAlignments {
        let mut set = self;
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match item {
                // The boolean words too: the grouped `CRATONVM_COMPAT` token
                // `compat-align` sets the variable to `1`.
                "all" | "1" | "true" | "on" | "yes" => set = HotSpotAlignments::all(),
                "none" | "0" | "false" | "off" | "no" => set = HotSpotAlignments::NONE,
                _ => {
                    let (on, name) = if let Some(rest) = item.strip_prefix('-') {
                        (false, rest)
                    } else {
                        (true, item.strip_prefix('+').unwrap_or(item))
                    };
                    match HotSpotAlignment::from_token(name) {
                        Some(a) => set = set.with(a, on),
                        None => eprintln!(
                            "[cratonvm] {COMPAT_ALIGN_KEY}: unknown alignment `{name}` ignored"
                        ),
                    }
                }
            }
        }
        set
    }

    /// `self` with `a` set to `on`.
    #[must_use]
    pub const fn with(self, a: HotSpotAlignment, on: bool) -> HotSpotAlignments {
        if on {
            HotSpotAlignments(self.0 | a.bit())
        } else {
            HotSpotAlignments(self.0 & !a.bit())
        }
    }

    /// Is `a` in the set (the `--compatible` answer)?
    #[inline]
    pub const fn contains(self, a: HotSpotAlignment) -> bool {
        self.0 & a.bit() != 0
    }

    /// Does this VM behave as HotSpot for `a`: `--jdk-only`, or `--compatible`
    /// with the row set. One compare and one bit test.
    #[inline]
    pub const fn aligned(self, mode: CompatibilityMode, a: HotSpotAlignment) -> bool {
        matches!(mode, CompatibilityMode::JdkOnly) || self.contains(a)
    }

    /// `self` with every graduated row ([`HotSpotAlignment::graduated`]) put
    /// back, for a VM over a real JDK. Pure.
    #[must_use]
    pub fn with_graduated_rows(self) -> HotSpotAlignments {
        HotSpotAlignment::ALL
            .into_iter()
            .filter(|a| a.graduated())
            .fold(self, |set, a| set.with(a, true))
    }

    /// The graduated rows `self` turns off (what a revert asked for and
    /// [`Self::with_graduated_rows`] will ignore), by token.
    pub fn graduated_rows_turned_off(self) -> Vec<&'static str> {
        HotSpotAlignment::ALL
            .into_iter()
            .filter(|a| a.graduated() && !self.contains(*a))
            .map(HotSpotAlignment::token)
            .collect()
    }

    /// The rows in force, by token (`CRATONVM_DBG=access` census lines).
    pub fn tokens(self) -> Vec<&'static str> {
        HotSpotAlignment::ALL
            .into_iter()
            .filter(|a| self.contains(*a))
            .map(HotSpotAlignment::token)
            .collect()
    }
}

impl Default for HotSpotAlignments {
    fn default() -> Self {
        HotSpotAlignments::defaults()
    }
}

/// How often each row's answer differed from the one the row-off route
/// would have given, for one VM (interpreter round i1 wave 51, lane L5;
/// `docs/feature-designs/profile-guided-inlining.md`). A
/// row graduates -- its row and its `is_jdk_only` gate deleted -- when the
/// host's `--compatible` workloads show that every counted divergence is
/// one HotSpot shows too, and a row whose count stays 0 everywhere is
/// graduated on its probe alone. The census can only say that for a row
/// whose call site knows both answers ([`HotSpotAlignment::counted`]); the
/// exit line prints `?` for the others, never a misleading 0.
///
/// Bumped only on the cold arm where the two answers differ (one relaxed
/// add), in either mode where the row-off answer is already at hand (under
/// `--jdk-only` the count then says how often the old `--compatible` route
/// would have answered otherwise); a site that would pay a lookup to learn
/// it counts under `--compatible` only. Owned per VM
/// (`ClassRealm`, shared by `Arc` with the class manager): no process
/// global. Printed once at exit under `CRATONVM_DBG=access`
/// ([`Self::census_line`]).
#[derive(Debug)]
pub struct AlignmentCensus {
    // One counter per row (a fixed 64 while the set was a `u64`).
    counts: [std::sync::atomic::AtomicU64; HotSpotAlignment::ALL.len()],
}

impl AlignmentCensus {
    /// Every count zero.
    pub fn new() -> AlignmentCensus {
        AlignmentCensus {
            counts: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// `a` answered other than its row-off route would have. Cold.
    #[cold]
    #[inline(never)]
    pub fn note(&self, a: HotSpotAlignment) {
        self.counts[a as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `a`'s count so far.
    pub fn count(&self, a: HotSpotAlignment) -> u64 {
        self.counts[a as usize].load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The exit line's body: `mode=<mode> <token>=<n> ...`, one item per
    /// row in bit order, `<token>=?` for a row no site counts, and `off`
    /// appended (`<token>=<n>/off`) for a row not in force.
    pub fn census_line(&self, mode: CompatibilityMode, set: HotSpotAlignments) -> String {
        let mut line = String::from(if matches!(mode, CompatibilityMode::JdkOnly) {
            "mode=jdk-only"
        } else {
            "mode=compatible"
        });
        for a in HotSpotAlignment::ALL {
            line.push(' ');
            line.push_str(a.token());
            line.push('=');
            if a.counted() {
                line.push_str(&self.count(a).to_string());
            } else {
                line.push('?');
            }
            if !set.aligned(mode, a) {
                line.push_str("/off");
            }
        }
        line
    }
}

impl Default for AlignmentCensus {
    fn default() -> Self {
        AlignmentCensus::new()
    }
}

impl HotSpotAlignment {
    /// Does some call site of this row count its divergences
    /// ([`AlignmentCensus`])? A row without one prints `?`.
    pub const fn counted(self) -> bool {
        match self {
            HotSpotAlignment::ParallelLoaders
            | HotSpotAlignment::InitiatingRecords
            | HotSpotAlignment::LoaderConstraints
            | HotSpotAlignment::LoaderThrows
            | HotSpotAlignment::CatchTypesAtLink
            | HotSpotAlignment::SupertypeShapeChecks
            | HotSpotAlignment::UserLoaderUnnamedModule
            | HotSpotAlignment::FindLoadedNameCheck
            | HotSpotAlignment::ComponentTypeFromArrayClass
            | HotSpotAlignment::NamespacedArrayTypes
            | HotSpotAlignment::RuntimeImageNames => true,
            HotSpotAlignment::GlobalNameAsk
            | HotSpotAlignment::StubPrepassOneAsk
            | HotSpotAlignment::IsolatedStaticOwner => true,
            // A registration-time row (`JdkUnmodifiableViews`: the stand-ins
            // are not registered) has no answer site to count.
            HotSpotAlignment::UrlAppHandler
            | HotSpotAlignment::ModuleMessages
            | HotSpotAlignment::JdkUnmodifiableViews => false,
            // Wave 52, lane L4: a refused `checkcast` / `aastore` /
            // compiled type check counts in both modes; a `false`
            // `instanceof`, `Class.isInstance` / `isAssignableFrom` under
            // `--compatible` only (where the row-off route pays the same
            // by-name walk). The condy cast and a proxy's interface match do
            // not count.
            HotSpotAlignment::NamespacedClassTypes | HotSpotAlignment::NamespacedArrayStores => {
                true
            }
            // Wave 52, lane L5: counted under `--compatible` where a native
            // takes the JDK's route instead of the stand-in's (the local
            // `URLClassLoader` define refused, a user loader's resource
            // lookup handed to `ClassLoader`'s body): how much of a
            // workload the row moved.
            HotSpotAlignment::JdkUrlClassLoaders => true,
            // Boot-time (the class path is built once): no answer site.
            HotSpotAlignment::UnflattenedAppClassPath => false,
            // Counted where the record answered a name the memo had no row
            // for: a loader request the row saved.
            HotSpotAlignment::RecordBehindMemo => true,
            // Registration-time, like `JdkUnmodifiableViews`.
            HotSpotAlignment::JdkFileBodies => false,
            // Counted under `--compatible` where `URL.openConnection` /
            // `openStream` hand a `jar:file:` URL to the JDK's handler instead
            // of the VM's carrier and byte reader.
            HotSpotAlignment::JdkJarUrlConnections => true,
            // Registration-time, like `JdkFileBodies`.
            HotSpotAlignment::JdkJarFileBodies => false,
            // Registration-time, like `JdkFileBodies` (SBR round 4, lane compat).
            HotSpotAlignment::JdkForkJoinPool
            | HotSpotAlignment::JdkProviders
            | HotSpotAlignment::JdkModuleGraph => false,
            // Registration-time, plus the overlay gate, which has no second
            // answer to compare (the node path serves the same map). JIT round
            // 16, hcb.
            HotSpotAlignment::JdkHashCollections => false,
            // Registration-time, like `JdkFileBodies` (strings round, `str16`).
            HotSpotAlignment::JdkStringBuilderBodies | HotSpotAlignment::JdkArraysBodies => false,
            // Registration-time, like `JdkHashCollections` (SBR round 5, lane awtjfr).
            HotSpotAlignment::JdkJava2d => false,
            // Registration-time (plus gates that have no second answer to
            // compare), SBR round 5, lane compat2.
            HotSpotAlignment::JdkStackWalker
            | HotSpotAlignment::JdkSignal
            | HotSpotAlignment::JdkRuntime
            | HotSpotAlignment::JdkStreams
            | HotSpotAlignment::JdkModuleAccess
            | HotSpotAlignment::JdkJsse => false,
            // Registration-time (JIT round 19, nio19).
            HotSpotAlignment::JdkPipe
            | HotSpotAlignment::JdkNioBuffers
            | HotSpotAlignment::JdkPreconditions => false,
            // Registration-time (interpreter round i2 wave 69, lane L1).
            HotSpotAlignment::JdkLockSupport => false,
            // Registration-time (interpreter round i2 wave 70, lane L1).
            HotSpotAlignment::JdkStampedLock => false,
            // A link failure counts (`load_verifier_types_at_link`).
            HotSpotAlignment::VerifierLoadsAtLink | HotSpotAlignment::JavaNameAsk => true,
            // Registration-time plus gates with no second answer to compare
            // (JIT round 20, nio20, lane L4).
            HotSpotAlignment::JdkReferences
            | HotSpotAlignment::JdkObjectBodies
            | HotSpotAlignment::JdkUnsafeBulkBodies
            | HotSpotAlignment::JdkOptional
            | HotSpotAlignment::JdkNioChannels => false,
            // Registration-time (JIT round 20, nio20 wave 2, lane L5).
            HotSpotAlignment::JdkArrayList
            | HotSpotAlignment::JdkLocale
            | HotSpotAlignment::JdkDate
            | HotSpotAlignment::JdkObjectIdentityBodies => false,
            // Registration-time (JIT round 20, nio20 wave 3, lane L1).
            HotSpotAlignment::JdkAsyncChannels => false,
            // Registration-time (JIT round 20, nio20 wave 4, lane L4).
            HotSpotAlignment::JdkHttpClient => false,
            // Registration-time (JIT round 20, nio20 wave 5, lane L4).
            HotSpotAlignment::JdkLocaleProviders => false,
            // Registration-time (JIT round 20, nio20 wave 5, lane L1).
            HotSpotAlignment::JdkHttpServer => false,
            // Registration-time (JIT round 20, nio20 wave 6, lane L5).
            HotSpotAlignment::JdkPrintWriter => false,
            // Registration-time (interpreter round i3 wave 71, lane L1).
            HotSpotAlignment::JdkCountDownLatch => false,
            // Registration-time (interpreter round i3 wave 72, lane L1).
            HotSpotAlignment::JdkAqls => false,
            HotSpotAlignment::JdkCopyOnWrite => false,
            // Registration-time (interpreter round i3 wave 74, lane L1).
            HotSpotAlignment::JdkCopyOnWriteArraySet => false,
            // Registration-time (interpreter round i3 wave 75, lane L1).
            HotSpotAlignment::JdkPriorityBlockingQueue => false,
            // Registration-time plus door checks and readers with no second
            // answer to compare (interpreter round i3 wave 76, lane L1; JIT
            // round 21, nio21 wave 1, lane L4).
            HotSpotAlignment::JdkConcurrentHashMap => false,
            // Registration-time (interpreter round i3 wave 80, lane L6).
            HotSpotAlignment::JdkProcess => false,
            // Registration-time (interpreter round i3 wave 80, lane L1; nio21
            // wave 2, lane L4).
            HotSpotAlignment::JdkCompletableFuture => false,
            // Registration-time (JIT round 21, nio21 wave 1, lane L4).
            HotSpotAlignment::JdkScalarAtomics => false,
            // Registration-time (JIT round 21, nio21 wave 2, lane L4); the
            // carrier-thread-local bridges' gate has no second answer to
            // compare either.
            HotSpotAlignment::JdkTreeMap
            | HotSpotAlignment::JdkLinkedCollections
            | HotSpotAlignment::JdkTimeUnit
            | HotSpotAlignment::JdkAdders
            | HotSpotAlignment::JdkCarrierThreadLocals => false,
            // Registration-time (JIT round 21, nio21 wave 3, lane L1); the
            // `jdk-uri` producer gate has no second answer to compare.
            HotSpotAlignment::JdkThreadInterrupt
            | HotSpotAlignment::JdkUnsafeDelegations
            | HotSpotAlignment::JdkUri
            | HotSpotAlignment::JdkByteArrayStreams => false,
            // Registration-time plus the `initPhase1` publish (JIT round 21,
            // nio21 wave 4, lane L1).
            HotSpotAlignment::JdkSharedSecrets => false,
            // Registration-time plus the `lang_invoke` gates, which have no
            // second answer to compare (JIT round 21, nio21 wave 12, lane L3).
            HotSpotAlignment::JdkInvoke => false,
            HotSpotAlignment::JdkArchiveFs => false,
            // Registration-time (interpreter round i3 wave 82, lane L1).
            HotSpotAlignment::JdkThreadPools => false,
            // Registration-time (JIT round 21, nio21 wave 13, lane L4).
            HotSpotAlignment::JdkSunMiscUnsafe => false,
            // Registration-time.
            HotSpotAlignment::JdkForeign => false,
            // Registration-time.
            HotSpotAlignment::JdkAtomicArrays => false,
            // Registration-time (JIT round 20 wave 14, lane cjavac).
            HotSpotAlignment::JdkJavacFileManager => false,
            // Counted under `--compatible` where a pointer from C became the
            // JDK's segment instead of the carrier (JIT round 20 wave 19, lane
            // yCompatSeg).
            HotSpotAlignment::JdkForeignSegments => true,
            // Interpreter round i6 wave 118, lane L4: VM-creation rows (read
            // into `ClassRealm` once); their answer sites do not count.
            HotSpotAlignment::IndyLinkChecks
            | HotSpotAlignment::SelectionChecks
            | HotSpotAlignment::MhCombinatorChecks
            | HotSpotAlignment::LdcHandleAccessChecks => false,
            // Interpreter round i6 wave 118, lane L5: registration-time rows and
            // a per-question read; no site counts them.
            HotSpotAlignment::CompatNamedPackages
            | HotSpotAlignment::CompatJdkImmutables
            | HotSpotAlignment::ModuleExportAllByIdentity => false,
            // Registration-time (interpreter round i6 wave 119, lane L5).
            HotSpotAlignment::JdkHashtable => false,
            // Registration-time (interpreter round i6 wave 120, lane L5).
            HotSpotAlignment::JdkLogging => false,
            // Registration-time (interpreter round i6 wave 122, lane L1).
            HotSpotAlignment::JdkStringGetBytes => false,
            // Registration-time (interpreter round i6 wave 123, lane L1).
            HotSpotAlignment::JdkMemoryUsage => false,
            // Registration-time (interpreter round i6 wave 124, lane L1).
            HotSpotAlignment::JdkManagementBeans => false,
            // Registration-time (interpreter round i6 wave 125, lane L1).
            HotSpotAlignment::JdkTypeVariableImpl => false,
            // Registration-time (interpreter round i6 wave 133, lane L6).
            HotSpotAlignment::JdkPriorityQueue => false,
            // Registration-time (JIT round j27 wave 5, lane VMALIGN).
            HotSpotAlignment::JdkVmInitLevel => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graduated_rows_come_back_whatever_the_set_says() {
        let none = HotSpotAlignments::NONE;
        assert_eq!(
            none.graduated_rows_turned_off(),
            vec!["jdk-fork-join-pool", "jdk-streams"]
        );
        let back = none.with_graduated_rows();
        assert!(back.contains(HotSpotAlignment::JdkForkJoinPool));
        assert!(back.contains(HotSpotAlignment::JdkStreams));
        assert!(back.graduated_rows_turned_off().is_empty());
        // Only the graduated rows move.
        assert!(!back.contains(HotSpotAlignment::JdkStackWalker));
        let reverted = HotSpotAlignments::defaults().edited("-jdk-streams,-jdk-stack-walker");
        let fixed = reverted.with_graduated_rows();
        assert!(fixed.contains(HotSpotAlignment::JdkStreams));
        assert!(!fixed.contains(HotSpotAlignment::JdkStackWalker));
        assert_eq!(
            HotSpotAlignments::defaults().with_graduated_rows(),
            HotSpotAlignments::defaults()
        );
    }

    #[test]
    fn tokens_round_trip_and_bits_are_distinct() {
        let mut seen = 0u128;
        for (i, a) in HotSpotAlignment::ALL.into_iter().enumerate() {
            assert_eq!(a as usize, i, "ALL is in bit order");
            assert_eq!(HotSpotAlignment::from_token(a.token()), Some(a));
            assert_eq!(seen & a.bit(), 0);
            seen |= a.bit();
        }
    }

    #[test]
    fn the_list_edits_left_to_right() {
        let d = HotSpotAlignments::defaults();
        assert!(d.contains(HotSpotAlignment::LoaderThrows));
        let e = d.edited("-loader-throws, +catch-types-at-link");
        assert!(!e.contains(HotSpotAlignment::LoaderThrows));
        assert!(e.contains(HotSpotAlignment::CatchTypesAtLink));
        assert_eq!(d.edited("none"), HotSpotAlignments::NONE);
        assert_eq!(d.edited("none,all"), HotSpotAlignments::all());
        assert_eq!(d.edited("1"), HotSpotAlignments::all());
        assert_eq!(d.edited("0"), HotSpotAlignments::NONE);
        let one = d.edited("none,namespaced-array-types");
        assert_eq!(one.tokens(), vec!["namespaced-array-types"]);
        assert_eq!(d.edited("no-such-row"), d);
    }

    #[test]
    fn the_census_line_counts_marks_uncounted_and_off_rows() {
        let census = AlignmentCensus::new();
        census.note(HotSpotAlignment::RuntimeImageNames);
        census.note(HotSpotAlignment::RuntimeImageNames);
        let set = HotSpotAlignments::defaults().with(HotSpotAlignment::LoaderThrows, false);
        let line = census.census_line(CompatibilityMode::Compatible, set);
        assert!(line.starts_with("mode=compatible "), "{line}");
        assert!(line.contains(" runtime-image-names=2"), "{line}");
        assert!(line.contains(" loader-throws=0/off"), "{line}");
        assert!(line.contains(" module-messages=?"), "{line}");
        let jdk = census.census_line(CompatibilityMode::JdkOnly, HotSpotAlignments::NONE);
        assert!(jdk.starts_with("mode=jdk-only "), "{jdk}");
        assert!(!jdk.contains("/off"), "{jdk}");
    }

    /// nio20 wave 5 (L1): the server row is real-JDK-only and reachable by its
    /// token; default ON since nio21 wave 13 (L4).
    #[test]
    fn jdk_http_server_is_a_default_on_real_jdk_row() {
        let a = HotSpotAlignment::JdkHttpServer;
        assert!(a.default_on());
        assert!(a.needs_real_jdk());
        assert!(HotSpotAlignments::defaults().contains(a));
        assert!(!HotSpotAlignments::defaults()
            .edited("-jdk-http-server")
            .contains(a));
        assert_eq!(HotSpotAlignment::from_token("jdk-http-server"), Some(a));
    }

    /// nio21 wave 1 (L4): the `ConcurrentHashMap` / `Properties` row is
    /// real-JDK-only, takes bit 64 (dev's interpreter round i3 wave 76 row of
    /// the same token, which the nio21 dev merge kept), and is reachable by its token in both
    /// directions; on by default since nio21 wave 4 (L1).
    #[test]
    fn jdk_concurrent_hash_map_is_a_default_on_real_jdk_row() {
        let a = HotSpotAlignment::JdkConcurrentHashMap;
        assert_eq!(a as usize, 64);
        assert!(a.default_on());
        assert!(a.needs_real_jdk());
        assert!(!a.counted());
        assert!(HotSpotAlignments::defaults().contains(a));
        let off = HotSpotAlignments::defaults().edited("-jdk-concurrent-hash-map");
        assert!(!off.contains(a));
        assert!(off.edited("+jdk-concurrent-hash-map").contains(a));
        assert_eq!(
            HotSpotAlignment::from_token("jdk-concurrent-hash-map"),
            Some(a)
        );
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        // Its sibling, the scalar atomics: bit 67, on by default since nio21
        // wave 3 (L1), and its token still reverts it.
        let b = HotSpotAlignment::JdkScalarAtomics;
        assert_eq!(b as usize, 67);
        assert!(b.default_on() && b.needs_real_jdk() && !b.counted());
        assert!(HotSpotAlignments::defaults().contains(b));
        assert!(!HotSpotAlignments::defaults()
            .edited("-jdk-scalar-atomics")
            .contains(b));
        assert_eq!(HotSpotAlignment::from_token("jdk-scalar-atomics"), Some(b));
    }

    /// nio21 wave 2 (L4): the six `HttpClient`-path families take bits 68,
    /// 69, 66 (dev's row of the same token), 70, 71, 72 since the nio21 dev
    /// merge
    /// (past a `u64`, so the set's width is part of the test), are on by
    /// default, need a real JDK, and each reverts alone by its token.
    #[test]
    fn r21w2_http_path_family_rows_are_on_and_revert_alone() {
        let rows = [
            (HotSpotAlignment::JdkTreeMap, 68, "jdk-tree-map"),
            (
                HotSpotAlignment::JdkLinkedCollections,
                69,
                "jdk-linked-collections",
            ),
            (
                HotSpotAlignment::JdkCompletableFuture,
                66,
                "jdk-completable-future",
            ),
            (HotSpotAlignment::JdkTimeUnit, 70, "jdk-time-unit"),
            (HotSpotAlignment::JdkAdders, 71, "jdk-adders"),
            (
                HotSpotAlignment::JdkCarrierThreadLocals,
                72,
                "jdk-carrier-thread-locals",
            ),
        ];
        let d = HotSpotAlignments::defaults();
        for (a, bit, token) in rows {
            assert_eq!(a as usize, bit, "{token}");
            assert_eq!(a.token(), token);
            assert_eq!(HotSpotAlignment::from_token(token), Some(a));
            assert!(
                a.default_on() && a.needs_real_jdk() && !a.counted(),
                "{token}"
            );
            assert!(d.contains(a), "{token}");
            let off = d.edited(&format!("-{token}"));
            assert!(!off.contains(a), "{token}");
            // Only that row moved.
            for (b, _, other) in rows {
                if b != a {
                    assert!(off.contains(b), "-{token} turned {other} off");
                }
            }
            assert!(
                off.contains(HotSpotAlignment::JdkScalarAtomics)
                    == d.contains(HotSpotAlignment::JdkScalarAtomics)
            );
        }
        // The census has a slot for every row.
        let census = AlignmentCensus::new();
        census.note(HotSpotAlignment::JdkCarrierThreadLocals);
        assert_eq!(census.count(HotSpotAlignment::JdkCarrierThreadLocals), 1);
        assert!(HotSpotAlignments::all().contains(HotSpotAlignment::JdkCarrierThreadLocals));
        assert!(!HotSpotAlignments::NONE.contains(HotSpotAlignment::JdkTreeMap));
    }

    /// nio21 wave 3 (L1): four more `HttpClient`-path rows take bits 73-76,
    /// and wave 4 (L1) the `SharedSecrets` getter bit 77 (since the nio21 dev merge); all are on by
    /// default since wave 4, need a real JDK, are registration-time, and each
    /// toggles alone by its token.
    #[test]
    fn r21w3_http_path_rows_take_bits_66_to_69() {
        let rows = [
            (
                HotSpotAlignment::JdkThreadInterrupt,
                73,
                "jdk-thread-interrupt",
                true,
            ),
            (
                HotSpotAlignment::JdkUnsafeDelegations,
                74,
                "jdk-unsafe-delegations",
                true,
            ),
            (HotSpotAlignment::JdkUri, 75, "jdk-uri", true),
            (
                HotSpotAlignment::JdkByteArrayStreams,
                76,
                "jdk-byte-array-streams",
                true,
            ),
            (
                HotSpotAlignment::JdkSharedSecrets,
                77,
                "jdk-shared-secrets",
                true,
            ),
            (
                HotSpotAlignment::JdkSunMiscUnsafe,
                81,
                "jdk-sun-misc-unsafe",
                true,
            ),
            (HotSpotAlignment::JdkForeign, 82, "jdk-foreign", true),
            (
                HotSpotAlignment::JdkAtomicArrays,
                83,
                "jdk-atomic-arrays",
                true,
            ),
            (
                HotSpotAlignment::JdkJavacFileManager,
                84,
                "jdk-javac-file-manager",
                true,
            ),
        ];
        let d = HotSpotAlignments::defaults();
        for (a, bit, token, on) in rows {
            assert_eq!(a as usize, bit, "{token}");
            assert_eq!(HotSpotAlignment::ALL[bit], a, "{token}");
            assert_eq!(HotSpotAlignment::from_token(token), Some(a));
            assert_eq!(a.default_on(), on, "{token}");
            assert!(a.needs_real_jdk() && !a.counted(), "{token}");
            assert_eq!(d.contains(a), on, "{token}");
            let flipped = d.edited(&format!("{}{token}", if on { "-" } else { "+" }));
            assert_eq!(flipped.contains(a), !on, "{token}");
            for (b, _, other, _) in rows {
                if b != a {
                    assert_eq!(flipped.contains(b), d.contains(b), "{token} moved {other}");
                }
            }
            assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        }
        // 81 since the interpreter round i3 waves 81-83 dev merge
        // (`JdkArchiveFs` is bit 79, `JdkThreadPools` bit 80); 82 with
        // `JdkSunMiscUnsafe` (bit 81, nio21 wave 13); 84 with `JdkAtomicArrays` (bit 83);
        // 85 with `JdkJavacFileManager` (bit 84, JIT round 20 wave 14, lane cjavac);
        // 86 with `JdkForeignSegments` (bit 85, JIT round 20 wave 19, lane
        // yCompatSeg). The round-20 rows moved up one bit at the dev merge.
        // 90 with the four L4 check families (bits 86-89, interpreter round
        // i6 wave 118, lane L4).
        // 93 with the three wave-49 registration-time switches (bits 90-92,
        // interpreter round i6 wave 118, lane L5).
        // 94 with `JdkHashtable` (bit 93, interpreter round i6 wave 119, lane L5).
        // 95 with `JdkLogging` (bit 94, interpreter round i6 wave 120, lane L5).
        // 96 with `JdkStringGetBytes` (bit 95, interpreter round i6 wave 122,
        // lane L1).
        // 97 with `JdkMemoryUsage` (bit 96, interpreter round i6 wave 123,
        // lane L1).
        // 98 with `JdkManagementBeans` (bit 97, interpreter round i6 wave 124,
        // lane L1).
        // 99 with `JdkTypeVariableImpl` (bit 98, interpreter round i6 wave
        // 125, lane L1).
        // 100 with `JdkPriorityQueue` (bit 99, interpreter round i6 wave 133,
        // lane L6).
        // 101 with `JdkVmInitLevel` (bit 100, JIT round j27 wave 5, lane
        // VMALIGN).
        assert_eq!(HotSpotAlignment::ALL.len(), 101);
    }

    /// JIT round j27 wave 5, lane VMALIGN: the `VM.initLevel` / `isBooted`
    /// row is bit 100, default ON, a real-JDK row, uncounted, with no legacy
    /// variable (its token reverts it alone), not graduated, and in force
    /// under `--jdk-only`.
    #[test]
    fn jdk_vm_init_level_is_a_default_on_real_jdk_row_at_bit_100() {
        let a = HotSpotAlignment::JdkVmInitLevel;
        assert_eq!(a as usize, 100);
        assert_eq!(HotSpotAlignment::ALL[100], a);
        assert_eq!(a.token(), "jdk-vm-init-level");
        assert_eq!(HotSpotAlignment::from_token("jdk-vm-init-level"), Some(a));
        assert_eq!(a.legacy_key(), None);
        assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!a.graduated());
        let d = HotSpotAlignments::defaults();
        assert!(d.contains(a));
        let off = d.edited("-jdk-vm-init-level");
        assert!(!off.contains(a));
        assert_eq!(off.with(a, true), d, "only this row moves");
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        assert!(!off.aligned(CompatibilityMode::Compatible, a));
    }

    /// Interpreter round i6 wave 133, lane L6: the `PriorityQueue` family row
    /// is bit 99, default OFF, a real-JDK row, uncounted, turned on by its own
    /// variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_priority_queue_is_a_default_off_real_jdk_row_at_bit_99() {
        let a = HotSpotAlignment::JdkPriorityQueue;
        assert_eq!(a as usize, 99);
        assert_eq!(HotSpotAlignment::ALL[99], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-priority-queue"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_PRIORITY_QUEUE"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!a.graduated());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-priority-queue").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 125, lane L1: the `TypeVariableImpl` family
    /// row is bit 98, default OFF, a real-JDK row, uncounted, turned on by its
    /// own variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_type_variable_impl_is_a_default_off_real_jdk_row_at_bit_98() {
        let a = HotSpotAlignment::JdkTypeVariableImpl;
        assert_eq!(a as usize, 98);
        assert_eq!(HotSpotAlignment::ALL[98], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-type-variable-impl"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-type-variable-impl").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 124, lane L1: the `sun.management` memory
    /// beans' family row is bit 97, default OFF, a real-JDK row, uncounted,
    /// turned on by its own variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_management_beans_is_a_default_off_real_jdk_row_at_bit_97() {
        let a = HotSpotAlignment::JdkManagementBeans;
        assert_eq!(a as usize, 97);
        assert_eq!(HotSpotAlignment::ALL[97], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-management-beans"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-management-beans").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 123, lane L1: the `MemoryUsage` family row
    /// is bit 96, default OFF, a real-JDK row, uncounted, turned on by its own
    /// variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_memory_usage_is_a_default_off_real_jdk_row_at_bit_96() {
        let a = HotSpotAlignment::JdkMemoryUsage;
        assert_eq!(a as usize, 96);
        assert_eq!(HotSpotAlignment::ALL[96], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-memory-usage"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_MEMORY_USAGE"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-memory-usage").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 122, lane L1: the `String.getBytes` family
    /// row is bit 95, default OFF, a real-JDK row, uncounted, turned on by its
    /// own variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_string_get_bytes_is_a_default_off_real_jdk_row_at_bit_95() {
        let a = HotSpotAlignment::JdkStringGetBytes;
        assert_eq!(a as usize, 95);
        assert_eq!(HotSpotAlignment::ALL[95], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-string-get-bytes"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_STRING_GET_BYTES"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-string-get-bytes").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 120, lane L5: the `java.util.logging` family
    /// row is bit 94, default OFF, a real-JDK row, uncounted, turned on by its
    /// own variable or its token, in force under `--jdk-only`.
    #[test]
    fn jdk_logging_is_a_default_off_real_jdk_row_at_bit_94() {
        let a = HotSpotAlignment::JdkLogging;
        assert_eq!(a as usize, 94);
        assert_eq!(HotSpotAlignment::ALL[94], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-logging"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_LOGGING"));
        assert!(!a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(!HotSpotAlignments::defaults().contains(a));
        assert!(HotSpotAlignments::defaults().edited("+jdk-logging").contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 119, lane L5: the `Hashtable` family row is
    /// bit 93, default ON since interpreter round i7 wave 144 (A/B w142lto),
    /// a real-JDK row, uncounted, turned off by its own variable (`=0`) or its
    /// token, and in force under `--jdk-only` whatever the set says.
    #[test]
    fn jdk_hashtable_is_a_default_on_real_jdk_row_at_bit_93() {
        let a = HotSpotAlignment::JdkHashtable;
        assert_eq!(a as usize, 93);
        assert_eq!(HotSpotAlignment::ALL[93], a);
        assert_eq!(HotSpotAlignment::from_token("jdk-hashtable"), Some(a));
        assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_HASHTABLE"));
        assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(HotSpotAlignments::defaults().contains(a));
        assert!(!HotSpotAlignments::defaults().edited("-jdk-hashtable").contains(a));
        assert!(!HotSpotAlignments::from_lookup(|k| {
            (k == "CRATONVM_COMPAT_JDK_HASHTABLE").then(|| "0".to_string())
        })
        .contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }

    /// Interpreter round i6 wave 118, lane L4 (`i50-L4-proposal-a-per-vm-table-of-hotspot-checks`):
    /// the four check families are rows 86-89, on by default, not real-JDK
    /// rows, uncounted, in force under `--jdk-only` whatever the set says, and
    /// each one's old `*_EVERY_MODE` variable is its legacy key (`=0` turns
    /// that row alone off).
    #[test]
    fn the_four_l4_check_families_are_rows_86_to_89() {
        let rows = [
            (HotSpotAlignment::IndyLinkChecks, 86, "indy-link-checks", "CRATONVM_INDY_CHECKS_EVERY_MODE"),
            (HotSpotAlignment::SelectionChecks, 87, "selection-checks", "CRATONVM_SELECTION_CHECKS_EVERY_MODE"),
            (
                HotSpotAlignment::MhCombinatorChecks,
                88,
                "mh-combinator-checks",
                "CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE",
            ),
            (
                HotSpotAlignment::LdcHandleAccessChecks,
                89,
                "ldc-handle-access-checks",
                "CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE",
            ),
        ];
        let d = HotSpotAlignments::defaults();
        for (a, bit, token, key) in rows {
            assert_eq!(a as usize, bit, "{token}");
            assert_eq!(HotSpotAlignment::ALL[bit], a, "{token}");
            assert_eq!(HotSpotAlignment::from_token(token), Some(a));
            assert_eq!(a.legacy_key(), Some(key), "{token}");
            assert!(a.default_on() && !a.needs_real_jdk() && !a.counted(), "{token}");
            assert!(d.contains(a), "{token}");
            assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
            let only = move |k: &str| (k == key).then(|| "0".to_string());
            let off = HotSpotAlignments::from_lookup(only);
            assert!(!off.contains(a), "{token}");
            for (b, _, other, _) in rows {
                if b != a {
                    assert!(off.contains(b), "{key}=0 moved {other}");
                }
            }
        }
    }

    /// JIT round 20 wave 19 (lane yCompatSeg, MEM20-3): the pointer-segment
    /// row takes bit 85, is on by default, needs a real JDK, is COUNTED (a
    /// call-time row), reverts alone by its token, and is in force under
    /// `--jdk-only` whatever the set says.
    #[test]
    fn jdk_foreign_segments_is_a_default_on_counted_row_at_bit_85() {
        let a = HotSpotAlignment::JdkForeignSegments;
        assert_eq!(a as usize, 85);
        assert_eq!(HotSpotAlignment::ALL[85], a);
        assert_eq!(
            HotSpotAlignment::from_token("jdk-foreign-segments"),
            Some(a)
        );
        assert!(a.default_on() && a.needs_real_jdk() && a.counted());
        let d = HotSpotAlignments::defaults();
        assert!(d.contains(a) && d.contains(HotSpotAlignment::JdkForeign));
        let off = d.edited("-jdk-foreign-segments");
        assert!(!off.contains(a));
        assert!(
            off.contains(HotSpotAlignment::JdkForeign),
            "-jdk-foreign-segments moved jdk-foreign"
        );
        assert!(
            d.edited("-jdk-foreign").contains(a),
            "-jdk-foreign moved jdk-foreign-segments"
        );
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        assert!(d.aligned(CompatibilityMode::Compatible, a));
    }

    /// nio21 wave 12 (L3): the `java.lang.invoke` row takes bit 78 (75 before
    /// the nio21 dev merge), is opt-in,
    /// needs a real JDK, is registration-time, and toggles by its token (default ON since 2026-10-09).
    #[test]
    fn jdk_invoke_is_a_default_on_real_jdk_row_at_bit_78() {
        let a = HotSpotAlignment::JdkInvoke;
        assert_eq!(a as usize, 78);
        assert_eq!(HotSpotAlignment::ALL[78], a);
        assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
        assert!(HotSpotAlignments::defaults().contains(a));
        assert!(!HotSpotAlignments::defaults()
            .edited("-jdk-invoke")
            .contains(a));
        assert_eq!(HotSpotAlignment::from_token("jdk-invoke"), Some(a));
        assert!(HotSpotAlignments::all().contains(a));
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        assert!(HotSpotAlignments::defaults().aligned(CompatibilityMode::Compatible, a));
    }

    #[test]
    fn jdk_only_has_every_row() {
        for a in HotSpotAlignment::ALL {
            assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
            assert!(!HotSpotAlignments::NONE.aligned(CompatibilityMode::Compatible, a));
        }
    }

    #[test]
    fn a_legacy_variable_still_turns_its_row_off() {
        // A plain lookup, not `with_thread_overrides`: the flags module's own
        // tests count live overrides process-wide.
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let set = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_LOADER_THROWS", "0"));
        assert!(!set.contains(HotSpotAlignment::LoaderThrows));
        assert!(set.contains(HotSpotAlignment::LoaderConstraints));
        let set = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-loader-constraints"));
        assert!(!set.contains(HotSpotAlignment::LoaderConstraints));
        // The list is read after the legacy variables.
        let set = HotSpotAlignments::from_lookup(|key: &str| match key {
            "CRATONVM_COMPAT_LOADER_THROWS" => Some("0".to_string()),
            COMPAT_ALIGN_KEY => Some("+loader-throws".to_string()),
            _ => None,
        });
        assert!(set.contains(HotSpotAlignment::LoaderThrows));
    }

    /// Interpreter round i2 wave 69, lane L1: the `LockSupport` row is on by
    /// default, needs a real JDK, and its own variable is its `=0` kill switch.
    #[test]
    fn the_lock_support_row_is_on_and_has_a_kill_switch() {
        let row = HotSpotAlignment::JdkLockSupport;
        assert!(HotSpotAlignments::defaults().contains(row));
        assert!(row.needs_real_jdk());
        assert_eq!(HotSpotAlignment::from_token("jdk-lock-support"), Some(row));
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_LOCK_SUPPORT", "0"));
        assert!(!off.contains(row));
        assert!(off.contains(HotSpotAlignment::JdkPreconditions));
        let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-lock-support"));
        assert!(!off.contains(row));
    }

    /// Interpreter round i2 wave 70, lane L1: the `StampedLock` row is on by
    /// default, needs a real JDK, and its own variable is its `=0` kill switch.
    #[test]
    fn the_stamped_lock_row_is_on_and_has_a_kill_switch() {
        let row = HotSpotAlignment::JdkStampedLock;
        assert_eq!(row as usize, 43);
        assert!(HotSpotAlignments::defaults().contains(row));
        assert!(row.needs_real_jdk());
        assert_eq!(HotSpotAlignment::from_token("jdk-stamped-lock"), Some(row));
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_STAMPED_LOCK", "0"));
        assert!(!off.contains(row));
        assert!(off.contains(HotSpotAlignment::JdkLockSupport));
        let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-stamped-lock"));
        assert!(!off.contains(row));
        assert!(off.contains(HotSpotAlignment::JdkLockSupport));
    }

    /// Interpreter round i3 wave 71, lane L1: the `CountDownLatch` row needs a
    /// real JDK, its own variable turns it on (`1`) and off (`0`) whatever the
    /// default, and so does a `CRATONVM_COMPAT_ALIGN` item; neither touches
    /// the neighbouring lock rows. (No assertion on the default itself.)
    #[test]
    fn the_count_down_latch_row_has_its_own_switch() {
        let row = HotSpotAlignment::JdkCountDownLatch;
        assert_eq!(row as usize, 59);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(
            HotSpotAlignment::from_token("jdk-count-down-latch"),
            Some(row)
        );
        assert_eq!(
            row.legacy_key(),
            Some("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH")
        );
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let on = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH", "1"));
        assert!(on.contains(row));
        let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH", "0"));
        assert!(!off.contains(row));
        assert_eq!(
            off.contains(HotSpotAlignment::JdkStampedLock),
            HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkStampedLock)
        );
        let on = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "+jdk-count-down-latch"));
        assert!(on.contains(row));
        let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-count-down-latch"));
        assert!(!off.contains(row));
    }

    /// Interpreter round i3 wave 72, lane L1: the `AbstractQueuedLongSynchronizer`
    /// and `CopyOnWriteArrayList` rows need a real JDK, are registration-time,
    /// and each has its own variable (`1` on, `0` off, whatever the default)
    /// and token; neither variable touches the other row or the latch row.
    /// (No assertion on either default.)
    #[test]
    fn the_aqls_and_copy_on_write_rows_have_their_own_switches() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        for (row, bit, token, key) in [
            (
                HotSpotAlignment::JdkAqls,
                60usize,
                "jdk-aqls",
                "CRATONVM_COMPAT_JDK_AQLS",
            ),
            (
                HotSpotAlignment::JdkCopyOnWrite,
                61usize,
                "jdk-copy-on-write",
                "CRATONVM_COMPAT_JDK_COPY_ON_WRITE",
            ),
        ] {
            assert_eq!(row as usize, bit);
            assert!(row.needs_real_jdk());
            assert!(!row.counted());
            assert_eq!(HotSpotAlignment::from_token(token), Some(row));
            assert_eq!(row.legacy_key(), Some(key));
            assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
            let off = HotSpotAlignments::from_lookup(only(key, "0"));
            assert!(!off.contains(row));
            assert_eq!(
                off.contains(HotSpotAlignment::JdkCountDownLatch),
                HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCountDownLatch)
            );
            let plus = format!("+{token}");
            let minus = format!("-{token}");
            assert!(HotSpotAlignments::NONE.edited(&plus).contains(row));
            assert!(!HotSpotAlignments::all().edited(&minus).contains(row));
        }
        let aqls_off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_AQLS", "0"));
        assert_eq!(
            aqls_off.contains(HotSpotAlignment::JdkCopyOnWrite),
            HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCopyOnWrite)
        );
    }

    /// Interpreter round i3 wave 74, lane L1: the `CopyOnWriteArraySet` row is
    /// bit 62, needs a real JDK, is registration-time, and has its own
    /// variable (`1` on, `0` off, whatever the default) and token; neither
    /// touches the `CopyOnWriteArrayList` row, nor that row's variable this
    /// one. (No assertion on either default.)
    #[test]
    fn the_copy_on_write_set_row_has_its_own_switch() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let row = HotSpotAlignment::JdkCopyOnWriteArraySet;
        let key = "CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET";
        assert_eq!(row as usize, 62);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(
            HotSpotAlignment::from_token("jdk-copy-on-write-set"),
            Some(row)
        );
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(!off.contains(row));
        assert_eq!(
            off.contains(HotSpotAlignment::JdkCopyOnWrite),
            HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCopyOnWrite)
        );
        let list_key = "CRATONVM_COMPAT_JDK_COPY_ON_WRITE";
        let list_on = HotSpotAlignments::from_lookup(only(list_key, "1"));
        assert_eq!(
            list_on.contains(row),
            HotSpotAlignments::defaults().contains(row)
        );
        assert!(HotSpotAlignments::NONE
            .edited("+jdk-copy-on-write-set")
            .contains(row));
        assert!(!HotSpotAlignments::all()
            .edited("-jdk-copy-on-write-set")
            .contains(row));
        // `jdk-copy-on-write` is a prefix of this token, not an alias of it.
        assert!(!HotSpotAlignments::NONE
            .edited("+jdk-copy-on-write")
            .contains(row));
    }

    /// Interpreter round i3 wave 75, lane L1: the `PriorityBlockingQueue` row
    /// is bit 63 -- the top bit of the old `u64` set, so `all()` / `NONE` and
    /// the edits must reach it without a wrapped shift -- needs a real JDK, is
    /// registration-time, and has its own variable (`1` on, `0` off, whatever
    /// the default) and token; it moves no other row. (No assertion on its
    /// default.) Wave 76 widened the set to a `u128`; the row keeps bit 63.
    #[test]
    fn the_priority_blocking_queue_row_is_the_top_bit_and_has_its_own_switch() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let row = HotSpotAlignment::JdkPriorityBlockingQueue;
        let key = "CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE";
        assert_eq!(row as usize, 63);
        assert!(HotSpotAlignment::ALL.into_iter().count() >= 64);
        assert_eq!(row.bit(), 1u128 << 63);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(
            HotSpotAlignment::from_token("jdk-priority-blocking-queue"),
            Some(row)
        );
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::all().contains(row));
        assert!(!HotSpotAlignments::NONE.contains(row));
        // Every row's bit is set in `all()`, and no other bit.
        let every = HotSpotAlignment::ALL
            .into_iter()
            .fold(0u128, |b, a| b | a.bit());
        assert_eq!(HotSpotAlignments::all(), HotSpotAlignments(every));
        assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
        let on = HotSpotAlignments::from_lookup(only(key, "1"));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(!off.contains(row));
        // Only this row moves.
        assert_eq!(on.with(row, false), off);
        let default_on = HotSpotAlignments::defaults().contains(row);
        assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
        assert!(HotSpotAlignments::NONE
            .edited("+jdk-priority-blocking-queue")
            .contains(row));
        assert!(!HotSpotAlignments::all()
            .edited("-jdk-priority-blocking-queue")
            .contains(row));
        assert_eq!(
            HotSpotAlignments::NONE
                .edited("+jdk-priority-blocking-queue")
                .tokens(),
            vec!["jdk-priority-blocking-queue"]
        );
        // The census line has a slot for it (one counter per row).
        let census = AlignmentCensus::new();
        assert_eq!(census.count(row), 0);
        let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
        assert!(
            line.contains(" jdk-priority-blocking-queue=?/off"),
            "{line}"
        );
    }

    /// Interpreter round i3 wave 76, lane L1: the `ConcurrentHashMap` row is
    /// bit 64 -- the first past the old `u64` -- needs a real JDK, is not
    /// counted, and has its own variable (`1` on, `0` off, whatever the
    /// default) and token; it moves no other row, and `all()` / `NONE` / the
    /// list edits reach it. (No assertion on its default.)
    #[test]
    fn the_concurrent_hash_map_row_is_bit_64_and_has_its_own_switch() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let row = HotSpotAlignment::JdkConcurrentHashMap;
        let key = "CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP";
        assert_eq!(row as usize, 64);
        assert_eq!(row.bit(), 1u128 << 64);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(
            HotSpotAlignment::from_token("jdk-concurrent-hash-map"),
            Some(row)
        );
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::all().contains(row));
        assert!(!HotSpotAlignments::NONE.contains(row));
        let on = HotSpotAlignments::from_lookup(only(key, "1"));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(on.contains(row));
        assert!(!off.contains(row));
        assert_eq!(on.with(row, false), off);
        let default_on = HotSpotAlignments::defaults().contains(row);
        assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
        // The bit-63 neighbour does not move with it.
        assert_eq!(
            on.contains(HotSpotAlignment::JdkPriorityBlockingQueue),
            HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkPriorityBlockingQueue)
        );
        assert_eq!(
            HotSpotAlignments::NONE
                .edited("+jdk-concurrent-hash-map")
                .tokens(),
            vec!["jdk-concurrent-hash-map"]
        );
        assert!(!HotSpotAlignments::all()
            .edited("-jdk-concurrent-hash-map")
            .contains(row));
        let census = AlignmentCensus::new();
        census.note(row);
        assert_eq!(census.count(row), 1);
        let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
        // Not the last item since wave 80 (`jdk-completable-future` is bit 66).
        assert!(line.contains(" jdk-concurrent-hash-map=?/off "), "{line}");
    }

    /// Interpreter round i3 wave 80, lane L1: the `CompletableFuture` row is
    /// bit 66, needs a real JDK, is not counted, and has its own variable (`1`
    /// on, `0` off, whatever the default) and token; it moves no other row.
    /// (No assertion on its default.)
    #[test]
    fn the_completable_future_row_is_bit_66_and_has_its_own_switch() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let row = HotSpotAlignment::JdkCompletableFuture;
        let key = "CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE";
        assert_eq!(row as usize, 66);
        assert_eq!(row.bit(), 1u128 << 66);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(
            HotSpotAlignment::from_token("jdk-completable-future"),
            Some(row)
        );
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::all().contains(row));
        assert!(!HotSpotAlignments::NONE.contains(row));
        let on = HotSpotAlignments::from_lookup(only(key, "1"));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(on.contains(row));
        assert!(!off.contains(row));
        assert_eq!(on.with(row, false), off);
        let default_on = HotSpotAlignments::defaults().contains(row);
        assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
        assert_eq!(
            HotSpotAlignments::NONE
                .edited("+jdk-completable-future")
                .tokens(),
            vec!["jdk-completable-future"]
        );
        assert!(!HotSpotAlignments::all()
            .edited("-jdk-completable-future")
            .contains(row));
        let census = AlignmentCensus::new();
        let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
        // Not the last item since the nio21 dev merge (the nio21 rows follow).
        assert!(line.contains(" jdk-completable-future=?/off "), "{line}");
    }

    /// Interpreter round i3 wave 82, lane L1: the thread-pool row is bit 80 (it was 67; the nio21 rows took 67..79 at the waves 81-83 dev merge),
    /// needs a real JDK, is not counted, and has its own variable (`1` on, `0`
    /// off, whatever the default) and token; it moves no other row. (No
    /// assertion on its default.)
    #[test]
    fn the_thread_pools_row_is_bit_80_and_has_its_own_switch() {
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let row = HotSpotAlignment::JdkThreadPools;
        let key = "CRATONVM_COMPAT_JDK_THREAD_POOLS";
        assert_eq!(row as usize, 80);
        assert_eq!(row.bit(), 1u128 << 80);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(HotSpotAlignment::from_token("jdk-thread-pools"), Some(row));
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::all().contains(row));
        assert!(!HotSpotAlignments::NONE.contains(row));
        let on = HotSpotAlignments::from_lookup(only(key, "1"));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(on.contains(row));
        assert!(!off.contains(row));
        assert_eq!(on.with(row, false), off);
        let default_on = HotSpotAlignments::defaults().contains(row);
        assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
        assert_eq!(
            HotSpotAlignments::NONE.edited("+jdk-thread-pools").tokens(),
            vec!["jdk-thread-pools"]
        );
        assert!(!HotSpotAlignments::all()
            .edited("-jdk-thread-pools")
            .contains(row));
        let census = AlignmentCensus::new();
        let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
        assert!(line.contains(" jdk-thread-pools=?/off"), "{line}");
    }

    /// Interpreter round i3 wave 76, lane L1: the widening to a `u128` is a
    /// pure refactor. Each row's bit is its discriminant, `defaults()` is
    /// exactly the rows whose `default_on` says so, `all()` / `NONE` and the
    /// list words are the full and the empty set, each row's `+token` /
    /// `-token` and legacy `1` / `0` move that row alone, and the census has
    /// one counter and one exit-line item per row.
    #[test]
    fn the_widened_set_answers_every_row_as_the_u64_did() {
        assert!(HotSpotAlignment::ALL.into_iter().count() <= ALIGNMENT_SET_CAPACITY);
        assert_eq!(ALIGNMENT_SET_CAPACITY, 128);
        let only =
            |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
        let d = HotSpotAlignments::defaults();
        for (i, a) in HotSpotAlignment::ALL.into_iter().enumerate() {
            assert_eq!(a.bit(), 1u128 << i);
            assert_eq!(d.contains(a), a.default_on(), "{}", a.token());
            assert!(HotSpotAlignments::all().contains(a));
            assert!(!HotSpotAlignments::NONE.contains(a));
            let plus = format!("+{}", a.token());
            let minus = format!("-{}", a.token());
            assert_eq!(d.edited(&plus), d.with(a, true), "{}", a.token());
            assert_eq!(d.edited(&minus), d.with(a, false), "{}", a.token());
            assert_eq!(
                HotSpotAlignments::NONE.edited(&plus).tokens(),
                vec![a.token()]
            );
            if let Some(key) = a.legacy_key() {
                assert_eq!(
                    HotSpotAlignments::from_lookup(only(key, "1")),
                    d.with(a, true)
                );
                assert_eq!(
                    HotSpotAlignments::from_lookup(only(key, "0")),
                    d.with(a, false)
                );
            }
        }
        assert_eq!(d.edited("all"), HotSpotAlignments::all());
        assert_eq!(d.edited("none"), HotSpotAlignments::NONE);
        assert_eq!(
            HotSpotAlignments::all().tokens().len(),
            HotSpotAlignment::ALL.len()
        );
        let census = AlignmentCensus::new();
        for a in HotSpotAlignment::ALL {
            census.note(a);
            assert_eq!(census.count(a), 1);
        }
        let line = census.census_line(CompatibilityMode::JdkOnly, HotSpotAlignments::NONE);
        assert_eq!(
            line.split(' ').count(),
            1 + HotSpotAlignment::ALL.len(),
            "{line}"
        );
    }
}
