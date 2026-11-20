#[allow(unused_imports)]
use super::*;

/// The families `--compatible` can adopt, one row each. Adding a family is one
/// entry here and one [`HotSpotAlignment`](cratonvm_types::HotSpotAlignment)
/// variant; no dispatch code changes.
pub(crate) const COMPAT_FAMILIES: &[CompatFamily] = &[
    // `ForkJoinPool` and the task classes the SBR wave-4 lane FJP retired.
    // `Runtime`, `StackWalker` and `Signal` rows of the same tables are not
    // this family's and stay as they are.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkForkJoinPool,
        classes: &[
            "java/util/concurrent/ForkJoinPool",
            "java/util/concurrent/ForkJoinTask",
            "java/util/concurrent/RecursiveAction",
            "java/util/concurrent/RecursiveTask",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // The JCA family SBR waves 1-5 (lanes JCA, JCA2, CRYPTOENG, DISPATCH, ...)
    // retired: `Security`, `Provider`, `GetInstance`, `MessageDigest`,
    // `Signature`, `KeyFactory`, `Cipher`, `Mac`, ... Measured on
    // `Sbr4CompatFamilies` with `CRATONVM_REAL=jca` (the dispatch-time spelling
    // of the same scope): `Security.getProvider("SUN")` is the installed
    // `sun.security.provider.Sun`, one object per name, and every engine's
    // `getProvider()` is the installed provider. `sun/security/jca/` and
    // `JceSecurity` carry the `<clinit>` no-ops that keep `Security` / `Providers`
    // from ever building a `ProviderList` on `--compatible`. The JSSE handshake
    // half (`sun/security/ssl/`) stays out: `--compatible`'s `SSLContext` /
    // `SSLEngine` are native carriers, and the real `SSLEngineImpl` cannot run
    // on a carrier (`conContext` is null). The keystore engines stay native for
    // the same reason: those carriers read their key and certificate from the
    // native keystore tables (`RSslLiveSession`, `RSslEndpointIdentification`:
    // `No TLS key/cert configured` with `KeyStore` real).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkProviders,
        classes: &[
            "java/security/",
            "javax/crypto/",
            "sun/security/",
            "com/sun/crypto/provider/",
        ],
        stub_classes: &[
            "java/security/",
            "javax/crypto/",
            "sun/security/",
            "com/sun/crypto/provider/",
        ],
        bridges: &[],
        except: &[
            "sun/security/ssl/",
            "java/security/KeyStore",
            "sun/security/pkcs12/",
            "sun/security/provider/JavaKeyStore",
        ],
    },
    // The boot module graph SBR round 2 waves 7 and 9 (lanes MODULE, MODLOAD)
    // retired: `ModuleLayer.boot` / `configuration` / `findModule` / `modules`,
    // `Configuration.modules`, `ModuleReferenceImpl.open` and the
    // `ModuleDescriptor` accessors. With the row, `ModuleLayer.boot()` is the
    // JDK's own `System.bootLayer` read and `finish_boot_layer` builds the real
    // boot `Configuration` over real `ModuleReference`s (module-path modules
    // included), so `boot().configuration().findModule("m")` finds a
    // `--module-path` module. No companion stubs: the loaders half of the JDK
    // model (`real_builtin_loaders_enabled`) stays `--real-jdk`'s.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkModuleGraph,
        // `java/lang/Module` itself is not in the family: its retired
        // `getResourceAsStream` runs the JDK body through the built-in loaders,
        // which are `--real-jdk`'s model.
        classes: &[
            "java/lang/ModuleLayer",
            "java/lang/module/",
            "jdk/internal/module/",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // The `java.util` hash collections lane 1 retired as ONE set (waves 3 and
    // 9, round 13 wave 7: `RETIRED_SHADOW_L1_HM_TRIPLES`, `..._L1_LHM_TRIPLES`,
    // `..._L1_HS_TRIPLES`, the sequenced-view tables), because a view carrier,
    // an iterator and the map share one backing
    // (`the_l1_hashmap_table_is_the_hashmap_family_and_only_it`). The prefixes
    // take the classes and their `$` views, iterators and nodes. No companion
    // stubs: the one `is_jdk_only()` gate the family had, the integer overlay,
    // asks `aligned(JdkHashCollections)` now. JIT round 16 (hcb).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkHashCollections,
        classes: &[
            "java/util/HashMap",
            "java/util/HashSet",
            "java/util/LinkedHashMap",
            "java/util/LinkedHashSet",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // `java.util.Hashtable` (interpreter round i6 wave 119, lane L5;
    // `i52-L5-proposal-a-compatible-row-per-retired-shadow-family`): the
    // family L1 wave 7 retired as one set (`RETIRED_SHADOW_L1_HT_TRIPLES`, 79
    // rows: the class, its three views, `$Entry`, `$Enumerator`), because the
    // views, the enumerators and the map share one backing. The prefix takes
    // the `$` classes. Its `SyntheticStub`s too (the `keys` / `elements` /
    // `clone` / constructor registrations of `native-collections` and
    // `deprecated_util`), which `--real-jdk` drops as well: a JDK-built
    // `Hashtable` must not meet a native that reads the side store. Not
    // `java/util/Properties` (a subclass: the `JdkConcurrentHashMap` row's,
    // keep it on with this one). Default ON since interpreter round i7 wave 144 (see the row's doc).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkHashtable,
        classes: &["java/util/Hashtable"],
        stub_classes: &["java/util/Hashtable"],
        bridges: &[],
        except: &[],
    },
    // `java.util.logging` (interpreter round i6 wave 120, lane L5;
    // `i52-L5-proposal-a-compatible-row-per-retired-shadow-family`): the family
    // with the most retired triples no family row covers yet (88, all in
    // `RETIRED_SHADOW_TRIPLES`: the 2026-08-11 census of every live `Bridge`
    // on a `java/util/logging/` receiver over `Code`, and `LogRecord`'s source
    // pair of 2026-08-12), and one package with no VM carrier state shared
    // outside it. Its `SyntheticStub`s too (`logging_shims`, `logmanager`'s
    // `LogManager` / `Logger` / `Level` stand-ins), as `--real-jdk` drops them.
    // Except `LoggingPermission`, one of the five permission classes a single
    // closure registers over (see the row's doc). Not this family:
    // `org/jboss/logmanager/` (an application library; its forced
    // `getEffectiveLevel` / `isLoggable` natives stay). Default OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkLogging,
        classes: &["java/util/logging/"],
        stub_classes: &["java/util/logging/"],
        bridges: &[],
        except: &["java/util/logging/LoggingPermission"],
    },
    // The builders (strings round, `str16`): every retired row and every
    // `SyntheticStub` of `StringBuilder` / `AbstractStringBuilder` -- what
    // `--real-jdk` drops there -- so the JDK's builder bodies run in both
    // modes; a `Bridge` or reviewed `Intrinsic` stays, as there. On a real
    // image the natives already worked on the JDK's compact `byte[]` builder,
    // so the objects do not change. `StringBuffer` is not in the family.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkStringBuilderBodies,
        classes: &["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"],
        stub_classes: &["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"],
        bridges: &[],
        except: &[],
    },
    // `java.lang.String`'s route-to-own-bytecode natives (interpreter round
    // i6 wave 122, lane L1; step 2 of
    // `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`):
    // the router census's first base-class JDK class
    // (`native-builtins/tests/router_census.rs`, `charset.rs` / `lang_string.rs`
    // `base java/lang/String`). Reviewed native by native; only the two that
    // are `Bridge`s are this family's:
    //
    // * `getBytes(Charset)` / `getBytes(String)` (`charset.rs`
    //   `register_real_charset_natives`, stated `Bridge`): an exact transcoding
    //   front (`string_get_bytes_front`) that declines to the JDK's body
    //   (`invoke_special_bytecode_only("java/lang/String", "getBytes", ..)`)
    //   for a null argument or an untranscribed charset. `--real-jdk` already
    //   runs the body for every call (concrete bytecode beats a `Bridge`,
    //   `resolve_dispatch` step 3); with the row in force `--compatible` does
    //   too. No VM state: the front reads only the receiver and the argument.
    //   The risk the 2026-08 note in `register_real_charset_natives` records
    //   (the JDK `CharsetEncoder` path over a synthetic `Buffer` overlay) is
    //   what the row's probe must clear before any flip.
    //
    // Not this family: `split` (three), `replaceAll`, `replaceFirst`,
    // `matches` (`string_regex_decline`) and `valueOf(Object)`
    // (`string_value_of_through_bytecode`) are reviewed `Intrinsic`s
    // (`register_synthetic_overrides`, `register_wrapper_natives`, the
    // `native_string_regex` re-registration), which `--real-jdk` keeps too; a
    // family row never takes what that mode keeps. `String` has no retired
    // rows, so `classes` names it only for the bridges, and `except` keeps
    // every other class whose name starts with `java/lang/String` out
    // (`StringIndexOutOfBoundsException` HAS retired rows). No stubs. Default
    // OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkStringGetBytes,
        classes: &["java/lang/String"],
        stub_classes: &[],
        bridges: &[
            ("java/lang/String", "getBytes", "(Ljava/nio/charset/Charset;)[B"),
            ("java/lang/String", "getBytes", "(Ljava/lang/String;)[B"),
        ],
        except: &[
            "java/lang/String$",
            "java/lang/StringB",
            "java/lang/StringC",
            "java/lang/StringD",
            "java/lang/StringI",
            "java/lang/StringL",
            "java/lang/StringT",
            "java/lang/StringU",
        ],
    },
    // `java.lang.management.MemoryUsage` (interpreter round i6 wave 123, lane
    // L1; step 2 of
    // `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`): the
    // router census's next base-class JDK class that needs a row after
    // `String` (`jmx.rs` `base java/lang/management/MemoryUsage`). Reviewed
    // native by native (`jmx.rs` `register_memory_usage`, stated `Bridge`):
    //
    // * `<init>(JJJJ)V` and the four `()J` getters are lane 2's retired rows
    //   (`RETIRED_SHADOW_L2_TRIPLES`), which `--real-jdk` refuses; the
    //   constructor already routes to the JDK's body
    //   (`invoke_special_bytecode_only`) whenever the class has bytes, and the
    //   getters read the same four fields the JDK's do. `classes` takes them.
    // * `toString` (registered only under `synthetic-jdk` or the
    //   `synthetic_memoryusage_tostring` flag) is a `Bridge` in front of the
    //   JDK's concrete `toString`, which `--real-jdk` runs: `bridges`.
    // * `<init>()V` is not the JDK's (the image declares no such constructor;
    //   the VM's synthetic receivers only), so it stays.
    //
    // No VM carrier state: the VM's own usages (`alloc_memory_usage`,
    // `management.rs`) allocate the class and write its fields by name
    // (`memory_usage_slots`). No stubs. Default OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkMemoryUsage,
        classes: &["java/lang/management/MemoryUsage"],
        stub_classes: &[],
        bridges: &[(
            "java/lang/management/MemoryUsage",
            "toString",
            "()Ljava/lang/String;",
        )],
        except: &[],
    },
    // The three `sun.management` memory beans (interpreter round i6 wave 124,
    // lane L1; step 2 of
    // `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`): the
    // router census's next base-class JDK classes after `MemoryUsage`
    // (`jmx.rs` `base sun/management/{GarbageCollectorImpl,MemoryManagerImpl,
    // MemoryPoolImpl}`). Reviewed native by native against the JDK 25 image
    // (`javap -p`; `jmx.rs` `register_garbage_collector_impl`,
    // `register_memory_manager_impl`, `register_memory_pool_impl`, all stated
    // `Bridge`):
    //
    // * Retired rows (`RETIRED_SHADOW_SBR2_AWT_TRIPLES`), which `--real-jdk`
    //   refuses, so `classes` takes them: each class's `<init>(String..)`
    //   (already routes to the JDK's constructor whenever the class has
    //   bytes), `getName` x3, `isValid` x2 and `MemoryPoolImpl.getType`. The
    //   JDK's bodies are one `getfield` each (`GarbageCollectorImpl`'s
    //   `getName` / `isValid` are `invokespecial` to `MemoryManagerImpl`'s),
    //   and the VM's beans write `name` / `isValid` / `isHeap` by name
    //   (`alloc_memory_pool_impl`, `init_memory_manager_fields`), so the
    //   readers answer what the natives did.
    // * `ACC_NATIVE` in JDK 25, genuine, stay: `getCollectionCount`,
    //   `getCollectionTime`, `getMemoryPools0`, and `MemoryPoolImpl`'s nine
    //   (`getUsage0`, `getPeakUsage0`, `getCollectionUsage0`,
    //   `setUsageThreshold0`, `setCollectionThreshold0`, `resetPeakUsage0`,
    //   `getMemoryManagers0`, `setPoolUsageSensor`, `setPoolCollectionSensor`).
    // * Not the JDK's (the image declares no such method), so no shadow and
    //   no `bridges` entry: `GarbageCollectorImpl.<init>(String,VMManagement)`
    //   and `GarbageCollectorImpl.gc()V`.
    //
    // No stubs, no carrier state, no `bridges`. The prefixes also name
    // `MemoryPoolImpl$PoolSensor` / `$CollectionSensor`, which have no
    // registration and no retired row, so no `except`. The rest of
    // `sun/management/` (`VMManagementImpl.is*Supported`, `ThreadImpl`,
    // `ClassLoadingImpl`, `MemoryImpl.isVerbose`) is not in this row: those
    // answers come from the VM's capability model, a separate review.
    // Default OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkManagementBeans,
        classes: &[
            "sun/management/GarbageCollectorImpl",
            "sun/management/MemoryManagerImpl",
            "sun/management/MemoryPoolImpl",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` (interpreter
    // round i6 wave 125, lane L1; step 2 of
    // `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows`): the
    // router census's next base-class JDK class after the `sun.management`
    // beans (`lang_reflect.rs` `base sun/reflect/generics/reflectiveObjects/
    // TypeVariableImpl`, `type_variable_impl_body`). Reviewed native by native
    // against the JDK 25 image (`javap -p -c`); all eleven are registered by
    // `lang_reflect.rs` `register_wp2_1_natives` (stated `Bridge`) and all
    // eleven are retired rows, which `--real-jdk` refuses, so `classes` takes
    // them:
    //
    // * the six annotation getters and `getAnnotatedBounds`
    //   (`RETIRED_SHADOW_SBR_REFLMETA_TRIPLES`, `getDeclaredAnnotations` in
    //   `RETIRED_SHADOW_REFLECT_GENERICS_DELEGATE_TRIPLES`): routers. For a
    //   receiver of this class they already run the JDK's body
    //   (`invoke_special_bytecode_only`) whenever the VM builds the JDK's type
    //   classes (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`, default on); the
    //   native arm left is a null argument (the JDK's `requireNonNull` NPE,
    //   raised by the native instead) and that switch off (an empty answer
    //   for a JDK-built variable, where HotSpot reads the declaration's type
    //   annotations).
    // * `getName`, `toString` (`return getName();`) and `getBounds`
    //   (`RETIRED_SHADOW_SBR_REFLECT_TRIPLES`): field readers. The JDK reads
    //   `name`, and `getBounds` reifies `bounds` once when it still holds
    //   `FieldTypeSignature[]`, then clones it; the VM's
    //   `generics::alloc_type_variable` stores `bounds` already reified (a
    //   `Type[]`) and writes `name` / `genericDeclaration` by name, so the
    //   JDK's bodies answer what the natives did. The native `toString`
    //   answered "?" for a null `name`, which a real one never has.
    // * `getTypeName`: not declared by the class (`Type.getTypeName` is a
    //   default method, `return toString();`), so with the row the inherited
    //   default answers through `toString`, the same name.
    //
    // Not in the row: the VM's own carrier, an instance of the INTERFACE
    // `java/lang/reflect/TypeVariable` (its natives are on that class name),
    // and `WildcardTypeImpl` / `ParameterizedTypeImpl` /
    // `GenericArrayTypeImpl`, which have retired rows of their own and need
    // their own review. No stubs, no `bridges`, no `except` (no other class
    // starts with this name). Default OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkTypeVariableImpl,
        classes: &["sun/reflect/generics/reflectiveObjects/TypeVariableImpl"],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // `java.util.PriorityQueue` and its `$Itr` (interpreter round i6 wave 133,
    // lane L6; `i52-L5-proposal-a-compatible-row-per-retired-shadow-family`,
    // the wave-121 census's next candidate). Reviewed against the JDK 25 image
    // (`javap -p`: `queue`, `size`, `comparator`, `modCount`):
    //
    // * all sixteen `native-collections` `register_priority_queue_natives`
    //   registrations (stated `Bridge`) are retired rows of
    //   `RETIRED_SHADOW_L1_TRIPLES`, as are `$Itr.hasNext` / `next` / `remove`,
    //   so the family takes them through `classes`: no `bridges`, no stubs.
    // * Carrier state: none outside the object. The natives keep the heap in
    //   the real `queue` (slot 0), `size` (1) and `comparator` (2); the
    //   readers elsewhere in `native-collections` (`pq_state`, the
    //   `PriorityQueue$Itr` fallback `real_inner_itr_state`, the bulk-copy
    //   readers) read those fields, which the JDK's bodies write too. The
    //   natives never write `modCount`, which is the gap the row closes: the
    //   JDK's `Itr` is fail-fast on it.
    // * The iterator carrier: `native_pq_iterator` (a retired row) is the
    //   only producer of a native-built `PriorityQueue$Itr`; with the row on
    //   `iterator()` is the JDK's `new Itr()`, and the force-native gate entry
    //   (`native_override.rs`, `PriorityQueue$Itr` `hasNext` / `next` /
    //   `remove`) forces only where a registration exists, which the row
    //   removes. The snapshot helpers that name the queue
    //   (`propagate_list_removal_in` calling `native_pq_remove`,
    //   `family_snapshot_generation` reading `size`) serve that native
    //   iterator's snapshot; read, not proven, to have no other producer --
    //   the probe's iterator-removal rows are the check.
    // * The constructors the natives do not register (`(SortedSet)`,
    //   `(PriorityQueue)`, `(int, Comparator)`) already run the JDK's
    //   bytecode in both modes.
    //
    // `PriorityBlockingQueue` is `JdkPriorityBlockingQueue`'s (another prefix).
    // Default OFF.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkPriorityQueue,
        classes: &["java/util/PriorityQueue"],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // `java.util.Arrays`' retired rows (strings round, `str16`): the copies
    // under `StringBuilder.toString()` were a native door per call. The
    // `Arrays$ArrayList` rows are lane 1's list family, not this one.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkArraysBodies,
        classes: &["java/util/Arrays"],
        stub_classes: &[],
        bridges: &[],
        except: &["java/util/Arrays$"],
    },
    // The Java2D / ImageIO / font stand-ins SBR round 5 lane awtjfr retired
    // (`RETIRED_SHADOW_SBR5_AWTJFR_TRIPLES`): the JDK's `BufferedImage`,
    // `SunGraphics2D`, `ImageIO`, the JPEG / PNG plugins and the `Font`
    // accessors run over the Rust loops, codec and font scaler of `native-awt`,
    // which register in every mode. No companion stubs: what stays native stays
    // a genuine `ACC_NATIVE` bridge.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkJava2d,
        classes: &[
            "com/sun/imageio/plugins/jpeg/JPEGImageReader",
            "com/sun/imageio/plugins/png/PNGImageWriter",
            "java/awt/Font",
            "java/awt/image/BufferedImage",
            "javax/imageio/ImageIO",
            "sun/java2d/HeadlessGraphicsEnvironment",
            "sun/java2d/SunGraphics2D",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // SBR round 5, lane compat2. `java.lang.Module` itself, which the
    // `jdk-module-graph` family left out: the access rows SBR waves 6, 7 and 9
    // retired (`addExports` / `addOpens` / `addUses` / `addReads`, `canRead` /
    // `canUse`, `isExported` / `isOpen`, `implAdd*`, `getName`,
    // `getClassLoader`, `getDescriptor`, `getLayer`, `getResourceAsStream`).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkModuleAccess,
        classes: &["java/lang/Module"],
        stub_classes: &["java/lang/Module"],
        bridges: &[],
        except: &["java/lang/ModuleLayer"],
    },
    // SBR round 5, lane compat2. The rows SBR wave 4 lane FJP retired on
    // `StackWalker` (`getInstance` x4): the walker is the JDK's own object, its
    // `walk` / `forEach` / `getCallerClass` natives stay (held for cost, the
    // walker lane's). `--real-jdk` refuses every stub of the class.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkStackWalker,
        classes: &["java/lang/StackWalker"],
        stub_classes: &["java/lang/StackWalker"],
        bridges: &[],
        except: &[],
    },
    // `jdk.internal.misc.Signal` (wave 4 FJP: `<init>`, `getName`,
    // `getNumber`; wave 5 FJP2: `raise`) and `sun.misc.Signal` (FJP2).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkSignal,
        classes: &["jdk/internal/misc/Signal", "sun/misc/Signal"],
        stub_classes: &["jdk/internal/misc/Signal", "sun/misc/Signal"],
        bridges: &[],
        except: &[],
    },
    // `Runtime` and `Runtime$Version` (wave 4 FJP: `getRuntime`, `version`, the
    // shutdown hooks; wave 5 FJP2: `load0`, `loadLibrary0`). `Runtime.exit`
    // stays native (the VM's exit path).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkRuntime,
        classes: &["java/lang/Runtime"],
        stub_classes: &["java/lang/Runtime"],
        bridges: &[],
        except: &["java/lang/RuntimeException", "java/lang/RuntimePermission"],
    },
    // The JSSE family SBR wave 4 lane SSL and wave 5 lane JSSE retired as ONE
    // unit (`RETIRED_SHADOW_SBR_SSL_TRIPLES`, `RETIRED_SHADOW_SBR_JSSE_TRIPLES`,
    // `RETIRED_SHADOW_R16_KEYSTORE_SPI_TRIPLES`): `SSLContext`, `SSLEngine`,
    // `SSLSocket`, the `sun.security.ssl` managers, the keystore engines and the
    // X.500 name carriers. The family minus its keystore half cannot run (a
    // native `SSLContext` beside real managers finds no identity), so the
    // keystore classes the `jdk-providers` family excepts are THIS family's.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkJsse,
        classes: &[
            "javax/net/ssl/",
            "sun/security/ssl/",
            "java/security/KeyStore",
            "sun/security/pkcs12/",
            "sun/security/provider/JavaKeyStore",
            "javax/security/auth/x500/",
        ],
        stub_classes: &[
            "javax/net/ssl/",
            "sun/security/ssl/",
            "java/security/KeyStore",
            "sun/security/pkcs12/",
            "sun/security/provider/JavaKeyStore",
            "javax/security/auth/x500/",
        ],
        bridges: &[],
        except: &[],
    },
    // The pipe JIT round 19 (nio19) retired whole
    // (`RETIRED_SHADOW_R19_NIO_PIPE_TRIPLES`): `Pipe.open` and the
    // `PipeImpl` / `SourceChannelImpl` / `SinkChannelImpl` surface. The JDK's
    // `PipeImpl` runs on `IOUtil.makePipe` and the file dispatcher, which serve
    // `--compatible` already. No companion stubs.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkPipe,
        classes: &[
            "java/nio/channels/Pipe",
            "sun/nio/ch/PipeImpl",
            "sun/nio/ch/SourceChannelImpl",
            "sun/nio/ch/SinkChannelImpl",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // The `(.., BiFunction)` bounds checks JOTP retired (JIT round 19,
    // nio19): pure JDK bytecode over `Preconditions.outOfBounds*`, no VM
    // state. The `(III)I` `Bridge` rows are not retired and stay.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkPreconditions,
        classes: &["jdk/internal/util/Preconditions"],
        stub_classes: &["jdk/internal/util/Preconditions"],
        bridges: &[],
        except: &[],
    },
    // The buffers, charsets and stream coders (JIT round 19, nio19):
    // `--real-jdk`'s retired rows and refused stand-ins on these classes, as a
    // unit, because a coder, its buffers and the `ScopedMemoryAccess` door they
    // read through share one object model (a VM-minted `CharBuffer` handed to a
    // real `CharsetDecoder` was the bridge this family needed before). The
    // `Bridge` / `Intrinsic` rows (`Unsafe`, the abstract-declaration buffer
    // rows that serve VM carriers, the charset intrinsics) stay, as there.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkNioBuffers,
        classes: &[
            "java/nio/Buffer",
            "java/nio/ByteBuffer",
            "java/nio/CharBuffer",
            "java/nio/ShortBuffer",
            "java/nio/IntBuffer",
            "java/nio/LongBuffer",
            "java/nio/FloatBuffer",
            "java/nio/DoubleBuffer",
            "java/nio/Heap",
            "java/nio/Direct",
            "java/nio/MappedByteBuffer",
            "java/nio/StringCharBuffer",
            "java/nio/ByteOrder",
            "java/nio/Bits",
            "java/nio/charset/",
            "sun/nio/cs/",
            "jdk/internal/misc/ScopedMemoryAccess",
        ],
        stub_classes: &[
            "java/nio/Buffer",
            "java/nio/ByteBuffer",
            "java/nio/CharBuffer",
            "java/nio/ShortBuffer",
            "java/nio/IntBuffer",
            "java/nio/LongBuffer",
            "java/nio/FloatBuffer",
            "java/nio/DoubleBuffer",
            "java/nio/Heap",
            "java/nio/Direct",
            "java/nio/MappedByteBuffer",
            "java/nio/StringCharBuffer",
            "java/nio/ByteOrder",
            "java/nio/Bits",
            "java/nio/charset/",
            "sun/nio/cs/",
            "jdk/internal/misc/ScopedMemoryAccess",
        ],
        bridges: &[],
        except: &[],
    },
    // The `java.lang.ref` reference model (JIT round 20, nio20, lane L4;
    // NIO19-6). Retired rows on these classes: the seven constructors
    // (`RETIRED_SHADOW_SBR3_REF_TRIPLES`), `WeakReference.refersTo`, the three
    // `get`s (`..._SBR4_SATB_TRIPLES`), `Reference.clear`,
    // `Reference.reachabilityFence`, `ReferenceQueue.poll` (`..._SBR2_SMALL`),
    // `remove()` / `remove(J)`.
    //
    // Does a native carry VM state the JDK body lacks? Read, row by row:
    //
    // * the constructors: YES, and it moves with the row. `ref_init_impl`
    //   writes the same two fields the JDK constructor writes (`referent`, and
    //   `queue` with `NULL_QUEUE` for none), but the subclass constructors also
    //   call `discover_reference` -- the processor's only way to hear of a
    //   `Reference` in `--compatible`. The row therefore engages discovery at
    //   ALLOCATION (`vm_init`: `RefDiscoveryState::from_flags`, the `--real-jdk`
    //   mechanism, SBR3 lane REF), which binds the entry from the object's own
    //   slots at the next collection, and seeds the soft clock from the JDK's
    //   `SoftReference.timestamp` (`refresh_soft_rows_from_java_timestamps`).
    // * `get` x3: no. The JDK body is `getfield referent`; the SATB keep-alive
    //   the natives applied lives on that `getfield` in both modes
    //   (`memory::ref_discovery::is_referent_field`, SBR4 lane SATB).
    // * `Reference.clear`: no. The JDK body is `clear0()`, whose native
    //   (`native_reference_clear0`) settles the processor row exactly as the
    //   stand-in's `retire_cleared_reference` did.
    // * `reachabilityFence`: no. The stand-in was a `black_box` no-op; the JDK
    //   body is empty and the JIT's fence handling does not need a registered
    //   native.
    // * `refersTo`: no. The JDK body is `refersTo0` (a registered native).
    // * `ReferenceQueue.poll` / `remove` x2: the JDK bodies take `lock` and,
    //   for `remove`, `lock.wait()`. The natives polled; the bodies need the
    //   collector's enqueue to `notifyAll` the queue's lock, which the row
    //   turns on (`gc_and_alloc::queue_wake_ups_recorded`), as `--real-jdk`.
    //
    // The non-retired bridges (`enqueue`, `isEnqueued`, the subclass `clear` /
    // `refersTo` spellings, `ReferenceQueue.<init>`, `PhantomReference.get`)
    // stay, as they do under `--real-jdk`; each was written for the real
    // layout (`enqueue` delegates to the JDK's `ReferenceQueue.enqueue`). No
    // companion stubs.
    //
    // Interpreter round i7 wave 135, lane L2
    // (`i134-R2-compatible-weakreference-refersto-is-not-compiled`): except the
    // three non-retired `refersTo` spellings, which go with the row as
    // `bridges`. `Reference.refersTo` is `final` with `Code`
    // (`return refersToImpl(obj)`), so a `WeakReference` receiver's lookup
    // missed the retired `WeakReference` row and found the BASE class's
    // `Bridge`: `--compatible` served every `refersTo` from the native
    // (`native_ref_refers_to`, a field read and a compare -- no VM state the
    // JDK body lacks: `refersToImpl` -> the registered `refersTo0` /
    // `PhantomReference.refersTo0` natives answer the same compare), the
    // stand-in sealed its callers out of the JIT, and `refersTo` /
    // `refersToImpl` were never compiled (`L5W132RefNotesThreadsBench
    // weak-new` 3769 ns against `--real-jdk`'s 308). `--real-jdk` runs the
    // JDK body for all four spellings already (concrete bytecode beats a
    // `Bridge`). `CRATONVM_COMPAT_ALIGN=-jdk-references` reverts the family,
    // these three included.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkReferences,
        classes: &[
            "java/lang/ref/Reference",
            "java/lang/ref/WeakReference",
            "java/lang/ref/SoftReference",
            "java/lang/ref/PhantomReference",
            "java/lang/ref/ReferenceQueue",
        ],
        stub_classes: &[],
        bridges: &[
            (
                "java/lang/ref/PhantomReference",
                "refersTo",
                "(Ljava/lang/Object;)Z",
            ),
            ("java/lang/ref/Reference", "refersTo", "(Ljava/lang/Object;)Z"),
            (
                "java/lang/ref/SoftReference",
                "refersTo",
                "(Ljava/lang/Object;)Z",
            ),
        ],
        except: &[],
    },
    // (`java.lang.Object`'s rows are NOT a class family: two of its six retired
    // rows carry VM state. See `is_object_body_row`, the `JdkObjectBodies`
    // row.)
    //
    // `java.util.Optional` and the three primitive Optionals (JIT round 20,
    // nio20, lane L4; NIO19-6, `R16Fieldsite2Layout optional`, 2.5x the
    // `--real-jdk` time). No VM state: every `Optional` the natives mint uses
    // the real layout (`OPT_FIELD_VALUE = 0`, `value`; the primitive ones
    // `isPresent`, `value`, `native-collections` `make_opt_prim`), so a
    // VM-minted object answers the JDK bodies and a JDK-built one answers
    // what is left. What changes is HotSpot's answer where the stand-in had
    // its own: `Optional.empty()` is the JDK's `EMPTY` singleton
    // (`Optional.empty() == Optional.empty()`), not a fresh object. The eight
    // functional rows were already left to bytecode on a real JDK
    // (`optional_functional_left_to_bytecode`). No companion stubs.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkOptional,
        classes: &["java/util/Optional"],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // The NIO socket / selector / datagram family (JIT round 20, nio20, lane
    // L4; NIO19-7): exactly the classes of `RETIRED_SHADOW_SBR_NIOREAL_TRIPLES`
    // (SBR wave 5 lane NIOREAL), which `--real-jdk` runs over the real
    // `sun.nio.ch` layer (`native-io/src/net_nio_real.rs`). The table holds the
    // FACTORIES too (`SocketChannel.open*`, `ServerSocketChannel.open*`,
    // `DatagramChannel.open*`, `Selector.open`, the `EPollSelectorProvider` /
    // `SelectorProviderImpl` `open*` rows) and `ServerSocketChannelImpl.accept`,
    // which are the only producers of a VM channel carrier on a real JDK
    // (`socket_channel.rs` `alloc_channel_as_impl`'s four callers,
    // `nio_selector.rs` `selector_open_native`), so the family moves whole: no
    // carrier is minted and no real channel meets a carrier's native. Census of
    // the other `--compatible` natives that touch a channel (nio20 L4): the
    // xnio / Undertow / Vert.x registrars run Rust-side event loops and hand
    // Java no channel; `p72_socket_adaptor_address` asks the channel through
    // `invoke_virtual`; the `java/nio/channels/*` rows on ABSTRACT methods
    // (`read`, `select`, `SelectionKey.interestOps()`, ...) stay as under
    // `--real-jdk` (a real receiver declares those methods itself). The
    // interrupt hook `System$1.blockedOn` is a row of the same table on a
    // class this family must not take whole; `NativeMethodRegistry::register`
    // drops it under this row or `JdkPipe` (`is_nio_blocker_hook`). On
    // Windows the table is empty, so the row is inert there. No companion
    // stubs (that keeps the row inert on Windows too).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkNioChannels,
        classes: &[
            "java/net/DatagramSocket",
            "java/net/MulticastSocket",
            "java/nio/channels/DatagramChannel",
            "java/nio/channels/SelectableChannel",
            "java/nio/channels/SelectionKey",
            "java/nio/channels/Selector",
            "java/nio/channels/ServerSocketChannel",
            "java/nio/channels/SocketChannel",
            "java/nio/channels/spi/AbstractSelectableChannel",
            "sun/nio/ch/DatagramChannelImpl",
            "sun/nio/ch/DatagramSocketAdaptor",
            "sun/nio/ch/EPollSelectorImpl",
            "sun/nio/ch/EPollSelectorProvider",
            "sun/nio/ch/MembershipKeyImpl",
            "sun/nio/ch/PollSelectorImpl",
            "sun/nio/ch/SelectionKeyImpl",
            "sun/nio/ch/SelectorImpl",
            "sun/nio/ch/SelectorProviderImpl",
            "sun/nio/ch/ServerSocketChannelImpl",
            "sun/nio/ch/SocketChannelImpl",
            "sun/nio/ch/WEPollSelectorImpl",
            "sun/nio/ch/WEPollSelectorProvider",
        ],
        stub_classes: &[],
        // nio20 wave 2 (L5; NIO20-L4-3), read against the 25/linux kind map and
        // `javap` of the 25 image: every non-retired registration left on these
        // classes in `--compatible` is either an abstract declaration (served
        // only to a receiver with no `Code`, as under `--real-jdk`), a real
        // `ACC_NATIVE` entry (`DatagramChannelImpl.send0` / `receive0` /
        // `disconnect0`), or names a method the image does not declare
        // (`DatagramChannelImpl.localAddress()Ljava/net/SocketAddress;` -- the
        // image's returns `InetSocketAddress`, a retired row --,
        // `SocketChannelImpl.configureBlocking(Z)..AbstractSelectableChannel;`
        // -- the image's is `AbstractSelectableChannel.configureBlocking(Z)
        // ..SelectableChannel;` --, `read0` / `write0` on the channel impls), so
        // no call site reaches it. Nothing to list.
        bridges: &[],
        except: &[],
    },
    // The asynchronous socket family (JIT round 20, nio20 wave 3, lane L1):
    // the socket / server-socket / group classes of
    // `RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES` (SBR wave 6 lane `asyncnet`), which
    // `--real-jdk` runs over the real `sun.nio.ch` layer (`EPoll`, `Net`,
    // `IOUtil.makePipe` / `write1` / `drain1`,
    // `UnixAsynchronousSocketChannelImpl.checkConnect`; all `ACC_NATIVE`
    // bridges registered in both modes, `net_nio_real.rs`).
    //
    // The VM-state review, carrier by carrier (`native-io/src/async_socket.rs`):
    //
    // * The PRODUCERS of a VM async carrier are `aio_asc_open` /
    //   `aio_asc_open_group` (`AsynchronousSocketChannel.open*`),
    //   `aio_assc_open*` (`AsynchronousServerSocketChannel.open*`), the
    //   `AcceptedChannel` mint of the handler-form `accept` and the
    //   `AsynchronousChannelGroup.with*ThreadPool` group carriers. Every one is
    //   registered on a triple of the table (the factories on the public
    //   classes AND their `Unix*Impl` / `*Impl` mirrors, `accept` on both
    //   impls), so with the row no carrier is minted and no JDK channel meets
    //   a carrier's native: the family moves whole, as `JdkNioChannels` does.
    // * `aio_registry` / `aio_owners` / the cancel flags and the per-VM
    //   completion queues are keyed by the carrier's private `F_REG_ID` slot
    //   (`aio_base`): no JDK-built channel has one, and nothing outside this
    //   file reads them. They stay empty under the row.
    // * The one cross-crate writer, `phases_late/net_channels.rs`'s
    //   `connect(SocketAddress)Future` body (`async_socket_note_connected`),
    //   is registered on `AsynchronousSocketChannel` (abstract there: served
    //   only to a receiver with no `Code`) and on `AsynchronousSocketChannelImpl`
    //   / `UnixAsynchronousSocketChannelImpl` (both table rows: dropped).
    // * Non-retired rows left on these classes in `--compatible` (25/linux
    //   kind map): the abstract `java/nio/channels/Asynchronous*` declarations
    //   (as under `--real-jdk`), `UnixAsynchronousSocketChannelImpl.
    //   checkConnect` (the real `ACC_NATIVE`), and names the 25 image does not
    //   declare (`EPollPort.open` / `close` / `drain` / `poll`, the IOCP-facade
    //   rows; `*ServerSocketChannelImpl.localAddress()`): no call site reaches
    //   them, so `bridges` is empty.
    //
    // Not in the family: the `AsynchronousFileChannel` / `FileLockImpl` /
    // provider `newAsynchronousFileChannel` / `UnixFileSystem.getFileStores`
    // rows of the same table (the file half: its producer is the providers'
    // row, which belongs to the `java.nio.file` provider family), and
    // `NativeDispatcher.preClose` (shared with the synchronous channels, as
    // `JdkNioChannels` leaves it). Inert on Windows
    // (`family_exists_on_this_platform`: the table is `#[cfg(unix)]`).
    // Witness: `NioConf19b.java -Dnio19.rows=asock` with
    // `CRATONVM_COMPAT_ALIGN=+jdk-async-channels` against HotSpot.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkAsyncChannels,
        classes: &[
            "java/nio/channels/AsynchronousChannelGroup",
            "java/nio/channels/AsynchronousServerSocketChannel",
            "java/nio/channels/AsynchronousSocketChannel",
            "sun/nio/ch/AsynchronousChannelGroupImpl",
            "sun/nio/ch/AsynchronousServerSocketChannelImpl",
            "sun/nio/ch/AsynchronousSocketChannelImpl",
            "sun/nio/ch/EPollPort",
            "sun/nio/ch/Iocp",
            "sun/nio/ch/Port",
            "sun/nio/ch/UnixAsynchronousServerSocketChannelImpl",
            "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
            "sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl",
            "sun/nio/ch/WindowsAsynchronousSocketChannelImpl",
            // Windows only: the platform file-channel impl and its abstract
            // parent. The layer runs the JDK's `WindowsAsynchronousFileChannelImpl`
            // over `net_iocp_win.rs`, so the carrier rows `native-io` mirrors
            // onto these two classes (`AFC_IMPLS`, `AFC_ABSTRACT_IMPL`) meet a
            // JDK-built receiver: `isOpen()` read the carrier's slot map at base 0
            // off a real object (`closed` true) and every read, write and lock
            // failed with `ClosedChannelException` (`WinAsync`, `NioConf19b afc`,
            // `--compatible`, 2026-10-13). Linux keeps its file half with the
            // provider family (above).
            #[cfg(windows)]
            "sun/nio/ch/AsynchronousFileChannelImpl",
            #[cfg(windows)]
            "sun/nio/ch/WindowsAsynchronousFileChannelImpl",
        ],
        stub_classes: &[],
        // The carrier's half-close natives (nio20 wave 3 L1 fix-back,
        // `async_socket.rs` `aio_asc_shutdown`): not retired rows, and they
        // stand in front of `AsynchronousSocketChannelImpl`'s `Code`. They
        // delegate a JDK-built receiver to that bytecode themselves; with the
        // family in force no carrier exists, so they go with it.
        bridges: &[
            (
                "sun/nio/ch/AsynchronousSocketChannelImpl",
                "shutdownInput",
                "()Ljava/nio/channels/AsynchronousSocketChannel;",
            ),
            (
                "sun/nio/ch/AsynchronousSocketChannelImpl",
                "shutdownOutput",
                "()Ljava/nio/channels/AsynchronousSocketChannel;",
            ),
            (
                "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
                "shutdownInput",
                "()Ljava/nio/channels/AsynchronousSocketChannel;",
            ),
            (
                "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
                "shutdownOutput",
                "()Ljava/nio/channels/AsynchronousSocketChannel;",
            ),
        ],
        except: &[],
    },
    // `java.util.ArrayList` with its `SubList`, `SubList$1`, `ListItr` and
    // spliterator (nio20 wave 2, L5; the r20 "families not adopted" page,
    // `R16Fieldsite2Layout sublist`, 5.6x the `--real-jdk` time). Every
    // registration on these classes is a retired row (`RETIRED_SHADOW_L1_*`:
    // the whole `ArrayList` surface, all 41 `SubList` rows, the ten
    // `SubList$1` rows) or a `SyntheticStub` (`ArrayList.subList`, which mints
    // the `cratonvm/internal/ArrayListSubList` fallback), so the family drops
    // them all, as `--real-jdk` does.
    //
    // Does a native carry VM state the JDK bodies lack? The census:
    //
    // * the `asl_*` sublist state (five slots past the declared fields) and
    //   the `SLI_*` iterator state are minted ONLY by `alloc_asl_view` /
    //   `alloc_sli_view`, whose only callers are `ArrayList.subList`,
    //   `SubList.subList` and the `SubList` `iterator` / `listIterator` rows
    //   -- all in this family. With the family in force no such carrier
    //   exists, and the natives that read one (registered on the real classes,
    //   so dropped too, and on `cratonvm/internal/ArrayListSubList`, which
    //   nothing mints any more) see none.
    // * every other `ArrayList` a `--compatible` native mints
    //   (`alloc_arraylist_with`, `make_list_of`, the `Collections` fallbacks,
    //   ...) writes the REAL `elementData` / `size` slots (`al_slots`) and a
    //   non-null `elementData`, which is all the JDK bodies read; `modCount`
    //   starts at 0, as `new ArrayList()`'s does. (The one mint that wrote the
    //   synthetic `(0, 1)` slots on a real class, the thread pool's
    //   `shutdownNow` fallback, is fixed in the same wave.)
    // * `ArrayList$Itr` is EXCEPTED: it is the cursor class the VM mints for
    //   EVERY backing with a real `modCount`, `Vector` included
    //   (`itr_backing_has_real_mod_count`), and its natives (`hasNext` /
    //   `next` yield to the JDK cursor already, `remove` writes through to the
    //   backing with `propagate_list_removal`) are what keep a `Vector`-backed
    //   one coherent. It stays exactly as it is in `--compatible` today; a
    //   JDK-built `Itr` already reaches the same three natives there.
    //
    // Witness: `Nio20CompatFamilies2.java` rows `sublist-*`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkArrayList,
        classes: &["java/util/ArrayList"],
        stub_classes: &["java/util/ArrayList"],
        bridges: &[],
        except: &["java/util/ArrayList$Itr"],
    },
    // The `java.net.http` client and `WebSocket` (JIT round 20, nio20 wave 4,
    // lane L4; `r20-compatible-http-client-carriers-websocket-and-wire-defaults`,
    // `NioConf19` rows `ws` and `https`). `--real-jdk` runs the JDK's
    // `jdk.internal.net.http` over the real `sun.nio.ch` layer since SBR wave 6
    // lane HTTPSTACK; the module declares no `ACC_NATIVE` method, so every
    // registration on these classes is a carrier.
    //
    // The VM-state review, carrier by carrier:
    //
    // * The PRODUCERS of a VM client carrier are `HttpClient.newHttpClient` /
    //   `newBuilder` (three registrars: `net_phase_e` `re5_alloc_client`,
    //   `http2` `alloc_http_client`, `http_client` on `HttpClientImpl`) and
    //   `HttpRequest.newBuilder` x2 / the `BodyPublishers` / `BodyHandlers`
    //   factories: all retired rows of `RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES`
    //   (the 50 client rows, every one `SyntheticStub` on the 25/linux kind
    //   map), so with the row no carrier is minted. `servlet.rs`'s
    //   `HttpResponse` mint is inside `HttpClient.send`, an abstract-method row
    //   a JDK-built `HttpClientFacade` never reaches (it declares `send`).
    // * The carriers' state -- `http2`'s connection pool and HPACK tables,
    //   `net_phase_e`'s client side tables, `http_client`'s pooled sockets,
    //   `phases_late/net_channels`' `WebSocket` sockets -- is keyed by the
    //   carrier object or its private slots: no JDK-built object has one, and
    //   nothing outside those files reads them. They stay empty under the row.
    // * The non-retired rows left on these classes in `--compatible` (25/linux
    //   kind map: 67 `Bridge`s) are abstract declarations (`HttpClient`'s
    //   accessors and `send*`, the `Builder` / `HttpResponse` / `ResponseInfo`
    //   / `BodyPublisher` interface methods, served only to a receiver with no
    //   `Code`, as under `--real-jdk`) or name methods the 25 image does not
    //   declare (`HttpRequest.toString` -- `HttpRequestImpl` declares it --,
    //   `Http1HeaderParser.parse([B)Z` / `statusCode`, `Http2ClientImpl.
    //   openConnections` / `sendHttp2`, `HttpClientImpl.<init>()V` and its
    //   `HttpRequestImpl`-typed `send*`, `Http1Exchange.<init>()V` /
    //   `writeRequest`, `HttpRequestImpl.<init>()V`). The one concrete JDK
    //   method a carrier registrar answers is `HttpClient.newWebSocketBuilder`
    //   (`http2`, whose JDK body throws `UnsupportedOperationException`):
    //   listed in `bridges`, though `HttpClientFacade` overrides it.
    //
    // HTTPS: the JDK client calls `SSLContext.createSSLEngine` and drives the
    // engine through `SSLFlowDelegate` (`wrap(ByteBuffer[], ByteBuffer)`,
    // `unwrap`, delegated tasks), which is the JSSE carrier engine unless
    // `jdk-jsse` is in force -- the same engine Netty and Tomcat drive.
    // Prerequisite: `JdkNioChannels` (`HttpClientImpl$SelectorManager` is a
    // real `Selector` over real `SocketChannel`s); `NativeMethodRegistry::
    // compat_row_prerequisites_hold` refuses the row without it, and the row is
    // inert on Windows with it (`family_exists_on_this_platform`). Witness:
    // `NioConf19.java -Dnio19.rows=ws,sslsock` and
    // `NioConf19b -Dnio19.rows=httpstream,httpheaders` against HotSpot.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkHttpClient,
        classes: &["java/net/http/", "jdk/internal/net/http/"],
        stub_classes: &["java/net/http/", "jdk/internal/net/http/"],
        bridges: &[(
            "java/net/http/HttpClient",
            "newWebSocketBuilder",
            "()Ljava/net/http/WebSocket$Builder;",
        )],
        except: &[],
    },
    // The `com.sun.net.httpserver` server (JIT round 20, nio20 wave 5, lane
    // L1; `r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot`): the
    // server half of `RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES` that the
    // `JdkHttpClient` family left -- `HttpServer.create` x2 and the 11
    // `HttpServerImpl` rows (the six `HttpContextImpl` rows are registered
    // over the synthetic library only since nio20 wave 4) -- which
    // `--real-jdk` runs as the JDK's `sun.net.httpserver.ServerImpl` over the
    // real `sun.nio.ch` layer since SBR wave 6 lane HTTPSTACK.
    //
    // The VM-state review, carrier by carrier (`net_phase_e` `re10_*`,
    // `phases_late/net_channels` `register_p72_http_server`):
    //
    // * The only PRODUCERS of a carrier server are `re10_create_server` /
    //   `re10_create_unbound_server`, registered on `HttpServer.create` x2 and
    //   `HttpServerImpl.create` x2: all four are table rows, so with the row
    //   no carrier is minted. A carrier `HttpExchange` / `HttpContext` is
    //   minted only by that server's `createContext` and its dispatch loop.
    // * The carrier state -- `server_registry`, `request_queue`, the
    //   `http_link` side tables, the dispatcher threads -- is keyed by the
    //   carrier's own `server_id` slot or object: no JDK-built server has one,
    //   and nothing outside those two files reads them.
    // * Left registered on these classes (25/linux kind map): `Bridge`s on the
    //   ABSTRACT `HttpServer` / `HttpExchange` / `HttpContext` declarations
    //   (served only to a receiver with no `Code`, as under `--real-jdk`; the
    //   JDK's `HttpServerImpl` / `ExchangeImpl`-backed exchange / `HttpContextImpl`
    //   declare their own), and `HttpContextImpl.setAuthenticator(..)V`, a
    //   descriptor no image declares. The carrier-only class
    //   `HttpExchange$ResponseBody` (no JDK image has it) is a stub class
    //   here, so its five `SyntheticStub`s go too.
    //
    // Prerequisite `JdkNioChannels` (the JDK's `ServerImpl` drives a real
    // `Selector` over a real `ServerSocketChannel`); `NativeMethodRegistry::
    // compat_row_prerequisites_hold` refuses the row without it, and the row is
    // inert on Windows (`family_exists_on_this_platform`). Default ON
    // (`HotSpotAlignment::default_on`: the probes equal `--real-jdk` with it,
    // the carrier's speed is a JIT page, not a reason to keep a second server;
    // `CRATONVM_COMPAT_ALIGN=-jdk-http-server` reverts). Witness:
    // `Nio20HttpHot.java -Dnio20.rows=hc` and `SbrW5HttpBench`
    // (`CRATONVM_COMPAT_ALIGN=-jdk-http-server` for the off arm).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkHttpServer,
        classes: &["com/sun/net/httpserver/", "sun/net/httpserver/"],
        stub_classes: &["com/sun/net/httpserver/", "sun/net/httpserver/"],
        bridges: &[],
        except: &[],
    },
    // `java.util.Locale` and its nested classes (nio20 wave 2, L5;
    // `R16Fieldsite2Layout locale`, 16x the `--real-jdk` time). Every
    // `--compatible` registration on `Locale` is a retired row
    // (`RETIRED_SHADOW_SBR_LOCALE_TRIPLES` and the L-wave tables: the
    // factories, `getDefault` / `setDefault`, the accessors, `toString`,
    // `toLanguageTag`, `stripExtensions`, `getExtension`, `getISO3Language`).
    //
    // The VM state, read row by row:
    //
    // * `synthetic_locale_data` / `locale_data` (the subtag side tables) and
    //   `CACHED_LOCALE` are written by the family's own natives
    //   (`getDefault`, `forLanguageTag`, `setDefault`) and by the
    //   `locale_alloc` callers outside it (`locale_resources`, `text_intl`).
    //   Every Locale those mint also gets a real `sun.util.locale.BaseLocale`
    //   in `baseLocale` (`locale_populate_full`) and a null
    //   `localeExtensions` (the JDK's "no extensions" shape), so the JDK's
    //   `getLanguage` / `equals` / `toLanguageTag` bodies answer it from its
    //   own fields: the side tables are a cache of `baseLocale`, not state the
    //   bodies lack.
    // * the side tables' READERS that stay (`String.toLowerCase` /
    //   `toUpperCase(Locale)`, `default_case_mapping_language`, the resource
    //   natives) fall back to the real `baseLocale` and to the real
    //   `Locale.defaultLocale` static -- the `--real-jdk` route, where the
    //   same rows are retired and `CACHED_LOCALE` is never written.
    // * the default: the JDK's `Locale.<clinit>` already runs on a real JDK
    //   (it builds the constants, then `defaultLocale = initDefault()`), so
    //   `defaultLocale` is set from the same `user.language` / `user.country`
    //   the native default read; with the row `getDefault()` answers that
    //   static, the JDK's cached instance, where the native answered a fresh
    //   VM-minted `Locale` no `==` matched.
    //
    // `sun/util/locale/` (the provider adapters) is NOT in the family: its
    // retired rows (`LocaleResources`, `CalendarDataUtility`,
    // `JRELocaleProviderAdapter.getLocaleServiceProvider`) serve every
    // formatter, a separate decision. Witness: `Nio20CompatFamilies2` rows
    // `locale-*`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkLocale,
        classes: &["java/util/Locale"],
        stub_classes: &["java/util/Locale"],
        bridges: &[],
        except: &[],
    },
    // `java.util.Date`'s 19 retired rows (`RETIRED_SHADOW_L1_DATE_TRIPLES`:
    // the deprecated field constructors, getters and setters, `toGMTString`).
    // No VM state: the natives read and write `fastTime` (slot 0, the real
    // field) and nothing else. That is also their defect: they never touch
    // `cdate`, so after any JDK body normalised a `Date` (`toString`,
    // `getTime` on a denormalised one) a native `setYear` / `setMonth` moved
    // `fastTime` under a stale `cdate` the next `toString` printed, and they
    // computed in UTC where HotSpot uses the default `TimeZone`. `TimeZone`
    // stays out (see `RETIRED_SHADOW_L1_DATE_TRIPLES`' own note). nio20 wave
    // 2, L5. Witness: `Nio20CompatFamilies2` rows `date-*`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkDate,
        classes: &["java/util/Date"],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // The locale PROVIDERS (nio20 wave 5, L4; W2L5-2, the r20 "`--compatible`
    // locale row is still twelve times the `--real-jdk` time" page): the
    // `ResourceBundle` / `LocaleResources` / `Bundles` / `LocaleData` /
    // `JRELocaleProviderAdapter` / `CalendarDataUtility` floor every
    // formatter stands on, and the two `java.text` families retired with it
    // under `--real-jdk` (`DecimalFormatSymbols.getInstance` / `initialize`,
    // `BreakIterator` whole). Every registration on these classes in the
    // 25/linux `--compatible` kind map is a retired row
    // (`RETIRED_SHADOW_SBR_LOCALE2_TRIPLES`, `_SBR_LOCALE_TRIPLES`'
    // `Control.getCandidateLocales`, `_SBR2_TAIL_TRIPLES`' `ResourceBundle$1`
    // and `CalendarDataUtility` rows, `_L1_LP_TRIPLES`, `_STATELESS_TRIPLES`'
    // two `DecimalFormatSymbols` rows, `_L1_BI_TRIPLES`): 49 registrations,
    // all `synthetic-stub`, plus `ResourceBundle.handleGetObject` / `getKeys`
    // (`Bridge`s over ABSTRACT declarations, which stay, as under
    // `--real-jdk`).
    //
    // The VM-state review, native by native:
    //
    // * The bundle CARRIERS (`rb_get_bundle` / `build_bundle` in
    //   `locale_resources.rs`, `resource_bundle_get_bundle` in
    //   `phases_late/text_intl.rs`: instances of the abstract
    //   `java/util/ResourceBundle` with a hidden map in slot 0) are minted ONLY
    //   by the eight `getBundle` rows, `LocaleData.getBundle` and `Bundles.of`
    //   -- all in this family, whichever registrar registered them. With the
    //   row no carrier is minted; the two abstract-method `Bridge`s that stay
    //   answered only carriers (a real bundle declares both), exactly the
    //   `--real-jdk` shape `RETIRED_SHADOW_SBR_LOCALE2_TRIPLES` records.
    // * The `LocaleResources` readers, `CalendarDataUtility`,
    //   `JRELocaleProviderAdapter.getLocaleServiceProvider`: pure functions of
    //   the image's CLDR classes (`cldr_cache`, `break_iterator_info`); they
    //   hand back fresh arrays / strings / maps and keep nothing the JDK's
    //   `LocaleResources.cache` would lack.
    // * `DecimalFormatSymbols.initialize`: sets the instance through its own
    //   setters plus `locale` / `currencyInitialized`, every one a field the
    //   JDK body writes itself; `getInstance` is `new DecimalFormatSymbols(l)`.
    // * `BreakIterator`: the fabricated carrier (`bi_alloc_kind`, class
    //   exactly `java/text/BreakIterator`) is minted only by the four factory
    //   rows of this family; the instance rows already step aside for a real
    //   iterator (`bi_is_fabricated_carrier`).
    // * `ResourceBundle$1` (`JavaUtilResourceBundleAccess`): the three shims
    //   read and write `parent` / `locale`, as the JDK bodies do.
    // * `Currency.getSymbol(Locale)` / `getDisplayName(Locale)` are not retired
    //   rows but `Bridge`s over concrete JDK `Code` (`--real-jdk` runs the
    //   bodies, `resolve_dispatch` step 3): their JDK bodies go through
    //   `LocaleServiceProviderPool` -> the providers this family hands back to
    //   the JDK, so they are listed in `bridges` and go with it.
    //
    // What `--compatible` loses is its OWN re-implementations of the CLDR
    // lookups (every one of the r16 locale lane's fixes was a gap in one), and
    // what it gains is the JDK's per-`LocaleResources` caches: the stand-ins
    // rebuilt a bundle / symbol array per call. NOT here: `TimeZone` (its five
    // rows stay with the `ZoneInfo` carriers; the `RETIRED_SHADOW_L1_DATE_TRIPLES`
    // note), `Normalizer`, the `Formatter` / `String.format(Locale, ..)` /
    // `DateFormat.format` `Intrinsic`s (kept in both modes). Witness:
    // `Nio20LocaleProviders.java` (both modes against HotSpot;
    // `-Dnio20lp.bench=1` for the timings).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkLocaleProviders,
        classes: &[
            "java/util/ResourceBundle",
            "sun/util/locale/provider/",
            "sun/util/resources/",
            "java/text/DecimalFormatSymbols",
            "java/text/BreakIterator",
            "java/util/Currency",
        ],
        stub_classes: &[
            "java/util/ResourceBundle",
            "sun/util/locale/provider/",
            "sun/util/resources/",
            "java/text/DecimalFormatSymbols",
            "java/text/BreakIterator",
        ],
        bridges: &[
            (
                "java/util/Currency",
                "getSymbol",
                "(Ljava/util/Locale;)Ljava/lang/String;",
            ),
            (
                "java/util/Currency",
                "getDisplayName",
                "(Ljava/util/Locale;)Ljava/lang/String;",
            ),
        ],
        except: &[],
    },
    // `java.io.PrintWriter` (nio20 wave 6, L5; the `swriter` row of the r19
    // wrapper-rows page, and every `PrintWriter` over a `StringWriter` /
    // `OutputStreamWriter`): its seven retired rows
    // (`RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES`, registered by
    // `logging_shims::register_printstream_fallback_natives`; the 25/linux
    // kind map has exactly those seven on the class, all `synthetic-stub`).
    //
    // The VM-state review, native by native:
    //
    // * `<init>(OutputStream)` (`native_printwriter_init_outputstream`): ends
    //   in `invoke_special` of the JDK's `PrintWriter(OutputStream, boolean)`,
    //   so a native-built receiver already had the JDK's `lock` / `out` /
    //   `autoFlush` / `psOut`; the JDK's one-argument body is that same call.
    // * `println` x4 / `write(String..)` x2 (`native_println_*`,
    //   `native_printwriter_write_string*`): each re-encoded the text as a Rust
    //   `String` (`read_string`: an unpaired surrogate became U+FFFD where
    //   HotSpot writes it through) and wrote it through `out`
    //   (`route_write_through_out`), or to an fd for a console carrier. The
    //   one thing they kept that the JDK bodies do not is
    //   `record_printed_line`: a per-thread `Vec<String>` that only the
    //   in-process test harness reads (`runtime/tck.rs`) and nothing drains,
    //   so under the row it stops growing for `PrintWriter` lines.
    // * no native mints a `PrintWriter` carrier on a real JDK (the class-floor
    //   shape in `class_manager.rs` is the synthetic library's), and the
    //   `trouble` side reads the real field (`print_error_state`), which the
    //   JDK bodies set themselves.
    //
    // `java.io.PrintStream` is NOT in the family (`System.out` / `err` are VM
    // console carriers; `RETIRED_SHADOW_L4_*`'s own hold note). Witness:
    // `Nio20CompatRows.java` rows `pw-*`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkPrintWriter,
        classes: &["java/io/PrintWriter"],
        stub_classes: &["java/io/PrintWriter"],
        bridges: &[],
        except: &[],
    },
    // `java.util.concurrent.locks.LockSupport` (interpreter round i2 wave 69,
    // lane L1): the seven rows the LockSupport block of the
    // `RETIRED_SHADOW_*` tables retired (`park` x2, `parkNanos` x2,
    // `parkUntil(Object, long)`, `unpark`, `getBlocker`). None is `native` in
    // JDK 25: each is a short body over `Unsafe.park` / `unpark` (which stay
    // the VM's `ACC_NATIVE` bridges) and the `parkBlocker` field. `--compatible`
    // ran the stand-ins, which set the blocker only behind
    // `CRATONVM_STAND_IN_WAIT_OBJECTS`, yielded a virtual thread before
    // setting it, and answered `getBlocker` as a second reader of the field.
    // No companion stubs.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkLockSupport,
        classes: &["java/util/concurrent/locks/LockSupport"],
        stub_classes: &[],
        except: &[],
        bridges: &[],
    },
    // `java.util.concurrent.locks.StampedLock` and its lock views
    // (`$ReadLockView`, `$WriteLockView`, `$ReadWriteLockView`; interpreter
    // round i2 wave 70, lane L1). No row is retired: the 31 stand-ins
    // `native-builtins` `register_stamped_lock_natives` registers are stated
    // `SyntheticStub`s, which `--real-jdk` refuses as a whole surface. JDK 25
    // declares no `native` method on any of these classes; the bodies need
    // only `Unsafe` CAS on `state` and `LockSupport.park(this)` (the
    // `JdkLockSupport` row's bodies, or the VM's stand-ins with that row
    // off). So the family is its stub class: every `SyntheticStub`
    // registration on the class prefix is dropped together, which keeps the
    // all-or-nothing shape `registry_contracts.rs`
    // `stamped_lock_surface_is_intercepted_completely` asks of a partial
    // surface (none at all, here). `classes` names it too, so a row a later
    // wave retires on it joins the family without a line here.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkStampedLock,
        classes: &["java/util/concurrent/locks/StampedLock"],
        stub_classes: &["java/util/concurrent/locks/StampedLock"],
        except: &[],
        bridges: &[],
    },
    // The four scalar atomics (nio21 wave 1, L4; the r20 `HttpClient` page):
    // their 42 `RETIRED_SHADOW_STATELESS_TRIPLES` rows. Default ON since nio21
    // wave 3 (L1; the row's comment). The VM-state review: every stand-in (`phases_early`,
    // `util_concurrent_ext`) reads and writes slot 0, which on the real class
    // is the one instance field `value` the JDK bodies CAS through `Unsafe` /
    // `VarHandle`; a VM-minted atomic is all-zero, the JDK's initial value.
    // So a body and a stand-in can serve one object in turn, and the
    // non-retired registrations stay. NO `stub_classes`: a `SyntheticStub`
    // on these classes may be the last write of a triple the JDK declares
    // `native` (`AtomicLong.VMSupportsCS8`), which must stay registered. The
    // `Array` / `FieldUpdater` classes share the name prefixes and are not
    // the family's.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkScalarAtomics,
        classes: &[
            "java/util/concurrent/atomic/AtomicInteger",
            "java/util/concurrent/atomic/AtomicLong",
            "java/util/concurrent/atomic/AtomicReference",
            "java/util/concurrent/atomic/AtomicBoolean",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[
            "java/util/concurrent/atomic/AtomicIntegerArray",
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater",
            "java/util/concurrent/atomic/AtomicLongArray",
            "java/util/concurrent/atomic/AtomicLongFieldUpdater",
            "java/util/concurrent/atomic/AtomicReferenceArray",
            "java/util/concurrent/atomic/AtomicReferenceFieldUpdater",
        ],
    },
    // nio21 wave 2 (L4): the `HttpClient` request path's stand-in families,
    // from the wave-1 exact-invocation census (`Nio20HttpHot -Dnio20.rows=hc`,
    // `--compatible` top 60: `TreeMap.put` 12 600, `TreeSet.isEmpty` 10 058,
    // `ArrayDeque.pollFirst` 21 410, `LinkedList.isEmpty` 8 583,
    // `CompletableFuture.complete` 16 776, `TimeUnit.toNanos` 5 602,
    // `LongAdder.add` 5 599 per 200 requests; none of them in the `--real-jdk`
    // list). The rule for all five: on the 25/linux `--compatible` kind map
    // EVERY registration on these class prefixes is a retired row or a
    // `SyntheticStub` (plus `TreeMap`'s seven `Bridge`s, below), so with
    // `classes` + `stub_classes` the row leaves `--compatible` with exactly
    // the registrations `--real-jdk` keeps there: none. What remains to review
    // is the VM state the stand-ins kept and who outside the registry reads it.
    //
    // `java.util.TreeMap` / `java.util.TreeSet` (`RETIRED_SHADOW_L1_TM_TRIPLES`,
    // retired under `--real-jdk` since L1 wave 8, 2026-09-12; 119 + 38 rows):
    //
    // * the stand-ins' store is `tm_array_table` / `tm_fast_table` (a Rust side
    //   table) with a one-way mirror into the real `root` / `size`
    //   (`tm_publish_real_root`). With the row no stand-in is registered, so
    //   nothing writes the side table for a JDK-built map; the readers that
    //   stay (`collect_collection_elements`, the `java/util/Map` /
    //   `SortedMap` / `NavigableMap` interface natives' `is_tree_map_receiver`
    //   routes, the view decoders) go through `tm_collect_pairs`, which
    //   arbitrates the two stores by the receiver's real `size` and reads the
    //   real `root` when the side table is empty -- the `--real-jdk` route,
    //   measured by L1 wave 8 on 163 probes (`StaleViewAddAllProbe`);
    // * VM mints: `Charset.availableCharsets` (`charset_buffers.rs`,
    //   `t3_impl.rs`, slot-written) is not registered over a 25 image (absent
    //   from the kind map); `native-builtins` `lib.rs` builds its charset map
    //   through the real `TreeMap.<init>` and `native_map_put_entry_pub`, which
    //   is the `tm_put_impl` route both modes already share;
    // * the seven non-retired `Bridge`s (`TreeMap.iterator()`, six
    //   `TreeMap$KeySet` rows: `<init>()` / `(Collection)` / `(Comparator)`,
    //   `clone`, `readObject`, `writeObject`) name methods the 25 image does
    //   not declare on those classes, so no call site reaches them and
    //   `bridges` is empty.
    //
    // Witness: `Nio21HttpFamilies.java -Dnio21.rows=tree` and
    // `Nio20HttpHot -Dnio20.rows=hdr,tmap` against HotSpot / `--real-jdk`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkTreeMap,
        classes: &["java/util/TreeMap", "java/util/TreeSet"],
        stub_classes: &["java/util/TreeMap", "java/util/TreeSet"],
        bridges: &[],
        except: &[],
    },
    // `java.util.LinkedList` / `java.util.ArrayDeque` (`RETIRED_SHADOW_L1_TRIPLES`;
    // 48 + 37 kind-map rows, every one a `synthetic-stub`). The L1 wave-3
    // authority table ("`LinkedList`: `ll_set` writes `first` / `last` /
    // `size` AND the real `LinkedList$Node` item / next / prev order;
    // `ArrayDeque`: real `elements` / `head` / `tail`") is the review:
    //
    // * `ArrayDeque`'s natives derive the count from the real `head` / `tail`
    //   (`ad_state`), so a body and a stand-in already served one deque in
    //   turn; with the row only bodies do;
    // * `LinkedList`'s `ll_overlay` is written only by `ll_set`, i.e. by the
    //   family's own natives; with the row it stays empty and every reader
    //   (`ll_get`) falls back to the real fields, as under `--real-jdk`;
    // * the VM-minted `LinkedList$ListItr` (state past the declared fields,
    //   `alloc_lli_view`) is minted only by `LinkedList.iterator` /
    //   `listIterator` x2 -- this family's rows -- so with the row every
    //   `ListItr` is the JDK's and no carrier meets a JDK body. (Contrast
    //   `ArrayList$Itr`, which `JdkArrayList` excepts because the VM mints it
    //   for `Vector` too.) `LinkedList$Itr` is a class the JDK does not
    //   declare: its three stubs go with the prefix;
    // * the `java/util/Queue` / `Deque` interface bridges (the `ArrayDeque`
    //   implementations) stay, as under `--real-jdk`: they serve a receiver
    //   with no `Code`, and both JDK classes declare every one of them.
    //
    // Witness: `Nio21HttpFamilies -Dnio21.rows=linked`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkLinkedCollections,
        classes: &["java/util/LinkedList", "java/util/ArrayDeque"],
        stub_classes: &["java/util/LinkedList", "java/util/ArrayDeque"],
        bridges: &[],
        except: &[],
    },
    // `java.util.concurrent.TimeUnit`'s nine `RETIRED_SHADOW_L5_TRIPLES` rows
    // (`reflect_annotations.rs`: a Spring partial-boot workaround, "real-JDK
    // enum conversion can NPE in partial boot"). The JDK bodies read the
    // constant's own `scale` / `maxNanos` ... fields, which `TimeUnit.<clinit>`
    // sets on a real image in both modes; the stand-ins mapped the ordinal
    // through a Rust table and kept nothing. `--real-jdk` (Spring Boot sample
    // included) has run the bodies since the L5 retirement. Witness:
    // `Nio21HttpFamilies -Dnio21.rows=timeunit`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkTimeUnit,
        classes: &["java/util/concurrent/TimeUnit"],
        stub_classes: &["java/util/concurrent/TimeUnit"],
        bridges: &[],
        except: &[],
    },
    // `LongAdder` / `DoubleAdder` (`RETIRED_SHADOW_STATELESS_TRIPLES`, 10 + 5
    // rows; every kind-map registration on the two classes). NOT stateless:
    // a contended stand-in `add` goes to a Rust cell strip
    // (`long_adder_cells`, keyed by the object) that the JDK's `Striped64`
    // bodies never see, so a body and a stand-in canNOT serve one adder in
    // turn -- which is why the family is the whole surface or nothing. With
    // the row no stand-in runs and the strip is never created; nothing outside
    // the stand-ins reads it. `Striped64` itself and the accumulators have no
    // registrations. Witness: `Nio21HttpFamilies -Dnio21.rows=adders`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkAdders,
        classes: &[
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
        ],
        stub_classes: &[
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
        ],
        bridges: &[],
        except: &[],
    },
    // `AtomicIntegerArray` / `AtomicLongArray`: the whole surface of both classes
    // (26 + 26 rows of `RETIRED_SHADOW_STATELESS_TRIPLES`, the constructors
    // included -- every registration of `register_atomic_extras_natives` on the
    // two classes is already a retired row, so the family needs no `bridges`).
    // Not stateful: the stand-ins keep the backing array in the one field the
    // real classes have (`array`), so a body and a stand-in could serve one
    // object in turn; the family is nonetheless the whole surface, not a
    // method list, so no object is ever served by half of each. The cost of
    // keeping them was the JIT: a registration in front of a body keeps the body
    // out of the compiler (`promote-refuse door_registered_native`) and seals
    // every caller (`calls-native-shadowed-method`), so `AtomicIntegerArray.get`
    // ran interpreted at 2.1 us a call and netty's `MpscIntQueue` -- an
    // `AtomicIntegerArray` subclass, the free list of every
    // `AdaptivePoolingAllocator` size class -- put an allocator cycle at 15 us
    // against 0.8 us under `--real-jdk`. `AtomicReferenceArray` is NOT in this
    // family: its stand-ins exist because the JDK body routes writes through a
    // `VarHandle` on `Object[]` (reference barriers), a separate decision.
    // Witness: `vm/tests/varhandle_array_element_fast.rs` (both modes, the
    // row on and off).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkAtomicArrays,
        classes: &[
            "java/util/concurrent/atomic/AtomicIntegerArray",
            "java/util/concurrent/atomic/AtomicLongArray",
        ],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // nio21 wave 3 (L1): `java.net.URI` (`RETIRED_SHADOW_SBR_URI_TRIPLES`, 30
    // rows; on the 25/linux kind map they are every registration on the
    // class, all `synthetic-stub`). Default ON since nio21 wave 4 (L1; the
    // row's comment). The VM-state review:
    //
    // * the stand-ins kept no side table; what made the class unadoptable was
    //   the PRODUCERS: the VM mints `URI` objects with no constructor at six
    //   sites (`make_uri` / `make_uri_units`, `URL.toURI`, `URI.create`,
    //   `Path.toUri` x3, `File.toURI`, the HTTP request URI), widths 5-18. All
    //   of them publish through `net_phase_e::uri_publish_named`, which under
    //   the row (`net_phase_e::jdk_uri_model`, `--real-jdk` or this row)
    //   copies in every instance field of a URI the JDK's own `URI(String)`
    //   built, as `--real-jdk` has done since SBR wave 1; `make_uri*`
    //   construct it outright. A mint is then field for field the object
    //   HotSpot's producer of the same text holds;
    // * the readers: `uri_raw_string` rebuilds the text the JDK's
    //   `defineString` way for a URI whose `string` cache is null (`resolve`,
    //   `normalize`, `relativize`) under the same gate. ONE reader outside
    //   `net_phase_e`, `nio_file::p57_uri_full_text`, asked `is_jdk_only`
    //   (a `Path.of(base.resolve("x"))` read raw slots with the row on); it
    //   follows the row since nio21 wave 4 (L1), which is what let it go ON.
    //
    // `URISyntaxException` shares the prefix and is not the family's.
    // Witness: `Nio21W3Doors.java -Dnio21.rows=uri` against
    // HotSpot, `--compatible` (and `CRATONVM_COMPAT_ALIGN=-jdk-uri` for the
    // stand-ins). (The w3 note named a `Nio21UriFamily` probe that was never
    // written.)
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkUri,
        classes: &["java/net/URI"],
        stub_classes: &["java/net/URI"],
        bridges: &[],
        except: &["java/net/URISyntaxException"],
    },
    // nio21 wave 3 (L1): `java.io.ByteArrayOutputStream`
    // (`RETIRED_SHADOW_L4_TRIPLES`, 13 rows; every registration on the class on
    // the 25/linux kind map, all `synthetic-stub`). Default ON since nio21
    // wave 4 (L1): the fixed-length upload's Java-facing stream is the JDK's
    // own `HttpURLConnection$StreamingOutputStream` over the carrier
    // (`http_url_connection::huc_streaming_output_stream`), so no HTTP state
    // rides a `ByteArrayOutputStream` native. The VM-state review:
    //
    // * the stand-ins (`native-io` `native_baos_*`) are layout code over the
    //   real `buf` / `count` (slots 0 / 1); since nio21 wave 2 the
    //   `HttpURLConnection` live-upload hook rides its own carrier class
    //   (`cratonvm/internal/HucBodyStream`, not under this prefix), whose
    //   natives fall back to the BAOS method non-virtually -- the JDK body
    //   under the row. With `CRATONVM_HUC_BODY_STREAM_CARRIER=0` the plain
    //   stream's hook goes with the rows, so `huc_get_output_stream` sends a
    //   fixed-length body buffered instead of starting a live upload;
    // * VM mints (`huc_get_output_stream`, `phases_early`'s synthetic
    //   `getOutputStream`) write `buf` = a `byte[]` and `count` = 0, which the
    //   JDK's `ensureCapacity` grows like any other;
    // * `java/io/OutputStream.write(I)V` (abstract in `java.base`, a `Bridge`)
    //   and `cratonvm/synthetic/ProcessPipeOutputStream`'s own five natives
    //   (an `OutputStream` subclass, NOT a `ByteArrayOutputStream`) are other
    //   classes and stay.
    //
    // Witness: `R16Fieldsite2Layout -Dfs2.rows=osw` and `Nio21HttpFamilies`
    // with `CRATONVM_COMPAT_ALIGN=+jdk-byte-array-streams`.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkByteArrayStreams,
        classes: &["java/io/ByteArrayOutputStream"],
        stub_classes: &["java/io/ByteArrayOutputStream"],
        bridges: &[],
        except: &[],
    },
    // `java.util.concurrent.CountDownLatch` (interpreter round i3 wave 71,
    // lane L1; the wave-70 `StampedLock` shape). No row is retired: the
    // stand-ins are eleven stated `SyntheticStub` registrations
    // (`native-builtins` `util_concurrent_ext::register_concurrent_natives`:
    // `<init>(I)V`, `countDown`, both `await`s, `getCount`, `toString`; and
    // the `lib.rs` `surefire_cdl` early copy of the first five, through
    // `register_with_kind`), which `--real-jdk` refuses as a whole surface.
    // JDK 25 declares no `native` method on the class; its bodies need only
    // AQS (`Unsafe` CAS on `state`, `LockSupport.park(this)`: the
    // `JdkLockSupport` row's bodies, or the VM's stand-ins with that row off).
    // The stand-ins kept the count in an `int[]` holder written into the
    // `sync` slot, so they are all-or-nothing: the family is its stub class.
    // The prefix also names `CountDownLatch$Sync`, which has no stand-in (and
    // so loses nothing). `classes` names it too, so a row a later wave retires
    // on it joins the family without a line here.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkCountDownLatch,
        classes: &["java/util/concurrent/CountDownLatch"],
        stub_classes: &["java/util/concurrent/CountDownLatch"],
        except: &[],
        bridges: &[],
    },
    // `java.util.concurrent.locks.AbstractQueuedLongSynchronizer` (interpreter
    // round i3 wave 72, lane L1). Its three rows (`getState`, `setState`,
    // `compareAndSetState`) are retired in `RETIRED_SHADOW_*` above, so
    // `--real-jdk` runs the JDK's bodies: a `getfield` / `putfield` of the
    // `volatile long state` and `Unsafe.compareAndSetLong(this, STATE, ..)`,
    // the same CAS `AtomicLong` runs in both modes. The stand-ins
    // (`native-builtins` `util_concurrent_ext::register_aqls_state_natives`,
    // inheriting `register_essential_natives_with_shims`' ambient `Bridge`,
    // re-tagged `SyntheticStub` as retired rows) kept the word in a
    // process-global side table and never wrote the field; since wave 73 the
    // `=0` arm's stand-ins read and CAS that same `state` field, and the
    // table is gone. No companion stubs; the
    // prefix also names `$ConditionObject` / `$Node` and friends, which have
    // no stand-in.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkAqls,
        classes: &["java/util/concurrent/locks/AbstractQueuedLongSynchronizer"],
        stub_classes: &[],
        except: &[],
        bridges: &[],
    },
    // `java.util.concurrent.CopyOnWriteArrayList` (interpreter round i3 wave
    // 72, lane L1). Its 16 rows are retired in `RETIRED_SHADOW_*` above (the
    // eleven readers and mutators plus `addAll(Collection)`, `bulkRemove`,
    // `contains`, `indexOf`, `iterator`), so `--real-jdk` runs the JDK's
    // bodies. Every `--compatible` stand-in on the class is one of those rows
    // -- the fourteen `Bridge`s of `native-builtins`
    // `util_concurrent_ext::register_concurrent_natives`, the four of
    // `native-collections` `register_concurrent_completeness_natives`, and
    // the `vm_init` `addIfAbsent` -- so `classes` drops them all whatever
    // their kind, and no `bridges` list is needed. The VM-state review: the
    // stand-ins read and wrote the real `array` field (the synthetic two-slot
    // layout only on the synthetic library), so a JDK body reads what a
    // stand-in wrote; but they locked the LIST's monitor where the JDK locks
    // `lock`, so mixing the two lost updates
    // (`i71-L1-compatible-copyonwritearraylist-bridges-lock-the-list-not-its-lock-20261104`).
    // A COWAL a native mints without its constructor (`native-collections`'
    // `CopyOnWriteArraySet` iterator delegate) has a null `lock`; the JDK
    // reads `lock` only in mutators, and that delegate only iterates.
    // `CopyOnWriteArraySet` is NOT in it (its rows are the `SET_CLASSES`
    // surface routed by `cow_set_route`). Default on since the wave-74 triage.
    // Since wave 76 (lane L1) the stand-ins are registered on the synthetic
    // library only (`if !registry.real_jdk()` at both registrars), so over a
    // real JDK this entry drops nothing the registrars still offer: it stays
    // as the guard for a COWAL row some other registrar might add.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkCopyOnWrite,
        classes: &["java/util/concurrent/CopyOnWriteArrayList"],
        stub_classes: &[],
        except: &[],
        bridges: &[],
    },
    // `java.lang.invoke` (nio21 wave 12, lane L3): the handle family `--real-jdk`
    // retired AS ONE UNIT (`RETIRED_SHADOW_SBR4_MHCONS_TRIPLES`, the union of
    // the SBR2/SBR3 held tables: a carrier combinator left native reads garbage
    // from a real handle a retired `Lookup.find*` built, and the other way
    // round), plus the other retired rows on the package (`MethodType`,
    // `MemberName`, `VarHandle.varType` / `coordinateTypes` /
    // `isAccessModeSupported` / `toMethodHandle`, the `CallSite`
    // `dynamicInvoker`s) and its companion stubs (`MethodHandleProxies`,
    // `InvokerBytecodeGenerator`). So the row is `--real-jdk`'s registration
    // policy for exactly this package, never a subset that would leave carriers
    // and real handles meeting: MEASURED on a 2026-10-06 build, the
    // `--real-jdk-report` of `Nio21ConcreteReceivers` refuses 105 registrations
    // on `java/lang/invoke/` (98 `lang_invoke.rs`, 4 `lookup_define.rs`, 3
    // `lib.rs`), every one a retired row or a stated stub, and records no
    // `java/lang/invoke` `Bridge` shadowing bytecode, so `bridges` is empty
    // until a broader census names one. What stays is what stays under
    // `--real-jdk`: the `ACC_NATIVE` doors (`invokeExact` / `invoke` /
    // `invokeBasic` / `linkTo*`, the `VarHandle` access modes,
    // `MethodHandleNatives`), which serve a real handle and a carrier alike.
    // The VM-state review: a carrier's state is its own slots and the
    // `vh_meta` / `MH_*` side tables, which a JDK-built handle never reads;
    // the one cross-over is a carrier minted OUTSIDE these classes'
    // registrations (`invokedynamic`'s lambda linkage, `panama`'s downcall
    // handle, `ObjectStreamClass$RecordSupport.deserializationCtr`), and the
    // `ACC_NATIVE` doors already serve both shapes under `--real-jdk`. Default
    // ON since the 2026-10-09 census (`HotSpotAlignment::default_on`;
    // `CRATONVM_COMPAT_ALIGN=-jdk-invoke` reverts).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkInvoke,
        classes: &["java/lang/invoke/"],
        stub_classes: &["java/lang/invoke/"],
        except: &[],
        bridges: &[],
    },
    // `java.util.concurrent.CopyOnWriteArraySet` (interpreter round i3 wave
    // 74, lane L1; `i72-L1-proposal-a-copyonwritearrayset-family-row-20261105`).
    // Its `--compatible` surface is the 24 `Bridge`s `native-collections`
    // `register_hashset_natives` mirrors onto every `SET_CLASSES` name. Three
    // are retired rows (`<init>()V`, `isEmpty`, `iterator`;
    // `RETIRED_SHADOW_SBR2_SPRINGAPP_TRIPLES`), which `classes` takes. The
    // other 21 are listed in `bridges`, each reviewed for the same reason:
    // over a real JDK every one of them already hands a real set to its own
    // bytecode (`cow_set_route`, an `invoke_special_bytecode_only` on the
    // receiver's class: the JDK's method where `CopyOnWriteArraySet` declares
    // it, `AbstractSet` / `AbstractCollection` / `Collection`'s where it
    // inherits it), so the row changes who decides, not what runs. The
    // fabricated set of the synthetic library (where the map surface IS the
    // implementation) is untouched: the row needs a real JDK. `<init>(I)V` and
    // `<init>(IF)V` name constructors JDK 25's class does not declare, so no
    // call can reach them over a real JDK; they go with the family so that the
    // class keeps no registration at all with the row on (the census in
    // `native-builtins/tests/juc_compat_surface.rs` asserts zero). The VM-state
    // review: the routed natives keep no state of their own for a real set
    // (`hs_map_slot` answers `None` for one), so no side table outlives the
    // drop. Default on since the wave-74 triage.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkCopyOnWriteArraySet,
        classes: &["java/util/concurrent/CopyOnWriteArraySet"],
        stub_classes: &[],
        except: &[],
        bridges: &[
            ("java/util/concurrent/CopyOnWriteArraySet", "<init>", "(I)V"),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "<init>",
                "(IF)V",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "<init>",
                "(Ljava/util/Collection;)V",
            ),
            ("java/util/concurrent/CopyOnWriteArraySet", "size", "()I"),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "add",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "remove",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "contains",
                "(Ljava/lang/Object;)Z",
            ),
            ("java/util/concurrent/CopyOnWriteArraySet", "clear", "()V"),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "toArray",
                "()[Ljava/lang/Object;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "toArray",
                "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "toString",
                "()Ljava/lang/String;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "hashCode",
                "()I",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "equals",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "forEach",
                "(Ljava/util/function/Consumer;)V",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "stream",
                "()Ljava/util/stream/Stream;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "spliterator",
                "()Ljava/util/Spliterator;",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "addAll",
                "(Ljava/util/Collection;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "removeAll",
                "(Ljava/util/Collection;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "retainAll",
                "(Ljava/util/Collection;)Z",
            ),
            (
                "java/util/concurrent/CopyOnWriteArraySet",
                "containsAll",
                "(Ljava/util/Collection;)Z",
            ),
        ],
    },
    // `java.util.concurrent.PriorityBlockingQueue` (interpreter round i3 wave
    // 75, lane L1; candidate 3 of
    // `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`).
    // Its nine rows are retired in `RETIRED_SHADOW_L5_TRIPLES` (`<init>()V`,
    // `isEmpty`, `offer`, `peek`, both `poll`s, `put`, `size`, `take`), so
    // `--real-jdk` runs the JDK's bodies. Every `--compatible` stand-in on the
    // class over a real JDK is one of those rows: the nine `Bridge`s of
    // `native-collections` `register_priority_blocking_queue_natives` (called
    // from `register_collections_natives`; `native-builtins`
    // `register_p65_priority_blocking_queue` is `synthetic-jdk`-only, through
    // `register_synthetic_overrides`), so `classes` drops them all and no
    // `bridges` list is needed. The stand-ins are all-or-nothing, and the
    // family drops all of them together: they keep a SORTED array (natural
    // order, the queue's `comparator` ignored) in the real `queue` / `size`
    // fields, which the JDK's own `toString` / `iterator` / `remove(Object)` /
    // `drainTo` / `Collection` constructor treat as a binary heap. The
    // VM-state review: the stand-ins keep no side table (only the two fields,
    // plus a `lock` / `notEmpty` their `<init>()V` seeds because it skips the
    // JDK constructor), and they wait on the queue's own monitor, so a JDK
    // body never waits on anything a stand-in signals. The prefix also names
    // `$PBQSpliterator`, which has no stand-in. Default ON since the wave-75
    // triage (`HotSpotAlignment::default_on`;
    // `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE=0` reverts).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkPriorityBlockingQueue,
        classes: &["java/util/concurrent/PriorityBlockingQueue"],
        stub_classes: &[],
        except: &[],
        bridges: &[],
    },
    // `java.util.concurrent.ConcurrentHashMap` and every nested class of it
    // (`$KeySetView`, `$ValuesView`, `$EntrySetView`, the iterators,
    // `$MapEntry`, `$Node`; interpreter round i3 wave 76, lane L1;
    // `i75-L1-proposal-a-concurrenthashmap-family-row-20261108`). 144 of its
    // rows are retired (`RETIRED_SHADOW_PHASE3_TRIPLES` and its siblings), so
    // `--real-jdk` runs the JDK's map over the JDK's `table`; `classes` takes
    // those from whichever registrar makes them. The stand-ins are the
    // "CLUSTER NOTE (H4-1)" cluster above `native-collections`
    // `register_concurrent_hashmap_natives`, closed under minting: that
    // registrar and the view / iterator / entry carrier loops skip every
    // registration of theirs on these classes under the row (retired or not),
    // so no `bridges` list is needed for them; `native-builtins`
    // `register_m18_concurrent_fixes`' six CHM rows are all retired, so
    // `classes` takes them. The VM-state review: the stand-ins keep every entry in a
    // SEGMENTED store hung off `AbstractMap.keySet` (slot 0), which the JDK
    // bodies never read and the JDK's own `table` / `baseCount` never mirror,
    // so the family is all-or-nothing (the 2026-08-30 bisect: arming only the
    // nested classes broke the map). What stays, deliberately: the five
    // `java/util/concurrent/ConcurrentMap` INTERFACE rows (another class name,
    // so not this family; under the row they are registered as doors that
    // hand a CHM receiver to its own bytecode), and the `tabAt` / `setTabAt` /
    // `casTabAt` intrinsics, which operate on the JDK's real `table` array.
    //
    // nio21 (wave 1, L4) built the same row independently; merged here at the
    // nio21 dev merge, which adds from that side: `java.util.Properties`, the
    // other half of the PHASE3 pair `RETIRED_SHADOW_PHASE3_TRIPLES` retired
    // (its 185 rows over the two classes and their `$` views and iterators),
    // because either half alone is a split store -- its stand-ins kept their
    // own side table over the segmented map, and with the row every JDK
    // `Properties` body reads a JDK-built `map` -- and both classes'
    // `SyntheticStub`s (`stub_classes`), as `--real-jdk` refuses them. The
    // review on that side:
    //
    // * the JIT leaf door (`collection_leaf_kind` `ChmGet`) binds only a site
    //   that RESOLVED to `native_chm_get`, so it is inert with the row; the
    //   direct helper `jit_concurrent_hashmap_get_direct` declines under the
    //   row (`helpers.rs` `chm_gate_under_jdk_model`);
    // * the per-thread String-node memo is filled only by `native_chm_get`;
    // * the interpreter's force-native lists (`native_override.rs`) resolve
    //   no id for an unregistered triple and decline, as under `--real-jdk`;
    // * `native-collections`' own readers (the `map_collect_*` collectors,
    //   `properties_backing_chm`, the `java/util/Map` interface natives'
    //   CHM reroutes) follow `chm_jdk_model`, which is this row in either
    //   mode;
    // * the unconstructed mints (`classloader*.rs`) are all-zero maps, which
    //   the JDK bodies take as empty (`table == null`); the constructed ones
    //   run `<init>`;
    // * the system properties follow the row since nio21 wave 2 (L1): the
    //   `initPhase1` publish of the real `System.props` (`replace_real_map`),
    //   the `sysprops_merge` read-through, `System.getProperty`'s removal
    //   check, `fresh_boot_properties` and the `setProperties` re-publish ask
    //   `aligned(JdkConcurrentHashMap)` where they asked `is_jdk_only`
    //   (`r21-compatible-chm-row-needs-the-system-properties-arms-FIXED`); the
    //   three `System` property rows go with the row
    //   (`is_chm_pair_system_property_row`).
    //
    // That side also listed the five `ConcurrentMap` interface rows in
    // `bridges` (dropping them); the merge keeps this side's doors instead
    // (cluster note item 1: they serve receivers whose class declares
    // nothing, and under the row they hand a CHM to its own bytecode).
    // Witness: `Nio20CompatRows.java` (`chm*` rows),
    // `Nio20LocaleProviders`, `Nio21ChmPair`. Default ON (see the row's doc).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap,
        classes: &[
            "java/util/concurrent/ConcurrentHashMap",
            "java/util/Properties",
        ],
        stub_classes: &[
            "java/util/concurrent/ConcurrentHashMap",
            "java/util/Properties",
        ],
        except: &[],
        bridges: &[],
    },
    // A child process (interpreter round i3 wave 80, lane L6;
    // `i59-L6-compatible-synthetic-process-lists-no-jdk-frames-20261023.md`).
    // No row is retired: `--real-jdk` already runs the JDK's `ProcessBuilder`
    // and `ProcessImpl` because every stand-in is a stated `SyntheticStub`
    // (`native-io` `register_process_natives`: `ProcessBuilder.start` and the
    // minted `cratonvm/synthetic/Process*` classes; `native-builtins`
    // `register_phase57_process`: the `ProcessBuilder` constructors and
    // accessors). So the family is its stub classes -- `java/lang/Process*`
    // (that is, `ProcessBuilder` and its nested classes) and the minted
    // classes -- and the 13 `java/lang/Process` rows `register_process_natives`
    // keeps as ambient `Bridge`s on the abstract class (call-site keyed
    // lookups for the minted receiver): `--compatible`'s doors are native
    // first, so a `Process.waitFor()` call site would hand a JDK-built
    // `ProcessImpl` to a native that reads the minted layout. Each of those
    // methods is abstract or has `Code` on JDK 25 `Process`, and `ProcessImpl`
    // overrides the abstract ones. Not this family: `ProcessHandle` /
    // `ProcessHandleImpl` (their stand-ins delegate to the real
    // `ProcessHandleImpl` over a real JDK), `ProcessEnvironment` (`environ()`
    // is `ACC_NATIVE`) and `ProcessImpl` (its natives are the bridges the
    // JDK body calls). `Runtime.exec` is the `JdkRuntime` family's (default
    // on: the JDK's `exec` reaches `ProcessBuilder.start()`). Default ON since
    // the wave-81 triage (`HotSpotAlignment::default_on`;
    // `CRATONVM_COMPAT_JDK_PROCESS=0` reverts).
    //
    // Wave 80 host run (`L6W59BlockedNativeStates` / `L6W80CompatJdkProcess`
    // with the row): the JDK's `ProcessImpl` stdout is a
    // `Process$PipeInputStream` (a `FileInputStream` subclass), whose `read`
    // reached `native-io`'s public-surface `FileInputStream` stand-ins (stated
    // `SyntheticStub`s over concrete bytecode) and so listed no
    // `FileInputStream.read` / `readBytes` frame. So the family takes
    // `FileInputStream`'s stubs too: under the row its public surface runs the
    // JDK bodies over the `readBytes` / `read0` / `available0` bridges, as
    // under `--real-jdk` (the VM's minted `FileInputStream`s carry a real
    // layout with every constructor field backfilled, so the bodies run on
    // them too).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkProcess,
        classes: &["java/lang/Process"],
        stub_classes: &[
            "java/lang/Process",
            "cratonvm/synthetic/Process",
            "java/io/FileInputStream",
        ],
        except: &[
            "java/lang/ProcessHandle",
            "java/lang/ProcessEnvironment",
            "java/lang/ProcessImpl",
        ],
        bridges: &[
            ("java/lang/Process", "waitFor", "()I"),
            (
                "java/lang/Process",
                "waitFor",
                "(JLjava/util/concurrent/TimeUnit;)Z",
            ),
            ("java/lang/Process", "exitValue", "()I"),
            ("java/lang/Process", "isAlive", "()Z"),
            ("java/lang/Process", "destroy", "()V"),
            (
                "java/lang/Process",
                "destroyForcibly",
                "()Ljava/lang/Process;",
            ),
            ("java/lang/Process", "pid", "()J"),
            (
                "java/lang/Process",
                "toHandle",
                "()Ljava/lang/ProcessHandle;",
            ),
            (
                "java/lang/Process",
                "getInputStream",
                "()Ljava/io/InputStream;",
            ),
            (
                "java/lang/Process",
                "getErrorStream",
                "()Ljava/io/InputStream;",
            ),
            (
                "java/lang/Process",
                "getOutputStream",
                "()Ljava/io/OutputStream;",
            ),
            (
                "java/lang/Process",
                "descendants",
                "()Ljava/util/stream/Stream;",
            ),
            (
                "java/lang/Process",
                "onExit",
                "()Ljava/util/concurrent/CompletableFuture;",
            ),
        ],
    },
    // `java.util.concurrent.CompletableFuture` (interpreter round i3 wave 80,
    // lane L1; candidate 4 of
    // `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`).
    // Its eleven rows are retired in `RETIRED_SHADOW_L5_TRIPLES` (`allOf`,
    // `anyOf`, `complete`, `completeExceptionally`, `completeValue`,
    // `isCompletedExceptionally`, `thenCombine` and the four one-argument
    // `*Async` stages), so `--real-jdk` runs the JDK's bodies; `classes` takes
    // them from whichever registrar makes them: `native-collections`
    // `register_concurrent_completeness_natives` (the six non-async rows; the
    // four async ones only with `CRATONVM_CF_DELEGATING_YIELD=0`) and
    // `native-builtins` `register_essential_natives_with_shims`'
    // `completeValue`. `bridges` lists the seven delegating stages the same
    // registrar adds on the class with `CRATONVM_CF_DELEGATING_YIELD=0` (not
    // retired: under `--real-jdk` they are `Bridge`s the class's concrete
    // bytecode beats at dispatch); each one's real-JDK arm is a call of the
    // JDK's own `uni*Stage`, so the row changes who runs the null checks, not
    // what completes. The five `CompletionStage` INTERFACE rows of that
    // registrar are another class and stay. The VM-state review: the
    // stand-ins write only the JDK's own `result` / read `stack` (a real CF),
    // and keep no side table, so a JDK body reads what a stand-in wrote; the
    // two-slot synthetic CFs (an Int `done` in slot 1) other natives mint
    // reach the JDK bodies under the row, as they do under `--real-jdk`. The
    // prefix also names `$AltResult`, `$Signaller` and the `Completion`
    // classes, which have no stand-in.
    //
    // nio21 (wave 2, L4) built the same row independently; merged here at the
    // nio21 dev merge, which adds the class prefix's `SyntheticStub`s
    // (`stub_classes`), as `--real-jdk` refuses them. Its review: the
    // slot-written stand-in (`cf_make_synthetic_slots`) is minted only when
    // the real constructor AND `completedFuture` both fail, in either mode;
    // `http2.rs`'s two-slot CF is a carrier of the `jdk-http-client` family,
    // not minted with that row on. The census priced the stand-ins at nothing
    // (`CRATONVM_NATIVE_CF_COMPLETE=0`: `cf` 7.1-12.5 us vs 7.4-9.8 us,
    // noise). Witness: `Nio21HttpFamilies -Dnio21.rows=cf`. Default ON (see
    // the row's doc).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkCompletableFuture,
        classes: &["java/util/concurrent/CompletableFuture"],
        stub_classes: &["java/util/concurrent/CompletableFuture"],
        except: &[],
        bridges: &[
            (
                "java/util/concurrent/CompletableFuture",
                "thenApply",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "thenAccept",
                "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "thenRun",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "thenCompose",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "exceptionally",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "handle",
                "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
            ),
            (
                "java/util/concurrent/CompletableFuture",
                "whenComplete",
                "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
            ),
        ],
    },
    // `sun.misc.Unsafe` (JIT round 21, nio21 wave 13, lane L4;
    // `r21-plain-run-stderr-parity`): the legacy facade's retired rows
    // (`RETIRED_SHADOW_L5R_TRIPLES` and the later tables). Each JDK 25 body is
    // `beforeMemoryAccess()` (JEP 498: the first legacy memory access prints
    // HotSpot's four-line "terminally deprecated" warning, once, and honours
    // `--sun-misc-unsafe-memory-access`) and one call of the same method on
    // `jdk.internal.misc.Unsafe`, whose natives both modes keep. The
    // stand-ins held no state of their own, and skipped the warning, so a
    // `--compatible` run of any `sun.misc.Unsafe` user (jctools, netty,
    // Kryo) differed from HotSpot on stderr. `--real-jdk` has run the bodies
    // since the table's retirement. No companion stubs, no bridges.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkSunMiscUnsafe,
        classes: &["sun/misc/Unsafe"],
        stub_classes: &[],
        bridges: &[],
        except: &[],
    },
    // `java.lang.foreign` / `jdk.internal.foreign` (the compat-FFM lane;
    // `compatible-mode-array-element-and-filter-value-bridges-need-lambdaform-carriers`).
    // Every FFM row `--real-jdk` retires (the SBR2 / SBR3 / SBR4 / SBR5 FFM tables,
    // `RETIRED_SHADOW_SBR5_AVH_TRIPLES`, the layout `varHandle` rows) is not
    // registered, and the family's `SyntheticStub`s (which `--real-jdk` refuses)
    // go with them, so a segment is a real `NativeMemorySegmentImpl`, an arena a
    // real `ArenaImpl`, and `layout.varHandle()` / `arrayElementVarHandle()` the
    // JDK's real handles. `Bridge` and `Intrinsic` rows are never touched, so what
    // stays native in `--real-jdk` (the linker, the held rows) stays native here.
    // Default ON.
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkForeign,
        classes: &["java/lang/foreign/", "jdk/internal/foreign/"],
        stub_classes: &["java/lang/foreign/", "jdk/internal/foreign/"],
        bridges: &[],
        except: &[],
    },
    // `java.util.concurrent.ThreadPoolExecutor` and
    // `ScheduledThreadPoolExecutor` (interpreter round i3 wave 82, lane L1;
    // candidate 5 of
    // `i71-L1-proposal-juc-family-rows-for-the-remaining-compatible-stand-ins-20261104`,
    // and the router-family shape of
    // `i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107`).
    // Fifteen `ThreadPoolExecutor` rows and the two `ScheduledThreadPoolExecutor`
    // rows are retired in `RETIRED_SHADOW_L5_TRIPLES` (`submit` x3, `invokeAny`,
    // `shutdown`, `shutdownNow`, `isShutdown`, `isTerminated`,
    // `awaitTermination` and the six stat getters), so `--real-jdk` runs the
    // JDK's bodies; `classes` takes them from whichever registrar makes them
    // (`native-builtins` `register_executor_natives`, `native-collections`
    // `register_concurrent_completeness_natives`). `execute` is a stated
    // `SyntheticStub` (`register_executor_natives`), which `stub_classes` takes.
    // No `bridges`: no non-retired, non-stub registration names either class.
    // The VM-state review: every one of these natives is a router -- for a
    // real pool it hands the receiver to `ThreadPoolExecutor`'s own body
    // (`tp_real_call`, `es_real_pool_door`, `executor_is_real` ->
    // `invoke_special_bytecode_only`) and keeps no state; only the synthetic
    // two-slot executor (`Executors` factories, which the real-JDK boot does
    // not register: `drop_real_layout_synthetic`) reads their slots. So the
    // answer is the JDK's with the row on or off; the row removes the door,
    // the protected-stub arbitration and the router per call. The
    // `ExecutorService` interface rows are another class and stay (abstract
    // methods; `--real-jdk` keeps the `Bridge`s too). The prefixes also name
    // `$Worker`, the policies and `$DelayedWorkQueue`, which have no stand-in.
    // `ScheduledThreadPoolExecutor`'s registrations are already dropped over a
    // real JDK by `drop_real_layout_synthetic`; naming it keeps the family
    // whole if a registrar ever adds one. Default off (see the row's doc).
    CompatFamily {
        row: cratonvm_types::HotSpotAlignment::JdkThreadPools,
        classes: &[
            "java/util/concurrent/ThreadPoolExecutor",
            "java/util/concurrent/ScheduledThreadPoolExecutor",
        ],
        stub_classes: &[
            "java/util/concurrent/ThreadPoolExecutor",
            "java/util/concurrent/ScheduledThreadPoolExecutor",
        ],
        except: &[],
        bridges: &[],
    },
];

/// Is the triple one of the carrier-thread-local rows the
/// `JdkCarrierThreadLocals` row leaves to the JDK's bodies (nio21 wave 2, L4):
/// the eight `System$1` / `System$2` accessors of
/// `RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES`, or the four `sun.nio.ch.Util`
/// temporary-buffer rows of `RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES`? The two
/// tables are one decision (the Util table's own note: the real `Util`
/// bytecode over the shared carrier map is the `"buf" is null` crash), so one
/// row takes both. Not class families: `System$1` / `System$2` carry many other
/// retired rows this row must not take. Cold: registration only.
pub(crate) fn is_carrier_thread_local_family_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "java/lang/System$1" | "java/lang/System$2" => RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor),
        "sun/nio/ch/Util" => RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor),
        _ => false,
    }
}

/// Is `(class, method, descriptor)` `JavaLangAccess.blockedOn(Interruptible)`
/// on its `System$1` carrier: the hook `AbstractInterruptibleChannel.begin()`
/// and `AbstractSelector.begin()` publish their `Interruptible` through
/// (`Thread.blockedOn`, which sets `Thread.nioBlocker`)? A row of
/// `RETIRED_SHADOW_SBR_NIOREAL_TRIPLES` (non-Windows), whose `--compatible`
/// stand-in (`shared_secrets_bridge::jla_blocked_on`) is a no-op, so a real
/// channel blocked in a read never learns of an interrupt. It belongs to every
/// family whose channels are the JDK's own -- `JdkPipe` (default on: a real
/// `Pipe.source().read` parked forever on `Thread.interrupt()`, where HotSpot
/// throws `ClosedByInterruptException`) and `JdkNioChannels` -- so the
/// registry drops it under either (JIT round 20, nio20, lane L4). `System$1`'s
/// other retired rows are not touched. Cold: registration only.
pub(crate) fn is_nio_blocker_hook(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    class_name == "java/lang/System$1"
        && method_name == "blockedOn"
        && descriptor == "(Lsun/nio/ch/Interruptible;)V"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is `(class, descriptor)` part of the stream surface the `JdkStreams` row
/// leaves to the JDK's bytecode: a `java/util/stream/` class (pipelines, sources,
/// `Collectors`, `StreamSupport`), or any method whose return type is one of the
/// four stream interfaces (`Collection.stream`, `Arrays.stream`,
/// `BufferedReader.lines`, `Files.lines`, `Optional.stream`, ...). Cold:
/// registration only (SBR round 5, lane compat2).
///
/// The ABSTRACT methods of the stream interfaces are not in it: under `--real-jdk`
/// they stay registered (a `Bridge` with no `Code` to yield to), and they are what
/// serves a carrier a native still hands out (the `StackWalker.walk` stream); a
/// pipeline's real receiver dispatches to its own bytecode, so they never run
/// for one.
pub(crate) fn is_stream_surface(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if class_name.starts_with("java/util/stream/") {
        return !is_abstract_stream_interface_method(class_name, method_name);
    }
    descriptor.ends_with(")Ljava/util/stream/Stream;")
        || descriptor.ends_with(")Ljava/util/stream/IntStream;")
        || descriptor.ends_with(")Ljava/util/stream/LongStream;")
        || descriptor.ends_with(")Ljava/util/stream/DoubleStream;")
}

/// `true` for a method of `BaseStream` / `Stream` / `IntStream` / `LongStream` /
/// `DoubleStream` / `Collector` that the JDK declares ABSTRACT (everything but
/// the static factories and the three-or-four `default` methods listed here).
pub(in crate::retired_shadow) fn is_abstract_stream_interface_method(class_name: &str, method_name: &str) -> bool {
    let concrete: &[&str] = match class_name {
        "java/util/stream/BaseStream" => return true,
        "java/util/stream/Stream" => &[
            "builder",
            "concat",
            "empty",
            "generate",
            "iterate",
            "of",
            "ofNullable",
            "dropWhile",
            "takeWhile",
            "mapMulti",
            "mapMultiToInt",
            "mapMultiToLong",
            "mapMultiToDouble",
            "toList",
            "gather",
        ],
        "java/util/stream/IntStream" | "java/util/stream/LongStream" => &[
            "builder",
            "concat",
            "empty",
            "generate",
            "iterate",
            "of",
            "range",
            "rangeClosed",
            "dropWhile",
            "takeWhile",
            "mapMulti",
        ],
        "java/util/stream/DoubleStream" => &[
            "builder",
            "concat",
            "empty",
            "generate",
            "iterate",
            "of",
            "dropWhile",
            "takeWhile",
            "mapMulti",
        ],
        "java/util/stream/Collector" => &["of"],
        _ => return false,
    };
    method_name != "<clinit>" && !concrete.contains(&method_name)
}

/// The concrete provider classes `--compatible` stamps its providers with
/// (`nio_file::FILE_SYSTEM_PROVIDER_IMPLS`, kept here by name: this crate sits
/// below `native-builtins`).
pub(in crate::retired_shadow) const ARCHIVE_PROVIDER_IMPL_CLASSES: &[&str] = &[
    "sun/nio/fs/LinuxFileSystemProvider",
    "sun/nio/fs/MacOSXFileSystemProvider",
    "sun/nio/fs/BsdFileSystemProvider",
    "sun/nio/fs/AixFileSystemProvider",
    "sun/nio/fs/SolarisFileSystemProvider",
    "sun/nio/fs/WindowsFileSystemProvider",
    "sun/nio/fs/UnixFileSystemProvider",
];

/// Is `(class, method, descriptor)` one of `RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES`,
/// or the `FileSystemProvider.newFileSystem(Path, Map)` row mirrored onto a
/// concrete provider class? The `JdkArchiveFs` row drops them under `--compatible`.
pub(crate) fn is_archive_provider_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES
        .iter()
        .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
        || (method_name == "newFileSystem"
            && descriptor == "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;"
            && ARCHIVE_PROVIDER_IMPL_CLASSES.contains(&class_name))
}

/// The `--compatible` Bridges over `jdk.compiler`'s file manager
/// (`native-builtins` `register_essential_natives_with_shims`, bodies in
/// `test_frameworks.rs`; force-native in `native_override.rs`). They are not
/// a `RETIRED_SHADOW_*` table: under `--real-jdk` a Bridge over concrete
/// bytecode never wins, so nothing had to retire them there. Under
/// `--compatible` they ran and answered differently from the JDK bodies (see
/// `HotSpotAlignment::JdkJavacFileManager`); the `JdkJavacFileManager` row
/// drops them. JIT round 20 wave 14, lane cjavac
/// (`docs/feature-designs/native-dispatch-architecture.md`).
pub(crate) const COMPAT_JAVAC_FILE_MANAGER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "com/sun/tools/javac/file/JavacFileManager",
        "checkNotModuleOrientedLocation",
        "(Ljavax/tools/JavaFileManager$Location;)V",
    ),
    (
        "com/sun/tools/javac/file/JavacFileManager",
        "list",
        "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;",
    ),
    (
        "com/sun/tools/javac/file/JavacFileManager",
        "inferBinaryName",
        "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;",
    ),
    (
        "com/sun/tools/javac/file/JavacFileManager$PathAndContainer",
        "compareTo",
        "(Ljava/lang/Object;)I",
    ),
    ("com/sun/tools/javac/file/RelativePath", "hashCode", "()I"),
    ("com/sun/tools/javac/file/RelativePath", "equals", "(Ljava/lang/Object;)Z"),
    (
        "com/sun/tools/javac/file/RelativePath",
        "compareTo",
        "(Lcom/sun/tools/javac/file/RelativePath;)I",
    ),
    ("com/sun/tools/javac/file/RelativePath", "getPath", "()Ljava/lang/String;"),
];

/// Is `(class, method, descriptor)` one of [`COMPAT_JAVAC_FILE_MANAGER_TRIPLES`]?
/// Cold: registration only.
pub(crate) fn is_javac_file_manager_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name.starts_with("com/sun/tools/javac/file/")
        && COMPAT_JAVAC_FILE_MANAGER_TRIPLES
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// `--real-jdk` `Bridge` registrations over a JDK 25 method that has `Code`
/// (declared or inherited), which the strict boot does not make at all
/// (`NativeMethodRegistry::register`). JIT round 20 wave 17, lane bridge: the
/// rows behind the bridge ratchet's strict "Bridge registrations that STATE
/// their kind and shadow concrete bytecode" (38 on 25/linux against a frozen
/// 26; `regression-suite/bridge-ratchet.sh`, owed item 3 of
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// Why dropping them is not a behaviour change: since 2026-10-08
/// (`3d31008561`) every policy-checked door yields a `Bridge` to the Code of
/// the method it resolves (`resolve_dispatch` step 3, `resolve_step1_native`,
/// `resolve_native_dispatch_wave1`'s `bytecode_available`, the warm virtual
/// populate's `resolve_cached_native_registration_with_cm`), so under
/// `--real-jdk` none of these rows can answer a call whose receiver class or
/// static owner declares or inherits the method. The ancestor walks of
/// `invoke_or_native` and step 1 used to let a declaring ancestor's native win
/// (`docs/feature-designs/native-dispatch-architecture.md`,
/// fixed the same round); the receivers these rows meet there under
/// `--real-jdk` on Linux are JDK-built objects, for which the JDK body is
/// HotSpot's answer (several of the natives already delegated such a
/// receiver to it). Each row,
/// with what it stood in for:
///
/// * `ScopedMemoryAccess.getChar` / `putChar`: the public wrappers. Their
///   `...Internal` twins are the reviewed `Intrinsic`s and stay; the wrapper's
///   Java `finally` is authoritative, exactly as for the `Byte` / `Short` /
///   `Int` / `Long` and `CharUnaligned` wrappers `RETIRED_SHADOW_SBR2_FFM` /
///   `RETIRED_SHADOW_SBR5_FFMLINK` retired.
/// * `BootLoader.findResources`: the sibling of `findResource`, which
///   `RETIRED_SHADOW_SBR2_LOADERS` retired once the built-in loaders became
///   the JDK's own objects; the JDK body answers over them.
/// * `Spliterator.getComparator` / `forEachRemaining`: interface DEFAULT
///   methods. A receiver minted on the interface runs the default bodies, which
///   call the abstract `tryAdvance` / `characteristics` rows (kept).
/// * `DirectoryStream.spliterator`: `Iterable`'s default body,
///   `Spliterators.spliteratorUnknownSize(iterator(), 0)`, which is what the
///   native's default arm calls anyway.
/// * `VarHandle.describeConstable`: `VarHandle`'s own body (`Optional.empty()`);
///   the JDK handle classes that answer a descriptor override it with Code.
///
/// Not a §1.4 retirement table: nothing is re-tagged, so `--compatible` keeps
/// every one of these registrations exactly as before (its kind map and the
/// registry census do not move); only the strict registry stops carrying a
/// `Bridge` no strict door can admit. `the native-registry census`'s
/// `strict_registry_drops_only_the_stubs` counts these back.
pub(in crate::retired_shadow) const JDK_ONLY_YIELDED_BRIDGE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/invoke/VarHandle",
        "describeConstable",
        "()Ljava/util/Optional;",
    ),
    (
        "java/nio/file/DirectoryStream",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/Spliterator",
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/Spliterator",
        "getComparator",
        "()Ljava/util/Comparator;",
    ),
    (
        "jdk/internal/loader/BootLoader",
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getChar",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putChar",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JC)V",
    ),
];

/// The `concrete_receiver::mirror_class_registrations` copies (native-io's
/// `register_async_socket_real` and `register_watch_service`, native-builtins'
/// `register_phase57_nio_file`) that land on a Unix JDK class whose method has
/// Code. They exist for receivers the VM MINTS on those classes; under
/// `--real-jdk` on Linux none is minted (`JdkAsyncChannels` is in force, the
/// Linux watch and file-system shadows are retired so the JDK builds
/// `LinuxWatchService` / `LinuxFileSystem` / the attribute views itself), and
/// every door above runs the Code. Unix only: a Windows `--real-jdk` VM still
/// mints its watch objects (`native-io` `watch_yield_if_real`) and, with
/// `CRATONVM_WINDOWS_CHANNEL_LAYER=0`, its async channel carriers, and the
/// ancestor walk can hand those to the `AbstractWatch*` /
/// `AsynchronousSocketChannelImpl` rows, so Windows keeps them.
#[cfg(unix)]
pub(in crate::retired_shadow) const JDK_ONLY_YIELDED_MIRROR_TRIPLES: &[(&str, &str, &str)] = &[
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "shutdownInput", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "shutdownOutput", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "shutdownInput", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "shutdownOutput", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/fs/AbstractWatchKey", "pollEvents", "()Ljava/util/List;"),
    ("sun/nio/fs/AbstractWatchKey", "reset", "()Z"),
    ("sun/nio/fs/AbstractWatchKey", "watchable", "()Ljava/nio/file/Watchable;"),
    ("sun/nio/fs/AbstractWatchKey$Event", "context", "()Ljava/lang/Object;"),
    ("sun/nio/fs/AbstractWatchKey$Event", "count", "()I"),
    ("sun/nio/fs/AbstractWatchKey$Event", "kind", "()Ljava/nio/file/WatchEvent$Kind;"),
    ("sun/nio/fs/AbstractWatchService", "close", "()V"),
    ("sun/nio/fs/AbstractWatchService", "poll", "()Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/AbstractWatchService", "poll", "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/AbstractWatchService", "take", "()Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/UnixFileAttributeViews$Basic", "name", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileAttributeViews$Basic", "readAttributes", "()Ljava/nio/file/attribute/BasicFileAttributes;"),
    (
        "sun/nio/fs/UnixFileAttributeViews$Basic",
        "setTimes",
        "(Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;)V",
    ),
    ("sun/nio/fs/UnixFileAttributeViews$Posix", "getOwner", "()Ljava/nio/file/attribute/UserPrincipal;"),
    ("sun/nio/fs/UnixFileAttributeViews$Posix", "name", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileAttributeViews$Posix", "readAttributes", "()Ljava/nio/file/attribute/PosixFileAttributes;"),
    ("sun/nio/fs/UnixFileAttributeViews$Posix", "setOwner", "(Ljava/nio/file/attribute/UserPrincipal;)V"),
    ("sun/nio/fs/UnixFileAttributeViews$Posix", "setPermissions", "(Ljava/util/Set;)V"),
    (
        "sun/nio/fs/UnixFileAttributeViews$Posix",
        "setTimes",
        "(Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;Ljava/nio/file/attribute/FileTime;)V",
    ),
    ("sun/nio/fs/UnixFileSystem", "close", "()V"),
    ("sun/nio/fs/UnixFileSystem", "getPath", "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;"),
    ("sun/nio/fs/UnixFileSystem", "getPathMatcher", "(Ljava/lang/String;)Ljava/nio/file/PathMatcher;"),
    ("sun/nio/fs/UnixFileSystem", "getRootDirectories", "()Ljava/lang/Iterable;"),
    ("sun/nio/fs/UnixFileSystem", "getSeparator", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileSystem", "isOpen", "()Z"),
    ("sun/nio/fs/UnixFileSystem", "isReadOnly", "()Z"),
    ("sun/nio/fs/UnixFileSystem", "provider", "()Ljava/nio/file/spi/FileSystemProvider;"),
];

#[cfg(not(unix))]
pub(in crate::retired_shadow) const JDK_ONLY_YIELDED_MIRROR_TRIPLES: &[(&str, &str, &str)] = &[];

/// Is `(class, method, descriptor)` one of [`JDK_ONLY_YIELDED_BRIDGE_TRIPLES`]
/// or [`JDK_ONLY_YIELDED_MIRROR_TRIPLES`] (this platform's)? A `Bridge`
/// registration of one is not made under `--real-jdk` over a real JDK. Cold:
/// registration only, and only asked for a `Bridge` under `--real-jdk`.
pub fn is_jdk_only_yielded_bridge(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    JDK_ONLY_YIELDED_BRIDGE_TRIPLES
        .iter()
        .chain(JDK_ONLY_YIELDED_MIRROR_TRIPLES.iter())
        .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// `--compatible` over a real JDK: the three instance resource lookups of
/// `java.lang.ClassLoader`, a reviewed `Intrinsic` in that mode only
/// (`NativeMethodRegistry::register` restates a `Bridge` registration of one
/// from any registrar). JIT round 20 wave 19, lane bridge: the `--compatible`
/// residual of
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// REVIEW (why each may stand over the JDK's Code there, AGENTS.md "only a
/// reviewed Intrinsic may win"):
///
/// * `--compatible` never builds the JDK's built-in loaders: the application
///   class path is the VM's own and `BuiltinClassLoader.nameToModule` / `ucp`
///   stay empty, so the JDK body's last step for a built-in loader
///   (`BuiltinClassLoader.findResource(s)`) answers null for every name. The
///   callbacks (`classloader::cl_get_resource_essential`,
///   `cl_get_resources_essential`, `cl_get_resource_as_stream_essential`)
///   answer a built-in loader from the VM's class path: the only working
///   answer there, as `BootLoader.findResourceAsStream` is (the walkcompat
///   `Intrinsic` of wave 4).
/// * For a USER-DEFINED loader (and a bare `URLClassLoader`) the callbacks run
///   the JDK's own `ClassLoader` body (`jdk_only_user_loader_receiver`, on with
///   the default `JdkUrlClassLoaders` row), so its parent delegation and its own
///   `findResource(s)` overrides are HotSpot's; a loader that overrides the
///   method itself is never reached (its class declares the method).
/// * As a `Bridge` the registrations ran COLD only (the ancestor walks before
///   wave 17 let an ancestor's `Bridge` beat Code) while every warm door
///   (`resolve_dispatch` step 3) ran the JDK body: an app class's
///   `getResourceAsStream` answered on its first call and null once its call
///   site was warm. Host w17q showed the cold half failing when the walks
///   yielded (`R20Rev13FinalOverrideLoader`, `R20RevNatModuleResourceScope`,
///   ...). As an `Intrinsic`, `resolve_dispatch` step 2 admits it at every door.
///
/// `--real-jdk` keeps the ambient `Bridge`: the built-in loaders are the JDK's
/// own objects there and their Code answers (resolve_dispatch step 3).
pub(in crate::retired_shadow) const COMPAT_LOADER_RESOURCE_INTRINSIC_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/ClassLoader",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ),
    (
        "java/lang/ClassLoader",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
    ),
    (
        "java/lang/ClassLoader",
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
    ),
];

/// Is `(class, method, descriptor)` one of
/// [`COMPAT_LOADER_RESOURCE_INTRINSIC_TRIPLES`]? Cold: registration only.
pub fn is_compat_loader_resource_intrinsic(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/ClassLoader"
        && COMPAT_LOADER_RESOURCE_INTRINSIC_TRIPLES
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// Is `(class, method, descriptor)` one of the high-frequency startup bridges
/// that `--compatible` promotes to a reviewed `Intrinsic` over real JDK bytecode
/// to achieve sub-second fast boot?
///
/// Under `--compatible`, one group remains:
/// 2. `Parameter.getParameterAnnotations` / `Method` / `Constructor` / `Executable`
///    (j27 wave 9: `J27CompatFastBootGroupsRows`' annotation rows equal HotSpot 25
///    in both modes -- synthetic and mandated parameters, inner, enum, record,
///    anonymous constructors, bridges, lambdas, CLASS-retention, repeatables).
///
/// Group 4 (`ResourceBundle`, `LocaleResources`, `Bundles`) was withdrawn in
/// j27 wave 9 (lane W9RPL, same page as group 1): promoted, the natives
/// answered `J27CompatFastBootGroupsRows` off HotSpot 25 in `--compatible` --
/// the root bundle for every locale with `getLocale()` / `getBaseBundleName()`
/// null, no cache identity, `Control`s ignored, missing-key messages without
/// the bundle, no `ClassCastException` / `NullPointerException` where the JDK
/// throws -- and `NumberFormat.getCurrencyInstance` threw `ClassCastException`
/// in `LocaleData.getCurrencyNames`. The JDK's bodies run again, as in
/// `--real-jdk`.
///
/// Group 1 (`javax/crypto/Cipher`) was withdrawn in j27 wave 9 (lane W9RPL;
/// `j27-compat-cipher-fast-boot-intrinsic-un-retires-the-native-cipher-FIXED-20261029.md`):
/// promoted to `Intrinsic`, the retired native `Cipher` (SBR wave 6,
/// `RETIRED_SHADOW_SBR_DISPATCH_TRIPLES`) beat the JDK's bytecode again in
/// default `--compatible`, and `SbrW1Jca` answered 13 rows off HotSpot 25
/// (PCBC / CTS / DESede-CTR / Blowfish refusals, GCM 16-byte IV, `AESWrap`,
/// ChaCha20 limit and in-place rows). As a `Bridge` it loses to the JDK's body
/// in `resolve_dispatch`, as in `--real-jdk`.
///
/// Group 3 (`Method.invoke(Object, Object[])` / `Constructor.newInstance(Object[])`)
/// was withdrawn: as an `Intrinsic` the native skipped the JDK accessor's
/// `asType` adapter form (`LambdaForm$MH.invoke`) and the direct handle's
/// `checkCustomized` count, so `SHOW_HIDDEN_FRAMES` walks and the customized
/// `LambdaForm$DMH` differed from HotSpot in `--compatible` only
/// (`R22FlipHiddenFrames`, `R22RuntimeCtorCustomize`;
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// The JDK's own bodies run there again, as in `--real-jdk`.
///
/// `--real-jdk` leaves them as ambient `Bridge`s (or dropped by retired shadow table),
/// so `--real-jdk` remains 100% authoritative and byte-for-byte HotSpot compliant.
pub fn is_compat_fast_boot_intrinsic(
    class_name: &str,
    method_name: &str,
    _descriptor: &str,
) -> bool {
    // 1. (withdrawn, j27 wave 9) `javax/crypto/Cipher`: see above.

    // 2. java/lang/reflect/Parameter.getParameterAnnotations
    if method_name == "getParameterAnnotations"
        && matches!(
            class_name,
            "java/lang/reflect/Parameter"
                | "java/lang/reflect/Method"
                | "java/lang/reflect/Constructor"
                | "java/lang/reflect/Executable"
        )
    {
        return true;
    }

    // 3. (withdrawn) `Method.invoke` / `Constructor.newInstance`: see above.

    // 4. (withdrawn, j27 wave 9) `ResourceBundle` / `LocaleResources` /
    // `Bundles`: see above.

    false
}

/// Windows, channel layer on: the rows that MINT or release the VM's
/// asynchronous file channel carrier (`native-io` `alloc_afc_channel`):
/// `AsynchronousFileChannel.open` / `lock()`, `newAsynchronousFileChannel` on
/// the abstract and the Windows provider (the mirror), `FileLockImpl.release`.
/// The family takes the platform impl's own rows (`COMPAT_FAMILIES`), and a
/// carrier left minting beside them hands the JDK's
/// `WindowsAsynchronousFileChannelImpl` a receiver whose constructor never ran
/// (`closeLock` null, `NonWritableChannelException` on a READ+WRITE channel,
/// `WinAsyncStress files`, once a call site had cached the native door). With
/// the layer on there is no carrier to mint: the JDK's `open` reaches
/// `WindowsChannelFactory` and `Iocp`. Linux keeps the carrier with the provider
/// family.
pub(in crate::retired_shadow) fn is_windows_afc_producer_shadow(class_name: &str, method_name: &str) -> bool {
    cfg!(windows)
        && windows_channel_layer()
        && matches!(
            (class_name, method_name),
            ("java/nio/channels/AsynchronousFileChannel", "open" | "lock")
                | (
                    "java/nio/file/spi/FileSystemProvider" | "sun/nio/fs/WindowsFileSystemProvider",
                    "newAsynchronousFileChannel"
                )
                | ("sun/nio/ch/FileLockImpl", "release")
        )
}

/// Is `(class, method, descriptor)` one of a [`CompatFamily`]'s reviewed
/// non-retired stand-ins over JDK `Code` (`CompatFamily::bridges`)? Asked by
/// `NativeMethodRegistry::register` under `JdkOnly` over a real JDK when
/// `CRATONVM_STRICT_FAMILY_BRIDGES` is on (interpreter round i7 wave 138,
/// lane L2, `i137-orch-jdk-only-soft-and-phantom-refersto-cost-4-us`): such a
/// `Bridge` never answers under `JdkOnly` (concrete bytecode beats it,
/// `resolve_dispatch` step 3), but its registration alone makes every compile
/// gate's `registered_native_will_run` answer "runs" for its triple, so the
/// JDK body stays interpreted. Cold: registration only. The family must
/// exist on this platform, as for `compat_row_of_shadow`.
pub(crate) fn is_compat_family_bridge(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    is_compat_family_bridge_with(
        class_name,
        method_name,
        descriptor,
        compat_family_of_class(class_name),
    )
}

/// [`is_compat_family_bridge`] with `family` =
/// [`compat_family_of_class`]`(class_name)` supplied by the caller (the
/// registry's per-class memo; see [`compat_row_of_shadow_with`]).
pub(crate) fn is_compat_family_bridge_with(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    family: Option<&'static CompatFamily>,
) -> bool {
    family.is_some_and(|family| {
        family
            .bridges
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
    })
}

/// The family whose `classes` list names `class_name`, if any. Cold:
/// registration only.
pub(crate) fn compat_family_of_class(class_name: &str) -> Option<&'static CompatFamily> {
    static INDEX: std::sync::OnceLock<PrefixIndex> = std::sync::OnceLock::new();
    let index = INDEX.get_or_init(|| PrefixIndex::of_families(|f| f.classes));
    first_family_naming(index, class_name, |f| f.classes)
}

/// A set of `&'static str` prefixes, each tagged with the index of the
/// [`COMPAT_FAMILIES`] entry (or `0` for an untagged list) it came from,
/// answering "which prefixes does this name start with" with a few binary
/// searches over the sorted prefixes instead of one `starts_with` per prefix.
///
/// Phase 5 fast boot (`p5-natreg`): `compat_family_of_class` walked every
/// family's `classes` list (hundreds of prefixes) for each registration and
/// measured ~25,000 cycles per call, ~40% of `NativeMethodRegistry::register`
/// under `--compatible`. The index is a pure function of the constant
/// prefix lists, so it is built once per process like
/// [`retired_shadow_row_set`]; platform filtering
/// ([`family_exists_on_this_platform`]) and the families' `except` lists are
/// still applied per query, in the original order.
///
/// # The walk (exact)
///
/// Every prefix of `name` in the set is `<= name`, and any two of them are
/// prefixes of each other. So with `hi` a prefix of `name` holding every
/// unreported match as a prefix of itself (initially `hi = name`), take `p`,
/// the largest entry `<= hi`:
///
/// * `p` is a prefix of `hi`: report it (and its equal entries); every other
///   match is `<= p`, hence a proper prefix of `p`, so continue with
///   `hi = p` minus its last byte.
/// * it is not: with `l` the length of their common prefix, `p[l] < hi[l]`
///   (`p <= hi`, and `p` does not end at `l`), so a match longer than `l`
///   would agree with `hi` at `l` and exceed `p` -- impossible. Continue with
///   `hi = hi[..l]`.
///
/// `hi` strictly shrinks every step.
/// `the_prefix_index_answers_what_the_linear_walks_answered` checks the
/// result against the linear walks over every class name the tables know.
pub(in crate::retired_shadow) struct PrefixIndex {
    /// `(prefix, tag)`, sorted by prefix bytes (equal prefixes adjacent).
    pub(in crate::retired_shadow) sorted: Vec<(&'static [u8], u16)>,
}

impl PrefixIndex {
    pub(in crate::retired_shadow) fn new(entries: impl IntoIterator<Item = (&'static str, u16)>) -> Self {
        let mut sorted: Vec<(&'static [u8], u16)> = entries
            .into_iter()
            .map(|(p, t)| (p.as_bytes(), t))
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        PrefixIndex { sorted }
    }

    pub(in crate::retired_shadow) fn of_families(list: impl Fn(&CompatFamily) -> &'static [&'static str]) -> Self {
        assert!(
            COMPAT_FAMILIES.len() <= 64,
            "PrefixIndex callers fold family indices into a u64 mask"
        );
        Self::new(
            COMPAT_FAMILIES
                .iter()
                .enumerate()
                .flat_map(|(i, f)| list(f).iter().map(move |p| (*p, i as u16))),
        )
    }

    /// Calls `hit` with the tag of every prefix `name` starts with.
    #[inline]
    pub(in crate::retired_shadow) fn for_each_match(&self, name: &str, mut hit: impl FnMut(u16)) {
        let mut hi: &[u8] = name.as_bytes();
        loop {
            let i = self.sorted.partition_point(|&(p, _)| p <= hi);
            if i == 0 {
                return;
            }
            let p = self.sorted[i - 1].0;
            if hi.starts_with(p) {
                let mut j = i;
                while j > 0 && self.sorted[j - 1].0 == p {
                    hit(self.sorted[j - 1].1);
                    j -= 1;
                }
                if p.is_empty() {
                    return;
                }
                hi = &p[..p.len() - 1];
            } else {
                let l = p.iter().zip(hi).take_while(|(a, b)| a == b).count();
                hi = &hi[..l];
            }
        }
    }
}

/// The first [`COMPAT_FAMILIES`] entry, in order, that exists on this
/// platform and whose `list` names `class_name` (a prefix of `list` matches
/// and none of the family's `except` prefixes does) -- the
/// `filter(family_exists_on_this_platform).find(names_class)` walk, with the
/// prefix matching done by `index`.
pub(in crate::retired_shadow) fn first_family_naming(
    index: &PrefixIndex,
    class_name: &str,
    list: impl Fn(&CompatFamily) -> &'static [&'static str],
) -> Option<&'static CompatFamily> {
    let mut mask = 0u64;
    index.for_each_match(class_name, |i| mask |= 1u64 << i);
    first_family_in_mask(mask, class_name, list)
}

/// The tail of [`first_family_naming`]: `mask` has bit `i` set for every
/// family `i` one of whose `list` prefixes `class_name` starts with.
pub(in crate::retired_shadow) fn first_family_in_mask(
    mut mask: u64,
    class_name: &str,
    list: impl Fn(&CompatFamily) -> &'static [&'static str],
) -> Option<&'static CompatFamily> {
    while mask != 0 {
        let i = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        let f = &COMPAT_FAMILIES[i];
        if family_exists_on_this_platform(f.row) && f.names_class(list(f), class_name) {
            return Some(f);
        }
    }
    None
}

/// [`compat_family_of_class`], [`compat_row_of_stub_class`] and
/// [`class_is_under_retired_prefix`] of one class name, from ONE walk over a
/// combined [`PrefixIndex`] (`NativeMethodRegistry::register`'s per-class
/// `ClassFacts`, `p5-natreg`). Same answers as the three calls;
/// `the_prefix_index_answers_what_the_linear_walks_answered` compares them.
pub(crate) struct ClassShadowFacts {
    pub(crate) family: Option<&'static CompatFamily>,
    pub(crate) stub_row: Option<cratonvm_types::HotSpotAlignment>,
    pub(crate) retired_prefix: bool,
}

pub(crate) fn class_shadow_facts(class_name: &str) -> ClassShadowFacts {
    // Tags: `0x000 | i` a `classes` prefix of family `i`, `0x100 | i` a
    // `stub_classes` prefix of family `i`, `0x200` a retired prefix.
    const STUB: u16 = 0x100;
    const RETIRED: u16 = 0x200;
    static INDEX: std::sync::OnceLock<PrefixIndex> = std::sync::OnceLock::new();
    let index = INDEX.get_or_init(|| {
        assert!(
            COMPAT_FAMILIES.len() <= 64,
            "family indices fold into a u64 mask"
        );
        PrefixIndex::new(
            COMPAT_FAMILIES
                .iter()
                .enumerate()
                .flat_map(|(i, f)| {
                    let i = i as u16;
                    f.classes
                        .iter()
                        .map(move |p| (*p, i))
                        .chain(f.stub_classes.iter().map(move |p| (*p, STUB | i)))
                })
                .chain(RETIRED_SHADOW_PREFIXES.iter().map(|p| (*p, RETIRED))),
        )
    });
    let (mut classes, mut stubs, mut retired_prefix) = (0u64, 0u64, false);
    index.for_each_match(class_name, |t| {
        if t == RETIRED {
            retired_prefix = true;
        } else if t & STUB != 0 {
            stubs |= 1u64 << (t & 0xff);
        } else {
            classes |= 1u64 << t;
        }
    });
    ClassShadowFacts {
        family: first_family_in_mask(classes, class_name, |f| f.classes),
        stub_row: first_family_in_mask(stubs, class_name, |f| f.stub_classes).map(|f| f.row),
        retired_prefix,
    }
}

/// Does the family exist on this build's platform? `JdkNioChannels` adopts
/// `RETIRED_SHADOW_SBR_NIOREAL_TRIPLES`, which is EMPTY on Windows (no
/// `WEPoll` layer: the VM's channel carriers stay there in both modes); but a
/// few rows on its classes are retired by other, platform-blind tables (the
/// javac covariant `NetworkChannel` bridges, round 15), and adopting those
/// alone over live carriers would be the half-family the NIOREAL record warns
/// against. So on Windows the family is not a family at all (nio20, lane L4).
pub(in crate::retired_shadow) fn family_exists_on_this_platform(row: cratonvm_types::HotSpotAlignment) -> bool {
    // `JdkAsyncChannels` (nio20 wave 3, L1): the same reasoning over
    // `RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES`, which is `#[cfg(unix)]`; on Windows the family exists with the
    // layer, over `RETIRED_SHADOW_SBR_ASYNCNET_WINDOWS_TRIPLES` and the `Iocp` natives of `net_iocp_win.rs`.
    // `JdkHttpClient` (nio20 wave 4, L4): the JDK client needs the real
    // selector and channels of `JdkNioChannels`; they exist on Windows with the Windows channel
    // layer (`native-io/src/net_wepoll.rs`, `net_nio_win.rs`), so the client family exists there
    // exactly when the layer is on.
    // `JdkHttpServer` (nio20 wave 5, L1): the same, for the JDK's `ServerImpl`.
    //
    // `JdkNioChannels` itself exists on Windows only where the Windows channel layer
    // is switched on (`windows_channel_layer`): its Windows table is the Linux one
    // over `WEPollSelectorImpl` / `WEPollSelectorProvider`. `--real-jdk` aligns every
    // row, so without that switch the carriers would be retired here the moment the
    // table is non-empty.
    !(cfg!(windows)
        && matches!(
            row,
            cratonvm_types::HotSpotAlignment::JdkAsyncChannels
                | cratonvm_types::HotSpotAlignment::JdkNioChannels
                | cratonvm_types::HotSpotAlignment::JdkHttpClient
                | cratonvm_types::HotSpotAlignment::JdkHttpServer
        )
        && !windows_channel_layer())
}

/// `CRATONVM_WINDOWS_CHANNEL_LAYER` (default ON, `0` kills it): run the JDK's own `sun.nio.ch` channels,
/// selectors and datagram channels on Windows over the Winsock natives of `native-io/src/net_wepoll.rs` and
/// `net_nio_win.rs` instead of the VM's carriers (the page `windows-has-no-real-channel-layer-FIXED-20261013.md`
/// holds the measurements). Read once: it decides which table the registry adopts.
pub(in crate::retired_shadow) fn windows_channel_layer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// The row of the family whose `stub_classes` list names `class_name`: a
/// registration of that class with kind `SyntheticStub` is dropped when the
/// row is in force (see [`CompatFamily`]). Cold: registration only.
pub(crate) fn compat_row_of_stub_class(
    class_name: &str,
) -> Option<cratonvm_types::HotSpotAlignment> {
    static INDEX: std::sync::OnceLock<PrefixIndex> = std::sync::OnceLock::new();
    let index = INDEX.get_or_init(|| PrefixIndex::of_families(|f| f.stub_classes));
    first_family_naming(index, class_name, |f| f.stub_classes).map(|f| f.row)
}

/// Is `(class, method, descriptor)` one of the archive family's retired rows
/// -- `RETIRED_SHADOW_JARFILE_TRIPLES` (every `JarFile` row and
/// `ZipFile$1.getManifestName`), and the `ZipFile`, `ZipEntry.setComment`,
/// `JarEntry`, `Manifest` and `Attributes` rows of `RETIRED_SHADOW_L1_TRIPLES`
/// / `RETIRED_SHADOW_L1_JT_TRIPLES`, which `--real-jdk` runs as the JDK's
/// bodies together? The registry drops every registration of them under
/// `Compatible` over a real JDK with the `JdkJarFileBodies` HotSpot-alignment
/// row, whatever the registrar or kind (so `ZipFile.close`'s `Intrinsic`
/// too): a real `JarFile` built by its real constructor hands its real
/// `JarFileEntry` / `Manifest` / `Attributes` to whatever natives are left,
/// so the value classes go with their producer (interpreter round i2 wave 58,
/// lane L5; `docs/feature-designs/native-dispatch-architecture.md`).
/// Only those classes reach the table search.
pub(crate) fn is_jar_file_family_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/util/jar/JarFile"
            | "java/util/zip/ZipFile"
            | "java/util/zip/ZipFile$1"
            | "java/util/zip/ZipEntry"
            | "java/util/jar/JarEntry"
            | "java/util/jar/Manifest"
            | "java/util/jar/Attributes"
            | "java/util/jar/Attributes$Name"
    ) && (triple_is_retired_shadow(class_name, method_name, descriptor)
        || COMPAT_JAR_FILE_FAMILY_ACCESS_ROWS
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor))
}

/// The `JavaUtilZipFileAccess` Bridges on the JDK's own `ZipFile$1`
/// (`shared_secrets_bridge.rs` `register_java_util_zip_file_access`) that go
/// with the archive family under `--compatible`'s `JdkJarFileBodies` row,
/// though no `RETIRED_SHADOW_*` table lists them: `--real-jdk` runs their JDK
/// bodies (a `Bridge` over concrete JDK bytecode loses there), and over a
/// real `JarFile` the `getEntry` Bridge is wrong, not merely redundant. The
/// JDK's `JarFile.getEntry` -> `getEntry0` -> `JUZFA.getEntry(this, name,
/// JarFileEntry::new)` asks it for a `JarFileEntry`, and the Bridge called the
/// public `getEntry(String)` back -- `JarFile.getEntry` again, a
/// `StackOverflowError` (measured, wave 58, lane L5, probe
/// `L5W58CompatJarFileBodies` with the row on); its `entryLocalNameEncoding`
/// answered `true` for every entry. Interpreter round i2 wave 58, lane L5.
pub(in crate::retired_shadow) static COMPAT_JAR_FILE_FAMILY_ACCESS_ROWS: &[(&str, &str, &str)] = &[
    (
        "java/util/zip/ZipFile$1",
        "getEntry",
        "(Ljava/util/zip/ZipFile;Ljava/lang/String;Ljava/util/function/Function;)Ljava/util/zip/ZipEntry;",
    ),
    (
        "java/util/zip/ZipFile$1",
        "entryLocalNameEncoding",
        "(Ljava/util/zip/ZipEntry;)Z",
    ),
];

/// Is `(class, method)` a `Collections.unmodifiable*` factory, or a method
/// of the immutable collections' serialization carrier
/// (`ImmutableCollections$*.writeReplace`, `CollSer.readResolve`,
/// `Collections$UnmodifiableRandomAccessList.writeReplace`)? The registry
/// drops every registration of them under `Compatible` over a real JDK with
/// the `JdkUnmodifiableViews` HotSpot-alignment row, whatever the registrar
/// or kind, so the JDK's bodies run (interpreter round i1 wave 51, lane L5).
/// By name and method only: every overload of a factory goes (the JDK has
/// one per factory). Two prefix tests per registration, cold (registration).
///
/// `Collections.nCopies` too (interpreter round i2 wave 55, lane L5): its
/// three stand-ins (`native-collections`, `phases_early`, `phases_late`)
/// answered an `ArrayList` wrapped in the JDK's
/// `Collections$UnmodifiableRandomAccessList` where HotSpot's body answers a
/// `Collections$CopiesList` (probe
/// `tools/probes/interp/L5/L5W55CompatCollectionClasses.java`, `nCopies`);
/// `--real-jdk` already runs that body (the lane-T rows above).
pub(crate) fn is_compat_unmodifiable_view_shadow(class_name: &str, method_name: &str) -> bool {
    match class_name {
        "java/util/Collections" => {
            method_name.starts_with("unmodifiable") || method_name == "nCopies"
        }
        "java/util/ImmutableCollections$List12"
        | "java/util/ImmutableCollections$ListN"
        | "java/util/ImmutableCollections$Set12"
        | "java/util/ImmutableCollections$SetN"
        | "java/util/ImmutableCollections$Map1"
        | "java/util/ImmutableCollections$MapN"
        | "java/util/Collections$UnmodifiableRandomAccessList" => method_name == "writeReplace",
        "java/util/CollSer" => method_name == "readResolve",
        _ => false,
    }
}

/// Lane 5 — `java/util/concurrent/`, `Thread`, `Unsafe`, retired 2026-09-10.
///
/// `docs/feature-designs/native-dispatch-architecture.md`
/// is the page; this is the table it fills. The lane's population is
/// **405 bucket-A/B rows over 23 classes**, which is not the 516 a prefix
/// filter over `--dump-native-registry --explain-jdk-only` reports, and not
/// the 659 a binary that predates the Phase 3 wave reports. One subtraction
/// gets from 516 to 405, and it is a lane-0 rule rather than this lane's
/// choice:
///
///   * 111 rows come from a registrar whose classes span more than one lane
///     (`register_throwable_subclass_natives` and the `native-collections`
///     collection-family loops). Lane 0 §3: **the unit of work for a
///     cross-cutting registrar is the registrar, and lane T owns it whole** —
///     so `CopyOnWriteArraySet.equals`, registered by the same
///     `native-collections/src/lib.rs` line as `HashSet.equals` and
///     `LinkedHashSet.equals`, is not this lane's row to retire even though the
///     receiver is;
///
/// (The `ConcurrentHashMap` family's 99 rows are the difference between 659 and
/// 516: they were retired by the 2026-09-09 Phase 3 wave and are already in
/// [`RETIRED_SHADOW_PHASE3_TRIPLES`], so a dump from a binary that carries that
/// wave does not report them at all. **Take the dump from the binary you are
/// about to change.**)
///
/// # 98 of 405, and the other 307 are classified rather than deferred
///
/// | disposition | rows |
/// |---|---|
/// | retired here | **98** |
/// | held: the class's whole arm moves the VM AWAY from HotSpot | 127 |
/// | held: one unit with a held class | 133 |
/// | held: no instrument in this tree dispatches the row | 40 |
/// | dead registration — a door that never opens | 4 |
/// | held: a real-JDK keep arm this table would disarm | 2 |
/// | held: a partial with evidence against it | 1 |
///
/// # A retirement is mode-blind and a keep arm is not
///
/// This wave was 100 rows for most of a day. The two that came back out are
/// worth the paragraph, because the mechanism that removed them is general and
/// nothing in the four preconditions asks about it.
///
/// [`crate::registry::NativeMethodRegistry::register`] re-tags a retired triple
/// `Bridge` -> `SyntheticStub` **before** calling `register_inner`, and it does
/// so in every mode — the table is per-triple, not per-mode. Inside
/// `register_inner`, real-JDK mode (`drop_real_layout_synthetic`) keeps a small
/// number of natives it cannot safely execute as bytecode, and each of those
/// keeps is written as a predicate over `effective_category()`:
///
/// ```text
///   keep_real_scheduled_executor_bridge = effective_category() == Bridge && ...
///   keep_real_forkjoinpool_bridge       = effective_category() == Bridge && ...
///   keep_real_forkjointask_bridge       = effective_category() == Bridge && ...
/// ```
///
/// By the time those run, the re-tag has already made the answer
/// `SyntheticStub`. **A triple in this table can therefore lose its native in
/// REAL-JDK mode**, which is not what a §1.4 shadow retirement is for and is
/// not a mode this lane measured. `ScheduledThreadPoolExecutor.<init>(I,
/// ThreadFactory, RejectedExecutionHandler)` and `getCorePoolSize()I` are
/// exactly the two triples `keep_real_scheduled_executor_bridge` names — kept
/// for Spring's `ThreadPoolTaskScheduler` anonymous subclass — so they are out.
///
/// The instrument that caught it was
/// `registry::tests::real_layout_mode_drops_enumset_native_surface`, whose one
/// `Bridge`-survives control happened to BE the 3-arg constructor. That was
/// luck; `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows`
/// now covers all three keep arms on purpose, by registering each protected
/// triple through the real code path rather than restating the predicate.
///
/// The other nine classes in this table are named nowhere in `registry.rs`
/// except a hash-test fixture (`jdk/internal/misc/Unsafe`) and one comment
/// (`java/util/concurrent/ThreadPoolExecutor`), so the sweep that found these
/// two found no others.
///
/// # The `Unsafe` subset that IS retired, and the line it is drawn on
///
/// Sixteen `jdk/internal/misc/Unsafe` rows are here and seventy-five are not,
/// and the line is not a judgement call: **an atomic or a fence that delegates
/// to an `ACC_NATIVE` primitive at the SAME offset** is retirable, because the
/// JDK's Java body is then a loop over calls this VM already serves correctly
/// — `getAndAddInt` is `do { v = getIntVolatile(o, offset); } while
/// (!weakCompareAndSetInt(o, offset, v, v + delta));` and every term in it is
/// one of ours. Anything that does ARITHMETIC on the offset is not, because
/// this VM's offsets are slot indices.
///
/// That is why `getAndSetReference` retires and `getAndSetByte` cannot, though
/// they are neighbours in the same file with the same shape.
///
/// The per-class record, with the measurement behind each blocker, is on the
/// lane page. Three of the blockers are properties of this VM rather than of
/// any one method, and are restated here because a future wave will re-derive
/// them otherwise:
///
///   * **`objectFieldOffset` returns a SLOT INDEX, not a byte offset.** The
///     JDK implements the whole sub-word atomic family in Java over a 4-byte
///     CAS — `long wordOffset = offset & ~3; int shift = (int)(offset & 3) << 3`
///     — so retiring `compareAndSetByte` and its relatives hands that
///     arithmetic a number it does not describe: `offset & ~3` names a
///     DIFFERENT FIELD. `unsafe_natives_ext.rs` says the same from the other
///     side and carries the HotSpot comparison that established it, and
///     `L5SubwordAtomics.java` scores the family at all four byte
///     positions of a word, on a field and on an array element, for
///     `byte`/`boolean`/`short`/`char`: **132 rows, 0 diffs, unarmed.** The
///     natives are right; it is the retirement that would be wrong.
///   * the `get*Unaligned` / `put*Unaligned` rows on `Unsafe` are the same
///     defect in a different family: they decompose a byte range, and a slot
///     index has no bytes. (The identically-named `ScopedMemoryAccess` rows
///     ARE retired — they take a `MemorySegment` base and a real byte offset,
///     which is a different number.)
///   * **`sun/misc/Unsafe`'s 82 rows are dispatched by nothing here.** All 121
///     probes report the dial VACUOUS on that scope, and the 132 `--real-jdk`
///     corpus reports reach 18 of the 82. Precondition 1 fails by measurement,
///     not by omission.
///
/// # The original `*Internal` twin measurement
///
/// 2026-10-04: the fourteen scalar Internal rows leave these retirement tables
/// after their checked operation becomes a reviewed intrinsic. The public
/// wrappers retain their original verdict. 2026-10-07 (round 19,
/// `r16-ffm-plain-downcall-costs-microseconds`): `copyMemoryInternal` follows
/// them, for the shared-close window its Java body never held; the
/// `copyMemory` wrapper stays retired.
/// Session/thread/close, byte order, raw-memory capability and moving-GC
/// oracles are recorded in
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// # The `*Internal` twin rule, which originally retired 16 rows
///
/// Eight `ScopedMemoryAccess` rows were dispatched (`L4ByteBufferSweep` and
/// `L4TypedBufferSweep`, 518 dial yields between them, delta 0). Each is a
/// public wrapper whose only body calls its own `@ForceInline` `…Internal`
/// twin, and the twin is registered too — so retiring the wrapper alone would
/// produce a configuration NOBODY measured: real outer, native inner. The
/// class-wide arm that measured clean yielded both. So each retired wrapper
/// originally brought its twin. The fourteen rows with no dispatch on either
/// half stayed out in that measurement; the reviewed scalar Internal
/// intrinsics added in October supersede that historical twin rule.
///
/// # `AbstractExecutorService`'s four rows are a door that never opens
///
/// `submit` ×3 and `invokeAny` are registered on `AbstractExecutorService`,
/// which is abstract. A dispatch door asks the registry about the DECLARING
/// class of the resolved method, and every concrete executor in the image —
/// `ThreadPoolExecutor`, `ForkJoinPool` — carries its own registration of the
/// same names, so the abstract one is never the answer.
/// `L5ExecutorSweep.java` builds the one receiver shape that could
/// reach it (a direct subclass declaring only `execute`) and the rows still
/// read `invocations: 0`. Lane 0 §1: deleting these is worth doing and is
/// **not** a retirement, so they are not in this table.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L5_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Thread$FieldHolder", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V"),
    ("java/lang/Thread$State", "valueOf", "(Ljava/lang/String;)Ljava/lang/Thread$State;"),
    ("java/lang/Thread$State", "values", "()[Ljava/lang/Thread$State;"),
    ("java/util/concurrent/CompletableFuture", "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "complete", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CompletableFuture", "completeExceptionally", "(Ljava/lang/Throwable;)Z"),
    ("java/util/concurrent/CompletableFuture", "completeValue", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CompletableFuture", "isCompletedExceptionally", "()Z"),
    ("java/util/concurrent/CompletableFuture", "thenAcceptAsync", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenApplyAsync", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenCombine", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenComposeAsync", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CompletableFuture", "thenRunAsync", "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/concurrent/CopyOnWriteArrayList", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "addIfAbsent", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "bulkRemove", "(Ljava/util/function/Predicate;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "clear", "()V"),
    ("java/util/concurrent/CopyOnWriteArrayList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/concurrent/CopyOnWriteArrayList", "isEmpty", "()Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/CopyOnWriteArrayList", "set", "(ILjava/lang/Object;)Ljava/lang/Object;"),
    ("java/util/concurrent/CopyOnWriteArrayList", "size", "()I"),
    ("java/util/concurrent/CopyOnWriteArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "<init>", "()V"),
    ("java/util/concurrent/PriorityBlockingQueue", "isEmpty", "()Z"),
    ("java/util/concurrent/PriorityBlockingQueue", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/concurrent/PriorityBlockingQueue", "peek", "()Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "poll", "()Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "poll", "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"),
    ("java/util/concurrent/PriorityBlockingQueue", "put", "(Ljava/lang/Object;)V"),
    ("java/util/concurrent/PriorityBlockingQueue", "size", "()I"),
    ("java/util/concurrent/PriorityBlockingQueue", "take", "()Ljava/lang/Object;"),
    // `ScheduledThreadPoolExecutor.<init>(I,ThreadFactory,RejectedExecutionHandler)`
    // and `getCorePoolSize()I` were REMOVED on 2026-09-10, before this table
    // ever landed, because `registry.rs`'s `keep_real_scheduled_executor_bridge`
    // kept them in REAL-JDK mode for Spring's `ThreadPoolTaskScheduler`
    // anonymous subclass, and a retirement table cannot coexist with a keep
    // arm on the same two triples (see this module's header, "a retirement is
    // mode-blind and a keep arm is not"). RETIRED 2026-09-17
    // (lane-concurrency-forkjoin-scheduler-20260917.md item 2): the keep arm
    // is deleted in the SAME change as these two rows landing, per that
    // page's own §4 — the two do not coexist. The real-JDK Spring
    // `scheduling.concurrent.*` cluster this arm was written to protect
    // (`ThreadPoolTaskSchedulerTests` and friends, 200 test methods) is
    // byte-identical with the arm gone; see the FIXED write-up.
    ("java/util/concurrent/ScheduledThreadPoolExecutor", "<init>", "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V"),
    ("java/util/concurrent/ScheduledThreadPoolExecutor", "getCorePoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "getActiveCount", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getCompletedTaskCount", "()J"),
    ("java/util/concurrent/ThreadPoolExecutor", "getCorePoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getMaximumPoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getPoolSize", "()I"),
    ("java/util/concurrent/ThreadPoolExecutor", "getTaskCount", "()J"),
    ("java/util/concurrent/ThreadPoolExecutor", "invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;"),
    ("java/util/concurrent/ThreadPoolExecutor", "isShutdown", "()Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "isTerminated", "()Z"),
    ("java/util/concurrent/ThreadPoolExecutor", "shutdown", "()V"),
    ("java/util/concurrent/ThreadPoolExecutor", "shutdownNow", "()Ljava/util/List;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/ThreadPoolExecutor", "submit", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;"),
    ("java/util/concurrent/TimeUnit", "convert", "(JLjava/util/concurrent/TimeUnit;)J"),
    ("java/util/concurrent/TimeUnit", "sleep", "(J)V"),
    ("java/util/concurrent/TimeUnit", "toDays", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toHours", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMicros", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMillis", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toMinutes", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toNanos", "(J)J"),
    ("java/util/concurrent/TimeUnit", "toSeconds", "(J)J"),
    ("jdk/internal/misc/ScopedMemoryAccess", "copyMemory", "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V"),
    // `copyMemoryInternal` left this table in round nio20 wave 3 (lane L5): it
    // is a reviewed, windowed `Intrinsic` (`native-builtins/src/lib.rs`, the
    // bulk `ScopedMemoryAccess` rows), like the scalar Internal rows before it,
    // so a shared close's handshake waits for it. Its public wrapper stays.
    ("jdk/internal/misc/ScopedMemoryAccess", "getIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J"),
    ("jdk/internal/misc/ScopedMemoryAccess", "getShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putInt", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JI)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V"),
    ("jdk/internal/misc/ScopedMemoryAccess", "putShortUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V"),
    ("jdk/internal/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
    ("jdk/internal/misc/Unsafe", "getAndAddLong", "(Ljava/lang/Object;JJ)J"),
    ("jdk/internal/misc/Unsafe", "getAndSetInt", "(Ljava/lang/Object;JI)I"),
    ("jdk/internal/misc/Unsafe", "getAndSetLong", "(Ljava/lang/Object;JJ)J"),
    ("jdk/internal/misc/Unsafe", "getAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "getReferenceAcquire", "(Ljava/lang/Object;J)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "loadFence", "()V"),
    ("jdk/internal/misc/Unsafe", "putIntOpaque", "(Ljava/lang/Object;JI)V"),
    ("jdk/internal/misc/Unsafe", "putReferenceOpaque", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "putReferenceRelease", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "storeFence", "()V"),
    ("jdk/internal/misc/Unsafe", "storeStoreFence", "()V"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetInt", "(Ljava/lang/Object;JII)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetIntPlain", "(Ljava/lang/Object;JII)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
    ("jdk/internal/misc/Unsafe", "weakCompareAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z"),
    ("jdk/internal/misc/VM", "getSavedProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("jdk/internal/misc/VM", "isBooted", "()Z"),
    ("jdk/internal/misc/VM", "maxDirectMemory", "()J"),
];

/// Lane 2 (`java/lang/` remainder, `java/math/`), 2026-09-10.
///
/// Lane 2's own population is **390 shadows over 57 classes**, once lane T's
/// boundary-crossing registrars are carved out of the 992 rows under its prefix
/// set. This table holds the rows that earned a retirement; every other row in
/// that population is dispositioned in
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # `java/lang/Character` — three deprecated statics, and nothing to argue
///
/// `isJavaLetter`, `isJavaLetterOrDigit` and `isSpace`. The first two are
/// one-line delegations to `isJavaIdentifierStart` / `isJavaIdentifierPart`,
/// which carry no native here and so already run real bytecode; `isSpace` is
/// pure arithmetic over a 64-bit constant and calls nothing at all:
///
/// ```text
///   public static boolean isSpace(char);
///        0: iload_0
///        1: bipush 32
///        3: if_icmpgt 22
///        6: ldc2_w  // long 4294981120l
///        ...
/// ```
///
/// Armed alone over the 40-vector `--real-jdk` corpus: **40 passed, 0 failed**,
/// the unarmed baseline exactly.
///
/// # `java/math/BigInteger` — 24 rows, and the blocker was a different native
///
/// The 2026-08-19 package screen recorded `java/math/` at 35/36 armed, and that
/// number is why the prefix was never admitted. Re-taken 2026-09-10 per class,
/// the cost is `BigInteger`'s and the vector is `RJdkSecurity`, asserting
/// `2^127-1 must be prime`.
///
/// It is not a `BigInteger` shadow at all. Real `BigInteger.shiftRight`
/// bytecode returned `(2^127-2) >> 1` short by exactly 2^32 — the top `mag[]`
/// limb left at zero — because JDK 25 does that shift in
/// `shiftRightImplWorker`, an `@IntrinsicCandidate` this VM registers a native
/// over in `native-builtins/src/biginteger_intrinsics.rs`. Both that worker and
/// its left-shift twin ran one iteration short of the JDK contract.
///
/// **The census could not see either of them**, because they are registered
/// `NativeKind::Intrinsic` and an `Intrinsic` is exempt from the shadow census
/// by construction. So the native blocking this lane's retirement was, by the
/// instrument's own design, invisible to the lane. Measured by calling the five
/// registered intrinsics reflectively (`L2IntrinsicProbe.java`):
/// **33 of 60 rows differed from HotSpot 25.0.4+7, all in the two shift
/// workers**, while `implSquareToLen`, `implMulAdd` and `mulAdd` were 0-diff in
/// the same run. Fixed in the same change as this table.
///
/// The 24 rows themselves were already probed: `BigIntegerSweep.java`
/// is **13253 rows, 0 diffs**, every ordered pair of a boundary corpus through
/// every binary operation.
///
/// ## All 24 are retired. Nine were held, and the RULE that held them expired
///
/// Wave 1 retired ten and held fourteen. With all 24 retired the probe-tree A/B
/// on two binaries had read:
///
/// ```text
///   BigIntegerSweep   control (no table, JIT on)    0 differing lines
///                     trial   (table,    JIT on)   18   = 9 rows
///                     trial   (table,   --nojit)    6   = 3 rows
/// ```
///
/// A positive delta is the one result that is a reason not to retire, so the
/// nine were held per-TRIPLE and the hold was generalised to a rule. Both halves
/// are now closed, and for different reasons: one was a real defect, the other a
/// contract that had never been checked against the oracle.
///
/// **`add`, `subtract`, `multiply` -- a SURVIVOR, and a real defect.** All three
/// were registered twice. `phases_late.rs` owns the slot as a `Bridge`;
/// `math_bignum.rs` registered an `Intrinsic` first which owned no slot and was
/// *dead in compatible mode in all three feature arms*. Refusing the `Bridge`
/// under `--real-jdk` did not reach bytecode -- it woke the dead loser, which
/// returns **null** for a null argument where the real body throws:
///
/// ```text
///   --real-jdk-report, BigIntegerSweep run, before the fix:
///     27 synthetic-native-registered refusals on the retired classes
///      3 of them carrying a survivor
///        add       survivor=intrinsic@native-builtins/src/math_bignum.rs:1404
///        subtract  survivor=intrinsic@native-builtins/src/math_bignum.rs:1410
///        multiply  survivor=intrinsic@native-builtins/src/math_bignum.rs:1416
/// ```
///
/// Fixed 2026-09-12 by deleting the three dead registrations, which changes
/// nothing in compatible mode because they own no slot there. The same shape had
/// shipped in the `java/util/logging` wave a month earlier and was fixed in the
/// same change. `the native-registry census`'s `no_retired_triple_survives_the_strict_boot`
/// is the gate that makes it impossible to ship a third time: one line of set
/// arithmetic over the strict boot's registry, asking which of its surviving rows
/// these tables claim.
///
/// **The other eleven -- the rule's premise was false.** Interpreted they were
/// HotSpot-exact; once the real body was JIT-compiled the `NullPointerException`
/// arrived with no message. The VM-side defect was real and was fixed by
/// `0d013f359`. The *rule* built on top of it was not: asked of the oracle at the
/// same heat, HotSpot drops the message too.
///
/// ```text
///   probes/L2BigIntNpe14.java -- 15 null-argument rows, cold then 200k warm
///     HotSpot 25.0.4+7, default flags         9 of 15 hot rows answer a bare
///                                             `null`, and WHICH nine differs
///                                             between runs of the same class file
///     HotSpot -XX:-OmitStackTraceInFastThrow  0 of 15; hot == cold, all helpful
/// ```
///
/// `OmitStackTraceInFastThrow` is on by default and lets C2 throw a preallocated
/// exception carrying neither message nor stack trace. So "hot equals cold" was
/// never the JDK contract, and the run-to-run instability that made the hold
/// structural rather than empirical is the ORACLE's instability, reproduced. The
/// stable contract is the interpreted message -- identical under both oracle
/// configurations -- and this VM matches it on all 30 rows in both modes, never
/// taking the fast-throw latitude. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// The ten value-shaped rows of wave 1 -- `bitCount`, `bitLength`,
/// `intValueExact`, `isProbablePrime`, `longValueExact`, `not`, `shiftLeft`,
/// `shiftRight`, `testBit`, `toByteArray` -- remain 0-diff over the whole
/// 13253-row sweep with the JIT on.
///
/// **Seven rows removed on the 2026-09-11 catch-up merge with lane T**:
/// `ExceptionInInitializerError`'s two constructors and `initCause`,
/// `NullPointerException(String)`, `Throwable.initCause`, and
/// `UnsatisfiedLinkError`'s two constructors were claimed by BOTH this table
/// and `RETIRED_SHADOW_LT_TRIPLES` — `no_triple_is_claimed_by_two_tables`
/// caught it. Lane T's page dates its claim 2026-09-10, a day before this
/// page's own "blocker cleared 2026-09-11"; per that test's own rule, the
/// later claim is the duplicate. These seven java/lang/ Throwable-family rows
/// are lane T's to keep — they are seven of the 783 in
/// `RETIRED_SHADOW_LT_TRIPLES` and were never independently measured here.
///
/// # `java/lang/Package.getPackages()` left this table on 2026-09-11
///
/// Not un-retired: the four empty-array registrations behind it were DELETED,
/// so there is no longer a shadow for strict mode to refuse. A retirement row
/// for a triple nothing registers is rot that reads like a measurement. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L2_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Character", "isJavaLetter", "(C)Z"),
    ("java/lang/Character", "isJavaLetterOrDigit", "(C)Z"),
    ("java/lang/Character", "isSpace", "(C)Z"),
    ("java/lang/IllegalThreadStateException", "<init>", "()V"),
    (
        "java/lang/IllegalThreadStateException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    ("java/lang/Object", "<init>", "()V"),
    ("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/Object", "finalize", "()V"),
    ("java/lang/Object", "toString", "()Ljava/lang/String;"),
    ("java/lang/Object", "wait", "()V"),
    ("java/lang/Object", "wait", "(J)V"),
    ("java/lang/Package", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/Package", "hashCode", "()I"),
    ("java/lang/StringUTF16", "getChars", "([BII[CI)V"),
    ("java/lang/VirtualMachineError", "<init>", "()V"),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/VirtualMachineError",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getGarbageCollectorMXBeans",
        "()Ljava/util/List;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getPlatformMXBean",
        "(Ljava/lang/Class;)Ljava/lang/management/PlatformManagedObject;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getPlatformMXBeans",
        "(Ljava/lang/Class;)Ljava/util/List;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
    ),
    (
        "java/lang/management/ManagementFactory",
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
    ),
    ("java/lang/management/MemoryUsage", "<init>", "(JJJJ)V"),
    ("java/lang/management/MemoryUsage", "getCommitted", "()J"),
    ("java/lang/management/MemoryUsage", "getInit", "()J"),
    ("java/lang/management/MemoryUsage", "getMax", "()J"),
    ("java/lang/management/MemoryUsage", "getUsed", "()J"),
    ("java/math/BigInteger", "<init>", "(I[B)V"),
    ("java/math/BigInteger", "<init>", "([B)V"),
    (
        "java/math/BigInteger",
        "add",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "and",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    ("java/math/BigInteger", "bitCount", "()I"),
    ("java/math/BigInteger", "bitLength", "()I"),
    (
        "java/math/BigInteger",
        "divide",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "gcd",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    ("java/math/BigInteger", "intValueExact", "()I"),
    ("java/math/BigInteger", "isProbablePrime", "(I)Z"),
    ("java/math/BigInteger", "longValueExact", "()J"),
    (
        "java/math/BigInteger",
        "mod",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "modInverse",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "multiply",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    ("java/math/BigInteger", "not", "()Ljava/math/BigInteger;"),
    (
        "java/math/BigInteger",
        "or",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "remainder",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "shiftLeft",
        "(I)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "shiftRight",
        "(I)Ljava/math/BigInteger;",
    ),
    (
        "java/math/BigInteger",
        "subtract",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
    ("java/math/BigInteger", "testBit", "(I)Z"),
    ("java/math/BigInteger", "toByteArray", "()[B"),
    (
        "java/math/BigInteger",
        "xor",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
    ),
];

